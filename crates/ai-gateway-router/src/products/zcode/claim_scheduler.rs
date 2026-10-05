//! ZCode 套餐自动领取调度器（Go `go-gateway/internal/zcode/claim_scheduler.go`，607 行）。
//!
//! # 为什么需要它
//!
//! 套餐权益**不会自动到账**：必须每 5 分钟探一次 `billing/preview`，发现可领套餐就调
//! `billing/claim`。前端 `src/components/zcode-schedule-info.tsx:44-46` 对用户的承诺是
//! 「启动即跑 + 每 5 分钟轮询」—— 少了这个调度器，那句话就是假的。
//!
//! # 三条硬约束（Go 模块头 `:3-61` 逐字记录）
//!
//! 1. **串行单飞**：一个循环、账号在每次 tick 内**顺序**处理；没有并发领取。
//! 2. **`hold_until` 是硬闸门**：已被 hold 的账号**一次 preview 请求都不发**
//!    （[`Ctx::check_hold`] 命中即 `continue`，见 [`ClaimScheduler::tick_once`]）。
//! 3. **领取零重试**：`claim_plan` 调一次就定结果，失败只记冷却，绝不自动重试。
//!
//! 范围仅 `billing/preview` + `billing/claim`，绝不碰聊天端点。
//!
//! # 三个已经踩过的坑（Go 源码里有长注释，这里照抄结论）
//!
//! 1. **`next_wait` 必须取最短，且「未设置」要用 `Option` 表达**（Go 用 `0` 当哨兵）。
//!    第一版把它初始化成 `ClaimPollInterval` 再 `if wait < nextWait` ⇒ 5 分钟变成
//!    **上限**，2 小时的 hold 被截成 5 分钟 ⇒ **hold 完全失效**。实测症状是
//!    「领取成功后应 hold 到套餐截止（≈2h），实际 5m0s」。反向取最长同样错：
//!    一个账号 hold 24h 会让**其他账号 24 小时不被检查**。
//! 2. **preview 的 404 是「活动端点尚未部署」这一预期状态**，必须走
//!    `NotFound ⇒ Idle + 正常轮询`，**不是** `http_error + 冷却`。该响应体是
//!    `{"error_msg":"404 Route Not Found"}`，**没有 `code` 字段** ⇒ 只能靠 HTTP 状态
//!    判（[`ClaimError::status`] 就是为此存在的）。第一版只认业务码 1001 ⇒ 落到
//!    `http_error` ⇒ 测试报「preview 404 应走正常轮询（5m0s），实际 10m0s」，
//!    把探测退避到 10 分钟 ⇒ **会错过活动刚上线的那一刻**。
//! 3. **`tick_account` 返回 `(结果, 下次等待)` 的顺序**。Go 第一版写成 `_, res :=`，
//!    编译期报 `cannot use res (variable of int64 type time.Duration) as
//!    ClaimTickResult value`。
//!
//! # 与 Go 的三处有意偏离
//!
//! * `hold_until` 用 [`SystemTime`] 而不是 `Instant`：`PlanOffer.ends_at` 是 Unix 秒，
//!   与 `UNIX_EPOCH + Duration::from_secs(n)` 天然同构；`Instant` 还得另立基准。
//!   本 crate 既有的时钟口径也是 `SystemTime`（`models_cache.rs:171/189/214/233`
//!   全是 `now: SystemTime` 入参）。
//! * 上游两个原语走 [`ClaimUpstream`] 抽象：`Client` 上它们是**固有方法**而非 trait，
//!   不抽象就没法在单测里塞假 client（任务书要求「时钟可注入、不真 sleep、无真实
//!   网络」）。生产路径 `impl ClaimUpstream for Client` 只做一次转发，行为不变。
//! * [`ClaimTickOutcome`] 多一个 `None` 变体（值 `""`）：Go 的零值就是 `""`，不属于
//!   那 5 个具名常量中的任何一个，而 Rust 侧要 `Default` 才能表达「一次都没跑过」。
//!   跨实现契约关心的是具名常量的**字符串值**，`""` 本来就在契约里
//!   （[`ClaimFailureKind::None`]）。
//!
//! # 接线不在本文件
//!
//! 本模块只交付「可调用的调度器」：谁构造、谁 `tokio::spawn(sched.run(&cancel))`、
//! 界面上的开关接到哪个 `enabled` 闭包，都是 `lib` 侧接线任务的范围。因此本文件里
//! 若干 `pub(crate)` 方法目前只有测试与诊断调用 —— 与 `dispatch.rs:77` 的口径一致，
//! 用模块级 `allow(dead_code)` 而不是散落的逐项标注。
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use super::claim::{ClaimError, ClaimResult, PlanOffer, PlanPreview};
use super::client::Client;
use super::cred::ZcodeCred;
use crate::scheduler::Cancel;

// ─────────────────────────── 常量（Go `:73-83`） ───────────────────────────

/// Go `ClaimPollInterval`：正常轮询间隔（活动未上线 / 没有可领套餐）。
pub const CLAIM_POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Go `ClaimCooldown`：一次失败后的冷却。
pub const CLAIM_COOLDOWN: Duration = Duration::from_secs(10 * 60);

/// Go `ClaimHoldCap`：单次 hold 的上限（24 小时）。
///
/// 存在的意义是防上游给出荒唐的 `ends_at`（例如 2099 年）把账号永久冻住。
pub const CLAIM_HOLD_CAP: Duration = Duration::from_secs(24 * 3600);

// ────────────────────── 失败种类（Go `ClaimFailureKind`） ──────────────────────

/// Go `ClaimFailureKind`（`:88-102`）。
///
/// ★ **这是跨实现契约**：种类决定下次轮询间隔，且 [`Self::as_str`] 的返回值必须与
/// Go 侧逐字一致 —— 界面与日志都在消费这些字符串。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ClaimFailureKind {
    /// `""` —— 无失败（也是 Go 的零值）。
    #[default]
    None,
    /// `"not_found"`（业务码 1001）：活动端点尚未部署/未上线，**属预期状态**。
    NotFound,
    /// `"unavailable"`（1002）。
    Unavailable,
    /// `"already_claimed"`（1003）：本周期已领过，**不是错误**。
    AlreadyClaimed,
    /// `"ineligible"`（1004）：账号不满足活动条件。
    Ineligible,
    /// `"quota_exhausted"`（1005）：名额/额度用完。
    QuotaExhausted,
    /// `"invalid_request"`（3001）。
    InvalidRequest,
    /// `"captcha"`（3007）：上游要求人机验证。
    Captcha,
    /// `"login_required"`（HTTP 401 或业务码 401）：凭证失效 ⇒ 停止自动领取。
    LoginRequired,
    /// `"http_error"`：其它 4xx/5xx。
    HttpError,
    /// `"unknown"`：判不出来（含所有本地错误）。
    Unknown,
}

impl ClaimFailureKind {
    /// 跨实现契约的字符串值（与 Go 常量逐字一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "",
            Self::NotFound => "not_found",
            Self::Unavailable => "unavailable",
            Self::AlreadyClaimed => "already_claimed",
            Self::Ineligible => "ineligible",
            Self::QuotaExhausted => "quota_exhausted",
            Self::InvalidRequest => "invalid_request",
            Self::Captcha => "captcha",
            Self::LoginRequired => "login_required",
            Self::HttpError => "http_error",
            Self::Unknown => "unknown",
        }
    }
}

// ────────────────────── 本次 tick 的结局（Go `ClaimTickOutcome`） ──────────────────────

/// Go `ClaimTickOutcome`（`:104-113`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ClaimTickOutcome {
    /// `""` —— Go 的零值（一次 tick 都没跑过时 [`ClaimScheduler::last_result`] 的样子）。
    ///
    /// Go 那边零值同样不属于 5 个具名常量；Rust 需要它来给 `Default` 一个落点。
    #[default]
    None,
    /// `"claimed"` —— 真的领到了。
    Claimed,
    /// `"idle"` —— 没得领（无套餐 / 已领过 / 404 未上线）。
    Idle,
    /// `"skipped_hold"` —— 被 hold 拦住，**没发任何请求**。
    SkippedHold,
    /// `"failed"` —— 领取失败（含 login_required）。
    Failed,
    /// `"error"` —— 传输/本地错误。
    Error,
}

impl ClaimTickOutcome {
    /// 跨实现契约的字符串值（与 Go 常量逐字一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Claimed => "claimed",
            Self::Idle => "idle",
            Self::SkippedHold => "skipped_hold",
            Self::Failed => "failed",
            Self::Error => "error",
        }
    }
}

// ─────────────────────────── 单次 tick 的结果 ───────────────────────────

