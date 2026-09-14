//! Token 刷新与保活。
//!
//! 对照 server.py `refresh_account_token` / `ensure_fresh_token` /
//! `run_keepalive_cycle`。

use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;

use crate::modules::account::{build_auth_headers, upsert_account};
use crate::modules::config::{
    http_request, load_checkin_config, norm_ts, now_ms, RunFlagGuard, api_endpoint_for,
    WORKBUDDY_API_PREFIX,
};

static KEEPALIVE_RUNNING: AtomicBool = AtomicBool::new(false);

/// 刷新响应分类：决定是否把账号标记为「需重新登录」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// 成功（code 0/200 且带回 accessToken）。
    Ok,
    /// 服务端明确拒绝：refresh token 已失效，需人工重新登录。
    NeedsRelogin,
    /// 传输层失败（网络不可达 / 代理未启动）：**不改变**需重登判定。
    TransportError,
}

/// 纯函数：按刷新响应判定结果。
///
/// `code == -1` 是 `http_request` 对「请求根本没发出去 / 响应无法解析」的约定
/// （见 `config::http_request_with_proxy`），与服务端是否认可凭证无关。
/// 把它当作「需重新登录」会造成误报：实测一次代理抖动就让整批国际版账号
/// 同时被打上该标记，进而被排除出网关凭证目录，直到下次刷新成功才恢复。
pub fn classify_refresh_response(resp: &Value) -> RefreshOutcome {
    let code = resp.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code == -1 {
        return RefreshOutcome::TransportError;
    }
    if code != 0 && code != 200 {
        return RefreshOutcome::NeedsRelogin;
    }
    // code 正常但没带回 accessToken：响应形态异常，同样不能算成功。
    let data = resp.get("data").cloned().unwrap_or_else(|| json!({}));
    let has_at = data
        .get("accessToken")
        .and_then(Value::as_str)
        .or_else(|| data.get("access_token").and_then(Value::as_str))
        .is_some();
    if has_at {
        RefreshOutcome::Ok
    } else {
        RefreshOutcome::NeedsRelogin
    }
}

/// 历史误报的标记原因：传输层失败被当作凭证失效（旧版本行为）。
///
/// 旧版本把所有非 0/200 的刷新结果都写成 needs_relogin，其中 `code=-1`
/// 表示请求根本没发出去（网络不可达 / 代理未启动），与凭证是否有效无关。
fn is_transport_error_reason(reason: &str) -> bool {
    reason.contains("code=-1")
}

