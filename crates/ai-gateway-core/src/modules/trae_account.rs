//! Trae 账号库：存储、JWT 解析、设备指纹绑定。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/models.rs`（`RawAccount`）+
//! `src-tauri/src/jwt.rs` + `commands/accounts.rs`（设备派生部分）。
//!
//! ## 与 WorkBuddy 账号库分开存
//!
//! Trae 账号的凭证形态完全不同（`Cloud-IDE-JWT` + `refresh_token`，而非
//! WorkBuddy 的 `access_token` + `domain`），且**两个 Trae 应用共用同一份账号库**
//! （Trae Work 与 Trae 只是同一账号的两套登录态，不是两批账号）。
//! 混进 WorkBuddy 的 `accounts.json` 会让两边的身份匹配、导出、网关凭证生成
//! 全部需要按类型分支，得不偿失。

use serde_json::{json, Value};

use crate::modules::config;
use crate::modules::trae_device;

/// JWT 解析结果（不校验签名，仅本地展示与路由用途）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JwtInfo {
    pub user_id: Option<String>,
    /// 剩余有效小时数。
    pub exp_hours: Option<f64>,
    pub exp_timestamp: Option<i64>,
}

impl JwtInfo {
    /// 由剩余小时数推导状态：`>24h` ok / `>0` warn / 已过期 expired / 未知 unknown。
    pub fn status(&self) -> &'static str {
        match self.exp_hours {
            Some(h) if h > 24.0 => "ok",
            Some(h) if h > 0.0 => "warn",
            Some(_) => "expired",
            None => "unknown",
        }
    }
}

/// 解析 JWT，提取 uid 与过期时间。
///
/// 兼容三种前缀（`Cloud-IDE-JWT ` / `Bearer ` / 裸 token）：上游在不同路径下
/// 观察到过不同前缀，用户手动粘贴时更是三种都可能出现。
pub fn parse_jwt(jwt_full: &str) -> JwtInfo {
    use base64::Engine;

    let trimmed = jwt_full.trim();
    let token = trimmed
        .strip_prefix("Cloud-IDE-JWT ")
        .or_else(|| trimmed.strip_prefix("Bearer "))
        .unwrap_or(trimmed)
        .trim();

    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return JwtInfo::default();
    }
    // JWT payload 是 base64url 无填充；先去掉可能的残余 '=' 再解码
    let payload_b64 = parts[1].trim_end_matches('=');
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload_b64) else {
        return JwtInfo::default();
    };
    let Ok(payload) = serde_json::from_slice::<Value>(&bytes) else {
        return JwtInfo::default();
    };

    // uid 优先取 data.id（Cloud-IDE id 空间），回退 auth_id / sub
    let user_id = payload
        .get("data")
        .and_then(|d| d.get("id"))
        .and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
        })
        .or_else(|| {
            payload
                .get("auth_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| payload.get("sub").and_then(Value::as_str).map(str::to_string));

    // exp 可能是整数、浮点或数字字符串
    let exp_timestamp = payload.get("exp").and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_f64().map(|f| f as i64))
            .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
    });
    let exp_hours = exp_timestamp.map(|exp| (exp - chrono::Utc::now().timestamp()) as f64 / 3600.0);

    JwtInfo {
        user_id,
        exp_hours,
        exp_timestamp,
    }
}

/// 把裸 token 规范成 `Cloud-IDE-JWT <token>` 形态（幂等）。
pub fn normalize_jwt(jwt: &str) -> String {
    let trimmed = jwt.trim();
    if trimmed.starts_with("Cloud-IDE-JWT ") || trimmed.starts_with("Bearer ") {
        trimmed.to_string()
    } else {
        format!("Cloud-IDE-JWT {trimmed}")
    }
}

/// 账号库文件（`<store_dir>/trae_accounts.json`）。
pub fn accounts_file() -> std::path::PathBuf {
    config::store_dir().join("trae_accounts.json")
}

