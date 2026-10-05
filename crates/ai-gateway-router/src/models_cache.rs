//! 区域模型清单缓存（对应 Go `internal/server/capability.go` 的 `regionModelCache`
//! 与 `fetchModelsForRegion` / `probeAccountsInRegion`）。
//!
//! # 为什么必须是「按区域」的缓存
//!
//! WorkBuddy 的模型清单接口是 `/v3/config`，**同一个账号只能看到自己那个区域的
//! 清单**。因此「国服有哪些模型」与「国际版有哪些模型」是两个独立的事实，
//! 各有各的成功时间与失败退避。把它们合成一份缓存会产生一个很难察觉的错误：
//! 国际版拉不到时把国服的清单当成国际版的真值，于是「国际版也支持 `hy4-preview`」
//! —— 客户端选中后必然 11102。
//!
//! # 三层时间语义（本模块最容易改错的地方）
//!
//! ```text
//! 成功 TTL   = 1h        命中则直接复用，**不**看失败退避
//! 失败退避   = 15s → 30s → 60s → 120s → 240s → 300s（上限）
//! 无账号     = 立刻记一次失败（同样进退避）
//! ```
//!
//! 三个刻意的取舍：
//!
//! 1. **成功即整体替换**：新记录里 `fails = 0`、`last_err = None`。若不清零，
//!    偶发抖动累积起来会把退避顶到 5 分钟上限，表现为「上游早就恢复了，界面还
//!    说拉不到」。Go `capability.go:261-264` 逐字记的就是这个。
//! 2. **失败退避只挡拉取，不挡「已知真值」**：`cached()` 只认成功记录，
//!    `in_backoff()` 只认失败记录，两者互不干扰。
//! 3. **绝不喂熔断器**：`/v3/config` 是能力探测接口，它失败**不代表该账号不能聊天**。
//!    历史上 Go 侧在这里调 `NoteError`，导致国际版账号（恰好占据最早到期档位、
//!    且旧端点在国际版恒 500）被反复记失败直到熔断 —— 于是账号「没做错任何事」
//!    却被停掉。本模块**只**返回错误文案，不碰账号状态。
//!
//! # 为什么用 `ProbeUIDs` 而不是 `AvailableUIDs`
//!
//! 拉 `/v3/config` 是只读探测：不产生流量、不消耗积分。因此被用户标记
//! 「不接流量」（`no_route`）的账号同样可用 —— 而且它们往往是**唯一**能提供某个
//! 区域真值的账号。Go 侧实测踩过：改用 `AvailableUIDs` 后，被停用的国际版账号
//! 拿不到真值，于是 `deepseek-v4.1-flash` 被误标成「仅国服存在」（两区其实都有）。
//!
//! # 为什么还要按产品过滤
//!
//! 池是**混装**的：Qoder / ZCode 账号与 WorkBuddy 账号同处一个池。它们的
//! `product_of()` 若不是 workbuddy 就不能拿来打 `/v3/config`（那是别人的上游）。
//! Go `capability.go:272-295` 逐字记了一次真实事故：Qoder 账号的 `domain` 是
//! `qoder.com.cn` 且没有 `product` 字段 ⇒ 被归一成 workbuddy ⇒ `region()` 判成
//! 国服 ⇒ 又因为 uid 字典序靠前被选中国服探测 ⇒ 拿 Qoder token 打 WorkBuddy
//! `/v3/config` ⇒ 空清单 ⇒ **整个国服被判「没拉到」**。
//!
//! 结论（逐字）：「**两个错误叠加才造成这个结果**，故两处都要修：筛选（只挑本
//! 产品）+ 重试（一个坏账号不该让整个区域变未知）」。

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use crate::auth::{Auth, Region, PRODUCT_WORKBUDDY};
use crate::upstream::ModelInfo;

/// 成功清单的存活时间（Go `dynamicModelsTTL`）。
pub const DYNAMIC_MODELS_TTL: Duration = Duration::from_secs(3600);

/// 一个区域最多试几个账号（Go `maxProbeAccounts`）。
///
/// 为什么是 3 而不是 1：单个账号可能因为凭证/权限问题拿到空清单，而**一个坏账号
/// 不该让整个区域变成「未知」**。为什么不是「全部」：上游故障时挨个试只会把
/// 请求延迟放大到分钟级，而它们**必然**给出同一个结果。
pub const MAX_PROBE_ACCOUNTS: usize = 3;

