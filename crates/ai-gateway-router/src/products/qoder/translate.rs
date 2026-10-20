//! OpenAI 请求体 → Qoder agent 请求体。
//!
//! # 方向对照
//!
//! | 侧 | 形态 |
//! |----|------|
//! | 入 | OpenAI `{model, messages, tools, stream}` |
//! | 出 | Qoder `{request_id, agent_id, chat_task, chat_context, messages, model_config}` |
//!
//! 请求体随后由 [`super::encoding::qoder_encode`] 编码；响应流是
//! `data: {"body":"{...choices...}"}` → 由 [`super::sse`] / [`super::stream`]
//! 翻成 `data: {"choices":[...]}`。
//!
//! ⚠ 翻译错了**不报错**，只会让用户看到空回答或上游 400。
//!
//! # 为什么是纯透传而不是构造模板
//!
//! 参考实现实测模板 system + 模板 tools 均非必需，纯透传后 baseline
//! `prompt_tokens` 从约 **10K** 降到约 **60**。客户端消息全量转发
//!（含 system/assistant/tool 多轮），tools 仅在客户端显式传入时注入。

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use super::cred::new_uuid;

/// 上游只认这一个值（**常量而非变量**）。
///
/// 逆向穷举（2026-09）：
///
/// | `model_config` | 上游是否返回 `reasoning_content` |
/// |----------------|----------------------------------|
/// | `{key, source:"system"}` | ✅ |
/// | `{key, is_reasoning:true}` | ❌ |
/// | 全字段但**缺** `source` | ❌ |
/// | `{key, is_reasoning, source:"custom"}` | ❌ |
///
/// `is_reasoning` 完全不影响。我们此前发的正是"缺 source"那格，
/// 而 [`super::stream`] 又在解析回传 `reasoning_content` ⇒ 下游永远白等。
const MODEL_SOURCE_SYSTEM: &str = "system";

/// `model_config.format`：上游只认 `openai`（与官方客户端一致）。
const MODEL_FORMAT_OPENAI: &str = "openai";

/// `model_config` 里 `max_input_tokens` 缺失时的兜底值。
///
/// 取自官方客户端抓包的 `180000`。★ 这只是**兜底**：真实值优先取模型清单里的
/// `max_input_tokens` / `context_config`（见 [`ModelMeta`]）。
const DEFAULT_MAX_INPUT_TOKENS: i64 = 180_000;

/// `model_config` 的元数据来源（模型清单里那一项的投影）。
///
/// # 为什么必须带上这些字段，而不是只发 `key`
///
/// 我们原先只发 `{key, is_reasoning, source}` 三个字段，而上游的
/// `model_config` 是一个**结构体**：缺字段时它不会报错，而是**回落成官方客户端的
/// 默认身份**（`display_name="Auto"`、`format` 缺失、`max_input_tokens` 缺失）。
///
/// 表现就是本次现场：模型名明明传对了（`model=DeepSeek-Flash`、`x-model-key` 头正确、
/// `/usage` 里也有 qoder 归属的 `DeepSeek-Flash` 记录），但**回答自称是 Qoder** ——
/// 上游按"官方 Qoder CLI 默认身份"接管了系统提示。
///
/// ⇒ 结论：**模型名不是问题，`model_config` 的完整度才是**。字段必须与模型清单
/// 里那一项对齐，让上游认出"这是在用某个具体模型"，而不是"一个没填身份的客户端"。
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ModelMeta {
    /// 上游 key（`model_config.key`）。
    pub(crate) key: String,
    /// 展示名（`model_config.display_name`）。空则回落 `key`。
    pub(crate) display_name: String,
    /// 是否推理模型（`model_config.is_reasoning`）。
    pub(crate) is_reasoning: bool,
    /// 是否视觉模型（`model_config.is_vl`）。
    pub(crate) is_vl: bool,
    /// 最大输入 token（`model_config.max_input_tokens`）。
    pub(crate) max_input_tokens: i64,
    /// 会话归属（账号 uid）：**只参与 `session_id` 派生，不写进 `model_config`**。
    ///
    /// # 为什么必须按账号隔离会话（2026-10-20 现场）
    ///
    /// `session_id` 是上游的会话键。若同一段对话在换号后继续用**同一个**
    /// `session_id`，新账号可能继承前一个账号的会话状态（或直接被视为
    /// 「不属于你的会话」）—— 现场表现为两个账号**连续空流**，而官方客户端
    /// （一个会话固定一个账号）完全正常。
    ///
    /// 空字符串 = 不隔离（单测与老调用点）；生产路径由 `Dispatch` 填入 `cr.uid`。
    pub(crate) session_owner: String,
}

impl ModelMeta {
    /// 只给 key 时的最小元数据（测试与"清单里查不到"时用）。
    pub(crate) fn key_only(key: &str) -> Self {
        Self {
            key: key.to_string(),
            ..Default::default()
        }
    }

    /// 实际写进 `display_name` 的值：空则回落 key。
    fn display_name_or_key(&self) -> &str {
        if self.display_name.is_empty() {
            &self.key
        } else {
            &self.display_name
        }
    }

    /// 实际写进 `max_input_tokens` 的值：非正数则回落兜底常量。
    fn max_input_tokens_or_default(&self) -> i64 {
        if self.max_input_tokens > 0 {
            self.max_input_tokens
        } else {
            DEFAULT_MAX_INPUT_TOKENS
        }
    }

    /// 构造 `model_config`（顶层与 `chat_context.extra.modelConfig` **共用同一份**）。
    fn to_config(&self) -> Value {
        json!({
            "key": self.key,
            "display_name": self.display_name_or_key(),
            // 上游对空 model 的容忍度未知，显式给空串（与官方客户端一致）
            "model": "",
            "format": MODEL_FORMAT_OPENAI,
            "is_vl": self.is_vl,
            "is_reasoning": self.is_reasoning,
            "api_key": "",
            "url": "",
            "source": MODEL_SOURCE_SYSTEM,
            "max_input_tokens": self.max_input_tokens_or_default(),
        })
    }
}

