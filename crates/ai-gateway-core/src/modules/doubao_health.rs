//! 豆包运维健康史（`<store_dir>/doubao_health.json`）。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/commands/doubao.rs` 的 `doubao_history`
//! 一节（`HISTORY_MAX = 400`、事件 schema、`windows_of_parsed`），按本仓库
//! 「JSON 文件 + 原子写」的存储约定改写。
//!
//! ## 为什么需要它
//!
//! 保活 / 续期 / 额度巡检都是**后台静默**动作：跑成功了用户看不到，跑失败了
//! 用户也看不到 —— 直到某天发现账号掉登录。池级的 `last_keepalive_at` 只有一个
//! 时间点，回答不了「这周续期成功过几次」「额度是什么时候开始掉到 20% 的」。
//! 因此把三类动作逐次记成事件，供趋势图与健康卡消费。
//!
//! ## 为什么上限 400 条
//!
//! 三类事件每天最多各一条，400 条 ≈ 4 个月，足够画 14 天趋势与 7 天健康卡，
//! 同时把文件压在几十 KB 量级（每次读写都是整文件，不能无限长）。
//!
//! ## 为什么额度百分比从 `windows` 取而不是 `items`
//!
//! 本仓库 [`crate::modules::doubao_quota::ParsedQuota`] 有两条解析路径：
//! `windows` 是严格路径（camelCase `usedPercent`），`items` 是宽松兜底
//! （只保证拿到等级/名称，**不含百分比**）。上游参考项目从 `items` 取
//! `used_percent` 是它自己的历史包袱；照搬到这里只会得到一片空趋势。

use serde_json::{json, Value};

use crate::modules::config;

/// 事件条数上限（超出后丢弃最旧的）。
pub const HISTORY_MAX: usize = 400;

/// 事件类型。
pub const KIND_KEEPALIVE: &str = "keepalive";
pub const KIND_RENEW: &str = "renew";
pub const KIND_QUOTA: &str = "quota";

/// 健康史文件路径。
pub fn history_file() -> std::path::PathBuf {
    config::store_dir().join("doubao_health.json")
}

/// 读取全部事件（最旧在前，与写入顺序一致）。
pub fn load() -> Vec<Value> {
    let path = history_file();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| {
            serde_json::from_str::<Value>(t.trim_start_matches('\u{feff}')).ok()
        })
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

fn save(events: &[Value]) -> Result<(), String> {
    let text = serde_json::to_string_pretty(events).map_err(|e| e.to_string())?;
    config::atomic_write(&history_file(), &text).map_err(|e| e.to_string())
}

/// 追加一条事件；超出 [`HISTORY_MAX`] 时裁掉最旧的。
///
/// 返回 `Err` 只用于「写盘失败」——事件本身记不下来不该让整个保活流程失败，
/// 因此调用方通常忽略返回值（见 `apps_ops` 的用法）。
pub fn append(event: Value) -> Result<(), String> {
    let mut events = load();
    events.push(event);
    if events.len() > HISTORY_MAX {
        let drop = events.len() - HISTORY_MAX;
        events.drain(0..drop);
    }
    save(&events)
}

/// 记一条保活事件。
pub fn record_keepalive(ok: bool, summary: &str) -> Result<(), String> {
    append(json!({
        "at": config::utc_iso(),
        "kind": KIND_KEEPALIVE,
        "ok": ok,
        "summary": summary,
    }))
}

/// 记一条续期事件（整体汇总，不逐账号）。
pub fn record_renew(summary: &Value) -> Result<(), String> {
    let ok = summary.get("errors").and_then(Value::as_i64).unwrap_or(0) == 0
        && summary.get("expired").and_then(Value::as_i64).unwrap_or(0) == 0;
    append(json!({
        "at": config::utc_iso(),
        "kind": KIND_RENEW,
        "ok": ok,
        "uid": Value::Null,
        "summary": format!(
            "成功 {} / 已过期 {} / 跳过 {} / 失败 {}",
            summary.get("ok").and_then(Value::as_i64).unwrap_or(0),
            summary.get("expired").and_then(Value::as_i64).unwrap_or(0),
            summary.get("skipped").and_then(Value::as_i64).unwrap_or(0),
            summary.get("errors").and_then(Value::as_i64).unwrap_or(0),
        ),
    }))
}

