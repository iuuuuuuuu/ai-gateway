// Package pool 账号池：单一状态机（健康/冷却/熔断）+ 在途租约 + 三因子加权挑选 + state.json 持久化。
//
// 每个账号只有三个正交状态维度：
//  1. 健康维度（唯一权威）：healthy = !disabled && !until 生效 && !breakerUntil 生效
//     - until：按错误类型的即时冷却（CoolSoft 429 / CoolHard 余额耗尽）
//     - breakerUntil：连续失败（fails）累计触发熔断的指数退避截止
//  2. 并发维度：inFlight（在途租约，运行态）
//  3. 统计维度：successCount / errTotal（累计，供成功率权重）/ lastUsed / lastSuccess / lastErr
//
// 挑选策略（两级）：
//  1. 到期分层：先按「最近到期积分」的到期日把 healthy 候选分组，只保留最早到期的一档 ——
//     优先消耗快过期的额度，避免积分作废；到期日未知的账号排最后。
//  2. 档内挑选：同档账号按「闲置补偿 + 成功率」加权取 Top5，再加权随机抽签，
//     即同一天到期的账号平均分摊。
//
// 短名单（Top5）的公平性保证（见 shuffleTiesWs）：当截断边界上存在等权重平局时
// 随机打散，避免同权重账号因 UID 字典序而固定霸占短名单、其余账号永远拿不到流量
//（issue #5：14 个账号只被路由到 5 个）。
//
// 当所有账号都没有到期信息时，自动退回原三因子口径
//（credits 占比 ×10 + 闲置补偿 + 成功率 ×3），行为与引入分层前一致。
package pool

import (
	"context"
	"encoding/json"
	"log"
	"math/rand/v2"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"workbuddy2api/internal/auth"
)

// CoolKind 冷却类型。
type CoolKind int

const (
	CoolHard CoolKind = iota // 余额不足 → 冷却到次日 04:00（等签到恢复）
	CoolSoft                 // 429 → 短冷却
)

func (k CoolKind) String() string {
	switch k {
	case CoolHard:
		return "hard_credit"
	case CoolSoft:
		return "soft_rate"
	}
	return "unknown"
}

// Status 单个账号对外暴露的状态（脱敏）。
type Status struct {
	UID             string    `json:"uid"`
	Nickname        string    `json:"nickname,omitempty"`
	Credits         int64     `json:"credits"`
	Cooling         bool      `json:"cooling"`
	CoolKind        string    `json:"cool_kind,omitempty"`
	CoolRemaining   int64     `json:"cool_remaining_sec,omitempty"`
	Until           time.Time `json:"until,omitempty"`
	Reason          string    `json:"reason,omitempty"`
	Disabled        bool      `json:"disabled"`
	// NoRoute 用户手动禁用：**只不接流量**，养号任务照跑。
	//
	// 与 Disabled 分开下发，界面才能如实区分两种「不接流量」：
	//   NoRoute  → 用户自己关的；签到 / 上报 / 成长任务**仍在跑**
	//   Disabled → 网关判定该号已死（session 死 / 额度冻结），任务也会跳过
	// 两者在界面上若都写成「禁用」，用户就无法判断「这个号还在不在养」——
	// 而「禁用了但仍在养号」正是本功能的语义。
	NoRoute         bool      `json:"no_route,omitempty"`

	// Product 该账号属于**哪个客户端产品**（`workbuddy` / `qoder` / `zcode`）。
	//
	// # 为什么必须下发（所有者 2026-09-20 要求）
	//
	// 原话：「在兼容网关哪里的账号池,也要标记上进入池子的账号属于那个客户端」。
	//
	// 三个产品的账号混在**同一个池**里（多产品路由开启时），而界面上
	// 只有昵称/备注 —— 用户看到 `wish`、`aliyun-…` 这样的名字，
	// **不知道它来自哪个客户端**。于是：
	//
	//	· 排查"为什么 zcode: 前缀选不出号"时，看不出池里到底有几个 ZCode 号
	//	· 想给某个号单独停流量时，得先去别的页面确认它是哪家的
	//
	// ⚠ 值走 `ProductOf()`（**空串归一成 workbuddy**），不是裸 `e.a.Product`：
	// 老账号的 Product 是空串，若原样下发，界面会显示空、或前端各自
	// 拿空串去猜 —— 那正是"同一个号在不同页面显示不同产品"的成因。
	Product string `json:"product,omitempty"`

	SuccessCount    int64     `json:"success_count,omitempty"`
	ErrTotal        int64     `json:"err_total,omitempty"`
	LastSuccessTime time.Time `json:"last_success,omitempty"`
	LastErrTime     time.Time `json:"last_err,omitempty"`

	// 运行态（不持久化）：在途请求数 + 熔断器状态。
	InFlight     int       `json:"in_flight"`
	BreakerFails int       `json:"breaker_fails"`
	BreakerUntil time.Time `json:"breaker_until,omitempty"`

	// SoonestExpireAt 「最近到期积分」的到期时刻（Unix 秒）；0/缺省 = 未知。
	// ExpireDay 是其本地日期（YYYY-MM-DD），即选号分层用的档位键；
	// 同一 ExpireDay 的账号在均衡时被视为同一优先级。
	SoonestExpireAt int64  `json:"soonest_expire_at,omitempty"`
	ExpireDay       string `json:"expire_day,omitempty"`

	// Queued 该账号是否正因「到期档位更晚」而排队等待（当前轮不到它）。
	//
	// 由后端按与选号**完全相同**的档位口径算出，前端直接展示即可 ——
	// 前端无法自行判断：它既拿不到 healthy/模型冷却/在途 这三套判定，
	// 也没有「当前生效档位」这个池级信息。用 success_count==0 之类近似会误判
	//（新导入或持续失败的账号同样是 0，但它们不是排队）。
	//
	// 语义：只表示「按分层规则现在轮不到」，不代表故障。前面的档位被消耗或冷却后，
	// 它会自动进入路由。
	Queued bool `json:"queued,omitempty"`

	// ModelCooling 该账号当前因「模型级限流」而冷却的模型列表（按到期时间升序）。
	//
	// 与 CoolKind 的区别（前端据此区分两种冷却）：
	//   - CoolKind=hard_credit / cooling=true → 账号级：余额（积分）欠费，整号不可用
	//   - ModelCooling 非空                   → 模型级：仅这些模型不可用，换模型仍可用
	// 两者可同时存在（例如余额充足但某模型额度用尽）。
	ModelCooling []ModelCooling `json:"model_cooling,omitempty"`
}

type entry struct {
	a            *auth.Auth
	credits      int64
	successCount int64     // 累计成功
	errTotal     int64     // 累计错误（供成功率权重 successRate = successCount/(successCount+errTotal)，不清零）
	lastErr      time.Time // 最近一次错误时间
	lastSuccess  time.Time // 最近一次成功时间
	coolKind     CoolKind
	until        time.Time // 冷却截止（即时冷却：CoolSoft 429 / CoolHard 余额耗尽）
	disabled     bool
	reason       string
	lastUsed     time.Time // 最近被选中时刻（防并发撞号）
	// lastUsedSeq 单调序号：每次被选中时自增，用于打破 lastUsed 的时钟粒度平局。
	//
	// 必要性（实测）：Windows 上 time.Now() 的分辨率约 511µs（30 万次采样仅 3 个
	// 不同值），并发 Pick 会给多个账号写入**完全相同**的 lastUsed。LRU 兜底用
	// `c.lastUsed.Before(e.lastUsed)` 比较，平局时恒为 false，于是永远停留在
	// cands[0] —— 100 并发下实测 91/100 全部撞同一个账号（惊群）。
	// 序号由 Pool 在持锁下自增，因此「平局时先选中者优先」= 严格的 LRU 顺序。
	lastUsedSeq int64
	// sessionDeadFails 连续 ErrSessionDead（12153）计数，达到阈值才禁用。
	//
	// 为什么需要它：12153 会被**临时性**触发（网络抖动 / 上游闪断 / refresh 竞态），
	// 旧行为一次即 Disable，把健康账号永久杀掉 —— 实测发现一批 disabled 账号
	// 其实 refresh 完全正常，是历史误判的受害者。
	// 改为「连续 N 次才禁用」后，偶发失败不会杀号；
	// 任何证明账号未死的时刻（refresh 成功 / chat 成功 / 手工复活）都清零。
	sessionDeadFails int

	// breakerUntil / fails / retryCount 为熔断器运行态（不持久化）。
	// fails 是唯一的"连续失败"计数器：任何错误喂入，达到 breakerThreshold 触发熔断（指数退避），
	// 跨入口累计，成功/熔断/统一复活时清零（保留 retryCount 驱动退避指数）。
	breakerUntil time.Time // 熔断截止（指数退避）
	fails        int       // 连续失败计数（熔断用，唯一权威）
	retryCount   int       // 已熔断次数（指数退避的指数）

	// inFlight 单账号在途请求数（运行态，不持久化）。用 atomic 避免 Pick 热路径拿写锁。
	inFlight atomic.Int64

	// expireAt 该账号「最近到期积分」的到期时刻（Unix 秒）；0 = 未知。
	//
	// 运行期权威值，来源有二，按新鲜度覆盖：
	//  1. 凭证文件里的 credit 块（宿主 workbuddy-switch 查询后写入，见 upsertLocked）
	//  2. 网关自身的积分巡检（见 CreditRefresher）
	// 持久化进 state.json，重启后无需等首轮巡检即可继续按到期分层选号。
	expireAt int64

	// modelCools 该账号**按模型**的冷却表（模型名 → 冷却记录）。
	//
	// 与 until 正交：until 是账号级冷却（余额不足 / 账号被限速），一旦生效该账号
	// 所有模型都不可用；modelCools 只封单个模型 —— 上游 6004 明确提示
	// 「您也可以切换其他模型继续使用」，故不能按账号整体冷却。
	//
	// 持久化进 state.json：模型限流的重置时间常达数小时（实测 4.6~6.7h），
	// 而网关会因切换工作模式、应用重启等原因重启；不持久化的话重启即遗忘，
	// 立刻重新撞同一批 6004，用户看到的仍是「频繁不可用」。
	modelCools map[string]modelCool

	// costRate 该账号当前请求模型的「单位额度消耗率」；0 = 未声明。
	//
	// 跨产品可比性的载体：两产品的额度单位不同（WorkBuddy 积分 ≠ Qoder 积分），
	// 绝对值不可比，但"这个模型在这份额度上消耗得多快"是可比的。
	//
	// 语义（三态，与全仓库口径一致）：
	//	> 0  已声明：值越大越贵，权重越小
	//	= 0  **未声明** → 中性（不猜测、不惩罚）
	//
	// ⚠ 为什么不复用 credits：现有 tierWeightOf **刻意不含 credits**
	//（pool.go 的注释说明了原因：同档内按积分分配会让高积分账号长期吃掉流量）。
	// 所以"按成本路由"必须另立维度，塞回 credits 是无效的 —— 只要有任何账号
	// 带到期信息就会走 tiered 分支，credits 会被整个丢掉。
	//
	// 运行态，不持久化：它取决于**当前请求的模型**，跨请求无意义。
	costRate float64
}

// expiryDayKey 返回账号「最近到期积分」的到期日（本地时区，YYYY-MM-DD）。
//
// 空串表示未知（尚无到期数据）。
// 用「日」而非精确时刻做分层键：上游额度按天失效，同一天到期的账号应视为同一档。
func (e *entry) expiryDayKey() string {
	if e.expireAt <= 0 {
		return ""
	}
	return time.Unix(e.expireAt, 0).In(time.Local).Format("2006-01-02")
}

// productOf 取账号所属产品，**nil 安全**且把空串归一成 workbuddy。
//
// 为什么要两层兜底而不是直接 `e.a.ProductOf()`：
//   · `e.a` 可能是 nil（池里曾经有过 nil 账号的路径），直接调会 panic
//   · 空串归一由 `auth.ProductOf()` 负责，这里只是复用它的口径 ——
//     **不要**在这里另写一份判断，否则两条路径迟早分叉
//      （界面显示 workbuddy、选号却按别的产品过滤，那是极难查的一类缺陷）
func productOf(a *auth.Auth) string {
	if a == nil {
		return ""
	}
	return a.ProductOf()
}

// healthy 报告账号当前是否可选（未禁用、未处于任一冷却/熔断期、未被用户标记不接流量）。
func (e *entry) healthy(now time.Time) bool {
	if e.disabled {
		return false
	}
	// 用户手动禁用 = **只不接流量**，养号任务照跑。
	//
	// ⚠ 本函数**不是**所有选号路径的统一闸门 —— 这里曾经写着「pickLocked /
	// pickEarliestExpiryLocked / routedTierDayLocked 都靠它筛候选，一处即覆盖
	// 全部分流场景」，但那是**错的**：pickEarliestExpiryLocked 因为要选
	// 「冷却中的账号」（本函数会把它们判掉），自己手写了一遍筛选，**不经过这里**。
	// 结果是它的 NoRoute 检查缺失，被标「不接流量」的账号在兜底时仍会被选中
	//（现场症状：用户禁用了国际版账号，国服冷却后请求仍打到连不通的国际版 → 503）。
	//
	// 教训：**「统一闸门」是承诺，不是事实** —— 加检查时要逐个调用点核对，
	// 不能凭这里的注释认为已经覆盖。现已补上该检查，并有回归测试
	//（fallback_noroute_test.go::TestSelectionPathsAgreeOnNoRoute）防止再次分叉。
	//
	// 而养号任务**不走**本函数：它们只判 `st.Disabled`（网关自判定的死号：
	// session 死 / 额度冻结，那种跑了也白跑）。于是「禁用 = 不接流量、但照常养号」
	// 这个语义自然成立 —— 这正是所有者要的。
	//
	// 注意 NoRoute 账号的凭证**是存在**的（宿主照常导出），所以任务能遍历到它。
	// 若哪天有人把导出一并去掉，任务会再次静默停跑 —— 那正是本次修的 bug。
	if e.a != nil && e.a.NoRoute {
		return false
	}
	if !e.until.IsZero() && now.Before(e.until) {
		return false
	}
	if !e.breakerUntil.IsZero() && now.Before(e.breakerUntil) {
		return false
	}
	return true
}

// expiry 返回账号当前仍在生效的最近冷却/熔断截止时间（两个截止取较早者）；不在冷却期返回零值。
// 供全冷却兜底选取"最早到期"账号用。
func (e *entry) expiry(now time.Time) time.Time {
	var t time.Time
	if !e.until.IsZero() && now.Before(e.until) {
		t = e.until
	}
	if !e.breakerUntil.IsZero() && now.Before(e.breakerUntil) {
		if t.IsZero() || e.breakerUntil.Before(t) {
			t = e.breakerUntil
		}
	}
	return t
}

// fallbackKind 报告兜底账号属于哪一类冷却（soft：即时软冷却；breaker：熔断期）。
// 只对参与兜底的账号调用（CoolHard 已被 pickEarliestExpiryLocked 排除）。判定口径：
// 若熔断截止是当前生效的最近截止（含"仅有熔断无软冷却"），记为 breaker；否则记为 soft。
func (e *entry) fallbackKind(now time.Time) string {
	if !e.breakerUntil.IsZero() && now.Before(e.breakerUntil) {
		if e.until.IsZero() || !now.Before(e.until) || e.breakerUntil.Before(e.until) {
			return "breaker"
		}
	}
	return "soft"
}

// stateAccount 单个账号的持久化状态（JSON tag 全小写下划线，向后兼容：缺字段零值）。
type stateAccount struct {
	Credits      int64     `json:"credits"`
	Disabled     bool      `json:"disabled"`
	Reason       string    `json:"reason,omitempty"`
	Until        time.Time `json:"until,omitempty"`
	CoolKind     CoolKind  `json:"cool_kind"`
	SuccessCount int64     `json:"success_count,omitempty"`
	// err_total 累计错误计数。旧版 err_count（连续错误）仍可读：加载时映射到 err_total，
	// 仅作一次性迁移，不再回写 err_count。
	ErrTotal    int64     `json:"err_total,omitempty"`
	ErrCount    int       `json:"err_count,omitempty"` // 兼容旧文件的迁移源，仅读取
	LastSuccess time.Time `json:"last_success,omitempty"`
	LastErr     time.Time `json:"last_err,omitempty"`
	// ExpireAt 「最近到期积分」的到期时刻（Unix 秒）；0/缺省 = 未知。
	// 持久化它可让重启后立刻恢复到期分层，无需等首轮积分巡检。
	ExpireAt int64 `json:"expire_at,omitempty"`
	// ModelCools 按模型的冷却表（模型名 → 记录）。旧文件缺该字段时零值（无模型冷却），
	// 向后兼容。持久化的必要性见 entry.modelCools 的注释。
	ModelCools map[string]stateModelCool `json:"model_cools,omitempty"`
}

// stateModelCool 单条模型冷却的持久化形态。
type stateModelCool struct {
	Until       time.Time `json:"until"`
	Reason      string    `json:"reason,omitempty"`
	ResetParsed bool      `json:"reset_at_parsed,omitempty"`
}

// stateFile 持久化格式。
type stateFile struct {
	Accounts map[string]stateAccount `json:"accounts"`
}

// flushInterval 后台落盘周期。
var flushInterval = 5 * time.Second

// persistLogEvery 连续落盘失败每 N 次打一条提醒（flusher 5s 一把 ≈ 1 分钟一次），
// 避免磁盘持续满/权限丢失时日志刷屏。
const persistLogEvery = 12