/// 构造 Qoder agent 请求体。
///
/// `meta` 是**上游 key + 模型清单里的元数据**（不是客户端看到的显示名）。
/// 只传 key 也能工作（[`ModelMeta::key_only`]），但 `model_config` 会缺字段，
/// 上游会回落成官方客户端默认身份 —— 见 [`ModelMeta`] 的说明。
///
/// # 形状对齐官方客户端（2026-10-20）
///
/// 从官方 `qoder-worker-runtime` 的请求体构造函数 `bve` 核对出的差异，
/// 全部按官方形状补齐（此前只有简单请求能过，DSH Hermes 的复杂请求会被
/// 上游静默回 `data: [DONE]`）：
///
/// - **system 单独放 `system` 字段**（数组），不再混在 `messages` 里；
/// - 顶层补 `source: 1` / `version: "3"` / `is_retry: false` / `task_id: "common"`；
/// - `aliyun_user_type` 官方是 **空串**（此前写的 `personal_professional_trial`
///   是旧参考实现的常量）；
/// - `chat_context.text` / `originalContent` 官方是**字符串**，不是 `{type,text}`；
/// - `chat_context.extra.modelConfig` 官方只有 `{key, is_reasoning}`（完整 10 字段
///   只放在顶层 `model_config`）；
/// - 消息带 `contents` 块（官方 `uPc` / `gPc` 的输出形状）；
/// - `parameters.max_tokens` 取客户端传的值（官方取模型的 `max_output_tokens`，
///   我们拿不到那个字段，就用客户端的，缺省不带）。
pub(crate) fn build_agent_body(openai_body: &[u8], meta: &ModelMeta) -> Result<Vec<u8>, String> {
    let req: Value =
        serde_json::from_slice(openai_body).map_err(|e| format!("解析 OpenAI 请求体失败: {e}"))?;

    let messages = req
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if messages.is_empty() {
        return Err("请求里没有 messages".to_string());
    }

    let tools = req
        .get("tools")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    // `max_tokens` / `max_completion_tokens` 都认（后者是新版 OpenAI 字段）。
    let max_tokens = req
        .get("max_tokens")
        .or_else(|| req.get("max_completion_tokens"))
        .and_then(Value::as_i64)
        .filter(|n| *n > 0);

    agent_body(&messages, meta, &tools, max_tokens)
}

/// 构造 agent 请求体（形状对齐官方客户端，见 [`build_agent_body`]）。
fn agent_body(
    messages: &[Value],
    meta: &ModelMeta,
    tools: &[Value],
    max_tokens: Option<i64>,
) -> Result<Vec<u8>, String> {
    let prompt = last_user_text(messages);
    let now_ms = now_unix_millis();
    let new_id = new_uuid();

    let model_config = meta.to_config();

    // developer 先归一成 system，再按官方形状把 system 拆出来。
    let normalized = normalize_developer_roles(messages);
    let (system_texts, conversation) = split_system(&normalized);
    let system_blocks: Vec<Value> = system_texts
        .iter()
        .map(|t| json!({ "text": t }))
        .collect();

    let sanitized_messages: Vec<Value> = conversation.iter().filter_map(sanitize_message).collect();
    let sanitized_tools: Vec<Value> = tools.iter().filter_map(sanitize_tool).collect();
    let dropped_tools = tools.len() - sanitized_tools.len();
    if dropped_tools > 0 {
        crate::logline!(
            "qoder dispatch: 丢弃 {} 个无法转换的工具（function 原样、custom 转 function，其余形状不支持）",
            dropped_tools
        );
    }
    let image_blocks = count_image_blocks(&sanitized_messages);
    if image_blocks > 0 {
        crate::logline!(
            "qoder dispatch: 消息含 {} 个图片块（保留在 contents，与官方客户端同形）",
            image_blocks
        );
    }

    // 键序与 Go 侧源码一致（serde_json 无 preserve_order ⇒ BTreeMap，输出按 key 排序，
    // 与 Go 的 json.Marshal(map) 行为一致）。
    let mut base: Map<String, Value> = Map::new();
    base.insert("request_id".into(), json!(new_id.clone()));
    // ★ 与 request_id **同一个** newID
    base.insert("chat_record_id".into(), json!(new_id));
    base.insert("request_set_id".into(), json!(new_uuid()));
    // ★★ session_id 必须用**首条** user 文本派生（不是最后一条）：
    //    它在整段对话里稳定，上游的会话缓存才能命中。官方客户端就是这么做的
    //    ——`sessionId` 在整个会话里不变（见 2026-10-20 现场实测日志）。
    //    用 `prompt`（最后一条）会让每轮都是新 session。
    //
    // ★★ 同时把**账号 uid** 混进派生种子：换号后不能复用上一个账号的会话键，
    //    否则新账号可能继承坏会话（现场表现为两号连续空流）。
    let session_scope = if meta.session_owner.is_empty() {
        meta.key.clone()
    } else {
        format!("{}\u{0}{}", meta.session_owner, meta.key)
    };
    base.insert(
        "session_id".into(),
        json!(derive_session_id(&session_scope, &first_user_text(messages))),
    );
    base.insert("stream".into(), json!(true));
    // ★ 官方是空串（旧参考实现写 personal_professional_trial，已按官方更正）。
    base.insert("aliyun_user_type".into(), json!(""));
    base.insert("agent_id".into(), json!("agent_common"));
    base.insert("chat_task".into(), json!("FREE_INPUT"));
    base.insert("is_reply".into(), json!(true));
    base.insert("is_retry".into(), json!(false));
    base.insert("source".into(), json!(1));
    base.insert("version".into(), json!("3"));
    base.insert("task_id".into(), json!("common"));
    base.insert("session_type".into(), json!("qodercli"));
    base.insert("model_config".into(), model_config.clone());
    base.insert("system".into(), Value::Array(system_blocks));
    // 官方 `parameters`：max_tokens 来自客户端 / 模型；没有就不带这一项。
    let mut parameters: Map<String, Value> = Map::new();
    if let Some(mt) = max_tokens {
        parameters.insert("max_tokens".into(), json!(mt));
    }
    base.insert("parameters".into(), Value::Object(parameters));
    // `chat_context.text` 是上游协议**必填**项（哪怕 messages 里已有完整历史）。
    // 官方该字段是**字符串**，extra 里的 modelConfig 只带 key / is_reasoning。
    base.insert(
        "chat_context".into(),
        json!({
            "chatPrompt": "",
            "text": prompt,
            "extra": {
                "context": [],
                "modelConfig": {"key": meta.key, "is_reasoning": meta.is_reasoning},
                "originalContent": prompt,
            },
            "features": [],
            "imageUrls": Value::Null,
        }),
    );
    base.insert("messages".into(), Value::Array(sanitized_messages));
    // 官方恒带 `tools`（没传就是空数组）。
    base.insert("tools".into(), Value::Array(sanitized_tools));
    base.insert(
        "business".into(),
        json!({
            "id": new_uuid(),
            "begin_at": now_ms,
            "name": truncate_runes(&prompt, 30),
        }),
    );

    serde_json::to_vec(&Value::Object(base)).map_err(|e| format!("序列化 Qoder 请求体失败: {e}"))
}

