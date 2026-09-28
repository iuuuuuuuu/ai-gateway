package qoder

// dispatch.go 实现 server.QoderUpstream 接口（把 qoder 包接进网关的派发层）。
//
// ## 为什么要一个适配器而不是让 Client 直接实现接口
//
// server 的接口是**按 server 的需要**定义的（窄：发请求 / 聚合），
// 而 Client 的方法签名是为 qoder 自己的使用场景设计的。
// 直接让 Client 去满足 server 的接口会把两边的关注点绑死：
// 将来 server 想多要一个能力，就得改 Client 的公开 API。
//
// 适配器把"接口适配"这件事收在一个文件里，两边都能独立演进。
//
// ## 模型名映射的职责
//
// 客户端发来的是**它看到的模型名**（如 qwen3-max），而上游要的是**模型 key**
// （如 qmodel_preview）。映射表来自上游的模型列表接口，缓存在这里。
//
// 映射不到时的处理分两种，判据是**我们手里有没有权威清单**：
//
//	清单拉取成功 → 上游**确实没有**这个模型 ⇒ 拒绝（ModelNotFoundError）
//	清单拉取失败 → 我们**不知道**上游有没有 ⇒ 原样透传，交给上游决定
//
// # 为什么"查不到就原样发过去"是错的（2026-09-28 修正）
//
// 旧注释写着「给上游一个机会，它若真不认识会回明确错误」—— 实测**这个前提
// 不成立**：Qoder 上游对**不认识的模型 key 不报错**，而是静默回退到免费通道、
// HTTP 200 正常返回内容。伪造的 `zzz-not-a-real-model-xyz` 与真实模型得到
// **逐项相同**的响应画像（最终 usage 帧 billable=false、credits=0）。
//
// 后果（所有者现场）：他配的 `qoder:deepseek-v4.1-flash` 在 Qoder 两个区域的
// 清单里都不存在，却被"原样回退"放行 ⇒ 用量统计里出现
// 「deepseek-v4.1-flash · Qoder」的**幽灵数据**，而那个模型根本不属于它。
//
// ⚠ 判据必须是**模型清单**，绝不能用计费字段（billable / credits）：
//
//	· 会误杀合法免费模型：实测 `qfmodel`(Qwen3.8-Flash) 是正常可用的免费
//	  模型，回的却是 billable=false、credits 极小 —— 用计费判据会把
//	  **正常可用的模型**判成"不存在"；
//	· 时序上做不到：billable 只在**最后一个 usage 帧**出现，流式请求走到
//	  那里时正文早已发给客户端，无法撤回（我们要的是**发请求之前**判定）。
//
// 清单本来就要拉来做名字映射，且已有 10 分钟缓存 ⇒ 零额外成本。

import (
	"context"
	"fmt"
	"io"
	"log"
	"sort"
	"strings"
	"sync"
	"time"

	"workbuddy2api/internal/auth"
)

// Dispatch 实现 server.QoderUpstream。
type Dispatch struct {
	client *Client

	// modelKeyOf 客户端模型名 → 上游模型 key（大小写不敏感）。
	//
	// 缓存策略：按账号区域分别缓存（两区的模型清单不同），
	// TTL 10 分钟 —— 上游加模型是低频事件，而每次请求都拉清单
	// 会白白消耗额度并增加延迟。
	mu       sync.RWMutex
	modelMap map[Region]modelCache
}

type modelCache struct {
	// models **完整清单**，供"未命中即拒绝"的判定与文案展示。
	//
	// ⚠ 这里**刻意不存**"客户端名 → key"的映射表：快速路径若直接查映射表
	// 命中就 return，同一个假模型会**第一次被拦、之后 10 分钟内全部放行**
	// （缓存命中跳过校验），且 Available 也拿不到 —— 对用户就是
	// "有时拦有时不拦"，且极难在单测里发现。两条路径共用 resolveInList，
	// 判定必然一致；需要名字映射时用 ResolveModelMap 现算（探针命令在用）。
	models  []DynamicModel
	fetched time.Time
}

// modelCacheTTL 模型清单的缓存时长。
//
// 10 分钟：上游加/改模型是低频事件；而缓存太久会让新模型在一段时间内
// 不可用（用户看到"模型不存在"）。这个折中与网关其它缓存口径一致。
const modelCacheTTL = 10 * time.Minute

// NewDispatch 构造派发适配器。
func NewDispatch(c *Client) *Dispatch {
	if c == nil {
		c = New()
	}
	return &Dispatch{client: c, modelMap: map[Region]modelCache{}}
}

