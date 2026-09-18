//! 协议适配层：把三种客户端协议翻译成上游的 OpenAI Chat 形态，再转回各自协议。
//!
//! 对应 Go 包 `internal/server/` 的协议文件：
//!
//! | 本模块           | Go 源文件                     | 协议                          |
//! |------------------|-------------------------------|-------------------------------|
//! | `rectify`        | `rectify.go`                  | tool 配对整流（三种入口共用） |
//! | `claude_models`  | `messages.go` 的映射区        | Claude 模型名 → 上游真实名    |
//! | `anthropic`      | `messages.go` + `_stream.go`  | Anthropic Messages            |
//! | `responses`      | `responses.go` + `_stream.go` | OpenAI Responses              |
//!
//! # 为什么必须有这一层
//!
//! 上游只接受 OpenAI Chat Completions，但两类主流客户端只讲别的协议：
//!
//! - **Claude Code / Claude Desktop** 只讲 Anthropic Messages（`POST /v1/messages`
//!   + `x-api-key` 头 + content block 数组）
//! - **Codex CLI** 自 0.146 起彻底移除 `wire_api = "chat"`，只接受
//!   `wire_api = "responses"`，因此网关若只提供 chat 入口，Codex 一侧无论怎么
//!   写配置都连不上
//!
//! 适配层只做「形状转换」，不碰任何调度状态：账号池、粘性会话、熔断冷却、
//! 模型冷却、统计日志全部复用 [`crate::forward`] 那一份实现。

pub mod anthropic;
pub mod anthropic_stream;
pub mod claude_models;
pub mod rectify;
pub mod responses;
pub mod responses_stream;

use serde_json::{Map, Value};

/// 取字符串字段（非字符串返回空串，与 Go 侧 `str()` 一致）。
pub fn str_of(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

/// 从对象里取字符串字段。
pub fn str_field(m: &Map<String, Value>, key: &str) -> String {
    m.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

/// 取整数字段（兼容 f64 / i64 / u64 三种 JSON 数字形态）。
///
/// 与 Go 侧 `numOf` 对应：Go 的 `json.Unmarshal` 把数字统一解成 float64，
/// 而 serde_json 保留整数类型，因此这里要把三种都认下来。
pub fn num_of(v: &Value) -> i64 {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i
            } else if let Some(u) = n.as_u64() {
                u as i64
            } else {
                n.as_f64().unwrap_or(0.0) as i64
            }
        }
        _ => 0,
    }
}

/// 从对象里取整数字段。
pub fn num_field(m: &Map<String, Value>, key: &str) -> i64 {
    m.get(key).map(num_of).unwrap_or(0)
}

/// 取嵌套数字字段（`m[outer][inner]`）。
pub fn nested_num(m: &Map<String, Value>, outer: &str, inner: &str) -> i64 {
    m.get(outer)
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(inner))
        .map(num_of)
        .unwrap_or(0)
}

/// 取首条 choice 的正文文本（`message.content`，回退 `delta.content`）。
pub fn chat_message_text(chat: &Map<String, Value>) -> String {
    let Some(choice) = first_choice(chat) else {
        return String::new();
    };
    match choice.get("message").and_then(|m| m.as_object()) {
        Some(msg) => str_field(msg, "content"),
        // message 缺失时回退 delta（流式形态）
        None => choice
            .get("delta")
            .and_then(|d| d.as_object())
            .map(|d| str_field(d, "content"))
            .unwrap_or_default(),
    }
}

/// 取首条 choice 的 `tool_calls` 列表。
pub fn chat_tool_calls(chat: &Map<String, Value>) -> Vec<Map<String, Value>> {
    let Some(choice) = first_choice(chat) else {
        return Vec::new();
    };
    let Some(msg) = choice.get("message").and_then(|m| m.as_object()) else {
        return Vec::new();
    };
    msg.get("tool_calls")
        .and_then(|t| t.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_object().cloned()).collect())
        .unwrap_or_default()
}

