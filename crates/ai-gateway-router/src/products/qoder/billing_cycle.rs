//! 把 Qoder 的**计费周期**（下次重置时刻）刷新接进网关的积分巡检循环。
//!
//! 对应 Go `go-gateway/cmd/server/billing_cycle.go`（240 行）。
//!
//! # 这个文件解决什么（N4 的最后一公里）
//!
//! [`crate::usage`] 的周期逻辑（2026-09-28）已经实现了「边界 → 周期投影」的全部
//! 逻辑，[`super::userstatus`] 也已经能从上游取到 `nextResetAt`；但**没有任何
//! 生产路径调用它们** —— `set_billing_cycle` / `cycle_of` 当时是死代码，于是
//! `/usage` 的 `billingCycles` 永远是空 map，界面上的「本周期用量 / 下次重置」
//! 永远没有数据，**而且不报任何错**。
//!
//! 本文件就是那条缺失的连线：遍历池里的 Qoder 账号 → 逐个取上游周期 →
//! 写进 [`crate::usage::Stats`]。
//!
//! # 为什么是只读巡检，不是领取动作
//!
//! 与 [`super::campaign`]（Qoder 权益领取）刻意分开：那边是**写操作**（会消耗/
//! 占用上游资源，需要幂等、hold 闸门、配额判断），这边是**纯只读状态轮询**，
//! 属「积分到期巡检」那一族 —— 故搭 `scheduler::run_credit_refresh_loop` 的便车。
//!
//! # ⚠ 已知耦合：`credit_refresh_enabled=false` 时本刷新也不跑
//!
//! 它搭的是积分巡检那条循环。若用户关掉 `pool.credit_refresh_enabled`，计费周期
//! 刷新会**一起停** —— Go `billing_cycle.go:24-27` 明确记录，这一点写进了 N4 的
//! 降级说明，**不是疏漏**。（要让它独立就得再开一条常驻循环，而「只读状态巡检」
//! 合成一条是刻意的：少一条常驻 task、少一处周期配置。）
//!
//! # UID 同源（最容易写错、且错了不报错的一处）
//!
//! `usage.record_billed` 记的是 `pool.Status.UID`，而 `set_billing_cycle` 必须记在
//! **同一个** uid 上，否则周期与用量永远 join 不上 —— 界面显示 0，没有任何错误
//! 信息。证据链（Go 任务要求自查）：
//!
//! ```text
//! cmd/server/main.go:40            qoderAuthOf: UID = cr.UID
//! internal/pool/pool.go:1447       upsertLocked 以 a.UID 为键
//! internal/qoder/dispatch.go:372   Cred{UID: a.UID, …}
//! internal/server/forward.go:967   uid := acct.UID
//! internal/server/handler.go:576   RecordBilled(s.uid, …)
//! ```
//!
//! 故本文件一律用 `pool.Status.UID` 作键，并把查询用的 `QoderCred.uid`
//! **强制**对齐到它（见 [`billing_cycle_cred_of`] 的尾注）。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::auth::Auth;
use crate::pool::Pool;
use crate::scheduler::{BillingCycleRunner, Cancel};
use crate::usage::Stats;

use super::client::QoderClient;
use super::cred::QoderCred;
use super::region::{region_from_domain, QoderRegion};
use super::userstatus::resolve_next_reset_ms;

/// 账号之间的间隔，避免瞬间并发打满上游。
///
/// 与 `crate::scheduler` 的 `CREDIT_REFRESH_GAP` **同量级**（都是 300ms）：
/// 那个常量在 `scheduler` 里，而本模块与 `scheduler` 是**平级**依赖
///（`scheduler` 不 import `products::qoder`，这条方向不能反过来），故这里独立
/// 声明一份。**改一处时另一处要跟着看**（两处都在注释里互相指了名）。
pub(crate) const BILLING_CYCLE_GAP: Duration = Duration::from_millis(300);

/// 取一个账号的「下次重置时刻」（epoch 毫秒）。
///
/// 返回 `0` 表示**上游没有给出可信值**（调用方保留上一次的周期，不要清零）。
///
/// # 为什么收成一个函数类型，而不是直接用 [`QoderClient`]
///
/// `QoderClient::fetch_user_status_with_base` 的 `base` 是**测试注入点**，而
/// `QoderClient` 的 `rpc` 字段是私有的 —— 直接吃 `&QoderClient` 就只能打真上游。
/// 收窄成闭包后：生产传 [`qoder_billing_cycle_fetcher`]，测试传桩。
pub(crate) type BillingCycleFetcher =
    Arc<dyn Fn(QoderCred) -> Pin<Box<dyn Future<Output = Result<i64, String>> + Send>> + Send + Sync>;

