//! Qoder 令牌刷新（Go `internal/qoder/token.go` 的移植）。
//!
//! ★ 走 **OpenAPI 域**：`{Region.OpenAPI()}/api/v1/deviceToken/refresh`，只需
//! `Bearer DT` 语义的裸 POST —— **不签名、不带 Authorization**。与模型列表 / 对话
//! 的 Gateway 域 + COSY 签名是两条完全不同的路径（混用会静默失败）。
//!
//! 与 WorkBuddy 的差别：这里的 `drt` **每次刷新都会轮换**，且刷新结果**必须落盘**
//! —— 不落盘的话重启后读到的是旧 `drt`，表现为"重启后账号就登录失效了"。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::cred::{first_non_empty, truncate_str, QoderCred};

/// 刷新提前量：令牌剩余寿命小于它就提前刷新。
///
/// 上游签发 30 天量级的令牌，2 小时余量足够覆盖一次网络重试，又不会因为窗口过大
/// 而频繁刷新（每次刷新都会轮换 `drt`）。
pub(crate) const REFRESH_SKEW: Duration = Duration::from_secs(2 * 60 * 60);

/// 刷新响应体读取上限（Go `io.LimitReader(resp.Body, 1<<20)`）。
const MAX_REFRESH_BYTES: usize = 1 << 20;

/// 刷新端点路径（拼在 OpenAPI 域之后）。
const REFRESH_PATH: &str = "/api/v1/deviceToken/refresh";

/// 刷新响应（Go `refreshResponse`）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct RefreshResponse {
    #[serde(default)]
    token: String,
    #[serde(default)]
    device_token: String,
    #[serde(default)]
    refresh_token: String,
    /// ★ 单位是**毫秒**（不是秒）。
    #[serde(default)]
    expires_in: i64,
    /// RFC3339；`expires_in` 缺失时的兜底。
    #[serde(default)]
    expires_at: String,
}

/// 刷新失败：区分"凭证本身失效（要重新登录）"与"其它失败"。
///
/// Go 侧用 `AuthInvalidError` 表达前者；Rust 的 [`crate::error::GatewayError`] 里
/// **没有**对应变体，故这里自己成型，由调用方决定措辞。
#[derive(Debug)]
pub(crate) enum RefreshError {
    /// 没有 `drt`、或上游以 401/403 明确拒绝 —— 用户必须重新登录。
    AuthInvalid(String),
    /// 网络、5xx、响应格式等可重试/可沿用的失败。
    Other(String),
}

impl RefreshError {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::AuthInvalid(m) | Self::Other(m) => m,
        }
    }
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

