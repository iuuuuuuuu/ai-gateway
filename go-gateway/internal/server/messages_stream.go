package server

// messages_stream.go 把上游的 OpenAI Chat SSE 实时翻译成 Anthropic Messages SSE。
//
// Claude Code / Claude Desktop 对事件序列有硬要求，缺事件会直接报错：
//   message_start → content_block_start → content_block_delta* →
//   content_block_stop → message_delta → message_stop
//
// 工具调用映射为 tool_use 内容块，参数以 input_json_delta 分片下发。

import (
	"bufio"
	"io"
	"net/http"
	"sort"
	"strconv"
	"strings"
)

// streamAnthropic 把 chat SSE 转成 Anthropic SSE 写回客户端。
func (h *Handler) streamAnthropic(w http.ResponseWriter, result *chatResult, model string, stat *chatStat) {
	defer func() {
		if result.Stream != nil {
			result.Stream.Close()
		}
		h.release(result.UID)
	}()

	hdr := w.Header()
	hdr.Set("Content-Type", "text/event-stream")
	hdr.Set("Cache-Control", "no-cache")
	hdr.Set("Connection", "keep-alive")
	hdr.Set("X-Accel-Buffering", "no")

	out := newSSEWriter(w)
	ctx := newAnthropicStreamState(model)

	// message_start：必须在任何内容块之前发送，Claude 据此建立消息。
	if err := out.write("message_start", map[string]any{
		"type":    "message_start",
		"message": ctx.messageObject("", "in_progress"),
	}); err != nil {
		return
	}

	br := bufio.NewReaderSize(result.Stream, 64*1024)
	for {
		line, err := br.ReadString('\n')
		line = strings.TrimRight(line, "\r\n")
		// 注意 err 的作用域：下面 jsonUnmarshal 用的是内层 :=，不能遮蔽外层 err，
		// 否则「读到的最后一行之后 err != nil」(含 ctx 被取消) 会被吃掉，
		// 循环既不 break 也不判错，把异常断流当成正常收尾（见本文件末尾的守卫）。
		if payload, ok := strings.CutPrefix(line, "data: "); ok {
			if payload == "[DONE]" {
				ctx.sawDone = true
				break
			}
			var chunk map[string]any
			if jerr := jsonUnmarshal(payload, &chunk); jerr == nil {
				ctx.sawAnyFrame = true
				if werr := ctx.consume(out, chunk); werr != nil {
					return
				}
			}
		}
		if err != nil {
			break
		}
	}

	// 上游在 HTTP 200 的流中途发 {"error":{...}} 表示终止性失败（渠道未批准、
	// 账号被封，见 upstream/sse.go 的 normalizeFrame 注释），读取循环还会因
	// IdleTimeout 取消 ctx 而中途断开。这两种情况下都**不能**继续走正常收尾：
	// 补一个 stop_reason=end_turn 的 message_delta + message_stop，等于向客户端
	// 宣告「模型正常答完了」，Claude Code 会把空/截断的回答当成一次成功回合并
	// 继续推进对话，用户只看到模型不回答或答了半截，而网关侧 /usage 还记成
	// 成功（status 200），上游故障完全无痕。
	//
	// 对照实现：OpenAI 透传路径在 upstream/sse.go 显式保留 error 字段，
	// Responses 路径在 responses_stream.go 发 response.failed —— 只有这里漏了。
	if msg := ctx.failureMessage(); msg != "" {
		ctx.closeOpenBlocks(out)
		_ = out.write("error", toAnthropicStreamError(msg))
		return
	}

	ctx.closeOpenBlocks(out)
	if ctx.usage != nil {
		stat.toks = numOf(ctx.usage["completion_tokens"])
		stat.setUsageMap(ctx.usage)
	}
	_ = out.write("message_delta", map[string]any{
		"type": "message_delta",
		"delta": map[string]any{
			"stop_reason":   ctx.stopReason(),
			"stop_sequence": nil,
		},
		"usage": map[string]any{
			"output_tokens": numOf(ctx.usage["completion_tokens"]),
		},
	})
	_ = out.write("message_stop", map[string]any{"type": "message_stop"})
}

