//! 成长任务「一键完成」的编排层（对应 Go `internal/growtask/runner.go`）。
//!
//! # 职责边界
//!
//! - `upstream` —— 只负责「一次 HTTP 调用长什么样」（端点、头、信封、事件形状）；
//! - `growtask` —— 只负责「按什么顺序、做几次、何时算成功、什么时候领奖」；
//! - `scheduler` —— 只负责「什么时候触发」。
//!
//! 三者分开后，「一键完成」既能被用户手动触发（宿主调 [`Runner::run_one`] /
//! [`Runner::run_all`]），也能被排程复用（scheduler 定时跑），而不必把动作实现
//! 复制两份。
//!
//! # 核心不变量：上报 200 ≠ 计分
//!
//! 服务端的行为计分是**异步**的，且用错客户端指纹时会被**静默丢弃**
//!（HTTP 200 但进度不动）。因此每个动作完成后都必须回读任务进度，达标才领奖
//! —— 绝不把「上报成功」当成「任务完成」。这是本模块存在的根本理由。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};

use crate::auth::Auth;
use crate::records::Recorder;
use crate::scheduler::{uid8, Cancel};
use crate::upstream::client::Client;
use crate::upstream::tasks::{is_growth_task_not_completed, GrowthTask, TASK_ACCEPT_ACCEPTED};

use super::actions::{self, action_for, Action};
use super::tasks::locked_text;

// ---------------------------------------------------------------------------
// 状态常量
// ---------------------------------------------------------------------------

/// 动作已执行（是否达标另看 `claimed` / `progress_after`）。
pub const STATUS_DONE: &str = "done";
/// 无需执行：已完成 / 该账号无此任务 / 前置条件不满足。
pub const STATUS_SKIPPED: &str = "skipped";
pub const STATUS_ERROR: &str = "error";
/// 需真实人工动作。
pub const STATUS_UNSUPPORTED: &str = "unsupported";

// ---------------------------------------------------------------------------
// 默认参数
// ---------------------------------------------------------------------------

const DEFAULT_POLL_ATTEMPTS: u32 = 4;
/// 轮询间隔是**实测标定值**：上游计分异步，对话类任务完成后立即回读仍是 0/1，
/// 约 5–8 秒后才变 1/1（参考实现三账号实测）。一次性回读会把「还在计分」误判成
/// 「未达标」，从而跳过自动领奖 —— 奖励就此漏掉且用户看不出来。4 次 × 3 秒 ≈
/// 12 秒预算。
const DEFAULT_POLL_GAP: Duration = Duration::from_secs(3);
/// 对齐真实客户端节奏，避免秒级连发形成异常流量画像。
const DEFAULT_REPORT_GAP: Duration = Duration::from_millis(1050);
/// 比 `report_gap` 更大：对话消耗 token 且在上游留真实会话记录。
const DEFAULT_CHAT_GAP: Duration = Duration::from_secs(6);
/// ★ 45 秒**不是保守取值，而是该任务的反作弊阈值**：上游对「有效对话」有节奏
/// 校验，数秒级连发的事件会先被计入进度、随后被判无效**整体回滚**。调小不会更
/// 快完成，只会让本轮上报全部作废。详见 [`super::actions`] 的 `run_mp_chat_task`。
const DEFAULT_MP_CHAT_GAP: Duration = Duration::from_secs(45);

/// 汇总记录的标题（单项记录用任务标题，回落任务码）。
const RECORDS_TITLE: &str = "成长任务";

/// 账号忙碌时的文案。
///
/// ★ 暴露成常量而非只回一句文案：宿主可据此返回 409（与「执行失败」区分），
/// 界面才能提示「请等本轮结束」而不是「任务出错」。
pub const ERR_ACCOUNT_BUSY: &str = "该账号有任务动作正在执行中";

// ---------------------------------------------------------------------------
// 参数与执行体
// ---------------------------------------------------------------------------

/// 构造参数（对应 Go `Options`）。
///
/// 五个可调项为 `0` 时沿用默认值（Go 侧是 `<= 0`）。
pub struct Options {
    /// 上游客户端（必填）。
    pub upstream: Client,
    /// 账号记录回写器；`None` = 不记录。
    ///
    /// ★ 宿主的网关子进程把 stdout 丢进 `Stdio::null`，任务照跑但界面上一条痕迹
    /// 都没有。写 `account_records.json` 才能让「成长任务」出现在
    /// 「账号记录 → 任务」里。
    pub records: Option<Arc<Recorder>>,
    pub poll_attempts: u32,
    pub poll_gap: Duration,
    pub report_gap: Duration,
    pub chat_gap: Duration,
    pub mp_chat_gap: Duration,
}

impl Options {
    /// 只指定上游，其余沿用默认。
    pub fn new(upstream: Client) -> Self {
        Options {
            upstream,
            records: None,
            poll_attempts: 0,
            poll_gap: Duration::ZERO,
            report_gap: Duration::ZERO,
            chat_gap: Duration::ZERO,
            mp_chat_gap: Duration::ZERO,
        }
    }

    pub fn with_records(mut self, records: Option<Arc<Recorder>>) -> Self {
        self.records = records;
        self
    }
}

/// 成长任务执行体。
///
/// # 并发模型
///
/// **账号级互斥**（同账号的动作串行）。同账号并发跑多个动作不仅浪费上游配额
///（expert 系含真实对话），还会让进度回读互相干扰 —— A 动作刚报的事件可能被
/// 误算到 B 动作的进度差异里。
pub struct Runner {
    pub(crate) up: Client,
    pub(crate) rec: Option<Arc<Recorder>>,
    pub(crate) poll_attempts: u32,
    pub(crate) poll_gap: Duration,
    pub(crate) report_gap: Duration,
    pub(crate) chat_gap: Duration,
    pub(crate) mp_chat_gap: Duration,
    /// 正在执行动作的账号 uid 集合。只用 `std::sync::Mutex` 且**不跨 await 持有**。
    busy: Mutex<HashSet<String>>,
}

impl Runner {
    pub fn new(o: Options) -> Self {
        Runner {
            up: o.upstream,
            rec: o.records,
            poll_attempts: if o.poll_attempts == 0 {
                DEFAULT_POLL_ATTEMPTS
            } else {
                o.poll_attempts
            },
            poll_gap: if o.poll_gap.is_zero() {
                DEFAULT_POLL_GAP
            } else {
                o.poll_gap
            },
            report_gap: if o.report_gap.is_zero() {
                DEFAULT_REPORT_GAP
            } else {
                o.report_gap
            },
            chat_gap: if o.chat_gap.is_zero() {
                DEFAULT_CHAT_GAP
            } else {
                o.chat_gap
            },
            mp_chat_gap: if o.mp_chat_gap.is_zero() {
                DEFAULT_MP_CHAT_GAP
            } else {
                o.mp_chat_gap
            },
            busy: Mutex::new(HashSet::new()),
        }
    }

    /// 尝试占用某账号。已在执行中返回 `false`。
    pub fn lock(&self, uid: &str) -> bool {
        let mut g = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        if g.contains(uid) {
            return false;
        }
        g.insert(uid.to_string());
        true
    }