// ChatStream 实现 server.QoderUpstream。
//
// 流程：解析客户端模型名 → 映射成上游 key → 构造 Qoder 请求体
//
//	→ 编码 + 签名 → 发送 → 把嵌套 SSE 惰性翻译成 OpenAI 帧。
func (d *Dispatch) ChatStream(ctx context.Context, a *auth.Auth, openAIBody []byte) (io.ReadCloser, int, []byte, error) {
	cr := credOf(a)
	if cr == nil {
		return nil, 0, nil, fmt.Errorf("账号缺少 Qoder 凭证（Product=qoder 但凭证为空）")
	}

	// 客户端模型名 → 上游 key（同时判定该模型在上游是否**真的存在**）
	clientModel := modelNameOf(openAIBody)
	modelKey, models, ok := d.resolveModelKey(ctx, cr, clientModel)
	if !ok {
		// ⚠ 在**发请求之前**拒绝：上游对不认识的 key 不报错，而是静默回退到
		// 免费通道并返回 200 —— 放过去就会产生"幽灵用量"（见 ModelNotFoundError
		// 的注释）。绝不降级为"原样透传"。
		err := &ModelNotFoundError{
			Model:     clientModel,
			Region:    cr.Region,
			Available: availableKeys(models),
		}
		log.Printf("qoder dispatch uid=%s region=%s: 上游没有模型 %q（清单 %d 个），已拒绝（不发请求）",
			a.UID, cr.Region, clientModel, len(models))
		return nil, 0, nil, err
	}

	// OpenAI 请求体 → Qoder 请求体
	agentBody, err := BuildAgentBody(openAIBody, modelKey)
	if err != nil {
		return nil, 0, nil, fmt.Errorf("构造 Qoder 请求体失败: %w", err)
	}

	// 令牌临近过期先刷新（与 WorkBuddy 路径的口径一致）。
	if cr.NeedsRefresh(refreshSkew) && cr.DRT != "" {
		if err := cr.RefreshToken(ctx, d.client.http()); err != nil {
			// 刷新失败不直接放弃：可能是网络抖动，用现有令牌试一次。
			// 真过期了上游会回 105，那时再报错也不迟。
			log.Printf("qoder dispatch uid=%s: 刷新令牌失败，沿用现有令牌: %v", a.UID, err)
		}
	}

	body, status, respBody, err := d.client.ChatStream(ctx, cr, agentBody)
	if err != nil {
		return nil, 0, nil, err
	}
	if body == nil {
		// 上游拒绝（status >= 400）：把原始响应体交给调用方分类处理。
		return nil, status, respBody, nil
	}

	// 把嵌套 SSE 翻译成标准 OpenAI SSE。
	// 调用方（server）只看到 OpenAI 形状，与 WorkBuddy 路径逐字一致。
	return NewOpenAIStream(body, clientModel), status, nil, nil
}

// Aggregate 实现 server.QoderUpstream（非流式聚合）。
func (d *Dispatch) Aggregate(r io.Reader, model string) (map[string]any, error) {
	return AggregateQoder(r, model)
}

// ModelNotFoundError 客户端请求的模型**不在上游清单里**。
//
// # 为什么它是一个独立类型（而不是普通 error）
//
// 调用方（server 层）必须能把它与"账号故障"区分开，因为处置**完全相反**：
//
//	账号故障     → 换号重试、冷却账号（值得做的事）
//	本错误       → **立即返回 400**，绝不换号、绝不罚账号
//
// 这是**请求侧**错误：同一个请求体发给任何账号，上游清单都一样没有它。
// 若不单独成型，它会被包成 503「账号全部不可用」——用户去查账号池，
// 而账号一个都没问题；同时那次请求还会被对着池里每个账号重传一遍。
//
// # 判据为什么是"清单"而不是计费字段
//
// 上游对不认识的 key **不报错**：静默回退免费通道并返回 200
// （实测伪造 key 与真实模型的响应画像逐项相同）。故只能靠清单判定。
//
// 明确**不用** billable / credits 当判据，两条理由都是实测的：
//
//  1. **会误杀合法免费模型**：`qfmodel`(Qwen3.8-Flash) 是正常可用的免费模型，
//     回的却是 billable=false、credits 极小 —— 用它当"不存在"的判据会把
//     **正常可用的模型**判死。
//  2. **时序上做不到**：billable 只在**最后一个 usage 帧**出现；流式请求走到
//     那里时正文早已发给客户端，无法撤回。而本错误的价值正在于
//     **发请求之前**就判定。
type ModelNotFoundError struct {
	// Model 客户端请求的模型名（原始形态，未归一化）。
	Model string
	// Region 判定所用的区域 —— 两区清单不同，说清是哪个区下的结论。
	Region Region
	// Available 上游该区域可用的模型 key（已排序，供文案展示）。
	Available []string
}

