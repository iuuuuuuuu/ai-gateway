//! 豆包会话保活与续期。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/tasks/doubao_session.rs`。
//!
//! ## 为什么真正的续期手段是「启动客户端」而不是 HTTP
//!
//! 实测（豆包 Chromium 147）：`Local State` 的 `os_crypt.encrypted_key` 经
//! DPAPI + AES-256-GCM 解出的**仍是二进制密文** —— 客户端在 Chromium 的 `v10`
//! 之外还有一层客户端级加密。因此离线拿不到明文 `sessionid`，也就无法用 HTTP
//! 主动续期。而字节 passport 是 30 天**滑动**续期：只要客户端带有效会话上线一次，
//! 服务端就顺延。所以保活 = 启动 → 等待落盘 → 优雅关闭。
//!
//! 本模块的 cookie 解密**仅用于诊断**（告诉用户「凭证为什么读不出来」）。
//!
//! ## 两段式探活
//!
//! 1. `api_probe`（**权威**）：POST 会员额度接口。为什么不用 `info/v2/` ——
//!    它对任意 sid 都返回 200 + SPA HTML，200 完全不能作为有效性的判据。
//! 2. `renew_probe`（保活）：GET 轻量端点，解析 `Set-Cookie` 拿服务端可能下发的新
//!    `sessionid` / `sid_guard`。

use serde_json::{json, Value};

use crate::modules::doubao_account;
use crate::modules::config;

/// 保活端点默认值（通知未读数，轻量、必须登录）。
pub const DEFAULT_RENEW_URL: &str = "https://www.doubao.com/info/v2/";
/// 探活端点默认值（会员额度汇总，POST）。
pub const DEFAULT_PROBE_URL: &str =
    "https://www.doubao.com/alice/commerce/sale/subscription/quota/summary/";
/// 会话失效业务码。
pub const SESSION_EXPIRED_CODE: i64 = 710012001;

/// 需要关注的 cookie 名。
const TARGET_COOKIES: [&str; 5] = [
    "sessionid",
    "sessionid_ss",
    "sid_tt",
    "uid_tt",
    "sid_guard",
];

/// 探活结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeStatus {
    /// 会话有效。
    Ok,
    /// 会话已失效（需重新登录 / 重新抓包）。
    Expired,
    /// 无法判定（网络错误等）—— 调用方应按「不改变现状」处理。
    Unknown,
    /// 明确错误。
    Error,
}

impl ProbeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProbeStatus::Ok => "ok",
            ProbeStatus::Expired => "expired",
            ProbeStatus::Unknown => "unknown",
            ProbeStatus::Error => "error",
        }
    }
}

/// 探活结果详情。
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub status: ProbeStatus,
    pub detail: String,
    /// 凭证来源（`local_state_multi_sids` / `cookie_decrypt` / `pool`）。
    pub source: String,
    /// 服务端可能下发的新凭证。
    pub new_session_id: Option<String>,
    pub new_sid_guard: Option<String>,
}

/// 构造直连 HTTP 客户端。
///
/// `no_proxy()` 是关键：本地 MITM 代理未启动时会是一个死端口，
/// 走系统代理会让所有探活请求失败，表现为「所有账号突然全部失效」。
fn agent() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .no_proxy()
        // 不自动跟随重定向：302 → passport 正是「会话过期」的判据
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("HTTP 客户端创建失败: {e}"))
}

/// 构造保活请求头。
fn probe_headers() -> Vec<(&'static str, String)> {
    vec![
        (
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AI Gateway/1.0".to_string(),
        ),
        ("Referer", "https://www.doubao.com/".to_string()),
        ("Accept", "application/json, text/plain, */*".to_string()),
        ("Content-Type", "application/json".to_string()),
    ]
}

/// 从响应头里提取目标 cookie（`Set-Cookie` → `name=value`）。
fn extract_set_cookies(resp: &reqwest::Response) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for value in resp.headers().get_all(reqwest::header::SET_COOKIE).iter() {
        let Ok(text) = value.to_str() else { continue };
        // 只取第一个 `name=value` 段（后面是 Path/Expires 等属性）
        let Some(pair) = text.split(';').next() else {
            continue;
        };
        let Some((name, val)) = pair.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if TARGET_COOKIES.contains(&name) {
            out.insert(name.to_string(), val.trim().to_string());
        }
    }
    out
}

/// 判断响应是否表示「会话已失效」。
fn is_expired_response(status: u16, resp: &reqwest::Response) -> bool {
    // 302/301 → passport 登录页
    if (300..400).contains(&status) {
        if let Some(location) = resp.headers().get(reqwest::header::LOCATION) {
            if let Ok(text) = location.to_str() {
                return text.contains("passport") || text.contains("login");
            }
        }
    }
    status == 401
}

