//! 上游 SSE 流处理：聚合成单个 OpenAI 响应，或逐帧规范化透传给客户端。
//!
//! 对应 Go 源文件 `internal/upstream/sse.go`。
//!
//! # 两条路径
//!
//! - [`aggregate`]：把完整 SSE 流读成 `chat.completion`（非流式请求用）
//! - [`normalize_frame`]：按 OpenAI 流式规范白名单重建单帧（流式透传用）
//!
//! # 为什么必须做白名单重建
//!
//! 上游会在帧里塞进标准之外的东西：`finish_reason: ""`、空 `content`、
//! 空 `tool_calls` 列表、空占位 `function_call`、顶层未知字段。
//! 严格按规范解析的客户端遇到这些会报错或误判。重建后空键一律省略、
//! `finish_reason` 缺失补 `null`，任意标准客户端都能正常解析。
//!
//! **唯一的例外是 `error` 帧**：上游常在 HTTP 200 的流中途发 `{"error":{...}}`
//!（如渠道未批准、账号被封）来表示失败。白名单重建会把它降级成一个普通的
//! `chat.completion.chunk`，客户端于是把「截断的回答 + 正常 [DONE]」当成一次成功，
//! 永远不知道请求失败了。因此 `error` 必须原样保留。

use serde_json::{Map, Value};

/// SSE 流中「有效数据帧」为零时的错误文案。
pub const EMPTY_STREAM_ERR: &str = "upstream stream contained no valid data events";

/// 聚合结果：一个 OpenAI `chat.completion` 响应体。
pub type ChatCompletion = Map<String, Value>;

/// 读取完整 SSE 流，聚合 `delta.content` 为单个 `chat.completion`。
///
/// 分片/半行由逐行读取处理；遇到 `data: [DONE]` 结束。
/// `tool_calls` 以流式 delta 到达，按 `index` 合并（首片带 id/type/name，
/// 后续只带 arguments 片段）。
///
/// 上游返回 200 但没有任何有效数据事件（空流 / 只有 `[DONE]` / 只有注释行）时
/// 返回错误，由调用方映射为 502 —— 不合成空 content 的假成功响应。
pub fn aggregate(sse: &str) -> Result<ChatCompletion, String> {
    let mut id = String::new();
    let mut model = String::new();
    let mut created: f64 = 0.0;
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut role = "assistant".to_string();
    let mut finish_reason = "stop".to_string();
    let mut usage: Option<Value> = None;
    let mut got_any_content = false;
    let mut valid_events = 0usize;
    // index → 累计的 tool_call 对象；order 记录出现顺序（聚合后按 index 升序输出）
    let mut tool_calls: std::collections::BTreeMap<i64, Map<String, Value>> =
        std::collections::BTreeMap::new();

    for line in sse.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        if payload == "[DONE]" {
            // 上游显式结束：停止读取，DONE 之后的任何数据一律忽略。
            break;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(payload) else {
            // 解析失败静默跳过，且**不计入**有效事件数。
            continue;
        };
        valid_events += 1;
        if id.is_empty() {
            if let Some(v) = chunk.get("id").and_then(|v| v.as_str()) {
                id = v.to_string();
            }
        }
        if model.is_empty() {
            if let Some(v) = chunk.get("model").and_then(|v| v.as_str()) {
                model = v.to_string();
            }
        }
        if created == 0.0 {
            if let Some(v) = chunk.get("created").and_then(|v| v.as_f64()) {
                created = v;
            }
        }
        if let Some(u) = chunk.get("usage") {
            if u.is_object() {
                usage = Some(u.clone());
            }
        }
        let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) else {
            continue;
        };
        for c in choices {
            let Some(c) = c.as_object() else { continue };
            if let Some(fr) = c.get("finish_reason").and_then(|v| v.as_str()) {
                if !fr.is_empty() {
                    finish_reason = fr.to_string();
                }
            }
            if let Some(delta) = c.get("delta").and_then(|d| d.as_object()) {
                if let Some(r) = delta.get("role").and_then(|v| v.as_str()) {
                    if !r.is_empty() {
                        role = r.to_string();
                    }
                }
                if let Some(txt) = delta.get("content").and_then(|v| v.as_str()) {
                    content.push_str(txt);
                    // 只有**非空**正文才锁死 message 回退路径：首帧常带
                    // `"content":""`（仅含 role 的保活帧），若空串也置位，
                    // 后续「完整消息放在 message 里」的上游形态就再也读不到内容，
                    // 客户端只会收到空回复。
                    if !txt.is_empty() {
                        got_any_content = true;
                    }
                }
                if let Some(rc) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                    reasoning.push_str(rc);
                }
                if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        let Some(call) = tc.as_object() else { continue };
                        let idx = index_of_tool_call(call);
                        let merged = tool_calls
                            .entry(idx)
                            .or_insert_with(|| Map::new());
                        merge_tool_call_delta(merged, call);
                    }
                }
            }
            // 有的上游把完整消息放在 message 里（非 delta）
            if !got_any_content {
                if let Some(txt) = c
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|v| v.as_str())
                {
                    content.push_str(txt);
                }
            }
        }
    }

    if valid_events == 0 {
        return Err(EMPTY_STREAM_ERR.to_string());
    }
    if id.is_empty() {
        id = format!("chatcmpl-{}", now_nanos());
    }
    if created == 0.0 {
        created = now_secs() as f64;
    }

    let mut message = Map::new();
    message.insert("role".into(), Value::String(role));
    message.insert("content".into(), Value::String(content));
    if !reasoning.is_empty() {
        message.insert("reasoning_content".into(), Value::String(reasoning));
    }
    if !tool_calls.is_empty() {
        let calls: Vec<Value> = tool_calls
            .into_iter()
            .map(|(idx, mut m)| {
                // index 回填到每个调用里（Go 侧 mergeToolCallDelta 首片已写入）
                m.entry("index".to_string()).or_insert(Value::from(idx));
                Value::Object(m)
            })
            .collect();
        message.insert("tool_calls".into(), Value::Array(calls));
    }

    let mut resp = Map::new();
    resp.insert("id".into(), Value::String(id));
    resp.insert("object".into(), Value::String("chat.completion".into()));
    resp.insert("created".into(), Value::from(created as i64));
    resp.insert("model".into(), Value::String(model));
    resp.insert(
        "choices".into(),
        Value::Array(vec![serde_json::json!({
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish_reason,
        })]),
    );
    if let Some(u) = usage {
        resp.insert("usage".into(), u);
    }
    Ok(resp)
}

