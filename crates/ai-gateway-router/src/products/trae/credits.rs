//! Trae 账号的积分采集（router 侧）：把积分与最近到期日写进账号池。
//!
//! # 为什么需要这个模块
//!
//! server.rs 的 status_to_json 对每个账号**恒**下发 credits，而 soonest_expire_at /
//! expire_day 只在非 0 / 非空时才下发。Trae 是另一条产品线：
//!
//! 1. scheduler 的积分巡检带一条产品闸门
//!    （scheduler/mod.rs:981 if a.product_of() != PRODUCT_WORKBUDDY { continue; }）——
//!    那条出站请求打的是 WorkBuddy 端点，闸门是**故意的**（不判产品会让 Qoder /
//!    ZCode / Trae 账号带着错凭证去打 401）；
//! 2. cred.rs:341 的 TraeCred::to_auth() 把 soonest_expire_at 恒置 0。
//!
//! 两条叠加 ⇒ Trae 账号在 /status 里永远是 credits: 0 且没有到期日。
//!
//! ★ 这不只是显示问题：pool.rs:1982 的 routed_tier_day 用 expiry_day_key() 取最早
//! 到期那一档，pool.rs:2949-2963 的 earliest_expiry_tier 只保留该档账号，
//! pool.rs:2986-2988 在 tiered 时把权重里的 credits 项整个跳过。于是
//! 「expire_day 恒空」= Trae 账号永远进不了最早档 = 只要池里还有一个有到期日的
//! WorkBuddy 账号，Trae 就**永远选不中**（业主看到的「显示在用、实际没在用」的机制面）。
//! 本模块补的就是这条采集链路。
//!
//! # 上游接口（真机 CN 专属）
//!
//! POST {auth_host}/trae/api/v2/pay/ide_user_ent_usage，请求体**恰好**
//! {"require_usage":true}。
//!
//! * ★ 真机 main.js @1890142 的 _fetchEntitlementUsageRaw 里带 req_source: 2|1，
//!   但本网关实测**带它会拿到 HTTP 400 空体**（业主真实账号）——
//!   故只发 require_usage，不复制那个字段。这是与真机**已知且刻意**的差异。
//! * ★ 真机同一行的前置条件是 providerCode === CN ⇒ 国际版（Realm::Sg）
//!   **没有这个端点**。本模块对 sg 账号直接跳过：不发请求、不写池、只记一条日志。
//! * 头表复用 client.rs:381 的 headers_for(cred, profile, false, None)（真实抓包
//!   校准过的 Trae 头 + Authorization），**不**照抄 core 那套 ide_query_headers
//!   （x-market-user-id / x-session-id / x-platform 真机 _buildRequestConfig 里没有）。
//!
//! # 解析口径（照真机 function KHe 逐字，main.js @1863224）
//!
//! * 权益包列表 user_entitlement_pack_list 缺失或为空 ⇒ **不写池**（未知，不是 0）。
//! * 主口径是**逐包求和**：remaining_sum += max(credits_limit - credits_amount, 0)，
//!   只累加 credits_limit > 0 的包。免费额度包**没有** credits_limit 字段
//!   （不是 0），它不该贡献剩余量，也不该被当成「整包没有额度」。
//! * 一个正限额包都没有、也不是无限额 ⇒ 退化用 usage_summary 的
//!   total_amount - consumed_amount（两者都能解析成数字时）；仍拿不到 ⇒ 不写。
//! * 到期日 = 各包 entitlement_base_info.end_time 与顶层 expire_time 里**最小的
//!   正值**（Unix 秒；0 = 「永久」，不是 1970）。
//!
//! # credits_limit == -1（无限额）为什么**不写** credits
//!
//! 真机 KHe 对 -1 的处理是把 remaining 置为 Infinity，语义是「额度无限，别拿它跟
//! 有限额账号比大小」。本仓 pool 的 credits 是 i64，而且**不是纯展示字段**：
//! pool.rs:2987-2988 的选号权重是 w += e.credits as f64 / max_credits as f64 * 10.0。
//! 写 i64::MAX 之类的哨兵值会让这个账号在**每一次选号**里都吃掉最大权重，还会把一个
//! 天文数字显示到 /status。故取舍是：**跳过 credits 写入**（CreditsSample::credits
//! 为 None，池里保留旧值），到期日照写（到期日与额度大小无关），并记一条「无限额」
//! 日志 —— 这样「/status 里 credits 还是旧值」是可解释的，不是静默失败。
//!
//! # 新增的出站请求
//!
//! 每个 Trae 账号**每轮 1 次**（默认 15 分钟一轮，见 DEFAULT_CREDITS_REFRESH_INTERVAL），
//! 只打 ent_usage 这一个端点。回调只在**池里真有 Trae 账号**时才装
//! （new_credits_refresh_runner 返回 None ⇒ 不装、不起 task、一个请求都不发）。
//! 本采集**只读 + 只写** credits / expire_at：不写任何账号文件、不改 token、不发
//! chat / 签到类请求、不改冷却、不解冻、不动选号逻辑（故**不**调
//! pool.reenable_if_credits —— Trae 的硬冷却语义未经核实，由采集去复活账号是越权）。
//!
//! # 与 core trae_credits.rs 的关系
//!
//! core 里有一份 1039 行的同类实现（宿主 HTTP 通路用）。本模块是**照本仓约定**
//! 重写的一份：ai-gateway-router **不能**依赖 ai-gateway-core（core 会 include!
//! 内嵌网关二进制，router→core 的依赖被刻意删掉，见 Cargo.toml:20-40）。
//! 解析口径的差异：core 的 parse_credit_total 以 usage_summary 为主口径，
//! 本模块以真机 JS 的逐包求和为主口径。

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use super::client::{self, Client};
use super::cred::TraeCred;
use super::device::{self, DeviceProfile};
use super::dispatch;
use super::mask_uid;
use super::realm::{self, Realm};
use crate::auth::Auth;
use crate::pool::Pool;
use crate::scheduler::Cancel;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 端点路径。★ v2 与 /pay/ 层都不能少（真机逐字）。
pub const PATH_ENT_USAGE: &str = "/trae/api/v2/pay/ide_user_ent_usage";