/// 第一段：权威探活（POST 会员额度接口）。
pub async fn api_probe(sid: &str, probe_url: &str) -> ProbeResult {
    let base = ProbeResult {
        status: ProbeStatus::Unknown,
        detail: String::new(),
        source: "api_probe".to_string(),
        new_session_id: None,
        new_sid_guard: None,
    };
    let client = match agent() {
        Ok(c) => c,
        Err(e) => {
            return ProbeResult {
                detail: e,
                status: ProbeStatus::Error,
                ..base
            }
        }
    };
    let cookie = format!("sessionid={sid}; sessionid_ss={sid}");
    let mut req = client
        .post(probe_url)
        .body(r#"{"product_line":"membership"}"#)
        .header("Cookie", cookie);
    for (k, v) in probe_headers() {
        req = req.header(k, v);
    }

    match req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if is_expired_response(status, &resp) {
                return ProbeResult {
                    status: ProbeStatus::Expired,
                    detail: format!("HTTP {status} 跳转登录页"),
                    ..base
                };
            }
            let text = resp.text().await.unwrap_or_default();
            match serde_json::from_str::<Value>(&text) {
                Ok(body) => {
                    let code = body.get("code").and_then(Value::as_i64);
                    match code {
                        // code 缺失 / null / 0 都算成功
                        None | Some(0) => ProbeResult {
                            status: ProbeStatus::Ok,
                            detail: "会话有效".to_string(),
                            ..base
                        },
                        Some(SESSION_EXPIRED_CODE) => ProbeResult {
                            status: ProbeStatus::Expired,
                            detail: "服务端判定会话已失效".to_string(),
                            ..base
                        },
                        Some(c) => ProbeResult {
                            status: ProbeStatus::Unknown,
                            detail: format!("业务码 {c}"),
                            ..base
                        },
                    }
                }
                Err(_) => ProbeResult {
                    status: ProbeStatus::Unknown,
                    detail: format!("非 JSON 响应（HTTP {status}）"),
                    ..base
                },
            }
        }
        Err(e) => ProbeResult {
            status: ProbeStatus::Error,
            detail: e.to_string(),
            ..base
        },
    }
}

/// 第二段：保活探活（GET 轻量端点，回收服务端可能下发的新 cookie）。
pub async fn renew_probe(sid: &str, renew_url: &str) -> ProbeResult {
    let base = ProbeResult {
        status: ProbeStatus::Unknown,
        detail: String::new(),
        source: "renew_probe".to_string(),
        new_session_id: None,
        new_sid_guard: None,
    };
    let client = match agent() {
        Ok(c) => c,
        Err(e) => {
            return ProbeResult {
                detail: e,
                status: ProbeStatus::Error,
                ..base
            }
        }
    };
    let cookie = format!("sessionid={sid}; sessionid_ss={sid}");
    let mut req = client.get(renew_url).header("Cookie", cookie);
    for (k, v) in probe_headers() {
        req = req.header(k, v);
    }

    match req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if is_expired_response(status, &resp) {
                return ProbeResult {
                    status: ProbeStatus::Expired,
                    detail: format!("HTTP {status} 跳转登录页"),
                    ..base
                };
            }
            if !(200..300).contains(&status) {
                return ProbeResult {
                    status: ProbeStatus::Error,
                    detail: format!("HTTP {status}"),
                    ..base
                };
            }
            let cookies = extract_set_cookies(&resp);
            ProbeResult {
                status: ProbeStatus::Ok,
                detail: if cookies.is_empty() {
                    "保活成功（服务端未下发新凭证）".to_string()
                } else {
                    format!("保活成功（服务端下发 {} 个新 cookie）", cookies.len())
                },
                new_session_id: cookies.get("sessionid").cloned(),
                new_sid_guard: cookies.get("sid_guard").cloned(),
                ..base
            }
        }
        Err(e) => ProbeResult {
            status: ProbeStatus::Error,
            detail: e.to_string(),
            ..base
        },
    }
}

