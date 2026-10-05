//! 签到结果按日落库：per-uid 记录每日**最终**签到状态。
//!
//! 落盘 `<store_dir>/trae_checkin_results.json`，保留 90 天自动裁剪。
//!
//! 为什么按天只留一条：签到一天只能成功一次，重试轮会把同 uid 的结果反复改写。
//! 若追加而不是覆盖，趋势图会把「失败 → 重试成功」算成一次失败加一次成功，
//! 成功率凭空变低 —— 用户看到的是「怎么老失败」，实际每次都签上了。

use serde_json::{json, Value};

/// 历史保留天数（超出部分读写时裁剪）。
pub const RETENTION_DAYS: i64 = 90;

fn results_file() -> std::path::PathBuf {
    config_store_dir().join("trae_checkin_results.json")
}

fn config_store_dir() -> std::path::PathBuf {
    crate::modules::config::store_dir()
}

/// 当日日期键（**本机时区**，`YYYY-MM-DD`）。
///
/// 用本机时区而不是 UTC：用户说的「今天签到了吗」指的是自己日历上的今天。
/// 用 UTC 会让 UTC+8 的用户在早上 8 点前看到「今天」还是昨天。
pub fn today_key() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// 读取结果文件（缺失或损坏时回退为空结构）。
fn load_raw() -> Value {
    let path = results_file();
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let parsed: Value = serde_json::from_str(raw.trim_start_matches('\u{feff}'))
        .unwrap_or_else(|_| json!({ "days": {} }));
    if parsed.get("days").and_then(Value::as_object).is_some() {
        parsed
    } else {
        // 结构不对（比如被手工编辑过）时当成空，而不是让上层到处判空
        json!({ "days": {} })
    }
}

fn save_raw(value: &Value) {
    let path = results_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    let _ = crate::modules::config::atomic_write(&path, &text);
}

/// 裁剪保留期之外的日期（`YYYY-MM-DD` 定宽，可直接字典序比较）。
fn trim(days: &mut serde_json::Map<String, Value>) {
    let cutoff = (chrono::Local::now() - chrono::Duration::days(RETENTION_DAYS))
        .format("%Y-%m-%d")
        .to_string();
    days.retain(|d, _| d.as_str() >= cutoff.as_str());
}

/// 该状态是否是**探针**结论（只说明「今天签没签」，不代表本次跑签的结果）。
///
/// 探针结论不能覆盖真实的跑签结果：一次 `fail` 是有价值的信号，
/// 被随手上报的 `not_checked_in` 抹掉就等于把失败藏起来了。
fn is_probe_status(status: &str) -> bool {
    matches!(status, "checked_in" | "not_checked_in")
}

/// 记录当日最终签到状态。
///
/// `entries` 是 `(uid, 昵称, 状态)`；同 uid **覆盖**（重试轮自然合并为最终态）。
///
/// `skip`（冷却中本轮没打上游）**也要记**：调用方过去统一 `filter(status != "skip")`，
/// 结果是「本轮全部跳过」时一个字节都不写，当天的表停留在上一轮的陈旧结论上 ——
/// 界面既看不到跳过，也看不到跳过之前那次真实失败。
pub fn record_today(entries: impl IntoIterator<Item = (String, String, String)>) {
    record_inner(entries.into_iter().collect(), false);
}

/// 记录**探针**结论（`refresh_today` 只读查询的结果）。
///
/// 与 [`record_today`] 的两点不同：
/// 1. 不覆盖当天已有的**真实跑签结果**（`success` / `already` / `fail`）；
/// 2. 一条都写不进去时不建日期条目 —— 探针没改变任何事实，就不该留下痕迹。
pub fn record_probe_today(entries: impl IntoIterator<Item = (String, String, String)>) {
    record_inner(entries.into_iter().collect(), true);
}

