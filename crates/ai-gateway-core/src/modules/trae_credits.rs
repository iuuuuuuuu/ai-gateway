//! Trae 积分与套餐身份：IDE 积分 / 权益包 / 付费身份 / 每日快照。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/commands/accounts.rs`
//! （`fetch_credit_detail` / `fetch_remaining_credits` / `refresh_pay_status` /
//! `credits_daily_list`）与 `src-tauri/src/commands/trae_apps.rs`
//! （`apps_entitlement_read` / `refresh_pay_status`），按本仓库「core 不依赖 Tauri」
//! 的约定改写为纯逻辑 + 异步查询。
//!
//! ## 为什么积分要分三个来源
//!
//! Trae 的额度不是一个数字，而是**三条互不相加的账**：
//!
//! | 来源 | 接口 | 说明 |
//! |---|---|---|
//! | IDE 积分 | `ide_user_credit_detail_v2` | 通用积分，签到发的那份 |
//! | 权益包 | `remaining_credits` | 套餐附赠/活动赠品，各带独立到期时间 |
//! | 付费身份 | `ide_user_pay_status` | `identityStr`，决定账号是不是 Pro/试用 |
//!
//! 把三者混成一个「余额」会让用户看到「还剩 500 分」却不知道哪部分本月到期。
//! 因此本模块分来源采集、分来源展示，并由 [`daily_snapshot`] 按天落盘供趋势图消费。
//!
//! ## 为什么要每日快照而不是实时算
//!
//! 趋势图（近 14/30 天）需要历史点，而接口只给当前值。每日一行、90 天滚动裁剪，
//! 既够画图也不会无限增长。同一天重复查询**覆盖**当天行而不是追加 ——
//! 否则一天开十次界面就会在图上留下十个尖刺。

use serde_json::{json, Value};

use crate::modules::{config, trae_account, trae_device};

/// IDE 积分明细分页大小。
const CREDIT_PAGE_SIZE: i64 = 100;
/// 积分历史保留天数。
pub const CREDITS_HISTORY_KEEP_DAYS: i64 = 90;

/// 需要携带完整客户端指纹头的查询端点（2026-09 实测：新签发的 JWT 会校验设备指纹，
/// 只带 `authorization` 会 401）。
pub const EP_IDE_USER_PAY_STATUS: &str =
    "https://api.trae.cn/trae/api/v2/ide_user_pay_status";
pub const EP_REMAINING_CREDITS: &str =
    "https://api.trae.cn/trae/api/v2/ide_user_remaining_credits";
pub const EP_CREDIT_DETAIL: &str =
    "https://api.trae.cn/trae/api/v2/ide_user_credit_detail_v2";

/// 套餐身份（`pay_status` 响应里我们要的三个字段）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PayIdentity {
    /// 身份标识（`identityStr` / `identity`），如 `Pro` / `Free` / `Trial`。
    pub identity: Option<String>,
    /// 身份到期时间（原样字符串，上游格式不统一，不强行解析成时间类型）。
    pub expire_at: Option<String>,
    /// 原始响应，供界面兜底展示与排查。
    pub raw: Value,
}

impl PayIdentity {
    pub fn to_json(&self) -> Value {
        json!({
            "identity": self.identity,
            "expireAt": self.expire_at,
            "raw": self.raw,
        })
    }
}

/// 单个权益包。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreditPack {
    pub name: String,
    pub remaining: i64,
    pub total: Option<i64>,
    pub expire_at: Option<String>,
}

impl CreditPack {
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "remaining": self.remaining,
            "total": self.total,
            "expireAt": self.expire_at,
        })
    }
}

/// 从一个已经下钻过的对象里找第一个存在的键（宽窄两套命名）。
fn pick<'a>(obj: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| obj.get(*k))
}

