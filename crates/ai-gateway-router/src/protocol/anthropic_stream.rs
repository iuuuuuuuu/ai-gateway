//! Anthropic Messages SSE 翻译：把上游的 OpenAI Chat SSE 实时转成 Anthropic SSE。
//!
//! 对应 Go 源文件 `internal/server/messages_stream.go`。
//!
//! # 事件序列是硬要求
//!
//! Claude Code / Claude Desktop 对事件顺序有硬性要求，缺事件会直接报错：
//!
//! ```text
//! message_start → content_block_start → content_block_delta* →
//! content_block_stop → message_delta → message_stop
//! ```
//!
//! 工具调用映射为 `tool_use` 内容块，参数以 `input_json_delta` 分片下发。
//!
//! # 两处非显然的顺序约束
//!
//! 1. **`content_block_start` 必须先于该块的任何 delta**，而 Anthropic 的
//!    `content_block_start` 对 `tool_use` 块**必须带 `name`**。上游允许
//!    `function.arguments` 先于 `function.name` 到达（分片边界由上游写入顺序决定），
//!    因此 name 未知时**不能**直接发 delta —— 参数要先暂存，待块开启后补发。
//! 2. **`tool_use` 的 stop_reason 只在真的开启过工具块时才给**。上游只发了
//!    arguments（甚至只有 index）却没给 name 时块永远开不起来，客户端一个
//!    `tool_use` 块都收不到；此时若仍回 `tool_use`，客户端会被要求执行一个它
//!    根本没收到的工具，只能卡住等下一次输入。这种情况退化为 `end_turn`。

use serde_json::{json, Map, Value};

use crate::protocol::anthropic::{anthropic_error, anthropic_usage};
use crate::protocol::{new_message_id, num_of, str_field};

/// Anthropic SSE 事件写出器（`event: <名>\ndata: <json>\n\n`）。
#[derive(Debug, Default)]
pub struct SseOut {
    buf: String,
}

impl SseOut {
    /// 新建写出器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一个具名事件。
    pub fn write(&mut self, event: &str, payload: &Value) -> Result<(), String> {
        let raw = serde_json::to_string(payload).map_err(|e| e.to_string())?;
        self.buf.push_str("event: ");
        self.buf.push_str(event);
        self.buf.push_str("\ndata: ");
        self.buf.push_str(&raw);
        self.buf.push_str("\n\n");
        Ok(())
    }

    /// 取出已累积的帧文本并清空缓冲（调用方负责写给客户端并 flush）。
    pub fn take(&mut self) -> String {
        std::mem::take(&mut self.buf)
    }

    /// 当前缓冲是否为空。
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

/// 单个工具调用的累积状态。
#[derive(Debug, Default)]
struct ToolState {
    id: String,
    name: String,
    /// 暂存「先于 name 到达」的参数分片。
    ///
    /// name 未知时块无法开启，此时不能发 delta，否则会先于 `content_block_start`
    /// 发出，客户端按协议拒绝/丢弃该工具块。name 到达后补发。
    ///
    /// 注意这里**不需要**再单独累积「已发出」的参数：Anthropic 的
    /// `input_json_delta` 是纯增量下发，客户端自己拼接，网关无需留副本。
    pending: String,
    /// 块在 Anthropic 协议里的 index。
    index: usize,
    /// 是否已发出 `content_block_start`。
    open: bool,
}

/// 流式转换的累积状态。
pub struct AnthropicStreamState {
    message_id: String,
    model: String,
    usage: Option<Value>,

    text_open: bool,
    text_index: usize,

    tool_calls: std::collections::BTreeMap<i64, ToolState>,
    /// 工具调用出现的顺序（用于分配块 index）。
    tool_order: Vec<i64>,

    finish_reason: String,

