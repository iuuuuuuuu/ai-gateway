//! Trae 多账号签到。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/tasks/trae_checkin.rs`，按本仓库的
//! `reqwest` 异步栈改写（上游用同步 `ureq`）。
//!
//! ## 流程
//!
//! 1. `status` 预检 —— 今日已签则跳过（避免无谓的 claim 调用）
//! 2. `claim` —— 仅**网络异常**按重试次数重试；业务失败不重试（重试只会再失败一次）
//! 3. 错误分类 → 冷却落盘（区分「账号级失效」与「临时限流」）
//! 4. 积分归属三层兜底（claim 奖励字段 → 复查余额差值 → 旧行为）
//!
//! ## 请求头为什么这么多
//!
//! 服务端把 JWT 与签发时的**设备指纹**绑定：只带 `authorization` 会 401。
//! `x-device-id` / `vscode-sessionid` / `x-market-user-id` 必须与账号一一对应，
//! 由 [`crate::modules::trae_device`] 确定性派生。

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::modules::config;
use crate::modules::trae_account::{self, JwtInfo};
use crate::modules::trae_device::{self, DeviceEntry};

const SIGNIN_URL: &str = "https://api.trae.cn/trae/api/v2/ug/checkin_credits/claim";
const STATUS_URL: &str = "https://api.trae.cn/trae/api/v2/ug/checkin_credits/status";

/// JWT 剩余有效期低于该值（小时）时写入告警。
const EXPIRY_WARN_HOURS: f64 = 24.0;
/// 网络异常重试间隔。
const RETRY_GAP_SECS: u64 = 1;
/// 单次请求超时。
const REQUEST_TIMEOUT_SECS: u64 = 30;

/// claim 响应中疑似「本次奖励」的候选字段（按优先级排列）。
///
/// 为什么要这么多候选：上游不同版本/套餐下奖励字段名不一致；而 `status` 顶层的
/// `credits` 无法离线确证是「签到后余额」还是「可领奖励额度」，因此优先信 claim
/// 响应自身的奖励字段。
const CLAIM_REWARD_KEYS: [&str; 10] = [
    "reward",
    "reward_credits",
    "claim_credits",
    "checkin_credits",
    "delta",
    "increase",
    "obtain",
    "gained",
    "credits",
    "amount",
];

/// 响应包裹层键名（逐层深入找真实 payload）。
const ENVELOPE_KEYS: [&str; 5] = ["data", "result", "resp", "response", "info"];
/// 包裹层最大深度（防病态嵌套导致栈溢出）。
const MAX_DEPTH: usize = 8;

/// 失败分类（决定冷却时长与是否永久失效）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    /// 套餐限制（已领完 / 非会员），冷却 12 小时。
    PlanLimit,
    /// 软限流（429），冷却 60 秒。
    SoftRate,
    /// 会话失效（401）—— **永久**，只能重新登录。
    SessionDead,
    /// 接口不存在（404），冷却 60 秒。
    NotFound,
    /// 服务端错误（5xx），冷却 10 分钟。
    Server,
    /// 客户端错误（其他 4xx），冷却 10 分钟。
    Client,
    /// 业务错误码（HTTP 200 但 code != 0），冷却 5 分钟。
    BusinessError,
    /// 网络异常（请求未发出或超时）—— 不落冷却。
    Unknown,
}

impl ErrorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorKind::PlanLimit => "PlanLimit",
            ErrorKind::SoftRate => "SoftRate",
            ErrorKind::SessionDead => "SessionDead",
            ErrorKind::NotFound => "NotFound",
            ErrorKind::Server => "Server",
            ErrorKind::Client => "Client",
            ErrorKind::BusinessError => "BusinessError",
            ErrorKind::Unknown => "Unknown",
        }
    }

    /// 冷却截止时间戳（秒）；`None` 表示不落冷却。
    pub fn cooldown_until(&self, now: i64) -> Option<i64> {
        match self {
            // 永久失效用一个大但有限的值，避免溢出与「永不过期」的判断歧义
            ErrorKind::SessionDead => Some(9_999_999_999),
            ErrorKind::PlanLimit => Some(now + 43_200),
            ErrorKind::SoftRate => Some(now + 60),
            ErrorKind::NotFound => Some(now + 60),
            ErrorKind::Server | ErrorKind::Client => Some(now + 600),
            ErrorKind::BusinessError => Some(now + 300),
            ErrorKind::Unknown => None,
        }
    }
}