// snapshot 池状态快照（Redis 镜像用）。与本地 state.json 同源（stateFile），
// 额外带 savedAt 时间戳供"择新恢复"（比较本地与 Redis 快照的新旧）。
type snapshot struct {
	stateFile
	SavedAt time.Time `json:"saved_at"`
}

// Pool 账号池。
type Pool struct {
	mu      sync.RWMutex
	byUID   map[string]*entry
	stateFp string
	dirty   atomic.Bool // 内存有变更待落盘

	// pickSeq 选号单调序号（仅由持写锁的 pick 路径自增）。
	// 与 entry.lastUsedSeq 配合，为 lastUsed 提供亚时钟粒度的先后次序，
	// 使 LRU 兜底在 lastUsed 完全相同时仍能选出真正最久未用者（见 entry.lastUsedSeq）。
	pickSeq int64

	// store 池状态快照镜像（redisstore.Store）；nil = 无需镜像（未配置 Redis / Noop 之外也可能 nil）。
	// SaveState/LoadState 经它接线，与本地 state.json 并存作启动恢复备份。
	store StoreSnapshotter

	// 熔断器调优（SetBreaker 注入；默认值见 defaultBreaker*）。
	breakerThreshold   int
	breakerCooldown    time.Duration
	breakerCooldownMax time.Duration

	// 三因子加权调优（SetWeights 注入；默认值见 defaultIdle*）。
	idleWeightPerHour float64
	idleWeightMax     float64

	// maxInFlight 单账号最大在途请求数；0 = 不限（租约关闭）。
	maxInFlight int

	// rotationOn 是否启用「单一模型 + 积分轮转」模式（见 SetRotation）。
	//
	// 不持久化：它由网关启动时从配置读取，属于部署配置而非运行态；
	// 落盘反而会让「配置改成负载均衡后重启」被旧状态覆盖。
	rotationOn bool

	// modelPlatforms 「模型 → 允许的平台」白名单（见 SetModelPlatforms）。
	//
	// 空 map = 不限制（保持既有行为）。同样不持久化：它是**部署配置**，
	// 由网关启动时从 native config 读入；落盘会让"改配置后重启"被旧值覆盖。
	modelPlatforms map[string][]string

	// rotationUID 轮转模式下当前正在烧的那个账号（空串 = 尚未选定）。
	//
	// 这是轮转模式的全部状态：只要它仍可用就继续用，不可用才换下一个。
	// 不持久化：重启后重新按到期日挑一个即可，语义上无损失（都是"挑最早的"）。
	rotationUID string

	// randInt64N 仅供测试注入确定性随机源；nil 时用 math/rand/v2 全局源。
	// 生产代码不应设置此字段。
	randInt64N func(n int64) int64

	// persistFails 本地 state.json 连续落盘失败计数（仅 saveLocked 在持锁下读写，无需 atomic）。
	// 用于落盘失败的日志节流：首败/每 N 次提醒/恢复各打一条，避免磁盘满时刷屏。
	persistFails int

	// ---- 多产品路由（默认关闭，关闭时行为与单产品逐字相同）----
	//
	// 为什么做成开关：跨产品权重是最容易"悄悄改变现有行为"的地方，
	// 而现有行为（单产品 WorkBuddy）已在生产环境跑了很久、有真实用户依赖。
	// 开关关闭时：不读成本字段、不做产品区分，所有既有路径**逐字不变**。
	multiProductOn bool

	// productModelSet 各产品**实际提供**的模型（小写归一），由宿主透传。
	//
	// # 为什么必须有它（2026-09-20 实测缺陷）
	//
	// 用户用 `deepseek-v4.1-flash`（一个 **WorkBuddy** 的模型）发了请求，
	// 却被路由到 **Qoder 账号**上，失败两次 —— 因为无前缀模型名时
	// `PickForModelRegion` 只按"账号当前可用"挑，**不问"这个产品有没有这个模型"**。
	// Qoder 账号当时恰好可用就被选中，而它根本没有这个模型。
	//
	// 症状对用户极难理解：模型名是 WorkBuddy 的，报错却来自 Qoder。
	//
	// 故维护"产品 → 模型集合"，在**无前缀**时用它排除
	// "不提供该模型的产品"的账号。有前缀时本来就有产品约束，不受影响。
	//
	// ⚠ 空 map / 某产品不在 map 里 = **不约束该产品**（保持既有行为）：
	// 宿主没透传时不能因为"不知道"就把账号排掉 —— 那会让所有人不可用。
	productModelSet map[string]map[string]bool

	// productModelFallbackSet 网关**自己补的兜底清单**（同样小写归一）。
	//
	// # 与 productModelSet 的分工（2026-09-22，这是本轮修复的核心）
	//
	//	productModelSet          宿主给的清单 —— 来自上游**真实查询**，可信
	//	productModelFallbackSet  网关补的清单 —— 来自内置静态表，是**猜测**
	//
	// 前者用于**两个**判断（"不排除"与"声明"），后者**只用于"不排除"**。
	//
	// # 为什么必须分开（所有者现场）
	//
	//	发 `GLM-5.3`        → 400 model_not_in_region
	//	发 `Auto`           → 400 model_not_in_region
	//	发 `Qwen3.8-Flash`  → **200 正常**
	//
	// 差别在**重叠**：`Qwen3.8-Flash` 只有 qoder 声明（路由唯一）；
	// 而 `GLM-5.3` / `Auto` 同时被 workbuddy（内置静态表里有 `glm-5.3` /
	// `auto`）与 qoder 声明。
	//
	// `pickForModelAny` 优先只在"明确声明"的产品里挑 ⇒ 两个产品都进
	// `declared` ⇒ 一起竞争 ⇒ 池里 **19 个 WorkBuddy 账号 vs 1 个 qoder**
	// ⇒ 大概率选到 WorkBuddy，而它其实**没有** `GLM-5.3`
	//（静态表是网关的猜测，不是上游的真实能力）⇒ 11102。
	//
	// 修法：兜底清单不参与"声明"竞争，只保证"不排除"。
	// 即"我不知道 WorkBuddy 提供什么，所以别排除它；但也别声称它提供"。
	//
	// ⚠ 旧注释曾写「多列几个不会让任何账号失去资格，所以宁可补全」——
	// 那句话在引入 `declared` 优先逻辑之后就**失效了**，见上。
	productModelFallbackSet map[string]map[string]bool

	// costWeight 成本乘子的强度（0 = 成本不参与，1 = 满强度）。
	//
	// 设计意图（见 design.md §2.3）：成本是**小幅微调**，不是主导项。
	// 主导项仍是"同档内平均分摊"——那关系到一旦过期就净损失的额度。
	// 具体量级由单测断言份额比例来校准，不硬编码魔数。
	costWeight float64
}

// defaultBreaker* 熔断器默认参数（FreeBuff2API 参考口径）。
const (
	defaultBreakerThreshold   = 3
	defaultBreakerCooldown    = 30 * time.Minute
	defaultBreakerCooldownMax = 6 * time.Hour
)

// StoreSnapshotter 池状态快照镜像的最小接口（redisstore.Store 满足；Noop 空实现安全）。
// 与本地 state.json 并存，作启动恢复备份：快照比本地新才采用，否则本地优先。
type StoreSnapshotter interface {
	SaveState(data []byte)
	LoadState() ([]byte, bool)
}

// defaultIdle* 闲置补偿默认参数（claude-api selectWeightedRandom 参考口径）。
const (
	defaultIdleWeightPerHour = 0.5
	defaultIdleWeightMax     = 5.0
)

// shortlistSize top-N 短名单大小。
//
// 短名单的用途是「让持续报错/额度低的号自然让出流量」，但 N 一旦小于账号总数，
// 就必然有账号被长期排除在外。同权重时靠随机洗牌轮换（见 shuffleTiesWs），
// 保证被排除的账号随时间轮转，不会固定饿死某几个号。
const shortlistSize = 5

// New 构建池；stateFp 非空时尝试加载旧状态，并启动后台周期性落盘 goroutine。
func New(stateFp string) *Pool {
	p := &Pool{
		byUID:              map[string]*entry{},
		stateFp:            stateFp,
		breakerThreshold:   defaultBreakerThreshold,
		breakerCooldown:    defaultBreakerCooldown,
		breakerCooldownMax: defaultBreakerCooldownMax,
		idleWeightPerHour:  defaultIdleWeightPerHour,
		idleWeightMax:      defaultIdleWeightMax,
	}
	if stateFp != "" {
		p.load()
		p.startFlusher()
	}
	return p
}

// SetBreaker 注入熔断器参数（main 从 config 解析后调用）。非正值保留原值（用默认）。
func (p *Pool) SetBreaker(threshold int, cooldown, cooldownMax time.Duration) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if threshold > 0 {
		p.breakerThreshold = threshold
	}
	if cooldown > 0 {
		p.breakerCooldown = cooldown
	}
	if cooldownMax > 0 {
		p.breakerCooldownMax = cooldownMax
	}
}

// SetWeights 注入三因子加权的闲置补偿参数。非正值保留原值（用默认）。
func (p *Pool) SetWeights(idlePerHour, idleMax float64) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if idlePerHour > 0 {
		p.idleWeightPerHour = idlePerHour
	}
	if idleMax > 0 {
		p.idleWeightMax = idleMax
	}
}

// SetMaxInFlight 注入单账号最大在途请求数；0 = 不限。负值保留原值。
func (p *Pool) SetMaxInFlight(n int) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if n >= 0 {
		p.maxInFlight = n
	}
}

// SetMultiProduct 开关多产品路由，并注入成本乘子强度。
//
// 关闭时（默认）所有既有路径**逐字不变**：不读 costRate、不加成本乘子。
// 这是 design.md §5 要求的回滚点 —— 产品维度出问题就关掉它，
// 立刻回到已验证的单产品行为，而不需要回滚代码。
//
// costWeight <= 0 时保留默认强度（0.3）。传 0 想表达"成本不参与"时，
// 应关闭开关而不是设 0 —— 两者语义不同：关闭=完全没有产品维度；
// 设 0=有产品维度但成本中性（仍可用于产品级统计与派发）。
func (p *Pool) SetMultiProduct(on bool, costWeight float64) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.multiProductOn = on
	if costWeight > 0 {
		p.costWeight = costWeight
	}
}

// ProductRegions 报告某产品的账号覆及哪些区域（去重、顺序稳定）。
//
// # 用途
//
// `/v1/models` 要生成 `平台:区域:模型名` 形式的组合名，而**只能给该产品
// 确实有账号的区域**生成 —— 无脑给所有区域会造出 "qoder:国服:xxx" 这类
// 没有账号可用的名字，用户选中后得到"账号不可用"，比不显示更糟。
//
// 为什么不复用 `List()`：它返回的 Status **不含 Region**，而为了这一个用途
// 去拓宽 Status 会让它对所有调用方都多一个字段。单独一个窄方法更清楚。
//
// `RegionAny` 不返回（它表示"不限区域"，不是一个可写进前缀的区域名）。
func (p *Pool) ProductRegions(product string) []auth.Region {
	p.mu.RLock()
	defer p.mu.RUnlock()
	seen := map[auth.Region]bool{}
	var out []auth.Region
	for _, e := range p.byUID {
		if e == nil || e.a == nil {
			continue
		}
		if e.a.ProductOf() != product {
			continue
		}
		r := e.a.Region()
		if r == auth.RegionAny || seen[r] {
			continue
		}
		seen[r] = true
		out = append(out, r)
	}
	// 稳定顺序（map 遍历无序会让每次返回的组合名顺序不同，
	// 客户端菜单跟着跳；也让测试无法断言）
	sort.Slice(out, func(i, j int) bool { return out[i] < out[j] })
	return out
}

// MultiProductOn 报告多产品路由是否开启（供诊断与测试断言）。
func (p *Pool) MultiProductOn() bool {
	p.mu.RLock()
	defer p.mu.RUnlock()
	return p.multiProductOn
}

// SetProductModels 注入"各产品实际提供哪些模型"（宿主透传）。
//
// 为什么需要它：无前缀的模型名（如 `deepseek-v4.1-flash`）此前会被路由到
// **任何**当前可用的账号上 —— 包括根本没有这个模型的产品（实测被路由到
// Qoder 并失败两次）。见 productModelSet 字段的注释。
//
// 入参形状：`{"workbuddy": ["deepseek-v4.1-flash", …], "qoder": ["qwen3.8-flash", …]}`。
// 模型名**大小写不敏感**（上游写法不一，比较时统一小写）。
//
// 传空 map 等于"不约束"（回到既有行为）。
func (p *Pool) SetProductModels(byProduct map[string][]string) {
	set := make(map[string]map[string]bool, len(byProduct))
	for prod, models := range byProduct {
		if prod == "" {
			continue
		}
		m := make(map[string]bool, len(models))
		for _, id := range models {
			if id = strings.ToLower(strings.TrimSpace(id)); id != "" {
				m[id] = true
			}
		}
		// ⚠ 只有**非空**集合才登记：某产品清单为空时登记成空集合会让
		// "它不提供任何模型"成立 ⇒ 它的账号全被排除。而那多半是
		// "宿主还没拿到清单"，不是"它真的没有模型"。
		if len(m) > 0 {
			set[prod] = m
		}
	}
	p.mu.Lock()
	p.productModelSet = set
	p.mu.Unlock()
}

// SetProductModelsFallback 设置网关**自己补的**兜底清单（只影响"不排除"）。
//
// 见 `productModelFallbackSet` 字段的注释：它与 `SetProductModels` 的分工是
//
//	SetProductModels          宿主清单（可信）→ 参与"不排除"与"声明"
//	SetProductModelsFallback  网关兜底（猜测）→ **只**参与"不排除"
//
// 入参形状与 `SetProductModels` 相同。传空 map 等于清空兜底。
func (p *Pool) SetProductModelsFallback(byProduct map[string][]string) {
	set := make(map[string]map[string]bool, len(byProduct))
	for prod, models := range byProduct {
		if prod == "" {
			continue
		}
		m := make(map[string]bool, len(models))
		for _, id := range models {
			if id = strings.ToLower(strings.TrimSpace(id)); id != "" {
				m[id] = true
			}
		}
		if len(m) > 0 {
			set[prod] = m
		}
	}
	p.mu.Lock()
	p.productModelFallbackSet = set
	p.mu.Unlock()
}

// ProductOffersModel 报告某产品是否提供该模型。
//
// 返回 (是否提供, 是否有该产品的清单)。第二个返回值让调用方能区分
// "确定不提供"与"不知道" —— 后者**不该**排除账号。
func (p *Pool) ProductOffersModel(product, model string) (bool, bool) {
	p.mu.RLock()
	set, ok := p.productModelSet[product]
	p.mu.RUnlock()
	if !ok || len(set) == 0 {
		return false, false // 不知道
	}
	return set[strings.ToLower(strings.TrimSpace(model))], true
}

// productMayServe 判断某账号是否**可能**服务该模型（无前缀时的额外过滤）。
//
// 规则：
//	· 模型名为空        → 放行（让上游自己报错，比我们猜好）
//	· 该产品没有清单    → 放行（"不知道"不等于"不提供"）
//	· 该产品清单里有它  → 放行
//	· 该产品清单里没它  → **排除**（这就是本次修的缺陷）
//
// ⚠ 只对**多产品开启**时生效：单产品模式下不该有任何新约束，
// 那是"既有行为逐字不变"的承诺。
func (p *Pool) productMayServe(product, model string) bool {
	if model == "" {
		return true
	}
	p.mu.RLock()
	on := p.multiProductOn
	p.mu.RUnlock()
	if !on {
		return true
	}
	offers, known := p.ProductOffersModel(product, model)
	if !known {
		return true
	}
	return offers
}

// SetCostRate 设置某账号对**当前模型**的单位额度消耗率（0 = 未声明）。
//
// 调用方（server 的派发层）在每次请求前按"该模型在该产品的成本倍率"
// 计算好并注入。传 0 表示未声明 → 权重中性。
//
// 为什么不在这里按模型查倍率：倍率表来自上游（WorkBuddy 的 /v3/config、
// Qoder 的 model/list），pool 不该依赖 upstream（会循环依赖）。
func (p *Pool) SetCostRate(uid string, rate float64) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if e, ok := p.byUID[uid]; ok {
		e.costRate = rate
	}
}

// ClearCostRates 清空全部账号的成本率（跨请求复用池时调用，避免残留）。
func (p *Pool) ClearCostRates() {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, e := range p.byUID {
		e.costRate = 0
	}
}

// SetStore 注入池状态快照镜像（redisstore.Store）。nil 表示不镜像（纯本地恢复）。
// 必须在 SyncToDir 之前调用，使"择新恢复"发生在账号对齐之前。
func (p *Pool) SetStore(s StoreSnapshotter) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.store = s
}

// RestoreFromSnapshot 择新恢复：比较本地 state.json 与 Redis 快照，采用较新者。
// 无快照、快照无 savedAt、或本地不存在/不可读时，都会被判定为"本地优先/跳过快照"，
// 同时打一条恢复来源日志。必须在 SyncToDir 之前调用（SyncToDir 只增删不入值）。
func (p *Pool) RestoreFromSnapshot() {
	store := p.store
	if store == nil || p.stateFp == "" {
		return
	}
	localInfo, localErr := os.Stat(p.stateFp)
	raw, ok := store.LoadState()
	if !ok {
		if localErr == nil {
			log.Printf("pool: 恢复来源=本地 state.json（无 Redis 快照）")
		}
		return
	}
	var snap snapshot
	if json.Unmarshal(raw, &snap) != nil || snap.SavedAt.IsZero() {
		// 快照无 savedAt：无法比较新旧，本地优先。
		log.Printf("pool: 恢复来源=本地 state.json（Redis 快照无 saved_at）")
		return
	}
	if localErr == nil && !localInfo.ModTime().After(snap.SavedAt) {
		// 快照不早于本地 → 采用快照。
		p.mu.Lock()
		p.applySnapshotLocked(snap)
		p.mu.Unlock()
		p.dirty.Store(true)
		log.Printf("pool: 恢复来源=Redis 快照 (saved_at=%s)", snap.SavedAt.Format(time.RFC3339))
		return
	}
	log.Printf("pool: 恢复来源=本地 state.json（较新于 Redis 快照 %s）", snap.SavedAt.Format(time.RFC3339))
}

