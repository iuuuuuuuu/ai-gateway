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
//! | 额度总览 | `v2/pay/ide_user_ent_usage` | `usage_summary` 的总额与已用 |
//! | 权益包 | `v2/pay/ide_user_ent_usage` | `user_entitlement_pack_list`，各带独立到期时间 |
//! | 付费身份 | `v2/pay/ide_user_pay_status` | `user_pay_identity_str`，决定账号是不是 Pro/试用 |
//!
//! ★ 端点路径是 2026-10-04 用真实国内账号实测校准的：`/pay/` 这一层**不能少**
//! —— 少了就落到 TLB 的 404 HTML 上，界面表现为三条「HTTP 404: <html>…」。
//! `ide_user_remaining_credits` 与 `ide_user_credit_detail_v2` 在 Trae 客户端
//! 安装包里**零命中**，是本模块早期凭空写出来的，已删除。
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

use crate::modules::{config, trae_account, trae_device, trae_realm};

/// 积分历史保留天数。
pub const CREDITS_HISTORY_KEEP_DAYS: i64 = 90;

/// 付费身份查询路径（两个区域同路径，host 随账号所在区域变）。
const PATH_PAY_STATUS: &str = "/trae/api/v2/pay/ide_user_pay_status";
/// 权益包 / 总额 / 用量查询路径。v1 同路径但字段更少，故用 v2。
const PATH_ENT_USAGE: &str = "/trae/api/v2/pay/ide_user_ent_usage";

/// 按账号所在区域拼出查询端点。
///
/// ★ host 必须随 realm 走：国内版 `https://api.trae.cn`，国际版
/// `https://growsg-normal.trae.ai`（见 [`trae_realm::auth_host`]）。硬编码国内 host
/// 会让国际号的积分查询必然 401。
///
/// 注：早期注释称「只带 `authorization` 会 401，必须带完整设备指纹头」。2026-10-04
/// 用真实 JWT 实测，只带 `content-type` + `authorization` 即可 200 —— 设备指纹头
/// 仍然照发（无害且更接近客户端），但不再是必要条件。
fn endpoint(realm: trae_realm::Realm, path: &str) -> String {
    format!("{}{}", trae_realm::auth_host(realm), path)
}

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

