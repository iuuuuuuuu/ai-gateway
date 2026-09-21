//! MITM 解密请求处理（`device_proxy.py` `tunnel_https` / `forward_upstream` 迁移，P4-5）。
//!
//! 流程（对齐 Python 版）：
//! 1. 读原始请求（head + Content-Length/Chunked body）
//! 2. JWT 自动捕获（authorization / x-cloudide-token / x-icube-token，**不限 host**）→ 写回账号库
//! 3. 真实设备指纹捕获（`x-device-id` / `vscode-sessionid` / `x-market-user-id`）→ 设备表
//! 4. 签到接口头改写（注入按账号派生的 x-device-id / x-market-user-id / vscode-sessionid）
//! 5. WebSocket 升级 → 交由 [`crate::modules::device_proxy::ws`] 模块接管
//! 6. 其余转发上游：非流式整体缓冲 + 凭据抓取（refresh_token / 豆包会话）；
//!    流式（SSE）逐块 chunked 转发 + 全量摘要落日志
//!
//! ## 复用本仓库既有模块（不重复造轮子）
//!
//! - 账号库：[`crate::modules::trae_account`]（`trae_accounts.json`，`user_id` 键）
//! - 设备指纹：[`crate::modules::trae_device`]（`device_map.json`，抓包真值优先于派生值）
//! - 豆包凭证：[`crate::modules::doubao_account::save_captured`]
//! - 冷却表：[`crate::modules::trae_checkin::clear_cooldown`]

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use http_body_util::{BodyExt, Full, Limited};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

use crate::modules::device_proxy::logger::{ProxyLog, RequestLogger};
use crate::modules::device_proxy::upstream::UpstreamConnector;

/// 建连/首读超时（对齐 Python `_CONN_TIMEOUT`）
const CONN_TIMEOUT: Duration = Duration::from_secs(300);
/// 流式请求上游超时（对齐 Python forward_upstream is_stream 分支）
const STREAM_TIMEOUT: Duration = Duration::from_secs(300);
/// 非流式请求上游超时
const PLAIN_TIMEOUT: Duration = Duration::from_secs(30);
/// 请求头缓冲上限（对齐 Python recv_until max_size=10MB）
const MAX_HEAD: usize = 10 * 1024 * 1024;
/// 请求体上限（Python 无显式上限，此处防御性收紧；正常 API 请求远小于该值）
const MAX_BODY: usize = 64 * 1024 * 1024;
/// 非流式响应体缓冲上限（Python 无显式上限，此处防御性；流式路径逐块转发不整体缓冲）
pub(crate) const MAX_RESP_BODY: usize = 256 * 1024 * 1024;
/// 流式响应日志缓冲上限（对齐 Python `MAX_LOG_BODY`）
const MAX_LOG_BODY: usize = 10 * 1024 * 1024;

/// 签到领取接口（请求头改写命中路径）
pub const SIGNIN_PATH: &str = "/trae/api/v2/ug/checkin_credits/claim";

/// 已知 TRAE 接口（path 子串匹配 → 日志用途标签），对齐 Python `KNOWN_TRAE_PATHS`
const KNOWN_TRAE_PATHS: &[(&str, &str)] = &[
    ("/trae/api/v2/ug/checkin_credits/claim", "签到领取"),
    ("/trae/api/v2/ug/checkin_credits/status", "签到状态"),
    ("/cloudide/api", "CloudIDE网关"),
    ("/oauth/", "OAuth"),
    ("ExchangeToken", "换Token"),
    ("ide_user_ent_usage", "积分用量"),
    ("/api/agent/v3/llm_utils_chat", "大模型对话"),
    ("/api/ide/v1/get_detail_param", "模型列表"),
    ("/api/remote/v1/plugins", "插件接口"),
    ("/api/remote/v1/skills", "技能列表"),
];

/// 请求侧跳过头（对齐 Python `HOP_BY_HOP`）
pub(crate) const HOP_BY_HOP_REQ: &[&str] = &[
    "proxy-connection",
    "connection",
    "keep-alive",
    "proxy-authorization",
    "host",
    "content-length",
];

/// 响应侧跳过头（对齐 Python `send_response`/`_stream_response` 过滤集）
const HOP_BY_HOP_RESP: &[&str] = &["transfer-encoding", "connection", "keep-alive", "content-length"];

/// accounts 写回互斥：防止并发连接同时读改写 JSON（Python 用 RLock，等价保护）。
///
/// 本仓库的 [`crate::modules::trae_account`] 是无锁的 load/save，因此这把锁是
/// 「读-改-写」原子性的唯一保证，JWT 与 refresh_token 的合并写必须持同一把锁。
static ACCOUNTS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
/// 豆包凭证进程内去重缓存 + 落盘锁（对齐 Python `_doubao_captured_cache`/`_capture_lock`）。
///
/// 初值取磁盘上已有的抓包凭证：代理重启后第一次抓到的凭证若与磁盘一致，
/// 就不该再写一遍（否则每次重启都会刷一次文件时间戳）。
static DOUBAO_LOCK: OnceLock<Mutex<Option<serde_json::Value>>> = OnceLock::new();

pub(crate) fn accounts_lock() -> &'static Mutex<()> {
    ACCOUNTS_LOCK.get_or_init(|| Mutex::new(()))
}

fn doubao_cache() -> &'static Mutex<Option<serde_json::Value>> {
    DOUBAO_LOCK.get_or_init(|| Mutex::new(crate::modules::doubao_account::load_captured()))
}

/// 代理共享上下文：路径/开关/日志句柄，由 `mod.rs` 主循环构造并贯穿各模块
pub struct ProxyCtx {
    pub log: ProxyLog,
    pub req_logger: Arc<RequestLogger>,
    /// 监听域名列表（小写）
    pub targets: Vec<String>,
    /// 自动捕获 JWT 写回账号库（默认开）
    pub auto_capture_jwt: bool,
    /// 数据根目录（本仓库恒为 [`crate::modules::config::store_dir`]）
    pub data_dir: PathBuf,
}

impl ProxyCtx {
    /// 域名后缀匹配（对齐 Python `host_in_targets`）
    pub fn host_in_targets(&self, host: &str) -> bool {
        let h = host.to_ascii_lowercase();
        self.targets
            .iter()
            .any(|d| h == *d || h.ends_with(&format!(".{d}")))
    }

    /// path 子串匹配已知接口（对齐 Python `classify_path`）
    pub fn classify_path(&self, path: &str) -> Option<&'static str> {
        KNOWN_TRAE_PATHS
            .iter()
            .find(|(sub, _)| path.contains(sub))
            .map(|(_, name)| *name)
    }
}

// ---------------- 原始请求读取（客户端侧） ----------------

