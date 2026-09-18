package pool

// 多产品路由的分布测试。
//
// ## 为什么必须测「分布」而不是「某次选中谁」
//
// 选号是加权随机，单次结果说明不了任何问题。C5 的验收要求是
// 「两产品都被路由到、且贵的一方份额更低」—— 这是**统计性质**，
// 必须跑足够样本测量份额。
//
// ## 为什么成本维度要做成开关
//
// 跨产品权重是最容易"悄悄改变现有行为"的地方，而现有单产品行为已在生产
// 环境跑了很久。开关关闭时（默认）不读 costRate、不加乘子，
// 所有既有路径**逐字不变** —— 这也是出问题时的回滚点。

import (
	"testing"
	"time"

	"workbuddy2api/internal/auth"
)

// mkProductAuth 造一个指定产品的账号。
func mkProductAuth(uid, product string) *auth.Auth {
	a := &auth.Auth{
		UID:         uid,
		Nickname:    uid,
		AccessToken: "token-" + uid,
		ExpiresAt:   time.Now().Add(24 * time.Hour).Unix(),
		Domain:      "copilot.tencent.com",
	}
	if product == auth.ProductQoder {
		a.Product = auth.ProductQoder
		// Qoder 的域名不同（区域判定也走这条）
		a.Domain = "qoder.sh"
	}
	return a
}

// newTestPool 造一个测试池并灌入账号（New("") = 不落盘）。
//
// ⚠ 必须把 minPickGap 置 0，否则**测不到加权路径**：
// 生产值 100ms 是为了防并发撞号（同一账号在该窗口内不重复被选中），
// 而测试在紧密循环里连跑数千次 Pick，2 个账号会立刻全部落入
// "刚被用过" 状态 → 走 LRU 兜底 → **权重被完全忽略**。
//
// 这个坑很隐蔽：份额会精确地 50/50（LRU 严格轮换），
// 看起来像"成本维度没生效"，实际是测试没跑到加权分支。
// 源码注释里写着「minPickGap=0（测试用）时过滤恒通过，退化为纯加权随机」。
func newTestPool(t *testing.T, auths ...*auth.Auth) *Pool {
	t.Helper()
	prev := minPickGap
	minPickGap = 0
	t.Cleanup(func() { minPickGap = prev })

	p := New("")
	for _, a := range auths {
		p.Add(a)
	}
	return p
}

// pickCounts 跑 n 次 Pick，统计每个 uid 被选中的次数。
func pickCounts(p *Pool, n int) map[string]int {
	counts := map[string]int{}
	for i := 0; i < n; i++ {
		if a := p.Pick(); a != nil {
			counts[a.UID]++
		}
	}
	return counts
}

// TestMultiProductOffIsIdenticalToSingleProduct 关闭开关时成本维度**完全不生效**。
//
// 这是"零漂移"的核心断言：同样两个账号、同样的 costRate 差异，
// 关闭开关时份额必须与"从没设过 costRate"一致。
// 若关闭时仍受成本影响，说明开关没真正切断 —— 那会悄悄改变生产行为。
func TestMultiProductOffIsIdenticalToSingleProduct(t *testing.T) {
	const rounds = 4000

	// 基线：不设 costRate
	p1 := newTestPool(t, mkProductAuth("a", auth.ProductWorkBuddy), mkProductAuth("b", auth.ProductWorkBuddy))
	p1.SetMultiProduct(false, 0)
	base := pickCounts(p1, rounds)

	// 对照组：设了极端 costRate，但开关**关闭**
	p2 := newTestPool(t, mkProductAuth("a", auth.ProductWorkBuddy), mkProductAuth("b", auth.ProductWorkBuddy))
	p2.SetMultiProduct(false, 0)
	p2.SetCostRate("a", 0.01)  // 极便宜
	p2.SetCostRate("b", 100.0) // 极贵
	got := pickCounts(p2, rounds)

	// 开关关闭时，两者份额应几乎相同（允许随机抖动）
	diff := absInt(base["a"]-got["a"]) + absInt(base["b"]-got["b"])
	if diff > rounds/10 {
		t.Errorf("关闭多产品开关后，costRate 仍影响了份额：基线 a=%d b=%d，设了成本后 a=%d b=%d（差异 %d，上限 %d）",
			base["a"], base["b"], got["a"], got["b"], diff, rounds/10)
	}
}