func (e *ModelNotFoundError) Error() string {
	if e == nil {
		return "模型不存在"
	}
	return fmt.Sprintf("上游 Qoder（%s）没有模型 %s", e.Region.Label(), e.Model)
}

// availableKeys 取出清单里的模型 key（去重 + 排序，供稳定文案展示）。
//
// 排序是刻意的：错误文案会进日志与用户界面，map 遍历顺序会让同一条错误
// 每次显示不同的清单顺序，看起来像"清单在变"。
func availableKeys(models []DynamicModel) []string {
	seen := make(map[string]bool, len(models))
	out := make([]string, 0, len(models))
	for _, m := range models {
		if m.Key == "" || seen[m.Key] {
			continue
		}
		seen[m.Key] = true
		out = append(out, m.Key)
	}
	sort.Strings(out)
	return out
}

// resolveInList 在**已知清单**里解析客户端模型名 —— 纯函数，便于单测。
//
// 返回 (上游 key, 是否可用)：
//
//	ok=true  → 可以发请求（key 可能是原样名字，见下面"清单为空"）
//	ok=false → 清单**成功拉取**且上游确实没有这个模型 ⇒ 调用方必须拒绝，
//	           不得发请求（见 ModelNotFoundError）
//
// # 为什么 ok=false 的条件里必须有 len(models) > 0
//
// 拿不到权威信息时**不许拦**。清单拉取失败（网络抖动 / 令牌过期 / 上游
// 改协议）返回的是空清单，那说明"我们不知道上游有什么"，而不是
// "上游什么都没有"。此时原样透传，把判断权交回上游 —— 拦下去会让一次
// 短暂的拉取失败变成"所有模型都不可用"，故障面被我们自己放大。
func resolveInList(models []DynamicModel, clientModel string) (string, bool) {
	// 客户端没指定模型：用上游第一个可用模型（空 key 会被上游拒）。
	if clientModel == "" {
		if len(models) > 0 {
			return models[0].Key, true
		}
		return "", true
	}

	// 已经是上游 key 的形态（qmodel_ 前缀）→ 直接用。
	//
	// ⚠ 这是**文档化的逃生口**，刻意保留且**不参与清单校验**：上游随时可能
	// 上线我们清单里还没有的 key，而清单有 10 分钟缓存 —— 没有这个口子，
	// 新模型在缓存过期前完全不可用。用户明确写 `qmodel*` 即表示
	// "我知道自己在写上游 key"。
	if strings.HasPrefix(clientModel, "qmodel") {
		return clientModel, true
	}

	// ⚠ 两侧都要归一化，只 lower 客户端那侧是不够的（实测抓到）。
	//
	// 这里原来写的是 `lower := strings.ToLower(clientModel)`，然后与
	// `strings.ToLower(NormalizeModelName(m.DisplayName))` 比 ——
	// 上游那侧归一化了、客户端这侧只做了小写 ⇒ 用户写
	// `DeepSeek_Flash` / `deepseek flash` / 两边带空格 的变体**全部匹配不上**，
	// 于是被判成"上游没有这个模型"而拒绝一个**本来可用**的模型。
	//
	// 故客户端名也过 NormalizeModelName（它本就包含 TrimSpace + 小写 +
	// 空格/下划线/连字符统一），两侧同一把尺子。
	lower := strings.ToLower(strings.TrimSpace(clientModel))
	norm := NormalizeModelName(clientModel)
	for _, m := range models {
		if strings.ToLower(m.Key) == lower {
			return m.Key, true
		}
		if m.DisplayName != "" && NormalizeModelName(m.DisplayName) == norm {
			return m.Key, true
		}
	}

	if len(models) == 0 {
		// 清单不可用 ⇒ 无法判定 ⇒ 原样透传（见上面的说明）。
		return clientModel, true
	}
	// 清单可用且查不到 ⇒ 上游确实没有这个模型。
	return "", false
}

