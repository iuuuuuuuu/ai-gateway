//! Anthropic ⇄ OpenAI 协议翻译 —— Go `internal/zcode/translate.go` 的 Rust 移植。
//!
//! # 为什么需要翻译（这不是可选项）
//!
//! 我们最初假设 ZCode 是"OpenAI 原样透传"的通道。**那个假设是错的**：
//! 打 `open.bigmodel.cn/api/coding/paas/v4/chat/completions`（OpenAI 形状）
//! 恒回 `429 {"code":1113,"msg":"余额不足或无可用资源包"}`，而官方客户端打的
//! `zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages`（Anthropic 形状）
//! HTTP 200、流式正常。
//!
//! 那个 1113 **不是"账号没钱"**（同一账号有 300 万 + 500 万 token/日），
//! 而是"**这条通道没有该账号的资源包**" —— 额度挂在 start-plan 通道上。
//!
//! # 两个方向的协议差异（照抄 Go 侧文件头）
//!
//! ```text
//!                      OpenAI 形态                       Anthropic 形态
//!   system             messages 里 role:"system"        顶层 system 字段（可数组）
//!   消息内容           字符串或数组                      必须是块数组
//!   工具               tools[].function.{name,…}         tools[].{name,description,input_schema}
//!   工具调用           assistant.tool_calls               assistant.content[].type="tool_use"
//!   工具结果           role:"tool"                        user.content[].type="tool_result"
//!   生成长度上限        max_tokens 可选                    max_tokens **必填**
//!   停止词             stop                              stop_sequences
//!   思考               reasoning_content（非标准）         thinking + output_config.effort
//!   响应                choices[].delta                   event: content_block_delta 等
//!   结束标记            data: [DONE]                      event: message_stop
//! ```
//!
//! ⚠ **翻译错了不会报错**，只会让用户看到空回答或上游 400 ⇒ 两个方向都必须单测。
//!
//! # 模块结构
//!
//! * [`build_anthropic_body`] / [`build_anthropic_body_with_meta`] —— 方向一：请求体。
//! * [`AnthropicStreamTranslator`] —— 方向二的内核：**纯同步状态机**，喂一行 SSE
//!   吐一串 OpenAI chunk。抽成同步结构体是为了**单测不必起 HTTP 服务、不必进异步**
//!   （Go 侧靠 `io.Pipe` 达到同一目的）。
//! * [`translate_stream`] —— 把上面那个内核套到"上游字节流"上，产出可直接塞进
//!   `reqwest::Body::wrap_stream` 的 `Stream<Item = Result<Bytes, io::Error>>`。
//! * [`translate_non_stream`] / [`is_anthropic_body`] —— 方向三与非流式兜底。
//!   ⚠ 这两个在 Go 侧**生产代码零调用点**（只被 `translate_test.go` 调用）；
//!   移植它们是为了保住幂等兜底与错误体识别能力，**不要**在热路径上误以为它们在跑。

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::{pin_mut, StreamExt};
use rand::RngCore;
use serde_json::{json, Map, Value};

use super::systemblock::{build_official_system, system_block_enabled, EnvInfo};

/// `max_tokens` 的默认值。
///
/// Anthropic 的 `max_tokens` 是**必填**，而 OpenAI 客户端经常不传 ——
/// 直接透传会被上游 400，且错误信息不会说"缺 max_tokens"。
///
/// 取值来自 `builtinModels[].maxCompletionTokens`（实测抓包：GLM-5.3 与
/// GLM-5.3-Flash 都是 128000）。⚠ 这是**上限声明**而不是"要生成这么多"，
/// 上游按实际生成量计费。
pub const DEFAULT_MAX_TOKENS: i64 = 128000;

/// 官方请求体里的 `metadata.user_id` 载荷。
///
/// 官方每次对话都在请求体里带
/// `"metadata": {"user_id": "{\"device_id\":\"…\",\"account_uuid\":\"\",\"session_id\":\"…\"}"}`。
///
/// ⚠ `user_id` 的值本身是一个**序列化后的 JSON 字符串**（双重编码）——
/// 那是官方 SDK 的写法，照抄。上游据此把请求关联到"哪台设备、哪个会话"，
/// 而风控正是靠这类维度区分真实客户端与脚本 ⇒ 即便不影响认证也应发。
#[derive(Debug, Clone, Default)]
pub struct AnthropicMeta {
    pub device_id: String,
    pub account_uuid: String,
    pub session_id: String,
}

/// 把 OpenAI 的请求体翻成 Anthropic 的请求体（无会话元数据）。
pub fn build_anthropic_body(openai_body: &[u8], model: &str) -> Result<Vec<u8>, String> {
    build_anthropic_body_with_meta(openai_body, model, &AnthropicMeta::default())
}