/// 解析 `sid_guard`，得到会话过期时间。
///
/// 格式：`<sid>|<create_ts>|<duration_secs>|...`（值可能被 URL 编码）。
pub fn parse_sid_guard(value: &str) -> Option<String> {
    let decoded = if value.contains('%') {
        urlencoding::decode(value)
            .map(|c| c.into_owned())
            .unwrap_or_else(|_| value.to_string())
    } else {
        value.to_string()
    };
    let parts: Vec<&str> = decoded.split('|').collect();
    if parts.len() < 3 {
        return None;
    }
    let create: i64 = parts[1].trim().parse().ok()?;
    let duration: i64 = parts[2].trim().parse().ok()?;
    if duration <= 0 {
        return None;
    }
    chrono::DateTime::from_timestamp(create + duration, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
}

/// 保活汇总。
#[derive(Debug, Clone, Default)]
pub struct RenewSummary {
    pub ok: usize,
    pub expired: usize,
    pub skipped: usize,
    pub errors: usize,
    pub results: Vec<Value>,
}

impl RenewSummary {
    pub fn to_json(&self) -> Value {
        json!({
            "ok": self.ok,
            "expired": self.expired,
            "skipped": self.skipped,
            "errors": self.errors,
            "total": self.results.len(),
            "results": self.results,
        })
    }
}

/// 对所有持有凭证的账号跑一轮 HTTP 续期探活。
///
/// `sync_only` 为真时只做诊断不发起请求。
pub async fn run_renewal(sync_only: bool) -> RenewSummary {
    let mut summary = RenewSummary::default();
    let renew_url = config::app_setting_str("doubao_renew_url")
        .unwrap_or_else(|| DEFAULT_RENEW_URL.to_string());
    let probe_url = config::app_setting_str("doubao_quota_url")
        .unwrap_or_else(|| DEFAULT_PROBE_URL.to_string());

    for acc in doubao_account::load_accounts() {
        let uid = acc.get("user_id").and_then(Value::as_str).unwrap_or("");
        let name = acc
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(uid)
            .to_string();
        let sid = acc
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if sid.is_empty() {
            summary.skipped += 1;
            summary.results.push(json!({
                "userId": uid, "name": name, "status": "skip",
                "message": "未配置 sessionid",
            }));
            continue;
        }
        if sync_only {
            summary.skipped += 1;
            summary.results.push(json!({
                "userId": uid, "name": name, "status": "skip",
                "message": "仅诊断模式，未发起请求",
            }));
            continue;
        }

        // 第一段：权威探活
        let first = api_probe(&sid, &probe_url).await;
        match first.status {
            ProbeStatus::Expired => {
                summary.expired += 1;
                let _ = doubao_account::mutate_account(uid, |obj| {
                    obj.insert("expired".to_string(), json!(true));
                });
                summary.results.push(json!({
                    "userId": uid, "name": name, "status": "expired",
                    "message": "会话已失效，请重新登录豆包并抓包更新凭证",
                }));
                continue;
            }
            ProbeStatus::Ok => {}
            ProbeStatus::Unknown | ProbeStatus::Error => {
                summary.errors += 1;
                summary.results.push(json!({
                    "userId": uid, "name": name, "status": "error",
                    "message": first.detail,
                }));
                continue;
            }
        }

        // 第二段：保活 + 回收新凭证
        let second = renew_probe(&sid, &renew_url).await;
        let now = config::utc_iso();
        match second.status {
            ProbeStatus::Ok => {
                summary.ok += 1;
                let new_sid = second.new_session_id.clone();
                let new_guard = second.new_sid_guard.clone();
                let _ = doubao_account::mutate_account(uid, |obj| {
                    obj.insert("expired".to_string(), json!(false));
                    obj.insert("last_renew_at".to_string(), json!(now));
                    // 服务端下发新凭证时必须写回：滑动续期正是通过换新 sid 完成的，
                    // 不写回就等于续期白做（下次探活还是用旧的、更快过期的凭证）。
                    if let Some(sid) = new_sid {
                        obj.insert("session_id".to_string(), json!(sid));
                    }
                    if let Some(guard) = new_guard {
                        if let Some(expire) = parse_sid_guard(&guard) {
                            obj.insert("session_expire_at".to_string(), json!(expire));
                        }
                        obj.insert("sid_guard".to_string(), json!(guard));
                    }
                });
                summary.results.push(json!({
                    "userId": uid, "name": name, "status": "ok",
                    "message": second.detail,
                }));
            }
            ProbeStatus::Expired => {
                summary.expired += 1;
                let _ = doubao_account::mutate_account(uid, |obj| {
                    obj.insert("expired".to_string(), json!(true));
                });
                summary.results.push(json!({
                    "userId": uid, "name": name, "status": "expired",
                    "message": "保活时发现会话失效",
                }));
            }
            ProbeStatus::Unknown | ProbeStatus::Error => {
                summary.errors += 1;
                summary.results.push(json!({
                    "userId": uid, "name": name, "status": "error",
                    "message": second.detail,
                }));
            }
        }
        // 账号间隔，避免触发风控
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    summary
}

/// 诊断：检查各账号的凭证来源与可读性。
///
/// 存在的意义：用户常问「为什么我的 sessionid 读不出来」。实测结论是
/// 客户端做了第二层加密，离线拿不到明文 —— 这个诊断把该结论直接摆给用户看，
/// 而不是让他反复重试。
pub fn diagnose() -> Value {
    let accounts = doubao_account::load_accounts();
    let captured = doubao_account::load_captured();
    let accounts_dir = crate::modules::app_profile::profile_for(
        crate::modules::app_profile::TargetApp::Doubao,
        &config::store_dir(),
    )
    .data_dir;
    // 抓包文件里 `multi_sids` 解析出多少个账号 —— 同机多账号时它能一次补全多个
    let captured_sids = captured
        .as_ref()
        .and_then(|c| c.get("sids"))
        .and_then(Value::as_object)
        .map(|m| m.len())
        .unwrap_or(0);
    let (missing_credentials, inferable) = doubao_account::credential_inference_state();

    json!({
        "accountsTotal": accounts.len(),
        "accountsWithSession": accounts.iter().filter(|a| {
            a.get("session_id").and_then(Value::as_str).map(|s| !s.trim().is_empty()).unwrap_or(false)
        }).count(),
        // 缺凭证的账号数，以及能否靠「唯一候选」自动补上
        "accountsMissingCredential": missing_credentials,
        "uidInferenceAvailable": inferable,
        "capturedAvailable": captured.is_some(),
        "capturedUid": captured.as_ref().and_then(|c| c.get("uid")).cloned(),
        "capturedSidsCount": captured_sids,
        "capturedAt": captured.as_ref().and_then(|c| c.get("captured_at")).cloned(),
        "userDataDir": accounts_dir.to_string_lossy(),
        "userDataExists": accounts_dir.is_dir(),
        "note": "豆包客户端在 Chromium v10 之外还有一层客户端级加密，离线无法解出明文 sessionid。\
保活依靠启动客户端触发服务端滑动续期；凭证可通过本地代理抓包自动获取。\
同机多账号的凭证在客户端登录/使用时会随 multi_sids 一并抓取并各自补全。",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析_sid_guard_得到过期时间() {
        // 2026-01-01 00:00:00 UTC 起，有效 30 天
        let create = 1_767_225_600i64;
        let guard = format!("abc|{create}|2592000|other");
        let expire = parse_sid_guard(&guard).expect("应能解析");
        let expected = chrono::DateTime::from_timestamp(create + 2_592_000, 0)
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        assert_eq!(expire, expected);
    }

    #[test]
    fn 解析_sid_guard_处理_url_编码() {
        let create = 1_767_225_600i64;
        let raw = format!("abc|{create}|2592000");
        let encoded: String = urlencoding::encode(&raw).into_owned();
        assert!(encoded.contains('%'), "编码后应含 % 以覆盖解码分支");
        assert!(parse_sid_guard(&encoded).is_some());
    }

    #[test]
    fn 解析_sid_guard_对畸形输入返回_none() {
        assert_eq!(parse_sid_guard(""), None);
        assert_eq!(parse_sid_guard("abc"), None);
        assert_eq!(parse_sid_guard("abc|123"), None, "不足 3 段");
        assert_eq!(parse_sid_guard("abc|notanumber|100"), None);
        assert_eq!(parse_sid_guard("abc|123|notanumber"), None);
        assert_eq!(parse_sid_guard("abc|123|0"), None, "零时长无效");
        assert_eq!(parse_sid_guard("abc|123|-5"), None, "负时长无效");
    }

    #[test]
    fn 会话失效码常量与上游一致() {
        assert_eq!(SESSION_EXPIRED_CODE, 710012001);
    }

    #[test]
    fn 保活与探活端点默认值() {
        assert_eq!(DEFAULT_RENEW_URL, "https://www.doubao.com/info/v2/");
        assert!(DEFAULT_PROBE_URL.contains("quota/summary"));
    }

    #[test]
    fn 诊断不泄露凭证明文() {
        use crate::modules::config::test_isolation::Isolated;
        let _iso = Isolated::new("diag");
        let _ = doubao_account::upsert_account("123456", Some("测试"), None, true);
        let _ = doubao_account::apply_credential(
            "123456",
            Some("secret-session-value"),
            None,
            None,
            "manual",
            true,
        );
        let diag = diagnose();
        let text = serde_json::to_string(&diag).unwrap();
        assert!(!text.contains("secret-session-value"), "诊断输出不得含凭证明文");
        assert_eq!(diag["accountsTotal"], 1);
        assert_eq!(diag["accountsWithSession"], 1);
    }

    #[test]
    fn 汇总_json_形态() {
        let mut s = RenewSummary::default();
        s.ok = 1;
        s.results.push(json!({"userId": "u", "status": "ok"}));
        let j = s.to_json();
        assert_eq!(j["ok"], 1);
        assert_eq!(j["total"], 1);
    }
}
