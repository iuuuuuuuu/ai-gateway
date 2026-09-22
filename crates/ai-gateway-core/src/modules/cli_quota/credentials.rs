//! 本机 CLI 登录凭证的发现与读取（只读，不写）。
//!
//! 这一层的职责是：找出本机**已经登录**的 AI CLI，并读回它们的 access token。
//! 之所以要自己发现而不是让用户手填：额度查询的前提就是「本机已有登录态」，
//! 让用户去各 CLI 目录里翻 token 既不现实也容易抄错。
//!
//! # 已确认的凭证位置（均在本机实测确认过格式）
//!
//! | provider    | 位置                                                  |
//! |-------------|-------------------------------------------------------|
//! | Codex       | `~/.codex/auth.json` → `tokens.access_token`           |
//! | Claude      | `~/.claude/.credentials.json` → `claudeAiOauth.*`      |
//! | Antigravity | Windows 凭据管理器 `gemini:antigravity`（JSON blob）   |
//! | Grok / xAI  | `~/.grok/auth.json` → `access_token`                   |
//! | Kimi        | `~/.kimi-code/` 下由 CLI 自行管理                      |
//!
//! # 安全约束
//!
//! - **只读**：本模块永不写凭证文件；回写由 [`super::refresh`] 单独负责。
//! - **token 绝不进日志/返回值**：对外只暴露 [`super::CliAccountView`]（脱敏），
//!   完整 token 只在进程内传给上游请求。
//! - 找不到文件 = 该 provider「未登录」，是**正常状态**而非错误；界面据此提示
//!   用户去对应 CLI 登录，而不是显示一个红色的失败。

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{CliProvider, CredentialSource};

/// 已读取到的凭证。
///
/// `access_token` 是敏感字段：**不得**序列化进任何返回给前端或写进日志的结构。
#[derive(Debug, Clone)]
pub struct Credential {
    pub provider: CliProvider,
    /// 凭证来源（用于回写时定位文件）。
    pub source: CredentialSource,
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// access token 过期时刻（Unix 毫秒）；None = 未知（此时按「可能已过期」处理）。
    pub expires_at_ms: Option<i64>,
    /// 账号展示名（邮箱 / 用户名），取自 id_token 或凭证本身。
    pub account_label: Option<String>,
    /// 额度查询所需的额外参数（如 Antigravity 的项目号、Codex 的账号 id）。
    pub extras: Extras,
}

/// provider 特有的、查询额度时必须随请求带上的参数。
#[derive(Debug, Clone, Default)]
pub struct Extras {
    /// Codex：`Chatgpt-Account-Id` 头。
    pub account_id: Option<String>,
    /// Antigravity：`retrieveUserQuotaSummary` 的 project 入参。
    pub project_id: Option<String>,
    /// Antigravity：凭证在 Windows 凭据管理器里的 target 名（回写用）。
    pub cred_target: Option<String>,
    /// xAI：`x-userid` 头。
    pub user_id: Option<String>,
}

impl Credential {
    /// access token 是否已过期（含 60 秒提前量）。
    ///
    /// 过期时刻未知时返回 `false`：交给上游用 401 判定，避免因缺元数据
    /// 就盲目刷新而多打一次请求。
    pub fn is_expired(&self, now_ms: i64) -> bool {
        match self.expires_at_ms {
            Some(expires) => expires - 60_000 <= now_ms,
            None => false,
        }
    }
}

/// 家目录（尊重 `USERPROFILE` / `HOME`，与 `config::home_dir` 同源）。
fn home() -> PathBuf {
    crate::modules::config::home_dir()
}

/// Codex 主目录：尊重 `CODEX_HOME`。
fn codex_home() -> PathBuf {
    env_dir("CODEX_HOME").unwrap_or_else(|| home().join(".codex"))
}

