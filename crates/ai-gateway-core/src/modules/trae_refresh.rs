//! Trae 凭证续期：`ExchangeToken` 刷新 JWT + refresh_token 生命周期管理。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/commands/oauth.rs::exchange_token_refresh`
//! 与 `src-tauri/src/commands/accounts.rs::refresh_jwt_impl`，按本仓库「core 不依赖
//! Tauri」的约定改写。
//!
//! ## 为什么必须有这一层
//!
//! Trae 账号的 JWT（`Cloud-IDE-JWT`）有效期只有数十小时，而 `refresh_token` 可达
//! 90 天。没有自动续期时，账号会在签到/积分查询上集体 401，用户只能重新 OAuth 登录。
//! 上游把这条链路做成了三件事：
//!
//! 1. **惰性刷新门**（[`lazy_refresh_needed`]）：JWT 还新鲜时**不**刷新 ——
//!    `ExchangeToken` 会**轮换** `refresh_token`，工具侧每刷一次，Trae IDE 手里那份
//!    旧凭证就失效一次，双端互相踢下线。少刷 = 少冲突。
//! 2. **失败分类**（[`classify_refresh_failure`]）：只有服务端**明确拒绝**
//!    （数字 `code != 0`）才立刻判定 `refresh_token_invalid`；网络/解析类失败只计数，
//!    连续 3 次才兜底置失效。早期把两者混为一谈，一次代理抖动就把整批账号误判失效。
//! 3. **成功即解冻**：刷新成功后清冷却（含 `SessionDead` 永久冷却），否则调度会
//!    永远跳过该账号。

use serde_json::{json, Value};

use crate::modules::trae_account;

/// `ExchangeToken` 端点。
pub const EXCHANGE_URL: &str =
    "https://api.trae.com.cn/cloudide/api/v3/trae/oauth/ExchangeToken";

/// 惰性刷新阈值：JWT 剩余有效期低于该秒数才刷新。
///
/// 48h 而不是「快过期才刷」：Trae 客户端与服务端之间还有一层会话，留出足够窗口
/// 让一次刷新失败后仍有重试机会（保活每天跑一轮）。
pub const TRAE_LAZY_REFRESH_MIN_SECS: i64 = 48 * 3600;

/// 刷新冷却：失败后该账号在此时长内不再重试。
///
/// 为什么需要：`ExchangeToken` 被限流时，多个调用点（保活 / 签到前 / 手动按钮）
/// 会同时打过来，每次都失败并写一遍 vault；60s 冷却把并发压成一次。
pub const REFRESH_COOLDOWN_SECS: u64 = 60;

/// 刷新失败分类。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RefreshFailureKind {
    /// 服务端明确拒绝（`code != 0`，或换发后的 token 归属他人）→ 立即判定失效。
    Rejected,
    /// 网络/解析/协议级失败 → 只计数，连续 3 次才兜底置失效。
    Transient,
}

/// 惰性刷新门：是否需要刷新。
///
/// 语义（与上游 `lazy_refresh_needed` 逐条对齐）：
/// - JWT 剩余 > 48h **且** `refresh_token` 剩余 > 48h（或未知）→ 不刷新
/// - JWT 剩余恰好 48h → 刷新（判定是严格大于）
/// - JWT `exp` 解析不出（损坏/缺失）→ 刷新（保守）
/// - `refresh_token` 剩余 < 48h → 刷新（趁还能用来换一份新的）
/// - 无 `refresh_token` → 只要 JWT 新鲜就不提前刷
pub fn lazy_refresh_needed(jwt: &str, rt_expires_at: Option<i64>, now: i64) -> bool {
    let exp = trae_account::parse_jwt(jwt).exp_timestamp;
    let jwt_fresh = match exp {
        Some(exp) => exp - now > TRAE_LAZY_REFRESH_MIN_SECS,
        // 解析不出 exp：无法证明新鲜，保守刷新
        None => false,
    };
    let rt_expiring = rt_expires_at
        .map(|rt| rt - now < TRAE_LAZY_REFRESH_MIN_SECS)
        .unwrap_or(false);
    !jwt_fresh || rt_expiring
}

