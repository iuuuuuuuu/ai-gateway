package server

// 上下文超长的端到端回归（2026-09-15 现场）。
//
// 现场表现（另一个会话）：
//
//	400 {"code":"no_healthy_account","message":"all accounts unavailable
//	     (cooling/disabled): upstream client (http 400): {\"code\":11115,
//	     \"msg\":\"prompt is too long: 1119655 tokens > 1048576 maximum\"...
//
// 而当时账号池 19 个号 18 个健康、0 个禁用 —— 那句「账号全部不可用」是假的。
//
// 两个必须锁住的行为：
//  1. 请求侧错误**不得**触发换号重试（否则整个请求体对着每个账号重传一遍）。
//  2. 错误码必须反映真实原因，不能一律 no_healthy_account。

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/upstream"
)

// realContextTooLongBody 现场真实响应体（逐字复制）。
const realContextTooLongBody = `{"code":11115,"msg":"prompt is too long: 1119655 tokens > 1048576 maximum",` +
	`"requestId":"8a6bc76c-690c-457b-8702-0065ba4cb1e5",` +
	`"extError":{"code":"context_length_exceeded","message":"prompt is too long: 1119655 tokens > 1048576 maximum",` +
	`"param":"","type":"invalid_request_error","StatusCode":400,"Request":null,"Response":null},` +
	`"displayMsg":{"en":"The request exceeds the model context limit. Please shorten the conversation or remove attachments.",` +
	`"zh":"对话内容超出模型长度上限，请精简对话或减少附件后重试。"}}`

// TestContextTooLongDoesNotRotateAccounts 核心回归：上下文超长只打上游一次。
//
// 旧实现会把同一个请求体发给池里每个账号（默认 MaxRotate=3），
// 对 1.12M token 的请求意味着白白上传三次。
func TestContextTooLongDoesNotRotateAccounts(t *testing.T) {
	var calls int32
	up := newFakeUpstream(t, func(string) (int, string, bool) {
		atomic.AddInt32(&calls, 1)
		return 400, realContextTooLongBody, false
	})
	p := testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999},
		&auth.Auth{UID: "u2", AccessToken: "at2", ExpiresAt: 9999999999},
		&auth.Auth{UID: "u3", AccessToken: "at3", ExpiresAt: 9999999999},
	)
	h := NewHandler(Config{Pool: p, Upstream: up, MaxRotate: 3})

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"hi"}]}`)))

	if n := atomic.LoadInt32(&calls); n != 1 {
		t.Errorf("上下文超长应只打上游 1 次（换号无用），实际 %d 次", n)
	}
	if rec.Code != http.StatusBadRequest {
		t.Errorf("code=%d want 400（请求侧错误，非账号故障）", rec.Code)
	}
}

// TestContextTooLongDoesNotPenalizeAccounts 请求侧错误不得惩罚账号。
func TestContextTooLongDoesNotPenalizeAccounts(t *testing.T) {
	up := newFakeUpstream(t, func(string) (int, string, bool) {
		return 400, realContextTooLongBody, false
	})
	p := testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999})
	h := NewHandler(Config{Pool: p, Upstream: up})

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"glm-5.2","messages":[]}`)))

	st, _ := p.Status("u1")
	if st.Cooling || st.Disabled || st.ErrTotal != 0 {
		t.Errorf("请求侧错误不应惩罚账号: cooling=%v disabled=%v errTotal=%d",
			st.Cooling, st.Disabled, st.ErrTotal)
	}
}

