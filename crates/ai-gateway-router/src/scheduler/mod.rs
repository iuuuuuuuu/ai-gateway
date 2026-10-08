//! 养号任务调度器（对应 Go `internal/scheduler/*.go`）。
//!
//! 子模块对应关系：
//!
//! | 本模块        | Go 源文件        | 职责                             |
//! |---------------|------------------|----------------------------------|
//! | `mod`         | `scheduler.go`   | 配置、任务注册、唤醒时刻、签到    |
//! | `activity`    | `activity.go`    | 活跃度上报（点亮连登）            |
//! | `nightowl`    | `nightowl.go`    | 夜猫窗口判定                      |
//! | `school`      | `school.go`      | 开学季任务领取                    |
//! | `trial`       | `trial.go`       | 国际版试用加油包                  |
//! | `travel`      | `travel.go`      | 宠物旅行派出/领取/领养            |
//! | `growthmap`   | `growthmap.go`   | 活跃地图：补签/兑换/抽奖/盲盒     |
//!
//! # 为什么调度器自己写 `account_records.json`
//!
//! 这 4 个任务（活跃上报 / 夜猫子 / 开学季 / trial）的日志此前只走 `log.Printf`
//! 写 stdout，而宿主启动网关子进程时把 stdout/stderr 丢进了 `Stdio::null` ——
//! 任务照跑，但界面上一条执行痕迹都没有（所有者报的正是这个现象）。
//! 改为写宿主已经在读的 `account_records.json`，记录就能与签到并列出现在
//! 「账号记录 → 任务」里。见 [`crate::records`]。
//!
//! # 与 Go 的两处结构性差异（非语义差异）
//!
//! 1. **取消信号**用 [`Cancel`] 而不是 `context.Context`；Go 的
//!    `context.WithValue` 账号作用域改成了显式的 `Option<&str>` 参数
//!    （类型安全，且不必为每个任务各自 `ctx.Value(...)` 解包一次）。
//! 2. **两个注入式执行体**（产品日常任务 / 计费周期刷新）用
//!    [`std::sync::RwLock`] 做内部可变，因此 `set_*_runner` 只取 `&self`
//!    —— Go 那边靠 `*Scheduler` 指针直接改 `cfg` 字段。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use chrono::{DateTime, Local};
use futures_util::future::BoxFuture;
use serde::Serialize;

use crate::auth::{Auth, PRODUCT_WORKBUDDY};
use crate::error::GatewayError;
use crate::pool::{Pool, Status};
use crate::records::Recorder;
use crate::upstream::classify::ErrKind;
use crate::upstream::client::{Client, CreditInfo};

pub mod nightowl;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 每号每天的活跃上报条数（对应 Go `defaultActivityReportCount`）。
///
/// 取 3 而非 1：单条上报偶发被服务端丢弃（见 [`Scheduler::read_streak_state`]），
/// 多条提高点亮成功率；也不宜过多，避免被风控当成异常流量。
pub const DEFAULT_ACTIVITY_REPORT_COUNT: i32 = 3;

/// 同账号两条上报之间的间隔，避免秒发触发风控。
pub const ACTIVITY_REPORT_GAP: Duration = Duration::from_millis(800);

/// 账号之间的间隔，摊平请求。
pub const ACTIVITY_ACCOUNT_DELAY: Duration = Duration::from_millis(400);

/// trial 任务账号之间的间隔。
pub const TRIAL_GAP: Duration = Duration::from_millis(400);

/// 积分巡检账号之间的间隔，避免瞬间并发打满上游。
pub const CREDIT_REFRESH_GAP: Duration = Duration::from_millis(300);

/// 积分到期巡检的默认周期。
///
/// # 为什么需要独立于签到的高频巡检
///
/// 到期日决定选号优先级，而它会随消费变化（快过期的额度烧完后，该账号的最近
/// 到期日跳到下一档，应立刻让出流量）。签到每天只跑两次，间隔太久会让分层选号
/// 长期依据过时数据。15 分钟 × 账号数 的请求量相对上游可忽略，且巡检本身不签到、
/// 不改账号状态。
pub const DEFAULT_CREDIT_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// 启动后补跑最近错过的时点，避免网关在计划时刻前后重启/机器唤醒后直接等到
/// 第二天。只补最近一轮，不会把多天任务堆成风暴。
pub const STARTUP_CATCH_UP_GRACE: Duration = Duration::from_secs(30 * 60);

/// 任务名（对外稳定标识，对应 Go `TaskName*`）。
///
/// # 为什么用字符串而不是 [`TaskKind`] 暴露给宿主
///
/// `TaskKind` 是内部排程的实现细节（顺序会随排程重构变化），而宿主
///（GUI / webui）需要的是稳定的对外标识。
pub const TASK_NAME_ACTIVITY: &str = "activity";
/// 夜猫子补报（见 [`nightowl`]）。
pub const TASK_NAME_NIGHT_OWL: &str = "nightowl";
/// 开学季任务领取。
pub const TASK_NAME_SCHOOL: &str = "school";
/// 国际版 trial 加油包。
pub const TASK_NAME_TRIAL: &str = "trial";
/// 活跃地图闭环。
///
/// 它**没有独立排程时点**（并入活跃上报），但仍注册为可手动触发的任务名 ——
/// 日排程每个账号一天只跑一轮，用户想立刻确认「我的补签卡/抽奖次数有没有被
/// 处理」时需要入口。
pub const TASK_NAME_GROWTH_MAP: &str = "growthmap";

/// 产品日常任务（Qoder 权益领取 / ZCode claim）。
///
/// # 与参考实现的差异（刻意）
///
/// 所有者要求「qoder这个活动卡片…而且任务也应该自动执行」「他那个仓库还有个
/// 自动领取那个积分包的功能，我们也要接进来」。这与 `internal/zcode/claim.go`
/// 文件头「**不做定时自动抢** —— 那会让账号表现出非人类的活动模式」不矛盾：
/// 反对的是「自动**抢**」= 高频探测 + 抢限量名额（参考实现默认 5 分钟一轮）；
/// 所有者要的是「自动做掉每天的幂等任务」。
///
/// ```text
/// 抢：高频（5 分钟）、有竞争、失败要重试 ← 那才是非人类画像
/// 做：每天一次、幂等（已领会返回"已领过"）、失败不重试
/// ```
///
/// 故刻意与参考实现的 5 分钟轮询不同：每天固定时点跑一次、**串行**（同一时刻
/// 只跑一个产品，避免并发指纹）、**有界重试**（失败不在同一轮紧密重试；配置了
/// 早晚两个时点则下一时点自动补跑）。两端点都幂等（Qoder `replayed:true` /
/// ZCode `1003`）。
pub const TASK_NAME_PRODUCT_TASKS: &str = "product_tasks";

/// 全部可手动触发的任务名（`/tasks/run?task=<name>` 的白名单）。
pub const TASK_NAMES: [&str; 6] = [
    TASK_NAME_ACTIVITY,
    TASK_NAME_NIGHT_OWL,
    TASK_NAME_SCHOOL,
    TASK_NAME_TRIAL,
    TASK_NAME_GROWTH_MAP,
    TASK_NAME_PRODUCT_TASKS,
];

// ---------------------------------------------------------------------------
// 取消信号
// ---------------------------------------------------------------------------

/// 协作式取消信号（对应 Go 的 `context.Context`）。
///
/// # 为什么不用 `Notify`
///
/// `Notify::notify_waiters()` 只唤醒**当前**的等待者，存在「先查标志（未取消）→
/// 取消发生（无等待者）→ 才 `await notified()`」的丢唤醒窗口，而那正好是调度器
/// 每次等待唤醒时点都会走的路径。`watch` 带版本号，`changed()` 只对**新**变更
/// 返回，配合先查一次当前值即可闭合该窗口。
#[derive(Clone)]
pub struct Cancel {
    tx: tokio::sync::watch::Sender<bool>,
    rx: tokio::sync::watch::Receiver<bool>,
}

impl Default for Cancel {
    fn default() -> Self {
        Self::new()
    }
}

impl Cancel {
    pub fn new() -> Self {
        let (tx, rx) = tokio::sync::watch::channel(false);
        Self { tx, rx }
    }

    /// 请求取消；幂等。
    pub fn cancel(&self) {
        let _ = self.tx.send(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// 等到取消发生。
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let mut rx = self.rx.clone();
        while rx.changed().await.is_ok() {
            if *rx.borrow() {
                return;
            }
        }
    }

    /// 睡 `d` 后返回 `true`；若期间被取消则立即返回 `false`
    ///（对应 Go `sleepCtx` 的 `bool` 返回值）。
    pub async fn sleep(&self, d: Duration) -> bool {
        if self.is_cancelled() {
            return false;
        }
        let mut rx = self.rx.clone();
        tokio::select! {
            _ = tokio::time::sleep(d) => !self.is_cancelled(),
            _ = async {
                while rx.changed().await.is_ok() {
                    if *rx.borrow() {
                        break;
                    }
                }
            } => false,
        }
    }
}

// ---------------------------------------------------------------------------
// 任务类型与结果
// ---------------------------------------------------------------------------

/// 内部排程任务类型（对应 Go `taskKind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskKind {
    Checkin,
    Keepalive,
    Activity,
    NightOwl,
    School,
    Trial,
    /// Qoder 权益活动领取。
    QoderClaim,
    /// 活跃地图闭环，与每日活跃上报同一时点触发。
    GrowthMap,
}

/// 一轮任务的结果（对应 Go `TaskRunResult`）。
///
/// # 为什么要回报「跑了没有」而不只是成功/失败
///
/// 这些任务都有前置条件（夜猫子限时段、开学季限活动期、活跃上报与开学季只跑
/// 国服），不满足时**跳过是正确行为**而非错误。宿主据此在界面上说明原因，
/// 否则用户点了「立即执行」看不到任何变化，会以为功能坏了。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TaskRunResult {
    pub task: String,
    /// `false` = 被前置条件挡下。
    pub ran: bool,
    /// 跳过原因码；`ran = true` 时为空。
    ///
    /// 用稳定的英文码而非直接给中文：宿主可据此分支，文案留给 [`Self::message`]，
    /// 改文案不会破坏调用方的判断。
    #[serde(skip_serializing_if = "String::is_empty")]
    pub skip: String,
    /// 面向用户的中文说明。
    pub message: String,
}

impl TaskRunResult {
    /// 跳过（`ran = false`，带稳定原因码与面向用户的说明）。
    pub fn skipped(task: &str, skip: &str, message: &str) -> Self {
        Self {
            task: task.to_string(),
            ran: false,
            skip: skip.to_string(),
            message: message.to_string(),
        }
    }

    /// 已触发一轮。
    pub fn ran(task: &str, message: &str) -> Self {
        Self {
            task: task.to_string(),
            ran: true,
            skip: String::new(),
            message: message.to_string(),
        }
    }
}

/// 调度器的错误（对应 Go 的 `ErrTaskRunning` 与 `fmt.Errorf("unknown task %q")`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerError {
    /// 一轮活跃上报按账号数 × 条数 × 间隔串行跑，大账号池下可长达数分钟。
    /// 用户连点「立即执行」若不拦，会叠加出成倍的重复上报 —— 那正是活跃上报
    /// 刻意控制条数要规避的风控画像。
    TaskRunning,
    UnknownTask(String),
}

impl std::fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TaskRunning => f.write_str("task already running"),
            Self::UnknownTask(name) => write!(f, "unknown task {name:?}"),
        }
    }
}

impl std::error::Error for SchedulerError {}

/// `ErrTaskRunning` 的判定（对应 Go `errors.Is(err, scheduler.ErrTaskRunning)`）。
pub fn is_task_running(e: &SchedulerError) -> bool {
    matches!(e, SchedulerError::TaskRunning)
}

// ---------------------------------------------------------------------------
// 注入式执行体
// ---------------------------------------------------------------------------

/// 产品日常任务执行体：返回 `(detail, err)`。
pub type ProductTasksRunner =
    Arc<dyn Fn() -> BoxFuture<'static, (String, Option<String>)> + Send + Sync>;

/// 计费周期刷新执行体。
pub type BillingCycleRunner = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

/// 调度器配置（对应 Go `scheduler.Config`）。
///
/// # 关于 `*_disabled` 而不是 `*_enabled`
///
/// 与 Go 同款：零值配置即「两类任务都启用」，与引入开关前的行为逐字一致 ——
/// 若用 `enabled` 命名，`Config::default()` 会静默关掉全部任务。
#[derive(Clone)]
pub struct Config {
    /// 账号池。`None` = 没有账号可跑（对应 Go `poolList` 的 nil 收口）。
    ///
    /// Go 的注释记着 2026-09-27 的实测崩溃：启动补跑让 `Run()` 在启动瞬间就可能
    /// 执行任务，而单测普遍用 `New(Config{...})` 构造调度器**不给 Pool**，
    /// 于是 `Run → 补跑 trial → Pool 为 nil → panic`。那些用例的行为是**正确**的
    ///（它们只断言「ctx 取消后 Run 能返回」），不该为每个用例补 Pool，
    /// 故在**读池这一个点**上收口：池不存在 = 没有账号可跑 = 空列表。
    /// 生产路径的 Pool 恒非 `None`（`bin/gateway.rs` 必注入）。
    pub pool: Option<Arc<Mutex<Pool>>>,
    /// 上游客户端。
    pub upstream: Client,

    /// 签到时点，默认 `[9, 21]`。
    pub checkin_hours: Vec<i32>,
    /// token 保活时点，默认 `[22]`。
    pub keepalive_hours: Vec<i32>,
    /// 活跃上报时点，默认 `[10]`。
    ///
    /// 签到只恢复余额，**连登天数**要靠对话活跃上报点亮；一条
    /// `chat_request_send` 同时点亮连登 + 解锁 `first_buddy`（领养前置）
    /// ⇒ 它是「能领养」的前提。
    pub activity_hours: Vec<i32>,
    /// trial 加油包时点，默认 `[9, 21]`。
    ///
    /// 国际版没有签到、没有任务中心，trial 是其唯一积分增益动作；上游用
    /// 14051 表达「已领过」，客户端视为幂等成功，故可每天重试。
    pub trial_hours: Vec<i32>,
    /// 开学季时点，默认 `[12]`。
    ///
    /// 该活动是**限时**的，服务端下发 `in_period`，下线后自动跳过；只领取
    /// 已达标的奖励，不伪造完成动作。
    pub school_hours: Vec<i32>,
    /// 夜猫子时点，默认 `[1]`。
    ///
    /// growth 有时段敏感任务只在夜猫窗口（23:00–08:00 CST）内计入，
    /// 01 点避开 22 点的 token 保活。
    pub night_owl_hours: Vec<i32>,

    pub checkin_disabled: bool,
    pub keepalive_disabled: bool,
    pub activity_disabled: bool,
    pub night_owl_disabled: bool,
    pub school_disabled: bool,
    pub trial_disabled: bool,

    /// Qoder 权益领取时点，默认 `[10, 21]`。
    ///
    /// 所有者（2026-09-22）「qoder改为 早十点,晚九点 两次触发,防止错漏」；
    /// 活动每天 10:00（UTC+8）重置，单条时限约 22 小时；只排一个时点则任何
    /// 抖动都让当天彻底领不到（上游 5xx / 超时 / 恰在该时点前已跑过 / 网关当时
    /// 没在运行）。10 点是重置后第一轮，21 点再确认；端点幂等。
    ///
    /// ★ 与 Go 同：**没有默认值兜底** —— 空切片 ⇒ 该 slot 不产生时点。
    pub qoder_claim_hours: Vec<i32>,
    /// 对应 `schedule.qoder_claim_enabled = false` **或**总闸
    /// `schedule.product_tasks_enabled = false`（两级回落由配置层算好）。
    pub qoder_claim_disabled: bool,

    /// 每号每天的活跃上报条数，默认 [`DEFAULT_ACTIVITY_REPORT_COUNT`]。
    pub activity_report_count: i32,
    /// 签到 + 猫猫旅行 + 活跃上报覆盖的账号区域：`"cn"`（缺省，仅国服）/ `"all"`。
    ///
    /// 国际版（workbuddy.ai）的 billing 与 growth 接口暂无真实数据，默认跳过；
    /// **token 保活不受此限制**（两个区域都需要刷新）。
    pub checkin_scope: String,
    /// 账号记录回写器；`None` = 不记录。
    pub records: Option<Arc<Recorder>>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("pool", &self.pool.as_ref().map(|_| "<pool>"))
            .field("checkin_hours", &self.checkin_hours)
            .field("keepalive_hours", &self.keepalive_hours)
            .field("activity_hours", &self.activity_hours)
            .field("trial_hours", &self.trial_hours)
            .field("school_hours", &self.school_hours)
            .field("night_owl_hours", &self.night_owl_hours)
            .field("qoder_claim_hours", &self.qoder_claim_hours)
            .field("activity_report_count", &self.activity_report_count)
            .field("checkin_scope", &self.checkin_scope)
            .field("records", &self.records.as_ref().map(|_| "<recorder>"))
            .finish_non_exhaustive()
    }
}