/// Go `ClaimTickResult`（`:116-125`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaimTickResult {
    /// 本次结局。
    pub outcome: ClaimTickOutcome,
    /// 失败种类（成功/空闲时是 [`ClaimFailureKind::None`]）。
    pub kind: ClaimFailureKind,
    /// 账号 uid（**原文**，不截断；日志才用 [`short_uid_log`]）。
    pub uid: String,
    /// 涉及的套餐 id。
    pub plan_id: String,
    /// 上游业务码。
    pub code: String,
    /// 人话消息。
    pub msg: String,
    /// 下次该等多久。
    pub next_in: Duration,
}

// ─────────────────────────── 注入点类型别名 ───────────────────────────

/// 可注入时钟。缺省（`None`）走 [`SystemTime::now`]。
pub type NowFn = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// Go `loadCreds func() ([]*Cred, []string, error)`。
///
/// 第二个元素是加载过程中的告警（坏文件等），本调度器不使用 —— Go 侧也只用它记日志。
pub type LoadCreds = Arc<dyn Fn() -> Result<(Vec<ZcodeCred>, Vec<String>), String> + Send + Sync>;

/// Go `enabled func() bool`：界面上的「自动领取」开关。
///
/// 返回 `false` 时**只是暂停**，绝不退出循环 —— 见 [`ClaimScheduler::tick_once`]。
pub type EnabledFn = Arc<dyn Fn() -> bool + Send + Sync>;

// ─────────────────────────── 上游抽象（仅为了可测） ───────────────────────────

/// `claim.rs` 里两个原语的 trait 化视图。
///
/// 存在的唯一理由是**可测**：`Client::fetch_plan_preview` / `Client::claim_plan` 是
/// 固有方法，不抽象就没法在单测里驱动一个「按脚本回放 404 / 401 / 1005」的假上游。
/// 生产路径只有 [`Client`] 一个实现，且是纯转发。
#[async_trait]
pub(crate) trait ClaimUpstream: Send + Sync {
    /// 对应 `Client::fetch_plan_preview`。
    async fn preview(&self, cr: &ZcodeCred) -> Result<PlanPreview, ClaimError>;
    /// 对应 `Client::claim_plan`。
    async fn claim(&self, cr: &ZcodeCred, plan_id: &str) -> Result<ClaimResult, ClaimError>;
}

#[async_trait]
impl ClaimUpstream for Client {
    async fn preview(&self, cr: &ZcodeCred) -> Result<PlanPreview, ClaimError> {
        // 固有方法优先于 trait 方法解析，这里不会递归。
        Client::fetch_plan_preview(self, cr).await
    }

    async fn claim(&self, cr: &ZcodeCred, plan_id: &str) -> Result<ClaimResult, ClaimError> {
        Client::claim_plan(self, cr, plan_id).await
    }
}

// ─────────────────────────── 被 `mu` 保护的共享状态 ───────────────────────────

/// Go `mu sync.Mutex` 保护的那三样东西（`holdUntil` / `stopped` / `lastResult`）。
#[derive(Default)]
struct State {
    /// **按账号**的 hold（Go `holdUntil map[string]time.Time`）。
    hold_until: HashMap<String, SystemTime>,
    /// 凭证失效后置位，直到 [`ClaimScheduler::reset_stop`]。
    stopped: bool,
    /// 最近一次 tick 的结果（Go `lastResult`）。
    last_result: ClaimTickResult,
}

/// 一次调用用得到的上下文：共享状态 + 时钟。
///
/// 单独抽出来是为了让决策表能写成自由函数 —— Go 的手动路径
/// （[`claim_once`]）在没有调度器时会 `NewClaimScheduler(...)` 造一个**临时**的
/// hold 表，Rust 侧对应「借一个临时 `State`」。
#[derive(Clone, Copy)]
struct Ctx<'a> {
    state: &'a Mutex<State>,
    now_fn: &'a Option<NowFn>,
}

impl<'a> Ctx<'a> {
    /// 中毒的锁也要继续用：一次 panic 不该把调度器永久卡死。
    fn lock(&self) -> MutexGuard<'a, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Go `s.now()`（nil 时 `time.Now`）。
    fn now(&self) -> SystemTime {
        match self.now_fn {
            Some(f) => f(),
            None => SystemTime::now(),
        }
    }

    /// Go `checkHold`（`:339-348`）：命中返回 `(结果, 剩余时间)`，未命中返回 `None`。
    ///
    /// 注意：**过期的条目不会在这里删除**（Go 也不删），只是不再命中。
    fn check_hold(&self, uid: &str, now: SystemTime) -> Option<(ClaimTickResult, Duration)> {
        let until = *self.lock().hold_until.get(uid)?;
        // Go: `!now.Before(until)` ⇒ 不 hold（到点即放行）。
        if now >= until {
            return None;
        }
        let wait = until.duration_since(now).unwrap_or(Duration::ZERO);
        let res = ClaimTickResult {
            outcome: ClaimTickOutcome::SkippedHold,
            uid: uid.to_string(),
            next_in: wait,
            ..Default::default()
        };
        Some((res, wait))
    }

    /// Go `setHold`（`:351-364`）：写入 hold 并返回剩余时间；不成立时返回
    /// [`CLAIM_POLL_INTERVAL`] 表示「没有 hold」。
    ///
    /// * `until` 为 `None`（Go 的零值 `time.Time`）⇒ 不 hold；
    /// * `until` 不晚于 `now` ⇒ 不 hold（**也不清除已有条目**，与 Go 一致）；
    /// * 超出 [`CLAIM_HOLD_CAP`] ⇒ 截断到 24h。
    fn set_hold(&self, uid: &str, until: Option<SystemTime>) -> Duration {
        let now = self.now();
        let Some(until) = until else {
            return CLAIM_POLL_INTERVAL;
        };
        if until <= now {
            return CLAIM_POLL_INTERVAL;
        }
        let until = match until.duration_since(now) {
            Ok(d) if d > CLAIM_HOLD_CAP => now + CLAIM_HOLD_CAP,
            _ => until,
        };
        let wait = until.duration_since(now).unwrap_or(Duration::ZERO);
        self.lock().hold_until.insert(uid.to_string(), until);
        wait
    }

    /// Go `s.record(res)`。
    fn record(&self, res: ClaimTickResult) {
        self.lock().last_result = res;
    }
}

// ─────────────────────────── 调度器本体 ───────────────────────────

/// Go `ClaimScheduler`（`:135-159`）。
pub struct ClaimScheduler {
    client: Arc<dyn ClaimUpstream>,
    load_creds: Option<LoadCreds>,
    enabled: Option<EnabledFn>,
    /// Go 的 `mu` + 三个受保护字段。
    state: Mutex<State>,
    /// Go `now func() time.Time`（nil ⇒ `time.Now`）。
    now_fn: Option<NowFn>,
}

impl ClaimScheduler {
    /// Go `NewClaimScheduler`（`:165-180`）。
    ///
    /// `client` 为 `None` 时自建一个（Go: `c == nil ⇒ New()`），因此**永远不会**出现
    /// 「调度器在手但上游为空」。
    pub fn new(
        client: Option<Client>,
        load_creds: Option<LoadCreds>,
        enabled: Option<EnabledFn>,
    ) -> Self {
        Self {
            client: Arc::new(client.unwrap_or_else(Client::new)),
            load_creds,
            enabled,
            state: Mutex::new(State::default()),
            now_fn: None,
        }
    }

