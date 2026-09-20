package server

// forward.go 抽出「选号 → 轮转 → 转发上游 chat/completions」的核心流程，
// 供三种协议入口共用：
//
//   - POST /v1/chat/completions  原生 OpenAI Chat Completions（本文件直通）
//   - POST /v1/responses         OpenAI Responses API（Codex / ChatGPT 系）
//   - POST /v1/messages          Anthropic Messages API（Claude Code / Claude Desktop）
//
// 设计：后两者在进入本流程之前把请求体转换成 OpenAI Chat 形态，
// 拿到上游的 chat 响应（非流式 map 或原始 SSE 流）后，各自再转回目标协议。
// 这样账号池、粘性会话、熔断冷却、积分冷却、统计日志全部只有一份实现，
// 协议适配层只负责「形状转换」，不碰任何调度状态。

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"log"
	"net/http"
	"strings"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/prompt"
	"workbuddy2api/internal/session"
	"workbuddy2api/internal/upstream"
)

// sseProbeTimeout 流式请求预读首帧的**兜底**超时（未配置时用）。
//
// # 取值依据
//
// 首帧 = 上游开始产出（TTFB）。实测本机到 WorkBuddy 的 TTFB 约 1.1s。
//
// ⚠⚠ 但它对**推理模型的长思考**可能不够（2026-09-20 修正）：
// 用户配置里 `upstream.header_timeout_seconds = 120`（"聊天 SSE 首字节前
// 上限"），说明他期望上游可以有 2 分钟才吐第一个字节。
// 而旧代码**硬编码 30 秒**，于是：
//
//	思考 40 秒才开始输出的模型 → probe 超时 → 判"上游返回空流" → 换号
//	→ 换过去还是同一个模型、同样超时 → 整轮失败
//
// 用户看到的是 `empty upstream stream`，而**上游其实完全正常**，
// 只是思考久了点。这是"用错误的判据把正常请求判成失败"。
//
// 故：**优先用配置的 header_timeout_seconds**（那是用户对"首字节等待"
// 的显式表态），只在未配置时回落到这个常量。
const sseProbeTimeout = 30 * time.Second

// probeTimeoutFor 返回本次流式请求该用的首帧预读超时。
//
// 优先 `upstream.Client.HeaderTimeout`（由配置
// `upstream.header_timeout_seconds` 而来，**用户对"等上游开口"的显式表态**），
// 未配置（<=0）时回落 `sseProbeTimeout`。
//
// 为什么与 SSE 的 header timeout 复用同一个配置而不是新加一个键：
// 它们**语义相同** —— 都是"等上游开口"的上限。多一个键会让用户
// 面对两个含义几乎一样的数字，而不知道该调哪个。
func (h *Handler) probeTimeoutFor() time.Duration {
	if h != nil && h.cfg.Upstream != nil && h.cfg.Upstream.HeaderTimeout > 0 {
		return h.cfg.Upstream.HeaderTimeout
	}
	return sseProbeTimeout
}

// ⚠ unsupportedEffortError 与 checkRequestedEffort 已移除（2026-09-18）。
//
// 它们实现了「客户端指定的档位不在该模型 supportedEfforts 里 → 400 拒绝」，
// 前提是**supportedEfforts 是该模型的硬范围**。实测推翻了它：
//
//	hy3     声明 [low, high]     → medium / max / minimal / **off** 全部接受
//	glm-5.2 声明 [high, xhigh]  → low / max / medium / **off** 全部接受
//
// 上游对这些「范围外」的档照常接受，说明 supportedEfforts 只是
// 「界面上建议列出哪些」，**不是**可用范围。据它拦截会把合法请求拒掉，
// 而且用户拿到 400 后毫无办法 —— 他用的档其实能用。
//
// 现在网关对 reasoning_effort 一律原样透传；真不被接受的档由上游自己报 400，
// 错误原样返回（那是唯一权威的判据）。详见 forward.go 里 chatCompletions
// 中「思考档位**不在这里校验**」那段注释。

// modelLockedError 请求的模型不在「限制使用的模型」白名单内。
//
// 单独成型（而不是拼一个字符串）是为了让调用方能识别它并回以 400 +
// 明确的错误码，而不是当成「账号不可用」的 503 —— 后者会误导用户去查账号。
//
// 名字保留历史叫法（Locked）：错误码 model_not_allowed 与各协议的判定链
// 都建在它上面，改名的收益抵不过动这条链的风险。
type modelLockedError struct {
	requested string   // 客户端请求的模型（可能为空，表示请求体未带 model）
	allowed   []string // 当前放行的模型白名单（非空；空名单根本不会走到这里）
}

func (e *modelLockedError) Error() string {
	got := e.requested
	if got == "" {
		got = "(未指定)"
	}
	list := allowedModelsText(e.allowed)
	// 文案必须**列出全部**允许的模型：用户配了 3 个模型时只报「不允许 x」
	// 完全没法排查 —— 他不知道该改成哪一个，只能挨个试。
	return "当前已限制可使用的模型，只允许调用 " + list + "；收到的是 " + got +
		"。请在客户端把模型改为 " + list + " 中的任意一个，或在界面的「放行模型」里调整限制。"
}

// chatResult 一次成功的上游调用结果。
//
// 二者互斥：
//   - Stream != nil  → 流式：调用方负责 Close，并按目标协议解析/转换 SSE。
//   - Response != nil → 非流式：上游 SSE 已被 Aggregate 成 OpenAI chat.completion。
type chatResult struct {
	UID   string
	Model string
	// Product 供本次结果来自哪个产品（"" = WorkBuddy）。
	//
	// 调用方据此决定**怎么读这个流**：WorkBuddy 已是 OpenAI 形状，直接透传；
	// Qoder 是嵌套形状，必须先翻译。搞混会得到空回答（HTTP 仍 200）。
	Product  string
	Stream   io.ReadCloser
	Response map[string]any
}

// IsQoder 报告本次结果是否来自 Qoder（调用方据此选择流的读法）。
func (r *chatResult) IsQoder() bool { return r != nil && r.Product == auth.ProductQoder }

// forwardChat 执行「选号 → token 刷新 → 转发 → 失败换号」的完整轮转。
//
// ⚠ 本函数是 forwardChatCtx 的便捷包装（ctx = context.Background()），
// **仅供既有调用点与单测**使用 —— 它们不关心取消。生产路径必须用
// forwardChatCtx(ctx, ...) 传 r.Context()，否则换号退避无法被客户端断开取消
//（见 backoff.go 的 sleepCtx）。
func (h *Handler) forwardChat(body []byte, stream bool, sessKey string) (*chatResult, int, error) {
	return h.forwardChatCtx(context.Background(), body, stream, sessKey)
}

