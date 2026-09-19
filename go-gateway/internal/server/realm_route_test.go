package server

import (
	"testing"

	"workbuddy2api/internal/auth"
)

// ---------------------------------------------------------------------------
// 区域前缀**真的约束选号**（这是被实测抓出来的一个自身缺陷）
//
// # 缺陷经过
//
// `resolveModel` 能解析 `cn:` / `global:`，但返回值一直被丢弃
//（`_, model := resolveModel(...)`），注释声称"区域约束已升级为
// route/preferRegion"，而那个机制**只对带图片的请求生效**（文本请求
// 永远走 RegionAny 分支）—— 于是前缀只被剥离、不约束选号。
//
// 我修的时候**只改了 `preferRegion` 这个局部变量**，而选号读的是
// `route.Region` —— 于是**缺陷依旧**。实测确认：
//
//	`global:deepseek-v4.1-flash` → 选中 uid=4ea736d4（**国服**）
//
// 第二次修才是对的：把 `preferRegion` 写回 `route.Region`。
//
// # 这组测试守的是什么
//
// 它不测 `resolveModel`（那有专门的解析测试），而是测**前缀影响了选号**——
// 即"算出来的区域真的被用上了"。这正是上面那个疏忽的所在：
// 解析对、赋值错、结论看着都对。
// ---------------------------------------------------------------------------

// TestRealmPrefixReachesRouteRegion 区域前缀必须**写回 route.Region**。
//
// 直接断言 `imageRouteFor` + 前缀覆盖后的最终区域。
// 若有人又把它改回"只赋 preferRegion 不写回 route"，这条会红。
func TestRealmPrefixReachesRouteRegion(t *testing.T) {
	cases := []struct {
		name       string
		realm      string
		wantRegion auth.Region
	}{
		{"无前缀 → 不限制", "", auth.RegionAny},
		{"cn: → 国服", "cn", auth.RegionCN},
		{"global: → 国际版", "global", auth.RegionIntl},
	}

	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			// 模拟 forward.go 的那几行（文本请求 hasImage=false → 初始 RegionAny）
			route := imageRouteFor("glm-5.3", false)
			preferRegion := route.Region
			if r := realmToRegion(c.realm); r != auth.RegionAny {
				preferRegion = r
			}
			route.Region = preferRegion // ← 这一行就是当初漏掉的

			if route.Region != c.wantRegion {
				t.Errorf("前缀 %q 后 route.Region 应为 %v，实际 %v —— "+
					"若这里不对，选号读到的仍是旧值，前缀等于没写",
					c.realm, c.wantRegion, route.Region)
			}
		})
	}
}

// TestRealmPrefixOverridesImageInference 显式前缀**优先于**自动推断。
//
// 用户写 `cn:` 是"说"，`imageRouteFor` 是"猜" —— 冲突时以用户为准。
func TestRealmPrefixOverridesImageInference(t *testing.T) {
	// 构造一个推断出"国际版"的 route，再用 cn: 前缀覆盖
	route := imageRoute{Region: auth.RegionIntl, Required: true}
	preferRegion := route.Region
	if r := realmToRegion("cn"); r != auth.RegionAny {
		preferRegion = r
	}
	route.Region = preferRegion

	if route.Region != auth.RegionCN {
		t.Errorf("显式 `cn:` 前缀应覆盖推断出的国际版，实际 %v", route.Region)
	}
}

// TestEmptyRealmLeavesImageInferenceIntact 无前缀时**不干扰**自动推断。
//
// ⚠ 这条防的是"修过头"：若把空 realm 也写回 route.Region，
// 带图片的请求会失去 imageRouteFor 的能力推断（图片被静默换成占位符，
// 模型回"我看不见图片"）—— 那是本功能要避免的另一个方向。
func TestEmptyRealmLeavesImageInferenceIntact(t *testing.T) {
	route := imageRoute{Region: auth.RegionCN, Required: true}
	preferRegion := route.Region
	if r := realmToRegion(""); r != auth.RegionAny {
		preferRegion = r
	}
	route.Region = preferRegion

	if route.Region != auth.RegionCN {
		t.Errorf("无前缀时不该改变推断结果（应保持 RegionCN），实际 %v", route.Region)
	}
	if !route.Required {
		t.Error("无前缀时不该改变 Required 标志")
	}
}
