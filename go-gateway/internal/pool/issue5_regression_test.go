package pool

import (
	"fmt"
	"testing"

	"workbuddy2api/internal/auth"
)

// issue #5 回归：14 个账号（同一天到期、权重完全相同）被均衡路由。
//
// 修复前：短名单按权重降序 + UID 升序截断到 top5，权重全相同时短名单恒为
// acct01..acct05，其余 9 个号永远进不来 —— 用户实测「14 个账号只用到 4~5 个」。
func TestIssue5_AllAccountsGetRouted(t *testing.T) {
	withNoPickGap(t)
	p := New("")
	const n = 14
	for i := 1; i <= n; i++ {
		p.Add(&auth.Auth{UID: fmt.Sprintf("acct%02d", i), SoonestExpireAt: expiryAt(7)})
	}

	const rounds = 3000
	counts := map[string]int{}
	for i := 0; i < rounds; i++ {
		got := p.Pick()
		if got == nil {
			t.Fatal("pick returned nil")
		}
		counts[got.UID]++
	}

	if len(counts) != n {
		t.Fatalf("只有 %d/%d 个账号被路由到（issue #5 未修复）: %v", len(counts), n, counts)
	}
	// 均衡性：权重相同的情况下，每个账号占比应接近 1/14≈7.1%，允许较宽裕的波动。
	ideal := rounds / n
	for uid, c := range counts {
		if c < ideal/2 || c > ideal*2 {
			t.Errorf("账号 %s 被选中 %d 次，偏离均衡值 %d 过多: %v", uid, c, ideal, counts)
		}
	}
}

// issue #5 回归：即使到期日各不相同（分层后可能只剩少数几档），也不应长期
// 只压榨同一个账号；最早档用尽/冷却后必须轮到下一档。
func TestIssue5_TierRotationReachesOtherAccounts(t *testing.T) {
	withNoPickGap(t)
	p := New("")
	// 每档 2 个账号，共 7 档（1..7 天后到期）。
	for tier := 1; tier <= 7; tier++ {
		for k := 0; k < 2; k++ {
			uid := fmt.Sprintf("t%d_%d", tier, k)
			p.Add(&auth.Auth{UID: uid, SoonestExpireAt: expiryAt(tier)})
		}
	}

	// 首轮必须落在最早档（t1_*）——「先烧快过期额度」的既定语义不能被破坏。
	first := p.Pick()
	if first == nil {
		t.Fatal("pick returned nil")
	}
	if first.UID != "t1_0" && first.UID != "t1_1" {
		t.Fatalf("首个选中账号 %s 不在最早到期档 t1_*", first.UID)
	}

	// 把最早档冷却掉（模拟额度烧完/429），流量必须自动轮到下一档。
	p.Cooldown("t1_0", CoolSoft, 3600*1e9, "烧完")
	p.Cooldown("t1_1", CoolSoft, 3600*1e9, "烧完")
	next := p.Pick()
	if next == nil {
		t.Fatal("pick returned nil after cooling earliest tier")
	}
	if next.UID != "t2_0" && next.UID != "t2_1" {
		t.Fatalf("最早档冷却后选中 %s，应轮到下一档 t2_*", next.UID)
	}
}

// issue #5 回归（最贴近用户实际形态）：14 个账号、开启真实的 minPickGap、
// 走请求级轮换入口 PickExcluding。修复前短名单恒为前 5 个 UID，
// 其余 9 个账号永远拿不到流量。
func TestIssue5_RotationWithRealPickGapUsesAllAccounts(t *testing.T) {
	// 故意不调用 withNoPickGap：保留生产默认的 100ms 防撞号窗口。
	p := New("")
	const n = 14
	for i := 1; i <= n; i++ {
		p.Add(&auth.Auth{UID: fmt.Sprintf("acct%02d", i), SoonestExpireAt: expiryAt(7)})
	}

	counts := map[string]int{}
	// 模拟 300 个请求，每个请求首轮即成功（真实场景主流路径）。
	for req := 0; req < 300; req++ {
		tried := map[string]bool{}
		a := p.PickExcluding(tried)
		if a == nil {
			t.Fatal("pick returned nil")
		}
		counts[a.UID]++
	}

	t.Logf("14 账号分布: %v", counts)
	if len(counts) != n {
		t.Fatalf("只有 %d/%d 个账号被路由到（issue #5 未修复）: %v", len(counts), n, counts)
	}
	// 真实 minPickGap 下应高度均衡：每个账号占比接近 1/14≈21 次。
	ideal := 300 / n
	for uid, c := range counts {
		if c < ideal/2 || c > ideal*2 {
			t.Errorf("账号 %s 被选中 %d 次，偏离均衡值 %d 过多: %v", uid, c, ideal, counts)
		}
	}
}

// issue #5 回归：同档（同一天到期）内按设计应「平均分摊」，credits 不再倾斜。
//
// 这里锁定的是既有设计意图而非 bug：同档内 credits 项被刻意去掉（tierWeightOf），
// 因为同一天到期意味着紧迫度相同，按积分加权会让高积分号长期吃掉流量。
// 该测试的作用是防止后续修复 issue #5 时误把 credits 倾斜引入同档。
func TestIssue5_SameTierIgnoresCredits(t *testing.T) {
	withNoPickGap(t)
	p := New("")
	const n = 14
	for i := 1; i <= n; i++ {
		uid := fmt.Sprintf("acct%02d", i)
		p.Add(&auth.Auth{UID: uid, SoonestExpireAt: expiryAt(7)})
	}
	// 一个账号积分极高，但同档内不应因此获得倾斜。
	p.SetCredits("acct01", 100000)
	for i := 2; i <= n; i++ {
		p.SetCredits(fmt.Sprintf("acct%02d", i), 1)
	}

	counts := map[string]int{}
	const rounds = 2800
	for i := 0; i < rounds; i++ {
		counts[p.Pick().UID]++
	}
	if len(counts) != n {
		t.Fatalf("只有 %d/%d 个账号被用到: %v", len(counts), n, counts)
	}
	ideal := rounds / n
	// 同档内应大致均分：每个账号占比不低于理想值的一半。
	if counts["acct01"] < ideal/2 {
		t.Errorf("高积分账号在同档内被过度压制: acct01=%d, ideal=%d", counts["acct01"], ideal)
	}
	for uid, c := range counts {
		if c < ideal/2 || c > ideal*2 {
			t.Errorf("账号 %s 被选中 %d 次，偏离同档均分值 %d 过多: %v", uid, c, ideal, counts)
		}
	}
}