// Acquire 为账号占一个在途名额；false 表示该账号已达上限（或不存在）。
// 必须在成功 Pick 后调用；调用方负责 defer Release。
func (p *Pool) Acquire(uid string) bool {
	p.mu.RLock()
	e, ok := p.byUID[uid]
	limit := p.maxInFlight
	p.mu.RUnlock()
	if !ok {
		return false
	}
	if limit <= 0 {
		// 不限：计数仍累加（供状态观测），但永不拒绝。
		e.inFlight.Add(1)
		return true
	}
	for {
		cur := e.inFlight.Load()
		if cur >= int64(limit) {
			return false
		}
		if e.inFlight.CompareAndSwap(cur, cur+1) {
			return true
		}
	}
}

// AcquireWait 是 `Acquire` 的**等待版**：名额满时短暂轮询等待，而不是立刻失败。
//
// # 为什么必须有它（2026-09-22，所有者报告的现场）
//
// 所有者用 DSH 客户端发 `qoder:Qwen3.8-Flash`，**每次都失败**：
//
//	503 {"code":"no_healthy_account",
//	     "message":"上游服务异常（HTTP 503），已切换到其他账号"}
//
// 而**同样的请求用 curl 发就成功**。差别在于 DSH 的
// `@earendil-works/pi-ai` 层带 `retryProviderRequest`（默认重试 5 次，
// 见 `openai-completions.js:213`），而每个重试都是**独立的并发请求**。
//
// 实测确认的边界（本机 :7864）：
//
//	并发 3 → 3×200          （= max_in_flight）
//	并发 4 → 3×200 + 1×503  ← 客户端 5 次重试必然撞上
//	并发 5 → 3×200 + 2×503
//
// 把 `max_in_flight` 提到 100 后，并发 5/10/15/20 **全部 200**
//（qoder 上游实测能扛 20 并发，网关的 3 是过度保守）。
//
// # 为什么"等待"比"直接失败"正确
//
// 在途名额是**瞬时**资源：一个请求几秒就结束并释放名额。
// 名额暂时满 ≠ 账号不可用。旧行为把两者混为一谈 ——
// 用户看到「所有账号不可用（冷却/禁用）」，而去查一个**完全健康**的账号
//（实测该账号 `cooling=false`、`disabled=false`、`in_flight=0`）。
//
// ⚠ 等待有上限（`wait`），且尊重 `ctx` 取消 —— 不能因为等名额
// 把请求无限挂住（客户端已放弃的请求不该继续占着 goroutine）。
//
// 返回 false 表示：等满了 `wait` 仍未拿到名额，**或** ctx 已取消。
// 调用方应把它当作"这个号暂时用不了"，继续轮转下一个账号。
func (p *Pool) AcquireWait(ctx context.Context, uid string, wait time.Duration) bool {
	// 先试一次：绝大多数请求（未达上限）在这里就成功，不引入任何延迟
	if p.Acquire(uid) {
		return true
	}

	// 名额满：短暂轮询。间隔取 5ms —— 足够快地拿到刚释放的名额，
	// 又不至于把 CPU 打满（等待上限通常 <2s，即最多几百次检查）。
	//
	// 为什么不用 sync.Cond / channel 唤醒：名额的释放方（`Release`）
	// 在**请求结束**路径上，那里不该引入额外的同步开销与死锁风险。
	// 轮询在这个量级（每账号几十个并发）完全够用，且实现简单到不会错。
	const pollEvery = 5 * time.Millisecond
	deadline := time.Now().Add(wait)
	for time.Now().Before(deadline) {
		select {
		case <-ctx.Done():
			return false
		default:
		}
		time.Sleep(pollEvery)
		if p.Acquire(uid) {
			return true
		}
	}
	return false
}

// Release 释放一个在途名额。幂等减到 0 为止（防重复释放扣成负数）。
func (p *Pool) Release(uid string) {	p.mu.RLock()
	e, ok := p.byUID[uid]
	p.mu.RUnlock()
	if !ok {
		return
	}
	for {
		cur := e.inFlight.Load()
		if cur <= 0 {
			return
		}
		if e.inFlight.CompareAndSwap(cur, cur-1) {
			return
		}
	}
}

// SetRandomSource 仅供测试注入确定性随机源；生产代码不应调用。
// 注入源取 n∈[0,n) 后，pickWeighted 的抽签结果完全可预测。
func (p *Pool) SetRandomSource(fn func(n int64) int64) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.randInt64N = fn
}

// startFlusher 每 flushInterval 检查 dirty 标志，有变更则 saveLocked 落盘。
func (p *Pool) startFlusher() {
	interval := flushInterval // 在启动 goroutine 前同步读取，避免与测试对 flushInterval 的恢复写竞争
	go func() {
		t := time.NewTicker(interval)
		defer t.Stop()
		for range t.C {
			p.mu.Lock()
			if p.dirty.Swap(false) {
				p.saveLocked()
			}
			p.mu.Unlock()
		}
	}()
}

// Flush 同步把内存状态落盘（幂等：无变更不写盘）。供进程退出前调用。
func (p *Pool) Flush() {
	p.mu.Lock()
	if p.dirty.Swap(false) {
		p.saveLocked()
	}
	p.mu.Unlock()
}

// Add 加入账号；已存在则保留原状态、更新凭证（upsert 单账号，不影响其他账号）。
func (p *Pool) Add(a *auth.Auth) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.upsertLocked(a)
}

// SyncToDir 用最新扫描结果对齐池：新账号加入、消失的账号剔除（状态保留）。
// 剔除结果持久化回 state.json，避免已删账号在下次启动时被 load() 复活。
//
// ⚠ 本方法的剔除是**按 uid 全量比对**的：调用方交出的 auths 必须覆盖池里
// 「归它管」的全部账号，否则没交出去的那些会被当成「凭证文件已删除」删掉。
// 运行期的热加载不满足这个前提（它只扫 WorkBuddy 的 auths 目录，而池里还混着
// Qoder / ZCode 账号），故走 syncToDirLocked 并显式声明保留范围，见 watch.go。
func (p *Pool) SyncToDir(auths []*auth.Auth) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.syncToDirLocked(auths, nil)
}

// syncToDirLocked 是 SyncToDir 的实现；keep 非 nil 时决定「池中存在但本次扫描
// 未见」的账号是否**保留**（返回 true = 保留，不剔除）。调用方必须已持有 p.mu。
//
// 为什么需要 keep 这个口子：本进程的池是**三个产品共用**的（见 cmd/server/main.go
// 的多产品路由：Qoder / ZCode 凭证由 p.Add 直接塞进同一个池），而 SyncToDir 的
// 剔除按 uid 全量比对。启动路径满足前提（多产品账号在 SyncToDir 之后才 Add），
// 但**运行期的目录热加载不满足** —— 它只扫 WorkBuddy 的 auths 目录。
// 若那里直接调 SyncToDir，一次热加载就会把全部 Qoder / ZCode 账号删出池子，
// 症状是「往 WorkBuddy 加了个账号，另外两个平台的账号全不见了」，且要重启才回来。
func (p *Pool) syncToDirLocked(auths []*auth.Auth, keep func(*entry) bool) {
	seen := make(map[string]bool, len(auths))
	for _, a := range auths {
		seen[a.UID] = true
		p.upsertLocked(a)
	}
	changed := false
	for uid, e := range p.byUID {
		if seen[uid] {
			continue
		}
		if keep != nil && keep(e) {
			continue
		}
		delete(p.byUID, uid)
		changed = true
	}
	if changed {
		p.saveLocked()
	}
}

// upsertLocked 更新或插入单个账号；已存在则只换凭证、保留 credits/cooling 状态。
// 调用方必须已持有 p.mu；Add 与 SyncToDir 共用此 upsert 逻辑。
func (p *Pool) upsertLocked(a *auth.Auth) {
	if e, ok := p.byUID[a.UID]; ok {
		e.a = a // 保留 credits/cooling 状态
		// 到期日：凭证里的值比内存新时才覆盖（宿主同步过来的通常比巡检新）。
		if a.SoonestExpireAt > 0 {
			e.expireAt = a.SoonestExpireAt
		}
		return
	}
	p.byUID[a.UID] = &entry{a: a, expireAt: a.SoonestExpireAt}
}

// SetExpiry 更新账号「最近到期积分」的到期时刻（Unix 秒）；<=0 表示未知，忽略。
//
// 供积分巡检回填。与 SetCredits 分开是因为二者来源不同：
// credits 由签到/巡检更新，到期日还可能来自宿主写入的凭证元数据。
func (p *Pool) SetExpiry(uid string, soonestExpireAt int64) {
	if soonestExpireAt <= 0 {
		return
	}
	p.mu.Lock()
	defer p.mu.Unlock()
	if e, ok := p.byUID[uid]; ok && e.expireAt != soonestExpireAt {
		e.expireAt = soonestExpireAt
		p.dirty.Store(true)
	}
}

// SetCreditsAndExpiry 一次更新余额与到期日（巡检路径用，避免两次加锁）。
func (p *Pool) SetCreditsAndExpiry(uid string, credits, soonestExpireAt int64) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if e, ok := p.byUID[uid]; ok {
		e.credits = credits
		if soonestExpireAt > 0 {
			e.expireAt = soonestExpireAt
		}
		p.dirty.Store(true)
	}
}

// Pick 选出一个可用账号；无可用返回 nil。
//
// 策略见 pick：先按「积分最近到期日」分层（优先烧快过期的额度），
// 同档内再按权重加权随机（同级平均分摊）。
func (p *Pool) Pick() *auth.Auth {
	return p.PickExcluding(nil)
}

// PickExcluding 同上，但跳过 tried 中的 uid（请求级轮换）。
func (p *Pool) PickExcluding(tried map[string]bool) *auth.Auth {
	return p.pick(tried, "")
}

// PickForModel 选出一个**该模型当前未被限流**的可用账号；无可用返回 nil。
//
// model 为空时等价于 PickExcluding（不做模型过滤）——调用方拿不到模型名时
// 退化为原有行为，不会因为新特性而选不出账号。
func (p *Pool) PickForModel(model string, tried map[string]bool) *auth.Auth {
	return p.PickForModelRegion(model, tried, auth.RegionAny)
}

// PickForModelRegion 在 PickForModel 之上叠加**区域偏好**。
//
// 为什么需要：同名模型在两个区域可能是不同的后端模型，能力并不一致
// （见 auth.Region 的注释：国服 glm-5.3 能读图，国际版同名模型读不到）。
// 不指定区域时选号是随机的，于是「同一个 glm-5.3」会时好时坏 ——
// 用户看到的是模型不稳定，实际是命中了两个不同后端。
//
// prefer 语义是**偏好而非强制**：
//   - prefer=RegionAny，或该区域没有可用账号 → 与 PickForModel 完全一致；
//   - 有该区域的可用账号 → 只在其中挑；挑不到才回退到全池。
//
// 之所以不做强制：宁可回退到一个「区域不符但能用」的账号，也不要因为
// 该区域账号恰好都在冷却/在途占满而直接 503 —— 后者会让本来能成功的请求失败。
// 上层（server）只对**确知存在区域差异**的场景才传具体区域。
func (p *Pool) PickForModelRegion(model string, tried map[string]bool, prefer auth.Region) *auth.Auth {
	if prefer == auth.RegionAny {
		return p.pickForModelAny(model, tried)
	}
	// 先按区域收窄候选；收窄后选不出（返回 nil）再放开，保证不因偏好而失败。
	if acct := p.pickForModelInRegion(model, tried, prefer); acct != nil {
		return acct
	}
	return p.pickForModelAny(model, tried)
}

// PickForModelRegionStrict 同 PickForModelRegion，但区域是**强制**约束：
// 该区域没有可用账号时返回 nil，**不**回退到其它区域。
//
// 与偏好版的适用场景不同，不要互相替代：
//
//	偏好版（PickForModelRegion）—— 「用这个区域更好，但其它区域也能用」。
//	  例：负载均衡时希望优先烧某个区域的额度。回退是合理的。
//	强制版（本函数）—— 「只在这个区域才对」。例：带图片的 glm-5.x 只有国服
//	  后端能读图，跨区会让图片被静默替换成占位符，模型回「我看不见图片」；
//	  此时回退不是「降级可用」，而是「静默地做错事」。
//
// 返回 nil 让上层给出**可读的错误**（缺哪个区域的账号），而不是让用户
// 对着一个看似成功的响应里模型说「无法查看图片」发愣。
func (p *Pool) PickForModelRegionStrict(model string, tried map[string]bool, region auth.Region) *auth.Auth {
	if region == auth.RegionAny {
		return p.pickForModelAny(model, tried)
	}
	return p.pickForModelInRegion(model, tried, region)
}

// PickForModelProductRegion 按**产品 + 区域**选号（所有者的需求：`平台:区域:模型名`）。
//
// # 为什么需要它
//
// 三个平台**有重名模型** —— `glm-5.3` 既在 ZCode 的套餐里、也在 WorkBuddy
// 的清单里。裸名请求时账号池按「最早到期分层」选号，可能选中一个
// **没有该模型资源包**的账号，那条通道回 `1113 无可用资源包`，
// 用户看到的是"余额不足"（其实额度充足，只是走错了平台）。
//
// 客户端用 `qoder:glm-5.3` / `zcode:国际版:glm-5.3` 显式指定即可消除歧义。
//
// # 实现方式：复用「收窄 tried」这个既有惯用法
//
// 与 `pickForModelInRegion` 同一套路 —— 把不符合的账号**塞进排除集**，
// 而不是给每个 pick 函数加参数。好处：
//
//	· 不必改十几处 pick 签名（改动面小 = 出错面小）
//	· 区域与产品用同一机制，行为可预期
//	· 排除集与实际池子在**同一次 RLock 内**构造，避免两次加锁之间
//	  池子变化导致筛选条件悄悄失效
//
// # 产品约束是**强制**的，不走兜底
//
// 与区域偏好不同：区域可以"偏好不成再放开"，而产品**不行** ——
// 用户写 `qoder:` 就是明确说"只走这个平台"（他可能知道别的平台没额度）。
// 若兜底跨平台，他会看到"我明明指定了 qoder，却报了 ZCode 的 1113"。
//
// 故产品约束走 `Strict` 路径（无全冷却兜底），选不出就返回 nil，
// 由上层给出**指名道姓**的错误。
//
// # `product == ""` 时退化为原行为
//
// 空串 = 不限制产品，直接转交原有的区域逻辑 —— 老客户端不带前缀，
// 行为必须逐字不变。
func (p *Pool) PickForModelProductRegion(
	model string, tried map[string]bool, prefer auth.Region, product string,
) *auth.Auth {
	if product == "" {
		return p.PickForModelRegion(model, tried, prefer)
	}

	p.mu.RLock()
	rot := p.rotationOn
	scoped := make(map[string]bool, len(tried)+len(p.byUID))
	for uid := range tried {
		scoped[uid] = true
	}
	// 排除两类账号：
	//  1. 产品不符（用 ProductOf() —— 老账号的 Product 是空串，
	//     语义上等价于 workbuddy；裸读会让它们全部被排除，
	//     表现为"指定 workbuddy 却一个号都选不出"）
	//  2. 区域不符（与 pickForModelInRegion 同一判据；prefer 为 Any 时跳过）
	for uid, e := range p.byUID {
		if e.a == nil {
			scoped[uid] = true
			continue
		}
		if e.a.ProductOf() != product {
			scoped[uid] = true
			continue
		}
		if prefer != auth.RegionAny && e.a.Region() != prefer {
			scoped[uid] = true
		}
	}
	p.mu.RUnlock()

	if rot {
		if a := p.pickRotationStrict(scoped, model); a != nil {
			return a
		}
	} else if a := p.pickStrict(scoped, model); a != nil {
		return a
	}

	// 该产品一个健康账号都没有 —— **不直接返回 nil**（2026-09-22 修正）。
	//
	// # 所有者现场
	//
	//	客户端固定用 `qoder:Qwen3.8-Flash`，突然全部 503：
	//	  {"code":"no_healthy_account","message":"all accounts unavailable (cooling/disabled)"}
	//
	// 查证：qoder 只有 **1 个**账号，它连续吃到 3 次上游 503
	//（`applyErrorPolicy` 的 ErrServer 分支 → `NoteError` → 达阈值熔断），
	// 于是被熔断 **30 分钟**。而产品前缀请求只能选该产品的账号，
	// `pickStrict` 又刻意不兜底 ⇒ 选不出 ⇒ 整个产品**彻底不可用 30 分钟**。
	//
	// # 为什么这里必须兜底（而 pickStrict 的"不兜底"在别处是对的）
	//
	// `pickStrict` 不兜底是为了避免**跨区域/跨产品**的降级（那会让图片被
	// 换成占位符、或拿错平台的凭证）。但本函数的排除集**已经锁死了产品**，
	// 兜底只在**同一产品内部**选，不存在"降级到别的产品"的问题。
	//
	// 而熔断的语义是"这个号可能不好，**换个号**" —— 当该产品**只有一个号**时，
	// 熔断就失去了意义：没有别的号可换，熔断只是把一次**上游瞬时故障**
	//（5xx）放大成 30 分钟的**全量停服**。
	//
	// 取舍：宁可拿这个"可能不好"的号去试一次 ——
	//
	//	· 上游已恢复 ⇒ 请求成功（用户不必干等 30 分钟）
	//	· 上游仍故障 ⇒ 用户看到**真实的上游错误**，而不是
	//	  "所有账号不可用"这种把排查方向引向账号的错误提示
	//
	// 两种情况都不比现状差。且 `tried` 仍在排除集里，**一次请求内不会重复打同一个号**。
	//
	// ⚠ 只在"该产品**确实**一个健康号都没有"时兜底：只要有一个健康的，
	// 上面的严格路径就返回了，本分支根本不会执行 —— 所以多账号产品的
	// 熔断保护**完全不受影响**。
	//
	// ⚠ 复用 `pickEarliestExpiryLocked`（它已正确跳过 disabled / NoRoute /
	// 硬冷却 / 模型冷却 / 在途占满），且 `scoped` 同时充当"只在本产品内选"
	// 的排除集 —— 不必再写一份筛选，避免两条路径的判据分叉
	//（本文件已有 `TestSelectionPathsAgreeOnNoRoute` 钉住那次教训）。
	return p.pickEarliestExpiryLocked(scoped, time.Now(), model)
}