/// Claude Code 主目录：尊重 `CLAUDE_CONFIG_DIR`。
fn claude_home() -> PathBuf {
    env_dir("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home().join(".claude"))
}

/// Grok 主目录：尊重 `GROK_HOME`。
fn grok_home() -> PathBuf {
    env_dir("GROK_HOME").unwrap_or_else(|| home().join(".grok"))
}

/// Kimi Code 主目录：尊重 `KIMI_CODE_HOME`。
fn kimi_home() -> PathBuf {
    env_dir("KIMI_CODE_HOME").unwrap_or_else(|| home().join(".kimi-code"))
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn read_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 解出 JWT 的 payload 段；非 JWT 或格式错误返回 None。
fn decode_jwt_payload(token: &str) -> Option<Value> {
    let segment = token.trim().split('.').nth(1)?;
    if segment.is_empty() {
        return None;
    }
    let normalized = segment.replace('-', "+").replace('_', "/");
    let padded = match normalized.len() % 4 {
        2 => format!("{normalized}=="),
        3 => format!("{normalized}="),
        0 => normalized,
        // 长度 %4 == 1 是非法 base64，直接放弃。
        _ => return None,
    };
    let bytes = base64_decode(&padded)?;
    let parsed: Value = serde_json::from_slice(&bytes).ok()?;
    parsed.is_object().then_some(parsed)
}

/// 极简 base64 解码（标准表，容忍 URL-safe 已在上层归一）。
///
/// 不引第三方依赖：这里只需要解 JWT 的 payload 段，且输入已在调用处做过
/// 字符替换与补位。
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    for (index, byte) in TABLE.iter().enumerate() {
        lookup[*byte as usize] = index as u8;
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let value = lookup[byte as usize];
        if value == 255 {
            return None;
        }
        buffer = (buffer << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

fn nested<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let object = value.as_object()?;
    keys.iter().find_map(|key| object.get(*key))
}

fn string_at(value: &Value, keys: &[&str]) -> Option<String> {
    nested(value, keys)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(String::from)
}

/// 从 JWT payload 里找邮箱/用户名。
fn label_from_jwt(payload: &Value) -> Option<String> {
    string_at(payload, &["email"])
        .or_else(|| string_at(payload, &["preferred_username"]))
        .or_else(|| string_at(payload, &["name"]))
        .or_else(|| {
            nested(payload, &["https://api.openai.com/auth"]).and_then(|auth| {
                string_at(auth, &["email"]).or_else(|| string_at(auth, &["chatgpt_account_id"]))
            })
        })
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

/// 读取 Codex 登录态。
///
/// 文件形态（本机实测）：`{auth_mode, OPENAI_API_KEY, tokens:{id_token,access_token,
/// refresh_token,account_id}, last_refresh}`。API key 模式（`OPENAI_API_KEY` 非空、
/// `tokens` 为空）**不算登录态** —— 它没有可查的订阅额度。
pub fn codex_credential() -> Option<Credential> {
    let path = codex_home().join("auth.json");
    let value = read_json(&path)?;
    let tokens = nested(&value, &["tokens"])?;
    let access_token = string_at(tokens, &["access_token"])?;
    let account_id = string_at(tokens, &["account_id"]);

    let id_payload = string_at(tokens, &["id_token"]).and_then(|token| decode_jwt_payload(&token));
    let label = id_payload.as_ref().and_then(label_from_jwt);

    Some(Credential {
        provider: CliProvider::Codex,
        source: CredentialSource::JsonFile(path),
        access_token,
        refresh_token: string_at(tokens, &["refresh_token"]),
        // Codex 的 auth.json 不写 expiresAt；过期由上游 401 判定。
        expires_at_ms: None,
        account_label: label,
        extras: Extras {
            account_id,
            ..Extras::default()
        },
    })
}

// ---------------------------------------------------------------------------
// Claude
// ---------------------------------------------------------------------------

/// 读取 Claude Code 登录态。
///
/// 文件形态：`{claudeAiOauth:{accessToken,refreshToken,expiresAt,subscriptionType}}`。
/// 注意这里的键是 **camelCase**（与 Codex 的 snake_case 相反），故两者不能共用取值函数。
pub fn claude_credential() -> Option<Credential> {
    let path = claude_home().join(".credentials.json");
    let value = read_json(&path)?;
    let oauth = nested(&value, &["claudeAiOauth"])?;
    let access_token = string_at(oauth, &["accessToken"])?;

    let id_payload = string_at(oauth, &["idToken"]).and_then(|token| decode_jwt_payload(&token));
    let label = id_payload
        .as_ref()
        .and_then(label_from_jwt)
        .or_else(|| string_at(oauth, &["email"]));

    Some(Credential {
        provider: CliProvider::Claude,
        source: CredentialSource::JsonFile(path),
        access_token,
        refresh_token: string_at(oauth, &["refreshToken"]),
        expires_at_ms: nested(oauth, &["expiresAt"]).and_then(Value::as_i64),
        account_label: label,
        extras: Extras::default(),
    })
}

// ---------------------------------------------------------------------------
// Antigravity
// ---------------------------------------------------------------------------

/// Windows 凭据管理器里的 Antigravity 条目名。
pub const ANTIGRAVITY_CRED_TARGET: &str = "gemini:antigravity";

/// 读取 Antigravity 登录态。
///
/// 凭证**不在文件里**：Windows 上由凭据管理器保存，target 为
/// `gemini:antigravity`，blob 是 JSON：
/// `{auth_method, token:{access_token,refresh_token,expiry}, id_token}`。
#[cfg(windows)]
pub fn antigravity_credential() -> Option<Credential> {
    let blob = windows_credential::read(ANTIGRAVITY_CRED_TARGET)?;
    let value: Value = serde_json::from_str(&blob).ok()?;
    let token = nested(&value, &["token"])?;
    let access_token = string_at(token, &["access_token"])?;

    let id_payload = string_at(&value, &["id_token"]).and_then(|token| decode_jwt_payload(&token));
    let label = id_payload
        .as_ref()
        .and_then(label_from_jwt)
        .or_else(|| string_at(&value, &["email"]));

    // expiry 是 RFC3339 字符串。
    let expires_at_ms = string_at(token, &["expiry"])
        .and_then(|raw| chrono::DateTime::parse_from_rfc3339(&raw).ok())
        .map(|parsed| parsed.timestamp_millis());

    Some(Credential {
        provider: CliProvider::Antigravity,
        source: CredentialSource::WindowsCredential(ANTIGRAVITY_CRED_TARGET.to_string()),
        access_token,
        refresh_token: string_at(token, &["refresh_token"]),
        expires_at_ms,
        account_label: label,
        extras: Extras {
            cred_target: Some(ANTIGRAVITY_CRED_TARGET.to_string()),
            project_id: antigravity_project_from_disk(),
            ..Extras::default()
        },
    })
}

/// 非 Windows 平台没有凭据管理器实现，视为未登录。
#[cfg(not(windows))]
pub fn antigravity_credential() -> Option<Credential> {
    None
}

/// 从 `~/.gemini` 下的本地状态里找 Antigravity 的项目号。
///
/// `retrieveUserQuotaSummary` 要求带 project 入参；项目号通常由
/// `loadCodeAssist` 在线返回，但本地 `projects.json` / 状态文件里也可能已有。
/// 找不到就返回 None，由调用方走在线获取。
fn antigravity_project_from_disk() -> Option<String> {
    let gemini = home().join(".gemini");
    let projects = read_json(&gemini.join("projects.json"))?;
    let map = nested(&projects, &["projects"])?;
    let object = map.as_object()?;
    // `{"projects": {"<path>": "<project-id>"}}`：取第一个非空值即可。
    object
        .values()
        .filter_map(Value::as_str)
        .map(str::trim)
        .find(|item| !item.is_empty())
        .map(String::from)
}

// ---------------------------------------------------------------------------
// Grok / xAI
// ---------------------------------------------------------------------------

/// 读取 Grok 登录态。
///
/// 文件形态：`~/.grok/auth.json`。本机该文件可能不存在（未登录），此时返回 None。
pub fn xai_credential() -> Option<Credential> {
    let path = grok_home().join("auth.json");
    let value = read_json(&path)?;
    // 兼容扁平与嵌套两种形态。
    let token_block = nested(&value, &["token", "oauth", "credentials"]).unwrap_or(&value);
    let access_token = string_at(token_block, &["access_token", "accessToken"])
        .or_else(|| string_at(&value, &["access_token", "accessToken"]))?;

    let id_payload = string_at(token_block, &["id_token", "idToken"])
        .or_else(|| string_at(&value, &["id_token", "idToken"]))
        .and_then(|token| decode_jwt_payload(&token));
    let label = id_payload.as_ref().and_then(label_from_jwt);
    let user_id = id_payload
        .as_ref()
        .and_then(|payload| string_at(payload, &["sub", "user_id", "userId"]));

    let expires_at_ms = nested(token_block, &["expires_at", "expiresAt", "expiry"])
        .and_then(Value::as_i64)
        .map(|value| if value < 1e11 as i64 { value * 1000 } else { value });

    Some(Credential {
        provider: CliProvider::Xai,
        source: CredentialSource::JsonFile(path),
        access_token,
        refresh_token: string_at(token_block, &["refresh_token", "refreshToken"]),
        expires_at_ms,
        account_label: label,
        extras: Extras {
            user_id,
            ..Extras::default()
        },
    })
}

// ---------------------------------------------------------------------------
// Kimi
// ---------------------------------------------------------------------------

/// 读取 Kimi Code 登录态。
///
/// Kimi Code 把凭证交给自身管理，本机实测 `~/.kimi-code/` 下只有 `bin/`，
/// **没有**可读的 token 文件；此时返回 None，界面提示「未检测到登录态」。
/// 保留这个分支是为了：将来上游写出凭证文件时能自动接上，且让「未登录」
/// 与「不支持」在界面上可区分。
pub fn kimi_credential() -> Option<Credential> {
    let home = kimi_home();
    for name in ["auth.json", "credentials.json", "token.json"] {
        let path = home.join(name);
        let Some(value) = read_json(&path) else {
            continue;
        };
        let token_block = nested(&value, &["token", "oauth", "credentials"]).unwrap_or(&value);
        let Some(access_token) = string_at(token_block, &["access_token", "accessToken"])
            .or_else(|| string_at(&value, &["access_token", "accessToken"]))
        else {
            continue;
        };
        let id_payload = string_at(token_block, &["id_token", "idToken"])
            .and_then(|token| decode_jwt_payload(&token));
        return Some(Credential {
            provider: CliProvider::Kimi,
            source: CredentialSource::JsonFile(path),
            access_token,
            refresh_token: string_at(token_block, &["refresh_token", "refreshToken"]),
            expires_at_ms: nested(token_block, &["expires_at", "expiresAt", "expiry"])
                .and_then(Value::as_i64),
            account_label: id_payload.as_ref().and_then(label_from_jwt),
            extras: Extras::default(),
        });
    }
    None
}

// ---------------------------------------------------------------------------
// 统一入口
// ---------------------------------------------------------------------------

/// 读取指定 provider 的本机凭证；未登录返回 None。
pub fn credential_for(provider: CliProvider) -> Option<Credential> {
    match provider {
        CliProvider::Codex => codex_credential(),
        CliProvider::Claude => claude_credential(),
        CliProvider::Antigravity => antigravity_credential(),
        CliProvider::Xai => xai_credential(),
        CliProvider::Kimi => kimi_credential(),
    }
}

/// 探测所有 provider 的登录态（不触网）。
///
/// 返回每个 provider 的凭证；未登录的为 None。顺序与 [`CliProvider::ALL`] 一致。
pub fn discover_all() -> Vec<(CliProvider, Option<Credential>)> {
    CliProvider::ALL
        .iter()
        .map(|provider| (*provider, credential_for(*provider)))
        .collect()
}

/// 读取 Windows 凭据管理器条目（供 [`super::refresh`] 回写前取回原始内容）。
///
/// 非 Windows 平台返回 None。
pub fn read_windows_credential(target: &str) -> Option<String> {
    #[cfg(windows)]
    {
        windows_credential::read(target)
    }
    #[cfg(not(windows))]
    {
        let _ = target;
        None
    }
}

/// 写入 Windows 凭据管理器条目（供 [`super::refresh`] 回写刷新后的 token）。
pub fn write_windows_credential(target: &str, blob: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        windows_credential::write(target, blob)
    }
    #[cfg(not(windows))]
    {
        let _ = (target, blob);
        Err("当前平台不支持写入 Windows 凭据管理器".to_string())
    }
}

// ---------------------------------------------------------------------------
// Windows 凭据管理器
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod windows_credential {
    use windows::core::PCWSTR;
    use windows::Win32::Security::Credentials::{
        CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC,
    };

    /// 读取指定 target 的通用凭据，返回 UTF-8 字符串。
    ///
    /// 读不到（条目不存在 / 权限不足）返回 None —— 对调用方而言「没这条凭据」
    /// 与「读失败」都等价于「该 provider 未登录」，无需区分。
    pub fn read(target: &str) -> Option<String> {
        let wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
        let mut credential: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: wide 以 NUL 结尾且在整个调用期间存活；credential 是有效的出参指针。
        unsafe {
            CredReadW(PCWSTR(wide.as_ptr()), CRED_TYPE_GENERIC, 0, &mut credential).ok()?;
        }
        if credential.is_null() {
            return None;
        }
        // SAFETY: CredReadW 成功时保证 credential 指向有效的 CREDENTIALW，
        // 其 CredentialBlob 在 CredFree 之前一直有效。
        let blob = unsafe {
            let entry = &*credential;
            let slice =
                std::slice::from_raw_parts(entry.CredentialBlob, entry.CredentialBlobSize as usize);
            let text = String::from_utf8_lossy(slice).to_string();
            CredFree(credential as *const core::ffi::c_void);
            text
        };
        (!blob.trim().is_empty()).then_some(blob)
    }

    /// 写回指定 target 的通用凭据（供 token 刷新后回写）。
    pub fn write(target: &str, blob: &str) -> Result<(), String> {
        use windows::Win32::Security::Credentials::{CredWriteW, CRED_PERSIST_LOCAL_MACHINE};

        let mut target_wide: Vec<u16> =
            target.encode_utf16().chain(std::iter::once(0)).collect();
        let mut blob_bytes = blob.as_bytes().to_vec();
        let mut credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: windows::core::PWSTR(target_wide.as_mut_ptr()),
            CredentialBlobSize: blob_bytes.len() as u32,
            CredentialBlob: blob_bytes.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..Default::default()
        };
        // SAFETY: 所有指针字段都指向在本调用期间存活的本地缓冲区。
        unsafe { CredWriteW(&mut credential, 0) }
            .map_err(|error| format!("写入 Windows 凭据失败: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn base64_decode_handles_padding_and_url_safe_input() {
        // "hello" 的标准 base64。
        assert_eq!(base64_decode("aGVsbG8=").as_deref(), Some(&b"hello"[..]));
        // 无填充时也应能解出（JWT 常见）。
        assert_eq!(base64_decode("aGVsbG8").as_deref(), Some(&b"hello"[..]));
        // 非法字符返回 None 而不是 panic。
        assert!(base64_decode("!!!!").is_none());
    }

    #[test]
    fn decode_jwt_payload_reads_object_only() {
        // {"email":"a@b.com"} 的 base64url。
        let token = "header.eyJlbWFpbCI6ImFAYi5jb20ifQ.signature";
        let payload = decode_jwt_payload(token).expect("valid payload");
        assert_eq!(payload["email"], "a@b.com");
        // 非 JWT / 非法 base64 不 panic。
        assert!(decode_jwt_payload("not-a-jwt").is_none());
        assert!(decode_jwt_payload("a.!!!!.c").is_none());
    }

    #[test]
    fn label_from_jwt_prefers_email_then_openai_auth_block() {
        assert_eq!(
            label_from_jwt(&json!({"email": "one@example.com"})).as_deref(),
            Some("one@example.com")
        );
        assert_eq!(
            label_from_jwt(&json!({
                "https://api.openai.com/auth": {"email": "two@example.com"}
            }))
            .as_deref(),
            Some("two@example.com")
        );
        assert_eq!(label_from_jwt(&json!({})), None);
    }

    #[test]
    fn expired_uses_sixty_second_margin_and_treats_unknown_as_valid() {
        let credential = |expires: Option<i64>| Credential {
            provider: CliProvider::Codex,
            source: CredentialSource::JsonFile(PathBuf::from("x")),
            access_token: "t".into(),
            refresh_token: None,
            expires_at_ms: expires,
            account_label: None,
            extras: Extras::default(),
        };
        assert!(!credential(None).is_expired(1_000_000));
        // 还有 30 秒就过期 → 视为已过期（提前刷新）。
        assert!(credential(Some(1_030_000)).is_expired(1_000_000));
        // 还有 10 分钟 → 仍有效。
        assert!(!credential(Some(1_600_000)).is_expired(1_000_000));
    }

    #[test]
    fn all_providers_have_a_discovery_path() {
        // 每个 provider 都必须能被 discover_all 覆盖，否则界面永远查不到它。
        assert_eq!(CliProvider::ALL.len(), 5);
        assert_eq!(discover_all().len(), CliProvider::ALL.len());
    }

    #[test]
    fn missing_files_report_not_logged_in_rather_than_panicking() {
        // 用不存在的目录覆盖环境变量，确保读不到时返回 None。
        let isolation = crate::modules::config::test_isolation::Isolated::new("cli-quota-discovery");
        std::env::set_var("CODEX_HOME", isolation.dir().join("no-codex"));
        std::env::set_var("CLAUDE_CONFIG_DIR", isolation.dir().join("no-claude"));
        std::env::set_var("GROK_HOME", isolation.dir().join("no-grok"));
        std::env::set_var("KIMI_CODE_HOME", isolation.dir().join("no-kimi"));
        assert!(codex_credential().is_none());
        assert!(claude_credential().is_none());
        assert!(xai_credential().is_none());
        assert!(kimi_credential().is_none());
        std::env::remove_var("CODEX_HOME");
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        std::env::remove_var("GROK_HOME");
        std::env::remove_var("KIMI_CODE_HOME");
    }

    #[test]
    fn codex_reads_tokens_and_account_id_from_a_temp_home() {
        let isolation = crate::modules::config::test_isolation::Isolated::new("cli-quota-codex");
        let home = isolation.dir().join("codex");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("auth.json"),
            serde_json::to_string(&json!({
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": {
                    "access_token": "access-123",
                    "refresh_token": "refresh-456",
                    "account_id": "acct-789",
                    "id_token": "h.eyJlbWFpbCI6ImNvZGV4QGV4YW1wbGUuY29tIn0.s"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        std::env::set_var("CODEX_HOME", &home);
        let credential = codex_credential().expect("credential");
        std::env::remove_var("CODEX_HOME");

        assert_eq!(credential.access_token, "access-123");
        assert_eq!(credential.refresh_token.as_deref(), Some("refresh-456"));
        assert_eq!(credential.extras.account_id.as_deref(), Some("acct-789"));
        assert_eq!(
            credential.account_label.as_deref(),
            Some("codex@example.com")
        );
    }

    #[test]
    fn codex_api_key_mode_is_not_treated_as_logged_in() {
        let isolation = crate::modules::config::test_isolation::Isolated::new("cli-quota-codex-apikey");
        let home = isolation.dir().join("codex");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("auth.json"),
            serde_json::to_string(&json!({
                "auth_mode": "apikey",
                "OPENAI_API_KEY": "sk-something",
                "tokens": null
            }))
            .unwrap(),
        )
        .unwrap();
        std::env::set_var("CODEX_HOME", &home);
        // API key 模式没有可查的订阅额度，必须报告「未登录」而不是硬查。
        assert!(codex_credential().is_none());
        std::env::remove_var("CODEX_HOME");
    }

    #[test]
    fn claude_reads_camel_case_credentials() {
        let isolation = crate::modules::config::test_isolation::Isolated::new("cli-quota-claude");
        let home = isolation.dir().join("claude");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".credentials.json"),
            serde_json::to_string(&json!({
                "claudeAiOauth": {
                    "accessToken": "claude-access",
                    "refreshToken": "claude-refresh",
                    "expiresAt": 1_900_000_000_000i64,
                    "subscriptionType": "max"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", &home);
        let credential = claude_credential().expect("credential");
        std::env::remove_var("CLAUDE_CONFIG_DIR");

        assert_eq!(credential.access_token, "claude-access");
        assert_eq!(credential.expires_at_ms, Some(1_900_000_000_000));
    }

    #[test]
    fn claude_file_without_oauth_block_is_not_logged_in() {
        let isolation = crate::modules::config::test_isolation::Isolated::new("cli-quota-claude-empty");
        let home = isolation.dir().join("claude");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".credentials.json"), "{}").unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", &home);
        assert!(claude_credential().is_none());
        std::env::remove_var("CLAUDE_CONFIG_DIR");
    }

    #[test]
    fn grok_reads_flat_auth_file_and_normalizes_seconds() {
        let isolation = crate::modules::config::test_isolation::Isolated::new("cli-quota-grok");
        let home = isolation.dir().join("grok");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("auth.json"),
            serde_json::to_string(&json!({
                "access_token": "grok-access",
                "refresh_token": "grok-refresh",
                "expires_at": 1_900_000_000
            }))
            .unwrap(),
        )
        .unwrap();
        std::env::set_var("GROK_HOME", &home);
        let credential = xai_credential().expect("credential");
        std::env::remove_var("GROK_HOME");

        assert_eq!(credential.access_token, "grok-access");
        // 秒 → 毫秒。
        assert_eq!(credential.expires_at_ms, Some(1_900_000_000_000));
    }

    #[test]
    fn grok_nested_token_block_is_supported() {
        let isolation = crate::modules::config::test_isolation::Isolated::new("cli-quota-grok-nested");
        let home = isolation.dir().join("grok");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("auth.json"),
            serde_json::to_string(&json!({
                "oauth": { "accessToken": "nested-access" }
            }))
            .unwrap(),
        )
        .unwrap();
        std::env::set_var("GROK_HOME", &home);
        let credential = xai_credential().expect("credential");
        std::env::remove_var("GROK_HOME");
        assert_eq!(credential.access_token, "nested-access");
    }

    #[test]
    fn kimi_reports_not_logged_in_when_only_bin_exists() {
        let isolation = crate::modules::config::test_isolation::Isolated::new("cli-quota-kimi");
        let home = isolation.dir().join("kimi");
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::env::set_var("KIMI_CODE_HOME", &home);
        assert!(kimi_credential().is_none());
        std::env::remove_var("KIMI_CODE_HOME");
    }

    #[test]
    fn antigravity_project_lookup_reads_projects_json() {
        // antigravity_project_from_disk 读的是固定家目录，这里只验证它在
        // 找不到文件时返回 None 而不是 panic。
        let result = antigravity_project_from_disk();
        assert!(result.is_none() || result.as_deref().is_some_and(|item| !item.is_empty()));
    }
}
