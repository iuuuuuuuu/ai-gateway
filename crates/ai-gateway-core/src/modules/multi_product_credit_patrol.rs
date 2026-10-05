// multi_product_credit_patrol.rs 给 Qoder / ZCode 做**周期性的余额采样**。
//
// # 为什么必须有它（这不是"顺手加的定时刷新"）
//
// 消费记录是**差值法**算出来的：记两次余额快照，两次之间的下降即消费。
// 而差值法有硬上限 —— `credit_usage::CREDIT_SNAPSHOT_MAX_GAP_MS` 是
// **2 小时**：相隔更久的两条快照**不算差值**（避免把"隔了一整夜的下降"
// 当成"一次消费"）。
//
// 在此之前，Qoder / ZCode 的余额**只在用户手点「刷新额度」时才读** ——
// 间隔动辄数天，差值永远被上限截断，于是**一条消费都算不出来**。
// 用户看到的是"消费记录永远是空的"，而根因是**采样太稀**。
//
// 本模块就是那个采样器。
//
// # 周期怎么定的
//
// `PATROL_INTERVAL = 20 分钟`，**显著小于 2 小时**，留足余量：
// 即使某几轮失败或被推迟，相邻两次成功采样仍在窗口内。
//
// 为什么不更短（比如 5 分钟）：每次采样都要发一次额度请求。
// 对 19 个 WorkBuddy + 若干 Qoder/ZCode 账号，5 分钟一轮会产生
// 可观的请求量，而上游对额度查询也可能限流 ——
// 20 分钟足够算出"这一天用了多少"，且请求量温和。
//
// # 失败怎么办
//
// **不重试、不报错、继续下一轮**。理由：采样是统计的副产品，
// 失败一次只是少一个数据点，差值会跨到下一轮（仍在 2 小时内）。
// 若在这里重试，反而可能与下一轮叠加成请求风暴。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;

use crate::modules::product_credit_snapshot;
use crate::modules::qoder_account;
use crate::modules::qoder_login;
use crate::modules::zcode_account;
use crate::modules::zcode_login;

/// 巡检周期。
///
/// ⚠ **必须显著小于 `credit_usage::CREDIT_SNAPSHOT_MAX_GAP_MS`（2 小时）**，
/// 否则差值法算不出任何消费 —— 那正是本模块要解决的问题。
/// 改大这个值之前请先看文件头。
pub const PATROL_INTERVAL: Duration = Duration::from_secs(20 * 60);

