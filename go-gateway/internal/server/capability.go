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
	"sort"
	"strings"
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
	// **用 ProbeUIDs 而不是 AvailableUIDs**：拉 /v3/config 是只读探测，
	// 不产生流量也不消耗积分，因此被用户标记「不接流量」的账号同样可用 ——
	// 而且它们往往是唯一能提供某个区域真值的账号。
	//
	// 实测踩过：用 AvailableUIDs 时，被禁用的国际版账号拿不到真值，
	// 于是 deepseek-v4.1-flash 被误标成「仅国服存在」（两区其实都有）。
	for _, uid := range h.cfg.Pool.ProbeUIDs() {
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
	// Efforts 该区域上游声明的**允许思考档**（reasoning.supportedEfforts）。
	//
	// 空 = 未声明（含固定档模型、静态兜底表）→ /v1/models 不下发任何档位字段。
	// 与 SupportsImages 同一套「宁可不写也不编造」的语义：编造的档位会被客户端
	// 拿去发请求，而它要么被上游降级、要么被忽略，用户看到的是「调了没生效」。
	Efforts []string
	// DefaultEffort 该区域上游声明的默认思考档（空=未声明）。
	//
	// 与 Efforts 正交：Efforts 答「允许哪些」，本字段答「不指定时用哪档」。
	// 上游未保证默认档一定在 Efforts 里，故两者分别保存、分别下发。
	//
	// 上游用**两个键**表达它：reasoning.defaultEffort（带 supportedEfforts 的
	// 12 个模型）与 reasoning.effort（不带的 18/8 个）。实测两者互斥，
	// upstream 层已合并，这里只存合并后的结果。
	DefaultEffort string
	// SupportsReasoning 上游声明的思考能力（nil = 未声明，**不是** false）。
	//
	// 用途：区分「只有固定档」（Efforts 空但 DefaultEffort 有值）与真正的
	// 「不支持思考」。早先只按 Efforts 是否为空判断，把前者误判成后者 ——
	// 18 个（国服）/ 8 个（国际版）模型因此在界面上显示成「—」。
	SupportsReasoning *bool
	// CanDisableThinking 是否允许关闭思考（reasoning.canDisableThinking）。
	//
	// nil = 未声明。实测多档模型都显式给了它；为 false 时**不能传 off**，
	// 客户端据此把「关闭思考」选项置灰，而不是发一个必然被拒的请求。
	CanDisableThinking *bool
	// CreditMultiplier 该区域上游声明的**计费倍率**（nil = 未声明）。
	//
	// 为什么按区域存而不是按模型名存：同名模型在两区可能是不同的后端、
	// 计费也不同。实测（2026-09-18 拉两区 /v3/config）：
	//
	//	deepseek-v4.1-flash   国服 "x0.03"（计费）   国际版 "x0.00"（免费）
	//
	// 把两区合并成一份「全局免费清单」会让国服的 ds4.1 被误判成免费，
	// 或者反过来让国际版的被误判成计费 —— 两个方向都是「统计与实际不符」，
	// 正是所有者报的那个问题。故这里有区域维度的桶是必需的。
	//
	// 三态语义与 SupportsImages 一致：nil = 未声明（**不是**免费）。
	// 上游只给部分模型写该字段（实测国服 52 个里只有 33 个），
	// 把未声明当免费会把该计费的统计成 0。
	CreditMultiplier *float64
}

// capabilityIndex 按区域索引的模型能力表。
type capabilityIndex struct {
	byRegion map[auth.Region]map[string]regionCapability
	// knownRegion 该区域的清单是否来自**真实拉取**（而非静态兜底）。
	// 静态兜底表是手抄的、可能过期，用它做「不存在」判断会误伤。
	knownRegion map[auth.Region]bool
	// gapReason 该区域**没有真值**的原因（种类 + 面向用户的中文短语）。
	//
	// 只在 knownRegion[region]==false 时设置。分开记是因为「为什么没有真值」
	// 直接决定用户该做什么：没有账号要去启用/同步账号，账号都在冷却要等，
	// 拉取失败要稍后重试。三者若都说成「未知」，用户只能干瞪眼。
	gapReason map[auth.Region]regionGapInfo
}

