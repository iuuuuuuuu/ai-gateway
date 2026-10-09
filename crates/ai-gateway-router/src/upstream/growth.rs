//! 成长域上游调用：连登、补签、兑换、抽奖、盲盒、旅行、开学季、国际版试用。
//!
//! 对应 Go 文件：
//!
//! | Go 源文件          | 职责                                              |
//! |--------------------|---------------------------------------------------|
//! | `growth_map.go`    | 连登状态、热力图、补签卡、业务答复判定            |
//! | `growth_reward.go` | 档位兑换、抽奖次数/抽奖、礼包与补偿领取           |
//! | `blindbox.go`      | 盲盒额度与开盒                                    |
//! | `travel.go`        | 宠物旅行状态机 + 连登天数回读                     |
//! | `school.go`        | 开学季任务清单与领取                              |
//! | `trial.go`         | 国际版 IDE 试用加油包                             |
//!
//! # 为什么这些方法必须写在 `impl Client`
//!
//! [`Client`] 的 `http` / `chat` / `cfg` 是**私有字段**，私有可见性只覆盖
//! 定义它的模块及其子模块。请求管道（[`Client::growth_json`] 等）留在
//! `client.rs`，本文件只按域拼路径与解析响应。
//!
//! # 三条不能改错的口径
//!
//! 1. **任务列表走 no-UA 口径**，其余 growth 端点带 `Go-http-client/1.1`：
//!    上游按 UA 判定客户端平台并据此过滤任务清单（实测不发 UA → 20 项含
//!    小程序任务；发 Go 默认 UA → 18 项）。全局去掉 UA 会改变其余端点的指纹。
//! 2. **达标判据用 `tiers[].days` 与 `streak.days` 直接比较**，
//!    绝不用 `remaining_days`：实测它与 `tiers[].days` 不自洽。
//! 3. **`HeatmapDayScore` 的 `ok=false` 表示「没有该日格 = 无判据」**，
//!    不能当成 `score == 0` —— 那会对窗口外的日期发起必然失败的补签。
//!
//! # 两个域：growth（chat）与 billing
//!
//! 六个 Go 源文件并**不**都走同一个域，照抄路径容易把请求打到错的基址上：
//!
//! | 域 | 管道 | 端点 |
//! |---|---|---|
//! | chat（`copilot.tencent.com`） | [`Client::growth_json`] | 连登 / 热力图 / 补签 / 兑换 / 抽奖 / 盲盒 / 旅行 / 领养 |
//! | billing（`www.codebuddy.cn`） | [`Client::billing_json`] | **开学季**（`school.go`）/ **trial 加油包**（`trial.go`）/ 礼包 / 补偿 |
//!
//! `school.go` 文件头逐字记录了这条：「活动接口走 portal 前缀、基址同 billing 域
//! （国服 = codebuddy.cn），且**响应信封可能不同**，故单独解析。」trial 同理 ——
//! 它是 billing 侧的试用额度接口，不是成长活动。
//!
//! # 三条「看起来像 bug、其实是刻意」的取舍
//!
//! - **解析失败一律 `Err`，不降级成零值**（连登状态、抽奖次数、开学季清单）。
//!   调用方靠「真 0」与「拿不到」区分两种完全不同的处境：`streak.days == 0`
//!   是「上报成功但服务端静默丢弃」的**唯一**信号，把解析失败也降级成 0 会让
//!   这条告警彻底失效。
//! - **反过来，回执字段缺失不算失败**（兑换、抽奖、旅行领奖、礼包）：上游返回
//!   空 `data` 或只回部分字段时按 0 记日志即可，为此报错会让一次**已经生效**的
//!   领取在本地被记成失败。
//! - **`claim_trial` 的业务码 14051 是幂等成功**（`Ok(false)` 而不是 `Err`）：
//!   「本周期已领过」是每天都会发生的常态，报错会让调用方误以为需要重试。
//!
//! # `client_token` 必须每次新
//!
//! 兑换与抽奖的 `client_token` 是**幂等键**，直接决定「本次领取有没有生效」。
//! 复用旧键可能被上游按幂等键去重吞掉本次领取，表现为「返回成功但没到账」；
//! 抽奖更严重 —— 第二次抽奖会被当成第一次的重放而丢弃。故空串时自动生成
//! 16 字节随机数的十六进制形式，不用时间戳、不用计数器。
//!
//! # 一个已存在的同名方法
//!
//! `report_chat_activity` 定义在 `upstream::desktop`（活跃上报走 billing 域且
//! 带 report 头族），**本文件刻意不重复定义**，否则同一 `impl Client` 内会出现
//! 两个同名方法（E0592）。

use chrono::{DateTime, Datelike, Local, NaiveDateTime};
use rand::RngCore;

use crate::auth::Auth;
use crate::error::{GatewayError, Result};
use crate::upstream::classify::ErrKind;
use crate::upstream::client::Client;
use crate::upstream::tasks::path_escape;

// ──────────────────────────── 端点常量 ────────────────────────────

/// 连登状态（`growth_map.go` 与 `travel.go` 共用同一端点）。
pub const GROWTH_STREAK_PATH: &str = "/activity/growth/streak";
/// 活跃热力图（定长自然日窗口）。
pub const GROWTH_HEATMAP_PATH: &str = "/activity/growth/heatmap";
/// 补签卡使用。
pub const GROWTH_MAKEUP_CARD_USE_PATH: &str = "/activity/growth/makeup-cards/use";
/// 连登档位兑换。
pub const GROWTH_REDEEM_PATH: &str = "/activity/growth/redeem";
/// 抽奖次数余额。
pub const GROWTH_LOTTERY_CHANCES_PATH: &str = "/activity/growth/lottery/chances";
/// 抽奖。
pub const GROWTH_LOTTERY_DRAW_PATH: &str = "/activity/growth/lottery/draw";
/// 新手礼包（**billing 域**）。
pub const CLAIM_GIFT_PATH: &str = "/billing/meter/claim-gift";
/// 活动补偿（**billing 域**）。
pub const CLAIM_COMPENSATION_PATH: &str = "/billing/meter/claim-compensation";
/// 盲盒额度。
pub const GROWTH_BLINDBOX_QUOTA_PATH: &str = "/activity/growth/buddy/quota";
/// 开盲盒。
pub const GROWTH_BLINDBOX_OPEN_PATH: &str = "/activity/growth/buddy/open";
/// 开学季任务清单（**billing 域**、portal 前缀）。
pub const SCHOOL_TASKS_PATH: &str = "/portal/activity/school/tasks";
/// 开学季领取路径**模板**（`{}` 处填任务码）。
///
/// ★ 旧端点 `/v2/activity/growth/tasks/reward/claim` 是 **404 错端点**
///（Go `school.go:25` 的注释逐字标注「勿用」）：它在 chat 域上根本不存在，
/// 失败形态与「任务未完成」无法区分，极易被误判成「进度没达标」。
pub const SCHOOL_CLAIM_PATH_FMT: &str = "/portal/activity/school/tasks/{}/claim";
/// 国际版 IDE 试用加油包（**billing 域**）。
pub const TRIAL_PATH: &str = "/billing/ide/trial";
/// 宠物旅行状态。
pub const TRAVEL_STATUS_PATH: &str = "/activity/growth/buddy/travel/status";
/// 派出旅行。
pub const TRAVEL_DEPART_PATH: &str = "/activity/growth/buddy/travel/depart";
/// 到站领奖。
pub const TRAVEL_CLAIM_PATH: &str = "/activity/growth/buddy/travel/claim";
/// 宠物信息。
pub const BUDDY_INFO_PATH: &str = "/activity/growth/buddy/info";
/// 领养宠物。
pub const BUDDY_FIRST_PATH: &str = "/activity/growth/buddy/first";
/// 领养协议同意。
pub const BUDDY_AGREEMENT_PATH: &str = "/activity/growth/buddy/agreement";

/// 单轮开盒上限。
///
/// 开盒**吃能量**，而抽奖与兑换用同一份能量。限次保证「必得收益先拿到」，
/// 剩下的能量才拿去开盒。调用方还应再取 `min(affordable, 本值)`。
pub const MAX_BLIND_BOX_OPENS: i64 = 5;

/// 旅行状态：空闲，可派出。
pub const TRAVEL_STATE_IDLE: &str = "idle";
/// 旅行状态：在途，本轮不动。
pub const TRAVEL_STATE_TRAVELING: &str = "traveling";
/// 旅行状态：已到站，可领奖。
pub const TRAVEL_STATE_ARRIVED: &str = "arrived";

/// 开学季任务状态：已达标待领取。
pub const SCHOOL_TASK_STATUS_FINISHED: &str = "finished";

/// 「领养门槛未达」的答复标记（**小写**，比较时只 lower 左边）。
const BUDDY_TASK_INCOMPLETE_MARKER: &str = "first_buddy task not completed yet";

/// trial 已领取的两种指纹。
///
/// ★ 两个指纹**以不同 HTTP 状态抵达**，故 `trial_already_err` 刻意不判状态码：
/// - `code=14051` —— HTTP 200 + 业务码非 0（`unwrap_envelope` 把它拼进 `msg`）；
/// - `"code":14051` —— HTTP ≥ 400 时原始响应体被截断进 `msg`。
///
/// 大小写敏感：两者都是上游逐字输出的 ASCII，用 fold 匹配反而会放过
/// 一个形如 `Code=14051` 的伪造响应。
const TRIAL_ALREADY_MARKERS: &[&str] = &["code=14051", "\"code\":14051"];

/// 兑换「本月已领」标记（408 与 409 都用同一组）。
const REDEEM_ALREADY_MARKERS: &[&str] = &["duplicate", "already claimed", "已领取", "重复领取"];
/// 兑换「连登天数不足」标记。
const REDEEM_NOT_ENOUGH_MARKERS: &[&str] = &["连续登录天数不足", "not enough", "insufficient"];
/// 抽奖「无次数」标记。
const LOTTERY_NO_CHANCE_MARKERS: &[&str] = &["insufficient lottery chance balance"];
/// 抽奖「未开启」标记。
const LOTTERY_DISABLED_MARKERS: &[&str] = &["lottery disabled"];

