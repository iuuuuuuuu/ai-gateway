// Package scheduler 定时任务：每日签到（09/21点，末尾顺带派猫/领奖）+ token keepalive（22点）。
// 签到成功后重新查余额，余额 > 0 的冷却账号自动解冻。
package scheduler

import (
	"context"
	"errors"
	"fmt"
	"log"
	"strings"
	"sync"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/pool"
	"workbuddy2api/internal/records"
	"workbuddy2api/internal/upstream"
)

// Config 调度器依赖。
//
// 任务开关用「禁用」命名而非「启用」：零值 Config 即两类任务都启用，
// 与引入开关前的行为逐字一致（老调用方/老测试无需改动）。
type Config struct {
	Pool           *pool.Pool
	Upstream       *upstream.Client
	CheckinHours   []int // 默认 [9, 21]
	KeepaliveHours []int // 默认 [22]
	// ActivityHours 活跃上报时点，默认 [10]。
	//
	// 为什么要单独一个任务：签到只恢复余额，**连登天数**要靠对话活跃上报点亮。
	// 一条 chat_request_send 同时点亮连登 + 解锁 first_buddy（领养前置），
	// 因此它是「能领养」的前提。
	ActivityHours []int
	// TrialHours 国际版 trial 加油包领取时点，默认 [9, 21]（与签到同步）。
	//
	// 国际版没有签到/任务中心，trial 是其唯一的积分增益动作；
	// 上游用 14051 表达「已领过」，客户端视为幂等成功，故可每天重试。
	TrialHours []int
	// SchoolHours 开学季活动任务时点，默认 [12]。
	//
	// 该活动是**限时**的：服务端下发 in_period，下线后自动跳过，
	// 代码无需人工清理。只领取已达标的奖励，不伪造完成动作。
	SchoolHours []int
	// NightOwlHours 夜猫子任务时点，默认 [1]。
	//
	// growth 有个时段敏感任务只在夜猫窗口（23:00–08:00 CST）内计入，
	// 故单独排一个落在窗口内的时点（01 点避开 22 点的 token 保活）。
	NightOwlHours []int

	// CheckinDisabled 显式关闭签到排程（对应 config 的 schedule.checkin_enabled=false）。
	// 禁用后不再有任何签到时点，搭签到便车的猫猫旅行也随之停摆。
	CheckinDisabled bool
	// KeepaliveDisabled 显式关闭 token 保活排程（schedule.keepalive_enabled=false）。
	KeepaliveDisabled bool
	// ActivityDisabled 显式关闭活跃上报排程（schedule.activity_enabled=false）。
	ActivityDisabled bool
	// NightOwlDisabled 显式关闭夜猫子排程（schedule.nightowl_enabled=false）。
	NightOwlDisabled bool
	// SchoolDisabled 显式关闭开学季活动排程（schedule.school_enabled=false）。
	SchoolDisabled bool
	// TrialDisabled 显式关闭 trial 领取排程（schedule.trial_enabled=false）。
	TrialDisabled bool

	// QoderClaimHours Qoder 权益活动的每日领取时点，默认 [10, 21]。
	//
	// # 所有者要求（2026-09-22）
	//
	//	「qoder改为 早十点,晚九点 两次触发,防止错漏」
	//
	// # 为什么要两个时点（不是"多此一举"）
	//
	// 活动**每天 10:00（UTC+8）重置**，且单条时限约 22 小时。
	// 只排一个时点的话，任何一次抖动都会让**当天彻底领不到**：
	//
	//	· 上游 5xx / 网络超时
	//	· 恰好在该时点前已跑过（时点未到就重置了）
	//	· 网关当时没在运行
	//
	// 两个时点互相兜底：10 点是重置后第一轮，21 点再确认一次。
	// 端点幂等（已领会回 `replayed:true`），故重复执行无副作用。
	QoderClaimHours []int
	// QoderClaimDisabled 显式关闭 Qoder 自动领取排程。
	//
	// 对应 `schedule.qoder_claim_enabled=false` **或**总闸
	// `schedule.product_tasks_enabled=false`（两级回落见 Config.QoderClaimOn）。
	QoderClaimDisabled bool


	// RunProductTasks 产品日常任务的**执行体**，由网关自己实现（见 main.go）。
	//
	// # ⚠ 为什么不是"宿主注入"（我第一版设计错了，此处记录以免再犯）
	//
	// 我最初按 `RunTaskFor` 的模式设计成"宿主注入执行体"（依赖倒置）。
	// 但读了架构后发现**方向是反的**：
	//
	//	Rust 宿主  ──HTTP POST /tasks/run──▶  Go 网关
	//
	// 即宿主调网关，网关**从不回调宿主**（两个独立进程）。
	// 故"宿主注入"根本无人可注入 —— 那会变成一段永远为 nil 的死代码。
	//
	// 而实际上**网关自己就能做**：`internal/zcode` 有完整的客户端与
	// 凭证读取（`zcode.NewDispatch(zcode.New())` 已在 main.go 接线），
	// 凭证目录由宿主通过配置透传（`pool.zcode_auth_dir`）。
	//
	// 故本字段改由 main.go 用自己的实现填充，不再是"等宿主注入"。
	//
	// 返回一句可读的结果描述（写进任务记录）。
	// nil = 该构建未接线（排程直接跳过，不报错）。
	RunProductTasks func() (detail string, err error)

	// ActivityReportCount 每个账号每日上报条数，默认 3（与官方客户端行为接近）。
	// 多条共用同一 conversationId，requestId 各自独立。
	ActivityReportCount int

	// CheckinScope 签到 + 猫猫旅行 + 活跃上报覆盖的账号区域："cn"（缺省，仅国服）/ "all"。
	//
	// 国际版（workbuddy.ai）的 billing 与 growth 接口暂无真实数据，默认跳过；
	// token 保活不受此限制（两个区域都需要刷新）。
	CheckinScope string

	// Records 账号记录写入器（nil = 不记录）。
	//
	// 为什么要它：这 4 个任务的日志此前只走 log.Printf 写 stdout，而宿主启动
	// 网关子进程时把 stdout/stderr 丢进了 Stdio::null —— 任务照跑，但界面上
	// 一条执行痕迹都没有（用户报的正是这个现象）。改为写宿主已经在读的
	// account_records.json，记录就能与签到并列出现在「账号记录 → 任务」里。
	Records *records.Recorder
}

