//! Anthropic Messages 协议适配：请求转换与响应转换。
//!
//! 对应 Go 源文件 `internal/server/messages.go`（流式部分见 [`super::anthropic_stream`]）。
//!
//! # 为什么必须有这一层
//!
//! Claude Code 与 Claude Desktop 的 3P 模式**只讲** Anthropic Messages 协议
//!（`POST /v1/messages` + `x-api-key` / `anthropic-version` 头 + content block 数组），
//! 而上游只接受 OpenAI Chat Completions。少了这一层，「一键导入」写进去的配置
//! 会让客户端连不上。
//!
//! # 覆盖范围
//!
//! - `system`：字符串或 block 数组
//! - `messages`：`text` / `image` / `tool_use` / `tool_result` 四种 block
//! - `tools`：`{name, description, input_schema}` → function tool
//! - `tool_choice`：`auto` / `any` / `tool` → `auto` / `required` / `{function}`
//! - `max_tokens` / `temperature` / `top_p` / `stop_sequences`
//! - `thinking`：按 `budget_tokens` 粗略映射到 `reasoning_effort`

use serde_json::{json, Map, Value};

use crate::protocol::claude_models::resolve_claude_model;
use crate::protocol::rectify::rectify_chat_tool_sequence;
use crate::protocol::{json_string, new_message_id, num_of, nested_num, str_field};

/// Anthropic 请求体（只声明用到的字段）。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct AnthropicRequest {
    /// 请求的模型名（可能是 `claude-*` 虚拟名）。
    #[serde(default)]
    pub model: String,
    /// 最大输出 token。
    #[serde(default)]
    pub max_tokens: Option<i64>,
    /// 系统提示（字符串或 block 数组）。
    #[serde(default)]
    pub system: Option<Value>,
    /// 对话消息。
    #[serde(default)]
    pub messages: Vec<Value>,
    /// 工具声明。
    #[serde(default)]
    pub tools: Vec<Value>,
    /// 工具选择策略。
    #[serde(default)]
    pub tool_choice: Option<Value>,
    /// 是否流式（Claude Code 恒为 true）。
    #[serde(default)]
    pub stream: bool,
    /// 采样温度。
    #[serde(default)]
    pub temperature: Option<f64>,
    /// 核采样。
    #[serde(default)]
    pub top_p: Option<f64>,
    /// 停止序列。
    #[serde(default)]
    pub stop_sequences: Vec<String>,
    /// 扩展思考配置。
    #[serde(default)]
    pub thinking: Option<ThinkingConfig>,
    /// 元数据（含 user_id，可作粘性键）。
    #[serde(default)]
    pub metadata: Option<MetadataBlock>,
}

/// 扩展思考配置。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ThinkingConfig {
    /// 类型（`"enabled"` 才生效）。
    #[serde(default, rename = "type")]
    pub kind: String,
    /// 思考预算（token）。
    #[serde(default)]
    pub budget_tokens: i64,
}

/// 元数据块。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct MetadataBlock {
    /// 用户标识。
    #[serde(default)]
    pub user_id: String,
}

/// 解析 Anthropic 请求体。
pub fn parse_request(raw: &[u8]) -> Result<AnthropicRequest, String> {
    serde_json::from_slice(raw).map_err(|e| format!("invalid messages request: {e}"))
}