/// 按 HTTP 状态与业务码分类失败。
pub fn classify_error(http_status: i32, code: Option<i64>) -> ErrorKind {
    if http_status == 200 {
        return match code {
            Some(1005) => ErrorKind::PlanLimit,
            Some(c) if c != 0 => ErrorKind::BusinessError,
            _ => ErrorKind::Unknown,
        };
    }
    match http_status {
        0 => ErrorKind::Unknown,
        401 => ErrorKind::SessionDead,
        404 => ErrorKind::NotFound,
        429 => ErrorKind::SoftRate,
        s if (500..600).contains(&s) => ErrorKind::Server,
        s if (400..500).contains(&s) => ErrorKind::Client,
        _ => ErrorKind::Unknown,
    }
}

/// 单个账号的签到结果。
#[derive(Debug, Clone)]
pub struct AccountOutcome {
    pub user_id: String,
    pub name: String,
    /// `success` / `already` / `fail` / `skip`
    pub status: &'static str,
    pub code: Option<i64>,
    pub message: String,
    pub credits: Option<i64>,
    pub delta: Option<i64>,
    pub error_type: Option<String>,
    pub cooldown_until: Option<i64>,
}

/// 单轮签到汇总。
#[derive(Debug, Clone, Default)]
pub struct RoundSummary {
    pub outcomes: Vec<AccountOutcome>,
    pub ok: usize,
    pub already: usize,
    pub failed: usize,
    pub skipped: usize,
}

impl RoundSummary {
    pub fn to_json(&self) -> Value {
        json!({
            "ok": self.ok,
            "already": self.already,
            "failed": self.failed,
            "skipped": self.skipped,
            "total": self.outcomes.len(),
            "results": self.outcomes.iter().map(|o| json!({
                "userId": o.user_id,
                "name": o.name,
                "status": o.status,
                "code": o.code,
                "message": o.message,
                "credits": o.credits,
                "delta": o.delta,
                "errorType": o.error_type,
                "cooldownUntil": o.cooldown_until,
            })).collect::<Vec<_>>(),
        })
    }
}

/// 构造签到/状态接口的请求头。
///
/// 不发送 `accept-encoding`：避免收到无法解压的压缩响应（与上游约定一致）。
fn build_headers(jwt: &str, dev: &DeviceEntry) -> HashMap<String, String> {
    let mut h = HashMap::new();
    let auth = if jwt.starts_with("Cloud-IDE-JWT ") {
        jwt.to_string()
    } else {
        format!("Cloud-IDE-JWT {}", jwt.trim())
    };
    for (k, v) in [
        ("accept", "*/*"),
        ("accept-language", "zh-CN"),
        ("content-type", "application/json"),
        ("user-agent", "VSCode 1.107.1 (TRAE SOLO CN)"),
        ("x-market-client-id", "VSCode 1.107.1"),
        ("x-user-region", "CN"),
        ("x-lgw-req-sdk-type", "3"),
        ("package-type", "stable_cn"),
        ("x-lscbd-aid", "787976"),
        ("x-lscbd-platform", "windows"),
        ("app-version", "0.1.45"),
        ("sec-fetch-dest", "empty"),
        ("sec-fetch-mode", "no-cors"),
        ("sec-fetch-site", "none"),
    ] {
        h.insert(k.to_string(), v.to_string());
    }
    h.insert("authorization".to_string(), auth);
    h.insert("x-market-user-id".to_string(), dev.market_user_id.clone());
    h.insert("x-device-id".to_string(), dev.device_id.clone());
    h.insert("vscode-sessionid".to_string(), dev.session_id.clone());
    h.insert("x-request-id".to_string(), random_hex(32));
    h.insert("x-tt-trace-id".to_string(), format!("00-{}-01", random_hex(16)));
    h
}