    pub fn unlock(&self, uid: &str) {
        let mut g = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        g.remove(uid);
    }

    /// 占用某账号，返回的守卫**在 drop 时自动解锁**（对应 Go 的
    /// `defer r.Unlock(uid)`）。已被占用 ⇒ `None`。
    ///
    /// ★ 优先用这个而不是 [`Runner::lock`]：手动 `unlock` 在 future 被 drop
    /// 时不会执行，会让该账号**永久**卡死（见 [`AccountGuard`]）。
    pub fn try_lock(&self, uid: &str) -> Option<AccountGuard<'_>> {
        if self.lock(uid) {
            Some(AccountGuard {
                runner: self,
                uid: uid.to_string(),
            })
        } else {
            None
        }
    }

    /// 账号记录回写器（宿主拿它做「账号库 id → uid」翻译，见 `/tasks/growth`）。
    pub fn records(&self) -> Option<&Arc<Recorder>> {
        self.rec.as_ref()
    }
}

/// 账号级互斥的 RAII 守卫：**drop 时自动解锁**。
///
/// # 为什么必须是 RAII
///
/// Go 侧两处持有账号锁的地方都是 `defer r.Unlock(...)`
///（`internal/growtask/tasks.go:350`、`cmd/server/growth_tasks.go:116`），而
/// `defer` 在**任何**返回路径上都执行。Rust 里若把 `unlock` 写在 `await`
/// 之后，一旦 future 在中途被 drop，`unlock` 就永远不会执行 —— 而 axum 在
/// **客户端断开连接**时正是这样 drop handler future 的。
///
/// 后果不是「一次失败」而是**账号永久卡死**：该账号此后每次请求都返回
/// 「该账号的任务正在执行中」，只有重启网关才能恢复。
///
/// ★ 已实机复现（2026-10-01，`test-bin\inst-growtask-rust`）：客户端 2 秒超时
/// 断开 ⇒ 上游随即恢复瞬时响应 ⇒ 同一账号连续 150 秒 30 次重试**全部**被拒；
/// 同一时刻 `list` 与该账号的其它操作正常、**其它账号正常** ⇒ 泄漏是账号级且
/// 永久的。修复后同样的探针在约 5 秒内恢复可执行。
///
/// ★ 不要把这里换回「手动 unlock」：`Drop` 覆盖**所有**路径（含 panic 与取消），
/// 手动调用只覆盖作者当时想到的那一条。
pub struct AccountGuard<'a> {
    runner: &'a Runner,
    uid: String,
}

impl Drop for AccountGuard<'_> {
    fn drop(&mut self) {
        self.runner.unlock(&self.uid);
    }
}

impl AccountGuard<'_> {
    /// 被守卫的账号 uid。
    pub fn uid(&self) -> &str {
        &self.uid
    }
}

// ---------------------------------------------------------------------------
// 结果结构（JSON 字段名与 Go 逐字一致）
// ---------------------------------------------------------------------------

fn is_empty_string(s: &String) -> bool {
    s.is_empty()
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

/// 单项任务的结果（对应 Go `ItemResult`）。
///
/// ★ `progress_before` / `progress_after` 是「上报 200 ≠ 计分」的证据：只有
/// `progress_after` 达标才算真的完成，因此必须把它回报给用户，而不是只报
/// 「上报成功」。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ItemResult {
    #[serde(rename = "task_code")]
    pub code: String,
    #[serde(skip_serializing_if = "is_empty_string")]
    pub title: String,
    #[serde(skip_serializing_if = "is_empty_string")]
    pub desc: String,
    pub status: String,
    pub message: String,
    #[serde(skip_serializing_if = "is_empty_string")]
    pub progress_before: String,
    #[serde(skip_serializing_if = "is_empty_string")]
    pub progress_after: String,
    #[serde(skip_serializing_if = "is_false")]
    pub claimed: bool,
    #[serde(skip_serializing_if = "is_zero_i64")]
    pub credit: i64,
    #[serde(skip_serializing_if = "is_zero_i64")]
    pub energy: i64,
    #[serde(skip_serializing_if = "is_empty_string")]
    pub claim_error: String,
    #[serde(skip_serializing_if = "is_false")]
    pub needs_chat: bool,
}

/// 单账号的整轮结果（对应 Go `AccountResult`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountResult {
    /// 账号标识（宿主用它回填界面；本模块不打进日志）。
    pub uid: String,
    pub realm: String,
    #[serde(skip_serializing_if = "is_false")]
    pub skipped: bool,
    #[serde(skip_serializing_if = "is_empty_string")]
    pub skip_reason: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<ItemResult>,
    pub claimed: usize,
    pub credit: i64,
    pub energy: i64,
    #[serde(skip_serializing_if = "is_empty_string")]
    pub error: String,
}

/// 「整轮跑全部账号」的参数（对应 Go `FleetOptions`）。
///
/// ★ Go 侧还有 `OnAccount` / `OnResult` 两个回调字段，**全项目无人使用**，
/// 故 Rust 侧不移植。
#[derive(Debug, Clone)]
pub struct FleetOptions {
    pub accounts: Vec<Auth>,
    /// ★ 上限刻意压到 [`MAX_FLEET_CONCURRENCY`]：每个账号的动作里都可能有真实
    /// 对话，并发过高会在上游形成异常流量画像，得不偿失。账号**内**始终串行
    ///（见 [`Runner`] 的账号级互斥）。
    pub concurrency: usize,
    /// ★ 默认 `false`：国际版的成长域是另一套任务集（`first_chat` /
    /// `skill_installed` …，无奖励字段、无 progress），本模块的动作对它无意义。
    /// 实测确认。
    pub include_intl: bool,
}

impl Default for FleetOptions {
    fn default() -> Self {
        FleetOptions {
            accounts: Vec::new(),
            concurrency: 1,
            include_intl: false,
        }
    }
}

/// 车队并发上限（Go `maxFleetConcurrency`）。
pub const MAX_FLEET_CONCURRENCY: usize = 3;

/// 多账号汇总（对应 Go `FleetSummary`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FleetSummary {
    /// 参与账号数（已排除跳过的）。
    pub accounts: usize,
    pub skipped: usize,
    pub claimed: usize,
    pub credit: i64,
    pub energy: i64,
    /// 有整轮错误的账号数。
    pub failed: usize,
}

// ---------------------------------------------------------------------------
// 主流程
// ---------------------------------------------------------------------------