// pickForModelAny 原有行为：不做区域过滤。
//
// # ⚠ 2026-09-20：加了"该产品是否提供该模型"的过滤（修实测缺陷）
//
// 用户用 `deepseek-v4.1-flash`（WorkBuddy 的模型）发请求，却被路由到
// **Qoder 账号**上并失败两次 —— 因为这里只按"账号当前可用"挑，
// **不问"这个产品有没有这个模型"**。Qoder 账号当时恰好可用就被选中。
//
// 故这里排除"不提供该模型的产品"的账号（见 productMayServe）。
// ⚠ 只在多产品开启 + 宿主透传了清单时生效；两者任一不满足即保持既有行为，
// 否则会因"不知道清单"而把所有账号排掉。
// pickForModelAny 原有行为：不做区域过滤。
//
// # ⚠ 2026-09-20：加了"该产品是否提供该模型"的过滤（修实测缺陷）
//
// 用户用 `deepseek-v4.1-flash`（WorkBuddy 的模型）发请求，却被路由到
// **Qoder 账号**上并失败两次 —— 因为这里只按"账号当前可用"挑，
// **不问"这个产品有没有这个模型"**。Qoder 账号当时恰好可用就被选中。
//
// 故这里排除"不提供该模型的产品"的账号（见 productMayServeLocked）。
// ⚠ 只在多产品开启 + 宿主透传了清单时生效；两者任一不满足即保持既有行为 ——
// 否则会因"不知道清单"而把所有账号排掉，那比原缺陷更糟。
func (p *Pool) pickForModelAny(model string, tried map[string]bool) *auth.Auth {
	p.mu.RLock()
	rot := p.rotationOn
	// 在同一把读锁内构造排除集：分两次加锁会让池子在两次之间变化，
	// 使筛选条件与实际池子不一致。
	//
	// ⚠ 必须**复制** tried 而不是就地改：它是调用方传进来的 map，
	// 就地写会把"已排除集"污染到调用方的重试循环里 —— 表现为
	// "第一次挑不到号之后，后续永远挑不到"。
	scoped := make(map[string]bool, len(tried)+4)
	for uid := range tried {
		scoped[uid] = true
	}

	// ⚠⚠ 两轮排除（2026-09-21 修复：裸名 Qwen3.8-Flash 被路由到 WorkBuddy）
	//
	// 第 1 轮：**明确声明不提供**该模型的产品 → 排除。
	//   即"有清单、且清单里没有它"。这是确定的否定，永远可以排除。
	//
	// 第 2 轮：**没声明提供**的产品（清单未知，或清单里没有）
	//   → **先记下，本轮不排除**。
	//   若第 1 轮之后还有"明确声明提供"的候选，就只在它们里挑；
	//   否则再放开用这些（见下面的 preferDeclared）。
	//
	// # 为什么不能像旧代码那样"未知 ⇒ 照常参与"
	//
	// 旧代码：`if !productMayServeLocked(...) { scoped[uid] = true }`，
	// 而 `productMayServeLocked` 对"清单未知"返回 **true**（不排除）。
	//
	// 于是当 `product_models` **缺 workbuddy** 时（宿主那侧来自用户白名单，
	// 默认为空 ⇒ 不写），WorkBuddy 账号**不被排除**，而它没有
	// `Qwen3.8-Flash` ⇒ 请求在撞上第一个 WorkBuddy 账号时就回 11102，
	// 压根轮不到真正有这个模型的 qoder 账号。
	//
	// 所有者的现场正是如此：
	//
	//	`Qwen3.8-Flash`        → 400 model_not_in_region
	//	`qoder:Qwen3.8-Flash`  → 成功
	//
	// 关键区别：**"不知道" ≠ "能满足"**。未知只该让它在**没有更好选择时**
	// 兜底，而不该让它与"明确声明提供"的产品**平等竞争** ——
	// 后者才是这个模型真正该去的地方。
	var declared, unknown []string
	for uid, e := range p.byUID {
		if e.a == nil {
			continue
		}
		if p.productDeclaresModelLocked(e.a.ProductOf(), model) {
			declared = append(declared, uid)
		} else if !p.productMayServeLocked(e.a.ProductOf(), model) {
			// 有清单且明确没有 → 确定排除
			scoped[uid] = true
		} else {
			// 清单未知（或该产品不参与多产品约束）→ 兜底候选
			unknown = append(unknown, uid)
		}
	}
	p.mu.RUnlock()

	pick := func(t map[string]bool) *auth.Auth {
		if rot {
			return p.pickRotation(t, model)
		}
		return p.pick(t, model)
	}

	// 优先只在"明确声明提供该模型"的产品里挑。
	if len(declared) > 0 {
		pref := make(map[string]bool, len(scoped))
		for uid := range scoped {
			pref[uid] = true
		}
		// 把兜底候选也排除掉，让选号只看声明过的产品。
		for _, uid := range unknown {
			pref[uid] = true
		}
		if acct := pick(pref); acct != nil {
			return acct
		}
		// 声明过的产品里挑不到（都在冷却/在途占满）——**不能直接放弃**，
		// 否则"声明了但暂时不可用"会让请求 503，而兜底候选明明能用。
		// 故继续往下走，用完整候选集（含未知）再挑一次。
	}

	return pick(scoped)
}

// productMayServeLocked 是 productMayServe 的**持锁**版本。
//
// 单独一个是因为 pickForModelAny 必须在**同一次** RLock 内完成
// "读多产品开关 + 读清单 + 构造排除集"，否则三次读之间池子会变。
func (p *Pool) productMayServeLocked(product, model string) bool {
	if model == "" || !p.multiProductOn {
		return true
	}
	key := strings.ToLower(strings.TrimSpace(model))
	set, ok := p.productModelSet[product]
	if !ok || len(set) == 0 {
		// 宿主没有这个产品的可信清单。
		//
		// ⚠ 但网关可能补过兜底清单 —— 它在这里**有效**（本函数问的是
		// "是否不排除"，兜底清单正是为此存在）。
		if fb, ok2 := p.productModelFallbackSet[product]; ok2 && len(fb) > 0 {
			return fb[key]
		}
		return true // 完全不知道这个产品提供什么 → 不排除
	}
	if set[key] {
		return true
	}
	// 可信清单里没有，但兜底清单里有 ⇒ 仍然不排除。
	//
	// 为什么不直接 `return false`：宿主那份清单可能**不完整**
	//（例如只列了用户白名单里的几个），而兜底清单是"网关已知的更多
	// 可能性"。两者取并集才安全 —— 排除一个其实能服务的账号，
	// 用户会看到"模型明明存在却报不可用"。
	if fb, ok2 := p.productModelFallbackSet[product]; ok2 && fb[key] {
		return true
	}
	return false
}

// productDeclaresModelLocked 报告该产品**明确声明**提供该模型。
//
// 与 productMayServeLocked 的区别（这个区别是本次修复的核心）：
//
//	productMayServeLocked       "是否**不排除**它" —— 清单未知时返回 true（宁可放行）
//	productDeclaresModelLocked  "是否**明确列出**了它" —— 清单未知时返回 false
//
// # 为什么只看**宿主**清单，不看网关兜底清单（2026-09-22 修正）
//
// 兜底清单（`productModelFallbackSet`）来自网关的内置静态表，是**猜测**：
// 它列的是"网关以为 WorkBuddy 支持的模型"，而不是上游的真实能力。
//
// 曾经把它也算作"声明"，结果是所有者现场：
//
//	发 `GLM-5.3` → 400 model_not_in_region（而 `Qwen3.8-Flash` 正常）
//
// 因为 `GLM-5.3` 同时被兜底清单（`glm-5.3`）与 qoder 的可信清单声明
// ⇒ 两个产品都进 `declared` ⇒ 一起竞争 ⇒ 19 个 WorkBuddy 账号压倒
// 1 个 qoder 账号 ⇒ 选到其实**没有**该模型的 WorkBuddy ⇒ 11102。
//
// 而 `Qwen3.8-Flash` 只有 qoder 声明（路由唯一）⇒ 正常。
//
// ⇒ **"声明"必须是可信的**。兜底清单只用于"不排除"。
//
// # 为什么要区分（2026-09-21 所有者报的现场）
//
//	发 `Qwen3.8-Flash`（裸名）→ 400 model_not_in_region
//	发 `qoder:Qwen3.8-Flash`   → 成功
//
// 根因：`product_models` 里**只有 qoder / zcode，没有 workbuddy**
//（宿主侧那份来自用户白名单，默认为空 ⇒ 不写 workbuddy）。
// 于是"workbuddy 清单未知" ⇒ `productMayServeLocked` 返回 true ⇒
// **不排除 WorkBuddy 账号**。而 WorkBuddy 没有 Qwen3.8-Flash ⇒ 上游回 11102，
// 请求在**撞上第一个 WorkBuddy 账号**时就失败了，压根没轮到 qoder 账号。
//
// 关键洞察：**"不知道"不该等同于"能满足"**。
// 旧逻辑把未知当作"可以服务"，于是当清单不全时，未列出的产品会**抢占**
// 那些其实只有别的产品才有的模型。而按"声明"排序后，清单不全最坏只是
// "没优先到"，绝不会让请求撞到一个明显不匹配的产品上。
func (p *Pool) productDeclaresModelLocked(product, model string) bool {
	if model == "" || !p.multiProductOn {
		return false
	}
	set, ok := p.productModelSet[product]
	if !ok || len(set) == 0 {
		return false // 清单未知 → 不算"声明"
	}
	return set[strings.ToLower(strings.TrimSpace(model))]
}

// pickForModelInRegion 只在指定区域的账号里挑。
//
// 复用 pick/pickRotation 的整套策略（到期分层、加权、在途、冷却），
// 只是把候选集先按区域过滤 —— 这样区域偏好不会绕过任何既有保护，
// 也不会因为多一套选号实现而产生行为漂移。
//
// **关键**：区域内选不出号时返回 nil，**不做全冷却兜底**。
// 兜底（pickEarliestExpiryLocked）会选「冷却中但截止最早」的账号，
// 于是「偏好国服」会变成「把一个正在冷却的国服号塞进来」——冷却被绕过。
// 实测用例 TestImageRequestSkipsCooledCNAccount 锁住这一点：
// 国服号在冷却、国际版号健康时，必须选国际版号，而不是回头用冷却的国服号。
//
// 返回 nil 让上层（PickForModelRegion）决定是否放开区域 —— 那是**跨区域**的
// 降级，与「在同一区域内绕过冷却」是完全不同性质的退让。
func (p *Pool) pickForModelInRegion(model string, tried map[string]bool, region auth.Region) *auth.Auth {
	p.mu.RLock()
	rot := p.rotationOn
	scoped := make(map[string]bool, len(tried))
	for uid := range tried {
		scoped[uid] = true
	}
	// 把所有非本区域账号塞进排除集，等价于「只在区域内挑」。
	// 构造与加锁在同一次 RLock 内完成，避免两次加锁之间账号池变化
	// 导致排除集与实际池子不一致（那会让筛选条件随并发悄悄失效）。
	for uid, e := range p.byUID {
		if e.a.Region() != region {
			scoped[uid] = true
		}
	}
	p.mu.RUnlock()

	if rot {
		return p.pickRotationStrict(scoped, model)
	}
	return p.pickStrict(scoped, model)
}

// pickStrict 与 pick 相同，但**不做全冷却兜底**：没有可用候选时返回 nil。
//
// 单独抽出来而不是给 pick 加参数，是为了让「要不要兜底」在调用点一眼可见 ——
// 兜底会绕过冷却，是个需要显式决定的语义。
func (p *Pool) pickStrict(tried map[string]bool, model string) *auth.Auth {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.pickLocked(tried, model, false)
}

// pickRotationStrict 轮转模式下的严格版（不兜底），见 pickStrict。
//
// 另外必须处理「当前轮转锁定的账号不在本区域」：pickRotation 会优先续用
// p.rotationUID，而它可能已被排除在 scoped 之外。若不管，续用逻辑会绕过
// 区域约束（用国际版号接图片请求）—— 正是本特性要避免的。
func (p *Pool) pickRotationStrict(tried map[string]bool, model string) *auth.Auth {
	p.mu.Lock()
	defer p.mu.Unlock()
	if tried != nil && tried[p.rotationUID] {
		// 锁定的账号被本区域排除：本轮不用它，也不改 rotationUID（它可能
		// 只是不满足这次的区域要求，其它请求仍在正常用它）。
		return p.pickRotationLockedSkipCurrent(tried, model)
	}
	return p.pickRotationLocked(tried, model, false)
}

// pickBestFrom 从给定候选里挑一个（复用主路径的排序口径）。
//
// 用途：平台白名单让正常候选**全空**时的兜底（见 pickLocked 的
// excludedByPlatform）。它不是"随便给一个" —— 仍按：
//
//	到期分层（先烧快过期的）→ 权重降序（credits / 成功率 / 闲置补偿）
//
// 复用同一套口径的理由：兜底也是真实请求，选号质量不该因为"走了兜底"
// 就退化成随机。若这里另写一套，两个路径的行为会随时间漂移。
func (p *Pool) pickBestFrom(cands []*entry, tried map[string]bool, now time.Time, model string) *entry {
	// 在途占满的仍要排除（它们会立刻失败）
	usable := make([]*entry, 0, len(cands))
	for _, e := range cands {
		if tried != nil && tried[e.a.UID] {
			continue
		}
		if !e.healthy(now) || e.modelCooled(model, now) || p.inFlightFull(e) {
			continue
		}
		usable = append(usable, e)
	}
	if len(usable) == 0 {
		return nil
	}

	usable, tiered := p.earliestExpiryTierLocked(usable)

	var maxCredits int64
	for _, e := range usable {
		if e.credits > maxCredits {
			maxCredits = e.credits
		}
	}
	var best *entry
	var bestW float64
	for _, e := range usable {
		w := p.weightOf(e, maxCredits, now)
		if tiered {
			w = p.tierWeightOf(e, now)
		}
		if best == nil || w > bestW {
			best, bestW = e, w
		}
	}
	return best
}

// SetRotation 开关「单一模型 + 积分轮转」模式。
//
// 该模式与「负载均衡」的差别：负载均衡在最早到期的那一档**内部分摊**，
// 同一时刻多个账号并行承接流量；轮转模式则是**串行烧号** —— 始终只用**一个**
// 账号，把它烧到不可用（余额耗尽 / 被限流 / 熔断）才换下一个，且换的仍是
// 按到期紧迫度排序的下一个。适合「把某个账号的额度用干净再走」的用法。
func (p *Pool) SetRotation(on bool) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.rotationOn = on
}

// SetModelPlatforms 设置「模型 → 允许的平台」映射（所有者的需求）。
//
// # 为什么需要
//
// 所有者的要求：「我希望可以加上 **平台区分使用哪个平台的模型**」。
//
// 背景：同名模型可能同时存在于多个平台（实测 `glm-5.2` 在 WorkBuddy 与
// ZCode 上都有，`deepseek-v4.1-flash` 在三个平台上都有）。选号当前是
// 按权重随机的 —— 用户无法指定"这个模型走 ZCode"。
//
// 而他**需要**这个能力，因为各平台的额度性质不同：
//
//	ZCode 体验套餐：每日重置，**9/23 到期后归零** → 该优先烧掉
//	WorkBuddy：长期额度
//
// 不指定的话，体验额度可能到过期都没用上。
//
// # 语义：**允许**（白名单），不是"偏好"
//
//	m["glm-5.2"] = ["zcode"]         → 只允许 ZCode
//	m["glm-5.2"] = ["zcode","qoder"] → 两个都可以（走默认加权）
//	m 里没有这个模型 / 值为空          → 不限制（保持现状）
//
// 为什么用"允许"而不是"优先"：只勾一个平台就等于"只用它"，
// 语义自然，且不必再引入"强制 vs 偏好"两套模式。
//
// ⚠ 但被允许的平台**全部不可用**时（冷却/禁用/在途满），选号会回退到全池
// —— 宁可回退一个"平台不符但能用"的账号，也不要让本来能成功的请求 503。
// 这与 PickForModelRegion 的取舍一致。
func (p *Pool) SetModelPlatforms(m map[string][]string) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.modelPlatforms = m
}

