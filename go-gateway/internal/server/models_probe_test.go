package server

import (
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/upstream"
)

// ---------------------------------------------------------------------------
// 回归：拉取 /models 失败**不得**喂熔断器
//
// 实测缺陷（2026-09）：国际版账号的 /console/enterprises/personal/models
// 恒返回 HTTP 500，而同账号的 chat 完全正常。fetchDynamicModels 在失败时
// 调了 Pool.NoteError，于是：
//
//   国际版账号恰好占据最早到期档位（分层选号优先选它们）
//   → 客户端每次启动探模型都记一次失败
//   → 累计 3 次（breaker_threshold）触发 30 分钟熔断
//   → 界面上表现为「这几个国际版账号莫名被熔断」
//
// 关键点：熔断的语义是「该账号**调用业务接口**失败」；/models 是能力探测
// （拿 contextWindow / supportedEfforts），失败只该退回静态表。
// ---------------------------------------------------------------------------

// modelsFailUpstream 模拟「/models 恒 500」的上游 —— 国际版账号的实测行为。
// 复用同包已有的 newFakeUpstream，不另造一套。
func modelsFailUpstream(t *testing.T) *upstream.Client {
	t.Helper()
	return newFakeUpstream(t, func(_ string) (int, string, bool) {
		return http.StatusInternalServerError, `{"code":500,"msg":"internal"}`, false
	})
}

// resetModelsCache 清掉动态模型缓存，让下一次调用真的去打上游。
//
// 生产代码有 5 分钟失败负缓存，测试里必须绕开它才能复现「反复探测」。
func resetModelsCache() {
	dynamicModelsCache.Lock()
	dynamicModelsCache.ids = nil
	dynamicModelsCache.fetched = time.Time{}
	dynamicModelsCache.lastFail = time.Time{}
	dynamicModelsCache.Unlock()
}

// callModels 调一次 /v1/models，返回状态码。
func callModels(h *Handler) int {
	req := httptest.NewRequest(http.MethodGet, "/v1/models", nil)
	rec := httptest.NewRecorder()
	h.mux.ServeHTTP(rec, req)
	return rec.Code
}

// TestModelsProbeFailureDoesNotFeedBreaker 拉模型失败后，账号的失败计数必须保持 0。
func TestModelsProbeFailureDoesNotFeedBreaker(t *testing.T) {
	resetModelsCache()
	p := testPoolWith(&auth.Auth{
		UID:             "cn-1",
		AccessToken:     "t",
		SoonestExpireAt: time.Now().Add(24 * time.Hour).Unix(),
	})
	h := NewHandler(Config{Pool: p, Upstream: modelsFailUpstream(t), MaxRotate: 1})

	code := callModels(h)

	// 接口本身必须成功（回退静态表），不能因为上游不可达而报错
	if code != http.StatusOK {
		t.Fatalf("/v1/models 应回退静态表并返回 200，实际 %d", code)
	}

	// 核心断言：失败计数必须仍是 0（未被熔断器记账）
	st, ok := p.Status("cn-1")
	if !ok {
		t.Fatal("账号应存在")
	}
	if st.BreakerFails != 0 {
		t.Fatalf("拉模型失败不应喂熔断器，BreakerFails=%d（期望 0）", st.BreakerFails)
	}
	if st.Cooling {
		t.Fatal("账号不应因此进入冷却")
	}
}

