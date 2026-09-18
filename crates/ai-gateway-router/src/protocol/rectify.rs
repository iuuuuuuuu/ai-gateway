//! 请求整流：修复客户端历史里的 tool 配对问题。
//!
//! 对应 Go 源文件 `internal/server/rectify.go`。
//!
//! # 背景
//!
//! Claude Desktop / Claude Code / Codex 的会话历史在上下文压缩、并发工具乱序
//! 完成、中断恢复等情况下，会出现 `assistant.tool_calls` 与 `role:tool` 消息
//! **不配对**（缺失应答、孤儿结果、顺序错乱）。上游（CodeBuddy）对 tool 序列
//! 校验严格，命中即 HTTP 400 code=11148（`tool_call_sequence_broken`）。
//!
//! # 规则
//!
//! 1. 无 `tool` 消息应答的 `tool_call` → 删除；删空且无正文的 assistant → 整条删除
//! 2. 无对应 `tool_call` 的孤儿 tool 消息 → 删除；同一 `tool_call_id` 的重复应答只留一条
//! 3. 应答消息归位到对应 assistant 正后方，并按 `tool_calls` 顺序稳定重排
//!    （覆盖并发乱序与 assistant 被拆散两种形态）
//!
//! **整流是幂等的**：配对完好的请求原样返回（零改动，`fixed == 0`）。
//!
//! **不伪造内容**：只做「删不配对、排对顺序」，不做占位补齐 —— 补一条假的
//! tool 应答会改变模型看到的对话语义。

use serde_json::{Map, Value};

