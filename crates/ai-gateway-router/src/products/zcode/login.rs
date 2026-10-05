//! ZCode OAuth 设备流（Go `internal/zcode/login.go` 的对应物，762 行）。
//!
//! # 两条路都要支持
//!
//! ```text
//! 方式 A  OAuth 设备流（本文件）  —— 备选
//! 方式 B  导入已有凭证（cred.rs 的 Parse）—— **主路径**
//! ```
//!
//! ★ 与 Qoder 的关键差异：Qoder 的凭证**只能**靠 OAuth 拿到，而 ZCode 的凭证是
//! 用户可以复制的一串（`{apiKey}.{secret}`）。故「导入」是主路径，OAuth 是备选
//! （Go `main.go:118` 的注释）。这条差异决定了本文件的定位：**够用就好**，
//! 不必为了它增加复杂度。
//!
//! # 会话必须落盘
//!
//! 发起与轮询是**两个独立进程**：
//!
//! ```text
//! 宿主 → gateway.exe zcode-login start --provider zai   （进程 A，退出）
//! 宿主 → gateway.exe zcode-login poll  --session <id>    （进程 B，新进程）
//! ```
//!
//! 进程 A 生成的 `pollToken` 必须被进程 B 拿到（轮询要用它做 Bearer），否则
//! 轮询会 401。故进程 A 把会话写进临时文件。
//!
//! # 两个服务商的兑换流程**不同**（这是最容易写错的地方）
//!
//! ```text
//! zai:      accessToken --(POST /api/auth/z/login)--> bizToken
//!           然后用 **Bearer bizToken** 调业务接口
//! bigmodel: **没有 z/login 这一步**，直接用 accessToken 当 biz token，
//!           且认证头是**裸值**（不带 "Bearer " 前缀！）
//! ```

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::cred::{self, ZcodeCred};
use super::errors::{self, UpstreamError};
use super::provider::Provider;

/// 发起登录（与 provider 无关，都打 `zcode.z.ai`）。
pub(crate) const OAUTH_INIT_PATH: &str = "/api/v1/oauth/cli/init";
/// 轮询（后接 flow_id）。
pub(crate) const OAUTH_POLL_PATH: &str = "/api/v1/oauth/cli/poll/";
/// Z.AI 的业务令牌兑换。
pub(crate) const ZAI_LOGIN_PATH: &str = "/api/auth/z/login";
/// 查机构与项目。
pub(crate) const CUSTOMER_INFO_PATH: &str = "/api/biz/customer/getCustomerInfo";
/// 密钥列表/创建（`%s` = orgId, projectId）。
pub(crate) const API_KEYS_PATH_FMT: &str = "/api/biz/v1/organization/%s/projects/%s/api_keys";
/// 取 secretKey（`%s/%s` = org/proj，`%s` = apiKey）。
pub(crate) const API_KEY_COPY_PATH_FMT: &str =
    "/api/biz/v1/organization/%s/projects/%s/api_keys/copy/%s";

/// 我们创建的密钥名（参考实现用同名，便于复用已存在的）。
///
/// 为什么固定名字：这样重复登录会**复用**同一个密钥，而不是每次都新建 ——
/// 否则用户控制台里会堆积一堆 `zcode-api-key`。
pub(crate) const API_KEY_NAME: &str = "zcode-api-key";

/// 优先选择的机构/项目名。
///
/// 上游账号可能属于多个机构。参考实现优先选名字含「默认机构」的，因为编码套餐
/// 通常挂在那里。找不到则取第一个。
const DEFAULT_ORG_MARKER: &str = "默认机构";
const DEFAULT_PROJECT_MARKER: &str = "默认项目";

/// 会话文件的最长存活时间。
///
/// 用户在浏览器里授权可能要几分钟（登录、二次验证、切换账号）。参考实现的登录
/// 超时是 5 分钟，这里给 30 分钟留足余量 —— 反正会话文件过期会自动清理。
pub(crate) const SESSION_TTL: Duration = Duration::from_secs(30 * 60);

/// 各响应的 body 上限（照抄 Go 的 `io.LimitReader` 取值）。
const MAX_LOGIN_BODY: usize = 1 << 20;
const MAX_BIZ_BODY: usize = 4 << 20;

/// 一次登录过程的状态（发起与轮询之间传递）。
///
/// 必须能被**序列化**：发起与轮询是两个独立的进程/请求（宿主调子命令，或前端
/// 轮询 HTTP 接口），会话要跨调用传递。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LoginSession {
    /// 服务商。
    ///
    /// ★ 走 `as_str` / `parse_provider` 而不是给 `Provider` 加 `derive`：
    /// Go 那边 `Provider` 是个字符串类型，`ProviderUnknown` 是**空串**
    /// （不是 `"unknown"`）。用 `#[serde(rename_all)]` 会把 `Unknown` 写成
    /// `"unknown"`，于是「老会话文件里的 `""`」读不出来，表现为「会话损坏」。
    /// 会话文件是**跨进程契约**，字段值必须与 Go 逐字一致。
    #[serde(with = "provider_serde")]
    pub provider: Provider,
    /// 上游给的流程标识（轮询路径参数）。
    pub flow_id: String,
    /// 客户端生成的 Bearer。
    ///
    /// ⚠ 轮询时用**自己生成的那个**，不是上游返回的 `poll_token` —— 参考实现
    /// 明确写了这点。
    pub poll_token: String,
    /// 用户在浏览器打开的链接。
    pub authorize_url: String,
    /// 会话过期时刻（Unix 秒）。
    pub expires_at: i64,
    /// 轮询间隔（秒）。
    pub poll_interval_sec: i64,
    /// 本地创建时刻（Unix 秒），用于清理过期会话文件。
    pub created_at: i64,
}