/// 账号是否**确实**需要重新登录。
///
/// 兼容历史误报：旧版本留下的 `code=-1` 标记并不代表凭证失效——实测这类账号
/// 的 refresh token 仍能正常换取新凭证。因此这类标记按「不需重登」处理，
/// 避免它们被永久排除出网关账号池。
///
/// 判定口径统一在此处，网关导出、界面展示都复用它。
pub fn needs_relogin(acc: &Value) -> bool {
    if acc.get("needs_relogin").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    let reason = acc
        .get("needs_relogin_reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    !is_transport_error_reason(reason)
}

/// 清除账号库里历史误报的 needs_relogin 标记（传输层失败导致的），返回清理数量。
///
/// 供启动流程调用一次：让升级上来的用户不必手动重新登录就能恢复这些账号。
/// 真正失效的凭证（服务端明确拒绝）不受影响，仍保持需重登状态。
pub fn repair_false_relogin_flags() -> usize {
    let mut accounts = crate::modules::account::load_accounts();
    let mut repaired = 0usize;
    for acc in accounts.iter_mut() {
        if acc.get("needs_relogin").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let reason = acc
            .get("needs_relogin_reason")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !is_transport_error_reason(reason) {
            continue;
        }
        if let Some(map) = acc.as_object_mut() {
            map.remove("needs_relogin");
            map.remove("needs_relogin_reason");
        }
        repaired += 1;
    }
    if repaired > 0 {
        if let Err(e) = crate::modules::account::save_accounts(&accounts) {
            eprintln!("[refresh] 清理误报的需重登标记失败: {e}");
            return 0;
        }
    }
    repaired
}

/// 刷新单账号 token（POST /v2/plugin/auth/token/refresh），成功则落盘并返回新账号。
///
/// 仅当服务端**明确拒绝**凭证时才标记 needs_relogin，避免无限重试；
/// 传输层失败只记录瞬时错误，不改变该标记（见 `classify_refresh_response`）。
pub async fn refresh_account_token(mut account: Value) -> Value {
    let previous_access_token = account
        .get("access_token")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let rt = account
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default();
    if rt.is_empty() {
        account["needs_relogin"] = json!(true);
        account["needs_relogin_reason"] = json!("缺少 refresh token，无法刷新，需重新登录");
        let _ = upsert_account(&account);
        return account;
    }

    let mut headers = build_auth_headers(&account);
    headers.insert("X-Refresh-Token".to_string(), rt.clone());
    // 刷新端点随账号区域走：国际版与国服的域不同
    let url = format!(
        "{}{WORKBUDDY_API_PREFIX}/auth/token/refresh",
        api_endpoint_for(&account)
    );
    let resp = http_request(&url, "POST", Some(json!({})), Some(&headers)).await;
    let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);

    match classify_refresh_response(&resp) {
        // 传输层失败：只记录瞬时错误，保留上一次的判定结果与凭证。
        RefreshOutcome::TransportError => {
            account["last_refresh_error"] = json!(format!(
                "网络请求失败: {}",
                resp.get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("未知错误")
            ));
            account["last_refresh_error_at"] = json!(now_ms());
            let _ = upsert_account(&account);
            return account;
        }
        RefreshOutcome::NeedsRelogin => {
            let reason = if code != 0 && code != 200 {
                format!(
                    "刷新失败(code={code}): {}",
                    resp.get("message")
                        .or_else(|| resp.get("msg"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("未知错误")
                )
            } else {
                "刷新响应缺少 accessToken".to_string()
            };
            account["needs_relogin"] = json!(true);
            account["needs_relogin_reason"] = json!(reason);
            let _ = upsert_account(&account);
            return account;
        }
        RefreshOutcome::Ok => {}
    }

    let data = resp.get("data").cloned().unwrap_or_else(|| json!({}));
    let Some(new_at) = data
        .get("accessToken")
        .and_then(|v| v.as_str())
        .or_else(|| data.get("access_token").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
    else {
        // classify 已保证此处必有 token；防御性兜底，避免 unwrap 崩溃。
        account["needs_relogin"] = json!(true);
        account["needs_relogin_reason"] = json!("刷新响应缺少 accessToken");
        let _ = upsert_account(&account);
        return account;
    };

    account["access_token"] = json!(new_at);
    if let Some(new_rt) = data
        .get("refreshToken")
        .and_then(|v| v.as_str())
        .or_else(|| data.get("refresh_token").and_then(|v| v.as_str()))
    {
        account["refresh_token"] = json!(new_rt);
    }
    // 官方接口只返回相对 expiresIn（秒），需换算为绝对时间戳
    let new_exp = norm_ts(data.get("expiresAt").or_else(|| data.get("expires_at")));
    let new_exp = match new_exp {
        Some(v) => Some(v),
        None => data
            .get("expiresIn")
            .and_then(|v| v.as_i64())
            .map(|e| now_ms() + e * 1000),
    };
    if let Some(v) = new_exp {
        account["expiresAt"] = json!(v);
    }
    let fallback_rt_exp = norm_ts(
        account
            .get("auth_raw")
            .and_then(|a| a.get("refreshExpiresAt")),
    );
    let mut new_rt_exp = norm_ts(
        data.get("refreshExpiresAt")
            .or_else(|| data.get("refresh_expires_at")),
    );
    if new_rt_exp.is_none() {
        new_rt_exp = fallback_rt_exp;
    }
    let new_rt_exp = match new_rt_exp {
        Some(v) => Some(v),
        None => data
            .get("refreshExpiresIn")
            .and_then(|v| v.as_i64())
            .map(|e| now_ms() + e * 1000),
    };
    if let Some(v) = new_rt_exp {
        account["refreshExpiresAt"] = json!(v);
    }
    account["refreshedAt"] = json!(now_ms());
    let map = account.as_object_mut().unwrap();
    map.remove("needs_relogin");
    map.remove("needs_relogin_reason");
    // 刷新成功即清除上一轮的网络错误记录（瞬时故障不该留在账号画像里）。
    map.remove("last_refresh_error");
    map.remove("last_refresh_error_at");
    let _ = upsert_account(&account);
    // 当前 CLI 账号刷新后同步 settings env。
    // 同步失败不阻断 WorkBuddy 保活；状态接口会根据 settings 与账号库是否
    // 一致显示“待同步”，避免把认证配置错误混入账号数据。
    let _ = crate::modules::codebuddy_cli::sync_windows_env_for_account(
        &account,
        previous_access_token.as_deref(),
    );
    account
}

/// 惰性刷新：expiresAt 缺失或剩余 < lazy_refresh_hours 则刷新。返回最新账号。
pub async fn ensure_fresh_token(mut account: Value, cfg: &Value) -> Value {
    let lazy_h = cfg
        .get("lazy_refresh_hours")
        .and_then(|v| v.as_i64())
        .unwrap_or(24);
    let exp = account.get("expiresAt").and_then(|v| v.as_i64());
    let stale = match exp {
        Some(e) => now_ms() >= e || e - now_ms() < lazy_h * 3600 * 1000,
        None => true,
    };
    let has_rt = !account
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .is_empty();
    if stale && has_rt {
        account = refresh_account_token(account).await;
    }
    account
}

/// 保活检查：每天由后台循环调用一次，默认（keepalive_days <= 0）无条件刷新
/// 全部带 refresh token 的账号；keepalive_days > 0 时仅刷新剩余不足该天数的账号。
///
/// 高频保活是为了避免官方服务端清理闲置的 refresh 会话——曾出现闲置数天后
/// 刷新返回 12153 invalid_grant（Session doesn't have required client）导致
/// 账号被迫重新登录。
pub async fn run_keepalive_cycle() -> Value {
    let Some(_guard) = RunFlagGuard::try_acquire(&KEEPALIVE_RUNNING) else {
        return json!({"skipped": "already_running"});
    };
    let cfg = load_checkin_config();
    let keep_days = cfg
        .get("keepalive_days")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let accounts = crate::modules::account::load_accounts();
    let total = accounts.len();
    let mut results: Vec<Value> = Vec::new();
    for mut acc in accounts {
        let exp = acc.get("expiresAt").and_then(|v| v.as_i64());
        let stale = keep_days <= 0
            || match exp {
                Some(e) => now_ms() >= e || e - now_ms() < keep_days * 24 * 3600 * 1000,
                None => true,
            };
        if !stale {
            continue;
        }
        if acc
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
        {
            acc["needs_relogin"] = json!(true);
            acc["needs_relogin_reason"] = json!("缺少 refresh token，无法保活，需重新登录");
            let _ = upsert_account(&acc);
            results.push(json!({
                "email": crate::modules::account::account_display_name(&acc),
                "status": "missing_rt",
            }));
            continue;
        }
        let fresh = refresh_account_token(acc).await;
        let failed = fresh.get("needs_relogin").and_then(|v| v.as_bool()) == Some(true);
        results.push(json!({
            "email": crate::modules::account::account_display_name(&fresh),
            "status": if failed { "failed" } else { "ok" },
            "error": if failed {
                fresh.get("needs_relogin_reason").and_then(|v| v.as_str()).map(|s| s.to_string())
            } else {
                None
            },
        }));
    }
    json!({"checked": total, "refreshed": results})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归保护：传输层失败（code=-1）绝不能被判为「需重新登录」。
    ///
    /// 曾经的缺陷是把所有非 0/200 都当作凭证失效，于是一次代理抖动就让整批
    /// 国际版账号被打上 needs_relogin，进而被排除出网关凭证目录。
    #[test]
    fn transport_error_does_not_require_relogin() {
        let transport = json!({
            "code": -1,
            "message": "error sending request for url (https://www.workbuddy.ai/v2/plugin/auth/token/refresh)",
        });
        assert_eq!(
            classify_refresh_response(&transport),
            RefreshOutcome::TransportError
        );

        // 缺少 message 的 code=-1 同样是传输层失败
        assert_eq!(
            classify_refresh_response(&json!({"code": -1})),
            RefreshOutcome::TransportError
        );
    }

    /// 服务端明确拒绝才标记需重登。
    #[test]
    fn server_rejection_requires_relogin() {
        // 12153 session 失效
        assert_eq!(
            classify_refresh_response(&json!({
                "code": 12153,
                "message": "Offline user session not found",
            })),
            RefreshOutcome::NeedsRelogin
        );
        // 401 同样
        assert_eq!(
            classify_refresh_response(&json!({"code": 401, "message": "unauthorized"})),
            RefreshOutcome::NeedsRelogin
        );
        // code 正常但缺 accessToken：形态异常，也算需重登
        assert_eq!(
            classify_refresh_response(&json!({"code": 0, "data": {}})),
            RefreshOutcome::NeedsRelogin
        );
    }

    #[test]
    fn successful_refresh_is_ok() {
        assert_eq!(
            classify_refresh_response(&json!({
                "code": 0,
                "data": {"accessToken": "AT-NEW", "refreshToken": "RT-NEW"},
            })),
            RefreshOutcome::Ok
        );
        // 兼容 snake_case 与 code=200
        assert_eq!(
            classify_refresh_response(&json!({
                "code": 200,
                "data": {"access_token": "AT-NEW"},
            })),
            RefreshOutcome::Ok
        );
    }

    /// 历史误报：旧版本把 `code=-1`（网络失败）写成 needs_relogin。
    ///
    /// 实测这类账号的 refresh token 仍然有效（能正常换取新凭证），
    /// 因此判定时必须把这种标记视为「不需重登」，否则账号会被永久
    /// 排除出网关账号池。
    #[test]
    fn legacy_transport_error_flags_are_not_treated_as_relogin() {
        let legacy = json!({
            "uid": "uid-1",
            "needs_relogin": true,
            "needs_relogin_reason": "刷新失败(code=-1): error sending request for url (https://www.workbuddy.ai/v2/plugin/auth/token/refresh)",
        });
        assert!(
            !needs_relogin(&legacy),
            "传输层失败留下的历史标记不得视为需重登"
        );

        // 真正失效的凭证仍然算需重登
        let real = json!({
            "uid": "uid-2",
            "needs_relogin": true,
            "needs_relogin_reason": "刷新失败(code=12153): Offline user session not found",
        });
        assert!(needs_relogin(&real), "服务端拒绝必须保持需重登");

        // 未标记的账号自然不需要重登
        assert!(!needs_relogin(&json!({"uid": "uid-3"})));

        // 标记为 true 但原因缺失：无法判断是误报，保守保持需重登
        let no_reason = json!({"uid": "uid-4", "needs_relogin": true});
        assert!(needs_relogin(&no_reason));
    }
}
