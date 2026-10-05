//! 任务中心的**查询**能力（对应 Go `internal/growtask/tasks.go`）。
//!
//! # 与 [`super::runner`] 的分工
//!
//! - `runner.rs` —— 单账号的动作编排（`run_all` / `run_one`）；
//! - `tasks.rs`  —— 只读查询（供界面「任务中心」列表）。
//!
//! 查询与执行分开是有意的：界面刷新列表**不能**有任何副作用。若把「列任务」实现成
//! 「顺手报名」，用户每刷新一次页面就会产生写操作，这既违反直觉也难以排查
//!（「我没点任何按钮，怎么账号状态变了」）。

use crate::auth::Auth;
use crate::upstream::tasks::GrowthTask;

use super::actions::{action_for, unsupported_hint};
use super::runner::Runner;

// ---------------------------------------------------------------------------
// 展示状态
// ---------------------------------------------------------------------------

/// 奖励已领取。
pub const VIEW_CLAIMED: &str = "claimed";
/// 进度已达标、待领取。
pub const VIEW_CLAIMABLE: &str = "claimable";
/// 已参与且有进度但未达标。
pub const VIEW_IN_PROGRESS: &str = "in_progress";
/// 已报名但尚无进度。
pub const VIEW_ACCEPTED: &str = "accepted";
/// 尚未报名（可自动报名）。
pub const VIEW_NOT_ACCEPTED: &str = "not_accepted";
/// 需人工操作的任务。
pub const VIEW_UNSUPPORTED: &str = "unsupported";
/// 上游标记未解锁。
pub const VIEW_LOCKED: &str = "locked";

/// 国际版独有的 5 个任务码 —— **不下发给界面**（删除，不是置灰）。
///
/// # 为什么是「删掉」而不是「标成需人工」（2026-09-25 实测结论）
///
/// 这 5 项是国际版（workbuddy.ai）growth 域返回的**静态目录**：7 个国际版账号 ×
/// 5 项 = 35 项，**每一项都恒为 `available`**，且响应里没有 `accept_status` /
/// `reward_credit` / `reward_energy` / `has_reward` / `progress` 字段 —— 也就是说
/// **没有任何状态机可以推进**，不存在「做完了」这个状态。三条独立证据：
///
/// 1. 目录是静态的：任何国际版账号、任何时刻读到的都是同样 5 项 `available`；
/// 2. 真实遥测事件无效：按客户端 `EVENT_NAME_MAPPING` 的口径，把全部 5 个事件码
///    （`chat_request_send` / `skill_installed` / `wechat_linked` / `expert_summoned`
///    / `template_used`，带完整业务字段）POST 到 `/v2/report`，全部 HTTP 200
///    `{"code":0,"msg":"OK"}`，回读任务清单**零变化**；
/// 3. 官方客户端自己也不走接口：国际版客户端把用户**交给 growth-center 网页**
///    （Cookie/会话通道），网关够不到那条通道。
///
/// 结论：这 5 项在本工具里**只能**渲染成「需在客户端手动完成」，对用户是纯噪音。
///
/// ⚠ 与 `Expert_Philanthropy` / `workstation_expert` 的区别（那两个**保留**）：
/// 它们是**国服**任务，有真实的报名与进度状态，只是动作需人工（捐款回执 /
/// 桌面端换血对话），因此仍列出并给出具体的置灰原因。删掉的只有这 5 个「点了也没有
/// 状态可变」的。
///
/// ⚠ 国际版的任务码 ≠ 遥测事件码。上面括号里列的是**客户端遥测**事件码，与任务码
/// 恰好有 4 个同名（`first_chat` 对应的事件码是 `chat_request_send`，它根本不在
/// `EVENT_NAME_MAPPING` 里）—— 同名是巧合，不能当成「任务码即事件码」的依据。
const INTL_ONLY_CODES: [&str; 5] = [
    "first_chat",
    "skill_installed",
    "wechat_linked",
    "expert_summoned",
    "template_used",
];

// ---------------------------------------------------------------------------
// 视图结构（JSON 字段名与 Go 逐字一致）
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

