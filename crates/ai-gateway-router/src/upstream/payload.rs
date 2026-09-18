//! 出站请求体改写。
//!
//! 对应 Go 源文件 `internal/upstream/payload.go`。
//!
//! 改写项（顺序即 Go 侧顺序，不可调换）：
//!   1. 强制 `stream: true`（上游拒绝非流式）
//!   2. `tool_choice` 归一化（上游该字段是 string，对象形式会 400 code=11101）
//!   3. `role` 归一化：`developer` → `system`（上游 role 白名单不含 developer）
//!   4. 国际版补首条 `system`（实测首条非 system → 400 code=11128）
//!   5. `reasoning_effort` 按模型能力降级
//!   6. 指纹脱敏（受开关控制）
//!
//! # 为什么 role 归一化与脱敏解耦
//!
//! `developer` → `system` 是**协议兼容**（补上游白名单），不是内容脱敏。
//! 因此即使 `sanitize=false` 也照常归一 —— 否则关掉脱敏会让 Codex 类客户端
//! 全部撞 11128。

use serde_json::{Map, Value};

use crate::upstream::sanitize::sanitize_messages;

/// 档位从低到高。
fn effort_rank(name: &str) -> Option<i32> {
    Some(match name {
        "off" => 0,
        "minimal" => 1,
        "low" => 2,
        "medium" => 3,
        "high" => 4,
        "xhigh" => 5,
        "max" => 6,
        _ => return None,
    })
}

/// 改写请求体（单 pass）。
///
/// `efforts` 为 `None` 表示未知（不降级）；`intl` 为账号是否国际版。
pub fn prepare_body_for_region(
    src: &[u8],
    sanitize: bool,
    efforts: Option<&std::collections::HashMap<String, Vec<String>>>,
    intl: bool,
) -> Vec<u8> {
    if src.is_empty() {
        return src.to_vec();
    }
    let Ok(Value::Object(mut obj)) = serde_json::from_slice::<Value>(src) else {
        // 解析失败原样返回：让上游报出更诚实的错误，而不是我们改写出一份四不像。
        return src.to_vec();
    };

    obj.insert("stream".into(), Value::Bool(true));
    normalize_tool_choice(&mut obj);
    normalize_roles(&mut obj);
    // 归一化之后再做国际版适配：developer 已被改写成 system，
    // 此时首条若已是 system 就不必补（否则会给 Codex 之类客户端多插一条）。
    if intl {
        ensure_system_first(&mut obj);
    }
    normalize_reasoning_effort(&mut obj, efforts);
    if sanitize {
        if let Some(Value::Array(msgs)) = obj.get_mut("messages") {
            sanitize_messages(msgs);
        }
    }

    serde_json::to_vec(&Value::Object(obj)).unwrap_or_else(|_| src.to_vec())
}

/// 单 pass 改写（不带 effort 能力表）。
pub fn prepare_body(src: &[u8], sanitize: bool) -> Vec<u8> {
    prepare_body_for_region(src, sanitize, None, false)
}

/// 保证 `messages[0]` 是 system（国际版协议要求）。
///
/// 仅在首条不是 system 时补一条**最小**的 system，不改动任何既有消息，也不合并 ——
/// 合并会改变模型看到的对话结构，风险大于收益。空 messages（或缺失）时不动：
/// 那种请求本就缺少上下文，交给上游报错更诚实。
///
/// 补的内容刻意保持中性，因为这里无法得知调用方想要的系统提示；
/// 它只为满足协议前提，不承载业务语义。
fn ensure_system_first(obj: &mut Map<String, Value>) {
    let Some(Value::Array(msgs)) = obj.get("messages") else {
        return;
    };
    let Some(Value::Object(first)) = msgs.first() else {
        return;
    };
    let role = first.get("role").and_then(|r| r.as_str()).unwrap_or("");
    if role.trim().eq_ignore_ascii_case("system") {
        return;
    }
    let sys = serde_json::json!({
        "role": "system",
        "content": "You are a helpful assistant.",
    });
    let mut out = Vec::with_capacity(msgs.len() + 1);
    out.push(sys);
    out.extend(msgs.iter().cloned());
    obj.insert("messages".into(), Value::Array(out));
}

/// 把 `messages` 里的 `developer` 角色归一为 `system`。
///
/// 背景：上游对 role 做白名单校验，`developer` 不在白名单内，命中即 400 code=11128。
/// `developer` 是 OpenAI 新规范里 system 的别名（Codex / Cursor 等用它承载
/// system 级指令），改写为 system 不丢语义。
///
/// 只认 `developer` 这一个值：其余 role（含未知值）一律原样保留，
/// 不合并、不重排、不删除任何消息。
fn normalize_roles(obj: &mut Map<String, Value>) {
    let Some(Value::Array(msgs)) = obj.get_mut("messages") else {
        return;
    };
    for m in msgs.iter_mut() {
        let Some(msg) = m.as_object_mut() else { continue };
        let is_developer = msg
            .get("role")
            .and_then(|r| r.as_str())
            .map(|r| r.trim().eq_ignore_ascii_case("developer"))
            .unwrap_or(false);
        if is_developer {
            msg.insert("role".into(), Value::String("system".into()));
        }
    }
}

