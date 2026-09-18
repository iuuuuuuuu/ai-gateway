package qoder

// 翻译层与流翻译的单元测试。
//
// 这一层是"客户端看到的格式"的出口。翻译错了**不会报错**：
//   · 请求翻译错 → 上游 400 或挂起
//   · 响应翻译错 → 客户端收到空回答（而 HTTP 是 200）
//
// 后者最难排查，故重点覆盖。

import (
	"encoding/json"
	"io"
	"strings"
	"testing"
)

// TestBuildAgentBodyCarriesAllMessages 客户端消息必须全量转发。
//
// 参考实现实测：模板 system + 模板 tools 均非必需，纯透传后
// baseline prompt_tokens 从约 10K 降到约 60 —— 那 10K 是白烧的额度。
func TestBuildAgentBodyCarriesAllMessages(t *testing.T) {
	openai := []byte(`{
		"model": "qwen3-max",
		"messages": [
			{"role":"system","content":"你是助手"},
			{"role":"user","content":"第一个问题"},
			{"role":"assistant","content":"第一个回答"},
			{"role":"user","content":"第二个问题"}
		]
	}`)
	raw, err := BuildAgentBody(openai, "qmodel_preview")
	if err != nil {
		t.Fatal(err)
	}
	var body map[string]any
	if err := json.Unmarshal(raw, &body); err != nil {
		t.Fatal(err)
	}

	msgs, ok := body["messages"].([]any)
	if !ok {
		t.Fatal("请求体里应有 messages")
	}
	if len(msgs) != 4 {
		t.Errorf("应全量转发 4 条消息（含 system 与多轮），实际 %d 条", len(msgs))
	}

	// 协议必填字段
	for _, k := range []string{"request_id", "chat_record_id", "agent_id", "chat_task", "chat_context", "model_config", "stream"} {
		if _, ok := body[k]; !ok {
			t.Errorf("请求体缺少必填字段 %s", k)
		}
	}
	// model_config.key 必须是上游 key
	mc, _ := body["model_config"].(map[string]any)
	if mc["key"] != "qmodel_preview" {
		t.Errorf("model_config.key 应为上游 key，实际 %v", mc["key"])
	}
	// chat_context.text 必填，取最后一条 user 消息
	cc, _ := body["chat_context"].(map[string]any)
	txt, _ := cc["text"].(map[string]any)
	if txt["text"] != "第二个问题" {
		t.Errorf("chat_context.text 应取最后一条 user 消息，实际 %v", txt["text"])
	}
}

// TestBuildAgentBodyToolsOnlyWhenProvided tools 仅在客户端传入时注入。
func TestBuildAgentBodyToolsOnlyWhenProvided(t *testing.T) {
	without := []byte(`{"messages":[{"role":"user","content":"hi"}]}`)
	raw, err := BuildAgentBody(without, "m")
	if err != nil {
		t.Fatal(err)
	}
	var body map[string]any
	_ = json.Unmarshal(raw, &body)
	if _, ok := body["tools"]; ok {
		t.Error("客户端没传 tools 时不应注入 tools 字段")
	}

	with := []byte(`{"messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"f"}}]}`)
	raw2, err := BuildAgentBody(with, "m")
	if err != nil {
		t.Fatal(err)
	}
	var body2 map[string]any
	_ = json.Unmarshal(raw2, &body2)
	tools, ok := body2["tools"].([]any)
	if !ok || len(tools) != 1 {
		t.Errorf("客户端传了 tools 时应注入，实际 %v", body2["tools"])
	}
}

// TestLastUserTextHandlesMultimodal 多模态 content 也要能取到文本。
//
// 只认字符串会让带图片的请求拿到空 prompt，而 chat_context.text 为空时
// 上游可能直接拒绝 —— 表现为"发图就报错"。
func TestLastUserTextHandlesMultimodal(t *testing.T) {
	msgs := []map[string]any{
		{"role": "user", "content": "第一条"},
		{"role": "user", "content": []any{
			map[string]any{"type": "text", "text": "看看这张图"},
			map[string]any{"type": "image_url", "image_url": map[string]any{"url": "data:..."}},
		}},
	}
	got := lastUserText(msgs)
	if got != "看看这张图" {
		t.Errorf("多模态应拼出文本段，实际 %q", got)
	}

	// 纯图片（无 text 段）→ 空串，不应 panic
	onlyImg := []map[string]any{
		{"role": "user", "content": []any{
			map[string]any{"type": "image_url", "image_url": map[string]any{"url": "x"}},
		}},
	}
	if got := lastUserText(onlyImg); got != "" {
		t.Errorf("纯图片应返回空串，实际 %q", got)
	}
}

