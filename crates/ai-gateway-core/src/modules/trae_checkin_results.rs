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

/// 记录当日最终签到状态。
///
/// `entries` 是 `(uid, 昵称, 状态)`；同 uid **覆盖**（重试轮自然合并为最终态）。
pub fn record_today(entries: impl IntoIterator<Item = (String, String, String)>) {
    // 先收集再判空：空输入（例如全部账号都在冷却、一个都没真跑）不该建出
    // 一个 accounts 为空的日期条目 —— 那会在趋势里变成一根 total=0 的柱子，
    // 前端按比例分段时除零得到 NaN。
    let entries: Vec<(String, String, String)> = entries.into_iter().collect();
    if entries.is_empty() {
        return;
    }

    let mut root = load_raw();
    let Some(days) = root.get_mut("days").and_then(Value::as_object_mut) else {
        return;
    };
    let today = today_key();
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

/// 单个趋势点（某一天的汇总计数）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrendPoint {
    pub date: String,
    pub ok: u64,
    pub already: u64,
    pub failed: u64,
}

impl TrendPoint {
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
                        // 旧记录里可能有 "skip"，与失败同样计入未签到
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
    let total = ok + already + failed;

    json!({
        "days": days,
        "points": points.iter().map(TrendPoint::to_json).collect::<Vec<_>>(),
        "summary": {
            "ok": ok,
            "already": already,
            "failed": failed,
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
    fn 全跳过时不建出空日期条目() {
        let _iso = seed("checkin_results_all_skip");
        // 所有账号都在冷却 → 没有任何真实结果传入
        record_today(std::iter::empty());
        let t = trends(Some(7));
        assert_eq!(
            t["points"].as_array().unwrap().len(),
            0,
            "空输入不该造出 total=0 的柱子（前端会除零）：{t}"
        );
        assert_eq!(t["summary"]["observedDays"], json!(0));
    }

    #[test]
    fn 当天的累计不会被空输入清掉() {
        let _iso = seed("checkin_results_keep_after_empty");
        record_today([("u1".to_string(), "a".to_string(), "success".to_string())]);
        // 之后再跑一次全是跳过的签到
        record_today(std::iter::empty());
        assert_eq!(trends(Some(7))["summary"]["ok"], json!(1), "已有记录必须保留");
    }
}
