//! 嵌套 SSE → 标准 OpenAI SSE。
//!
//! 上游给的是 `data: {"body":"{\"choices\":[...]}"}`，客户端要的是
//! `data: {"id":...,"object":"chat.completion.chunk",...,"choices":[...]}`。
//!
//! # 三条踩过的坑（都写进了状态机）
//!
//! 1. **`finish_reason` 只能出现一次**：上游自己会在最后一个内容分片里给
//!    `finish_reason`（本模块原样转发），而收尾逻辑若**无条件**再补一帧
//!    `finish_reason:"stop"`，同一条流里就会出现两次 —— 客户端会一直转圈
//!    然后报错重试。实测 `qoder:Qwen3.8-Flash → 2 次`、`zcode:GLM-5.3-Flash → 1 次`。
//!    ⚠ 只有**非空**的 `finish_reason` 才算"给过"：上游偶尔发
//!    `"finish_reason":""` 的空帧，把它当结束信号会让客户端一直等。
//! 2. **usage 要拆成独立帧**：上游把 usage 塞在**带 choices 的 finish 分片**里，
//!    而严格遵循 OpenAI 规范的客户端只从 `choices` 为空的独立分片读 usage。
//!    顺序很重要：**先发内容帧（含 finish_reason），再发 usage 帧**。
//! 3. **`data: [DONE]` 前不许造假的空帧**：一条什么都没产生的流
//!    （`created == 0`）只发 `data: [DONE]`。

use serde_json::{json, Map, Value};

use super::sse::{parse_chunk, ChunkParse, Chunk};

/// 默认分片 ID。
///
/// 与 upstream 侧的 `chatcmpl-wb2api` **不同**：保留产品标识便于排障。
pub(crate) const DEFAULT_CHUNK_ID: &str = "chatcmpl-qoder";

/// 把上游 SSE 文本翻译成标准 OpenAI SSE。
///
/// 有状态：同一个 `QoderStream` 实例必须按顺序喂入分片。
#[derive(Debug)]
pub(crate) struct QoderStream {
    model: String,
    created: i64,
    role_sent: bool,
    finish_sent: bool,
}

