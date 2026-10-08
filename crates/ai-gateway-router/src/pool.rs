//! 账号池：单一状态机（健康/冷却/熔断）+ 在途租约 + 到期分层 + 加权挑选。
//!
//! 对应 Go 源文件 `internal/pool/pool.go`。
//!
//! # 状态模型
//!
//! 每个账号有三个正交维度：
//! 1. **健康维度**（唯一权威）：`healthy = !disabled && !until 生效 && !breaker_until 生效`
//!    - `until`：按错误类型的即时冷却（软 429 / 硬 余额耗尽）
//!    - `breaker_until`：连续失败累计触发熔断的指数退避截止
//! 2. **并发维度**：`in_flight`（在途租约，运行态）
//! 3. **统计维度**：`success_count` / `err_total` / `last_used` / `last_success` / `last_err`
//!
//! # 挑选策略（两级）
//!
//! 1. **到期分层**：先按「最近到期积分」的到期日把 healthy 候选分组，只保留最早到期的一档
//!    ——优先消耗快过期的额度；到期日未知的账号排最后。
//! 2. **档内挑选**：同档账号按「闲置补偿 + 成功率」加权取 Top5，再加权随机抽签。
//!
//! 所有账号都没有到期信息时自动退回原三因子口径
//!（credits 占比 ×10 + 闲置补偿 + 成功率 ×3）。
//!
//! # 与 Go 实现的关键差异
//!
//! - Go 用 `&entry` 指针在锁内原地改；Rust 用 `Vec<Entry>` + 索引，
//!   以避免 `Arc<RwLock<Entry>>` 的复杂度和额外分配。
//! - **保留了 Go 版修复后的 LRU 平局处理**（[`Entry::last_used_seq`]），
//!   这是 Go 侧 Windows 时钟粒度导致惊群的修复，迁移时必须带上。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::auth::Auth;
// `Region` 由下方 `pub use crate::auth::Region;` 重导出（权威定义在 auth 模块）。

/// 冷却类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CoolKind {
    /// 余额不足 → 冷却到次日 04:00（等签到恢复）。
    #[default]
    Soft,
    /// 429 → 短冷却。
    Hard,
}

impl CoolKind {
    /// 序列化名，与 Go 侧 `String()` 一致（供 /status 透出）。
    pub fn as_str(&self) -> &'static str {
        match self {
            CoolKind::Soft => "soft_rate",
            CoolKind::Hard => "hard_credit",
        }
    }
}

/// 连续 `ErrSessionDead`（12153）达到该次数才禁用账号。
///
/// # 为什么是 3 而不是 1
///
/// 12153 会被**临时性**触发（网络抖动 / 上游闪断 / refresh 竞态），
/// 单次即杀号会误杀健康账号。连续 3 次（跨多次保活周期）才认为是真的 session 死亡。
///
/// Go 侧实测发现过一批 `disabled` 账号其实 refresh 完全正常 ——
/// 它们是「一次即杀」历史行为的受害者。阈值放宽到 3 是为了不再制造这类误判。
///
/// 对应 Go `internal/pool/pool.go:3145 const sessionDeadThreshold = 3`。
pub const SESSION_DEAD_THRESHOLD: i64 = 3;

/// 因连续 session 死亡而禁用时写入 `reason` 的文案。
///
/// 与旧文案逐字一致，便于既有运维脚本按 `reason` 匹配。
/// 对应 Go `internal/pool/pool.go:3152 const sessionDeadReason = "12153 session dead"`。
pub const SESSION_DEAD_REASON: &str = "12153 session dead";

/// 单账号对外暴露的状态（脱敏），对应 Go 侧 `pool.Status`。
#[derive(Debug, Clone, Default)]
pub struct Status {
    /// 账号 UID。
    pub uid: String,
    /// 昵称。
    pub nickname: String,
    /// 剩余积分。
    pub credits: i64,
    /// 是否冷却中。
    pub cooling: bool,
    /// 冷却类型名（`"soft_rate"` / `"hard_credit"`）。
    pub cool_kind: String,
    /// 冷却剩余秒数。
    pub cool_remaining_sec: i64,
    /// 冷却截止时刻。
    pub until: Option<SystemTime>,
    /// 冷却原因。
    pub reason: String,
    /// 是否被禁用。
    pub disabled: bool,
    /// 用户手动禁用：**只不接流量**，养号任务照跑。
    ///
    /// 与 [`Status::disabled`] 分开下发，界面才能如实区分两种「不接流量」：
    ///
    /// - `no_route` → 用户自己关的；签到 / 上报 / 成长任务**仍在跑**
    /// - `disabled` → 网关判定该号已死（session 死 / 额度冻结），任务也会跳过
    ///
    /// 两者在界面上若都写成「禁用」，用户就无法判断「这个号还在不在养」——
    /// 而「禁用了但仍在养号」正是本功能的语义。
    pub no_route: bool,
    /// 累计成功数。
    pub success_count: i64,
    /// 累计错误数。
    pub err_total: i64,
    /// 最近成功时刻。
    pub last_success: Option<SystemTime>,
    /// 最近错误时刻。
    pub last_err: Option<SystemTime>,

    /// 该账号属于**哪个客户端产品**（`workbuddy` / `qoder` / `zcode`）。
    ///
    /// # 为什么必须下发（所有者 2026-09-20 要求）
    ///
    /// 原话：「在兼容网关哪里的账号池,也要标记上进入池子的账号属于那个客户端」。
    ///
    /// 三个产品的账号混在**同一个池**里（多产品路由开启时），而界面上
    /// 只有昵称/备注 —— 用户看到 `acct-a`、`acct-b-…` 这样的名字，
    /// **不知道它来自哪个客户端**。于是：
    ///
    /// - 排查「为什么 `zcode:` 前缀选不出号」时，看不出池里到底有几个 ZCode 号
    /// - 想给某个号单独停流量时，得先去别的页面确认它是哪家的
    ///
    /// ⚠ 值走 [`Auth::product_of`]（**空串归一成 workbuddy**），不是裸
    /// `a.product`：老账号的 `product` 是空串，若原样下发，界面会显示空、
    /// 或前端各自拿空串去猜 —— 那正是「同一个号在不同页面显示不同产品」的成因。
    pub product: String,

    /// 该账号所属区域（`cn` / `intl`）。
    ///
    /// # 为什么必须下发
    ///
    /// 与 `product` 同因：三个产品的账号混在同一个池里，而**区域决定了用户
    /// 最关心的几件事** —— 该模型能不能用（区域白名单）、走不走代理、
    /// 以及这个号是否适用签到/旅行这类养号任务。界面上不标，用户看到
    /// 两个同名账号就只能靠猜。
    ///
    /// ⚠ 值走 [`Auth::region`]，判据由账号自身的域名决定（[`region_of_domain`]
    /// 的**完整域名白名单**，不在清单里的按国服）。与选号用的是**同一个口径**，
    /// 不要在这里另写判断 —— 两处一旦分叉，界面显示的区域会与实际路由的区域
    /// 不一致。`region()` 永不返回 `Any`，故有账号就一定有区域。
    ///
    /// [`Auth::region`]: crate::auth::Auth::region
    /// [`region_of_domain`]: crate::auth::region_of_domain
    pub region: String,

    /// 该账号是否正因「到期档位更晚」而排队等待（当前轮不到它）。
    ///
    /// 由后端按与选号**完全相同**的档位口径算出，前端直接展示即可 ——
    /// 前端无法自行判断：它既拿不到 healthy/模型冷却/在途 这三套判定，
    /// 也没有「当前生效档位」这个池级信息。用 `success_count == 0` 之类近似
    /// 会误判（新导入或持续失败的账号同样是 0，但它们不是排队）。
    ///
    /// 语义：只表示「按分层规则现在轮不到」，不代表故障。前面的档位被消耗或
    /// 冷却后，它会自动进入路由。
    pub queued: bool,

    /// 该账号当前因「模型级限流」而冷却的模型列表（按到期时间升序）。
    ///
    /// 与 [`Status::cool_kind`] 的区别（前端据此区分两种冷却）：
    ///
    /// - `cool_kind = "hard_credit"` / `cooling = true` → 账号级：余额欠费，整号不可用
    /// - `model_cooling` 非空 → 模型级：仅这些模型不可用，换模型仍可用
    ///
    /// 两者可同时存在（例如余额充足但某模型额度用尽）。
    pub model_cooling: Vec<ModelCooling>,

    // 运行态（不持久化）
    /// 在途请求数。
    pub in_flight: i64,
    /// 熔断器连续失败计数。
    pub breaker_fails: i64,
    /// 熔断截止时刻。
    pub breaker_until: Option<SystemTime>,

    /// 「最近到期积分」的到期时刻（Unix 秒）；0 = 未知。
    pub soonest_expire_at: i64,
    /// 到期日（本地日期 `YYYY-MM-DD`），分层用的档位键。
    pub expire_day: String,
}

/// 单条「账号 + 模型」冷却的内部记录。
///
/// 与 Go 侧 `modelCool` 一致：除到期时刻外还要带上原因与「到期时间是否取自
/// 上游文案」，否则 `/status` 的 `model_cooling[].reset_at_parsed` 只能瞎猜。
#[derive(Debug, Clone)]
struct ModelCool {
    until: SystemTime,
    reason: String,
    reset_parsed: bool,
}

/// 向外部（`/status`、前端）描述一条模型冷却。
///
/// 与 Go 侧 `pool.ModelCooling` 同 tag 同语义。
#[derive(Debug, Clone, Default)]
pub struct ModelCooling {
    /// 模型名。
    pub model: String,
    /// 冷却截止时刻。
    pub until: Option<SystemTime>,
    /// 剩余秒数（向上取整，与 Go 侧 `+0.999` 口径一致）。
    pub remaining_sec: i64,
    /// 面向用户的说明（含模型名与重置时间）。
    pub reason: String,
    /// true = 到期时间取自上游文案；false = 回退固定软冷却。
    pub reset_at_parsed: bool,
}

/// 池内部账号条目。
#[derive(Debug, Clone)]
struct Entry {
    a: Auth,

    credits: i64,
    success_count: i64,
    err_total: i64,
    last_err: Option<SystemTime>,
    last_success: Option<SystemTime>,
    cool_kind: Option<CoolKind>,
    /// 冷却截止（即时冷却）。
    until: Option<SystemTime>,
    disabled: bool,
    reason: String,
    /// 最近被选中时刻（防并发撞号）。
    last_used: Option<SystemTime>,

    /// 熔断截止（指数退避）。
    breaker_until: Option<SystemTime>,
    /// 连续失败计数（熔断用，唯一权威）。
    fails: i64,
    /// 已熔断次数（指数退避的指数）。
    retry_count: i64,

    /// **连续** `ErrSessionDead`（12153）计数，达到
    /// [`SESSION_DEAD_THRESHOLD`] 才禁用。
    ///
    /// # 为什么不是「一次即杀」
    ///
    /// 旧行为是「一次 12153 即 `disable`」，但该错误会被**临时性**触发
    ///（网络抖动 / 上游闪断 / refresh 竞态）—— 一次失败就永久杀号会误杀健康账号。
    /// Go 侧实测发现过一批 `disabled` 账号其实 refresh 完全正常，是历史误判的受害者。
    ///
    /// # 为什么必须清零（而不只是累加）
    ///
    /// 「连续」正是本机制的语义核心：不清零的话，一个月的偶发抖动累计到 3 次
    /// 照样会杀号，机制退化成「累计 N 次即杀」。清零时机见
    /// [`Pool::clear_session_dead`]（refresh 成功 / chat 成功 / 手工复活）。
    ///
    /// ★ **不持久化**（与 Go 侧 `entry.sessionDeadFails` 一致）：Go 的
    /// `stateAccount` 里没有这个字段，计数器只活在内存中，重启即归零。
    /// 这是有意的 —— 重启本身就是一次「重新观察」，带着旧计数跨重启
    /// 会让「连续」跨过重启边界而失去意义。
    session_dead_fails: i64,

    /// 单账号在途请求数（运行态，不持久化）。
    in_flight: i64,

    /// 「最近到期积分」的到期时刻（Unix 秒）；0 = 未知。
    expire_at: i64,

    /// 最近一次 **token 保活成功** 的时刻（Unix 秒）；0 = 从未成功过。
    ///
    /// 持久化到 `state.json`（见 [`StateAccount::keepalive_at`]）；用途是
    /// 「启动 / 每轮巡检时判距上次成功是否已 ≥24 小时 ⇒ 补跑一次保活」。
    keepalive_at: i64,

    /// 单调序号：打破 `last_used` 的时钟粒度平局。
    ///
    /// 详见模块文档「与 Go 实现的关键差异」；对应 Go 侧 `entry.lastUsedSeq`。
    /// 0 表示从未被选中（与 `last_used = None` 对应）。
    last_used_seq: i64,

    /// 模型级冷却：`model → 冷却记录`。
    ///
    /// 与账号级 `until` 分开记账是本字段存在的全部理由：`429 code=6004` 只表示
    /// 该账号的**这个模型**额度用尽，上游明确提示可换模型继续用。若按账号整体冷却，
    /// 会连带浪费这些仍可用的额度。
    model_until: std::collections::HashMap<String, ModelCool>,

    /// 该账号当前请求模型的「单位额度消耗率」。
    ///
    /// # 为什么需要三态（`cost_rate` + `cost_known`）
    ///
    /// `0` 有两种含义：「免费」与「不知道」。若只有一个 `f64`，未声明的账号
    /// 会被当成免费号而在加权时获得最高优先级 —— 差一点就让「免费优先」
    /// 静默失效。故拆成两个字段：`cost_known` 为 true 时 `cost_rate` 才可信
    /// （此时 `0` 就是真免费）。
    ///
    /// 与 Go 侧 `entry.costRate` / `entry.costKnown` 一致。
    cost_rate: f64,
    /// `cost_rate` 是否**已声明**（见上）。
    cost_known: bool,
}

impl Entry {
    /// 返回「最近到期积分」的到期日（`YYYY-MM-DD`），空串表示未知。
    ///
    /// 用「日」而非精确时刻做分层键：上游额度按天失效，
    /// 同一天到期的账号应视为同一档。
    fn expiry_day_key(&self) -> String {
        if self.expire_at <= 0 {
            return String::new();
        }
        // Go 侧用 time.Unix(expireAt,0).In(time.Local).Format("2006-01-02")。
        expiry_day_key(self.expire_at)
    }

    /// 报告账号当前是否可选（未禁用、未处于任一冷却/熔断期、用户未关闭接流量）。
    ///
    /// # ⚠ `no_route` 必须在这里排除
    ///
    /// Go 侧踩过同一个坑两次（`pool.go:293-298` 的注释）：第一版把 no_route 只加在
    /// 主选号路径上，**兜底路径漏了** —— 结果是「被标不接流量的账号在兜底时仍会被
    /// 选中」。宿主在凭证文件里写的开关因此**完全不生效**（ZCode 侧也实测确认过
    /// 同样的缺陷）。
    ///
    /// 所以这里（`healthy`）是唯一判据：所有选号路径（主路径、兜底、按 uid、
    /// 探测）都经由它，不存在第二条需要单独维护的检查。
    fn healthy(&self, now: SystemTime) -> bool {
        if self.disabled {
            return false;
        }
        // 用户手动禁用（宿主账号页的「停止接流量」开关）。
        //
        // 与 `disabled` 语义不同：`disabled` 是网关判定该号已死，养号任务该跳过；
        // `no_route` 只是「别把用户请求路由到它」，签到 / 上报 / 成长任务**照跑**
        // （那些路径走 `available_uids` 之外的任务枚举，不经过本函数）。
        if self.a.no_route {
            return false;
        }
        if let Some(u) = self.until {
            if now < u {
                return false;
            }
        }
        if let Some(b) = self.breaker_until {
            if now < b {
                return false;
            }
        }
        true
    }

    /// 报告该账号的**指定模型**当前是否可用。
    ///
    /// 模型级冷却（429 code=6004）只影响单个模型，账号其他模型仍然可用 ——
    /// 上游文案明确写着「您也可以切换其他模型继续使用」。
    /// 因此模型冷却独立记账，**不碰**账号级的 `until`。
    fn model_available(&self, model: &str, now: SystemTime) -> bool {
        if model.is_empty() {
            return true;
        }
        match self.model_until.get(model) {
            Some(mc) => now >= mc.until,
            None => true,
        }
    }

    /// 清理**已过期**的模型冷却条目，返回是否有条目被删除。
    ///
    /// 与 Go 侧 `entry.pruneModelCoolsLocked` 逐条对应。为什么必须清理：
    /// `model_until` 是一个只增不减的 map，每个被限流过的模型名都会留下一条记录。
    /// 账号池长期运行（数周）后，一个高频账号会积累几十上百条早已过期的条目，
    /// 每次选号都要在 `model_available` 里查一遍这张越来越大的表 —— 内存与
    /// 查找成本都随运行时间单调增长。
    ///
    /// 顺带清理而不是定时清理：选号本来就在遍历所有账号，顺手做是零额外遍历成本；
    /// 单独的清理任务反而要再加一个定时器和一把锁。
    fn prune_model_cools(&mut self, now: SystemTime) -> bool {
        if self.model_until.is_empty() {
            return false;
        }
        let before = self.model_until.len();
        self.model_until.retain(|_, mc| now < mc.until);
        self.model_until.len() != before
    }

    /// 该账号当前**仍生效**的冷却截止时刻（软冷却与熔断取**更早**的那个）。
    ///
    /// 与 Go 侧 `entry.expiry(now)` 逐条对应：只考虑 `now` 之后仍生效的截止点，
    /// 两者都生效时取更早的那个（先到的截止点才是「这个号什么时候能回来」的答案）。
    ///
    /// ⚠ 不能用 `until.or(breaker_until)`：那是「优先取软冷却」，当软冷却的截止
    /// 比熔断晚时，兜底会把这个账号的恢复时间估晚，导致「明明已经能用了却一直
    /// 排在后面」。Go 侧用 `if t.IsZero() || e.breakerUntil.Before(t)` 表达取更早者。
    fn expiry(&self, now: SystemTime) -> Option<SystemTime> {
        let mut t: Option<SystemTime> = None;
        if let Some(u) = self.until {
            if now < u {
                t = Some(u);
            }
        }
        if let Some(b) = self.breaker_until {
            if now < b && t.map(|x| b < x).unwrap_or(true) {
                t = Some(b);
            }
        }
        t
    }

    /// 兜底选中该账号时，它是因「软冷却」还是「熔断」被选中的（供日志排查）。
    ///
    /// 与 Go 侧 `entry.fallbackKind(now)` 逐条对应。
    fn fallback_kind(&self, now: SystemTime) -> &'static str {
        if let Some(b) = self.breaker_until {
            if now < b {
                let soft_active = self.until.map(|u| now < u).unwrap_or(false);
                if !soft_active || self.until.map(|u| b < u).unwrap_or(true) {
                    return "breaker";
                }
            }
        }
        "soft"
    }
}

/// 服务区域。
///
/// ★ 权威定义在 [`crate::auth::Region`]（与 Go 侧 `auth.Region` 同包位置），
/// 此处**重导出**而非另立一个枚举 —— 此前 pool 与 auth 各有一份 `Region`，
/// 一旦两边的判据分叉（pool 曾用旧的 `ends_with(".ai")`）就会出现
/// 「同一个账号在选号时算国际版、在 `/status` 里算国服」这类静默不一致。
pub use crate::auth::Region;

/// 账号所属区域（不返回 `Any`）。
///
/// 与 Go 侧 `pool.regionOf` 一致：依据凭证里的 `domain` 判定。
pub fn region_of(a: &Auth) -> Region {
    a.region()
}

/// 「账号身份」的轻量投影（uid + 产品 + 区域）。
///
/// 与 Go 侧 `pool.AccountKey` 对应。供派发层在热路径上按**账号所在区域**
/// 注入成本率用（见 [`Pool::account_keys`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountKey {
    pub uid: String,
    pub product: String,
    pub region: Region,
}

/// 归一化「各产品提供哪些模型」。
///
/// 规则（与 Go 侧 `SetProductModels` 逐条对应）：
///
/// - 产品名与模型名一律 `trim` + 小写（上游写法不一，比较时统一小写）
/// - 空产品名、空模型名跳过
/// - ★ **空清单不登记**：某产品清单为空时登记成空集合会让「它不提供任何模型」
///   成立 ⇒ 它的账号全被排除，而那多半只是「宿主还没拿到清单」。
///   不登记 ⇒ 查询返回「不知道」⇒ 不排除（方向是「宁可放行」）。
pub fn normalize_product_models(
    raw: &HashMap<String, Vec<String>>,
) -> HashMap<String, HashSet<String>> {
    let mut out: HashMap<String, HashSet<String>> = HashMap::new();
    for (product, models) in raw {
        let plat = product.trim().to_ascii_lowercase();
        if plat.is_empty() {
            continue;
        }
        let mut set: HashSet<String> = HashSet::new();
        for m in models {
            let id = m.trim().to_ascii_lowercase();
            if id.is_empty() {
                continue;
            }
            set.insert(id);
        }
        // ★ 空集合不登记（见上）
        if set.is_empty() {
            continue;
        }
        out.insert(plat, set);
    }
    out
}

/// 归一化「平台 × 区域 → 模型清单」。
///
/// # 跨语言冻结契约
///
/// 与 Go 侧 `pool.normalizePlatformModels` **逐条对应**。抽成同一个 helper 是
/// 为了让它同时服务 `platform_models`（白名单）与 `platform_models_disabled`
///（否决项）—— 两者形状完全相同、只是语义相反；各写一套迟早会分叉，而分叉
/// 会让宿主下发的同一个键在两侧得到不同的解释（一类极难排查的静默失效）。
///
/// 规则：
///
/// - 平台名 / 区域名 / 模型名一律 `trim` + 小写
/// - 空平台名、空模型名跳过
/// - ★ **某平台某区域的清单为空 ⇒ 不写入** ⇒ 该组合视为「未配置」，
///   既不拦（白名单）也不否决（禁用项）。
///   界面里把清单删光的语义应是「没配」，而不是「一个都不许用」——
///   后者会让该平台彻底不可用。
pub fn normalize_platform_models(
    raw: &HashMap<String, HashMap<String, Vec<String>>>,
) -> HashMap<String, HashMap<String, HashSet<String>>> {
    let mut out: HashMap<String, HashMap<String, HashSet<String>>> = HashMap::new();
    for (platform, by_region) in raw {
        let plat = platform.trim().to_ascii_lowercase();
        if plat.is_empty() {
            continue;
        }
        let mut regions: HashMap<String, HashSet<String>> = HashMap::new();
        for (region, models) in by_region {
            let key = region.trim().to_ascii_lowercase();
            let mut set: HashSet<String> = HashSet::new();
            for m in models {
                let id = m.trim().to_ascii_lowercase();
                if id.is_empty() {
                    continue;
                }
                set.insert(id);
            }
            // ★ 空清单不写入（见上）
            if set.is_empty() {
                continue;
            }
            regions.insert(key, set);
        }
        // 平台下所有区域都空 ⇒ 整个平台视为未配置
        if regions.is_empty() {
            continue;
        }
        out.insert(plat, regions);
    }
    out
}

/// 选号时的区域约束强度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionFilter {
    /// 不做区域约束。
    Any,
    /// 只在该区域挑，挑不到返回 `None`（跨区降级会静默丢图片，不可接受）。
    ///
    /// ⚠ 这里**没有** `Prefer` 变体（曾经有，已删）：Go 侧 `PickForModelRegion`
    /// 的「偏好」语义是「先在区域内**严格**挑（不兜底），挑不到再放开到全池
    /// 走 `pickForModelAny`」—— 两个**独立**的选号调用，而不是一次调用里的
    /// 宽松区域过滤。曾经把它实现成 `RegionFilter::Prefer` 时，放开的那一步
    /// 只会放宽区域、**不会**做「该产品是否提供该模型」的两轮排除，于是
    /// `cn:Qwen3.8-Flash` 会重新出现「撞上第一个 WorkBuddy 账号回 11102」的
    /// 老缺陷。现在由 [`Pool::pick_for_model_region`] 显式分两步调用。
    Strict(Region),
}

/// 把 Unix 秒时间戳格式化为本地日期 `YYYY-MM-DD`。
///
/// 与 Go 侧 `time.Unix(n,0).In(time.Local).Format("2006-01-02")` 等价。
fn expiry_day_key(unix_secs: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(unix_secs, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d").to_string(),
        None => String::new(),
    }
}

/// 状态文件中的单个账号（与 Go 侧 `stateAccount` 同 tag）。
///
/// 只读入需要持久化的维度：冷却/禁用/统计/到期。
/// `last_used` / 在途 / 熔断的 fails/retry 是运行态，**不持久化**
///（与 Go 一致：重启后熔断计数归零，但 breaker_until 的冷却语义由 until 承担）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StateAccount {
    /// 剩余积分。
    #[serde(default)]
    pub credits: i64,
    /// 是否被禁用。
    #[serde(default)]
    pub disabled: bool,
    /// 禁用/冷却原因。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// 冷却截止时刻。
    #[serde(
        default,
        deserialize_with = "de_go_time",
        serialize_with = "ser_go_time",
        skip_serializing_if = "Option::is_none"
    )]
    pub until: Option<SystemTime>,
    /// 冷却类型编号（与 Go `CoolKind` 的 iota 一致：0=soft, 1=hard）。
    #[serde(default)]
    pub cool_kind: i32,
    /// 累计成功数。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub success_count: i64,
    /// 累计错误数。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub err_total: i64,
    /// 旧版连续错误计数（仅作一次性迁移源，不再回写）。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub err_count: i64,
    /// 最近成功时刻。
    #[serde(
        default,
        deserialize_with = "de_go_time",
        serialize_with = "ser_go_time",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_success: Option<SystemTime>,
    /// 最近错误时刻。
    #[serde(
        default,
        deserialize_with = "de_go_time",
        serialize_with = "ser_go_time",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_err: Option<SystemTime>,
    /// 「最近到期积分」的到期时刻（Unix 秒）；0/缺省 = 未知。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub expire_at: i64,
    /// 最近一次 **token 保活成功** 的时刻（Unix 秒）；0 = 从未成功过。
    ///
    /// # 为什么要持久化它（2026-10-08）
    ///
    /// 保活此前**只在配置的整点（默认 22:00）触发**，而网关是个独立子进程：
    /// 22:00 那一刻它没在跑（用户关了应用、或刚重启过），当天的保活就被**静默跳过**
    /// —— 下一次机会是 24 小时以后。而保活要防的正是「服务端清理闲置 refresh 会话」
    /// （闲置数天后刷新返回 12153 invalid_grant，账号被迫重新登录）。
    ///
    /// 有了这个时刻，网关启动时就能判「距上次成功刷新是否已 ≥24 小时」：
    /// 是则补跑一次。整点槽位照旧保留（它管的是「正常运行时的节奏」），
    /// 这个字段管的是「错过之后的兜底」。
    ///
    /// 与 `until` / `last_success` 同款序列化（Go 侧 `state.json` 可读）。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub keepalive_at: i64,
}

/// 反序列化 Go `time.Time` 的 RFC3339 字符串。
///
/// 关键边界：Go 的**零值时间**序列化为 `"0001-01-01T00:00:00Z"`，
/// 直接交给 `SystemTime` 会在 `SystemTime::UNIX_EPOCH` 之前溢出而整文件解析失败。
/// 这里把零值（以及任何无法表示的远古时间）映射为 `None`，语义与 Go 侧
/// 「`Until.IsZero()` 表示不在冷却期」完全一致。
fn de_go_time<'de, D>(d: D) -> std::result::Result<Option<SystemTime>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    let opt = Option::<String>::deserialize(d)?;
    let Some(s) = opt else { return Ok(None) };
    if s.is_empty() {
        return Ok(None);
    }
    // Go 零值时间（以及 1970 年之前的时间）→ None
    let parsed = chrono::DateTime::parse_from_rfc3339(&s).map_err(serde::de::Error::custom)?;
    let secs = parsed.timestamp();
    if secs <= 0 {
        return Ok(None);
    }
    Ok(Some(
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64),
    ))
}

/// 把 `Option<SystemTime>` 序列化为 Go 可读的 RFC3339。
///
/// 保持与 Go 侧 `time.Time` 的 JSON 形态一致，便于两边互相读取 state.json。
fn ser_go_time<S>(v: &Option<SystemTime>, s: S) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match v {
        None => s.serialize_none(),
        Some(t) => {
            let d = t
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default();
            let dt = chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                .ok_or_else(|| serde::ser::Error::custom("timestamp out of range"))?;
            s.serialize_str(&dt.to_rfc3339())
        }
    }
}

/// `0` 不序列化（对齐 Go 的 `omitempty`）。
fn is_zero(v: &i64) -> bool {
    *v == 0
}

/// 状态文件顶层结构。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StateFile {
    /// uid → 账号状态。
    #[serde(default)]
    pub accounts: HashMap<String, StateAccount>,
}

impl Pool {
    /// 从 `state.json` 载入持久化状态（覆盖/插入）。
    ///
    /// 对应 Go 侧 `load()` + `applyAccountsLocked`：文件缺失或解析失败时静默返回
    ///（首次启动属正常情况），不阻断启动。
    ///
    /// 与 Go 一致的细节：
    /// - `err_total` 与旧版 `err_count` 取较大者（旧语义下 err_count 也真实发生过错误，不应丢）。
    /// - 载入的是**占位凭证**（仅 UID）；后续 `add()` 会换成完整凭证并保留这些运行态。
    pub fn load_state(&mut self) {
        let fp = self.state_fp.clone();
        if fp.is_empty() {
            return;
        }
        let Ok(raw) = std::fs::read(&fp) else {
            return;
        };
        let Ok(sf) = serde_json::from_slice::<StateFile>(&raw) else {
            return;
        };
        self.apply_accounts(sf.accounts);
        self.loaded_from_state = true;
    }

