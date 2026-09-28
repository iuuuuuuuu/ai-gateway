package pool

// platform_models_test.go 锁定「平台 × 区域 → 允许的模型」白名单。
//
// # 守的是什么（2026-09-28 所有者要求）
//
// 他原话（三句，缺一不可）：
//
//	「在选号侧就是要挡请求」
//	「给每个平台手动配置支持的模型，而且要区分国内外版本」
//	「手动配置的+接口返回的,可不是以手动配置的为准」
//
// 现场：他发 `qoder:deepseek-v4.1-flash`。qoder 上游**实际上能服务**它
//（实测 10/10 全 200），但那份"能服务"的超集不等于"用户允许跑"。
//
// # 三个来源取并集（这是本文件的核心断言）
//
//	1. `platformModels`          用户手动配的（按平台 × 区域）
//	2. `productModelSet`         接口/刷新账号返回的
//	3. `productModelFallbackSet` 网关内置兜底
//
// 任一命中即放行。**不能**以手动配置为准 —— 用户通常只补几个，
// 以它为准会把接口返回的另外十几个全挡掉。

import (
	"testing"

	"workbuddy2api/internal/auth"
)

// newWhitelistPool 造一个有两个平台账号的池（不配任何清单）。
func newWhitelistPool(t *testing.T) *Pool {
	t.Helper()
	p := New("")
	p.Add(&auth.Auth{UID: "qd-cn", AccessToken: "t", Product: auth.ProductQoder, Domain: "qoder.com.cn"})
	p.Add(&auth.Auth{UID: "qd-intl", AccessToken: "t", Product: auth.ProductQoder, Domain: "qoder.sh"})
	p.Add(&auth.Auth{UID: "wb-cn", AccessToken: "t", Product: auth.ProductWorkBuddy, Domain: "www.workbuddy.cn"})
	p.SetMultiProduct(true, 0)
	return p
}

// TestPlatformWhitelistBlocksUnlisted 白名单配了但里面没有 → 拦。
func TestPlatformWhitelistBlocksUnlisted(t *testing.T) {
	p := newWhitelistPool(t)
	p.SetPlatformModels(map[string]map[string][]string{
		"qoder": {"cn": {"Qwen3.8-Flash"}},
	})

	allowed, configured := p.PlatformAllowsModel("qoder", auth.RegionCN, "deepseek-v4.1-flash")
	if !configured {
		t.Fatal("配了白名单，第二个返回值应为 true（表示「用户明确表态了」）")
	}
	if allowed {
		t.Error("白名单里没有 deepseek-v4.1-flash，应拦住" +
			"（否则会发出去、并在统计里记成「该模型 · qoder」）")
	}
}

// TestPlatformWhitelistAllowsListed 白名单里有 → 放行。
func TestPlatformWhitelistAllowsListed(t *testing.T) {
	p := newWhitelistPool(t)
	p.SetPlatformModels(map[string]map[string][]string{
		"qoder": {"cn": {"Qwen3.8-Flash", "DeepSeek-V4-Pro"}},
	})
	for _, m := range []string{"Qwen3.8-Flash", "qwen3.8-flash", "QWEN3.8-FLASH"} {
		if allowed, _ := p.PlatformAllowsModel("qoder", auth.RegionCN, m); !allowed {
			t.Errorf("%q 在白名单里（大小写不敏感），应放行", m)
		}
	}
}

