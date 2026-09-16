package server

// 上游流中途错误帧的回归（2026-09-16 现场）。
//
// 现场表现：上游对 HTTP 200 的响应体先吐几个正常 chunk，再补一帧
// {"error":{"code":11128,"msg":"渠道未批准"}} 表示终止性失败（账号被封 /
// 渠道未批准在 sse.go:311 的 normalizeFrame 注释里已明确记录过该形态）。
//
// 修前行为：messages_stream.go 的读取循环只认 data: 前缀与 [DONE]，error 帧
// 既没有 choices 也没有触发任何分支，被整个忽略；循环读到 EOF 后照常补
// message_delta(stop_reason=end_turn) + message_stop。客户端（Claude Code /
// Claude Desktop）于是收到一个格式合法、stop_reason=end_turn、output_tokens=0
// 的完整回合，把「截断的半截回答」当成一次成功并继续推进对话；网关侧
// /usage 也按 status=200 记成成功，上游故障完全无痕。
//
// 必须锁住的行为：
//  1. 终止性 error 帧必须以 Anthropic 的 error 事件透出，且**不得**再补
//     message_delta / message_stop（那等于宣告正常完成）。
//  2. 上游在给出 finish_reason 之后异常断开（含 IdleTimeout 取消 ctx，表现为
//     context.Canceled）同样按失败收尾，而不是补一个 end_turn。
//  3. 上游什么都没给就干净 EOF 仍按原有的正常收尾，避免把合法空答复误报成故障。
//
// 对照实现：OpenAI 透传路径在 upstream/sse.go 显式保留 error 字段；
// Responses 路径发 response.failed —— 只有 messages 路径当初漏了。

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"workbuddy2api/internal/auth"
)

// midStreamErrorSSE 上游「正常 chunk → 终止性 error 帧 → [DONE]」的真实形态。
const midStreamErrorSSE = "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"partial answer\"},\"finish_reason\":null}]}\n\n" +
	"data: {\"error\":{\"code\":11128,\"msg\":\"渠道未批准\"}}\n\n" +
	"data: [DONE]\n\n"

// messagesStreamBody 发一个最小的 Anthropic 流式请求，返回响应体。
func messagesStreamBody(t *testing.T, h *Handler) (int, string) {
	t.Helper()
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/messages",
		strings.NewReader(`{"model":"deepseek-v4.1-flash","stream":true,`+
			`"max_tokens":64,"messages":[{"role":"user","content":"hi"}]}`)))
	return rec.Code, rec.Body.String()
}

// TestAnthropicStreamSurfacesMidStreamError 核心回归：error 帧必须透出，不得伪造成 end_turn。
func TestAnthropicStreamSurfacesMidStreamError(t *testing.T) {
	up := newFakeUpstream(t, func(string) (int, string, bool) { return 200, midStreamErrorSSE, true })
	h := NewHandler(Config{Pool: testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999},
	), Upstream: up, MaxRotate: 1})

	status, body := messagesStreamBody(t, h)

	if status != http.StatusOK {
		t.Fatalf("status 头在 message_start 时已提交，应为 200，实际 %d", status)
	}
	if !strings.Contains(body, "event: error") {
		t.Errorf("上游 error 帧必须以 Anthropic error 事件透出，实际响应体：\n%s", body)
	}
	if !strings.Contains(body, "渠道未批准") && !strings.Contains(body, "11128") {
		t.Errorf("error 事件必须带上上游的真实原因（msg 或 code），实际响应体：\n%s", body)
	}
	// 这两条是本次修复的核心：修前它们都会被无条件写出，等于对客户端宣告成功。
	if strings.Contains(body, "message_stop") {
		t.Errorf("已命中终止性错误时不得再发 message_stop（那会被理解为正常完成）：\n%s", body)
	}
	if strings.Contains(body, "end_turn") {
		t.Errorf("已命中终止性错误时不得回 end_turn：\n%s", body)
	}
}

// TestAnthropicStreamTruncatedAfterFinishReasonIsFailure 上游给过 finish_reason
// 但还没写 [DONE] 就断开，必须按失败收尾。
//
// 对应真实触发：IdleTimeout 在两次事件之间取消 ctx，ReadString 返回
// context.Canceled，旧实现 `if err != nil { break }` 之外没有任何判错，
// 于是照常补 end_turn + message_stop。注意 finish_reason 在那之前就到了，
// 所以不能拿 finish_reason 当「上游已正常收尾」的标志 —— [DONE] 才是。
func TestAnthropicStreamTruncatedAfterFinishReasonIsFailure(t *testing.T) {
	// 有 finish_reason、有 content，但流被截断（无 [DONE]）。
	const truncated = "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"half\"},\"finish_reason\":null}]}\n\n" +
		"data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"

	up := newFakeUpstream(t, func(string) (int, string, bool) { return 200, truncated, true })
	h := NewHandler(Config{Pool: testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999},
	), Upstream: up, MaxRotate: 1})

	_, body := messagesStreamBody(t, h)

	if strings.Contains(body, "end_turn") {
		t.Errorf("上游未写 [DONE] 就断开，不得伪造成 end_turn：\n%s", body)
	}
	if !strings.Contains(body, "event: error") {
		t.Errorf("异常断流必须透出 error 事件：\n%s", body)
	}
}