// TestMultiProductBothProductsSelected 两产品都必须被选中（不能饿死一方）。
func TestMultiProductBothProductsSelected(t *testing.T) {
	p := newTestPool(t,
		mkProductAuth("wb", auth.ProductWorkBuddy),
		mkProductAuth("qd", auth.ProductQoder),
	)
	p.SetMultiProduct(true, 0.3)

	counts := pickCounts(p, 2000)
	if counts["wb"] == 0 {
		t.Error("WorkBuddy 账号一次都没被选中 —— 产品维度把一方饿死了")
	}
	if counts["qd"] == 0 {
		t.Error("Qoder 账号一次都没被选中 —— 产品维度把一方饿死了")
	}
	t.Logf("份额：workbuddy=%d qoder=%d", counts["wb"], counts["qd"])
}

// TestCheaperProductGetsMoreShare 更便宜的一方份额更高。
//
// 这是 design.md §2.3 的核心验收：成本必须真正参与决策。
func TestCheaperProductGetsMoreShare(t *testing.T) {
	const rounds = 6000

	p := newTestPool(t,
		mkProductAuth("cheap", auth.ProductWorkBuddy),
		mkProductAuth("pricey", auth.ProductQoder),
	)
	p.SetMultiProduct(true, 0.3)
	p.SetCostRate("cheap", 0.2)
	p.SetCostRate("pricey", 5.0)

	counts := pickCounts(p, rounds)
	cheap, pricey := counts["cheap"], counts["pricey"]
	t.Logf("份额：cheap=%d(%.1f%%) pricey=%d(%.1f%%)",
		cheap, float64(cheap)*100/rounds, pricey, float64(pricey)*100/rounds)

	if cheap <= pricey {
		t.Errorf("更便宜的一方份额应更高，实际 cheap=%d pricey=%d", cheap, pricey)
	}
	// 差异必须**可观测**（不是统计噪声级别）
	if float64(cheap) < float64(pricey)*1.2 {
		t.Errorf("份额差异太小（cheap/pricey = %.2f）—— 成本维度形同虚设",
			float64(cheap)/float64(pricey))
	}
}

// TestUndeclaredCostIsNeutral 成本未声明时必须中性（份额不塌成 0）。
//
// 三态语义的守卫：若把 0 当成"很贵"或"很便宜"，都会让某产品被系统性冷落。
// 现实中很常见：某个产品还没查询到倍率表，此时绝不能因此丢掉它的流量。
func TestUndeclaredCostIsNeutral(t *testing.T) {
	const rounds = 4000

	p := newTestPool(t,
		mkProductAuth("declared", auth.ProductWorkBuddy),
		mkProductAuth("undeclared", auth.ProductQoder),
	)
	p.SetMultiProduct(true, 0.3)
	p.SetCostRate("declared", 1.0)
	// undeclared 故意不设 → 0 = 未声明

	counts := pickCounts(p, rounds)
	got := counts["undeclared"]
	share := float64(got) * 100 / rounds
	t.Logf("未声明成本的一方份额：%d(%.1f%%)", got, share)

	if got == 0 {
		t.Error("成本未声明的一方一次都没被选中 —— 0 被误当成极端值了")
	}
	if share < 35 || share > 65 {
		t.Errorf("成本未声明时应接近中性（35%%~65%%），实际 %.1f%%", share)
	}
}

// TestCostDoesNotOverrideExpiryTier 成本**不得**压过到期分层。
//
// 这是最重要的边界：到期分层关系到"一旦过期就净损失的额度"，
// 而成本只是微调。若便宜的账号下个月才到期、贵的明天就到期，
// 必须先烧贵的 —— 否则明天就损失掉那份额度。
func TestCostDoesNotOverrideExpiryTier(t *testing.T) {
	const rounds = 3000

	soon := mkProductAuth("expires-soon", auth.ProductWorkBuddy)
	late := mkProductAuth("expires-late", auth.ProductQoder)
	p := newTestPool(t, soon, late)

	// 制造到期分层：soon 明天到期，late 下个月到期。
	soonExpiry := time.Now().Add(24 * time.Hour).Unix()
	lateExpiry := time.Now().Add(30 * 24 * time.Hour).Unix()
	p.SetCreditsAndExpiry("expires-soon", 1000, soonExpiry)
	p.SetCreditsAndExpiry("expires-late", 1000, lateExpiry)

	p.SetMultiProduct(true, 0.3)
	// 故意让**快到期的一方更贵**：若成本能压过分层，贵的就不会被选中
	p.SetCostRate("expires-soon", 50.0) // 很贵但明天就过期
	p.SetCostRate("expires-late", 0.01) // 很便宜但下个月才过期

	counts := pickCounts(p, rounds)
	t.Logf("份额：expires-soon=%d expires-late=%d", counts["expires-soon"], counts["expires-late"])

	if counts["expires-soon"] == 0 {
		t.Error("快到期的一方（即使更贵）必须被优先烧掉 —— " +
			"成本压过了到期分层，会让即将过期的额度白白损失")
	}
	if counts["expires-late"] != 0 {
		t.Errorf("下个月才到期的一方不应分到流量（分层应只保留最早一档），实际 %d 次",
			counts["expires-late"])
	}
}

