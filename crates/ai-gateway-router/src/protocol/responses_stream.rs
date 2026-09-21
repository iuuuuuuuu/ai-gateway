//! Responses SSE 翻译：把上游的 OpenAI Chat SSE 实时转成 Responses SSE。
//!
//! 对应 Go 源文件 `internal/server/responses_stream.go`。
//!
//! # 事件序列是硬要求
//!
//! Codex 恒以 `stream: true` 调用 `/v1/responses`，且对事件序列有硬性要求：
//! 必须先有 `response.created`，随后是 `output_item.added` /
//! `output_text.delta`，结束时以 `response.completed` 收尾并携带 `usage`。
//! 缺事件会导致 Codex 报 `stream closed before response.completed`。
//!
//! # 三处非显然的约束
//!
//! 1. **`response.output_item.done` 不能省**：Codex 0.146 仅在
//!    `output_item.done` 时把输出项收进会话状态（不消费 `output_item.added` /
//!    `output_text.done`），缺它会导致 `last_agent_message` 为空、终端不显示回复。
//! 2. **`response.function_call_arguments.delta` 必须落在已 `output_item.added`
//!    的 item 上**。上游允许 `function.arguments` 先于 `name`/`id` 到达，此时直接发
//!    delta 会让客户端收到指向未声明 item 的增量 —— 参数要先暂存，added 后补发。
//! 3. **`response.failed` 与 `response.completed` 只能发一个**。上游中途断流时
//!    必须发 `failed` 并**提前返回**：继续走成功收尾会补出 `completed`，
//!    而客户端只认最后一个事件，于是半截回复被当成正常完成。
//!
//! 本实现逐帧转换、不缓冲整个响应，因此首字节延迟与原生 chat 透传一致。

use serde_json::{json, Map, Value};

use crate::protocol::responses::responses_usage;
use crate::protocol::{num_of, str_field};

/// 单个工具调用的累积状态。
#[derive(Debug, Default)]
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
    /// 暂存「先于 `output_item.added` 到达」的参数分片。
    pending: String,
    /// 是否已发出 `output_item.added`。
    added: bool,
}

/// 一个 reasoning 输出项的累积状态。
///
/// 上游（DeepSeek 等推理模型）把思维链放在 `delta.reasoning_content` 里，
/// 且**先于**正文 `delta.content` 输出。它必须映射成 Responses 协议里
/// 独立的 `reasoning` item —— 缺少它会让「思考阶段」在网关侧变成一段
/// 完全静默的空档（实测可达 20 s 以上），客户端/中间层按空闲超时掐断连接，
/// 表现为「回答写到一半突然断流」。
#[derive(Debug, Default)]
struct ReasoningItem {
    id: String,
    /// 在 `output` 数组中的下标（见 [`ResponsesStreamState::alloc_slot`]）。
    index: usize,
    /// 已累积的推理正文。
    text: String,
    /// 是否已发出 `output_item.added` 与 `output_item.done`。
    added: bool,
    done: bool,
}

/// `output` 数组里的槽位。
///
/// 顺序即 `output_index` 的分配顺序：reasoning / 正文 / 各工具调用**共享**
/// 同一个递增下标空间，绝不能各自从 0 开始（客户端按 (response, output_index)
/// 对齐 item，撞下标会让增量落到错误的 item 上）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemSlot {
    Reasoning(usize),
    Text,
    Tool(i64),
}

/// 流式转换的累积状态。
pub struct ResponsesStreamState {
    response_id: String,
    model: String,
    created_at: i64,
    text: String,
    text_started: bool,
    text_item_id: String,
    sequence: i64,
    tool_calls: std::collections::BTreeMap<i64, ToolCall>,
    tool_order: Vec<i64>,
    usage: Option<Value>,
    /// 上游在流中途发过终止性 error 帧时的原因；非空表示本次必须按失败收尾。
    upstream_err: Option<String>,
    /// 已开启的 reasoning 输出项（按出现顺序，下标即 `ItemSlot::Reasoning(i)`）。
    reasoning_items: Vec<ReasoningItem>,
    /// `output` 数组各槽位的分配顺序（决定 `output_index`）。
    items: Vec<ItemSlot>,
}

impl ResponsesStreamState {
    /// 新建状态。
    pub fn new(model: &str) -> Self {
        Self {
            response_id: "resp_wb2api".into(),
            model: model.to_string(),
            created_at: 0,
            text: String::new(),
            text_started: false,
            text_item_id: String::new(),
            sequence: 0,
            tool_calls: std::collections::BTreeMap::new(),
            tool_order: Vec::new(),
            usage: None,
            upstream_err: None,
            reasoning_items: Vec::new(),
            items: Vec::new(),
        }
    }

