//! OpenAI Responses 协议适配：请求转换与响应转换。
//!
//! 对应 Go 源文件 `internal/server/responses.go`（流式见 [`super::responses_stream`]）。
//!
//! # 为什么必须有这一层
//!
//! Codex CLI 自 0.146 起彻底移除了 `wire_api = "chat"`，只接受
//! `wire_api = "responses"`（二进制内明确写着
//! "`wire_api = \"chat\"` is no longer supported"）。因此网关若只提供
//! `/v1/chat/completions`，Codex 一侧无论怎么写配置都连不上。
//!
//! # 覆盖范围（Codex 实际会发的形状）
//!
//! - `input`：字符串，或 item 数组（`message` / `function_call` / `function_call_output`）
//! - `instructions`：等价于 system 消息
//! - `tools`：function 工具（含 `strict` / `namespace` 形态）
//! - `tool_choice` / `parallel_tool_calls` / `reasoning.effort` / `text.verbosity`
//! - `stream`：Codex 恒为 true

use serde_json::{json, Map, Value};

use crate::protocol::rectify::rectify_chat_tool_sequence;
use crate::protocol::{chat_message_text, chat_tool_calls, num_of, nested_num, str_field};

/// Responses 请求体（只声明用到的字段）。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ResponsesRequest {
    /// 请求的模型名。
    #[serde(default)]
    pub model: String,
    /// 输入（字符串或 item 数组）。
    #[serde(default)]
    pub input: Option<Value>,
    /// 系统指令（等价于 system 消息）。
    #[serde(default)]
    pub instructions: String,
    /// 工具声明。
    #[serde(default)]
    pub tools: Vec<Value>,
    /// 工具选择策略。
    #[serde(default)]
    pub tool_choice: Option<Value>,
    /// 是否允许并行工具调用。
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    /// 是否流式（Codex 恒为 true）。
    #[serde(default)]
    pub stream: bool,
    /// 推理配置。
    #[serde(default)]
    pub reasoning: Option<ReasoningBlock>,
    /// 文本配置。
    #[serde(default)]
    pub text: Option<TextBlock>,
    /// 会话粘性：Codex 会带它，可直接当会话键。
    #[serde(default)]
    pub prompt_cache_key: String,
    /// 元数据。
    #[serde(default)]
    pub metadata: Option<ResponsesMetadata>,
    /// 最大输出 token。
    #[serde(default)]
    pub max_output_tokens: Option<i64>,
    /// 采样温度。
    #[serde(default)]
    pub temperature: Option<f64>,
    /// 核采样。
    #[serde(default)]
    pub top_p: Option<f64>,
}

/// 推理配置块。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ReasoningBlock {
    /// 推理档位。
    #[serde(default)]
    pub effort: String,
}

/// 文本配置块。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct TextBlock {
    /// 冗长度。
    #[serde(default)]
    pub verbosity: String,
}

/// Responses 的元数据块。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ResponsesMetadata {
    /// 会话 id。
    #[serde(default)]
    pub conversation_id: String,
}

/// 解析 Responses 请求体。
pub fn parse_request(raw: &[u8]) -> Result<ResponsesRequest, String> {
    serde_json::from_slice(raw).map_err(|e| format!("invalid responses request: {e}"))
}

/// 把 Responses 请求翻译成 OpenAI Chat Completions 请求。
///
/// 返回 `(请求体, 会话粘性键)`。
pub fn responses_to_chat(raw: &[u8]) -> Result<(Vec<u8>, String), String> {
    let req = parse_request(raw)?;

    let mut messages: Vec<Value> = Vec::with_capacity(8);
    let instructions = req.instructions.trim();
    if !instructions.is_empty() {
        messages.push(json!({"role": "system", "content": instructions}));
    }
    messages.extend(responses_input_to_messages(req.input.as_ref())?);

    // 与 /v1/messages 同源的整流：Codex 历史经压缩/截断后也可能出现
    // function_call 与 function_call_output 不配对，上游命中 400 code=11148。
    let fixed = rectify_chat_tool_sequence(&mut messages);
    if fixed > 0 {
        eprintln!("rectify /v1/responses tool sequence: fixed={fixed}");
    }

    let mut out = Map::new();
    out.insert("model".into(), json!(req.model));
    out.insert("messages".into(), Value::Array(messages));
    out.insert("stream".into(), json!(req.stream));

    let tools = responses_tools_to_chat(&req.tools);
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
    }
    if let Some(tc) = req.tool_choice.as_ref() {
        if !tc.is_null() {
            out.insert("tool_choice".into(), normalize_responses_tool_choice(tc));
        }
    }
    if let Some(v) = req.parallel_tool_calls {
        out.insert("parallel_tool_calls".into(), json!(v));
    }
    if let Some(v) = req.max_output_tokens {
        out.insert("max_tokens".into(), json!(v));
    }
    if let Some(v) = req.temperature {
        out.insert("temperature".into(), json!(v));
    }
    if let Some(v) = req.top_p {
        out.insert("top_p".into(), json!(v));
    }
    if let Some(r) = req.reasoning.as_ref() {
        if !r.effort.is_empty() {
            out.insert("reasoning_effort".into(), json!(r.effort));
        }
    }

    let body = serde_json::to_vec(&Value::Object(out))
        .map_err(|e| format!("marshal chat request: {e}"))?;
    Ok((body, responses_session_key(&req)))
}

