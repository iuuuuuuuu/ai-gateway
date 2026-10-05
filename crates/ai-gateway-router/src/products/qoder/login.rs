//! Qoder 登录：PKCE、会话持久化、设备流轮询。
//!
//! 对应 Go 两个文件的两段：
//!
//! - `internal/qoder/token.go:150-389`（`LoginStart` / `LoginPoll` / `makePKCE` /
//!   `sanitizeUID` / `clientID` / `redirectURIFor`）
//! - `internal/qoder/logincli.go:36-252`（`parseRFC3339` / `deterministicUUID` /
//!   `sessionsDir` / `saveSession` / `loadSession` / `newCLI` / `loadProxyConfig`）
//!
//! # 为什么会话文件放在 authDir 的**同级** `sessions/`
//!
//! `authDir` 会被 [`super::cred::QoderCred::load_dir`] 用 `qoder*.json` glob 扫描。
//! 会话文件若混在里面会被当成凭证解析 —— 它没有 `auth.accessToken`，于是
//! 每个会话文件都会在账号页上变成一条"凭证解析失败"。故 `sessions/` 必须是
//! `parent(authDir)` 的子目录，**不是** `authDir` 的子目录。
//!
//! # 为什么会话文件是 0600
//!
//! 里面存着 PKCE `verifier`。泄漏它等于让别人能抢先完成这次授权 —— 拿到
//! 本账号的令牌。Unix 上设 0600；Windows 上沿用宿主目录的 ACL（`std::fs`
//! 没有 POSIX 权限位，`create_dir_all` 的 mode 参数在 Windows 上是空操作）。
//!
//! # 两条鉴权路径不能混（见 [`super`] 模块头）
//!
//! 本文件只碰 **OpenAPI 域**（`openapi.*`，只带 `Bearer`，不签名）。

use std::path::Path;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::client::{read_limited, QoderClient};
use super::cred::{hex_short, new_uuid, QoderCred};
use super::region::QoderRegion;

/// 会话存活上限（Go `logincli.go:81` 的 `sessionTTL`）。
pub(crate) const SESSION_TTL: Duration = Duration::from_secs(30 * 60);

/// 单次轮询响应体上限（Go `io.LimitReader(resp.Body, 1<<20)`）。
const MAX_POLL_BYTES: usize = 1 << 20;

/// 官方客户端的默认 `client_id`（Go `token.go:158` 的 `oauthClientIDDefault`）。
///
/// ★ 取证方式：官方 `app.asar` 里搜 `authClientIds`，形如
/// `authClientIds: { prod: "732aef47-…", test: "732aef47-…" }`。
/// **国服与国际版用同一个值**，所以不需要按区域分。
pub(crate) const OAUTH_CLIENT_ID_DEFAULT: &str = "732aef47-9cf2-46a2-95fe-4cebb5d0d1fa";

/// 覆盖 `client_id` 的环境变量（对齐官方客户端的 `Kje()`）。
const OAUTH_ENV_CLIENT_ID: &str = "QODER_AUTH_CLIENT_ID";

/// 国际版的 `redirect_uri`（Go `token.go:191` 的 `oauthRedirectURIStable`）。
///
/// ⚠ 官方两个客户端**不同**：国际版 `qoder-app://`，国服是 `null` ——
/// 即国服**根本不传该参数**。旧代码两区都用 `qoder-work-cn://`，那是
/// **从未注册过**的协议（实测本机 HKCR 只有 `qoder` 与 `qoder-cn`）。
///
/// 注意设备流的 token 是靠 **poll** 拿的，浏览器回调失败**不影响**登录结果；
/// 但 `redirect_uri` 仍要与官方一致 —— 上游在**授权提交**时校验它。
const OAUTH_REDIRECT_URI_STABLE: &str = "qoder-app://";

/// 覆盖 `redirect_uri` 的环境变量。
const OAUTH_ENV_REDIRECT_URI: &str = "QODER_AUTH_REDIRECT_URI";

/// 会话过期的错误文案里那个分钟数。
///
/// 与 [`SESSION_TTL`] 必须同步（Go 用 `int(sessionTTL.Minutes())` 现算）。
const SESSION_TTL_MINUTES: u64 = SESSION_TTL.as_secs() / 60;