// checkinScopeAllows 该账号是否参与签到与猫猫旅行。
func (s *Scheduler) checkinScopeAllows(a *auth.Auth) bool {
	if strings.EqualFold(strings.TrimSpace(s.cfg.CheckinScope), "all") {
		return true
	}
	return !upstream.IsIntl(a)
}

// 手动触发用的任务名。用字符串而非 taskKind 暴露给宿主：
// taskKind 是内部排程的实现细节（顺序会随排程重构变化），
// 而宿主（GUI/webui）需要的是稳定的对外标识。
const (
	TaskNameActivity = "activity"
	TaskNameNightOwl = "nightowl"
	TaskNameSchool   = "school"
	TaskNameTrial    = "trial"
	// TaskNameGrowthMap 活跃地图闭环（补签/兑换/抽奖/礼包）。
	//
	// 它没有独立排程时点（并入活跃上报，见 growthmap.go 顶部说明），
	// 但仍注册为可手动触发的任务名：日排程每个账号一天只跑一轮，
	// 用户想立刻确认「我的补签卡/抽奖次数有没有被处理」时需要一个入口。
	TaskNameGrowthMap = "growthmap"

	// TaskNameProductTasks 其它产品（Qoder / ZCode）的**日常任务**。
	//
	// # 所有者要求（2026-09-20）
	//
	//	「qoder这个活动卡片…而且任务也应该自动执行」
	//	「他那个仓库还有个自动领取那个积分包的功能，我们也要接进来」
	//
	// 这两条都是"别让我每天手点" —— 日常任务自动做掉。
	//
	// # ⚠ 与我自己此前写下的风控结论的关系
	//
	// `internal/zcode/claim.go` 的文件头写着「**不做定时自动抢** ——
	// 那会让账号表现出非人类的活动模式」。那条结论**仍然成立**，
	// 它反对的是「自动**抢**」= 高频探测 + 抢限量名额
	//（参考实现默认 5 分钟一轮，因为限量套餐先到先得）。
	//
	// 而所有者要的是**自动做掉每天的幂等任务**，两者不是一回事：
	//
	//	抢：  高频（5 分钟）、有竞争、失败要重试   ← 那才是非人类画像
	//	做：  每天一次、幂等（已领会返回"已领过"）、失败不重试
	//
	// 故本实现刻意**与参考实现的 5 分钟轮询不同**：
	//
	//	· 每天固定时点跑一次（与签到/活跃上报同一套排程机制）
	//	· **串行**：同一时刻只跑一个产品，避免并发指纹
	//	· **零重试**：失败就等下一个时点，不做退避重试
	//	· 两个端点都是幂等的（Qoder `replayed:true` / ZCode `1003`），
	//	  故"重复执行"本身不产生副作用
	//
	// 执行体由**宿主**提供（依赖倒置）：Qoder/ZCode 的凭证与接口都在宿主侧，
	// 网关不持有它们，与 `RunTaskFor` 的既有做法一致。
	TaskNameProductTasks = "product_tasks"
)

// ErrTaskRunning 该任务已有一轮手动触发在执行中。
//
// 为什么需要去重：一轮活跃上报按账号数 × 条数 × 间隔串行跑，大账号池下可长达数分钟。
// 用户连点「立即执行」若不拦，会叠加出成倍的重复上报 —— 那正是活跃上报刻意
// 控制条数要规避的风控画像。
var ErrTaskRunning = errors.New("task already running")

// Scheduler 调度器。
type Scheduler struct {
	cfg Config

	// mu/adoptTried 领养当日失败记录：uid → 自然日（CST）。门槛未达的账号当日不再重试，
	// 避免同日多趟对上游重试轰炸；进程重启即清零（无需持久化）。
	//
	// growthClaimed 同一把锁下的「活跃地图当日已跑」标记（uid → 自然日 CST）。
	// 两类任务都是「每日一轮」的养号动作，用同一把锁即可：它们只在写各自 map 时短暂持有，
	// 不跨网络请求，不会把排程拖慢。
	mu             sync.Mutex
	adoptTried     map[string]string
	growthClaimed  map[string]string

	// taskMu/taskBusy 手动触发的在跑标记。与 mu 分开：排程循环会自动跑同一批任务，
	// 共用一把锁会让「手动触发」与「到点执行」互相阻塞，把定时任务拖慢。
	taskMu   sync.Mutex
	taskBusy map[string]bool
}