// forwardChatCtx 执行「选号 → token 刷新 → 转发 → 失败换号」的完整轮转。
//
// 参数：
//   - ctx：本次请求的上下文。**只有换号退避**用它（客户端断开 / 请求超时即
//     中止轮转，不替一个没人要的请求继续打上游）；上游调用本身仍沿用既有行为
//     （WorkBuddy 走 upstream.Client 自身的超时，产品路径显式传
//     context.Background()），本次改动不动那条路径。
//   - body：已转换成 OpenAI Chat 形态的请求体（原始字节，发往上游前由
//     upstream.Client 再做一次 PrepareBody：强制 stream、归一化 role/tool_choice）。
//   - stream：调用方是否要求流式。上游恒为流式，非流式时本函数读完后 Aggregate。
//   - sessKey：会话粘性键；空串表示不做粘性绑定。
//
// 返回：
//   - result：成功时非 nil。
//   - status/lastErr：失败时给出应回给客户端的 HTTP 状态与最后一处错误，
//     调用方据此生成对应协议的错误体。
//
// 失败语义与原有 chatCompletions 完全一致：传输层错误只换号不喂熔断，
// 业务错误按 Classify 结果施加冷却/禁用/熔断。
func (h *Handler) forwardChatCtx(ctx context.Context, body []byte, stream bool, sessKey string) (*chatResult, int, error) {
	tried := map[string]bool{}
	var lastErr error
	var lastUID string
	lastStatus := http.StatusServiceUnavailable
	// lastKind/lastBody 记录最后一次上游失败的分类与原始响应体，
	// 用于在全部账号失败时给客户端一句**可读**的原因（见函数末尾的 FriendlyMessage）。
	var lastKind upstream.ErrKind
	var lastBody string
	// lastTransportErr 非 nil 表示最后一次失败是传输层（无上游响应体）。
	var lastTransportErr error

	var stickyUID string
	if sessKey != "" && h.cfg.Session != nil {
		if uid, ok := h.cfg.Session.Resolve(sessKey); ok {
			stickyUID = uid
		}
	}

	// 在途租约：成功选中即占名额，函数出口统一释放（成功即转移给调用方持有）。
	var heldUID string
	var handedOff bool
	defer func() {
		if heldUID != "" && !handedOff {
			h.cfg.Pool.Release(heldUID)
		}
	}()
	releaseHeld := func() {
		if heldUID != "" {
			h.cfg.Pool.Release(heldUID)
			heldUID = ""
		}
	}
	fail := func(uid string) {
		releaseHeld()
		if stickyUID != "" && uid == stickyUID && h.cfg.Session != nil {
			h.cfg.Session.Unbind(sessKey)
			stickyUID = ""
		}
	}

	// 请求的目标模型：用于「模型级限流」的选号过滤与冷却记账。
	// 取不到时为空串，各环节自动退化为原有行为（不做模型过滤）。
	//
	// 同时解析可选的路由前缀 `[产品:][区域:]`：前缀只是给网关的**选号指令**，
	// 上游不认识它，故必须把请求体里的 model 改写成裸名（见下方 rewriteModel）。
	// 注意返回顺序是 (product, realm, bare) —— 写反会把前缀当成模型名，
	// 表现为「单一模型锁定」报「收到的是 (未指定)」。
	//
	// # 前缀**参与选号**（2026-09-20 修正）
	//
	// 此前这里写的是 `_, model := resolveModel(...)`，realm 被**丢弃**，
	// 且注释声称"区域约束已升级为 route/preferRegion"。但 `preferRegion`
	// 只来自 `imageRouteFor`，而那个函数**只对带图片的请求生效** ——
	// 于是 `cn:` / `global:` 前缀实际只被剥离、**完全不约束选号**。
	//
	// 实测确认（uitest/diag-region-prefix2.cjs，8 轮）：
	//
	//	`cn:deepseek-v4.1-flash`  → 选中 e889fe8a（www.workbuddy.ai，**国际版**）
	//	`global:...`              → 选中 6b0c77ab（国际版）
	//
	// 即带 `cn:` 前缀却选中了国际版账号 —— 前缀无效。所有者要的
	// 「平台:国际版:模型名」正依赖这个机制，故必须修。
	product, realm, model := resolveModel(modelOf(body))

	// 前缀只是给网关的**选号指令**，上游不认识它 —— 必须把请求体里的
	// model 改写成裸名，否则上游返回 400 code=11102 model [cn:xxx] not found
	//（实测确认：前缀成功约束了选号，却让请求本身失败）。
	// 无前缀时 model 与原值相同，rewriteModel 会原样返回，不做多余序列化。
	// 系统提示词替换必须排在 rewriteModel **之前**：两者都会重新序列化请求体，
	// 先做提示词替换可以少一次整体编码，也避免「模型已改写成裸名、提示词却没换」
	// 这种半改状态的中间结果出现在日志/排查视野里。
	//
	// 放在 forwardChat 而不是各协议入口：三种协议（chat / messages / responses）
	// 的请求体都在这里汇合成 OpenAI Chat 形态，在此改写只需一处，
	// 也不会漏掉任何一条出站路径。
	//
	// mode=passthrough（缺省）时**完全不调用**任何改写函数：既有行为必须逐字不变
	//（不重新序列化、不动 messages），因此这里是显式分支而非「传空串让它空转」。
	switch h.cfg.PromptMode {
	case prompt.ModeCustom:
		// 整体替换：会丢掉客户端的项目规范（见 config.go 的模式说明）
		if h.cfg.PromptText != "" {
			body = prompt.Rewrite(body, h.cfg.PromptText)
		}
	case prompt.ModeAppend:
		// 追加：客户端规则在前、网关提示词在后，两者共存
		if h.cfg.PromptText != "" {
			body = prompt.Append(body, h.cfg.PromptText)
		}
	}

	body = rewriteModel(body, model)

	// 区域路由：仅对「图像能力两区不同」的模型 + 带图片的请求生效，
	// 其余情况返回 RegionAny，行为与引入本特性之前完全一致。
	// 详见 imageRouteFor 的注释。
	route := imageRouteFor(model, requestHasImage(body))
	preferRegion := route.Region

	// **显式前缀优先于自动推断**（2026-09-20 新增）。
	//
	// 用户写了 `cn:` / `global:`（或中文 `国际版:`）就是明确指令，
	// 不该被 imageRouteFor 的自动推断覆盖 —— 那是"猜"，而用户是"说"。
	// 二者冲突时以用户为准；推断只在用户没表态时兜底。
	//
	// ⚠ 注意 `imageRouteFor` 在"两区都支持"或"都没实测"时返回 RegionAny，
	// 此时前缀就是唯一约束（这是绝大多数情况 —— 文本请求从不带图，
	// 永远走 RegionAny 那条分支，也就解释了为什么此前前缀形同虚设）。
	if r := realmToRegion(realm); r != auth.RegionAny {
		preferRegion = r
	}
	// ⚠⚠ **必须把 preferRegion 写回 route** —— 选号与随后的错误分支用的都是
	// `route.Region`，而不是 `preferRegion`。
	//
	// 我第一版只改了 `preferRegion` 这个局部变量，于是：
	//	· `pickAccountFor(..., route, ...)` 读的是 **route.Region（仍是 RegionAny）**
	//	· 前缀**没有任何效果**
	// 实测确认（uitest/diag-429-isolate.cjs）：
	//
	//	`global:deepseek-v4.1-flash` → 选中 uid=4ea736d4（**国服**）
	//
	// 写了区域前缀却选中国服账号 —— 与"前缀被丢弃"是同一个症状，
	// 只是这次是**我自己的疏忽**（算出了正确值却没用它）。
	route.Region = preferRegion

	// 「限制使用的模型」白名单：非空时只放行名单内的模型。
	//
	// 为什么在选号之前就拒绝（而不是换个模型重试）：这份名单是**策略**的一部分
	//（轮转模式下尤其如此 —— 语义是「把这个账号的指定模型额度烧干净再换号」）。
	// 若允许其他模型通过，客户端换个模型就能绕过轮转与额度控制，也让「当前烧的
	// 是哪个模型」变得不可预期 —— 因此明确拒绝并说明原因，比静默改写模型
	//（用户以为在用 A、实际用了 B）更安全。
	//
	// 比较用的是剥前缀后的**裸模型名**（上面 resolveModel 的结果）：用户配的是
	// `deepseek-v4.1-flash`，客户端可能带 `cn:` 前缀请求，两者应视为同一个模型。
	// 大小写不敏感 —— 客户端写法并不统一。名单为空 = 不限制（默认，向后兼容）。
	//
	// 名单取自 h.allowed（NewHandler 里归一化并缓存），不是 h.cfg.AllowedModel ——
	// 后者是单值的历史字段，已在归一化时合并进 h.allowed。
	if !modelAllowed(h.allowed, model) {
		// 返回非 nil 的 result：调用方会在错误分支里读 result.UID 记日志，
		// 返回 nil 会 panic。UID 留空即可（本次没有选中任何账号）。
		return &chatResult{Model: model}, http.StatusBadRequest,
			&modelLockedError{requested: model, allowed: h.allowed}
	}

	// 思考档位**不在这里校验**（2026-09-18 移除，实测推翻了原假设）。
	//
	// 原实现：客户端指定的档不在该模型 supportedEfforts 里 → 直接 400。
	// 前提是「supportedEfforts 是该模型的**硬范围**」。实测证明前提不成立：
	//
	//	hy3    声明 [low, high]      → medium / max / minimal / **off** 全部接受
	//	glm-5.2 声明 [high, xhigh]   → low / max / medium / **off** 全部接受
	//
	// 也就是说 supportedEfforts 描述的是「界面上建议列出哪些档」，
	// **不是**「只接受这些档」。据它拦截会把上游本来接受的合法请求拒掉 ——
	// 而这类误拒比静默降级更糟：用户拿到 400 却毫无办法（他用的档其实能用）。
	//
	// 于是网关对档位的职责收敛为**如实透传**：既不拦、也不改写。
	// 真不被上游接受的档（如 deepseek-v4.1-flash 的 off）由上游自己报 400，
	// 错误信息原样返回给客户端 —— 那才是唯一权威的判据。

	// rotateWait 本次请求「准备发起第 i 次尝试」前的退避等待，返回 false 表示
	// 客户端已断开，调用方应中止轮转。
	//
	// # 为什么放在**上游调用之前**（而不是循环顶部）
	//
	// 退避的唯一目的是「不要把一串账号零延迟地打给上游」（见 backoff.go）。
	// 因此它必须紧贴上游调用，并且只在**确实要打上游**时才等：
	//
	//   - 循环顶部还有选号 / 取租约 / 刷新 token 几步，其中「池里选不出号」
	//     会直接 break 返回错误。若退避放在循环顶部，那条路径会先白等一次
	//     再报「账号全部不可用」—— 用户平白多等半秒以上（MaxRotate 越大越久），
	//     而他等到的还是一个失败。放在这里就完全不会白等。
	//   - 它同时仍是所有**真正会打上游**的路径的必经点：选号成功 → 取到租约
	//     → 刷新成功之后，无论前一次是因为什么失败的（传输层 / 4xx / 空流 /
	//     刷新失败），两个连续的上游尝试之间都隔着一次抖动退避。
	//
	// 三条约束都在这里满足：
	//
	//  1. **第 0 次不退避**：i==0 时没有任何账号失败过，没什么可等的。
	//     正常请求的第一发不该被拖慢 —— 这正是「退避写在失败之后、而不是
	//     请求之前」的含义。
	//  2. **最后一次失败后不白等**：i 走到 MaxRotate 就退出循环了，
	//     退避只发生在「后面确实还有一次尝试」时（i 从 1 到 MaxRotate-1）。
	//     最后一次失败后直接返回错误，不再空等。
	//  3. **可取消**：客户端断开 / 请求超时后 sleepCtx 立即返回 false，
	//     调用方中止轮转 —— 不替一个没人要的请求继续打上游。
	rotateWait := func(i int) bool {
		if i <= 0 {
			return true // 第一次尝试：不退避
		}
		d := backoffAfter(i - 1)
		if d <= 0 {
			return true
		}
		// 只在真的要换号时才打日志：这条日志是排查「上游为何看到一串账号」
		// 的第一手线索，但正常成功路径上不该出现它。
		log.Printf("chat rotate: 第 %d 次换号前退避 %s", i, d)
		return sleepCtx(ctx, d)
	}

	for i := 0; i < h.cfg.MaxRotate; i++ {

		var acct *auth.Auth
		if stickyUID != "" {
			// ⚠ 粘性路径**也必须**校验产品，否则 `zcode:glm-5.3` 会继续用
			// 该会话此前绑定的 WorkBuddy 账号 —— 实测确认过这个漏洞
			//（见 PickByUIDForModelProductRegion 的注释）。
			acct = h.cfg.Pool.PickByUIDForModelProductRegion(stickyUID, model, preferRegion, product)
			if acct == nil {
				if h.cfg.Session != nil {
					h.cfg.Session.Unbind(sessKey)
				}
				stickyUID = ""
			}
		}
		if acct == nil {
			acct = h.pickAccountFor(model, tried, route, product)
		}
		if acct == nil {
			// 区域受限且选不出号：**不降级**，明确告诉用户缺哪个区域的账号。
			// 静默跨区会让图片被后端换成占位符，模型回「我看不见图片」——
			// 用户完全无从判断是网络、模型还是网关的问题。
			if route.Required {
				return &chatResult{Model: model}, http.StatusServiceUnavailable,
					&forwardFailure{
						Kind:    FailureImageRegionUnavailable,
						Status:  http.StatusServiceUnavailable,
						Message: imageRegionUnavailableMessage(route.Region),
					}
			}
			lastStatus = http.StatusServiceUnavailable
			break
		}
		tried[acct.UID] = true
		lastUID = acct.UID

		if !h.cfg.Pool.Acquire(acct.UID) {
			if stickyUID != "" && acct.UID == stickyUID && h.cfg.Session != nil {
				h.cfg.Session.Unbind(sessKey)
				stickyUID = ""
			}
			continue
		}
		heldUID = acct.UID

		// 按账号所属产品派发上游实现。
		//
		// 这一处是整个多产品路由的**唯一**分叉点：pool 可能选中任一产品的
		// 账号，而各产品的鉴权/端点/请求体编码/响应形状完全不同。
		// 不加这个分支就会拿别人的凭证去请求 WorkBuddy 的端点 ——
		// 失败信息会显示成"账号不可用"，排查方向被引向凭证，
		// 完全看不出是派发错了产品。
		productUp, isProduct := h.dispatchUpstream(acct)

		// token 临近过期 → 先 refresh（失败冷却换号）。
		//
		// ⚠ 只对 **WorkBuddy 账号**做这件事。两个原因：
		//
		//  1. 其它产品**没有 WorkBuddy 的刷新接口** —— 调
		//     `Upstream.RefreshToken` 会失败并把账号冷却掉，请求永远走不到派发。
		//     实测踩到：ZCode 账号没有 ExpiresAt（它的凭证长期有效），
		//     于是 NeedsRefresh 恒为 true，每次请求都在这里失败退出。
		//
		//  2. 各产品的刷新方式完全不同（Qoder 是 deviceToken/refresh + drt 轮换），
		//     故刷新必须由**产品自己的实现**负责（Qoder 的 Dispatch.ChatStream
		//     内部就会按需刷新）。
		if !isProduct && acct.NeedsRefresh(h.cfg.RefreshSkew) {
			if err := h.cfg.Upstream.RefreshToken(acct); err != nil {
				lastErr = err
				var ue *upstream.Error
				// 末尾的可读文案读的是 lastKind/lastBody/lastTransportErr，三者必须
				// 与 lastErr 同步更新 —— 否则会沿用**上一个账号**留下的分类，例如把
				// 「刷新失败」报成「额度已耗尽」，把用户引向错误的排查方向。
				if errors.As(err, &ue) {
					lastKind, lastTransportErr = ue.Kind, nil
				} else {
					// 非 upstream.Error 的失败基本都是传输层（超时 / 连接被拒）
					lastKind, lastTransportErr = upstream.ErrNone, err
				}
				lastBody = ""
				if errors.As(err, &ue) && ue.Kind == upstream.ErrSessionDead {
					h.cfg.Pool.Disable(acct.UID, "refresh session dead")
				} else {
					h.cfg.Pool.NoteError(acct.UID)
				}
				fail(acct.UID)
				continue
			}
			if err := acct.SaveAtomic(); err != nil {
				log.Printf("chat refresh uid=%s: save auth failed: %v", acct.UID, err)
			}
		}

		// 换号退避：i>0 说明上一次尝试已经失败，接下来这一发打的是另一个账号。
		// 放在这里（真正要打上游之前）而不是循环顶部，是为了不让「池里选不出号」
		// 那条直接 break 的路径白等一次 —— 详见 rotateWait 的注释。
		if !rotateWait(i) {
			return &chatResult{UID: lastUID}, http.StatusServiceUnavailable, rotateCanceledErr(ctx)
		}

		var rc io.ReadCloser
		var status int
		var respBody []byte
		var terr error
		if isProduct {
			// 产品路径：实现内部负责该产品的一切协议细节
			//（Qoder 的签名/编码/嵌套 SSE 翻译；ZCode 的模型名映射）。
			// 传的是**客户端原始 OpenAI 请求体**。
			rc, status, respBody, terr = productUp.ChatStream(context.Background(), acct, body)
		} else {
			rc, status, respBody, terr = h.cfg.Upstream.ChatStream(acct, body)
		}
		if terr != nil {
			lastStatus = http.StatusServiceUnavailable
			lastErr = terr
			// 传输层失败（超时/连接被拒/DNS）没有上游业务体，Classify 不适用；
			// 单独标记，使末尾的错误文案也能给出可读原因而不是原始 Go 报错
			// （实测：原始文案会把 tcp 四元组与 wsarecv 细节直接抛给客户端）。
			lastKind = upstream.ErrNone
			lastBody = ""
			lastTransportErr = terr
			fail(acct.UID)
			continue
		}
		if status >= 400 {
			lastStatus = status
			kind := upstream.Classify(status, string(respBody))
			lastKind = kind
			lastBody = string(respBody)
			lastTransportErr = nil
			lastErr = &upstream.Error{Kind: kind, Status: status, Msg: string(respBody)}

			// WAF 403：可能拦的是**出口 IP**，不是账号（见 wafip.go 的实测记录）。
			//
			// 与下面两类请求侧错误不同，这里的结论不是「请求有问题」而是
			// 「我们共同的出口可能被拦了」—— 它由**多个不同账号**在窗口内
			// 一起 403 推断出来，所以判定状态是进程级共享的。
			//
			// 为什么必须在**继续换号之前**判断：这正是本缺陷的放大机制 ——
			// 所有号轮一遍全 403，而网关还在换号，把请求放大 MaxRotate 倍，
			// 恰好是 WAF 最想惩罚的行为，封禁被越打越重。
			//
			// ⚠ 判定成立时**不动账号状态**（不 applyErrorPolicy）：
			// 账号是好的，问题在网络出口。把好账号冷却掉只会让用户在
			// 「IP 解封之后」发现号也被自己人停了。
			if isWafForbidden(status, string(respBody)) {
				blocked := wafIP.noteWaf(acct.UID)
				uid := acct.UID
				distinct := wafIP.distinct()
				// 记录日志：这是「为什么请求突然全都失败」的第一手线索，
				// 也是事后判断「到底是不是 IP 被拦」的唯一依据。
				log.Printf("chat uid=%s: WAF 403（窗口内 %d 个不同账号 403，已判定 IP 被拦=%v）",
					uid, distinct, blocked)
				if blocked {
					// 跨过阈值：立即终止轮转。再换号只会让 WAF 看到更多账号被打。
					//
					// ⚠ 这里只 releaseHeld，**不调 fail(uid)**：fail 会在该号正是
					// 粘性绑定号时 Unbind 掉会话。但这个账号是好的（被拦的是出口 IP），
					// 解绑只会让同一会话下次换到别的号 —— 而 IP 解封后它本可继续用。
					releaseHeld()
					return &chatResult{UID: uid}, http.StatusServiceUnavailable, &forwardFailure{
						Kind:    FailureEgressIPBlocked,
						Status:  http.StatusServiceUnavailable,
						Message: egressIPBlockedMessage(distinct),
					}
				}
				// 未跨阈值：本次只是一个账号 403，不足以断定 IP 问题。
				// 沿用既有语义继续换号 —— 403 的 kind 是 ErrClient，
				// applyErrorPolicy 的 default 分支本就只换号不罚账号。
				h.applyErrorPolicy(uid, model, kind, string(respBody))
				fail(uid)
				continue
			}

			// 上下文超长是**请求侧**错误：换号无用（同一请求体发给任何账号都同样失败），
			// 继续轮转只会把整个请求体对着每个账号重传一遍（实测 1.12M token × 3），
			// 最后还被伪装成「账号全部不可用」，把排查方向引向账号故障。
			//
			// 立即以真实状态返回，交由客户端精简上下文后重试。
			// 不罚账号也不换号：账号状态完全不动（applyErrorPolicy 对请求侧错误
			// 本就只换号不罚，这里连换号都省掉）。
			//
			// 这是**唯一**改变对外状态码的路径；其余失败仍沿用原有的
			// 503 no_healthy_account 契约（账号池耗尽的语义）。
			if kind == upstream.ErrContextTooLong {
				uid := acct.UID
				releaseHeld()
				// 带上 UID：调用方在失败路径也要读 result.UID 记日志，
				// 返回 nil 会让它空指针崩溃（本测试即抓到此点）。
				return &chatResult{UID: uid}, status, &forwardFailure{
					Kind:   FailureContextTooLong,
					Status: status,
					// 保留上游原文：下游客户端靠文案识别上下文溢出并触发自动压缩，
					// 只回我们自己的措辞会让它认不出这是溢出。
					Message: upstream.ContextTooLongMessage(string(respBody)),
				}
			}

			// 思考档位被上游拒绝：**同样是请求侧错误**，但它比上下文超长更需要
			// 「原样转达」—— 用户要看到上游到底说了什么（如
			// "the reasoning effort value is not supported by the current model"），
			// 才能知道该换成哪个档位。
			//
			// 为什么不能让它落进下面的轮转：
			//   · 换号毫无意义（同一请求体给任何账号都会被拒）；
			//   · 轮转结束后消息会被包装成 503「账号全部不可用」，
			//     用户会去查账号，而真实原因在请求里；
			//   · 上游原文在那一层已被 FriendlyMessage 吞掉。
			//
			// 注意这里**不罚账号也不换号**：账号状态完全不动（上游拒绝的是参数，
			// 不是这个账号）。status 原样透出（通常 400），客户端据此判断是请求问题。
			if kind == upstream.ErrEffortRejected {
				uid := acct.UID
				releaseHeld()
				return &chatResult{UID: uid}, status, &forwardFailure{
					Kind:   FailureEffortRejected,
					Status: status,
					// 保留上游 msg 原文；取不到时回退截断的 body。
					Message: "上游不接受本次指定的思考档位（reasoning_effort）：" +
						upstream.EffortRejectedDetail(string(respBody)),
				}
			}

			// 模型在这条通道上不存在（11102）：**同样是请求侧错误**，
			// 但必须说清"等也没用"，而不是让用户以为要等账号恢复。
			//
			// 为什么不轮转：同名模型可能只在某一侧上游存在（见 capability.go
			// 的 regionNote）。同一个模型名发给任何账号都会被同一侧拒 ——
			// 轮转只是把同一个请求对着每个账号重传一遍，最后还是失败，
			// 且账号会被无谓地记上错误（实测旧行为正是如此）。
			//
			// 所有者 2026-09-20 的现场：他用裸名 `Qwen3.8-Flash`（放行清单里
			// 就是裸名），网关挑到了没有该模型的区域，回 11102，最终展示成
			// 「all accounts unavailable (cooling/disabled)」——
			// 他的反应是「这个模型不能用」，而真实原因是**区域没指定**。
			if kind == upstream.ErrModelNotInRegion {
				uid := acct.UID
				releaseHeld()
				return &chatResult{UID: uid}, status, &forwardFailure{
					Kind:   FailureModelNotInRegion,
					Status: status,
					Message: upstream.ModelNotInRegionMessage(
						modelOf(body), string(respBody)),
				}
			}

			h.applyErrorPolicy(acct.UID, model, kind, string(respBody))
			fail(acct.UID)
			continue
		}

		// ⚠ 流式请求必须在**写响应头之前**确认这条流真的有内容。
		//
		// # 为什么（真实缺陷，2026-09-19 现场：empty upstream stream）
		//
		// 旧实现在这里直接 `NoteSuccess` 然后 return，把 rc 交给调用方转写。
		// 但上游有一种失败形态是 **HTTP 200 + 空流**：
		//
		//	ZCode 对话通道对受限账号返回 200，流里一帧有效数据都没有
		//	（实测 code=3012 "unusual activity"）
		//
		// 后果有两个，都很严重：
		//  1. 用户收到 `empty upstream stream`，而状态码 200 已经写出去了，
		//     上层**再也无法换账号重试**；
		//  2. `NoteSuccess` 已调用 → 坏账号被记成"成功"、**永不冷却**
		//     → 每次请求都选中它 → 用户的对话**持续失败**。
		//
		// 修法：预读首帧再决定。此时还没写任何响应头，可以安全地
		// 走**已有的失败策略**（applyErrorPolicy 分类 + 冷却）并换号重试。
		//
		// ⚠ 复用 applyErrorPolicy 而不是自己调 Cooldown：账号状态的
		// 分类规则（哪些错误该冷却、多久、是否禁用）集中在那一个函数里，
		// 另起一套会让同一个错误在不同路径下产生不同后果。
		if stream && rc != nil && status < 400 {
			probed, first, perr := upstream.ProbeFirstFrame(rc, h.probeTimeoutFor())
			rc = probed // 首帧已从 rc 消费，必须接回去（否则丢帧）
			var probeMsg string
			if perr != nil || first == "" {
				probeMsg = "上游返回空流（无有效数据帧）"
			} else if bad, reason := upstream.IsUpstreamErrorFrame(first); bad {
				probeMsg = "上游首帧即错误：" + reason
			}
			if probeMsg != "" {
				log.Printf("chat uid=%s product=%s: %s — 冷却换号",
					acct.UID, acct.ProductOf(), probeMsg)
				// 用 503 让 applyErrorPolicy 走"服务端/上游故障"分类
				//（它会据此选择冷却时长与是否停用），而不是"请求侧错误"。
				kind := upstream.Classify(http.StatusServiceUnavailable, probeMsg)
				lastStatus = http.StatusServiceUnavailable
				lastKind = kind
				lastBody = probeMsg
				lastTransportErr = nil
				lastErr = errors.New(probeMsg)
				h.applyErrorPolicy(acct.UID, model, kind, probeMsg)
				fail(acct.UID)
				rc.Close()
				if heldUID != "" {
					h.cfg.Pool.Release(heldUID)
					heldUID = ""
				}
				continue
			}
		}

		h.cfg.Pool.NoteSuccess(acct.UID)
		// 粘性跟随最终成功号。
		if sessKey != "" && h.cfg.Session != nil {
			h.cfg.Session.Bind(sessKey, acct.UID)
		}

		uid := acct.UID
		handedOff = true // 租约移交调用方，由其读完/关闭后释放

		product := ""
		if isProduct {
			product = acct.ProductOf()
		}

		if stream {
			return &chatResult{UID: uid, Model: modelOf(body), Product: product, Stream: rc}, status, nil
		}

		// 非流式：按产品选择聚合方式。
		//
		// ⚠ 必须用 isProduct 而不是某个具体产品的判定 —— 我第一版只判了
		// `isQoder`，于是 **ZCode 的账号会走 WorkBuddy 的聚合器**
		//（upstream.Aggregate），结果是拿 ZCode 的流去按 WorkBuddy 的
		// 解析逻辑处理，表现为"无法连接上游"这种与真实原因无关的报错。
		//
		// 新增产品时这里容易漏 —— 故用产品无关的 `productUp` 变量。
		var resp map[string]any
		var aggErr error
		if isProduct {
			resp, aggErr = productUp.Aggregate(rc, modelOf(body))
		} else {
			resp, aggErr = upstream.Aggregate(rc)
		}
		rc.Close()
		h.cfg.Pool.Release(uid)
		handedOff = false
		heldUID = ""
		if aggErr != nil {
			return nil, http.StatusBadGateway, aggErr
		}
		return &chatResult{UID: uid, Model: modelOf(body), Product: product, Response: resp}, http.StatusOK, nil
	}

	msg := "all accounts unavailable (cooling/disabled)"
	if lastErr != nil {
		// 客户端可读性：优先用提炼后的原因（如「账号额度已耗尽…」），
		// 拿不到再用原始文案 —— 原始文案含整段上游 JSON 或 tcp 底层细节，又长又难懂。
		if friendly := upstream.FriendlyMessage(lastKind, lastStatus, lastBody); friendly != "" {
			msg = friendly
		} else if lastTransportErr != nil {
			msg = "无法连接上游（网络超时 / 连接被拒）：请检查本机网络或代理设置后重试"
		} else {
			msg += ": " + lastErr.Error()
		}
	}
	// 失败时也带上最后尝试过的账号，请求日志据此仍能显示 uid（与原实现一致）。
	return &chatResult{UID: lastUID}, clientFacingStatus(lastStatus), errors.New(msg)
}

