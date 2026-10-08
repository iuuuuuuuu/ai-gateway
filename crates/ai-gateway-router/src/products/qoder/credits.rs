//! Qoder 积分采集（`GET /api/v2/quota/usage`）。
//!
//! 与 `products/trae/credits.rs` 同形（那是本模块的模板）：独立一条定时循环，
//! 每轮给池里每个 Qoder 账号发 1 次只读请求，把 `remaining` 写进账号池的
//! `credits` 字段。
//!
//! # 这个文件解决什么（业主 2026-10-09 现场）
//!
//! 业主截图：两个 Qoder 账号（国服 `019f…` / 国际版 `01a0…`）在「账号池」
//! 页的**积分余额列显示 0**，而宿主侧 `~/.wb-switch/qoder/accounts.json` 里
//! 明明写着 `credits: 1996` 与 `credits: 1700`。业主原话：
//!
//! > qoder这个虽然到期时间查不出来,但是积分余额显示错误,这个是可以查出来的
//!
//! ## 为什么是 0 而不是「—」
//!
//! 账号池页的「积分余额」列取的是**网关 `/status` 的 `credits`**
//!（`GatewayPage.tsx` 的 `PoolRowMetrics.balance`）。而那个字段只有两条写入路径：
//!
//! 1. `scheduler::refresh_credits_with_gap`（WorkBuddy 积分巡检）—— 带产品闸门
//!    `a.product_of() != PRODUCT_WORKBUDDY ⇒ continue`，Qoder 账号**必被跳过**；
//! 2. `products/trae/credits.rs` 的采集循环 —— 只认 Trae。
//!
//! ⇒ Qoder 账号的 `credits` **从来没有被写过**，恒为 `Entry::default()` 的 0。
//! 界面渲染判据是 `typeof credits === "number"`，而 `0` 是数字 ⇒ 显示 **0**
//! 而不是「—」。这正是业主看到的现象。
//!
//! ## 为什么另起一条循环而不是塞进现有巡检
//!
//! 三条理由，与 Trae 那处（`bin/gateway.rs:757-766`）逐条对应：
//!
//! 1. **产品闸门**：scheduler 的积分巡检打的是 WorkBuddy 端点，Qoder 凭证打过去
//!    只会 401 并在账号记录里留下不属于它的错误；
//! 2. **端点/鉴权完全不同**：Qoder 走 OpenAPI 域、只带 `Bearer DT`**不签名**
//!    （`client.rs:270-278` 明确写了「两条路径不能混」）；
//! 3. **不占用计费周期槽位**：scheduler 里 `qoder` 那个单槽回调已经被
//!    `billing_cycle`（`nextResetAt`）占用，塞进去会让两个职责互相阻塞。
//!
//! ## 只读，且**不改**账号状态
//!
//! 与 Trae 采集同口径：本循环只写 `credits` 与 `expire_at` 两个字段，
//! **不碰** `until` / `breaker_until` / `disabled`。查额度失败**绝不**喂熔断器 ——
//! 那是「能力探测」不是「账号做错了事」，历史上 Go 侧在这里误喂过。

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::auth::PRODUCT_QODER;
use crate::pool::Pool;
use crate::scheduler::Cancel;

use super::client::QoderClient;
use super::dispatch;

/// 巡检周期。与 Trae 采集同值（15 分钟）。
///
/// 为什么是 15 分钟：积分余额是**展示**用的（选号不看它 —— 选号看的是
/// `cost_rate` 与到期档位），15 分钟足以让界面不显示陈旧值，而请求量
///（账号数 × 每 15 分钟 1 次）相对上游可忽略。
pub const DEFAULT_CREDITS_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// 账号之间的间隔，避免瞬间并发打满上游（与 Trae 的 300ms 同量级）。
pub const CREDITS_REFRESH_GAP: Duration = Duration::from_millis(300);

/// 一轮采集的执行体（形状与 `trae::credits::CreditsRunner` 相同）。
pub type CreditsRunner = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// 取一次额度（可注入，供单测脱离网络）。
pub(crate) type QuotaFetcher = Arc<
    dyn Fn(super::cred::QoderCred)
        -> Pin<Box<dyn Future<Output = Result<i64, String>> + Send>>
        + Send
        + Sync
>;

/// 池里是否有 Qoder 账号（没有就不装回调、一个请求都不发）。
///
/// 与 `trae::credits::pool_has_trae` 同口径，也与 `billing_cycle` 的
/// 构造期判据一致：热加载新增账号要等重启（Go 侧同款取舍）。
fn pool_has_qoder(pool: &Arc<Mutex<Pool>>) -> bool {
    match pool.lock() {
        Ok(p) => p.list().iter().any(|st| {
            p.auth_by_uid(&st.uid)
                .map(|a| a.product_of() == PRODUCT_QODER)
                .unwrap_or(false)
        }),
        Err(_) => false,
    }
}