// TestPlatformWhitelistUnionWithInterface ★ 并集：接口返回的也要放行。
//
// # 这条是最重要的断言
//
// 所有者原话：「手动配置的+接口返回的,可不是以手动配置的为准」。
//
// 场景：用户手动只补了 `deepseek-v4.1-flash`（接口没报这个），
// 而接口返回的 18 个模型**也要能用**。若以手动为准，那 18 个全被挡 ——
// 用户会看到"我刚配了一个，结果其他全不能用了"。
func TestPlatformWhitelistUnionWithInterface(t *testing.T) {
	p := newWhitelistPool(t)
	// 接口返回 18 个（这里取 3 个代表）
	p.SetProductModels(map[string][]string{
		"qoder": {"Qwen3.8-Flash", "Qwen3.8-Max", "GLM-5.3"},
	})
	// 用户手动只补 1 个接口没报的
	p.SetPlatformModels(map[string]map[string][]string{
		"qoder": {"cn": {"deepseek-v4.1-flash"}},
	})

	// 手动配的那个 → 放行
	if allowed, _ := p.PlatformAllowsModel("qoder", auth.RegionCN, "deepseek-v4.1-flash"); !allowed {
		t.Error("手动配的模型应放行")
	}
	// 接口返回的那 3 个 → **也要**放行（这是并集语义的核心）
	for _, m := range []string{"Qwen3.8-Flash", "Qwen3.8-Max", "GLM-5.3"} {
		if allowed, _ := p.PlatformAllowsModel("qoder", auth.RegionCN, m); !allowed {
			t.Errorf("接口返回的 %q 也必须放行 —— 不能以手动配置为准"+
				"（否则用户补一个模型会把其他全挡掉）", m)
		}
	}
	// 两边都没有的 → 拦
	if allowed, _ := p.PlatformAllowsModel("qoder", auth.RegionCN, "gpt-5.5"); allowed {
		t.Error("两边都没有的模型应拦住")
	}
}

// TestPlatformWhitelistUnionWithFallback 兜底清单也参与并集。
func TestPlatformWhitelistUnionWithFallback(t *testing.T) {
	p := newWhitelistPool(t)
	p.SetPlatformModels(map[string]map[string][]string{
		"qoder": {"cn": {"Qwen3.8-Flash"}},
	})
	p.SetProductModelsFallback(map[string][]string{
		"qoder": {"deepseek-v4.1-flash"},
	})
	if allowed, _ := p.PlatformAllowsModel("qoder", auth.RegionCN, "deepseek-v4.1-flash"); !allowed {
		t.Error("兜底清单里的模型应放行（兜底只放宽、不收紧）")
	}
}

// TestPlatformWhitelistRegionSeparated ★ 区分国内外版本。
//
// 所有者原话：「而且要区分国内外版本」。
//
// 同一个平台在国内/国际版能用的模型可能不同 —— 白名单必须按区域分开判。
func TestPlatformWhitelistRegionSeparated(t *testing.T) {
	p := newWhitelistPool(t)
	p.SetPlatformModels(map[string]map[string][]string{
		"qoder": {
			"cn":   {"Qwen3.8-Flash"},
			"intl": {"Qwen3.8-Max"},
		},
	})

	// 国服允许 Qwen3.8-Flash，不允许 Qwen3.8-Max
	if allowed, _ := p.PlatformAllowsModel("qoder", auth.RegionCN, "Qwen3.8-Flash"); !allowed {
		t.Error("国服白名单里有 Qwen3.8-Flash，应放行")
	}
	if allowed, _ := p.PlatformAllowsModel("qoder", auth.RegionCN, "Qwen3.8-Max"); allowed {
		t.Error("国服白名单里没有 Qwen3.8-Max（它在 intl 那份），应拦住 —— " +
			"这正是「要区分国内外版本」的含义")
	}
	// 国际版反过来
	if allowed, _ := p.PlatformAllowsModel("qoder", auth.RegionIntl, "Qwen3.8-Max"); !allowed {
		t.Error("国际版白名单里有 Qwen3.8-Max，应放行")
	}
	if allowed, _ := p.PlatformAllowsModel("qoder", auth.RegionIntl, "Qwen3.8-Flash"); allowed {
		t.Error("国际版白名单里没有 Qwen3.8-Flash（它在 cn 那份），应拦住")
	}
}

