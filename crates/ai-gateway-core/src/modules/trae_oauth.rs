//! Trae OAuth 授权码登录（F-78，对标参考实现 `commands/oauth.rs` + `oauth_loopback.rs`）。
//!
//! 与「粘贴 JWT」的关系：OAuth 是**获得** JWT 的正规途径，粘贴只是兜底。
//! 一条完整的登录链路是：
//!
//! 1. [`login_url`] 签发授权 URL（带 `state` 与 PKCE challenge）；
//! 2. 用户在浏览器完成授权，授权页 302 回 `redirect_uri`；
//! 3. 回调地址被本机回环监听器（Tauri 宿主实现）捕获，原文交给
//!    [`parse_callback`] 解析出 AuthCode；
//! 4. [`exchange_auth_code`] 用 PKCE verifier 换 access_token / refresh_token；
//! 5. [`fetch_user_info`] 取昵称 → 落库（`trae_account::upsert_account`）。
//!
//! **本模块只做纯逻辑与 HTTP，不监听端口** —— 监听器需要 axum + Tauri 事件，
//! 属于宿主职责；core 保持无 Tauri 依赖（见 `AGENTS.md` 架构约定）。
//!
//! 安全要点（参考实现踩过的坑，逐条保留）：
//!
//! - **CSRF**：回调必须带签发时记录的 `state`，否则拒绝 —— 否则攻击者可以把
//!   自己的授权码回调给受害者的应用，让受害者账号被替换成攻击者的。
//! - **PKCE**：`code_verifier` 只存在于本进程内存，回调换 token 时携带。
//! - **HTML 转义**：回调结果页会把上游错误原文与用户可控的 URL 插进 HTML，
//!   必须实体转义，否则错误信息里的 `<script>` 会被浏览器执行。

use std::sync::Mutex;

use serde_json::{json, Value};

use super::config;

/// 真实 Trae IDE 登录 URL 的实证 client_id。
///
/// 旧值 `en1oxy7wnw8j9n` 会让授权页停在 billing status 后不回跳（参考实现抓包结论）。
pub const OAUTH_CLIENT_ID: &str = "ono9krqynydwx5";
/// 授权页据此进入 `native_ide` 原生授权流程（前端调 GetPCAuthCode 后 302 回回调地址）。
pub const OAUTH_PAGE_PLUGIN_VERSION: &str = "2.3.83560";
pub const OAUTH_PAGE_APP_VERSION: &str = "3.3.100";
pub const OAUTH_PAGE_PLATFORM_CODE: &str = "IDE_PC";
/// 云 IDE API 附带的 `X-App-Id`。
pub const OAUTH_APP_ID: &str = "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8";
/// 本机回环监听端口与回调地址（须与授权 URL 中的 `redirect_uri` 完全一致）。
pub const OAUTH_LOOPBACK_PORT: u16 = 17388;
pub const OAUTH_REDIRECT_URI: &str = "http://127.0.0.1:17388/authorize";
/// 授权页地址。
pub const OAUTH_AUTHORIZE_URL: &str = "https://www.trae.com.cn/authorize";
/// AuthCode 换 token。
pub const OAUTH_EXCHANGE_URL: &str =
    "https://api.trae.com.cn/cloudide/api/v3/trae/oauth/ExchangeToken";
/// 取用户信息。
pub const OAUTH_USER_INFO_URL: &str = "https://api.trae.com.cn/cloudide/api/v3/trae/user/info";

/// 最近签发的一次登录会话（CSRF + PKCE）。
#[derive(Debug, Clone)]
pub struct PendingLogin {
    /// 签发时生成的 `state`（同时作为 `loginTraceID` 回传）。
    pub state: String,
    /// PKCE `code_verifier`（只在本进程内存里）。
    pub pkce_verifier: String,
}

static PENDING: Mutex<Option<PendingLogin>> = Mutex::new(None);

/// 生成 `len` 个十六进制字符的随机串。
///
/// 用 `rand::rngs::OsRng`（OS CSPRNG，Windows 上落到 BCryptGenRandom）而不是
/// 时间戳/LCG：`state` 与 PKCE verifier 的不可预测性是这两道防护的全部意义 ——
/// 可预测的随机数等于没有防护。
fn random_hex(len: usize) -> String {
    use rand::RngCore as _;
    let mut bytes = vec![0u8; len.div_ceil(2)];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(len);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out.truncate(len);
    out
}

