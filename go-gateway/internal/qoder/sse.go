package qoder

// sse.go 解析 Qoder 的**嵌套 SSE** 响应。
//
// 与标准 OpenAI 流的区别（这是本文件存在的唯一理由）：
//
//	OpenAI   : data: {"choices":[{"delta":{"content":"文本"}}]}
//	Qoder    : data: {"body":"{\"choices\":[...]}"}   ← body 是**字符串**，里面又是一层 JSON
//
// 也就是说 Qoder 把标准 OpenAI 的 chunk 序列化成了字符串、再包一层。
// 直接按 OpenAI 解析会得到空内容（因为顶层没有 choices 字段）。
//
// 实测确认（2026-09-18，直接调上游 /v2/chat/completions）：
//
//	: heartbeat
//	data: {"id":"cmb-...","model":"deepseek-v4.1-flash","object":"chat.completion.chunk",
//	       "choices":[{"index":0,"delta":{"role":"assistant","content":"",...}}]}
//
// 注意上游还会发 `: heartbeat` 心跳行（以冒号开头，按 SSE 规范是注释）。

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"strings"
)

// Chunk 一个解析后的流式分片（已拍平成 OpenAI 形状）。
type Chunk struct {
	// Content 正文增量。
	Content string
	// ReasoningContent 推理过程增量（推理模型才有）。
	ReasoningContent string
	// FinishReason 结束原因（通常最后一个分片才有）。
	FinishReason string
	// Model 模型名（上游回显）。
	Model string
	// Raw 原始 data 行（供排障：解析不出内容时能看到上游到底发了什么）。
	Raw string
}

// SSEReader 逐行读取并解析 Qoder 的 SSE 流。
type SSEReader struct {
	sc     *bufio.Scanner
	closed bool
}

// NewSSEReader 构造流读取器。
//
// 缓冲区上限设成 4MB：上游单个分片可能很大（例如一次性返回整段长文本时），
// bufio.Scanner 默认 64KB 会直接报 "token too long" 并中断整个流 ——
// 那个错误信息完全不提"缓冲区"，很容易被误判成上游返回了坏数据。
func NewSSEReader(r io.Reader) *SSEReader {
	sc := bufio.NewScanner(r)
	sc.Buffer(make([]byte, 0, 64*1024), 4*1024*1024)
	return &SSEReader{sc: sc}
}

// Next 读取下一个分片。
//
// 返回：
//
//	(*Chunk, nil)  读到一个有效分片
//	(nil, io.EOF)  流正常结束
//	(nil, err)     读取或解析失败
//
// 心跳行（`: heartbeat`）与空行会被跳过 —— 它们不是数据。
func (s *SSEReader) Next() (*Chunk, error) {
	for s.sc.Scan() {
		line := strings.TrimSpace(s.sc.Text())
		if line == "" {
			continue
		}
		// SSE 注释行（以冒号开头）：上游用它发心跳。
		if strings.HasPrefix(line, ":") {
			continue
		}
		if !strings.HasPrefix(line, "data:") {
			// 其它字段（event: / id: / retry:）本接口用不到，跳过即可。
			continue
		}
		payload := strings.TrimSpace(strings.TrimPrefix(line, "data:"))
		if payload == "" {
			continue
		}
		// 标准结束标记
		if payload == "[DONE]" {
			return nil, io.EOF
		}
		ch, err := parseChunk(payload)
		if err != nil {
			// 两类错误要区别对待（此前一律吞掉，导致流内错误被静默忽略）：
			//
			//	① 上游明确报错（inStreamError）→ **必须向上抛**。
			//	   吞掉它的后果是用户看到"回答是空的"，而真正原因在流里，
			//	   排查时会去怀疑模型、网络、提示词，全都不对。
			//	② 只是不认识的形状（未知字段）→ 跳过继续读。
			//	   上游加字段是常事，为此中断整个流会让回答被截断。
			var ise *inStreamError
			if errors.As(err, &ise) {
				return nil, err
			}
			return &Chunk{Raw: payload}, nil
		}
		return ch, nil
	}
	if err := s.sc.Err(); err != nil {
		return nil, fmt.Errorf("读取流失败: %w", err)
	}
	return nil, io.EOF
}