/// 补签「没有补签卡余额」标记（HTTP 403）。
const MAKEUP_NO_CARD_MARKERS: &[&str] = &["no makeup card balance"];
/// 补签「该日并未漏签」标记（HTTP 400，两条同义文案）。
const MAKEUP_NOT_BROKEN_MARKERS: &[&str] = &["date is not broken", "no makeup needed"];
/// 补签「跨月被拒」标记（HTTP 400）。
const MAKEUP_OUT_OF_MONTH_MARKERS: &[&str] = &["only current month makeup allowed"];

// ──────────────────────────── 响应类型 ────────────────────────────

/// 连登状态快照（对应 Go `GrowthStreakState`）。
///
/// 一次 GET 供三处判据：连登天数（上报自检）、补签卡余额（决定要不要补签）、
/// 档位领取情况（决定要不要兑换）。`makeup_cards` 与 `redemption_status` 是
/// `data` 下的**同级**切片，不在 `streak` 里。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrowthStreakState {
    /// 连登计数。
    pub streak: GrowthStreak,
    /// 补签卡余额与上限。
    pub makeup_cards: MakeupCards,
    /// 档位领取情况。
    pub redemption: GrowthRedemptionStatus,
}

impl GrowthStreakState {
    /// 连登天数；无数据时为 0。
    ///
    /// ★ 调用方（调度器的 `read_streak_state`）用 `days() == 0` 判「上报 200 但
    /// 服务端静默丢弃」—— 缺 `userId` 时服务端返 200 却直接丢弃事件，连登不动。
    /// 因此本方法必须能如实返回 0，**不能**把「没有数据」也伪装成 0：
    /// 那种情况在上游解析处已经返回 `Err`，两者语义各自成立。
    pub fn days(&self) -> i64 {
        self.streak.days
    }
}

/// 连登计数（Go `GrowthStreakState.Streak`）。
///
/// Go 侧另有 `month_total_days`（本月累计天数），本项目不使用：达标判据一律以
/// `days` 与 `tiers[].days` 为准，暴露第二个「天数」只会诱导调用方拿错数。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct GrowthStreak {
    /// 连续登录天数。
    #[serde(default)]
    pub days: i64,
}

/// 补签卡余额（Go `GrowthMakeupCards`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct MakeupCards {
    /// 可用张数。
    #[serde(default)]
    pub balance: i64,
    /// 上限。
    #[serde(default)]
    pub max: i64,
}

/// 档位领取情况（Go `GrowthRedemptionStatus`）。
///
/// ★ **不要用 `remaining_days`**：实测它与 `tiers[].days` 不自洽（同一账号
/// `days=2` 时报 `remaining_days=3`，而 7d 档的 `days=7`、7-2=5），拿它当门槛
/// 会挑错档甚至跳过可领档位。故本类型**刻意不暴露**该字段，达标判据一律由
/// 调用方拿 `tiers[].days` 与 `streak.days` 直接比较。
///
/// 上游把领取状态拆成三个平铺字段（`tier_7d_status` / `tier_14d_status` /
/// `tier_28d_status`），这里在解析时归一成一份 `claimed` 列表 ——
/// 与 Go 的 `Claimed(tier)` 语义等价，但调用方不必知道上游的字段命名。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrowthRedemptionStatus {
    /// 档位表（上游实测按 `days` 升序，但调用方应按 `days` 取最大值而非下标）。
    pub tiers: Vec<GrowthTierSpec>,
    /// 本月**已领取**的档位名（如 `["7d"]`）。
    pub claimed: Vec<String>,
}

impl GrowthRedemptionStatus {
    /// 该档位本月是否已领取。
    ///
    /// 空档位名恒为 `false`（与 Go `Claimed` 的 `default: false` 一致）：
    /// 上游数据不完整时会出现 `tier == ""` 的条目，它不该被当成「已领」而
    /// 让调用方放弃整档，也不该被当成「可领」而发出必然被拒的请求 ——
    /// 调用方按空名先跳过即可。
    pub fn claimed(&self, tier: &str) -> bool {
        if tier.is_empty() {
            return false;
        }
        self.claimed.iter().any(|t| t == tier)
    }
}

/// 一个连登档位（Go `GrowthTierSpec`）。
///
/// 只保留判据需要的两个字段：`tier` 与 `days`。Go 侧另有
/// `credit`/`energy`/`cards`/`chances`（该档发放的奖励量），本项目不在上游层
/// 使用它们 —— 实际到账以后端回执 [`GrowthRedeemResult`] 为准，
/// 拿档位表的预告值记日志会与实际到账不一致。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct GrowthTierSpec {
    /// 档位名（`7d` / `14d` / `28d`）。
    #[serde(default)]
    pub tier: String,
    /// 达标所需连登天数。
    #[serde(default)]
    pub days: i64,
}

/// 宠物旅行状态（Go `TravelState`）。
///
/// Go 侧另有 `reward_credit`（到站奖励的预告值），本项目不暴露：实际到账以后端
/// [`Client::travel_claim`] 的返回值为准。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct TravelState {
    /// `idle` / `traveling` / `arrived`（见 [`TRAVEL_STATE_IDLE`] 等常量）。
    #[serde(default)]
    pub state: String,
    /// 在途/到站的记录号；`0` 表示没有记录（到站但无记录时不能领奖）。
    #[serde(default)]
    pub record_id: i64,
    /// 当日是否已用掉出发次数（自然日 00:00 CST 重置）。
    #[serde(default)]
    pub daily_limit_reached: bool,
}

/// 宠物信息（Go `Buddy`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct Buddy {
    /// 宠物 ID。
    #[serde(default)]
    pub id: i64,
    /// 宠物名。
    #[serde(default)]
    pub name: String,
}

/// 开学季任务条目（Go `SchoolTask`）。
///
/// 只保留调度需要的三个字段：`code`（领取路径参数）、`status`（决定是否领取）、
/// `reward_credit`（日志用）。Go 侧另有 `progress`/`target_count`/`task_type`，
/// 本项目不使用。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct SchoolTask {
    /// 任务码（来自 `task_code`）。
    #[serde(default, rename = "task_code")]
    pub code: String,
    /// `finished`（已达标待领）/ `unfinished`（未达标）/ `claimed`（已领）。
    #[serde(default)]
    pub status: String,
    /// 奖励积分（日志用）。
    #[serde(default)]
    pub reward_credit: i64,
}

/// 盲盒额度（Go `BlindboxQuota`）。
///
/// `affordable` 由**上游下发**，本地绝不自行计算（能量消耗规则可能变化）。
/// Go 侧另有 `balance`（总能量），本项目不使用：决定「开几个」的只有 `affordable`。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct BlindboxQuota {
    /// 当前能量还能开几个盒子。
    #[serde(default)]
    pub affordable: i64,
}

/// 开出来的一个盒子（Go `BlindboxItem`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlindboxItem {
    /// 物品名（优先 `instance.name`，回退 `template.name`）。
    pub name: String,
    /// 稀有度（同上取法；上游可能下发数字，见 [`rarity_text`]）。
    pub rarity: String,
}

/// 兑换回执（Go `GrowthRedeemResult`）。
///
/// Go 侧另有 `cards_overflow`（补签卡溢出被丢弃的数量），本项目不暴露：
/// 溢出是「卡已满」的提示，不影响调用方的成功/失败判定。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct GrowthRedeemResult {
    /// 本次到账积分。
    #[serde(default)]
    pub credit_granted: i64,
    /// 本次到账能量。
    #[serde(default)]
    pub energy_granted: i64,
    /// 本次到账补签卡。
    #[serde(default)]
    pub cards_granted: i64,
    /// 本次到账抽奖次数。
    #[serde(default)]
    pub chances_granted: i64,
}

/// 单次抽奖结果（Go `GrowthLotteryDrawResult`）。
///
/// Go 侧另有 `prize_code`，本项目不暴露：调用方只按奖品名/类型/积分数记日志。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrowthLotteryResult {
    /// 奖品名。
    pub prize_name: String,
    /// `credit` = 积分 / `physical` = 实物（实物需人工填收货地址，本项目不代填）。
    pub prize_type: String,
    /// 积分奖品到账数量。
    pub credit_amount: i64,
}

/// 热力图的一天（Go `HeatmapCell`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct HeatmapCell {
    /// 日期（上游为 `2026-09-16` 形式；判据只取前 10 字节）。
    #[serde(default)]
    pub date: String,
    /// 当日活跃分。
    #[serde(default)]
    pub score: i32,
}

// ──────────────────────────── 线上形状 ────────────────────────────

/// 连登状态的线上形状（三块同级切片）。
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct StreakStateWire {
    #[serde(default)]
    streak: GrowthStreak,
    #[serde(default)]
    makeup_cards: MakeupCards,
    #[serde(default)]
    redemption_status: RedemptionStatusWire,
}

/// 档位领取情况的线上形状：三个平铺状态字段 + 档位表。
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct RedemptionStatusWire {
    #[serde(default)]
    tier_7d_status: String,
    #[serde(default)]
    tier_14d_status: String,
    #[serde(default)]
    tier_28d_status: String,
    #[serde(default)]
    tiers: Vec<GrowthTierSpec>,
    /// ★ 刻意解析后丢弃：见 [`GrowthRedemptionStatus`] 的说明。
    #[serde(default, rename = "remaining_days")]
    _remaining_days: i64,
}

impl RedemptionStatusWire {
    /// 把三个平铺状态字段归一成已领档位列表。
    fn into_status(self) -> GrowthRedemptionStatus {
        const CLAIMED: &str = "claimed";
        let mut claimed = Vec::new();
        for (tier, status) in [
            ("7d", self.tier_7d_status),
            ("14d", self.tier_14d_status),
            ("28d", self.tier_28d_status),
        ] {
            if status == CLAIMED {
                claimed.push(tier.to_string());
            }
        }
        GrowthRedemptionStatus {
            tiers: self.tiers,
            claimed,
        }
    }
}

