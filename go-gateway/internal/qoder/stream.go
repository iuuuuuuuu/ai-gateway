package qoder

// stream.go 把 Qoder 的嵌套 SSE 翻译成标准 OpenAI SSE 并写出。
//
// 这是"客户端看到的格式"的最终出口。与 upstream.Stream 的关系：
//
//	upstream.Stream   把 **WorkBuddy 的 OpenAI 形状** 帧规范化后转发
//	本文件            把 **Qoder 的嵌套形状** 先拍平，再产出 OpenAI 帧
//
// 两者产出的对外格式**必须一致** —— 客户端不该感知到"这次命中的是哪个产品"。
// 故这里复用与 upstream 相同的帧形状（object/id/choices/delta 白名单）。

import (
	"encoding/json"
	"io"
	"strings"
	"time"
)

// defaultChunkID 合成的 chunk id（上游不给 id 时使用）。
//
// 与 upstream 侧的 "chatcmpl-wb2api" 不同：保留产品标识便于排障时
// 从客户端抓包就能看出"这次是 Qoder 供的"。
const defaultChunkID = "chatcmpl-qoder"

// openAIStream 把 Qoder 的嵌套 SSE **惰性**转换成标准 OpenAI SSE 字节流。
//
// ## 为什么做成 io.ReadCloser 而不是直接写 http.ResponseWriter
//
// 网关内部有**三个**消费者读这个流：
//
//	/v1/chat/completions  → upstream.Stream（原样透传 OpenAI 帧）
//	/v1/messages          → messages_stream.go（OpenAI 帧 → Anthropic 事件）
//	/v1/responses         → responses_stream.go（OpenAI 帧 → Responses 事件）
//
// 若在每个消费者里都写一遍"如果是 Qoder 就换种读法"，会有三处分支、
// 三处可能漏改。做成转换器后，**消费者完全不需要知道产品差异** ——
// 它们看到的永远是标准 OpenAI 帧，与 WorkBuddy 路径逐字一致。
//
// ## 为什么是惰性的
//
// 上游是流式响应，边读边转才能保持流式体验（首字延迟）。
// 一次性读完再转会让用户等整段回答生成完 —— 那就不是流式了。
type openAIStream struct {
	src    io.ReadCloser
	reader *SSEReader
	model  string

	// pending 尚未被调用方取走的输出字节。
	pending []byte
	// done 上游已结束（[DONE] 已产出）。
	done bool
	// created 首次产帧时固定，保证整条流的时间戳一致。
	created int64
	// roleSent 是否已产出过带 role 的首帧。
	roleSent bool
}

// NewOpenAIStream 包装 Qoder 的嵌套流，产出标准 OpenAI SSE。
//
// 调用方必须 Close（会一并关闭底层流）。
func NewOpenAIStream(src io.ReadCloser, model string) io.ReadCloser {
	return &openAIStream{src: src, reader: NewSSEReader(src), model: model}
}

func (s *openAIStream) Read(p []byte) (int, error) {
	for len(s.pending) == 0 {
		if s.done {
			return 0, io.EOF
		}
		if err := s.fill(); err != nil {
			return 0, err
		}
	}
	n := copy(p, s.pending)
	s.pending = s.pending[n:]
	return n, nil
}

func (s *openAIStream) Close() error { return s.src.Close() }

