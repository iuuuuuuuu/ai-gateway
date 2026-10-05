//! 成长任务清单 / 报名 / 领奖（对应 Go `internal/upstream/tasks.go` + `mp.go`）。
//!
//! # 端点（全部实测确认，2026-09-16，真实账号）
//!
//! | 操作 | 端点 | 口径 |
//! |---|---|---|
//! | 列表 | `GET {chatBase}/v2/activity/growth/tasks` | **no-UA** |
//! | 报名 | `POST {chatBase}/v2/activity/growth/tasks/accept` | 默认口径 / 小程序口径 |
//! | 领奖 | `POST {webBase}/activity/growth/tasks/{task_code}/claim` | **Web 域**、网页头族 |
//!
//! 领奖**必须走 Web 域**（`www.workbuddy.cn`）。CLI 域上形如
//! `/v2/activity/growth/tasks/reward/claim` 的路径不存在：实测恒返回 HTTP 400，
//! 与「任务未完成」的报错无法区分，极易被误判成「进度没达标」而反复重试上报 ——
//! 这是比端点写错本身更难排查的失败形态，故在此显式记录。
//!
//! # 语义要点（实测）
//!
//! - `accept` 是「报名」，本身不产生进度；但它**不是纯装饰** —— 未报名的任务
//!   上游把 `progress` 整个下发为 `null`（实测 16/18 个任务如此），只有报名后
//!   才会出现 `{current,target}`。因此「先报名再上报行为」是必要的顺序，
//!   否则进度无从回读，「上报 200 ≠ 计分」的自检也就失去了依据。
//! - `accept` 可幂等重放：重复报名返回 `status=already_accepted` 而非报错。
//! - `claim` 仅进度达标后可领；重复领返回 `already_claimed=true`（幂等）。
//!
//! # 为什么这些方法必须写在 `impl Client`
//!
//! [`Client`] 的 `http` / `chat` / `cfg` 是**私有字段**，私有可见性只覆盖
//! 定义它的模块及其子模块（兄弟模块 `upstream::tasks` **读不到**）。
//! 请求管道（[`Client::growth_json`] 等）留在 `client.rs`，本文件只按域拼路径与解析响应。

use std::collections::BTreeMap;

use crate::auth::Auth;
use crate::error::{GatewayError, Result};
use crate::upstream::client::Client;
use crate::upstream::desktop::WEB_UA;

/// growth 域任务列表路径。
pub const GROWTH_TASKS_LIST_PATH: &str = "/v2/activity/growth/tasks";
/// growth 域任务报名路径。
pub const GROWTH_TASKS_ACCEPT_PATH: &str = "/v2/activity/growth/tasks/accept";
/// 领奖路径**模板**（`{}` 处填任务码）：任务码在**路径**里，且基址是 Web 域。
///
/// 单独成常量而不是就地拼接：这条路径的「Web 域 + 路径参数」两个特征
/// 都是实测踩出来的，集中一处便于对照 Go `growthTaskClaimPathFmt`。
pub const GROWTH_TASK_CLAIM_PATH_FMT: &str = "/activity/growth/tasks/{}/claim";

// ──────────────────────────── 报名状态机 ────────────────────────────

/// 未报名。此时上游不下发 `progress`（为 `null`）。
pub const TASK_ACCEPT_NOT_ACCEPTED: &str = "not_accepted";
/// 已报名，尚无进度。进度开始被服务端跟踪。
pub const TASK_ACCEPT_ACCEPTED: &str = "accepted";
/// 已报名且进度已大于 0。
///
/// ★ 这是 2026-09-16 联调时才观察到的**第三个**状态：任务报名后进度一旦大于 0，
/// `accept_status` 就从 `accepted` 变成 `in_progress`。早期只登记了
/// `accepted`/`claimed` 两个值，导致 [`GrowthTask::needs_accept`] 用
/// `!= accepted` 判定时会把进行中的任务误判成「未报名」并重复报名。
pub const TASK_ACCEPT_IN_PROGRESS: &str = "in_progress";
/// 进度已达标、尚未领奖。
///
/// 实测（2026-09-18，校园日任务）：进度到 target 后 `accept_status` 变成
/// `completed`，此时 claim 才会成功。它与 `in_progress` 的区别是「够不够领」——
/// 只看 `current >= target` 也能判断，但**状态字段更权威**：上游可能下发
/// `current < target` 却已是 `completed`（如期数型任务的口径差异）。
pub const TASK_ACCEPT_COMPLETED: &str = "completed";
/// 奖励已领取。
pub const TASK_ACCEPT_CLAIMED: &str = "claimed";

/// 成长任务条目。
///
/// 字段名与上游 JSON 对齐（上游用 `reward_` 前缀）；对外只透出这个子集，
/// 避免把 `icon_url` / 弹窗按钮样式等纯展示字段带进日志与接口。
///
/// `omitempty` 逐字照抄 Go 的 struct tag —— 这个结构体**会被序列化**
/// （`/tasks/growth` 的 HTTP 响应），少一个 `skip_serializing_if` 就会让
/// 报文里多出一堆空字段。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GrowthTask {
    /// 任务码（上游 `task_code`，旧形状为 `code`）。
    pub code: String,
    /// 标题。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// 操作指引（含客户端跳转说明）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// 达成条件简述。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task_desc: String,
    /// 奖励积分（上游 `reward_credit`）。
    #[serde(default, rename = "reward_credit", skip_serializing_if = "is_zero_i64")]
    pub credit: i64,
    /// 奖励能量（上游 `reward_energy`）。
    #[serde(default, rename = "reward_energy", skip_serializing_if = "is_zero_i64")]
    pub energy: i64,
    /// 是否奖励伙伴。
    #[serde(default, skip_serializing_if = "is_false")]
    pub reward_buddy: bool,
    /// 是否带奖励。
    #[serde(default, skip_serializing_if = "is_false")]
    pub has_reward: bool,
    /// 任务类型。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task_type: String,
    /// 端标记（PC / 限定 / 限量）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tag: String,
    /// `workbuddy://` 跳转协议。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub jump_url: String,
    /// 是否未解锁。
    #[serde(default, skip_serializing_if = "is_false")]
    pub locked: bool,
    /// 未解锁任务的解锁时刻（RFC3339，如 `2026-10-01T00:00:00+08:00`）。
    ///
    /// 只在 `locked` 为真时下发。实测（2026-09-30，`Sequential_Tasks_2`）：
    /// 顺序任务集的**下一项一出现就带日期锁**，这与「上一项是否完成」是
    /// 两个独立条件 —— 完成上一项只是让下一项**出现在列表里**，
    /// 能否动手还要看这个时间。少了它，界面只能说「尚未解锁」，
    /// 用户无从判断是故障还是等待。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unlock_at: String,
    /// 报名状态，取值见 `TASK_ACCEPT_*`。
    ///
    /// 字段名用上游的 `accept_status`（**不是** `status`）：Go 的权威形状如此，
    /// 且 `status` 在上游旧形状里是另一个字段（见 [`GrowthTaskWire`]）。
    /// 需要旧名可走 [`GrowthTask::status`] 别名访问器。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub accept_status: String,

    /// 上游是否下发了 `progress` 对象。
    ///
    /// ★ 必须与 `current`/`target` 分开表达：未报名时 `progress` 为 **`null`**，
    /// 若把它当成 `{0,0}` 就分不清「未报名、进度未知」与「已报名、进度为零」，
    /// 而这两种状态的下一步动作完全不同（前者要报名，后者要上报行为）。
    #[serde(default)]
    pub has_progress: bool,
    /// 当前进度。
    #[serde(default)]
    pub current: i64,
    /// 目标进度。
    #[serde(default)]
    pub target: i64,
}

fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}
fn is_false(v: &bool) -> bool {
    !*v
}

impl GrowthTask {
    /// 是否**已参与**（报名过或已领奖）。
    ///
    /// 为什么不能只判 `== accepted`：报名后只要产生了进度，状态就变成
    /// `in_progress`（实测）。用 `!= accepted` 判定会把「进行中」误判成
    /// 「未报名」，于是每轮一键完成都重复报名一遍 —— 虽然上游幂等不会出错，
    /// 但会对上游发出无意义请求，并让「本次新报名 N 个」的汇报数字虚高。
    pub fn participated(&self) -> bool {
        matches!(
            self.accept_status.as_str(),
            TASK_ACCEPT_ACCEPTED | TASK_ACCEPT_IN_PROGRESS | TASK_ACCEPT_CLAIMED
        )
    }

    /// 奖励是否已领取。
    pub fn claimed(&self) -> bool {
        self.accept_status == TASK_ACCEPT_CLAIMED
    }

    /// 是否需要报名。
    ///
    /// 已领取的任务不再报名：上游对已领取任务的报名语义未定义，重放没有收益，
    /// 且会给「一键完成」的结果里混入无意义的动作记录。
    ///
    /// 另：`locked` 的任务也不报名 —— 上游标记未解锁时报名必然被拒，
    /// 发出去只会得到一条失败记录。
    pub fn needs_accept(&self) -> bool {
        !self.participated() && !self.locked
    }

    /// 进度达标且尚未领取（本地推算，用于决定是否自动领奖）。
    ///
    /// 三重条件缺一不可：未领取、进度已知、目标为正且已达成。
    /// 要求 `target > 0` 是有意的 —— 上游对无进度语义的任务（如国际版任务清单）
    /// 不下发 `progress`，此时 `target` 恰为 0，若不排除就会被误判成「已达标」
    /// 并触发领奖。
    pub fn claimable(&self) -> bool {
        !self.claimed() && self.has_progress && self.target > 0 && self.current >= self.target
    }

    /// 进度的可读表示，用于日志与结果汇报的「前后对比」。
    pub fn progress_text(&self) -> String {
        if self.has_progress {
            format!("{}/{}", self.current, self.target)
        } else if self.claimed() {
            "claimed".to_string()
        } else if !self.accept_status.is_empty() {
            self.accept_status.clone()
        } else {
            "?".to_string()
        }
    }

    // ── 别名访问器 ──
    //
    // 供调用方按「更顺口的名字」读取，而不改变 serde 的线上形状。
    // 线上键名一律以上游 / Go 为准（`accept_status` / `has_progress` 等）。

    /// 报名状态的别名（等价 [`GrowthTask::accept_status`]）。
    pub fn status(&self) -> &str {
        &self.accept_status
    }

    /// 进度的别名：`Some((current, target))` 仅当上游真的下发了 `progress`。
    pub fn progress(&self) -> Option<(i64, i64)> {
        if self.has_progress {
            Some((self.current, self.target))
        } else {
            None
        }
    }

    /// 目标的别名（等价 [`GrowthTask::target`]）。
    pub fn target_count(&self) -> i64 {
        self.target
    }

    /// 奖励积分的别名（等价 [`GrowthTask::credit`]）。
    pub fn reward_credit(&self) -> i64 {
        self.credit
    }
}

/// 小整数转字符串。
///
/// Go 侧刻意手写了 `itoa64`（`tasks.go:157-177`），Rust 用 `to_string()` 即可 ——
/// 但**保留一个同名的薄封装**，因为 [`GrowthTask::progress_text`] 与
/// `desktop.rs` 的 `X-Request-ID` 都要求与 Go 逐字相同的十进制输出
/// （无前导零、无 `+`、负数带 `-`），集中一处便于测试钉死。
pub(crate) fn itoa64(n: i64) -> String {
    n.to_string()
}

// ──────────────────────── 上游线上形状（新旧两套） ────────────────────────

/// 上游任务条目的**线上形状**。
///
/// 同时登记新旧两套字段名（`task_code`/`code`、`accept_status`/`status`）：
/// 国际版任务清单用的是旧形状（`code` + `status` + `level_name`），
/// 严格按新形状解析会得到一堆空 `code` 的条目，进而在日志里显示成空白任务名。
/// 兼容成本极低而误判成本很高，故两套都收。
///
/// ★ 每个字段都要 `#[serde(default)]`：Go 的 `json.Unmarshal` 对缺失字段
/// 留零值，Rust 不加就会因为「字段不存在」直接报错。
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct GrowthTaskWire {
    #[serde(default)]
    task_code: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    task_desc: String,
    #[serde(default)]
    reward_credit: i64,
    #[serde(default)]
    reward_energy: i64,
    #[serde(default)]
    reward_buddy: bool,
    #[serde(default)]
    has_reward: bool,
    #[serde(default)]
    task_type: String,
    #[serde(default)]
    tag: String,
    #[serde(default)]
    jump_url: String,
    #[serde(default)]
    locked: bool,
    #[serde(default)]
    accept_status: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    unlock_at: String,
    /// `null` 与「不存在」都必须落到 `None` —— 这正是 `has_progress` 的来源。
    #[serde(default)]
    progress: Option<GrowthTaskProgressWire>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct GrowthTaskProgressWire {
    #[serde(default)]
    current: i64,
    #[serde(default)]
    target: i64,
}

impl GrowthTaskWire {
    /// 归一成对外形状。
    ///
    /// 返回 `None` = 该条目**没有任务码**，无法驱动任何动作，应当丢弃。
    fn into_task(self) -> Option<GrowthTask> {
        let code = if self.task_code.is_empty() {
            self.code
        } else {
            self.task_code
        };
        if code.is_empty() {
            // 无任务码的条目无法驱动任何动作，丢弃而非透出空条目。
            return None;
        }
        let status = if self.accept_status.is_empty() {
            self.status
        } else {
            self.accept_status
        };
        let mut task = GrowthTask {
            code,
            title: self.title,
            description: self.description,
            task_desc: self.task_desc,
            credit: self.reward_credit,
            energy: self.reward_energy,
            reward_buddy: self.reward_buddy,
            has_reward: self.has_reward,
            task_type: self.task_type,
            tag: self.tag,
            jump_url: self.jump_url,
            locked: self.locked,
            unlock_at: self.unlock_at,
            accept_status: status,
            // progress 为 null 时保持 has_progress=false：这是「未报名」的信号，
            // 不能被折叠成 0/0（见 GrowthTask::has_progress 的说明）。
            has_progress: false,
            current: 0,
            target: 0,
        };
        if let Some(p) = self.progress {
            task.has_progress = true;
            task.current = p.current;
            task.target = p.target;
        }
        Some(task)
    }
}

/// 单个任务的报名结果。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AcceptResult {
    /// 任务码（上游 `task_code`）。
    #[serde(default)]
    pub task_code: String,
    /// `accepted` / `already_accepted`，mp 口径下还可能是 `error`。
    #[serde(default)]
    pub status: String,
}

/// 领奖响应（`already_claimed` / `credit` / `energy`）。
///
/// `claim_growth_task` 与 `claim_reward_mp` 的解析**逐字相同**，Go 侧为此
/// 抽了 `parseClaimReward`；Rust 沿用同一个形状，避免两条路径漂移。
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ClaimRewardResp {
    #[serde(default)]
    already_claimed: bool,
    #[serde(default)]
    credit: i64,
    #[serde(default)]
    energy: i64,
}

