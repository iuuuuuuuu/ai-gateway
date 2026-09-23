package pool

// NoRouteExcludesAll 用来把「无账号可用」的错误文案说准。
//
// # 现场（2026-09-22 所有者报告）
//
// 他发 `intl:deepseek-v4.1-flash`，得到：
//
//	503 {"code":"no_healthy_account",
//	     "message":"all accounts unavailable (cooling/disabled)"}
//
// 但 5 个国际版账号实测 `cooling=false disabled=false` —— 健康得很。
// 真实原因是它们**全被设了 `no_route`**（就是本包另一条用例里说的
//「他把国际版账号全部标为不接流量」）。
//
// 「冷却/禁用」这句会让他去等"恢复"，而实际只要把路由开关打开。
//
// # ⚠⚠ 本文件最重要的一条是 TestNoRouteExcludesAllFalseWhenOneIsUsable
//
// 我第一版把判据写成 `NoRouteOnlyCount(...) > 0`，即"只要有 no_route 账号
// 就报『都被你禁用了』"。实测当场打脸：
//
//	Qoder 有 2 个账号 —— qoder.com.cn(no_route) + qoder.sh(本来可用)
//	请求选中 qoder.sh → 它自己请求失败 → 轮转到 acct==nil
//	我的文案却说「1 个账号都被设为 no_route，所以没有账号可用了」
//
// **真实原因是 qoder.sh 失败了**，no_route 只是少了一个候选 ——
// 那正是"错误文案把排查方向引偏"，与我要修的缺陷同一类，只是换了个方向。
//
// 故判据收紧成 `noRoute == total && total > 0`，并用下面那条用例钉死。

import (
	"testing"
	"time"

	"workbuddy2api/internal/auth"
)

// TestNoRouteExcludesAllWhenEveryUsableAccountIsNoRoute 基本情形：全 no_route ⇒ 成立。
func TestNoRouteExcludesAllWhenEveryUsableAccountIsNoRoute(t *testing.T) {
	p := New("")
	for _, uid := range []string{"intl-1", "intl-2", "intl-3"} {
		p.Add(&auth.Auth{
			UID:         uid,
			AccessToken: "tok",
			Domain:      "www.workbuddy.ai",
			NoRoute:     true, // 用户手动「不接流量」
		})
	}
	nr, total := p.NoRouteExcludesAll("workbuddy", "deepseek-v4.1-flash", auth.RegionIntl)
	if total != 3 || nr != 3 {
		t.Errorf("应得 (3,3)，实际 (%d,%d)", nr, total)
	}
	if !(total > 0 && nr == total) {
		t.Error("三个账号全是 no_route ⇒ 调用方应据此报『都被设为不参与选号』")
	}
}

// TestNoRouteExcludesAllFalseWhenOneIsUsable ⚠ 核心用例：只要还有一个
// 本来可用的非 no_route 账号，就**不得**归因到 no_route。
//
// 这条对应我实测踩到的真实形态（Qoder 2 个账号、1 个 no_route、
// 另 1 个可用但请求失败）。若不拦，"no_route 是唯一原因"就是假话。
func TestNoRouteExcludesAllFalseWhenOneIsUsable(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "qoder-cn", AccessToken: "tok",
		Domain: "qoder.com.cn", Product: auth.ProductQoder, NoRoute: true})
	p.Add(&auth.Auth{UID: "qoder-intl", AccessToken: "tok",
		Domain: "qoder.sh", Product: auth.ProductQoder}) // 可用

	nr, total := p.NoRouteExcludesAll("qoder", "Qwen3.8-Flash", auth.RegionAny)
	if total != 2 || nr != 1 {
		t.Errorf("应得 (1,2)，实际 (%d,%d)", nr, total)
	}
	if nr == total {
		t.Error("还有一个可用账号 ⇒ **不能**说『都被设为 no_route』：" +
			"真实原因需要从那次失败的请求里找，不能盖掉")
	}
}

// TestNoRouteExcludesAllIgnoresNormalAccounts 反例：没设 no_route 的不该被数进来。
//
// 与第一条成对。只测"数得出来"可能把它写成"返回该产品账号总数"，
// 那样错误文案会把健康账号也说成"被你禁用了"。
func TestNoRouteExcludesAllIgnoresNormalAccounts(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "ok-1", AccessToken: "tok", Domain: "www.workbuddy.ai"})
	p.Add(&auth.Auth{UID: "ok-2", AccessToken: "tok", Domain: "www.workbuddy.ai"})
	nr, total := p.NoRouteExcludesAll("workbuddy", "any", auth.RegionIntl)
	if nr != 0 || total != 2 {
		t.Errorf("没有 no_route 账号时应得 (0,2)，实际 (%d,%d)", nr, total)
	}
	if nr == total && total > 0 {
		t.Error("没有 no_route 账号时不该报『都被设为 no_route』")
	}
}