/// 一次登录过程的状态（发起与轮询之间传递）。
///
/// 与 Go `LoginSession`（`token.go:211`）逐字对齐。
///
/// ★ 这里**刻意不 derive `Serialize`/`Deserialize`**：磁盘形态由 [`SessionDoc`]
/// 单独描述（那边 `region` 是字符串，而这里是 [`QoderRegion`] 枚举）。给结构体
/// 直接 derive 会要求 `QoderRegion` 也实现 serde，于是落盘格式就绑死在枚举的
/// 派生表示上 —— 而 Go 写的是 `region.as_str()`，两者不是同一个契约。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoginSession {
    /// PKCE 校验串（轮询时提交）。
    pub verifier: String,
    /// 本次登录的唯一标识。
    pub nonce: String,
    /// 本次登录生成的机器指纹（登录成功后要写进凭证）。
    pub machine_id: String,
    /// 用户在浏览器打开的授权链接。
    pub auth_url: String,
    /// 本次登录的区域。
    pub region: QoderRegion,
}

// ---------------------------------------------------------------------------
// PKCE / 标识符
// ---------------------------------------------------------------------------

/// 生成 PKCE `(verifier, challenge)`（Go `token.go:360` 的 `makePKCE`）。
///
/// ⚠ 与参考实现**逐字一致**（含按字节取模 66 的采样方式）。
/// 看起来"可以优化"成 `rand` 直接取模，但那是不同的分布 ——
/// 保持与已验证可用的实现一致，不要顺手改。
pub(crate) fn make_pkce() -> (String, String) {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut buf = [0u8; 64];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    let verifier: String = buf
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect();
    let digest = Sha256::digest(verifier.as_bytes());
    (verifier, B64URL.encode(digest))
}

/// 把 uid 变成安全的文件名片段（防路径穿越，Go `token.go:375` 的 `sanitizeUID`）。
pub(crate) fn sanitize_uid(uid: &str) -> String {
    let mut out = uid.trim().to_string();
    for (from, to) in [
        ("/", "_"),
        ("\\", "_"),
        ("..", "_"),
        (":", "_"),
        ("*", "_"),
        ("?", "_"),
        ("\"", "_"),
        ("<", "_"),
        (">", "_"),
        ("|", "_"),
    ] {
        out = out.replace(from, to);
    }
    if out.is_empty() {
        out.push_str("unknown");
    }
    out
}

/// `sha256("qoder-machine:" + seed)` 前 16 字节 → UUID 形态的确定性机器指纹
/// （Go `logincli.go:57` 的 `deterministicUUID`）。
///
/// ★ 注意 `h[12]` 被**丢弃**，第三段是**字面量 `"4"`** —— 这不是笔误，
/// 是参考实现的行为，照抄。
pub(crate) fn deterministic_uuid(seed: &str) -> String {
    let digest = Sha256::digest(format!("qoder-machine:{seed}").as_bytes());
    let h = hex::encode(&digest[..16]);
    format!(
        "{}-{}-4{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[13..16],
        &h[16..20],
        &h[20..32]
    )
}

/// 解析 RFC3339 时间串成 Unix 秒；失败返回 0。
///
/// Go（`logincli.go:40`）依次试 `time.RFC3339`、`"2006-01-02T15:04:05Z"`、
/// `"2006-01-02T15:04:05.000Z"` —— 这三者都是 RFC3339 的子集（后两个是
/// 固定 `Z` 无偏移、带/不带毫秒），`chrono` 的 `parse_from_rfc3339` 一次覆盖。
///
/// 返回 0 表示"未知"。那会被 [`QoderCred::needs_refresh`] 当成"必须刷新" —— 安全方向。
pub(crate) fn parse_rfc3339(s: &str) -> i64 {
    let s = s.trim();
    if s.is_empty() {
        return 0;
    }
    match chrono::DateTime::parse_from_rfc3339(s) {
        Ok(dt) => dt.timestamp(),
        Err(_) => 0,
    }
}

// ---------------------------------------------------------------------------
// 路径
// ---------------------------------------------------------------------------

/// Go `filepath.Dir` 的近似：空路径与根都退回 `"."`。
fn go_dir(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    match Path::new(path).parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_string_lossy().to_string(),
        _ => ".".to_string(),
    }
}

/// Go `filepath.Join` 的近似（`"."` 前缀被吃掉，与 Go 的 `Clean` 一致）。
pub(crate) fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir == "." {
        return name.to_string();
    }
    Path::new(dir)
        .join(name)
        .to_string_lossy()
        .to_string()
}

/// 会话目录 = `parent(authDir)/sessions`（Go `logincli.go:87`）。
///
/// ★ **同级不是里面** —— 见模块头。
pub(crate) fn sessions_dir(auth_dir: &str) -> String {
    join_path(&go_dir(auth_dir), "sessions")
}