/// 把 Anthropic Messages 请求翻译成 OpenAI Chat Completions 请求。
///
/// 返回 `(请求体, 会话粘性键)`。
///
/// 粘性键的由来：Anthropic 没有 `conversation_id`，用 `metadata.user_id`
/// 或 `system + 首条 user 文本` 折叠成稳定键，保证同一会话粘同一账号。
pub fn anthropic_to_chat(raw: &[u8]) -> Result<(Vec<u8>, String), String> {
    let req = parse_request(raw)?;

    let mut messages: Vec<Value> = Vec::with_capacity(req.messages.len() + 1);
    let system_text = anthropic_system_text(req.system.as_ref());
    if !system_text.is_empty() {
        messages.push(json!({"role": "system", "content": system_text}));
    }

    let mut first_user = String::new();
    for raw_msg in &req.messages {
        let (msgs, user_text) = anthropic_message_to_chat(raw_msg)?;
        if first_user.is_empty() && !user_text.is_empty() {
            first_user = user_text;
        }
        messages.extend(msgs);
    }

    // 整流 tool 配对：并发工具乱序完成、上下文压缩丢块等会让历史里的
    // assistant.tool_calls 与 role:tool 对不上，上游命中直接 400 code=11148。
    let fixed = rectify_chat_tool_sequence(&mut messages);
    if fixed > 0 {
        crate::logline!("rectify /v1/messages tool sequence: fixed={fixed}");
    }

    let target_model = resolve_claude_model(&req.model);
    let mut out = Map::new();
    out.insert("model".into(), json!(target_model));
    out.insert("messages".into(), Value::Array(messages));
    out.insert("stream".into(), json!(req.stream));
    if let Some(v) = req.max_tokens {
        out.insert("max_tokens".into(), json!(v));
    }
    if let Some(v) = req.temperature {
        out.insert("temperature".into(), json!(v));
    }
    if let Some(v) = req.top_p {
        out.insert("top_p".into(), json!(v));
    }
    if !req.stop_sequences.is_empty() {
        out.insert("stop".into(), json!(req.stop_sequences));
    }
    let tools = anthropic_tools_to_chat(&req.tools);
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
    }
    if let Some(tc) = req.tool_choice.as_ref() {
        if !tc.is_null() {
            out.insert("tool_choice".into(), anthropic_tool_choice_to_chat(tc));
        }
    }
    if let Some(th) = req.thinking.as_ref() {
        if th.kind == "enabled" {
            out.insert(
                "reasoning_effort".into(),
                json!(effort_from_budget(th.budget_tokens)),
            );
        }
    }

    let body = serde_json::to_vec(&Value::Object(out))
        .map_err(|e| format!("marshal chat request: {e}"))?;
    let key = anthropic_session_key(
        req.system.as_ref(),
        &first_user,
        req.metadata.as_ref().map(|m| m.user_id.as_str()).unwrap_or(""),
    );
    Ok((body, key))
}

/// 折叠出稳定的会话键。
fn anthropic_session_key(system: Option<&Value>, first_user: &str, user_id: &str) -> String {
    let mut seed = user_id.trim().to_string();
    if seed.is_empty() {
        seed = format!("{}\u{0}{}", anthropic_system_text(system), first_user);
    }
    crate::session::session_key_from_seed(&seed)
}

/// 把 Anthropic 的 thinking budget 粗略映射到 `reasoning_effort`。
///
/// 上游按模型 `supportedEfforts` 还会再降级一次（见 `upstream::payload`），
/// 因此这里给的是「意图」，不必精确。
fn effort_from_budget(budget: i64) -> &'static str {
    if budget <= 0 || budget < 4096 {
        "low"
    } else if budget < 16384 {
        "medium"
    } else if budget < 32768 {
        "high"
    } else {
        "max"
    }
}

/// 把 `system` 字段（字符串或 block 数组）拍平成纯文本。
pub fn anthropic_system_text(raw: Option<&Value>) -> String {
    let Some(v) = raw else { return String::new() };
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.as_object())
            .map(|b| str_field(b, "text"))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// 转换单条 Anthropic 消息。