/// 取 `tool_call` 分片的 `index`。
///
/// 兼容三种上游形态（实测均出现过）：数字、字符串数字、缺省（视为单调用 → 0）。
///
/// 为什么必须兼容字符串：只认数字时字符串 index 会静默变成 0，
/// 使多个并行工具调用全部合并进槽位 0 —— 名字被后者覆盖、参数被拼接，
/// 客户端拿到一个损坏的工具调用。
fn index_of_tool_call(call: &Map<String, Value>) -> i64 {
    match call.get("index") {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0).max(0),
        _ => 0,
    }
}

/// 把流式 `tool_call` 片段合并到累计对象：
/// `id`/`type`/`function.name` 直覆盖（后续分片通常缺省），`function.arguments` 拼接。
fn merge_tool_call_delta(merged: &mut Map<String, Value>, delta: &Map<String, Value>) {
    if let Some(v) = delta.get("id").and_then(|v| v.as_str()) {
        if !v.is_empty() {
            merged.insert("id".into(), Value::String(v.to_string()));
        }
    }
    if let Some(v) = delta.get("type").and_then(|v| v.as_str()) {
        if !v.is_empty() {
            merged.insert("type".into(), Value::String(v.to_string()));
        }
    }
    let Some(df) = delta.get("function").and_then(|f| f.as_object()) else {
        return;
    };
    if !merged.contains_key("function") {
        merged.insert("function".into(), Value::Object(Map::new()));
    }
    let Some(mf) = merged.get_mut("function").and_then(|f| f.as_object_mut()) else {
        return;
    };
    if let Some(v) = df.get("name").and_then(|v| v.as_str()) {
        if !v.is_empty() {
            mf.insert("name".into(), Value::String(v.to_string()));
        }
    }
    if let Some(v) = df.get("arguments").and_then(|v| v.as_str()) {
        if !v.is_empty() {
            let prev = mf
                .get("arguments")
                .and_then(|a| a.as_str())
                .unwrap_or("")
                .to_string();
            mf.insert("arguments".into(), Value::String(prev + v));
        }
    }
}

