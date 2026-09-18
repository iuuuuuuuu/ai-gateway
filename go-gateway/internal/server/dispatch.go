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

// QoderUpstream Qoder 产品的上游能力（由 qoder 包实现，见 qoder.Dispatch）。
//
// nil 表示不支持 Qoder —— 此时所有账号都按 WorkBuddy 处理，
// 行为与单产品时代完全一致（多产品关闭时的默认状态）。
//
// 接口刻意很窄：server 只需要"把请求发出去、把流拿回来、把非流式结果聚合成
// OpenAI JSON"。**流的形状转换不在这里** —— 由实现直接返回一个已翻译成
// 标准 OpenAI SSE 的 reader（见 qoder.NewOpenAIStream 的注释：
// 三个消费者都读这个流，做成转换器后它们无需知道产品差异）。
type QoderUpstream interface {
	// ChatStream 用 Qoder 账号发起对话，返回**已翻译成 OpenAI SSE** 的流。
	//
	// openAIBody 是客户端发来的 OpenAI 请求体；实现内部负责：
	// 取模型 key → 构造 Qoder 请求体 → QoderEncode → COSY 签名 → 发送
	// → 把嵌套 SSE 惰性翻译成 OpenAI 帧。
	ChatStream(ctx context.Context, a *auth.Auth, openAIBody []byte) (rc io.ReadCloser, status int, respBody []byte, err error)

	// Aggregate 读完 Qoder 流并聚合成 OpenAI 非流式响应。
	Aggregate(r io.Reader, model string) (map[string]any, error)
}

// dispatchUpstream 按账号所属产品选择上游实现。
//
// 返回 (client, isQoder)：
//
//	isQoder=false → 调用方走既有的 WorkBuddy 路径（upstream.Client）
//	isQoder=true  → 调用方走 Qoder 路径（本接口）
//
// 判定依据是账号的 Product 字段（空串归一成 workbuddy）。
//
// ## 为什么产品未知时**回退到 WorkBuddy** 而不是报错
//
// 生产环境里绝大多数账号没有 Product 字段（历史凭证文件），
// 报错会让所有既有用户立刻不可用。回退到 WorkBuddy 是安全的：
// 那些账号本来就是 WorkBuddy 的。
//
// 但**Qoder 凭证误判成 WorkBuddy** 会造成"用 Qoder 的 token 打 WorkBuddy 端点"
// —— 故加载层必须正确设置 Product（见 qoder 的凭证加载）。
func (h *Handler) dispatchUpstream(a *auth.Auth) (QoderUpstream, bool) {
	if h.cfg.Qoder == nil {
		// 多产品未启用：所有账号按 WorkBuddy 处理（默认状态）。
		return nil, false
	}
	if a == nil || !a.IsQoder() {
		return nil, false
	}
	return h.cfg.Qoder, true
}