// clientFacingStatus 把内部状态码归一成**不会误导客户端**的状态码。
//
// # 为什么需要它（2026-09-20 实测缺陷，所有者报告）
//
// 所有者原话：
//
//	「密钥明明是正确的，不知道怎么对话过程中就提示『本轮运行失败 API 密钥无效』，
//	  而且我还是可以通过我配置的密钥获取到模型的名称」
//
// 查证结论：**密钥从头到尾都是对的**，是状态码被客户端误读了。
//
// DSH Desktop 客户端的映射（`dsh-llm-deepseek/lib/index.js:1527`）：
//
//	if (status === 401 || status === 403) return "AUTH";
//
// 而 `AUTH` 在界面上被渲染成 **「API 密钥无效」**
//（`dsh-client-ui-chat/lib/client.js:2696` 的 `message.failure.auth`）。
//
// 于是完整因果是：
//
//	上游返回 403（排队 / 额度 / 风控 / WAF）
//	→ 我们**原样透传 403**
//	→ 客户端把 403 归类为 AUTH
//	→ 界面显示「API 密钥无效」  ← 与事实完全不符
//
// 用户据此会去**反复改密钥**，而真正原因是上游拒绝了这次请求 ——
// 这是最坏的一类误导：它把用户引向一个不可能修好的方向。
//
// # 为什么改成 502 而不是保留 403
//
// 语义上 403 是「服务器理解请求但拒绝执行」，属于**上游**的决定；
// 而 401 才是「你没带对凭据」。403 被客户端当成凭据问题纯属误读。
// 502 Bad Gateway 准确表达「上游拒绝了/不可用」，且客户端不会把它
// 归类成 AUTH，而是走 `SERVER` 分支 —— 那才是可重试的正确语义。
//
// ⚠ **我们自己从不回 403**（鉴权中间件只用 401，见 handler.go:208），
// 故这里把 403 改掉不会与"我们自己的拒绝"混淆。
//
// 401 不在这里处理：那是我们自己的鉴权结果，**必须**保持 401 让客户端
// 提示"密钥无效" —— 那种情况下提示是对的。
func clientFacingStatus(status int) int {
	if status == http.StatusForbidden {
		return http.StatusBadGateway
	}
	return status
}