// ModelPlatforms 报告当前的模型→平台映射（供诊断与测试断言）。
func (p *Pool) ModelPlatforms() map[string][]string {
	p.mu.RLock()
	defer p.mu.RUnlock()
	out := make(map[string][]string, len(p.modelPlatforms))
	for k, v := range p.modelPlatforms {
		out[k] = append([]string(nil), v...)
	}
	return out
}

// platformAllowedLocked 该模型是否允许在该平台上使用。
//
// 未配置 → 允许（保持现状，不改变既有行为）。
func (p *Pool) platformAllowedLocked(model, product string) bool {
	if len(p.modelPlatforms) == 0 || model == "" {
		return true
	}
	allowed, ok := p.modelPlatforms[model]
	if !ok || len(allowed) == 0 {
		// 该模型没被配置 → 不限制
		return true
	}
	// WorkBuddy 是默认产品，`Product` 字段可能是空串（老账号没这个字段）。
	// 按 WorkBuddy 处理，否则老账号会被静默排除出所有请求。
	if product == "" {
		product = auth.ProductWorkBuddy
	}
	for _, a := range allowed {
		if a == product {
			return true
		}
	}
	return false
}

// RotationOn 报告当前是否处于轮转模式。
func (p *Pool) RotationOn() bool {
	p.mu.RLock()
	defer p.mu.RUnlock()
	return p.rotationOn
}

// pickRotation 轮转选号：锁定一个账号，直到它不可用才换下一个。
//
// 与 pick 的关键差异：
//   - **不做档位内分摊**，而是确定性取「按到期日升序的第一个可用账号」；
//   - 已有在用的账号（rotationUID）只要仍然可用就继续用它，不随机、不轮换；
//   - 只有当它变成不可用（冷却/熔断/余额耗尽/该模型被限流/在途占满）时，
//     才按同样的顺序挑下一个。
//
// tried 仍被尊重（请求级轮换：同一请求内换过号就不再回头），
// 这样上层 forward 的重试逻辑无需改动。
func (p *Pool) pickRotation(tried map[string]bool, model string) *auth.Auth {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.pickRotationLocked(tried, model, true)
}

// pickRotationLocked 轮转选号的实现。调用方必须已持 p.mu。
//
// allowFallback=false 时不做全冷却兜底（无可用候选返回 nil），供区域收窄后
// 的调用路径使用 —— 兜底会把「冷却中的账号」选回来，绕过区域约束。
func (p *Pool) pickRotationLocked(tried map[string]bool, model string, allowFallback bool) *auth.Auth {
	now := time.Now()

	// 1) 仍在用的账号若还可用，直接续用 —— 这是「串行烧号」的核心。
	if p.rotationUID != "" {
		if e, ok := p.byUID[p.rotationUID]; ok && p.rotationUsableLocked(e, now, model, tried) {
			p.markUsed(e)
			return e.a
		}
	}

	// 2) 需要换号：按「到期日升序 + 同级按 uid」确定性排序，取第一个可用者。
	//    确定性排序（而非随机）是刻意的：轮转模式的语义就是「有明确的下一个」，
	//    随机会让「烧完 A 该轮到谁」变得不可预期。
	cands := make([]*entry, 0, len(p.byUID))
	for uid, e := range p.byUID {
		if tried != nil && tried[uid] {
			continue
		}
		if !p.rotationUsableLocked(e, now, model, nil) {
			continue
		}
		cands = append(cands, e)
	}
	if len(cands) == 0 {
		if !allowFallback {
			// 区域收窄后无可用候选：不改 rotationUID（锁定号可能只是不满足
			// 本次的区域要求，其它请求仍该继续用它），交给上层决定是否放开区域。
			return nil
		}
		// 无可用账号：沿用既有兜底（取最早截止的冷却账号试一次）。
		p.rotationUID = ""
		return p.pickEarliestExpiryLocked(tried, now, model)
	}
	sort.Slice(cands, func(i, j int) bool {
		ki, kj := cands[i].expiryDayKey(), cands[j].expiryDayKey()
		if ki != kj {
			// 到期日未知（空串）排最后：与分层选号的口径一致。
			if ki == "" {
				return false
			}
			if kj == "" {
				return true
			}
			return ki < kj
		}
		return cands[i].a.UID < cands[j].a.UID
	})
	chosen := cands[0]
	prev := p.rotationUID
	p.rotationUID = chosen.a.UID
	if prev != chosen.a.UID {
		log.Printf("pool: rotation_switch %s -> %s (expire=%s)", prev, chosen.a.UID, chosen.expiryDayKey())
	}
	p.markUsed(chosen)
	return chosen.a
}

// pickRotationLockedSkipCurrent 轮转模式下**跳过当前锁定的账号**挑一个。
//
// 用于「锁定的账号不满足本次约束（如区域不符）」的场景：此时不能续用它，
// 但也不该清掉 rotationUID —— 它可能只是对本次请求不合适（例如带图片的
// glm-5.3 偏好国服，而锁定号是国际版），对不要求区域的普通请求仍然是
// 正确的「当前号」。清掉会让轮转语义变成「被一次图片请求打乱」。
//
// 实现上把锁定号并入 tried 后走正常路径：tried 语义正是「本次不选它」，
// 与这里要表达的意思一致，因此无需再写一套挑选逻辑。
func (p *Pool) pickRotationLockedSkipCurrent(tried map[string]bool, model string) *auth.Auth {
	skip := make(map[string]bool, len(tried)+1)
	for uid := range tried {
		skip[uid] = true
	}
	skip[p.rotationUID] = true
	locked := p.rotationUID
	acct := p.pickRotationLocked(skip, model, false)
	// pickRotationLocked 换号时会改写 rotationUID；本次「跳过」不应改变
	// 轮转的持久状态，因此原样还原（它仍代表那个账号，只是本次不用）。
	p.rotationUID = locked
	return acct
}

// rotationUsableLocked 报告账号在轮转模式下是否可用。调用方必须已持锁。
//
// 判定与普通 pick 的候选过滤保持一致（健康 / 该模型未被限流 / 未占满在途），
// 额外尊重 tried：轮转模式在一个请求内换号时也不该回头。
func (p *Pool) rotationUsableLocked(e *entry, now time.Time, model string, tried map[string]bool) bool {
	if tried != nil && tried[e.a.UID] {
		return false
	}
	if !e.healthy(now) {
		return false
	}
	if e.modelCooled(model, now) {
		return false
	}
	if p.inFlightFull(e) {
		return false
	}
	return true
}

// pick 选出本次请求使用的账号，并记录 lastUsed（防并发撞号）。
//
// model 非空时跳过「该模型正处于模型级冷却」的账号（见 entry.modelCooled）；
// 这样上游 6004 命中的账号只在该模型上被绕开，其余模型照常参与负载均衡。
//
// 两级策略：
//
//  1. 到期分层（expiry tier）—— 先按「最近到期积分」的到期日把候选分组，
//     只保留最早到期的那一组，把流量优先导向「快过期」的额度，避免积分作废。
//     同一天到期的账号归为同一档，因此组内是平均使用（不会因为某号积分多就多打）。
//     到期日未知的账号排到最后：只在其它账号都用完/不可用时才轮到它们。
//  2. 组内挑选 —— 到期同档内不再看积分多少（同档代表同样紧迫），
//     只用「闲置补偿 + 成功率」做小幅微调后加权随机，
//     效果即「同一天到期的账号平均分摊」。
//
// 并发防雪崩：跳过 lastUsed 距今 < minPickGap 的账号（除非候选全部刚被用过，
// 此时退回最近最少使用 LRU 账号），迫使高并发请求发散。
// minPickGap=0（测试用）时过滤恒通过，退化为纯加权随机。
func (p *Pool) pick(tried map[string]bool, model string) *auth.Auth {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.pickLocked(tried, model, true)
}

// pickLocked 选号的实现。调用方必须已持 p.mu。
//
// allowFallback=false 时不做全冷却兜底（无可用候选返回 nil），供区域收窄后的
// 调用路径使用。兜底会选「冷却中但截止最早」的账号，若区域收窄后仍走兜底，
// 「偏好国服」就退化成「把冷却中的国服号塞回来」—— 冷却被静默绕过。
func (p *Pool) pickLocked(tried map[string]bool, model string, allowFallback bool) *auth.Auth {
	now := time.Now()

	var cands []*entry
	// excludedByPlatform 记录"仅因为平台白名单被排除"的账号。
	//
	// 用途：白名单让候选**全空**时用它兜底。否则会出现这样的坏结果：
	// 用户勾了"glm-5.2 只用 ZCode"，而 ZCode 账号恰好都在冷却 →
	// 候选为空 → 兜底逻辑也只从被允许的平台里找 → 仍然空 → **503**。
	// 而 WorkBuddy 明明可用。宁可给他一个"平台不符但能用"的账号。
	var excludedByPlatform []*entry
	for uid, e := range p.byUID {
		if tried != nil && tried[uid] {
			continue
		}
		// 顺手清理已过期的模型冷却，避免 map 随模型种类无限增长。
		if e.pruneModelCoolsLocked(now) {
			p.dirty.Store(true)
		}
		if !e.healthy(now) {
			continue
		}
		// 模型级限流：只跳过"该模型被冷却"的账号，其余模型不受影响。
		if e.modelCooled(model, now) {
			continue
		}
		// 平台白名单：用户明确指定了"这个模型只用某几个平台"时，
		// 其余平台的账号不参与候选（见 SetModelPlatforms）。
		if !p.platformAllowedLocked(model, e.a.Product) {
			excludedByPlatform = append(excludedByPlatform, e)
			continue
		}
		if p.inFlightFull(e) {
			continue // 在途占满：跳过（max=0 不限时不触发）
		}
		cands = append(cands, e)
	}
	if len(cands) == 0 {
		if !allowFallback {
			return nil
		}
		// 兜底 1：白名单内一个可用账号都没有，但**白名单外**有健康的 ——
		// 用它。见 excludedByPlatform 的说明（不做的话就是 503）。
		if len(excludedByPlatform) > 0 {
			if picked := p.pickBestFrom(excludedByPlatform, tried, now, model); picked != nil {
				p.markUsed(picked)
				return picked.a
			}
		}
		// 兜底 2：全冷却兜底 —— 无 healthy 候选时，从冷却账号里选 until
		// 最早到期的一个（熔断/冷却共用 expiry 口径）。禁用的账号永不参与。
		return p.pickEarliestExpiryLocked(tried, now, model)
	}

	// 到期分层：只保留最早到期的一档。
	//
	// 分档而非加权，是为了让「先烧快过期额度」成为确定性行为：加权随机下
	// 即将过期的额度仍会被分走一部分流量，而额度一旦过期就是净损失。
	cands, tiered := p.earliestExpiryTierLocked(cands)

	// top5 短名单按权重降序截断（而非 credits 单纯降序）：否则闲置补偿 + 成功率
	// 根本进不了短名单决策，低 credits 但高成功率/久置的账号会永远排不进 top5。
	var maxCredits int64
	for _, e := range cands {
		if e.credits > maxCredits {
			maxCredits = e.credits
		}
	}
	// 权重只算一次：顶 5 截断要排序，若在 sort 比较器里现算 weightOf 会翻成 O(n log n) 次
	// 冗余浮点计算（46 账号约 500 次）。先做 O(n) 预计算，再按权重降序排序。
	ws := make([]weightedEntry, len(cands))
	for i, e := range cands {
		w := p.weightOf(e, maxCredits, now)
		if tiered {
			w = p.tierWeightOf(e, now)
		}
		ws[i] = weightedEntry{e: e, w: w}
	}
	// 短名单截断的平局处理（issue #5）：权重完全相同时（首次启动、credits 全 0、
	// 无成功/错误记录）若一律按 UID 字典序截断，短名单会恒为最小的 5 个 UID，
	// 其余账号永远进不来 —— 实测 14 个账号只有 5 个被路由到。
	//
	// 但排序本身必须有确定性的平局键：cands 来自 map 遍历（顺序随机），
	// 若仅用 SliceStable 按权重排序，等权重账号的相对顺序会随 map 随机化而抖动，
	// 使「注入随机源即可复现」的契约失效。因此：
	//   - 先按 (权重降序, UID 升序) 得到确定性顺序；
	//   - 仅当截断边界上存在等权重平局（即谁入选由 UID 决定）时，才用随机洗牌
	//     把这一档打散，让被排除的账号随时间轮换。
	sort.Slice(ws, func(i, j int) bool {
		if ws[i].w != ws[j].w {
			return ws[i].w > ws[j].w
		}
		return ws[i].e.a.UID < ws[j].e.a.UID
	})
	p.shuffleTiesWs(ws)
	sort.SliceStable(ws, func(i, j int) bool { return ws[i].w > ws[j].w })
	cands = cands[:0]
	for _, c := range ws {
		cands = append(cands, c.e)
	}
	if len(cands) > shortlistSize {
		cands = cands[:shortlistSize]
	}

	eligible := make([]*entry, 0, len(cands))
	for _, e := range cands {
		if now.Sub(e.lastUsed) >= minPickGap {
			eligible = append(eligible, e)
		}
	}
	var e *entry
	if len(eligible) == 0 {
		// 候选全部刚被用过：LRU 兜底，维持发散且不 starve 任一候选。
		//
		// 比较必须用「lastUsed 时刻 + 单调序号」双键：时钟粒度（Windows 约 511µs）
		// 会让并发写入的 lastUsed 完全相同，单靠 Before() 会因平局恒 false 而
		// 永远停在 cands[0]，退化成惊群。序号更小 = 更早被选中 = 更久未用。
		e = cands[0]
		for _, c := range cands[1:] {
			if olderLastUsed(c, e) {
				e = c
			}
		}
	} else {
		e = p.pickWeightedMode(eligible, tiered) // eligible 保序 = top5 降序子集
	}
	// 先自增序号再取时间：保证「先选中者序号更小」，与上面的比较口径一致。
	p.pickSeq++
	e.lastUsedSeq = p.pickSeq
	e.lastUsed = time.Now()
	return e.a
}

// olderLastUsed 报告 a 是否比 b "更久未被选中"。
//
// 双键比较：lastUsed 时刻优先，完全相同时（时钟粒度导致）用单调序号兜底
// —— 序号更小者更早被选中，因而更久未用。
func olderLastUsed(a, b *entry) bool {
	if !a.lastUsed.Equal(b.lastUsed) {
		return a.lastUsed.Before(b.lastUsed)
	}
	return a.lastUsedSeq < b.lastUsedSeq
}

// earliestExpiryTierLocked 返回候选集中「最近到期日」最早的那一档账号，
// 以及是否真的按到期分了档（false = 全员无到期信息，调用方回退原三因子口径）。
//
// 到期日未知的账号视为最晚（排最后）：只有全部候选都没有到期信息时，
// 它们才会成为唯一的一档。调用方必须已持有 p.mu。
//
// ## 「未知到期日排最后」是**刻意**的（WorkBuddy 语境下）
//
// 见 `TestPickUnknownExpiryGoesLast`：无到期信息的账号在还有别的账号可用时
// 不该被选中。理由是在 WorkBuddy 语境下，到期日未知通常意味着该账号的积分
// 数据还没拉到（可能是死号/僵尸号），拿它接流量风险更高。
//
// ## ⚠ 但这条规则**不能跨产品套用**（多产品场景的真实 bug）
//
// 到期分层是**积分**（credits）概念的一部分：它回答"哪份积分先过期"。
// 而 **ZCode 账号根本没有积分与到期概念**（`expireAt` 恒为 0）——
// 对它套用"未知到期日排最后"是**范畴错误**：那不是"数据没拉到"，
// 而是"这个产品没有这个东西"。
//
// 实测后果（uitest/diag-zcode-debug6.cjs，19 个 WorkBuddy + 1 个 ZCode）：
// ZCode 账号虽然进了池（`/status` 可见、`byUID=20`），却**永远进不了候选集**：
//
//	[TIER-DEBUG] 候选=20 scored=19 exempt=1 byUID=20   ← 第一次（修复后可见）
//	[TIER-DEBUG] 候选=19 scored=19 exempt=0 byUID=20   ← 之后
//
// 表现为「界面显示账号正常、日志显示已载入，但请求永远走 WorkBuddy」，
// 一个从界面完全看不出来的静默故障。
//
// ## 修法：分层**只在 WorkBuddy 账号之间**生效
//
// 非 WorkBuddy 产品（Qoder / ZCode）不参与积分分层，原样进入候选集。
// 这样：
//   · WorkBuddy 的「先烧快过期」与「未知排最后」**逐字不变**（既有测试仍过）
//   · 其它产品不会被积分维度的规则误伤
//
// 之所以不改成"未知档也保留"：那会推翻 WorkBuddy 那条刻意的设计决定
//（`TestPickUnknownExpiryGoesLast` 明确要求未知档在还有别的账号时不被选中）。
// 这里要修的是**跨产品误用**，不是那条决定本身。
func (p *Pool) earliestExpiryTierLocked(cands []*entry) ([]*entry, bool) {
	if len(cands) <= 1 {
		return cands, false
	}

	// 先把候选分成「参与积分分层的」与「不参与的（其它产品）」。
	//
	// 只有参与分层的账号才有"未知到期日"这个概念。
	var scored []*entry
	var exempt []*entry
	for _, e := range cands {
		if e.a != nil && e.a.ProductOf() != auth.ProductWorkBuddy {
			exempt = append(exempt, e)
			continue
		}
		scored = append(scored, e)
	}

	// 其它产品原样保留（它们不受积分维度约束）
	keep := func(out []*entry) []*entry {
		if len(exempt) == 0 {
			return out
		}
		merged := make([]*entry, 0, len(out)+len(exempt))
		merged = append(merged, out...)
		merged = append(merged, exempt...)
		return merged
	}

	if len(scored) == 0 {
		// 全是其它产品：没有积分维度可分，退化为随机
		return cands, false
	}

	best := ""
	for _, e := range scored {
		key := e.expiryDayKey()
		if key == "" {
			continue // 未知不参与比较（WorkBuddy 语境下它们排最后，见上）
		}
		if best == "" || key < best {
			best = key
		}
	}
	if best == "" {
		// WorkBuddy 侧全员未知：不分档，退化为原三因子随机
		return cands, false
	}

	out := make([]*entry, 0, len(cands))
	for _, e := range scored {
		if e.expiryDayKey() == best {
			out = append(out, e)
		}
	}
	return keep(out), true
}