// TestNoRouteExcludesAllExcludesAlreadyUnusable no_route **之外**本来就不可用的
// 账号不该进分母。
//
// # 为什么这条重要
//
// 一个产品若有 5 个号：1 个 no_route + 4 个正在冷却。
// 真实原因是"冷却"，而若把 4 个冷却的也计入分母，我们会说成
// 「5 个都被你设成不接流量了」—— **另一句假话**，而且比通用文案更误导
//（用户会去翻一个本来就正确的设置）。
func TestNoRouteExcludesAllExcludesAlreadyUnusable(t *testing.T) {
	p := New("")
	// 这个确实该数（本来可用 + no_route）。
	p.Add(&auth.Auth{UID: "noroute-ok", AccessToken: "tok",
		Domain: "www.workbuddy.ai", NoRoute: true})
	// 这个虽然 no_route，但**正在冷却** ⇒ 不该进分母。
	p.Add(&auth.Auth{UID: "noroute-cooling", AccessToken: "tok",
		Domain: "www.workbuddy.ai", NoRoute: true})
	p.Cooldown("noroute-cooling", CoolSoft, 10*time.Minute, "测试：冷却中")

	nr, total := p.NoRouteExcludesAll("workbuddy", "deepseek-v4.1-flash", auth.RegionIntl)
	if total != 1 || nr != 1 {
		t.Errorf("只该数「除 no_route 外本来可用」的那个，应得 (1,1)，实际 (%d,%d)\n"+
			"（把冷却中的算进分母会说成「都被你禁用了」——另一句假话）", nr, total)
	}
}

// TestNoRouteExcludesAllEmptyProductHasZeroTotal 该产品一个账号都没有时
// total 必须是 0 —— 调用方据此**不能**归因到 no_route（与 no_route 无关）。
func TestNoRouteExcludesAllEmptyProductHasZeroTotal(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "wb", AccessToken: "tok", Domain: "copilot.tencent.com"})

	nr, total := p.NoRouteExcludesAll("qoder", "m", auth.RegionAny)
	if total != 0 || nr != 0 {
		t.Errorf("没有 qoder 账号时该得 (0,0)，实际 (%d,%d)", nr, total)
	}
	// ⚠ `total > 0` 这个护栏不能少：否则 0==0 会被当成"全被 no_route 挡了"。
	if total > 0 && nr == total {
		t.Error("total==0 时不该判定为『全被 no_route 挡了』")
	}
}

// TestNoRouteExcludesAllRespectsProductAndRegion 产品/区域不符的账号不该被数。
//
// 否则用户发 `intl:` 时，我们会把**国服**被他禁用的账号也算进来，
// 说成"10 个账号都被设成不接流量"—— 而他真正要用的国际版只有 5 个。
func TestNoRouteExcludesAllRespectsProductAndRegion(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "intl-nr", AccessToken: "tok",
		Domain: "www.workbuddy.ai", NoRoute: true})
	p.Add(&auth.Auth{UID: "cn-nr", AccessToken: "tok",
		Domain: "copilot.tencent.com", NoRoute: true})

	nr, total := p.NoRouteExcludesAll("workbuddy", "m", auth.RegionIntl)
	if total != 1 || nr != 1 {
		t.Errorf("RegionIntl 只该数国际版的，应得 (1,1)，实际 (%d,%d)", nr, total)
	}
	if nr, total := p.NoRouteExcludesAll("workbuddy", "m", auth.RegionAny); total != 2 || nr != 2 {
		t.Errorf("RegionAny 应得 (2,2)，实际 (%d,%d)", nr, total)
	}
}

// TestNoRouteExcludesAllRespectsProduct 产品不符时不该数 ——
// 否则 qoder 的请求会把 workbuddy 的 no_route 账号也算进来。
func TestNoRouteExcludesAllRespectsProduct(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "wb-nr", AccessToken: "tok",
		Domain: "www.workbuddy.ai", NoRoute: true})
	p.Add(&auth.Auth{UID: "qoder-nr", AccessToken: "tok",
		Domain: "qoder.sh", Product: auth.ProductQoder, NoRoute: true})

	if nr, total := p.NoRouteExcludesAll("qoder", "m", auth.RegionAny); total != 1 || nr != 1 {
		t.Errorf("qoder 应得 (1,1)，实际 (%d,%d)", nr, total)
	}
	if nr, total := p.NoRouteExcludesAll("workbuddy", "m", auth.RegionAny); total != 1 || nr != 1 {
		t.Errorf("workbuddy 应得 (1,1)，实际 (%d,%d)", nr, total)
	}
}
