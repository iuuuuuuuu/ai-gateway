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
	// BizHost 业务 API host（**凭证兑换**用，如 z/login、getCustomerInfo）。
	BizHost string
	// QuotaHost 额度/账单查询 host。
	//
	// ## ⚠ 为什么它必须与 BizHost 分开（实测踩到）
	//
	// 我最初让额度查询复用 `BizHost`，于是 bigmodel 账号发去了
	// `open.bigmodel.cn/api/v1/zcode-plan/billing/balance` —— 上游回
	//
	//	HTTP 200 {"code":500,"msg":"404 NOT_FOUND"}
	//
	// 那个响应**是 200**，故不会被 `if resp.StatusCode >= 400` 拦下，
	// 会被当成"拿到了数据但额度为空" —— 界面显示「额度 0」/「未知」，
	// 而用户明明有 3 亿 token。这类"错误被包装成成功"的失败最难发现。
	//
	// 实测（uitest/diag-balance-endpoint.cjs）三个 host 对照：
	//
	//	zcode.z.ai          → code=0，完整额度 ✓
	//	open.bigmodel.cn    → code=500 404 NOT_FOUND ✗
	//	api.z.ai            → code=500 404 NOT_FOUND ✗
	//
	// 即**两个服务商的额度查询都走 zcode.z.ai** —— 那是统一的账单网关，
	// 与凭证兑换（各服务商自己的 host）不是一回事。
	QuotaHost string
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
			QuotaHost:     QuotaHost,
		}
	default:
		return endpoints{
			OpenAIBase:    "https://api.z.ai/api/coding/paas/v4",
			AnthropicBase: "https://api.z.ai/api/anthropic",
			BizHost:       "https://api.z.ai",
			QuotaHost:     QuotaHost,
		}
	}
}

// QuotaHost 额度/账单查询的统一 host。
//
// **两个服务商都用它** —— 实测 api.z.ai 与 open.bigmodel.cn 对
// billing/balance 都回 404，只有 zcode.z.ai 给出数据。
const QuotaHost = "https://zcode.z.ai"

// ⚠⚠ 已知的**架构级不一致**（2026-09-19 实测，尚未修，需产品决策）
//
// # 现象
//
// 额度挂在 **start-plan** 上，而我们的对话走 **coding/paas** —— 两条通道。
//
//	① 额度（`GET zcode.z.ai/api/v1/zcode-plan/billing/balance`）
//	   planKind = "paid"
//	   entries[0].planId    = "zcode-v3-start-plan-wk-0918"   ← **start-plan**
//	                     showName = "GLM-5.3-Flash"
//	                     remaining = 299999978 / 300000000    ← 2.99 亿 token
//
//	② 对话（`{OpenAIBase}/chat/completions`，即 coding/paas）
//	   任意模型（glm-5.3 / glm-4.6 / glm-5.3-flash）一律：
//	   429 {"code":1113,"msg":"余额不足或无可用资源包,请充值。"}
//
//	③ 同一账号的 `{OpenAIBase}/models` 却是 **HTTP 200**，能列出 11 个模型
//	   → 该通道**对该账号是开放的**，只是"没有资源包"
//
// # 为什么这不是"账号没额度"
//
// `1113` 的语义是**「这条通道没有该账号的资源包」**，而不是「账号没额度」。
// 账号确实有 2.99 亿 token —— 只是挂在 start-plan 上。
//
// 用户看到的现象是「明明有额度却报余额不足」，而我们的错误文案
//（`ErrNoResourcePack` 的 FriendlyMessage）也没能说清这一点。
//
// # 为什么我没直接改（这是产品决策，不是 bug 修复）
//
// 切到 anthropic（start-plan）通道意味着：
//
//	· 协议从 OpenAI 换成 Anthropic（要重写请求/响应翻译层）
//	· 该通道要求**验证码**（3007），而我们已实现求解器
//	· 但该通道在解完验证码后回 **3012 unusual activity** ——
//	  已做 A/B 实验证明**与请求形状无关**（补官方 system 块 + meta_user
//	  后仍然 3012），是账号/风控层
//
// 即：切过去**也不一定能用**，却要付出一整套协议翻译的代价。
// 故先把事实记录在这里，由所有者决定要不要投入。
//
// 验证脚本：`uitest/diag-zcode-channel.cjs`、`uitest/diag-system-block-ab.cjs`

// OpenAIBase / AnthropicBase / BizHost / QuotaHost 便捷访问器。
func (p Provider) OpenAIBase() string    { return p.endpointsOf().OpenAIBase }
func (p Provider) AnthropicBase() string { return p.endpointsOf().AnthropicBase }
func (p Provider) BizHost() string       { return p.endpointsOf().BizHost }
func (p Provider) QuotaHost() string     { return p.endpointsOf().QuotaHost }

// AllProviders 全部已知服务商（供界面渲染选项）。
func AllProviders() []Provider { return []Provider{ProviderZAI, ProviderBigmodel} }
