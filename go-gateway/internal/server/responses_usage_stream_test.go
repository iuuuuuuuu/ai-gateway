package server

import (
	"encoding/json"
	"net/http/httptest"
	"strings"
	"testing"

	"workbuddy2api/internal/auth"
)

// chatSSE 把若干 chat chunk 拼成 SSE 流（末尾补 [DONE]）。
func chatSSE(chunks ...string) string {
	var sb strings.Builder
	for _, c := range chunks {
		sb.WriteString("data: " + c + "\n\n")
	}
	sb.WriteString("data: [DONE]\n\n")
	return sb.String()
}

// sseEvents 解析网关返回的 SSE 体为事件列表（只取 data 行）。
func sseEvents(t *testing.T, body string) []map[string]any {
	t.Helper()
	var out []map[string]any
	for _, block := range strings.Split(body, "\n\n") {
		for _, line := range strings.Split(block, "\n") {
			p, ok := strings.CutPrefix(strings.TrimSpace(line), "data: ")
			if !ok || p == "[DONE]" {
				continue
			}
			var m map[string]any
			if err := json.Unmarshal([]byte(p), &m); err != nil {
				t.Fatalf("事件不是合法 JSON: %q: %v", p, err)
			}
			out = append(out, m)
		}
	}
	return out
}

// postStream 用给定上游 SSE 请求一次流式接口，返回解析后的事件序列。
func postStream(t *testing.T, sse, path, reqBody string) []map[string]any {
	t.Helper()
	up := newFakeUpstream(t, func(string) (int, string, bool) { return 200, sse, true })
	h := NewHandler(Config{
		Pool:     testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999}),
		Upstream: up,
	})
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", path, strings.NewReader(reqBody)))
	if rec.Code != 200 {
		t.Fatalf("code=%d body=%s", rec.Code, rec.Body)
	}
	return sseEvents(t, rec.Body.String())
}

// ---------------------------------------------------------------------------
// Bug A：/v1/responses 流式收尾事件必须携带 usage
// ---------------------------------------------------------------------------

// Codex 恒以 stream:true 调用 /v1/responses，且只从
// response.completed.response.usage 读 token 用量。修复前 snapshot() 只输出
// id/object/created_at/status/model/output —— usage 在网关内部被解析（consume 里
// s.usage = u）并用于统计，却从未进入任何客户端可见事件，导致每轮用量显示 0/未知。
// 非流式路径本来就有 usage（responses.go 的 chatToResponses），流式是唯一缺口。
func TestResponsesStreamCompletedCarriesUsage(t *testing.T) {
	sse := chatSSE(
		`{"id":"c1","model":"m","choices":[{"index":0,"delta":{"content":"hello"}}]}`,
		`{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}`,
		// 标准 stream_options.include_usage 尾帧：choices 为空、只带 usage。
		`{"id":"c1","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":22,"total_tokens":33}}`,
	)
	events := postStream(t, sse, "/v1/responses", `{"model":"m","stream":true,"input":"hi"}`)

	var resp map[string]any
	found := false
	for _, e := range events {
		if str(e["type"]) == "response.completed" {
			resp, _ = e["response"].(map[string]any)
			found = true
		}
	}
	if !found || resp == nil {
		t.Fatal("未收到带 response 对象的 response.completed 事件")
	}
	usage, ok := resp["usage"].(map[string]any)
	if !ok {
		t.Fatalf("response.completed.response 缺少 usage（Codex 会显示 0 用量）: keys=%v", mapKeys(resp))
	}
	if got := numOf(usage["input_tokens"]); got != 11 {
		t.Errorf("usage.input_tokens=%d want 11（usage=%v）", got, usage)
	}
	if got := numOf(usage["output_tokens"]); got != 22 {
		t.Errorf("usage.output_tokens=%d want 22（usage=%v）", got, usage)
	}
	if got := numOf(usage["total_tokens"]); got != 33 {
		t.Errorf("usage.total_tokens=%d want 33（usage=%v）", got, usage)
	}
}

// 上游未给 usage 时也必须输出全 0 的 usage 对象而非省略字段，
// 否则严格客户端会把缺失字段当解析错误。
func TestResponsesStreamUsageDefaultsToZero(t *testing.T) {
	sse := chatSSE(
		`{"id":"c1","model":"m","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}`,
	)
	events := postStream(t, sse, "/v1/responses", `{"model":"m","stream":true,"input":"hi"}`)

	for _, e := range events {
		if str(e["type"]) != "response.completed" {
			continue
		}
		resp, _ := e["response"].(map[string]any)
		usage, ok := resp["usage"].(map[string]any)
		if !ok {
			t.Fatalf("无上游 usage 时也应输出 usage 对象: keys=%v", mapKeys(resp))
		}
		for _, k := range []string{"input_tokens", "output_tokens", "total_tokens"} {
			if _, present := usage[k]; !present {
				t.Errorf("usage 缺字段 %s: %v", k, usage)
			}
		}
		return
	}
	t.Fatal("未收到 response.completed")
}

func mapKeys(m map[string]any) []string {
	out := make([]string, 0, len(m))
	for k := range m {
		out = append(out, k)
	}
	return out
}