/// 池里全部 Qoder 账号的 uid（**稳定顺序**：按 uid 升序）。
///
/// 排序是刻意的：错误文案与日志会带上 uid，若顺序随机，同一轮的错误每次显示
/// 的顺序都不同，看起来像「账号在变」。
fn qoder_uids(pool: &Arc<Mutex<Pool>>) -> Vec<String> {
    let mut uids: Vec<String> = match pool.lock() {
        Ok(p) => p
            .list()
            .iter()
            .filter(|st| {
                p.auth_by_uid(&st.uid)
                    .map(|a| a.product_of() == PRODUCT_QODER)
                    .unwrap_or(false)
            })
            .map(|st| st.uid.clone())
            .collect(),
        Err(_) => Vec::new(),
    };
    uids.sort();
    uids
}

/// 脱敏 uid（日志用）：只留前 8 位。
fn mask_uid(uid: &str) -> String {
    uid.chars().take(8).collect()
}

/// 跑一轮采集：逐个 Qoder 账号查额度，成功就写池。
///
/// 返回 `(处理数, 更新数)`。
///
/// # 三条纪律（与 Trae 采集逐条对齐）
///
/// 1. **失败不写池**：查不到就跳过这个账号，**绝不**写 0 —— 把「查不到」写成 0
///    正是本次要修的 bug 的同类（业主看到的就是一个假的 0）；
/// 2. **不改账号状态**：只写 `credits`，不碰 `until`/`breaker_until`/`disabled`；
/// 3. **取消即退**：宿主关停时不该继续发请求。
async fn refresh_credits_once(
    pool: &Arc<Mutex<Pool>>,
    fetch: &QuotaFetcher,
    cancel: &Cancel,
) -> (usize, usize) {
    let mut processed = 0usize;
    let mut updated = 0usize;
    let mut first = true;

    for uid in qoder_uids(pool) {
        if cancel.is_cancelled() {
            return (processed, updated);
        }
        if !first && !cancel.sleep(CREDITS_REFRESH_GAP).await {
            return (processed, updated);
        }
        first = false;
        processed += 1;

        let Some(a) = auth_of(pool, &uid) else {
            crate::logline!(
                "qoder-credits {}: 账号已不在池中，跳过",
                mask_uid(&uid)
            );
            continue;
        };
        let Some(cr) = dispatch::cred_of(&a) else {
            crate::logline!(
                "qoder-credits {}: 凭证不可用于查询（缺 token 或文件读不出），已跳过",
                mask_uid(&uid)
            );
            continue;
        };

        let remain = match fetch(cr).await {
            Ok(v) => v,
            Err(e) => {
                crate::logline!("qoder-credits {}: {e}", mask_uid(&uid));
                continue;
            }
        };

        match pool.lock() {
            Ok(mut p) => {
                p.set_credits(&uid, remain);
                updated += 1;
            }
            Err(e) => crate::logline!(
                "Qoder 积分刷新：账号池锁中毒，{} 本轮跳过（{e}）",
                mask_uid(&uid)
            ),
        }
    }

    (processed, updated)
}

/// 从池里取某个 uid 的 `Auth` 克隆。
fn auth_of(pool: &Arc<Mutex<Pool>>, uid: &str) -> Option<crate::auth::Auth> {
    match pool.lock() {
        Ok(p) => p.auth_by_uid(uid),
        Err(_) => None,
    }
}
/// 取一次额度：`GET /api/v2/quota/usage` → `remaining`。
///
/// ★ 只取 `remaining`（= `userQuota.remaining + addOnQuota.remaining`，由
/// [`QoderClient::fetch_quota`] 算好）—— 池的 `credits` 字段就是这个语义
/// （Trae 那处同款）。到期档位这里**不写**：`fetch_quota` 的 `expiresAt` 是
/// **套餐到期**不是周期重置，写进 `expire_at` 会污染选号的到期分层。
/// 业主自己也说了「到期时间查不出来」—— 那本就是另一件事（见 `userstatus.rs`）。
async fn fetch_remaining(client: &QoderClient, cr: super::cred::QoderCred) -> Result<i64, String> {
    let q = client.fetch_quota(&cr).await?;
    Ok(q.remaining)
}

/// 装回调：池里没有 Qoder 账号时返回 None。
///
/// ★ 这是**新增的出站请求**：每个 Qoder 账号每轮 1 次。返回 None 时一个请求都不发。
///
/// 调用方在 `bin/gateway.rs` 里把它包成一条独立的定时循环（周期
/// [`DEFAULT_CREDITS_REFRESH_INTERVAL`]），**不**复用 scheduler 的积分巡检槽位
///（那条循环带 WorkBuddy 产品闸门）。
#[must_use]
pub fn new_credits_refresh_runner(pool: Arc<Mutex<Pool>>, cancel: Cancel) -> Option<CreditsRunner> {
    if !pool_has_qoder(&pool) {
        return None;
    }
    let client = Arc::new(QoderClient::new());
    let fetch: QuotaFetcher = Arc::new(move |cr: super::cred::QoderCred| {
        let client = Arc::clone(&client);
        Box::pin(async move { fetch_remaining(&client, cr).await })
            as Pin<Box<dyn Future<Output = Result<i64, String>> + Send>>
    });
    new_credits_refresh_runner_with(pool, fetch, cancel)
}