/// 首次失败后的退避时长（Go `modelsRetryAfter` 的 `first`）。
const RETRY_FIRST: Duration = Duration::from_secs(15);

/// 退避上限（Go `modelsRetryAfter` 的 `cap` = 5min）。
const RETRY_CAP: Duration = Duration::from_secs(300);

/// 该区域**没有可用账号**时的失败文案（Go 逐字）。
pub const NO_ACCOUNT_MESSAGE: &str =
    "该区域没有可用账号（池里没有本产品的账号，或全在冷却/禁用）";

/// 第 `fails` 次失败后应等多久才重试（Go `modelsRetryAfter`，逐字对齐）。
///
/// ```text
/// fails  0/1 → 15s    2 → 30s    3 → 60s    4 → 120s    5 → 240s    6+ → 300s
/// ```
///
/// ★ `fails == 0` 与 `fails == 1` 同值（Go 的循环从 `i = 1` 起、条件是 `i < fails`），
/// 这不是笔误：首次失败时计数从 0 变 1，两者都应是「最短退避」。
pub fn retry_after(fails: u32) -> Duration {
    let mut d = RETRY_FIRST;
    let mut i = 1u32;
    while i < fails {
        d *= 2;
        if d >= RETRY_CAP {
            return RETRY_CAP;
        }
        i += 1;
    }
    d
}

/// UID 的短写（Go `shortUID`）：≤8 位原样，否则取前 8 位。
///
/// 用途只有一个：把「是哪个账号失败了」写进给用户看的错误文案。完整 uid 太长会
/// 把文案挤爆，而前 8 位在同一份凭证目录里足以区分账号。
pub fn short_uid(uid: &str) -> String {
    if uid.len() <= 8 {
        return uid.to_string();
    }
    // 按**字符**截断而不是字节：uid 理论上可能含非 ASCII，按字节切会 panic
    // （Go 的 `uid[:8]` 按字节切，遇到非 ASCII 会切出乱码但不 panic）。
    uid.chars().take(8).collect()
}

/// 某区域的缓存记录（Go `regionModels`）。
#[derive(Debug, Clone, Default)]
struct RegionModels {
    /// 成功拉到的清单；空 = 从没成功过。
    infos: Vec<ModelInfo>,
    /// 上次成功的时间。
    fetched: Option<SystemTime>,
    /// 上次失败的时间。
    last_err: Option<SystemTime>,
    /// 上次失败的文案（给用户看的，含是哪个账号失败的）。
    last_err_msg: String,
    /// 连续失败次数（成功即清零）。
    fails: u32,
}

/// 某区域的缓存观测快照（`/v1/models/regions` 的 `fetched_at` / `last_error` /
/// `last_error_message` 三个字段的唯一数据源）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RegionMeta {
    /// 上次成功时间；`None` = 从没成功过（JSON 里是 `null`，不是空串）。
    pub fetched: Option<SystemTime>,
    /// 上次失败时间；`None` = 从没失败过。
    pub last_error: Option<SystemTime>,
    /// 上次失败的文案；空串 = 没有失败记录。
    pub last_error_message: String,
    /// 连续失败次数（供调试观察退避档位）。
    pub fails: u32,
}

/// 按区域分桶的模型清单缓存。
///
/// # 为什么不是 `static`/全局
///
/// Go 侧 `regionModelCache` 是**包级全局**（`capability.go:88-92`），代价是单测
/// 之间互相污染 —— 于是 Go 不得不额外提供 `resetModelsCache()` /
/// `ageModelsCacheFailure(d)` 两个**只为测试存在**的入口。
///
/// Rust 侧放进 [`crate::server::AppState`]：进程内只有一个实例（行为等价），
/// 而每个单测各自持有一份，不需要任何测试专用后门。
#[derive(Debug, Default)]
pub struct ModelsCache {
    by_region: std::sync::Mutex<HashMap<Region, RegionModels>>,
}

impl ModelsCache {
    /// 空缓存。
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Region, RegionModels>> {
        match self.by_region.lock() {
            Ok(g) => g,
            // 锁中毒说明另一个线程在持锁时 panic。缓存是**纯观测数据**，
            // 恢复它不会掩盖任何业务状态，直接取回内部值继续。
            Err(p) => p.into_inner(),
        }
    }