impl Runner {
    /// 跑某账号的**全部**成长任务（对应 Go `RunAll`）。
    ///
    /// # 流程（顺序有依赖，不可调换）
    ///
    /// 1. 拉任务列表；
    /// 2. 批量报名未报名的任务 —— 未报名时上游不下发 `progress`（实测为
    ///    `null`），不先报名则后续回读拿不到进度，「上报 200 ≠ 计分」的自检就
    ///    失去依据；
    /// 3. 逐项执行动作 —— 单项失败不影响后续项（尽力而为）；
    /// 4. 每项执行后回读进度（有界轮询等异步计分落定），达标即自动领奖。
    ///
    /// `cancel` 被触发时中途停止（**已完成的项保留在结果里**）；锁由调用方通过
    /// [`Runner::lock`] 持有。
    pub async fn run_all(&self, cancel: &Cancel, a: &Auth) -> Vec<ItemResult> {
        let mut out: Vec<ItemResult> = Vec::new();
        let before = match self.up.list_growth_tasks(a).await {
            Ok(t) => t,
            Err(e) => {
                // 列表都拉不到（网络/鉴权/区域），后续动作没有任何依据，直接整体
                // 报错。不在这里编造「全部跳过」：那会让用户以为任务都已处理过。
                let msg = format!("拉取任务列表失败: {e}");
                out.push(ItemResult {
                    code: "(任务列表)".to_string(),
                    status: STATUS_ERROR.to_string(),
                    message: msg.clone(),
                    ..Default::default()
                });
                self.record(&a.uid, "成长任务", "(任务列表)", &msg, crate::records::RESULT_FAILED);
                return out;
            }
        };
        let by_code = index_tasks(before);
        if let Some(res) = self.accept_pending(a, &by_code).await {
            out.push(res);
        }
        for act in actions::ACTIONS {
            if cancel.is_cancelled() {
                out.push(ItemResult {
                    code: act.code.to_string(),
                    status: STATUS_SKIPPED.to_string(),
                    message: "已取消（宿主提前结束本轮）".to_string(),
                    ..Default::default()
                });
                break;
            }
            let item = self.run_action(cancel, a, act, by_code.get(act.code)).await;
            out.push(item);
            if !cancel.sleep(self.report_gap).await {
                break;
            }
        }
        self.summarize(a, &out);
        out
    }

    /// 跑某账号的**单个**任务（对应 Go `RunOne`）。
    ///
    /// ★ 与 [`Runner::run_all`] 复用同一套 `run_action` / `wait_progress` /
    /// `claim` 实现（重复实现两份是进度回读口径漂移的常见来源）。
    pub async fn run_one(&self, cancel: &Cancel, a: &Auth, code: &str) -> ItemResult {
        let Some(act) = action_for(code) else {
            return ItemResult {
                code: code.to_string(),
                status: STATUS_UNSUPPORTED.to_string(),
                message: "该任务需要客户端内的人工操作（无对应接口），无法自动完成；请在官方客户端按任务说明操作"
                    .to_string(),
                ..Default::default()
            };
        };
        let tasks = match self.up.list_growth_tasks(a).await {
            Ok(t) => t,
            Err(e) => {
                return ItemResult {
                    code: code.to_string(),
                    status: STATUS_ERROR.to_string(),
                    message: format!("拉取任务列表失败: {e}"),
                    ..Default::default()
                }
            }
        };
        let mut t = index_tasks(tasks).remove(code);
        let Some(cur) = t.as_ref() else {
            return ItemResult {
                code: code.to_string(),
                status: STATUS_SKIPPED.to_string(),
                message: "该账号没有此任务".to_string(),
                ..Default::default()
            };
        };
        if cur.needs_accept() {
            match self.accept_tasks(a, &[code.to_string()]).await {
                Err(e) => {
                    eprintln!(
                        "growtask: accept {code} uid={}: {e}（继续走行为链路）",
                        uid8(&a.uid)
                    );
                }
                Ok(_) => {
                    if let Some(fresh) = self.refetch(a, code).await {
                        t = Some(fresh);
                    }
                }
            }
        }
        let item = self.run_action(cancel, a, act, t.as_ref()).await;
        self.record(
            &a.uid,
            &growth_record_title(t.as_ref(), code),
            code,
            &item.message,
            result_for(&item.status),
        );
        item
    }

    /// 单项编排（对应 Go `runAction`）。
    async fn run_action(
        &self,
        cancel: &Cancel,
        a: &Auth,
        act: &Action,
        before: Option<&GrowthTask>,
    ) -> ItemResult {
        let mut item = ItemResult {
            code: act.code.to_string(),
            desc: act.desc.to_string(),
            needs_chat: act.needs_chat,
            ..Default::default()
        };
        let Some(before) = before else {
            item.status = STATUS_SKIPPED.to_string();
            item.message = "该账号没有此任务".to_string();
            return item;
        };
        item.title = before.title.clone();
        item.progress_before = before.progress_text();
        if before.claimed() {
            item.status = STATUS_SKIPPED.to_string();
            item.message = "该任务奖励已领取".to_string();
            return item;
        }
        if before.claimable() {
            // 已达标但没领过：补领奖即可，不必再跑一遍动作。
            item.status = STATUS_DONE.to_string();
            item.message = "进度已达标，直接领奖".to_string();
            item.progress_after = before.progress_text();
            self.claim(a, act.code, &mut item).await;
            return item;
        }
        if before.locked {
            // ★ 位置在 `claimable` **之后**：万一上游下发了「已达标但带锁」的
            // 矛盾组合，领奖是纯读奖励、不发行为事件，试一次没有代价；而发行为
            // 事件没有意义。
            //
            // ★ 判成 skipped 而不是 error：上游一切正常，是时间没到。实测
            //（2026-09-30，`Sequential_Tasks_2`）对 locked 任务走完整链路，上游
            // 返回 HTTP 200、不报任何错，动作函数也如实汇报「已上报事件」，于是
            // 整项被判成 done —— 而真实结果是事件被服务端**静默丢弃**、进度纹丝
            // 不动。报 error 会让用户以为程序或账号出了问题，而实际只需要等。
            item.status = STATUS_SKIPPED.to_string();
            item.message = format!(
                "{}，现在执行不会计分（上游会静默丢弃），已跳过",
                locked_text(&before.unlock_at)
            );
            return item;
        }

        match actions::run(act.kind, self, cancel, a, Some(before)).await {
            Err(e) => {
                item.status = STATUS_ERROR.to_string();
                item.message = e.to_string();
                return item;
            }
            // ★ 跳过不能算完成：`Skipped` 表示「按上游规则现在执行不计分」，
            // 记成 `done` 会让面板亮出绿色「完成」而进度纹丝不动（`black_cat`
            // 在夜猫窗口外就是这条路，2026-10-05 实测 15 个账号全中）。
            Ok(outcome) => {
                let status = status_for_outcome(&outcome);
                item.status = status.to_string();
                item.message = outcome.message().to_string();
                if status == STATUS_SKIPPED {
                    // 跳过不做事后回读：什么都不会变，白等一轮轮询。
                    return item;
                }
            }
        }

        // 回读验证：上报 200 ≠ 计分，用有界轮询等异步计分落定。
        if let Some(after) = self.wait_progress(cancel, a, act.code).await {
            item.progress_after = after.progress_text();
            if after.claimable() {
                self.claim(a, act.code, &mut item).await;
                return item;
            }
            // 无奖励 + 进度未推进：必须如实说明，不能把「上报成功」写成「已完成」。
            //
            // 触发场景是 `black_cat`（0 积分 0 能量）：实测在同一夜内用三种判据
            // 口径各做一次真实对话，进度都停在 1/3 不动 —— 而它的目标恰好是 3。
            // 最可能的原因是**按夜计数**（每夜最多计 1 次，需跨 3 夜），这是单夜
            // 实测无法证实也无法证伪的，故只陈述观察到的事实 + 标注推断。
            //
            // 之所以不直接判为失败：动作确实执行了（对话真的发了），判失败会让
            // 用户以为程序出错；而沉默不说又会让用户以为任务已完成。
            if !after.claimable()
                && !after.claimed()
                && before.credit == 0
                && before.energy == 0
                && after.current <= before.current
            {
                item.message.push_str(
                    "；但进度未推进（该任务无奖励，且实测同一夜内多次真实对话不累加，疑为按夜计数、需跨夜完成）",
                );
            }
        }
        item
    }

