//! 代理日志（原 `device_proxy.py` `log` / `ProxyRequestLogger` / `_mask_*` /
//! `extract_sse_summary` / `_ws_hexdump` 的 Rust 版）。
//!
//! 两条日志流：
//! - [`ProxyLog`]：操作日志 → `logs/proxy.log` + 宿主 [`ProxyEventSink`]（含
//!   `account-captured` 派生），对应上游「stdout 逐行被桌面端消费」的通路；
//! - [`RequestLogger`]：抓包日志 → `logs/proxy_req_YYYY-MM-DD.log`（同日 100MB 滚动 `.N` 序号），
//!   凭证头/体级凭证键值一律脱敏后落盘（**脱敏红线**，见上游审查修复 P0）。
//!
//! 脱敏覆盖面（红线，改动前请先读这段）：
//! `sessionid` / `sid_guard` / `ttwid` / JWT（`authorization` / `x-cloudide-token` /
//! `x-icube-token`）/ `refresh_token` / `access_token` / `api_key` / `secret` /
//! `client_secret` / `password` / `pass_token` 等键值，头级与体级都要掩码。

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::modules::device_proxy::ProxyEventSink;

/// 凭证头脱敏清单（键比较不区分大小写）：命中即掩码，禁止明文落盘。
/// `x-cloudide-token` / `x-icube-token` 与 [`crate::modules::device_proxy::handler::auth_header_value`]
/// 嗅探的 JWT 承载头保持同源（审查修复：此前这两头承载 token 却未命中清单，明文落盘违反脱敏红线）。
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "proxy-authorization",
    "x-cloudide-token",
    "x-icube-token",
    "x-auth-token",
    "x-session-token",
];

/// 体级凭证键值脱敏：值整体替换为 `***`（fail-closed：编译期正则，无运行期失效路径）。
///
/// 键集合覆盖 handler 采信的凭证键：
/// - 豆包会话凭证 `sessionid` / `sid_guard` / `ttwid`（抓包凭证就是账号密码，绝不可落盘）
/// - JWT 与刷新令牌 `jwt` / `access_token` / `refresh_token` / `id_token` / `session_token`
/// - 裸 `token`（handler 把响应里的 `token` 采信为 access_token）
/// - `client_secret` 单独列出 —— `\bsecret\b` 因下划线属 word 字符永远匹配不到它
fn body_mask_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r#"(?i)((?:"(?:access_token|refresh_token|id_token|session_token|sessionid|sid_guard|ttwid|jwt|api_key|apikey|secret|client_secret|token|authorization|password|pass_token)"|\b(?:access_token|refresh_token|id_token|session_token|sessionid|sid_guard|ttwid|jwt|api_key|apikey|client_secret|token|authorization|password|pass_token)\b)\s*[=:]\s*)("[^"]*"|'[^']*'|[^,;&\s}]+)"#,
        )
        .expect("body mask regex")
    })
}

/// 凭证头掩码：`len>16` 取前 8 + `…` + 后 4，否则 `***`
pub fn mask_header_value(name: &str, value: &str) -> String {
    if SENSITIVE_HEADERS.iter().any(|h| h.eq_ignore_ascii_case(name)) {
        let chars: Vec<char> = value.chars().collect();
        return if chars.len() > 16 {
            let head: String = chars[..8].iter().collect();
            let tail: String = chars[chars.len() - 4..].iter().collect();
            format!("{head}…{tail}")
        } else {
            "***".to_string()
        };
    }
    value.to_string()
}