impl LoginSession {
    /// 轮询间隔（带下限 1 秒）。
    ///
    /// 上游可能返回 0 或很小的值；没有下限会导致高频轮询（打上游）。
    ///
    /// ★ `#[allow(dead_code)]`：**Rust 侧刻意不用它** —— 轮询节奏由宿主的
    /// 前端定时器决定（`zcode_login.rs::login_poll` 是「调一次、返回一次」，
    /// 没有内部 sleep 循环），故这里没有消费者。保留它是因为它是
    /// `LoginSession` 的**语义组成部分**：`poll_interval_sec` 会被 `start`
    /// 透给宿主（`pollIntervalS`），而「下限 1 秒」这条规则若只存在于 Go 侧，
    /// 将来谁把轮询循环搬进 Rust 就会丢掉它。有测试钉住这个下限。
    #[allow(dead_code)]
    pub fn poll_interval(&self) -> Duration {
        let sec = if self.poll_interval_sec < 1 {
            1
        } else {
            self.poll_interval_sec
        };
        Duration::from_secs(sec as u64)
    }
}

/// OAuth 拿到的中间令牌（还需兑换成最终凭证）。
///
/// ★ `refresh_token` / `user_id` 带 `#[allow(dead_code)]`：Go 那边它们被
/// `LoginTokens` 原样带着、`ExchangeCredential` 不消费（兑换只用
/// `AccessToken`）。留着是因为它们是**上游真实返回的东西**，且 `user_id`
/// 与 `cred.account_id_from_jwt` 是同一概念的两种来源 —— 删掉字段会让
/// 「上游明明给了、我们却没接」这件事在代码里看不出来。
#[derive(Debug, Clone, Default)]
pub(crate) struct LoginTokens {
    pub provider: Provider,
    pub access_token: String,
    #[allow(dead_code)]
    pub refresh_token: String,
    /// ZCode plan 令牌（额度查询要用）。
    pub jwt: String,
    /// JWT 签发时刻（本地记录）。
    pub jwt_issued_at: i64,
    #[allow(dead_code)]
    pub user_id: String,
}

/// `PollOnce` 的错误：要么是真失败，要么是「尚未完成」。
///
/// 「尚未完成」单独成型是为了让调用方能区分「继续等」与「真失败」—— 二者都返回
/// 一个笼统的 `Err(String)` 的话，界面只能笼统报错，用户不知道是该再等等还是
/// 重新登录。
#[derive(Debug, Clone)]
pub(crate) enum PollError {
    /// 尚未完成（**正常中间态**）。
    Pending(String),
    /// 真失败。
    Fatal(String),
}

impl PollError {
    /// 是不是「继续等」。
    pub fn is_pending(&self) -> bool {
        matches!(self, PollError::Pending(_))
    }
}

impl std::fmt::Display for PollError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // 与 Go `LoginPending.Error()` 逐字一致。
            PollError::Pending(reason) => {
                if reason.is_empty() {
                    write!(f, "授权尚未完成，请在浏览器中完成授权后重试")
                } else {
                    write!(f, "授权尚未完成：{reason}")
                }
            }
            PollError::Fatal(s) => write!(f, "{s}"),
        }
    }
}

/// `Provider` 的 serde 桥：落盘成 `as_str()`，读回走 `parse_provider()`。
///
/// 这样 `Unknown` 落盘是**空串**（与 Go 的 `ProviderUnknown = ""` 一致），而不是
/// `"unknown"`；读回时 `parse_provider("")` 也回到 `Unknown`。
mod provider_serde {
    use crate::products::zcode::provider::{parse_provider, Provider};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(p: &Provider, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(p.as_str())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Provider, D::Error> {
        let s = String::deserialize(d)?;
        Ok(parse_provider(&s))
    }
}

/// 会话文件目录。
///
/// 放在 `authDir` 的**同级**（不是里面）：`authDir` 会被 `load_dir` 用 glob
/// `zcode*.json` 扫描，会话文件混进去会被当成凭证解析并报错。
pub(crate) fn sessions_dir(auth_dir: &str) -> std::path::PathBuf {
    std::path::Path::new(auth_dir)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("sessions")
}

/// 某个 flow 的会话文件路径。
pub(crate) fn session_file(auth_dir: &str, flow_id: &str) -> std::path::PathBuf {
    sessions_dir(auth_dir).join(format!("{}.json", sanitize_name(flow_id)))
}

/// 把任意字符串变成安全的文件名片段（防路径穿越）。
///
/// 安全项：`flowID` 来自上游响应，直接拼进路径的话，恶意/异常的 `flowID`
///（如 `../../etc/passwd`）会写到目录外。
pub(crate) fn sanitize_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for r in s.chars() {
        match r {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => out.push(r),
            _ => out.push('_'),
        }
    }
    if out.is_empty() {
        return "unknown".to_string();
    }
    out
}

/// 写会话文件。对应 Go `saveSession`（`logincli.go:71-82`）。
pub(crate) fn save_session(auth_dir: &str, s: &LoginSession) -> Result<(), String> {
    let dir = sessions_dir(auth_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建会话目录失败: {e}"))?;
    let raw = serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?;
    let path = session_file(auth_dir, &s.flow_id);
    std::fs::write(&path, &raw).map_err(|e| format!("写入会话文件失败: {e}"))?;
    // 0600：会话里含 pollToken（泄漏它等于让别人能抢答轮询）。
    cred::restrict_to_owner(&path);
    Ok(())
}

/// 读会话文件。对应 Go `loadSession`（`logincli.go:84-102`）。
///
/// 三种错误逐字对齐 Go：不存在 / 损坏 / 超时（超时会**顺手删掉**文件）。
pub(crate) fn load_session(auth_dir: &str, flow_id: &str) -> Result<LoginSession, String> {
    let path = session_file(auth_dir, flow_id);
    let raw = match std::fs::read(&path) {
        Ok(r) => r,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                return Err(format!(
                    "登录会话不存在或已过期（{flow_id}）。请重新发起登录"
                ));
            }
            return Err(e.to_string());
        }
    };
    let s: LoginSession = serde_json::from_slice(&raw).map_err(|e| format!("会话文件损坏: {e}"))?;
    if s.created_at > 0 && now_unix() - s.created_at > SESSION_TTL.as_secs() as i64 {
        // 过期的会话文件顺手删掉（Go 同款）。
        let _ = std::fs::remove_file(&path);
        return Err(format!(
            "登录会话已超过 {} 分钟，请重新发起登录",
            SESSION_TTL.as_secs() / 60
        ));
    }
    Ok(s)
}

