package pool

// 账号「禁用」的两套语义必须在选号与养号之间正确分工。
//
// 所有者报过的真实缺陷：6 个被手动禁用的账号**完全不参与任何养号任务**
// （签到 / 活跃上报 / 成长任务 / 猫猫旅行全跳过）。根因是宿主「禁用就不导出
// 凭证」，而网关的账号池靠扫描凭证目录建立 —— 池里没有它，任务自然遍历不到。
//
// 修正后的语义：
//
//	用户手动禁用（auth.NoRoute）→ **只不接流量**；凭证照常导出，养号任务照跑
//	网关自判定（entry.disabled）→ session 死 / 额度冻结；任务也跳过多余
//
// 本文件锁住「选号侧确实排除了 NoRoute」这一半。
// 另一半（任务侧不跳 NoRoute）由 scheduler 的用例覆盖。

import (
	"testing"
	"time"

	"workbuddy2api/internal/auth"
)

// TestNoRouteAccountIsNeverPicked 标记 no_route 的账号**永不参与选号**。
func TestNoRouteAccountIsNeverPicked(t *testing.T) {
	p := New("")
	p.SetRandomSource(func(n int64) int64 { return 0 })

	normal := &auth.Auth{UID: "u-normal", AccessToken: "t1", ExpiresAt: 9999999999}
	blocked := &auth.Auth{UID: "u-blocked", AccessToken: "t2", ExpiresAt: 9999999999, NoRoute: true}
	p.Add(normal)
	p.Add(blocked)
	p.SetCredits(normal.UID, 1000)
	// 给被禁用的账号**更高的积分**：若过滤没生效，它一定会被选中
	//（testPoolWith 的确定性随机源恒定选积分最高者），于是用例能真正抓到回归。
	p.SetCredits(blocked.UID, 999999)

	for i := 0; i < 20; i++ {
		got := p.Pick()
		if got == nil {
			t.Fatal("应该能选到未禁用的账号")
		}
		if got.UID == blocked.UID {
			t.Fatalf("no_route 账号被选中了（第 %d 次）—— 禁用没有生效在选号上", i+1)
		}
	}
}

// TestNoRouteAccountStaysInStatusList 被禁用的账号**仍出现在状态列表里**。
//
// 为什么重要：界面要靠这个列表把「禁用了但仍在养号」显示出来。
// 若它从列表里消失，用户就看不到这个账号的任何养号进展 ——
// 这正是缺陷期间的表现（账号页里那些号像是凭空消失了）。
func TestNoRouteAccountStaysInStatusList(t *testing.T) {
	p := New("")
	a := &auth.Auth{UID: "u-blocked", AccessToken: "t", ExpiresAt: 9999999999, NoRoute: true}
	p.Add(a)

	list := p.List()
	if len(list) != 1 {
		t.Fatalf("账号应仍在状态列表里，实际 %d 条", len(list))
	}
	st := list[0]
	if !st.NoRoute {
		t.Error("状态里应带 no_route 标记 —— 界面靠它区分「用户禁用」与「网关判定死亡」")
	}
	if st.Disabled {
		t.Error("no_route 不该被当成 disabled：后者意味着养号任务也会跳过")
	}
}

// TestNoRouteExcludedFromTierDay no_route 账号不参与到期档位判定。
//
// 档位决定「先烧谁」，若把一个不接流量的账号算进去，
// 它可能独占最早档位，导致真正能接流量的账号被排到后面。
func TestNoRouteExcludedFromTierDay(t *testing.T) {
	p := New("")
	now := time.Now()

	blocked := &auth.Auth{UID: "u-blocked", AccessToken: "t1", ExpiresAt: 9999999999, NoRoute: true}
	usable := &auth.Auth{UID: "u-usable", AccessToken: "t2", ExpiresAt: 9999999999}
	p.Add(blocked)
	p.Add(usable)
	// 被禁用的账号到期更早：若算进档位，它会成为唯一的最早档
	p.SetExpiry(blocked.UID, now.Add(1*time.Hour).Unix())
	p.SetExpiry(usable.UID, now.Add(72*time.Hour).Unix())

	p.mu.Lock()
	day := p.routedTierDayLocked(now)
	// 直接读池内条目的到期日来算期望值（SetExpiry 写的是池内部字段，
	// 不写回 auth.SoonestExpireAt —— 用后者算期望会得到 1970）。
	wantEntry := p.byUID[usable.UID]
	p.mu.Unlock()

	want := wantEntry.expiryDayKey()
	if want == "" {
		t.Fatal("夹具没设上到期日，用例无意义")
	}
	if day != want {
		t.Errorf("档位应取可用账号的到期日 %s，实际 %q —— no_route 账号不该参与档位判定", want, day)
	}
}

// TestNoRouteCountedAsUnavailableNotCooling no_route 账号计入「禁用」而不是「冷却」。
//
// 为什么要有这条：healthy() 对 no_route 返回 false，若不单独分一支，
// 它就会落进 `case !e.healthy(now): cooling++`，于是界面上「冷却 N 个」
// 凭空多出几个根本没在冷却的账号 —— 用户会去找一个不存在的限流问题。
func TestNoRouteCountedAsUnavailableNotCooling(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "u-ok", AccessToken: "t1", ExpiresAt: 9999999999})
	p.Add(&auth.Auth{UID: "u-nr", AccessToken: "t2", ExpiresAt: 9999999999, NoRoute: true})

	total, healthy, cooling, disabled, _ := p.CountsDetailed()

	if total != 2 {
		t.Errorf("total=%d want 2", total)
	}
	if healthy != 1 {
		t.Errorf("healthy=%d want 1（只有未标记的那个可用）", healthy)
	}
	if cooling != 0 {
		t.Errorf("cooling=%d want 0 —— no_route 不是冷却，算进来会让界面报出不存在的限流", cooling)
	}
	if disabled != 1 {
		t.Errorf("disabled=%d want 1（no_route 与 disabled 同归「不可用」）", disabled)
	}
}
//
// 契约的另一头在宿主（Rust）侧：它把禁用账号的 no_route 写进凭证。
// 若这里读不到，整个「禁用但照常养号」就不成立 —— 账号会照旧接流量。
func TestNoRouteParsedFromCredential(t *testing.T) {
	nested := []byte(`{"auth":{"accessToken":"at","refreshToken":"rt","expiresAt":9999999999,"domain":"copilot.tencent.com"},
		"account":{"uid":"u1","nickname":"n","no_route":true}}`)
	a, err := auth.Parse(nested)
	if err != nil {
		t.Fatalf("解析失败: %v", err)
	}
	if !a.NoRoute {
		t.Error("嵌套形状的 no_route 未被解析")
	}

	// 扁平形状同样要支持（两种写法的字段语义必须一致，
	// 否则同一个账号换种写法就会「突然开始接流量」）。
	flat := []byte(`{"accessToken":"at","uid":"u2","no_route":true}`)
	b, err := auth.Parse(flat)
	if err != nil {
		t.Fatalf("扁平形状解析失败: %v", err)
	}
	if !b.NoRoute {
		t.Error("扁平形状的 no_route 未被解析")
	}

	// 未标记时为 false（不能因为字段存在就默认 true，那会把所有账号踢出路由）。
	plain := []byte(`{"accessToken":"at","uid":"u3"}`)
	c, err := auth.Parse(plain)
	if err != nil {
		t.Fatalf("解析失败: %v", err)
	}
	if c.NoRoute {
		t.Error("未标记 no_route 的账号不该被排除出路由")
	}
}