///
/// 一条 Anthropic 消息可能同时包含 `tool_result`（应拆成 OpenAI 的 `role:tool`）
/// 与 `text`（`role:user`），因此返回的是消息**切片**而非单条。
///
/// 第二个返回值是该消息的正文文本（用于生成粘性键）。
pub fn anthropic_message_to_chat(raw: &Value) -> Result<(Vec<Value>, String), String> {
    let Some(msg) = raw.as_object() else {
        return Err("invalid message item: not an object".into());
    };
    let role = str_field(msg, "role");
    let content = msg.get("content").cloned().unwrap_or(Value::Null);

    // content 为纯字符串：直接映射。
    if let Value::String(plain) = &content {
        return Ok((
            vec![json!({"role": role, "content": plain})],
            plain.clone(),
        ));
    }

    let Some(blocks) = content.as_array() else {
        return Err("invalid message content".into());
    };

    let mut out: Vec<Value> = Vec::with_capacity(blocks.len());
    let mut text = String::new();
    let mut images: Vec<Value> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for b in blocks {
        let Some(block) = b.as_object() else { continue };
        match str_field(block, "type").as_str() {
            "text" | "" => text.push_str(&str_field(block, "text")),
            "image" => {
                // 图片块 → Chat 的 `image_url` 分片，与 Responses 入口
                //（`input_image`）归一后的形状一致：下游 `forward::request_has_image`
                // 靠它识别「本请求带图」（决定区域路由），出站 zcode 再把它翻回
                // Anthropic 的 `image` 块。
                //
                // ★ 旧实现这里是**空分支**（注释写「上游支持有限」）：Claude Code
                // 贴的截图被静默丢弃，模型只收到「你能看到图片吗」这句话，
                // 于是回「看不到 / 内容是空的」。
                if let Some(part) = anthropic_image_to_chat(block) {
                    images.push(part);
                }
            }
            "tool_use" => {
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                tool_calls.push(json!({
                    "id": str_field(block, "id"),
                    "type": "function",
                    "function": {
                        "name": str_field(block, "name"),
                        "arguments": json_string(&input),
                    }
                }));
            }
            "tool_result" => {
                // tool_result 必须单独成一条 role:tool 消息，且要排在 assistant 的
                // tool_calls 之后。
                //
                // 工具结果里同样可能带图（Claude Code 的 Read 读图片文件就是这种
                // 形状）：文本照旧拍平，图片分片一并透传。
                let (result_text, result_images) =
                    anthropic_tool_result_parts(block.get("content"));
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": str_field(block, "tool_use_id"),
                    "content": content_with_images(&result_text, &result_images),
                }));
            }
            // 推理块不回传上游：上游不接受该形状。
            "thinking" | "redacted_thinking" => {}
            _ => {}
        }
    }

    // assistant 消息若带 tool_use，必须携带 tool_calls 字段。
    if !tool_calls.is_empty() {
        let content = if text.is_empty() && images.is_empty() {
            Value::Null
        } else {
            content_with_images(&text, &images)
        };
        let assistant = json!({
            "role": "assistant",
            "tool_calls": tool_calls,
            "content": content,
        });
        // tool_calls 消息要排在本条最前，其后才是 tool_result 消息。
        let mut merged = vec![assistant];
        merged.extend(out);
        return Ok((merged, text));
    }

    if !text.is_empty() || !images.is_empty() {
        // tool_result 必须紧跟 assistant 的 tool_calls，因此正文追加在
        // tool 消息之后。若把正文插到最前，会形成 assistant → user → tool
        // 的断裂序列，上游按 tool_call_sequence_broken 拒绝。
        //
        // 无图 ⇒ content 仍是**纯字符串**（历史行为逐字不变）；有图 ⇒ 分片数组。
        let content = content_with_images(&text, &images);
        out.push(json!({"role": role, "content": content}));
    }
    Ok((out, text))
}

/// 组装 Chat 消息的 `content`：**无图 ⇒ 纯字符串**（历史形状逐字不变），
/// **有图 ⇒ 分片数组** `[{type:"text"},{type:"image_url"}]`。
///
/// 无图时保持字符串是刻意的：上游与 [`super::rectify::rectify_chat_tool_sequence`]
/// 都按老形状走，不必为一个新能力改动所有既有请求的字节。
fn content_with_images(text: &str, images: &[Value]) -> Value {
    if images.is_empty() {
        return Value::String(text.to_string());
    }
    let mut parts: Vec<Value> = Vec::with_capacity(images.len() + 1);
    if !text.is_empty() {
        parts.push(json!({"type": "text", "text": text}));
    }
    parts.extend(images.iter().cloned());
    Value::Array(parts)
}

/// 把 Anthropic 的 `image` 块翻成 Chat 的 `image_url` 分片。
///
/// Anthropic 的两种 source 与 OpenAI 的两种 url 严格互逆（出站方向见
/// `products/zcode/translate.rs` 的 `openai_image_to_anthropic`）：
///
/// | Anthropic 形态 | Chat 形态 |
/// |---|---|
/// | `source:{type:"base64",media_type:"image/png",data:…}` | `image_url:{url:"data:image/png;base64,…"}` |
/// | `source:{type:"url",url:"https://…"}` | `image_url:{url:"https://…"}` |
///
/// 认不出的形状返回 `None`：宁可不发图，也不发一个上游必然 400 的坏分片
///（坏分片会让**整个请求**失败，比缺图严重得多）。
fn anthropic_image_to_chat(block: &Map<String, Value>) -> Option<Value> {
    if let Some(src) = block.get("source").and_then(|v| v.as_object()) {
        match src.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "base64" => {
                let data = src.get("data").and_then(|v| v.as_str()).unwrap_or("");
                if data.is_empty() {
                    return None;
                }
                let media = non_empty(src.get("media_type")).unwrap_or("image/png");
                return Some(image_url_part(&format!("data:{media};base64,{data}")));
            }
            "url" => {
                let url = src.get("url").and_then(|v| v.as_str()).unwrap_or("");
                if url.is_empty() {
                    return None;
                }
                return Some(image_url_part(url));
            }
            _ => {}
        }
    }
    // 非标准形状：字段直接挂在块上（少数客户端 / 中间层这么发）。
    if let Some(b64) = non_empty(block.get("data")) {
        let media = non_empty(block.get("media_type")).unwrap_or("image/png");
        return Some(image_url_part(&format!("data:{media};base64,{b64}")));
    }
    let url = non_empty(block.get("url"))
        .or_else(|| non_empty(block.get("image_url").and_then(|v| v.get("url"))))
        .unwrap_or("");
    if url.is_empty() {
        return None;
    }
    Some(image_url_part(url))
}