// FailureKind 失败类别：决定回给客户端的错误码。
//
// 存在的意义是让**请求侧**错误说实话：旧实现无论什么原因都回
// no_healthy_account + "all accounts unavailable (cooling/disabled)"，
// 把「这次请求太大」伪装成「账号全挂了」，排查时被直接带偏（2026-09-15 现场）。
type FailureKind int

const (
	// FailureUpstream 其余上游失败：沿用既有契约（503 no_healthy_account）。
	// 账号池耗尽的语义由它承载，客户端据此稍后重试。
	FailureUpstream FailureKind = iota
	// FailureContextTooLong 请求上下文超出模型窗口：请求侧错误，换号无用。
	FailureContextTooLong
	// FailureImageRegionUnavailable 带图片的请求需要一个特定区域的账号，
	// 而账号池里该区域此刻没有可用账号。
	//
	// 单独成型是为了让错误文案说清「缺什么、该怎么办」—— 与账号池耗尽的
	// 503 不同，这不是「稍后重试就好」，而是「你得加一个那个区域的账号」。
	FailureImageRegionUnavailable
	// FailureEffortRejected 上游不接受本次指定的思考档位：请求侧错误，换号无用。
	//
	// 与 FailureContextTooLong 同一性质（用户的请求需要改，而不是账号有问题），
	// 但它更依赖**上游原文**：该换哪个档位取决于具体模型，没有任何通用表
	//（实测 off 在 14/16 个模型被接受，却被两个 deepseek 模型拒绝），
	// 所以文案必须带回上游说的话，而不是替用户猜一个「可用范围」。
	FailureEffortRejected
	// FailureEgressIPBlocked 多个不同账号在短时间内一起被 WAF 403，
	// 判定为**出口 IP 被拦**（详见 wafip.go）。
	//
	// 为什么要单独成型：它与「账号池耗尽」的默认契约恰好相反 ——
	// 默认契约说「稍后重试就好」（账号会恢复），而这里的结论是
	// 「**换号无用**，问题在我们的网络出口」，用户要查的是代理 / VPN /
	// 公网 IP。混进 no_healthy_account 会让他去查账号池，方向完全错。
	FailureEgressIPBlocked
	// FailureModelNotInRegion 该模型在当前通道/区域上不存在（上游 11102）。
	//
	// 所有者 2026-09-20 原话：
	//
	//	「这个模型,如果是排队,就应该直接报错出来要排队多久,
	//	  而不是说这个模型不能用」
	//
	// 它把这类错误看成"排队"，而实际是**区域不匹配**（排队是 10605，
	// 已有可读文案）。但用户的诉求成立：**消息在误导** ——
	// 此前它被轮转后包成 `503 no_healthy_account` +
	// 「all accounts unavailable (cooling/disabled)」，让人以为要等账号恢复。
	//
	// 故单独成型：不轮转、不冷却账号，直接说"这个模型在这条通道上不存在，
	// 等多久都不会出现"，并给出可操作的出路（换模型 / 用区域前缀）。
	FailureModelNotInRegion
)

