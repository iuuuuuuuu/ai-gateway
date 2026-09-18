package qoder

import "strings"

// Region 服务区域。
//
// 为什么需要它：**参考实现只支持国服**（硬编码 qoder.com.cn），
// 而实测发现国际版是独立的一套域名、且协议同构 ——
// 本机安装的 Qoder 客户端用的就是国际版（endpoint 缓存实测为 api3.qoder.sh）。
//
// 实测依据（2026-09-18，逐项 HTTP 探测）：
//
//	项            国服                          国际版
//	────────────  ────────────────────────────  ──────────────────────────────
//	授权页         qoder.com.cn/device/...        qoder.com/device/...
//	openapi       openapi.qoder.com.cn          openapi.qoder.sh
//	gateway       gateway.qoder.com.cn          api3.qoder.sh
//	同一 client_id 302 进登录页 ✓                 302 进登录页 ✓
//
// 最后一行是关键：**同一个 client_id 在两区都被接受**，所以只需把域名做成
// 配置项即可支持双区，不必为国际版单独申请凭据。
//
// ⚠ 注意国际版的**授权页**在 qoder.com，而 API 在 qoder.sh ——
// 这两者不同域，不要"统一"成一个（实测 qoder.sh 的网页不存在，DNS 都不解析）。
type Region string

const (
	// RegionCN 国服。
	RegionCN Region = "cn"
	// RegionIntl 国际版。
	RegionIntl Region = "intl"
	// RegionUnknown 未知 —— **不猜**。
	//
	// 为什么要有这个态：凭证里可能没有 domain（用户手写的扁平形），
	// 这时若默认成国服，国际版账号会被打到国服端点、必然失败。
	// 让调用方显式决定（界面让用户选 / 导入时问），比静默猜错好。
	RegionUnknown Region = ""
)

// RegionFromDomain 按登录域名判断区域。
//
// 判定依据与 WorkBuddy 侧的口径一致（看后缀），但 Qoder 的域名完全不同：
// 国服 .com.cn、国际版 .sh 与 .com。
//
// 实测：国际版的 API 域是 qoder.sh，但**授权页**是 qoder.com；
// 而 qoder.com 也可能是国际版的另一种写法，故两者都归为国际版。
func RegionFromDomain(domain string) Region {
	d := strings.ToLower(strings.TrimSpace(domain))
	if d == "" {
		return RegionUnknown
	}
	switch {
	case strings.HasSuffix(d, ".com.cn"), strings.HasSuffix(d, "qoder.com.cn"):
		return RegionCN
	case strings.HasSuffix(d, ".sh"), strings.HasSuffix(d, "qoder.sh"):
		return RegionIntl
	case strings.HasSuffix(d, "qoder.com"), d == "qoder.com":
		// qoder.com 是国际版授权页；不是 .com.cn，故不算国服。
		return RegionIntl
	default:
		return RegionUnknown
	}
}

// endpoints 一个区域的全部端点。
type endpoints struct {
	// Website 授权页基址（浏览器打开的那个）。
	Website string
	// OpenAPI 轻量接口（额度、套餐、用户信息、令牌刷新）。
	// 这些用普通 Bearer，**不需要 COSY 签名**。
	OpenAPI string
	// Gateway 业务接口（模型列表、对话）。**需要 COSY 签名**。
	Gateway string
}

// endpointsOf 返回该区域的端点表。
//
// 未知区域回落到国服：这是**有意的**，因为未知区域的凭证多半是手写的国服文件；
// 但调用方应在界面上让用户确认，而不是让它悄悄跑错（见 RegionUnknown 的注释）。
func (r Region) endpointsOf() endpoints {
	switch r {
	case RegionIntl:
		return endpoints{
			Website: "https://qoder.com",
			OpenAPI: "https://openapi.qoder.sh",
			Gateway: "https://api3.qoder.sh",
		}
	default:
		return endpoints{
			Website: "https://qoder.com.cn",
			OpenAPI: "https://openapi.qoder.com.cn",
			Gateway: "https://gateway.qoder.com.cn",
		}
	}
}

// Domain 返回该区域的**主域名**（写入凭证文件的 domain 字段）。
//
// 用 API 域的根域（qoder.sh / qoder.com.cn），因为下游是按它判区域的
//（RegionFromDomain 接受 .sh 与 .com.cn 后缀）。
func (r Region) Domain() string {
	if r == RegionIntl {
		return "qoder.sh"
	}
	return "qoder.com.cn"
}

// Label 面向用户的区域名。
func (r Region) Label() string {
	switch r {
	case RegionIntl:
		return "国际版"
	case RegionCN:
		return "国服"
	default:
		return "未指定"
	}
}

// Website / OpenAPI / Gateway 便捷访问器（避免调用方到处写 endpointsOf()）。
func (r Region) Website() string { return r.endpointsOf().Website }
func (r Region) OpenAPI() string { return r.endpointsOf().OpenAPI }
func (r Region) Gateway() string { return r.endpointsOf().Gateway }