/// 热力图响应。
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct HeatmapResp {
    #[serde(default)]
    cells: Vec<HeatmapCell>,
}

/// 抽奖次数响应。
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct LotteryChancesResp {
    #[serde(default)]
    balance: i64,
}

/// 抽奖回执的线上形状（多一个不暴露的 `prize_code`）。
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct LotteryDrawWire {
    #[serde(default)]
    prize_name: String,
    #[serde(default)]
    prize_type: String,
    #[serde(default)]
    credit_amount: i64,
}

/// 礼包/补偿回执。
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ClaimCreditResp {
    #[serde(default)]
    credit: i64,
}

/// 开学季清单响应。
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct SchoolTasksResp {
    #[serde(default)]
    in_period: bool,
    #[serde(default)]
    tasks: Vec<SchoolTask>,
}

/// 宠物信息响应（`buddy` 可能是对象、`null` 或缺失）。
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct BuddyResp {
    #[serde(default)]
    buddy: serde_json::Value,
}

/// 开盒响应的一条：`instance` 与 `template` 互为回退。
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct BlindboxEntryWire {
    #[serde(default)]
    instance: BlindboxSideWire,
    #[serde(default)]
    template: BlindboxSideWire,
}

/// 开盒条目的单侧（`instance` 或 `template`）。
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct BlindboxSideWire {
    #[serde(default)]
    name: String,
    /// ★ 用 `Value` 而不是 `String`：上游稀有度**可能是数字也可能是字符串**，
    /// 直接解成 `String` 会让一个数字稀有度把整次开盒结果变成解析失败。
    #[serde(default)]
    rarity: serde_json::Value,
}

/// 开盒响应。
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct BlindboxOpenResp {
    #[serde(default)]
    results: Vec<BlindboxEntryWire>,
}

// ──────────────────────────── 域调用 ────────────────────────────

impl Client {
    /// 读取连登状态快照（对应 Go `GrowthStreakState`）。
    ///
    /// **解析失败返回 `Err`，不降级成零值** —— 调用方靠 `days() == 0` 区分
    /// 「上报成功但服务端静默丢弃」与「拿不到数据」，降级会让这条告警失效。
    pub async fn growth_streak_state(&self, a: &Auth) -> Result<GrowthStreakState> {
        let data = self
            .growth_json(a, reqwest::Method::GET, GROWTH_STREAK_PATH, None)
            .await?;
        let wire: StreakStateWire = serde_json::from_slice(&data)
            .map_err(|e| GatewayError::UpstreamParse(format!("streak state parse: {e}")))?;
        Ok(GrowthStreakState {
            streak: wire.streak,
            makeup_cards: wire.makeup_cards,
            redemption: wire.redemption_status.into_status(),
        })
    }

    /// 读取活跃热力图（定长自然日窗口）。
    ///
    /// 窗口外的日子**整格缺席**，这是 [`heatmap_day_score`] 的 `ok=false` 的
    /// 现实来源，不是异常。
    pub async fn growth_heatmap(&self, a: &Auth) -> Result<Vec<HeatmapCell>> {
        let data = self
            .growth_json(a, reqwest::Method::GET, GROWTH_HEATMAP_PATH, None)
            .await?;
        let resp: HeatmapResp = serde_json::from_slice(&data)
            .map_err(|e| GatewayError::UpstreamParse(format!("heatmap parse: {e}")))?;
        Ok(resp.cells)
    }

    /// 用一张补签卡补 `target_date`（`2026-09-15` 形式）。
    ///
    /// 只关心成功/失败，不解析回执。业务拒绝（无卡 / 无漏签 / 跨月）由调用方按
    /// [`is_makeup_no_card`] / [`is_makeup_not_broken`] / [`is_makeup_out_of_month`]
    /// 静默处理，或按 [`is_business_rejection`] 一并兜底。
    pub async fn use_makeup_card(&self, a: &Auth, target_date: &str) -> Result<()> {
        let body = serde_json::json!({ "target_date": target_date });
        self.growth_json(
            a,
            reqwest::Method::POST,
            GROWTH_MAKEUP_CARD_USE_PATH,
            Some(&body),
        )
        .await?;
        Ok(())
    }

    /// 兑换连登档位奖励（`tier` 取 `7d` / `14d` / `28d`）。
    ///
    /// `client_token` 是**幂等键，每次调用都必须新**（空串时自动生成）：
    /// 复用旧键可能被上游按幂等键去重吞掉本次领取，表现为「返回成功但没到账」。
    ///
    /// ★ 回执字段缺失**不视为失败**：调用方按 0 记日志即可。为此报错会让一次
    /// 已经生效的领取在本地被记成失败。
    pub async fn growth_redeem(
        &self,
        a: &Auth,
        tier: &str,
        client_token: &str,
    ) -> Result<GrowthRedeemResult> {
        let token = if client_token.is_empty() {
            growth_client_token(&format!("redeem-{tier}"))
        } else {
            client_token.to_string()
        };
        let body = serde_json::json!({ "tier": tier, "client_token": token });
        let data = self
            .growth_json(a, reqwest::Method::POST, GROWTH_REDEEM_PATH, Some(&body))
            .await?;
        // 与 Go 逐字一致：`if len(data) > 0 { _ = json.Unmarshal(...) }`。
        Ok(parse_or_default::<GrowthRedeemResult>(&data))
    }

    /// 查询抽奖次数余额。
    ///
    /// **解析失败返回 `Err`**（与 Go 一致）：抽奖是消耗性动作，拿不到确切余额时
    /// 不能靠「抽到报错为止」试探 —— 那会多打一次必然失败的写请求。
    pub async fn growth_lottery_chances(&self, a: &Auth) -> Result<i64> {
        let data = self
            .growth_json(a, reqwest::Method::GET, GROWTH_LOTTERY_CHANCES_PATH, None)
            .await?;
        let resp: LotteryChancesResp = serde_json::from_slice(&data)
            .map_err(|e| GatewayError::UpstreamParse(format!("lottery chances parse: {e}")))?;
        Ok(resp.balance)
    }

    /// 抽一次奖。
    ///
    /// `client_token` 每次必须新（空串时自动生成）：抽奖直接决定中奖归属，
    /// 复用幂等键可能让上游把第二次抽奖当成第一次的重放而丢弃。
    ///
    /// ★ 奖品字段缺失不视为失败（同 [`Client::growth_redeem`]）。
    pub async fn growth_lottery_draw(
        &self,
        a: &Auth,
        client_token: &str,
    ) -> Result<GrowthLotteryResult> {
        let token = if client_token.is_empty() {
            growth_client_token("draw")
        } else {
            client_token.to_string()
        };
        let body = serde_json::json!({ "client_token": token });
        let data = self
            .growth_json(a, reqwest::Method::POST, GROWTH_LOTTERY_DRAW_PATH, Some(&body))
            .await?;
        let wire: LotteryDrawWire = parse_or_default(&data);
        Ok(GrowthLotteryResult {
            prize_name: wire.prize_name,
            prize_type: wire.prize_type,
            credit_amount: wire.credit_amount,
        })
    }

    /// 领取新手礼包，返回到账积分（已领过返回 0）。
    ///
    /// 走 **billing 域**（`claim-gift` 是计费侧接口，不是成长活动）。
    pub async fn claim_gift(&self, a: &Auth) -> Result<i64> {
        self.claim_billing_credit(a, CLAIM_GIFT_PATH).await
    }

    /// 领取活动补偿，返回到账积分（活动未开启返回 0）。
    ///
    /// 走 **billing 域**。
    pub async fn claim_compensation(&self, a: &Auth) -> Result<i64> {
        self.claim_billing_credit(a, CLAIM_COMPENSATION_PATH).await
    }

    /// 礼包/补偿共用的领取实现。
    ///
    /// ★ 空对象 `{}` 作为请求体（不是 `nil`）：这两个接口要求 JSON body 存在。
    /// ★ 回执解析失败返回 `(0, Ok)`：与 Go 逐字一致（`_ = json.Unmarshal`），
    /// 缺失的 `credit` 意味着「本次没到账」而不是「请求失败」。
    async fn claim_billing_credit(&self, a: &Auth, path: &str) -> Result<i64> {
        let body = serde_json::json!({});
        let data = self
            .billing_json(a, reqwest::Method::POST, path, Some(&body))
            .await?;
        Ok(parse_or_default::<ClaimCreditResp>(&data).credit)
    }

    /// 查询盲盒额度。
    ///
    /// `affordable` 由上游下发，本地绝不自行计算。
    pub async fn growth_blindbox_quota(&self, a: &Auth) -> Result<BlindboxQuota> {
        let data = self
            .growth_json(a, reqwest::Method::GET, GROWTH_BLINDBOX_QUOTA_PATH, None)
            .await?;
        serde_json::from_slice(&data)
            .map_err(|e| GatewayError::UpstreamParse(format!("blindbox quota parse: {e}")))
    }