// imageRegionUnavailableMessage 生成「带图片请求缺少该区域账号」的说明。
//
// 必须同时给出**原因**与**出路**：这类失败用户第一次遇到时完全无法自行判断
// （网关返回 503，客户端只显示「服务不可用」），而原因（上游两区同名模型的
// 图像能力不同）与出路（补一个那个区域的账号）都只有网关知道。
func imageRegionUnavailableMessage(region auth.Region) string {
	name := "国服"
	if region == auth.RegionIntl {
		name = "国际版"
	}
	return "该模型带图片的请求需要" + name + "账号（实测只有" + name +
		"后端能读取图片，另一个区域会把图片替换成占位符后交给模型，" +
		"表现为模型回复「无法查看图片」），但账号池里此刻没有可用的" + name +
		"账号。请添加/启用一个" + name + "账号，或去掉图片后重试。"
}

// forwardFailure 一次需要特殊上报的转发失败。
//
// 只有需要偏离「503 no_healthy_account」默认契约的失败才用它；
// 其余失败仍是普通 error，行为与旧实现完全一致。
type forwardFailure struct {
	Kind    FailureKind
	Status  int    // 回给客户端的 HTTP 状态
	Message string // 面向客户端的错误消息
}

func (e *forwardFailure) Error() string { return e.Message }