// TestPlatformWhitelistUnconfiguredDoesNotBlock 没配 → 不拦（向后兼容）。
//
// 老配置没有 `platform_models` 键。若把它当"什么都不允许"，
// 升级后所有平台都会变成"没有可用账号"—— 那是灾难性回归。
func TestPlatformWhitelistUnconfiguredDoesNotBlock(t *testing.T) {
	p := newWhitelistPool(t)
	// 完全不配
	allowed, configured := p.PlatformAllowsModel("qoder", auth.RegionCN, "anything")
	if !allowed {
		t.Error("没配白名单时不该拦（向后兼容：老配置没有这个键）")
	}
	if configured {
		t.Error("没配时第二个返回值应为 false（表示「用户没表态」）")
	}
}

// TestPlatformWhitelistOtherPlatformUnaffected 配了 A 平台不影响 B 平台。
func TestPlatformWhitelistOtherPlatformUnaffected(t *testing.T) {
	p := newWhitelistPool(t)
	p.SetPlatformModels(map[string]map[string][]string{
		"qoder": {"cn": {"Qwen3.8-Flash"}},
	})
	// workbuddy 没配 → 不拦
	if allowed, configured := p.PlatformAllowsModel("workbuddy", auth.RegionCN, "任意模型"); !allowed || configured {
		t.Error("只配了 qoder 的白名单，不该影响 workbuddy")
	}
}

// TestPlatformWhitelistEmptyMeansUnconfigured 清空白名单 = 不拦。
//
// 界面上把某平台的模型全删光，语义应是"没配白名单"，
// 而不是"该平台一个模型都不许用"。
func TestPlatformWhitelistEmptyMeansUnconfigured(t *testing.T) {
	p := newWhitelistPool(t)
	p.SetPlatformModels(map[string]map[string][]string{
		"qoder": {"cn": {}}, // 空清单
	})
	if allowed, configured := p.PlatformAllowsModel("qoder", auth.RegionCN, "x"); !allowed || configured {
		t.Error("空清单应视为「未配置」而不是「全禁」")
	}
	// 显式清空
	p.SetPlatformModels(nil)
	if allowed, configured := p.PlatformAllowsModel("qoder", auth.RegionCN, "x"); !allowed || configured {
		t.Error("SetPlatformModels(nil) 应清空约束")
	}
}

// TestPlatformWhitelistRegionAgnosticFallback 只配通用清单（`""` 键）时两个区域都认。
func TestPlatformWhitelistRegionAgnosticFallback(t *testing.T) {
	p := newWhitelistPool(t)
	p.SetPlatformModels(map[string]map[string][]string{
		"qoder": {"": {"Qwen3.8-Flash"}},
	})
	for _, r := range []auth.Region{auth.RegionCN, auth.RegionIntl} {
		if allowed, _ := p.PlatformAllowsModel("qoder", r, "Qwen3.8-Flash"); !allowed {
			t.Errorf("通用清单（不分区域）应同时适用于区域 %v", r)
		}
		if allowed, _ := p.PlatformAllowsModel("qoder", r, "别的"); allowed {
			t.Errorf("区域 %v：不在通用清单里应拦住", r)
		}
	}
}

// TestPlatformWhitelistBlocksPicking ★ 端到端：拦在**选号**这一步。
//
// 所有者要的正是这个 ——「在选号侧就是要挡请求」。
func TestPlatformWhitelistBlocksPicking(t *testing.T) {
	p := newWhitelistPool(t)
	p.SetPlatformModels(map[string]map[string][]string{
		"qoder": {"cn": {"Qwen3.8-Flash"}, "intl": {"Qwen3.8-Flash"}},
	})

	// qoder 白名单里没有该模型 → 选号侧就该选不出
	if got := p.PickForModelProductRegion("deepseek-v4.1-flash", nil, auth.RegionAny, "qoder"); got != nil {
		t.Errorf("白名单不允许时选号侧应选不出（否则请求会发出去），实际 %s", got.UID)
	}
	// 白名单里有的 → 能选出
	if got := p.PickForModelProductRegion("Qwen3.8-Flash", nil, auth.RegionAny, "qoder"); got == nil {
		t.Error("白名单允许的模型应能选出账号")
	}
	// 没配白名单的平台不受影响
	if got := p.PickForModelProductRegion("deepseek-v4.1-flash", nil, auth.RegionAny, "workbuddy"); got == nil {
		t.Error("workbuddy 没配白名单，不该被拦")
	}
}

