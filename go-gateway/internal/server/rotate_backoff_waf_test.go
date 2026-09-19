package server

// rotate_backoff_waf_test.go 把「退避」与「WAF IP 判定」接到 forward.go 轮转循环
// 之后的**接线断言**。
//
// 纯函数单测（backoff_test.go / wafip_test.go）只能证明那两个模块自己是对的；
// 本文件证明它们**真的在轮转里生效** —— 缺陷报告里的两处问题都是「模块缺失
// 或未接线」，只测模块会漏掉「写了但没调用」。

import (
	"context"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/upstream"
)

// chatReq 造一个走完整 HTTP 入口的 chat/completions 请求。
func chatReq(body string) (*http.Request, *httptest.ResponseRecorder) {
	return httptest.NewRequest("POST", "/v1/chat/completions", strings.NewReader(body)),
		httptest.NewRecorder()
}

// resetWafIP 隔离用例之间的进程级 WAF 窗口状态。
//
// wafIP 是包级变量（出口 IP 属于整个进程，见 wafip.go），因此用例必须
// 自己清理 —— 否则前一个用例留下的 403 会让后一个用例的第一次调用
// 就返回「已判定 IP 被拦」。
func resetWafIP(t *testing.T) {
	t.Helper()
	wafIP.reset()
	t.Cleanup(func() { wafIP.reset() })
}

// TestRotateBackoffWaitsBetweenAccounts 断言换号前**真的**等待了退避时长。
//
// 这是本缺陷的核心断言：修复前两个账号之间的间隔是零（毫秒级），
// 上游看到的是「同一来源瞬间打了一串账号」—— 正是触发 WAF 的行为。
//
// 用「第二个账号被调用的时刻 - 第一个账号被调用的时刻」来断言，
// 而不是断言内部调了 sleep：后者在 sleep 被短路时仍会通过。
func TestRotateBackoffWaitsBetweenAccounts(t *testing.T) {
	resetWafIP(t)
	// 注入取下界的随机源：500ms 基准 → 375ms 下界。断言用它作为「至少等了」的门槛。
	withRotateRand(t, func(n int64) int64 { return 0 })

	var firstAt, secondAt time.Time
	calls := 0
	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		calls++
		if calls == 1 {
			firstAt = time.Now()
			return 402, `{"code":1,"msg":"余额不足"}`, false
		}
		secondAt = time.Now()
		return 200, sseOK, true
	})
	p := testPoolWith(
		&auth.Auth{UID: "bad", AccessToken: "at-bad", ExpiresAt: 9999999999},
		&auth.Auth{UID: "good", AccessToken: "at-good", ExpiresAt: 9999999999},
	)
	p.SetCredits("bad", 2000) // 让 bad 先被选中
	p.SetCredits("good", 1000)
	h := NewHandler(Config{Pool: p, Upstream: up, MaxRotate: 3})

	req, rec := chatReq(`{"model":"glm-5.2","messages":[]}`)
	h.ServeHTTP(rec, req)

	if rec.Code != http.StatusOK {
		t.Fatalf("应换号成功，code=%d body=%s", rec.Code, rec.Body)
	}
	if secondAt.IsZero() {
		t.Fatal("第二个账号未被调用，换号没发生")
	}
	if gap := secondAt.Sub(firstAt); gap < 375*time.Millisecond {
		t.Errorf("两次尝试的间隔 = %s，应 >= 375ms（下界）—— 零延迟连打正是本缺陷", gap)
	}
}