impl Config {
    /// 只填上游客户端的最小配置（时点与开关走 [`Scheduler::new`] 的默认值）。
    pub fn new(upstream: Client) -> Self {
        Self {
            pool: None,
            upstream,
            checkin_hours: Vec::new(),
            keepalive_hours: Vec::new(),
            activity_hours: Vec::new(),
            trial_hours: Vec::new(),
            school_hours: Vec::new(),
            night_owl_hours: Vec::new(),
            checkin_disabled: false,
            keepalive_disabled: false,
            activity_disabled: false,
            night_owl_disabled: false,
            school_disabled: false,
            trial_disabled: false,
            qoder_claim_hours: Vec::new(),
            qoder_claim_disabled: false,
            activity_report_count: 0,
            checkin_scope: String::new(),
            records: None,
        }
    }
}

// ---------------------------------------------------------------------------
// 调度器
// ---------------------------------------------------------------------------

/// 养号任务调度器（对应 Go `Scheduler`）。
pub struct Scheduler {
    cfg: Config,

    /// 产品日常任务执行体（`None` = 未接线）。
    ///
    /// # 为什么用 setter 而不是配置字段
    ///
    /// 与 Go 同因：执行体依赖 ZCode / Qoder 的派发器，而它在 `bin/gateway.rs` 里的
    /// 构造位置**晚于**调度器（初始化顺序所致）。用 setter 让「先建调度器、
    /// 后接线执行体」成为合法顺序，而不必为了一个字段去挪动一大片初始化代码。
    ///
    /// ⚠ 必须在 [`Scheduler::run`] **之前**调用：排程循环启动时读一次该字段来算
    /// 唤醒时点；之后注入不会让本轮的唤醒计划生效（要等下一个整点重算）。
    product_tasks_runner: RwLock<Option<ProductTasksRunner>>,
    /// 计费周期刷新执行体（`None` = 未接线，直接跳过不报错）。
    ///
    /// 与积分巡检**语义不同**，混在一起会互相污染：积分巡检（WorkBuddy）有出站
    /// 写语义的余额刷新，判产品闸门后只跑 WorkBuddy；计费周期（Qoder）纯只读
    /// 状态轮询，只跑 Qoder。⚠ 类型只用到取消信号：本模块不依赖产品实现。
    ///
    /// `None` = 该构建未接线（**直接跳过，不 panic**）—— 单产品部署必须能跑。
    ///
    /// ⚠ 必须在 [`Scheduler::run_credit_refresh_loop`] **之前**调用：该循环启动时
    /// 立刻跑一轮；在循环启动后才注入会漏掉启动那一轮（要等下一个 interval）。
    billing_cycle_runner: RwLock<Option<BillingCycleRunner>>,

    /// 领养当日失败记录：uid → 自然日（CST）。
    ///
    /// 门槛未达的账号当日不再重试，避免同日多趟对上游重试轰炸；进程重启即清零
    ///（无需持久化）。`growth_claimed` 是同一把锁下的「活跃地图当日已跑」标记。
    /// 两类任务都是「每日一轮」的养号动作，用同一把锁即可：它们只在写各自 map 时
    /// 短暂持有，不跨网络请求，不会把排程拖慢。
    daily: Mutex<DailyMarks>,

    /// 手动触发的在跑标记。与 [`Self::daily`] 分开：排程循环会自动跑同一批任务，
    /// 共用一把锁会让「手动触发」与「到点执行」互相阻塞，把定时任务拖慢。
    ///
    /// 手动触发与到点排程共用这一组标记：它们在跑同一批上游请求，若互不感知，
    /// 用户恰好在 10:00 点「立即执行」就会与排程那轮叠成双倍上报。
    task_busy: Mutex<HashSet<String>>,
}

#[derive(Default)]
struct DailyMarks {
    adopt_tried: HashMap<String, String>,
    growth_claimed: HashMap<String, String>,
}

/// `claim_task` 的 RAII 释放（对应 Go `defer releaseTask`）。
struct TaskBusyGuard<'a> {
    sched: &'a Scheduler,
    name: String,
}

impl Drop for TaskBusyGuard<'_> {
    fn drop(&mut self) {
        self.sched.release_task(&self.name);
    }
}