    /// 领奖并把结果写进 `item`（对应 Go `claim`）。
    ///
    /// 领奖失败**不**把整项判成 error：动作本身已经成功（进度确实推进了），把它
    /// 标成失败会让用户以为白做了。真实情况是「达标但领奖没成功」，用
    /// `claim_error` 单独表达，用户可在界面上手动重试。
    async fn claim(&self, a: &Auth, code: &str, item: &mut ItemResult) {
        match self.claim_reward(a, code).await {
            Err(e) => {
                item.claim_error = e.to_string();
                if is_growth_task_not_completed(&e) {
                    item.message
                        .push_str("；领奖时上游仍报未达标（计分可能还在落定），可稍后重试");
                } else {
                    item.message.push_str("；达标但领奖失败（可稍后重试）");
                }
            }
            Ok((credit, energy, true)) => {
                item.message.push_str("；奖励此前已领取");
                let _ = (credit, energy);
            }
            Ok((credit, energy, false)) => {
                item.claimed = true;
                item.credit = credit;
                item.energy = energy;
                item.message
                    .push_str(&format!("；已自动领奖 +{credit} 积分 +{energy} 能量"));
            }
        }
    }

    /// 按任务码分派领奖域（对应 Go `claimReward`）。
    ///
    /// ★ 小程序限定任务的领奖走聊天域（`chat_base`）+ mp 头；普通任务走 Web 域
    ///（`www.workbuddy.cn`）。用错域的表现是 HTTP 400 —— 与「任务未达标」是同
    /// 一个状态码，两者从响应上分不出来（见
    /// [`is_growth_task_not_completed`]）。换句话说：走错域的失败会被报成「计分
    /// 可能还在落定」，用户会一直重试，而任务永远领不到。故必须按码分派，不能靠
    /// 试错。
    async fn claim_reward(&self, a: &Auth, code: &str) -> crate::Result<(i64, i64, bool)> {
        if actions::is_mp_only(code) {
            self.up.claim_reward_mp(a, code).await
        } else {
            self.up.claim_growth_task(a, code).await
        }
    }

    /// 有界轮询等异步计分落定（对应 Go `waitProgress`）。
    ///
    /// 达标即返回（不空转满预算）；预算耗尽返回最后一次结果（可能仍未达标）——
    /// 返回 `None` 表示连查询都失败，此时调用方**不应**把进度写成某个猜测值。
    async fn wait_progress(&self, cancel: &Cancel, a: &Auth, code: &str) -> Option<GrowthTask> {
        let mut t = self.refetch(a, code).await;
        if t.as_ref()
            .is_none_or(|t| t.claimable() || t.claimed())
        {
            return t;
        }
        for _ in 1..self.poll_attempts {
            if !cancel.sleep(self.poll_gap).await {
                return t;
            }
            let Some(next) = self.refetch(a, code).await else {
                return t;
            };
            let settled = next.claimable() || next.claimed();
            t = Some(next);
            if settled {
                return t;
            }
        }
        t
    }

    /// 重新拉一次任务列表并取出指定任务（对应 Go `refetch`）。
    pub(crate) async fn refetch(&self, a: &Auth, code: &str) -> Option<GrowthTask> {
        match self.up.list_growth_tasks(a).await {
            Ok(tasks) => index_tasks(tasks).remove(code),
            Err(e) => {
                eprintln!("growtask: refetch {code} uid={}: {e}", uid8(&a.uid));
                None
            }
        }
    }

    /// 批量报名尚未参加的任务（对应 Go `acceptPending`）。
    async fn accept_pending(&self, a: &Auth, by_code: &HashMap<String, GrowthTask>) -> Option<ItemResult> {
        let codes: Vec<String> = by_code
            .values()
            .filter(|t| t.needs_accept())
            .map(|t| t.code.clone())
            .collect();
        if codes.is_empty() {
            return None;
        }
        let mut res = ItemResult {
            code: "(批量报名)".to_string(),
            desc: "报名尚未参加的任务".to_string(),
            ..Default::default()
        };
        match self.accept_tasks(a, &codes).await {
            Err(e) => {
                // 报名失败不阻塞后续动作：行为事件才是进度的唯一判据，而报名只是
                // 让服务端开始跟踪进度。若真的因为未报名而不计分，后续的回读会如
                // 实显示 0/N —— 结论仍然诚实。
                res.status = STATUS_ERROR.to_string();
                res.message = format!("报名 {} 个任务失败（不阻塞后续执行）: {e}", codes.len());
            }
            Ok(results) => {
                res.status = STATUS_DONE.to_string();
                let news = results
                    .iter()
                    .filter(|r| r.status == TASK_ACCEPT_ACCEPTED)
                    .count();
                res.message = format!(
                    "已报名 {} 个任务（其中 {} 个为本次新报名）",
                    codes.len(),
                    news
                );
            }
        }
        Some(res)
    }

    /// 按码分组报名（对应 Go `acceptTasks`）。
    ///
    /// ★ 小程序限定任务（[`actions::is_mp_only`]）的报名要求
    /// `X-Client-Platform: miniprogram`。缺该头时上游返回 **HTTP 200 + code=0**，
    /// 但逐条结果是 `{"status":"error","message":"task not found"}` —— 报名根本
    /// 没发生，而只看 HTTP 状态（甚至只看 code=0）会以为成功了。
    ///
    /// ★ 为什么**不给整批统一加 mp 头**：[`Runner::run_all`] 的批量报名把当轮
    /// **所有**未报名任务并成一次请求，通常是混合批次（普通 + 小程序）。给整批加
    /// mp 头是另一种输入，其行为**未经验证**，所以按码分组、分别请求，再合并结果。
    ///
    /// ★ 失败时返回**已受理的部分结果**而非丢弃：混合批次里一半成功一半失败是
    /// 常见形态，调用方据此能给出诚实的汇报，而不是把已成功的那些也说成失败。
    async fn accept_tasks(
        &self,
        a: &Auth,
        codes: &[String],
    ) -> crate::Result<Vec<crate::upstream::tasks::AcceptResult>> {
        let (mp, plain): (Vec<String>, Vec<String>) = codes
            .iter()
            .cloned()
            .partition(|c| actions::is_mp_only(c));
        let mut out = Vec::new();
        if !plain.is_empty() {
            out.extend(self.up.accept_growth_tasks(a, &plain).await?);
        }
        if !mp.is_empty() {
            out.extend(self.up.accept_tasks_mp(a, &mp).await?);
        }
        Ok(out)
    }

