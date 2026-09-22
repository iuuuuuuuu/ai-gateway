package pool

// single_account_breaker_test.go —— 钉住「单账号产品被熔断后彻底不可用」这个缺陷。
//
// # 所有者的现场（2026-09-22）
//
// 客户端固定用 `qoder:Qwen3.8-Flash`，突然全部 503：
//
//	{"code":"no_healthy_account",
//	 "message":"all accounts unavailable (cooling/disabled)"}
//
// 原文：
//
//	「我在qoder客户端发送没问题,我都没重登」
//	「重启后也还是在报错503,你就不能自己跑一下实测吗?非要让我测试」
//
// # 根因链
//
//	1. qoder 只有 **1 个**账号
//	2. 它连续吃到 3 次上游 503（`applyErrorPolicy` 的 ErrServer 分支
//	   → `NoteError` → 达 breakerThreshold=3）⇒ 熔断 **30 分钟**
//	3. 产品前缀请求只能选该产品的账号（`PickForModelProductRegion` 的
//	   排除集锁死了 product）
//	4. 而 `pickStrict` **刻意不兜底** ⇒ 选不出 ⇒ 503
//
// ⇒ 整个产品**彻底不可用 30 分钟**，且熔断不持久化、没有任何手动清除入口
//（`ReviveDisabled` 明确写着"不清熔断"）。
//
// # 关键事实：账号其实是好的
//
// 用一个独立网关实例（同一份配置）实测 `qoder:Qwen3.8-Flash` → **200 OK**。
// 说明那 3 次 503 是**上游瞬时故障**，账号本身健康 ——
// 熔断把一次瞬时故障放大成了 30 分钟停服。
//
// # 修法
//
// `PickForModelProductRegion` 在"该产品一个健康账号都没有"时，
// 退到**同产品内部**的冷却兜底（`pickEarliestExpiryLocked`）。
//
// 为什么不违反 `pickStrict` 的"不兜底"原则：那条原则是为了避免
// **跨产品/跨区域**降级（会拿错凭证、或让图片被换占位符）。
// 而这里的排除集已经锁死了产品，兜底只在同一产品内选。

import (
	"testing"
	"time"

	"workbuddy2api/internal/auth"
)

// singleQoderFixture 复现所有者配置：qoder 1 个账号 + workbuddy 19 个。
func singleQoderFixture(t *testing.T) *Pool {
	t.Helper()
	p := New("")
	t.Cleanup(func() { p.Flush() })

	for i := 0; i < 19; i++ {
		uid := "wb-" + string(rune('a'+i))
		p.Add(&auth.Auth{UID: uid, AccessToken: "t-" + uid, ExpiresAt: 9999999999})
		p.SetCredits(uid, int64(1000+i))
	}
	p.Add(&auth.Auth{UID: "qd-1", Product: auth.ProductQoder, AccessToken: "t-qd", ExpiresAt: 9999999999})
	p.SetCredits("qd-1", 500)

	p.SetMultiProduct(true, 0.3)
	p.SetProductModels(map[string][]string{
		"qoder": {"Qwen3.8-Flash", "GLM-5.3"},
	})
	return p
}

// TestSingleAccountProductSurvivesBreaker ★ 单账号产品被熔断后仍能选出一个号。
//
// 这是本轮修复的核心断言：`qoder:` 前缀请求在 qoder 账号熔断期间
// **不该**直接 503 —— 因为没有别的号可换，熔断只会把一次上游瞬时
// 故障放大成 30 分钟停服。
func TestSingleAccountProductSurvivesBreaker(t *testing.T) {
	p := singleQoderFixture(t)

	// 模拟：连续 3 次上游 5xx ⇒ 熔断（与 applyErrorPolicy 的 ErrServer 分支一致）
	for i := 0; i < 3; i++ {
		p.NoteError("qd-1")
	}

	// 前置条件确认：账号**确实**处于熔断期（否则这条测试没有意义）
	st, _ := p.Status("qd-1")
	if !st.Cooling {
		t.Fatal("前置条件不成立：qd-1 未进入熔断期，本测试失去意义")
	}
	if st.BreakerUntil.IsZero() {
		t.Fatal("前置条件不成立：BreakerUntil 为零，说明不是熔断触发的冷却")
	}
	t.Logf("qd-1 已熔断，恢复时刻 %s", st.BreakerUntil.Format(time.RFC3339))

	// 核心断言：产品前缀请求仍能选出 qd-1
	acct := p.PickForModelProductRegion("Qwen3.8-Flash", nil, auth.RegionAny, auth.ProductQoder)
	if acct == nil {
		t.Fatal(
			"熔断期间 `qoder:` 请求选不出任何账号 ⇒ 用户看到 503 " +
				"「all accounts unavailable」并被迫干等 30 分钟。\n" +
				"qoder 只有 1 个账号，没有别的号可换 —— 熔断在这里" +
				"只是把一次上游瞬时故障（5xx）放大成全量停服。\n" +
				"正确行为：退到**同产品内部**的冷却兜底，让请求去试一次 —— " +
				"上游已恢复则成功，仍故障则用户看到**真实的上游错误**。")
	}
	if acct.UID != "qd-1" {
		t.Fatalf("应选到 qd-1，实际 %s（产品约束被兜底绕过了？）", acct.UID)
	}
}