/// 解领奖响应：`(credit, energy, already)`。
///
/// 与 Go `parseClaimReward` 同语义：`already_claimed=true` 时积分/能量一律归零
/// （那是「此前已领过」的幂等回执，不是本次到账）。
fn parse_claim_reward(data: &[u8]) -> Result<(i64, i64, bool)> {
    let resp: ClaimRewardResp = serde_json::from_slice(data)
        .map_err(|e| GatewayError::UpstreamParse(format!("claim reward parse: {e}")))?;
    if resp.already_claimed {
        return Ok((0, 0, true));
    }
    Ok((resp.credit, resp.energy, false))
}

/// 解报名响应，**解析失败返回空结果而不是错误**。
///
/// 与 Go `AcceptGrowthTasks` / `AcceptTasksMP` 同策略（`_ = json.Unmarshal(...)`）：
/// 报名请求本身已成功，逐条状态只是锦上添花。把整个报名判成错误会让调用方重试，
/// 而重试并不能改善这里的解析。
fn parse_accept_results(data: &[u8]) -> Vec<AcceptResult> {
    #[derive(serde::Deserialize)]
    struct Resp {
        #[serde(default)]
        results: Vec<AcceptResult>,
    }
    serde_json::from_slice::<Resp>(data)
        .map(|r| r.results)
        .unwrap_or_default()
}

/// Go `url.PathEscape` 的等价实现（`encodePathSegment` 模式）。
///
/// 领奖的任务码在**路径**里，未转义时 `Sequential_Tasks/1` 这类码会被上游当成
/// 两级路径而 404。Go 用 `url.PathEscape`，语义**不是**「除了
/// `A-Za-z0-9-_.~` 全部转义」——`$ & + : = @` 这六个保留字符在
/// `encodePathSegment` 模式下是**原样保留**的（见 Go `net/url` 的
/// `shouldEscape`），只有 `/ ; , ?` 与其余字节转义。多转义 `:`/`@` 会让
/// 任务码在上游侧对不上，故这里逐条照抄 Go 的白名单而非「从严全转」。
///
/// 非 ASCII 按 UTF-8 逐字节转义，十六进制用**大写**（Go 用 `upperhex`）。
pub(crate) fn path_escape(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'~' | b'$' | b'&' | b'+' | b':' | b'=' | b'@'
            );
        if keep {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0F) as usize] as char);
        }
    }
    out
}

/// 插入 header，忽略非法值（凭证里的 uid 等理论上可能含非 ASCII）。
///
/// 与 [`crate::upstream::headers`] 里的同名私有函数同口径：那里是**私有**的
/// （兄弟模块读不到），而领奖走的是网页头族，四条 `*_json` 管道覆盖不到。
fn put_header(headers: &mut reqwest::header::HeaderMap, key: &'static str, value: &str) {
    if let Ok(v) = reqwest::header::HeaderValue::from_str(value) {
        headers.insert(key, v);
    }
}

// ──────────────────────────── 域调用 ────────────────────────────

impl Client {
    /// 拉取全量成长任务列表。
    ///
    /// 区域差异：国际版（`workbuddy.ai`）的同名端点返回的是另一套 5 项任务
    /// （`first_chat` / `skill_installed` / ...，无奖励字段、无 `progress`），
    /// 本函数的解析对两套形状都成立，但调用方应据区域决定是否推进。
    ///
    /// **走 [`Client::growth_json_no_ua`]**：上游按 `User-Agent` 裁剪任务清单，
    /// 而 Go 的 `net/http` 会自动补 `Go-http-client/1.1`，导致「小程序限定」的
    /// `school_season` 与 `Sequential_Tasks_1` 被过滤掉（18 项而非 20 项）。
    pub async fn list_growth_tasks(&self, a: &Auth) -> Result<Vec<GrowthTask>> {
        let data = self
            .growth_json_no_ua(a, reqwest::Method::GET, GROWTH_TASKS_LIST_PATH, None)
            .await?;
        let resp: BTreeMap<String, serde_json::Value> = serde_json::from_slice(&data)
            .map_err(|e| GatewayError::UpstreamParse(format!("tasks list parse: {e}")))?;
        let wire: Vec<GrowthTaskWire> = match resp.get("tasks") {
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|e| GatewayError::UpstreamParse(format!("tasks list items parse: {e}")))?,
            // `tasks` 缺失等价于空清单（Go 的 json.Unmarshal 留零值切片）。
            None => Vec::new(),
        };
        Ok(wire.into_iter().filter_map(GrowthTaskWire::into_task).collect())
    }

    /// 报名一批任务，返回上游对每个任务的处理结果。
    ///
    /// 幂等：重复报名返回 `status=already_accepted` 且 HTTP 200，
    /// 因此调用方无需先查状态再决定是否报名，直接重放即可。
    ///
    /// ★ `codes` 为空返回空（**不发请求**）。
    /// ★ 结果解析失败**不算失败** —— 见 [`parse_accept_results`]。
    pub async fn accept_growth_tasks(
        &self,
        a: &Auth,
        codes: &[String],
    ) -> Result<Vec<AcceptResult>> {
        if codes.is_empty() {
            return Ok(Vec::new());
        }
        let body = serde_json::json!({ "task_codes": codes });
        let data = self
            .growth_json(
                a,
                reqwest::Method::POST,
                GROWTH_TASKS_ACCEPT_PATH,
                Some(&body),
            )
            .await?;
        Ok(parse_accept_results(&data))
    }

    /// 以**小程序口径**报名一批任务，返回上游对每个任务的处理结果。
    ///
    /// 与 [`Client::accept_growth_tasks`] 的关系：同端点、同请求体，唯一差别是
    /// `X-Client-Platform: miniprogram` 头。
    ///
    /// 刻意**不**让 `accept_growth_tasks` 内部自动补头：批量报名里通常混着普通
    /// 任务，给整个批次加 mp 头会把普通任务也按小程序口径处理 —— 那是另一种输入，
    /// 上游按它可能给出不同结果（实测 mp 列表是默认口径的**子集**，不是超集）。
    ///
    /// 返回值与 [`Client::accept_growth_tasks`] 同形，调用方据此逐条核对 `status`：
    /// 对 mp 任务而言「HTTP 200 但 `status=error`」是真实存在的失败形态，
    /// 只看 `error` 会把这种失败当成成功。
    pub async fn accept_tasks_mp(&self, a: &Auth, codes: &[String]) -> Result<Vec<AcceptResult>> {
        if codes.is_empty() {
            return Ok(Vec::new());
        }
        let body = serde_json::json!({ "task_codes": codes });
        let data = self
            .growth_json_mp(
                a,
                reqwest::Method::POST,
                GROWTH_TASKS_ACCEPT_PATH,
                Some(&body),
            )
            .await?;
        Ok(parse_accept_results(&data))
    }

    /// 领取成长任务奖励，返回 `(credit, energy, already_claimed)`。
    ///
    /// ★ **走 [`Client::web_base`]（web 指纹），不是 chat/growth 域。**
    /// 成长中心是网页端调用，其 `Origin`/`Referer`/`User-Agent` 与 CLI 完全不同；
    /// 复用 CLI 头族会让请求看起来是「CLI 在调网页接口」，会被上游按
    /// 「非 Web 来源」拒绝。因此这里**就地构造头**，不走 `billing_json` /
    /// `growth_json` / `growth_json_mp` 那三个管道（它们会带上 CLI 的 UA 与
    /// `X-Domain`）。
    ///
    /// 四条照抄 Go `ClaimGrowthTask`（`tasks.go:318-363`）的要点：
    /// 1. `task_code.trim()` 为空 ⇒ **立即**返回 `(0, 0, false)`，**不发请求**；
    /// 2. 任务码在**路径**里（`GROWTH_TASK_CLAIM_PATH_FMT` + [`path_escape`]），
    ///    **无请求体**；
    /// 3. 头族含 `x-client-platform: web` 与指向成长中心的 `Origin`/`Referer`；
    /// 4. `already_claimed == true` ⇒ `(0, 0, true)`，否则 `(credit, energy, false)`。
    ///
    /// 解析与 [`Client::claim_reward_mp`] 共用 [`parse_claim_reward`]，
    /// 两条路径不会漂移（Go 亦为此抽出 `parseClaimReward`）。
    pub async fn claim_growth_task(&self, a: &Auth, task_code: &str) -> Result<(i64, i64, bool)> {
        if task_code.trim().is_empty() {
            return Ok((0, 0, false));
        }
        let base = self.web_base(a).to_string();
        let path = GROWTH_TASK_CLAIM_PATH_FMT.replacen("{}", &path_escape(task_code), 1);
        let mut h = reqwest::header::HeaderMap::new();
        put_header(&mut h, "Authorization", &format!("Bearer {}", a.access_token));
        put_header(&mut h, "Accept", "application/json, text/plain, */*");
        put_header(&mut h, "Content-Type", "application/json");
        put_header(&mut h, "Origin", &base);
        put_header(&mut h, "Referer", &format!("{base}/profile/growth-center"));
        put_header(&mut h, "x-client-platform", "web");
        put_header(&mut h, "User-Agent", WEB_UA);
        if !a.uid.is_empty() {
            put_header(&mut h, "X-User-Id", &a.uid);
        }
        if !a.enterprise_id.is_empty() {
            put_header(&mut h, "X-Enterprise-Id", &a.enterprise_id);
            put_header(&mut h, "X-Tenant-Id", &a.enterprise_id);
        }
        if !a.domain.is_empty() {
            put_header(&mut h, "X-Domain", &a.domain);
        }
        let req = self.http().post(format!("{base}{path}")).headers(h);
        let data = self.do_json(req).await?;
        parse_claim_reward(&data)
    }

    /// 领取**小程序**任务的奖励，mp 域失败时回落 Web 域。
    ///
    /// 为什么 mp 优先：小程序任务在 chat 域（与报名同一个基址）领奖，
    /// Web 域对它们是另一种来源。
    ///
    /// 为什么回落**只认 4xx**：5xx 是上游故障，换个域重试并不会好；而 4xx
    ///（含 `task not completed` 与 `task not found`）意味着这个域不接受这次领奖，
    /// 换 Web 域是有意义的下一步 —— 两个域对同一任务的可见性并不总是相同。
    /// 传输层错误同样不回落（它不是「这个域不认」，重试只是白打一次）。
    ///
    /// ★ 回落**不**改变「进度未达标」的判定语义：回落返回的错误同样可被
    /// [`is_growth_task_not_completed`] 识别，调用方看到的仍是同一种错误。
    pub async fn claim_reward_mp(&self, a: &Auth, task_code: &str) -> Result<(i64, i64, bool)> {
        if task_code.trim().is_empty() {
            return Ok((0, 0, false));
        }
        let path = GROWTH_TASK_CLAIM_PATH_FMT.replacen("{}", &path_escape(task_code), 1);
        match self
            .growth_json_mp(a, reqwest::Method::POST, &path, None)
            .await
        {
            Ok(data) => parse_claim_reward(&data),
            Err(err) => {
                let is_4xx = matches!(
                    &err,
                    GatewayError::UpstreamError(ue) if (400..500).contains(&ue.status)
                );
                if !is_4xx {
                    return Err(err);
                }
                self.claim_growth_task(a, task_code).await
            }
        }
    }
}

