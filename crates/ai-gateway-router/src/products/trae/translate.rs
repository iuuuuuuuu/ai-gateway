//! Trae 上游 SSE → 标准 OpenAI SSE 的翻译层。
//!
//! # 上游帧的真实形状（真机实测，见 test-bin/trae-upstream-probe/out-cn-*.txt）
//!
//! 上游不是 OpenAI 那种单行 `data: {...}`，而是**三行一组**、带序号与事件名：
//!
//! ```text
//! id:1
//! event:output
//! data:{"content":"PONG"}
//!
//! id:2
//! event:done
//! data:{"finish_reason":"stop"}
//!
//! ```
//!
//! ⇒ **必须按 `event:` 分派，不能只看 `data:` 前缀**。同一个 `data` 载荷在
//! `output` 下是正文、在 `error` 下是错误、在 `token_usage` 下是计数。
//! `id:` 行只是序号，忽略它**但不能把它当帧分隔**（空行才是分隔）。
//!
//! # 为什么本模块自己实现，不复用 `crate::upstream::sse`
//!
//! 那个模块的 `aggregate` 只认 `strip_prefix("data: ")`（**带空格**），而 Trae
//! 发的是 `data:{...}`（无空格）；它的 `SseFramesIter` 还会按白名单**重写**每一帧
//! （补 `usage: null`、换 `id`），口径与本产品的产物不同。产品适配层必须自洽
//! —— 与 Qoder/ZCode 同样的理由（见 `products/mod.rs` 契约）。
//!
//! # 输出必须满足的两个硬约束
//!
//! ① 每帧写成 `data: {json}` 加两个换行（**带空格**）且是**顶层 JSON 对象** ——
//!    `forward.rs` 会用 `sse::probe_first_frame` 预读首帧，而
//!    `sse::valid_sse_frame` 只认这种形状；否则永远找不到有效帧，用户看到
//!    「上游返回空流（无有效数据帧）」。
//! ② 上游 `event:error` 必须翻成 OpenAI 的 `{"error":{...}}` 形状，否则
//!    `sse::is_upstream_error_frame` 认不出，客户端会拿到一帧看似正常的空内容。

use bytes::Bytes;
use futures_util::{pin_mut, StreamExt};
use serde_json::{json, Map, Value};

/// 单行上限：超过就丢掉这一行继续翻译（**有意偏离** Go 的 bufio.Scanner ——
/// 它遇超长行会直接停止扫描，整条流只剩一个 `[DONE]`）。
const MAX_SSE_LINE: usize = 8 * 1024 * 1024;

/// 归一化后的事件。
#[derive(Debug, Clone, PartialEq)]
pub enum Ev {
    /// 正文/思考/工具调用增量。
    Text {
        content: String,
        reasoning: String,
        tool_calls: Option<Value>,
    },
    /// 上游宣告结束。
    Done { finish: String },
    /// 上游错误事件。
    Error { code: Value, message: String },
    /// 已知但不需要产出帧的事件（排队、进度、用量、元信息…）。
    Ignore,
}