// resolveModelKey 把客户端模型名解析成上游 key，并判定该模型是否存在于上游。
//
// 返回 (key, models, ok)：
//
//	ok=true  → 可以发请求
//	ok=false → 上游确实没有该模型，**调用方必须直接返回 ModelNotFoundError**，
//	           不得调用上游
//
// models 一并返回是刻意的：错误文案要列出"上游实际有什么"，
// 而 resolveInList 已经拿到了那份清单。让调用方再查一次缓存会引入
// "判定用的清单"与"文案用的清单"可能不一致的窗口。
func (d *Dispatch) resolveModelKey(ctx context.Context, cr *Cred, clientModel string) (string, []DynamicModel, bool) {
	// 空模型名：用清单第一个（走 cachedModels，缓存命中不重复拉取）。
	if clientModel == "" {
		models := d.cachedModels(ctx, cr)
		key, ok := resolveInList(models, clientModel)
		return key, models, ok
	}

	// ⚠ 快速路径（缓存命中）也**必须**过校验。
	//
	// 这里曾经是直接查映射表 `cache.m` 命中就 return —— 若只在校验放在
	// "缓存未命中 → 刷新" 那条路径上，同一个假模型会**第一次被拦、
	// 之后 10 分钟内全部放行**（命中快速路径 ⇒ 跳过校验），
	// 对用户就是"有时拦有时不拦"。故两条路径共用 resolveInList。
	d.mu.RLock()
	cache, ok := d.modelMap[cr.Region]
	d.mu.RUnlock()
	if ok && time.Since(cache.fetched) < modelCacheTTL && len(cache.models) > 0 {
		key, allowed := resolveInList(cache.models, clientModel)
		return key, cache.models, allowed
	}

	// 缓存未命中 / 已过期：刷新一次再判定。
	models := d.refreshModels(ctx, cr)
	key, allowed := resolveInList(models, clientModel)
	return key, models, allowed
}

// cachedModels 返回缓存的模型清单（过期或缺失时刷新）。
func (d *Dispatch) cachedModels(ctx context.Context, cr *Cred) []DynamicModel {
	d.mu.RLock()
	cache, ok := d.modelMap[cr.Region]
	d.mu.RUnlock()
	if ok && time.Since(cache.fetched) < modelCacheTTL {
		// 缓存里存着**完整清单**（modelCache.models），可以直接用 ——
		// 不再是"只有映射表"（那是加 models 字段之前的情况）。
		return cache.models
	}
	return d.refreshModels(ctx, cr)
}

// refreshModels 拉取并缓存模型清单。
//
// 失败时返回 nil —— 此时调用方**原样透传**（不能因为"拉清单失败"就拦请求：
// 拿不到权威信息时不许判定"上游没有这个模型"，否则一次网络抖动会让
// 所有模型都不可用）。见 resolveInList 里 `len(models) == 0` 那条分支。
func (d *Dispatch) refreshModels(ctx context.Context, cr *Cred) []DynamicModel {
	models, err := d.client.FetchModels(ctx, cr)
	if err != nil {
		log.Printf("qoder dispatch uid=%s: 拉取模型清单失败（本次不校验模型名，原样透传）: %v", cr.UID, err)
		return nil
	}
	d.mu.Lock()
	// ⚠ 存**完整清单**（models）：快速路径要拿它做"未命中即拒绝"判定与
	// 文案展示。绝不能只存"名字 → key"的映射表 —— 缓存命中的请求会绕过
	// 校验（见 modelCache.models 的注释）。
	d.modelMap[cr.Region] = modelCache{models: models, fetched: time.Now()}
	d.mu.Unlock()
	return models
}

// ---------------------------------------------------------------------------
// auth.Auth ↔ qoder.Cred 的桥接
// ---------------------------------------------------------------------------

// credOf 从 auth.Auth 构造 Qoder 凭证。
//
// ## 为什么需要这个转换
//
// 网关的账号池统一用 auth.Auth（因为选号逻辑要跨产品共用），
// 而 Qoder 的签名需要额外的字段（机器指纹、dt/drt 的明确语义）。
// 两个结构各有存在的理由，桥接放在这一层。
//
// 令牌刷新后的写回：Cred 持有 FilePath，SaveAtomic 会写回原文件，
// 故刷新结果会持久化（与 WorkBuddy 的 SaveAtomic 同语义）。
func credOf(a *auth.Auth) *Cred {
	if a == nil {
		return nil
	}
	c := &Cred{
		UID:         a.UID,
		Nickname:    a.Nickname,
		DT:          a.AccessToken,
		DRT:         a.RefreshToken,
		DTExpiresAt: a.ExpiresAt,
		FilePath:    a.FilePath,
		// 区域由域名判定（与加载层口径一致）
		Region: RegionFromDomain(a.Domain),
	}
	if c.Region == RegionUnknown {
		// 域名认不出区域时按国服（endpointsOf 的默认行为），
		// 但**不写回** Domain —— 让用户能在界面上修正。
		c.Region = RegionCN
	}
	return c
}

// modelNameOf 从 OpenAI 请求体里取模型名。
func modelNameOf(body []byte) string { return modelKeyOf(body) }