// TestRotateBackoffFirstAttemptNotDelayed 断言**第一次尝试不退避**。
//
// 正常请求的第一发不该被拖慢：用户没做错任何事，凭什么多等半秒。
// 这条也是「退避写在失败之后」这个设计的直接体现。
func TestRotateBackoffFirstAttemptNotDelayed(t *testing.T) {
	resetWafIP(t)
	// 注入取**上界**的源：若实现错误地在第一次尝试前也退避，
	// 这里的延迟会最明显（625ms），断言更容易抓到。
	withRotateRand(t, func(n int64) int64 { return n - 1 })

	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		return 200, sseOK, true
	})
	h := NewHandler(Config{
		Pool:      testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999}),
		Upstream:  up,
		MaxRotate: 3,
	})

	start := time.Now()
	req, rec := chatReq(`{"model":"glm-5.2","messages":[]}`)
	h.ServeHTTP(rec, req)
	elapsed := time.Since(start)

	if rec.Code != http.StatusOK {
		t.Fatalf("code=%d body=%s", rec.Code, rec.Body)
	}
	if elapsed > 300*time.Millisecond {
		t.Errorf("第一次尝试耗时 %s —— 正常请求的第一发不该退避", elapsed)
	}
}

// TestRotateBackoffCanceledByContext 断言退避**可被 ctx 取消**。
//
// 客户端断开后不该还在为一个没人要的请求空等（一次断连白占一个 goroutine
// 数秒，轮转越靠后浪费越大）。这里直接调 forwardChatCtx 传一个已取消的 ctx。
func TestRotateBackoffCanceledByContext(t *testing.T) {
	resetWafIP(t)
	withRotateRand(t, func(n int64) int64 { return n - 1 }) // 最长退避，最能暴露「没被取消」

	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		return 402, `{"code":1,"msg":"余额不足"}`, false
	})
	p := testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999},
		&auth.Auth{UID: "u2", AccessToken: "at2", ExpiresAt: 9999999999},
	)
	h := NewHandler(Config{Pool: p, Upstream: up, MaxRotate: 3})

	ctx, cancel := context.WithCancel(context.Background())
	cancel() // 请求的发起方已经走了

	start := time.Now()
	_, _, err := h.forwardChatCtx(ctx, []byte(`{"model":"glm-5.2","messages":[]}`), false, "")
	elapsed := time.Since(start)

	if err == nil {
		t.Fatal("ctx 已取消时换号退避应中止并报错")
	}
	if elapsed > time.Second {
		t.Errorf("耗时 %s —— ctx 已取消却还在退避空等", elapsed)
	}
	// 文案要能看出是「退避时被取消」，而不是笼统的账号不可用。
	if !strings.Contains(err.Error(), "中止换号") {
		t.Errorf("错误文案应说明换号被中止，实际 %q", err.Error())
	}
}

// TestWafIPBlockedStopsRotation 多个不同账号 403 时**立即终止轮转**。
//
// 这是缺陷 2 的核心断言。修复前：所有号轮一遍全 403，网关还会继续换号，
// 把请求放大 MaxRotate 倍 —— 恰好是 WAF 最想惩罚的行为。
//
// 断言四件事：
//  1. 上游只被打了 2 次（= 阈值），不是 MaxRotate 次 —— 没有放大；
//  2. 错误码是 egress_ip_blocked（不是 no_healthy_account），文案说明「换号无用」；
//  3. **账号没有被禁用**（账号是好的，问题在网络出口）；
//  4. 账号也没有被冷却（连软冷却都不该有）。
func TestWafIPBlockedStopsRotation(t *testing.T) {
	resetWafIP(t)
	withRotateRand(t, func(n int64) int64 { return 0 })

	calls := 0
	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		calls++
		// 无业务信封的 403：典型的边缘拦截页。
		return 403, `<html><body>403 Forbidden</body></html>`, false
	})
	p := testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999},
		&auth.Auth{UID: "u2", AccessToken: "at2", ExpiresAt: 9999999999},
		&auth.Auth{UID: "u3", AccessToken: "at3", ExpiresAt: 9999999999},
	)
	p.SetCredits("u1", 3000)
	p.SetCredits("u2", 2000)
	p.SetCredits("u3", 1000)
	h := NewHandler(Config{Pool: p, Upstream: up, MaxRotate: 3, SoftCooldown: time.Minute})

	req, rec := chatReq(`{"model":"glm-5.2","messages":[]}`)
	h.ServeHTTP(rec, req)

	if calls != 2 {
		t.Errorf("上游被打了 %d 次，期望 2 次（跨过 2 个不同账号的阈值即停）—— 继续打就是在放大风控", calls)
	}
	if rec.Code != http.StatusServiceUnavailable {
		t.Errorf("code=%d，期望 503", rec.Code)
	}
	body := rec.Body.String()
	if !strings.Contains(body, "egress_ip_blocked") {
		t.Errorf("应回 egress_ip_blocked 错误码（不能是 no_healthy_account），body=%s", body)
	}
	if !strings.Contains(body, "不同账号") || !strings.Contains(body, "换号无用") {
		t.Errorf("文案应说明「不同账号」与「换号无用」，body=%s", body)
	}

	// ⚠ 账号是好的：绝不能因为 IP 被拦而禁用或冷却任何账号。
	for _, uid := range []string{"u1", "u2", "u3"} {
		st, ok := p.Status(uid)
		if !ok {
			t.Fatalf("账号 %s 不在池里", uid)
		}
		if st.Disabled {
			t.Errorf("账号 %s 被禁用了 —— 被拦的是出口 IP，账号本身是好的", uid)
		}
		if st.Cooling {
			t.Errorf("账号 %s 被冷却了（%s）—— 被拦的是出口 IP，账号本身是好的", uid, st.CoolKind)
		}
	}
}