// TaskRunResult 手动触发一轮任务的结果。
//
// 为什么要回报「跑了没有」而不只是成功/失败：这些任务都有前置条件
// （夜猫子限时段、开学季限活动期、活跃上报与开学季只跑国服），
// 不满足时**跳过是正确行为**而非错误。宿主据此在界面上说明原因，
// 否则用户点了「立即执行」看不到任何变化，会以为功能坏了。
type TaskRunResult struct {
	Task string `json:"task"`
	// Ran 是否真的执行了一轮（false = 被前置条件挡下）。
	Ran bool `json:"ran"`
	// Skip 跳过原因码；Ran=true 时为空。
	//
	// 用稳定的英文码而非直接给中文：宿主可据此分支（如高亮提示），
	// 文案留给 Message，改文案不会破坏调用方的判断。
	Skip string `json:"skip,omitempty"`
	// Message 面向用户的中文说明，可直接显示在界面上。
	Message string `json:"message"`
}

// claimTask 尝试认领某个任务；已被认领时返回 false。
//
// 手动触发与到点排程共用一组标记：它们在跑同一批上游请求，若互不感知，
// 用户恰好在 10:00 点「立即执行」就会与排程那轮叠成双倍上报 ——
// 正是活跃上报刻意控制条数要规避的风控画像。
func (s *Scheduler) claimTask(name string) bool {
	s.taskMu.Lock()
	defer s.taskMu.Unlock()
	if s.taskBusy[name] {
		return false
	}
	s.taskBusy[name] = true
	return true
}

// releaseTask 归还认领。
func (s *Scheduler) releaseTask(name string) {
	s.taskMu.Lock()
	delete(s.taskBusy, name)
	s.taskMu.Unlock()
}

// RunTaskByName 手动触发一轮指定任务（供宿主「立即执行」按钮）。
//
// 只做「立刻跑一轮」，不检查 enabled 开关：开关管的是**后台是否自动排程**，
// 用户主动点击就该执行 —— 与宿主侧 checkin_all / travel_run 的既有语义一致。
//
// 未知任务名返回错误而非静默成功：宿主拼错名字时必须能看见，否则按钮点了没反应。
func (s *Scheduler) RunTaskByName(name string) (TaskRunResult, error) {
	return s.RunTaskFor(name, "")
}

// RunTaskFor 触发指定任务；accountUID 非空时**只作用于该账号**。
//
// 为什么需要按账号跑（所有者明确要求）：
// 「养号任务」原先只能作用于全部账号 —— 用户在某个账号的菜单里点
// 「活跃上报」，跑的却是整池。那与菜单的位置所暗示的语义相反
//（他在那张卡片上操作，期望影响那张卡片）。
// 现在账号菜单走本入口（单账号），右上角「一键操作」走 RunTaskByName（全账号）。
//
// 区域不符时**提前回报原因**而不是静默跑空：单账号触发时用户盯着结果，
// 什么都不发生比一句说明糟糕得多。共用 checkinScopeAllows / IsIntl 的口径，
// 保证「为什么这个号不参与」与排程侧的说法一致。
func (s *Scheduler) RunTaskFor(name, accountUID string) (TaskRunResult, error) {
	switch name {
	case TaskNameActivity, TaskNameNightOwl, TaskNameSchool, TaskNameTrial, TaskNameGrowthMap:
	case TaskNameProductTasks:
		// 产品日常任务（Qoder/ZCode）：账号作用域**不适用** ——
		// 它按产品遍历，且凭证在宿主侧（网关不知道哪些 uid 属于哪个产品）。
		// 故这里不给它做 accountScopeSkip 预检，直接交给宿主执行体；
		// 宿主自己知道该跑哪些账号。
		return s.runProductTasksFor(name)
	default:
		return TaskRunResult{}, fmt.Errorf("unknown task %q", name)
	}

	// 单账号模式先做可执行性检查（拿不到账号 / 区域不符都是**确定的**结论，
	// 不必占用任务锁，也不必让用户等一轮）。
	if accountUID != "" {
		a := s.cfg.Pool.AuthByUID(accountUID)
		if a == nil {
			return TaskRunResult{
				Task: name, Skip: "account_not_found",
				Message: "该账号不在网关账号池中（可能已禁用、需重新登录或尚未同步）",
			}, nil
		}
		if msg, ok := s.accountScopeSkip(name, a); !ok {
			return TaskRunResult{Task: name, Skip: "region_mismatch", Message: msg}, nil
		}
	}

	if !s.claimTask(name) {
		return TaskRunResult{
				Task: name, Skip: "already_running",
				Message: "该任务正在执行中，请稍后再试",
			},
			ErrTaskRunning
	}
	defer s.releaseTask(name)

	// 夜猫子只在夜猫窗口内计入：窗口外触发会被 runNightOwl 直接跳过，
	// 与其让用户白等一轮然后什么都没发生，不如提前回报原因。
	if name == TaskNameNightOwl && !withinNightWindow() {
		// ⚠ 这里**必须补一条记录**，因为下面会 `return` —— 根本不进
		// runNightOwl，故写在那里的留痕是**死代码**。
		//
		// 我第一版就写错了位置（加在 runNightOwl 的窗口外分支里），
		// 测试直接把它揪出来：`手动触发夜猫子…实际 0 条`。
		// 这正是"先写失败测试再改"的价值：位置错了一眼可见。
		//
		// 所有者原话（2026-09-20）：「workbuddy的夜猫子任务也不知道到底
		// 执行了没，任务记录里面也没有」。他正是在账号卡片上点的按钮 ——
		// 那个动作走的就是本函数。
		//
		// 为什么这里可以放心写：手动触发是**用户主动行为**，一次点击一条
		// 记录是合理的；而自动排程在窗口外根本不会被排入（见 runCareTask），
		// 故不必担心刷屏。
		const nightOwlSkipMsg = "当前不在夜猫子时段（23:00–08:00 北京时间），上游不计入本次上报"
		s.cfg.Records.TaskAllDaily(name, records.ResultInfo, nightOwlSkipMsg)
		if accountUID != "" {
			// 账号级记录：整轮汇总**不带 accountId**，用户在账号卡片的
			// 记录里按账号筛选时看不到它，于是"点了却什么都没发生"。
			s.cfg.Records.Task(accountUID, "夜猫子任务", records.ResultInfo, nightOwlSkipMsg)
		}
		return TaskRunResult{
			Task:    name,
			Skip:    "outside_window",
			Message: nightOwlSkipMsg,
		}, nil
	}

	// 把账号作用域放进 ctx：各任务的遍历循环只需加一行
	// `if !inAccountScope(ctx, st.UID) { continue }`，排程路径不带作用域、
	// 行为逐字不变（不需要为每个任务再写一份「单账号版」实现）。
	ctx := context.Background()
	if accountUID != "" {
		ctx = withAccountScope(ctx, accountUID)
	}

	switch name {
	case TaskNameActivity:
		s.runActivity(ctx)
	case TaskNameNightOwl:
		s.runNightOwl(ctx)
	case TaskNameSchool:
		s.runSchool(ctx)
	case TaskNameTrial:
		s.runTrial(ctx)
	case TaskNameGrowthMap:
		s.runGrowthMap(ctx)
	}

	// 单账号模式回报具体账号，让界面能把结果落到那张卡片上。
	if accountUID != "" {
		return TaskRunResult{Task: name, Ran: true, Message: "已触发该账号的一轮"}, nil
	}
	return TaskRunResult{Task: name, Ran: true, Message: "已触发一轮"}, nil
}