/// 宽容取整数：接受整数、整值浮点、数字字符串；拒绝布尔与小数。
///
/// 与签到模块同口径 —— 上游同一字段在不同版本里可能是 `"50"` 或 `50.0`，
/// 严格 `as_i64` 会把这两种都读成 0，表现为「积分全是 0」。
fn as_int_tolerant(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(i)
            } else {
                let f = n.as_f64()?;
                if f.fract() == 0.0 {
                    Some(f as i64)
                } else {
                    None
                }
            }
        }
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// 逐层剥开信封键（外层在前）。
fn unwrap_scopes(value: &Value) -> Vec<&Value> {
    const ENVELOPE_KEYS: &[&str] = &["data", "result", "resp", "response"];
    let mut out = vec![value];
    let mut cur = value;
    for _ in 0..8 {
        let Some(next) = ENVELOPE_KEYS.iter().find_map(|k| cur.get(*k)) else {
            break;
        };
        if next.is_null() {
            break;
        }
        out.push(next);
        cur = next;
    }
    out
}

/// 解析 `pay_status` 响应为 [`PayIdentity`]（纯函数，可离线测）。
pub fn parse_pay_status(resp: &Value) -> PayIdentity {
    const IDENTITY_KEYS: &[&str] = &["identityStr", "identity", "identity_str", "planName", "plan"];
    const EXPIRE_KEYS: &[&str] = &[
        "identityExpireAt",
        "identity_expire_at",
        "expireAt",
        "expire_at",
        "expiredAt",
    ];
    for scope in unwrap_scopes(resp) {
        let identity = pick(scope, IDENTITY_KEYS)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if identity.is_none() {
            continue;
        }
        let expire_at = pick(scope, EXPIRE_KEYS)
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .filter(|s| !s.is_empty());
        return PayIdentity {
            identity,
            expire_at,
            raw: resp.clone(),
        };
    }
    PayIdentity {
        identity: None,
        expire_at: None,
        raw: resp.clone(),
    }
}

/// 解析 `remaining_credits` 响应为权益包列表（纯函数）。
///
/// 数组字段名在不同版本里有 `credits` / `packages` / `gifts` / `list`，
/// 条目内余额字段同理 —— 逐个候选查找而不是认死一个名字。
pub fn parse_credit_packs(resp: &Value) -> Vec<CreditPack> {
    const ARRAY_KEYS: &[&str] = &["credits", "packages", "gifts", "list", "items"];
    const NAME_KEYS: &[&str] = &["name", "packageName", "creditName", "title", "type"];
    const REMAIN_KEYS: &[&str] = &["remaining", "remain", "balance", "left", "remainingCredits"];
    const TOTAL_KEYS: &[&str] = &["total", "totalCredits", "amount", "quota"];
    const EXPIRE_KEYS: &[&str] = &["expireAt", "expire_at", "expiredAt", "expirationTime"];

    for scope in unwrap_scopes(resp) {
        let Some(items) = ARRAY_KEYS
            .iter()
            .find_map(|k| scope.get(*k))
            .and_then(Value::as_array)
        else {
            continue;
        };
        if items.is_empty() {
            continue;
        }
        let mut out = Vec::new();
        for item in items {
            let remaining = pick(item, REMAIN_KEYS)
                .and_then(as_int_tolerant)
                .unwrap_or(0);
            out.push(CreditPack {
                name: pick(item, NAME_KEYS)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("积分包")
                    .to_string(),
                remaining,
                total: pick(item, TOTAL_KEYS).and_then(as_int_tolerant),
                expire_at: pick(item, EXPIRE_KEYS).map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                }),
            });
        }
        return out;
    }
    Vec::new()
}

