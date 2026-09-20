package zcode

// claim_scheduler.go 套餐自动领取的调度器 —— **按参考实现的实际逻辑实现**。
//
// # 所有者要求（2026-09-20）
//
//	「zcode要按照实际逻辑去做啊，他那个仓库怎么做我们就怎么做」
//
// 这纠正了我上一轮的做法：我当时按自己的判断"保守化"成"每天一次"，
// 而参考实现（TriDefender/zcode-api `src/claim/scheduler.ts`）的实际逻辑是
// **启动即跑 + 5 分钟轮询 + 动态 hold/cooldown 退避**。
//
// # 参考实现的实际调度语义（逐条照搬）
//
// 参数：
//
//	pollIntervalMs = 300_000   （5 分钟，正常轮询）
//	cooldownMs     = 600_000   （10 分钟，失败冷却）
//	启动即跑（scheduleNext(0)）
//
// 每次 tick 的决策表：
//
//	情形                                       动作            下次间隔
//	─────────────────────────────────────────  ──────────────  ─────────────────
//	now < holdUntil                            skipped_hold    剩余 hold（**不发请求**）
//	取 JWT 失败 / 无 JWT                        error           cooldown
//	preview 404                                idle            正常轮询
//	plans 为空                                  idle            正常轮询
//	configured planId 不在列表                  idle            正常轮询
//	取验证码失败                                error           cooldown
//	claim 抛异常                                error           cooldown
//	claim 成功                                  claimed         hold 到 ends_at
//	already_claimed / quota_exhausted           failed          hold 到 failureEndsAt（≤24h）
//	ineligible / unavailable / not_found        failed          cooldown
//	captcha / unknown / 网络                    failed          cooldown
//	login_required                              failed          **永久停止**
//
// # 三条必须照搬的硬约束
//
//	① **串行单飞**：上一个 tick 完全结束才排下一个（参考实现用
//	     `setTimeout`-in-`finally` 链）。本实现用单 goroutine 循环保证。
//	② **holdUntil 是硬闸**：未到期**连 preview 都不发**。
//	    这一条最关键 —— 没有它，"5 分钟轮询"会变成对上游的持续探测。
//	③ **claim 零重试**：失败只等下一个窗口（cooldown 或 failureEndsAt），
//	    **绝不立刻重试**。
//
// # 业务码 → 失败种类（镜像桌面端）
//
//	1001 not_found        套餐不存在（活动未上线）
//	1002 unavailable      当前不可领 / 活动已结束
//	1003 already_claimed  **已经领过**（不是错误，目标已达成）
//	1004 ineligible       账号无资格（含 appVersion 低于门槛）
//	1005 quota_exhausted  每日名额已满（先到先得）
//	3001 invalid_request  缺参数（最常见：缺 X-Device-Mid）
//	3007 captcha          需要/验证码失败
//	401  login_required   凭证失效 → **永久停止**
//
// # 与对话路径的关系
//
// 本调度器**只做领取**（`billing/preview` + `billing/claim`），
// 不碰对话端点 —— 两者独立，领取失败不影响对话可用性。
import (
	"context"
	"errors"
	"fmt"
	"log"
	"strings"
	"sync"
	"time"
)

// 参考实现的调度参数（逐条照搬，不自行调整）。
const (
	// ClaimPollInterval 正常轮询间隔（参考实现 `pollIntervalMs`）。
	ClaimPollInterval = 5 * time.Minute
	// ClaimCooldown 失败后的冷却（参考实现 `cooldownMs`）。
	ClaimCooldown = 10 * time.Minute
	// ClaimHoldCap `failureEndsAt` 的 hold 上限（参考实现为 24h）。
	//
	// 为什么要有上限：上游若给一个很远的 `ends_at`（如活动持续一个月），
	// 不作上限就再也不会探测 —— 而中途可能上新别的套餐。
	ClaimHoldCap = 24 * time.Hour
)

// ClaimFailureKind 领取失败的种类（镜像参考实现的 `failureKind`）。
//
// ⚠ 这是**跨实现契约**：种类决定下次轮询间隔，改取值要同步改 tick 的 switch。
type ClaimFailureKind string