/// 把一个 `event: <name>` 加 `data: <json>` 对归一化成 [`Ev`]。
///
/// 与 JS 参考实现 `openai-format.js:110-200 normalizeLlmUtilsChunk` 同口径。
pub fn normalize(event: &str, chunk: &Value) -> Ev {
    match event {
        "output" => {
            // 旧格式 `{"response": "..."}`；新格式（2026-05）`{"content": "..."}`。
            let mut content = String::new();
            if let Some(resp) = chunk.get("response").and_then(Value::as_str) {
                // 构建提示词的进度噪声，不是回答内容。
                if resp.starts_with("Building prompt:")
                    || resp.starts_with("Completed building prompt")
                {
                    return Ev::Ignore;
                }
                content.push_str(resp);
            }
            if let Some(c) = chunk.get("content").and_then(Value::as_str) {
                content.push_str(c);
            }

            // 旧 `reasoning_content` / 新 `reasoning` 都累加。
            let mut reasoning = String::new();
            for key in ["reasoning_content", "reasoning"] {
                if let Some(r) = chunk.get(key).and_then(Value::as_str) {
                    reasoning.push_str(r);
                }
            }

            let tool_calls = chunk
                .get("tool_calls")
                .and_then(Value::as_array)
                .filter(|a| !a.is_empty())
                .map(|a| Value::Array(a.clone()));

            if content.is_empty() && reasoning.is_empty() && tool_calls.is_none() {
                // ★ 三者全空 ⇒ 不产出帧（参考实现同款：空 delta 会让部分客户端
                //   显示一个空的思考块）。
                return Ev::Ignore;
            }
            Ev::Text {
                content,
                reasoning,
                tool_calls,
            }
        }
        "done" => Ev::Done {
            finish: chunk
                .get("finish_reason")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("stop")
                .to_string(),
        },
        "error" => Ev::Error {
            code: chunk.get("code").cloned().unwrap_or(Value::Null),
            message: chunk
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
        },
        // 排队 / 进度 / 用量 / 元信息：已知且刻意不产出帧。
        "token_usage" | "extra_info" | "metadata" | "timing_cost" | "progress_notice"
        | "queue_begin" | "request_wait_in_queue" | "queue_end" => Ev::Ignore,
        _ => Ev::Ignore,
    }
}

/// 把上游 `event:error` 的载荷渲染成一句人话。
pub fn error_text(code: &Value, message: &str) -> String {
    let c = match code {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if c.is_empty() {
        message.to_string()
    } else {
        format!("[Error {c}: {message}]")
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 把一个 JSON 值包成一帧 SSE（`data: {json}` 加空行）。
fn frame(v: Value) -> Bytes {
    // ★ `data: ` **带空格** + 顶层对象 —— 见模块头部的硬约束①。
    Bytes::from(format!("data: {v}\n\n"))
}

/// 流式翻译器：逐行喂入上游 SSE，吐出标准 OpenAI SSE 帧。
pub struct TraeStreamTranslator {
    id: String,
    model: String,
    created: i64,
    /// 是否已在某一帧里带过 `role: assistant`。
    started: bool,
    /// 是否已吐过结束帧（防止 `done` 与流末尾重复收尾）。
    finished: bool,
    /// 当前 `event:` 名（跨行保持）。
    event: String,
}

impl TraeStreamTranslator {
    pub fn new(model: &str) -> Self {
        Self {
            id: format!("chatcmpl-trae-{}", uuid::Uuid::new_v4().simple()),
            model: model.to_string(),
            created: now_unix(),
            started: false,
            finished: false,
            event: String::new(),
        }
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Bytes {
        frame(json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish,
            }],
        }))
    }

    /// 结束帧加 `[DONE]`（幂等）。
    fn finish_frames(&mut self, finish: &str) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        vec![
            self.chunk(json!({}), Some(finish)),
            Bytes::from_static(b"data: [DONE]\n\n"),
        ]
    }

    /// 喂入一行（**不含**换行符）。
    pub fn push_line(&mut self, line: &str) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        let t = line.trim();
        if t.is_empty() {
            // 空行 = 帧分隔。上游在 `data` 之后发空行，这里不需要做事
            // （分派依据是 `event:` 行，不是空行）。
            return Vec::new();
        }
        if let Some(rest) = t.strip_prefix("event:") {
            self.event = rest.trim().to_string();
            return Vec::new();
        }
        let Some(rest) = t.strip_prefix("data:") else {
            // `id:1` 之类的行：忽略，但**不断帧**。
            return Vec::new();
        };
        let payload = rest.trim();
        if payload.is_empty() {
            return Vec::new();
        }
        if payload == "[DONE]" {
            return self.finish_frames("stop");
        }
        let Ok(v) = serde_json::from_str::<Value>(payload) else {
            // 解析失败原样跳过（与参考实现同款：坏帧不该杀掉整条流）。
            return Vec::new();
        };
        let event = self.event.clone();
        self.push_event(&event, &v)
    }

    fn push_event(&mut self, event: &str, v: &Value) -> Vec<Bytes> {
        match normalize(event, v) {
            Ev::Text {
                content,
                reasoning,
                tool_calls,
            } => {
                let mut delta = Map::new();
                if !self.started {
                    // 首帧带上 role（OpenAI 的形状），避免客户端自己猜。
                    self.started = true;
                    delta.insert("role".to_string(), Value::String("assistant".to_string()));
                }
                if !content.is_empty() {
                    delta.insert("content".to_string(), Value::String(content));
                }
                if !reasoning.is_empty() {
                    delta.insert("reasoning_content".to_string(), Value::String(reasoning));
                }
                if let Some(tc) = tool_calls {
                    let mapped = map_tool_calls(&tc);
                    // 空数组不插键：`"tool_calls": []` 会被部分客户端当成
                    // 「本轮有工具调用」而挂上一个空名调用。
                    if mapped.as_array().is_some_and(|a| !a.is_empty()) {
                        delta.insert("tool_calls".to_string(), mapped);
                    }
                }
                if delta.len() == 1 && delta.contains_key("role") {
                    // 只有 role 没有内容：不发（会产出空帧）。
                    self.started = false;
                    return Vec::new();
                }
                vec![self.chunk(Value::Object(delta), None)]
            }
            Ev::Done { finish } => self.finish_frames(&finish),
            Ev::Error { code, message } => vec![frame(json!({
                "error": {
                    "message": error_text(&code, &message),
                    "type": "upstream_error",
                    "code": code,
                }
            }))],
            Ev::Ignore => Vec::new(),
        }
    }

    /// 上游流结束（可能没有 `done` 事件）。
    pub fn finish(&mut self) -> Vec<Bytes> {
        self.finish_frames("stop")
    }
}