// TestModelsProbeRepeatedFailureStillNoBreaker 反复失败也不能累计到熔断阈值。
//
// 这正是线上现象：客户端多次启动 → 每次探测一次 → 累计 3 次 → 熔断。
func TestModelsProbeRepeatedFailureStillNoBreaker(t *testing.T) {
	p := testPoolWith(&auth.Auth{
		UID:             "cn-1",
		AccessToken:     "t",
		SoonestExpireAt: time.Now().Add(24 * time.Hour).Unix(),
	})
	h := NewHandler(Config{Pool: p, Upstream: modelsFailUpstream(t), MaxRotate: 1})

	// 探测 5 次（超过 breaker_threshold=3），每次前清掉负缓存
	for i := 0; i < 5; i++ {
		resetModelsCache()
		if code := callModels(h); code != http.StatusOK {
			t.Fatalf("第 %d 次 /v1/models 应返回 200，实际 %d", i+1, code)
		}
	}

	st, _ := p.Status("cn-1")
	if st.BreakerFails != 0 {
		t.Fatalf("反复探测失败不应累计失败次数，BreakerFails=%d", st.BreakerFails)
	}
	if st.Cooling {
		t.Fatal("反复探测失败不应导致熔断")
	}
}

// TestModelsProbePrefersNonIntlAccount 两者都在时，探测必须选国服账号。
//
// 国际版该端点恒 500；且国际版常占据最早到期档位（分层选号会优先选它），
// 若不绕开就会一直选中它、动态列表永远拉不到。
func TestModelsProbePrefersNonIntlAccount(t *testing.T) {
	p := testPoolWith(
		// 国际版到期更早 —— 按分层选号它会被优先选中
		&auth.Auth{UID: "intl-1", AccessToken: "t", Domain: "www.workbuddy.ai", SoonestExpireAt: time.Now().Add(time.Hour).Unix()},
		&auth.Auth{UID: "cn-1", AccessToken: "t", Domain: "copilot.tencent.com", SoonestExpireAt: time.Now().Add(72 * time.Hour).Unix()},
	)
	h := NewHandler(Config{Pool: p, Upstream: modelsFailUpstream(t), MaxRotate: 1})

	got := h.pickModelsProbeAccount()
	if got == nil {
		t.Fatal("应选出账号")
	}
	if got.UID != "cn-1" {
		t.Fatalf("探测应优先非国际版账号，实际选了 %q（国际版 models 端点恒 500）", got.UID)
	}
}

// TestModelsProbeFallsBackToIntlWhenOnlyIntl 全是国际版时仍要返回一个账号。
//
// 而不是返回 nil：万一上游修好了该端点，应当能自愈。
func TestModelsProbeFallsBackToIntlWhenOnlyIntl(t *testing.T) {
	p := testPoolWith(&auth.Auth{
		UID:             "intl-1",
		AccessToken:     "t",
		Domain:          "www.workbuddy.ai",
		SoonestExpireAt: time.Now().Add(24 * time.Hour).Unix(),
	})
	h := NewHandler(Config{Pool: p, Upstream: modelsFailUpstream(t), MaxRotate: 1})

	got := h.pickModelsProbeAccount()
	if got == nil {
		t.Fatal("全是国际版时仍应返回一个账号（上游修好后可自愈），而非 nil")
	}
	if got.UID != "intl-1" {
		t.Fatalf("应返回唯一的国际版账号，实际 %q", got.UID)
	}
}

// TestModelsProbeSkipsUnavailableAccounts 冷却/禁用的账号不应被选来探测。
func TestModelsProbeSkipsUnavailableAccounts(t *testing.T) {
	p := testPoolWith(
		&auth.Auth{UID: "intl-cooling", AccessToken: "t", Domain: "www.workbuddy.ai", SoonestExpireAt: time.Now().Add(time.Hour).Unix()},
		&auth.Auth{UID: "cn-ok", AccessToken: "t", Domain: "copilot.tencent.com", SoonestExpireAt: time.Now().Add(72 * time.Hour).Unix()},
	)
	p.Disable("cn-ok", "测试禁用")
	h := NewHandler(Config{Pool: p, Upstream: modelsFailUpstream(t), MaxRotate: 1})

	got := h.pickModelsProbeAccount()
	// 唯一可用的就是国际版那个 —— 应返回它，而不是被禁用的 cn-ok
	if got == nil {
		t.Fatal("应回退到可用的国际版账号")
	}
	if got.UID != "intl-cooling" {
		t.Fatalf("不应选被禁用的账号，实际 %q", got.UID)
	}
}
