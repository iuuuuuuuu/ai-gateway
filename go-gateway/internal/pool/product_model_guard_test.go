package pool

// product_model_guard_test.go 「产品不提供该模型就不该被路由到它」的回归测试。
//
// # 这条测试守的是一个**实测缺陷**（2026-09-20）
//
// 所有者用 `deepseek-v4.1-flash`（一个 **WorkBuddy** 的模型）发请求，
// 却被路由到 **Qoder 账号**上，**失败两次** —— 而 Qoder 根本没有这个模型。
//
// 原因：无前缀模型名走 `pickForModelAny`，它只按"账号当前是否可用"挑，
// **不问"这个产品有没有这个模型"**。Qoder 账号当时恰好可用就被选中。
//
// 症状对用户极难理解：**模型名是 WorkBuddy 的，报错却来自 Qoder**。
//
// # 为什么用单测而不是真机复现
//
// 复现需要"Qoder 账号恰好可用"这一时序条件，且会真的打上游消费额度。
// 而判据全在 `pickForModelAny` 的候选集构造里，单测能精确锁定。
//
// # 三个必须保住的既有行为（防修过头）
//
//  1. 没有清单的产品**不排除**（"不知道"≠"不提供"）——
//     否则宿主没透传时所有账号都会被排掉，那比原缺陷更糟
//  2. 单产品模式（multiProduct 关闭）**行为逐字不变**
//  3. 有前缀的模型名走另一条路径，本来就带产品约束，不受影响
import (
	"testing"

	"workbuddy2api/internal/auth"
)

// newProductGuardPool 造一个含 workbuddy + qoder 两个账号的池。
//
// 照既有测试的写法（`product_route_test.go`）：`New("")` + `p.Add(...)`。
// workbuddy 用**显式常量**而不是空串 —— 两者语义等价（`ProductOf()` 会把
// 空串归一成 workbuddy），但显式写出来让"这个账号属于哪个产品"一眼可见，
// 也避免读者以为空串是"未设置"。
func newProductGuardPool(t *testing.T) *Pool {
	t.Helper()
	p := New("")
	p.Add(&auth.Auth{UID: "wb-1", AccessToken: "t", Product: auth.ProductWorkBuddy})
	p.Add(&auth.Auth{UID: "qd-1", AccessToken: "t", Product: auth.ProductQoder})
	p.SetMultiProduct(true, 0)
	return p
}

// TestUnprefixedModelSkipsProductWithoutIt 核心回归：无前缀时，
// 不提供该模型的产品**不该**被选中。
//
// 这正是实测缺陷：`deepseek-v4.1-flash` 是 WorkBuddy 的，
// 而 Qoder 账号被选中并失败。
func TestUnprefixedModelSkipsProductWithoutIt(t *testing.T) {
	p := newProductGuardPool(t)

	// workbuddy 提供 deepseek-v4.1-flash；qoder 只提供 qwen 系
	p.SetProductModels(map[string][]string{
		"workbuddy": {"deepseek-v4.1-flash", "glm-5.3"},
		"qoder":     {"qwen3.8-flash", "qwen3.8-max"},
	})

	// 反复挑多次：必须**每次都**落到 workbuddy 那个账号
	for i := 0; i < 20; i++ {
		got := p.PickForModelRegion("deepseek-v4.1-flash", nil, auth.RegionAny)
		if got == nil {
			t.Fatalf("第 %d 次：应能选出 workbuddy 账号，实际 nil", i)
		}
		if got.UID != "wb-1" {
			t.Fatalf("第 %d 次：`deepseek-v4.1-flash` 是 WorkBuddy 的模型，"+
				"却路由到了 %s（product=%q）—— 这正是实测缺陷"+
				"（用户看到模型名是 WorkBuddy 的，报错却来自 Qoder）",
				i, got.UID, got.ProductOf())
		}
	}
}

// TestUnprefixedModelPrefersOwningProduct 反方向：Qoder 独有的模型
// 必须落到 Qoder 账号上（不能因为修 A 方向而把 B 方向弄坏）。
func TestUnprefixedModelPrefersOwningProduct(t *testing.T) {
	p := newProductGuardPool(t)
	p.SetProductModels(map[string][]string{
		"workbuddy": {"deepseek-v4.1-flash"},
		"qoder":     {"qwen3.8-flash"},
	})
	for i := 0; i < 20; i++ {
		got := p.PickForModelRegion("qwen3.8-flash", nil, auth.RegionAny)
		if got == nil {
			t.Fatalf("第 %d 次：应能选出 qoder 账号", i)
		}
		if got.ProductOf() != auth.ProductQoder {
			t.Fatalf("第 %d 次：qwen3.8-flash 应路由到 Qoder，实际 %s（%s）",
				i, got.UID, got.ProductOf())
		}
	}
}

