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
		"workbuddy": {},                // 空
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

// TestProductPrefixedModelStillWorks 带前缀时选号要**同时**满足两件事：
// 产品对得上，且该产品**确实提供**这个模型。
//
// # ⚠⚠ 这条用例被反转过两次，最终结论以**所有者 2026-09-28 的原话**为准
//
// 第一版（我写的）：断言"显式指定 qoder 而 qoder 不提供该模型 → 选不出账号"。
// 实测返回了 qd-1，我误以为是自己写错了，于是改成"显式指定即尊重"，
// 并写下了一句看起来很有道理的注释：
//
//	「用户写 `qoder:deepseek-v4.1-flash` 的意思就是'用 qoder 跑它'。
//	  产品清单是用来防止**无前缀时误路由**的，不是用来否决用户显式指令的。」
//
// **那句话不是所有者的意图。** 他 2026-09-28 明确纠正：
//
//	「那个意图不是我的，在选号侧就是要挡请求，你修复一下吧」
//
// 现场是他发 `qoder:deepseek-v4.1-flash` —— 而 qoder 的 18 个模型里
// **根本没有**它，上游却回了内容，于是用量统计里出现
// 「deepseek-v4.1-flash · Qoder」这一行幽灵数据；
// 而 qoder 收到 WorkBuddy 风格的参数后由模型提供商拒绝、回 400 11133，
// 用户看到「参数不符合当前模型要求」，完全想不到是**平台选错了**。
//
// ⇒ 现在的契约：**清单里有才放行**（不知道时不拦，见下面的
// TestProductUnknownListDoesNotBlock 与 productMayServeLocked 的注释）。
func TestProductPrefixedModelStillWorks(t *testing.T) {
	p := newProductGuardPool(t)
	p.SetProductModels(map[string][]string{
		"workbuddy": {"deepseek-v4.1-flash"},
		"qoder":     {"qwen3.8-flash"},
	})
	// 显式指定 qoder，但 qoder 清单里**没有**该模型 → 必须选不出
	got := p.PickForModelProductRegion("deepseek-v4.1-flash", nil, auth.RegionAny, "qoder")
	if got != nil {
		t.Fatalf("qoder 清单里没有该模型，不该选出账号（否则会发出去被上游以"+
			"11133 拒绝，并在统计里记成「该模型 · Qoder」的幽灵数据）；实际选中 %s", got.UID)
	}
	// 指定 workbuddy（清单里**有**）→ 正常选出
	got2 := p.PickForModelProductRegion("deepseek-v4.1-flash", nil, auth.RegionAny, "workbuddy")
	if got2 == nil || got2.UID != "wb-1" {
		t.Errorf("显式指定 workbuddy 且清单里有该模型，应选出 wb-1，实际 %v", got2)
	}
	// qoder 自己的模型仍要能选出来（别把拦截做成"整个产品不可用"）
	got3 := p.PickForModelProductRegion("qwen3.8-flash", nil, auth.RegionAny, "qoder")
	if got3 == nil || got3.ProductOf() != auth.ProductQoder {
		t.Errorf("qoder 自己的模型必须仍能选出，实际 %v", got3)
	}
}

// TestProductUnknownListDoesNotBlock ★ 清单**未知**时不能拦。
//
// # 为什么这条和上面同等重要
//
// 判据是"**知道**才拦"而不是"没写就拦"。宿主那份 `product_models`
// 来自用户白名单，**可能不全**（实测：workbuddy 就不在里面）。
// 若把"清单里没有"一律当成"不支持"，workbuddy 账号会被整片误杀 ——
// 表现为"裸名模型全部报没有可用账号"，那比幽灵统计严重得多。
//
// `productMayServeLocked` 的三态语义正是为此：
//
//	清单有 + 含该模型 → 放行
//	清单有 + 不含     → 看兜底表，兜底表也没有才拦
//	清单**未知**      → 放行（宁可放行）
func TestProductUnknownListDoesNotBlock(t *testing.T) {
	p := newProductGuardPool(t)
	// 只给 qoder 清单，**不给** workbuddy —— 模拟宿主白名单为空的实际情形
	p.SetProductModels(map[string][]string{"qoder": {"qwen3.8-flash"}})

	got := p.PickForModelProductRegion("deepseek-v4.1-flash", nil, auth.RegionAny, "workbuddy")
	if got == nil {
		t.Fatal("workbuddy 清单**未知**时不该拦 —— 否则宿主白名单为空会让" +
			"所有裸名模型报「没有可用账号」，那是比幽灵统计严重得多的回归")
	}
	if got.ProductOf() != auth.ProductWorkBuddy {
		t.Errorf("应选出 workbuddy 账号，实际 %s（%s）", got.UID, got.ProductOf())
	}
}

// TestProductFallbackListStillAllows 兜底（猜测）清单里的模型仍放行。
//
// 兜底清单是"网关已知的更多可能性"，用于避免"宿主清单不全导致误拦"。
// 它只放宽、不收紧 —— 与 productMayServeLocked 的取向一致。
func TestProductFallbackListStillAllows(t *testing.T) {
	p := newProductGuardPool(t)
	p.SetProductModels(map[string][]string{"qoder": {"qwen3.8-flash"}})
	p.SetProductModelsFallback(map[string][]string{"qoder": {"deepseek-v4.1-flash"}})

	got := p.PickForModelProductRegion("deepseek-v4.1-flash", nil, auth.RegionAny, "qoder")
	if got == nil {
		t.Fatal("可信清单没有但**兜底清单有**时应放行 —— 兜底表只放宽、不收紧")
	}
	if got.ProductOf() != auth.ProductQoder {
		t.Errorf("应选出 qoder 账号，实际 %s（%s）", got.UID, got.ProductOf())
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