/// 取一个上游 tool_call 的名字（`name` / `tool_name` / `function.name`）。
///
/// 三种形状都没有时返回空串 —— 调用方据此判定「这个调用没有名字」。
fn tool_name(tc: &Value) -> &str {
    ["name", "tool_name"]
        .iter()
        .find_map(|k| tc.get(*k).and_then(Value::as_str))
        .or_else(|| {
            tc.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
        })
        .unwrap_or("")
}

/// 把上游的 tool_calls 数组映射成 OpenAI 的 delta.tool_calls 形状。
///
/// 与 JS 参考实现 `openai-format.js:219-235` 逐项一致：名字取
/// `name`/`function.name`/`tool_name`，参数取 `params`/`arguments`/`input`/
/// `function.arguments`，对象参数序列化成字符串。
fn map_tool_calls(raw: &Value) -> Value {
    let Some(arr) = raw.as_array() else {
        return Value::Array(Vec::new());
    };
    let out: Vec<Value> = arr
        .iter()
        .enumerate()
        // ★ 丢弃名字为空的调用：上游偶发只发 id 与参数、**整帧不带 `name` 键**
        //   （2026-10-03 实测三例），照原样透传会让客户端收到 `name:""` 的
        //   tool-call ⇒ `unknown tool ""` ⇒ 这条坏调用永久留在历史里，
        //   之后每一轮请求都被上游 400 code=11133 打回。宁可丢掉这次调用
        //   （模型下一轮会重发一个带名字的），也不要污染历史。
        .filter(|(_, tc)| !tool_name(tc).trim().is_empty())
        .map(|(i, tc)| {
            let name = tool_name(tc).to_string();
            let args = ["params", "arguments", "input"]
                .iter()
                .find_map(|k| tc.get(*k))
                .or_else(|| tc.get("function").and_then(|f| f.get("arguments")))
                .cloned()
                .unwrap_or_else(|| json!({}));
            let args_str = match args {
                Value::String(s) => s,
                other => other.to_string(),
            };
            let index = tc.get("index").and_then(Value::as_u64).unwrap_or(i as u64);
            let id = tc
                .get("id")
                .and_then(Value::as_str)
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    let hex = uuid::Uuid::new_v4().simple().to_string();
                    format!("call_{}", &hex[..24])
                });
            json!({
                "index": index,
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": args_str },
            })
        })
        .collect();
    Value::Array(out)
}

