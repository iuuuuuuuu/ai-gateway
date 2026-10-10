//! 可编排的 mock 上游 —— **测试资产**（对应侦察报告 F-09）。
//!
//! 移植自 `zcode-dengyie/tests/mock_upstream/server.py`（413 行 FastAPI），
//! 按 Rust 侧既有约定收敛：**`std::net::TcpListener` + 手写 HTTP/1.1**
//!（`Cargo.toml` 的 tokio **无 `net` feature**），范例见
//! `crate::upstream::tasks` 测试模块的 `spawn_fake_upstream`。
//!
//! # 为什么需要它（侦察报告 `:238` 点名的缺口）
//!
//! 我们有大量零散的 `httptest.NewServer` 式假上游（每处各写一遍、各自只回一个
//! 固定响应），但**没有一个有状态的、可按账号/按调用次数编排的 mock**。后果是
//! 线上实测到的**故障形态**没法固化成回归测试 —— 报告点名两条：
//!
//! | 形态 | 来源 | 本文档对应场景 |
//! |---|---|---|
//! | 429 但 body 是额度文案 | `server.py:222-225`（线上 2026-09-13 实测） | [`Scenario::RateLimitedQuotaBody`] |
//! | 405 承载 `{"code":3012,…}` | `server.py:232-234`（线上 2026-09-05 实测） | [`Scenario::RiskControl3012`] |
//!
//! # 控制协议（请求头，对齐 `server.py:11-16`）
//!
//! ```text
//! x-mock-scenario:  <name>        本请求的故障场景（缺省 ok）
//! x-mock-sequence:  s1,s2,...     按**调用次数**依序消费，耗尽后保持最后一个
//! x-mock-bind:      <凭证前缀>    与凭证绑定；网关不会带这个头，
//!                                 测试侧先注入到账号的上游头里即可实现「按账号注入」
//! ```
//!
//! 与 Python 版的一处**刻意收敛**：那边把 sequence 存在 `app.state`（进程级
//! 共享字典，按 bind 分账），Rust 侧改成**从请求头读 `x-mock-sequence`** ——
//! 无共享状态、可并行跑测试、每个用例自带脚本。计数仍按 `x-mock-bind`
//!（缺省回落到 `authorization`/`x-api-key` 前 16 字符）分账，故「按账号编排」
//! 的语义不变。
//!
//! # 观测协议
//!
//! * 响应头回 `x-mock-call-index`：该 bind 第几次被调用（从 0 开始）。
//! * [`MockUpstream::calls`] 记录全部 `(method, path, headers, body_bytes)`。
//!
//! 同时兼容 `server.py` 的 `x-mock-call-index` 语义，测试可直接断言。

#![allow(dead_code)] // 测试资产：被不同用例按需取用，未用到的成员不应报 warning

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// 一次被记录的请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedCall {
    /// HTTP 方法。
    pub method: String,
    /// 请求路径（含 query）。
    pub path: String,
    /// 全部请求头（键小写，便于大小写不敏感断言）。
    pub headers: Vec<(String, String)>,
    /// 请求体原始字节。
    pub body: Vec<u8>,
    /// 该 bind 的调用序号（从 0 开始）。
    pub call_index: u64,
}

impl RecordedCall {
    /// 取一个请求头（键大小写不敏感）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// 请求体的 UTF-8 文本（非法字节按替换字符处理，测试断言足够）。
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// 故障场景（对齐 `server.py:216-241` 的 `_messages_result`）。
///
/// ★ 两条**线上实测形态**（本资产存在的理由）排在最前，且字段顺序即优先级。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scenario {
    /// 正常 200（JSON 或 SSE，按请求体的 `stream` 字段决定）。
    Ok,
    /// ★★ **429 + 额度文案**（线上 2026-09-13 实测：api.z.ai 频控 429 但 body
    /// 带额度文案）。
    ///
    /// 归类比想象的绕：429 通常判限流，而 body 里的额度文案指向配额 ——
    /// 我们的 [`crate::products::zcode::errors::classify`] 明确要求
    /// **文案里的额度信号先于状态码**。故这里钉住的是「**不被误判成限流**」。
    RateLimitedQuotaBody,
    /// 429 + `rate limited` 文案（真正的限流，作为上面的**反证对照**）。
    RateLimited,
    /// ★★ **405 承载 3012**（线上 2026-09-05 实测）。
    ///
    /// 405 这个载体很反直觉（风控一般是 403/429），而它**同时**是「缺官方
    /// system 块」的症状与风控的载体 —— 见 `products/zcode/systemblock.rs` 文件头。
    RiskControl3012,
    /// 200 + `{"code":3007}`（人机验证）。
    ///
    /// ⚠ 注意 `server.py:230-231` 给的是 **400**，而我们 `errors.rs:100-101`
    /// 记录的实测形态同样是 **400**。故这里用 400 —— 与两处权威一致。
    Captcha3007,
    /// 403 + `captcha verify failed`（另一条验证码形态，`server.py:228-229`）。
    CaptchaChallenge,
    /// SSE 流**中途截断**（`server.py:188-192`）。
    SseTruncate,
    /// 200 + 完全空的 SSE 流：复现 2026-10-11 国际区整体空流现场。
    EmptyStream,
    /// 401 + `invalid api key`（凭证无效）。
    AuthInvalid,
    /// 5xx（上游故障）。
    ServerError,
    /// 402 + `insufficient balance`（额度耗尽）。
    QuotaExhausted,
    /// 404（路由不存在）。
    NotFound,
    /// 200 但 body 不是 JSON（HTML 错误页；`server.py:239-240`）。
    GarbageBody,
    /// 未收录的场景名（原样透传，便于测试自定义）。
    Other(String),
}

