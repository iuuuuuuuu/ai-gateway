package server

import (
	"net/http"
	"strings"

	"workbuddy2api/internal/auth"
)

// admin.go 提供「可观测接口」：/v1/models/regions 与 /debug/*。
//
// 存在的原因：区域路由与能力下发都建立在「上游两区能力不同」这一**实测事实**
// 上，而实测需要能看见真值。没有可观测接口时，用户遇到「同一个模型时好时坏」
// 只能靠猜；有了它，一条 curl 就能确认每个区域到底认哪些模型、图片能力如何、
// 账号各自被判成哪个区域。
//
// 这类接口是必需的（而不是「有更好」）：本次缺陷本身就是
// 「网关悄悄地用了错区域的真值」而完全不可诊断 —— 同样的错误不该在修复后重现。

// modelsRegions 返回按区域划分的模型能力真值（GET /v1/models/regions）。
//
// 与 /v1/models 的差别：后者是给客户端消费的 OpenAI 形状（每个模型名一条），
// 前者是给人看的对比视图，明确列出每个区域有哪些模型、图片能力是什么、
// 真值来自哪里、什么时候拉的。
func (h *Handler) modelsRegions(w http.ResponseWriter, r *http.Request) {
	regionView := func(region auth.Region) map[string]any {
		infos := h.fetchModelsForRegion(region)
		regionModelCache.Lock()
		rm := regionModelCache.byRegion[region]
		var fetched, failed any
		if rm != nil {
			if !rm.fetched.IsZero() {
				fetched = rm.fetched.Format("2006-01-02T15:04:05Z07:00")
			}
			if !rm.lastErr.IsZero() {
				failed = rm.lastErr.Format("2006-01-02T15:04:05Z07:00")
			}
		}
		regionModelCache.Unlock()

		models := make([]map[string]any, 0, len(infos))
		for _, mi := range infos {
			effective := measuredOverride(mi.ID, region, mi.SupportsImages)
			entry := map[string]any{
				"id": mi.ID,
				// 网关**实际依据**的值（实测覆盖后）。指针序列化成 null
				// = 未声明（三态，不是「不支持」）。
				"supportsImages": effective,
				"context_length": mi.ContextWindow,
				"max_tokens":     mi.MaxTokens,
				// 上游声明的**计费倍率**（credit_multiplier）：0 = 免费、>0 = 计费、
				// null = 上游未声明该字段（**不是免费**）。
				//
				// 这是宿主判断「这个账号调这个模型该不该扣分」的**唯一权威依据**：
				// 宿主不自己硬编码免费清单，而是读这里按区域给出的上游真值。
				// 之所以必须按区域暴露：同名模型两区计费不同（实测
				// deepseek-v4.1-flash 国服 x0.03 / 国际版 x0.00），
				// 合并成一份全局清单必然在其中一侧出错。
				"credit_multiplier": mi.CreditMultiplier,
				// 上游原文（如 "x0.00" / "x0.34 credits" / ""），便于人工核对
				// 「倍率是不是解析错了」。解析后与原文并列展示，排查时不必再拉上游。
				"credits_raw": mi.CreditsRaw,
			}
			// 上游说的与网关实际用的不一致时，把上游原话也列出来。
			// 这正是本次缺陷的核心：上游对国际版 glm-5.x 报 true，实测 false。
			// 不显式展示的话，看到 supportsImages=false 的人会以为网关有 bug。
			if !sameBool(entry["supportsImages"], mi.SupportsImages) {
				entry["upstream_claims_supports_images"] = mi.SupportsImages
				entry["overridden_by"] = "measured"
			}
			models = append(models, entry)
		}
		return map[string]any{
			"region":       region.String(),
			"truth_source": truthSourceFor(region, len(infos) > 0),
			"fetched_at":   fetched,
			"last_error":   failed,
			"model_count":  len(models),
			"models":       models,
		}
	}

	writeJSON(w, http.StatusOK, map[string]any{
		"regions": []any{regionView(auth.RegionCN), regionView(auth.RegionIntl)},
		"notes": []string{
			"同名模型可能只在一个区域存在；跨区域调用返回 code=11102 model service info not found。" +
				"网关会按 supported_regions 把该模型的请求路由到它所在的区域。",
			"glm-5.3 / glm-5.2 两区都宣称 supportsImages=true，但实测只有国服后端能读图：" +
				"国际版会把图片替换成固定占位符（prompt_tokens 增量恒为 +33，与图片体积无关）。" +
				"带图片的这两个模型请求因此偏好国服账号。",
			"hy3 / kimi-k2.6 实测两区都能读图，故不做区域偏好 —— 对它们偏好只会白白放弃一半账号的额度。",
		},
	})
}