/// 请求体。★ **恰好**这一个字段 —— 加 req_source 实测回 400 空体（见模块注释）。
pub const ENT_USAGE_BODY: &str = r#"{"require_usage":true}"#;

/// 单次请求超时。与 client::DEFAULT_TIMEOUT 同值（30s），这里显式写出来是因为
/// 任务口径就是 30s，且本请求不该继承流式那条无总超时的 client。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// 账号之间的间隔，避免瞬间并发打满上游。
///
/// 与 scheduler::CREDIT_REFRESH_GAP（300ms）同量级；本层刻意各自声明一份
/// （与 products/qoder/billing_cycle.rs:67 同一取舍），改一处要看另一处。
pub const CREDITS_REFRESH_GAP: Duration = Duration::from_millis(300);

/// 默认巡检周期：与 scheduler::DEFAULT_CREDIT_REFRESH_INTERVAL 对齐（15 分钟）。
///
/// 同样的理由：到期日决定选号档位，会随消费变化；15 分钟 × 账号数 的请求量相对上游
/// 可忽略，且本巡检不签到、不改账号状态。
pub const DEFAULT_CREDITS_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// 一轮采集的执行体（形状与 scheduler::BillingCycleRunner 相同）。
pub type CreditsRunner = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// 取一次采集结果（解析后的）。抽成注入点是为了让循环体能脱离网络单测。
pub(crate) type CreditsFetcher = Arc<
    dyn Fn(TraeCred) -> Pin<Box<dyn Future<Output = Result<CreditsSample, String>> + Send>>
        + Send
        + Sync,
>;

// ---------------------------------------------------------------------------
// 纯解析（无网络、可单测）
// ---------------------------------------------------------------------------

/// 一次 ide_user_ent_usage 响应里我们关心的东西。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditsSample {
    /// 剩余积分。None = **不写池**（无限额 / 响应里没有任何可用口径）。
    pub credits: Option<i64>,
    /// 最近到期时刻（Unix 秒）。0 = 没有可用到期日
    /// （pool.set_credits_and_expiry 在 <= 0 时**不覆盖**旧值，正好）。
    pub soonest_expire_at: i64,
    /// 至少有一个包是无限额（credits_limit == -1）。
    pub unlimited: bool,
    /// 各包已用量之和（credits_limit != 0 的包才计入，照真机 JS）。仅日志用。
    pub used: i64,
    /// 参与求和的**正限额**包数（日志 / 测试用）。
    pub counted_packs: usize,
}

impl Default for CreditsSample {
    fn default() -> Self {
        Self {
            credits: None,
            soonest_expire_at: 0,
            unlimited: false,
            used: 0,
            counted_packs: 0,
        }
    }
}

/// 权益包列表的键名。真机是 user_entitlement_pack_list，另两个是防御性别名
/// （不同版本的响应外层可能改名；认不出来就当未知，不写池）。
const PACK_LIST_KEYS: &[&str] = &[
    "user_entitlement_pack_list",
    "entitlement_pack_list",
    "pack_list",
];

/// 容错取 f64：数字 / 数字字符串都认，NaN / Inf 不认。
///
/// credits_amount 实测是小数（33.3536），credits_limit 是整数 —— 统一走 f64，
/// 最后再四舍五入回 i64（与真机 JS 的浮点语义一致）。
fn as_f64_tolerant(v: &Value) -> Option<f64> {
    let x = match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    if x.is_finite() {
        Some(x)
    } else {
        None
    }
}

/// 四舍五入成 i64，**不 panic**：非有限值当 0，越界夹到边界。
fn round_i64(x: f64) -> i64 {
    if !x.is_finite() {
        return 0;
    }
    let r = x.round();
    if r <= 0.0 {
        0
    } else if r >= i64::MAX as f64 {
        i64::MAX
    } else {
        r as i64
    }
}

/// entitlement_base_info.quota.credits_limit。
///
/// ★ None（路径缺失）与 Some(0.0) 是**两种不同**的东西：免费额度包根本没有
/// credits_limit 字段（不是 0），而 0 表示「这个包没有额度」。真机 JS 靠
/// undefined !== 0 区分，这里靠 Option。
fn pack_credits_limit(pack: &Value) -> Option<f64> {
    pack.get("entitlement_base_info")?
        .get("quota")?
        .get("credits_limit")
        .and_then(as_f64_tolerant)
}

/// usage.credits_amount。usage 可以是空对象 {} ⇒ 已用按 0。
fn pack_used(pack: &Value) -> Option<f64> {
    pack.get("usage")?
        .get("credits_amount")
        .and_then(as_f64_tolerant)
}

/// 单个包的到期时刻：entitlement_base_info.end_time 与顶层 expire_time 里
/// **最小的正值**（实测两者同值；有的包只有 expire_time）。0 = 永久 ⇒ None。
fn pack_expire(pack: &Value) -> Option<i64> {
    let candidates = [
        pack.pointer("/entitlement_base_info/end_time"),
        pack.get("expire_time"),
    ];
    let mut best: Option<i64> = None;
    for v in candidates.into_iter().flatten() {
        let Some(secs) = as_f64_tolerant(v).map(round_i64) else {
            continue;
        };
        if secs <= 0 {
            continue; // 0 / 负数 = 「永久」或脏数据，不是 1970
        }
        best = Some(match best {
            Some(b) => b.min(secs),
            None => secs,
        });
    }
    best
}

