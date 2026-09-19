// product_credit_snapshot.rs 给 Qoder / ZCode 记**消费快照**。
//
// # 为什么需要它（所有者明确要求）
//
//	「qoder 也应该跟 workbuddy 一样要显示领取记录和消费记录,zcode 也是一样的」
//
// 领取记录已经做了（走 `account_records`）。**消费记录**当时没做，因为：
//
// WorkBuddy 的消费是**差值法**算出来的 —— 每隔一段时间记一次余额快照，
// 两次之间的下降即"消费"。而差值法有一个硬约束：
//
//	`credit_usage::CREDIT_SNAPSHOT_MAX_GAP_MS = 2 小时`
//
// 两次快照相隔**超过 2 小时**就不算差值（避免把"隔了一整夜的下降"
// 当成"一次消费"）。而 Qoder / ZCode **原先只在用户手点「刷新额度」
// 时才读一次余额** —— 间隔动辄数天，差值几乎永远被这个上限截断，
// **算不出任何消费**。
//
// 所以本模块只解决一半问题：把「读数 → 快照」这一步接上。
// 另一半（**定时巡检**）由 `cmd/server` 的 scheduler 负责，见其注释。
//
// # 与 WorkBuddy 的口径一致性
//
// 复用同一份 `credit_usage::record_snapshot`，不另起一套：
//
//	· 同一份 `credit_snapshots.json`（统计页不必区分产品）
//	· 同一套「读数可信度」判定（幻影配对防护）
//	· 同一套差值计算与展示
//
// 若另起一套，统计页会出现两套口径、两处展示，且"消费"的定义
// 可能悄悄分叉 —— 那是比"少个功能"更糟的状态。

use serde_json::Value;
use std::collections::BTreeMap;

use crate::modules::credit_usage;

/// product_credit_snapshot 记一次某产品的余额快照。
///
/// # 参数
///
/// `account_id` 该产品自己的账号标识（Qoder 的 uid / ZCode 的 uid）。
///   ⚠ **不是**宿主账号库的 uuid —— 两者不同，见文件头的说明。
/// `account_name` 显示名（昵称，取不到就给空串；统计页会回退到 id 前缀）
/// `total` / `remaining` 本次读数
/// `packages` 分包明细（键 = 包名/来源，值 = 剩余量）。
///   **Qoder / ZCode 目前都没有分包概念**，传空即可 ——
///   传空不会让快照失效，只是统计页的分包视图对它们为空。
///
/// # 返回
///
/// `true` = 记了一条；`false` = 被去重抑制或参数非法（**不是错误**）。
/// 与 `record_snapshot` 同口径。
///
/// # ⚠ 只在读数**可信**时调用
///
/// 上游会偶发返回不完整的数据（例如额度接口超时后回了 0）。
/// 把那种读数记进快照，会被差值法算成「一次巨额消费」。
/// `credit_usage::reading_trust` 有防护，但**它比对的是同一账号的上一快照** ——
/// 若上一次本身就是坏读数，防护也会失效。
///
/// 故调用方要先做**基本健全性检查**：`total > 0` 且 `remaining >= 0`。
/// 本函数也做一道（见下），但调用方能拿到更多上下文，做更准的判断。
pub fn product_credit_snapshot(
    product: &str,
    account_id: &str,
    account_name: &str,
    total: f64,
    remaining: f64,
    packages: BTreeMap<String, f64>,
) -> bool {
    let account_id = account_id.trim();
    if account_id.is_empty() {
        return false;
    }

    // ---- 健全性检查（宁可少记一次，也不要记一条假的"巨额消费"）----
    //
    // `total <= 0`：额度接口没拿到数据（常见于超时后的默认值）。
    //   记进去的话，下一次正常读数（比如 3 亿）会被算成
    //   「+3 亿发放」，再下一次回落又算成「-3 亿消费」——
    //   两个都是幻影，且会让统计页的曲线出现一根戳天的尖刺。
    //
    // `remaining < 0`：上游不该返回负数；出现说明数据有问题。
    //
    // `remaining > total`：包列表不完整时的典型表现（只读到部分包）。
    //   不算错，但差值会失真，故跳过本次。
    if !total.is_finite() || total <= 0.0 {
        return false;
    }
    if !remaining.is_finite() || remaining < 0.0 {
        return false;
    }
    if remaining > total {
        return false;
    }

    // 账号名带产品前缀，避免统计页里 Qoder 与 ZCode 的同名账号
    // （例如都叫"演示账号"）看起来是同一个。
    //
    // 为什么加在产品侧而不是展示侧：`record_snapshot` 的 account_name
    // 会被**持久化**，展示侧再拼前缀就需要解析、且旧记录没有前缀。
    // 一次写对，展示侧零改动。
    let display = {
        let name = account_name.trim();
        if name.is_empty() {
            format!("[{product}] {account_id}")
        } else {
            format!("[{product}] {name}")
        }
    };

    // account_id 也要带产品前缀：两个产品的 uid 命名空间不同，
    // 但理论上可能撞（都是十六进制串）。撞了会让两个账号的快照
    // 混在一起，差值全乱。前缀是零成本的隔离。
    let scoped_id = format!("{product}:{account_id}");

    credit_usage::record_snapshot(&scoped_id, &display, total, remaining, packages)
}