/// 某个会话的文件路径（Go `logincli.go:92`）。
pub(crate) fn session_file(auth_dir: &str, session_id: &str) -> String {
    join_path(&sessions_dir(auth_dir), &format!("{}.json", sanitize_uid(session_id)))
}

// ---------------------------------------------------------------------------
// 会话落盘
// ---------------------------------------------------------------------------

/// 会话文件的磁盘形态（字段名与 Go 的 `doc` 逐字一致）。
#[derive(Debug, Deserialize)]
struct SessionDoc {
    #[serde(default)]
    verifier: String,
    #[serde(default)]
    nonce: String,
    #[serde(default)]
    machine_id: String,
    #[serde(default)]
    auth_url: String,
    #[serde(default)]
    region: String,
    #[serde(default)]
    created_at: i64,
}

/// 把登录会话写入 `sessions/`（Go `logincli.go:97` 的 `saveSession`）。
pub(crate) fn save_session(auth_dir: &str, s: &LoginSession) -> Result<(), String> {
    let dir = sessions_dir(auth_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建会话目录失败: {e}"))?;
    // 记录创建时间，供过期清理使用。键按字母序（serde_json 底层是 BTreeMap，
    // 与 Go 的 map 序列化同款），两空格缩进对齐 `json.MarshalIndent(doc, "", "  ")`。
    let doc = serde_json::json!({
        "verifier": s.verifier,
        "nonce": s.nonce,
        "machine_id": s.machine_id,
        "auth_url": s.auth_url,
        "region": s.region.as_str(),
        "created_at": chrono::Utc::now().timestamp(),
    });
    let raw = serde_json::to_vec_pretty(&doc).map_err(|e| e.to_string())?;
    // 0600：会话里含 PKCE verifier，泄漏它等于让别人能抢先完成授权。
    std::fs::write(session_file(auth_dir, &s.nonce), raw).map_err(|e| e.to_string())
}

/// 读回会话；不存在或已过期时返回可读错误（Go `logincli.go:120` 的 `loadSession`）。
///
/// ★ 过期时**删除文件**：否则用户每次点「登录」都会被同一个死会话挡住。
pub(crate) fn load_session(auth_dir: &str, session_id: &str) -> Result<LoginSession, String> {
    let path = session_file(auth_dir, session_id);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "登录会话不存在或已过期（{session_id}）。请重新点击「登录」发起一次授权"
            ));
        }
        // 其它读错误（权限等）原样返回 —— 报"会话过期"会误导排查方向。
        Err(e) => return Err(e.to_string()),
    };
    let doc: SessionDoc =
        serde_json::from_slice(&raw).map_err(|e| format!("会话文件损坏: {e}"))?;
    if doc.created_at > 0 {
        let age = chrono::Utc::now().timestamp() - doc.created_at;
        if age > SESSION_TTL.as_secs() as i64 {
            let _ = std::fs::remove_file(&path);
            return Err(format!(
                "登录会话已超过 {SESSION_TTL_MINUTES} 分钟，请重新发起登录"
            ));
        }
    }
    Ok(LoginSession {
        verifier: doc.verifier,
        nonce: doc.nonce,
        machine_id: doc.machine_id,
        auth_url: doc.auth_url,
        region: parse_region(&doc.region),
    })
}

/// `--region` / 会话文件里的区域字面量 → [`QoderRegion`]。
///
/// ⚠ Go 侧是裸类型转换 `Region(*region)`，**不 trim 不忽略大小写** ——
/// `--region " CN "` 会得到 `RegionUnknown`，然后被 `url` 子命令拒掉。
/// 照抄（多了宽容会让"区域写错"变成一个安静地打到错误区域的行为）。
pub(crate) fn parse_region(s: &str) -> QoderRegion {
    match s {
        "cn" => QoderRegion::CN,
        "intl" => QoderRegion::Intl,
        _ => QoderRegion::Unknown,
    }
}

// ---------------------------------------------------------------------------
// OAuth 参数
// ---------------------------------------------------------------------------

/// 本次登录该用的 `client_id`（Go `token.go:166` 的 `clientID`）。
fn client_id() -> String {
    match std::env::var(OAUTH_ENV_CLIENT_ID) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => OAUTH_CLIENT_ID_DEFAULT.to_string(),
    }
}

