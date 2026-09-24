package pool

// 「免费额度绝对优先」的测试（2026-09-23 所有者需求）。
//
// # 需求原话
//
//	「模型路由不但要考虑积分到期,积分总量,还要考虑 实际消耗积分,这些因素
//	  比如 deepseek-v4.1-flash 在国际版是free 0倍率,国内是0.03 …
//	  这时候就应该优先路由国际账号,因为他是free 其次才是积分快到期的账号」
//
// 即**免费是第一优先级，到期日是第二**。本文件钉住这条语义，
// 以及它与既有「到期分层优先」契约的分界。
//
// # 实测数据（2026-09-23 拉自己的账号得到）
//
//	WorkBuddy  deepseek-v4.1-flash  国服 x0.03  国际版 x0.00（免费）
//	Qoder      Qwen3.8-Flash        priceFactor 0（免费）
//
// 同名模型两区差价 100%，免费那侧应承接全部流量。

import (
	"testing"
	"time"

	"workbuddy2api/internal/auth"
)

// TestFreeQuotaBeatsExpiryTier 免费**必须压过**到期分层。
//
// 这是本需求的核心断言。构造一个"最容易失败"的场景：
//   - 付费账号（模拟国服 x0.03）**明天**就到期  ← 到期分层会强烈偏向它
//   - 免费账号（模拟国际版 x0.00）**下个月**才到期 ← 分层会把它排后面
//
// 按旧行为（成本仅在档内微调），免费的那个**根本进不了候选集**：
// 到期分层只保留最早一档，而它是"下个月"那档。
// 按新行为（免费优先于到期），它必须拿到**全部**流量。
func TestFreeQuotaBeatsExpiryTier(t *testing.T) {
	const rounds = 2000

	paid := mkProductAuth("paid-cn", auth.ProductWorkBuddy)
	free := mkProductAuth("free-intl", auth.ProductWorkBuddy)
	p := newTestPool(t, paid, free)

	// 付费方明天到期（分层会让它优先），免费方下个月才到期。
	p.SetCreditsAndExpiry("paid-cn", 1000, time.Now().Add(24*time.Hour).Unix())
	p.SetCreditsAndExpiry("free-intl", 1000, time.Now().Add(30*24*time.Hour).Unix())

	// 两个区域账号：付费国服 0.03，免费国际版 0.00。
	// ⚠ 免费那侧必须传 known=true（0 是"已声明免费"，不是"未声明"）——
	// 这正是三态语义的关键，传错了本测试会红。
	p.SetMultiProduct(true, 0.3)
	p.SetCostRateKnown("paid-cn", 0.03, true)
	p.SetCostRateKnown("free-intl", 0.00, true)

	counts := pickCounts(p, rounds)
	t.Logf("份额：paid-cn=%d free-intl=%d", counts["paid-cn"], counts["free-intl"])

	if counts["free-intl"] == 0 {
		t.Fatal("免费账号一次都没被选中 —— 「免费优先」没有生效。" +
			"最可能的原因：freeTierLocked 没被调用，或者 0 被当成「未声明」")
	}
	if counts["paid-cn"] != 0 {
		t.Errorf("付费账号不应分到流量（免费额度无限优先），实际 %d 次", counts["paid-cn"])
	}
}

// TestFreePriorityIsOptOutWhenMultiProductOff 多产品开关关闭时**完全不介入**。
//
// 这是"零漂移"承诺的一部分：`multiProductOn=false` 时不应读 costRate、
// 不应改变候选集。既有的 TestCostSwitchOffIsZeroDrift 测的是"权重不受影响"，
// 本测试补上**候选集**这一半 —— 免费优先会**收窄候选**，
// 若不跟随开关关闭，就是一个比权重更剧烈的行为改变。
func TestFreePriorityIsOptOutWhenMultiProductOff(t *testing.T) {
	const rounds = 1500

	a := mkProductAuth("a", auth.ProductWorkBuddy)
	b := mkProductAuth("b", auth.ProductWorkBuddy)
	p := newTestPool(t, a, b)

	// 两个都有到期信息，且 a 更早到期（分层应偏向 a）。
	p.SetCreditsAndExpiry("a", 1000, time.Now().Add(24*time.Hour).Unix())
	p.SetCreditsAndExpiry("b", 1000, time.Now().Add(30*24*time.Hour).Unix())

	// 只给 b 标"免费"，但**不打开**多产品开关。
	p.SetCostRateKnown("a", 0.03, true)
	p.SetCostRateKnown("b", 0.00, true)

	counts := pickCounts(p, rounds)
	t.Logf("份额：a=%d b=%d", counts["a"], counts["b"])

	// 开关关闭 ⇒ 免费标记必须**完全无效** ⇒ 退回"只烧最早一档" ⇒ 全是 a。
	if counts["b"] != 0 {
		t.Errorf("多产品开关关闭时，免费标记不该收窄候选集（b 分到了 %d 次）", counts["b"])
	}
	if counts["a"] == 0 {
		t.Error("开关关闭时应保持原有到期分层行为（a 一次都没被选中）")
	}
}