/// 读取全部 Trae 账号（缺失或损坏返回空列表）。
pub fn load_accounts() -> Vec<Value> {
    std::fs::read_to_string(accounts_file())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| {
            v.get("accounts")
                .and_then(Value::as_array)
                .cloned()
                .or_else(|| v.as_array().cloned())
        })
        .unwrap_or_default()
}

/// 写回账号库（原子写，保持 `{"accounts": [...]}` 结构）。
pub fn save_accounts(accounts: &[Value]) -> std::io::Result<()> {
    std::fs::create_dir_all(config::store_dir())?;
    let content = serde_json::to_string_pretty(&json!({ "accounts": accounts })).unwrap_or_default();
    config::atomic_write(&accounts_file(), &content)
}

/// 按 uid 查找账号。
pub fn find_account(uid: &str) -> Option<Value> {
    let uid = uid.trim();
    load_accounts()
        .into_iter()
        .find(|a| a.get("user_id").and_then(Value::as_str) == Some(uid))
}

/// 账号展示名（`name` → `账号_<uid 前 8 位>`）。
pub fn display_name(acc: &Value) -> String {
    acc.get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            let uid = acc.get("user_id").and_then(Value::as_str).unwrap_or("");
            format!("账号_{}", uid.chars().take(8).collect::<String>())
        })
}

/// 新增或更新账号（按 uid 去重）。
///
/// 更新已存在的账号时：刷新 `jwt` / `refresh_token`，并把连续刷新失败计数清零 ——
/// 用户重新登录意味着凭证已换新，旧的失败历史不应继续影响调度决策。
pub fn upsert_account(
    uid: &str,
    name: Option<&str>,
    jwt: &str,
    refresh_token: Option<&str>,
) -> Result<Value, String> {
    let uid = uid.trim();
    if uid.is_empty() {
        return Err("缺少账号标识（uid）".to_string());
    }
    if jwt.trim().is_empty() {
        return Err("缺少 JWT".to_string());
    }
    let mut accounts = load_accounts();
    let now = config::utc_iso();
    let normalized = normalize_jwt(jwt);

    if let Some(existing) = accounts
        .iter_mut()
        .find(|a| a.get("user_id").and_then(Value::as_str) == Some(uid))
    {
        let obj = existing
            .as_object_mut()
            .ok_or_else(|| "账号记录格式异常".to_string())?;
        obj.insert("jwt".to_string(), json!(normalized));
        if let Some(rt) = refresh_token.map(str::trim).filter(|s| !s.is_empty()) {
            obj.insert("refresh_token".to_string(), json!(rt));
        }
        if let Some(n) = name.map(str::trim).filter(|s| !s.is_empty()) {
            obj.insert("name".to_string(), json!(n));
        }
        obj.insert("updated_at".to_string(), json!(now));
        obj.insert("refresh_token_fails".to_string(), json!(0));
        obj.insert("refresh_token_invalid".to_string(), json!(false));
        let updated = existing.clone();
        save_accounts(&accounts).map_err(|e| e.to_string())?;
        return Ok(updated);
    }

    let acc = json!({
        "user_id": uid,
        "name": name.map(str::trim).filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("账号_{}", uid.chars().take(8).collect::<String>())),
        "jwt": normalized,
        "refresh_token": refresh_token.map(str::trim).filter(|s| !s.is_empty()),
        "added_at": now,
        "updated_at": now,
        "refresh_token_fails": 0,
        "refresh_token_invalid": false,
    });
    accounts.push(acc.clone());
    save_accounts(&accounts).map_err(|e| e.to_string())?;
    Ok(acc)
}

/// 删除账号。
pub fn delete_account(uid: &str) -> Result<(), String> {
    let mut accounts = load_accounts();
    let before = accounts.len();
    accounts.retain(|a| a.get("user_id").and_then(Value::as_str) != Some(uid.trim()));
    if accounts.len() == before {
        return Err("账号不存在".to_string());
    }
    save_accounts(&accounts).map_err(|e| e.to_string())
}