/// 按 OpenAI 流式规范白名单重建帧。
///
/// 剔除上游噪声（`finish_reason:""` → `null`、空 content/refusal、
/// 空 `tool_calls` 列表、空占位 `function_call`、顶层未知字段），
/// 空 delta 键一律省略，`usage` 缺失 → `null`，保证任意标准客户端按规范解析。
///
/// `error` 字段原样保留（见模块文档）。
pub fn normalize_frame(obj: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    for k in [
        "id",
        "object",
        "created",
        "model",
        "system_fingerprint",
        "service_tier",
    ] {
        if let Some(v) = obj.get(k) {
            if !v.is_null() {
                out.insert(k.to_string(), v.clone());
            }
        }
    }
    out.entry("object".to_string())
        .or_insert(Value::String("chat.completion.chunk".into()));
    out.entry("id".to_string())
        .or_insert(Value::String("chatcmpl-wb2api".into()));

    if let Some(chs) = obj.get("choices").and_then(|c| c.as_array()) {
        let mut nchs = Vec::with_capacity(chs.len());
        for ci in chs {
            let Some(c) = ci.as_object() else { continue };
            let mut nc = Map::new();
            if let Some(idx) = c.get("index") {
                nc.insert("index".into(), idx.clone());
            }
            let mut delta = Map::new();
            if let Some(d) = c.get("delta").and_then(|d| d.as_object()) {
                if let Some(v) = d.get("role").and_then(|v| v.as_str()) {
                    if !v.is_empty() {
                        delta.insert("role".into(), Value::String(v.to_string()));
                    }
                }
                if let Some(v) = d.get("content").and_then(|v| v.as_str()) {
                    if !v.is_empty() {
                        delta.insert("content".into(), Value::String(v.to_string()));
                    }
                }
                if let Some(v) = d.get("reasoning_content").and_then(|v| v.as_str()) {
                    if !v.is_empty() {
                        delta.insert("reasoning_content".into(), Value::String(v.to_string()));
                    }
                }
                if let Some(v) = d.get("refusal").and_then(|v| v.as_str()) {
                    if !v.is_empty() {
                        delta.insert("refusal".into(), Value::String(v.to_string()));
                    }
                }
                if let Some(tcs) = d.get("tool_calls").and_then(|v| v.as_array()) {
                    if !tcs.is_empty() {
                        delta.insert("tool_calls".into(), Value::Array(tcs.clone()));
                    }
                }
                if let Some(fc) = d.get("function_call") {
                    if !fc.is_null() {
                        // 空占位 function_call（name/arguments 全空）视为噪声剔除
                        let keep = match fc.as_object() {
                            Some(fcm) => {
                                let n = fcm.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                let a =
                                    fcm.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
                                !n.is_empty() || !a.is_empty()
                            }
                            None => true,
                        };
                        if keep {
                            delta.insert("function_call".into(), fc.clone());
                        }
                    }
                }
            }
            nc.insert("delta".into(), Value::Object(delta));
            match c.get("finish_reason").and_then(|v| v.as_str()) {
                Some(fr) if !fr.is_empty() => {
                    nc.insert("finish_reason".into(), Value::String(fr.to_string()));
                }
                _ => {
                    nc.insert("finish_reason".into(), Value::Null);
                }
            }
            nchs.push(Value::Object(nc));
        }
        out.insert("choices".into(), Value::Array(nchs));
    }
    match obj.get("usage") {
        Some(u) => {
            out.insert("usage".into(), u.clone());
        }
        None => {
            out.insert("usage".into(), Value::Null);
        }
    }
    // error 帧必须原样保留（见模块文档）。
    if let Some(e) = obj.get("error") {
        if !e.is_null() {
            out.insert("error".into(), e.clone());
        }
    }
    Value::Object(out)
}