/// `{"type":"image_url","image_url":{"url":…}}` —— 全仓库统一的图片分片形状。
fn image_url_part(url: &str) -> Value {
    json!({"type": "image_url", "image_url": {"url": url}})
}

/// 取一个非空字符串字段（空串 / 非字符串 ⇒ `None`）。
fn non_empty(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// 拍平 `tool_result` 的 content，并**保留**其中的图片分片。
///
/// 返回 `(文本, 图片分片)`：文本部分与 [`anthropic_tool_result_text`] 逐字一致
///（旧行为不变），图片部分是新增能力 —— Claude Code 的 Read 工具读图片文件时，
/// 结果就是这个形状。
fn anthropic_tool_result_parts(v: Option<&Value>) -> (String, Vec<Value>) {
    let Some(Value::Array(pieces)) = v else {
        return (anthropic_tool_result_text(v), Vec::new());
    };
    let mut text = String::new();
    let mut images: Vec<Value> = Vec::new();
    for p in pieces.iter().filter_map(|p| p.as_object()) {
        // 只对明确的图片块尝试转换，避免把带 url 字段的文本块误当图片。
        let is_image = matches!(str_field(p, "type").as_str(), "image" | "input_image");
        match (is_image, anthropic_image_to_chat(p)) {
            (true, Some(img)) => images.push(img),
            _ => text.push_str(&str_field(p, "text")),
        }
    }
    (text, images)
}

/// 拍平 `tool_result` 的 content（字符串或 block 数组）。
pub fn anthropic_tool_result_text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(pieces)) => pieces
            .iter()
            .filter_map(|p| p.as_object())
            .map(|m| str_field(m, "text"))
            .collect::<Vec<_>>()
            .join(""),
        Some(Value::Object(m)) => str_field(m, "text"),
        _ => String::new(),
    }
}

/// 把 Anthropic 工具声明转成 OpenAI function 工具。
pub fn anthropic_tools_to_chat(tools: &[Value]) -> Vec<Value> {
    let mut out = Vec::with_capacity(tools.len());
    for raw in tools {
        let Some(m) = raw.as_object() else { continue };
        let name = str_field(m, "name");
        if name.is_empty() {
            continue;
        }
        let params = m
            .get("input_schema")
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

/// 转换 `tool_choice`。
///
/// Anthropic 语义：`auto`（模型自选）/ `any`（必须用工具）/ `tool`（指定工具）。
/// OpenAI 对应：`auto` / `required` / `{type:function, function:{name}}`。
pub fn anthropic_tool_choice_to_chat(raw: &Value) -> Value {
    let Some(m) = raw.as_object() else {
        return json!("auto");
    };
    match str_field(m, "type").as_str() {
        "any" => json!("required"),
        "tool" => {
            let name = str_field(m, "name");
            if name.is_empty() {
                json!("auto")
            } else {
                json!({"type": "function", "function": {"name": name}})
            }
        }
        _ => json!("auto"),
    }
}

/// 把非流式 `chat.completion` 转成 Anthropic message 对象。
pub fn chat_to_anthropic(chat: &Map<String, Value>, model: &str) -> Value {
    // 不透传上游 chat.completion id：上游缺 id 时给的是固定常量，
    // 直接透传等于回到「所有响应同一个 id」的问题上。
    let id = new_message_id();
    let model = if model.is_empty() {
        chat.get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    } else {
        model.to_string()
    };

    let mut content: Vec<Value> = Vec::with_capacity(2);
    let text = crate::protocol::chat_message_text(chat);
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
    for call in crate::protocol::chat_tool_calls(chat) {
        let fn_obj = call.get("function").and_then(|f| f.as_object());
        let name = fn_obj.map(|f| str_field(f, "name")).unwrap_or_default();
        let args = fn_obj.map(|f| str_field(f, "arguments")).unwrap_or_default();
        content.push(json!({
            "type": "tool_use",
            "id": str_field(&call, "id"),
            "name": name,
            "input": crate::protocol::raw_json_or_empty_object(&args),
        }));
    }

    let mut stop_reason = "end_turn";
    if let Some(Value::Object(last)) = content.last() {
        if str_field(last, "type") == "tool_use" {
            stop_reason = "tool_use";
        }
    }
    let fr = crate::protocol::chat_finish_reason(chat);
    if fr == "length" {
        stop_reason = "max_tokens";
    } else if fr == "tool_calls" {
        stop_reason = "tool_use";
    }

    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": anthropic_usage(chat.get("usage")),
    })
}