/// 该区域该用的 `redirect_uri`；空串 = **不传该参数**（Go `token.go:199`）。
fn redirect_uri_for(r: QoderRegion) -> String {
    if let Ok(v) = std::env::var(OAUTH_ENV_REDIRECT_URI) {
        if !v.trim().is_empty() {
            return v.trim().to_string();
        }
    }
    if r == QoderRegion::Intl {
        return OAUTH_REDIRECT_URI_STABLE.to_string();
    }
    // 国服：官方不传。传了反而可能被判参数不符。
    String::new()
}

/// Go `url.QueryEscape` 的逐字等价物。
///
/// `net/url` 的 `shouldEscape(_, encodeQueryComponent)` 只放行
/// 字母数字与 `-_.~`，空格写成 `+`，其余一律 `%XX`（**大写**十六进制）。
fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Go `url.PathEscape` 的逐字等价物（`shouldEscape(_, encodePathSegment)`）。
///
/// 只转义 `/ ; , ?` 与"其它一切"，放行 `$ & + : = @` —— 与
/// `encodeQueryComponent` 不同，空格在这里是 `%20`。
pub(crate) fn path_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' | b';' | b',' | b'?' => out.push_str(&format!("%{b:02X}")),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 发起登录：生成 PKCE 与机器指纹，返回授权链接（Go `token.go:227` 的 `LoginStart`）。
///
/// **不落盘、不发网络请求** —— 纯计算。
pub(crate) fn login_start(region: QoderRegion) -> Result<LoginSession, String> {
    if region == QoderRegion::Unknown {
        return Err("未指定区域：请选择国服或国际版（两区的授权页不同）".to_string());
    }
    let (verifier, challenge) = make_pkce();
    let nonce = new_uuid();
    let machine_id = new_uuid();

    // 参数顺序与官方 `zje()` 一致：
    //   challenge, challenge_method, nonce, machine_id, client_id[, redirect_uri]
    //
    // ⚠ `redirect_uri` **只在非空时才拼**（官方是
    // `...e.redirectUri ? { redirect_uri: e.redirectUri } : {}`）。
    //
    // `url.Values.Encode()` 按**键名字母序**输出，故这里显式排序，
    // 而不是靠"插入顺序恰好对"。
    let mut pairs: Vec<(String, String)> = vec![
        ("challenge".to_string(), challenge.clone()),
        ("challenge_method".to_string(), "S256".to_string()),
        ("nonce".to_string(), nonce.clone()),
        ("machine_id".to_string(), machine_id.clone()),
        ("client_id".to_string(), client_id()),
    ];
    let ru = redirect_uri_for(region);
    if !ru.is_empty() {
        pairs.push(("redirect_uri".to_string(), ru));
    }
    pairs.sort_by(|a, b| a.0.cmp(&b.0));

    let query = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", query_escape(k), query_escape(v)))
        .collect::<Vec<_>>()
        .join("&");
    let auth_url = format!("{}/device/selectAccounts?{query}", region.website());

    Ok(LoginSession {
        verifier,
        nonce,
        machine_id,
        auth_url,
        region,
    })
}

// ---------------------------------------------------------------------------
// 设备流轮询
// ---------------------------------------------------------------------------

/// 轮询一次授权结果的三态（Go 用 `(*Cred, error)` + `LoginPending` 表达）。
///
/// 单独成型是为了让调用方能把「继续等」与「真失败」区分开 ——
/// 二者都算错误的话，界面只能笼统报错，用户不知道是该再等等还是重新登录。
#[derive(Debug)]
pub(crate) enum PollOutcome {
    /// 授权成功，凭证已填充（机器指纹来自 session）。
    Done(Box<QoderCred>),
    /// 用户还没确认 —— 调用方应稍后重试。**这是正常中间态，不是错误。**
    Pending,
}