/// 把一帧 payload 规范化为 SSE 帧文本；JSON 解析失败时原样返回。
///
/// 返回值第二项表示是否为**有效**帧（JSON 解析成功）。解析失败的帧仍会写出
///（保持透明），但不计入有效帧数 —— 空流判定依赖这个计数。
pub fn normalize_payload(payload: &str) -> (String, bool) {
    match serde_json::from_str::<Value>(payload) {
        Ok(Value::Object(obj)) => {
            let normalized = normalize_frame(&obj);
            match serde_json::to_string(&normalized) {
                Ok(s) => (s, true),
                Err(_) => (payload.to_string(), true),
            }
        }
        _ => (payload.to_string(), false),
    }
}

/// 逐帧解析上游 SSE，产出 `(帧文本, 是否有效)`；已过滤 `[DONE]` 与空行。
///
/// 调用方负责把帧写成 `data: {帧}\n\n` 并 flush。
pub struct SseFrames<'a> {
    lines: std::str::Lines<'a>,
    done: bool,
}

impl<'a> SseFrames<'a> {
    /// 从原始 SSE 文本构造迭代器。
    pub fn new(sse: &'a str) -> Self {
        Self {
            lines: sse.lines(),
            done: false,
        }
    }
}

impl Iterator for SseFrames<'_> {
    /// `(payload, valid)`；`valid=false` 表示 JSON 未解析成功。
    type Item = (String, bool);

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        for line in self.lines.by_ref() {
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.starts_with("data: [DONE]") {
                // 上游显式结束：DONE 之后的任何数据（含垃圾帧）一律不再透传。
                self.done = true;
                return None;
            }
            if let Some(payload) = trimmed.strip_prefix("data: ") {
                return Some(normalize_payload(payload));
            }
            // 注释/其他非空行：原样透传（视为有效，避免误判空流）
            if !trimmed.is_empty() {
                return Some((trimmed.to_string(), true));
            }
        }
        None
    }
}

/// 逐帧解析**流式**上游 SSE（用于把响应边收边转，而不是先读满内存）。
///
/// 与 [`SseFrames`] 的差别：那个吃 `&str`（适合已读完整的聚合场景），
/// 这个吃字节流（适合透传场景，长回答不必等上游结束）。
///
/// 内部维护行缓冲：SSE 帧以 `\n` 分隔，而网络分片可能把一行切成几段，
/// 因此必须按 `\n` 边界切分，不能按分片边界。
pub struct SseFramesIter<S> {
    inner: S,
    buf: String,
    done: bool,
    valid_frames: usize,
    wrote_error: bool,
}

impl<S> SseFramesIter<S> {
    /// 从字节流构造。
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            buf: String::new(),
            done: false,
            valid_frames: 0,
            wrote_error: false,
        }
    }

    /// 已转发的有效帧数（JSON 解析成功的数据帧）。
    pub fn valid_frames(&self) -> usize {
        self.valid_frames
    }
}