/// 把 `ExchangeToken` 的响应体分类成成功/失败，并提取新凭证。
///
/// 返回 `(access_token, refresh_token)`；`refresh_token` 可能未轮换（响应里没有）。
///
/// **为什么不能只看 `code`**：老实现 `code.unwrap_or(-1)` 会把**没有 `code` 字段**
/// 的异构响应（上游换过一版信封）当成拒绝，第一次刷新就把账号标成失效。这里改成
/// 「有数字 code 且非 0 才算拒绝」，其余情况继续往下找 token 字段。
pub fn parse_exchange_response(body: &Value) -> Result<(String, Option<String>), String> {
    if let Some(code) = body.get("code").and_then(Value::as_i64) {
        if code != 0 {
            let msg = body
                .get("msg")
                .or_else(|| body.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("(无错误信息)");
            return Err(format!("code={code} {msg}"));
        }
    }
    // 信封逐层下钻（不同版本包在 data / result / resp 里）
    let scopes = unwrap_scopes(body);
    const ACCESS_KEYS: &[&str] = &["AccessToken", "access_token", "accessToken", "token", "Jwt", "JWT"];
    const REFRESH_KEYS: &[&str] = &["RefreshToken", "refresh_token", "refreshToken"];
    let access = scopes
        .iter()
        .find_map(|s| ACCESS_KEYS.iter().find_map(|k| s.get(*k)).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "响应中未找到 access token".to_string())?
        .to_string();
    let refresh = scopes
        .iter()
        .find_map(|s| REFRESH_KEYS.iter().find_map(|k| s.get(*k)).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok((access, refresh))
}

/// 逐层剥开信封键，收集所有层级（外层在前）。
fn unwrap_scopes(value: &Value) -> Vec<&Value> {
    const ENVELOPE_KEYS: &[&str] = &["data", "result", "resp", "response", "info"];
    const MAX_DEPTH: usize = 8;
    let mut out = vec![value];
    let mut cur = value;
    for _ in 0..MAX_DEPTH {
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

/// 归一化 access token 为完整的 `Cloud-IDE-JWT <token>` 形态。
pub fn normalize_access_token(token: &str) -> String {
    trae_account::normalize_jwt(token)
}

/// 用 `refresh_token` 换一份新的 access/refresh token。
///
/// 请求体形态（Trae CN `main.js` 逆向 + 上游抓包固化）：`{ClientID, RefreshToken,
/// DeviceInfo{DeviceID,...}}`。响应里 access/refresh 可能同时轮换，也可能只换
/// access —— 因此返回的 refresh 是 `Option`。
///
/// 错误文案带 `服务端拒绝` 前缀时，调用方按 [`RefreshFailureKind::Rejected`]
/// 分类（立即置失效）；其余按瞬时失败处理。
pub async fn exchange_token(
    refresh_token: &str,
) -> Result<(String, Option<String>, Option<i64>), String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| format!("构造 HTTP 客户端失败: {e}"))?;
    let body = json!({
        "ClientID": OAUTH_CLIENT_ID,
        "RefreshToken": refresh_token,
        "DeviceInfo": {
            "PlatformCode": "IDE_PC",
            "DeviceType": "PC",
        },
    });
    let resp = client
        .post(EXCHANGE_URL)
        .header("content-type", "application/json")
        // 真实客户端此头为空串，缺失会被网关按未知客户端处理
        .header("x-cloudide-token", "")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("请求 ExchangeToken 失败（网络/代理问题）: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(format!(
            "服务端拒绝（HTTP {status}）：refresh_token 已失效，请重新登录"
        ));
    }
    if !status.is_success() {
        return Err(format!(
            "ExchangeToken 返回 HTTP {status}: {}",
            text.chars().take(200).collect::<String>()
        ));
    }
    let parsed: Value = serde_json::from_str(&text).map_err(|e| {
        format!(
            "ExchangeToken 响应不是 JSON: {e}（原始前 200 字: {}）",
            text.chars().take(200).collect::<String>()
        )
    })?;
    // 业务码明确拒绝 → 归到 Rejected（前缀由调用方识别）
    if let Some(code) = parsed.get("code").and_then(Value::as_i64) {
        if code != 0 {
            let msg = parsed
                .get("msg")
                .or_else(|| parsed.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("(无错误信息)");
            return Err(format!("服务端拒绝（code={code} {msg}）"));
        }
    }
    let (access, refresh) = parse_exchange_response(&parsed)?;
    let expires = extract_refresh_expiry(&parsed);
    Ok((normalize_access_token(&access), refresh, expires))
}

/// OAuth 客户端 id（与 Trae CN 客户端一致；抓包固化值）。
pub const OAUTH_CLIENT_ID: &str = "ono9krqynydwx5";

/// 从响应体里取 `refresh_token` 过期时间（兼容秒/毫秒两种时间戳）。
pub fn extract_refresh_expiry(body: &Value) -> Option<i64> {
    const KEYS: &[&str] = &[
        "refresh_token_expires_at",
        "refresh_expires_at",
        "refreshTokenExpiresAt",
        "refresh_expires_at_ms",
    ];
    let raw = unwrap_scopes(body)
        .iter()
        .find_map(|s| KEYS.iter().find_map(|k| s.get(*k)).and_then(Value::as_i64))?;
    Some(if raw > 10_000_000_000 { raw / 1000 } else { raw })
}

/// 记录一次刷新失败：递增连续失败计数，按分类决定是否判定失效。
///
/// 返回 `(连续失败次数, 是否已判定失效)`，供调用方写日志/回显。
pub fn record_refresh_failure(uid: &str, kind: RefreshFailureKind) -> (i64, bool) {
    let rejected = kind == RefreshFailureKind::Rejected;
    trae_account::note_refresh_failure(uid, rejected)
}

/// 刷新成功的收尾：清零失败计数、解除失效标记、写入新凭证。
pub fn apply_refresh_success(
    uid: &str,
    access_token: &str,
    new_refresh_token: Option<&str>,
    rt_expires_at: Option<i64>,
) -> Result<Value, String> {
    trae_account::update_jwt(uid, access_token)?;
    trae_account::mark_refresh_success(uid, new_refresh_token, rt_expires_at)
}

/// 刷新结果的可序列化形态（宿主直接回给界面）。
pub fn refresh_result_json(
    uid: &str,
    ok: bool,
    message: &str,
    jwt_exp_hours: Option<f64>,
) -> Value {
    json!({
        "userId": uid,
        "ok": ok,
        "message": message,
        "jwtExpHours": jwt_exp_hours,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    /// 基准时刻（固定 now，全部边界相对它推算，不依赖真实时钟）
    const T0: i64 = 1_700_000_000;
    const H48: i64 = 48 * 3600;

    /// 构造带指定 exp 的最小 JWT（parse 不验签）。
    fn jwt_with_exp(exp: Option<i64>) -> String {
        let payload = match exp {
            Some(e) => format!(r#"{{"data":{{"id":"u-test"}},"exp":{e}}}"#),
            None => r#"{"data":{"id":"u-test"}}"#.to_string(),
        };
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.as_bytes());
        format!("h.{enc}.s")
    }

    #[test]
    fn 新鲜_jwt_与_rt_都充足时不刷新() {
        let jwt = jwt_with_exp(Some(T0 + H48 + 3600));
        assert!(!lazy_refresh_needed(&jwt, Some(T0 + 96 * 3600), T0));
    }

    #[test]
    fn jwt_恰好_48h_触发刷新() {
        let jwt = jwt_with_exp(Some(T0 + H48));
        assert!(lazy_refresh_needed(&jwt, Some(T0 + 96 * 3600), T0));
    }

    #[test]
    fn jwt_不足_48h_触发刷新() {
        let jwt = jwt_with_exp(Some(T0 + H48 - 3600));
        assert!(lazy_refresh_needed(&jwt, Some(T0 + 96 * 3600), T0));
    }

    #[test]
    fn rt_临期时即使_jwt_新鲜也刷新() {
        let jwt = jwt_with_exp(Some(T0 + 200 * 3600));
        assert!(lazy_refresh_needed(&jwt, Some(T0 + H48 - 1), T0));
    }

    #[test]
    fn rt_恰好_48h_不触发() {
        let jwt = jwt_with_exp(Some(T0 + 200 * 3600));
        assert!(!lazy_refresh_needed(&jwt, Some(T0 + H48), T0));
    }

    #[test]
    fn exp_缺失或损坏时保守刷新() {
        assert!(lazy_refresh_needed("not-a-jwt", Some(T0 + 96 * 3600), T0));
        assert!(lazy_refresh_needed(&jwt_with_exp(None), Some(T0 + 96 * 3600), T0));
    }

    #[test]
    fn 已过期_jwt_触发刷新() {
        let jwt = jwt_with_exp(Some(T0 - 3600));
        assert!(lazy_refresh_needed(&jwt, Some(T0 + 96 * 3600), T0));
    }

    #[test]
    fn 无_rt_时只要_jwt_新鲜就不提前刷() {
        let jwt = jwt_with_exp(Some(T0 + 49 * 3600));
        assert!(!lazy_refresh_needed(&jwt, None, T0));
    }

    #[test]
    fn 响应解析_顶层字段() {
        let body = json!({"code": 0, "AccessToken": "tok", "RefreshToken": "rt"});
        let (a, r) = parse_exchange_response(&body).unwrap();
        assert_eq!(a, "tok");
        assert_eq!(r.as_deref(), Some("rt"));
    }

    #[test]
    fn 响应解析_信封下钻() {
        let body = json!({"data": {"result": {"access_token": "tok2", "refresh_token": "rt2"}}});
        let (a, r) = parse_exchange_response(&body).unwrap();
        assert_eq!(a, "tok2");
        assert_eq!(r.as_deref(), Some("rt2"));
    }

    #[test]
    fn 响应解析_数字_code_非零判为拒绝() {
        let body = json!({"code": 12153, "msg": "Offline user session not found"});
        let err = parse_exchange_response(&body).unwrap_err();
        assert!(err.contains("12153"), "{err}");
    }

    /// 关键回归：**没有 code 字段**的异构响应不得被误判为拒绝。
    #[test]
    fn 响应解析_无_code_字段时不误判拒绝() {
        let body = json!({"AccessToken": "tok3"});
        let (a, _) = parse_exchange_response(&body).unwrap();
        assert_eq!(a, "tok3");
    }

    #[test]
    fn 响应解析_无_access_token_时报错() {
        let body = json!({"code": 0, "RefreshToken": "rt"});
        assert!(parse_exchange_response(&body).is_err());
    }

    #[test]
    fn 刷新过期时间_秒与毫秒都归一为秒() {
        assert_eq!(extract_refresh_expiry(&json!({"refresh_token_expires_at": 1_700_000_000})), Some(1_700_000_000));
        assert_eq!(
            extract_refresh_expiry(&json!({"data": {"refreshTokenExpiresAt": 1_700_000_000_000i64}})),
            Some(1_700_000_000)
        );
        assert_eq!(extract_refresh_expiry(&json!({})), None);
    }

    #[test]
    fn access_token_归一化补前缀() {
        assert_eq!(normalize_access_token("abc"), "Cloud-IDE-JWT abc");
        assert_eq!(
            normalize_access_token("Cloud-IDE-JWT abc"),
            "Cloud-IDE-JWT abc"
        );
    }
}
