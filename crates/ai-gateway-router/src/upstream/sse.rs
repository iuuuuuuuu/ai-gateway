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
use std::time::Duration;

/// SSE 流中「有效数据帧」为零时的错误文案。
pub const EMPTY_STREAM_ERR: &str = "upstream stream contained no valid data events";

/// 首帧预读等待超时时的错误文案（Go `sse.go:475` 逐字）。
pub const PROBE_TIMEOUT_ERR: &str = "上游首帧超时";

/// 把 reqwest 的**流读取错误**翻成可读归因（⑫ 修复）。
///
/// 缺陷成因：`reqwest::Error` 在 body 层的 Display 恒为
/// `error decoding response body`（`reqwest-0.12.28/src/error.rs:233`
/// `Kind::Decode => f.write_str("error decoding response body")`），**不含 cause**；
/// 而 `Response::bytes_stream` / `Response::chunk` 把所有 body 层错误都包成
/// `Kind::Decode`（`async_impl/response.rs:356-358`、`:315-330`）。
/// 于是「流中空闲超时」与「上游提前关连接」在日志和用户文案里长得一模一样，
/// 业主 2026-10-03 看到的就是那句
/// `upstream stream ended before the completion event: error decoding response body`。
///
/// 真正的判据只有 `reqwest::Error::is_timeout()`（`error.rs:115-138`）：它沿
/// `source()` 链找 `TimedOut` / `hyper::Error::is_timeout()` /
/// `io::ErrorKind::TimedOut`。空闲超时的完整链是
/// `Kind::Decode` → `Kind::Body` → `TimedOut`
///（`async_impl/body.rs:351-353`），能穿透两层包装；上游断连则是
/// `Decode` → `Body` → `hyper::Error(IncompleteMessage)`
///（`hyper-1.11.0/src/error.rs:532` = `connection closed before message completed`）。
///
/// ⚠ 措辞硬约束：`server.rs` 会把这个字符串拼进
/// `upstream stream ended before the completion event: {…}`，而 DSH 侧的
/// `classifyPiAiError`（`@deepseek-ai/dsh-llm-pi-ai/lib/index.js:1388`）用
/// `/stream ended (?:before|without)\b/i` 把它归成 `TRANSPORT`（可重试、自动换号）。
/// 这里只改冒号**后面**的归因，前缀由调用方保留 ⇒ 归类不变。
///
/// 测试无法自造 `reqwest::Error`（`error.rs:311-325` 的构造器全是
/// `pub(crate)`），所以断言只能走**真实网络**（见本文件测试
/// `stream_errors_are_attributed_not_opaque`）。
pub fn stream_error_text(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "上游流中断：流中空闲超时（上游在配置的空闲时限内没有吐出任何数据）".to_string()
    } else {
        format!("上游流中断：{}", error_cause(e))
    }
}

/// reqwest 自身 `Kind` 的固定 Display 文案（`reqwest-0.12.28/src/error.rs:229-235`）。
///
/// 它们**只有类型信息、没有 cause** —— 业主 2026-10-03 看到的那句
/// `error decoding response body` 就是 `Kind::Decode` 的这一句。
/// 出现在 `source()` 链上时必须跳过，否则真因（hyper / io 层）会被它挡住。
///
/// 对照实现：
/// - `Kind::Request => f.write_str("error sending request")`
/// - `Kind::Body    => f.write_str("request or response body error")`
/// - `Kind::Decode  => f.write_str("error decoding response body")`
const REQWEST_KIND_NOISE: [&str; 3] = [
    "error sending request",
    "request or response body error",
    "error decoding response body",
];

/// `reqwest::Error` 的真因文案（`source()` 链上那一层）。
///
/// 取不到时（极少见）给一句保底，**不要**退回 Display —— 那正是要修的病。
fn error_cause(e: &reqwest::Error) -> String {
    // 沿 `source()` 链取最多三层（去重、并跳过 reqwest 自己的占位层）。
    // 只看第一层不够：上游提前断连时第一层是 hyper-util 的
    //「error reading a body from connection」，真正的
    //「connection closed before message completed」在第二层
    //（`hyper-1.11.0/src/error.rs:532`）。
    let mut chain: Vec<String> = Vec::new();
    let mut cur = std::error::Error::source(e);
    while let Some(err) = cur {
        let text = err.to_string();
        if !text.is_empty()
            && !REQWEST_KIND_NOISE.contains(&text.as_str())
            && !chain.iter().any(|c| c == &text)
        {
            chain.push(text);
        }
        if chain.len() >= 3 {
            break;
        }
        cur = err.source();
    }
    if chain.is_empty() {
        "连接被对端关闭".to_string()
    } else {
        chain.join(" ← ")
    }
}

/// 首帧预读读到一半就出错时的归因文案（只进日志，见 `forward.rs` 的 probe 消费点）。
///
/// 与 `stream_error_text` 分开是因为语境不同：预读阶段**一帧都没拿到**，
/// 「流中空闲超时」会被误读成「答到一半卡住了」。
fn probe_error_text(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "上游流中断：等待首帧时空闲超时（上游一直没吐数据）".to_string()
    } else {
        format!("上游流中断：{}", error_cause(e))
    }
}

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