/// 随机十六进制串（`len` 位）。
pub fn random_hex(len: usize) -> String {
    uuid::Uuid::new_v4().simple().to_string().chars().take(len).collect()
}

/// POST 到 Trae 接口，返回 `(http_status, parsed_body, raw_text)`。
///
/// `http_status == 0` 表示网络异常（请求根本没发出去或超时）。
async fn http_post(jwt: &str, dev: &DeviceEntry, url: &str) -> (i32, Option<Value>, String) {
    let headers = build_headers(jwt, dev);
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
        // 直连语义：签到请求不应被本地 MITM 代理劫持（代理未启动时会导致
        // 整批账号签到失败），与上游「ureq 不读系统代理」的约定对齐。
        .no_proxy()
        .build()
    {
        Ok(c) => c,
        Err(e) => return (0, None, format!("HTTP 客户端创建失败: {e}")),
    };
    let mut req = client.post(url).body("{}");
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16() as i32;
            let raw = resp.text().await.unwrap_or_default();
            let parsed = serde_json::from_str(&raw).ok();
            (status, parsed, raw)
        }
        Err(e) => (0, None, e.to_string()),
    }
}

/// 逐层剥开包裹键，收集所有层级（外层在前）。
fn unwrap_scopes(value: &Value) -> Vec<&Value> {
    let mut out = vec![value];
    let mut cur = value;
    for _ in 0..MAX_DEPTH {
        let Some(next) = ENVELOPE_KEYS
            .iter()
            .find_map(|k| cur.get(*k).filter(|v| v.is_object()))
        else {
            break;
        };
        out.push(next);
        cur = next;
    }
    out
}

/// 从外层到内层找第一个存在的字段。
fn find_payload_field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    unwrap_scopes(value)
        .into_iter()
        .find_map(|scope| scope.get(key))
}

/// 宽容地取整数：接受整数、整数值浮点、数字字符串；拒绝布尔、小数、非数字。
fn as_int_tolerant(value: &Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        return Some(n);
    }
    if let Some(f) = value.as_f64() {
        // 只接受整数值浮点，避免把 1.5 个积分当成 1
        return (f.fract() == 0.0).then_some(f as i64);
    }
    value.as_str().and_then(|s| s.trim().parse::<i64>().ok())
}

/// status 预检结果。
struct StatusOutcome {
    ok: bool,
    checked_in: Option<bool>,
    credits: Option<i64>,
    message: String,
}

/// 预检今日是否已签。
async fn status_check(jwt: &str, dev: &DeviceEntry) -> StatusOutcome {
    let none = |message: &str| StatusOutcome {
        ok: false,
        checked_in: None,
        credits: None,
        message: message.to_string(),
    };
    let (status, body, raw) = http_post(jwt, dev, STATUS_URL).await;
    if status == 0 {
        return none(if raw.is_empty() { "网络异常" } else { &raw });
    }
    let Some(data) = body.filter(|b| b.is_object()) else {
        let head: String = raw.chars().take(200).collect();
        return StatusOutcome {
            ok: false,
            checked_in: None,
            credits: None,
            message: format!("非 JSON 响应: {head}"),
        };
    };
    // code 只取**顶层**：向下挖会把嵌套对象里的无关业务码当成响应码
    let code = data.get("code").and_then(Value::as_i64);
    if code.is_some_and(|c| c != 0) {
        let message = find_payload_field(&data, "message")
            .and_then(Value::as_str)
            .unwrap_or("接口返回错误")
            .to_string();
        return StatusOutcome {
            ok: false,
            checked_in: None,
            credits: None,
            message: format!("[{code:?}] {message}"),
        };
    }
    let checked_in = find_payload_field(&data, "checked_in")
        .or_else(|| find_payload_field(&data, "checkedIn"))
        .and_then(Value::as_bool);
    let credits = find_payload_field(&data, "credits").and_then(as_int_tolerant);
    StatusOutcome {
        ok: true,
        checked_in,
        credits,
        message: "查询成功".to_string(),
    }
}