/// 宽容取数（可带小数）。真实响应里 `usage.credits_amount` 是 `33.3536` 这种值，
/// 严格整数读取会把它当成 0 ⇒ 界面显示「一分没用」。
fn as_f64_tolerant(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// 宽容取整数（小数四舍五入；布尔 / 非数字串 ⇒ None）。
fn as_int_rounded(v: &Value) -> Option<i64> {
    as_f64_tolerant(v).map(|f| f.round() as i64)
}

/// 旧形状的到期时间原样转字符串（可能是 `2026-12-31` 这种日期串，也可能是 ISO）。
fn expiry_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// unix 秒 → RFC3339 串。
///
/// ★ 必须转：前端 `formatPackExpiry`（`src/pages/TraePage.tsx`）只认 ISO 串，
/// 拿到纯数字串会 `Date.parse` 失败后退化成 `slice(0, 10)`，把到期日显示成
/// `1793462399` 这样一串数字。真实响应里「永久」用 `0` 表示 ⇒ 下发 `None`
/// （前端显示「长期有效」），不要转成 1970 年。
fn unix_seconds_to_iso(secs: i64) -> Option<String> {
    if secs <= 0 {
        return None;
    }
    chrono::DateTime::from_timestamp(secs, 0).map(|d| d.to_rfc3339())
}

/// `Value` 形态的 unix 秒 → RFC3339 串。
fn value_unix_to_iso(v: &Value) -> Option<String> {
    as_int_tolerant(v).and_then(unix_seconds_to_iso)
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
    // ★ `user_pay_identity_str` 是客户端真实字段名（值如 "Free"）；早先只认
    // `identityStr`，于是真实响应里**一个都不匹配** ⇒ 身份恒为空 ⇒ 徽标不显示。
    // 注意同一响应里还有个 `user_pay_identity`（数字 0），那是枚举值不是名字，
    // 不要加进来 —— `as_str` 会把它过滤掉，加了只会让人误读。
    const IDENTITY_KEYS: &[&str] = &[
        "identityStr",
        "user_pay_identity_str",
        "identity",
        "identity_str",
        "planName",
        "plan",
    ];
    const EXPIRE_KEYS: &[&str] = &[
        "identityExpireAt",
        "identity_expire_at",
        "expireAt",
        "expire_at",
        "expiredAt",
        // ★ 真机映射里有这个键（main.js:1870505 的 pay_status 响应映射），但实测
        // 线上这份 ide_user_pay_status 响应体里**没有**它（见
        // test-bin/trae-pay-status-v2.json：只有 user_pay_identity /
        // user_pay_identity_str，顶层无任何到期字段）。既然真机读它，解析侧就得认
        // —— 哪天服务端开始下发，这里立刻能取到；不下发时自然落空，不会伪造。
        "last_pro_entitlement_expire_time",
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

/// 解析额度响应为权益包列表（纯函数）。
///
/// 数组字段名在不同版本里有 `credits` / `packages` / `gifts` / `list` /
/// `items`；真实接口 `v2/pay/ide_user_ent_usage` 用的是
/// `user_entitlement_pack_list` —— 逐个候选查找而不是认死一个名字。
pub fn parse_credit_packs(resp: &Value) -> Vec<CreditPack> {
    const ARRAY_KEYS: &[&str] = &[
        "credits",
        "packages",
        "gifts",
        "list",
        "items",
        "user_entitlement_pack_list",
    ];

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
            out.push(parse_one_pack(item));
        }
        return out;
    }
    Vec::new()
}

/// 解析单个权益包条目（纯函数）。
///
/// 两套形状都要认：
///   - **旧形状**（早先凭空写的候选键名，单测钉着）：余额与总额直接躺在条目上
///     （`remaining` / `remain` / `balance` …，`total` / `totalCredits` / `quota` …）。
///   - **真实形状**（`v2/pay/ide_user_ent_usage` 的 `user_entitlement_pack_list`）：
///     额度上限在 `entitlement_base_info.quota.credits_limit`，已用量在
///     `usage.credits_amount`（可能是 `33.3536` 这种小数，且**空对象表示已用 0**），
///     到期在 `expire_time`（unix 秒）。
fn parse_one_pack(item: &Value) -> CreditPack {
    const NAME_KEYS: &[&str] = &["name", "packageName", "creditName", "title", "type"];
    const DISPLAY_KEYS: &[&str] = &["display_desc", "group_name"];
    const REMAIN_KEYS: &[&str] = &["remaining", "remain", "balance", "left", "remainingCredits"];
    const TOTAL_KEYS: &[&str] = &["total", "totalCredits", "amount", "quota"];
    const EXPIRE_KEYS: &[&str] = &["expireAt", "expire_at", "expiredAt", "expirationTime"];

    let old_remaining = pick(item, REMAIN_KEYS).and_then(as_int_tolerant);
    let old_total = pick(item, TOTAL_KEYS).and_then(as_int_tolerant);
    let limit = item
        .get("entitlement_base_info")
        .and_then(|b| b.get("quota"))
        .and_then(|q| q.get("credits_limit"))
        .and_then(as_int_tolerant);
    let used = item
        .get("usage")
        .and_then(|u| u.get("credits_amount"))
        .and_then(as_int_rounded)
        .unwrap_or(0);

    let total = old_total.or(limit);
    let remaining = match (old_remaining, limit) {
        (Some(r), _) => r,
        (None, Some(l)) => (l - used).max(0),
        // ★ 免费额度包没有 `credits_limit`（它本身就是「无限」），此时只能给 0。
        // 前端 `packRatio` 见到 `total` 为 `null` 就不画进度条，不会显示成「0/0」。
        (None, None) => 0,
    };

    let name = pick(item, DISPLAY_KEYS)
        .or_else(|| pick(item, NAME_KEYS))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("积分包")
        .to_string();

    let expire_at = pick(item, EXPIRE_KEYS)
        .map(expiry_to_string)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            item.get("expire_time")
                .or_else(|| {
                    item.get("entitlement_base_info")
                        .and_then(|b| b.get("end_time"))
                })
                .and_then(value_unix_to_iso)
        });

    CreditPack {
        name,
        remaining,
        total,
        expire_at,
    }
}