// TestWafBusinessForbiddenStillRotates 业务 403（带信封）仍按原语义换号。
//
// 关键回归保护：本特性只该拦「边缘 WAF 的 403」。上游业务拒绝
// （如 ZCode 的 `403 {"code":3101,"msg":"coding plan is required"}`）
// 是该账号/该请求自己的事，必须保持改动前的行为（继续换号），
// 否则一个缺套餐的账号就能让整个网关停止轮转。
func TestWafBusinessForbiddenStillRotates(t *testing.T) {
	resetWafIP(t)
	withRotateRand(t, func(n int64) int64 { return 0 })

	calls := 0
	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		calls++
		if authz == "Bearer at1" {
			return 403, `{"code":3101,"msg":"coding plan is required"}`, false
		}
		return 200, sseOK, true
	})
	p := testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999},
		&auth.Auth{UID: "u2", AccessToken: "at2", ExpiresAt: 9999999999},
	)
	p.SetCredits("u1", 2000)
	p.SetCredits("u2", 1000)
	h := NewHandler(Config{Pool: p, Upstream: up, MaxRotate: 3})

	req, rec := chatReq(`{"model":"glm-5.2","messages":[]}`)
	h.ServeHTTP(rec, req)

	if rec.Code != http.StatusOK {
		t.Fatalf("业务 403 应继续换号并成功，code=%d body=%s", rec.Code, rec.Body)
	}
	if calls != 2 {
		t.Errorf("上游被打了 %d 次，期望 2 次（业务 403 只换号）", calls)
	}
	if strings.Contains(rec.Body.String(), "egress_ip_blocked") {
		t.Errorf("业务 403 不该被判成出口 IP 被拦，body=%s", rec.Body)
	}
	// 业务 403 不该被记进 IP 窗口。
	if got := wafIP.distinct(); got != 0 {
		t.Errorf("业务 403 不该进入 IP 窗口，实际窗口内 %d 个账号", got)
	}
}

// TestWafSingleAccountRepeatedForbiddenNotIPBlocked 同一账号反复 403 不判 IP 问题。
//
// 这是「不同账号」这条判据在接线层的验证：一个坏账号的 403 不该让
// 整个网关停止轮转（否则一个号就能瘫痪全部流量）。
func TestWafSingleAccountRepeatedForbiddenNotIPBlocked(t *testing.T) {
	resetWafIP(t)
	withRotateRand(t, func(n int64) int64 { return 0 })

	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		return 403, `<html>403 Forbidden</html>`, false
	})
	// 池里只有**一个**账号：它反复 403 也只是账号问题。
	p := testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999})
	h := NewHandler(Config{Pool: p, Upstream: up, MaxRotate: 3})

	req, rec := chatReq(`{"model":"glm-5.2","messages":[]}`)
	h.ServeHTTP(rec, req)

	if strings.Contains(rec.Body.String(), "egress_ip_blocked") {
		t.Errorf("同一账号的 403 不该判成出口 IP 被拦，body=%s", rec.Body)
	}
	if got := wafIP.distinct(); got != 1 {
		t.Errorf("窗口内不同账号数 = %d，期望 1", got)
	}
}

