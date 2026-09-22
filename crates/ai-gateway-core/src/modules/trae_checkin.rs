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
    /// 按当前 `outcomes` 的最终状态重算四个计数。
    ///
    /// 重试轮会用新结果覆盖旧结果，覆盖后必须重算 —— 否则计数会与逐条结果
    /// 对不上（界面上出现「失败 3 / 成功 2」但列表里只有 4 条）。
    pub fn recount(&mut self) {        self.ok = 0;
        self.already = 0;
        self.failed = 0;
        self.skipped = 0;
        for o in &self.outcomes {
            match o.status {
                "success" => self.ok += 1,
                "already" => self.already += 1,
                "skip" => self.skipped += 1,
                _ => self.failed += 1,
            }
        }
    }

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

/// JWT 探活结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JwtLiveness {
    /// 服务端明确接受该 JWT
    Alive,
    /// 服务端明确拒绝（401 / 业务码 1001）—— 需要重新登录
    Dead(String),
    /// 探不出来（网络异常 / 代理未启动 / 响应无法解析）
    ///
    /// **必须与 `Dead` 区分**：把网络问题当成失效会把一批好账号标成需要重登，
    /// 用户按提示去重登却发现账号本来是好的。
    Unknown(String),
}

impl JwtLiveness {
    pub fn is_dead(&self) -> bool {
        matches!(self, JwtLiveness::Dead(_))
    }

    pub fn message(&self) -> &str {
        match self {
            JwtLiveness::Alive => "JWT 有效",
            JwtLiveness::Dead(m) | JwtLiveness::Unknown(m) => m,
        }
    }
}

/// 服务端「JWT 已失效」的业务码（抓包固化）。
const CODE_SESSION_DEAD: i64 = 1001;

/// 探一次 JWT 是否还能用（切换账号前的预检）。
///
/// 复用签到 status 接口：它是唯一一个「无副作用且必然校验登录态」的端点，
/// 单独为此发明一个探测请求既没必要也更容易被风控注意到。
///
/// **失败放行**（返回 `Unknown` 而不是 `Dead`）：调用方据此只给警告、不阻断切换。
/// 硬阻断会锁死「需要重新登录才能修好的账号」—— 而重新登录的入口恰恰就在切换
/// 流程里，形成死锁。
pub async fn probe_jwt_alive(jwt: &str, dev: &DeviceEntry) -> JwtLiveness {
    let jwt = jwt.trim();
    if jwt.is_empty() {
        return JwtLiveness::Dead("JWT 为空".to_string());
    }
    let (status, body, raw) = http_post(jwt, dev, STATUS_URL).await;
    if status == 0 {
        return JwtLiveness::Unknown(if raw.is_empty() {
            "探活失败：网络异常".to_string()
        } else {
            format!("探活失败：{raw}")
        });
    }
    if status == 401 {
        return JwtLiveness::Dead("JWT 已失效（HTTP 401），需要重新登录".to_string());
    }
    let Some(data) = body.filter(|b| b.is_object()) else {
        let head: String = raw.chars().take(200).collect();
        return JwtLiveness::Unknown(format!("探活失败：非 JSON 响应 {head}"));
    };
    match data.get("code").and_then(Value::as_i64) {
        Some(CODE_SESSION_DEAD) => {
            let detail = find_payload_field(&data, "message")
                .and_then(Value::as_str)
                .unwrap_or("登录态失效");
            JwtLiveness::Dead(format!("JWT 已失效（业务码 {CODE_SESSION_DEAD}：{detail}）"))
        }
        // 其他非 0 业务码（如套餐用尽 1005）说明**登录态本身有效**，只是业务受限
        Some(c) if c != 0 => JwtLiveness::Alive,
        _ => JwtLiveness::Alive,
    }
}