    /// 用持久化账号状态覆盖/插入。调用方语义同 Go `applyAccountsLocked`。
    fn apply_accounts(&mut self, accounts: HashMap<String, StateAccount>) {
        for (uid, s) in accounts {
            // err_total 优先；旧文件的 err_count 作一次性迁移源（取较大者）。
            let err_total = s.err_total.max(s.err_count);
            let cool_kind = match s.cool_kind {
                1 => Some(CoolKind::Hard),
                0 if s.until.is_some() => Some(CoolKind::Soft),
                _ => None,
            };
            let entry = Entry {
                a: Auth {
                    uid: uid.clone(),
                    ..Default::default()
                },
                credits: s.credits,
                success_count: s.success_count,
                err_total,
                last_err: s.last_err,
                last_success: s.last_success,
                cool_kind,
                until: s.until,
                disabled: s.disabled,
                reason: s.reason,
                last_used: None,
                breaker_until: None,
                fails: 0,
                retry_count: 0,
                // 不持久化（Go 同）：重启即归零，见 `Entry::session_dead_fails`。
                session_dead_fails: 0,
                in_flight: 0,
                expire_at: s.expire_at,
                keepalive_at: s.keepalive_at,
                last_used_seq: 0,
                model_until: std::collections::HashMap::new(),
                cost_rate: 0.0,
                cost_known: false,
            };
            if let Some(&idx) = self.by_uid.get(&uid) {
                self.entries[idx] = entry;
            } else {
                let idx = self.entries.len();
                self.entries.push(entry);
                self.by_uid.insert(uid, idx);
            }
        }
    }

    /// 把当前状态落盘到 `state.json`。
    ///
    /// 仅在 `dirty` 时写（对应 Go 侧 `saveLocked` 的 dirty 门控）。
    /// 返回是否真的写了盘。
    pub fn flush(&mut self) -> bool {
        if !self.dirty || self.state_fp.is_empty() {
            return false;
        }
        let mut accounts = HashMap::new();
        for e in &self.entries {
            // 与 Go 侧 saveLocked 一致：只持久化有意义的维度
            let cool_kind = match e.cool_kind {
                Some(CoolKind::Soft) => 0,
                Some(CoolKind::Hard) => 1,
                None => 0,
            };
            accounts.insert(
                e.a.uid.clone(),
                StateAccount {
                    credits: e.credits,
                    disabled: e.disabled,
                    reason: e.reason.clone(),
                    until: e.until,
                    cool_kind,
                    success_count: e.success_count,
                    err_total: e.err_total,
                    err_count: 0,
                    last_success: e.last_success,
                    last_err: e.last_err,
                    expire_at: e.expire_at,
                    keepalive_at: e.keepalive_at,
                },
            );
        }
        let sf = StateFile { accounts };
        let Ok(raw) = serde_json::to_string_pretty(&sf) else {
            return false;
        };
        let path = std::path::Path::new(&self.state_fp);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        if crate::auth::atomic_write(path, raw.as_bytes()).is_ok() {
            self.dirty = false;
            true
        } else {
            false
        }
    }

    /// 是否曾经从 state.json 载入过（供启动日志判定恢复来源）。
    pub fn loaded_from_state(&self) -> bool {
        self.loaded_from_state
    }
}

/// 账号池。
pub struct Pool {
    by_uid: HashMap<String, usize>,
    entries: Vec<Entry>,
    /// 状态文件路径（空 = 不持久化）。
    state_fp: String,

    dirty: bool,

    // 可调参数
    breaker_threshold: i64,
    breaker_cooldown: Duration,
    breaker_cooldown_max: Duration,
    max_in_flight: i64,
    idle_weight_per_hour: f64,
    idle_weight_max: f64,

    /// 单调序号：每次选号自增，写入 [`Entry::last_used_seq`]。
    pick_seq: i64,
    /// 可选注入的确定性随机源（测试用）。
    rand: Option<Box<dyn Fn(i64) -> i64 + Send + Sync>>,
    /// 是否曾从 state.json 成功载入（启动日志用）。
    loaded_from_state: bool,

    // ---- 多产品路由（见 [`Pool::set_multi_product`]）----
    /// 多产品路由开关。
    ///
    /// 关闭时（默认）所有既有路径**逐字不变**：不读 `cost_rate`、不加成本乘子、
    /// 不做产品过滤。这是设计上的回滚点 —— 产品维度出问题就关掉它，立刻回到
    /// 已验证的单产品行为，而不需要回滚代码。
    multi_product_on: bool,
    /// 成本维度权重强度（`set_multi_product` 的第二个参数）。
    cost_weight: f64,
    /// 「产品 → 该产品**实际提供**的模型集合」（宿主透传，已归一化为小写）。
    ///
    /// 与 [`Pool::product_model_fallback_set`] 的分工：
    ///
    /// - 本字段（宿主清单，可信）→ 参与「不排除」与「声明」
    /// - `product_model_fallback_set`（网关兜底，猜测）→ **只**参与「不排除」
    product_model_set: HashMap<String, HashSet<String>>,
    /// 网关自己补的兜底清单（只影响「不排除」）。
    product_model_fallback_set: HashMap<String, HashSet<String>>,
    /// 「兜底清单**权威**的产品集合」（产品名，小写）。
    ///
    /// # 为什么必须区分「猜测」与「权威」（2026-10-07 所有者现场）
    ///
    /// 兜底清单有两种来源，可信度天差地别：
    ///
    /// - WorkBuddy 那份是 `static_workbuddy_model_ids`，2026-09-18 的人工快照 ——
    ///   上游新上模型它就落空，只能算**猜测**；
    /// - Trae 那份是 `products::trae::dispatch::static_trae_model_ids`，派生自
    ///   客户端别名表（文档原话：它是**唯一**的模型名真相）—— 对「上游能不能解析
    ///   这个名字」而言是**权威**的。
    ///
    /// 未区分之前，两种「未命中」被一视同仁地降级到 `guess_excluded`（最后一档
    /// 才放开）。于是业主现场：`space-bunny` 既不在 WorkBuddy 兜底清单、也不在
    /// Trae 兜底清单 ⇒ 27 个 WorkBuddy 账号与 **1 个 Trae 账号**一起进最后一档
    /// ⇒ 按权重抽签 ⇒ 约 3% 的概率抽到 Trae ⇒ 上游回 4001 ⇒ 业主看到的
    /// `400 model_not_in_upstream`「两条产品线的 function 池（chat_v3 /
    /// solo_work_lite）都拒绝了 space-bunny」。
    ///
    /// 标记为权威的产品，其兜底清单未命中即「**确定**没有该模型」
    ///（见 [`Pool::product_definitely_lacks_model`]）⇒ 该产品的账号进 `scoped`
    /// （硬排除），不再与「只是没听说过」的账号平等抽签。
    ///
    /// ⚠ 只标记**兜底清单即全量真相**的产品。WorkBuddy 绝不能进这个集合 ——
    /// 那会把它打回 2026-10-06 的 503（`space-bunny` 这类新模型在任何路径上
    /// 都选不出号）。
    product_fallback_authoritative: HashSet<String>,
    /// 「产品 → 上游**实测**存在的模型集合」（运行期从真值刷新，见
    /// `[Pool::observe_product_models`]）。
    ///
    /// # 为什么要有第三份清单（2026-10-06 所有者现场）
    ///
    /// 业主报「用 `space-bunny` 报 503 `no_healthy_account`，可这个模型
    /// WorkBuddy 明明有」。根因不在上游：`space-bunny` 是 **WorkBuddy 国服
    /// 真实模型**（网关自己的 `/debug/capability` 里 `dynamic_cn_ids` 就有它），
    /// 但它**不在**静态兜底清单里（`server.rs::static_workbuddy_model_ids`
    /// 那 30 条是 2026-09-18 的快照）⇒ `product_may_serve` 判「兜底未命中」
    /// ⇒ 27 个账号全被排除 ⇒ `pick_earliest_expiry` 只收冷却中的号 ⇒ 一个都
    /// 挑不出 ⇒ 503。
    ///
    /// ⇒ 两手一起上：① 兜底未命中**只降级、不灭绝**（见
    /// `[Pool::pick_for_model_any`]）；② 把每次拉到的上游真值记在这里，让清单
    /// **自愈** —— 新模型名不必等发版更新静态表。
    ///
    /// # 与另外两份的分工（三档语义，缺一不可）
    ///
    /// - `product_model_set`（宿主清单，**可信**）→ 参与「不排除」+「声明」竞争
    /// - `product_model_fallback_set`（网关静态兜底，**猜测**）→ 只参与
    ///   「不排除」；未命中只是**降级**到最后一档
    /// - 本字段（上游**实测**，最可信）→ 只参与「不排除」
    ///
    /// ⚠ **绝不**把实测真值合并进 `product_model_set`：那会把 workbuddy 变成
    /// 「声明方」，与 2026-09-21 的 qoder 事故同型（19 个 WorkBuddy 账号压倒
    /// 1 个 qoder 账号 ⇒ 11102）。
    product_model_observed_set: HashMap<String, HashSet<String>>,
    /// 「产品 → **已经拉到过真值**的区域集合」（见 [`Pool::observe_product_models_in_region`]）。
    ///
    /// 只在**两区都拉到过**时，才敢拿下面那张表去收紧区域 —— 一区没拉到就说
    /// 「另一个区没有这个模型」会把整批账号误杀（区域路由最忌讳的一类误判）。
    product_truth_regions: HashMap<String, HashSet<Region>>,
    /// 「产品 → 模型 → 在哪些区域**见过**它」（上游真值的区域维度）。
    ///
    /// # 为什么必须记区域（2026-10-06 真机实测）
    ///
    /// 上一轮修复只记了「这个产品有哪些模型名」—— **没有区域**。于是
    /// `space-bunny`（只在国服）与 12 个国际版账号照样匹配：裸名选号走
    /// [`Pool::pick_for_model_any`] 最后那句 `pick_inner(..., RegionFilter::Any, ...)`，
    /// 区域不限 ⇒ 抽到国际版号 ⇒ 上游回 400
    /// `model [space-bunny] service info not found`。
    ///
    /// 真机证据（隔离实例 :17864）：`/debug/capability` 的 `dynamic_cn_ids` **有**
    /// `space-bunny`、`dynamic_intl_ids` **没有**；而 `国服:space-bunny` 恒 200、
    /// `国际版:space-bunny` 恒 400。
    product_model_regions: HashMap<String, HashMap<String, HashSet<Region>>>,
    /// 「平台 → 区域 → 允许的模型」用户白名单（已归一化）。
    platform_models: HashMap<String, HashMap<String, HashSet<String>>>,
    /// 「平台 → 区域 → **禁用**的模型」否决项（已归一化）。
    ///
    /// 与白名单形状完全相同、语义相反且**更强**：命中即拦，**先于**并集判定。
    platform_models_disabled: HashMap<String, HashMap<String, HashSet<String>>>,
    /// 「模型 → 允许的平台」白名单（宿主透传的 `model_platforms`）。
    ///
    /// 与 `platform_models` 是**两个不同的东西**，别混用：
    ///
    /// - `platform_models`  键是**平台**，问「这个平台允许跑哪些模型」
    /// - `model_platforms`  键是**模型**，问「这个模型允许在哪些平台跑」
    ///
    /// 后者对应所有者需求原文「我希望可以加上**平台区分使用哪个平台的模型**」。
    /// 同名模型可能同时在多个平台上（实测 `glm-5.2` 在 WorkBuddy 与 ZCode 都有），
    /// 选号默认按权重随机，用户无法指定「这个模型走 ZCode」。而他**需要**这个
    /// 能力：各平台额度性质不同 —— ZCode 体验套餐每日重置、到期后归零 ⇒ 该优先
    /// 烧掉；WorkBuddy 是长期额度。
    ///
    /// 语义是**允许**（白名单）而不是「偏好」：只勾一个平台就等于「只用它」。
    /// ⚠ 但被允许的平台**全部不可用**时（冷却/禁用/在途满），选号会回退到全池
    /// —— 宁可回退一个「平台不符但能用」的账号，也不要让本来能成功的请求 503。
    /// 这与区域偏好的取舍一致。
    model_platforms: HashMap<String, Vec<String>>,
    /// 「平台 → 是否参加兼容网关路由」总开关（宿主透传）。
    ///
    /// ★ 语义**严格等于** `no_route` 的平台级版本 —— 所有者原话：
    ///
    /// > 「关闭平台只是不参与兼容网关,并不是不执行任务请注意」
    ///
    /// 也就是说它只把该平台的账号从**选号**里排除；签到 / 上报 / 成长任务 /
    /// 额度巡检等养号任务照跑。
    ///
    /// ⚠ 因此它**不能**写进 `Entry.disabled`（那个语义是「网关判定该号已死，
    /// 养号任务该跳过」），也**不能**影响 `probe_uids` / `available_uids`
    /// —— 探测是只读的，恰恰是这些「不接流量」的号最有价值的用途。
    ///
    /// **map 里没有该平台 ⇒ 视为开启**：老配置没有这个键，升级后行为必须逐字不变。
    platform_enabled: HashMap<String, bool>,
}

impl Default for Pool {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl Pool {
    /// 构建空池。`state_fp` 为空表示不持久化。
    pub fn new(state_fp: String) -> Self {
        Self {
            by_uid: HashMap::new(),
            entries: Vec::new(),
            state_fp,
            dirty: false,
            breaker_threshold: 3,
            breaker_cooldown: Duration::from_secs(30 * 60),
            breaker_cooldown_max: Duration::from_secs(6 * 60 * 60),
            max_in_flight: 0,
            idle_weight_per_hour: 0.5,
            idle_weight_max: 5.0,
            pick_seq: 0,
            rand: None,
            loaded_from_state: false,
            multi_product_on: false,
            cost_weight: 0.3,
            product_model_set: HashMap::new(),
            product_model_fallback_set: HashMap::new(),
            product_fallback_authoritative: HashSet::new(),
            product_model_observed_set: HashMap::new(),
            product_truth_regions: HashMap::new(),
            product_model_regions: HashMap::new(),
            platform_models: HashMap::new(),
            platform_models_disabled: HashMap::new(),
            model_platforms: HashMap::new(),
            platform_enabled: HashMap::new(),
        }
    }

    /// 注入熔断参数。
    pub fn set_breaker(&mut self, threshold: i64, cooldown: Duration, cooldown_max: Duration) {
        if threshold > 0 {
            self.breaker_threshold = threshold;
        }
        if cooldown > Duration::ZERO {
            self.breaker_cooldown = cooldown;
        }
        if cooldown_max > Duration::ZERO {
            self.breaker_cooldown_max = cooldown_max;
        }
    }

    /// 注入闲置补偿权重。
    pub fn set_weights(&mut self, idle_per_hour: f64, idle_max: f64) {
        if idle_per_hour > 0.0 {
            self.idle_weight_per_hour = idle_per_hour;
        }
        if idle_max > 0.0 {
            self.idle_weight_max = idle_max;
        }
    }

    /// 注入单账号最大在途请求数；0 = 不限。
    pub fn set_max_in_flight(&mut self, n: i64) {
        if n >= 0 {
            self.max_in_flight = n;
        }
    }

    /// 注入确定性随机源（仅供测试）；传 `None` 恢复真实随机。
    pub fn set_random_source(&mut self, f: Option<Box<dyn Fn(i64) -> i64 + Send + Sync>>) {
        self.rand = f;
    }

    // -----------------------------------------------------------------------
    // 多产品路由（对应 Go 侧 pool.go 的 SetMultiProduct / SetProductModels /
    // SetPlatformModels / PlatformAllowsModel / productMayServe / 成本 API）
    // -----------------------------------------------------------------------

    /// 开关多产品路由，并注入成本乘子强度。
    ///
    /// # 为什么做成开关（Go 侧注释原文，理由即载荷）
    ///
    /// 跨产品路由是最容易「悄悄改变现有行为」的地方，而现有单产品行为已在生产
    /// 环境跑了很久。关闭时所有既有路径**逐字不变**：不读 `cost_rate`、不加成本
    /// 乘子。这是设计要求里的**回滚点** —— 产品维度出问题就关掉它，立刻回到
    /// 已验证的单产品行为，而不需要回滚代码。
    ///
    /// `cost_weight <= 0` 时保留默认强度（0.3）。传 0 想表达「成本不参与」时，
    /// 应关闭开关而不是设 0 —— 两者语义不同：关闭 = 完全没有产品维度；
    /// 设 0 = 有产品维度但成本中性（仍可用于产品级统计与派发）。
    pub fn set_multi_product(&mut self, on: bool, cost_weight: f64) {
        self.multi_product_on = on;
        if cost_weight > 0.0 {
            self.cost_weight = cost_weight;
        }
    }

    /// 多产品路由是否开启（供诊断与测试断言）。
    pub fn multi_product_on(&self) -> bool {
        self.multi_product_on
    }

    /// 某产品的账号覆及哪些区域（去重、顺序稳定）。
    ///
    /// # 用途
    ///
    /// `/v1/models` 要生成 `平台:区域:模型名` 形式的组合名，而**只能给该产品
    /// 确实有账号的区域**生成 —— 无脑给所有区域会造出 `qoder:国服:xxx` 这类
    /// 没有账号可用的名字，用户选中后得到「账号不可用」，比不显示更糟。
    ///
    /// `Region::Any` 不返回（它表示「不限区域」，不是一个可写进前缀的区域名）。
    /// 返回顺序稳定（`Region` 的声明序）—— map 遍历无序会让每次返回的组合名
    /// 顺序不同，客户端菜单跟着跳，测试也无法断言。
    pub fn product_regions(&self, product: &str) -> Vec<Region> {
        let mut seen: Vec<Region> = Vec::new();
        for e in &self.entries {
            if e.a.product_of() != product {
                continue;
            }
            let r = e.a.region();
            if r == Region::Any || seen.contains(&r) {
                continue;
            }
            seen.push(r);
        }
        seen.sort_by_key(|r| match r {
            Region::Any => 0,
            Region::CN => 1,
            Region::Intl => 2,
        });
        seen
    }

    /// 注入「各产品实际提供哪些模型」（宿主透传）。
    ///
    /// # 为什么需要它
    ///
    /// 无前缀的模型名（如 `deepseek-v4.1-flash`）此前会被路由到**任何**当前
    /// 可用的账号上 —— 包括根本没有这个模型的产品（Go 侧实测被路由到 Qoder
    /// 并失败两次）。
    ///
    /// 入参形状：`{"workbuddy": ["deepseek-v4.1-flash", …], "qoder": [...]}`。
    /// 模型名**大小写不敏感**（上游写法不一，比较时统一小写）。
    /// 传空 map 等于「不约束」（回到既有行为）。
    pub fn set_product_models(&mut self, by_product: &HashMap<String, Vec<String>>) {
        self.product_model_set = normalize_product_models(by_product);
    }

    /// 设置网关**自己补的**兜底清单（只影响「不排除」）。
    ///
    /// 与 [`Pool::set_product_models`] 的分工：
    ///
    /// - `set_product_models`          宿主清单（可信）→ 参与「不排除」与「声明」
    /// - `set_product_models_fallback` 网关兜底（猜测）→ **只**参与「不排除」
    pub fn set_product_models_fallback(&mut self, by_product: &HashMap<String, Vec<String>>) {
        self.product_model_fallback_set = normalize_product_models(by_product);
    }

    /// 标记哪些产品的兜底清单是**权威**的（未命中即「确定没有」，见字段文档）。
    ///
    /// 调用点：`bin/gateway.rs` 只把 `trae` 放进来。**绝不要**把 `workbuddy`
    /// 也放进来 —— 它的静态表是快照，当权威会退回 2026-10-06 的 503。
    ///
    /// 传空切片等于「谁都不权威」（回到 2026-10-06 的行为）。
    pub fn set_product_fallback_authoritative(&mut self, products: &[String]) {
        self.product_fallback_authoritative = products
            .iter()
            .map(|p| p.trim().to_ascii_lowercase())
            .filter(|p| !p.is_empty())
            .collect();
    }

    /// 该产品的兜底清单是否是**权威**的（且确实存在）。
    fn fallback_authoritative(&self, product: &str) -> bool {
        if !self.product_fallback_authoritative.contains(product) {
            return false;
        }
        // 权威但**空**的兜底清单等于「网关没有更多信息」，不能当「已知没有」
        //（与 [`Pool::product_may_serve`] 里 `fb` 的非空判据保持一致）。
        self.product_model_fallback_set
            .get(product)
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    }

    /// 用上游**实测**的模型清单刷新某产品的「观测集」（只增不减）。
    ///
    /// 调用点：`[crate::server`] 的 `/v1/models` —— 那里本来就已经按区域拉了一次
    /// 上游真值（`[crate::server::region_snapshot`]`），顺手把它喂进池子是**零额外
    /// 请求**的。宿主与客户端都会定期打这个接口（宿主用它判断模型计费），
    /// 因此观测集在正常使用下始终新鲜。
    ///
    /// # 为什么只增不减
    ///
    /// 上游清单会随版本增删，但「删」几乎总是「这次没拉到」或「该账号所在区域
    /// 没有」的误判。观测集只用来**放宽**（`[Pool::product_may_serve`]`），少一条会
    /// 立刻变成「排除」，多一条的代价只是「试一次」—— 两边不对称，故取宽。
    ///
    /// ⚠ 只对**已经有兜底清单**的产品生效：没有兜底清单的产品本来就「不排除」，
    /// 给它记观测集白占内存。
    pub fn observe_product_models(&mut self, product: &str, ids: &[String]) {
        let plat = product.trim().to_ascii_lowercase();
        if plat.is_empty() || ids.is_empty() {
            return;
        }
        let has_fallback = self
            .product_model_fallback_set
            .get(&plat)
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if !has_fallback {
            return;
        }
        let set = self.product_model_observed_set.entry(plat).or_default();
        for id in ids {
            let k = id.trim().to_ascii_lowercase();
            if !k.is_empty() {
                set.insert(k);
            }
        }
    }

    /// 用上游真值按**区域**记录某产品在某区见过哪些模型。
    ///
    /// 与 `[Pool::observe_product_models`] 的分工：那个只回答「这个产品有没有这个
    /// 模型」（放宽用），本函数回答「**哪个区**有」（收紧用）。两者都只增不减。
    ///
    /// # 为什么必须两区都拉到过才敢用（`product_truth_regions`）
    ///
    /// 「国际版清单里没有 `space-bunny`」只有在**国际版清单确实拉到了**时才
    /// 成立。若国际版这次拉失败（无账号 / 上游抖动 / 退避窗口），把「没见到」当成
    /// 「没有」会把 12 个国际版账号连同它们真正支持的模型一起误杀 —— 那比原缺陷
    /// 更糟（原缺陷只是 400，误杀会变成整批 503）。故用 `truth_regions` 记「拉到过」
    /// 的区域，只有**两区都在里面**时 `[Pool::model_region_hint`] 才敢给结论。
    pub fn observe_product_models_in_region(
        &mut self,
        product: &str,
        region: Region,
        ids: &[String],
    ) {
        if region == Region::Any {
            return;
        }
        let plat = product.trim().to_ascii_lowercase();
        if plat.is_empty() || ids.is_empty() {
            return;
        }
        let has_fallback = self
            .product_model_fallback_set
            .get(&plat)
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if !has_fallback {
            return;
        }
        self.product_truth_regions
            .entry(plat.clone())
            .or_default()
            .insert(region);
        let by_model = self.product_model_regions.entry(plat).or_default();
        for id in ids {
            let k = id.trim().to_ascii_lowercase();
            if !k.is_empty() {
                by_model.entry(k).or_default().insert(region);
            }
        }
    }

    /// 上游真值给出的「该模型只在一个区域存在」结论（收紧区域用）。
    ///
    /// 返回 `Some(region)` 当且仅当：
    ///
    /// 1. 该产品**两区**的清单都拉到过（`product_truth_regions` 含 CN 与 Intl）；
    /// 2. 该模型**恰好**在其中一个区域的清单里出现过。
    ///
    /// 其余一切情况返回 `None`（= 不约束区域，保持既有行为）：
    ///
    /// - 一区或两区没拉到 ⇒ 数据不全，不能下结论；
    /// - 两区都见过 ⇒ 本来就不需要约束；
    /// - 两区都没见过 ⇒ 这个模型名不在真值里（可能只有静态表或只有兜底清单），
    ///   区域维度无从谈起 —— 交给产品维度的判据去管。
    ///
    /// # 为什么判据是「恰好一区」而不是「含某区」
    ///
    /// 同名模型在两区是**不同后端**（`glm-5.3` 国服能读图、国际版不能），
    /// 两区都见过时强行指定任一区都会白丢一半额度，且没有任何依据说哪个更好。
    pub fn model_region_hint(&self, product: &str, model: &str) -> Option<Region> {
        if model.is_empty() || !self.multi_product_on {
            return None;
        }
        let seen = self.product_truth_regions.get(product)?;
        if !(seen.contains(&Region::CN) && seen.contains(&Region::Intl)) {
            return None;
        }
        let regions = self
            .product_model_regions
            .get(product)?
            .get(&model.trim().to_ascii_lowercase())?;
        if regions.len() != 1 {
            return None;
        }
        regions.iter().next().copied()
    }

    /// 「该产品**确定**不能服务该模型」—— 即 `[Pool::pick_for_model_any`] 里
    /// `scoped`（永久排除）那一档的判据。
    ///
    /// 与 `[Pool::product_may_serve`] 的区别只有一点，但正是 2026-10-06 修复的核心：
    /// **静态兜底清单未命中不算「确定」**。
    ///
    /// `product_may_serve` 对「有兜底清单但未命中」返回 `false`（那是对的，
    /// 它回答的是「有没有依据说它能服务」），可这个 `false` 有两种完全不同的
    /// 成因：
    ///
    /// 1. 宿主清单**已知**且不含它 ⇒ **确定**没有（Trae 之于
    ///    `deepseek-v4.1-flash`：宿主下发的 trae 清单可信）；
    /// 2. 只是静态兜底清单没跟上 ⇒ 上游新上的模型（`space-bunny`）必然落这里。
    ///
    /// 旧代码把两者一视同仁地扔进 `scoped`，于是第 2 种会**灭绝**整个产品
    /// —— 27 个账号全被排除，报 503「所有账号不可用」。
    ///
    /// # 第 3 种成因（2026-10-07 新增）：兜底清单**权威**且未命中
    ///
    /// `product_may_serve` 的 `false` 还有第三种成因：网关自己补的兜底清单
    /// **未命中**。它是不是「确定」，取决于这份清单够不够格当真相 ——
    /// 见 [`Pool::product_fallback_authoritative`]：
    ///
    /// - Trae 的清单派生自客户端别名表，对「上游能不能解析这个名字」是权威的
    ///   ⇒ 未命中 = 确定没有；
    /// - WorkBuddy 的静态表是 2026-09-18 快照 ⇒ 未命中只是猜测，**不**算确定。
    ///
    /// ⚠ 实测观测集（`product_model_observed_set`）永远**不**算否定：它可能只
    /// 覆盖了一个区域，拿它当否定会误杀另一个区域独有的模型。
    pub fn product_definitely_lacks_model(&self, product: &str, model: &str) -> bool {
        if model.is_empty() || !self.multi_product_on {
            return false;
        }
        if self.product_may_serve(product, model) {
            return false;
        }
        // ① 宿主清单（最可信来源）说「没有」⇒ 确定。
        // ② 权威兜底清单未命中 ⇒ 也确定（Trae 事故的硬保护）。
        // ③ 观测集不算 —— 它可能只覆盖了一个区域。
        let (_, known) = self.product_offers_model(product, model);
        known || self.fallback_authoritative(product)
    }

    /// 报告三份清单里**有没有任何一份**包含该模型（= 有没有依据说它能服务）。
    ///
    /// 只用于诊断与降级分档，不参与「声明」竞争。
    pub fn product_model_known_anywhere(&self, product: &str, model: &str) -> bool {
        if model.is_empty() {
            return true;
        }
        let key = model.trim().to_ascii_lowercase();
        for src in [
            &self.product_model_set,
            &self.product_model_observed_set,
            &self.product_model_fallback_set,
        ] {
            if let Some(set) = src.get(product) {
                if set.contains(&key) {
                    return true;
                }
            }
        }
        false
    }

    /// 报告某产品是否提供该模型。
    ///
    /// 返回 `(是否提供, 是否有该产品的清单)`。第二个返回值让调用方能区分
    /// 「确定不提供」与「不知道」—— 后者**不该**排除账号。
    pub fn product_offers_model(&self, product: &str, model: &str) -> (bool, bool) {
        let Some(set) = self.product_model_set.get(product) else {
            return (false, false); // 不知道
        };
        if set.is_empty() {
            return (false, false);
        }
        (set.contains(&model.trim().to_ascii_lowercase()), true)
    }

    /// 注入「平台 × 区域 → 允许的模型」白名单（宿主透传）。
    ///
    /// 形状见 [`normalize_platform_models`] 的注释。传空 map 等于**清空白名单**
    /// （= 不约束任何平台，回到既有行为）。
    pub fn set_platform_models(&mut self, raw: &HashMap<String, HashMap<String, Vec<String>>>) {
        self.platform_models = normalize_platform_models(raw);
    }

    /// 注入「平台 × 区域 → **禁用**的模型」否决项（宿主透传）。
    ///
    /// 形状与 [`Pool::set_platform_models`] 完全相同，但语义相反：命中即**拦**，
    /// 且**先于**并集判定。
    ///
    /// # 归一化规则
    ///
    /// 与 `set_platform_models` **逐条一致**（两者共用 [`normalize_platform_models`]，
    /// 刻意不各写一套 —— 契约冻结为「形状完全相同」，分叉会让宿主下发的键在两侧
    /// 得到不同的解释）。
    pub fn set_platform_models_disabled(
        &mut self,
        raw: &HashMap<String, HashMap<String, Vec<String>>>,
    ) {
        self.platform_models_disabled = normalize_platform_models(raw);
    }

    /// 该平台是否参加兼容网关的**选号**。
    ///
    /// ★ 语义 = `no_route` 的平台级版本（见 `Pool.platform_enabled` 字段文档）：
    /// 只影响「挑哪个号处理用户请求」，**不影响**任何养号任务。
    ///
    /// 三态：map 里没有该平台 / 值为 `true` ⇒ 参加；显式 `false` ⇒ 不参加。
    /// 平台名按 `trim + 小写` 比对（与 `platform_models` 那一组同口径）；
    /// 空平台名一律视为参加 —— 空值表示「没指定平台」，不是「一个叫空的平台」。
    pub fn platform_enabled(&self, product: &str) -> bool {
        let plat = product.trim().to_ascii_lowercase();
        if plat.is_empty() {
            return true;
        }
        self.platform_enabled.get(&plat).copied().unwrap_or(true)
    }