// pickAccount 按 imageRoute 选号。
//
// 与旧的 PickForModelRegion（偏好语义）的区别在于 Required：
//
//	Required=false → 偏好：先在该区域挑，挑不到放开到全池（旧行为）。
//	                 「区域不符但能用」好过因为该区域没号而失败。
//	Required=true  → 强制：只在该区域挑，挑不到返回 nil，由调用方报错。
//	                 此时跨区降级**不是**「能用就行」—— 后端会静默丢弃图片，
//	                 用户拿到的是「模型说它看不见图片」，无从排查。
// pickAccountFor 按**产品 + 区域**选号（product 为空时与 pickAccount 等价）。
//
// # 为什么单独一个函数而不是给 pickAccount 加参数
//
// `pickAccount` 的语义是「按区域选号」，而产品约束是**另一层**：
// 它来自客户端前缀（`qoder:glm-5.3`）且**强制**（不兜底跨平台）。
// 两者混在一个函数里，将来改区域逻辑时容易顺手把产品约束也带上兜底 ——
// 那会让"我明明指定了 qoder"变成"报的是 ZCode 的错"。
//
// 故显式分开，并在 `pool.PickForModelProductRegion` 的注释里写明
// 为什么产品不兜底而区域可以。
func (h *Handler) pickAccountFor(model string, tried map[string]bool, route imageRoute, product string) *auth.Auth {
	if product == "" {
		return h.pickAccount(model, tried, route)
	}
	// 产品约束走 Strict 路径（见 PickForModelProductRegion 的说明）：
	// 无论 route.Required 与否，都不做全冷却兜底 —— 显式指令不降级。
	return h.cfg.Pool.PickForModelProductRegion(model, tried, route.Region, product)
}