    /// 该区域是否有**未过期的成功清单**；有则返回克隆。
    pub fn cached(&self, region: Region, now: SystemTime) -> Option<Vec<ModelInfo>> {
        let g = self.lock();
        let rm = g.get(&region)?;
        if rm.infos.is_empty() {
            return None;
        }
        let fetched = rm.fetched?;
        if now.duration_since(fetched).unwrap_or_default() < DYNAMIC_MODELS_TTL {
            return Some(rm.infos.clone());
        }
        None
    }

    /// 该区域是否正处于**失败退避**窗口内（在窗口内不该再发起拉取）。
    ///
    /// ★ 只看失败时间，不看成功时间：一份过期但有效的清单不该因为「刚失败过一次」
    /// 而被丢弃 —— 那会让界面在退避期间从「有清单」退回「未知」，比展示一份
    /// 稍旧的清单更糟（用户会以为模型消失了）。
    pub fn in_backoff(&self, region: Region, now: SystemTime) -> bool {
        let g = self.lock();
        let Some(rm) = g.get(&region) else {
            return false;
        };
        let Some(last) = rm.last_err else {
            return false;
        };
        now.duration_since(last).unwrap_or_default() < retry_after(rm.fails)
    }

    /// 该区域**是否拿到过真值**（对应 Go `capabilityIndex.knownRegion`）。
    ///
    /// ★ 判据是「`infos` 非空」而**不是**「TTL 内」：Go 的 `knownRegion` 由
    /// `buildCapabilityIndex` 在 `len(infos) > 0` 时置位，而 `fetchModelsForRegion`
    /// 会返回**过期的**缓存清单。把「过期」当成「未知」会让界面在每小时的头几秒
    /// 反复闪回「仅国服存在」的错误结论。
    pub fn has_truth(&self, region: Region) -> bool {
        self.lock()
            .get(&region)
            .map(|rm| !rm.infos.is_empty())
            .unwrap_or(false)
    }

    /// 记录一次成功。★ **整体替换** ⇒ 清零 `fails` 与 `last_err`。
    pub fn record_success(&self, region: Region, infos: Vec<ModelInfo>, now: SystemTime) {
        let mut g = self.lock();
        g.insert(
            region,
            RegionModels {
                infos,
                fetched: Some(now),
                last_err: None,
                last_err_msg: String::new(),
                fails: 0,
            },
        );
    }

    /// 记录一次失败（**不动**已有清单，只推进退避）。
    ///
    /// 为什么不保留旧清单却记失败：`cached()` 与 `in_backoff()` 是两条独立判据，
    /// 调用方先查 `cached()` 再查 `in_backoff()`，因此「有旧清单 + 有失败记录」
    /// 会先命中缓存 —— 正是想要的语义（清单还能用，退避只管下次拉取）。
    pub fn record_failure(&self, region: Region, msg: String, now: SystemTime) {
        let mut g = self.lock();
        let rm = g.entry(region).or_default();
        rm.last_err = Some(now);
        rm.last_err_msg = msg;
        rm.fails = rm.fails.saturating_add(1);
    }

    /// 观测快照（`/v1/models/regions` 用）。
    pub fn meta(&self, region: Region) -> RegionMeta {
        let g = self.lock();
        match g.get(&region) {
            Some(rm) => RegionMeta {
                fetched: rm.fetched,
                last_error: rm.last_err,
                last_error_message: rm.last_err_msg.clone(),
                fails: rm.fails,
            },
            None => RegionMeta::default(),
        }
    }

    /// 清空全部区域的缓存（`/v1/models?refresh=1` 用，对应 Go `resetModelsCache`）。
    ///
    /// ★ Go `handler.go:801-825` 逐字记了一次真实缺陷：`resetModelsCache()` 当时
    /// **在生产路径零调用**，于是用户点「刷新」时，`modelsFetchFailCooldown`
    /// 还在 5 分钟窗口内，函数静默 `return nil` —— 界面上毫无变化，而按钮的
    /// 文案正在让他做一件代码不支持的事。故本函数的调用点必须真实存在
    /// （见 `server::models` 对 `?refresh=1` 的处理）。
    pub fn reset(&self) {
        self.lock().clear();
    }