// TestUnknownProductIsNotExcluded 没有清单的产品**不能**被排除。
//
// 这是最重要的防修过头：「不知道它提供什么」不等于「它不提供」。
// 若这里排除了，宿主没透传清单时**所有账号都会消失**，用户直接 503 ——
// 比原缺陷严重得多。
func TestUnknownProductIsNotExcluded(t *testing.T) {
	p := newProductGuardPool(t)
	// 只给 qoder 清单，**不给 workbuddy**（模拟宿主未透传）
	p.SetProductModels(map[string][]string{
		"qoder": {"qwen3.8-flash"},
	})

	// 一个不在任何清单里的模型：两个账号都该保留在候选里
	got := p.PickForModelRegion("some-unknown-model", nil, auth.RegionAny)
	if got == nil {
		t.Fatal("没有清单的产品不该被排除（否则宿主未透传时全部账号消失）")
	}
}

// TestEmptyProductListIsIgnored 空清单等于"不知道"，不是"不提供"。
func TestEmptyProductListIsIgnored(t *testing.T) {
	p := newProductGuardPool(t)
	p.SetProductModels(map[string][]string{
		"workbuddy": {},              // 空
		"qoder":     {"qwen3.8-flash"}, // 非空
	})
	// workbuddy 清单为空 ⇒ 不约束 workbuddy 账号
	if got := p.PickForModelRegion("whatever", nil, auth.RegionAny); got == nil {
		t.Fatal("空清单不该让该产品的账号被排除")
	}
}

// TestSingleProductModeUnchanged 单产品模式**行为逐字不变**。
//
// multiProduct 关闭时不该有任何新约束 —— 那是"既有行为逐字不变"的承诺，
// 也是产品维度出问题时的回滚点。
func TestSingleProductModeUnchanged(t *testing.T) {
	p := newProductGuardPool(t)
	p.SetProductModels(map[string][]string{
		"workbuddy": {"only-this"},
		"qoder":     {"only-that"},
	})
	// 关掉多产品
	p.SetMultiProduct(false, 0)

	// 即便模型"不属于"某产品，单产品模式下也不该排除
	if got := p.PickForModelRegion("only-that", nil, auth.RegionAny); got == nil {
		t.Fatal("单产品模式不该因产品清单而排除账号（既有行为必须不变）")
	}
}

// TestProductPrefixedModelStillWorks 带前缀时**用户显式指定了产品**，
// 故产品约束优先于"该产品是否提供该模型"。
//
// # ⚠ 我第一版写错了这条断言（记录以免再犯）
//
// 我原本断言"显式指定 qoder 而 qoder 不提供该模型 → 应选不出账号"。
// 实测返回了 qd-1 —— 而**那才是对的**：
//
//	用户写 `qoder:deepseek-v4.1-flash` 的意思就是"用 qoder 跑它"。
//	产品清单是用来防止**无前缀时误路由**的，不是用来否决用户显式指令的。
//
// 若真按我原来的断言实现，"用户显式指定却被我们静默拒绝"会变成更难懂的行为。
// 故改断言为"显式指定即尊重"。
func TestProductPrefixedModelStillWorks(t *testing.T) {
	p := newProductGuardPool(t)
	p.SetProductModels(map[string][]string{
		"workbuddy": {"deepseek-v4.1-flash"},
		"qoder":     {"qwen3.8-flash"},
	})
	// 显式指定 qoder：即便该模型不在 qoder 清单里，也**尊重用户指令**
	got := p.PickForModelProductRegion("deepseek-v4.1-flash", nil, auth.RegionAny, "qoder")
	if got == nil {
		t.Fatal("显式指定 qoder 时应选出 Qoder 账号 —— 用户意图优先于清单（清单只用于防误路由）")
	}
	if got.ProductOf() != auth.ProductQoder {
		t.Errorf("应选出 qoder 账号，实际 %s（%s）", got.UID, got.ProductOf())
	}
	// 指定 workbuddy 则选 workbuddy
	got2 := p.PickForModelProductRegion("deepseek-v4.1-flash", nil, auth.RegionAny, "workbuddy")
	if got2 == nil || got2.UID != "wb-1" {
		t.Errorf("显式指定 workbuddy 应选出 wb-1，实际 %v", got2)
	}
}

// TestProductOffersModelQuery 查询接口本身。
func TestProductOffersModelQuery(t *testing.T) {
	p := newProductGuardPool(t)
	p.SetProductModels(map[string][]string{
		"workbuddy": {"DeepSeek-V4.1-Flash"}, // 故意大写，验证大小写不敏感
	})
	if ok, known := p.ProductOffersModel("workbuddy", "deepseek-v4.1-flash"); !ok || !known {
		t.Errorf("模型名比较应大小写不敏感：got ok=%v known=%v", ok, known)
	}
	if _, known := p.ProductOffersModel("zcode", "x"); known {
		t.Error("没有清单的产品应报 known=false（调用方据此不排除）")
	}
	if ok, known := p.ProductOffersModel("workbuddy", "not-there"); ok || !known {
		t.Errorf("清单里没有的模型应报 ok=false known=true，实际 ok=%v known=%v", ok, known)
	}
}

// TestSetProductModelsEmptyClearsConstraint 传空 map 等于清掉约束。
func TestSetProductModelsEmptyClearsConstraint(t *testing.T) {
	p := newProductGuardPool(t)
	p.SetProductModels(map[string][]string{"workbuddy": {"a"}})
	p.SetProductModels(map[string][]string{}) // 清掉
	if got := p.PickForModelRegion("b", nil, auth.RegionAny); got == nil {
		t.Fatal("清掉清单后不该再排除任何产品")
	}
}
