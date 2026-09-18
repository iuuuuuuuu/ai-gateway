package server

// dispatch.go 多产品请求派发。
//
// ## 为什么需要这一层
//
// 选号是**跨产品**的（pool 可能选中任一产品的账号），而"把请求发给上游"
// 是**产品内**的实现：
//
//	WorkBuddy:  Bearer 令牌 + /v2/chat/completions + 标准 OpenAI 响应
//	Qoder:      COSY 签名 + /algo/... + 请求体编码 + **嵌套** SSE 响应
//
// 若不加这一层，pool 选中 Qoder 账号后会走 WorkBuddy 的 ChatStream ——
// 拿 Qoder 的凭证去请求 WorkBuddy 的端点，必然失败。而且失败信息会显示成
// "账号不可用"，把排查方向引向凭证，完全看不出是派发错了产品。
//
// ## 接口为什么这么窄
//
// server 包只需要两个能力：**发一次请求拿到流**、**把流翻译成 OpenAI 形状**。
// 其余（COSY 签名、令牌刷新、双区端点）都是 qoder 包的内部事务，
// server 不该知道。这样也便于单测注入假实现。

import (
	"context"
	"io"

	"workbuddy2api/internal/auth"
)

// ProductUpstream 一个**非 WorkBuddy 产品**的上游能力。
//
// ## 为什么叫 ProductUpstream 而不是 QoderUpstream
//
// 这个接口最早只有 Qoder 一个实现，故当时叫 QoderUpstream。
// 现在有 ZCode（未来可能还有别的），继续用产品名会让读代码的人
// 以为"只有 Qoder 走这条路" —— 而它其实是**所有非 WorkBuddy 产品**的
// 统一接入点。
//
// WorkBuddy 不走这个接口：它是默认路径，直接调 upstream.Client
//（历史最久、路径最热，不为了对称而多一层间接）。
//
// ## 接口为什么这么窄
//
// server 只需要三个能力：发请求拿流、把非流式结果聚合成 OpenAI JSON。
// 其余（签名、令牌刷新、端点选择、请求体编码）都是各产品包的内部事务，
// server 不该知道。这样也便于单测注入假实现。
//
// nil 表示该产品不可用 —— 此时对应账号会退回 WorkBuddy 路径
//（多产品关闭时的默认状态）。
type ProductUpstream interface {
	// ChatStream 用该产品的账号发起对话，返回**已是标准 OpenAI SSE** 的流。
	//
	// openAIBody 是客户端发来的 OpenAI 请求体；实现内部负责该产品的一切
	// 协议细节（签名 / 编码 / 端点 / 形状转换）。
	//
	// 返回已翻译成 OpenAI 形状的流是**刻意约定**：网关内部有三个消费者
	//（chat/completions、messages、responses），让它们各自判断产品差异
	// 会有三处分支、三处可能漏改。翻译收在实现里，消费者看到的一律相同。
	ChatStream(ctx context.Context, a *auth.Auth, openAIBody []byte) (rc io.ReadCloser, status int, respBody []byte, err error)

	// Aggregate 读完流并聚合成 OpenAI 非流式响应。
	Aggregate(r io.Reader, model string) (map[string]any, error)
}

// dispatchUpstream 按账号所属产品选择上游实现。
//
// 返回 (client, isProduct)：
//
//	isProduct=false → 调用方走既有的 WorkBuddy 路径（upstream.Client）
//	isProduct=true  → 调用方走该产品的实现（本接口）
//
// 判定依据是账号的 Product 字段（空串归一成 workbuddy）。
//
// ## 为什么产品未知时**回退到 WorkBuddy** 而不是报错
//
// 生产环境里绝大多数账号没有 Product 字段（历史凭证文件），
// 报错会让所有既有用户立刻不可用。回退到 WorkBuddy 是安全的：
// 那些账号本来就是 WorkBuddy 的。
//
// 但**其它产品凭证误判成 WorkBuddy** 会造成"用别人的 token 打 WorkBuddy 端点"
// —— 故加载层必须正确设置 Product（见各产品的凭证加载）。
func (h *Handler) dispatchUpstream(a *auth.Auth) (ProductUpstream, bool) {
	if a == nil {
		return nil, false
	}
	switch a.Product {
	case auth.ProductQoder:
		if h.cfg.Qoder == nil {
			// 该产品未启用（多产品关闭）：退回 WorkBuddy 路径。
			// 这与"账号是 Qoder 但配置里没开 Qoder"的不一致状态有关 ——
			// 报错会让用户看到"账号不可用"，而真正原因是配置没开。
			return nil, false
		}
		return h.cfg.Qoder, true
	case auth.ProductZcode:
		if h.cfg.Zcode == nil {
			return nil, false
		}
		return h.cfg.Zcode, true
	default:
		return nil, false
	}
}