fn record_inner(entries: Vec<(String, String, String)>, probe: bool) {
    // 先收集再判空：**真正的一条结果都没有**（没有账号、或调用方传了空集合）时
    // 不该建出 accounts 为空的日期条目 —— 那会在趋势里变成一根 total=0 的柱子，
    // 前端按比例分段时除零得到 NaN。
    //
    // ★ 注意「全部跳过」不是这种情况：跳过是真实结果（带原因、带冷却截止时间），
    // 必须落库，否则当天没有任何痕迹（详见 [`record_today`] 的说明）。
    if entries.is_empty() {
        return;
    }

    let mut root = load_raw();
    let today = today_key();
    if root.get("days").and_then(Value::as_object).is_none() {
        root["days"] = json!({});
    }

    // ★ 先算出真正要写哪些，再决定要不要建日期条目：
    // `days.entry(today)` 会顺手把空壳建出来，而探针很可能一条都不该写。
    let existing = root
        .get("days")
        .and_then(|d| d.get(&today))
        .and_then(|d| d.get("accounts"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let entries: Vec<(String, String, String)> = entries
        .into_iter()
        .filter(|(uid, _, _)| {
            if !probe {
                return true;
            }
            let cur = existing
                .get(uid)
                .and_then(|r| r.get("status"))
                .and_then(Value::as_str)
                .unwrap_or("");
            // 没有记录、或是旧的探针 / skip 结论，都可以被新探针覆盖
            cur.is_empty() || is_probe_status(cur) || cur == "skip"
        })
        .collect();
    if entries.is_empty() {
        return;
    }

    let Some(days) = root.get_mut("days").and_then(Value::as_object_mut) else {
        return;
    };
    let day = days
        .entry(today)
        .or_insert_with(|| json!({ "accounts": {} }));
    if day.get("accounts").and_then(Value::as_object).is_none() {
        *day = json!({ "accounts": {} });
    }
    let now = crate::modules::config::utc_iso();
    if let Some(accounts) = day.get_mut("accounts").and_then(Value::as_object_mut) {
        for (uid, name, status) in entries {
            accounts.insert(
                uid,
                json!({ "name": name, "status": status, "updatedAt": now }),
            );
        }
    }
    trim(days);
    save_raw(&root);
}


/// 从落库状态串派生「今天到底签没签」。
///
/// 三态是刻意的：
/// - `Some(true)`：`success`（本次跑签成功）/ `already`（跑签时上游回答今日已签）/
///   `checked_in`（`refresh_today` 从上游 status 端点直接读到已签）。
/// - `Some(false)`：`not_checked_in`（上游**明确**回答今日未签）。
/// - `None`：其余（`fail`，以及历史遗留的 `skip`）—— **不知道**。
///
/// 为什么 `fail` 不给 `Some(false)`：失败可能是网络异常或上游 9074，账号当天
/// 其实早就签过了，只是这次没问出来。把「没问到」画成「未签到」就是在给用户看
/// 假信息 —— 这正是本函数存在的理由。
fn derive_checked_in(status: &str) -> Option<bool> {
    match status {
        "success" | "already" | "checked_in" => Some(true),
        "not_checked_in" => Some(false),
        _ => None,
    }
}

/// 读某账号**今天**的落库记录。
///
/// 只认 [`today_key`]（本机时区）这一天；没有记录返回 `None`（= 不知道）。
pub fn today_status(uid: &str) -> Option<Value> {
    let root = load_raw();
    let today = today_key();
    let rec = root.get("days")?.get(&today)?.get("accounts")?.get(uid)?;
    let status = rec.get("status").and_then(Value::as_str).unwrap_or("");
    Some(json!({
        "checkedIn": derive_checked_in(status),
        "status": status,
        "name": rec.get("name").and_then(Value::as_str).unwrap_or(""),
        "updatedAt": rec.get("updatedAt").and_then(Value::as_str).unwrap_or(""),
    }))
}

/// 今天的全账号状态表：`{"date": "YYYY-MM-DD", "accounts": {uid: {...}}}`。
///
/// 界面按 uid 查表渲染徽标；**缺键 = 不知道**（不是「未签到」），据此不渲染徽标，
/// 而不是画一个「未签到」去误导用户。
pub fn today_map() -> Value {
    let date = today_key();
    let root = load_raw();
    let accounts = root
        .get("days")
        .and_then(|d| d.get(&date))
        .and_then(|d| d.get("accounts"))
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .map(|(uid, rec)| {
                    let status = rec.get("status").and_then(Value::as_str).unwrap_or("");
                    (
                        uid.clone(),
                        json!({
                            "checkedIn": derive_checked_in(status),
                            "status": status,
                            "name": rec.get("name").and_then(Value::as_str).unwrap_or(""),
                            "updatedAt": rec.get("updatedAt").and_then(Value::as_str).unwrap_or(""),
                        }),
                    )
                })
                .collect::<serde_json::Map<String, Value>>()
        })
        .unwrap_or_default();
    json!({ "date": date, "accounts": accounts })
}
/// 单个趋势点（某一天的汇总计数）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrendPoint {
    pub date: String,
    pub ok: u64,
    pub already: u64,
    pub failed: u64,
    /// 冷却中跳过（本轮没打上游）。
    ///
    /// **不计入** [`Self::total`]：跳过既不是成功也不是失败，算进分母只会无端
    /// 拉低成功率。它必须单独可见 —— 否则「全部跳过」在图上等于什么都没有。
    pub skipped: u64,
}

