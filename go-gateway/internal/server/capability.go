package server

// capability.go 维护**按区域**的模型能力真值，供两处共用：
//
//  1. /v1/models 下发模型能力（图片输入等）；
//  2. forwardChat 在「带图片的请求」上做区域路由。
//
// 为什么必须按区域分开存：同名模型在两个区域可能是**不同的后端模型**，
// 图片能力并不一致。实测（2026-09-16，本机 2 国服 + 5 国际版账号逐账号发图）：
//
//	glm-5.3 / glm-5.2
//	  国服   能读图。64x64 纯红图 → prompt_tokens +22，回答「红色」
//	  国际版 读不到图。同样的图 → prompt_tokens 恒 +33（与图片体积无关：
//	          换成 512x512 蓝图仍是 +33），回答「抱歉，我无法查看图片」
//	  两区都宣称 supportsImages=true —— 所以**不能靠能力声明区分**，
//	  只能靠运行时区域路由。
//
//	hy3（反例，说明了为什么白名单必须来自逐账号实测）
//	  两区都能读图（token 增量随图片体积变化、能答对颜色），
//	  但国服增量 +22、国际版 +159 —— 国际版走的是另一种图片编码，
//	  能力相同、实现不同。
//
//	kimi-k2.6
//	  两区都能读图（增量 +17 一致）。
//
// 因此真值必须逐区域拉取并分开缓存：用国服账号拉到的清单去标注国际版模型，
// 会把「能读图」谎报成事实，客户端据此发出必然被静默降级的请求。

import (
	"sync"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/upstream"
)

// regionModels 一个区域拉到的模型清单及其拉取时间。
type regionModels struct {
	infos   []upstream.ModelInfo
	fetched time.Time
	lastErr time.Time // 最近一次失败（负缓存用）
}

// regionModelCache 按区域缓存模型清单。
//
// 为什么每个区域各留一份：动态拉取是「抽一个账号去问」，而账号属于某个区域，
// 拿到的清单只对那个区域成立。缓存在这里按区域分桶之后，混合账号池下
// /v1/models 才能把「国服能力」与「国际版能力」分别标注，而不是用其中
// 一个区域的真值去覆盖另一个区域的同名模型。
var regionModelCache = struct {
	sync.Mutex
	byRegion map[auth.Region]*regionModels
}{byRegion: map[auth.Region]*regionModels{}}

// resetModelsCache 清空按区域的模型清单缓存。
//
// 供测试在用例之间隔离（缓存是包级全局的，不清会让上一个用例的上游假响应
// 渗进下一个用例）。生产路径不调用 —— 缓存过期由 TTL 与负缓存负责。
func resetModelsCache() {
	regionModelCache.Lock()
	regionModelCache.byRegion = map[auth.Region]*regionModels{}
	regionModelCache.Unlock()
}

// cachedModelCount 返回缓存中模型条目总数（跨区域求和）。
//
// 供测试断言「动态拉取成功且已入缓存」。跨区域求和是因为修复后按区域分桶：
// 单区账号的测试只会填一个桶，多区账号会填两个，求和才能覆盖两种情况。
func cachedModelCount() int {
	regionModelCache.Lock()
	defer regionModelCache.Unlock()
	n := 0
	for _, rm := range regionModelCache.byRegion {
		n += len(rm.infos)
	}
	return n
}

// ageModelsCacheFailure 把所有区域的「失败时间戳」人为拨旧，用于测试负缓存过期。
//
// 直接改时间戳而不是 sleep：负缓存时长为 5 分钟，真等会让测试无法接受。
func ageModelsCacheFailure(d time.Duration) {
	regionModelCache.Lock()
	defer regionModelCache.Unlock()
	for _, rm := range regionModelCache.byRegion {
		if !rm.lastErr.IsZero() {
			rm.lastErr = time.Now().Add(-d)
		}
	}
}