/// 取首条 choice 的 `finish_reason`。
pub fn chat_finish_reason(chat: &Map<String, Value>) -> String {
    first_choice(chat)
        .and_then(|c| c.get("finish_reason"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// 取首条 choice。
fn first_choice(chat: &Map<String, Value>) -> Option<&Map<String, Value>> {
    chat.get("choices")?
        .as_array()?
        .first()?
        .as_object()
}

/// 把 `arguments` 字符串解析回对象；解析失败或为空时返回 `{}`。
///
/// 与 Go 侧 `rawJSONOrEmptyObject` 一致：Anthropic 的 `tool_use.input` 必须是对象，
/// 而上游给的是 JSON 字符串，需要转回来。
pub fn raw_json_or_empty_object(s: &str) -> Value {
    if s.trim().is_empty() {
        return Value::Object(Map::new());
    }
    match serde_json::from_str::<Value>(s) {
        Ok(v) if v.is_object() => v,
        _ => Value::Object(Map::new()),
    }
}

/// 把任意 JSON 值序列化成紧凑字符串（`tool_use.input` → `arguments`）。
///
/// 与 Go 侧 `jsonString` 一致：`nil` → `"{}"`。
pub fn json_string(v: &Value) -> String {
    match v {
        Value::Null => "{}".to_string(),
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_string()),
    }
}

/// 生成 Anthropic 形状的唯一消息 id（`msg_` + 24 位十六进制）。
///
/// **为什么必须唯一**：Claude 客户端把 `message.id` 当作消息身份键，用于会话
/// transcript 重建、去重与上下文压缩。上游响应缺 id 时若退化为固定常量
/// （如 `msg_wb2api`），不同轮次的 assistant 响应会被客户端误判为同一条消息，
/// 历史重建后 `tool_use` / `tool_result` 错配，最终触发上游 400 code=11148。
pub fn new_message_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // 用纳秒时间戳 + 进程内计数器混合：无需引入 rand 依赖，且同进程内不会碰撞。
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // 12 字节 = 24 位十六进制，与 Go 侧 rand.Read(buf[:12]) 的形态一致。
    // 注意 `{:08x}` 是**最小**宽度而非截断：lo 是完整 u64，必须先掩到 32 位，
    // 否则会渲染出最多 16 个字符，id 长度随内容浮动。
    let hi = nanos;
    let lo = (n.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (nanos >> 17)) & 0xFFFF_FFFF;
    format!("msg_{hi:016x}{lo:08x}")
}

/// 生成带前缀的唯一 id（`resp_` / `chatcmpl-` 等）。
pub fn new_prefixed_id(prefix: &str) -> String {
    let full = new_message_id();
    // new_message_id 已带 "msg_" 前缀，这里剥掉再套目标前缀。
    let body = full.strip_prefix("msg_").unwrap_or(&full);
    format!("{prefix}{body}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn num_of_handles_all_json_number_forms() {
        assert_eq!(num_of(&json!(5)), 5);
        assert_eq!(num_of(&json!(5.9)), 5);
        assert_eq!(num_of(&json!(-3)), -3);
        assert_eq!(num_of(&json!("x")), 0);
        assert_eq!(num_of(&json!(null)), 0);
    }

    #[test]
    fn nested_num_reads_inner() {
        let m = obj(json!({"usage": {"prompt_tokens_details": {"cached_tokens": 7}}}));
        assert_eq!(nested_num(&m, "prompt_tokens_details", "cached_tokens"), 0);
        let u = m.get("usage").unwrap().as_object().unwrap();
        assert_eq!(nested_num(u, "prompt_tokens_details", "cached_tokens"), 7);
        // 缺失路径不 panic
        assert_eq!(nested_num(u, "nope", "nope"), 0);
    }

    #[test]
    fn chat_message_text_prefers_message_over_delta() {
        let c = obj(json!({"choices":[{"message":{"content":"full"}}]}));
        assert_eq!(chat_message_text(&c), "full");
        let d = obj(json!({"choices":[{"delta":{"content":"part"}}]}));
        assert_eq!(chat_message_text(&d), "part");
        assert_eq!(chat_message_text(&obj(json!({}))), "");
    }

    #[test]
    fn chat_tool_calls_and_finish_reason() {
        let c = obj(json!({
            "choices":[{
                "finish_reason":"tool_calls",
                "message":{"tool_calls":[{"id":"a"},{"id":"b"}]}
            }]
        }));
        assert_eq!(chat_tool_calls(&c).len(), 2);
        assert_eq!(chat_finish_reason(&c), "tool_calls");
    }

    #[test]
    fn raw_json_or_empty_object_never_fails() {
        assert_eq!(raw_json_or_empty_object(""), json!({}));
        assert_eq!(raw_json_or_empty_object("not json"), json!({}));
        assert_eq!(raw_json_or_empty_object("[1,2]"), json!({}), "非对象也回退空对象");
        assert_eq!(raw_json_or_empty_object(r#"{"a":1}"#), json!({"a":1}));
    }

    #[test]
    fn json_string_matches_go() {
        assert_eq!(json_string(&Value::Null), "{}");
        assert_eq!(json_string(&json!("raw")), "raw");
        assert_eq!(json_string(&json!({"a":1})), r#"{"a":1}"#);
    }

    /// 消息 id 必须唯一 —— 恒定 id 会让客户端误并不同轮次的响应。
    #[test]
    fn new_message_id_is_unique_and_well_formed() {
        let a = new_message_id();
        let b = new_message_id();
        assert_ne!(a, b, "连续两次必须不同");
        assert!(a.starts_with("msg_"), "{a}");
        assert_eq!(a.len(), 4 + 24, "msg_ + 24 位十六进制: {a}");
        assert!(a[4..].chars().all(|c| c.is_ascii_hexdigit()), "{a}");
    }

    #[test]
    fn prefixed_id_replaces_prefix() {
        let id = new_prefixed_id("resp_");
        assert!(id.starts_with("resp_"), "{id}");
        assert!(!id.contains("msg_"), "{id}");
    }
}