// TestBuildAgentBodyRejectsEmptyMessages 空 messages 必须报错。
func TestBuildAgentBodyRejectsEmptyMessages(t *testing.T) {
	if _, err := BuildAgentBody([]byte(`{"messages":[]}`), "m"); err == nil {
		t.Error("空 messages 应报错")
	}
	if _, err := BuildAgentBody([]byte(`{坏`), "m"); err == nil {
		t.Error("非法 JSON 应报错")
	}
}

// TestTruncateRunesIsCharSafe 按字符截断（不能按字节）。
func TestTruncateRunesIsCharSafe(t *testing.T) {
	// 30 个汉字按字节截断会劈成半个字
	s := strings.Repeat("中", 50)
	got := truncateRunes(s, 30)
	if len([]rune(got)) != 30 {
		t.Errorf("应截到 30 个字符，实际 %d", len([]rune(got)))
	}
	if !strings.HasPrefix(s, got) {
		t.Error("截断结果应是原文前缀")
	}
	// 不超长时原样返回
	if truncateRunes("abc", 10) != "abc" {
		t.Error("不超长时应原样返回")
	}
}

// TestRelayStreamProducesOpenAIShape 流翻译必须产出标准 OpenAI 帧。
//
// 这是最关键的一条：客户端按 OpenAI 解析，形状不对会得到空内容
// （而 HTTP 是 200，看不出错）。
func TestRelayStreamProducesOpenAIShape(t *testing.T) {
	// Qoder 的嵌套形状：body 是字符串
	upstream := strings.Join([]string{
		": heartbeat",
		"",
		`data: {"body":"{\"id\":\"cmb-1\",\"model\":\"qwen3-max\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"你好\"}}]}"}`,
		"",
		`data: {"body":"{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"世界\"}}]}"}`,
		"",
		`data: {"body":"{\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}"}`,
		"",
		"data: [DONE]",
		"",
	}, "\n")

	out := readAllTranslated(t, upstream, "qwen3-max")
	assertOpenAIShape(t, out, "你好世界")
}

// readAllTranslated 用转换器读完整条流。
//
// 转换器是惰性的 io.ReadCloser（三个消费者共用），故测试也按 reader 读，
// 而不是写进 http.ResponseWriter —— 后者已被移除（见 stream.go 的注释）。
func readAllTranslated(t *testing.T, upstream, model string) string {
	t.Helper()
	rc := NewOpenAIStream(io.NopCloser(strings.NewReader(upstream)), model)
	defer rc.Close()
	raw, err := io.ReadAll(rc)
	if err != nil {
		t.Fatalf("读取翻译后的流失败: %v", err)
	}
	return string(raw)
}

// assertOpenAIShape 逐帧校验输出是标准 OpenAI SSE 且正文正确。
func assertOpenAIShape(t *testing.T, out, wantContent string) {
	t.Helper()
	if !strings.Contains(out, "data: [DONE]") {
		t.Error("应以 data: [DONE] 结束")
	}
	if strings.Contains(out, `"body"`) {
		t.Error("输出里不应残留嵌套的 body 字段 —— 说明没有拍平")
	}

	var content strings.Builder
	seenRole := false
	seenFinish := false
	for _, line := range strings.Split(out, "\n") {
		line = strings.TrimSpace(line)
		if !strings.HasPrefix(line, "data: ") || strings.Contains(line, "[DONE]") {
			continue
		}
		var frame map[string]any
		if err := json.Unmarshal([]byte(strings.TrimPrefix(line, "data: ")), &frame); err != nil {
			t.Fatalf("帧不是合法 JSON: %v（原文 %s）", err, line)
		}
		if frame["object"] != "chat.completion.chunk" {
			t.Errorf("object 应为 chat.completion.chunk，实际 %v", frame["object"])
		}
		choices, _ := frame["choices"].([]any)
		if len(choices) == 0 {
			continue
		}
		c := choices[0].(map[string]any)
		if fr, ok := c["finish_reason"].(string); ok && fr != "" {
			seenFinish = true
		}
		if d, ok := c["delta"].(map[string]any); ok {
			if r, ok := d["role"].(string); ok && r != "" {
				seenRole = true
			}
			if s, ok := d["content"].(string); ok {
				content.WriteString(s)
			}
		}
	}

	if content.String() != wantContent {
		t.Errorf("正文应为 %q，实际 %q（形状翻译可能有误）", wantContent, content.String())
	}
	if !seenRole {
		t.Error("应有一个带 role=assistant 的首帧（部分客户端依赖它）")
	}
	if !seenFinish {
		t.Error("应有 finish_reason 帧（否则客户端可能一直等）")
	}
}