/// 把 `developer` 角色就地规范化成 `system`。
///
/// # 为什么必须做
///
/// 客户端对 `openai-completions` 会用 `developer` 替代 `system`
///（pi-ai 的 `supportsDeveloperRole` 为 true 时），而 **qoder 上游只认 `system`**。
/// 修复前的现象是对话根本发不出去，每次
/// `503 {"code":"no_healthy_account", ...}`，而同网关同时刻 `zcode:` 与 curl 都好
/// ⇒ 用户会反复说「zcode 没问题、qoder 不行」。
///
/// 另两条路径都已规范化（`internal/upstream/payload.go`、
/// `internal/zcode/translate.go`），qoder 是唯一漏掉的。
///
/// ⚠ 复现关键是**组合**：`developer` 单独发（不带 tools、非流式）→ 上游容忍 200；
/// `developer + tools + 流式` → **必然 503**。只测裸 developer 会误判。
fn normalize_developer_roles(messages: &[Value]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    for msg in messages {
        let mut msg = msg.clone();
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role.trim().eq_ignore_ascii_case("developer") {
            if let Some(obj) = msg.as_object_mut() {
                obj.insert("role".to_string(), json!("system"));
            }
        }
        out.push(msg);
    }
    out
}

/// 把 system 消息拆出来（官方形状：`system` 是**独立数组**，`messages` 里不带 system）。
///
/// 官方 `bve`：`system` = 系统提示块数组，`messages` = 对话消息（system 已被移除）。
/// 旧实现把 system 留在 `messages` 里 —— 简单请求能过，但复杂请求（工具 + 长
/// system）会被上游静默回 `[DONE]`。
fn split_system(messages: &[Value]) -> (Vec<String>, Vec<Value>) {
    let mut system = Vec::new();
    let mut conversation = Vec::new();
    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        if role.trim().eq_ignore_ascii_case("system") {
            let text = content_text_of(msg.get("content").unwrap_or(&Value::Null));
            if !text.is_empty() {
                system.push(text);
            }
        } else {
            conversation.push(msg.clone());
        }
    }
    (system, conversation)
}

/// 把 `content` 归一成块数组（官方 `contents` 的形状）。
///
/// 只保留三种块：`text`（含 `input_text`）、`image_url`、`image`；其余丢弃
/// （上游不认识，带着只会让它静默失败）。
fn content_parts(content: &Value) -> Vec<Value> {
    match content {
        Value::String(s) if !s.is_empty() => vec![json!({ "type": "text", "text": s })],
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                let ty = item.get("type").and_then(Value::as_str).unwrap_or("");
                match ty {
                    "text" | "input_text" => {
                        if let Some(t) = item.get("text").and_then(Value::as_str) {
                            if !t.is_empty() {
                                out.push(json!({ "type": "text", "text": t }));
                            }
                        }
                    }
                    "image_url" | "image" => out.push(item.clone()),
                    _ => {}
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// 从 `content` 里取纯文本（块数组按换行拼接）。
fn content_text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(_) => parts_text(&content_parts(content)),
        _ => String::new(),
    }
}

/// 块数组 → 纯文本。
fn parts_text(parts: &[Value]) -> String {
    parts
        .iter()
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 把一条 OpenAI 消息归一成官方形状（带 `contents` 块；system 已在上游拆走）。
fn sanitize_message(msg: &Value) -> Option<Value> {
    let role = msg.get("role").and_then(Value::as_str)?.trim().to_ascii_lowercase();
    let content = msg.get("content").cloned().unwrap_or(Value::Null);
    let parts = content_parts(&content);
    let text = parts_text(&parts);
    let mut out = Map::new();
    out.insert("role".into(), json!(role));
    match role.as_str() {
        "assistant" => {
            // 官方 `gPc`：content 是字符串，contents 是块数组，tool_calls 是标准 OpenAI 形状。
            out.insert("content".into(), json!(text));
            // 官方保留历史里的 reasoning_content（`gPc` 会写回同名字段）。
            if let Some(rc) = msg.get("reasoning_content").and_then(Value::as_str) {
                if !rc.is_empty() {
                    out.insert("reasoning_content".into(), json!(rc));
                }
            }
            let mut contents: Vec<Value> = parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                .cloned()
                .collect();
            let mut calls: Vec<Value> = Vec::new();
            for tc in msg.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                let id = tc.get("id").and_then(Value::as_str).unwrap_or("");
                let func = tc.get("function");
                let custom = tc.get("custom");
                let name = func
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .or_else(|| custom.and_then(|c| c.get("name")).and_then(Value::as_str))
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                // arguments 必须是**字符串**（标准 OpenAI）；custom 的 input 可能是字符串/对象。
                let arguments = func
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| {
                        custom.and_then(|c| c.get("input")).map(|v| match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                    })
                    .unwrap_or_else(|| "{}".to_string());
                calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                }));
                contents.push(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "arguments": arguments,
                }));
            }
            if !calls.is_empty() {
                out.insert("tool_calls".into(), Value::Array(calls));
            }
            if !contents.is_empty() {
                out.insert("contents".into(), Value::Array(contents));
            }
        }
        "tool" => {
            // 官方 `uPc` 的 tool 消息：content 是字符串 + tool_call_id；**带图片时**
            // content/contents 都是块数组（图片照常转发给视觉上游，不是丢弃）。
            let has_image = parts
                .iter()
                .any(|p| matches!(p.get("type").and_then(Value::as_str), Some("image_url") | Some("image")));
            if has_image {
                out.insert("content".into(), Value::Array(parts.clone()));
                out.insert("contents".into(), Value::Array(parts));
            } else {
                out.insert("content".into(), json!(text));
            }
            if let Some(id) = msg.get("tool_call_id") {
                out.insert("tool_call_id".into(), id.clone());
            }
            if let Some(name) = msg.get("name") {
                out.insert("name".into(), name.clone());
            }
        }
        _ => {
            // user（以及未知角色按 user 处理）：有图片时 content 是块数组，与官方一致。
            let has_image = parts
                .iter()
                .any(|p| matches!(p.get("type").and_then(Value::as_str), Some("image_url") | Some("image")));
            if has_image {
                out.insert("content".into(), Value::Array(parts.clone()));
            } else {
                out.insert("content".into(), json!(text));
            }
            if !parts.is_empty() {
                out.insert("contents".into(), Value::Array(parts));
            }
        }
    }
    Some(Value::Object(out))
}