    /// 注入「平台 → 是否参加路由」总开关（宿主透传 / 配置热加载）。
    ///
    /// 归一化规则与 `set_platform_models` 同口径：平台名 `trim + 小写`、
    /// 空键跳过。**只保留显式 `false` 的项**：`true` 与「键不存在」是同一
    /// 语义（参加），写进 map 只会让「宿主下发的是全量还是增量」变得难以分辨，
    /// 也让配置文件和界面状态多出一堆噪声。
    pub fn set_platform_enabled(&mut self, raw: &HashMap<String, bool>) {
        let mut next: HashMap<String, bool> = HashMap::new();
        for (k, v) in raw.iter() {
            let plat = k.trim().to_ascii_lowercase();
            if plat.is_empty() || *v {
                continue;
            }
            next.insert(plat, false);
        }
        self.platform_enabled = next;
    }

    /// 注入「模型 → 允许的平台」白名单（宿主透传，**不归一化**）。
    ///
    /// 与上面那组「平台 → 模型」白名单**方向相反**，两者共存、互不替代：
    ///
    /// - `set_platform_models`  平台 → 该平台允许跑哪些模型
    /// - `set_model_platforms`  模型 → 该模型允许跑在哪些平台
    ///
    /// # 为什么需要这个方向（所有者需求原文）
    ///
    /// > 「我希望可以加上**平台区分使用哪个平台的模型**」
    ///
    /// 背景：同名模型同时存在于多个平台 —— 实测 `glm-5.2` 在 WorkBuddy 与 ZCode
    /// 都有，`deepseek-v4.1-flash` 三个平台都有 —— 而各平台的额度**性质不同**
    ///（ZCode 体验套餐每日重置、**9/23 到期后归零** ⇒ 该优先烧；
    /// WorkBuddy 是长期额度）。没有这个开关时，同一个模型名会随机落到任一平台，
    /// 用户没法表达「这个模型就走 ZCode 烧掉快到期的额度」。
    ///
    /// # 语义是**允许**（白名单），不是「偏好」
    ///
    /// - `{"glm-5.2": ["zcode"]}`         → 只允许 ZCode
    /// - `{"glm-5.2": ["zcode","qoder"]}` → 两个都可以
    /// - **map 里没有该模型 / 值为空**     → 不限制（保持现状）
    ///
    /// ⚠ 与 Go 侧 `SetModelPlatforms` 一致**不做归一化**：键是用户写的模型名，
    /// 归一化会让「用户配了但大小写不同」和「用户没配」变得无法区分 —— 而这两者
    /// 在本函数里语义完全不同（后者 = 不限制）。匹配时按原样比较（见
    /// [`Pool::platform_allowed`]）。
    pub fn set_model_platforms(&mut self, raw: &HashMap<String, Vec<String>>) {
        self.model_platforms = raw.clone();
    }

    /// 报告「该模型是否允许跑在该平台」。
    ///
    /// 与 Go 侧 `platformAllowedLocked` 逐条对应，三态语义：
    ///
    /// - 没配任何模型白名单 / 模型名为空 / 该模型不在 map 里 / 值为空 → **放行**
    /// - 配了且含该平台 → 放行
    /// - 配了且不含该平台 → **拦**
    ///
    /// ★ `product` 为空串时归一为 WorkBuddy（默认产品）：老账号没有 `product`
    /// 字段，不归一会被**静默排除出所有请求**（症状：升级后所有请求都选不到号）。
    ///
    /// ⚠ 与 [`Pool::platform_allows_model`] 是两个不同维度、都要过：
    /// 前者答「这个平台允许跑哪些模型」，本函数答「这个模型允许跑在哪些平台」。
    pub fn platform_allowed(&self, model: &str, product: &str) -> bool {
        if self.model_platforms.is_empty() || model.is_empty() {
            return true;
        }
        let Some(allowed) = self.model_platforms.get(model) else {
            return true; // 该模型没被配置 → 不限制
        };
        if allowed.is_empty() {
            return true;
        }
        let product = if product.is_empty() {
            crate::auth::PRODUCT_WORKBUDDY
        } else {
            product
        };
        allowed.iter().any(|a| a == product)
    }

    /// 报告用户白名单是否允许「该平台的该区域」跑这个模型。
    ///
    /// 返回 `(是否允许, 是否配了白名单)`。第二个返回值让调用方区分
    /// 「用户明确禁止」与「用户没配」—— 后者**不该**拦（向后兼容）。
    ///
    /// # 查找顺序（从具体到宽松）
    ///
    /// 1. 平台 + 区域 都命中 → 按它判
    /// 2. 只有平台的「不分区域」清单（`""` 键）→ 按它判
    /// 3. 平台在 map 里但两个区域都没配 → 该平台视为「未配置」→ 不拦
    /// 4. 平台不在 map 里 → 不拦
    ///
    /// 第 2 条是为兼容「只想配一份、不区分区域」的简单用法；用户一旦按区域配了，
    /// 就走区域那份（更具体者优先）。注意该区域那份与「不分区域」那份**都判**
    /// （而不是「有区域就只看区域」）：用户可能同时配了通用清单与某区域的补充
    /// 清单，只认后者会漏掉前者。
    ///
    /// # 判据是**并集**，不是「以手动配置为准」（所有者 2026-09-28 明确）
    ///
    /// 他原话：
    ///
    /// > 「手动配置的+接口返回的,可不是以手动配置的为准」
    ///
    /// 即三个来源取**并集**，任一命中即放行：
    ///
    /// 1. `platform_models`           用户手动配的（**按平台 × 区域**）
    /// 2. `product_model_set`         接口/刷新账号返回的（不区分区域）
    /// 3. `product_model_fallback_set` 网关内置兜底（只用于「别拦我们以为能用的」）
    ///
    /// 但并集**之前**还有一个**否决项**（来源 0）：
    ///
    /// 0. `platform_models_disabled`  用户显式禁用的 → **拦**，压过上面三个的并集
    ///
    /// 来源 0 不是「第四个并集成员」，而是**前置否决** —— 它存在的理由正是并集
    /// 本身：从手动清单里删掉一个模型删不掉（另两个来源仍会放行它），故「不许
    /// 它跑」必须是一个独立于并集的判据。
    ///
    /// # 为什么必须取并集而不是「手动为准」
    ///
    /// - 用户手动配的通常**只补几个**（如 qoder 的 `deepseek-v4.1-flash`），
    ///   若以它为准，接口返回的另外 18 个会被全部挡掉 —— 那是灾难。
    /// - 反过来只认接口返回的，用户就没法用那些「上游能服务但没报出来」的模型
    ///   （实测 qoder 能服务 `deepseek-v4.1-flash` 但清单里没有）。
    ///
    /// # 三态返回
    ///
    /// ```text
    /// 配了 + 命中并集   → (true,  true)   放行
    /// 配了 + 不在并集   → (false, true)   **拦**
    /// 一个来源都没配    → (true,  false)  不拦（向后兼容）
    /// ```
    ///
    /// 第三态很重要：老配置没有 `platform_models`、账号也没刷新过（清单空）时
    /// 必须放行 —— 否则升级后所有平台都会变成「没有可用账号」。
    ///
    /// ⚠ 区域只作用于第 0/1 个来源（两者都是用户配置的，有区域维度）。
    /// 第 2/3 个来源没有区域维度，取的是跨区域超集 —— 方向是「宁可放行」，
    /// 不会误拦。用户若确实要按区域区分，用手动配置即可（它更具体，同时判）。
    pub fn platform_allows_model(&self, product: &str, region: Region, model: &str) -> (bool, bool) {
        let model = model.trim().to_ascii_lowercase();
        if model.is_empty() {
            return (true, false);
        }
        let plat = product.trim().to_ascii_lowercase();
        if plat.is_empty() {
            return (true, false);
        }
        let key = if region == Region::Intl { "intl" } else { "cn" };

        // ---- 落点 A：平台总开关（同样压过下面三个来源的并集）----
        //
        // ★ 放在**最前**、连来源 0 的否决项之前：一个被关掉的平台，无论白名单
        // 怎么写、接口返回了什么，都不该出现在选号里。返回 `(false, true)` 而不是
        // `(false, false)`：`configured=true` 表示「用户明确表过态」，调用方
        //（`forward.rs` 的准入闸）据此把请求判成「配置拒绝」而不是「暂时没有号」——
        // 后者会让用户以为是冷却、去等，而这里等多久都不会变。
        if !self.platform_enabled(&plat) {
            return (false, true);
        }

        // ---- 来源 0：用户显式禁用（否决项，压过下面三个来源的并集）----
        //
        // ⚠⚠ 必须放在**任何来源之前**。放到后面（或把它并进 `configured` 那套
        // 三态逻辑里）就失去意义了：并集的语义是「任一来源命中即放行」，
        // 而禁用的语义是「不管哪个来源说它能用，都不许跑」—— 后者必须**先**判。
        if let Some(by_region) = self.platform_models_disabled.get(&plat) {
            if !by_region.is_empty() {
                // 该区域那份与「不分区域」那份**都判**（与来源 1 同一查找口径）：
                // 用户在 `""` 里禁用的模型，两个区域都不许跑。
                for k in [key, ""] {
                    if let Some(set) = by_region.get(k) {
                        if set.contains(&model) {
                            // 显式禁用 → 拦（configured=true，与「没表态」区分）
                            return (false, true);
                        }
                    }
                }
            }
        }

        let mut configured = false;

        // ---- 来源 1：用户手动配的（按平台 × 区域，最具体）----
        if let Some(by_region) = self.platform_models.get(&plat) {
            if !by_region.is_empty() {
                for k in [key, ""] {
                    if let Some(set) = by_region.get(k) {
                        if !set.is_empty() {
                            configured = true;
                            if set.contains(&model) {
                                return (true, true);
                            }
                        }
                    }
                }
            }
        }

        // ---- 来源 2：接口/刷新账号返回的（不区分区域）----
        if let Some(set) = self.product_model_set.get(&plat) {
            if !set.is_empty() {
                configured = true;
                if set.contains(&model) {
                    return (true, true);
                }
            }
        }

        // ---- 来源 3：网关内置兜底（猜测，只用于放宽）----
        if let Some(set) = self.product_model_fallback_set.get(&plat) {
            if !set.is_empty() {
                configured = true;
                if set.contains(&model) {
                    return (true, true);
                }
            }
        }

        // ⚠⚠ 这里必须按 `configured` 分流，**不能**直接 `return (false, configured)`。
        //
        // 后者会让「一个来源都没配」（configured=false）时返回 allowed=false ——
        // 即**把「用户没表态」当成了「用户禁止」**。
        //
        // 后果是灾难性的：老配置没有 `platform_models` 键、账号也没刷新过
        //（清单为空）时，所有平台的**所有**模型都会被拦 —— 用户升级后会发现
        //「全部模型都报平台不支持」。
        if !configured {
            return (true, false); // 没配任何来源 → 不拦（向后兼容）
        }
        (false, true) // 配了但都不含 → 拦
    }

    /// 返回「该平台该区域」**允许的模型并集**（已排序）。
    ///
    /// 供错误文案展示（「该平台允许哪些」）。未配置时返回空 —— 调用方据此
    /// **不要**列清单，而不是列一个空的。
    ///
    /// # 必须是并集（与 [`Pool::platform_allows_model`] 的判据一致）
    ///
    /// 文案里列的清单必须**就是**判据用的那份，否则会出现
    /// 「它说允许 X，我发 X 却被挡」这种自相矛盾。故三个来源取并集。
    ///
    /// # 并集之后还要**扣除**被禁用的（来源 0）
    ///
    /// 否决项压过并集，故它命中的模型**必须**从这里扣除。不扣的话文案会列出
    /// 「我允许 X」，而实际发 X 会被拦 —— 那正是本函数注释开头明确要避免的
    /// 自相矛盾，只是换了个来源。
    ///
    /// ⚠ 手动配置**跨区域**可能不同，但接口返回的那份没有区域维度 —— 故并集
    /// 结果可能比「该区域实际允许的」稍多。这是**有意**的偏向：文案列多了最多
    /// 让用户试一次，列少了会让他以为某模型不可用。
    ///
    /// ⚠ 这个「宁可列多」的偏向**不适用于扣除**：扣多了会把其实允许的模型从
    /// 文案里抹掉。故扣除只认**该区域那份 + 不分区域那份**（与
    /// [`Pool::platform_allows_model`] 的来源 0 同一查找口径，逐字对应）。
    pub fn platform_allowed_models(&self, product: &str, region: Region) -> Vec<String> {
        let plat = product.trim().to_ascii_lowercase();
        if plat.is_empty() {
            return Vec::new();
        }
        let key = if region == Region::Intl { "intl" } else { "cn" };
        let mut merged: HashSet<String> = HashSet::new();

        // 来源 1：手动配置（该区域 + 不分区域）
        if let Some(by_region) = self.platform_models.get(&plat) {
            for k in [key, ""] {
                if let Some(set) = by_region.get(k) {
                    merged.extend(set.iter().cloned());
                }
            }
        }
        // 来源 2/3：接口返回 + 网关兜底（都无区域维度）
        for src in [&self.product_model_set, &self.product_model_fallback_set] {
            if let Some(set) = src.get(&plat) {
                merged.extend(set.iter().cloned());
            }
        }

        // 来源 0（否决项）：从并集里**扣除**被显式禁用的。
        //
        // 查找口径必须与 `platform_allows_model` 的来源 0 **逐字一致** ——
        // 两边不一致会重新制造「文案与判据分叉」（本函数存在的意义就是防这个）。
        if let Some(by_region) = self.platform_models_disabled.get(&plat) {
            if !by_region.is_empty() {
                for k in [key, ""] {
                    if let Some(set) = by_region.get(k) {
                        for m in set {
                            merged.remove(m);
                        }
                    }
                }
            }
        }

        let mut out: Vec<String> = merged.into_iter().collect();
        out.sort();
        out
    }

    /// 判断某账号是否**可能**服务该模型（无前缀时的额外过滤）。
    ///
    /// 规则（与 Go 侧 `productMayServeLocked` 逐条对应）：
    ///
    /// - 模型名为空        → 放行（让上游自己报错，比我们猜好）
    /// - 该产品没有清单    → 看**兜底清单**：兜底里有 → 放行，否则放行（「不知道」不等于「不提供」）
    /// - 该产品清单里有它  → 放行
    /// - 该产品清单里没它  → 兜底清单里有它则仍放行，否则**排除**
    ///
    /// # ★ 兜底清单在这里**有效**（与 `product_declares_model` 相反）
    ///
    /// 本函数问的是「是否**不排除**」，而兜底清单正是为「别拦我们以为它能用的」
    /// 而存在。取并集才安全：排除一个其实能服务的账号，用户会看到
    /// 「模型明明存在却报不可用」。
    ///
    /// ⚠ 只对**多产品开启**时生效：单产品模式下不该有任何新约束，
    /// 那是「既有行为逐字不变」的承诺。
    pub fn product_may_serve(&self, product: &str, model: &str) -> bool {
        if model.is_empty() {
            return true;
        }
        if !self.multi_product_on {
            return true;
        }
        let key = model.trim().to_ascii_lowercase();
        // 兜底清单只在**非空**时有效 —— 空的兜底清单等于「网关也没有更多信息」，
        // 那时只能按「不知道」处理（放行），不能当成「已知没有」。
        let fb = self
            .product_model_fallback_set
            .get(product)
            .filter(|fb| !fb.is_empty());
        let fallback = fb.map(|fb| fb.contains(&key)).unwrap_or(false);
        // ★ 来源 4：上游**实测**集（2026-10-06 新增，见 [Pool::observe_product_models]）。
        //
        // 它比兜底清单**强**：兜底是人工快照（会过时，`space-bunny` 就是漏网的
        // 那一个），实测是真值。命中即放行 —— 这不需要任何猜测。
        let observed = self
            .product_model_observed_set
            .get(product)
            .map(|s| s.contains(&key))
            .unwrap_or(false);
        let (offers, known) = self.product_offers_model(product, model);
        if !known {
            // ★ 宿主清单未知，但网关补过兜底清单 ⇒ **以兜底清单为准**：
            //   命中 → 放行；未命中 → **排除**。
            //
            // 为什么不能一律放行（Go 侧实测教训）：宿主那份清单来自用户的
            // 「限制使用的模型」白名单，**默认为空** ⇒ 不写 workbuddy。若这里
            // 一律放行，WorkBuddy 账号就不会被排除，而它没有 `Qwen3.8-Flash`
            // ⇒ 裸名请求撞上第一个 WorkBuddy 账号就回 11102，压根轮不到真正
            // 有这个模型的 qoder 账号。所有者现场正是如此：
            //
            //	`Qwen3.8-Flash`        → 400 model_not_in_region
            //	`qoder:Qwen3.8-Flash`  → 成功
            //
            // 关键区别：**「不知道」≠「能满足」**。兜底清单是网关**已知的更多
            // 可能性**，比「完全不知道」更有信息量，故此时以它为准。
            // ★ 实测集优先于兜底清单：它证明上游**此刻**确实有这个模型。
            if observed {
                return true;
            }
            if let Some(fb) = fb {
                return fb.contains(&key);
            }
            return true; // 连兜底清单都没有 → 真不知道 → 不排除
        }
        offers || fallback || observed
    }

    /// 判断某产品是否**明确声明**提供该模型（无前缀选号时的两轮排除用）。
    ///
    /// 与 [`Pool::product_may_serve`] 的区别正是本次修复的核心：
    /// 「不排除」（清单未知返 `true`）与「明确列出」（清单未知返 `false`）。
    ///
    /// # ★ 只看**宿主清单**，不看网关兜底清单
    ///
    /// 2026-09-22 修正。曾经把兜底也算「声明」→ 所有者现场 `GLM-5.3` 报
    /// 400 `model_not_in_region`（而 `Qwen3.8-Flash` 正常），因为 `glm-5.3`
    /// 同时被兜底清单与 qoder 可信清单声明 ⇒ 两产品都进 `declared` ⇒ 一起竞争
    /// ⇒ 19 个 WorkBuddy 账号压倒 1 个 qoder 账号 ⇒ 选到其实**没有**该模型的
    /// WorkBuddy ⇒ 11102。
    ///
    /// ⇒ **「声明」必须是可信的**，兜底清单只用于「不排除」。
    pub fn product_declares_model(&self, product: &str, model: &str) -> bool {
        if model.is_empty() || !self.multi_product_on {
            return false;
        }
        let (offers, known) = self.product_offers_model(product, model);
        known && offers
    }

    /// 「账号身份」的轻量投影，专供**热路径**上按区域注入成本率用。
    ///
    /// 为什么不复用 [`Status`]：`Status` 要算排队档位、模型冷却、成功率等一堆
    /// 东西（见 [`Pool::list`]），在每次 chat 请求的关键路径上调它是浪费。
    /// 这里只要「这个账号是哪个产品的、属于哪个区域」两个字段。
    ///
    /// 返回 uid 升序，便于测试断言。
    pub fn account_keys(&self) -> Vec<AccountKey> {
        let mut out: Vec<AccountKey> = self
            .entries
            .iter()
            .map(|e| AccountKey {
                uid: e.a.uid.clone(),
                product: e.a.product_of().to_string(),
                region: e.a.region(),
            })
            .collect();
        out.sort_by(|a, b| a.uid.cmp(&b.uid));
        out
    }

    /// 设置某账号对**当前模型**的单位额度消耗率。
    ///
    /// `known=false` 表示「未声明」（中性）；`known=true` 时 `rate` 才可信，
    /// 且 **`rate = 0` 是「免费」而不是「未声明」** —— 这个区分至关重要。
    ///
    /// 调用方（server 的派发层）在每次请求前按「该模型在该产品的成本倍率」
    /// 计算好并注入。为什么不在这里按模型查倍率：倍率表来自上游
    ///（WorkBuddy 的 `/v3/config`、Qoder 的 `model/list`），pool 不该依赖
    /// upstream（会循环依赖）。
    pub fn set_cost_rate(&mut self, uid: &str, rate: f64) {
        self.set_cost_rate_known(uid, rate, true);
    }

    /// [`Pool::set_cost_rate`] 的三态版本（见上）。
    pub fn set_cost_rate_known(&mut self, uid: &str, rate: f64, known: bool) {
        if let Some(&idx) = self.by_uid.get(uid) {
            self.entries[idx].cost_rate = rate;
            self.entries[idx].cost_known = known;
        }
    }

    /// 清空全部账号的成本率（跨请求复用池时调用，避免残留）。
    pub fn clear_cost_rates(&mut self) {
        for e in &mut self.entries {
            e.cost_rate = 0.0;
            e.cost_known = false;
        }
    }

    /// 成本乘子：消耗率越低（越便宜）→ 乘子越接近上限 `1 + cost_weight`；
    /// 消耗率越高（越贵）→ 越接近下限 `1 - cost_weight`。
    ///
    /// 用 `cost_rate / (1 + cost_rate)` 做归一化而不是线性：消耗率的量纲不确定
    ///（各产品自定义），线性映射会让某个产品的大数值把乘子压到 0 附近、等于
    /// 永久冷落它。这个映射天然落在 `[0, 1)`，不会产生极端值。
    ///
    /// `cost_weight` 取 0.3 左右时，最便宜与最贵之间的权重差异约 60% ——
    /// 足以让份额有可观测的差异，又不足以压过「同档平均分摊」。
    fn cost_multiplier(&self, cost_rate: f64) -> f64 {
        if cost_rate <= 0.0 {
            return 1.0; // 未声明 → 中性
        }
        // 归一化到 [0,1)：便宜 → 接近 0，贵 → 接近 1
        let norm = cost_rate / (1.0 + cost_rate);
        // norm=0 → 1+cost_weight；norm→1 → 1-cost_weight
        1.0 + self.cost_weight * (1.0 - 2.0 * norm)
    }

    /// 报告该账号对当前模型是否**已声明免费**（倍率 = 0）。
    ///
    /// 必须是「已声明 且 等于 0」两个条件同时成立：只有已声明才谈得上免费，
    /// 未声明是「不知道」，不能当成免费。
    pub fn cost_is_free(&self, uid: &str) -> bool {
        match self.by_uid.get(uid) {
            Some(&idx) => {
                let e = &self.entries[idx];
                e.cost_known && e.cost_rate <= 0.0
            }
            None => false,
        }
    }

    /// 内部随机源：优先注入源，否则用进程级随机。
    fn rand_n(&self, n: i64) -> i64 {
        if let Some(f) = &self.rand {
            return f(n);
        }
        // 与 Go 侧 math/rand/v2 全局源语义一致：无需密码学强度。
        use std::cell::Cell;
        thread_local! {
            // xorshift64*，基于时间 + 地址扰动播种
            static STATE: Cell<u64> = Cell::new(seed_once());
        }
        STATE.with(|s| {
            let mut x = s.get();
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            s.set(x);
            let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
            if n <= 0 {
                return 0;
            }
            ((v >> 33) as i64) % n
        })
    }

    /// 新增或更新账号。
    pub fn add(&mut self, a: Auth) {
        self.upsert(a);
    }

    /// 新增或更新单个账号（对应 Go `upsertLocked`）。
    ///
    /// 已存在时**保留运行态**（在途/熔断/冷却/统计），只刷新凭证与元数据。
    ///
    /// ★ 这条「保留」是热加载安全的前提：凭证文件被宿主刷新（换 token）后，
    /// 账号不该因此丢掉冷却与统计 —— 否则一次 `flush` 就会让正在冷却的账号
    /// 立刻复活（表现为「明明额度用完了却又被选中」）。
    fn upsert(&mut self, a: Auth) {
        if let Some(&idx) = self.by_uid.get(&a.uid) {
            let e = &mut self.entries[idx];
            let soonest = a.soonest_expire_at;
            e.a = a;
            if soonest > 0 {
                e.expire_at = soonest;
            }
            return;
        }
        let uid = a.uid.clone();
        let expire_at = a.soonest_expire_at;
        let idx = self.entries.len();
        self.entries.push(Entry {
            a,
            credits: 0,
            success_count: 0,
            err_total: 0,
            last_err: None,
            last_success: None,
            cool_kind: None,
            until: None,
            disabled: false,
            reason: String::new(),
            last_used: None,
            breaker_until: None,
            fails: 0,
            retry_count: 0,
            session_dead_fails: 0,
            in_flight: 0,
            expire_at,
            keepalive_at: 0,
            last_used_seq: 0,
            model_until: std::collections::HashMap::new(),
            cost_rate: 0.0,
            cost_known: false,
        });
        self.by_uid.insert(uid, idx);
    }

    /// 重建 `uid → entries 下标` 索引。
    ///
    /// ★ 为什么必须有它：Go 的 `byUID` 存的是**指针**，`delete(p.byUID, uid)`
    /// 一句就完成剔除；Rust 存的是 `Vec` 下标，删掉中间一个元素会让其后所有
    /// 下标**整体错位** —— 若不重建，下一次选号就会拿到**别人的凭证**
    ///（把 A 的 token 拿去发 B 的请求）。所有「删除条目」的路径都必须经由本函数。
    fn reindex(&mut self) {
        self.by_uid.clear();
        for (i, e) in self.entries.iter().enumerate() {
            self.by_uid.insert(e.a.uid.clone(), i);
        }
    }

    /// 与凭证目录**全量对齐**（对应 Go `SyncToDir`）：新账号加入、已消失的账号剔除，
    /// 已存在的账号保留运行态（凭证换新，冷却/熔断/统计不动）。
    ///
    /// ★ 调用方前提（Go `pool.go:1404-1407` 逐字警告）：本方法的剔除是**按 uid
    /// 全量比对**的 —— `auths` 必须覆盖池里「归它管」的**全部**账号，否则没交出去的
    /// 那些会被当成「凭证文件已删除」删掉。启动路径满足该前提（此时池里只有本目录的
    /// 账号）；**运行期的热加载不满足**，必须改用 [`Pool::sync_to_dir_keeping`]。
    pub fn sync_to_dir(&mut self, auths: &[Auth]) {
        self.sync_to_dir_keeping(auths, |_| false);
    }

    /// 同 [`Pool::sync_to_dir`]，但 `keep` 返回 `true` 的账号**不剔除**
    ///（对应 Go `syncToDirLocked(auths, keep)`）。
    ///
    /// ★ 热加载必须传 keep（见 `crate::authwatch::keep_workbuddy_only`）：
    /// 池里同时装着三个产品的账号，而本目录只管 WorkBuddy —— 直接全量剔除会在
    /// 一次热加载后把 Qoder / ZCode 的账号全部删出池子，且要重启才回来。
    pub fn sync_to_dir_keeping<F>(&mut self, auths: &[Auth], keep: F)
    where
        F: Fn(&Auth) -> bool,
    {
        let mut seen: HashSet<String> = HashSet::with_capacity(auths.len());
        for a in auths {
            seen.insert(a.uid.clone());
            self.upsert(a.clone());
        }

        let old = std::mem::take(&mut self.entries);
        let mut kept: Vec<Entry> = Vec::with_capacity(old.len());
        let mut removed = false;
        for e in old {
            if seen.contains(&e.a.uid) || keep(&e.a) {
                kept.push(e);
            } else {
                removed = true;
            }
        }
        self.entries = kept;
        if removed {
            // 下标错位必须重建，且要置脏让下一次 flush 把剔除结果落盘
            //（对应 Go 的 `changed` 门控 + `saveLocked()`）。
            self.reindex();
            self.dirty = true;
        }
    }

    /// 报告账号是否已占满在途名额（max=0 不限 → 恒 false）。
    fn in_flight_full(&self, e: &Entry) -> bool {
        if self.max_in_flight <= 0 {
            return false;
        }
        e.in_flight >= self.max_in_flight
    }

    /// 占用在途名额；已满返回 false。
    pub fn acquire(&mut self, uid: &str) -> bool {
        let Some(&idx) = self.by_uid.get(uid) else {
            return false;
        };
        if self.in_flight_full(&self.entries[idx]) {
            return false;
        }
        self.entries[idx].in_flight += 1;
        true
    }

    /// 释放在途名额。
    pub fn release(&mut self, uid: &str) {
        if let Some(&idx) = self.by_uid.get(uid) {
            let e = &mut self.entries[idx];
            if e.in_flight > 0 {
                e.in_flight -= 1;
            }
        }
    }

    /// 记录一次成功：累加成功计数、清连续失败与熔断运行态。
    ///
    /// 与 Go 侧 `NoteSuccess` 一致：清 fails / retry_count / breaker_until，
    /// 不碰 until / cool_kind（那些是即时冷却，各自到期）。
    ///
    /// ★ 同时清零 [`Entry::session_dead_fails`]：一次 chat 成功就证明 session
    /// 没死，连续 12153 计数必须归零。不清零的话，一个月的偶发抖动会累计到
    /// 阈值照样杀号 —— 而「连续」正是该机制的语义核心。
    pub fn note_success(&mut self, uid: &str) {
        let Some(&idx) = self.by_uid.get(uid) else { return };
        let e = &mut self.entries[idx];
        e.success_count += 1;
        e.last_success = Some(SystemTime::now());
        e.fails = 0;
        e.retry_count = 0;
        e.breaker_until = None;
        e.session_dead_fails = 0;
        self.dirty = true;
    }

    /// 记录一次错误：喂入连续失败计数器，达到阈值触发熔断（指数退避）。
    ///
    /// 对应 Go 侧 `NoteError`：`errTotal++` 后调用 `recordBreakerFailureLocked`，
    /// 而 `fails++` 发生在 `recordBreakerFailureLocked` 内部（Go 版实现），
    /// 因此这里**不能**再额外自增，否则阈值会少一次即触发。
    pub fn note_error(&mut self, uid: &str) {
        let Some(&idx) = self.by_uid.get(uid) else { return };
        let e = &mut self.entries[idx];
        e.err_total += 1;
        e.last_err = Some(SystemTime::now());
        self.record_breaker_failure(idx);
        self.dirty = true;
    }

    /// 累计一次熔断失败；达到阈值则按指数退避熔断。
    ///
    /// 熔断与冷却（until）解耦：冷却按错误类别给固定时长，
    /// 熔断则对「反复失败」逐次加长封禁。
    ///
    /// 与 Go 侧 `recordBreakerFailureLocked` 一致：**`fails++` 在本函数内完成**，
    /// 因此所有失败入口（`NoteError` / `Cooldown`）都只是调用它，不自行自增。
    fn record_breaker_failure(&mut self, idx: usize) {
        // fails++ 在此完成（与 Go 版一致）：失败入口不自行自增。
        self.entries[idx].fails += 1;
        let fails = self.entries[idx].fails;
        let retry = self.entries[idx].retry_count;
        if fails < self.breaker_threshold {
            return;
        }
        let mut d = self.breaker_cooldown;
        for _ in 0..retry {
            d = d.saturating_mul(2);
            if d >= self.breaker_cooldown_max {
                d = self.breaker_cooldown_max;
                break;
            }
        }
        let e = &mut self.entries[idx];
        e.fails = 0;
        e.retry_count += 1;
        e.breaker_until = Some(SystemTime::now() + d);
    }