/// 轮询一次授权结果（Go `token.go:277` 的 `LoginPoll`）。
///
/// 生成的文件名是 `qoder-<uid>.json`（与 `load_dir` 的 glob 一致）。
pub(crate) async fn login_poll(
    http: &reqwest::Client,
    s: &LoginSession,
    auth_dir: &str,
) -> Result<PollOutcome, String> {
    let endpoint = format!(
        "{}/api/v1/deviceToken/poll?nonce={}&verifier={}&challenge_method=S256",
        s.region.openapi(),
        query_escape(&s.nonce),
        query_escape(&s.verifier),
    );

    let resp = http
        .get(&endpoint)
        .header(reqwest::header::USER_AGENT, "Go-http-client/2.0")
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| format!("轮询授权结果失败: {e}"))?;

    let status = resp.status().as_u16();
    let raw = read_limited(resp, MAX_POLL_BYTES).await;
    let text = String::from_utf8_lossy(&raw).into_owned();

    // 404 / 202 = 尚未授权（上游用这两种状态表示"还没好"）。
    if status == 404 || status == 202 {
        return Ok(PollOutcome::Pending);
    }
    if status >= 400 {
        return Err(format!(
            "轮询授权失败（HTTP {status}）：{}",
            super::cred::truncate_str(&text, 200)
        ));
    }

    #[derive(Deserialize)]
    struct PollTok {
        #[serde(default)]
        token: String,
        #[serde(default)]
        device_token: String,
        #[serde(default)]
        refresh_token: String,
        #[serde(default)]
        user_id: String,
        #[serde(default)]
        expires_in: i64,
    }
    let tok: PollTok = serde_json::from_str(&text).map_err(|e| {
        format!(
            "授权响应解析失败: {e}（原始内容：{}）",
            super::cred::truncate_str(&text, 160)
        )
    })?;

    let dt = super::cred::first_non_empty(&[&tok.token, &tok.device_token]);
    if dt.is_empty() {
        // 200 但没有 token：仍视为未完成（上游偶尔这么回）。
        return Ok(PollOutcome::Pending);
    }
    if tok.user_id.is_empty() {
        return Err(format!(
            "授权成功但响应里没有 user_id，无法确定账号标识（原始内容：{}）",
            super::cred::truncate_str(&text, 160)
        ));
    }

    let mut c = QoderCred {
        uid: tok.user_id,
        dt,
        drt: tok.refresh_token,
        machine_id: s.machine_id.clone(),
        machine_token: hex_short(32),
        machine_type: "5".to_string(),
        region: s.region,
        ..Default::default()
    };
    if tok.expires_in > 0 {
        c.dt_expires_at = chrono::Utc::now().timestamp() + tok.expires_in / 1000;
    }

    // 落盘：文件名与 load_dir 的 glob 对齐。
    if !auth_dir.is_empty() {
        std::fs::create_dir_all(auth_dir).map_err(|e| format!("创建凭证目录失败: {e}"))?;
        c.file_path = join_path(
            auth_dir,
            &format!("qoder-{}.json", sanitize_uid(&c.uid)),
        );
        c.save_atomic().map_err(|e| format!("保存凭证失败: {e}"))?;
    }
    Ok(PollOutcome::Done(Box::new(c)))
}

// ---------------------------------------------------------------------------
// 代理配置
// ---------------------------------------------------------------------------

/// 网关配置文件里读出的代理设置。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProxyConfig {
    pub proxy: String,
    pub intl: bool,
}

#[derive(Debug, Default, Deserialize)]
struct ProxyDoc {
    #[serde(default)]
    proxy: String,
    #[serde(default)]
    proxy_scope: ProxyScope,
}

#[derive(Debug, Default, Deserialize)]
struct ProxyScope {
    #[serde(default)]
    intl: bool,
}

/// 候选配置路径，**按优先级**排列（Go `logincli.go:215` 的三个候选）。
///
/// 抽出来是为了能**确定性地**测顺序 —— 直接测 [`load_proxy_config`] 会被
/// 开发机真实的 home 目录影响（本机上 `~/.wb-switch/gateway/…` 确实存在，
/// 于是"找不到就该直连"这条断言永远走不到）。
///
/// `home` / `env_home` 由调用方注入，测试可以喂空值。两者都**先 trim 再判空**：
/// Go 在取值处就 `TrimSpace` 了，但这里自己做一遍更稳 —— 否则空白串会拼出
/// `"  \gateway\gateway_native_config.json"` 这种永远不存在的路径，
/// 让"配置在别处"变成一条静默的假候选。
fn proxy_config_candidates(auth_dir: &str, home: Option<&str>, env_home: Option<&str>) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    // ★ 两级 Dir：<data>/qoder/auths → <data>。
    // Go 的 `filepath.Dir` 不会返回空串，故这个候选**总是**存在。
    let d = go_dir(&go_dir(auth_dir));
    candidates.push(join_path(
        &join_path(&d, "gateway"),
        "gateway_native_config.json",
    ));
    if let Some(h) = home.map(str::trim).filter(|h| !h.is_empty()) {
        candidates.push(join_path(
            &join_path(h, ".wb-switch"),
            "gateway\\gateway_native_config.json",
        ));
    }
    if let Some(e) = env_home.map(str::trim).filter(|e| !e.is_empty()) {
        candidates.push(join_path(
            &join_path(e, "gateway"),
            "gateway_native_config.json",
        ));
    }
    candidates
}