// truthSourceFor 描述该区域的真值来源，便于排查「为什么能力标注不准」。
func truthSourceFor(region auth.Region, dynamic bool) string {
	if dynamic {
		return "dynamic"
	}
	if region == auth.RegionCN {
		return "static_fallback" // 国服有手抄静态表兜底
	}
	return "unknown" // 国际版静态表非权威，不用于判断存在性
}

// debugHandler 处理 /debug/<area> 路径。
//
// 放在同一函数里而不是各自注册，是为了让「有哪些调试视图」一目了然
// （新增视图只需在这里加一条 case，不会漏改路由注册）。
func (h *Handler) debugHandler(w http.ResponseWriter, r *http.Request) {
	area := strings.Trim(strings.TrimPrefix(r.URL.Path, "/debug/"), "/")
	switch area {
	case "capability":
		h.debugCapability(w, r)
	case "":
		writeJSON(w, http.StatusOK, map[string]any{"areas": []string{"capability"}})
	default:
		writeJSON(w, http.StatusNotFound, map[string]any{
			"error": "未知的调试视图：" + area,
			"areas": []string{"capability"},
		})
	}
}

// debugReviveAccount 复活一个被**系统自动禁用**的账号（POST /debug/accounts/{uid}/revive）。
//
// # 为什么这个端点必须存在（而不是「有更好」）
//
// disabled 会持久化进 state.json，而它此前**只有写入方没有任何清除入口**：
// 账号一旦被自动判定为死号（session 死 / 额度冻结），在网关内就永远救不回来 ——
// 重新导入凭证也无效（upsertLocked 对已存在账号只换凭证、不碰 disabled），
// 用户唯一出路是手改 state.json。这与 /debug/* 的既有宗旨一致：
// **不可诊断、不可恢复的状态本身就是缺陷**，而不是「运维自己想办法」。
//
// # 鉴权与形态刻意与既有 /debug/* 完全一致
//
// 同一个 mux + 同一个 withAuth（见 NewHandler 的路由注册）。本端点比只读视图更敏感
// ——它会改变选号结果，未鉴权暴露等于给人一个「把死号放回流量池」的开关。
//
// # 语义边界（照 pool.ReviveDisabled）
//
//   - 只清**系统判定位** disabled/reason 与连续 12153 计数；
//   - **不动**用户的手工轴 NoRoute（宿主写进凭证文件的「停止接流量」开关）；
//   - **不动**熔断与账号级冷却（它们各有自己的到期路径，复活不假装账号健康）。
//
// 因此「复活成功但仍不接流量」有两种正常可能，响应里都如实透出：
// 该账号仍被用户标了 no_route，或它正处于冷却/熔断期。不显式说明的话，
// 用户会以为复活没生效。
//
// 幂等：账号本来就启用 → 200 + revived=false（不是错误，客户端可安全重试）。
// uid 不在池中 → 404（打错 uid 时给出明确的「查无此号」，而不是静默成功）。
func (h *Handler) debugReviveAccount(w http.ResponseWriter, r *http.Request) {
	uid := r.PathValue("uid")
	if uid == "" {
		writeJSON(w, http.StatusBadRequest, map[string]any{"error": "缺少账号 uid"})
		return
	}
	before, ok := h.cfg.Pool.Status(uid)
	if !ok {
		writeJSON(w, http.StatusNotFound, map[string]any{
			"error": "账号不在池中：" + uid,
			"uid":   uid,
			"hint": "账号池与凭证目录是对齐的（含运行期热加载）。若刚导入凭证，" +
				"稍等一轮目录扫描（默认 5s）后重试；若凭证已被删除，它确实不会在池里。",
		})
		return
	}
	revived := h.cfg.Pool.ReviveDisabled(uid)

	after, _ := h.cfg.Pool.Status(uid)
	resp := map[string]any{
		"uid":     uid,
		"revived": revived,
		"disabled": after.Disabled,
		"no_route": after.NoRoute,
		"reason":   after.Reason,
		"cooling":  after.Cooling,
		// 复活前的禁用原因：这是排查「它当初为什么被判死」的唯一线索，
		// 复活后 reason 已被清空，故必须在这里留下。
		"disabled_reason_before": before.Reason,
	}
	// 「复活了但依然不接流量」的两个真实原因，逐个如实说明（见函数注释）。
	var notes []string
	if !revived {
		notes = append(notes, "该账号本来就未被系统禁用，未做任何改动（幂等）。")
	}
	if after.NoRoute {
		notes = append(notes, "该账号仍被标为「不接流量」(no_route)：这是用户在界面上拨的开关，"+
			"写在凭证文件里，复活只解系统禁用位、不动它。要恢复流量请在界面上关掉该开关。")
	}
	if after.Cooling {
		notes = append(notes, "该账号当前处于冷却/熔断期，冷却到期或下一次成功请求后才会恢复接流量"+
			"（复活不清冷却与熔断）。")
	}
	if len(notes) > 0 {
		resp["notes"] = notes
	}
	writeJSON(w, http.StatusOK, resp)
}