    /// 上游在流中途发过终止性 error 帧时的原因；非空表示本次必须按失败收尾。
    upstream_err: Option<String>,
}

impl AnthropicStreamState {
    /// 新建状态。`message_id` 每响应唯一。
    pub fn new(model: &str) -> Self {
        Self {
            // 每响应唯一（见 new_message_id 的注释）：客户端按 id 合并历史，
            // 恒定 id 会让不同轮次的响应被误并，破坏 tool 配对。
            message_id: new_message_id(),
            model: model.to_string(),
            usage: None,
            text_open: false,
            text_index: 0,
            tool_calls: std::collections::BTreeMap::new(),
            tool_order: Vec::new(),
            finish_reason: String::new(),
            upstream_err: None,
        }
    }

    /// 组装 `message` 对象（`message_start` 与 `message_delta` 共用形状）。
    fn message_object(&self) -> Value {
        json!({
            "id": self.message_id,
            "type": "message",
            "role": "assistant",
            "model": self.model,
            "content": [],
            "stop_reason": Value::Null,
            "stop_sequence": Value::Null,
            "usage": anthropic_usage(self.usage.as_ref()),
        })
    }

    /// 发出 `message_start`。必须在任何内容块之前。
    pub fn start(&self, out: &mut SseOut) -> Result<(), String> {
        out.write(
            "message_start",
            &json!({"type": "message_start", "message": self.message_object()}),
        )
    }

