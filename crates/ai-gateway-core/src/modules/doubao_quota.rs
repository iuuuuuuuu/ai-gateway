//! 豆包会员额度查询。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/tasks/doubao_quota.rs`。
//!
//! ## 两段式解析
//!
//! 1. **精确解析**：按 2026-09 抓包确认的结构取值
//!    （`current_subscription` / `window_limit_section.window_limit_groups`）
//! 2. **宽容兜底**：结构对不上时按「同时含名称键与总量键的对象」深挖（深度上限 10）
//!
//! 为什么不能只做兜底：兜底拿不到「已用百分比 / 重置时间 / 是否耗尽」这些
//! 界面上真正有用的字段，只能给出「总共多少、剩多少」。

use serde_json::{json, Value};

use crate::modules::doubao_account;
use crate::modules::doubao_session::{self, ProbeStatus};
use crate::modules::config;

/// 额度接口默认值。
pub const DEFAULT_QUOTA_URL: &str =
    "https://www.doubao.com/alice/commerce/sale/subscription/quota/summary/";

/// 窗口类型码 → 展示名。
fn window_type_label(code: i64) -> Option<&'static str> {
    match code {
        1 => Some("当前时段"),
        2 => Some("近 7 天"),
        _ => None,
    }
}

/// 毫秒时间戳 → 本地可读时间。
fn fmt_ts_ms(ts: i64) -> Option<String> {
    let secs = if ts > 10_000_000_000 { ts / 1000 } else { ts };
    chrono::DateTime::from_timestamp(secs, 0).map(|dt| dt.format("%m-%d %H:%M").to_string())
}

/// 秒时间戳 → 本地可读时间（含日期）。
fn fmt_ts_secs(ts: i64) -> Option<String> {
    let secs = if ts > 10_000_000_000 { ts / 1000 } else { ts };
    chrono::DateTime::from_timestamp(secs, 0).map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
}

/// 解析后的额度。
#[derive(Debug, Clone, Default)]
pub struct ParsedQuota {
    /// 会员等级展示名（如「豆包 Pro」）。
    pub level: Option<String>,
    /// 套餐到期时间。
    pub expire_at: Option<String>,
    pub has_subscription: bool,
    pub is_gift: bool,
    /// 订阅详情。
    pub subscription: Option<Value>,
    /// 窗口额度项（当前时段 / 近 7 天等）。
    pub windows: Vec<Value>,
    /// 兜底解析出的额度项。
    pub items: Vec<Value>,
}

impl ParsedQuota {
    /// 是否解析出了任何有用信息。
    pub fn has_data(&self) -> bool {
        self.level.is_some()
            || self.expire_at.is_some()
            || !self.windows.is_empty()
            || !self.items.is_empty()
    }

    /// 单行摘要（界面 hover 展示）。
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for w in self.windows.iter().take(4) {
            let name = w.get("name").and_then(Value::as_str).unwrap_or("额度");
            let exhausted = w.get("exhausted").and_then(Value::as_bool).unwrap_or(false);
            let used = w.get("usedPercent").and_then(Value::as_f64);
            let reset = w.get("resetAt").and_then(Value::as_str);
            if exhausted {
                parts.push(format!("{name} 已用完"));
            } else if let Some(p) = used {
                match reset {
                    Some(r) => parts.push(format!("{name} 已用 {p:.0}%（{r} 重置）")),
                    None => parts.push(format!("{name} 已用 {p:.0}%")),
                }
            }
        }
        // 剩余名额：`parts` 最多 4 项，用 saturating_sub 保证不为负。
        // 早期写成 `4 - parts.len().min(4)`，当窗口数 ≥4 时结果恒为 0，
        // 兜底项永远进不来（虽然 items 与 windows 通常互斥，但不该靠这个前提）。
        for item in self.items.iter().take(4usize.saturating_sub(parts.len())) {
            let name = item.get("name").and_then(Value::as_str).unwrap_or("额度");
            if let (Some(left), Some(total)) = (
                item.get("left").and_then(Value::as_f64),
                item.get("total").and_then(Value::as_f64),
            ) {
                parts.push(format!("{name} {left:.0}/{total:.0}"));
            }
        }
        if parts.is_empty() {
            "暂无额度信息".to_string()
        } else {
            parts.join(" · ")
        }
    }
}