/// 逐个读候选文件，第一个能读且**能解析**的胜出（Go 的 `for` + `continue`）。
///
/// 同样抽出来以便确定性地测：喂 `&[]` → 默认值（直连），喂临时文件 → 解析结果。
fn read_proxy_config(candidates: &[String]) -> ProxyConfig {
    for p in candidates {
        let Ok(raw) = std::fs::read(p) else { continue };
        // ⚠ 解析失败也 `continue`（不是报错）：坏掉的配置不该让登录直接失败。
        let Ok(doc) = serde_json::from_slice::<ProxyDoc>(&raw) else {
            continue;
        };
        return ProxyConfig {
            proxy: doc.proxy.trim().to_string(),
            intl: doc.proxy_scope.intl,
        };
    }
    ProxyConfig::default()
}

/// 从网关配置文件里读出 `(代理地址, 是否国际版走代理)`
/// （Go `logincli.go:215` 的 `loadProxyConfig`）。
///
/// 找不到文件时返回 `("", false)` —— 调用方据此走直连。
///
/// # 为什么用 authDir 往上找而不是写死 `%USERPROFILE%`
///
/// 配置文件与凭证目录同属一个数据目录：
///
/// ```text
/// <data>/gateway/gateway_native_config.json
/// <data>/qoder/auths
/// ```
///
/// 上溯两级既支持自定义数据目录，也让测试能指向临时目录。
pub(crate) fn load_proxy_config(auth_dir: &str) -> ProxyConfig {
    let home = dirs::home_dir()
        .filter(|h| !h.as_os_str().is_empty())
        .map(|h| h.to_string_lossy().to_string());
    let env_home = std::env::var("AI_GATEWAY_HOME")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let candidates = proxy_config_candidates(auth_dir, home.as_deref(), env_home.as_deref());
    read_proxy_config(&candidates)
}