    /// 处理单个 chat SSE chunk。
    pub fn consume(&mut self, out: &mut SseOut, chunk: &Map<String, Value>) -> Result<(), String> {
        // 上游在 HTTP 200 的流**中途**发 `{"error":{...}}` 表示终止性失败
        //（渠道未批准、账号被封）。必须记下来并按失败收尾 —— 忽略它会照常补出
        // message_stop，等于向客户端宣告「模型正常答完了」，Claude Code 会把
        // 空/截断的回答当成一次成功回合继续推进对话，而网关侧还记成成功。
        if let Some(e) = chunk.get("error") {
            if !e.is_null() {
                self.upstream_err = Some(error_frame_text(e));
            }
        }
        if let Some(m) = chunk.get("model").and_then(|v| v.as_str()) {
            if self.model.is_empty() {
                self.model = m.to_string();
            }
        }
        if let Some(u) = chunk.get("usage") {
            if u.is_object() {
                self.usage = Some(u.clone());
            }
        }

        let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) else {
            return Ok(());
        };
        for ci in choices {
            let Some(choice) = ci.as_object() else { continue };
            let fr = str_field(choice, "finish_reason");
            if !fr.is_empty() {
                self.finish_reason = fr;
            }
            let Some(delta) = choice.get("delta").and_then(|d| d.as_object()) else {
                continue;
            };
            let text = str_field(delta, "content");
            if !text.is_empty() {
                self.open_text(out)?;
                out.write(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": self.text_index,
                        "delta": {"type": "text_delta", "text": text},
                    }),
                )?;
            }
            if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                for tci in tcs {
                    let Some(call) = tci.as_object() else { continue };
                    self.consume_tool_call(out, call)?;
                }
            }
        }
        Ok(())
    }

    /// 开启文本块（幂等）。
    fn open_text(&mut self, out: &mut SseOut) -> Result<(), String> {
        if self.text_open {
            return Ok(());
        }
        self.text_open = true;
        out.write(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": self.text_index,
                "content_block": {"type": "text", "text": ""},
            }),
        )
    }

    /// 处理一个 tool_call 分片。
    fn consume_tool_call(
        &mut self,
        out: &mut SseOut,
        call: &Map<String, Value>,
    ) -> Result<(), String> {
        let idx = num_of(call.get("index").unwrap_or(&Value::Null));
        if !self.tool_calls.contains_key(&idx) {
            self.tool_order.push(idx);
            let index = self.next_block_index();
            self.tool_calls.insert(
                idx,
                ToolState {
                    index,
                    ..Default::default()
                },
            );
        }
        // 先取出 id/name/args，再改状态（避免同时持有不可变与可变借用）。
        let id = str_field(call, "id");
        let fn_obj = call.get("function").and_then(|f| f.as_object());
        let name = fn_obj.map(|f| str_field(f, "name")).unwrap_or_default();
        let args = fn_obj.map(|f| str_field(f, "arguments")).unwrap_or_default();

        let tc = self.tool_calls.get_mut(&idx).expect("刚插入");
        if !id.is_empty() {
            tc.id = id;
        }
        if !name.is_empty() {
            tc.name = name;
        }
        let should_open = !tc.open && !tc.name.is_empty();
        let block_index = tc.index;

        // 拿到 name 后才能开块（Anthropic 的 content_block_start 必须带 name）。
        if should_open {
            let (tc_id, tc_name, pending) = {
                let tc = self.tool_calls.get_mut(&idx).expect("存在");
                tc.open = true;
                (
                    tc.id.clone(),
                    tc.name.clone(),
                    std::mem::take(&mut tc.pending),
                )
            };
            out.write(
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": block_index,
                    "content_block": {
                        "type": "tool_use",
                        "id": tc_id,
                        "name": tc_name,
                        "input": {},
                    },
                }),
            )?;
            // name 迟到时，把先于 name 到达的参数在 start 之后补发（内容不丢）。
            if !pending.is_empty() {
                write_tool_args(out, block_index, &pending)?;
            }
        }

        if !args.is_empty() {
            let open = self.tool_calls.get(&idx).map(|t| t.open).unwrap_or(false);
            if open {
                write_tool_args(out, block_index, &args)?;
            } else if let Some(tc) = self.tool_calls.get_mut(&idx) {
                // name 未到：先暂存，待 start 之后再补发（绝不发出跨 start 的 delta）。
                tc.pending.push_str(&args);
            }
        }
        Ok(())
    }

    /// 下一个可用的块 index。
    fn next_block_index(&self) -> usize {
        if self.text_open {
            1 + self.tool_order.len().saturating_sub(1)
        } else {
            self.tool_order.len().saturating_sub(1)
        }
    }

    /// 按 Anthropic 规范逐个关闭已开启的内容块。
    pub fn close_open_blocks(&self, out: &mut SseOut) -> Result<(), String> {
        if self.text_open {
            out.write(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": self.text_index}),
            )?;
        }
        for (_, tc) in self.tool_calls.iter() {
            if !tc.open {
                continue;
            }
            out.write(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": tc.index}),
            )?;
        }
        Ok(())
    }

    /// 把 chat 的 `finish_reason` 映射成 Anthropic 的 `stop_reason`。
    ///
    /// 注意 `tool_use` 的判定口径：只有**真正开启过工具块**才算工具调用
    ///（详见模块文档第 2 条）。
    pub fn stop_reason(&self) -> &'static str {
        match self.finish_reason.as_str() {
            "length" => "max_tokens",
            "tool_calls" => {
                if self.has_open_tool_block() {
                    "tool_use"
                } else {
                    "end_turn"
                }
            }
            "" => {
                if self.has_open_tool_block() {
                    "tool_use"
                } else {
                    "end_turn"
                }
            }
            _ => "end_turn",
        }
    }

    /// 是否至少有一个工具块真的发给了客户端。
    fn has_open_tool_block(&self) -> bool {
        self.tool_calls.values().any(|tc| tc.open)
    }

    /// 收尾：关闭内容块，发 `message_delta` 与 `message_stop`。
    ///
    /// 返回累积的 usage（供统计）。
    pub fn finish(&self, out: &mut SseOut) -> Result<Option<Value>, String> {
        self.close_open_blocks(out)?;
        let output_tokens = self
            .usage
            .as_ref()
            .and_then(|u| u.as_object())
            .map(|u| u.get("completion_tokens").map(num_of).unwrap_or(0))
            .unwrap_or(0);
        out.write(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": self.stop_reason(),
                    "stop_sequence": Value::Null,
                },
                "usage": {"output_tokens": output_tokens},
            }),
        )?;
        out.write("message_stop", &json!({"type": "message_stop"}))?;
        Ok(self.usage.clone())
    }

    /// 发一个错误事件（上游中途断流时用）。
    ///
    /// **必须配合提前 return**：往下走会照常发 `message_stop`，而它在 Anthropic
    /// 协议里表示「本轮正常结束」，客户端会把截断的半截回复当成完整回答收下 ——
    /// 表现为「输出莫名断了且无任何报错」。
    pub fn write_error(&self, out: &mut SseOut, msg: &str) -> Result<(), String> {
        out.write("error", &anthropic_error("upstream_error", msg))
    }

    /// 本次累积的 usage。
    pub fn usage(&self) -> Option<&Value> {
        self.usage.as_ref()
    }

    /// 判定本次流是否应当按**失败**收尾，返回原因（`None` = 可正常收尾）。
    ///
    /// `saw_done` / `saw_any_frame` 由调用方从帧迭代器取（见
    /// [`crate::upstream::sse::SseFramesIter::premature_end`]）。
    ///
    /// 判定顺序（与 Go 侧 `streamFailureMessage` 同一口径）：
    /// 1. 上游显式发过终止性 error 帧 → 失败（根因优先）
    /// 2. 见过 `[DONE]` → 正常收尾
    /// 3. 没见过 `[DONE]` 且一个 data 帧都没读到（空流）→ 正常收尾
    /// 4. 没见过 `[DONE]` 但已吐过内容 → 失败
    pub fn failure_message(&self, saw_done: bool, saw_any_frame: bool) -> Option<String> {
        if let Some(e) = &self.upstream_err {
            return Some(format!("upstream error: {e}"));
        }
        if saw_done || !saw_any_frame {
            return None;
        }
        Some("upstream stream ended unexpectedly before [DONE]".to_string())
    }
}