/// 把 OpenAI 的请求体翻成 Anthropic 的请求体。
///
/// `model` 由调用方给定（已经过 [`super::dispatch`] 的模型名归一化），
/// 不再从请求体里读 —— 上游只认小写 id，读原值会把展示名当 API 名。
pub fn build_anthropic_body_with_meta(
    openai_body: &[u8],
    model: &str,
    meta: &AnthropicMeta,
) -> Result<Vec<u8>, String> {
    // ⚠ Go 侧 `json.Unmarshal(body, &in)` 的 `in` 是 `map[string]any`：
    // 顶层是数组/字符串/数字都会报错，唯独 `null` **不报错**（留下 nil map，
    // 后续走"没有可用的消息"分支）。这里逐字对齐。
    let in_obj: Map<String, Value> = match serde_json::from_slice::<Value>(openai_body) {
        Ok(Value::Object(m)) => m,
        Ok(Value::Null) => Map::new(),
        Ok(_) => return Err("解析 OpenAI 请求体失败: 请求体不是 JSON 对象".to_string()),
        Err(e) => return Err(format!("解析 OpenAI 请求体失败: {e}")),
    };

    let mut out: Map<String, Value> = Map::new();
    out.insert("model".to_string(), Value::String(model.to_string()));

    // ---- system：从 messages 里抽出来，放到顶层 ----
    //
    // Anthropic 的 system 是**顶层字段**，且可以是字符串或块数组。
    // 我们用块数组（与抓包一致，且支持 cache_control）。
    let empty: [Value; 0] = [];
    let raw_msgs: &[Value] = in_obj
        .get("messages")
        .and_then(|v| v.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&empty);

    let mut system_blocks: Vec<Value> = Vec::new();
    let mut msgs: Vec<Value> = Vec::with_capacity(raw_msgs.len());

    for rm in raw_msgs {
        let Some(m) = rm.as_object() else {
            continue;
        };
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
        match role {
            "system" | "developer" => {
                // OpenAI 的 system（含新的 developer 角色）→ Anthropic 顶层 system。
                //
                // ⚠ 合并成**一个**字符串块 vs 保留多块：官方抓包是**多块**
                //（每块可带 cache_control，用于提示缓存）。保留多块。
                //
                // ⚠ 空判据是 `!= ""`（不是 trim 后为空）—— 与 Go 逐字一致：
                // 客户端发一个只有空格的 system，Go 也会把它当成一块发出去。
                let txt = content_to_text(m.get("content"));
                if !txt.is_empty() {
                    system_blocks.push(json!({ "type": "text", "text": txt }));
                }
            }
            "user" => {
                msgs.push(json!({
                    "role": "user",
                    "content": openai_content_to_anthropic(m.get("content")),
                }));
            }
            "assistant" => {
                let mut blocks = openai_content_to_anthropic(m.get("content"));
                // 工具调用：OpenAI 的 `tool_calls[]` → Anthropic 的 content 里的
                // `tool_use` 块。
                if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
                    for t in tcs {
                        let Some(tm) = t.as_object() else {
                            continue;
                        };
                        let Some(fn_) = tm.get("function").and_then(|v| v.as_object()) else {
                            continue;
                        };
                        let name = fn_.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let args_str = fn_.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
                        let args: Value = if args_str.trim().is_empty() {
                            json!({})
                        } else {
                            // 解析失败时**原样塞成字符串**而不是丢弃 —— 丢弃会让模型
                            // 看到"调用了但没参数"，症状更怪。
                            serde_json::from_str(args_str)
                                .unwrap_or_else(|_| json!({ "__raw": args_str }))
                        };
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": str_or(tm.get("id"), &format!("toolu_{}", rand_id())),
                            "name": name,
                            "input": args,
                        }));
                    }
                }
                // ⚠ Anthropic 不接受 content 为**空数组**的消息 —— 但 assistant
                // 只带 tool_calls、没有文本时就会空。补一个空文本块兜住。
                if blocks.is_empty() {
                    blocks.push(json!({ "type": "text", "text": "" }));
                }
                msgs.push(json!({ "role": "assistant", "content": blocks }));
            }
            "tool" => {
                // OpenAI 的 tool 结果 → Anthropic 的 user + tool_result 块。
                //
                // ⚠ Anthropic **没有** `tool` 角色：工具结果要作为 **user**
                // 消息里的 `tool_result` 块回传。
                msgs.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": str_or(m.get("tool_call_id"), ""),
                        "content": content_to_text(m.get("content")),
                    }],
                }));
            }
            _ => {
                // 未知角色：当成 user 处理，不丢消息。
                msgs.push(json!({
                    "role": "user",
                    "content": openai_content_to_anthropic(m.get("content")),
                }));
            }
        }
    }

    if msgs.is_empty() {
        return Err("请求里没有可用的消息".to_string());
    }
    out.insert("messages".to_string(), Value::Array(msgs));

    if !system_blocks.is_empty() {
        out.insert("system".to_string(), Value::Array(system_blocks.clone()));
    }

    // ---- 官方 system 块注入（JWT 账号**必需**）----
    //
    // 参考实现 `gakiyukz/zcode2api-plus`（Go，持续更新、能跑通）在
    // `upstream/request.go` 里逐字写着：
    //
    //     // JWT 账号请求必须注入到顶层 system，否则上游返回 405。
    //
    // 而 **405 正是 3012 的载体**（`HTTP 405 {"code":3012,"msg":"request has
    // been blocked due to unusual activity."}`）。
    //
    // 我们此前**只从客户端 messages 里抽 system**（见上面的循环）：普通 API
    // 客户端（Cline / DSH / curl）根本不发 system 消息 ⇒ `system_blocks` 为空
    // ⇒ **官方块从未被注入**。而 `build_official_system` 早就写好了、形状也验证过，
    // 却在生产代码里**一个调用点都没有** —— 它只被测试调用。
    //
    // `systemblock.rs` 文件头那次 A/B（开/关两组都 3012）**是在风控期做的**：
    // 当时该账号连官方客户端都一律吃 3012。在"所有请求都失败"的窗口里比较两个
    // 方案，结论必然无差别 —— **那是实验设计的错误，不是方案无效。** 现在风控已
    // 解除且参考实现无条件注入能跑通 ⇒ 改为默认注入。
    //
    // ⚠ 官方块**排在调用方内容前面**（与参考实现一致）：替换会丢掉客户端的
    // 项目规范与工具约定。
    // ⚠ 注入失败**不阻断请求**（降级优于报错）。
    // ⚠ `system_block_enabled()` 是**逃生开关**（默认开，显式写 0 才关）。
    //
    // ⚠ Go 传的是 `envInfo{}` **零值**（`translate.go:262`）—— 请求体翻译层
    // 拿不到本机 cwd/shell/OS 版本，而这些字段取不到时整行省略。这里照抄零值，
    // 不是笔误。
    if system_block_enabled() {
        let existing = if system_blocks.is_empty() {
            None
        } else {
            Some(Value::Array(system_blocks))
        };
        if let Ok(blk) = build_official_system(existing.as_ref(), model, &EnvInfo::default()) {
            out.insert("system".to_string(), Value::Array(blk));
        }
    }

    // ---- metadata：官方的会话元数据（抓包实测必发）----
    if !meta.device_id.is_empty() || !meta.session_id.is_empty() {
        let inner = json!({
            "device_id": meta.device_id,
            "account_uuid": meta.account_uuid,
            "session_id": meta.session_id,
        });
        // 全字符串的 map 序列化不会失败；Go 侧同样忽略了这个错误。
        let inner_str = serde_json::to_string(&inner).unwrap_or_default();
        out.insert(
            "metadata".to_string(),
            json!({ "user_id": inner_str }),
        );
    }

    // ---- max_tokens：**必填** ----
    match num_or_zero(in_obj.get("max_tokens")) {
        Some(v) if v > 0.0 => {
            out.insert("max_tokens".to_string(), json_number_from_f64(v));
        }
        _ => {
            out.insert("max_tokens".to_string(), Value::Number(DEFAULT_MAX_TOKENS.into()));
        }
    }

    // ---- 采样参数（同名直通）----
    for k in ["temperature", "top_p"] {
        if let Some(v) = in_obj.get(k) {
            if !v.is_null() {
                out.insert(k.to_string(), v.clone());
            }
        }
    }

    // ---- stop → stop_sequences（OpenAI 允许 string 或 []string）----
    match in_obj.get("stop") {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                out.insert("stop_sequences".to_string(), json!([s]));
            }
        }
        Some(Value::Array(a)) => {
            if !a.is_empty() {
                out.insert("stop_sequences".to_string(), Value::Array(a.clone()));
            }
        }
        _ => {}
    }

    // ---- stream 直通 ----
    if let Some(v) = in_obj.get("stream").and_then(|v| v.as_bool()) {
        out.insert("stream".to_string(), Value::Bool(v));
    }

    // ---- tools ----
    //
    //     OpenAI     {type:"function", function:{name, description, parameters}}
    //     Anthropic  {name, description, input_schema}
    if let Some(tools) = in_obj.get("tools").and_then(|v| v.as_array()) {
        if !tools.is_empty() {
            let mut anth_tools: Vec<Value> = Vec::with_capacity(tools.len());
            for t in tools {
                let Some(tm) = t.as_object() else {
                    continue;
                };
                let Some(fn_) = tm.get("function").and_then(|v| v.as_object()) else {
                    // 已经是 Anthropic 形状（客户端直接按 Anthropic 发的）→ 原样保留
                    if tm.contains_key("input_schema") {
                        anth_tools.push(Value::Object(tm.clone()));
                    }
                    continue;
                };
                let name = fn_.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if name.is_empty() {
                    continue;
                }
                let mut at = Map::new();
                at.insert("name".to_string(), Value::String(name.to_string()));
                if let Some(d) = fn_.get("description").and_then(|v| v.as_str()) {
                    if !d.is_empty() {
                        at.insert("description".to_string(), Value::String(d.to_string()));
                    }
                }
                // input_schema 是**必填**：缺了上游会 400。
                match fn_.get("parameters") {
                    Some(p) if !p.is_null() => {
                        at.insert("input_schema".to_string(), p.clone());
                    }
                    _ => {
                        at.insert(
                            "input_schema".to_string(),
                            json!({ "type": "object", "properties": {} }),
                        );
                    }
                }
                anth_tools.push(Value::Object(at));
            }
            if !anth_tools.is_empty() {
                out.insert("tools".to_string(), Value::Array(anth_tools));
                // ⚠ Go 侧这里是无条件 `out["tool_choice"] = anthropicToolChoice(...)`，
                // 而该函数在 `"none"` 时返回 nil ⇒ Go 实际发出的是
                // `"tool_choice": null`，**与它自己的注释"不发"矛盾**。
                // Rust 侧按注释的**意图**办：返回 None 时**不插入该字段**。
                // 理由：缺字段对 Anthropic 明确等价于默认 `auto`（与 null 想表达的
                // "没有偏好"一致），而 `null` 是否被上游接受**没有实测证据**；
                // 两者语义相同、风险不对称，故选更保守的那个。
                // 这是与 Go 的**一处有意偏离**，已在报告里列出。
                if let Some(tc) = anthropic_tool_choice(in_obj.get("tool_choice")) {
                    out.insert("tool_choice".to_string(), tc);
                }
            }
        }
    }

    // ---- 思考（reasoning）----
    //
    // 官方抓包：
    //     "thinking": {"type":"enabled"},
    //     "output_config": {"effort":"low"}
    //
    // 且 `builtinModels` 里写明档位 → output_config.effort 的映射：
    //     low→"low"  high→"high"  max→"max"，defaultLevel="max"
    //
    // OpenAI 客户端用 `reasoning_effort` 表达档位，故这里做映射。
    // ⚠ 注意 Go 的 `else` 绑在**类型断言**上：`reasoning_effort` 存在但不是字符串
    //（例如数字）时也会去看 `reasoning`。这里用同样的结构。
    let effort = in_obj
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            in_obj
                .get("reasoning")
                .and_then(|v| v.as_object())
                .and_then(|rc| rc.get("effort"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });
    if let Some(eff) = effort {
        let lvl = anthropic_effort(&eff);
        if !lvl.is_empty() {
            out.insert("thinking".to_string(), json!({ "type": "enabled" }));
            out.insert("output_config".to_string(), json!({ "effort": lvl }));
        }
    }

    serde_json::to_vec(&out).map_err(|e| format!("序列化 Anthropic 请求体失败: {e}"))
}

/// 把客户端档位名映射成 `output_config.effort` 的取值。
///
/// 取值来自 `builtinModels[].reasoning.levels`（实测抓包）：
///
/// ```text
/// low → low / high → high / max → max
/// ```
///
/// 另有 `"medium"` 与 `"minimal"` 两个常见别名（OpenAI 系的档位名），分别就近
/// 映射到 high / low —— ⚠ **就近**而不是拒绝：拒绝会让"用户选了档位却完全没生效"，
/// 比"档位略偏"更糟。
fn anthropic_effort(eff: &str) -> &'static str {
    match eff.trim().to_lowercase().as_str() {
        "low" | "minimal" | "none" => "low",
        // 上游只认 low/high/max；medium 取中间值 high
        "medium" | "mid" | "default" => "high",
        "high" => "high",
        "max" | "xhigh" | "highest" => "max",
        _ => "",
    }
}