// pickEarliestExpiryLocked 全冷却兜底：在非禁用的软冷却/熔断账号中选截止最早的一个。
// 分级：disabled 永不参与；CoolHard（余额耗尽，等签到的号）同样排除——调了必 402，浪费轮换并产生噪音日志；
// CoolSoft 与熔断号允许参与（可能已恢复，失败成本仅一轮换）。
// 被 tried 排除、在途占满的账号同样跳过（维持请求级轮换 + 租约语义）。无任何可用返回 nil。
//
// model 非空时同样排除"该模型正被限流"的账号：否则兜底会把刚被 6004 拒掉的账号
// 立刻再选一次，既浪费轮换又让用户看到重复报错。若排除后无候选则返回 nil，
// 由调用方报告"全部账号该模型均受限"（比继续撞限流更有信息量）。
func (p *Pool) pickEarliestExpiryLocked(tried map[string]bool, now time.Time, model string) *auth.Auth {
	var best *entry
	for uid, e := range p.byUID {
		if tried != nil && tried[uid] {
			continue
		}
		if e.disabled {
			continue // 禁用的账号永不参与兜底
		}
		// 用户手动禁用（不接流量）同样永不参与兜底。
		//
		// 为什么必须显式写在这里：本函数**没有**调 healthy()（它自己手写筛选，
		// 因为 healthy() 会把「正在冷却」也判掉，而兜底恰恰要选冷却中的账号）。
		// 于是 healthy() 里的 NoRoute 检查在这里**不会生效** —— 实测确认：
		// 一个被标 NoRoute 且处于冷却期的账号，会被本函数选中（有回归测试钉住）。
		//
		// 现场症状：所有者把国际版账号全部标为「不接流量」（因为不禁用就没法用），
		// 国服账号因上游限流冷却后，兜底把这些他明确禁用的账号又选了回来，
		// 请求打到连不通的国际版 → 503。用户看到的是「禁用也没用」。
		if e.a != nil && e.a.NoRoute {
			continue
		}
		if e.coolKind == CoolHard && !e.until.IsZero() && now.Before(e.until) {
			continue // 余额耗尽号（处于有效 hard 冷却期）不参与兜底：等签到恢复，调了必 402
		}
		if e.modelCooled(model, now) {
			continue // 该模型正被限流：换了也是同一个 6004
		}
		if p.inFlightFull(e) {
			continue
		}
		exp := e.expiry(now)
		if exp.IsZero() {
			continue
		}
		if best == nil || exp.Before(best.expiry(now)) {
			best = e
		}
	}
	if best == nil {
		return nil
	}
	log.Printf("pool: fallback_earliest_expiry uid=%s until=%s kind=%s", best.a.UID, best.expiry(now).Format(time.RFC3339), best.fallbackKind(now))
	p.markUsed(best)
	return best.a
}

// markUsed 记录一次选中使用（lastUsed 时刻 + 单调序号）。
//
// 调用方必须已持有 p.mu。序号在此自增，保证「先选中者序号更小」，
// 使 olderLastUsed 在时钟粒度平局时仍能还原真实先后（见 entry.lastUsedSeq）。
func (p *Pool) markUsed(e *entry) {
	p.pickSeq++
	e.lastUsedSeq = p.pickSeq
	e.lastUsed = time.Now()
}

// inFlightFull 报告账号是否已占满在途名额（max=0 不限 → 恒 false）。
// 调用方需已持 p.mu（读锁或写锁均可，本方法只读 p.maxInFlight）。
func (p *Pool) inFlightFull(e *entry) bool {
	if p.maxInFlight <= 0 {
		return false
	}
	return e.inFlight.Load() >= int64(p.maxInFlight)
}

// minPickGap 防并发撞号窗口：同一账号在该窗口内不重复被选中（除非 top5 全部刚被用过）。
// 生产默认 100ms；纯加权分布测试可临时置 0 关闭防撞号。
var minPickGap = 100 * time.Millisecond

// pickWeighted 三因子加权随机（claude-api selectWeightedRandom 参考口径）：
//
//		weight = credits 比例 × 10 + idleWeight + successRate × 3
//
//	  - credits 比例 = 该号 credits / 候选集内最大 credits（避免量纲爆炸）
//	  - idleWeight = min(距 lastUsed 小时数 × idleWeightPerHour, idleWeightMax)；从未使用给满分
//	  - successRate = successCount/(successCount+errTotal)；无请求记录给 1.5（中性偏信任）
//
// credits 全 0 时仍按 idle+successRate 加权（不退化均匀随机）。
// 权重为浮点，用 int64 定点（×1e6）抽签可保持确定性随机源注入（randInt64N 语义不变）。
// 随机源优先用 p.randInt64N（仅供测试注入确定性），nil 时回退 math/rand/v2 全局源。
func (p *Pool) pickWeighted(cands []*entry) *entry {
	return p.pickWeightedMode(cands, false)
}

// pickWeightedMode 加权抽签。tiered=true 时用「组内权重」（去掉 credits 项）：
// 同一到期档内应平均使用，若仍按 credits 加权，积分多的号会被明显倾斜，
// 与「同档平均分摊」的目标相悖。tiered=false 保留原三因子口径（无到期信息时的回退）。
func (p *Pool) pickWeightedMode(cands []*entry, tiered bool) *entry {
	now := time.Now()
	var maxCredits int64
	for _, e := range cands {
		if e.credits > maxCredits {
			maxCredits = e.credits
		}
	}

	const scale = 1_000_000 // 定点放大：int64 累加权重大整数抽签
	weights := make([]int64, len(cands))
	var total int64
	for i, e := range cands {
		w := p.weightOf(e, maxCredits, now)
		if tiered {
			w = p.tierWeightOf(e, now)
		}
		weights[i] = int64(w * scale)
		total += weights[i]
	}

	rnd := rand.Int64N
	if p.randInt64N != nil {
		rnd = p.randInt64N
	}
	if total <= 0 {
		return cands[int(rnd(int64(len(cands))))]
	}
	r := rnd(total)
	var acc int64
	for i, e := range cands {
		acc += weights[i]
		if r < acc {
			return e
		}
	}
	return cands[len(cands)-1]
}

// weightedEntry 候选账号及其挑选权重（短名单排序用）。
type weightedEntry struct {
	e *entry
	w float64
}

// shuffleTiesWs 只在**截断边界上存在等权重平局**时随机洗牌，用于打破短名单的确定性偏向。
//
// 为什么要洗牌：短名单按权重降序截断，等权重账号必须有一个稳定的先后顺序，否则排序
// 结果不确定。但如果这个顺序恒为 UID 字典序，短名单就会固定包含最小的 N 个 UID，
// 其余账号永远进不来（issue #5：14 个号只用 5 个）。随机化后同权重账号可公平轮换。
//
// 为什么必须限定「边界有平局才洗」：
//   - 权重各不相同（如 credits 差异明显）时不存在平局，洗牌只会在注入确定性随机源
//     的测试里白白消耗随机数、打乱既有可复现语义，属无谓副作用；
//   - 只在真正需要打破平局时消费随机数，保持「权重高者优先」的确定性不变。
//
// 前置条件：ws 已按 (权重降序, UID 升序) 排好。调用方必须已持有 p.mu。
func (p *Pool) shuffleTiesWs(ws []weightedEntry) {
	if len(ws) <= shortlistSize {
		return // 全部候选都在短名单内，截断不会排除任何账号 → 无需洗牌
	}
	// 检查「第 shortlistSize 名」与「第 shortlistSize+1 名」是否同权重：
	// 同权重才意味着谁入选由排序顺序（UID）决定，需要随机化。
	cut := shortlistSize
	if ws[cut-1].w != ws[cut].w {
		return // 边界无平局：入榜与落榜权重不同，结果已确定，不应洗牌
	}

	rnd := rand.Int64N
	if p.randInt64N != nil {
		rnd = p.randInt64N
	}
	// Fisher-Yates 洗牌（从后往前，与 [0,i] 中随机一个交换）。
	for i := len(ws) - 1; i > 0; i-- {
		j := int(rnd(int64(i + 1)))
		if j < 0 || j > i {
			j = i // 注入源契约外返回越界值时兜底，避免 panic
		}
		ws[i], ws[j] = ws[j], ws[i]
	}
}

// tierWeightOf 到期同档内的权重：只保留闲置补偿 + 成功率，不含 credits。
//
// 为什么去掉 credits：同一到期档意味着这些额度的「紧迫度相同」，
// 按积分多少分配会让高积分账号长期吃掉大部分流量，同档内就不再是平均使用。
// 闲置补偿与成功率作为小幅微调保留：前者防止某个号被完全闲置，
// 后者让持续报错的号自然让出流量（两者量级远小于原 credits 项的 ×10）。
func (p *Pool) tierWeightOf(e *entry, now time.Time) float64 {
	w := 1.0

	// 闲置补偿（口径与 weightOf 完全一致）。
	if e.lastUsed.IsZero() {
		w += p.idleWeightMax // 从未使用 → 满分
	} else {
		hours := now.Sub(e.lastUsed).Hours()
		idleW := hours * p.idleWeightPerHour
		if idleW > p.idleWeightMax {
			idleW = p.idleWeightMax
		}
		if idleW < 0 {
			idleW = 0 // lastUsed 在未来（时钟回拨）时钳 0
		}
		w += idleW
	}

	// 成功率 ×3（口径与 weightOf 完全一致）。
	totalReq := e.successCount + e.errTotal
	if totalReq > 0 {
		w += float64(e.successCount) / float64(totalReq) * 3
	} else {
		w += 1.5 // 无请求记录 → 中性偏信任
	}

	// 成本微调（仅多产品模式）：同样是"紧迫度相同"的额度，更便宜的一份额度
	// 应该多承担一些流量。
	//
	// 为什么是**乘子**而不是加项：加项会与上面的闲置/成功率项线性叠加，
	// 而成本在跨产品场景下只应是"同等条件下略作区分"。乘子的影响随权重
	// 成比例，不会因为账号闲置久而淹没。
	//
	// 三态语义：costRate = 0（未声明）→ 乘子 1.0（中性，不惩罚）。
	// 若误把未声明当"很便宜"或"很贵"，都会让某产品被系统性冷落。
	if p.multiProductOn && p.costWeight > 0 && e.costRate > 0 {
		w *= p.costMultiplierLocked(e.costRate)
	}
	return w
}

// costMultiplierLocked 把「消耗率」映射成权重乘子。
//
// 设计取舍（见 design.md §2.3）：
//   · 消耗率越低（越便宜）→ 乘子越接近上限 1 + costWeight
//   · 消耗率越高（越贵）  → 乘子越接近下限 1 - costWeight
//
// 用 costRate/(1+costRate) 做归一化而不是线性：消耗率的量纲不确定
//（各产品自定义），线性映射会让某个产品的大数值把乘子压到 0 附近、
// 等于永久冷落它。这个映射天然落在 [0,1)，不会产生极端值。
//
// costWeight 取 0.3 左右时，最便宜与最贵之间的权重差异约 60% ——
// 足以让份额有可观测的差异，又不足以压过"同档平均分摊"。
func (p *Pool) costMultiplierLocked(costRate float64) float64 {
	if costRate <= 0 {
		return 1.0 // 未声明 → 中性
	}
	// 归一化到 [0,1)：便宜 → 接近 0，贵 → 接近 1
	norm := costRate / (1 + costRate)
	// norm=0 → 1+costWeight；norm→1 → 1-costWeight
	return 1 + p.costWeight*(1-2*norm)
}

// weightOf 计算单个账号的三因子权重。
func (p *Pool) weightOf(e *entry, maxCredits int64, now time.Time) float64 {
	w := 1.0

	// 1. credits 比例 ×10（会计入 mid-credit 锚点，避免全员 0 时 credits 项为 0）。
	if maxCredits > 0 {
		w += float64(e.credits) / float64(maxCredits) * 10
	}

	// 2. 闲置补偿。
	if e.lastUsed.IsZero() {
		w += p.idleWeightMax // 从未使用 → 满分
	} else {
		hours := now.Sub(e.lastUsed).Hours()
		idleW := hours * p.idleWeightPerHour
		if idleW > p.idleWeightMax {
			idleW = p.idleWeightMax
		}
		if idleW < 0 {
			idleW = 0 // lastUsed 在未来（时钟回拨）时钳 0
		}
		w += idleW
	}

	// 3. 成功率 ×3。
	totalReq := e.successCount + e.errTotal
	if totalReq > 0 {
		w += float64(e.successCount) / float64(totalReq) * 3
	} else {
		w += 1.5 // 无请求记录 → 中性偏信任
	}

	// 4. 成本微调（仅多产品模式）。
	//
	// ⚠ 两条路径都要加：tiered=false 时走本函数（**所有账号都无到期信息**），
	// 而 tiered=true 时走 tierWeightOf。只在其中一处加会导致
	// 「没有到期信息的池子里成本完全不生效」—— 实测就是先漏了这里，
	// 表现为份额精确 50/50（成本维度形同虚设）。
	//
	// 为什么放在最后乘：前三个因子决定"谁更该被用"，成本只在此基础上
	// 做小幅倾斜。放前面乘会让成本与 credits 项互相放大。
	if p.multiProductOn && p.costWeight > 0 && e.costRate > 0 {
		w *= p.costMultiplierLocked(e.costRate)
	}
	return w
}

// SetCredits 更新账号余额。
func (p *Pool) SetCredits(uid string, credits int64) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if e, ok := p.byUID[uid]; ok {
		e.credits = credits
		p.dirty.Store(true)
	}
}

// modelCool 单个「账号+模型」的冷却记录。
//
// 为什么按 (uid, model) 而不是按账号：上游 6004 是**模型级**限流，报错文案明确写着
// 「您也可以切换其他模型继续使用」。按账号整体冷却会连带封掉该账号其他仍可用的模型，
// 白白浪费额度。
type modelCool struct {
	until       time.Time // 冷却截止（上游给的重置时刻，或解析失败时的固定软冷却）
	reason      string    // 面向用户的说明（含模型名与重置时间）
	resetParsed bool      // true=到期时间取自上游文案；false=回退固定软冷却
}

// ModelCooling 向外部（/status、前端）描述一条模型冷却。
type ModelCooling struct {
	Model         string    `json:"model"`
	Until         time.Time `json:"until"`
	RemainingSec  int64     `json:"remaining_sec"`
	Reason        string    `json:"reason,omitempty"`
	ResetAtParsed bool      `json:"reset_at_parsed"` // true=到期时间取自上游文案；false=回退固定软冷却
}

// CooldownModel 把某个「账号+模型」冷却到指定时刻。
//
// 与 Cooldown 的区别：Cooldown 作用于整个账号（e.until），本方法只影响该账号的
// 指定模型 —— 其余模型与其余账号都不受影响。
//
// 不喂熔断器：模型级限流是**配额**信号而非账号故障，反复命中说明该模型额度确实用尽，
// 喂熔断只会把整个账号封掉，与「换模型仍可用」的事实相悖。
func (p *Pool) CooldownModel(uid, model string, until time.Time, reason string, resetParsed bool) {
	if model == "" || until.IsZero() {
		return
	}
	p.mu.Lock()
	defer p.mu.Unlock()
	e, ok := p.byUID[uid]
	if !ok {
		return
	}
	if e.modelCools == nil {
		e.modelCools = make(map[string]modelCool, 2)
	}
	e.modelCools[model] = modelCool{until: until, reason: reason, resetParsed: resetParsed}
	p.dirty.Store(true)
}

// modelCooled 报告该账号的指定模型当前是否处于冷却中。调用方必须已持锁。
func (e *entry) modelCooled(model string, now time.Time) bool {
	if model == "" || len(e.modelCools) == 0 {
		return false
	}
	mc, ok := e.modelCools[model]
	return ok && now.Before(mc.until)
}

// modelCoolingList 返回该账号当前仍在生效的模型冷却（按到期时间升序）。调用方必须已持锁。
func (e *entry) modelCoolingList(now time.Time) []ModelCooling {
	if len(e.modelCools) == 0 {
		return nil
	}
	out := make([]ModelCooling, 0, len(e.modelCools))
	for m, mc := range e.modelCools {
		if !now.Before(mc.until) {
			continue // 已过期
		}
		rem := int64(time.Until(mc.until).Seconds() + 0.999)
		if rem < 0 {
			rem = 0
		}
		out = append(out, ModelCooling{
			Model:         m,
			Until:         mc.until,
			RemainingSec:  rem,
			Reason:        mc.reason,
			ResetAtParsed: mc.resetParsed,
		})
	}
	sort.Slice(out, func(i, j int) bool {
		if !out[i].Until.Equal(out[j].Until) {
			return out[i].Until.Before(out[j].Until)
		}
		return out[i].Model < out[j].Model
	})
	return out
}