// TestPlatformAllowedModelsIsUnion 文案里列的清单必须是**判据用的那份**。
//
// 否则会出现"它说允许 X，我发 X 却被挡"这种自相矛盾。
func TestPlatformAllowedModelsIsUnion(t *testing.T) {
	p := newWhitelistPool(t)
	p.SetProductModels(map[string][]string{"qoder": {"FromInterface"}})
	p.SetPlatformModels(map[string]map[string][]string{
		"qoder": {"cn": {"FromManual"}},
	})

	got := p.PlatformAllowedModels("qoder", auth.RegionCN)
	want := map[string]bool{"frommanual": true, "frominterface": true}
	if len(got) != len(want) {
		t.Fatalf("应是并集（%d 个），实际 %d 个：%v", len(want), len(got), got)
	}
	for _, m := range got {
		if !want[m] {
			t.Errorf("并集里出现意外项 %q（期望 %v）", m, want)
		}
		delete(want, m)
	}
	for m := range want {
		t.Errorf("并集里缺少 %q —— 文案必须列出**判据用的那份**，"+
			"否则会出现「它说允许 X，我发 X 却被挡」", m)
	}
}

// TestPlatformAllowedModelsNilWhenUnconfigured 未配置时返回 nil（不是空切片）。
func TestPlatformAllowedModelsNilWhenUnconfigured(t *testing.T) {
	p := newWhitelistPool(t)
	if got := p.PlatformAllowedModels("qoder", auth.RegionCN); got != nil {
		t.Errorf("未配置时应返回 nil（调用方据此不列清单），实际 %v", got)
	}
}

// =============================================================================
// 「禁用」否决项（platform_models_disabled）
// =============================================================================
//
// # 为什么需要它（这些用例的存在理由）
//
// 放行判据是**并集**：手动配置 ∪ 接口返回 ∪ 网关兜底，任一命中即放行。
// 于是「从手动清单里删掉一个模型」**删不掉** —— 它可能仍由"接口返回"
// 那一支放行。实测 `qoder:deepseek-v4.1-flash` 正是如此。
//
// 用户要的"禁用"是一个**独立于并集的否决项**，必须在并集**之前**判。
// 下面第 1~3 条分别钉住它压过三个来源中的每一个。

// TestPlatformDisabledBeatsInterfaceSource ★ 核心用例：禁用压过**接口返回**来源。
//
// 这是本功能存在的理由：接口清单里有 X、手动清单里也有 X，
// 两条来源都说"能用"，但用户显式禁用了 X ⇒ 必须**拦**。
//
// 若这条变红（返回 true），说明否决项被并集盖过了 —— 那么用户
// 无论如何都挡不住一个由"接口返回"放行的模型，本功能等于没做。
func TestPlatformDisabledBeatsInterfaceSource(t *testing.T) {
	p := newWhitelistPool(t)
	// 来源 1（手动）与来源 2（接口）**都**说 X 能用
	p.SetPlatformModels(map[string]map[string][]string{
		"qoder": {"cn": {"deepseek-v4.1-flash"}},
	})
	p.SetProductModels(map[string][]string{
		"qoder": {"deepseek-v4.1-flash", "Qwen3.8-Flash"},
	})
	// 用户显式禁用 X
	p.SetPlatformModelsDisabled(map[string]map[string][]string{
		"qoder": {"cn": {"deepseek-v4.1-flash"}},
	})

	allowed, configured := p.PlatformAllowsModel("qoder", auth.RegionCN, "deepseek-v4.1-flash")
	if allowed {
		t.Fatal("★ X 被显式禁用，即使手动配置与接口返回**都**放行它，也必须拦住 —— " +
			"否则用户永远挡不住一个由「接口返回」来源放行的模型（这正是本功能的存在理由）")
	}
	if !configured {
		t.Error("显式禁用时第二个返回值应为 true：这是「用户明确禁止」，" +
			"不是「用户没表态」—— 调用方要据此知道「该拦」而不是「不知道」")
	}

	// 没被禁用的那个不受影响（不能把整个平台都拦掉）
	if allowed, _ := p.PlatformAllowsModel("qoder", auth.RegionCN, "Qwen3.8-Flash"); !allowed {
		t.Error("只禁用了 X，接口返回的 Qwen3.8-Flash 应照常放行")
	}
}