/// 构造计费周期刷新的执行体（**生产入口**）。
///
/// 返回 `None` 表示「当前构建没有 Qoder 账号可刷」（回调不装，排程直接跳过）——
/// 与 `new_product_tasks_runner` 同一口径：单产品部署是**合法配置**，
/// 不该在巡检里报错。
///
/// # 为什么这个函数是 `pub`（而不是收 `QoderClient`）
///
/// `bin/gateway.rs` 是**独立 crate**，看不到 lib 的 `pub(crate)` 项 ——
/// [`QoderClient`] 正是 `pub(crate)`。故这里由 lib 内部自建客户端，
/// 对外只暴露 `pub` 类型（[`Pool`] / [`Stats`] / [`Cancel`] / [`BillingCycleRunner`]）。
///
/// # ⚠ 构造期就要求池里有 Qoder 账号
///
/// 池里一个 Qoder 账号都没有 ⇒ 返回 `None`（不装回调）。这与 Go
/// `newBillingCycleRefreshRunner` 的 `fetch == nil` 分支同源：Go 那边多产品关闭时
/// `qoderClient == nil` ⇒ runner 为 nil。代价是**热加载新增的 Qoder 账号要等重启**
/// 才会被周期巡检覆盖 —— 与 Go 的 `qoderClient == nil` 情形同类，写进注释以免被
/// 误当成缺陷。
pub fn new_billing_cycle_refresh_runner(
    pool: Arc<std::sync::Mutex<Pool>>,
    store: Arc<Stats>,
    cancel: Cancel,
) -> Option<BillingCycleRunner> {
    // 池里没有 Qoder 账号 ⇒ 不装回调（单产品部署是合法配置，不该报错）。
    if !pool_has_qoder(&pool) {
        return None;
    }
    let client = Arc::new(QoderClient::new());
    new_billing_cycle_refresh_runner_with(pool, qoder_billing_cycle_fetcher(client), store, cancel)
}

/// 可注入 fetcher 的构造（测试用；`bin` 走上面那个 `pub` 入口）。
///
/// ⚠ 与 [`new_billing_cycle_refresh_runner`] 一样**先判池里有没有 Qoder 账号** ——
/// 判据只此一处，两个入口同口径（否则「池为空返回 None」这条会只在生产路径成立，
/// 测试路径悄悄装上回调）。
pub(crate) fn new_billing_cycle_refresh_runner_with(
    pool: Arc<std::sync::Mutex<Pool>>,
    fetch: BillingCycleFetcher,
    store: Arc<Stats>,
    cancel: Cancel,
) -> Option<BillingCycleRunner> {
    if !pool_has_qoder(&pool) {
        return None;
    }
    Some(Arc::new(move || {
        // `Fn` 可被调用多次 ⇒ 捕获物必须在**每次调用内**克隆。
        let pool = Arc::clone(&pool);
        let fetch = Arc::clone(&fetch);
        let store = Arc::clone(&store);
        let cancel = cancel.clone();
        Box::pin(async move {
            let (processed, updated) =
                refresh_billing_cycles_once(&pool, &*fetch, &store, &cancel).await;
            // 日志逐字对齐 Go（`billing_cycle.go:93-95`）：**仅当处理过账号才打**，
            // 否则每 15 分钟刷一行「处理 0 个」是纯噪声。
            if processed > 0 {
                crate::logline!("计费周期刷新：处理 {processed} 个 Qoder 账号，{updated} 个周期有更新");
            }
        }) as Pin<Box<dyn Future<Output = ()> + Send>>
    }))
}

/// 池里是否至少有一个 Qoder 账号。
fn pool_has_qoder(pool: &Arc<std::sync::Mutex<Pool>>) -> bool {
    match pool.lock() {
        Ok(p) => p
            .list()
            .iter()
            .any(|st| p.auth_by_uid(&st.uid).map(|a| a.is_qoder()).unwrap_or(false)),
        // 中毒锁：当作「没有」⇒ 不装回调（比 panic 好）。
        Err(_) => false,
    }
}