// TestUndeclaredCostIsNotFree 未声明的成本**不得**被当成免费。
//
// # 这是一个真实风险的守卫
//
// 倍率表是**逐步补齐**的：某个产品可能还没查询到倍率。
// 若把"未声明"（costKnown=false）当成免费，那个产品会**抢走全部流量** ——
// 而它可能恰恰是最贵的一个。
//
// 与 TestUndeclaredCostIsNeutral 的关系：那条测"权重中性"，
// 本条测"不参与免费的绝对优先"。后者是更强的主张，必须单独钉住。
func TestUndeclaredCostIsNotFree(t *testing.T) {
	const rounds = 2000

	knownPaid := mkProductAuth("known-paid", auth.ProductWorkBuddy)
	unknown := mkProductAuth("unknown", auth.ProductWorkBuddy)
	p := newTestPool(t, knownPaid, unknown)

	// 两者都明天到期（同一档），排除到期分层的干扰。
	exp := time.Now().Add(24 * time.Hour).Unix()
	p.SetCreditsAndExpiry("known-paid", 1000, exp)
	p.SetCreditsAndExpiry("unknown", 1000, exp)

	p.SetMultiProduct(true, 0.3)
	p.SetCostRateKnown("known-paid", 0.03, true) // 已声明：付费
	// unknown 故意**不设** ⇒ costKnown=false ⇒ 不得被当成免费
	p.ClearUnknownCost(t, "unknown")

	counts := pickCounts(p, rounds)
	t.Logf("份额：known-paid=%d unknown=%d", counts["known-paid"], counts["unknown"])

	if counts["unknown"] == 0 {
		t.Fatal("未声明成本的一方被完全排除了 —— " +
			"「未声明」被误当成了「免费」，会让还没查到倍率的产品独占流量")
	}
	if counts["known-paid"] == 0 {
		t.Error("已声明付费的一方一次都没被选中")
	}
}

// TestFreeTierKeepsExpiryOrderAmongFree 免费候选**之间**仍要先烧快过期的。
//
// 「免费优先」只改变"免不免费"这一层；同一层内到期分层必须继续生效，
// 否则两个免费账号之间会变成随机分摊，快作废的那份额度分不完就浪费了。
func TestFreeTierKeepsExpiryOrderAmongFree(t *testing.T) {
	const rounds = 2000

	soonFree := mkProductAuth("free-soon", auth.ProductWorkBuddy)
	lateFree := mkProductAuth("free-late", auth.ProductWorkBuddy)
	paid := mkProductAuth("paid", auth.ProductWorkBuddy)
	p := newTestPool(t, soonFree, lateFree, paid)

	p.SetCreditsAndExpiry("free-soon", 1000, time.Now().Add(24*time.Hour).Unix())
	p.SetCreditsAndExpiry("free-late", 1000, time.Now().Add(30*24*time.Hour).Unix())
	// 付费方最早到期 —— 若免费层没生效，它会抢走流量。
	p.SetCreditsAndExpiry("paid", 1000, time.Now().Add(2*time.Hour).Unix())

	p.SetMultiProduct(true, 0.3)
	p.SetCostRateKnown("free-soon", 0.00, true)
	p.SetCostRateKnown("free-late", 0.00, true)
	p.SetCostRateKnown("paid", 0.03, true)

	counts := pickCounts(p, rounds)
	t.Logf("份额：free-soon=%d free-late=%d paid=%d",
		counts["free-soon"], counts["free-late"], counts["paid"])

	if counts["free-soon"] == 0 {
		t.Error("免费且最早到期的一方应被优先烧掉")
	}
	if counts["free-late"] != 0 {
		t.Errorf("免费但下个月才到期的一方不该分到流量（免费层内仍要分层），实际 %d 次",
			counts["free-late"])
	}
	if counts["paid"] != 0 {
		t.Errorf("付费方不应分到流量（有免费候选时），实际 %d 次", counts["paid"])
	}
}

// TestFreeTierFallsBackWhenFreeUnavailable 免费账号不可用时必须**退回**付费账号。
//
// 这是最重要的可用性守卫：免费优先**绝不能**变成"免费挂了就整体不可用"。
// 免费账号在冷却/禁用/在途占满时，必须能正常用付费账号服务。
func TestFreeTierFallsBackWhenFreeUnavailable(t *testing.T) {
	free := mkProductAuth("free", auth.ProductWorkBuddy)
	paid := mkProductAuth("paid", auth.ProductWorkBuddy)
	p := newTestPool(t, free, paid)

	p.SetMultiProduct(true, 0.3)
	p.SetCostRateKnown("free", 0.00, true)
	p.SetCostRateKnown("paid", 0.03, true)

	// 把免费账号禁用（模拟"用户关了它"或"它被判死"）。
	p.Disable("free", "test: 模拟免费账号不可用")

	counts := pickCounts(p, 500)
	t.Logf("份额：free=%d paid=%d", counts["free"], counts["paid"])

	if counts["paid"] == 0 {
		t.Fatal("免费账号不可用时，必须退回付费账号 —— " +
			"否则「免费优先」会变成「免费一挂就整体 503」")
	}
	if counts["free"] != 0 {
		t.Error("被禁用的免费账号不该被选中")
	}
}

// ClearUnknownCost 确保某账号保持「成本未声明」。
//
// 存在意义：其它用例可能已经给池里的账号设过 costRate，
// 而这里要测的正是"从没设过"的状态。显式清一次比依赖执行顺序可靠。
func (p *Pool) ClearUnknownCost(t *testing.T, uid string) {
	t.Helper()
	p.SetCostRateKnown(uid, 0, false)
}