const (
	ClaimFailNone           ClaimFailureKind = ""
	ClaimFailNotFound       ClaimFailureKind = "not_found"       // 1001
	ClaimFailUnavailable    ClaimFailureKind = "unavailable"     // 1002
	ClaimFailAlreadyClaimed ClaimFailureKind = "already_claimed" // 1003
	ClaimFailIneligible     ClaimFailureKind = "ineligible"      // 1004
	ClaimFailQuotaExhausted ClaimFailureKind = "quota_exhausted" // 1005
	ClaimFailInvalidRequest ClaimFailureKind = "invalid_request" // 3001
	ClaimFailCaptcha        ClaimFailureKind = "captcha"         // 3007
	ClaimFailLoginRequired  ClaimFailureKind = "login_required"  // 401
	ClaimFailHTTPError      ClaimFailureKind = "http_error"
	ClaimFailUnknown        ClaimFailureKind = "unknown"
)

// ClaimTickOutcome 一次 tick 的结果（镜像参考实现的 outcome）。
type ClaimTickOutcome string

const (
	ClaimOutcomeClaimed     ClaimTickOutcome = "claimed"
	ClaimOutcomeIdle        ClaimTickOutcome = "idle"
	ClaimOutcomeSkippedHold ClaimTickOutcome = "skipped_hold"
	ClaimOutcomeFailed      ClaimTickOutcome = "failed"
	ClaimOutcomeError       ClaimTickOutcome = "error"
)

// ClaimTickResult 一次 tick 的完整结果（供日志与「立即领取」的界面回报）。
type ClaimTickResult struct {
	Outcome ClaimTickOutcome
	Kind    ClaimFailureKind
	UID     string
	PlanID  string
	Code    string
	Msg     string
	// NextIn 本次 tick 之后要等多久再跑（已算入 hold/cooldown）。
	NextIn time.Duration
}

// ClaimScheduler 套餐自动领取的调度器。
//
// # 生命周期
//
//	NewClaimScheduler(...) → go s.Run(ctx)
//
// `Run` 启动即跑一次（与参考实现 `scheduleNext(0)` 一致），
// 之后按动态间隔循环。ctx 取消即退出。
type ClaimScheduler struct {
	client *Client
	// loadCreds 取全部凭证（由 Dispatch 提供，复用既有加载逻辑）。
	loadCreds func() ([]*Cred, []string, error)
	// enabled 返回是否启用（读配置，支持运行时改）。
	enabled func() bool

	// mu 保护 hold 状态与停止标记。
	mu sync.Mutex
	// holdUntil 硬闸：在此之前**连 preview 都不发**。
	//
	// 按账号记录（uid → 时刻）：一个账号领到了不该影响别的账号探测。
	holdUntil map[string]time.Time
	// stopped 永久停止（`login_required` 触发，需重新登录）。
	//
	// 参考实现的行为：`login_required` → `stop()`，**不重试**。
	// 理由是凭证失效重试一万次也不会好，只会增加上游的失败计数。
	stopped bool

	// lastResult 最近一次 tick 的结果（供界面/诊断读）。
	lastResult ClaimTickResult

	// now 可注入的时钟（测试用；nil = time.Now）。
	now func() time.Time
}

// NewClaimScheduler 构造调度器。
//
// `loadCreds` 与 `enabled` 由调用方注入：前者复用 `Dispatch.LoadCreds`
// （凭证目录的解析口径只该有一处），后者让配置改动能即时生效。
func NewClaimScheduler(
	c *Client,
	loadCreds func() ([]*Cred, []string, error),
	enabled func() bool,
) *ClaimScheduler {
	if c == nil {
		c = New()
	}
	return &ClaimScheduler{
		client:    c,
		loadCreds: loadCreds,
		enabled:   enabled,
		holdUntil: map[string]time.Time{},
		now:       time.Now,
	}
}

