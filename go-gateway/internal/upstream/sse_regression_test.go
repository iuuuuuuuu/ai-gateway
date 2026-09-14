package upstream

import (
	"encoding/json"
	"net/http/httptest"
	"strings"
	"testing"
)

// ---------------------------------------------------------------------------
// 回归测试：上游适配层的三处缺陷
//
// 这三个问题的共同点是「上游返回 HTTP 200，但内容被网关悄悄改写/丢弃」，
// 客户端只会看到二次损坏的结果，因此必须锁死。
// ---------------------------------------------------------------------------

// flushRecorder 实现 http.Flusher 的 recorder，便于 Stream 正常 flush。
type flushRecorder struct {
	*httptest.ResponseRecorder
}

func (f *flushRecorder) Flush() { f.ResponseRecorder.Flush() }

// Bug 1：上游把 tool_call 的 index 发成 JSON 字符串时，多个不同的工具调用
// 会被合并到槽位 0。
//
// 原实现只认 float64：`if v, ok := call["index"].(float64); ok { idx = int(v) }`，
// 字符串 "0"/"1" 一律落到 0，导致两条并行工具调用挤进同一槽位 —— 名字被后者覆盖、
// 参数被拼接，模型拿到的工具调用直接损坏（会去调用错误的工具、参数是坏的 JSON）。
func TestToolCallStringIndexDoesNotCollapse(t *testing.T) {
	in := "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[" +
		"{\"index\":\"0\",\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\\\"BJ\\\"}\"}}]}}]}\n\n" +
		"data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[" +
		"{\"index\":\"1\",\"id\":\"call_b\",\"type\":\"function\",\"function\":{\"name\":\"get_time\",\"arguments\":\"{\\\"tz\\\":\\\"UTC\\\"}\"}}]}}]}\n\n" +
		"data: [DONE]\n\n"

	out, err := Aggregate(strings.NewReader(in))
	if err != nil {
		t.Fatalf("aggregate: %v", err)
	}
	calls := aggregatedToolCalls(t, out)
	if len(calls) != 2 {
		raw, _ := json.Marshal(calls)
		t.Fatalf("两个不同的工具调用被合并成 %d 个（字符串 index 未解析）: %s", len(calls), raw)
	}
	// 名字与参数必须各自独立，不能交叉污染。
	got := map[string]string{}
	for _, c := range calls {
		fn, _ := c["function"].(map[string]any)
		got[strOf(fn["name"])] = strOf(fn["arguments"])
	}
	if got["get_weather"] != `{"city":"BJ"}` {
		t.Errorf("get_weather 参数=%q want %q", got["get_weather"], `{"city":"BJ"}`)
	}
	if got["get_time"] != `{"tz":"UTC"}` {
		t.Errorf("get_time 参数=%q want %q", got["get_time"], `{"tz":"UTC"}`)
	}
}

// 对照：数字 index（标准形态）行为不变，证明修复没有改变既有语义。
func TestToolCallNumericIndexStillMerges(t *testing.T) {
	in := "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[" +
		"{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"ci\"}}]}}]}\n\n" +
		"data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[" +
		"{\"index\":0,\"function\":{\"arguments\":\"ty\\\":\\\"BJ\\\"}\"}}]}}]}\n\n" +
		"data: [DONE]\n\n"

	out, err := Aggregate(strings.NewReader(in))
	if err != nil {
		t.Fatalf("aggregate: %v", err)
	}
	calls := aggregatedToolCalls(t, out)
	if len(calls) != 1 {
		t.Fatalf("同一 index 的分片应合并为 1 个调用，得到 %d", len(calls))
	}
	fn, _ := calls[0]["function"].(map[string]any)
	if got := strOf(fn["arguments"]); got != `{"city":"BJ"}` {
		t.Errorf("分片参数拼接=%q want %q", got, `{"city":"BJ"}`)
	}
}