/// 生成 PKCE 的 `code_verifier` / `code_challenge`（S256）。
///
/// `code_challenge = BASE64URL-NOPAD(SHA256(verifier))`（RFC 7636 §4.2）。
fn pkce_pair() -> (String, String) {
    use base64::Engine as _;
    use sha2::Digest as _;
    let verifier = random_hex(64);
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    (verifier, challenge)
}

/// 签发授权 URL，并记录本次登录会话（供回调校验）。
///
/// 每次调用都会覆盖上一次未完成的会话 —— 用户重开登录弹窗时，旧会话就该作废。
pub fn login_url(account_name: Option<&str>) -> Result<Value, String> {
    let (verifier, challenge) = pkce_pair();
    build_login_url(account_name, &challenge, verifier)
}

/// 构造授权 URL 并登记 pending 会话。
fn build_login_url(
    account_name: Option<&str>,
    challenge: &str,
    verifier: String,
) -> Result<Value, String> {
    let state = random_hex(32);
    if state.is_empty() || verifier.is_empty() {
        return Err("无法获取系统随机数，OAuth 登录不可用（请检查系统熵源）".to_string());
    }
    if challenge.is_empty() {
        return Err("PKCE challenge 生成失败".to_string());
    }

    // 登录成功后的备注名：Trae 会把 loginTraceID 原样回传，昵称由调用方在落库时使用；
    // 这里把用户填的备注名编码进 state 旁路，避免再开一个全局槽位。
    // 只允许安全字符，防止备注名里的 & 破坏 URL。
    let name_param = account_name
        .map(|n| n.trim())
        .filter(|n| !n.is_empty())
        .map(|n| format!("&login_hint={}", urlencoding::encode(n)))
        .unwrap_or_default();

    let url = format!(
        "{OAUTH_AUTHORIZE_URL}?login_channel=native_ide\
         &plugin_version={OAUTH_PAGE_PLUGIN_VERSION}\
         &app_version={OAUTH_PAGE_APP_VERSION}\
         &platform_code={OAUTH_PAGE_PLATFORM_CODE}\
         &client_id={OAUTH_CLIENT_ID}\
         &redirect_uri={redirect}\
         &response_type=code\
         &scope=user_info%20cloudide\
         &state={state}\
         &loginTraceID={state}{name_param}",
        redirect = urlencoding::encode(OAUTH_REDIRECT_URI),
        state = state,
    );

    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(PendingLogin {
        state: state.clone(),
        pkce_verifier: verifier,
    });

    Ok(json!({
        "url": url,
        "state": state,
        "redirectUri": OAUTH_REDIRECT_URI,
        "port": OAUTH_LOOPBACK_PORT,
    }))
}

/// 解析 OAuth 回调地址，返回 AuthCode。
///
/// 支持三种形态（参考实现实测都出现过）：
/// 1. `?code=xxx`（标准授权码模式）；
/// 2. `?authCode=xxx`；
/// 3. 回调页把 code 包在 JSON 里回传 —— 此时取 `authCodeInfo.AuthCode`。
///
/// `state` 校验失败一律拒绝：这是 CSRF 防护的唯一关卡。
pub fn parse_callback(callback_url: &str) -> Result<String, String> {
    let params = parse_query(callback_url);

    // 先校验 state —— 即便后续解析失败，也不该让未校验的回调走到换 token
    let pending = PENDING.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(pending) = pending else {
        return Err("没有待处理的登录会话，请重新点击「OAuth 登录」发起".to_string());
    };
    let got_state = params
        .iter()
        .find(|(k, _)| k == "state" || k == "loginTraceID")
        .map(|(_, v)| v.clone());
    match got_state {
        Some(s) if s == pending.state => {}
        Some(s) => {
            return Err(format!(
                "回调 state 与本次登录会话不匹配（收到 {s}），已拒绝以防 CSRF；请重新发起登录"
            ))
        }
        None => return Err("回调地址缺少 state 参数，已拒绝以防 CSRF；请重新发起登录".to_string()),
    }

    if let Some(err) = params.iter().find(|(k, _)| k == "error").map(|(_, v)| v) {
        let desc = params
            .iter()
            .find(|(k, _)| k == "error_description")
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        return Err(if desc.is_empty() {
            format!("授权被拒绝：{err}")
        } else {
            format!("授权被拒绝：{err}（{desc}）")
        });
    }

    for key in ["code", "authCode", "AuthCode", "auth_code"] {
        if let Some((_, v)) = params.iter().find(|(k, _)| k == key) {
            if !v.trim().is_empty() {
                return Ok(v.clone());
            }
        }
    }

    // 兜底：回调页可能把结果以 JSON 形式塞在某个参数里
    for (_, v) in &params {
        if let Some(code) = extract_auth_code_from_json(v) {
            return Ok(code);
        }
    }

    Err("回调地址里找不到授权码（code），请确认复制的是浏览器地址栏中的完整 URL".to_string())
}

