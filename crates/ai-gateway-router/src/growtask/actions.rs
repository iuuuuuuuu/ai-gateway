//! 「一键完成」的动作表：任务码 → 具体动作实现（对应 Go `internal/growtask/actions.go`）。
//!
//! # 分类依据是**依赖强度**而非代码量
//!
//! 分类同时决定执行顺序与风险：
//!
//! - **A 类** 纯行为事件上报（无外部依赖、不消耗对话额度，最稳，放最前）；
//! - **B 类** 真实对话（消耗 token、上游留真实会话记录）；
//! - **C 类** 专家市场链（还要求专家 id 真实存在、判据事件的 `requestId` 必须是
//!   服务端分配的真实 id，依赖最多）；
//! - **D 类** 不可自动。
//!
//! 执行顺序 = 表顺序；单项失败不影响后续项。

use std::time::Duration;

use crate::auth::Auth;
use crate::error::GatewayError;
use crate::scheduler::Cancel;
use crate::upstream::desktop::{
    self, appearance_apply_event, automation_create_event, buddy_app_sequence, chat_sequence,
    design_canvas_sequence, expert_actual_use_event, expert_summon_sequence, in_night_window,
    playbook_prompt_sequence, skill_sequence, template_use_sequence, MarketExpert,
};
use crate::upstream::growth::is_buddy_task_incomplete;
use crate::upstream::tasks::GrowthTask;

use super::runner::Runner;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 企鹅教师助手（`Buddy_App_QQ` 的判据）。
const QQ_TEACHER_BUDDY_ID: &str = "cb_y5Dy46tPQGGWtueMxXbe";
const QQ_TEACHER_BUDDY_NAME: &str = "企鹅教师助手";
/// 和平精英激战金秋。
const HP_THEME_KEY: &str = "theme-tkmw7j";
const LIBRARY_DOC_URL: &str = "https://www.workbuddy.cn/space/d/o0KWYeynteVv06UnAZqIFm";
/// 腾讯轻量云专家。
const LIGHTHOUSE_EXPERT_ID: &str = "ex_2cvvUZQhDyeJ";
/// ★ 用 `fast-model` 而非昂贵模型：目的是产生「真实对话」这一事实，与模型能力
/// 无关，没必要为一次 1+1 消耗主力模型额度。
const FREE_MODEL: &str = "fast-model";

/// 顺序解锁任务组的第一项（小程序对话）。
pub const SEQUENTIAL_CHAT_CODE: &str = "Sequential_Tasks_1";
/// 顺序解锁任务组的第二项（小程序专家使用）。
pub const SEQUENTIAL_EXPERT_CODE: &str = "Sequential_Tasks_2";
/// 顺序解锁任务组的第三项（按真人节奏补报 5 次小程序对话）。
pub const SEQUENTIAL_CHAT5_CODE: &str = "Sequential_Tasks_3";

/// 小程序限定的任务码集合（同时决定**报名与领奖**走哪个口径）。
///
/// ★ 口径差别是实测出来的（2026-09-30，同一账号同一天）：默认口径 accept
/// `Sequential_Tasks_1` → HTTP 200 + code=0，但逐条结果
/// `{"status":"error","message":"task not found"}`；加 mp 头
///（`X-Client-Platform: miniprogram`）→ `{"status":"accepted"}`。即「HTTP 200 +
/// code=0」并不代表报名成功 —— 200 的壳里可以装着「任务不存在」。
///
/// ★ 只收**已验证**的三个码，不动 `school_season` 的既有报名路径（校园日活动已
/// 结束、上游不再下发，无法实测它是否同样强制 mp 头）。
const MP_ONLY_CODES: [&str; 3] = [
    SEQUENTIAL_CHAT_CODE,
    SEQUENTIAL_EXPERT_CODE,
    SEQUENTIAL_CHAT5_CODE,
];

/// 该任务码是否走小程序口径（报名 / 领奖）。
pub fn is_mp_only(code: &str) -> bool {
    MP_ONLY_CODES.contains(&code)
}

// ---------------------------------------------------------------------------
// 动作表
// ---------------------------------------------------------------------------

/// 动作种类（替代 Go 的函数指针字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Chat5,
    FirstBuddy,
    RichMeow,
    BuddyApp,
    AutomationCreate,
    LibraryRead,
    TemplateUse,
    PlaybookPrompt,
    CreateCanvas,
    Appearance,
    SchoolSeason,
    SequentialChat,
    MiniExpert,
    SequentialChat5,
    ModelChat,
    Expert5,
    ExpertTeam3,
    ExpertLighthouse,
    Skill1,
    BlackCat,
}

/// 一个可自动完成的成长任务动作。
pub struct Action {
    /// 目标任务码（与上游 `task_code` 精确对应）。
    pub code: &'static str,
    /// 展示用的动作说明。
    pub desc: &'static str,
    /// 是否消耗真实对话（供界面提示资源消耗）。
    pub needs_chat: bool,
    pub kind: ActionKind,
}

/// 动作表：**顺序即执行顺序**。
///
/// ★ 顺序理由：`chat_5` 放最前因复用 billing 域既有通道、依赖最少；
/// `Buddy_App` / `Buddy_App_QQ` 共用一次上报（同一组 buddyapp 事件同时满足两个
/// 任务），幂等由任务状态跳过兜底 —— 第一个跑完点亮后第二个会在前置检查里跳过；
/// `school_season` 挂在 C 类之前因它零对话消耗（纯事件上报）。
pub const ACTIONS: &[Action] = &[
    Action {
        code: "chat_5",
        desc: "补报对话活跃事件至达标（按差额）",
        needs_chat: false,
        kind: ActionKind::Chat5,
    },
    Action {
        code: "first_buddy",
        desc: "上报解锁 → 同意协议 → 领养第一只 Buddy",
        needs_chat: false,
        kind: ActionKind::FirstBuddy,
    },
    Action {
        code: "RichMeow_Chat",
        desc: "上报桌面端完整对话事件链（含成功回执）",
        needs_chat: false,
        kind: ActionKind::RichMeow,
    },
    Action {
        code: "Buddy_App",
        desc: "上报进入 Buddy 应用事件链",
        needs_chat: false,
        kind: ActionKind::BuddyApp,
    },
    Action {
        code: "Buddy_App_QQ",
        desc: "上报进入企鹅教师助手事件链",
        needs_chat: false,
        kind: ActionKind::BuddyApp,
    },
    Action {
        code: "automation_1",
        desc: "上报定时任务创建成功事件",
        needs_chat: false,
        kind: ActionKind::AutomationCreate,
    },
    Action {
        code: "Library_read",
        desc: "以 Web 指纹上报资料库阅读事件",
        needs_chat: false,
        kind: ActionKind::LibraryRead,
    },
    Action {
        code: "template_5",
        desc: "上报模板使用事件组 ×5",
        needs_chat: false,
        kind: ActionKind::TemplateUse,
    },
    Action {
        code: "playbook_prompt",
        desc: "上报灵感案例「做同款」发送事件组",
        needs_chat: false,
        kind: ActionKind::PlaybookPrompt,
    },
    Action {
        code: "create_canvas",
        desc: "上报设计创意画布创建事件组",
        needs_chat: false,
        kind: ActionKind::CreateCanvas,
    },
    Action {
        code: "Hp_Appearance",
        desc: "设置主题并上报皮肤生效事件",
        needs_chat: false,
        kind: ActionKind::Appearance,
    },
    Action {
        code: "school_season",
        desc: "上报校园日活动对话事件（小程序指纹 + activityId）",
        needs_chat: false,
        kind: ActionKind::SchoolSeason,
    },
    Action {
        code: SEQUENTIAL_CHAT_CODE,
        desc: "上报小程序对话事件（成长任务集，无 activityId）",
        needs_chat: false,
        kind: ActionKind::SequentialChat,
    },
    Action {
        code: SEQUENTIAL_EXPERT_CODE,
        desc: "上报小程序专家使用事件（expert_actual_use）",
        needs_chat: false,
        kind: ActionKind::MiniExpert,
    },
    Action {
        code: SEQUENTIAL_CHAT5_CODE,
        desc: "按真人节奏补报 5 次小程序对话（反作弊：需间隔）",
        needs_chat: false,
        kind: ActionKind::SequentialChat5,
    },
    Action {
        code: "Model_chat_GLM5.2",
        desc: "glm-5.2 真实对话一次并对齐模型上报",
        needs_chat: true,
        kind: ActionKind::ModelChat,
    },
    Action {
        code: "expert_5",
        desc: "真实专家召唤 + 使用链 ×5",
        needs_chat: true,
        kind: ActionKind::Expert5,
    },
    Action {
        code: "Expert_team_use_3",
        desc: "真实专家团召唤 + 使用链 ×3",
        needs_chat: true,
        kind: ActionKind::ExpertTeam3,
    },
    Action {
        code: "Expert_lighthouse",
        desc: "轻量云专家召唤 + 使用链（真实对话回执）",
        needs_chat: true,
        kind: ActionKind::ExpertLighthouse,
    },
    Action {
        code: "skill_1",
        desc: "真实对话 + 技能加载（skill_info）事件",
        needs_chat: true,
        kind: ActionKind::Skill1,
    },
    Action {
        code: "black_cat",
        desc: "夜猫时段内补足真实对话（23:00–08:00 北京时间）",
        // ★ 必须为 true（判据是夜猫时段内的真实对话），界面才能如实提示。
        needs_chat: true,
        kind: ActionKind::BlackCat,
    },
];

