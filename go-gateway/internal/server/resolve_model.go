package server

import (
	"strings"

	"workbuddy2api/internal/auth"
)

// resolveModel 解析模型名里的**路由前缀协议**：`[产品:][区域:]模型名`。
//
// # 协议（前缀大小写敏感，与既有行为一致）
//
//	"glm-5.3"                      → product="", realm="", bare=glm-5.3（不限制）
//	"cn:glm-5.3"                   → realm=cn,       bare=glm-5.3
//	"global:glm-5.3"               → realm=global,   bare=glm-5.3
//	"国际版:glm-5.3"                → realm=global,   bare=glm-5.3（中文别名）
//	"qoder:glm-5.3"                → product=qoder,  bare=glm-5.3
//	"qoder:global:glm-5.3"         → product=qoder, realm=global, bare=glm-5.3
//	"zcode:国际版:glm-5.3"          → product=zcode, realm=global, bare=glm-5.3
//	"deepseek:v3"                  → 都不匹配，原样返回（前段不在枚举内）
//
// # 为什么需要它（所有者的需求）
//
// 三个平台**有重名模型** —— `glm-5.3` 既在 ZCode 的套餐里、也在 WorkBuddy
// 的清单里。用户输入裸名时，账号池按「最早到期分层」选号，可能选中
// 一个**没有该模型资源包**的账号（ZCode 那条通道回 `1113 无可用资源包`）。
// 客户端用前缀显式指定平台/区域即可消除歧义。
//
// # 为什么加中文别名「国际版」
//
// 所有者原话就是「平台:国际版:模型名」。要求他改写成英文 `global`
// 是我们改他的需求，而不是满足它。两种写法都接受，成本仅一个枚举项。
//
// # 为什么无前缀时返回空串而非 "cn"
//
// 空串表示**不限制**，保持既有行为（老客户端不带前缀，
// 不应因此改变选号范围）。这一点很重要：若默认成 cn，
// 所有国际版账号会**静默从候选里消失**。
func resolveModel(model string) (product, realm, bare string) {
	rest := model
	// 最多剥两层前缀（产品 + 区域），顺序固定：先产品后区域。
	// 反过来的 `global:qoder:x` 也接受 —— 见下面的循环。
	for i := 0; i < 2; i++ {
		idx := strings.IndexByte(rest, ':')
		if idx < 0 {
			break
		}
		prefix := rest[:idx]
		tail := rest[idx+1:]

		switch {
		case product == "" && isProductPrefix(prefix):
			product = normalizeProduct(prefix)
			rest = tail
		case realm == "" && isRealmPrefix(prefix):
			realm = normalizeRealm(prefix)
			rest = tail
		default:
			// 前段既不是产品也不是区域 —— 整串原样保留。
			//
			// ⚠ 必须立刻停：`deepseek:v3` 里的 `deepseek` 不是前缀，
			// 若继续往下剥会把模型名本身吃掉（那正是既有注释
			// 「前段不在枚举内，不剥离」要防的事）。
			return product, realm, rest
		}
	}
	return product, realm, rest
}

// isProductPrefix 判断某段是否是**产品**前缀。
//
// 只认这三个（与 `auth.Product*` 常量一致）。
// 不接受 `workbuddy` 之外的别名 —— 产品标识是机器用的，不该有歧义。
func isProductPrefix(s string) bool {
	switch strings.ToLower(s) {
	case productWorkBuddy, productQoder, productZcode:
		return true
	}
	return false
}

// normalizeProduct 归一化产品前缀为小写标识。
//
// 产品名允许任意大小写（`Qoder:` / `QODER:` 都接受）——
// 与区域前缀的"大小写敏感"不同：区域是短英文词，容易与模型名混淆；
// 产品名是明确的品牌词，宽容更友好，且不会与模型名撞。
func normalizeProduct(s string) string {
	return strings.ToLower(strings.TrimSpace(s))
}

// isRealmPrefix 判断某段是否是**区域**前缀。
//
// 接受英文与中文别名：
//
//	cn / CN           → 国服
//	global / 国际版    → 国际版
func isRealmPrefix(s string) bool {
	return s == realmCN || s == realmGlobal || s == realmIntlCN
}

// normalizeRealm 归一化区域前缀为内部枚举值（`cn` / `global`）。
func normalizeRealm(s string) string {
	if s == realmIntlCN {
		return realmGlobal
	}
	return s
}

// product / realm 枚举。
//
// 产品标识与 `auth.ProductWorkBuddy` 等**必须一致** ——
// 它们最终会与账号的 `Product` 字段比较，拼错就是"永远选不中"。
const (
	productWorkBuddy = "workbuddy"
	productQoder     = "qoder"
	productZcode     = "zcode"
)

const (
	realmCN     = "cn"
	realmGlobal = "global"
	// realmIntlCN 是 `global` 的**中文别名**（所有者习惯写「国际版」）。
	realmIntlCN = "国际版"
)

// realmToRegion 把区域前缀映射成 `auth.Region`。
//
// 空串 → RegionAny（不限制）。这样调用方不必自己判空 ——
// 直接把结果赋给 preferRegion 即可，无前缀时自然保持"不限制"。
func realmToRegion(realm string) auth.Region {
	switch realm {
	case realmCN:
		return auth.RegionCN
	case realmGlobal:
		return auth.RegionIntl
	default:
		return auth.RegionAny
	}
}