// accountScopeSkip 该任务能否作用于这个账号；ok=false 时 msg 说明原因。
//
// 口径与各任务遍历循环里的区域过滤**逐条对应** —— 两处若不一致，
// 就会出现「预检说能跑、实际跑空」的矛盾。
//
// # ⚠⚠ 产品闸门（2026-09-22 修的真实缺陷：Qoder/ZCode 跑了 WorkBuddy 的任务）
//
// 所有者现场：Qoder 账号的记录里出现「开学季活动」「活跃上报」「夜猫子任务」，
// ZCode 也一样（原话：「这个qoder怎么执行workbuddy的任务了?」
// 「zcode我看到记录里面,也会跑workbuddy的任务,这不要串任务好吗?」）。
//
// # 根因
//
// 本函数此前**只判区域**（国服/国际版），**完全不判产品**。
// 而所有养号任务都是遍历 `Pool.List()` 的（见 activity.go / nightowl.go /
// growthmap.go / school.go），那个列表包含**所有产品**的账号。
//
// 于是 Qoder / ZCode 账号被当成 WorkBuddy 账号：
//
//	· 打到 WorkBuddy 的 growth 端点 —— 带着 Qoder 的凭证 ⇒ **401**
//	  （记录里那条「开学季活动…upstream client (http 401)」就是它）
//	· 在账号记录里写下**根本不属于它**的任务记录 ⇒ 用户以为账号有问题
//
// ⚠ 为什么 `checkinScopeAllows` 拦不住：它只看 `upstream.IsIntl(a)`，
// 而 Qoder/ZCode 账号的 Domain **不是** workbuddy.ai ⇒ 被判成"国服" ⇒ 放行。
// 区域与产品是**两个正交的维度**，不能用其中一个代替另一个。
//
// ⚠ 这些任务是 **WorkBuddy 专属**语义（签到、猫猫、活跃上报、夜猫子、
// 开学季、活跃地图）—— 它们打的是 WorkBuddy 的接口，用 WorkBuddy 的
// 事件模型。Qoder/ZCode 有自己的任务体系（权益活动 / 套餐领取，
// 走 `TaskNameProductTasks`），**不该混进来**。
//
// 判据用 `ProductOf()`（它把空值与 "workbuddy" 都归成 workbuddy），
// 而不是 `a.Product == ""` —— 后者会漏掉显式写了 "workbuddy" 的账号。
func (s *Scheduler) accountScopeSkip(name string, a *auth.Auth) (string, bool) {
	// ---- 产品闸门：以下任务全是 WorkBuddy 专属 ----
	//
	// 白名单式判断（列"谁能跑"而不是"谁不能跑"）：将来新增产品时
	// 默认**不参与**，而不是默认参与 —— 前者是安全的失败方向。
	switch name {
	case TaskNameActivity, TaskNameNightOwl, TaskNameSchool, TaskNameGrowthMap, TaskNameTrial:
		if a.ProductOf() != auth.ProductWorkBuddy {
			return "该任务是 WorkBuddy 专属：其它产品有自己的任务体系", false
		}
	}

	switch name {
	case TaskNameTrial:
		// trial 只跑国际版（国服无此端点），与 runTrial 的过滤相反。
		if !upstream.IsIntl(a) {
			return "trial 加油包仅国际版可用：国服没有这个端点", false
		}
	default:
		// 活跃上报 / 夜猫子 / 开学季 / 活跃地图都只跑国服
		//（国际版 growth 接口暂无真实数据），与各 run* 的 checkinScopeAllows 一致。
		if !s.checkinScopeAllows(a) {
			return "该任务是国服专属：国际版没有对应的数据接口", false
		}
	}
	return "", true
}

// accountScopeKey ctx 里承载「只跑这个账号」的键。
type accountScopeKey struct{}