/// 精确解析（按抓包确认的结构）。
fn parse_exact(data: &Value) -> ParsedQuota {
    let mut out = ParsedQuota::default();
    let Some(root) = data.get("data").filter(|v| v.is_object()) else {
        return out;
    };

    // 会员等级：display.short_name 优先，回退 membership_display_name
    let current = root.get("current_subscription");
    if let Some(cs) = current.filter(|v| v.is_object()) {
        out.level = cs
            .get("display")
            .and_then(|d| d.get("short_name"))
            .and_then(Value::as_str)
            .or_else(|| root.get("membership_display_name").and_then(Value::as_str))
            .map(str::to_string);
        out.expire_at = cs.get("end_time").and_then(Value::as_i64).and_then(fmt_ts_secs);
        out.is_gift = cs.get("is_gift").and_then(Value::as_bool).unwrap_or(false);
        // status 为 null / 0 视为无有效订阅
        out.has_subscription = cs
            .get("status")
            .and_then(Value::as_i64)
            .is_some_and(|s| s != 0);
    }
    if out.level.is_none() {
        out.level = root
            .get("membership_display_name")
            .and_then(Value::as_str)
            .map(str::to_string);
    }

    // 订阅详情
    if let Some(sub) = root.get("subscription").filter(|v| v.is_object()) {
        let start = sub.get("start_time").and_then(Value::as_i64);
        let end = sub.get("end_time").and_then(Value::as_i64);
        // period_days 由起止时间反推（上游不总是下发）
        let period_days = match (start, end) {
            (Some(s), Some(e)) if e > s => Some(((e - s) as f64 / 86400.0).round() as i64),
            _ => sub.get("period_days").and_then(Value::as_i64),
        };
        out.subscription = Some(json!({
            "name": sub.get("name"),
            "periodDays": period_days,
            "startAt": start.and_then(fmt_ts_secs),
            "expireAt": end.and_then(fmt_ts_secs),
            "isGift": sub.get("is_gift").and_then(Value::as_bool).unwrap_or(false),
            "active": sub.get("status").and_then(Value::as_i64).is_some_and(|s| s != 0),
        }));
    }

    // 窗口额度
    if let Some(groups) = root
        .get("window_limit_section")
        .and_then(|s| s.get("window_limit_groups"))
        .and_then(Value::as_array)
    {
        for group in groups {
            let Some(limits) = group.get("window_limits").and_then(Value::as_array) else {
                continue;
            };
            for limit in limits {
                let name = limit
                    .get("window_type")
                    .and_then(Value::as_i64)
                    .and_then(window_type_label)
                    .map(str::to_string)
                    .or_else(|| {
                        limit
                            .get("name")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| "额度".to_string());
                let used_percent = limit.get("used_percent").and_then(Value::as_f64);
                let exhausted = limit
                    .get("usage_exhausted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    || used_percent.is_some_and(|p| p >= 100.0);
                out.windows.push(json!({
                    "name": name,
                    "usedPercent": used_percent,
                    "exhausted": exhausted,
                    "resetAt": limit.get("end_time").and_then(Value::as_i64).and_then(fmt_ts_ms),
                }));
            }
        }
    }
    out
}

/// 兜底：按「同时含名称键与总量键」深挖额度项。
fn parse_fallback(data: &Value) -> (Vec<Value>, Option<String>) {
    const NAME_KEYS: [&str; 6] = ["name", "title", "display_name", "product_name", "quota_name", "type_name"];
    const TOTAL_KEYS: [&str; 6] = ["total", "total_count", "limit", "quota", "total_amount", "max"];
    const LEFT_KEYS: [&str; 6] = ["left", "remaining", "remain", "available", "left_count", "balance"];
    const LEVEL_KEYS: [&str; 5] = [
        "membership_display_name",
        "display_name",
        "short_name",
        "level_name",
        "product_name",
    ];

    fn deep_find(value: &Value, keys: &[&str], depth: usize) -> Option<Value> {
        if depth == 0 {
            return None;
        }
        if let Some(obj) = value.as_object() {
            for key in keys {
                if let Some(found) = obj.get(*key) {
                    if found.is_string() || found.is_number() {
                        return Some(found.clone());
                    }
                }
            }
            for v in obj.values() {
                if let Some(found) = deep_find(v, keys, depth - 1) {
                    return Some(found);
                }
            }
        } else if let Some(arr) = value.as_array() {
            for v in arr {
                if let Some(found) = deep_find(v, keys, depth - 1) {
                    return Some(found);
                }
            }
        }
        None
    }

    fn collect_items(value: &Value, out: &mut Vec<Value>, depth: usize) {
        if depth == 0 || out.len() >= 20 {
            return;
        }
        if let Some(obj) = value.as_object() {
            let name = NAME_KEYS
                .iter()
                .find_map(|k| obj.get(*k).and_then(Value::as_str));
            let total = TOTAL_KEYS
                .iter()
                .find_map(|k| obj.get(*k).and_then(Value::as_f64));
            if let (Some(name), Some(total)) = (name, total) {
                let left = LEFT_KEYS
                    .iter()
                    .find_map(|k| obj.get(*k).and_then(Value::as_f64));
                out.push(json!({
                    "name": name,
                    "total": total,
                    "left": left,
                    "used": left.map(|l| total - l),
                }));
            }
            for v in obj.values() {
                collect_items(v, out, depth - 1);
            }
        } else if let Some(arr) = value.as_array() {
            for v in arr {
                collect_items(v, out, depth - 1);
            }
        }
    }

    let mut items = Vec::new();
    collect_items(data, &mut items, 10);
    // 名称回退（深度限制 10，与 items 一致）
    let level = deep_find(data, &LEVEL_KEYS, 10)
        .and_then(|v| v.as_str().map(str::to_string));
    (items, level)
}

/// 解析额度响应。
pub fn parse_quota(resp: &Value) -> ParsedQuota {
    let mut out = parse_exact(resp);
    // 精确解析没拿到任何信息时才启用兜底，避免兜底把精确结果的语义冲淡
    if !out.has_data() {
        let (items, level) = parse_fallback(resp);
        out.items = items;
        if out.level.is_none() {
            out.level = level;
        }
    }
    out
}

/// 查询单个账号的额度。
pub async fn fetch_single(uid: &str) -> Result<ParsedQuota, String> {
    let acc = doubao_account::find_account(uid).ok_or_else(|| "账号不存在".to_string())?;
    let sid = acc
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if sid.is_empty() {
        return Err("该账号未配置 sessionid，无法查询额度".to_string());
    }
    let sid_guard = acc
        .get("sid_guard")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let url = config::app_setting_str("doubao_quota_url")
        .unwrap_or_else(|| DEFAULT_QUOTA_URL.to_string());

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        // 直连：本地 MITM 代理未启动时是死端口，走系统代理会让查询全部失败
        .no_proxy()
        .build()
        .map_err(|e| format!("HTTP 客户端创建失败: {e}"))?;

    let cookie = match sid_guard {
        Some(guard) => format!("sessionid={sid}; sid_guard={guard}"),
        None => format!("sessionid={sid}"),
    };
    let resp = client
        .post(&url)
        .body(r#"{"product_line":"membership"}"#)
        .header("Cookie", cookie)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36",
        )
        .header("Referer", "https://www.doubao.com/")
        .header("Accept", "application/json, text/plain, */*")
        .header("Content-Type", "application/json")
        .send()
        .await
        .map_err(|e| format!("请求失败: {e}"))?;

    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let body: Value = serde_json::from_str(&text)
        .map_err(|_| format!("非 JSON 响应（HTTP {status}）: {}", text.chars().take(200).collect::<String>()))?;

    let code = body.get("code").and_then(Value::as_i64);
    match code {
        None | Some(0) => Ok(parse_quota(&body)),
        Some(doubao_session::SESSION_EXPIRED_CODE) => Err(
            "登录态已失效，请开启代理重新抓取凭证或重新保存该账号登录态".to_string(),
        ),
        Some(c) => Err(format!("接口返回业务码 {c}")),
    }
}

/// 批量查询全部持有凭证的账号，并把结果写回额度缓存。
pub async fn run_batch() -> Value {
    let mut ok = 0usize;
    let mut failed = 0usize;
    let mut exhausted: Vec<String> = Vec::new();
    let mut results: Vec<Value> = Vec::new();

    for acc in doubao_account::load_accounts() {
        let uid = acc.get("user_id").and_then(Value::as_str).unwrap_or("");
        let name = acc.get("name").and_then(Value::as_str).unwrap_or(uid).to_string();
        let has_session = acc
            .get("session_id")
            .and_then(Value::as_str)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if !has_session {
            continue;
        }

        match fetch_single(uid).await {
            Ok(parsed) => {
                ok += 1;
                let summary = parsed.summary();
                let is_exhausted = parsed
                    .windows
                    .iter()
                    .any(|w| w.get("exhausted").and_then(Value::as_bool).unwrap_or(false));
                if is_exhausted {
                    exhausted.push(name.clone());
                }
                // 写回额度缓存（供账号列表直接展示，无需每次重新查询）
                let level = parsed.level.clone();
                let expire = parsed.expire_at.clone();
                let summary_clone = summary.clone();
                let _ = doubao_account::mutate_account(uid, |obj| {
                    obj.insert("quota_level".to_string(), json!(level));
                    obj.insert("quota_expire_at".to_string(), json!(expire));
                    obj.insert("quota_summary".to_string(), json!(summary_clone));
                    obj.insert("quota_checked_at".to_string(), json!(config::utc_iso()));
                });
                results.push(json!({
                    "userId": uid, "name": name, "ok": true, "summary": summary,
                    "level": parsed.level, "expireAt": parsed.expire_at,
                }));
            }
            Err(e) => {
                failed += 1;
                results.push(json!({
                    "userId": uid, "name": name, "ok": false, "error": e,
                }));
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    json!({
        "ok": ok,
        "failed": failed,
        "exhausted": exhausted,
        "results": results,
    })
}

/// 查询结果转前端形态。
pub fn to_view(uid: &str, parsed: &ParsedQuota) -> Value {
    json!({
        "userId": uid,
        "ok": true,
        "level": parsed.level,
        "expireAt": parsed.expire_at,
        "hasSubscription": parsed.has_subscription,
        "isGift": parsed.is_gift,
        "subscription": parsed.subscription,
        "windows": parsed.windows,
        "items": parsed.items,
        "summary": parsed.summary(),
    })
}

/// 会话探活（供界面「诊断」按钮）。
pub async fn probe_account(uid: &str) -> Value {
    let Some(acc) = doubao_account::find_account(uid) else {
        return json!({"ok": false, "error": "账号不存在"});
    };
    let sid = acc
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if sid.is_empty() {
        return json!({"ok": false, "error": "该账号未配置 sessionid"});
    }
    let probe_url = config::app_setting_str("doubao_quota_url")
        .unwrap_or_else(|| doubao_session::DEFAULT_PROBE_URL.to_string());
    let result = doubao_session::api_probe(sid, &probe_url).await;
    json!({
        "ok": result.status == ProbeStatus::Ok,
        "status": result.status.as_str(),
        "detail": result.detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-09 抓包确认的真实结构（节选）。
    fn captured_shape() -> Value {
        json!({
            "code": 0,
            "data": {
                "membership_display_name": "豆包 Pro",
                "current_subscription": {
                    "display": {"short_name": "Pro"},
                    "end_time": 1_800_000_000i64,
                    "is_gift": false,
                    "status": 1
                },
                "subscription": {
                    "name": "豆包 Pro 连续包月",
                    "start_time": 1_797_000_000i64,
                    "end_time": 1_800_000_000i64,
                    "status": 1
                },
                "window_limit_section": {
                    "window_limit_groups": [{
                        "window_limits": [
                            {"window_type": 1, "used_percent": 80.0, "usage_exhausted": false, "end_time": 1_798_000_000_000i64},
                            {"window_type": 2, "used_percent": 100.0, "usage_exhausted": true, "end_time": 1_799_000_000_000i64}
                        ]
                    }]
                }
            }
        })
    }

    #[test]
    fn 精确解析抓包结构() {
        let parsed = parse_quota(&captured_shape());
        assert_eq!(parsed.level.as_deref(), Some("Pro"));
        assert!(parsed.expire_at.is_some());
        assert!(parsed.has_subscription);
        assert!(!parsed.is_gift);
        assert_eq!(parsed.windows.len(), 2);
        assert_eq!(parsed.windows[0]["name"], "当前时段");
        assert_eq!(parsed.windows[0]["usedPercent"], 80.0);
        assert_eq!(parsed.windows[0]["exhausted"], false);
        assert_eq!(parsed.windows[1]["name"], "近 7 天");
        assert_eq!(
            parsed.windows[1]["exhausted"], true,
            "used_percent 达 100 即使 usage_exhausted 为假也应判耗尽"
        );
        // 订阅周期由起止时间反推
        assert_eq!(parsed.subscription.as_ref().unwrap()["periodDays"], 35);
    }

    #[test]
    fn 摘要包含窗口与重置时间() {
        let parsed = parse_quota(&captured_shape());
        let summary = parsed.summary();
        assert!(summary.contains("当前时段"), "摘要应含窗口名: {summary}");
        assert!(summary.contains("80%"), "摘要应含已用百分比: {summary}");
        assert!(summary.contains("近 7 天 已用完"), "耗尽窗口应显示已用完: {summary}");
    }

    #[test]
    fn 结构不匹配时走兜底解析() {
        // 完全不同的结构：只有嵌套的名称 + 总量
        let odd = json!({
            "result": {
                "something": {
                    "quota_list": [
                        {"name": "图片生成", "total": 100, "remaining": 40},
                        {"name": "视频生成", "total": 10, "left": 3}
                    ]
                }
            }
        });
        let parsed = parse_quota(&odd);
        assert_eq!(parsed.items.len(), 2, "兜底应挖出两个额度项");
        assert_eq!(parsed.items[0]["name"], "图片生成");
        assert_eq!(parsed.items[0]["left"], 40.0);
        assert_eq!(parsed.items[0]["used"], 60.0);
        let summary = parsed.summary();
        assert!(summary.contains("图片生成 40/100"), "兜底摘要格式: {summary}");
    }

    #[test]
    fn 无订阅时_has_subscription_为假() {
        let no_sub = json!({
            "code": 0,
            "data": {
                "current_subscription": {"status": 0, "display": {"short_name": "Free"}},
                "membership_display_name": "免费版"
            }
        });
        let parsed = parse_quota(&no_sub);
        assert!(!parsed.has_subscription);
        assert_eq!(parsed.level.as_deref(), Some("Free"));
    }

    #[test]
    fn 空响应不panic且标记无数据() {
        for empty in [json!({}), json!({"code": 0}), json!({"data": {}}), json!(null)] {
            let parsed = parse_quota(&empty);
            assert!(!parsed.has_data() || parsed.level.is_some());
            // summary 必须有可展示的兜底文案
            assert!(!parsed.summary().is_empty());
        }
    }

    #[test]
    fn 时间戳格式化兼容秒与毫秒() {
        // 秒
        let secs = 1_800_000_000i64;
        assert!(fmt_ts_secs(secs).is_some());
        // 毫秒（应被识别并换算，而不是当成公元 5 万年）
        let ms = secs * 1000;
        assert_eq!(fmt_ts_secs(ms), fmt_ts_secs(secs));
        assert_eq!(fmt_ts_ms(ms), fmt_ts_ms(secs));
    }

    #[test]
    fn 窗口类型码映射() {
        assert_eq!(window_type_label(1), Some("当前时段"));
        assert_eq!(window_type_label(2), Some("近 7 天"));
        assert_eq!(window_type_label(99), None);
    }

    #[test]
    fn 额度视图形态() {
        let parsed = parse_quota(&captured_shape());
        let view = to_view("123", &parsed);
        assert_eq!(view["userId"], "123");
        assert_eq!(view["ok"], true);
        assert_eq!(view["level"], "Pro");
        assert!(view["windows"].as_array().unwrap().len() == 2);
        assert!(view["summary"].as_str().unwrap().contains("当前时段"));
    }
}
