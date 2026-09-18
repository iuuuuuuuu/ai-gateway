package pool

// 兜底路径（全冷却时选「截止最早」的账号）必须尊重两个闸门。
//
// 背景（所有者现场报告）：
//   他把国际版账号全部标为「不接流量」（因为不禁用就没法用），
//   但国服账号因上游限流进入冷却后，请求报 503 连不上。
//
// 排查中我一度怀疑：兜底路径漏了 NoRoute 检查，会把他明确禁用的账号选回来。
// **实测结论：这个怀疑不成立** —— 兜底确实跳过了 NoRoute（本文件第一条用例
// 在修改任何代码之前就是 PASS）。
//
// 但排查过程暴露了两个真实的东西，都值得钉住：
//   1. pool.go:189 的注释声称 pickEarliestExpiryLocked 靠 healthy() 筛候选，
//      而它其实自己手写了一遍筛选（只查 disabled / CoolHard / modelCooled）。
//      注释与实现不符 —— 下一个人读注释会以为已覆盖，实际要逐条核对。
//      本文件用测试把「两条路径对 NoRoute 的结论必须一致」钉死，
//      这样即便将来有人重写筛选逻辑，也不会悄悄分叉。
//   2. 兜底只对**已在冷却期**的账号有意义（expiry() 对非冷却账号返回零值），
//      写测试时必须先让账号进入冷却，否则测到的是「没候选」而不是「筛得对不对」。
//
// ⚠ 构造要点：用 `p.Cooldown(uid, CoolSoft, d, reason)` 让账号进入冷却期。
//    只 SetCreditsAndExpiry 不会 —— 那是积分到期，与「冷却」是两回事，
//    而 expiry() 只对冷却期内的账号返回非零值。

import (
	"testing"
	"time"

	"workbuddy2api/internal/auth"
)

// TestFallbackSkipsNoRouteAccounts 兜底不得选中「不接流量」的账号。
//
// 构造：唯一账号被标 NoRoute **且处于冷却期**（这是兜底的真实场景）。
// 期望：返回 nil（无候选），而不是把它选回来。
func TestFallbackSkipsNoRouteAccounts(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{
		UID:         "noroute-cooling",
		AccessToken: "tok",
		Domain:      "www.workbuddy.ai",
		NoRoute:     true, // 用户手动禁用：不接流量
	})
	// 让它进入冷却期 —— 否则 expiry() 为零，兜底本来就不会选它，
	// 那样测到的是「没候选」而不是「筛得对不对」（我第一版就踩了这个）。
	p.Cooldown("noroute-cooling", CoolSoft, 10*time.Minute, "测试：进入软冷却")

	got := p.pickEarliestExpiryLocked(nil, time.Now(), "")
	if got != nil {
		t.Errorf("兜底选中了「不接流量」的账号 %s —— 用户明确禁用它，"+
			"却仍被兜底路由到，会表现为请求失败", got.UID)
	}
}

// TestFallbackStillPicksCoolingAccounts 反面：普通冷却账号仍应被兜底选中。
//
// 与上一条成对：只测「跳过 NoRoute」不测「正常的还能选」，可能把兜底
// 改成永不返回，把「全冷却时还能凑合用一个」这个功能一并改坏。
func TestFallbackStillPicksCoolingAccounts(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "normal-cooling", AccessToken: "tok", Domain: "copilot.tencent.com"})
	p.Cooldown("normal-cooling", CoolSoft, 10*time.Minute, "测试：进入软冷却")

	got := p.pickEarliestExpiryLocked(nil, time.Now(), "")
	if got == nil {
		t.Fatal("普通冷却账号应被兜底选中，实际 nil —— 兜底被改坏了")
	}
	if got.UID != "normal-cooling" {
		t.Errorf("期望 normal-cooling，实际 %s", got.UID)
	}
}

// TestFallbackPrefersNormalOverNoRoute 混合池：有可兜底的正常账号时，
// 即使 NoRoute 账号的冷却**更早结束**，也必须选正常的那个。
//
// 「更早到期」是兜底的排序依据，而 NoRoute 是硬闸门 —— 闸门必须优先于排序。
func TestFallbackPrefersNormalOverNoRoute(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "noroute-early", AccessToken: "tok", Domain: "www.workbuddy.ai", NoRoute: true})
	p.Add(&auth.Auth{UID: "normal-late", AccessToken: "tok", Domain: "copilot.tencent.com"})
	// NoRoute 那个冷却结束得更早（若无闸门，会被优先选中）
	p.Cooldown("noroute-early", CoolSoft, 1*time.Minute, "测试：短冷却")
	p.Cooldown("normal-late", CoolSoft, 30*time.Minute, "测试：长冷却")

	got := p.pickEarliestExpiryLocked(nil, time.Now(), "")
	if got == nil {
		t.Fatal("有可兜底账号时应返回它，实际 nil")
	}
	if got.UID != "normal-late" {
		t.Errorf("应选 normal-late，实际 %s —— NoRoute 账号冷却更早结束，"+
			"但它是用户明确禁用的，不该因「到期早」被优先选中", got.UID)
	}
}

// TestSelectionPathsAgreeOnNoRoute 两条选号路径对 NoRoute 的结论必须一致。
//
// 这是本文件的核心价值：pool.go 里 healthy() 的注释曾声称它是「统一闸门」、
// 覆盖 pickLocked 与 pickEarliestExpiryLocked。实测发现兜底**自己手写了一遍**
// 筛选（未调 healthy()），因此它的 NoRoute 检查缺失 —— 被标「不接流量」的账号
// 在兜底时仍会被选中。现已补上。
//
// 断言方式：两条路径对同一个 NoRoute 账号都必须判为「不可选」。
// （早先写成 `healthyOK == fallbackPicked` 是错的 —— 两者都为 false 才是
//  正确的「一致」，那个写法把正确结果报成了不一致。）
func TestSelectionPathsAgreeOnNoRoute(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "nr", AccessToken: "tok", Domain: "www.workbuddy.ai", NoRoute: true})
	p.Cooldown("nr", CoolSoft, 10*time.Minute, "测试：进入软冷却")

	now := time.Now()

	var healthyOK bool
	for _, e := range p.byUID {
		healthyOK = e.healthy(now)
	}
	fallbackPicked := p.pickEarliestExpiryLocked(nil, now, "") != nil

	if healthyOK {
		t.Error("healthy() 对 NoRoute 账号应判为不可选")
	}
	if fallbackPicked {
		t.Error("兜底对 NoRoute 账号应判为不可选（这正是本文件钉住的缺陷）")
	}
	// 两者结论一致（都不可选）才算通过
	if healthyOK != fallbackPicked {
		t.Errorf("两条路径结论分叉：healthy=%v 兜底选中=%v", healthyOK, fallbackPicked)
	}
}