/// 整流 `messages` 中的 tool 配对。
///
/// 返回修复动作计数（0 表示无需修改，调用方可据此决定是否打日志）。
pub fn rectify_chat_tool_sequence(messages: &mut Vec<Value>) -> usize {
    if messages.is_empty() {
        return 0;
    }

    // ── 第一遍：收集全部 tool_call id 与全部 tool 应答 id ──
    let mut called: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in messages.iter() {
        let Some(obj) = m.as_object() else { continue };
        if str_field(obj, "role") != "assistant" {
            continue;
        }
        for call in tool_calls_of(obj) {
            let id = str_field(&call, "id");
            if !id.is_empty() {
                called.insert(id);
            }
        }
    }
    let mut responded: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in messages.iter() {
        let Some(obj) = m.as_object() else { continue };
        if str_field(obj, "role") != "tool" {
            continue;
        }
        let id = str_field(obj, "tool_call_id");
        if !id.is_empty() {
            responded.insert(id);
        }
    }
    if called.is_empty() && responded.is_empty() {
        return 0;
    }

    // ── 第二遍：删除缺失应答的 tool_call、孤儿 tool 消息、重复应答、空 assistant ──
    let mut fixed = 0usize;
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    for m in messages.iter() {
        let Some(obj) = m.as_object() else {
            out.push(m.clone());
            continue;
        };
        match str_field(obj, "role").as_str() {
            "tool" => {
                let id = str_field(obj, "tool_call_id");
                // 孤儿（无调用）或重复应答（同一 id 第二条起）一律丢弃。
                if id.is_empty() || !called.contains(&id) || used.contains(&id) {
                    fixed += 1;
                    continue;
                }
                used.insert(id);
                out.push(m.clone());
            }
            "assistant" => {
                let calls = tool_calls_of(obj);
                if calls.is_empty() {
                    out.push(m.clone());
                    continue;
                }
                let mut kept: Vec<Value> = Vec::with_capacity(calls.len());
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                for call in calls {
                    let id = str_field(&call, "id");
                    // 空 id、无应答、重复 id 的调用都删掉。
                    if id.is_empty() || !responded.contains(&id) || seen.contains(&id) {
                        fixed += 1;
                        continue;
                    }
                    seen.insert(id);
                    kept.push(Value::Object(call));
                }
                let mut cloned = obj.clone();
                if kept.is_empty() {
                    cloned.remove("tool_calls");
                    if content_empty(&cloned) {
                        fixed += 1;
                        continue;
                    }
                    out.push(Value::Object(cloned));
                    continue;
                }
                cloned.insert("tool_calls".into(), Value::Array(kept));
                out.push(Value::Object(cloned));
            }
            _ => out.push(m.clone()),
        }
    }

    // ── 第三遍：应答归位到对应 assistant 正后方，并按 tool_calls 顺序稳定重排 ──
    //
    // 覆盖两种畸形形态：
    //   - 乱序：tool 消息都在紧跟段里，但顺序与调用顺序不一致（并发完成）
    //   - 隔断：tool 消息落在更远处（assistant 被拆散成多条、结果被隔开）
    let mut consumed = vec![false; out.len()];
    let mut rebuilt: Vec<Value> = Vec::with_capacity(out.len());
    let mut i = 0usize;
    while i < out.len() {
        if consumed[i] {
            i += 1;
            continue;
        }
        let m = &out[i];
        let obj = m.as_object();
        let calls = obj.map(tool_calls_of).unwrap_or_default();
        if obj.map(|o| str_field(o, "role")) != Some("assistant".to_string()) || calls.is_empty() {
            rebuilt.push(m.clone());
            i += 1;
            continue;
        }

        // tool_call id → 在 calls 中的下标
        let mut order: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (k, call) in calls.iter().enumerate() {
            let id = str_field(call, "id");
            if !id.is_empty() {
                order.entry(id).or_insert(k);
            }
        }

        // 紧跟的连续 tool 段（仅收属于本 assistant 调用的应答；
        // 混入的其他应答留给其真正的 assistant 处理）。
        let mut j = i + 1;
        while j < out.len()
            && out[j].as_object().map(|o| str_field(o, "role")) == Some("tool".to_string())
            && !consumed[j]
            && order_has(&order, out[j].as_object().unwrap())
        {
            j += 1;
        }
        let mut seg: Vec<Value> = Vec::with_capacity(calls.len());
        for k in i + 1..j {
            consumed[k] = true;
            seg.push(out[k].clone());
        }
        // 全局查找被隔断的应答（id 精确匹配，第二遍后 id 全局唯一）。
        if seg.len() < order.len() {
            let mut have: std::collections::HashSet<String> = seg
                .iter()
                .filter_map(|tm| tm.as_object())
                .map(|tm| str_field(tm, "tool_call_id"))
                .collect();
            for k in 0..out.len() {
                if seg.len() >= order.len() {
                    break;
                }
                if k == i || consumed[k] {
                    continue;
                }
                let Some(tm) = out[k].as_object() else { continue };
                if have.contains(&str_field(tm, "tool_call_id")) {
                    continue;
                }
                if str_field(tm, "role") != "tool" || !order_has(&order, tm) {
                    continue;
                }
                consumed[k] = true;
                have.insert(str_field(tm, "tool_call_id"));
                seg.push(out[k].clone());
                fixed += 1; // 被隔断的应答挪回 assistant 正后方
            }
        }

        // 按 tool_calls 顺序稳定排序
        let mut sorted = seg.clone();
        sorted.sort_by_key(|tm| {
            let id = tm
                .as_object()
                .map(|o| str_field(o, "tool_call_id"))
                .unwrap_or_default();
            order.get(&id).copied().unwrap_or(usize::MAX)
        });
        for k in 0..seg.len() {
            let a = seg[k].as_object().map(|o| str_field(o, "tool_call_id"));
            let b = sorted[k].as_object().map(|o| str_field(o, "tool_call_id"));
            if a != b {
                fixed += 1;
            }
        }

        rebuilt.push(m.clone());
        rebuilt.extend(sorted);
        i = j;
    }

    if fixed == 0 {
        return 0;
    }
    *messages = rebuilt;
    fixed
}

/// 判断 tool 消息是否属于当前 assistant 的调用集合。
fn order_has(order: &std::collections::HashMap<String, usize>, m: &Map<String, Value>) -> bool {
    let id = str_field(m, "tool_call_id");
    if id.is_empty() {
        return false;
    }
    order.contains_key(&id)
}

/// 取 assistant 消息的 `tool_calls` 数组（缺省 / 类型不符时为空）。
fn tool_calls_of(m: &Map<String, Value>) -> Vec<Map<String, Value>> {
    m.get("tool_calls")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_object().cloned()).collect())
        .unwrap_or_default()
}

