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

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/prompt"
	"workbuddy2api/internal/session"
	"workbuddy2api/internal/upstream"
)

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
// 参数：
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
func (h *Handler) forwardChat(body []byte, stream bool, sessKey string) (*chatResult, int, error) {
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
	// 同时解析可选的区域前缀 `cn:` / `global:`：前缀只是给网关的**选号指令**，
	// 上游不认识它，故必须把请求体里的 model 改写成裸名（见下方 rewriteModel）。
	// 注意返回顺序是 (realm, bare) —— 写反会把区域当成模型名，
	// 表现为「单一模型锁定」报「收到的是 (未指定)」。
	//
	// realm 不再参与选号：上游已把区域约束升级为 route/preferRegion
	//（含「按区域的模型能力真值」），比字符串 realm 更完整且能区分
	// 「偏好」与「强制」。这里只取 bare 用于改写请求体。
	_, model := resolveModel(modelOf(body))

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
	// mode=passthrough（缺省）时**完全不调用** Rewrite：既有行为必须逐字不变
	//（不重新序列化、不动 messages），因此这里是显式分支而非「传空串让它空转」。
	if h.cfg.PromptMode == prompt.ModeCustom && h.cfg.PromptText != "" {
		body = prompt.Rewrite(body, h.cfg.PromptText)
	}

	body = rewriteModel(body, model)

	// 区域路由：仅对「图像能力两区不同」的模型 + 带图片的请求生效，
	// 其余情况返回 RegionAny，行为与引入本特性之前完全一致。
	// 详见 imageRouteFor 的注释。
	route := imageRouteFor(model, requestHasImage(body))
	preferRegion := route.Region

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

	for i := 0; i < h.cfg.MaxRotate; i++ {
		var acct *auth.Auth
		if stickyUID != "" {
			acct = h.cfg.Pool.PickByUIDForModelRegion(stickyUID, model, preferRegion)
			if acct == nil {
				if h.cfg.Session != nil {
					h.cfg.Session.Unbind(sessKey)
				}
				stickyUID = ""
			}
		}
		if acct == nil {
			acct = h.pickAccount(model, tried, route)
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

			h.applyErrorPolicy(acct.UID, model, kind, string(respBody))
			fail(acct.UID)
			continue
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
	return &chatResult{UID: lastUID}, lastStatus, errors.New(msg)
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
