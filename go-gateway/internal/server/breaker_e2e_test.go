package server

// breaker_e2e_test.go —— 端到端实测：真实 HTTP 链路下，单账号产品熔断后仍能发出请求。
//
// # 与 `internal/pool/single_account_breaker_test.go` 的分工
//
// 那个文件测的是 `Pool` 的**选号逻辑**（单元级）。
// 本文件补上端到端：**真实 Handler + 假上游 + 真实 HTTP 请求**，
// 走完「解析产品前缀 → 选号 → 派发 → 上游 → 错误分类 → NoteError → 熔断」
// 的完整链路，然后在熔断状态下再发一次请求看结果。
//
// # 为什么必须补这一层（所有者强制要求）
//
// 所有者原话：
//
//	「你就不能自己跑一下实测吗?非要让我测试」
//	「每次完成需求或者修复bug之后,必须强制跑实测,才能交付给我」
//
// 单元测试证明不了"打包后能跑"。本测试用**真实 HTTP** 复现所有者现场：
//
//	1. 上游返回 503（模拟他那 3 次失败）
//	2. 连续 3 次 ⇒ 账号熔断（`applyErrorPolicy` 的 ErrServer 分支）
//	3. 再发请求 ⇒ **修复前**：本地直接 503 `no_healthy_account`
//	              （正是他看到的错误）
//	              **修复后**：请求仍被发出，用户拿到上游的真实错误

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/pool"
	"workbuddy2api/internal/upstream"
)

// always503Upstream 造一个"聊天端点永远 503"的假上游。
//
// 同时把它自己收到的聊天请求次数通过返回值暴露出来 ——
// **这是本测试的核心判据**：熔断期间请求有没有被真正发出去。
//
// 为什么不用 `sseAndConfigUpstream`：那个总是成功，造不出熔断。
func always503Upstream(t *testing.T) (*upstream.Client, *int64) {
	t.Helper()
	var hits int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		// 只统计聊天请求；配置/账单端点不计（它们不是本测试的关注点）
		if strings.Contains(r.URL.Path, "chat") {
			atomic.AddInt64(&hits, 1)
		}
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusServiceUnavailable)
		_, _ = w.Write([]byte(`{"code":50300,"msg":"fake upstream down"}`))
	}))
	t.Cleanup(srv.Close)

	u, err := url.Parse(srv.URL)
	if err != nil {
		t.Fatalf("解析假上游地址失败: %v", err)
	}
	c := upstream.New()
	// 全部端点指向假上游：真值只有一个，避免"某个端点漏改"。
	//
	// ⚠ 国际版是**单个** `BaseIntl`（chat/billing/签到/token 刷新都在
	// 同一域名下），而国服是分开的三个 —— 这个不对称是本文件头一次写错的地方
	//（我按国服的形状去设 `ChatBaseIntl`，它根本不存在）。
	c.ChatBaseCN = srv.URL
	c.BillingBaseCN = srv.URL
	c.WebBaseCN = srv.URL
	c.BaseIntl = srv.URL
	c.WebBaseIntl = srv.URL
	_ = u
	return c, &hits
}

// testPoolWith 造一个池子（复用既有辅助若存在，否则内部实现）。
//
// ⚠ 本函数只在既有测试文件没有同名辅助时才需要 —— 编译会立刻告诉我们。
func singleAccountHandler(t *testing.T, accounts ...*auth.Auth) (*Handler, *pool.Pool, *int64) {
	t.Helper()
	resetModelsCache()

	p := pool.New("")
	t.Cleanup(func() { p.Flush() })
	for _, a := range accounts {
		p.Add(a)
		p.SetCredits(a.UID, 1000)
	}
	p.SetMultiProduct(true, 0.3)

	up, hits := always503Upstream(t)
	h := NewHandler(Config{Pool: p, Upstream: up, MaxRotate: 1})
	return h, p, hits
}

