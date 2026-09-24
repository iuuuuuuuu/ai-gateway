package server

// unusual.go 上游 3012「unusual activity」的**产品级熔断**：识别出来就别再换号了。
// 不同产品的熔断状态严格隔离：ZCode 的 3012 不得阻断 Qoder/WorkBuddy。
//
// # 要解决的问题（所有者 2026-09-21 明确要求）
//
// 所有者原话：
//
//	「2」（选择：加"ZCode 请求频率限制"或"3012 后自动退避一段时间不重试"）
//	「就在刚刚这个版本又报错了」（同一晚遇到 3012）
//
// # 旧行为为什么会**放大**风控
//
// 3012 的 HTTP 载体是 405，而 `upstream.Classify` 里 405 落进通用
// `ErrClient`（见 client.go 的分类链：它只对 429/402/404/5xx/若干业务码
// 做了特殊归类，405 不在其中）。`applyErrorPolicy` 对 `ErrClient` 的处理是
// 「只换号不罚，防雪崩」—— 于是网关会**把池里 21 个账号挨个打一遍**，
// 每个都吃一次 3012。
//
// 对上游风控而言这正是最坏形状：**同一出口 IP 在极短时间内用一批不同凭证
// 反复触发同一规则**。这与 wafip.go 里记的那条完全同构：
//
//	「所有号轮一遍全 403，而网关还在继续换号，把请求放大 MaxRotate 倍 ——
//	  恰好是 WAF 最想惩罚的行为，于是封禁被越打越重。」
//
// # 判据：3012 是**请求级/瞬时**风控，不是账号问题
//
// 这一条已经查实（证据在 zcode/provider.go 的文件头）：
//
//	· 同一账号 07:01 是 3012、07:02 就成功
//	· **官方客户端自己也吃 3012**（日志 turn.failed，405 code=3012）
//	· 换端点 / 改请求形状 / 三种 deviceMid / system prompt A/B —— 全部被证伪
//
// ⇒ 既然与账号无关，**换号就是纯浪费**，而且每次浪费都往风控上再添一笔。
//
// # 与 wafip.go 的分工（两者判据不同，不要合并）
//
//	wafip.go   判据 = **多个不同账号**都吃 403 ⇒ 出口 IP 被拦
//	unusual.go 判据 = **单个**账号吃到 3012     ⇒ 上游对本次请求的风控
//
// 为什么 3012 不需要"多个账号"才成立：它已经在官方客户端上复现过，
// 即**与我们的账号池无关**。再花 20 次请求去"交叉验证"只会加重风控 ——
// 而那正是本模块要消除的行为。
//
// # 为什么用滑动窗口而不是永久标记
//
// 3012 是**有时长效期**的风控（会自己过期）。永久标记会让网关在解封后
// 仍然拒绝服务，把"临时风控"变成"我们自己造成的永久故障"。
// 窗口过后自动恢复，无需人工干预。

import (
	"encoding/json"
	"strconv"
	"strings"
	"sync"
	"time"
)

const (
	// unusualWindow 3012 熔断窗口：这段时间内**不再尝试**上游。
	//
	// # 取值依据
	//
	// 已知的实测恢复尺度：
	//
	//	· provider.go 记录：同一账号 07:01 是 3012、07:02 就成功（**1 分钟**）
	//	· 所有者本机更早一次现场：约 1.5 小时才整体恢复
	//
	// 两者差两个数量级，说明它**没有固定恢复时间**。故取一个折中：
	//
	//	90 秒 —— 比"1 分钟就恢复"稍长（避免刚过窗口又立刻撞上），
	//	又远小于"1.5 小时"（那种情况等多久都没用，不如让请求快速失败、
	//	把选择权交回用户，而不是让他盯着一个转圈的重试）。
	//
	// ⚠ 这个值是**保守猜测**，不是实测出来的最优点。选它的理由是
	// "宁可少打几次"：熔断短了只是多撞一次风控，长了才影响可用性 ——
	// 而在一段明确的瞬时风控里，多撞几次的代价更大（见文件头）。
	unusualWindow = 90 * time.Second

	// unusualCooldownAfterBreaker 连续多轮都撞 3012 时，把窗口翻倍的上限。
	//
	// 为什么要翻倍：若 90 秒后仍然 3012，说明这次风控持续得比典型情况长。
	// 固定 90 秒会让网关每 90 秒去撞一次，对风控而言是**稳定的周期性行为**
	// —— 比随机重试更像脚本。翻倍 + 上限让它逐步退让而不是规律叩门。
	unusualMaxWindow = 30 * time.Minute
)