impl<S> SseFramesIter<S>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    /// 取下一条待写出的 SSE 帧文本（已含 `data: ` 前缀与结尾空行）。
    ///
    /// 返回 `None` 表示流已结束（此时已按需补出 error 帧与 `[DONE]`）。
    pub async fn next_frame(&mut self) -> Option<Result<String, String>> {
        use futures_util::StreamExt;
        loop {
            // 先从缓冲里取完整行
            if let Some(idx) = self.buf.find('\n') {
                let line: String = self.buf.drain(..=idx).collect();
                let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
                if trimmed.starts_with("data: [DONE]") {
                    // 上游显式结束：DONE 之后的任何数据（含垃圾帧）一律不再透传。
                    self.done = true;
                    return Some(Ok(self.finish()));
                }
                if let Some(payload) = trimmed.strip_prefix("data: ") {
                    let (text, valid) = normalize_payload(payload);
                    if valid {
                        self.valid_frames += 1;
                    }
                    return Some(Ok(format!("data: {text}\n\n")));
                }
                if !trimmed.is_empty() {
                    // 注释/其他行：原样透传
                    return Some(Ok(format!("{trimmed}\n\n")));
                }
                // 空行（帧分隔）吞掉：本函数自产 "\n\n"
                continue;
            }

            if self.done {
                return None;
            }

            // 缓冲里没有完整行 → 再读一片
            match self.inner.next().await {
                Some(Ok(chunk)) => {
                    self.buf.push_str(&String::from_utf8_lossy(&chunk));
                }
                Some(Err(e)) => return Some(Err(e.to_string())),
                None => {
                    // 上游流结束：冲掉残余缓冲，再补收尾
                    if !self.buf.is_empty() {
                        let rest = std::mem::take(&mut self.buf);
                        let trimmed = rest.trim_end_matches(['\r', '\n']).to_string();
                        if let Some(payload) = trimmed.strip_prefix("data: ") {
                            if payload != "[DONE]" {
                                let (text, valid) = normalize_payload(payload);
                                if valid {
                                    self.valid_frames += 1;
                                }
                                return Some(Ok(format!("data: {text}\n\n")));
                            }
                        }
                    }
                    self.done = true;
                    return Some(Ok(self.finish()));
                }
            }
        }
    }

    /// 收尾：空流补 error 帧，并保证恰好一个 `[DONE]`。
    fn finish(&mut self) -> String {
        let mut out = String::new();
        if self.valid_frames == 0 && !self.wrote_error {
            // 空流（0 有效帧）：先写一帧 error（**绕过** normalize_payload，
            // 否则 error 字段会被白名单剥掉），再补 [DONE] 让客户端正常收尾。
            self.wrote_error = true;
            out.push_str("data: {\"error\":{\"message\":\"empty upstream stream\",\"type\":\"upstream_error\"}}\n\n");
        }
        out.push_str("data: [DONE]\n\n");
        out
    }
}