// TestPlatformDisabledBeatsManualSource 禁用压过**手动配置**来源。
func TestPlatformDisabledBeatsManualSource(t *testing.T) {
	p := newWhitelistPool(t)
	p.SetPlatformModels(map[string]map[string][]string{
		"qoder": {"cn": {"Qwen3.8-Flash", "deepseek-v4.1-flash"}},
	})
	p.SetPlatformModelsDisabled(map[string]map[string][]string{
		"qoder": {"cn": {"deepseek-v4.1-flash"}},
	})

	if allowed, configured := p.PlatformAllowsModel("qoder", auth.RegionCN, "deepseek-v4.1-flash"); allowed || !configured {
		t.Errorf("手动清单里有 X、但 X 被禁用 ⇒ 应 (false, true)，实际 (%v, %v)", allowed, configured)
	}
	if allowed, _ := p.PlatformAllowsModel("qoder", auth.RegionCN, "Qwen3.8-Flash"); !allowed {
		t.Error("同一份手动清单里未被禁用的模型应照常放行")
	}
}

// TestPlatformDisabledBeatsFallbackSource 禁用压过**网关兜底**来源。
//
// 兜底是"猜测"，只用于放宽 —— 但用户显式禁用是**明确表态**，
// 猜测再宽也不该盖过它。
func TestPlatformDisabledBeatsFallbackSource(t *testing.T) {
	p := newWhitelistPool(t)
	p.SetProductModelsFallback(map[string][]string{
		"qoder": {"deepseek-v4.1-flash"},
	})
	p.SetPlatformModelsDisabled(map[string]map[string][]string{
		"qoder": {"cn": {"deepseek-v4.1-flash"}},
	})

	if allowed, configured := p.PlatformAllowsModel("qoder", auth.RegionCN, "deepseek-v4.1-flash"); allowed || !configured {
		t.Errorf("兜底清单里有 X、但 X 被禁用 ⇒ 应 (false, true)，实际 (%v, %v)", allowed, configured)
	}
}

// TestPlatformDisabledRegionSeparated 区域隔离：`cn` 禁用不影响 `intl`。
//
// 与白名单同口径（「要区分国内外版本」）—— 禁用也必须分区域。
func TestPlatformDisabledRegionSeparated(t *testing.T) {
	p := newWhitelistPool(t)
	// 两个区域的允许来源都含 X
	p.SetPlatformModels(map[string]map[string][]string{
		"qoder": {
			"cn":   {"deepseek-v4.1-flash"},
			"intl": {"deepseek-v4.1-flash"},
		},
	})
	// 只在国服禁用
	p.SetPlatformModelsDisabled(map[string]map[string][]string{
		"qoder": {"cn": {"deepseek-v4.1-flash"}},
	})

	if allowed, configured := p.PlatformAllowsModel("qoder", auth.RegionCN, "deepseek-v4.1-flash"); allowed || !configured {
		t.Errorf("国服禁用了 X ⇒ 国服应拦，实际 (%v, %v)", allowed, configured)
	}
	if allowed, _ := p.PlatformAllowsModel("qoder", auth.RegionIntl, "deepseek-v4.1-flash"); !allowed {
		t.Error("只禁用了国服那份，国际版应照常放行 —— 禁用也要区分国内外版本")
	}
}