/// 跑一轮；返回（处理过的账号数，周期被更新的账号数）。
///
/// # 循环体的三条约定（与 `scheduler::refresh_credits_with_gap` 同口径）
///
/// ```text
/// ① **严格串行**，账号之间留 BILLING_CYCLE_GAP —— 上游对短时并发不友好
/// ② 单账号失败**只记日志并 continue**：一个号取不到不该让整轮停摆，
///    也**不写**该账号的周期状态（宁可没有数据，也不要一个假边界）
/// ③ 取消立即返回：网关停机时不该还在等上游
/// ```
///
/// # 关于 `no_route` 账号
///
/// **照刷**。`no_route` 的语义是「只不接流量」（见 [`Auth::no_route`] 的注释），
/// 养号任务照跑；周期刷新是只读轮询，同属这一族。而且用户把开关打开后，我们
/// 应当已经有它的周期，而不是等到下一次巡检才有。
pub(crate) async fn refresh_billing_cycles_once(
    pool: &Arc<std::sync::Mutex<Pool>>,
    fetch: &(dyn Fn(QoderCred) -> Pin<Box<dyn Future<Output = Result<i64, String>> + Send>>
          + Send
          + Sync),
    store: &Arc<Stats>,
    cancel: &Cancel,
) -> (usize, usize) {
    let mut processed = 0usize;
    let mut updated = 0usize;

    // 先取一份快照，**不跨 await 持锁** —— 上游请求可能几百毫秒，持锁会阻塞
    // 整个网关的选号。
    let entries: Vec<(String, Option<Auth>)> = match pool.lock() {
        Ok(p) => p
            .list()
            .into_iter()
            .map(|st| {
                let a = p.auth_by_uid(&st.uid);
                (st.uid, a)
            })
            .collect(),
        Err(e) => {
            // 中毒锁：如实报出来，但不让巡检崩掉。
            crate::logline!("计费周期刷新：账号池锁中毒，本轮跳过（{e}）");
            return (0, 0);
        }
    };

    for (uid, a) in entries {
        // ③ 取消立即返回（`Cancel` 是 watch 通道，`is_cancelled` 是同步读）。
        if cancel.is_cancelled() {
            return (processed, updated);
        }
        // ⚠ 产品闸门必须是 `is_qoder()`。
        //
        // **不要**写成 `a.product_of() != PRODUCT_WORKBUDDY` —— 那个判据会把
        // ZCode 账号一起放进来，然后拿 ZCode 的凭证去打 Qoder 的端点。这种错
        // 不会报错，只会让 Qoder 的周期被 ZCode 的账号数冲淡。
        let Some(a) = a else { continue };
        if !a.is_qoder() {
            continue;
        }
        // 间隔加在「已经处理过至少一个账号」之后：若第一个 Qoder 账号排在池里
        // 的第 3 位，前面跳过的两个不该让我们白等 300ms。
        if processed > 0 {
            // 用 `cancel.sleep`（可取消）而不是裸 `tokio::time::sleep`：
            // 网关停机时不该卡在这 300ms 上等它走完。
            if !cancel.sleep(BILLING_CYCLE_GAP).await {
                return (processed, updated);
            }
        }
        processed += 1;

        let Some(cr) = billing_cycle_cred_of(&a) else {
            crate::logline!(
                "billing-cycle {}: 凭证不可用于查询（缺 uid），已跳过",
                mask_uid(&uid)
            );
            continue;
        };
        let next_ms = match fetch(cr).await {
            Ok(v) => v,
            Err(e) => {
                // ② 错误隔离：这一行日志是唯一的痕迹，整轮继续。
                //    ★ 且**不写**该账号的周期（宁可没数据，也不要假边界）。
                crate::logline!("billing-cycle {}: {e}", mask_uid(&uid));
                continue;
            }
        };
        // `next_ms <= 0` / 不可信时 `set_billing_cycle` 内部会**拒绝并保留**
        // 上一周期（`usage.rs` 的四情形 ⓪）—— 这里刻意**不做二次判断**：
        // 可信度判据只能有一处，两处必然漂移。
        if store.set_billing_cycle(&uid, next_ms) {
            updated += 1;
        }
    }
    (processed, updated)
}

