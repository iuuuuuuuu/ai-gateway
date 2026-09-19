package pool

// 「模型 → 允许的平台」白名单的契约测试。
//
// # 所有者的需求
//
//	「我希望可以加上 **平台区分使用哪个平台的模型**」
//
// 背景：同名模型可能同时在多个平台上（实测 `glm-5.2` 在 WorkBuddy 与
// ZCode 上都有）。选号是加权随机的，用户无法指定"这个模型走 ZCode" ——
// 而各平台的额度性质不同（ZCode 体验套餐 9/23 到期作废，该优先烧掉）。
//
// # 这几条测试要钉住的三件事
//
//  1. **只勾一个平台 = 只用它**（这是需求的核心）
//  2. **不配置 = 现状不变**（不能因为加了这个功能而影响老用户）
//  3. **全被排除时不硬失败**（回退兜底，不要 503）

import (
	"testing"

	"workbuddy2api/internal/auth"
)

// pickProductFor 反复选号，返回选中的产品集合（用足够多次覆盖随机性）。
//
// 为什么取"集合"而不是单次结果：选号是加权随机的，单次结果没有判别力 ——
// 一个"从不选 ZCode"的实现与"偶尔选 ZCode"的实现，单次都可能选中 WorkBuddy。
func pickProductFor(t *testing.T, p *Pool, model string, n int) map[string]int {
	t.Helper()
	got := map[string]int{}
	for i := 0; i < n; i++ {
		a := p.PickForModel(model, nil)
		if a == nil {
			continue
		}
		got[a.Product]++
	}
	return got
}

// TestModelPlatformsRestrictsToAllowedProduct 只勾一个平台 = 只用它。
//
// 这是需求的核心：用户勾了"glm-5.2 只用 ZCode"，就不能再出现
// WorkBuddy 的账号被选中 —— 否则体验额度烧不掉，用户的目的没达到。
func TestModelPlatformsRestrictsToAllowedProduct(t *testing.T) {
	wb := mkProductAuth("wb-1", auth.ProductWorkBuddy)
	zc := mkProductAuth("zc-1", auth.ProductZcode)
	p := newTestPool(t, wb, zc)
	p.SetMultiProduct(true, 0)

	// 不限制时：两个平台都可能被选中
	before := pickProductFor(t, p, "glm-5.2", 60)
	if len(before) < 2 {
		t.Fatalf("未配置白名单时应能选到两个平台，实际只选到 %v", before)
	}

	// 限制为只用 ZCode
	p.SetModelPlatforms(map[string][]string{"glm-5.2": {"zcode"}})
	after := pickProductFor(t, p, "glm-5.2", 60)

	if after[auth.ProductWorkBuddy] != 0 {
		t.Errorf("勾了「只用 ZCode」后仍选到 WorkBuddy %d 次 —— 需求未达成", after[auth.ProductWorkBuddy])
	}
	if after[auth.ProductZcode] == 0 {
		t.Errorf("应选到 ZCode，实际一次都没有：%v", after)
	}
}

// TestModelPlatformsEmptyMeansNoRestriction 不配置 = 现状不变。
//
// 这条防的是"加功能把老行为改坏"：没配置白名单的用户，
// 行为必须与加该功能之前**完全一致**。
func TestModelPlatformsEmptyMeansNoRestriction(t *testing.T) {
	wb := mkProductAuth("wb-1", auth.ProductWorkBuddy)
	zc := mkProductAuth("zc-1", auth.ProductZcode)

	// 基线：完全没有白名单
	p1 := newTestPool(t, wb, zc)
	p1.SetMultiProduct(true, 0)
	base := pickProductFor(t, p1, "glm-5.2", 60)

	// 设了一个**空 map**（等价于没配）
	p2 := newTestPool(t, mkProductAuth("wb-1", auth.ProductWorkBuddy), mkProductAuth("zc-1", auth.ProductZcode))
	p2.SetMultiProduct(true, 0)
	p2.SetModelPlatforms(map[string][]string{})
	empty := pickProductFor(t, p2, "glm-5.2", 60)

	if len(base) != len(empty) {
		t.Errorf("空白名单不该改变行为：基线选到 %v，空 map 选到 %v", base, empty)
	}

	// 配了**别的模型**时，这个模型仍不受限
	p3 := newTestPool(t, mkProductAuth("wb-1", auth.ProductWorkBuddy), mkProductAuth("zc-1", auth.ProductZcode))
	p3.SetMultiProduct(true, 0)
	p3.SetModelPlatforms(map[string][]string{"other-model": {"zcode"}})
	other := pickProductFor(t, p3, "glm-5.2", 60)
	if len(other) < 2 {
		t.Errorf("白名单只配了别的模型时，glm-5.2 不该受限，实际只选到 %v", other)
	}
}