    /// 开 `count` 个盒子，返回**实际开出来的**物品。
    ///
    /// - `count <= 0` ⇒ `Err`（与 Go 同：`开盒数量必须为正`）；调用方应先查
    ///   [`Client::growth_blindbox_quota`] 再决定数量，不要靠 0 探测。
    /// - `count > `[`MAX_BLIND_BOX_OPENS`] ⇒ 夹到上限。
    /// - 返回值长度**可能小于**请求的 `count`：能量在中途耗尽时上游只回一部分，
    ///   调用方应以返回长度为准。返回空 `Vec` 而不是 `None`（与 Go 的空切片一致）。
    pub async fn growth_blindbox_open(
        &self,
        a: &Auth,
        count: i64,
    ) -> Result<Vec<BlindboxItem>> {
        if count <= 0 {
            return Err(GatewayError::Other("开盒数量必须为正".into()));
        }
        let count = count.min(MAX_BLIND_BOX_OPENS);
        let body = serde_json::json!({ "count": count });
        let data = self
            .growth_json(a, reqwest::Method::POST, GROWTH_BLINDBOX_OPEN_PATH, Some(&body))
            .await?;
        let resp: BlindboxOpenResp = serde_json::from_slice(&data)
            .map_err(|e| GatewayError::UpstreamParse(format!("blindbox open parse: {e}")))?;
        let mut out = Vec::with_capacity(resp.results.len());
        for r in resp.results {
            let mut name = r.instance.name;
            if name.is_empty() {
                name = r.template.name;
            }
            let mut rarity = rarity_text(&r.instance.rarity);
            if rarity.is_empty() {
                rarity = rarity_text(&r.template.rarity);
            }
            out.push(BlindboxItem { name, rarity });
        }
        Ok(out)
    }

    /// 拉取开学季任务清单，返回 `(in_period, tasks)`。
    ///
    /// 走 **billing 域**（`school.go` 文件头：portal 前缀 + billing 基址 + 信封可能不同）。
    ///
    /// `in_period == false` 是**正常状态**（活动已下线或尚未开始，此时 `tasks` 为空），
    /// 不是错误 —— 上游用 200 + 该标志表达「不在期」，报错会让调用方把常态记成故障。
    pub async fn school_tasks(&self, a: &Auth) -> Result<(bool, Vec<SchoolTask>)> {
        let data = self
            .billing_json(a, reqwest::Method::GET, SCHOOL_TASKS_PATH, None)
            .await?;
        let resp: SchoolTasksResp = serde_json::from_slice(&data)
            .map_err(|e| GatewayError::UpstreamParse(format!("school tasks parse: {e}")))?;
        Ok((resp.in_period, resp.tasks))
    }

    /// 领取一个开学季任务奖励。
    ///
    /// ★ `task_code` 为空 ⇒ **不发请求**直接 `Ok(())`（与 Go 一致）：空任务码会
    /// 拼出一个必然 404 的路径，而 404 与「任务未完成」难以区分。
    /// ★ 任务码在**路径**里，且**无请求体**。
    pub async fn school_claim(&self, a: &Auth, task_code: &str) -> Result<()> {
        if task_code.is_empty() {
            return Ok(());
        }
        let path = SCHOOL_CLAIM_PATH_FMT.replacen("{}", &path_escape(task_code), 1);
        self.billing_json(a, reqwest::Method::POST, &path, None)
            .await?;
        Ok(())
    }

    /// 领取国际版 IDE 试用加油包。
    ///
    /// 返回 `true` = 本次真的领到；`false` = 本周期已领过（业务码 14051），
    /// **两者都是成功**。
    ///
    /// 非国际版账号直接 `Err`（与 Go 同）：这是国际版独有的接口，
    /// 拿国服凭证打过去只会拿到一个无法解释的错误。
    pub async fn claim_trial(&self, a: &Auth) -> Result<bool> {
        if !a.is_intl() {
            return Err(GatewayError::Other("claim trial: only intl accounts".into()));
        }
        match self
            .billing_json(a, reqwest::Method::POST, TRIAL_PATH, None)
            .await
        {
            Ok(_) => Ok(true),
            Err(e) => {
                if let GatewayError::UpstreamError(ue) = &e {
                    if trial_already_err(&ue.msg) {
                        // 幂等成功：无需本地记账，每天试一次即可。
                        return Ok(false);
                    }
                }
                Err(e)
            }
        }
    }

    /// 读取宠物旅行状态。
    pub async fn travel_status(&self, a: &Auth) -> Result<TravelState> {
        let data = self
            .growth_json(a, reqwest::Method::GET, TRAVEL_STATUS_PATH, None)
            .await?;
        serde_json::from_slice(&data)
            .map_err(|e| GatewayError::UpstreamParse(format!("travel status parse: {e}")))
    }

    /// 派宠物出发（`location_id` 实测 1~4 收益/时长区间相同）。
    pub async fn travel_depart(&self, a: &Auth, location_id: i64) -> Result<()> {
        let body = serde_json::json!({ "location_id": location_id });
        self.growth_json(a, reqwest::Method::POST, TRAVEL_DEPART_PATH, Some(&body))
            .await?;
        Ok(())
    }

    /// 到站领奖，返回到账积分。
    ///
    /// ★ 奖励字段缺失**不视为失败**（与 Go 逐字一致）：调用方按 0 记日志即可。
    pub async fn travel_claim(&self, a: &Auth, record_id: i64) -> Result<i64> {
        let body = serde_json::json!({ "record_id": record_id });
        let data = self
            .growth_json(a, reqwest::Method::POST, TRAVEL_CLAIM_PATH, Some(&body))
            .await?;
        Ok(parse_or_default::<ClaimCreditResp>(&data).credit)
    }

    /// 读取宠物信息；未领养时为 `None`。
    ///
    /// `buddy` 字段可能是对象、`null`、空串或整个缺失 —— 后三种都表示「没有猫」，
    /// 是正常状态（调用方据此走领养流程），不是错误。只有「有内容但不是合法对象」
    /// 才报 `Err`。
    pub async fn buddy_info(&self, a: &Auth) -> Result<Option<Buddy>> {
        let data = self
            .growth_json(a, reqwest::Method::GET, BUDDY_INFO_PATH, None)
            .await?;
        let resp: BuddyResp = serde_json::from_slice(&data)
            .map_err(|e| GatewayError::UpstreamParse(format!("buddy info parse: {e}")))?;
        match resp.buddy {
            serde_json::Value::Null => Ok(None),
            // Go 侧 `json.RawMessage` 会原样留下字符串字面量，故 `""`/`"null"` 也要判。
            serde_json::Value::String(s) => {
                let t = s.trim();
                if t.is_empty() || t == "null" {
                    return Ok(None);
                }
                serde_json::from_str::<Buddy>(t)
                    .map(Some)
                    .map_err(|e| GatewayError::UpstreamParse(format!("buddy parse: {e}")))
            }
            v => serde_json::from_value::<Buddy>(v)
                .map(Some)
                .map_err(|e| GatewayError::UpstreamParse(format!("buddy parse: {e}"))),
        }
    }

    /// 领养宠物。
    ///
    /// ★ 请求体是**空对象** `{}`（不是 `nil`）：Go 侧写的就是
    /// `map[string]any{}`，改成无 body 会得到不同的答复。
    ///
    /// 对话门槛未达（HTTP 400 `first_buddy task not completed yet`）属预期行为，
    /// 调用方按 [`is_buddy_task_incomplete`] 判定后当日不再重试。
    pub async fn buddy_first(&self, a: &Auth) -> Result<()> {
        let body = serde_json::json!({});
        self.growth_json(a, reqwest::Method::POST, BUDDY_FIRST_PATH, Some(&body))
            .await?;
        Ok(())
    }

    /// 同意领养协议（必须在 [`Client::buddy_first`] 之前）。
    pub async fn buddy_agreement(&self, a: &Auth) -> Result<()> {
        let body = serde_json::json!({ "agree": true });
        self.growth_json(a, reqwest::Method::POST, BUDDY_AGREEMENT_PATH, Some(&body))
            .await?;
        Ok(())
    }
}

// ──────────────────────────── 判据函数 ────────────────────────────

/// 取热力图上某天的活跃分，返回 `(score, ok)`。
///
/// ★★ `ok == false` 表示「**没有该日格 = 无判据**」，**不是** `score == 0`。
/// 活跃地图是**定长自然日窗口**（实测 365 格），窗口外的日期整格缺席；
/// 把缺席当成 0 分会对窗口外的日期发起必然失败的补签，且失败原因
///（`only current month makeup allowed` 之外的 404/400）会掩盖真正的问题。
///
/// 比较只取前 10 字节（上游日期带时间后缀时仍能对上），用 `get(..10)` 而非
/// 直接切片：非 ASCII 日期（不该出现，但可能来自代理注入）不会 panic。
pub fn heatmap_day_score(cells: &[HeatmapCell], date: &str) -> (i32, bool) {
    for c in cells {
        if let Some(head) = c.date.get(..10) {
            if head == date {
                return (c.score, true);
            }
        }
    }
    (0, false)
}

/// 把本机时刻换算成上游业务时区（CST，UTC+8）的朴素时刻。
///
/// ★ 用「UTC 朴素时刻 + 8 小时」而不是 `FixedOffset::east_opt(8 * 3600)`：
/// `east_opt` 返回 `Option`，取 `Some` 必须 `expect`/`unwrap`，而本文件的生产
/// 路径要求**零 panic 出口**（同仓 `classify::upstream_zone` 用了 `expect`，
/// 那是既有代码；这里不复制那个写法）。中国无夏令时，UTC+8 是恒定偏移，
/// 两种写法结果逐位相同。
///
/// 固定 +8 而不用 `Asia/Shanghai`：Windows / 精简镜像缺 tzdata 时
/// `LoadLocation` 会失败，而中国无夏令时，固定偏移完全等价。
fn growth_cst(now: DateTime<Local>) -> NaiveDateTime {
    now.naive_utc() + chrono::Duration::hours(8)
}