// TestRotateBackoffNotWaitedWhenNoAccountLeft 池里选不出号时**不白等**退避。
//
// 这条守住 rotateWait 的摆放位置：它必须紧贴上游调用，不能放在循环顶部。
// 放在顶部时，「池里选不出号 → break 返回 503」这条路径会先白等一次退避
// 再报「账号全部不可用」—— 用户平白多等半秒以上（MaxRotate 越大越久），
// 而他等到的仍然是一个失败。
//
// 场景：池里只有 1 个账号，它 402 失败后进硬冷却且已被 tried 标记，
// 第 2 轮 pickAccount 直接返回 nil → break。**没有任何上游调用**发生。
func TestRotateBackoffNotWaitedWhenNoAccountLeft(t *testing.T) {
	resetWafIP(t)
	// 注入取**上界**的源：若实现错误地在选不出号时也退避，延迟最明显（625ms+）。
	withRotateRand(t, func(n int64) int64 { return n - 1 })

	calls := 0
	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		calls++
		return 402, `{"code":1,"msg":"余额不足"}`, false
	})
	p := testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999})
	h := NewHandler(Config{Pool: p, Upstream: up, MaxRotate: 3})

	start := time.Now()
	req, rec := chatReq(`{"model":"glm-5.2","messages":[]}`)
	h.ServeHTTP(rec, req)
	elapsed := time.Since(start)

	if rec.Code == http.StatusOK {
		t.Fatalf("不该成功（只有一个账号且已失败），body=%s", rec.Body)
	}
	if calls != 1 {
		t.Fatalf("上游被打了 %d 次，期望 1 次（第 2 轮选不出号，不该再打）", calls)
	}
	// 选不出号的那一轮**没有上游调用**，因此不该有任何退避。
	if elapsed > 300*time.Millisecond {
		t.Errorf("耗时 %s —— 选不出号时仍在退避空等，用户白等了一次失败", elapsed)
	}
}

// TestForwardChatWrapperStillWorks 断言 forwardChat 便捷包装仍可用。
//
// 既有调用点与大量单测用 forwardChat（不传 ctx），这条保证它们没被破坏。
func TestForwardChatWrapperStillWorks(t *testing.T) {
	resetWafIP(t)
	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		return 200, sseOK, true
	})
	h := NewHandler(Config{
		Pool:     testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999}),
		Upstream: up,
	})
	res, _, err := h.forwardChat([]byte(`{"model":"glm-5.2","messages":[]}`), true, "")
	if err != nil {
		t.Fatalf("forwardChat 失败: %v", err)
	}
	if res == nil || res.UID != "u1" {
		t.Fatalf("res=%+v", res)
	}
	res.Stream.Close()
	h.release(res.UID)
}

// TestWafForbiddenKindIsClientError 确认 WAF 403 的分类前提仍然成立。
//
// upstream.Classify 对 403 的结论是 ErrClient，而 applyErrorPolicy 对
// ErrClient 是「只换号不罚」—— 所以即使不走新路径，账号也不会被冷却。
// 这条用例把该前提钉住：若将来 Classify 把 403 改成会罚账号的 kind，
// 本模块「不动账号状态」的设计就需要重新审视。
func TestWafForbiddenKindIsClientError(t *testing.T) {
	if k := upstream.Classify(403, `<html>403</html>`); k != upstream.ErrClient {
		t.Errorf("Classify(403) = %v，期望 ErrClient —— 若它变成会罚账号的 kind，"+
			"WAF 403 路径「不动账号状态」的前提就不成立了", k)
	}
}
