// Package usage 网关侧 Token 用量统计。
//
// 数据来源：每次成功请求结束后，从上游响应（非流式 usage 对象 / 流式 SSE 末帧
// usage）提取 prompt/completion/cache 计量，按「服务器本地日期」聚合。
//
// 四个维度全部按天存储，导出时可按天数范围过滤：
//   - days：全体请求（summary/daily 由它派生）
//   - models：模型 × 日期
//   - accounts：账号 × 日期
//   - accountModels：账号 × 模型 × 日期
//
// accountModels 是唯一**不能**由其它维度推导出来的：models 与 accounts 各自
// 聚合后，交叉关系已经丢失（只知道「甲账号共 3 万」「glm-5.2 共 4 万」，
// 无法还原「甲账号的 glm-5.2 用了多少」）。界面「按账号筛选看用了哪些模型」
// 必须读它，因此单独累计一份，而不是让前端拿两个维度硬凑。
//
// 持久化：与账号池 state.json 同目录的 usage.json；Record 只置脏标志，
// 由后台 flusher 周期落盘，进程退出时 main 调用 Flush 兜底。
// 落盘与导出只含聚合数字、uid 与模型名，不含消息正文或认证信息。
package usage

import (
	"encoding/json"
	"log"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

// dayLayout 聚合用的日期键格式（本地时区），固定宽度保证字符串可直接比较。
const dayLayout = "2006-01-02"

// flushInterval 后台落盘周期。
var flushInterval = 5 * time.Second

// Counters 一组请求的 Token 计量。Input 已包含 CacheRead（与上游 prompt_tokens
// 含 cached tokens 的口径一致），CacheWrite 为单独新增的缓存写入量。
type Counters struct {
	Input      int64 `json:"input"`
	Output     int64 `json:"output"`
	CacheRead  int64 `json:"cacheRead"`
	CacheWrite int64 `json:"cacheWrite"`
	Records    int64 `json:"records"`
}

// Add 累加另一组计量。
func (c Counters) Add(o Counters) Counters {
	return Counters{
		Input:      c.Input + o.Input,
		Output:     c.Output + o.Output,
		CacheRead:  c.CacheRead + o.CacheRead,
		CacheWrite: c.CacheWrite + o.CacheWrite,
		Records:    c.Records + o.Records,
	}
}

// Empty 报告是否没有任何计量（用于跳过记录与聚合）。
func (c Counters) Empty() bool {
	return c.Input == 0 && c.Output == 0 && c.CacheRead == 0 && c.CacheWrite == 0 && c.Records == 0
}

// Value 把计量导出成与宿主 Token 统计页一致的字段口径：
// total = input + output + cacheWrite（不含 cacheRead，避免与 input 重复计数）。
func (c Counters) Value() map[string]any {
	uncached := c.Input - c.CacheRead
	if uncached < 0 {
		uncached = 0
	}
	var hitRate any
	if c.Input > 0 {
		hitRate = float64(c.CacheRead) / float64(c.Input)
	}
	return map[string]any{
		"total":         c.Input + c.Output + c.CacheWrite,
		"input":         c.Input,
		"output":        c.Output,
		"cacheRead":     c.CacheRead,
		"cacheWrite":    c.CacheWrite,
		"uncachedInput": uncached,
		"records":       c.Records,
		"cacheHitRate":  hitRate,
	}
}

// fileState usage.json 的磁盘结构。
//
// AccountModels 用 omitempty：老版本网关写下的文件没有这个键，读回来是 nil，
// 由 load 兜底成空 map —— 不因为「多了个字段」就让既有历史统计失效。
type fileState struct {
	Version       int                                       `json:"version"`
	SavedAt       time.Time                                 `json:"savedAt"`
	Days          map[string]Counters                       `json:"days"`
	Models        map[string]map[string]Counters            `json:"models,omitempty"`
	Accounts      map[string]map[string]Counters            `json:"accounts,omitempty"`
	AccountModels map[string]map[string]map[string]Counters `json:"accountModels,omitempty"`

	// Billed / BillingMeta 计费归属（2026-09-21 新增，**必须落盘**）。
	//
	// # 为什么必须持久化（所有者报的现场）
	//
	// 所有者原话：
	//
	//	「这个应该持久化平台 显示啊,这要不刚开始都没办法区分」
	//
	// 他的截图：`按模型` 里 `deepseek-v4.1-flash` 显示 **1.7B / 4223 次**，
	// 而同一模型的计费明细只有 **3 次** —— 因为它此前**只在内存里**，
	// 网关一重启就清零，于是历史 99.9% 的用量都没有平台归属。
	//
	// 这正是"刚开始都没办法区分"的成因：越早的用量越没有归属信息。
	//
	// 落盘后：平台信息与用量**同时**被记住，重启不再丢。
	Billed map[string]map[string]map[string]Counters `json:"billed,omitempty"`
	// BillingMeta 各 (模型, 归属键) 的元信息（产品/区域/倍率三态）。
	//
	// 键是 `model\x00归属键`（与内存里的形状一致）。倍率**必须**一起存：
	// 只有计数而没有倍率，界面就无法显示"这一份用量按多少倍率计费"。
	BillingMeta map[string]Billing `json:"billingMeta,omitempty"`
}

// Stats 网关 Token 用量聚合器。并发安全；path 为空时纯内存（不落盘）。
type Stats struct {
	mu       sync.Mutex
	dirty    atomic.Bool
	path     string
	days     map[string]Counters
	models   map[string]map[string]Counters
	accounts map[string]map[string]Counters
	// accountModels：uid -> model -> 日期 -> 计量。
	accountModels map[string]map[string]map[string]Counters

	// billed：model -> 计费归属键 -> 日期 -> 计量（2026-09-21 新增）。
	//
	// 计费归属键是 `产品|区域|倍率`（见 billingKey）。同一模型的用量按
	// 归属拆开，界面才能对每一份用量显示它自己的倍率 —— 这是所有者要的
	// 「如果有多个 则需要拆开显示」。
	billed map[string]map[string]map[string]Counters
	// billingMeta：`model\x00归属键` -> 该归属的元信息（含倍率三态）。
	//
	// 为什么不把它塞进 Counters：Counters 是**可加的计量**（Add 语义明确），
	// 而倍率是归属的**属性**、不可加（两个倍率相加没有意义）。
	// 混在一起会让 Add 悄悄产生一个假的倍率。
	billingMeta map[string]Billing

	// now 供测试注入时钟；nil 时用 time.Now。
	now func() time.Time
}

// New 构建统计器；path 非空时加载旧数据并启动后台落盘。
func New(path string) *Stats {
	s := &Stats{
		path:          path,
		days:          map[string]Counters{},
		models:        map[string]map[string]Counters{},
		accounts:      map[string]map[string]Counters{},
		accountModels: map[string]map[string]map[string]Counters{},
		billed:        map[string]map[string]map[string]Counters{},
		billingMeta:   map[string]Billing{},
	}
	if path != "" {
		s.load()
		go s.flusher()
	}
	return s
}

func (s *Stats) clock() time.Time {
	if s.now != nil {
		return s.now()
	}
	return time.Now()
}

// Record 记录一次成功请求的用量；uid/model 为空时归一成 "-"。
func (s *Stats) Record(uid, model string, c Counters) {
	s.RecordAt(uid, model, s.clock(), c)
}

// Billing 一次请求的**计费归属**：走的是哪个平台、哪个区域、倍率多少。
//
// # 为什么需要它（2026-09-21 所有者要求）
//
// 所有者原话：
//
//	「兼容网关的token用量也要显示出这个模型的倍率（如果有多个 则需要拆开显示）」
//
// 「多个」指的是：同一个模型名可能由多个平台/区域提供（实测 `glm-5.3`
// 同时在 WorkBuddy 与 ZCode 上、`deepseek-v4.1-flash` 两区倍率相反）。
// 只按模型名聚合时，这些来源的用量会混成一行，倍率无从显示 ——
// 而用户要的正是"这一份用量是按哪个倍率计费的"。
//
// 故按 (平台, 区域, 倍率) 三元组拆开记账。三元组而不是二元组：
// 同一平台同一区域的倍率也可能随上游调整而变化（实测国服倍率会变），
// 把不同倍率的用量合并会让"倍率"这一列失去意义。
type Billing struct {
	// Product 平台标识：workbuddy / qoder / zcode。
	Product string
	// Region 区域码：cn / intl（空 = 该平台不分区）。
	Region string
	// Multiplier 本次请求的计费倍率；nil = 未声明（**不是**免费）。
	Multiplier *float64
	// HasMultiplier 该平台是否有倍率概念（false = 界面不显示倍率列）。
	HasMultiplier bool
}

// RecordBilled 同 Record，但额外记录计费归属（平台/区域/倍率）。
//
// 与 Record 并存而不是替换它：既有调用点（与老测试）只关心 uid/model，
// 让它们继续用 Record 可以零改动；需要倍率的调用点用本方法。
func (s *Stats) RecordBilled(uid, model string, b Billing, c Counters) {
	s.recordAt(uid, model, b, s.clock(), c)
}

// RecordAt 同 Record，但显式指定时刻（测试用）。
func (s *Stats) RecordAt(uid, model string, at time.Time, c Counters) {
	s.recordAt(uid, model, Billing{}, at, c)
}

// recordAt 是 Record / RecordBilled / RecordAt 的公共实现。
func (s *Stats) recordAt(uid, model string, b Billing, at time.Time, c Counters) {
	if c.Input == 0 && c.Output == 0 && c.CacheRead == 0 && c.CacheWrite == 0 {
		// 上游未返回可用 usage：不计数，也不产生空记录。
		return
	}
	day := at.In(time.Local).Format(dayLayout)
	entry := Counters{
		Input:      c.Input,
		Output:     c.Output,
		CacheRead:  c.CacheRead,
		CacheWrite: c.CacheWrite,
		Records:    1,
	}
	if model == "" {
		model = "-"
	} else {
		// ⚠ 统计键必须是**裸模型名**（去掉 `平台:` / `区域:` 路由前缀）。
		//
		// # 为什么在这里做，而不是信任调用方
		//
		// 所有者原话（2026-09-21）：
		//
		//	「不要再把 平台:模型名 这种类似的 单独列一个统计了,这是错误的」
		//
		// 现场：用量列表里同时出现
		//
		//	`国服:deepseek-v4.1-flash`   12 tokens / 1 次   ← 错
		//	`deepseek-v4.1-flash · 国服`  1.6B / 4014 次    ← 对
		//
		// 而这两个其实是**同一个模型的同一份用量**，只是前者把路由前缀
		// 当成了模型名的一部分。
		//
		// 上游调用点（server 的 chatStat）已修过一版（`setModel` 会剥前缀），
		// 但那只覆盖了它自己那几个入口；**任何**别的调用方（含历史数据、
		// 未来的新入口）仍可能传进带前缀的名字。放在这里等于给整个
		// usage 包上一道**兜底**：不管谁怎么传，落进统计的都是裸名。
		//
		// 效果：新写入的 `国服:xxx` 会与既有的 `xxx` **落进同一个键**，
		// 历史遗留的那 12 tokens 自动归并到 `deepseek-v4.1-flash` 里，
		// 不需要单独清洗旧数据。
		model = bareModelName(model)
	}
	if uid == "" {
		uid = "-"
	}

	s.mu.Lock()
	defer s.mu.Unlock()
	addTo(s.days, day, entry)
	addTo(nested(s.models, model), day, entry)
	addTo(nested(s.accounts, uid), day, entry)
	addTo(nested2(s.accountModels, uid, model), day, entry)
	// 计费维度：按 (平台, 区域, 倍率) 拆开。
	//
	// 只有**声明了平台**的请求才进这张表 —— Billing 零值表示调用方
	// （老调用点）没提供归属信息，那种情况混进一个 "" 平台会让界面
	// 出现一行没有来源的用量，比不显示更让人困惑。
	if b.Product != "" || b.HasMultiplier {
		key := billingKey(b)
		addTo(nested2(s.billed, model, key), day, entry)
		// 记住该 (模型, 归属) 的元信息，供 Snapshot 输出倍率。
		s.billingMeta[model+"\x00"+key] = b
	}
	s.dirty.Store(true)
}

// bareModelName 去掉模型名上的**路由前缀**，只留裸模型名。
//
// # 为什么 usage 包自己实现一份（而不是复用 server.resolveModel）
//
// `resolveModel` 住在 `internal/server`。usage 是**更底层**的包（server 依赖它），
// 反向依赖会成环。而为一次字符串切分把 `resolveModel` 下移，会让
// 一个纯统计包背上"产品/区域前缀"这些路由概念。
//
// 故这里只做**统计需要的那一件事**：把 `前缀:前缀:模型名` 收敛成 `模型名`。
// 路由语义（哪个前缀合法、归一到什么）仍由 server 负责 —— 两边职责不重叠：
//
//	server.resolveModel   决定**路由**（前缀 → 产品/区域）
//	usage.bareModelName   决定**归并**（前缀一律丢掉，只为统计聚合）
//
// # 已知取舍
//
// 若某个模型名**本身**含冒号，这里会误切。实测没有这种模型名（上游命名是
// `glm-5.3-flash` 这类），且 server 侧同一套规则已跑了很久没出问题。
// 真出现时应当修命名，而不是让统计为此变复杂。
func bareModelName(model string) string {
	s := strings.TrimSpace(model)
	if !strings.Contains(s, ":") {
		return s
	}
	// 只切**前两段**：路由前缀最多「产品:区域:」两层（如 `workbuddy:国服:xxx`）。
	// 多切会把可能含冒号的模型名吃掉。
	parts := strings.SplitN(s, ":", 3)
	switch len(parts) {
	case 3:
		// 产品:区域:模型  → 取第三段
		if tail := strings.TrimSpace(parts[2]); tail != "" {
			return tail
		}
		// 形如 `平台:模型:`（尾部空）—— 退回第二段
		if mid := strings.TrimSpace(parts[1]); mid != "" {
			return mid
		}
	case 2:
		// 平台:模型 或 区域:模型 → 取第二段
		if tail := strings.TrimSpace(parts[1]); tail != "" {
			return tail
		}
	}
	// 切完是空（如 `::`）——原样返回，交给上面 `model == ""` 的兜底逻辑。
	return s
}

// billingKey 把计费归属压成一个稳定的字符串键。
//
// 倍率参与键：同一平台同一区域在不同倍率下的用量要分开（见 Billing 的注释）。
// nil 倍率用 "?" 表示，与 0（免费）区分开 —— 那正是三态语义的落点。
func billingKey(b Billing) string {
	mult := "?"
	if b.Multiplier != nil {
		mult = strconv.FormatFloat(*b.Multiplier, 'f', -1, 64)
	}
	return b.Product + "|" + b.Region + "|" + mult
}

func nested(m map[string]map[string]Counters, key string) map[string]Counters {
	inner, ok := m[key]
	if !ok {
		inner = map[string]Counters{}
		m[key] = inner
	}
	return inner
}

// nested2 同 nested，但多一层键（账号 -> 模型 -> 日期）。
//
// 单独写一个而不是把 nested 泛型化：Go 的 map 嵌套每加一层类型就变一次，
// 泛型版本为了省这几行会引入类型参数与约束，读起来比多一个 10 行函数更绕。
func nested2(m map[string]map[string]map[string]Counters, key, sub string) map[string]Counters {
	byModel, ok := m[key]
	if !ok {
		byModel = map[string]map[string]Counters{}
		m[key] = byModel
	}
	return nested(byModel, sub)
}

func addTo(m map[string]Counters, key string, c Counters) {
	m[key] = m[key].Add(c)
}

// Snapshot 导出一份可 JSON 序列化的聚合快照。days<=0 表示全部历史。
func (s *Stats) Snapshot(days int) map[string]any {
	s.mu.Lock()
	defer s.mu.Unlock()

	now := s.clock()
	cutoff := ""
	if days > 0 {
		cutoff = now.AddDate(0, 0, -(days - 1)).Format(dayLayout)
	}

	summary := sumDays(s.days, cutoff)
	models := groupList(s.models, cutoff)
	accounts := groupList(s.accounts, cutoff)
	daily := daySeries(s.days, cutoff)

	dailyByModel := map[string]any{}
	for model, series := range s.models {
		dailyByModel[model] = daySeries(series, cutoff)
	}

	// accountModels：账号 -> 模型分组列表。
	//
	// 只保留**该范围内仍有计量**的账号与模型（groupList 会丢掉全空的），
	// 因此「全部为 0 的账号」不会以空数组形式出现在响应里 —— 界面据此
	// 区分「这个号这段时间没消耗」与「这个号根本不在统计里」。
	accountModels := map[string]any{}
	for uid, byModel := range s.accountModels {
		groups := groupList(byModel, cutoff)
		if len(groups) == 0 {
			continue
		}
		accountModels[uid] = groups
	}

	// modelBilling：模型 -> 该模型的**按计费归属拆开**的用量列表。
	//
	// # 为什么需要它（2026-09-21 所有者要求）
	//
	// 所有者原话：「兼容网关的token用量也要显示出这个模型的倍率
	//（如果有多个 则需要拆开显示）」。
	//
	// `models` 那一份是按模型名聚合的（一行一个模型），无法承载倍率 ——
	// 同一个模型名可能由多个平台/区域提供，各自倍率不同（实测
	// `deepseek-v4.1-flash` 国服计费、国际版免费），合并成一行后
	// 只能显示其中一个倍率，必然误导。
	//
	// 故这里额外给一份**按归属拆开**的视图：
	//
	//	{ "deepseek-v4.1-flash": [
	//	    { key:"deepseek-v4.1-flash", product:"workbuddy", region:"cn",
	//	      creditMultiplier:0.03, hasMultiplier:true, total:… },
	//	    { … region:"intl", creditMultiplier:0, … } ] }
	//
	// 前端据此在"用量"处把同一模型的多份用量分行显示、各带自己的倍率。
	modelBilling := map[string]any{}
	for model, byBilling := range s.billed {
		groups := groupList(byBilling, cutoff)
		if len(groups) == 0 {
			continue
		}
		for _, g := range groups {
			key, _ := g["key"].(string)
			b := s.billingMeta[model+"\x00"+key]
			// 归属的元信息（平台/区域/倍率三态）逐项附上。
			//
			// 倍率用 `creditMultiplier` 键并允许 null：与 /v1/models 的
			// channels 同名字段保持同一套三态语义，前端可以用同一个渲染函数。
			g["product"] = b.Product
			if b.Region != "" {
				g["region"] = b.Region
			}
			g["hasMultiplier"] = b.HasMultiplier
			if b.HasMultiplier {
				// ⚠ 必须**解引用**：Billing.Multiplier 是 *float64，
				// 直接把指针放进 map 会序列化成指针地址（实测
				// `0x15bb13fb22f8`）而不是数字 —— 那是个静默的错值，
				// 界面会把它当成一个巨大的倍率显示。
				//
				// nil 保持 nil：序列化成 JSON null，正是"未声明"这一态。
				if b.Multiplier != nil {
					g["creditMultiplier"] = *b.Multiplier
				} else {
					g["creditMultiplier"] = nil
				}
			}
		}
		modelBilling[model] = groups
	}

	// 回填：给**没有计费归属**的历史用量补上平台/区域。
	//
	// # 为什么要回填（所有者 2026-09-21）
	//
	// 所有者原话：
	//
	//	「这个应该持久化平台 显示啊,这要不刚开始都没办法区分」
	//
	// 他的截图：`按模型` 里 `deepseek-v4.1-flash` 是 **1.7B / 4223 次**，
	// 而同一模型的计费明细只有 **3 次** —— 因为计费归属此前**只在内存里**
	//（`fileState` 缺该字段），网关一重启就清零。于是历史 99.9% 的用量
	// 都没有平台信息，"刚开始"的那些尤其如此。
	//
	// 落盘（本文件 fileState 的 Billed/BillingMeta）只解决**今后**；
	// 已经丢掉的归属要靠回填。
	//
	// # 依据：accountModels 里有 (账号 × 模型 × 日期) 的交叉
	//
	// 账号 uid 的**形态**本身就带平台信息：
	//
	//	zcode-xxxx    → zcode
	//	qoder-xxxx    → qoder
	//	裸 UUID       → workbuddy（历史账号没有前缀，见下）
	//
	// 而 WorkBuddy 的账号在 auth 层用**空串**表示（见 auth.ProductOf：
	// 空串归一成 workbuddy），落进 usage 时就是裸 UUID。这与
	// `accounts.json` 里的 uid 形态一致（实测 11/13 能对上）。
	//
	// ⚠ 区域**补不出来**：历史数据里没有区域信息，而倍率又依赖区域
	//（国服 0.03 / 国际版 0）。故回填只补 `product`，
	// **不编造 region 与倍率** —— 编一个会让界面显示错误的计费，
	// 比显示"未知"更糟。前端的倍率列对缺 region 的行会显示"未记录"。
	//
	// # 为什么用 accountModels 而不是别的
	//
	// 它是唯一带账号维度的持久化数据。`models` 只有模型名、
	// `accounts` 只有账号 —— 都无法建立"这个模型×这个账号"的关联。
	s.backfillBillingLocked(modelBilling, cutoff)

	var rangeDays any
	if days > 0 {
		rangeDays = days
	}
	return map[string]any{
		"generatedAt":   now.UnixMilli(),
		"rangeDays":     rangeDays,
		"summary":       summary.Value(),
		"models":        models,
		"accounts":      accounts,
		"accountModels": accountModels,
		"modelBilling":  modelBilling,
		"daily":         daily,
		"dailyByModel":  dailyByModel,
	}
}

// productFromUID 由账号 uid 的**形态**推断平台。
//
// # 依据（实测所有者本机的数据）
//
//	qoder-{qoder-uid}-…     → qoder      （带前缀）
//	zcode-abcdef123456   → zcode      （带前缀）
//	{wb-uid}-58ce-…      → workbuddy  （裸 UUID）
//
// 裸 UUID 归 WorkBuddy 是**依据 auth 层的约定**，不是猜：
// `auth.Auth.Product` 为空串时 `ProductOf()` 归一成 `ProductWorkBuddy`
//（见 auth.go），而 WorkBuddy 账号落进 usage 时用的就是它自己的 uid
//（裸 UUID，实测与 `accounts.json` 里的 `uid` 一致，11/13 能对上）。
//
// 返回 "" 表示**无法判定** —— 调用方必须据此跳过，不要归到一个默认平台。
// 这一点是刻意的：把一个来源不明的用量说成 WorkBuddy，会污染
// WorkBuddy 的统计并让倍率显示错误，比显示"未知"更糟。
func productFromUID(uid string) string {
	switch {
	case strings.HasPrefix(uid, "qoder-"):
		return "qoder"
	case strings.HasPrefix(uid, "zcode-"):
		return "zcode"
	case isBareUUID(uid):
		return "workbuddy"
	default:
		return "" // 判不出就别猜
	}
}

// isBareUUID 报告 s 是否为 36 字符的 UUID（8-4-4-4-12，含连字符）。
//
// 只做形态判定，不做严格的十六进制校验：目的是区分
// "裸 UUID"（= WorkBuddy 账号）与"带前缀的账号"（qoder-/zcode-），
// 而不是验证这个 UUID 合法。
func isBareUUID(s string) bool {
	if len(s) != 36 {
		return false
	}
	for i, c := range s {
		switch i {
		case 8, 13, 18, 23:
			if c != '-' {
				return false
			}
		default:
			isHex := (c >= '0' && c <= '9') || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F')
			if !isHex {
				return false
			}
		}
	}
	return true
}

// backfillBillingLocked 给**没有计费归属**的历史用量补上 `product`。
//
// # 补什么、不补什么
//
//	补   product  —— 由账号 uid 的形态确定（见 productFromUID）
//	不补 region   —— 历史数据里没有这个信息，编一个会让倍率显示错
//	不补 倍率      —— 倍率依赖区域，同理
//
// 前端对只有 product 而没有倍率的行，倍率列会显示"未记录"——
// 那是**如实**的表达，比编造一个 0 或 0.03 好得多。
//
// # 为什么按 (模型 × 平台) 汇总
//
// 同一模型可能有多个 WorkBuddy 账号用过（都归 workbuddy），
// 它们应当合并成**一条** `deepseek-v4.1-flash · workbuddy`，
// 而不是每个账号一条 —— 界面的分组粒度是"模型 × 平台"，
// 不是"模型 × 账号"（账号维度另有 `accountModels`）。
//
// # 与既有归属的合并
//
// 若某模型**已经有**该平台的归属条目（新数据记录的，带倍率），
// 回填的计数会**并入**它而不是另起一行 —— 否则同一平台会出现两行。
// 但倍率仍用既有那份（回填的没有倍率信息）。
//
// modelBilling 是 `map[模型][]条目`；本函数就地改它。
func (s *Stats) backfillBillingLocked(modelBilling map[string]any, cutoff string) {
	// 先算每个 (模型, 平台) 的已有归属，供后面判断"是否需要回填"。
	type pk struct{ model, product string }
	known := map[pk]bool{}
	for model, v := range modelBilling {
		if arr, ok := v.([]map[string]any); ok {
			for _, g := range arr {
				if p, _ := g["product"].(string); p != "" {
					known[pk{model, p}] = true
				}
			}
		}
	}

	// ⚠ 用 (模型, 平台) → 已建的回填条目 做累加索引。
	//
	// 为什么必须有它，而不是靠 `known` 判断"跳过"：
	// 同一平台可能**多个账号**用过同一模型（实测 WorkBuddy 有 19 个账号）。
	// 第一版写的是"看到 known 里有该 (模型,平台) 就 continue" —— 于是
	// 只有**第一个**账号的用量被回填，其余的静默丢掉
	//（测试断言 132 实际 120，正好少了第二个账号的 12）。
	//
	// 正确语义是"累加"：同平台的多个账号合并成一条。
	// `backfilled` 记录已经**由回填创建**的条目，供后续账号并进去。
	backfilled := map[pk]map[string]any{}

	for uid, byModel := range s.accountModels {
		product := productFromUID(uid)
		if product == "" {
			continue // 判不出平台 → 不猜
		}
		for model, perDay := range byModel {
			// 该模型在该平台上**已有真实归属**（带倍率的那份）→ 不重复回填。
			// 那份更权威：它带 region 与倍率，而回填补不出这些。
			if known[pk{model, product}] {
				continue
			}
			// 按同一 cutoff 过滤，与其它维度口径一致。
			c := sumDays(perDay, cutoff)
			if c.Empty() {
				continue
			}
			// 已经为这个 (模型,平台) 建过回填条目 → **累加**进去
			//（同平台的多个账号合并成一条，见上面 index 的说明）。
			if g := backfilled[pk{model, product}]; g != nil {
				writeCounters(g, countersFrom(g).Add(c))
				continue
			}
			// 新建一条。key 用模型名（与既有条目一致：界面按键去重/匹配）。
			g := map[string]any{
				"key":     model,
				"product": product,
				// ⚠ 不设 region / hasMultiplier / creditMultiplier：
				// 历史数据没有这些信息，前端据此显示"未记录"。
			}
			writeCounters(g, c)
			modelBilling[model] = append(asRows(modelBilling[model]), g)
			backfilled[pk{model, product}] = g
		}
	}
}

// asRows 把快照里的模型条目归一成 `[]map[string]any`。
//
// 为什么需要：`modelBilling` 的值类型是 `any`（快照是
// `map[string]any`），新建条目时可能是 nil（该模型此前没有任何归属）。
// 直接 `append(nil, …)` 得到的是 `[]map[string]any` 类型 **不匹配** 的
// 切片，JSON 序列化后会变成 `null` 而不是数组 —— 一个静默的坏形状。
func asRows(v any) []map[string]any {
	if arr, ok := v.([]map[string]any); ok {
		return arr
	}
	return nil
}

// writeCounters 把 Counters 的各个字段写进快照条目的 map。
//
// 与 `Counters.Value()` 同一形状（见 groupList 的用法），
// 但那条路径只处理单日/汇总，这里需要一个可复用的写法。
func writeCounters(g map[string]any, c Counters) {
	for k, v := range c.Value() {
		g[k] = v
	}
}

// countersFrom 从快照条目里读回 Counters（writeCounters 的逆）。
//
// 为什么需要"读回"：回填要**累加**同一 (模型,平台) 下多个账号的用量，
// 而快照条目是 map。每次累加都从 map 读回当前值再 Add，
// 避免为回填另建一套影子状态（那会与 map 分叉）。
func countersFrom(g map[string]any) Counters {
	return Counters{
		Input:      intOf(g["input"]),
		Output:     intOf(g["output"]),
		CacheRead:  intOf(g["cacheRead"]),
		CacheWrite: intOf(g["cacheWrite"]),
		Records:    intOf(g["records"]),
	}
}

func sumDays(m map[string]Counters, cutoff string) Counters {	var total Counters
	for day, c := range m {
		if cutoff != "" && day < cutoff {
			continue
		}
		total = total.Add(c)
	}
	return total
}

// groupList 把「键 -> 日期 -> 计量」两层结构压平成按 total 降序的列表。
func groupList(m map[string]map[string]Counters, cutoff string) []map[string]any {
	out := make([]map[string]any, 0, len(m))
	for key, series := range m {
		c := sumDays(series, cutoff)
		if c.Empty() {
			continue
		}
		value := c.Value()
		value["key"] = key
		out = append(out, value)
	}
	sortByTotalDesc(out)
	return out
}

// daySeries 把日聚合导出成按日期升序的序列（供前端趋势图使用）。
func daySeries(m map[string]Counters, cutoff string) []map[string]any {
	keys := make([]string, 0, len(m))
	for day := range m {
		if cutoff != "" && day < cutoff {
			continue
		}
		keys = append(keys, day)
	}
	sort.Strings(keys)
	out := make([]map[string]any, 0, len(keys))
	for _, day := range keys {
		value := m[day].Value()
		value["key"] = day
		out = append(out, value)
	}
	return out
}

func sortByTotalDesc(values []map[string]any) {
	sort.SliceStable(values, func(i, j int) bool {
		return intOf(values[i]["total"]) > intOf(values[j]["total"])
	})
}

// Flush 同步把内存状态落盘（幂等：无变更或纯内存模式直接返回）。
func (s *Stats) Flush() {
	if s.path == "" || !s.dirty.Load() {
		return
	}
	s.mu.Lock()
	raw, err := json.MarshalIndent(s.fileStateLocked(), "", "  ")
	s.dirty.Store(false)
	s.mu.Unlock()
	if err != nil {
		log.Printf("usage: 序列化失败: %v", err)
		return
	}

	if dir := filepath.Dir(s.path); dir != "" {
		_ = os.MkdirAll(dir, 0o755)
	}
	tmp := s.path + ".tmp"
	if err := os.WriteFile(tmp, raw, 0o600); err != nil {
		log.Printf("usage: 落盘失败: %v", err)
		s.dirty.Store(true)
		return
	}
	if err := os.Rename(tmp, s.path); err != nil {
		log.Printf("usage: 落盘失败: %v", err)
		s.dirty.Store(true)
	}
}

func (s *Stats) flusher() {
	t := time.NewTicker(flushInterval)
	defer t.Stop()
	for range t.C {
		s.Flush()
	}
}

// fileStateLocked 收集内存状态为磁盘结构。调用方必须已持有 s.mu。
func (s *Stats) fileStateLocked() fileState {
	return fileState{
		Version:       1,
		SavedAt:       s.clock(),
		Days:          s.days,
		Models:        s.models,
		Accounts:      s.accounts,
		AccountModels: s.accountModels,
		// ⚠ 计费归属必须一起落盘（2026-09-21）。
		//
		// 此前它只在内存里 ⇒ 网关一重启就清零 ⇒ 历史用量全部没有平台归属
		//（所有者现场：同一模型 4223 次用量 vs 3 次带归属）。
		// 见 fileState 里 Billed 字段的注释。
		Billed:      s.billed,
		BillingMeta: s.billingMeta,
	}
}

func (s *Stats) load() {
	raw, err := os.ReadFile(s.path)
	if err != nil {
		return
	}
	var fs fileState
	if err := json.Unmarshal(raw, &fs); err != nil {
		log.Printf("usage: 解析 %s 失败，忽略: %v", s.path, err)
		return
	}
	if fs.Version > 1 {
		log.Printf("usage: %s 版本 %d 高于当前支持，忽略", s.path, fs.Version)
		return
	}
	if fs.Days != nil {
		s.days = fs.Days
	}
	// ⚠ 读取时也要归一（2026-09-21）。
	//
	// 只改写入点**不够**：所有者磁盘上的 usage.json 里已经存着
	//
	//	models["zcode:GLM-5.3-Flash"]
	//	models["国服:deepseek-v4.1-flash"]
	//
	// 这两条是**旧版本写下的**，光堵住新写入只会让它们"不再增长"，
	// 界面里那两行**依然存在** —— 而所有者的要求正是"不要单独列一个统计"。
	//
	// 故在装载时把它们并进裸名键。这样：
	//
	//	· 存量数据自动归并，不需要单独写一次清洗脚本或改用户文件
	//	· 与写入点的归一化**同一套规则**（都走 bareModelName），不会分叉
	//	· 归并发生在内存里；下次 flush 写出的是已归并的形态，
	//	  于是磁盘数据也会**自愈**
	if fs.Models != nil {
		s.models = normalizeModelKeys(fs.Models)
	}
	if fs.Accounts != nil {
		s.accounts = fs.Accounts
	}
	// 老版本文件没有 accountModels（字段缺失 = nil）：保持 New 里的空 map，
	// 让后续 Record 直接写入而不是往 nil map 里塞（那会 panic）。
	//
	// 这里**不**拿 models/accounts 去反推一份：反推出来的是「每个账号都用了
	// 全部模型」这种假交叉，界面会显示成一堆错误的模型明细，比缺失更糟。
	if fs.AccountModels != nil {
		// 每层的模型键同样要归一（形状是 {账号: {模型: {日期: counters}}}）。
		s.accountModels = make(map[string]map[string]map[string]Counters, len(fs.AccountModels))
		for uid, perModel := range fs.AccountModels {
			s.accountModels[uid] = normalizeModelKeys(perModel)
		}
	}
	// 计费归属（2026-09-21 起落盘）。
	//
	// 模型键同样要归一 —— 老文件里可能有 `国服:xxx` 这种带前缀的计费键
	//（它由旧的写入路径产生）。不归一会让界面上同一模型出现两行计费明细。
	if fs.Billed != nil {
		s.billed = make(map[string]map[string]map[string]Counters, len(fs.Billed))
		for model, byKey := range fs.Billed {
			bare := bareModelName(model)
			dst := s.billed[bare]
			if dst == nil {
				dst = make(map[string]map[string]Counters, len(byKey))
				s.billed[bare] = dst
			}
			for key, perDay := range byKey {
				day := dst[key]
				if day == nil {
					day = make(map[string]Counters, len(perDay))
					dst[key] = day
				}
				for d, c := range perDay {
					day[d] = day[d].Add(c)
				}
			}
		}
	}
	if fs.BillingMeta != nil {
		s.billingMeta = make(map[string]Billing, len(fs.BillingMeta))
		for k, v := range fs.BillingMeta {
			// 键是 `model\x00归属键`：模型那半也要归一，否则查不到
			//（查表用的是 `model+"\x00"+key`，见 Snapshot）。
			if i := strings.IndexByte(k, 0); i >= 0 {
				k = bareModelName(k[:i]) + k[i:]
			}
			s.billingMeta[k] = v
		}
	}
}

// normalizeModelKeys 把「模型 → 日期 → 计数」这一层的键归一成裸模型名，
// 同名的用量**累加**。
//
// 累加而不是覆盖：`deepseek-v4.1-flash` 与 `国服:deepseek-v4.1-flash`
// 是**同一份用量**的两种写法，丢掉任一份都会让数字变小 ——
// 那比多显示一行更糟（用户会以为用量丢了）。
func normalizeModelKeys(in map[string]map[string]Counters) map[string]map[string]Counters {
	if len(in) == 0 {
		return in
	}
	// 先判断"是否需要归一"，避免无谓地重建整个 map（这份数据结构很大，
	// 每次启动都全量复制会拖慢启动）。
	need := false
	for k := range in {
		if bare := bareModelName(k); bare != k {
			need = true
			break
		}
	}
	if !need {
		return in
	}
	out := make(map[string]map[string]Counters, len(in))
	for model, perDay := range in {
		bare := bareModelName(model)
		dst := out[bare]
		if dst == nil {
			dst = make(map[string]Counters, len(perDay))
			out[bare] = dst
		}
		for day, c := range perDay {
			dst[day] = dst[day].Add(c)
		}
	}
	return out
}

// ParseOpenAIUsage 从上游 usage 对象提取计量。
//
// 返回 ok=false 表示该对象没有任何可识别的 token 字段（例如上游漏发 usage），
// 调用方应跳过本次统计。字段兼容 OpenAI 标准命名与 CodeBuddy 实际会返回的别名：
//   - input:  prompt_tokens / input_tokens
//   - output: completion_tokens / output_tokens
//   - cacheRead:  prompt_cache_hit_tokens > prompt_tokens_details.cached_tokens
//     > input_tokens_details[].cached_tokens
//   - cacheWrite: prompt_cache_write_tokens / cache_write_input_tokens /
//     cache_creation_input_tokens
func ParseOpenAIUsage(u map[string]any) (Counters, bool) {
	if u == nil {
		return Counters{}, false
	}
	input, hasInput := firstNumber(u, "prompt_tokens", "input_tokens")
	output, hasOutput := firstNumber(u, "completion_tokens", "output_tokens", "output_text_tokens")
	if !hasInput && !hasOutput {
		// 兜底：只认明确出现的 token 字段，避免把无关对象记成 0 记录。
		if !hasAnyKey(u, "prompt_tokens", "input_tokens", "completion_tokens", "output_tokens") {
			return Counters{}, false
		}
	}

	read, _ := firstNumber(u, "prompt_cache_hit_tokens", "cache_read_input_tokens")
	if read == 0 {
		if details, ok := u["prompt_tokens_details"].(map[string]any); ok {
			read, _ = firstNumber(details, "cached_tokens")
		}
	}
	if read == 0 {
		if details, ok := u["input_tokens_details"].([]any); ok {
			for _, item := range details {
				if m, ok := item.(map[string]any); ok {
					if n, ok := firstNumber(m, "cached_tokens"); ok && n > 0 {
						read = n
						break
					}
				}
			}
		}
	}
	write, _ := firstNumber(u, "prompt_cache_write_tokens", "cache_write_input_tokens", "cache_creation_input_tokens")

	return Counters{Input: input, Output: output, CacheRead: read, CacheWrite: write}, true
}

func firstNumber(m map[string]any, keys ...string) (int64, bool) {
	for _, key := range keys {
		if v, ok := m[key]; ok {
			return numberValue(v), true
		}
	}
	return 0, false
}

func hasAnyKey(m map[string]any, keys ...string) bool {
	for _, key := range keys {
		if _, ok := m[key]; ok {
			return true
		}
	}
	return false
}

func numberValue(v any) int64 {
	switch n := v.(type) {
	case float64:
		return int64(n)
	case float32:
		return int64(n)
	case int:
		return int64(n)
	case int32:
		return int64(n)
	case int64:
		return n
	case json.Number:
		if parsed, err := n.Int64(); err == nil {
			return parsed
		}
		if parsed, err := n.Float64(); err == nil {
			return int64(parsed)
		}
	case string:
		trimmed := strings.TrimSpace(n)
		if parsed, err := json.Number(trimmed).Int64(); err == nil {
			return parsed
		}
	}
	return 0
}

func intOf(v any) int64 {
	return numberValue(v)
}