/// 归一化后的帧文本里是否带**非空** `finish_reason`（`choices[].finish_reason`）。
///
/// ★ 必须在 [`normalize_frame`] **之后**判：归一化会给缺键的帧补上
/// `"finish_reason": null`，所以「键存在」不等于「上游宣告了完成」。
fn payload_has_finish_reason(text: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    let Some(choices) = v.get("choices").and_then(|c| c.as_array()) else {
        return false;
    };
    choices.iter().any(|c| {
        c.get("finish_reason")
            .and_then(|f| f.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    })
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
    /// 是否读到过规范的 `[DONE]` 结束帧。
    ///
    /// 这是「上游按约定收尾了」的**权威标志**。不能用 `finish_reason` 代替：
    /// 上游是在给出 `finish_reason` **之后**才写 `[DONE]` 的，而那一瞬恰恰是
    /// 空闲超时最容易掐断的位置（`usage` 也还没落下来）。所以见过
    /// `finish_reason` 不等于流正常结束。
    saw_done: bool,
    /// 是否读到过**非空**的 `finish_reason`（上游已宣告本轮答完）。
    ///
    /// 与 [`SseFramesIter::saw_done`] 的分工：`[DONE]` 仍是「流正常收尾」的
    /// 权威标志，但上游是**先**给 `finish_reason`、**后**写 `[DONE]`，两者之间
    /// 那一瞬恰是连接最容易被掐断的位置（`usage` 也还没落下来）。实测业主现场
    /// （2026-10-07，27 个账号）的断流里就有「内容已吐完、`finish_reason` 已到，
    /// 只差 `[DONE]`」这一形态 —— 那时若仍按失败收尾，用户看到的是回答其实已经
    /// 完整、却被标成「本轮运行失败」。
    ///
    /// 因此这个标志只用来**放宽**收尾判定（见 [`SseFramesIter::premature_end`]），
    /// 不用来宣告成功：真失败（吐了一半就断）时它仍然是 false。
    saw_finish_reason: bool,
    /// 是否读到过任何一个可解析的 data 帧。
    ///
    /// 用于区分「空流」（上游正常解析后发现没什么可发，客户端得到一个合法的
    /// 空回合，无损）与「吐了一半就断」（必须按失败收尾）。
    saw_any_frame: bool,
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
            saw_done: false,
            saw_finish_reason: false,
            saw_any_frame: false,
        }
    }

    /// 把首帧预读**已经消费掉**的字节交回缓冲，等价于它们还在 `inner` 里。
    ///
    /// 必须调、且必须在读第一帧之前调：预读只消费了一部分字节，丢掉就会丢首帧，
    /// 表现为回答少开头几个字（Go `sse.go:383-392` 逐字警告）。
    ///
    /// 为什么用「灌缓冲」而不是重建 `reqwest::Response`：重建只有
    /// `From<http::Response<T>>` 一条公开路径，而它**不传 `read_timeout`** ——
    /// 长回答会永久挂住（`reqwest-0.12.28\src\async_impl\response.rs:36-52` 的
    /// `new()` 是私有的）。灌缓冲则完全绕开这一点：body 还是原来那个，
    /// 空闲超时照旧生效。
    pub fn push_prefix(&mut self, prefix: &str) {
        if prefix.is_empty() {
            return;
        }
        // 预读阶段是从头开始读的，所以这里必须是「前置」而不是「追加」。
        // 正常情况下 `buf` 还是空的；非空时把已缓冲内容接在后面，保持字节顺序。
        if self.buf.is_empty() {
            self.buf.push_str(prefix);
        } else {
            let mut merged = String::with_capacity(prefix.len() + self.buf.len());
            merged.push_str(prefix);
            merged.push_str(&self.buf);
            self.buf = merged;
        }
    }

    /// 已转发的有效帧数（JSON 解析成功的数据帧）。
    pub fn valid_frames(&self) -> usize {
        self.valid_frames
    }

    /// 是否读到过 `[DONE]`。
    pub fn saw_done(&self) -> bool {
        self.saw_done
    }

    /// 是否读到过非空的 `finish_reason`（上游已宣告本轮答完，但可能还没写 `[DONE]`）。
    pub fn saw_finish_reason(&self) -> bool {
        self.saw_finish_reason
    }

    /// 是否读到过任何一个可解析的 data 帧。
    pub fn saw_any_frame(&self) -> bool {
        self.saw_any_frame
    }

    /// 判定一次上游流是否应当按**失败**收尾，返回原因（`None` = 可正常收尾）。
    ///
    /// 四种情形（前三条与 Go 侧 `streamFailureMessage` 同一口径，第四条是本仓
    /// 2026-10-07 追加的放宽）：
    ///
    /// - 见过 `[DONE]` → 正常收尾
    /// - 没见过 `[DONE]`，且一个 data 帧都没读到（空流）→ 正常收尾
    ///   （上游正常解析后发现没什么可发时就是这样，客户端得到合法空回合，无损）
    /// - 没见过 `[DONE]`，但已经吐过内容 → **失败**
    ///   （IdleTimeout 取消连接、或上游提前关连接的形态；补一个
    ///   `stop_reason=end_turn` 等于告诉客户端「模型答完了」，它会据此推进对话，
    ///   用户只看到回答被从中间截断）
    /// - 见过**非空** `finish_reason`、只差 `[DONE]` → 正常收尾
    ///   （见 [`SseFramesIter::saw_finish_reason`]；上游已经宣告本轮答完，内容完整，
    ///   少的只是那个终止字节。此时仍按失败收尾会让用户看到「回答写完了却报
    ///   本轮运行失败」，而客户端拿到 error 帧后只能整轮重试 —— 重试会重新生成，
    ///   比丢掉一个终止字节昂贵得多）
    pub fn premature_end(&self) -> Option<&'static str> {
        if self.saw_done || !self.saw_any_frame || self.saw_finish_reason {
            return None;
        }
        Some("upstream stream ended unexpectedly before [DONE]")
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
                    // 记下 saw_done —— 它是「流正常结束」的权威标志。
                    self.saw_done = true;
                    self.done = true;
                    return Some(Ok(self.finish()));
                }
                if let Some(payload) = trimmed.strip_prefix("data: ") {
                    let (text, valid) = normalize_payload(payload);
                    if valid {
                        self.valid_frames += 1;
                        self.saw_any_frame = true;
                        if payload_has_finish_reason(&text) {
                            self.saw_finish_reason = true;
                        }
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
                Some(Err(e)) => return Some(Err(stream_error_text(&e))),
                None => {
                    // 上游流结束：冲掉残余缓冲，再补收尾
                    if !self.buf.is_empty() {
                        let rest = std::mem::take(&mut self.buf);
                        let trimmed = rest.trim_end_matches(['\r', '\n']).to_string();
                        if let Some(payload) = trimmed.strip_prefix("data: ") {
                            if payload == "[DONE]" {
                                // 残缓冲里就是结束帧（上游没写尾随换行）。
                                self.saw_done = true;
                            } else {
                                let (text, valid) = normalize_payload(payload);
                                if valid {
                                    self.valid_frames += 1;
                                    self.saw_any_frame = true;
                                    if payload_has_finish_reason(&text) {
                                        self.saw_finish_reason = true;
                                    }
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

// ───────────────────────────────────────────────────────────────────────────
// 首帧预读（对应 Go `internal/upstream/sse.go:383-700`）
// ───────────────────────────────────────────────────────────────────────────

/// 判断一行 SSE 文本是否是**有效数据帧**（Go `ValidSSEFrame`）。
///
/// 口径必须与 [`SseFramesIter::next_frame`] 的有效帧计数**完全一致**
///（Go `sse.go:479-487` 逐字）：JSON 能解析成对象即算有效，`[DONE]` 不算 ——
/// 它只表示结束，不代表有内容。
///
/// ⚠ 为什么口径必须一致：两者不一致会出现「probe 说有效、Stream 说空」的
/// 自相矛盾，用户看到「明明探测通过却收到 empty upstream stream」。真实缺陷
///（所有者 2026-09-20 现场：装了新版仍报 `empty upstream stream`）：旧实现只认
/// `data:` 前缀，于是 `data: [DONE]`（或任何非 JSON 的 data 行）让 **probe
/// 放行、Stream 判空**，而 200 已经写出去了、**再也无法换号**。
///
/// ⚠ 用 `"data:"`（**不带空格**）再 `trim_start`，比聚合路径的 `"data: "` 宽 ——
/// 上游确实会发 `data:{...}` 这种没有空格的形态。
pub fn valid_sse_frame(line: &str) -> bool {
    let Some(rest) = line.trim().strip_prefix("data:") else {
        return false;
    };
    let d = rest.trim();
    if d.is_empty() || d == "[DONE]" {
        return false;
    }
    // ★ 只认顶层对象，与 `normalize_payload` 同一判据（`data: null` 不算有效帧，
    // 因为 `next_frame` 也不会把它计成有效帧）。
    matches!(serde_json::from_str::<Value>(d), Ok(Value::Object(_)))
}

/// 判断一帧是否是上游的**错误帧**，返回 `(是否错误, 可读原因)`。
///
/// 对应 Go `IsUpstreamErrorFrame`（`sse.go:513-578`）。上游常用
/// `HTTP 200 + {"error":{...}}` 表达失败（ZCode 的 code=3012/3007、WorkBuddy
/// 的 11128 渠道未批准）。
///
/// ⚠ 不能只看「有没有 error 字段」就判坏：某些上游在正常流里也会带非致命
/// error 字段，故额外要求它**不含任何 choices 内容**。没有 `error` 字段时
/// 退到形状②（[`top_level_error_frame`]）。
pub fn is_upstream_error_frame(line: &str) -> (bool, String) {
    let Some(rest) = line.trim().strip_prefix("data:") else {
        return (false, String::new());
    };
    let d = rest.trim();
    if d.is_empty() || d == "[DONE]" {
        return (false, String::new());
    }
    let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(d) else {
        return (false, String::new());
    };

    let Some(e) = obj.get("error").filter(|v| !v.is_null()) else {
        // 形状②：错误在顶层，没有 error 包装。
        return top_level_error_frame(&obj);
    };

    // 有实际产出就不算失败（上游会在正常帧里捎带非致命 error）。
    if let Some(Value::Array(chs)) = obj.get("choices") {
        for c in chs {
            let Value::Object(cm) = c else { continue };
            if let Some(Value::Object(delta)) = cm.get("delta") {
                if let Some(Value::String(s)) = delta.get("content") {
                    if !s.is_empty() {
                        return (false, String::new());
                    }
                }
            }
            if let Some(Value::String(fr)) = cm.get("finish_reason") {
                if !fr.is_empty() {
                    return (false, String::new());
                }
            }
        }
    }

    let mut msg = String::new();
    match e {
        Value::String(s) => msg = s.clone(),
        Value::Object(m) => {
            if let Some(Value::String(s)) = m.get("message") {
                msg = s.clone();
            }
            if msg.is_empty() {
                if let Some(c) = m.get("code") {
                    msg = format!("code={}", go_scalar(c));
                }
            }
        }
        _ => {}
    }
    (true, msg)
}

/// 形状②：错误在**顶层**，没有 `error` 包装（Go `topLevelErrorFrame`，`sse.go:598-640`）。
///
/// 三重否定 —— 这一帧必须**既不是内容、也不是用量**（没 `body` → 不是嵌套内容帧；
/// 没 `choices` → 不是直接 OpenAI 内容帧；没 `usage` → 不是用量帧）。三者都没有时
/// 这一帧在 `Stream` 里**本来就会被忽略**（不计入有效帧计数），故把它判成错误只
/// 可能比「当作未知帧跳过」更好，**不存在误伤正常帧的风险**（由 4 条 Go 单测锁定，
/// 见本文件 `top_level_error_frame_*` 系列）。另外排除 `code` 为 `0`/`200` 的
///「成功」语义。
///
/// 形状②实测样本（2026-09-20，官方 Qoder CN 客户端日志）：免费模型排队时上游在
/// HTTP **200** 的流**首帧**下发
/// `{"code":"403","message":"{\"code\":\"10605\",\"message\":\"{\\\"isQueued\\\":true,
/// \\\"queueCount\\\":7309,\\\"waitTime\\\":206}\"}"}` —— 它**没有 error 字段**，
/// 旧实现返回 false ⇒ 探测认为首帧正常 ⇒ 照常写 200 把流交给客户端 ⇒ 客户端进入
/// 对话流程后才发现是错误。所有者原话：「如果需要排队,直接就报错出来,不要等待对话」。
fn top_level_error_frame(obj: &Map<String, Value>) -> (bool, String) {
    if obj.contains_key("body") || obj.contains_key("choices") || obj.contains_key("usage") {
        return (false, String::new());
    }
    let Some(raw) = obj.get("code").filter(|v| !v.is_null()) else {
        return (false, String::new());
    };
    let code = go_scalar(raw);
    let code = code.trim().trim_matches('"');
    if code.is_empty() || code == "0" || code == "200" {
        return (false, String::new());
    }

    let mut msg = String::new();
    if let Some(Value::String(s)) = obj.get("message") {
        msg = s.clone();
    }
    if msg.is_empty() {
        if let Some(Value::String(s)) = obj.get("msg") {
            msg = s.clone();
        }
    }
    if msg.is_empty() {
        return (true, format!("code={code}"));
    }
    // ⚠ 渲染不出来就**退回原文**（Go `sse.go:635`）—— 宁可难看，也不能吞掉信息。
    match humanize_upstream_message(&msg) {
        Some(human) => (true, human),
        None => (true, msg),
    }
}

/// 把上游嵌套的排队 / 限流消息渲染成人话；渲染不出来返回 `None`（Go
/// `humanizeUpstreamMessage`，`sse.go:650-685`）。
///
/// 与 `crate::products::qoder` 里同名逻辑的关系（Go `sse.go:642-649` 逐字）：
/// 那份作用在**流翻译层**（客户端已经在对话里了），这份作用在**首帧探测层**
///（还没写响应头，可以直接报错）。两层都要，因为两层各自可能先拿到这个帧。
///
/// 最多剥 **4** 层 —— 够用，且防异常自引用串把循环挂住。
pub fn humanize_upstream_message(msg: &str) -> Option<String> {
    let mut cur = msg.to_string();
    for _ in 0..4 {
        let Ok(Value::Object(m)) = serde_json::from_str::<Value>(&cur) else {
            return None;
        };
        if let Some(Value::Bool(true)) = m.get("isQueued") {
            let mut b = String::from("免费模型正在排队（上游主动限流，非故障）");
            if let Some(n) = int_field_of(&m, "queueCount") {
                b.push_str(&format!("：前面约 {n} 个请求"));
            }
            if let Some(w) = int_field_of(&m, "waitTime") {
                b.push_str(&format!("，预计等待约 {w} 秒"));
            }
            if let Some(Value::String(k)) = m.get("modelKey") {
                if !k.is_empty() {
                    b.push_str(&format!("（模型 {k}）"));
                }
            }
            if let Some(r) = int_field_of(&m, "retryAfterSeconds") {
                b.push_str(&format!("。建议 {r} 秒后重试"));
            }
            if let Some(Value::Bool(true)) = m.get("serviceAvailable") {
                b.push_str("；上游服务状态正常");
            }
            return Some(b);
        }
        match m.get("message") {
            Some(Value::String(s)) if !s.trim().is_empty() => cur = s.clone(),
            _ => return None,
        }
    }
    None
}

/// 取一个整数字段（Go `intFieldOf`，`sse.go:688-700`）。
fn int_field_of(m: &Map<String, Value>, key: &str) -> Option<i64> {
    match m.get(key) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
}

/// 复刻 Go 的 `fmt.Sprintf("%v", v)` 对 JSON 标量的输出（数字/字符串不带引号）。
fn go_scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "<nil>".to_string(),
        other => other.to_string(),
    }
}

/// 首帧预读的结果。
pub struct ProbeOutcome {
    /// 已经从上游流里消费掉的**原始字节**（含每一行的行尾换行）。
    ///
    /// ★★ 必须原样交回给下游 —— 流式走 [`SseFramesIter::push_prefix`]，
    /// 非流式直接拼在 `resp.text()` 前面。丢掉它就等于丢掉回答的开头几个字。
    pub consumed: String,
    /// 探到的首帧行（含行尾换行；`""` = 没探到）。
    pub first: String,
    /// 预读失败的原因（超时 / 上游流报错 / 流提前结束）。`None` = 读到了首帧。
    ///
    /// ⚠ 调用方**不要**把它的文本直接给用户：Go `forward.go:916` 把
    /// `perr != nil` 与 `first == ""` 归成同一句「上游返回空流（无有效数据帧）」。
    /// 它只用于日志。
    pub error: Option<String>,
}

/// 只预读**一帧**：在写响应头之前确认这条流真的有内容（Go `ProbeFirstFrame`，
/// `sse.go:393-477`）。
///
/// # 为什么必须有这一步
///
/// 上游有一种失败形态是 **HTTP 200 + 空流**（ZCode 对话通道对受限账号返回 200，
/// 流里一帧有效数据都没有，实测 code=3012 "unusual activity"）。不预读就直接
/// 把流交给调用方，后果有两个：① 用户收到 `empty upstream stream`，而 200 已经
/// 写出去了，**再也无法换账号重试**；② 坏账号被记成「成功」、**永不冷却** ⇒
/// 每次请求都选中它 ⇒ 用户的对话**持续失败**。
///
/// 只预读一帧而不是整个流：足够区分「空流」与「正常流」，又不破坏流式体验
///（首帧延迟只增加一个上游 TTFB）。
///
/// # 实现要点
///
/// - 用 [`reqwest::Response::chunk`]，它读的是**原始 body**，因此
///   `read_timeout`（空闲超时）仍然生效；**不要**改用
///   `Response::from(http::Response::new(...))` 重建 —— 那个 `From` 实现不传
///   `read_timeout`，长回答会永久挂住。
/// - `consumed` 累积**所有**读到的字节（可能比首帧多出一点，因为网络分片会
///   越过帧边界）。多读的部分一并交回下游，顺序不变，因此无害。
/// - 超时后不再读：`chunk()` 的 future 被 drop，已 append 的字节是确定的，
///   不存在 Go 那边「读协程仍在写 `consumed`」的数据竞争。
pub async fn probe_first_frame(resp: &mut reqwest::Response, timeout: Duration) -> ProbeOutcome {
    /// 单行上限。Go 的 `bufio.ReadString` 会无限增长，这里给一个远高于真实帧
    ///（通常几 KB）的上限，避免畸形流把内存吃光。
    const MAX_PROBE_BYTES: usize = 4 * 1024 * 1024;

    let deadline = tokio::time::Instant::now() + timeout;
    let mut consumed: Vec<u8> = Vec::new();

    loop {
        // 先看已读字节里有没有「完整且有效」的帧。每次从头扫 —— 数据量很小，
        // 换来的是与 Go「逐行读、逐行判」完全等价的语义。
        if let Some(line) = first_valid_line(&consumed) {
            return ProbeOutcome {
                consumed: String::from_utf8_lossy(&consumed).into_owned(),
                first: line,
                error: None,
            };
        }

        if consumed.len() > MAX_PROBE_BYTES {
            return ProbeOutcome {
                consumed: String::from_utf8_lossy(&consumed).into_owned(),
                first: String::new(),
                error: Some(format!("上游首帧超过 {MAX_PROBE_BYTES} 字节仍无有效数据帧")),
            };
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return ProbeOutcome {
                consumed: String::from_utf8_lossy(&consumed).into_owned(),
                first: String::new(),
                error: Some(PROBE_TIMEOUT_ERR.to_string()),
            };
        }

        match tokio::time::timeout(remaining, resp.chunk()).await {
            Err(_) => {
                return ProbeOutcome {
                    consumed: String::from_utf8_lossy(&consumed).into_owned(),
                    first: String::new(),
                    error: Some(PROBE_TIMEOUT_ERR.to_string()),
                }
            }
            Ok(Ok(Some(bytes))) => consumed.extend_from_slice(&bytes),
            // 上游把流关了都没给出一帧有效数据。
            Ok(Ok(None)) => {
                return ProbeOutcome {
                    consumed: String::from_utf8_lossy(&consumed).into_owned(),
                    first: String::new(),
                    error: Some("upstream closed before first frame".to_string()),
                }
            }
            Ok(Err(e)) => {
                return ProbeOutcome {
                    consumed: String::from_utf8_lossy(&consumed).into_owned(),
                    first: String::new(),
                    error: Some(probe_error_text(&e)),
                }
            }
        }
    }
}

/// 在已读字节里找第一行有效数据帧（返回含行尾换行的整行）。
fn first_valid_line(buf: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(buf);
    let mut start = 0usize;
    for (i, ch) in text.char_indices() {
        if ch != '\n' {
            continue;
        }
        let line = &text[start..=i];
        if valid_sse_frame(line) {
            return Some(line.to_string());
        }
        start = i + 1;
    }
    None
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

    // ── 首帧预读 ────────────────────────────────────────────────────────────

    /// ★★ 口径必须与 `next_frame` 的有效帧计数一致：`[DONE]` 不算有效帧。
    ///
    /// 真实缺陷（所有者 2026-09-20 现场）：旧实现只认 `data:` 前缀，于是
    /// `data: [DONE]` 让 probe 放行、`Stream` 判空，而 200 已经写出去、再也无法换号。
    #[test]
    fn valid_sse_frame_rejects_done_and_non_json() {
        assert!(valid_sse_frame("data: {\"choices\":[]}"));
        // 没有空格也要认（上游确实会发这种）。
        assert!(valid_sse_frame("data:{\"choices\":[]}"));
        // 前后有空白也要认。
        assert!(valid_sse_frame("  data: {\"choices\":[]}  "));
        // 心跳注释行不是数据帧。
        assert!(!valid_sse_frame(": heartbeat"));
        assert!(!valid_sse_frame(""));
        // ★ [DONE] 只表示结束，不代表有内容。
        assert!(!valid_sse_frame("data: [DONE]"));
        assert!(!valid_sse_frame("data:[DONE]"));
        assert!(!valid_sse_frame("data: "));
        // 非对象 JSON 也不算（与 normalize_payload 同判据）。
        assert!(!valid_sse_frame("data: not json"));
        assert!(!valid_sse_frame("data: null"));
        assert!(!valid_sse_frame("data: [1,2]"));
    }

    /// 上游用 `HTTP 200 + {"error":{...}}` 表达失败（ZCode 3012/3007、
    /// WorkBuddy 11128 渠道未批准）。
    #[test]
    fn error_frame_detected_for_error_wrapper() {
        let (bad, msg) = is_upstream_error_frame(
            "data: {\"error\":{\"code\":\"3012\",\"message\":\"unusual activity\"}}",
        );
        assert!(bad);
        assert_eq!(msg, "unusual activity");
    }

    /// ⚠ 不能只看「有没有 error 字段」就判坏：某些上游在正常流里也会带非致命
    /// error 字段，故额外要求它**不含任何 choices 内容**。
    #[test]
    fn error_field_with_real_content_is_not_an_error_frame() {
        let (bad, _) = is_upstream_error_frame(
            "data: {\"error\":{\"message\":\"non-fatal\"},\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}",
        );
        assert!(!bad, "有实际产出就不算失败");

        let (bad, _) = is_upstream_error_frame(
            "data: {\"error\":{\"message\":\"non-fatal\"},\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}",
        );
        assert!(!bad, "有 finish_reason 也算产出");
    }

    /// ★★ 形状②实测样本（2026-09-20 官方 Qoder CN 客户端日志）：免费模型排队时
    /// 上游在 HTTP **200** 的流**首帧**下发，它**没有 error 字段** ⇒ 旧实现返回
    /// false ⇒ 探测认为首帧正常 ⇒ 客户端进入对话流程后才发现是错误。
    /// 所有者原话：「如果需要排队,直接就报错出来,不要等待对话」。
    #[test]
    fn top_level_error_frame_peels_nested_queue_message() {
        let line = "data: {\"code\":\"403\",\"message\":\"{\\\"code\\\":\\\"10605\\\",\\\"message\\\":\\\"{\\\\\\\"isQueued\\\\\\\":true,\\\\\\\"queueCount\\\\\\\":7309,\\\\\\\"waitTime\\\\\\\":206}\\\"}\"}";
        let (bad, msg) = is_upstream_error_frame(line);
        assert!(bad, "顶层 code=403 必须判成错误帧");
        assert!(msg.contains("免费模型正在排队"), "{msg}");
        assert!(msg.contains("7309"), "{msg}");
        assert!(msg.contains("206"), "{msg}");
    }

    /// 三重否定：有 body / choices / usage 的帧绝不能误判成错误帧
    ///（这类帧在 `Stream` 里本来就会被忽略，判错反而会误伤）。
    #[test]
    fn top_level_error_frame_never_misfires_on_real_frames() {
        // 嵌套内容帧（Qoder 形状：body 里再包一层 JSON 字符串）
        let (bad, _) = is_upstream_error_frame(
            "data: {\"code\":\"0\",\"body\":\"{\\\"choices\\\":[{\\\"delta\\\":{\\\"content\\\":\\\"x\\\"}}]}\"}",
        );
        assert!(!bad, "有 body 不是错误帧");
        // 直接 OpenAI 内容帧
        let (bad, _) = is_upstream_error_frame(
            "data: {\"code\":\"500\",\"choices\":[{\"delta\":{\"content\":\"x\"}}]}",
        );
        assert!(!bad, "有 choices 不是错误帧");
        // 用量帧
        let (bad, _) = is_upstream_error_frame(
            "data: {\"code\":\"500\",\"usage\":{\"prompt_tokens\":1}}",
        );
        assert!(!bad, "有 usage 不是错误帧");
        // code=0 / code=200 是「成功」语义
        let (bad, _) = is_upstream_error_frame("data: {\"code\":\"0\",\"message\":\"ok\"}");
        assert!(!bad, "code=0 不是错误");
        let (bad, _) = is_upstream_error_frame("data: {\"code\":200,\"message\":\"ok\"}");
        assert!(!bad, "code=200 不是错误");
        // 没有 code 字段
        let (bad, _) = is_upstream_error_frame("data: {\"message\":\"no code\"}");
        assert!(!bad, "没有 code 不是错误帧");
    }

    /// ⚠ 渲染不出来就**退回原文**（Go `sse.go:635`）—— 宁可难看，也不能吞掉信息。
    #[test]
    fn humanize_falls_back_to_raw_text() {
        assert_eq!(humanize_upstream_message("plain text error"), None);
        // 嵌套但没有 isQueued：一直剥到剥不动，返回 None。
        assert_eq!(
            humanize_upstream_message("{\"message\":\"{\\\"message\\\":\\\"leaf\\\"}\"}"),
            None
        );
        // 真实排队消息
        let human = humanize_upstream_message(
            "{\"isQueued\":true,\"queueCount\":3,\"waitTime\":9,\"modelKey\":\"qwen3\",\"retryAfterSeconds\":9,\"serviceAvailable\":true}",
        )
        .expect("排队消息应能渲染");
        assert!(human.contains("前面约 3 个请求"), "{human}");
        assert!(human.contains("预计等待约 9 秒"), "{human}");
        assert!(human.contains("（模型 qwen3）"), "{human}");
        assert!(human.contains("建议 9 秒后重试"), "{human}");
        assert!(human.contains("上游服务状态正常"), "{human}");
    }

    #[test]
    fn top_level_error_frame_reports_bare_code() {
        let (bad, msg) = is_upstream_error_frame("data: {\"code\":\"11128\"}");
        assert!(bad);
        assert_eq!(msg, "code=11128");
    }

    #[test]
    fn int_field_of_accepts_numbers_only() {
        let Value::Object(m) = frame(r#"{"a":3,"b":"3","c":3.7,"d":null}"#) else {
            panic!()
        };
        assert_eq!(int_field_of(&m, "a"), Some(3));
        assert_eq!(int_field_of(&m, "b"), None, "字符串不是数字");
        assert_eq!(int_field_of(&m, "c"), Some(3), "浮点截断");
        assert_eq!(int_field_of(&m, "d"), None);
        assert_eq!(int_field_of(&m, "missing"), None);
    }

    /// ★★ 预读必须**逐字节**保留已消费的字节（含行尾换行）—— 少一个 `\n` 会让
    /// `Stream` 把相邻帧粘连，表现为「工具调用参数不完整」「工具块数=0」这类
    /// 看似无关的解析错误（Go `sse.go:451-459` 逐字）。
    #[test]
    fn first_valid_line_keeps_trailing_newline() {
        let buf = b": heartbeat\ndata: {\"choices\":[]}\n";
        let line = first_valid_line(buf).expect("应找到有效帧");
        assert_eq!(line, "data: {\"choices\":[]}\n", "必须含行尾换行");
    }

    #[test]
    fn first_valid_line_ignores_done_only_stream() {
        assert_eq!(first_valid_line(b"data: [DONE]\n\n"), None);
    }

    /// `push_prefix` 必须把预读字节**前置**，且与后续帧无缝拼接。
    #[tokio::test]
    async fn push_prefix_prepends_consumed_bytes() {
        let consumed = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n";
        let mut frames = SseFramesIter::new(futures_util::stream::empty::<
            Result<bytes::Bytes, reqwest::Error>,
        >());
        frames.push_prefix(consumed);
        frames.push_prefix(""); // 空前缀是 no-op
        let first = frames
            .next_frame()
            .await
            .expect("应有帧")
            .expect("应有效");
        assert!(first.contains("Hel"), "{first}");
        assert!(first.starts_with("data: "), "{first}");
    }

    /// ★★ 预读与流式解析的口径必须一致 —— 否则会出现「探测通过却报空流」。
    /// 这里把 `valid_sse_frame` 认下的帧喂给 `SseFramesIter`，确认它确实计入
    /// 有效帧（而不是被当成未知帧丢掉）。
    #[tokio::test]
    async fn probe_acceptance_matches_stream_frame_counting() {
        let consumed = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n";
        assert!(valid_sse_frame(consumed), "预读必须认这帧");
        let mut frames = SseFramesIter::new(futures_util::stream::empty::<
            Result<bytes::Bytes, reqwest::Error>,
        >());
        frames.push_prefix(consumed);
        while frames.next_frame().await.is_some() {}
        assert_eq!(frames.valid_frames(), 1, "流式解析也必须认这帧");
        assert!(frames.saw_any_frame());
        assert!(frames.premature_end().is_none() || frames.valid_frames() > 0);
    }

    /// 用真实 `reqwest::Response` 造一个可控的分片流。
    ///
    /// 这里走 `From<http::Response<T>>` 只是**测试**手段：它在生产代码里不可用，
    /// 因为那条路径不传 `read_timeout`（长回答会永久挂住）。预读本身读的是
    /// `Response::chunk`，对两种构造方式一视同仁。
    fn probe_response(chunks: Vec<String>) -> reqwest::Response {
        let stream = futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<bytes::Bytes, std::io::Error>(bytes::Bytes::from(c))),
        );
        reqwest::Response::from(axum::http::Response::new(reqwest::Body::wrap_stream(stream)))
    }

    /// ★★ 核心不变式：`consumed` + 剩余流 == 原始字节流（逐字节）。
    ///
    /// 少一个 `\n` 就会让 `Stream` 把相邻帧粘连，表现为「工具调用参数不完整」
    ///「工具块数=0」这类看似无关的解析错误（Go `sse.go:451-459` 逐字）。
    #[tokio::test]
    async fn probe_replays_bytes_identically() {
        let original = concat!(
            ": heartbeat\n",
            "\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n",
            "\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n",
            "\n",
            "data: [DONE]\n",
            "\n",
        );
        // 故意切在奇怪的位置：注释行、帧中间、跨帧边界。
        let chunks = vec![
            original[..12].to_string(),
            original[12..48].to_string(),
            original[48..].to_string(),
        ];
        let mut resp = probe_response(chunks);
        let out = probe_first_frame(&mut resp, Duration::from_secs(5)).await;

        assert!(out.error.is_none(), "{:?}", out.error);
        assert!(valid_sse_frame(&out.first), "{:?}", out.first);
        assert!(out.first.contains("Hel"), "{:?}", out.first);

        let rest = resp.text().await.unwrap();
        let replayed = format!("{}{}", out.consumed, rest);
        assert_eq!(replayed, original, "重放必须与原始字节流逐字节相同");
    }

    /// 空流：一帧有效数据都没有 ⇒ 必须报错，好让调用方在写响应头之前换账号。
    #[tokio::test]
    async fn probe_empty_stream_reports_error() {
        let mut resp = probe_response(vec![String::new()]);
        let out = probe_first_frame(&mut resp, Duration::from_secs(5)).await;
        assert!(out.first.is_empty());
        assert!(out.error.is_some(), "空流必须报错");
    }

    /// 只有 `[DONE]` 的流同样不算「有内容」—— 这正是旧实现 probe 放行、
    /// `Stream` 判空的那个缺陷形态。
    #[tokio::test]
    async fn probe_done_only_stream_reports_error() {
        let mut resp = probe_response(vec!["data: [DONE]\n\n".to_string()]);
        let out = probe_first_frame(&mut resp, Duration::from_secs(5)).await;
        assert!(out.first.is_empty(), "只有 [DONE] 不算有效帧");
        assert!(out.error.is_some());
    }

    /// 上游一直不吐字节（ZCode 受限账号的典型形态）⇒ 超时，且文案与 Go 一致。
    #[tokio::test]
    async fn probe_timeout_reports_chinese_message() {
        let stream = futures_util::stream::pending::<Result<bytes::Bytes, std::io::Error>>();
        let mut resp =
            reqwest::Response::from(axum::http::Response::new(reqwest::Body::wrap_stream(stream)));
        let out = probe_first_frame(&mut resp, Duration::from_millis(30)).await;
        assert_eq!(out.error.as_deref(), Some(PROBE_TIMEOUT_ERR));
        assert!(out.first.is_empty());
    }

    /// 首帧即错误帧的形态：预读只负责把它取出来，判定交给调用方。
    #[tokio::test]
    async fn probe_surfaces_error_frame_for_caller() {
        let mut resp = probe_response(vec![
            "data: {\"error\":{\"code\":\"3012\",\"message\":\"unusual activity\"}}\n\n".to_string(),
        ]);
        let out = probe_first_frame(&mut resp, Duration::from_secs(5)).await;
        assert!(out.error.is_none());
        let (bad, msg) = is_upstream_error_frame(&out.first);
        assert!(bad, "调用方应能据此判定失败");
        assert_eq!(msg, "unusual activity");
    }

    /// 真实 TCP 假上游：接受一次连接、读掉请求头，然后按 `respond` 回包、
    /// 按 `finish` 处置连接，返回可用的 base URL。
    ///
    /// 为什么非要走真实网络：**测试无法自造 `reqwest::Error`**
    /// （`reqwest-0.12.28/src/error.rs:311-325` 的 `builder`/`body`/`decode`
    /// 全是 `pub(crate)`），而 ⑫ 的判据 `is_timeout()` 只有打在真的错误上才有意义。
    fn spawn_stub_upstream<F, G>(respond: F, finish: G) -> String
    where
        F: FnOnce(&mut std::net::TcpStream) + Send + 'static,
        G: FnOnce(std::net::TcpStream) + Send + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let addr = listener.local_addr().expect("取本地地址");
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = std::io::Read::read(&mut sock, &mut buf);
                respond(&mut sock);
                finish(sock);
            }
        });
        format!("http://{addr}")
    }

    /// 读到**第一条**流读取错误就停（生产调用点三处都在第一个 `Err` 后 break/return；
    /// 且错误后流未置 `done`，继续读会重复报同一条）。
    async fn first_error(
        mut frames: SseFramesIter<
            impl futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
        >,
    ) -> Option<String> {
        while let Some(item) = frames.next_frame().await {
            if let Err(e) = item {
                return Some(e);
            }
        }
        None
    }

    /// ★★★ ⑫ 回归：上游提前关连接时，必须报出**真因**，不能再是 reqwest 那句
    /// 无 cause 的 `error decoding response body`（业主 2026-10-03 报的原文是
    /// `upstream stream ended before the completion event: error decoding response body`）。
    #[tokio::test]
    async fn premature_close_is_attributed_not_opaque() {
        let base = spawn_stub_upstream(
            |sock| {
                use std::io::Write as _;
                let _ = sock.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 100\r\n\r\n",
                );
                let _ = sock.write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"half\"}}]}\n\n");
                let _ = sock.flush();
            },
            |sock| {
                // 半关写端：对端读端看到 EOF，而 content-length=100 只给了 ~50 字节
                // ⇒ hyper 报 IncompleteMessage（**不是**超时）。
                let _ = sock.shutdown(std::net::Shutdown::Write);
                std::thread::sleep(Duration::from_millis(500));
            },
        );
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("构造客户端");
        let resp = client
            .get(format!("{base}/stream"))
            .send()
            .await
            .expect("错误应发生在读 body 时，而不是建连时");
        let msg = first_error(SseFramesIter::new(resp.bytes_stream()))
            .await
            .expect("半途断流必须报错");
        assert!(
            !msg.contains("error decoding response body"),
            "⑫ 回归：不能再把 reqwest 的固定串透出去：{msg}"
        );
        assert!(msg.starts_with("上游流中断："), "{msg}");
        // ⑫ 精修：reqwest 自己的 `Kind` 占位串（`Kind::Body` 的
        // `request or response body error`）实测会被收进链首，必须滤掉。
        assert!(
            !msg.contains("request or response body error"),
            "reqwest 的占位层不该出现在归因链里：{msg}"
        );
        assert!(
            msg.contains(" ← "),
            "应当保留多层链（真因 + 至少一层上下文）：{msg}"
        );
        // 实测这条链是 hyper-util 的「error reading a body from connection」
        // 加上内层「end of file before message length reached」
        //（content-length 未满足就 EOF）；不同 hyper 版本的措辞可能不同，
        // 这里只钉「必须带上真因」，不钉具体字符串。
        assert!(
            msg.contains("error reading a body from connection")
                || msg.contains("closed before message completed")
                || msg.contains("end of file")
                || msg.contains("连接被对端关闭"),
            "应当带上真因：{msg}"
        );
        assert!(!msg.contains("空闲超时"), "这不是超时形态：{msg}");
    }

    /// ★★★ ⑫：流中**空闲超时**必须是另一种可辨认的归因。
    ///
    /// 上游发完一帧后既不关连接也不再吐字节，`read_timeout` 到点后 reqwest 报
    /// `Kind::Body` + `TimedOut`（`async_impl/body.rs:351-353`），
    /// 经 `bytes_stream` 被包成 `Kind::Decode` ⇒ Display 与「上游断连」完全相同，
    /// **只有 `is_timeout()` 能区分**。
    #[tokio::test]
    async fn idle_timeout_is_attributed_as_timeout() {
        let base = spawn_stub_upstream(
            |sock| {
                use std::io::Write as _;
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n");
                let _ = sock.write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"half\"}}]}\n\n");
                let _ = sock.flush();
            },
            |sock| {
                // 吊住连接不发数据，等客户端超时。
                std::thread::sleep(Duration::from_millis(1500));
                let _ = sock.shutdown(std::net::Shutdown::Both);
            },
        );
        let client = reqwest::Client::builder()
            .no_proxy()
            .read_timeout(Duration::from_millis(250))
            .build()
            .expect("构造客户端");
        let resp = client
            .get(format!("{base}/stream"))
            .send()
            .await
            .expect("错误应发生在读 body 时，而不是建连时");
        let msg = first_error(SseFramesIter::new(resp.bytes_stream()))
            .await
            .expect("空闲超时必须报错");
        assert!(
            !msg.contains("error decoding response body"),
            "⑫ 回归：不能再把 reqwest 的固定串透出去：{msg}"
        );
        assert!(msg.contains("空闲超时"), "空闲超时必须与上游断连区分开：{msg}");
    }

    /// ★★ 归因文案必须仍然落进 DSH 的 `TRANSPORT` 归类（前缀由调用方拼接，
    /// 见 `server.rs` 的 `format!("upstream stream ended before the completion event: {e}")`）。
    /// 这里把两种归因各拼一遍，钉住「换归因不换归类」。
    #[test]
    fn attributed_text_keeps_the_retryable_prefix_contract() {
        for tail in [
            "上游流中断：流中空闲超时（上游在配置的空闲时限内没有吐出任何数据）",
            "上游流中断：connection closed before message completed",
        ] {
            let full = format!("upstream stream ended before the completion event: {tail}");
            let lower = full.to_lowercase();
            assert!(lower.contains("stream ended before"), "{full}");
            for bad in ["401", "403", "429", "500", "502", "503"] {
                assert!(!lower.contains(bad), "裸状态码会改变归类：{full}");
            }
        }
    }

    /// ★★ ⑫ 精修：噪声过滤表必须与 reqwest 的 Display 实现保持一致。
    ///
    /// 这张表是**硬编码**的（reqwest 没把 `Kind` 的 Display 文案暴露成常量），
    /// 所以用测试钉住它 —— reqwest 升级改了文案时这里会红，而不是静默退化成
    /// 业主当初看到的那句无信息量的 `error decoding response body`。
    #[test]
    fn reqwest_kind_noise_matches_reqwest_display() {
        assert_eq!(REQWEST_KIND_NOISE.len(), 3);
        assert!(REQWEST_KIND_NOISE.contains(&"error decoding response body"));
        assert!(REQWEST_KIND_NOISE.contains(&"request or response body error"));
        assert!(REQWEST_KIND_NOISE.contains(&"error sending request"));
        // 真因层（hyper / io）绝不能在这张表里，否则会被一起过滤掉。
        for real in [
            "error reading a body from connection",
            "connection closed before message completed",
            "end of file before message length reached",
        ] {
            assert!(!REQWEST_KIND_NOISE.contains(&real), "{real}");
        }
    }

    /// ★★ 端到端不变式：**预读过的流**与**没预读的流**必须产出完全相同的下游帧。
    ///
    /// 这是整个首帧预读移植的核心保证 —— 预读只是为了提前发现失败，绝不能改变
    /// 成功路径上任何一个字节（少一个 `\n` 就会让相邻帧粘连，表现为「工具调用
    /// 参数不完整」「工具块数=0」这类看似无关的解析错误）。
    #[tokio::test]
    async fn probed_and_unprobed_streams_yield_identical_frames() {
        let original = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n",
            "\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"}}]}\n",
            "\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"t1\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"a\\\"\"}}]}}]}\n",
            "\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":1}\"}}]}}]}\n",
            "\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n",
            "\n",
            "data: [DONE]\n",
            "\n",
        );

        // 对照组：不预读，直接把整条流喂进去。
        let plain = SseFramesIter::new(probe_response(vec![original.to_string()]).bytes_stream());
        let expected = collect_frames(plain).await;

        // 实验组：先预读，再把 consumed 交回，喂剩下的流。
        let mut resp = probe_response(vec![
            original[..20].to_string(),
            original[20..150].to_string(),
            original[150..].to_string(),
        ]);
        let out = probe_first_frame(&mut resp, Duration::from_secs(5)).await;
        assert!(out.error.is_none(), "{:?}", out.error);
        let mut probed = SseFramesIter::new(resp.bytes_stream());
        probed.push_prefix(&out.consumed);
        let actual = collect_frames(probed).await;

        assert_eq!(actual, expected, "预读不得改变任何一帧");
        // 工具调用参数跨帧分片是 SSE 的正常形态（客户端负责拼接）；这里确认两段
        // 分片都还在、且顺序正确 —— 粘连或丢帧会在这里现形。
        let joined = actual.1.clone();
        let first = joined
            .find("\\\"a\\\"")
            .expect("第一段参数分片应在输出里");
        let second = joined.find(":1}").expect("第二段参数分片应在输出里");
        assert!(first < second, "参数分片顺序不得颠倒");
        assert!(joined.contains("tool_calls"), "{joined}");
    }

    /// 把帧迭代器读干，返回 `(有效帧数, 输出文本)` 便于比对。
    async fn collect_frames<S>(mut frames: SseFramesIter<S>) -> (usize, String)
    where
        S: futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
    {
        let mut out = String::new();
        while let Some(item) = frames.next_frame().await {
            if let Ok(text) = item {
                out.push_str(&text);
            }
        }
        (frames.valid_frames(), out)
    }

    /// ★★ 上游吐过内容却**没**写 `[DONE]` 就断流：迭代器只补 `[DONE]`，
    /// 绝不替上游宣告完成 —— 判定交给调用方（`premature_end()`），由它先补一帧
    /// 终止性 error（`server.rs` 的 `chat_stream_error_frame`）。
    ///
    /// 这里钉死三件事：① 内容帧一个不少；② 末帧是补出来的 `data: [DONE]`；
    /// ③ `premature_end()` 非空 —— 少了它调用方就不会补 error 帧，用户看到的
    /// 就是「会话中断」。
    #[tokio::test]
    async fn truncated_stream_is_flagged_but_content_is_preserved() {
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"half\"}}]}\n",
            "\n",
        );
        let mut frames = SseFramesIter::new(probe_response(vec![body.to_string()]).bytes_stream());
        let mut out = String::new();
        while let Some(item) = frames.next_frame().await {
            if let Ok(text) = item {
                out.push_str(&text);
            }
        }

        assert_eq!(frames.valid_frames(), 1, "{out}");
        assert!(out.contains("\"content\":\"half\""), "{out}");
        assert!(
            out.ends_with("data: [DONE]\n\n"),
            "必须补 [DONE] 让客户端能收尾：{out}"
        );
        assert!(
            frames.premature_end().is_some(),
            "吐过内容却没见 [DONE] 必须被标为提前结束：{out}"
        );
        // `normalize_frame` 会补 `finish_reason: null`，所以判据必须是「没有一个非 null 的
        // finish_reason」——补一个 `"stop"` 就等于替上游宣告「模型答完了」。
        for line in out.lines().filter_map(|l| l.strip_prefix("data: ")) {
            if line.trim() == "[DONE]" {
                continue;
            }
            let v: Value = serde_json::from_str(line).expect("帧必须是合法 JSON");
            if let Some(c) = v["choices"].get(0) {
                assert!(c["finish_reason"].is_null(), "不得替上游宣告完成：{out}");
            }
        }
        assert!(
            !out.contains("error"),
            "迭代器不负责判定失败，补 error 是调用方的事：{out}"
        );
    }

    /// ★★ 上游**已经宣告过非空 `finish_reason`**、只差终止字节 `[DONE]`：
    /// 内容完整，不得再判成提前结束。
    ///
    /// 2026-10-07 追加。上游是先给 `finish_reason`、后写 `[DONE]`，两者之间那一瞬
    /// 恰是连接最容易被掐断的位置（业主现场 27 个账号的断流里就有这一形态）。
    /// 旧口径下这一形态会被 `premature_end()` 标成失败，调用方随即补一帧 error，
    /// 用户看到的是「回答明明写完了，却报本轮运行失败」，客户端只能整轮重试。
    #[tokio::test]
    async fn finish_reason_without_done_is_not_flagged_as_premature() {
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"}}]}\n",
            "\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
            "\n",
        );
        let mut frames = SseFramesIter::new(probe_response(vec![body.to_string()]).bytes_stream());
        let mut out = String::new();
        while let Some(item) = frames.next_frame().await {
            if let Ok(text) = item {
                out.push_str(&text);
            }
        }

        assert_eq!(frames.valid_frames(), 2, "{out}");
        assert!(frames.saw_finish_reason(), "必须记下非空 finish_reason：{out}");
        assert!(!frames.saw_done(), "上游确实没写 [DONE]：{out}");
        assert!(out.contains("\"finish_reason\":\"stop\""), "{out}");
        assert!(out.ends_with("data: [DONE]\n\n"), "{out}");
        assert!(
            frames.premature_end().is_none(),
            "内容已完整（finish_reason 已到），不得再判提前结束：{out}"
        );
    }

    /// `finish_reason: null` 是归一化补出来的缺省值，**不算**上游宣告完成。
    ///
    /// `normalize_frame` 会给每一帧都补上 `finish_reason` 键（缺省为 null），所以
    /// 判据必须是「非空字符串」，而不是「键存在」——否则任何一帧都会被当成答完。
    #[tokio::test]
    async fn null_finish_reason_does_not_count_as_completion() {
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"half\"},\"finish_reason\":null}]}\n",
            "\n",
        );
        let mut frames = SseFramesIter::new(probe_response(vec![body.to_string()]).bytes_stream());
        let mut out = String::new();
        while let Some(item) = frames.next_frame().await {
            if let Ok(text) = item {
                out.push_str(&text);
            }
        }

        assert_eq!(frames.valid_frames(), 1, "{out}");
        assert!(!frames.saw_finish_reason(), "null 不是宣告完成：{out}");
        assert!(
            frames.premature_end().is_some(),
            "吐过内容却没宣告完成必须仍判提前结束：{out}"
        );
    }

    /// EOF 时残留在缓冲里的那一帧同样要参与 `finish_reason` 判定。
    ///
    /// 上游最后一段字节常常不带结尾换行（连接就在那里断），那一帧只会在
    /// 「干净 EOF 冲缓冲」这条分支里被归一化 —— 若只在前一条分支记录，
    /// 「内容吐完 + 无换行收尾」的形态仍会被误判成截断。
    #[tokio::test]
    async fn finish_reason_in_tail_buffer_counts_too() {
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}"
        );
        let mut frames = SseFramesIter::new(probe_response(vec![body.to_string()]).bytes_stream());
        let mut out = String::new();
        while let Some(item) = frames.next_frame().await {
            if let Ok(text) = item {
                out.push_str(&text);
            }
        }

        assert_eq!(frames.valid_frames(), 1, "{out}");
        assert!(frames.saw_finish_reason(), "尾部残留帧也要判：{out}");
        assert!(
            frames.premature_end().is_none(),
            "内容已完整（finish_reason 已到），不得再判提前结束：{out}"
        );
    }
}
