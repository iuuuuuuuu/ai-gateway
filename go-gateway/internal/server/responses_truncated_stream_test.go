package server

import (
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/upstream"
)

// truncatedUpstream 模拟「上游流中途断掉」：先正常吐几个 chunk，再返回一个
// 非 EOF 的读错误（真实场景里是 idle 超时 cancel、连接被重置、上游 5xx 截断）。
func truncatedUpstream(t *testing.T, prefix string) *upstream.Client {
	t.Helper()
	return &upstream.Client{
		HTTP: &http.Client{Transport: roundTripFunc(func(r *http.Request) (*http.Response, error) {
			return &http.Response{
				StatusCode: 200,
				Header:     http.Header{"Content-Type": []string{"text/event-stream"}},
				Body:       &failingBody{data: []byte(prefix)},
			}, nil
		})},
		ChatBaseCN:    "https://fake.example",
		BillingBaseCN: "https://fake.example",
	}
}

// failingBody 先吐完 data，再在后续 Read 里返回非 EOF 错误。
type failingBody struct {
	data []byte
	done bool
}

func (b *failingBody) Read(p []byte) (int, error) {
	if len(b.data) > 0 {
		n := copy(p, b.data)
		b.data = b.data[n:]
		return n, nil
	}
	return 0, errors.New("simulated upstream disconnect")
}

func (b *failingBody) Close() error { return nil }

// 上游中途断流时，网关必须以 response.failed **收尾**，且**不得**在其后再发
// response.completed —— 两个事件同时出现会自相矛盾：客户端按「最后一个是
// completed」判定成功，把截断的半截回复当成正常完成，用户看到的就是
// 「输出莫名其妙断了，而且没有任何报错」。
func TestTruncatedUpstreamStreamEndsWithFailedNotCompleted(t *testing.T) {
	prefix := chatSSEPrefix(
		`{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"这是被截断的回复"}}]}`,
	)
	up := truncatedUpstream(t, prefix)
	h := NewHandler(Config{
		Pool:     testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999}),
		Upstream: up,
	})
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/responses",
		strings.NewReader(`{"model":"m","stream":true,"input":"hi"}`)))

	events := sseEvents(t, rec.Body.String())
	var types []string
	completedAt, failedAt := -1, -1
	for i, e := range events {
		ty, _ := e["type"].(string)
		types = append(types, ty)
		switch ty {
		case "response.completed":
			if completedAt < 0 {
				completedAt = i
			}
		case "response.failed":
			if failedAt < 0 {
				failedAt = i
			}
		}
	}

	if failedAt < 0 {
		t.Fatalf("上游断流后没有 response.failed，事件序列：%v", types)
	}
	if completedAt >= 0 {
		t.Fatalf("上游断流后又发了 response.completed（掩掉了失败），事件序列：%v", types)
	}
	// failed 必须是最后一个事件，客户端才能据此判定本轮失败。
	if failedAt != len(events)-1 {
		t.Fatalf("response.failed 不是最后一个事件，事件序列：%v", types)
	}
}

// chatSSEPrefix 拼一段**不带 [DONE]** 的 SSE：用于模拟中途断流。
func chatSSEPrefix(chunks ...string) string {
	var sb strings.Builder
	for _, c := range chunks {
		sb.WriteString("data: " + c + "\n\n")
	}
	return sb.String()
}

// 正常收尾（上游发 [DONE] 后干净结束）必须仍然是 response.completed，
// 不能因为上面的修复把成功路径也判成失败。
func TestCleanUpstreamStreamStillCompletes(t *testing.T) {
	up := newFakeUpstream(t, func(string) (int, string, bool) {
		return 200, chatSSE(
			`{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"完整回复"}}]}`,
			`{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":5,"total_tokens":8}}`,
		), true
	})
	h := NewHandler(Config{
		Pool:     testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999}),
		Upstream: up,
	})
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/responses",
		strings.NewReader(`{"model":"m","stream":true,"input":"hi"}`)))

	events := sseEvents(t, rec.Body.String())
	var sawCompleted, sawFailed bool
	for _, e := range events {
		switch e["type"] {
		case "response.completed":
			sawCompleted = true
		case "response.failed":
			sawFailed = true
		}
	}
	if !sawCompleted {
		t.Fatalf("正常流没有 response.completed，事件序列：%v", eventTypes(events))
	}
	if sawFailed {
		t.Fatalf("正常流不该出现 response.failed")
	}
}

func eventTypes(events []map[string]any) []string {
	out := make([]string, 0, len(events))
	for _, e := range events {
		ty, _ := e["type"].(string)
		out = append(out, ty)
	}
	return out
}

var _ io.ReadCloser = (*failingBody)(nil)

// /v1/messages 与 /v1/responses 同源：上游中途断流时不得发 message_stop。
// message_stop 在 Anthropic 协议里等于「本轮正常结束」，Claude Code 收到它就
// 把截断的半截回复当成完整回答收下，同样是「断了但没报错」。
func TestTruncatedUpstreamMessagesStreamEndsWithErrorNotStop(t *testing.T) {
	prefix := chatSSEPrefix(
		`{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"这是被截断的回复"}}]}`,
	)
	up := truncatedUpstream(t, prefix)
	h := NewHandler(Config{
		Pool:     testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999}),
		Upstream: up,
	})
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/messages",
		strings.NewReader(`{"model":"m","stream":true,"max_tokens":100,"messages":[{"role":"user","content":"hi"}]}`)))

	events := sseEvents(t, rec.Body.String())
	sawError, sawStop := false, false
	for _, e := range events {
		switch e["type"] {
		case "error":
			sawError = true
		case "message_stop":
			sawStop = true
		}
	}
	if !sawError {
		t.Fatalf("上游断流后没有 error 事件，事件序列：%v", eventTypes(events))
	}
	if sawStop {
		t.Fatalf("上游断流后又发了 message_stop（掩掉了失败），事件序列：%v", eventTypes(events))
	}
}