impl TrendPoint {
    /// 参与成功率计算的样本数（**不含** `skipped`）。
    pub fn total(&self) -> u64 {
        self.ok + self.already + self.failed
    }

    /// 成功率（百分比，无样本时为 `None`）。
    ///
    /// 「已签到」算成功：它表示账号当天确实签上了，只是不是这次跑签的。
    /// 不算进去会把「连续几天都早已签到」显示成 0% 成功率。
    pub fn success_rate(&self) -> Option<f64> {
        let total = self.total();
        if total == 0 {
            return None;
        }
        Some((self.ok + self.already) as f64 * 100.0 / total as f64)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "date": self.date,
            "ok": self.ok,
            "already": self.already,
            "failed": self.failed,
            "skipped": self.skipped,
            "total": self.total(),
            "successRate": self.success_rate(),
        })
    }
}

/// 查询最近 N 天趋势（按日期**升序**）。
///
/// 只返回实际有记录的日期，不补齐缺失的天 —— 签到趋势是柱状图，
/// 补一个 0 高度的柱子看起来像「那天全失败」，实际是那天没跑。
pub fn query_recent(days: u32) -> Vec<TrendPoint> {
    let root = load_raw();
    let Some(days_map) = root.get("days").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut keys: Vec<&String> = days_map.keys().collect();
    keys.sort();
    let skip = keys.len().saturating_sub(days as usize);
    keys.into_iter()
        .skip(skip)
        .map(|date| {
            let mut point = TrendPoint {
                date: date.clone(),
                ok: 0,
                already: 0,
                failed: 0,
                skipped: 0,
            };
            if let Some(accounts) = days_map
                .get(date)
                .and_then(|d| d.get("accounts"))
                .and_then(Value::as_object)
            {
                for rec in accounts.values() {
                    match rec.get("status").and_then(Value::as_str) {
                        Some("success") => point.ok += 1,
                        Some("already") => point.already += 1,
                        // 「已签到」= refresh_today 从上游 status 端点直接读到的确认态，
                        // 与 already 同义：账号当天确实签上了，只是不是这次跑签的。
                        // 不计入 already 会让「打开页面看到已签」变成一根失败柱。
                        Some("checked_in") => point.already += 1,
                        // 冷却中跳过：既没成功也没失败，单列一档。记成失败会让趋势图
                        // 上冒出一堆本不该存在的失败柱（账号只是还在冷却里，当天甚至
                        // 可能早就签过了）。
                        Some("skip") => point.skipped += 1,
                        _ => point.failed += 1,
                    }
                }
            }
            point
        })
        .collect()
}