/// 按上游 string 类型的 `tool_choice` 改写。
///
/// - `"none"` / `{"type":"none"}` → 删 `tool_choice` 并删 `tools`/`functions`
/// - `{"type":"auto"|"required"}` → 字符串
/// - `{"type":"function","function":{"name":"x"}}` → 字符串 `"x"`
/// - 其他对象 / 非标量 → 删 `tool_choice`
fn normalize_tool_choice(obj: &mut Map<String, Value>) {
    let Some(tc) = obj.get("tool_choice") else {
        return;
    };
    let tc = tc.clone();
    let suppress = |o: &mut Map<String, Value>| {
        o.remove("tools");
        o.remove("functions");
    };
    match tc {
        Value::String(s) => {
            if s.trim().eq_ignore_ascii_case("none") {
                obj.remove("tool_choice");
                suppress(obj);
            }
        }
        Value::Object(v) => {
            let typ = v
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            match typ.as_str() {
                "none" => {
                    obj.remove("tool_choice");
                    suppress(obj);
                }
                "auto" | "required" => {
                    obj.insert("tool_choice".into(), Value::String(typ));
                }
                "function" => {
                    let name = v
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .or_else(|| v.get("name").and_then(|n| n.as_str()))
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let val = if name.is_empty() {
                        "auto".to_string()
                    } else {
                        name
                    };
                    obj.insert("tool_choice".into(), Value::String(val));
                }
                _ => {
                    obj.remove("tool_choice");
                }
            }
        }
        _ => {
            obj.remove("tool_choice");
        }
    }
}