// pickAccount 按区域选号（无产品约束）。
func (h *Handler) pickAccount(model string, tried map[string]bool, route imageRoute) *auth.Auth {
	if route.Region == auth.RegionAny {
		return h.cfg.Pool.PickForModelRegion(model, tried, auth.RegionAny)
	}
	if route.Required {
		return h.cfg.Pool.PickForModelRegionStrict(model, tried, route.Region)
	}
	return h.cfg.Pool.PickForModelRegion(model, tried, route.Region)
}

// failureOf 取出 *forwardFailure（若有），供各协议入口按类别选错误码。
func failureOf(err error) *forwardFailure {
	var f *forwardFailure
	if errors.As(err, &f) {
		return f
	}
	return nil
}

// errText 供各协议入口取失败文案。
func errText(err error) string {
	if err == nil {
		return "unknown error"
	}
	return err.Error()
}

// release 供调用方在流式转发结束后归还租约。
func (h *Handler) release(uid string) {
	if uid != "" {
		h.cfg.Pool.Release(uid)
	}
}

// modelOf 从 OpenAI Chat 请求体里取 model 字段（仅用于日志与回填响应）。
func modelOf(body []byte) string {
	var probe struct {
		Model string `json:"model"`
	}
	_ = json.Unmarshal(body, &probe)
	return probe.Model
}

// effortOf 从请求体里取出客户端显式指定的思考档位（两个拼写都认）。
//
// 返回 `any` 而不是 `string`：要区分「没带这个字段」与「带了但值不是字符串」。
// 前者是「用默认档」（放行），后者该由上游去报类型错，网关不替它判。
// 用两个指针字段实现，避免 `map[string]any` 解一遍整个请求体
//（请求体可能很大，含图片 base64）。
func effortOf(body []byte) any {
	var probe struct {
		Snake *string `json:"reasoning_effort"`
		Camel *string `json:"reasoningEffort"`
	}
	if err := json.Unmarshal(body, &probe); err != nil {
		return nil
	}
	// snake 优先：与 normalizeReasoningEffort 的取值顺序一致，
	// 否则校验用一个、改写用另一个，会出现「校验过了却被改成别的档」。
	if probe.Snake != nil {
		return *probe.Snake
	}
	if probe.Camel != nil {
		return *probe.Camel
	}
	return nil
}

// effortsForModel 返回该模型在**任一区域**声明的支持档位（空=未声明）。
//
// 为什么跨区域取并集：请求进到 forwardChat 时还没选号，不知道会用哪个区域的
// 账号（选号在下面才发生）。若只按某一区域校验，会给另一区域的合法档位误报 400。
// 取并集是**保守**方向：宁可漏拦，也不要把合法请求挡在门外 ——
// 漏拦的后果是回到上游处理（与原行为一致），误拦的后果是用户完全无法使用。
//
// 只有「所有区域都没声明」时才返回空（= 无从校验，放行）。
//
// **只读缓存，绝不触发拉取**：本函数在每次 chat 请求的关键路径上，
// 若在这里调 buildCapabilityIndex()，缓存未命中时它会真的去打上游
//（fetchModelsForRegion → FetchModels），后果有两个且都严重：
//   - 每次冷启动后的第一个请求都要先等一次模型拉取；
//   - 测试里那次拉取会被计成一次「上游调用」，让「上下文超长只打上游 1 次」
//     这类计数断言无故失败（实测踩到：TestContextTooLongDoesNotRotateAccounts
//     从 1 次变成 2 次）。
//
// 缓存空 = 尚未探测过模型 → 无从校验 → 放行。这不是妥协：没有能力数据时
// 本来就无法判断，放行等同于本特性引入之前的行为（那时一律透传给上游）。
func (h *Handler) effortsForModel(model string) []string {
	if model == "" {
		return nil
	}
	regionModelCache.Lock()
	defer regionModelCache.Unlock()
	var out []string
	seen := map[string]bool{}
	for _, region := range []auth.Region{auth.RegionCN, auth.RegionIntl} {
		rm := regionModelCache.byRegion[region]
		if rm == nil {
			continue
		}
		for _, mi := range rm.infos {
			if mi.ID != model {
				continue
			}
			for _, e := range mi.Efforts {
				key := strings.TrimSpace(strings.ToLower(e))
				if key == "" || seen[key] {
					continue
				}
				seen[key] = true
				out = append(out, e)
			}
		}
	}
	return out
}