/// 解析 IDE 积分明细响应为总剩余积分（纯函数）。
pub fn parse_credit_total(resp: &Value) -> Option<i64> {
    const TOTAL_KEYS: &[&str] = &[
        "totalRemaining",
        "total_remaining",
        "remaining",
        "balance",
        "credits",
        "total",
    ];
    for scope in unwrap_scopes(resp) {
        if let Some(v) = pick(scope, TOTAL_KEYS).and_then(as_int_tolerant) {
            return Some(v);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 网络查询
// ---------------------------------------------------------------------------

/// 查询类请求共用的客户端（超时 30s；积分接口偶发慢响应，但不能无限等）。
fn agent() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("构造 HTTP 客户端失败: {e}"))
}

/// Trae IDE 查询类 POST：挂完整客户端指纹头。
///
/// 与签到模块分离是**刻意**的 —— 签到接口不校验设备指纹，而 iде 查询接口校验；
/// 合并成一份「通用头」会让签到也带上多余的指纹字段。
pub fn ide_query_headers(jwt: &str, dev: &trae_device::DeviceEntry) -> Vec<(String, String)> {
    let auth = if jwt.trim_start().starts_with("Cloud-IDE-JWT ") {
        jwt.trim().to_string()
    } else {
        format!("Cloud-IDE-JWT {}", jwt.trim())
    };
    vec![
        ("authorization".to_string(), auth),
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
        ("x-device-id".to_string(), dev.device_id.clone()),
        ("x-market-user-id".to_string(), dev.market_user_id.clone()),
        ("x-session-id".to_string(), dev.session_id.clone()),
        ("x-app-id".to_string(), "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8".to_string()),
        ("x-platform".to_string(), "IDE_PC".to_string()),
        ("x-request-id".to_string(), crate::modules::trae_checkin::random_hex(32)),
    ]
}

/// POST 一个 JSON 体，返回解析后的响应。
async fn ide_post(url: &str, jwt: &str, dev: &trae_device::DeviceEntry, body: Value) -> Result<Value, String> {
    let mut req = agent()?.post(url).json(&body);
    for (k, v) in ide_query_headers(jwt, dev) {
        req = req.header(k, v);
    }
    let resp = req.send().await.map_err(|e| format!("请求失败: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "HTTP {status}: {}",
            text.chars().take(200).collect::<String>()
        ));
    }
    serde_json::from_str(&text).map_err(|e| {
        format!(
            "响应不是 JSON: {e}（原始前 200 字: {}）",
            text.chars().take(200).collect::<String>()
        )
    })
}

/// 取账号的 (uid, jwt, device)。
fn resolve(uid: &str) -> Result<(String, String, trae_device::DeviceEntry), String> {
    let acc = trae_account::find_account(uid).ok_or_else(|| format!("账号 {uid} 不存在"))?;
    let jwt = acc
        .get("jwt")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if jwt.trim().is_empty() {
        return Err(format!("账号 {uid} 没有 JWT，无法查询积分"));
    }
    let dev = trae_device::resolve_device(uid);
    Ok((uid.to_string(), jwt, dev))
}

/// 查询单个账号的付费身份。
pub async fn fetch_pay_status(uid: &str) -> Result<PayIdentity, String> {
    let (_uid, jwt, dev) = resolve(uid)?;
    let resp = ide_post(EP_IDE_USER_PAY_STATUS, &jwt, &dev, json!({})).await?;
    Ok(parse_pay_status(&resp))
}

/// 查询单个账号的权益包。
pub async fn fetch_credit_packs(uid: &str) -> Result<Vec<CreditPack>, String> {
    let (_uid, jwt, dev) = resolve(uid)?;
    let resp = ide_post(
        EP_REMAINING_CREDITS,
        &jwt,
        &dev,
        json!({ "pageSize": CREDIT_PAGE_SIZE, "pageNum": 1 }),
    )
    .await?;
    Ok(parse_credit_packs(&resp))
}

/// 查询单个账号的 IDE 积分总额。
pub async fn fetch_credit_total(uid: &str) -> Result<Option<i64>, String> {
    let (_uid, jwt, dev) = resolve(uid)?;
    let resp = ide_post(
        EP_CREDIT_DETAIL,
        &jwt,
        &dev,
        json!({ "pageSize": CREDIT_PAGE_SIZE, "pageNum": 1 }),
    )
    .await?;
    Ok(parse_credit_total(&resp))
}