// regionExplain 一个模型名在**区域维度**上的已知与未知。
//
// 为什么必须把「已知」与「未知」分开存，而不是只留一个 supported 列表 ——
// 这两件事对用户的含义完全不同：
//
//	另一区**有真值且清单里没有它** → 「仅某区存在」（可以断言，用户该换模型）
//	另一区**压根没有真值**         → 只能说「未检测到该区账号，无法确认」
//	                                 （用户该去补/启用那个区的账号）
//
// 所有者反馈的正是后者被说成了前者。完整根因链（逐层可核）：
//
//	① 账号库里 6 个国际版账号全部 disabled=true（实测 accounts.json：
//	   14 个国服 disabled=false + 6 个国际版 disabled=true）；
//	② 宿主导出凭证到网关目录时**跳过了禁用账号**，于是那份目录里一个
//	   `.ai` 域名都没有（实测 gateway_auths/ 共 14 个文件，域名全是
//	   copilot.tencent.com / www.codebuddy.cn）;
//	③ 网关账号池是**扫描该目录**建立的 → 池里没有国际版账号；
//	④ pickProbeAccountInRegion(RegionIntl) 返回 nil（它刻意不跨区回退）
//	   → 拉不到国际版清单 → knownRegion[intl] = false；
//	⑤ capabilityFieldsFor 只看到 supported=[cn] 一个元素，于是下发
//	   supported_regions=["cn"]，`regionNote` 便说「该模型名仅在国服上游存在」。
//
// 而事实是「你没有可用的国际版账号，所以看不到国际版真值」。
// **「没拉到」不等于「不存在」** —— 两者对用户是相反的行动指引：
// 前者要去补账号，后者只能换模型。故这里把结论与原因一并带出。
type regionExplain struct {
	// supported 有真值、且确实列出了该模型的区域（静态兜底表不算，见 FromStatic）。
	supported []string
	// unverified 本轮**没有真值**的区域及其原因。
	unverified []regionGap
}

// regionGap 一个「没有真值」的区域及其原因。
type regionGap struct {
	// code 区域码（"cn" / "intl"），与 supported_regions 同一套取值。
	code string
	// why 面向用户的原因短语（见 capabilityIndex.gapReason）。
	why string
}

// regionCodes 区域码常量。
//
// 与 auth.Region.String() 同值：下发字段与 auth 包必须用同一套拼写，
// 否则前端要认两套码（历史上 region 字段就因拼写不一致出过错）。
const (
	regionCodeCN   = "cn"
	regionCodeIntl = "intl"
)

// regionByCode 把区域码还原成 auth.Region（未知码按国服处理，与 auth 的
// 「domain 缺失按国服」保持同一套保守口径）。
func regionByCode(code string) auth.Region {
	if code == regionCodeIntl {
		return auth.RegionIntl
	}
	return auth.RegionCN
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
		gapReason:   map[auth.Region]regionGapInfo{},
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
				// 思考档：动态路径透传上游声明；静态兜底表**不编造**档位
				//（手抄表里没有 reasoning 信息，编一个会让客户端调了没生效）。
				Efforts:       mi.Efforts,
				DefaultEffort: mi.DefaultEffort,
				// 思考能力三态一并透传，供区分「固定档」与「不支持思考」。
				SupportsReasoning:  mi.SupportsReasoning,
				CanDisableThinking: mi.CanDisableThinking,
				// 计费倍率：动态路径透传上游真值；静态兜底表**不编造**
				//（手抄表里没有 credits，且 infosFromStatic 造不出倍率）。
				// 未声明保持 nil，由消费方显示「不知道」而不是「免费」。
				CreditMultiplier: mi.CreditMultiplier,
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
		// 没有真值：记下**原因**，供 capabilityFieldsFor 生成准确文案。
		// 顺序即优先级：先排除「池里没有该区账号」（最需要用户动手），
		// 再区分「有账号但这次没拉到」（稍后重试即可）。
		idx.gapReason[region] = h.regionGapReason(region)
		add(region, infosFromStatic(static), false)
	}
	return idx
}

// regionGapReason 说明「该区域为什么没有真值」，返回面向用户的中文短语。
//
// 为什么要分三种原因而不是统一说「未知」：它们对应的**用户动作完全不同** ——
// 没有账号要去启用/同步账号（这是所有者实际遇到的那种），有账号但拉取失败
// 只需稍后重试，账号都在冷却则要等冷却到期。混成一句「未知」，
// 用户既不知道该做什么，也不知道这是不是自己造成的。
// regionGap 描述「某区域为什么没有拿到真实模型清单」。
//
// # ⚠ 为什么要区分种类，而不是只给一句文案（2026-09-20 实测缺陷）
//
// 所有者的现场：界面提示「国服账号清单本次未拉到…**补齐国服账号后刷新即可确认**」，
// 而他**明明有 12 个国服账号**。他的反应是「我明明国内外账号都有,居然还有
// 这个提示 这是个bug」—— **他判断对了**。
//
// 根因是文案：`unverifiedNote` 无条件结尾写「补齐X账号」，而真实成因有两种，
// 行动建议**完全不同**：
//
//	KindMissingAccounts —— 该区一个可用账号都没有 ⇒ 补账号**是对的**
//	KindFetchFailed     —— 有账号但这次没拉到（上游不可达/网络波动）
//	                       ⇒ 补账号**毫无用处**，该"稍后点刷新重试"
//
// 把「补账号」的建议给到一个已经有账号的人，就是在误导他去做无用功。
//
// 故这里返回**种类 + 文案**，由 `unverifiedNote` 按种类给对应的行动建议。
// ⚠ 不要靠解析 reason 字符串里的关键词来判断种类 —— 文案改一个字就失效。
type regionGapCause int

