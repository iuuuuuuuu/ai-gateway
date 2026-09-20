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
	// Usage 用量汇总（**通常只在最后一个分片里**）。
	//
	// # 为什么必须留住它（此前被静默丢弃）
	//
	// 原实现遇到"既不是 body 信封、也没有 choices"的分片就返回
	// `&Chunk{Raw: payload}` —— 注释写着「可能是 usage 汇总等，不算解析失败」。
	// 问题是**它被丢掉了**，于是：
	//
	//	· Qoder 路径对客户端**从不报 token 用量**
	//	· 本地统计与成本台账拿不到 Qoder 的数
	//
	// 而严格遵循 OpenAI 规范的客户端只从 `choices` 为空的独立分片里读
	// usage —— 上游恰好把 usage 塞在**带 choices 的 finish 分片**里，
	// 两条规则正好错开。故下游要拆成两帧（见 stream.go 的 frame 处理）。
	Usage map[string]any
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

	// 形状三：**独立 usage 分片**（`choices` 为空数组或缺失，只有 usage）。
	//
	// 这是标准 OpenAI 的用法上报形状 —— 严格客户端只从这里读用量。
	// 此前这段落到下面的 `&Chunk{Raw: payload}` 里被**丢掉**，于是
	// Qoder 路径对客户端从不报 token 用量。
	if u, ok := top["usage"]; ok && len(u) > 0 && string(u) != "null" {
		var usage map[string]any
		if err := json.Unmarshal(u, &usage); err == nil && len(usage) > 0 {
			return &Chunk{Usage: usage, Raw: payload}, nil
		}
	}

	// 其它：不认识的分片。不算解析失败（跳过继续读），但要保留原文供排障。
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

// Error 把上游错误渲染成**给人看**的文案。
//
// # 为什么要"渲染"而不是直接拼原文（2026-09-20 补）
//
// 上游的排队错误是**三层嵌套 + 两层 JSON 存在字符串里**，原文长这样：
//
//	code=403 msg={"code":"10605","message":"{\"isQueued\":true,\"queueCount\":7309,…}"}
//
// 直接拼出来用户完全看不懂，得自己一层层解转义才知道"是在排队"。
// 故这里识别出**已知的业务码**并渲染成一句话。
//
// ⚠ 渲染失败时**必须退回原文** —— 宁可难看，也不能吞掉信息。
func (e *inStreamError) Error() string {
	if msg, ok := e.humanMessage(); ok {
		return msg
	}
	if e.Code == "" {
		return "上游在流内报错：" + e.Msg
	}
	return fmt.Sprintf("上游在流内报错 code=%s msg=%s", e.Code, e.Msg)
}

// humanMessage 识别已知业务码并渲染成可读文案。
//
// 返回 ok=false 表示"不认识这个码" —— 调用方退回原文。
func (e *inStreamError) humanMessage() (string, bool) {
	// 把可能的多层嵌套剥开，取出最内层的业务对象。
	inner := e.peelNested()
	if inner == nil {
		return "", false
	}
	// ---- 排队（10605）：免费档拥挤时的正常保护，不是故障 ----
	if queued, ok := inner["isQueued"].(bool); ok && queued {
		var b strings.Builder
		b.WriteString("免费模型正在排队（上游主动限流，非故障）")
		if n, ok := numField(inner, "queueCount"); ok {
			fmt.Fprintf(&b, "：前面约 %d 个请求", n)
		}
		if w, ok := numField(inner, "waitTime"); ok {
			fmt.Fprintf(&b, "，预计等待约 %d 秒", w)
		}
		if m, ok := inner["modelKey"].(string); ok && m != "" {
			fmt.Fprintf(&b, "（模型 %s）", m)
		}
		if r, ok := numField(inner, "retryAfterSeconds"); ok {
			fmt.Fprintf(&b, "。建议 %d 秒后重试", r)
		}
		// 明确告知"服务正常"——避免用户以为账号或服务出了问题
		if av, ok := inner["serviceAvailable"].(bool); ok && av {
			b.WriteString("；上游服务状态正常")
		}
		return b.String(), true
	}
	return "", false
}

// peelNested 剥开"JSON 存在字符串里"的多层嵌套，返回最内层对象。
//
// 上游的形状（实测）：
//
//	第1层 {"code":"403","message":"<JSON>"}
//	第2层 {"code":"10605","message":"<JSON>"}
//	第3层 {"isQueued":true,…}        ← 要的就是它
//
// 最多剥 4 层（够用且防死循环 —— 恶意/异常的自引用串会让无界循环挂住）。
func (e *inStreamError) peelNested() map[string]any {
	cur := e.Msg
	for i := 0; i < 4; i++ {
		var m map[string]any
		if err := json.Unmarshal([]byte(cur), &m); err != nil {
			return nil
		}
		// 已经到"业务对象"（有 isQueued 之类的字段）就返回
		if _, ok := m["isQueued"]; ok {
			return m
		}
		// 否则继续往 message 里剥
		next, ok := m["message"].(string)
		if !ok || strings.TrimSpace(next) == "" {
			return m
		}
		cur = next
	}
	return nil
}