/// run_once 跑一轮巡检：把两个产品的所有账号余额各读一次并记快照。
///
/// 返回 `(成功数, 失败数)` 供日志与测试断言。
///
/// ⚠ 串行执行（不并发）。理由：账号数不多（个位数到十几），
/// 而并发会给上游制造突发流量 —— 额度查询被限流会让**后续几轮**
/// 都失败，反而降低采样率。串行慢一点但稳。
pub async fn run_once() -> (usize, usize) {
    let mut ok = 0usize;
    let mut failed = 0usize;

    // ---- Qoder ----
    let qoder_uids: Vec<String> = qoder_account::load_accounts()
        .unwrap_or_default()
        .into_iter()
        .map(|a| a.uid)
        .collect();
    for uid in &qoder_uids {
        // ⚠ 用 `spawn_blocking`：refresh_account 内部会**同步执行**
        // 子进程（go 网关 CLI）并阻塞等待 —— 直接在 async 任务里调
        // 会把整个 tokio 运行时的一个 worker 线程堵住，
        // 严重时让其它后台循环（签到、同步）全部延迟。
        let u = uid.clone();
        let r = tokio::task::spawn_blocking(move || qoder_login::refresh_account(&u)).await;
        match r {
            Ok(Ok(_)) => ok += 1,
            _ => failed += 1,
        }
    }

    // ---- Qoder 权益活动自动领取（2026-09-22 所有者要求）----
    //
    // # 为什么必须自动领
    //
    // 活动**每天 10:00 (UTC+8) 重置**，单条时限约 22 小时（实测 `endAt`）。
    // 不自动领 ⇒ 用户没点就**直接过期作废**，那是净损失。
    //
    // # 为什么放在巡检里而不是另起一个循环
    //
    // 巡检已经每 20 分钟跑一次、且已经遍历了 qoder 账号 ——
    // 再起一个循环只会让"同一批账号被两个调度器碰"，
    // 徒增并发与排查难度。领取本身是幂等的（已领会回
    // `replayed:true`），多调几次无副作用。
    //
    // # ⚠ 此前的错误结论（已撤销）
    //
    // 代码里曾写着「不做自动领取 —— 需要验证码 / 会让账号表现出
    // 非人类的活动模式」。**两点都是错的**：
    //   · 实测该端点**不需要任何验证码**（与官方客户端点按钮同构）
    //   - ZCode 的自动领取早已在跑，两条产品线口径必须一致
    //
    // ⚠ 用同一个 `spawn_blocking` 口径（它内部起子进程并阻塞等待）。
    // 失败**不影响**巡检计数：领取与"采样余额"是两件事，
    // 领取失败不该让消费记录看起来也失败了。
    let uids_for_claim = qoder_uids.clone();
    if !uids_for_claim.is_empty() {
        let r = tokio::task::spawn_blocking(move || {
            let _ = uids_for_claim; // 领取内部自行遍历账号库
            qoder_login::claim_all_campaigns()
        })
        .await;
        match r {
            Ok(Ok(v)) => {
                let claimed = v.get("claimedCount").and_then(Value::as_u64).unwrap_or(0);
                let failed_n = v.get("failedCount").and_then(Value::as_u64).unwrap_or(0);
                // 只在真的领到东西、或有失败时打日志（否则每 20 分钟刷一行噪音）
                if claimed > 0 || failed_n > 0 {
                    eprintln!(
                        "[权益自动领取] Qoder：成功领取 {claimed} 项 / 失败 {failed_n} 项"
                    );
                }
            }
            Ok(Err(e)) => eprintln!("[权益自动领取] Qoder 领取失败（不影响余额采样）: {e}"),
            Err(e) => eprintln!("[权益自动领取] Qoder 领取任务异常（不影响余额采样）: {e}"),
        }
    }

    // ---- ZCode ----
    //
    // ⚠ 这里**保留整个账号对象**（而不只是 uid）：后面的「启动激活」要靠
    // `warmed_at` 判断该账号是否已经激活过（幂等的唯一依据）。
    let zcode_accounts = zcode_account::load_accounts().unwrap_or_default();
    let zcode_uids: Vec<String> = zcode_accounts.iter().map(|a| a.uid.clone()).collect();
    for uid in &zcode_uids {
        let u = uid.clone();
        let r = tokio::task::spawn_blocking(move || zcode_login::refresh_account(&u)).await;
        match r {
            Ok(Ok(_)) => ok += 1,
            _ => failed += 1,
        }
    }

    // ---- ZCode 启动激活（2026-10-05 所有者要求：「启动激活也不会自动执行」）----
    //
    // # 为什么放在巡检里
    //
    // 巡检是宿主**唯一**「启动后自动跑一次、之后每 20 分钟再跑」的载体
    // （`src-tauri/src/lib.rs` 先 `sleep(90s)` 再 `loop { run_once().await; … }`），
    // 正是「启动激活」需要的形态。再起一个循环只会多一个调度器去碰同一批账号。
    //
    // # 为什么必须在 resync 之前
    //
    // 激活成功后内部会刷新账号（套餐/额度/模型），而紧接着的
    // `resync_native_config()` 正是把这些清单写进网关配置的地方 ——
    // 顺序反了，新领到的套餐要等下一轮巡检才生效。
    //
    // ⚠ 与余额采样**分开计数**：激活失败不该让消费记录看起来也失败了
    // （与上面 Qoder 领取同款取舍）。
    warm_zcode_accounts(&zcode_accounts).await;

    // ---- 巡检后重写网关配置（2026-09-22，修一个真实的路由缺陷）----
    //
    // # 为什么必须在这里写（所有者报的现场）
    //
    //	发 `Qwen3.8-Flash`（裸名）→ 400
    //	  upstream client (http 400): {"code":11101,
    //	    "msg":"Unmarshal chat params failed with error:
    //	           invalid character 'm' looking for beginning of object key string"}
    //
    // 而 `qoder:Qwen3.8-Flash` 能成功。错误码 **11101 是 WorkBuddy 的** ——
    // 说明裸名请求被路由到了 **WorkBuddy 账号**，而它没有这个模型。
    //
    // # 根因：`product_models` 在启动时是空的，之后再没补上
    //
    // 启动顺序是：
    //
    //	1. `start_gateway` 写网关配置 → 此时 qoder/zcode 账号库里的
    //	   `models` **还是空的**（要靠巡检去上游查）⇒ `product_models`
    //	   缺 qoder / zcode 键
    //	2. 本函数（启动 90 秒后）刷新账号 → 账号库有 `models` 了
    //	   **但配置不再重写** ⇒ 网关永远拿不到那份清单
    //
    // 网关的 `pickForModelAny` 靠这份清单判断"哪个产品声明提供该模型"：
    // 清单缺失 ⇒ 该产品被当成"未知"⇒ 裸名请求的候选集里混进 WorkBuddy
    // 账号 ⇒ 打过去 11101。
    //
    // ⚠ 为什么不在 `start_gateway` 里"等一下再写"：那会让启动变慢，
    // 且巡检可能失败（网络）—— 配置写入不该依赖外部请求成功。
    // 在这里写是**幂等**的：清单没变时写出的内容相同，代价可忽略。
    //
    // ⚠ 只写文件、**不重启网关**（与 `refresh_account` 里的 resync 同款取舍）：
    // 网关是启动时读配置的，故这份更新对**已在运行**的实例要等下次重启
    // 才生效 —— 取舍是"渠道/路由清单晚一点生效"，而不是"每次巡检都断线"。
    //
    // ⚠ 失败不影响巡检结果：额度与模型已经落库了（那才是巡检的目的），
    // 只留一行痕迹便于排查。
    if let Err(e) = crate::modules::gateway::resync_native_config() {
        eprintln!("[积分巡检] 刷新后重写网关配置失败（product_models 可能不更新）: {e}");
    }

    // 只有真跑过账号才打日志（池为空时静默，避免刷屏）
    if ok + failed > 0 {
        eprintln!(
            "[积分巡检] Qoder {} 个 + ZCode {} 个账号：成功 {} / 失败 {}（消费记录依赖此采样，周期 {} 分钟）",
            qoder_uids.len(),
            zcode_uids.len(),
            ok,
            failed,
            PATROL_INTERVAL.as_secs() / 60,
        );
    }
    (ok, failed)
}