// TestSingleAccountProductSurvivesBreakerE2E 端到端复现所有者的 503 现场。
//
// 断言核心：**熔断期间请求依然被发到上游**（而不是被本地拦下报
// "所有账号不可用"）。这样：
//
//   - 上游已恢复 → 用户拿到 200（不必干等 30 分钟）
//   - 上游仍故障 → 用户看到**真实的上游错误**
//
// 两种都不比"本地直接 503 + 误导性文案"差。
func TestSingleAccountProductSurvivesBreakerE2E(t *testing.T) {
	h, p, hits := singleAccountHandler(t,
		&auth.Auth{UID: "wb-only", AccessToken: "t", ExpiresAt: 9999999999})

	// 产品前缀锁死候选：只可能是 wb-only 这一个号
	const body = `{"model":"workbuddy:glm-5.3","messages":[{"role":"user","content":"hi"}],"stream":false}`

	// ---- 阶段一：连打 3 次，把账号打到熔断 ----
	//（MaxRotate=1 ⇒ 一次请求只试一个号，第 3 次失败即达 breakerThreshold=3）
	for i := 0; i < 3; i++ {
		_, respBody := postChat(t, h, body)
		if got := atomic.LoadInt64(hits); got != int64(i+1) {
			t.Fatalf("第 %d 次请求没打到上游（hits=%d）—— 账号本该健康可用。响应：%s",
				i+1, got, respBody)
		}
	}

	// 前置条件：账号**确实**进入熔断（否则本测试没有意义）
	st, ok := p.Status("wb-only")
	if !ok {
		t.Fatal("账号不在池中")
	}
	if !st.Cooling {
		t.Fatalf("前置条件不成立：账号未进入熔断期（fails=%d）——"+
			"说明错误分类没走到 ErrServer 分支，本测试失去意义", st.BreakerFails)
	}
	t.Logf("账号已熔断，恢复时刻 %s（这是所有者看到 503 的时刻）",
		st.BreakerUntil.Format(time.RFC3339))

	// ---- 阶段二：熔断期间再发一次 ----
	before := atomic.LoadInt64(hits)
	code, respBody := postChat(t, h, body)
	after := atomic.LoadInt64(hits)

	// ★ 核心断言：请求**仍被发到上游**
	if after == before {
		var resp map[string]any
		_ = json.Unmarshal([]byte(respBody), &resp)
		t.Fatalf(
			"熔断期间请求**没有被发到上游**（hits 仍为 %d）⇒ 用户看到本地生成的 503。\n"+
				"HTTP %d，响应：%s\n\n"+
				"单账号产品没有别的号可换，熔断在这里只是把一次**上游瞬时故障**\n"+
				"放大成 30 分钟的**全量停服** —— 这正是所有者报告的现场：\n"+
				"  {\"code\":\"no_healthy_account\",\n"+
				"   \"message\":\"all accounts unavailable (cooling/disabled)\"}\n\n"+
				"正确行为：退到**同产品内部**的冷却兜底，让请求去试一次。\n"+
				"上游已恢复则成功；仍故障则用户看到**真实的上游错误**。",
			after, code, respBody)
	}

	// 状态码应当是上游的 5xx（真实故障），而不是本地编的"账号不可用"
	if code < 500 {
		t.Errorf("期望透出上游的 5xx（真实故障），实际 HTTP %d：%s", code, respBody)
	}
	t.Logf("熔断期间仍成功打到上游：hits %d → %d，返回 HTTP %d",
		before, after, code)
}

// TestBreakerStillProtectsMultiAccountProductE2E 多账号产品的熔断保护**不受影响**。
//
// 回归保护：只要该产品还有健康账号，请求就必须**换到那个号**。
// 否则本修复会把"熔断保护"整个废掉（坏号被反复选中）。
func TestBreakerStillProtectsMultiAccountProductE2E(t *testing.T) {
	resetModelsCache()

	// 这次用**成功**的上游（两个账号都健康时应当 200）
	up := sseAndConfigUpstream(t,
		configBodyWithMultiplier("glm-5.3", "x0.03"),
		configBodyWithMultiplier("glm-5.3", "x0.00"))

	p := pool.New("")
	t.Cleanup(func() { p.Flush() })
	for _, uid := range []string{"hb-1", "hb-2"} {
		p.Add(&auth.Auth{UID: uid, AccessToken: "t-" + uid, ExpiresAt: 9999999999})
		p.SetCredits(uid, 1000)
	}
	p.SetMultiProduct(true, 0.3)
	h := NewHandler(Config{Pool: p, Upstream: up, MaxRotate: 1})

	// 把 hb-1 打到熔断
	for i := 0; i < 3; i++ {
		p.NoteError("hb-1")
	}
	st1, _ := p.Status("hb-1")
	if !st1.Cooling {
		t.Fatal("前置条件不成立：hb-1 未熔断")
	}

	// 反复发请求：**必须**每次都成功（选到健康的 hb-2）
	const body = `{"model":"workbuddy:glm-5.3","messages":[{"role":"user","content":"hi"}],"stream":false}`
	for i := 0; i < 20; i++ {
		code, respBody := postChat(t, h, body)
		if code != http.StatusOK {
			t.Fatalf(
				"第 %d 次请求 HTTP %d（期望 200）—— 有健康账号时**不该**走兜底路径。\n"+
					"否则多账号产品的熔断保护会失效（坏号被反复选中）。\n响应：%s",
				i, code, respBody)
		}
	}
}