    /// 分配（或复用）一个输出槽位，返回它在 `output` 数组中的下标。
    ///
    /// reasoning / 正文 / 工具三者共用同一递增下标空间；重复调用同一槽位
    /// 返回同一个下标（幂等），因此可安全地在每个分片上调用。
    fn alloc_slot(&mut self, slot: ItemSlot) -> usize {
        if let Some(i) = self.items.iter().position(|s| *s == slot) {
            return i;
        }
        self.items.push(slot);
        self.items.len() - 1
    }

    /// 取一个已分配槽位的下标（未分配时按「追加」预估，不改变状态）。
    fn slot_index(&self, slot: ItemSlot) -> usize {
        self.items
            .iter()
            .position(|s| *s == slot)
            .unwrap_or(self.items.len())
    }

    /// 递增并返回序号。
    fn seq(&mut self) -> i64 {
        let n = self.sequence;
        self.sequence += 1;
        n
    }

    /// 组装一个 Responses 响应对象。
    ///
    /// **必须携带 usage**：Codex 只在 `response.completed.response.usage` 里读
    /// token 用量，缺失会让每一轮的用量显示为 0/未知。`usage` 未知时
    /// [`responses_usage`] 返回全 0 而不是省略字段。
    fn snapshot(&self, status: &str, output: Option<Vec<Value>>) -> Value {
        json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "model": self.model,
            "output": output.unwrap_or_default(),
            "usage": responses_usage(self.usage.as_ref()),
        })
    }

    /// 发出 `response.created`。必须最先发。
    pub fn created_event(&mut self, out: &mut super::anthropic_stream::SseOut) -> Result<(), String> {
        let seq = self.seq();
        let snap = self.snapshot("in_progress", None);
        out.write(
            "response.created",
            &json!({
                "type": "response.created",
                "sequence_number": seq,
                "response": snap,
            }),
        )
    }

    /// 发出一个 `response.in_progress` 心跳。
    ///
    /// 用途：上游思考阶段可能长时间只有 reasoning 增量、甚至完全静默，而任何
    /// 中间层（含 Codex 自带的 5 min idle timeout）都会在长时间收不到**事件**时
    /// 掐断连接。注意 SSE 注释帧（`: ping`）不算事件 —— `eventsource-stream`
    /// 把注释丢弃、`stream.next()` 根本不返回，因此心跳必须是**真事件**。
    /// `response.in_progress` 在 Codex 侧是显式忽略的空操作，安全。
    pub fn heartbeat(&mut self, out: &mut super::anthropic_stream::SseOut) -> Result<(), String> {
        let seq = self.seq();
        let snap = self.snapshot("in_progress", None);
        out.write(
            "response.in_progress",
            &json!({
                "type": "response.in_progress",
                "sequence_number": seq,
                "response": snap,
            }),
        )
    }

    /// 开启文本输出项（幂等）。
    fn start_text_item(&mut self, out: &mut super::anthropic_stream::SseOut) -> Result<(), String> {
        if self.text_started {
            return Ok(());
        }
        self.text_started = true;
        self.text_item_id = format!("msg_{}", self.response_id);
        let seq = self.seq();
        let item_id = self.text_item_id.clone();
        let output_index = self.alloc_slot(ItemSlot::Text);
        out.write(
            "response.output_item.added",
            &json!({
                "type": "response.output_item.added",
                "sequence_number": seq,
                "output_index": output_index,
                "item": {
                    "id": item_id,
                    "type": "message",
                    "role": "assistant",
                    "status": "in_progress",
                    "content": [],
                },
            }),
        )
    }

    /// 处理单个 chat SSE chunk。
    pub fn consume(
        &mut self,
        out: &mut super::anthropic_stream::SseOut,
        chunk: &Map<String, Value>,
    ) -> Result<(), String> {
        // 上游在 HTTP 200 的流**中途**发 `{"error":{...}}` 表示终止性失败。
        // 忽略它会照常补出 response.completed，客户端把截断的回答当成正常完成。
        if let Some(e) = chunk.get("error") {
            if !e.is_null() {
                self.upstream_err = Some(super::anthropic_stream::error_frame_text(e));
            }
        }
        if let Some(id) = chunk.get("id").and_then(|v| v.as_str()) {
            if !id.is_empty() && self.response_id == "resp_wb2api" {
                self.response_id = id.to_string();
            }
        }
        if self.created_at == 0 {
            self.created_at = chunk.get("created").map(num_of).unwrap_or(0);
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
            if let Some(delta) = choice.get("delta").and_then(|d| d.as_object()) {
                // 推理增量：开/续 reasoning item，绝不混进正文（见 ReasoningItem）。
                let reasoning = str_field(delta, "reasoning_content");
                if !reasoning.is_empty() {
                    self.write_reasoning_delta(out, &reasoning)?;
                }
                // 正文一律先关掉 reasoning item：Codex 只认「最后一个
                // output_item.added」为 active item，正文增量落在 reasoning item
                // 上会被当成推理正文而非回答。
                let text = str_field(delta, "content");
                if !text.is_empty() {
                    self.close_reasoning_item(out)?;
                    self.start_text_item(out)?;
                    self.text.push_str(&text);
                    let seq = self.seq();
                    let item_id = self.text_item_id.clone();
                    let output_index = self.slot_index(ItemSlot::Text);
                    out.write(
                        "response.output_text.delta",
                        &json!({
                            "type": "response.output_text.delta",
                            "sequence_number": seq,
                            "item_id": item_id,
                            "output_index": output_index,
                            "content_index": 0,
                            "delta": text,
                        }),
                    )?;
                }
                if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                    if !tcs.is_empty() {
                        self.close_reasoning_item(out)?;
                    }
                    for tci in tcs {
                        let Some(call) = tci.as_object() else { continue };
                        self.consume_tool_call(out, call)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// 处理一个 tool_call 分片。
    fn consume_tool_call(
        &mut self,
        out: &mut super::anthropic_stream::SseOut,
        call: &Map<String, Value>,
    ) -> Result<(), String> {
        let idx = num_of(call.get("index").unwrap_or(&Value::Null));
        if !self.tool_calls.contains_key(&idx) {
            self.tool_order.push(idx);
            self.tool_calls.insert(idx, ToolCall::default());
            self.alloc_slot(ItemSlot::Tool(idx));
        }
        let id = str_field(call, "id");
        let fn_obj = call.get("function").and_then(|f| f.as_object());
        let name = fn_obj.map(|f| str_field(f, "name")).unwrap_or_default();
        let args = fn_obj.map(|f| str_field(f, "arguments")).unwrap_or_default();

        // 更新累积状态（借用在此作用域内结束，避免与后续 self.seq() 冲突）。
        let should_add = {
            let tc = self.tool_calls.get_mut(&idx).expect("刚插入");
            if !id.is_empty() {
                tc.id = id;
            }
            if !name.is_empty() {
                tc.name = name;
            }
            if !args.is_empty() {
                tc.arguments.push_str(&args);
            }
            !tc.added && (!tc.name.is_empty() || !tc.id.is_empty())
        };

        // 首个带 name/id 的分片补 output_item.added；后续分片只发 arguments.delta。
        if should_add {
            let seq = self.seq();
            let output_index = self.tool_output_index(idx);
            let (tc_id, tc_name, pending) = {
                let tc = self.tool_calls.get_mut(&idx).expect("存在");
                tc.added = true;
                (
                    tc.id.clone(),
                    tc.name.clone(),
                    std::mem::take(&mut tc.pending),
                )
            };
            out.write(
                "response.output_item.added",
                &json!({
                    "type": "response.output_item.added",
                    "sequence_number": seq,
                    "output_index": output_index,
                    "item": {
                        "type": "function_call",
                        "id": tc_id,
                        "call_id": tc_id,
                        "name": tc_name,
                        "arguments": "",
                        "status": "in_progress",
                    },
                }),
            )?;
            // 补发先于 added 到达的参数分片（顺序合法，内容不丢）。
            if !pending.is_empty() {
                self.write_tool_args_delta(out, idx, &pending)?;
            }
        }

        if !args.is_empty() {
            let added = self.tool_calls.get(&idx).map(|t| t.added).unwrap_or(false);
            if added {
                self.write_tool_args_delta(out, idx, &args)?;
            } else if let Some(tc) = self.tool_calls.get_mut(&idx) {
                tc.pending.push_str(&args);
            }
        }
        Ok(())
    }

    /// 工具项在 `output` 数组中的下标（与 reasoning / 正文共用递增下标空间）。
    fn tool_output_index(&self, idx: i64) -> usize {
        self.slot_index(ItemSlot::Tool(idx))
    }

    /// 发出一个 `response.function_call_arguments.delta`。
    fn write_tool_args_delta(
        &mut self,
        out: &mut super::anthropic_stream::SseOut,
        idx: i64,
        args: &str,
    ) -> Result<(), String> {
        let seq = self.seq();
        let output_index = self.tool_output_index(idx);
        let item_id = self.tool_calls.get(&idx).map(|t| t.id.clone()).unwrap_or_default();
        out.write(
            "response.function_call_arguments.delta",
            &json!({
                "type": "response.function_call_arguments.delta",
                "sequence_number": seq,
                "item_id": item_id,
                "output_index": output_index,
                "delta": args,
            }),
        )
    }

    /// 开启（或复用）当前 reasoning 输出项，返回它在 `reasoning_items` 中的下标。
    ///
    /// `ReasoningItemReasoningSummary` 在 Codex 侧**没有** `serde(default)`，
    /// 因此 `summary` 是必填字段 —— 缺它会整条 item 反序列化失败（只记一条
    /// debug 日志），表现与「没有推理」无异。
    fn open_reasoning_item(
        &mut self,
        out: &mut super::anthropic_stream::SseOut,
    ) -> Result<usize, String> {
        let need_new = self.reasoning_items.last().map(|r| r.done).unwrap_or(true);
        if need_new {
            let id = format!(
                "rs{}_{}",
                self.reasoning_items.len(),
                self.response_id
            );
            self.reasoning_items.push(ReasoningItem {
                id,
                ..Default::default()
            });
        }
        let i = self.reasoning_items.len() - 1;
        if self.reasoning_items[i].added {
            return Ok(i);
        }
        let index = self.alloc_slot(ItemSlot::Reasoning(i));
        self.reasoning_items[i].added = true;
        self.reasoning_items[i].index = index;
        let seq = self.seq();
        let id = self.reasoning_items[i].id.clone();
        out.write(
            "response.output_item.added",
            &json!({
                "type": "response.output_item.added",
                "sequence_number": seq,
                "output_index": index,
                "item": {
                    "id": id,
                    "type": "reasoning",
                    "summary": [],
                },
            }),
        )?;
        Ok(i)
    }

    /// 写入一段推理增量。
    ///
    /// 用 `response.reasoning_text.delta`（原始推理正文）而非
    /// `response.reasoning_summary_text.delta`：后者在 Codex 侧必须带
    /// `summary_index`，缺了会被整帧丢弃。
    fn write_reasoning_delta(
        &mut self,
        out: &mut super::anthropic_stream::SseOut,
        delta: &str,
    ) -> Result<(), String> {
        let i = self.open_reasoning_item(out)?;
        self.reasoning_items[i].text.push_str(delta);
        let index = self.reasoning_items[i].index;
        let id = self.reasoning_items[i].id.clone();
        let seq = self.seq();
        out.write(
            "response.reasoning_text.delta",
            &json!({
                "type": "response.reasoning_text.delta",
                "sequence_number": seq,
                "item_id": id,
                "output_index": index,
                "content_index": 0,
                "delta": delta,
            }),
        )
    }

    /// 关闭当前 reasoning 输出项（幂等）。
    ///
    /// 必须显式关：Codex 只认最后一个 `output_item.added` 为 active item，
    /// 不关就开正文 item，正文增量会被算进推理正文里。
    fn close_reasoning_item(
        &mut self,
        out: &mut super::anthropic_stream::SseOut,
    ) -> Result<(), String> {
        let Some(i) = self.reasoning_items.len().checked_sub(1) else {
            return Ok(());
        };
        if !self.reasoning_items[i].added || self.reasoning_items[i].done {
            return Ok(());
        }
        self.reasoning_items[i].done = true;
        let index = self.reasoning_items[i].index;
        let seq = self.seq();
        let item = Self::reasoning_item_value(&self.reasoning_items[i]);
        out.write(
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "sequence_number": seq,
                "output_index": index,
                "item": item,
            }),
        )
    }

    /// 组装完整的 `reasoning` 输出项。
    fn reasoning_item_value(r: &ReasoningItem) -> Value {
        json!({
            "id": r.id,
            "type": "reasoning",
            "summary": [],
            "content": [{"type": "reasoning_text", "text": r.text}],
        })
    }

    /// 组装完整的 assistant message 输出项。
    fn text_message_item(&self) -> Value {
        json!({
            "id": self.text_item_id,
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{
                "type": "output_text",
                "text": self.text,
                "annotations": [],
            }],
        })
    }

    /// 组装完整的 `function_call` 输出项。
    fn function_call_item(tc: &ToolCall) -> Value {
        json!({
            "type": "function_call",
            "id": tc.id,
            "call_id": tc.id,
            "name": tc.name,
            "arguments": tc.arguments,
            "status": "completed",
        })
    }

    /// 收尾输出项列表（按槽位分配顺序，与各事件里的 `output_index` 一致）。
    fn output_items(&self) -> Vec<Value> {
        let mut items: Vec<Value> = Vec::with_capacity(self.items.len());
        for slot in &self.items {
            match slot {
                ItemSlot::Reasoning(i) => {
                    if let Some(r) = self.reasoning_items.get(*i) {
                        items.push(Self::reasoning_item_value(r));
                    }
                }
                ItemSlot::Text => {
                    if self.text_started {
                        items.push(self.text_message_item());
                    }
                }
                ItemSlot::Tool(idx) => {
                    if let Some(tc) = self.tool_calls.get(idx) {
                        items.push(Self::function_call_item(tc));
                    }
                }
            }
        }
        items
    }

    /// 为每个输出项补发 `response.output_item.done`。
    ///
    /// Codex 0.146 只在 `output_item.done` 时把输出项收进会话状态，
    /// 缺它会导致终端不显示回复（见模块文档第 1 条）。
    pub fn finish_items(&mut self, out: &mut super::anthropic_stream::SseOut) -> Result<(), String> {
        for slot in self.items.clone() {
            let (index, item) = match slot {
                // reasoning 的 done 在 close_reasoning_item 时已发（正文一开始
                // 就把它关掉，不能拖到这里 —— 否则正文增量会落到推理 item 上）。
                ItemSlot::Reasoning(_) => continue,
                ItemSlot::Text => {
                    if !self.text_started {
                        continue;
                    }
                    (self.slot_index(ItemSlot::Text), self.text_message_item())
                }
                ItemSlot::Tool(idx) => {
                    let Some(tc) = self.tool_calls.get(&idx) else {
                        continue;
                    };
                    (self.slot_index(ItemSlot::Tool(idx)), Self::function_call_item(tc))
                }
            };
            let seq = self.seq();
            out.write(
                "response.output_item.done",
                &json!({
                    "type": "response.output_item.done",
                    "sequence_number": seq,
                    "output_index": index,
                    "item": item,
                }),
            )?;
        }
        Ok(())
    }

    /// 成功收尾：`output_text.done` → 每个 item 的 `done` → `response.completed`。
    pub fn finish(&mut self, out: &mut super::anthropic_stream::SseOut) -> Result<(), String> {
        // 只推理没有正文（或上游在此断掉）时也要把 reasoning item 关上，
        // 否则 finished 状态里留着一个 added 未 done 的 item。
        self.close_reasoning_item(out)?;
        if !self.text_started {
            self.start_text_item(out)?;
        }
        let seq = self.seq();
        let item_id = self.text_item_id.clone();
        let output_index = self.slot_index(ItemSlot::Text);
        let text = self.text.clone();
        out.write(
            "response.output_text.done",
            &json!({
                "type": "response.output_text.done",
                "sequence_number": seq,
                "item_id": item_id,
                "output_index": output_index,
                "content_index": 0,
                "text": text,
            }),
        )?;
        self.finish_items(out)?;
        let seq = self.seq();
        let snap = self.snapshot("completed", Some(self.output_items()));
        out.write(
            "response.completed",
            &json!({
                "type": "response.completed",
                "sequence_number": seq,
                "response": snap,
            }),
        )
    }

    /// 失败收尾：发 `response.failed`。
    ///
    /// **调用方必须在此之后立即 return**，不能再发 `completed`
    ///（见模块文档第 3 条）。
    pub fn fail(&mut self, out: &mut super::anthropic_stream::SseOut, msg: &str) -> Result<(), String> {
        let seq = self.seq();
        let mut snap = self.snapshot("failed", Some(Vec::new()));
        snap["error"] = json!({"code": "upstream_error", "message": msg});
        out.write(
            "response.failed",
            &json!({
                "type": "response.failed",
                "sequence_number": seq,
                "response": snap,
            }),
        )
    }

    /// 本次累积的 usage。
    pub fn usage(&self) -> Option<&Value> {
        self.usage.as_ref()
    }

    /// 判定本次流是否应当按**失败**收尾（口径同
    /// [`crate::protocol::anthropic_stream::AnthropicStreamState::failure_message`]）。
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::anthropic_stream::SseOut;

    fn chunk(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn count_events(frames: &str, event: &str) -> usize {
        frames
            .lines()
            .filter(|l| *l == format!("event: {event}"))
            .count()
    }

    fn payloads(frames: &str) -> Vec<Value> {
        frames
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|d| serde_json::from_str::<Value>(d).ok())
            .collect()
    }

    /// 文本流的事件序列：created → item.added → text.delta* → text.done →
    /// item.done → completed。
    #[test]
    fn text_stream_event_sequence() {
        let mut out = SseOut::new();
        let mut st = ResponsesStreamState::new("gpt-5.6");
        st.created_event(&mut out).unwrap();
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"content":"Hel"}}]})),
        )
        .unwrap();
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"content":"lo"}}]})),
        )
        .unwrap();
        st.finish(&mut out).unwrap();

        let frames = out.take();
        let events: Vec<String> = frames
            .lines()
            .filter_map(|l| l.strip_prefix("event: ").map(str::to_string))
            .collect();
        assert_eq!(
            events,
            vec![
                "response.created",
                "response.output_item.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        assert_eq!(count_events(&frames, "response.completed"), 1);
    }

    /// `output_item.done` 不能省 —— Codex 只在它到达时收进会话状态。
    #[test]
    fn output_item_done_is_emitted() {
        let mut out = SseOut::new();
        let mut st = ResponsesStreamState::new("m");
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"content":"x"}}]})),
        )
        .unwrap();
        st.finish(&mut out).unwrap();
        let frames = out.take();
        assert_eq!(count_events(&frames, "response.output_item.done"), 1);
    }

    /// `response.completed` 必须携带 usage（Codex 只从这里读用量）。
    #[test]
    fn completed_carries_usage() {
        let mut out = SseOut::new();
        let mut st = ResponsesStreamState::new("m");
        st.consume(
            &mut out,
            &chunk(json!({
                "choices":[{"index":0,"delta":{"content":"x"}}],
                "usage":{"prompt_tokens":11,"completion_tokens":4,"total_tokens":15}
            })),
        )
        .unwrap();
        st.finish(&mut out).unwrap();
        let frames = out.take();
        let ps = payloads(&frames);
        let done = ps
            .iter()
            .find(|p| p["type"] == json!("response.completed"))
            .unwrap();
        assert_eq!(done["response"]["usage"]["input_tokens"], json!(11));
        assert_eq!(done["response"]["usage"]["output_tokens"], json!(4));
        assert_eq!(done["response"]["status"], json!("completed"));
        // output 里带完整文本项
        assert_eq!(done["response"]["output"][0]["content"][0]["text"], json!("x"));
    }

    /// 序号必须单调递增。
    #[test]
    fn sequence_numbers_increase() {
        let mut out = SseOut::new();
        let mut st = ResponsesStreamState::new("m");
        st.created_event(&mut out).unwrap();
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"content":"a"}}]})),
        )
        .unwrap();
        st.finish(&mut out).unwrap();
        let ps = payloads(&out.take());
        let seqs: Vec<i64> = ps
            .iter()
            .filter_map(|p| p["sequence_number"].as_i64())
            .collect();
        assert!(seqs.len() >= 4, "{seqs:?}");
        for w in seqs.windows(2) {
            assert!(w[1] > w[0], "序号必须递增: {seqs:?}");
        }
    }

    /// 工具调用：item.added 带 name/call_id，参数以 arguments.delta 下发。
    #[test]
    fn tool_call_stream() {
        let mut out = SseOut::new();
        let mut st = ResponsesStreamState::new("m");
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"id":"c1","function":{"name":"f","arguments":"{\"a\""}}
            ]}}]})),
        )
        .unwrap();
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":":1}"}}
            ]}}]})),
        )
        .unwrap();
        st.finish(&mut out).unwrap();

        let frames = out.take();
        let ps = payloads(&frames);
        let added = ps
            .iter()
            .find(|p| p["type"] == json!("response.output_item.added") && p["item"]["type"] == json!("function_call"))
            .unwrap();
        assert_eq!(added["item"]["name"], json!("f"));
        assert_eq!(added["item"]["call_id"], json!("c1"));

        let deltas: Vec<&Value> = ps
            .iter()
            .filter(|p| p["type"] == json!("response.function_call_arguments.delta"))
            .collect();
        assert_eq!(deltas.len(), 2, "{ps:?}");
        assert_eq!(deltas[0]["delta"], json!(r#"{"a""#));
        assert_eq!(deltas[1]["delta"], json!(r#":1}"#));

        // completed 里带完整 arguments
        let done = ps
            .iter()
            .find(|p| p["type"] == json!("response.completed"))
            .unwrap();
        let fcall = done["response"]["output"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["type"] == json!("function_call"))
            .unwrap();
        assert_eq!(fcall["arguments"], json!(r#"{"a":1}"#));
    }

    /// **关键顺序约束**：arguments 先于 name 到达时，delta 绝不能先于 item.added。
    #[test]
    fn args_before_name_never_precedes_item_added() {
        let mut out = SseOut::new();
        let mut st = ResponsesStreamState::new("m");
        // 只有 arguments
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"{\"a\""}}
            ]}}]})),
        )
        .unwrap();
        // name/id 到达
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"id":"c1","function":{"name":"f","arguments":":1}"}}
            ]}}]})),
        )
        .unwrap();

        let frames = out.take();
        let events: Vec<&str> = frames
            .lines()
            .filter_map(|l| l.strip_prefix("event: "))
            .collect();
        // 首个事件必须是 output_item.added，不能是 arguments.delta
        assert_eq!(
            events.first(),
            Some(&"response.output_item.added"),
            "delta 不得先于 item.added: {events:?}"
        );
        // 两段参数都被补发
        let ps = payloads(&frames);
        let joined: String = ps
            .iter()
            .filter(|p| p["type"] == json!("response.function_call_arguments.delta"))
            .filter_map(|p| p["delta"].as_str())
            .collect();
        assert_eq!(joined, r#"{"a":1}"#, "参数必须完整补发");
    }

    /// 失败收尾只发 `response.failed`，绝不发 `completed`。
    #[test]
    fn failure_emits_failed_not_completed() {
        let mut out = SseOut::new();
        let mut st = ResponsesStreamState::new("m");
        st.created_event(&mut out).unwrap();
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"content":"partial"}}]})),
        )
        .unwrap();
        st.fail(&mut out, "upstream stream error").unwrap();
        let frames = out.take();
        assert_eq!(count_events(&frames, "response.failed"), 1);
        assert_eq!(
            count_events(&frames, "response.completed"),
            0,
            "失败时绝不能发 completed（两者自相矛盾）"
        );
        let ps = payloads(&frames);
        let failed = ps
            .iter()
            .find(|p| p["type"] == json!("response.failed"))
            .unwrap();
        assert_eq!(failed["response"]["status"], json!("failed"));
        assert_eq!(failed["response"]["error"]["code"], json!("upstream_error"));
    }

    /// 无文本输出时 finish 也要产出合法的文本项骨架。
    #[test]
    fn finish_without_text_still_emits_item() {
        let mut out = SseOut::new();
        let mut st = ResponsesStreamState::new("m");
        st.finish(&mut out).unwrap();
        let frames = out.take();
        assert_eq!(count_events(&frames, "response.output_item.added"), 1);
        assert_eq!(count_events(&frames, "response.output_item.done"), 1);
        assert_eq!(count_events(&frames, "response.completed"), 1);
    }

    /// 文本项占 output_index 0，工具项从 1 起。
    #[test]
    fn output_indices_do_not_collide() {
        let mut out = SseOut::new();
        let mut st = ResponsesStreamState::new("m");
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"content":"hi"}}]})),
        )
        .unwrap();
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"id":"c1","function":{"name":"f","arguments":"{}"}}
            ]}}]})),
        )
        .unwrap();
        st.finish(&mut out).unwrap();
        let ps = payloads(&out.take());
        let text_idx = ps
            .iter()
            .find(|p| p["type"] == json!("response.output_item.added") && p["item"]["type"] == json!("message"))
            .map(|p| p["output_index"].clone())
            .unwrap();
        let tool_idx = ps
            .iter()
            .find(|p| p["type"] == json!("response.output_item.added") && p["item"]["type"] == json!("function_call"))
            .map(|p| p["output_index"].clone())
            .unwrap();
        assert_eq!(text_idx, json!(0));
        assert_eq!(tool_idx, json!(1));
        assert_ne!(text_idx, tool_idx);
    }

    /// 响应 id 从上游 chunk 继承（首次非空时）。
    #[test]
    fn response_id_inherits_from_upstream() {
        let mut out = SseOut::new();
        let mut st = ResponsesStreamState::new("m");
        st.created_event(&mut out).unwrap();
        st.consume(
            &mut out,
            &chunk(json!({"id":"chatcmpl-real","choices":[{"index":0,"delta":{"content":"x"}}]})),
        )
        .unwrap();
        st.finish(&mut out).unwrap();
        let ps = payloads(&out.take());
        let done = ps
            .iter()
            .find(|p| p["type"] == json!("response.completed"))
            .unwrap();
        assert_eq!(done["response"]["id"], json!("chatcmpl-real"));
    }
    /// **回归**：上游的 `reasoning_content` 必须转成可见事件，不能是静默空档。
    ///
    /// 修复前的行为是「整段推理一个字都不发」，思考阶段在网关侧表现为
    /// 十几秒的零字节空档，客户端/中间层按空闲超时掐断连接 —— 用户看到
    /// 「回答突然断流」。这里断言推理阶段**至少**有 added + delta + done。
    #[test]
    fn reasoning_content_is_streamed_not_dropped() {
        let mut out = SseOut::new();
        let mut st = ResponsesStreamState::new("m");
        st.created_event(&mut out).unwrap();
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"reasoning_content":"想一下"}}]})),
        )
        .unwrap();
        let frames = out.take();

        let events: Vec<String> = frames
            .lines()
            .filter_map(|l| l.strip_prefix("event: ").map(str::to_string))
            .collect();
        assert_eq!(
            events,
            vec![
                "response.created",
                "response.output_item.added",
                "response.reasoning_text.delta",
            ],
            "推理阶段必须有可见事件（否则客户端会因空闲超时断流）"
        );

        let ps = payloads(&frames);
        let added = ps
            .iter()
            .find(|p| p["type"] == json!("response.output_item.added"))
            .unwrap();
        assert_eq!(added["item"]["type"], json!("reasoning"));
        // `summary` 在 Codex 侧是必填字段（无 serde(default)），缺了整条 item
        // 反序列化会失败。
        assert!(
            added["item"].get("summary").is_some(),
            "reasoning item 必须带 summary 字段: {added}"
        );
        let delta = ps
            .iter()
            .find(|p| p["type"] == json!("response.reasoning_text.delta"))
            .unwrap();
        assert_eq!(delta["delta"], json!("想一下"));
        // Codex 的 ReasoningContentDelta 分支要求 content_index 存在。
        assert_eq!(delta["content_index"], json!(0));
    }

    /// 推理正文绝不能混进 assistant 正文（两者在客户端是不同的事件类型）。
    #[test]
    fn reasoning_text_is_not_merged_into_answer() {
        let mut out = SseOut::new();
        let mut st = ResponsesStreamState::new("m");
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"reasoning_content":"推理"}}]})),
        )
        .unwrap();
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"content":"答案"}}]})),
        )
        .unwrap();
        st.finish(&mut out).unwrap();

        let ps = payloads(&out.take());
        let text_deltas: String = ps
            .iter()
            .filter(|p| p["type"] == json!("response.output_text.delta"))
            .filter_map(|p| p["delta"].as_str())
            .collect();
        assert_eq!(text_deltas, "答案", "正文增量里不能混进推理内容");

        let done = ps
            .iter()
            .find(|p| p["type"] == json!("response.completed"))
            .unwrap();
        let output = done["response"]["output"].as_array().unwrap();
        let answer = output
            .iter()
            .find(|o| o["type"] == json!("message"))
            .unwrap();
        assert_eq!(answer["content"][0]["text"], json!("答案"));
        let reasoning = output
            .iter()
            .find(|o| o["type"] == json!("reasoning"))
            .unwrap();
        assert_eq!(reasoning["content"][0]["text"], json!("推理"));
    }

    /// reasoning / 正文 / 工具三者的 `output_index` 必须互不冲突，且
    /// `completed.output` 的顺序与之一致（客户端按 (response, index) 对齐 item）。
    #[test]
    fn reasoning_text_tool_indices_do_not_collide() {
        let mut out = SseOut::new();
        let mut st = ResponsesStreamState::new("m");
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"reasoning_content":"想"}}]})),
        )
        .unwrap();
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"content":"答"}}]})),
        )
        .unwrap();
        st.consume(
            &mut out,
            &chunk(json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"id":"c1","function":{"name":"f","arguments":"{}"}}
            ]}}]})),
        )
        .unwrap();
        st.finish(&mut out).unwrap();

        let ps = payloads(&out.take());
        let mut kinds: Vec<(String, i64)> = Vec::new();
        for p in ps.iter().filter(|p| p["type"] == json!("response.output_item.added")) {
            kinds.push((
                p["item"]["type"].as_str().unwrap_or_default().to_string(),
                p["output_index"].as_i64().unwrap(),
            ));
        }
        assert_eq!(
            kinds,
            vec![
                ("reasoning".to_string(), 0),
                ("message".to_string(), 1),
                ("function_call".to_string(), 2),
            ],
            "三种 item 必须共享同一递增下标空间: {kinds:?}"
        );

        // completed.output 的下标必须与各自事件里的 output_index 一一对应。
        let done = ps
            .iter()
            .find(|p| p["type"] == json!("response.completed"))
            .unwrap();
        let output = done["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 3, "{output:?}");
        assert_eq!(output[0]["type"], json!("reasoning"));
        assert_eq!(output[1]["type"], json!("message"));
        assert_eq!(output[2]["type"], json!("function_call"));
    }

    /// 心跳必须是**真事件**：SSE 注释帧不会被 `stream.next()` 返回，
    /// 因此不能用来重置 Codex 的空闲超时。
    #[test]
    fn heartbeat_emits_real_event() {
        let mut out = SseOut::new();
        let mut st = ResponsesStreamState::new("m");
        st.heartbeat(&mut out).unwrap();
        let frames = out.take();
        assert_eq!(count_events(&frames, "response.in_progress"), 1);
        let ps = payloads(&frames);
        assert_eq!(ps[0]["type"], json!("response.in_progress"));
        // Codex 侧按 kind 分支，必须能被正常解析（非注释帧）。
        assert!(ps[0]["sequence_number"].is_i64());
    }
}