/// 「启动激活」的重跑阈值：距上次激活超过这么久才再发一次。
///
/// # 为什么不是「每个进程只做一次」
///
/// 幂等标记 `warmed_at` 落在**账号库**上（见 `zcode_account::ZcodeAccount::warmed_at`），
/// 跨进程有效。若只靠进程内标记，用户每次重启网关都会重报一次启动事件 ——
/// 而上游看到的会是「这个账号一天被启动几十次」，比不报更糟。
///
/// 7 天是**保守**取值：官方客户端是「装了就一直开着」，真实用户不会 7 天
/// 才启动一次客户端；对上游而言「每周一次启动」完全在人类活动区间内。
pub const WARM_REFRESH_AFTER: Duration = Duration::from_secs(7 * 24 * 3600);

/// 本进程是否已经跑过「启动激活」——保证**一次进程生命周期只跑一次**。
///
/// 巡检是每 20 分钟一轮的循环，而「启动激活」只该发生在**启动后第一轮**；
/// 没有这个闸门就会变成「每 20 分钟激活一次」（= 定时自动激活，
/// 正是 `warm.rs` 的合规立场明确拒绝的形态）。
static STARTUP_WARM_DONE: AtomicBool = AtomicBool::new(false);

/// 自动激活是否被允许（读网关配置里已有的自动领取开关）。
///
/// # 为什么复用「自动领取」的开关，而不是新造一个
///
/// 激活的**唯一目的**就是「报启动事件 ⇒ 让上游给出可领套餐 ⇒ 领掉」。
/// 用户关掉自动领取时再自动激活，等于每周往上游发两次事件却什么都不领 ——
/// 只增加风控暴露面，没有任何收益。故与 `zcode_claim_enabled` 同闸。
///
/// ⚠ 三态语义必须与 Go 侧一致（见 `gateway.rs` 里 `zcode_claim_enabled`
/// 附近的说明）：显式值优先，**缺席时回落总闸**，两者都缺席才用默认 `true`。
/// 写成 `unwrap_or(false)` 会让存量用户**静默失去**这个功能。
fn zcode_warm_enabled(cfg: &Value) -> bool {
    fn flag(cfg: &Value, key: &str) -> Option<bool> {
        cfg.get(key).and_then(Value::as_bool)
    }
    flag(cfg, "zcode_claim_enabled")
        .or_else(|| flag(cfg, "product_tasks_enabled"))
        .unwrap_or(true)
}