// rewriteModel 把请求体里的 model 字段改成 bare。
//
// 为什么必须改写：`cn:` / `global:` 前缀只是**给网关的选号指令**，
// 上游并不认识它。若把带前缀的原串原样转发，上游会返回
//
//	400 code=11102 model [cn:glm-5.2] service info not found
//
// 即「前缀成功约束了选号，却让请求本身失败」——
// 实测确认过这个现象（前缀请求全部 11102，无前缀的同名请求正常）。
//
// bare 为空或与当前值相同时返回原 body（不重新序列化，避免无谓的格式变化）。
// 解析失败时同样原样返回：宁可让上游报错，也不要把请求体改坏。
func rewriteModel(body []byte, bare string) []byte {
	if bare == "" {
		return body
	}
	var doc map[string]json.RawMessage
	if err := json.Unmarshal(body, &doc); err != nil {
		return body
	}
	cur, ok := doc["model"]
	if !ok {
		return body // 没有 model 字段：无从改写
	}
	var curStr string
	if err := json.Unmarshal(cur, &curStr); err != nil || curStr == bare {
		return body // 不是字符串或无需改动
	}
	encoded, err := json.Marshal(bare)
	if err != nil {
		return body
	}
	doc["model"] = encoded
	out, err := json.Marshal(doc)
	if err != nil {
		return body
	}
	return out
}

// requestHasImage 报告 OpenAI Chat 请求体里是否携带图片分片。
//
// 三种协议入口最终都会把图片归一成 `{"type":"image_url", ...}` 分片
// （responses.go 的 input_image、messages.go 的 Anthropic image 块），
// 因此只需在这里认这一种形状。
//
// 判据用「分片里存在 image_url 键」而不是「type == image_url」：
// 上游/客户端对 image 分片的 type 写法不止一种（实测有 image_url、
// input_image），按 type 精确匹配会漏判，而漏判的后果是请求被路由到
// 读不到图片的后端 —— 正是本函数要避免的。
func requestHasImage(body []byte) bool {
	var probe struct {
		Messages []struct {
			Content json.RawMessage `json:"content"`
		} `json:"messages"`
	}
	if err := json.Unmarshal(body, &probe); err != nil {
		return false
	}
	for _, m := range probe.Messages {
		if len(m.Content) == 0 {
			continue
		}
		// content 可能是字符串（纯文本）或分片数组；只有数组才可能含图片。
		var parts []map[string]any
		if err := json.Unmarshal(m.Content, &parts); err != nil {
			continue
		}
		for _, p := range parts {
			if _, ok := p["image_url"]; ok {
				return true
			}
			if t, _ := p["type"].(string); t == "image_url" || t == "input_image" || t == "image" {
				return true
			}
		}
	}
	return false
}

// imageRoute 带图片的请求该如何选号。
type imageRoute struct {
	// Region 应使用的区域；RegionAny = 不做区域约束。
	Region auth.Region
	// Required 为 true 时**只**在该区域选号，选不出就报错，绝不跨区降级。
	//
	// 与「偏好」的区别是本特性的核心：偏好会在该区域没号时静默回退到读不到图的
	// 后端，用户拿到一句「抱歉，我无法查看图片」却毫不知情；Required 则明确
	// 告诉用户「需要哪个区域的账号」，可排查、可行动。
	Required bool
}

// imageRouteFor 报告「带图片的该模型请求」应使用的区域。
//
// 背景（实测 2026-09-16，逐账号 × 逐模型发图验证，见 measured.go 的实测表）：
//
//	glm-5.3 / glm-5.2 是**两区共有**的模型名，但两区是**不同的后端模型**。
//	国服后端能读图；国际版后端把图片替换成固定占位符（prompt_tokens 增量
//	恒为 +33，与图片体积无关），模型只能回「无法查看图片」。
//
// 池里国服与国际版账号混用，选号又只看到期日与冷却、不看区域，
// 于是同一个 glm-5.3 会随机命中两个后端 —— 用户看到「时好时坏」。
//
// 三种情形：
//
//	不带图片                  → 不约束（RegionAny）。两个区域的文本能力都正常，
//	                            没必要为纯文本放弃一半账号的额度。
//	带图片 + 该模型**只在一区**可读 → Required：迁移到那个区域，选不出就报错。
//	带图片 + 两区都能读        → 不约束（RegionAny）。例如 hy3 / kimi-k2.6
//	                            实测两区都能读，对它们偏好只会白白损失一半额度。
//
// 与 measured.go 的分工：那里给的是「某模型在某区域能不能读图」的**事实**，
// 这里把它翻译成**路由决策**。两者共用同一份实测表，不会各自漂移。
func imageRouteFor(model string, hasImage bool) imageRoute {
	if !hasImage {
		return imageRoute{Region: auth.RegionAny}
	}
	cn := measuredImageCapability(model, auth.RegionCN) == measSupported
	intl := measuredImageCapability(model, auth.RegionIntl) == measSupported
	switch {
	case cn && intl:
		// 两区都能读：不约束。对它们做偏好只会放弃一半账号的额度而无任何收益。
		return imageRoute{Region: auth.RegionAny}
	case cn && !intl:
		return imageRoute{Region: auth.RegionCN, Required: true}
	case intl && !cn:
		return imageRoute{Region: auth.RegionIntl, Required: true}
	default:
		// 两区都读不到，或该模型没有实测结论：不约束，交给运行时按原策略处理。
		// 这里**不**直接报错 —— 「没实测过」不等于「不行」，把没验过的模型
		// 一律拒掉会误伤本可用的图片能力。
		return imageRoute{Region: auth.RegionAny}
	}
}

// buildSessKey 为没有原生会话字段的协议（如 Anthropic Messages）合成粘性键。
//
// Anthropic 请求体没有 conversation_id，但有 system + 首条 user 消息；
// 用它们的短哈希做键即可让同一会话稳定命中同一账号。
func buildSessKey(seed string) string {
	if seed == "" {
		return ""
	}
	return session.SessionKeyFromSeed(seed)
}