/// 「任务中心」列表项：查询结果，可安全序列化给界面（对应 Go `TaskView`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct TaskView {
    #[serde(rename = "task_code")]
    pub code: String,
    #[serde(skip_serializing_if = "is_empty_string")]
    pub title: String,
    /// 客户端操作指引。
    #[serde(skip_serializing_if = "is_empty_string")]
    pub description: String,
    /// 达成条件。
    #[serde(skip_serializing_if = "is_empty_string")]
    pub task_desc: String,
    #[serde(skip_serializing_if = "is_zero_i64")]
    pub credit: i64,
    #[serde(skip_serializing_if = "is_zero_i64")]
    pub energy: i64,

    /// 展示状态（见 `VIEW_*` 常量）。
    pub status: String,
    /// 状态的中文说明，界面可直接显示。
    pub status_text: String,

    #[serde(skip_serializing_if = "is_empty_string")]
    pub accept_status: String,
    /// 上游是否下发了进度。未报名时为 `false`。
    pub has_progress: bool,
    pub current: i64,
    pub target: i64,
    /// `"3/5"` 形式，界面直接显示。
    pub progress: String,

    /// 是否可领取奖励（本地推算）。
    pub claimable: bool,
    pub claimed: bool,

    /// 上游标记为尚未解锁（此时**不可执行**，见 `unlock_at`）。
    ///
    /// 必须透出给界面：锁定与「需人工完成」是两种不同的等待，界面把它们混在一起
    /// 显示，用户就分不清「等时间到」与「得自己去点」。
    #[serde(skip_serializing_if = "is_false")]
    pub locked: bool,
    /// 解锁时刻（RFC3339）。只在 `locked` 为真时非空。
    #[serde(skip_serializing_if = "is_empty_string")]
    pub unlock_at: String,

    /// 是否可一键完成。
    pub automatable: bool,
    /// 完成它是否需要消耗真实对话（界面据此提示资源消耗）。
    #[serde(skip_serializing_if = "is_false")]
    pub needs_chat: bool,
    /// 一键完成会做什么。
    #[serde(skip_serializing_if = "is_empty_string")]
    pub action_desc: String,
    /// 不可自动任务的人工操作指引。
    #[serde(skip_serializing_if = "is_empty_string")]
    pub hint: String,
}

// ---------------------------------------------------------------------------
// 查询
// ---------------------------------------------------------------------------

impl Runner {
    /// 查询某账号的成长任务状态（**只读**，无任何写操作）。
    ///
    /// 返回顺序：待办优先（可领奖 → 进行中 → 可报名），已领奖垫底。界面据此直接
    /// 渲染，不必自己再排一遍 —— 排序口径只在这里定义一次。
    pub async fn list_tasks(&self, a: &Auth) -> crate::Result<Vec<TaskView>> {
        let tasks = self.up.list_growth_tasks(a).await?;
        let mut out: Vec<TaskView> = Vec::with_capacity(tasks.len());
        for t in &tasks {
            // 国际版那 5 个静态任务直接**不下发**（见 [`INTL_ONLY_CODES`] 的说明）：
            // 它们没有可推进的状态，列出来只会变成 5 条「需在客户端手动完成」的
            // 噪音。过滤放在这里而不是让界面自己跳过 —— 列表口径只定义一次才不会
            // 漂移。
            if INTL_ONLY_CODES.contains(&t.code.as_str()) {
                continue;
            }
            let mut view = TaskView {
                code: t.code.clone(),
                title: t.title.clone(),
                description: t.description.clone(),
                task_desc: t.task_desc.clone(),
                credit: t.credit,
                energy: t.energy,
                accept_status: t.accept_status.clone(),
                has_progress: t.has_progress,
                current: t.current,
                target: t.target,
                progress: t.progress_text(),
                claimable: t.claimable(),
                claimed: t.claimed(),
                locked: t.locked,
                unlock_at: t.unlock_at.clone(),
                ..Default::default()
            };
            match action_for(&t.code) {
                Some(act) => {
                    view.automatable = true;
                    view.needs_chat = act.needs_chat;
                    view.action_desc = act.desc.to_string();
                }
                None => view.hint = unsupported_hint(&t.code).to_string(),
            }
            let (status, text) = view_status(t, view.automatable);
            view.status = status.to_string();
            view.status_text = text;
            out.push(view);
        }
        // ★ 稳定排序（对应 Go `sort.SliceStable`）：`sort_by_key` 是稳定的，同 rank
        // 的项保持上游下发顺序。
        out.sort_by_key(|v| view_rank(&v.status));
        Ok(out)
    }

