package server

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/upstream"
)

// ---------------------------------------------------------------------------
// 需求（2026-09-15）：模型级限流（429 code=6004）的全链路行为
//
// 用上游真实响应体验证：识别 → 解析重置时间 → 按 uid+model 冷却 → 路由避开该模型。
// ---------------------------------------------------------------------------

// modelRate429 上游真实响应体（现场抓取，2026-09-15）。
const modelRate429 = `{"code":"6004","msg":"您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，您也可以切换其他模型继续使用。","requestId":"62b8393f-e7ce-46e6-912e-6e610a8d1e01"}`

// resetTimeIn 生成一个 n 小时后的"重置时刻"文案，避免依赖固定日期导致过期。
func resetTimeIn(d time.Duration) string {
	return time.Now().Add(d).In(time.FixedZone("UTC+8", 8*3600)).Format("2006-01-02 15:04:05")
}

func modelRateBody(d time.Duration) string {
	return `{"code":"6004","msg":"您的使用量已超出频率限制，将在 ` + resetTimeIn(d) + ` UTC+8 重置，您也可以切换其他模型继续使用。","requestId":"x"}`
}

// TestModelRateCooldownFromRealBody 核心：429+6004 应记为**模型级**冷却，
// 到期时间取自上游文案，且不把账号标成账号级冷却。
func TestModelRateCooldownFromRealBody(t *testing.T) {
	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		if authz == "Bearer at-bad" {
			return 429, modelRateBody(2 * time.Hour), false
		}
		return 200, sseOK, true
	})
	p := testPoolWith(
		&auth.Auth{UID: "bad", AccessToken: "at-bad", ExpiresAt: 9999999999},
		&auth.Auth{UID: "good", AccessToken: "at-good", ExpiresAt: 9999999999},
	)
	p.SetCredits("bad", 2000) // bad 先被选中（确定性源）
	p.SetCredits("good", 1000)
	h := NewHandler(Config{Pool: p, Upstream: up})

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"glm-5.2","messages":[]}`)))
	if rec.Code != 200 {
		t.Fatalf("换号后应成功：code=%d body=%s", rec.Code, rec.Body)
	}

	st, _ := p.Status("bad")
	// 关键区分：模型冷却 ≠ 账号级冷却
	if st.Cooling {
		t.Errorf("模型级限流不应把账号标成账号级冷却（那是余额欠费的语义）: %+v", st)
	}
	if len(st.ModelCooling) != 1 {
		t.Fatalf("应记 1 条模型冷却，实际 %+v", st.ModelCooling)
	}
	mc := st.ModelCooling[0]
	if mc.Model != "glm-5.2" {
		t.Errorf("model=%q want glm-5.2", mc.Model)
	}
	// 到期时间应约 2 小时后（来自文案，不是固定 soft_rate 60s）
	if mc.RemainingSec < 7100 || mc.RemainingSec > 7300 {
		t.Errorf("remaining_sec=%d，期望约 2 小时（取自上游重置时间，而非固定软冷却）", mc.RemainingSec)
	}
	if !mc.ResetAtParsed {
		t.Error("reset_at_parsed 应为 true")
	}
	if mc.Reason == "" || !strings.Contains(mc.Reason, "glm-5.2") {
		t.Errorf("reason 应含模型名，实际=%q", mc.Reason)
	}
}

// TestModelRateAvoidsAccountForSameModel 该账号在同模型上被后续请求绕开，
// 但换模型仍会用它（"您也可以切换其他模型继续使用"）。
//
// fake 上游按**模型**区分：bad 只在 glm-5.2 上被限流，换模型即恢复正常
// —— 这正是上游文案承诺的行为，也是本需求要保护的语义。
func TestModelRateAvoidsAccountForSameModel(t *testing.T) {
	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		if authz == "Bearer at-bad" {
			return 429, modelRateBody(2 * time.Hour), false
		}
		return 200, sseOK, true
	})
	p := testPoolWith(
		&auth.Auth{UID: "bad", AccessToken: "at-bad", ExpiresAt: 9999999999},
		&auth.Auth{UID: "good", AccessToken: "at-good", ExpiresAt: 9999999999},
	)
	p.SetCredits("bad", 2000)
	p.SetCredits("good", 1000)
	h := NewHandler(Config{Pool: p, Upstream: up})

	// 第一次：bad 被限流，换到 good
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"glm-5.2","messages":[]}`)))
	if rec.Code != 200 {
		t.Fatalf("first call failed: %d %s", rec.Code, rec.Body)
	}

	// 第二次（同模型）：应直接避开 bad —— 由 good 服务，且 bad 的成功数不增加。
	beforeBad, _ := p.Status("bad")
	beforeGood, _ := p.Status("good")
	rec2 := httptest.NewRecorder()
	h.ServeHTTP(rec2, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"glm-5.2","messages":[]}`)))
	if rec2.Code != 200 {
		t.Fatalf("second call failed: %d %s", rec2.Code, rec2.Body)
	}
	afterBad, _ := p.Status("bad")
	afterGood, _ := p.Status("good")
	if afterBad.SuccessCount != beforeBad.SuccessCount {
		t.Errorf("同模型下 bad 应被绕开，但成功数从 %d 涨到 %d", beforeBad.SuccessCount, afterBad.SuccessCount)
	}
	if afterGood.SuccessCount != beforeGood.SuccessCount+1 {
		t.Errorf("good 应承接第二次请求：%d → %d", beforeGood.SuccessCount, afterGood.SuccessCount)
	}
}

// TestModelRateOtherModelStillUsable 换模型后该账号**仍会被路由到**（不被账号级封禁）。
//
// 与上一个用例分开写：这里只断言"bad 仍是健康账号、可被选中"，
// 用 PickForModel 直接验证池的裁决，不受 fake 上游行为影响。
func TestModelRateOtherModelStillUsable(t *testing.T) {
	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		if authz == "Bearer at-bad" {
			return 429, modelRateBody(2 * time.Hour), false
		}
		return 200, sseOK, true
	})
	p := testPoolWith(
		&auth.Auth{UID: "bad", AccessToken: "at-bad", ExpiresAt: 9999999999},
		&auth.Auth{UID: "good", AccessToken: "at-good", ExpiresAt: 9999999999},
	)
	p.SetCredits("bad", 2000)
	p.SetCredits("good", 1000)
	h := NewHandler(Config{Pool: p, Upstream: up})

	// 触发一次 glm-5.2 限流
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"glm-5.2","messages":[]}`)))
	if rec.Code != 200 {
		t.Fatalf("code=%d body=%s", rec.Code, rec.Body)
	}

	// 同模型：bad 被排除
	if got := p.PickForModel("glm-5.2", nil); got != nil && got.UID == "bad" {
		t.Error("同模型下 bad 应被排除")
	}
	// 换模型：bad 重新可用（账号本身未被封）
	if got := p.PickForModel("deepseek-v4.1-flash", nil); got == nil || got.UID != "bad" {
		t.Errorf("换模型后 bad 应重新可用（它是健康账号），got=%+v", got)
	}
	// 账号状态仍是健康（非账号级冷却）
	st, _ := p.Status("bad")
	if st.Cooling || st.Disabled {
		t.Errorf("bad 不应是账号级冷却/禁用: cooling=%v disabled=%v", st.Cooling, st.Disabled)
	}
}

