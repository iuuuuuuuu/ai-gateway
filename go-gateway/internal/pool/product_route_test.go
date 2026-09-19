package pool

import (
	"testing"

	"workbuddy2api/internal/auth"
)

// ---------------------------------------------------------------------------
// 按**产品**约束选号的回归（所有者需求：`平台:模型名`）
//
// 背景：三个平台有**重名模型**（实测：glm-5.3 / glm-5.2 / glm-5.3-flash /
// glm-5.1 各来自 workbuddy + zcode）。裸名请求可能选中一个**没有该模型
// 资源包**的平台，那条通道回 `1113 无可用资源包`，用户看到的是"余额不足"。
//
// 客户端用 `zcode:glm-5.3` 显式指定平台即可消除歧义。
//
// ⚠ 这些用例覆盖的是**池层**的过滤逻辑。端到端（真实账号 + 真实请求）
// 由 `uitest/verify-model-prefix-live.cjs` 覆盖 —— 两者互补：
// 这里保证"给定池子状态，选号正确"，那里保证"真实上游真的能通"。
// ---------------------------------------------------------------------------

// newProductPool 建一个含三种产品的池。
func newProductPool(t *testing.T) *Pool {
	t.Helper()
	p := New("")
	p.Add(&auth.Auth{UID: "wb-1", AccessToken: "t", Product: auth.ProductWorkBuddy})
	p.Add(&auth.Auth{UID: "qd-1", AccessToken: "t", Product: auth.ProductQoder})
	p.Add(&auth.Auth{UID: "zc-1", AccessToken: "t", Product: auth.ProductZcode})
	return p
}

// TestPickForModelProductRegionFiltersByProduct 产品前缀只选该产品的账号。
func TestPickForModelProductRegionFiltersByProduct(t *testing.T) {
	cases := []struct {
		product string
		wantUID string
	}{
		{"workbuddy", "wb-1"},
		{"qoder", "qd-1"},
		{"zcode", "zc-1"},
	}
	for _, c := range cases {
		p := newProductPool(t)
		got := p.PickForModelProductRegion("glm-5.3", nil, auth.RegionAny, c.product)
		if got == nil {
			t.Errorf("product=%q 应能选出账号，实际 nil", c.product)
			continue
		}
		if got.UID != c.wantUID {
			t.Errorf("product=%q 应选中 %s，实际 %s", c.product, c.wantUID, got.UID)
		}
	}
}

// TestPickForModelProductRegionEmptyMeansNoConstraint 空产品 = 不限制（老客户端行为不变）。
//
// ⚠ 这条守的是**向后兼容**：老客户端不带前缀，选号范围不该因此改变。
// 若空串被当成"某个产品"，那些客户端会突然选不出号。
func TestPickForModelProductRegionEmptyMeansNoConstraint(t *testing.T) {
	p := newProductPool(t)
	got := p.PickForModelProductRegion("glm-5.3", nil, auth.RegionAny, "")
	if got == nil {
		t.Fatal("空产品必须表示**不限制**，应能选出账号")
	}
}

// TestPickForModelProductRegionNoMatchReturnsNil 该产品没有账号时返回 nil（不兜底跨平台）。
//
// # 为什么**不能**兜底
//
// 用户写 `qoder:` 就是明确说"只走这个平台"（他可能知道别的平台没额度）。
// 若我们兜底跨平台，他会看到"我明明指定了 qoder，却报了 ZCode 的 1113"——
// 那比直接说"没有可用的 qoder 账号"更难排查。
func TestPickForModelProductRegionNoMatchReturnsNil(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "wb-1", AccessToken: "t", Product: auth.ProductWorkBuddy})

	got := p.PickForModelProductRegion("glm-5.3", nil, auth.RegionAny, "zcode")
	if got != nil {
		t.Errorf("池里没有 zcode 账号时**必须返回 nil**（不能兜底给 workbuddy），实际选中 %s", got.UID)
	}
}