/// 删掉会话文件（轮询终结时用）。对应 Go 的 `os.Remove(sessionFile(...))`。
pub(crate) fn remove_session(auth_dir: &str, flow_id: &str) {
    let _ = std::fs::remove_file(session_file(auth_dir, flow_id));
}

/// 生成 `n` 字节的随机 hex 字符串（`2n` 个字符）。对应 Go `randomHex`。
///
/// 失败时返回错误而不是静默降级：`pollToken` 是轮询的 Bearer，弱随机等于让别人
/// 能猜出它并抢答轮询。
fn random_hex(n: usize) -> Result<String, String> {
    let mut b = vec![0u8; n];
    rand::rngs::OsRng
        .try_fill_bytes(&mut b)
        .map_err(|e| format!("生成随机字节失败: {e}"))?;
    Ok(hex::encode(b))
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl super::client::Client {
    /// 发起 OAuth 登录。对应 Go `StartLogin`（`login.go:131-208`）。
    ///
    /// 会发一次网络请求（拿 `authorize_url`）。
    pub(crate) async fn start_login(&self, provider: Provider) -> Result<LoginSession, String> {
        if provider == Provider::Unknown {
            return Err("未指定服务商：请选择 Z.AI 或智谱（两者的授权页不同）".to_string());
        }

        // pollToken 是**客户端生成的 32 随机字节的 hex**（64 字符）。
        // 参考实现：`randomBytes(32).toString("hex")`。
        let tok = random_hex(32).map_err(|e| format!("生成轮询令牌失败: {e}"))?;

        let body = serde_json::json!({ "provider": provider.as_str() }).to_string();
        let url = format!("{}{}", self.config_origin(), OAUTH_INIT_PATH);
        let mut req = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            // ★ 这里发的是**我们自己生成的** pollToken。
            .header("Authorization", format!("Bearer {tok}"))
            .body(body)
            .timeout(self.timeout);
        for (k, v) in self.identity.headers() {
            if !v.is_empty() {
                req = req.header(k, v);
            }
        }

        let resp = req
            .send()
            .await
            .map_err(|e| format!("发起 ZCode 登录失败: {e}"))?;
        let status = resp.status().as_u16();
        let raw = super::client::read_limited(resp, MAX_LOGIN_BODY).await;
        if status >= 400 {
            return Err(UpstreamError::new(status, &String::from_utf8_lossy(&raw)).to_string());
        }

        let doc: Value = serde_json::from_slice(&raw).map_err(|e| {
            format!(
                "登录响应解析失败: {e}（原始内容：{}）",
                errors::truncate(&String::from_utf8_lossy(&raw), 160)
            )
        })?;

        // 信封：{code, msg, data}
        let code = doc.get("code").and_then(Value::as_i64).unwrap_or(0);
        if code != 0 {
            return Err(UpstreamError {
                kind: errors::ErrKind::None,
                status,
                code: format!("{code}"),
                msg: doc
                    .get("msg")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            }
            .to_string());
        }

        let text = String::from_utf8_lossy(&raw);
        let data = doc.get("data").cloned().unwrap_or(Value::Null);
        let str_of = |k: &str| -> String {
            data.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let flow_id = str_of("flow_id");
        let authorize_url = str_of("authorize_url");
        let expires_at = data.get("expires_at").and_then(Value::as_i64).unwrap_or(0);
        let poll_interval_sec = data
            .get("poll_interval_sec")
            .and_then(Value::as_i64)
            .unwrap_or(0);

        // 形状守卫（参考实现也做）：缺字段会让后续轮询必然失败，而错误会显示成
        // 「轮询失败」，看不出是发起时就缺了东西。
        if flow_id.trim().is_empty() {
            return Err(format!(
                "登录响应缺少 flow_id（原始内容：{}）",
                errors::truncate(&text, 160)
            ));
        }
        if authorize_url.trim().is_empty() {
            return Err(format!(
                "登录响应缺少 authorize_url（原始内容：{}）",
                errors::truncate(&text, 160)
            ));
        }
        if expires_at <= 0 {
            return Err(format!(
                "登录响应缺少有效的 expires_at（原始内容：{}）",
                errors::truncate(&text, 160)
            ));
        }

        Ok(LoginSession {
            provider,
            flow_id,
            poll_token: tok,
            authorize_url,
            expires_at,
            poll_interval_sec,
            created_at: now_unix(),
        })
    }

    /// 轮询一次授权结果。对应 Go `PollOnce`（`login.go:232-349`）。
    ///
    /// # 错误语义（照抄参考实现，实测确认）
    ///
    /// ```text
    /// 网络异常            → pending（重试）
    /// HTTP 4xx（除 408/429）→ **致命**
    /// HTTP 408 / 429 / 5xx → pending（重试）
    /// 非 JSON body        → pending（重试）
    /// code 不是数字       → pending（重试）
    /// code ≠ 0            → **致命**
    /// status = ready      → 成功
    /// status = failed     → **致命**
    /// 其它 status          → **致命**
    /// ```
    ///
    /// 「哪些当致命、哪些当重试」是刻意的：把临时故障当致命会让用户白重登一次，
    /// 把真失败当重试会让用户无限等下去。
    pub(crate) async fn poll_once(&self, s: &LoginSession) -> Result<LoginTokens, PollError> {
        let url = format!(
            "{}{}{}",
            self.config_origin(),
            OAUTH_POLL_PATH,
            path_escape(&s.flow_id)
        );
        let mut req = self
            .http
            .get(&url)
            // 轮询用**我们自己生成**的 pollToken（不是上游返回的 poll_token）。
            .header("Authorization", format!("Bearer {}", s.poll_token))
            .header("Accept", "application/json")
            .timeout(self.timeout);
        for (k, v) in self.identity.headers() {
            if !v.is_empty() {
                req = req.header(k, v);
            }
        }

        let resp = match req.send().await {
            Ok(r) => r,
            // 网络异常 → 当作未完成重试（参考实现语义）。
            Err(_) => {
                return Err(PollError::Pending("网络异常，将重试".to_string()));
            }
        };
        let status = resp.status().as_u16();
        let raw = super::client::read_limited(resp, MAX_LOGIN_BODY).await;
        let text = String::from_utf8_lossy(&raw).to_string();

        // HTTP 层判定
        if status == 408 || status == 429 || status >= 500 {
            return Err(PollError::Pending(format!("上游 HTTP {status}，将重试")));
        }
        if status >= 400 {
            // 4xx（除 408/429）→ 致命
            return Err(PollError::Fatal(format!(
                "轮询授权失败（HTTP {status}）：{}",
                errors::truncate(&text, 200)
            )));
        }

        // 解析信封
        let doc: Value = match serde_json::from_slice(&raw) {
            Ok(v) => v,
            // 非 JSON → 当作未完成重试
            Err(_) => {
                return Err(PollError::Pending(
                    "上游返回了非 JSON 响应，将重试".to_string(),
                ));
            }
        };

        // code 不是数字 → 当作未完成重试
        //
        // ★ 注意 `as_i64` 对 `12.0` 这种 JSON 数字是**成功**的（serde_json 会把
        // 整数值的浮点表示为 i64），与 Go 的 `float64` 断言口径一致。
        let Some(code_num) = doc.get("code").and_then(Value::as_i64) else {
            return Err(PollError::Pending(
                "上游响应缺少数字 code，将重试".to_string(),
            ));
        };
        if code_num != 0 {
            // code ≠ 0 → 致命
            return Err(PollError::Fatal(
                UpstreamError {
                    kind: errors::ErrKind::None,
                    status,
                    code: format!("{code_num}"),
                    msg: doc
                        .get("msg")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                }
                .to_string(),
            ));
        }

        let data = doc.get("data").cloned().unwrap_or(Value::Null);
        if !data.is_object() {
            return Err(PollError::Pending(
                "上游 data 结构异常，将重试".to_string(),
            ));
        }

        let status_field = data
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match status_field.as_str() {
            "pending" => return Err(PollError::Pending(String::new())),
            "failed" => {
                return Err(PollError::Fatal(
                    "授权失败（上游 status=failed）。请重新发起登录".to_string(),
                ));
            }
            "ready" => {}
            other => {
                return Err(PollError::Fatal(format!(
                    "上游返回了未知的授权状态 {other:?}"
                )));
            }
        }

        // status=ready：必须能取到该 provider 的 access_token。
        //
        // ★ provider token 的 key 就是 provider 字符串本身（`data["zai"]` /
        // `data["bigmodel"]`）。
        let provider_key = s.provider.as_str();
        let Some(raw_prov) = data.get(provider_key) else {
            return Err(PollError::Fatal(format!(
                "授权成功但响应里没有 {provider_key:?} 的令牌（原始内容：{}）",
                errors::truncate(&text, 200)
            )));
        };
        let access_token = raw_prov
            .get("access_token")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let refresh_token = raw_prov
            .get("refresh_token")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if access_token.trim().is_empty() {
            return Err(PollError::Fatal(format!(
                "授权成功但 {provider_key:?} 的 access_token 为空（原始内容：{}）",
                errors::truncate(&text, 200)
            )));
        }

        let mut tokens = LoginTokens {
            provider: s.provider,
            access_token,
            refresh_token,
            jwt: data
                .get("token")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            jwt_issued_at: 0,
            user_id: data
                .get("user")
                .and_then(|u| u.get("user_id"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        };
        if !tokens.jwt.is_empty() {
            tokens.jwt_issued_at = now_unix();
        }
        Ok(tokens)
    }

    /// 把 OAuth 令牌兑换成最终凭证。对应 Go `ExchangeCredential`（`login.go:376-433`）。
    ///
    /// 最终凭证：
    ///
    /// ```text
    /// zai:      {apiKey}.{secretKey}   —— secretKey **缺失即失败**
    /// bigmodel: {apiKey}[.{secretKey}] —— secretKey 拿不到则降级为单段
    /// ```
    pub(crate) async fn exchange_credential(
        &self,
        t: &LoginTokens,
    ) -> Result<ZcodeCred, String> {
        if t.access_token.trim().is_empty() {
            return Err("OAuth 令牌为空，无法兑换凭证".to_string());
        }

        let host = t.provider.endpoints().biz_host;
        let auth_header = if t.provider == Provider::Zai {
            // ① 换业务令牌
            let biz_token = self.zai_biz_token(&t.access_token).await?;
            format!("Bearer {biz_token}")
        } else {
            // bigmodel：**裸值**，不带 Bearer 前缀
            t.access_token.clone()
        };

        // ② 机构与项目
        let (org_id, project_id) = self.customer_info(&host, &auth_header).await?;

        // ③ 找或建 api_key
        let api_key = self
            .find_or_create_api_key(&host, &auth_header, &org_id, &project_id)
            .await?;

        // ④ 取 secretKey
        let secret = match self
            .copy_secret_key(&host, &auth_header, &org_id, &project_id, &api_key)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                // zai 的 secretKey **必需**（参考实现 `requireSecretKey=true`）。
                if t.provider == Provider::Zai {
                    return Err(format!(
                        "Z.AI 的密钥缺少 secretKey，无法完成登录（该服务商要求两段式凭证）: {e}"
                    ));
                }
                // bigmodel 可选：降级为单段
                String::new()
            }
        };

        let credential = if secret.is_empty() {
            api_key
        } else {
            format!("{api_key}.{secret}")
        };

        // 注意：不能用 `c`（它是 `&self`），用 `cred` 避免遮蔽。
        let mut cred = cred::parse_credential(&credential)?;
        cred.provider = t.provider;
        cred.jwt = t.jwt.clone();
        cred.jwt_issued_at = t.jwt_issued_at;
        Ok(cred)
    }

    /// 用 OAuth `accessToken` 换 Z.AI 的业务令牌。对应 Go `zaiBizToken`（`login.go:439-486`）。
    ///
    /// ⚠ 这个端点**不用信封的 `code===0` 判定** —— 它直接读顶层字段（参考实现明确
    /// 写了这点，实测无效 token 返回 200 + `{"code":500,...}`）。
    async fn zai_biz_token(&self, access_token: &str) -> Result<String, String> {
        let url = format!("{}{}", Provider::Zai.endpoints().biz_host, ZAI_LOGIN_PATH);
        let body = serde_json::json!({ "token": access_token }).to_string();
        let mut req = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            // ★ 无 Authorization（这一条与 Go 一致）。
            .body(body)
            .timeout(self.timeout);
        for (k, v) in self.identity.headers() {
            if !v.is_empty() {
                req = req.header(k, v);
            }
        }

        let resp = req
            .send()
            .await
            .map_err(|e| format!("兑换 Z.AI 业务令牌失败: {e}"))?;
        let status = resp.status().as_u16();
        let raw = super::client::read_limited(resp, MAX_LOGIN_BODY).await;
        if status >= 400 {
            return Err(UpstreamError::new(status, &String::from_utf8_lossy(&raw)).to_string());
        }

        let text = String::from_utf8_lossy(&raw);
        let doc: Value = serde_json::from_slice(&raw)
            .map_err(|e| format!("业务令牌响应解析失败: {e}（原始内容：{}）", errors::truncate(&text, 160)))?;

        // 三种形状都兼容（参考实现的 ?? 链）。
        let tok = first_non_empty(&[
            str_at(&doc, "access_token"),
            str_at(&doc, "accessToken"),
            doc.pointer("/data/access_token")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ]);
        // 形状守卫：静默返回空会让后续请求全部 401，而错误显示成「凭证无效」。
        if tok.trim().is_empty() {
            return Err(format!(
                "业务令牌响应里没有 access_token（原始内容：{}）",
                errors::truncate(&text, 200)
            ));
        }
        Ok(tok)
    }

    /// 查机构与项目（两个服务商同一套路径，只是 host 与认证头不同）。
    ///
    /// 对应 Go `customerInfo`（`login.go:489-563`）。
    async fn customer_info(
        &self,
        host: &str,
        auth_header: &str,
    ) -> Result<(String, String), String> {
        let url = format!("{host}{CUSTOMER_INFO_PATH}");
        let data = self
            .biz_get(&url, auth_header)
            .await
            .map_err(|e| format!("查询机构信息失败: {e}"))?;
        let text = data.to_string();

        // ★ 两种键名都要认（`organizations` 与 `orgs`）。
        let mut orgs = data
            .get("organizations")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if orgs.is_empty() {
            orgs = data
                .get("orgs")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
        }
        if orgs.is_empty() {
            return Err(format!(
                "账号下没有任何机构（原始内容：{}）",
                errors::truncate(&text, 200)
            ));
        }

        // 优先选名字含「默认机构」的（编码套餐通常挂在那里），否则取第一个。
        let mut chosen = orgs[0].clone();
        for o in &orgs {
            let name = first_non_empty(&[str_at(o, "organizationName"), str_at(o, "name")]);
            if name.contains(DEFAULT_ORG_MARKER) {
                chosen = o.clone();
                break;
            }
        }
        let org_id = first_non_empty(&[
            str_at(&chosen, "organizationId"),
            str_at(&chosen, "id"),
            str_at(&chosen, "orgId"),
        ]);
        if org_id.is_empty() {
            return Err(format!(
                "机构缺少 id 字段（原始内容：{}）",
                errors::truncate(&text, 200)
            ));
        }

        let projects = chosen
            .get("projects")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if projects.is_empty() {
            return Err(format!(
                "默认机构下没有任何项目（原始内容：{}）",
                errors::truncate(&text, 200)
            ));
        }
        let mut proj = projects[0].clone();
        for p in &projects {
            let name = first_non_empty(&[str_at(p, "projectName"), str_at(p, "name")]);
            if name.contains(DEFAULT_PROJECT_MARKER) {
                proj = p.clone();
                break;
            }
        }
        let project_id = first_non_empty(&[str_at(&proj, "projectId"), str_at(&proj, "id")]);
        if project_id.is_empty() {
            return Err(format!(
                "项目缺少 id 字段（原始内容：{}）",
                errors::truncate(&text, 200)
            ));
        }
        Ok((org_id, project_id))
    }

    /// 找已存在的密钥，找不到就创建。对应 Go `findOrCreateAPIKey`（`login.go:568-639`）。
    ///
    /// 列表失败**静默忽略**直接创建（参考实现的做法）—— 列不出不代表建不了。
    async fn find_or_create_api_key(
        &self,
        host: &str,
        auth_header: &str,
        org_id: &str,
        project_id: &str,
    ) -> Result<String, String> {
        let list_url = format!(
            "{host}{}",
            format_path(API_KEYS_PATH_FMT, &[org_id, project_id])
        );

        // 先尝试列出
        if let Ok(data) = self.biz_get(&list_url, auth_header).await {
            if let Some(keys) = data.as_array() {
                for k in keys {
                    if str_at(k, "name") == API_KEY_NAME {
                        let api_key = str_at(k, "apiKey");
                        if !api_key.trim().is_empty() {
                            return Ok(api_key);
                        }
                    }
                }
            }
        }

        // 创建
        let body = serde_json::json!({ "name": API_KEY_NAME }).to_string();
        let mut req = self
            .http
            .post(&list_url)
            .header("Content-Type", "application/json")
            // ★ 裸值，**没有** `Bearer ` 前缀（`auth_header` 里 zai 已经带了）。
            .header("Authorization", auth_header)
            .header("Accept", "application/json")
            .body(body)
            .timeout(self.timeout);
        for (k, v) in self.identity.headers() {
            if !v.is_empty() {
                req = req.header(k, v);
            }
        }

        let resp = req
            .send()
            .await
            .map_err(|e| format!("创建 API 密钥失败: {e}"))?;
        let status = resp.status().as_u16();
        let raw = super::client::read_limited(resp, MAX_LOGIN_BODY).await;
        if status >= 400 {
            return Err(UpstreamError::new(status, &String::from_utf8_lossy(&raw)).to_string());
        }

        let text = String::from_utf8_lossy(&raw);
        // ⚠ 响应有**信封**（`{"code":0,"data":{"apiKey":"..."}}`），必须解开。
        //
        // 实测踩到：直接读顶层 `apiKey` 会拿不到值 —— 因为真正的 key 在 `data`
        // 里。这个 bug 的表现是「创建密钥的响应里没有 apiKey」，看起来像上游改了
        // 字段，实际是我们没解信封。
        let doc: Value = serde_json::from_slice(&raw).map_err(|e| {
            format!(
                "创建密钥响应解析失败: {e}（原始内容：{}）",
                errors::truncate(&text, 200)
            )
        })?;
        let api_key = first_non_empty(&[
            doc.pointer("/data/apiKey")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            str_at(&doc, "apiKey"),
        ]);
        // 形状守卫（参考实现 CL-06）：上游字段改名时必须报错，而不是存下
        // "undefined" 让后续每个请求都 401。
        if api_key.trim().is_empty() {
            return Err(format!(
                "创建密钥的响应里没有 apiKey（原始内容：{}）",
                errors::truncate(&text, 200)
            ));
        }
        Ok(api_key)
    }

    /// 取密钥的 `secretKey`。对应 Go `copySecretKey`（`login.go:657-677`）。
    ///
    /// # 为什么「取不到」要返回**错误**而不是空串
    ///
    /// 上游可能不返回 `secretKey`（返回 `{"code":0,"data":{}}`）。若这里静默返回
    /// `Ok(String::new())`，调用方就**无法区分**「取不到」与「取到了空值」——
    /// 于是 zai 会拿一个单段凭证去用，而它**永远签不了名**（参考实现：
    /// `requireSecretKey=true` 对 zai 是硬要求）。
    ///
    /// 故这里把「取不到」当成错误返回，由**调用方**决定严重性：
    ///
    /// ```text
    /// zai      → 致命（必须两段式）
    /// bigmodel → 降级为单段（secret 可选）
    /// ```
    async fn copy_secret_key(
        &self,
        host: &str,
        auth_header: &str,
        org_id: &str,
        project_id: &str,
        api_key: &str,
    ) -> Result<String, String> {
        let url = format!(
            "{host}{}",
            format_path(API_KEY_COPY_PATH_FMT, &[org_id, project_id, api_key])
        );
        let data = self.biz_get(&url, auth_header).await?;
        let text = data.to_string();

        // ★ 两种拼写都认（`secretKey` 与 `secret_key`）。
        let secret = first_non_empty(&[str_at(&data, "secretKey"), str_at(&data, "secret_key")]);
        if secret.is_empty() {
            return Err(format!(
                "响应里没有 secretKey（原始内容：{}）",
                errors::truncate(&text, 160)
            ));
        }
        Ok(secret)
    }

    /// 业务接口的 GET（自动解信封）。对应 Go `bizGet`（`login.go:686-736`）。
    ///
    /// 信封规则（参考实现 `requestBizApi`）：
    ///
    /// ```text
    /// code = body.code ?? body.status
    /// code 非空且不是 0/200/"0"/"200" → 报错
    /// 返回 body.data ?? body
    /// ```
    async fn biz_get(&self, raw_url: &str, auth_header: &str) -> Result<Value, String> {
        let mut req = self
            .http
            .get(raw_url)
            // ★ 裸值（zai 已在 `auth_header` 里带了 `Bearer `）。
            .header("Authorization", auth_header)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .timeout(self.timeout);
        for (k, v) in self.identity.headers() {
            if !v.is_empty() {
                req = req.header(k, v);
            }
        }

        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let raw = super::client::read_limited(resp, MAX_BIZ_BODY).await;
        if status >= 400 {
            return Err(UpstreamError::new(status, &String::from_utf8_lossy(&raw)).to_string());
        }

        let Ok(env) = serde_json::from_slice::<Value>(&raw) else {
            // 不是信封形状：原样返回（有些接口直接给数据）。
            //
            // ★ Go 这里是「JSON 解析失败 ⇒ 返回 raw」，而 raw 是**字节**；Rust 这边
            // 解析失败就没有 `Value` 可返回了。用 `Value::Null` 让调用方拿到「空」
            // 并自己报「原始内容」—— 这两条路径在上游正常时都走不到。
            return Ok(Value::Null);
        };

        let code = env.get("code").or_else(|| env.get("status"));
        if let Some(c) = code {
            if !is_ok_code(c) {
                return Err(format!(
                    "业务接口返回错误码 {}：{}",
                    display_code(c),
                    env.get("msg").and_then(Value::as_str).unwrap_or("")
                ));
            }
        }

        // `body.data ?? body`（`data` 为 `null` 时回落到整条）。
        match env.get("data") {
            Some(Value::Null) | None => Ok(env),
            Some(d) => Ok(d.clone()),
        }
    }
}

/// 判定信封的 `code` 是否表示成功。对应 Go `isOKCode`（`login.go:742-753`）。
///
/// 接受 `0` / `200` 的**数字与字符串**两种形态（参考实现的做法）—— 上游不同接口
/// 的表示不一致。
pub(crate) fn is_ok_code(v: &Value) -> bool {
    if let Some(n) = v.as_i64() {
        return n == 0 || n == 200;
    }
    if let Some(f) = v.as_f64() {
        return f == 0.0 || f == 200.0;
    }
    if let Some(s) = v.as_str() {
        let s = s.trim();
        return s == "0" || s == "200" || s.is_empty();
    }
    false
}

/// `code` 的显示形态。
///
/// Go 用 `%v`：数字打整数（`500`）而不是 `500.0`。`serde_json` 的 `Value::to_string`
/// 对 `Number` 已经给整数形态，直接用它。
fn display_code(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// 取对象里某个字符串字段（非字符串 ⇒ 空串）。
fn str_at(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// 取第一个非空值。对应 Go `firstNonEmpty`。
fn first_non_empty(vals: &[String]) -> String {
    for v in vals {
        if !v.trim().is_empty() {
            return v.clone();
        }
    }
    String::new()
}

/// 填 `%s` 占位符并做 `url.PathEscape`。对应 Go 的
/// `fmt.Sprintf(fmt, url.PathEscape(a), url.PathEscape(b), …)`。
fn format_path(fmt: &str, args: &[&str]) -> String {
    let mut out = String::with_capacity(fmt.len() + 32);
    let mut it = fmt.split("%s");
    if let Some(head) = it.next() {
        out.push_str(head);
    }
    for (i, seg) in it.enumerate() {
        match args.get(i) {
            Some(a) => out.push_str(&path_escape(a)),
            None => out.push_str("%s"),
        }
        out.push_str(seg);
    }
    out
}

/// `url.PathEscape` 的对应物（Go 版不转义 `+`、`:`、`@`、`&`、`=`、`$`）。
///
/// 与 `claim.rs::url_encode`（那是 `url.QueryEscape`，空格 ⇒ `+`）**不同**：
/// 路径段里空格必须编成 `%20`，用 `+` 会被上游当成字面加号。
fn path_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let unreserved = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~')
            // Go 的 `shouldEscape(c, encodePathSegment)` 对这五个**不**转义。
            || matches!(b, b'+' | b':' | b'@' | b'&' | b'=' | b'$');
        if unreserved {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_match_the_go_constants() {
        assert_eq!(OAUTH_INIT_PATH, "/api/v1/oauth/cli/init");
        assert_eq!(OAUTH_POLL_PATH, "/api/v1/oauth/cli/poll/");
        assert_eq!(ZAI_LOGIN_PATH, "/api/auth/z/login");
        assert_eq!(CUSTOMER_INFO_PATH, "/api/biz/customer/getCustomerInfo");
        assert_eq!(
            API_KEYS_PATH_FMT,
            "/api/biz/v1/organization/%s/projects/%s/api_keys"
        );
        assert_eq!(
            API_KEY_COPY_PATH_FMT,
            "/api/biz/v1/organization/%s/projects/%s/api_keys/copy/%s"
        );
        // ★ 固定名字 ⇒ 重复登录复用同一个密钥（否则控制台会堆一堆）。
        assert_eq!(API_KEY_NAME, "zcode-api-key");
    }

    #[test]
    fn session_ttl_is_thirty_minutes() {
        assert_eq!(SESSION_TTL.as_secs(), 30 * 60);
    }

    #[test]
    fn sessions_dir_is_a_sibling_of_the_auth_dir_not_a_child() {
        // ★ 关键：会话文件不能放进 authDir —— 那会被 `load_dir` 的 glob
        // `zcode*.json` 当成凭证解析并报错。
        let d = sessions_dir("/data/zcode/auths");
        assert_eq!(d, std::path::Path::new("/data/zcode/sessions"));
        assert!(!d.starts_with("/data/zcode/auths"));
    }

    #[test]
    fn sanitize_name_keeps_only_safe_characters() {
        assert_eq!(sanitize_name("abc-123_XYZ"), "abc-123_XYZ");
        assert_eq!(sanitize_name("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_name("中文"), "__");
    }

    #[test]
    fn sanitize_name_defeats_path_traversal() {
        // ★ 安全项：flowID 来自上游响应。直接拼进路径的话
        // `../../etc/passwd` 会写到目录外。
        let got = sanitize_name("../../etc/passwd");
        assert!(!got.contains('/'), "不能留下路径分隔符：{got}");
        assert!(!got.contains('\\'));
        assert!(!got.contains(".."), "不能留下 .. 片段：{got}");
        let f = session_file("/data/zcode/auths", "../../etc/passwd");
        assert!(f.starts_with("/data/zcode/sessions"), "必须仍在 sessions 里：{f:?}");
    }

    #[test]
    fn sanitize_name_falls_back_to_unknown_when_empty() {
        assert_eq!(sanitize_name(""), "unknown");
    }

    #[test]
    fn poll_interval_has_a_one_second_floor() {
        // 上游可能返回 0 或负数；没有下限会导致高频轮询（打上游）。
        let mut s = sample_session();
        s.poll_interval_sec = 0;
        assert_eq!(s.poll_interval(), Duration::from_secs(1));
        s.poll_interval_sec = -5;
        assert_eq!(s.poll_interval(), Duration::from_secs(1));
        s.poll_interval_sec = 3;
        assert_eq!(s.poll_interval(), Duration::from_secs(3));
    }

    #[test]
    fn poll_error_pending_display_matches_the_go_wording() {
        // 逐字对齐 Go `LoginPending.Error()`。
        assert_eq!(
            PollError::Pending(String::new()).to_string(),
            "授权尚未完成，请在浏览器中完成授权后重试"
        );
        assert_eq!(
            PollError::Pending("网络异常，将重试".to_string()).to_string(),
            "授权尚未完成：网络异常，将重试"
        );
        assert!(PollError::Pending(String::new()).is_pending());
        assert!(!PollError::Fatal("x".to_string()).is_pending());
    }

    #[test]
    fn is_ok_code_accepts_zero_and_two_hundred_in_both_shapes() {
        // 上游不同接口的表示不一致（参考实现的做法）。
        assert!(is_ok_code(&serde_json::json!(0)));
        assert!(is_ok_code(&serde_json::json!(200)));
        assert!(is_ok_code(&serde_json::json!("0")));
        assert!(is_ok_code(&serde_json::json!("200")));
        assert!(is_ok_code(&serde_json::json!("")));
        assert!(!is_ok_code(&serde_json::json!(500)));
        assert!(!is_ok_code(&serde_json::json!("500")));
        assert!(!is_ok_code(&serde_json::json!(null)));
    }

    #[test]
    fn format_path_escapes_each_argument() {
        assert_eq!(
            format_path(API_KEYS_PATH_FMT, &["org-1", "proj-2"]),
            "/api/biz/v1/organization/org-1/projects/proj-2/api_keys"
        );
        // 需要转义的字符（`/` 在路径段里必须编码，否则会多出一级路径）。
        assert_eq!(
            format_path(API_KEYS_PATH_FMT, &["a/b", "c d"]),
            "/api/biz/v1/organization/a%2Fb/projects/c%20d/api_keys"
        );
    }

    #[test]
    fn path_escape_keeps_go_unreserved_characters_and_uses_percent_for_space() {
        assert_eq!(path_escape("abcXYZ019-_.~"), "abcXYZ019-_.~");
        // Go 的 encodePathSegment 对这六个不转义。
        assert_eq!(path_escape("+:@&=$"), "+:@&=$");
        // ★ 空格编成 `%20`（不是 `url.QueryEscape` 的 `+`）。
        assert_eq!(path_escape("a b"), "a%20b");
        assert_eq!(path_escape("/"), "%2F");
    }

    #[test]
    fn save_and_load_session_round_trips() {
        let d = temp_auth_dir("login-roundtrip");
        let s = sample_session();
        save_session(&d, &s).expect("应能写会话");

        let got = load_session(&d, &s.flow_id).expect("应能读回会话");
        assert_eq!(got.flow_id, s.flow_id);
        // ★ pollToken 必须跨进程可见（轮询要用它做 Bearer，否则 401）。
        assert_eq!(got.poll_token, s.poll_token);
        assert_eq!(got.provider, Provider::Zai);
        assert_eq!(got.authorize_url, s.authorize_url);

        let _ = std::fs::remove_dir_all(std::path::Path::new(&d).parent().unwrap());
    }

    #[test]
    fn load_session_missing_file_says_the_go_wording() {
        let d = temp_auth_dir("login-missing");
        let e = load_session(&d, "flow-x").expect_err("不存在的会话应报错");
        assert_eq!(e, "登录会话不存在或已过期（flow-x）。请重新发起登录");
        let _ = std::fs::remove_dir_all(std::path::Path::new(&d).parent().unwrap());
    }

    #[test]
    fn load_session_corrupt_file_says_the_go_wording() {
        let d = temp_auth_dir("login-corrupt");
        let p = session_file(&d, "flow-y");
        std::fs::create_dir_all(p.parent().unwrap()).expect("应能建 sessions 目录");
        std::fs::write(&p, b"{not json").expect("应能写坏文件");
        let e = load_session(&d, "flow-y").expect_err("损坏的会话应报错");
        assert!(e.starts_with("会话文件损坏: "), "实际：{e}");
        let _ = std::fs::remove_dir_all(std::path::Path::new(&d).parent().unwrap());
    }

    #[test]
    fn load_session_expired_deletes_the_file_and_says_the_go_wording() {
        let d = temp_auth_dir("login-expired");
        let mut s = sample_session();
        // 31 分钟前创建 ⇒ 超过 30 分钟 TTL。
        s.created_at = now_unix() - 31 * 60;
        save_session(&d, &s).expect("应能写会话");
        let p = session_file(&d, &s.flow_id);
        assert!(p.exists(), "写完之后文件应在");

        let e = load_session(&d, &s.flow_id).expect_err("过期会话应报错");
        assert_eq!(e, "登录会话已超过 30 分钟，请重新发起登录");
        // ★ 顺手删掉文件（Go 同款）—— 否则过期会话会一直堆在磁盘上。
        assert!(!p.exists(), "过期会话文件应被删除");
        let _ = std::fs::remove_dir_all(std::path::Path::new(&d).parent().unwrap());
    }

    #[test]
    fn remove_session_is_idempotent() {
        let d = temp_auth_dir("login-remove");
        let s = sample_session();
        save_session(&d, &s).expect("应能写会话");
        remove_session(&d, &s.flow_id);
        assert!(!session_file(&d, &s.flow_id).exists());
        // 再删一次不该 panic（轮询终结与超时清理可能撞在一起）。
        remove_session(&d, &s.flow_id);
        let _ = std::fs::remove_dir_all(std::path::Path::new(&d).parent().unwrap());
    }

    #[test]
    fn session_json_uses_the_go_field_names() {
        // 会话文件是跨进程契约，字段名必须与 Go 的 json tag 逐字一致
        // （否则升级后老会话文件读不出来，表现为「会话损坏」）。
        let s = sample_session();
        let v = serde_json::to_value(&s).expect("应能序列化");
        for k in [
            "provider",
            "flow_id",
            "poll_token",
            "authorize_url",
            "expires_at",
            "poll_interval_sec",
            "created_at",
        ] {
            assert!(v.get(k).is_some(), "缺字段 {k}");
        }
    }

    #[test]
    fn random_hex_has_twice_the_requested_length() {
        let a = random_hex(32).expect("应能生成");
        assert_eq!(a.len(), 64, "32 字节 ⇒ 64 个 hex 字符");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        // 两次调用不该相同（这是轮询的 Bearer）。
        let b = random_hex(32).expect("应能生成");
        assert_ne!(a, b);
    }

    fn sample_session() -> LoginSession {
        LoginSession {
            provider: Provider::Zai,
            flow_id: "flow-abc".to_string(),
            poll_token: "deadbeef".to_string(),
            authorize_url: "https://example.invalid/auth".to_string(),
            expires_at: now_unix() + 600,
            poll_interval_sec: 2,
            created_at: now_unix(),
        }
    }

    /// 造一个临时 auth 目录（`<tmp>/<name>-<pid>/auths`）。
    ///
    /// ★ 返回的字符串以 `auths` 结尾 —— `sessions_dir` 取它的**同级**，所以每次
    /// 测试各占一个 `<name>-<pid>` 目录，互不干扰。
    fn temp_auth_dir(name: &str) -> String {
        let d = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let auths = d.join("auths");
        let _ = std::fs::create_dir_all(&auths);
        auths.to_string_lossy().to_string()
    }
}