/// 更新账号的 JWT（刷新流程用）。
pub fn update_jwt(uid: &str, jwt: &str) -> Result<(), String> {
    let mut accounts = load_accounts();
    let acc = accounts
        .iter_mut()
        .find(|a| a.get("user_id").and_then(Value::as_str) == Some(uid.trim()))
        .ok_or_else(|| "账号不存在".to_string())?;
    let obj = acc
        .as_object_mut()
        .ok_or_else(|| "账号记录格式异常".to_string())?;
    obj.insert("jwt".to_string(), json!(normalize_jwt(jwt)));
    obj.insert("updated_at".to_string(), json!(config::utc_iso()));
    save_accounts(&accounts).map_err(|e| e.to_string())
}

/// 标记 refresh_token 连续刷新失败（达到阈值时判定失效）。
pub fn note_refresh_failure(uid: &str, invalid: bool) {
    let mut accounts = load_accounts();
    let Some(acc) = accounts
        .iter_mut()
        .find(|a| a.get("user_id").and_then(Value::as_str) == Some(uid.trim()))
    else {
        return;
    };
    let Some(obj) = acc.as_object_mut() else { return };
    let fails = obj
        .get("refresh_token_fails")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        + 1;
    obj.insert("refresh_token_fails".to_string(), json!(fails));
    if invalid {
        obj.insert("refresh_token_invalid".to_string(), json!(true));
    }
    let _ = save_accounts(&accounts);
}