// fetchModelsForRegion 拉取指定区域的模型清单（带 1h 缓存与 5min 负缓存）。
//
// 与旧 fetchDynamicModels 的差别：账号是**按区域**挑的，不再是「池中任一健康
// 账号」。旧做法在混合池下会随选号抖动（今天抽到国服、明天抽到国际版），
// 于是同一客户端的模型清单在两次启动之间可能整体换一套。
func (h *Handler) fetchModelsForRegion(region auth.Region) []upstream.ModelInfo {
	regionModelCache.Lock()
	if rm := regionModelCache.byRegion[region]; rm != nil {
		if len(rm.infos) > 0 && time.Since(rm.fetched) < dynamicModelsTTL {
			out := rm.infos
			regionModelCache.Unlock()
			return out
		}
		if !rm.lastErr.IsZero() && time.Since(rm.lastErr) < modelsFetchFailCooldown {
			regionModelCache.Unlock()
			return nil
		}
	}
	regionModelCache.Unlock()

	acct := h.pickProbeAccountInRegion(region)
	if acct == nil {
		return nil
	}
	infos, err := h.cfg.Upstream.FetchModels(acct)
	if err != nil || len(infos) == 0 {
		// **不喂熔断器**：/v3/config 是能力探测接口，它的失败不代表该账号不能聊天。
		// 历史上曾在此调 NoteError，导致国际版账号（恰好占据最早到期档位、
		// 且旧端点在国际版恒 500）被反复记失败直到熔断。
		regionModelCache.Lock()
		rm := regionModelCache.byRegion[region]
		if rm == nil {
			rm = &regionModels{}
			regionModelCache.byRegion[region] = rm
		}
		rm.lastErr = time.Now()
		regionModelCache.Unlock()
		return nil
	}
	regionModelCache.Lock()
	regionModelCache.byRegion[region] = &regionModels{infos: infos, fetched: time.Now()}
	regionModelCache.Unlock()
	return infos
}

// pickProbeAccountInRegion 在指定区域里挑一个用于探测模型清单的账号。
//
// region=RegionAny 时「优先国服、其次国际版」：探测只读能力发现，不需要遵循
// 分层/轮转选号策略，那些策略是给流量用的。优先国服是因为历史上国际版的
// 旧探测端点恒 500（现端点 /v3/config 两区都可用，这里只为兼容）。
//
// region=具体区域时**只在那个区域里找**，找不到返回 nil —— 不退回其它区域。
// 拿国际版账号去拉清单再标注成「国服真值」，正是本次修复要消除的错误；
// 返回 nil 让调用方明确知道「该区域这次没有真值」，进而退回保守行为。
func (h *Handler) pickProbeAccountInRegion(region auth.Region) *auth.Auth {
	var anyIntl *auth.Auth
	for _, uid := range h.cfg.Pool.AvailableUIDs() {
		a := h.cfg.Pool.AuthByUID(uid)
		if a == nil {
			continue
		}
		intl := a.Region() == auth.RegionIntl
		if region == auth.RegionAny {
			if intl {
				if anyIntl == nil {
					anyIntl = a
				}
				continue
			}
			return a
		}
		if a.Region() == region {
			return a
		}
		if intl {
			anyIntl = a
		}
	}
	if region != auth.RegionAny {
		// 该区域没有账号：不给跨区域账号，避免把另一个区域的真值标成这个区域的。
		return nil
	}
	// RegionAny 且池里只有国际版：仍返回一个，让探测能自愈（上游修好端点时）。
	return anyIntl
}

// regionCapability 一个区域里某个模型的能力事实。
type regionCapability struct {
	// Present 该区域的上游是否列出了这个模型。
	//
	// 「列了但不支持图片」与「压根没列这个模型名」是两种不同的事，用户
	// 需要看到的也是不同的提示（换模型 vs 换账号/换区），因此分开表达。
	Present bool
	// SupportsImages 该区域的**有效**图片能力（实测覆盖后的结果）。
	//
	// nil = 未声明（上游未声明且无实测结论），三态中的「不知道」。
	SupportsImages *bool
	// UpstreamImages 上游原本声明的值（未经实测覆盖），仅供对比展示。
	//
	// 保留它是为了可诊断：上游说支持、实测说不支持时，用户能一眼看出
	// 差异出在「上游元数据」而不是网关判断。
	UpstreamImages *bool
	// FromStatic 该条目来自手抄兜底表，而非上游真值。
	//
	// 用途：判断「另一个区域没有这个模型」时必须排除它。静态表会过期
	// （实测踩过：国服静态表漏了 glm-5.3，于是网关把带图片的 glm-5.3
	// 请求判成「只能去国际版」，正好推向读不到图的那一侧）。
	// 静态表只该用来「提供信息」，不该用来「否定存在」。
	FromStatic bool
}

// capabilityIndex 按区域索引的模型能力表。
type capabilityIndex struct {
	byRegion map[auth.Region]map[string]regionCapability
	// knownRegion 该区域的清单是否来自**真实拉取**（而非静态兜底）。
	// 静态兜底表是手抄的、可能过期，用它做「不存在」判断会误伤。
	knownRegion map[auth.Region]bool
}