/// 记一条额度事件（带窗口快照，供趋势图）。
///
/// `windows` 直接取 [`crate::modules::doubao_quota::ParsedQuota::windows`]。
pub fn record_quota(
    ok: bool,
    uid: &str,
    level: &str,
    summary: &str,
    windows: &[Value],
) -> Result<(), String> {
    append(json!({
        "at": config::utc_iso(),
        "kind": KIND_QUOTA,
        "ok": ok,
        "uid": uid,
        "level": level,
        "summary": summary,
        // 只保留画趋势需要的三个字段，避免把整份响应抄进历史文件
        "windows": windows.iter().map(|w| json!({
            "name": w.get("name").cloned().unwrap_or(Value::Null),
            "usedPercent": w.get("usedPercent").cloned().unwrap_or(Value::Null),
            "exhausted": w.get("exhausted").cloned().unwrap_or(Value::Null),
            "resetAt": w.get("resetAt").cloned().unwrap_or(Value::Null),
        })).collect::<Vec<Value>>(),
    }))
}

/// 「当前时段」窗口的已用百分比（趋势图取这个值）。
///
/// 为什么只认「当前时段」：豆包的额度窗口有多个（当前时段 / 本周 / 本月），
/// 混在一起画线会得到一条锯齿状的假趋势 —— 三个窗口的用量本来就不在同一量纲。
/// 上游参考项目同样是按窗口名过滤的。
pub fn current_window_used_percent(windows: &[Value]) -> Option<f64> {
    windows
        .iter()
        .find(|w| {
            w.get("name")
                .and_then(Value::as_str)
                .map(|n| n.contains("当前时段") || n.contains("当前"))
                .unwrap_or(false)
        })
        .and_then(|w| w.get("usedPercent"))
        .and_then(Value::as_f64)
}

/// 近 `days` 天的额度趋势点：`[{date, usedPercent, level, uid}]`（按日期升序）。
///
/// 同一天多条事件取**最后一条**：一天内可能手动查一次、定时任务又查一次，
/// 取最后一条才代表当天收尾时的真实状态。
pub fn quota_trend(days: i64, now: chrono::DateTime<chrono::Utc>) -> Vec<Value> {
    let cutoff = now - chrono::Duration::days(days.max(1));
    let mut by_day: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
    for e in load() {
        if e.get("kind").and_then(Value::as_str) != Some(KIND_QUOTA) {
            continue;
        }
        if e.get("ok").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let Some(at) = e.get("at").and_then(Value::as_str) else {
            continue;
        };
        let Some(ts) = parse_iso_ts(at) else { continue };
        if ts < cutoff {
            continue;
        }
        let windows = e
            .get("windows")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let Some(used) = current_window_used_percent(&windows) else {
            continue;
        };
        by_day.insert(
            at.chars().take(10).collect(),
            json!({
                "date": at.chars().take(10).collect::<String>(),
                "usedPercent": used,
                "level": e.get("level").cloned().unwrap_or(Value::Null),
                "uid": e.get("uid").cloned().unwrap_or(Value::Null),
            }),
        );
    }
    by_day.into_values().collect()
}

