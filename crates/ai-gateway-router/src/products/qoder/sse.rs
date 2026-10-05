//! Qoder 嵌套 SSE 解析。
//!
//! 上游的流**不是**标准 OpenAI SSE：每一帧形如
//! `data: {"body":"{\"choices\":[...]}"}` —— 真正的 OpenAI 分片被塞在
//! `body` 字段里（还要再解一层 JSON 字符串）。
//!
//! 本模块只负责「把上游的分片解析成 [`Chunk`]」，翻译成标准 OpenAI SSE
//! 由 [`super::stream`] 负责。
//!
//! # 为什么 `Chunk::usage` 要单独成型
//!
//! 原实现遇到"既不是 body 信封、也没有 choices"的分片就返回
//! `Chunk { raw: payload }`，问题是**它被丢掉了** ⇒ Qoder 路径对客户端
//! **从不报 token 用量**；而严格遵循 OpenAI 规范的客户端只从 `choices`
//! 为空的独立分片读 usage，上游恰好把 usage 塞在**带 choices 的 finish 分片**里，
//! 两条规则正好错开，故下游要拆成两帧。

use serde_json::{Map, Value};

/// 流内错误（上游用 HTTP 200 + 错误帧表达失败）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InStreamError {
    pub(crate) code: String,
    pub(crate) msg: String,
}

impl InStreamError {
    /// 面向用户的文案。
    pub(crate) fn message(&self) -> String {
        if let Some(h) = human_message(&self.msg) {
            return h;
        }
        if self.code.is_empty() {
            format!("上游在流内报错：{}", self.msg)
        } else {
            format!("上游在流内报错 code={} msg={}", self.code, self.msg)
        }
    }
}

impl std::fmt::Display for InStreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for InStreamError {}

/// 一个上游分片。
#[derive(Debug, Clone, Default)]
pub(crate) struct Chunk {
    pub(crate) content: String,
    pub(crate) reasoning_content: String,
    pub(crate) finish_reason: String,
    pub(crate) tool_calls: Vec<Value>,
    pub(crate) model: String,
    pub(crate) usage: Map<String, Value>,
    /// 原始 data 行（供排障：解析不出内容时能看到上游到底发了什么）。
    ///
    /// Go 侧同样是"只写不读"的排障字段（`sse.go:59` 注释）。保留是为了在
    /// 排查"用户看到空回答"时能直接打印上游原文，故显式放行。
    #[allow(dead_code)]
    pub(crate) raw: String,
}

/// 解析一个 payload 的结果。
///
/// ★ 三种结果必须区分开，对应 Go 侧 `SSEReader.Next` 的三个分支：
///
/// - [`ChunkParse::Chunk`] —— 正常分片（含"不认识的分片"，`raw` 保留原文）
/// - [`ChunkParse::Skip`] —— 解析不出来：**跳过继续读**，不能让整条流断掉
/// - [`ChunkParse::Fatal`] —— 流内错误：**必须上抛**（否则用户看到空回答）
#[derive(Debug)]
pub(crate) enum ChunkParse {
    Chunk(Chunk),
    Skip,
    Fatal(InStreamError),
}

/// 从一行 SSE 里剥出 payload。
///
/// 返回 `None` 表示"这行不是数据行"（空行、`:` 注释/心跳、非 `data:` 前缀）。
/// 返回 `Some(None)` 表示 `[DONE]`。
#[allow(clippy::option_option)]
pub(crate) fn payload_of_line(line: &str) -> Option<Option<String>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') {
        return None;
    }
    let rest = line.strip_prefix("data:")?;
    let payload = rest.trim();
    if payload.is_empty() {
        return None;
    }
    if payload == "[DONE]" {
        return Some(None);
    }
    Some(Some(payload.to_string()))
}

/// 解析一个 payload 成分片。
pub(crate) fn parse_chunk(payload: &str) -> ChunkParse {
    let top: Map<String, Value> = match serde_json::from_str(payload) {
        Ok(m) => m,
        // 不是合法 JSON 对象 ⇒ 跳过（Go 侧返回 &Chunk{Raw: payload}）
        Err(_) => return ChunkParse::Skip,
    };
    parse_chunk_from_map(&top, payload)
}