/// 从 claim 响应解析本次奖励积分。
///
/// 只接受**正数**：零值占位字段（很多响应会带 `reward: 0`）会遮蔽后面真正的字段，
/// 若允许 0 通过就会把「签到得了 50 分」记成 0 分。
fn parse_claim_reward(data: &Value) -> Option<i64> {
    let scopes = unwrap_scopes(data);
    // 内层优先：越深越接近真实业务数据
    for scope in scopes.iter().rev() {
        for key in CLAIM_REWARD_KEYS {
            if let Some(v) = scope.get(key).and_then(as_int_tolerant) {
                if v > 0 {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// 执行一次 claim（不重试）。
async fn signin_request(jwt: &str, dev: &DeviceEntry) -> (i32, Option<Value>, String) {
    http_post(jwt, dev, SIGNIN_URL).await
}

/// 带重试的 claim：**只**在网络异常时重试。
async fn signin_with_retry(
    jwt: &str,
    dev: &DeviceEntry,
    retry: u32,
) -> (i32, Option<Value>, String) {
    let mut attempt = 0u32;
    loop {
        let (status, body, raw) = signin_request(jwt, dev).await;
        // 业务失败不重试：服务端已经明确拒绝，再打一次只会再失败一次并加重风控
        if status != 0 || attempt >= retry {
            return (status, body, raw);
        }
        attempt += 1;
        tokio::time::sleep(std::time::Duration::from_secs(RETRY_GAP_SECS)).await;
    }
}

/// 积分归属三层兜底。
async fn resolve_claim_credits(
    claim_body: Option<&Value>,
    jwt: &str,
    dev: &DeviceEntry,
    credits_before: Option<i64>,
) -> (Option<i64>, Option<i64>) {
    // 第一层：claim 响应自身的奖励字段（接口返回为准）
    if let Some(reward) = claim_body.and_then(parse_claim_reward) {
        return (Some(reward), credits_before);
    }
    // 第二层：复查余额差值
    let after = status_check(jwt, dev).await;
    if after.ok {
        if let (Some(before), Some(after_credits)) = (credits_before, after.credits) {
            let delta = after_credits - before;
            if delta >= 0 {
                return (Some(delta), Some(after_credits));
            }
        }
        return (credits_before, after.credits.or(credits_before));
    }
    // 第三层：旧行为（无法确定增量，退化为把余额当增量展示）
    (credits_before, credits_before)
}

/// 对单个账号执行一轮签到。
pub async fn checkin_account(
    jwt: &str,
    uid: &str,
    name: &str,
    retry: u32,
) -> AccountOutcome {
    let mut outcome = AccountOutcome {
        user_id: uid.to_string(),
        name: name.to_string(),
        status: "fail",
        code: None,
        message: String::new(),
        credits: None,
        delta: None,
        error_type: None,
        cooldown_until: None,
    };

    if jwt.trim().is_empty() {
        outcome.message = "未配置 JWT".to_string();
        outcome.error_type = Some(ErrorKind::SessionDead.as_str().to_string());
        return outcome;
    }

    let info: JwtInfo = trae_account::parse_jwt(jwt);
    if let Some(hours) = info.exp_hours {
        if hours < EXPIRY_WARN_HOURS {
            outcome.message = format!("JWT 将在 {hours:.1} 小时后过期，请及时重新登录；");
        }
    }
    let dev = trae_device::resolve_device(uid);

    // 1. status 预检
    let pre = status_check(jwt, &dev).await;
    if pre.ok && pre.checked_in == Some(true) {
        outcome.status = "already";
        outcome.credits = pre.credits;
        outcome.delta = Some(0);
        outcome.message = format!("{}今日已签到", outcome.message);
        return outcome;
    }
    let credits_before = pre.credits;

    // 2. claim
    let (status, body, raw) = signin_with_retry(jwt, &dev, retry).await;
    let code = body.as_ref().and_then(|b| b.get("code")).and_then(Value::as_i64);
    outcome.code = code;

    if status == 0 {
        // 网络异常：不落冷却（否则一次代理抖动就让整批账号被冷却）
        outcome.message = format!("{}网络异常: {raw}", outcome.message);
        outcome.error_type = Some(ErrorKind::Unknown.as_str().to_string());
        return outcome;
    }
    let business_ok = code.is_none_or(|c| c == 0);
    if status == 200 && business_ok {
        let (delta, credits) = resolve_claim_credits(body.as_ref(), jwt, &dev, credits_before).await;
        outcome.status = "success";
        outcome.delta = delta;
        outcome.credits = credits;
        let reward_text = delta.map(|d| format!("获得 {d} 积分")).unwrap_or_default();
        outcome.message = format!("{}签到成功 {reward_text}", outcome.message);
        clear_cooldown(uid);
        record_credits_history(uid, credits, delta);
        return outcome;
    }

    // 3. 失败分类
    let kind = classify_error(status, code);
    let message = body
        .as_ref()
        .and_then(|b| find_payload_field(b, "message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| raw.chars().take(200).collect());
    outcome.message = format!("{}{}", outcome.message, message);
    outcome.error_type = Some(kind.as_str().to_string());
    let now = config::now_secs();
    if let Some(until) = kind.cooldown_until(now) {
        save_cooldown(uid, kind.as_str(), until, &message);
        outcome.cooldown_until = Some(until);
    }
    outcome
}

/// 跑一轮签到（对给定账号列表逐个执行）。
pub async fn run_round(accounts: &[Value], retry: u32) -> RoundSummary {
    let mut summary = RoundSummary::default();
    for acc in accounts {
        let uid = acc.get("user_id").and_then(Value::as_str).unwrap_or("");
        let name = trae_account::display_name(acc);
        let jwt = acc.get("jwt").and_then(Value::as_str).unwrap_or("");

        // 冷却中的账号跳过（避免对已判定失效/限流的账号持续打请求）
        if let Some(until) = cooldown_until(uid) {
            if until > config::now_secs() {
                summary.skipped += 1;
                summary.outcomes.push(AccountOutcome {
                    user_id: uid.to_string(),
                    name,
                    status: "skip",
                    code: None,
                    message: "冷却中，本轮跳过".to_string(),
                    credits: None,
                    delta: None,
                    error_type: None,
                    cooldown_until: Some(until),
                });
                continue;
            }
        }

        let outcome = checkin_account(jwt, uid, &name, retry).await;
        match outcome.status {
            "success" => summary.ok += 1,
            "already" => summary.already += 1,
            _ => summary.failed += 1,
        }
        summary.outcomes.push(outcome);
        // 账号间隔，避免触发风控
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    }
    summary
}

// ---------------------------------------------------------------------------
// 冷却表（`<store_dir>/trae_cooldowns.json`）
// ---------------------------------------------------------------------------

fn cooldowns_file() -> std::path::PathBuf {
    config::store_dir().join("trae_cooldowns.json")
}

fn load_cooldowns() -> serde_json::Map<String, Value> {
    std::fs::read_to_string(cooldowns_file())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn save_cooldowns(map: &serde_json::Map<String, Value>) {
    let path = cooldowns_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let content = serde_json::to_string_pretty(&Value::Object(map.clone())).unwrap_or_default();
    let _ = config::atomic_write(&path, &content);
}

/// 账号当前冷却截止时间（无冷却返回 `None`）。
pub fn cooldown_until(uid: &str) -> Option<i64> {
    load_cooldowns()
        .get(uid)?
        .get("until")
        .and_then(Value::as_i64)
}

/// 记录冷却。
pub fn save_cooldown(uid: &str, kind: &str, until: i64, reason: &str) {
    let mut map = load_cooldowns();
    let error_count = map
        .get(uid)
        .and_then(|v| v.get("error_count"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
        + 1;
    map.insert(
        uid.to_string(),
        json!({
            "type": kind,
            "until": until,
            "reason": reason,
            "error_count": error_count,
        }),
    );
    save_cooldowns(&map);
}

/// 清除冷却（签到成功后调用）。
pub fn clear_cooldown(uid: &str) {
    let mut map = load_cooldowns();
    if map.remove(uid).is_some() {
        save_cooldowns(&map);
    }
}

/// 全部冷却记录（供界面展示）。
pub fn all_cooldowns() -> Value {
    Value::Object(load_cooldowns())
}

// ---------------------------------------------------------------------------
// 积分历史（`<store_dir>/trae_credits_history.json`，90 天滚动）
// ---------------------------------------------------------------------------

const CREDITS_HISTORY_KEEP_DAYS: i64 = 90;

fn credits_history_file() -> std::path::PathBuf {
    config::store_dir().join("trae_credits_history.json")
}

/// 记录一次签到后的积分快照。
fn record_credits_history(uid: &str, credits: Option<i64>, delta: Option<i64>) {
    let Some(credits) = credits else { return };
    let date = config::utc_iso().chars().take(10).collect::<String>();
    let mut records: Vec<Value> = std::fs::read_to_string(credits_history_file())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    records.push(json!({
        "date": date,
        "userId": uid,
        "credits": credits,
        "delta": delta.unwrap_or(0),
    }));
    // 90 天滚动裁剪
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(CREDITS_HISTORY_KEEP_DAYS))
        .format("%Y-%m-%d")
        .to_string();
    records.retain(|r| {
        r.get("date")
            .and_then(Value::as_str)
            .map(|d| d >= cutoff.as_str())
            .unwrap_or(false)
    });
    let path = credits_history_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let content = serde_json::to_string_pretty(&Value::Array(records)).unwrap_or_default();
    let _ = config::atomic_write(&path, &content);
}

/// 读取积分历史（供趋势图）。
pub fn load_credits_history() -> Vec<Value> {
    std::fs::read_to_string(credits_history_file())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 错误分类覆盖全部状态码() {
        assert_eq!(classify_error(200, Some(1005)), ErrorKind::PlanLimit);
        assert_eq!(classify_error(200, Some(0)), ErrorKind::Unknown);
        assert_eq!(classify_error(200, Some(7)), ErrorKind::BusinessError);
        assert_eq!(classify_error(200, None), ErrorKind::Unknown);
        assert_eq!(classify_error(401, None), ErrorKind::SessionDead);
        assert_eq!(classify_error(404, None), ErrorKind::NotFound);
        assert_eq!(classify_error(429, None), ErrorKind::SoftRate);
        assert_eq!(classify_error(500, None), ErrorKind::Server);
        assert_eq!(classify_error(503, None), ErrorKind::Server);
        assert_eq!(classify_error(400, None), ErrorKind::Client);
        assert_eq!(classify_error(403, None), ErrorKind::Client);
        assert_eq!(classify_error(0, None), ErrorKind::Unknown);
    }

    #[test]
    fn 冷却时长符合上游约定() {
        let now = 1_000_000i64;
        assert_eq!(ErrorKind::SessionDead.cooldown_until(now), Some(9_999_999_999));
        assert_eq!(ErrorKind::PlanLimit.cooldown_until(now), Some(now + 43_200));
        assert_eq!(ErrorKind::SoftRate.cooldown_until(now), Some(now + 60));
        assert_eq!(ErrorKind::NotFound.cooldown_until(now), Some(now + 60));
        assert_eq!(ErrorKind::Server.cooldown_until(now), Some(now + 600));
        assert_eq!(ErrorKind::BusinessError.cooldown_until(now), Some(now + 300));
        assert_eq!(
            ErrorKind::Unknown.cooldown_until(now),
            None,
            "网络异常不落冷却，否则一次抖动会冷却整批账号"
        );
    }

    #[test]
    fn 包裹层剥离与字段查找() {
        let v = json!({"data": {"result": {"checked_in": true, "credits": 100}}});
        assert_eq!(
            find_payload_field(&v, "checked_in").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(find_payload_field(&v, "credits").and_then(as_int_tolerant), Some(100));
        // 顶层直接命中
        assert_eq!(find_payload_field(&v, "data").is_some(), true);
        // 不存在
        assert!(find_payload_field(&v, "nope").is_none());
    }

    #[test]
    fn 宽容整数解析拒绝小数与布尔() {
        assert_eq!(as_int_tolerant(&json!(42)), Some(42));
        assert_eq!(as_int_tolerant(&json!(42.0)), Some(42));
        assert_eq!(as_int_tolerant(&json!("42")), Some(42));
        assert_eq!(as_int_tolerant(&json!(" 42 ")), Some(42));
        // 小数不是合法积分
        assert_eq!(as_int_tolerant(&json!(42.5)), None);
        // 布尔不得被当成 1/0
        assert_eq!(as_int_tolerant(&json!(true)), None);
        assert_eq!(as_int_tolerant(&json!("abc")), None);
        assert_eq!(as_int_tolerant(&json!(null)), None);
    }

    #[test]
    fn 奖励解析跳过零值占位字段() {
        // reward: 0 是占位，真实值在后面的 claim_credits
        let v = json!({"data": {"reward": 0, "claim_credits": 50}});
        assert_eq!(
            parse_claim_reward(&v),
            Some(50),
            "零值占位不得遮蔽真实奖励字段"
        );
        // 只有零值 → 无奖励可解析
        assert_eq!(parse_claim_reward(&json!({"data": {"reward": 0}})), None);
        // 内层优先于外层
        let nested = json!({"credits": 10, "data": {"reward": 99}});
        assert_eq!(parse_claim_reward(&nested), Some(99));
    }

    #[test]
    fn 冷却表读写与清除() {
        use crate::modules::config::test_isolation::Isolated;
        let _iso = Isolated::new("trae-cool");

        let uid = "u-cool-1";
        assert_eq!(cooldown_until(uid), None);
        save_cooldown(uid, "SoftRate", 12345, "限流");
        assert_eq!(cooldown_until(uid), Some(12345));
        // 累计错误计数
        save_cooldown(uid, "SoftRate", 23456, "限流");
        assert_eq!(cooldown_until(uid), Some(23456));
        let map = load_cooldowns();
        assert_eq!(map[uid]["error_count"], 2);
        clear_cooldown(uid);
        assert_eq!(cooldown_until(uid), None);
    }

    #[test]
    fn 汇总计数与_json_形态() {
        let mut s = RoundSummary::default();
        s.outcomes.push(AccountOutcome {
            user_id: "a".into(),
            name: "A".into(),
            status: "success",
            code: Some(0),
            message: "ok".into(),
            credits: Some(100),
            delta: Some(50),
            error_type: None,
            cooldown_until: None,
        });
        s.ok = 1;
        s.outcomes.push(AccountOutcome {
            user_id: "b".into(),
            name: "B".into(),
            status: "already",
            code: None,
            message: "已签".into(),
            credits: None,
            delta: None,
            error_type: None,
            cooldown_until: None,
        });
        s.already = 1;
        let j = s.to_json();
        assert_eq!(j["ok"], 1);
        assert_eq!(j["already"], 1);
        assert_eq!(j["total"], 2);
        assert_eq!(j["results"][0]["userId"], "a");
        assert_eq!(j["results"][0]["delta"], 50);
    }

    #[test]
    fn 空_jwt_直接判失败且标记会话失效() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = rt.block_on(checkin_account("", "u1", "测试", 0));
        assert_eq!(outcome.status, "fail");
        assert_eq!(outcome.message, "未配置 JWT");
        assert_eq!(outcome.error_type.as_deref(), Some("SessionDead"));
    }

    #[test]
    fn 随机十六进制长度正确() {
        assert_eq!(random_hex(32).len(), 32);
        assert_eq!(random_hex(16).len(), 16);
        assert!(random_hex(32).chars().all(|c| c.is_ascii_hexdigit()));
    }
}
