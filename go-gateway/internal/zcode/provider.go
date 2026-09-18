package zcode

// provider.go 两个服务商的端点表。
//
// ## 为什么是"服务商"而不是"区域"
//
// Qoder 的国服/国际版是**同一产品的两个区域**（同一套协议、不同域名）。
// ZCode 的 Z.AI 与智谱是**两个不同服务商**：
//
//   · 域名不同（api.z.ai vs open.bigmodel.cn）
//   · 凭证来源不同（各自的控制台）
//   · 凭证形态略有差异（Z.AI 的 secret 必需；智谱可选）
//   · 但**协议相同**（同一套 OpenAI / Anthropic 端点路径）
//
// 故用 Provider 而非 Region 表达。
//
// ## 端点表（实测确认）
//
// 两个 host 都实测可达：
//
//	api.z.ai/api/coding/paas/v4/chat/completions       → 401（凭证错误，说明路径对）
//	open.bigmodel.cn/api/coding/paas/v4/...            → 401
//
// 参考实现里智谱的 host 有两处不一致的写法（`bigmodel.cn` 与
// `open.bigmodel.cn`）—— 实测**两者都可达且返回相同**，本包统一用
// `open.bigmodel.cn`（与 providers.ts 一致）。

import "strings"

// Provider 服务商标识。
type Provider string

const (
	// ProviderZAI Z.AI（z.ai）。
	ProviderZAI Provider = "zai"
	// ProviderBigmodel 智谱 BigModel。
	ProviderBigmodel Provider = "bigmodel"
	// ProviderUnknown 未知 —— **不猜**。
	//
	// 为什么要有这个态：凭证里可能没有 provider 字段（用户手写的扁平形），
	// 这时若默认成某一个，另一个服务商的账号会被打到错误的端点、必然失败。
	// 让调用方显式决定（界面让用户选），比静默猜错好。
	ProviderUnknown Provider = ""
)

// ParseProvider 把字符串解析成 Provider（大小写不敏感，容错常见写法）。
func ParseProvider(s string) Provider {
	switch strings.ToLower(strings.TrimSpace(s)) {
	case "zai", "z.ai", "z-ai", "z_ai":
		return ProviderZAI
	case "bigmodel", "big-model", "big_model", "zhipu", "智谱", "glm":
		return ProviderBigmodel
	default:
		return ProviderUnknown
	}
}

// Label 面向用户的服务商名。
func (p Provider) Label() string {
	switch p {
	case ProviderZAI:
		return "Z.AI"
	case ProviderBigmodel:
		return "智谱"
	default:
		return "未指定"
	}
}

// endpoints 一个服务商的全部端点。
type endpoints struct {
	// OpenAIBase OpenAI 兼容端点基址（**我们只走这个**）。
	OpenAIBase string
	// AnthropicBase Anthropic 端点基址（保留：将来若需要可切换）。
	AnthropicBase string
	// BizHost 业务 API host（凭证兑换、额度查询用）。
	BizHost string
}

// endpointsOf 返回服务商的端点表。
//
// 未知服务商**回落到 Z.AI**：这是**有意的**，因为未知多半是用户手写的
// 凭证（Z.AI 更常见）；但调用方应在界面上让用户确认，而不是让它悄悄跑错
//（见 ProviderUnknown 的注释）。
func (p Provider) endpointsOf() endpoints {
	switch p {
	case ProviderBigmodel:
		return endpoints{
			OpenAIBase:    "https://open.bigmodel.cn/api/coding/paas/v4",
			AnthropicBase: "https://open.bigmodel.cn/api/anthropic",
			BizHost:       "https://open.bigmodel.cn",
		}
	default:
		return endpoints{
			OpenAIBase:    "https://api.z.ai/api/coding/paas/v4",
			AnthropicBase: "https://api.z.ai/api/anthropic",
			BizHost:       "https://api.z.ai",
		}
	}
}

// OpenAIBase / AnthropicBase / BizHost 便捷访问器。
func (p Provider) OpenAIBase() string    { return p.endpointsOf().OpenAIBase }
func (p Provider) AnthropicBase() string { return p.endpointsOf().AnthropicBase }
func (p Provider) BizHost() string       { return p.endpointsOf().BizHost }

// AllProviders 全部已知服务商（供界面渲染选项）。
func AllProviders() []Provider { return []Provider{ProviderZAI, ProviderBigmodel} }