/// 解密后的原始 HTTP/1.1 请求
pub struct RawRequest {
    pub method: String,
    pub path: String,
    /// 原始大小写头对（可重复；Set-Cookie 等多值头不折叠）
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl RawRequest {
    /// 大小写不敏感取头（同名字段取最后一次出现，对齐 Python dict 语义）
    pub fn hget(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .rev()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// 替换头（移除同名大小写变体后追加）
    pub fn hset(&mut self, name: &str, value: String) {
        self.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
        self.headers.push((name.to_string(), value));
    }

    /// WebSocket 升级检测（对齐 Python `is_websocket_upgrade`）
    pub fn is_websocket_upgrade(&self) -> bool {
        self.hget("upgrade").map(|v| v.eq_ignore_ascii_case("websocket")).unwrap_or(false)
    }
}

/// 读一条原始请求：head 直到 `\r\n\r\n` + body（Content-Length / Chunked）。
/// 返回 `Ok(None)` 表示客户端已关闭（EOF）。头超限视为连接异常（记日志后断开）。
pub async fn read_raw_request<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<RawRequest>, String> {
    read_raw_request_buf(r, BytesMut::new()).await
}

/// 带初始缓冲的变体：连接分发阶段已读取的字节注入后继续解析
///（`mod.rs` 明文路径在判路由时已消耗部分流字节，由此续接；缓冲可能含 body 前缀）
pub async fn read_raw_request_buf<R: AsyncRead + Unpin>(
    r: &mut R,
    mut buf: BytesMut,
) -> Result<Option<RawRequest>, String> {
    let head_end = loop {
        if let Some(pos) = find_head_end(&buf) {
            break pos;
        }
        if buf.len() > MAX_HEAD {
            return Err(format!("请求头缓冲区超限 ({})", buf.len()));
        }
        let mut chunk = [0u8; 8192];
        let n = r
            .read(&mut chunk)
            .await
            .map_err(|e| format!("读取请求头失败: {e}"))?;
        if n == 0 {
            return Ok(None); // EOF
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = buf.split_to(head_end + 4).freeze();
    let rest = buf;

    let head_str = String::from_utf8_lossy(&head);
    let mut lines = head_str.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let _version = parts.next().unwrap_or("HTTP/1.1");

    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let mut req = RawRequest { method, path, headers, body: Bytes::new() };

    let lookup = |name: &str| {
        req.headers
            .iter()
            .rev()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };

    // Content-Length 优先；Chunked 降级解析（Python 版不处理 Chunked 请求体，此处增强）
    if let Some(te) = lookup("transfer-encoding") {
        if te.to_ascii_lowercase().contains("chunked") {
            req.body = read_chunked_body(r, rest).await?;
            return Ok(Some(req));
        }
    }
    let cl: usize = lookup("content-length")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if cl > MAX_BODY {
        return Err(format!("请求体超限 ({cl} > {MAX_BODY})"));
    }
    let mut body = rest;
    while body.len() < cl {
        let mut chunk = [0u8; 8192];
        let n = r.read(&mut chunk).await.map_err(|e| format!("读取请求体失败: {e}"))?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(cl);
    req.body = body.freeze();
    Ok(Some(req))
}

/// 定位 `\r\n\r\n`（返回 head 部分结束下标，即最后一个 `\r` 的下标）
pub(crate) fn find_head_end(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 {
        return None;
    }
    (0..=buf.len() - 4).find(|&i| &buf[i..i + 4] == b"\r\n\r\n")
}

/// Chunked 请求体解码（head 之后的剩余字节为初始输入）
async fn read_chunked_body<R: AsyncRead + Unpin>(r: &mut R, mut buf: BytesMut) -> Result<Bytes, String> {
    let mut out = BytesMut::new();
    loop {
        // 读取一行 chunk size
        let line = loop {
            if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                break buf.split_to(pos + 1);
            }
            if buf.len() > 1024 * 1024 {
                return Err("chunk size 行超限".to_string());
            }
            let mut chunk = [0u8; 4096];
            let n = r.read(&mut chunk).await.map_err(|e| format!("读取 chunk 失败: {e}"))?;
            if n == 0 {
                return Err("chunked body 意外 EOF".to_string());
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let size_str = String::from_utf8_lossy(&line);
        let size = usize::from_str_radix(size_str.trim().split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| format!("非法 chunk size: {size_str}"))?;
        if size == 0 {
            // 终结块后的 trailer 不再消费（极少见，Python 版同样不处理 chunked 请求）
            return Ok(out.freeze());
        }
        if out.len() + size > MAX_BODY {
            return Err("chunked 请求体超限".to_string());
        }
        while buf.len() < size + 2 {
            let mut chunk = [0u8; 8192];
            let n = r.read(&mut chunk).await.map_err(|e| format!("读取 chunk 数据失败: {e}"))?;
            if n == 0 {
                return Err("chunked body 意外 EOF".to_string());
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let mut data = buf.split_to(size + 2);
        data.truncate(size);
        out.extend_from_slice(&data);
    }
}

// ---------------- 响应发送（客户端侧） ----------------

/// 逐帧读取上游响应体（Limited 上限 + 每帧空闲超时）。
///
/// 对齐 Python socket timeout 的「逐读」语义（差异修复：原 `collect()` 仅在建立请求段
/// 有超时，读体阶段无逐读超时 —— 僵死上游 trickling body 可无限拖住连接与并发槽）。
pub(crate) async fn collect_body_with_idle_timeout(
    body: Limited<hyper::body::Incoming>,
    idle: Duration,
) -> Result<Bytes, String> {
    let mut body = body;
    let mut buf = BytesMut::new();
    loop {
        match timeout(idle, body.frame()).await {
            Err(_) => return Err(format!("读上游响应体空闲超时 ({}s)", idle.as_secs())),
            Ok(None) => return Ok(buf.freeze()),
            Ok(Some(Err(e))) => return Err(format!("读上游响应失败: {e}")),
            Ok(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    buf.extend_from_slice(data);
                }
            }
        }
    }
}

/// 缓冲式响应回写（对齐 Python `send_response`）：保留重复头（Set-Cookie 多值），
/// 重写 Content-Length 并强制 keep-alive。
pub(crate) async fn send_response<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    status: u16,
    reason: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (k, v) in headers {
        if HOP_BY_HOP_RESP.iter().any(|h| k.eq_ignore_ascii_case(h)) {
            continue;
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    head.push_str("Connection: keep-alive\r\n\r\n");
    w.write_all(head.as_bytes()).await?;
    w.write_all(body).await?;
    w.flush().await
}

/// HTTP 状态码标准短语（hyper 不透出上游 reason，取标准值即可，客户端仅展示用）
pub(crate) fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

// ---------------- JWT 校验 / 提取 ----------------

/// 校验 Cloud-IDE-JWT：3 段、header `alg=RS256`、payload `data.id` 存在。
/// 通过则返回规范化 `Cloud-IDE-JWT <jwt>`（对齐 Python `_valid_cloud_ide_jwt`）。
pub fn valid_cloud_ide_jwt(tok: &str) -> Option<String> {
    use base64::Engine;
    let parts: Vec<&str> = tok.trim().split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let decode = |s: &str| -> Option<serde_json::Value> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s.trim_end_matches('='))
            .ok()?;
        serde_json::from_slice(&bytes).ok()
    };
    let header = decode(parts[0])?;
    let payload = decode(parts[1])?;
    let alg_ok = header.get("alg").and_then(|v| v.as_str()) == Some("RS256");
    let has_data_id = payload
        .get("data")
        .and_then(|d| d.get("id"))
        .map(|v| !v.is_null())
        .unwrap_or(false);
    if alg_ok && has_data_id {
        Some(format!("Cloud-IDE-JWT {}", tok.trim()))
    } else {
        None
    }
}

/// 从鉴权头提取 user_id（`data.id` → `auth_id` → `sub`；对齐 Python `extract_user_id`）。
///
/// 复用本仓库 [`crate::modules::trae_account::parse_jwt`]：它已兼容
/// `Cloud-IDE-JWT ` / `Bearer ` / 裸 token 三种前缀。
pub fn extract_user_id(auth_header: &str) -> Option<String> {
    crate::modules::trae_account::parse_jwt(auth_header).user_id
}

/// JWT exp 时间戳（秒）；解析失败 `None`
fn jwt_exp_ts(jwt_full: &str) -> Option<i64> {
    crate::modules::trae_account::parse_jwt(jwt_full).exp_timestamp
}

// ---------------- 账号库写回 ----------------

/// 按 user_id 更新/追加账号 JWT（对齐 Python `update_account_jwt` 语义：
/// exp 防降级；找不到则追加 `auto_<uid8>`；更新成功自动解除冷却）
pub fn update_account_jwt(ctx: &ProxyCtx, user_id: &str, jwt_full: &str) -> &'static str {
    update_account_jwt_and_refresh(ctx, user_id, jwt_full, None)
}

/// JWT 与（可选）refresh_token 一次持锁原子更新 + 单次落盘
///（审查修复「两段锁」：原 `update_account_jwt` + `update_account_refresh_token`
/// 分两次持锁写盘，间隙会落盘「JWT 已更新 / refresh_token 未更新」的中间态，
/// 并发读（签到 / 另一次捕获）会看到半更新数据）。
///
/// 语义对齐 Python 两函数合用：JWT exp 防降级（`skipped` 不阻塞 refresh 更新）、
/// refresh 同值 `unchanged`、追加账号时两者一并写入。
fn update_account_jwt_and_refresh(
    ctx: &ProxyCtx,
    user_id: &str,
    jwt_full: &str,
    refresh_token: Option<&str>,
) -> &'static str {
    let _g = accounts_lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut accounts = crate::modules::trae_account::load_accounts();
    let new_exp = jwt_exp_ts(jwt_full);
    let exp_str = |ts: Option<i64>| {
        ts.and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "?".to_string())
    };
    let new_exp_str = exp_str(new_exp);
    let now = crate::modules::config::utc_iso();

    let idx = accounts
        .iter()
        .position(|a| a.get("user_id").and_then(|v| v.as_str()) == Some(user_id));

    if let Some(i) = idx {
        let (old_jwt, name) = {
            let acc = &accounts[i];
            (
                acc.get("jwt").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                acc.get("name").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
            )
        };
        let mut changed = false;
        let mut jwt_status = "unchanged";
        if old_jwt != jwt_full {
            // 防降级：新 token 过期时间不晚于旧的 → 跳过 JWT（用 uid= 记法避免误触发捕获事件）
            if let (Some(old_ts), Some(new_ts)) = (jwt_exp_ts(&old_jwt), new_exp) {
                if new_ts <= old_ts {
                    ctx.log.log(&format!(
                        "  [JWT 跳过(更旧)] uid={user_id} 账号={name} 旧 exp={} 新 exp={new_exp_str}",
                        exp_str(Some(old_ts))
                    ));
                    jwt_status = "skipped";
                }
            }
            if jwt_status != "skipped" {
                if let Some(obj) = accounts[i].as_object_mut() {
                    obj.insert("jwt".to_string(), serde_json::Value::String(jwt_full.to_string()));
                    obj.insert("updated_at".to_string(), serde_json::Value::String(now.clone()));
                    // 新 token 到手 = 刷新链路健康，清掉历史失败标记
                    obj.insert("refresh_token_fails".to_string(), serde_json::json!(0));
                    obj.insert("refresh_token_invalid".to_string(), serde_json::json!(false));
                }
                ctx.log.log(&format!("  [JWT 自动更新] user={user_id} 账号={name} exp={new_exp_str}"));
                changed = true;
                jwt_status = "updated";
            }
        }
        if let Some(rt) = refresh_token {
            if accounts[i].get("refresh_token").and_then(|v| v.as_str()) != Some(rt) {
                if let Some(obj) = accounts[i].as_object_mut() {
                    obj.insert("refresh_token".to_string(), serde_json::Value::String(rt.to_string()));
                    obj.insert("refresh_token_updated_at".to_string(), serde_json::Value::String(now));
                }
                ctx.log.log(&format!("  [refresh_token 更新] uid={user_id} 账号={name}"));
                changed = true;
            }
        }
        if changed {
            if write_accounts(ctx, &accounts).is_err() {
                ctx.log.log("  [accounts] 写入失败");
            }
            if jwt_status == "updated" {
                clear_cooldown(ctx, user_id);
            }
        }
        return jwt_status;
    }

    // 新账号追加（refresh_token 一并写入，省去追加后二次写盘；对齐 Python 两步净效果）
    let short: String = user_id.chars().take(8).collect();
    let mut new_acc = serde_json::json!({
        "user_id": user_id,
        "name": format!("auto_{short}"),
        "jwt": jwt_full,
        "added_at": now,
        "updated_at": now,
        "refresh_token_fails": 0,
        "refresh_token_invalid": false,
    });
    if let Some(rt) = refresh_token {
        new_acc["refresh_token"] = serde_json::Value::String(rt.to_string());
        new_acc["refresh_token_updated_at"] = serde_json::Value::String(
            crate::modules::config::utc_iso(),
        );
    }
    accounts.push(new_acc);
    ctx.log
        .log(&format!("  [JWT 自动追加新账号] user={user_id} -> name=auto_{short} exp={new_exp_str}"));
    if write_accounts(ctx, &accounts).is_err() {
        ctx.log.log("  [accounts] 写入失败");
    }
    "appended"
}

fn write_accounts(_ctx: &ProxyCtx, accounts: &[serde_json::Value]) -> Result<(), String> {
    crate::modules::trae_account::save_accounts(accounts).map_err(|e| e.to_string())
}

/// 新 JWT 捕获成功 → 自动解除该账号冷却（对齐 Python `clear_cooldown` 意图；
/// Python 版实为 NameError 空转，此处为真实修复）。
///
/// 本仓库的冷却表在 [`crate::modules::trae_checkin`]（`trae_cooldowns.json`）。
fn clear_cooldown(ctx: &ProxyCtx, user_id: &str) {
    if crate::modules::trae_checkin::cooldown_until(user_id).is_some() {
        crate::modules::trae_checkin::clear_cooldown(user_id);
        ctx.log.log(&format!(
            "  [冷却解除] uid={user_id}（新 JWT 捕获成功，自动解除登录失效标记）"
        ));
    }
}

// ---------------- ExchangeToken refresh_token 抓取 ----------------

/// 从 ExchangeToken/OAuth 响应体提取 refresh_token 并写回
///（对齐 Python `try_capture_refresh_token_from_response`）
fn try_capture_refresh_token(ctx: &ProxyCtx, path: &str, resp_body: &[u8]) {
    if resp_body.is_empty() || !resp_body.windows(13).any(|w| w == b"refresh_token") {
        return;
    }
    if !(path.contains("ExchangeToken") || path.to_ascii_lowercase().contains("oauth")) {
        return;
    }
    let Ok(data) = serde_json::from_slice::<serde_json::Value>(resp_body) else {
        return;
    };
    let inner = data.get("data").filter(|v| v.is_object()).unwrap_or(&data);
    let Some(rt) = inner.get("refresh_token").and_then(|v| v.as_str()) else {
        return;
    };
    let at = inner
        .get("access_token")
        .and_then(|v| v.as_str())
        .or_else(|| inner.get("token").and_then(|v| v.as_str()))
        .unwrap_or("");
    if at.is_empty() {
        ctx.log.log("  [refresh_token] 响应中含 refresh_token 但无法提取 user_id，跳过");
        return;
    }
    // 已带前缀的 access_token 直接采信（对齐 Python），否则校验
    let validated = if at.starts_with("Cloud-IDE-JWT") {
        at.to_string()
    } else {
        match valid_cloud_ide_jwt(at) {
            Some(v) => v,
            None => {
                ctx.log.log("  [refresh_token] 响应中含 refresh_token 但无法提取 user_id，跳过");
                return;
            }
        }
    };
    let Some(uid) = extract_user_id(&validated) else {
        ctx.log.log("  [refresh_token] 响应中含 refresh_token 但无法提取 user_id，跳过");
        return;
    };
    // 一次持锁同时更新 JWT 与 refresh_token（审查修复：两段锁间隙会落盘半更新中间态）
    update_account_jwt_and_refresh(ctx, &uid, &validated, Some(rt));
}

// ---------------- 真实设备指纹抓取 ----------------

/// 从请求头捕获 Trae 客户端**真实**使用的设备指纹并写入设备表。
///
/// 为什么值得抓：服务端把 JWT 与签发时的设备指纹绑定，抓到的真值一定匹配；
/// [`crate::modules::trae_device::derive_device`] 的派生值只是「客户端没被代理过」的
/// 推断。有真值就该覆盖派生值（[`crate::modules::trae_device::record_captured_device`]
/// 的合并语义：只覆盖非空字段，缺失字段保留原值）。
///
/// 仅在能同时拿到 uid 与 `x-device-id` 时才写：没有 uid 的设备指纹无处归属。
fn try_capture_device_identity(ctx: &ProxyCtx, req: &RawRequest) {
    let Some(auth) = auth_header_value(req) else { return };
    let raw = auth.strip_prefix("Cloud-IDE-JWT ").unwrap_or(auth);
    let Some(uid) = extract_user_id(raw) else { return };
    let Some(device_id) = req.hget("x-device-id").map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    let session_id = req.hget("vscode-sessionid").map(str::trim).filter(|s| !s.is_empty());
    let market = req.hget("x-market-user-id").map(str::trim).filter(|s| !s.is_empty());
    // 与设备表比对后再决定是否落盘/记日志，避免每请求都刷日志
    let existing = crate::modules::trae_device::resolve_device(&uid);
    if existing.device_id == device_id
        && session_id.map(|s| s == existing.session_id).unwrap_or(true)
        && market.map(|m| m == existing.market_user_id).unwrap_or(true)
    {
        return;
    }
    crate::modules::trae_device::record_captured_device(&uid, device_id, session_id, market);
    ctx.log.log(&format!(
        "  [设备捕获] uid={uid} x-device-id={device_id} vscode-sessionid={} x-market-user-id={}",
        session_id.unwrap_or("无"),
        market.unwrap_or("无"),
    ));
}

// ---------------- 豆包会话凭证抓取 ----------------

/// Cookie 请求头宽松解析（对齐 Python `_parse_cookie_header`）
pub fn parse_cookie_header(cookie: &str) -> Vec<(String, String)> {
    cookie
        .split(';')
        .filter_map(|part| {
            let (k, v) = part.split_once('=')?;
            Some((k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        })
        .collect()
}

/// 从 `multi_sids` cookie 解析当前登录 uid（对齐 Python `_doubao_uid_from_multi_sids`）。
///
/// `multi_sids` 形态是 `uid1:sid1|uid2:sid2`（URL 编码），一个浏览器里可能挂着多个
/// 字节系账号的会话。**必须用 sessionid 去比中对应那一项**，否则会把 A 账号的
/// sessionid 记到 B 账号名下（跨账号凭证污染）。
pub fn doubao_uid_from_multi_sids(cookie: &str, session_id: &str) -> String {
    parse_multi_sids_map(cookie)
        .into_iter()
        .find(|(_, sid)| sid == session_id)
        .map(|(uid, _)| uid)
        .unwrap_or_default()
}

/// 从 `multi_sids` cookie 解析出**全部** `(uid, sessionid)` 项。
///
/// 与 [`doubao_uid_from_multi_sids`] 的关系：那个函数是「本次会话是谁」（必须 sid 匹配），
/// 这个是「这台机器上还挂着谁」。两者共用同一套解码与合法性校验，区别只在调用方
/// 是否按 sessionid 过滤 —— 因此这里刻意**不做** sid 匹配，过滤留给调用方。
///
/// 存在的意义：一个豆包客户端可同时挂多个账号（`uidA:sidA|uidB:sidB`），
/// 而一次请求只会带当前那一个 sessionid。只看当前项会让其余账号的 sessionid
/// 白丢 —— 而它们本来就在同一个 cookie 里，属于「已经读得到却没填进去」。
///
/// 合法性：uid 必须非空且纯数字，sid 必须非空。畸形项直接跳过。
pub fn parse_multi_sids_map(cookie: &str) -> Vec<(String, String)> {
    // OnceLock 单次编译（审查修复：原每次调用重编译正则）
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"multi_sids=([^;\s]+)").expect("multi_sids regex"));
    let Some(m) = re.captures(cookie) else {
        return Vec::new();
    };
    let raw = urlencoding::decode(m.get(1).map(|g| g.as_str()).unwrap_or(""))
        .unwrap_or_default()
        .to_string();
    let mut out: Vec<(String, String)> = Vec::new();
    for pair in raw.split(['|', ';']) {
        let Some((uid, sid)) = pair.split_once(':') else {
            continue;
        };
        let (uid, sid) = (uid.trim(), sid.trim());
        if uid.is_empty() || sid.is_empty() || !uid.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        // 同一 uid 重复出现时保留先到者：cookie 里的先后顺序即服务端给出的优先级
        if out.iter().any(|(u, _)| u == uid) {
            continue;
        }
        out.push((uid.to_string(), sid.to_string()));
    }
    out
}

/// `doubao.com` 域请求 Cookie 中提取会话凭证，变化时经
/// [`crate::modules::doubao_account::save_captured`] 落盘（对齐 Python
/// `try_capture_doubao_credentials`）。
///
/// 硬性约束（都是实测踩坑）：
/// 1. **host 必须匹配 `.doubao.com` 后缀** —— 字节系其他站点也有同名 cookie，
///    混进来会污染豆包账号池；
/// 2. `sessionid` 为空即放弃（没有它整条凭证毫无意义）；
/// 3. `multi_sids` 里的 sid **必须等于本次抓到的 sessionid** 才认 uid（见上）；
/// 4. 与上一次捕获逐字段比对，未变化不写盘（否则每请求都刷文件时间戳）。
pub fn try_capture_doubao_credentials(
    ctx: &ProxyCtx,
    host: &str,
    req_headers: &[(String, String)],
    resp_headers: &[(String, String)],
) {
    const CRED_COOKIES: &[&str] = &["sessionid", "sid_guard", "ttwid"];
    let host_l = host.to_ascii_lowercase();
    if !host_l.ends_with(".doubao.com") {
        return;
    }
    let cookie = req_headers
        .iter()
        .rev()
        .find(|(k, _)| k.eq_ignore_ascii_case("cookie"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let mut jar = parse_cookie_header(&cookie);
    // 兜底：响应 Set-Cookie 中补齐请求缺失的凭证 cookie
    for (k, v) in resp_headers {
        if !k.eq_ignore_ascii_case("set-cookie") {
            continue;
        }
        let first = v.split(';').next().unwrap_or("");
        if let Some((ck, cv)) = first.split_once('=') {
            let ck = ck.trim();
            if CRED_COOKIES.contains(&ck) && !jar.iter().any(|(k, _)| k == ck) {
                jar.push((ck.to_string(), cv.trim().to_string()));
            }
        }
    }
    let get = |name: &str| {
        jar.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let session_id = get("sessionid");
    // 硬性约束 2：sessionid 为空直接放弃
    if session_id.is_empty() {
        return;
    }
    let sid_guard = get("sid_guard");
    let ttwid = get("ttwid");
    let uid = doubao_uid_from_multi_sids(&cookie, &session_id);
    // 全量 `multi_sids` 映射：uid → sessionid。**本次会话的那一项也在内**，
    // 它是「已知正确」的锚点，回写时据此把同机其余账号的 sid 也补上
    // （见 `doubao_account::auto_apply_captured`）。
    let sids: serde_json::Map<String, serde_json::Value> = parse_multi_sids_map(&cookie)
        .into_iter()
        .map(|(u, s)| (u, serde_json::Value::String(s)))
        .collect();
    let captured = serde_json::json!({
        "session_id": session_id,
        "sid_guard": sid_guard,
        "ttwid": ttwid,
        "uid": uid,
        "sids": sids,
        "host": host_l,
        "captured_at": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
    });
    let mut cache = doubao_cache().lock().unwrap_or_else(|e| e.into_inner());
    let unchanged = cache
        .as_ref()
        .map(|c| {
            c.get("session_id") == captured.get("session_id")
                && c.get("sid_guard") == captured.get("sid_guard")
                && c.get("ttwid") == captured.get("ttwid")
                && c.get("uid") == captured.get("uid")
                // sids 映射必须参与比对：同机其它账号登录/登出会只改映射不改
                // sessionid，漏比对会让新账号的 sid 一直不落盘
                && c.get("sids") == captured.get("sids")
        })
        .unwrap_or(false);
    if unchanged {
        return;
    }
    *cache = Some(captured.clone());
    // 复用本仓库既有的抓包凭证落盘（`<store_dir>/doubao_captured_credential.json`），
    // 凭证回写（doubao_account::auto_apply_captured）读的就是这个文件
    if let Err(e) = crate::modules::doubao_account::save_captured(&captured) {
        ctx.log.log(&format!("  [doubao] 凭证落盘失败: {e}"));
    }
    // 只记长度，绝不记值（凭证就是账号密码）
    ctx.log.log(&format!(
        "  [doubao] 抓到会话凭证: sessionid={} 字符{}{}{}",
        session_id.len(),
        if sid_guard.is_empty() { String::new() } else { format!("，sid_guard={} 字符", sid_guard.len()) },
        if ttwid.is_empty() { String::new() } else { format!("，ttwid={} 字符", ttwid.len()) },
        if uid.is_empty() { "（uid 未识别）".to_string() } else { format!("，uid={uid}") },
    ));
}

// ---------------- 上游转发 ----------------

/// 一次转发结果：`false` 表示上游要求关闭连接或出错（对齐 Python keep-alive 语义）
async fn forward_upstream<S: AsyncRead + AsyncWrite + Unpin>(
    io: &mut tokio_rustls::server::TlsStream<S>,
    client: &Client<UpstreamConnector, Full<Bytes>>,
    ctx: &ProxyCtx,
    host: &str,
    port: u16,
    req: &RawRequest,
) -> bool {
    let method = req.method.clone();
    let is_stream = req.path.contains("llm_utils_chat")
        || req
            .hget("accept")
            .map(|v| v.contains("event-stream"))
            .unwrap_or(false);
    ctx.log.log(&format!(
        "  [forward] -> {method} https://{host}:{port}{} ({} bytes body)",
        req.path,
        req.body.len()
    ));

    // 组装上游请求：过滤跳过头 + accept-encoding 降级（br/zstd 无解压支持，对齐 Python）
    let uri = format!("https://{host}:{port}{}", req.path);
    let mut builder = Request::builder()
        .method(method.as_str())
        .uri(&uri)
        .header("host", format!("{host}:{port}"));
    let mut header_pairs: Vec<(String, String)> = Vec::new();
    for (k, v) in &req.headers {
        if HOP_BY_HOP_REQ.iter().any(|h| k.eq_ignore_ascii_case(h)) {
            continue;
        }
        let v = if k.eq_ignore_ascii_case("accept-encoding") {
            "gzip, deflate".to_string()
        } else {
            v.clone()
        };
        header_pairs.push((k.clone(), v));
    }
    for (k, v) in &header_pairs {
        if let (Ok(name), Ok(val)) = (
            k.parse::<hyper::header::HeaderName>(),
            v.parse::<hyper::header::HeaderValue>(),
        ) {
            builder = builder.header(name, val);
        }
    }
    // Python：GET 请求不带 body
    let body_bytes = if method.eq_ignore_ascii_case("GET") { Bytes::new() } else { req.body.clone() };
    let request = builder.body(Full::new(body_bytes)).expect("upstream request build");

    let timeout_dur = if is_stream { STREAM_TIMEOUT } else { PLAIN_TIMEOUT };
    let resp = match timeout(timeout_dur, client.request(request)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            ctx.log
                .log(&format!("  [forward] 错误: {e} (host={host}, path={})", req.path));
            let _ = send_response(io, 502, "Bad Gateway", &[], b"Bad Gateway").await;
            return false;
        }
        Err(_) => {
            ctx.log.log(&format!(
                "  [forward] 超时 (timeout={}s): {host}{}",
                timeout_dur.as_secs(),
                req.path
            ));
            let _ = send_response(io, 504, "Gateway Timeout", &[], b"Gateway Timeout").await;
            return false;
        }
    };

    let status = resp.status().as_u16();
    let resp_pairs: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).to_string()))
        .collect();
    let reason = reason_phrase(status);

    if is_stream {
        return stream_response(io, ctx, host, req, resp).await;
    }

    // 非流式：整体读取，Limited 超限即断 + 逐帧空闲超时（差异修复：对齐 Python
    // socket timeout 30s 的逐读语义，防 trickling body 拖住连接与并发槽）
    match collect_body_with_idle_timeout(Limited::new(resp.into_body(), MAX_RESP_BODY), PLAIN_TIMEOUT).await
    {
        Ok(resp_body) => {
            ctx.log.log(&format!(
                "  [forward] <- {status} {reason} ({}) bytes from {host}{}",
                resp_body.len(),
                req.path
            ));
            if send_response(io, status, reason, &resp_pairs, &resp_body).await.is_err() {
                return false;
            }
            // 凭据抓取 + 请求日志
            try_capture_refresh_token(ctx, &req.path, &resp_body);
            try_capture_doubao_credentials(ctx, host, &req.headers, &resp_pairs);
            ctx.req_logger.log_request(
                &method,
                host,
                &req.path,
                &req.headers,
                &req.body,
                status,
                reason,
                &resp_pairs,
                &resp_body,
            );
            // 上游要求关闭连接 → 退出 keep-alive 循环
            !resp_pairs
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("connection") && v.eq_ignore_ascii_case("close"))
        }
        Err(e) => {
            ctx.log
                .log(&format!("  [forward] 读上游响应失败: {e} (host={host}, path={})", req.path));
            let _ = send_response(io, 502, "Bad Gateway", &[], b"Bad Gateway").await;
            false
        }
    }
}

/// 流式（SSE）响应转发：逐块 chunked 推给客户端，同步累积日志缓冲（对齐 Python `_stream_response`）
async fn stream_response<S: AsyncRead + AsyncWrite + Unpin>(
    io: &mut tokio_rustls::server::TlsStream<S>,
    ctx: &ProxyCtx,
    host: &str,
    req: &RawRequest,
    resp: hyper::Response<hyper::body::Incoming>,
) -> bool {
    let status = resp.status().as_u16();
    let reason = reason_phrase(status);
    let resp_pairs: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).to_string()))
        .collect();

    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (k, v) in &resp_pairs {
        if HOP_BY_HOP_RESP.iter().any(|h| k.eq_ignore_ascii_case(h)) {
            continue;
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("Transfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n");
    if io.write_all(head.as_bytes()).await.is_err() {
        return false;
    }
    let _ = io.flush().await;
    ctx.log
        .log(&format!("  [forward] <- {status} {reason} (streaming) from {host}{}", req.path));

    let mut total = 0usize;
    let mut logged: Vec<u8> = Vec::new();
    let mut interrupted = false;
    let mut body = resp.into_body();
    loop {
        // 逐帧空闲超时（对齐 Python 流式分支 300s socket timeout 的逐读语义；
        // 差异修复：原裸 frame() 无超时，上游僵死时隧道永久挂起）
        match timeout(STREAM_TIMEOUT, body.frame()).await {
            Err(_) => {
                ctx.log.log(&format!(
                    "  [forward] 流式空闲超时 ({}s)，中断 (已转发 {total} bytes)",
                    STREAM_TIMEOUT.as_secs()
                ));
                interrupted = true;
                break;
            }
            Ok(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    let chunk: &[u8] = data;
                    let mut pkt = format!("{:x}\r\n", chunk.len()).into_bytes();
                    pkt.extend_from_slice(chunk);
                    pkt.extend_from_slice(b"\r\n");
                    if io.write_all(&pkt).await.is_err() || io.flush().await.is_err() {
                        interrupted = true;
                        break;
                    }
                    total += chunk.len();
                    if logged.len() < MAX_LOG_BODY {
                        logged.extend_from_slice(chunk);
                    }
                }
            }
            Ok(Some(Err(e))) => {
                ctx.log
                    .log(&format!("  [forward] 流式转发中断: {e} (已转发 {total} bytes)"));
                interrupted = true;
                break;
            }
            Ok(None) => break,
        }
    }
    if !interrupted {
        let _ = io.write_all(b"0\r\n\r\n").await;
        let _ = io.flush().await;
    }
    ctx.log.log(&format!(
        "  [forward] 流式转发{}，共 {total} bytes",
        if interrupted { "中断" } else { "完成" }
    ));
    ctx.req_logger.log_request(
        &req.method,
        host,
        &req.path,
        &req.headers,
        &req.body,
        status,
        reason,
        &resp_pairs,
        &logged,
    );
    !interrupted
}