/// 体级凭证键值脱敏（对齐 Python `_mask_body`）
pub fn mask_body(text: &str) -> String {
    body_mask_regex().replace_all(text, r#"$1"***""#).to_string()
}

/// 按 `Content-Encoding` 解压响应体用于日志展示（gzip/deflate；br/zstd 与 Python 版同样跳过）
pub fn decompress_body(body: &[u8], resp_headers: &[(String, String)]) -> Vec<u8> {
    let encoding = resp_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-encoding"))
        .map(|(_, v)| v.trim().to_ascii_lowercase());
    let Some(encoding) = encoding else {
        return body.to_vec();
    };
    let mut out = Vec::new();
    match encoding.as_str() {
        "gzip" => {
            if flate2::read::MultiGzDecoder::new(body).read_to_end(&mut out).is_err() {
                return body.to_vec();
            }
        }
        "deflate" => {
            // 先按 zlib-wrapped 试，失败再按 raw deflate（对齐 Python 双尝试）
            if flate2::read::ZlibDecoder::new(body).read_to_end(&mut out).is_err() {
                out.clear();
                if flate2::read::DeflateDecoder::new(body).read_to_end(&mut out).is_err() {
                    return body.to_vec();
                }
            }
        }
        _ => return body.to_vec(),
    }
    out
}

/// SSE 流式响应摘要（模型 / token 用量等）。仅对 `Content-Type: text/event-stream` 生效。
pub fn extract_sse_summary(
    resp_headers: &[(String, String)],
    body: &[u8],
) -> Option<Vec<(String, String)>> {
    let is_sse = resp_headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("content-type") && v.to_ascii_lowercase().contains("event-stream")
    });
    if !is_sse || body.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(body);
    let mut summary: Vec<(String, String)> = Vec::new();
    let mut push = |k: &str, v: String| {
        if let Some(slot) = summary.iter_mut().find(|(ek, _)| ek == k) {
            slot.1 = v;
        } else {
            summary.push((k.to_string(), v));
        }
    };
    let mut current_event = String::new();
    let mut output_count = 0usize;
    for line in text.split('\n') {
        let line = line.trim();
        if let Some(ev) = line.strip_prefix("event:") {
            current_event = ev.trim().to_string();
        } else if let Some(data_str) = line.strip_prefix("data:") {
            let data_str = data_str.trim();
            if data_str.is_empty() {
                continue;
            }
            let Ok(data) = serde_json::from_str::<serde_json::Value>(data_str) else {
                continue;
            };
            match current_event.as_str() {
                "metadata" => {
                    if let Some(m) = data
                        .get("model")
                        .or_else(|| data.get("model_name"))
                        .and_then(|v| v.as_str())
                    {
                        push("model", m.to_string());
                    }
                    if let Some(sid) = data.get("session_id").and_then(|v| v.as_str()) {
                        let truncated: String = sid.chars().take(16).collect();
                        push("session_id", format!("{truncated}..."));
                    }
                }
                "output" => output_count += 1,
                "token_usage" => {
                    if let Some(pt) = data.get("prompt_tokens").and_then(serde_json::Value::as_i64) {
                        push("prompt_tokens", pt.to_string());
                    }
                    if let Some(ct) = data.get("completion_tokens").and_then(serde_json::Value::as_i64)
                    {
                        push("completion_tokens", ct.to_string());
                    }
                    if let Some(tt) = data.get("total_tokens").and_then(serde_json::Value::as_i64) {
                        push("total_tokens", tt.to_string());
                    }
                }
                "done" => {
                    if let Some(fr) = data.get("finish_reason").and_then(|v| v.as_str()) {
                        push("finish_reason", fr.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    if output_count > 0 {
        push("output_chunks", output_count.to_string());
    }
    (!summary.is_empty()).then_some(summary)
}

// ---------------- 操作日志（proxy.log + 宿主事件桥接） ----------------

/// 操作日志：落盘 `logs/proxy.log` 并经 [`ProxyEventSink`] 上报宿主；
/// 行内含 `user=<纯数字>` 时派生 `account-captured` 事件并累加捕获计数
///（对齐 Python 版 stdout 行被桌面端 `extract_uid` 消费的语义）。
/// 超过 10MB 滚动为 `proxy.log.1`（审查修复：原 append-only 无上限，长跑高频 WS 帧日志可无限增长）。
#[derive(Clone)]
pub struct ProxyLog {
    file: Arc<Mutex<Option<std::fs::File>>>,
    log_path: PathBuf,
    events: Option<Arc<dyn ProxyEventSink>>,
    captured: Arc<AtomicI64>,
}

/// `proxy.log` 单文件滚动上限
const PROXY_LOG_MAX: u64 = 10 * 1024 * 1024;

fn open_append(path: &std::path::Path) -> Option<std::fs::File> {
    OpenOptions::new().create(true).append(true).open(path).ok()
}

impl ProxyLog {
    pub fn new(
        log_path: PathBuf,
        events: Option<Arc<dyn ProxyEventSink>>,
        captured: Arc<AtomicI64>,
    ) -> Self {
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = open_append(&log_path).or_else(|| {
            let fallback = std::env::temp_dir().join("ai_gateway_proxy.log");
            open_append(&fallback)
        });
        Self { file: Arc::new(Mutex::new(file)), log_path, events, captured }
    }

    /// 记录一行操作日志（自动加时间戳前缀）
    pub fn log(&self, line: &str) {
        let stamped = format!("[{}] {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), line);
        {
            let mut g = self.file.lock().unwrap_or_else(|e| e.into_inner());
            if g.is_none() {
                *g = open_append(&self.log_path);
            }
            if let Some(f) = g.as_mut() {
                // 滚动：超限先关句柄（Windows rename 需独占）→ 换名 .1 → 重开
                if f.metadata().map(|m| m.len()).unwrap_or(0) >= PROXY_LOG_MAX {
                    let _ = g.take();
                    let rotated = self.log_path.with_extension("log.1");
                    let _ = std::fs::remove_file(&rotated);
                    let _ = std::fs::rename(&self.log_path, &rotated);
                    *g = open_append(&self.log_path);
                }
            }
            if let Some(f) = g.as_mut() {
                let _ = writeln!(f, "{stamped}");
                let _ = f.flush();
            }
        }
        if let Some(events) = &self.events {
            events.log(line);
            if let Some(uid) = extract_uid(line) {
                self.captured.fetch_add(1, Ordering::Relaxed);
                events.account_captured(&uid);
            }
        }
    }
}

/// 从日志行提取 `user=<uid>`（供 `account-captured` 事件派生）。
///
/// 只认 `user=` / `user_id=` 记法：签到改写行用的是 `uid=`，
/// 若一并采信会让每次签到都误报「捕获到账号」（上游审查修复）。
pub fn extract_uid(line: &str) -> Option<String> {
    for key in ["user=", "user_id="] {
        if let Some(idx) = line.find(key) {
            let rest = &line[idx + key.len()..];
            let end = rest
                .find(|c: char| !(c.is_ascii_digit() || c == '_'))
                .unwrap_or(rest.len());
            let uid = &rest[..end];
            if !uid.is_empty() && uid.chars().all(|c| c.is_ascii_digit()) {
                return Some(uid.to_string());
            }
        }
    }
    None
}

// ---------------- 抓包日志（滚动文件） ----------------

/// 抓包日志：按日命名 `proxy_req_%Y-%m-%d.log`，同日单文件超 100MB 换 `.N` 序号继续追加。
struct RollingState {
    file: Option<std::fs::File>,
    day: String,
    seq: u32,
    size: u64,
}

pub struct RequestLogger {
    dir: PathBuf,
    max_size: u64,
    state: Mutex<RollingState>,
}

impl RequestLogger {
    pub fn new(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Self {
            dir,
            max_size: 100 * 1024 * 1024,
            state: Mutex::new(RollingState {
                file: None,
                day: String::new(),
                seq: 0,
                size: 0,
            }),
        }
    }

    /// 确保当日文件可写（跨日/超限换文件），返回已定位的写入口
    fn ensure_file<'a>(
        state: &'a mut RollingState,
        dir: &std::path::Path,
        max_size: u64,
    ) -> std::io::Result<&'a mut std::fs::File> {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        if state.file.is_some() && state.day == today && state.size < max_size {
            return Ok(state.file.as_mut().expect("checked"));
        }
        state.file = None;
        if state.day != today {
            state.day = today;
            state.seq = 0;
        }
        let base = dir.join(format!("proxy_req_{}.log", state.day));
        loop {
            let path = if state.seq == 0 {
                base.clone()
            } else {
                base.with_extension(format!("log.{}", state.seq))
            };
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if size < max_size {
                let f = OpenOptions::new().create(true).append(true).open(&path)?;
                state.file = Some(f);
                state.size = size;
                break;
            }
            state.seq += 1;
        }
        Ok(state.file.as_mut().expect("just set"))
    }

    fn write_block(&self, data: &str) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(f) = Self::ensure_file(&mut st, &self.dir, self.max_size) {
            let _ = f.write_all(data.as_bytes());
            let _ = f.flush();
            st.size += data.len() as u64;
        }
    }

    /// 记录一次完整请求/响应（脱敏红线：凭证头与体级凭证键值掩码后落盘）
    #[allow(clippy::too_many_arguments)]
    pub fn log_request(
        &self,
        method: &str,
        host: &str,
        path: &str,
        req_headers: &[(String, String)],
        req_body: &[u8],
        resp_status: u16,
        resp_reason: &str,
        resp_headers: &[(String, String)],
        resp_body: &[u8],
    ) {
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let mut out = String::new();
        out.push_str(&format!("\n{}\n", "=".repeat(80)));
        out.push_str(&format!("[{ts}] {method} {host}{path}\n"));
        out.push_str("--- Request Headers ---\n");
        for (k, v) in req_headers {
            out.push_str(&format!("  {k}: {}\n", mask_header_value(k, v)));
        }
        if !req_body.is_empty() {
            let preview = String::from_utf8_lossy(&req_body[..req_body.len().min(4096)]);
            out.push_str(&format!("--- Request Body ({} bytes) ---\n", req_body.len()));
            out.push_str(&mask_body(&preview));
            out.push('\n');
        }
        out.push_str(&format!("--- Response: {resp_status} {resp_reason} ---\n"));
        for (k, v) in resp_headers {
            out.push_str(&format!("  {k}: {}\n", mask_header_value(k, v)));
        }
        if !resp_body.is_empty() {
            let decompressed = decompress_body(resp_body, resp_headers);
            let preview = String::from_utf8_lossy(&decompressed[..decompressed.len().min(8192)]);
            out.push_str(&format!(
                "--- Response Body ({} bytes, decompressed {} bytes) ---\n",
                resp_body.len(),
                decompressed.len()
            ));
            out.push_str(&mask_body(&preview));
            out.push('\n');
        }
        if let Some(summary) = extract_sse_summary(resp_headers, resp_body) {
            out.push_str("--- SSE Summary ---\n");
            for (k, v) in summary {
                out.push_str(&format!("  {k}: {v}\n"));
            }
        }
        out.push('\n');
        self.write_block(&out);
    }

    /// 记录 WebSocket 升级握手（升级请求同样携带 Authorization 等凭证头，掩码后落盘）
    pub fn log_websocket(&self, host: &str, path: &str, req_headers: &[(String, String)]) {
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let mut out = String::new();
        out.push_str(&format!("\n{}\n", "=".repeat(80)));
        out.push_str(&format!("[{ts}] [WebSocket Upgrade] {host}{path}\n"));
        out.push_str("--- Request Headers ---\n");
        for (k, v) in req_headers {
            out.push_str(&format!("  {k}: {}\n", mask_header_value(k, v)));
        }
        out.push_str("--- WebSocket tunnel established: 双向帧载荷将在隧道中按 SEQ 记录 ---\n");
        out.push('\n');
        self.write_block(&out);
    }
}

// ---------------- WS 帧可视化辅助 ----------------

/// 二进制数据 → 可读 hex+ascii 文本（截断 400 字节，对齐 Python `_ws_hexdump`）
pub fn ws_hexdump(data: &[u8], max_bytes: usize) -> String {
    let mut lines = Vec::new();
    let n = data.len().min(max_bytes);
    for (offset, chunk) in data[..n].chunks(16).enumerate() {
        let hexs: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        let hex_padded: String = hexs.join(" ");
        let asc: String = chunk
            .iter()
            .map(|&b| if (0x20..=0x7e).contains(&b) { b as char } else { '.' })
            .collect();
        lines.push(format!("    {:04x}: {:<48}  {asc}", offset * 16, hex_padded));
    }
    if data.len() > n {
        lines.push(format!("    ... (截断，完整 {} bytes)", data.len()));
    }
    lines.join("\n")
}

/// 抽取连续可打印 ASCII 串（`>=min_len`），用于快速定位字面量字段（对齐 `_ws_extract_strings`）
pub fn ws_extract_strings(data: &[u8], min_len: usize, max_strings: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    for &b in data {
        if (0x20..=0x7e).contains(&b) {
            buf.push(b);
        } else {
            if buf.len() >= min_len {
                out.push(String::from_utf8_lossy(&buf).to_string());
            }
            buf.clear();
        }
    }
    if buf.len() >= min_len {
        out.push(String::from_utf8_lossy(&buf).to_string());
    }
    out.truncate(max_strings);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_credentials_in_bodies() {
        let masked = mask_body(r#"{"access_token":"abc.def.ghi","refresh_token": "xyz123"}"#);
        assert!(masked.contains(r#""access_token":"***""#));
        assert!(masked.contains(r#""refresh_token": "***""#));
        assert!(!masked.contains("abc.def"));
        // 审查修复回归：裸 token 键（handler 采信为 access_token）与 client_secret 必须掩码
        let masked2 = mask_body(r#"{"token":"jwt-plain","client_secret":"cs-secret","note":"keep"}"#);
        assert!(masked2.contains(r#""token":"***""#), "裸 token 键: {masked2}");
        assert!(masked2.contains(r#""client_secret":"***""#), "client_secret: {masked2}");
        assert!(masked2.contains("keep"));
        assert!(!masked2.contains("jwt-plain"));
        assert!(!masked2.contains("cs-secret"));
        let masked3 = mask_body("token=abc.def&x=1");
        assert!(masked3.contains(r#"token="***""#), "urlencoded: {masked3}");
    }

    /// 脱敏红线：豆包会话凭证（sessionid / sid_guard / ttwid）与 JWT 一律不得明文落盘
    #[test]
    fn masks_doubao_credentials_and_jwt() {
        let masked = mask_body(
            r#"{"sessionid":"abc123session","sid_guard":"sid%3Dvalue","ttwid":"tt-value","jwt":"eyJhbG.payload.sig"}"#,
        );
        assert!(!masked.contains("abc123session"), "sessionid 明文泄漏: {masked}");
        assert!(!masked.contains("sid%3Dvalue"), "sid_guard 明文泄漏: {masked}");
        assert!(!masked.contains("tt-value"), "ttwid 明文泄漏: {masked}");
        assert!(!masked.contains("eyJhbG.payload.sig"), "jwt 明文泄漏: {masked}");
        // 非引号包裹形态（cookie 风格 / urlencoded）同样要掩码
        let cookie_style = mask_body("sessionid=plain-session; ttwid=plain-tt");
        assert!(!cookie_style.contains("plain-session"), "cookie 形态泄漏: {cookie_style}");
        assert!(!cookie_style.contains("plain-tt"), "cookie 形态泄漏: {cookie_style}");
    }

    #[test]
    fn masks_sensitive_headers() {
        let long = mask_header_value("Authorization", "Cloud-IDE-JWT eyJhbGciOiJSUzI1NiJ9.sig");
        assert!(long.starts_with("Cloud-ID"));
        assert!(long.ends_with(".sig"));
        assert!(long.contains('…'));
        assert_eq!(mask_header_value("Cookie", "a=b"), "***");
        assert_eq!(mask_header_value("Content-Type", "application/json"), "application/json");
        // 审查修复回归：JWT 承载头（与 handler::auth_header_value 嗅探集合同源）必须掩码
        let ide = mask_header_value("x-cloudide-token", "eyJhbGciOiJIUzI1NiJ9.payload.sig");
        assert!(!ide.contains("payload"), "x-cloudide-token 掩码: {ide}");
        // 长值（>16）走「前8…后4」掩码（与 Authorization 用例同款设计）：
        // 断言中段载荷被隐藏，而非整值消失（尾 4 字符本就保留）
        let icube = mask_header_value("X-Icube-Token", "eyJhbGciOiJIUzI1NiJ9.p.s");
        assert!(!icube.contains("OiJIUzI1NiJ9"), "x-icube-token 掩码: {icube}");
        assert_ne!(icube, "eyJhbGciOiJIUzI1NiJ9.p.s", "x-icube-token 必须掩码");
    }

    /// Cookie 头（豆包凭证的载体）必须整体掩码，不能只掩码值的一部分
    #[test]
    fn cookie_headers_are_masked() {
        assert_eq!(mask_header_value("Cookie", "sessionid=short"), "***");
        let long = mask_header_value("Set-Cookie", "sessionid=0123456789abcdef0123456789abcdef; Path=/");
        assert!(!long.contains("0123456789abcdef"), "Set-Cookie 明文泄漏: {long}");
    }

    #[test]
    fn gzip_roundtrip_decompress() {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"hello world").unwrap();
        let gz = enc.finish().unwrap();
        let headers = vec![("Content-Encoding".to_string(), "gzip".to_string())];
        assert_eq!(decompress_body(&gz, &headers), b"hello world");
    }

    #[test]
    fn sse_summary_extraction() {
        let headers = vec![("Content-Type".to_string(), "text/event-stream".to_string())];
        let body = concat!(
            "event: metadata\n",
            "data: {\"model\":\"glm-5.3\",\"session_id\":\"abcdefghijklmnopqrst\"}\n",
            "event: output\n",
            "data: {}\n",
            "event: output\n",
            "data: {}\n",
            "event: token_usage\n",
            "data: {\"prompt_tokens\":10,\"completion_tokens\":20,\"total_tokens\":30}\n",
            "event: done\n",
            "data: {\"finish_reason\":\"stop\"}\n",
        );
        let s = extract_sse_summary(&headers, body.as_bytes()).expect("summary");
        let get = |k: &str| s.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.clone());
        assert_eq!(get("model").as_deref(), Some("glm-5.3"));
        assert_eq!(get("session_id").as_deref(), Some("abcdefghijklmnop..."));
        assert_eq!(get("output_chunks").as_deref(), Some("2"));
        assert_eq!(get("total_tokens").as_deref(), Some("30"));
        assert_eq!(get("finish_reason").as_deref(), Some("stop"));
    }

    #[test]
    fn uid_extraction_from_log_lines() {
        // 真捕获行（handler: [JWT 自动更新/追加] user=…）触发派生事件
        assert_eq!(
            extract_uid("  [JWT 自动更新] user=4487568582777872 exp=..."),
            Some("4487568582777872".into())
        );
        // 签到改写行用 uid= 记法（审查修复：user= 会每次误触发 account-captured）
        assert_eq!(extract_uid("  [签到改写] uid=4487568582777872 -> x-device-id=1"), None);
        assert_eq!(extract_uid("no user here"), None);
        assert_eq!(extract_uid("user=abc"), None);
    }

    #[test]
    fn hexdump_format() {
        let s = ws_hexdump(b"AB", 400);
        assert!(s.contains("41 42"));
        assert!(s.contains("AB"));
    }

    /// 抓包日志落盘：凭证头与体级凭证都必须脱敏（端到端回归）
    #[test]
    fn request_logger_writes_masked_blocks() {
        let dir = std::env::temp_dir().join(format!(
            "ai_gateway_reqlog_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let logger = RequestLogger::new(dir.clone());
        logger.log_request(
            "POST",
            "www.doubao.com",
            "/api/chat",
            &[
                ("Cookie".to_string(), "sessionid=super-secret-sid".to_string()),
                ("Authorization".to_string(), "Cloud-IDE-JWT eyJhbG.abcdef.sig".to_string()),
            ],
            br#"{"refresh_token":"rt-plain-secret"}"#,
            200,
            "OK",
            &[("Content-Type".to_string(), "application/json".to_string())],
            br#"{"access_token":"at-plain-secret","sessionid":"sid-plain-secret"}"#,
        );
        let day = chrono::Local::now().format("%Y-%m-%d").to_string();
        let text = std::fs::read_to_string(dir.join(format!("proxy_req_{day}.log"))).unwrap();
        for secret in [
            "super-secret-sid",
            "rt-plain-secret",
            "at-plain-secret",
            "sid-plain-secret",
            "abcdef.sig",
        ] {
            assert!(!text.contains(secret), "凭证明文落盘: {secret}\n{text}");
        }
        assert!(text.contains("***"));
        assert!(text.contains("www.doubao.com"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