/// 近 `days` 天三类事件的计数：`{keepalive, renew, quota, ok, failed}`。
pub fn health_counts(days: i64, now: chrono::DateTime<chrono::Utc>) -> Value {
    let cutoff = now - chrono::Duration::days(days.max(1));
    let mut keepalive = 0i64;
    let mut renew = 0i64;
    let mut quota = 0i64;
    let mut ok = 0i64;
    let mut failed = 0i64;
    let mut last_keepalive: Option<String> = None;
    for e in load() {
        let Some(at) = e.get("at").and_then(Value::as_str) else {
            continue;
        };
        let Some(ts) = parse_iso_ts(at) else { continue };
        if ts < cutoff {
            continue;
        }
        if e.get("ok").and_then(Value::as_bool) == Some(true) {
            ok += 1;
        } else {
            failed += 1;
        }
        match e.get("kind").and_then(Value::as_str) {
            Some(KIND_KEEPALIVE) => {
                keepalive += 1;
                if last_keepalive.as_deref().map(|p| at > p).unwrap_or(true) {
                    last_keepalive = Some(at.to_string());
                }
            }
            Some(KIND_RENEW) => renew += 1,
            Some(KIND_QUOTA) => quota += 1,
            _ => {}
        }
    }
    json!({
        "days": days,
        "keepalive": keepalive,
        "renew": renew,
        "quota": quota,
        "ok": ok,
        "failed": failed,
        "lastKeepaliveAt": last_keepalive,
    })
}

/// 解析时间串为 UTC 时刻。
///
/// 需要容忍两种形态，因为本仓库两处时间源格式不同：
/// - [`config::utc_iso`] 产出 `YYYY-MM-DDTHH-MM-SSZ`（**时间里是连字符**，
///   沿用 Python 版 `utc_iso` 的形态）；
/// - RFC3339 `YYYY-MM-DDTHH:MM:SSZ`（冒号，手工写入或未来切换格式时）。
///
/// 只认 RFC3339 的话，本仓库自己写的事件会被全部丢弃 —— 趋势图和健康卡恒为空，
/// 且不报错（静默失效）。
fn parse_iso_ts(text: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let t = text.trim();
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(t) {
        return Some(d.with_timezone(&chrono::Utc));
    }
    for fmt in [
        "%Y-%m-%dT%H-%M-%SZ",
        "%Y-%m-%dT%H-%M-%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
    ] {
        if let Ok(d) = chrono::NaiveDateTime::parse_from_str(t, fmt) {
            return Some(d.and_utc());
        }
    }
    None
}

