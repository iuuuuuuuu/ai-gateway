package server

// 上游拒绝思考档位时的处理契约（所有者明确要求）：
// 「当思考档位不被接受的时候，就把上游报错处理一下，返回出来」。
//
// 背景：网关**不再**自己预校验档位（supportedEfforts 不是硬范围，
// 据它拦截会误拒合法请求 —— 见 forward.go 的说明）。但上游**确实会**拒绝
// 某些档位：实测 deepseek-v4.1-flash 与 deepseek-v4-pro 拒绝 off，
// 原文 "the reasoning effort value is not supported by the current model"。
//
// 这时网关要做的是**如实转达**，而不是把它混进「账号不可用」的 503 里 ——
// 后者会让用户去查账号，而真实原因是请求参数。
//
// 三条契约：
//   ① 状态码用上游的真实状态（400），不是 503
//   ② 错误码单独成型（reasoning_effort_rejected），让客户端能识别
//   ③ 消息**带上上游原文**，用户据此判断该换哪个档位

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

// 上游真实原文（2026-09-18 实测，deepseek-v4.1-flash + reasoning_effort=off）。
const effortRejectedBody = `{"code":400,"msg":"the reasoning effort value is not supported by the current model","requestId":"x"}`

// effortRejectingHandler 上游一律回 400 + 档位拒绝原文。
func effortRejectingHandler(t *testing.T) (*Handler, *int32) {
	t.Helper()
	resetModelsCache()
	var calls int32
	p := testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999, Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40},
		&auth.Auth{UID: "u2", AccessToken: "at2", ExpiresAt: 9999999999, Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40},
		&auth.Auth{UID: "u3", AccessToken: "at3", ExpiresAt: 9999999999, Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40},
	)
	h := NewHandler(Config{
		Pool:      p,
		MaxRotate: 3,
		Upstream: newFakeUpstream(t, func(string) (int, string, bool) {
			atomic.AddInt32(&calls, 1)
			return http.StatusBadRequest, effortRejectedBody, false
		}),
	})
	return h, &calls
}

// TestEffortRejectedReturnsUpstreamStatus 用上游真实状态码（400），不是 503。
//
// 报 503 会把「你的参数上游不接受」说成「服务端暂时不可用」，
// 客户端据此会去重试，而重试永远不会成功。
func TestEffortRejectedReturnsUpstreamStatus(t *testing.T) {
	h, _ := effortRejectingHandler(t)

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"effort-fixed","reasoning_effort":"off","messages":[{"role":"user","content":"hi"}]}`)))

	if rec.Code != http.StatusBadRequest {
		t.Errorf("状态码应为上游的 400（请求侧错误），实际 %d（body=%s）", rec.Code, rec.Body.String())
	}
	if rec.Code == http.StatusServiceUnavailable {
		t.Error("**不得**回 503 —— 那会让客户端以为是服务端故障而去重试")
	}
}

// TestEffortRejectedHasDedicatedCode 错误码单独成型，不与账号故障混用。
func TestEffortRejectedHasDedicatedCode(t *testing.T) {
	h, _ := effortRejectingHandler(t)

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"effort-fixed","reasoning_effort":"off","messages":[{"role":"user","content":"hi"}]}`)))

	var payload map[string]any
	if err := json.Unmarshal(rec.Body.Bytes(), &payload); err != nil {
		t.Fatalf("响应不是 JSON: %v", err)
	}
	errObj, _ := payload["error"].(map[string]any)
	code, _ := errObj["code"].(string)
	if code != "reasoning_effort_rejected" {
		t.Errorf("错误码应为 reasoning_effort_rejected，实际 %q（body=%s）", code, rec.Body.String())
	}
	if code == "no_healthy_account" {
		t.Error("**不得**落进 no_healthy_account —— 那会把排查方向引向账号")
	}
}