/// 依次尝试 `metadata.conversation_id`、`prompt_cache_key`。
fn responses_session_key(req: &ResponsesRequest) -> String {
    if let Some(m) = req.metadata.as_ref() {
        let id = m.conversation_id.trim();
        if !id.is_empty() {
            return id.to_string();
        }
    }
    req.prompt_cache_key.trim().to_string()
}

/// 把 Responses 的 `input` 翻译成 Chat 的 `messages`。
///
/// `input` 有两种形态：
/// - 纯字符串：直接当 user 消息
/// - item 数组：逐条按 `type` 分派
pub fn responses_input_to_messages(input: Option<&Value>) -> Result<Vec<Value>, String> {
    let Some(input) = input else {
        return Ok(Vec::new());
    };
    match input {
        Value::Null => Ok(Vec::new()),
        Value::String(text) => Ok(vec![json!({"role": "user", "content": text})]),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let Some(obj) = item.as_object() else { continue };
                match str_field(obj, "type").as_str() {
                    "message" => {
                        if let Some(msg) = responses_message_to_chat(obj) {
                            out.push(msg);
                        }
                    }
                    "function_call" => out.push(responses_function_call_to_chat(obj)),
                    "function_call_output" => out.push(responses_function_output_to_chat(obj)),
                    // 推理条目不回转给上游：上游不接受该形状，
                    // 且内容已体现在后续 assistant 消息里。
                    "reasoning" => {}
                    // 缺 type 时按 message 兜底（部分客户端省略）。
                    "" => {
                        if let Some(msg) = responses_message_to_chat(obj) {
                            out.push(msg);
                        }
                    }
                    _ => {}
                }
            }
            Ok(out)
        }
        _ => Err("invalid responses input items".into()),
    }
}

/// 转换一条 Responses message item。
fn responses_message_to_chat(item: &Map<String, Value>) -> Option<Value> {
    let mut role = str_field(item, "role");
    if role.is_empty() {
        role = "user".into();
    }
    let content = responses_content_to_chat(item.get("content"))?;
    Some(json!({"role": role, "content": content}))
}

/// 把 Responses 的 `content` 数组拍平成 Chat 的字符串或分片数组。
///
/// Responses 的内容分片形如 `{"type":"input_text","text":"..."}`；
/// 图片分片归一成 `image_url`（与 `/v1/messages` 的图片路径一致，
/// 供 `forward::request_has_image` 识别并做区域路由）。
///
/// 返回 `None` 表示无法转换（调用方跳过该条）。
fn responses_content_to_chat(v: Option<&Value>) -> Option<Value> {
    match v {
        None | Some(Value::Null) => Some(json!("")),
        Some(Value::String(s)) => Some(json!(s)),
        Some(Value::Object(m)) => responses_content_to_chat(Some(&Value::Array(vec![Value::Object(m.clone())]))),
        Some(Value::Array(pieces)) => {
            let mut text = String::new();
            let mut parts: Vec<Value> = Vec::with_capacity(pieces.len());
            let mut text_only = true;
            for piece in pieces {
                let Some(m) = piece.as_object() else { continue };
                match str_field(m, "type").as_str() {
                    "input_text" | "output_text" | "text" | "" => {
                        let t = str_field(m, "text");
                        text.push_str(&t);
                        parts.push(json!({"type": "text", "text": t}));
                    }
                    "input_image" => {
                        text_only = false;
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": {"url": str_field(m, "image_url")},
                        }));
                    }
                    _ => {}
                }
            }
            if text_only {
                Some(json!(text))
            } else {
                Some(Value::Array(parts))
            }
        }
        _ => Some(json!("")),
    }
}