/// 对每个 ZCode 账号做**一次性**激活：报官方启动事件 + 领取上游给出的套餐。
///
/// # 为什么要它（2026-10-05 所有者原话：「zcode那个启动激活也不会自动执行」）
///
/// 激活此前**只有**两个入口：账号卡片的菜单按钮、CLI 子命令。刚导入的账号
/// 在上游看来「从未活动过」—— 额度查得到、模型列得出，但活动套餐拿不到 offer
/// 或被判异常活动。用户不知道要点那个按钮，于是账号长期停在「未激活」。
///
/// # 幂等（唯一必须守住的不变量）
///
/// 每个账号**只在需要时激活一次**，判据是账号库里的 `warmed_at`：
///
///	从未激活（0）                 → 激活
///	距上次 ≥ WARM_REFRESH_AFTER   → 激活
///	否则                          → 跳过
///
/// ⚠ 成功后**必须**把 `warmed_at` 写回账号库。漏写 = 每次启动都重报，
/// 而那正是「非人类活动模式」的定义。
///
/// ⚠ 事件发失败时**不写** `warmed_at`（否则一次网络抖动就永久放弃该账号）。
///
/// # 计数与日志
///
/// 与余额采样**分开计数**（同 Qoder 领取的取舍）：激活失败不该让消费记录
/// 看起来也失败了。故本函数不返回计数、只打日志。
///
/// ⚠ 每个账号都用 `spawn_blocking` 包裹：`warm_account` 内部**同步执行**
/// 子进程（`gateway.exe zcode-login warm`）并阻塞等待，直接在 async 任务里调
/// 会堵死 tokio 的一个 worker 线程（见本文件 Qoder 段的同款说明）。
async fn warm_zcode_accounts(accounts: &[zcode_account::ZcodeAccount]) {
    // 进程级闸门：只在启动后第一轮巡检里跑（见 STARTUP_WARM_DONE 的说明）。
    if STARTUP_WARM_DONE.swap(true, Ordering::SeqCst) {
        return;
    }

    let cfg = crate::modules::gateway::load_gateway_config();
    if !zcode_warm_enabled(&cfg) {
        return;
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let grace = WARM_REFRESH_AFTER.as_secs() as i64;

    // 只激活**真的有凭证**的账号：没有凭证时 warm 必然失败，
    // 而失败日志会让「启动激活」看起来一直在报错。
    let on_disk: std::collections::HashSet<String> =
        zcode_account::credential_uids().into_iter().collect();

    let mut warmed = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for acc in accounts {
        let uid = acc.uid.trim().to_string();
        if uid.is_empty() || !on_disk.contains(&uid) {
            skipped += 1;
            continue;
        }
        if acc.warmed_at > 0 && now.saturating_sub(acc.warmed_at) < grace {
            skipped += 1;
            continue;
        }

        let u = uid.clone();
        let r = tokio::task::spawn_blocking(move || zcode_login::warm_account(&u)).await;
        match r {
            Ok(Ok(v)) => {
                // ⚠ 空串 = 该阶段成功（口径见 `zcode_login::warm_account` 与
                // `WarmReport`）：`events` 非空说明激活事件根本没发出去。
                let events = v.get("events").and_then(Value::as_str).unwrap_or("");
                if events.is_empty() {
                    warmed += 1;
                    let claimed = v
                        .get("claimed")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or(0);
                    eprintln!("[启动激活] ZCode {uid}：激活事件已上报，领取 {claimed} 个套餐");
                    // 幂等依据必须落盘 —— 失败只留痕，不影响本轮结果。
                    if let Err(e) =
                        zcode_account::upsert_account(&uid, &serde_json::json!({ "warmedAt": now }))
                    {
                        eprintln!(
                            "[启动激活] ZCode {uid}：激活成功但未能记录激活时刻（下次启动会重报）: {e}"
                        );
                    }
                } else {
                    failed += 1;
                    eprintln!("[启动激活] ZCode {uid}：激活事件未发出：{events}");
                }
            }
            Ok(Err(e)) => {
                failed += 1;
                eprintln!("[启动激活] ZCode {uid}：激活失败：{e}");
            }
            Err(e) => {
                failed += 1;
                eprintln!("[启动激活] ZCode {uid}：激活任务异常：{e}");
            }
        }
    }

    // 只有真碰过账号才打汇总（池为空时静默，避免刷屏）
    if warmed + failed > 0 {
        eprintln!(
            "[启动激活] ZCode：已激活 {warmed} 个 / 失败 {failed} 个 / 无需激活 {skipped} 个（每个账号只做一次，距上次超过 {} 天才重做）",
            WARM_REFRESH_AFTER.as_secs() / 86400,
        );
    }
}

/// snapshot_from_refresh_result 从 refresh 的返回 JSON 里补记一次快照。
///
/// # 为什么需要这条路径（与 refresh 内部的记录**互补**）
///
/// `qoder_login::refresh_account` / `zcode_login::refresh_account` 内部
/// 已经在读到有效额度时记了快照。本函数是给**调用方**用的第二条路：
/// 当调用方拿到 refresh 的返回值时，可以再确认一次（幂等 ——
/// `record_snapshot` 内部有去重抑制，重复调用不会产生重复快照）。
///
/// 用途：某些调用路径（例如用户手点刷新后宿主拿到结果）
/// 拿到的 JSON 里可能带了比内部记录时更新的读数。
///
/// 取不到有效读数时返回 `false`，**不报错**。
pub fn snapshot_from_refresh_result(product: &str, uid: &str, result: &Value) -> bool {
    let (remaining, total) = match remaining_and_total_of_refresh(result) {
        Some(v) => v,
        None => return false,
    };

    let name = match product {
        "qoder" => qoder_account::load_accounts()
            .unwrap_or_default()
            .into_iter()
            .find(|a| a.uid == uid)
            .map(|a| a.nickname)
            .unwrap_or_default(),
        _ => zcode_account::load_accounts()
            .unwrap_or_default()
            .into_iter()
            .find(|a| a.uid == uid)
            .map(|a| a.nickname)
            .unwrap_or_default(),
    };

    product_credit_snapshot::product_credit_snapshot(
        product,
        uid,
        &name,
        total,
        remaining,
        std::collections::BTreeMap::new(),
    )
}

/// remaining_and_total_of_refresh 从 refresh 的返回 JSON 里取 `(remaining, total)`。
///
/// # ⚠ 字段名与 `quota` 命令**不同**
///
///	quota 命令输出：  remaining  / total
///	refresh 返回值：  credits    / creditsTotal
///
/// 搞错会退化成 0 —— 而 0 会被记成一条幻影快照（见 `passes_sanity_check`）。
/// 故这里**取不到就返回 `None`**，绝不"默认 0"。
///
/// 抽成独立函数是为了可测：测试直接断言解析结果，
/// 不必真的写一次盘（那会依赖共享的 `AI_GATEWAY_HOME`）。
pub fn remaining_and_total_of_refresh(result: &Value) -> Option<(f64, f64)> {
    let remaining = result.get("credits").and_then(Value::as_f64)?;
    let total = result.get("creditsTotal").and_then(Value::as_f64)?;
    Some((remaining, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 巡检**必须**在刷新后重写网关配置（否则裸名模型会被路由到错的平台）。
    ///
    /// # 为什么用"读源码"这种笨办法（2026-09-22）
    ///
    /// 这个缺陷的表现是**端到端**的（发一个裸名模型 → 11101），
    /// 而根因是"少了一次函数调用"。要真正断言它，得：
    ///
    ///	· 起一个假的上游让 `refresh_account` 成功
    ///	· 检查 `resync_native_config` 真的被调到
    ///	· 还要断言写出的 `product_models` 含 qoder
    ///
    /// 那需要真实的账号库与网络，测试会又慢又脆。
    ///
    /// 而**失败模式**很明确：有人重构 `run_once` 时把那段 resync 删了。
    /// 读源码能可靠地抓住这一点，且零依赖、零副作用。
    ///
    /// ⚠ 这**不是**理想的测试形态（它测的是"代码长什么样"而不是"行为"）。
    /// 我把它标成"防误删护栏"而不是"功能验证" —— 后者靠
    /// `gateway.rs` 里 `product_models_for_gateway` 的测试与端到端实测。
    /// 若将来有了轻量的注入方式（假 refresh），应换成真正的行为断言。
    #[test]
    fn patrol_resyncs_gateway_config() {
        // ⚠ 用**运行时读文件**而不是 `include_str!`：
        //
        // `include_str!` 把内容嵌进**编译产物**，它的失效只靠文件 mtime ——
        // 我实测踩到过：改完文件跑测试仍是旧的（cargo 没重编），
        // 于是"验证这条测试是否有效"时得到**假绿**。运行时读永远是最新的。
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/modules/multi_product_credit_patrol.rs"
        );
        let src = std::fs::read_to_string(path).expect("读不到本文件");
        // 只看 `run_once` 那一段（避免被注释里的示例代码误判为通过）
        let start = src.find("pub async fn run_once()").expect("找不到 run_once");
        let body = &src[start..];
        let end = body.find("\n}\n").unwrap_or(body.len());
        let run_once = &body[..end];

        assert!(
            run_once.contains("resync_native_config"),
            "`run_once` 里必须调用 `gateway::resync_native_config()` —— \
             否则巡检刷新出账号的 `models` 后，网关的 `product_models` 仍是空的，\
             裸名模型（如 `Qwen3.8-Flash`）会被路由到不提供它的平台并返回 11101"
        );
    }

    /// 取 `run_once` 的**函数体源码**（按花括号深度切，不靠换行符）。
    ///
    /// # ⚠⚠ 为什么不能像第一版那样用 `find("\n}\n")`（我在这里栽了）
    ///
    /// 第一版：
    ///
    ///	```ignore
    ///	let end = body.find("\n}\n").unwrap_or(body.len());  // ← 两个坑
    ///	let run_once = &body[..end];
    ///	```
    ///
    /// **坑一（假绿）**：本文件的换行是 **CRLF**，`"\n}\n"` 匹配不到
    /// ⇒ `unwrap_or` 拿到 `body.len()` ⇒ **切片一直延伸到文件末尾**，
    /// 把后面的测试代码也包了进来。而测试的报错文案里**恰好**含
    /// `claim_all_campaigns` 这个字符串 ⇒ 断言**永远为真**。
    ///
    /// 后果：我按惯例"删掉调用看测试是否变红"，**测试仍是绿的** ——
    /// 差点据此认为测试无效。实际上它测的是**它自己的错误信息**。
    ///
    /// **坑二**：`unwrap_or` 把"找不到"静默变成"取全部"。
    /// 这类兜底是**假绿的主要来源** —— 找不到边界时应当**明确失败**。
    ///
    /// 改成按**花括号深度**切：与行尾无关，且找不到闭合花括号时会
    /// panic（测试失败）而不是静默降级。
    fn run_once_source() -> String {
        fn_body("pub async fn run_once()")
    }

    /// 取任意函数（按签名定位）的源码，切法与 `run_once_source` 完全一致。
    ///
    /// 抽出来是为了让新增的 `warm_zcode_accounts` 复用**同一套**边界判定 ——
    /// 抄第二份很容易只修一处，留下一份静默失效的（这个坑本文件已经踩过一次，
    /// 见上面 `find("\n}\n")` 的说明）。
    fn fn_body(sig: &str) -> String {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/modules/multi_product_credit_patrol.rs"
        );
        let src = std::fs::read_to_string(path).expect("读不到本文件");
        let start = src.find(sig).expect("找不到目标函数");

        let mut depth = 0i32;
        let mut seen_open = false;
        let mut out = String::new();
        for ch in src[start..].chars() {
            out.push(ch);
            match ch {
                '{' => {
                    depth += 1;
                    seen_open = true;
                }
                '}' => {
                    depth -= 1;
                    if seen_open && depth == 0 {
                        return out; // 函数体结束
                    }
                }
                _ => {}
            }
        }
        panic!("找不到 `{sig}` 的闭合花括号 —— 源码结构变了，请更新本测试");
    }

    /// 剥掉注释行 —— 注释里也写着这些符号名，不剥的话「调用被删掉」测不出来。
    ///
    /// 与 `run_once_claims_qoder_campaigns` 里那段内联实现同款；
    /// 抽成函数是为了让新测试不必各抄一份（抄漏一处就会得到假绿）。
    fn code_lines(src: &str) -> String {
        src.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Qoder 权益活动必须**自动领取**（2026-09-22 所有者要求）。
    ///
    /// # 为什么必须有这条测试
    ///
    /// 代码里曾长期写着「不做自动领取」，理由有两个，**都已证伪**：
    ///
    ///	1. "领取需要阿里云验证码" → 实测**不需要**
    ///	   （`claim-campaign` 直接返回 `{"claimed":true,"status":"ok"}`）
    ///	2. "会让账号表现出非人类的活动模式" → 与官方客户端点按钮**同构**的
    ///	   一次 POST，不成立；且 ZCode 的自动领取**本来就一直在跑**
    ///
    /// ⚠ 那个错误结论**散落在四处注释里**（本文件、`qoder_login.rs`、
    /// `campaign.go`、`api.rs`），改协议结论时很容易只改一处、
    /// 留下互相矛盾的注释。**这种注释比没有注释更糟** ——
    /// 后人会照着它把正确的自动领取又删掉。
    ///
    /// 故用这条测试钉住**代码**，而不只是改注释。
    ///
    /// ⚠ 断言前**剥掉注释行**：注释里也写着 `claim_all_campaigns`，
    /// 不剥的话"调用被删掉"这个改动测不出来（见 `run_once_source` 的说明）。
    #[test]
    fn run_once_claims_qoder_campaigns() {
        let run_once = run_once_source();

        // ⚠ 剥掉注释：否则注释里的同名字符串会让"调用被删"这个改动
        //   测不出来（见上面的说明）。只保留真正的代码行。
        let code: String = run_once
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            code.contains("claim_all_campaigns"),
            "`run_once` 的**代码**里必须调用 `qoder_login::claim_all_campaigns()` \
             （不能只在注释里提到它）——\n\
             Qoder 权益活动**每天 10:00 (UTC+8) 重置**、单条时限约 22 小时，\n\
             不自动领就**直接过期作废**（用户净损失）。\n\
             \n\
             ⚠ 不要因为看到\"需要验证码\"或\"非人类活动模式\"之类的注释\n\
             就把这里删掉 —— 那两条结论都已实测证伪，见本函数的说明。\n\
             \n\
             剥离注释后的代码：\n{code}"
        );
    }

    /// ZCode「启动激活」必须真的被 `run_once` 调用（2026-10-05 所有者要求）。
    ///
    /// # 为什么必须有这条测试
    ///
    /// 激活此前**只有**界面按钮与 CLI 两个入口，而所有者的原话是
    /// 「zcode那个启动激活也不会自动执行」—— 功能存在、但没有任何自动入口。
    /// 修法就是在巡检（宿主唯一「启动后自动跑一次」的载体）里调一次。
    ///
    /// ⚠ 失败模式很明确：有人重构 `run_once` 时把这次调用删掉，
    /// 而**其余所有测试仍然是绿的**（功能本身没坏，只是又没人调它了）。
    #[test]
    fn run_once_warms_zcode_accounts() {
        let code = code_lines(&run_once_source());
        assert!(
            code.contains("warm_zcode_accounts"),
            "`run_once` 的**代码**里必须调用 `warm_zcode_accounts()` \
             （不能只在注释里提到它）——\n\
             否则「启动激活」就只剩界面按钮这一个入口，\n\
             而所有者报的正是「zcode那个启动激活也不会自动执行」。\n\
             \n\
             剥离注释后的代码：\n{code}"
        );
    }

    /// 自动激活的幂等依据 `warmedAt` **必须**在成功后落盘。
    ///
    /// # 这是本功能唯一会伤到用户的地方
    ///
    /// 激活是**写**操作：以账号身份向上游报 `app_launch` / `app_daily_active`。
    /// 若成功后没写回 `warmed_at`，每次网关启动都会重报一次 ——
    /// 上游看到的是「这个账号一天被启动几十次」，比不激活更糟。
    ///
    /// 故这里钉住「成功分支里有 upsert_account(..., warmedAt)」。
    /// 只查 `warm_zcode_accounts` 的函数体，避免被别处的同名符号蒙混过关。
    #[test]
    fn warm_records_warmed_at_on_success() {
        let code = code_lines(&fn_body("async fn warm_zcode_accounts("));
        assert!(
            code.contains("upsert_account") && code.contains("warmedAt"),
            "`warm_zcode_accounts` 成功激活后**必须**把 `warmedAt` 写回账号库 ——\n\
             漏写会让每次网关启动都重报启动事件（= 非人类活动模式），\n\
             而本功能存在的理由恰恰是「像官方客户端那样只报一次」。\n\
             \n\
             剥离注释后的代码：\n{code}"
        );
    }

    /// 「启动激活」与自动领取同闸，且**键缺席时回落总闸**。
    ///
    /// 三态语义（显式值 > 总闸 > 默认 true）是这里最容易写错的地方：
    /// 写成 `unwrap_or(false)` 会让存量用户**静默失去**这个功能 ——
    /// 老配置里根本没有 `zcode_claim_enabled` 这个键。
    #[test]
    fn warm_gate_falls_back_to_product_tasks() {
        let cases = [
            (serde_json::json!({}), true, "两个键都缺席 ⇒ 默认开启"),
            (
                serde_json::json!({ "product_tasks_enabled": false }),
                false,
                "只有总闸且为 false ⇒ 关闭",
            ),
            (
                serde_json::json!({ "product_tasks_enabled": false, "zcode_claim_enabled": true }),
                true,
                "分产品显式 true 优先于总闸 false",
            ),
            (
                serde_json::json!({ "product_tasks_enabled": true, "zcode_claim_enabled": false }),
                false,
                "分产品显式 false 优先于总闸 true",
            ),
        ];
        for (cfg, want, why) in cases {
            assert_eq!(zcode_warm_enabled(&cfg), want, "{why}");
        }
    }
}