// ---------------- MITM 连接主循环 ----------------

/// 处理一条已解密的 MITM TLS 连接（对齐 Python `tunnel_https` keep-alive 循环）
pub async fn serve_mitm<S: AsyncRead + AsyncWrite + Unpin>(
    mut io: tokio_rustls::server::TlsStream<S>,
    host: String,
    port: u16,
    ctx: Arc<ProxyCtx>,
) {
    ctx.log
        .log(&format!("  [MITM] 进入 HTTPS 解密隧道: {host}:{port}"));
    // Python forward_upstream 每请求新建 HTTPSConnection（无连接池）；
    // 此处每连接建一个 Client 等价复用，MITM 上游恒为直连（对齐 Python）
    let client: Client<UpstreamConnector, Full<Bytes>> =
        Client::builder(TokioExecutor::new()).build(UpstreamConnector::new(None, ctx.log.clone()));

    loop {
        // 首读给 300s 超时（对齐握手期 _CONN_TIMEOUT；keep-alive 空闲由客户端断开驱动）
        let req = match timeout(CONN_TIMEOUT, read_raw_request(&mut io)).await {
            Err(_) => {
                ctx.log.log(&format!("  [MITM] 读取 TLS 请求超时: {host}:{port}"));
                break;
            }
            Ok(Ok(None)) => {
                ctx.log.log(&format!("  [MITM] 客户端关闭连接: {host}:{port}"));
                break;
            }
            Ok(Err(e)) => {
                ctx.log.log(&format!("  [MITM] 读取 TLS 请求错误: {e}"));
                break;
            }
            Ok(Ok(Some(r))) => r,
        };

        let is_target = ctx.host_in_targets(&host);
        let tag = if is_target { " [TRAE]" } else { "" };
        let ep_tag = ctx
            .classify_path(&req.path)
            .map(|n| format!("  <{n}>"))
            .unwrap_or_default();
        ctx.log
            .log(&format!("  {} {host}{}{tag}{ep_tag}", req.method, req.path));

        // 鉴权头嗅探（诊断，每 host 一次）+ JWT 自动捕获（不限 host，对齐 Python）
        if ctx.auto_capture_jwt {
            if let Some(auth) = auth_header_value(&req) {
                let raw = auth.strip_prefix("Cloud-IDE-JWT ").unwrap_or(auth);
                static AUTH_HINT: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
                let is_valid = valid_cloud_ide_jwt(raw).is_some();
                let seen = AUTH_HINT.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
                let inserted = seen
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(format!("auth:{host}"));
                if inserted {
                    let marker = if is_valid { "Cloud-IDE-JWT ✅可捕获" } else { "其他/不可识别" };
                    let prefix: String = auth.chars().take(18).collect();
                    ctx.log.log(&format!(
                        "  [JWT-DEBUG] 在 {host} 发现鉴权头 (类型: {marker}: {prefix}…)"
                    ));
                }
                if let (true, Some(valid)) = (is_valid, valid_cloud_ide_jwt(raw)) {
                    if let Some(uid) = extract_user_id(&valid) {
                        update_account_jwt(&ctx, &uid, &valid);
                    }
                }
                // 真实设备指纹捕获（与 JWT 同源：都由鉴权头定位账号）
                try_capture_device_identity(&ctx, &req);
            }
        }

        // 设备头改写（仅签到接口）：改写后走专用转发并继续 keep-alive 循环
        if req.path.contains(SIGNIN_PATH) {
            let uid = auth_header_value(&req).and_then(extract_user_id);
            match uid {
                Some(uid) => {
                    // 抓包真值优先（record_captured_device 写入的），缺失才用派生值
                    let dev = crate::modules::trae_device::resolve_device(&uid);
                    let mut rewritten = req;
                    rewritten.hset("x-device-id", dev.device_id.clone());
                    // 派生值缺失时不注入空头（审查修复：空值头非法且可能被上游 400）
                    if !dev.market_user_id.is_empty() {
                        rewritten.hset("x-market-user-id", dev.market_user_id.clone());
                    }
                    if !dev.session_id.is_empty() {
                        rewritten.hset("vscode-sessionid", dev.session_id.clone());
                    }
                    ctx.log.log(&format!(
                        "  [签到改写] uid={uid} -> x-device-id={} x-market-user-id={} vscode-sessionid={}",
                        dev.device_id,
                        if dev.market_user_id.is_empty() { "无" } else { &dev.market_user_id },
                        if dev.session_id.is_empty() { "无" } else { &dev.session_id },
                    ));
                    let keep = forward_upstream(&mut io, &client, &ctx, &host, port, &rewritten).await;
                    if !keep {
                        break;
                    }
                    continue;
                }
                None => ctx.log.log("  [签到] 未解析到 user id，未改写"),
            }
        }

        // WebSocket 升级 → 专用通道接管连接
        if req.is_websocket_upgrade() {
            ctx.log.log(&format!(
                "  [WebSocket] 检测到升级请求: {} {host}:{port}{} (Connection={})",
                req.method,
                req.path,
                req.hget("connection").unwrap_or("?")
            ));
            crate::modules::device_proxy::ws::forward_websocket(&mut io, &ctx, &host, port, &req).await;
            break; // WS 连接已接管，退出 keep-alive 循环
        }

        let keep = forward_upstream(&mut io, &client, &ctx, &host, port, &req).await;
        if !keep {
            break;
        }
    }
    let _ = io.shutdown().await;
}