/// 按任务码取动作。
pub fn action_for(code: &str) -> Option<&'static Action> {
    ACTIONS.iter().find(|a| a.code == code)
}

/// 全部可自动完成的任务码（对应 Go `SupportedCodes`）。
pub fn supported_codes() -> Vec<&'static str> {
    ACTIONS.iter().map(|a| a.code).collect()
}

// ---------------------------------------------------------------------------
// 不可自动任务
// ---------------------------------------------------------------------------

/// 需真实捐款（服务端领奖时校验捐赠回执）。
const PHILANTHROPY_CODE: &str = "Expert_Philanthropy";
const WORKSTATION_CODE: &str = "workstation_expert";

/// 不可自动任务的人工操作指引（对应 Go `UnsupportedHint`）。
pub fn unsupported_hint(code: &str) -> &'static str {
    if code == PHILANTHROPY_CODE {
        // ★ 刻意**不**尝试伪造：回执由真实支付链路产生，伪造属欺诈且必然失败。
        return "该任务需真实捐款：服务端在领奖时校验捐赠回执，无法通过接口绕过；请按任务说明在客户端内完成";
    }
    if code == WORKSTATION_CODE {
        // ★ 不做（2026-09-20 对照参考脚本 workbuddyv3 后确认）的五步：① 把桌面端
        // `.info` 认证文件换成目标账号（改本机登录态）② `taskkill /F /IM
        // WorkBuddy.exe` 杀掉用户桌面端 ③ 重启等 8 秒 ④ 通过 CDP 发带
        // `expert://WorkspaceBuilder` 资源引用的对话 ⑤ 再杀一次重启恢复原登录态。
        // 理由：都在动用户正在用的桌面端 —— 打断用户会话（数据可能未保存）、与
        // 「网关只做 HTTP 层自动化」定位冲突（它是服务，不该操作 GUI）、失败时可能
        // 把用户桌面端留在「换了登录态但没恢复」的状态。收益（一个任务的积分）远
        // 小于风险。
        return "该任务需在 WorkBuddy 桌面端内与「工作台搭建师」专家对话（网关不操作桌面端：那会打断你正在用的会话）；请在客户端内完成";
    }
    "该任务需要客户端内的人工操作，无法自动完成；请按任务说明在官方客户端操作"
}

// ---------------------------------------------------------------------------
// 调度
// ---------------------------------------------------------------------------

/// 执行一个动作，返回给用户看的说明（对应 Go `action.run`）。
///
/// # 返回值语义（尽力而为）
///
/// 动作执行结局。
///
/// ★ 存在的唯一理由：`Ok` 里必须能区分「真的做了事」和「按上游规则跳过」。
/// 早前的签名只有 `Result<String, GatewayError>`，`black_cat` 在夜猫窗口外返回
/// `Ok(说明)` 表示「跳过」，而调用方 `runner::run_action` 把一切 `Ok` 都记成
/// `done` ⇒ 面板显示绿色「完成」徽章，用户以为任务做完了，实际进度纹丝不动。
/// 实测（2026-10-05，跨账号队列）：15 个账号的 `参与「夜猫子」夜间折扣活动`
/// 全部落成 `success` +「当前不在夜猫时段…」，就是这条假成功的现场证据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 动作真的执行了（是否推进进度由调用方回读判定）。
    Done(String),
    /// 按上游规则或前置条件**跳过**：现在执行不会计分，用户只能等。
    Skipped(String),
}

impl Outcome {
    /// 面向用户的说明（两种结局都带说明）。
    pub fn message(&self) -> &str {
        match self {
            Outcome::Done(m) | Outcome::Skipped(m) => m,
        }
    }

    /// 是否属于「跳过」。调用方据此决定记 `skipped` 还是 `done`。
    pub fn is_skipped(&self) -> bool {
        matches!(self, Outcome::Skipped(_))
    }
}

/// 夜猫窗口外的固定说明文案（`run_black_cat` 与单测共用）。
const NIGHT_WINDOW_OUT_MSG: &str =
    "当前不在夜猫时段（23:00–08:00 北京时间），上游不计入本次上报；请在时段内重试";

/// 夜猫窗口判定（抽成纯函数，便于单测）。
///
/// 返回 `Some(Skipped)` 表示**现在不该执行**：上游规则决定「窗口外的上报不计分」，
/// 这是正常状态而不是故障，所以是 `Skipped` 而不是 `Err`。
fn black_cat_window_gate(now: chrono::DateTime<chrono::Utc>) -> Option<Outcome> {
    if in_night_window(now) {
        None
    } else {
        Some(Outcome::Skipped(NIGHT_WINDOW_OUT_MSG.to_string()))
    }
}