// TestEffortRejectedKeepsUpstreamText 消息里必须带上游原文。
//
// 这是本功能的核心价值：该换哪个档位取决于具体模型（实测 off 在 14/16 个
// 模型可用、两个 deepseek 拒绝），没有任何通用表能替用户判断 ——
// 所以必须把上游说的话原样给他。
func TestEffortRejectedKeepsUpstreamText(t *testing.T) {
	h, _ := effortRejectingHandler(t)

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"effort-fixed","reasoning_effort":"off","messages":[{"role":"user","content":"hi"}]}`)))

	var payload map[string]any
	_ = json.Unmarshal(rec.Body.Bytes(), &payload)
	errObj, _ := payload["error"].(map[string]any)
	msg, _ := errObj["message"].(string)

	if !strings.Contains(msg, "the reasoning effort value is not supported") {
		t.Errorf("消息应保留上游原文，实际 %q", msg)
	}
	if !strings.Contains(msg, "reasoning_effort") {
		t.Errorf("消息应点明是哪个字段的问题，实际 %q", msg)
	}
}

// TestEffortRejectedDoesNotRotateAccounts 不换号、不罚账号。
//
// 这是请求侧错误：同一请求体发给任何账号都会被同一个模型拒绝。
// 轮转会白打 3 次上游并把整个请求体重传（实测上下文超长那条路径踩过同样的坑），
// 最后还把真实原因掩盖成「账号全部不可用」。
func TestEffortRejectedDoesNotRotateAccounts(t *testing.T) {
	h, calls := effortRejectingHandler(t)

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"effort-fixed","reasoning_effort":"off","messages":[{"role":"user","content":"hi"}]}`)))

	if rec.Code != http.StatusBadRequest {
		t.Fatalf("前置条件：应 400，实际 %d", rec.Code)
	}
	if n := atomic.LoadInt32(calls); n != 1 {
		t.Errorf("请求侧错误只应打上游 1 次，实际 %d 次 —— "+
			"轮转会白打多次并把真实原因掩盖成「账号全部不可用」", n)
	}
	// 账号状态不该被这条错误污染（它拒绝的是参数，不是账号）。
	for _, st := range h.cfg.Pool.List() {
		if st.Disabled {
			t.Errorf("档位被拒不该禁用账号 %s", st.UID)
		}
		if st.ErrTotal > 0 {
			t.Errorf("档位被拒不该计入账号错误数（%s err_total=%d）", st.UID, st.ErrTotal)
		}
	}
}

// TestEffortRejectedClassification 分类器认得这类响应。
//
// 三种真实形态都要认出：上游 msg 字段、error.message、以及大小写差异。
func TestEffortRejectedClassification(t *testing.T) {
	yes := []string{
		effortRejectedBody,
		`{"error":{"message":"the reasoning effort value is not supported by the current model"}}`,
		`{"msg":"The Reasoning Effort Value Is Not Supported By The Current Model"}`,
		`unsupported reasoning effort`,
	}
	for _, body := range yes {
		if !upstream.IsEffortRejected(body) {
			t.Errorf("应识别为档位被拒: %s", body)
		}
		if got := upstream.Classify(http.StatusBadRequest, body); got != upstream.ErrEffortRejected {
			t.Errorf("Classify 应返回 ErrEffortRejected，实际 %v（body=%s）", got, body)
		}
	}

	// 反面：其它 400 不得被误判成档位问题（否则用户会去找一个不存在的档位错误）。
	no := []string{
		`{"code":11102,"msg":"model service info not found"}`,
		`{"code":11115,"msg":"prompt is too long"}`,
		`{"code":11128,"msg":"channel not approved"}`,
	}
	for _, body := range no {
		if upstream.IsEffortRejected(body) {
			t.Errorf("不该识别为档位被拒: %s", body)
		}
	}
}

// TestEffortRejectedDetailFallsBackToBody msg 取不到时回退原始 body，而不是编造。
func TestEffortRejectedDetailFallsBackToBody(t *testing.T) {
	if got := upstream.EffortRejectedDetail(`{"msg":"upstream said this"}`); got != "upstream said this" {
		t.Errorf("应取出 msg 字段，实际 %q", got)
	}
	if got := upstream.EffortRejectedDetail(`{"error":{"message":"nested msg"}}`); got != "nested msg" {
		t.Errorf("应取出 error.message，实际 %q", got)
	}
	if got := upstream.EffortRejectedDetail(`raw text body`); got != "raw text body" {
		t.Errorf("取不到 msg 时应回退原始 body，实际 %q", got)
	}
	if got := upstream.EffortRejectedDetail(``); got == "" {
		t.Error("空 body 也应给出一句说明（而不是空串让界面显示空白）")
	}
}