/// 把工具归一成上游支持的 `type: "function"`。
///
/// # 为什么是**转换**而不是丢弃（2026-10-20）
///
/// DSH / pi-ai 在开启 grammar 采样时会发 `type: "custom"`（带 `format.grammar`），
/// 而官方 Qoder 客户端从不发这种形状 —— 上游对它**不报错**，只回 `[DONE]`。
/// 直接丢掉会让模型彻底看不到这个工具（用户的功能静默消失）；转成 function
/// 工具后模型仍能调用它，只是失去 grammar 强制约束（调用参数改用字符串输入）。
fn sanitize_tool(tool: &Value) -> Option<Value> {
    match tool.get("type").and_then(Value::as_str) {
        Some("function") => {
            let name = tool
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if name.is_empty() {
                return None;
            }
            Some(tool.clone())
        }
        Some("custom") => {
            let custom = tool.get("custom")?;
            let name = custom
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if name.is_empty() {
                return None;
            }
            let description = custom
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // grammar 约束转成「字符串输入 + 格式说明」：模型仍能按语法产出内容。
            let hint = custom
                .pointer("/format/grammar/definition")
                .and_then(Value::as_str)
                .or_else(|| custom.pointer("/format/grammar/syntax").and_then(Value::as_str))
                .unwrap_or("");
            let mut parameters = json!({
                "type": "object",
                "properties": {"input": {"type": "string"}},
                "required": ["input"],
            });
            if !hint.is_empty() {
                parameters["properties"]["input"]["description"] = json!(format!(
                    "请输出符合以下语法定义的内容：{hint}"
                ));
            }
            Some(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": parameters,
                },
            }))
        }
        _ => None,
    }
}

/// 统计消息里的图片块数量（只用于日志，不改变内容）。
fn count_image_blocks(messages: &[Value]) -> usize {
    messages
        .iter()
        .filter_map(|m| m.get("contents").and_then(Value::as_array))
        .flatten()
        .filter(|p| {
            matches!(
                p.get("type").and_then(Value::as_str),
                Some("image_url") | Some("image")
            )
        })
        .count()
}

/// 会话 ID：`sha256(会话命名空间 + "\0" + 首条 user 文本)` 取前 16 字节拼 UUID 形态。
///
/// `session_namespace` 通常是上游模型 key；生产路径由 [`agent_body`] 拼成
/// `账号 uid + "\0" + 模型 key`，避免换号后复用上一个账号的会话键。
///
/// # 为什么不能每次 new UUID
///
/// 上游按 `session_id` 做**会话级 prompt cache**。每轮新 session ⇒ 缓存
/// **在结构上不可能命中** ⇒ 每轮把完整上下文当新前缀重新计费
///（agentic IDE 动辄几万 token）。
///
/// # ⚠ 绝不能把 system 纳入哈希
///
/// CodeBuddy 等 agentic IDE 每轮向 system prompt 注入动态内容
///（时间 / git 快照 / linter / 光标），system 逐轮变 → session 逐轮变
/// → 上游缓存永远读不到上一轮前缀（**命中恒 0**）。
/// 只用「会话命名空间 + 首条 user 文本」。不引入安全风险（session_id 不是凭据）。
///
/// `\0` 是分隔符：避免 `("ab","c")` 与 `("a","bc")` 撞同一个哈希。
pub(crate) fn derive_session_id(session_namespace: &str, first_user_text: &str) -> String {
    let mut h = Sha256::new();
    h.update(session_namespace.as_bytes());
    h.update([0u8]);
    h.update(first_user_text.as_bytes());
    let sum = h.finalize();
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        sum[0], sum[1], sum[2], sum[3],
        sum[4], sum[5],
        sum[6], sum[7],
        sum[8], sum[9],
        sum[10], sum[11], sum[12], sum[13], sum[14], sum[15],
    )
}