/// 一次完整的健康摘要（供 `doubao_history` 命令直接返回）。
pub fn snapshot(days: i64) -> Value {
    let now = chrono::Utc::now();
    json!({
        "events": load(),
        "trend": quota_trend(14, now),
        "health": health_counts(7, now),
        "requestedDays": days,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::config::test_isolation::Isolated;

    fn iso_days_ago(days: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::days(days))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    }

    fn quota_event(days_ago: i64, used: f64, ok: bool) -> Value {
        json!({
            "at": iso_days_ago(days_ago),
            "kind": KIND_QUOTA,
            "ok": ok,
            "uid": "u-1",
            "level": "Pro",
            "windows": [
                {"name": "当前时段", "usedPercent": used},
                {"name": "本周", "usedPercent": 10.0}
            ]
        })
    }

    #[test]
    fn 追加与读取() {
        let _iso = Isolated::new("dbhealth-append");
        assert!(load().is_empty());
        record_keepalive(true, "保活成功").unwrap();
        record_keepalive(false, "保活失败").unwrap();
        let events = load();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["kind"], KIND_KEEPALIVE);
        assert_eq!(events[0]["ok"], true);
        assert_eq!(events[1]["ok"], false);
        assert!(events[0]["at"].as_str().unwrap().contains('T'));
    }

    /// 关键回归：上限裁剪必须丢最旧的，不能丢最新的。
    #[test]
    fn 超出上限丢弃最旧事件() {
        let _iso = Isolated::new("dbhealth-cap");
        for i in 0..(HISTORY_MAX + 1) {
            record_keepalive(true, &format!("第 {i} 次")).unwrap();
        }
        let events = load();
        assert_eq!(events.len(), HISTORY_MAX);
        assert_eq!(
            events[0]["summary"], "第 1 次",
            "应裁掉第 0 条，保留最新的 400 条"
        );
        assert_eq!(
            events[HISTORY_MAX - 1]["summary"],
            format!("第 {HISTORY_MAX} 次")
        );
    }

    #[test]
    fn 额度百分比只认当前时段窗口() {
        let windows = vec![
            json!({"name": "本周", "usedPercent": 10.0}),
            json!({"name": "当前时段", "usedPercent": 42.5}),
        ];
        assert_eq!(current_window_used_percent(&windows), Some(42.5));
        // 没有当前时段窗口 → None（不得退化成拿第一个窗口顶替）
        assert_eq!(
            current_window_used_percent(&[json!({"name": "本月", "usedPercent": 1.0})]),
            None
        );
        assert_eq!(current_window_used_percent(&[]), None);
    }

    #[test]
    fn 额度事件不抄整份响应() {
        let _iso = Isolated::new("dbhealth-trim");
        let windows = vec![json!({
            "name": "当前时段",
            "usedPercent": 33.0,
            "exhausted": false,
            "resetAt": "2026-01-01",
            "extraJunk": "x".repeat(500),
        })];
        record_quota(true, "u-1", "Pro", "ok", &windows).unwrap();
        let e = &load()[0];
        let w = &e["windows"][0];
        assert!(w.get("extraJunk").is_none(), "只保留画趋势所需字段");
        assert_eq!(w["usedPercent"], 33.0);
        assert_eq!(w["name"], "当前时段");
    }

    #[test]
    fn 趋势按天取最后一条且升序() {
        let _iso = Isolated::new("dbhealth-trend");
        // 同一天两条：取后写入的那条
        let mut older = quota_event(1, 10.0, true);
        older["at"] = json!(iso_days_ago(1));
        append(older).unwrap();
        let mut newer = quota_event(1, 55.0, true);
        newer["at"] = json!(iso_days_ago(1));
        append(newer).unwrap();
        append(quota_event(0, 60.0, true)).unwrap();
        // 超出窗口的应被排除
        append(quota_event(30, 99.0, true)).unwrap();

        let trend = quota_trend(14, chrono::Utc::now());
        assert_eq!(trend.len(), 2, "只应含近 14 天的两个日期");
        assert_eq!(trend[0]["usedPercent"], 55.0, "同一天取最后一条");
        assert_eq!(trend[1]["usedPercent"], 60.0);
        let dates: Vec<&str> = trend.iter().filter_map(|p| p["date"].as_str()).collect();
        let mut sorted = dates.clone();
        sorted.sort();
        assert_eq!(dates, sorted, "趋势必须按日期升序");
    }

    #[test]
    fn 趋势跳过失败事件与无百分比事件() {
        let _iso = Isolated::new("dbhealth-trend-skip");
        append(quota_event(1, 20.0, false)).unwrap();
        append(json!({
            "at": iso_days_ago(1),
            "kind": KIND_QUOTA, "ok": true, "uid": "u-1",
            "windows": [{"name": "本周", "usedPercent": 5.0}]
        }))
        .unwrap();
        assert!(quota_trend(14, chrono::Utc::now()).is_empty());
    }

    #[test]
    fn 健康计数分类与时间窗() {
        let _iso = Isolated::new("dbhealth-count");
        record_keepalive(true, "ok").unwrap();
        record_keepalive(false, "fail").unwrap();
        record_renew(&json!({"ok": 2, "expired": 0, "skipped": 1, "errors": 0})).unwrap();
        append(quota_event(1, 30.0, true)).unwrap();
        // 窗口外
        append(json!({
            "at": iso_days_ago(30), "kind": KIND_KEEPALIVE, "ok": true, "summary": "旧"
        }))
        .unwrap();

        let h = health_counts(7, chrono::Utc::now());
        assert_eq!(h["keepalive"], 2, "窗口外的不计入");
        assert_eq!(h["renew"], 1);
        assert_eq!(h["quota"], 1);
        assert_eq!(h["ok"], 3);
        assert_eq!(h["failed"], 1);
        assert!(h["lastKeepaliveAt"].is_string());
    }

    #[test]
    fn 续期事件的成功判定() {
        let _iso = Isolated::new("dbhealth-renew");
        record_renew(&json!({"ok": 3, "expired": 0, "skipped": 0, "errors": 0})).unwrap();
        assert_eq!(load()[0]["ok"], true);
        record_renew(&json!({"ok": 3, "expired": 1, "skipped": 0, "errors": 0})).unwrap();
        assert_eq!(load()[1]["ok"], false, "有账号过期不算整体成功");
        record_renew(&json!({"ok": 3, "expired": 0, "skipped": 0, "errors": 2})).unwrap();
        assert_eq!(load()[2]["ok"], false, "有请求失败不算整体成功");
    }

    #[test]
    fn 快照含事件趋势与健康() {
        let _iso = Isolated::new("dbhealth-snapshot");
        append(quota_event(1, 25.0, true)).unwrap();
        record_keepalive(true, "ok").unwrap();
        let s = snapshot(30);
        assert_eq!(s["events"].as_array().unwrap().len(), 2);
        assert_eq!(s["trend"].as_array().unwrap().len(), 1);
        assert_eq!(s["health"]["keepalive"], 1);
        assert_eq!(s["requestedDays"], 30);
    }

    /// 关键回归：`config::utc_iso()` 写的是 `T09-30-00Z`（时间里是连字符），
    /// 只认 RFC3339 冒号形态的解析器会把本仓库自己写的事件**全部**丢掉，
    /// 趋势与健康卡恒为空且不报错。
    #[test]
    fn 认得本仓库自己的时间格式() {
        let _iso = Isolated::new("dbhealth-iso");
        // 真实走 record_* 写入（用的是 config::utc_iso）
        record_keepalive(true, "ok").unwrap();
        let at = load()[0]["at"].as_str().unwrap().to_string();
        assert!(
            parse_iso_ts(&at).is_some(),
            "config::utc_iso 产出的 {at} 必须能被解析"
        );

        // 两种形态都能解析
        assert!(parse_iso_ts("2026-01-02T03-04-05Z").is_some());
        assert!(parse_iso_ts("2026-01-02T03:04:05Z").is_some());
        assert!(parse_iso_ts("2026-01-02T03:04:05").is_some());
        assert!(parse_iso_ts("不是时间").is_none());

        // 今天写入的事件必须落进 7 天窗口
        let h = health_counts(7, chrono::Utc::now());
        assert_eq!(h["keepalive"], 1, "刚写入的保活事件应在窗口内");
        assert!(h["lastKeepaliveAt"].is_string());
    }

    /// 日期分组必须与时间格式无关（取前 10 字符 = `YYYY-MM-DD`）。
    #[test]
    fn 日期分组不受时间分隔符影响() {
        let _iso = Isolated::new("dbhealth-day");
        append(json!({
            "at": "2026-01-02T03-04-05Z",
            "kind": KIND_QUOTA, "ok": true, "uid": "u-1", "level": "Pro",
            "windows": [{"name": "当前时段", "usedPercent": 12.0}]
        }))
        .unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2026-01-03T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let trend = quota_trend(7, now);
        assert_eq!(trend.len(), 1);
        assert_eq!(trend[0]["date"], "2026-01-02");
    }

    #[test]
    fn 损坏文件不崩溃() {
        let _iso = Isolated::new("dbhealth-broken");
        std::fs::create_dir_all(config::store_dir()).unwrap();
        std::fs::write(history_file(), "{ 不是数组").unwrap();
        assert!(load().is_empty());
        // 损坏后仍可继续追加
        record_keepalive(true, "ok").unwrap();
        assert_eq!(load().len(), 1);
    }
}