// ──────────────────────── 「进度未达标」判定 ────────────────────────

/// 领奖被拒时的「进度未达标」文案特征。
///
/// 上游在进度不足时返回业务错误而非静默失败，文案形态多样
///（`task not completed` / `not finished` / `未完成`），登记多种以便调用方
/// 能把「没达标」与「真的出错」区分开：前者应等待计分落定后重试，
/// 后者应放弃并留痕，混为一谈会让日志把正常等待渲染成故障。
const GROWTH_TASK_NOT_COMPLETED_MARKERS: &[&str] = &[
    "task not completed",
    "not completed yet",
    "task not finished",
    "not finished",
    "未完成",
    "进度不足",
];

/// 报告领奖错误是否为「进度未达标」。
///
/// ★ **只认 4xx**：5xx 是上游故障，与任务进度无关，归为「未达标」会让调用方
/// 白白等待一个永远不会靠等待解决的错误。传输层错误（连接失败 / 超时）
/// 同样不算 —— Go 侧那里是 `*Error` 类型断言失败，Rust 侧对应
/// [`GatewayError::Transport`]。
///
/// 判定用 `msg`（Go 是 `ue.Msg`，即响应体片段），做小写化后包含匹配；
/// 中文标记不含大写字母，小写化对它们无影响（与 Go `strings.ToLower` 同口径）。
pub fn is_growth_task_not_completed(err: &GatewayError) -> bool {
    let GatewayError::UpstreamError(ue) = err else {
        return false;
    };
    if ue.status < 400 || ue.status >= 500 {
        return false;
    }
    let lower = ue.msg.to_lowercase();
    GROWTH_TASK_NOT_COMPLETED_MARKERS
        .iter()
        .any(|m| lower.contains(&m.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::classify::{ErrKind, UpstreamError};

    fn task(status: &str) -> GrowthTask {
        GrowthTask {
            code: "t1".into(),
            accept_status: status.into(),
            ..Default::default()
        }
    }

    fn upstream_err(status: u16, msg: &str) -> GatewayError {
        GatewayError::UpstreamError(UpstreamError {
            kind: ErrKind::Client,
            status,
            msg: msg.into(),
        })
    }

    // ── 五个派生方法的真值表 ──

    #[test]
    fn participated_truth_table() {
        for (status, want) in [
            (TASK_ACCEPT_NOT_ACCEPTED, false),
            (TASK_ACCEPT_ACCEPTED, true),
            // ★ 这一行是历史上真实踩过的坑：只登记 accepted/claimed 时它是 false。
            (TASK_ACCEPT_IN_PROGRESS, true),
            (TASK_ACCEPT_COMPLETED, false),
            (TASK_ACCEPT_CLAIMED, true),
            ("", false),
            ("weird", false),
        ] {
            assert_eq!(
                task(status).participated(),
                want,
                "participated({status:?}) 应为 {want}"
            );
        }
    }

    #[test]
    fn claimed_only_for_claimed_status() {
        assert!(task(TASK_ACCEPT_CLAIMED).claimed());
        for s in [
            TASK_ACCEPT_NOT_ACCEPTED,
            TASK_ACCEPT_ACCEPTED,
            TASK_ACCEPT_IN_PROGRESS,
            TASK_ACCEPT_COMPLETED,
            "",
        ] {
            assert!(!task(s).claimed(), "claimed({s:?}) 应为 false");
        }
    }

    #[test]
    fn needs_accept_skips_participated_and_locked() {
        assert!(task(TASK_ACCEPT_NOT_ACCEPTED).needs_accept());
        assert!(!task(TASK_ACCEPT_ACCEPTED).needs_accept());
        assert!(!task(TASK_ACCEPT_IN_PROGRESS).needs_accept());
        assert!(!task(TASK_ACCEPT_CLAIMED).needs_accept());
        // completed 未领奖 → 不在 participated 集合里，需要报名。
        assert!(task(TASK_ACCEPT_COMPLETED).needs_accept());
        // locked：上游必然拒，不发。
        let mut t = task(TASK_ACCEPT_NOT_ACCEPTED);
        t.locked = true;
        assert!(!t.needs_accept(), "locked 任务不应报名");
    }

    #[test]
    fn claimable_requires_positive_target() {
        let mk = |has_progress: bool, current: i64, target: i64, status: &str| GrowthTask {
            has_progress,
            current,
            target,
            accept_status: status.into(),
            ..Default::default()
        };
        assert!(mk(true, 5, 5, TASK_ACCEPT_COMPLETED).claimable());
        assert!(mk(true, 6, 5, TASK_ACCEPT_IN_PROGRESS).claimable());
        assert!(!mk(true, 4, 5, TASK_ACCEPT_IN_PROGRESS).claimable(), "未达标");
        assert!(!mk(false, 9, 9, TASK_ACCEPT_ACCEPTED).claimable(), "无进度不算");
        // ★ 国际版任务不下发 progress、target 恰为 0 —— 不能误判成「已达标」。
        assert!(!mk(false, 0, 0, TASK_ACCEPT_ACCEPTED).claimable());
        assert!(
            !mk(true, 0, 0, TASK_ACCEPT_ACCEPTED).claimable(),
            "target=0 不算达标"
        );
        assert!(!mk(true, 5, 5, TASK_ACCEPT_CLAIMED).claimable(), "已领不再领");
    }

    #[test]
    fn progress_text_four_cases() {
        let mut t = GrowthTask {
            has_progress: true,
            current: 3,
            target: 7,
            ..Default::default()
        };
        assert_eq!(t.progress_text(), "3/7");
        t.has_progress = false;
        t.accept_status = TASK_ACCEPT_CLAIMED.into();
        assert_eq!(t.progress_text(), "claimed");
        t.accept_status = TASK_ACCEPT_IN_PROGRESS.into();
        assert_eq!(t.progress_text(), "in_progress");
        t.accept_status = String::new();
        assert_eq!(t.progress_text(), "?");
    }

    #[test]
    fn alias_accessors_mirror_real_fields() {
        let t = GrowthTask {
            accept_status: TASK_ACCEPT_IN_PROGRESS.into(),
            has_progress: true,
            current: 1,
            target: 9,
            credit: 100,
            ..Default::default()
        };
        assert_eq!(t.status(), TASK_ACCEPT_IN_PROGRESS);
        assert_eq!(t.progress(), Some((1, 9)));
        assert_eq!(t.target_count(), 9);
        assert_eq!(t.reward_credit(), 100);
        assert_eq!(GrowthTask::default().progress(), None);
    }

    // ── 线上形状解析 ──

    #[test]
    fn wire_accepts_new_shape() {
        // 含中文 ⇒ 必须用 `r#"…"#`（&str）再 `.as_bytes()`；`br#"…"#` 是 byte 字面量，
        // 只允许 ASCII，中文 fixture 会直接编译失败。
        let raw = r#"{
            "task_code":"school_season","title":"校园日","description":"d","task_desc":"td",
            "reward_credit":100,"reward_energy":5,"reward_buddy":true,"has_reward":true,
            "task_type":"daily","tag":"PC","jump_url":"workbuddy://x","locked":true,
            "unlock_at":"2026-10-01T00:00:00+08:00","accept_status":"in_progress",
            "progress":{"current":1,"target":3}
        }"#
        .as_bytes();
        let w: GrowthTaskWire = serde_json::from_slice(raw).unwrap();
        let t = w.into_task().expect("有 task_code");
        assert_eq!(t.code, "school_season");
        assert_eq!(t.title, "校园日");
        assert_eq!(t.credit, 100);
        assert_eq!(t.energy, 5);
        assert!(t.reward_buddy && t.has_reward && t.locked);
        assert_eq!(t.task_type, "daily");
        assert_eq!(t.tag, "PC");
        assert_eq!(t.jump_url, "workbuddy://x");
        assert_eq!(t.unlock_at, "2026-10-01T00:00:00+08:00");
        assert_eq!(t.accept_status, "in_progress");
        assert!(t.has_progress);
        assert_eq!((t.current, t.target), (1, 3));
    }

    #[test]
    fn wire_accepts_legacy_shape() {
        // 国际版任务清单用旧形状（code + status），严格按新形状解析会得到空 code。
        let raw = r#"{"code":"first_chat","status":"accepted","title":"首次对话"}"#.as_bytes();
        let w: GrowthTaskWire = serde_json::from_slice(raw).unwrap();
        let t = w.into_task().expect("旧形状也要有 code");
        assert_eq!(t.code, "first_chat");
        assert_eq!(t.accept_status, "accepted", "status 要回落成 accept_status");
    }

    #[test]
    fn wire_prefers_new_field_over_legacy() {
        let raw =
            br#"{"task_code":"new","code":"old","accept_status":"claimed","status":"accepted"}"#;
        let w: GrowthTaskWire = serde_json::from_slice(raw).unwrap();
        let t = w.into_task().unwrap();
        assert_eq!(t.code, "new");
        assert_eq!(t.accept_status, "claimed");
    }

    #[test]
    fn wire_null_progress_keeps_has_progress_false() {
        let raw = br#"{"task_code":"t","accept_status":"not_accepted","progress":null}"#;
        let w: GrowthTaskWire = serde_json::from_slice(raw).unwrap();
        let t = w.into_task().unwrap();
        assert!(!t.has_progress, "progress=null 必须落到 has_progress=false");
        assert_eq!((t.current, t.target), (0, 0));
        // 与「已报名、进度为零」区分：那种情况 progress 是对象，has_progress=true。
        let raw2 =
            br#"{"task_code":"t","accept_status":"accepted","progress":{"current":0,"target":0}}"#;
        let w2: GrowthTaskWire = serde_json::from_slice(raw2).unwrap();
        assert!(w2.into_task().unwrap().has_progress);
    }

    #[test]
    fn wire_drops_entries_without_code() {
        assert!(GrowthTaskWire::default().into_task().is_none());
        let raw = r#"{"title":"没有任务码"}"#.as_bytes();
        let w: GrowthTaskWire = serde_json::from_slice(raw).unwrap();
        assert!(w.into_task().is_none(), "无任务码条目应丢弃而非透出空条目");
    }

    #[test]
    fn accept_result_shape() {
        let r: AcceptResult =
            serde_json::from_str(r#"{"task_code":"a","status":"accepted"}"#).unwrap();
        assert_eq!(r.task_code, "a");
        assert_eq!(r.status, "accepted");
        // 上游可能只给一个字段。
        let r2: AcceptResult = serde_json::from_str(r#"{"status":"error"}"#).unwrap();
        assert_eq!(r2.task_code, "");
    }

    #[test]
    fn parse_accept_results_tolerates_garbage() {
        assert!(parse_accept_results(b"not json").is_empty(), "解析失败不算失败");
        assert!(parse_accept_results(br#"{"results":[]}"#).is_empty());
        let ok = parse_accept_results(br#"{"results":[{"task_code":"a","status":"accepted"}]}"#);
        assert_eq!(ok.len(), 1);
    }

    #[test]
    fn parse_claim_reward_zeroes_on_already_claimed() {
        let (c, e, already) =
            parse_claim_reward(br#"{"already_claimed":true,"credit":99,"energy":9}"#).unwrap();
        assert_eq!((c, e, already), (0, 0, true), "幂等回执不算本次到账");
        let (c, e, already) = parse_claim_reward(br#"{"credit":100,"energy":5}"#).unwrap();
        assert_eq!((c, e, already), (100, 5, false));
        assert!(parse_claim_reward(b"oops").is_err());
    }

    // ── 「进度未达标」判定 ──

    #[test]
    fn not_completed_matches_english_and_chinese_markers() {
        for msg in [
            "task not completed",
            "Task Not Completed",
            "not completed yet",
            "task not finished",
            "not finished",
            "任务未完成",
            "进度不足，请稍后再试",
        ] {
            assert!(
                is_growth_task_not_completed(&upstream_err(400, msg)),
                "{msg:?} 应判为未达标"
            );
        }
        for msg in ["unauthorized", "internal error", "task not found"] {
            assert!(
                !is_growth_task_not_completed(&upstream_err(400, msg)),
                "{msg:?} 不应判为未达标"
            );
        }
    }

    #[test]
    fn not_completed_only_accepts_4xx() {
        // ★ 5xx 是上游故障，换个时间重试才是对的；归成「未达标」会让调用方白等。
        for status in [500u16, 502, 503, 599] {
            assert!(
                !is_growth_task_not_completed(&upstream_err(status, "task not completed")),
                "http {status} 不应判为未达标"
            );
        }
        for status in [200u16, 201, 302] {
            assert!(!is_growth_task_not_completed(&upstream_err(
                status,
                "task not completed"
            )));
        }
        for status in [400u16, 403, 404, 429, 499] {
            assert!(is_growth_task_not_completed(&upstream_err(
                status,
                "task not completed"
            )));
        }
    }

    #[test]
    fn not_completed_rejects_non_upstream_errors() {
        assert!(!is_growth_task_not_completed(&GatewayError::Transport(
            "connection refused".into()
        )));
        assert!(!is_growth_task_not_completed(&GatewayError::UpstreamParse(
            "task not completed".into()
        )));
    }

    #[test]
    fn itoa64_matches_go_decimal_output() {
        assert_eq!(itoa64(0), "0");
        assert_eq!(itoa64(7), "7");
        assert_eq!(itoa64(1789036585355), "1789036585355");
        assert_eq!(itoa64(-42), "-42");
        assert_eq!(itoa64(i64::MIN + 1), "-9223372036854775807");
    }

    // ── 序列化形状（omitempty 逐字照抄） ──

    #[test]
    fn growth_task_serialization_matches_go_omitempty() {
        let v = serde_json::to_value(GrowthTask::default()).unwrap();
        let obj = v.as_object().unwrap();
        // 无 omitempty 的键始终出现。
        for k in ["code", "has_progress", "current", "target"] {
            assert!(obj.contains_key(k), "{k} 必须始终出现");
        }
        // 有 omitempty 的键在零值时消失。
        for k in [
            "title",
            "description",
            "task_desc",
            "reward_credit",
            "reward_energy",
            "reward_buddy",
            "has_reward",
            "task_type",
            "tag",
            "jump_url",
            "locked",
            "unlock_at",
            "accept_status",
        ] {
            assert!(!obj.contains_key(k), "{k} 零值时应被 omitempty 去掉");
        }
        let full = GrowthTask {
            code: "c".into(),
            title: "t".into(),
            credit: 1,
            locked: true,
            accept_status: "accepted".into(),
            has_progress: true,
            current: 1,
            target: 2,
            ..Default::default()
        };
        let obj = serde_json::to_value(full).unwrap();
        assert_eq!(obj["reward_credit"], serde_json::json!(1));
        assert_eq!(obj["accept_status"], serde_json::json!("accepted"));
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
        fn header(&self, name: &str) -> Option<&str> {
            let want = name.to_ascii_lowercase();
            self.headers
                .iter()
                .find(|(k, _)| k.to_ascii_lowercase() == want)
                .map(|(_, v)| v.as_str())
        }
        fn has_header(&self, name: &str) -> bool {
            self.header(name).is_some()
        }
    }

    fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    /// 起一个假上游（`std::net::TcpListener`；tokio 的 `net` feature 未启用）。
    ///
    /// 返回 `(base_url, seen)`。响应体固定为 `resp_body`、状态码固定 200。
    fn spawn_fake_upstream(
        resp_body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<Seen>>>) {
        spawn_fake_upstream_seq(vec![(200, resp_body)])
    }

    /// 起一个按**连接顺序**应答的假上游。
    ///
    /// 第 N 个连接返回 `responses[N]`，超出后重复最后一个 —— 用于
    /// 「mp 域 4xx → 回落 web 域 200」这类需要**多跳**才能验完的路径：
    /// 两条路径指向同一个假上游，靠先后顺序区分而不是靠端口。
    fn spawn_fake_upstream_seq(
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

    /// 把 chat / billing / web 三个域**全部**指向同一个假上游。
    ///
    /// 报名走 chat 域、领奖走 web 域、mp 领奖回落会跨域；只指一个域会让另一跳
    /// 漏到真实域名（= 往真实账号写真实数据），故这里三个一起指。
    fn client_for(base: &str) -> Client {
        Client::new(crate::upstream::ClientConfig {
            chat_base_cn: base.to_string(),
            billing_base_cn: base.to_string(),
            web_base_cn: base.to_string(),
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn list_growth_tasks_uses_no_ua_and_parses_both_shapes() {
        let (base, seen) = spawn_fake_upstream(
            r#"{"code":0,"data":{"tasks":[
                {"task_code":"school_season","accept_status":"not_accepted"},
                {"code":"first_chat","status":"accepted"},
                {"title":"无码条目应被丢弃"}
            ]}}"#,
        );
        let client = Client::new(crate::upstream::ClientConfig {
            chat_base_cn: base,
            ..Default::default()
        })
        .unwrap();
        let tasks = client.list_growth_tasks(&account()).await.unwrap();
        assert_eq!(tasks.len(), 2, "无任务码的条目应被丢弃");
        assert_eq!(tasks[0].code, "school_season");
        assert_eq!(tasks[1].code, "first_chat");
        assert_eq!(tasks[1].accept_status, "accepted");

        let got = seen.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].method, "GET");
        assert_eq!(got[0].path, GROWTH_TASKS_LIST_PATH);
        // ★ 列表必须走 no-UA 口径：带上任何 UA 都会被上游裁到 18 项。
        assert!(
            !got[0].has_header("user-agent"),
            "任务列表不得携带 User-Agent，实际头: {:?}",
            got[0].headers
        );
    }

    #[tokio::test]
    async fn accept_growth_tasks_posts_codes_with_go_ua() {
        let (base, seen) = spawn_fake_upstream(
            r#"{"code":0,"data":{"results":[{"task_code":"a","status":"accepted"}]}}"#,
        );
        let client = Client::new(crate::upstream::ClientConfig {
            chat_base_cn: base,
            ..Default::default()
        })
        .unwrap();
        let out = client
            .accept_growth_tasks(&account(), &["a".to_string()])
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].status, "accepted");

        let got = seen.lock().unwrap();
        assert_eq!(got[0].method, "POST");
        assert_eq!(got[0].path, GROWTH_TASKS_ACCEPT_PATH);
        let body: serde_json::Value = serde_json::from_str(&got[0].body).unwrap();
        assert_eq!(body["task_codes"], serde_json::json!(["a"]));
        // 默认口径带 Go UA，且**不带** mp 头。
        assert_eq!(got[0].header("user-agent"), Some("Go-http-client/1.1"));
        assert!(!got[0].has_header("x-client-platform"));
    }

    #[tokio::test]
    async fn accept_tasks_mp_sends_miniprogram_platform_header() {
        let (base, seen) = spawn_fake_upstream(
            r#"{"code":0,"data":{"results":[{"task_code":"Sequential_Tasks_1","status":"accepted"}]}}"#,
        );
        let client = Client::new(crate::upstream::ClientConfig {
            chat_base_cn: base,
            ..Default::default()
        })
        .unwrap();
        let out = client
            .accept_tasks_mp(&account(), &["Sequential_Tasks_1".to_string()])
            .await
            .unwrap();
        assert_eq!(out[0].status, "accepted");

        let got = seen.lock().unwrap();
        assert_eq!(got[0].path, GROWTH_TASKS_ACCEPT_PATH);
        assert_eq!(
            got[0].header("x-client-platform"),
            Some(crate::upstream::client::MP_PLATFORM),
            "mp 口径必须带 X-Client-Platform: miniprogram"
        );
        assert_ne!(
            got[0].header("x-client-platform"),
            Some("mp-weixin"),
            "★ 不要与 report 域的 mp-weixin 混用"
        );
    }

    #[tokio::test]
    async fn empty_codes_short_circuit_without_request() {
        let (base, seen) = spawn_fake_upstream(r#"{"code":0,"data":{}}"#);
        let client = Client::new(crate::upstream::ClientConfig {
            chat_base_cn: base,
            ..Default::default()
        })
        .unwrap();
        let a = account();
        assert!(client.accept_growth_tasks(&a, &[]).await.unwrap().is_empty());
        assert!(client.accept_tasks_mp(&a, &[]).await.unwrap().is_empty());
        assert_eq!(
            seen.lock().unwrap().len(),
            0,
            "codes 为空必须不发请求（Go len(codes)==0 → nil,nil）"
        );
    }

    #[tokio::test]
    async fn accept_tolerates_unparsable_results() {
        // 报名请求本身 200 且 code=0，但 data 形状不对 ⇒ 不算失败。
        let (base, _seen) = spawn_fake_upstream(r#"{"code":0,"data":{"unexpected":1}}"#);
        let client = Client::new(crate::upstream::ClientConfig {
            chat_base_cn: base,
            ..Default::default()
        })
        .unwrap();
        let out = client
            .accept_growth_tasks(&account(), &["a".to_string()])
            .await
            .unwrap();
        assert!(out.is_empty(), "解析失败不算失败（Go _ = json.Unmarshal）");
    }

    /// 业务码非 0 ⇒ [`GatewayError::UpstreamError`]，但 **status 仍是 HTTP 200**。
    ///
    /// ★ 这条断言此前写反过：它假设「业务码非 0 也算未达标」。Go 的
    /// `IsGrowthTaskNotCompleted`（`tasks.go:389`）要求 `400 <= Status < 500`，
    /// 而 `doJSON` 在 `env.Code != 0` 时填的是**响应本身的状态码**（这里是 200）
    /// —— 也就是说业务码错误**永远不**满足未达标判定，无论文案多像。
    /// 未达标只从真正的 4xx 来（上游用 HTTP 状态表达它）。
    #[tokio::test]
    async fn upstream_business_error_keeps_http_status_and_is_not_not_completed() {
        let (base, _seen) = spawn_fake_upstream(r#"{"code":14018,"msg":"task not completed"}"#);
        let client = Client::new(crate::upstream::ClientConfig {
            chat_base_cn: base,
            ..Default::default()
        })
        .unwrap();
        let err = client
            .accept_growth_tasks(&account(), &["a".to_string()])
            .await
            .unwrap_err();

        let GatewayError::UpstreamError(ue) = &err else {
            panic!("业务码非 0 必须是 UpstreamError，实际 {err:?}");
        };
        assert_eq!(ue.status, 200, "业务码错误的 status 取自 HTTP 响应，即 200");
        assert!(
            ue.msg.starts_with("code=14018 msg=task not completed"),
            "msg 形状应为 `code=<n> msg=<原文>`，实际 {:?}",
            ue.msg
        );
        assert!(
            !is_growth_task_not_completed(&err),
            "业务码非 0 且 HTTP 200 **不算**未达标（Go 只认 4xx），实际 {err}"
        );
    }

    /// 未达标判定的正例：4xx + 文案命中才为真（与 Go 逐条对齐）。
    #[test]
    fn not_completed_requires_4xx_and_marker() {
        // 4xx + 命中文案 → 真。
        assert!(is_growth_task_not_completed(&upstream_err(
            400,
            "task not completed"
        )));
        assert!(is_growth_task_not_completed(&upstream_err(
            403,
            "进度不足，无法领取"
        )));
        // 大小写无关（Go 是 strings.ToLower 双通道比较）。
        assert!(is_growth_task_not_completed(&upstream_err(
            400,
            "Task Not Finished"
        )));
        // 4xx 但文案不含任何标记 → 假（真的出错，不该等）。
        assert!(!is_growth_task_not_completed(&upstream_err(
            400,
            "invalid task code"
        )));
        // 5xx：上游故障，与进度无关（Go 明确排除，否则调用方会白等）。
        assert!(!is_growth_task_not_completed(&upstream_err(
            500,
            "task not completed"
        )));
        // 2xx：业务码错误的形态（status 取自 HTTP 响应）。
        assert!(!is_growth_task_not_completed(&upstream_err(
            200,
            "code=14018 msg=task not completed"
        )));
        // 非上游错误（传输层/解析）→ 假。
        assert!(!is_growth_task_not_completed(&GatewayError::Transport(
            "connection refused".into()
        )));
        assert!(!is_growth_task_not_completed(
            &GatewayError::UpstreamParse("bad json".into())
        ));
    }

    // ── path_escape（Go url.PathEscape 等价） ──

    /// 期望值由**真实 Go 工具链**产出（`go run` 打印 `url.PathEscape` 的输出），
    /// 不是手推的。
    ///
    /// ★ 这里钉死 Go `encodePathSegment` 模式的反直觉之处：保留集是
    /// `$ & + - . : = @ _ ~` + 字母数字（共 72 个），**`:` 与 `@` 不转义**；
    /// 只有 `/ ; , ?` 与其余字节转义。若按「除 `A-Za-z0-9-_.~` 全转」实现，
    /// `Sequential_Tasks/1` 仍能过，但含 `:`/`@` 的任务码会多转义而 404。
    #[test]
    fn path_escape_matches_go_path_escape() {
        assert_eq!(path_escape("Sequential_Tasks/1"), "Sequential_Tasks%2F1");
        assert_eq!(
            path_escape("a b:c@d,e;f?g#h%i"),
            "a%20b:c@d%2Ce%3Bf%3Fg%23h%25i"
        );
        assert_eq!(path_escape("任务/1"), "%E4%BB%BB%E5%8A%A1%2F1");
        // 全保留集原样通过（Go 实测 72 个字符）。
        let keep = "$&+-.0123456789:=@ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz~";
        assert_eq!(path_escape(keep), keep, "Go 保留集必须原样通过");
        // 十六进制大写（Go 用 upperhex）。
        assert_eq!(path_escape("\u{7f}"), "%7F");
        assert_eq!(path_escape(""), "");
    }

    // ── 领奖（web 域 / mp 域） ──

    #[tokio::test]
    async fn claim_growth_task_empty_code_short_circuits_without_request() {
        let (base, seen) = spawn_fake_upstream(r#"{"code":0,"data":{}}"#);
        let client = client_for(&base);
        let a = account();
        for code in ["", "   ", "\t\n"] {
            assert_eq!(
                client.claim_growth_task(&a, code).await.unwrap(),
                (0, 0, false),
                "空白任务码必须返回零值"
            );
        }
        assert_eq!(
            seen.lock().unwrap().len(),
            0,
            "任务码为空必须不发请求（Go strings.TrimSpace(taskCode)==\"\" → 0,0,false,nil）"
        );
    }

    #[tokio::test]
    async fn claim_growth_task_uses_web_fingerprint_and_no_body() {
        let (base, seen) = spawn_fake_upstream(
            r#"{"code":0,"data":{"already_claimed":false,"credit":7,"energy":3}}"#,
        );
        let client = client_for(&base);
        let out = client
            .claim_growth_task(&account(), "Sequential_Tasks/1")
            .await
            .unwrap();
        assert_eq!(out, (7, 3, false));

        let got = seen.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].method, "POST");
        // ★ 任务码在路径里且已按 Go PathEscape 转义（`/` → `%2F`）。
        assert_eq!(
            got[0].path,
            "/activity/growth/tasks/Sequential_Tasks%2F1/claim"
        );
        // 无请求体。
        assert_eq!(got[0].body, "", "领奖无请求体（Go NewRequest(..., nil)）");
        // 网页头族：这四条是「不能复用 CLI 头族」的证据。
        assert_eq!(
            got[0].header("user-agent"),
            Some(crate::upstream::desktop::WEB_UA),
            "★ 必须用 webUA，用 CLI UA 会被按「CLI 调网页接口」拒绝"
        );
        assert_eq!(got[0].header("x-client-platform"), Some("web"));
        assert_eq!(
            got[0].header("origin"),
            Some(base.as_str()),
            "Origin 必须指向 web 基址"
        );
        assert_eq!(
            got[0].header("referer"),
            Some(format!("{base}/profile/growth-center").as_str()),
            "Referer 必须指向成长中心"
        );
        // 鉴权与身份头。
        assert_eq!(got[0].header("authorization"), Some("Bearer at-test"));
        assert_eq!(
            got[0].header("accept"),
            Some("application/json, text/plain, */*")
        );
        assert_eq!(got[0].header("content-type"), Some("application/json"));
        assert_eq!(got[0].header("x-user-id"), Some("uid-test"));
        assert_eq!(got[0].header("x-enterprise-id"), Some("ent-test"));
        assert_eq!(got[0].header("x-tenant-id"), Some("ent-test"));
        assert_eq!(got[0].header("x-domain"), Some("https://copilot.tencent.com"));
    }

    #[tokio::test]
    async fn claim_growth_task_already_claimed_zeroes_rewards() {
        let (base, _seen) = spawn_fake_upstream(
            r#"{"code":0,"data":{"already_claimed":true,"credit":7,"energy":3}}"#,
        );
        let client = client_for(&base);
        assert_eq!(
            client.claim_growth_task(&account(), "daily").await.unwrap(),
            (0, 0, true),
            "已领过 ⇒ (0,0,true)，即使响应里带了 credit/energy"
        );
    }

    #[tokio::test]
    async fn claim_reward_mp_prefers_chat_domain_with_mp_header() {
        let (base, seen) = spawn_fake_upstream(
            r#"{"code":0,"data":{"already_claimed":false,"credit":5,"energy":1}}"#,
        );
        let client = client_for(&base);
        assert_eq!(
            client.claim_reward_mp(&account(), "Sequential_Tasks_1").await.unwrap(),
            (5, 1, false)
        );
        let got = seen.lock().unwrap();
        assert_eq!(got.len(), 1, "成功时不得回落，只打一次");
        assert_eq!(
            got[0].header("x-client-platform"),
            Some(crate::upstream::client::MP_PLATFORM),
            "★ mp 口径是 miniprogram，不是 report 域的 mp-weixin"
        );
        assert_eq!(
            got[0].header("user-agent"),
            Some("Go-http-client/1.1"),
            "mp 口径复用 growth 管道的 Go UA"
        );
    }

    /// ★ 本任务的核心语义：**只有 4xx 才回落 Web 域**。
    ///
    /// 一次调用打**两次**：先 mp（chat 域）4xx，再 web 域 200。
    #[tokio::test]
    async fn claim_reward_mp_falls_back_to_web_on_4xx() {
        let (base, seen) = spawn_fake_upstream_seq(vec![
            (400, r#"{"code":400,"msg":"task not found"}"#),
            (
                200,
                r#"{"code":0,"data":{"already_claimed":false,"credit":9,"energy":2}}"#,
            ),
        ]);
        let client = client_for(&base);
        let out = client
            .claim_reward_mp(&account(), "Sequential_Tasks_1")
            .await
            .unwrap();
        assert_eq!(out, (9, 2, false), "回落 Web 域后取 Web 域的结果");

        let got = seen.lock().unwrap();
        assert_eq!(got.len(), 2, "4xx ⇒ 必须回落，总共两次请求");
        // 第一跳：mp 域。
        assert_eq!(got[0].header("x-client-platform"), Some("miniprogram"));
        assert_eq!(got[0].path, "/activity/growth/tasks/Sequential_Tasks_1/claim");
        // 第二跳：Web 域（靠头族区分，两跳路径相同）。
        assert_eq!(got[1].header("x-client-platform"), Some("web"));
        assert_eq!(got[1].header("user-agent"), Some(crate::upstream::desktop::WEB_UA));
        assert_eq!(got[1].header("origin"), Some(base.as_str()));
    }

    /// 5xx 是上游故障，换域不会好 —— 只打一次并原样返回错误。
    #[tokio::test]
    async fn claim_reward_mp_does_not_fall_back_on_5xx() {
        let (base, seen) = spawn_fake_upstream_seq(vec![(500, r#"{"code":500,"msg":"boom"}"#)]);
        let client = client_for(&base);
        let err = client
            .claim_reward_mp(&account(), "Sequential_Tasks_1")
            .await
            .unwrap_err();
        let GatewayError::UpstreamError(ue) = &err else {
            panic!("5xx 应为 UpstreamError，实际 {err:?}");
        };
        assert_eq!(ue.status, 500);
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "★ 5xx 不得回落（Go 只在 400<=Status<500 时换 Web 域）"
        );
    }

    /// 传输层错误（连不上）同样不回落：它不是「这个域不认」。
    #[tokio::test]
    async fn claim_reward_mp_does_not_fall_back_on_transport_error() {
        // 端口 1 上不会有监听者。
        let client = client_for("http://127.0.0.1:1");
        let err = client
            .claim_reward_mp(&account(), "Sequential_Tasks_1")
            .await
            .unwrap_err();
        assert!(
            matches!(err, GatewayError::Transport(_)),
            "传输层错误应原样透出，实际 {err:?}"
        );
    }

    #[tokio::test]
    async fn claim_reward_mp_empty_code_short_circuits_without_request() {
        let (base, seen) = spawn_fake_upstream(r#"{"code":0,"data":{}}"#);
        let client = client_for(&base);
        assert_eq!(
            client.claim_reward_mp(&account(), "  ").await.unwrap(),
            (0, 0, false)
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            0,
            "空白任务码必须不发请求，且**不得**因回落而打 web 域"
        );
    }
}