    /// 即时冷却（软 429 / 硬 余额耗尽）。
    ///
    /// 冷却入口同时是熔断器的失败信号：喂入 fails，达到阈值按指数退避熔断。
    pub fn cooldown(&mut self, uid: &str, kind: CoolKind, d: Duration, reason: &str) {
        let Some(&idx) = self.by_uid.get(uid) else { return };
        let e = &mut self.entries[idx];
        e.until = Some(SystemTime::now() + d);
        e.cool_kind = Some(kind);
        e.reason = reason.to_string();
        self.record_breaker_failure(idx);
        self.dirty = true;
    }

    /// 冷却到下一个 04:00（本地时区），用于余额耗尽等签到恢复。
    pub fn cooldown_until_tomorrow_4am(&mut self, uid: &str, reason: &str) {
        let now = chrono::Local::now();
        let d = next_day_4am_duration(now);
        self.cooldown(uid, CoolKind::Hard, d, reason);
    }

    /// 永久禁用（session 死亡），需人工重登。
    pub fn disable(&mut self, uid: &str, reason: &str) {
        let Some(&idx) = self.by_uid.get(uid) else { return };
        let e = &mut self.entries[idx];
        e.disabled = true;
        e.reason = reason.to_string();
        self.dirty = true;
    }

    /// 累计一次 `ErrSessionDead`（12153），达到 [`SESSION_DEAD_THRESHOLD`] 才禁用。
    ///
    /// 返回 `true` 表示本次已达阈值并**完成禁用**（调用方据此决定是否记日志）；
    /// 返回 `false` 表示只是计数 +1，账号仍可用。
    ///
    /// # 为什么不用 [`Pool::disable`]
    ///
    /// `disable` 是「一次即杀」。12153 会被临时性触发，单次即杀会误杀健康账号
    ///（Go 侧实测有一批 `disabled` 账号 refresh 完全正常）。本方法用**连续**计数
    /// 把临时抖动与真的 session 死亡区分开。
    ///
    /// ★ 达阈值时**重置计数为 0**（与 Go 一致）：这样账号被
    /// [`Pool::revive_disabled`] 复活后，计数从零开始重新观察，
    /// 而不是「再抖一次就立刻又杀」。
    ///
    /// 对应 Go `internal/pool/pool.go:3167 NoteSessionDead`。
    pub fn note_session_dead(&mut self, uid: &str) -> bool {
        let Some(&idx) = self.by_uid.get(uid) else {
            return false;
        };
        let e = &mut self.entries[idx];
        e.session_dead_fails += 1;
        self.dirty = true;
        if e.session_dead_fails < SESSION_DEAD_THRESHOLD {
            return false;
        }
        e.session_dead_fails = 0;
        e.disabled = true;
        e.reason = SESSION_DEAD_REASON.to_string();
        true
    }

    /// 清零连续 12153 计数，**不改任何其它状态**。
    ///
    /// 任何「证明账号没死」的时刻都应调用它：refresh 成功、chat 成功、
    /// 手工复活。不清零的话，一个月的偶发抖动累计到 3 次照样会杀号 ——
    /// 而「连续」正是本修复的语义核心，累计计数会让它退化成「累计 3 次即杀」。
    ///
    /// ★ **不置 `dirty`**（与 Go 一致）：该计数不持久化，清零没有需要落盘的
    /// 状态变化。多置一次 `dirty` 只会让后台 flush 白写一遍 state.json。
    ///
    /// 对应 Go `internal/pool/pool.go:3190 ClearSessionDead`。
    pub fn clear_session_dead(&mut self, uid: &str) {
        let Some(&idx) = self.by_uid.get(uid) else { return };
        self.entries[idx].session_dead_fails = 0;
    }

    /// 记录一次 **token 保活成功** 的时刻（Unix 秒），并置 `dirty`。
    ///
    /// # 为什么要落盘（2026-10-08）
    ///
    /// 保活此前只在配置整点（默认 22:00）触发，而网关是独立子进程：那一刻它没在跑
    /// （应用关了 / 刚重启过），当天保活就被**静默跳过**，下次机会是 24 小时后。
    /// 而保活要防的正是「服务端清理闲置 refresh 会话」（闲置数天后 12153 invalid_grant，
    /// 账号被迫重新登录）。故把这个时刻持久化，启动时据此判「是否已 ≥24 小时」。
    ///
    /// ⚠ 置 `dirty`：这是**需要落盘**的状态（与 `clear_session_dead` 相反）。
    pub fn set_keepalive_at(&mut self, uid: &str, at_unix: i64) {
        let Some(&idx) = self.by_uid.get(uid) else { return };
        self.entries[idx].keepalive_at = at_unix;
        self.dirty = true;
    }

    /// 该账号最近一次保活成功的时刻（Unix 秒）；0 = 从未成功过。
    ///
    /// UID 不在池中返回 0（与 `session_dead_fails` 同口径）。
    pub fn keepalive_at(&self, uid: &str) -> i64 {
        match self.by_uid.get(uid) {
            Some(&idx) => self.entries[idx].keepalive_at,
            None => 0,
        }
    }

    /// 池内所有账号的 `(uid, keepalive_at)` 快照（保活巡检据此判哪些该补跑）。
    pub fn keepalive_snapshot(&self) -> Vec<(String, i64)> {
        self.entries
            .iter()
            .map(|e| (e.a.uid.clone(), e.keepalive_at))
            .collect()
    }
    /// 当前**连续** 12153 计数（供 scheduler 日志与测试断言）。
    ///
    /// UID 不在池中返回 0（与 Go 一致：`byUID` 未命中时返回零值）。
    ///
    /// 对应 Go `internal/pool/pool.go:3237 SessionDeadFails`。
    pub fn session_dead_fails(&self, uid: &str) -> i64 {
        match self.by_uid.get(uid) {
            Some(&idx) => self.entries[idx].session_dead_fails,
            None => 0,
        }
    }

    /// 阈值常量（供 scheduler / server 记日志时引用，避免各处硬编码）。
    ///
    /// 对应 Go `internal/pool/pool.go:3155 SessionDeadThreshold`。
    pub fn session_dead_threshold(&self) -> i64 {
        SESSION_DEAD_THRESHOLD
    }

    /// **模型级**冷却：只冷却该账号的这一个模型，账号其他模型不受影响。
    ///
    /// 与 [`Pool::cooldown`] 的关键区别：本方法**不喂熔断器** ——
    /// 模型限流是配额信号而非账号故障，喂熔断会把整个账号封掉，
    /// 连带浪费该账号其他模型仍可用的额度。
    ///
    /// 到期时间优先取上游文案里的重置时刻（见 `classify::parse_reset_time`），
    /// 解析不出才回退固定软冷却。
    pub fn cooldown_model(&mut self, uid: &str, model: &str, until: SystemTime, reason: &str) {
        self.cooldown_model_ex(uid, model, until, reason, false)
    }

    /// [`Pool::cooldown_model`] 的完整版本：额外记录到期时间是否取自上游文案。
    ///
    /// `reset_parsed = true` 表示 `until` 是从上游 429 文案里解析出来的真实重置
    /// 时刻，而不是回退的固定软冷却 —— 前端据此区分「确切知道什么时候恢复」
    /// 与「大概等一会儿」。
    pub fn cooldown_model_ex(
        &mut self,
        uid: &str,
        model: &str,
        until: SystemTime,
        reason: &str,
        reset_parsed: bool,
    ) {
        if model.is_empty() {
            // 无模型信息时退化为账号级软冷却，避免「模型限流」被静默忽略。
            let d = until.duration_since(SystemTime::now()).unwrap_or_default();
            self.cooldown(uid, CoolKind::Soft, d, reason);
            return;
        }
        let Some(&idx) = self.by_uid.get(uid) else { return };
        let e = &mut self.entries[idx];
        e.model_until.insert(
            model.to_string(),
            ModelCool {
                until,
                reason: reason.to_string(),
                reset_parsed,
            },
        );
        e.reason = reason.to_string();
        self.dirty = true;
    }

    /// 该账号当前处于冷却中的模型列表（`(模型名, 剩余秒数)`），供 `/status` 观测。
    pub fn cooling_models(&self, uid: &str) -> Vec<(String, i64)> {
        self.model_cooling(uid)
            .into_iter()
            .map(|mc| (mc.model, mc.remaining_sec))
            .collect()
    }

    /// 该账号当前仍在生效的模型冷却（按到期时间升序），供 `/status` 透出。
    ///
    /// 与 Go 侧 `entry.modelCoolingList` 同语义：只返回**尚未过期**的条目，
    /// 剩余秒数向上取整（Go 侧 `int64(sec + 0.999)`），保证「还剩 0.4 秒」
    /// 不会显示成 0 秒而让用户以为已经恢复。
    pub fn model_cooling(&self, uid: &str) -> Vec<ModelCooling> {
        let now = SystemTime::now();
        let Some(&idx) = self.by_uid.get(uid) else {
            return Vec::new();
        };
        let mut out: Vec<ModelCooling> = self.entries[idx]
            .model_until
            .iter()
            .filter(|(_, mc)| now < mc.until)
            .map(|(m, mc)| {
                let rem = mc
                    .until
                    .duration_since(now)
                    .map(|d| d.as_secs() as i64 + i64::from(d.subsec_nanos() > 0))
                    .unwrap_or(0);
                ModelCooling {
                    model: m.clone(),
                    until: Some(mc.until),
                    remaining_sec: rem,
                    reason: mc.reason.clone(),
                    reset_at_parsed: mc.reset_parsed,
                }
            })
            .collect();
        out.sort_by(|a, b| a.until.cmp(&b.until));
        out
    }

    /// 更新余额。
    pub fn set_credits(&mut self, uid: &str, credits: i64) {
        let Some(&idx) = self.by_uid.get(uid) else { return };
        self.entries[idx].credits = credits;
        self.dirty = true;
    }

    /// 更新余额与到期时刻。
    pub fn set_credits_and_expiry(&mut self, uid: &str, credits: i64, soonest_expire_at: i64) {
        let Some(&idx) = self.by_uid.get(uid) else { return };
        let e = &mut self.entries[idx];
        e.credits = credits;
        if soonest_expire_at > 0 {
            e.expire_at = soonest_expire_at;
        }
        self.dirty = true;
    }

    /// 签到后解冻：仅当 remain > 0 且账号非禁用时清冷却（余额恢复）。
    ///
    /// 注意：不碰熔断器——熔断到期或下次 chat 成功才恢复。
    pub fn reenable_if_credits(&mut self, uid: &str, remain: i64) {
        let Some(&idx) = self.by_uid.get(uid) else { return };
        let e = &mut self.entries[idx];
        if remain > 0 && !e.disabled {
            e.credits = remain;
            e.until = None;
            e.cool_kind = None;
            e.reason.clear();
        } else {
            e.credits = remain;
        }
        self.dirty = true;
    }

    /// 全量状态列表（按 UID 排序，稳定输出）。
    pub fn list(&self) -> Vec<Status> {
        let now = SystemTime::now();
        // 先算出「当前会被路由的那一档」，供每个账号标注是否在排队。
        //
        // 为什么必须由后端算：前端拿不到档位规则（要复刻 healthy + 模型冷却 +
        // 到期分层三套判定），而用「success_count == 0」之类的近似会误判 ——
        // 新导入的账号、或一直失败的账号同样是 0，但它们不是排队。只有用与选号
        // **完全相同**的口径，「排队中」才是可信的事实而非猜测。
        let routed_day = self.routed_tier_day(now);
        let mut out: Vec<Status> = self
            .entries
            .iter()
            .map(|e| {
                let mut st = self.status_of(e, now);
                if !routed_day.is_empty() && st.expire_day > routed_day {
                    // 到期日更晚 ⇒ 不在当前档位 ⇒ 排队等待
                    st.queued = true;
                }
                st
            })
            .collect();
        out.sort_by(|a, b| a.uid.cmp(&b.uid));
        out
    }

    /// 返回当前实际会被路由的到期日档位（空串 = 无法确定）。
    ///
    /// 口径与 pick 保持一致：剔除禁用、账号级冷却、在途占满的账号，
    /// 取「最早到期日」那一档。
    ///
    /// # 关于**模型冷却**（这里是 Go 侧踩过的坑，务必保留这条排除）
    ///
    /// 档位本身是账号级属性，但「当前是否轮得到」取决于**本次请求的模型**。
    /// Go 侧实测现场：4 个更早档位的账号对主力模型处于模型冷却，网关于是跳过
    /// 它们、把流量给了更晚档位的 8 个账号。若这里不排除模型冷却账号，就会算出
    /// 「最早档位是 10-01」，进而把真正在服务的 8 个账号全标成「排队」，还把
    /// 冷却中的账号标成「会路由」—— 与事实完全相反。
    /// 已由 Go 侧 `TestQueuedFlagMatchesLiveScenario` 锁定。
    ///
    /// 以「是否存在至少一个模型处于冷却」作为排除依据：只要有模型在冷却，
    /// 该账号对那个模型就不可用，不能代表「轮得到」。这是保守估计，
    /// 对单模型为主的用法足够准确。
    fn routed_tier_day(&self, now: SystemTime) -> String {
        let mut best = String::new();
        for e in &self.entries {
            if !e.healthy(now) || self.in_flight_full(e) {
                continue;
            }
            if e.model_until.values().any(|mc| now < mc.until) {
                continue;
            }
            let day = e.expiry_day_key();
            if day.is_empty() {
                continue;
            }
            if best.is_empty() || day < best {
                best = day;
            }
        }
        best
    }