/// 与 [`new_credits_refresh_runner`] 同形，但取数可注入（单测用）。
pub(crate) fn new_credits_refresh_runner_with(
    pool: Arc<Mutex<Pool>>,
    fetch: QuotaFetcher,
    cancel: Cancel,
) -> Option<CreditsRunner> {
    if !pool_has_qoder(&pool) {
        return None;
    }
    Some(Arc::new(move || {
        let pool = Arc::clone(&pool);
        let fetch = Arc::clone(&fetch);
        let cancel = cancel.clone();
        Box::pin(async move {
            let (processed, updated) = refresh_credits_once(&pool, &fetch, &cancel).await;
            if processed > 0 {
                crate::logline!("Qoder 积分刷新：处理 {processed} 个账号，{updated} 个有更新");
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
    use crate::auth::Auth;

    fn qoder_auth(uid: &str) -> Auth {
        let mut a = Auth::default();
        a.uid = uid.to_string();
        a.product = PRODUCT_QODER.to_string();
        a.domain = "qoder.sh".to_string();
        a.access_token = "dt-token".to_string();
        a.refresh_token = "drt-token".to_string();
        a
    }

    fn wb_auth(uid: &str) -> Auth {
        let mut a = Auth::default();
        a.uid = uid.to_string();
        a.domain = "www.workbuddy.cn".to_string();
        a.refresh_token = "rt".to_string();
        a
    }

    fn fetcher_returning(v: i64) -> QuotaFetcher {
        Arc::new(move |_cr| Box::pin(async move { Ok(v) }) as Pin<Box<dyn Future<Output = Result<i64, String>> + Send>>)
    }

    /// ★★ 业主现场回归：Qoder 账号的余额必须被写进池。
    ///
    /// 修之前 `credits` 恒为 0（没有任何循环写它），界面显示 0 而不是真实余额。
    #[test]
    fn writes_remaining_into_the_pool() {
        let pool = Arc::new(Mutex::new(Pool::new(String::new())));
        pool.lock().unwrap().add(qoder_auth("q-1"));
        let fetch = fetcher_returning(1996);
        let cancel = Cancel::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (processed, updated) = rt.block_on(refresh_credits_once(&pool, &fetch, &cancel));
        assert_eq!(processed, 1);
        assert_eq!(updated, 1);
        let credits = pool.lock().unwrap().list()[0].credits;
        assert_eq!(credits, 1996, "池里的余额必须是上游查到的值，不是 0");
    }

    /// ★★ 反例：**查不到就不写**。把失败写成 0 正是本 bug 的同类。
    #[test]
    fn a_failed_lookup_does_not_write_zero() {
        let pool = Arc::new(Mutex::new(Pool::new(String::new())));
        pool.lock().unwrap().add(qoder_auth("q-1"));
        pool.lock().unwrap().set_credits("q-1", 1700);
        let fetch: QuotaFetcher = Arc::new(|_cr| {
            Box::pin(async move { Err("上游 503".to_string()) })
                as Pin<Box<dyn Future<Output = Result<i64, String>> + Send>>
        });
        let cancel = Cancel::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (processed, updated) = rt.block_on(refresh_credits_once(&pool, &fetch, &cancel));
        assert_eq!(processed, 1, "账号确实被处理了");
        assert_eq!(updated, 0, "失败不该算一次更新");
        assert_eq!(
            pool.lock().unwrap().list()[0].credits,
            1700,
            "查不到时必须保留旧值，绝不写 0"
        );
    }

    /// 产品闸门：WorkBuddy 账号不进这条循环（它的余额由 scheduler 那条管）。
    #[test]
    fn only_qoder_accounts_are_processed() {
        let pool = Arc::new(Mutex::new(Pool::new(String::new())));
        pool.lock().unwrap().add(qoder_auth("q-1"));
        pool.lock().unwrap().add(wb_auth("wb-1"));
        assert_eq!(qoder_uids(&pool), vec!["q-1".to_string()]);
        let fetch = fetcher_returning(42);
        let cancel = Cancel::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (processed, _) = rt.block_on(refresh_credits_once(&pool, &fetch, &cancel));
        assert_eq!(processed, 1, "只处理 Qoder 账号");
    }

    /// 池里没有 Qoder 账号 ⇒ 不装回调（一个出站请求都不发）。
    #[test]
    fn no_qoder_account_means_no_runner() {
        let pool = Arc::new(Mutex::new(Pool::new(String::new())));
        pool.lock().unwrap().add(wb_auth("wb-1"));
        assert!(new_credits_refresh_runner(pool, Cancel::new()).is_none());
    }

    /// uid 顺序稳定（按 uid 升序）—— 否则同一轮的错误文案顺序每次不同。
    #[test]
    fn uid_order_is_stable() {
        let pool = Arc::new(Mutex::new(Pool::new(String::new())));
        pool.lock().unwrap().add(qoder_auth("q-b"));
        pool.lock().unwrap().add(qoder_auth("q-a"));
        assert_eq!(qoder_uids(&pool), vec!["q-a".to_string(), "q-b".to_string()]);
    }
}