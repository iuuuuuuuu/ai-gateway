package qoder

// SSE 解析的单元测试。
//
// 重点覆盖**嵌套 body** 这个与标准 OpenAI 不同的形状 ——
// 若按 OpenAI 直接解析，得到的是空内容而不是报错，
// 表现为"请求成功但回答是空的"，很难定位。

import (
	"io"
	"strings"
	"testing"
)

// TestSSENestedBody 嵌套形状（上游实际使用的）。
func TestSSENestedBody(t *testing.T) {
	// 上游真实形状：body 是**字符串**，里面又是一层 OpenAI JSON
	stream := strings.Join([]string{
		": heartbeat",
		"",
		`data: {"body":"{\"id\":\"cmb-1\",\"model\":\"deepseek-v4.1-flash\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"你好\"}}]}"}`,
		"",
		`data: {"body":"{\"id\":\"cmb-1\",\"model\":\"deepseek-v4.1-flash\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"世界\"}}]}"}`,
		"",
		`data: {"body":"{\"id\":\"cmb-1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}"}`,
		"",
		"data: [DONE]",
		"",
	}, "\n")

	r := NewSSEReader(strings.NewReader(stream))
	content, reasoning, err := r.ReadAll()
	if err != nil {
		t.Fatalf("读取流失败: %v", err)
	}
	if content != "你好世界" {
		t.Errorf("正文应为「你好世界」，实际 %q —— "+
			"若为空说明嵌套 body 没被解开（按 OpenAI 直接解析会得到空内容）", content)
	}
	if reasoning != "" {
		t.Errorf("该流没有推理内容，实际 %q", reasoning)
	}
}

// TestSSEDirectOpenAIShape 直接形状也要兼容（上游可能改版）。
func TestSSEDirectOpenAIShape(t *testing.T) {
	stream := strings.Join([]string{
		`data: {"id":"x","model":"m","choices":[{"index":0,"delta":{"content":"直接"}}]}`,
		"",
		`data: {"choices":[{"index":0,"delta":{"content":"形状"}}]}`,
		"",
		"data: [DONE]",
	}, "\n")

	r := NewSSEReader(strings.NewReader(stream))
	content, _, err := r.ReadAll()
	if err != nil {
		t.Fatalf("读取失败: %v", err)
	}
	if content != "直接形状" {
		t.Errorf("正文应为「直接形状」，实际 %q", content)
	}
}

// TestSSEReasoningSeparated 推理内容必须与正文分开。
//
// 混在一起会让用户看到模型的思考过程 —— 上游把它们放在不同字段，
// 我们也必须分开传递。
func TestSSEReasoningSeparated(t *testing.T) {
	stream := strings.Join([]string{
		`data: {"body":"{\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"让我想想\"}}]}"}`,
		"",
		`data: {"body":"{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"答案\"}}]}"}`,
		"",
		"data: [DONE]",
	}, "\n")

	r := NewSSEReader(strings.NewReader(stream))
	content, reasoning, err := r.ReadAll()
	if err != nil {
		t.Fatal(err)
	}
	if content != "答案" {
		t.Errorf("正文应为「答案」，实际 %q", content)
	}
	if reasoning != "让我想想" {
		t.Errorf("推理内容应为「让我想想」，实际 %q", reasoning)
	}
}

// TestSSEHeartbeatSkipped 心跳行必须被跳过。
func TestSSEHeartbeatSkipped(t *testing.T) {
	stream := strings.Join([]string{
		": heartbeat",
		": heartbeat",
		"",
		`data: {"choices":[{"delta":{"content":"x"}}]}`,
		"data: [DONE]",
	}, "\n")
	r := NewSSEReader(strings.NewReader(stream))
	content, _, err := r.ReadAll()
	if err != nil {
		t.Fatal(err)
	}
	if content != "x" {
		t.Errorf("心跳不应影响正文，实际 %q", content)
	}
}