impl QoderStream {
    pub(crate) fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            created: 0,
            role_sent: false,
            finish_sent: false,
        }
    }

    /// 处理一个 payload，返回要写出的字节（可能是空）。
    ///
    /// `Err` = 流内错误，调用方应把它写成错误帧 + `[DONE]` 后收尾。
    pub(crate) fn push_payload(&mut self, payload: &str) -> Result<Vec<u8>, String> {
        match parse_chunk(payload) {
            ChunkParse::Skip => Ok(Vec::new()),
            ChunkParse::Fatal(e) => Err(e.message()),
            ChunkParse::Chunk(ch) => Ok(self.push_chunk(&ch)),
        }
    }

    /// 处理一个已解析的分片。
    pub(crate) fn push_chunk(&mut self, ch: &Chunk) -> Vec<u8> {
        // 纯 usage / 心跳分片：没有正文、没有工具调用、没有结束信号
        if ch.content.is_empty()
            && ch.reasoning_content.is_empty()
            && ch.tool_calls.is_empty()
            && ch.finish_reason.is_empty()
        {
            if !ch.usage.is_empty() {
                return self.usage_frame(&self.model_of(ch), &ch.usage).into_bytes();
            }
            return Vec::new();
        }

        if self.created == 0 {
            self.created = now_unix();
        }

        let mut delta: Map<String, Value> = Map::new();
        if !self.role_sent {
            delta.insert("role".into(), json!("assistant"));
            self.role_sent = true;
        }
        if !ch.reasoning_content.is_empty() {
            delta.insert("reasoning_content".into(), json!(ch.reasoning_content));
        }
        if !ch.content.is_empty() {
            delta.insert("content".into(), json!(ch.content));
        }
        if !ch.tool_calls.is_empty() {
            // 原样转发（由下游的 merge 逻辑按 index 合并）
            delta.insert("tool_calls".into(), Value::Array(ch.tool_calls.clone()));
        }

        let mut choice: Map<String, Value> = Map::new();
        choice.insert("index".into(), json!(0));
        choice.insert("delta".into(), Value::Object(delta));
        if !ch.finish_reason.is_empty() {
            choice.insert("finish_reason".into(), json!(ch.finish_reason));
            // ★ 只有非空才算"给过"
            self.finish_sent = true;
        }

        let model = self.model_of(ch);
        let content_frame = self.frame("chat.completion.chunk", &model, choice);

        // ⚠ 顺序：先内容帧，后 usage 帧
        if !ch.usage.is_empty() {
            let mut out = content_frame.into_bytes();
            out.extend_from_slice(self.usage_frame(&model, &ch.usage).as_bytes());
            return out;
        }
        content_frame.into_bytes()
    }

    /// 上游流结束时的收尾字节。
    pub(crate) fn finish(&mut self) -> Vec<u8> {
        // 一条什么都没产生的流：不造假空帧
        if self.created == 0 {
            return b"data: [DONE]\n\n".to_vec();
        }
        let mut out = Vec::new();
        if !self.finish_sent {
            out.extend_from_slice(self.finish_frame().as_bytes());
        }
        out.extend_from_slice(b"data: [DONE]\n\n");
        out
    }

    /// 出错时的收尾字节（错误帧 + `[DONE]`）。
    pub(crate) fn error_frame(&mut self, msg: &str) -> Vec<u8> {
        let payload = json!({
            "error": {
                "message": msg,
                "type": "upstream_error",
                "code": "qoder_stream_error",
            }
        });
        format!("data: {payload}\n\ndata: [DONE]\n\n").into_bytes()
    }

    fn model_of(&self, ch: &Chunk) -> String {
        if ch.model.is_empty() {
            self.model.clone()
        } else {
            ch.model.clone()
        }
    }

    fn finish_frame(&self) -> String {
        let mut choice: Map<String, Value> = Map::new();
        choice.insert("index".into(), json!(0));
        choice.insert("delta".into(), Value::Object(Map::new()));
        choice.insert("finish_reason".into(), json!("stop"));
        self.frame("chat.completion.chunk", &self.model, choice)
    }

    fn usage_frame(&self, model: &str, usage: &Map<String, Value>) -> String {
        let usage = enrich_cache_aliases(usage);
        let body = json!({
            "id": DEFAULT_CHUNK_ID,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": model,
            "choices": Vec::<Value>::new(),
            "usage": usage,
        });
        format!("data: {body}\n\n")
    }

    fn frame(&self, object: &str, model: &str, choice: Map<String, Value>) -> String {
        let body = json!({
            "id": DEFAULT_CHUNK_ID,
            "object": object,
            "created": self.created,
            "model": model,
            "choices": vec![Value::Object(choice)],
        });
        format!("data: {body}\n\n")
    }
}

/// 补上 CodeBuddy 读的缓存字段别名。
///
/// 标准 OpenAI 放 `prompt_tokens_details.cached_tokens`，而 CodeBuddy 读顶层
/// `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` / `prompt_cache_write_tokens`。
///
/// ⚠ **原生字段优先，绝不覆盖**。
pub(crate) fn enrich_cache_aliases(usage: &Map<String, Value>) -> Map<String, Value> {
    let mut usage = usage.clone();
    let Some(details) = usage.get("prompt_tokens_details").and_then(|v| v.as_object()) else {
        return usage;
    };
    let Some(cached) = details.get("cached_tokens").and_then(num_of_any) else {
        return usage;
    };
    if !usage.contains_key("prompt_cache_hit_tokens") {
        usage.insert("prompt_cache_hit_tokens".into(), json!(cached));
    }
    if !usage.contains_key("prompt_cache_miss_tokens") {
        if let Some(prompt) = usage.get("prompt_tokens").and_then(num_of_any) {
            if prompt >= cached {
                usage.insert("prompt_cache_miss_tokens".into(), json!(prompt - cached));
            }
        }
    }
    usage
}