// toAnthropicStreamError 构造 Anthropic SSE 的终止性 error 事件体。
//
// Anthropic 规范里 error 事件本身就是终止事件，客户端收到即报错结束整个回合，
// 因此发出后不要再补 message_delta / message_stop（那会被理解为正常完成）。
func toAnthropicStreamError(msg string) map[string]any {
	return map[string]any{
		"type": "error",
		"error": map[string]any{
			"type":    "api_error",
			"message": msg,
		},
	}
}

// ---------------------------------------------------------------------------
// 流式收尾的公共判定（messages_stream.go 与 responses_stream.go 共用）
// ---------------------------------------------------------------------------

// streamFailureMessage 判定一次上游流是否应当按失败收尾，返回原因（空串 = 可正常收尾）。
//
// 三种情形：
//   - 上游显式发过终止性 error 帧 → 失败（根因优先，先报它）。
//   - 见过 [DONE] → 正常收尾。
//   - 没见 [DONE]：
//   - 整条流一个 data 帧都没读到（空流）→ 按原有的正常收尾。上游正常解析后
//     发现没什么可发时就是这样，客户端得到一个合法的空回合，无损。
//   - 已经吐出过内容却在半路断开 → 失败。这正是 IdleTimeout 取消 ctx
//     （idle.go 的 cancel 路径）或上游提前关连接的形态，补一个
//     stop_reason=end_turn 等于告诉客户端「模型答完了」，它会据此推进对话，
//     用户只看到回答被从中间截断。
//
// 注意不能拿 finish_reason 当收尾标志：上游是在给出 finish_reason **之后**
// 才写 [DONE] 的，而那一瞬恰恰是空闲超时最容易掐断的位置（usage 也还没落下来）。
func streamFailureMessage(upstreamErr string, sawDone, sawAnyFrame bool) string {
	if upstreamErr != "" {
		return "upstream error: " + upstreamErr
	}
	if sawDone {
		return ""
	}
	if !sawAnyFrame {
		// 空流：没有内容会被截断，沿用原有行为（避免把合法空答复误报成故障）。
		return ""
	}
	return "upstream stream ended unexpectedly before [DONE]"
}

// errorFrameText 把上游流内的 {"error": ...} 帧抽成可读文案。
//
// 形态不固定：{"error":{"message":...}}（OpenAI 形状）、{"error":{"msg":...}}
// （部分中转）、以及裸字符串。逐层取第一个非空文案；都取不到时退一步带上 code
// （便于对照上游错误码表定位，如 11128 渠道未批准），保证失败至少可见。
func errorFrameText(e any) string {
	if m, ok := e.(map[string]any); ok {
		for _, k := range []string{"message", "msg", "detail", "error_description"} {
			if v := str(m[k]); v != "" {
				return v
			}
		}
		if c := numOf(m["code"]); c != 0 {
			return "code=" + strconv.Itoa(c)
		}
	}
	if v := str(e); v != "" {
		return v
	}
	return "unknown upstream error"
}

// anthropicStreamState 记录流式转换的累积状态。
type anthropicStreamState struct {
	messageID string
	model     string
	usage     map[string]any

	textOpen  bool
	textIndex int
	text      strings.Builder

	toolCalls map[int]*anthropicToolState
	toolOrder []int

	finishReason string

	// upstreamErr 非空表示上游在流中途发过终止性 error 帧（HTTP 200 +
	// {"error":{...}}），本次请求必须按失败收尾而不是补 end_turn。
	upstreamErr string
	// sawDone 记录是否读到了规范的 [DONE] 结束帧。它是「上游按约定收尾了」的
	// 权威标志 —— 不能拿 finish_reason 代替：上游可以给出 finish_reason 后
	// 才开始收尾，也可能（如空闲超时掐断）给了 finish_reason 却再没有下文。
	sawDone bool
	// sawAnyFrame 记录是否读到过任何一个可解析的 data 帧。用于区分「空流」
	// （无内容可截断，照旧按正常收尾）与「吐了一半就断」（必须报错）。
	sawAnyFrame bool
}

type anthropicToolState struct {
	id        string
	name      string
	arguments strings.Builder
	// pending 暂存「先于 name 到达」的参数分片（name 未知时块无法开启，
	// 此时不能发 delta，否则会先于 content_block_start）。name 到达后补发。
	pending strings.Builder
	index   int
	open    bool
}