/// `function_call` item → assistant 消息（带 tool_calls）。
fn responses_function_call_to_chat(item: &Map<String, Value>) -> Value {
    json!({
        "role": "assistant",
        "tool_calls": [{
            "id": str_field(item, "call_id"),
            "type": "function",
            "function": {
                "name": str_field(item, "name"),
                "arguments": str_field(item, "arguments"),
            }
        }]
    })
}

/// `function_call_output` item → `role:tool` 消息。
fn responses_function_output_to_chat(item: &Map<String, Value>) -> Value {
    let text = match responses_content_to_chat(item.get("output")) {
        Some(Value::String(s)) => Value::String(s),
        Some(other) => other,
        None => json!(""),
    };
    json!({
        "role": "tool",
        "tool_call_id": str_field(item, "call_id"),
        "content": text,
    })
}

/// 转换工具声明。
///
/// Responses 的工具形如：
/// - `{"type":"function","name":...,"description":...,"parameters":{...}}`
/// - `{"type":"function","function":{...}}`（部分兼容实现）
/// - `{"type":"namespace","name":...}`（Codex 私有扩展，降级为无参函数）
pub fn responses_tools_to_chat(tools: &[Value]) -> Vec<Value> {
    let mut out = Vec::with_capacity(tools.len());
    for raw in tools {
        let Some(m) = raw.as_object() else { continue };
        if let Some(nested) = m.get("function").and_then(|f| f.as_object()) {
            out.push(json!({"type": "function", "function": nested}));
            continue;
        }
        let name = str_field(m, "name");
        if name.is_empty() {
            continue;
        }
        let params = m
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        out.push(json!({
            "type": "function",
            "function": {
                "name": name,
                "description": str_field(m, "description"),
                "parameters": params,
            }
        }));
    }
    out
}

/// 把 `tool_choice` 归一成上游接受的 string 或标准对象。
pub fn normalize_responses_tool_choice(raw: &Value) -> Value {
    if let Value::String(s) = raw {
        return json!(s);
    }
    if let Some(m) = raw.as_object() {
        let t = str_field(m, "type");
        if t == "function" {
            let mut name = str_field(m, "name");
            if name.is_empty() {
                name = m
                    .get("function")
                    .and_then(|f| f.as_object())
                    .map(|f| str_field(f, "name"))
                    .unwrap_or_default();
            }
            if !name.is_empty() {
                return json!({"type": "function", "function": {"name": name}});
            }
        }
        if matches!(t.as_str(), "auto" | "none" | "required") {
            return json!(t);
        }
    }
    json!("auto")
}

/// 把非流式 `chat.completion` 转成 Responses 响应对象。
pub fn chat_to_responses(chat: &Map<String, Value>, model: &str) -> Value {
    let mut id = str_field(chat, "id");
    if id.is_empty() {
        id = "resp_wb2api".into();
    }
    let model = if model.is_empty() {
        str_field(chat, "model")
    } else {
        model.to_string()
    };

    let mut output: Vec<Value> = Vec::new();
    let text = chat_message_text(chat);
    if !text.is_empty() {
        output.push(json!({
            "id": format!("msg_{id}"),
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }));
    }
    for call in chat_tool_calls(chat) {
        let fn_obj = call.get("function").and_then(|f| f.as_object());
        let name = fn_obj.map(|f| str_field(f, "name")).unwrap_or_default();
        let args = fn_obj.map(|f| str_field(f, "arguments")).unwrap_or_default();
        let cid = str_field(&call, "id");
        output.push(json!({
            "type": "function_call",
            "id": cid,
            "call_id": cid,
            "name": name,
            "arguments": args,
            "status": "completed",
        }));
    }

    json!({
        "id": id,
        "object": "response",
        "created_at": chat.get("created").map(num_of).unwrap_or(0),
        "status": "completed",
        "model": model,
        "output": output,
        "usage": responses_usage(chat.get("usage")),
    })
}

/// 把 Chat 的 usage 映射成 Responses 的 usage 形状。
///
/// usage 未知时返回**全 0 而不是省略字段**：Codex 只在
/// `response.completed.response.usage` 里读用量，字段缺失会让客户端解析成 null 报错。
pub fn responses_usage(v: Option<&Value>) -> Value {
    let Some(u) = v.and_then(|v| v.as_object()) else {
        return json!({"input_tokens": 0, "output_tokens": 0, "total_tokens": 0});
    };
    let mut out = Map::new();
    out.insert(
        "input_tokens".into(),
        json!(u.get("prompt_tokens").map(num_of).unwrap_or(0)),
    );
    out.insert(
        "output_tokens".into(),
        json!(u.get("completion_tokens").map(num_of).unwrap_or(0)),
    );
    out.insert(
        "total_tokens".into(),
        json!(u.get("total_tokens").map(num_of).unwrap_or(0)),
    );
    let cached = nested_num(u, "prompt_tokens_details", "cached_tokens");
    if cached > 0 {
        out.insert(
            "input_tokens_details".into(),
            json!({"cached_tokens": cached}),
        );
    }
    let reasoning = nested_num(u, "completion_tokens_details", "reasoning_tokens");
    if reasoning > 0 {
        out.insert(
            "output_tokens_details".into(),
            json!({"reasoning_tokens": reasoning}),
        );
    }
    Value::Object(out)
}