fn num_of_any(v: &Value) -> Option<i64> {
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    v.as_f64().map(|f| f as i64)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 把上游 SSE 文本整体聚合成一个非流式响应。
///
/// 对应 Go 侧 `AggregateQoder`。
pub(crate) fn aggregate(sse: &str, model: &str) -> Result<Map<String, Value>, String> {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut finish = String::new();

    for line in sse.lines() {
        let Some(payload) = super::sse::payload_of_line(line) else {
            continue;
        };
        let Some(payload) = payload else {
            break; // [DONE]
        };
        match parse_chunk(&payload) {
            ChunkParse::Skip => continue,
            ChunkParse::Fatal(e) => return Err(e.message()),
            ChunkParse::Chunk(ch) => {
                content.push_str(&ch.content);
                reasoning.push_str(&ch.reasoning_content);
                if !ch.tool_calls.is_empty() {
                    tool_calls.extend(ch.tool_calls.iter().cloned());
                }
                if !ch.finish_reason.is_empty() {
                    finish = ch.finish_reason.clone();
                }
            }
        }
    }
    if finish.is_empty() {
        finish = "stop".to_string();
    }

    let mut msg: Map<String, Value> = Map::new();
    msg.insert("role".into(), json!("assistant"));
    msg.insert("content".into(), json!(content));
    if !tool_calls.is_empty() {
        msg.insert("tool_calls".into(), Value::Array(merge_tool_call_deltas(&tool_calls)));
    }
    // ⚠ 空字符串会让部分客户端显示空的思考块 ⇒ 只有非空才写
    if !reasoning.is_empty() {
        msg.insert("reasoning_content".into(), json!(reasoning));
    }

    let mut choice: Map<String, Value> = Map::new();
    choice.insert("index".into(), json!(0));
    choice.insert("message".into(), Value::Object(msg));
    choice.insert("finish_reason".into(), json!(finish));

    let mut out: Map<String, Value> = Map::new();
    out.insert("id".into(), json!(DEFAULT_CHUNK_ID));
    out.insert("object".into(), json!("chat.completion"));
    out.insert("created".into(), json!(now_unix()));
    out.insert("model".into(), json!(model));
    out.insert("choices".into(), json!(vec![Value::Object(choice)]));
    Ok(out)
}

/// 按 `index` 合并流式 tool_call 分片。
///
/// `function` 的字段做**字符串拼接**（上游按增量发），其余字段**仅当
/// 已有值为空时**才写入。
pub(crate) fn merge_tool_call_deltas(parts: &[Value]) -> Vec<Value> {
    let mut by_index: Vec<(i64, Map<String, Value>)> = Vec::new();

    for part in parts {
        let idx = part.get("index").and_then(num_of_any).unwrap_or(0);
        let pos = match by_index.iter().position(|(i, _)| *i == idx) {
            Some(p) => p,
            None => {
                let mut m: Map<String, Value> = Map::new();
                m.insert("index".into(), json!(idx));
                by_index.push((idx, m));
                by_index.len() - 1
            }
        };
        let call = &mut by_index[pos].1;
        let Some(obj) = part.as_object() else { continue };
        for (k, add) in obj {
            if k == "function" {
                let entry = call
                    .entry("function".to_string())
                    .or_insert_with(|| Value::Object(Map::new()));
                if let (Some(dst), Some(src)) = (entry.as_object_mut(), add.as_object()) {
                    for (fk, fv) in src {
                        let old = dst.get(fk).and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let add_s = fv.as_str().map(|s| s.to_string()).unwrap_or_else(|| fv.to_string());
                        dst.insert(fk.clone(), json!(format!("{old}{add_s}")));
                    }
                }
                continue;
            }
            if k == "index" {
                continue;
            }
            // 其余字段：仅当不存在或为空串时才写
            let empty = call
                .get(k)
                .map(|v| v.as_str().map(|s| s.is_empty()).unwrap_or(false) || v.is_null())
                .unwrap_or(true);
            if empty {
                call.insert(k.clone(), add.clone());
            }
        }
    }

    by_index.into_iter().map(|(_, m)| Value::Object(m)).collect()
}

/// 请求体是否是流式（`{"stream": true}`）。
/// 请求体里 `stream` 是否为真。
///
/// Go 侧 `PeekStream`（`stream.go:412`）同样「导出但生产不调用」—— 生产路径
/// 由网关的 `forward.rs` 决定要不要走流式，这里留着是因为它是**流式与非流式
/// 两条路径的唯一判据**，值得有一份可单测的实现。
#[allow(dead_code)]
pub(crate) fn peek_stream(body: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 收集一条流产生的全部 payload 行。
    fn run(frames: &[&str], model: &str) -> Vec<String> {
        let mut s = QoderStream::new(model);
        let mut out = String::new();
        for f in frames {
            out.push_str(&String::from_utf8(s.push_payload(f).unwrap()).unwrap());
        }
        out.push_str(&String::from_utf8(s.finish()).unwrap());
        out.lines()
            .filter_map(|l| l.strip_prefix("data: ").map(|s| s.to_string()))
            .collect()
    }

    fn nested(inner: &str) -> String {
        serde_json::json!({ "body": inner }).to_string()
    }

    #[test]
    fn first_content_frame_carries_role_once() {
        let f1 = nested(r#"{"choices":[{"delta":{"content":"a"}}],"model":"m"}"#);
        let f2 = nested(r#"{"choices":[{"delta":{"content":"b"}}],"model":"m"}"#);
        let out = run(&[&f1, &f2], "m");
        // 4 行 = 两帧内容 + **补的** finish 帧 + [DONE]。
        // ⚠ 补帧是刻意的（Go `stream.go:123-127`）：上游没给过 finish_reason
        //   时不补会让客户端一直等 —— 这里两个分片都没带 finish_reason。
        assert_eq!(out.len(), 4, "实际: {out:#?}");
        let v0: Value = serde_json::from_str(&out[0]).unwrap();
        assert_eq!(v0["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(v0["choices"][0]["delta"]["content"], "a");
        let v1: Value = serde_json::from_str(&out[1]).unwrap();
        assert!(v1["choices"][0]["delta"].get("role").is_none(), "role 只发一次");
        let v2: Value = serde_json::from_str(&out[2]).unwrap();
        assert_eq!(v2["choices"][0]["finish_reason"], "stop", "补的收尾帧");
        assert_eq!(out[3], "[DONE]");
    }

    #[test]
    fn finish_reason_appears_exactly_once_when_upstream_gives_it() {
        // ⚠ 这是修复过的 bug：无条件补帧会让 finish_reason 出现两次
        let f1 = nested(r#"{"choices":[{"delta":{"content":"a"}}],"model":"m"}"#);
        let f2 = nested(r#"{"choices":[{"delta":{"content":"b"},"finish_reason":"stop"}],"model":"m"}"#);
        let out = run(&[&f1, &f2], "m");
        let count = out
            .iter()
            .filter(|l| l.contains("\"finish_reason\":\"stop\""))
            .count();
        assert_eq!(count, 1, "finish_reason 只能出现一次，实际 {count} 次");
    }

    #[test]
    fn finish_is_backfilled_when_upstream_never_gives_it() {
        // 上游只发内容、始终不给 finish_reason 时，不补会让客户端一直等
        let f1 = nested(r#"{"choices":[{"delta":{"content":"a"}}],"model":"m"}"#);
        let out = run(&[&f1], "m");
        let count = out
            .iter()
            .filter(|l| l.contains("\"finish_reason\":\"stop\""))
            .count();
        assert_eq!(count, 1, "必须补一帧，且只补一帧");
    }

    #[test]
    fn empty_finish_reason_does_not_count_as_sent() {
        // 上游偶尔发 "finish_reason":"" 的空帧 —— 那不是结束信号
        let f1 = nested(r#"{"choices":[{"delta":{"content":"a"},"finish_reason":""}],"model":"m"}"#);
        let out = run(&[&f1], "m");
        let count = out
            .iter()
            .filter(|l| l.contains("\"finish_reason\":\"stop\""))
            .count();
        assert_eq!(count, 1, "空 finish_reason 不算给过，仍要补");
    }

    #[test]
    fn empty_stream_emits_only_done() {
        let out = run(&[], "m");
        assert_eq!(out, vec!["[DONE]"], "不造假的空帧");
    }

    #[test]
    fn usage_is_split_into_a_choices_empty_frame_after_the_content_frame() {
        let f = nested(
            r#"{"choices":[{"delta":{"content":"尾"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7},"model":"m"}"#,
        );
        let out = run(&[&f], "m");
        // 期望：[内容帧, usage 帧, [DONE]]
        assert_eq!(out.len(), 3, "实际: {out:#?}");
        let content: Value = serde_json::from_str(&out[0]).unwrap();
        assert_eq!(content["choices"][0]["delta"]["content"], "尾");
        assert_eq!(content["choices"][0]["finish_reason"], "stop");
        assert!(content.get("usage").is_none(), "内容帧不带 usage");

        let usage: Value = serde_json::from_str(&out[1]).unwrap();
        assert_eq!(usage["choices"].as_array().unwrap().len(), 0, "usage 帧 choices 必须为空");
        assert_eq!(usage["usage"]["total_tokens"], 7);
        assert_eq!(usage["object"], "chat.completion.chunk");
        assert_eq!(out[2], "[DONE]");
    }

    #[test]
    fn standalone_usage_frame_is_forwarded() {
        let f1 = nested(r#"{"choices":[{"delta":{"content":"a"}}],"model":"m"}"#);
        let f2 = r#"{"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2},"model":"m"}"#;
        let out = run(&[&f1, f2], "m");
        // 实际：[内容帧, usage 帧, 补的 finish 帧, [DONE]]
        // ⚠ 不要用 out.last() —— 那是 `[DONE]`，不是 JSON。
        assert_eq!(out.last().unwrap(), "[DONE]");
        let usage: Value = serde_json::from_str(&out[1]).unwrap();
        assert_eq!(usage["usage"]["total_tokens"], 2, "实际: {out:#?}");
        assert_eq!(
            usage["choices"].as_array().unwrap().len(),
            0,
            "usage 帧 choices 必须为空"
        );
        assert_eq!(usage["model"], "m", "usage 帧要带上游给的 model");
        // 纯 usage 分片**不产生内容帧**，故流里只有一帧带 choices 的内容帧
        let content_frames = out
            .iter()
            .filter(|l| **l != "[DONE]" && l.contains("\"content\""))
            .count();
        assert_eq!(content_frames, 1, "实际: {out:#?}");
    }

    #[test]
    fn cache_aliases_are_filled_without_overwriting_native_fields() {
        let mut usage: Map<String, Value> = serde_json::from_str(
            r#"{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":40}}"#,
        )
        .unwrap();
        let out = enrich_cache_aliases(&usage);
        assert_eq!(out["prompt_cache_hit_tokens"], 40);
        assert_eq!(out["prompt_cache_miss_tokens"], 60);

        // 原生字段优先
        usage.insert("prompt_cache_hit_tokens".into(), json!(999));
        usage.insert("prompt_cache_miss_tokens".into(), json!(888));
        let out = enrich_cache_aliases(&usage);
        assert_eq!(out["prompt_cache_hit_tokens"], 999, "绝不覆盖原生字段");
        assert_eq!(out["prompt_cache_miss_tokens"], 888);

        // 没有 details ⇒ 原样返回
        let bare: Map<String, Value> = serde_json::from_str(r#"{"prompt_tokens":5}"#).unwrap();
        assert_eq!(enrich_cache_aliases(&bare), bare);

        // prompt < cached 时不写 miss（避免负数）
        let odd: Map<String, Value> =
            serde_json::from_str(r#"{"prompt_tokens":1,"prompt_tokens_details":{"cached_tokens":40}}"#)
                .unwrap();
        let out = enrich_cache_aliases(&odd);
        assert!(!out.contains_key("prompt_cache_miss_tokens"));
    }

    #[test]
    fn in_stream_error_becomes_error_frame_then_done() {
        let mut s = QoderStream::new("m");
        let payload = r#"{"code":"403","message":"{\"isQueued\":true,\"queueCount\":3}"}"#;
        let err = s.push_payload(payload).unwrap_err();
        assert!(err.contains("排队"), "{err}");
        let out = String::from_utf8(s.error_frame(&err)).unwrap();
        assert!(out.contains("\"code\":\"qoder_stream_error\""), "{out}");
        assert!(out.ends_with("data: [DONE]\n\n"), "{out}");
    }

    #[test]
    fn tool_call_deltas_merge_by_index_with_string_concat() {
        let parts: Vec<Value> = serde_json::from_str(
            r#"[{"index":0,"id":"call_1","type":"function","function":{"name":"get_","arguments":"{\"a\""}},
                {"index":0,"function":{"name":"weather","arguments":":1}"}},
                {"index":1,"id":"call_2","function":{"name":"other","arguments":"{}"}}]"#,
        )
        .unwrap();
        let merged = merge_tool_call_deltas(&parts);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0]["index"], 0);
        assert_eq!(merged[0]["id"], "call_1");
        assert_eq!(merged[0]["function"]["name"], "get_weather", "name 要拼接");
        assert_eq!(merged[0]["function"]["arguments"], "{\"a\":1}", "arguments 要拼接");
        assert_eq!(merged[1]["function"]["name"], "other");
    }

    #[test]
    fn tool_call_merge_does_not_overwrite_nonempty_fields() {
        let parts: Vec<Value> = serde_json::from_str(
            r#"[{"index":0,"id":"first","type":"function"},{"index":0,"id":"second","type":"other"}]"#,
        )
        .unwrap();
        let merged = merge_tool_call_deltas(&parts);
        assert_eq!(merged[0]["id"], "first", "非空字段不被后来的覆盖");
        assert_eq!(merged[0]["type"], "function");
    }

    #[test]
    fn tool_calls_are_forwarded_verbatim_in_stream() {
        let f = nested(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"f","arguments":"{}"}}]}}],"model":"m"}"#);
        let out = run(&[&f], "m");
        let v: Value = serde_json::from_str(&out[0]).unwrap();
        assert_eq!(v["choices"][0]["delta"]["tool_calls"][0]["id"], "c");
        assert_eq!(v["choices"][0]["delta"]["tool_calls"][0]["function"]["name"], "f");
    }

    #[test]
    fn reasoning_content_is_forwarded_and_model_falls_back() {
        let f = nested(r#"{"choices":[{"delta":{"reasoning_content":"想"}}]}"#);
        let out = run(&[&f], "fallback-model");
        let v: Value = serde_json::from_str(&out[0]).unwrap();
        assert_eq!(v["choices"][0]["delta"]["reasoning_content"], "想");
        assert_eq!(v["model"], "fallback-model", "上游没给 model 时用客户端请求的模型名");
        assert_eq!(v["id"], "chatcmpl-qoder");
    }

    #[test]
    fn aggregate_builds_a_full_completion() {
        let sse = concat!(
            "data: {\"body\":\"{\\\"choices\\\":[{\\\"delta\\\":{\\\"reasoning_content\\\":\\\"想\\\"}}]}\"}\n\n",
            "data: {\"body\":\"{\\\"choices\\\":[{\\\"delta\\\":{\\\"content\\\":\\\"你好\\\"}}]}\"}\n\n",
            "data: {\"body\":\"{\\\"choices\\\":[{\\\"delta\\\":{\\\"content\\\":\\\"世界\\\"},\\\"finish_reason\\\":\\\"stop\\\"}]}\"}\n\n",
            "data: [DONE]\n\n"
        );
        let out = aggregate(sse, "m").unwrap();
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["model"], "m");
        assert_eq!(out["choices"][0]["message"]["content"], "你好世界");
        assert_eq!(out["choices"][0]["message"]["reasoning_content"], "想");
        assert_eq!(out["choices"][0]["message"]["role"], "assistant");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["id"], "chatcmpl-qoder");
    }

    #[test]
    fn aggregate_defaults_finish_reason_to_stop() {
        let sse = "data: {\"body\":\"{\\\"choices\\\":[{\\\"delta\\\":{\\\"content\\\":\\\"x\\\"}}]}\"}\n\n";
        let out = aggregate(sse, "m").unwrap();
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn aggregate_omits_empty_reasoning_content() {
        // 空字符串会让部分客户端显示空的思考块
        let sse = "data: {\"body\":\"{\\\"choices\\\":[{\\\"delta\\\":{\\\"content\\\":\\\"x\\\"}}]}\"}\n\n";
        let out = aggregate(sse, "m").unwrap();
        assert!(
            out["choices"][0]["message"].get("reasoning_content").is_none(),
            "空的 reasoning_content 不应写出"
        );
    }

    #[test]
    fn aggregate_surfaces_in_stream_error() {
        let sse = "data: {\"code\":\"403\",\"message\":\"{\\\"isQueued\\\":true,\\\"queueCount\\\":5}\"}\n\n";
        let err = aggregate(sse, "m").unwrap_err();
        assert!(err.contains("排队"), "{err}");
        assert!(err.contains('5'), "{err}");
    }

    #[test]
    fn aggregate_skips_malformed_and_unknown_frames() {
        let sse = concat!(
            "data: not-json\n\n",
            ": heartbeat\n\n",
            "data: {\"hello\":1}\n\n",
            "data: {\"body\":\"{\\\"choices\\\":[{\\\"delta\\\":{\\\"content\\\":\\\"ok\\\"}}]}\"}\n\n",
            "data: [DONE]\n\n",
            "data: {\"body\":\"{\\\"choices\\\":[{\\\"delta\\\":{\\\"content\\\":\\\"after-done\\\"}}]}\"}\n\n"
        );
        let out = aggregate(sse, "m").unwrap();
        assert_eq!(out["choices"][0]["message"]["content"], "ok", "[DONE] 之后不再累加");
    }

    #[test]
    fn aggregate_merges_tool_calls() {
        let sse = concat!(
            "data: {\"body\":\"{\\\"choices\\\":[{\\\"delta\\\":{\\\"tool_calls\\\":[{\\\"index\\\":0,\\\"id\\\":\\\"c\\\",\\\"function\\\":{\\\"name\\\":\\\"ge\\\",\\\"arguments\\\":\\\"{\\\\\\\"a\\\\\\\"\\\"}}]}}]}\"}\n\n",
            "data: {\"body\":\"{\\\"choices\\\":[{\\\"delta\\\":{\\\"tool_calls\\\":[{\\\"index\\\":0,\\\"function\\\":{\\\"name\\\":\\\"t_x\\\",\\\"arguments\\\":\\\":1}\\\"}}]}}]}\"}\n\n"
        );
        let out = aggregate(sse, "m").unwrap();
        let tc = &out["choices"][0]["message"]["tool_calls"];
        assert_eq!(tc[0]["function"]["name"], "get_x");
        assert_eq!(tc[0]["function"]["arguments"], "{\"a\":1}");
        assert_eq!(tc[0]["id"], "c");
    }

    #[test]
    fn peek_stream_reads_only_a_true_boolean() {
        assert!(peek_stream(br#"{"model":"m","stream":true}"#));
        assert!(!peek_stream(br#"{"model":"m","stream":false}"#));
        assert!(!peek_stream(br#"{"model":"m"}"#));
        assert!(!peek_stream(br#"{"stream":"true"}"#), "字符串 true 不算");
        assert!(!peek_stream(b"not json"));
    }
}
