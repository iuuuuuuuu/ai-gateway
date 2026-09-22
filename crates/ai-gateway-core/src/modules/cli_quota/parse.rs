//! 各 provider 额度响应的解析（纯函数，不触网、不读盘）。
//!
//! 对照 EasyCLIProxyAPI `src/services/quotaService.ts` 的 `quotaRowsFor()` 与
//! `src/services/xaiBilling.ts`：把 5 家上游各不相同的 JSON 形态归一成
//! [`QuotaWindow`] 列表。
//!
//! 这里刻意只做**纯解析**：上游字段命名在同一家内部就会混用 snake_case 与
//! camelCase（实测 Codex 的 `used_percent` / `usedPercent` 并存），因此所有取值
//! 都走 [`field`] 多键回退，而不是反序列化到固定 struct —— 少一个字段就整条
//! 额度解析失败，对「查询」这种只读场景是不可接受的。
//!
//! 解析失败一律返回空窗口列表，由调用方决定报错文案；**绝不返回伪造的 0%**，
//! 否则界面会把「拉不到」显示成「额度用尽」。

use chrono::TimeZone;
use serde_json::Value;

use super::{QuotaWindow, ResetCredit};

const FIVE_HOUR_SECONDS: f64 = 18_000.0;
const WEEK_SECONDS: f64 = 604_800.0;
const MIN_MONTH_SECONDS: f64 = 28.0 * 86_400.0;
const MAX_MONTH_SECONDS: f64 = 31.0 * 86_400.0;

// ---------------------------------------------------------------------------
// 通用取值
// ---------------------------------------------------------------------------

/// 按候选键顺序取第一个存在的字段（兼容 snake_case / camelCase）。
fn field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let object = value.as_object()?;
    keys.iter().find_map(|key| object.get(*key))
}

/// 取嵌套对象；缺失或类型不符返回 None。
fn object<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    field(value, keys).filter(|item| item.is_object())
}

/// 取数组；缺失或类型不符返回空切片。
fn array<'a>(value: &'a Value, keys: &[&str]) -> &'a [Value] {
    field(value, keys)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// 数值化：数字、数字字符串、以及 xAI 的 `{"val": n}` 包装形态。
fn number(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    if let Some(nested) = value.as_object().and_then(|item| item.get("val")) {
        return number(Some(nested));
    }
    let parsed = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    parsed.is_finite().then_some(parsed)
}

fn text(value: Option<&Value>) -> Option<String> {
    let value = value?;
    let raw = match value {
        Value::String(item) => item.trim().to_string(),
        Value::Number(item) => item.to_string(),
        _ => return None,
    };
    (!raw.is_empty()).then_some(raw)
}