/// 把池里的账号还原成 Qoder 凭证。
///
/// # 为什么优先从**文件**读，而不是照抄 `dispatch::cred_of`
///
/// [`super::dispatch`] 的 `cred_of` 从 [`Auth`] 重建时会**丢掉机器指纹** ——
/// [`Auth`] 根本没有那三个字段。而 `/user/status` 走的正是**机器身份**通道
///（见 [`super::userstatus`] 的文件头），`cosy-machineid` 就是从
/// [`QoderCred::machine_id`] 取的 —— 从池重建会让这个头**静默不发**。
/// 凭证文件里有完整的指纹（`cred.rs::load_file` 读回来、`save_atomic` 写回去），
/// 故优先按 `file_path` 重新加载。
///
/// 文件读不到时（凭证是运行时注入的 / 文件被删 / 宿主正在改写）**回退**到从池
/// 重建：少一个头在国服仍能查到周期（实测国服 host 不校验指纹），比整轮跳过好。
pub(crate) fn billing_cycle_cred_of(a: &Auth) -> Option<QoderCred> {
    if a.uid.is_empty() {
        return None;
    }
    let mut cr = if a.file_path.is_empty() {
        None
    } else {
        QoderCred::load_file(&a.file_path).ok()
    };
    if cr.is_none() {
        let mut rebuilt = QoderCred {
            uid: a.uid.clone(),
            nickname: a.nickname.clone(),
            dt: a.access_token.clone(),
            drt: a.refresh_token.clone(),
            dt_expires_at: a.expires_at,
            file_path: a.file_path.clone(),
            // 区域由域名判定（与加载层、`dispatch::cred_of` 同口径）。
            region: region_from_domain(&a.domain),
            ..Default::default()
        };
        if rebuilt.region == QoderRegion::Unknown {
            // 域名认不出区域时按国服（`endpoints` 的默认行为），
            // 但**不写回** `domain` —— 让用户能在界面上修正。
            rebuilt.region = QoderRegion::CN;
        }
        cr = Some(rebuilt);
    }
    // ⚠ 强制把 uid 对齐到**池里的键**。
    //
    // `/user/status` 的 `userId` 与 `usage.set_billing_cycle` 的 uid 必须是同一个
    // 值（见文件头的 UID 同源说明）。文件里的 uid 与池键不同只可能来自手改或
    // 重复导入，此时以池键为准 —— 否则会「查 A 账号的周期、记在 B 账号名下」，
    // 而那种错**不会报错**，只会让两个账号的数据都不对。
    if let Some(c) = cr.as_mut() {
        c.uid = a.uid.clone();
    }
    cr
}

/// 把真 client 收窄成 [`BillingCycleFetcher`]。
///
/// ⚠ 这里**只调** `/user/status`，不调 `fetch_quota` ⇒
/// [`resolve_next_reset_ms`] 的 **Quota 兜底分支（第二个参数）在生产路径上
/// 永远不会走到，仅由 `userstatus.rs` 的单测覆盖**。
///
/// 为什么不顺手把兜底也接上：`/api/v2/quota/usage` 的 `expiresAt` 是**套餐到期**，
/// 不是周期重置（无到期套餐报 9999-12-31 哨兵）。多打一个端点换来一个语义不同的
/// 数字，只会让「周期」看起来更确定而实际更错 —— 参考实现也只把它当
/// 「nextResetAt 缺失时的兜底」。
///
/// 于是这里只做**可信度净化**：[`resolve_next_reset_ms`] 会在 `nextResetAt`
/// 不可信时返回 0，由 `set_billing_cycle` 拒绝并保留上一周期。
pub(crate) fn qoder_billing_cycle_fetcher(client: Arc<QoderClient>) -> BillingCycleFetcher {
    Arc::new(move |cr: QoderCred| {
        let client = Arc::clone(&client);
        Box::pin(async move {
            let st = client.fetch_user_status(&cr).await?;
            Ok(resolve_next_reset_ms(Some(&st), None))
        }) as Pin<Box<dyn Future<Output = Result<i64, String>> + Send>>
    })
}