// withAccountScope 把任务限定到单个账号。
func withAccountScope(ctx context.Context, uid string) context.Context {
	return context.WithValue(ctx, accountScopeKey{}, uid)
}

// inAccountScope 报告 uid 是否在当前作用域内。
//
// 无作用域（排程、或「作用于全部账号」的手动触发）时**恒为 true** ——
// 这是本机制能与既有排程共存的关键：排程路径不设作用域，行为逐字不变。
func inAccountScope(ctx context.Context, uid string) bool {
	scoped, _ := ctx.Value(accountScopeKey{}).(string)
	return scoped == "" || scoped == uid
}

// accountScopeUID 返回被限定的账号 uid；空串 = 未限定。
//
// # 用途：区分「手动触发某个账号」与「自动排程」
//
// 两者的记录策略必须不同（所有者 2026-09-20 反馈「我手动执行了但是查看
// 记录却没有」）：
//
//	手动（非空）—— 用户盯着结果 ⇒ 无论成败都要留**账号级**记录
//	自动（空）  —— 没人看 ⇒ 只在"有新变化"时写，避免把记录刷成噪音
//
// 判据就是"有没有人在看"，而这里正是那个信息的唯一来源。
// 抽成具名函数而不是让各任务直接读 ctx：语义在调用点一眼可见，
// 也避免每个任务各自 `_, ok := ctx.Value(...)` 写错键。
func accountScopeUID(ctx context.Context) string {
	scoped, _ := ctx.Value(accountScopeKey{}).(string)
	return scoped
}


// runCareTask 到点执行一个养号任务，并与手动触发互斥。
//
// 与手动触发的唯一区别：排程这轮被占用时**记一行日志就走**，不排队。
// 排队会让「迟到唤醒 + 用户刚点过立即执行」叠成两轮；而下一个整点很快就会再来，
// 漏掉一轮的代价远小于双倍上报。
func (s *Scheduler) runCareTask(name string, run func()) {
	if !s.claimTask(name) {
		log.Printf("%s: skipped (a manual run is already in progress)", name)
		return
	}
	defer s.releaseTask(name)
	run()
}

// New 构建。
func New(cfg Config) *Scheduler {
	if len(cfg.CheckinHours) == 0 {
		cfg.CheckinHours = []int{9, 21}
	}
	if len(cfg.KeepaliveHours) == 0 {
		cfg.KeepaliveHours = []int{22}
	}
	if len(cfg.ActivityHours) == 0 {
		cfg.ActivityHours = []int{10}
	}
	if len(cfg.NightOwlHours) == 0 {
		cfg.NightOwlHours = []int{1}
	}
	if len(cfg.SchoolHours) == 0 {
		cfg.SchoolHours = []int{12}
	}
	if len(cfg.TrialHours) == 0 {
		cfg.TrialHours = []int{9, 21}
	}
	if cfg.ActivityReportCount <= 0 {
		cfg.ActivityReportCount = defaultActivityReportCount
	}
	return &Scheduler{
		cfg:           cfg,
		adoptTried:    make(map[string]string),
		growthClaimed: make(map[string]string),
		taskBusy:      make(map[string]bool),
	}
}

// nextFire 返回 now 之后最近的一个整点触发时间；hours 为本地小时（0-23）。
func nextFire(now time.Time, hours []int) time.Time {
	var earliest time.Time
	for _, h := range hours {
		t := time.Date(now.Year(), now.Month(), now.Day(), h, 0, 0, 0, now.Location())
		if !t.After(now) {
			t = t.Add(24 * time.Hour)
		}
		if earliest.IsZero() || t.Before(earliest) {
			earliest = t
		}
	}
	return earliest
}

// taskKind 调度任务类型。
type taskKind int

const (
	taskCheckin taskKind = iota
	taskKeepalive
	taskActivity
	taskNightOwl
	taskSchool
	taskTrial
	// taskQoderClaim Qoder 权益活动领取（2026-09-22 新增）。
	//
	// ⚠ 它与 `taskTrial`/`taskCheckin` 等**不是**同一类：那些跑 WorkBuddy
	// 的账号任务，这个跑的是 Qoder 的活动领取。放在同一个 slot 列表里
	// 是因为**排程机制**（时点 → nextFire）完全一样，复用它最省事，
	// 也保证"到点该跑什么"只有一个真相来源。
	taskQoderClaim
)