// numField 取一个数字字段（JSON 解出来是 float64）。
func numField(m map[string]any, key string) (int64, bool) {
	switch v := m[key].(type) {
	case float64:
		return int64(v), true
	case int64:
		return v, true
	case json.Number:
		if i, err := v.Int64(); err == nil {
			return i, true
		}
	}
	return 0, false
}

// errorFromEnvelope 从响应信封里提取错误。
//
// # 上游报错的**两种**形状都要查（这是 2026-09-20 补的第二种）
//
//	① 有 error 包装：{"error":{"code":"500","msg":"..."}}
//	② **无** error 包装，错误信息直接在顶层：{"code":"403","message":"..."}
//
// ## 为什么第二种必须识别（它此前被静默跳过）
//
// 实测（官方 Qoder CN 客户端日志，免费模型排队时）：
//
//	POST gateway.qoder.com.cn/.../agent_chat_generation  → HTTP **200**
//	SSE 第 1 帧：
//	  {"code":"403","message":"{\"code\":\"10605\",\"message\":
//	    \"{\\\"isQueued\\\":true,\\\"queueCount\\\":7309,...}\"}"}
//
// 这个帧**四个分支全不匹配**：没有 `error`、没有 `body`、没有 `choices`、
// 没有 `usage` —— 于是落到 `parseChunk` 最后的"不认识的分片"，**被静默跳过**。
// 流随即结束、一个内容帧都没有 ⇒ 走 stream.go 的"上游无内容"分支
// ⇒ **用户看到的是一句空回答**，而真正原因是上游在排队。
//
// 用户会去怀疑模型、网络、提示词 —— 全都不对。这正是最难查的那类缺陷：
// **错误信息一路都在，只是我们没接住。**
//
// ## 为什么不担心误判正常帧
//
// 只在**这一帧本来就不会被当成内容**时才把它当错误（见下面的判据）。
// 正常帧的形状是 `{"body":…}` / `{"choices":…}` / `{"usage":…}` —— 都带
// 其中之一，故**一个都不受影响**（有测试锁定这一点）。
func errorFromEnvelope(top map[string]json.RawMessage) error {
	// ---- 形状①：标准 error 包装 ----
	if raw, ok := top["error"]; ok && len(raw) > 0 && string(raw) != "null" {
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

	// ---- 形状②：错误直接在顶层（无 error 包装）----
	//
	// 判据（**三重否定**，缺一不可）：这一帧既不是内容、也不是用量、也不是心跳。
	//
	//	· 没有 body    → 不是嵌套内容帧
	//	· 没有 choices → 不是直接 OpenAI 内容帧
	//	· 没有 usage   → 不是用量帧
	//
	// 三者都没有时，这一帧**原本就会被丢掉**（parseChunk 的最后一行）。
	// 故把它识别成错误，只可能比"静默丢弃"更好 —— **不存在把正常帧判成错误的风险**。
	_, hasBody := top["body"]
	_, hasChoices := top["choices"]
	_, hasUsage := top["usage"]
	if hasBody || hasChoices || hasUsage {
		return nil
	}
	rawCode, hasCode := top["code"]
	if !hasCode || len(rawCode) == 0 || string(rawCode) == "null" {
		return nil
	}
	// 取 code 的字面值（上游有的发字符串 "403"，有的发数字 403）
	code := strings.Trim(strings.TrimSpace(string(rawCode)), `"`)
	// 取 message / msg（两种都见过）
	msg := rawStringField(top, "message")
	if msg == "" {
		msg = rawStringField(top, "msg")
	}
	if msg == "" {
		// 有 code 没文案：仍当错误（原文带出便于排查）
		return &inStreamError{Code: code, Msg: strings.TrimSpace(string(rawCode))}
	}
	// 明确表示"成功"的 code 不算错误（防上游给正常帧加个 code:"0"）
	if code == "0" || code == "200" || code == "" {
		return nil
	}
	return &inStreamError{Code: code, Msg: msg}
}

// rawStringField 从顶层取一个字符串字段（容忍"值是字符串"与"值是对象"两种）。
func rawStringField(top map[string]json.RawMessage, key string) string {
	raw, ok := top[key]
	if !ok || len(raw) == 0 || string(raw) == "null" {
		return ""
	}
	var s string
	if err := json.Unmarshal(raw, &s); err == nil {
		return s
	}
	// 不是字符串（少见）：返回原文
	return strings.TrimSpace(string(raw))
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
		// Usage 用量。**上游常把它塞在带 choices 的 finish 分片里**，
		// 而严格 OpenAI 客户端只从 `choices` 为空的独立分片读它 ——
		// 两条规则正好错开。故这里先收下，下游再拆成两帧（见 stream.go）。
		Usage map[string]any `json:"usage"`
	}
	if err := json.Unmarshal(data, &v); err != nil {
		return nil, err
	}
	// 流内错误：上游有时在 200 的流里下发 error 对象。
	if v.Error != nil {
		return nil, &inStreamError{Code: v.Error.Code, Msg: v.Error.Msg}
	}

	ch := &Chunk{Model: v.Model, Raw: raw, Usage: v.Usage}
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