    /// 构造单个账号的对外状态。
    fn status_of(&self, e: &Entry, now: SystemTime) -> Status {
        let cooling = e
            .until
            .map(|u| now < u)
            .unwrap_or(false)
            || e.breaker_until.map(|b| now < b).unwrap_or(false);
        let cool_remaining = e
            .until
            .filter(|u| now < *u)
            .and_then(|u| u.duration_since(now).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        Status {
            uid: e.a.uid.clone(),
            nickname: e.a.nickname.clone(),
            credits: e.credits,
            cooling,
            cool_kind: if e.cool_kind.is_some() {
                e.cool_kind.unwrap().as_str().to_string()
            } else {
                String::new()
            },
            cool_remaining_sec: cool_remaining,
            until: e.until,
            reason: e.reason.clone(),
            disabled: e.disabled,
            no_route: e.a.no_route,
            success_count: e.success_count,
            err_total: e.err_total,
            last_success: e.last_success,
            last_err: e.last_err,
            // ★ 走归一化后的 product_of()，不是裸 a.product：老账号的 product
            // 是空串，原样下发会让界面显示空、或前端各自拿空串去猜。
            product: e.a.product_of().to_string(),
            // ★ 与选号共用同一个判据（auth::region_of_domain），
            // 不要在这里另写判断 —— 两处分叉会导致界面区域与实际路由不一致。
            region: e.a.region().as_str().to_string(),
            // queued 由 list() 按池级档位填，单账号构造时恒 false。
            queued: false,
            model_cooling: self.model_cooling(&e.a.uid),
            in_flight: e.in_flight,
            breaker_fails: e.fails,
            breaker_until: e.breaker_until,
            soonest_expire_at: e.expire_at,
            expire_day: e.expiry_day_key(),
        }
    }

    /// 五类计数：total / healthy / cooling / disabled / in_flight_full。
    pub fn counts_detailed(&self) -> (usize, usize, usize, usize, usize) {
        let now = SystemTime::now();
        let mut healthy = 0usize;
        let mut cooling = 0usize;
        let mut disabled = 0usize;
        let mut in_flight_full = 0usize;
        for e in &self.entries {
            if e.disabled {
                disabled += 1;
            }
            let is_cooling = e.until.map(|u| now < u).unwrap_or(false)
                || e.breaker_until.map(|b| now < b).unwrap_or(false);
            if is_cooling {
                cooling += 1;
            }
            if e.healthy(now) {
                healthy += 1;
                if self.in_flight_full(e) {
                    in_flight_full += 1;
                }
            }
        }
        (self.entries.len(), healthy, cooling, disabled, in_flight_full)
    }

    /// 当前**在途**（已分配名额、尚未释放）的请求数合计。
    ///
    /// # 为什么必须单独暴露（2026-10-08）
    ///
    /// 业主排查「对话输出完就中断」时，最需要回答的一个问题是
    /// **「重启网关的那一刻，有没有正在服务的流」** —— 有在途流就重启，
    /// 那些流会被拦腰截断（客户端看到的就是「本轮运行失败」）。
    ///
    /// 但池子此前只对外暴露 `in_flight_full`（**占满名额的账号数**，
    /// 见 [`Pool::counts_detailed`]），它是「有几个账号被顶满了」，
    /// 不是「有几个请求在飞」。用一个恒为 0/1 的计数去判「现在安全吗」，
    /// 结论必然是错的 —— 静默期的 1 个在途流与 27 个账号全满，
    /// 在这个字段上可能完全一样。
    ///
    /// 故这里给出**请求维度**的合计值：`in_flight == 0` 才是「现在重启不会
    /// 打断任何流」的硬判据（配合宿主侧的优雅重启，见 `restart_gateway_serialized`）。
    pub fn in_flight_total(&self) -> usize {
        self.entries.iter().map(|e| e.in_flight.max(0) as usize).sum()
    }

    /// 当前是否有账号可受理（healthy 且未占满在途）。
    pub fn servable_now(&self) -> bool {
        let now = SystemTime::now();
        self.entries
            .iter()
            .any(|e| e.healthy(now) && !self.in_flight_full(e))
    }

    /// 返回当前 healthy 且未占满在途名额的 UID 列表（按 UID 排序，稳定输出）。
    pub fn available_uids(&self) -> Vec<String> {
        let now = SystemTime::now();
        let mut uids: Vec<String> = self
            .entries
            .iter()
            .filter(|e| e.healthy(now) && !self.in_flight_full(e))
            .map(|e| e.a.uid.clone())
            .collect();
        uids.sort();
        uids
    }

    /// 按 UID 直取账号凭证，**不做任何健康检查**（对应 Go `AuthByUID`）。
    ///
    /// # 与 [`Pool::pick_by_uid`] 的分工（别混用）
    ///
    /// - `pick_by_uid`：**选号**用。会做 `healthy` 检查、会 `mark_used` 防撞号。
    /// - `auth_by_uid`：**观测**用。只读快照，禁用/冷却/不接流量的账号照样返回 ——
    ///   区域诊断恰恰最需要看到「正在冷却的那个号属于哪个区域」，而
    ///   `pick_by_uid` 对它们返回 `None`，会让最该看见的账号恰好不可见。
    ///
    /// 返回的是**克隆**（Go 返回指针）：Rust 侧池由 `Mutex` 保护，把 `&Auth`
    /// 借出去会把锁的生命周期绑到调用方，而调用方往往要跨 `await` 使用它。
    pub fn auth_by_uid(&self, uid: &str) -> Option<Auth> {
        let &idx = self.by_uid.get(uid)?;
        Some(self.entries[idx].a.clone())
    }

    /// 池中**全部**账号的 UID（含冷却/熔断/禁用），按 UID 排序。
    ///
    /// 与 [`Pool::available_uids`] 的区别是只读观测用途：区域诊断要能看到
    /// 「池里到底有哪些账号、各自属于哪个区域」，若只列 healthy 账号，
    /// 故障排查时最需要看的那几个（正在冷却的）恰好不可见。
    pub fn all_uids(&self) -> Vec<String> {
        let mut uids: Vec<String> = self.entries.iter().map(|e| e.a.uid.clone()).collect();
        uids.sort();
        uids
    }

    /// 可用于**只读探测**的账号 UID（按 UID 排序）。对应 Go `ProbeUIDs`。
    ///
    /// 与 [`Pool::available_uids`] 的唯一区别：**包含被用户标记「不接流量」
    /// （`no_route`）的账号**。
    ///
    /// # 为什么探测要包含它们
    ///
    /// `no_route` 的语义是「别把**用户请求**路由到它」，而拉一次 `/v3/config`
    /// 是只读的、不产生任何流量或积分消耗 —— 它既不违反用户意图，又恰恰是
    /// 这些账号最有价值的用途。
    ///
    /// 实测踩过的坑（Go `pool.go:3369-3373` 逐字）：`pickProbeAccountInRegion`
    /// 原先用 `AvailableUIDs`，而 `no_route` 被加进 `healthy()` 之后，被停用的
    /// 国际版账号**既不接流量、也拿不到区域真值了** —— 表现为
    /// 「`deepseek-v4.1-flash` 明明两区都有，界面却标『仅国服』」
    /// （国际版拉不到清单，于是被当成「该区没有这个模型」）。
    ///
    /// 仍**排除** `disabled`（网关判定 session 死 / 额度冻结）：那种账号连
    /// `/v3/config` 都会失败，拿来探测只会白跑一轮并写进日志噪音。
    pub fn probe_uids(&self) -> Vec<String> {
        let now = SystemTime::now();
        let mut uids: Vec<String> = self
            .entries
            .iter()
            .filter(|e| {
                if e.disabled {
                    return false;
                }
                // 只看「未在冷却/熔断期」：探测失败多半是账号凭证问题，
                // 与冷却无关，但冷却中的账号同样会失败，跳过即可。
                if let Some(u) = e.until {
                    if now < u {
                        return false;
                    }
                }
                if let Some(b) = e.breaker_until {
                    if now < b {
                        return false;
                    }
                }
                !e.a.access_token.is_empty()
            })
            .map(|e| e.a.uid.clone())
            .collect();
        uids.sort();
        uids
    }

    /// 单账号状态（对应 Go `Pool.Status(uid)`）；UID 不在池中返回 `None`。
    ///
    /// `queued` 恒 `false`：它由 [`Pool::list`] 按池级档位填，单账号查询没有
    /// 池级上下文。运维端点用不到该字段。
    pub fn status_by_uid(&self, uid: &str) -> Option<Status> {
        let &idx = self.by_uid.get(uid)?;
        Some(self.status_of(&self.entries[idx], SystemTime::now()))
    }

    /// 复活一个被**系统自动禁用**的账号（对应 Go `ReviveDisabled`）。
    ///
    /// # 语义边界（必须与 Go 逐字一致）
    ///
    /// - 只清**系统判定位** `disabled` / `reason`；
    /// - **不动**用户的手工轴 `no_route`（宿主写进凭证文件的「停止接流量」开关）；
    /// - **不动**熔断与账号级冷却（它们各有自己的到期路径，复活不假装账号健康）。
    ///
    /// 因此「复活成功但仍不接流量」是**正常**结果，调用方必须如实透出原因
    /// （见 `server::debug_revive_account` 的 `notes`），否则用户会以为复活没生效。
    ///
    /// 幂等：账号本来就启用 → 返回 `false`（不是错误，客户端可安全重试）。
    /// UID 不在池中同样返回 `false`（调用方先用 [`Pool::status_by_uid`] 区分 404）。
    ///
    /// ★ 返回 `false` 有**两种**含义，调用方必须先用 `status_by_uid` 判存在性 ——
    /// 这正是 Go 侧 `debugReviveAccount` 先查 `Status(uid)` 再调本函数的原因。
    pub fn revive_disabled(&mut self, uid: &str) -> bool {
        let Some(&idx) = self.by_uid.get(uid) else {
            return false;
        };
        let e = &mut self.entries[idx];
        if !e.disabled {
            return false;
        }
        e.disabled = false;
        e.reason.clear();
        // 手工复活是「证明账号没死」的最强信号，连续 12153 计数必须清零
        //（与 Go `ReviveDisabled` 一致）—— 否则复活后账号带着旧计数，
        // 下一次偶发抖动就直接又被杀掉。
        e.session_dead_fails = 0;
        self.dirty = true;
        true
    }

    /// 按 UID 直取（若 healthy 且未占满在途），记录使用时间防撞号。
    pub fn pick_by_uid(&mut self, uid: &str) -> Option<Auth> {
        let idx = *self.by_uid.get(uid)?;
        let now = SystemTime::now();
        if !self.entries[idx].healthy(now) || self.in_flight_full(&self.entries[idx]) {
            return None;
        }
        self.mark_used(idx);
        Some(self.entries[idx].a.clone())
    }

    /// 挑号（无排除集）。
    pub fn pick(&mut self) -> Option<Auth> {
        self.pick_excluding(&[])
    }

    /// 按模型挑号（跳过 `tried`），并**过滤掉该模型正处于冷却中的账号**。
    ///
    /// 模型级冷却必须在这里过滤：否则选号会挑中一个该模型已限流的账号，
    /// 白跑一轮再换号，表现为「一直撞同一个 6004」。
    ///
    /// 另外还做「该产品是否提供该模型」的两轮排除 —— 见 [`Self::pick_for_model_any`]。
    pub fn pick_for_model(&mut self, model: &str, tried: &[String]) -> Option<Auth> {
        self.pick_for_model_any(model, tried)
    }

    /// 裸名请求（无产品前缀）的选号：在 [`Self::pick_for_model`] 的基础上加
    /// 「该产品是否提供该模型」的两轮排除。
    ///
    /// # 为什么需要它（实测缺陷）
    ///
    /// 用户用 `deepseek-v4.1-flash`（WorkBuddy 的模型）发请求，却被路由到
    /// **Qoder 账号**上并失败两次 —— 因为原来只按「账号当前可用」挑，
    /// **不问「这个产品有没有这个模型」**。Qoder 账号当时恰好可用就被选中。
    ///
    /// # 两轮排除
    ///
    /// - **第 1 轮**：**明确声明不提供**该模型的产品 → 排除。即「有清单、且
    ///   清单里没有它」。这是确定的否定，永远可以排除。
    /// - **第 2 轮**：**没声明提供**的产品（清单未知，或清单里没有）→ 先记下，
    ///   本轮**不排除**。若第 1 轮之后还有「明确声明提供」的候选，就只在它们
    ///   里挑；否则再放开用这些。
    ///
    /// # 为什么不能「未知 ⇒ 照常参与」
    ///
    /// 旧写法是 `if !product_may_serve(...) { 排除 }`，而 `product_may_serve`
    /// 对「清单未知」返回 `true`（不排除）。于是当 `product_models` **缺
    /// workbuddy** 时（宿主那侧来自用户白名单，默认为空 ⇒ 不写），WorkBuddy
    /// 账号**不被排除**，而它没有 `Qwen3.8-Flash` ⇒ 请求在撞上第一个 WorkBuddy
    /// 账号时就回 11102，压根轮不到真正有这个模型的 qoder 账号。
    ///
    /// 所有者的现场正是如此：
    ///
    /// ```text
    /// Qwen3.8-Flash        → 400 model_not_in_region
    /// qoder:Qwen3.8-Flash  → 成功
    /// ```
    ///
    /// 关键区别：**「不知道」≠「能满足」**。未知只该让它在**没有更好选择时**
    /// 兜底，而不该让它与「明确声明提供」的产品**平等竞争** —— 后者才是这个
    /// 模型真正该去的地方。
    ///
    /// ⚠ 只在多产品开启 + 宿主透传了清单时生效；两者任一不满足即保持既有行为
    /// —— 否则会因「不知道清单」而把所有账号排掉，那比原缺陷更糟。
    pub fn pick_for_model_any(&mut self, model: &str, tried: &[String]) -> Option<Auth> {
        // ⚠ 必须**复制** tried 而不是就地改：它是调用方传进来的切片，就地写会把
        // 「已排除集」污染到调用方的重试循环里 —— 表现为「第一次挑不到号之后，
        // 后续永远挑不到」。
        let mut declared: Vec<String> = Vec::new();
        let mut unknown: Vec<String> = Vec::new();
        let mut scoped: Vec<String> = tried.to_vec();
        // ★ 第三档（2026-10-06）：**只**因为「网关静态兜底清单没跟上」而被排除的号。
        //
        // 与 `scoped` 必须分开：`scoped` 里还混着 `tried`（本次已试过），一旦
        // 把它们一起放行，就会在一次请求内重复打同一个号。本档只承载「清单猜测
        // 落空」这一种原因 —— 它们其实是健康的、只是我们**没听说过**这个模型名。
        let mut guess_excluded: Vec<String> = Vec::new();

        for e in self.entries.iter() {
            let uid = e.a.uid.clone();
            let product = e.a.product_of();
            // ---- 落点 B：平台总开关 ----
            //
            // ⚠ 这条**必须**有：`pick_inner` 只在显式指定平台时才走产品强约束，
            // 而裸模型名（`pick_for_model`）会穿过产品维度直接到这里 —— 只改
            // `platform_allows_model`（准入闸）挡不住它，表现为「关了平台，
            // 裸名请求还是落到了那个平台的账号上」。
            //
            // 当作「确定排除」推进 `scoped`，与下面「有清单且明确没有」同档：平台被
            // 关掉是用户的明确表态，不该再被当成「未知」放进兜底候选。
            if !self.platform_enabled(&product) {
                scoped.push(uid);
                continue;
            }
            if self.product_declares_model(&product, model) {
                declared.push(uid);
            } else if self.product_definitely_lacks_model(&product, model) {
                // 宿主清单**已知**且明确没有 → 确定排除（Trae 事故那一档，必须保留）
                scoped.push(uid);
            } else if self.fallback_authoritative(&product) {
                // ★★ 2026-10-07 业主现场：**权威**兜底清单未命中同样是「确定没有」。
                //
                // 与上一支的差别只在**依据来源**：上一支靠宿主下发的清单，这一支
                // 靠网关自补的权威清单（目前只有 Trae）。两支都进 `scoped`。
                //
                // 为什么不能让它落到 `guess_excluded`：那里装的是「我们**没听说过**
                // 这个模型名」的账号 —— 它们可能真的有（`space-bunny` 之于
                // WorkBuddy）。Trae 的清单是别名表，`space-bunny` 不可能被上游解析
                // ⇒ 让它进最后一档抽签，就是拿 27 个可能成功的号去陪 1 个注定
                // 失败的号（实测抽中率约 3%，表现为业主那条
                // `400 model_not_in_upstream`）。
                scoped.push(uid);
            } else if !self.product_may_serve(&product, model) {
                // ★ 仅「静态兜底清单未命中」→ **只降级，不灭绝**（2026-10-06 修复）
                guess_excluded.push(uid);
            } else {
                // 清单未知（或该产品不参与多产品约束）→ 兜底候选
                unknown.push(uid);
            }
        }

        // 优先只在「明确声明提供该模型」的产品里挑。
        if !declared.is_empty() {
            let mut pref = scoped.clone();
            // 把兜底候选也排除掉，让选号只看声明过的产品。
            pref.extend(unknown.iter().cloned());
            if let Some(a) = self.pick_inner(&pref, model, RegionFilter::Any, "", true) {
                return Some(a);
            }
            // 声明过的产品里暂时挑不到时，**不能跨到未知产品**。
            // 未知只表示网关没有可信能力清单，绝不等于它能服务这个模型；
            // 对 Qwen3.8-Flash 这种只有 Qoder 声明的模型，放行 WorkBuddy
            // 会把请求打到根本没有该模型的上游，并把失败计入错误平台。
            // 因此这里宁可返回 None，让调用方明确报告没有该产品的可用账号。
            return None;
        }

        // declared 为空 ⇒ 没有任何产品明确声明提供它，此时才放开到 unknown。
        //
        // ⚠ 这一支必须**把 guess_excluded 挡在外面**。它们与 unknown 是两种性质：
        // unknown 里有「网关的猜测**说它可能有**」（兜底清单命中）或「完全没信息」，
        // 而 guess_excluded 是「网关的猜测**说它没有**」。混在一起抽签就等于把
        // 猜测当成了中性 —— 2026-10-03 的事故正是这个形状：Trae 的兜底清单里
        // 确实没有 `deepseek-v4.1-flash`，而 WorkBuddy 的兜底清单里有它；
        // 若两者平等竞争，19 个 WorkBuddy 账号也可能输给 1 个 Trae 账号，
        // 用户看到「上游 Trae 没有模型」（回归测试
        // `trae_fallback_excludes_a_model_trae_does_not_offer` 钉住这条）。
        let mut pref = scoped.clone();
        pref.extend(guess_excluded.iter().cloned());
        if let Some(a) = self.pick_inner(&pref, model, RegionFilter::Any, "", true) {
            return Some(a);
        }

        // ★★ 最后一档（2026-10-06 所有者现场）：连「有依据」的号都挑不出来时，
        // 才允许拿「只是我们没听说过这个模型名」的号试一次。
        //
        // # 为什么必须有这一档
        //
        // 业主用 `space-bunny`（WorkBuddy 国服**真实**模型，网关自己的
        // `/debug/capability` 里就列着它）发请求，得到的是
        // `503 no_healthy_account / all accounts unavailable (cooling/disabled)`
        // —— 而 27 个账号一个都没坏。链条：静态兜底清单（2026-09-18 快照）里
        // 没有这个模型名 ⇒ 判定「未命中」⇒ 全池进 `scoped` ⇒ `pick_inner` 候选
        // 为空 ⇒ 全冷却兜底 `pick_earliest_expiry` **只收冷却中的号** ⇒ `None`。
        //
        // # 为什么安全
        //
        // ① 它排在**最后**：声明方（`declared`）与非猜测候选（`unknown`）都优先，
        //    正常路径根本走不到这里 ⇒ 2026-09-21 的 qoder 事故（声明方被 WorkBuddy
        //    的猜测清单淹没 ⇒ 11102）不会被重新引入。
        // ② 宿主清单**已知**且明确没有的产品**不在这里**（它们进了 `scoped`）
        //    ⇒ Trae 保护原样保留：`deepseek-v4.1-flash` 不会被路由到 Trae。
        // ③ `tried` 仍被排除（见本函数开头），一次请求内不会重复打同一个号。
        // ④ 兜底**仍然**尊重健康、在途、模型冷却、平台开关、no_route（`pick_inner`
        //    与 `pick_earliest_expiry` 各自的排除集不变）。
        //
        // 取舍：宁可拿一个「我们以为它没有、其实可能有」的号试一次 —— 成了就是
        // 成功，不成也只会得到**真实的上游错误**，比「所有账号不可用」这种把人
        // 引向账号池的误导文案有用得多。
        if guess_excluded.is_empty() {
            return None;
        }
        self.pick_inner(&scoped, model, RegionFilter::Any, "", true)
    }

    /// 按模型 + 区域偏好挑号。
    ///
    /// `required=true` 时**只**在指定区域挑，挑不到返回 `None`（调用方据此报错）；
    /// `required=false` 时先在该区域挑，挑不到放开到全池。
    ///
    /// 两者语义差别是「带图片请求」的核心：偏好会在该区域没号时静默回退到
    /// 读不到图的后端，用户拿到一句「抱歉，我无法查看图片」却毫不知情；
    /// 强制则明确告诉用户「需要哪个区域的账号」，可排查、可行动。
    ///
    /// # 与 Go `PickForModelRegion` / `PickForModelRegionStrict` 的对应
    ///
    /// ```text
    /// Go PickForModelRegion(m, tried, Any)        → pickForModelAny
    /// Go PickForModelRegion(m, tried, r)          → pickForModelInRegion(r)，nil 再 pickForModelAny
    /// Go PickForModelRegionStrict(m, tried, r)    → pickForModelInRegion(r)（不放开）
    /// ```
    ///
    /// ★ 「放开到全池」这一支**必须走 [`Self::pick_for_model_any`]** 而不是直接
    /// `pick_inner(..., RegionFilter::Prefer)`：后者只放开区域，**不做**
    /// 「该产品是否提供该模型」的两轮排除。带 `cn:` / `global:` 前缀的请求也会
    /// 走到这里（Go 侧同一路径），若在这里丢掉产品排除，`cn:Qwen3.8-Flash`
    /// 就会重新出现「撞上第一个 WorkBuddy 账号回 11102」的老缺陷。
    pub fn pick_for_model_region(
        &mut self,
        model: &str,
        tried: &[String],
        region: Region,
        required: bool,
    ) -> Option<Auth> {
        if region == Region::Any {
            // Go 侧 `prefer == RegionAny` 直接走 pickForModelAny。
            return self.pick_for_model_any(model, tried);
        }
        if required {
            // 严格区域**不做**全冷却兜底（Go `pickStrict` 语义）：兜底会选「冷却中但
            // 截止最早」的账号 ⇒「偏好国服」退化成「把正在冷却的国服号塞回来」，
            // 那是绕过冷却。返回 None 让调用方决定是否放开区域 —— 那是**跨区域**
            // 降级，与「同区域内绕过冷却」性质完全不同。
            return self.pick_inner(tried, model, RegionFilter::Strict(region), "", false);
        }
        // 偏好：先严格在该区域挑（不兜底），挑不到再放开到全池
        //（放开的那一支走 pick_for_model_any，保留产品两轮排除）。
        if let Some(a) = self.pick_inner(tried, model, RegionFilter::Strict(region), "", false) {
            return Some(a);
        }
        self.pick_for_model_any(model, tried)
    }

    /// 按模型 + 区域 + **产品**挑号（带产品前缀的请求走这条）。
    ///
    /// # ★ 产品约束是强制的、不走兜底
    ///
    /// 与 Go 侧 `PickForModelProductRegion` 对应。文档记录的三点：
    ///
    /// 1. 三平台**有重名模型**（`glm-5.3` 既在 ZCode 套餐也在 WorkBuddy 清单），
    ///    裸名请求可能选中**没有该模型资源包**的账号 → `1113 无可用资源包`，
    ///    用户看到「余额不足」（其实额度充足，只是走错平台）。
    /// 2. 产品约束**不走兜底**（区域可以「偏好不成再放开」，产品不行 ——
    ///    用户写 `qoder:` 就是明确「只走这个平台」；若兜底跨平台，他会看到
    ///    「我明明指定了 qoder，却报了 ZCode 的 1113」）。
    /// 3. `product == ""` 时退化为区域逻辑（老客户端不带前缀，行为逐字不变）。
    ///
    /// # 选不出号时**只在同产品内**兜底（2026-09-22 修正）
    ///
    /// 客户端固定用 `qoder:Qwen3.8-Flash` 突然全 503（`no_healthy_account`）：
    /// qoder 只有 1 个账号，连吃 3 次上游 503 → 熔断 30 分钟；产品前缀请求只能
    /// 选该产品账号 + 严格路径刻意不兜底 ⇒ 整个产品彻底不可用 30 分钟。
    ///
    /// 取舍：宁可拿「可能不好」的号试一次（上游已恢复 → 成功；仍故障 → 用户看到
    /// **真实上游错误**而非误导性的「所有账号不可用」）。`tried` 仍在排除集内，
    /// 一次请求内不会重复打同一个号。
    ///
    /// ⚠ 只在「该产品**确实**一个健康号都没有」时兜底（有健康号则严格路径已返回）。
    /// 产品约束在 `product` 参数里已生效，兜底不会跨产品。
    pub fn pick_for_model_product_region(
        &mut self,
        model: &str,
        tried: &[String],
        region: Region,
        product: &str,
    ) -> Option<Auth> {
        if product.is_empty() {
            return self.pick_for_model_region(model, tried, region, false);
        }
        let filter = if region == Region::Any {
            RegionFilter::Any
        } else {
            RegionFilter::Strict(region)
        };
        self.pick_inner(tried, model, filter, product, true)
    }

    /// 报告「该范围内是否所有本可用账号都被标了 no_route」。
    ///
    /// 返回 `(no_route 数, 总数)`。调用方判据必须是 `no_route == total && total > 0`，
    /// **不是** `no_route > 0` —— 第一版写成 `> 0` 实测打脸：Qoder 有 2 个账号
    ///（`qoder.com.cn` no_route + `qoder.sh` 本来可用），请求选中 qoder.sh →
    /// 它自己失败 → 轮转耗尽 → 文案却说「1 个账号都被设为 no_route」，
    /// 而真实原因是 qoder.sh 失败。
    ///
    /// # 分母只算「本来能用」的账号
    ///
    /// 冷却中 / 已禁用的**不进分母**，否则「1 个 no_route + 4 个冷却」会被说成
    /// 「5 个都被设成不接流量」—— 用户会去检查一个根本没问题的开关。
    ///
    /// `model` 当前不参与判定（no_route 是**账号级**设置，与模型无关），
    /// 保留参数只为将来不改签名。
    pub fn no_route_excludes_all(
        &self,
        product: &str,
        _model: &str,
        prefer: Region,
    ) -> (usize, usize) {
        let now = SystemTime::now();
        let mut no_route = 0usize;
        let mut total = 0usize;
        for e in &self.entries {
            if !product.is_empty() && e.a.product_of() != product {
                continue;
            }
            if prefer != Region::Any && e.a.region() != prefer {
                continue;
            }
            // 先判「本来能不能用」—— 冷却中/已禁用的不进分母
            if e.disabled {
                continue;
            }
            if e.until.map(|u| now < u).unwrap_or(false) {
                continue;
            }
            if e.breaker_until.map(|b| now < b).unwrap_or(false) {
                continue;
            }
            total += 1;
            if e.a.no_route {
                no_route += 1;
            }
        }
        (no_route, total)
    }

    /// 按 UID + 模型 + 区域直取（粘性命中路径用）。
    pub fn pick_by_uid_for_model_region(
        &mut self,
        uid: &str,
        model: &str,
        region: Region,
    ) -> Option<Auth> {
        // ★★ 2026-10-08：粘性快路径也必须尊重成本维度。
        //
        // 现场：会话一旦绑到计费号，30 分钟 TTL 内所有请求都走它 —— `free_tier`
        // 在主路径把计费号筛掉了，粘性却把它请了回来（15 个国服号被均匀摊了
        // 710 条）。返回 `None` 会让调用方解绑并走完整选号，那里成本维度生效。
        if self.sticky_costs_more_than_a_free_option(uid, model) {
            return None;
        }
        let idx = *self.by_uid.get(uid)?;
        let now = SystemTime::now();
        let e = &self.entries[idx];
        if !e.healthy(now) || self.in_flight_full(e) || !e.model_available(model, now) {
            return None;
        }
        // 区域偏好：不匹配则视为不可用（由调用方决定是否改绑）。
        if region != Region::Any && region_of(&e.a) != region {
            return None;
        }
        self.mark_used(idx);
        Some(self.entries[idx].a.clone())
    }

    /// 该账号对当前模型是「确定计费」而池里**存在**确定免费的候选吗？
    ///
    /// # 为什么需要它（2026-10-08 业主现场）
    ///
    /// 现场：12 个国际版号对 `deepseek-v4.1-flash` 是 `x0.00`（免费）、15 个国服号
    /// 是 `x0.11`（计费）。正常选号路径的 `free_tier` 把国服号筛掉了，**但粘性
    /// 快路径与兜底都漏了这一步** —— 会话一旦绑到一个计费号，之后 30 分钟 TTL 内
    /// 全部请求都走它，成本维度形同不存在。
    ///
    /// 实测形状：15 个国服号被**均匀摊了 710 条**（45–50 条/号，极均匀 ⇒ 会话
    /// 各自绑到不同号），每条平均 27 万 input token，而同期国际版 10,844 条。
    ///
    /// # 判据为什么是「有免费候选」而不是「本号不是免费」
    ///
    /// 只判后者会让**全池都在计费**时永远解绑 —— 那时粘性白丢、每轮重新选号，
    /// 却挑不出更好的号。与 `free_tier` 的语义保持一致：只有「确实存在更优解」
    /// 才值得放弃粘性。
    ///
    /// # 为什么不看 `tried` / 在途 / 冷却
    ///
    /// 那些是**运行态**，粘性本来就是「少一次选号」的优化，判据越复杂越容易
    /// 与主路径漂移。这里只问「成本维度上有没有更好的选择」—— 解绑后主路径会
    /// 用完整的 `pick_inner` 重新选，那些条件自然都会被考虑。
    fn sticky_costs_more_than_a_free_option(&self, uid: &str, model: &str) -> bool {
        if !self.multi_product_on || self.cost_weight <= 0.0 {
            return false; // 成本维度未启用 ⇒ 逐字退回旧行为
        }
        let Some(&idx) = self.by_uid.get(uid) else {
            return false;
        };
        let e = &self.entries[idx];
        // 本号未声明倍率 ⇒ 中性，不算「更贵」（与 free_tier 的「未声明不算免费」对称）。
        if !e.cost_known || e.cost_rate <= 0.0 {
            return false;
        }
        let now = SystemTime::now();
        // 池里是否存在**确定免费**且此刻可服务的候选（不含本号）。
        self.entries.iter().any(|o| {
            o.a.uid != uid
                && o.cost_known
                && o.cost_rate <= 0.0
                && o.healthy(now)
                && o.model_available(model, now)
                && !self.in_flight_full(o)
        })
    }
    /// 挑号，跳过 `tried` 中的 UID。
    pub fn pick_excluding(&mut self, tried: &[String]) -> Option<Auth> {
        self.pick_inner(tried, "", RegionFilter::Any, "", true)
    }

    /// 按 uid + 模型 + 区域 + **产品**直取（粘性命中路径用）。
    ///
    /// # ★ 为什么粘性路径也必须校验产品
    ///
    /// 与 Go 侧 `PickByUIDForModelProductRegion` 对应。文档记录的是**实测发现的
    /// 漏洞**：派发层的选号顺序是「先试粘性会话绑定的账号，绑不上才走产品过滤」，
    /// 而产品过滤只在后一支。于是**只要该会话此前绑过一个账号**，
    /// `zcode:glm-5.3` 会继续用那个绑定的账号 —— 哪怕它是 WorkBuddy 的。
    ///
    /// 实测确认（`uitest/verify-model-prefix-live.cjs`）：`zcode:glm-5.3`
    /// → 选中 acct-c（**workbuddy**）→ 报「额度已耗尽」。
    /// 用户看到的是「我明明指定了 zcode，却报了 WorkBuddy 账号的错」。
    pub fn pick_by_uid_for_model_product_region(
        &mut self,
        uid: &str,
        model: &str,
        region: Region,
        product: &str,
    ) -> Option<Auth> {
        if !product.is_empty() {
            let &idx = self.by_uid.get(uid)?;
            // ★ 必须用 ProductOf()：老账号 product 为空串等价 workbuddy，
            // 裸读会让它们全被排除（症状「指定 workbuddy 却一个号都选不出」）。
            if self.entries[idx].a.product_of() != product {
                return None;
            }
            // ★ 落点 E：粘性直取也必须过平台总开关。
            //
            // 与产品校验同一个道理：派发层「先试粘性绑定的账号，绑不上才走产品
            // 过滤」，而产品过滤只在后一支。若这里不判，一个此前绑过 qoder 的
            // 会话在「关闭 qoder 参加路由」之后仍会继续用那个 qoder 账号 ——
            // 与实测确认过的跨产品漏洞是同一类。product 非空 ⇒ 用户明确指定了
            // 平台，此时开关必须生效（空产品表示「不指定」，交给 pick_inner）。
            if !self.platform_enabled(product) {
                return None;
            }
        } else {
            // ★ 落点 F（2026-10-06）：**空产品**（裸模型名）时的粘性路径同样要过
            // 「这个号所属产品确定没有这个模型」这一闸。
            //
            // 现场（业主日志 2026-10-06 17:3x）：`gpt-6-astra` 与 `space-bunny`
            // **先后落到同一个 Trae 账号** `trae-2593120869167228` 上，然后双双报
            // 「上游 Trae 国内版没有模型 …（清单 0 个）」。成因就是这里：产品为空
            // 时旧代码直接退化成 `pick_by_uid_for_model_region`，只查健康/在途/
            // 模型冷却，**完全不看产品能不能服务该模型** —— 于是一个此前被绑过
            // 的 Trae 号，在裸名请求里会被反复复用。
            //
            // ⚠ 判据用 `product_definitely_lacks_model` 而**不是**
            // `product_may_serve`：后者会把「静态兜底清单没跟上」也算成「不能服务」
            // ⇒ 绑过号的会话在 `space-bunny` 这类新模型上永远解绑，白丢粘性。
            let &idx = self.by_uid.get(uid)?;
            let owner = self.entries[idx].a.product_of().to_string();
            if self.product_definitely_lacks_model(&owner, model) {
                return None;
            }
            // ★ 落点 J（2026-10-06）：区域维度同样要过 —— 会话绑定的号可能在
            // 该模型并不存在的那个区。
            //
            // 为什么用「已观测到只在另一区」这种软判据而不是直接 `return None`：
            // 粘性绑定是**成本优化**（少一次选号），不是正确性来源。绑错区只会让
            // 这一个请求拿 400，然后下面 `acct.is_none()` 分支会解绑并重新选号；
            // 但若判据本身有噪声（真值只拉到一区），解绑会白丢粘性。故只在
            // `model_region_hint` 给出**确定结论**且与账号区域不符时才拒绝。
            //
            // ⚠ 只在用户**没有**指定区域时生效（`region == Region::Any`）：用户写
            // 了 `国际版:` 就是明确指令，那时该由下面 pick_by_uid_for_model_region
            // 的区域判据负责，不该被这里的软判据抢先。
            if region == Region::Any {
                if let Some(hint) = self.model_region_hint(&owner, model) {
                    if region_of(&self.entries[idx].a) != hint {
                        return None;
                    }
                }
            }
        }
        self.pick_by_uid_for_model_region(uid, model, region)
    }

    /// 挑号主路径：候选过滤（tried / 健康 / 在途 / 模型冷却 / 区域）→ 到期分层 → 加权抽签。
    ///
    /// `allow_fallback` 决定「候选为空时要不要走全冷却兜底」。单独抽成参数而非
    /// 给 `pick` 加分支，是为了让「要不要兜底」在**调用点一眼可见**
    ///（Go 侧 `pickStrict` 的注释原话）。
    fn pick_inner(
        &mut self,
        tried: &[String],
        model: &str,
        region: RegionFilter,
        product: &str,
        allow_fallback: bool,
    ) -> Option<Auth> {
        let now = SystemTime::now();

        // 1) 收集候选（按 UID 排序保证确定性）
        //
        // ★ 产品约束与平台白名单在这里是**强制排除**（塞进 `scoped`），不走兜底：
        // 用户写 `qoder:` 就是明确「只走这个平台」；若兜底跨平台，他会看到
        // 「我明明指定了 qoder，却报了 ZCode 的 1113」。
        let scoped: HashSet<&str> = tried.iter().map(|s| s.as_str()).collect();
        // 仅因平台白名单被排除的账号（兜底 1 用）
        let mut excluded_by_platform: Vec<usize> = Vec::new();
        let mut cands: Vec<usize> = Vec::new();
        let mut pruned = false;
        for i in 0..self.entries.len() {
            if scoped.contains(self.entries[i].a.uid.as_str()) {
                continue;
            }
            // ★ 顺手清理过期的模型冷却（Go `pickLocked` 同款）。
            //
            // `model_until` 只增不减：长期运行后高频账号会积累上百条早已过期的
            // 条目，而每次选号都要在 `model_available` 里查这张越来越大的表 ——
            // 内存与查找成本随运行时间单调增长。此处遍历本来就要做，顺带清理
            // 是零额外成本；只在真删掉东西时才标脏，避免每轮选号都触发写盘。
            if self.entries[i].prune_model_cools(now) {
                pruned = true;
            }
            let e = &self.entries[i];
            if !e.healthy(now) || self.in_flight_full(e) || !e.model_available(model, now) {
                continue;
            }
            if !product.is_empty() {
                // ★ 产品强约束：不匹配即排除（用 ProductOf()，空串归一 workbuddy）
                if e.a.product_of() != product {
                    continue;
                }
            }
            // ★ 落点 D：平台总开关（「这个平台还参加兼容网关路由吗」）。
            //
            // 与上面的产品强约束同档 —— **强制排除，绝不塞进 excluded_by_platform**：
            // 那个池子是给「模型白名单」兜底用的（宁可回退一个平台白名单不符但能用的
            // 账号），若把关掉的平台也放进去，用户点了「关闭 qoder 参加路由」之后
            // 依然会被兜底选中 qoder 的号 —— 开关等于没关。
            //
            // ⚠ 实测踩过的坑：这个判据最初只加在 pick_for_model_any（裸名路径）里，
            // 于是「带产品前缀」的请求（qoder:xxx）走 pick_for_model_product_region
            // → 本函数，直接绕过开关：实测关掉 qoder 后 qoder:国服:GLM-5.3 仍返回
            // 200（真上游内容）。选号的所有路径都必须在**这里**过一遍。
            //
            // 语义提醒（业主原话）：「关闭平台只是不参与兼容网关，并不是不执行任务」。
            // 这里只影响选号 —— 不写 Entry.disabled、不碰 probe_uids/available_uids，
            // 签到/上报/成长任务/额度巡检照常跑。
            if !self.platform_enabled(&e.a.product_of()) {
                continue;
            }
            // ★★ 落点 G（2026-10-06）：裸名请求（`product` 为空）也必须过
            // 「该号所属产品**确定**不提供该模型」这一闸。
            //
            // # 为什么必须有它
            //
            // 选号的**所有**入口最终都汇到这里，而此前只有裸名路径
            // （`pick_for_model_any`）做了产品两轮排除。于是同一类缺陷从三条缝里
            // 漏出来，业主现场三条全中：
            //
            // 1. 区域偏好路径 —— `pick_for_model_region` 第一段直接调本函数且
            //    产品参数为空（带图片的请求正是走这条路）⇒ `space-bunny` 落到
            //    Trae 号上，报「上游 Trae 国内版没有模型 space-bunny（清单 0 个）」。
            // 2. 严格区域路径（`required = true`）—— 同一句调用。
            // 3. 粘性直取 —— 见 `pick_by_uid_for_model_product_region` 的落点 F。
            //
            // # 判据为什么是「确定」而不是「不排除」
            //
            // 用 `product_may_serve` 会把「静态兜底清单没跟上」也算成不能服务，
            // 于是上游新上的模型（`space-bunny`）在任何路径上都选不出号 ——
            // 那正是本次要修的 503。`product_definitely_lacks_model` 只在**宿主清单
            // 已知且不含它**时为真，Trae 保护（Trae 之于 `deepseek-v4.1-flash`）
            // 原样保留。
            //
            // ⚠ 只在 `product` 为空时生效：显式产品前缀（`trae:xxx`）是用户的
            // **明确指令**，设计上「根本不查清单」（见 `products/trae/dispatch.rs` 的
            // 「显式请求…兜底是安全的」）。清单不全不该让显式请求失效。
            if product.is_empty()
                && !model.is_empty()
                && self.product_definitely_lacks_model(&e.a.product_of(), model)
            {
                continue;
            }
            // 平台白名单（「模型 → 允许的平台」）：配了且不含该平台 → 排除。
            // 记进 excluded_by_platform 供兜底 —— 「宁可回退一个平台不符但能用的
            // 账号，也不要让本来能成功的请求 503」。
            if !self.platform_allowed(model, e.a.product_of()) {
                excluded_by_platform.push(i);
                continue;
            }
            cands.push(i);
        }
        if pruned {
            // 真删掉了过期条目才标脏：否则每轮选号都会触发一次写盘。
            self.dirty = true;
        }
        cands.sort_by(|&a, &b| self.entries[a].a.uid.cmp(&self.entries[b].a.uid));

        // 区域过滤：只保留目标区域的候选（`Any` 原样）。
        //
        // 没有「偏好」这一档 —— Go 的 `PickForModelRegion` 是「先严格在区域内挑
        // （不兜底），挑不到再放开全池」，那是两次独立调用（见
        // `pick_for_model_region`）。把「放开」做成本函数里的宽松过滤会绕过
        // 产品两轮排除。
        cands = match region {
            RegionFilter::Any => cands,
            RegionFilter::Strict(r) => cands
                .into_iter()
                .filter(|&i| region_of(&self.entries[i].a) == r)
                .collect(),
        };

        // ★★ 落点 I（2026-10-06 真机实测）：裸名（区域不限）时，用**上游真值**
        // 排除「我们确知它不在这个区」的账号。
        //
        // # 现场
        //
        // `space-bunny` 是 WorkBuddy **国服**真实模型（隔离实例 :17864 的
        // `/debug/capability` 里 `dynamic_cn_ids` 有它、`dynamic_intl_ids` 没有），
        // 而裸名请求走 `RegionFilter::Any` ⇒ 12 个国际版账号也在候选里
        // ⇒ 抽到国际版号 ⇒ 上游回 400 `model [space-bunny] service info not found`。
        // 业主最初看到的 503 已被上一轮修复消除，但**症状只是从「全池不可用」
        // 变成了「一半概率打到错区域」** —— 这一档才是真正的收口。
        //
        // # 为什么只排除「有结论且不符」的，而不是「只留结论区」
        //
        // 结论只对**该产品**成立（`model_region_hint` 按产品查）。别的产品
        //（qoder 声明的同名模型）没有结论时不该被牵连 —— 那会把它们一起误杀。
        //
        // # 为什么收紧后为空就退回原集合
        //
        // 收紧的目标区可能整批在冷却（或全被 tried 排除）。此时若直接返回空，
        // 调用方会报「所有账号不可用」—— 又是把人引向账号池的误导文案。宁可
        // 拿一个错区的号试一次，让他看到**真实的上游错误**（与全冷却兜底同一取舍）。
        if region == RegionFilter::Any && self.multi_product_on && !model.is_empty() {
            let narrowed: Vec<usize> = cands
                .iter()
                .copied()
                .filter(|&i| {
                    let e = &self.entries[i];
                    match self.model_region_hint(e.a.product_of(), model) {
                        Some(r) => region_of(&e.a) == r,
                        None => true, // 没有结论 ⇒ 不牵连
                    }
                })
                .collect();
            if !narrowed.is_empty() {
                cands = narrowed;
            }
        }

        if cands.is_empty() {
            if !allow_fallback {
                // 严格模式（区域强制 / 产品前缀）不做全冷却兜底：
                // 兜底会选「冷却中但截止最早」的账号 ⇒「偏好国服」会退化成
                // 「把一个正在冷却的国服号塞回来」= 绕过冷却。
                return None;
            }
            // 兜底 1：仅因平台白名单被排除的账号里按正常口径选一个。
            //
            // 为什么不是「随便给一个」：兜底也是真实请求，选号质量不该因为走了
            // 兜底就退化成随机；若另写一套口径，两条路径的行为会随时间漂移。
            if !excluded_by_platform.is_empty() {
                if let Some(idx) = self.pick_best_from(&excluded_by_platform, &scoped, now, model) {
                    self.mark_used(idx);
                    return Some(self.entries[idx].a.clone());
                }
            }
            // 兜底 2：全冷却池里选截止最早的（产品/区域约束已在 `scoped` 内生效）
            return self.pick_earliest_expiry(tried, now, model, product);
        }

        // 2) 免费优先（多产品 + 成本权重开启时才介入）
        let (cands, _) = self.free_tier(cands);

        // 3) 到期分层：只保留最早到期的一档
        let (cands, tiered) = self.earliest_expiry_tier(cands);

        // 4) top5 短名单：按权重降序截断（平局按 UID 升序，保证确定性）
        let max_credits = cands
            .iter()
            .map(|&i| self.entries[i].credits)
            .max()
            .unwrap_or(0);
        let mut weighted: Vec<(usize, f64)> = cands
            .iter()
            .map(|&i| {
                let w = if tiered {
                    self.tier_weight_of(i, now)
                } else {
                    self.weight_of(i, max_credits, now)
                };
                (i, w)
            })
            .collect();
        weighted.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| self.entries[a.0].a.uid.cmp(&self.entries[b.0].a.uid))
        });
        // ★ 截断边界上的等权重平局必须随机化：否则短名单恒为最小的 N 个 UID，
        // 其余账号永远进不来（Go issue #5：14 个号只用 5 个）。
        //
        // 三步（与 Go `pickLocked` 一致，顺序不能变）：
        //   ① 按 (权重降序, UID 升序) 排 —— 必须有确定性平局键，否则排序结果
        //      取决于 map 遍历顺序（随机）；
        //   ② 仅在截断边界存在等权重平局时洗牌 —— 打散「最小的 N 个 UID」；
        //   ③ **稳定**按权重降序重排 —— 恢复「权重高者优先」，同时保留 ② 在
        //      等权重组内部打乱后的顺序。
        self.shuffle_ties(&mut weighted);
        weighted.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut cands: Vec<usize> = weighted.into_iter().map(|(i, _)| i).collect();
        if cands.len() > SHORTLIST_SIZE {
            cands.truncate(SHORTLIST_SIZE);
        }