func newAnthropicStreamState(model string) *anthropicStreamState {
	// messageID 每响应唯一（见 newMessageID）：客户端按 id 合并历史，
	// 恒定 id 会让不同轮次的响应被误并，破坏 tool 配对。
	return &anthropicStreamState{
		messageID: newMessageID(),
		model:     model,
		toolCalls: map[int]*anthropicToolState{},
		textIndex: 0,
	}
}

func (s *anthropicStreamState) messageObject(stopReason, status string) map[string]any {
	msg := map[string]any{
		"id":            s.messageID,
		"type":          "message",
		"role":          "assistant",
		"model":         s.model,
		"content":       []any{},
		"stop_reason":   nil,
		"stop_sequence": nil,
		"usage":         anthropicUsage(s.usage),
	}
	if stopReason != "" {
		msg["stop_reason"] = stopReason
	}
	_ = status
	return msg
}

// failureMessage 返回应当按失败收尾的原因；空串表示可以正常收尾。
func (s *anthropicStreamState) failureMessage() string {
	return streamFailureMessage(s.upstreamErr, s.sawDone, s.sawAnyFrame)
}

// noteUpstreamError 记录上游终止性 error 帧的可读文案。
func (s *anthropicStreamState) noteUpstreamError(e any) {
	if s.upstreamErr == "" {
		s.upstreamErr = errorFrameText(e)
	}
}

// consume 处理单个 chat SSE chunk。
func (s *anthropicStreamState) consume(out *sseWriter, chunk map[string]any) error {
	// 终止性 error 帧优先：它没有 choices，若直接落进下面的解析会被整个忽略，
	// 表现为「客户端收到一个成功但空洞的回合」。
	if e, ok := chunk["error"]; ok && e != nil {
		s.noteUpstreamError(e)
		return nil
	}
	if m := str(chunk["model"]); m != "" && s.model == "" {
		s.model = m
	}
	if u, ok := chunk["usage"].(map[string]any); ok && u != nil {
		s.usage = u
	}

	choices, _ := chunk["choices"].([]any)
	for _, ci := range choices {
		choice, _ := ci.(map[string]any)
		if choice == nil {
			continue
		}
		if fr := str(choice["finish_reason"]); fr != "" {
			s.finishReason = fr
		}
		delta, _ := choice["delta"].(map[string]any)
		if delta == nil {
			continue
		}
		if text := str(delta["content"]); text != "" {
			if err := s.openText(out); err != nil {
				return err
			}
			s.text.WriteString(text)
			if err := out.write("content_block_delta", map[string]any{
				"type":  "content_block_delta",
				"index": s.textIndex,
				"delta": map[string]any{"type": "text_delta", "text": text},
			}); err != nil {
				return err
			}
		}
		if tcs, ok := delta["tool_calls"].([]any); ok {
			for _, tci := range tcs {
				call, _ := tci.(map[string]any)
				if call == nil {
					continue
				}
				if err := s.consumeToolCall(out, call); err != nil {
					return err
				}
			}
		}
	}
	return nil
}

func (s *anthropicStreamState) openText(out *sseWriter) error {
	if s.textOpen {
		return nil
	}
	s.textOpen = true
	return out.write("content_block_start", map[string]any{
		"type":  "content_block_start",
		"index": s.textIndex,
		"content_block": map[string]any{
			"type": "text",
			"text": "",
		},
	})
}