/// 生成 Responses 形状的错误体。
pub fn responses_error(code: &str, msg: &str) -> Value {
    json!({
        "error": {
            "message": msg,
            "type": "api_error",
            "code": code,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn string_input_becomes_user_message() {
        let msgs = responses_input_to_messages(Some(&json!("hello"))).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], json!("user"));
        assert_eq!(msgs[0]["content"], json!("hello"));
    }

    #[test]
    fn missing_input_is_empty() {
        assert!(responses_input_to_messages(None).unwrap().is_empty());
        assert!(responses_input_to_messages(Some(&Value::Null)).unwrap().is_empty());
    }

    /// message item 的 content 分片归一成字符串（纯文本时）。
    #[test]
    fn message_item_content_flattens() {
        let input = json!([{
            "type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "a"}, {"type": "input_text", "text": "b"}]
        }]);
        let msgs = responses_input_to_messages(Some(&input)).unwrap();
        assert_eq!(msgs[0]["content"], json!("ab"));
    }

    /// 带图片的 content → 分片数组，图片归一成 image_url。
    #[test]
    fn image_content_becomes_image_url() {
        let input = json!([{
            "type": "message", "role": "user",
            "content": [
                {"type": "input_text", "text": "look"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAA"}
            ]
        }]);
        let msgs = responses_input_to_messages(Some(&input)).unwrap();
        let parts = msgs[0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], json!("text"));
        assert_eq!(parts[1]["type"], json!("image_url"));
        assert_eq!(parts[1]["image_url"]["url"], json!("data:image/png;base64,AAA"));
    }

    /// function_call / function_call_output 的配对转换。
    #[test]
    fn function_call_and_output_pairing() {
        let input = json!([
            {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{\"a\":1}"},
            {"type": "function_call_output", "call_id": "c1", "output": "result"}
        ]);
        let msgs = responses_input_to_messages(Some(&input)).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], json!("assistant"));
        assert_eq!(msgs[0]["tool_calls"][0]["id"], json!("c1"));
        assert_eq!(msgs[0]["tool_calls"][0]["function"]["arguments"], json!("{\"a\":1}"));
        assert_eq!(msgs[1]["role"], json!("tool"));
        assert_eq!(msgs[1]["tool_call_id"], json!("c1"));
        assert_eq!(msgs[1]["content"], json!("result"));
    }

    /// reasoning 条目必须丢弃（上游不接受该形状）。
    #[test]
    fn reasoning_items_are_dropped() {
        let input = json!([
            {"type": "reasoning", "summary": []},
            {"type": "message", "role": "user", "content": "x"}
        ]);
        let msgs = responses_input_to_messages(Some(&input)).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], json!("user"));
    }

    #[test]
    fn tools_conversion_forms() {
        // 平铺形态
        let t = responses_tools_to_chat(&[json!({
            "type": "function", "name": "f", "description": "d",
            "parameters": {"type": "object"}
        })]);
        assert_eq!(t[0]["function"]["name"], json!("f"));
        assert_eq!(t[0]["function"]["parameters"], json!({"type": "object"}));

        // 嵌套 function 形态
        let t2 = responses_tools_to_chat(&[json!({
            "type": "function",
            "function": {"name": "g", "parameters": {"type": "object"}}
        })]);
        assert_eq!(t2[0]["function"]["name"], json!("g"));

        // 缺 parameters → 补空 schema
        let t3 = responses_tools_to_chat(&[json!({"type": "function", "name": "h"})]);
        assert_eq!(
            t3[0]["function"]["parameters"],
            json!({"type": "object", "properties": {}})
        );

        // 缺 name → 跳过
        assert!(responses_tools_to_chat(&[json!({"type": "namespace"})]).is_empty());
    }

    #[test]
    fn tool_choice_normalization() {
        assert_eq!(normalize_responses_tool_choice(&json!("auto")), json!("auto"));
        assert_eq!(
            normalize_responses_tool_choice(&json!({"type": "function", "name": "f"})),
            json!({"type": "function", "function": {"name": "f"}})
        );
        assert_eq!(
            normalize_responses_tool_choice(&json!({"type": "required"})),
            json!("required")
        );
        // 未知 → auto
        assert_eq!(
            normalize_responses_tool_choice(&json!({"type": "weird"})),
            json!("auto")
        );
    }

    #[test]
    fn chat_to_responses_text() {
        let c = obj(json!({
            "id": "chatcmpl-1", "created": 1700000000, "model": "m",
            "choices": [{"message": {"content": "hello"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
        }));
        let out = chat_to_responses(&c, "");
        assert_eq!(out["object"], json!("response"));
        assert_eq!(out["status"], json!("completed"));
        assert_eq!(out["created_at"], json!(1700000000));
        assert_eq!(out["output"][0]["type"], json!("message"));
        assert_eq!(out["output"][0]["content"][0]["type"], json!("output_text"));
        assert_eq!(out["output"][0]["content"][0]["text"], json!("hello"));
        assert_eq!(out["usage"]["input_tokens"], json!(5));
    }

    #[test]
    fn chat_to_responses_tool_calls() {
        let c = obj(json!({
            "id": "r1",
            "choices": [{"message": {
                "content": null,
                "tool_calls": [{"id": "c1", "function": {"name": "f", "arguments": "{}"}}]
            }, "finish_reason": "tool_calls"}]
        }));
        let out = chat_to_responses(&c, "m");
        assert_eq!(out["output"][0]["type"], json!("function_call"));
        assert_eq!(out["output"][0]["call_id"], json!("c1"));
        assert_eq!(out["output"][0]["name"], json!("f"));
    }

    /// usage 缺失时给全 0（Codex 解析 null 会报错）。
    #[test]
    fn responses_usage_defaults_to_zero() {
        let u = responses_usage(None);
        assert_eq!(u["input_tokens"], json!(0));
        assert_eq!(u["output_tokens"], json!(0));
        assert_eq!(u["total_tokens"], json!(0));
    }

    #[test]
    fn responses_usage_details() {
        let u = responses_usage(Some(&json!({
            "prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120,
            "prompt_tokens_details": {"cached_tokens": 40},
            "completion_tokens_details": {"reasoning_tokens": 7}
        })));
        assert_eq!(u["input_tokens_details"]["cached_tokens"], json!(40));
        assert_eq!(u["output_tokens_details"]["reasoning_tokens"], json!(7));
    }

    /// 端到端请求转换。
    #[test]
    fn full_request_conversion() {
        let raw = json!({
            "model": "gpt-5.6",
            "instructions": "be terse",
            "input": [{"type": "message", "role": "user", "content": "hi"}],
            "stream": true,
            "max_output_tokens": 500,
            "temperature": 0.2,
            "reasoning": {"effort": "high"},
            "parallel_tool_calls": true,
            "tools": [{"type": "function", "name": "f", "parameters": {"type": "object"}}],
            "tool_choice": "auto",
            "prompt_cache_key": "cache-1"
        });
        let (body, key) = responses_to_chat(raw.to_string().as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["stream"], json!(true));
        assert_eq!(v["max_tokens"], json!(500));
        assert_eq!(v["temperature"], json!(0.2));
        assert_eq!(v["reasoning_effort"], json!("high"));
        assert_eq!(v["parallel_tool_calls"], json!(true));
        assert_eq!(v["messages"][0]["role"], json!("system"));
        assert_eq!(v["messages"][0]["content"], json!("be terse"));
        assert_eq!(v["messages"][1]["role"], json!("user"));
        assert_eq!(v["tools"][0]["function"]["name"], json!("f"));
        assert_eq!(key, "cache-1");
    }

    /// 会话键优先级：metadata.conversation_id > prompt_cache_key。
    #[test]
    fn session_key_precedence() {
        let a = json!({"metadata": {"conversation_id": "conv"}, "prompt_cache_key": "cache"});
        let (_, k) = responses_to_chat(a.to_string().as_bytes()).unwrap();
        assert_eq!(k, "conv");

        let b = json!({"prompt_cache_key": "cache"});
        let (_, k2) = responses_to_chat(b.to_string().as_bytes()).unwrap();
        assert_eq!(k2, "cache");

        let c = json!({});
        let (_, k3) = responses_to_chat(c.to_string().as_bytes()).unwrap();
        assert!(k3.is_empty());
    }

    #[test]
    fn invalid_request_rejected() {
        assert!(responses_to_chat(b"not json").is_err());
    }
}