/// 把上游字节流翻成 OpenAI SSE 字节流。**纯流式，绝不缓冲整条回答。**
pub fn translate_stream<S, E>(
    upstream: S,
    model: String,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>>
where
    S: futures_util::Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    async_stream::stream! {
        let mut tr = TraeStreamTranslator::new(&model);
        let mut pending: Vec<u8> = Vec::new();
        let mut skipping = false;
        pin_mut!(upstream);

        'outer: while let Some(item) = upstream.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(e) => {
                    yield Ok(frame(json!({
                        "error": {
                            "message": format!("读取上游流失败: {e}"),
                            "type": "upstream_error",
                            "code": Value::Null,
                        }
                    })));
                    break 'outer;
                }
            };
            pending.extend_from_slice(&chunk);
            loop {
                let Some(p) = pending.iter().position(|b| *b == b'\n') else {
                    break;
                };
                let rest = pending.split_off(p + 1);
                let mut raw = std::mem::replace(&mut pending, rest);
                raw.pop(); // 去掉换行字节
                if raw.len() > MAX_SSE_LINE {
                    // 丢掉超长行继续翻译（见 MAX_SSE_LINE 的说明）。
                    skipping = true;
                    continue;
                }
                skipping = false;
                let line = String::from_utf8_lossy(&raw).to_string();
                for b in tr.push_line(&line) {
                    yield Ok(b);
                }
                if tr.finished {
                    break 'outer;
                }
            }
        }

        if !skipping && !pending.is_empty() {
            let line = String::from_utf8_lossy(&pending).to_string();
            for b in tr.push_line(&line) {
                yield Ok(b);
            }
        }
        for b in tr.finish() {
            yield Ok(b);
        }
    }
}