    /// 只返回「还没做完」的可自动任务（界面「一键完成」的作用范围）。
    ///
    /// ⚠ 未解锁（`locked`）的必须排除。实测（2026-09-30，`Sequential_Tasks_2`，
    /// `unlock_at=2026-10-01T00:00:00+08:00`）：对锁定任务上报行为事件，上游返回
    /// **HTTP 200 且不报错**，但事件被静默丢弃、进度纹丝不动 —— 若不排除，一键完成
    /// 会把它们算进范围，逐条报「已完成」而实际什么都没发生，用户既拿不到奖励，也
    /// 看不出是锁的问题。锁是**上游的时间条件**，不是本地可以绕过的东西，唯一的正确
    /// 处理是等。
    pub async fn pending_tasks(&self, a: &Auth) -> crate::Result<Vec<TaskView>> {
        let all = self.list_tasks(a).await?;
        Ok(all
            .into_iter()
            .filter(|v| !v.claimed && v.automatable && !v.locked)
            .collect())
    }
}

/// 推导展示状态与中文说明（对应 Go `viewStatus`）。
///
/// ★ 分支顺序不可换：`claimed` → `claimable` → `!automatable` → `locked` →
/// `!participated` → `has_progress` → 兜底 `accepted`。特别是 `locked` 必须排在
/// `!participated` **之前** —— 锁定任务在两种列表口径下都可能是 `not_accepted`，
/// 先说「未报名」会让用户以为点一下就能报名。
fn view_status(t: &GrowthTask, automatable: bool) -> (&'static str, String) {
    if t.claimed() {
        (VIEW_CLAIMED, "奖励已领取".to_string())
    } else if t.claimable() {
        (VIEW_CLAIMABLE, "已达标，可领取奖励".to_string())
    } else if !automatable {
        (VIEW_UNSUPPORTED, "需在客户端手动完成".to_string())
    } else if t.locked {
        (VIEW_LOCKED, locked_text(&t.unlock_at))
    } else if !t.participated() {
        (VIEW_NOT_ACCEPTED, "未报名（可一键报名并完成）".to_string())
    } else if t.has_progress {
        (VIEW_IN_PROGRESS, "进行中".to_string())
    } else {
        (VIEW_ACCEPTED, "已报名".to_string())
    }
}

/// 未解锁状态的中文说明：带上解锁时刻（对应 Go `lockedText`）。
///
/// 为什么值得为一句文案单独写个函数：实测（2026-09-30，`Sequential_Tasks_2`）上游
/// 下发的 locked 任务是**日期锁**，`unlock_at` 明确到秒。只说「尚未解锁」用户无从
/// 判断是上游故障还是正常等待，也无法决定「现在要不要重试」；带上时刻，「等明天」
/// 就变成了可自行核实的结论。
///
/// 解析失败或字段缺失时回退成「尚未解锁」而**不报错**：这是纯展示字段，上游改格式
/// 不该让整张任务列表打不开 —— 少一句话远好过列表整体失败。
pub(crate) fn locked_text(unlock_at: &str) -> String {
    let s = unlock_at.trim();
    if s.is_empty() {
        return "尚未解锁".to_string();
    }
    // Go 侧用 `time.RFC3339` 解析后按**本地时区**格式化；Rust 侧同样先解析成绝对
    // 时刻，再转本地时区渲染 —— 只改显示，不改判定。
    match chrono::DateTime::parse_from_rfc3339(s) {
        Ok(ts) => format!(
            "尚未解锁（{} 开放）",
            ts.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M")
        ),
        Err(_) => "尚未解锁".to_string(),
    }
}