/// 在信封各层里找第一个**非空**的权益包数组。
///
/// 空数组按真机 JS 处理（if (e.length === 0) return;）⇒ 未知 ⇒ 返回 None（不写池）。
fn find_pack_list(resp: &Value) -> Option<&Vec<Value>> {
    for scope in client::unwrap_scopes(resp) {
        for key in PACK_LIST_KEYS {
            if let Some(arr) = scope.get(*key).and_then(Value::as_array) {
                if !arr.is_empty() {
                    return Some(arr);
                }
            }
        }
    }
    None
}

/// 退化口径：usage_summary.total_amount - consumed_amount。
///
/// 只在「没有任何正限额包、也不是无限额」时才用（真机 JS 的退化分支）。
fn summary_remaining(resp: &Value) -> Option<i64> {
    for scope in client::unwrap_scopes(resp) {
        let Some(sum) = scope.get("usage_summary") else {
            continue;
        };
        let Some(total) = sum.get("total_amount").and_then(as_f64_tolerant) else {
            continue;
        };
        let used = sum
            .get("consumed_amount")
            .and_then(as_f64_tolerant)
            .unwrap_or(0.0);
        return Some(round_i64(total - used));
    }
    None
}

/// 解析一次 ide_user_ent_usage 响应。
///
/// 返回 None = **响应里没有可用的权益包列表**（缺失 / 空 / 形状不对）⇒ 调用方不写池。
/// 这与「解析出 0 分」是两回事，混起来会把「上游改字段」变成「账号额度清零」。
pub fn parse_ent_usage(resp: &Value) -> Option<CreditsSample> {
    let packs = find_pack_list(resp)?;

    let mut remaining_sum = 0f64;
    let mut used_sum = 0f64;
    let mut unlimited = false;
    let mut any_positive = false;
    let mut counted_packs = 0usize;
    let mut soonest_expire_at = 0i64;

    for pack in packs {
        let limit = pack_credits_limit(pack);
        let used = pack_used(pack).unwrap_or(0.0);

        match limit {
            // ★ 无限额：真机 JS 把 remaining 置为 Infinity，本模块选择「不写 credits」。
            Some(v) if v == -1.0 => unlimited = true,
            Some(v) if v > 0.0 => {
                any_positive = true;
                counted_packs += 1;
                remaining_sum += (v - used).max(0.0);
            }
            // 0 / 缺失 / 脏值：都不参与求和（免费包没有上限，拿它当 0 是对的）。
            _ => {}
        }

        // 真机 JS：if (c !== 0) { r += l } —— 缺失（undefined）也要计入。
        if limit != Some(0.0) {
            used_sum += used;
        }

        if let Some(t) = pack_expire(pack) {
            soonest_expire_at = if soonest_expire_at == 0 {
                t
            } else {
                soonest_expire_at.min(t)
            };
        }
    }

    let credits = if unlimited {
        None
    } else if any_positive {
        Some(round_i64(remaining_sum))
    } else {
        summary_remaining(resp)
    };

    Some(CreditsSample {
        credits,
        soonest_expire_at,
        unlimited,
        used: round_i64(used_sum),
        counted_packs,
    })
}

// ---------------------------------------------------------------------------
// 网络
// ---------------------------------------------------------------------------

/// 请求体前 n 个字符（错误信息里带上，便于对账上游到底回了什么）。
fn head_chars(raw: &[u8], n: usize) -> String {
    String::from_utf8_lossy(raw).chars().take(n).collect()
}

/// POST 一次 ide_user_ent_usage。base 显式传入（= realm::auth_host(realm)），
/// 这样单测能把它指向本地假上游，**不必**改进程级环境变量。
///
/// 非 2xx ⇒ Err，带上状态码与响应体前 200 字（照 core ide_post 的口径）。
pub(crate) async fn post_ent_usage(
    client: &Client,
    cred: &TraeCred,
    profile: &DeviceProfile,
    base: &str,
) -> Result<Value, String> {
    let url = format!("{}{}", base.trim_end_matches('/'), PATH_ENT_USAGE);
    let headers = client::headers_for(cred, profile, false, None);

    let mut rb = client
        .http
        .post(&url)
        .timeout(REQUEST_TIMEOUT)
        .body(ENT_USAGE_BODY);
    for (name, value) in headers.iter() {
        rb = rb.header(name, value);
    }

    let resp = rb
        .send()
        .await
        .map_err(|e| format!("连接 Trae 上游失败（{e}）"))?;
    let status = resp.status().as_u16();
    let raw = client::read_limited(resp, client::MAX_CONFIG_BODY).await;

    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {}", head_chars(&raw, 200)));
    }
    serde_json::from_slice::<Value>(&raw).map_err(|e| {
        format!(
            "响应不是 JSON: {e}（原始前 200 字: {}）",
            head_chars(&raw, 200)
        )
    })
}

/// 默认取数：解析设备指纹 → POST → 解析。
pub(crate) async fn fetch_sample(client: &Client, cred: &TraeCred) -> Result<CreditsSample, String> {
    // 设备指纹的取法与 dispatch.rs:107-115 一致（从凭证路径反推数据目录）。
    let store_dir = device::store_dir_of_cred_file(&cred.file_path);
    let profile = device::resolve(cred.realm, store_dir.as_deref());
    // ★ 必须用 realm::auth_host（带 TRAE_AUTH_HOST_CN / _SG 环境变量覆盖），不要硬编码域名。
    let base = realm::auth_host(cred.realm);
    let body = post_ent_usage(client, cred, &profile, &base).await?;
    parse_ent_usage(&body).ok_or_else(|| {
        "响应里没有可用的权益包列表（user_entitlement_pack_list 缺失或为空），本轮不写池"
            .to_string()
    })
}