// ClaimOnce 处理**单个账号**的一轮领取（手动与自动共用）。
//
// # 为什么抽成公开方法
//
// 手动触发（界面「立即领取」）与自动调度必须走**同一套**领取逻辑：
// 各写一份必然出现"手动能领、自动领不到"这类分歧，而那种缺陷极难查
//（两条路径看起来都对，只是行为不同）。
//
// ⚠ 手动路径**绕过 hold**（用户要"现在就看"），但仍受幂等保护 ——
// 已领过的仍回 `already_claimed`，不会被重复领取。
//
// `sched` 可为 nil（纯手动场景没有调度器）：此时不读也不写 hold。
func (d *Dispatch) ClaimOnce(ctx context.Context, cr *Cred, sched *ClaimScheduler) ClaimTickResult {
	if d == nil || d.client == nil || cr == nil {
		return ClaimTickResult{Outcome: ClaimOutcomeError, Kind: ClaimFailUnknown,
			Msg: "zcode dispatch 未初始化"}
	}
	if sched == nil {
		// 临时造一个只用一次的（不共享 hold），供手动路径用。
		//
		// ⚠ `tickAccount` 的返回顺序是 **(结果, 下次等待)** ——
		// 我第一版写成了 `_, res :=`，把 Duration 当成结果返回，
		// 编译期报 `cannot use res (variable of int64 type time.Duration)
		// as ClaimTickResult value`。
		tmp := NewClaimScheduler(d.client, nil, nil)
		res, _ := tmp.tickAccount(ctx, cr)
		return res
	}
	res, _ := sched.tickAccount(ctx, cr)
	return res
}

// Run 启动即跑一次，然后按动态间隔循环（参考实现的 `scheduler.start()`）。
//
// ⚠ **串行单飞**：整个循环是单 goroutine，且每次 tick 内也逐账号串行 ——
// 任何时刻最多一个 claim 在飞（参考实现用 setTimeout 链达到同样效果）。
func (s *ClaimScheduler) Run(ctx context.Context) {
	if s.loadCreds == nil {
		return
	}
	// 启动即跑（参考实现 `scheduleNext(0)` 的语义）。
	next := s.tickOnce(ctx)

	for {
		if next <= 0 {
			next = ClaimPollInterval
		}
		// ⚠ 用 Timer 而不是 Ticker：间隔每次都可能不同
		//（hold 到 ends_at / cooldown / 正常轮询），Ticker 的固定间隔做不到。
		timer := time.NewTimer(next)
		select {
		case <-ctx.Done():
			timer.Stop()
			return
		case <-timer.C:
			next = s.tickOnce(ctx)
		}
	}
}

// tickOnce 跑一次完整决策，返回"下次该等多久"。
func (s *ClaimScheduler) tickOnce(ctx context.Context) time.Duration {
	// 永久停止后不再跑（参考实现 stop() 的语义）。
	s.mu.Lock()
	if s.stopped {
		s.mu.Unlock()
		return ClaimPollInterval
	}
	s.mu.Unlock()

	if s.enabled != nil && !s.enabled() {
		// 用户关掉了：按正常间隔继续检查（不退出循环），
		// 这样在界面上重新打开能立刻生效，不必重启网关。
		return ClaimPollInterval
	}

	creds, _, err := s.loadCreds()
	if err != nil {
		s.record(ClaimTickResult{Outcome: ClaimOutcomeError, Kind: ClaimFailUnknown,
			Msg: "加载凭证失败: " + err.Error(), NextIn: ClaimCooldown})
		return ClaimCooldown
	}
	if len(creds) == 0 {
		s.record(ClaimTickResult{Outcome: ClaimOutcomeIdle, NextIn: ClaimPollInterval})
		return ClaimPollInterval
	}

	now := s.now()
	// nextWait 整轮该等多久。
	//
	// # 多账号下 hold 的聚合语义（我们自己定的，参考实现是单账号）
	//
	// 参考实现一个进程一个账号，它只有一个 `holdUntil` 变量 ——
	// 故"多账号怎么聚合"它没告诉我们，必须自己定，而**取最短**才对：
	//
	//	取最短 ⇒ 最早到期的账号一到点就醒来；仍在 hold 的账号被
	//	          `checkHold` 跳过（**零网络开销**，那正是 hold 的设计目的）
	//	取最长 ⇒ 一个账号 hold 24h，**其它账号 24 小时都不再被检查** ——
	//	          会把"别的账号可能上新套餐"整体拖住
	//
	// 判据是"醒来一次的成本"：仍被 hold 的账号不发任何请求，
	// 故早醒**廉价**，而晚醒有代价（错过领取窗口）。
	//
	// # ⚠ 我第一版把它写成了"上限"（被自己的测试抓到）
	//
	// 原来是 `nextWait := ClaimPollInterval` 再 `if wait < nextWait` ——
	// 于是 5 分钟成了**上限**，2 小时的 hold 被截成 5m：
	//
	//	nextWait = 5m;  if 2h < 5m { ... }  → 不成立 → 仍是 5m
	//
	// 后果是 **hold 完全失效**（领到套餐后仍每 5 分钟白探）。
	// 测试直接报「领取成功后应 hold 到套餐截止（≈2h），实际 5m0s」。
	//
	// 用 0 作"未设置"哨兵，最后再兜底成正常间隔 —— 这样
	// 单账号 hold 2h 就是 2h，而 A-hold-2h + B-无hold 仍是 5m（正确）。
	//
	// 上面的注释我一度写成"取最长"，那是改错方向的中间状态，已更正。
	var nextWait time.Duration

	for _, cr := range creds {
		if ctx.Err() != nil {
			return nextWait
		}
		if cr == nil || cr.UID == "" {
			continue
		}

		// ① hold 硬闸：未到期**连 preview 都不发**。
		if _, wait, held := s.checkHold(cr.UID, now); held {
			if wait > 0 && (nextWait == 0 || wait < nextWait) {
				nextWait = wait
			}
			continue
		}

		res, wait := s.tickAccount(ctx, cr)
		if wait > 0 && (nextWait == 0 || wait < nextWait) {
			nextWait = wait
		}
		if res.Outcome == ClaimOutcomeFailed && res.Kind == ClaimFailLoginRequired {
			// 凭证失效 → **永久停止**（参考实现 stop()）。
			// 重试一万次也不会好，只会把上游的失败计数打上去。
			s.mu.Lock()
			s.stopped = true
			s.mu.Unlock()
			log.Printf("zcode claim: 凭证失效（login_required），自动领取已停止；重新登录后重启网关可恢复")
			break
		}
	}
	// 一个账号都没给出等待时长（全部被跳过/无凭证）→ 用正常间隔。
	if nextWait <= 0 {
		nextWait = ClaimPollInterval
	}
	return nextWait
}