    /// 测试专用构造：直接塞一个假上游 + 固定时钟。
    #[cfg(test)]
    fn from_upstream(
        client: Arc<dyn ClaimUpstream>,
        load_creds: Option<LoadCreds>,
        enabled: Option<EnabledFn>,
        now_fn: Option<NowFn>,
    ) -> Self {
        Self {
            client,
            load_creds,
            enabled,
            state: Mutex::new(State::default()),
            now_fn,
        }
    }

    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            state: &self.state,
            now_fn: &self.now_fn,
        }
    }

    /// Go `Run(ctx)`（`:218-240`）。
    ///
    /// **启动即跑一次**，之后用「一次性 timer」而不是 ticker —— 因为间隔每次都不同
    /// （5 分钟 / 10 分钟 / 到套餐截止）。取消时立刻返回。
    ///
    /// `load_creds` 为 `None` ⇒ 直接返回（Go 的 `if s.loadCreds == nil { return }`），
    /// 否则 `tick_once` 里会空指针。
    pub async fn run(&self, cancel: &Cancel) {
        if self.load_creds.is_none() {
            return;
        }
        let mut next = self.tick_once(cancel).await;
        loop {
            if next <= Duration::ZERO {
                next = CLAIM_POLL_INTERVAL;
            }
            // 本 crate 的标准「可取消 timer」（`scheduler/mod.rs:190`）。
            if !cancel.sleep(next).await {
                return;
            }
            next = self.tick_once(cancel).await;
        }
    }

    /// Go `tickOnce(ctx)`（`:243-336`）：跑一轮**所有账号**，返回下次该等多久。
    pub(crate) async fn tick_once(&self, cancel: &Cancel) -> Duration {
        let ctx = self.ctx();

        // ① 凭证失效后停摆（`reset_stop` 才复活）。
        if ctx.lock().stopped {
            return CLAIM_POLL_INTERVAL;
        }

        // ② 界面开关关掉 ⇒ **只暂停，不退出循环**。
        //
        // Go 原注释：这样在界面上重新打开能立刻生效，不必重启网关。
        if let Some(enabled) = &self.enabled {
            if !enabled() {
                return CLAIM_POLL_INTERVAL;
            }
        }

        // ③ 取凭证。
        let Some(load) = &self.load_creds else {
            return CLAIM_POLL_INTERVAL;
        };
        let creds = match load() {
            Ok((creds, _warns)) => creds,
            Err(e) => {
                ctx.record(ClaimTickResult {
                    outcome: ClaimTickOutcome::Error,
                    kind: ClaimFailureKind::Unknown,
                    msg: format!("加载凭证失败: {e}"),
                    next_in: CLAIM_COOLDOWN,
                    ..Default::default()
                });
                return CLAIM_COOLDOWN;
            }
        };
        if creds.is_empty() {
            ctx.record(ClaimTickResult {
                outcome: ClaimTickOutcome::Idle,
                next_in: CLAIM_POLL_INTERVAL,
                ..Default::default()
            });
            return CLAIM_POLL_INTERVAL;
        }

        // ④ 本轮统一用**同一个** now（Go 也是循环外取一次）。
        let now = ctx.now();
        // ★ `None` = 「还没有任何账号给出等待时间」。**绝不能**用
        // `CLAIM_POLL_INTERVAL` 初始化：那样 5 分钟就变成了上限，长 hold 被截断。
        let mut next_wait: Option<Duration> = None;

        for cr in creds.iter() {
            if cancel.is_cancelled() {
                // Go 在这里是 `return nextWait`（**不**做 `<= 0 ⇒ ClaimPollInterval`
                // 归一）；调用方 `run` 马上就会因取消而退出，返回值没人看。
                return next_wait.unwrap_or(Duration::ZERO);
            }
            if cr.uid.is_empty() {
                continue;
            }

            // ⑤ hold 是硬闸门：命中即跳过，**一次 preview 都不发**。
            if let Some((_res, wait)) = ctx.check_hold(&cr.uid, now) {
                if wait > Duration::ZERO {
                    next_wait = Some(shortest(next_wait, wait));
                }
                continue;
            }

            let (res, wait) = self.tick_account(cr).await;
            if wait > Duration::ZERO {
                next_wait = Some(shortest(next_wait, wait));
            }

            // ⑥ 凭证失效 ⇒ 停摆并跳出账号循环。
            if res.outcome == ClaimTickOutcome::Failed && res.kind == ClaimFailureKind::LoginRequired
            {
                ctx.lock().stopped = true;
                eprintln!(
                    "zcode claim: 凭证失效（login_required），自动领取已停止；\
                     重新登录后重启网关可恢复"
                );
                break;
            }
        }

        // ⑦ 一个都没给出等待时间 ⇒ 正常轮询。
        match next_wait {
            Some(d) if d > Duration::ZERO => d,
            _ => CLAIM_POLL_INTERVAL,
        }
    }

    /// Go `tickAccount(ctx, cr)`（`:369-459`）——**返回 `(结果, 下次等待)` 的顺序**。
    pub(crate) async fn tick_account(&self, cr: &ZcodeCred) -> (ClaimTickResult, Duration) {
        tick_account_with(&*self.client, self.ctx(), cr).await
    }

    /// Go `LastResult()`。
    pub fn last_result(&self) -> ClaimTickResult {
        self.ctx().lock().last_result.clone()
    }

    /// Go `Stopped()`。
    pub fn stopped(&self) -> bool {
        self.ctx().lock().stopped
    }

    /// Go `ResetStop()`（`:462-487`）。
    ///
    /// ★ **只清 `stopped`，不动 `hold_until`** —— 重新登录后不该把已有 hold 丢掉。
    pub fn reset_stop(&self) {
        self.ctx().lock().stopped = false;
    }

    /// Go `claimSchedulerSummary()`（`:601-607`）。
    pub fn summary(&self) -> String {
        format!(
            "自动领取已启动（每 {} 轮询，失败冷却 {}，成功/已领后 hold 到套餐截止）",
            go_duration(CLAIM_POLL_INTERVAL),
            go_duration(CLAIM_COOLDOWN)
        )
    }
}

/// Go 里这三个是 `ClaimScheduler` 的未导出方法；Rust 侧真正的实现在 [`Ctx`] 上
/// （决策表要能在没有调度器时借用临时状态），这里是同名包装，供接线与诊断使用。
///
/// 标 `allow(dead_code)` 的理由与 `dispatch.rs:77` 一致：生产调用方要等 `lib` 里的
/// 接线落地。
#[allow(dead_code)]
impl ClaimScheduler {
    /// Go `checkHold`。
    pub(crate) fn check_hold(
        &self,
        uid: &str,
        now: SystemTime,
    ) -> Option<(ClaimTickResult, Duration)> {
        self.ctx().check_hold(uid, now)
    }

    /// Go `setHold`。
    pub(crate) fn set_hold(&self, uid: &str, until: Option<SystemTime>) -> Duration {
        self.ctx().set_hold(uid, until)
    }

    /// Go `record`。
    pub(crate) fn record(&self, res: ClaimTickResult) {
        self.ctx().record(res)
    }
}

// ─────────────────────────── 决策表（Go `tickAccount` 主体） ───────────────────────────

/// Go `(*ClaimScheduler).tickAccount` 的完整语义，写成自由函数以便「有调度器」与
/// 「无调度器（手动领取）」两条路径**共用同一份逻辑**。
///
/// Go 原注释：手动与自动必须走同一套领取逻辑 —— 各写一份必然出现「手动能领、
/// 自动领不到」这类分歧。
async fn tick_account_with(
    up: &dyn ClaimUpstream,
    ctx: Ctx<'_>,
    cr: &ZcodeCred,
) -> (ClaimTickResult, Duration) {
    // ① 探活动。
    let preview = match up.preview(cr).await {
        Ok(p) => p,
        Err(e) => {
            let (kind, code) = classify_claim_error(&e);
            let msg = e.to_string();
            let (outcome, next) = match kind {
                // ★ 活动端点尚未部署/未上线 —— 属预期状态，走正常轮询。
                ClaimFailureKind::NotFound => (ClaimTickOutcome::Idle, CLAIM_POLL_INTERVAL),
                ClaimFailureKind::LoginRequired => (ClaimTickOutcome::Failed, CLAIM_COOLDOWN),
                _ => (ClaimTickOutcome::Error, CLAIM_COOLDOWN),
            };
            let res = ClaimTickResult {
                outcome,
                kind,
                uid: cr.uid.clone(),
                code,
                msg,
                next_in: next,
                ..Default::default()
            };
            ctx.record(res.clone());
            return (res, next);
        }
    };

    // ② 没有可领套餐（含 `preview == nil` 的等价情形）。
    if preview.offers.is_empty() {
        let res = ClaimTickResult {
            outcome: ClaimTickOutcome::Idle,
            uid: cr.uid.clone(),
            next_in: CLAIM_POLL_INTERVAL,
            ..Default::default()
        };
        ctx.record(res.clone());
        return (res, CLAIM_POLL_INTERVAL);
    }

    // ③ 挑一个可领的；全被跳过 ⇒ 同样按空闲处理。
    let Some(target) = pick_offer(&preview.offers) else {
        let res = ClaimTickResult {
            outcome: ClaimTickOutcome::Idle,
            uid: cr.uid.clone(),
            next_in: CLAIM_POLL_INTERVAL,
            ..Default::default()
        };
        ctx.record(res.clone());
        return (res, CLAIM_POLL_INTERVAL);
    };
    let target = target.clone();

    // ④ 领。**零重试**。
    match up.claim(cr, &target.plan_id).await {
        Err(e) => {
            let (kind, code) = classify_claim_error(&e);
            let mut tr = ClaimTickResult {
                outcome: ClaimTickOutcome::Failed,
                kind,
                uid: cr.uid.clone(),
                plan_id: target.plan_id.clone(),
                code,
                msg: e.to_string(),
                next_in: Duration::ZERO,
            };
            // 这两种「失败」其实是「本周期不用再试了」⇒ hold 到套餐截止。
            if kind == ClaimFailureKind::AlreadyClaimed || kind == ClaimFailureKind::QuotaExhausted
            {
                let wait = ctx.set_hold(&cr.uid, ends_at_time(&target));
                if wait > Duration::ZERO {
                    tr.next_in = wait;
                    ctx.record(tr.clone());
                    return (tr, wait);
                }
            }
            tr.next_in = CLAIM_COOLDOWN;
            ctx.record(tr.clone());
            (tr, CLAIM_COOLDOWN)
        }
        Ok(res) => {
            // ④a 上游说「已经领过了」—— 不是错误，按空闲记，并 hold。
            if res.already_claimed {
                let mut tr = ClaimTickResult {
                    outcome: ClaimTickOutcome::Idle,
                    kind: ClaimFailureKind::AlreadyClaimed,
                    uid: cr.uid.clone(),
                    plan_id: target.plan_id.clone(),
                    code: res.code.clone(),
                    msg: res.msg.clone(),
                    next_in: Duration::ZERO,
                };
                let wait = ctx.set_hold(&cr.uid, ends_at_time(&target));
                if wait > Duration::ZERO {
                    tr.next_in = wait;
                    ctx.record(tr.clone());
                    return (tr, wait);
                }
                tr.next_in = CLAIM_POLL_INTERVAL;
                ctx.record(tr.clone());
                return (tr, CLAIM_POLL_INTERVAL);
            }

            // ④b 真领到了。
            let mut tr = ClaimTickResult {
                outcome: ClaimTickOutcome::Claimed,
                uid: cr.uid.clone(),
                plan_id: target.plan_id.clone(),
                msg: "已领取".to_string(),
                next_in: Duration::ZERO,
                ..Default::default()
            };
            let wait = ctx.set_hold(&cr.uid, ends_at_time(&target));
            if wait > Duration::ZERO {
                tr.next_in = wait;
                ctx.record(tr.clone());
                eprintln!(
                    "zcode claim: 账号 {} 领取成功 plan={}，下次再探 {} 后",
                    short_uid_log(&cr.uid),
                    target.plan_id,
                    go_duration(round_minute(wait))
                );
                return (tr, wait);
            }
            tr.next_in = CLAIM_POLL_INTERVAL;
            ctx.record(tr.clone());
            eprintln!(
                "zcode claim: 账号 {} 领取成功 plan={}",
                short_uid_log(&cr.uid),
                target.plan_id
            );
            (tr, CLAIM_POLL_INTERVAL)
        }
    }
}