impl Scenario {
    /// 解析场景名（大小写不敏感、容忍空白与连字符）。
    pub fn parse(name: &str) -> Self {
        let key = name.trim().to_ascii_lowercase().replace('-', "_");
        match key.as_str() {
            "" | "ok" => Scenario::Ok,
            "rate_limited_quota_body" => Scenario::RateLimitedQuotaBody,
            "rate_limited" => Scenario::RateLimited,
            "risk_control_3012" | "risk_control" => Scenario::RiskControl3012,
            "captcha_3007" => Scenario::Captcha3007,
            "captcha_challenge" => Scenario::CaptchaChallenge,
            "sse_truncate" => Scenario::SseTruncate,
            "empty_stream" => Scenario::EmptyStream,
            "auth_invalid" => Scenario::AuthInvalid,
            "server_error" => Scenario::ServerError,
            "quota_exhausted" => Scenario::QuotaExhausted,
            "not_found" => Scenario::NotFound,
            "garbage_body" => Scenario::GarbageBody,
            other => Scenario::Other(other.to_string()),
        }
    }

    /// 该场景的 `(status, body, content_type, extra_headers)`。
    ///
    /// 该场景的 `(status, body_text, content_type, extra_headers)` —— **给测试断言用**。
    ///
    /// 暴露它是为了让回归测试能「先确认夹具长什么样、再断言归类」，
    /// 而不是把响应体在测试里再抄一遍（抄一遍就会与夹具漂移）。
    pub fn response_for_test(
        self,
        stream: bool,
        truncate_at: Option<usize>,
    ) -> (u16, String, &'static str, Vec<(String, String)>) {
        let (status, body, ctype, extra) = self.response(stream, truncate_at);
        (status, String::from_utf8_lossy(&body).into_owned(), ctype, extra)
    }

    /// 该场景的 `(status, body, content_type, extra_headers)`。
    ///
    /// `stream` = 请求体里 `"stream":true`（决定 `Ok` 回 JSON 还是 SSE）。
    /// `truncate_at` = `x-mock-sse-truncate-at`（SSE 截断位置；缺省截一半）。
    fn response(
        &self,
        stream: bool,
        truncate_at: Option<usize>,
    ) -> (u16, Vec<u8>, &'static str, Vec<(String, String)>) {
        match self {
            Scenario::Ok => {
                if stream {
                    // 正常流式：完整、不截断。
                    (
                        200,
                        sse_stream_full(3).into_bytes(),
                        "text/event-stream",
                        vec![],
                    )
                } else {
                    (200, ok_json_body().into_bytes(), "application/json", vec![])
                }
            }
            Scenario::RateLimitedQuotaBody => (
                429,
                error_body("quota exceeded for this key").into_bytes(),
                "application/json",
                vec![("retry-after".to_string(), "30".to_string())],
            ),
            Scenario::RateLimited => (
                429,
                error_body("rate limited").into_bytes(),
                "application/json",
                vec![("retry-after".to_string(), "30".to_string())],
            ),
            Scenario::RiskControl3012 => (
                405,
                br#"{"code":3012,"msg":"request has been blocked due to unusual activity."}"#.to_vec(),
                "application/json",
                vec![],
            ),
            Scenario::Captcha3007 => (
                400,
                br#"{"code":3007,"msg":"captcha verify failed"}"#.to_vec(),
                "application/json",
                vec![],
            ),
            Scenario::CaptchaChallenge => (
                403,
                error_body("captcha verify failed").into_bytes(),
                "application/json",
                vec![("x-aliyun-captcha-verify-param".to_string(), "need".to_string())],
            ),
            Scenario::SseTruncate => (
                200,
                // ⚠ 直接把 `truncate_at` 透传给 `sse_stream`：`None` 表示
                // **截一半**（`server.py:191-192` 的默认形态），`Some(n)` 表示
                // 截到第 n 个字符。第一版这里写成 `Some(truncate_at.unwrap_or(0))`
                // —— 于是缺省变成 `Some(0)` ⇒ **空流**，"截一半"的语义整个失效
                //（被 `sse_truncate_scenario_really_truncates` 抓到）。
                sse_stream(3, truncate_at).into_bytes(),
                "text/event-stream",
                vec![],
            ),
            Scenario::EmptyStream => (
                200,
                // 空流：HTTP 200 + 一帧有效数据都没有 —— 2026-10-11 现场
                // `deepseek-v4.1-flash-sg` 国际区整体故障的复现形态
                //（触发网关「上游返回空流（无有效数据帧）」分支）。
                Vec::new(),
                "text/event-stream",
                vec![],
            ),
            Scenario::AuthInvalid => (
                401,
                r#"{"error":{"message":"invalid api key","type":"authentication_error"}}"#
                    .as_bytes()
                    .to_vec(),
                "application/json",
                vec![],
            ),
            Scenario::ServerError => (
                500,
                br#"{"error":"internal"}"#.to_vec(),
                "application/json",
                vec![],
            ),
            Scenario::QuotaExhausted => (
                402,
                error_body("insufficient balance").into_bytes(),
                "application/json",
                vec![],
            ),
            Scenario::NotFound => (
                404,
                r#"{"error":{"message":"no such route","type":"invalid_request_error"}}"#
                    .as_bytes()
                    .to_vec(),
                "application/json",
                vec![],
            ),
            Scenario::GarbageBody => (200, b"<html>not json</html>".to_vec(), "text/html", vec![]),
            Scenario::Other(_) => (200, ok_json_body().into_bytes(), "application/json", vec![]),
        }
    }
}