// TestPlatformDisabledRegionAgnosticKey 不分区域键 `""` 的禁用对两个区域都生效。
func TestPlatformDisabledRegionAgnosticKey(t *testing.T) {
	p := newWhitelistPool(t)
	p.SetPlatformModels(map[string]map[string][]string{
		"qoder": {"": {"deepseek-v4.1-flash"}},
	})
	p.SetPlatformModelsDisabled(map[string]map[string][]string{
		"qoder": {"": {"deepseek-v4.1-flash"}},
	})

	for _, r := range []auth.Region{auth.RegionCN, auth.RegionIntl} {
		if allowed, configured := p.PlatformAllowsModel("qoder", r, "deepseek-v4.1-flash"); allowed || !configured {
			t.Errorf("区域 %v：`\"\"` 键里的禁用应对两个区域都生效，实际 (%v, %v)", r, allowed, configured)
		}
	}
}

// TestPlatformDisabledUnconfiguredChangesNothing 未配置禁用 ⇒ 行为与引入前逐字不变。
//
// 老配置没有 `platform_models_disabled` 键。nil 必须等于"没禁用任何东西"，
// 而不是"全禁" —— 后者会让升级后所有平台立刻不可用。
//
// 这里把并集的三种结果（放行 / 拦 / 不拦）都跑一遍，
// 断言与"引入禁用项之前"完全一致。
func TestPlatformDisabledUnconfiguredChangesNothing(t *testing.T) {
	p := newWhitelistPool(t)
	p.SetProductModels(map[string][]string{"qoder": {"FromInterface"}})
	p.SetPlatformModels(map[string]map[string][]string{"qoder": {"cn": {"FromManual"}}})

	// 显式调用一次 nil（模拟老配置缺键时的透传）
	p.SetPlatformModelsDisabled(nil)

	cases := []struct {
		model      string
		wantAllow  bool
		wantConfig bool
		why        string
	}{
		{"FromManual", true, true, "手动清单命中"},
		{"FromInterface", true, true, "接口返回命中"},
		{"Neither", false, true, "配了但都不含 ⇒ 拦"},
	}
	for _, c := range cases {
		allowed, configured := p.PlatformAllowsModel("qoder", auth.RegionCN, c.model)
		if allowed != c.wantAllow || configured != c.wantConfig {
			t.Errorf("%s：期望 (%v, %v)，实际 (%v, %v) —— 未配置禁用项时行为必须逐字不变",
				c.why, c.wantAllow, c.wantConfig, allowed, configured)
		}
	}

	// 完全没配任何来源的平台 ⇒ 仍是不拦
	if allowed, configured := p.PlatformAllowsModel("workbuddy", auth.RegionCN, "anything"); !allowed || configured {
		t.Error("一个来源都没配的平台：未配置禁用项时仍应 (true, false)（不拦）")
	}
}

// TestPlatformDisabledDeductedFromAllowedModels 文案里**不包含**被禁用的模型。
//
// 否则会出现"它说允许 X，我发 X 却被挡"—— 那正是 PlatformAllowedModels
// 的注释里明确要避免的自相矛盾。
func TestPlatformDisabledDeductedFromAllowedModels(t *testing.T) {
	p := newWhitelistPool(t)
	p.SetProductModels(map[string][]string{"qoder": {"FromInterface", "DisabledOne"}})
	p.SetPlatformModels(map[string]map[string][]string{"qoder": {"cn": {"FromManual"}}})
	p.SetPlatformModelsDisabled(map[string]map[string][]string{"qoder": {"cn": {"DisabledOne"}}})

	got := p.PlatformAllowedModels("qoder", auth.RegionCN)
	for _, m := range got {
		if m == "disabledone" {
			t.Errorf("文案里不该出现被禁用的模型（实际清单 %v）—— "+
				"它会说「允许 disabledone」而实际发出去会被拦", got)
		}
	}
	// 其余项必须保留（扣除不能扣多）
	want := map[string]bool{"frommanual": true, "frominterface": true}
	if len(got) != len(want) {
		t.Fatalf("应剩 %d 个，实际 %d 个：%v", len(want), len(got), got)
	}
	for _, m := range got {
		if !want[m] {
			t.Errorf("扣除后出现意外项 %q（期望 %v）", m, want)
		}
	}

	// 跨区域扣多也不行：intl 没禁用 ⇒ 文案里必须还在
	intl := p.PlatformAllowedModels("qoder", auth.RegionIntl)
	found := false
	for _, m := range intl {
		if m == "disabledone" {
			found = true
		}
	}
	if !found {
		t.Errorf("intl 没有禁用 disabledone，文案里应保留它（实际 %v）—— "+
			"扣除只认该区域那份 + 不分区域那份", intl)
	}
}