// TestPickForModelProductRegionLegacyEmptyProductCountsAsWorkBuddy 老账号的空 Product 算 WorkBuddy。
//
// # 这条防的是一个很容易写出的缺陷
//
// `auth.Auth.Product` 是后加的字段，老账号是**空串**。
// 若过滤时裸读 `e.a.Product != product`，则 `workbuddy:` 前缀下
// 所有老账号都被判成"不符" —— 表现为"指定了 workbuddy 却一个号都选不出"，
// 而池里明明全是它。必须用 `ProductOf()`（空串 → workbuddy）。
func TestPickForModelProductRegionLegacyEmptyProductCountsAsWorkBuddy(t *testing.T) {
	p := New("")
	// 故意不设 Product（模拟老账号）
	p.Add(&auth.Auth{UID: "legacy-1", AccessToken: "t"})

	got := p.PickForModelProductRegion("glm-5.3", nil, auth.RegionAny, "workbuddy")
	if got == nil {
		t.Fatal("老账号（Product 为空串）必须被当作 WorkBuddy —— 否则 `workbuddy:` 前缀会选不出任何号")
	}
	if got.UID != "legacy-1" {
		t.Errorf("应选中 legacy-1，实际 %s", got.UID)
	}
}

// TestPickByUIDForModelProductRegionRejectsWrongProduct 粘性会话路径也要校验产品。
//
// # 这是我实测发现的真实漏洞
//
// `forward.go` 的选号顺序是「先试粘性会话绑定的账号，绑不上才走产品过滤」。
// 修复前粘性那一支**没有产品校验**，于是 `zcode:glm-5.3` 会继续用该会话
// 此前绑定的 WorkBuddy 账号 —— 实测确认：
//
//	`zcode:glm-5.3` → 选中 e2891116（**workbuddy**）→ 报"额度已耗尽"
//
// 用户看到的是"我明明指定了 zcode，却报了 WorkBuddy 账号的错"。
func TestPickByUIDForModelProductRegionRejectsWrongProduct(t *testing.T) {
	p := newProductPool(t)

	// 绑定一个 workbuddy 账号，却要求走 zcode → 必须拒绝
	if got := p.PickByUIDForModelProductRegion("wb-1", "glm-5.3", auth.RegionAny, "zcode"); got != nil {
		t.Errorf("粘性绑定的账号产品不符时**必须返回 nil**（让上层解绑重选），实际返回 %s", got.UID)
	}

	// 产品相符 → 正常返回
	if got := p.PickByUIDForModelProductRegion("zc-1", "glm-5.3", auth.RegionAny, "zcode"); got == nil {
		t.Error("产品相符时应正常返回该账号")
	} else if got.UID != "zc-1" {
		t.Errorf("应返回 zc-1，实际 %s", got.UID)
	}

	// 空产品 = 不校验产品（向后兼容）
	if got := p.PickByUIDForModelProductRegion("wb-1", "glm-5.3", auth.RegionAny, ""); got == nil {
		t.Error("空产品应跳过产品校验（老客户端行为不变）")
	}
}

// TestPickForModelProductRegionCombinesWithRegion 产品 + 区域**同时**约束。
//
// 所有者要的完整写法 `平台:国际版:模型名` 对应这个组合。
func TestPickForModelProductRegionCombinesWithRegion(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "wb-cn", AccessToken: "t", Product: auth.ProductWorkBuddy, Domain: "www.workbuddy.cn"})
	p.Add(&auth.Auth{UID: "wb-intl", AccessToken: "t", Product: auth.ProductWorkBuddy, Domain: "www.workbuddy.ai"})
	p.Add(&auth.Auth{UID: "zc-cn", AccessToken: "t", Product: auth.ProductZcode})

	// workbuddy + 国际版 → 只能选 wb-intl
	got := p.PickForModelProductRegion("glm-5.3", nil, auth.RegionIntl, "workbuddy")
	if got == nil {
		t.Fatal("workbuddy + 国际版 应能选出 wb-intl")
	}
	if got.UID != "wb-intl" {
		t.Errorf("应选中 wb-intl，实际 %s（区域或产品约束之一没生效）", got.UID)
	}

	// zcode + 国际版 → zcode 那个号没有 Domain（默认国服）→ 应选不出
	if got := p.PickForModelProductRegion("glm-5.3", nil, auth.RegionIntl, "zcode"); got != nil {
		t.Errorf("zcode 没有国际版账号时应返回 nil，实际 %s", got.UID)
	}
}