/// 正常响应体（`server.py:207-215` 的 `ok_body`）。
fn ok_json_body() -> String {
    r#"{"id":"msg_mock_001","type":"message","role":"assistant","model":"glm-5.3","content":[{"type":"text","text":"Hello from mock upstream"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#
        .to_string()
}

/// `server.py:106-107` 的 `_error_body`。
fn error_body(message: &str) -> String {
    format!(r#"{{"error":{{"message":"{message}","type":"upstream_error"}}}}"#)
}

/// 生成 Anthropic 形状的 SSE 流（`server.py:243-250` 的 `_sse_stream`）。
///
/// `truncate` 语义逐字对齐 `server.py:188-192`：
///
/// ```text
/// Some(n)  → 截到第 n 个字符（x-mock-sse-truncate-at 显式指定）
/// None     → 截到**一半**（该场景的默认形态）
/// ```
pub fn sse_stream(chunks: usize, truncate: Option<usize>) -> String {
    let s = sse_stream_full(chunks);
    match truncate {
        Some(at) => s.chars().take(at).collect(),
        None => s.chars().take(s.chars().count() / 2).collect(),
    }
}

/// 完整的（未截断的）SSE 流。
pub fn sse_stream_full(chunks: usize) -> String {
    let mut parts = vec![
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"role\":\"assistant\"}}\n\n"
            .to_string(),
    ];
    for i in 0..chunks {
        parts.push(format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\
             \"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"chunk-{i}\"}}}}\n\n"
        ));
    }
    parts.push(
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n\
         event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
            .to_string(),
    );
    parts.concat()
}

/// 一个 mock 上游实例。
///
/// 析构即关闭（后台线程随监听套接字消失而退出 —— `listener.incoming()` 在
/// 监听套接字被 drop 后返回 `Err`，循环 `break`）。测试无需显式清理。
pub struct MockUpstream {
    base: String,
    calls: Arc<Mutex<Vec<RecordedCall>>>,
}