/// 非流式聚合：把上游流**全文**（未翻译的三行帧原文）拼成一个 chat.completion。
///
/// `forward.rs:1122-1125` 的契约：产品上游收到的就是**上游原始全文**
/// （「预读前缀 + 剩余全文」），所以这里必须自己按 `event:` 解析。
pub fn aggregate(sse: &str, model: &str) -> Result<Map<String, Value>, String> {
    let mut event = String::new();
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut finish = String::new();
    let mut seen = 0usize;

    for line in sse.split('\n') {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(rest) = t.strip_prefix("event:") {
            event = rest.trim().to_string();
            continue;
        }
        let Some(rest) = t.strip_prefix("data:") else {
            continue;
        };
        let payload = rest.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        seen += 1;
        match normalize(&event, &v) {
            Ev::Text {
                content: c,
                reasoning: r,
                ..
            } => {
                content.push_str(&c);
                reasoning.push_str(&r);
            }
            Ev::Done { finish: f } => {
                if !f.is_empty() {
                    finish = f;
                }
            }
            Ev::Error { code, message } => {
                return Err(error_text(&code, &message));
            }
            Ev::Ignore => {}
        }
    }

    if seen == 0 {
        return Err("上游流里没有可用的事件（可能是空响应或未知协议）".to_string());
    }
    if finish.is_empty() {
        finish = "stop".to_string();
    }

    let mut msg = Map::new();
    msg.insert("role".to_string(), Value::String("assistant".to_string()));
    msg.insert("content".to_string(), Value::String(content));
    if !reasoning.is_empty() {
        // ★ 只在非空时给：空串会让部分客户端显示空的思考块。
        msg.insert("reasoning_content".to_string(), Value::String(reasoning));
    }
    let mut choice = Map::new();
    choice.insert("index".to_string(), Value::from(0));
    choice.insert("message".to_string(), Value::Object(msg));
    choice.insert("finish_reason".to_string(), Value::String(finish));

    let mut out = Map::new();
    out.insert(
        "id".to_string(),
        Value::String(format!("chatcmpl-trae-{}", uuid::Uuid::new_v4().simple())),
    );
    out.insert(
        "object".to_string(),
        Value::String("chat.completion".to_string()),
    );
    out.insert("created".to_string(), Value::from(now_unix()));
    out.insert("model".to_string(), Value::String(model.to_string()));
    out.insert(
        "choices".to_string(),
        Value::Array(vec![Value::Object(choice)]),
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    fn collect(lines: &[&str]) -> String {
        let mut tr = TraeStreamTranslator::new("glm-5.2");
        let mut out = String::new();
        for l in lines {
            for b in tr.push_line(l) {
                out.push_str(&String::from_utf8_lossy(&b));
            }
        }
        for b in tr.finish() {
            out.push_str(&String::from_utf8_lossy(&b));
        }
        out
    }

    fn payloads(sse: &str) -> Vec<Value> {
        sse.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter(|p| *p != "[DONE]")
            .filter_map(|p| serde_json::from_str::<Value>(p).ok())
            .collect()
    }

    #[test]
    fn a_text_event_becomes_an_openai_delta_with_a_role() {
        let out = collect(&[
            "id:1",
            "event:output",
            "data:{\"content\":\"PONG\"}",
            "",
        ]);
        let p = payloads(&out);
        assert_eq!(p[0]["object"], "chat.completion.chunk");
        assert_eq!(p[0]["model"], "glm-5.2");
        assert_eq!(p[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(p[0]["choices"][0]["delta"]["content"], "PONG");
        assert_eq!(p[0]["choices"][0]["finish_reason"], Value::Null);
        assert!(out.ends_with("data: [DONE]\n\n"), "末尾缺 [DONE]：{out}");
        let last = payloads(&out).pop().unwrap();
        assert_eq!(last["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn the_role_is_only_sent_on_the_first_frame() {
        let out = collect(&[
            "event:output",
            "data:{\"content\":\"a\"}",
            "event:output",
            "data:{\"content\":\"b\"}",
        ]);
        let p = payloads(&out);
        assert_eq!(p[0]["choices"][0]["delta"]["role"], "assistant");
        assert!(p[1]["choices"][0]["delta"].get("role").is_none());
        assert_eq!(p[1]["choices"][0]["delta"]["content"], "b");
    }

    #[test]
    fn dispatch_follows_the_event_name_not_the_data_shape() {
        // 同一个 data 载荷在 output 下是正文，在 token_usage 下什么都不是。
        let as_output = collect(&["event:output", "data:{\"content\":\"x\"}"]);
        assert!(as_output.contains("\"content\":\"x\""));
        let as_usage = collect(&["event:token_usage", "data:{\"content\":\"x\"}"]);
        assert!(!as_usage.contains("\"content\":\"x\""));
        assert!(as_usage.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn the_old_response_field_and_the_prompt_progress_noise() {
        let out = collect(&[
            "event:output",
            "data:{\"response\":\"hel\"}",
            "event:output",
            "data:{\"response\":\"lo\"}",
        ]);
        let p = payloads(&out);
        assert_eq!(p[0]["choices"][0]["delta"]["content"], "hel");
        assert_eq!(p[1]["choices"][0]["delta"]["content"], "lo");

        let noise = collect(&[
            "event:output",
            "data:{\"response\":\"Building prompt: 1234\"}",
        ]);
        assert!(!noise.contains("Building prompt"));
    }

    #[test]
    fn reasoning_is_carried_as_reasoning_content() {
        let out = collect(&[
            "event:output",
            "data:{\"reasoning\":\"想一下\"}",
            "event:output",
            "data:{\"reasoning_content\":\"再想想\"}",
            "event:output",
            "data:{\"content\":\"答案\"}",
        ]);
        let p = payloads(&out);
        assert_eq!(p[0]["choices"][0]["delta"]["reasoning_content"], "想一下");
        assert_eq!(p[1]["choices"][0]["delta"]["reasoning_content"], "再想想");
        assert_eq!(p[2]["choices"][0]["delta"]["content"], "答案");
    }

    #[test]
    fn an_empty_output_event_produces_no_frame_at_all() {
        let out = collect(&["event:output", "data:{}"]);
        assert_eq!(payloads(&out).len(), 1);
        assert!(out.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn an_error_event_becomes_an_openai_error_frame() {
        let out = collect(&[
            "event:error",
            "data:{\"code\":4011,\"message\":\"rate limited\",\"extra\":null}",
        ]);
        let p = payloads(&out);
        assert_eq!(p[0]["error"]["type"], "upstream_error");
        assert_eq!(p[0]["error"]["code"], 4011);
        assert!(p[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("rate limited"));
    }

    #[test]
    fn the_done_event_carries_its_finish_reason() {
        let out = collect(&[
            "event:output",
            "data:{\"content\":\"a\"}",
            "event:done",
            "data:{\"finish_reason\":\"tool_calls\"}",
        ]);
        let p = payloads(&out);
        assert_eq!(p[1]["choices"][0]["finish_reason"], "tool_calls");
        assert!(out.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn a_second_done_event_does_not_emit_a_second_finish() {
        let out = collect(&[
            "event:done",
            "data:{\"finish_reason\":\"stop\"}",
            "event:done",
            "data:{\"finish_reason\":\"stop\"}",
        ]);
        assert_eq!(out.matches("[DONE]").count(), 1);
        assert_eq!(payloads(&out).len(), 1);
    }

    #[test]
    fn an_explicit_done_payload_closes_the_stream() {
        let out = collect(&["event:output", "data:{\"content\":\"a\"}", "data:[DONE]"]);
        assert!(out.ends_with("data: [DONE]\n\n"));
        assert_eq!(payloads(&out).len(), 2);
    }

    #[test]
    fn broken_json_and_unknown_lines_are_skipped_without_killing_the_stream() {
        let out = collect(&[
            "id:7",
            ": keep-alive",
            "event:output",
            "data:{not json}",
            "event:output",
            "data:{\"content\":\"ok\"}",
        ]);
        let p = payloads(&out);
        assert_eq!(p[0]["choices"][0]["delta"]["content"], "ok");
    }

    #[test]
    fn tool_calls_are_reshaped_into_the_openai_delta() {
        let out = collect(&[
            "event:output",
            "data:{\"tool_calls\":[{\"name\":\"read_file\",\"params\":{\"path\":\"a.txt\"}}]}",
        ]);
        let p = payloads(&out);
        let tc = &p[0]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["index"], 0);
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["function"]["name"], "read_file");
        assert_eq!(tc["function"]["arguments"], "{\"path\":\"a.txt\"}");
        assert!(tc["id"].as_str().unwrap().starts_with("call_"));
    }

    /// ★ 上游整帧不带 `name` 键（2026-10-03 实测三例）：空名调用被丢弃，
    /// 同帧里有名字的调用照常透传，且 `index` 保持上游给的值。
    #[test]
    fn nameless_tool_call_is_dropped_and_named_ones_survive() {
        let out = collect(&[
            "event:output",
            "data:{\"tool_calls\":[{\"index\":0,\"name\":\"run_code\",\"params\":{}},{\"index\":1,\"id\":\"call_bad\",\"params\":{}}]}",
        ]);
        let p = payloads(&out);
        let tc = p[0]["choices"][0]["delta"]["tool_calls"].as_array().unwrap();
        assert_eq!(tc.len(), 1, "空名调用应被丢弃: {tc:?}");
        assert_eq!(tc[0]["function"]["name"], "run_code");
        assert_eq!(tc[0]["index"], 0);
    }

    /// 全是空名调用 ⇒ 该帧连 `tool_calls` 键都不带（正文照常发）。
    #[test]
    fn all_nameless_tool_calls_produce_no_tool_calls_key() {
        let out = collect(&[
            "event:output",
            "data:{\"content\":\"hi\",\"tool_calls\":[{\"id\":\"call_1\",\"params\":{}}]}",
        ]);
        let p = payloads(&out);
        assert_eq!(p[0]["choices"][0]["delta"]["content"], "hi");
        assert!(
            p[0]["choices"][0]["delta"].get("tool_calls").is_none(),
            "不得产出 tool_calls 键: {}",
            p[0]
        );
    }
    #[tokio::test]
    async fn translate_stream_reassembles_lines_split_across_chunks() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"id:1\nevent:out")),
            Ok(Bytes::from_static(b"put\ndata:{\"content\":\"he")),
            Ok(Bytes::from_static(
                b"llo\"}\n\nevent:done\ndata:{\"finish_reason\":\"stop\"}\n\n",
            )),
        ];
        let s = translate_stream(stream::iter(chunks), "glm-5.2".to_string());
        let out: Vec<u8> = s
            .fold(Vec::new(), |mut acc, b| async move {
                acc.extend_from_slice(&b.unwrap());
                acc
            })
            .await;
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("\"content\":\"hello\""), "{text}");
        assert!(text.ends_with("data: [DONE]\n\n"));
    }

    #[tokio::test]
    async fn translate_stream_reports_a_read_failure_as_an_error_frame() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(
                b"event:output\ndata:{\"content\":\"a\"}\n\n",
            )),
            Err(std::io::Error::new(std::io::ErrorKind::Other, "boom")),
        ];
        let s = translate_stream(stream::iter(chunks), "m".to_string());
        let out: Vec<u8> = s
            .fold(Vec::new(), |mut acc, b| async move {
                acc.extend_from_slice(&b.unwrap());
                acc
            })
            .await;
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("读取上游流失败"));
        assert!(text.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn aggregate_concatenates_the_three_line_frames() {
        let sse = "id:1\nevent:output\ndata:{\"content\":\"he\"}\n\n\
                   id:2\nevent:output\ndata:{\"content\":\"llo\"}\n\n\
                   id:3\nevent:done\ndata:{\"finish_reason\":\"stop\"}\n\n";
        let m = aggregate(sse, "glm-5.2").unwrap();
        assert_eq!(m["object"], "chat.completion");
        assert_eq!(m["model"], "glm-5.2");
        assert_eq!(m["choices"][0]["message"]["content"], "hello");
        assert_eq!(m["choices"][0]["message"]["role"], "assistant");
        assert_eq!(m["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn aggregate_keeps_reasoning_and_drops_empty_reasoning() {
        let sse = "event:output\ndata:{\"reasoning\":\"想\"}\n\n\
                   event:output\ndata:{\"content\":\"答\"}\n\n\
                   event:done\ndata:{}\n\n";
        let m = aggregate(sse, "m").unwrap();
        assert_eq!(m["choices"][0]["message"]["reasoning_content"], "想");
        assert_eq!(m["choices"][0]["message"]["content"], "答");

        let plain = "event:output\ndata:{\"content\":\"答\"}\n\n";
        let m2 = aggregate(plain, "m").unwrap();
        assert!(m2["choices"][0]["message"]
            .get("reasoning_content")
            .is_none());
    }

    #[test]
    fn aggregate_reports_an_upstream_error_instead_of_a_fake_answer() {
        let sse = "id:1\nevent:error\n\
                   data:{\"code\":4001,\"message\":\"param is invalid\",\"extra\":null}\n\n";
        let e = aggregate(sse, "m").unwrap_err();
        assert!(e.contains("4001"), "{e}");
        assert!(e.contains("param is invalid"), "{e}");
    }

    #[test]
    fn aggregate_rejects_an_empty_stream() {
        assert!(aggregate("", "m").is_err());
        assert!(aggregate("id:1\nevent:output\n", "m").is_err());
    }

    #[test]
    fn aggregate_tolerates_crlf_and_blank_lines() {
        let sse = "id:1\r\nevent:output\r\ndata:{\"content\":\"hi\"}\r\n\r\n";
        let m = aggregate(sse, "m").unwrap();
        assert_eq!(m["choices"][0]["message"]["content"], "hi");
    }

    #[test]
    fn error_text_handles_a_missing_code() {
        assert_eq!(error_text(&Value::Null, "boom"), "boom");
        assert_eq!(
            error_text(&json!(4011), "slow down"),
            "[Error 4011: slow down]"
        );
        assert_eq!(
            error_text(&Value::String("4011".into()), "slow down"),
            "[Error 4011: slow down]"
        );
    }
}