// fill 从上游读一个分片，翻译成 OpenAI 帧追加到 pending。
func (s *openAIStream) fill() error {
	ch, err := s.reader.Next()
	if err == io.EOF {
		// 上游结束：补收尾帧 + [DONE]（客户端靠它们判断流已完）
		s.done = true
		if s.created == 0 {
			// 上游一个内容帧都没有：只发 [DONE]，不造假的空帧
			s.pending = []byte("data: [DONE]\n\n")
			return nil
		}
		s.pending = []byte(s.finishFrame() + "data: [DONE]\n\n")
		return nil
	}
	if err != nil {
		// 流内错误：以 OpenAI 错误帧告知，而不是静默结束 ——
		// 静默会让用户以为"回答就是空的"，真正原因在流里。
		s.done = true
		payload, _ := json.Marshal(map[string]any{
			"error": map[string]any{
				"message": err.Error(),
				"type":    "upstream_error",
				"code":    "qoder_stream_error",
			},
		})
		s.pending = []byte("data: " + string(payload) + "\n\ndata: [DONE]\n\n")
		return nil
	}

	if ch.Content == "" && ch.ReasoningContent == "" && ch.FinishReason == "" {
		return nil // 心跳/未知形状：继续读
	}
	if s.created == 0 {
		s.created = time.Now().Unix()
	}

	delta := map[string]any{}
	if !s.roleSent {
		// OpenAI 客户端惯例：先来一个 role 帧，缺它有些客户端不认后续内容。
		delta["role"] = "assistant"
		s.roleSent = true
	}
	if ch.ReasoningContent != "" {
		delta["reasoning_content"] = ch.ReasoningContent
	}
	if ch.Content != "" {
		delta["content"] = ch.Content
	}

	choice := map[string]any{"index": 0, "delta": delta}
	if ch.FinishReason != "" {
		choice["finish_reason"] = ch.FinishReason
	}
	model := ch.Model
	if model == "" {
		model = s.model
	}
	s.pending = []byte(frame("chat.completion.chunk", s.created, model, choice))
	return nil
}

// finishFrame 收尾帧：上游没给 finish_reason 时补一个 stop。
//
// 不补的话部分客户端会一直等（它们靠 finish_reason 判断结束）。
func (s *openAIStream) finishFrame() string {
	return frame("chat.completion.chunk", s.created, s.model, map[string]any{
		"index":         0,
		"delta":         map[string]any{},
		"finish_reason": "stop",
	})
}

// frame 产出一帧 OpenAI SSE（含结尾空行）。
func frame(object string, created int64, model string, choice map[string]any) string {
	raw, _ := json.Marshal(map[string]any{
		"id":      defaultChunkID,
		"object":  object,
		"created": created,
		"model":   model,
		"choices": []any{choice},
	})
	return "data: " + string(raw) + "\n\n"
}

// AggregateQoder 读完整个 Qoder 流，聚合成一个 OpenAI **非流式**响应。
//
// 非流式请求（stream=false）走这里：网关对内是 OpenAI 协议，
// 客户端要一个完整 JSON，而不是流。
func AggregateQoder(r io.Reader, model string) (map[string]any, error) {
	reader := NewSSEReader(r)
	var content, reasoning strings.Builder
	finish := ""

	for {
		ch, err := reader.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			return nil, err
		}
		content.WriteString(ch.Content)
		reasoning.WriteString(ch.ReasoningContent)
		if ch.FinishReason != "" {
			finish = ch.FinishReason
		}
	}
	if finish == "" {
		finish = "stop"
	}

	msg := map[string]any{"role": "assistant", "content": content.String()}
	// reasoning_content 只在非空时给：空字符串会让部分客户端显示一个空的思考块。
	if reasoning.Len() > 0 {
		msg["reasoning_content"] = reasoning.String()
	}
	return map[string]any{
		"id":      defaultChunkID,
		"object":  "chat.completion",
		"created": time.Now().Unix(),
		"model":   model,
		"choices": []any{map[string]any{
			"index":         0,
			"message":       msg,
			"finish_reason": finish,
		}},
	}, nil
}

// PeekStream 判断客户端是否要求流式（从 OpenAI 请求体里读 stream 字段）。
//
// 单独抽出来是因为派发层需要**在选号前**就知道用哪条路径 ——
// 流式与非流式的上游处理方式不同（一个边读边写，一个读完聚合）。
func PeekStream(body []byte) bool {
	var probe struct {
		Stream *bool `json:"stream"`
	}
	if err := json.Unmarshal(body, &probe); err != nil {
		return false
	}
	return probe.Stream != nil && *probe.Stream
}