/// 把 OpenAI 的 `tool_choice` 翻成 Anthropic 形状。
///
/// ```text
///   OpenAI                        Anthropic
///   ────────────────────────────  ──────────────────────────────────────
///   "auto" / 缺省                 {"type":"auto"}
///   "none"                        （**不发** —— 见下）
///   "required"                    {"type":"any"}
///   {"function":{"name":"f"}}     {"type":"tool","name":"f"}
/// ```
///
/// ⚠ OpenAI 的 `"none"` 在 Anthropic 里**没有等价物**。这里选择**不发**
/// `tool_choice` 而不是发一个"禁用"的假值 —— Anthropic 只认 auto/any/tool，
/// 发个不认识的值会 400；而"不发"时上游默认 auto，配合"不发 tools"才是正确的
/// 禁用姿势（真正的禁用由调用方在 tools 层面处理）。
///
/// ⚠ 与 Go 的差异见调用点的注释：Go 会把这里的 `nil` 写进 map 产出
/// `"tool_choice": null`，Rust 侧按本注释的意图**不发**。
fn anthropic_tool_choice(v: Option<&Value>) -> Option<Value> {
    match v {
        Some(Value::String(s)) => match s.trim().to_lowercase().as_str() {
            "auto" | "" => Some(json!({ "type": "auto" })),
            "required" | "any" => Some(json!({ "type": "any" })),
            "none" => None,
            // Go 的 string 分支内部 switch 没有 default ⇒ 落到函数尾部的兜底
            _ => Some(json!({ "type": "auto" })),
        },
        Some(Value::Object(t)) => {
            if let Some(name) = t
                .get("function")
                .and_then(|v| v.as_object())
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
            {
                if !name.is_empty() {
                    return Some(json!({ "type": "tool", "name": name }));
                }
            }
            if let Some(ty) = t.get("type").and_then(|v| v.as_str()) {
                // 已经是 Anthropic 形状 → 原样
                if ty == "auto" || ty == "any" || ty == "tool" {
                    return Some(Value::Object(t.clone()));
                }
            }
            Some(json!({ "type": "auto" }))
        }
        _ => Some(json!({ "type": "auto" })),
    }
}

/// 把 OpenAI 的 content 转成 Anthropic 的块数组。
///
/// ```text
///   OpenAI 形态                         Anthropic 形态
///   ──────────────────────────────────  ──────────────────────────────────
///   "纯字符串"                          [{type:"text",text:"…"}]
///   [{type:"text",text:"…"}]            [{type:"text",text:"…"}]
///   [{type:"image_url",…}]              [{type:"image",source:{…}}]
/// ```
///
/// ⚠ OpenAI 的 `image_url` 有两种来源（http(s) URL 或 data: URI），Anthropic
/// 分别对应 `source.type="url"` 与 `source.type="base64"`。弄错会让**带图请求
/// 整体 400**，故单独处理。
fn openai_content_to_anthropic(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(s)) => {
            // 空串也要给一个空文本块（Go 侧同样如此）
            vec![json!({ "type": "text", "text": s })]
        }
        Some(Value::Array(items)) => {
            let mut out: Vec<Value> = Vec::with_capacity(items.len());
            for part in items {
                let Some(pm) = part.as_object() else {
                    if let Some(s) = part.as_str() {
                        out.push(json!({ "type": "text", "text": s }));
                    }
                    continue;
                };
                let ty = pm.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match ty {
                    "text" | "input_text" => {
                        out.push(json!({
                            "type": "text",
                            "text": str_or(pm.get("text"), ""),
                        }));
                    }
                    "image_url" | "input_image" => {
                        if let Some(blk) = openai_image_to_anthropic(pm) {
                            out.push(blk);
                        }
                    }
                    "image" => {
                        // 已经是 Anthropic 形状
                        out.push(Value::Object(pm.clone()));
                    }
                    _ => {
                        // 未知块：若有 text 字段就当文本，否则**保留原样**
                        //（上游可能认识我们不认识的新块类型；丢掉比透传更糟）
                        match pm.get("text").and_then(|v| v.as_str()) {
                            Some(s) => out.push(json!({ "type": "text", "text": s })),
                            None => out.push(Value::Object(pm.clone())),
                        }
                    }
                }
            }
            if out.is_empty() {
                vec![json!({ "type": "text", "text": "" })]
            } else {
                out
            }
        }
        Some(Value::Object(m)) => {
            // 单块对象（非数组）—— 少见但要兜住
            match m.get("text").and_then(|v| v.as_str()) {
                Some(s) => vec![json!({ "type": "text", "text": s })],
                None => vec![json!({ "type": "text", "text": "" })],
            }
        }
        Some(Value::Null) | None => vec![json!({ "type": "text", "text": "" })],
        Some(other) => vec![json!({ "type": "text", "text": go_sprint(other) })],
    }
}

/// 把 OpenAI 的 `image_url` 块翻成 Anthropic 的 `image` 块。
fn openai_image_to_anthropic(pm: &Map<String, Value>) -> Option<Value> {
    // OpenAI: {"type":"image_url","image_url":{"url":"…"}}
    let mut url = String::new();
    match pm.get("image_url") {
        Some(Value::Object(iu)) => {
            if let Some(u) = iu.get("url").and_then(|v| v.as_str()) {
                url = u.to_string();
            }
        }
        Some(Value::String(s)) => url = s.clone(),
        _ => {}
    }
    // 有些客户端直接给 base64 或 url 字段
    if url.is_empty() {
        if let Some(u) = pm.get("url").and_then(|v| v.as_str()) {
            url = u.to_string();
        }
    }
    if let Some(b64) = pm.get("data").and_then(|v| v.as_str()) {
        if !b64.is_empty() {
            return Some(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": str_or(pm.get("media_type"), "image/png"),
                    "data": b64,
                },
            }));
        }
    }
    if url.is_empty() {
        return None;
    }
    // data: URI → base64 source
    if let Some(rest) = url.strip_prefix("data:") {
        // data:image/png;base64,XXXX
        let (media, data) = rest.split_once(',')?;
        let media = media.strip_suffix(";base64").unwrap_or(media);
        let media = if media.is_empty() { "image/png" } else { media };
        return Some(json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media, "data": data },
        }));
    }
    // http(s) URL → url source
    Some(json!({
        "type": "image",
        "source": { "type": "url", "url": url },
    }))
}

/// 把任意 content 压成纯文本（用于 system / tool 结果）。
fn content_to_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => {
            let mut b = String::new();
            for part in items {
                match part {
                    Value::String(s) => b.push_str(s),
                    Value::Object(m) => {
                        if let Some(s) = m.get("text").and_then(|v| v.as_str()) {
                            b.push_str(s);
                        }
                    }
                    _ => {}
                }
            }
            b
        }
        Some(Value::Object(m)) => m
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// 方向二：Anthropic 响应流 → OpenAI 响应流
// ---------------------------------------------------------------------------

/// 翻译内核：**纯同步**的 Anthropic SSE → OpenAI SSE 状态机。
///
/// 每喂一行（不含换行符）就吐一串 OpenAI 的 `data: …\n\n` 帧。
///
/// # 为什么抽成同步结构体
///
/// Go 侧靠 `io.Pipe` + goroutine 让翻译层只做"字节进、字节出"，从而**单测不必
/// 起 HTTP 服务**。Rust 侧把内核再往前推一步：状态机本身同步、可逐个事件断言，
/// 异步只留在 [`translate_stream`] 那一层薄封装里。这样最容易出错的部分
///（tool_use 编号、usage 拆分、`[DONE]` 补齐）全部是**确定性可测**的。
pub struct AnthropicStreamTranslator {
    id: String,
    created: i64,
    model: String,
    role_sent: bool,
    /// tool_use 块的 index → OpenAI 的 tool_calls 序号。
    /// Anthropic 用 `index`（content block 序号），OpenAI 用 tool_calls 数组下标，
    /// 两者**不是一回事**（中间可能有 text 块），故要单独编号。
    tool_idx: i64,
    block_to_tool: HashMap<i64, i64>,
    finished: bool,
}

impl AnthropicStreamTranslator {
    pub fn new(model: &str) -> Self {
        Self {
            id: format!("chatcmpl-{}", rand_id()),
            created: now_unix(),
            model: model.to_string(),
            role_sent: false,
            tool_idx: -1,
            block_to_tool: HashMap::new(),
            finished: false,
        }
    }