    /// 整轮汇总：统计 + 写记录（对应 Go `summarize`）。
    fn summarize(&self, a: &Auth, items: &[ItemResult]) {
        let (mut done, mut skipped, mut failed, mut claimed) = (0usize, 0usize, 0usize, 0usize);
        let (mut credit, mut energy) = (0i64, 0i64);
        for it in items {
            match it.status.as_str() {
                STATUS_DONE => done += 1,
                STATUS_SKIPPED => skipped += 1,
                STATUS_ERROR => failed += 1,
                _ => {}
            }
            if it.claimed {
                claimed += 1;
                credit += it.credit;
                energy += it.energy;
            }
        }
        let detail = format!(
            "执行 {done} 项（跳过 {skipped}、失败 {failed}），领奖 {claimed} 项 +{credit} 积分 +{energy} 能量"
        );
        eprintln!("growtask: uid={} {detail}", uid8(&a.uid));
        // 一轮什么都没做（全部跳过）不值得写记录：那是「无变化」，天天写只会刷屏。
        if done == 0 && failed == 0 {
            return;
        }
        let result = if failed > 0 {
            crate::records::RESULT_FAILED
        } else {
            crate::records::RESULT_SUCCESS
        };
        self.record(&a.uid, RECORDS_TITLE, "", &detail, result);
    }

    /// 写一条「每天最多一条」的账号记录（对应 Go `record`）。
    ///
    /// ★ 2026-09-22 修：所有者反馈「任务执行记录我看也没有」。查证后发现记录
    /// **一直在写**，但 18 项成长任务此前共用标题「成长任务」，而
    /// [`Recorder::task_daily`] 的去重键是「账号 + 标题 + 结果，每天一条」⇒ 同一天
    /// 跑多项只留一条，用户看不到具体跑了什么。**粒度过粗被误读成「没有记录」。**
    /// 故单项记录一律用任务标题（回落任务码）。
    ///
    /// ★ 用 daily 而非每次写：用户连点「一键完成」会产生多轮，每轮都写会让记录区
    /// 被同一件事刷满。
    fn record(&self, uid: &str, title: &str, code: &str, detail: &str, result: &str) {
        let Some(rec) = self.rec.as_ref() else {
            return;
        };
        let mut title = title.trim().to_string();
        if title.is_empty() {
            title = code.trim().to_string();
        }
        if title.is_empty() {
            title = RECORDS_TITLE.to_string();
        }
        rec.task_daily(uid, &title, result, detail);
    }
}

/// 记录标题：上游标题优先，回落**任务码**（对应 Go `growthRecordTitle`）。
///
/// ★ 回落用任务码而不是笼统的「成长任务」：任务码虽不漂亮，但至少能区分是哪一项。
fn growth_record_title(t: Option<&GrowthTask>, code: &str) -> String {
    match t {
        None => code.to_string(),
        Some(t) => {
            let s = t.title.trim();
            if s.is_empty() {
                code.to_string()
            } else {
                s.to_string()
            }
        }
    }
}

/// 动作结局 → 条目状态。
///
/// ★ 抽成纯函数是为了可单测：`Skipped` 必须记 `skipped` 而不是 `done`，这个
/// 映射一旦写错就会出现「面板显示完成、进度没动」的假成功 —— 实测（2026-10-05）
/// 15 个账号的 `参与「夜猫子」夜间折扣活动` 全部落成 `success` +「当前不在夜猫
/// 时段…」，就是这条映射缺失造成的。
fn status_for_outcome(outcome: &actions::Outcome) -> &'static str {
    if outcome.is_skipped() {
        STATUS_SKIPPED
    } else {
        STATUS_DONE
    }
}

/// 状态 → 记录结果（对应 Go `resultFor`）。
fn result_for(status: &str) -> &'static str {
    if status == STATUS_ERROR {
        crate::records::RESULT_FAILED
    } else {
        crate::records::RESULT_SUCCESS
    }
}

/// 任务码 → 任务（对应 Go `indexTasks`）。空码不入表。
fn index_tasks(tasks: Vec<GrowthTask>) -> HashMap<String, GrowthTask> {
    let mut m = HashMap::with_capacity(tasks.len());
    for t in tasks {
        if !t.code.is_empty() {
            m.insert(t.code.clone(), t);
        }
    }
    m
}

// ---------------------------------------------------------------------------
// 多账号
// ---------------------------------------------------------------------------

impl Runner {
    /// 跑一批账号（对应 Go `RunAllAccounts`）。
    ///
    /// 账号之间可并发（默认 1，串行），账号内串行。单账号失败不影响其他账号
    /// —— 一个账号登不上不该让整批停摆。返回顺序与传入顺序一致（即使内部并发
    /// 执行），宿主可直接按序渲染。
    pub async fn run_all_accounts(&self, cancel: &Cancel, opts: &FleetOptions) -> Vec<AccountResult> {
        let mut conc = opts.concurrency;
        if conc == 0 {
            conc = 1;
        }
        if conc > MAX_FLEET_CONCURRENCY {
            conc = MAX_FLEET_CONCURRENCY;
        }
        let mut results = vec![AccountResult::default(); opts.accounts.len()];
        // ★ 把 `Auth` **拷进** future，而不是让它借用 `opts`：借用版会让返回的
        // future 类型带上调用点的生命周期参数，于是整个函数的 future 不再是
        // 「对任意生命周期都成立」。平时看不出来，但 `/tasks/growth` 的 handler
        // 一注册就会炸成 `error: implementation of FnOnce is not general enough`
        // （错误位置指向路由那行，离真凶很远）。
        let jobs: Vec<(usize, Auth)> = opts.accounts.iter().cloned().enumerate().collect();
        let mut stream = stream::iter(jobs)
            .map(|(i, a)| async move { (i, self.run_account(cancel, &a, opts.include_intl).await) })
            .buffer_unordered(conc);
        while let Some((i, res)) = stream.next().await {
            results[i] = res;
        }
        results
    }

    /// 单账号的整轮（对应 Go `runAccount`）。
    async fn run_account(&self, cancel: &Cancel, a: &Auth, include_intl: bool) -> AccountResult {
        let mut res = AccountResult {
            uid: a.uid.clone(),
            // ★ 用 `is_intl()` 而不是 `Region::as_str()`：后者给的是 `"intl"`，
            // 而 Go `Realm()` 给的是 `"global"`。
            realm: if a.is_intl() { "global" } else { "cn" }.to_string(),
            ..Default::default()
        };
        if a.access_token.is_empty() {
            res.skipped = true;
            res.skip_reason = "账号无访问令牌（需重新登录）".to_string();
            return res;
        }
        if !include_intl && a.is_intl() {
            res.skipped = true;
            res.skip_reason = "国际版账号的成长任务体系不同，已跳过".to_string();
            return res;
        }
        // ★ RAII 守卫：future 被 drop（客户端断开）时也会解锁。手动 unlock 写在
        // await 之后会让该账号永久卡死，详见 AccountGuard 的文档。
        let Some(_guard) = self.try_lock(&a.uid) else {
            res.skipped = true;
            res.skip_reason = ERR_ACCOUNT_BUSY.to_string();
            return res;
        };
        let items = self.run_all(cancel, a).await;
        for it in &items {
            if it.claimed {
                res.claimed += 1;
                res.credit += it.credit;
                res.energy += it.energy;
            }
        }
        if items.len() == 1 && items[0].code == "(任务列表)" && items[0].status == STATUS_ERROR {
            res.error = items[0].message.clone();
        }
        res.items = items;
        res
    }
}