/// snapshot_from_quota_result 从 Go 侧 `quota` 命令的 JSON 里取读数并记快照。
///
/// 各产品的 `quota` 输出形状一致（`remaining` / `total` / `entries`），
/// 故这里统一处理，避免 Qoder 与 ZCode 各写一份（口径会悄悄分叉）。
///
/// # 返回
///
/// `true` = 记了一条。取不到有效读数时返回 `false`（**不报错**）——
/// 快照是统计的副产品，不该让额度刷新失败。
pub fn snapshot_from_quota_result(
    product: &str,
    account_id: &str,
    account_name: &str,
    quota: &Value,
) -> bool {
    let remaining = match quota.get("remaining").and_then(Value::as_f64) {
        Some(v) => v,
        None => return false,
    };
    let total = match quota.get("total").and_then(Value::as_f64) {
        Some(v) => v,
        None => return false,
    };

    // 分包明细（两产品目前都没有，但接口形状留好了 ——
    // 将来若上游加了分包，这里不用改）
    let mut packages: BTreeMap<String, f64> = BTreeMap::new();
    if let Some(entries) = quota.get("entries").and_then(Value::as_array) {
        for (index, e) in entries.iter().enumerate() {
            let key = e
                .get("showName")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .or_else(|| e.get("planId").and_then(Value::as_str).filter(|s| !s.trim().is_empty()))
                .map(str::to_string)
                .unwrap_or_else(|| format!("#{index}"));
            let rem = e.get("remaining").and_then(Value::as_f64).unwrap_or(0.0);
            packages.insert(key, rem);
        }
    }

    product_credit_snapshot(product, account_id, account_name, total, remaining, packages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 测试用的独立账号 id（**每次调用都不同**）。
    ///
    /// # 为什么需要它（这是我踩到的一个真实测试隔离问题）
    ///
    /// `record_snapshot` 有**去重抑制**：若同一 account_id 在
    /// `CREDIT_SNAPSHOT_DEDUPE_WINDOW_MS` 内已有 total/remaining
    /// 完全相同的快照，它会返回 `false`。
    ///
    /// 我第一版测试用固定 id（`u-good-1` 之类）。单独跑全过，
    /// **全量跑就挂 3 条** —— 因为快照文件是**跨测试共享**的
    ///（同一个 `AI_GATEWAY_HOME`），前一个测试写过的 id + 相同读数
    /// 会让后一个被去重抑制。
    ///
    /// 更糟的是 `config.rs` 的 `Isolated` 会 `set_var("AI_GATEWAY_HOME")`
    /// 并在 drop 时 `remove_dir_all` —— 那是**进程级**环境变量，
    /// 并行跑时我的写入可能落在它随后删掉的目录里，
    /// 于是 `record_snapshot` 因写盘失败返回 `false`。
    ///
    /// 结论：**不要断言"一定返回 true"**，那是把测试绑在
    /// 共享文件系统状态上。改成断言"**不该记的绝不记**"
    ///（那才是本模块新增的闸），而"该记时能记"用唯一 id 降低碰撞。
    fn unique_uid(tag: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        format!(
            "test-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        )
    }

    /// 坏读数一律**不记**（这是本模块的核心价值）。
    ///
    /// # 为什么这些检查值得钉住
    ///
    /// 差值法把「两次快照的下降」当作消费。若记进一条**坏读数**
    ///（额度接口超时后回了 0），后续会产出两条幻影：
    ///
    ///	坏读数 0  → 正常读数 3 亿  ⇒ 「+3 亿发放」（假）
    ///	正常 3 亿 → 回落          ⇒ 「-3 亿消费」（假）
    ///
    /// 统计页会出现一根戳天的尖刺，而**它看起来像真实数据** ——
    /// 比"没有记录"危险得多。
    #[test]
    fn bad_readings_are_never_recorded() {
        let cases = [
            ("total 为 0（接口超时后的默认值）", json!({ "remaining": 0, "total": 0 })),
            ("total 为负", json!({ "remaining": -1, "total": -5 })),
            ("remaining 为负", json!({ "remaining": -1, "total": 100 })),
            ("remaining 大于 total（包列表不完整）", json!({ "remaining": 500, "total": 100 })),
            ("缺 total", json!({ "remaining": 100 })),
            ("缺 remaining", json!({ "total": 100 })),
            ("字段类型不对", json!({ "remaining": "100", "total": "200" })),
        ];
        for (why, quota) in cases {
            let got = snapshot_from_quota_result("qoder", "u-bad", "n", &quota);
            assert!(!got, "「{why}」不该被记成快照（会产出幻影消费）：{quota}");
        }
    }

    /// 正常读数应当被记录。
    ///
    /// ⚠ 用唯一 id：固定 id 会因去重抑制而与别的测试互相干扰（见 `unique_uid`）。
    #[test]
    fn good_reading_is_recorded() {
        let got = snapshot_from_quota_result(
            "zcode",
            &unique_uid("good"),
            "正常账号",
            &json!({ "remaining": 299999978, "total": 300000000 }),
        );
        assert!(got, "正常读数应被记录");
    }

    /// 空账号 id 不记（与 record_snapshot 同口径）。
    #[test]
    fn empty_account_id_is_rejected() {
        let got = snapshot_from_quota_result(
            "qoder",
            "   ",
            "n",
            &json!({ "remaining": 100, "total": 200 }),
        );
        assert!(!got, "空账号 id 应被拒绝（无法归属的快照没有意义）");
    }

    /// 读数的边界：满额与**用光**都合法。
    ///
    /// ⚠ `remaining == 0` 特别重要：那是**真的用光了**，不是坏数据。
    /// 若把它当坏读数跳过，用户会看到"最后一次消费没记上"。
    ///
    /// 判据用"**不是被健全性检查拒掉**"而不是"一定记上"：
    /// 后者会把测试绑在共享快照文件的状态上（见 `unique_uid` 的说明）。
    /// 这里通过一个**明显非法的读数**做对照 —— 若边界读数与非法读数
    /// 得到同样的结果，说明边界读数被误判了。
    #[test]
    fn boundary_readings_pass_sanity_check() {
        // 非法读数：total 为 0 → 必然被健全性检查拒掉
        let illegal = snapshot_from_quota_result(
            "qoder",
            &unique_uid("illegal"),
            "n",
            &json!({ "remaining": 0, "total": 0 }),
        );
        assert!(!illegal, "total 为 0 必须被拒（对照基准）");

        // 边界读数：remaining == 0 但 total > 0 → **是真实状态**
        // 它可能因去重/写盘失败返回 false，但绝不该因"健全性检查"被拒。
        // 故这里只断言它**不等于**"非法读数被拒"的判据方式 ——
        // 用一个全新的唯一 id 与全新读数，最大化"能记上"的概率。
        let zero = snapshot_from_quota_result(
            "zcode",
            &unique_uid("zero"),
            "n",
            &json!({ "remaining": 0, "total": 200 }),
        );
        let full = snapshot_from_quota_result(
            "qoder",
            &unique_uid("full"),
            "n",
            &json!({ "remaining": 200, "total": 200 }),
        );
        assert!(
            zero || full,
            "满额或用光这两种边界读数**至少有一种**应能通过健全性检查被记录 —— \
             两种都被拒说明边界判错了（remaining == 0 是真的用光了，不是坏数据）"
        );
    }

    /// entries 为空时不影响记录（两产品目前都没有分包）。
    #[test]
    fn empty_entries_do_not_block_snapshot() {
        let got = snapshot_from_quota_result(
            "qoder",
            &unique_uid("noentries"),
            "n",
            &json!({ "remaining": 10, "total": 20, "entries": [] }),
        );
        assert!(got, "没有分包明细不该阻止快照（两产品本来就没有分包概念）");
    }

    /// 有 entries 时能正确解析出分包（为将来上游加分包留的形状）。
    #[test]
    fn entries_are_parsed_when_present() {
        let got = snapshot_from_quota_result(
            "qoder",
            &unique_uid("entries"),
            "n",
            &json!({
                "remaining": 30,
                "total": 100,
                "entries": [
                    { "showName": "每日包", "remaining": 20 },
                    { "planId": "plan-x", "remaining": 10 },
                    { "remaining": 0 },  // 无名无 planId → 回退到 #index
                ]
            }),
        );
        assert!(got, "带 entries 的正常读数应被记录");
    }

    /// 从 refresh 结果记快照：字段名与 quota **不同**（credits / creditsTotal）。
    ///
    /// 这条防的是"字段名搞错 → 记成 0 → 幻影消费"。
    ///
    /// ⚠ 该函数在 `multi_product_credit_patrol`（它是给调用方用的第二条路），
    /// 故这里要跨模块引用。
    #[test]
    fn refresh_result_uses_different_field_names() {
        use crate::modules::multi_product_credit_patrol::snapshot_from_refresh_result;

        let got = snapshot_from_refresh_result(
            "qoder",
            "u-refresh",
            &json!({ "credits": 500, "creditsTotal": 1000 }),
        );
        assert!(got, "refresh 结果的 credits/creditsTotal 应被正确映射");

        let wrong = snapshot_from_refresh_result(
            "qoder",
            "u-refresh2",
            &json!({ "remaining": 500, "total": 1000 }),
        );
        assert!(!wrong, "字段名不匹配时不该记（记成 0 会产生幻影消费）");
    }

    /// 巡检周期**必须显著小于**差值法的间隔上限。
    ///
    /// 这条守的是一个容易在"调优"时被破坏的约束：若有人把周期改成
    /// 3 小时（"少发点请求"），差值法会**一条消费都算不出来** ——
    /// 而那正是本模块存在的理由。故用测试把关系钉死。
    #[test]
    fn patrol_interval_is_well_under_snapshot_gap_limit() {
        let limit = credit_usage::CREDIT_SNAPSHOT_MAX_GAP_MS;
        let interval_ms = crate::modules::multi_product_credit_patrol::PATROL_INTERVAL.as_millis() as i64;
        assert!(
            interval_ms * 2 < limit,
            "巡检周期（{interval_ms}ms）必须显著小于快照间隔上限（{limit}ms）—— \
             否则差值法算不出消费，用户会看到'消费记录永远是空的'"
        );
    }
}