/// 趋势总览：逐日点 + 区间汇总。
pub fn trends(days: Option<u32>) -> Value {
    // 上限 90 与保留期一致：请求超过保留期的天数没有意义
    let days = days.unwrap_or(30).clamp(1, RETENTION_DAYS as u32);
    let points = query_recent(days);

    let ok: u64 = points.iter().map(|p| p.ok).sum();
    let already: u64 = points.iter().map(|p| p.already).sum();
    let failed: u64 = points.iter().map(|p| p.failed).sum();
    let skipped: u64 = points.iter().map(|p| p.skipped).sum();
    // skipped 不进 total：它既不是成功也不是失败，进分母只会无端拉低成功率
    let total = ok + already + failed;

    json!({
        "days": days,
        "points": points.iter().map(TrendPoint::to_json).collect::<Vec<_>>(),
        "summary": {
            "ok": ok,
            "already": already,
            "failed": failed,
            "skipped": skipped,
            "total": total,
            "observedDays": points.len(),
            // 无样本时给 null 而不是 0：0% 会被读成「全失败」
            "successRate": if total == 0 { Value::Null } else { json!((ok + already) as f64 * 100.0 / total as f64) },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::config::test_isolation::Isolated;

    fn seed(tag: &str) -> Isolated {
        Isolated::new(tag)
    }

    #[test]
    fn 重试轮覆盖为最终态而不是累加() {
        let _iso = seed("checkin_results_overwrite");
        let today = today_key();
        record_today([
            ("u1".to_string(), "账号1".to_string(), "fail".to_string()),
            ("u2".to_string(), "账号2".to_string(), "success".to_string()),
        ]);
        // 重试轮：u1 这次成功
        record_today([("u1".to_string(), "账号1".to_string(), "success".to_string())]);

        let points = query_recent(7);
        assert_eq!(points.len(), 1, "同一天只应有一条记录：{points:?}");
        assert_eq!(points[0].date, today);
        assert_eq!(points[0].ok, 2, "重试成功后应是 2 个成功，而不是 1 成功 1 失败");
        assert_eq!(points[0].failed, 0);
    }

    #[test]
    fn 成功率把已签到算作成功() {
        let _iso = seed("checkin_results_rate");
        record_today([
            ("u1".to_string(), "a".to_string(), "already".to_string()),
            ("u2".to_string(), "b".to_string(), "already".to_string()),
        ]);
        let point = &query_recent(7)[0];
        // 全都「已签到」时成功率必须是 100%，不是 0%
        assert_eq!(point.success_rate(), Some(100.0));
        assert_eq!(point.ok, 0);
        assert_eq!(point.already, 2);
    }

    #[test]
    fn 空历史时成功率为_null_而不是零() {
        let _iso = seed("checkin_results_empty");
        let t = trends(Some(30));
        assert_eq!(t["summary"]["total"], json!(0));
        assert_eq!(t["summary"]["observedDays"], json!(0));
        assert!(
            t["summary"]["successRate"].is_null(),
            "没有样本时成功率必须是 null：{}",
            t["summary"]
        );
        assert_eq!(t["points"].as_array().unwrap().len(), 0, "不该补出 0 高度的柱子");
    }

    #[test]
    fn 趋势按日期升序且只取最近_n_天() {
        let _iso = seed("checkin_results_order");
        // 直接构造多天历史（record_today 只能写今天）
        let mut root = load_raw();
        let days = root.get_mut("days").unwrap().as_object_mut().unwrap();
        for d in ["2026-01-01", "2026-01-02", "2026-01-03", "2026-01-04"] {
            days.insert(
                d.to_string(),
                json!({ "accounts": { "u1": { "name": "a", "status": "success", "updatedAt": "" } } }),
            );
        }
        save_raw(&root);

        let points = query_recent(2);
        assert_eq!(points.len(), 2, "只应返回最近 2 天");
        assert_eq!(points[0].date, "2026-01-03", "必须按日期升序");
        assert_eq!(points[1].date, "2026-01-04");
    }

    #[test]
    fn 超过保留期的记录被裁掉() {
        let _iso = seed("checkin_results_trim");
        let mut root = load_raw();
        let days = root.get_mut("days").unwrap().as_object_mut().unwrap();
        days.insert(
            "2000-01-01".to_string(),
            json!({ "accounts": { "old": { "name": "旧", "status": "success", "updatedAt": "" } } }),
        );
        save_raw(&root);

        // 任意一次写入都应触发裁剪
        record_today([("u1".to_string(), "a".to_string(), "success".to_string())]);
        let kept = load_raw();
        assert!(
            kept["days"].get("2000-01-01").is_none(),
            "超出 90 天的记录应被裁剪：{kept}"
        );
        assert!(kept["days"].get(&today_key()).is_some(), "今天的记录必须保留");
    }

    #[test]
    fn 损坏的结果文件被当成空历史() {
        let _iso = seed("checkin_results_broken");
        let path = results_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json at all").unwrap();
        let t = trends(Some(7));
        assert_eq!(t["summary"]["total"], json!(0));

        // 结构不对（缺少 days）也要能恢复
        std::fs::write(&path, json!({ "something": 1 }).to_string()).unwrap();
        record_today([("u1".to_string(), "a".to_string(), "success".to_string())]);
        assert_eq!(trends(Some(7))["summary"]["ok"], json!(1));
    }

    #[test]
    fn 未知状态计入失败而不是被静默丢弃() {
        let _iso = seed("checkin_results_unknown");
        let mut root = load_raw();
        root["days"]["2026-02-01"] =
            json!({ "accounts": { "u1": { "name": "a", "status": "weird", "updatedAt": "" } } });
        save_raw(&root);
        let point = &query_recent(7)[0];
        assert_eq!(point.failed, 1, "未知状态必须可见，不能被丢掉让总数对不上");
        assert_eq!(point.skipped, 0, "只有显式的 skip 才进跳过档");
        assert_eq!(point.total(), 1);
    }

    #[test]
    fn 趋势天数被夹在保留期内() {
        let _iso = seed("checkin_results_clamp");
        assert_eq!(trends(Some(100_000))["days"], json!(RETENTION_DAYS));
        assert_eq!(trends(Some(0))["days"], json!(1));
        assert_eq!(trends(None)["days"], json!(30));
    }

    #[test]
    fn 空输入不建出空日期条目() {
        let _iso = seed("checkin_results_empty_input");
        // 一条结果都没有（没有账号、或调用方传了空集合）→ 不建日期条目，
        // 否则趋势里会出现一根 total=0 的柱子（前端按比例分段时除零）。
        // ★ 注意这与「全部跳过」不同：跳过是真实结果，必须落库（见下一个测试）。
        record_today(std::iter::empty());
        let t = trends(Some(7));
        assert_eq!(
            t["points"].as_array().unwrap().len(),
            0,
            "空输入不该造出 total=0 的柱子（前端会除零）：{t}"
        );
        assert_eq!(t["summary"]["observedDays"], json!(0));
    }

    /// 冷却中跳过：单独计一档，既不是失败也不进成功率的分母。
    ///
    /// 旧口径把 skip 归进 failed（代码注释里也写着「与失败同样计入未签到」），
    /// 于是「账号在冷却里」被显示成一次签到失败。
    #[test]
    fn 跳过单独计一档且不进成功率分母() {
        let _iso = seed("checkin_results_skip_bucket");
        record_today([
            ("u1".to_string(), "a".to_string(), "success".to_string()),
            ("u2".to_string(), "b".to_string(), "skip".to_string()),
        ]);
        let t = trends(Some(7));
        let point = &t["points"].as_array().unwrap()[0];
        assert_eq!(point["skipped"], json!(1));
        assert_eq!(point["failed"], json!(0), "跳过不是失败");
        assert_eq!(point["ok"], json!(1));
        assert_eq!(point["total"], json!(1), "跳过不进 total");
        assert_eq!(point["successRate"], json!(100.0), "跳过不进分母");
        assert_eq!(t["summary"]["skipped"], json!(1));
        assert_eq!(t["summary"]["total"], json!(1));
    }

    /// 当天**全部**跳过时也必须留下一条记录（而不是空白）。
    ///
    /// 旧口径下三个调用点都 `filter(status != "skip")`，全跳过等于什么都没写，
    /// 当天的落库表停留在上一轮的陈旧结论上，趋势图里连一根柱子都没有。
    #[test]
    fn 全部跳过时当天仍然可见() {
        let _iso = seed("checkin_results_all_skip");
        record_today([
            ("u1".to_string(), "a".to_string(), "skip".to_string()),
            ("u2".to_string(), "b".to_string(), "skip".to_string()),
        ]);
        let t = trends(Some(7));
        let points = t["points"].as_array().unwrap();
        assert_eq!(points.len(), 1, "全跳过也要画出一根柱子：{t}");
        assert_eq!(points[0]["skipped"], json!(2));
        assert_eq!(points[0]["total"], json!(0), "没有成功/失败样本");
        assert_eq!(
            points[0]["successRate"],
            Value::Null,
            "无样本 → null 而不是 0"
        );
        assert_eq!(t["summary"]["observedDays"], json!(1));
        assert_eq!(t["summary"]["successRate"], Value::Null);
    }

    #[test]
    fn 当天的累计不会被空输入清掉() {
        let _iso = seed("checkin_results_keep_after_empty");
        record_today([("u1".to_string(), "a".to_string(), "success".to_string())]);
        // 之后再跑一次全是跳过的签到
        record_today(std::iter::empty());
        assert_eq!(trends(Some(7))["summary"]["ok"], json!(1), "已有记录必须保留");
    }

    #[test]
    fn 今日状态三态派生() {
        let _iso = seed("checkin_results_today_status");
        record_today([
            ("u1".to_string(), "签上了".to_string(), "success".to_string()),
            ("u2".to_string(), "已签".to_string(), "already".to_string()),
            ("u3".to_string(), "上游确认已签".to_string(), "checked_in".to_string()),
            ("u4".to_string(), "上游确认没签".to_string(), "not_checked_in".to_string()),
            ("u5".to_string(), "失败".to_string(), "fail".to_string()),
        ]);

        assert_eq!(today_status("u1").unwrap()["checkedIn"], json!(true));
        assert_eq!(today_status("u2").unwrap()["checkedIn"], json!(true));
        assert_eq!(today_status("u3").unwrap()["checkedIn"], json!(true));
        assert_eq!(today_status("u4").unwrap()["checkedIn"], json!(false));
        // ★ 失败必须给 null：网络异常 / 9074 都可能让「其实已签」的账号记成 fail，
        // 画成 false 就是在告诉用户「未签到」，那是假信息。
        assert_eq!(
            today_status("u5").unwrap()["checkedIn"],
            Value::Null,
            "失败不是「未签到」，是「不知道」"
        );
        assert!(today_status("没这个账号").is_none(), "缺记录 = 不知道，而不是未签到");
    }

    #[test]
    fn 今日状态表只含今天且缺键表示不知道() {
        let _iso = seed("checkin_results_today_map");
        // 昨天有一条、今天没有 —— 徽标不得沿用昨天的结论
        let mut root = load_raw();
        let yesterday = (chrono::Local::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        root["days"][&yesterday] = json!({ "accounts": { "u-old": { "name": "旧", "status": "checked_in", "updatedAt": "" } } });
        save_raw(&root);

        let m = today_map();
        assert_eq!(m["date"], json!(today_key()));
        assert_eq!(
            m["accounts"].as_object().unwrap().len(),
            0,
            "昨天的记录不得被当成今天的状态：{m}"
        );

        record_today([("u-new".to_string(), "新".to_string(), "checked_in".to_string())]);
        let m = today_map();
        assert_eq!(m["accounts"]["u-new"]["checkedIn"], json!(true));
        assert!(m["accounts"].get("u-old").is_none(), "今天没跑过的账号必须缺键");
    }

    #[test]
    fn 上游确认已签计入成功列而不是失败列() {
        let _iso = seed("checkin_results_checked_in_column");
        record_today([
            ("u1".to_string(), "a".to_string(), "checked_in".to_string()),
            ("u2".to_string(), "b".to_string(), "not_checked_in".to_string()),
        ]);
        let point = &query_recent(7)[0];
        assert_eq!(point.already, 1, "上游确认已签应计入「已签」列");
        assert_eq!(point.failed, 1, "上游确认没签才算未完成");
        assert_eq!(point.success_rate(), Some(50.0));
    }

    #[test]
    fn 只读探针不覆盖当天真实跑签结果() {
        let _iso = seed("checkin_results_probe_no_clobber");
        // 一键签到真跑过：u1 失败、u2 成功
        record_today([
            ("u1".to_string(), "失败号".to_string(), "fail".to_string()),
            ("u2".to_string(), "成功号".to_string(), "success".to_string()),
        ]);
        // 用户随后打开 Trae 页，探针上报「今天没签」
        record_probe_today([
            ("u1".to_string(), "失败号".to_string(), "not_checked_in".to_string()),
            ("u2".to_string(), "成功号".to_string(), "not_checked_in".to_string()),
        ]);

        // ★ 这是本轮修的核心：失败必须留着，否则趋势图会把失败藏起来
        assert_eq!(
            today_status("u1").unwrap()["status"],
            json!("fail"),
            "探针不得抹掉当天的失败记录"
        );
        assert_eq!(today_status("u2").unwrap()["status"], json!("success"));
        assert_eq!(query_recent(7)[0].failed, 1, "失败柱必须还在");
    }

    #[test]
    fn 只读探针在当天没有真实结果时照常落库() {
        let _iso = seed("checkin_results_probe_only");
        // 当天还没有任何真实结果：探针结论就是当前最好信息，应当落库
        record_probe_today([("u1".to_string(), "a".to_string(), "not_checked_in".to_string())]);
        assert_eq!(today_status("u1").unwrap()["checkedIn"], json!(false));

        // 探针之间可以互相覆盖（都是探针结论）
        record_probe_today([("u1".to_string(), "a".to_string(), "checked_in".to_string())]);
        assert_eq!(today_status("u1").unwrap()["checkedIn"], json!(true));

        // 空输入不得建出空壳日期条目
        let before = trends(Some(7))["summary"]["observedDays"].clone();
        record_probe_today(std::iter::empty());
        assert_eq!(trends(Some(7))["summary"]["observedDays"], before);
    }
}