/// 从可能是 JSON 的字符串里挖出 AuthCode。
fn extract_auth_code_from_json(raw: &str) -> Option<String> {
    let decoded = urlencoding::decode(raw)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| raw.to_string());
    let value: Value = serde_json::from_str(decoded.trim()).ok()?;
    // 参考实现实测路径：{ "authCodeInfo": { "AuthCode": "..." } }
    for path in [
        &["authCodeInfo", "AuthCode"][..],
        &["authCodeInfo", "authCode"][..],
        &["data", "authCodeInfo", "AuthCode"][..],
        &["code"][..],
        &["authCode"][..],
    ] {
        let mut cur = &value;
        let mut ok = true;
        for key in path {
            match cur.get(*key) {
                Some(next) => cur = next,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            if let Some(s) = cur.as_str().filter(|s| !s.trim().is_empty()) {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// 极简 query 解析（含 URL 解码；同名参数保留全部）。
fn parse_query(url: &str) -> Vec<(String, String)> {
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or(url);
    let query = query.split('#').next().unwrap_or(query);
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let dec = |s: &str| {
                urlencoding::decode(s)
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| s.to_string())
            };
            (dec(k), dec(v))
        })
        .collect()
}

/// 用授权码换 token（带 PKCE verifier）。
///
/// 返回 `(access_token, refresh_token)`。`refresh_token` 是长期凭证 ——
/// 有了它才能自动续期，因此缺失也算失败（否则用户会得到一个几小时后就失效的账号）。
pub async fn exchange_auth_code(code: &str) -> Result<(String, Option<String>), String> {
    let verifier = PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|p| p.pkce_verifier.clone())
        .ok_or_else(|| "没有待处理的登录会话，请重新发起 OAuth 登录".to_string())?;

    let body = json!({
        "client_id": OAUTH_CLIENT_ID,
        "code": code,
        "grant_type": "authorization_code",
        "redirect_uri": OAUTH_REDIRECT_URI,
        "code_verifier": verifier,
    });
    let mut headers = std::collections::HashMap::new();
    headers.insert(
        "Content-Type".to_string(),
        "application/json".to_string(),
    );
    headers.insert("X-App-Id".to_string(), OAUTH_APP_ID.to_string());

    // OAuth 交换必须**直连**：本地 MITM 代理解密浏览器 OAuth 流量会让授权页
    // 报 ERR_CERT_AUTHORITY_INVALID（参考实现为此显式加了代理豁免）。
    let resp = config::http_request_direct(OAUTH_EXCHANGE_URL, "POST", Some(body), Some(&headers)).await;
    parse_exchange(OAUTH_EXCHANGE_URL, &resp)
}

/// 从 ExchangeToken 响应里取 access / refresh token。
fn parse_exchange(url: &str, resp: &Value) -> Result<(String, Option<String>), String> {
    let code = resp.get("code").and_then(Value::as_i64).unwrap_or(0);
    let data = resp.get("data").cloned().unwrap_or(Value::Null);
    let biz = data
        .get("code")
        .and_then(Value::as_i64)
        .or_else(|| data.get("Code").and_then(Value::as_i64));

    if code == 401 || code == 403 {
        return Err(format!(
            "ExchangeToken 被服务端拒绝（HTTP {code}）—— client_id 或授权码已失效（{url}）"
        ));
    }
    if let Some(b) = biz.filter(|b| *b != 0) {
        let msg = data
            .get("message")
            .or_else(|| data.get("Message"))
            .and_then(Value::as_str)
            .unwrap_or("");
        return Err(format!("ExchangeToken 业务失败（code={b}）{msg}"));
    }

    let haystack = data.get("result").unwrap_or(&data);
    let access = ["access_token", "accessToken", "AccessToken", "token"]
        .iter()
        .find_map(|k| haystack.get(*k).and_then(Value::as_str))
        .or_else(|| extract_token_from_meta(haystack))
        .ok_or_else(|| {
            let mut clone = resp.clone();
            mask_sensitive(&mut clone);
            format!("ExchangeToken 响应里没有 access_token：{clone}")
        })?;

    let refresh = ["refresh_token", "refreshToken", "RefreshToken"]
        .iter()
        .find_map(|k| haystack.get(*k).and_then(Value::as_str))
        .map(str::to_string);

    Ok((access.to_string(), refresh))
}