/// 宽松布尔：真值集与 EasyCLIProxyAPI 的 `booleanValue` 保持一致。
fn truthy(value: Option<&Value>) -> Option<bool> {
    match value? {
        Value::Bool(flag) => Some(*flag),
        Value::Number(number) => number.as_f64().map(|item| item != 0.0),
        Value::String(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "y" | "on" => Some(true),
            "false" | "0" | "no" | "n" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn clamp_percent(value: f64) -> f64 {
    value.clamp(0.0, 100.0)
}

/// 已用百分比 → 剩余百分比。
fn remaining_from_used(used: Option<f64>) -> Option<f64> {
    used.map(|item| clamp_percent(100.0 - clamp_percent(item)))
}

/// 0..1 的分数 → 百分比；`"42%"` 字符串也接受。
fn percent_from_fraction(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    if let Value::String(raw) = value {
        let trimmed = raw.trim();
        if let Some(stripped) = trimmed.strip_suffix('%') {
            return stripped.trim().parse::<f64>().ok().map(clamp_percent);
        }
    }
    number(Some(value)).map(|item| clamp_percent(item * 100.0))
}

/// 把上游各种时间表示归一成毫秒时间戳。
///
/// 支持：RFC3339 / `YYYY-MM-DD HH:MM:SS` / 纯数字（秒或毫秒，按量级判定）。
fn instant(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(number) = number(Some(value)) {
        if !number.is_finite() || number <= 0.0 {
            return None;
        }
        // 与 auth.rs 的 normalize_epoch 同口径：< 1e11 视为秒。
        return Some(if number < 1e11 {
            (number * 1000.0).round() as i64
        } else {
            number.round() as i64
        });
    }
    let raw = value.as_str()?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(parsed.timestamp_millis());
    }
    for pattern in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
    ] {
        if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(raw, pattern) {
            return chrono::Local
                .from_local_datetime(&parsed)
                .earliest()
                .map(|item| item.timestamp_millis());
        }
    }
    None
}

/// 绝对重置时刻：先试绝对时间键，再试「剩余秒数」键（相对当前时刻换算）。
fn reset_at(value: &Value, absolute_keys: &[&str], relative_keys: &[&str]) -> Option<i64> {
    for key in absolute_keys {
        if let Some(ms) = instant(value.get(*key)) {
            return Some(ms);
        }
    }
    let now = chrono::Utc::now().timestamp_millis();
    for key in relative_keys {
        if let Some(seconds) = number(value.get(*key)) {
            if seconds >= 0.0 && seconds.is_finite() {
                return Some(now + (seconds * 1000.0).round() as i64);
            }
        }
    }
    None
}

/// 窗口时长 → 中文标签（对齐 EasyCLIProxyAPI 的 `codexWindowLabel`）。
fn duration_label(seconds: Option<f64>) -> String {
    let Some(seconds) = seconds.filter(|item| *item > 0.0) else {
        return String::new();
    };
    let day = 86_400.0;
    let hour = 3_600.0;
    let minute = 60.0;
    if seconds == FIVE_HOUR_SECONDS {
        return "5 小时".to_string();
    }
    if seconds == WEEK_SECONDS {
        return "每周".to_string();
    }
    if (MIN_MONTH_SECONDS..=MAX_MONTH_SECONDS).contains(&seconds) {
        return "每月".to_string();
    }
    if (seconds % day) == 0.0 {
        format!("{} 天", seconds / day)
    } else if (seconds % hour) == 0.0 {
        format!("{} 小时", seconds / hour)
    } else if (seconds % minute) == 0.0 {
        format!("{} 分钟", seconds / minute)
    } else {
        format!("{seconds} 秒")
    }
}

// ---------------------------------------------------------------------------
// Codex（ChatGPT 订阅额度）
// ---------------------------------------------------------------------------

/// 解析 Codex `wham/usage` 响应。
///
/// 结构：`rate_limit.{primary_window,secondary_window}` 为 5 小时 / 每周窗口，
/// 另有 `code_review_rate_limit` 与 `additional_rate_limits[]` 两组附加窗口。
/// `rate_limit_reset_credits` 是「手动重置次数」，单独取出供界面展示。
pub fn codex(payload: &Value) -> (Vec<QuotaWindow>, Option<ResetCredit>) {
    let mut windows = Vec::new();

    let mut push_rate_limit = |raw: Option<&Value>, prefix: &str| {
        let Some(limit) = raw.filter(|item| item.is_object()) else {
            return;
        };
        let mut entries: Vec<(&Value, &str)> = Vec::new();
        if let Some(primary) = object(limit, &["primary_window", "primaryWindow"]) {
            entries.push((primary, "primary"));
        }
        if let Some(secondary) = object(limit, &["secondary_window", "secondaryWindow"]) {
            entries.push((secondary, "secondary"));
        }
        // 5 小时窗口排在每周之前：上游字段顺序不保证，按语义排序才对得上界面习惯。
        entries.sort_by_key(|(window, kind)| {
            let seconds = number(field(window, &["limit_window_seconds", "limitWindowSeconds"]));
            match seconds {
                Some(item) if item == FIVE_HOUR_SECONDS => 0,
                Some(item)
                    if item == WEEK_SECONDS
                        || (MIN_MONTH_SECONDS..=MAX_MONTH_SECONDS).contains(&item) =>
                {
                    1
                }
                _ if *kind == "primary" => 0,
                _ => 1,
            }
        });

        let reached = truthy(field(limit, &["limit_reached", "limitReached"])) == Some(true)
            || truthy(field(limit, &["allowed"])) == Some(false);

        for (window, kind) in entries {
            let seconds = number(field(window, &["limit_window_seconds", "limitWindowSeconds"]));
            let label = match duration_label(seconds) {
                text if !text.is_empty() => format!("{prefix}{text}"),
                // 上游没给窗口时长时按槽位兜底，与 EasyCLIProxyAPI 一致。
                _ if kind == "primary" => format!("{prefix}5 小时"),
                _ => format!("{prefix}每周"),
            };
            let reset_at_ms = reset_at(
                window,
                &["reset_at", "resetAt"],
                &["reset_after_seconds", "resetAfterSeconds"],
            );
            let remaining = remaining_from_used(number(field(window, &["used_percent", "usedPercent"])))
                .or_else(|| (reached && reset_at_ms.is_some()).then_some(0.0));
            windows.push(QuotaWindow {
                label,
                remaining_percent: remaining,
                reset_at_ms,
                detail: None,
            });
        }
    };

    push_rate_limit(object(payload, &["rate_limit", "rateLimit"]), "");
    push_rate_limit(
        object(payload, &["code_review_rate_limit", "codeReviewRateLimit"]),
        "代码审查 ",
    );
    for (index, item) in array(payload, &["additional_rate_limits", "additionalRateLimits"])
        .iter()
        .enumerate()
    {
        let name = text(field(
            item,
            &["limit_name", "limitName", "metered_feature", "meteredFeature"],
        ))
        .unwrap_or_else(|| format!("附加额度 {}", index + 1));
        push_rate_limit(object(item, &["rate_limit", "rateLimit"]), &format!("{name} "));
    }

    let credits = object(payload, &["rate_limit_reset_credits", "rateLimitResetCredits"]).map(
        |block| {
            let count = number(field(block, &["available_count", "availableCount"]))
                .map(|item| item.max(0.0).floor() as u32);
            let applicable =
                number(field(block, &["applicable_available_count", "applicableAvailableCount"]))
                    .map(|item| item.max(0.0).floor() as u32);
            let now = chrono::Utc::now().timestamp_millis();
            let earliest = array(block, &["credits"])
                .iter()
                .filter(|credit| {
                    text(field(credit, &["reset_type", "resetType"])).as_deref()
                        == Some("codex_rate_limits")
                        && text(field(credit, &["status"])).as_deref() == Some("available")
                })
                .filter_map(|credit| instant(field(credit, &["expires_at", "expiresAt"])))
                .filter(|ms| *ms > now)
                .min();
            ResetCredit {
                available: count,
                applicable,
                earliest_expiry_ms: earliest,
            }
        },
    );

    (windows, credits)
}

// ---------------------------------------------------------------------------
// Claude（Pro / Max 订阅额度）
// ---------------------------------------------------------------------------

/// 解析 Claude `api/oauth/usage` 响应。
///
/// 顶层是若干固定窗口键（`five_hour` / `seven_day` / …），每项带 `utilization`
/// （已用百分比）与 `resets_at`。`extra_usage` 是超额用量，金额单位为**美分**。
pub fn claude(payload: &Value) -> Vec<QuotaWindow> {
    const LABELS: [(&str, &str); 6] = [
        ("five_hour", "5 小时"),
        ("seven_day", "7 天"),
        ("seven_day_oauth_apps", "7 天（OAuth 应用）"),
        ("seven_day_opus", "7 天（Opus）"),
        ("seven_day_sonnet", "7 天（Sonnet）"),
        ("seven_day_cowork", "7 天（协作）"),
    ];

    let mut windows = Vec::new();
    for (key, label) in LABELS {
        let Some(raw) = payload.get(key).filter(|item| item.is_object()) else {
            continue;
        };
        // 没有 utilization 说明这个窗口对本账号不适用，跳过而不是显示 0%。
        if field(raw, &["utilization"]).is_none() {
            continue;
        }
        windows.push(QuotaWindow {
            label: label.to_string(),
            remaining_percent: remaining_from_used(number(field(raw, &["utilization"]))),
            reset_at_ms: reset_at(raw, &["resets_at", "resetsAt"], &[]),
            detail: None,
        });
    }

    if let Some(extra) = object(payload, &["extra_usage", "extraUsage"]) {
        if truthy(field(extra, &["is_enabled", "isEnabled"])) == Some(true) {
            let monthly_limit = number(field(extra, &["monthly_limit", "monthlyLimit"]));
            let used = number(field(extra, &["used_credits", "usedCredits"]));
            // 上游没给 utilization 时用「已用 / 总额」自行折算，避免显示成未知。
            let computed = match (monthly_limit, used) {
                (Some(limit), Some(used)) if limit > 0.0 => {
                    Some(clamp_percent((limit - used) / limit * 100.0))
                }
                _ => None,
            };
            let remaining = remaining_from_used(number(field(extra, &["utilization"])))
                .or(computed);
            let detail = match (used, monthly_limit) {
                (Some(used), Some(limit)) => {
                    Some(format!("{} / {}", usd_from_cents(used), usd_from_cents(limit)))
                }
                _ => None,
            };
            windows.push(QuotaWindow {
                label: "额外用量".to_string(),
                remaining_percent: remaining,
                reset_at_ms: None,
                detail,
            });
        }
    }

    windows
}

/// 美分 → `$x.xx`。
fn usd_from_cents(cents: f64) -> String {
    format!("${:.2}", cents / 100.0)
}

// ---------------------------------------------------------------------------
// Antigravity（Google Cloud Code 额度）
// ---------------------------------------------------------------------------

/// 解析 Antigravity `v1internal:retrieveUserQuotaSummary` 响应。
///
/// 结构：`groups[].buckets[]`，每个 bucket 带 `remaining_fraction`（0..1）。
/// `body` 字段是上游把 JSON 当字符串再包一层的形态，需要拆开。
pub fn antigravity(payload: &Value) -> Vec<QuotaWindow> {
    let nested = field(payload, &["body"])
        .and_then(Value::as_str)
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    let summary = nested.as_ref().unwrap_or(payload);

    let mut windows = Vec::new();
    for group in array(summary, &["groups"]) {
        let group_label =
            text(field(group, &["display_name", "displayName"])).unwrap_or_else(|| "额度".to_string());
        let group_description = text(field(group, &["description"]));
        let buckets = array(group, &["buckets"]);
        // 5 小时窗口排最前，其次是每周，与 Codex 一致。
        let mut ordered: Vec<&Value> = buckets.iter().collect();
        ordered.sort_by_key(|bucket| {
            let window = text(field(bucket, &["window"]))
                .unwrap_or_default()
                .to_ascii_lowercase();
            if ["5h", "five-hour", "five_hour"].contains(&window.as_str()) {
                0
            } else if ["weekly", "week"].contains(&window.as_str()) {
                1
            } else {
                2
            }
        });

        for (index, bucket) in ordered.iter().enumerate() {
            let Some(remaining) = percent_from_fraction(field(
                bucket,
                &["remaining_fraction", "remainingFraction"],
            )) else {
                continue;
            };
            let bucket_label = text(field(bucket, &["display_name", "displayName", "window"]));
            // 只在桶名确实提供了额外信息时拼接，避免出现「G · G」这种重复标签。
            let label = match bucket_label {
                Some(item) if item != group_label => format!("{group_label} · {item}"),
                _ => group_label.clone(),
            };
            let detail = text(field(bucket, &["description"]))
                .or_else(|| group_description.clone());
            windows.push(QuotaWindow {
                label: if label.is_empty() {
                    format!("额度 {}", index + 1)
                } else {
                    label
                },
                remaining_percent: Some(remaining),
                reset_at_ms: reset_at(bucket, &["reset_time", "resetTime"], &[]),
                detail,
            });
        }
    }
    windows
}

/// 从 `loadCodeAssist` 响应里取套餐名与项目号。
///
/// 返回 `(套餐名, 项目号)`：套餐用于展示，项目号是额度查询的必需入参。
pub fn antigravity_plan(payload: &Value) -> (Option<String>, Option<String>) {
    let current = object(payload, &["currentTier", "current_tier"]);
    let paid = object(payload, &["paidTier", "paid_tier"]);
    // 有付费档就用付费档，否则回落到当前档。
    let effective = paid
        .filter(|item| text(field(item, &["id"])).is_some())
        .or(current);

    let tier_id = text(field(effective.unwrap_or(&Value::Null), &["id"]))
        .unwrap_or_default()
        .to_ascii_lowercase();
    let tier_name = text(field(effective.unwrap_or(&Value::Null), &["name"]));
    let plan = match tier_id.as_str() {
        "free-tier" => Some("Free".to_string()),
        "g1-pro-tier" => Some("Pro".to_string()),
        "g1-ultra-tier" => Some("Ultra".to_string()),
        "g1-ultra-lite-tier" => Some("Ultra Lite".to_string()),
        _ => tier_name.or_else(|| (!tier_id.is_empty()).then(|| tier_id.clone())),
    };

    let project = text(field(
        payload,
        &[
            "cloudaicompanionProject",
            "cloud_ai_companion_project",
            "project",
        ],
    ));
    (plan, project)
}

// ---------------------------------------------------------------------------
// xAI / Grok
// ---------------------------------------------------------------------------

/// 解析 xAI `v1/billing` 响应。
///
/// 兼容两种包装：直接给 config，或 `{"config": {...}}`。金额字段同样是美分，
/// 且可能被包成 `{"val": n}`。
pub fn xai(payload: &Value) -> Vec<QuotaWindow> {
    let config = object(payload, &["config"]).unwrap_or(payload);
    let period = object(config, &["currentPeriod", "current_period"]);
    let period_type = text(field(
        period.unwrap_or(&Value::Null),
        &["type"],
    ))
    .unwrap_or_default()
    .to_ascii_lowercase();

    let period_start = text(field(period.unwrap_or(&Value::Null), &["start"]))
        .or_else(|| text(field(config, &["billingPeriodStart", "billing_period_start"])));
    let period_end = text(field(period.unwrap_or(&Value::Null), &["end"]))
        .or_else(|| text(field(config, &["billingPeriodEnd", "billing_period_end"])));

    let credit_usage = number(field(config, &["creditUsagePercent", "credit_usage_percent"]));
    let monthly_limit = number(field(config, &["monthlyLimit", "monthly_limit"]));
    let used = number(field(config, &["used"]));
    let on_demand_cap = number(field(config, &["onDemandCap", "on_demand_cap"]));
    let on_demand_used = number(field(config, &["onDemandUsed", "on_demand_used"]));

    let included_used = match (used, monthly_limit) {
        (Some(used), Some(limit)) if limit > 0.0 => Some(used.min(limit)),
        (Some(used), _) => Some(used),
        _ => None,
    };
    // 超出套餐的部分算「按需用量」。
    let derived_on_demand = match (used, monthly_limit) {
        (Some(used), Some(limit)) => Some((used - limit).max(0.0)),
        _ => None,
    };
    let on_demand_used = on_demand_used.or(derived_on_demand);

    let has_weekly = credit_usage.is_some() || period_type.contains("weekly");
    let products = array(config, &["productUsage", "product_usage"]);

    let mut windows = Vec::new();
    let mut product_rows = Vec::new();
    for (index, item) in products.iter().enumerate() {
        let name = text(field(item, &["product"])).unwrap_or_else(|| format!("产品 {}", index + 1));
        product_rows.push(QuotaWindow {
            label: name,
            remaining_percent: remaining_from_used(number(field(
                item,
                &["usagePercent", "usage_percent"],
            ))),
            reset_at_ms: None,
            detail: None,
        });
    }

    if has_weekly {
        let label = if period_type.contains("monthly") {
            "每月"
        } else {
            "每周"
        };
        windows.push(QuotaWindow {
            label: label.to_string(),
            remaining_percent: remaining_from_used(credit_usage),
            reset_at_ms: instant(period_end.as_deref().map(Value::from).as_ref()),
            detail: None,
        });
    }
    windows.extend(product_rows);

    if let Some(cap) = on_demand_cap.filter(|item| *item > 0.0) {
        windows.push(QuotaWindow {
            label: "按需用量".to_string(),
            remaining_percent: remaining_from_used(match (on_demand_used, cap) {
                (Some(used), cap) => Some(used / cap * 100.0),
                _ => None,
            }),
            reset_at_ms: instant(period_start.as_deref().map(Value::from).as_ref()),
            detail: on_demand_used
                .map(|used| format!("{} / {}", usd_from_cents((cap - used).max(0.0)), usd_from_cents(cap))),
        });
    }

    if monthly_limit.is_some() || used.is_some() {
        let used_percent = match (included_used, monthly_limit) {
            (Some(used), Some(limit)) if limit > 0.0 => Some(used / limit * 100.0),
            _ => None,
        };
        windows.push(QuotaWindow {
            label: "本月套餐内用量".to_string(),
            remaining_percent: remaining_from_used(used_percent),
            reset_at_ms: instant(period_end.as_deref().map(Value::from).as_ref()),
            detail: match (included_used, monthly_limit) {
                (Some(used), Some(limit)) => Some(format!(
                    "{} / {}",
                    usd_from_cents((limit - used).max(0.0)),
                    usd_from_cents(limit)
                )),
                _ => None,
            },
        });
    }

    windows
}

// ---------------------------------------------------------------------------
// Kimi
// ---------------------------------------------------------------------------

/// 解析 Kimi `coding/v1/usages` 响应。
///
/// 结构：`limits[]`（每项含 `detail` 与 `window`）外加可选 `usage` 单窗口。
/// 金额/次数没有统一单位，一律以「已用 / 总量」原样展示。
pub fn kimi(payload: &Value) -> Vec<QuotaWindow> {
    let mut items: Vec<Value> = array(payload, &["limits"]).to_vec();
    if let Some(usage) = object(payload, &["usage"]) {
        let mut usage = usage.clone();
        if let Some(object) = usage.as_object_mut() {
            let label = text(field(payload, &["usage"]).and_then(|item| {
                field(item, &["name", "title"])
            }))
            .unwrap_or_else(|| "每周".to_string());
            object.insert("__label".to_string(), Value::String(label));
        }
        items.push(usage);
    }

    let mut windows = Vec::new();
    for (index, raw) in items.iter().enumerate() {
        let detail = object(raw, &["detail"]).unwrap_or(raw);
        let limit = number(field(detail, &["limit"]));
        let used = number(field(detail, &["used"]));
        let remaining = number(field(detail, &["remaining"]));
        let used_value = used.or_else(|| match (limit, remaining) {
            (Some(limit), Some(remaining)) => Some(limit - remaining),
            _ => None,
        });
        if used_value.is_none() && limit.is_none() {
            continue;
        }

        let window = object(raw, &["window"]);
        let duration = number(field(
            window.unwrap_or(&Value::Null),
            &["duration"],
        ))
        .or_else(|| number(field(raw, &["duration"])))
        .or_else(|| number(field(detail, &["duration"])));
        let unit = text(field(window.unwrap_or(&Value::Null), &["timeUnit", "time_unit"]))
            .or_else(|| text(field(raw, &["timeUnit", "time_unit"])))
            .or_else(|| text(field(detail, &["timeUnit", "time_unit"])))
            .unwrap_or_default()
            .to_ascii_lowercase()
            .trim_start_matches("time_unit_")
            .to_string();
        let duration_text = match duration.filter(|item| *item > 0.0) {
            Some(value) if unit.starts_with("week") => format!("{} 天", value * 7.0),
            Some(value) if unit.starts_with("day") => format!("{value} 天"),
            Some(value) if unit.starts_with("hour") => format!("{value} 小时"),
            Some(value) if unit.starts_with("second") => format!("{value} 秒"),
            Some(value) if (value % 60.0) == 0.0 => format!("{} 小时", value / 60.0),
            Some(value) => format!("{value} 分钟"),
            None => String::new(),
        };
        let duration_label = if duration_text.is_empty() {
            String::new()
        } else {
            format!("{duration_text}窗口")
        };

        let label = text(field(raw, &["__label"]))
            .or_else(|| text(field(raw, &["label", "name", "title", "scope"])))
            .or_else(|| text(field(detail, &["name", "title", "scope"])))
            .or_else(|| (!duration_label.is_empty()).then_some(duration_label))
            .unwrap_or_else(|| format!("额度 {}", index + 1));

        let remaining_percent = match (limit, used_value) {
            (Some(limit), used) if limit > 0.0 => {
                Some(clamp_percent((limit - used.unwrap_or(0.0)).max(0.0) / limit * 100.0))
            }
            (_, Some(used)) if used > 0.0 => Some(0.0),
            _ => None,
        };

        windows.push(QuotaWindow {
            label,
            remaining_percent,
            reset_at_ms: reset_at(
                detail,
                &["reset_at", "resetAt", "reset_time", "resetTime"],
                &["reset_in", "resetIn", "ttl"],
            ),
            detail: limit.map(|limit| format!("{} / {limit}", used_value.unwrap_or(0.0))),
        });
    }
    windows
}

/// 从 Claude `api/oauth/profile` 响应里取套餐名。
pub fn claude_plan(payload: &Value) -> Option<String> {
    let account = object(payload, &["account"]);
    let organization = object(payload, &["organization"]);
    if truthy(field(account.unwrap_or(&Value::Null), &["has_claude_max"])) == Some(true) {
        return Some("Max".to_string());
    }
    if truthy(field(account.unwrap_or(&Value::Null), &["has_claude_pro"])) == Some(true) {
        return Some("Pro".to_string());
    }
    if text(field(organization.unwrap_or(&Value::Null), &["organization_type"]))
        .map(|item| item.eq_ignore_ascii_case("claude_team"))
        == Some(true)
        && text(field(
            organization.unwrap_or(&Value::Null),
            &["subscription_status"],
        ))
        .map(|item| item.eq_ignore_ascii_case("active"))
            == Some(true)
    {
        return Some("Team".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn labels(windows: &[QuotaWindow]) -> Vec<String> {
        windows.iter().map(|item| item.label.clone()).collect()
    }

    #[test]
    fn codex_parses_windows_and_sorts_five_hour_first() {
        let payload = json!({
            "rate_limit": {
                "primary_window": {
                    "used_percent": 25.0,
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 3600
                },
                "secondary_window": {
                    "used_percent": 10,
                    "limit_window_seconds": 18000,
                    "reset_at": 1800000000
                }
            },
            "code_review_rate_limit": {
                "primary_window": { "used_percent": 50, "limit_window_seconds": 18000 }
            },
            "additional_rate_limits": [
                { "limit_name": "GPT-5", "rate_limit": { "primary_window": { "used_percent": 80 } } }
            ],
            "rate_limit_reset_credits": { "available_count": 2, "applicable_available_count": 1 }
        });

        let (windows, credits) = codex(&payload);
        let labels = labels(&windows);
        // 5 小时窗口必须排在每周之前（上游给的是 primary=每周）。
        assert_eq!(labels[0], "5 小时");
        assert_eq!(labels[1], "每周");
        assert_eq!(labels[2], "代码审查 5 小时");
        assert_eq!(labels[3], "GPT-5 5 小时");
        assert_eq!(windows[0].remaining_percent, Some(90.0));
        assert_eq!(windows[1].remaining_percent, Some(75.0));
        let credits = credits.expect("reset credits");
        assert_eq!(credits.available, Some(2));
        assert_eq!(credits.applicable, Some(1));
    }

    #[test]
    fn codex_treats_limit_reached_as_zero_remaining() {
        let payload = json!({
            "rate_limit": {
                "primary_window": { "used_percent": 100, "limit_window_seconds": 18000 },
                "limit_reached": true,
                "allowed": false
            }
        });
        let (windows, credits) = codex(&payload);
        assert!(credits.is_none());
        assert_eq!(windows[0].remaining_percent, Some(0.0));
    }

    #[test]
    fn codex_marks_reached_window_zero_when_percent_missing() {
        // `limit_reached` / `allowed` 与窗口同级（在 rate_limit 块上），
        // 不是放在窗口里 —— 对照 EasyCLIProxyAPI 的 `source.limit_reached`。
        let payload = json!({
            "rate_limit": {
                "primary_window": { "limit_window_seconds": 18000, "reset_at": 1800000000 },
                "limit_reached": true
            }
        });
        let (windows, _) = codex(&payload);
        assert_eq!(windows[0].remaining_percent, Some(0.0));

        // allowed=false 同样视为触顶。
        let payload = json!({
            "rate_limit": {
                "primary_window": { "limit_window_seconds": 18000, "reset_at": 1800000000 },
                "allowed": false
            }
        });
        let (windows, _) = codex(&payload);
        assert_eq!(windows[0].remaining_percent, Some(0.0));
    }

    #[test]
    fn codex_keeps_remaining_unknown_when_reached_without_reset_time() {
        // 触顶但连重置时间都没有时，上游数据不足以判定「0% 剩余」，
        // 必须保持未知（null）而不是编一个 0 —— 0 代表「已用尽」。
        let payload = json!({
            "rate_limit": {
                "primary_window": { "limit_window_seconds": 18000 },
                "limit_reached": true
            }
        });
        let (windows, _) = codex(&payload);
        assert_eq!(windows[0].remaining_percent, None);
    }

    #[test]
    fn codex_skips_windows_without_usable_data() {
        let payload = json!({ "rate_limit": { "primary_window": { "used_percent": 5 } } });
        let (windows, credits) = codex(&payload);
        assert_eq!(windows.len(), 1);
        assert!(credits.is_none());
        assert_eq!(windows[0].remaining_percent, Some(95.0));
    }

    #[test]
    fn claude_parses_fixed_windows_and_extra_usage() {
        let payload = json!({
            "five_hour": { "utilization": 12.5, "resets_at": "2030-01-01T00:00:00Z" },
            "seven_day": { "utilization": 40 },
            "seven_day_opus": { "utilization": 0 },
            "seven_day_cowork": { "utilization": 3 },
            "extra_usage": {
                "is_enabled": true,
                "monthly_limit": 10000,
                "used_credits": 2500
            }
        });

        let windows = claude(&payload);
        assert_eq!(
            labels(&windows),
            vec![
                "5 小时",
                "7 天",
                "7 天（Opus）",
                "7 天（协作）",
                "额外用量"
            ]
        );
        assert_eq!(windows[0].remaining_percent, Some(87.5));
        assert_eq!(windows[1].remaining_percent, Some(60.0));
        assert_eq!(windows[4].remaining_percent, Some(75.0));
        assert_eq!(windows[4].detail.as_deref(), Some("$25.00 / $100.00"));
        assert!(windows[0].reset_at_ms.is_some());
    }

    #[test]
    fn claude_ignores_windows_without_utilization_and_disabled_extra_usage() {
        let payload = json!({
            "five_hour": { "utilization": 10 },
            "seven_day_sonnet": { "resets_at": "2030-01-01T00:00:00Z" },
            "extra_usage": { "is_enabled": false, "monthly_limit": 100 }
        });
        let windows = claude(&payload);
        assert_eq!(labels(&windows), vec!["5 小时"]);
    }

    #[test]
    fn claude_plan_detects_max_pro_and_team() {
        assert_eq!(
            claude_plan(&json!({"account": {"has_claude_max": true}})).as_deref(),
            Some("Max")
        );
        assert_eq!(
            claude_plan(&json!({"account": {"has_claude_pro": true}})).as_deref(),
            Some("Pro")
        );
        assert_eq!(
            claude_plan(&json!({
                "organization": {"organization_type": "claude_team", "subscription_status": "active"}
            }))
            .as_deref(),
            Some("Team")
        );
        assert_eq!(claude_plan(&json!({"account": {}})), None);
    }

    #[test]
    fn antigravity_parses_groups_and_buckets_with_window_order() {
        let payload = json!({
            "groups": [{
                "display_name": "Gemini 模型",
                "description": "每日额度",
                "buckets": [
                    { "window": "weekly", "remaining_fraction": 0.5, "reset_time": "2030-01-01T00:00:00Z" },
                    { "window": "5h", "remaining_fraction": 0.25 }
                ]
            }]
        });
        let windows = antigravity(&payload);
        assert_eq!(windows.len(), 2);
        // 5h 桶必须排到 weekly 之前。
        assert_eq!(windows[0].label, "Gemini 模型 · 5h");
        assert_eq!(windows[0].remaining_percent, Some(25.0));
        assert_eq!(windows[1].label, "Gemini 模型 · weekly");
        assert_eq!(windows[1].remaining_percent, Some(50.0));
        assert_eq!(windows[1].detail.as_deref(), Some("每日额度"));
    }

    #[test]
    fn antigravity_unwraps_body_string_and_percent_strings() {
        let payload = json!({
            "body": "{\"groups\":[{\"display_name\":\"G\",\"buckets\":[{\"window\":\"5h\",\"remaining_fraction\":\"42%\"}]}]}"
        });
        let windows = antigravity(&payload);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].remaining_percent, Some(42.0));
        // 组名 + 窗口名拼接：单看 "5h" 认不出是哪个套餐组的额度。
        assert_eq!(windows[0].label, "G · 5h");
    }

    #[test]
    fn antigravity_uses_group_name_when_bucket_has_no_label() {
        let payload = json!({
            "groups": [{
                "display_name": "Gemini 模型",
                "buckets": [{ "remaining_fraction": 0.5 }]
            }]
        });
        let windows = antigravity(&payload);
        assert_eq!(windows[0].label, "Gemini 模型");
        assert_eq!(windows[0].remaining_percent, Some(50.0));
    }

    #[test]
    fn antigravity_plan_prefers_paid_tier_and_reads_project() {
        let payload = json!({
            "currentTier": { "id": "free-tier", "name": "Free" },
            "paidTier": { "id": "g1-ultra-tier", "name": "Ultra" },
            "cloudaicompanionProject": "my-project-123"
        });
        let (plan, project) = antigravity_plan(&payload);
        assert_eq!(plan.as_deref(), Some("Ultra"));
        assert_eq!(project.as_deref(), Some("my-project-123"));
    }

    #[test]
    fn antigravity_plan_falls_back_to_current_tier() {
        let (plan, project) = antigravity_plan(&json!({ "currentTier": { "id": "g1-pro-tier" } }));
        assert_eq!(plan.as_deref(), Some("Pro"));
        assert_eq!(project, None);
    }

    #[test]
    fn xai_parses_weekly_period_and_products() {
        let payload = json!({
            "config": {
                "currentPeriod": { "type": "WEEKLY", "start": "2030-01-01T00:00:00Z", "end": "2030-01-08T00:00:00Z" },
                "creditUsagePercent": 30,
                "productUsage": [{ "product": "Grok Code", "usagePercent": 10 }]
            }
        });
        let windows = xai(&payload);
        assert_eq!(labels(&windows), vec!["每周", "Grok Code"]);
        assert_eq!(windows[0].remaining_percent, Some(70.0));
        assert_eq!(windows[1].remaining_percent, Some(90.0));
        assert!(windows[0].reset_at_ms.is_some());
    }

    #[test]
    fn xai_handles_val_wrapped_cents_and_on_demand_split() {
        let payload = json!({
            "current_period": { "type": "monthly", "end": "2030-02-01T00:00:00Z" },
            "monthly_limit": { "val": 10000 },
            "used": { "val": 15000 },
            "on_demand_cap": { "val": 5000 }
        });
        let windows = xai(&payload);
        let by_label = |name: &str| {
            windows
                .iter()
                .find(|item| item.label == name)
                .unwrap_or_else(|| panic!("missing {name}"))
        };
        // 套餐内用满 100%，超出 5000 美分算按需用量。
        assert_eq!(by_label("本月套餐内用量").remaining_percent, Some(0.0));
        assert_eq!(by_label("按需用量").remaining_percent, Some(0.0));
        assert_eq!(
            by_label("按需用量").detail.as_deref(),
            Some("$0.00 / $50.00")
        );
    }

    #[test]
    fn xai_accepts_bare_config_without_wrapper() {
        let windows = xai(&json!({ "creditUsagePercent": 5, "currentPeriod": { "type": "weekly" } }));
        assert_eq!(windows[0].remaining_percent, Some(95.0));
    }

    #[test]
    fn kimi_parses_limits_and_usage_with_duration_labels() {
        let payload = json!({
            "limits": [
                {
                    "detail": { "limit": 100, "used": 25, "reset_at": "2030-01-01T00:00:00Z" },
                    "window": { "duration": 5, "timeUnit": "TIME_UNIT_HOUR" }
                },
                { "name": "每周额度", "detail": { "limit": 200, "remaining": 50 } }
            ],
            "usage": { "name": "额外用量", "detail": { "limit": 10, "used": 10 } }
        });

        let windows = kimi(&payload);
        assert_eq!(
            labels(&windows),
            vec!["5 小时窗口", "每周额度", "额外用量"]
        );
        assert_eq!(windows[0].remaining_percent, Some(75.0));
        assert_eq!(windows[0].detail.as_deref(), Some("25 / 100"));
        assert_eq!(windows[1].remaining_percent, Some(25.0));
        assert_eq!(windows[2].remaining_percent, Some(0.0));
        assert!(windows[0].reset_at_ms.is_some());
    }

    #[test]
    fn kimi_skips_entries_without_any_numeric_signal() {
        let windows = kimi(&json!({ "limits": [{ "detail": { "name": "empty" } }] }));
        assert!(windows.is_empty());
    }

    #[test]
    fn malformed_payloads_never_produce_fake_zero_windows() {
        assert!(codex(&json!("not-an-object")).0.is_empty());
        assert!(claude(&json!({})).is_empty());
        assert!(antigravity(&json!({ "groups": "nope" })).is_empty());
        assert!(xai(&json!({})).is_empty());
        assert!(kimi(&json!({})).is_empty());
    }

    #[test]
    fn remaining_percent_is_clamped_to_valid_range() {
        let payload = json!({
            "five_hour": { "utilization": 150 },
            "seven_day": { "utilization": -20 }
        });
        let windows = claude(&payload);
        assert_eq!(windows[0].remaining_percent, Some(0.0));
        assert_eq!(windows[1].remaining_percent, Some(100.0));
    }

    #[test]
    fn instant_accepts_seconds_millis_and_iso() {
        assert_eq!(instant(Some(&json!(1_800_000_000))), Some(1_800_000_000_000));
        assert_eq!(
            instant(Some(&json!(1_800_000_000_000i64))),
            Some(1_800_000_000_000)
        );
        assert_eq!(
            instant(Some(&json!("2030-01-01T00:00:00Z"))),
            Some(1_893_456_000_000)
        );
        assert_eq!(instant(Some(&json!("nonsense"))), None);
        assert_eq!(instant(Some(&json!(0))), None);
    }

    #[test]
    fn duration_label_matches_known_windows() {
        assert_eq!(duration_label(Some(FIVE_HOUR_SECONDS)), "5 小时");
        assert_eq!(duration_label(Some(WEEK_SECONDS)), "每周");
        assert_eq!(duration_label(Some(30.0 * 86_400.0)), "每月");
        assert_eq!(duration_label(Some(86_400.0)), "1 天");
        assert_eq!(duration_label(Some(3_600.0)), "1 小时");
        assert_eq!(duration_label(None), "");
    }
}