    /// 两区缓存里的模型总数（诊断用，对应 Go `cachedModelCount`）。
    pub fn cached_model_count(&self) -> usize {
        self.lock().values().map(|rm| rm.infos.len()).sum()
    }

    /// **同步**取两区真值快照（不发起任何网络请求），形状与
    /// [`crate::server::region_snapshot`] 的返回值一致，可直接喂给
    /// [`crate::capability::CapabilityIndex::build`]。
    ///
    /// # 为什么需要它
    ///
    /// 用量记账（`record_usage`）发生在**请求路径**上，而它是同步函数：那里既不能
    /// `await`，也**不该**为了记一条倍率再去打上游 —— 用户等的是回答，不是我们的
    /// 账单数字。故这里只读缓存。
    ///
    /// # 与 Go 的差异（如实记录）
    ///
    /// Go `fillBilling` → `buildCapabilityIndex()` 内部**会**发起拉取（必要时）。
    /// 差异只在「缓存恰好过期且退避窗口外」这一种情况下显现：Go 会拿到新清单，
    /// 这里拿到的是**过期但有效**的清单。取不到时一律保持 `None`
    /// （= 未声明），**绝不编造数字** —— 这与 Go 在 `idx == nil` 或该区域没有该
    /// 模型时同样保持 nil 是同一口径。
    ///
    /// ★ 刻意**不看 TTL**（用 `infos` 非空判据而非 [`ModelsCache::cached`]）：
    /// 一份过期的倍率仍然远好过没有倍率 —— 倍率是**展示与统计**用的，过期一小时
    /// 的 `x0.03` 不会让用户做出错误决定，而 `null` 会让整条账单显示成「未声明」。
    pub fn snapshot(&self) -> HashMap<Region, Option<Vec<ModelInfo>>> {
        let g = self.lock();
        let mut out = HashMap::new();
        for region in [Region::CN, Region::Intl] {
            let infos = g
                .get(&region)
                .map(|rm| rm.infos.clone())
                .unwrap_or_default();
            out.insert(region, if infos.is_empty() { None } else { Some(infos) });
        }
        out
    }
}

/// 挑出可用于探测某区域清单的账号（Go `probeAccountsInRegion`，逐字对齐）。
///
/// 规则三条，缺一不可：
///
/// 1. 只挑 `product_of() == workbuddy` 的账号（见模块文档里的 Qoder 事故）；
/// 2. 用 [`crate::pool::Pool::probe_uids`]（含 `no_route`，排除 `disabled`）；
/// 3. **国服不跨区回退**：`Region::CN` 只返回国服账号。国际版账号打国服端点会
///    拿到 11102 而不是清单，用它当「国服真值」是错的。
///
/// `Region::Any` 的语义是「随便哪个区都行，优先国服」：优先 `in_region`，
/// 空则回落到国际版。
pub fn probe_accounts_in_region(pool: &crate::pool::Pool, region: Region) -> Vec<Auth> {
    let mut in_region: Vec<Auth> = Vec::new();
    let mut any_intl: Vec<Auth> = Vec::new();

    for uid in pool.probe_uids() {
        let Some(a) = pool.auth_by_uid(&uid) else {
            continue;
        };
        if a.product_of() != PRODUCT_WORKBUDDY {
            continue;
        }
        if a.region() == Region::Intl {
            any_intl.push(a);
            continue;
        }
        in_region.push(a);
    }

    match region {
        Region::Any => {
            if !in_region.is_empty() {
                return in_region;
            }
            any_intl
        }
        Region::Intl => any_intl,
        Region::CN => in_region,
    }
}

/// 该区域是否**有可用探测账号**（对应 Go `probeAccountInRegion(region) != nil`）。
///
/// 只用于二值判定（`GapCause::MissingAccounts` vs `FetchFailed`），故不返回账号。
pub fn has_probe_account(pool: &crate::pool::Pool, region: Region) -> bool {
    !probe_accounts_in_region(pool, region).is_empty()
}