impl Scheduler {
    /// 构建（对应 Go `New`）。
    pub fn new(cfg: Config) -> Self {
        let mut cfg = cfg;
        if cfg.checkin_hours.is_empty() {
            cfg.checkin_hours = vec![9, 21];
        }
        if cfg.keepalive_hours.is_empty() {
            cfg.keepalive_hours = vec![22];
        }
        if cfg.activity_hours.is_empty() {
            cfg.activity_hours = vec![10];
        }
        if cfg.night_owl_hours.is_empty() {
            cfg.night_owl_hours = vec![1];
        }
        if cfg.school_hours.is_empty() {
            cfg.school_hours = vec![12];
        }
        if cfg.trial_hours.is_empty() {
            cfg.trial_hours = vec![9, 21];
        }
        if cfg.activity_report_count <= 0 {
            cfg.activity_report_count = DEFAULT_ACTIVITY_REPORT_COUNT;
        }
        // ★ `qoder_claim_hours` 刻意**不兜底**（与 Go 逐字一致）：空切片 ⇒
        //   `next_fire` 返回 None ⇒ 该 slot 不产生时点。
        Self {
            cfg,
            product_tasks_runner: RwLock::new(None),
            billing_cycle_runner: RwLock::new(None),
            daily: Mutex::new(DailyMarks::default()),
            task_busy: Mutex::new(HashSet::new()),
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    // ---- 注入式执行体 ----

    /// 注入产品日常任务的执行体（对应 Go `SetProductTasksRunner`）。
    pub fn set_product_tasks_runner(&self, runner: ProductTasksRunner) {
        *self.product_tasks_runner.write().unwrap_or_else(|e| e.into_inner()) = Some(runner);
    }

    /// 注入计费周期刷新的执行体（对应 Go `SetBillingCycleRefreshRunner`）。
    pub fn set_billing_cycle_refresh_runner(&self, runner: BillingCycleRunner) {
        *self.billing_cycle_runner.write().unwrap_or_else(|e| e.into_inner()) = Some(runner);
    }

    fn product_tasks_runner(&self) -> Option<ProductTasksRunner> {
        self.product_tasks_runner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn billing_cycle_runner(&self) -> Option<BillingCycleRunner> {
        self.billing_cycle_runner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    // ---- 账号作用域 ----

    /// 账号作用域判定（对应 Go `inAccountScope`）。
    ///
    /// 无作用域（排程、或「作用于全部账号」的手动触发）时**恒为 true** ——
    /// 这是本机制能与既有排程共存的关键：排程路径不设作用域，行为逐字不变。
    fn in_account_scope(scope: Option<&str>, uid: &str) -> bool {
        match scope {
            None => true,
            Some("") => true,
            Some(s) => s == uid,
        }
    }

    /// 作用域内的账号 UID（空串 = 未限定）。
    ///
    /// # 用途：区分「手动触发某个账号」与「自动排程」
    ///
    /// 两者的记录策略必须不同（所有者 2026-09-20 反馈「我手动执行了但是查看记录
    /// 却没有」）：
    ///
    /// - 手动（非空）—— 用户盯着结果 ⇒ 无论成败都要留**账号级**记录；
    /// - 自动（空）—— 没人看 ⇒ 只在「有新变化」时写，避免把记录刷成噪音。
    ///
    /// 判据就是「有没有人在看」，而这里正是那个信息的唯一来源。
    fn account_scope_uid(scope: Option<&str>) -> &str {
        scope.unwrap_or("")
    }

    // ---- 记录回写（Go 侧是 nil-safe 的方法调用，Rust 侧收口在这三个 helper） ----

    fn rec_task(&self, uid: &str, title: &str, result: &str, detail: &str) {
        if let Some(r) = &self.cfg.records {
            r.task(uid, title, result, detail);
        }
    }

    fn rec_task_daily(&self, uid: &str, title: &str, result: &str, detail: &str) {
        if let Some(r) = &self.cfg.records {
            r.task_daily(uid, title, result, detail);
        }
    }

    fn rec_task_all_daily(&self, title: &str, result: &str, detail: &str) {
        if let Some(r) = &self.cfg.records {
            r.task_all_daily(title, result, detail);
        }
    }

    // ---- 任务互斥 ----

    /// 认领一个任务名；已在跑则返回 `false`（对应 Go `claimTask`）。
    fn claim_task(&self, name: &str) -> bool {
        let mut busy = self.task_busy.lock().unwrap_or_else(|e| e.into_inner());
        busy.insert(name.to_string())
    }

    fn release_task(&self, name: &str) {
        let mut busy = self.task_busy.lock().unwrap_or_else(|e| e.into_inner());
        busy.remove(name);
    }

    /// 到点执行一个养号任务，并与手动触发互斥（对应 Go `runCareTask`）。
    ///
    /// 与手动触发的唯一区别：排程这轮被占用时**记一行日志就走**，不排队。
    /// 排队会让「迟到唤醒 + 用户刚点过立即执行」叠成两轮；而下一个整点很快就会
    /// 再来，漏掉一轮的代价远小于双倍上报。
    async fn run_care_task<F>(&self, name: &str, run: F)
    where
        F: std::future::Future<Output = ()>,
    {
        if !self.claim_task(name) {
            crate::logline!("{name}: skipped (a manual run is already in progress)");
            return;
        }
        let _guard = TaskBusyGuard {
            sched: self,
            name: name.to_string(),
        };
        run.await;
    }

    // ---- 产品闸门 / 区域闸门 ----

    /// 签到范围判定（对应 Go `checkinScopeAllows`）。
    fn checkin_scope_allows(&self, a: &Auth) -> bool {
        self.cfg.checkin_scope.trim().eq_ignore_ascii_case("all") || !a.is_intl()
    }

    /// 产品闸门 + 区域闸门（对应 Go `accountScopeSkip`）。`Err(msg)` = 跳过并回报原因。
    ///
    /// 口径与各任务遍历循环里的过滤**逐条对应** —— 两处若不一致，就会出现
    /// 「预检说能跑、实际跑空」的矛盾。
    ///
    /// # 产品闸门（2026-09-22 修的真实缺陷）
    ///
    /// 所有者现场：「这个qoder怎么执行workbuddy的任务了?」「zcode我看到记录里面,
    /// 也会跑workbuddy的任务,这不要串任务好吗?」
    ///
    /// 根因：本函数此前**只判区域、完全不判产品**，而所有养号任务都遍历
    /// `Pool::list()`（含**所有产品**的账号）⇒ 打到 WorkBuddy 的 growth 端点带着
    /// Qoder 凭证 ⇒ **401**（记录里那条「开学季活动…upstream client (http 401)」
    /// 就是它），并写下**根本不属于它**的任务记录。
    ///
    /// ⚠ [`Self::checkin_scope_allows`] 拦不住：它只看 `Auth::is_intl`，而
    /// Qoder / ZCode 的 domain **不是** workbuddy.ai ⇒ 被判成「国服」⇒ 放行。
    /// **区域与产品是两个正交的维度**，不能用其中一个代替另一个。
    ///
    /// 判据用 [`Auth::product_of`]（把空值与 `"workbuddy"` 都归成 workbuddy），
    /// **不是** `a.product == ""`（后者会漏掉显式写了 `"workbuddy"` 的账号）。
    fn account_scope_skip(&self, name: &str, a: &Auth) -> Result<(), String> {
        // 白名单式判断（列「谁能跑」而不是「谁不能跑」）：将来新增产品时默认
        // **不参与**，而不是默认参与 —— 前者是安全的失败方向。
        match name {
            TASK_NAME_ACTIVITY | TASK_NAME_NIGHT_OWL | TASK_NAME_SCHOOL | TASK_NAME_GROWTH_MAP
            | TASK_NAME_TRIAL => {
                if a.product_of() != PRODUCT_WORKBUDDY {
                    return Err("该任务是 WorkBuddy 专属：其它产品有自己的任务体系".to_string());
                }
            }
            _ => {}
        }
        match name {
            TASK_NAME_TRIAL => {
                if !a.is_intl() {
                    return Err("trial 加油包仅国际版可用：国服没有这个端点".to_string());
                }
            }
            _ => {
                if !self.checkin_scope_allows(a) {
                    return Err("该任务是国服专属：国际版没有对应的数据接口".to_string());
                }
            }
        }
        Ok(())
    }

    // ---- 账号池读取 ----

    /// 账号池快照；池不存在时返回空表（对应 Go `poolList`）。
    fn pool_list(&self) -> Vec<Status> {
        match &self.cfg.pool {
            None => Vec::new(),
            Some(p) => p.lock().unwrap_or_else(|e| e.into_inner()).list(),
        }
    }

    fn auth_by_uid(&self, uid: &str) -> Option<Auth> {
        self.cfg
            .pool
            .as_ref()?
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .auth_by_uid(uid)
    }

    fn lock_pool(&self) -> Option<std::sync::MutexGuard<'_, Pool>> {
        Some(self.cfg.pool.as_ref()?.lock().unwrap_or_else(|e| e.into_inner()))
    }

    // ---- 排程时点 ----

    /// `now` 之后最近的一个整点触发时间；`hours` 为本地小时。
    ///
    /// 与 Go `nextFire` 同：**不**做 0-23 边界检查（Go 的 `time.Date` 对越界小时
    /// 会自然进位，这里用「当日零点 + hour 小时」复刻同一归一化行为）。
    pub fn next_fire(now: DateTime<Local>, hours: &[i32]) -> Option<DateTime<Local>> {
        let mut earliest: Option<DateTime<Local>> = None;
        for &h in hours {
            let Some(mut t) = local_at_hour(now, h) else {
                continue;
            };
            if t <= now {
                t += chrono::Duration::hours(24);
            }
            if earliest.is_none_or(|e| t < e) {
                earliest = Some(t);
            }
        }
        earliest
    }

    /// `now` 之后最近的唤醒时刻，以及该时刻需要执行的全部任务（对应 Go `nextWake`）。
    ///
    /// 多类任务若配到同一小时（如签到与旅行都含 9），该时刻需一并执行。
    /// 已显式禁用的任务不进候选。
    pub fn next_wake(&self, now: DateTime<Local>) -> (Option<DateTime<Local>>, Vec<TaskKind>) {
        let mut slots: Vec<(DateTime<Local>, TaskKind)> = Vec::new();
        let mut push = |hours: &[i32], kind: TaskKind| {
            if let Some(at) = Self::next_fire(now, hours) {
                slots.push((at, kind));
            }
        };
        if !self.cfg.checkin_disabled {
            push(&self.cfg.checkin_hours, TaskKind::Checkin);
        }
        if !self.cfg.keepalive_disabled {
            push(&self.cfg.keepalive_hours, TaskKind::Keepalive);
        }
        if !self.cfg.activity_disabled {
            push(&self.cfg.activity_hours, TaskKind::Activity);
        }
        if !self.cfg.night_owl_disabled {
            push(&self.cfg.night_owl_hours, TaskKind::NightOwl);
        }
        if !self.cfg.school_disabled {
            push(&self.cfg.school_hours, TaskKind::School);
        }
        if !self.cfg.trial_disabled {
            push(&self.cfg.trial_hours, TaskKind::Trial);
        }
        // Qoder 权益活动领取（所有者要求早晚两次）。
        //
        // ⚠ 复用同一套 slot 机制而不是另起一个循环：`next_wake` 是「到点该跑什么」
        //   的**唯一真相来源**，加一个独立 ticker 就会有两个调度器各算各的，
        //   到点时可能同时触发、也可能互相错开（前者并发、后者漏跑）。
        if !self.cfg.qoder_claim_disabled {
            push(&self.cfg.qoder_claim_hours, TaskKind::QoderClaim);
        }
        // 成长地图是 WorkBuddy 成长任务的一部分，必须随活跃上报自动运行。
        // 之前只有手动入口、没有 slot，因此用户不点按钮就永远不会执行。
        if !self.cfg.activity_disabled {
            push(&self.cfg.activity_hours, TaskKind::GrowthMap);
        }

        let Some(earliest) = slots.iter().map(|(at, _)| *at).min() else {
            return (None, Vec::new());
        };
        let kinds = slots
            .into_iter()
            .filter(|(at, _)| *at == earliest)
            .map(|(_, k)| k)
            .collect();
        (Some(earliest), kinds)
    }

    /// 最近一个**已过去**的计划时点（对应 Go `lastScheduledAt`）。
    ///
    /// 与 [`Self::next_fire`] 不同，这里对越界小时**跳过**（Go 同）。
    pub fn last_scheduled_at(now: DateTime<Local>, hours: &[i32]) -> Option<DateTime<Local>> {
        let mut latest: Option<DateTime<Local>> = None;
        for &hour in hours {
            if !(0..=23).contains(&hour) {
                continue;
            }
            let Some(mut t) = local_at_hour(now, hour) else {
                continue;
            };
            if t > now {
                t -= chrono::Duration::hours(24);
            }
            if latest.is_none_or(|l| t > l) {
                latest = Some(t);
            }
        }
        latest
    }

    /// 该计划时点是否落在启动补跑窗口内（对应 Go `withinCatchUp`）。
    pub fn within_catch_up(now: DateTime<Local>, scheduled: Option<DateTime<Local>>) -> bool {
        match scheduled {
            None => false,
            Some(s) => {
                s <= now
                    && (now - s)
                        .to_std()
                        .map(|d| d <= STARTUP_CATCH_UP_GRACE)
                        .unwrap_or(false)
            }
        }
    }

    // ---- 主循环 ----

    /// 补跑启动前最近错过的任务时点（对应 Go `runStartupCatchUp`）。
    async fn run_startup_catch_up(&self, cancel: &Cancel, now: DateTime<Local>) {
        if !self.cfg.activity_disabled
            && Self::within_catch_up(now, Self::last_scheduled_at(now, &self.cfg.activity_hours))
        {
            self.run_care_task(TASK_NAME_ACTIVITY, self.run_activity(cancel, None))
                .await;
            self.run_care_task(TASK_NAME_GROWTH_MAP, self.run_growth_map(cancel, None))
                .await;
        }
        if !self.cfg.school_disabled
            && Self::within_catch_up(now, Self::last_scheduled_at(now, &self.cfg.school_hours))
        {
            self.run_care_task(TASK_NAME_SCHOOL, self.run_school(cancel, None))
                .await;
        }
        if !self.cfg.night_owl_disabled
            && Self::within_catch_up(now, Self::last_scheduled_at(now, &self.cfg.night_owl_hours))
        {
            self.run_care_task(TASK_NAME_NIGHT_OWL, self.run_night_owl(cancel, None))
                .await;
        }
        if !self.cfg.trial_disabled
            && Self::within_catch_up(now, Self::last_scheduled_at(now, &self.cfg.trial_hours))
        {
            self.run_care_task(TASK_NAME_TRIAL, self.run_trial(cancel, None))
                .await;
        }
        if !self.cfg.qoder_claim_disabled
            && Self::within_catch_up(
                now,
                Self::last_scheduled_at(now, &self.cfg.qoder_claim_hours),
            )
        {
            self.run_care_task(TASK_NAME_PRODUCT_TASKS, self.run_product_tasks(cancel))
                .await;
        }
    }

    /// 主循环，阻塞直到取消（对应 Go `Run`）。
    pub async fn run(&self, cancel: &Cancel) {
        self.run_startup_catch_up(cancel, Local::now()).await;
        loop {
            let (next, kinds) = self.next_wake(Local::now());
            let Some(next) = next else {
                // 所有任务全部禁用：不空转，只等退出信号。
                cancel.cancelled().await;
                return;
            };
            let dur = (next - Local::now())
                .to_std()
                .unwrap_or(Duration::ZERO);
            if !cancel.sleep(dur).await {
                return;
            }
            // 到点任务在排程时确定（不依赖唤醒时刻的小时数），迟到唤醒也不会漏跑。
            for kind in kinds {
                match kind {
                    TaskKind::Checkin => self.run_checkin_now().await,
                    TaskKind::Keepalive => self.run_keepalive_now().await,
                    TaskKind::Activity => {
                        self.run_care_task(TASK_NAME_ACTIVITY, self.run_activity(cancel, None))
                            .await;
                    }
                    TaskKind::NightOwl => {
                        self.run_care_task(TASK_NAME_NIGHT_OWL, self.run_night_owl(cancel, None))
                            .await;
                    }
                    TaskKind::School => {
                        self.run_care_task(TASK_NAME_SCHOOL, self.run_school(cancel, None))
                            .await;
                    }
                    TaskKind::Trial => {
                        self.run_care_task(TASK_NAME_TRIAL, self.run_trial(cancel, None))
                            .await;
                    }
                    TaskKind::QoderClaim => {
                        self.run_care_task(TASK_NAME_PRODUCT_TASKS, self.run_product_tasks(cancel))
                            .await;
                    }
                    TaskKind::GrowthMap => {
                        // 成长地图与活跃上报同一时点，但使用独立锁，
                        // 避免任一任务被手动触发时静默吞掉另一项。
                        self.run_care_task(TASK_NAME_GROWTH_MAP, self.run_growth_map(cancel, None))
                            .await;
                    }
                }
            }
        }
    }

    /// 周期性刷新所有账号的积分余额与到期日，阻塞直到取消
    ///（对应 Go `RunCreditRefreshLoop`）。
    pub async fn run_credit_refresh_loop(&self, cancel: &Cancel, interval: Duration) {
        let interval = if interval.is_zero() {
            DEFAULT_CREDIT_REFRESH_INTERVAL
        } else {
            interval
        };
        // 启动先跑一轮：否则重启后要等一个周期才拿到到期日，
        // 这段时间内分层选号只能依赖 state.json 里持久化的旧值。
        self.refresh_credits_with_gap(cancel).await;
        // 计费周期刷新搭同一条循环的便车（同属「只读状态巡检」）。
        self.run_billing_cycle_refresh(cancel).await;

        loop {
            if !cancel.sleep(interval).await {
                return;
            }
            self.refresh_credits_with_gap(cancel).await;
            self.run_billing_cycle_refresh(cancel).await;
        }
    }

    /// 跑一轮计费周期刷新；未接线时直接跳过（对应 Go `runBillingCycleRefresh`）。
    async fn run_billing_cycle_refresh(&self, cancel: &Cancel) {
        if cancel.is_cancelled() {
            return;
        }
        let Some(runner) = self.billing_cycle_runner() else {
            return;
        };
        runner().await;
    }

    /// 跑一轮积分巡检，账号之间留出间隔；取消时提前退出
    ///（对应 Go `refreshCreditsWithGap`）。
    async fn refresh_credits_with_gap(&self, cancel: &Cancel) {
        let mut first = true;
        for st in self.pool_list() {
            if cancel.is_cancelled() {
                return;
            }
            let Some(a) = self.auth_by_uid(&st.uid) else {
                continue;
            };
            if a.refresh_token.is_empty() {
                continue;
            }
            // ⚠ 产品闸门：下面这条出站请求打的是 **WorkBuddy** 端点。不判产品时，
            //    Qoder/ZCode 账号会被带着自己的凭证打过去 ⇒ 401，并在账号记录里
            //    留下不属于它的错误。
            if a.product_of() != PRODUCT_WORKBUDDY {
                continue;
            }
            if !first && !cancel.sleep(CREDIT_REFRESH_GAP).await {
                return;
            }
            first = false;

            let info = match self.cfg.upstream.user_resource_detail(&a).await {
                Ok(info) => info,
                Err(e) => {
                    crate::logline!("credit-refresh {}: {e}", st.uid);
                    continue;
                }
            };
            if let Some(mut pool) = self.lock_pool() {
                pool.set_credits_and_expiry(&st.uid, info.remain, info.soonest_expire_at);
                if info.remain > 0 {
                    // 余额恢复的账号顺带复活，避免硬冷却的号空等到下一个签到时点。
                    pool.reenable_if_credits(&st.uid, info.remain);
                }
            }
        }
    }

    // ---- 三个「立即执行」入口 ----

    /// 立即对所有账号执行签到 + 余额刷新 + 解冻，末尾顺带跑一趟猫猫旅行。
    ///
    /// 冷却中的账号也参与（签到就是为了解冻它们）；禁用的跳过。
    /// 区域范围（`checkin_scope`，默认仅国服）之外的账号跳过签到与旅行。
    ///
    /// 旅行搭签到便车而非独立排程：每日上限按「派出」计 1 次/天且在派出时锁定
    /// 奖励，晚领不丢分，故分钟粒度巡检无增益，与签到时点（09/21 点）合并执行即可。
    /// 注意顺序：先签到解冻，旅行才能覆盖到本轮刚恢复的账号。
    pub async fn run_checkin_now(&self) {
        for st in self.pool_list() {
            let Some(a) = self.auth_by_uid(&st.uid) else {
                continue;
            };
            if a.refresh_token.is_empty() {
                continue;
            }
            // ⚠ 产品闸门：签到是 WorkBuddy 专属。此前只判区域，于是 Qoder/ZCode
            //    账号也被拿去签到 —— 带错凭证打到 WorkBuddy 端点必 401，
            //    且往账号记录里写脏数据。
            if a.product_of() != PRODUCT_WORKBUDDY {
                continue;
            }
            if !self.checkin_scope_allows(&a) {
                continue;
            }
            if let Err(e) = self.cfg.upstream.daily_checkin(&a).await {
                crate::logline!("checkin {}: {e}", st.uid);
                // 已签到等业务错误也继续走余额查询
            }
            let info = match self.cfg.upstream.user_resource_detail(&a).await {
                Ok(info) => info,
                Err(e) => {
                    crate::logline!("user-resource {}: {e}", st.uid);
                    continue;
                }
            };
            // 一次请求同时取回余额与「最近到期」：到期日驱动账号池的分层选号，
            // 顺带回写凭证文件，让宿主也能看到最新到期信息。
            if let Some(mut pool) = self.lock_pool() {
                pool.set_credits_and_expiry(&st.uid, info.remain, info.soonest_expire_at);
                // 签到就是为了解冻 ⇒ 这里**无条件**复活（与
                // `RunCreditRefreshNow` 的「仅余额 > 0 才复活」刻意不同）。
                pool.reenable_if_credits(&st.uid, info.remain);
            }
        }
        // 签到收尾：顺带推进一趟旅行状态机（领养 / 派出 / 领奖）。
        self.run_travel_now().await;
    }

    /// 立即刷新所有账号的积分余额与到期日（不签到、不解冻）
    ///（对应 Go `RunCreditRefreshNow`）。
    ///
    /// 与签到的分工：签到是「每天两次」的重操作（含旅行），而到期日会随消费实时
    /// 变化，需要更高频地刷新才能让分层选号跟上。
    pub async fn run_credit_refresh_now(&self) {
        for st in self.pool_list() {
            let Some(a) = self.auth_by_uid(&st.uid) else {
                continue;
            };
            if a.refresh_token.is_empty() {
                continue;
            }
            // ⚠ 产品闸门：同 `refresh_credits_with_gap`。
            if a.product_of() != PRODUCT_WORKBUDDY {
                continue;
            }
            let info = match self.cfg.upstream.user_resource_detail(&a).await {
                Ok(info) => info,
                Err(e) => {
                    crate::logline!("credit-refresh {}: {e}", st.uid);
                    continue;
                }
            };
            if let Some(mut pool) = self.lock_pool() {
                pool.set_credits_and_expiry(&st.uid, info.remain, info.soonest_expire_at);
                // 余额耗尽时不在此解冻（那是签到的职责）；但余额恢复的账号顺带
                // 复活，避免硬冷却的号要等到下一个签到时点才回到池中。
                if info.remain > 0 {
                    pool.reenable_if_credits(&st.uid, info.remain);
                }
            }
        }
    }

    /// 立即对所有账号刷新 token（对应 Go `RunKeepaliveNow`）。
    ///
    /// session 死亡走**连续计数**语义（见 [`Pool::note_session_dead`]）：一次刷新
    /// 失败不再立即杀号 —— 12153 会被网络抖动 / 上游闪断临时触发，一次即禁用会
    /// 误杀健康账号。连续 [`Pool::session_dead_threshold`] 次才禁用；刷新成功则
    /// 清零计数，因此被误判的账号有复活路径。
    pub async fn run_keepalive_now(&self) {
        for st in self.pool_list() {
            let Some(mut a) = self.auth_by_uid(&st.uid) else {
                continue;
            };
            if a.refresh_token.is_empty() {
                continue;
            }
            // ⚠ 产品闸门：同 `refresh_credits_with_gap`。
            if a.product_of() != PRODUCT_WORKBUDDY {
                continue;
            }
            if let Err(e) = self.cfg.upstream.refresh_token(&mut a).await {
                if upstream_err_kind(&e) == ErrKind::SessionDead {
                    let (hit, fails, threshold) = match self.lock_pool() {
                        Some(mut pool) => (
                            pool.note_session_dead(&st.uid),
                            pool.session_dead_fails(&st.uid),
                            pool.session_dead_threshold(),
                        ),
                        None => (false, 0, 0),
                    };
                    if hit {
                        crate::logline!(
                            "keepalive {}: session dead x{threshold}, disabled (re-login required)",
                            uid8(&st.uid)
                        );
                    } else {
                        // 未达阈值：保留账号，下轮再判（误判防护）
                        crate::logline!(
                            "keepalive {}: session dead {fails}/{threshold} (not disabling yet): {e}",
                            uid8(&st.uid)
                        );
                    }
                } else {
                    crate::logline!("keepalive {}: {e}", uid8(&st.uid));
                }
                continue;
            }
            // 刷新成功 = 账号确实未死：清零连续 12153 计数（复活路径）。
            if let Some(mut pool) = self.lock_pool() {
                pool.clear_session_dead(&st.uid);
            }
            if let Err(e) = a.save_atomic() {
                crate::logline!("keepalive {} save: {e}", uid8(&st.uid));
            }
        }
    }

    // ---- 产品日常任务 ----

    /// 手动触发入口（对应 Go `runProductTasksFor`）。
    async fn run_product_tasks_for(&self, name: &str) -> (TaskRunResult, Option<SchedulerError>) {
        let Some(runner) = self.product_tasks_runner() else {
            return (
                TaskRunResult::skipped(name, "unsupported", "该构建未接入产品任务执行体"),
                None,
            );
        };
        if !self.claim_task(name) {
            return (
                TaskRunResult::skipped(name, "already_running", "该任务正在执行中，请稍后再试"),
                Some(SchedulerError::TaskRunning),
            );
        }
        let _guard = TaskBusyGuard {
            sched: self,
            name: name.to_string(),
        };

        let (detail, err) = runner().await;
        match err {
            Some(err) => {
                // 失败也要留痕（所有者要求「任务一定要留痕」）。
                //
                // ⚠ 用 `task_all_daily`（整轮汇总、不带 accountId）而不是逐账号：
                //   执行体内部按产品/账号遍历，它才知道每条结果属于谁；网关在这里
                //   只知道「整轮的结果」。
                self.rec_task_all_daily("产品日常任务", crate::records::RESULT_FAILED, &err);
                (
                    TaskRunResult {
                        task: name.to_string(),
                        ran: false,
                        skip: String::new(),
                        message: err.clone(),
                    },
                    Some(SchedulerError::UnknownTask(err)),
                )
            }
            None => {
                self.rec_task_all_daily("产品日常任务", crate::records::RESULT_SUCCESS, &detail);
                (TaskRunResult::ran(name, &detail), None)
            }
        }
    }

    /// 排程路径：到点自动执行一轮产品日常任务（对应 Go `runProductTasks`）。
    async fn run_product_tasks(&self, _cancel: &Cancel) {
        let Some(runner) = self.product_tasks_runner() else {
            return;
        };
        let (detail, err) = runner().await;
        match err {
            Some(err) => {
                // ⚠ **不重试**：失败就等明天那个时点。这是与参考实现最重要的差异，
                //   也是本功能不扩大风控面的关键 —— 重试风暴正是把账号打进风控的
                //   典型特征（我们在 ZCode 上亲身经历过）。
                crate::logline!("product_tasks: {err}");
                self.rec_task_all_daily("产品日常任务", crate::records::RESULT_FAILED, &err);
            }
            None => {
                if !detail.is_empty() {
                    crate::logline!("product_tasks: {detail}");
                }
                self.rec_task_all_daily("产品日常任务", crate::records::RESULT_SUCCESS, &detail);
            }
        }
    }

    // ---- 手动触发入口 ----

    /// 只做「立刻跑一轮」，不检查 enabled 开关（对应 Go `RunTaskByName`）。
    ///
    /// 开关管的是**后台是否自动排程**，用户主动点击就该执行。未知任务名返回错误
    /// 而非静默成功：宿主拼错名字时必须能看见。
    pub async fn run_task_by_name(&self, name: &str) -> (TaskRunResult, Option<SchedulerError>) {
        self.run_task_for(name, "").await
    }

    /// 按账号跑一轮（对应 Go `RunTaskFor`）。
    ///
    /// # 为什么需要按账号跑（所有者明确要求）
    ///
    /// 「养号任务」原先只能作用于全部账号 —— 用户在某个账号的菜单里点「活跃上报」，
    /// 跑的却是整池。那与菜单位置所暗示的语义相反。现在账号菜单走本入口
    ///（单账号），右上角「一键操作」走 [`Self::run_task_by_name`]（全账号）。
    /// 区域不符时**提前回报原因**而不是静默跑空：单账号触发时用户盯着结果，
    /// 什么都不发生比一句说明糟糕得多。
    pub async fn run_task_for(
        &self,
        name: &str,
        account_uid: &str,
    ) -> (TaskRunResult, Option<SchedulerError>) {
        // `product_tasks` 账号作用域**不适用**：它按产品遍历且凭证在宿主侧，
        // 网关不知道哪些 uid 属于哪个产品，故不做 `account_scope_skip` 预检。
        if name == TASK_NAME_PRODUCT_TASKS {
            return self.run_product_tasks_for(name).await;
        }
        if !TASK_NAMES.contains(&name) {
            return (
                TaskRunResult {
                    task: name.to_string(),
                    ran: false,
                    skip: String::new(),
                    message: String::new(),
                },
                Some(SchedulerError::UnknownTask(name.to_string())),
            );
        }

        let scope: Option<&str> = if account_uid.is_empty() {
            None
        } else {
            Some(account_uid)
        };

        // 单账号模式先做可执行性检查（拿不到账号 / 区域不符都是**确定的**结论，
        // 不必占任务锁）。
        if !account_uid.is_empty() {
            let Some(a) = self.auth_by_uid(account_uid) else {
                return (
                    TaskRunResult::skipped(
                        name,
                        "account_not_found",
                        "该账号不在网关账号池中（可能已禁用、需重新登录或尚未同步）",
                    ),
                    None,
                );
            };
            if let Err(msg) = self.account_scope_skip(name, &a) {
                return (TaskRunResult::skipped(name, "region_mismatch", &msg), None);
            }
        }

        if !self.claim_task(name) {
            return (
                TaskRunResult::skipped(name, "already_running", "该任务正在执行中，请稍后再试"),
                Some(SchedulerError::TaskRunning),
            );
        }
        let _guard = TaskBusyGuard {
            sched: self,
            name: name.to_string(),
        };

        // ★★ 夜猫子窗口预检：这里**必须补一条记录**，因为下面直接返回，根本不进
        //    `run_night_owl`，写在那里的留痕是**死代码**（Go 第一版就写错了位置，
        //    测试直接揪出「手动触发夜猫子…实际 0 条」）。
        if name == TASK_NAME_NIGHT_OWL && !nightowl::within_night_window() {
            const NIGHT_OWL_SKIP_MSG: &str =
                "当前不在夜猫子时段（23:00–08:00 北京时间），上游不计入本次上报";
            self.rec_task_all_daily(name, crate::records::RESULT_INFO, NIGHT_OWL_SKIP_MSG);
            if !account_uid.is_empty() {
                self.rec_task(account_uid, "夜猫子任务", crate::records::RESULT_INFO, NIGHT_OWL_SKIP_MSG);
            }
            return (
                TaskRunResult::skipped(name, "outside_window", NIGHT_OWL_SKIP_MSG),
                None,
            );
        }

        match name {
            TASK_NAME_ACTIVITY => self.run_activity_inner(scope).await,
            TASK_NAME_NIGHT_OWL => self.run_night_owl_inner(scope).await,
            TASK_NAME_SCHOOL => self.run_school_inner(scope).await,
            TASK_NAME_TRIAL => self.run_trial_inner(scope).await,
            TASK_NAME_GROWTH_MAP => self.run_growth_map_inner(scope).await,
            _ => unreachable!("task name validated by TASK_NAMES"),
        }

        let message = if account_uid.is_empty() {
            "已触发一轮"
        } else {
            "已触发该账号的一轮"
        };
        (TaskRunResult::ran(name, message), None)
    }

    /// 手动触发路径的薄包装（对应 Go 里手动入口传 `context.Background()`）。
    ///
    /// 排程路径传的是**真实的取消信号** —— 进程退出时那些长轮次（活跃上报在大号池
    /// 下可长达数分钟）要能立刻停下，而不是被 `with_graceful_shutdown` 等着跑完。
    /// 手动触发则相反：这是用户主动点的一次动作，没有「该中断」的上游信号，
    /// 半途放弃只会留下做了一半的状态。
    ///
    /// `Cancel::new()` 的接收端从未收到 `cancel()`，故各任务里的 `sleep` 都会正常
    /// 走完 —— 语义与 `context.Background()` 一致，不需要在每个任务体里再判
    /// 「有没有取消信号」。
    async fn run_activity_inner(&self, scope: Option<&str>) {
        let cancel = Cancel::new();
        self.run_activity(&cancel, scope).await;
    }

    async fn run_night_owl_inner(&self, scope: Option<&str>) {
        let cancel = Cancel::new();
        self.run_night_owl(&cancel, scope).await;
    }

    async fn run_school_inner(&self, scope: Option<&str>) {
        let cancel = Cancel::new();
        self.run_school(&cancel, scope).await;
    }

    async fn run_trial_inner(&self, scope: Option<&str>) {
        let cancel = Cancel::new();
        self.run_trial(&cancel, scope).await;
    }

    async fn run_growth_map_inner(&self, scope: Option<&str>) {
        let cancel = Cancel::new();
        self.run_growth_map(&cancel, scope).await;
    }

    // =======================================================================
    // 任务体
    // =======================================================================

    /// 活跃上报（对应 Go `activity.go` 的 `runActivity`）。
    ///
    /// # 为什么需要它
    ///
    /// 签到只恢复**余额**，连登天数与领养资格靠对话活跃度点亮。没有本任务时，
    /// 纯挂机账号的连登不会增长、也无法领养猫猫。
    ///
    /// 风控口径：每号每天上报若干条即可（默认 3 条），不做多时点高频上报。
    async fn run_activity(&self, cancel: &Cancel, scope: Option<&str>) {
        let mut first = true;
        for st in self.pool_list() {
            if !Self::in_account_scope(scope, &st.uid) {
                continue;
            }
            let Some(a) = self.auth_by_uid(&st.uid) else {
                continue;
            };
            if a.access_token.is_empty() {
                continue;
            }
            // 产品 + 区域闸门，统一走 `account_scope_skip`（**单一真相来源**）。
            // 此前只调 `checkin_scope_allows`（只判区域）⇒ Qoder/ZCode 被当成
            // WorkBuddy 跑，打 WorkBuddy 端点吃 401 还写下不属于它的记录。
            if self.account_scope_skip(TASK_NAME_ACTIVITY, &a).is_err() {
                continue;
            }
            if !first && !cancel.sleep(ACTIVITY_ACCOUNT_DELAY).await {
                return;
            }
            first = false;

            // 多条共用同一 conversationId，requestId 各自独立。
            let cid = format!("wb2api-{}", Local::now().timestamp_millis());
            let count = self.cfg.activity_report_count;
            let mut ok = 0;
            let mut last_err = String::new();
            for i in 1..=count {
                let rid = format!("{cid}-r{i}");
                if let Err(e) = self.cfg.upstream.report_chat_activity(&a, &cid, &rid).await {
                    crate::logline!(
                        "activity {}: report {i}/{count} failed: {e}",
                        uid8(&a.uid)
                    );
                    last_err = e.to_string();
                    break;
                }
                ok += 1;
                if i < count && !cancel.sleep(ACTIVITY_REPORT_GAP).await {
                    return;
                }
            }
            if ok < count {
                // 失败必须留痕：这是用户排查「连登为什么没涨」的唯一线索
                //（宿主把网关子进程的 stdout 丢进了 `Stdio::null`）。
                self.rec_task_daily(
                    &a.uid,
                    "活跃上报",
                    crate::records::RESULT_FAILED,
                    &format!("第 {}/{} 条上报失败: {}", ok + 1, count, last_err),
                );
                continue;
            }
            crate::logline!("activity {}: reported {ok}/{count} ok", uid8(&a.uid));
            // 成功用**非** Daily：确实有新变化，每次都要留痕。
            self.rec_task(
                &a.uid,
                "活跃上报",
                crate::records::RESULT_SUCCESS,
                &format!("已上报 {ok}/{count} 条（点亮连登天数）"),
            );

            let (streak, _suspect) = self.read_streak_state(&a).await;
            // 对话量刚补满 ⇒ 立即重试领养，豁免当日防抖。
            self.travel_adopt_force(&a).await;
            // 活跃地图闭环紧跟上报之后：它的每一步都以「刚被点亮的连登天数」
            // 为前提，分开排程会顺序倒挂（当天点亮的天数要等到明天才能兑换）。
            self.growth_map_round(&a, streak.as_ref(), false).await;
        }
    }

    /// 读取连登快照（对应 Go `activity.go` 的 `readStreakState`）。
    ///
    /// 返回 `(state, suspect)`；`suspect = true` 表示「上报成功但连登可疑」：
    /// 查询失败，或 `days == 0`。后者尤其重要 —— 上报返回 200 不等于真的计分
    ///（事件缺 `userId` 时服务端 200 但直接丢弃），实测标定 2026-09-15 的 19 个
    /// 真实账号：上报前多个账号 `days=0`，上报 1 条后同一批全部变 `days=1`。
    async fn read_streak_state(&self, a: &Auth) -> (Option<crate::upstream::growth::GrowthStreakState>, bool) {
        match self.cfg.upstream.growth_streak_state(a).await {
            Err(e) => {
                crate::logline!(
                    "WARN: activity {}: streak check failed (report ok): {e}",
                    uid8(&a.uid)
                );
                (None, true)
            }
            Ok(st) if st.days() == 0 => {
                crate::logline!(
                    "WARN: activity {}: report ok but streak.days=0 (silent drop?)",
                    uid8(&a.uid)
                );
                (Some(st), true)
            }
            Ok(st) => {
                crate::logline!("activity {}: streak days={}", uid8(&a.uid), st.days());
                (Some(st), false)
            }
        }
    }

    /// 夜猫子补报（对应 Go `nightowl.go` 的 `runNightOwl`）。
    ///
    /// 与活跃上报的关系：两者都发 `chat_request_send` 事件，形状完全相同，
    /// 只是**触发时机**与**条数**不同 —— 活跃上报每天 10 点发若干条点亮连登；
    /// 夜猫子只在窗口内补 1 条以点亮 `black_cat`。故复用同一上报方法。
    async fn run_night_owl(&self, cancel: &Cancel, scope: Option<&str>) {
        // 用 uid 而不是 bool 更直接 —— 手动触发**必然**有具体账号，
        // 而写账号记录正需要这个 uid。
        let manual_uid = Self::account_scope_uid(scope).to_string();
        const NIGHT_OWL_SKIP_MSG: &str =
            "当前不在夜猫时段（23:00–08:00 北京时间），上游不计入本次上报";

        if !nightowl::within_night_window() {
            crate::logline!("nightowl: skipped (outside 23:00-08:00 CST window)");
            // 整轮汇总**不带 accountId**，用户在账号卡片按账号筛选时看不到它，
            // 故手动触发要**再**补一条账号级。
            self.rec_task_all_daily(TASK_NAME_NIGHT_OWL, crate::records::RESULT_INFO, NIGHT_OWL_SKIP_MSG);
            if !manual_uid.is_empty() {
                self.rec_task(&manual_uid, "夜猫子任务", crate::records::RESULT_INFO, NIGHT_OWL_SKIP_MSG);
            }
            return;
        }

        let mut first = true;
        for st in self.pool_list() {
            if !Self::in_account_scope(scope, &st.uid) {
                continue;
            }
            let Some(a) = self.auth_by_uid(&st.uid) else {
                continue;
            };
            if a.access_token.is_empty() {
                continue;
            }
            if self.account_scope_skip(TASK_NAME_NIGHT_OWL, &a).is_err() {
                continue;
            }
            // 复用活跃上报的账号间隔。
            if !first && !cancel.sleep(ACTIVITY_ACCOUNT_DELAY).await {
                return;
            }
            first = false;

            let cid = format!("wb2api-night-{}", Local::now().timestamp_millis());
            if let Err(e) = self
                .cfg
                .upstream
                .report_chat_activity(&a, &cid, &format!("{cid}-r1"))
                .await
            {
                crate::logline!("nightowl {}: report failed: {e}", uid8(&a.uid));
                self.rec_task_daily(&a.uid, "夜猫子任务", crate::records::RESULT_FAILED, &e.to_string());
                continue;
            }
            crate::logline!("nightowl {}: black_cat reported ok", uid8(&a.uid));
            self.rec_task(
                &a.uid,
                "夜猫子任务",
                crate::records::RESULT_SUCCESS,
                "已在夜猫时段补报一次（点亮 black_cat 任务）",
            );
        }
    }

    /// 开学季活动任务（对应 Go `school.go` 的 `runSchool`）。
    ///
    /// # 设计边界（重要）
    ///
    /// 本任务**只领取已经达标的任务**，不伪造完成动作。活动任务分两类：
    /// 可被正常使用行为点亮的（`chat_3_times` / `desktop_chat_1_time`，由活跃上报
    /// 自然覆盖）、需要真实动作的（`task_student_verify` 学生认证 / `share_invite`
    /// 邀请 / `expert_use` 专家功能）—— 后者**不伪造**：伪造认证 / 邀请属于刷量，
    /// 既违反活动规则也可能导致账号被封。
    ///
    /// 活动是**限时**的：服务端下发 `in_period`；下线后静默跳过（不报错、不重试），
    /// 活动结束后代码自然失效，无需人工清理。
    async fn run_school(&self, cancel: &Cancel, scope: Option<&str>) {
        const SCHOOL_CLAIM_GAP: Duration = Duration::from_millis(400);

        // `manual_uid` 非空 = 用户在某个账号的菜单里手动触发；空串 = 自动排程。
        //
        // # 为什么这个区分是必要的（所有者 2026-09-20 反馈）
        //
        // 原话：「开学季任务也是，我手动执行了但是查看记录却没有」。
        // 根因：成功的路径里，「活动不在期」与「在期但无已达标任务」**都不写
        // 账号级记录**。原作者的顾虑是「逐账号写会把记录刷成噪音」——
        // 那条顾虑**对自动排程成立**（每天一次、没人看），但对**手动触发**不成立：
        // 用户主动点了按钮，就必须有反馈。判据不是主观偏好，而是**有没有人在看**。
        let manual_uid = Self::account_scope_uid(scope).to_string();
        // `any_in_period` 与 `any_answered` 必须分开记，否则两种情况会被混为一谈：
        //   - 「问了上游，回答是不在期」→ 结论成立，值得写一条汇总记录；
        //   - 「一个账号都没问到」（全被禁用/区域不符/拉清单全失败）→ 我们对活动
        //     状态一无所知，此时写「活动不在期」是**编造结论**，会把用户引偏。
        let mut any_in_period = false;
        let mut any_answered = false;
        let mut first = true;

        for st in self.pool_list() {
            if !Self::in_account_scope(scope, &st.uid) {
                continue;
            }
            let Some(a) = self.auth_by_uid(&st.uid) else {
                continue;
            };
            if a.access_token.is_empty() {
                continue;
            }
            // 实测该活动只有国服有（国际版返回 404 Route Not Found），
            // 与签到共用同一开关（`checkin_scope`）。
            if self.account_scope_skip(TASK_NAME_SCHOOL, &a).is_err() {
                continue;
            }
            if !first && !cancel.sleep(SCHOOL_CLAIM_GAP).await {
                return;
            }
            first = false;

            let (in_period, tasks) = match self.cfg.upstream.school_tasks(&a).await {
                Ok(v) => v,
                Err(e) => {
                    crate::logline!("school {}: fetch tasks failed: {e}", uid8(&a.uid));
                    // 失败按天去重：活动任务每天只跑一次，失败留一条即可。
                    self.rec_task_daily(
                        &a.uid,
                        "开学季活动",
                        crate::records::RESULT_FAILED,
                        &format!("拉取活动任务清单失败: {e}"),
                    );
                    continue;
                }
            };
            any_answered = true;
            if !in_period {
                crate::logline!("school {}: activity not in period, skip", uid8(&a.uid));
                // ⚠ 只对手动触发补写。自动排程每天跑一次，为「活动不在期」逐账号
                //    写记录会把记录刷成噪音 —— 原作者那条顾虑**对自动路径成立**。
                if !manual_uid.is_empty() {
                    self.rec_task(
                        &a.uid,
                        "开学季活动",
                        crate::records::RESULT_INFO,
                        "活动不在期（已下线或尚未开始），本次未做任何领取",
                    );
                }
                continue;
            }
            any_in_period = true;

            let (claimed, claim_errs) = self.claim_finished_tasks(&a, &tasks).await;
            if claimed > 0 {
                crate::logline!("school {}: claimed {claimed} task(s)", uid8(&a.uid));
                // 只在**真的领到奖励**时写成功记录 —— 那才是「有新变化」。
                self.rec_task(
                    &a.uid,
                    "开学季活动",
                    crate::records::RESULT_SUCCESS,
                    &format!("领取 {claimed} 个已达标任务奖励"),
                );
            } else if !manual_uid.is_empty() && claim_errs.is_empty() {
                // ⚠ 排除了 `claim_errs` 非空的情况 —— 那些失败会在下面逐条写
                //    failed 记录，再加一条 info 只会让同一件事出现两次。
                self.rec_task(
                    &a.uid,
                    "开学季活动",
                    crate::records::RESULT_ALREADY,
                    "活动在期，但当前没有可领取的已达标任务（无需操作）",
                );
            }
            // 领取失败必须留痕（按天去重）：这是用户排查「奖励为什么没到账」的线索。
            for e in &claim_errs {
                self.rec_task_daily(&a.uid, "开学季活动", crate::records::RESULT_FAILED, e);
            }
        }

        // 汇总只在「确实问过上游、且都回答不在期」时写：没问到任何账号就断言活动
        // 下线，会让用户拿着错误结论去排查。
        if any_answered && !any_in_period {
            crate::logline!("school: activity not in period for any account (probably ended)");
            self.rec_task_all_daily(
                "开学季活动",
                crate::records::RESULT_INFO,
                "活动不在期（已下线或尚未开始），本次未做任何领取",
            );
        }
    }

    /// 领取已达标的任务奖励，返回成功数量与逐条失败原因
    ///（对应 Go `school.go` 的 `claimFinishedTasks`）。
    ///
    /// 只处理 `status == "finished"`（已达标待领取）的任务；`pending`（未达标）与
    /// `claimed`（已领取）都跳过 —— 前者没资格、后者会报重复领取。
    ///
    /// 把错误回传给调用方而不是就地记录：本函数只认「哪些失败了」，而调用方
    ///（[`Self::run_school`]）才知道该按什么粒度写账号记录（按天去重 / 逐条）。
    async fn claim_finished_tasks(
        &self,
        a: &Auth,
        tasks: &[crate::upstream::growth::SchoolTask],
    ) -> (i32, Vec<String>) {
        /// 任务已达标、待领取的状态值（实测）。
        const SCHOOL_STATUS_FINISHED: &str = "finished";

        let mut n = 0;
        let mut errs = Vec::new();
        for t in tasks {
            if t.status != SCHOOL_STATUS_FINISHED {
                continue;
            }
            if let Err(e) = self.cfg.upstream.school_claim(a, &t.code).await {
                crate::logline!("school {}: claim {} failed: {e}", uid8(&a.uid), t.code);
                // 带上任务 code：活动有多个任务，不写清楚用户不知道是哪个没领到。
                errs.push(format!("领取 {} 失败: {e}", t.code));
                continue;
            }
            n += 1;
            crate::logline!(
                "school {}: claim {} ok (+{} credits)",
                uid8(&a.uid),
                t.code,
                t.reward_credit
            );
        }
        (n, errs)
    }

    /// 国际版 trial 加油包领取（对应 Go `trial.go` 的 `runTrial`）。
    ///
    /// # 为什么需要它
    ///
    /// 国际版**没有签到、没有任务中心**（那些都是国服独有的活动），trial 加油包
    /// 是国际版唯一天然的积分增益动作。对从未被发放额度的国际版账号（实测存在
    /// `total_count=0 / packages=[]` 的账号）尤其重要。
    ///
    /// # 为什么可以每天重试
    ///
    /// 上游用业务码 14051 表达「已领取过」，客户端把它判定为**幂等成功**而非错误。
    /// 因此无需本地记账、无需「只跑一次」的特殊逻辑 —— 每天试一次，已领过的账号
    /// 静默通过，新账号自动领到。
    async fn run_trial(&self, cancel: &Cancel, scope: Option<&str>) {
        // 空池或测试夹具下直接返回（Go 同）。
        if self.cfg.pool.is_none() {
            return;
        }
        let mut first = true;
        let (mut newly, mut already) = (0, 0);
        for st in self.pool_list() {
            if !Self::in_account_scope(scope, &st.uid) {
                continue;
            }
            let Some(a) = self.auth_by_uid(&st.uid) else {
                continue;
            };
            if a.access_token.is_empty() {
                continue;
            }
            // ★ 区域过滤与其它任务**相反**：这里只要国际版；**不复用**
            //   `checkin_scope_allows` —— 那个是「默认只跑国服」，语义正好相反。
            //   注意：这里**没有** `account_scope_skip` 调用（与 Go 逐字一致）。
            if !a.is_intl() {
                continue;
            }
            if !first && !cancel.sleep(TRIAL_GAP).await {
                return;
            }
            first = false;

            match self.cfg.upstream.claim_trial(&a).await {
                Err(e) => {
                    crate::logline!("trial {}: {e}", uid8(&a.uid));
                    self.rec_task_daily(&a.uid, "trial 加油包", crate::records::RESULT_FAILED, &e.to_string());
                }
                Ok(true) => {
                    newly += 1;
                    crate::logline!("trial {}: claimed new trial pack", uid8(&a.uid));
                    self.rec_task(
                        &a.uid,
                        "trial 加油包",
                        crate::records::RESULT_SUCCESS,
                        "领取到新的 trial 加油包",
                    );
                }
                Ok(false) => {
                    already += 1;
                    self.rec_task_daily(
                        &a.uid,
                        "trial 加油包",
                        crate::records::RESULT_ALREADY,
                        "本周期已领取过",
                    );
                }
            }
        }
        // 只在有新领取时打汇总，避免每天刷一行「全部已领过」的噪音日志。
        if newly > 0 {
            crate::logline!("trial: claimed {newly} new pack(s), {already} already claimed");
        }
    }

    /// 猫猫旅行巡检：对池内每个可用账号单趟推进一次状态机
    ///（对应 Go `travel.go` 的 `RunTravelNow`）。
    ///
    /// 无猫 → 同意协议 + 领养；有猫 → 按 `travel/status` 分派 派出 / 领奖 / 跳过。
    ///
    /// 禁用账号跳过；区域范围（`checkin_scope`，默认仅国服）之外的账号跳过
    ///（国际版 growth 接口暂无真实数据）；401/查询失败只跳过该账号本轮
    ///（不强刷 token，交 22:00 keepalive）。
    pub async fn run_travel_now(&self) {
        let mut first = true;
        for st in self.pool_list() {
            let Some(a) = self.auth_by_uid(&st.uid) else {
                continue;
            };
            if a.refresh_token.is_empty() {
                continue;
            }
            // ⚠ 产品闸门：猫猫旅行是 WorkBuddy 专属 —— Qoder/ZCode 账号没有猫猫，
            //    跑它只会拿到 401 并写脏记录。
            if a.product_of() != PRODUCT_WORKBUDDY {
                continue;
            }
            if !self.checkin_scope_allows(&a) {
                continue;
            }
            if !first {
                tokio::time::sleep(TRAVEL_ACCOUNT_DELAY).await;
            }
            first = false;
            self.travel_one(&a).await;
        }
    }

    /// 单账号单趟状态机：查有无猫 + 查状态 + 最多一个动作，不轮询不等待。
    async fn travel_one(&self, a: &Auth) {
        let buddy = match self.cfg.upstream.buddy_info(a).await {
            Ok(b) => b,
            Err(e) => {
                crate::logline!("travel {}: buddy-info: {e}", a.uid);
                return;
            }
        };
        if buddy.is_none() {
            self.travel_adopt(a).await;
            return;
        }
        let ts = match self.cfg.upstream.travel_status(a).await {
            Ok(ts) => ts,
            Err(e) => {
                crate::logline!("travel {}: status: {e}", a.uid);
                return;
            }
        };
        match ts.state.as_str() {
            TRAVEL_STATE_ARRIVED => self.travel_claim(a, &ts).await,
            TRAVEL_STATE_IDLE => self.travel_depart(a, &ts).await,
            TRAVEL_STATE_TRAVELING => {
                crate::logline!("travel {}: skip (traveling record={})", a.uid, ts.record_id)
            }
            other => crate::logline!("travel {}: skip (unknown state {other:?})", a.uid),
        }
    }

    /// 空闲且未达当日上限时派出（每日 1 次，自然日 00:00 CST 重置）。
    async fn travel_depart(&self, a: &Auth, ts: &crate::upstream::growth::TravelState) {
        if ts.daily_limit_reached {
            crate::logline!("travel {}: skip (daily limit reached)", a.uid);
            return;
        }
        if let Err(e) = self.cfg.upstream.travel_depart(a, TRAVEL_LOCATION_ID).await {
            crate::logline!("travel {}: depart: {e}", a.uid);
            return;
        }
        crate::logline!("travel {}: depart ok location={TRAVEL_LOCATION_ID}", a.uid);
    }

    /// 到站领奖（必须带 `record_id`）。
    async fn travel_claim(&self, a: &Auth, ts: &crate::upstream::growth::TravelState) {
        if ts.record_id == 0 {
            crate::logline!("travel {}: claim skipped (arrived but no record_id)", a.uid);
            return;
        }
        match self.cfg.upstream.travel_claim(a, ts.record_id).await {
            Ok(reward) => crate::logline!(
                "travel {}: claim ok record={} reward={reward}",
                a.uid, ts.record_id
            ),
            Err(e) => crate::logline!("travel {}: claim record={}: {e}", a.uid, ts.record_id),
        }
    }

    /// 无猫时领养（对应 Go `travelAdopt`）。
    ///
    /// conversation 门槛未达标（HTTP 400 `first_buddy task not completed yet`）
    /// 属预期行为：记一次当日已试后静默跳过，不再重试。
    async fn travel_adopt(&self, a: &Auth) {
        if self.adopt_tried_today(&a.uid) {
            return;
        }
        self.adopt_buddy(a, false).await;
    }

    /// 豁免当日防抖的领养重试（对应 Go `travelAdoptForce`）。
    ///
    /// 用在活跃上报之后：刚补满对话量，此前「门槛未达」的判断已经失效，应当立即
    /// 重试一次，而不是等到明天。成功则顺带清掉当日标记。
    async fn travel_adopt_force(&self, a: &Auth) {
        self.adopt_buddy(a, true).await;
    }

    /// 领养主流程；`force = true` 时忽略当日防抖标记。
    async fn adopt_buddy(&self, a: &Auth, force: bool) {
        if !force && self.adopt_tried_today(&a.uid) {
            return;
        }
        if let Err(e) = self.cfg.upstream.buddy_agreement(a).await {
            crate::logline!("travel {}: agreement: {e}", uid8(&a.uid));
            return;
        }
        match self.cfg.upstream.buddy_first(a).await {
            Ok(()) => {
                // 成功即清标记，避免残留影响后续判断。
                self.clear_adopt_tried(&a.uid);
                crate::logline!("travel {}: adopt ok (+300 credits)", uid8(&a.uid));
            }
            Err(e) if crate::upstream::growth::is_buddy_task_incomplete(&e) => {
                self.mark_adopt_tried(&a.uid);
                crate::logline!(
                    "travel {}: adopt skipped (conversation threshold not reached, retry tomorrow)",
                    uid8(&a.uid)
                );
            }
            Err(e) => crate::logline!("travel {}: adopt: {e}", uid8(&a.uid)),
        }
    }

    /// 该账号当日是否已判定领养门槛未达。
    fn adopt_tried_today(&self, uid: &str) -> bool {
        let marks = self.daily.lock().unwrap_or_else(|e| e.into_inner());
        marks.adopt_tried.get(uid).map(String::as_str) == Some(travel_day(Local::now()).as_str())
    }

    fn mark_adopt_tried(&self, uid: &str) {
        let mut marks = self.daily.lock().unwrap_or_else(|e| e.into_inner());
        marks.adopt_tried.insert(uid.to_string(), travel_day(Local::now()));
    }

    fn clear_adopt_tried(&self, uid: &str) {
        let mut marks = self.daily.lock().unwrap_or_else(|e| e.into_inner());
        marks.adopt_tried.remove(uid);
    }

    /// 该账号当日是否已跑过一轮活跃地图闭环。
    fn growth_claimed_today(&self, uid: &str) -> bool {
        let marks = self.daily.lock().unwrap_or_else(|e| e.into_inner());
        marks.growth_claimed.get(uid).map(String::as_str) == Some(travel_day(Local::now()).as_str())
    }

    fn mark_growth_claimed(&self, uid: &str) {
        let mut marks = self.daily.lock().unwrap_or_else(|e| e.into_inner());
        marks.growth_claimed.insert(uid.to_string(), travel_day(Local::now()));
    }

    /// 活跃地图闭环（对应 Go `growthmap.go` 的 `runGrowthMap`）。
    ///
    /// # 为什么需要它
    ///
    /// 活跃上报只负责**点亮**连登天数，但天数点亮之后还有三步收益没人做：
    /// ① 断签会把连登打回原点（7/14/28 档全部重攒），而补签卡躺在账户里没人用；
    /// ② 达到 7/14/28 天的里程碑奖励是**兑换制**，不会自动到账；
    /// ③ 兑换送的抽奖次数也不会自动抽。三者都是「不做就白白过期」的收益。
    ///
    /// 挂在活跃上报成功之后执行（复用已有排程与区域过滤，不新增时点、不新增开关）。
    async fn run_growth_map(&self, cancel: &Cancel, scope: Option<&str>) {
        let mut first = true;
        for st in self.pool_list() {
            if !Self::in_account_scope(scope, &st.uid) {
                continue;
            }
            let Some(a) = self.auth_by_uid(&st.uid) else {
                continue;
            };
            if a.access_token.is_empty() {
                continue;
            }
            // 本闭环的地基是 growth 连登，只有被点亮过连登的账号才有兑换/补签可言。
            if self.account_scope_skip(TASK_NAME_GROWTH_MAP, &a).is_err() {
                continue;
            }
            // 比活跃上报的账号间隔大：本任务紧跟在活跃上报（每号 3 条 + streak
            // 回读）之后，若再以同样密度连打，同一账号会在短时间内产生一串请求，
            // 正是刻意要避开的画像。
            if !first && !cancel.sleep(GROWTH_ACCOUNT_DELAY).await {
                return;
            }
            first = false;
            self.growth_map_round(&a, None, true).await;
        }
    }

    /// 单账号一轮闭环（对应 Go `growthMapRound`）。
    ///
    /// `state` 为调用方**已经读到的**连登快照（活跃上报那条路径会顺带复用它的
    /// 自检请求）；传 `None` 表示由本函数自己去读。`force = true` 时忽略
    /// 「当日已跑」闸（手动触发路径）。
    ///
    /// 顺序是刻意的：先领礼包/补偿（与连登无关，先落袋），再补签（会改变 streak
    /// 天数），最后用**补签后**的天数兑换与抽奖 —— 反过来会让补签恢复的天数白等一天。
    async fn growth_map_round(
        &self,
        a: &Auth,
        state: Option<&crate::upstream::growth::GrowthStreakState>,
        force: bool,
    ) {
        if !force && self.growth_claimed_today(&a.uid) {
            return;
        }
        self.claim_growth_benefits(a).await;

        // 补签读的是热力格（判据）与卡余额（同一快照），补签成功后天数会变，
        // 因此必须重读一次再挑档，否则会拿补签前的旧天数判断达标。
        let mut fresh_state: Option<crate::upstream::growth::GrowthStreakState> = None;
        if state.is_none() {
            match self.cfg.upstream.growth_streak_state(a).await {
                Ok(s) => fresh_state = Some(s),
                Err(e) => {
                    log_growth_err("growth", &a.uid, "streak-state", &e);
                    return;
                }
            }
        }

        // ★ 借用必须止于 await：补签成功后要重新赋值 `fresh_state`，
        //   若这里先取引用再 await，借用检查器会拒绝后面的赋值（E0506）。
        //   故每次 await 都就地重新取一次引用，而不是持有一个长期的 `state`。
        if self.make_up_yesterday(a, fresh_state.as_ref().or(state)).await {
            if let Ok(fresh) = self.cfg.upstream.growth_streak_state(a).await {
                fresh_state = Some(fresh);
            }
        }
        let state = fresh_state.as_ref().or(state);

        self.redeem_growth_tier(a, state).await;
        self.draw_growth_lottery(a).await;
        self.open_blindboxes(a).await;
        // 只有完成有效 streak 读取与本轮动作后才标记当天已处理。
        // 旧代码一进函数就标记：网络失败/上游异常后当天永不重试，用户看到
        //「失败后不会再尝试」。本轮动作都是幂等的，失败后由下一个调度时点补跑
        // 不会造成重复奖励。
        self.mark_growth_claimed(&a.uid);
    }

    /// 开盲盒：能量足够就开，每轮最多 [`crate::upstream::growth::MAX_BLIND_BOX_OPENS`] 个
    ///（对应 Go `openBlindboxes`）。
    ///
    /// # 为什么放在最后
    ///
    /// 开盒**吃能量**（每次固定消耗），而抽奖与兑换也用同一份能量。放在最后 + 限次，
    /// 保证前面的必得收益先拿到，剩下的才拿去开盒。
    async fn open_blindboxes(&self, a: &Auth) {
        let q = match self.cfg.upstream.growth_blindbox_quota(a).await {
            Ok(q) => q,
            Err(e) => {
                log_growth_err("growth", &a.uid, "blindbox-quota", &e);
                return;
            }
        };
        if q.affordable <= 0 {
            // 能量不足是**常态**（每天都可能不够），不值得留痕。
            return;
        }
        let n = q.affordable.min(crate::upstream::growth::MAX_BLIND_BOX_OPENS);
        let items = match self.cfg.upstream.growth_blindbox_open(a, n).await {
            Ok(items) => items,
            Err(e) => {
                log_growth_err("growth", &a.uid, "blindbox-open", &e);
                return;
            }
        };
        if items.is_empty() {
            return;
        }
        // 只在**真的开出来**时留痕（与签到/礼包同一原则）。
        let names: Vec<String> = items
            .iter()
            .map(|it| {
                if it.rarity.is_empty() {
                    it.name.clone()
                } else {
                    format!("{}({})", it.name, it.rarity)
                }
            })
            .collect();
        let detail = format!("开出 {} 个：{}", items.len(), names.join("、"));
        crate::logline!("growth {}: blindbox ok ({} 个)", uid8(&a.uid), items.len());
        self.rec_task(&a.uid, GROWTH_TASK_TITLE, crate::records::RESULT_SUCCESS, &detail);
    }

    /// 新手礼包 + 活动补偿（对应 Go `claimGrowthBenefits`）。
    ///
    /// 两者都是「有则领」的幂等写：礼包每号一次，补偿活动有则领、无则业务错误。
    /// 只在**真的到账**时写记录与日志 —— 已领过是每天都会发生的常态，不值得留痕。
    ///
    /// 失败分两类处理（这是本任务「静默」原则的准确边界）：
    /// - 业务拒绝（已领过 / 活动未开启）：常态，完全静默，连日志都不打。
    /// - 其他（401 登录态失效 / 404 端点没了 / 429 限流 / 5xx / 传输层）：真出问题了，
    ///   必须记 —— 否则「token 死了」这类故障会被永久静默。
    async fn claim_growth_benefits(&self, a: &Auth) {
        match self.cfg.upstream.claim_gift(a).await {
            Err(e) => log_growth_err("growth", &a.uid, "claim-gift", &e),
            Ok(credit) if credit > 0 => {
                crate::logline!("growth {}: gift ok (+{credit} credit)", uid8(&a.uid));
                self.rec_task(
                    &a.uid,
                    GROWTH_TASK_TITLE,
                    crate::records::RESULT_SUCCESS,
                    &format!("领取新手礼包 +{credit} 积分"),
                );
            }
            Ok(_) => {}
        }
        match self.cfg.upstream.claim_compensation(a).await {
            Err(e) => log_growth_err("growth", &a.uid, "claim-compensation", &e),
            Ok(credit) if credit > 0 => {
                crate::logline!("growth {}: compensation ok (+{credit} credit)", uid8(&a.uid));
                self.rec_task(
                    &a.uid,
                    GROWTH_TASK_TITLE,
                    crate::records::RESULT_SUCCESS,
                    &format!("领取活动补偿 +{credit} 积分"),
                );
            }
            Ok(_) => {}
        }
    }

    /// 昨日漏签且有补签卡时补签，返回是否真的补上了（对应 Go `makeUpYesterday`）。
    ///
    /// 判据链（顺序不可调换，每一步都在缩小写操作的范围）：
    ///
    /// ```text
    /// heatmap 昨日格存在且 score==0（确实漏签）
    ///   → 「无该日格」直接放弃（无判据，不是漏签）
    ///   → 「昨日不在本月」直接放弃（上游只允许补当月，请求必然被拒）
    ///   → streak.makeup_cards.balance > 0（有卡可扣）
    ///   → POST makeup-cards/use {target_date: 昨日}
    /// ```
    ///
    /// 为什么值得补：连登一断，7/14/28 三档全部重攒，而一张卡就能保住 ——
    /// 代价远小于收益。查询失败一律静默返回 `false`：补签是纯增益动作，
    /// 判据拿不到时宁可不做（每日都会重试，无写风险）。
    async fn make_up_yesterday(
        &self,
        a: &Auth,
        state: Option<&crate::upstream::growth::GrowthStreakState>,
    ) -> bool {
        // 月初判据先于网络请求：1 号的「昨日」必然在上个月，补签必被拒。
        // 放在最前面可以省掉每日一次的 heatmap 请求。
        if crate::upstream::growth::growth_makeup_out_of_month(Local::now()) {
            return false;
        }
        let cells = match self.cfg.upstream.growth_heatmap(a).await {
            Ok(c) => c,
            // 只读判据失败：静默（无写风险，明日再判）
            Err(_) => return false,
        };
        let yesterday = crate::upstream::growth::growth_yesterday(Local::now());
        let (score, ok) = crate::upstream::growth::heatmap_day_score(&cells, &yesterday);
        if !ok {
            // 无该日格 = 无判据，不动（活跃地图是定长窗口，窗口外日子天然缺席）
            return false;
        }
        if score != 0 {
            // 昨日有分：未漏签，无需补
            return false;
        }
        let balance = state.map(|s| s.makeup_cards.balance).unwrap_or(0);
        if balance <= 0 {
            // 有漏签但无卡：静默（卡由兑换发放，下轮再看）
            return false;
        }
        if let Err(e) = self.cfg.upstream.use_makeup_card(a, &yesterday).await {
            if crate::upstream::growth::is_business_rejection(&e) {
                // 无卡/无漏签/跨月：上游已给出明确答复，属常态，静默。
                return false;
            }
            log_growth_err("growth", &a.uid, &format!("makeup {yesterday}"), &e);
            return false;
        }
        crate::logline!("growth {}: makeup ok {yesterday} (streak kept)", uid8(&a.uid));
        self.rec_task(
            &a.uid,
            GROWTH_TASK_TITLE,
            crate::records::RESULT_SUCCESS,
            &format!("补签 {yesterday}（消耗 1 张补签卡，保住连登）"),
        );
        true
    }

    /// 从高到低挑「已达标且本月未领」的最高档位兑换一次
    ///（对应 Go `redeemGrowthTier`）。
    ///
    /// 一次只领一档：档位是里程碑，同日把 7/14/28 一次领完会让当天产生多次写请求；
    /// 而档位不会过期（同月内有效），每天领一档在月内完全来得及。
    async fn redeem_growth_tier(
        &self,
        a: &Auth,
        state: Option<&crate::upstream::growth::GrowthStreakState>,
    ) {
        let Some(state) = state else { return };
        let tier = growth_eligible_tier(state.days(), &state.redemption);
        if tier.is_empty() {
            // 未达标或本月已领完：正常态，不打日志（很多天都到不了 7d）
            return;
        }
        match self.cfg.upstream.growth_redeem(a, &tier, "").await {
            Ok(res) => {
                crate::logline!(
                    "growth {}: redeem tier={tier} ok (+{} credit, +{} energy, +{} card, +{} chance)",
                    uid8(&a.uid),
                    res.credit_granted,
                    res.energy_granted,
                    res.cards_granted,
                    res.chances_granted
                );
                self.rec_task(
                    &a.uid,
                    GROWTH_TASK_TITLE,
                    crate::records::RESULT_SUCCESS,
                    &format!("兑换连登 {tier} 奖励（+{} 积分）", res.credit_granted),
                );
            }
            Err(e)
                if crate::upstream::growth::is_redeem_already_claimed(&e)
                    || crate::upstream::growth::is_redeem_not_enough_days(&e) =>
            {
                // 本月已领 / 天数不足：上游的幂等答复，属常态，静默。
            }
            Err(e) => log_growth_err("growth", &a.uid, &format!("redeem {tier}"), &e),
        }
    }

    /// 抽奖：只在有次数时抽，每次用新的 `client_token`
    ///（对应 Go `drawGrowthLottery`）。
    ///
    /// 抽奖是**消耗性**的（一次一扣），因此必须先查到确切的余额再抽，不能靠
    /// 「抽到报错为止」试探 —— 那样会多打一次必然失败的写请求。
    async fn draw_growth_lottery(&self, a: &Auth) {
        let chances = match self.cfg.upstream.growth_lottery_chances(a).await {
            Ok(c) => c,
            Err(e) => {
                log_growth_err("growth", &a.uid, "lottery-chances", &e);
                return;
            }
        };
        if chances <= 0 {
            // 无次数：绝大多数账号的常态，静默
            return;
        }
        // 上限存在的意义：chances 由兑换累积而来，理论上有界（每月三档各送 1），
        // 但一旦上游行为变化或本地计数出错，无上限的循环会把次数在一轮里烧光。
        // 取 3 = 单月三档全部兑换能拿到的次数，正常路径永远碰不到这个上限。
        const MAX_GROWTH_LOTTERY_DRAWS: i64 = 3;
        let chances = chances.min(MAX_GROWTH_LOTTERY_DRAWS);
        for _ in 0..chances {
            // 空 = 每次自动生成新 client_token
            match self.cfg.upstream.growth_lottery_draw(a, "").await {
                Ok(res) => {
                    crate::logline!(
                        "growth {}: lottery prize={} type={} (+{} credit)",
                        uid8(&a.uid),
                        res.prize_name,
                        res.prize_type,
                        res.credit_amount
                    );
                    let detail = if res.prize_type.is_empty() {
                        "抽奖获得奖品".to_string()
                    } else {
                        format!("抽奖获得 {}", res.prize_name)
                    };
                    self.rec_task(&a.uid, GROWTH_TASK_TITLE, crate::records::RESULT_SUCCESS, &detail);
                }
                Err(e)
                    if crate::upstream::growth::is_lottery_no_chance(&e)
                        || crate::upstream::growth::is_lottery_disabled(&e) =>
                {
                    // 无次数/未开启：上游答复，属常态，静默停下（剩余次数不可能再成功）。
                    return;
                }
                Err(e) => {
                    log_growth_err("growth", &a.uid, "lottery-draw", &e);
                    return;
                }
            }
        }
    }
}

/// 派出地点固定 4（古镇客栈）：4 个地点收益/时长区间完全相同，无最优解。
const TRAVEL_LOCATION_ID: i64 = 4;

/// 空闲可派出。
const TRAVEL_STATE_IDLE: &str = "idle";
/// 在途。
const TRAVEL_STATE_TRAVELING: &str = "traveling";
/// 到站可领奖。
const TRAVEL_STATE_ARRIVED: &str = "arrived";

/// 账号间限速：全量账号约 40s，避免上游风控。
const TRAVEL_ACCOUNT_DELAY: Duration = Duration::from_millis(800);

/// 活跃地图账号之间的间隔。
const GROWTH_ACCOUNT_DELAY: Duration = Duration::from_millis(500);

/// 账号记录里的任务标题（界面「账号记录 → 任务」按此展示）。
const GROWTH_TASK_TITLE: &str = "活跃地图";

/// 该账号所属的上游自然日（CST），格式 `2006-01-02`。
///
/// 上游每日重置按自然日 00:00 CST（Asia/Shanghai）。中国无夏令时，固定 +8 即可，
/// 不依赖容器 tzdata。
fn travel_day(t: DateTime<Local>) -> String {
    t.with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).expect("+8h"))
        .format("%Y-%m-%d")
        .to_string()
}

/// 从连登快照里挑「尚未领取且已达标」的最高档位；`""` = 无可领档
///（对应 Go `growthEligibleTier`）。
///
/// 达标判据用 `tiers[].days` 与 `days` 直接比较，**不用 `remaining_days`**：
/// 实测该字段与 tiers 不自洽（同一账号 `days=2` 时报 `remaining_days=3`，
/// 而 7d 档的 `days=7`，7-2=5），拿它当门槛会挑错档甚至跳过可领档位。
///
/// 上游 tiers 数组实测按 `days` 升序，但这里按 `days` 取最大值而不是取最后一个
/// 下标 —— 顺序变了也不会静默挑错。
fn growth_eligible_tier(
    days: i64,
    rs: &crate::upstream::growth::GrowthRedemptionStatus,
) -> String {
    let mut best = String::new();
    let mut best_days = -1i64;
    for sp in &rs.tiers {
        if sp.tier.is_empty() || days < sp.days {
            continue; // 未达标
        }
        if rs.claimed(&sp.tier) {
            continue; // 本月已领
        }
        if sp.days > best_days {
            best = sp.tier.clone();
            best_days = sp.days;
        }
    }
    best
}

/// 记录一次**真正值得关注**的失败（传输层 / 5xx）
///（对应 Go `growthmap.go` 的 `logGrowthErr`）。
///
/// 存在的意义是把「上游业务拒绝」与「真的坏了」在代码层面分开：前者是每天都会
/// 发生的常态（已领过、无卡、天数不足），后者才需要人去查。混在一起打日志会让
/// 真正的故障淹没在噪音里。
fn log_growth_err(prefix: &str, uid: &str, what: &str, err: &GatewayError) {
    if crate::upstream::growth::is_business_rejection(err) {
        // 兜底：调用方本应各自静默，这里防的是将来新增分支时漏判。
        return;
    }
    crate::logline!("WARN: {prefix} {}: {what}: {err}", uid8(uid));
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// 本地时区当日 `hour` 点整。
///
/// 用「当日零点 + hour 小时」而不是 `and_hms_opt(hour, 0, 0)`：Go 的 `time.Date`
/// 对越界小时（如 25）会自然进位到次日 01:00，而 `and_hms_opt` 直接返回 `None`。
/// 复刻归一化行为可避免「配置里写了 25 点 ⇒ 该时点静默消失」这种与 Go 不同的表现。
fn local_at_hour(now: DateTime<Local>, hour: i32) -> Option<DateTime<Local>> {
    let midnight = now.date_naive().and_hms_opt(0, 0, 0)?;
    let base = midnight.and_local_timezone(Local).earliest()?;
    Some(base + chrono::Duration::hours(i64::from(hour)))
}

/// 日志用的短 uid（对应 Go `uid8`）。
///
/// 刻意不打印完整 uid：日志会被贴到 issue / PR，完整 uid 属于账号标识。
pub fn uid8(uid: &str) -> &str {
    match uid.char_indices().nth(8) {
        Some((i, _)) => &uid[..i],
        None => uid,
    }
}

/// 从 [`GatewayError`] 里取上游错误类别（对应 Go 的 `errors.As(err, &ue)`）。
pub fn upstream_err_kind(e: &GatewayError) -> ErrKind {
    match e {
        GatewayError::UpstreamError(u) => u.kind,
        _ => ErrKind::None,
    }
}

/// 保留给测试与诊断：把 [`CreditInfo`] 与账号 UID 一起打包打印。
pub fn credit_summary(uid: &str, info: &CreditInfo) -> String {
    format!("{uid}: remain={} expire_at={}", info.remain, info.soonest_expire_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 排程时点 ----

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        use chrono::TimeZone;
        Local
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .expect("valid local time")
    }

    /// `next_fire` 取「今天或明天里最早的那个时点」。
    ///
    /// 关键在「不晚于 now 就 +24h」：09:00 的时点在 08:00 看是今天 09:00，
    /// 在 10:00 看是明天 09:00 —— 否则排程会算出过去的时间而空转。
    #[test]
    fn next_fire_picks_today_or_tomorrow() {
        let now = at(2026, 9, 20, 8, 0);
        assert_eq!(
            Scheduler::next_fire(now, &[9, 21]),
            Some(at(2026, 9, 20, 9, 0)),
            "08:00 时最早的时点是今天 09:00"
        );

        let now = at(2026, 9, 20, 10, 0);
        assert_eq!(
            Scheduler::next_fire(now, &[9, 21]),
            Some(at(2026, 9, 20, 21, 0)),
            "10:00 时 09:00 已过，取今天 21:00"
        );

        let now = at(2026, 9, 20, 22, 0);
        assert_eq!(
            Scheduler::next_fire(now, &[9, 21]),
            Some(at(2026, 9, 21, 9, 0)),
            "22:00 时两个时点都过了，取明天 09:00"
        );
    }

    /// 恰好等于时点的那一刻，`!t.After(now)` 成立 ⇒ 推到明天。
    ///
    /// 这是 Go 的原始行为（`if !t.After(now)`）。若写成 `t.Before(now)`，恰好
    /// 到点的瞬间会算出「现在」，`time.Until` 为 0 ⇒ 立即触发一次，然后下一轮
    /// 又算出同一时刻 ⇒ 无限触发。
    #[test]
    fn next_fire_at_exact_hour_rolls_to_tomorrow() {
        let now = at(2026, 9, 20, 9, 0);
        assert_eq!(
            Scheduler::next_fire(now, &[9]),
            Some(at(2026, 9, 21, 9, 0)),
            "恰好 09:00 应推到明天，避免零延时自旋"
        );
    }

    /// 空时点列表 ⇒ `None`（不是「今天零点」）。
    ///
    /// `qoder_claim_hours` 刻意没有默认值兜底，`None` 正是「该 slot 不产生时点」
    /// 的表达；若返回零时间，`next_wake` 会把它当成一个真实时点参与比较。
    #[test]
    fn next_fire_returns_none_for_empty_hours() {
        let now = at(2026, 9, 20, 8, 0);
        assert_eq!(Scheduler::next_fire(now, &[]), None);
    }

    /// 越界小时按 Go `time.Date` 的自然进位处理，而不是静默丢弃。
    ///
    /// `and_hms_opt(25, 0, 0)` 会返回 `None`，那样「配置写了 25 点」就表现为
    /// 该时点凭空消失，与 Go 不同。这里钉住进位语义。
    #[test]
    fn out_of_range_hour_carries_over_instead_of_disappearing() {
        let now = at(2026, 9, 20, 8, 0);
        assert_eq!(
            Scheduler::next_fire(now, &[25]),
            Some(at(2026, 9, 21, 1, 0)),
            "25 点应进位成次日 01:00"
        );
    }

    /// `last_scheduled_at` 取「最近一次已经过去的时点」。
    #[test]
    fn last_scheduled_at_returns_most_recent_past_slot() {
        let now = at(2026, 9, 20, 22, 30);
        assert_eq!(
            Scheduler::last_scheduled_at(now, &[9, 21]),
            Some(at(2026, 9, 20, 21, 0)),
            "22:30 时最近一次是今天 21:00"
        );

        let now = at(2026, 9, 20, 8, 0);
        assert_eq!(
            Scheduler::last_scheduled_at(now, &[9, 21]),
            Some(at(2026, 9, 19, 21, 0)),
            "08:00 时最近一次是昨天 21:00"
        );
    }

    /// ★ 与 `next_fire` 的关键差异：`last_scheduled_at` **跳过**越界小时。
    ///
    /// 两个函数对同一份配置的边界处理不同，这不是笔误 —— 「下一次」用进位是合理
    /// 的（明天 01:00 确实是个未来时刻），而「上一次」若也进位，25 点会算出
    /// 明天的 01:00（一个**未来**时刻），补跑判定随即失效。故 Go 在这里显式跳过。
    #[test]
    fn last_scheduled_at_skips_out_of_range_hour() {
        let now = at(2026, 9, 20, 12, 0);
        assert_eq!(
            Scheduler::last_scheduled_at(now, &[25, 9]),
            Some(at(2026, 9, 20, 9, 0)),
            "25 应被跳过，只剩 9 点参与比较"
        );
        assert_eq!(
            Scheduler::last_scheduled_at(now, &[25]),
            None,
            "全是越界小时 ⇒ 无有效时点"
        );
    }

    /// 补跑窗口是「过去 30 分钟内错过的时点」。
    #[test]
    fn within_catch_up_only_covers_the_recent_grace_window() {
        let now = at(2026, 9, 20, 9, 10);
        // 9:00 的时点刚过 10 分钟 ⇒ 在 30 分钟窗口内，补跑
        assert!(Scheduler::within_catch_up(now, Some(at(2026, 9, 20, 9, 0))));
        // 8:00 的时点过了 70 分钟 ⇒ 超出窗口，不补
        assert!(!Scheduler::within_catch_up(now, Some(at(2026, 9, 20, 8, 0))));
        // 未来时点不补
        assert!(!Scheduler::within_catch_up(now, Some(at(2026, 9, 20, 10, 0))));
        // 无时点不补
        assert!(!Scheduler::within_catch_up(now, None));
    }

    /// 恰好 30 分钟仍算窗口内（`<=` 而非 `<`），多一秒就出界。
    #[test]
    fn within_catch_up_boundary_is_inclusive() {
        let now = at(2026, 9, 20, 9, 30);
        assert!(
            Scheduler::within_catch_up(now, Some(at(2026, 9, 20, 9, 0))),
            "恰好 30 分钟应算窗口内"
        );
        let now = at(2026, 9, 20, 9, 30);
        let scheduled = at(2026, 9, 20, 8, 59) - chrono::Duration::seconds(59);
        assert!(
            !Scheduler::within_catch_up(now, Some(scheduled)),
            "超过 30 分钟应出界"
        );
    }

    // ---- 任务名 ----

    /// 对外任务名必须与 Go 逐字一致：宿主（GUI / webui）按这些字符串调用。
    #[test]
    fn task_names_are_the_stable_external_identifiers() {
        assert_eq!(TASK_NAME_ACTIVITY, "activity");
        assert_eq!(TASK_NAME_NIGHT_OWL, "nightowl");
        assert_eq!(TASK_NAME_SCHOOL, "school");
        assert_eq!(TASK_NAME_TRIAL, "trial");
        assert_eq!(TASK_NAME_GROWTH_MAP, "growthmap");
        assert_eq!(TASK_NAME_PRODUCT_TASKS, "product_tasks");
        assert_eq!(TASK_NAMES.len(), 6);
    }

    // ---- 任务结果 ----

    /// `skip` 是稳定的英文码（宿主据此分支），`message` 是中文文案。
    ///
    /// 序列化时 `skip` 为空则省略 —— 与 Go 的 `omitempty` 一致，宿主读到的
    /// JSON 形状完全相同。
    #[test]
    fn task_run_result_serializes_like_go() {
        let skipped = TaskRunResult::skipped("trial", "region_mismatch", "该任务是国服专属");
        let v = serde_json::to_value(&skipped).unwrap();
        assert_eq!(v["task"], "trial");
        assert_eq!(v["ran"], false);
        assert_eq!(v["skip"], "region_mismatch");
        assert_eq!(v["message"], "该任务是国服专属");

        let ran = TaskRunResult::ran("activity", "已触发一轮");
        let v = serde_json::to_value(&ran).unwrap();
        assert_eq!(v["ran"], true);
        assert!(v.get("skip").is_none(), "ran=true 时 skip 应被省略");
        assert_eq!(v["message"], "已触发一轮");
    }

    /// `ErrTaskRunning` 的错误文案与 Go 逐字一致 —— 宿主按字符串匹配过它。
    #[test]
    fn task_running_error_message_matches_go() {
        let e = SchedulerError::TaskRunning;
        assert_eq!(e.to_string(), "task already running");
        assert!(is_task_running(&e));
        let other = SchedulerError::UnknownTask("bogus".into());
        assert!(!is_task_running(&other));
    }

    // ---- 产品闸门 / 区域闸门 ----

    fn auth_with(uid: &str, product: &str, domain: &str) -> Auth {
        Auth {
            uid: uid.into(),
            access_token: format!("at-{uid}"),
            product: product.into(),
            domain: domain.into(),
            ..Default::default()
        }
    }

    fn sched(checkin_scope: &str) -> Scheduler {
        let mut cfg = Config::new(Client::new(crate::upstream::client::ClientConfig::default()).unwrap());
        cfg.checkin_scope = checkin_scope.into();
        Scheduler::new(cfg)
    }

    /// ★★ 产品闸门：Qoder / ZCode 账号**不得**被 WorkBuddy 专属任务选中。
    ///
    /// 这是 2026-09-22 修的真实缺陷。所有者现场：
    /// 「这个qoder怎么执行workbuddy的任务了?」「zcode我看到记录里面,也会跑
    /// workbuddy的任务,这不要串任务好吗?」
    ///
    /// 根因：闸门此前**只判区域、完全不判产品**，而所有养号任务遍历
    /// `Pool::list()`（含所有产品的账号）⇒ 打到 WorkBuddy 端点带 Qoder 凭证
    /// ⇒ 401，并在账号记录里写下根本不属于它的任务记录。
    #[test]
    fn workbuddy_only_tasks_reject_other_products() {
        let s = sched("cn");
        let wb_cn = auth_with("u1", "workbuddy", "www.workbuddy.cn");
        let wb_intl = auth_with("u4", "workbuddy", "www.workbuddy.ai");
        let qoder = auth_with("u2", "qoder", "qoder.sh");
        let zcode = auth_with("u3", "zcode", "z.ai");

        for name in [
            TASK_NAME_ACTIVITY,
            TASK_NAME_NIGHT_OWL,
            TASK_NAME_SCHOOL,
            TASK_NAME_GROWTH_MAP,
            TASK_NAME_TRIAL,
        ] {
            // trial 的区域闸门与其它四个**相反**（只跑国际版），故「能放行的
            // WorkBuddy 账号」对它必须换成国际版号 —— 用国服号会撞上区域闸门，
            // 那是另一条判据的拒绝理由，不是产品闸门。
            let wb = if name == TASK_NAME_TRIAL { &wb_intl } else { &wb_cn };
            assert!(s.account_scope_skip(name, wb).is_ok(), "workbuddy 应放行 {name}");
            let err = s.account_scope_skip(name, &qoder).unwrap_err();
            assert_eq!(err, "该任务是 WorkBuddy 专属：其它产品有自己的任务体系");
            let err = s.account_scope_skip(name, &zcode).unwrap_err();
            assert_eq!(err, "该任务是 WorkBuddy 专属：其它产品有自己的任务体系");
        }
    }

    /// 空产品串归一成 workbuddy —— 判据用 `product_of()` 而不是 `product == ""`。
    ///
    /// 老凭证文件没有 `product` 字段，显式写了 `"workbuddy"` 的账号也存在；
    /// 若判据写成 `a.product == ""`，后者会被误判成非 WorkBuddy 而**静默不跑**。
    #[test]
    fn empty_product_normalizes_to_workbuddy() {
        let s = sched("cn");
        let legacy = auth_with("u1", "", "www.workbuddy.cn");
        assert_eq!(legacy.product_of(), PRODUCT_WORKBUDDY);
        assert!(
            s.account_scope_skip(TASK_NAME_ACTIVITY, &legacy).is_ok(),
            "空产品串应归一成 workbuddy 并放行"
        );
    }

    /// ★ trial 的区域判据与其它任务**相反**：只跑国际版。
    #[test]
    fn trial_is_intl_only() {
        let s = sched("cn");
        let cn = auth_with("u1", "workbuddy", "www.workbuddy.cn");
        let intl = auth_with("u2", "workbuddy", "www.workbuddy.ai");

        assert_eq!(
            s.account_scope_skip(TASK_NAME_TRIAL, &cn).unwrap_err(),
            "trial 加油包仅国际版可用：国服没有这个端点"
        );
        assert!(s.account_scope_skip(TASK_NAME_TRIAL, &intl).is_ok());
    }

    /// `checkin_scope = "cn"`（缺省）时国际版账号被跳过；`"all"` 时放行。
    #[test]
    fn checkin_scope_gates_region() {
        let cn = auth_with("u1", "workbuddy", "www.workbuddy.cn");
        let intl = auth_with("u2", "workbuddy", "www.workbuddy.ai");

        let only_cn = sched("cn");
        assert!(only_cn.account_scope_skip(TASK_NAME_ACTIVITY, &cn).is_ok());
        assert_eq!(
            only_cn.account_scope_skip(TASK_NAME_ACTIVITY, &intl).unwrap_err(),
            "该任务是国服专属：国际版没有对应的数据接口"
        );

        let all = sched("all");
        assert!(all.account_scope_skip(TASK_NAME_ACTIVITY, &cn).is_ok());
        assert!(
            all.account_scope_skip(TASK_NAME_ACTIVITY, &intl).is_ok(),
            "checkin_scope=all 时国际版应放行"
        );
    }

    /// `checkin_scope` 的大小写与空白都要容忍（Go 用 `EqualFold` + `TrimSpace`）。
    #[test]
    fn checkin_scope_tolerates_case_and_whitespace() {
        for v in ["all", "ALL", "All", " all ", "\tALL\n"] {
            let s = sched(v);
            assert!(
                s.checkin_scope_allows(&auth_with("u", "workbuddy", "www.workbuddy.ai")),
                "{v:?} 应被识别成 all"
            );
        }
        for v in ["cn", "", "cn ", "none"] {
            let s = sched(v);
            assert!(
                !s.checkin_scope_allows(&auth_with("u", "workbuddy", "www.workbuddy.ai")),
                "{v:?} 应只跑国服"
            );
        }
    }

    // ---- 账号作用域 ----

    /// 无作用域（自动排程）时 `in_account_scope` **恒为 true**。
    ///
    /// 这是本机制能与既有排程共存的关键：排程路径不设作用域，行为逐字不变。
    #[test]
    fn account_scope_is_open_when_unset() {
        assert!(Scheduler::in_account_scope(None, "u1"));
        assert!(Scheduler::in_account_scope(None, "u2"));
        assert!(Scheduler::in_account_scope(Some(""), "u1"));
        assert_eq!(Scheduler::account_scope_uid(None), "");
        assert_eq!(Scheduler::account_scope_uid(Some("")), "");
    }

    /// 单账号作用域下只放行该账号。
    #[test]
    fn account_scope_limits_to_one_account() {
        let scope = Some("u1");
        assert!(Scheduler::in_account_scope(scope, "u1"));
        assert!(!Scheduler::in_account_scope(scope, "u2"));
        assert_eq!(Scheduler::account_scope_uid(scope), "u1");
    }

    // ---- 未知任务名 ----

    /// 未知任务名返回 `UnknownTask` 而不是静默成功 —— 宿主拼错名字必须能看见。
    #[tokio::test]
    async fn unknown_task_name_is_an_error() {
        let s = sched("cn");
        let (res, err) = s.run_task_by_name("bogus").await;
        assert_eq!(res.task, "bogus");
        assert!(!res.ran);
        assert!(!is_task_running(err.as_ref().unwrap()));
        assert!(matches!(err, Some(SchedulerError::UnknownTask(ref n)) if n == "bogus"));
    }

    /// `product_tasks` 走独立分支：账号作用域对它**不适用**（它按产品遍历，
    /// 网关不知道哪些 uid 属于哪个产品），故不返回 `account_not_found`。
    #[tokio::test]
    async fn product_tasks_ignores_account_scope() {
        let s = sched("cn");
        let (res, err) = s.run_task_for(TASK_NAME_PRODUCT_TASKS, "nonexistent-uid").await;
        assert_eq!(res.task, TASK_NAME_PRODUCT_TASKS);
        // 未接线执行体 ⇒ 报「该构建未接入」，而不是「账号不存在」
        assert_eq!(res.skip, "unsupported");
        assert!(err.is_none());
    }

    /// 单账号触发时，池里没有这个 uid ⇒ `account_not_found`（而非静默跑空）。
    #[tokio::test]
    async fn missing_account_reports_account_not_found() {
        let s = sched("cn");
        let (res, err) = s.run_task_for(TASK_NAME_ACTIVITY, "nope").await;
        assert_eq!(res.skip, "account_not_found");
        assert!(!res.ran);
        assert!(res.message.contains("不在网关账号池中"));
        assert!(err.is_none(), "拿不到账号是确定的结论，不是执行错误");
    }

    /// 单账号触发时产品不符 ⇒ `region_mismatch`，且**提前**回报原因。
    #[tokio::test]
    async fn wrong_product_account_reports_region_mismatch() {
        let mut cfg =
            Config::new(Client::new(crate::upstream::client::ClientConfig::default()).unwrap());
        cfg.checkin_scope = "cn".into();
        let pool = Arc::new(Mutex::new(Pool::new(String::new())));
        pool.lock().unwrap().add(auth_with("q1", "qoder", "qoder.sh"));
        cfg.pool = Some(pool);
        let s = Scheduler::new(cfg);

        let (res, err) = s.run_task_for(TASK_NAME_ACTIVITY, "q1").await;
        assert_eq!(res.skip, "region_mismatch");
        assert_eq!(res.message, "该任务是 WorkBuddy 专属：其它产品有自己的任务体系");
        assert!(err.is_none());
    }

    // ---- 并发闸门 ----

    /// 同一个任务名不能同时跑两轮（对应 Go `claimTask` / `releaseTask`）。
    #[test]
    fn claim_task_is_exclusive_and_released() {
        let s = sched("cn");
        assert!(s.claim_task("activity"), "第一次应能认领");
        assert!(!s.claim_task("activity"), "同一名字第二次应失败");
        assert!(s.claim_task("nightowl"), "不同名字互不影响");
        s.release_task("activity");
        assert!(s.claim_task("activity"), "释放后应能重新认领");
    }

    /// `TaskBusyGuard` 在 drop 时释放（对应 Go `defer releaseTask`）。
    #[test]
    fn task_busy_guard_releases_on_drop() {
        let s = sched("cn");
        {
            assert!(s.claim_task("trial"));
            let _g = TaskBusyGuard {
                sched: &s,
                name: "trial".into(),
            };
        }
        assert!(s.claim_task("trial"), "guard drop 后应已释放");
    }

    /// 到点执行时若被手动触发占用 ⇒ 记日志跳过，**不排队**。
    ///
    /// 排队会让「迟到唤醒 + 用户刚点过立即执行」叠成两轮；而下一个整点很快
    /// 再来，漏一轮的代价远小于双倍上报。
    #[tokio::test]
    async fn run_care_task_skips_when_busy_instead_of_queueing() {
        let s = sched("cn");
        assert!(s.claim_task("activity"));
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        s.run_care_task("activity", async move {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .await;
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst), "被占用时不应执行");
        // 占用标记仍在（是调用方持有的，不归 run_care_task 释放）
        assert!(!s.claim_task("activity"));
    }

    // ---- 默认值 ----

    /// `new` 的默认值与 Go 逐字一致。
    #[test]
    fn new_applies_go_defaults() {
        let s = sched("");
        let c = s.config();
        assert_eq!(c.checkin_hours, vec![9, 21]);
        assert_eq!(c.keepalive_hours, vec![22]);
        assert_eq!(c.activity_hours, vec![10]);
        assert_eq!(c.night_owl_hours, vec![1]);
        assert_eq!(c.school_hours, vec![12]);
        assert_eq!(c.trial_hours, vec![9, 21]);
        assert_eq!(c.activity_report_count, DEFAULT_ACTIVITY_REPORT_COUNT);
        assert_eq!(c.activity_report_count, 3);
    }

    /// ★ `qoder_claim_hours` 刻意**不兜底** —— 空切片 ⇒ 该 slot 不产生时点。
    ///
    /// 与其它六个时点不同，这不是遗漏：Qoder 权益领取的时点必须由宿主显式配置
    ///（`schedule.qoder_claim_hours`），未配置即「不自动跑」。
    #[test]
    fn qoder_claim_hours_has_no_default() {
        let s = sched("");
        assert!(s.config().qoder_claim_hours.is_empty());
        assert_eq!(Scheduler::next_fire(at(2026, 9, 20, 8, 0), &s.config().qoder_claim_hours), None);
    }

    /// 显式配置的时点不被默认值覆盖。
    #[test]
    fn explicit_hours_win_over_defaults() {
        let mut cfg =
            Config::new(Client::new(crate::upstream::client::ClientConfig::default()).unwrap());
        cfg.checkin_hours = vec![7];
        cfg.activity_report_count = 5;
        let s = Scheduler::new(cfg);
        assert_eq!(s.config().checkin_hours, vec![7]);
        assert_eq!(s.config().activity_report_count, 5);
    }

    /// 零值 `Config` 即「全部任务启用」—— 这正是用 `*_disabled` 而非 `*_enabled`
    /// 命名的理由：若用 `enabled`，`Config::default()` 会静默关掉全部任务。
    #[test]
    fn zero_config_enables_everything() {
        let s = sched("");
        let c = s.config();
        assert!(!c.checkin_disabled);
        assert!(!c.keepalive_disabled);
        assert!(!c.activity_disabled);
        assert!(!c.night_owl_disabled);
        assert!(!c.school_disabled);
        assert!(!c.trial_disabled);
        assert!(!c.qoder_claim_disabled);
    }

    // ---- next_wake ----

    /// `next_wake` 收集**同一时刻**的全部任务类型（如签到与旅行都在 9 点）。
    #[test]
    fn next_wake_collects_all_kinds_at_the_same_instant() {
        let s = sched("cn");
        // 08:00：签到 9/21、保活 22、活跃 10、夜猫 1、开学 12、trial 9/21
        // ⇒ 最早是 09:00，此刻有签到与 trial 两个任务
        let (next, kinds) = s.next_wake(at(2026, 9, 20, 8, 0));
        assert_eq!(next, Some(at(2026, 9, 20, 9, 0)));
        assert!(kinds.contains(&TaskKind::Checkin), "9 点应含签到");
        assert!(kinds.contains(&TaskKind::Trial), "9 点应含 trial");
        assert!(!kinds.contains(&TaskKind::Keepalive), "保活在 22 点，不在 9 点");
    }

    /// 全部任务禁用 ⇒ 无唤醒时点（`None`），排程循环不空转。
    #[test]
    fn next_wake_is_none_when_everything_is_disabled() {
        let mut cfg =
            Config::new(Client::new(crate::upstream::client::ClientConfig::default()).unwrap());
        cfg.checkin_disabled = true;
        cfg.keepalive_disabled = true;
        cfg.activity_disabled = true;
        cfg.night_owl_disabled = true;
        cfg.school_disabled = true;
        cfg.trial_disabled = true;
        cfg.qoder_claim_disabled = true;
        let s = Scheduler::new(cfg);
        let (next, kinds) = s.next_wake(at(2026, 9, 20, 8, 0));
        assert_eq!(next, None);
        assert!(kinds.is_empty());
    }

    /// ★ 活跃地图与活跃上报**同一时点**（`activity_hours`），且同受
    /// `activity_disabled` 闸门控制。
    ///
    /// 它此前只有手动入口没有 slot，用户不点按钮就永远不执行。
    #[test]
    fn growth_map_shares_the_activity_slot() {
        let s = sched("cn");
        let (next, kinds) = s.next_wake(at(2026, 9, 20, 9, 30));
        // 9:30 后最早的时点是 10:00（活跃上报 + 活跃地图）
        assert_eq!(next, Some(at(2026, 9, 20, 10, 0)));
        assert!(kinds.contains(&TaskKind::Activity));
        assert!(kinds.contains(&TaskKind::GrowthMap));

        // 关掉活跃上报 ⇒ 活跃地图也停（同闸）
        let mut cfg =
            Config::new(Client::new(crate::upstream::client::ClientConfig::default()).unwrap());
        cfg.activity_disabled = true;
        let s = Scheduler::new(cfg);
        let (_, kinds) = s.next_wake(at(2026, 9, 20, 9, 30));
        assert!(!kinds.contains(&TaskKind::GrowthMap), "活跃上报停则活跃地图也停");
    }

    /// Qoder 权益领取复用**同一套** slot 机制（不是独立 ticker）。
    ///
    /// 独立 ticker 会有两个调度器各算各的，到点时可能同时触发或互相错开
    ///（前者并发、后者漏跑）。
    #[test]
    fn qoder_claim_uses_the_same_slot_mechanism() {
        let mut cfg =
            Config::new(Client::new(crate::upstream::client::ClientConfig::default()).unwrap());
        cfg.qoder_claim_hours = vec![10, 21];
        let s = Scheduler::new(cfg);
        // 09:30：活跃上报 10 点、Qoder 领取 10 点 ⇒ 同一时刻一并执行
        let (next, kinds) = s.next_wake(at(2026, 9, 20, 9, 30));
        assert_eq!(next, Some(at(2026, 9, 20, 10, 0)));
        assert!(kinds.contains(&TaskKind::QoderClaim));
        assert!(kinds.contains(&TaskKind::Activity));
    }

    // ---- uid8 ----

    /// 日志只打 uid 前 8 位（按**字符**边界，不是字节）。
    ///
    /// 日志会被贴到 issue / PR，完整 uid 属于账号标识。按字节切会在多字节
    /// 字符中间截断而 panic。
    #[test]
    fn uid8_truncates_on_char_boundary() {
        assert_eq!(uid8("short"), "short");
        assert_eq!(uid8("12345678"), "12345678");
        assert_eq!(uid8("1234567890"), "12345678");
        // 多字节字符：9 个汉字应截成 8 个，而不是按字节切出乱码
        assert_eq!(uid8("一二三四五六七八九"), "一二三四五六七八");
    }

    // ---- 错误分类 ----

    /// 从 `GatewayError` 里取上游错误类别（对应 Go 的 `errors.As`）。
    #[test]
    fn upstream_err_kind_extracts_kind() {
        use crate::upstream::classify::{ErrKind, UpstreamError};
        let e = GatewayError::UpstreamError(UpstreamError {
            kind: ErrKind::SessionDead,
            status: 401,
            msg: "12153".into(),
        });
        assert_eq!(upstream_err_kind(&e), ErrKind::SessionDead);

        let other = GatewayError::Other("boom".into());
        assert_eq!(upstream_err_kind(&other), ErrKind::None);
    }

    // ---- 每日标记 ----

    /// 领养「当日已试」标记的读写与清除。
    ///
    /// 门槛未达的账号当日不再重试，避免同日多趟对上游重试轰炸；但活跃上报
    /// **成功后**要强制重试一次（`travel_adopt_force`），因为刚补满对话量，
    /// 此前「门槛未达」的判断已经失效。
    #[test]
    fn adopt_marks_round_trip() {
        let s = sched("cn");
        assert!(!s.adopt_tried_today("u1"));
        s.mark_adopt_tried("u1");
        assert!(s.adopt_tried_today("u1"));
        assert!(!s.adopt_tried_today("u2"), "标记是按账号隔离的");
        s.clear_adopt_tried("u1");
        assert!(!s.adopt_tried_today("u1"));
    }

    /// 活跃地图「当日已跑」标记的读写。
    #[test]
    fn growth_claimed_marks_round_trip() {
        let s = sched("cn");
        assert!(!s.growth_claimed_today("u1"));
        s.mark_growth_claimed("u1");
        assert!(s.growth_claimed_today("u1"));
        assert!(!s.growth_claimed_today("u2"));
    }

    /// 标记用的自然日是 **CST**（与夜猫窗口判定同一口径）。
    ///
    /// 换台机器 / 改系统时区不该让「今天」的定义漂移，否则同一天可能被判成
    /// 两天而多跑一轮。
    #[test]
    fn travel_day_uses_cst() {
        // 2026-09-20 23:30 CST 的 UTC 是 15:30，CST 自然日仍是 09-20
        let d = travel_day(at(2026, 9, 20, 23, 30));
        assert_eq!(d, "2026-09-20");
        // 跨过 CST 午夜后才是次日
        assert_eq!(travel_day(at(2026, 9, 21, 0, 30)), "2026-09-21");
    }

    // ---- 档位挑选 ----

    /// 从高到低挑「已达标且本月未领」的最高档位。
    ///
    /// ★ 达标判据用 `tiers[].days` 与 `days` 直接比较，**不用 `remaining_days`**：
    /// 实测该字段与 tiers 不自洽（同一账号 `days=2` 时报 `remaining_days=3`，
    /// 而 7d 档的 `days=7`，7-2=5），拿它当门槛会挑错档甚至跳过可领档位。
    #[test]
    fn eligible_tier_picks_highest_unclaimed() {
        use crate::upstream::growth::{GrowthRedemptionStatus, GrowthTierSpec};
        let rs = GrowthRedemptionStatus {
            tiers: vec![
                GrowthTierSpec { tier: "7d".into(), days: 7 },
                GrowthTierSpec { tier: "14d".into(), days: 14 },
                GrowthTierSpec { tier: "28d".into(), days: 28 },
            ],
            claimed: vec![],
        };
        assert_eq!(growth_eligible_tier(30, &rs), "28d", "30 天应挑最高档");
        assert_eq!(growth_eligible_tier(14, &rs), "14d");
        assert_eq!(growth_eligible_tier(7, &rs), "7d");
        assert_eq!(growth_eligible_tier(6, &rs), "", "未达标 ⇒ 无可领档");
    }

    /// 已领取的档位被跳过（`claimed` 判定）。
    #[test]
    fn eligible_tier_skips_claimed_tiers() {
        use crate::upstream::growth::{GrowthRedemptionStatus, GrowthTierSpec};
        let rs = GrowthRedemptionStatus {
            tiers: vec![
                GrowthTierSpec { tier: "7d".into(), days: 7 },
                GrowthTierSpec { tier: "14d".into(), days: 14 },
            ],
            claimed: vec!["14d".into()],
        };
        assert_eq!(growth_eligible_tier(20, &rs), "7d", "14d 已领 ⇒ 降到 7d");
    }

    /// ★ 按 `days` 取最大值而不是取最后一个下标。
    ///
    /// 上游 `tiers` 数组实测按 `days` 升序，但顺序变了也不该静默挑错档。
    #[test]
    fn eligible_tier_is_order_independent() {
        use crate::upstream::growth::{GrowthRedemptionStatus, GrowthTierSpec};
        // 故意打乱顺序：28d 在最前
        let rs = GrowthRedemptionStatus {
            tiers: vec![
                GrowthTierSpec { tier: "28d".into(), days: 28 },
                GrowthTierSpec { tier: "7d".into(), days: 7 },
                GrowthTierSpec { tier: "14d".into(), days: 14 },
            ],
            claimed: vec![],
        };
        assert_eq!(
            growth_eligible_tier(30, &rs),
            "28d",
            "顺序打乱后仍应挑 days 最大的档"
        );
    }

    /// 空 `tier` 的档位被跳过（上游数据不完整时的防御）。
    #[test]
    fn eligible_tier_skips_empty_tier_name() {
        use crate::upstream::growth::{GrowthRedemptionStatus, GrowthTierSpec};
        let rs = GrowthRedemptionStatus {
            tiers: vec![
                GrowthTierSpec { tier: "".into(), days: 28 },
                GrowthTierSpec { tier: "7d".into(), days: 7 },
            ],
            claimed: vec![],
        };
        assert_eq!(growth_eligible_tier(30, &rs), "7d");
    }

    // ---- 本地时刻 ----

    /// `local_at_hour` 对越界小时自然进位（复刻 Go `time.Date`）。
    #[test]
    fn local_at_hour_carries_over() {
        let now = at(2026, 9, 20, 8, 0);
        assert_eq!(local_at_hour(now, 9), Some(at(2026, 9, 20, 9, 0)));
        assert_eq!(
            local_at_hour(now, 25),
            Some(at(2026, 9, 21, 1, 0)),
            "25 点应进位成次日 01:00"
        );
        assert_eq!(
            local_at_hour(now, 0),
            Some(at(2026, 9, 20, 0, 0)),
            "0 点应回到当日零点"
        );
    }

    // ---- 取消信号 ----

    /// 取消后 `is_cancelled` 立即为真，`sleep` 立即返回 `false`。
    #[tokio::test]
    async fn cancel_makes_sleep_return_false() {
        let c = Cancel::new();
        assert!(!c.is_cancelled());
        assert!(c.sleep(Duration::from_millis(5)).await, "未取消时 sleep 应返回 true");
        c.cancel();
        assert!(c.is_cancelled());
        assert!(
            !c.sleep(Duration::from_secs(60)).await,
            "已取消时 sleep 应立即返回 false，而不是等满 60 秒"
        );
    }

    /// 在 `sleep` 进行中取消 ⇒ 立即醒来返回 `false`。
    ///
    /// ★ 这正是排程循环等唤醒时点时的路径：进程要退出时不能等满 15 分钟的
    /// 积分巡检间隔。
    #[tokio::test]
    async fn cancel_wakes_up_an_in_flight_sleep() {
        let c = Arc::new(Cancel::new());
        let c2 = Arc::clone(&c);
        let started = Arc::new(tokio::sync::Notify::new());
        let started2 = Arc::clone(&started);
        let h = tokio::spawn(async move {
            started2.notify_one();
            c2.sleep(Duration::from_secs(3600)).await
        });
        started.notified().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        c.cancel();
        let slept_full = tokio::time::timeout(Duration::from_secs(5), h)
            .await
            .expect("取消后应立即返回，不该等满 3600 秒")
            .unwrap();
        assert!(!slept_full, "被取消的 sleep 应返回 false");
    }

    /// `cancelled()` 在已取消时立即返回（不挂起）。
    #[tokio::test]
    async fn cancelled_returns_immediately_when_already_cancelled() {
        let c = Cancel::new();
        c.cancel();
        tokio::time::timeout(Duration::from_secs(1), c.cancelled())
            .await
            .expect("已取消时 cancelled() 应立即返回");
    }

    /// ★ 取消发生在「查标志之后、await 之前」也不能丢唤醒。
    ///
    /// 这正是不用 `Notify` 的理由：`notify_waiters()` 只唤醒**当前**等待者，
    /// 存在「先查标志（未取消）→ 取消发生（无等待者）→ 才 await notified()」
    /// 的丢唤醒窗口，而那正是排程器每次等待唤醒时点都会走的路径。
    /// `watch` 带版本号可闭合该窗口。
    #[tokio::test]
    async fn cancel_between_check_and_await_is_not_lost() {
        let c = Cancel::new();
        // 模拟「查完标志」的那一刻
        assert!(!c.is_cancelled());
        // 此刻取消发生，还没有任何等待者
        c.cancel();
        // 然后才进入等待 —— 必须立即返回，而不是永久挂起
        tokio::time::timeout(Duration::from_secs(1), c.cancelled())
            .await
            .expect("查标志与 await 之间的取消不能丢");
    }
}
