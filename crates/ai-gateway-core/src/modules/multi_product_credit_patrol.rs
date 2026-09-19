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

    // ---- ZCode ----
    let zcode_uids: Vec<String> = zcode_account::load_accounts()
        .unwrap_or_default()
        .into_iter()
        .map(|a| a.uid)
        .collect();
    for uid in &zcode_uids {
        let u = uid.clone();
        let r = tokio::task::spawn_blocking(move || zcode_login::refresh_account(&u)).await;
        match r {
            Ok(Ok(_)) => ok += 1,
            _ => failed += 1,
        }
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