/// 部分响应把 token 包在 `ResponseMetadata` 里（参考实现对照 GetPCAuthCode 形态）。
fn extract_token_from_meta(v: &Value) -> Option<&str> {
    for key in ["ResponseMetadata", "responseMetadata"] {
        if let Some(meta) = v.get(key) {
            for tk in ["AccessToken", "accessToken", "Token"] {
                if let Some(s) = meta.get(tk).and_then(Value::as_str) {
                    return Some(s);
                }
            }
        }
    }
    None
}

/// 递归把疑似 token 的字段值打码（错误信息里绝不能带出明文凭证）。
pub fn mask_sensitive(v: &mut Value) {
    const SENSITIVE: [&str; 6] = [
        "access_token",
        "accessToken",
        "AccessToken",
        "refresh_token",
        "refreshToken",
        "RefreshToken",
    ];
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if SENSITIVE.iter().any(|s| k.eq_ignore_ascii_case(s)) {
                    if val.is_string() {
                        *val = json!("<已脱敏>");
                        continue;
                    }
                }
                mask_sensitive(val);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(mask_sensitive),
        _ => {}
    }
}

/// 用 access_token 取账号昵称；失败返回 `None`（昵称不是登录的必要条件）。
pub async fn fetch_user_info(access_token: &str) -> Option<(Option<String>, Option<String>)> {
    let mut headers = std::collections::HashMap::new();
    headers.insert(
        "Authorization".to_string(),
        format!("Bearer {access_token}"),
    );
    headers.insert("X-App-Id".to_string(), OAUTH_APP_ID.to_string());
    let resp =
        config::http_request_direct(OAUTH_USER_INFO_URL, "GET", None, Some(&headers)).await;
    let data = resp.get("data")?;
    let body = data.get("result").unwrap_or(data);
    let name = ["name", "userName", "nick_name", "nickName"]
        .iter()
        .find_map(|k| body.get(*k).and_then(Value::as_str))
        .map(str::to_string);
    let uid = ["id", "userId", "user_id"]
        .iter()
        .find_map(|k| body.get(*k).and_then(Value::as_str))
        .map(str::to_string);
    if name.is_none() && uid.is_none() {
        return None;
    }
    Some((name, uid))
}

/// 登录会话是否仍在等待回调（界面据此决定是否显示「等待浏览器授权」）。
pub fn has_pending() -> bool {
    PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}