// Bug 2：空的 delta.content（仅含 role 的开场/保活帧）会永久关闭
// 「完整消息放在 message 里」的回退路径，导致真实回答被整段丢弃。
func TestEmptyDeltaContentDoesNotLatchFallback(t *testing.T) {
	in := "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n" +
		"data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"the real answer\"}}]}\n\n" +
		"data: [DONE]\n\n"

	out, err := Aggregate(strings.NewReader(in))
	if err != nil {
		t.Fatalf("aggregate: %v", err)
	}
	msg := aggregatedMessage(t, out)
	if msg["content"] != "the real answer" {
		t.Errorf("空 delta.content 锁死了 message 回退路径，真实内容丢失: content=%q", msg["content"])
	}
}

// 对照：已有非空正文时，仍应以 delta 为准（回退路径不得抢占正常流）。
func TestNonEmptyDeltaStillWinsOverMessageFallback(t *testing.T) {
	in := "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"from delta\"}}]}\n\n" +
		"data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"from message\"}}]}\n\n" +
		"data: [DONE]\n\n"

	out, err := Aggregate(strings.NewReader(in))
	if err != nil {
		t.Fatalf("aggregate: %v", err)
	}
	msg := aggregatedMessage(t, out)
	if msg["content"] != "from delta" {
		t.Errorf("已有 delta 正文时不应拼接 message 内容: content=%q", msg["content"])
	}
}

// Bug 3：流中途的上游错误帧被白名单重建吞掉，客户端把「截断回答 + 正常结束」
// 当成一次成功。
//
// 上游常在 HTTP 200 的流中途发 {"error":{...}}（如 code=11128 渠道未批准、
// 账号被封）表示失败。normalizeFrame 的白名单没有 error 键，于是该帧被降级成
// 普通 chunk，错误信息彻底消失；handler 在直通路径上也不会重试，无法挽回。
func TestMidStreamErrorFrameSurvives(t *testing.T) {
	in := "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial answer\"}}]}\n\n" +
		"data: {\"error\":{\"message\":\"upstream account banned\",\"type\":\"server_error\",\"code\":\"11128\"}}\n\n" +
		"data: [DONE]\n\n"

	rec := &flushRecorder{ResponseRecorder: httptest.NewRecorder()}
	if err := Stream(rec, strings.NewReader(in)); err != nil {
		t.Fatalf("stream err: %v", err)
	}
	body := rec.Body.String()
	if !strings.Contains(body, "upstream account banned") {
		t.Errorf("流中途的错误帧被静默删除，客户端会当成正常结束:\n%s", body)
	}
	if !strings.Contains(body, "11128") {
		t.Errorf("错误码未透出:\n%s", body)
	}
}

// 对照：正常帧的白名单重建行为不变（空 content 仍被剔除、usage 缺失仍补 null）。
func TestNormalizeFrameWhitelistUnchanged(t *testing.T) {
	rec := &flushRecorder{ResponseRecorder: httptest.NewRecorder()}
	in := "data: {\"id\":\"c1\",\"noise\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\"}}]}\n\n" +
		"data: [DONE]\n\n"
	if err := Stream(rec, strings.NewReader(in)); err != nil {
		t.Fatalf("stream err: %v", err)
	}
	body := rec.Body.String()
	if strings.Contains(body, "noise") {
		t.Errorf("顶层未知字段应被剔除:\n%s", body)
	}
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

func aggregatedMessage(t *testing.T, out map[string]any) map[string]any {
	t.Helper()
	choices, _ := out["choices"].([]any)
	if len(choices) == 0 {
		t.Fatal("响应没有 choices")
	}
	choice, _ := choices[0].(map[string]any)
	msg, _ := choice["message"].(map[string]any)
	if msg == nil {
		t.Fatal("响应没有 message")
	}
	return msg
}

func aggregatedToolCalls(t *testing.T, out map[string]any) []map[string]any {
	t.Helper()
	msg := aggregatedMessage(t, out)
	raw, _ := msg["tool_calls"].([]map[string]any)
	return raw
}

func strOf(v any) string {
	s, _ := v.(string)
	return s
}