// ---------------------------------------------------------------------------
// 巡检循环
// ---------------------------------------------------------------------------

/// 池里是否有 Trae 账号（中毒锁当「没有」）。
fn pool_has_trae(pool: &Arc<Mutex<Pool>>) -> bool {
    match pool.lock() {
        Ok(p) => p
            .list()
            .iter()
            .any(|st| p.auth_by_uid(&st.uid).map(|a| a.is_trae()).unwrap_or(false)),
        Err(_) => false,
    }
}

/// 跑一轮采集，返回 (处理账号数, 实际写入数)。
///
/// # 三条约定（与 products/qoder/billing_cycle.rs:157-200 同款）
///
/// 1. **严格串行**，账号之间留 CREDITS_REFRESH_GAP；
/// 2. 单账号失败只打日志 continue，**不写**该账号任何状态；
/// 3. 取消立即返回。
///
/// 快照先取、锁**不跨 await** 持有。
pub(crate) async fn refresh_credits_once(
    pool: &Arc<Mutex<Pool>>,
    fetch: &CreditsFetcher,
    cancel: &Cancel,
) -> (usize, usize) {
    let entries: Vec<(String, Option<Auth>, i64)> = match pool.lock() {
        Ok(p) => p
            .list()
            .into_iter()
            .map(|st| {
                let a = p.auth_by_uid(&st.uid);
                (st.uid, a, st.credits)
            })
            .collect(),
        Err(e) => {
            eprintln!("Trae 积分刷新：账号池锁中毒，本轮跳过（{e}）");
            return (0, 0);
        }
    };

    let mut processed = 0usize;
    let mut updated = 0usize;

    for (uid, auth, current_credits) in entries {
        if cancel.is_cancelled() {
            return (processed, updated);
        }
        let Some(a) = auth else { continue };

        // ★ 产品闸门必须是 is_trae()。**不要**写成 a.product_of() != PRODUCT_WORKBUDDY
        //   —— 那会把 Qoder / ZCode 账号一起放进来，拿它们的凭证去打 Trae 端点。
        if !a.is_trae() {
            continue;
        }

        if processed > 0 {
            if !cancel.sleep(CREDITS_REFRESH_GAP).await {
                return (processed, updated);
            }
        }
        processed += 1;

        let Some(cr) = dispatch::cred_of(&a) else {
            println!(
                "trae-credits {}: 凭证不可用于查询（缺 token 或文件读不出），已跳过",
                mask_uid(&uid)
            );
            continue;
        };

        // ★ 端点 CN 专属（真机 providerCode === CN 才发）：国际版直接跳过，不发请求。
        if cr.realm != Realm::Cn {
            println!(
                "trae-credits {}: 国际版（sg）没有这个端点（真机 CN 专属），已跳过",
                mask_uid(&uid)
            );
            continue;
        }

        let sample = match fetch(cr).await {
            Ok(v) => v,
            Err(e) => {
                println!("trae-credits {}: {e}", mask_uid(&uid));
                continue;
            }
        };

        let expire = sample.soonest_expire_at;
        let why = if sample.unlimited {
            "无限额（credits_limit=-1）"
        } else {
            "没有可用的额度口径"
        };
        // 无限额（或没有额度口径）时 credits 透传**快照里的原值**，只把到期日写进去。
        // 透传是安全的：同一个 uid 只有本循环会写 credits（scheduler 的产品闸门把
        // 非 WorkBuddy 全部跳过），且本循环严格串行。
        let value = match sample.credits {
            Some(c) => Some(c),
            None if expire > 0 => Some(current_credits),
            None => None,
        };
        let Some(value) = value else {
            println!(
                "trae-credits {}: {why}且无到期日，本轮无可写字段（跳过）",
                mask_uid(&uid)
            );
            continue;
        };

        match pool.lock() {
            Ok(mut p) => {
                p.set_credits_and_expiry(&uid, value, expire);
                updated += 1;
                if sample.credits.is_none() {
                    println!(
                        "trae-credits {}: {why}（已用 {}）—— 跳过 credits 写入，仅更新到期日",
                        mask_uid(&uid),
                        sample.used
                    );
                }
            }
            Err(e) => eprintln!(
                "Trae 积分刷新：账号池锁中毒，{} 本轮跳过（{e}）",
                mask_uid(&uid)
            ),
        }
    }

    (processed, updated)
}

/// 装回调：池里没有 Trae 账号时返回 None。
///
/// 与 products/qoder/billing_cycle.rs:100-111 同一口径 —— 构造期就要求池里有账号，
/// 代价是热加载新增账号要等重启（Go 侧同款取舍）。
///
/// ★ 这是**新增的出站请求**：每个 Trae 账号每轮 1 次。返回 None 时一个请求都不发。
///
/// 调用方在 bin/gateway.rs 里把它包成一条独立的定时循环（周期
/// [`DEFAULT_CREDITS_REFRESH_INTERVAL`]），**不**复用 scheduler 的积分巡检槽位
///（那条循环带 WorkBuddy 产品闸门，且与 Qoder 的计费周期刷新共用）。
#[must_use]
pub fn new_credits_refresh_runner(pool: Arc<Mutex<Pool>>, cancel: Cancel) -> Option<CreditsRunner> {
    if !pool_has_trae(&pool) {
        return None;
    }
    let mut client = Client::new();
    // 与 TraeUpstream::new() 同款：国际版走宿主代理（proxy_scope.intl），国内直连。
    // 本采集只对 CN 账号发请求，但代理口径与 chat 通路保持一致。
    client.apply_host_proxy();
    let client = Arc::new(client);
    let fetch: CreditsFetcher = Arc::new(move |cred: TraeCred| {
        let client = Arc::clone(&client);
        Box::pin(async move { fetch_sample(&client, &cred).await })
            as Pin<Box<dyn Future<Output = Result<CreditsSample, String>> + Send>>
    });
    new_credits_refresh_runner_with(pool, fetch, cancel)
}