// pruneModelCoolsLocked 清掉已过期的模型冷却，避免 map 无限增长。调用方必须已持锁。
// 返回是否真的删除了条目（调用方据此决定要不要置 dirty）。
func (e *entry) pruneModelCoolsLocked(now time.Time) bool {
	if len(e.modelCools) == 0 {
		return false
	}
	changed := false
	for m, mc := range e.modelCools {
		if !now.Before(mc.until) {
			delete(e.modelCools, m)
			changed = true
		}
	}
	return changed
}

// Cooldown 冷却账号至 now+d（即时冷却：CoolSoft 429 / CoolHard 余额耗尽）。
//
// 冷却入口同时是熔断器的失败信号：喂入 fails，达到阈值按指数退避熔断（与 until 正交）。
//
// 例外（issue #5）：CoolSoft（429 限流）**不参与**指数退避的指数放大。
// 429 是瞬时过载信号，重试同一账号很快就能成功；若让它放大 retryCount，
// 一次流量高峰会把账号按 30m→1h→2h→4h→6h 逐步封死（实测 3 次 429 即封 30 分钟），
// 而该状态会落盘，重启也不恢复 —— 用户反馈的「频繁不可用 + 重启无效」即源于此。
// 软冷却仍正常累计 fails（保留「反复失败即熔断」的语义），但熔断时长不叠加放大。
func (p *Pool) Cooldown(uid string, kind CoolKind, d time.Duration, reason string) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if e, ok := p.byUID[uid]; ok {
		e.until = time.Now().Add(d)
		e.coolKind = kind
		e.reason = reason
		// 软冷却（429）不放大退避指数；硬冷却与 5xx 仍走完整熔断语义。
		p.recordBreakerFailureLockedKind(e, kind != CoolSoft)
		p.dirty.Store(true)
	}
}

// recordBreakerFailureLocked 累计一次熔断失败；达到阈值则按指数退避熔断。
// 熔断与冷却（until）解耦：冷却按错误类别给固定时长，熔断则对"反复失败"逐次加长封禁。
// 调用方必须已持有 p.mu。
func (p *Pool) recordBreakerFailureLocked(e *entry) {
	p.recordBreakerFailureLockedKind(e, true)
}

// recordBreakerFailureLockedKind 是 recordBreakerFailureLocked 的带参版本：
// escalate=false 时只累计 fails、照常触发熔断，但**不递增 retryCount**，
// 因而熔断时长不随次数指数增长（用于 429 这类瞬时错误的防放大，见 Cooldown 注释）。
// 调用方必须已持有 p.mu。
func (p *Pool) recordBreakerFailureLockedKind(e *entry, escalate bool) {
	e.fails++
	if e.fails < p.breakerThreshold {
		return
	}
	d := p.breakerCooldown
	// escalate 决定是否按「已放大次数」取退避指数；不放大时恒用首次熔断时长。
	if escalate {
		for i := 0; i < e.retryCount; i++ {
			d *= 2
			if d >= p.breakerCooldownMax {
				d = p.breakerCooldownMax
				break
			}
		}
	}
	// 触发熔断：重置失败计数供下一轮重新累计；retryCount 仅在放大时递增。
	e.fails = 0
	if escalate {
		e.retryCount++
	}
	e.breakerUntil = time.Now().Add(d)
}

// CooldownUntilTomorrow4AM 冷却到下一个 04:00（本地时区）。
// 用于 ErrHardCredit 场景：积分耗尽账号等签到任务（09:00/21:00）恢复。
func (p *Pool) CooldownUntilTomorrow4AM(uid string, reason string) {
	now := time.Now()
	p.Cooldown(uid, CoolHard, nextDay4AM(now).Sub(now), reason)
}

// nextDay4AM 返回 now 之后最近的一个 04:00（与 now 同一时区）。
// now 在当天 04:00 之前（凌晨 00:00~04:00）时返回当天 04:00——此时签到尚未执行，
// 该窗内触发的硬冷却等当天签到即可恢复；返回次日会白冷约一天。
// 04:00 整及之后返回次日 04:00。
// time.Date 对日溢出自动进位（月末→下月 1 号、年末→下年 1 号），天然覆盖跨日/跨月/跨年。
func nextDay4AM(now time.Time) time.Time {
	if now.Hour() < 4 {
		return time.Date(now.Year(), now.Month(), now.Day(), 4, 0, 0, 0, now.Location())
	}
	return time.Date(now.Year(), now.Month(), now.Day()+1, 4, 0, 0, 0, now.Location())
}

// Disable 禁用（session 死亡），需人工重登后**显式复活**才回到选号池
//（见 ReviveDisabled 与 /debug/accounts/{uid}/revive）。
//
// ⚠ 重新导入凭证**不会**解开禁用：upsertLocked 对已存在账号只换凭证、不动
// disabled。这曾是一个真实缺陷（自动禁用的账号在网关内没有任何恢复路径），
// 故复活必须是一个显式动作。
func (p *Pool) Disable(uid, reason string) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if e, ok := p.byUID[uid]; ok {
		e.disabled = true
		e.reason = reason
		p.dirty.Store(true)
	}
}

// sessionDeadThreshold 连续 ErrSessionDead（12153）达到该次数才永久禁用。
//
// 取 3 的理由：12153 会被临时性触发（网络抖动 / 上游闪断 / refresh 竞态），
// 单次即杀号会误杀健康账号。连续 3 次（跨多次保活周期）才认为是真的 session 死亡。
const sessionDeadThreshold = 3

// sessionDeadReason 禁用原因（与旧文案保持一致，便于既有运维脚本匹配）。
const sessionDeadReason = "12153 session dead"

// SessionDeadThreshold 暴露阈值（供 scheduler 日志 / 运维文档引用）。
func SessionDeadThreshold() int { return sessionDeadThreshold }

// NoteSessionDead 记录一次 ErrSessionDead（12153）——**不立即禁用**。
//
// 旧行为是「一次 12153 即 Disable」，但该错误会被临时性触发（网络抖动 /
// 上游闪断 / refresh 竞态），一次失败就永久杀号会误杀健康账号 ——
// 实测发现一批 disabled 账号其实 refresh 完全正常，是历史误判的受害者。
//
// 现改为连续 sessionDeadThreshold 次才禁用：计数 +1，达阈值则 Disable 并清计数。
// 返回 true 表示本次已达阈值并完成禁用。
//
// 清零时机见 ClearSessionDead（refresh 成功 / chat 成功 / 手工复活）。
func (p *Pool) NoteSessionDead(uid string) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	e, ok := p.byUID[uid]
	if !ok {
		return false
	}
	e.sessionDeadFails++
	p.dirty.Store(true)
	if e.sessionDeadFails < sessionDeadThreshold {
		return false
	}
	e.sessionDeadFails = 0
	e.disabled = true
	e.reason = sessionDeadReason
	return true
}

// ClearSessionDead 清零连续 12153 计数。
//
// 任何「证明账号没死」的时刻都应调用它：refresh 成功、chat 成功、手工复活。
// 不清零的话，一个月的偶发抖动累计到 3 次照样会杀号 —— 而「连续」正是本修复
// 的语义核心，累计计数会让它退化成「累计 3 次即杀」。
func (p *Pool) ClearSessionDead(uid string) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if e, ok := p.byUID[uid]; ok {
		e.sessionDeadFails = 0
	}
}

// ReviveDisabled 人工/端点复活入口：清除 disabled + reason + 连续 12153 计数，
// 账号回到池子（若无其他冷却/熔断则立即可选）。
//
// # 为什么必须有这个出口
//
// disabled 会被写进 state.json（见 stateOverviewLocked），而它此前**只有写入方**
// （Disable / NoteSessionDead 达阈值）没有任何清除入口 —— 唯一能清的是
// ReenableIfCredits，但它显式要求 `!e.disabled`（见其实现）。于是账号一旦被
// 自动判定为死号（session 死 / 额度冻结）就在网关内**永远救不回来**：
// 重新导入凭证也没用，因为 upsertLocked 对已存在账号只换凭证、不碰 disabled。
// 用户唯一的出路是手改 state.json，而那是普通用户做不到、也不该做的事。
//
// # 只解系统自动禁用，不动用户的手工轴
//
// 本方法只清 `disabled`（系统判定位）。**不碰** auth.Auth.NoRoute —— 那是用户在
// 界面上拨的「停止接流量」开关，由宿主写进凭证文件，属用户意图而非池运行态。
// 若这里顺手清了它，一次「复活」就会把用户明确摘除的号悄悄放回选号池。
// 用户要恢复流量，应去界面上关掉那个开关（宿主会重写凭证文件，见 pool 的 upsert）。
//
// 同理不清熔断（fails/retryCount/breakerUntil）与账号级冷却（until/coolKind）：
// 它们各有自己的到期/恢复路径，复活只负责「把死号重新放回候选」，不假装它健康。
//
// 返回 true 表示本次**确实改了状态**（幂等：账号本就启用、或 uid 不存在 → false）。
// 落盘走 dirty 标志（与 Disable 一致：置位后由后台 flusher 或 Flush 写盘）。
func (p *Pool) ReviveDisabled(uid string) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	e, ok := p.byUID[uid]
	if !ok || !e.disabled {
		return false
	}
	e.disabled = false
	e.reason = ""
	e.sessionDeadFails = 0
	p.dirty.Store(true)
	return true
}

// SessionDeadFails 当前连续 12153 计数（供 scheduler 日志与测试断言）。
func (p *Pool) SessionDeadFails(uid string) int {
	p.mu.RLock()
	defer p.mu.RUnlock()
	if e, ok := p.byUID[uid]; ok {
		return e.sessionDeadFails
	}
	return 0
}

// reviveCoolingLocked 只清冷却（until/coolKind/reason）并更新 credits，不动熔断器
// （fails/retryCount/breakerUntil）。签到解冻走这里：签到成功只证明余额恢复与
// billing 通道健康，不证明 chat 通道健康，熔断（连续 5xx 信号）不应被签到覆盖。
// 调用方必须已持有 p.mu。
func (p *Pool) reviveCoolingLocked(e *entry, credits int64) {
	e.credits = credits
	e.until = time.Time{}
	e.coolKind = 0
	e.reason = ""
}

// ReenableIfCredits 签到后解冻：仅当 remain > 0 且账号非禁用时，清冷却（余额恢复）。
// 注意：不碰熔断器——熔断到期（breakerUntil 过期）或下次 chat 成功（NoteSuccess）才恢复。
func (p *Pool) ReenableIfCredits(uid string, remain int64) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if e, ok := p.byUID[uid]; ok {
		if remain > 0 && !e.disabled {
			p.reviveCoolingLocked(e, remain)
		} else {
			e.credits = remain
		}
		p.dirty.Store(true)
	}
}

// NoteError 记录一次错误：喂入唯一的连续失败计数器 fails + 累计错误 errTotal。
// 达到 breakerThreshold 触发熔断（指数退避），连续失败语义整体并入熔断器（不再有独立的 err 冷却）。
func (p *Pool) NoteError(uid string) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if e, ok := p.byUID[uid]; ok {
		e.errTotal++
		e.lastErr = time.Now()
		p.recordBreakerFailureLocked(e)
		p.dirty.Store(true)
	}
}

// NoteSuccess 成功请求累加成功计数、刷新 lastSuccess，并清空连续失败与熔断运行态。
// 二进制模型：清 fails + retryCount + breakerUntil；不碰 until/coolKind（那些是即时冷却，各自到期）。
func (p *Pool) NoteSuccess(uid string) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if e, ok := p.byUID[uid]; ok {
		e.successCount++
		e.lastSuccess = time.Now()
		e.fails = 0
		e.retryCount = 0
		e.breakerUntil = time.Time{}
		// 一次 chat 成功就证明 session 没死，连续 12153 计数必须清零：
		// 不清零的话，一个月的偶发抖动会累计到阈值照样杀号 ——
		// 而「连续」正是该修复的语义核心，累计计数会让它退化成「累计 N 次即杀」。
		e.sessionDeadFails = 0
		p.dirty.Store(true)
	}
}

// Status 查询单账号状态。
func (p *Pool) Status(uid string) (Status, bool) {
	p.mu.RLock()
	defer p.mu.RUnlock()
	e, ok := p.byUID[uid]
	if !ok {
		return Status{}, false
	}
	return p.statusOf(uid, e), true
}

// AuthByUID 返回账号的完整凭证（给调度器/运维接口用）。
func (p *Pool) AuthByUID(uid string) *auth.Auth {
	p.mu.RLock()
	defer p.mu.RUnlock()
	if e, ok := p.byUID[uid]; ok {
		return e.a
	}
	return nil
}

// AllUIDs 返回池中**全部**账号的 UID（含冷却/熔断/禁用），按 UID 排序。
//
// 与 AvailableUIDs 的区别是只读观测用途：区域诊断要能看到「池里到底有哪些
// 账号、各自属于哪个区域」，若只列 healthy 账号，故障排查时最需要看的那几个
// （正在冷却的）恰好不可见。
func (p *Pool) AllUIDs() []string {
	p.mu.RLock()
	defer p.mu.RUnlock()
	uids := make([]string, 0, len(p.byUID))
	for uid := range p.byUID {
		uids = append(uids, uid)
	}
	sort.Strings(uids)
	return uids
}

// AvailableUIDs 返回当前 healthy 且未占满在途名额的账号 UID 列表（按 UID 排序，稳定输出）。
// 供会话粘性路由（internal/session）做快路径命中校验 + 双段分配；无可用返回空切片。
func (p *Pool) AvailableUIDs() []string {
	p.mu.RLock()
	defer p.mu.RUnlock()
	now := time.Now()
	uids := make([]string, 0, len(p.byUID))
	for uid, e := range p.byUID {
		if !e.healthy(now) {
			continue
		}
		if p.inFlightFull(e) {
			continue
		}
		uids = append(uids, uid)
	}
	sort.Strings(uids)
	return uids
}

// ProbeUIDs 返回**可用于只读探测**的账号 UID（按 UID 排序）。
//
// 与 AvailableUIDs 的唯一区别：**包含被用户标记「不接流量」（NoRoute）的账号**。
//
// 为什么探测要包含它们：NoRoute 的语义是「别把**用户请求**路由到它」，
// 而拉一次 `/v3/config` 是只读的、不产生任何流量或积分消耗 —— 它既不违反
// 用户意图，又恰恰是这些账号最有价值的用途。
//
// 实测踩过的坑：`pickProbeAccountInRegion` 原先用 AvailableUIDs，而我把 NoRoute
// 加进 healthy() 之后，被禁用的国际版账号**既不接流量、也拿不到区域真值了** ——
// 表现为「deepseek-v4.1-flash 明明两区都有，界面却标『仅国服』」
//（因为国际版拉不到清单，于是被当成「该区没有这个模型」）。
// 这正是「把不接流量与不作为混为一谈」的第二次犯法，与导出侧那个 bug 同源。
//
// 仍**排除** e.disabled（网关判定 session 死 / 额度冻结）：那种账号连
// /v3/config 都会失败，拿来探测只会白跑一轮并写进日志噪音。
func (p *Pool) ProbeUIDs() []string {
	p.mu.RLock()
	defer p.mu.RUnlock()
	now := time.Now()
	uids := make([]string, 0, len(p.byUID))
	for uid, e := range p.byUID {
		if e.disabled {
			continue
		}
		// 只看「未在冷却/熔断期」：探测失败多半是账号凭证问题，
		// 与冷却无关，但冷却中的账号同样会失败，跳过即可。
		if !e.until.IsZero() && now.Before(e.until) {
			continue
		}
		if !e.breakerUntil.IsZero() && now.Before(e.breakerUntil) {
			continue
		}
		if e.a == nil || e.a.AccessToken == "" {
			continue
		}
		uids = append(uids, uid)
	}
	sort.Strings(uids)
	return uids
}

// PickByUID 若 uid 当前 healthy 且未占满在途名额，返回其凭证（记录 lastUsed 防撞号）；
// 否则返回 nil。供会话粘性路由命中校验与直取使用。
func (p *Pool) PickByUID(uid string) *auth.Auth {
	return p.PickByUIDForModel(uid, "")
}

// PickByUIDForModel 同 PickByUID，但额外要求该账号的指定模型未被限流。
//
// 粘性路由必须走这里：会话已绑定的账号若正好在请求的模型上被 6004 限流，
// 直接复用会稳定撞同一个错误（用户看到"换账号也没用"）。返回 nil 让上层
// 解绑并重新分配，粘性语义与「绑定号不可用即失效」的既有约定一致。
func (p *Pool) PickByUIDForModel(uid, model string) *auth.Auth {
	p.mu.Lock()
	defer p.mu.Unlock()
	e, ok := p.byUID[uid]
	if !ok {
		return nil
	}
	now := time.Now()
	if !e.healthy(now) {
		return nil
	}
	if e.modelCooled(model, now) {
		return nil
	}
	if p.inFlightFull(e) {
		return nil
	}
	// 统一走 markUsed：与 pick() 共用同一序号空间，保证 LRU 排序跨入口一致。
	p.markUsed(e)
	return e.a
}