/// 拉取某区域的模型清单（Go `fetchModelsForRegion`，逐字对齐）。
///
/// 返回**空**表示「这次拿不到」，调用方据此回落静态兜底表并记缺口原因
/// （[`GapCause`](crate::capability::GapCause)）。命中缓存或处于退避窗口时
/// **不发起任何网络请求**。
///
/// `probe` 由调用方在**短暂持池锁**时用 [`probe_accounts_in_region`] 取好
/// （`std::sync::MutexGuard` 不能跨 `await`，而且探测期间不该占着池锁 ——
/// Go 侧同样在 `:187` 先解锁再发起请求）。
///
/// # 为什么逐个账号重试而不是只试一个
///
/// 所有者现场（2026-09-20）：池里混进一个 Qoder 账号，它恰好排在最前 ⇒
/// 空清单 ⇒ **整个国服被判成「没拉到」**，而他**有 14 个正常国服账号**。
/// 逐字结论：「一个坏账号不该让整个区域变未知」—— 这也是对凭证目录里存在
/// 异常账号（用户手动放的、旧版本残留的）的**防御**：筛选能挡掉已知的混入，
/// 重试能兜住未知的坏账号。
///
/// # 绝不喂熔断器
///
/// `/v3/config` 是能力探测接口，它失败**不代表该账号不能聊天**。历史上 Go
/// 侧在这里调 `NoteError`，导致国际版账号（恰好占据最早到期档位、且旧端点
/// 在国际版恒 500）被反复记失败直到熔断。本函数**只**写缓存里的错误文案。
pub async fn fetch_models_for_region(
    cache: &ModelsCache,
    client: &crate::upstream::Client,
    probe: &[Auth],
    region: Region,
) -> Vec<ModelInfo> {
    // ① 未过期的成功清单直接复用；② 退避窗口内连试都不试。
    //
    // 顺序不能反：先看缓存，一份还能用的清单不该因为「刚刚失败过一次」而被丢弃。
    let now = SystemTime::now();
    if let Some(hit) = cache.cached(region, now) {
        return hit;
    }
    if cache.in_backoff(region, now) {
        return Vec::new();
    }

    // ③ 没有可用账号：这一支**没有**走下面的 err 分支，若不记，
    //    `/v1/models/regions` 会显示「失败但没原因」，排查时是盲区。
    if probe.is_empty() {
        cache.record_failure(region, NO_ACCOUNT_MESSAGE.to_string(), SystemTime::now());
        return Vec::new();
    }

    // ④ 试到第一个成功为止（上限 MAX_PROBE_ACCOUNTS，避免账号很多时打太多上游）。
    let mut infos: Vec<ModelInfo> = Vec::new();
    let mut last_failure = String::new();
    let mut tried = 0usize;
    for acct in probe {
        if tried >= MAX_PROBE_ACCOUNTS {
            break;
        }
        tried += 1;
        match client.fetch_models(acct).await {
            Ok(got) if !got.is_empty() => {
                infos = got;
                last_failure.clear();
                break;
            }
            Ok(_) => {
                // 200 但清单为空：**响应有效**，只是这个账号看不到模型。
                // 与「请求失败」分开记 —— 前者要换账号，后者要查网络。
                last_failure = format!(
                    "账号 {} → 上游返回 0 个模型（响应有效但清单为空）",
                    short_uid(&acct.uid)
                );
            }
            Err(e) => {
                last_failure = format!("账号 {} → {e}", short_uid(&acct.uid));
            }
        }
    }

    if infos.is_empty() {
        // 全部试过都失败：记下**为什么** + 试了几个。
        //
        // 只记一个会误导成「这个区域只有一个账号」，而实际可能是「试了 3 个都不行」
        // —— 两者的排查动作完全不同（前者去补账号，后者查上游/网络）。
        if tried > 1 {
            last_failure = format!("连续试了 {tried} 个账号都不行；最后一个：{last_failure}");
        }
        cache.record_failure(region, last_failure, SystemTime::now());
        return Vec::new();
    }

    // ⑤ 成功即**清零退避**：下次失败重新从 15 秒开始。
    //
    // 不清零的后果：偶发抖动累积起来会把退避顶到 5 分钟上限，于是「偶尔失败一次」
    // 也要等 5 分钟 —— 又回到「要手动同步才行」。系统应当自己好。
    cache.record_success(region, infos.clone(), SystemTime::now());
    infos
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Auth;

    fn info(id: &str) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            ..Default::default()
        }
    }

    /// 退避档位必须与 Go `modelsRetryAfter` 逐档一致 —— 错一档的表现是
    /// 「上游恢复后界面还在说拉不到」，而用户唯一的办法是重启网关。
    #[test]
    fn retry_backoff_matches_go_ladder() {
        let secs = |f: u32| retry_after(f).as_secs();
        assert_eq!(secs(0), 15, "首次失败");
        assert_eq!(secs(1), 15);
        assert_eq!(secs(2), 30);
        assert_eq!(secs(3), 60);
        assert_eq!(secs(4), 120);
        assert_eq!(secs(5), 240);
        assert_eq!(secs(6), 300, "第 6 次应封顶而不是 480");
        assert_eq!(secs(50), 300, "上限恒定，不溢出");
    }

    #[test]
    fn short_uid_keeps_short_and_truncates_long() {
        assert_eq!(short_uid("abc"), "abc");
        assert_eq!(short_uid("12345678"), "12345678");
        assert_eq!(short_uid("123456789"), "12345678");
        // 非 ASCII 不能 panic（按字符截断）
        let _ = short_uid("账号账号账号账号账号账号");
    }

    #[test]
    fn success_is_cached_within_ttl_only() {
        let c = ModelsCache::new();
        let now = SystemTime::now();
        c.record_success(Region::CN, vec![info("a")], now);

        assert_eq!(c.cached(Region::CN, now).unwrap().len(), 1);
        // TTL 内命中
        assert!(c
            .cached(Region::CN, now + Duration::from_secs(3599))
            .is_some());
        // 刚过 TTL 就不命中（但真值仍在：has_truth 只看有没有清单）
        assert!(c
            .cached(Region::CN, now + Duration::from_secs(3601))
            .is_none());
        assert!(c.has_truth(Region::CN), "过期不等于未知");
    }

    /// 区域必须**独立**：国际版拉不到不能借国服的清单当自己的真值。
    #[test]
    fn regions_are_bucketed_independently() {
        let c = ModelsCache::new();
        let now = SystemTime::now();
        c.record_success(Region::CN, vec![info("cn-only")], now);

        assert!(c.cached(Region::CN, now).is_some());
        assert!(c.cached(Region::Intl, now).is_none(), "两区不能互相兜底");
        assert!(c.has_truth(Region::CN));
        assert!(!c.has_truth(Region::Intl));
    }

    /// 失败退避只挡拉取，**不**让已有清单消失。
    #[test]
    fn failure_does_not_drop_existing_truth() {
        let c = ModelsCache::new();
        let now = SystemTime::now();
        c.record_success(Region::Intl, vec![info("x")], now);
        c.record_failure(Region::Intl, "账号 abcd → 上游 500".into(), now);

        assert!(c.cached(Region::Intl, now).is_some(), "旧清单仍可用");
        assert!(c.has_truth(Region::Intl));
        assert!(c.in_backoff(Region::Intl, now), "退避已生效");
        // 退避到期后不再挡
        assert!(!c.in_backoff(Region::Intl, now + Duration::from_secs(16)));
    }

    /// ★ 成功必须**清零**退避计数 —— 不清零的话偶发抖动会累积到 5 分钟上限。
    #[test]
    fn success_resets_backoff_counter() {
        let c = ModelsCache::new();
        let now = SystemTime::now();
        for i in 0..6 {
            c.record_failure(Region::CN, format!("fail {i}"), now);
        }
        assert_eq!(c.meta(Region::CN).fails, 6);
        assert_eq!(retry_after(c.meta(Region::CN).fails).as_secs(), 300);

        c.record_success(Region::CN, vec![info("a")], now);
        let m = c.meta(Region::CN);
        assert_eq!(m.fails, 0, "成功即清零");
        assert!(m.last_error.is_none(), "成功即清掉失败时间");
        assert_eq!(m.last_error_message, "");
        assert!(!c.in_backoff(Region::CN, now));
    }

    /// `last_error_message` 必须**逐次覆盖**（对应 2026-09-20 缺陷：
    /// 界面只显示了一个时间戳，用户无从知道失败原因）。
    #[test]
    fn last_error_message_is_kept_and_overwritten() {
        let c = ModelsCache::new();
        let now = SystemTime::now();
        c.record_failure(Region::Intl, "账号 abcd1234 → 上游 500".into(), now);
        assert_eq!(
            c.meta(Region::Intl).last_error_message,
            "账号 abcd1234 → 上游 500"
        );

        c.record_failure(Region::Intl, "该区域没有可用账号（池里没有本产品的账号，或全在冷却/禁用）".into(), now);
        assert!(c.meta(Region::Intl).last_error_message.contains("没有可用账号"));
        assert_eq!(c.meta(Region::Intl).fails, 2);
        // 从没成功过 → fetched 为 None（JSON null）
        assert!(c.meta(Region::Intl).fetched.is_none());
    }

    /// 失败记录在**没有**成功记录时同样要建桶：否则 `meta()` 拿不到失败文案，
    /// 界面就只能显示「未知」而说不出为什么。
    #[test]
    fn failure_without_prior_success_still_records() {
        let c = ModelsCache::new();
        let now = SystemTime::now();
        c.record_failure(Region::CN, "x".into(), now);
        let m = c.meta(Region::CN);
        assert_eq!(m.fails, 1);
        assert!(m.last_error.is_some());
        assert!(c.cached(Region::CN, now).is_none());
        assert!(!c.has_truth(Region::CN));
    }

    #[test]
    fn reset_clears_everything() {
        let c = ModelsCache::new();
        let now = SystemTime::now();
        c.record_success(Region::CN, vec![info("a"), info("b")], now);
        c.record_success(Region::Intl, vec![info("c")], now);
        assert_eq!(c.cached_model_count(), 3);

        c.reset();
        assert_eq!(c.cached_model_count(), 0);
        assert!(!c.has_truth(Region::CN));
        assert!(!c.in_backoff(Region::CN, now), "重置后退避也一并清掉");
    }

    /// 空清单**不算**成功：`/v3/config` 返回 200 但 `agents[cli].models` 为空
    /// 是「这个账号看不到清单」，不是「这个区域没有模型」。
    #[test]
    fn empty_success_is_not_truth() {
        let c = ModelsCache::new();
        let now = SystemTime::now();
        c.record_success(Region::CN, Vec::new(), now);
        assert!(c.cached(Region::CN, now).is_none());
        assert!(!c.has_truth(Region::CN));
    }

    // ---- probe_accounts_in_region ----

    fn acct(uid: &str, domain: &str, product: &str) -> Auth {
        Auth {
            uid: uid.into(),
            access_token: "at".into(),
            domain: domain.into(),
            product: product.into(),
            ..Default::default()
        }
    }

    #[test]
    fn probe_skips_other_products() {
        let mut p = crate::pool::Pool::new(String::new());
        p.add(acct("wb-cn", "www.workbuddy.cn", ""));
        // ★ Qoder 账号：domain 是国服域名、product 是 qoder
        p.add(acct("qd", "qoder.com.cn", "qoder"));

        let got = probe_accounts_in_region(&p, Region::CN);
        assert_eq!(got.len(), 1, "只挑本产品");
        assert_eq!(got[0].uid, "wb-cn");
    }

    /// 国服**绝不**跨区回退：拿国际版账号打国服端点只会拿到 11102，
    /// 把它当成「国服真值」会谎报国服缺模型。
    #[test]
    fn cn_never_falls_back_to_intl() {
        let mut p = crate::pool::Pool::new(String::new());
        p.add(acct("wb-intl", "www.workbuddy.ai", ""));

        assert!(probe_accounts_in_region(&p, Region::CN).is_empty());
        assert!(!has_probe_account(&p, Region::CN));
        assert_eq!(probe_accounts_in_region(&p, Region::Intl).len(), 1);
    }

    /// `Region::Any` 优先国服，国服没有才用国际版。
    #[test]
    fn any_prefers_cn_then_intl() {
        let mut p = crate::pool::Pool::new(String::new());
        p.add(acct("wb-intl", "www.workbuddy.ai", ""));
        assert_eq!(probe_accounts_in_region(&p, Region::Any)[0].uid, "wb-intl");

        p.add(acct("wb-cn", "www.workbuddy.cn", ""));
        assert_eq!(probe_accounts_in_region(&p, Region::Any)[0].uid, "wb-cn");
    }

    /// `no_route` 的账号**可以**用来探测（只读、不产生流量），
    /// 而 `disabled` 的不行（连 `/v3/config` 都会失败）。
    #[test]
    fn probe_includes_no_route_but_excludes_disabled() {
        let mut p = crate::pool::Pool::new(String::new());
        let mut a = acct("wb-intl", "www.workbuddy.ai", "");
        a.no_route = true;
        p.add(a);
        assert_eq!(
            probe_accounts_in_region(&p, Region::Intl).len(),
            1,
            "no_route 的账号是某些区域唯一真值来源"
        );

        p.disable("wb-intl", "session dead");
        assert!(probe_accounts_in_region(&p, Region::Intl).is_empty());
    }

    /// 输出顺序稳定（按 uid 排序）：否则「试了哪 3 个账号」在日志里每次都不同，
    /// 故障排查时无法对照。
    #[test]
    fn probe_order_is_stable() {
        let mut p = crate::pool::Pool::new(String::new());
        for uid in ["wb-z", "wb-a", "wb-m"] {
            p.add(acct(uid, "www.workbuddy.cn", ""));
        }
        let got: Vec<String> = probe_accounts_in_region(&p, Region::CN)
            .into_iter()
            .map(|a| a.uid)
            .collect();
        assert_eq!(got, vec!["wb-a", "wb-m", "wb-z"]);
    }

    /// 没有 token 的账号不能用来探测（必然 401，白跑一轮）。
    #[test]
    fn probe_skips_empty_token() {
        let mut p = crate::pool::Pool::new(String::new());
        let mut a = acct("wb-cn", "www.workbuddy.cn", "");
        a.access_token.clear();
        p.add(a);
        assert!(probe_accounts_in_region(&p, Region::CN).is_empty());
    }

    // ---- fetch_models_for_region（只覆盖「不发起网络请求」的三条分支）----

    fn test_client() -> crate::upstream::Client {
        crate::upstream::Client::new(crate::upstream::ClientConfig::default())
            .expect("构造测试客户端")
    }

    /// 缓存命中时**一个上游请求都不该发**（`probe` 传空也照样返回清单）。
    #[tokio::test]
    async fn fetch_returns_cached_without_touching_upstream() {
        let cache = ModelsCache::new();
        let client = test_client();
        cache.record_success(Region::CN, vec![info("glm-5.3")], SystemTime::now());

        let got = fetch_models_for_region(&cache, &client, &[], Region::CN).await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "glm-5.3");
        // ★ 没有账号却命中缓存 ⇒ 不该被记成「没有可用账号」
        assert_eq!(cache.meta(Region::CN).fails, 0);
    }

    /// 退避窗口内不重试：`probe` 里有账号也不该发请求。
    #[tokio::test]
    async fn fetch_respects_backoff_window() {
        let cache = ModelsCache::new();
        let client = test_client();
        cache.record_failure(Region::Intl, "账号 abcd → 上游 500".into(), SystemTime::now());

        let got = fetch_models_for_region(
            &cache,
            &client,
            &[acct("wb-intl", "www.workbuddy.ai", "")],
            Region::Intl,
        )
        .await;
        assert!(got.is_empty());
        // 失败计数**没有**再涨（说明连试都没试）
        assert_eq!(cache.meta(Region::Intl).fails, 1);
        assert_eq!(
            cache.meta(Region::Intl).last_error_message,
            "账号 abcd → 上游 500",
            "退避期间不该覆盖上次的原因"
        );
    }

    /// 无账号分支必须记原因：不记的话 `/v1/models/regions` 会显示
    /// 「失败但没原因」，排查时是盲区。
    #[tokio::test]
    async fn fetch_without_accounts_records_reason() {
        let cache = ModelsCache::new();
        let client = test_client();

        let got = fetch_models_for_region(&cache, &client, &[], Region::Intl).await;
        assert!(got.is_empty());
        let m = cache.meta(Region::Intl);
        assert_eq!(m.fails, 1);
        assert_eq!(m.last_error_message, NO_ACCOUNT_MESSAGE);
        assert!(m.last_error.is_some());
        // 没有成功记录 ⇒ fetched 为 null（界面据此区分「从没拉到」与「拉过但过期」）
        assert!(m.fetched.is_none());
        // 记完之后进入退避，下一轮不会立刻再记一次
        assert!(cache.in_backoff(Region::Intl, SystemTime::now()));
    }
}