        // 5) 并发防雪崩：跳过 last_used 距今 < min_pick_gap 的账号
        let eligible: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|&i| match self.entries[i].last_used {
                None => true,
                Some(lu) => now.duration_since(lu).unwrap_or(Duration::MAX) >= MIN_PICK_GAP,
            })
            .collect();

        let chosen = if eligible.is_empty() {
            // 候选全部刚被用过：LRU 兜底。
            //
            // 必须用「时刻 + 单调序号」双键比较：时钟粒度会让并发写入的
            // last_used 完全相同，单靠时刻比较会因平局恒 false 而永远停在
            // 第一个候选，退化成惊群（Go 侧已修复，此处沿用）。
            let mut best = cands[0];
            for &c in &cands[1..] {
                if self.older_last_used(c, best) {
                    best = c;
                }
            }
            best
        } else {
            self.pick_weighted_mode(&eligible, tiered)
        };

        self.mark_used(chosen);
        Some(self.entries[chosen].a.clone())
    }

    /// 在「仅因平台白名单被排除」的账号里按**正常口径**挑一个（兜底 1）。
    ///
    /// 与 Go 侧 `pickBestFrom` 逐条对应。不是「随便给一个」：仍按
    /// 「免费优先 → 到期分层 → 权重降序」。
    fn pick_best_from(
        &self,
        cands: &[usize],
        tried: &HashSet<&str>,
        now: SystemTime,
        model: &str,
    ) -> Option<usize> {
        let usable: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|&i| {
                let e = &self.entries[i];
                !tried.contains(e.a.uid.as_str())
                    && e.healthy(now)
                    && e.model_available(model, now)
                    && !self.in_flight_full(e)
            })
            .collect();
        if usable.is_empty() {
            return None;
        }
        let (usable, _) = self.free_tier(usable);
        let (usable, tiered) = self.earliest_expiry_tier(usable);
        let max_credits = usable
            .iter()
            .map(|&i| self.entries[i].credits)
            .max()
            .unwrap_or(0);
        let mut best: Option<usize> = None;
        let mut best_w = 0.0_f64;
        for &i in &usable {
            let w = if tiered {
                self.tier_weight_of(i, now)
            } else {
                self.weight_of(i, max_credits, now)
            };
            if best.is_none() || w > best_w {
                best = Some(i);
                best_w = w;
            }
        }
        best
    }

    /// 短名单平局洗牌（Fisher-Yates，仅当**截断边界存在等权重平局**时）。
    ///
    /// 与 Go 侧 `shuffleTiesWs` 逐条对应。为什么必须限定「边界有平局才洗」：
    ///
    /// - 权重各不相同（如 credits 差异明显）时不存在平局，洗牌只会在注入确定性
    ///   随机源的测试里白白消耗随机数、打乱既有可复现语义，属无谓副作用；
    /// - 只在真正需要打破平局时消费随机数，保持「权重高者优先」的确定性不变。
    ///
    /// 前置条件：`ws` 已按 (权重降序, UID 升序) 排好。
    fn shuffle_ties(&mut self, ws: &mut [(usize, f64)]) {
        if ws.len() <= SHORTLIST_SIZE {
            return; // 全部候选都在短名单内，截断不会排除任何账号 → 无需洗牌
        }
        // 检查「第 SHORTLIST_SIZE 名」与「第 SHORTLIST_SIZE+1 名」是否同权重：
        // 同权重才意味着谁入选由排序顺序（UID）决定，需要随机化。
        let cut = SHORTLIST_SIZE;
        if ws[cut - 1].1 != ws[cut].1 {
            return; // 边界无平局：入榜与落榜权重不同，结果已确定，不应洗牌
        }
        for i in (1..ws.len()).rev() {
            let j = self.rand_n(i as i64 + 1);
            let j = if j < 0 || j > i as i64 { i } else { j as usize };
            ws.swap(i, j);
        }
    }

    /// 免费优先：若有「确定免费」的候选，只用它们。
    ///
    /// 与 Go 侧 `freeTierLocked` 逐条对应。优先级关系（与到期分层不冲突）：
    ///
    /// - 有免费候选 → **只用免费的**（到期分层在它们内部继续生效）
    /// - 没有免费候选 → 完全退回原行为（到期分层优先，成本仅档内微调）
    ///
    /// 所以既有契约「两个**付费**账号之间必须仍先烧快过期的那个」逐字不变。
    ///
    /// # 为什么「未声明」不算免费
    ///
    /// `cost_known = false` 是「不知道收不收费」。当成免费会让**某个还没查到
    /// 倍率的平台**抢走全部流量 —— 而那些平台的倍率表恰恰是最后才补齐的。
    ///
    /// # 为什么全免费时也返回「未收窄」
    ///
    /// 否则后续 `len(cands) <= 1` 之类短路判断会因「我们动过 slice」产生
    /// 不必要的行为差异。
    fn free_tier(&self, cands: Vec<usize>) -> (Vec<usize>, bool) {
        if !self.multi_product_on || self.cost_weight <= 0.0 {
            return (cands, false); // 完全不介入，保证「不新增行为」
        }
        let free: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|&i| {
                let e = &self.entries[i];
                e.cost_known && e.cost_rate <= 0.0
            })
            .collect();
        if free.is_empty() || free.len() == cands.len() {
            return (cands, false);
        }
        (free, true)
    }

    /// 报告 a 是否比 b "更久未被选中"（时刻优先，平局用单调序号）。
    fn older_last_used(&self, a: usize, b: usize) -> bool {
        let ea = &self.entries[a];
        let eb = &self.entries[b];
        match (ea.last_used, eb.last_used) {
            (None, None) => ea.last_used_seq < eb.last_used_seq,
            (None, Some(_)) => true, // 从未使用 = 最久未用
            (Some(_), None) => false,
            (Some(x), Some(y)) => {
                if x != y {
                    x < y
                } else {
                    ea.last_used_seq < eb.last_used_seq
                }
            }
        }
    }

    /// 全冷却兜底：在非禁用的软冷却/熔断账号中选截止最早的一个。
    ///
    /// 分级：disabled 永不参与；硬冷却（余额耗尽）账号若仍处于有效冷却期也排除
    ///（调了必 402）；软冷却与熔断号允许参与。
    ///
    /// # ⚠ 本函数**没有**调 `healthy()`
    ///
    /// 它自己手写筛选，因为 `healthy()` 会把「正在冷却」也判掉，而兜底恰恰要选
    /// 冷却中的账号。于是 `healthy()` 里的 `NoRoute` 检查在这里**不会生效** ——
    /// 必须显式再写一遍（Go 侧实测确认过：一个被标 NoRoute 且处于冷却期的账号
    /// 会被本函数选中，有回归测试钉住）。
    ///
    /// 现场症状：所有者把国际版账号全部标为「不接流量」（因为不禁用就没法禁），
    /// 国服账号因上游限流冷却后，兜底把这些他明确禁用的账号又选了回来，
    /// 请求打到连不通的国际版 → 503。用户看到的是「禁用也没用」。
    ///
    /// # `model` 非空时同样排除「该模型正被限流」的账号
    ///
    /// 否则兜底会把刚被 6004 拒掉的账号立刻再选一次，既浪费轮换又让用户看到
    /// 重复报错。若排除后无候选则返回 `None`，由调用方报告「全部账号该模型均
    /// 受限」（比继续撞限流更有信息量）。
    ///
    /// # ★★ 成本维度：兜底也要「免费优先」（2026-10-08 业主现场）
    ///
    /// 现场：12 个国际版号对 `deepseek-v4.1-flash` 是 `x0.00`（免费）、15 个国服号
    /// 是 `x0.11`（计费）。正常路径的 `free_tier` 把国服号筛掉了（实测 502 次 chat
    /// 全走国际版），**但兜底漏了这一步** —— 于是国服号被稳定选中：
    ///
    /// ```text
    /// 国际版 12 个号长期占满在途（max_in_flight=3，只有它们会被正常选中）
    /// ⇒ 兜底里 `in_flight_full` 把它们全跳过
    /// ⇒ 只剩 15 个永远空闲的国服号
    /// ⇒ 按到期日最早挑，每次都挑中国服号
    /// ```
    ///
    /// 实测读数：15 个国服号被**均匀摊了 710 条**（47–50 条/号），
    /// 每条平均 27 万 input token —— 与真实对话同一量级（养号任务走 billing 域，
    /// 不记 token）。而同期国际版 10,844 条。
    ///
    /// 修法：在「挑到期最早」之前先跑一遍 [`Self::free_tier`] —— 与 `pick_best_from`
    /// （兜底 1）**已经做的事**保持一致。那条兜底有 free_tier，只有本函数漏了，
    /// 看起来更像当初漏写而不是有意设计。
    ///
    /// ⚠ 收窄后若为空则**退回未收窄**：`free_tier` 自己已保证「没有免费候选 ⇒
    /// 原样返回」，这里的 `is_empty` 只是防御（免费候选可能全被下面的 tried /
    /// 冷却 / 模型限流条件排掉）。宁可拿一个付费号试一次，也不要返回 None 让
    /// 调用方报「所有账号不可用」—— 那是把人引向账号池的误导文案。
    fn pick_earliest_expiry(
        &mut self,
        tried: &[String],
        now: SystemTime,
        model: &str,
        product: &str,
    ) -> Option<Auth> {
        // ① 先按「硬性排除」收集候选（与修复前逐条一致）。
        let mut cands: Vec<usize> = Vec::new();
        for i in 0..self.entries.len() {
            let e = &self.entries[i];
            if tried.iter().any(|t| t == &e.a.uid) {
                continue;
            }
            if e.disabled {
                continue; // 禁用的账号永不参与兜底
            }
            // ★ 用户手动禁用（不接流量）同样永不参与兜底（见本函数文档）
            if e.a.no_route {
                continue;
            }
            // ---- 落点 C：平台总开关（全冷却兜底也必须尊重它）----
            //
            // ⚠ 这是**第三个**落点，漏了它就会出现「平台关了，但全池都在冷却时
            // 兜底又把该平台的号捞出来」—— 兜底路径不经过 `pick_inner` 的候选
            // 循环，只看本函数这几条排除。
            if !self.platform_enabled(&e.a.product_of()) {
                continue;
            }
            // 产品约束：兜底也必须只在本产品内（见 `pick_for_model_product_region`）
            if !product.is_empty() && e.a.product_of() != product {
                continue;
            }
            // 余额耗尽号处于有效 hard 冷却期 → 不参与兜底
            if e.cool_kind == Some(CoolKind::Hard)
                && e.until.map(|u| now < u).unwrap_or(false)
            {
                continue;
            }
            if !e.model_available(model, now) {
                continue; // 该模型正被限流：换了也是同一个 6004
            }
            if self.in_flight_full(e) {
                continue;
            }
            if e.expiry(now).is_none() {
                continue; // 无到期信息 ⇒ 不参与「挑最早」
            }
            cands.push(i);
        }

        // ② ★★ 成本维度：有「确定免费」的候选就只用它们（见本函数文档）。
        //
        // ⚠ 收窄后为空必须**退回未收窄**，不能直接返回 None —— 免费候选可能
        // 全被上面的条件排掉了（tried / 冷却 / 模型限流），此时宁可拿一个付费号
        // 试一次，也不要让调用方报「所有账号不可用」。
        let (cands, narrowed) = self.free_tier(cands);

        // ③ 在收窄后的候选里挑到期最早的。
        let mut best: Option<usize> = None;
        for &i in &cands {
            let Some(exp) = self.entries[i].expiry(now) else {
                continue;
            };
            if best.is_none()
                || Some(exp) < best.and_then(|b| self.entries[b].expiry(now))
            {
                best = Some(i);
            }
        }
        let idx = best?;
        // 兜底选号必须留痕：这是「本不该被选中」的账号（冷却中 / 被白名单排除），
        // 一旦线上出现「怎么又打到那个号了」，唯一的排查入口就是这行日志。
        // 与 Go 侧 `log.Printf("pool: fallback_earliest_expiry uid=%s until=%s kind=%s")` 对应。
        //
        // ★ 2026-10-08 起额外打 `cost=`：兜底「为什么会挑到这个号」曾经无法回答
        //（业主现场：国服号被稳定选中，而日志里没有任何一行提到成本）。
        // `free` = 这一轮被免费优先收窄过；`paid` = 候选里没有确定免费的号。
        {
            let e = &self.entries[idx];
            let cost = if narrowed { "free" } else { "paid" };
            crate::logline!(
                "pool: fallback_earliest_expiry uid={} until={:?} kind={} cost={} rate={}",
                e.a.uid,
                e.expiry(now),
                e.fallback_kind(now),
                cost,
                if e.cost_known { format!("{}", e.cost_rate) } else { "?".into() }
            );
        }
        self.mark_used(idx);
        Some(self.entries[idx].a.clone())
    }

    /// 到期分层：返回最早到期那一档，以及是否真的分了档。
    ///
    /// 到期日未知的账号视为最晚（排最后）：只有全部候选都没有到期信息时，
    /// 它们才会成为唯一的一档。
    ///
    /// # ★ 非 WorkBuddy 产品**豁免**积分分层
    ///
    /// 与 Go 侧 `earliestExpiryTierLocked` 逐条对应：Qoder / ZCode 的额度不是
    /// 「积分 + 到期日」模型，它们的 `expire_at` 要么为空、要么含义不同。
    /// 把它们和 WorkBuddy 一起分档会让「其它产品永远排在 WorkBuddy 的到期档
    /// 之后」（它们没有到期日 ⇒ 被当成最晚），或在只有它们时才退化。
    /// 故原样保留、不参与比档。
    ///
    /// 全是其它产品时返回「未分层」：没有积分维度可分，退化为随机。
    fn earliest_expiry_tier(&self, cands: Vec<usize>) -> (Vec<usize>, bool) {
        if cands.len() <= 1 {
            return (cands, false);
        }
        let mut scored: Vec<usize> = Vec::with_capacity(cands.len());
        let mut exempt: Vec<usize> = Vec::new();
        for &i in &cands {
            if self.entries[i].a.product_of() != crate::auth::PRODUCT_WORKBUDDY {
                exempt.push(i);
                continue;
            }
            scored.push(i);
        }
        let keep = |out: Vec<usize>| -> Vec<usize> {
            if exempt.is_empty() {
                return out;
            }
            let mut merged = out;
            merged.extend(exempt.iter().copied());
            merged
        };
        if scored.is_empty() {
            // 全是其它产品：没有积分维度可分，退化为随机
            return (cands, false);
        }
        // 找出最小的非空 day key（字典序比较即日期先后，因格式固定为 YYYY-MM-DD）
        let mut best: Option<String> = None;
        for &i in &scored {
            let k = self.entries[i].expiry_day_key();
            if k.is_empty() {
                continue; // 到期未知视为最晚，不参与选档
            }
            if best.as_ref().map(|b| &k < b).unwrap_or(true) {
                best = Some(k);
            }
        }
        let Some(best) = best else {
            // WorkBuddy 侧全员未知 → 不分层
            return (cands, false);
        };
        let out: Vec<usize> = scored
            .into_iter()
            .filter(|&i| self.entries[i].expiry_day_key() == best)
            .collect();
        (keep(out), true)
    }

    /// 三因子权重（含 credits 项）；`tiered` 时改用组内权重（去掉 credits）。
    fn weight_of(&self, idx: usize, max_credits: i64, now: SystemTime) -> f64 {
        self.weight_of_mode(idx, max_credits, now, false)
    }

    /// 组内权重：只保留闲置补偿 + 成功率，不含 credits。
    ///
    /// 同一到期档意味着这些额度的「紧迫度相同」，按积分多少分配会让高积分账号
    /// 长期吃掉大部分流量，同档内就不再是平均使用。
    fn tier_weight_of(&self, idx: usize, now: SystemTime) -> f64 {
        self.weight_of_mode(idx, 0, now, true)
    }

    /// 权重计算统一入口。
    fn weight_of_mode(&self, idx: usize, max_credits: i64, now: SystemTime, tiered: bool) -> f64 {
        let e = &self.entries[idx];
        let mut w = 1.0;

        // 1. credits 比例 ×10（tiered 时跳过）
        if !tiered && max_credits > 0 {
            w += e.credits as f64 / max_credits as f64 * 10.0;
        }

        // 2. 闲置补偿
        match e.last_used {
            None => w += self.idle_weight_max,
            Some(lu) => {
                let hours = now
                    .duration_since(lu)
                    .unwrap_or(Duration::ZERO)
                    .as_secs_f64()
                    / 3600.0;
                let mut idle_w = hours * self.idle_weight_per_hour;
                if idle_w > self.idle_weight_max {
                    idle_w = self.idle_weight_max;
                }
                if idle_w < 0.0 {
                    idle_w = 0.0; // 时钟回拨钳 0
                }
                w += idle_w;
            }
        }

        // 3. 成功率 ×3
        let total_req = e.success_count + e.err_total;
        if total_req > 0 {
            w += e.success_count as f64 / total_req as f64 * 3.0;
        } else {
            w += 1.5; // 无请求记录 → 中性偏信任
        }

        // 4. 成本维度（仅多产品开启时）
        //
        // ★★ 必须放在**最后乘**，而不是加进上面的加法项里。
        //
        // 前三个因子决定「谁更该被用」（谁额度更该烧、谁闲置更久、谁更可靠），
        // 成本只在此基础上做**小幅倾斜**。放前面乘会让成本与 credits 项互相
        // 放大（高积分 × 低成本 = 平方级优势，把其余账号全挤掉）。
        //
        // ★★ 两条路径都要走到这里 —— 本函数被 `weight_of`（tiered=false，所有
        // 账号都无到期信息时）与 `tier_weight_of`（tiered=true）共用，所以在这
        // 一处加就同时覆盖了两条路径。Go 侧正是因为两条路径各写一份、只加了其
        // 中一处，才出现「没有到期信息的池子里成本完全不生效」（实测表现为份额
        // 精确 50/50，成本维度形同虚设）。Rust 侧把权重计算收敛成单一入口
        //（`weight_of_mode`）就是为了让这类漏加在结构上不可能发生。
        if self.multi_product_on && self.cost_weight > 0.0 && e.cost_rate > 0.0 {
            w *= self.cost_multiplier(e.cost_rate);
        }
        w
    }

    /// 加权随机抽签（定点 ×1e6 累加，与 Go 侧同口径）。
    fn pick_weighted_mode(&self, cands: &[usize], tiered: bool) -> usize {
        let now = SystemTime::now();
        let max_credits = cands
            .iter()
            .map(|&i| self.entries[i].credits)
            .max()
            .unwrap_or(0);

        const SCALE: f64 = 1_000_000.0;
        let weights: Vec<i64> = cands
            .iter()
            .map(|&i| {
                let w = if tiered {
                    self.tier_weight_of(i, now)
                } else {
                    self.weight_of(i, max_credits, now)
                };
                (w * SCALE) as i64
            })
            .collect();
        let total: i64 = weights.iter().sum();

        if total <= 0 {
            let n = cands.len() as i64;
            return cands[(self.rand_n(n) as usize).min(cands.len() - 1)];
        }
        let r = self.rand_n(total);
        let mut acc: i64 = 0;
        for (k, &i) in cands.iter().enumerate() {
            acc += weights[k];
            if r < acc {
                return i;
            }
        }
        cands[cands.len() - 1]
    }

    /// 记录一次选中使用（时刻 + 单调序号）。
    fn mark_used(&mut self, idx: usize) {
        self.pick_seq += 1;
        let e = &mut self.entries[idx];
        e.last_used_seq = self.pick_seq;
        e.last_used = Some(SystemTime::now());
    }
}

/// 防并发撞号窗口：同一账号在该窗口内不重复被选中（除非候选全部刚被用过）。
const MIN_PICK_GAP: Duration = Duration::from_millis(100);

/// 短名单大小：加权抽签前按权重降序保留的候选数。
///
/// 与 Go 侧 `shortlistSize` 对应。为什么要有短名单：权重最高的账号应当拿到
/// 大部分流量，但**不能**让权重第 6 名之后完全没有机会 —— 那个账号可能刚好
/// 是唯一能服务某个模型的。截断到 5 个既保留了「高权重优先」，又让抽签在
/// 一个可控的候选集里进行。
const SHORTLIST_SIZE: usize = 5;

/// 计算从 `now` 到下一个本地 04:00 的时长。
///
/// 与 Go 侧 `nextDay4AM` 一致：now 在当天 04:00 之前返回当天 04:00
///（此时签到尚未执行，等当天签到即可）；04:00 及之后返回次日 04:00。
fn next_day_4am_duration(now: chrono::DateTime<chrono::Local>) -> Duration {
    use chrono::{Datelike, Local, TimeZone, Timelike};

    // 用 date() 取当天 00:00，再 +1 天 +4 小时，天然覆盖月末/年末进位
    //（等价于 Go 侧 time.Date 对日溢出的自动进位）。
    let today4am = Local
        .with_ymd_and_hms(now.year(), now.month(), now.day(), 4, 0, 0)
        .single()
        .unwrap_or_else(|| Local.with_ymd_and_hms(now.year(), now.month(), now.day(), 4, 0, 0).earliest()
            .unwrap_or_else(|| now));

    // now 在当天 04:00 之前 → 当天 04:00；否则次日 04:00。
    let target = if now.hour() < 4 {
        today4am
    } else {
        today4am + chrono::Duration::days(1)
    };

    let secs = target.signed_duration_since(now).num_seconds().max(0) as u64;
    Duration::from_secs(secs)
}

