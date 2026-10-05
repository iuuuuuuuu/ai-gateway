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

/// 构造 Qoder agent 请求体。
///
/// `model_key` 必须是**上游 key**（不是客户端看到的显示名）。
pub(crate) fn build_agent_body(openai_body: &[u8], model_key: &str) -> Result<Vec<u8>, String> {
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

    let mut out = agent_body(&messages, model_key)?;

    // tools 仅在客户端**显式**传入时注入（模板 tools 会让 prompt_tokens 从 ~60 涨到 ~10K）
    if let Some(tools) = req.get("tools").and_then(|v| v.as_array()) {
        if !tools.is_empty() {
            let mut m: Map<String, Value> = serde_json::from_slice(&out)
                .map_err(|e| format!("解析 OpenAI 请求体失败: {e}"))?;
            m.insert("tools".to_string(), Value::Array(tools.clone()));
            out = serde_json::to_vec(&Value::Object(m))
                .map_err(|e| format!("序列化 Qoder 请求体失败: {e}"))?;
        }
    }
    Ok(out)
}

/// 构造 agent 请求体（不含 tools）。
fn agent_body(messages: &[Value], model_key: &str) -> Result<Vec<u8>, String> {
    let prompt = last_user_text(messages);
    let now_ms = now_unix_millis();
    let new_id = new_uuid();

    let model_config = json!({
        "key": model_key,
        "is_reasoning": false,
        "source": MODEL_SOURCE_SYSTEM,
    });

    // 键序与 Go 侧源码一致（serde_json 无 preserve_order ⇒ BTreeMap，输出按 key 排序，
    // 与 Go 的 json.Marshal(map) 行为一致）。
    let mut base: Map<String, Value> = Map::new();
    base.insert("request_id".into(), json!(new_id.clone()));
    // ★ 与 request_id **同一个** newID
    base.insert("chat_record_id".into(), json!(new_id));
    base.insert("request_set_id".into(), json!(new_uuid()));
    base.insert("session_id".into(), json!(derive_session_id(model_key, &prompt)));
    base.insert("stream".into(), json!(true));
    base.insert("aliyun_user_type".into(), json!("personal_professional_trial"));
    base.insert("agent_id".into(), json!("agent_common"));
    base.insert("chat_task".into(), json!("FREE_INPUT"));
    base.insert("is_reply".into(), json!(true));
    base.insert("image_urls".into(), Value::Null);
    base.insert("session_type".into(), json!("qodercli"));
    base.insert("model_config".into(), model_config.clone());
    // `chat_context.text` 是上游协议**必填**项（哪怕 messages 里已有完整历史）
    base.insert(
        "chat_context".into(),
        json!({
            "chatPrompt": "",
            "text": {"type": "text", "text": prompt},
            "extra": {
                "context": [],
                "modelConfig": model_config,
                "originalContent": {"type": "text", "text": prompt},
            },
            "features": [],
            "imageUrls": Value::Null,
        }),
    );
    base.insert(
        "messages".into(),
        Value::Array(normalize_developer_roles(messages)),
    );
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

/// 会话 ID：`sha256(modelKey + "\0" + 首条 user 文本)` 取前 16 字节拼 UUID 形态。
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
/// 只用「模型 + 首条 user 文本」。不引入安全风险（session_id 不是凭据）。
///
/// `\0` 是分隔符：避免 `("ab","c")` 与 `("a","bc")` 撞同一个哈希。
pub(crate) fn derive_session_id(model_key: &str, first_user_text: &str) -> String {
    let mut h = Sha256::new();
    h.update(model_key.as_bytes());
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
    for msg in messages.iter().rev() {
        if msg.get("role").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        let Some(content) = msg.get("content") else {
            continue;
        };
        if let Some(s) = content.as_str() {
            if !s.is_empty() {
                return s.to_string();
            }
            continue;
        }
        if let Some(parts) = content.as_array() {
            let mut sb = String::new();
            for p in parts {
                if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                    if !t.is_empty() {
                        sb.push_str(t);
                    }
                }
            }
            if !sb.is_empty() {
                return sb;
            }
        }
    }
    String::new()
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
        let out = parse(&build_agent_body(body, "qmodel_x").unwrap());
        assert_eq!(out["messages"][0]["role"], "system");
        // 其它角色不动
        assert_eq!(out["messages"][1]["role"], "user");
        // 大小写/空格也要认
        let body2 = br#"{"messages":[{"role":" Developer ","content":"x"},{"role":"user","content":"y"}]}"#;
        let out2 = parse(&build_agent_body(body2, "k").unwrap());
        assert_eq!(out2["messages"][0]["role"], "system");
    }

    #[test]
    fn model_config_has_source_system_and_is_reasoning_false() {
        let out = parse(&build_agent_body(br#"{"messages":[{"role":"user","content":"hi"}]}"#, "qmodel_preview").unwrap());
        assert_eq!(out["model_config"]["key"], "qmodel_preview");
        assert_eq!(out["model_config"]["source"], "system");
        assert_eq!(out["model_config"]["is_reasoning"], false);
        // chat_context.extra 里的那份也必须一致
        assert_eq!(out["chat_context"]["extra"]["modelConfig"]["source"], "system");
    }

    #[test]
    fn base_fields_match_go_spec() {
        // ★ 非 ASCII 内容必须走 `&str` 再 `.as_bytes()`：`br#"..."#` 是**字节**串，
        // 只接受 ASCII（rustc 会直接报 "non-ASCII character in raw byte string literal"）。
        let body = r#"{"messages":[{"role":"user","content":"你好"}]}"#;
        let out = parse(&build_agent_body(body.as_bytes(), "k").unwrap());
        assert_eq!(out["agent_id"], "agent_common");
        assert_eq!(out["chat_task"], "FREE_INPUT");
        assert_eq!(out["session_type"], "qodercli");
        assert_eq!(out["stream"], true);
        assert_eq!(out["is_reply"], true);
        assert_eq!(out["aliyun_user_type"], "personal_professional_trial");
        assert!(out["image_urls"].is_null());
        assert!(out["chat_context"]["imageUrls"].is_null());
        assert!(out["chat_context"]["features"].as_array().unwrap().is_empty());
        assert!(out["chat_context"]["extra"]["context"].as_array().unwrap().is_empty());
        // request_id 与 chat_record_id 必须相同
        assert_eq!(out["request_id"], out["chat_record_id"]);
        // 但 request_set_id / business.id 是各自独立的
        assert_ne!(out["request_id"], out["request_set_id"]);
        assert_ne!(out["request_set_id"], out["business"]["id"]);
        // chat_context.text 取最后一条 user 文本
        assert_eq!(out["chat_context"]["text"]["text"], "你好");
        assert_eq!(out["chat_context"]["text"]["type"], "text");
        assert_eq!(out["chat_context"]["extra"]["originalContent"]["text"], "你好");
        assert_eq!(out["chat_context"]["chatPrompt"], "");
    }

    #[test]
    fn business_name_is_truncated_by_runes() {
        let long = "一".repeat(50);
        let body = serde_json::to_vec(&json!({
            "messages": [{"role":"user","content": long}]
        }))
        .unwrap();
        let out = parse(&build_agent_body(&body, "k").unwrap());
        let name = out["business"]["name"].as_str().unwrap();
        assert_eq!(name.chars().count(), 30, "按字符截断，不能按字节");
        assert_eq!(name, "一".repeat(30));
    }

    #[test]
    fn tools_injected_only_when_client_sends_them() {
        // 不带 tools
        let out = parse(&build_agent_body(br#"{"messages":[{"role":"user","content":"hi"}]}"#, "k").unwrap());
        assert!(out.get("tools").is_none(), "客户端没传 tools 就不该注入");
        // 带 tools
        let body = br#"{"messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"f"}}]}"#;
        let out = parse(&build_agent_body(body, "k").unwrap());
        assert_eq!(out["tools"][0]["function"]["name"], "f");
        // 空数组也不注入
        let out = parse(&build_agent_body(br#"{"messages":[{"role":"user","content":"hi"}],"tools":[]}"#, "k").unwrap());
        assert!(out.get("tools").is_none());
    }

    #[test]
    fn build_rejects_bad_body_and_missing_messages() {
        assert!(build_agent_body(b"not json", "k").unwrap_err().contains("解析 OpenAI 请求体失败"));
        let e = build_agent_body(br#"{"model":"m"}"#, "k").unwrap_err();
        assert!(e.contains("没有 messages"), "实际: {e}");
        assert!(build_agent_body(br#"{"messages":[]}"#, "k").unwrap_err().contains("没有 messages"));
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
        let sa = parse(&build_agent_body(a, "k").unwrap())["session_id"].clone();
        let sb = parse(&build_agent_body(b, "k").unwrap())["session_id"].clone();
        assert_eq!(sa, sb, "system 变化不得改变 session_id");
    }

    #[test]
    fn full_history_is_passed_through() {
        let body = br#"{"messages":[
            {"role":"system","content":"s"},
            {"role":"user","content":"u1"},
            {"role":"assistant","content":"a1"},
            {"role":"tool","content":"t1","tool_call_id":"x"},
            {"role":"user","content":"u2"}]}"#;
        let out = parse(&build_agent_body(body, "k").unwrap());
        assert_eq!(out["messages"].as_array().unwrap().len(), 5, "消息全量透传");
        assert_eq!(out["messages"][3]["role"], "tool");
    }
}