// consumeToolCall 处理一个 tool_call 分片。
//
// Anthropic 协议要求 input_json_delta 必须落在「已经 content_block_start 过」的
// index 上，而 content_block_start 又必须带 name。上游允许 function.arguments 先于
// function.name 到达（分片边界由上游写入顺序决定），因此这里**不能**在 name 未知时
// 直接发 delta：那样会先于 start 发出，客户端按协议拒绝/丢弃该工具块。
//
// 处理方式：arguments 一律先累积到 tc.arguments；只有块已开启（name 已知）才发出
// input_json_delta。name 到达时会把此前累积的参数一并补发，保证内容不丢、顺序合法。
func (s *anthropicStreamState) consumeToolCall(out *sseWriter, call map[string]any) error {
	idx := numOf(call["index"])
	tc, ok := s.toolCalls[idx]
	if !ok {
		tc = &anthropicToolState{index: s.nextBlockIndex()}
		s.toolCalls[idx] = tc
		s.toolOrder = append(s.toolOrder, idx)
	}
	if id := str(call["id"]); id != "" {
		tc.id = id
	}
	fn, _ := call["function"].(map[string]any)
	if fn != nil {
		if name := str(fn["name"]); name != "" {
			tc.name = name
		}
	}

	// 拿到 name 后才能开块（Anthropic 的 content_block_start 必须带 name）。
	if !tc.open && tc.name != "" {
		tc.open = true
		if err := out.write("content_block_start", map[string]any{
			"type":  "content_block_start",
			"index": tc.index,
			"content_block": map[string]any{
				"type":  "tool_use",
				"id":    tc.id,
				"name":  tc.name,
				"input": map[string]any{},
			},
		}); err != nil {
			return err
		}
		// name 迟到时，把先于 name 到达的参数在 start 之后补发（内容不丢）。
		if pending := tc.pending.String(); pending != "" {
			if err := s.writeToolArgs(out, tc, pending); err != nil {
				return err
			}
			tc.pending.Reset()
		}
	}
	if fn != nil {
		if args := str(fn["arguments"]); args != "" {
			tc.arguments.WriteString(args)
			if !tc.open {
				// name 未到：先暂存，待 start 之后再补发（绝不发出跨 start 的 delta）。
				tc.pending.WriteString(args)
			} else if err := s.writeToolArgs(out, tc, args); err != nil {
				return err
			}
		}
	}
	return nil
}

// writeToolArgs 在已开启的工具块上发出一个 input_json_delta。
func (s *anthropicStreamState) writeToolArgs(out *sseWriter, tc *anthropicToolState, args string) error {
	return out.write("content_block_delta", map[string]any{
		"type":  "content_block_delta",
		"index": tc.index,
		"delta": map[string]any{
			"type":         "input_json_delta",
			"partial_json": args,
		},
	})
}

func (s *anthropicStreamState) nextBlockIndex() int {
	if s.textOpen {
		return 1 + len(s.toolOrder)
	}
	return len(s.toolOrder)
}

// closeOpenBlocks 按 Anthropic 规范逐个关闭已开启的内容块。
func (s *anthropicStreamState) closeOpenBlocks(out *sseWriter) {
	if s.textOpen {
		_ = out.write("content_block_stop", map[string]any{
			"type":  "content_block_stop",
			"index": s.textIndex,
		})
	}
	order := append([]int(nil), s.toolOrder...)
	sort.Ints(order)
	for _, idx := range order {
		tc := s.toolCalls[idx]
		if tc == nil || !tc.open {
			continue
		}
		_ = out.write("content_block_stop", map[string]any{
			"type":  "content_block_stop",
			"index": tc.index,
		})
	}
}

// stopReason 把 chat 的 finish_reason 映射成 Anthropic 的 stop_reason。
//
// 注意 tool_use 的判定口径：只有**真正开启过工具块**（tc.open）才算工具调用。
// 上游只发了 arguments（甚至只有 index）却没给 name 时，块永远开不起来，
// 客户端一个 tool_use 块都收不到；此时若仍回 tool_use，客户端会被要求执行一个
// 它根本没收到的工具，只能卡住等下一次输入。这种情况下退化为 end_turn，
// 让客户端按普通文本轮次收尾。
func (s *anthropicStreamState) stopReason() string {
	switch s.finishReason {
	case "length":
		return "max_tokens"
	case "tool_calls":
		if s.hasOpenToolBlock() {
			return "tool_use"
		}
		return "end_turn"
	case "stop":
		return "end_turn"
	case "":
		if s.hasOpenToolBlock() {
			return "tool_use"
		}
		return "end_turn"
	default:
		return "end_turn"
	}
}

// hasOpenToolBlock 报告是否至少有一个工具块真的发给了客户端。
func (s *anthropicStreamState) hasOpenToolBlock() bool {
	for _, tc := range s.toolCalls {
		if tc != nil && tc.open {
			return true
		}
	}
	return false
}

var _ = io.EOF