// checkHold 判断该账号是否仍在 hold 期内。
func (s *ClaimScheduler) checkHold(uid string, now time.Time) (ClaimTickResult, time.Duration, bool) {
	s.mu.Lock()
	until, ok := s.holdUntil[uid]
	s.mu.Unlock()
	if !ok || !now.Before(until) {
		return ClaimTickResult{}, 0, false
	}
	wait := until.Sub(now)
	return ClaimTickResult{Outcome: ClaimOutcomeSkippedHold, UID: uid, NextIn: wait}, wait, true
}

// setHold 把该账号 hold 到指定时刻（上限 ClaimHoldCap）。
func (s *ClaimScheduler) setHold(uid string, until time.Time) time.Duration {
	now := s.now()
	if !until.After(now) {
		// 上游给的时刻已过（或没给）→ 不 hold，走正常轮询。
		return ClaimPollInterval
	}
	if d := until.Sub(now); d > ClaimHoldCap {
		until = now.Add(ClaimHoldCap)
	}
	s.mu.Lock()
	s.holdUntil[uid] = until
	s.mu.Unlock()
	return until.Sub(now)
}

// tickAccount 处理单个账号（preview → 挑一个 → claim）。
//
// 返回该账号导致的"下次等待"。
func (s *ClaimScheduler) tickAccount(ctx context.Context, cr *Cred) (ClaimTickResult, time.Duration) {
	preview, err := s.client.FetchPlanPreview(ctx, cr)
	if err != nil {
		kind, code := classifyClaimError(err)
		switch kind {
		case ClaimFailNotFound:
			// 活动端点尚未部署 / 活动未上线 —— **属预期状态**，
			// 走正常轮询而不是 error backoff（参考实现就是这么分的：
			// "preview 抛 404 → idle，不走 error backoff"）。
			res := ClaimTickResult{Outcome: ClaimOutcomeIdle, Kind: kind, UID: cr.UID,
				Code: code, Msg: err.Error(), NextIn: ClaimPollInterval}
			s.record(res)
			return res, ClaimPollInterval
		case ClaimFailLoginRequired:
			res := ClaimTickResult{Outcome: ClaimOutcomeFailed, Kind: kind, UID: cr.UID,
				Code: code, Msg: err.Error(), NextIn: ClaimCooldown}
			s.record(res)
			return res, ClaimCooldown
		default:
			res := ClaimTickResult{Outcome: ClaimOutcomeError, Kind: kind, UID: cr.UID,
				Code: code, Msg: err.Error(), NextIn: ClaimCooldown}
			s.record(res)
			return res, ClaimCooldown
		}
	}

	// 没有可领套餐 = 正常（活动未上线/已领完）。
	if preview == nil || len(preview.Offers) == 0 {
		res := ClaimTickResult{Outcome: ClaimOutcomeIdle, UID: cr.UID, NextIn: ClaimPollInterval}
		s.record(res)
		return res, ClaimPollInterval
	}

	target := pickOffer(preview.Offers)
	if target == nil {
		res := ClaimTickResult{Outcome: ClaimOutcomeIdle, UID: cr.UID, NextIn: ClaimPollInterval}
		s.record(res)
		return res, ClaimPollInterval
	}

	res, err := s.client.ClaimPlan(ctx, cr, target.PlanID)
	if err != nil {
		kind, code := classifyClaimError(err)
		tr := ClaimTickResult{Outcome: ClaimOutcomeFailed, Kind: kind, UID: cr.UID,
			PlanID: target.PlanID, Code: code, Msg: err.Error()}

		// `already_claimed` / `quota_exhausted` 且上游给了截止时刻：
		// hold 到那个时刻（上限 24h），不必每 5 分钟白探。
		if kind == ClaimFailAlreadyClaimed || kind == ClaimFailQuotaExhausted {
			if wait := s.setHold(cr.UID, target.endsAtTime()); wait > 0 {
				tr.NextIn = wait
				s.record(tr)
				return tr, wait
			}
		}
		tr.NextIn = ClaimCooldown
		s.record(tr)
		return tr, ClaimCooldown
	}

	// 成功（或幂等命中）。
	if res != nil && res.AlreadyClaimed {
		// 幂等命中：目标已达成 ⇒ hold 到该套餐截止，不再白探。
		kind := ClaimFailAlreadyClaimed
		tr := ClaimTickResult{Outcome: ClaimOutcomeIdle, Kind: kind, UID: cr.UID,
			PlanID: target.PlanID, Code: res.Code, Msg: res.Msg}
		if wait := s.setHold(cr.UID, target.endsAtTime()); wait > 0 {
			tr.NextIn = wait
			s.record(tr)
			return tr, wait
		}
		tr.NextIn = ClaimPollInterval
		s.record(tr)
		return tr, ClaimPollInterval
	}

	// 真的领到了：hold 到 ends_at（参考实现：成功 → hold 到 ends_at）。
	tr := ClaimTickResult{Outcome: ClaimOutcomeClaimed, UID: cr.UID,
		PlanID: target.PlanID, Msg: "已领取"}
	if wait := s.setHold(cr.UID, target.endsAtTime()); wait > 0 {
		tr.NextIn = wait
		s.record(tr)
		log.Printf("zcode claim: 账号 %s 领取成功 plan=%s，下次再探 %v 后",
			shortUIDLog(cr.UID), target.PlanID, wait.Round(time.Minute))
		return tr, wait
	}
	tr.NextIn = ClaimPollInterval
	s.record(tr)
	log.Printf("zcode claim: 账号 %s 领取成功 plan=%s", shortUIDLog(cr.UID), target.PlanID)
	return tr, ClaimPollInterval
}