// buildCapabilityIndex 汇总各区域的能力真值。
//
// 每个区域都走「动态优先、静态兜底」的两级来源：
//
//	动态拉到 → 用它（真值，含上游此刻的 supportsImages）
//	拉不到   → 用手抄静态表（该区域的模型清单 + 实测图片能力）
//
// 为什么静态表也要入表（而不是「拉不到就不建桶」）：静态表是**该区域的**
// 清单，本身就是一份可用的事实来源。它比动态真值旧，但远好于「没有」——
// 没有会让 /v1/models 对整批模型都不下发能力字段，客户端全部按纯文本处理，
// 图片能力整个消失（而网关看起来完全正常，是最难排查的一类问题）。
//
// knownRegion 只对**动态真值**置位：静态表用于「该区域有哪些模型」是可信的，
// 但用于「另一区域没有某模型」的判定则不可信（表可能过期）。两者用途不同，
// 因此分开表达。
func (h *Handler) buildCapabilityIndex() *capabilityIndex {
	idx := &capabilityIndex{
		byRegion:    map[auth.Region]map[string]regionCapability{},
		knownRegion: map[auth.Region]bool{},
	}
	add := func(region auth.Region, infos []upstream.ModelInfo, dynamic bool) {
		m := idx.byRegion[region]
		if m == nil {
			m = map[string]regionCapability{}
			idx.byRegion[region] = m
		}
		for _, mi := range infos {
			if mi.ID == "" {
				continue
			}
			m[mi.ID] = regionCapability{
				Present: true,
				// 实测结论覆盖上游声明：上游对「两区同名、后端不同」的模型
				// 会给出同一份（错误的）supportsImages，必须以实测为准。
				// 未实测的模型原样透传上游声明（含三态 nil）。
				SupportsImages: measuredOverride(mi.ID, region, mi.SupportsImages),
				// UpstreamImages 保留上游原话，供 /v1/models/regions 对比展示 ——
				// 「上游说什么」与「实测是什么」不一致时，最需要能一眼看出来。
				UpstreamImages: mi.SupportsImages,
				// FromStatic 标记这条来自手抄兜底表而非上游真值。
				// 判断「另一个区域没有该模型」时只看真值，静态表不算数
				//（表会过期，曾因它漏了 glm-5.3 而把图片请求推向读不到图的一侧）。
				FromStatic: !dynamic,
			}
		}
	}

	for region, static := range map[auth.Region][]map[string]any{
		auth.RegionCN:   staticModels,
		auth.RegionIntl: staticModelsIntl,
	} {
		if infos := h.fetchModelsForRegion(region); len(infos) > 0 {
			add(region, infos, true)
			idx.knownRegion[region] = true
			continue
		}
		add(region, infosFromStatic(static), false)
	}
	return idx
}

// infosFromStatic 把静态表条目转成 ModelInfo（仅用于国服兜底路径）。
//
// 静态表带 supportsImages=true（见 withImageCapability 的注释：该清单
// 实测全部支持图片），故这里恒为 true；它只在动态拉取失败时生效。
func infosFromStatic(entries []map[string]any) []upstream.ModelInfo {
	yes := true
	out := make([]upstream.ModelInfo, 0, len(entries))
	for _, m := range entries {
		id, _ := m["id"].(string)
		if id == "" {
			continue
		}
		out = append(out, upstream.ModelInfo{ID: id, SupportsImages: &yes})
	}
	return out
}

// lookupRegion 返回 (区域能力, 该区域是否列出了该模型)。
func (i *capabilityIndex) lookupRegion(id string, region auth.Region) (regionCapability, bool) {
	if i == nil {
		return regionCapability{}, false
	}
	m := i.byRegion[region]
	if m == nil {
		return regionCapability{}, false
	}
	c, ok := m[id]
	return c, ok
}