// parseChunk 解析单个 data 行，兼容"嵌套 body"与"直接 OpenAI 形状"两种。
//
// 为什么要兼容两种：上游可能在某个版本改成直接下发（不再套 body），
// 而两种形状无法从状态码上区分 —— 都兼容就不会因为上游改版而全部失效。
func parseChunk(payload string) (*Chunk, error) {
	var top map[string]json.RawMessage
	if err := json.Unmarshal([]byte(payload), &top); err != nil {
		return nil, err
	}

	// 流内错误可能出现在**外层**（顶层 error 字段）或**内层**（body 里的 error）。
	// 实测踩到：只查内层会漏掉外层，于是错误被当成空分片静默跳过 ——
	// 用户看到"回答是空的"，而真正原因是上游在流里报了错。
	if err := errorFromEnvelope(top); err != nil {
		return nil, err
	}

	// 形状一：嵌套 —— {"body":"{\"choices\":[...]}"}
	if rawBody, ok := top["body"]; ok {
		var inner string
		if err := json.Unmarshal(rawBody, &inner); err == nil && strings.TrimSpace(inner) != "" {
			return parseOpenAIShaped([]byte(inner), payload)
		}
	}

	// 形状二：直接就是 OpenAI 形状（含 choices 字段）
	if _, ok := top["choices"]; ok {
		return parseOpenAIShaped([]byte(payload), payload)
	}

	// 其它：可能是 usage 汇总等 —— 不算解析失败，返回空分片。
	return &Chunk{Raw: payload}, nil
}

// inStreamError 上游在**流内部**下发的错误（HTTP 状态码仍是 200）。
//
// 单独成型是为了与"解析不了这个形状"区分开：
//
//	inStreamError → 上游明确说"这次请求失败了"，必须向上抛
//	其它解析错误   → 只是我们不认识这个字段，跳过继续读
//
// 不区分的话，流内错误会被静默吞掉，用户看到空回答而原因不明。
type inStreamError struct {
	Code string
	Msg  string
}

func (e *inStreamError) Error() string {
	if e.Code == "" {
		return "上游在流内报错：" + e.Msg
	}
	return fmt.Sprintf("上游在流内报错 code=%s msg=%s", e.Code, e.Msg)
}

// errorFromEnvelope 从响应信封里提取错误（顶层 error 字段）。
//
// 上游两种报错位置都要查：
//
//	{"error":{"code":"500","msg":"..."}}                        顶层（本函数）
//	{"body":"{\"error\":{\"code\":\"500\",\"msg\":\"...\"}}" }  嵌在 body 里（parseOpenAIShaped）
func errorFromEnvelope(top map[string]json.RawMessage) error {
	raw, ok := top["error"]
	if !ok || len(raw) == 0 || string(raw) == "null" {
		return nil
	}
	var e struct {
		Code string `json:"code"`
		Msg  string `json:"msg"`
		// 有些实现用 message 而不是 msg
		Message string `json:"message"`
	}
	if err := json.Unmarshal(raw, &e); err != nil {
		// error 字段存在但不是对象：当作有错，原文带出
		return &inStreamError{Msg: string(raw)}
	}
	return &inStreamError{Code: e.Code, Msg: firstNonEmpty(e.Msg, e.Message)}
}

// parseOpenAIShaped 解析标准 OpenAI chunk 形状。
func parseOpenAIShaped(data []byte, raw string) (*Chunk, error) {
	var v struct {
		Model   string `json:"model"`
		Choices []struct {
			Delta struct {
				Content          string `json:"content"`
				ReasoningContent string `json:"reasoning_content"`
			} `json:"delta"`
			Message struct {
				Content          string `json:"content"`
				ReasoningContent string `json:"reasoning_content"`
			} `json:"message"`
			FinishReason string `json:"finish_reason"`
		} `json:"choices"`
		Error *struct {
			Code string `json:"code"`
			Msg  string `json:"msg"`
		} `json:"error"`
	}
	if err := json.Unmarshal(data, &v); err != nil {
		return nil, err
	}
	// 流内错误：上游有时在 200 的流里下发 error 对象。
	if v.Error != nil {
		return nil, &inStreamError{Code: v.Error.Code, Msg: v.Error.Msg}
	}

	ch := &Chunk{Model: v.Model, Raw: raw}
	if len(v.Choices) > 0 {
		c := v.Choices[0]
		// delta 用于流式增量；message 用于非流式（有些实现两者都给）。
		ch.Content = firstNonEmpty(c.Delta.Content, c.Message.Content)
		ch.ReasoningContent = firstNonEmpty(c.Delta.ReasoningContent, c.Message.ReasoningContent)
		ch.FinishReason = c.FinishReason
	}
	return ch, nil
}

// ReadAll 读完整个流并拼接正文（非流式场景用）。
//
// 返回正文与推理内容（分开返回：推理内容不该混进给用户的回答里）。
func (s *SSEReader) ReadAll() (content, reasoning string, err error) {
	var sb, rb strings.Builder
	for {
		ch, err := s.Next()
		if err == io.EOF {
			return sb.String(), rb.String(), nil
		}
		if err != nil {
			// 已经读到的内容仍然返回 —— 部分内容比什么都没有有用。
			return sb.String(), rb.String(), err
		}
		sb.WriteString(ch.Content)
		rb.WriteString(ch.ReasoningContent)
	}
}
