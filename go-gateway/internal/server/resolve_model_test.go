package server

import (
	"testing"

	"workbuddy2api/internal/auth"
)

// ---------------------------------------------------------------------------
// 模型名前缀协议回归：`[产品:][区域:]模型名`
//
// 为什么需要：三个平台**有重名模型**（`glm-5.3` 既在 ZCode 套餐里、
// 也在 WorkBuddy 清单里），且同一模型名在两个区域可能是不同的服务。
// 账号池默认按「最早到期分层」选号，可能选中一个**没有该模型资源包**
// 的账号 —— 那条通道回 `1113 无可用资源包`，用户看到的是"余额不足"。
// 客户端用前缀显式指定平台/区域即可消除歧义。
// ---------------------------------------------------------------------------

// TestResolveModelPrefix 区域前缀（既有协议，必须保持兼容）。
func TestResolveModelPrefix(t *testing.T) {
	cases := []struct {
		in          string
		wantProduct string
		wantRealm   string
		wantBare    string
	}{
		{"cn:glm-5.2", "", "cn", "glm-5.2"},
		{"global:gpt-5.4", "", "global", "gpt-5.4"},
		// 无前缀 = 不限制（保持既有行为）
		{"glm-5.2", "", "", "glm-5.2"},
		// 前段不在枚举内：不剥离（模型名本身可能含冒号）
		{"deepseek:v3", "", "", "deepseek:v3"},
		// 区域前缀大小写敏感：不做归一（与既有行为一致）
		{"GLOBAL:gpt-5", "", "", "GLOBAL:gpt-5"},
		{"Cn:glm", "", "", "Cn:glm"},
	}
	for _, c := range cases {
		p, r, bare := resolveModel(c.in)
		if p != c.wantProduct || r != c.wantRealm || bare != c.wantBare {
			t.Errorf("resolveModel(%q)=(%q,%q,%q) want (%q,%q,%q)",
				c.in, p, r, bare, c.wantProduct, c.wantRealm, c.wantBare)
		}
	}
}

// TestResolveModelProductPrefix 产品前缀（本次新增）。
func TestResolveModelProductPrefix(t *testing.T) {
	cases := []struct {
		in          string
		wantProduct string
		wantRealm   string
		wantBare    string
	}{
		{"qoder:glm-5.3", "qoder", "", "glm-5.3"},
		{"zcode:glm-5.3", "zcode", "", "glm-5.3"},
		{"workbuddy:deepseek-v4.1-flash", "workbuddy", "", "deepseek-v4.1-flash"},
		// 产品名允许大小写（品牌词，不会与模型名撞）
		{"Qoder:glm-5.3", "qoder", "", "glm-5.3"},
		{"ZCODE:glm-5.3", "zcode", "", "glm-5.3"},
	}
	for _, c := range cases {
		p, r, bare := resolveModel(c.in)
		if p != c.wantProduct || r != c.wantRealm || bare != c.wantBare {
			t.Errorf("resolveModel(%q)=(%q,%q,%q) want (%q,%q,%q)",
				c.in, p, r, bare, c.wantProduct, c.wantRealm, c.wantBare)
		}
	}
}

// TestResolveModelProductAndRealm 产品 + 区域**两段**（所有者要的完整写法）。
//
// 这是他的原话「平台:国际版:模型名」对应的形态。
func TestResolveModelProductAndRealm(t *testing.T) {
	cases := []struct {
		in          string
		wantProduct string
		wantRealm   string
		wantBare    string
	}{
		{"qoder:global:glm-5.3", "qoder", "global", "glm-5.3"},
		{"zcode:cn:glm-5.3", "zcode", "cn", "glm-5.3"},
		{"workbuddy:global:deepseek-v4.1-flash", "workbuddy", "global", "deepseek-v4.1-flash"},
		// 顺序反了也应接受（用户不该被"必须先产品后区域"绊住）
		{"global:qoder:glm-5.3", "qoder", "global", "glm-5.3"},
	}
	for _, c := range cases {
		p, r, bare := resolveModel(c.in)
		if p != c.wantProduct || r != c.wantRealm || bare != c.wantBare {
			t.Errorf("resolveModel(%q)=(%q,%q,%q) want (%q,%q,%q)",
				c.in, p, r, bare, c.wantProduct, c.wantRealm, c.wantBare)
		}
	}
}

// TestResolveModelChineseRealmAlias 中文别名「国际版」。
//
// # 为什么必须支持
//
// 所有者原话就是「平台:国际版:模型名」。要求他改写成英文 `global`
// 是**改他的需求**，而不是满足它。两种写法都接受，成本仅一个枚举项。
func TestResolveModelChineseRealmAlias(t *testing.T) {
	cases := []struct {
		in          string
		wantProduct string
		wantRealm   string
		wantBare    string
	}{
		{"国际版:glm-5.3", "", "global", "glm-5.3"},
		{"qoder:国际版:glm-5.3", "qoder", "global", "glm-5.3"},
		{"zcode:国际版:glm-5.3", "zcode", "global", "glm-5.3"},
	}
	for _, c := range cases {
		p, r, bare := resolveModel(c.in)
		if p != c.wantProduct || r != c.wantRealm || bare != c.wantBare {
			t.Errorf("resolveModel(%q)=(%q,%q,%q) want (%q,%q,%q)",
				c.in, p, r, bare, c.wantProduct, c.wantRealm, c.wantBare)
		}
	}
}