/// 脱敏 uid（日志里不出现完整账号标识）。
///
/// 本仓库是公开仓库，日志可能被用户贴到 issue 里 —— 只留前 8 位足够定位，
/// 又不足以反查账号。与 `products::qoder::mask_uid` 同款（Go `main.go:96`）。
fn mask_uid(uid: &str) -> String {
    if uid.len() <= 8 {
        return uid.to_string();
    }
    format!("{}…", &uid[..8])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Auth;
    use crate::pool::Pool;
    use crate::usage::Stats;
    use crate::products::qoder::region::QoderRegion;
    use serde_json::Value;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;


    // ─────────────── 计费周期巡检 runner（Go `billing_cycle_test.go` 的关键用例） ───────────────

    fn qoder_auth(uid: &str, file_path: &str) -> Auth {
        Auth {
            uid: uid.to_string(),
            product: crate::auth::PRODUCT_QODER.to_string(),
            domain: "qoder.com.cn".to_string(),
            access_token: "dt-placeholder".to_string(),
            refresh_token: "drt-placeholder".to_string(),
            file_path: file_path.to_string(),
            ..Default::default()
        }
    }

    fn zcode_auth(uid: &str) -> Auth {
        Auth {
            uid: uid.to_string(),
            product: crate::auth::PRODUCT_ZCODE.to_string(),
            ..Default::default()
        }
    }

    fn pool_with(auths: Vec<Auth>) -> Arc<std::sync::Mutex<Pool>> {
        let mut p = Pool::new(String::new());
        for a in auths {
            p.add(a);
        }
        Arc::new(std::sync::Mutex::new(p))
    }

    /// 一个按 uid 记账的假 fetcher：记录被查询过的 uid，按脚本返回结果。
    type FetchLog = Arc<std::sync::Mutex<Vec<(String, Option<i64>)>>>;

    fn fake_fetcher(
        log: FetchLog,
        results: Vec<Result<i64, String>>,
        seen: Arc<std::sync::Mutex<usize>>,
    ) -> Arc<dyn Fn(QoderCred) -> Pin<Box<dyn Future<Output = Result<i64, String>> + Send>> + Send + Sync>
    {
        // ⚠ 用 `VecDeque::pop_front`，**不要**用 `iter().nth(idx)` ——
        // 那会每调一次就从头重新消费一遍，脚本实际只生效第一条（本移植第一版
        // 就是这么写的，表现为「第二个账号也拿到第一条的结果」）。
        let results: Arc<std::sync::Mutex<std::collections::VecDeque<Result<i64, String>>>> =
            Arc::new(std::sync::Mutex::new(results.into()));
        Arc::new(move |cr: QoderCred| {
            log.lock().unwrap().push((cr.uid.clone(), None));
            let (results, seen) = (Arc::clone(&results), Arc::clone(&seen));
            Box::pin(async move {
                *seen.lock().unwrap() += 1;
                results
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(Ok(0))
            }) as Pin<Box<dyn Future<Output = Result<i64, String>> + Send>>
        })
    }

    /// ④ 池为空 ⇒ runner 返回 `None`（不装回调）。
    ///
    /// 单产品部署是**合法配置**，不该在巡检里报错，也不该每 15 分钟打一行噪声。
    #[test]
    fn runner_is_none_when_the_pool_has_no_qoder_account() {
        let store = Arc::new(Stats::new(""));
        let log: FetchLog = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::new(std::sync::Mutex::new(0usize));

        // 空池
        let empty = pool_with(vec![]);
        assert!(
            new_billing_cycle_refresh_runner_with(
                empty,
                fake_fetcher(log.clone(), vec![], seen.clone()),
                Arc::clone(&store),
                Cancel::new()
            )
            .is_none(),
            "空池应返回 None"
        );

        // 只有 WorkBuddy / ZCode 账号 ⇒ 同样没有可刷的
        let others = pool_with(vec![
            Auth { uid: "wb-1".into(), ..Default::default() },
            zcode_auth("zcode-1"),
        ]);
        assert!(
            new_billing_cycle_refresh_runner_with(
                others,
                fake_fetcher(log.clone(), vec![], seen),
                store,
                Cancel::new()
            )
            .is_none(),
            "没有 Qoder 账号应返回 None"
        );
    }

    /// ⑤ 跳过非 Qoder 账号（**用 ZCode 账号做反证**）。
    ///
    /// 产品闸门若写成 `product_of() != "workbuddy"`，ZCode 账号会被放进来，
    /// 然后拿 ZCode 的凭证去打 Qoder 端点 —— 不报错，只让周期被冲淡。
    #[tokio::test]
    async fn runner_skips_non_qoder_accounts() {
        let pool = pool_with(vec![
            Auth { uid: "wb-1".into(), ..Default::default() },
            zcode_auth("zcode-1"),
            qoder_auth("qoder-real", ""),
            zcode_auth("zcode-2"),
        ]);
        let store = Arc::new(Stats::new(""));
        let log: FetchLog = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::new(std::sync::Mutex::new(0usize));
        let fetch = fake_fetcher(log.clone(), vec![Ok(1_790_000_000_000)], seen);

        let (processed, updated) =
            refresh_billing_cycles_once(&pool, &*fetch, &store, &Cancel::new()).await;

        assert_eq!(processed, 1, "只应处理 1 个 Qoder 账号（其余是 WorkBuddy / ZCode）");
        assert_eq!(updated, 1);
        let called: Vec<String> = log.lock().unwrap().iter().map(|(u, _)| u.clone()).collect();
        assert_eq!(called, vec!["qoder-real".to_string()], "只有 Qoder 账号被查询");
        assert!(
            store.cycle_of("zcode-1").is_none(),
            "ZCode 账号不该被写入周期（产品闸门写错就会出现）"
        );
        assert!(store.cycle_of("qoder-real").is_some(), "Qoder 账号应被写入周期");
    }

    /// ⑥ 单账号 fetch 失败 ⇒ **不写**该账号周期 + 继续下一个。
    #[tokio::test]
    async fn a_failed_fetch_is_logged_and_the_round_continues() {
        let pool = pool_with(vec![qoder_auth("qoder-a", ""), qoder_auth("qoder-b", "")]);
        let store = Arc::new(Stats::new(""));
        let log: FetchLog = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::new(std::sync::Mutex::new(0usize));
        let fetch = fake_fetcher(
            log,
            vec![Err("上游 500".to_string()), Ok(1_790_000_000_000)],
            seen,
        );

        let (processed, updated) =
            refresh_billing_cycles_once(&pool, &*fetch, &store, &Cancel::new()).await;

        assert_eq!(processed, 2, "两个账号都该被处理（失败的那个也要计入 processed）");
        assert_eq!(updated, 1, "只有成功的那个有更新");
        assert!(
            store.cycle_of("qoder-a").is_none(),
            "失败的账号**不该**被写入周期 —— 宁可没数据，也不要假边界"
        );
        assert!(store.cycle_of("qoder-b").is_some(), "后续账号必须继续处理");
    }

    /// ⑥b 上游返回不可信值（0 / 哨兵）⇒ 不产生周期（净化在 `set_billing_cycle` 里）。
    #[tokio::test]
    async fn an_implausible_value_does_not_create_a_cycle() {
        let pool = pool_with(vec![qoder_auth("qoder-a", "")]);
        let store = Arc::new(Stats::new(""));
        let log: FetchLog = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::new(std::sync::Mutex::new(0usize));
        // fetcher 返回 0（= 上游没报可信值）。
        let fetch = fake_fetcher(log, vec![Ok(0)], seen);

        let (processed, updated) =
            refresh_billing_cycles_once(&pool, &*fetch, &store, &Cancel::new()).await;
        assert_eq!(processed, 1);
        assert_eq!(updated, 0, "不可信值不该算作「有更新」");
        assert!(store.cycle_of("qoder-a").is_none(), "不可信值不该建立周期");
    }

    /// ⑦ uid 强制对齐到**池键**（凭证文件里写的是另一个 uid）。
    ///
    /// 不对齐就会「查 A 账号的周期、记在 B 账号名下」，而那种错**不会报错**。
    #[test]
    fn cred_uid_is_forced_to_the_pool_key() {
        let dir = std::env::temp_dir().join(format!("wbswg-qoder-uid-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("qoder-credfile.json");
        let path_s = path.to_string_lossy().to_string();
        // 文件里的 uid 与池键**故意不同**。
        std::fs::write(
            &path_s,
            r#"{"auth":{"accessToken":"dt-x","refreshToken":"drt-x","domain":"qoder.com.cn"},
                "account":{"uid":"uid-in-file"},"machine":{"id":"machine-id-in-file"}}"#,
        )
        .unwrap();

        let a = qoder_auth("uid-pool-key", &path_s);
        let cr = billing_cycle_cred_of(&a).expect("应能构造凭证");
        assert_eq!(cr.uid, "uid-pool-key", "uid 必须强制对齐到池键");
        // 文件能读到时，机器指纹必须从文件带出来（否则 cosy-machineid 静默不发）。
        assert_eq!(cr.machine_id, "machine-id-in-file", "指纹必须从文件读回");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⑧ 凭证文件读不到 ⇒ **回退**到从池重建（不整轮跳过）。
    #[test]
    fn cred_falls_back_to_rebuilding_from_the_pool() {
        // 文件不存在。
        let a = qoder_auth("uid-fallback", "D:\\definitely-not-a-real-cred-path.json");
        let cr = billing_cycle_cred_of(&a).expect("文件读不到时应回退重建");
        assert_eq!(cr.uid, "uid-fallback", "回退后 uid 仍是池键");
        assert_eq!(cr.dt, "dt-placeholder", "回退时带上池里的 dt");
        assert_eq!(cr.drt, "drt-placeholder");
        // 区域由域名判定。
        assert_eq!(cr.region, QoderRegion::CN);

        // 空 uid ⇒ None（无法查询）。
        let mut bad = qoder_auth("", "");
        bad.uid = String::new();
        assert!(billing_cycle_cred_of(&bad).is_none(), "空 uid 应返回 None");
    }

    /// ⑧b 回退路径下区域未知 ⇒ 按国服，但**不写回** domain。
    #[test]
    fn fallback_defaults_an_unknown_region_to_cn_without_rewriting_the_domain() {
        let mut a = qoder_auth("uid-unknown-region", "");
        a.domain = String::new();
        let cr = billing_cycle_cred_of(&a).expect("应能回退重建");
        assert_eq!(cr.region, QoderRegion::CN, "未知区域按国服");
    }

    /// 间隔只在处理过至少一个账号之后才等 —— 若第一个 Qoder 账号排在第 3 位，
    /// 前面跳过的两个不该让我们白等 300ms。
    #[tokio::test]
    async fn the_gap_only_applies_after_the_first_processed_account() {
        // 两个非 Qoder 账号排在前面：本轮应**立即**返回，不等 300ms。
        let pool = pool_with(vec![
            Auth { uid: "wb-1".into(), ..Default::default() },
            zcode_auth("zcode-1"),
            qoder_auth("qoder-a", ""),
        ]);
        let store = Arc::new(Stats::new(""));
        let log: FetchLog = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::new(std::sync::Mutex::new(0usize));
        let fetch = fake_fetcher(log, vec![Ok(1_790_000_000_000)], seen);

        let started = std::time::Instant::now();
        let (processed, _) = refresh_billing_cycles_once(&pool, &*fetch, &store, &Cancel::new()).await;
        assert_eq!(processed, 1);
        assert!(
            started.elapsed() < BILLING_CYCLE_GAP,
            "只有一个 Qoder 账号时不该白等间隔（实际 {:?}）",
            started.elapsed()
        );
    }

    /// 取消后立即返回（不继续处理后续账号）。
    #[tokio::test]
    async fn a_cancelled_run_stops_immediately() {
        let pool = pool_with(vec![qoder_auth("qoder-a", ""), qoder_auth("qoder-b", "")]);
        let store = Arc::new(Stats::new(""));
        let log: FetchLog = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::new(std::sync::Mutex::new(0usize));
        let fetch = fake_fetcher(log, vec![Ok(1_790_000_000_000)], seen);

        let cancel = Cancel::new();
        cancel.cancel();
        let (processed, updated) =
            refresh_billing_cycles_once(&pool, &*fetch, &store, &cancel).await;
        assert_eq!((processed, updated), (0, 0), "已取消时不该处理任何账号");
        assert!(store.cycle_of("qoder-a").is_none());
    }

    /// 生产入口：池里有 Qoder 账号时能装上回调。
    #[test]
    fn the_public_entry_builds_a_runner_when_qoder_accounts_exist() {
        let pool = pool_with(vec![qoder_auth("qoder-a", "")]);
        let store = Arc::new(Stats::new(""));
        assert!(
            new_billing_cycle_refresh_runner(pool, store, Cancel::new()).is_some(),
            "池里有 Qoder 账号时应装上回调"
        );
    }

    /// 极简 HTTP/1.1 假上游：收一个请求，记下来，回一份给定响应。
    fn serve_once(
        response_body: &'static str,
        status_line: &'static str,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 回环端口");
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            // 读到 header 结束 + content-length 指定的 body 收满为止。
            loop {
                let n = match sock.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&tmp[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let len: usize = text
                        .lines()
                        .find_map(|l| {
                            let l = l.to_ascii_lowercase();
                            l.strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if buf.len() >= head_end + 4 + len {
                        break;
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

    // ─────────── 端到端：runner → usage.snapshot（②③① 的接缝） ───────────
    //
    // 这是本任务**真正**要证明的一件事：上游给了一个可信的 nextResetAt 之后，
    // `/usage` 的 `billingCycles` 里会出现一行、且 uid 与用量同源。
    //
    // 任务书专门警告过这条接缝：「UID 不同源 ⇒ 周期与用量永远 join 不上 ——
    // 界面显示 0、**无任何错误**」。故这里不用假 fetcher，而是走**真 HTTP**
    //（本地回环假上游）→ `fetch_user_status` → `resolve_next_reset_ms` →
    // `set_billing_cycle` → `snapshot()`，一次把整条链钉住。

    #[tokio::test]
    async fn a_plausible_upstream_value_lands_in_the_usage_snapshot() {
        let reset_ms = 1_793_318_400_000i64;
        let body = format!(
            r#"{{"userType":"teams","plan":"PLAN_TIER_TEAM","nextResetAt":{reset_ms}}}"#
        );
        let body: &'static str = Box::leak(body.into_boxed_str());
        let (base, rx) = serve_once(body, "HTTP/1.1 200 OK");

        // 账号：池键 = uid-pool-key，凭证文件里的 uid 故意写成**别的值**。
        let dir = std::env::temp_dir().join(format!("wbswg-qoder-e2e-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("qoder-e2e.json");
        let path_s = path.to_string_lossy().to_string();
        std::fs::write(
            &path_s,
            r#"{"auth":{"accessToken":"dt-x","refreshToken":"drt-x","domain":"qoder.com.cn"},
                "account":{"uid":"uid-in-file-DIFFERENT"},"machine":{"id":"machineid-e2e"}}"#,
        )
        .unwrap();

        let pool = pool_with(vec![Auth {
            uid: "uid-pool-key".to_string(),
            product: crate::auth::PRODUCT_QODER.to_string(),
            domain: "qoder.com.cn".to_string(),
            access_token: "dt-x".to_string(),
            refresh_token: "drt-x".to_string(),
            file_path: path_s.clone(),
            ..Default::default()
        }]);
        let store = Arc::new(Stats::new(""));

        // 真 HTTP 的 fetcher（base 指向本地假上游）。
        let base_for_fetch = base.clone();
        let fetch: Arc<
            dyn Fn(QoderCred) -> Pin<Box<dyn Future<Output = Result<i64, String>> + Send>>
                + Send
                + Sync,
        > = Arc::new(move |cr: QoderCred| {
            let base = base_for_fetch.clone();
            Box::pin(async move {
                let client = QoderClient::new();
                let st = client.fetch_user_status_with_base(&cr, &base).await?;
                Ok(resolve_next_reset_ms(Some(&st), None))
            }) as Pin<Box<dyn Future<Output = Result<i64, String>> + Send>>
        });

        let (processed, updated) =
            refresh_billing_cycles_once(&pool, &*fetch, &store, &Cancel::new()).await;
        assert_eq!((processed, updated), (1, 1), "应处理 1 个、更新 1 个");

        // ★ 核心断言：周期必须记在**池键**上（不是凭证文件里的那个 uid）。
        assert!(
            store.cycle_of("uid-in-file-DIFFERENT").is_none(),
            "绝不能记在凭证文件的 uid 上 —— 那样与用量 join 不上，且不报错"
        );
        let p = store.cycle_of("uid-pool-key").expect("周期必须记在池键上");
        assert_eq!(p.next_reset_ms, reset_ms);
        assert!(p.known);

        // `/usage` 的快照里必须出现这一行（界面读的就是它）。
        let snap = store.snapshot(30);
        let cycles = snap["billingCycles"]
            .as_object()
            .expect("快照必须带 billingCycles 对象");
        let row = cycles
            .get("uid-pool-key")
            .expect("billingCycles 里必须有一行，键是池键");
        assert_eq!(row["nextResetAtMs"], reset_ms);
        assert!(row.get("cycleStartAtMs").is_some(), "行里应带周期起点");
        assert_eq!(cycles.len(), 1, "只应有一行：{cycles:?}");

        // ★★ 上游收到的 `userId` 必须是**池键**。
        //
        // 这才是「uid 强制对齐」真正防的那件事：凭证文件里的 uid 与池键不同时，
        // 若不对齐就会「查 A 账号的周期、记在 B 账号名下」—— 不报错，两个账号
        // 的数据都不对。写入侧的键由循环变量保证，**查询侧**才是对齐要守的地方。
        let raw = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("应收到上游请求");
        let sent = raw.split("\r\n\r\n").nth(1).expect("应有请求体");
        let decoded = String::from_utf8(
            super::super::encoding::qoder_decode(sent).expect("请求体应是 QoderEncode"),
        )
        .unwrap();
        let outer: Value = serde_json::from_str(&decoded).unwrap();
        let inner = outer["payload"].as_str().unwrap();
        assert!(
            inner.contains(r#""userId":"uid-pool-key""#),
            "发给上游的 userId 必须是池键（否则查的是另一个账号）：{inner}"
        );
        assert!(
            !inner.contains("uid-in-file-DIFFERENT"),
            "绝不能把凭证文件里的 uid 当查询对象：{inner}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 端到端反证：上游**没给**可信值（0）⇒ `/usage` 的 `billingCycles` 保持为空，
    /// 且**不报错**（这正是任务书描述的「界面没数据但不报错」那种状态 ——
    /// 区别在于这里它是**正确**的：上游确实没给）。
    #[tokio::test]
    async fn an_unreported_reset_leaves_the_snapshot_empty_without_an_error() {
        let (base, _rx) = serve_once(r#"{"userType":"teams","plan":"P"}"#, "HTTP/1.1 200 OK");
        let pool = pool_with(vec![qoder_auth("uid-pool-key", "")]);
        let store = Arc::new(Stats::new(""));
        let base_for_fetch = base.clone();
        let fetch: Arc<
            dyn Fn(QoderCred) -> Pin<Box<dyn Future<Output = Result<i64, String>> + Send>>
                + Send
                + Sync,
        > = Arc::new(move |cr: QoderCred| {
            let base = base_for_fetch.clone();
            Box::pin(async move {
                let client = QoderClient::new();
                let st = client.fetch_user_status_with_base(&cr, &base).await?;
                Ok(resolve_next_reset_ms(Some(&st), None))
            }) as Pin<Box<dyn Future<Output = Result<i64, String>> + Send>>
        });

        let (processed, updated) =
            refresh_billing_cycles_once(&pool, &*fetch, &store, &Cancel::new()).await;
        assert_eq!(processed, 1, "账号仍应被处理");
        assert_eq!(updated, 0, "上游没给可信值 ⇒ 不算更新");
        let snap = store.snapshot(30);
        assert!(
            snap["billingCycles"].as_object().unwrap().is_empty(),
            "没有可信周期时不该凭空造一行"
        );
    }
}