// TestAnthropicStreamDoneAfterFinishReasonSucceeds 规范收尾：finish_reason 之后
// 跟着 [DONE]，必须照常收尾 —— 这条锁住「不能把所有带 finish_reason 的流都当故障」，
// 否则真实上游的正常响应会被大面积误报成错误。
func TestAnthropicStreamDoneAfterFinishReasonSucceeds(t *testing.T) {
	const wellFormed = "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n" +
		"data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n" +
		"data: [DONE]\n\n"

	up := newFakeUpstream(t, func(string) (int, string, bool) { return 200, wellFormed, true })
	h := NewHandler(Config{Pool: testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999},
	), Upstream: up, MaxRotate: 1})

	_, body := messagesStreamBody(t, h)

	if !strings.Contains(body, "message_stop") {
		t.Errorf("规范收尾（finish_reason + [DONE]）应发 message_stop：\n%s", body)
	}
	if !strings.Contains(body, "end_turn") {
		t.Errorf("规范收尾应回 end_turn：\n%s", body)
	}
	if strings.Contains(body, "event: error") {
		t.Errorf("规范收尾不应被误报成错误：\n%s", body)
	}
}

// TestAnthropicStreamCleanEOFStillSucceeds 上游什么都没给就干净 EOF（无 error 帧、
// 无 finish_reason、无 [DONE]）沿用原有的正常收尾 —— 这覆盖 gzip 等场景下 EOF 与
// [DONE] 等价，不能因为本次修复把合法空答复误报成故障。注意这里没有任何已下发
// 的内容会被截断，所以判为成功是无损的。
func TestAnthropicStreamCleanEOFStillSucceeds(t *testing.T) {
	// 真正的「什么都没给」：空响应体。
	up := newFakeUpstream(t, func(string) (int, string, bool) { return 200, "", true })
	h := NewHandler(Config{Pool: testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999},
	), Upstream: up, MaxRotate: 1})

	_, body := messagesStreamBody(t, h)

	if !strings.Contains(body, "message_stop") {
		t.Errorf("上游干净 EOF 应照常收尾（发 message_stop）：\n%s", body)
	}
	if strings.Contains(body, "event: error") {
		t.Errorf("干净 EOF 不应被误报成错误：\n%s", body)
	}
}

// TestResponsesStreamSurfacesMidStreamError Responses 路径同样不得把 error 帧
// 吞掉后补 response.completed（Codex 会把截断回答当成成功回合）。
func TestResponsesStreamSurfacesMidStreamError(t *testing.T) {
	up := newFakeUpstream(t, func(string) (int, string, bool) { return 200, midStreamErrorSSE, true })
	h := NewHandler(Config{Pool: testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999},
	), Upstream: up, MaxRotate: 1})

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/responses",
		strings.NewReader(`{"model":"deepseek-v4.1-flash","stream":true,`+
			`"input":[{"role":"user","content":"hi"}]}`)))
	body := rec.Body.String()

	if !strings.Contains(body, "response.failed") {
		t.Errorf("上游 error 帧必须以 response.failed 透出，实际响应体：\n%s", body)
	}
	if !strings.Contains(body, "渠道未批准") && !strings.Contains(body, "11128") {
		t.Errorf("response.failed 必须带上上游真实原因：\n%s", body)
	}
	if strings.Contains(body, "response.completed") {
		t.Errorf("已命中终止性错误时不得再发 response.completed：\n%s", body)
	}
}

// TestErrorFrameTextShapes error 帧文案的形态覆盖：三种常见形状都要能取出可读文案，
// 且都取不到时退化为 code，绝不返回空串（空串会让失败重新变成不可见）。
func TestErrorFrameTextShapes(t *testing.T) {
	cases := []struct {
		name string
		in   any
		want string
	}{
		{"openai message", map[string]any{"message": "account banned"}, "account banned"},
		{"relay msg", map[string]any{"msg": "渠道未批准"}, "渠道未批准"},
		{"detail", map[string]any{"detail": "rate limited"}, "rate limited"},
		{"code only", map[string]any{"code": float64(11128)}, "code=11128"},
		{"bare string", "boom", "boom"},
		{"empty object", map[string]any{}, "unknown upstream error"},
		{"nil", nil, "unknown upstream error"},
	}
	for _, c := range cases {
		if got := errorFrameText(c.in); got != c.want {
			t.Errorf("%s: errorFrameText(%v) = %q, want %q", c.name, c.in, got, c.want)
		}
	}
}

// TestStreamFailureMessagePrefersUpstreamError 三种情形的判定口径：
// 见到 [DONE] 或整条流为空 → 正常收尾；error 帧或「吐了一半就断」→ 失败，
// 且 error 帧优先（那是根因）。
func TestStreamFailureMessagePrefersUpstreamError(t *testing.T) {
	if got := streamFailureMessage("", true, true); got != "" {
		t.Errorf("见到 [DONE] 应返回空串，实际 %q", got)
	}
	if got := streamFailureMessage("", false, false); got != "" {
		t.Errorf("空流（无任何帧）应沿用正常收尾，实际 %q", got)
	}
	if got := streamFailureMessage("banned", true, true); !strings.Contains(got, "banned") {
		t.Errorf("应优先报上游 error，实际 %q", got)
	}
	if got := streamFailureMessage("", false, true); got == "" {
		t.Errorf("吐出过内容却没见 [DONE] 就断开，必须判为失败")
	}
}