/// 「昨日」的 CST 日期（`2026-09-15` 形式）。
///
/// 与 Go `GrowthYesterday` 等价：先转 CST 再减一天。
/// Go 用 `AddDate(0, 0, -1)`（日历减法），这里用 `- Duration::days(1)`
/// （24 小时减法）—— CST 无夏令时，两者恒等。
pub fn growth_yesterday(now: DateTime<Local>) -> String {
    (growth_cst(now) - chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string()
}

/// CST 下的「今天」是否是 1 号（= 昨日必然在上个月，补签必被拒）。
///
/// ★ 调用方必须把它**放在网络请求之前**：1 号那天「昨日」必然跨月，补签请求
/// 100% 会被上游拒绝，先判可以省掉每日一次的 heatmap 请求。
pub fn growth_makeup_out_of_month(now: DateTime<Local>) -> bool {
    growth_cst(now).day() == 1
}

/// 是否为「领养门槛未达」（HTTP 400 + `first_buddy task not completed yet`）。
///
/// 与 Go `IsBuddyTaskIncomplete` 逐字一致：**必须**同时满足 400 与标记命中 ——
/// 只看标记会把 5xx 上游故障也判成「门槛未达」而当日不再重试。
pub fn is_buddy_task_incomplete(err: &GatewayError) -> bool {
    let GatewayError::UpstreamError(ue) = err else {
        return false;
    };
    if ue.status != 400 {
        return false;
    }
    ue.msg.to_lowercase().contains(BUDDY_TASK_INCOMPLETE_MARKER)
}

/// 是否为「上游业务拒绝」（有明确答复的 4xx），即**常态**、应当静默。
///
/// 与 Go `IsBusinessRejection` 逐字一致，含一条实测踩出来的例外：
///
/// ★ **401 一律不算业务拒绝**。实测（真实账号，失效 token 调 growth 端点）
/// 上游返回的是 openresty 的 **401 HTML 错误页**，内容里没有 `12153`，
/// 于是 [`crate::upstream::classify`] 把它判成 `ErrClient` —— 若按
/// 「4xx = 业务拒绝」一刀切静默，表现就是「token 已死但任务永不吭声」。
///
/// 传输层/解析层失败没有 HTTP 状态，同样返回 `false`（必须记日志）。
pub fn is_business_rejection(err: &GatewayError) -> bool {
    let GatewayError::UpstreamError(ue) = err else {
        return false;
    };
    if ue.status == 401 {
        return false;
    }
    ue.kind == ErrKind::Client
}

/// 补签是否被拒为「没有补签卡余额」（403）。
///
/// 属**正常态**：补签卡是连登奖励发的，新号 / 刚用完的号本来就没有。
pub fn is_makeup_no_card(err: &GatewayError) -> bool {
    is_err_marker(err, 403, MAKEUP_NO_CARD_MARKERS)
}

/// 补签是否被拒为「该日并未漏签」（400）。
///
/// 属**正常态**：说明这一天其实已经打过卡，无需补。
pub fn is_makeup_not_broken(err: &GatewayError) -> bool {
    is_err_marker(err, 400, MAKEUP_NOT_BROKEN_MARKERS)
}

/// 补签是否被拒为「跨月」（400）。
///
/// 属**正常态**：上游只允许补当月，调用方本应在月初就跳过
/// （见 [`growth_makeup_out_of_month`]）。
pub fn is_makeup_out_of_month(err: &GatewayError) -> bool {
    is_err_marker(err, 400, MAKEUP_OUT_OF_MONTH_MARKERS)
}

/// 兑换是否被拒为「本月已领 / 重复领取」（409 或 408）。
pub fn is_redeem_already_claimed(err: &GatewayError) -> bool {
    is_err_marker(err, 409, REDEEM_ALREADY_MARKERS)
        || is_err_marker(err, 408, REDEEM_ALREADY_MARKERS)
}

/// 兑换是否被拒为「连登天数不足」（403）。
pub fn is_redeem_not_enough_days(err: &GatewayError) -> bool {
    is_err_marker(err, 403, REDEEM_NOT_ENOUGH_MARKERS)
}

/// 抽奖是否被拒为「无次数」（400）。
pub fn is_lottery_no_chance(err: &GatewayError) -> bool {
    is_err_marker(err, 400, LOTTERY_NO_CHANCE_MARKERS)
}

/// 抽奖是否被拒为「未开启」（400）。
pub fn is_lottery_disabled(err: &GatewayError) -> bool {
    is_err_marker(err, 400, LOTTERY_DISABLED_MARKERS)
}

/// trial 的答复是否表示「本周期已领过」。
///
/// ★ 刻意**不判状态码**：两个指纹以不同状态抵达（见 [`TRIAL_ALREADY_MARKERS`]），
/// 加了状态判据就会漏掉其中一种，表现为「已领过被当成真错误」每天报一次。
/// 大小写敏感（`contains`，不 fold）。
fn trial_already_err(msg: &str) -> bool {
    TRIAL_ALREADY_MARKERS.iter().any(|m| msg.contains(m))
}

/// 「指定状态 + 任一标记命中」的通用判定（对应 Go `isErrMarker`）。
///
/// 标记用**双边小写后子串匹配**而不是全等：上游把同一句话包在
/// `msg=...` / JSON 片段里，全等几乎永远匹配不上。中文关键词不受
/// `to_lowercase` 影响，仍逐字命中。
fn is_err_marker(err: &GatewayError, status: u16, markers: &[&str]) -> bool {
    let GatewayError::UpstreamError(ue) = err else {
        return false;
    };
    if ue.status != status {
        return false;
    }
    let lower = ue.msg.to_lowercase();
    markers.iter().any(|m| lower.contains(&m.to_lowercase()))
}

/// 反序列化；失败时返回默认值（对应 Go 的 `_ = json.Unmarshal`）。
///
/// 用于「回执字段缺失不视为失败」的四条路径（兑换、抽奖、旅行领奖、礼包）。
fn parse_or_default<T: serde::de::DeserializeOwned + Default>(data: &[u8]) -> T {
    if data.is_empty() {
        return T::default();
    }
    serde_json::from_slice(data).unwrap_or_default()
}

/// 生成每次唯一的幂等键 `<prefix>-<32hex>`（对应 Go `growthClientToken`）。
///
/// 用 `OsRng` 而不是时间戳/计数器：幂等键直接决定「本次领取有没有生效」，
/// 可预测或可碰撞的键会让上游把两次不同的领取判成同一次。
/// 取不到熵时退回纳秒时间戳的十六进制（与 Go 的回退一致）——
/// 此时唯一性变弱但**仍然每次不同**，比直接失败更可取。
fn growth_client_token(prefix: &str) -> String {
    let mut buf = [0u8; 16];
    if rand::rngs::OsRng.try_fill_bytes(&mut buf).is_ok() {
        return format!("{prefix}-{}", hex::encode(buf));
    }
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    format!("{prefix}-{nanos:x}")
}

/// 把盲盒稀有度字段转成字符串（对应 Go `rarityText`）。
///
/// ★ 上游的稀有度**可能是数字也可能是字符串**（参考实现直接用 `%s` 打印，
/// 两种都能出）。直接按 `String` 反序列化会在遇到数字时让**整个**开盒结果
/// 解析失败 —— 一次已经生效的开盒会被本地记成失败。
///
/// 数字按 Go 的两种格式输出：整数值走十进制（不要显示成 `3.000000`），
/// 非整数走最短往返表示。
fn rarity_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return i.to_string();
            }
            if let Some(u) = n.as_u64() {
                return u.to_string();
            }
            match n.as_f64() {
                Some(f) if f.is_finite() => {
                    if f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 {
                        (f as i64).to_string()
                    } else {
                        format!("{f}")
                    }
                }
                _ => String::new(),
            }
        }
        // 数组/对象不是稀有度的合法形状，与 Go 的 `default:` 一致返回空串。
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::classify::UpstreamError;
    // `chrono::TimeZone` 由 `at()` 内部按需 `use`（本模块只有它用到）。

    fn upstream_err(status: u16, msg: &str) -> GatewayError {
        GatewayError::UpstreamError(UpstreamError {
            kind: ErrKind::Client,
            status,
            msg: msg.into(),
        })
    }

    /// 12153（登录态失效）类错误：`kind` 是 `SessionDead` 而不是 `Client`。
    fn session_dead_err(status: u16, msg: &str) -> GatewayError {
        GatewayError::UpstreamError(UpstreamError {
            kind: ErrKind::SessionDead,
            status,
            msg: msg.into(),
        })
    }

    fn cell(date: &str, score: i32) -> HeatmapCell {
        HeatmapCell {
            date: date.into(),
            score,
        }
    }

    /// 构造一个「**CST（+08:00）墙上时间是这组数字**」的时刻，再换算成 `Local`。
    ///
    /// ★★ 2026-10-09：此前叫 `at` 且用 `Local.with_ymd_and_hms(...)` —— 即按
    /// **运行机**时区解释这组数字。而本模块的被测函数
    /// （`growth_yesterday` / `growth_makeup_out_of_month`，都经 `growth_cst`）
    /// **一律按 CST 判日** ⇒ CI 的 UTC runner 上断言全错
    ///（本地是 CST，所以这条一直没在开发机上暴露）。
    ///
    /// 改成先按 +08:00 构造再换算：得到的是**同一个瞬时**，
    /// 无论运行机在哪个时区，CST 判定结果都不变。
    ///
    /// ⚠ 名字刻意从 `at` 改成 `cst`：本模块里**每个**调用方都是 CST 语义，
    /// 名字带上口径，避免以后有人当成「本地时刻」误用。
    fn cst(y: i32, m: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        use chrono::TimeZone;
        chrono::FixedOffset::east_opt(8 * 3600)
            .expect("+8h")
            .with_ymd_and_hms(y, m, d, h, mi, 0)
            .single()
            .expect("CST 时刻唯一")
            .with_timezone(&Local)
    }

    // ── 热力图判据 ──

    #[test]
    fn heatmap_day_score_distinguishes_absent_from_zero() {
        let cells = vec![cell("2026-09-15", 0), cell("2026-09-16", 12)];
        // 命中且分数为 0 ⇒ (0, true)：**有**该日格，确实是漏签。
        assert_eq!(heatmap_day_score(&cells, "2026-09-15"), (0, true));
        assert_eq!(heatmap_day_score(&cells, "2026-09-16"), (12, true));
        // ★ 缺席 ⇒ (0, false)：无判据，绝不能当成漏签。
        assert_eq!(heatmap_day_score(&cells, "2026-01-01"), (0, false));
        assert_eq!(heatmap_day_score(&[], "2026-09-15"), (0, false));
    }

    #[test]
    fn heatmap_day_score_compares_first_ten_bytes() {
        let cells = vec![cell("2026-09-15T00:00:00+08:00", 3)];
        assert_eq!(heatmap_day_score(&cells, "2026-09-15"), (3, true));
        // 短于 10 字节的日期不参与匹配（不 panic）。
        let short = vec![cell("2026", 9)];
        assert_eq!(heatmap_day_score(&short, "2026"), (0, false));
    }

    // ── CST 日历判据 ──

    #[test]
    fn yesterday_is_cst_calendar_day() {
        assert_eq!(growth_yesterday(cst(2026, 9, 20, 23, 30)), "2026-09-19");
        assert_eq!(growth_yesterday(cst(2026, 9, 21, 0, 30)), "2026-09-20");
        // 跨月：9 月 1 日的昨日是 8 月 31 日。
        assert_eq!(growth_yesterday(cst(2026, 9, 1, 9, 0)), "2026-08-31");
        // 跨年。
        assert_eq!(growth_yesterday(cst(2027, 1, 1, 9, 0)), "2026-12-31");
    }

    #[test]
    fn makeup_out_of_month_only_on_the_first() {
        assert!(growth_makeup_out_of_month(cst(2026, 9, 1, 0, 5)));
        assert!(growth_makeup_out_of_month(cst(2026, 9, 1, 23, 59)));
        assert!(!growth_makeup_out_of_month(cst(2026, 9, 2, 0, 1)));
        assert!(!growth_makeup_out_of_month(cst(2026, 9, 30, 12, 0)));
    }

    // ── 业务答复判定 ──

    #[test]
    fn business_rejection_excludes_401() {
        assert!(is_business_rejection(&upstream_err(400, "unknown tier")));
        assert!(is_business_rejection(&upstream_err(403, "not enough")));
        // ★ 实测：失效 token 得到 401 HTML 错误页，kind 也是 Client —— 必须报出来。
        assert!(!is_business_rejection(&upstream_err(401, "<html>401</html>")));
        // 传输/解析层没有 HTTP 状态。
        assert!(!is_business_rejection(&GatewayError::Transport("reset".into())));
        assert!(!is_business_rejection(&GatewayError::UpstreamParse("bad".into())));
    }

    /// 12153（登录态失效）**不是**业务拒绝 —— 它是必须报警的故障。
    ///
    /// ★ 与 task-24 任务书写法不一致，此处以 Go 为准：Go `growth_map_test.go:333`
    /// 的表项「401 登录态失效（12153）」期望值是 **false**。两条路径都会得到 false：
    /// 401 被状态码排除；即便状态码不是 401，`kind` 也会是 `SessionDead` 而非
    /// `Client`。任务书那一行若按字面理解（「对 12153 必须返回 true」）会与 Go
    /// 相反，故在此显式钉住正确值。
    #[test]
    fn business_rejection_excludes_session_dead() {
        assert!(!is_business_rejection(&upstream_err(
            401,
            "Offline user session not found"
        )));
        assert!(!is_business_rejection(&session_dead_err(
            401,
            "code=12153 msg=Offline user session not found"
        )));
        // 即便状态码被上游写成 200，SessionDead 类也不属于业务答复。
        assert!(!is_business_rejection(&session_dead_err(200, "12153")));
    }

    /// 盲盒额度：`affordable` 直接透传上游下发值，本地绝不自行推算。
    #[test]
    fn blindbox_quota_passes_affordable_through() {
        assert_eq!(MAX_BLIND_BOX_OPENS, 5, "Go MaxBlindboxOpens 就是 5");
        // 上游同时下发 balance（总能量）与 affordable（本次可开）；后者与前者
        // 不构成任何本地可推算的关系，只认 affordable（多出的 balance 被忽略）。
        let resp: BlindboxQuota =
            serde_json::from_str(r#"{"balance":9,"affordable":3}"#).expect("额度应可解析");
        assert_eq!(resp.affordable, 3);
        // 字段缺失 ⇒ 0（调用方据此静默跳过，不发开盒请求）。
        let empty: BlindboxQuota = serde_json::from_str("{}").expect("空对象合法");
        assert_eq!(empty.affordable, 0);
    }

    #[test]
    fn redeem_markers_require_exact_status() {
        assert!(is_redeem_already_claimed(&upstream_err(409, "already claimed")));
        assert!(is_redeem_already_claimed(&upstream_err(408, "重复领取")));
        assert!(is_redeem_not_enough_days(&upstream_err(
            403,
            "连续登录天数不足，请继续打卡或使用补签卡"
        )));
        // 状态不对 ⇒ 不认（403 的「天数不足」不能被当成「已领」）。
        assert!(!is_redeem_already_claimed(&upstream_err(403, "already claimed")));
        assert!(!is_redeem_not_enough_days(&upstream_err(409, "not enough")));
    }

    #[test]
    fn lottery_markers_are_case_folded() {
        assert!(is_lottery_no_chance(&upstream_err(
            400,
            "Insufficient Lottery Chance Balance"
        )));
        assert!(is_lottery_disabled(&upstream_err(400, "Lottery Disabled")));
        assert!(!is_lottery_no_chance(&upstream_err(400, "lottery disabled")));
    }

    #[test]
    fn buddy_incomplete_requires_400() {
        let msg = "first_buddy task not completed yet";
        assert!(is_buddy_task_incomplete(&upstream_err(400, msg)));
        // ★ 5xx 上游故障不能被当成「门槛未达」而当日放弃重试。
        assert!(!is_buddy_task_incomplete(&upstream_err(500, msg)));
        assert!(!is_buddy_task_incomplete(&upstream_err(400, "other")));
        assert!(!is_buddy_task_incomplete(&GatewayError::Transport(msg.into())));
    }

    /// 补签的三种业务拒绝（对应 Go `growth_map_test.go:264-308`）。
    ///
    /// ★ 三条**必须互不误判**：它们都是 4xx，只按状态码分不开，
    /// 区别全在文案上 —— 混判会让调度侧把「该日无漏签」当成「无卡」而
    /// 反复尝试，或把「跨月」当成可重试而每天多打一次上游。
    #[test]
    fn makeup_markers_are_disjoint() {
        // 实测三例（状态 + 文案逐字来自上游）。
        let no_card = upstream_err(403, r#"{"code":403,"msg":"no makeup card balance"}"#);
        let not_broken = upstream_err(
            400,
            r#"{"code":400,"msg":"date is not broken, no makeup needed"}"#,
        );
        let out_of_month = upstream_err(
            400,
            r#"{"code":400,"msg":"only current month makeup allowed"}"#,
        );

        assert!(is_makeup_no_card(&no_card));
        assert!(is_makeup_not_broken(&not_broken));
        assert!(is_makeup_out_of_month(&out_of_month));

        // 交叉否定：任一条不能命中另两条。
        assert!(!is_makeup_not_broken(&no_card) && !is_makeup_out_of_month(&no_card));
        assert!(!is_makeup_no_card(&not_broken) && !is_makeup_out_of_month(&not_broken));
        assert!(!is_makeup_no_card(&out_of_month) && !is_makeup_not_broken(&out_of_month));

        // 三种都是业务拒绝（否则调度侧为每天的常态刷 WARN）。
        for e in [&no_card, &not_broken, &out_of_month] {
            assert!(is_business_rejection(e));
        }
    }

    /// 补签判据同样要求状态码精确（文案对但状态错 ⇒ 不认）。
    #[test]
    fn makeup_markers_require_exact_status() {
        // 403 的文案配 400：不是「无卡」。
        assert!(!is_makeup_no_card(&upstream_err(400, "no makeup card balance")));
        // 400 的文案配 403：不是「未漏签」/「跨月」。
        assert!(!is_makeup_not_broken(&upstream_err(403, "date is not broken")));
        assert!(!is_makeup_out_of_month(&upstream_err(
            403,
            "only current month makeup allowed"
        )));
        // 传输层错误没有状态码。
        assert!(!is_makeup_no_card(&GatewayError::Transport(
            "no makeup card balance".into()
        )));
    }

    #[test]
    fn trial_already_matches_both_fingerprints() {
        // HTTP 200 + 业务码（unwrap_envelope 拼出的形状）。
        assert!(trial_already_err("code=14051 msg=already claimed"));
        // HTTP ≥ 400 的原始响应体。
        assert!(trial_already_err(r#"{"code":14051,"msg":"已领取"}"#));
        assert!(!trial_already_err("code=14050 msg=other"));
        assert!(!trial_already_err(""));
    }

    // ── 档位判定 ──

    #[test]
    fn claimed_list_maps_to_go_switch() {
        let rs = GrowthRedemptionStatus {
            tiers: vec![],
            claimed: vec!["14d".into()],
        };
        assert!(rs.claimed("14d"));
        assert!(!rs.claimed("7d"));
        assert!(!rs.claimed("28d"));
        // 空档位名恒 false（Go 的 default 分支）。
        assert!(!GrowthRedemptionStatus::default().claimed(""));
    }

    // ── 稀有度转换 ──

    #[test]
    fn rarity_text_matches_go_any_handling() {
        use serde_json::json;
        assert_eq!(rarity_text(&json!(null)), "");
        assert_eq!(rarity_text(&json!("SSR")), "SSR");
        // 整数值不能显示成 "3.000000"。
        assert_eq!(rarity_text(&json!(3)), "3");
        assert_eq!(rarity_text(&json!(0)), "0");
        assert_eq!(rarity_text(&json!(true)), "true");
        assert_eq!(rarity_text(&json!(false)), "false");
        assert_eq!(rarity_text(&json!({"a":1})), "");
        assert_eq!(rarity_text(&json!([1])), "");
    }

    // ── 幂等键 ──

    #[test]
    fn client_token_is_prefixed_and_unique() {
        let a = growth_client_token("redeem-7d");
        let b = growth_client_token("redeem-7d");
        assert!(a.starts_with("redeem-7d-"), "前缀必须保留: {a}");
        assert_ne!(a, b, "每次调用必须新（幂等键复用会让上游吞掉领取）");
        // 32 位十六进制（回退路径除外，仅断言非空后缀）。
        assert!(a.len() > "redeem-7d-".len());
        assert!(growth_client_token("draw").starts_with("draw-"));
    }

    // ── 响应解析 ──

    #[test]
    fn streak_state_wire_derives_claimed_and_drops_remaining_days() {
        let raw = r#"{
            "streak":{"days":2,"month_total_days":8},
            "makeup_cards":{"balance":0,"max":4},
            "redemption_status":{
                "tier_7d_status":"claimed",
                "tier_14d_status":"unlocked",
                "remaining_days":3,
                "tiers":[
                    {"tier":"7d","days":7,"credit":0,"energy":2,"cards":1,"chances":1},
                    {"tier":"14d","days":14,"credit":100,"energy":5,"cards":1,"chances":1}
                ]
            },
            "timezone":"Asia/Shanghai","launch_date":"2026-06-17"
        }"#;
        let wire: StreakStateWire = serde_json::from_str(raw).expect("真实信封应可解析");
        let st = GrowthStreakState {
            streak: wire.streak,
            makeup_cards: wire.makeup_cards,
            redemption: wire.redemption_status.into_status(),
        };
        assert_eq!(st.days(), 2);
        assert_eq!(st.makeup_cards.balance, 0);
        assert_eq!(st.makeup_cards.max, 4);
        assert_eq!(st.redemption.tiers.len(), 2);
        assert_eq!(st.redemption.tiers[1].days, 14);
        // 只有 7d 标了 claimed。
        assert!(st.redemption.claimed("7d"));
        assert!(!st.redemption.claimed("14d"));
    }

    #[test]
    fn streak_state_parses_empty_object_as_zero_days() {
        // 字段全缺 ⇒ 全零值（Go 的 json.Unmarshal 语义）。
        let wire: StreakStateWire = serde_json::from_str("{}").expect("空对象合法");
        let st = GrowthStreakState {
            streak: wire.streak,
            makeup_cards: wire.makeup_cards,
            redemption: wire.redemption_status.into_status(),
        };
        assert_eq!(st.days(), 0, "真 0 必须如实返回（调度器靠它判静默丢弃）");
        assert!(st.redemption.claimed.is_empty());
    }

    #[test]
    fn school_task_maps_task_code_to_code() {
        let raw = r#"{"in_period":true,"tasks":[
            {"task_code":"s1","status":"finished","reward_credit":50,"progress":1,"target_count":1,"task_type":"single"}
        ]}"#;
        let resp: SchoolTasksResp = serde_json::from_str(raw).expect("清单应可解析");
        assert!(resp.in_period);
        assert_eq!(resp.tasks.len(), 1);
        assert_eq!(resp.tasks[0].code, "s1");
        assert_eq!(resp.tasks[0].status, SCHOOL_TASK_STATUS_FINISHED);
        assert_eq!(resp.tasks[0].reward_credit, 50);
    }

    #[test]
    fn school_tasks_out_of_period_is_not_an_error() {
        let resp: SchoolTasksResp =
            serde_json::from_str(r#"{"in_period":false,"tasks":[]}"#).expect("不在期是正常状态");
        assert!(!resp.in_period);
        assert!(resp.tasks.is_empty());
        // tasks 缺失等价于空清单。
        let resp: SchoolTasksResp = serde_json::from_str(r#"{"in_period":false}"#).expect("缺字段合法");
        assert!(resp.tasks.is_empty());
    }

    #[test]
    fn blindbox_open_falls_back_from_instance_to_template() {
        let raw = r#"{"results":[
            {"instance":{"name":"猫粮","rarity":"R"},"template":{"name":"t","rarity":"T"}},
            {"instance":{"name":"","rarity":null},"template":{"name":"备用名","rarity":3}},
            {"instance":{"name":"整名","rarity":""},"template":{"name":"不应采用","rarity":"SSR"}}
        ]}"#;
        let resp: BlindboxOpenResp = serde_json::from_str(raw).expect("含数字稀有度也必须能解析");
        assert_eq!(resp.results.len(), 3);
        assert_eq!(resp.results[0].instance.name, "猫粮");
        assert_eq!(rarity_text(&resp.results[0].instance.rarity), "R");
        // instance.name 为空 ⇒ 回退 template.name；数字稀有度 ⇒ 十进制。
        assert!(resp.results[1].instance.name.is_empty());
        assert_eq!(resp.results[1].template.name, "备用名");
        assert_eq!(rarity_text(&resp.results[1].template.rarity), "3");
        // instance.rarity 为空串 ⇒ 回退 template.rarity。
        assert_eq!(resp.results[2].template.name, "不应采用");
        assert_eq!(rarity_text(&resp.results[2].template.rarity), "SSR");
    }

    #[test]
    fn buddy_wire_accepts_object_null_and_absent() {
        let obj: BuddyResp =
            serde_json::from_str(r#"{"buddy":{"id":7,"name":"cat"}}"#).expect("对象合法");
        assert_eq!(obj.buddy.get("id").and_then(|v| v.as_i64()), Some(7));
        let nul: BuddyResp = serde_json::from_str(r#"{"buddy":null}"#).expect("null 合法");
        assert!(nul.buddy.is_null());
        let absent: BuddyResp = serde_json::from_str("{}").expect("缺失合法");
        assert!(absent.buddy.is_null());
    }

    // ── 假上游端到端 ──

    /// 假上游收到的一次请求。
    #[derive(Debug, Clone)]
    struct Seen {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl Seen {
        fn header_value(&self, name: &str) -> Option<&str> {
            let want = name.to_ascii_lowercase();
            self.headers
                .iter()
                .find(|(k, _)| k.to_ascii_lowercase() == want)
                .map(|(_, v)| v.as_str())
        }

        fn has_header(&self, name: &str) -> bool {
            self.header_value(name).is_some()
        }
    }

    fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    /// 起一个假上游（`std::net::TcpListener`；tokio 的 `net` feature 未启用）。
    fn spawn_fake_upstream(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<Seen>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&seen);
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let mut idx = 0usize;
            for stream in listener.incoming() {
                let mut s = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let mut raw: Vec<u8> = Vec::new();
                let mut buf = [0u8; 4096];
                while find_subslice(&raw, b"\r\n\r\n").is_none() {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => raw.extend_from_slice(&buf[..n]),
                    }
                }
                let header_end = match find_subslice(&raw, b"\r\n\r\n") {
                    Some(p) => p + 4,
                    None => raw.len(),
                };
                let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
                let mut lines = head.split("\r\n");
                let req_line = lines.next().unwrap_or("");
                let mut parts = req_line.split(' ');
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let mut headers: Vec<(String, String)> = Vec::new();
                let mut content_length = 0usize;
                for l in lines {
                    if let Some((k, v)) = l.split_once(':') {
                        let (k, v) = (k.trim().to_string(), v.trim().to_string());
                        if k.eq_ignore_ascii_case("content-length") {
                            content_length = v.parse().unwrap_or(0);
                        }
                        headers.push((k, v));
                    }
                }
                while raw.len() < header_end + content_length {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => raw.extend_from_slice(&buf[..n]),
                    }
                }
                let body = String::from_utf8_lossy(&raw[header_end.min(raw.len())..]).to_string();
                sink.lock().unwrap().push(Seen {
                    method,
                    path,
                    headers,
                    body,
                });
                let (status, resp_body) = responses[idx.min(responses.len() - 1)];
                idx += 1;
                let reason = if status == 200 { "OK" } else { "Error" };
                let out = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{}",
                    resp_body.len(),
                    resp_body
                );
                let _ = s.write_all(out.as_bytes());
                let _ = s.flush();
            }
        });
        (base, seen)
    }

    fn account() -> Auth {
        Auth {
            access_token: "at-test".into(),
            uid: "uid-test".into(),
            enterprise_id: "ent-test".into(),
            domain: "https://copilot.tencent.com".into(),
            ..Default::default()
        }
    }

    fn intl_account() -> Auth {
        Auth {
            domain: "https://www.workbuddy.ai".into(),
            ..account()
        }
    }

    /// 把 chat 与 billing 两个域都指向假上游（school/trial 走 billing）。
    ///
    /// ★★ **必须同时覆盖国际版分支**：`chat_base`/`billing_base` 对国际版账号走的是
    /// `base_intl`（非空则用，否则回落 `BASE_INTL`），只看 `*_cn` 会让国际版用例
    /// **漏到真实域名**。本测试写第一版时就踩到了：`claim_trial` 的两条用例带着假
    /// token 打到了 `www.workbuddy.ai`，从真实上游拿回 401 HTML 页。
    /// 虽然假 token 无副作用，但「单元测试打真实上游」本身是不可接受的失败形态
    /// —— 一旦某天用例带上真实凭证，就会变成对生产账号的真实写操作。
    fn client_for(base: &str) -> Client {
        Client::new(crate::upstream::ClientConfig {
            chat_base_cn: base.to_string(),
            billing_base_cn: base.to_string(),
            base_intl: base.to_string(),
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn streak_state_reads_growth_domain_with_go_ua() {
        let (base, seen) = spawn_fake_upstream(vec![(
            200,
            r#"{"code":0,"data":{"streak":{"days":2},"makeup_cards":{"balance":1,"max":4},"redemption_status":{"tier_7d_status":"claimed","tiers":[{"tier":"7d","days":7}]}}}"#,
        )]);
        let client = client_for(&base);
        let st = client.growth_streak_state(&account()).await.unwrap();
        assert_eq!(st.days(), 2);
        assert_eq!(st.makeup_cards.balance, 1);
        assert!(st.redemption.claimed("7d"));

        let got = seen.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].method, "GET");
        assert_eq!(got[0].path, GROWTH_STREAK_PATH);
        // growth 域除任务清单外一律带 Go 默认 UA（改掉会变更指纹）。
        assert_eq!(got[0].header_value("user-agent"), Some("Go-http-client/1.1"));
        // ★ 小程序平台头**只**给 tasks 域的 accept/claim 用；growth 端点带上它
        // 会把请求混进另一套指纹（`X-Client-Platform` 与 no-UA 不是可叠加开关）。
        assert!(
            !got[0].has_header("x-client-platform"),
            "growth 端点不得带 X-Client-Platform，实际头: {:?}",
            got[0].headers
        );
    }

    #[tokio::test]
    async fn streak_state_parse_failure_is_an_error_not_zero() {
        let (base, _seen) = spawn_fake_upstream(vec![(200, r#"{"code":0,"data":{"streak":"oops"}}"#)]);
        let client = client_for(&base);
        let err = client.growth_streak_state(&account()).await.unwrap_err();
        assert!(
            matches!(err, GatewayError::UpstreamParse(_)),
            "解析失败必须是 Err，不能降级成 days=0（那会与「真 0」混淆）: {err:?}"
        );
    }

    #[tokio::test]
    async fn school_tasks_goes_to_billing_domain() {
        let (base, seen) = spawn_fake_upstream(vec![(
            200,
            r#"{"code":0,"data":{"in_period":true,"tasks":[{"task_code":"s1","status":"finished","reward_credit":50}]}}"#,
        )]);
        let client = client_for(&base);
        let (in_period, tasks) = client.school_tasks(&account()).await.unwrap();
        assert!(in_period);
        assert_eq!(tasks[0].code, "s1");

        let got = seen.lock().unwrap();
        assert_eq!(got[0].path, SCHOOL_TASKS_PATH);
        // 走 billing 域 ⇒ 与 chat 域同端口但基址不同，这里靠 path 前缀断言。
        assert!(got[0].path.starts_with("/portal/activity/school/"));
    }

    #[tokio::test]
    async fn school_claim_skips_empty_code_without_request() {
        let (base, seen) = spawn_fake_upstream(vec![(200, r#"{"code":0,"data":{}}"#)]);
        let client = client_for(&base);
        client.school_claim(&account(), "").await.unwrap();
        assert!(seen.lock().unwrap().is_empty(), "空任务码不得发请求");

        client.school_claim(&account(), "s1").await.unwrap();
        let got = seen.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].method, "POST");
        assert_eq!(got[0].path, "/portal/activity/school/tasks/s1/claim");
        assert!(got[0].body.is_empty(), "领取无请求体");
    }

    #[tokio::test]
    async fn claim_trial_treats_14051_as_idempotent_success() {
        // HTTP 200 + 业务码 14051（unwrap_envelope 拼成 `code=14051 msg=...`）。
        let (base, seen) = spawn_fake_upstream(vec![(200, r#"{"code":14051,"msg":"already"}"#)]);
        let client = client_for(&base);
        assert!(!client.claim_trial(&intl_account()).await.unwrap());

        let got = seen.lock().unwrap();
        assert_eq!(got[0].path, TRIAL_PATH);
        assert_eq!(got[0].method, "POST");
        assert!(got[0].body.is_empty(), "trial 请求体是 nil，不是空对象");
    }

    #[tokio::test]
    async fn claim_trial_treats_4xx_14051_body_as_idempotent_success() {
        let (base, _seen) =
            spawn_fake_upstream(vec![(400, r#"{"code":14051,"msg":"already claimed"}"#)]);
        let client = client_for(&base);
        assert!(
            !client.claim_trial(&intl_account()).await.unwrap(),
            "4xx 的原始 body 指纹也必须判成已领"
        );
    }

    #[tokio::test]
    async fn claim_trial_rejects_non_intl_account() {
        let (base, seen) = spawn_fake_upstream(vec![(200, r#"{"code":0,"data":{}}"#)]);
        let client = client_for(&base);
        let err = client.claim_trial(&account()).await.unwrap_err();
        assert!(matches!(err, GatewayError::Other(_)), "非国际版应直接拒绝");
        assert!(seen.lock().unwrap().is_empty(), "非国际版不得发请求");
    }

    #[tokio::test]
    async fn blindbox_open_rejects_non_positive_and_clamps_count() {
        let (base, seen) = spawn_fake_upstream(vec![(
            200,
            r#"{"code":0,"data":{"results":[{"instance":{"name":"a","rarity":"R"}}]}}"#,
        )]);
        let client = client_for(&base);
        assert!(client.growth_blindbox_open(&account(), 0).await.is_err());
        assert!(seen.lock().unwrap().is_empty(), "非法数量不得发请求");

        let items = client.growth_blindbox_open(&account(), 99).await.unwrap();
        assert_eq!(items.len(), 1, "返回实际开出的数量");
        let got = seen.lock().unwrap();
        assert_eq!(got[0].path, GROWTH_BLINDBOX_OPEN_PATH);
        // 夹到上限。
        assert!(
            got[0].body.contains(r#""count":5"#),
            "count 应夹到 {MAX_BLIND_BOX_OPENS}，实际 body: {}",
            got[0].body
        );
    }

    #[tokio::test]
    async fn buddy_info_maps_null_to_none() {
        let (base, _seen) = spawn_fake_upstream(vec![(200, r#"{"code":0,"data":{"buddy":null}}"#)]);
        let client = client_for(&base);
        assert!(client.buddy_info(&account()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn buddy_info_maps_object_to_some() {
        let (base, _seen) = spawn_fake_upstream(vec![(
            200,
            r#"{"code":0,"data":{"buddy":{"id":7,"name":"cat"}}}"#,
        )]);
        let client = client_for(&base);
        let b = client.buddy_info(&account()).await.unwrap().expect("应有猫");
        assert_eq!(b.id, 7);
        assert_eq!(b.name, "cat");
    }

    #[tokio::test]
    async fn buddy_first_sends_empty_object_body() {
        let (base, seen) = spawn_fake_upstream(vec![(200, r#"{"code":0,"data":{}}"#)]);
        let client = client_for(&base);
        client.buddy_first(&account()).await.unwrap();
        let got = seen.lock().unwrap();
        assert_eq!(got[0].path, BUDDY_FIRST_PATH);
        assert_eq!(got[0].body, "{}", "领养体必须是空对象（不是无 body）");
    }

    #[tokio::test]
    async fn travel_claim_tolerates_missing_reward_field() {
        let (base, seen) = spawn_fake_upstream(vec![(200, r#"{"code":0,"data":{}}"#)]);
        let client = client_for(&base);
        let reward = client.travel_claim(&account(), 42).await.unwrap();
        assert_eq!(reward, 0, "奖励字段缺失不视为失败");
        let got = seen.lock().unwrap();
        assert_eq!(got[0].path, TRAVEL_CLAIM_PATH);
        assert!(got[0].body.contains(r#""record_id":42"#));
    }

    #[tokio::test]
    async fn growth_redeem_generates_token_when_empty() {
        let (base, seen) = spawn_fake_upstream(vec![(
            200,
            r#"{"code":0,"data":{"credit_granted":100,"energy_granted":5,"cards_granted":1,"chances_granted":1}}"#,
        )]);
        let client = client_for(&base);
        let res = client.growth_redeem(&account(), "7d", "").await.unwrap();
        assert_eq!(res.credit_granted, 100);
        assert_eq!(res.chances_granted, 1);
        let got = seen.lock().unwrap();
        assert!(
            got[0].body.contains(r#""client_token":"redeem-7d-"#),
            "空 token 必须自动生成，实际 body: {}",
            got[0].body
        );
    }

    #[tokio::test]
    async fn growth_lottery_draw_tolerates_missing_fields() {
        let (base, _seen) = spawn_fake_upstream(vec![(200, r#"{"code":0,"data":{}}"#)]);
        let client = client_for(&base);
        let res = client.growth_lottery_draw(&account(), "").await.unwrap();
        assert_eq!(res.prize_name, "");
        assert_eq!(res.credit_amount, 0);
    }

    #[tokio::test]
    async fn growth_lottery_chances_parse_failure_is_error() {
        let (base, _seen) = spawn_fake_upstream(vec![(200, r#"{"code":0,"data":{"balance":"x"}}"#)]);
        let client = client_for(&base);
        assert!(
            client.growth_lottery_chances(&account()).await.is_err(),
            "次数余额拿不到必须是 Err（不能靠「抽到报错为止」试探）"
        );
    }

    #[tokio::test]
    async fn claim_gift_zeroes_on_parse_failure() {
        let (base, seen) = spawn_fake_upstream(vec![(200, r#"{"code":0,"data":"oops"}"#)]);
        let client = client_for(&base);
        assert_eq!(client.claim_gift(&account()).await.unwrap(), 0);
        let got = seen.lock().unwrap();
        assert_eq!(got[0].path, CLAIM_GIFT_PATH);
        assert_eq!(got[0].body, "{}", "礼包请求体是空对象");
    }

    #[tokio::test]
    async fn use_makeup_card_posts_target_date() {
        let (base, seen) = spawn_fake_upstream(vec![(200, r#"{"code":0,"data":{}}"#)]);
        let client = client_for(&base);
        client.use_makeup_card(&account(), "2026-09-15").await.unwrap();
        let got = seen.lock().unwrap();
        assert_eq!(got[0].path, GROWTH_MAKEUP_CARD_USE_PATH);
        assert!(got[0].body.contains(r#""target_date":"2026-09-15""#));
    }

    #[tokio::test]
    async fn heatmap_returns_cells() {
        let (base, seen) = spawn_fake_upstream(vec![(
            200,
            r#"{"code":0,"data":{"cells":[{"date":"2026-09-15","score":0},{"date":"2026-09-16","score":12}]}}"#,
        )]);
        let client = client_for(&base);
        let cells = client.growth_heatmap(&account()).await.unwrap();
        assert_eq!(cells.len(), 2);
        assert_eq!(heatmap_day_score(&cells, "2026-09-15"), (0, true));
        let got = seen.lock().unwrap();
        assert_eq!(got[0].path, GROWTH_HEATMAP_PATH);
    }
}