/// 解析额度响应为**可用**积分总额（纯函数）。
///
/// ★ 真实形状（`v2/pay/ide_user_ent_usage`）：总额在 `usage_summary` 里，
/// 可用 = `total_amount` − `consumed_amount`。
/// 不能改用「逐包 remaining 求和」：免费额度包没有 `credits_limit`，
/// 求和会把它算成 0，界面上的总额就少了一块。
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
        if let Some(summary) = scope.get("usage_summary") {
            let total = summary.get("total_amount").and_then(as_int_rounded);
            let used = summary.get("consumed_amount").and_then(as_int_rounded);
            if let Some(t) = total {
                return Some(t - used.unwrap_or(0));
            }
        }
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

/// 取账号的 (uid, jwt, device, realm)。
///
/// ★ realm 必须一起取出来：积分端点的 host 随区域变（国内 `api.trae.cn` /
/// 国际 `growsg-normal.trae.ai`），硬编码国内 host 会让国际号必然 401。
fn resolve(uid: &str) -> Result<(String, String, trae_device::DeviceEntry, trae_realm::Realm), String> {
    let acc = trae_account::find_account(uid).ok_or_else(|| format!("账号 {uid} 不存在"))?;
    let jwt = acc
        .get("jwt")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if jwt.trim().is_empty() {
        return Err(format!("账号 {uid} 没有 JWT，无法查询积分"));
    }
    let realm = trae_account::account_realm(&acc).realm;
    let dev = trae_device::resolve_device(uid);
    Ok((uid.to_string(), jwt, dev, realm))
}

/// `v2/pay/ide_user_ent_usage` 的请求体。
///
/// ★ `require_usage` 必须为真 —— 否则响应里没有逐包的 `usage.credits_amount`，
/// 已用量就全成了 0。★ 绝不能加 `req_source`：实测带它会被上游判成非法请求
/// （HTTP 400 且响应体为空），排查时只看得到「空响应」。
fn ent_usage_body() -> Value {
    json!({ "require_usage": true })
}

/// `v2/pay/ide_user_pay_status` 的请求体。
///
/// ★ 真机不是发空对象：`main.js:1881578` 的 `_doFetchServerData` 组装
/// `{ trae_client: "IDE" | "Lite", device_id }`，其中 `trae_client` 由
/// 产品线决定（`Mqe.IDE = 1` / `Mqe.Lite = 2`）。此前这里发 `{}`，
/// 线上虽然也回 200，但那是**服务端容错**，不是契约 —— 一旦收紧就会静默退化。
/// 设备 id 取的就是我们已经在用的那份设备指纹（`x-device-id` 同源），
/// 不再额外造一个。
///
/// 产品线判定：本仓是 IDE 血统的账号库，没有 Lite/SOLO 的独立入口，
/// 故固定 `"IDE"`；设备 id 为空时只发 `trae_client`（宁可少发一个字段，
/// 也不要发空串被服务端判成非法设备）。
fn pay_status_body(dev: &trae_device::DeviceEntry) -> Value {
    let did = dev.device_id.trim();
    if did.is_empty() {
        json!({ "trae_client": "IDE" })
    } else {
        json!({ "trae_client": "IDE", "device_id": did })
    }
}

/// 查询单个账号的付费身份。
pub async fn fetch_pay_status(uid: &str) -> Result<PayIdentity, String> {
    let (_uid, jwt, dev, realm) = resolve(uid)?;
    let url = endpoint(realm, PATH_PAY_STATUS);
    let resp = ide_post(&url, &jwt, &dev, pay_status_body(&dev)).await?;
    Ok(parse_pay_status(&resp))
}

/// 查询单个账号的权益包。
pub async fn fetch_credit_packs(uid: &str) -> Result<Vec<CreditPack>, String> {
    let (_uid, jwt, dev, realm) = resolve(uid)?;
    let url = endpoint(realm, PATH_ENT_USAGE);
    let resp = ide_post(&url, &jwt, &dev, ent_usage_body()).await?;
    Ok(parse_credit_packs(&resp))
}