/// 取鉴权头值（authorization / x-cloudide-token / x-icube-token，对齐 Python 顺序）
fn auth_header_value(req: &RawRequest) -> Option<&str> {
    for name in ["authorization", "x-cloudide-token", "x-icube-token"] {
        if let Some(v) = req.hget(name) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::config::test_isolation::Isolated;

    #[test]
    fn jwt_validation_and_uid_extraction() {
        // 构造 header(alg=RS256) + payload(data.id=123) 的 JWT
        use base64::Engine;
        let b64 = |v: &serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
        };
        let header = b64(&serde_json::json!({"alg": "RS256", "typ": "JWT"}));
        let payload = b64(&serde_json::json!({"data": {"id": 4487568582777872i64}, "exp": 1893456000}));
        let tok = format!("{header}.{payload}.c2ln");
        let valid = valid_cloud_ide_jwt(&tok);
        assert!(valid.is_some());
        assert_eq!(extract_user_id(&valid.unwrap()).unwrap(), "4487568582777872");
        // 非 RS256 → 拒绝
        let header2 = b64(&serde_json::json!({"alg": "HS256"}));
        assert!(valid_cloud_ide_jwt(&format!("{header2}.{payload}.sig")).is_none());
        // 前缀剥离 + Bearer
        assert_eq!(extract_user_id("Cloud-IDE-JWT abc.def.ghi"), None); // 非法 token
    }

    #[test]
    fn parse_cookie_and_multi_sids() {
        let jar = parse_cookie_header("a=1; sessionid=\"abc\"; b=2");
        assert_eq!(
            jar.iter().find(|(k, _)| k == "sessionid").map(|(_, v)| v.as_str()),
            Some("abc")
        );
        let uid = doubao_uid_from_multi_sids(
            "multi_sids=111%3AsidA%7C222%3AsidB; sessionid=sidB",
            "sidB",
        );
        assert_eq!(uid, "222");
        assert_eq!(doubao_uid_from_multi_sids("multi_sids=111:sidA", "nope"), "");
    }

    /// multi_sids uid 提取：只有 sid 与本次 sessionid 相等的项才认（防跨账号污染）
    #[test]
    fn multi_sids_requires_matching_session_id() {
        // 三项，中间一项才是当前登录账号
        let cookie = "multi_sids=111%3AsidA%7C222%3AsidB%7C333%3AsidC; sessionid=sidB";
        assert_eq!(doubao_uid_from_multi_sids(cookie, "sidB"), "222");
        // sid 不匹配 → 空（宁可不写，也不能记错账号）
        assert_eq!(doubao_uid_from_multi_sids(cookie, "sidX"), "");
        // 非纯数字 uid 不认
        assert_eq!(doubao_uid_from_multi_sids("multi_sids=abc%3AsidB", "sidB"), "");
        // 无 multi_sids → 空
        assert_eq!(doubao_uid_from_multi_sids("sessionid=sidB", "sidB"), "");
        // 未编码形态（分号分隔）同样支持
        assert_eq!(doubao_uid_from_multi_sids("multi_sids=111:sidA|222:sidB", "sidB"), "222");
    }

    /// 全量 multi_sids 解析：一个客户端挂多个账号时，**所有**项都要能读出来。
    ///
    /// 为什么需要它：`multi_sids` 里本来就有同机其余账号的 sessionid，
    /// 只看当前项会让它们白丢（「读得到却没填进去」）。
    #[test]
    fn multi_sids_解析全部项() {
        let cookie = "multi_sids=111%3AsidA%7C222%3AsidB%7C333%3AsidC; sessionid=sidB";
        assert_eq!(
            parse_multi_sids_map(cookie),
            vec![
                ("111".to_string(), "sidA".to_string()),
                ("222".to_string(), "sidB".to_string()),
                ("333".to_string(), "sidC".to_string()),
            ],
            "三项都要解析出来，不能只留当前会话那一项"
        );

        // 未编码形态（`|` / `:` 原样）
        assert_eq!(
            parse_multi_sids_map("multi_sids=111:sidA|222:sidB"),
            vec![
                ("111".to_string(), "sidA".to_string()),
                ("222".to_string(), "sidB".to_string()),
            ]
        );
    }

    /// 全量解析的合法性过滤：畸形项跳过，但**不影响**同一 cookie 里的合法项。
    #[test]
    fn multi_sids_全量解析跳过畸形项() {
        // uid 非数字 / uid 空 / sid 空 都被跳过，222 保留
        let cookie = "multi_sids=abc%3AsidA%7C%3AsidB%7C222%3A%7C333%3AsidC";
        assert_eq!(
            parse_multi_sids_map(cookie),
            vec![("333".to_string(), "sidC".to_string())],
            "只剩合法项 333"
        );

        // 无 multi_sids → 空
        assert!(parse_multi_sids_map("sessionid=sidB").is_empty());
        assert!(parse_multi_sids_map("").is_empty());
        // 同一 uid 重复出现：保留先到者
        assert_eq!(
            parse_multi_sids_map("multi_sids=111:sidOld|111:sidNew"),
            vec![("111".to_string(), "sidOld".to_string())]
        );
    }

    /// 全量解析与「取当前项」必须**同源**：当前会话那一项一定在全量映射里。
    #[test]
    fn multi_sids_当前项包含在全量映射中() {
        let cookie = "multi_sids=111%3AsidA%7C222%3AsidB%7C333%3AsidC; sessionid=sidC";
        let current = doubao_uid_from_multi_sids(cookie, "sidC");
        assert_eq!(current, "333");
        assert!(
            parse_multi_sids_map(cookie).iter().any(|(u, _)| u == &current),
            "当前登录账号必须出现在全量映射里"
        );
    }

    #[test]
    fn raw_request_header_ops() {
        let mut req = RawRequest {
            method: "GET".into(),
            path: "/".into(),
            headers: vec![
                ("Host".into(), "a.com".into()),
                ("X-Token".into(), "old".into()),
            ],
            body: Bytes::new(),
        };
        assert_eq!(req.hget("x-token"), Some("old"));
        req.hset("X-TOKEN", "new".into());
        assert_eq!(req.hget("x-token"), Some("new"));
        assert_eq!(req.headers.len(), 2); // 旧值被移除
        assert!(!req.is_websocket_upgrade());
        req.hset("Upgrade", "WebSocket".into());
        assert!(req.is_websocket_upgrade());
    }

    #[test]
    fn find_head_end_positions() {
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n\r\n"), Some(14));
        assert_eq!(find_head_end(b"partial"), None);
        assert_eq!(find_head_end(b"ab\r\n\r\n"), Some(2));
    }

    #[test]
    fn reason_phrases() {
        assert_eq!(reason_phrase(200), "OK");
        assert_eq!(reason_phrase(502), "Bad Gateway");
        assert_eq!(reason_phrase(599), "");
    }

    /// 构造测试用 ProxyCtx（临时目录；不 emit 前端事件）
    fn test_ctx(tag: &str) -> ProxyCtx {
        let dir = std::env::temp_dir().join(format!(
            "ai_gateway_handler_test_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&dir);
        let captured = Arc::new(std::sync::atomic::AtomicI64::new(0));
        ProxyCtx {
            log: crate::modules::device_proxy::logger::ProxyLog::new(dir.join("proxy.log"), None, captured),
            req_logger: Arc::new(crate::modules::device_proxy::logger::RequestLogger::new(dir.clone())),
            targets: vec![],
            auto_capture_jwt: true,
            data_dir: dir.clone(),
        }
    }

    /// 构造带 exp 的 Cloud-IDE-JWT（新 exp 晚于旧 exp 用于防降级测试）
    fn make_jwt(exp: i64) -> String {
        use base64::Engine;
        let b64 = |v: &serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
        };
        let header = b64(&serde_json::json!({"alg": "RS256", "typ": "JWT"}));
        let payload = b64(&serde_json::json!({"data": {"id": "u-test"}, "exp": exp}));
        format!("Cloud-IDE-JWT {header}.{payload}.sig")
    }

    /// 审查修复回归（两段锁合并）：JWT + refresh_token 必须一次写盘同时生效，
    /// 不允许出现「JWT 已更新 / refresh_token 未更新」的中间态落盘
    #[test]
    fn jwt_and_refresh_update_atomically() {
        let _iso = Isolated::new("handler-atomic");
        let ctx = test_ctx("atomic");
        let old_jwt = make_jwt(1893456000);
        let new_jwt = make_jwt(1893456000 + 3600);
        crate::modules::trae_account::save_accounts(&[serde_json::json!({
            "user_id": "u-test", "name": "n", "jwt": old_jwt
        })])
        .unwrap();
        let status = update_account_jwt_and_refresh(&ctx, "u-test", &new_jwt, Some("rt-new"));
        assert_eq!(status, "updated");
        let accounts = crate::modules::trae_account::load_accounts();
        let acc = &accounts[0];
        assert_eq!(acc["jwt"].as_str(), Some(new_jwt.as_str()), "JWT 应已更新");
        assert_eq!(acc["refresh_token"].as_str(), Some("rt-new"), "refresh_token 应同次写盘更新");
        assert!(acc.get("refresh_token_updated_at").and_then(|v| v.as_str()).is_some());
    }

    /// JWT exp 防降级：更旧的新 token 必须被跳过，且不阻塞 refresh_token 更新
    #[test]
    fn older_jwt_is_skipped_but_refresh_still_updates() {
        let _iso = Isolated::new("handler-downgrade");
        let ctx = test_ctx("downgrade");
        let newer = make_jwt(1893456000 + 7200);
        let older = make_jwt(1893456000);
        crate::modules::trae_account::save_accounts(&[serde_json::json!({
            "user_id": "u-test", "name": "n", "jwt": newer
        })])
        .unwrap();
        let status = update_account_jwt_and_refresh(&ctx, "u-test", &older, Some("rt-2"));
        assert_eq!(status, "skipped");
        let accounts = crate::modules::trae_account::load_accounts();
        assert_eq!(accounts[0]["jwt"].as_str(), Some(newer.as_str()), "旧 JWT 不得覆盖新 JWT");
        assert_eq!(accounts[0]["refresh_token"].as_str(), Some("rt-2"), "refresh 更新不受跳过影响");
    }

    /// 审查修复回归（uid 切片）：非 ASCII uid 追加账号时按字符截断不 panic
    #[test]
    fn append_account_with_multibyte_uid() {
        let _iso = Isolated::new("handler-multibyte");
        let ctx = test_ctx("multibyte");
        let jwt = make_jwt(1893456000);
        let uid = "日本語ユーザー001";
        let status = update_account_jwt(&ctx, uid, &jwt);
        assert_eq!(status, "appended");
        let accounts = crate::modules::trae_account::load_accounts();
        let acc = &accounts[0];
        assert_eq!(acc["user_id"].as_str(), Some(uid));
        assert_eq!(acc["name"].as_str(), Some("auto_日本語ユーザー0")); // chars().take(8)
    }

    /// 豆包凭证抓取：host 必须匹配 `.doubao.com` 后缀，sessionid 为空即放弃
    #[test]
    fn doubao_capture_host_and_sessionid_gates() {
        let _iso = Isolated::new("handler-doubao");
        let ctx = test_ctx("doubao");
        let cookie = vec![(
            "Cookie".to_string(),
            "multi_sids=111%3AsidA; sessionid=sidA; ttwid=tt1".to_string(),
        )];

        // 非 doubao.com 域：即便 cookie 齐全也不抓（防字节系其他站点污染账号池）
        try_capture_doubao_credentials(&ctx, "www.example.com", &cookie, &[]);
        assert!(crate::modules::doubao_account::load_captured().is_none(), "非目标域不得落盘");

        // 裸 doubao.com（无前导点）也不匹配 `.doubao.com` 后缀
        try_capture_doubao_credentials(&ctx, "doubao.com", &cookie, &[]);
        assert!(crate::modules::doubao_account::load_captured().is_none());

        // sessionid 为空：放弃
        let no_sid = vec![("Cookie".to_string(), "ttwid=tt1".to_string())];
        try_capture_doubao_credentials(&ctx, "www.doubao.com", &no_sid, &[]);
        assert!(crate::modules::doubao_account::load_captured().is_none());

        // 正常路径：落盘并解析出 uid
        try_capture_doubao_credentials(&ctx, "www.doubao.com", &cookie, &[]);
        let captured = crate::modules::doubao_account::load_captured().expect("应已落盘");
        assert_eq!(captured["session_id"].as_str(), Some("sidA"));
        assert_eq!(captured["uid"].as_str(), Some("111"));
        assert_eq!(captured["ttwid"].as_str(), Some("tt1"));
    }

    /// 豆包凭证抓取：请求 Cookie 缺失时用响应 Set-Cookie 补齐；同值不重复落盘
    #[test]
    fn doubao_capture_falls_back_to_set_cookie_and_dedupes() {
        let _iso = Isolated::new("handler-doubao-set");
        let ctx = test_ctx("doubao-set");
        let set_cookie = vec![
            ("Set-Cookie".to_string(), "sessionid=sidZ; Path=/; HttpOnly".to_string()),
            ("Set-Cookie".to_string(), "ttwid=ttZ; Path=/".to_string()),
        ];
        try_capture_doubao_credentials(&ctx, "api.doubao.com", &[], &set_cookie);
        let captured = crate::modules::doubao_account::load_captured().expect("Set-Cookie 兜底应落盘");
        assert_eq!(captured["session_id"].as_str(), Some("sidZ"));
        assert_eq!(captured["ttwid"].as_str(), Some("ttZ"));

        // 同值再抓一次：不重复写盘（时间戳不变）
        let before = std::fs::metadata(crate::modules::doubao_account::captured_file())
            .and_then(|m| m.modified())
            .ok();
        std::thread::sleep(std::time::Duration::from_millis(20));
        try_capture_doubao_credentials(&ctx, "api.doubao.com", &[], &set_cookie);
        let after = std::fs::metadata(crate::modules::doubao_account::captured_file())
            .and_then(|m| m.modified())
            .ok();
        assert_eq!(before, after, "同值抓包不得重复落盘");
    }

    /// 真实设备指纹捕获：抓到客户端真值必须覆盖派生值，写进设备表
    /// （服务端把 JWT 与签发时指纹绑定，真值一定匹配，派生值只是推断）
    #[test]
    fn captured_device_identity_overrides_derived() {
        let _iso = Isolated::new("handler-device");
        let ctx = test_ctx("device");
        let jwt = make_jwt(1893456000);
        let req = RawRequest {
            method: "GET".into(),
            path: "/api/x".into(),
            headers: vec![
                ("Authorization".into(), jwt.clone()),
                ("x-device-id".into(), "987654321098765".into()),
                ("vscode-sessionid".into(), "f".repeat(32)),
                ("x-market-user-id".into(), "11111111-2222-4333-8444-555555555555".into()),
            ],
            body: Bytes::new(),
        };
        try_capture_device_identity(&ctx, &req);

        let map = crate::modules::trae_device::load_device_map();
        let entry = map.get("u-test").expect("设备表应写入 u-test");
        assert_eq!(entry["device_id"].as_str(), Some("987654321098765"));
        assert_eq!(entry["session_id"].as_str(), Some("f".repeat(32).as_str()));
        assert_eq!(
            entry["market_user_id"].as_str(),
            Some("11111111-2222-4333-8444-555555555555")
        );

        // 后续 resolve_device 必须返回抓到的真值（签到注入用这条路径）
        let dev = crate::modules::trae_device::resolve_device("u-test");
        assert_eq!(dev.device_id, "987654321098765");
    }

    /// 缺少 uid 或 x-device-id 时不写设备表（没有归属的指纹无处安放）
    #[test]
    fn device_capture_requires_uid_and_device_id() {
        let _iso = Isolated::new("handler-device-gate");
        let ctx = test_ctx("device-gate");
        // 只有 x-device-id，没有鉴权头 → 不写
        let no_auth = RawRequest {
            method: "GET".into(),
            path: "/".into(),
            headers: vec![("x-device-id".into(), "111111111111111".into())],
            body: Bytes::new(),
        };
        try_capture_device_identity(&ctx, &no_auth);
        assert!(crate::modules::trae_device::load_device_map().is_empty());

        // 有鉴权头但缺 x-device-id → 不写
        let no_dev = RawRequest {
            method: "GET".into(),
            path: "/".into(),
            headers: vec![("Authorization".into(), make_jwt(1893456000))],
            body: Bytes::new(),
        };
        try_capture_device_identity(&ctx, &no_dev);
        assert!(crate::modules::trae_device::load_device_map().is_empty());

        // 空值 x-device-id → 不写（空值头非法，写进去会污染设备表）
        let empty_dev = RawRequest {
            method: "GET".into(),
            path: "/".into(),
            headers: vec![
                ("Authorization".into(), make_jwt(1893456000)),
                ("x-device-id".into(), "   ".into()),
            ],
            body: Bytes::new(),
        };
        try_capture_device_identity(&ctx, &empty_dev);
        assert!(crate::modules::trae_device::load_device_map().is_empty());
    }
}
