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
	"encoding/json"
	"errors"
	"io"
	"log"
	"net/http"
	"strings"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/session"
	"workbuddy2api/internal/upstream"
)

// modelLockedError 「单一模型」模式拒绝了非目标模型的请求。
//
// 单独成型（而不是拼一个字符串）是为了让调用方能识别它并回以 400 +
// 明确的错误码，而不是当成「账号不可用」的 503 —— 后者会误导用户去查账号。
type modelLockedError struct {
	requested string // 客户端请求的模型（可能为空，表示请求体未带 model）
	allowed   string // 当前锁定的模型
}

func (e *modelLockedError) Error() string {
	got := e.requested
	if got == "" {
		got = "(未指定)"
	}
	return "当前为「单一模型」模式，只允许调用 " + e.allowed + "；收到的是 " + got +
		"。请在客户端把模型改为 " + e.allowed + "，或切换网关的工作模式。"
}

// chatResult 一次成功的上游调用结果。
//
// 二者互斥：
//   - Stream != nil  → 流式：调用方负责 Close，并按目标协议解析/转换 SSE。
//   - Response != nil → 非流式：上游 SSE 已被 Aggregate 成 OpenAI chat.completion。
type chatResult struct {
	UID      string
	Model    string
	Stream   io.ReadCloser
	Response map[string]any
}

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
	// 同时解析可选的区域前缀 `cn:` / `global:`：指定后选号被限制在该区域，
	// 避免把请求发给不支持该模型的区域（上游会返回 11102）。
	// 无前缀时 realm 为空 = 不限制区域，保持既有行为。
	// 注意返回顺序是 (realm, bare) —— 写反会把区域当成模型名，
	// 表现为「单一模型锁定」报「收到的是 (未指定)」。
	realm, model := resolveModel(modelOf(body))

	// 前缀只是给网关的**选号指令**，上游不认识它 —— 必须把请求体里的
	// model 改写成裸名，否则上游返回 400 code=11102 model [cn:xxx] not found
	//（实测确认：前缀成功约束了选号，却让请求本身失败）。
	// 无前缀时 model 与原值相同，rewriteModel 会原样返回，不做多余序列化。
	body = rewriteModel(body, model)

	// 「单一模型」锁定：非空时只放行该模型。
	//
	// 为什么在选号之前就拒绝（而不是换个模型重试）：轮转模式的语义是
	// 「把这个账号的指定模型额度烧干净再换号」，模型是策略的一部分。
	// 若允许其他模型通过，客户端换个模型就能绕过轮转与额度控制，
	// 也让「当前烧的是哪个模型」变得不可预期 —— 因此明确拒绝并说明原因，
	// 比静默改写模型（用户以为在用 A、实际用了 B）更安全。
	//
	// 比较用剥前缀后的裸模型名：用户配的是 `deepseek-v4.1-flash`，
	// 客户端可能带 `cn:` 前缀请求，两者应视为同一个模型。
	if allowed := h.cfg.AllowedModel; allowed != "" && !strings.EqualFold(model, allowed) {
		// 返回非 nil 的 result：调用方会在错误分支里读 result.UID 记日志，
		// 返回 nil 会 panic。UID 留空即可（本次没有选中任何账号）。
		return &chatResult{Model: model}, http.StatusBadRequest,
			&modelLockedError{requested: model, allowed: allowed}
	}

	for i := 0; i < h.cfg.MaxRotate; i++ {
		var acct *auth.Auth
		if stickyUID != "" {
			acct = h.cfg.Pool.PickByUIDForModel(stickyUID, model)
			// 粘性账号也必须满足区域约束：否则会话粘性会把请求
			// 一直钉在错误区域的账号上，前缀指定形同虚设。
			if acct != nil && realm != "" && acct.Realm() != realm {
				acct = nil
			}
			if acct == nil {
				if h.cfg.Session != nil {
					h.cfg.Session.Unbind(sessKey)
				}
				stickyUID = ""
			}
		}
		if acct == nil {
			acct = h.cfg.Pool.PickForModelRealm(model, realm, tried)
		}
		if acct == nil {
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

		// token 临近过期 → 先 refresh（失败冷却换号）
		if acct.NeedsRefresh(h.cfg.RefreshSkew) {
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

		rc, status, respBody, terr := h.cfg.Upstream.ChatStream(acct, body)
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

		if stream {
			return &chatResult{UID: uid, Model: modelOf(body), Stream: rc}, status, nil
		}

		resp, err := upstream.Aggregate(rc)
		rc.Close()
		h.cfg.Pool.Release(uid)
		handedOff = false
		heldUID = ""
		if err != nil {
			return nil, http.StatusBadGateway, err
		}
		return &chatResult{UID: uid, Model: modelOf(body), Response: resp}, http.StatusOK, nil
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