/// 查询单个账号的 IDE 积分总额。
pub async fn fetch_credit_total(uid: &str) -> Result<Option<i64>, String> {
    let (_uid, jwt, dev, realm) = resolve(uid)?;
    let url = endpoint(realm, PATH_ENT_USAGE);
    let resp = ide_post(&url, &jwt, &dev, ent_usage_body()).await?;
    Ok(parse_credit_total(&resp))
}

/// 把三条账的采集结果组装成前端契约形状（纯函数，便于单测钉住字段名）。
///
/// ★ 字段名必须与前端 `TraeCreditDetail`（`src/lib/api.ts`）**逐字对齐**，
/// 否则界面拿到的就是 `undefined`。这里踩过一个真实的崩溃：
///   - `packs` 曾经下发成 `null`（拿不到权益包时用 `.as_ref().map(...)` 包了一层），
///     而前端 `CreditCell` 直接读 `detail.packs.length` ⇒
///     `TypeError: Cannot read properties of null (reading 'length')`，
///     整个 Trae 账号页被错误边界接住变成一页报错。
///     ⇒ **`packs` / `errors` 恒为数组**，拿不到就是 `[]`，永不为 `null`。
///   - `identity` 是 `{identity, expireAt, raw}` 对象，前端要的是**扁平**的
///     `payIdentity`（字符串）+ `payExpireAt`，必须在这里拆平；此前直接下发
///     `identity` 对象，导致界面上「付费身份」徽标永远不显示。
///   - 总数此前叫 `ideTotal`，前端读的是 `total` ⇒ 界面上「当前可用总额」永远是 `—`。
///     现在两个键都发（`total` 给前端，`ideTotal` 留给旧调用方）。
fn credit_detail_json(
    uid: &str,
    identity: Option<&PayIdentity>,
    packs: Option<&[CreditPack]>,
    ide_total: Option<i64>,
    errors: Vec<String>,
) -> Value {
    let (pay_identity, identity_expire) = match identity {
        Some(i) => (
            // `i.identity` 本身已是 `Option<String>`：先 trim 再判空，
            // 上游偶尔回一个全空白的身份串，直接下发会让界面渲染空徽标。
            i.identity
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            i.expire_at.clone(),
        ),
        None => (None, None),
    };
    // ★ `payExpireAt`（界面上的「套餐到期」）：`pay_status` 响应里**根本没有**
    // 任何到期字段（2026-10-04 实测：全部字段列完也只有身份、试用、活动这些），
    // 所以这里以额度响应里各权益包最晚的到期时间为准；身份自带到期时间时优先用它
    // （将来上游补了这个字段就能自动接上，不必再改这里）。
    let pay_expire_at = identity_expire.clone().or_else(|| {
        packs
            .unwrap_or_default()
            .iter()
            .filter_map(|p| p.expire_at.as_deref())
            .max()
            .map(str::to_string)
    });
    let pack_list: Vec<Value> = packs
        .map(|p| p.iter().map(CreditPack::to_json).collect())
        .unwrap_or_default();
    let pack_total: i64 = packs.map(|p| p.iter().map(|x| x.remaining).sum()).unwrap_or(0);
    json!({
        "userId": uid,
        "total": ide_total,
        "ideTotal": ide_total,
        "packs": pack_list,
        "packTotal": pack_total,
        "payIdentity": pay_identity,
        "payExpireAt": pay_expire_at,
        "errors": errors,
    })
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
    // `fetch_credit_total` 返回 `Result<Option<i64>, String>`：接口成功但响应里
    // 找不到总额时是 `Ok(None)`，必须**拍平**成 `None`，否则会得到 `Some(None)`。
    let ide_total = match fetch_credit_total(uid).await {
        Ok(v) => v,
        Err(e) => {
            errors.push(format!("IDE 积分: {e}"));
            None
        }
    };
    // 三条账互不相加：界面按来源分别展示，这里给出各自的数就好
    credit_detail_json(uid, identity.as_ref(), packs.as_deref(), ide_total, errors)
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
    fn 付费身份_请求体带产品线与设备id() {
        let dev = trae_device::DeviceEntry {
            device_id: "123456789012345".to_string(),
            session_id: String::new(),
            market_user_id: String::new(),
        };
        let b = pay_status_body(&dev);
        assert_eq!(b.get("trae_client").and_then(Value::as_str), Some("IDE"));
        assert_eq!(
            b.get("device_id").and_then(Value::as_str),
            Some("123456789012345")
        );
    }

    #[test]
    fn 付费身份_设备id为空时不发空串() {
        let dev = trae_device::DeviceEntry {
            device_id: "   ".to_string(),
            session_id: String::new(),
            market_user_id: String::new(),
        };
        let b = pay_status_body(&dev);
        assert_eq!(b.get("trae_client").and_then(Value::as_str), Some("IDE"));
        assert!(b.get("device_id").is_none(), "空设备 id 绝不能发出去");
    }

    #[test]
    fn 付费身份_认真机那个_pro_到期键() {
        let a = parse_pay_status(&json!({
            "user_pay_identity_str": "Pro",
            "last_pro_entitlement_expire_time": "2027-03-01T00:00:00Z"
        }));
        assert_eq!(a.identity.as_deref(), Some("Pro"));
        assert_eq!(a.expire_at.as_deref(), Some("2027-03-01T00:00:00Z"));
    }

    #[test]
    fn 付费身份_线上真实响应体不伪造到期() {
        // 业主 2026-10-03 抓的真机响应（test-bin/trae-pay-status-v2.json 的骨架）：
        // 只有身份、没有任何到期字段 ⇒ 到期必须是 None，不能拿别的字段顶上。
        let a = parse_pay_status(&json!({
            "is_credits_billing": true,
            "server_time_ms": 1791028356473i64,
            "user_pay_identity": 0,
            "user_pay_identity_str": "Free"
        }));
        assert_eq!(a.identity.as_deref(), Some("Free"));
        assert!(a.expire_at.is_none(), "没有到期字段就必须是 None");
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

    // -----------------------------------------------------------------------
    // 前端契约（回归测试）
    //
    // 这一组测试的存在理由是一个真实崩溃：`fetch_credit_detail` 曾把 `packs`
    // 下发成 `null`，前端 `CreditCell` 直接读 `detail.packs.length` ⇒
    // `TypeError: Cannot read properties of null (reading 'length')`，
    // 整个 Trae 账号页被错误边界接住变成一页报错。
    //
    // 前端类型声明（`src/lib/api.ts` 的 `TraeCreditDetail`）是这份契约的
    // 另一侧，**改字段名必须两边同时改**。
    // -----------------------------------------------------------------------

    /// 三条账全部失败时的最坏形状：数组字段仍必须是数组，不是 `null`。
    #[test]
    fn 积分明细_三条账全失败时数组字段仍为数组() {
        let v = credit_detail_json(
            "u1",
            None,
            None,
            None,
            vec!["付费身份: 超时".into(), "权益包: 超时".into()],
        );
        assert!(v["packs"].is_array(), "packs 必须是数组，给 null 会让前端崩");
        assert_eq!(v["packs"].as_array().unwrap().len(), 0);
        assert!(v["errors"].is_array(), "errors 必须是数组");
        assert_eq!(v["errors"].as_array().unwrap().len(), 2);
        assert!(v["payIdentity"].is_null(), "没有身份时下发 null 而非空串");
        assert!(v["payExpireAt"].is_null());
        assert!(v["total"].is_null());
        assert_eq!(v["packTotal"], 0);
    }

    /// 前端读的键名必须逐字存在 —— 少一个键界面就是 `undefined`。
    #[test]
    fn 积分明细_键名与前端类型逐字一致() {
        let v = credit_detail_json("u1", None, None, Some(7), Vec::new());
        for k in [
            "userId",
            "total",
            "ideTotal",
            "packs",
            "packTotal",
            "payIdentity",
            "payExpireAt",
            "errors",
        ] {
            assert!(v.get(k).is_some(), "缺少前端要读的键: {k}");
        }
        assert_eq!(v["total"], 7, "前端读的是 total，不是 ideTotal");
        assert_eq!(v["ideTotal"], 7, "ideTotal 保留给旧调用方，两者必须同值");
    }

    /// 付费身份必须**拆平**成 `payIdentity` 字符串，而不是下发整个对象。
    #[test]
    fn 积分明细_付费身份被拆平为字符串() {
        let id = PayIdentity {
            identity: Some("个人版".into()),
            expire_at: Some("2126-01-01T00:00:00Z".into()),
            raw: json!({ "identity": "个人版" }),
        };
        let v = credit_detail_json("u1", Some(&id), None, None, Vec::new());
        assert_eq!(v["payIdentity"], "个人版", "必须扁平字符串，否则徽标永不显示");
        assert_eq!(v["payExpireAt"], "2126-01-01T00:00:00Z");
        assert!(v.get("identity").is_none(), "不再下发 identity 对象");
    }

    /// 身份为空串 / 上游根本没给身份时都归一成 `null` —— 否则界面渲染空徽标。
    #[test]
    fn 积分明细_空身份归一为_null() {
        let blank = PayIdentity {
            identity: Some("   ".into()),
            expire_at: None,
            raw: Value::Null,
        };
        let v = credit_detail_json("u1", Some(&blank), None, None, Vec::new());
        assert!(v["payIdentity"].is_null(), "全空白身份应归一为 null");

        let none = PayIdentity {
            identity: None,
            expire_at: None,
            raw: Value::Null,
        };
        let v2 = credit_detail_json("u1", Some(&none), None, None, Vec::new());
        assert!(v2["payIdentity"].is_null(), "上游没给身份也应归一为 null");
    }

    /// 权益包正常时的形状与合计。
    #[test]
    fn 积分明细_权益包列表与合计() {
        let packs = vec![
            CreditPack {
                name: "个人版积分包".into(),
                remaining: 1180,
                total: Some(2400),
                expire_at: Some("2026-12-01T00:00:00Z".into()),
            },
            CreditPack {
                name: "新用户体验包".into(),
                remaining: 386,
                total: None,
                expire_at: None,
            },
        ];
        let v = credit_detail_json("u1", None, Some(&packs), Some(8420), Vec::new());
        assert_eq!(v["packs"].as_array().unwrap().len(), 2);
        assert_eq!(v["packTotal"], 1180 + 386);
        assert_eq!(v["packs"][0]["name"], "个人版积分包");
        assert_eq!(v["packs"][0]["remaining"], 1180);
        assert!(v["packs"][1]["total"].is_null(), "total 缺失保持 null 而非 0");
        assert!(v["packs"][1]["expireAt"].is_null(), "不过期下发 null（前端显示长期有效）");
    }

    /// ★ 2026-10-04 实测响应：身份字段叫 `user_pay_identity_str`（值 "Free"），
    /// 早先只认 `identityStr` ⇒ 一个都不匹配 ⇒ 徽标永远不显示。
    #[test]
    fn 付费身份_真实字段名_user_pay_identity_str() {
        let resp = json!({
            "is_credits_billing": true,
            "user_pay_identity": 0,
            "user_pay_identity_str": "Free",
        });
        let a = parse_pay_status(&resp);
        assert_eq!(a.identity.as_deref(), Some("Free"));
        // 同一响应里没有到期字段 ⇒ 必须保持 None，不能拿别的数字凑。
        assert!(a.expire_at.is_none(), "真实响应没有到期字段，不得伪造");
    }

    /// ★ 真实 ent_usage 响应的逐包形状（字段名与层级照抄实测值）。
    fn real_ent_usage() -> Value {
        json!({
            "is_credits_billing": true,
            "usage_summary": {
                "consumed_amount": 133.35,
                "consumption_ratio": 0.1666875,
                "total_amount": 800,
            },
            "user_entitlement_pack_list": [
                {
                    "display_desc": "免费",
                    "expire_time": 1793462399,
                    "usage": {},
                    "entitlement_base_info": {
                        "product_id": 0,
                        "end_time": 1793462399,
                        "quota": {
                            "enable_solo_agent": true,
                            "no_bonus_quota": true,
                            "solo_agent_parallel_limit": 2,
                        },
                    },
                },
                {
                    "display_desc": "每月登录赠送",
                    "group_name": "每月登录积分",
                    "expire_time": 1793462399,
                    "usage": { "credits_amount": 33.3536 },
                    "entitlement_base_info": {
                        "product_id": 221,
                        "end_time": 1793462399,
                        "quota": { "credits_limit": 500, "no_bonus_quota": true },
                    },
                },
                {
                    "display_desc": "签到奖励",
                    "group_name": "每日签到",
                    "expire_time": 1793429317,
                    "usage": { "credits_amount": 100 },
                    "entitlement_base_info": {
                        "product_id": 208,
                        "quota": { "credits_limit": 100 },
                    },
                },
            ],
        })
    }

    /// 真实形状的权益包：额度上限在 `entitlement_base_info.quota.credits_limit`，
    /// 已用量在 `usage.credits_amount`（可为小数，空对象表示 0）。
    #[test]
    fn 权益包_真实_ent_usage_形状() {
        let packs = parse_credit_packs(&real_ent_usage());
        assert_eq!(packs.len(), 3);

        // 免费额度包：没有 credits_limit ⇒ total 保持 None，remaining 只能是 0。
        assert_eq!(packs[0].name, "免费");
        assert_eq!(packs[0].total, None, "没有 credits_limit 不能当成 0");
        assert_eq!(packs[0].remaining, 0);

        // 每月登录赠送：500 − 33.3536 ⇒ 467（四舍五入）。
        assert_eq!(packs[1].name, "每月登录赠送");
        assert_eq!(packs[1].total, Some(500));
        assert_eq!(packs[1].remaining, 467);

        // 签到奖励：100 − 100 ⇒ 0（用光了）。
        assert_eq!(packs[2].name, "签到奖励");
        assert_eq!(packs[2].total, Some(100));
        assert_eq!(packs[2].remaining, 0);

        // 到期时间必须是 ISO 串：前端 formatPackExpiry 只认 ISO。
        let exp = packs[1].expire_at.as_deref().unwrap();
        assert!(exp.starts_with("2026-") || exp.starts_with("2027-"), "unix 秒必须转 ISO，实际 {exp}");
        assert!(exp.contains('T'), "应是 RFC3339 串，实际 {exp}");
    }

    /// 总额取 `usage_summary` 的「总额 − 已用」，不是逐包 remaining 求和
    /// （免费包没有上限，求和会少一块）。
    #[test]
    fn ide_积分总额_真实_usage_summary() {
        assert_eq!(parse_credit_total(&real_ent_usage()), Some(800 - 133));
    }

    /// `usage` 为空对象 / 缺失时，已用量按 0 计（不能因为读不到就当整包没发）。
    #[test]
    fn 权益包_usage_为空表示没用量() {
        let v = json!({
            "user_entitlement_pack_list": [{
                "display_desc": "签到奖励",
                "expire_time": 0,
                "usage": {},
                "entitlement_base_info": { "quota": { "credits_limit": 100 } },
            }],
        });
        let packs = parse_credit_packs(&v);
        assert_eq!(packs[0].remaining, 100);
        assert!(packs[0].expire_at.is_none(), "expire_time=0 是「永久」，下发 null");
    }

    /// 「套餐到期」由各权益包最晚的到期时间推出来（pay_status 里根本没有这个字段）。
    #[test]
    fn 积分明细_套餐到期由最晚的包推出() {
        let packs = vec![
            CreditPack {
                name: "A".into(),
                remaining: 1,
                total: Some(1),
                expire_at: Some("2026-10-31T00:00:00+00:00".into()),
            },
            CreditPack {
                name: "B".into(),
                remaining: 1,
                total: Some(1),
                expire_at: Some("2026-11-30T00:00:00+00:00".into()),
            },
            CreditPack {
                name: "C".into(),
                remaining: 1,
                total: None,
                expire_at: None,
            },
        ];
        let v = credit_detail_json("u1", None, Some(&packs), None, Vec::new());
        assert_eq!(v["payExpireAt"], "2026-11-30T00:00:00+00:00");

        // 身份自带到期时间时优先用它（上游将来补字段就能自动接上）。
        let id = PayIdentity {
            identity: Some("Pro".into()),
            expire_at: Some("2126-01-01T00:00:00Z".into()),
            raw: Value::Null,
        };
        let v2 = credit_detail_json("u1", Some(&id), Some(&packs), None, Vec::new());
        assert_eq!(v2["payExpireAt"], "2126-01-01T00:00:00Z");
    }
}