const (
	// KindMissingAccounts 该区域没有任何可用账号（用户自己能修）。
	KindMissingAccounts regionGapCause = iota
	// KindFetchFailed 有账号，但本次没拉到清单（上游暂时不可达）。
	KindFetchFailed
)

// regionGapInfo 一并给出"成因"与"给人看的说明"。
type regionGapInfo struct {
	Cause  regionGapCause
	Reason string
}

// regionGapReason 判断「该区域为什么没有真值」。
//
// 为什么要分两种而不是统一说「未知」：它们对应的**用户动作完全不同** ——
// 没有账号要去启用/同步账号（这是所有者实际遇到过的另一种情形），
// 有账号但拉取失败只需稍后重试。混成一句「未知」，
// 用户既不知道该做什么，也不知道这是不是自己造成的。
func (h *Handler) regionGapReason(region auth.Region) regionGapInfo {
	if h.pickProbeAccountInRegion(region) == nil {
		// 该区域一个可用账号都没有。**必须与「拉取失败」区分开** ——
		// 这是唯一一种「用户自己能修好」的成因。
		return regionGapInfo{
			Cause:  KindMissingAccounts,
			Reason: "账号池里没有" + regionLabel(region.String()) + "的可用账号",
		}
	}
	// 有账号，但这次没拉到清单：上游暂时不可达（网络波动），或负缓存期内。
	return regionGapInfo{
		Cause:  KindFetchFailed,
		Reason: regionLabel(region.String()) + "账号清单本次未拉到（上游暂时不可达，点「刷新」可重试）",
	}
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
// productChannel 一个「模型来自哪个平台」的记录。
//
// 使用者的需求：
//
//	「智能体管理,哪里显示出来的模型,现在可以加一个渠道,是来自于哪个平台,
//	  如果重叠,就显示多个平台」
//
// ## 为什么需要新字段而不能复用 owned_by
//
// `owned_by` 在 OpenAI 惯例里是"归属组织"，我们此前只填了
// `workbuddy` / `workbuddy-intl` —— 那是**区域**，不是平台。
// 实测确认（uitest/diag-gateway-model-source.cjs）：30 个模型的
// owned_by 只有这两个取值，而 Qoder（Qwen3.8）与 ZCode（glm-*）
// 的模型**根本不在列表里**，因为 `mergedModelList` 只聚合了
// WorkBuddy 的两区动态清单 + 静态表。
//
// 故新增 `channels`：数组，元素是平台标识（`workbuddy` / `qoder` /
// `zcode`）。用数组而不是单值，正是为了表达"重叠"——
// 同一个模型名可能同时由多个平台提供（如 `glm-5.3` 既在 ZCode 套餐里、
// 也可能在 WorkBuddy 的模型清单里），此时要显示多个平台。
type productChannel struct {
	// Product 平台标识：workbuddy / qoder / zcode。
	Product string
	// Label 给人看的名字（界面直接显示这个）。
	Label string
	// Regions 该平台在哪些区域提供此模型（可能为空）。
	Regions []string
}

// channelLabels 平台标识 → 显示名。
//
// 与账号页用词保持一致（「WorkBuddy」「Qoder」「ZCode」），
// 避免同一样东西两个页面叫法不同。
var channelLabels = map[string]string{
	"workbuddy": "WorkBuddy",
	"qoder":     "Qoder",
	"zcode":     "ZCode",
	"trae":      "Trae",
	"doubao":    "豆包",
}

// mergedModelList 生成 /v1/models 的模型清单。
//
// ## 渠道（channels）怎么来的
//
// 每类平台各贡献一份"它提供哪些模型"：
//
//	WorkBuddy → 两区动态清单 + 静态表（既有逻辑）
//	Qoder     → 账号池里 Qoder 账号实际可用的模型（动态查）
//	ZCode     → 账号池里 ZCode 账号实际可用的模型（动态查）
//
// 同一个 id 被多个来源提供时，`channels` 里就会有多个元素 ——
// 这正是使用者要的"重叠就显示多个平台"。
//
// ## 为什么不在这里给每个平台发网络请求
//
// 那是**网关请求路径**上的函数，多一次外部调用就多一份延迟与失败面。
// 各产品的模型清单由**宿主**（账号页的刷新）预先取好并落盘，
// 这里只读缓存。见 `probeProducts`。
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

	// 其它产品（Qoder / ZCode）的模型也并入清单 ——
	// 否则用 Qoder 或 ZCode 账号时，客户端模型菜单里看不到它们。
	productModels := h.productModels()
	for _, pm := range productModels {
		addID(pm.ID)
	}

	// ---- 带前缀的**组合名**（所有者要求，2026-09-20）----
	//
	// 所有者原话：「我通过 models 接口 并没有返回 平台:国际:模型名、
	// 平台:模型名，这两个组合的模型名，只有单独的 模型名」。
	//
	// 为什么必须有：前缀是**用户指定平台/区域的唯一手段**。只给裸模型名时，
	// 用户想在 Qoder 上跑某个模型（或想在国服跑）就没法表达 ——
	// 而裸名字的解析规则是"哪个账号可用就用哪个"，会误路由
	//（实测：`deepseek-v4.1-flash` 被路由到 Qoder 账号并失败两次）。
	//
	// 故为**每个**模型补两种组合名：
	//
	//	平台:模型名            如 `qoder:qwen3.8-flash`
	//	平台:区域:模型名       如 `qoder:国际:qwen3.8-flash`
	//
	// ⚠ 只补**该平台确实提供**的组合（按 productModels 与区域能力判断），
	// 不无脑笛卡尔积 —— 否则菜单里会塞满用不了的名字，
	// 用户选中后得到"模型不存在"，比不显示更糟。
	//
	// ⚠ 组合名**不单独进 order**（那会让菜单长度翻三倍、淹没裸名字），
	// 而是作为 `aliases` 挂在裸名字那条目上 —— 客户端既能搜到，
	// 列表又不会膨胀。见下面组装处的 `aliases`。
	aliasesOf := map[string][]string{}
	addAlias := func(bare, alias string) {
		for _, have := range aliasesOf[bare] {
			if have == alias {
				return
			}
		}
		aliasesOf[bare] = append(aliasesOf[bare], alias)
	}
	// ① 非 WorkBuddy 产品：按 productModels 补「平台:模型名」与「平台:区域:模型名」
	for _, pm := range productModels {
		if pm.Product == "" {
			continue
		}
		bare := pm.ID
		addAlias(bare, pm.Product+":"+bare)
		// 区域维度：该产品账号覆及的每个区域各补一条。
		//
		// ⚠ 用**归一的显示名**（`国际版` / `国服`）而不是内部标识
		//（`intl` / `cn`）：用户在前缀里写的是前者，写后者解析不出。
		for _, r := range h.productRegions(pm.Product) {
			if label := realmLabelOf(r); label != "" {
				addAlias(bare, pm.Product+":"+label+":"+bare)
			}
		}
	}
	// ② WorkBuddy 的模型：补「workbuddy:模型名」与区域组合，
	//    以及**纯区域**前缀（`国际版:模型名`）—— 那是老客户端就在用的写法。
	for _, entries := range [][]map[string]any{staticModels, staticModelsIntl} {
		for _, m := range entries {
			id, _ := m["id"].(string)
			if id == "" {
				continue
			}
			addAlias(id, productWorkBuddy+":"+id)
			addAlias(id, productWorkBuddy+":"+realmIntlCN+":"+id)
			addAlias(id, realmIntlCN+":"+id)
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

	// 组装：id → 提供它的平台集合
	channels := map[string][]productChannel{}
	// WorkBuddy：两区动态清单 + 静态表都算它提供的
	for _, mi := range cnInfos {
		channels[mi.ID] = appendChannel(channels[mi.ID], productChannel{Product: "workbuddy", Label: channelLabels["workbuddy"], Regions: []string{"cn"}})
	}
	for _, mi := range intlInfos {
		channels[mi.ID] = appendChannel(channels[mi.ID], productChannel{Product: "workbuddy", Label: channelLabels["workbuddy"], Regions: []string{"intl"}})
	}
	for _, entries := range [][]map[string]any{staticModels, staticModelsIntl} {
		for _, m := range entries {
			id, _ := m["id"].(string)
			if id == "" {
				continue
			}
			channels[id] = appendChannel(channels[id], productChannel{Product: "workbuddy", Label: channelLabels["workbuddy"]})
		}
	}
	// Qoder / ZCode：来自账号池的缓存清单
	for _, pm := range productModels {
		label := channelLabels[pm.Product]
		if label == "" {
			label = pm.Product
		}
		channels[pm.ID] = appendChannel(channels[pm.ID], productChannel{Product: pm.Product, Label: label})
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

		// 渠道字段（使用者要求的「来自哪个平台」）
		if ch := channels[id]; len(ch) > 0 {
			e["channels"] = channelsToJSON(ch)
		}
		// 带前缀的组合名（所有者要求的 `平台:模型名` / `平台:区域:模型名`）。
		//
		// 为什么挂在 aliases 而不是各自作为独立条目：
		// 独立条目会让菜单长度翻两三倍、把裸名字淹没；而客户端真正需要的是
		// **能搜到/能选用**这些名字。挂在裸名字上两者兼得。
		if al := aliasesOf[id]; len(al) > 0 {
			e["aliases"] = al
		}
		out = append(out, e)
	}
	return out
}

// appendChannel 加一个渠道，去重（同平台只留一条，区域合并）。
// 去重键是 **Product**：同一平台的多个区域算一条渠道、区域合并显示。
// 若不去重，WorkBuddy 的国服+国际版会让每个模型都出现两条 "WorkBuddy"，
// 界面上看起来像重复而不是"两个平台"。
func appendChannel(list []productChannel, c productChannel) []productChannel {
	for i := range list {
		if list[i].Product == c.Product {
			for _, r := range c.Regions {
				dup := false
				for _, have := range list[i].Regions {
					if have == r {
						dup = true
						break
					}
				}
				if !dup {
					list[i].Regions = append(list[i].Regions, r)
				}
			}
			return list
		}
	}
	return append(list, c)
}

// channelsToJSON 把渠道列表转成下发形状。
//
// 同时给 `product`（稳定标识，程序用）与 `label`（显示名，界面用）：
// 只给标识会让界面自己去映射，只给显示名则界面没法按平台过滤。
func channelsToJSON(chs []productChannel) []map[string]any {
	out := make([]map[string]any, 0, len(chs))
	for _, c := range chs {
		e := map[string]any{"product": c.Product, "label": c.Label}
		if len(c.Regions) > 0 {
			e["regions"] = c.Regions
		}
		out = append(out, e)
	}
	return out
}

// productModel 一个「非 WorkBuddy 平台提供某模型」的记录。
type productModel struct {
	ID      string
	Product string
}

// realmLabelOf 把 `auth.Region` 映射成**用户在前缀里会写**的区域名。
//
// 为什么要这一步：内部标识是 `cn` / `intl`，而所有者习惯写「国服」/「国际版」
//（`resolve_model.go` 里 `realmIntlCN = "国际版"` 就是为此加的别名）。
// 模型清单里给出的组合名必须与用户会输入的写法一致，否则"给了名字但用不了"。
//
// RegionAny 返回空串：它表示"不限区域"，不是一个可写进前缀的区域名。
func realmLabelOf(r auth.Region) string {
	switch r {
	case auth.RegionCN:
		return "国服"
	case auth.RegionIntl:
		return realmIntlCN
	default:
		return ""
	}
}

// productRegions 报告某产品的账号覆及哪些区域（用于生成区域组合名）。
//
// 为什么按**产品账号实际所在区域**而不是"所有区域"：
// 无脑给所有区域会造出"qoder:国服:xxx"这类**并没有账号可用**的名字，
// 用户选中后得到"账号不可用"，比不显示更糟。
//
// 实现委托给 `pool.ProductRegions` —— 那里能同时拿到账号与其 Region。
func (h *Handler) productRegions(product string) []auth.Region {
	if h == nil || h.cfg.Pool == nil {
		return nil
	}
	return h.cfg.Pool.ProductRegions(product)
}

// productModels 从配置里读出 Qoder / ZCode 提供的模型。
//
// ## 为什么从配置读，而不是网关自己去查
//
// `/v1/models` 是**请求路径**上的接口 —— 每个客户端列一次模型就打一次
// 上游会带来延迟与失败面。而"某产品的账号能用哪些模型"只有宿主知道：
// 它持有账号库，在刷新账号时已查过并缓存（见 refresh_account）。
//
// 故宿主把汇总清单放进 `pool.product_models` 透传过来，网关只读 ——
// 与 `account_records` 的透传方式一致。
//
// 配置缺失时返回空：模型清单退化成只有 WorkBuddy 的，**不报错** ——
// 缺一份可选配置不该让整个 /v1/models 失败。
func (h *Handler) productModels() []productModel {
	var out []productModel
	// 顺序稳定：按产品名排序，让输出的渠道顺序可复现
	products := make([]string, 0, len(h.cfg.ProductModels))
	for p := range h.cfg.ProductModels {
		products = append(products, p)
	}
	sort.Strings(products)
	for _, p := range products {
		for _, id := range h.cfg.ProductModels[p] {
			id = strings.TrimSpace(id)
			if id != "" {
				out = append(out, productModel{ID: id, Product: p})
			}
		}
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
//
// 区域说明**分两种**（见 regionFields）：确知另一区没有（region_note），
// 与另一区没有真值、无从确认（unverified_regions + unverified_note）。
// 这两件事的文案绝不可混用 —— 前者是断言，后者只能说「没检查过」。
func (idx *capabilityIndex) capabilityFieldsFor(id string) map[string]any {
	cn, cnKnown := idx.lookupRegion(id, auth.RegionCN)
	intl, intlKnown := idx.lookupRegion(id, auth.RegionIntl)

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
	// 思考档与图片能力**正交**：图片能力未声明（nil）时仍可能有思考档，
	// 因此不能因为 `fields == nil` 就跳过它，否则「未声明图片能力的模型
	// 一律看不到档位」—— 而这两件事在上游是各自独立的字段。
	reasoning := modelReasoningFields(cap.Efforts, cap.DefaultEffort)
	if fields == nil && reasoning == nil {
		return nil
	}
	if fields == nil {
		fields = map[string]any{}
	}
	for k, v := range reasoning {
		fields[k] = v
	}
	// canDisableThinking 只在**确知**时下发（nil = 上游未声明，不能编造）。
	// 它决定客户端是否允许选「关闭思考」—— 为 false 时传 off 会被上游拒绝。
	if cap.CanDisableThinking != nil {
		fields["can_disable_thinking"] = *cap.CanDisableThinking
		fields["canDisableThinking"] = *cap.CanDisableThinking
	}
	for k, v := range idx.regionFields(id) {
		fields[k] = v
	}
	return fields
}

// regionFields 生成「区域维度」的字段：supported_regions / region_note /
// unverified_regions / unverified_note。
//
// 三种情形**必须分开表达**（这正是所有者反馈的那个缺陷）：
//
//	A. 两区都有真值且都列出它 → 不下发任何区域字段（无需说明）
//	B. 一区有真值并列出它，另一区**有真值但清单里没有** → supported_regions +
//	   regionNote：「仅某区存在」。此时 11102 的成因是**模型确实不在那一侧**，
//	   用户可以据此换模型 —— 现状是对的，保留。
//	C. 一区有真值并列出它，另一区**压根没有真值**（无可用账号 / 拉取失败）→
//	   **不能说「仅某区存在」**。改发 unverified_regions + unverifiedNote：
//	   「未检测到另一区账号，无法确认该区是否有此模型」。
//	   两个字段都发是刻意的：supported_regions 保留「目前只见于这一侧」这个
//	   已知事实（信息不为清爽而丢），unverified_regions 明确标出它是**未验证**的，
//	   前端据此用不同措辞与不同徽标（不能与 B 长得一样）。
func (idx *capabilityIndex) regionFields(id string) map[string]any {
	cn, cnKnown := idx.lookupRegion(id, auth.RegionCN)
	intl, intlKnown := idx.lookupRegion(id, auth.RegionIntl)

	// 有真值、且确实列出该模型的区域（静态表不算，见 FromStatic）。
	var supported []string
	if cnKnown && cn.Present && !cn.FromStatic {
		supported = append(supported, regionCodeCN)
	}
	if intlKnown && intl.Present && !intl.FromStatic {
		supported = append(supported, regionCodeIntl)
	}
	// 只关心「只见于单一区域」的情形；两区都有或都没有都不附区域说明。
	if len(supported) != 1 {
		return nil
	}

	// 另一侧是否真的被排除过。knownRegion[region] 表示该区清单来自真实拉取。
	other := regionCodeCN
	if supported[0] == regionCodeCN {
		other = regionCodeIntl
	}
	otherRegion := regionByCode(other)
	if !idx.knownRegion[otherRegion] {
		// C 情形：另一侧没有真值 —— 结论只能是「没检查过」，不是「不存在」。
		gap, ok := idx.gapReason[otherRegion]
		if !ok || gap.Reason == "" {
			// 兜底：理论上不该走到（gapReason 在 index 构建时已填）。
			// 按「拉取失败」处理而不是「没账号」—— 后者会给出"补账号"的
			// 错误建议（那正是 2026-09-20 那个缺陷的形态）。
			gap = regionGapInfo{
				Cause:  KindFetchFailed,
				Reason: regionLabel(other) + "的模型清单本次未拉到",
			}
		}
		return map[string]any{
			"supported_regions": []string{supported[0]},
			"unverified_regions": []string{other},
			"region_note":       unverifiedNote(supported[0], other, gap),
		}
	}
	// B 情形：另一侧有真值且清单里确实没有它 —— 可以断言。
	return map[string]any{
		"supported_regions": []string{supported[0]},
		"region_note":       regionNote(supported[0]),
	}
}

// regionNote 生成「已确认另一区没有该模型」的说明。
//
// 措辞里的「仅在X上游存在」与 11102 都只在**确知另一区没有**时成立，
// 故本函数只由 regionFields 的 B 情形调用。未验证的情形走 unverifiedNote。
func regionNote(region string) string {
	other := regionLabel(otherRegionCode(region))
	return "该模型名仅在" + regionLabel(region) + "上游存在；账号池里的" +
		other + "账号调用它会返回 11102 model service info not found。"
}

// unverifiedNote 生成「另一区没有真值，故无法确认」的说明。
//
// # ⚠ 行动建议必须**按成因**给（2026-09-20 实测缺陷）
//
// 所有者的原话：
//
//	「还有这个,我明明国内外账号都有,居然还有这个提示 这是个bug」
//
// 他判断对了。旧实现**无条件**结尾写「补齐X账号后刷新即可确认」，
// 而他的现场是 `KindFetchFailed`（有 12 个国服账号，只是那一轮上游没拉到）——
// 于是提示让他去"补账号"，而他账号早就够了。**建议指向了不存在的问题**。
//
// 两种成因给的建议完全不同，故必须分开：
//
//	KindMissingAccounts → 补账号（这是唯一"用户自己能修好"的情形）
//	KindFetchFailed     → 稍后刷新重试（补账号毫无用处）
//
// ⚠ 判据用**枚举**而不是解析 reason 字符串：文案改一个字就失效。
func unverifiedNote(region, other string, gap regionGapInfo) string {
	head := "已确认" + regionLabel(region) + "上游有此模型；但" +
		regionLabel(other) + "未检测到可用真值（" + gap.Reason + "），" +
		"因此**无法确认**" + regionLabel(other) + "是否也有它 —— " +
		"这不等于该模型" + regionLabel(other) + "没有。"

	switch gap.Cause {
	case KindMissingAccounts:
		// 真的没有该区账号 —— 补账号是有效的，如实说。
		return head + "补齐" + regionLabel(other) + "账号后，点本清单右上角的「刷新」即可确认。"
	default:
		// 有账号、只是这轮没拉到。**不要说"补账号"** ——
		// 那会让已有账号的用户去做无用功（本次缺陷）。
		//
		// 也不说"无法确认"就结束：用户需要知道**该做什么**（等一会儿重试），
		// 以及**这不是他的问题**（账号是够的）。
		//
		// ⚠ 必须写清按钮**在哪**（所有者 2026-09-20：
		// 「我点哪里的刷新啊?兼容网关这里还是有这个描述」）。
		// 只说"点刷新"，而页面上有好几个刷新按钮、且当时这个按钮
		// 只存在于「放行模型」下拉里 —— 那句话等于没说。
		return head + "你的" + regionLabel(other) +
			"账号是够的，这是上游暂时不可达；点本清单右上角的「刷新」即可重试。"
	}
}

// otherRegionCode 返回另一个区域的码。
func otherRegionCode(region string) string {
	if region == regionCodeIntl {
		return regionCodeCN
	}
	return regionCodeIntl
}

// regionLabel 把**区域码**翻译成中文名（"intl" → 国际版，其余 → 国服）。
//
// 入参是码而不是 auth.Region：调用点既有码也有 Region，统一收码可以少一层
// 转换（auth.Region.String() 产出的正是同一套码）。
func regionLabel(r string) string {
	if r == regionCodeIntl {
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

// modelReasoningFields 生成思考等级（reasoning effort）字段。
//
// 与 modelCapabilityFields 是**正交**的两个维度（一个答「能不能收图」，
// 一个答「思考用哪档」），因此独立成函数、独立调用，不合并成一个 map。
//
// efforts 为空 = 上游未声明（含固定档模型、静态兜底表）→ 返回 nil，不下发任何键。
// 这与图片能力的三态语义一致：**宁可不写，也不要凭空编造档位** —— 客户端会拿着
// 编造的档位去发请求，而该档位要么被上游降级、要么被忽略，用户看到的是
// 「我明明调了 max 却没生效」这类无从排查的现象。
//
// 一次下发**多种拼写**的理由与图片能力相同：各客户端读的字段名不统一，且没有
// 统一约定。所有已知解析器都只取自己认识的键，多余键不会报错。
//
//	OpenAI 风格     → supported_efforts / reasoning_efforts
//	OpenRouter 风格 → reasoning.supported_efforts / reasoning.default_effort
//	通用容错        → supportedEfforts / reasoningEfforts / defaultEffort
//
// 默认档只在 defaultEffort 非空时下发。**不要**用 efforts[0] 之类的猜测填充：
// 上游没声明默认档时，网关也不知道，编一个反而误导。
// modelReasoningFields 生成某模型的思考档位字段。
//
// **三种情形必须分开表达**（2026-09-18 实测修正，所有者报的就是这个缺陷）：
//
//	A. supportedEfforts 非空        → 列出可指定档位（+ 默认档）
//	B. supportedEfforts 为空，但有默认档 → **支持思考、但只有固定一档**，
//	   下发 reasoning_fixed=true / default_effort / supports_reasoning=true
//	C. 两者都无且 supportsReasoning 未声明 → 才是真的「不支持思考」，不下发
//
// 早先这里只判 `len(efforts)==0 → return nil`，把 B 与 C 混成一种，于是
// 18 个（国服）/ 8 个（国际版）模型在界面上显示成「—」，用户以为它们不能思考。
//
// ⚠ **2026-09-18 二次修正（所有者报「我现在就用的这个模型，用的 max 档位，
// 为什么没有拦截报错？」）**：
//
// 上一版把这种情况标成「固定单档」（reasoning_fixed=true）—— 那是**错的**。
// 实测 deepseek-v4.1-flash（未声明 supportedEfforts 的典型）：
//
//	reasoning_effort=off         → ✗ HTTP 400 the reasoning effort value is not supported
//	reasoning_effort=bogus_value → ✗ HTTP 400 同上
//	minimal/low/medium/high/max/xhigh → ✓ **全部接受**
//
//	单调性（各档 2 次，推理字符均值）：
//	    low 397 → medium 420 → high 646 → max 776   ✓ 单调递增
//
// 随机性不会产生单调序列，所以**档位真的生效** —— 用户调 max 确实得到了更长的推理。
// 上游拒绝 off / 未知值，说明它有一整套**标准档位阶梯**在校验，
// 只是没在 /v3/config 里逐模型列出 supportedEfforts。
//
// 结论：网关的职责是**如实区分两种情况**，而不是替上游「猜」它的能力：
//
//	A. 声明了 supportedEfforts → 列出该模型自己的档位（可能是子集，如 ["low","high"]）
//	B. 未声明但有默认档     → **未声明可选范围**：默认档 X，但档位可指定，
//	                          标准阶梯全部可用（网关不校验、原样透传）
//	C. 两者都无             → 上游没声明任何思考信息，不下发
//
// 为什么 B 不能标成「固定」：那个词会让用户以为「调了也没用」而放弃调档 ——
// 而实测证明调了有用。谎报能力的方向与「谎报支持图片」同样有害。
func modelReasoningFields(efforts []string, defaultEffort string) map[string]any {
	defaultEffort = strings.TrimSpace(defaultEffort)

	// 情形 C：既没有档位数组，也没有默认档 → 上游没声明任何思考信息。
	// 不下发字段（宁可不写，也不要谎报成「不支持」）。
	if len(efforts) == 0 && defaultEffort == "" {
		return nil
	}

	// 情形 A：上游**声明了**该模型的可选档位 → 如实列出它自己的范围，不做任何推断。
	//
	// ⚠ 注意「声明了」**不等于**「只有这些可用」：实测 hy3 声明 [low,high]
	// 却接受 medium/max/minimal/off。所以这组值只是「上游愿意列出来的」，
	// 不是硬范围 —— 这也是我们**不能**据此拦截请求的原因（见 forward.go）。
	if len(efforts) > 0 {
		return withEffortList(efforts, defaultEffort, false)
	}

	// 情形 B：未声明可选范围，但有默认档。
	//
	// 下发「范围未声明」标记 + **候选档位**，让客户端能给出可选项。
	//
	// ⚠ 这组值是**候选**而非承诺：逐模型实测（16 个国服模型 × 7 档）证明
	// 档位可用性按模型而定 —— off 在 14/16 个模型可用，却被两个 deepseek
	// 模型拒绝。因此字段名与文案都不能暗示「上游保证支持这些」。
	//
	// 用候选列表而非空列表：空列表会让客户端渲染出一个没有选项的档位控件，
	// 用户只能看到「固定」而无法尝试任何档位（那正是我上一版的错误）。
	return withEffortList(upstream.StandardEfforts(), defaultEffort, true)
}

// withEffortList 组装档位字段的公共形状（情形 A 与 B 共用）。
//
// undeclaredRange 为 true 时额外下发「可选范围未声明」标记 —— 语义是
// **上游没列出可选值**，而非「不可选」。两者对用户的含义相反：
// 前者鼓励尝试（实测调档真的生效），后者劝退。
func withEffortList(list []string, defaultEffort string, undeclaredRange bool) map[string]any {
	// 复制一份：list 可能是共享切片（情形 A 来自按区域的模型缓存）。
	// 直接塞进响应 map 会让调用方对返回值的任何 in-place 修改污染缓存 ——
	// 下次请求就会带着被改过的档位列表。
	cp := make([]string, len(list))
	copy(cp, list)

	nested := map[string]any{"supported_efforts": cp}
	out := map[string]any{
		// 主拼写：OpenAI / 多数客户端。
		"supported_efforts": cp,
		// 容错拼写。
		"supportedEfforts":  cp,
		"reasoning_efforts": cp,
		"reasoningEfforts":  cp,
		// OpenRouter 风格：嵌套在 reasoning 对象下。
		"reasoning": nested,
		// 有可选档位 ⇒ 支持思考，且**不是**固定档。
		"supports_reasoning": true,
		"supportsReasoning":  true,
		"reasoning_fixed":    false,
		"reasoningFixed":     false,
	}
	if undeclaredRange {
		nested["range_undeclared"] = true
		out["reasoning_range_undeclared"] = true
		out["reasoningRangeUndeclared"] = true
		// 逐个标注该档位**有没有上游清单背书**：
		// 上游 supportedEfforts 里出现过的是「声明过的」（low/medium/high/xhigh/max），
		// off 与 minimal 从未出现、只有我们的实测结果 —— 客户端可据此措辞更保守。
		backed := make(map[string]bool, len(cp))
		for _, e := range cp {
			backed[e] = upstream.UpstreamDeclaredEffort(e)
		}
		nested["declared_by_upstream"] = backed
		out["upstream_declared_efforts"] = backed
	}
	if defaultEffort != "" {
		nested["default_effort"] = defaultEffort
		out["default_effort"] = defaultEffort
		out["defaultEffort"] = defaultEffort
		out["default_reasoning_effort"] = defaultEffort
	}
	return out
}