/// 与 new_credits_refresh_runner 同形，但取数可注入（单测用）。
pub(crate) fn new_credits_refresh_runner_with(
    pool: Arc<Mutex<Pool>>,
    fetch: CreditsFetcher,
    cancel: Cancel,
) -> Option<CreditsRunner> {
    // 两个入口同口径：池里没有 Trae 账号就不装回调（测试注入时也一样）。
    if !pool_has_trae(&pool) {
        return None;
    }
    Some(Arc::new(move || {
        let pool = Arc::clone(&pool);
        let fetch = Arc::clone(&fetch);
        let cancel = cancel.clone();
        Box::pin(async move {
            let (processed, updated) = refresh_credits_once(&pool, &fetch, &cancel).await;
            if processed > 0 {
                println!("Trae 积分刷新：处理 {processed} 个账号，{updated} 个有更新");
            }
        }) as Pin<Box<dyn Future<Output = ()> + Send>>
    }))
}

// ---------------------------------------------------------------------------
// 单测
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::mpsc;

    // ── 夹具 ──

    /// 真实 ent_usage 响应的逐包形状（字段名与层级照抄实测值）。
    ///
    /// 三个包分别覆盖三种形态：① 免费包**没有** credits_limit；
    /// ② 小数 credits_amount；③ 用光（remaining = 0）且只有 expire_time。
    fn real_ent_usage() -> Value {
        json!({
            "is_credits_billing": true,
            "usage_summary": {
                "consumed_amount": 133.35,
                "consumption_ratio": 0.1666875,
                "total_amount": 800,
            },
            "user_entitlement_pack_list": [
                {
                    "display_desc": "免费",
                    "expire_time": 1793462399,
                    "usage": {},
                    "entitlement_base_info": {
                        "product_id": 0,
                        "end_time": 1793462399,
                        "quota": {
                            "enable_solo_agent": true,
                            "no_bonus_quota": true,
                            "solo_agent_parallel_limit": 2,
                        },
                    },
                },
                {
                    "display_desc": "每月登录赠送",
                    "group_name": "每月登录积分",
                    "expire_time": 1793462399,
                    "usage": { "credits_amount": 33.3536 },
                    "entitlement_base_info": {
                        "product_id": 221,
                        "end_time": 1793462399,
                        "quota": { "credits_limit": 500, "no_bonus_quota": true },
                    },
                },
                {
                    "display_desc": "签到奖励",
                    "group_name": "每日签到",
                    "expire_time": 1793429317,
                    "usage": { "credits_amount": 100 },
                    "entitlement_base_info": {
                        "product_id": 208,
                        "quota": { "credits_limit": 100 },
                    },
                },
            ],
        })
    }

    fn cred(uid: &str, realm: Realm) -> TraeCred {
        TraeCred {
            uid: uid.to_string(),
            user_id: uid.trim_start_matches("trae-").to_string(),
            nickname: String::new(),
            token: "Cloud-IDE-JWT test-token".to_string(),
            refresh_token: String::new(),
            expires_at: 0,
            realm,
            realm_evidence: realm::RealmEvidence::Explicit,
            region_code: String::new(),
            no_route: false,
            file_path: String::new(),
        }
    }

    fn profile() -> DeviceProfile {
        DeviceProfile {
            machine_id: "machine".to_string(),
            device_id: "device".to_string(),
            device_model: "PC".to_string(),
            cpu: "cpu".to_string(),
            os_name: "Windows".to_string(),
            os_version: "10".to_string(),
            is_solo: false,
        }
    }

    fn trae_auth(uid: &str, file_path: &str) -> Auth {
        Auth {
            uid: uid.to_string(),
            product: crate::auth::PRODUCT_TRAE.to_string(),
            access_token: "Cloud-IDE-JWT test-token".to_string(),
            file_path: file_path.to_string(),
            ..Default::default()
        }
    }

    fn other_auth(uid: &str, product: &str) -> Auth {
        Auth {
            uid: uid.to_string(),
            product: product.to_string(),
            ..Default::default()
        }
    }

    fn pool_with(auths: Vec<Auth>) -> Arc<Mutex<Pool>> {
        let mut p = Pool::new(String::new());
        for a in auths {
            p.add(a);
        }
        Arc::new(Mutex::new(p))
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "wbswg-trae-credits-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&d).expect("建临时目录失败");
        d
    }

    /// 写一份网关侧 Trae 凭证（形状与宿主导出的副本一致）。
    fn write_cred_file(dir: &std::path::Path, uid: &str, edition: &str) -> String {
        let path = dir.join(format!("{uid}.json"));
        let doc = json!({
            "account": {
                "uid": uid,
                "user_id": uid.trim_start_matches("trae-"),
                "_edition": edition,
            },
            "auth": { "token": "Cloud-IDE-JWT test-token" },
            "_edition": edition,
        });
        std::fs::write(&path, doc.to_string()).expect("写凭证失败");
        path.to_string_lossy().to_string()
    }

    fn sample(credits: Option<i64>, expire: i64) -> CreditsSample {
        CreditsSample {
            credits,
            soonest_expire_at: expire,
            ..Default::default()
        }
    }

    /// 假取数：按队列返回预置结果，并记录被调用的 uid。
    fn fake_fetcher(
        results: Vec<Result<CreditsSample, String>>,
    ) -> (CreditsFetcher, Arc<Mutex<Vec<String>>>) {
        let queue = Arc::new(Mutex::new(VecDeque::from(results)));
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_in_closure = Arc::clone(&seen);
        let f: CreditsFetcher = Arc::new(move |cred: TraeCred| {
            seen_in_closure.lock().unwrap().push(cred.uid.clone());
            let next = queue.lock().unwrap().pop_front();
            Box::pin(async move { next.unwrap_or_else(|| Err("测试队列已空".to_string())) })
                as Pin<Box<dyn Future<Output = Result<CreditsSample, String>> + Send>>
        });
        (f, seen)
    }

    fn pool_pair(pool: &Arc<Mutex<Pool>>, uid: &str) -> (i64, i64) {
        let p = pool.lock().unwrap();
        let st = p
            .list()
            .into_iter()
            .find(|s| s.uid == uid)
            .expect("账号不在池里");
        (st.credits, st.soonest_expire_at)
    }

    // ── 解析 ──

    #[test]
    fn parse_real_shape_sums_packs_and_soonest_expire() {
        let s = parse_ent_usage(&real_ent_usage()).expect("真实响应必须能解析");
        // 500 − 33.3536 = 466.6464 ⇒ 467；签到包 100 − 100 = 0；免费包无上限 ⇒ 0。
        assert_eq!(s.credits, Some(467));
        assert!(!s.unlimited);
        assert_eq!(s.counted_packs, 2);
        assert_eq!(s.used, 133, "33.3536 + 100 = 133.3536 ⇒ 133");
        // 三个包的到期时刻里最小的是签到包的 1793429317（另两个是 1793462399）。
        assert_eq!(s.soonest_expire_at, 1793429317);
    }

    #[test]
    fn parse_accepts_envelope_layers() {
        let inner = real_ent_usage();
        for wrapped in [
            json!({ "data": inner.clone() }),
            json!({ "resp": { "result": inner.clone() } }),
        ] {
            let s = parse_ent_usage(&wrapped).expect("信封里的响应也要能解析");
            assert_eq!(s.credits, Some(467));
            assert_eq!(s.soonest_expire_at, 1793429317);
        }
    }

    #[test]
    fn parse_unlimited_skips_credits_but_keeps_expire() {
        let v = json!({
            "user_entitlement_pack_list": [{
                "display_desc": "企业无限额",
                "usage": { "credits_amount": 12345 },
                "entitlement_base_info": {
                    "end_time": 1793462399,
                    "quota": { "credits_limit": -1 },
                },
            }],
        });
        let s = parse_ent_usage(&v).expect("必须能解析");
        assert!(s.unlimited, "credits_limit = -1 必须被识别为无限额");
        assert_eq!(
            s.credits, None,
            "无限额不得写 credits：i64 哨兵值会污染 pool.rs:2987 的选号权重"
        );
        assert_eq!(s.counted_packs, 0);
        assert_eq!(s.used, 12345, "无限额包的已用仍计入 used（照真机 JS）");
        assert_eq!(s.soonest_expire_at, 1793462399, "到期日与额度大小无关，照写");
    }

    #[test]
    fn parse_falls_back_to_usage_summary() {
        // 只有免费包（没有 credits_limit）+ usage_summary ⇒ 走退化口径。
        let v = json!({
            "usage_summary": { "total_amount": 800, "consumed_amount": 133.35 },
            "user_entitlement_pack_list": [{
                "display_desc": "免费",
                "expire_time": 0,
                "usage": {},
                "entitlement_base_info": { "quota": { "no_bonus_quota": true } },
            }],
        });
        let s = parse_ent_usage(&v).expect("必须能解析");
        assert!(!s.unlimited);
        assert_eq!(s.counted_packs, 0);
        assert_eq!(s.credits, Some(667), "800 − 133.35 = 666.65 ⇒ 667");
        assert_eq!(s.soonest_expire_at, 0, "expire_time = 0 是「永久」，不是 1970");
    }

    #[test]
    fn parse_zero_limit_pack_is_neither_positive_nor_used() {
        let v = json!({
            "user_entitlement_pack_list": [
                { "usage": { "credits_amount": 50 },
                  "entitlement_base_info": { "quota": { "credits_limit": 0 } } },
                { "usage": { "credits_amount": 7 },
                  "entitlement_base_info": { "quota": { "credits_limit": -1 } } },
            ],
        });
        let s = parse_ent_usage(&v).expect("必须能解析");
        assert!(s.unlimited);
        assert_eq!(s.credits, None);
        // credits_limit = 0 的包不计入 used（真机 JS 的 c !== 0），-1 的包计入。
        assert_eq!(s.used, 7);
        assert_eq!(s.counted_packs, 0);
    }

    #[test]
    fn parse_empty_or_malformed_never_panics() {
        // 这些都必须「不写池」（None），不能 panic，也不能伪造出 0。
        for v in [
            json!({}),
            json!(null),
            json!([]),
            json!(7),
            json!("nope"),
            json!({ "user_entitlement_pack_list": [] }),
            json!({ "user_entitlement_pack_list": "not-an-array" }),
            json!({ "user_entitlement_pack_list": 7 }),
            json!({ "data": { "user_entitlement_pack_list": [] } }),
            json!({ "code": 1, "msg": "boom" }),
        ] {
            assert!(
                parse_ent_usage(&v).is_none(),
                "没有非空权益包列表时必须返回 None，实际对 {v} 返回了 Some"
            );
        }

        // 列表里有脏元素：结构上可解析，但没有任何可用口径 ⇒ credits = None，不 panic。
        let dirty = json!({
            "user_entitlement_pack_list": [
                null,
                3,
                "str",
                { "entitlement_base_info": null },
                { "usage": "x", "entitlement_base_info": { "quota": { "credits_limit": "abc" } } },
                { "usage": { "credits_amount": "1e400" }, "expire_time": "later" },
            ],
        });
        let s = parse_ent_usage(&dirty).expect("有非空列表就算可解析");
        assert_eq!(s.credits, None);
        assert_eq!(s.soonest_expire_at, 0);
        assert!(!s.unlimited);
    }

    #[test]
    fn parse_soonest_expire_takes_min_positive() {
        let v = json!({
            "user_entitlement_pack_list": [
                { "expire_time": 0,
                  "entitlement_base_info": { "quota": { "credits_limit": 1 } } },
                { "expire_time": -5,
                  "entitlement_base_info": { "end_time": 1793462399, "quota": { "credits_limit": 1 } } },
                { "entitlement_base_info": { "end_time": 1793429317, "quota": { "credits_limit": 1 } } },
            ],
        });
        assert_eq!(parse_ent_usage(&v).unwrap().soonest_expire_at, 1793429317);

        // 全是 0 / 负数 ⇒ 没有可用到期日（pool 侧在 <= 0 时不覆盖旧值）。
        let none = json!({
            "user_entitlement_pack_list": [
                { "expire_time": 0,
                  "entitlement_base_info": { "end_time": 0, "quota": { "credits_limit": 1 } } },
                { "expire_time": -1,
                  "entitlement_base_info": { "quota": { "credits_limit": 1 } } },
            ],
        });
        assert_eq!(parse_ent_usage(&none).unwrap().soonest_expire_at, 0);
    }

    // ── 安装闸门 ──

    #[test]
    fn runner_is_none_without_trae_accounts() {
        let empty = pool_with(Vec::new());
        assert!(new_credits_refresh_runner(empty, Cancel::new()).is_none());

        let others = pool_with(vec![
            other_auth("wb-1", crate::auth::PRODUCT_WORKBUDDY),
            other_auth("qoder-1", crate::auth::PRODUCT_QODER),
            other_auth("zcode-1", crate::auth::PRODUCT_ZCODE),
        ]);
        assert!(
            new_credits_refresh_runner(others, Cancel::new()).is_none(),
            "只有别的产品账号时不得装回调（否则会白白发出站请求）"
        );

        let with_trae = pool_with(vec![trae_auth("trae-1", "")]);
        assert!(
            new_credits_refresh_runner(with_trae, Cancel::new()).is_some(),
            "池里有 Trae 账号时必须装回调"
        );
    }

    // ── 循环体 ──

    #[tokio::test]
    async fn loop_only_refreshes_trae_and_isolates_failures() {
        let dir = tmp_dir("only-trae");
        let f1 = write_cred_file(&dir, "trae-1", "cn");
        let f2 = write_cred_file(&dir, "trae-2", "cn");
        let pool = pool_with(vec![
            trae_auth("trae-1", &f1),
            trae_auth("trae-2", &f2),
            other_auth("wb-1", crate::auth::PRODUCT_WORKBUDDY),
        ]);
        let (fetch, seen) = fake_fetcher(vec![
            Ok(sample(Some(321), 1793429317)),
            Err("HTTP 500: boom".to_string()),
        ]);

        let (processed, updated) = refresh_credits_once(&pool, &fetch, &Cancel::new()).await;

        // 顺序不敏感：断言「只碰了这两个 Trae 账号」（池的遍历顺序不是本模块的契约）。
        let mut seen_uids = seen.lock().unwrap().clone();
        seen_uids.sort();
        assert_eq!(seen_uids, vec!["trae-1".to_string(), "trae-2".to_string()]);
        assert_eq!(processed, 2, "只处理 Trae 账号（wb-1 不得进入）");
        assert_eq!(updated, 1);
        assert_eq!(pool_pair(&pool, "trae-1"), (321, 1793429317));
        assert_eq!(pool_pair(&pool, "trae-2"), (0, 0), "取数失败必须不写该账号状态");
        assert_eq!(pool_pair(&pool, "wb-1").0, 0, "非 Trae 账号不得被写");
    }

    #[tokio::test]
    async fn loop_keeps_credits_when_unlimited() {
        let dir = tmp_dir("unlimited");
        let f1 = write_cred_file(&dir, "trae-9", "cn");
        let pool = pool_with(vec![trae_auth("trae-9", &f1)]);
        {
            let mut p = pool.lock().unwrap();
            p.set_credits_and_expiry("trae-9", 5000, 0);
        }
        let (fetch, _seen) = fake_fetcher(vec![Ok(CreditsSample {
            credits: None,
            soonest_expire_at: 1793462399,
            unlimited: true,
            used: 42,
            counted_packs: 0,
        })]);

        let (processed, updated) = refresh_credits_once(&pool, &fetch, &Cancel::new()).await;

        assert_eq!((processed, updated), (1, 1));
        assert_eq!(
            pool_pair(&pool, "trae-9"),
            (5000, 1793462399),
            "无限额：credits 原值透传，只更新到期日"
        );
    }

    #[tokio::test]
    async fn loop_skips_sg_accounts_without_request() {
        let dir = tmp_dir("sg-skip");
        let f1 = write_cred_file(&dir, "trae-sg", "sg");
        let pool = pool_with(vec![trae_auth("trae-sg", &f1)]);
        let (fetch, seen) = fake_fetcher(vec![Ok(sample(Some(1), 1))]);

        let (processed, updated) = refresh_credits_once(&pool, &fetch, &Cancel::new()).await;

        assert_eq!(processed, 1, "sg 账号算「处理过」（记了日志），但不发请求");
        assert_eq!(updated, 0);
        assert!(seen.lock().unwrap().is_empty(), "sg 不得发出站请求（端点 CN 专属）");
        assert_eq!(pool_pair(&pool, "trae-sg"), (0, 0));
    }

    #[tokio::test]
    async fn loop_returns_immediately_when_cancelled() {
        let dir = tmp_dir("cancel");
        let f1 = write_cred_file(&dir, "trae-c", "cn");
        let pool = pool_with(vec![trae_auth("trae-c", &f1)]);
        let (fetch, seen) = fake_fetcher(vec![Ok(sample(Some(1), 1))]);
        let cancel = Cancel::new();
        cancel.cancel();

        let (processed, updated) = refresh_credits_once(&pool, &fetch, &cancel).await;

        assert_eq!((processed, updated), (0, 0));
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn runner_writes_pool_after_one_round() {
        let dir = tmp_dir("runner");
        let f1 = write_cred_file(&dir, "trae-r", "cn");
        let pool = pool_with(vec![trae_auth("trae-r", &f1)]);
        let (fetch, _seen) = fake_fetcher(vec![Ok(sample(Some(777), 1793429317))]);
        let runner =
            new_credits_refresh_runner_with(Arc::clone(&pool), fetch, Cancel::new()).expect("池里有 Trae 账号 ⇒ 必须装上");

        runner().await;

        assert_eq!(pool_pair(&pool, "trae-r"), (777, 1793429317));
    }

    // ── 网络（本地假上游）──

    /// 起一个只服务一次的假上游，返回 (base, 收到的请求全文)。
    fn serve_once(
        response_body: &'static str,
        status_line: &'static str,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定失败");
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 4096];
            let mut header_end = None;
            while let Ok(n) = sock.read(&mut tmp) {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    header_end = Some(p);
                    break;
                }
            }
            if let Some(p) = header_end {
                let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                let cl = head
                    .split("content-length:")
                    .nth(1)
                    .and_then(|s| s.split(|c| c == '\r' || c == '\n').next())
                    .and_then(|s| s.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                while buf.len() < p + 4 + cl {
                    match sock.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        Err(_) => break,
                    }
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&buf).to_string());
            let resp = format!(
                "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.flush();
        });
        (format!("http://{addr}"), rx)
    }

    #[tokio::test]
    async fn post_sends_pay_path_and_exact_body() {
        let body = r#"{"user_entitlement_pack_list":[{"usage":{"credits_amount":33.3536},"expire_time":1793429317,"entitlement_base_info":{"end_time":1793462399,"quota":{"credits_limit":500}}}]}"#;
        let (base, rx) = serve_once(body, "HTTP/1.1 200 OK");
        let client = Client::new();
        let cred = cred("trae-1", Realm::Cn);

        let raw = post_ent_usage(&client, &cred, &profile(), &base)
            .await
            .expect("200 必须成功");
        let s = parse_ent_usage(&raw).expect("真实形状必须能解析");
        assert_eq!(s.credits, Some(467));
        assert_eq!(s.soonest_expire_at, 1793429317);

        let req = rx.recv_timeout(Duration::from_secs(5)).expect("没收到请求");
        assert!(
            req.starts_with("POST /trae/api/v2/pay/ide_user_ent_usage"),
            "路径必须是 v2 + /pay/ 层，实际: {}",
            req.lines().next().unwrap_or("")
        );
        assert!(
            req.contains(r#"{"require_usage":true}"#),
            "请求体必须恰好是 require_usage"
        );
        assert!(
            !req.to_lowercase().contains("req_source"),
            "★ 绝不能带 req_source（实测带它上游回 400 空体）"
        );
        let lower = req.to_lowercase();
        assert!(lower.contains("authorization: cloud-ide-jwt "));
        assert!(lower.contains("content-type: application/json"));
    }

    #[tokio::test]
    async fn post_reports_status_and_body_head() {
        let (base, rx) = serve_once(r#"{"code":1,"msg":"nope"}"#, "HTTP/1.1 400 Bad Request");
        let client = Client::new();
        let cred = cred("trae-1", Realm::Cn);

        let err = post_ent_usage(&client, &cred, &profile(), &base)
            .await
            .expect_err("400 必须返回 Err");
        assert!(err.starts_with("HTTP 400: "), "实际: {err}");
        assert!(err.contains("nope"), "错误信息必须带响应体前 200 字，实际: {err}");
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "假上游必须收到请求"
        );
    }

    #[tokio::test]
    async fn post_rejects_non_json_without_panic() {
        let (base, _rx) = serve_once("not json at all", "HTTP/1.1 200 OK");
        let client = Client::new();
        let cred = cred("trae-1", Realm::Cn);

        let err = post_ent_usage(&client, &cred, &profile(), &base)
            .await
            .expect_err("非 JSON 必须返回 Err");
        assert!(err.contains("响应不是 JSON"), "实际: {err}");
    }

    // ── fetch_sample：畸形响应不写池 ──

    /// 上游回了 200 但形状不对（权益包列表为空）⇒ fetch_sample 必须返回 Err，
    /// 且**不能**把「未知」当成「没额度」（否则会把账号额度清零）。
    #[tokio::test]
    async fn fetch_sample_rejects_empty_pack_list() {
        let (base, rx) = serve_once(r#"{"user_entitlement_pack_list":[]}"#, "HTTP/1.1 200 OK");
        let client = Client::new();
        let cred = cred("trae-1", Realm::Cn);

        // 直接打 post_ent_usage（fetch_sample 用的是 realm::auth_host，指向真实域名）。
        let raw = post_ent_usage(&client, &cred, &profile(), &base)
            .await
            .expect("200 必须成功");
        assert!(parse_ent_usage(&raw).is_none(), "空包列表必须是「未知」");
        assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok());
    }
}