// nextWake 返回 now 之后最近的唤醒时刻，以及该时刻需要执行的全部任务。
// 多类任务若配到同一小时（如签到与旅行都含 9），该时刻需一并执行。
// 已显式禁用的任务不进候选（nextFire 对其零值返回零时间，nextWake 再跳过零时点）。
func (s *Scheduler) nextWake(now time.Time) (time.Time, []taskKind) {
	type slot struct {
		at   time.Time
		kind taskKind
	}
	var slots []slot
	if !s.cfg.CheckinDisabled {
		slots = append(slots, slot{nextFire(now, s.cfg.CheckinHours), taskCheckin})
	}
	if !s.cfg.KeepaliveDisabled {
		slots = append(slots, slot{nextFire(now, s.cfg.KeepaliveHours), taskKeepalive})
	}
	if !s.cfg.ActivityDisabled {
		slots = append(slots, slot{nextFire(now, s.cfg.ActivityHours), taskActivity})
	}
	if !s.cfg.NightOwlDisabled {
		slots = append(slots, slot{nextFire(now, s.cfg.NightOwlHours), taskNightOwl})
	}
	if !s.cfg.SchoolDisabled {
		slots = append(slots, slot{nextFire(now, s.cfg.SchoolHours), taskSchool})
	}
	if !s.cfg.TrialDisabled {
		slots = append(slots, slot{nextFire(now, s.cfg.TrialHours), taskTrial})
	}
	// Qoder 权益活动领取（2026-09-22 新增，所有者要求早晚两次）。
	//
	// ⚠ 复用同一套 slot 机制而不是另起一个循环：`nextWake` 是"到点该跑什么"
	// 的**唯一真相来源**，加一个独立 ticker 就会有两个调度器各算各的，
	// 到点时可能同时触发、也可能互相错开（前者并发、后者漏跑）。
	if !s.cfg.QoderClaimDisabled {
		slots = append(slots, slot{nextFire(now, s.cfg.QoderClaimHours), taskQoderClaim})
	}
	var earliest time.Time
	for _, sl := range slots {
		if sl.at.IsZero() {
			continue
		}
		if earliest.IsZero() || sl.at.Before(earliest) {
			earliest = sl.at
		}
	}
	if earliest.IsZero() {
		return time.Time{}, nil
	}
	var kinds []taskKind
	for _, sl := range slots {
		if !sl.at.IsZero() && sl.at.Equal(earliest) {
			kinds = append(kinds, sl.kind)
		}
	}
	return earliest, kinds
}

// Run 主循环，阻塞直到 ctx 取消。
func (s *Scheduler) Run(ctx context.Context) {
	for {
		next, kinds := s.nextWake(time.Now())
		if next.IsZero() {
			// 所有任务全部禁用：不空转，只等退出信号。
			<-ctx.Done()
			return
		}
		timer := time.NewTimer(time.Until(next))
		select {
		case <-ctx.Done():
			timer.Stop()
			return
		case <-timer.C:
			// 到点任务在排程时确定（不依赖唤醒时刻的小时数），迟到唤醒也不会漏跑。
			for _, k := range kinds {
				switch k {
				case taskCheckin:
					s.RunCheckinNow()
				case taskKeepalive:
					s.RunKeepaliveNow()
				case taskActivity:
					s.runCareTask(TaskNameActivity, func() { s.runActivity(ctx) })
				case taskNightOwl:
					s.runCareTask(TaskNameNightOwl, func() { s.runNightOwl(ctx) })
				case taskSchool:
					s.runCareTask(TaskNameSchool, func() { s.runSchool(ctx) })
				case taskTrial:
					s.runCareTask(TaskNameTrial, func() { s.runTrial(ctx) })
				case taskQoderClaim:
					// Qoder 权益领取（2026-09-22 新增）。
					//
					// 复用 `runProductTasks`（那条路径本来就在，只是此前
					// 没有任何排程时点指向它）—— 它内部走 `RunProductTasks`
					// 执行体，会同时处理 Qoder 与 ZCode，且自带
					// claimTask/releaseTask 去重与「任务留痕」。
					//
					// ⚠ 不新建执行体：两条路径各写一份领取逻辑必然分叉
					//（"手动能领、自动领不到"这类问题就是这么来的）。
					s.runCareTask(TaskNameProductTasks, func() { s.runProductTasks(ctx) })
				}
			}
		}
	}
}

// DefaultCreditRefreshInterval 积分到期巡检的默认周期。
//
// 为什么需要独立于签到的高频巡检：到期日决定选号优先级，而它会随消费变化
// （快过期的额度烧完后，该账号的最近到期日跳到下一档，应立刻让出流量）。
// 签到每天只跑两次，间隔太久会让分层选号长期依据过时数据。
// 15 分钟 × 账号数 的请求量相对上游可忽略，且巡检本身不签到、不改账号状态。
const DefaultCreditRefreshInterval = 15 * time.Minute

// creditRefreshGap 巡检账号之间的间隔，避免瞬间并发打满上游。
const creditRefreshGap = 300 * time.Millisecond

// RunCreditRefreshLoop 周期性刷新所有账号的积分余额与到期日，阻塞直到 ctx 取消。
//
// interval <= 0 时用 DefaultCreditRefreshInterval。
func (s *Scheduler) RunCreditRefreshLoop(ctx context.Context, interval time.Duration) {
	if interval <= 0 {
		interval = DefaultCreditRefreshInterval
	}
	// 启动先跑一轮：否则重启后要等一个周期才拿到到期日，
	// 这段时间内分层选号只能依赖 state.json 里持久化的旧值。
	s.refreshCreditsWithGap(ctx)
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			s.refreshCreditsWithGap(ctx)
		}
	}
}

// refreshCreditsWithGap 跑一轮积分巡检，账号之间留出间隔；ctx 取消时提前退出。
func (s *Scheduler) refreshCreditsWithGap(ctx context.Context) {
	for i, st := range s.cfg.Pool.List() {
		if ctx.Err() != nil {
			return
		}
		if st.Disabled {
			continue
		}
		a := s.cfg.Pool.AuthByUID(st.UID)
		if a == nil || a.RefreshToken == "" {
			continue
		}
		// ⚠ 产品闸门（2026-09-22）：下面这条出站请求打的是 **WorkBuddy** 端点。
		// 不判产品时，Qoder/ZCode 账号会被带着自己的凭证打过去 ⇒ 401，
		// 并在账号记录里留下不属于它的错误（所有者现场：
		// 「这个qoder怎么执行workbuddy的任务了?」）。
		if a.ProductOf() != auth.ProductWorkBuddy {
			continue
		}
		if i > 0 {
			select {
			case <-ctx.Done():
				return
			case <-time.After(creditRefreshGap):
			}
		}
		info, err := s.cfg.Upstream.UserResourceDetail(a)
		if err != nil {
			log.Printf("credit-refresh %s: %v", st.UID, err)
			continue
		}
		s.cfg.Pool.SetCreditsAndExpiry(st.UID, info.Remain, info.SoonestExpireAt)
		if info.Remain > 0 {
			// 余额恢复的账号顺带复活，避免硬冷却的号空等到下一个签到时点。
			s.cfg.Pool.ReenableIfCredits(st.UID, info.Remain)
		}
	}
}