/// 把上游流内的 `{"error": ...}` 帧抽成可读文案。
///
/// 形态不固定：`{"error":{"message":...}}`（OpenAI 形状）、`{"error":{"msg":...}}`
/// （部分中转）、以及裸字符串。逐层取第一个非空文案；都取不到时退一步带上 `code`
///（便于对照上游错误码表定位，如 11128 渠道未批准），保证失败至少可见。
pub fn error_frame_text(e: &Value) -> String {
    if let Some(m) = e.as_object() {
        for k in ["message", "msg", "detail", "error_description"] {
            let v = m.get(k).and_then(|v| v.as_str()).unwrap_or("");
            if !v.is_empty() {
                return v.to_string();
            }
        }
        if let Some(code) = m.get("code").and_then(|c| c.as_i64()) {
            if code != 0 {
                return format!("code={code}");
            }
        }
    }
    if let Some(s) = e.as_str() {
        if !s.is_empty() {
            return s.to_string();
        }
    }
    "unknown upstream error".to_string()
}

/// 在已开启的工具块上发出一个 `input_json_delta`。
fn write_tool_args(out: &mut SseOut, index: usize, args: &str) -> Result<(), String> {
    out.write(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "input_json_delta", "partial_json": args},
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    /// 从累积帧文本里数某个事件出现了几次。
    fn count_events(frames: &str, event: &str) -> usize {
        frames
            .lines()
            .filter(|l| *l == format!("event: {event}"))
            .count()
    }

    /// 提取所有帧的 payload（供断言字段）。
    fn payloads(frames: &str) -> Vec<Value> {
        frames
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|d| serde_json::from_str::<Value>(d).ok())
            .collect()
    }

    #[test]
    fn text_stream_event_sequence() {
        let mut out = SseOut::new();
        let mut st = AnthropicStreamState::new("claude-x");
        st.start(&mut out).unwrap();
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"}}]})),
        )
        .unwrap();
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"content":"lo"}}]})),
        )
        .unwrap();
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})),
        )
        .unwrap();
        st.finish(&mut out).unwrap();

        let frames = out.take();
        // 事件顺序：start → block_start → delta*2 → block_stop → message_delta → message_stop
        assert_eq!(count_events(&frames, "message_start"), 1);
        assert_eq!(count_events(&frames, "content_block_start"), 1);
        assert_eq!(count_events(&frames, "content_block_delta"), 2);
        assert_eq!(count_events(&frames, "content_block_stop"), 1);
        assert_eq!(count_events(&frames, "message_delta"), 1);
        assert_eq!(count_events(&frames, "message_stop"), 1);

        let events: Vec<String> = frames
            .lines()
            .filter_map(|l| l.strip_prefix("event: ").map(str::to_string))
            .collect();
        assert_eq!(
            events,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        // 文本块 index 为 0
        let ps = payloads(&frames);
        assert_eq!(ps[1]["index"], json!(0));
        assert_eq!(ps[1]["content_block"]["type"], json!("text"));
    }

    /// 消息 id 必须唯一，且不得透传上游 id。
    #[test]
    fn message_id_is_fresh_and_not_upstream() {
        let mut out = SseOut::new();
        let st = AnthropicStreamState::new("m");
        st.start(&mut out).unwrap();
        let ps = payloads(&out.take());
        let id = ps[0]["message"]["id"].as_str().unwrap();
        assert!(id.starts_with("msg_"), "{id}");

        let mut out2 = SseOut::new();
        let st2 = AnthropicStreamState::new("m");
        st2.start(&mut out2).unwrap();
        let ps2 = payloads(&out2.take());
        assert_ne!(
            ps2[0]["message"]["id"], ps[0]["message"]["id"],
            "两次响应 id 必须不同"
        );
    }

    /// 工具调用：块 index 从 0 起（无文本块时），arguments 以 input_json_delta 下发。
    #[test]
    fn tool_call_emits_input_json_delta() {
        let mut out = SseOut::new();
        let mut st = AnthropicStreamState::new("m");
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"id":"t1","type":"function","function":{"name":"search","arguments":"{\"q\""}}
            ]}}]})),
        )
        .unwrap();
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":":\"rust\"}"}}
            ]}}]})),
        )
        .unwrap();
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})),
        )
        .unwrap();
        st.finish(&mut out).unwrap();

        let frames = out.take();
        let ps = payloads(&frames);
        let start = ps
            .iter()
            .find(|p| p["type"] == json!("content_block_start"))
            .unwrap();
        assert_eq!(start["content_block"]["type"], json!("tool_use"));
        assert_eq!(start["content_block"]["name"], json!("search"));
        assert_eq!(start["content_block"]["id"], json!("t1"));

        // 两个参数分片
        let deltas: Vec<&Value> = ps
            .iter()
            .filter(|p| p["type"] == json!("content_block_delta"))
            .collect();
        assert_eq!(deltas.len(), 2, "{ps:?}");
        assert_eq!(deltas[0]["delta"]["type"], json!("input_json_delta"));
        assert_eq!(deltas[0]["delta"]["partial_json"], json!(r#"{"q""#));
        assert_eq!(deltas[1]["delta"]["partial_json"], json!(r#":"rust"}"#));
        assert_eq!(st.stop_reason(), "tool_use");
    }

    /// **关键顺序约束**：arguments 先于 name 到达时，delta 绝不能先于 block_start 发出。
    #[test]
    fn args_before_name_never_precedes_block_start() {
        let mut out = SseOut::new();
        let mut st = AnthropicStreamState::new("m");
        // 第一片只有 arguments，没有 name
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"{\"a\""}}
            ]}}]})),
        )
        .unwrap();
        // name 到达
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"id":"t1","function":{"name":"f","arguments":":1}"}}
            ]}}]})),
        )
        .unwrap();

        let frames = out.take();
        let events: Vec<&str> = frames
            .lines()
            .filter_map(|l| l.strip_prefix("event: "))
            .collect();
        // 首个事件必须是 content_block_start，不能是 delta
        assert_eq!(
            events.first(),
            Some(&"content_block_start"),
            "delta 不得先于 start: {events:?}"
        );
        // 且两段参数都被补发（内容不丢）
        let ps = payloads(&frames);
        let joined: String = ps
            .iter()
            .filter(|p| p["type"] == json!("content_block_delta"))
            .filter_map(|p| p["delta"]["partial_json"].as_str())
            .collect();
        assert_eq!(joined, r#"{"a":1}"#, "参数必须完整补发");
    }

    /// name 始终未到时块开不起来 → stop_reason 退化为 end_turn（不能报 tool_use）。
    #[test]
    fn missing_name_degrades_stop_reason_to_end_turn() {
        let mut out = SseOut::new();
        let mut st = AnthropicStreamState::new("m");
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"{\"a\":1}"}}
            ]}}]})),
        )
        .unwrap();
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})),
        )
        .unwrap();
        assert_eq!(
            st.stop_reason(),
            "end_turn",
            "没有真正开启的 tool_use 块时不得报 tool_use"
        );
        let frames = {
            st.finish(&mut out).unwrap();
            out.take()
        };
        assert_eq!(
            count_events(&frames, "content_block_start"),
            0,
            "name 未知时不得开启工具块"
        );
    }

    /// finish_reason=length → max_tokens。
    #[test]
    fn length_maps_to_max_tokens() {
        let mut out = SseOut::new();
        let mut st = AnthropicStreamState::new("m");
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{},"finish_reason":"length"}]})),
        )
        .unwrap();
        assert_eq!(st.stop_reason(), "max_tokens");
    }

    /// 文本块与工具块共存时 index 不冲突。
    #[test]
    fn text_and_tool_indices_do_not_collide() {
        let mut out = SseOut::new();
        let mut st = AnthropicStreamState::new("m");
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"content":"hi"}}]})),
        )
        .unwrap();
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"id":"t1","function":{"name":"f","arguments":"{}"}}
            ]}}]})),
        )
        .unwrap();
        st.finish(&mut out).unwrap();

        let frames = out.take();
        let ps = payloads(&frames);
        let text_start = ps
            .iter()
            .find(|p| p["type"] == json!("content_block_start") && p["content_block"]["type"] == json!("text"))
            .unwrap();
        let tool_start = ps
            .iter()
            .find(|p| p["type"] == json!("content_block_start") && p["content_block"]["type"] == json!("tool_use"))
            .unwrap();
        assert_ne!(
            text_start["index"], tool_start["index"],
            "文本块与工具块 index 必须不同"
        );
    }

    /// usage 透传到 message_start 与 message_delta。
    #[test]
    fn usage_propagates() {
        let mut out = SseOut::new();
        let mut st = AnthropicStreamState::new("m");
        st.start(&mut out).unwrap();
        st.consume(
            &mut out,
            &chunk(json!({
                "choices":[{"index":0,"delta":{"content":"x"}}],
                "usage":{"prompt_tokens":10,"completion_tokens":3}
            })),
        )
        .unwrap();
        st.finish(&mut out).unwrap();
        let frames = out.take();
        let ps = payloads(&frames);
        // message_delta 的 usage.output_tokens
        let md = ps
            .iter()
            .find(|p| p["type"] == json!("message_delta"))
            .unwrap();
        assert_eq!(md["usage"]["output_tokens"], json!(3));
        assert_eq!(st.usage().unwrap()["completion_tokens"], json!(3));
    }

    /// 空流也能产出合法骨架（不 panic、事件齐全）。
    #[test]
    fn empty_stream_still_emits_skeleton() {
        let mut out = SseOut::new();
        let st = AnthropicStreamState::new("m");
        st.start(&mut out).unwrap();
        st.finish(&mut out).unwrap();
        let frames = out.take();
        assert_eq!(count_events(&frames, "message_start"), 1);
        assert_eq!(count_events(&frames, "message_delta"), 1);
        assert_eq!(count_events(&frames, "message_stop"), 1);
        assert_eq!(count_events(&frames, "content_block_start"), 0);
    }

    #[test]
    fn error_event_shape() {
        let mut out = SseOut::new();
        let st = AnthropicStreamState::new("m");
        st.write_error(&mut out, "boom").unwrap();
        let frames = out.take();
        assert_eq!(count_events(&frames, "error"), 1);
        let ps = payloads(&frames);
        assert_eq!(ps[0]["type"], json!("error"));
        assert_eq!(ps[0]["error"]["type"], json!("upstream_error"));
        assert_eq!(ps[0]["error"]["message"], json!("boom"));
    }

    /// 上游在 HTTP 200 的流中途发 `{"error":{...}}` 表示终止性失败。
    ///
    /// 忽略它会照常补出 `message_stop`，等于向客户端宣告「模型正常答完了」，
    /// Claude Code 会把截断的回答当成一次成功回合 —— 必须判为失败。
    #[test]
    fn mid_stream_error_frame_is_treated_as_failure() {
        let mut out = SseOut::new();
        let mut st = AnthropicStreamState::new("m");
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"content":"partial"}}]})),
        )
        .unwrap();
        // 上游中途报错
        st.consume(
            &mut out,
            &chunk(json!({"error":{"message":"channel not approved","code":11128}})),
        )
        .unwrap();

        let msg = st
            .failure_message(true, true)
            .expect("error 帧必须判为失败");
        assert!(msg.contains("channel not approved"), "{msg}");
    }

    /// error 帧优先于 [DONE]：即使后来收到了 [DONE]，根因也要报出来。
    #[test]
    fn error_frame_wins_over_done() {
        let mut out = SseOut::new();
        let mut st = AnthropicStreamState::new("m");
        st.consume(&mut out, &chunk(json!({"error":{"msg":"boom"}})))
            .unwrap();
        assert!(st.failure_message(true, true).is_some(), "error 应优先");
    }

    /// 见过 [DONE] 且无 error 帧 → 正常收尾。
    #[test]
    fn done_means_normal_end() {
        let st = AnthropicStreamState::new("m");
        assert!(st.failure_message(true, true).is_none());
    }

    /// 空流（一个 data 帧都没读到）→ 正常收尾（合法空回合，无损）。
    #[test]
    fn empty_stream_is_not_a_failure() {
        let st = AnthropicStreamState::new("m");
        assert!(st.failure_message(false, false).is_none());
    }

    /// 吐过内容却没见过 [DONE] 就断了（空闲超时 / 上游提前关连接）→ 失败。
    ///
    /// 不能拿 finish_reason 当收尾标志：上游是先给 finish_reason 再写 [DONE]，
    /// 而那一瞬正是空闲超时最容易掐断的位置。
    #[test]
    fn truncated_stream_without_done_is_a_failure() {
        let st = AnthropicStreamState::new("m");
        let msg = st
            .failure_message(false, true)
            .expect("吐了内容却没 [DONE] 必须判为失败");
        assert!(msg.contains("before [DONE]"), "{msg}");
    }

    /// error 帧文案抽取兼容多种形态。
    #[test]
    fn error_frame_text_variants() {
        assert_eq!(
            error_frame_text(&json!({"message":"m1"})),
            "m1",
            "OpenAI 形状"
        );
        assert_eq!(error_frame_text(&json!({"msg":"m2"})), "m2", "中转形状");
        assert_eq!(
            error_frame_text(&json!({"detail":"m3"})),
            "m3",
            "detail 形状"
        );
        assert_eq!(
            error_frame_text(&json!({"error_description":"m4"})),
            "m4"
        );
        // 只有 code 时退一步带上它（便于对照上游错误码表）
        assert_eq!(error_frame_text(&json!({"code":11128})), "code=11128");
        // 裸字符串
        assert_eq!(error_frame_text(&json!("raw")), "raw");
        // 都取不到时保证失败可见
        assert_eq!(error_frame_text(&json!({})), "unknown upstream error");
    }
}