/// 单个账号的完整积分视图（三条账分别采集，互不因对方失败而中断）。
pub async fn fetch_credit_detail(uid: &str) -> Value {
    let mut errors = Vec::new();
    let identity = match fetch_pay_status(uid).await {
        Ok(v) => Some(v),
        Err(e) => {
            errors.push(format!("付费身份: {e}"));
            None
        }
    };
    let packs = match fetch_credit_packs(uid).await {
        Ok(v) => Some(v),
        Err(e) => {
            errors.push(format!("权益包: {e}"));
            None
        }
    };
    let ide_total = match fetch_credit_total(uid).await {
        Ok(v) => Some(v),
        Err(e) => {
            errors.push(format!("IDE 积分: {e}"));
            None
        }
    };
    let pack_total: i64 = packs
        .as_ref()
        .map(|p| p.iter().map(|x| x.remaining).sum())
        .unwrap_or(0);
    json!({
        "userId": uid,
        "identity": identity.as_ref().map(|i| i.to_json()),
        "packs": packs.as_ref().map(|p| p.iter().map(CreditPack::to_json).collect::<Vec<_>>()),
        "ideTotal": ide_total,
        "packTotal": pack_total,
        // 三条账互不相加：界面按来源分别展示，这里给出各自的数就好
        "errors": errors,
    })
}

/// 本机两个 Trae 应用当前登录账号的套餐身份缓存文件。
pub fn pay_status_file() -> std::path::PathBuf {
    config::store_dir().join("trae_pay_status.json")
}

/// 读取套餐身份缓存 `{statuses: {uid: entry}, updated_at}`。
pub fn load_pay_status_cache() -> Value {
    let path = pay_status_file();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(t.trim_start_matches('\u{feff}')).ok())
        .unwrap_or_else(|| json!({ "statuses": {}, "updated_at": null }))
}