/// 取**最后一条** user 消息的文本。
///
/// `content` 是字符串时直接返回（非空）；是数组（多模态）时拼接所有
/// `{"type":"text","text":...}` 段，忽略 `image_url`。
///
/// 只认字符串会让带图片的请求拿到空 prompt，而 `chat_context.text`
/// 为空时上游可能直接拒绝。
pub(crate) fn last_user_text(messages: &[Value]) -> String {
    messages.iter().rev().find_map(user_text_of).unwrap_or_default()
}

/// 取**首条** user 消息的文本（`session_id` 的稳定输入）。
///
/// # 为什么必须是**首条**（2026-10-20 现场）
///
/// 官方 Qoder 客户端的 `sessionId` 在**整段对话**里不变（实测日志：
/// `sessionId=8937ddb7…` 连续数十个请求都一样）。上游按它做会话级缓存；
/// 若每轮取最后一条 user 文本，多轮对话的每一轮都会派生出**新的**
/// `session_id` ⇒ 缓存永不命中，且上游会把同一段对话当成大量新会话，
/// 触发限流/空流的概率显著上升。
///
/// 首条 user 文本在同一次对话中天然稳定（用户不会改第一句话），
/// 跨对话又不同，是网关在**无状态**前提下能做到的最好近似。
pub(crate) fn first_user_text(messages: &[Value]) -> String {
    messages.iter().find_map(user_text_of).unwrap_or_default()
}

/// 从一条消息里取文本；非 user 或空文本返回 `None`。
fn user_text_of(msg: &Value) -> Option<String> {
    if msg.get("role").and_then(|v| v.as_str()) != Some("user") {
        return None;
    }
    match msg.get("content")? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Array(parts) => {
            let mut sb = String::new();
            for p in parts {
                if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                    if !t.is_empty() {
                        sb.push_str(t);
                    }
                }
            }
            if sb.is_empty() {
                None
            } else {
                Some(sb)
            }
        }
        _ => None,
    }
}

/// 按**字符**（不是字节）截断 —— 按字节截会把汉字劈两半。
pub(crate) fn truncate_runes(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        return s.to_string();
    }
    chars[..n].iter().collect()
}