// TestBreakerFallbackStaysInsideProduct 兜底**不得**跨产品。
//
// 这是本修复的安全边界：兜底只在同一产品内选，绝不能把 WorkBuddy
// 账号拿给 `qoder:` 请求用（那会拿错凭证、打到错的端点）。
func TestBreakerFallbackStaysInsideProduct(t *testing.T) {
	p := singleQoderFixture(t)
	for i := 0; i < 3; i++ {
		p.NoteError("qd-1")
	}

	// 反复挑多次，确保不会随机漂到 workbuddy
	for i := 0; i < 30; i++ {
		acct := p.PickForModelProductRegion("Qwen3.8-Flash", nil, auth.RegionAny, auth.ProductQoder)
		if acct == nil {
			t.Fatalf("第 %d 次选不出账号", i)
		}
		if acct.ProductOf() != auth.ProductQoder {
			t.Fatalf(
				"第 %d 次选到了 %s 账号（%s）—— 兜底**跨产品**了！\n"+
					"`qoder:` 前缀是用户的显式指令，绝不能降级到别的产品："+
					"那会拿 WorkBuddy 的凭证去打 qoder 的端点（或反之）。",
				i, acct.ProductOf(), acct.UID)
		}
	}
}

// TestHealthyProductStillUsesStrictPath 有健康账号时**仍走严格路径**。
//
// 回归保护：本修复只该在"该产品一个健康号都没有"时生效。
// 只要有一个健康的，多账号产品的熔断保护完全不受影响。
func TestHealthyProductStillUsesStrictPath(t *testing.T) {
	p := singleQoderFixture(t)
	// 再加一个健康的 qoder 账号
	p.Add(&auth.Auth{UID: "qd-2", Product: auth.ProductQoder, AccessToken: "t-qd2", ExpiresAt: 9999999999})
	p.SetCredits("qd-2", 400)

	// 只熔断 qd-1
	for i := 0; i < 3; i++ {
		p.NoteError("qd-1")
	}
	st, _ := p.Status("qd-1")
	if !st.Cooling {
		t.Fatal("前置条件不成立：qd-1 未熔断")
	}

	// 反复挑：**必须每次都选到健康的 qd-2**，不能漂到熔断中的 qd-1
	for i := 0; i < 30; i++ {
		acct := p.PickForModelProductRegion("Qwen3.8-Flash", nil, auth.RegionAny, auth.ProductQoder)
		if acct == nil {
			t.Fatalf("第 %d 次选不出账号", i)
		}
		if acct.UID != "qd-2" {
			t.Fatalf(
				"第 %d 次选到了 %s —— 有健康账号时**不该**走兜底路径。\n"+
					"否则多账号产品的熔断保护会失效（坏号会被反复选中）。",
				i, acct.UID)
		}
	}
}

// TestBreakerFallbackSkipsDisabledAndNoRoute 兜底仍要跳过禁用与「不接流量」。
//
// 兜底复用 `pickEarliestExpiryLocked`，它已正确跳过这两类 ——
// 本测试钉住这个复用关系，防止将来有人换成一个"更简单"的兜底而丢掉这些判据。
func TestBreakerFallbackSkipsDisabledAndNoRoute(t *testing.T) {
	p := singleQoderFixture(t)
	for i := 0; i < 3; i++ {
		p.NoteError("qd-1")
	}

	// 场景 A：账号被系统禁用 ⇒ 兜底也不该选它
	p.Disable("qd-1", "session dead")
	if acct := p.PickForModelProductRegion("Qwen3.8-Flash", nil, auth.RegionAny, auth.ProductQoder); acct != nil {
		t.Errorf("账号已被禁用，兜底仍选出了 %s —— 禁用的号永不参与兜底", acct.UID)
	}
	p.ReviveDisabled("qd-1")

	// 场景 B：用户标「不接流量」⇒ 兜底也不该选它
	//（现场教训：所有者把国际版全标 NoRoute，兜底又把它们选了回来 → 503）
	p.mu.Lock()
	p.byUID["qd-1"].a.NoRoute = true
	p.mu.Unlock()
	if acct := p.PickForModelProductRegion("Qwen3.8-Flash", nil, auth.RegionAny, auth.ProductQoder); acct != nil {
		t.Errorf("账号被标 NoRoute，兜底仍选出了 %s —— 用户的显式指令不得被兜底绕过", acct.UID)
	}
}