// mergedModelList 生成 /v1/models 的 data 数组。
//
// 语义变化（本次修复的核心）：**每个模型名只出现一次**，能力取自
// 它与两区的关系，而不是「先按一个区的清单铺开、再用另一个区的静态表补齐」。
//
// 旧做法有两个问题：
//   - 同名模型（glm-5.3/glm-5.2/hy3/kimi-k2.6）在动态清单里带的是「抽中的那个
//     区域」的真值，另一个区域独有模型靠静态表补 —— 于是同名模型的能力标注
//     取决于这次探测抽到哪个区，客户端两次启动可能看到不同能力；
//   - 补进来的静态条目与动态条目在同一个数组里，客户端无法区分。
func (h *Handler) mergedModelList() []map[string]any {
	idx := h.buildCapabilityIndex()

	seen := map[string]bool{}
	var order []string
	base := map[string]map[string]any{}

	// 静态表提供 created/owned_by/context_length 兜底元数据。
	for _, entries := range [][]map[string]any{staticModels, staticModelsIntl} {
		for _, m := range entries {
			id, _ := m["id"].(string)
			if id == "" {
				continue
			}
			if _, ok := base[id]; !ok {
				base[id] = m
			}
		}
	}
	addID := func(id string) {
		if id == "" || seen[id] {
			return
		}
		seen[id] = true
		order = append(order, id)
	}

	// 顺序：先两区动态真值（国服在前），再静态表里剩余的条目。
	// 「国服在前」是刻意的：客户端模型菜单常按返回顺序展示，
	// 国服是图片能力与模型覆盖都更完整的一侧，排前面更容易被选中。
	cnInfos := h.fetchModelsForRegion(auth.RegionCN)
	intlInfos := h.fetchModelsForRegion(auth.RegionIntl)
	for _, infos := range [][]upstream.ModelInfo{cnInfos, intlInfos} {
		for _, mi := range infos {
			addID(mi.ID)
		}
	}
	for _, entries := range [][]map[string]any{staticModels, staticModelsIntl} {
		for _, m := range entries {
			if id, _ := m["id"].(string); id != "" {
				addID(id)
			}
		}
	}

	// 动态元数据（context_length/max_output_tokens）比静态表准，优先用。
	dynMeta := map[string]upstream.ModelInfo{}
	for _, infos := range [][]upstream.ModelInfo{cnInfos, intlInfos} {
		for _, mi := range infos {
			if _, ok := dynMeta[mi.ID]; !ok {
				dynMeta[mi.ID] = mi
			}
		}
	}

	out := make([]map[string]any, 0, len(order))
	for _, id := range order {
		e := map[string]any{
			"id":       id,
			"object":   "model",
			"created":  1753600000,
			"owned_by": "workbuddy",
		}
		if b := base[id]; b != nil {
			for _, k := range []string{"created", "owned_by", "context_length", "max_output_tokens"} {
				if v, ok := b[k]; ok {
					e[k] = v
				}
			}
		}
		if mi, ok := dynMeta[id]; ok {
			if mi.ContextWindow > 0 {
				e["context_length"] = mi.ContextWindow
			}
			if mi.MaxTokens > 0 {
				e["max_output_tokens"] = mi.MaxTokens
			}
		}
		if _, ok := e["context_length"]; !ok {
			e["context_length"] = int64(131072) // 兜底
		}
		for k, v := range idx.capabilityFieldsFor(id) {
			e[k] = v
		}
		out = append(out, e)
	}
	return out
}

// capabilityFieldsFor 生成某模型名应下发的能力字段。
//
// 三类情形，都以「该模型名在当前网关下**实际可用时**的能力」为准：
//
//	两区都有        → 用国服真值（国服清单权威，且与「带图片的请求偏好国服」
//	                  的运行时策略一致）
//	只一区有        → 用该区的真值，并附 supported_regions 说明
//	未收录          → 不下发任何能力字段（三态中的「未声明」）
//
// 关键：**只在一区存在并不等于不支持图片**。网关会按区域路由把该模型的请求
// 送到它所在的区域（见 forward.go 的 regionRouting），因此这里必须如实下发
// 该区域的能力，而不是降级为 false —— 降级会让客户端隐藏图片入口，
// 用户失去一个本来可用的能力，而这是可修的（把账号补到那个区域即可）。
//
// 但**区域归属仍要透出**：supported_regions 让客户端/用户知道该模型名只在
// 某一侧上游存在，出现 11102 model service info not found 时能立刻明白原因
// （而不是怀疑模型名拼错）。
func (idx *capabilityIndex) capabilityFieldsFor(id string) map[string]any {
	cn, cnKnown := idx.lookupRegion(id, auth.RegionCN)
	intl, intlKnown := idx.lookupRegion(id, auth.RegionIntl)

	// supported 只收「该区域有**真值**且确实列出该模型」的区域 ——
	// 静态兜底表不算。否则守则会因手抄表过期而误报「国际版没有 glm-5.3」，
	// 让客户端以为它不可用（实测踩过这个坑，见 regionCapability.FromStatic）。
	var supported []string
	if cnKnown && cn.Present && !cn.FromStatic {
		supported = append(supported, "cn")
	}
	if intlKnown && intl.Present && !intl.FromStatic {
		supported = append(supported, "intl")
	}
	// 能力本身仍可用静态兜底的条目（比「没有」强），但**只取有真值的那一侧**；
	// 两侧都没真值时退回静态值，只是不附 region 说明。
	var cap regionCapability
	haveCap := false
	if cnKnown && cn.Present {
		cap, haveCap = cn, true
	} else if intlKnown && intl.Present {
		cap, haveCap = intl, true
	}
	// 两区都没有该模型名：不下发能力字段。宁可不写，也不要谎报成纯文本 ——
	// 后者会让本可用的图片能力被客户端主动关掉。
	if !haveCap {
		return nil
	}

	fields := modelCapabilityFields(cap.SupportsImages)
	if fields == nil {
		return nil
	}
	if len(supported) == 1 {
		// 只在**真值确认**单区可用时附带说明。字段名用 snake_case 与本响应里
		// 其它字段（input_modalities、context_length）一致；已知解析器只取
		// 自己认识的键，多余键不会报错。
		fields["supported_regions"] = supported
		fields["region_note"] = regionNote(supported[0])
	}
	return fields
}