// ─────────────────────────── 手动领取（Go `ClaimOnce`） ───────────────────────────

/// Go `(*Dispatch).ClaimOnce(ctx, cr, sched)`（`:194-212`）。
///
/// * `sched` 为 `Some` ⇒ 走**同一个**调度器（连带它的 hold 表）；
/// * `sched` 为 `None` ⇒ 借一个**临时**的 hold 表（Go: `NewClaimScheduler(d.client,
///   nil, nil)`），结果不进 `last_result`。
///
/// ⚠ 手动路径**绕过 hold**（用户要「现在就看」），但仍受上游幂等保护
/// （`already_claimed` 照样会被识别）。
///
/// `up` 为 `None` 对应 Go 的 `d == nil || d.client == nil` ⇒
/// `{Error, Unknown, "zcode dispatch 未初始化"}`。
pub(crate) async fn claim_once(
    up: Option<&dyn ClaimUpstream>,
    cr: &ZcodeCred,
    sched: Option<&ClaimScheduler>,
) -> ClaimTickResult {
    // ★ 守卫必须在看 `sched` **之前**：Go 的顺序就是先 `d == nil || d.client == nil`
    // 再判 `sched == nil`，因此「上游没接好但传了调度器」也该报未初始化，
    // 而不是照样去发请求。
    let Some(up) = up else {
        return ClaimTickResult {
            outcome: ClaimTickOutcome::Error,
            kind: ClaimFailureKind::Unknown,
            msg: "zcode dispatch 未初始化".to_string(),
            ..Default::default()
        };
    };
    if let Some(sched) = sched {
        return sched.tick_account(cr).await.0;
    }
    // 临时状态：出栈即丢，与 Go 的 `tmp` 一致（手动领取不建立持久 hold）。
    let state = Mutex::new(State::default());
    let none: Option<NowFn> = None;
    let ctx = Ctx {
        state: &state,
        now_fn: &none,
    };
    tick_account_with(up, ctx, cr).await.0
}

/// Go `claimSchedulerSummary()` 对「没有调度器」那一半的处置（`"未启用"`）。
pub(crate) fn summary_of(sched: Option<&ClaimScheduler>) -> String {
    match sched {
        None => "未启用".to_string(),
        Some(s) => s.summary(),
    }
}

// ─────────────────────────── 分类（Go `classifyClaimError`） ───────────────────────────

/// Go `classifyClaimError(err) (ClaimFailureKind, string)`（`:493-530`）。
///
/// 顺序**不能反**：
/// 1. 有业务码且落在码表里 ⇒ 用码判；
/// 2. 否则用 HTTP 状态判：**404 ⇒ [`ClaimFailureKind::NotFound`]**、
///    401 ⇒ `LoginRequired`、`>= 400` ⇒ `HttpError`、其余 ⇒ `Unknown`。
///
/// 第 2 条里 404 那支是硬要求（见模块文档的坑 ②）：该响应体没有 `code` 字段。
///
/// Go 的 `err == nil ⇒ (None, "")` 在 Rust 里由 `Ok` 分支承担，故本函数只接
/// `&ClaimError`。非 `*Error`（即本地错误）⇒ `(Unknown, "")`。
pub(crate) fn classify_claim_error(e: &ClaimError) -> (ClaimFailureKind, String) {
    let code = e.code().trim().to_string();
    if !code.is_empty() {
        let kind = claim_kind_of_code(&code);
        if kind != ClaimFailureKind::None {
            return (kind, code);
        }
    }
    let status = e.status();
    let kind = if status == 404 {
        ClaimFailureKind::NotFound
    } else if status == 401 {
        ClaimFailureKind::LoginRequired
    } else if status >= 400 {
        ClaimFailureKind::HttpError
    } else {
        // 含 `ClaimError::Local`（status == 0）。
        ClaimFailureKind::Unknown
    };
    (kind, code)
}

/// Go `claimKindOfCode(code)`（`:533-554`）：业务码 ⇒ 种类。未知码返回
/// [`ClaimFailureKind::None`]（**不是** `Unknown`，调用方靠这个区分「码表命中」）。
pub(crate) fn claim_kind_of_code(code: &str) -> ClaimFailureKind {
    match code.trim() {
        "1001" => ClaimFailureKind::NotFound,
        "1002" => ClaimFailureKind::Unavailable,
        "1003" => ClaimFailureKind::AlreadyClaimed,
        "1004" => ClaimFailureKind::Ineligible,
        "1005" => ClaimFailureKind::QuotaExhausted,
        "3001" => ClaimFailureKind::InvalidRequest,
        "3007" => ClaimFailureKind::Captcha,
        "401" => ClaimFailureKind::LoginRequired,
        _ => ClaimFailureKind::None,
    }
}

// ─────────────────────────── 小工具 ───────────────────────────

/// 取更短的那个（`None` = 还没有值）。
fn shortest(current: Option<Duration>, candidate: Duration) -> Duration {
    match current {
        Some(d) if d <= candidate => d,
        _ => candidate,
    }
}

/// Go `pickOffer(offers) *PlanOffer`（`:568-580`）：跳过 `plan_id` 为空的、跳过
/// 状态是 `expired`/`ended`（去空白、忽略大小写）的，返回**第一个**剩下的。
///
/// Go 原注释：参考实现按 `priority` 取最高，但 `PlanOffer` 根本没解析 `priority`；
/// 由于 preview 已经按服务端顺序返回「当前可领」列表，「取第一个」与「取 priority
/// 最高」等价。故**不要**在这里尝试按 `priority` 排序。
pub(crate) fn pick_offer(offers: &[PlanOffer]) -> Option<&PlanOffer> {
    for o in offers {
        if o.plan_id.trim().is_empty() {
            continue;
        }
        let status = o.status.trim().to_lowercase();
        if status == "expired" || status == "ended" {
            continue;
        }
        return Some(o);
    }
    None
}