/// 汇总一批账号结果（对应 Go `Summarize`）。
pub fn summarize(results: &[AccountResult]) -> FleetSummary {
    let mut s = FleetSummary::default();
    for r in results {
        if r.skipped {
            s.skipped += 1;
            continue;
        }
        s.accounts += 1;
        s.claimed += r.claimed;
        s.credit += r.credit;
        s.energy += r.energy;
        if !r.error.is_empty() {
            s.failed += 1;
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::client::ClientConfig;
    use crate::upstream::tasks::{
        TASK_ACCEPT_ACCEPTED, TASK_ACCEPT_IN_PROGRESS, TASK_ACCEPT_NOT_ACCEPTED,
    };

    fn client() -> Client {
        Client::new(ClientConfig::default()).expect("默认配置应能构造客户端")
    }

    fn runner() -> Runner {
        Runner::new(Options::new(client()))
    }

    fn task(code: &str, status: &str) -> GrowthTask {
        GrowthTask {
            code: code.into(),
            accept_status: status.into(),
            ..Default::default()
        }
    }

    // ── Options 的缺省填充 ──

    /// 五个可调项为 0 时一律回落默认值（Go 侧是 `<= 0`，Rust 用 `== 0` /
    /// `is_zero()` —— `Duration` 没有负数，语义等价）。
    #[test]
    fn zero_options_fall_back_to_defaults() {
        let r = runner();
        assert_eq!(r.poll_attempts, DEFAULT_POLL_ATTEMPTS);
        assert_eq!(r.poll_gap, DEFAULT_POLL_GAP);
        assert_eq!(r.report_gap, DEFAULT_REPORT_GAP);
        assert_eq!(r.chat_gap, DEFAULT_CHAT_GAP);
        assert_eq!(r.mp_chat_gap, DEFAULT_MP_CHAT_GAP);
        assert!(r.records().is_none(), "默认不挂记录器");
    }

    /// 显式给了值就必须用显式的（否则「测试里把 gap 压到 0 让它跑快」会
    /// 静默失效，测试变成慢速真跑）。
    #[test]
    fn explicit_options_are_honoured() {
        let mut o = Options::new(client());
        o.poll_attempts = 1;
        o.poll_gap = Duration::from_millis(1);
        o.report_gap = Duration::from_millis(2);
        o.chat_gap = Duration::from_millis(3);
        o.mp_chat_gap = Duration::from_millis(4);
        let r = Runner::new(o);
        assert_eq!(r.poll_attempts, 1);
        assert_eq!(r.poll_gap, Duration::from_millis(1));
        assert_eq!(r.report_gap, Duration::from_millis(2));
        assert_eq!(r.chat_gap, Duration::from_millis(3));
        assert_eq!(r.mp_chat_gap, Duration::from_millis(4));
    }

    /// 默认值本身是有依据的，写死在用例里防止被「顺手调小」。
    #[test]
    fn default_gaps_match_the_measured_requirements() {
        assert_eq!(DEFAULT_POLL_ATTEMPTS, 4);
        assert_eq!(DEFAULT_POLL_GAP, Duration::from_secs(3));
        assert_eq!(DEFAULT_REPORT_GAP, Duration::from_millis(1050));
        assert_eq!(DEFAULT_CHAT_GAP, Duration::from_secs(6));
        // ★ 45 秒是反作弊阈值，不是保守取值 —— 调小会让整轮上报作废
        assert_eq!(DEFAULT_MP_CHAT_GAP, Duration::from_secs(45));
        assert_eq!(RECORDS_TITLE, "成长任务");
    }

    // ── 账号级互斥 ──

    /// 同账号第二次 `lock` 必须失败（**不排队**：排队会让第二次点击在几十秒后
    /// 突然执行，用户早已离开页面）。
    #[test]
    fn lock_rejects_second_holder() {
        let r = runner();
        assert!(r.lock("u1"), "首次应成功");
        assert!(!r.lock("u1"), "同账号第二次必须被拒（不排队）");
        r.unlock("u1");
        assert!(r.lock("u1"), "解锁后应能再取");
    }

    /// 不同账号互不影响（账号**间**可并发，账号**内**串行）。
    #[test]
    fn lock_is_per_account() {
        let r = runner();
        assert!(r.lock("u1"));
        assert!(r.lock("u2"), "不同账号不该互相阻塞");
        assert!(!r.lock("u1"));
        r.unlock("u1");
        assert!(r.lock("u1"));
        assert!(!r.lock("u2"), "u2 仍被持有");
    }

    /// `unlock` 对没持有的账号是安全的空操作（重复解锁不该 panic 或误放别人）。
    #[test]
    fn unlock_is_idempotent_and_scoped() {
        let r = runner();
        r.unlock("never-locked");
        assert!(r.lock("u1"));
        r.unlock("u2"); // 不该动到 u1
        assert!(!r.lock("u1"), "u1 仍应被持有");
        r.unlock("u1");
        r.unlock("u1");
        assert!(r.lock("u1"));
    }

    // ── 账号锁的 RAII 守卫 ──

    /// ★ 回归测试：守卫**离开作用域**即解锁（对应 Go 的 `defer r.Unlock`）。
    ///
    /// 这是 2026-10-01 实机复现的那个 parity 缺陷的钉子：手动 `unlock` 写在
    /// `await` 之后时，future 被 drop（客户端断开）就永不执行，该账号会**永久**
    /// 卡在忙碌态。守卫把解锁挂在 `Drop` 上，覆盖包括提前 return 与 drop 在内
    /// 的**所有**路径。
    #[test]
    fn guard_unlocks_on_drop() {
        let r = runner();
        {
            let g = r.try_lock("u1").expect("首次占用应成功");
            assert_eq!(g.uid(), "u1");
            // 守卫存活期间别人拿不到
            assert!(r.try_lock("u1").is_none(), "占用中不该再拿到");
            assert!(r.try_lock("u2").is_some(), "别的账号不受影响");
        }
        // 作用域结束 ⇒ Drop ⇒ 可再次占用
        assert!(r.try_lock("u1").is_some(), "守卫 drop 后应已解锁");
    }

    /// 占用失败 ⇒ `None`，且**不该**留下半占用的状态（否则一次冲突就永久锁死）。
    #[test]
    fn try_lock_returns_none_when_busy_without_side_effect() {
        let r = runner();
        let _held = r.try_lock("u1").expect("首次占用应成功");
        assert!(r.try_lock("u1").is_none());
        assert!(r.try_lock("u1").is_none(), "失败不该改变占用状态");
        // 原守卫仍有效
        assert!(r.try_lock("u1").is_none());
    }

    /// 守卫被 drop 后重复解锁是安全的（`Drop` + 手动 `unlock` 不会互相破坏）。
    #[test]
    fn guard_drop_is_idempotent_with_manual_unlock() {
        let r = runner();
        {
            let _g = r.try_lock("u1").expect("首次占用应成功");
            r.unlock("u1"); // 提前手动解锁（等价于 Go 里 defer 之外的显式解锁）
        }
        let held = r.try_lock("u1").expect("应能重新占用");
        assert!(r.try_lock("u1").is_none(), "只该有一个持有者");
        drop(held);
        assert!(r.try_lock("u1").is_some());
    }

    /// ★ 守卫**必须被绑定**才有效：`r.try_lock(uid);` 这样的临时值会在语句结束
    /// 时立刻 drop ⇒ 互斥**完全失效**。与 `MutexGuard` 同款语义，调用方一律写
    /// `let Some(_guard) = ... else { ... }`（`_guard` 是具名绑定，不是 `_` 通配）。
    ///
    /// 本测试钉住这个语义，避免有人误以为「拿到就锁住了」。
    #[test]
    fn unbound_guard_releases_immediately() {
        let r = runner();
        // 反例：丢弃返回值 ⇒ 立刻解锁
        assert!(r.try_lock("u1").is_some());
        assert!(
            r.try_lock("u1").is_some(),
            "未绑定的守卫应在语句结束时就已解锁"
        );
        // 正例：绑定后互斥生效
        let _g = r.try_lock("u1").expect("应能占用");
        assert!(r.try_lock("u1").is_none());
    }

    // ── index_tasks ──

    /// 空码不入表 —— 否则 `refetch("")` 之类会命中一个无意义条目。
    #[test]
    fn index_tasks_skips_empty_codes() {
        let m = index_tasks(vec![
            task("a", TASK_ACCEPT_ACCEPTED),
            task("", TASK_ACCEPT_ACCEPTED),
            task("b", TASK_ACCEPT_IN_PROGRESS),
        ]);
        assert_eq!(m.len(), 2);
        assert!(m.contains_key("a"));
        assert!(m.contains_key("b"));
        assert!(!m.contains_key(""));
    }

    /// 同码后者覆盖前者（上游理论上不该重复下发；真重复时取最后一条与
    /// Go 的 `for ... m[t.Code] = t` 一致）。
    #[test]
    fn index_tasks_last_wins() {
        let m = index_tasks(vec![
            task("a", TASK_ACCEPT_NOT_ACCEPTED),
            task("a", TASK_ACCEPT_ACCEPTED),
        ]);
        assert_eq!(m.len(), 1);
        assert_eq!(m["a"].accept_status, TASK_ACCEPT_ACCEPTED);
    }

    #[test]
    fn index_tasks_on_empty_input() {
        assert!(index_tasks(Vec::new()).is_empty());
    }

    // ── result_for / growth_record_title ──

    /// ★ 动作结局 → 条目状态：`Skipped` 必须落成 `skipped`，绝不能是 `done`。
    ///
    /// 这条映射是 ⑬「面板显示完成、进度没动」的修复核心：`black_cat` 在夜猫
    /// 窗口外返回 `Skipped`，若这里仍返回 `done`，用户会以为任务做完了。
    #[test]
    fn outcome_maps_skipped_to_skipped_not_done() {
        use actions::Outcome;
        let skip = Outcome::Skipped("当前不在夜猫时段".to_string());
        assert_eq!(status_for_outcome(&skip), STATUS_SKIPPED);
        assert_ne!(status_for_outcome(&skip), STATUS_DONE);
        let done = Outcome::Done("已完成".to_string());
        assert_eq!(status_for_outcome(&done), STATUS_DONE);
        // 跳过也必须是「成功」语义（上游一切正常，只是时间没到），
        // 否则账号记录里会出现一片红叉。
        assert_eq!(result_for(status_for_outcome(&skip)), crate::records::RESULT_SUCCESS);
        // 说明文案两种结局都要原样带出
        assert_eq!(skip.message(), "当前不在夜猫时段");
        assert_eq!(done.message(), "已完成");
        assert!(skip.is_skipped());
        assert!(!done.is_skipped());
    }

    /// 只有 `error` 记成失败；`skipped` / `unsupported` 都是成功语义（它们不是
    /// 故障，记成失败会让「账号记录」里全是红叉）。
    #[test]
    fn result_for_maps_only_error_to_failed() {
        assert_eq!(result_for(STATUS_ERROR), crate::records::RESULT_FAILED);
        for s in [STATUS_DONE, STATUS_SKIPPED, STATUS_UNSUPPORTED] {
            assert_eq!(result_for(s), crate::records::RESULT_SUCCESS, "{s}");
        }
        // 未知状态按成功处理（宁可少报故障，也不要把正常结果标红）
        assert_eq!(result_for("weird"), crate::records::RESULT_SUCCESS);
    }

    /// 记录标题优先用上游任务标题，回落任务码 —— 必须**逐任务**不同，否则同一天
    /// 多次运行会在 `(accountId, title, result, day)` 上去重成一条。
    #[test]
    fn growth_record_title_falls_back_to_code() {
        // 无任务 ⇒ 任务码
        assert_eq!(growth_record_title(None, "chat_5"), "chat_5");

        // 有任务但标题为空/纯空白 ⇒ 任务码
        let mut t = task("chat_5", TASK_ACCEPT_ACCEPTED);
        assert_eq!(growth_record_title(Some(&t), "chat_5"), "chat_5");
        t.title = "   ".into();
        assert_eq!(growth_record_title(Some(&t), "chat_5"), "chat_5");

        // 有标题 ⇒ 用标题，且两侧空白被裁掉
        t.title = "  每日对话  ".into();
        assert_eq!(growth_record_title(Some(&t), "chat_5"), "每日对话");

        // ★ 不同任务码在都没标题时必须给出不同的记录标题（否则同一天去重成一条）
        let a = growth_record_title(None, "chat_5");
        let b = growth_record_title(None, "first_buddy");
        assert_ne!(a, b, "无标题时不同任务必须有不同的记录标题");
    }

    // ── summarize ──

    /// ★ 跳过的账号**不计入** `accounts` —— 它表示「真正跑过的账号数」，
    /// 把跳过算进去会让界面上「3 个账号完成」实际只跑了 1 个。
    #[test]
    fn summarize_excludes_skipped_from_accounts() {
        let s = summarize(&[
            AccountResult {
                uid: "u1".into(),
                skipped: true,
                skip_reason: "账号无访问令牌（需重新登录）".into(),
                ..Default::default()
            },
            AccountResult {
                uid: "u2".into(),
                claimed: 2,
                credit: 30,
                energy: 12,
                ..Default::default()
            },
        ]);
        assert_eq!(s.accounts, 1, "跳过的账号不该计入 accounts");
        assert_eq!(s.skipped, 1);
        assert_eq!(s.claimed, 2);
        assert_eq!(s.credit, 30);
        assert_eq!(s.energy, 12);
        assert_eq!(s.failed, 0);
    }

    /// 跳过的账号即便带着 claimed/credit（不该有，但结构上可能）也不累加。
    #[test]
    fn summarize_ignores_values_of_skipped_accounts() {
        let s = summarize(&[AccountResult {
            uid: "u1".into(),
            skipped: true,
            claimed: 9,
            credit: 999,
            energy: 999,
            ..Default::default()
        }]);
        assert_eq!(s.accounts, 0);
        assert_eq!(s.skipped, 1);
        assert_eq!(s.claimed, 0);
        assert_eq!(s.credit, 0);
        assert_eq!(s.energy, 0);
    }

    /// 有整轮错误的账号计入 `failed` 但仍计入 `accounts`（它确实跑过）。
    #[test]
    fn summarize_counts_failed_but_still_accounts() {
        let s = summarize(&[AccountResult {
            uid: "u1".into(),
            error: "拉取任务列表失败: 上游 500".into(),
            ..Default::default()
        }]);
        assert_eq!(s.accounts, 1);
        assert_eq!(s.failed, 1);
        assert_eq!(s.skipped, 0);
    }

    #[test]
    fn summarize_on_empty_input() {
        let s = summarize(&[]);
        assert_eq!(s, FleetSummary::default());
        assert_eq!(s.accounts, 0);
        assert_eq!(s.skipped, 0);
        assert_eq!(s.failed, 0);
    }

    // ── FleetOptions ──

    /// 默认并发 1（串行）、不含国际版 —— 国际版的动作对本模块无意义。
    #[test]
    fn fleet_options_default_is_serial_and_cn_only() {
        let o = FleetOptions::default();
        assert_eq!(o.concurrency, 1);
        assert!(!o.include_intl);
        assert!(o.accounts.is_empty());
        assert_eq!(MAX_FLEET_CONCURRENCY, 3, "上限刻意压到 3");
    }

    // ── JSON 形状（宿主按这些名字渲染，改一个界面就空一块）──

    /// `ItemResult` 的空串/false/0 全部省略（Go 的 `omitempty`）。
    #[test]
    fn item_result_omits_empty_fields() {
        let v = ItemResult {
            code: "chat_5".into(),
            status: STATUS_DONE.into(),
            message: "已上报".into(),
            ..Default::default()
        };
        let j = serde_json::to_value(&v).unwrap();
        let obj = j.as_object().unwrap();
        for k in ["task_code", "status", "message"] {
            assert!(obj.contains_key(k), "缺字段 {k}: {j}");
        }
        for k in [
            "title",
            "desc",
            "progress_before",
            "progress_after",
            "claimed",
            "credit",
            "energy",
            "claim_error",
            "needs_chat",
        ] {
            assert!(!obj.contains_key(k), "字段 {k} 不该出现: {j}");
        }
    }

    /// 填满时字段名逐个核对，含 `task_code`（**没有** `code` 别名）。
    #[test]
    fn item_result_field_names_match_go() {
        let v = ItemResult {
            code: "chat_5".into(),
            title: "每日对话".into(),
            desc: "补报对话活跃事件至达标".into(),
            status: STATUS_DONE.into(),
            message: "已自动领奖".into(),
            progress_before: "0/5".into(),
            progress_after: "5/5".into(),
            claimed: true,
            credit: 10,
            energy: 5,
            claim_error: "".into(),
            needs_chat: true,
        };
        let j = serde_json::to_value(&v).unwrap();
        assert_eq!(j["task_code"], "chat_5");
        assert_eq!(j["progress_before"], "0/5");
        assert_eq!(j["progress_after"], "5/5");
        assert_eq!(j["claimed"], true);
        assert_eq!(j["needs_chat"], true);
        assert_eq!(j["credit"], 10);
        assert_eq!(j["energy"], 5);
        assert!(j.get("code").is_none(), "字段名是 task_code");
        assert!(j.get("claim_error").is_none(), "空 claim_error 应省略");
    }

    /// `AccountResult` 的空字段省略，`uid` / `realm` / 三个计数**恒在**
    ///（Go 侧无 omitempty）。
    #[test]
    fn account_result_omits_empty_fields() {
        let v = AccountResult {
            uid: "u1".into(),
            realm: "cn".into(),
            ..Default::default()
        };
        let j = serde_json::to_value(&v).unwrap();
        let obj = j.as_object().unwrap();
        for k in ["uid", "realm", "claimed", "credit", "energy"] {
            assert!(obj.contains_key(k), "缺字段 {k}: {j}");
        }
        for k in ["skipped", "skip_reason", "items", "error"] {
            assert!(!obj.contains_key(k), "字段 {k} 不该出现: {j}");
        }
    }

    /// `FleetSummary` 六个字段**全部无 omitempty** —— 界面按固定列渲染，
    /// 少一个键就会显示成空白而不是 0。
    #[test]
    fn fleet_summary_always_serializes_every_field() {
        let j = serde_json::to_value(FleetSummary::default()).unwrap();
        let obj = j.as_object().unwrap();
        for k in ["accounts", "skipped", "claimed", "credit", "energy", "failed"] {
            assert!(obj.contains_key(k), "缺字段 {k}: {j}");
        }
        assert_eq!(obj.len(), 6);
    }

    /// 两个结构都能往返（宿主侧的 TS 类型从 JSON 推）。
    #[test]
    fn results_roundtrip() {
        let v = ItemResult {
            code: "chat_5".into(),
            title: "每日对话".into(),
            status: STATUS_DONE.into(),
            message: "完成".into(),
            claimed: true,
            credit: 10,
            ..Default::default()
        };
        let s = serde_json::to_string(&v).unwrap();
        assert_eq!(serde_json::from_str::<ItemResult>(&s).unwrap(), v);

        let a = AccountResult {
            uid: "u1".into(),
            realm: "global".into(),
            items: vec![v],
            claimed: 1,
            credit: 10,
            ..Default::default()
        };
        let s = serde_json::to_string(&a).unwrap();
        assert_eq!(serde_json::from_str::<AccountResult>(&s).unwrap(), a);
    }

    /// 缺字段全部走默认（`#[serde(default)]`）。
    #[test]
    fn results_deserialize_from_minimal_objects() {
        let a: AccountResult = serde_json::from_str(r#"{"uid":"u1"}"#).unwrap();
        assert_eq!(a.uid, "u1");
        assert_eq!(a.realm, "");
        assert!(!a.skipped);
        assert!(a.items.is_empty());

        let i: ItemResult = serde_json::from_str(r#"{"task_code":"c"}"#).unwrap();
        assert_eq!(i.code, "c");
        assert_eq!(i.status, "");
        assert!(!i.claimed);
    }

    // ── 状态常量 ──

    /// 四个状态字面值（宿主与界面按它们分支）。
    #[test]
    fn status_constants_are_stable() {
        assert_eq!(STATUS_DONE, "done");
        assert_eq!(STATUS_SKIPPED, "skipped");
        assert_eq!(STATUS_ERROR, "error");
        assert_eq!(STATUS_UNSUPPORTED, "unsupported");
        // ★ 忙碌文案暴露成常量，宿主据此可回 409 与「执行失败」区分
        assert_eq!(ERR_ACCOUNT_BUSY, "该账号有任务动作正在执行中");
    }
}