/// 是否需要在 `within` 之内刷新（Go `Cred.NeedsRefresh`）。
///
/// - `dt` 为空 → true（没有令牌当然要拿一个）。
/// - `dt_expires_at <= 0` → true（★ 到期时间未知时**保守返回 true**：宁可按需刷
///   一次，也不要拿一个可能已过期的令牌去打上游 —— 那样失败信息指向"登录失效"，
///   会把排查引到错误的方向）。
/// - 否则比较 `now + within >= dt_expires_at`。
pub(crate) fn needs_refresh(cr: &QoderCred, within: Duration) -> bool {
    if cr.dt.is_empty() {
        return true;
    }
    if cr.dt_expires_at <= 0 {
        return true;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    now + within.as_secs() as i64 >= cr.dt_expires_at
}

/// 刷新设备令牌（Go `Cred.RefreshToken`），成功后就地更新并**落盘**。
///
/// 落盘的理由见模块文档：`drt` 每次刷新都轮换，不写回就等于下次启动拿着废令牌。
pub(crate) async fn refresh_token(
    cr: &mut QoderCred,
    http: &reqwest::Client,
) -> Result<(), RefreshError> {
    if cr.drt.is_empty() {
        return Err(RefreshError::AuthInvalid(
            "没有 refreshToken，无法刷新（需要重新登录）".to_string(),
        ));
    }
    let endpoint = format!("{}{}", cr.region.openapi(), REFRESH_PATH);
    let payload = serde_json::json!({ "refresh_token": cr.drt });

    let resp = http
        .post(&endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::USER_AGENT, "Go-http-client/2.0")
        .json(&payload)
        .send()
        .await
        .map_err(|e| RefreshError::Other(format!("刷新令牌请求失败: {e}")))?;

    let status = resp.status().as_u16();
    let raw = super::client::read_limited(resp, MAX_REFRESH_BYTES).await;
    let text = String::from_utf8_lossy(&raw).into_owned();
    if status == 401 || status == 403 {
        return Err(RefreshError::AuthInvalid(format!(
            "上游拒绝刷新（HTTP {status}）：{}",
            truncate_str(&text, 200)
        )));
    }
    if status >= 400 {
        return Err(RefreshError::Other(format!(
            "刷新令牌失败（HTTP {status}）：{}",
            truncate_str(&text, 200)
        )));
    }

    let out: RefreshResponse = serde_json::from_str(&text).map_err(|e| {
        RefreshError::Other(format!(
            "刷新响应解析失败: {e}（原始内容：{}）",
            truncate_str(&text, 160)
        ))
    })?;
    let new_dt = first_non_empty(&[out.token.as_str(), out.device_token.as_str()]);
    if new_dt.is_empty() {
        return Err(RefreshError::Other(format!(
            "刷新响应里没有 token 字段（原始内容：{}）",
            truncate_str(&text, 160)
        )));
    }
    cr.dt = new_dt;
    // ★ 只有上游给了新 drt 才轮换：给空值就当它没变，别把能用的令牌抹掉。
    if !out.refresh_token.is_empty() {
        cr.drt = out.refresh_token;
    }
    if out.expires_in > 0 {
        // ★ 毫秒。
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        cr.dt_expires_at = now_ms / 1000 + out.expires_in / 1000;
    } else if !out.expires_at.is_empty() {
        if let Ok(t) = chrono::DateTime::parse_from_rfc3339(&out.expires_at) {
            cr.dt_expires_at = t.timestamp();
        }
    }
    cr.save_atomic().map_err(RefreshError::Other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::products::qoder::region::QoderRegion;

    fn cred(dt: &str, expires_at: i64) -> QoderCred {
        QoderCred {
            dt: dt.to_string(),
            dt_expires_at: expires_at,
            ..Default::default()
        }
    }

    fn now_secs() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    #[test]
    fn needs_refresh_is_conservative_when_token_or_expiry_is_missing() {
        assert!(needs_refresh(&cred("", 0), REFRESH_SKEW));
        assert!(needs_refresh(&cred("dt-x", 0), REFRESH_SKEW));
        assert!(needs_refresh(&cred("dt-x", -1), REFRESH_SKEW));
    }

    #[test]
    fn needs_refresh_uses_the_skew_window() {
        let now = now_secs();
        // 还剩 1 小时 < 2 小时余量 ⇒ 该刷。
        assert!(needs_refresh(&cred("dt-x", now + 3600), REFRESH_SKEW));
        // 还剩 3 小时 ⇒ 不刷。
        assert!(!needs_refresh(&cred("dt-x", now + 3 * 3600), REFRESH_SKEW));
        // 已过期 ⇒ 该刷。
        assert!(needs_refresh(&cred("dt-x", now - 10), REFRESH_SKEW));
    }

    #[tokio::test]
    async fn refresh_requires_a_refresh_token() {
        let mut c = cred("dt-x", 0);
        c.region = QoderRegion::CN;
        let http = reqwest::Client::new();
        let err = refresh_token(&mut c, &http)
            .await
            .expect_err("没有 drt 时必须失败");
        match err {
            RefreshError::AuthInvalid(m) => {
                assert!(m.contains("没有 refreshToken"), "{m}");
            }
            other => panic!("应当是 AuthInvalid，实际 {other:?}"),
        }
    }

    #[test]
    fn refresh_response_prefers_token_then_device_token() {
        // 逐字对照 Go `firstNonEmpty(out.Token, out.DeviceToken)`。
        let out: RefreshResponse =
            serde_json::from_str(r#"{"token":"","device_token":"dt-from-device"}"#).unwrap();
        assert_eq!(
            first_non_empty(&[out.token.as_str(), out.device_token.as_str()]),
            "dt-from-device"
        );
        let out2: RefreshResponse =
            serde_json::from_str(r#"{"token":"dt-main","device_token":"dt-from-device"}"#).unwrap();
        assert_eq!(
            first_non_empty(&[out2.token.as_str(), out2.device_token.as_str()]),
            "dt-main"
        );
    }

    #[test]
    fn refresh_response_tolerates_unknown_fields_and_missing_ones() {
        let out: RefreshResponse = serde_json::from_str(r#"{"foo":1}"#).unwrap();
        assert_eq!(out.token, "");
        assert_eq!(out.expires_in, 0);
        assert_eq!(out.expires_at, "");
    }
}