/// 从已解析的顶层 map 提取分片（便于单测与复用）。
pub(crate) fn parse_chunk_from_map(top: &Map<String, Value>, payload: &str) -> ChunkParse {
    // ① 先看错误信封（上游用 200 + 错误帧表达失败）
    if let Some(e) = error_from_envelope(top) {
        return ChunkParse::Fatal(e);
    }

    // 形状一：`{"body":"{...}"}` 嵌套信封
    if let Some(body) = top.get("body") {
        if let Some(inner) = body.as_str() {
            let inner = inner.trim();
            if !inner.is_empty() {
                return parse_openai_shaped(inner.as_bytes(), payload);
            }
        }
    }

    // 形状二：顶层直接就是 OpenAI 分片
    if top.contains_key("choices") {
        return parse_openai_shaped(payload.as_bytes(), payload);
    }

    // 形状三：独立的 usage 分片
    if let Some(u) = top.get("usage") {
        if let Some(obj) = u.as_object() {
            if !obj.is_empty() {
                let mut ch = Chunk {
                    usage: obj.clone(),
                    raw: payload.to_string(),
                    ..Default::default()
                };
                if let Some(m) = top.get("model").and_then(|v| v.as_str()) {
                    ch.model = m.to_string();
                }
                return ChunkParse::Chunk(ch);
            }
        }
    }

    // 不认识的分片：跳过继续读，但保留原文
    ChunkParse::Chunk(Chunk {
        raw: payload.to_string(),
        ..Default::default()
    })
}

/// 解析标准 OpenAI 形态的分片。
pub(crate) fn parse_openai_shaped(data: &[u8], raw: &str) -> ChunkParse {
    let v: Value = match serde_json::from_slice(data) {
        Ok(v) => v,
        Err(_) => return ChunkParse::Skip,
    };

    if let Some(err) = v.get("error") {
        if !err.is_null() {
            return ChunkParse::Fatal(InStreamError {
                code: err.get("code").and_then(value_to_string).unwrap_or_default(),
                msg: err
                    .get("msg")
                    .and_then(value_to_string)
                    .or_else(|| err.get("message").and_then(value_to_string))
                    .unwrap_or_default(),
            });
        }
    }

    let mut ch = Chunk {
        model: v.get("model").and_then(|m| m.as_str()).unwrap_or("").to_string(),
        raw: raw.to_string(),
        ..Default::default()
    };
    if let Some(u) = v.get("usage").and_then(|u| u.as_object()) {
        ch.usage = u.clone();
    }

    if let Some(c) = v.get("choices").and_then(|c| c.as_array()).and_then(|c| c.first()) {
        let delta = c.get("delta");
        let message = c.get("message");
        ch.content = first_non_empty([
            delta.and_then(|d| d.get("content")).and_then(value_to_string).unwrap_or_default(),
            message.and_then(|m| m.get("content")).and_then(value_to_string).unwrap_or_default(),
        ]);
        ch.reasoning_content = first_non_empty([
            delta
                .and_then(|d| d.get("reasoning_content"))
                .and_then(value_to_string)
                .unwrap_or_default(),
            message
                .and_then(|m| m.get("reasoning_content"))
                .and_then(value_to_string)
                .unwrap_or_default(),
        ]);
        let delta_tc = delta
            .and_then(|d| d.get("tool_calls"))
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        ch.tool_calls = if delta_tc.is_empty() {
            message
                .and_then(|m| m.get("tool_calls"))
                .and_then(|t| t.as_array())
                .cloned()
                .unwrap_or_default()
        } else {
            delta_tc
        };
        ch.finish_reason = c
            .get("finish_reason")
            .and_then(|f| f.as_str())
            .unwrap_or("")
            .to_string();
    }
    ChunkParse::Chunk(ch)
}