/// 进程级随机种子（一次性）。
fn seed_once() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // 混合时间、计数器与栈地址，避免多线程同种子
    base ^ (n.wrapping_mul(0x9E37_79B9_7F4A_7C15)) ^ (Instant::now().elapsed().as_nanos() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(uid: &str) -> Auth {
        Auth {
            uid: uid.into(),
            access_token: format!("at-{uid}"),
            ..Default::default()
        }
    }

    #[test]
    fn add_and_pick() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.add(mk("u2"));
        let a = p.pick().unwrap();
        assert!(a.uid == "u1" || a.uid == "u2");
    }

    #[test]
    fn cooldown_makes_unhealthy_then_recovers() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        assert!(p.servable_now());
        p.cooldown("u1", CoolKind::Soft, Duration::from_millis(50), "429");
        assert!(!p.servable_now(), "冷却期内应不可选");
        std::thread::sleep(Duration::from_millis(80));
        assert!(p.servable_now(), "冷却到期后应恢复");
    }

    #[test]
    fn disable_is_permanent() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.disable("u1", "session dead");
        std::thread::sleep(Duration::from_millis(10));
        assert!(!p.servable_now());
        assert_eq!(p.list()[0].disabled, true);
    }

    // ---------- session_dead 家族（对应 Go NoteSessionDead / ClearSessionDead）----------

    /// 核心语义：**一次** 12153 不杀号。
    ///
    /// 这是本机制存在的全部理由。若这条挂了，说明退回了「一次即杀」的旧行为 ——
    /// 而那正是 Go 侧实测误杀过一批健康账号的原因。
    #[test]
    fn one_session_dead_does_not_disable() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        assert!(!p.note_session_dead("u1"), "第 1 次不应达阈值");
        assert!(p.servable_now(), "第 1 次 12153 后账号仍必须可选");
        assert!(!p.list()[0].disabled);
        assert_eq!(p.session_dead_fails("u1"), 1);
    }

    /// 连续 3 次才禁用，且禁用文案与旧版逐字一致（运维脚本按 `reason` 匹配）。
    #[test]
    fn three_consecutive_session_deads_disable_the_account() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        assert!(!p.note_session_dead("u1"), "第 1 次未达阈值");
        assert!(!p.note_session_dead("u1"), "第 2 次未达阈值");
        assert!(p.note_session_dead("u1"), "第 3 次应达阈值并完成禁用");
        assert!(!p.servable_now(), "达阈值后账号必须被禁用");
        let s = &p.list()[0];
        assert!(s.disabled);
        assert_eq!(s.reason, SESSION_DEAD_REASON);
        assert_eq!(s.reason, "12153 session dead", "文案必须与 Go 逐字一致");
    }

    /// 达阈值时计数**重置为 0**：复活后从零重新观察，
    /// 而不是「再抖一次就立刻又杀」。
    #[test]
    fn reaching_threshold_resets_the_counter() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.note_session_dead("u1");
        p.note_session_dead("u1");
        assert!(p.note_session_dead("u1"));
        assert_eq!(p.session_dead_fails("u1"), 0, "达阈值后计数必须归零");
        // 复活后需要重新连续 3 次才会再被禁用。
        assert!(p.revive_disabled("u1"));
        assert!(!p.note_session_dead("u1"), "复活后第 1 次不应立刻再杀");
        assert!(p.servable_now());
    }

    /// `clear_session_dead` 打断连续性：这正是「连续」二字的落点。
    ///
    /// 若这条挂了（即清零无效），一个月的偶发抖动累计到 3 次照样杀号，
    /// 机制退化成「累计 3 次即杀」。
    #[test]
    fn clear_session_dead_breaks_the_streak() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.note_session_dead("u1");
        p.note_session_dead("u1");
        assert_eq!(p.session_dead_fails("u1"), 2);
        p.clear_session_dead("u1");
        assert_eq!(p.session_dead_fails("u1"), 0, "清零后必须从零开始");
        // 再抖两次仍不应达阈值（若清零无效，第 3 次就会杀）。
        assert!(!p.note_session_dead("u1"));
        assert!(!p.note_session_dead("u1"));
        assert!(p.servable_now(), "清零有效时账号必须仍可用");
        // 第 3 次（清零后的连续第 3 次）才杀。
        assert!(p.note_session_dead("u1"));
        assert!(!p.servable_now());
    }

    /// 长期偶发抖动不得累计杀号 —— 用「抖动 → 清零」交替 10 轮验证，
    /// 模拟「网络抖动 / 上游闪断 / refresh 竞态」的真实节奏。
    #[test]
    fn intermittent_session_deads_never_accumulate_to_a_kill() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        for round in 0..10 {
            p.note_session_dead("u1");
            assert!(
                p.servable_now(),
                "第 {round} 轮抖动后账号不应被禁用（清零打断了连续性）"
            );
            p.clear_session_dead("u1");
        }
        assert_eq!(p.session_dead_fails("u1"), 0);
    }

    /// chat 成功也清零（Go `NoteSuccess` 里的 `sessionDeadFails = 0`）：
    /// 一次成功就证明 session 没死。
    #[test]
    fn note_success_clears_session_dead_counter() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.note_session_dead("u1");
        p.note_session_dead("u1");
        assert_eq!(p.session_dead_fails("u1"), 2);
        p.note_success("u1");
        assert_eq!(p.session_dead_fails("u1"), 0, "chat 成功必须清零连续计数");
        assert!(!p.note_session_dead("u1"));
        assert!(!p.note_session_dead("u1"));
        assert!(p.servable_now(), "成功清零后不应被两次抖动杀掉");
    }

    /// 手工复活也清零（Go `ReviveDisabled` 里的 `sessionDeadFails = 0`）：
    /// 复活是最强的「账号没死」信号。
    #[test]
    fn revive_disabled_clears_session_dead_counter() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.note_session_dead("u1");
        p.note_session_dead("u1");
        p.note_session_dead("u1");
        assert!(!p.servable_now());
        assert_eq!(p.session_dead_fails("u1"), 0, "达阈值时已重置");
        // 再抖一次把计数推到 1，然后复活。
        assert!(!p.note_session_dead("u1"));
        assert_eq!(p.session_dead_fails("u1"), 1);
        assert!(p.revive_disabled("u1"));
        assert_eq!(p.session_dead_fails("u1"), 0, "复活必须清零连续计数");
        assert!(p.servable_now());
    }

    // ---------- 平台总开关（platform_enabled）----------
    //
    // ★ 业主原话（m01323）：「关闭平台只是不参与兼容网关，并不是不执行任务请注意」
    // ⇒ 语义 = `no_route` 的平台级版本，只影响选号。

    /// ★ 实测回归：**带产品前缀**的请求也必须被平台总开关拦住。
    ///
    /// 这是真机实测抓到的缺陷：判据最初只加在裸名路径（`pick_for_model_any`），
    /// 于是 `qoder:国服:GLM-5.3` 走 `pick_for_model_product_region` →
    /// `pick_inner` 时**绕过开关**，关掉 qoder 后仍返回 200（真上游内容）。
    #[test]
    fn platform_switch_blocks_product_prefixed_pick() {
        let mut p = Pool::new(String::new());
        let mut q = mk("q1");
        q.product = "qoder".into();
        p.add(q);

        // 开关没配 ⇒ 参加（向后兼容）。
        assert!(
            p.pick_for_model_product_region("glm-5.3", &[], Region::Any, "qoder")
                .is_some(),
            "未配置开关时应照常选号"
        );

        p.set_platform_enabled(&HashMap::from([("qoder".to_string(), false)]));
        assert!(
            p.pick_for_model_product_region("glm-5.3", &[], Region::Any, "qoder")
                .is_none(),
            "平台已关闭时，带产品前缀的请求必须选不出号"
        );
    }

    /// 关闭平台**不得**影响任何养号/观测路径（业主 m01323 的硬约束）。
    ///
    /// 落点必须只在选号里：`probe_uids`（只读探测）、`all_uids`（运维观测）、
    /// `available_uids`（会话粘性回调）、`servable_now`（健康探针）都照旧 ——
    /// 一旦谁把开关塞进 `healthy()`，签到/上报/成长任务就会跟着停摆。
    #[test]
    fn platform_switch_never_touches_task_or_probe_paths() {
        let mut p = Pool::new(String::new());
        let mut q = mk("q1");
        q.product = "qoder".into();
        p.add(q);
        p.set_platform_enabled(&HashMap::from([("qoder".to_string(), false)]));

        assert_eq!(p.probe_uids(), vec!["q1".to_string()], "只读探测必须照跑");
        assert_eq!(p.all_uids(), vec!["q1".to_string()], "运维观测必须照常可见");
        assert_eq!(
            p.available_uids(),
            vec!["q1".to_string()],
            "可用账号列表不受开关影响"
        );
        assert!(p.servable_now(), "账号本身是健康的");
        // 账号级开关是另一个维度，绝不能被平台开关顺手改掉。
        let s = &p.list()[0];
        assert!(!s.disabled, "平台开关不得写进账号 disabled");
        assert!(!s.no_route, "平台开关不得写进账号 no_route");
    }

    /// 粘性直取路径同样要过平台开关（与产品校验同一个理由）。
    #[test]
    fn platform_switch_blocks_sticky_pick() {
        let mut p = Pool::new(String::new());
        let mut q = mk("q1");
        q.product = "qoder".into();
        p.add(q);
        p.set_platform_enabled(&HashMap::from([("qoder".to_string(), false)]));
        assert!(
            p.pick_by_uid_for_model_product_region("q1", "glm-5.3", Region::Any, "qoder")
                .is_none(),
            "粘性路径不得绕过平台开关"
        );
        // 空产品（不指定平台）时该函数退化成普通直取，开关由 pick_inner 负责。
        assert!(
            p.pick_by_uid_for_model_product_region("q1", "glm-5.3", Region::Any, "")
                .is_some(),
            "不指定平台时粘性直取行为不变"
        );
    }

    /// 归一化：只留显式 `false`，平台名 trim + 小写，空键跳过。
    #[test]
    fn set_platform_enabled_keeps_only_explicit_false() {
        let mut p = Pool::new(String::new());
        p.set_platform_enabled(&HashMap::from([
            ("  QoDer ".to_string(), false),
            ("workbuddy".to_string(), true),
            ("".to_string(), false),
        ]));
        assert!(!p.platform_enabled("qoder"), "应识别 trim+小写后的 qoder");
        assert!(!p.platform_enabled("QODER"));
        assert!(p.platform_enabled("workbuddy"), "true 不落盘 = 参加");
        assert!(p.platform_enabled("trae"), "未配置的平台 = 参加");
        assert!(p.platform_enabled(""), "空平台名一律视为参加");
    }

    /// 复活只清系统判定位，**不碰**用户手工拨的 `no_route` 开关。
    ///
    /// 顺手清掉会让一次「复活」把用户明确摘除的号悄悄放回选号池 ——
    /// 这是用户意图与池运行态的边界，不能越。
    #[test]
    fn revive_disabled_does_not_touch_user_no_route_switch() {
        let mut p = Pool::new(String::new());
        let mut a = mk("u1");
        a.no_route = true;
        p.add(a);
        p.note_session_dead("u1");
        p.note_session_dead("u1");
        p.note_session_dead("u1");
        assert!(p.revive_disabled("u1"));
        // 系统判定位已清，但用户开关仍在。
        assert_eq!(p.list()[0].disabled, false);
        assert_eq!(p.list()[0].reason, "");
        assert_eq!(p.list()[0].no_route, true, "复活不得清用户开关");
        assert!(!p.servable_now(), "no_route 账号仍不接流量");
    }

    /// 计数**不持久化**：重启即归零（与 Go `stateAccount` 无该字段一致）。
    ///
    /// 带着旧计数跨重启会让「连续」跨过重启边界而失去意义。
    #[test]
    fn session_dead_counter_is_not_persisted() {
        let dir = std::env::temp_dir().join(format!(
            "wbswg-pool-sdead-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.join("state.json");
        let sf = state.to_string_lossy().to_string();

        {
            let mut p = Pool::new(sf.clone());
            p.add(mk("u1"));
            p.note_session_dead("u1");
            p.note_session_dead("u1");
            assert_eq!(p.session_dead_fails("u1"), 2);
            assert!(p.flush(), "有 dirty 变化应落盘");
        }

        // 重启：加载 state.json。
        let mut p2 = Pool::new(sf);
        p2.add(mk("u1"));
        assert_eq!(
            p2.session_dead_fails("u1"),
            0,
            "计数不得持久化：重启后必须归零"
        );
        // 若旧计数被错误持久化，这里第 1 次就会达阈值并杀号。
        assert!(!p2.note_session_dead("u1"), "重启后第 1 次不应立刻杀号");
        assert!(p2.servable_now());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 未达阈值**不得**触碰 `disabled` / `reason`：抖动期账号状态必须干净。
    #[test]
    fn below_threshold_leaves_disabled_and_reason_untouched() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        for _ in 0..(SESSION_DEAD_THRESHOLD - 1) {
            p.note_session_dead("u1");
        }
        let s = &p.list()[0];
        assert!(!s.disabled, "未达阈值不得禁用");
        assert_eq!(s.reason, "", "未达阈值不得写 reason");
        assert_eq!(
            p.session_dead_fails("u1"),
            SESSION_DEAD_THRESHOLD - 1,
            "计数应如实累计到阈值-1"
        );
    }

    /// 未知 UID 的所有 session_dead 操作都安全（不 panic、不误伤他人）。
    #[test]
    fn session_dead_ops_on_unknown_uid_are_safe() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        assert!(!p.note_session_dead("nope"));
        assert_eq!(p.session_dead_fails("nope"), 0);
        p.clear_session_dead("nope");
        assert!(p.servable_now(), "未知 UID 的操作不得影响池内账号");
        assert_eq!(p.session_dead_fails("u1"), 0);
        assert_eq!(p.session_dead_threshold(), 3);
    }

    /// 两个账号的计数互不串扰。
    #[test]
    fn session_dead_counters_are_per_account() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.add(mk("u2"));
        p.note_session_dead("u1");
        p.note_session_dead("u1");
        p.note_session_dead("u2");
        assert_eq!(p.session_dead_fails("u1"), 2);
        assert_eq!(p.session_dead_fails("u2"), 1);
        assert!(p.note_session_dead("u1"), "u1 第 3 次应达阈值");
        assert!(p.servable_now(), "u2 仍健康，池应可选");
        let by: std::collections::HashMap<_, _> =
            p.list().into_iter().map(|s| (s.uid.clone(), s)).collect();
        assert!(by["u1"].disabled);
        assert!(!by["u2"].disabled);
        assert_eq!(p.session_dead_fails("u2"), 1, "u1 杀号不得影响 u2 的计数");
    }

    #[test]
    fn breaker_trips_after_threshold() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.set_breaker(3, Duration::from_secs(60), Duration::from_secs(3600));
        p.note_error("u1");
        assert!(p.servable_now(), "未达阈值不应熔断");
        p.note_error("u1");
        assert!(p.servable_now());
        p.note_error("u1");
        assert!(!p.servable_now(), "达到阈值(3)应熔断");
    }

    #[test]
    fn note_success_clears_breaker() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.set_breaker(2, Duration::from_secs(60), Duration::from_secs(3600));
        p.note_error("u1");
        p.note_error("u1");
        assert!(!p.servable_now());
        p.note_success("u1");
        assert!(p.servable_now(), "成功应清熔断");
    }

    #[test]
    fn in_flight_limits() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.set_max_in_flight(2);
        assert!(p.acquire("u1"));
        assert!(p.acquire("u1"));
        assert!(!p.acquire("u1"), "超过上限应拒绝");
        assert!(!p.servable_now(), "占满后不可受理");
        p.release("u1");
        assert!(p.servable_now());
    }

    #[test]
    fn pick_excluding_skips_tried() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        let a = p.pick_excluding(&["u1".to_string()]);
        assert!(a.is_none(), "唯一账号被排除后应无候选（且无冷却兜底）");
    }

    #[test]
    fn pick_by_uid_respects_health() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        assert!(p.pick_by_uid("u1").is_some());
        assert!(p.pick_by_uid("nope").is_none());
        p.cooldown("u1", CoolKind::Soft, Duration::from_secs(60), "x");
        assert!(p.pick_by_uid("u1").is_none());
    }

    #[test]
    fn anti_thundering_herd_uses_seq_tiebreak() {
        // 复刻 Go 侧 TestPickAntiThunderingHerd 的核心断言：
        // 大量连续 Pick 不应全部落在同一账号上。
        // （Go 版的并发版依赖 goroutine；这里用串行 Pick 验证 LRU 兜底能发散，
        //   因为串行时每个账号都会进入 minPickGap 窗口，必然走 LRU 分支。）
        let mut p = Pool::new(String::new());
        for i in 0..10 {
            p.add(mk(&format!("c{i:02}")));
        }
        let mut counts: HashMap<String, i64> = HashMap::new();
        const N: i64 = 100;
        for _ in 0..N {
            if let Some(a) = p.pick() {
                *counts.entry(a.uid.clone()).or_insert(0) += 1;
            }
        }
        assert!(counts.len() >= 2, "应覆盖多个账号: {counts:?}");
        for (uid, n) in &counts {
            assert!(*n <= N / 2, "惊群：{uid} 被选中 {n}/{N} 次");
        }
    }

    #[test]
    fn expiry_tier_prefers_earliest() {
        let mut p = Pool::new(String::new());
        let mut late = mk("late");
        late.soonest_expire_at = 2000000000; // 远未来
        let mut early = mk("early");
        early.soonest_expire_at = 1000000000; // 更早
        p.add(late);
        p.add(early);

        // 分层后只有最早档入选
        let mut counts: HashMap<String, i64> = HashMap::new();
        for _ in 0..50 {
            if let Some(a) = p.pick() {
                *counts.entry(a.uid.clone()).or_insert(0) += 1;
            }
        }
        assert_eq!(counts.get("early").copied().unwrap_or(0), 50, "应固定选最早到期档: {counts:?}");
        assert!(!counts.contains_key("late"), "较晚档不应被选中: {counts:?}");
    }

    #[test]
    fn counts_and_available_uids() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.add(mk("u2"));
        p.cooldown("u2", CoolKind::Soft, Duration::from_secs(60), "x");
        let (total, healthy, cooling, disabled, full) = p.counts_detailed();
        assert_eq!(total, 2);
        assert_eq!(healthy, 1);
        assert_eq!(cooling, 1);
        assert_eq!(disabled, 0);
        assert_eq!(full, 0);
        assert_eq!(p.available_uids(), vec!["u1".to_string()]);
    }

    /// 冷却入口也是熔断器的失败信号（对齐 Go `Cooldown → recordBreakerFailureLocked`）。
    ///
    /// 回归防护：Go 版把 `fails++` 放在 `recordBreakerFailureLocked` 内部，
    /// 因此冷却必须能触发熔断；若把自增搬到调用方，冷却就永远喂不进熔断器。
    #[test]
    fn cooldown_feeds_breaker() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.set_breaker(3, Duration::from_secs(3600), Duration::from_secs(3600));
        // 连续 3 次软冷却（429）应触发熔断，即使从未调用 note_error
        p.cooldown("u1", CoolKind::Soft, Duration::from_millis(1), "429");
        p.cooldown("u1", CoolKind::Soft, Duration::from_millis(1), "429");
        // 冷却本身极短，此时是否可选取决于熔断是否触发
        std::thread::sleep(Duration::from_millis(20));
        assert!(p.servable_now(), "未达阈值不应熔断");
        p.cooldown("u1", CoolKind::Soft, Duration::from_millis(1), "429");
        std::thread::sleep(Duration::from_millis(20));
        assert!(!p.servable_now(), "3 次冷却应触发熔断（冷却本身已过期）");
    }

    #[test]
    fn breaker_threshold_is_exclusive_of_extra_increment() {
        // 阈值 3：第 1、2 次失败不熔断，第 3 次熔断（不多不少）
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.set_breaker(3, Duration::from_secs(3600), Duration::from_secs(3600));
        p.note_error("u1");
        p.note_error("u1");
        assert!(p.servable_now(), "2 次失败不应熔断");
        p.note_error("u1");
        assert!(!p.servable_now(), "3 次失败应熔断");
        assert_eq!(p.list()[0].breaker_fails, 0, "触发后 fails 应清零");
    }

    /// 回归防护：Go 的零值 time.Time 序列化为 `"0001-01-01T00:00:00Z"`，
    /// 直接交给 SystemTime 会因早于 UNIX_EPOCH 而让**整个 state.json 解析失败**
    ///（实测表现为 credits 全为 0、冷却/统计全丢）。
    #[test]
    fn go_zero_time_parses_as_none() {
        let raw = br#"{
          "accounts": {
            "u1": {
              "credits": 4360,
              "disabled": false,
              "until": "0001-01-01T00:00:00Z",
              "cool_kind": 0,
              "success_count": 922,
              "last_success": "2026-09-12T05:51:44.1541013+08:00",
              "last_err": "0001-01-01T00:00:00Z",
              "expire_at": 1790636739
            }
          }
        }"#;
        let sf: StateFile = serde_json::from_slice(raw).expect("Go 状态文件必须可解析");
        let a = sf.accounts.get("u1").unwrap();
        assert_eq!(a.credits, 4360);
        assert_eq!(a.success_count, 922);
        assert_eq!(a.expire_at, 1790636739);
        // 零值时间 → None（= 不在冷却期）
        assert!(a.until.is_none(), "零值 until 应为 None");
        assert!(a.last_err.is_none(), "零值 last_err 应为 None");
        // 有效时间应正常解析
        assert!(a.last_success.is_some(), "有效 last_success 应解析成功");
    }

    #[test]
    fn load_state_restores_credits_and_survives_add() {
        let dir = std::env::temp_dir().join(format!("wbswg-state-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let fp = dir.join("state.json");
        std::fs::write(
            &fp,
            br#"{"accounts":{"u1":{"credits":4360,"success_count":922,"expire_at":1790636739,
                 "until":"0001-01-01T00:00:00Z","last_err":"0001-01-01T00:00:00Z"}}}"#,
        )
        .unwrap();

        let mut p = Pool::new(fp.to_string_lossy().to_string());
        p.load_state();
        assert!(p.loaded_from_state(), "应报告已从 state.json 恢复");

        // 只载入 state 时是占位凭证，credits 已恢复
        let st = &p.list()[0];
        assert_eq!(st.credits, 4360);
        assert_eq!(st.success_count, 922);
        assert!(!st.cooling, "零值 until 不应视为冷却");

        // add() 灌入完整凭证后，运行态必须保留（credits/统计不丢）
        p.add(Auth {
            uid: "u1".into(),
            access_token: "at".into(),
            nickname: "猫猫".into(),
            soonest_expire_at: 1790636739,
            ..Default::default()
        });
        let st = &p.list()[0];
        assert_eq!(st.credits, 4360, "add() 不应清掉已恢复的 credits");
        assert_eq!(st.success_count, 922, "add() 不应清掉已恢复的统计");
        assert_eq!(st.nickname, "猫猫", "add() 应换入完整凭证");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 启动路径必须用 `sync_to_dir` 而不是逐个 `add`：占位条目要能被剔除。
    ///
    /// ★ 这条测试守的是一个**真实存在过**的缺陷（对应 Go `main.go:206 p.SyncToDir(auths)`）：
    /// `load_state()` 会为 state.json 里的**每个**条目建一个占位条目
    ///（`access_token` 为空串），而 `add()` 只做「加入/换新」**不剔除**。
    /// 于是「凭证文件已删除、但 state.json 仍有条目」的账号会在**每次启动时复活**，
    /// 且 `healthy()` / `pick_inner()` 都**不检查 `access_token` 是否为空**
    ///（只有 `probe_uids()` 检查）⇒ 它会被正常选中，用空 token 发请求 ⇒ 必然 401。
    ///
    /// 现场可达：用户生产实例 state.json 有 31 条而 `gateway_auths\` 只有 27 个文件；
    /// 账号页删账号只删账号库条目与凭证文件，**不删 state.json 条目**
    ///（见 `modules/gateway.rs` 的清理循环）。
    #[test]
    fn startup_sync_to_dir_drops_orphan_state_entries_but_add_would_keep_them() {
        let dir = std::env::temp_dir().join(format!("wbswg-orphan-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let fp = dir.join("state.json");
        // 两个条目：u1 的凭证文件还在（下方 auths 里交出），u2 的已被删除。
        std::fs::write(
            &fp,
            br#"{"accounts":{"u1":{"credits":100},"u2":{"credits":200}}}"#,
        )
        .unwrap();

        let mut p = Pool::new(fp.to_string_lossy().to_string());
        p.load_state();
        assert_eq!(p.all_uids().len(), 2, "state 里的两条都应先成为占位条目");

        // ① 反证：逐个 add（修复前的写法）**不会**剔除孤儿 —— u2 留在池里。
        let mut before = Pool::new(fp.to_string_lossy().to_string());
        before.load_state();
        before.add(mk("u1"));
        let uids = before.all_uids();
        assert!(
            uids.contains(&"u2".to_string()),
            "add() 不剔除：这正是修复前每次启动都让已删账号复活的原因"
        );
        assert!(
            before
                .list()
                .iter()
                .any(|s| s.uid == "u2" && !s.cooling && s.disabled == false),
            "孤儿条目是「健康」的：healthy()/pick_inner() 都不看 access_token 是否为空"
        );

        // ② 修复后的写法：sync_to_dir 剔除孤儿，同时保留 u1 已恢复的运行态。
        p.sync_to_dir(&[mk("u1")]);
        let uids = p.all_uids();
        assert_eq!(uids.len(), 1, "孤儿条目必须被剔除，否则下次启动又复活");
        assert_eq!(uids[0], "u1");
        assert_eq!(p.list()[0].credits, 100, "剔除孤儿不应影响幸存账号的运行态");
        assert!(p.flush(), "剔除后必须置脏，好让结果落盘（对应 Go 的 changed 门控）");

        // 落盘结果可被自己读回：孤儿不再复活。
        let mut p2 = Pool::new(fp.to_string_lossy().to_string());
        p2.load_state();
        assert_eq!(p2.all_uids(), vec!["u1".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flush_only_writes_when_dirty() {
        let dir = std::env::temp_dir().join(format!("wbswg-flush-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let fp = dir.join("state.json");

        let mut p = Pool::new(fp.to_string_lossy().to_string());
        p.add(mk("u1"));
        // 刚 add，未标记 dirty → 不写
        assert!(!p.flush(), "非 dirty 不应写盘");

        p.set_credits("u1", 100);
        assert!(p.flush(), "dirty 后应写盘");
        assert!(fp.exists());

        // 写回内容可被自己读回
        let mut p2 = Pool::new(fp.to_string_lossy().to_string());
        p2.load_state();
        assert_eq!(p2.list()[0].credits, 100);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn err_count_legacy_field_migrates_to_err_total() {
        // 旧文件只有 err_count → 迁移进 err_total（取较大者）
        let raw = br#"{"accounts":{"u1":{"credits":1,"err_count":7}}}"#;
        let sf: StateFile = serde_json::from_slice(raw).unwrap();
        assert_eq!(sf.accounts["u1"].err_count, 7);
        assert_eq!(sf.accounts["u1"].err_total, 0);

        let dir = std::env::temp_dir().join(format!("wbswg-mig-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let fp = dir.join("state.json");
        std::fs::write(&fp, raw).unwrap();
        let mut p = Pool::new(fp.to_string_lossy().to_string());
        p.load_state();
        assert_eq!(p.list()[0].err_total, 7, "err_count 应迁移为 err_total");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reenable_if_credits_revives_cooling() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.cooldown("u1", CoolKind::Hard, Duration::from_secs(3600), "余额不足");
        assert!(!p.servable_now());
        p.reenable_if_credits("u1", 100);
        assert!(p.servable_now(), "签到有余额应解冻");
        // 余额为 0 则不解冻
        p.cooldown("u1", CoolKind::Hard, Duration::from_secs(3600), "余额不足");
        p.reenable_if_credits("u1", 0);
        assert!(!p.servable_now());
    }

    /// ★★ 2026-10-08 业主现场回归：**兜底选号也必须「免费优先」**。
    ///
    /// # 守的是什么
    ///
    /// 现场：12 个国际版号对 `deepseek-v4.1-flash` 是 `x0.00`（免费）、15 个国服号
    /// 是 `x0.11`（计费）。正常路径的 `free_tier` 把国服号筛掉了（实测 502 次 chat
    /// 全走国际版），**但兜底漏了这一步** ⇒ 国服号被稳定选中：
    ///
    /// ```text
    /// 国际版 12 个号长期占满在途（max_in_flight=3，只有它们会被正常选中）
    /// ⇒ 兜底里 `in_flight_full` 把它们全跳过
    /// ⇒ 只剩 15 个永远空闲的国服号 ⇒ 每次都挑中国服号
    /// ```
    ///
    /// 实测读数：15 个国服号被**均匀摊了 710 条**（47–50 条/号），每条平均
    /// 27 万 input token（与真实对话同量级）。
    ///
    /// # 为什么这里用「冷却」构造
    ///
    /// 兜底只在**正常候选为空**时触发，而 [`Entry::expiry`] 取的是**冷却截止时刻**
    ///（`until` / `breaker_until`），所以兜底池里的账号必然是「正在冷却」的。
    /// 本用例给两个号各加一段冷却，并把**免费号**的冷却结束得更**晚** —— 若兜底
    /// 不看成本，它必然挑中冷却更早结束的付费号；修好后必须挑中免费号。
    #[test]
    fn fallback_earliest_expiry_prefers_free_over_earlier_paid() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        p.add(mk("free1"));
        p.set_cost_rate_known("free1", 0.0, true);
        p.add(mk("paid1"));
        p.set_cost_rate_known("paid1", 0.11, true);
        // 付费号冷却 60s 后结束；免费号 120s —— 旧实现按「最早结束」必挑付费号。
        p.cooldown("paid1", CoolKind::Soft, Duration::from_secs(60), "test");
        p.cooldown("free1", CoolKind::Soft, Duration::from_secs(120), "test");

        let now = SystemTime::now();
        let a = p
            .pick_earliest_expiry(&[], now, "deepseek-v4.1-flash", "")
            .expect("兜底应能选出账号");
        assert_eq!(
            a.uid, "free1",
            "兜底必须免费优先 —— 挑中 paid1 说明它又绕过了成本维度（业主现场回归）"
        );
    }

    /// 兜底的「免费优先」不得在**没有免费候选**时改变行为。
    ///
    /// 既有契约逐字不变：「两个付费账号之间仍先烧快过期的那个」。
    /// 这条是上一条的**反向护栏** —— 少了它，一个「永远挑第一个」的实现也能过。
    #[test]
    fn fallback_earliest_expiry_keeps_earliest_when_all_paid() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        p.add(mk("paid_late"));
        p.set_cost_rate_known("paid_late", 0.50, true);
        p.add(mk("paid_early"));
        p.set_cost_rate_known("paid_early", 0.11, true);
        p.cooldown("paid_late", CoolKind::Soft, Duration::from_secs(120), "test");
        p.cooldown("paid_early", CoolKind::Soft, Duration::from_secs(60), "test");

        let now = SystemTime::now();
        let a = p
            .pick_earliest_expiry(&[], now, "deepseek-v4.1-flash", "")
            .expect("兜底应能选出账号");
        assert_eq!(
            a.uid, "paid_early",
            "全付费时仍必须挑冷却最早结束的（既有契约）"
        );
    }

    /// `cost_known = false`（未声明）**不算免费** —— 兜底同样适用。
    ///
    /// 否则某个还没查到倍率的平台会抢走全部兜底流量，而那正是倍率表最后补齐的一批。
    #[test]
    fn fallback_earliest_expiry_treats_unknown_cost_as_not_free() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        // 未声明倍率、冷却结束更晚 ⇒ 不得被当成免费而优先
        p.add(mk("unknown_late"));
        // 明确计费、冷却结束更早 ⇒ 必须挑它
        p.add(mk("paid_early"));
        p.set_cost_rate_known("paid_early", 0.11, true);
        p.cooldown("unknown_late", CoolKind::Soft, Duration::from_secs(120), "test");
        p.cooldown("paid_early", CoolKind::Soft, Duration::from_secs(60), "test");

        let now = SystemTime::now();
        let a = p
            .pick_earliest_expiry(&[], now, "deepseek-v4.1-flash", "")
            .expect("兜底应能选出账号");
        assert_eq!(
            a.uid, "paid_early",
            "未声明倍率不能被当成免费（那会让它抢走全部兜底流量）"
        );
    }

    /// 成本维度**关闭**时（`multi_product_on = false`）兜底逐字退回旧行为。
    #[test]
    fn fallback_earliest_expiry_is_inert_when_multi_product_off() {
        let mut p = Pool::new(String::new());
        // 刻意不开 multi_product：free_tier 必须完全不介入
        p.add(mk("free_late"));
        p.set_cost_rate_known("free_late", 0.0, true);
        p.add(mk("paid_early"));
        p.set_cost_rate_known("paid_early", 0.11, true);
        p.cooldown("free_late", CoolKind::Soft, Duration::from_secs(120), "test");
        p.cooldown("paid_early", CoolKind::Soft, Duration::from_secs(60), "test");

        let now = SystemTime::now();
        let a = p
            .pick_earliest_expiry(&[], now, "deepseek-v4.1-flash", "")
            .expect("兜底应能选出账号");
        assert_eq!(a.uid, "paid_early", "成本维度关闭时行为必须逐字不变");
    }
    /// ★★ 2026-10-08 业主现场回归：**粘性快路径也必须尊重成本维度**。
    ///
    /// # 守的是什么
    ///
    /// 现场：15 个国服号（`x0.11`）被**均匀摊了 710 条**对话记录，而 12 个国际版号
    /// 是 `x0.00`（免费）。成因是会话绑定：`pick_by_uid_for_model_region` 只查
    /// healthy / in_flight / model_available，**完全不看 cost** ⇒ 一旦绑到计费号，
    /// 30 分钟 TTL 内所有请求都走它。15 个号被均匀分摊正是「各会话绑到不同号」的形状。
    ///
    /// 修法：绑定号是「确定计费」而池里存在「确定免费」的可服务候选时，返回 `None`
    /// 让调用方解绑、走完整选号（那里 `free_tier` 生效）。
    #[test]
    fn sticky_binding_yields_to_a_free_account() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        p.add(mk("paid"));
        p.set_cost_rate_known("paid", 0.11, true);
        p.add(mk("free"));
        p.set_cost_rate_known("free", 0.0, true);

        // 绑到计费号，而池里有免费号 ⇒ 必须拒绝（调用方据此解绑）
        assert!(
            p.pick_by_uid_for_model_region("paid", "deepseek-v4.1-flash", Region::Any)
                .is_none(),
            "存在免费候选时，粘性不该继续把请求钉在计费号上（业主现场回归）"
        );
        // 免费号自身照常可粘（它不「更贵」）
        assert!(
            p.pick_by_uid_for_model_region("free", "deepseek-v4.1-flash", Region::Any)
                .is_some(),
            "免费号的粘性必须保留"
        );
    }

    /// 粘性的成本守卫**不得**在「没有更优解」时解绑。
    ///
    /// 反向护栏：少了它，一个「只要绑的是计费号就解绑」的实现也能过 —— 而那会让
    /// 全池计费时白丢粘性、每轮重新选号。
    #[test]
    fn sticky_binding_survives_when_every_account_is_paid() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        p.add(mk("paid_a"));
        p.set_cost_rate_known("paid_a", 0.11, true);
        p.add(mk("paid_b"));
        p.set_cost_rate_known("paid_b", 0.50, true);

        assert!(
            p.pick_by_uid_for_model_region("paid_a", "deepseek-v4.1-flash", Region::Any)
                .is_some(),
            "全池都在计费时不该解绑（否则粘性白丢、每轮重选）"
        );
    }

    /// 倍率**未声明**的绑定号不算「更贵」⇒ 粘性保留。
    ///
    /// 与 `free_tier` 的「未声明不算免费」对称：未声明是中性，不该触发解绑。
    #[test]
    fn sticky_binding_survives_when_cost_is_unknown() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        p.add(mk("unknown")); // 不设 cost ⇒ cost_known=false
        p.add(mk("free"));
        p.set_cost_rate_known("free", 0.0, true);

        assert!(
            p.pick_by_uid_for_model_region("unknown", "deepseek-v4.1-flash", Region::Any)
                .is_some(),
            "未声明倍率是中性，不该被当成「更贵」而解绑"
        );
    }

    /// 成本维度**关闭**时粘性逐字退回旧行为。
    #[test]
    fn sticky_binding_is_inert_when_cost_dimension_is_off() {
        let mut p = Pool::new(String::new());
        // 刻意不开 multi_product
        p.add(mk("paid"));
        p.set_cost_rate_known("paid", 0.11, true);
        p.add(mk("free"));
        p.set_cost_rate_known("free", 0.0, true);

        assert!(
            p.pick_by_uid_for_model_region("paid", "deepseek-v4.1-flash", Region::Any)
                .is_some(),
            "成本维度关闭时粘性行为必须逐字不变"
        );
    }
    /// 造一个指定产品的账号（`domain` 保持默认 ⇒ 区域按 CN 算，与产品无关）。
    fn mk_product(uid: &str, product: &str) -> Auth {
        Auth {
            uid: uid.into(),
            access_token: format!("at-{uid}"),
            product: product.into(),
            ..Default::default()
        }
    }

    /// ★ 两轮排除：宿主清单里**明确没有**该模型的产品，其账号必须被排除。
    ///
    /// 对应 Go `TestPickForModelAnySkipsProductWithoutModel` 的语义。
    #[test]
    fn pick_for_model_any_excludes_products_that_do_not_declare_it() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        p.add(mk_product("wb1", "workbuddy"));
        p.add(mk_product("qd1", "qoder"));
        let mut pm: HashMap<String, Vec<String>> = HashMap::new();
        pm.insert("workbuddy".into(), vec!["glm-5.3".into()]);
        pm.insert("qoder".into(), vec!["Qwen3.8-Flash".into()]);
        p.set_product_models(&pm);

        // workbuddy 声明有 glm-5.3 ⇒ 只有它进 declared ⇒ 必须选到 wb1。
        let a = p.pick_for_model_any("glm-5.3", &[]).unwrap();
        assert_eq!(a.uid, "wb1", "只有 workbuddy 声明提供 glm-5.3");

        // qoder 声明有 Qwen3.8-Flash ⇒ 必须选到 qd1，而不是「碰巧可用」的 wb1。
        let a = p.pick_for_model_any("Qwen3.8-Flash", &[]).unwrap();
        assert_eq!(a.uid, "qd1", "只有 qoder 声明提供 Qwen3.8-Flash");
    }

    /// ★★ 核心回归：`declared` 非空时**绝不跨到 unknown 产品**。
    ///
    /// 现场（所有者 2026-09-21）：`GLM-5.3` 同时被 workbuddy 的**兜底**清单
    /// 与 qoder 的可信清单覆盖 ⇒ 若把兜底当「声明」，两产品一起竞争 ⇒
    /// 19 个 WorkBuddy 账号压倒 1 个 qoder 账号 ⇒ 选中其实没有该模型的
    /// WorkBuddy ⇒ 11102。
    ///
    /// 这里构造的正是那个形状：qoder 声明有 `glm-5.3`，而 workbuddy 的清单
    /// **未知**（只有兜底清单）。声明方唯一 ⇒ 只能选 qoder。
    #[test]
    fn pick_for_model_any_never_crosses_to_unknown_when_declared_exists() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        // 19 个 workbuddy（清单未知，但有兜底清单）+ 1 个 qoder（清单可信）
        for i in 0..19 {
            p.add(mk_product(&format!("wb{i}"), "workbuddy"));
        }
        p.add(mk_product("qd1", "qoder"));
        let mut pm: HashMap<String, Vec<String>> = HashMap::new();
        pm.insert("qoder".into(), vec!["glm-5.3".into()]);
        p.set_product_models(&pm);
        // workbuddy 的清单**未知**，网关补了兜底清单（含 glm-5.3）——
        // 但兜底**只用于「不排除」**，不参与「声明」竞争。
        let mut fb: HashMap<String, Vec<String>> = HashMap::new();
        fb.insert("workbuddy".into(), vec!["glm-5.3".into()]);
        p.set_product_models_fallback(&fb);

        // 兜底清单命中 ⇒ workbuddy 不被「排除」（product_may_serve = true），
        // 但它**没有声明** ⇒ 不能与 qoder 竞争。
        assert!(p.product_may_serve("workbuddy", "glm-5.3"));
        assert!(
            !p.product_declares_model("workbuddy", "glm-5.3"),
            "兜底清单不得参与「声明」"
        );
        assert!(p.product_declares_model("qoder", "glm-5.3"));

        let a = p.pick_for_model_any("glm-5.3", &[]).unwrap();
        assert_eq!(a.uid, "qd1", "声明方唯一时必须选 qoder，绝不能落到未知的 workbuddy");
    }

    /// ★ `declared` 非空但**该产品内一个号都挑不出来**时返回 `None`，不得跨到 unknown。
    ///
    /// ⚠ 这里用 `disable` 而不是 `cooldown` 来制造「挑不出号」：软冷却中的账号
    /// 会被 `pick_earliest_expiry` 兜底**选回来**（那是设计意图 —— 兜底就是
    /// 「宁可拿可能不好的号试一次」），所以冷却不足以证明「挑不出来」。
    /// 永久禁用的账号才是真的挑不出来（兜底也会跳过 `disabled`）。
    #[test]
    fn pick_for_model_any_returns_none_when_declared_product_is_unavailable() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        p.add(mk_product("wb1", "workbuddy")); // 清单未知，可服务
        p.add(mk_product("qd1", "qoder")); // 声明方，但会被永久禁用
        let mut pm: HashMap<String, Vec<String>> = HashMap::new();
        pm.insert("qoder".into(), vec!["Qwen3.8-Flash".into()]);
        p.set_product_models(&pm);

        // 声明方唯一且彻底不可用 ⇒ 严格路径挑不到 ⇒ 宁可 None，也不落到 workbuddy。
        p.disable("qd1", "session dead");
        let got = p.pick_for_model_any("Qwen3.8-Flash", &[]);
        assert!(
            got.is_none(),
            "声明过的产品挑不到号时不能跨到未知产品，实际选到 {:?}",
            got.map(|a| a.uid)
        );
    }

    /// ★ `declared` 为空（没有任何产品声明提供它）时才放开到 unknown。
    #[test]
    fn pick_for_model_any_falls_back_to_unknown_when_nothing_declares() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        p.add(mk_product("wb1", "workbuddy"));
        p.add(mk_product("qd1", "qoder"));
        let mut pm: HashMap<String, Vec<String>> = HashMap::new();
        pm.insert("workbuddy".into(), vec!["glm-5.3".into()]);
        pm.insert("qoder".into(), vec!["Qwen3.8-Flash".into()]);
        p.set_product_models(&pm);

        // 谁都没声明 `some-new-model`，但两边清单里也没有它 ⇒
        // product_may_serve 为 false（有清单且明确没有）⇒ 全被排除 ⇒ None。
        assert!(
            p.pick_for_model_any("some-new-model", &[]).is_none(),
            "有清单且明确没有 ⇒ 确定排除，不能瞎选"
        );
    }

    /// ★ 多产品关闭时保持既有行为：不排除任何账号。
    #[test]
    fn pick_for_model_any_is_inert_when_multi_product_off() {
        let mut p = Pool::new(String::new());
        p.add(mk_product("wb1", "workbuddy"));
        let mut pm: HashMap<String, Vec<String>> = HashMap::new();
        pm.insert("qoder".into(), vec!["Qwen3.8-Flash".into()]);
        p.set_product_models(&pm);
        // 未开 multi_product ⇒ product_declares_model / product_may_serve 均放行。
        let a = p.pick_for_model_any("Qwen3.8-Flash", &[]).unwrap();
        assert_eq!(a.uid, "wb1", "多产品关闭时应保持既有行为");
    }

    /// ★ 兜底清单：命中 → 不排除；**未命中 → 排除**（这是与 Go 逐条等价的关键）。
    #[test]
    fn product_may_serve_uses_fallback_list_when_host_list_unknown() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        // workbuddy 的宿主清单**未知**，只有兜底清单
        let mut fb: HashMap<String, Vec<String>> = HashMap::new();
        fb.insert("workbuddy".into(), vec!["glm-5.3".into()]);
        p.set_product_models_fallback(&fb);

        assert!(
            p.product_may_serve("workbuddy", "glm-5.3"),
            "兜底清单命中 ⇒ 不排除"
        );
        assert!(
            !p.product_may_serve("workbuddy", "Qwen3.8-Flash"),
            "★ 有兜底清单但未命中 ⇒ **排除**（否则裸名请求会撞上 WorkBuddy 回 11102）"
        );
        // 连兜底清单都没有的产品 → 真不知道 → 不排除
        assert!(
            p.product_may_serve("qoder", "whatever"),
            "没有兜底清单 ⇒ 真不知道 ⇒ 不排除"
        );
    }

    /// ★★★ 真实事故回归（所有者 2026-10-03）：裸名 `deepseek-v4.1-flash` 被路由到 Trae。
    ///
    /// Trae **根本没有**这个模型。事故成因：宿主下发的 `product_models` 里
    /// 没有 `trae` ⇒ `product_offers_model` 回「不知道」，而当时 Trae 又没有兜底
    /// 清单 ⇒ `product_may_serve` 只能 `return true` ⇒ 裸名请求落到 Trae 账号上。
    ///
    /// 修法：给 Trae 注入它**客户端可发**的模型名清单当兜底。兜底命中 → 放行，
    /// 未命中 → 排除。这条测试锁住的就是「未命中必须排除」。
    #[test]
    fn trae_fallback_excludes_a_model_trae_does_not_offer() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        p.add(mk_product("wb1", "workbuddy"));
        p.add(mk_product("tr1", "trae"));
        // 宿主清单：只有 qoder / zcode（这正是所有者机器上的真实形状）
        let mut pm: HashMap<String, Vec<String>> = HashMap::new();
        pm.insert("qoder".into(), vec!["Qwen3.8-Flash".into()]);
        pm.insert("zcode".into(), vec!["zcode:glm-5".into()]);
        p.set_product_models(&pm);
        // 网关兜底：workbuddy 与 trae 各自的客户端可发清单
        let mut fb: HashMap<String, Vec<String>> = HashMap::new();
        fb.insert("workbuddy".into(), vec!["deepseek-v4.1-flash".into()]);
        fb.insert(
            "trae".into(),
            crate::server::static_trae_model_ids()
                .into_iter()
                .map(|s| s.to_ascii_lowercase())
                .collect(),
        );
        p.set_product_models_fallback(&fb);

        // ★ 裸名 `deepseek-v4.1-flash`：Trae 兜底清单里**没有**它 ⇒ 必须排除。
        assert!(
            !p.product_may_serve("trae", "deepseek-v4.1-flash"),
            "Trae 没有 deepseek-v4.1-flash ⇒ 裸名请求绝不能落到 Trae 账号上"
        );
        // 而 Trae 真正提供的模型仍然放行（不能因噎废食）。
        assert!(
            p.product_may_serve("trae", "glm-5.2"),
            "Trae 确实提供 glm-5.2 ⇒ 不能排除"
        );
        assert!(
            p.product_may_serve("trae", "GLM-5.2"),
            "大小写不敏感"
        );

        // 选号也必须选到 workbuddy，而不是 Trae。
        for _ in 0..200 {
            let a = p.pick_for_model_any("deepseek-v4.1-flash", &[]).unwrap();
            assert_eq!(a.uid, "wb1", "裸名只能落到声明/兜底里有它的产品");
        }
    }

    /// ★ 兜底清单本身**不得**参与「声明」竞争 —— 否则 Trae 会与 WorkBuddy 抢号。
    ///
    /// 这正是所有者机器上「兼容网关显示 Trae 账号被路由」的另一半成因：
    /// 只要 Trae 的兜底清单被当成「声明」，19 个 WorkBuddy 账号就压不住它。
    #[test]
    fn trae_fallback_never_counts_as_a_declaration() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        p.add(mk_product("wb1", "workbuddy"));
        p.add(mk_product("tr1", "trae"));
        let mut fb: HashMap<String, Vec<String>> = HashMap::new();
        fb.insert(
            "trae".into(),
            crate::server::static_trae_model_ids()
                .into_iter()
                .map(|s| s.to_ascii_lowercase())
                .collect(),
        );
        p.set_product_models_fallback(&fb);

        assert!(
            !p.product_declares_model("trae", "glm-5.2"),
            "兜底清单只用于「不排除」，绝不能算「声明」"
        );
        assert!(
            !p.product_declares_model("trae", "deepseek-v4.1-flash"),
            "Trae 既没声明也没兜底这个模型"
        );
    }

    /// ★★★ 真实事故回归（所有者 2026-10-06）：裸名 `space-bunny` 报
    /// `503 no_healthy_account / all accounts unavailable (cooling/disabled)`。
    ///
    /// # 现场形状（逐条取自业主机器的真实配置）
    ///
    /// - 宿主下发的 `product_models` **只有** qoder / zcode 两个键（workbuddy 缺 ——
    ///   宿主那份 workbuddy 清单来自「限制使用的模型」白名单，默认为空 ⇒ 不写）；
    /// - 网关补的静态兜底清单是 2026-09-18 的快照，**没有** `space-bunny`
    ///   （它是上游后加的 WorkBuddy 国服真实模型，网关自己的 `/debug/capability`
    ///   的 `dynamic_cn_ids` 里就列着它）；
    /// - 池里 4 个 workbuddy 号**全部健康**。
    ///
    /// 旧行为链：兜底未命中 ⇒ 判「不能服务」⇒ 全池进 `scoped` ⇒ `pick_inner` 候选
    /// 为空 ⇒ 全冷却兜底 `pick_earliest_expiry` **只收冷却中的号** ⇒ `None` ⇒ 503。
    /// 新行为：猜测未命中**只降级**，健康号仍能被选中。
    #[test]
    fn bare_model_missing_from_static_fallback_still_reaches_healthy_accounts() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        for i in 0..4 {
            p.add(mk_product(&format!("wb{i}"), "workbuddy"));
        }
        p.add(mk_product("qd1", "qoder"));
        // 宿主清单：只有 qoder / zcode（业主机器上的真实形状）
        let mut pm: HashMap<String, Vec<String>> = HashMap::new();
        pm.insert("qoder".into(), vec!["Qwen3.8-Flash".into()]);
        pm.insert("zcode".into(), vec!["zcode:glm-5".into()]);
        p.set_product_models(&pm);
        // 静态兜底：workbuddy 有清单，但**不含** space-bunny（快照落后于上游）
        let mut fb: HashMap<String, Vec<String>> = HashMap::new();
        fb.insert("workbuddy".into(), vec!["glm-5.3".into()]);
        p.set_product_models_fallback(&fb);

        assert!(
            !p.product_may_serve("workbuddy", "space-bunny"),
            "前提：静态兜底清单确实没有它（这正是现场）"
        );
        assert!(
            !p.product_definitely_lacks_model("workbuddy", "space-bunny"),
            "★ 兜底未命中不是「确定没有」—— 判成确定就会把整池灭绝"
        );

        // ★ 这条断言就是本次修复：旧代码在这里拿到 None（503）。
        for _ in 0..200 {
            let a = p.pick_for_model_any("space-bunny", &[]).unwrap();
            assert_eq!(
                a.product_of(),
                "workbuddy",
                "必须落到 workbuddy 号上，绝不能选 qoder（它确定没有这个模型）"
            );
        }

        // 观测集拿到真值后，「未命中」这一档整个消失（清单自愈）。
        p.observe_product_models("workbuddy", &["space-bunny".to_string()]);
        assert!(
            p.product_may_serve("workbuddy", "space-bunny"),
            "实测真值命中 ⇒ 直接放行，不再需要靠降级"
        );
    }

    /// ★ 观测集（上游实测真值）命中即放行，但**绝不算「声明」** ——
    /// 否则 WorkBuddy 会与声明方 qoder 平等竞争，重演 2026-09-21 的 11102。
    #[test]
    fn observed_live_models_relax_exclusion_but_never_declare() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        p.add(mk_product("wb1", "workbuddy"));
        p.add(mk_product("qd1", "qoder"));
        let mut pm: HashMap<String, Vec<String>> = HashMap::new();
        pm.insert("qoder".into(), vec!["space-bunny".into()]);
        p.set_product_models(&pm);
        let mut fb: HashMap<String, Vec<String>> = HashMap::new();
        fb.insert("workbuddy".into(), vec!["glm-5.3".into()]);
        p.set_product_models_fallback(&fb);
        // 大小写不敏感（上游与客户端写法不一）
        p.observe_product_models("workbuddy", &["SPACE-BUNNY".to_string()]);

        assert!(p.product_may_serve("workbuddy", "space-bunny"), "实测命中 ⇒ 不排除");
        assert!(
            !p.product_declares_model("workbuddy", "space-bunny"),
            "★ 观测集只放宽「不排除」，绝不参与「声明」"
        );
        assert!(p.product_declares_model("qoder", "space-bunny"));
        // 声明方唯一 ⇒ 只能选 qoder：观测集不得把 workbuddy 拉进竞争。
        for _ in 0..100 {
            assert_eq!(p.pick_for_model_any("space-bunny", &[]).unwrap().uid, "qd1");
        }
    }

    /// 观测集只在**已经有兜底清单**的产品上建立，且只增不减。
    #[test]
    fn observe_product_models_needs_an_existing_fallback_list() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        // 没有兜底清单的产品本来就不排除 ⇒ 记观测集没有意义
        p.observe_product_models("workbuddy", &["space-bunny".to_string()]);
        assert!(!p.product_model_known_anywhere("workbuddy", "space-bunny"));

        let mut fb: HashMap<String, Vec<String>> = HashMap::new();
        fb.insert("workbuddy".into(), vec!["glm-5.3".into()]);
        p.set_product_models_fallback(&fb);
        p.observe_product_models("workbuddy", &[" Space-Bunny ".to_string()]);
        assert!(
            p.product_model_known_anywhere("workbuddy", "space-bunny"),
            "trim + 小写后必须命中"
        );
        assert!(
            p.product_model_known_anywhere("workbuddy", "glm-5.3"),
            "兜底清单里的也算「有依据」"
        );
        assert!(!p.product_model_known_anywhere("workbuddy", "nope"));
    }

    /// ★ 新判据不得削弱 Trae 保护：**宿主清单已知且不含它** ⇒ 仍然「确定没有」。
    ///
    /// 这是与「兜底清单未命中」的分界线：前者可信（Trae 保护靠它），后者是猜测
    /// （`space-bunny` 事故靠它降级）。
    #[test]
    fn product_definitely_lacks_model_keeps_host_list_authoritative() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        let mut pm: HashMap<String, Vec<String>> = HashMap::new();
        pm.insert("trae".into(), vec!["glm-5.2".into()]);
        p.set_product_models(&pm);
        assert!(
            p.product_definitely_lacks_model("trae", "deepseek-v4.1-flash"),
            "宿主清单已知且没有它 ⇒ 确定没有（Trae 保护）"
        );
        assert!(!p.product_definitely_lacks_model("trae", "glm-5.2"));

        // 宿主清单**未知** + 兜底清单未命中 ⇒ 只是猜测 ⇒ 不是「确定」
        let mut fb: HashMap<String, Vec<String>> = HashMap::new();
        fb.insert("workbuddy".into(), vec!["glm-5.3".into()]);
        p.set_product_models_fallback(&fb);
        assert!(!p.product_definitely_lacks_model("workbuddy", "space-bunny"));
        assert!(!p.product_definitely_lacks_model("workbuddy", "nope"));
        // 宿主清单已知且没有它 ⇒ 又回到「确定」
        let mut pm2: HashMap<String, Vec<String>> = HashMap::new();
        pm2.insert("workbuddy".into(), vec!["glm-5.3".into()]);
        p.set_product_models(&pm2);
        assert!(p.product_definitely_lacks_model("workbuddy", "nope"));
    }

    /// ★★ 裸名请求的**所有**入口都要过产品确定性排除 —— 区域偏好/严格区域路径
    ///（带图片的请求走这两条）此前会绕过它。
    ///
    /// 业主现场：`space-bunny` 落到 Trae 号上，日志逐字
    /// 「上游 Trae 国内版没有模型 `space-bunny`（清单 0 个），请求侧错误」。
    #[test]
    fn bare_region_pick_never_crosses_to_a_product_that_lacks_the_model() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        // 唯一能服务 space-bunny 的 WorkBuddy 号在国际版；国服只剩 Trae 号。
        let mut wb = mk_product("wb1", "workbuddy");
        wb.domain = "https://www.workbuddy.ai".into();
        p.add(wb);
        p.add(mk_product("tr1", "trae"));
        // 宿主清单给了 trae（已知且不含 space-bunny）⇒ 确定排除
        let mut pm: HashMap<String, Vec<String>> = HashMap::new();
        pm.insert("trae".into(), crate::server::static_trae_model_ids());
        p.set_product_models(&pm);
        let mut fb: HashMap<String, Vec<String>> = HashMap::new();
        fb.insert("workbuddy".into(), vec!["glm-5.3".into()]);
        p.set_product_models_fallback(&fb);

        assert!(
            p.pick_for_model_region("space-bunny", &[], Region::CN, true).is_none(),
            "国服只剩 Trae 号 ⇒ 必须报「缺国服账号」，绝不能打到 Trae 上"
        );
        assert!(
            p.pick_for_model_region("space-bunny", &[], Region::CN, false)
                .map(|a| a.product_of().to_string())
                .as_deref()
                == Some("workbuddy"),
            "偏好路径放开到全池时也只能落到 workbuddy"
        );
    }

    /// 造一个**指定区域**的账号（区域由 `domain` 推：`.ai` ⇒ 国际版，其余 ⇒ 国服）。
    fn mk_region(uid: &str, product: &str, region: Region) -> Auth {
        let domain = match region {
            Region::Intl => "https://www.workbuddy.ai",
            _ => "https://copilot.tencent.com",
        };
        Auth {
            uid: uid.into(),
            access_token: format!("at-{uid}"),
            product: product.into(),
            domain: domain.into(),
            ..Default::default()
        }
    }

    /// ★★★ 真实事故回归（所有者 2026-10-06 真机）：裸名 `space-bunny` 选中**错区域**的号。
    ///
    /// # 现场（隔离实例 :17864 实测）
    ///
    /// 上一轮修复消除了 503，但症状变成「一半概率打到错区」：
    ///
    /// ```text
    /// space-bunny              → 400 model_not_in_region（上游原文
    ///                             "model [space-bunny] service info not found"）
    /// 国服:space-bunny          → 200
    /// 国际版:space-bunny        → 400 同上
    /// ```
    ///
    /// 而 `/debug/capability` 显示 `dynamic_cn_ids` **有** `space-bunny`、
    /// `dynamic_intl_ids` **没有** ⇒ 真值早已足够，只是没被用来约束区域。
    ///
    /// 修法：把真值按区记进 `product_model_regions`，裸名（区域不限）时排除
    /// 「确知它不在该区」的账号（`pick_inner` 落点 I）。
    #[test]
    fn bare_model_only_in_cn_never_picks_an_intl_account() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        // 1 个国服号 + 12 个国际版号（业主机器上的真实形状：15 国服 / 12 国际）
        p.add(mk_region("cn1", "workbuddy", Region::CN));
        for i in 0..12 {
            p.add(mk_region(&format!("intl{i}"), "workbuddy", Region::Intl));
        }
        // 静态兜底清单**没有** space-bunny（2026-09-18 快照）—— 这正是缺陷前提。
        let mut fb: HashMap<String, Vec<String>> = HashMap::new();
        fb.insert("workbuddy".into(), vec!["glm-5.3".into()]);
        p.set_product_models_fallback(&fb);
        // 两区真值都拉到过：国服有 space-bunny，国际版没有。
        p.observe_product_models_in_region(
            "workbuddy",
            Region::CN,
            &["space-bunny".to_string(), "glm-5.3".to_string()],
        );
        p.observe_product_models_in_region(
            "workbuddy",
            Region::Intl,
            // `glm-5.3` 两区都有（业主真值里确实如此）—— 用来钉「两区都见过 ⇒ 不约束」。
            &["default-model".to_string(), "glm-5.3".to_string()],
        );

        assert_eq!(
            p.model_region_hint("workbuddy", "space-bunny"),
            Some(Region::CN),
            "两区真值都在 ⇒ 必须给出「只在国服」的结论"
        );
        assert_eq!(
            p.model_region_hint("workbuddy", "glm-5.3"),
            None,
            "两区都见过 ⇒ 不给结论（强行指定会白丢一半额度）"
        );

        // ★ 这条断言就是本轮修复：旧代码会有一半概率抽到 intl*。
        for _ in 0..300 {
            let a = p.pick_for_model_any("space-bunny", &[]).unwrap();
            assert_eq!(a.uid, "cn1", "只在国服的模型绝不能选中国际版账号");
        }

        // 用户**显式**指定国际版时，以用户为准（软判据不得压过明确指令）：
        // 国服号被区域过滤掉后仍应选出国际版号，而不是报「没有账号」。
        assert!(
            p.pick_for_model_region("space-bunny", &[], Region::Intl, false).is_some(),
            "显式区域前缀优先于真值推断"
        );
    }

    /// ★ 真值只拉到一区时**不得**收紧区域 —— 否则会把另一区的账号整批误杀。
    ///
    /// 现场对应：国际版那次拉取失败（无账号 / 上游抖动 / 退避窗口），
    /// 「国际版清单里没有 space-bunny」是**未知**而不是**没有**。
    #[test]
    fn region_hint_requires_both_regions_fetched() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        p.add(mk_region("cn1", "workbuddy", Region::CN));
        p.add(mk_region("intl1", "workbuddy", Region::Intl));
        let mut fb: HashMap<String, Vec<String>> = HashMap::new();
        fb.insert("workbuddy".into(), vec!["glm-5.3".into()]);
        p.set_product_models_fallback(&fb);
        // 只拉到国服
        p.observe_product_models_in_region("workbuddy", Region::CN, &["space-bunny".to_string()]);
        assert_eq!(
            p.model_region_hint("workbuddy", "space-bunny"),
            None,
            "只有一区真值 ⇒ 不能下结论"
        );
        // 国际版号仍必须能被选中（这正是「不误杀」）
        let mut saw_intl = false;
        for _ in 0..200 {
            if p.pick_for_model_any("space-bunny", &[]).unwrap().uid == "intl1" {
                saw_intl = true;
                break;
            }
        }
        assert!(saw_intl, "一区真值不足时不得把另一区排除干净");
    }

    /// 区域收紧**只在没有任何目标区候选时才失效**（退回原集合），不得变成「全池不可用」。
    ///
    /// 业主机器上国服号可能整批冷却 —— 那时若直接返回空，用户会看到
    /// 「所有账号不可用」，又被引向账号池。
    #[test]
    fn region_narrowing_falls_back_when_target_region_is_unavailable() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        p.add(mk_region("cn1", "workbuddy", Region::CN));
        p.add(mk_region("intl1", "workbuddy", Region::Intl));
        let mut fb: HashMap<String, Vec<String>> = HashMap::new();
        fb.insert("workbuddy".into(), vec!["glm-5.3".into()]);
        p.set_product_models_fallback(&fb);
        p.observe_product_models_in_region("workbuddy", Region::CN, &["space-bunny".to_string()]);
        p.observe_product_models_in_region("workbuddy", Region::Intl, &["glm-5.3".to_string()]);
        // 国服号禁用 ⇒ 目标区没号可挑
        p.disable("cn1", "session dead");
        let got = p.pick_for_model_any("space-bunny", &[]);
        assert!(
            got.is_some(),
            "目标区没号时宁可退回全池试一次，也不能报「所有账号不可用」"
        );
    }

    /// 粘性路径同样要过区域闸（会话绑定号可能在模型并不存在的那个区）。
    #[test]
    fn sticky_pick_rejects_an_account_in_the_wrong_region() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        p.add(mk_region("cn1", "workbuddy", Region::CN));
        p.add(mk_region("intl1", "workbuddy", Region::Intl));
        let mut fb: HashMap<String, Vec<String>> = HashMap::new();
        fb.insert("workbuddy".into(), vec!["glm-5.3".into()]);
        p.set_product_models_fallback(&fb);
        p.observe_product_models_in_region("workbuddy", Region::CN, &["space-bunny".to_string()]);
        p.observe_product_models_in_region("workbuddy", Region::Intl, &["glm-5.3".to_string()]);

        assert!(
            p.pick_by_uid_for_model_product_region("intl1", "space-bunny", Region::Any, "")
                .is_none(),
            "绑定在国际版号上的会话，遇到只在国服的模型必须解绑重选"
        );
        assert!(
            p.pick_by_uid_for_model_product_region("cn1", "space-bunny", Region::Any, "")
                .is_some(),
            "区域相符时粘性照常命中"
        );
    }

    /// ★★★ 2026-10-07 业主现场回归：裸名 `space-bunny` 被抽中 Trae 账号。
    ///
    /// 现场：Rust 1.0.32 网关，宿主清单只有 qoder / zcode，workbuddy 与 trae 都靠
    /// 网关自补的兜底清单。`space-bunny` 两份兜底清单都没有 ⇒ 27 个 WorkBuddy 账号
    /// 与 1 个 Trae 账号一起进 `guess_excluded`（最后一档）⇒ 抽签抽到 Trae
    /// ⇒ 上游回 4001 ⇒ 业主看到
    /// `400 model_not_in_upstream`（「两条产品线的 function 池都拒绝了 space-bunny」）。
    ///
    /// 修法：Trae 的兜底清单派生自客户端别名表（上游 config_name 解析的唯一真相）
    /// ⇒ 标记**权威** ⇒ 未命中即「确定没有」⇒ 进 `scoped`（硬排除）。
    /// WorkBuddy **不**标记 ⇒ 它的「未命中」仍然只降级（否则退回 2026-10-06 的 503）。
    #[test]
    fn authoritative_fallback_keeps_bare_model_off_that_product() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        p.add(mk_product("wb1", "workbuddy"));
        p.add(mk_product("tr1", "trae"));
        // 宿主清单：只有 qoder / zcode（业主机器上的真实形状）
        let mut pm: HashMap<String, Vec<String>> = HashMap::new();
        pm.insert("qoder".into(), vec!["Qwen3.8-Flash".into()]);
        pm.insert("zcode".into(), vec!["zcode:glm-5".into()]);
        p.set_product_models(&pm);
        // 兜底清单：workbuddy 是快照（不权威）、trae 是别名表（权威）
        let mut fb: HashMap<String, Vec<String>> = HashMap::new();
        fb.insert("workbuddy".into(), vec!["deepseek-v4.1-flash".into()]);
        fb.insert("trae".into(), vec!["glm-5.2".into()]);
        p.set_product_models_fallback(&fb);
        p.set_product_fallback_authoritative(&["trae".to_string()]);

        // Trae 未命中 ⇒ **确定**没有 ⇒ 硬排除（不再进最后一档抽签）
        assert!(
            p.product_definitely_lacks_model("trae", "space-bunny"),
            "权威兜底清单未命中 = 确定没有（Trae 保护）"
        );
        // Trae 自己有的模型照旧放行，不能因噎废食
        assert!(!p.product_definitely_lacks_model("trae", "glm-5.2"));
        // WorkBuddy 是快照 ⇒ 未命中只是猜测 ⇒ **不**算确定（否则退回 503）
        assert!(
            !p.product_definitely_lacks_model("workbuddy", "space-bunny"),
            "非权威兜底清单未命中不得当成「确定没有」"
        );

        // 裸名抽 200 次，一次都不许落到 Trae 上
        for _ in 0..200 {
            let a = p.pick_for_model_any("space-bunny", &[]).unwrap();
            assert_eq!(
                a.uid, "wb1",
                "裸名 space-bunny 只能落到 WorkBuddy（Trae 上游解析不了这个名字）"
            );
        }
    }

    /// 权威兜底清单**空**时不得当权威 —— 与 `product_may_serve` 里 `fb` 的非空
    /// 判据保持一致：「网关没有更多信息」不能当成「已知没有」。
    #[test]
    fn authoritative_fallback_needs_a_non_empty_list() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        p.add(mk_product("tr1", "trae"));
        p.set_product_fallback_authoritative(&["trae".to_string()]);
        // 没有任何兜底清单（哪怕标了权威）⇒ 真不知道 ⇒ 不排除
        assert!(
            !p.product_definitely_lacks_model("trae", "space-bunny"),
            "权威但空的兜底清单等于「没有信息」，不得排除账号"
        );
        assert!(p.pick_for_model_any("space-bunny", &[]).is_some());
    }

    /// 粘性路径也要过权威闸：会话此前绑过 Trae 号，再发裸名 `space-bunny` 必须解绑。
    ///
    /// 业主日志里 `gpt-6-astra` 与 `space-bunny` **先后落到同一个 Trae 账号**
    /// （`trae-2593120869167228`），正是这条路径。
    #[test]
    fn sticky_pick_rejects_an_account_from_a_product_with_an_authoritative_list() {
        let mut p = Pool::new(String::new());
        p.set_multi_product(true, 0.3);
        p.add(mk_product("wb1", "workbuddy"));
        p.add(mk_product("tr1", "trae"));
        let mut fb: HashMap<String, Vec<String>> = HashMap::new();
        fb.insert("workbuddy".into(), vec!["deepseek-v4.1-flash".into()]);
        fb.insert("trae".into(), vec!["glm-5.2".into()]);
        p.set_product_models_fallback(&fb);
        p.set_product_fallback_authoritative(&["trae".to_string()]);

        assert!(
            p.pick_by_uid_for_model_product_region("tr1", "space-bunny", Region::Any, "")
                .is_none(),
            "绑在 Trae 号上的会话遇到 Trae 解析不了的模型必须解绑重选"
        );
        assert!(
            p.pick_by_uid_for_model_product_region("tr1", "glm-5.2", Region::Any, "")
                .is_some(),
            "Trae 真有的模型，粘性照常命中"
        );
        // 裸名主路径同样选不到 Trae
        for _ in 0..100 {
            let a = p
                .pick_for_model_product_region("space-bunny", &[], Region::Any, "")
                .unwrap();
            assert_eq!(a.uid, "wb1");
        }
    }
}