/// 排序权重：待办在前，已完成垫底（对应 Go `viewRank`）。
fn view_rank(status: &str) -> u8 {
    match status {
        VIEW_CLAIMABLE => 0,
        VIEW_IN_PROGRESS => 1,
        VIEW_NOT_ACCEPTED | VIEW_ACCEPTED => 2,
        VIEW_UNSUPPORTED | VIEW_LOCKED => 3,
        // claimed 及其它未知状态
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::tasks::{
        TASK_ACCEPT_ACCEPTED, TASK_ACCEPT_CLAIMED, TASK_ACCEPT_COMPLETED, TASK_ACCEPT_IN_PROGRESS,
        TASK_ACCEPT_NOT_ACCEPTED,
    };

    /// 造一个任务，只指定参与状态。
    fn task(status: &str) -> GrowthTask {
        GrowthTask {
            code: "t1".into(),
            accept_status: status.into(),
            ..Default::default()
        }
    }

    // ── view_status：分支顺序 ──

    /// 七个分支各自命中，文案逐字（Go `viewStatus` 的 switch 逐条）。
    #[test]
    fn view_status_covers_every_branch() {
        // claimed 优先于一切
        assert_eq!(view_status(&task(TASK_ACCEPT_CLAIMED), true).0, VIEW_CLAIMED);
        assert_eq!(
            view_status(&task(TASK_ACCEPT_CLAIMED), true).1,
            "奖励已领取"
        );

        // claimable：达标且未领
        let mut t = task(TASK_ACCEPT_IN_PROGRESS);
        t.has_progress = true;
        t.current = 5;
        t.target = 5;
        assert_eq!(view_status(&t, true).0, VIEW_CLAIMABLE);
        assert_eq!(view_status(&t, true).1, "已达标，可领取奖励");

        // !automatable
        assert_eq!(
            view_status(&task(TASK_ACCEPT_ACCEPTED), false).0,
            VIEW_UNSUPPORTED
        );
        assert_eq!(
            view_status(&task(TASK_ACCEPT_ACCEPTED), false).1,
            "需在客户端手动完成"
        );

        // locked
        let mut t = task(TASK_ACCEPT_ACCEPTED);
        t.locked = true;
        assert_eq!(view_status(&t, true).0, VIEW_LOCKED);

        // !participated
        assert_eq!(
            view_status(&task(TASK_ACCEPT_NOT_ACCEPTED), true).0,
            VIEW_NOT_ACCEPTED
        );
        assert_eq!(
            view_status(&task(TASK_ACCEPT_NOT_ACCEPTED), true).1,
            "未报名（可一键报名并完成）"
        );

        // has_progress
        let mut t = task(TASK_ACCEPT_IN_PROGRESS);
        t.has_progress = true;
        t.current = 1;
        t.target = 5;
        assert_eq!(view_status(&t, true).0, VIEW_IN_PROGRESS);
        assert_eq!(view_status(&t, true).1, "进行中");

        // 兜底 accepted
        assert_eq!(view_status(&task(TASK_ACCEPT_ACCEPTED), true).0, VIEW_ACCEPTED);
        assert_eq!(view_status(&task(TASK_ACCEPT_ACCEPTED), true).1, "已报名");
    }

    /// ★ 已领奖的任务即便进度显示「已达标」，也**不能**判成 `claimable`。
    ///
    /// 这条由 `GrowthTask::claimable()` 自身的 `!claimed()` 守卫保证（与 Go 逐字
    /// 一致），`view_status` 里 `claimed` 排在 `claimable` 之前则是**第二道防线**：
    /// 两个分支由构造互斥（`claimable() ⇒ !claimed()`），所以顺序本身观察不到，
    /// 但一旦哪天有人放宽了 `claimable()` 的守卫，先判 `claimed` 仍能挡住
    /// 「界面显示可领取、点下去却什么也不会发生」这个最糟的表现。
    #[test]
    fn claimed_is_never_claimable() {
        let mut t = task(TASK_ACCEPT_CLAIMED);
        t.has_progress = true;
        t.current = 5;
        t.target = 5;
        assert!(t.claimed());
        assert!(
            !t.claimable(),
            "claimable() 必须排除已领奖（否则会重复领奖）"
        );
        assert_eq!(view_status(&t, true).0, VIEW_CLAIMED);
    }

    /// ★ `!automatable` 压过 `locked`（Go 的顺序）：不支持的任务即便锁着，
    /// 报「需在客户端手动完成」也比报「尚未解锁」更有用 —— 前者是用户此刻就该
    /// 去做的事，后者会让人误以为等一等就能自动完成。
    #[test]
    fn unsupported_outranks_locked() {
        let mut t = task(TASK_ACCEPT_ACCEPTED);
        t.locked = true;
        assert_eq!(view_status(&t, false).0, VIEW_UNSUPPORTED);
    }

    /// ★ `locked` 压过 `!participated`（Go 的顺序，注释里点名了这条）：
    /// 锁定任务在两种列表口径下都可能是 `not_accepted`，先说「未报名」会让用户
    /// 以为点一下就能报名 —— 而实测（2026-09-30）对锁定任务报名/上报会得到
    /// HTTP 200 但被静默丢弃。
    #[test]
    fn locked_outranks_not_accepted() {
        let mut t = task(TASK_ACCEPT_NOT_ACCEPTED);
        t.locked = true;
        assert!(!t.participated(), "前提：确实是未报名");
        assert_eq!(view_status(&t, true).0, VIEW_LOCKED);
    }

    /// `completed` 不算 participated（`upstream::tasks` 的真值表）⇒ 应显示
    /// 「未报名（可一键报名并完成）」。这是上游的一种「做完了但没报名」的组合，
    /// 走 `needs_accept()` 能被自动补报名。
    #[test]
    fn completed_without_accept_shows_not_accepted() {
        let t = task(TASK_ACCEPT_COMPLETED);
        assert!(!t.participated());
        assert!(t.needs_accept());
        assert_eq!(view_status(&t, true).0, VIEW_NOT_ACCEPTED);
    }

    // ── view_rank ──

    /// 排序权重逐个断言，并验证**严格递增**的优先级链。
    #[test]
    fn view_rank_orders_pending_first() {
        assert_eq!(view_rank(VIEW_CLAIMABLE), 0);
        assert_eq!(view_rank(VIEW_IN_PROGRESS), 1);
        assert_eq!(view_rank(VIEW_NOT_ACCEPTED), 2);
        assert_eq!(view_rank(VIEW_ACCEPTED), 2);
        assert_eq!(view_rank(VIEW_UNSUPPORTED), 3);
        assert_eq!(view_rank(VIEW_LOCKED), 3);
        assert_eq!(view_rank(VIEW_CLAIMED), 4);
        // 未知状态垫底（与 claimed 同档，但稳定排序保证它们不相对于彼此乱序）
        assert_eq!(view_rank("weird"), 4);
    }

    // ── locked_text ──

    /// 三种情形：正常 RFC3339、空串、解析失败 —— 后两者都回退成「尚未解锁」
    /// 而**不报错**（纯展示字段，上游改格式不该让整张列表打不开）。
    #[test]
    fn locked_text_falls_back_without_failing() {
        assert_eq!(locked_text(""), "尚未解锁");
        assert_eq!(locked_text("   "), "尚未解锁", "纯空白等同缺失");
        assert_eq!(locked_text("2026-10-01"), "尚未解锁", "缺时刻不算 RFC3339");
        assert_eq!(locked_text("not a date"), "尚未解锁");
        assert_eq!(locked_text("2026-13-45T00:00:00+08:00"), "尚未解锁");
    }

    /// 合法 RFC3339 ⇒ 带上**本地时区**的时刻，且格式是 `YYYY-MM-DD HH:MM`。
    ///
    /// ★ 不断言具体时刻的字面值：用例会在不同时区的机器上跑，`Local` 换算后的
    /// 数字本就不同。断言的是**形状**与「解析确实成功了」。
    #[test]
    fn locked_text_renders_local_time() {
        let s = locked_text("2026-10-01T00:00:00+08:00");
        assert!(s.starts_with("尚未解锁（"), "{s}");
        assert!(s.ends_with(" 开放）"), "{s}");
        let inner = s
            .trim_start_matches("尚未解锁（")
            .trim_end_matches(" 开放）");
        // `%Y-%m-%d %H:%M` ⇒ 16 个字符，形如 2026-09-30 16:00
        assert_eq!(inner.len(), 16, "格式应为 YYYY-MM-DD HH:MM: {s}");
        assert_eq!(&inner[4..5], "-");
        assert_eq!(&inner[7..8], "-");
        assert_eq!(&inner[10..11], " ");
        assert_eq!(&inner[13..14], ":");
    }

    /// 带时区的 RFC3339 会被换算成本地时刻，而不是照抄字面 —— 这是「带上时刻
    /// 让用户能自行核实」的前提。
    #[test]
    fn locked_text_converts_across_timezones() {
        // 同一绝对时刻的两种写法必须渲染成同一个本地时刻
        let a = locked_text("2026-10-01T00:00:00+08:00");
        let b = locked_text("2026-09-30T16:00:00Z");
        assert_eq!(a, b);
    }

    // ── TaskView 的 JSON 形状 ──

    /// 默认值下只下发 `task_code` / `status` / `status_text` 与三个布尔/数字：
    /// 空串与 `false` 全部省略（对应 Go 的 `omitempty`）。
    #[test]
    fn task_view_omits_empty_fields() {
        let v = TaskView {
            code: "c1".into(),
            status: VIEW_ACCEPTED.into(),
            status_text: "已报名".into(),
            ..Default::default()
        };
        let j = serde_json::to_value(&v).unwrap();
        let obj = j.as_object().unwrap();
        // 必填项（Go 侧无 omitempty）
        for k in [
            "task_code",
            "status",
            "status_text",
            "has_progress",
            "current",
            "target",
            "progress",
            "claimable",
            "claimed",
            "automatable",
        ] {
            assert!(obj.contains_key(k), "缺字段 {k}: {j}");
        }
        // 空串 / false 省略
        for k in [
            "title",
            "description",
            "task_desc",
            "credit",
            "energy",
            "accept_status",
            "locked",
            "unlock_at",
            "needs_chat",
            "action_desc",
            "hint",
        ] {
            assert!(!obj.contains_key(k), "字段 {k} 不该出现: {j}");
        }
        assert_eq!(obj["task_code"], "c1");
    }

    /// 填满时字段名逐个核对（宿主按这些名字渲染，改一个界面就空一块）。
    #[test]
    fn task_view_field_names_match_go() {
        let v = TaskView {
            code: "c1".into(),
            title: "标题".into(),
            description: "指引".into(),
            task_desc: "条件".into(),
            credit: 10,
            energy: 5,
            status: VIEW_CLAIMABLE.into(),
            status_text: "已达标，可领取奖励".into(),
            accept_status: TASK_ACCEPT_IN_PROGRESS.into(),
            has_progress: true,
            current: 3,
            target: 5,
            progress: "3/5".into(),
            claimable: true,
            claimed: false,
            locked: true,
            unlock_at: "2026-10-01T00:00:00+08:00".into(),
            automatable: true,
            needs_chat: true,
            action_desc: "一键完成".into(),
            hint: "".into(),
        };
        let j = serde_json::to_value(&v).unwrap();
        assert_eq!(j["task_code"], "c1");
        assert_eq!(j["task_desc"], "条件");
        assert_eq!(j["status_text"], "已达标，可领取奖励");
        assert_eq!(j["accept_status"], TASK_ACCEPT_IN_PROGRESS);
        assert_eq!(j["has_progress"], true);
        assert_eq!(j["progress"], "3/5");
        assert_eq!(j["unlock_at"], "2026-10-01T00:00:00+08:00");
        assert_eq!(j["action_desc"], "一键完成");
        assert_eq!(j["credit"], 10);
        assert_eq!(j["energy"], 5);
        // 空 hint 仍然省略
        assert!(j.get("hint").is_none());
        // ★ 没有 `code` 这个别名：字段名是 `task_code`（Go 的 json tag 逐字）
        assert!(j.get("code").is_none());
    }

    /// `TaskView` 能往返（宿主侧的 TS 类型是从 JSON 推的，反序列化也得成立）。
    #[test]
    fn task_view_roundtrips() {
        let v = TaskView {
            code: "c1".into(),
            title: "标题".into(),
            status: VIEW_LOCKED.into(),
            status_text: locked_text("2026-10-01T00:00:00+08:00"),
            locked: true,
            current: 2,
            target: 4,
            progress: "2/4".into(),
            ..Default::default()
        };
        let s = serde_json::to_string(&v).unwrap();
        let back: TaskView = serde_json::from_str(&s).unwrap();
        assert_eq!(back, v);
    }

    /// 反序列化时缺字段全部走默认（`#[serde(default)]`）—— 宿主回传精简对象
    /// 不该报错。
    #[test]
    fn task_view_deserializes_from_minimal_object() {
        let v: TaskView = serde_json::from_str(r#"{"task_code":"c1"}"#).unwrap();
        assert_eq!(v.code, "c1");
        assert_eq!(v.status, "");
        assert!(!v.claimable);
    }

    // ── 国际版过滤口径 ──

    /// 5 个国际版任务码逐个都在名单里（删掉不是置灰）。
    #[test]
    fn intl_only_codes_covers_all_five() {
        assert_eq!(INTL_ONLY_CODES.len(), 5);
        for c in [
            "first_chat",
            "skill_installed",
            "wechat_linked",
            "expert_summoned",
            "template_used",
        ] {
            assert!(INTL_ONLY_CODES.contains(&c), "{c} 应在名单里");
        }
    }

    /// ★ 保留的两个国服人工任务是**国服**任务，不能被误加进删除名单 ——
    /// 它们有真实报名与进度，只是动作需人工（捐款回执 / 桌面端换血对话）。
    #[test]
    fn intl_only_codes_excludes_cn_manual_tasks() {
        for c in ["Expert_Philanthropy", "workstation_expert"] {
            assert!(!INTL_ONLY_CODES.contains(&c), "{c} 是国服任务，必须保留");
        }
    }
}