/// 从顶层信封里提取流内错误。
///
/// 形状①：`{"error": {...}}` 或 `{"error": "..."}`
/// 形状②：**三重否定判据**（缺一不可）—— 既无 `body`、也无 `choices`、
/// 也无 `usage` 时才看 `code`。
///
/// # 实测触发场景
///
/// POST `gateway.qoder.com.cn/.../agent_chat_generation` → HTTP **200**，
/// SSE 第 1 帧
/// `{"code":"403","message":"{\"code\":\"10605\",\"message\":\"{\\\"isQueued\\\":true,\\\"queueCount\\\":7309,...}\"}"}`
/// —— 该帧四个分支全不匹配 ⇒ 被静默跳过 ⇒ **用户看到空回答**。
pub(crate) fn error_from_envelope(top: &Map<String, Value>) -> Option<InStreamError> {
    // 形状①
    if let Some(raw) = top.get("error") {
        if !raw.is_null() {
            if let Some(obj) = raw.as_object() {
                return Some(InStreamError {
                    code: obj.get("code").and_then(value_to_string).unwrap_or_default(),
                    msg: first_non_empty([
                        obj.get("msg").and_then(value_to_string).unwrap_or_default(),
                        obj.get("message").and_then(value_to_string).unwrap_or_default(),
                    ]),
                });
            }
            return Some(InStreamError {
                code: String::new(),
                msg: raw_to_string(raw),
            });
        }
    }

    // 形状②：三重否定判据
    if top.contains_key("body") || top.contains_key("choices") || top.contains_key("usage") {
        return None;
    }

    let raw_code = top.get("code")?;
    if raw_code.is_null() {
        return None;
    }
    // Go: code := strings.Trim(strings.TrimSpace(string(rawCode)), "\"")
    let code = match raw_code {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if code.is_empty() {
        return None;
    }
    let msg = first_non_empty([
        top.get("message").and_then(value_to_string).unwrap_or_default(),
        top.get("msg").and_then(value_to_string).unwrap_or_default(),
    ]);
    // 防上游给正常帧加 `code:"0"`
    if code == "0" || code == "200" {
        return None;
    }
    if msg.is_empty() {
        return Some(InStreamError {
            code,
            msg: raw_to_string(raw_code),
        });
    }
    Some(InStreamError { code, msg })
}

/// 剥离嵌套的 `message` 字符串，找最内层的 `isQueued` 对象。
///
/// 最多剥 **4** 层。
fn peel_nested(msg: &str) -> Option<Map<String, Value>> {
    let mut cur = msg.to_string();
    for _ in 0..4 {
        let m: Map<String, Value> = serde_json::from_str(&cur).ok()?;
        if m.contains_key("isQueued") {
            return Some(m);
        }
        match m.get("message").and_then(|v| v.as_str()) {
            Some(next) if !next.trim().is_empty() => cur = next.to_string(),
            _ => return Some(m),
        }
    }
    None
}

/// 把排队类错误翻译成人话。
///
/// 原文示例：
/// `code=403 msg={"code":"10605","message":"{\"isQueued\":true,\"queueCount\":7309,…}"}`
fn human_message(msg: &str) -> Option<String> {
    let inner = peel_nested(msg)?;
    if inner.get("isQueued").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    let mut s = String::from("免费模型正在排队（上游主动限流，非故障）");
    if let Some(n) = num_field(&inner, "queueCount") {
        s.push_str(&format!("：前面约 {n} 个请求"));
    }
    if let Some(n) = num_field(&inner, "waitTime") {
        s.push_str(&format!("，预计等待约 {n} 秒"));
    }
    if let Some(m) = inner.get("modelKey").and_then(|v| v.as_str()) {
        if !m.is_empty() {
            s.push_str(&format!("（模型 {m}）"));
        }
    }
    if let Some(n) = num_field(&inner, "retryAfterSeconds") {
        s.push_str(&format!("。建议 {n} 秒后重试"));
    }
    if inner.get("serviceAvailable").and_then(|v| v.as_bool()) == Some(true) {
        s.push_str("；上游服务状态正常");
    }
    Some(s)
}

fn num_field(m: &Map<String, Value>, key: &str) -> Option<i64> {
    m.get(key)
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
}

/// 字段可能是字符串，也可能是数字/对象 —— 统一成字符串。
fn value_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// 非字符串时取原文（TrimSpace 后的紧凑 JSON）。
fn raw_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.trim().to_string(),
        other => other.to_string(),
    }
}