/// 判断消息正文是否为空（null / 空串 / 空数组都算空）。
fn content_empty(m: &Map<String, Value>) -> bool {
    match m.get("content") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        _ => false,
    }
}

fn str_field(m: &Map<String, Value>, key: &str) -> String {
    m.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msgs(v: Value) -> Vec<Value> {
        v.as_array().unwrap().clone()
    }

    fn assistant_with_calls(ids: &[&str]) -> Value {
        json!({
            "role": "assistant",
            "content": null,
            "tool_calls": ids.iter().map(|id| json!({
                "id": id, "type": "function",
                "function": {"name": "f", "arguments": "{}"}
            })).collect::<Vec<_>>()
        })
    }

    fn tool_msg(id: &str) -> Value {
        json!({"role": "tool", "tool_call_id": id, "content": "ok"})
    }

    /// 配对完好且有序 → 零改动（幂等，不重建切片）。
    #[test]
    fn well_formed_sequence_untouched() {
        let mut m = msgs(json!([
            {"role": "user", "content": "hi"},
            assistant_with_calls(&["a"]),
            tool_msg("a"),
            {"role": "assistant", "content": "done"},
        ]));
        let before = m.clone();
        assert_eq!(rectify_chat_tool_sequence(&mut m), 0);
        assert_eq!(m, before);
    }

    #[test]
    fn no_tool_messages_at_all_is_noop() {
        let mut m = msgs(json!([
            {"role": "system", "content": "s"},
            {"role": "user", "content": "u"},
        ]));
        let before = m.clone();
        assert_eq!(rectify_chat_tool_sequence(&mut m), 0);
        assert_eq!(m, before);
        let mut empty: Vec<Value> = Vec::new();
        assert_eq!(rectify_chat_tool_sequence(&mut empty), 0);
    }

    /// 规则 1：无应答的 tool_call 删除；删空且无正文的 assistant 整条删除。
    #[test]
    fn drops_unanswered_tool_call() {
        let mut m = msgs(json!([
            {"role": "user", "content": "u"},
            assistant_with_calls(&["a", "b"]),
            tool_msg("a"),
        ]));
        assert!(rectify_chat_tool_sequence(&mut m) > 0);
        // assistant 只剩 call a
        let calls = m[1]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], json!("a"));
    }

    #[test]
    fn drops_empty_assistant_after_call_removal() {
        let mut m = msgs(json!([
            {"role": "user", "content": "u"},
            assistant_with_calls(&["a"]),
            // 没有 tool 应答 → 调用被删 → assistant 无正文 → 整条删除
        ]));
        assert!(rectify_chat_tool_sequence(&mut m) > 0);
        assert_eq!(m.len(), 1, "空 assistant 应整条删除: {m:?}");
        assert_eq!(m[0]["role"], json!("user"));
    }

    /// assistant 有正文时只删 tool_calls，保留消息本身。
    #[test]
    fn keeps_assistant_with_text_when_calls_dropped() {
        let mut m = msgs(json!([
            {"role": "user", "content": "u"},
            {"role": "assistant", "content": "有正文",
             "tool_calls": [{"id": "a", "function": {"name": "f"}}]},
        ]));
        assert!(rectify_chat_tool_sequence(&mut m) > 0);
        assert_eq!(m.len(), 2);
        assert!(m[1].get("tool_calls").is_none(), "tool_calls 应被删除");
        assert_eq!(m[1]["content"], json!("有正文"));
    }

    /// 规则 2：孤儿 tool 消息删除。
    #[test]
    fn drops_orphan_tool_message() {
        let mut m = msgs(json!([
            {"role": "user", "content": "u"},
            tool_msg("ghost"),
        ]));
        assert!(rectify_chat_tool_sequence(&mut m) > 0);
        assert_eq!(m.len(), 1);
    }

    /// 规则 2：同一 id 的重复应答只留一条。
    #[test]
    fn drops_duplicate_tool_response() {
        let mut m = msgs(json!([
            {"role": "user", "content": "u"},
            assistant_with_calls(&["a"]),
            tool_msg("a"),
            tool_msg("a"),
        ]));
        assert!(rectify_chat_tool_sequence(&mut m) > 0);
        let tools: Vec<_> = m
            .iter()
            .filter(|x| x["role"] == json!("tool"))
            .collect();
        assert_eq!(tools.len(), 1, "重复应答应只留一条: {m:?}");
    }

    /// 规则 3：并发乱序完成 → 按 tool_calls 顺序重排。
    #[test]
    fn reorders_out_of_order_responses() {
        let mut m = msgs(json!([
            {"role": "user", "content": "u"},
            assistant_with_calls(&["a", "b", "c"]),
            tool_msg("c"),
            tool_msg("a"),
            tool_msg("b"),
        ]));
        assert!(rectify_chat_tool_sequence(&mut m) > 0);
        let ids: Vec<String> = m
            .iter()
            .filter(|x| x["role"] == json!("tool"))
            .map(|x| x["tool_call_id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["a", "b", "c"], "应按调用顺序重排");
    }

    /// 规则 3：被隔断的应答挪回 assistant 正后方。
    #[test]
    fn pulls_back_stranded_response() {
        let mut m = msgs(json!([
            {"role": "user", "content": "u"},
            assistant_with_calls(&["a"]),
            {"role": "user", "content": "隔断"},
            tool_msg("a"),
        ]));
        assert!(rectify_chat_tool_sequence(&mut m) > 0);
        // assistant 之后紧跟 tool
        assert_eq!(m[1]["role"], json!("assistant"));
        assert_eq!(m[2]["role"], json!("tool"), "应答应紧跟 assistant: {m:?}");
        assert_eq!(m[2]["tool_call_id"], json!("a"));
    }

    /// 空 id 的调用删除 —— 但**必须存在非空 id 的调用或 tool 消息**才会进入整流。
    ///
    /// 这是 Go 侧 `len(called)==0 && len(responded)==0` 早退的语义：只有空 id 调用时
    /// `called` 收集不到任何 id（空 id 不入集合），于是直接原样返回、零改动。
    /// 单独一条空 id 调用属于「本来就没有 tool 配对可修」的情况。
    #[test]
    fn drops_call_with_empty_id_when_rectify_engages() {
        // 有一个正常配对的调用 → 整流会真正执行，空 id 调用被删
        let mut m = msgs(json!([
            {"role": "user", "content": "u"},
            {"role": "assistant", "content": "t", "tool_calls": [
                {"id": "good", "function": {"name": "f", "arguments": "{}"}},
                {"id": "", "function": {"name": "g", "arguments": "{}"}}
            ]},
            tool_msg("good"),
        ]));
        assert!(rectify_chat_tool_sequence(&mut m) > 0);
        let calls = m[1]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1, "空 id 调用应被删除: {calls:?}");
        assert_eq!(calls[0]["id"], json!("good"));
    }

    /// 只有空 id 调用、且无任何 tool 消息 → 整流不介入（与 Go 早退一致）。
    #[test]
    fn lone_empty_id_call_is_left_alone() {
        let mut m = msgs(json!([
            {"role": "user", "content": "u"},
            {"role": "assistant", "content": "t",
             "tool_calls": [{"id": "", "function": {"name": "f"}}]},
        ]));
        let before = m.clone();
        assert_eq!(
            rectify_chat_tool_sequence(&mut m),
            0,
            "无配对可修时不得改动（Go 早退语义）"
        );
        assert_eq!(m, before);
    }

    /// 整流幂等：跑两遍第二遍不再改动。
    #[test]
    fn rectify_is_idempotent() {
        let mut m = msgs(json!([
            {"role": "user", "content": "u"},
            assistant_with_calls(&["a", "b"]),
            tool_msg("b"),
            tool_msg("a"),
        ]));
        let first = rectify_chat_tool_sequence(&mut m);
        assert!(first > 0);
        let after = m.clone();
        let second = rectify_chat_tool_sequence(&mut m);
        assert_eq!(second, 0, "第二遍不应再改动");
        assert_eq!(m, after);
    }
}