// RunCheckinNow 立即对所有账号执行签到 + 余额刷新 + 解冻，末尾顺带跑一趟猫猫旅行。
// 冷却中的账号也参与（签到就是为了解冻它们）；禁用的跳过。
// 区域范围（cfg.CheckinScope，默认仅国服）之外的账号跳过签到与旅行。
//
// 旅行搭签到便车而非独立排程：每日上限按「派出」计 1 次/天且在派出时锁定奖励，
// 晚领不丢分，故分钟粒度巡检无增益，与签到时点（09/21 点）合并执行即可。
// 注意顺序：先签到解冻，旅行才能覆盖到本轮刚恢复的账号。
func (s *Scheduler) RunCheckinNow() {
	for _, st := range s.cfg.Pool.List() {
		if st.Disabled {
			continue
		}
		a := s.cfg.Pool.AuthByUID(st.UID)
		if a == nil || a.RefreshToken == "" {
			continue
		}
		// ⚠ 产品闸门（2026-09-22）：签到是 WorkBuddy 专属。
		// 此前只判区域，于是 Qoder/ZCode 账号也被拿去签到 ——
		// 带错凭证打到 WorkBuddy 端点必 401，且往账号记录里写脏数据。
		if a.ProductOf() != auth.ProductWorkBuddy {
			continue
		}
		if !s.checkinScopeAllows(a) {
			continue
		}
		if err := s.cfg.Upstream.DailyCheckin(a); err != nil {
			log.Printf("checkin %s: %v", st.UID, err)
			// 已签到等业务错误也继续走余额查询
		}
		info, err := s.cfg.Upstream.UserResourceDetail(a)
		if err != nil {
			log.Printf("user-resource %s: %v", st.UID, err)
			continue
		}
		// 一次请求同时取回余额与「最近到期」：到期日驱动账号池的分层选号，
		// 顺带回写凭证文件，让宿主（workbuddy-switch）也能看到最新到期信息。
		s.cfg.Pool.SetCreditsAndExpiry(st.UID, info.Remain, info.SoonestExpireAt)
		s.cfg.Pool.ReenableIfCredits(st.UID, info.Remain)
	}
	// 签到收尾（09/21 点）：顺带推进一趟旅行状态机（领养 / 派出 / 领奖）。
	s.RunTravelNow()
}

// RunCreditRefreshNow 立即刷新所有账号的积分余额与到期日（不签到、不解冻）。
//
// 与签到的分工：签到是「每天两次」的重操作（含旅行），而到期日会随消费实时变化，
// 需要更高频地刷新才能让分层选号跟上（某账号把快过期额度烧完后，
// 它的最近到期日会跳到下一档，此时就应让出流量给更紧迫的账号）。
func (s *Scheduler) RunCreditRefreshNow() {
	for _, st := range s.cfg.Pool.List() {
		if st.Disabled {
			continue
		}
		a := s.cfg.Pool.AuthByUID(st.UID)
		if a == nil || a.RefreshToken == "" {
	continue
}
		// ⚠ 产品闸门（2026-09-22）：下面这条出站请求打的是 **WorkBuddy** 端点。
		// 不判产品时，Qoder/ZCode 账号会被带着自己的凭证打过去 ⇒ 401，
		// 并在账号记录里留下不属于它的错误（所有者现场：
		// 「这个qoder怎么执行workbuddy的任务了?」）。
		if a.ProductOf() != auth.ProductWorkBuddy {
			continue
		}
		info, err := s.cfg.Upstream.UserResourceDetail(a)
		if err != nil {
			log.Printf("credit-refresh %s: %v", st.UID, err)
			continue
		}
		s.cfg.Pool.SetCreditsAndExpiry(st.UID, info.Remain, info.SoonestExpireAt)
		// 余额耗尽时不在此解冻（那是签到的职责）；但余额恢复的账号顺带复活，
		// 避免硬冷却的号要等到下一个签到时点才回到池中。
		if info.Remain > 0 {
			s.cfg.Pool.ReenableIfCredits(st.UID, info.Remain)
		}
	}
}