fn first_non_empty<const N: usize>(vals: [String; N]) -> String {
    for v in vals {
        if !v.trim().is_empty() {
            return v;
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(payload: &str) -> Chunk {
        match parse_chunk(payload) {
            ChunkParse::Chunk(c) => c,
            other => panic!("期望正常分片，实际 {other:?}（payload={payload}）"),
        }
    }

    fn fatal(payload: &str) -> InStreamError {
        match parse_chunk(payload) {
            ChunkParse::Fatal(e) => e,
            other => panic!("期望流内错误，实际 {other:?}（payload={payload}）"),
        }
    }

    #[test]
    fn payload_of_line_skips_heartbeats_and_blank_lines() {
        assert_eq!(payload_of_line(""), None);
        assert_eq!(payload_of_line("   "), None);
        assert_eq!(payload_of_line(": keep-alive"), None);
        assert_eq!(payload_of_line("event: message"), None);
        assert_eq!(payload_of_line("data:"), None);
        assert_eq!(payload_of_line("data:   "), None);
        assert_eq!(payload_of_line("data: [DONE]"), Some(None));
        assert_eq!(payload_of_line("data: {\"a\":1}"), Some(Some("{\"a\":1}".to_string())));
        assert_eq!(payload_of_line("data:{\"a\":1}"), Some(Some("{\"a\":1}".to_string())));
    }

    #[test]
    fn shape_one_nested_body_envelope() {
        let inner = r#"{"choices":[{"delta":{"content":"你好"},"finish_reason":""}],"model":"qwen3"}"#;
        let outer = serde_json::json!({ "body": inner }).to_string();
        let ch = chunk(&outer);
        assert_eq!(ch.content, "你好");
        assert_eq!(ch.model, "qwen3");
        assert_eq!(ch.raw, outer, "raw 保留的是**外层**原文");
    }

    #[test]
    fn shape_two_flat_openai_chunk() {
        let payload = r#"{"choices":[{"delta":{"reasoning_content":"想"},"finish_reason":"stop"}],"model":"m"}"#;
        let ch = chunk(payload);
        assert_eq!(ch.reasoning_content, "想");
        assert_eq!(ch.finish_reason, "stop");
    }

    #[test]
    fn shape_three_usage_only_chunk() {
        let payload = r#"{"usage":{"prompt_tokens":10,"completion_tokens":2},"model":"m"}"#;
        let ch = chunk(payload);
        assert_eq!(ch.usage["prompt_tokens"], 10);
        assert_eq!(ch.content, "", "usage 分片没有正文");
    }

    #[test]
    fn usage_on_a_finish_chunk_is_kept_alongside_content() {
        // 上游把 usage 塞在**带 choices 的 finish 分片**里 —— 这正是要拆帧的原因
        let payload = r#"{"choices":[{"delta":{"content":"尾"},"finish_reason":"stop"}],"usage":{"total_tokens":12}}"#;
        let ch = chunk(payload);
        assert_eq!(ch.content, "尾");
        assert_eq!(ch.finish_reason, "stop");
        assert_eq!(ch.usage["total_tokens"], 12);
    }

    #[test]
    fn unknown_chunk_is_kept_with_raw() {
        let payload = r#"{"hello":"world"}"#;
        let ch = chunk(payload);
        assert_eq!(ch.content, "");
        assert_eq!(ch.raw, payload);
        assert!(ch.usage.is_empty());
    }

    #[test]
    fn malformed_payload_is_skipped_not_fatal() {
        // ⚠ 关键语义：解不出来只跳过，绝不能让整条流断掉
        assert!(matches!(parse_chunk("not json"), ChunkParse::Skip));
        assert!(matches!(parse_chunk("123"), ChunkParse::Skip));
        assert!(matches!(parse_chunk("[1,2]"), ChunkParse::Skip));
        // body 是合法 JSON 但内层不是对象
        assert!(matches!(
            parse_chunk(r#"{"body":"also not json"}"#),
            ChunkParse::Skip
        ));
    }

    #[test]
    fn top_level_error_shape_is_raised_not_skipped() {
        // 实测第 1 帧：HTTP 200 + {"code":"403","message":"{...isQueued...}"}
        let payload = r#"{"code":"403","message":"{\"code\":\"10605\",\"message\":\"{\\\"isQueued\\\":true,\\\"queueCount\\\":7309}\"}"}"#;
        let err = fatal(payload);
        assert_eq!(err.code, "403");
        let msg = err.message();
        assert!(msg.contains("排队"), "应当翻译成排队文案: {msg}");
        assert!(msg.contains("7309"), "应当带出队列长度: {msg}");
    }

    #[test]
    fn code_zero_and_200_are_not_errors() {
        // 防上游给正常帧加 code:"0"
        for c in ["0", "200"] {
            let payload = format!(r#"{{"code":"{c}","message":"ok"}}"#);
            assert!(
                matches!(parse_chunk(&payload), ChunkParse::Chunk(_)),
                "code={c} 不该当错误"
            );
        }
    }

    #[test]
    fn code_is_ignored_when_frame_has_body_or_choices_or_usage() {
        // 三重否定判据：只要有任何一条"这是正常帧"的证据，就不看 code
        let ch = chunk(r#"{"code":"403","choices":[{"delta":{"content":"x"}}]}"#);
        assert_eq!(ch.content, "x", "有 choices 时 code 不参与判定");

        let ch = chunk(r#"{"code":"403","body":"{\"choices\":[{\"delta\":{\"content\":\"y\"}}]}"}"#);
        assert_eq!(ch.content, "y");

        assert!(matches!(
            parse_chunk(r#"{"code":"403","usage":{"total_tokens":1}}"#),
            ChunkParse::Chunk(_)
        ));
    }

    #[test]
    fn error_envelope_shape_one() {
        let err = fatal(r#"{"error":{"code":"10605","msg":"quota"}}"#);
        assert_eq!(err.code, "10605");
        assert_eq!(err.msg, "quota");

        let err = fatal(r#"{"error":"plain string"}"#);
        assert_eq!(err.code, "");
        assert_eq!(err.msg, "plain string");

        let err = fatal(r#"{"error":{"code":"x","message":"via message"}}"#);
        assert_eq!(err.msg, "via message");
    }

    #[test]
    fn queue_message_variants() {
        let e = InStreamError {
            code: "403".into(),
            msg: r#"{"isQueued":true}"#.into(),
        };
        assert_eq!(e.message(), "免费模型正在排队（上游主动限流，非故障）");

        let e = InStreamError {
            code: "403".into(),
            msg: r#"{"isQueued":true,"queueCount":12,"waitTime":30,"modelKey":"qwen3-max","retryAfterSeconds":45,"serviceAvailable":true}"#.into(),
        };
        let m = e.message();
        assert!(m.contains("前面约 12 个请求"), "{m}");
        assert!(m.contains("预计等待约 30 秒"), "{m}");
        assert!(m.contains("模型 qwen3-max"), "{m}");
        assert!(m.contains("建议 45 秒后重试"), "{m}");
        assert!(m.contains("上游服务状态正常"), "{m}");

        // isQueued=false ⇒ 不翻译，回落到通用文案
        let e = InStreamError {
            code: "500".into(),
            msg: r#"{"isQueued":false}"#.into(),
        };
        assert_eq!(e.message(), r#"上游在流内报错 code=500 msg={"isQueued":false}"#);
    }

    #[test]
    fn peel_nested_finds_is_queued_up_to_four_levels() {
        let inner = r#"{"isQueued":true,"queueCount":7}"#;
        let l3 = serde_json::json!({ "message": inner }).to_string();
        let l2 = serde_json::json!({ "message": l3 }).to_string();
        let l1 = serde_json::json!({ "message": l2 }).to_string();
        let e = InStreamError {
            code: "403".into(),
            msg: l1,
        };
        assert!(e.message().contains("前面约 7 个请求"), "{}", e.message());

        // 没有 isQueued 且没有 message ⇒ 返回该层，但不是排队
        let e = InStreamError {
            code: "x".into(),
            msg: r#"{"foo":1}"#.into(),
        };
        assert_eq!(e.message(), r#"上游在流内报错 code=x msg={"foo":1}"#);
    }

    #[test]
    fn message_and_usage_shapes_are_accepted() {
        let ch = chunk(r#"{"choices":[{"message":{"content":"完整","reasoning_content":"思考"},"finish_reason":"stop"}]}"#);
        assert_eq!(ch.content, "完整");
        assert_eq!(ch.reasoning_content, "思考");

        // delta 优先于 message
        let ch = chunk(r#"{"choices":[{"delta":{"content":"D"},"message":{"content":"M"}}]}"#);
        assert_eq!(ch.content, "D");
    }

    #[test]
    fn tool_calls_fall_back_from_delta_to_message() {
        let ch = chunk(r#"{"choices":[{"message":{"tool_calls":[{"index":0,"id":"a"}]}}]}"#);
        assert_eq!(ch.tool_calls.len(), 1);
        assert_eq!(ch.tool_calls[0]["id"], "a");
    }

    #[test]
    fn in_stream_error_message_falls_back_to_generic_form() {
        let e = InStreamError {
            code: String::new(),
            msg: "boom".into(),
        };
        assert_eq!(e.message(), "上游在流内报错：boom");
    }
}