    /// 流是否已经结束（已写过 `[DONE]`）。
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// 处理一行上游 SSE（**不含**换行符），返回要下发的 OpenAI 帧。
    pub fn push_line(&mut self, line: &str) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        // ⚠ 只处理 `data:` 行 —— Anthropic 的 `event:` 行只是名字，类型在 JSON 里。
        let line = line.trim_end_matches('\r');
        let Some(rest) = line.strip_prefix("data:") else {
            return Vec::new();
        };
        let payload = rest.trim();
        if payload.is_empty() || payload == "[DONE]" {
            return Vec::new();
        }
        let Ok(ev) = serde_json::from_str::<Value>(payload) else {
            // 单个事件解析失败**不中断整条流** —— 中断会让用户看到的回答比上游
            // 实际给的短，且没有任何提示。跳过并继续。
            return Vec::new();
        };
        let ty = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match ty {
            "message_start" => {
                // 首个 chunk 带 role（OpenAI 约定）
                if self.role_sent {
                    return Vec::new();
                }
                self.role_sent = true;
                self.chunk(json!({ "role": "assistant", "content": "" }), None)
                    .into_iter()
                    .collect()
            }
            "content_block_start" => {
                let idx = int_of(ev.get("index"));
                let cb = ev.get("content_block").and_then(|v| v.as_object());
                let bt = cb
                    .and_then(|m| m.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                // ⚠ Go 侧在这里还写了一个 `blockType[idx] = bt` 的 map，但**全文件
                // 没有任何读取点**（已 grep 确认）—— 纯死代码，故不移植。
                if bt != "tool_use" {
                    // text / thinking 块在 start 时**不发** delta
                    //（Anthropic 的 text 起手是空串，发了等于多发一个空 chunk）
                    return Vec::new();
                }
                let Some(cb) = cb else {
                    return Vec::new();
                };
                self.tool_idx += 1;
                let ti = self.tool_idx;
                self.block_to_tool.insert(idx, ti);
                let name = cb.get("name").and_then(|v| v.as_str()).unwrap_or("");
                self.chunk(
                    json!({
                        "tool_calls": [{
                            "index": ti,
                            "id": str_or(cb.get("id"), &format!("call_{}", rand_id())),
                            "type": "function",
                            "function": {
                                "name": name,
                                // ⚠ Go 的注释说"用 `{` 起手表示 JSON 还没结束"，
                                // 但**代码实际写的是空串**（注释与代码不一致）。
                                // 这里**照代码**：注释是意图，代码是线上行为。
                                "arguments": "",
                            },
                        }],
                    }),
                    None,
                )
                .into_iter()
                .collect()
            }
            "content_block_delta" => {
                let idx = int_of(ev.get("index"));
                let d = ev.get("delta").and_then(|v| v.as_object());
                let dt = d
                    .and_then(|m| m.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let field = |k: &str| -> Option<&str> {
                    d.and_then(|m| m.get(k))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                };
                match dt {
                    "text_delta" => match field("text") {
                        Some(s) => self
                            .chunk(json!({ "content": s }), None)
                            .into_iter()
                            .collect(),
                        None => Vec::new(),
                    },
                    "thinking_delta" => {
                        // Anthropic 的思考增量 → 网关内部的 reasoning_content
                        //（网关的 upstream/sse.rs 就认这个字段名）
                        match field("thinking").or_else(|| field("text")) {
                            Some(s) => self
                                .chunk(json!({ "reasoning_content": s }), None)
                                .into_iter()
                                .collect(),
                            None => Vec::new(),
                        }
                    }
                    "input_json_delta" => {
                        // 工具参数增量：OpenAI 用**字符串拼接**（不完整 JSON 是常态）
                        let Some(ti) = self.block_to_tool.get(&idx).copied() else {
                            return Vec::new();
                        };
                        let Some(partial) = field("partial_json") else {
                            return Vec::new();
                        };
                        self.chunk(
                            json!({
                                "tool_calls": [{
                                    "index": ti,
                                    "function": { "arguments": partial },
                                }],
                            }),
                            None,
                        )
                        .into_iter()
                        .collect()
                    }
                    // 思考块签名，OpenAI 无对应物 —— 丢弃
                    _ => Vec::new(),
                }
            }
            // 无需发 chunk
            "content_block_stop" => Vec::new(),
            "message_delta" => {
                // 这里带 stop_reason 与 usage
                let d = ev.get("delta").and_then(|v| v.as_object());
                let sr = d
                    .and_then(|m| m.get("stop_reason"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let finish = openai_finish_reason(sr);
                let mut out: Vec<Bytes> = Vec::new();
                // usage 一并下发（网关会聚合它做统计）
                let usage = ev.get("usage").and_then(|v| v.as_object()).map(|u| {
                    // Anthropic 的 input_tokens **不含**缓存命中，OpenAI 的
                    // prompt_tokens 含 —— 加起来才是用户感知的"输入量"。
                    let inp = int_of(u.get("input_tokens"));
                    let cr = int_of(u.get("cache_read_input_tokens"));
                    let cw = int_of(u.get("cache_creation_input_tokens"));
                    let outt = int_of(u.get("output_tokens"));
                    let mut u2 = Map::new();
                    u2.insert("prompt_tokens".to_string(), json!(inp + cr + cw));
                    u2.insert("completion_tokens".to_string(), json!(outt));
                    u2.insert("total_tokens".to_string(), json!(inp + cr + cw + outt));
                    if cr > 0 {
                        u2.insert(
                            "prompt_tokens_details".to_string(),
                            json!({ "cached_tokens": cr }),
                        );
                    }
                    Value::Object(u2)
                });
                // finish_reason 与 usage **分两个 chunk** 更稳（有些客户端只读第一个）
                if !finish.is_empty() {
                    if let Some(b) = self.chunk(json!({}), Some(finish)) {
                        out.push(b);
                    }
                }
                if let Some(u) = usage {
                    let obj = json!({
                        "id": self.id,
                        "object": "chat.completion.chunk",
                        "created": self.created,
                        "model": self.model,
                        "choices": [],
                        "usage": u,
                    });
                    if let Ok(s) = serde_json::to_string(&obj) {
                        out.push(Bytes::from(format!("data: {s}\n\n")));
                    }
                }
                out
            }
            "message_stop" => self.done_frame(),
            "error" => {
                // 上游在中途报错：翻成 OpenAI 的 error 形状再结束，
                // 不能让客户端一直等（那样表现为"卡住"）。
                let err_obj = ev.get("error").and_then(|v| v.as_object());
                let msg = err_obj
                    .and_then(|m| m.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let msg = if msg.is_empty() { "上游返回错误" } else { msg };
                let typ = err_obj
                    .and_then(|m| m.get("type"))
                    .map(|v| str_or(Some(v), "upstream_error"))
                    .unwrap_or_else(|| "upstream_error".to_string());
                // Go 是 `map[string]any{"code": errObj["code"]}`：字段缺失时是 nil
                // ⇒ JSON `null`。这里照抄（`null` 在错误对象里无害）。
                let code = err_obj
                    .and_then(|m| m.get("code"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let frame = json!({
                    "error": { "message": msg, "type": typ, "code": code },
                });
                let mut out = Vec::new();
                if let Ok(s) = serde_json::to_string(&frame) {
                    out.push(Bytes::from(format!("data: {s}\n\n")));
                }
                out.extend(self.done_frame());
                out
            }
            // 心跳：忽略（OpenAI 无对应物，透传会让客户端 parse 失败）
            _ => Vec::new(),
        }
    }

    /// 流收尾。上游流意外结束（没发 `message_stop`）时补一个 `[DONE]`，
    /// 否则客户端会一直等 —— 表现为"回答到一半卡住"。
    ///
    /// 已经写过 `[DONE]` 就什么都不做（保证整条流只有一个 `[DONE]`）。
    pub fn finish(&mut self) -> Vec<Bytes> {
        self.done_frame()
    }

    fn done_frame(&mut self) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        vec![Bytes::from_static(b"data: [DONE]\n\n")]
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Option<Bytes> {
        let mut choice = Map::new();
        choice.insert("index".to_string(), json!(0));
        choice.insert("delta".to_string(), delta);
        choice.insert(
            "finish_reason".to_string(),
            match finish {
                Some(f) => Value::String(f.to_string()),
                None => Value::Null,
            },
        );
        let obj = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [Value::Object(choice)],
        });
        let s = serde_json::to_string(&obj).ok()?;
        Some(Bytes::from(format!("data: {s}\n\n")))
    }
}

/// 上游 SSE 单行的长度上限（字节）。
///
/// SSE 单行可能很长（工具参数、长文本）—— 常规 64KB 缓冲会截断，Go 侧因此把
/// `bufio.Scanner` 的上限放到 8MB（一个 `input_json_delta` 理论上是增量，但实测
/// 偶有整块下发）。
const MAX_SSE_LINE: usize = 8 * 1024 * 1024;

/// 把上游的字节流翻成 OpenAI 的 SSE 字节流。
///
/// 产出的 `Stream` 可直接交给 `reqwest::Body::wrap_stream`。⚠ **绝不缓冲整流**：
/// 缓冲会把"首字节延迟"变成"整段生成延迟"。
///
/// 与 Go 的一处**有意偏离**：Go 的 `bufio.Scanner` 遇到超过 8MB 的单行会直接
/// 停止扫描（`ErrTooLong` 未被检查），整条流就此结束、只剩一个 `[DONE]`。
/// 这里改为**丢掉那一行、继续翻译后面的内容** —— 一行超长不该让整段回答消失，
/// 而保留上限的意义（防止畸形上游把内存撑爆）不受影响。
pub fn translate_stream<S, E>(
    upstream: S,
    model: String,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>>
where
    S: futures_util::Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    async_stream::stream! {
        let mut tr = AnthropicStreamTranslator::new(&model);
        let mut pending: Vec<u8> = Vec::new();
        // 超长行丢弃模式：丢掉字节直到下一个换行
        let mut skipping = false;
        pin_mut!(upstream);

        while let Some(item) = upstream.next().await {
            let Ok(chunk) = item else {
                // 上游读取错误：与 Go 的 Scanner 出错即退出循环一致 —— 已翻好的
                // 内容照常下发，收尾补 `[DONE]`。
                break;
            };
            pending.extend_from_slice(&chunk);
            loop {
                if skipping {
                    match pending.iter().position(|&b| b == b'\n') {
                        Some(p) => {
                            pending.drain(..=p);
                            skipping = false;
                            continue;
                        }
                        None => {
                            pending.clear();
                            break;
                        }
                    }
                }
                let Some(p) = pending.iter().position(|&b| b == b'\n') else {
                    if pending.len() > MAX_SSE_LINE {
                        pending.clear();
                        skipping = true;
                    }
                    break;
                };
                let rest = pending.split_off(p + 1);
                let mut raw = std::mem::replace(&mut pending, rest);
                raw.pop(); // 去掉换行符
                // 只对**完整行**做 UTF-8 转换：跨 chunk 切断的多字节字符此时已拼齐。
                // 非法 UTF-8 用 lossy 替换（我们只匹配 ASCII 前缀再交给 JSON 解析）。
                let line = String::from_utf8_lossy(&raw).into_owned();
                for b in tr.push_line(&line) {
                    yield Ok(b);
                }
            }
        }

        // 收尾：最后一行可能没有换行符（Go 的 ScanLines 也会返回最后一个非空行）
        if !skipping && !pending.is_empty() {
            let raw = std::mem::take(&mut pending);
            let line = String::from_utf8_lossy(&raw).into_owned();
            for b in tr.push_line(&line) {
                yield Ok(b);
            }
        }
        for b in tr.finish() {
            yield Ok(b);
        }
    }
}

/// Anthropic 的 `stop_reason` → OpenAI 的 `finish_reason`。
fn openai_finish_reason(sr: &str) -> &'static str {
    match sr {
        "end_turn" | "stop_sequence" => "stop",
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        // 服务端暂停（长任务）—— OpenAI 无等价物，当正常结束处理
        "pause_turn" => "stop",
        _ => "",
    }
}

// ---------------------------------------------------------------------------
// 方向三：Anthropic 非流式响应 → OpenAI 非流式响应
// ---------------------------------------------------------------------------

/// 把 Anthropic 的 message 对象翻成 OpenAI 的 `chat.completion`。
///
/// ⚠ **本仓无生产调用点**（Go 侧同样只有 `translate_test.go` 调它）：对话请求
/// 被强制流式（见 `client.rs` 的 `force_stream`），所以热路径永远是流式那条。
/// 保留它是为了幂等兜底与将来接线，**不要在热路径上误以为它在跑**。
pub fn translate_non_stream(body: &[u8], model: &str) -> Result<Vec<u8>, String> {
    let Ok(in_obj) = serde_json::from_slice::<Value>(body) else {
        return Err("解析 Anthropic 响应失败: 响应体不是合法 JSON".to_string());
    };
    let Some(in_map) = in_obj.as_object() else {
        return Err("解析 Anthropic 响应失败: 响应体不是 JSON 对象".to_string());
    };
    // 已经是 OpenAI 形状就别动（幂等，便于重放/测试）
    if in_map.contains_key("choices") {
        return Ok(body.to_vec());
    }

    let mut msg = Map::new();
    msg.insert("role".to_string(), json!("assistant"));
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    if let Some(blocks) = in_map.get("content").and_then(|v| v.as_array()) {
        for b in blocks {
            let Some(bm) = b.as_object() else {
                continue;
            };
            match str_or(bm.get("type"), "") .as_str() {
                "text" => text.push_str(&str_or(bm.get("text"), "")),
                "thinking" => reasoning.push_str(&str_or(bm.get("thinking"), "")),
                "tool_use" => {
                    let args = serde_json::to_string(bm.get("input").unwrap_or(&Value::Null))
                        .unwrap_or_else(|_| "null".to_string());
                    tool_calls.push(json!({
                        "id": str_or(bm.get("id"), &format!("call_{}", rand_id())),
                        "type": "function",
                        "function": {
                            "name": str_or(bm.get("name"), ""),
                            "arguments": args,
                        },
                    }));
                }
                _ => {}
            }
        }
    }
    msg.insert("content".to_string(), Value::String(text));
    if !reasoning.is_empty() {
        msg.insert("reasoning_content".to_string(), Value::String(reasoning));
    }
    if !tool_calls.is_empty() {
        msg.insert("tool_calls".to_string(), Value::Array(tool_calls.clone()));
    }

    let mut finish = openai_finish_reason(&str_or(in_map.get("stop_reason"), ""));
    if finish.is_empty() {
        finish = if tool_calls.is_empty() { "stop" } else { "tool_calls" };
    }

    let mut out = Map::new();
    out.insert(
        "id".to_string(),
        Value::String(str_or(in_map.get("id"), &format!("chatcmpl-{}", rand_id()))),
    );
    out.insert("object".to_string(), json!("chat.completion"));
    out.insert("created".to_string(), json!(now_unix()));
    out.insert(
        "model".to_string(),
        Value::String(first_non_empty_str(model, &str_or(in_map.get("model"), ""))),
    );
    out.insert(
        "choices".to_string(),
        json!([{
            "index": 0,
            "message": Value::Object(msg),
            "finish_reason": finish,
        }]),
    );

    if let Some(u) = in_map.get("usage").and_then(|v| v.as_object()) {
        let inp = int_of(u.get("input_tokens"));
        let cr = int_of(u.get("cache_read_input_tokens"));
        let cw = int_of(u.get("cache_creation_input_tokens"));
        let outt = int_of(u.get("output_tokens"));
        out.insert(
            "usage".to_string(),
            json!({
                "prompt_tokens": inp + cr + cw,
                "completion_tokens": outt,
                "total_tokens": inp + cr + cw + outt,
            }),
        );
    }

    serde_json::to_vec(&out).map_err(|e| format!("序列化 OpenAI 响应失败: {e}"))
}

/// 判断响应体是否是 Anthropic 形状（用于错误分支的识别）。
pub fn is_anthropic_body(body: &[u8]) -> bool {
    let s = String::from_utf8_lossy(body);
    let s = s.trim();
    s.contains("\"type\":\"error\"") || s.contains("\"type\": \"error\"")
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// `strOr`：字符串且**非空**才用，否则回落默认值。
fn str_or(v: Option<&Value>, def: &str) -> String {
    match v {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => def.to_string(),
    }
}

/// 生成一个短随机 id（用于 `chatcmpl-` / `call_` / `toolu_` 前缀）。
///
/// 复用 `new_device_mid` 的随机源思路但不带连字符 —— OpenAI 的 id 形态是
/// `chatcmpl-<无连字符>`，Anthropic 的 tool id 是 `toolu_<…>`；两者都**不校验
/// 格式**（与 X-Device-Mid 不同），故短 id 足够。
fn rand_id() -> String {
    let mut b = [0u8; 12];
    if rand::rngs::OsRng.try_fill_bytes(&mut b).is_err() {
        // 随机源失败时回落到时间戳 —— **不能返回空串**（空 id 会让客户端把多个
        // 响应当成同一个）。
        return now_unix_nanos().to_string();
    }
    hex::encode(b)
}

/// 当前 Unix 秒（OpenAI 的 `created` 字段）。
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 当前 Unix 纳秒（`rand_id` 的失败兜底，正常路径走不到）。
fn now_unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// `firstNonEmptyStr`：`a` trim 后非空才用 `a`。
fn first_non_empty_str(a: &str, b: &str) -> String {
    if !a.trim().is_empty() {
        a.to_string()
    } else {
        b.to_string()
    }
}

/// `intOf`：把 JSON 值转 `i64`，**取不到就是 0**。
///
/// ⚠ Go 侧那条注释在这里同样成立：`encoding/json` 解到 `any` 时数字**一律**是
/// float64，用 int 断言会**静默拿到 0** —— 那会让 usage 全变 0、index 全变 0，
/// 且不报错。Rust 的 `serde_json` 会把整数解成 `i64`/`u64`、小数解成 `f64`，
/// 故三条路径都要吃（`as_f64` 兜住小数与超出 i64 的整数）。
fn int_of(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => {
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

/// `numOrZero`：JSON 数字是否有效（`max_tokens` 用）。
fn num_or_zero(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64(),
        _ => None,
    }
}

/// 把 f64 还原成 Go `json.Marshal(float64)` 会产出的数字形态。
///
/// ⚠ 不能直接用 `Value::from(f64)`：`serde_json` 会把 `128000.0` 写成
/// `128000.0`，而 Go 写的是 `128000`。`max_tokens` 在上游是**整数**语义
///（Anthropic 协议要求），带小数点是否被接受没有实测证据 ⇒ 整数值一律发整数。
fn json_number_from_f64(v: f64) -> Value {
    if v.is_finite() && v.fract() == 0.0 && v.abs() <= 9_007_199_254_740_992.0 {
        Value::Number((v as i64).into())
    } else {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

/// Go `fmt.Sprint` 在标量上的等价物（只用于"未知类型的 content"这一兜底分支）。
fn go_sprint(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => "<nil>".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(body: &str, model: &str) -> Value {
        let out = build_anthropic_body(body.as_bytes(), model).expect("应能翻译");
        serde_json::from_slice(&out).expect("输出应是合法 JSON")
    }

    fn frames(chunks: &[Bytes]) -> String {
        chunks
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect()
    }

    /// 把一批 SSE 行喂给内核，返回拼起来的全部输出。
    fn run(lines: &[&str]) -> String {
        let mut tr = AnthropicStreamTranslator::new("glm-5.3");
        let mut out = String::new();
        for l in lines {
            out.push_str(&frames(&tr.push_line(l)));
        }
        out.push_str(&frames(&tr.finish()));
        out
    }

    /// 输出里的**内容帧**（不含收尾的 `data: [DONE]`）。
    ///
    /// ⚠ 别直接数 `out.matches("data: ")` —— `run()` 末尾总会补一个 `[DONE]`
    /// （`translate_stream` 也是这么收尾的），把它算进去会让帧数永远多 1。
    fn data_frames(out: &str) -> Vec<&str> {
        out.lines()
            .filter(|l| l.starts_with("data: ") && *l != "data: [DONE]")
            .collect()
    }

    /// 取第 `i` 个内容帧的 JSON。
    fn frame_json(out: &str, i: usize) -> Value {
        let f = data_frames(out)[i];
        serde_json::from_str(f.trim_start_matches("data: "))
            .unwrap_or_else(|e| panic!("帧 {i} 不是合法 JSON: {e}；实际 {f}"))
    }

    // ---- 方向一：请求体 ----

    #[test]
    fn system_goes_to_top_level_and_official_block_comes_first() {
        // 与 Go `TestBuildAnthropicSystemGoesToTopLevel` 同一契约（且它记着旧契约
        // 已被**反转**：当时断言块数 == 2，注入官方块后必然失败 —— 那是断言过时）。
        let out = build(
            r#"{"messages":[
                {"role":"system","content":"你是助手"},
                {"role":"user","content":"hi"},
                {"role":"system","content":"补充规则"}
            ]}"#,
            "glm-5.3",
        );
        let sys = out["system"].as_array().expect("system 应提到顶层");
        assert!(sys.len() >= 2, "至少要有客户端那 2 块，实际 {sys:?}");
        // 官方块在**最前**
        assert!(
            sys[0]["text"].as_str().unwrap_or("").contains("You are ZCode"),
            "首块必须是官方 cliPrefix，实际 {:?}",
            sys[0]
        );
        // 客户端两块都还在且顺序不变
        let texts: Vec<String> = sys[3..]
            .iter()
            .map(|b| b["text"].as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(texts, vec!["你是助手", "补充规则"]);
        // messages 里不应再有 system 角色
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn max_tokens_is_always_present() {
        let out = build(r#"{"messages":[{"role":"user","content":"hi"}]}"#, "m");
        assert_eq!(out["max_tokens"], json!(DEFAULT_MAX_TOKENS));
        // 传入的保留
        let out2 = build(
            r#"{"max_tokens":123,"messages":[{"role":"user","content":"hi"}]}"#,
            "m",
        );
        assert_eq!(out2["max_tokens"], json!(123));
        // 0 回落
        let out3 = build(
            r#"{"max_tokens":0,"messages":[{"role":"user","content":"hi"}]}"#,
            "m",
        );
        assert_eq!(out3["max_tokens"], json!(DEFAULT_MAX_TOKENS));
        // ★ 必须是整数形态（不能是 128000.0）
        let s = String::from_utf8(build_anthropic_body(
            br#"{"messages":[{"role":"user","content":"hi"}]}"#,
            "m",
        )
        .unwrap())
        .unwrap();
        assert!(s.contains(r#""max_tokens":128000"#), "实际 {s}");
    }

    #[test]
    fn tool_calls_become_tool_use_blocks() {
        let out = build(
            r#"{"messages":[{"role":"assistant","content":null,"tool_calls":[
                {"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"北京\"}"}}
            ]}]}"#,
            "m",
        );
        let blocks = out["messages"][0]["content"].as_array().unwrap();
        // ⚠ `content: null` 走的是 `openai_content_to_anthropic` 的 nil 分支，
        // 那个分支**已经**给了一个空文本块（Go 的 `case nil:` 同样如此）——
        // 于是这里是 2 块，不是"只有 tool_use"。Go 那句 `if len(blocks)==0`
        // 补空文本块的兜底，只在 content 完全缺失**且没有 tool_calls** 时才触发。
        assert_eq!(blocks.len(), 2, "实际 {blocks:?}");
        assert_eq!(blocks[0], json!({"type":"text","text":""}));
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["id"], "call_1");
        assert_eq!(blocks[1]["name"], "get_weather");
        assert_eq!(blocks[1]["input"]["city"], "北京");
    }

    #[test]
    fn broken_tool_arguments_are_kept_under_raw() {
        // 丢弃会让模型看到"调用了但没参数"，症状更怪
        let out = build(
            r#"{"messages":[{"role":"assistant","tool_calls":[
                {"id":"c1","type":"function","function":{"name":"f","arguments":"{不是 JSON"}}
            ]}]}"#,
            "m",
        );
        let blocks = out["messages"][0]["content"].as_array().unwrap();
        // 同前一条：content 缺失 ⇒ 先有一个空文本块兜底，tool_use 在其后。
        assert_eq!(blocks.len(), 2, "实际 {blocks:?}");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["input"]["__raw"], "{不是 JSON");
    }

    #[test]
    fn assistant_with_only_tool_calls_gets_a_text_block() {
        // Anthropic 不接受 content 为空数组
        let out = build(
            r#"{"messages":[{"role":"assistant","content":[],"tool_calls":[
                {"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}
            ]}]}"#,
            "m",
        );
        let blocks = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], json!({"type":"text","text":""}));
        assert_eq!(blocks[1]["type"], "tool_use");
    }

    #[test]
    fn tool_role_becomes_user_tool_result() {
        // Anthropic 没有 `tool` 角色
        let out = build(
            r#"{"messages":[
                {"role":"user","content":"hi"},
                {"role":"tool","tool_call_id":"c1","content":[{"type":"text","text":"晴"}]}
            ]}"#,
            "m",
        );
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[1]["content"][0]["tool_use_id"], "c1");
        assert_eq!(msgs[1]["content"][0]["content"], "晴");
    }

    #[test]
    fn unknown_roles_are_kept_as_user() {
        let out = build(
            r#"{"messages":[{"role":"weird","content":"x"}]}"#,
            "m",
        );
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"][0]["text"], "x");
    }

    #[test]
    fn only_system_messages_is_an_error() {
        let e = build_anthropic_body(
            br#"{"messages":[{"role":"system","content":"only system"}]}"#,
            "m",
        )
        .unwrap_err();
        assert_eq!(e, "请求里没有可用的消息");
    }

    #[test]
    fn images_url_and_data_uri_become_anthropic_sources() {
        let out = build(
            r#"{"messages":[{"role":"user","content":[
                {"type":"text","text":"看图"},
                {"type":"image_url","image_url":{"url":"https://x/y.png"}},
                {"type":"image_url","image_url":{"url":"data:image/jpeg;base64,AAAA"}},
                {"type":"image_url","image_url":{"url":"data:broken"}}
            ]}]}"#,
            "m",
        );
        let blocks = out["messages"][0]["content"].as_array().unwrap();
        // 第三块是坏 data: URI ⇒ 丢弃
        assert_eq!(blocks.len(), 3, "实际 {blocks:?}");
        assert_eq!(blocks[1]["source"]["type"], "url");
        assert_eq!(blocks[1]["source"]["url"], "https://x/y.png");
        assert_eq!(blocks[2]["source"]["type"], "base64");
        assert_eq!(blocks[2]["source"]["media_type"], "image/jpeg");
        assert_eq!(blocks[2]["source"]["data"], "AAAA");
    }

    #[test]
    fn stop_string_and_array_become_stop_sequences() {
        let a = build(
            r#"{"messages":[{"role":"user","content":"h"}],"stop":"END"}"#,
            "m",
        );
        assert_eq!(a["stop_sequences"], json!(["END"]));
        let b = build(
            r#"{"messages":[{"role":"user","content":"h"}],"stop":["A","B"]}"#,
            "m",
        );
        assert_eq!(b["stop_sequences"], json!(["A", "B"]));
        // 空串 / 空数组都不发
        let c = build(
            r#"{"messages":[{"role":"user","content":"h"}],"stop":""}"#,
            "m",
        );
        assert!(c.get("stop_sequences").is_none());
    }

    #[test]
    fn tools_get_input_schema_even_without_parameters() {
        // input_schema 必填，缺了上游会 400
        let out = build(
            r#"{"messages":[{"role":"user","content":"h"}],"tools":[
                {"type":"function","function":{"name":"f"}},
                {"type":"function","function":{"name":"g","description":"d","parameters":{"type":"object"}}},
                {"type":"function","function":{"name":""}},
                {"name":"already","input_schema":{"type":"object"}}
            ],"tool_choice":"required"}"#,
            "m",
        );
        let tools = out["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3, "实际 {tools:?}");
        assert_eq!(tools[0]["input_schema"], json!({"type":"object","properties":{}}));
        assert!(tools[0].get("description").is_none());
        assert_eq!(tools[1]["input_schema"], json!({"type":"object"}));
        assert_eq!(tools[1]["description"], "d");
        // 已是 Anthropic 形状的原样保留
        assert_eq!(tools[2]["name"], "already");
        assert_eq!(out["tool_choice"], json!({"type":"any"}));
    }

    #[test]
    fn tool_choice_none_is_omitted_not_sent_as_null() {
        let out = build(
            r#"{"messages":[{"role":"user","content":"h"}],
                "tools":[{"type":"function","function":{"name":"f"}}],
                "tool_choice":"none"}"#,
            "m",
        );
        assert!(out.get("tools").is_some());
        // ⚠ 与 Go 的有意偏离：Go 发 `"tool_choice": null`，这里不发（见函数注释）
        assert!(
            out.get("tool_choice").is_none(),
            "实际 {:?}",
            out.get("tool_choice")
        );
    }

    #[test]
    fn tool_choice_function_form_becomes_tool_type() {
        let out = build(
            r#"{"messages":[{"role":"user","content":"h"}],
                "tools":[{"type":"function","function":{"name":"f"}}],
                "tool_choice":{"type":"function","function":{"name":"get_weather"}}}"#,
            "m",
        );
        assert_eq!(out["tool_choice"], json!({"type":"tool","name":"get_weather"}));
    }

    #[test]
    fn reasoning_effort_maps_to_thinking_and_output_config() {
        let cases = [
            ("low", "low"),
            ("minimal", "low"),
            ("none", "low"),
            ("medium", "high"), // 上游只认 low/high/max，medium 取中间值
            ("mid", "high"),
            ("high", "high"),
            ("max", "max"),
            ("xhigh", "max"),
            ("HIGH", "high"),
        ];
        for (input, want) in cases {
            let out = build(
                &format!(
                    r#"{{"messages":[{{"role":"user","content":"h"}}],"reasoning_effort":"{input}"}}"#
                ),
                "m",
            );
            assert_eq!(out["thinking"], json!({"type":"enabled"}), "输入 {input}");
            assert_eq!(out["output_config"], json!({"effort": want}), "输入 {input}");
        }
        // 认不出的档位：两个字段都不发（就近映射而不是拒绝，但确实映射不了就不发）
        let out = build(
            r#"{"messages":[{"role":"user","content":"h"}],"reasoning_effort":"whatever"}"#,
            "m",
        );
        assert!(out.get("thinking").is_none());
        // `reasoning: {effort: …}` 形态
        let out2 = build(
            r#"{"messages":[{"role":"user","content":"h"}],"reasoning":{"effort":"max"}}"#,
            "m",
        );
        assert_eq!(out2["output_config"], json!({"effort":"max"}));
    }

    #[test]
    fn metadata_is_double_encoded() {
        let out = serde_json::from_slice::<Value>(
            &build_anthropic_body_with_meta(
                br#"{"messages":[{"role":"user","content":"h"}]}"#,
                "m",
                &AnthropicMeta {
                    device_id: "dev-1".into(),
                    account_uuid: String::new(),
                    session_id: "sess-1".into(),
                },
            )
            .unwrap(),
        )
        .unwrap();
        // user_id 的值本身是一个 JSON 字符串
        let inner = out["metadata"]["user_id"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(inner).unwrap();
        assert_eq!(parsed["device_id"], "dev-1");
        assert_eq!(parsed["session_id"], "sess-1");
        assert_eq!(parsed["account_uuid"], "");
        // 全空 ⇒ 不发 metadata
        let out2 = build(r#"{"messages":[{"role":"user","content":"h"}]}"#, "m");
        assert!(out2.get("metadata").is_none());
    }

    #[test]
    fn sampling_params_and_stream_pass_through() {
        let out = build(
            r#"{"messages":[{"role":"user","content":"h"}],
                "temperature":0.7,"top_p":0.9,"stream":true}"#,
            "m",
        );
        assert_eq!(out["temperature"], json!(0.7));
        assert_eq!(out["top_p"], json!(0.9));
        assert_eq!(out["stream"], json!(true));
    }

    // ---- 方向二：流式 ----

    #[test]
    fn stream_emits_role_then_content_then_finish_then_usage_then_done() {
        let out = run(&[
            r#"event: message_start"#,
            r#"data: {"type":"message_start"}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"你好"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":10,"cache_read_input_tokens":5,"cache_creation_input_tokens":2,"output_tokens":7}}"#,
            r#"data: {"type":"message_stop"}"#,
        ]);
        let lines: Vec<&str> = out.lines().filter(|l| l.starts_with("data: ")).collect();
        // 1) role 起手
        assert!(lines[0].contains(r#""role":"assistant""#), "实际 {}", lines[0]);
        // 2) 内容增量
        assert!(lines[1].contains(r#""content":"你好""#), "实际 {}", lines[1]);
        // text 块 start 时**不发** delta ⇒ 内容帧紧跟在 role 之后
        // 3) finish_reason 单独一帧
        assert!(lines[2].contains(r#""finish_reason":"stop""#), "实际 {}", lines[2]);
        // 4) usage 单独一帧，choices 为空
        assert!(lines[3].contains(r#""choices":[]"#), "实际 {}", lines[3]);
        assert!(lines[3].contains(r#""prompt_tokens":17"#), "实际 {}", lines[3]);
        assert!(lines[3].contains(r#""completion_tokens":7"#));
        assert!(lines[3].contains(r#""total_tokens":24"#));
        assert!(lines[3].contains(r#""cached_tokens":5"#));
        // 5) 结束标记
        assert_eq!(lines[4], "data: [DONE]");
        assert_eq!(lines.len(), 5, "实际 {lines:?}");
    }

    #[test]
    fn stream_role_is_sent_only_once() {
        let out = run(&[
            r#"data: {"type":"message_start"}"#,
            r#"data: {"type":"message_start"}"#,
        ]);
        assert_eq!(out.matches(r#""role":"assistant""#).count(), 1);
    }

    #[test]
    fn stream_thinking_delta_becomes_reasoning_content() {
        let out = run(&[
            r#"data: {"type":"message_start"}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"想一下"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"xx"}}"#,
        ]);
        assert!(out.contains(r#""reasoning_content":"想一下""#), "实际 {out}");
        // 签名增量被丢弃
        assert!(!out.contains("xx"));
        // thinking 块 start 时不发 delta ⇒ 只有 role + 一条思考
        assert_eq!(data_frames(&out).len(), 2, "实际 {out}");
    }

    #[test]
    fn stream_tool_use_index_is_separate_from_block_index() {
        // text 块占 index 0，tool_use 在 index 1 ⇒ tool_calls 序号必须是 0
        let out = run(&[
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather"}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"ci"}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"ty\":\"bj\"}"}}"#,
        ]);
        let lines = data_frames(&out);
        assert_eq!(lines.len(), 3, "实际 {out}");
        // 起手帧：tool_calls[0] 的 index 是 **tool_calls 数组下标**（0），
        // 而不是 Anthropic 的 content block index（1）。
        let f0 = frame_json(&out, 0);
        assert_eq!(f0["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        assert_eq!(f0["choices"][0]["delta"]["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(
            f0["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
            "get_weather"
        );
        // ⚠ 照 Go 的**代码**（不是它那条"用 { 起手"的注释）：arguments 从空串起手
        assert_eq!(
            f0["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            ""
        );
        // 参数增量：**字符串拼接**语义，故 `{"ci` + `ty":"bj"}` 拼回完整 JSON。
        // ⚠ 别用 `contains("{\"ci")` 去断言 —— 那是**转义后**的文本，JSON 里是 `{\"ci`。
        // 解析成 Value 再比，才是在测语义而不是测转义。
        let f1 = frame_json(&out, 1);
        assert_eq!(
            f1["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            r#"{"ci"#
        );
        let f2 = frame_json(&out, 2);
        assert_eq!(
            f2["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            r#"ty":"bj"}"#
        );
        // 参数增量不带 id/type/name（OpenAI 的拼接约定）
        let tc1 = f1["choices"][0]["delta"]["tool_calls"][0].as_object().unwrap();
        assert!(!tc1.contains_key("id"), "增量不该重复带 id");
        assert!(!tc1.contains_key("type"));
        assert_eq!(
            tc1["function"].as_object().unwrap().len(),
            1,
            "增量只该带 arguments"
        );
    }

    #[test]
    fn stream_skips_broken_events_without_killing_the_stream() {
        let out = run(&[
            r#"data: {"type":"message_start"}"#,
            r#"data: {这不是 JSON"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"还在"}}"#,
            r#"data: [DONE]"#,
            r#"event: ping"#,
            r#"data: {"type":"message_stop"}"#,
        ]);
        assert!(out.contains("还在"));
        assert!(out.ends_with("data: [DONE]\n\n"));
        // 只有一个 [DONE]（message_stop 之后 finish() 不重复发）
        assert_eq!(out.matches("[DONE]").count(), 1);
    }

    #[test]
    fn stream_appends_done_when_upstream_ends_early() {
        // 上游没发 message_stop 就断了 —— 不补 [DONE] 客户端会一直等
        let out = run(&[
            r#"data: {"type":"message_start"}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"半句"}}"#,
        ]);
        assert!(out.ends_with("data: [DONE]\n\n"), "实际 {out}");
    }

    #[test]
    fn stream_error_event_is_translated_then_done() {
        let out = run(&[
            r#"data: {"type":"message_start"}"#,
            r#"data: {"type":"error","error":{"type":"overloaded_error","message":"上游忙","code":529}}"#,
        ]);
        // ⚠ 断言必须**解析后**比字段：`serde_json::Map` 默认是 BTreeMap，
        // 序列化时键按**字典序**（code, message, type），与插入顺序无关。
        // Go 的 `map[string]any` 同样按字典序 marshal —— 两边行为一致，
        // 是**断言写错了顺序**，不是实现错了。
        let err = frame_json(&out, 1);
        assert_eq!(err["error"]["message"], "上游忙");
        assert_eq!(err["error"]["type"], "overloaded_error");
        assert_eq!(err["error"]["code"], 529);
        // 报错帧之后立刻收尾，不让客户端一直等
        assert_eq!(data_frames(&out).len(), 2, "实际 {out}");
        assert!(out.ends_with("data: [DONE]\n\n"));
        // 已经写过 [DONE] ⇒ finish() 不再补
        let mut tr = AnthropicStreamTranslator::new("m");
        let _ = tr.push_line(r#"data: {"type":"error","error":{}}"#);
        assert!(tr.is_finished());
        assert!(tr.finish().is_empty());
    }

    #[test]
    fn stream_error_without_message_falls_back() {
        let out = run(&[r#"data: {"type":"error","error":{}}"#]);
        assert!(out.contains("上游返回错误"), "实际 {out}");
        // code 缺失 ⇒ null（与 Go 的 nil 一致）
        assert!(out.contains(r#""code":null"#), "实际 {out}");
    }

    #[test]
    fn stream_ignores_non_data_lines_and_done_payloads() {
        let out = run(&[
            r#"event: content_block_delta"#,
            "",
            r#"data: "#,
            r#"data: [DONE]"#,
        ]);
        // 除收尾的 [DONE] 外没有任何帧
        assert_eq!(out, "data: [DONE]\n\n");
    }

    #[test]
    fn stream_usage_chunk_is_omitted_when_usage_is_absent() {
        let out = run(&[
            r#"data: {"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#,
        ]);
        // message_delta 只带 stop_reason、没有 usage ⇒ 只发 finish_reason 一帧
        //（usage 缺失时不发空 usage 帧 —— 那会让统计多算一次）
        assert_eq!(data_frames(&out).len(), 1, "实际 {out}");
        assert_eq!(frame_json(&out, 0)["choices"][0]["finish_reason"], "length");
        assert!(!out.contains("prompt_tokens"));
    }

    #[test]
    fn stream_finish_reason_mapping_is_complete() {
        assert_eq!(openai_finish_reason("end_turn"), "stop");
        assert_eq!(openai_finish_reason("stop_sequence"), "stop");
        assert_eq!(openai_finish_reason("max_tokens"), "length");
        assert_eq!(openai_finish_reason("tool_use"), "tool_calls");
        assert_eq!(openai_finish_reason("refusal"), "content_filter");
        // 服务端暂停：OpenAI 无等价物，当正常结束
        assert_eq!(openai_finish_reason("pause_turn"), "stop");
        // 认不出 ⇒ 不发 finish_reason（"stream_usage_chunk…" 里已验证空值路径）
        assert_eq!(openai_finish_reason("whatever"), "");
    }

    #[tokio::test]
    async fn byte_stream_translates_across_chunk_boundaries() {
        // 故意在**多字节字符中间**切断，验证行缓冲不会把中文切坏
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\"}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好世界\"}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let bytes = sse.as_bytes().to_vec();
        let mid = bytes.len() - 20; // 落在最后一段里
        let chunks = vec![
            Ok::<Bytes, std::io::Error>(Bytes::copy_from_slice(&bytes[..mid])),
            Ok(Bytes::copy_from_slice(&bytes[mid..])),
        ];
        let stream = translate_stream(futures_util::stream::iter(chunks), "glm-5.3".into());
        pin_mut!(stream);
        let mut got = String::new();
        while let Some(item) = stream.next().await {
            got.push_str(&String::from_utf8_lossy(&item.expect("流项不应报错")));
        }
        assert!(got.contains("\"content\":\"你好世界\""), "实际 {got}");
        assert!(got.ends_with("data: [DONE]\n\n"), "实际 {got}");
    }

    #[tokio::test]
    async fn byte_stream_handles_missing_trailing_newline() {
        // 最后一行没有换行符（Go 的 ScanLines 也会返回它）
        let sse = "data: {\"type\":\"message_stop\"}";
        let chunks = vec![Ok::<Bytes, std::io::Error>(Bytes::from(sse))];
        let stream = translate_stream(futures_util::stream::iter(chunks), "m".into());
        pin_mut!(stream);
        let mut got = String::new();
        while let Some(item) = stream.next().await {
            got.push_str(&String::from_utf8_lossy(&item.expect("流项不应报错")));
        }
        assert_eq!(got, "data: [DONE]\n\n");
    }

    // ---- 方向三：非流式 ----

    #[test]
    fn non_stream_translates_anthropic_message() {
        let out = translate_non_stream(
            r#"{"id":"msg_1","model":"glm-5.3","stop_reason":"tool_use",
                 "content":[{"type":"text","text":"我来查"},
                            {"type":"thinking","thinking":"想想"},
                            {"type":"tool_use","id":"toolu_1","name":"f","input":{"a":1}}],
                 "usage":{"input_tokens":3,"cache_read_input_tokens":4,"cache_creation_input_tokens":5,"output_tokens":6}}"#
                .as_bytes(),
            "glm-5.3",
        )
        .unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["object"], "chat.completion");
        assert_eq!(v["id"], "msg_1");
        let m = &v["choices"][0]["message"];
        assert_eq!(m["content"], "我来查");
        assert_eq!(m["reasoning_content"], "想想");
        assert_eq!(m["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(m["tool_calls"][0]["function"]["arguments"], "{\"a\":1}");
        assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(v["usage"]["prompt_tokens"], 12);
        assert_eq!(v["usage"]["total_tokens"], 18);
    }

    #[test]
    fn non_stream_is_idempotent_for_openai_shape() {
        let body = br#"{"choices":[{"index":0,"message":{"role":"assistant","content":"x"}}]}"#;
        let out = translate_non_stream(body, "m").unwrap();
        assert_eq!(out, body.to_vec());
    }

    #[test]
    fn tool_call_representation_round_trips_both_directions() {
        // ★ 往返一致性（验收项 3）：两个方向必须用**同一套**表示，否则
        // 「模型请求调工具 ⇒ 我们把参数交给上游 ⇒ 上游回结果 ⇒ 我们交回客户端」
        // 这条链会在中间某一跳悄悄变形，而两端各自的单测都是绿的。
        //
        // 方向一：OpenAI 的 `tool_calls[].function.arguments`（**字符串**）⇒
        //         Anthropic 的 `tool_use.input`（**对象**）。
        let openai_req = r#"{"messages":[{"role":"assistant","content":"我来查","tool_calls":[
            {"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"北京\",\"n\":2}"}}
        ]}]}"#;
        let anth = build(openai_req, "m");
        let blocks = anth["messages"][0]["content"].as_array().unwrap();
        let tu = blocks
            .iter()
            .find(|b| b["type"] == "tool_use")
            .expect("必须产出 tool_use 块");
        assert_eq!(tu["id"], "call_1", "id 必须原样带过去（客户端要靠它配对）");
        assert_eq!(tu["name"], "get_weather");
        assert_eq!(
            tu["input"],
            json!({"city": "北京", "n": 2}),
            "arguments 字符串必须被解析成对象"
        );

        // 方向二：把上游回的**同一个** tool_use 块翻回 OpenAI ⇒
        //         必须回到 `arguments` 是**字符串**、`type` 是 `function` 的形状。
        let anth_resp = serde_json::to_vec(&json!({
            "id": "msg_1",
            "model": "m",
            "stop_reason": "tool_use",
            "content": [tu.clone()],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }))
        .unwrap();
        let back: Value =
            serde_json::from_slice(&translate_non_stream(&anth_resp, "m").unwrap()).unwrap();
        let call = &back["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["id"], "call_1", "★ 往返后 id 必须不变（否则配对断链）");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "get_weather");
        let args = call["function"]["arguments"]
            .as_str()
            .expect("★ OpenAI 侧 arguments 必须是**字符串**，不是对象");
        let parsed: Value = serde_json::from_str(args).expect("必须是合法 JSON 字符串");
        assert_eq!(parsed, json!({"city": "北京", "n": 2}), "参数必须逐字保住");
        assert_eq!(back["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn text_content_round_trips_both_directions() {
        // 同上，文本这一路：OpenAI 的 `content`（字符串）⇒ Anthropic 的
        // `content`（块数组）⇒ 回到字符串。
        let anth = build(
            r#"{"messages":[{"role":"user","content":"你好"},{"role":"assistant","content":"在"}]}"#,
            "m",
        );
        let msgs = anth["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["content"], json!([{"type":"text","text":"你好"}]));
        assert_eq!(msgs[1]["content"], json!([{"type":"text","text":"在"}]));

        let anth_resp = serde_json::to_vec(&json!({
            "id": "msg_2", "model": "m", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "在"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }))
        .unwrap();
        let back: Value =
            serde_json::from_slice(&translate_non_stream(&anth_resp, "m").unwrap()).unwrap();
        assert_eq!(back["choices"][0]["message"]["content"], "在");
        assert_eq!(back["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn non_stream_falls_back_to_stop_and_rejects_garbage() {
        let out = translate_non_stream(br#"{"content":[{"type":"text","text":"x"}]}"#, "m").unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        assert!(v["choices"][0]["message"].get("tool_calls").is_none());
        assert!(translate_non_stream(b"not json", "m").is_err());
    }

    #[test]
    fn is_anthropic_body_detects_both_spellings() {
        assert!(is_anthropic_body(br#"  {"type":"error","error":{}}"#));
        assert!(is_anthropic_body(br#"{"type": "error"}"#));
        assert!(!is_anthropic_body(br#"{"error":{"message":"x"}}"#));
    }

    // ---- 小工具 ----

    #[test]
    fn helpers_match_go_semantics() {
        // strOr：非空字符串才用
        assert_eq!(str_or(Some(&json!("a")), "d"), "a");
        assert_eq!(str_or(Some(&json!("")), "d"), "d");
        assert_eq!(str_or(Some(&json!(1)), "d"), "d");
        assert_eq!(str_or(None, "d"), "d");
        // intOf：取不到就是 0（不能静默丢数字）
        assert_eq!(int_of(Some(&json!(7))), 7);
        assert_eq!(int_of(Some(&json!(7.9))), 7);
        assert_eq!(int_of(Some(&json!("7"))), 0);
        assert_eq!(int_of(None), 0);
        // numOrZero
        assert_eq!(num_or_zero(Some(&json!(1.5))), Some(1.5));
        assert_eq!(num_or_zero(Some(&json!("1.5"))), None);
        // contentToText
        assert_eq!(content_to_text(Some(&json!("a"))), "a");
        assert_eq!(
            content_to_text(Some(&json!([{"type":"text","text":"a"},"b",3]))),
            "ab"
        );
        assert_eq!(content_to_text(Some(&json!(7))), "");
        // randID 不能返回空串（空 id 会让客户端把多个响应当同一个）
        assert!(!rand_id().is_empty());
        assert_eq!(rand_id().len(), 24);
        // firstNonEmptyStr
        assert_eq!(first_non_empty_str(" a ", "b"), " a ");
        assert_eq!(first_non_empty_str("  ", "b"), "b");
        // go_sprint 兜住非字符串 content
        assert_eq!(
            openai_content_to_anthropic(Some(&json!(12)))[0]["text"],
            "12"
        );
    }

    #[test]
    fn body_that_is_not_an_object_is_rejected() {
        assert!(build_anthropic_body(b"[1,2]", "m").is_err());
        // `null` 在 Go 里不报错，而是走"没有可用的消息"
        assert_eq!(
            build_anthropic_body(b"null", "m").unwrap_err(),
            "请求里没有可用的消息"
        );
    }
}