// TestCostRateClearedBetweenRequests 跨请求必须能清空成本率。
//
// 成本率取决于**当前请求的模型**：上一个请求的残留会让本请求按错误的
// 成本路由（比如上一个模型很贵，本请求换了便宜模型却仍被当贵处理）。
func TestCostRateClearedBetweenRequests(t *testing.T) {
	p := newTestPool(t, mkProductAuth("a", auth.ProductWorkBuddy))
	p.SetMultiProduct(true, 0.3)
	p.SetCostRate("a", 5.0)

	p.mu.RLock()
	before := p.byUID["a"].costRate
	p.mu.RUnlock()
	if before != 5.0 {
		t.Fatalf("costRate 应已设置，实际 %v", before)
	}

	p.ClearCostRates()

	p.mu.RLock()
	after := p.byUID["a"].costRate
	p.mu.RUnlock()
	if after != 0 {
		t.Errorf("清空后 costRate 应为 0，实际 %v —— 残留会让下个请求按错误的成本路由", after)
	}
}

// TestSetCostRateUnknownUIDIsNoop 给不存在的 uid 设成本率不得 panic。
func TestSetCostRateUnknownUIDIsNoop(t *testing.T) {
	p := newTestPool(t, mkProductAuth("a", auth.ProductWorkBuddy))
	p.SetCostRate("nonexistent", 1.0)
	p.ClearCostRates()
}

// TestProductOfNormalizesEmpty 空串必须归一成 WorkBuddy。
//
// 漏判会把 WorkBuddy 账号当成未知产品，走进 Qoder 的派发分支。
func TestProductOfNormalizesEmpty(t *testing.T) {
	wb := &auth.Auth{UID: "x"}
	if wb.ProductOf() != auth.ProductWorkBuddy {
		t.Errorf("空 Product 应归一成 %q，实际 %q", auth.ProductWorkBuddy, wb.ProductOf())
	}
	if wb.IsQoder() {
		t.Error("空 Product 不应被判为 Qoder")
	}

	qd := &auth.Auth{UID: "y", Product: auth.ProductQoder}
	if qd.ProductOf() != auth.ProductQoder {
		t.Errorf("ProductOf 应返回 qoder，实际 %q", qd.ProductOf())
	}
	if !qd.IsQoder() {
		t.Error("Product=qoder 应被判为 Qoder")
	}
}

// TestCostMultiplierBounds 乘子必须落在 [1-costWeight, 1+costWeight] 内。
//
// 若映射有误（比如线性映射大数值），乘子可能趋近 0 ——
// 那等于把某个产品**永久冷落**，而用户只会看到"某个账号从来不接流量"。
func TestCostMultiplierBounds(t *testing.T) {
	p := New("")
	const cw = 0.3
	p.SetMultiProduct(true, cw)

	cases := []struct {
		rate float64
		why  string
	}{
		{0, "未声明 → 中性"},
		{0.001, "极便宜"},
		{0.5, "中等"},
		{5, "较贵"},
		{1000, "极贵"},
		{1e9, "极端值"},
	}
	for _, tc := range cases {
		got := p.costMultiplierLocked(tc.rate)
		if got < 1-cw-1e-9 || got > 1+cw+1e-9 {
			t.Errorf("costMultiplier(%v) = %v 越界 [%v, %v]（%s）",
				tc.rate, got, 1-cw, 1+cw, tc.why)
		}
		if got <= 0 {
			t.Errorf("costMultiplier(%v) = %v ≤ 0 —— 会让该产品被永久冷落", tc.rate, got)
		}
	}
	// 未声明必须严格中性
	if got := p.costMultiplierLocked(0); got != 1.0 {
		t.Errorf("未声明的乘子必须严格为 1.0，实际 %v", got)
	}
	// 越便宜乘子越大
	cheap := p.costMultiplierLocked(0.1)
	pricey := p.costMultiplierLocked(10)
	if cheap <= pricey {
		t.Errorf("更便宜的一方乘子应更大：cheap=%v pricey=%v", cheap, pricey)
	}
}

func absInt(n int) int {
	if n < 0 {
		return -n
	}
	return n
}