impl MockUpstream {
    /// 起一个 mock；绑定 `127.0.0.1:0`（随机端口，**不触网**）。
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let calls: Arc<Mutex<Vec<RecordedCall>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&calls);
        std::thread::spawn(move || {
            // bind → 计数（对齐 `server.py:139-142`）。
            let mut counters: std::collections::HashMap<String, u64> =
                std::collections::HashMap::new();
            for stream in listener.incoming() {
                let mut s = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                // 一个连接可能承载多个请求（keep-alive）—— 逐个处理，
                // 直到对端关闭。⚠ 不处理 keep-alive 会让复用连接的客户端
                // 在第二个请求上挂死（reqwest 默认复用）。
                loop {
                    let Some(req) = read_request(&mut s) else { break };
                    let bind = bind_key(&req.headers);
                    let n = *counters.get(&bind).unwrap_or(&0);
                    counters.insert(bind.clone(), n + 1);

                    let scenario_name = req
                        .headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("x-mock-scenario"))
                        .map(|(_, v)| v.clone());
                    let sequence = req
                        .headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("x-mock-sequence"))
                        .map(|(_, v)| v.clone());
                    // `x-mock-scenario` 优先；否则按 sequence 依序消费，耗尽保持最后一个
                    //（对齐 `server.py:125-133` 的 `_scenario_for`）。
                    let scenario = match scenario_name {
                        Some(s) if !s.trim().is_empty() => Scenario::parse(&s),
                        _ => match sequence {
                            Some(seq) => {
                                let list: Vec<&str> =
                                    seq.split(',').map(|x| x.trim()).filter(|x| !x.is_empty()).collect();
                                if list.is_empty() {
                                    Scenario::Ok
                                } else {
                                    Scenario::parse(list[(n as usize).min(list.len() - 1)])
                                }
                            }
                            None => Scenario::Ok,
                        },
                    };

                    let stream = String::from_utf8_lossy(&req.body).contains("\"stream\":true");
                    let truncate_at = req
                        .headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("x-mock-sse-truncate-at"))
                        .and_then(|(_, v)| v.trim().parse::<usize>().ok());

                    sink.lock().unwrap().push(RecordedCall {
                        method: req.method.clone(),
                        path: req.path.clone(),
                        headers: req.headers.clone(),
                        body: req.body.clone(),
                        call_index: n,
                    });

                    let (status, body, ctype, extra) = scenario.response(stream, truncate_at);
                    let mut head = format!(
                        "HTTP/1.1 {status} {}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\n\
                         x-mock-call-index: {n}\r\n",
                        if status == 200 { "OK" } else { "Error" },
                        body.len()
                    );
                    for (k, v) in extra {
                        head.push_str(&format!("{k}: {v}\r\n"));
                    }
                    head.push_str("connection: keep-alive\r\n\r\n");
                    if s.write_all(head.as_bytes()).is_err() {
                        break;
                    }
                    if s.write_all(&body).is_err() {
                        break;
                    }
                    let _ = s.flush();
                }
            }
        });
        MockUpstream { base, calls }
    }

    /// 上游基址（`http://127.0.0.1:<port>`）。
    pub fn base(&self) -> &str {
        &self.base
    }

    /// 取全部已记录的请求（克隆快照）。
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }

    /// 已记录的请求数。
    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// 取第 `i` 个请求。
    pub fn call(&self, i: usize) -> RecordedCall {
        self.calls
            .lock()
            .unwrap()
            .get(i)
            .cloned()
            .unwrap_or_else(|| panic!("第 {i} 个请求不存在（共 {} 个）", self.call_count()))
    }
}

/// 一个已解析的 HTTP 请求。
struct RawRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// 读一个完整的 HTTP/1.1 请求；对端关闭时返回 `None`。
fn read_request(s: &mut std::net::TcpStream) -> Option<RawRequest> {
    let mut raw: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    // 读 header 结束。
    while find_subslice(&raw, b"\r\n\r\n").is_none() {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
    }
    let header_end = find_subslice(&raw, b"\r\n\r\n").unwrap() + 4;
    let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let req_line = lines.next().unwrap_or("");
    let mut parts = req_line.split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut content_length = 0usize;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim().to_string();
            if k == "content-length" {
                content_length = v.parse().unwrap_or(0);
            }
            // 同名头保留第一个（测试里足够）。
            if !headers.iter().any(|(hk, _)| *hk == k) {
                headers.push((k, v));
            }
        }
    }
    while raw.len() < header_end + content_length {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
    }
    let body = raw[header_end.min(raw.len())..].to_vec();
    Some(RawRequest {
        method,
        path,
        headers,
        body,
    })
}

/// 按 bind 分账的键（对齐 `server.py:119-123` 的 `_bind_key`）。
///
/// 显式 `x-mock-bind` 优先；否则取 `authorization`（剥 `Bearer `）或 `x-api-key`
/// 的前 16 字符 —— 于是「按账号注入」只需在账号的上游头里带 `x-mock-bind`。
fn bind_key(headers: &[(String, String)]) -> String {
    if let Some((_, v)) = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-mock-bind"))
    {
        if !v.trim().is_empty() {
            return v.trim().to_string();
        }
    }
    let cred = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .map(|(_, v)| v.strip_prefix("Bearer ").unwrap_or(v).to_string())
        .or_else(|| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("x-api-key"))
                .map(|(_, v)| v.clone())
        })
        .unwrap_or_default();
    let t = cred.trim();
    if t.is_empty() {
        "anonymous".to_string()
    } else {
        t.chars().take(16).collect()
    }
}

/// 子串查找（`upstream::tasks` 测试模块的同名工具有意未复用：那是测试私有项）。
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}