// TestContextTooLongErrorCodeAndWording 错误码与文案必须让客户端能识别溢出。
//
// 客户端（DeepSeek Harness）靠文案模式触发自动压缩；若我们把它包装成
// no_healthy_account，客户端只会以为「账号挂了，稍后重试」——
// 那正是本次会话反复重发超长请求、无法自愈的直接原因。
func TestContextTooLongErrorCodeAndWording(t *testing.T) {
	up := newFakeUpstream(t, func(string) (int, string, bool) {
		return 400, realContextTooLongBody, false
	})
	p := testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999})
	h := NewHandler(Config{Pool: p, Upstream: up})

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"glm-5.2","messages":[]}`)))

	var e struct {
		Error struct {
			Code    string `json:"code"`
			Message string `json:"message"`
		} `json:"error"`
	}
	if err := json.Unmarshal(rec.Body.Bytes(), &e); err != nil {
		t.Fatalf("error envelope 解析失败: %v body=%s", err, rec.Body)
	}

	if e.Error.Code != "context_length_exceeded" {
		t.Errorf("code=%q want context_length_exceeded（真实原因）", e.Error.Code)
	}
	if strings.Contains(e.Error.Code, "no_healthy_account") {
		t.Error("不得再报 no_healthy_account：账号是好的，失败的是这次请求")
	}
	if strings.Contains(e.Error.Message, "all accounts unavailable") {
		t.Errorf("不得声称账号全部不可用，实际=%q", e.Error.Message)
	}
	// 客户端识别溢出所需的特征串。
	if !strings.Contains(e.Error.Message, "prompt is too long") &&
		!strings.Contains(e.Error.Message, "context_length_exceeded") {
		t.Errorf("应含客户端可识别的溢出特征串，实际=%q", e.Error.Message)
	}
	// 人类定位所需的原始 token 数。
	if !strings.Contains(e.Error.Message, "1119655") {
		t.Errorf("应保留上游 token 数，实际=%q", e.Error.Message)
	}
}

// TestContextTooLongAllProtocols 三种协议入口都要给出各自的正确错误码与状态。
func TestContextTooLongAllProtocols(t *testing.T) {
	cases := []struct {
		name     string
		path     string
		body     string
		wantCode string
	}{
		{
			"chat/completions", "/v1/chat/completions",
			`{"model":"glm-5.2","messages":[{"role":"user","content":"hi"}]}`,
			"context_length_exceeded",
		},
		{
			"responses", "/v1/responses",
			`{"model":"glm-5.2","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}`,
			"context_length_exceeded",
		},
		{
			"messages", "/v1/messages",
			`{"model":"glm-5.2","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}`,
			"invalid_request_error",
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			up := newFakeUpstream(t, func(string) (int, string, bool) {
				return 400, realContextTooLongBody, false
			})
			h := NewHandler(Config{
				Pool:     testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999}),
				Upstream: up,
			})
			rec := httptest.NewRecorder()
			h.ServeHTTP(rec, httptest.NewRequest("POST", c.path, strings.NewReader(c.body)))

			if rec.Code != http.StatusBadRequest {
				t.Errorf("code=%d want 400；body=%s", rec.Code, rec.Body)
			}
			if !strings.Contains(rec.Body.String(), c.wantCode) {
				t.Errorf("body 应含 %q，实际=%s", c.wantCode, rec.Body)
			}
			if strings.Contains(rec.Body.String(), "no_healthy_account") {
				t.Errorf("不得报 no_healthy_account：%s", rec.Body)
			}
		})
	}
}

// TestGenuineNoAccountStillReportsNoHealthyAccount 真·无可用账号时仍按原契约报错。
//
// 这是防回归的另一面：修复不能把「账号真的全挂了」也一并改掉 ——
// 那种情况客户端**应该**稍后重试，错误码必须保持 no_healthy_account。
func TestGenuineNoAccountStillReportsNoHealthyAccount(t *testing.T) {
	up := newFakeUpstream(t, func(string) (int, string, bool) {
		t.Error("无可用账号时不应请求上游")
		return 200, sseOK, true
	})
	p := testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999})
	p.Disable("u1", "session dead") // 唯一的号被禁用 → 真的无号可用
	h := NewHandler(Config{Pool: p, Upstream: up})

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"glm-5.2","messages":[]}`)))

	if rec.Code != http.StatusServiceUnavailable {
		t.Errorf("code=%d want 503（账号池不可用）", rec.Code)
	}
	if !strings.Contains(rec.Body.String(), "no_healthy_account") {
		t.Errorf("真·无可用账号应保持 no_healthy_account，实际=%s", rec.Body)
	}
}

// TestGenericClientErrorKeepsLegacyContract 其余 4xx 行为不变（不惩罚账号、仍报 503）。
//
// 明确锁定：本次修复只给「上下文超长」开特例，不改变其他错误路径的对外契约。
func TestGenericClientErrorKeepsLegacyContract(t *testing.T) {
	up := newFakeUpstream(t, func(string) (int, string, bool) {
		return 400, `{"code":11102,"msg":"model service info not found"}`, false
	})
	p := testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999})
	h := NewHandler(Config{Pool: p, Upstream: up})

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"glm-5.2","messages":[]}`)))

	if rec.Code != http.StatusServiceUnavailable {
		t.Errorf("code=%d want 503（保持既有契约）", rec.Code)
	}
	st, _ := p.Status("u1")
	if st.Cooling || st.ErrTotal != 0 {
		t.Errorf("通用 4xx 不应惩罚账号: %+v", st)
	}
	_ = upstream.ErrNone
}