/// 从 claim 响应解析本次奖励积分。
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
    // 预检失败不阻断 claim（预检只是优化，claim 才是权威），但要把原因带上 ——
    // 否则「签到失败」时用户看不到是预检网络异常还是真的被拒。
    if !pre.ok {
        outcome.message = format!("{}预检未通过（{}）；", outcome.message, pre.message);
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
    outcome.cooldown_until = register_failure(uid, &kind, now, &message);
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

/// 需要「连续失败若干次」才真正落冷却的失败类型（瞬时故障）。
///
/// 为什么这两类要特殊对待：5xx 与普通 4xx 通常是服务端抖动或网关瞬时拒绝，
/// 单次失败就给账号坐 10 分钟冷板凳，会让整批签到里偶发的一次抖动直接吞掉该账号
/// 当天的签到机会（而且用户看到的是「冷却中」，看不出只是抖动）。上游的实测结论
/// 是连续 3 次才算真故障。
pub const TRANSIENT_TRIP_KINDS: [&str; 2] = ["Server", "Client"];

/// 瞬时故障连续几次才落冷却。
pub const TRANSIENT_FAILS_TO_TRIP: i64 = 3;

/// 只计数、不落冷却时的 `until` 占位值。
///
/// 用 0 而不是「不写记录」：`error_count` 需要跨轮次保留才能数到 3。
/// 同时 [`cooldown_until`] 会把 `<= 0` 视作「无冷却」，
/// 于是 `run_round` / 抓包代理都不会误跳过一个只是在计数的账号。
const COUNT_ONLY_UNTIL: i64 = 0;

/// 账号当前冷却截止时间（无冷却返回 `None`）。
///
/// `until <= 0` 是「只计数未落冷却」的占位记录，必须返回 `None`，
/// 否则调用方（`run_round`、抓包代理）会把账号当成冷却中而跳过。
pub fn cooldown_until(uid: &str) -> Option<i64> {
    let until = load_cooldowns()
        .get(uid)?
        .get("until")
        .and_then(Value::as_i64)?;
    if until <= COUNT_ONLY_UNTIL {
        None
    } else {
        Some(until)
    }
}

/// 当前连续失败次数（供界面展示与判断是否即将落冷却）。
pub fn error_count(uid: &str) -> i64 {
    load_cooldowns()
        .get(uid)
        .and_then(|v| v.get("error_count"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

/// 写一条冷却/计数记录。
fn write_cooldown(uid: &str, kind: &str, until: i64, reason: &str, error_count: i64) {
    let mut map = load_cooldowns();
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

/// 记录一次失败，并决定是否真的落冷却。
///
/// 返回**生效**的冷却截止时间；`None` 表示「只计数、不落冷却」
/// （瞬时故障尚未达到 [`TRANSIENT_FAILS_TO_TRIP`]）。
pub fn register_failure(uid: &str, kind: &ErrorKind, now: i64, reason: &str) -> Option<i64> {
    let planned = kind.cooldown_until(now)?;
    let kind_str = kind.as_str();

    if !TRANSIENT_TRIP_KINDS.contains(&kind_str) {
        // 非瞬时故障（会话失效 / 套餐限制 / 限流 / 接口不存在 / 业务错误）：
        // 语义明确，立刻落冷却。保留累计次数供排查。
        let next = error_count(uid) + 1;
        write_cooldown(uid, kind_str, planned, reason, next);
        return Some(planned);
    }

    let count = error_count(uid) + 1;
    if count < TRANSIENT_FAILS_TO_TRIP {
        // 只计数：until 占位 0，cooldown_until 会返回 None，下一个账号继续跑
        write_cooldown(uid, kind_str, COUNT_ONLY_UNTIL, reason, count);
        return None;
    }
    // 连续到阈值：落冷却并把计数清零，避免恢复后一次抖动又立刻冷板凳
    write_cooldown(uid, kind_str, planned, reason, 0);
    Some(planned)
}

/// 记录冷却（**低层写入**，不参与「连续失败计数」判定）。
///
/// 业务判定请用 [`register_failure`]；此函数保留给需要无条件写入冷板凳的场景
/// （以及既有测试），它会无条件累加 `error_count`。
pub fn save_cooldown(uid: &str, kind: &str, until: i64, reason: &str) {
    let next = error_count(uid) + 1;
    write_cooldown(uid, kind, until, reason, next);
}

/// 清除冷却（签到成功后调用）。返回是否真的移除了记录。
///
/// 返回值让调用方能在「确实清掉了一条」时才打日志；同时**无条件移除**整条记录，
/// 避免只计数（`until == 0`）的残留记录被漏清 —— 那种记录虽然不阻塞签到，
/// 但会让下一次瞬时失败直接数到阈值。
pub fn clear_cooldown(uid: &str) -> bool {
    let mut map = load_cooldowns();
    if map.remove(uid).is_some() {
        save_cooldowns(&map);
        true
    } else {
        false
    }
}

/// 清除**全部**账号的冷却记录，返回清掉的条数。
///
/// 界面上的「全部清除冷却」用于一键恢复：比如换网络环境后整批账号都被限流，
/// 逐条点太慢。返回条数让界面能如实说「已清除 N 条」而不是笼统的「操作成功」。
pub fn clear_all_cooldowns() -> usize {
    let count = load_cooldowns().len();
    if count > 0 {
        save_cooldowns(&serde_json::Map::new());
    }
    count
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

    /// G5 核心回归：单次 5xx 不得让账号坐 10 分钟冷板凳。
    #[test]
    fn 瞬时故障连续三次才落冷却() {
        use crate::modules::config::test_isolation::Isolated;
        let _iso = Isolated::new("trae-trip");
        let uid = "u-trip";
        let now = 1_700_000_000;

        // 第 1、2 次：只计数，不落冷却
        for expected in 1..TRANSIENT_FAILS_TO_TRIP {
            let until = register_failure(uid, &ErrorKind::Server, now, "500");
            assert_eq!(until, None, "第 {expected} 次瞬时失败不应落冷却");
            assert_eq!(error_count(uid), expected);
            assert_eq!(
                cooldown_until(uid),
                None,
                "只计数状态不得被当成冷却中，否则 run_round 会跳过该账号"
            );
        }

        // 第 3 次：落冷却并清零计数
        let until = register_failure(uid, &ErrorKind::Server, now, "500").unwrap();
        assert_eq!(until, now + 600);
        assert_eq!(cooldown_until(uid), Some(now + 600));
        assert_eq!(
            error_count(uid),
            0,
            "落冷却后计数清零，避免恢复后一次抖动立刻再次冷板凳"
        );
    }

    #[test]
    fn 瞬时故障中途成功会重新计数() {
        use crate::modules::config::test_isolation::Isolated;
        let _iso = Isolated::new("trae-trip-reset");
        let uid = "u-reset";
        let now = 1_700_000_000;

        register_failure(uid, &ErrorKind::Server, now, "500");
        register_failure(uid, &ErrorKind::Client, now, "400");
        assert_eq!(error_count(uid), 2);
        // 签到成功 → 清记录
        assert!(clear_cooldown(uid));
        assert_eq!(error_count(uid), 0);
        // 再次失败从 1 重新数，不会一次就落冷却
        assert_eq!(register_failure(uid, &ErrorKind::Server, now, "500"), None);
        assert_eq!(error_count(uid), 1);
    }

    /// 非瞬时故障语义不变：一次就落冷却。
    #[test]
    fn 非瞬时故障一次即落冷却() {
        use crate::modules::config::test_isolation::Isolated;
        let _iso = Isolated::new("trae-trip-direct");
        let now = 1_700_000_000;

        for (uid, kind, want) in [
            ("u-dead", ErrorKind::SessionDead, 9_999_999_999),
            ("u-plan", ErrorKind::PlanLimit, now + 43_200),
            ("u-rate", ErrorKind::SoftRate, now + 60),
            ("u-404", ErrorKind::NotFound, now + 60),
            ("u-biz", ErrorKind::BusinessError, now + 300),
        ] {
            assert_eq!(
                register_failure(uid, &kind, now, "x"),
                Some(want),
                "{uid} 应一次即落冷却"
            );
            assert_eq!(cooldown_until(uid), Some(want));
        }

        // Unknown 不落冷却
        assert_eq!(register_failure("u-unk", &ErrorKind::Unknown, now, "x"), None);
        assert_eq!(cooldown_until("u-unk"), None);
    }

    #[test]
    fn 冷却中的账号在计数状态下不被跳过() {
        use crate::modules::config::test_isolation::Isolated;
        let _iso = Isolated::new("trae-skip");
        let uid = "u-skip";
        let now = config::now_secs();

        // 计数状态：cooldown_until 返回 None → run_round 不会跳过
        register_failure(uid, &ErrorKind::Server, now, "500");
        assert_eq!(cooldown_until(uid), None);

        // 真冷却：返回 Some 且在未来 → run_round 会跳过
        register_failure(uid, &ErrorKind::Server, now, "500");
        register_failure(uid, &ErrorKind::Server, now, "500");
        let until = cooldown_until(uid).expect("第三次应落冷却");
        assert!(until > now);
    }

    /// 网络异常必须归到 `Unknown`（放行）而不是 `Dead`（判失效）：
    /// 代理没启动时把整批账号标成「需要重登」，用户按提示重登会发现账号本来是好的。
    #[test]
    fn 探活分类区分失效与探不出来() {
        let dead = JwtLiveness::Dead("401".into());
        let unknown = JwtLiveness::Unknown("网络异常".into());
        assert!(dead.is_dead());
        assert!(!unknown.is_dead(), "探不出来不得当作失效");
        assert_eq!(JwtLiveness::Alive.message(), "JWT 有效");
        assert!(!JwtLiveness::Alive.is_dead());
    }

    /// 空 JWT 无需发请求即可判定失效。
    #[tokio::test]
    async fn 空_jwt_探活直接判失效() {
        let dev = trae_device::derive_device("u-probe");
        let r = probe_jwt_alive("   ", &dev).await;
        assert!(r.is_dead());
        assert!(r.message().contains("为空"), "{}", r.message());
    }

    #[test]
    fn 清除计数残留记录() {        use crate::modules::config::test_isolation::Isolated;
        let _iso = Isolated::new("trae-clear-count");
        let uid = "u-count";
        register_failure(uid, &ErrorKind::Server, config::now_secs(), "500");
        assert_eq!(error_count(uid), 1);
        assert!(
            clear_cooldown(uid),
            "只计数记录也必须能被清掉，否则会累积到阈值"
        );
        assert_eq!(error_count(uid), 0);
        assert!(!clear_cooldown(uid), "无记录时返回 false");
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

    /// 造一条指定状态的结果（只为测计数，其余字段给占位值）。
    fn outcome(uid: &str, status: &'static str) -> AccountOutcome {
        AccountOutcome {
            user_id: uid.to_string(),
            name: uid.to_string(),
            status,
            code: None,
            message: String::new(),
            credits: None,
            delta: None,
            error_type: None,
            cooldown_until: None,
        }
    }

    #[test]
    fn 重算计数与逐条结果一致() {
        let mut summary = RoundSummary {
            outcomes: vec![
                outcome("a", "success"),
                outcome("b", "already"),
                outcome("c", "fail"),
                outcome("d", "skip"),
                outcome("e", "fail"),
            ],
            // 故意给一组错计数：recount 必须把它们纠正过来
            ok: 99,
            already: 99,
            failed: 99,
            skipped: 99,
        };
        summary.recount();
        assert_eq!(summary.ok, 1);
        assert_eq!(summary.already, 1);
        assert_eq!(summary.failed, 2);
        assert_eq!(summary.skipped, 1);
        // 总数必须等于逐条结果数，否则界面会出现「失败 3 但只有 4 条」
        assert_eq!(summary.ok + summary.already + summary.failed + summary.skipped, 5);
    }

    #[test]
    fn 重试覆盖后计数不会重复累加() {
        // 场景：首轮 a 失败、b 成功；重试轮 a 成功。
        // 覆盖式替换后必须是「成功 2 / 失败 0」，而不是「成功 1 / 失败 1」。
        let mut summary = RoundSummary {
            outcomes: vec![outcome("a", "fail"), outcome("b", "success")],
            ok: 1,
            already: 0,
            failed: 1,
            skipped: 0,
        };
        let slot = summary.outcomes.iter_mut().find(|o| o.user_id == "a").unwrap();
        *slot = outcome("a", "success");
        summary.recount();
        assert_eq!(summary.ok, 2);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.to_json()["total"], json!(2));
    }

    #[test]
    fn 重算空结果全为零() {
        let mut summary = RoundSummary::default();
        summary.recount();
        assert_eq!(summary.ok + summary.already + summary.failed + summary.skipped, 0);
    }
}