fn now_unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: &[u8]) -> Value {
        serde_json::from_slice(v).unwrap()
    }

    #[test]
    fn developer_role_is_normalized_to_system() {
        let body = br#"{"model":"m","messages":[
            {"role":"developer","content":"you are helpful"},
            {"role":"user","content":"hi"}]}"#;
        let out = parse(&build_agent_body(body, &ModelMeta::key_only("qmodel_x")).unwrap());
        // developer → system，且 system 按官方形状拆到独立的 `system` 字段
        assert_eq!(out["system"][0]["text"], "you are helpful");
        assert_eq!(out["messages"][0]["role"], "user", "messages 里不该再有 system");
        // 大小写/空格也要认
        let body2 = br#"{"messages":[{"role":" Developer ","content":"x"},{"role":"user","content":"y"}]}"#;
        let out2 = parse(&build_agent_body(body2, &ModelMeta::key_only("k")).unwrap());
        assert_eq!(out2["system"][0]["text"], "x");
        assert_eq!(out2["messages"][0]["role"], "user");
    }

    #[test]
    fn system_is_split_out_of_messages_into_its_own_field() {
        let body = r#"{"messages":[
            {"role":"system","content":"你是助手"},
            {"role":"user","content":"hi"},
            {"role":"system","content":"补充规则"}]}"#;
        let out = parse(&build_agent_body(body.as_bytes(), &ModelMeta::key_only("k")).unwrap());
        assert_eq!(out["system"].as_array().unwrap().len(), 2);
        assert_eq!(out["system"][0]["text"], "你是助手");
        assert_eq!(out["system"][1]["text"], "补充规则");
        assert_eq!(out["messages"].as_array().unwrap().len(), 1, "system 不得留在 messages 里");
        assert_eq!(out["messages"][0]["role"], "user");
    }

    #[test]
    fn model_config_carries_every_field_the_upstream_expects() {
        // ★ 现场缺陷：只发 {key, is_reasoning, source} 三个字段时，上游会回落成
        //   「官方客户端默认身份」—— 模型名传对了，回答却自称是 Qoder。
        //   这条测试钉住「必须发全 10 个字段」。
        let meta = ModelMeta {
            key: "qmodel_preview".into(),
            display_name: "DeepSeek-Flash".into(),
            is_reasoning: true,
            is_vl: false,
            max_input_tokens: 128_000,
            ..Default::default()
        };
        let out = parse(&build_agent_body(br#"{"messages":[{"role":"user","content":"hi"}]}"#, &meta).unwrap());
        let mc = &out["model_config"];
        assert_eq!(mc["key"], "qmodel_preview");
        assert_eq!(mc["display_name"], "DeepSeek-Flash");
        assert_eq!(mc["model"], "");
        assert_eq!(mc["format"], "openai");
        assert_eq!(mc["is_vl"], false);
        assert_eq!(mc["is_reasoning"], true, "必须如实转发清单里的值");
        assert_eq!(mc["api_key"], "");
        assert_eq!(mc["url"], "");
        assert_eq!(mc["source"], "system");
        assert_eq!(mc["max_input_tokens"], 128_000);
        // 字段总数：少一个就说明有人删了
        assert_eq!(
            mc.as_object().unwrap().len(),
            10,
            "model_config 必须发全 10 个字段，实际: {mc}"
        );
        // chat_context.extra 里按**官方形状**只带 {key, is_reasoning}
        //（完整 10 字段只放顶层 model_config）。
        let extra = &out["chat_context"]["extra"]["modelConfig"];
        assert_eq!(extra["key"], "qmodel_preview");
        assert_eq!(extra["is_reasoning"], true);
        assert_eq!(extra.as_object().unwrap().len(), 2, "官方 extra.modelConfig 只有 2 个键");
    }

    #[test]
    fn model_config_falls_back_when_metadata_is_missing() {
        // 清单里查不到（qmodel* 逃生口 / 清单为空）⇒ key-only 也必须能构造出
        // 完整的 model_config，用兜底值填，而不是漏字段。
        let out = parse(&build_agent_body(br#"{"messages":[{"role":"user","content":"hi"}]}"#, &ModelMeta::key_only("k")).unwrap());
        let mc = &out["model_config"];
        assert_eq!(mc["key"], "k");
        assert_eq!(mc["display_name"], "k", "空展示名回落 key");
        assert_eq!(mc["max_input_tokens"], DEFAULT_MAX_INPUT_TOKENS);
        assert_eq!(mc.as_object().unwrap().len(), 10);
        // 非推理模型默认 false，视觉默认 false
        assert_eq!(mc["is_reasoning"], false);
        assert_eq!(mc["is_vl"], false);
    }

    #[test]
    fn model_config_zero_max_tokens_falls_back() {
        // 清单里 max_input_tokens 为 0 时不能原样发 0（上游会认为收不了任何输入）
        let meta = ModelMeta { key: "k".into(), max_input_tokens: 0, ..Default::default() };
        let out = parse(&build_agent_body(br#"{"messages":[{"role":"user","content":"hi"}]}"#, &meta).unwrap());
        assert_eq!(out["model_config"]["max_input_tokens"], DEFAULT_MAX_INPUT_TOKENS);
    }

    #[test]
    fn model_config_has_source_system_and_is_reasoning_false() {
        let out = parse(&build_agent_body(br#"{"messages":[{"role":"user","content":"hi"}]}"#, &ModelMeta::key_only("qmodel_preview")).unwrap());
        assert_eq!(out["model_config"]["key"], "qmodel_preview");
        assert_eq!(out["model_config"]["source"], "system");
        assert_eq!(out["model_config"]["is_reasoning"], false);
        // chat_context.extra 里按官方形状只带 key / is_reasoning
        assert_eq!(out["chat_context"]["extra"]["modelConfig"]["key"], "qmodel_preview");
        assert_eq!(out["chat_context"]["extra"]["modelConfig"]["is_reasoning"], false);
    }

    #[test]
    fn base_fields_match_go_spec() {
        // ★ 非 ASCII 内容必须走 `&str` 再 `.as_bytes()`：`br#"..."#` 是**字节**串，
        // 只接受 ASCII（rustc 会直接报 "non-ASCII character in raw byte string literal"）。
        let body = r#"{"messages":[{"role":"user","content":"你好"}]}"#;
        let out = parse(&build_agent_body(body.as_bytes(), &ModelMeta::key_only("k")).unwrap());
        assert_eq!(out["agent_id"], "agent_common");
        assert_eq!(out["chat_task"], "FREE_INPUT");
        assert_eq!(out["session_type"], "qodercli");
        assert_eq!(out["stream"], true);
        assert_eq!(out["is_reply"], true);
        // ★ 官方形状（2026-10-20 对齐）：空串 + source/version/is_retry/task_id
        assert_eq!(out["aliyun_user_type"], "");
        assert_eq!(out["source"], 1);
        assert_eq!(out["version"], "3");
        assert_eq!(out["is_retry"], false);
        assert_eq!(out["task_id"], "common");
        assert!(out["system"].as_array().unwrap().is_empty());
        assert!(out["tools"].as_array().unwrap().is_empty());
        assert!(out["parameters"].is_object());
        assert!(out.get("image_urls").is_none(), "官方顶层没有 image_urls");
        assert!(out["chat_context"]["imageUrls"].is_null());
        assert!(out["chat_context"]["features"].as_array().unwrap().is_empty());
        assert!(out["chat_context"]["extra"]["context"].as_array().unwrap().is_empty());
        // request_id 与 chat_record_id 必须相同
        assert_eq!(out["request_id"], out["chat_record_id"]);
        // 但 request_set_id / business.id 是各自独立的
        assert_ne!(out["request_id"], out["request_set_id"]);
        assert_ne!(out["request_set_id"], out["business"]["id"]);
        // chat_context.text / originalContent 官方是**字符串**（不再是 {type,text}）
        assert_eq!(out["chat_context"]["text"], "你好");
        assert_eq!(out["chat_context"]["extra"]["originalContent"], "你好");
        assert_eq!(out["chat_context"]["chatPrompt"], "");
        // 消息带 contents 块（official 形状）
        assert_eq!(out["messages"][0]["content"], "你好");
        assert_eq!(out["messages"][0]["contents"][0]["type"], "text");
        assert_eq!(out["messages"][0]["contents"][0]["text"], "你好");
    }

    #[test]
    fn business_name_is_truncated_by_runes() {
        let long = "一".repeat(50);
        let body = serde_json::to_vec(&json!({
            "messages": [{"role":"user","content": long}]
        }))
        .unwrap();
        let out = parse(&build_agent_body(&body, &ModelMeta::key_only("k")).unwrap());
        let name = out["business"]["name"].as_str().unwrap();
        assert_eq!(name.chars().count(), 30, "按字符截断，不能按字节");
        assert_eq!(name, "一".repeat(30));
    }

    #[test]
    fn tools_are_always_forwarded_in_official_shape() {
        // 官方恒带 tools（没传就是空数组）
        let out = parse(&build_agent_body(br#"{"messages":[{"role":"user","content":"hi"}]}"#, &ModelMeta::key_only("k")).unwrap());
        assert_eq!(out["tools"].as_array().unwrap().len(), 0, "没传 tools ⇒ 空数组");
        // 带 tools：function 原样
        let body = br#"{"messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"f"}}]}"#;
        let out = parse(&build_agent_body(body, &ModelMeta::key_only("k")).unwrap());
        assert_eq!(out["tools"][0]["function"]["name"], "f");
        // 空数组 ⇒ 空数组
        let out = parse(&build_agent_body(br#"{"messages":[{"role":"user","content":"hi"}],"tools":[]}"#, &ModelMeta::key_only("k")).unwrap());
        assert_eq!(out["tools"].as_array().unwrap().len(), 0);
    }

    /// ★★ `type:"custom"`（grammar 工具）必须**转换**成 function，而不是丢弃。
    ///
    /// DSH / pi-ai 开启 grammar 采样时会发 custom 工具；官方 Qoder 客户端从不发
    /// 这种形状 —— 上游对它不报错，只回 `[DONE]`。丢掉会让模型看不到这个工具。
    #[test]
    fn custom_tool_is_converted_to_function_not_dropped() {
        let body = r#"{"messages":[{"role":"user","content":"hi"}],"tools":[
            {"type":"custom","custom":{"name":"run_grammar","description":"按语法输出",
             "format":{"type":"grammar","grammar":{"syntax":"regex","definition":"^[0-9]+$"}}}}]}"#;
        let out = parse(&build_agent_body(body.as_bytes(), &ModelMeta::key_only("k")).unwrap());
        assert_eq!(out["tools"].as_array().unwrap().len(), 1, "custom 工具必须被保留（转换后）");
        let f = &out["tools"][0]["function"];
        assert_eq!(f["name"], "run_grammar");
        assert_eq!(f["parameters"]["properties"]["input"]["type"], "string");
        assert!(
            f["parameters"]["properties"]["input"]["description"]
                .as_str()
                .unwrap()
                .contains("^[0-9]+$"),
            "语法定义要转成输入说明：{f}"
        );
    }

    /// 工具结果里的图片不能被丢弃：按官方形状保留 `content`/`contents` 块。
    #[test]
    fn tool_result_image_is_preserved_as_blocks() {
        let body = r#"{"messages":[{"role":"user","content":"看图"},{"role":"assistant","content":"","tool_calls":[{"id":"c1","type":"function","function":{"name":"shot","arguments":"{}"}}]},{"role":"tool","tool_call_id":"c1","content":[{"type":"text","text":"截图如下"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}]}]}"#;
        let out = parse(&build_agent_body(body.as_bytes(), &ModelMeta::key_only("k")).unwrap());
        let tool_msg = &out["messages"][2];
        assert_eq!(tool_msg["role"], "tool");
        assert_eq!(tool_msg["content"][0]["text"], "截图如下", "图片存在时 content 是块数组");
        assert_eq!(tool_msg["contents"][1]["type"], "image_url");
    }

    /// assistant 历史里的 `reasoning_content` 按官方形状保留（不丢）。
    #[test]
    fn assistant_reasoning_content_is_preserved() {
        let body = r#"{"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"ok","reasoning_content":"先想一下"}]}"#;
        let out = parse(&build_agent_body(body.as_bytes(), &ModelMeta::key_only("k")).unwrap());
        assert_eq!(out["messages"][1]["reasoning_content"], "先想一下");
    }

    #[test]
    fn build_rejects_bad_body_and_missing_messages() {
        assert!(build_agent_body(b"not json", &ModelMeta::key_only("k")).unwrap_err().contains("解析 OpenAI 请求体失败"));
        let e = build_agent_body(br#"{"model":"m"}"#, &ModelMeta::key_only("k")).unwrap_err();
        assert!(e.contains("没有 messages"), "实际: {e}");
        assert!(build_agent_body(br#"{"messages":[]}"#, &ModelMeta::key_only("k")).unwrap_err().contains("没有 messages"));
    }

    #[test]
    fn session_id_is_stable_and_uuid_shaped() {
        let a = derive_session_id("k", "hello");
        let b = derive_session_id("k", "hello");
        assert_eq!(a, b, "同输入必须同 session（否则上游缓存恒不命中）");
        assert_ne!(a, derive_session_id("k2", "hello"));
        assert_ne!(a, derive_session_id("k", "hello2"));
        // UUID 形态（上游对非 UUID 形态会拒）
        assert_eq!(a.len(), 36);
        assert_eq!(a.chars().filter(|c| *c == '-').count(), 4);
        let segs: Vec<&str> = a.split('-').collect();
        assert_eq!(
            segs.iter().map(|s| s.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
    }

    #[test]
    fn session_id_separator_prevents_collision() {
        // ("ab","c") 与 ("a","bc") 不能撞同一个
        assert_ne!(derive_session_id("ab", "c"), derive_session_id("a", "bc"));
    }

    #[test]
    fn last_user_text_handles_string_and_parts_and_skips_system() {
        let msgs: Vec<Value> = serde_json::from_str(
            r#"[{"role":"system","content":"sys"},
                {"role":"user","content":"first"},
                {"role":"assistant","content":"mid"},
                {"role":"user","content":"last"}]"#,
        )
        .unwrap();
        assert_eq!(last_user_text(&msgs), "last", "必须从后往前取最后一条 user");

        // 多模态 parts：只拼 text，忽略 image_url
        let msgs: Vec<Value> = serde_json::from_str(
            r#"[{"role":"user","content":[
                {"type":"text","text":"看这"},
                {"type":"image_url","image_url":{"url":"data:..."}},
                {"type":"text","text":"张图"}]}]"#,
        )
        .unwrap();
        assert_eq!(last_user_text(&msgs), "看这张图");

        // 空字符串的 user 继续往前找
        let msgs: Vec<Value> = serde_json::from_str(
            r#"[{"role":"user","content":"earlier"},{"role":"user","content":""}]"#,
        )
        .unwrap();
        assert_eq!(last_user_text(&msgs), "earlier");

        // 什么都没有 ⇒ 空串（调用方不能因此 panic）
        let msgs: Vec<Value> = serde_json::from_str(r#"[{"role":"system","content":"s"}]"#).unwrap();
        assert_eq!(last_user_text(&msgs), "");
    }

    #[test]
    fn session_id_ignores_system_prompt_drift() {
        // 上游原话：system 逐轮变会让缓存命中恒 0 ⇒ 绝不能纳入哈希
        let a = br#"{"messages":[{"role":"system","content":"time=1"},{"role":"user","content":"hi"}]}"#;
        let b = br#"{"messages":[{"role":"system","content":"time=2"},{"role":"user","content":"hi"}]}"#;
        let sa = parse(&build_agent_body(a, &ModelMeta::key_only("k")).unwrap())["session_id"].clone();
        let sb = parse(&build_agent_body(b, &ModelMeta::key_only("k")).unwrap())["session_id"].clone();
        assert_eq!(sa, sb, "system 变化不得改变 session_id");
    }

    /// ★★ 现场缺陷（2026-10-20）：`session_id` 此前用**最后一条** user 文本派生，
    /// 于是多轮对话的每一轮都是新 session —— 而上游按它做会话级缓存/会话计数。
    /// 官方客户端的 `sessionId` 在整段对话里**不变**（实测日志：同一个 UUID
    /// 连续数十个请求）。
    ///
    /// 这条钉住「新增一条 user 消息不得改变 session_id」。
    #[test]
    fn session_id_is_stable_across_turns() {
        let turn1 = r#"{"messages":[
            {"role":"user","content":"第一句：帮我看看这个项目"},
            {"role":"assistant","content":"好的"}]}"#;
        let turn2 = r#"{"messages":[
            {"role":"user","content":"第一句：帮我看看这个项目"},
            {"role":"assistant","content":"好的"},
            {"role":"user","content":"第二句：现在改一下"}]}"#;
        let s1 = parse(&build_agent_body(turn1.as_bytes(), &ModelMeta::key_only("dfmodel")).unwrap())["session_id"].clone();
        let s2 = parse(&build_agent_body(turn2.as_bytes(), &ModelMeta::key_only("dfmodel")).unwrap())["session_id"].clone();
        assert_eq!(s1, s2, "同一段对话新增一轮后 session_id 必须保持不变");
    }

    /// 首条 user 文本不同 ⇒ 不同对话，session_id 必须不同。
    #[test]
    fn session_id_differs_across_conversations() {
        let a = r#"{"messages":[{"role":"user","content":"问题 A"},{"role":"assistant","content":"x"},{"role":"user","content":"追问"}]}"#;
        let b = r#"{"messages":[{"role":"user","content":"问题 B"},{"role":"assistant","content":"x"},{"role":"user","content":"追问"}]}"#;
        let sa = parse(&build_agent_body(a.as_bytes(), &ModelMeta::key_only("k")).unwrap())["session_id"].clone();
        let sb = parse(&build_agent_body(b.as_bytes(), &ModelMeta::key_only("k")).unwrap())["session_id"].clone();
        assert_ne!(sa, sb, "不同对话不能共用 session_id（否则跨对话串味）");
    }

    /// ★★ 会话按键号隔离：同一段对话换号后必须换 session_id。
    ///
    /// 否则新账号会继承前一个账号的坏会话 —— 现场表现为两个账号**连续空流**，
    /// 而官方客户端（一个会话固定一个账号）完全正常。
    #[test]
    fn session_id_is_scoped_to_account() {
        let body = r#"{"messages":[{"role":"user","content":"同一段对话"}]}"#;
        let mut a = ModelMeta::key_only("dfmodel");
        a.session_owner = "account-a".into();
        let mut b = ModelMeta::key_only("dfmodel");
        b.session_owner = "account-b".into();
        let sa = parse(&build_agent_body(body.as_bytes(), &a).unwrap())["session_id"].clone();
        let sb = parse(&build_agent_body(body.as_bytes(), &b).unwrap())["session_id"].clone();
        assert_ne!(sa, sb, "换号必须换 session_id，不能复用上一个账号的会话键");
        // 同一账号重复构造必须稳定
        let sa2 = parse(&build_agent_body(body.as_bytes(), &a).unwrap())["session_id"].clone();
        assert_eq!(sa, sa2);
    }

    #[test]
    fn first_user_text_is_stable_and_handles_parts() {
        let msgs: Vec<Value> = serde_json::from_str(
            r#"[{"role":"system","content":"sys"},
                {"role":"user","content":"first"},
                {"role":"assistant","content":"mid"},
                {"role":"user","content":"last"}]"#,
        )
        .unwrap();
        assert_eq!(first_user_text(&msgs), "first");
        let parts: Vec<Value> = serde_json::from_str(
            r#"[{"role":"user","content":[
                {"type":"text","text":"看这"},
                {"type":"image_url","image_url":{"url":"data:..."}},
                {"type":"text","text":"张图"}]}]"#,
        )
        .unwrap();
        assert_eq!(first_user_text(&parts), "看这张图");
        let none: Vec<Value> =
            serde_json::from_str(r#"[{"role":"system","content":"s"}]"#).unwrap();
        assert_eq!(first_user_text(&none), "");
    }

    #[test]
    fn full_history_is_passed_through() {
        let body = br#"{"messages":[
            {"role":"system","content":"s"},
            {"role":"user","content":"u1"},
            {"role":"assistant","content":"a1"},
            {"role":"tool","content":"t1","tool_call_id":"x"},
            {"role":"user","content":"u2"}]}"#;
        let out = parse(&build_agent_body(body, &ModelMeta::key_only("k")).unwrap());
        // system 拆到 system 字段；其余全量透传（原来 5 条 → 现在 4 条）
        assert_eq!(out["messages"].as_array().unwrap().len(), 4, "对话消息全量透传");
        assert_eq!(out["system"][0]["text"], "s");
        assert_eq!(out["messages"][2]["role"], "tool");
        assert_eq!(out["messages"][2]["tool_call_id"], "x");
    }
}