// regionNote 生成面向用户的区域说明（客户端不读时至少人能看到）。
func regionNote(region string) string {
	other := "国际版"
	if region == "intl" {
		other = "国服"
	}
	return "该模型名仅在" + regionLabel(region) + "上游存在；账号池里的" +
		other + "账号调用它会返回 11102 model service info not found。"
}

func regionLabel(r string) string {
	if r == "intl" {
		return "国际版"
	}
	return "国服"
}

// modelCapabilityFields 生成模型能力字段（图片输入等）。
//
// 为什么一次下发**多种拼写**：客户端读的字段名各不相同，且都只在各自的
// provider 专用解析器里读，没有统一约定（实测 2026-09-16，见各客户端源码）：
//
//	OpenClaw   OpenAI Codex  → input_modalities / inputModalities
//	OpenClaw   Copilot       → capabilities.supports.vision
//	OpenClaw   HuggingFace   → architecture.input_modalities
//	OpenClaw   OpenRouter    → architecture.modality（"text+image->text"）
//	OpenClaw   Vercel AI GW  → tags 含 "vision"
//	OpenClaw   LM Studio     → capabilities.vision
//	ZCode      /v1/models    → 只读 id / supported_formats（不读能力字段）
//	DSH        /v1/models    → 只读 id/name/context/maxTokens（不读能力字段）
//
// 多写几种是安全的：所有已知解析器都只取自己认识的键，遇到多余键不会报错
// （OpenClaw 的 Copilot 解析器只额外要求 object=="model"，本函数已保证）。
// 这样 OpenClaw 等能读该字段的客户端可直接受益，其余客户端行为不变。
//
// supportsImages 为 nil（上游未声明）时**不下发**任何能力字段：宁可不写，
// 也不要谎报成纯文本 —— 后者会让本可用的图片能力被客户端主动关掉。
func modelCapabilityFields(supportsImages *bool) map[string]any {
	if supportsImages == nil {
		return nil
	}
	if !*supportsImages {
		// 显式不支持：明确告知，避免客户端按「默认支持」处理。
		return map[string]any{
			"supportsImages": false,
			"capabilities":   map[string]any{"vision": false, "supports": map[string]any{"vision": false}},
		}
	}
	return map[string]any{
		"supportsImages": true,
		// OpenClaw OpenAI Codex：接受 "image"/"vision" 两种写法。
		"input_modalities": []string{"text", "image"},
		"inputModalities":  []string{"text", "image"},
		// OpenClaw Copilot / LM Studio。
		"capabilities": map[string]any{
			"vision":   true,
			"supports": map[string]any{"vision": true},
		},
		// OpenClaw HuggingFace / OpenRouter。
		"architecture": map[string]any{
			"input_modalities": []string{"text", "image"},
			"modality":         "text+image->text",
		},
		// OpenClaw Vercel AI Gateway。
		"tags": []string{"vision"},
		// ZCode 自身配置用的词汇（对 /v1/models 无消费方，但无副作用且便于人读）。
		"modalities": map[string]any{
			"input":  []string{"text", "image"},
			"output": []string{"text"},
		},
	}
}