// TestModelPlatformsFallsBackWhenAllExcluded 全被排除时**不硬失败**。
//
// 若白名单里的平台账号全都不可用（禁用/冷却），选号必须回退到全池 ——
// 宁可给一个"平台不符但能用"的账号，也不要让本来能成功的请求 503。
// 这与 PickForModelRegion 的取舍一致（它的注释里写了同样的理由）。
func TestModelPlatformsFallsBackWhenAllExcluded(t *testing.T) {
	wb := mkProductAuth("wb-1", auth.ProductWorkBuddy)
	zc := mkProductAuth("zc-1", auth.ProductZcode)
	p := newTestPool(t, wb, zc)
	p.SetMultiProduct(true, 0)

	// 只允许 ZCode，然后把 ZCode 那条禁用
	p.SetModelPlatforms(map[string][]string{"glm-5.2": {"zcode"}})
	p.Disable("zc-1", "测试：模拟该平台全不可用")

	// 仍应选出 WorkBuddy（回退），而不是 nil
	got := p.PickForModel("glm-5.2", nil)
	if got == nil {
		t.Fatal("白名单内账号全不可用时应回退到全池，而不是直接不给账号（那会 503）")
	}
	if got.Product != auth.ProductWorkBuddy {
		t.Errorf("应回退到 WorkBuddy，实际 %q", got.Product)
	}
}

// TestModelPlatformsEmptyProductTreatedAsWorkBuddy 空 Product 按 WorkBuddy 处理。
//
// ## 为什么这条很重要
//
// 老账号（加多产品之前导入的）没有 `Product` 字段，读出来是空串。
// 若空串不映射到 WorkBuddy，它们会被白名单**静默排除** ——
// 表现为"配了白名单之后，一堆老账号突然不参与路由了"，
// 而用户完全看不出原因。
func TestModelPlatformsEmptyProductTreatedAsWorkBuddy(t *testing.T) {
	legacy := mkProductAuth("legacy-1", "") // 老账号：Product 为空
	zc := mkProductAuth("zc-1", auth.ProductZcode)
	p := newTestPool(t, legacy, zc)
	p.SetMultiProduct(true, 0)

	p.SetModelPlatforms(map[string][]string{"glm-5.2": {auth.ProductWorkBuddy}})
	got := p.PickForModel("glm-5.2", nil)
	if got == nil {
		t.Fatal("应能选到老账号（空 Product 按 WorkBuddy 处理）")
	}
	if got.UID != "legacy-1" {
		t.Errorf("应选到老账号 legacy-1，实际 %q（空 Product 没被当作 WorkBuddy）", got.UID)
	}
}

// TestModelPlatformsMultiAllowed 勾多个平台 = 它们都可以。
func TestModelPlatformsMultiAllowed(t *testing.T) {
	wb := mkProductAuth("wb-1", auth.ProductWorkBuddy)
	zc := mkProductAuth("zc-1", auth.ProductZcode)
	p := newTestPool(t, wb, zc)
	p.SetMultiProduct(true, 0)

	p.SetModelPlatforms(map[string][]string{"glm-5.2": {auth.ProductWorkBuddy, auth.ProductZcode}})
	got := pickProductFor(t, p, "glm-5.2", 60)
	if len(got) < 2 {
		t.Errorf("勾了两个平台时两个都该能选到，实际 %v", got)
	}
}

// TestModelPlatformsGetReflectsSet 读回来的与设进去的一致（含拷贝隔离）。
func TestModelPlatformsGetReflectsSet(t *testing.T) {
	p := newTestPool(t, mkProductAuth("a", auth.ProductWorkBuddy))
	p.SetModelPlatforms(map[string][]string{"m": {"zcode", "qoder"}})

	got := p.ModelPlatforms()
	if len(got["m"]) != 2 {
		t.Fatalf("应读回 2 个平台，实际 %v", got)
	}
	// 改动读回来的切片**不该**影响内部状态（防外部误改）
	got["m"][0] = "mutated"
	if p.ModelPlatforms()["m"][0] == "mutated" {
		t.Error("ModelPlatforms 应返回拷贝 —— 否则调用方能改坏选号配置")
	}
}