fn save_pay_status_cache(cache: &Value) -> Result<(), String> {
    let path = pay_status_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(cache).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

/// 批量刷新全部账号的付费身份，返回成功数量。
pub async fn refresh_all_pay_status() -> usize {
    let accounts = trae_account::load_accounts();
    let mut cache = load_pay_status_cache();
    let mut ok = 0usize;
    for acc in &accounts {
        let Some(uid) = acc.get("user_id").and_then(Value::as_str) else {
            continue;
        };
        if let Ok(identity) = fetch_pay_status(uid).await {
            ok += 1;
            // 顺带把占位名换成身份名，用户一眼认出是哪个号
            if let Some(name) = identity.identity.as_deref() {
                trae_account::set_name_if_placeholder(uid, name);
            }
            if let Some(map) = cache.get_mut("statuses").and_then(Value::as_object_mut) {
                map.insert(uid.to_string(), identity.to_json());
            }
        }
    }
    if let Some(obj) = cache.as_object_mut() {
        obj.insert("updated_at".to_string(), json!(config::utc_iso()));
    }
    let _ = save_pay_status_cache(&cache);
    ok
}

// ---------------------------------------------------------------------------
// 每日快照
// ---------------------------------------------------------------------------

/// 积分历史文件（按日期一行一条）。
pub fn credits_history_file() -> std::path::PathBuf {
    config::store_dir().join("trae_credits_history.json")
}

/// 读取积分历史。
pub fn load_credits_history() -> Vec<Value> {
    let path = credits_history_file();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(t.trim_start_matches('\u{feff}')).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

/// 写入一条当天积分快照（同一天**覆盖**，不追加）。
///
/// 覆盖而不是追加：趋势图按天画点，一天内反复查询若都追加，图上会出现
/// 十几个同一天的尖刺，看起来像积分剧烈波动，实际只是用户多开了几次界面。
pub fn daily_snapshot(uid: &str, total: i64, delta: i64) -> Result<(), String> {
    use chrono::Utc;
    let day = Utc::now().format("%Y-%m-%d").to_string();
    let mut rows = load_credits_history();
    if let Some(existing) = rows
        .iter_mut()
        .find(|r| {
            r.get("date").and_then(Value::as_str) == Some(day.as_str())
                && r.get("userId").and_then(Value::as_str) == Some(uid)
        })
    {
        if let Some(obj) = existing.as_object_mut() {
            obj.insert("credits".to_string(), json!(total));
            obj.insert("delta".to_string(), json!(delta));
            obj.insert("updatedAt".to_string(), json!(config::utc_iso()));
        }
    } else {
        rows.push(json!({
            "date": day,
            "userId": uid,
            "credits": total,
            "delta": delta,
            "updatedAt": config::utc_iso(),
        }));
    }
    // 90 天滚动裁剪：不裁剪的话文件会随使用年限无限增长
    let cutoff = (Utc::now() - chrono::Duration::days(CREDITS_HISTORY_KEEP_DAYS))
        .format("%Y-%m-%d")
        .to_string();
    rows.retain(|r| {
        r.get("date")
            .and_then(Value::as_str)
            .map(|d| d >= cutoff.as_str())
            .unwrap_or(false)
    });
    rows.sort_by(|a, b| {
        a.get("date")
            .and_then(Value::as_str)
            .unwrap_or("")
            .cmp(b.get("date").and_then(Value::as_str).unwrap_or(""))
    });
    let path = credits_history_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(&Value::Array(rows)).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 付费身份_顶层与信封都能解析() {
        let a = parse_pay_status(&json!({"identityStr": "Pro", "identityExpireAt": "2027-01-01"}));
        assert_eq!(a.identity.as_deref(), Some("Pro"));
        assert_eq!(a.expire_at.as_deref(), Some("2027-01-01"));

        let b = parse_pay_status(&json!({"data": {"identity": "Trial"}}));
        assert_eq!(b.identity.as_deref(), Some("Trial"));
    }

    #[test]
    fn 付费身份_缺失时返回_none_而不伪造() {
        let a = parse_pay_status(&json!({"code": 0, "data": {}}));
        assert!(a.identity.is_none());
    }

    #[test]
    fn 权益包_解析多个来源键名() {
        let packs = parse_credit_packs(&json!({
            "data": {"credits": [
                {"name": "活动赠送", "remaining": 500, "total": 500, "expireAt": "2026-12-31"},
                {"packageName": "套餐", "remain": "1200", "quota": 2000},
            ]}
        }));
        assert_eq!(packs.len(), 2);
        assert_eq!(packs[0].name, "活动赠送");
        assert_eq!(packs[0].remaining, 500);
        assert_eq!(packs[1].name, "套餐");
        assert_eq!(packs[1].remaining, 1200, "数字字符串也要能读");
        assert_eq!(packs[1].total, Some(2000));
    }

    #[test]
    fn 权益包_空数组与缺字段都返回空而不报错() {
        assert!(parse_credit_packs(&json!({"credits": []})).is_empty());
        assert!(parse_credit_packs(&json!({"data": {}})).is_empty());
        assert!(parse_credit_packs(&json!({})).is_empty());
    }

    #[test]
    fn 权益包_条目缺余额时按_0_计而不是丢弃() {
        // 条目存在但字段缺失 → 0；直接丢弃会让界面少显示一个包，用户以为丢了权益
        let packs = parse_credit_packs(&json!({"packages": [{"name": "X"}]}));
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0].remaining, 0);
    }

    #[test]
    fn ide_积分总额_多候选键名() {
        assert_eq!(parse_credit_total(&json!({"totalRemaining": 42})), Some(42));
        assert_eq!(
            parse_credit_total(&json!({"data": {"total_remaining": "99"}})),
            Some(99)
        );
        assert_eq!(parse_credit_total(&json!({"data": {}})), None);
    }

    #[test]
    fn 宽容整数_拒绝小数与布尔() {
        assert_eq!(as_int_tolerant(&json!(12)), Some(12));
        assert_eq!(as_int_tolerant(&json!(12.0)), Some(12));
        assert_eq!(as_int_tolerant(&json!(12.5)), None);
        assert_eq!(as_int_tolerant(&json!(true)), None);
        assert_eq!(as_int_tolerant(&json!("7")), Some(7));
        assert_eq!(as_int_tolerant(&json!("abc")), None);
    }

    #[test]
    fn 查询请求头_补_cloud_ide_jwt_前缀() {
        let dev = trae_device::DeviceEntry {
            device_id: "d".into(),
            session_id: "s".into(),
            market_user_id: "m".into(),
        };
        let h = ide_query_headers("abc", &dev);
        let auth = h.iter().find(|(k, _)| k == "authorization").unwrap();
        assert_eq!(auth.1, "Cloud-IDE-JWT abc");
        let h2 = ide_query_headers("Cloud-IDE-JWT abc", &dev);
        let auth2 = h2.iter().find(|(k, _)| k == "authorization").unwrap();
        assert_eq!(auth2.1, "Cloud-IDE-JWT abc", "已有前缀不得重复叠加");
    }
}