// debugCapability 返回能力真值的原始视图（含静态表与账号区域判定）。
//
// 刻意把静态表也列出来：本次修复中最难发现的一类问题是
// 「静态表与上游真值不一致」—— 客户端按静态表选了模型，运行时却失败。
func (h *Handler) debugCapability(w http.ResponseWriter, r *http.Request) {
	idx := h.buildCapabilityIndex()
	var cnIDs, intlIDs []string
	for _, mi := range h.fetchModelsForRegion(auth.RegionCN) {
		cnIDs = append(cnIDs, mi.ID)
	}
	for _, mi := range h.fetchModelsForRegion(auth.RegionIntl) {
		intlIDs = append(intlIDs, mi.ID)
	}

	writeJSON(w, http.StatusOK, map[string]any{
		"dynamic_cn_ids":   cnIDs,
		"dynamic_intl_ids": intlIDs,
		"static_cn_ids":    staticIDs(staticModels),
		"static_intl_ids":  staticIDs(staticModelsIntl),
		"cn_truth_known":   idx.knownRegion[auth.RegionCN],
		"intl_truth_known": idx.knownRegion[auth.RegionIntl],
		"accounts":         h.debugAccounts(),
	})
}

// debugAccounts 列出账号池里每个账号的 UID、区域与域名，用于核对区域判定。
//
// 区域判定依据 domain 后缀（.ai → 国际版），domain 缺失时归国服。
// 这一条最值得人工核对，因此把原始 domain 一并透出来。
func (h *Handler) debugAccounts() []map[string]any {
	out := []map[string]any{}
	for _, uid := range h.cfg.Pool.AllUIDs() {
		a := h.cfg.Pool.AuthByUID(uid)
		if a == nil {
			continue
		}
		out = append(out, map[string]any{
			"uid":      uid,
			"region":   a.Region().String(),
			"domain":   a.Domain,
			"nickname": a.Nickname,
		})
	}
	return out
}

func staticIDs(entries []map[string]any) []string {
	out := make([]string, 0, len(entries))
	for _, m := range entries {
		if id, _ := m["id"].(string); id != "" {
			out = append(out, id)
		}
	}
	return out
}

// sameBool 比较两个 *bool（含 nil 语义）。
func sameBool(a any, b *bool) bool {
	get := func(v any) *bool {
		switch t := v.(type) {
		case *bool:
			return t
		case bool:
			return &t
		case nil:
			return nil
		}
		return nil
	}
	x := get(a)
	if x == nil || b == nil {
		return x == nil && b == nil
	}
	return *x == *b
}