// record 存下最近一次结果。
func (s *ClaimScheduler) record(r ClaimTickResult) {
	s.mu.Lock()
	s.lastResult = r
	s.mu.Unlock()
}

// LastResult 读最近一次 tick 结果（供诊断/界面）。
func (s *ClaimScheduler) LastResult() ClaimTickResult {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.lastResult
}

// Stopped 是否已因凭证失效而永久停止。
func (s *ClaimScheduler) Stopped() bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.stopped
}

// ResetStop 清除"永久停止"标记（用户重新登录后由界面/CLI 调用）。
func (s *ClaimScheduler) ResetStop() {
	s.mu.Lock()
	s.stopped = false
	s.mu.Unlock()
}

// classifyClaimError 把错误映射成参考实现的 failureKind。
//
// ⚠ 映射表是**逐条照搬**参考实现的 `classifyClaimCode`（它镜像桌面端），
// 不要凭直觉增删 —— 每个码对应不同的下次轮询间隔，改错会让轮询节奏偏离。
func classifyClaimError(err error) (ClaimFailureKind, string) {
	if err == nil {
		return ClaimFailNone, ""
	}
	var e *Error
	if errors.As(err, &e) {
		code := strings.TrimSpace(e.Code)
		if code != "" {
			if k := claimKindOfCode(code); k != ClaimFailNone {
				return k, code
			}
		}
		// 没有可识别的业务码时按 HTTP 状态推。
		//
		// ⚠ **404 必须映射成 not_found**（→ 走正常轮询，不是 error 退避）。
		//
		// 参考实现的判据是「preview 抛 404 → idle → normal poll」，
		// 因为"活动端点尚未部署 / 活动还没上线"是**预期状态**，不是故障 ——
		// 把它当故障会让轮询退避到 10 分钟，从而**错过活动刚上线的那一刻**
		//（那恰恰是这个功能存在的理由：抢限量名额）。
		//
		// 我第一版漏了这一条，只认业务码 1001；而 404 响应体是
		// `{"error_msg":"404 Route Not Found"}` —— 里面**没有** `code` 字段，
		// 于是落到 ClaimFailHTTPError ⇒ 走 cooldown ⇒ 测试报
		// 「preview 404 应走正常轮询（5m0s），实际 10m0s」。
		if e.Status == 404 {
			return ClaimFailNotFound, code
		}
		if e.Status == 401 {
			return ClaimFailLoginRequired, code
		}
		if e.Status >= 400 {
			return ClaimFailHTTPError, code
		}
		return ClaimFailUnknown, code
	}
	return ClaimFailUnknown, ""
}