/// 当前 Unix 秒。
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 当前 Unix 纳秒（用于合成 id）。
fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn aggregate_basic_content() {
        let sse = concat!(
            "data: {\"id\":\"c1\",\"model\":\"m\",\"created\":1700000000,\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let out = aggregate(sse).unwrap();
        assert_eq!(out["id"], json!("c1"));
        assert_eq!(out["object"], json!("chat.completion"));
        assert_eq!(out["model"], json!("m"));
        assert_eq!(out["created"], json!(1700000000));
        assert_eq!(out["choices"][0]["message"]["content"], json!("Hello"));
        assert_eq!(out["choices"][0]["message"]["role"], json!("assistant"));
        assert_eq!(out["choices"][0]["finish_reason"], json!("stop"));
    }

    /// 空 content 的首帧不得锁死 message 回退路径。
    #[test]
    fn aggregate_empty_content_does_not_block_message_fallback() {
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"message\":{\"content\":\"full answer\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let out = aggregate(sse).unwrap();
        assert_eq!(out["choices"][0]["message"]["content"], json!("full answer"));
    }

    /// 空流（只有 [DONE]）必须报错，而不是合成空 content 的假成功。
    #[test]
    fn aggregate_empty_stream_errors() {
        let err = aggregate("data: [DONE]\n\n").unwrap_err();
        assert!(err.contains("no valid data events"), "{err}");
        assert!(aggregate("").is_err());
    }

    #[test]
    fn aggregate_reasoning_and_usage() {
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"think \"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ans\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"total_tokens\":7}}\n\n",
            "data: [DONE]\n\n",
        );
        let out = aggregate(sse).unwrap();
        assert_eq!(out["choices"][0]["message"]["reasoning_content"], json!("think "));
        assert_eq!(out["usage"]["total_tokens"], json!(7));
    }

    #[test]
    fn aggregate_merges_tool_calls_by_index() {
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"a\\\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":1}\"}}]}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let out = aggregate(sse).unwrap();
        let tc = &out["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(tc["id"], json!("call_1"));
        assert_eq!(tc["function"]["name"], json!("f"));
        assert_eq!(tc["function"]["arguments"], json!("{\"a\":1}"));
    }

    /// 字符串 index 必须与数字 index 等价处理：否则多个并行调用会全部挤进槽位 0。
    #[test]
    fn aggregate_string_index_does_not_collapse_calls() {
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":\"0\",\"id\":\"c0\",\"function\":{\"name\":\"a\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":\"1\",\"id\":\"c1\",\"function\":{\"name\":\"b\"}}]}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let out = aggregate(sse).unwrap();
        let calls = out["choices"][0]["message"]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 2, "两个并行调用不得合并: {calls:?}");
        assert_eq!(calls[0]["id"], json!("c0"));
        assert_eq!(calls[1]["id"], json!("c1"));
    }

    #[test]
    fn aggregate_ignores_data_after_done() {
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"}}]}\n\n",
            "data: [DONE]\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"GARBAGE\"}}]}\n\n",
        );
        let out = aggregate(sse).unwrap();
        assert_eq!(out["choices"][0]["message"]["content"], json!("x"));
    }

    #[test]
    fn normalize_frame_strips_noise() {
        let f = frame(
            r#"{"id":"c1","object":"chat.completion.chunk","unknown_field":123,
                "choices":[{"index":0,"delta":{"role":"assistant","content":"","tool_calls":[]},"finish_reason":""}]}"#,
        );
        let out = normalize_frame(f.as_object().unwrap());
        assert!(out.get("unknown_field").is_none(), "顶层未知字段应剔除");
        let d = &out["choices"][0]["delta"];
        assert!(d.get("content").is_none(), "空 content 应省略");
        assert!(d.get("tool_calls").is_none(), "空 tool_calls 应省略");
        assert_eq!(d["role"], json!("assistant"));
        assert_eq!(out["choices"][0]["finish_reason"], Value::Null);
        assert_eq!(out["usage"], Value::Null);
    }

    #[test]
    fn normalize_frame_keeps_error() {
        let f = frame(r#"{"error":{"message":"channel not approved","code":11128}}"#);
        let out = normalize_frame(f.as_object().unwrap());
        assert_eq!(out["error"]["code"], json!(11128));
        // 其余字段按规范补齐
        assert_eq!(out["object"], json!("chat.completion.chunk"));
        assert_eq!(out["id"], json!("chatcmpl-wb2api"));
    }

    #[test]
    fn normalize_frame_drops_empty_function_call() {
        let f = frame(
            r#"{"choices":[{"index":0,"delta":{"function_call":{"name":"","arguments":""}}}]}"#,
        );
        let out = normalize_frame(f.as_object().unwrap());
        assert!(out["choices"][0]["delta"].get("function_call").is_none());
    }

    #[test]
    fn normalize_frame_keeps_real_function_call() {
        let f = frame(r#"{"choices":[{"index":0,"delta":{"function_call":{"name":"f","arguments":"{}"}}}]}"#);
        let out = normalize_frame(f.as_object().unwrap());
        assert_eq!(out["choices"][0]["delta"]["function_call"]["name"], json!("f"));
    }

    #[test]
    fn sse_frames_filters_done_and_blank_lines() {
        let sse = concat!(
            "data: {\"a\":1}\n\n",
            ": comment line\n",
            "data: [DONE]\n\n",
            "data: {\"never\":true}\n\n",
        );
        let frames: Vec<_> = SseFrames::new(sse).collect();
        // 第一帧数据 + 一帧注释；DONE 之后不再产出
        assert_eq!(frames.len(), 2, "{frames:?}");
        assert!(frames[0].1, "JSON 帧应有效");
        assert!(frames[1].0.contains("comment"));
        assert!(
            frames.iter().all(|(p, _)| !p.contains("never")),
            "DONE 之后的帧不得透传"
        );
    }

    #[test]
    fn normalize_payload_reports_invalid_json() {
        let (payload, valid) = normalize_payload("not json");
        assert_eq!(payload, "not json");
        assert!(!valid, "非法 JSON 不得计入有效帧");
    }

    #[test]
    fn normalize_payload_rebuilds_valid_json() {
        let (payload, valid) = normalize_payload(r#"{"id":"x","choices":[]}"#);
        assert!(valid);
        let v: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["object"], json!("chat.completion.chunk"));
    }
}