/// 按模型 `supportedEfforts` 降级 `reasoning_effort`（snake/camel 双字段兼容）。
///
/// - 请求档位模型支持 → 原样透传
/// - 请求档位不支持 → 改为 ≤ 请求档位的最高支持档（降级）
/// - 支持档全部高于请求档 → 取最低支持档（偏离最小）
/// - 未知模型 / 未知档位 / 未携带字段 / 模型未缓存 → 一律透传
fn normalize_reasoning_effort(
    obj: &mut Map<String, Value>,
    efforts: Option<&std::collections::HashMap<String, Vec<String>>>,
) {
    let Some(efforts) = efforts.filter(|e| !e.is_empty()) else {
        return;
    };
    let model = obj.get("model").and_then(|m| m.as_str()).unwrap_or("");
    if model.is_empty() {
        return;
    }
    let Some(supported) = efforts.get(model).filter(|s| !s.is_empty()) else {
        return;
    };
    let key = if obj.contains_key("reasoning_effort") {
        "reasoning_effort"
    } else if obj.contains_key("reasoningEffort") {
        "reasoningEffort"
    } else {
        return;
    };
    let Some(req_str) = obj.get(key).and_then(|v| v.as_str()) else {
        return;
    };
    let req_lower = req_str.trim().to_ascii_lowercase();
    let Some(req_idx) = effort_rank(&req_lower) else {
        return;
    };

    // 在 ≤ 请求档位的支持档里选最高档；命中且与请求不同才改写。
    let mut best: Option<(&String, i32)> = None;
    for s in supported {
        if let Some(idx) = effort_rank(&s.trim().to_ascii_lowercase()) {
            if idx <= req_idx && best.map(|(_, b)| idx > b).unwrap_or(true) {
                best = Some((s, idx));
            }
        }
    }
    if let Some((best_name, _)) = best {
        if !best_name.eq_ignore_ascii_case(&req_lower) {
            obj.insert(key.into(), Value::String(best_name.clone()));
        }
        return;
    }
    // 支持档全部高于请求档：取最低支持档。
    let mut lowest: Option<(&String, i32)> = None;
    for s in supported {
        if let Some(idx) = effort_rank(&s.trim().to_ascii_lowercase()) {
            if lowest.map(|(_, l)| idx < l).unwrap_or(true) {
                lowest = Some((s, idx));
            }
        }
    }
    if let Some((lowest_name, _)) = lowest {
        obj.insert(key.into(), Value::String(lowest_name.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn obj_of(v: &Value) -> &Map<String, Value> {
        v.as_object().unwrap()
    }

    #[test]
    fn forces_stream_true() {
        let out = prepare_body(br#"{"model":"m","stream":false}"#, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["stream"], Value::Bool(true));
    }

    #[test]
    fn invalid_json_passthrough() {
        let src = b"{not json";
        assert_eq!(prepare_body(src, true), src.to_vec());
        assert_eq!(prepare_body(b"", true), Vec::<u8>::new());
    }

    #[test]
    fn tool_choice_none_removes_tools() {
        let out = prepare_body(
            br#"{"tool_choice":"none","tools":[{"a":1}],"functions":[{"b":2}]}"#,
            false,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        let o = obj_of(&v);
        assert!(!o.contains_key("tool_choice"));
        assert!(!o.contains_key("tools"));
        assert!(!o.contains_key("functions"));
    }

    #[test]
    fn tool_choice_object_forms() {
        // {"type":"none"} 同样抑制
        let out = prepare_body(br#"{"tool_choice":{"type":"none"},"tools":[1]}"#, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(!obj_of(&v).contains_key("tools"));

        // auto / required → 字符串
        for t in ["auto", "required"] {
            let src = format!(r#"{{"tool_choice":{{"type":"{t}"}}}}"#);
            let out = prepare_body(src.as_bytes(), false);
            let v: Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(v["tool_choice"], Value::String(t.into()), "{t}");
        }

        // function → 名字字符串
        let out = prepare_body(
            br#"{"tool_choice":{"type":"function","function":{"name":"do_thing"}}}"#,
            false,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["tool_choice"], Value::String("do_thing".into()));

        // function 无名 → auto
        let out = prepare_body(br#"{"tool_choice":{"type":"function"}}"#, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["tool_choice"], Value::String("auto".into()));

        // 未知对象类型 → 删除
        let out = prepare_body(br#"{"tool_choice":{"type":"weird"}}"#, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(!obj_of(&v).contains_key("tool_choice"));

        // 非标量（数组）→ 删除
        let out = prepare_body(br#"{"tool_choice":[1,2]}"#, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(!obj_of(&v).contains_key("tool_choice"));
    }

    #[test]
    fn developer_role_becomes_system_even_without_sanitize() {
        let out = prepare_body(
            br#"{"messages":[{"role":"developer","content":"x"},{"role":"user","content":"y"}]}"#,
            false,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["messages"][0]["role"], Value::String("system".into()));
        assert_eq!(v["messages"][1]["role"], Value::String("user".into()));
    }

    #[test]
    fn intl_prepends_system_when_missing() {
        let out = prepare_body_for_region(
            br#"{"messages":[{"role":"user","content":"hi"}]}"#,
            false,
            None,
            true,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], Value::String("system".into()));
        assert_eq!(msgs[1]["role"], Value::String("user".into()));
    }

    #[test]
    fn intl_does_not_prepend_when_system_already_first() {
        let out = prepare_body_for_region(
            br#"{"messages":[{"role":"system","content":"s"},{"role":"user","content":"hi"}]}"#,
            false,
            None,
            true,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["messages"].as_array().unwrap().len(), 2);
    }

    /// developer 被归一后首条已是 system，不应再多插一条。
    #[test]
    fn intl_developer_first_is_not_double_prepended() {
        let out = prepare_body_for_region(
            br#"{"messages":[{"role":"developer","content":"d"},{"role":"user","content":"u"}]}"#,
            false,
            None,
            true,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2, "不应补出第三条: {msgs:?}");
        assert_eq!(msgs[0]["role"], Value::String("system".into()));
    }

    #[test]
    fn non_intl_does_not_prepend() {
        let out = prepare_body(
            br#"{"messages":[{"role":"user","content":"hi"}]}"#,
            false,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["messages"].as_array().unwrap().len(), 1);
    }

    fn efforts_of(pairs: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    #[test]
    fn effort_downgrades_to_highest_supported_below_request() {
        let efforts = efforts_of(&[("m", &["low", "medium"])]);
        let out = prepare_body_for_region(
            br#"{"model":"m","reasoning_effort":"high"}"#,
            false,
            Some(&efforts),
            false,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["reasoning_effort"], Value::String("medium".into()));
    }

    #[test]
    fn effort_supported_passes_through() {
        let efforts = efforts_of(&[("m", &["low", "medium", "high"])]);
        let out = prepare_body_for_region(
            br#"{"model":"m","reasoning_effort":"medium"}"#,
            false,
            Some(&efforts),
            false,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["reasoning_effort"], Value::String("medium".into()));
    }

    #[test]
    fn effort_floors_when_all_supported_are_higher() {
        let efforts = efforts_of(&[("m", &["high", "max"])]);
        let out = prepare_body_for_region(
            br#"{"model":"m","reasoning_effort":"minimal"}"#,
            false,
            Some(&efforts),
            false,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["reasoning_effort"], Value::String("high".into()));
    }

    #[test]
    fn effort_unknown_model_or_value_passes_through() {
        let efforts = efforts_of(&[("m", &["low"])]);
        // 未知模型
        let out = prepare_body_for_region(
            br#"{"model":"other","reasoning_effort":"high"}"#,
            false,
            Some(&efforts),
            false,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["reasoning_effort"], Value::String("high".into()));

        // 未知档位
        let out = prepare_body_for_region(
            br#"{"model":"m","reasoning_effort":"bogus"}"#,
            false,
            Some(&efforts),
            false,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["reasoning_effort"], Value::String("bogus".into()));

        // 未携带字段
        let out = prepare_body_for_region(br#"{"model":"m"}"#, false, Some(&efforts), false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(obj_of(&v).get("reasoning_effort").is_none());
    }

    #[test]
    fn effort_camel_case_key_supported() {
        let efforts = efforts_of(&[("m", &["low"])]);
        let out = prepare_body_for_region(
            br#"{"model":"m","reasoningEffort":"high"}"#,
            false,
            Some(&efforts),
            false,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["reasoningEffort"], Value::String("low".into()));
    }
}