// TestModelRateFallsBackToSoftCooldown 文案里解析不出重置时间时回退固定软冷却。
func TestModelRateFallsBackToSoftCooldown(t *testing.T) {
	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		if authz == "Bearer at-bad" {
			// 6004 但没有可解析的时间字面量
			return 429, `{"code":"6004","msg":"您的使用量已超出频率限制，您也可以切换其他模型继续使用。"}`, false
		}
		return 200, sseOK, true
	})
	p := testPoolWith(
		&auth.Auth{UID: "bad", AccessToken: "at-bad", ExpiresAt: 9999999999},
		&auth.Auth{UID: "good", AccessToken: "at-good", ExpiresAt: 9999999999},
	)
	p.SetCredits("bad", 2000)
	p.SetCredits("good", 1000)
	h := NewHandler(Config{Pool: p, Upstream: up, SoftCooldown: 90 * time.Second})

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"glm-5.2","messages":[]}`)))
	if rec.Code != 200 {
		t.Fatalf("code=%d body=%s", rec.Code, rec.Body)
	}

	st, _ := p.Status("bad")
	if len(st.ModelCooling) != 1 {
		t.Fatalf("应记 1 条模型冷却，实际 %+v", st.ModelCooling)
	}
	mc := st.ModelCooling[0]
	if mc.ResetAtParsed {
		t.Error("解析失败时 reset_at_parsed 应为 false")
	}
	if mc.RemainingSec < 85 || mc.RemainingSec > 95 {
		t.Errorf("remaining_sec=%d，期望约 90s（配置的 soft_rate）", mc.RemainingSec)
	}
}

// TestPlain429StaysAccountLevel 普通 429（无 6004）仍走账号级软冷却，未被新逻辑改坏。
func TestPlain429StaysAccountLevel(t *testing.T) {
	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		if authz == "Bearer at-bad" {
			return 429, `{"code":"1001","msg":"too many requests"}`, false
		}
		return 200, sseOK, true
	})
	p := testPoolWith(
		&auth.Auth{UID: "bad", AccessToken: "at-bad", ExpiresAt: 9999999999},
		&auth.Auth{UID: "good", AccessToken: "at-good", ExpiresAt: 9999999999},
	)
	p.SetCredits("bad", 2000)
	p.SetCredits("good", 1000)
	h := NewHandler(Config{Pool: p, Upstream: up})

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"glm-5.2","messages":[]}`)))
	if rec.Code != 200 {
		t.Fatalf("code=%d body=%s", rec.Code, rec.Body)
	}

	st, _ := p.Status("bad")
	if !st.Cooling || st.CoolKind != "soft_rate" {
		t.Errorf("普通 429 应保持账号级软冷却: cooling=%v kind=%q", st.Cooling, st.CoolKind)
	}
	if len(st.ModelCooling) != 0 {
		t.Errorf("普通 429 不应产生模型冷却: %+v", st.ModelCooling)
	}
}

// TestModelRateAllAccountsCooled Returns503 且错误信息说明是模型级受限。
func TestModelRateAllAccountsCooled(t *testing.T) {
	up := newFakeUpstream(t, func(string) (int, string, bool) {
		return 429, modelRateBody(2 * time.Hour), false
	})
	p := testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999})
	h := NewHandler(Config{Pool: p, Upstream: up})

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"glm-5.2","messages":[]}`)))
	if rec.Code != http.StatusServiceUnavailable {
		t.Errorf("code=%d want 503（唯一账号的该模型已受限）", rec.Code)
	}
	st, _ := p.Status("u1")
	if len(st.ModelCooling) != 1 {
		t.Errorf("应记录模型冷却: %+v", st.ModelCooling)
	}
}

// TestClassifyIntegration 确认 upstream.Classify 对真实响应体的分类未被其他改动影响。
func TestClassifyIntegration(t *testing.T) {
	if got := upstream.Classify(429, modelRate429); got != upstream.ErrModelRate {
		t.Errorf("真实样本 → %v want ErrModelRate", got)
	}
}
