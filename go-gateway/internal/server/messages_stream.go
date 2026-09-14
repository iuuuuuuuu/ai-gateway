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
		if payload, ok := strings.CutPrefix(line, "data: "); ok {
			if payload == "[DONE]" {
				break
			}
			var chunk map[string]any
			if err := jsonUnmarshal(payload, &chunk); err == nil {
				if werr := ctx.consume(out, chunk); werr != nil {
					return
				}
			}
		}
		if err != nil {
			break
		}
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

// consume 处理单个 chat SSE chunk。
func (s *anthropicStreamState) consume(out *sseWriter, chunk map[string]any) error {
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