// PickByUIDForModelRegion 同 PickByUIDForModel，但额外要求账号属于 prefer 区域。
//
// 存在的意义：粘性会话会把后续请求固定到首次绑定的账号上，而区域偏好
// 只在「重新选号」时生效 —— 若不做这个检查，一个已绑定到国际版账号的会话
// 发图片时仍会走国际版后端，区域偏好形同虚设。
//
// 返回 nil 会让上层解绑并重新分配（与「绑定号不可用即失效」的既有约定一致）。
// prefer=RegionAny 时与 PickByUIDForModel 完全一致。
func (p *Pool) PickByUIDForModelRegion(uid, model string, prefer auth.Region) *auth.Auth {
	if prefer != auth.RegionAny {
		p.mu.RLock()
		e, ok := p.byUID[uid]
		var got auth.Region
		if ok {
			got = e.a.Region()
		}
		p.mu.RUnlock()
		if !ok || got != prefer {
			return nil
		}
	}
	return p.PickByUIDForModel(uid, model)
}

// PickByUIDForModelProductRegion 粘性会话版：额外校验**产品**是否匹配。
//
// # 为什么必须单独做（这是我实测发现的漏洞）
//
// `forward.go` 的选号顺序是「先试粘性会话绑定的账号，绑不上才走产品过滤」：
//
//	acct = PickByUIDForModelRegion(stickyUID, model, preferRegion)  ← 这一支没有产品校验
//	if acct == nil { acct = pickAccountFor(..., product) }          ← 产品过滤在这里
//
// 于是**只要该会话此前绑过一个账号**，`zcode:glm-5.3` 会继续用那个
// 绑定的账号 —— 哪怕它是 WorkBuddy 的。实测确认（
// `uitest/verify-model-prefix-live.cjs`）：
//
//	`zcode:glm-5.3` → 选中 e2891116（**workbuddy**）→ 报"额度已耗尽"
//
// 用户看到的是"我明明指定了 zcode，却报了 WorkBuddy 账号的错"。
//
// 故粘性路径也必须校验产品：不匹配就返回 nil，让上层解绑并走
// 带产品过滤的重新分配（那正是"绑定号不可用即失效"的既有约定）。
//
// # `product == ""` 时与 PickByUIDForModelRegion 完全一致
func (p *Pool) PickByUIDForModelProductRegion(
	uid, model string, prefer auth.Region, product string,
) *auth.Auth {
	if product != "" {
		p.mu.RLock()
		e, ok := p.byUID[uid]
		var got string
		if ok && e.a != nil {
			// 用 ProductOf()：老账号的 Product 是空串，语义上等价 workbuddy
			got = e.a.ProductOf()
		}
		p.mu.RUnlock()
		if !ok || got != product {
			return nil
		}
	}
	return p.PickByUIDForModelRegion(uid, model, prefer)
}

// CountsDetailed 返回 total/healthy/cooling/disabled/inFlightFull 五类计数。
// cooling 含常规冷却（until）与熔断期（breakerUntil）。
// 注意：healthy 口径不含 inFlight 维度（是状态机权威判定，只看 disabled/until/breakerUntil）；
// inFlightFull 是 healthy 的子集——healthy 里已达在途上限的账号数，供 /status 透出满载度。
// 与 ServableNow 的区别见该函数注释。
//
// 计数口径与 healthy 的分支**必须对齐**，否则界面上的数字会互相矛盾：
// 用户标记「不接流量」的账号（no_route）既不是 disabled（没死），也不是 cooling
//（没在冷却）—— 它在 healthy() 里为 false，若不单独分一支就会被算进 cooling，
// 于是界面上「冷却 N 个」凭空多出几个根本没冷却的号。
// 单独归入 disabled 这一支：两者对**可用性**的含义相同（都不接流量），
// 界面再按 NoRoute 标记分别显示不同文案。
func (p *Pool) CountsDetailed() (total, healthy, cooling, disabled, inFlightFull int) {
	p.mu.RLock()
	defer p.mu.RUnlock()
	now := time.Now()
	for _, e := range p.byUID {
		total++
		switch {
		case e.disabled || (e.a != nil && e.a.NoRoute):
			// no_route 与 disabled 同归「不可用」：都不参与选号。
			// 区分它们的是 Status.NoRoute（界面据此显示「不接流量」vs「已停用」）。
			disabled++
		case !e.healthy(now):
			cooling++
		default:
			healthy++
			if p.inFlightFull(e) {
				inFlightFull++
			}
		}
	}
	return total, healthy, cooling, disabled, inFlightFull
}

// ServableNow 报告池当前是否可服务：存在至少一个 healthy 且未占满在途名额的账号。
// 与 CountsDetailed 的 healthy 口径不同：healthy 只看 disabled/until/breakerUntil（状态机权威判定），
// 不看 inFlight；ServableNow 额外叠加在途维度，与 chat 的真实可达性（Pick 会跳过 inFlightFull 账号）对齐。
// 专供 /healthz 用，避免"全账号 healthy 但都占满"时探活误报 200 而 chat 返回 503 的口径裂缝。
func (p *Pool) ServableNow() bool {
	p.mu.RLock()
	defer p.mu.RUnlock()
	now := time.Now()
	for _, e := range p.byUID {
		if e.healthy(now) && !p.inFlightFull(e) {
			return true
		}
	}
	return false
}

// List 返回所有账号状态（按 UID 排序，稳定输出）。
func (p *Pool) List() []Status {
	p.mu.RLock()
	defer p.mu.RUnlock()
	now := time.Now()
	uids := make([]string, 0, len(p.byUID))
	for uid := range p.byUID {
		uids = append(uids, uid)
	}
	sort.Strings(uids)

	// 先算出「当前会被路由的那一档」，供每个账号标注是否在排队。
	//
	// 为什么必须由后端算：前端拿不到档位规则（要复刻 healthy + 模型冷却 + 到期分层
	// 三套判定），而用「success_count == 0」之类的近似会误判 —— 新导入的账号、
	// 或一直失败的账号同样是 0，但它们不是排队。只有用与选号**完全相同**的口径，
	// 「排队中」才是可信的事实而非猜测。
	routedDay := p.routedTierDayLocked(now)
	out := make([]Status, 0, len(uids))
	for _, uid := range uids {
		st := p.statusOf(uid, p.byUID[uid])
		if routedDay != "" && st.ExpireDay > routedDay {
			// 到期日更晚 ⇒ 不在当前档位 ⇒ 排队等待
			st.Queued = true
		}
		out = append(out, st)
	}
	return out
}

// routedTierDayLocked 返回当前实际会被路由的到期日档位（空串 = 无法确定）。
//
// 口径与 pick 保持一致：剔除禁用、账号级冷却、在途占满的账号，取「最早到期日」那一档。
//
// 关于**模型冷却**（这里是本实现踩过的坑，务必保留这条排除）：
// 档位本身是账号级属性，但「当前是否轮得到」取决于**本次请求的模型**。实测现场：
// 4 个更早档位的账号对主力模型 deepseek-v4.1-flash 处于模型冷却，网关于是跳过它们、
// 把流量给了更晚档位的 8 个账号。若这里不排除模型冷却账号，就会算出「最早档位是
// 10-01」，进而把真正在服务的 8 个账号全标成「排队」，还把冷却中的账号标成「会路由」
// —— 与事实完全相反。已由 TestQueuedFlagMatchesLiveScenario 锁定。
//
// 以「是否存在至少一个模型处于冷却」作为排除依据：只要有模型在冷却，该账号对那个
// 模型就不可用，不能代表「轮得到」。这是保守估计，对单模型为主的用法足够准确。
func (p *Pool) routedTierDayLocked(now time.Time) string {
	best := ""
	for _, e := range p.byUID {
		if e.disabled || !e.healthy(now) || p.inFlightFull(e) {
			continue
		}
		// 有任一模型在冷却 ⇒ 该账号当前可能整体不可用，不参与档位判定
		if len(e.modelCoolingList(now)) > 0 {
			continue
		}
		key := e.expiryDayKey()
		if key == "" {
			continue // 到期日未知：排最后，不参与档位判定
		}
		if best == "" || key < best {
			best = key
		}
	}
	return best
}

func (p *Pool) statusOf(uid string, e *entry) Status {
	now := time.Now()
	st := Status{
		UID:             uid,
		Nickname:        e.a.Nickname,
		Credits:         e.credits,
		Cooling:         now.Before(e.until) || now.Before(e.breakerUntil),
		Reason:          e.reason,
		Disabled:        e.disabled,
		NoRoute:         e.a != nil && e.a.NoRoute,
		Product:         productOf(e.a),
		SuccessCount:    e.successCount,
		ErrTotal:        e.errTotal,
		LastSuccessTime: e.lastSuccess,
		LastErrTime:     e.lastErr,
		Until:           e.until,
		InFlight:        int(e.inFlight.Load()),
		BreakerFails:    e.fails,
		BreakerUntil:    e.breakerUntil,
		SoonestExpireAt: e.expireAt,
		ExpireDay:       e.expiryDayKey(),
		ModelCooling:    e.modelCoolingList(now),
	}
	if st.Cooling {
		// 剩余秒数与类型都必须按**真正生效的那个截止**计算，不能只看 e.until。
		//
		// 两个截止正交：until 是按错误类别的即时冷却，breakerUntil 是连续失败的指数
		// 退避熔断。熔断触发时 until 可能早已归零（甚至从未设置），此时唯一生效的是
		// breakerUntil。旧实现按 time.Until(e.until) 算剩余、按 e.coolKind 报类型，
		// 于是熔断中的账号显示成「冷却中 · 剩余 0 秒 · 余额不足」——即使它余额充足，
		// 用户据此完全无法判断该等多久、以及到底为什么被停用（Issue #5 截图现场：
		// 7161 积分的账号显示「冷却中」且剩余 0 秒）。
		//
		// 注意 Until 字段保持原语义（= e.until 这个即时冷却截止）不变：它参与持久化
		// 口径且被既有契约测试依赖；「何时恢复」的权威答案由 CoolRemaining 与
		// BreakerUntil 共同表达。
		deadline := e.recoveryAt(now)
		st.CoolRemaining = int64(time.Until(deadline).Seconds() + 0.999)
		if st.CoolRemaining < 0 {
			st.CoolRemaining = 0
		}
		// 熔断主导恢复时刻时报 breaker，否则才是 until 的即时冷却类型。
		if e.breakerGovernsRecovery(now) {
			st.CoolKind = "breaker"
		} else {
			st.CoolKind = e.coolKind.String()
		}
	}
	return st
}

// recoveryAt 返回账号「何时恢复可选」——两个生效截止中**较晚**的那个。
//
// 与 expiry() 的区别（不可混用）：
//   - healthy() 要求 until 与 breakerUntil **都**已过期（两者是 AND 关系），
//     所以真正恢复的时刻是较晚者。expiry() 取的是较早者（供全冷却兜底挑
//     "最快有可能恢复"的账号去试），语义不同。
//   - 状态画像要回答用户"还要等多久"，必须用本函数。
// 不在冷却期时返回零值。
func (e *entry) recoveryAt(now time.Time) time.Time {
	var t time.Time
	if !e.until.IsZero() && now.Before(e.until) {
		t = e.until
	}
	if !e.breakerUntil.IsZero() && now.Before(e.breakerUntil) {
		if t.IsZero() || e.breakerUntil.After(t) {
			t = e.breakerUntil
		}
	}
	return t
}

// breakerGovernsRecovery 报告「账号何时恢复」是否由熔断截止决定（即 breakerUntil
// 是两者中较晚、因而真正卡住恢复的那个）。
//
// 与 fallbackKind 的比较方向**恰好相反**，不可复用：
//   - fallbackKind 服务于全冷却兜底，expiry() 取的是较早截止（兜底挑"最快有可能
//     恢复"的号去试），故它在 breakerUntil **更早**时报 breaker；
//   - 本函数服务于状态画像，要回答"还要等多久"，恢复取决于**较晚**的截止。
func (e *entry) breakerGovernsRecovery(now time.Time) bool {
	if e.breakerUntil.IsZero() || !now.Before(e.breakerUntil) {
		return false
	}
	// until 已失效，或熔断截止不早于 until → 熔断是较晚者，主导恢复。
	return e.until.IsZero() || !now.Before(e.until) || !e.breakerUntil.Before(e.until)
}

// ---------------------------------------------------------------------------
// 持久化
// ---------------------------------------------------------------------------

func (p *Pool) load() {
	raw, err := os.ReadFile(p.stateFp)
	if err != nil {
		return
	}
	var sf stateFile
	if json.Unmarshal(raw, &sf) != nil {
		return
	}
	p.applyAccountsLocked(sf.Accounts)
}

// applyAccountsLocked 用持久化账号状态覆盖/插入 byUID（placeholder 凭证，Add 时换全）。
// 本地 load() 与 Redis 快照恢复共用；调用方必须已持有 p.mu。
func (p *Pool) applyAccountsLocked(accounts map[string]stateAccount) {
	for uid, s := range accounts {
		// err_total 优先；旧文件的 err_count（连续错误）作一次性迁移源映射进来（二者取较大者，
		// 尽最大可能保留历史观测信号——旧语义下 err_count 也真实发生过错误，不应丢）。
		errTotal := s.ErrTotal
		if int64(s.ErrCount) > errTotal {
			errTotal = int64(s.ErrCount)
		}
		p.byUID[uid] = &entry{
			a:            &auth.Auth{UID: uid}, // placeholder，Add 时会换成完整凭证
			credits:      s.Credits,
			disabled:     s.Disabled,
			reason:       s.Reason,
			until:        s.Until,
			coolKind:     s.CoolKind,
			successCount: s.SuccessCount,
			errTotal:     errTotal,
			lastErr:      s.LastErr,
			lastSuccess:  s.LastSuccess,
			expireAt:     s.ExpireAt,
			modelCools:   restoreModelCools(s.ModelCools),
		}
	}
}

// applySnapshotLocked 用 Redis 快照覆盖内存状态（已在择新判定后采用）。调用方必须已持有 p.mu。
func (p *Pool) applySnapshotLocked(s snapshot) {
	p.byUID = map[string]*entry{}
	p.applyAccountsLocked(s.Accounts)
}

func (p *Pool) saveLocked() {
	if p.stateFp == "" {
		return
	}
	sf := p.stateOverviewLocked()
	raw, err := json.MarshalIndent(sf, "", "  ")
	if err != nil {
		p.notePersistFail(err)
		return
	}
	if dir := filepath.Dir(p.stateFp); dir != "" {
		_ = os.MkdirAll(dir, 0o755)
	}
	tmp := p.stateFp + ".tmp"
	if err := os.WriteFile(tmp, raw, 0o600); err != nil {
		p.notePersistFail(err)
		return
	}
	if err := os.Rename(tmp, p.stateFp); err != nil {
		p.notePersistFail(err)
		return
	}
	if p.persistFails > 0 {
		// 从连续失败中恢复：打一条恢复日志，避免"错误打完却无人知道已恢复"。
		log.Printf("pool: state.json 落盘恢复（此前连续失败 %d 次）", p.persistFails)
		p.persistFails = 0
	}

	// 同步镜像一份快照到 Redis（fire-and-forget），与本地 state.json 并存作恢复备份。
	if p.store != nil {
		snapRaw, err := json.Marshal(snapshot{stateFile: sf, SavedAt: time.Now()})
		if err == nil {
			p.store.SaveState(snapRaw)
		}
	}
}

// notePersistFail 记录一次本地 state.json 落盘失败，并按节流规则决定是否打日志：
// 首败（状态成功→失败）打完整错误、每 persistLogEvery 次连续失败打一条提醒、
// 其余连续失败静默（flusher 5s 一把，磁盘持续满时不刷屏）。
// 恢复成功的日志由 saveLocked 在成功路径统一打。与 redisstore 三处异步写的
// "失败仅打日志、不向上抛"范式对齐，但落盘失败对运维是盲区，故多一层节流（notification）。
func (p *Pool) notePersistFail(err error) {
	if p.persistFails == 0 {
		log.Printf("pool: state.json 落盘失败: %v", err)
	} else if p.persistFails%persistLogEvery == 0 {
		log.Printf("pool: state.json 连续落盘失败 %d 次: %v", p.persistFails, err)
	}
	p.persistFails++
}

// stateOverviewLocked 收集当前内存状态为 stateFile（供落盘 + 快照镜像复用）。调用方必须已持 p.mu。
func (p *Pool) stateOverviewLocked() stateFile {
	sf := stateFile{Accounts: map[string]stateAccount{}}
	for uid, e := range p.byUID {
		sf.Accounts[uid] = stateAccount{
			Credits:      e.credits,
			Disabled:     e.disabled,
			Reason:       e.reason,
			Until:        e.until,
			CoolKind:     e.coolKind,
			SuccessCount: e.successCount,
			ErrTotal:     e.errTotal,
			LastSuccess:  e.lastSuccess,
			LastErr:      e.lastErr,
			ExpireAt:     e.expireAt,
			ModelCools:   persistModelCools(e.modelCools),
		}
	}
	return sf
}

// persistModelCools 把内存态模型冷却转成持久化形态；空表返回 nil（JSON 里省略该字段）。
func persistModelCools(in map[string]modelCool) map[string]stateModelCool {
	if len(in) == 0 {
		return nil
	}
	out := make(map[string]stateModelCool, len(in))
	for m, mc := range in {
		out[m] = stateModelCool{Until: mc.until, Reason: mc.reason, ResetParsed: mc.resetParsed}
	}
	return out
}

// restoreModelCools 把持久化形态还原成内存态；空表返回 nil。
func restoreModelCools(in map[string]stateModelCool) map[string]modelCool {
	if len(in) == 0 {
		return nil
	}
	out := make(map[string]modelCool, len(in))
	for m, sc := range in {
		out[m] = modelCool{until: sc.Until, reason: sc.Reason, resetParsed: sc.ResetParsed}
	}
	return out
}