/// 丢弃当前登录会话（弹窗关闭 / 登录结束后调用）。
pub fn clear_pending() {
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// 回调结果页 HTML（成功 / 失败两个形态）。
///
/// **必须转义**：`message` 含上游错误原文，`callback_url` 含浏览器可控的
/// path/query，直接插值会让回调页变成 XSS 载体（参考实现 P0 缺陷 6 的回归点）。
pub fn callback_page(ok: bool, message: &str, callback_url: &str) -> String {
    fn esc(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#39;")
    }
    let (icon, title, color) = if ok {
        ("✓", "登录成功", "#16a34a")
    } else {
        ("✕", "登录失败", "#dc2626")
    };
    let body = if ok {
        format!(
            "<p>{}</p><p>账号已加入账号库，可关闭此页面返回应用。</p>",
            esc(message)
        )
    } else {
        format!(
            "<p>{}</p>\
             <p>可改用手动兜底：复制浏览器<b>地址栏中的完整 URL</b>，\
             回到应用的 OAuth 登录窗口粘贴提交。</p>\
             <p class=\"url\">{}</p>",
            esc(message),
            esc(callback_url)
        )
    };
    format!(
        "<!DOCTYPE html>\
         <html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Trae 账号登录</title>\
         <style>\
           body {{ font-family: system-ui, -apple-system, 'Segoe UI', 'Microsoft YaHei', sans-serif; \
                  display: flex; align-items: center; justify-content: center; min-height: 100vh; \
                  margin: 0; background: #f6f7f9; }}\
           .card {{ background: #fff; border-radius: 12px; padding: 40px 48px; max-width: 560px; \
                   box-shadow: 0 4px 16px rgba(0,0,0,.08); text-align: center; }}\
           .icon {{ width: 56px; height: 56px; border-radius: 50%; color: #fff; font-size: 28px; \
                    line-height: 56px; margin: 0 auto 16px; background: {color}; }}\
           h1 {{ font-size: 20px; margin: 0 0 12px; }}\
           p {{ color: #555; line-height: 1.7; margin: 6px 0; }}\
           .url {{ word-break: break-all; background: #f1f5f9; border-radius: 6px; padding: 8px 12px; \
                  font-size: 12px; color: #334155; text-align: left; }}\
         </style></head>\
         <body><div class=\"card\">\
           <div class=\"icon\">{icon}</div>\
           <h1>{title}</h1>\
           {body}\
         </div></body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用：登记一个固定 state 的登录会话。
    ///
    /// `PENDING` 是进程级全局槽位，而测试默认并行 —— 不串行化的话
    /// 「A 登记 state → B 登记 state → A 断言」会互相踩掉。
    /// 借用 `config::test_isolation` 的全局锁而不是模块内自建锁：
    /// 那个锁还保护环境变量等其它共享状态，自建锁解决不了跨模块干扰。
    fn seed(state: &str) -> config::test_isolation::Isolated {
        let iso = config::test_isolation::Isolated::new("trae_oauth");
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(PendingLogin {
            state: state.to_string(),
            pkce_verifier: "v".repeat(64),
        });
        iso
    }

    #[test]
    fn 授权_url_带齐_native_ide_参数() {
        let out = login_url(Some("我的主号")).expect("应能签发");
        let url = out["url"].as_str().unwrap();
        assert!(url.starts_with(OAUTH_AUTHORIZE_URL));
        assert!(url.contains("login_channel=native_ide"));
        assert!(url.contains(&format!("client_id={OAUTH_CLIENT_ID}")));
        assert!(url.contains("code_challenge=") || url.contains("loginTraceID="));
        assert!(url.contains("platform_code=IDE_PC"));
        // redirect_uri 必须与回环监听地址完全一致，否则浏览器回调落不到我们端口
        assert!(url.contains(&urlencoding::encode(OAUTH_REDIRECT_URI).into_owned()));
        assert_eq!(out["port"], json!(OAUTH_LOOPBACK_PORT as i64));
        assert_eq!(out["redirectUri"], json!(OAUTH_REDIRECT_URI));
    }

    #[test]
    fn 备注名里的特殊字符不会破坏_url() {
        // 备注名带 & 和空格时必须被编码，否则会多出伪造的 query 参数
        let out = login_url(Some("A&B = C")).expect("应能签发");
        let url = out["url"].as_str().unwrap();
        assert!(!url.contains("&login_hint=A&B"), "备注名未被编码：{url}");
        assert!(url.contains("login_hint=A%26B"), "备注名应被 URL 编码：{url}");
    }

    #[test]
    fn 回调_state_不匹配必须拒绝() {
        let _iso = seed("expected-state");
        let err = parse_callback("http://127.0.0.1:17388/authorize?code=abc&state=attacker")
            .expect_err("state 不匹配必须报错");
        assert!(err.contains("CSRF"), "错误信息应点明 CSRF 风险：{err}");
        // 缺 state 同样拒绝
        assert!(parse_callback("http://127.0.0.1:17388/authorize?code=abc").is_err());
    }

    #[test]
    fn 回调解析认得三种授权码形态() {
        let _iso = seed("s1");
        assert_eq!(
            parse_callback("http://127.0.0.1:17388/authorize?code=CODE1&state=s1").unwrap(),
            "CODE1"
        );

        // JSON 包裹形态：回调页把结果塞进参数里（沿用同一个 state）
        let json = urlencoding::encode(r#"{"authCodeInfo":{"AuthCode":"CODE3"}}"#);
        let url = format!("http://127.0.0.1:17388/authorize?payload={json}&state=s1");
        assert_eq!(parse_callback(&url).unwrap(), "CODE3");

        // authCode 形态
        assert_eq!(
            parse_callback("http://127.0.0.1:17388/authorize?authCode=CODE2&state=s1").unwrap(),
            "CODE2"
        );
    }

    #[test]
    fn 回调带_loginTraceID_也能过_state_校验() {
        let _iso = seed("trace-1");
        // 授权页把 state 原样回传为 loginTraceID
        assert_eq!(
            parse_callback("http://127.0.0.1:17388/authorize?code=C&loginTraceID=trace-1").unwrap(),
            "C"
        );
    }

    #[test]
    fn 回调里的授权拒绝要如实传达() {
        let _iso = seed("s");
        let err = parse_callback(
            "http://127.0.0.1:17388/authorize?error=access_denied&error_description=user%20said%20no&state=s",
        )
        .expect_err("应报错");
        assert!(err.contains("access_denied"));
        assert!(err.contains("user said no"), "错误描述应被解码：{err}");
    }

    #[test]
    fn 没有待处理会话时回调一律拒绝() {
        let _iso = config::test_isolation::Isolated::new("trae_oauth");
        clear_pending();
        let err = parse_callback("http://127.0.0.1:17388/authorize?code=x&state=anything")
            .expect_err("无会话必须报错");
        assert!(err.contains("重新"), "应提示重新发起：{err}");
    }

    #[test]
    fn 换_token_拒绝_401_与业务错误() {
        let rejected = parse_exchange("u", &json!({ "code": 401 }));
        let err = rejected.expect_err("401 必须报错");
        assert!(err.contains("被服务端拒绝"), "{err}");

        let biz = parse_exchange("u", &json!({ "code": 0, "data": { "code": 1001, "message": "bad" } }));
        assert!(biz.is_err());

        // 正常响应
        let ok = parse_exchange(
            "u",
            &json!({ "code": 0, "data": { "result": { "access_token": "AT", "refresh_token": "RT" } } }),
        )
        .expect("应成功");
        assert_eq!(ok.0, "AT");
        assert_eq!(ok.1.as_deref(), Some("RT"));
    }

    #[test]
    fn 换_token_缺_access_token_时报错带脱敏响应() {
        let err = parse_exchange(
            "u",
            &json!({ "code": 0, "data": { "refresh_token": "SECRET-RT" } }),
        )
        .expect_err("缺 access_token 应报错");
        // 错误信息里绝不能出现明文 refresh_token
        assert!(!err.contains("SECRET-RT"), "错误信息泄露了 refresh_token：{err}");
        assert!(err.contains("已脱敏"), "{err}");
    }

    #[test]
    fn 脱敏覆盖嵌套结构() {
        let mut v = json!({
            "a": { "b": [{ "AccessToken": "LEAK1" }] },
            "refresh_token": "LEAK2",
            "safe": "keep"
        });
        mask_sensitive(&mut v);
        let s = v.to_string();
        assert!(!s.contains("LEAK1"), "{s}");
        assert!(!s.contains("LEAK2"), "{s}");
        assert!(s.contains("keep"), "非敏感字段不该被改动：{s}");
    }

    #[test]
    fn 回调结果页转义脚本注入() {
        // 参考实现 P0 缺陷 6 的回归点
        let html = callback_page(
            false,
            "兑换失败: <script>alert(1)</script>",
            "http://127.0.0.1:17388/authorize?code=ab&state=<img src=x onerror=alert(2)>",
        );
        assert!(!html.contains("<script>"), "脚本标签未被转义");
        assert!(!html.contains("<img"), "img 标签未被转义");
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("&lt;img src=x onerror=alert(2)&gt;"));
        assert!(html.contains("登录失败"));
        assert!(html.contains("code=ab&amp;state="), "& 应被转义为 &amp;");
    }

    #[test]
    fn 成功结果页展示账号名() {
        let html = callback_page(true, "账号 [测试] 登录成功", "");
        assert!(html.contains("登录成功"));
        assert!(html.contains("账号 [测试] 登录成功"));
    }

    #[test]
    fn query_解析保留重复参数并解码() {
        let params = parse_query("http://x/y?a=1&a=2&b=%E4%B8%AD%E6%96%87#frag");
        assert_eq!(params.iter().filter(|(k, _)| k == "a").count(), 2);
        assert!(params.iter().any(|(k, v)| k == "b" && v == "中文"));
        // #fragment 不该被当成 query 的一部分
        assert!(!params.iter().any(|(k, _)| k.contains("frag")));
    }

    #[test]
    fn 随机串长度与字符集正确() {
        let a = random_hex(32);
        let b = random_hex(32);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        // 两次结果不同（32 字符 hex 相撞的概率可忽略）
        assert_ne!(a, b);
        // 奇数长度也要精确截断到请求的长度
        assert_eq!(random_hex(5).len(), 5);
        assert_eq!(random_hex(1).len(), 1);
    }
}