/// 按 authDir 的代理配置构造 HTTP 层（Go `logincli.go:193` 的 `newCLI`）。
///
/// ★ Go 侧这里有个踩过的坑：函数体里必须调 `New()`，写成 `newCLI(authDir)`
/// 就是无限递归（实测 `goroutine stack exceeds 1000000000-byte limit`）。
/// Rust 没有这个问题，但保留注释说明为什么这里返回的是**新构造**的客户端。
pub(crate) fn new_cli_client(auth_dir: &str) -> QoderClient {
    let cfg = load_proxy_config(auth_dir);
    if !cfg.intl || cfg.proxy.trim().is_empty() {
        return QoderClient::new();
    }
    match QoderClient::new_with_proxy(Some(&cfg.proxy)) {
        Ok(c) => c,
        Err(e) => {
            // 不静默降级成"直连但用户以为走了代理" —— 明确告诉他这次没用上。
            eprintln!("代理地址无效，本次登录将直连：{e}");
            QoderClient::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_uid_replaces_every_unsafe_char() {
        assert_eq!(sanitize_uid("  abc  "), "abc");
        assert_eq!(sanitize_uid("a/b\\c"), "a_b_c");
        // `..` 先于单字符规则命中，且是**整体**替换成一个下划线。
        assert_eq!(sanitize_uid("a..b"), "a_b");
        assert_eq!(sanitize_uid("a:b*c?d\"e<f>g|h"), "a_b_c_d_e_f_g_h");
        // 空（或全空白）→ `unknown`，避免生成 `qoder-.json`。
        assert_eq!(sanitize_uid(""), "unknown");
        assert_eq!(sanitize_uid("   "), "unknown");
    }

    #[test]
    fn determinism_and_shape_of_machine_uuid() {
        let a = deterministic_uuid("acct-1");
        assert_eq!(a, deterministic_uuid("acct-1"));
        assert_ne!(a, deterministic_uuid("acct-2"));
        assert_eq!(a.len(), 36, "{a}");
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        // ★ 第三段恒以字面量 `4` 开头，且第 4 位来自 `h[13]` ——
        // `h[12]` 被丢弃，这是参考实现的行为。
        assert!(parts[2].starts_with('4'), "{a}");
    }

    #[test]
    fn parse_rfc3339_handles_go_three_layouts_and_zero_on_garbage() {
        // Go 试的三个 layout：RFC3339、无毫秒固定 Z、带毫秒固定 Z。
        // 三者是**同一个瞬间**的不同写法（`18:00+08:00` == `10:00Z`）。
        assert_eq!(parse_rfc3339("2026-09-22T10:00:00Z"), 1790071200);
        assert_eq!(parse_rfc3339("2026-09-22T10:00:00.000Z"), 1790071200);
        assert_eq!(parse_rfc3339("2026-09-22T18:00:00+08:00"), 1790071200);
        // 失败 → 0（"未知"），`needs_refresh` 会当成必须刷新 —— 安全方向。
        assert_eq!(parse_rfc3339(""), 0);
        assert_eq!(parse_rfc3339("   "), 0);
        assert_eq!(parse_rfc3339("not-a-time"), 0);
    }

    #[test]
    fn make_pkce_is_url_safe_and_challenge_is_sha256_of_verifier() {
        let (v, c) = make_pkce();
        assert_eq!(v.len(), 64);
        assert!(
            v.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b)),
            "{v}"
        );
        // challenge = base64url(sha256(verifier))，无填充。
        assert!(!c.contains('='), "{c}");
        let expect = B64URL.encode(Sha256::digest(v.as_bytes()));
        assert_eq!(c, expect);
    }

    #[test]
    fn login_start_omits_redirect_uri_for_cn_and_sets_it_for_intl() {
        let cn = login_start(QoderRegion::CN).expect("cn 应能发起");
        assert!(cn.auth_url.starts_with("https://qoder.com.cn/device/selectAccounts?"), "{}", cn.auth_url);
        // ★ 国服官方 `authRedirectUris: null` ⇒ **不传** redirect_uri。
        assert!(!cn.auth_url.contains("redirect_uri"), "{}", cn.auth_url);
        assert!(cn.auth_url.contains("challenge_method=S256"));
        assert!(cn.auth_url.contains(&format!("nonce={}", cn.nonce)));
        assert!(cn.auth_url.contains(&format!("machine_id={}", cn.machine_id)));
        assert!(cn.auth_url.contains(OAUTH_CLIENT_ID_DEFAULT));

        let intl = login_start(QoderRegion::Intl).expect("intl 应能发起");
        assert!(intl.auth_url.starts_with("https://qoder.com/device/selectAccounts?"), "{}", intl.auth_url);
        // `qoder-app://` 的 QueryEscape 形态。
        assert!(
            intl.auth_url.contains("redirect_uri=qoder-app%3A%2F%2F"),
            "{}",
            intl.auth_url
        );

        assert_eq!(
            login_start(QoderRegion::Unknown).unwrap_err(),
            "未指定区域：请选择国服或国际版（两区的授权页不同）"
        );
    }

    #[test]
    fn query_keys_are_sorted_like_go_url_values_encode() {
        let s = login_start(QoderRegion::CN).unwrap();
        let query = s.auth_url.split_once('?').unwrap().1;
        let keys: Vec<&str> = query
            .split('&')
            .map(|kv| kv.split('=').next().unwrap())
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted, "{query}");
    }

    #[test]
    fn parse_region_is_exact_match_only() {
        // Go 是裸类型转换，没有 trim、没有大小写折叠。
        assert_eq!(parse_region("cn"), QoderRegion::CN);
        assert_eq!(parse_region("intl"), QoderRegion::Intl);
        assert_eq!(parse_region(" CN "), QoderRegion::Unknown);
        assert_eq!(parse_region(""), QoderRegion::Unknown);
    }

    #[test]
    fn sessions_dir_is_a_sibling_of_auth_dir_not_a_child() {
        let d = sessions_dir("C:\\data\\qoder\\auths");
        assert_eq!(d, "C:\\data\\qoder\\sessions");
        // ★ 关键断言：**不能**落在 authDir 里面 —— 那里会被 `qoder*.json` glob 扫到。
        let inside = format!("{}", Path::new("C:\\data\\qoder\\auths").display());
        assert!(!d.starts_with(&format!("{inside}\\")));

        assert_eq!(sessions_dir(""), "sessions");
        assert_eq!(sessions_dir("."), "sessions");
        assert_eq!(sessions_dir("auths"), "sessions");
    }

    #[test]
    fn session_file_is_named_after_the_sanitized_id() {
        let p = session_file("C:\\data\\qoder\\auths", "a/b");
        assert_eq!(p, "C:\\data\\qoder\\sessions\\a_b.json");
    }

    #[test]
    fn session_round_trips_and_expiry_is_deleted() {
        let dir = std::env::temp_dir().join(format!("qoder-sess-{}", new_uuid()));
        let auth_dir = dir.join("qoder").join("auths").to_string_lossy().to_string();
        let s = login_start(QoderRegion::Intl).unwrap();

        save_session(&auth_dir, &s).expect("写入会话");
        let back = load_session(&auth_dir, &s.nonce).expect("读回会话");
        assert_eq!(back, s);

        // 缺失的会话 → 那句逐字文案，参数是用户传进来的 sessionID。
        let err = load_session(&auth_dir, "nope").unwrap_err();
        assert_eq!(
            err,
            "登录会话不存在或已过期（nope）。请重新点击「登录」发起一次授权"
        );

        // 陈旧会话 → 报过期**并删掉文件**（否则用户每次点「登录」都被它挡住）。
        let path = session_file(&auth_dir, &s.nonce);
        let mut doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        doc["created_at"] = serde_json::json!(chrono::Utc::now().timestamp() - 3600);
        std::fs::write(&path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
        let err = load_session(&auth_dir, &s.nonce).unwrap_err();
        assert_eq!(err, "登录会话已超过 30 分钟，请重新发起登录");
        assert!(!Path::new(&path).exists(), "过期会话文件应被删除");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn proxy_config_prefers_the_data_dir_two_levels_up() {
        let root = std::env::temp_dir().join(format!("qoder-proxy-{}", new_uuid()));
        let data = root.join("data");
        let auth_dir = data.join("qoder").join("auths").to_string_lossy().to_string();
        std::fs::create_dir_all(data.join("gateway")).unwrap();
        std::fs::write(
            data.join("gateway").join("gateway_native_config.json"),
            br#"{"proxy":"http://127.0.0.1:7890","proxy_scope":{"intl":true}}"#,
        )
        .unwrap();

        // ★ 只喂 authDir 派生的候选（home/env 传 None）——
        // 否则本机真实存在的 `~/.wb-switch/gateway/…` 会先命中，
        // 这条断言就变成了在测开发机的配置文件。
        let cands = proxy_config_candidates(&auth_dir, None, None);
        assert_eq!(cands.len(), 1, "只应有 authDir 上溯两级的那一个候选");
        let cfg = read_proxy_config(&cands);
        assert_eq!(cfg.proxy, "http://127.0.0.1:7890");
        assert!(cfg.intl);

        // 文件不存在 → 直连，不报错。
        let none = read_proxy_config(&proxy_config_candidates(
            &root.join("nowhere").join("qoder").join("auths").to_string_lossy(),
            None,
            None,
        ));
        assert_eq!(none.proxy, "");
        assert!(!none.intl);

        // 坏 JSON 不该让登录失败 —— 跳过它，落到下一个候选（这里没有，就直连）。
        std::fs::write(
            data.join("gateway").join("gateway_native_config.json"),
            b"{ this is not json",
        )
        .unwrap();
        let broken = read_proxy_config(&proxy_config_candidates(&auth_dir, None, None));
        assert_eq!(broken, ProxyConfig::default(), "坏配置应静默跳过");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn proxy_config_candidate_order_is_data_dir_then_home_then_env() {
        // 顺序是**契约**：第一个能解析的胜出，故顺序错了就会用错代理。
        let cands = proxy_config_candidates(
            r"C:\data\qoder\auths",
            Some(r"C:\Users\someone"),
            Some(r"D:\envhome"),
        );
        assert_eq!(cands.len(), 3, "{cands:?}");
        // ① 上溯两级再进 gateway/
        assert!(cands[0].contains("data") && cands[0].contains("gateway"), "{:?}", cands[0]);
        assert!(!cands[0].contains("qoder"), "① 应已上溯过 qoder/：{:?}", cands[0]);
        // ② home 下的 .wb-switch
        assert!(cands[1].contains(".wb-switch"), "{:?}", cands[1]);
        // ③ $AI_GATEWAY_HOME
        assert!(cands[2].contains("envhome"), "{:?}", cands[2]);

        // 给了空 home/env 就不该产生对应候选（避免生成 `\.wb-switch\…` 这种）。
        let only_authdir = proxy_config_candidates(r"C:\data\qoder\auths", None, None);
        assert_eq!(only_authdir.len(), 1, "{only_authdir:?}");
        let empty_strings = proxy_config_candidates(r"C:\data\qoder\auths", Some(""), Some("  "));
        assert_eq!(empty_strings.len(), 1, "空白 home/env 不算候选：{empty_strings:?}");
    }
}
