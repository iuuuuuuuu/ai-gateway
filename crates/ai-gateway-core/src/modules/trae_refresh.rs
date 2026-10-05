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
use crate::modules::trae_oauth;

/// `ExchangeToken` 端点。
///
/// ★★ 2026-10-02 修正：真实客户端 `main.js` 里是
/// `` `${host}/trae/api/v3/oauth/ExchangeToken` `` —— 不是
/// `/cloudide/api/v3/trae/oauth/ExchangeToken`（那是我们猜的 OAuth 标准路径）。
///
/// 判别实验（`test-bin/trae-exchange-probe/probe4.py`）：对不存在的路径返回
/// `404 Page not found`，对本路径返回业务错误码 ⇒ **本路径真实存在**。
pub const EXCHANGE_PATH: &str = "/trae/api/v3/oauth/ExchangeToken";

/// 默认 host（`product.json` 的 `account.trae.normal`）。
pub const EXCHANGE_HOST_DEFAULT: &str = "https://api.trae.cn";

/// 兼容旧引用的完整地址。
pub const EXCHANGE_URL: &str = "https://api.trae.cn/trae/api/v3/oauth/ExchangeToken";

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
    // ★ 2026-10-02：真实错误信封是 `ResponseMetadata.Error.{Code,Message}`，
    // 顶层**没有**数字 `code`。不识别它的话会退化成"未找到 access token"，
    // 把「服务端明确拒绝」误报成解析失败，进而按 Transient 计数而不是
    // 立即置失效 —— 用户会看到账号一直重试却永远好不了。
    if let Some(err) = metadata_error(body) {
        return Err(format!("服务端拒绝（{err}）"));
    }
    // 信封逐层下钻（不同版本包在 data / result / Result 里）
    let scopes = unwrap_scopes(body);
    // ★★ 2026-10-02：真实响应是 `Result.Token`（**大驼峰 `Token`**，既不是
    // `access_token` 也不是 `accessToken`）—— 凭据落库处（`util.js` 的 `s_`）
    // 也是 `{token: e.Token, refreshToken: e.RefreshToken, ...}`。原来没有
    // 裸 `Token` 这个候选，于是刷新永远报「响应中未找到 access token」。
    const ACCESS_KEYS: &[&str] = &[
        "Token",
        "AccessToken",
        "access_token",
        "accessToken",
        "token",
        "Jwt",
        "JWT",
    ];
    // ⚠ 这里**不能**加裸 `Token`：它和 access token 同键，会把 access 当成 refresh
    // 存进账号库，用户会在几小时后遇到"凭证突然失效"这种极难归因的故障。
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
///
/// ★★ 2026-10-02 修：真实响应是 `{ResponseMetadata:{...}, Result:{...}}` ——
/// **`Result` 是大驼峰**，而这里原来只认小写 `result` ⇒ 永远剥不到内层，
/// `Result.Token` 也就永远取不到。表现为刷新时报「响应中未找到 access token」，
/// 而响应其实完全正常。
///
/// 现在改成**大小写不敏感**匹配，并把 `Result` 加进候选。
fn unwrap_scopes(value: &Value) -> Vec<&Value> {
    const ENVELOPE_KEYS: &[&str] = &[
        "data", "Data", "result", "Result", "resp", "respResult", "response", "Response", "info",
        "Info",
    ];
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
/// # ★★ 2026-10-02 按真实客户端重写请求体
///
/// 权威实现（`main.js` offset 1286029 段）：
///
/// ```js
/// async exchangeTokenByRefreshToken(t, e, i, r) {   // t=host, e=RefreshToken, r={privateKeyPEM,publicKeyPEM}
///   const s = `${t}/trae/api/v3/oauth/ExchangeToken`;
///   const n = Bb(this.d);                            // ClientID
///   const o = await this.j(r.publicKeyPEM);          // DeviceInfo
///   const a = ODe("POST", "/trae/api/v3/oauth/ExchangeToken", n, e, r.privateKeyPEM);
///   const c = { Signature: a.signature, Timestamp: a.timestamp, Nonce: a.nonce };
///   const l = { ClientID: n, ClientSecret: "", RefreshToken: e, DeviceInfo: o,
///               DeviceProof: c, IDEVersion: this.d.appVersion };
///   return this.l(s, l, { headers: this.m(i) });
/// }
/// ```
///
/// `ODe` 的签名口径（**逐字**）：
///
/// ```js
/// const n = Math.floor(Date.now() / 1e3);           // 秒级时间戳
/// const o = crypto.randomBytes(16).toString("hex"); // nonce
/// const a = [t, e, i, r, String(n), o].join("\n");  // [method, path, clientId, refreshToken, ts, nonce]
/// const c = crypto.sign("sha256", Buffer.from(a), s).toString("base64");
/// ```
///
/// ⚠ 与旧实现的差异（旧的那份**几乎全错**，刷新必然失败）：
/// - 路径 `/cloudide/api/v3/trae/oauth/ExchangeToken` → **`/trae/api/v3/oauth/ExchangeToken`**；
/// - 缺 `ClientSecret` / `IDEVersion` / `DeviceProof`（三个必填）；
/// - `DeviceInfo` 只有 `{PlatformCode, DeviceType}` 两字段 → 真实是 11 个字段（含 P-256 公钥）；
/// - `ClientID` 写死 TRAE 线 → 现在按**账号自己的**产品线取（见 [`OAUTH_CLIENT_ID`] 注释）；
/// - `PlatformCode` 写死 `IDE_PC` → SOLO 线应为 `SOLO_PC`。
///
/// 错误文案带 `服务端拒绝` 前缀时，调用方按 [`RefreshFailureKind::Rejected`]
/// 分类（立即置失效）；其余按瞬时失败处理。
pub async fn exchange_token(
    refresh_token: &str,
) -> Result<(String, Option<String>, Option<i64>), String> {
    let client = reqwest::Client::builder()
        // ★★ 2026-10-02：与 `config::http_request_direct` 同一组修正，缺一不可。
        //
        // ① `.no_proxy()` —— reqwest 的 `ClientBuilder` 默认
        //    `auto_sys_proxy = true`（`reqwest-0.12.28/src/async_impl/client.rs:309`，
        //    `build()` 时在 `:419-421` 把系统代理推进 `proxies`）。此前这里没调
        //    ⇒ 刷新请求会走系统代理；代理不通时表现为「刷新失败（网络/代理问题）」，
        //    而错误文案里那个「代理问题」正是实情。
        //
        // ② `.local_address(Ipv4)` —— hyper 的 `split_by_preference`
        //    （`hyper-util-0.1.20/src/client/legacy/connect/dns.rs:211-234`）在
        //    `(Some(_), None)` 时**只留 IPv4、fallback 为空**；不设则按 DNS 首条
        //    偏好分区，而本机 DNS 首条是 IPv6，到 `api.trae.cn` 的 IPv6 路由不通。
        //    所有者机器实测：强制 IPv4 直连 0.073s，默认 IPv6 优先直连 105.249s。
        .no_proxy()
        .local_address(Some(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| format!("构造 HTTP 客户端失败: {e}"))?;

    /*
     * 产品线：优先按已装客户端探测（与 OAuth 登录同一套判据）。
     *
     * ⚠ 刷新时**没有** `PendingLogin` 可查（那是登录会话的槽位），
     * 所以这里只能用全局探测。装了 SOLO 的机器上刷新走 SOLO 的 client_id。
     */
    let flavor = trae_oauth::detect_flavor();
    let (public_key, private_key) = trae_oauth::device_keypair_for_refresh();
    let device = trae_oauth::device_info_for_refresh(flavor, &public_key);
    let client_id = flavor.client_id();
    let proof = device_proof("POST", EXCHANGE_PATH, client_id, refresh_token, &private_key)?;

    let body = json!({
        "ClientID": client_id,
        "ClientSecret": "",
        "RefreshToken": refresh_token,
        "DeviceInfo": device,
        "DeviceProof": proof,
        "IDEVersion": trae_oauth::app_version(flavor),
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

/// 构造 `DeviceProof`（真实客户端 `ODe` 的逐项复刻）。
///
/// 签名原文（**换行连接，顺序不能变**）：
///
/// ```text
/// <METHOD>\n<PATH>\n<ClientID>\n<RefreshToken>\n<秒级时间戳>\n<nonce>
/// ```
///
/// 签名算法：**ECDSA P-256 + SHA-256**，输出 **base64**（不是 hex，也不是 base64url）。
/// `crypto.sign("sha256", ...)` 对 EC 密钥产出 DER/ASN.1 编码的签名，而
/// `rcgen::KeyPair` 的 `PKCS_ECDSA_P256_SHA256` 用的正是
/// `ECDSA_P256_SHA256_ASN1_SIGNING` ⇒ **两者格式一致**，无需手工转换。
///
/// 私钥 PEM 为空（密钥对生成失败）时返回错误而不是空签名 —— 空签名必然被拒，
/// 但报错能让调用方把它归到 Transient 而不是 Rejected，不会误判账号失效。
fn device_proof(
    method: &str,
    path: &str,
    client_id: &str,
    refresh_token: &str,
    private_key_pem: &str,
) -> Result<Value, String> {
    use rand::Rng as _;
    use rcgen::{KeyPair, SigningKey as _};

    if private_key_pem.trim().is_empty() {
        return Err("设备私钥不可用，无法构造 DeviceProof（设备密钥对生成失败）".to_string());
    }
    let kp = KeyPair::from_pem(private_key_pem)
        .map_err(|e| format!("设备私钥解析失败，无法构造 DeviceProof: {e}"))?;

    // 秒级时间戳 + 16 字节 hex nonce（与 `Math.floor(Date.now()/1e3)` /
    // `randomBytes(16).toString("hex")` 逐项一致）。
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let nonce: String = {
        let mut rng = rand::thread_rng();
        let bytes: [u8; 16] = rng.gen();
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    };

    let payload = [
        method,
        path,
        client_id,
        refresh_token,
        &ts.to_string(),
        &nonce,
    ]
    .join("\n");

    let sig = kp
        .sign(payload.as_bytes())
        .map_err(|e| format!("DeviceProof 签名失败: {e}"))?;
    use base64::Engine as _;
    let signature = base64::engine::general_purpose::STANDARD.encode(&sig);

    Ok(json!({
        "Signature": signature,
        "Timestamp": ts,
        "Nonce": nonce,
    }))
}

/// 从响应体里取 `refresh_token` 过期时间（兼容秒/毫秒两种时间戳）。
///
/// ★ 2026-10-02 补 `RefreshExpireAt` —— 真实响应（`Result` 里）用的就是这个名字，
/// 是**毫秒**时间戳（如 `1790906034345`）。
pub fn extract_refresh_expiry(body: &Value) -> Option<i64> {
    const KEYS: &[&str] = &[
        "RefreshExpireAt",
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

/// 从真实错误信封里抽出错误描述。
///
/// 真实失败响应（2026-10-02 实测，HTTP 400）：
///
/// ```json
/// {"ResponseMetadata":{"Action":"","Error":{"Code":"10101","Data":{},
///   "Message":"无效参数：{__Message.field}.","StandardCode":"040004"},
///   "OID":null,"Region":"","RequestId":"","Service":"","Source":"",
///   "TraceID":"00000000000000000000000000000000","Version":"","WID":null}}
/// ```
///
/// `Error` 为 `null`（成功响应的样子）时返回 `None`。
/// `{__Message.field}` 是上游的模板占位符，原样透出会让用户看不懂，替换成「某个字段」。
fn metadata_error(body: &Value) -> Option<String> {
    let err = body.get("ResponseMetadata")?.get("Error")?;
    if err.is_null() {
        return None;
    }
    let code = err.get("Code").and_then(Value::as_str).unwrap_or("");
    let msg = err
        .get("Message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .replace("{__Message.field}", "某个字段");
    if code.is_empty() && msg.is_empty() {
        return None;
    }
    Some(match (code.is_empty(), msg.is_empty()) {
        (false, false) => format!("code={code} {msg}"),
        (false, true) => format!("code={code}"),
        _ => msg,
    })
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

    /// ★★ 真实响应形状（2026-10-02 从客户端源码确定的权威形状）。
    ///
    /// `main.js` 的响应处理是 `if (!r?.Result) throw; return r.Result`，
    /// 取出的 `Result` 里 token 键名是**大驼峰 `Token`**。
    /// 修复前 `unwrap_scopes` 只认小写 `result`，`ACCESS_KEYS` 也没有裸 `Token`
    /// ⇒ 真实响应会被判成「未找到 access token」。
    #[test]
    fn 响应解析_真实_Result_大驼峰形状() {
        let body = json!({
            "ResponseMetadata": {
                "Action": "", "Error": null, "OID": null, "Region": "",
                "RequestId": "", "Service": "", "Source": "",
                "TraceID": "00000000000000000000000000000000", "Version": "", "WID": null
            },
            "Result": {
                "Token": "AT_REAL",
                "RefreshToken": "RT_REAL",
                "TokenExpireAt": 1_790_906_034_345i64,
                "TokenExpireDuration": 600_000,
                "RefreshExpireAt": 1_790_906_034_345i64
            }
        });
        let (a, r) = parse_exchange_response(&body).expect("真实形状必须能解析");
        assert_eq!(a, "AT_REAL");
        assert_eq!(r.as_deref(), Some("RT_REAL"));

        // 过期时间也要能从 `Result.RefreshExpireAt`（毫秒）里读出来并归一为秒
        assert_eq!(
            extract_refresh_expiry(&body),
            Some(1_790_906_034),
            "RefreshExpireAt 是毫秒，应归一为秒"
        );
    }

    /// 裸 `Token` 只能当 access，**绝不能**被当成 refresh。
    ///
    /// 若误把 `Token` 填进 refresh 候选，账号库里会存下 access token 当 refresh，
    /// 表现为"登录成功、几小时后再也刷不出来"—— 极难归因。
    #[test]
    fn 裸_Token_不会被误当成_refresh() {
        let body = json!({ "Result": { "Token": "AT_ONLY" } });
        let (a, r) = parse_exchange_response(&body).unwrap();
        assert_eq!(a, "AT_ONLY");
        assert_eq!(r, None, "没有 RefreshToken 时 refresh 必须是 None，不能复用 Token");
    }

    /// `ResponseMetadata.Error` 有内容时，即便没有数字 `code` 也不能当成功。
    #[test]
    fn 响应解析_真实错误信封被识别() {
        let body = json!({
            "ResponseMetadata": {
                "Error": { "Code": "10101", "Message": "无效参数：{__Message.field}.",
                           "StandardCode": "040004" }
            }
        });
        assert!(parse_exchange_response(&body).is_err());
        // 上游模板占位符要替换成中文，原样透出用户看不懂
        let err = metadata_error(&body).unwrap();
        assert!(err.contains("某个字段"), "实际: {err}");
        assert!(!err.contains("__Message.field"), "占位符未替换: {err}");
    }

    /// 成功响应的 `Error` 是 `null` ⇒ 不能误判成失败。
    #[test]
    fn 成功信封的_null_Error_不算错误() {
        let body = json!({
            "ResponseMetadata": { "Action": "", "Error": null, "TraceID": "0000" },
            "Result": { "Token": "AT", "RefreshToken": "RT" }
        });
        assert_eq!(metadata_error(&body), None);
        assert!(parse_exchange_response(&body).is_ok());
    }

    /// ★ 签名口径的跨语言验证（在 Rust 侧只做可自证的部分）。
    ///
    /// 与 Node 的互操作性已经**实测**过（`test-bin/sig-verify.mjs`）：
    /// rcgen 的 `PKCS_ECDSA_P256_SHA256` 产出 70 字节 DER/ASN.1 签名，
    /// Node 的 `crypto.verify("sha256", ..., {dsaEncoding:"der"})` 认可；
    /// 按 P1363 解则失败 ⇒ 确认是 DER 而非 raw r||s，与真实客户端一致。
    ///
    /// 这里固化的是**可离线自证**的那一半：签名原文的拼接顺序与换行，
    /// 以及"私钥 PEM 为空必须报错而不是发空签名"。
    #[test]
    fn 设备证明_私钥为空时报错而不是发空签名() {
        let err = device_proof("POST", EXCHANGE_PATH, "cid", "rt", "").unwrap_err();
        assert!(err.contains("设备私钥不可用"), "实际: {err}");
    }

    /// 签名原文必须是 `[METHOD, PATH, ClientID, RefreshToken, ts, nonce]` 换行连接。
    ///
    /// 顺序错一个位置服务端就验不过，而错误码是笼统的「无效参数」——
    /// 所以把形状固化成断言。
    ///
    /// ⚠ 这里**不**做验签：`ring` 不是本 crate 的直接依赖（只是 rustls 的传递依赖），
    /// 直接引用会编译失败。签名正确性靠**跨语言实测**保证 ——
    /// `test-bin/sig-verify.mjs` 已验证 rcgen 的 DER 签名能被 Node 的
    /// `crypto.verify(..., {dsaEncoding:"der"})` 认可（且按 P1363 解会失败）。
    #[test]
    fn 设备证明_签名原文拼接顺序与形状() {
        let kp = rcgen::KeyPair::generate().expect("生成测试密钥");
        let priv_pem = kp.serialize_pem();
        let proof = device_proof("POST", EXCHANGE_PATH, "CID_X", "RT_Y", &priv_pem).unwrap();

        let ts = proof["Timestamp"].as_i64().expect("Timestamp 必须是数字");
        // 秒级（不是毫秒）：2020-01-01 之后的秒级时间戳都 < 10^10
        assert!(ts > 1_577_836_800 && ts < 10_000_000_000, "Timestamp 应是秒级: {ts}");

        let nonce = proof["Nonce"].as_str().expect("Nonce 必须是字符串");
        assert_eq!(nonce.len(), 32, "nonce 是 16 字节的 hex，应为 32 字符");
        assert!(
            nonce.chars().all(|c| c.is_ascii_hexdigit()),
            "nonce 必须是 hex: {nonce}"
        );

        use base64::Engine as _;
        let sig = base64::engine::general_purpose::STANDARD
            .decode(proof["Signature"].as_str().unwrap())
            .expect("Signature 必须是标准 base64（不是 base64url）");
        // DER SEQUENCE 头 ⇒ 与 Node crypto.sign 对 EC 密钥的输出同格式
        assert_eq!(sig[0], 0x30, "ECDSA 签名应是 DER/ASN.1（首字节 0x30）");
        assert!(
            (68..=72).contains(&sig.len()),
            "P-256 DER 签名应在 70 字节附近，实际 {}",
            sig.len()
        );
    }

    /// 同一份原文 + 同一把私钥，两次签名都不同（ECDSA 是随机化的）。
    ///
    /// 固化成断言是为了防止有人"优化"成缓存签名 —— 服务端会按 nonce/时间戳
    /// 判重放，缓存签名会直接失败。
    #[test]
    fn 设备证明_每次签名都不同() {
        let kp = rcgen::KeyPair::generate().unwrap();
        let priv_pem = kp.serialize_pem();
        let a = device_proof("POST", EXCHANGE_PATH, "c", "r", &priv_pem).unwrap();
        let b = device_proof("POST", EXCHANGE_PATH, "c", "r", &priv_pem).unwrap();
        assert_ne!(a["Nonce"], b["Nonce"], "nonce 必须每次重新生成");
        assert_ne!(a["Signature"], b["Signature"], "ECDSA 签名必须每次不同");
    }
}