/// Go `(*PlanOffer).endsAtTime()`（`:585-590`）：`ends_at <= 0` ⇒ `None`
/// （Go 的零值 `time.Time`，[`Ctx::set_hold`] 会据此判定「不 hold」）。
pub(crate) fn ends_at_time(o: &PlanOffer) -> Option<SystemTime> {
    if o.ends_at <= 0 {
        return None;
    }
    // ★ 用 `checked_add` 而不是 `+`：`SystemTime + Duration` 在溢出时**panic**，
    // 而上游若给出荒唐的 `ends_at`（畸形/被篡改的响应），调度器会当场崩掉。
    // Go 那边 `time.Unix(n, 0)` 不 panic，这里必须跟上：溢出 ⇒ 当作「没有截止
    // 时间」⇒ 不建立 hold（`set_hold` 返回波动间隔），而不是把整轮 tick 带走。
    UNIX_EPOCH.checked_add(Duration::from_secs(o.ends_at as u64))
}

/// Go `shortUIDLog(uid)`（`:593-598`）：超过 8 字节就截到 8 字节，**没有省略号**。
///
/// 与 `products/zcode/mod.rs::mask_uid` 不同（那个会加 `…`）—— 这里是日志用，
/// Go 侧就是纯截断。截断点会退到字符边界（Go 按字节截会切出非法 UTF-8，Rust 不能）。
pub(crate) fn short_uid_log(uid: &str) -> &str {
    if uid.len() <= 8 {
        return uid;
    }
    let mut end = 8;
    while end > 0 && !uid.is_char_boundary(end) {
        end -= 1;
    }
    &uid[..end]
}

/// Go `time.Duration.String()` 的**够用子集**：只覆盖「秒级及以上的整值」。
///
/// 本调度器只会打印 [`CLAIM_POLL_INTERVAL`]（`5m0s`）、[`CLAIM_COOLDOWN`]
/// （`10m0s`）和 `wait.Round(time.Minute)` 之后的 hold（如 `2h0m0s`），全部落在
/// 这一子集里。亚秒值一律按整秒输出（Go 会输出 `1.5s` / `500ms`）—— 调度器
/// 从不产生亚秒等待。
pub(crate) fn go_duration(d: Duration) -> String {
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{h}h"));
    }
    if h > 0 || m > 0 {
        out.push_str(&format!("{m}m"));
    }
    out.push_str(&format!("{s}s"));
    out
}

/// Go `wait.Round(time.Minute)`：四舍五入到整分钟（**只用于日志文案**）。
fn round_minute(d: Duration) -> Duration {
    let secs = d.as_secs() + u64::from(d.subsec_nanos() >= 500_000_000);
    Duration::from_secs(((secs + 30) / 60) * 60)
}

// ─────────────────────────── 单测 ───────────────────────────

#[cfg(test)]
mod tests {
    use super::super::errors::UpstreamError;
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const T0: u64 = 1_700_000_000;

    fn t0() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(T0)
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn cred(uid: &str) -> ZcodeCred {
        ZcodeCred {
            uid: uid.to_string(),
            ..Default::default()
        }
    }

    fn offer(plan_id: &str, status: &str, ends_at: i64) -> PlanOffer {
        PlanOffer {
            plan_id: plan_id.to_string(),
            status: status.to_string(),
            ends_at,
            ..Default::default()
        }
    }

    fn preview(offers: Vec<PlanOffer>) -> PlanPreview {
        PlanPreview {
            offers,
            ..Default::default()
        }
    }

    /// 构造与**生产路径逐字一致**的上游错误。
    ///
    /// ⚠ 不能直接用 UpstreamError::new：生产走的是 claim.rs:534
    /// upstream_error，那里对 code / msg 是**无条件覆盖**
    /// （Go 是结构体字面量四字段全赋值），而 UpstreamError::new 内部的
    /// extract_message 会在 {"error_msg":"404 Route Not Found"} 上
    /// 退化成「截断原文」⇒ 整条 body 漏进 msg ⇒ Display 变成
    /// HTTP 404 {"error_msg":...} 而不是干净的 HTTP 404。
    /// 这个假 client 曾因此把 404 用例判红（实现是对的，夹具不忠实）。
    ///
    /// kind 不必重算：调度器的分类只看 status / code
    /// （与 Go classifyClaimError 一致），不看 ErrKind。
    fn up_err(status: u16, body: &str) -> ClaimError {
        let mut e = UpstreamError::new(status, body);
        e.code = super::super::claim::code_of(body);
        e.msg = super::super::claim::msg_of(body);
        ClaimError::Upstream(e)
    }

    /// 按脚本回放的假上游：记录调用次数，队列空了就返回「什么都没有」。
    #[derive(Default)]
    struct Fake {
        previews: Mutex<VecDeque<Result<PlanPreview, ClaimError>>>,
        claims: Mutex<VecDeque<Result<ClaimResult, ClaimError>>>,
        preview_calls: AtomicUsize,
        claim_calls: AtomicUsize,
    }