// TestResolveModelEdge 边界：空串、仅冒号、空前缀、空裸名。
func TestResolveModelEdge(t *testing.T) {
	cases := []struct {
		in       string
		wantProd string
		wantReal string
		wantBare string
	}{
		{"", "", "", ""},
		{":", "", "", ":"},           // 空前缀不匹配任何枚举
		{":model", "", "", ":model"}, // 同上
		{"cn:", "", "cn", ""},        // 前缀合法 + 空裸名仍剥离
		{"global:", "", "global", ""},
		{"qoder:", "qoder", "", ""},
		{"cn:a:b", "", "cn", "a:b"},           // 只剥一层区域
		{"qoder:cn:a:b", "qoder", "cn", "a:b"}, // 剥两层后剩下的原样保留
	}
	for _, c := range cases {
		p, r, bare := resolveModel(c.in)
		if p != c.wantProd || r != c.wantReal || bare != c.wantBare {
			t.Errorf("resolveModel(%q)=(%q,%q,%q) want (%q,%q,%q)",
				c.in, p, r, bare, c.wantProd, c.wantReal, c.wantBare)
		}
	}
}

// TestResolveModelDoesNotEatUnknownPrefix 未知前缀**绝不**被剥掉。
//
// # 这条守的是什么
//
// 循环剥前缀若写成"无脑按冒号切分"，`deepseek:v3` 会被剥成 `v3` ——
// 模型名被吃掉，用户看到的是"模型不存在"，而原因在网关。
// 既有注释明确写了「前段不在枚举内，不剥离」，本条把它钉死。
func TestResolveModelDoesNotEatUnknownPrefix(t *testing.T) {
	for _, in := range []string{
		"deepseek:v3",
		"foo:bar",
		"some-vendor:model:v2",
		"a:b:c",
	} {
		p, r, bare := resolveModel(in)
		if p != "" || r != "" || bare != in {
			t.Errorf("resolveModel(%q)=(%q,%q,%q)：未知前缀必须原样保留，不能吃掉模型名",
				in, p, r, bare)
		}
	}
}

// TestResolveModelReturnsProductFirst 锁住返回顺序 `(product, realm, bare)`。
//
// 这是真实踩过的坑：写成 `model, realm := resolveModel(...)` 会静默交换，
// 表现为「单一模型锁定」报「收到的是 (未指定)」—— 因为 model 变成了空串。
// 该缺陷**不会导致编译错误**，只能靠测试或线上现象发现。
func TestResolveModelReturnsProductFirst(t *testing.T) {
	first, second, third := resolveModel("qoder:global:glm-5.3")
	if first != "qoder" {
		t.Errorf("第一个返回值应是 product（qoder），实际 %q", first)
	}
	if second != "global" {
		t.Errorf("第二个返回值应是 realm（global），实际 %q", second)
	}
	if third != "glm-5.3" {
		t.Errorf("第三个返回值应是裸模型名（glm-5.3），实际 %q", third)
	}
}

// TestRealmToRegion 区域前缀 → auth.Region 的映射。
//
// 空串必须映射成 RegionAny（不限制）—— 若误映射成 CN，
// 所有国际版账号会**静默从候选里消失**（用户看到"没有可用账号"，
// 而他明明有国际版号）。
func TestRealmToRegion(t *testing.T) {
	cases := map[string]auth.Region{
		"":       auth.RegionAny,
		"cn":     auth.RegionCN,
		"global": auth.RegionIntl,
		"bogus":  auth.RegionAny,
	}
	for in, want := range cases {
		if got := realmToRegion(in); got != want {
			t.Errorf("realmToRegion(%q)=%v want %v", in, got, want)
		}
	}
}

// TestProductPrefixConstantsMatchAuth 产品前缀常量必须与 auth 包一致。
//
// 它们最终会与账号的 `ProductOf()` 比较 —— 拼错就是"永远选不中"，
// 而那种错误**不会编译失败**（两边都是字符串）。
func TestProductPrefixConstantsMatchAuth(t *testing.T) {
	pairs := []struct {
		ours string
		auth string
	}{
		{productWorkBuddy, auth.ProductWorkBuddy},
		{productQoder, auth.ProductQoder},
		{productZcode, auth.ProductZcode},
	}
	for _, p := range pairs {
		if p.ours != p.auth {
			t.Errorf("产品常量不一致：server 包 %q vs auth 包 %q —— 会导致该平台永远选不中",
				p.ours, p.auth)
		}
	}
}