// TestSSEUnknownShapeDoesNotBreak 不认识的 data 行不得中断整个流。
//
// 上游加字段是常事；若解析失败就中断，用户会看到"回答被截断"，
// 而真正原因只是多了一个不认识的键。
func TestSSEUnknownShapeDoesNotBreak(t *testing.T) {
	stream := strings.Join([]string{
		`data: {"choices":[{"delta":{"content":"前"}}]}`,
		`data: {"something_new":{"a":1}}`, // 不认识的形状
		`data: {"choices":[{"delta":{"content":"后"}}]}`,
		"data: [DONE]",
	}, "\n")
	r := NewSSEReader(strings.NewReader(stream))
	content, _, err := r.ReadAll()
	if err != nil {
		t.Fatalf("遇到未知形状不应报错: %v", err)
	}
	if content != "前后" {
		t.Errorf("未知形状应被跳过、流继续，实际 %q", content)
	}
}

// TestSSEInStreamErrorReported 流内 error 必须报出来。
//
// 上游有时在 200 的流里下发 error 对象 —— 只看状态码会以为成功。
func TestSSEInStreamErrorReported(t *testing.T) {
	stream := `data: {"error":{"code":"500","msg":"内部错误"}}` + "\n"
	r := NewSSEReader(strings.NewReader(stream))
	_, err := r.Next()
	if err == nil {
		t.Fatal("流内错误应被报出，实际被当成正常分片")
	}
	if !strings.Contains(err.Error(), "内部错误") {
		t.Errorf("错误信息应包含上游文案，实际 %v", err)
	}
}

// TestSSEFinishReason 结束原因要能取到。
func TestSSEFinishReason(t *testing.T) {
	stream := `data: {"choices":[{"delta":{},"finish_reason":"length"}]}` + "\n"
	r := NewSSEReader(strings.NewReader(stream))
	ch, err := r.Next()
	if err != nil {
		t.Fatal(err)
	}
	if ch.FinishReason != "length" {
		t.Errorf("finish_reason 应为 length，实际 %q", ch.FinishReason)
	}
}

// TestSSEEmptyStream 空流应正常结束（不 panic、不报错）。
func TestSSEEmptyStream(t *testing.T) {
	r := NewSSEReader(strings.NewReader(""))
	content, _, err := r.ReadAll()
	if err != nil {
		t.Fatalf("空流不应报错: %v", err)
	}
	if content != "" {
		t.Errorf("空流正文应为空，实际 %q", content)
	}
}

// TestSSELargeChunk 大分片不得因缓冲区上限而中断。
//
// bufio.Scanner 默认 64KB；上游在一次性返回长文本时会超过它，
// 报错是 "token too long"，完全看不出是缓冲区问题。
func TestSSELargeChunk(t *testing.T) {
	big := strings.Repeat("字", 100000) // 约 300KB（UTF-8 三字节）
	stream := `data: {"choices":[{"delta":{"content":"` + big + `"}}]}` + "\n" + "data: [DONE]\n"
	r := NewSSEReader(strings.NewReader(stream))
	content, _, err := r.ReadAll()
	if err != nil {
		t.Fatalf("大分片读取失败（可能是缓冲区太小）: %v", err)
	}
	if content != big {
		t.Errorf("大分片内容不完整：期望 %d 字符，实际 %d", len(big), len(content))
	}
}

// TestSSEPartialContentOnError 出错时已读到的内容仍应返回。
//
// 部分内容比什么都没有有用 —— 用户至少能看到模型说到哪儿了。
func TestSSEPartialContentOnError(t *testing.T) {
	// 用一个会在中途报错的 Reader
	r := NewSSEReader(&failAfterReader{
		data: `data: {"choices":[{"delta":{"content":"已读到的"}}]}` + "\n",
	})
	content, _, err := r.ReadAll()
	if err == nil {
		t.Fatal("底层读失败时应报错")
	}
	if content != "已读到的" {
		t.Errorf("出错前的内容应被返回，实际 %q", content)
	}
}

// failAfterReader 先返回一段数据，然后报错。
type failAfterReader struct {
	data string
	done bool
}

func (f *failAfterReader) Read(p []byte) (int, error) {
	if !f.done {
		f.done = true
		n := copy(p, f.data)
		return n, nil
	}
	return 0, io.ErrUnexpectedEOF
}