// RunKeepaliveNow 立即对所有账号刷新 token。
//
// session 死亡走**连续计数**语义（见 pool.NoteSessionDead）：一次刷新失败不再
// 立即杀号 —— 12153 会被网络抖动/上游闪断临时触发，一次即禁用会误杀健康账号。
// 连续 pool.SessionDeadThreshold() 次才禁用；刷新成功则清零计数，
// 因此被误判的账号有复活路径。
func (s *Scheduler) RunKeepaliveNow() {
	for _, st := range s.cfg.Pool.List() {
		if st.Disabled {
			continue
		}
		a := s.cfg.Pool.AuthByUID(st.UID)
		if a == nil || a.RefreshToken == "" {
			continue
		}
		// ⚠ 产品闸门（2026-09-22）：下面这条出站请求打的是 **WorkBuddy** 端点。
		// 不判产品时，Qoder/ZCode 账号会被带着自己的凭证打过去 ⇒ 401，
		// 并在账号记录里留下不属于它的错误（所有者现场：
		// 「这个qoder怎么执行workbuddy的任务了?」）。
		if a.ProductOf() != auth.ProductWorkBuddy {
			continue
		}
		if err := s.cfg.Upstream.RefreshToken(a); err != nil {
			var ue *upstream.Error
			if errors.As(err, &ue) && ue.Kind == upstream.ErrSessionDead {
				if s.cfg.Pool.NoteSessionDead(st.UID) {
					log.Printf("keepalive %s: session dead x%d, disabled (re-login required)",
						uid8(st.UID), pool.SessionDeadThreshold())
				} else {
					// 未达阈值：保留账号，下轮再判（误判防护）
					log.Printf("keepalive %s: session dead %d/%d (not disabling yet): %v",
						uid8(st.UID), s.cfg.Pool.SessionDeadFails(st.UID),
						pool.SessionDeadThreshold(), err)
				}
			} else {
				log.Printf("keepalive %s: %v", uid8(st.UID), err)
			}
			continue
		}
		// 刷新成功 = 账号确实未死：清零连续 12153 计数（复活路径）。
		s.cfg.Pool.ClearSessionDead(st.UID)
		if err := a.SaveAtomic(); err != nil {
			log.Printf("keepalive %s save: %v", uid8(st.UID), err)
		}
	}
}


// ---------------------------------------------------------------------------
// 产品日常任务（Qoder 活动 / ZCode claim）
// ---------------------------------------------------------------------------

// runProductTasksFor 手动触发入口（供 `POST /tasks/run?task=product_tasks`）。
//
// 与排程路径共用同一个执行体，只多把结果包成 TaskRunResult 回给界面。
func (s *Scheduler) runProductTasksFor(name string) (TaskRunResult, error) {
	if s.cfg.RunProductTasks == nil {
		return TaskRunResult{
			Task: name, Skip: "unsupported",
			Message: "该构建未接入产品任务执行体",
		}, nil
	}
	if !s.claimTask(name) {
		return TaskRunResult{
			Task: name, Skip: "already_running",
			Message: "该任务正在执行中，请稍后再试",
		}, ErrTaskRunning
	}
	defer s.releaseTask(name)

	detail, err := s.cfg.RunProductTasks()
	if err != nil {
		// 失败也要留痕（所有者要求「任务一定要留痕」）。
		//
		// ⚠ 用 TaskAllDaily（整轮汇总、不带 accountId）而不是逐账号：
		// 执行体内部按产品/账号遍历，它才知道每条结果属于谁；
		// 网关在这里只知道"整轮的结果"。逐账号的细节由宿主执行体自己写
		//（它有 account_records 的完整能力）。
		s.cfg.Records.TaskAllDaily("产品日常任务", records.ResultFailed, err.Error())
		return TaskRunResult{Task: name, Message: err.Error()}, err
	}
	s.cfg.Records.TaskAllDaily("产品日常任务", records.ResultSuccess, detail)
	return TaskRunResult{Task: name, Ran: true, Message: detail}, nil
}

// runProductTasks 排程路径：到点自动执行一轮产品日常任务。
//
// # 与参考实现的差异（刻意，见 TaskNameProductTasks 的说明）
//
//	参考实现：每 5 分钟轮询、抢限量名额、失败退避重试
//	本实现：  每天一次、**零重试**、失败就等下一个时点
//
// 理由是"抢"与"做"是两件事：所有者要的是"别让我每天手点"（做），
// 而高频探测+重试才是非人类画像（抢）。两个端点都幂等，
// 故"今天已经领过了"是最坏情况，不需要重试去争。
func (s *Scheduler) runProductTasks(_ context.Context) {
	if s.cfg.RunProductTasks == nil {
		return
	}
	detail, err := s.cfg.RunProductTasks()
	if err != nil {
		// ⚠ **不重试**：失败就等明天那个时点。这是与参考实现最重要的差异，
		// 也是本功能不扩大风控面的关键 —— 重试风暴正是把账号打进风控的
		// 典型特征（我们在 ZCode 上亲身经历过）。
		log.Printf("product_tasks: %v", err)
		s.cfg.Records.TaskAllDaily("产品日常任务", records.ResultFailed, err.Error())
		return
	}
	if detail != "" {
		log.Printf("product_tasks: %s", detail)
	}
	s.cfg.Records.TaskAllDaily("产品日常任务", records.ResultSuccess, detail)
}


// SetProductTasksRunner 注入产品日常任务的执行体。
//
// # 为什么用 setter 而不是配置字段（2026-09-20）
//
// 执行体需要 `zcode.Dispatch`，而那个变量在 `main.go` 里的赋值位置
// **晚于** `scheduler.New`（初始化顺序所致）。若走 `scheduler.Config`，
// 调用点会引用一个还没声明的变量（编译期就报 `undefined`）。
//
// 用 setter 让"先建调度器、后接线执行体"成为合法顺序，
// 而不必为了一个字段去挪动一大片初始化代码。
//
// ⚠ 必须在 `Run(ctx)` **之前**调用：排程循环启动时读一次该字段来算唤醒时点；
// 之后注入不会让本轮的唤醒计划生效（要等下一个整点重算）。
// main.go 的调用位置满足这一点（在 `go sch.Run(ctx)` 之前）。
func (s *Scheduler) SetProductTasksRunner(fn func() (string, error)) {
	s.cfg.RunProductTasks = fn
}