// claimKindOfCode 业务码 → 失败种类（逐条照搬参考实现）。
func claimKindOfCode(code string) ClaimFailureKind {
	switch code {
	case "1001":
		return ClaimFailNotFound
	case "1002":
		return ClaimFailUnavailable
	case "1003":
		return ClaimFailAlreadyClaimed
	case "1004":
		return ClaimFailIneligible
	case "1005":
		return ClaimFailQuotaExhausted
	case "3001":
		return ClaimFailInvalidRequest
	case "3007":
		return ClaimFailCaptcha
	case "401":
		return ClaimFailLoginRequired
	default:
		return ClaimFailNone
	}
}

// pickOffer 从可领套餐里挑一个。
//
// # 选择口径
//
// 参考实现按 `priority` 最高挑（服务端顺序作 tiebreak），但 **`PlanOffer`
// 没有解析 priority**（preview 的返回项里有，我们只取了 planId/name/status/endsAt）。
//
// 与"取第一个"的等价性：preview 返回的**本来就是"当前可领"的清单**，
// 顺序由服务端给，先出现的即它认为更该领的。实测该清单通常 0 或 1 条。
//
// ⚠ 若将来发现同一账号有多个可领套餐且顺序不稳，再补 priority 解析
//（那需要改 `PlanOffer` 的形状，故留到确有需要时）。
func pickOffer(offers []PlanOffer) *PlanOffer {
	for i := range offers {
		if strings.TrimSpace(offers[i].PlanID) == "" {
			continue
		}
		// status 非空且明显不可领时跳过（上游偶尔把"已结束"也发下来）
		if s := strings.ToLower(strings.TrimSpace(offers[i].Status)); s == "expired" || s == "ended" {
			continue
		}
		return &offers[i]
	}
	return nil
}

// endsAtTime 该套餐的领取截止时刻（零值 = 上游没给）。
//
// 用 `EndsAt`（Unix 秒）—— preview 的每一项都带它。
func (o *PlanOffer) endsAtTime() time.Time {
	if o == nil || o.EndsAt <= 0 {
		return time.Time{}
	}
	return time.Unix(o.EndsAt, 0)
}

// shortUIDLog 日志用的短 uid（与 cmd/server 的同名函数无关，包内自足）。
func shortUIDLog(uid string) string {
	if len(uid) <= 8 {
		return uid
	}
	return uid[:8]
}

// claimSchedulerSummary 一句话描述调度器状态（供启动日志）。
func (s *ClaimScheduler) claimSchedulerSummary() string {
	if s == nil {
		return "未启用"
	}
	return fmt.Sprintf("自动领取已启动（每 %v 轮询，失败冷却 %v，成功/已领后 hold 到套餐截止）",
		ClaimPollInterval, ClaimCooldown)
}