/// 账号的展示元数据（**不泄露 JWT**）。
///
/// JWT 绝不离开后端：界面只需要知道「有效 / 即将过期 / 已过期」与 uid。
pub fn account_meta(acc: &Value) -> Value {
    let jwt = acc.get("jwt").and_then(Value::as_str).unwrap_or("");
    let info = parse_jwt(jwt);
    let uid = acc
        .get("user_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let device = trae_device::derive_device(&uid);
    json!({
        "userId": uid,
        "name": display_name(acc),
        "addedAt": acc.get("added_at"),
        "updatedAt": acc.get("updated_at"),
        "jwtStatus": info.status(),
        "jwtExpHours": info.exp_hours,
        "jwtExpTimestamp": info.exp_timestamp,
        "hasRefreshToken": acc
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "refreshTokenInvalid": acc
            .get("refresh_token_invalid")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "refreshTokenFails": acc
            .get("refresh_token_fails")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        // 设备指纹脱敏展示：完整值只在后端注入请求头时使用
        "deviceIdMasked": mask_tail(&device.device_id, 4),
    })
}

/// 脱敏：只保留末 `keep` 位。
fn mask_tail(value: &str, keep: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= keep {
        return "*".repeat(chars.len());
    }
    let masked = "*".repeat(chars.len() - keep);
    format!("{masked}{}", chars[chars.len() - keep..].iter().collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    /// 造一个 payload 可控的 JWT（签名段是占位，解析不校验签名）。
    fn make_jwt(payload: Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        format!("{header}.{body}.sig")
    }

    #[test]
    fn 解析_jwt_的_uid_与过期时间() {
        let exp = chrono::Utc::now().timestamp() + 7200;
        let jwt = make_jwt(json!({"data": {"id": "2328112497170937"}, "exp": exp}));
        let info = parse_jwt(&jwt);
        assert_eq!(info.user_id.as_deref(), Some("2328112497170937"));
        assert_eq!(info.exp_timestamp, Some(exp));
        assert!(info.exp_hours.unwrap() > 1.9 && info.exp_hours.unwrap() < 2.1);
        assert_eq!(info.status(), "warn", "2 小时内过期应为 warn");
    }

    #[test]
    fn 解析_jwt_兼容三种前缀() {
        let jwt = make_jwt(json!({"data": {"id": "u1"}, "exp": 9999999999i64}));
        for candidate in [
            jwt.clone(),
            format!("Cloud-IDE-JWT {jwt}"),
            format!("Bearer {jwt}"),
            format!("  Cloud-IDE-JWT {jwt}  "),
        ] {
            assert_eq!(
                parse_jwt(&candidate).user_id.as_deref(),
                Some("u1"),
                "前缀形态 {candidate:.20}… 应能解析"
            );
        }
    }

    #[test]
    fn 解析_jwt_的_id_为数字时也能取到() {
        let jwt = make_jwt(json!({"data": {"id": 2328112497170937i64}}));
        assert_eq!(
            parse_jwt(&jwt).user_id.as_deref(),
            Some("2328112497170937")
        );
    }

    #[test]
    fn 解析_jwt_回退_auth_id_与_sub() {
        let a = make_jwt(json!({"auth_id": "from-auth-id"}));
        assert_eq!(parse_jwt(&a).user_id.as_deref(), Some("from-auth-id"));
        let b = make_jwt(json!({"sub": "from-sub"}));
        assert_eq!(parse_jwt(&b).user_id.as_deref(), Some("from-sub"));
    }

    #[test]
    fn 解析_jwt_对畸形输入不panic() {
        for bad in ["", "not-a-jwt", "a.b", "a.!!!.c", "...."] {
            let info = parse_jwt(bad);
            assert!(info.user_id.is_none() || !bad.is_empty());
        }
    }

    #[test]
    fn exp_为浮点或数字字符串时也能解析() {
        let f = make_jwt(json!({"data": {"id": "u"}, "exp": 9999999999.0}));
        assert_eq!(parse_jwt(&f).exp_timestamp, Some(9999999999));
        let s = make_jwt(json!({"data": {"id": "u"}, "exp": "9999999999"}));
        assert_eq!(parse_jwt(&s).exp_timestamp, Some(9999999999));
    }

    #[test]
    fn 状态判定阈值() {
        let mk = |h: Option<f64>| JwtInfo { exp_hours: h, ..Default::default() }.status();
        assert_eq!(mk(Some(100.0)), "ok");
        assert_eq!(mk(Some(25.0)), "ok");
        assert_eq!(mk(Some(24.0)), "warn");
        assert_eq!(mk(Some(0.5)), "warn");
        assert_eq!(mk(Some(0.0)), "expired");
        assert_eq!(mk(Some(-1.0)), "expired");
        assert_eq!(mk(None), "unknown");
    }

    #[test]
    fn 规范化_jwt_前缀幂等() {
        assert_eq!(normalize_jwt("abc"), "Cloud-IDE-JWT abc");
        assert_eq!(normalize_jwt("Cloud-IDE-JWT abc"), "Cloud-IDE-JWT abc");
        assert_eq!(normalize_jwt("Bearer abc"), "Bearer abc");
        assert_eq!(normalize_jwt("  abc  "), "Cloud-IDE-JWT abc");
    }

    #[test]
    fn 脱敏只保留末几位() {
        assert_eq!(mask_tail("1234567890", 4), "******7890");
        assert_eq!(mask_tail("1234", 4), "****");
        assert_eq!(mask_tail("12", 4), "**");
        assert_eq!(mask_tail("", 4), "");
    }

    #[test]
    fn account_meta_不含_jwt_原文() {
        let exp = chrono::Utc::now().timestamp() + 100_000;
        let jwt = make_jwt(json!({"data": {"id": "u1"}, "exp": exp}));
        let acc = json!({
            "user_id": "u1",
            "name": "测试号",
            "jwt": format!("Cloud-IDE-JWT {jwt}"),
            "refresh_token": "rt-secret",
        });
        let meta = account_meta(&acc);
        let text = serde_json::to_string(&meta).unwrap();
        assert!(!text.contains("rt-secret"), "refresh_token 不得出现在元数据里");
        assert!(!text.contains(&jwt), "JWT 原文不得出现在元数据里");
        assert_eq!(meta["userId"], "u1");
        assert_eq!(meta["name"], "测试号");
        assert_eq!(meta["hasRefreshToken"], true);
        assert_eq!(meta["jwtStatus"], "ok");
    }
}