    impl Fake {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }
        fn push_preview(&self, r: Result<PlanPreview, ClaimError>) {
            self.previews.lock().unwrap().push_back(r);
        }
        fn push_claim(&self, r: Result<ClaimResult, ClaimError>) {
            self.claims.lock().unwrap().push_back(r);
        }
        fn preview_calls(&self) -> usize {
            self.preview_calls.load(Ordering::SeqCst)
        }
        fn claim_calls(&self) -> usize {
            self.claim_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ClaimUpstream for Fake {
        async fn preview(&self, _cr: &ZcodeCred) -> Result<PlanPreview, ClaimError> {
            self.preview_calls.fetch_add(1, Ordering::SeqCst);
            self.previews
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(PlanPreview::default()))
        }

        async fn claim(&self, _cr: &ZcodeCred, _plan_id: &str) -> Result<ClaimResult, ClaimError> {
            self.claim_calls.fetch_add(1, Ordering::SeqCst);
            self.claims
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(ClaimResult::default()))
        }
    }

    fn sched(
        up: Arc<dyn ClaimUpstream>,
        load: Option<LoadCreds>,
        enabled: Option<EnabledFn>,
    ) -> ClaimScheduler {
        ClaimScheduler::from_upstream(up, load, enabled, Some(Arc::new(t0)))
    }

    fn load_one(uid: &str) -> LoadCreds {
        let uid = uid.to_string();
        Arc::new(move || Ok((vec![cred(&uid)], Vec::new())))
    }

    fn ok_claim() -> ClaimResult {
        ClaimResult {
            ok: true,
            ..Default::default()
        }
    }

    // ── 1. 启动即跑 + 取消即退 ──────────────────────────────────────────

    #[tokio::test]
    async fn run_ticks_immediately_and_exits_on_cancel() {
        let fake = Fake::new();
        fake.push_preview(Ok(preview(vec![])));
        let s = Arc::new(sched(fake.clone(), Some(load_one("u1")), None));
        let cancel = Cancel::new();
        let c2 = cancel.clone();
        let s2 = s.clone();
        let h = tokio::spawn(async move { s2.run(&cancel).await });

        for _ in 0..400 {
            if fake.preview_calls() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            fake.preview_calls() >= 1,
            "run 必须**启动即跑**一次 tick，不能先等一个间隔"
        );

        c2.cancel();
        tokio::time::timeout(Duration::from_secs(2), h)
            .await
            .expect("取消后 run 应立即返回")
            .unwrap();
    }

    #[tokio::test]
    async fn run_without_loader_returns_immediately() {
        let fake = Fake::new();
        let s = sched(fake.clone(), None, None);
        let r = tokio::time::timeout(Duration::from_secs(1), s.run(&Cancel::new())).await;
        assert!(r.is_ok(), "loadCreds 为 None 时 run 必须立即返回");
        assert_eq!(fake.preview_calls(), 0);
    }

    // ── 2. 成功后 hold 到套餐截止 ──────────────────────────────────────

    #[tokio::test]
    async fn successful_claim_sets_hold_until_plan_end() {
        let fake = Fake::new();
        let ends = T0 + 2 * 3600;
        fake.push_preview(Ok(preview(vec![offer("p1", "active", ends as i64)])));
        fake.push_claim(Ok(ok_claim()));
        let s = sched(fake.clone(), Some(load_one("u1")), None);

        let (res, wait) = s.tick_account(&cred("u1")).await;
        assert_eq!(res.outcome, ClaimTickOutcome::Claimed);
        assert_eq!(res.kind, ClaimFailureKind::None);
        assert_eq!(res.plan_id, "p1");
        assert_eq!(res.msg, "已领取");
        assert_eq!(res.next_in, wait);
        assert_eq!(
            wait,
            Duration::from_secs(2 * 3600),
            "领取成功后必须 hold 到套餐截止（≈2h），不是 5m0s"
        );
        assert_eq!(s.last_result().outcome, ClaimTickOutcome::Claimed);

        let (hr, hw) = s.check_hold("u1", t0()).expect("领取成功后必须写入 hold");
        assert_eq!(hr.outcome, ClaimTickOutcome::SkippedHold);
        assert_eq!(hr.uid, "u1");
        assert_eq!(hw, Duration::from_secs(2 * 3600));
    }

    #[tokio::test]
    async fn held_account_costs_zero_network_calls() {
        let fake = Fake::new();
        let ends = T0 + 2 * 3600;
        fake.push_preview(Ok(preview(vec![offer("p1", "active", ends as i64)])));
        fake.push_claim(Ok(ok_claim()));
        let s = sched(fake.clone(), Some(load_one("u1")), None);

        let w1 = s.tick_once(&Cancel::new()).await;
        assert_eq!(w1, Duration::from_secs(2 * 3600));
        assert_eq!(fake.preview_calls(), 1);

        let w2 = s.tick_once(&Cancel::new()).await;
        assert_eq!(
            fake.preview_calls(),
            1,
            "hold 期内一次 preview 都不许发（硬闸门）"
        );
        assert_eq!(fake.claim_calls(), 1);
        assert_eq!(
            w2,
            Duration::from_secs(2 * 3600),
            "hold 的剩余时间必须原样成为下次等待 —— 退化成 5m0s 就是那个经典 bug"
        );
    }

    // ── 3. 已经领过 / 额度用完 ────────────────────────────────────────

    #[tokio::test]
    async fn already_claimed_result_goes_idle_and_holds() {
        let fake = Fake::new();
        let ends = T0 + 3600;
        fake.push_preview(Ok(preview(vec![offer("p1", "", ends as i64)])));
        fake.push_claim(Ok(ClaimResult {
            ok: true,
            code: "1003".to_string(),
            msg: "已领取过".to_string(),
            already_claimed: true,
            claimed_at: T0 as i64,
        }));
        let s = sched(fake.clone(), Some(load_one("u1")), None);

        let (res, wait) = s.tick_account(&cred("u1")).await;
        assert_eq!(res.outcome, ClaimTickOutcome::Idle, "已领过不是错误");
        assert_eq!(res.kind, ClaimFailureKind::AlreadyClaimed);
        assert_eq!(res.code, "1003");
        assert_eq!(res.msg, "已领取过");
        assert_eq!(res.plan_id, "p1");
        assert_eq!(wait, Duration::from_secs(3600));
    }

    #[tokio::test]
    async fn quota_exhausted_claim_error_fails_and_holds() {
        let fake = Fake::new();
        let ends = T0 + 1800;
        fake.push_preview(Ok(preview(vec![offer("p1", "", ends as i64)])));
        fake.push_claim(Err(up_err(429, "{\"code\":\"1005\",\"msg\":\"额度已用完\"}")));
        let s = sched(fake.clone(), Some(load_one("u1")), None);

        let (res, wait) = s.tick_account(&cred("u1")).await;
        assert_eq!(res.outcome, ClaimTickOutcome::Failed);
        assert_eq!(res.kind, ClaimFailureKind::QuotaExhausted);
        assert_eq!(res.code, "1005");
        assert_eq!(res.plan_id, "p1");
        assert_eq!(
            wait,
            Duration::from_secs(1800),
            "额度用完要 hold 到套餐截止，而不是 10 分钟后再去撞一次"
        );
    }

    #[tokio::test]
    async fn other_claim_error_only_cools_down() {
        let fake = Fake::new();
        fake.push_preview(Ok(preview(vec![offer("p1", "", (T0 + 3600) as i64)])));
        fake.push_claim(Err(up_err(500, "{}")));
        let s = sched(fake.clone(), Some(load_one("u1")), None);

        let (res, wait) = s.tick_account(&cred("u1")).await;
        assert_eq!(res.outcome, ClaimTickOutcome::Failed);
        assert_eq!(res.kind, ClaimFailureKind::HttpError);
        assert_eq!(wait, CLAIM_COOLDOWN);
        assert!(s.check_hold("u1", t0()).is_none(), "普通失败不建立 hold");
    }

    // ── 4. preview 404（那个经典坑） ──────────────────────────────────

    #[tokio::test]
    async fn preview_404_is_idle_with_normal_polling() {
        let fake = Fake::new();
        fake.push_preview(Err(up_err(404, "{\"error_msg\":\"404 Route Not Found\"}")));
        let s = sched(fake.clone(), Some(load_one("u1")), None);

        let (res, wait) = s.tick_account(&cred("u1")).await;
        assert_eq!(
            res.outcome,
            ClaimTickOutcome::Idle,
            "404 = 活动端点尚未部署，属预期状态，不是 error"
        );
        assert_eq!(res.kind, ClaimFailureKind::NotFound);
        assert_eq!(res.msg, "HTTP 404");
        assert!(res.code.is_empty(), "该响应体没有 code 字段 ⇒ 只能靠 HTTP 状态判");
        assert_eq!(
            wait, CLAIM_POLL_INTERVAL,
            "必须走正常轮询（5m0s）；退避到冷却（10m0s）会错过活动刚上线的那一刻"
        );
        assert_eq!(fake.claim_calls(), 0);

        // 走一遍 tick_once，确认整轮给出的也是 5 分钟。
        fake.push_preview(Err(up_err(404, "{\"error_msg\":\"404 Route Not Found\"}")));
        assert_eq!(s.tick_once(&Cancel::new()).await, CLAIM_POLL_INTERVAL);
    }

    #[tokio::test]
    async fn empty_offers_are_idle_and_never_claim() {
        let fake = Fake::new();
        fake.push_preview(Ok(preview(vec![])));
        let s = sched(fake.clone(), Some(load_one("u1")), None);

        let (res, wait) = s.tick_account(&cred("u1")).await;
        assert_eq!(res.outcome, ClaimTickOutcome::Idle);
        assert_eq!(res.kind, ClaimFailureKind::None);
        assert_eq!(wait, CLAIM_POLL_INTERVAL);
        assert_eq!(fake.claim_calls(), 0, "没有可领套餐时绝不能调 claim");

        // 有套餐但全被跳过 ⇒ 同样空闲、同样不调 claim。
        fake.push_preview(Ok(preview(vec![
            offer("", "", 0),
            offer("p", "expired", 0),
        ])));
        let (res2, wait2) = s.tick_account(&cred("u1")).await;
        assert_eq!(res2.outcome, ClaimTickOutcome::Idle);
        assert_eq!(res2.plan_id, "");
        assert_eq!(wait2, CLAIM_POLL_INTERVAL);
        assert_eq!(fake.claim_calls(), 0);
    }

    // ── 5. login_required 停摆与恢复 ─────────────────────────────────

    #[tokio::test]
    async fn preview_401_stops_the_scheduler_and_reset_stop_resumes() {
        let fake = Fake::new();
        fake.push_preview(Err(up_err(401, "{\"error_msg\":\"unauthorized\"}")));
        fake.push_preview(Ok(preview(vec![])));
        let s = sched(fake.clone(), Some(load_one("u1")), None);

        assert_eq!(s.tick_once(&Cancel::new()).await, CLAIM_COOLDOWN);
        assert!(s.stopped(), "login_required 必须停止自动领取");
        assert_eq!(s.last_result().outcome, ClaimTickOutcome::Failed);
        assert_eq!(s.last_result().kind, ClaimFailureKind::LoginRequired);

        let calls = fake.preview_calls();
        assert_eq!(s.tick_once(&Cancel::new()).await, CLAIM_POLL_INTERVAL);
        assert_eq!(fake.preview_calls(), calls, "停摆后不得再发任何请求");

        s.reset_stop();
        assert!(!s.stopped());
        assert_eq!(s.tick_once(&Cancel::new()).await, CLAIM_POLL_INTERVAL);
        assert_eq!(
            fake.preview_calls(),
            calls + 1,
            "reset_stop 之后必须恢复探测"
        );
    }

    #[tokio::test]
    async fn reset_stop_keeps_existing_holds() {
        let fake = Fake::new();
        let s = sched(fake.clone(), None, None);
        assert_eq!(
            s.set_hold("u1", Some(at(T0 + 3600))),
            Duration::from_secs(3600)
        );
        s.reset_stop();
        assert!(
            s.check_hold("u1", t0()).is_some(),
            "reset_stop 只清 stopped，不能顺手丢掉 hold"
        );
    }

    // ── 6. 界面开关：只暂停，不退出 ───────────────────────────────────

    #[tokio::test]
    async fn disabled_scheduler_pauses_without_stopping() {
        let fake = Fake::new();
        let s = sched(fake.clone(), Some(load_one("u1")), Some(Arc::new(|| false)));

        assert_eq!(s.tick_once(&Cancel::new()).await, CLAIM_POLL_INTERVAL);
        assert_eq!(fake.preview_calls(), 0);
        assert!(
            !s.stopped(),
            "enabled=false 只是暂停，不能置 stopped（否则界面重开也救不回来）"
        );
    }

    #[tokio::test]
    async fn re_enabling_takes_effect_without_restart() {
        let fake = Fake::new();
        fake.push_preview(Ok(preview(vec![])));
        let on = Arc::new(AtomicBool::new(false));
        let on2 = on.clone();
        let s = sched(
            fake.clone(),
            Some(load_one("u1")),
            Some(Arc::new(move || on2.load(Ordering::SeqCst))),
        );

        assert_eq!(s.tick_once(&Cancel::new()).await, CLAIM_POLL_INTERVAL);
        assert_eq!(fake.preview_calls(), 0);

        on.store(true, Ordering::SeqCst);
        assert_eq!(s.tick_once(&Cancel::new()).await, CLAIM_POLL_INTERVAL);
        assert_eq!(
            fake.preview_calls(),
            1,
            "界面上重新打开后下一 tick 就要生效，不必重启网关"
        );
    }

    // ── 7. 多账号：取最短 ────────────────────────────────────────────

    #[tokio::test]
    async fn multi_account_takes_the_shortest_wait() {
        let long = T0 + 2 * 3600;
        let short = T0 + 5 * 60;
        let load: LoadCreds = Arc::new(|| Ok((vec![cred("ua"), cred("ub")], Vec::new())));

        for reversed in [false, true] {
            let fake = Fake::new();
            let (a, b) = if reversed { (short, long) } else { (long, short) };
            for (plan, ends) in [("pA", a), ("pB", b)] {
                fake.push_preview(Ok(preview(vec![offer(plan, "", ends as i64)])));
                fake.push_claim(Ok(ok_claim()));
            }
            let s = sched(fake.clone(), Some(load.clone()), None);
            let w = s.tick_once(&Cancel::new()).await;
            assert_eq!(
                w,
                Duration::from_secs(5 * 60),
                "必须取**最短**（reversed={reversed}）：长 hold 的账号不能拖住其他账号"
            );
        }
    }

    #[tokio::test]
    async fn held_account_does_not_block_other_accounts() {
        let fake = Fake::new();
        // ua 领到 ⇒ hold 24h；ub 没有可领套餐 ⇒ 5 分钟后再探。
        fake.push_preview(Ok(preview(vec![offer("pa", "", (T0 + 24 * 3600) as i64)])));
        fake.push_claim(Ok(ok_claim()));
        fake.push_preview(Ok(preview(vec![])));
        let load: LoadCreds = Arc::new(|| Ok((vec![cred("ua"), cred("ub")], Vec::new())));
        let s = sched(fake.clone(), Some(load), None);

        assert_eq!(s.tick_once(&Cancel::new()).await, CLAIM_POLL_INTERVAL);
        assert_eq!(fake.preview_calls(), 2, "两个账号都探过");

        let w = s.tick_once(&Cancel::new()).await;
        assert_eq!(fake.preview_calls(), 3, "被 hold 的 ua 不发请求，ub 照探");
        assert_eq!(
            w,
            CLAIM_POLL_INTERVAL,
            "一个账号 hold 24h 不能让其他账号 24 小时不被检查"
        );
    }

    // ── 8. 无凭证 / 凭证加载失败 ─────────────────────────────────────

    #[tokio::test]
    async fn no_credentials_records_idle() {
        let fake = Fake::new();
        let load: LoadCreds = Arc::new(|| Ok((Vec::new(), Vec::new())));
        let s = sched(fake.clone(), Some(load), None);

        assert_eq!(s.tick_once(&Cancel::new()).await, CLAIM_POLL_INTERVAL);
        assert_eq!(s.last_result().outcome, ClaimTickOutcome::Idle);
        assert_eq!(fake.preview_calls(), 0);
    }

    #[tokio::test]
    async fn load_creds_error_records_error_and_cools_down() {
        let fake = Fake::new();
        let load: LoadCreds = Arc::new(|| Err("磁盘炸了".to_string()));
        let s = sched(fake.clone(), Some(load), None);

        assert_eq!(s.tick_once(&Cancel::new()).await, CLAIM_COOLDOWN);
        assert_eq!(s.last_result().outcome, ClaimTickOutcome::Error);
        assert_eq!(s.last_result().kind, ClaimFailureKind::Unknown);
        assert_eq!(s.last_result().msg, "加载凭证失败: 磁盘炸了");
        assert_eq!(fake.preview_calls(), 0);
    }

    #[tokio::test]
    async fn blank_uid_is_skipped() {
        let fake = Fake::new();
        let load: LoadCreds = Arc::new(|| Ok((vec![cred(""), cred("u1")], Vec::new())));
        fake.push_preview(Ok(preview(vec![])));
        let s = sched(fake.clone(), Some(load), None);

        assert_eq!(s.tick_once(&Cancel::new()).await, CLAIM_POLL_INTERVAL);
        assert_eq!(fake.preview_calls(), 1, "uid 为空的凭证必须跳过");
    }

    // ── 9. pick_offer / ends_at_time ─────────────────────────────────

    #[test]
    fn pick_offer_skips_blank_and_expired() {
        assert!(pick_offer(&[]).is_none());
        assert!(pick_offer(&[offer("", "active", 0)]).is_none(), "plan_id 空 ⇒ 跳过");
        assert!(
            pick_offer(&[offer("p", "Expired", 0)]).is_none(),
            "expired 要忽略大小写"
        );
        assert!(
            pick_offer(&[offer("p", " ENDED ", 0)]).is_none(),
            "ended 前后空白也要跳过"
        );
        let offers = [
            offer("", "", 0),
            offer("p2", "expired", 0),
            offer("p3", "", 0),
            offer("p4", "", 0),
        ];
        let picked = pick_offer(&offers).expect("p3 可领");
        assert_eq!(picked.plan_id, "p3", "取**第一个**可领的，不按 priority 排序");
    }

    #[test]
    fn ends_at_time_rejects_non_positive() {
        assert!(ends_at_time(&offer("p", "", 0)).is_none());
        assert!(ends_at_time(&offer("p", "", -1)).is_none());
        assert_eq!(
            ends_at_time(&offer("p", "", 3600)).unwrap(),
            UNIX_EPOCH + Duration::from_secs(3600)
        );
    }

    #[test]
    fn ends_at_time_survives_an_absurd_ends_at_instead_of_panicking() {
        // ★ 回归：`SystemTime + Duration` 溢出会 **panic**，而上游若给出荒唐的
        // `ends_at`（畸形响应），调度器会当场崩掉、整轮 tick 被带走。
        // Go 的 `time.Unix(n, 0)` 不 panic ⇒ 这里必须等价地「当作没有截止时间」。
        assert!(
            ends_at_time(&offer("p", "", i64::MAX)).is_none(),
            "溢出要退化成 None（不 hold），而不是 panic"
        );
    }

    // ── 10. set_hold / check_hold ────────────────────────────────────

    #[test]
    fn set_hold_clamps_to_cap_and_ignores_past_or_absent_end() {
        let fake = Fake::new();
        let s = sched(fake, None, None);

        assert_eq!(
            s.set_hold("u1", None),
            CLAIM_POLL_INTERVAL,
            "ends_at<=0 ⇒ 不 hold"
        );
        assert_eq!(
            s.set_hold("u1", Some(at(T0 - 1))),
            CLAIM_POLL_INTERVAL,
            "已经过去的 ends_at ⇒ 不 hold"
        );
        assert!(s.check_hold("u1", t0()).is_none());

        assert_eq!(
            s.set_hold("u2", Some(at(T0 + 48 * 3600))),
            CLAIM_HOLD_CAP,
            "48h 必须被 24h 上限截断"
        );
        assert_eq!(s.check_hold("u2", t0()).unwrap().1, CLAIM_HOLD_CAP);

        assert_eq!(
            s.set_hold("u3", Some(at(T0 + 3600))),
            Duration::from_secs(3600)
        );
        assert!(s.check_hold("u3", t0()).is_some());
        // 到点即放行。
        assert!(s.check_hold("u3", at(T0 + 3600)).is_none());
    }

    // ── 11. 分类表 ───────────────────────────────────────────────────

    #[test]
    fn claim_kind_of_code_covers_the_whole_table() {
        let table = [
            ("1001", ClaimFailureKind::NotFound),
            ("1002", ClaimFailureKind::Unavailable),
            ("1003", ClaimFailureKind::AlreadyClaimed),
            ("1004", ClaimFailureKind::Ineligible),
            ("1005", ClaimFailureKind::QuotaExhausted),
            ("3001", ClaimFailureKind::InvalidRequest),
            ("3007", ClaimFailureKind::Captcha),
            ("401", ClaimFailureKind::LoginRequired),
            ("", ClaimFailureKind::None),
            ("9999", ClaimFailureKind::None),
            (" 1003 ", ClaimFailureKind::AlreadyClaimed),
        ];
        for (code, want) in table {
            assert_eq!(claim_kind_of_code(code), want, "业务码 {code:?}");
        }
        // 跨实现契约：字符串值必须逐字一致。
        assert_eq!(ClaimFailureKind::NotFound.as_str(), "not_found");
        assert_eq!(ClaimFailureKind::QuotaExhausted.as_str(), "quota_exhausted");
        assert_eq!(ClaimFailureKind::LoginRequired.as_str(), "login_required");
        assert_eq!(ClaimFailureKind::None.as_str(), "");
        assert_eq!(ClaimTickOutcome::SkippedHold.as_str(), "skipped_hold");
        assert_eq!(ClaimTickOutcome::None.as_str(), "");
    }

    #[test]
    fn classify_claim_error_prefers_business_code_then_status() {
        let nf = up_err(404, "{\"error_msg\":\"404 Route Not Found\"}");
        assert_eq!(
            classify_claim_error(&nf),
            (ClaimFailureKind::NotFound, String::new())
        );
        assert_eq!(
            classify_claim_error(&up_err(401, "{}")),
            (ClaimFailureKind::LoginRequired, String::new())
        );
        assert_eq!(
            classify_claim_error(&up_err(500, "{}")),
            (ClaimFailureKind::HttpError, String::new())
        );
        assert_eq!(
            classify_claim_error(&up_err(403, "{\"code\":\"3007\"}")),
            (ClaimFailureKind::Captcha, "3007".to_string())
        );
        assert_eq!(
            classify_claim_error(&up_err(400, "{\"code\":\"1003\"}")),
            (ClaimFailureKind::AlreadyClaimed, "1003".to_string()),
            "业务码命中时优先于 HTTP 状态"
        );
        assert_eq!(
            classify_claim_error(&up_err(200, "{\"code\":\"9999\"}")),
            (ClaimFailureKind::Unknown, "9999".to_string()),
            "码表未命中 ⇒ 状态 200 不构成 http_error ⇒ Unknown"
        );
        assert_eq!(
            classify_claim_error(&ClaimError::Local("参数不合法".to_string())),
            (ClaimFailureKind::Unknown, String::new()),
            "本地错误不罚账号，也不带 HTTP 语义"
        );
    }

    // ── 12. 手动领取 ────────────────────────────────────────────────

    #[tokio::test]
    async fn claim_once_without_client_reports_uninitialised() {
        let res = claim_once(None, &cred("u1"), None).await;
        assert_eq!(res.outcome, ClaimTickOutcome::Error);
        assert_eq!(res.kind, ClaimFailureKind::Unknown);
        assert_eq!(res.msg, "zcode dispatch 未初始化");
    }

    #[tokio::test]
    async fn claim_once_guards_missing_upstream_even_when_a_scheduler_is_passed() {
        // ★ 守卫顺序回归：Go 是 `d == nil || d.client == nil` **先于** `sched == nil`。
        // 若反过来，上游没接好但传了调度器时就会绕过守卫、照样去发请求。
        let fake = Fake::new();
        let s = sched(fake.clone(), Some(load_one("u1")), None);
        let res = claim_once(None, &cred("u1"), Some(&s)).await;
        assert_eq!(res.outcome, ClaimTickOutcome::Error);
        assert_eq!(res.msg, "zcode dispatch 未初始化");
        assert_eq!(fake.preview_calls(), 0, "守卫命中就一次网络都不该发");
    }

    #[tokio::test]
    async fn claim_once_without_scheduler_uses_a_temporary_hold_store() {
        let fake = Fake::new();
        fake.push_preview(Ok(preview(vec![offer("p9", "", 0)])));
        fake.push_claim(Ok(ok_claim()));
        let up: &dyn ClaimUpstream = &*fake;

        let res = claim_once(Some(up), &cred("u1"), None).await;
        assert_eq!(res.outcome, ClaimTickOutcome::Claimed);
        assert_eq!(res.plan_id, "p9");
        assert_eq!(
            res.next_in, CLAIM_POLL_INTERVAL,
            "ends_at<=0 ⇒ 没有 hold ⇒ 正常轮询"
        );
    }

    #[tokio::test]
    async fn claim_once_bypasses_hold_but_reuses_the_scheduler_logic() {
        let fake = Fake::new();
        fake.push_preview(Ok(preview(vec![offer("p1", "", (T0 + 3600) as i64)])));
        fake.push_claim(Ok(ok_claim()));
        let s = sched(fake.clone(), Some(load_one("u1")), None);

        let w = s.tick_once(&Cancel::new()).await;
        assert_eq!(w, Duration::from_secs(3600));
        assert!(s.check_hold("u1", t0()).is_some(), "已进入 hold");

        fake.push_preview(Ok(preview(vec![offer("p2", "", (T0 + 7200) as i64)])));
        fake.push_claim(Ok(ok_claim()));
        let up: &dyn ClaimUpstream = &*fake;
        let res = claim_once(Some(up), &cred("u1"), Some(&s)).await;
        assert_eq!(res.outcome, ClaimTickOutcome::Claimed);
        assert_eq!(res.plan_id, "p2");
        assert_eq!(fake.preview_calls(), 2, "手动路径绕过 hold（用户要「现在就看」）");
    }

    // ── 13. 文案与格式化 ────────────────────────────────────────────

    #[test]
    fn summary_matches_go_text() {
        let fake = Fake::new();
        let s = sched(fake, None, None);
        assert_eq!(
            s.summary(),
            "自动领取已启动（每 5m0s 轮询，失败冷却 10m0s，成功/已领后 hold 到套餐截止）"
        );
        assert_eq!(summary_of(None), "未启用");
        assert_eq!(summary_of(Some(&s)), s.summary());
    }

    #[test]
    fn go_duration_matches_go_formatting() {
        assert_eq!(go_duration(CLAIM_POLL_INTERVAL), "5m0s");
        assert_eq!(go_duration(CLAIM_COOLDOWN), "10m0s");
        assert_eq!(go_duration(CLAIM_HOLD_CAP), "24h0m0s");
        assert_eq!(go_duration(Duration::from_secs(2 * 3600)), "2h0m0s");
        assert_eq!(go_duration(Duration::from_secs(3661)), "1h1m1s");
        assert_eq!(go_duration(Duration::from_secs(45)), "45s");
        assert_eq!(go_duration(Duration::ZERO), "0s");
    }

    #[test]
    fn round_minute_rounds_half_away_from_zero() {
        assert_eq!(
            round_minute(Duration::from_secs(2 * 3600 + 29)),
            Duration::from_secs(2 * 3600)
        );
        assert_eq!(
            round_minute(Duration::from_secs(2 * 3600 + 30)),
            Duration::from_secs(2 * 3600 + 60)
        );
        assert_eq!(
            round_minute(Duration::from_secs(59)),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn short_uid_log_truncates_to_eight_bytes_without_ellipsis() {
        assert_eq!(short_uid_log(""), "");
        assert_eq!(short_uid_log("12345678"), "12345678");
        assert_eq!(short_uid_log("123456789"), "12345678");
        assert_eq!(short_uid_log("0123456789abcdef"), "01234567");
        assert!(
            !short_uid_log("0123456789abcdef").contains('…'),
            "Go 版是纯截断，与 mod.rs::mask_uid 的 `…` 不同"
        );
    }

    // ── 14. 时钟注入 ────────────────────────────────────────────────

    #[tokio::test]
    async fn injected_clock_drives_hold_expiry() {
        let fake = Fake::new();
        let t = Arc::new(Mutex::new(t0()));
        let t2 = t.clone();
        let s = ClaimScheduler::from_upstream(
            fake.clone(),
            Some(load_one("u1")),
            None,
            Some(Arc::new(move || *t2.lock().unwrap())),
        );

        fake.push_preview(Ok(preview(vec![offer("p1", "", (T0 + 3600) as i64)])));
        fake.push_claim(Ok(ok_claim()));
        assert_eq!(s.tick_once(&Cancel::new()).await, Duration::from_secs(3600));

        // 时钟往前推 1 小时：hold 到点 ⇒ 必须重新探测，且这次没有可领套餐。
        *t.lock().unwrap() = at(T0 + 3600);
        fake.push_preview(Ok(preview(vec![])));
        assert_eq!(s.tick_once(&Cancel::new()).await, CLAIM_POLL_INTERVAL);
        assert_eq!(fake.preview_calls(), 2, "hold 到点后必须重新探测");
    }
}