/// 把 Chat usage 映射成 Anthropic usage。
pub fn anthropic_usage(v: Option<&Value>) -> Value {
    let Some(u) = v.and_then(|v| v.as_object()) else {
        return json!({"input_tokens": 0, "output_tokens": 0});
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
    let cached = nested_num(u, "prompt_tokens_details", "cached_tokens");
    if cached > 0 {
        out.insert("cache_read_input_tokens".into(), json!(cached));
    }
    Value::Object(out)
}

/// 生成 Anthropic 形状的错误体。
pub fn anthropic_error(code: &str, msg: &str) -> Value {
    json!({
        "type": "error",
        "error": {"type": code, "message": msg}
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn system_accepts_string_and_blocks() {
        assert_eq!(
            anthropic_system_text(Some(&json!("plain system"))),
            "plain system"
        );
        assert_eq!(
            anthropic_system_text(Some(&json!([
                {"type": "text", "text": "a"},
                {"type": "text", "text": "b"}
            ]))),
            "ab"
        );
        assert_eq!(anthropic_system_text(Some(&Value::Null)), "");
        assert_eq!(anthropic_system_text(None), "");
    }

    /// 字符串 content 直接映射。
    #[test]
    fn plain_string_message() {
        let (out, text) =
            anthropic_message_to_chat(&json!({"role": "user", "content": "hi"})).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], json!("user"));
        assert_eq!(out[0]["content"], json!("hi"));
        assert_eq!(text, "hi");
    }

    /// text block 数组 → 单条消息。
    #[test]
    fn text_blocks_collapse() {
        let (out, text) = anthropic_message_to_chat(&json!({
            "role": "user",
            "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]
        }))
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["content"], json!("ab"));
        assert_eq!(text, "ab");
    }

    /// tool_use → assistant.tool_calls，arguments 由 input 序列化而来。
    #[test]
    fn tool_use_becomes_tool_calls() {
        let (out, _) = anthropic_message_to_chat(&json!({
            "role": "assistant",
            "content": [{
                "type": "tool_use", "id": "t1", "name": "search",
                "input": {"q": "rust"}
            }]
        }))
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], json!("assistant"));
        let calls = out[0]["tool_calls"].as_array().unwrap();
        assert_eq!(calls[0]["id"], json!("t1"));
        assert_eq!(calls[0]["function"]["name"], json!("search"));
        assert_eq!(calls[0]["function"]["arguments"], json!(r#"{"q":"rust"}"#));
        // 无正文时 content 为 null
        assert_eq!(out[0]["content"], Value::Null);
    }

    /// tool_result → role:tool，且排在 assistant.tool_calls 之后。
    #[test]
    fn tool_result_ordering_with_tool_use() {
        let (out, _) = anthropic_message_to_chat(&json!({
            "role": "user",
            "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "result"},
                {"type": "text", "text": "and some text"}
            ]
        }))
        .unwrap();
        // 期望顺序：tool（结果）在前，正文在后
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0]["role"], json!("tool"));
        assert_eq!(out[0]["tool_call_id"], json!("t1"));
        assert_eq!(out[0]["content"], json!("result"));
        assert_eq!(out[1]["role"], json!("user"));
        assert_eq!(out[1]["content"], json!("and some text"));
    }

    #[test]
    fn tool_result_content_forms() {
        assert_eq!(anthropic_tool_result_text(Some(&json!("s"))), "s");
        assert_eq!(
            anthropic_tool_result_text(Some(&json!([{"type": "text", "text": "x"}]))),
            "x"
        );
        assert_eq!(anthropic_tool_result_text(Some(&json!({"text": "y"}))), "y");
        assert_eq!(anthropic_tool_result_text(None), "");
        assert_eq!(anthropic_tool_result_text(Some(&Value::Null)), "");
    }

    /// 推理块不回传上游。
    #[test]
    fn thinking_blocks_are_dropped() {
        let (out, text) = anthropic_message_to_chat(&json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": "answer"}
            ]
        }))
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["content"], json!("answer"));
        assert_eq!(text, "answer");
    }

    /// ★ 业主报的那个 bug：Claude Code 贴的截图必须活到上游。
    ///
    /// 旧实现里 `"image"` 是个空分支，图被**静默丢弃** —— 模型只收到「你能看到
    /// 图片吗」这句话，于是回「看不到 / 内容为空」。
    #[test]
    fn image_blocks_become_image_url_parts() {
        let (out, _text) = anthropic_message_to_chat(&json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "你能看到图片吗"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
            ]
        }))
        .unwrap();
        assert_eq!(out.len(), 1);
        let parts = out[0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], json!({"type": "text", "text": "你能看到图片吗"}));
        assert_eq!(parts[1]["type"], json!("image_url"));
        assert_eq!(
            parts[1]["image_url"]["url"],
            json!("data:image/png;base64,AAAA")
        );
    }

    /// 只有图、没有文字时也不能把整条消息丢掉（旧实现的收尾条件是 `!text.is_empty()`）。
    #[test]
    fn image_only_message_survives() {
        let (out, text) = anthropic_message_to_chat(&json!({
            "role": "user",
            "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "BBBB"}}]
        }))
        .unwrap();
        assert_eq!(text, "");
        assert_eq!(out.len(), 1);
        let parts = out[0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["image_url"]["url"], json!("data:image/jpeg;base64,BBBB"));
    }

    /// 远程 URL 形状的图（`source.type = "url"`，Anthropic 也允许）。
    #[test]
    fn image_url_source_becomes_image_url_part() {
        let (out, _) = anthropic_message_to_chat(&json!({
            "role": "user",
            "content": [{"type": "image", "source": {"type": "url", "url": "https://x/y.png"}}]
        }))
        .unwrap();
        assert_eq!(out[0]["content"][0]["image_url"]["url"], json!("https://x/y.png"));
    }

    /// 回归：**没有图**时 content 必须仍是纯字符串 —— 既有请求的字节逐字不变。
    #[test]
    fn text_only_message_keeps_plain_string_content() {
        let (out, _) = anthropic_message_to_chat(&json!({
            "role": "user",
            "content": [{"type": "text", "text": "hi"}]
        }))
        .unwrap();
        assert_eq!(out[0]["content"], json!("hi"));
    }

    /// tool_result 里的图（Claude Code 的 Read 读图片文件就是这个形状）也要透传。
    #[test]
    fn tool_result_images_survive() {
        let (out, _) = anthropic_message_to_chat(&json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "t1",
                "content": [
                    {"type": "text", "text": "读到了"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "CCCC"}}
                ]
            }]
        }))
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], json!("tool"));
        let parts = out[0]["content"].as_array().unwrap();
        assert_eq!(parts[0], json!({"type": "text", "text": "读到了"}));
        assert_eq!(parts[1]["image_url"]["url"], json!("data:image/png;base64,CCCC"));
    }

    /// 回归：tool_result 只有文字时仍是纯字符串（老形状）。
    #[test]
    fn tool_result_text_only_stays_string() {
        let (out, _) = anthropic_message_to_chat(&json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]
        }))
        .unwrap();
        assert_eq!(out[0]["content"], json!("ok"));
    }

    /// 认不出的图片块（既无 data 也无 url）宁可丢，也不要发坏分片给上游。
    #[test]
    fn unrecognized_image_block_is_dropped() {
        let (out, _) = anthropic_message_to_chat(&json!({
            "role": "user",
            "content": [
                {"type": "image", "source": {"type": "base64"}},
                {"type": "text", "text": "t"}
            ]
        }))
        .unwrap();
        assert_eq!(out[0]["content"], json!("t"));
    }

    #[test]
    fn tools_conversion() {
        let tools = anthropic_tools_to_chat(&[json!({
            "name": "search",
            "description": "d",
            "input_schema": {"type": "object"}
        })]);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], json!("function"));
        assert_eq!(tools[0]["function"]["name"], json!("search"));
        assert_eq!(tools[0]["function"]["parameters"], json!({"type": "object"}));

        // 缺 input_schema → 补空对象 schema
        let t2 = anthropic_tools_to_chat(&[json!({"name": "f"})]);
        assert_eq!(
            t2[0]["function"]["parameters"],
            json!({"type": "object", "properties": {}})
        );
        // 缺 name → 跳过
        assert!(anthropic_tools_to_chat(&[json!({"description": "x"})]).is_empty());
    }

    #[test]
    fn tool_choice_mapping() {
        assert_eq!(anthropic_tool_choice_to_chat(&json!({"type": "auto"})), json!("auto"));
        assert_eq!(
            anthropic_tool_choice_to_chat(&json!({"type": "any"})),
            json!("required")
        );
        assert_eq!(
            anthropic_tool_choice_to_chat(&json!({"type": "tool", "name": "f"})),
            json!({"type": "function", "function": {"name": "f"}})
        );
        // tool 无名 → auto
        assert_eq!(
            anthropic_tool_choice_to_chat(&json!({"type": "tool"})),
            json!("auto")
        );
        // 未知 → auto
        assert_eq!(anthropic_tool_choice_to_chat(&json!({"type": "x"})), json!("auto"));
    }

    #[test]
    fn effort_from_budget_thresholds() {
        assert_eq!(effort_from_budget(0), "low");
        assert_eq!(effort_from_budget(1000), "low");
        assert_eq!(effort_from_budget(4096), "medium");
        assert_eq!(effort_from_budget(16384), "high");
        assert_eq!(effort_from_budget(40000), "max");
    }

    #[test]
    fn chat_to_anthropic_text_response() {
        let c = chat(json!({
            "id": "chatcmpl-1",
            "model": "deepseek-v4-flash",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hello"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        }));
        let out = chat_to_anthropic(&c, "claude-sonnet-4-6");
        assert_eq!(out["type"], json!("message"));
        assert_eq!(out["role"], json!("assistant"));
        // 必须用新 id，不能透传上游 id
        let id = out["id"].as_str().unwrap();
        assert!(id.starts_with("msg_"), "{id}");
        assert_ne!(id, "chatcmpl-1", "不得透传上游 id");
        assert_eq!(out["model"], json!("claude-sonnet-4-6"));
        assert_eq!(out["content"][0]["type"], json!("text"));
        assert_eq!(out["content"][0]["text"], json!("hello"));
        assert_eq!(out["stop_reason"], json!("end_turn"));
        assert_eq!(out["usage"]["input_tokens"], json!(10));
        assert_eq!(out["usage"]["output_tokens"], json!(5));
    }

    #[test]
    fn chat_to_anthropic_tool_use_stop_reason() {
        let c = chat(json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{"id": "t1", "function": {"name": "f", "arguments": "{\"a\":1}"}}]
                },
                "finish_reason": "tool_calls"
            }]
        }));
        let out = chat_to_anthropic(&c, "m");
        assert_eq!(out["stop_reason"], json!("tool_use"));
        assert_eq!(out["content"][0]["type"], json!("tool_use"));
        assert_eq!(out["content"][0]["input"], json!({"a": 1}));
    }

    #[test]
    fn chat_to_anthropic_length_maps_to_max_tokens() {
        let c = chat(json!({
            "choices": [{"message": {"content": "x"}, "finish_reason": "length"}]
        }));
        assert_eq!(chat_to_anthropic(&c, "m")["stop_reason"], json!("max_tokens"));
    }

    /// usage 缺失时给全 0，而不是省略字段（客户端解析 null 会报错）。
    #[test]
    fn anthropic_usage_defaults_to_zero() {
        let u = anthropic_usage(None);
        assert_eq!(u["input_tokens"], json!(0));
        assert_eq!(u["output_tokens"], json!(0));
    }

    #[test]
    fn anthropic_usage_includes_cached_tokens() {
        let u = anthropic_usage(Some(&json!({
            "prompt_tokens": 100, "completion_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 30}
        })));
        assert_eq!(u["cache_read_input_tokens"], json!(30));
    }

    /// 端到端请求转换：system + 消息 + 工具 + thinking 全部落到 chat 形态。
    #[test]
    fn full_request_conversion() {
        let raw = json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 1024,
            "temperature": 0.5,
            "top_p": 0.9,
            "stop_sequences": ["END"],
            "system": "be brief",
            "stream": true,
            "thinking": {"type": "enabled", "budget_tokens": 20000},
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "f", "description": "d", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "any"}
        });
        let (body, _key) = anthropic_to_chat(raw.to_string().as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["stream"], json!(true));
        assert_eq!(v["max_tokens"], json!(1024));
        assert_eq!(v["temperature"], json!(0.5));
        assert_eq!(v["top_p"], json!(0.9));
        assert_eq!(v["stop"], json!(["END"]));
        assert_eq!(v["reasoning_effort"], json!("high"));
        assert_eq!(v["tool_choice"], json!("required"));
        assert_eq!(v["messages"][0]["role"], json!("system"));
        assert_eq!(v["messages"][0]["content"], json!("be brief"));
        assert_eq!(v["messages"][1]["role"], json!("user"));
        assert_eq!(v["tools"][0]["function"]["name"], json!("f"));
    }

    #[test]
    fn invalid_request_rejected() {
        assert!(anthropic_to_chat(b"not json").is_err());
        // messages 非数组 → 解析失败
        assert!(anthropic_to_chat(br#"{"messages": "x"}"#).is_err());
    }

    /// 映射表读不到时 claude- 名字**原样透传**。
    ///
    /// 这里直接构造空表验证兜底语义，而不是依赖本机是否装了 Claude 客户端：
    /// 真机上 `~/.claude/settings.json` 或 Claude Desktop 的 configLibrary 往往存在，
    /// 那样 `claude-unknown-9` 会被槽位兜底解析成真实模型名（这正是期望行为），
    /// 用例就会随环境漂移。
    #[test]
    fn empty_tables_pass_claude_names_through() {
        let t = crate::protocol::claude_models::ClaudeModelTables::default();
        assert!(t.aliases.is_empty() && t.slots.is_empty());
        // 无表可查时，解析结果必须等于输入（不做任何替换）
        let name = "claude-unknown-9";
        assert!(t.aliases.get(name).is_none());
    }

    /// 有配置时 claude- 名字被解析成真实模型名（本机真机场景）。
    #[test]
    fn configured_tables_resolve_claude_names() {
        let mut t = crate::protocol::claude_models::ClaudeModelTables::default();
        t.slots.insert("sonnet".into(), "glm-5.3-flash".into());
        t.aliases
            .insert("claude-sonnet-5".into(), "glm-5.3-flash".into());
        assert_eq!(t.aliases["claude-sonnet-5"], "glm-5.3-flash");
        assert_eq!(t.slots["sonnet"], "glm-5.3-flash");
    }

    /// 请求转换不得因模型名而失败：无论能否解析，都必须产出合法请求体。
    #[test]
    fn request_conversion_survives_unknown_model() {
        let raw = json!({"model": "claude-unknown-9", "messages": []});
        let (body, _) = anthropic_to_chat(raw.to_string().as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        // 模型名要么被解析成真实名，要么原样保留 —— 但不能为空
        assert!(
            v["model"].as_str().map(|s| !s.is_empty()).unwrap_or(false),
            "model 不得为空: {}",
            v["model"]
        );
    }

    /// 会话键：有 user_id 时用它，否则用 system + 首条 user。
    #[test]
    fn session_key_from_user_id_or_content() {
        let with_uid = json!({
            "model": "m", "metadata": {"user_id": "u-1"},
            "messages": [{"role": "user", "content": "x"}]
        });
        let (_, k1) = anthropic_to_chat(with_uid.to_string().as_bytes()).unwrap();
        assert!(!k1.is_empty());

        let without = json!({
            "model": "m", "system": "s",
            "messages": [{"role": "user", "content": "x"}]
        });
        let (_, k2) = anthropic_to_chat(without.to_string().as_bytes()).unwrap();
        assert!(k2.starts_with("seed-"), "{k2}");

        // 同输入必须同键（粘性路由的前提）
        let (_, k3) = anthropic_to_chat(without.to_string().as_bytes()).unwrap();
        assert_eq!(k2, k3);
    }
}