// TestPlatformDisabledEmptyMeansUnconfigured 空清单 = 未配置 = **不否决**。
//
// 与白名单同一口径：界面里把禁用列表清空，语义应是"没有禁用任何模型"，
// 而不是"该平台所有模型都禁用" —— 后者会让该平台彻底不可用。
func TestPlatformDisabledEmptyMeansUnconfigured(t *testing.T) {
	p := newWhitelistPool(t)
	p.SetProductModels(map[string][]string{"qoder": {"X"}})
	// 空清单
	p.SetPlatformModelsDisabled(map[string]map[string][]string{
		"qoder": {"cn": {}},
	})
	if allowed, configured := p.PlatformAllowsModel("qoder", auth.RegionCN, "X"); !allowed || !configured {
		t.Errorf("空清单应视为「未配置」而不是「全禁」：期望 (true, true)（接口返回命中），实际 (%v, %v)",
			allowed, configured)
	}

	// 显式清空
	p.SetPlatformModelsDisabled(map[string]map[string][]string{
		"qoder": {"cn": {"X"}},
	})
	if allowed, _ := p.PlatformAllowsModel("qoder", auth.RegionCN, "X"); allowed {
		t.Fatal("先确认禁用确实生效（这条若失败，下面的清空断言没有意义）")
	}
	p.SetPlatformModelsDisabled(nil)
	if allowed, configured := p.PlatformAllowsModel("qoder", auth.RegionCN, "X"); !allowed || !configured {
		t.Errorf("SetPlatformModelsDisabled(nil) 应清空否决项：期望 (true, true)，实际 (%v, %v)", allowed, configured)
	}
}

// TestPlatformDisabledBlocksPicking ★ 端到端：否决项在**选号**这一步生效。
//
// `PickForModelProductRegion` 的第 3 类排除集调的是
// `platformAllowsModelLocked`，故否决项应当**自动**生效 —— 这条用例
// 就是来确认"不用改 PickForModelProductRegion"这个判断的。
func TestPlatformDisabledBlocksPicking(t *testing.T) {
	p := newWhitelistPool(t)
	// 接口来源放行 X（并集里 X 是"允许"的）
	p.SetProductModels(map[string][]string{"qoder": {"deepseek-v4.1-flash", "Qwen3.8-Flash"}})
	// 但用户禁用了它
	p.SetPlatformModelsDisabled(map[string]map[string][]string{
		"qoder": {"cn": {"deepseek-v4.1-flash"}, "intl": {"deepseek-v4.1-flash"}},
	})

	if got := p.PickForModelProductRegion("deepseek-v4.1-flash", nil, auth.RegionAny, "qoder"); got != nil {
		t.Errorf("★ 被禁用的模型在选号侧必须选不出（否则请求会发出去），实际选到 %s", got.UID)
	}
	// 没被禁用的照常能选
	if got := p.PickForModelProductRegion("Qwen3.8-Flash", nil, auth.RegionAny, "qoder"); got == nil {
		t.Error("未被禁用的模型应照常能选出账号")
	}
	// 别的平台不受影响
	if got := p.PickForModelProductRegion("deepseek-v4.1-flash", nil, auth.RegionAny, "workbuddy"); got == nil {
		t.Error("只禁用了 qoder，workbuddy 不该被影响")
	}
}