// unusualBreaker 单一产品的 3012 熔断状态。
//
// 3012 是上游产品接口返回的业务风控码，**不能跨产品传播**：
// zcode 的 3012 只说明 ZCode 上游当前拒绝请求，不能阻断 Qoder/WorkBuddy。
// 同一产品内仍跨账号共享，因为换号会放大同一个上游/出口的风控。
type unusualBreaker struct {
	mu sync.Mutex
	// until 熔断截止；零值或已过 = 不熔断。
	until time.Time
	// window 当前窗口长度（连续命中会翻倍，成功后复位）。
	window time.Duration
	// now 时钟注入点；生产用 time.Now，单测注入假时钟以断言窗口行为。
	now func() time.Time
	// hits 累计命中次数（仅供诊断/文案，不影响判定）。
	hits int
}

// unusualBreakers 按产品隔离 3012 熔断：zcode / qoder / workbuddy。
//
// 空产品归一为 workbuddy，兼容历史 WorkBuddy 凭证。
type unusualBreakers struct {
	mu        sync.Mutex
	byProduct map[string]*unusualBreaker
}

func newUnusualBreakers() *unusualBreakers {
	return &unusualBreakers{byProduct: map[string]*unusualBreaker{}}
}

func (r *unusualBreakers) forProduct(product string) *unusualBreaker {
	if product == "" {
		product = "workbuddy"
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	if b := r.byProduct[product]; b != nil {
		return b
	}
	b := newUnusualBreaker()
	r.byProduct[product] = b
	return b
}

func (r *unusualBreakers) reset() {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.byProduct = map[string]*unusualBreaker{}
}

// 以下兼容旧测试/调用约定：未指定产品时按 WorkBuddy 处理。
func (r *unusualBreakers) tripped() (bool, time.Duration) { return r.forProduct("workbuddy").tripped() }
func (r *unusualBreakers) note3012() time.Time            { return r.forProduct("workbuddy").note3012() }
func (r *unusualBreakers) noteSuccess()                   { r.forProduct("workbuddy").noteSuccess() }

var unusual = newUnusualBreakers()

func newUnusualBreaker() *unusualBreaker {
	return &unusualBreaker{window: unusualWindow, now: time.Now}
}

// tripped 报告当前是否处于 3012 熔断中，以及还需多久。
//
// 调用方（forward.go 的轮转循环）在**发起任何上游请求之前**先问它：
// 熔断期间直接失败，不再换号 —— 这正是本模块要达成的效果。
func (b *unusualBreaker) tripped() (bool, time.Duration) {
	now := b.now()
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.until.IsZero() || !now.Before(b.until) {
		return false, 0
	}
	return true, b.until.Sub(now)
}

// note3012 记录一次 3012，并开启（或延长）熔断窗口。
//
// 返回本次熔断的截止时刻，供调用方写日志。
//
// # 为什么命中就"延长"而不是"忽略重复"
//
// 与 wafip.go 的 noteWaf 不同：那个判据是"不同账号数"，同一账号重复命中
// 不该增加计数（那是账号问题）。而 3012 的判据是"上游正在风控"——
// 每一次命中都说明**此刻仍在风控中**，故重复命中应当把窗口往后推
// （并以翻倍方式退让，见 unusualCooldownAfterBreaker）。
func (b *unusualBreaker) note3012() time.Time {
	now := b.now()
	b.mu.Lock()
	defer b.mu.Unlock()
	b.hits++
	// 连续命中（上次熔断还没结束就又中）⇒ 窗口翻倍，逐步退让。
	if !b.until.IsZero() && now.Before(b.until) {
		b.window *= 2
		if b.window > unusualMaxWindow {
			b.window = unusualMaxWindow
		}
	} else if b.window != unusualWindow {
		// 上次熔断**已过期**后才再次命中：这是"新一轮风控"，
		// 窗口从基准重新开始 —— 否则一次长风控会把窗口永久顶在上限，
		// 之后每次瞬时抖动都要用户等 30 分钟。
		b.window = unusualWindow
	}
	b.until = now.Add(b.window)
	return b.until
}

// noteSuccess 一次上游成功即**清除**熔断。
//
// 为什么成功要立刻复位：它证明上游此刻不再风控。继续熔断会让用户
// 在一次短暂抖动之后仍然被自己的网关挡在门外 —— 那是我们制造的故障。
func (b *unusualBreaker) noteSuccess() {
	b.mu.Lock()
	defer b.mu.Unlock()
	b.until = time.Time{}
	b.window = unusualWindow
	b.hits = 0
}

// reset 清空状态（仅供单测隔离用例之间）。
func (b *unusualBreaker) reset() {
	b.mu.Lock()
	defer b.mu.Unlock()
	b.until = time.Time{}
	b.window = unusualWindow
	b.hits = 0
}

// unusualActivityMarkers 3012 响应体的识别特征（小写比较）。
//
// 上游原文（实测）：
//
//	HTTP 405 {"code":3012,"msg":"request has been blocked due to unusual activity."}
//
// 认**业务码 + 文案**两重，任一命中即可：文案可能被上游改写，
// 而 code 是稳定的；反过来某些通道可能只回文案不带 code。
var unusualActivityMarkers = []string{
	"unusual activity",
	"request has been blocked due to unusual activity",
}

// isUnusualActivity 报告一次响应是否为 3012 风控。
//
// 判据（任一成立）：
//
//  1. 响应体里出现独立的业务码 3012；
//  2. 响应体含 "unusual activity" 文案。
//
// ⚠ 为什么还要看状态码：3012 的载体实测是 **405**（不是 4xx 里常见的
// 403/429），而 405 在网关里还有别的可能来源（方法不允许）。故要求
// 状态码是 4xx 或 5xx —— 一个 HTTP 200 的正常回复里恰好含这些词
// （比如用户在聊风控）不该触发熔断。
func isUnusualActivity(status int, body string) bool {
	if status < 400 {
		return false
	}
	if hasBareCode(body, "3012") {
		return true
	}
	lower := strings.ToLower(body)
	for _, m := range unusualActivityMarkers {
		if strings.Contains(lower, m) {
			return true
		}
	}
	return false
}

// hasBareCode 报告响应体里的业务码**恰好等于** code。
//
// # ⚠ 这里必须解析 JSON，不能做子串匹配（我自己写的测试抓出了两个 bug）
//
// 第一版是 `strings.Contains(body, `"code":`+code)` 加两种引号变体。测试立刻红了：
//
//	带空格   `{"code": 3012, ...}`   → **漏判**（上游美化输出就读不出来）
//	别的码   `{"code":30120, ...}`   → **误判**（`"code":3012` 是 `"code":30120`
//	                                   的前缀！）
//
// 误判更糟：它会因为一个**无关的业务码**把整个网关熔断 90 秒，
// 表现为"偶发地一大段时间全都不可用"，而用户完全看不出与 30120 有关。
//
// 改成解析 JSON 后两种情况都自然正确 —— 而且不必再枚举引号/空格变体，
// 那本身就是"用字符串凑 JSON"的征兆。
//
// 兜底：JSON 解析失败时（上游偶尔回非标准体）退回**带引号的精确匹配**
// （`"code":"3012"` 或 `"code":3012` 后必须跟非数字字符），
// 但绝不用裸子串 —— 那正是 30120 误判的来源。
func hasBareCode(body, code string) bool {
	t := strings.TrimSpace(body)
	if t == "" {
		return false
	}
	// 主路径：正经 JSON。
	var doc map[string]json.RawMessage
	if err := json.Unmarshal([]byte(t), &doc); err == nil {
		if raw, ok := doc["code"]; ok {
			return rawCodeEquals(raw, code)
		}
		return false
	}
	// 兜底：非 JSON 体。用带边界的匹配，避免 `30120` 命中 `3012`。
	for _, shape := range []string{
		`"code":` + code, // 下面还要确认后一个字符不是数字
		`"code": "` + code + `"`,
	} {
		i := strings.Index(t, shape)
		if i < 0 {
			continue
		}
		if strings.HasSuffix(shape, `"`) {
			return true // 已是完整的带引号形式
		}
		// 裸数字形式：后一个字符不能是数字（否则 30120 会被当成 3012）
		j := i + len(shape)
		if j >= len(t) {
			return true
		}
		c := t[j]
		if c < '0' || c > '9' {
			return true
		}
	}
	return false
}

// rawCodeEquals 判断 JSON 里的 code 字段是否等于期望值（数字或字符串两种形态）。
func rawCodeEquals(raw json.RawMessage, want string) bool {
	s := strings.TrimSpace(string(raw))
	s = strings.Trim(s, `"`) // 字符串形态：去掉引号
	return s == want
}

// unusualActivityMessage 生成 3012 熔断期间给客户端的文案。
//
// 必须说清三件事（缺一件用户就会走错方向）：
//
//	现象 —— 上游回了 3012「unusual activity」
//	结论 —— 这是上游风控，**与账号无关**（所以换号无用）
//	出路 —— 等窗口过去，或换模型；网关已停止重试以免加重风控
//
// ⚠ 刻意不说"账号被封"：实测同一账号一分钟后就成功，且官方客户端也吃它。
// 说成封号会把用户引向"去换账号"这个**完全无效**的方向
// （历史上正是这么误导过所有者）。
func unusualActivityMessage(wait time.Duration) string {
	secs := int(wait.Round(time.Second) / time.Second)
	if secs < 1 {
		secs = 1
	}
	return "上游风控（3012 unusual activity）：这次拒绝是**上游对本次请求**的判定，" +
		"与账号无关 —— 实测同一账号一分钟后即可用，且官方客户端也会被同样拒绝。" +
		"网关已**停止换号重试**（继续试只会加重风控），约 " +
		strconv.Itoa(secs) + " 秒后自动恢复。可换其它模型先用，或稍后重试。"
}
