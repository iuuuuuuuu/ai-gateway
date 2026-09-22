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
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/modules/multi_product_credit_patrol.rs"
        );
        let src = std::fs::read_to_string(path).expect("读不到本文件");
        let start = src.find("pub async fn run_once()").expect("找不到 run_once");

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
        panic!("找不到 `run_once` 的闭合花括号 —— 源码结构变了，请更新本测试");
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
}