/// 能推进进度就返回 `Done(说明)`；只有**确知无法推进**（前置条件不满足、上游拒绝）
/// 才返回 `Err`。上报部分成功/部分失败时返回 `Done(说明)` 并把失败写进说明 ——
/// 因为进度可能是部分推进的，而 `Err` 会让调用方跳过回读。
///
/// ★ 只有「按上游规则跳过」才返回 `Skipped(说明)`：调用方据此记 `skipped` 而不是
/// `done`，否则会出现「面板显示完成、进度其实没动」的假成功。
pub async fn run(
    kind: ActionKind,
    r: &Runner,
    cancel: &Cancel,
    a: &Auth,
    before: Option<&GrowthTask>,
) -> Result<Outcome, GatewayError> {
    match kind {
        ActionKind::Chat5 => run_chat5(r, cancel, a, before).await.map(Outcome::Done),
        ActionKind::FirstBuddy => run_first_buddy(r, cancel, a).await.map(Outcome::Done),
        ActionKind::RichMeow => run_rich_meow(r, a).await.map(Outcome::Done),
        ActionKind::BuddyApp => run_buddy_app(r, a).await.map(Outcome::Done),
        ActionKind::AutomationCreate => run_automation_create(r, a).await.map(Outcome::Done),
        ActionKind::LibraryRead => run_library_read(r, a).await.map(Outcome::Done),
        ActionKind::TemplateUse => run_template_use(r, cancel, a).await.map(Outcome::Done),
        ActionKind::PlaybookPrompt => run_playbook_prompt(r, a).await.map(Outcome::Done),
        ActionKind::CreateCanvas => run_create_canvas(r, a).await.map(Outcome::Done),
        ActionKind::Appearance => run_appearance(r, cancel, a).await.map(Outcome::Done),
        ActionKind::SchoolSeason => run_school_season(r, cancel, a, before)
            .await
            .map(Outcome::Done),
        ActionKind::SequentialChat => {
            run_mp_chat_task(r, cancel, a, before, SEQUENTIAL_CHAT_CODE, false)
                .await
                .map(Outcome::Done)
        }
        ActionKind::SequentialChat5 => {
            run_mp_chat_task(r, cancel, a, before, SEQUENTIAL_CHAT5_CODE, false)
                .await
                .map(Outcome::Done)
        }
        ActionKind::MiniExpert => run_mini_expert(r, a, before).await.map(Outcome::Done),
        ActionKind::ModelChat => run_model_chat(r, cancel, a).await.map(Outcome::Done),
        ActionKind::Expert5 => run_expert5(r, cancel, a).await.map(Outcome::Done),
        ActionKind::ExpertTeam3 => run_expert_team3(r, cancel, a).await.map(Outcome::Done),
        ActionKind::ExpertLighthouse => run_expert_lighthouse(r, a).await.map(Outcome::Done),
        ActionKind::Skill1 => run_skill1(r, a).await.map(Outcome::Done),
        ActionKind::BlackCat => run_black_cat(r, cancel, a, before).await,
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// 当前时间（毫秒，UTC）。
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// `request_id` 末 8 位（用于拼 `messageId`）。
///
/// ★ 用 `desktop::tail` 而不是字节切片：它按 `char_boundary` 回退，多字节字符
/// 不会切出非法 UTF-8。
fn req_tail(req: &str) -> String {
    desktop::tail(req, 8)
}

// ---------------------------------------------------------------------------
// A 类：纯行为事件上报
// ---------------------------------------------------------------------------

/// `chat_5` —— 补报对话活跃事件至达标。
async fn run_chat5(
    r: &Runner,
    cancel: &Cancel,
    a: &Auth,
    before: Option<&GrowthTask>,
) -> Result<String, GatewayError> {
    let Some(before) = before else {
        return Err(GatewayError::Other("任务不存在".to_string()));
    };
    let mut target = before.target;
    // 未报名时上游不下发 progress；`run_all` 已先报名，这里兜底用上游的真实目标 5。
    if target <= 0 {
        target = 5;
    }
    let need = target - before.current;
    if need <= 0 {
        return Ok("进度已达标，无需上报".to_string());
    }
    for i in 0..need {
        let cid = format!("wb2api-chat5-{}-{}", now_ms(), i);
        if let Err(e) = r.up.report_chat_activity(a, &cid, "").await {
            // 部分成功也如实汇报：已发的那些条可能已计分。
            return Ok(format!("已补报 {i}/{need} 条后中断: {e}"));
        }
        if i < need - 1 && !cancel.sleep(r.report_gap).await {
            return Ok(format!("已补报 {}/{need} 条（本轮被取消）", i + 1));
        }
    }
    Ok(format!("已补报 {need} 条对话活跃事件"))
}

/// `school_season` —— 校园日活动对话事件。
///
/// ★ 这是 growth 域下发的任务但**判据在 school 域**：发一条带
/// `activityId=school_open_day_2026` 的**小程序指纹**对话事件即点亮（target=1）。
///
/// ★ 为什么不能复用 `run_chat5` 的 [`Client::report_chat_activity`]（桌面版指纹）：
/// 实测（2026-09-18，真实账号）桌面版形状的事件发出去 HTTP 200，但 growth 域进度
/// **恒为 0**；换小程序指纹后 current 从 0 变 1、accept_status 变 completed、claim
/// 到账 100 积分 + 5 能量。差别就在 `source/ideName/ideType/extName/mode` 这几个
/// 指纹字段上。
async fn run_school_season(
    r: &Runner,
    cancel: &Cancel,
    a: &Auth,
    before: Option<&GrowthTask>,
) -> Result<String, GatewayError> {
    let Some(before) = before else {
        return Err(GatewayError::Other("任务不存在".to_string()));
    };
    let mut target = before.target;
    if target <= 0 {
        target = 1;
    }
    let need = target - before.current;
    if need <= 0 {
        return Ok("进度已达标，无需上报".to_string());
    }
    let mut sent = 0i64;
    for i in 0..need {
        let cid = format!("wb-run-{}-{}", now_ms(), i);
        if let Err(e) = r.up.report_school_season_activity(a, &cid).await {
            return Ok(format!("已上报 {sent}/{need} 条后中断: {e}"));
        }
        sent += 1;
        if i < need - 1 && !cancel.sleep(r.report_gap).await {
            return Ok(format!("已上报 {sent}/{need} 条（本轮被取消）"));
        }
    }
    // ★ 服务端归账需要一点时间：实测上报后**立即回读仍可能是 0/1**，等约 2 秒才
    // 稳定变成 1/1。睡这 2 秒是为了让调用方的回读能拿到新进度 —— 否则读出旧值，
    // 表现为「报了但没生效」，而实际上只是读早了。
    if !cancel.sleep(Duration::from_secs(2)).await {
        return Ok(format!("已上报 {sent} 条（等待归账时被取消）"));
    }
    Ok(format!("已上报 {sent} 条校园日活动事件"))
}

/// 小程序对话任务的间隔抖动（对应 Go `mpChatJitter`）。
///
/// ★ 为什么需要抖动：固定周期的上报本身就是一种机器指纹 ——「每 45.000 秒一条、
/// 连续 5 条」比「间隔 45~55 秒」更像脚本。参考实现用固定 0~10 秒随机；这里改成
/// **跟随 gap 缩放**（`gap/5`，45s → 9s，同量级），好处是测试把 gap 压到毫秒级时
/// 抖动自动一起消失，不需要额外开一个「关掉抖动」的开关（那种开关本身就会被误用
/// 成「测试里把反作弊节奏也关掉」）。
fn mp_chat_jitter(gap: Duration) -> Duration {
    let span = gap / 5;
    if span.is_zero() {
        return Duration::ZERO;
    }
    let ms = span.as_millis() as u64;
    Duration::from_millis(rand::random::<u64>() % ms.max(1))
}

/// 小程序对话任务（`Sequential_Tasks_1` / `_3`）的共同实现。
///
/// ★ 与 `school_season` 的唯一差别：**不带 `activityId`**。两个任务的判据事件
/// eventCode 相同（`chat_request_send`）、指纹相同（`mini_program` /
/// `wx_app_cloud` / `WorkBuddy_MP` / `workbuddy-mp`），服务端靠**有没有
/// activityId** 区分：`school_season` 必带
/// `activityId=school_open_day_2026`，成长任务集必须不带。搞反的表现是 HTTP 200 +
/// 进度恒为 0 —— 最难排查的一类失败。
///
/// ★ 反作弊：**每条上报前都睡**（首条也睡）。上游对「有效对话」有节奏校验：数秒级
/// 连发的事件会**先被计入进度**（回读 5/5、accept_status 甚至短暂变 completed），
/// 随后被判定无效**整体回滚** —— 进度回落、claim 返回 400 task not completed。
/// 2 秒连发 4 条全灭；45 秒间隔逐条上报全存活且能领到奖励。首条也等的理由：实测
/// 存在另一种失败 —— 上一轮被回滚后立即重报同样无效；宁可多等 45 秒，也不要发出
/// 一条注定被静默丢弃的事件（后者要赔上整个重试周期）。
async fn run_mp_chat_task(
    r: &Runner,
    cancel: &Cancel,
    a: &Auth,
    before: Option<&GrowthTask>,
    code: &str,
    with_activity_id: bool,
) -> Result<String, GatewayError> {
    let Some(before) = before else {
        return Err(GatewayError::Other("任务不存在".to_string()));
    };
    // ★ 为什么要重新读一次任务：accept 之后上游才下发真实 progress。`run_all` 的
    // 批量报名路径里，动作拿到的 `before` 是**报名前**的快照（target=0、
    // progress=null），照它算差额会少报：参考实现实测 `Sequential_Tasks_6` 报名后
    // 真实 target=10，只补 1 条就被误判达标去领奖 → claim 400。故先 refetch 一次，
    // 拿不到才回落用 before。
    let t = match r.refetch(a, code).await {
        Some(fresh) => fresh,
        None => before.clone(),
    };
    if t.claimed() {
        return Ok("该任务奖励已领取".to_string());
    }
    let mut target = t.target;
    if target <= 0 {
        target = 1;
    }
    if t.current >= target {
        return Ok("进度已达标，无需上报".to_string());
    }
    let need = target - t.current;
    let mut sent = 0i64;
    for i in 0..need {
        if !cancel
            .sleep(r.mp_chat_gap + mp_chat_jitter(r.mp_chat_gap))
            .await
        {
            if sent == 0 {
                return Ok("本轮在首次上报前被取消".to_string());
            }
            return Ok(format!("已上报 {sent}/{need} 条（本轮被取消）"));
        }
        let cid = format!("wb2api-mp-{}-{}", now_ms(), i);
        let res = if with_activity_id {
            r.up.report_school_season_activity(a, &cid).await
        } else {
            r.up.report_mp_chat_activity(a, &cid).await
        };
        if let Err(e) = res {
            return Ok(format!("已上报 {sent}/{need} 条后中断: {e}"));
        }
        sent += 1;
    }
    Ok(format!("已上报 {sent} 条小程序对话事件"))
}

/// `Sequential_Tasks_2` —— 小程序专家使用事件。
///
/// ★ 判据 = mp 指纹 `expert_actual_use`（**不带** activityId/conversationId、
/// extVersion=2.2.8、type=send_message）—— 与 school 域的专家事件是两套口径，
/// 别混用。参考实现实测上报即 completed，领奖 +200 积分 +5 能量。
///
/// ★ 判据事件的专家 id 必须是市场里**真实存在的 `ex_` id**：空 id 或编造的 id
/// 服务端照样返回 200，但不计分。所以拉不到市场列表就整个任务不动作 —— 宁可不做，
/// 也不要发一条注定不计分的事件（用户会以为已经完成）。
///
/// ★ 与参考实现的一处**顺序差异**：它把市场检查放在 accept **之前**，而本项目里
/// accept 由 [`Runner`] 统一负责，动作函数拿到的已经是报名后的状态。差异后果仅是多
/// 报名一次 —— 报名幂等（重复返回 `already_accepted`），任务停在「已报名未上报」
/// 也不影响下一轮重试，故不为此改动 Runner 的统一报名时序。
async fn run_mini_expert(
    r: &Runner,
    a: &Auth,
    before: Option<&GrowthTask>,
) -> Result<String, GatewayError> {
    let Some(before) = before else {
        return Err(GatewayError::Other("任务不存在".to_string()));
    };
    let t = match r.refetch(a, SEQUENTIAL_EXPERT_CODE).await {
        Some(fresh) => fresh,
        None => before.clone(),
    };
    if t.claimed() {
        return Ok("该任务奖励已领取".to_string());
    }
    let mut target = t.target;
    if target <= 0 {
        target = 1;
    }
    if t.current >= target {
        return Ok("进度已达标，无需上报".to_string());
    }
    let experts = match r.up.market_expert_list(a, "").await {
        Ok(v) => v,
        Err(e) => {
            return Ok(format!("专家市场不可用（{e}），跳过以防半程态"));
        }
    };
    let Some(e) = experts.first() else {
        return Ok("专家市场未返回可用专家，跳过以防半程态".to_string());
    };
    let name = if e.display_name_zh.is_empty() {
        e.profession_zh.clone()
    } else {
        e.display_name_zh.clone()
    };
    if let Err(err) = r
        .up
        .report_mp_expert_use(a, &e.expert_id, &name, &e.expert_type)
        .await
    {
        return Ok(format!("上报 expert_actual_use 失败: {err}"));
    }
    Ok(format!("已上报小程序专家使用事件（{name}）"))
}

/// `first_buddy` —— 上报解锁 → 同意协议 → 领养第一只 Buddy。
///
/// ★ 顺序不可换：领养接口要求 `first_buddy` 任务的前置条件（当日有对话活跃）已
/// 满足，未满足时返回 HTTP 400 `"first_buddy task not completed yet"`。该错误当日
/// 重试无意义（门槛按自然日重置），因此识别出来就返回说明而非 `Err`。
async fn run_first_buddy(
    r: &Runner,
    cancel: &Cancel,
    a: &Auth,
) -> Result<String, GatewayError> {
    let cid = format!("wb2api-adopt-{}", now_ms());
    if let Err(e) = r.up.report_chat_activity(a, &cid, "").await {
        return Err(GatewayError::Other(format!("前置上报失败: {e}")));
    }
    if !cancel.sleep(r.report_gap).await {
        return Ok("前置上报已完成，本轮被取消".to_string());
    }
    if let Err(e) = r.up.buddy_agreement(a).await {
        return Err(GatewayError::Other(format!("同意协议失败: {e}")));
    }
    if let Err(e) = r.up.buddy_first(a).await {
        if is_buddy_task_incomplete(&e) {
            return Ok(
                "前置已上报，但领养门槛未过（上游要求当日活跃），请稍后重试".to_string()
            );
        }
        return Err(GatewayError::Other(format!("领养失败: {e}")));
    }
    Ok("已领养第一只 Buddy".to_string())
}

/// `RichMeow_Chat` —— 桌面端完整对话事件链。
///
/// ★ 为什么不发真实对话：该任务判据是「桌面端完成一次对话」，由**事件链**判定，
/// 其中 `chat_message_response` 的 `isSuccessful=true` 是成功回执 ⇒ 纯事件即可
/// 点亮，无需消耗真实对话额度。
async fn run_rich_meow(r: &Runner, a: &Auth) -> Result<String, GatewayError> {
    let ms = now_ms();
    let conv = format!("wb2api-rm-{ms}");
    let req = format!("wb2api-rm-req-{ms}");
    let events = chat_sequence(&conv, &req, &format!("req-{ms}-user"), FREE_MODEL, FREE_MODEL);
    r.up.report_desktop_events(a, &events).await?;
    Ok("已上报桌面端对话事件链（含成功回执）".to_string())
}

/// `Buddy_App` / `Buddy_App_QQ` —— buddyapp 进入五连事件。
async fn run_buddy_app(r: &Runner, a: &Auth) -> Result<String, GatewayError> {
    let events = buddy_app_sequence(QQ_TEACHER_BUDDY_ID, QQ_TEACHER_BUDDY_NAME);
    r.up.report_desktop_events(a, &events).await?;
    Ok("已上报 buddyapp 进入五连事件（同时覆盖 Buddy_App 与 Buddy_App_QQ）".to_string())
}

/// `automation_1` —— 定时任务创建成功事件。
async fn run_automation_create(r: &Runner, a: &Auth) -> Result<String, GatewayError> {
    let ev = automation_create_event("wb2api 自动化");
    r.up.report_desktop_events(a, &[ev]).await?;
    Ok("已上报定时任务创建成功事件".to_string())
}

/// `Library_read` —— 资料库阅读事件。
///
/// ★ **必须用 Web 指纹**：判据是网页端的元素点击，用桌面指纹上报同类事件不计分
///（实测）。
async fn run_library_read(r: &Runner, a: &Auth) -> Result<String, GatewayError> {
    r.up.report_web_event(
        a,
        "web_element_click",
        LIBRARY_DOC_URL,
        "library_doc_intro_click",
        "WorkBuddy资料库介绍",
    )
    .await?;
    Ok("已上报资料库介绍阅读事件（Web 指纹）".to_string())
}

/// `template_5` —— 模板使用事件组 ×5。
///
/// ★ 每组都带一条完整对话链：判据事件 JOIN 到对话上，只发 `template_used` 而没有
/// 对话链不会被计入。
async fn run_template_use(r: &Runner, cancel: &Cancel, a: &Auth) -> Result<String, GatewayError> {
    const TEMPLATES: [(&str, &str); 5] = [
        ("1", "深度研究"),
        ("2", "周报生成"),
        ("3", "竞品分析"),
        ("4", "活动策划"),
        ("5", "代码评审"),
    ];
    for (i, (id, name)) in TEMPLATES.iter().enumerate() {
        let ms = now_ms();
        let conv = format!("wb2api-tpl-{ms}-{i}");
        let req = format!("wb2api-tpl-req-{ms}-{i}");
        let events = template_use_sequence(&conv, &req, id, name);
        if let Err(e) = r.up.report_desktop_events(a, &events).await {
            return Ok(format!("已上报 {i}/{} 组模板事件后中断: {e}", TEMPLATES.len()));
        }
        if !cancel.sleep(Duration::from_millis(300)).await {
            return Ok(format!(
                "已上报 {}/{} 组模板事件（本轮被取消）",
                i + 1,
                TEMPLATES.len()
            ));
        }
    }
    Ok("已上报模板使用事件组 ×5".to_string())
}

/// `playbook_prompt` —— 灵感案例「做同款」发送事件组。
async fn run_playbook_prompt(r: &Runner, a: &Auth) -> Result<String, GatewayError> {
    let ms = now_ms();
    let conv = format!("wb2api-pb-{ms}");
    let req = format!("wb2api-pb-req-{ms}");
    let events = playbook_prompt_sequence(&conv, &req, "pm-gtm-launch-plan", "新产品上市 GTM 发布计划一页纸");
    r.up.report_desktop_events(a, &events).await?;
    Ok("已上报 playbook_cta_click + playbook_prompt_send".to_string())
}

/// `create_canvas` —— 设计创意画布创建事件组。
async fn run_create_canvas(r: &Runner, a: &Auth) -> Result<String, GatewayError> {
    let ms = now_ms();
    let conv = format!("wb2api-canvas-{ms}");
    let req = format!("wb2api-canvas-req-{ms}");
    let events = design_canvas_sequence(&conv, &req);
    r.up.report_desktop_events(a, &events).await?;
    Ok("已上报 wbx_design_canvas_task_create/open".to_string())
}

/// `Hp_Appearance` —— 设置主题并上报皮肤生效事件。
///
/// ★ 两步都要：只调 `appearance/set` 不计分（判据是设置页关闭时的
/// `appearance_skin_apply` 事件），只发事件则缺少服务端侧的主题设置记录。
async fn run_appearance(r: &Runner, cancel: &Cancel, a: &Auth) -> Result<String, GatewayError> {
    if let Err(e) = r.up.set_appearance_theme(a, HP_THEME_KEY).await {
        return Err(GatewayError::Other(format!("设置主题失败: {e}")));
    }
    // 给服务端留出处理主题切换的时间，再发「生效」事件 —— 两个请求挨着发会出现
    // 「先报生效、后设主题」的时序倒置。
    if !cancel.sleep(Duration::from_secs(2)).await {
        return Ok("主题已设置，本轮被取消（未上报生效事件）".to_string());
    }
    let ev = appearance_apply_event(HP_THEME_KEY);
    r.up.report_desktop_events(a, &[ev]).await?;
    Ok("已设置主题并上报皮肤生效事件".to_string())
}

// ---------------------------------------------------------------------------
// B 类：真实对话
// ---------------------------------------------------------------------------

/// `Model_chat_GLM5.2` —— glm-5.2 真实对话一次并对齐模型上报。
///
/// ★ 两次上报缺一不可：真实对话产生「用过该模型」的服务端事实，对齐模型的上报事件
/// 则是任务计分器读取的字段 —— 只做其中之一都不计分。
async fn run_model_chat(r: &Runner, cancel: &Cancel, a: &Auth) -> Result<String, GatewayError> {
    const MODEL_ID: &str = "glm-5.2";
    const MODEL_NAME: &str = "GLM-5.2";
    let (conv, _) = match r
        .up
        .chat_with_model(a, MODEL_ID, "hi，请回复一句话", "")
        .await
    {
        Ok(v) => v,
        Err(e) => return Err(GatewayError::Other(format!("glm-5.2 对话失败: {e}"))),
    };
    if !cancel.sleep(r.report_gap).await {
        return Ok("对话已完成，本轮被取消（未上报）".to_string());
    }
    if let Err(e) = r
        .up
        .report_chat_activity_model(a, &conv, "", MODEL_ID, MODEL_NAME)
        .await
    {
        return Ok(format!("对话已完成，但模型对齐上报失败: {e}"));
    }
    Ok("已完成 glm-5.2 真实对话并对齐模型上报".to_string())
}

// ---------------------------------------------------------------------------
// C 类：专家市场链
// ---------------------------------------------------------------------------

/// `expert_5` —— 真实专家召唤 + 使用链 ×5。
async fn run_expert5(r: &Runner, cancel: &Cancel, a: &Auth) -> Result<String, GatewayError> {
    run_expert_batch(r, cancel, a, "agent", 5, "craft").await
}

/// `Expert_team_use_3` —— 真实专家团召唤 + 使用链 ×3。
async fn run_expert_team3(r: &Runner, cancel: &Cancel, a: &Auth) -> Result<String, GatewayError> {
    run_expert_batch(r, cancel, a, "team", 3, "craft").await
}

/// 专家批量链的共同实现。
///
/// ★ 逐个继续而非遇错中断：一位专家失败不该让其余 4 位也做不成，而任务的判据是
/// 「计数」而非「全部成功」。★ `ok == 0` 才是真失败（列出首个原因便于排查）。
async fn run_expert_batch(
    r: &Runner,
    cancel: &Cancel,
    a: &Auth,
    expert_type: &str,
    count: usize,
    mode: &str,
) -> Result<String, GatewayError> {
    let experts = match r.up.market_expert_list(a, expert_type).await {
        Ok(v) => v,
        Err(e) => {
            return Err(GatewayError::Other(format!("拉取专家市场列表失败: {e}")));
        }
    };
    if experts.is_empty() {
        return Err(GatewayError::Other("专家市场列表为空".to_string()));
    }
    let mut ok = 0usize;
    let mut first_err: Option<GatewayError> = None;
    for (i, e) in experts.iter().enumerate() {
        if ok >= count {
            break;
        }
        if cancel.is_cancelled() {
            break;
        }
        if i > 0 && !cancel.sleep(r.chat_gap).await {
            break;
        }
        if let Err(err) = expert_once(r, a, e, mode).await {
            if first_err.is_none() {
                first_err = Some(err);
            }
            continue;
        }
        ok += 1;
    }
    if ok == 0 {
        return match first_err {
            Some(e) => Err(GatewayError::Other(format!(
                "专家使用链全部失败（{expert_type}）: {e}"
            ))),
            None => Err(GatewayError::Other(format!(
                "专家使用链全部失败（{expert_type}）"
            ))),
        };
    }
    let mut msg = format!("已对 {ok}/{count} 位真实专家完成召唤+使用链（类型 {expert_type}）");
    if ok < count {
        msg.push_str("（未达目标数，可稍后重试）");
    }
    Ok(msg)
}

/// 对一位专家走完整链路（对应 Go `expertOnce`）。
///
/// ★ 单次链路三件事缺一不可（实测口径）：① 召唤事件组（`expert_summon_click` /
/// `expert_summoned`）② 真实对话拿服务端 `requestId` —— **自造 id 不计数**
/// ③ 使用事件（`expert_actual_use`）JOIN 到该 `requestId` + 一条对话链。
async fn expert_once(
    r: &Runner,
    a: &Auth,
    e: &MarketExpert,
    mode: &str,
) -> Result<(), GatewayError> {
    let summon = expert_summon_sequence(e);
    if let Err(err) = r.up.report_desktop_events(a, &summon).await {
        return Err(GatewayError::Other(format!(
            "召唤链 {}: {err}",
            e.expert_id
        )));
    }
    let (conv, req) = match r
        .up
        .chat_with_model(a, FREE_MODEL, "1+1等于几？直接回答。", &e.expert_id)
        .await
    {
        Ok(v) => v,
        Err(err) => {
            return Err(GatewayError::Other(format!(
                "专家对话 {}: {err}",
                e.expert_id
            )));
        }
    };
    let mut events = chat_sequence(&conv, &req, &format!("msg-{}", req_tail(&req)), FREE_MODEL, FREE_MODEL);
    events.push(expert_actual_use_event(e, &conv, &req, mode));
    if let Err(err) = r.up.report_desktop_events(a, &events).await {
        return Err(GatewayError::Other(format!(
            "使用事件 {}: {err}",
            e.expert_id
        )));
    }
    Ok(())
}

/// `Expert_lighthouse` —— 轻量云专家召唤 + 使用链。
///
/// ★ 与 `expert_5` 同构，两处差异（实测真实样本）：专家固定为腾讯轻量云专家，但
/// 版本等信息以市场列表为准（服务端下发的才真实）；使用事件的 mode 为 `LOCAL`
///（而非 `craft`）。
async fn run_expert_lighthouse(r: &Runner, a: &Auth) -> Result<String, GatewayError> {
    let mut lh = MarketExpert {
        expert_id: LIGHTHOUSE_EXPERT_ID.to_string(),
        expert_type: "agent".to_string(),
        display_name_zh: "腾讯轻量云专家".to_string(),
        profession_zh: "腾讯轻量云专家".to_string(),
        version: "1.0.2".to_string(),
        categories: Vec::new(),
    };
    // 市场列表命中则用服务端的字段（version / categories 等以服务端为准）。
    if let Ok(experts) = r.up.market_expert_list(a, "agent").await {
        if let Some(found) = experts.into_iter().find(|e| e.expert_id == LIGHTHOUSE_EXPERT_ID) {
            lh = found;
        }
    }
    if let Err(e) = expert_once(r, a, &lh, "LOCAL").await {
        return Err(GatewayError::Other(format!("轻量云专家链: {e}")));
    }
    Ok("已上报轻量云专家召唤+使用链（真实对话回执）".to_string())
}

/// `skill_1` —— 真实对话 + 技能加载事件。
///
/// ★ 判据是 `skill_info`（桌面指纹）JOIN 真实会话 —— 方向要注意：此前的
/// `skill_request_send` / `skill_installed` 等事件都不计分，唯一有效的是
/// `skill_info`。
async fn run_skill1(r: &Runner, a: &Auth) -> Result<String, GatewayError> {
    const SKILL_NAME: &str = "润泽小馆·日报撰写";
    const SKILL_ID: &str = "skill_2097350077599879168";
    let (conv, req) = match r
        .up
        .chat_with_model(a, FREE_MODEL, "1+1等于几？直接回答。", "")
        .await
    {
        Ok(v) => v,
        Err(e) => return Err(GatewayError::Other(format!("真实对话失败: {e}"))),
    };
    let events = skill_sequence(&conv, &req, SKILL_NAME, SKILL_ID, "1.0.0");
    if let Err(e) = r.up.report_desktop_events(a, &events).await {
        return Err(GatewayError::Other(format!("skill_info 事件上报失败: {e}")));
    }
    Ok("已上报真实对话 + skill_info 技能加载事件".to_string())
}

// ---------------------------------------------------------------------------
// D 类：时段敏感
// ---------------------------------------------------------------------------

/// `black_cat` —— 夜猫时段内补足真实对话（23:00–08:00 北京时间）。
///
/// ★ 实测现状（2026-09-16 夜，真实账号）：目标值为 **3**（不是 1），奖励为
/// **0 积分 0 能量**，而同一夜内用三种判据口径各做一次真实对话后进度都停在 1/3
/// 不动 —— A. CLI 域 chat（既有 ChatStream）+ billing 域模型对齐上报；B. 桌面域
/// 完整对话事件链（自造 conversationId/requestId）；C. 桌面域真实对话 + 用服务端
/// 返回的真实 requestId 构造对话链。最可能的原因是**按夜计数**（每夜最多计 1 次，
/// 需跨 3 夜完成）—— 目标 3 + 无奖励的组合与「连续 3 晚」的语义吻合。但这是推断：
/// 单夜实测既无法证实也无法证伪，故按「尽力补足差额」实现，由调用方的进度回读
/// 如实反映结果，不假装成功。
///
/// ★ 窗口外**跳过而不报错**：这是上游规则决定的正常状态，不是故障。报 `Err` 会让
/// 「一键完成」的结果里出现一条看起来像失败的记录，而用户完全无能为力（只能等到
/// 夜里）。
///
/// ★★ 但**跳过也不能算完成**：本函数返回 `Outcome::Skipped`，由 `runner::run_action`
/// 记成 `skipped`。早前这里返回 `Ok` ⇒ 被记成 `done` ⇒ 面板显示绿色「完成」徽章，
/// 而进度一个点都没动（2026-10-05 跨账号队列实测：15 个账号全部如此）。
async fn run_black_cat(
    r: &Runner,
    cancel: &Cancel,
    a: &Auth,
    before: Option<&GrowthTask>,
) -> Result<Outcome, GatewayError> {
    if let Some(skip) = black_cat_window_gate(chrono::Utc::now()) {
        return Ok(skip);
    }
    run_black_cat_inner(r, cancel, a, before)
        .await
        .map(Outcome::Done)
}

/// 夜猫窗口内的实际动作（窗口判定在 `run_black_cat` 里）。
async fn run_black_cat_inner(
    r: &Runner,
    cancel: &Cancel,
    a: &Auth,
    before: Option<&GrowthTask>,
) -> Result<String, GatewayError> {
    // 实测目标值；未报名拿不到 progress 时的兜底。
    let mut target = 3i64;
    let mut current = 0i64;
    if let Some(b) = before {
        if b.target > 0 {
            target = b.target;
        }
        current = b.current;
    }
    let mut need = target - current;
    if need <= 0 {
        return Ok("进度已达标，无需补足".to_string());
    }
    // 差额每次最多补 3 次，避免一次点击打出远超需求的对话量。
    if need > 3 {
        need = 3;
    }

    // 先发一条对话活跃上报：若上游认这条，成本最低。
    let cid = format!("wb2api-night-{}", now_ms());
    if let Err(e) = r.up.report_chat_activity(a, &cid, &format!("{cid}-r1")).await {
        return Err(e);
    }
    if !cancel.sleep(r.report_gap).await {
        return Ok("已上报一次夜猫活跃事件（本轮被取消）".to_string());
    }

    const MODEL_ID: &str = "glm-5.2";
    const MODEL_NAME: &str = "GLM-5.2";
    let mut done = 0i64;
    for i in 0..need {
        if cancel.is_cancelled() {
            break;
        }
        let (conv, _) = match r
            .up
            .chat_with_model(a, MODEL_ID, "1+1等于几？直接回答。", "")
            .await
        {
            Ok(v) => v,
            Err(e) => {
                if done == 0 {
                    return Ok(format!("已上报夜猫活跃事件，但真实对话失败: {e}"));
                }
                return Ok(format!("已完成 {done}/{need} 次夜间对话后中断: {e}"));
            }
        };
        // 对齐模型的上报：让这次对话在计分器眼里确实「用过 glm-5.2」。
        if let Err(e) = r
            .up
            .report_chat_activity_model(a, &conv, "", MODEL_ID, MODEL_NAME)
            .await
        {
            if done == 0 {
                return Ok(format!("对话已完成，但模型对齐上报失败: {e}"));
            }
            return Ok(format!("已完成 {done}/{need} 次夜间对话（对齐上报失败: {e}）"));
        }
        done += 1;
        if i < need - 1 && !cancel.sleep(r.chat_gap).await {
            break;
        }
    }
    Ok(format!(
        "已在夜猫时段上报并完成 {done}/{need} 次 glm-5.2 真实对话"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 动作表 ──

    /// 表里 21 项，任务码唯一（重复会让 `action_for` 静默命中第一条，
    /// 后一条永远不会执行）。
    #[test]
    fn action_codes_are_unique() {
        let codes = supported_codes();
        assert_eq!(codes.len(), 21, "动作表项数变了就要一并核对文档");
        let mut seen = std::collections::HashSet::new();
        for c in &codes {
            assert!(seen.insert(*c), "任务码重复: {c}");
        }
    }

    /// 三个顺序解锁任务码必须在表里且走小程序口径。
    #[test]
    fn sequential_codes_are_registered_and_mp_only() {
        for c in [
            SEQUENTIAL_CHAT_CODE,
            SEQUENTIAL_EXPERT_CODE,
            SEQUENTIAL_CHAT5_CODE,
        ] {
            assert!(action_for(c).is_some(), "{c} 应在动作表里");
            assert!(is_mp_only(c), "{c} 应走小程序口径");
        }
    }

    /// ★ 只收已验证的三个码。`school_season` 的报名路径不动（校园日活动已结束、
    /// 上游不再下发，无法实测它是否同样强制 mp 头），误加进去会让它的报名
    /// 走到一个没有依据的分支上。
    #[test]
    fn mp_only_codes_are_exactly_three() {
        assert!(!is_mp_only("school_season"), "school_season 未经验证，不能加");
        assert!(!is_mp_only("chat_5"));
        assert!(!is_mp_only(""));
    }

    /// `needs_chat` 只该标在真的会产生对话的任务上 —— 界面据此提示资源消耗，
    /// 标错会让用户以为点一下不花钱（或反之被吓退）。
    #[test]
    fn needs_chat_matches_the_real_dialogue_tasks() {
        for c in [
            "chat_5",
            "first_buddy",
            "RichMeow_Chat",
            "Buddy_App",
            "Buddy_App_QQ",
            "automation_1",
            "Library_read",
            "template_5",
            "playbook_prompt",
            "create_canvas",
            "Hp_Appearance",
            "school_season",
            SEQUENTIAL_CHAT_CODE,
            SEQUENTIAL_EXPERT_CODE,
            SEQUENTIAL_CHAT5_CODE,
        ] {
            let a = action_for(c).unwrap_or_else(|| panic!("{c} 应在动作表里"));
            assert!(!a.needs_chat, "{c} 是纯事件上报，不该标 needs_chat");
        }
        for c in [
            "Model_chat_GLM5.2",
            "expert_5",
            "Expert_team_use_3",
            "Expert_lighthouse",
            "skill_1",
            "black_cat",
        ] {
            let a = action_for(c).unwrap_or_else(|| panic!("{c} 应在动作表里"));
            assert!(a.needs_chat, "{c} 会发真实对话，必须标 needs_chat");
        }
    }

    /// `Buddy_App` / `Buddy_App_QQ` 共用同一个 `ActionKind`（一次上报同时满足
    /// 两个任务），这是有意的合并，不是复制粘贴漏改。
    #[test]
    fn buddy_app_codes_share_one_kind() {
        let a = action_for("Buddy_App").unwrap();
        let b = action_for("Buddy_App_QQ").unwrap();
        assert_eq!(a.kind, ActionKind::BuddyApp);
        assert_eq!(b.kind, ActionKind::BuddyApp);
        assert_ne!(a.code, b.code, "两个任务码不同，只是实现共用");
    }

    /// 每个 `ActionKind` 都恰有一个动作项 —— 漏一个意味着那个任务永远无法自动完成，
    /// 多一个意味着有项永远跑不到。
    #[test]
    fn every_action_kind_has_exactly_one_entry() {
        let all = [
            ActionKind::Chat5,
            ActionKind::FirstBuddy,
            ActionKind::RichMeow,
            ActionKind::BuddyApp,
            ActionKind::AutomationCreate,
            ActionKind::LibraryRead,
            ActionKind::TemplateUse,
            ActionKind::PlaybookPrompt,
            ActionKind::CreateCanvas,
            ActionKind::Appearance,
            ActionKind::SchoolSeason,
            ActionKind::SequentialChat,
            ActionKind::MiniExpert,
            ActionKind::SequentialChat5,
            ActionKind::ModelChat,
            ActionKind::Expert5,
            ActionKind::ExpertTeam3,
            ActionKind::ExpertLighthouse,
            ActionKind::Skill1,
            ActionKind::BlackCat,
        ];
        for k in all {
            let n = ACTIONS.iter().filter(|a| a.kind == k).count();
            // BuddyApp 是两个码共用一个 kind，故为 2
            let want = if k == ActionKind::BuddyApp { 2 } else { 1 };
            assert_eq!(n, want, "{k:?} 的动作项数应为 {want}，实为 {n}");
        }
    }

    /// 每项都非空：空任务码会让 `action_for("")` 命中它，而 `run_one` 的
    /// 「任务不存在」判定是 `code.is_empty()` 之外的路径 —— 会造成错配。
    #[test]
    fn action_entries_are_well_formed() {
        for a in ACTIONS {
            assert!(!a.code.is_empty(), "有空任务码");
            assert!(!a.desc.is_empty(), "{} 缺动作说明", a.code);
        }
        assert!(action_for("").is_none(), "空码不该命中任何动作");
        assert!(action_for("不存在").is_none());
    }

    /// ★ 动作表的顺序**就是执行顺序**，而 `chat_5` 必须在最前（复用 billing 域
    /// 既有通道、依赖最少）。顺序被无意改动会让某些任务在前置检查里被跳过。
    #[test]
    fn chat5_runs_first() {
        assert_eq!(ACTIONS[0].code, "chat_5");
    }

    // ── 不可自动任务 ──

    /// 两个国服人工任务各有**具体**的置灰原因，且互不相同 —— 泛泛一句
    /// 「需人工完成」会让用户不知道到底要做什么。
    #[test]
    fn unsupported_hints_are_specific() {
        let p = unsupported_hint(PHILANTHROPY_CODE);
        assert!(p.contains("捐款"), "{p}");
        assert!(p.contains("回执"), "{p}");

        let w = unsupported_hint(WORKSTATION_CODE);
        assert!(w.contains("桌面端"), "{w}");
        assert!(w.contains("工作台搭建师"), "{w}");

        assert_ne!(p, w, "两个任务的原因必须不同");
        // 未知码回落成通用文案
        let g = unsupported_hint("whatever");
        assert!(g.contains("无法自动完成"), "{g}");
        assert_ne!(g, p);
    }

    /// ★ 这两个码**不能**进动作表：能进就意味着「一键完成」会去点它们，而它们
    /// 没有对应接口（伪造捐款回执属欺诈；桌面端操作会打断用户会话）。
    #[test]
    fn manual_only_codes_are_not_automatable() {
        for c in [PHILANTHROPY_CODE, WORKSTATION_CODE] {
            assert!(action_for(c).is_none(), "{c} 不该可自动完成");
        }
    }

    // ── 小工具 ──

    /// `req_tail` 按**字节**取末 8 位（与 Go `reqTail` 的 `len(req)-8` 同口径），
    /// 区别在于碰到多字节字符时 `desktop::tail` 会回退到字符边界 —— Go 会切出
    /// 非法 UTF-8 的碎片，Rust 则给出一个**更短但合法**的串。
    ///
    /// 这里把两种情形都钉住：ASCII 下逐字节等价，多字节下「宁可短，不可碎」。
    #[test]
    fn req_tail_is_byte_counted_but_char_boundary_safe() {
        // ── ASCII：与 Go 完全等价 ──
        assert_eq!(req_tail("short"), "short", "不足 8 字节原样返回");
        assert_eq!(req_tail("12345678"), "12345678", "正好 8 字节");
        assert_eq!(req_tail("abcdefghij"), "cdefghij");
        assert_eq!(req_tail(""), "");

        // ── 多字节：不会 panic，也不会切出半个字符 ──
        // 10 个中文字符 = 30 字节；start = 22 落在「八」中间（21..24），
        // 回退到 24 ⇒ 只剩「九十」（6 字节 < 8）。
        let s = "一二三四五六七八九十";
        let t = req_tail(s);
        assert_eq!(t, "九十");
        assert!(s.ends_with(&t), "必须是原串的后缀（没有错位）");
        assert!(t.chars().count() <= 8);
        // 中英混排：start = 26-8 = 18 落在「六」中间（17..20）⇒ 回退到 20 ⇒「七八」
        let m = req_tail("ab一二三四五六七八");
        assert_eq!(m, "七八");
        assert!(m.chars().count() < 8, "多字节下结果可能短于 8 个字符");
        // ★ 关键性质：无论输入什么，结果都是合法 UTF-8 的**后缀**，且长度不超 8 字节
        for s in ["", "a", "abcdefgh", "abcdefghi", "一二三四五六七八九十", "ab一二三四五六七八"] {
            let t = req_tail(s);
            assert!(s.ends_with(&t), "{s:?} 的后缀应为 {t:?}");
            assert!(t.len() <= 8 || s.len() <= 8, "{s:?} ⇒ {t:?} 超长");
        }
    }

    /// 抖动跟随 gap 缩放：gap 为 0 时**必须**是 0（测试把 gap 压到毫秒级时
    /// 抖动自动一起消失，不需要额外开一个「关掉抖动」的开关）。
    #[test]
    fn mp_chat_jitter_scales_with_gap() {
        assert_eq!(mp_chat_jitter(Duration::ZERO), Duration::ZERO);
        // 小于 5ms 时 span 归零
        assert_eq!(mp_chat_jitter(Duration::from_millis(4)), Duration::ZERO);

        // 45s 基准 ⇒ 上界 9s
        let gap = Duration::from_secs(45);
        for _ in 0..50 {
            let j = mp_chat_jitter(gap);
            assert!(j < Duration::from_secs(9), "抖动 {j:?} 应小于 gap/5");
        }
        // 至少有一次非零（`rand` 全 0 的概率可忽略）
        let any = (0..50).any(|_| !mp_chat_jitter(gap).is_zero());
        assert!(any, "抖动不该恒为 0");
    }

    // ── 夜猫窗口门控 ──

    /// ★★ 夜猫窗口外必须是 `Skipped`，**不能**是 `Done`。
    ///
    /// 这条是 ⑬ 的修复核心。实测（2026-10-05 跨账号任务队列）：15 个账号的
    /// `参与「夜猫子」夜间折扣活动` 全部落成 `success` +「当前不在夜猫时段…」，
    /// 面板亮绿色「完成」徽章，而进度一个点都没动 —— 因为窗口外的 `Ok` 被调用方
    /// 无条件记成了 `done`。
    #[test]
    fn black_cat_outside_window_is_skipped_not_done() {
        use chrono::{TimeZone, Utc};
        // 北京时间 = UTC+8。12:00 北京 = 04:00 UTC（窗口外）
        let noon = Utc.with_ymd_and_hms(2026, 10, 5, 4, 0, 0).unwrap();
        let g = black_cat_window_gate(noon).expect("12:00 北京时间应在窗口外");
        assert!(g.is_skipped(), "窗口外必须是 Skipped，实为 {g:?}");
        assert_eq!(g.message(), NIGHT_WINDOW_OUT_MSG);
        assert!(g.message().contains("夜猫"), "{} 要说明原因", g.message());
        assert!(
            g.message().contains("23:00–08:00"),
            "{} 要写清窗口区间",
            g.message()
        );
        // 22:59 北京 = 14:59 UTC（窗口外边界）
        let before = Utc.with_ymd_and_hms(2026, 10, 5, 14, 59, 0).unwrap();
        assert!(black_cat_window_gate(before).is_some(), "22:59 应在窗口外");
    }

    /// 窗口内必须放行（`None` = 不跳过），否则任务永远做不了。
    #[test]
    fn black_cat_inside_window_passes_the_gate() {
        use chrono::{TimeZone, Utc};
        // 23:00 / 00:00 / 07:59 北京 = 15:00 / 16:00 / 23:59 UTC
        for (h, m) in [(15u32, 0u32), (16, 0), (23, 59)] {
            let at = Utc.with_ymd_and_hms(2026, 10, 5, h, m, 0).unwrap();
            assert!(
                black_cat_window_gate(at).is_none(),
                "UTC {h:02}:{m:02} 应在夜猫窗口内"
            );
        }
        // 08:00 北京 = 00:00 UTC（窗口终点，属窗口外）
        let end = Utc.with_ymd_and_hms(2026, 10, 6, 0, 0, 0).unwrap();
        assert!(black_cat_window_gate(end).is_some(), "08:00 是终点，应在窗口外");
    }

    /// `Outcome` 的两个变体都要能取出说明，且 `is_skipped` 只对 `Skipped` 为真。
    #[test]
    fn outcome_message_and_is_skipped() {
        let d = Outcome::Done("做了".to_string());
        let s = Outcome::Skipped("没做".to_string());
        assert_eq!(d.message(), "做了");
        assert_eq!(s.message(), "没做");
        assert!(!d.is_skipped());
        assert!(s.is_skipped());
        assert_ne!(d, s, "Done 与 Skipped 必须可区分");
    }

    /// `now_ms` 是毫秒级 UTC 时间戳（用于事件里的 `timestamp`）。
    #[test]
    fn now_ms_is_milliseconds() {
        let t = now_ms();
        // 2020-01-01 之后的毫秒值下界，且远小于 2100 年 —— 秒级/微秒级会立刻露馅
        assert!(t > 1_577_836_800_000, "看起来不是毫秒: {t}");
        assert!(t < 4_102_444_800_000, "看起来不是毫秒: {t}");
    }
}