// TestRelayStreamSynthesizesFinish 上游没给 finish_reason 时也要补一个。
//
// 不补的话部分客户端会一直等（它们靠 finish_reason 判断结束）。
func TestRelayStreamSynthesizesFinish(t *testing.T) {
	upstream := strings.Join([]string{
		`data: {"choices":[{"delta":{"content":"只有内容"}}]}`,
		"data: [DONE]",
	}, "\n")
	out := readAllTranslated(t, upstream, "m")
	if !strings.Contains(out, `"finish_reason":"stop"`) {
		t.Error("应补一个 finish_reason=stop 的收尾帧")
	}
}

// TestRelayStreamReportsInStreamError 流内错误必须以错误帧告知客户端。
//
// 静默结束会让用户以为"回答就是空的"，而真正原因在流里。
func TestRelayStreamReportsInStreamError(t *testing.T) {
	upstream := `data: {"error":{"code":"500","msg":"上游内部错误"}}` + "\n"
	out := readAllTranslated(t, upstream, "m")
	if !strings.Contains(out, "上游内部错误") {
		t.Errorf("应把上游文案告知客户端，实际输出 %q", out)
	}
	if !strings.Contains(out, `"error"`) {
		t.Error("应以 OpenAI 错误帧形状输出（含 error 字段）")
	}
}

// TestRelayStreamEmptyUpstream 上游无内容时只发 [DONE]（不造假的空帧）。
func TestRelayStreamEmptyUpstream(t *testing.T) {
	out := readAllTranslated(t, ": heartbeat\n\n", "m")
	if !strings.Contains(out, "[DONE]") {
		t.Error("即使无内容也应以 [DONE] 结束（客户端需要明确的终止信号）")
	}
	if strings.Contains(out, "chat.completion.chunk") {
		t.Error("无内容时不应造出内容帧")
	}
}

// TestAggregateQoderNonStream 非流式聚合必须产出合法 OpenAI JSON。
func TestAggregateQoderNonStream(t *testing.T) {
	upstream := strings.Join([]string{
		`data: {"body":"{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"第一段\"}}]}"}`,
		`data: {"body":"{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"第二段\"}}]}"}`,
		`data: {"body":"{\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"思考\"},\"finish_reason\":\"stop\"}]}"}`,
		"data: [DONE]",
	}, "\n")

	got, err := AggregateQoder(strings.NewReader(upstream), "qwen3-max")
	if err != nil {
		t.Fatal(err)
	}
	if got["object"] != "chat.completion" {
		t.Errorf("object 应为 chat.completion，实际 %v", got["object"])
	}
	choices, _ := got["choices"].([]any)
	if len(choices) != 1 {
		t.Fatalf("应有 1 个 choice，实际 %d", len(choices))
	}
	c := choices[0].(map[string]any)
	msg, _ := c["message"].(map[string]any)
	if msg["content"] != "第一段第二段" {
		t.Errorf("正文拼接有误，实际 %q", msg["content"])
	}
	if msg["reasoning_content"] != "思考" {
		t.Errorf("推理内容应为「思考」，实际 %v", msg["reasoning_content"])
	}
	if c["finish_reason"] != "stop" {
		t.Errorf("finish_reason 应为 stop，实际 %v", c["finish_reason"])
	}
}

// TestAggregateQoderOmitsEmptyReasoning 空推理内容不应出现。
//
// 给一个空字符串会让部分客户端显示一个空的"思考"块。
func TestAggregateQoderOmitsEmptyReasoning(t *testing.T) {
	upstream := `data: {"choices":[{"delta":{"content":"只有正文"}}]}` + "\n"
	got, err := AggregateQoder(strings.NewReader(upstream), "m")
	if err != nil {
		t.Fatal(err)
	}
	msg := got["choices"].([]any)[0].(map[string]any)["message"].(map[string]any)
	if _, ok := msg["reasoning_content"]; ok {
		t.Error("无推理内容时不应带 reasoning_content 字段")
	}
}

// TestPeekStream 流式判定。
func TestPeekStream(t *testing.T) {
	cases := map[string]bool{
		`{"stream":true}`:                  true,
		`{"stream":false}`:                 false,
		`{"stream":null}`:                  false,
		`{}`:                               false, // 未声明 → 非流式
		`{坏`:                              false, // 非法 JSON 不 panic
		`{"stream":true,"model":"x"}`:      true,
	}
	for in, want := range cases {
		if got := PeekStream([]byte(in)); got != want {
			t.Errorf("PeekStream(%s) = %v，期望 %v", in, got, want)
		}
	}
}
