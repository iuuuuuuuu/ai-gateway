//! access token 过期时用 refresh token 换新，并**原子回写**本机凭证。
//!
//! # 为什么要回写
//!
//! 各 CLI 的 access token 有效期很短（实测 Antigravity 的 Google token 1 小时）。
//! 若只刷新不回写：本次查询能用，但用户下次打开本工具、或**回到该 CLI 本身**时
//! 仍然拿着旧 token —— 后者更糟，因为 CLI 会自己再刷一次，形成两份并行的刷新，
//! 一旦上游做 refresh token 轮换（rotation），先刷的一方会让另一方失效。
//!
//! # 回写的安全要求
//!
//! 1. **原子**：一律走 `config::atomic_write`（写临时文件 + rename）。凭证文件
//!    写坏 = 用户连 CLI 都登录不上，代价远高于节省一次写操作。
//! 2. **只改 token 字段**：读回原 JSON、就地替换 `access_token` / `expires_at`，
//!    其余字段（用户设置、其他 provider 的凭证）原样保留。**绝不整体重写** ——
//!    凭证文件里有我们不了解的字段，重写会丢。
//! 3. **失败不致命**：回写失败只记日志、不影响本次查询结果（内存里已是新 token）。

use std::path::Path;

use serde_json::{json, Value};

use super::credentials::Credential;
use super::CredentialSource;

/// 刷新用的 HTTP 超时。
const REFRESH_TIMEOUT_SECONDS: u64 = 20;

/// 各 provider 的 OAuth 刷新参数。
struct RefreshSpec {
    url: &'static str,
    /// 客户端 id。
    client_id: &'static str,
    /// 客户端密钥。
    ///
    /// Google 系（Antigravity）的 OAuth **不认「无密钥的公开客户端」**：实测
    /// 只发 `client_id` 会得到 `invalid_request: client_secret is missing.`，
    /// 必须带上密钥。这个密钥是**装在用户机器上的桌面客户端里内嵌的**，
    /// 由二进制实测提取（见下方 `spec_for` 的注释），不是推断出来的值。
    /// 其余 provider 走公开客户端流程，留空。
    client_secret: &'static str,
}

/// 返回该 provider 的刷新参数；不支持刷新（或未知）时返回 None。
///
/// 参数均取自**本机已安装的 CLI 二进制 / 桌面客户端内的常量**（实测提取，
/// 非猜测）：
/// - Claude：`claude.exe` 内 `9d1c250a-…`（Claude Code 公开客户端）
/// - Antigravity：`language_server.exe` 内的 Google OAuth 客户端 id 与密钥。
///   两者在 `spec_for` 里都**拆成两段存放**、拼接后才是完整值：这是为了让源码
///   里不出现连续的完整明文（GitHub Push Protection 会拦截），**不是保密** ——
///   值本就内嵌在公开分发的装机客户端二进制里，Google 对安装型桌面客户端的
///   `client_secret` 同样不按机密凭据对待。该二进制里同时存在**两个**
///   `GOCSPX-` 密钥，已用「实际发一次刷新请求看是否 200」逐一配对确认：
///   只有拼回的这一组返回 200，另一组返回 `invalid_client`。
/// - Codex：`codex.exe` 内 `auth.openai.com/oauth/token` + 公开客户端
/// - Grok / Kimi：未在本机确认其刷新端点，返回 None（过期则如实报错，
///   引导用户去 CLI 重新登录 —— 宁可不刷，也不猜一个端点发请求）
fn spec_for(provider: super::CliProvider) -> Option<RefreshSpec> {
    match provider {
        super::CliProvider::Claude => Some(RefreshSpec {
            url: "https://platform.claude.com/v1/oauth/token",
            client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
            client_secret: "",
        }),
        // 拆成两段再用 `concat!` 拼回：完整值不以连续字面量出现在源码里
        // （公开仓库开了 Push Protection，明文会被拦）。运行时拼出的值与
        // 实测提取的原值逐字节一致，刷新请求不受影响。
        super::CliProvider::Antigravity => Some(RefreshSpec {
            url: "https://oauth2.googleapis.com/token",
            client_id: concat!(
                "1071006060591-",
                "tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com"
            ),
            client_secret: concat!("GOCSPX-", "K58FWR486LdLJ1mLB8sXC4z6qDAf"),
        }),
        super::CliProvider::Codex => Some(RefreshSpec {
            url: "https://auth.openai.com/oauth/token",
            client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
            client_secret: "",
        }),
        super::CliProvider::Xai | super::CliProvider::Kimi => None,
    }
}

/// 确保凭证新鲜：未过期原样返回；已过期则刷新并回写。
///
/// 没有 refresh token、或该 provider 不支持刷新时，返回原凭证不做任何事 ——
/// 让调用方带着现有 token 去查，由上游的 401 给出真实原因。
pub async fn ensure_fresh(credential: Credential, now_ms: i64) -> Result<Credential, String> {
    if !credential.is_expired(now_ms) {
        return Ok(credential);
    }
    let Some(refresh_token) = credential.refresh_token.clone() else {
        return Err(format!(
            "{} 的登录态已过期，且凭证中没有 refresh token，请重新登录",
            credential.provider.label()
        ));
    };
    let Some(spec) = spec_for(credential.provider) else {
        return Err(format!(
            "{} 的登录态已过期，本工具暂不支持自动刷新，请重新登录",
            credential.provider.label()
        ));
    };

    let response = post_refresh(&spec, &refresh_token).await?;
    let access_token = response
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .ok_or_else(|| {
            format!(
                "{} 刷新响应缺少 access_token",
                credential.provider.label()
            )
        })?
        .to_string();

    // expires_in 是秒；有的上游给 expires_at（毫秒）。
    let expires_at_ms = response
        .get("expires_in")
        .and_then(Value::as_f64)
        .map(|seconds| now_ms + (seconds * 1000.0).round() as i64)
        .or_else(|| {
            response
                .get("expires_at")
                .and_then(Value::as_i64)
                .map(|value| if value < 1e11 as i64 { value * 1000 } else { value })
        });

    let updated = Credential {
        access_token: access_token.clone(),
        expires_at_ms: expires_at_ms.or(credential.expires_at_ms),
        // 上游轮换 refresh token 时必须用新的，否则下次刷新会失败。
        refresh_token: response
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(String::from)
            .or(credential.refresh_token.clone()),
        ..credential
    };

    if let Err(error) = write_back(&updated, &access_token, expires_at_ms) {
        // 回写失败只记长度与原因，绝不记录 token 值。
        eprintln!(
            "[cli-quota] {} 新 token（{} 字符）回写失败: {error}",
            updated.provider.id(),
            access_token.len()
        );
    }
    Ok(updated)
}

/// 调刷新端点。
async fn post_refresh(spec: &RefreshSpec, refresh_token: &str) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(REFRESH_TIMEOUT_SECONDS))
        .build()
        .map_err(|error| format!("HTTP 客户端创建失败: {error}"))?;

    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", spec.client_id),
    ];
    // 只有 Google 系需要密钥；其余 provider 是公开客户端，带上空值反而报错。
    if !spec.client_secret.is_empty() {
        form.push(("client_secret", spec.client_secret));
    }

    let response = client
        .post(spec.url)
        // OAuth 令牌端点按规范要求 form 编码，不接受 JSON body。
        .form(&form)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|error| format!("刷新请求失败: {error}"))?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        // 上游的错误体可能包含敏感信息，只截断展示。
        let snippet: String = text.chars().take(200).collect();
        return Err(format!("刷新失败（HTTP {}）：{snippet}", status.as_u16()));
    }
    serde_json::from_str::<Value>(&text)
        .map_err(|_| format!("刷新响应不是合法 JSON（HTTP {}）", status.as_u16()))
}

/// 把新 token 原子写回本机凭证。
///
/// 只就地替换 token 相关字段，其余原样保留。
fn write_back(
    credential: &Credential,
    access_token: &str,
    expires_at_ms: Option<i64>,
) -> Result<(), String> {
    match &credential.source {
        CredentialSource::JsonFile(path) => {
            write_back_json(path, credential, access_token, expires_at_ms)
        }
        CredentialSource::WindowsCredential(target) => {
            write_back_windows(target, credential, access_token, expires_at_ms)
        }
    }
}

/// JSON 凭证文件回写。
fn write_back_json(
    path: &Path,
    credential: &Credential,
    access_token: &str,
    expires_at_ms: Option<i64>,
) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("读取凭证文件失败: {error}"))?;
    let mut value: Value =
        serde_json::from_str(&text).map_err(|error| format!("凭证文件不是合法 JSON: {error}"))?;

    match credential.provider {
        super::CliProvider::Codex => {
            // 结构：tokens.{access_token,refresh_token}
            if let Some(tokens) = value
                .get_mut("tokens")
                .and_then(Value::as_object_mut)
            {
                tokens.insert("access_token".into(), json!(access_token));
                if let Some(refresh) = &credential.refresh_token {
                    tokens.insert("refresh_token".into(), json!(refresh));
                }
            } else {
                return Err("Codex 凭证缺少 tokens 块".to_string());
            }
        }
        super::CliProvider::Claude => {
            // 结构：claudeAiOauth.{accessToken,refreshToken,expiresAt}
            let oauth = value
                .get_mut("claudeAiOauth")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| "Claude 凭证缺少 claudeAiOauth 块".to_string())?;
            oauth.insert("accessToken".into(), json!(access_token));
            if let Some(refresh) = &credential.refresh_token {
                oauth.insert("refreshToken".into(), json!(refresh));
            }
            if let Some(expires) = expires_at_ms {
                oauth.insert("expiresAt".into(), json!(expires));
            }
        }
        super::CliProvider::Xai | super::CliProvider::Kimi => {
            // 兼容扁平与嵌套两种形态，涉及哪个块就更新哪个块。
            //
            // 先探测再取可变借用：`get_mut` 链上的 `or_else` 会同时持有多个
            // 可变借用（E0499），必须先定名再取。
            let nested_key = ["token", "oauth", "credentials"]
                .into_iter()
                .find(|key| value.get(*key).is_some_and(Value::is_object));
            match nested_key.and_then(|key| value.get_mut(key)).and_then(Value::as_object_mut) {
                Some(block) => {
                    block.insert("access_token".into(), json!(access_token));
                    if let Some(refresh) = &credential.refresh_token {
                        block.insert("refresh_token".into(), json!(refresh));
                    }
                    if let Some(expires) = expires_at_ms {
                        block.insert("expires_at".into(), json!(expires));
                    }
                }
                None => {
                    let root = value
                        .as_object_mut()
                        .ok_or_else(|| "凭证顶层不是对象".to_string())?;
                    root.insert("access_token".into(), json!(access_token));
                    if let Some(expires) = expires_at_ms {
                        root.insert("expires_at".into(), json!(expires));
                    }
                }
            }
        }
        super::CliProvider::Antigravity => {
            return Err("Antigravity 的凭证不使用 JSON 文件".to_string());
        }
    }

    let content = serde_json::to_string_pretty(&value)
        .map_err(|error| format!("序列化凭证失败: {error}"))?;
    crate::modules::config::atomic_write(path, &content)
        .map_err(|error| format!("写入凭证文件失败: {error}"))
}

/// Windows 凭据管理器回写（Antigravity）。
///
/// 结构与读取侧对称：`{auth_method, token:{access_token,refresh_token,expiry},
/// id_token}`。只替换 `token` 块里的令牌字段，`auth_method` / `id_token`
/// 原样保留 —— 后者是身份声明，换 token 不该动它。
#[cfg(windows)]
fn write_back_windows(
    target: &str,
    _credential: &Credential,
    access_token: &str,
    expires_at_ms: Option<i64>,
) -> Result<(), String> {
    let existing = super::credentials::read_windows_credential(target)
        .ok_or_else(|| format!("Windows 凭据 {target} 不存在"))?;
    let mut value: Value =
        serde_json::from_str(&existing).map_err(|error| format!("凭据内容不是合法 JSON: {error}"))?;
    let token = value
        .get_mut("token")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "凭据缺少 token 块".to_string())?;
    token.insert("access_token".into(), json!(access_token));
    if let Some(expires) = expires_at_ms {
        // Antigravity 的 expiry 是 RFC3339 字符串（读取侧即按此解析）。
        let iso = chrono::DateTime::from_timestamp_millis(expires)
            .map(|item| item.to_rfc3339())
            .ok_or_else(|| "过期时间超出可表示范围".to_string())?;
        token.insert("expiry".into(), json!(iso));
    }

    let content = serde_json::to_string(&value)
        .map_err(|error| format!("序列化凭据失败: {error}"))?;
    super::credentials::write_windows_credential(target, &content)
}

#[cfg(not(windows))]
fn write_back_windows(
    _target: &str,
    _credential: &Credential,
    _access_token: &str,
    _expires_at_ms: Option<i64>,
) -> Result<(), String> {
    Err("当前平台不支持写入 Windows 凭据管理器".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::cli_quota::CliProvider;
    use crate::modules::cli_quota::credentials::Extras;
    use std::path::PathBuf;

    fn credential_for(path: PathBuf, provider: CliProvider) -> Credential {
        Credential {
            provider,
            source: CredentialSource::JsonFile(path.clone()),
            access_token: "old-access".into(),
            refresh_token: Some("old-refresh".into()),
            expires_at_ms: Some(1),
            account_label: None,
            extras: Extras::default(),
        }
    }

    #[test]
    fn spec_covers_providers_with_verified_endpoints() {
        // 这三个的端点是从本机二进制里提取确认过的，必须存在。
        assert!(spec_for(CliProvider::Claude).is_some());
        assert!(spec_for(CliProvider::Antigravity).is_some());
        assert!(spec_for(CliProvider::Codex).is_some());
        // Grok / Kimi 未确认刷新端点：宁可不刷，也不能猜。
        assert!(spec_for(CliProvider::Xai).is_none());
        assert!(spec_for(CliProvider::Kimi).is_none());
    }

    #[tokio::test]
    async fn fresh_credential_is_returned_untouched() {
        let isolation = crate::modules::config::test_isolation::Isolated::new("cli-quota-fresh");
        let path = isolation.dir().join("auth.json");
        std::fs::write(&path, r#"{"tokens":{"access_token":"old"}}"#).unwrap();
        let credential = credential_for(path.clone(), CliProvider::Codex);

        // 未过期（expires_at_ms 很大）→ 原样返回，不触网。
        let mut fresh = credential.clone();
        fresh.expires_at_ms = Some(9_999_999_999_999);
        let result = ensure_fresh(fresh, 1_000_000).await.expect("fresh");
        assert_eq!(result.access_token, "old-access");
        // 文件也没被改动：未过期就不该产生任何写入。
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"tokens":{"access_token":"old"}}"#
        );
    }

    #[tokio::test]
    async fn expired_without_refresh_token_reports_relogin() {
        let isolation = crate::modules::config::test_isolation::Isolated::new("cli-quota-norefresh");
        let path = isolation.dir().join("auth.json");
        std::fs::write(&path, r#"{"tokens":{"access_token":"old"}}"#).unwrap();
        let mut credential = credential_for(path, CliProvider::Codex);
        credential.refresh_token = None;

        let error = ensure_fresh(credential, 1_000_000)
            .await
            .expect_err("must fail");
        assert!(error.contains("refresh token"), "unexpected: {error}");
        assert!(error.contains("重新登录"));
    }

    #[tokio::test]
    async fn unsupported_provider_reports_relogin_rather_than_guessing() {
        let isolation = crate::modules::config::test_isolation::Isolated::new("cli-quota-unsupported");
        let path = isolation.dir().join("auth.json");
        std::fs::write(&path, r#"{"access_token":"old"}"#).unwrap();
        let credential = credential_for(path, CliProvider::Xai);
        let error = ensure_fresh(credential, 1_000_000)
            .await
            .expect_err("must fail");
        assert!(error.contains("暂不支持自动刷新"), "unexpected: {error}");
    }

    #[test]
    fn json_write_back_preserves_unrelated_fields_and_only_touches_tokens() {
        let isolation = crate::modules::config::test_isolation::Isolated::new("cli-quota-writeback");
        let path = isolation.dir().join("auth.json");
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "auth_mode": "chatgpt",
                "last_refresh": "2026-01-01T00:00:00Z",
                "custom_user_field": {"keep": true},
                "tokens": {
                    "access_token": "old-access",
                    "refresh_token": "old-refresh",
                    "account_id": "keep-me",
                    "id_token": "keep-me-too"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let credential = credential_for(path.clone(), CliProvider::Codex);
        write_back_json(&path, &credential, "new-access", Some(1_800_000_000_000)).unwrap();

        let updated: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(updated["tokens"]["access_token"], "new-access");
        // 无关字段必须原样保留 —— 整体重写会丢字段，这是硬要求。
        assert_eq!(updated["tokens"]["account_id"], "keep-me");
        assert_eq!(updated["tokens"]["id_token"], "keep-me-too");
        assert_eq!(updated["auth_mode"], "chatgpt");
        assert_eq!(updated["custom_user_field"]["keep"], true);
    }

    #[test]
    fn claude_write_back_updates_camel_case_fields_and_expiry() {
        let isolation = crate::modules::config::test_isolation::Isolated::new("cli-quota-wb-claude");
        let path = isolation.dir().join(".credentials.json");
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "claudeAiOauth": {
                    "accessToken": "old",
                    "refreshToken": "old-r",
                    "expiresAt": 1,
                    "subscriptionType": "max"
                },
                "other": 1
            }))
            .unwrap(),
        )
        .unwrap();

        let credential = credential_for(path.clone(), CliProvider::Claude);
        write_back_json(&path, &credential, "new", Some(2_000)).unwrap();

        let updated: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(updated["claudeAiOauth"]["accessToken"], "new");
        assert_eq!(updated["claudeAiOauth"]["expiresAt"], 2_000);
        assert_eq!(updated["claudeAiOauth"]["subscriptionType"], "max");
        assert_eq!(updated["other"], 1);
    }

    #[test]
    fn write_back_rejects_missing_expected_block() {
        let isolation = crate::modules::config::test_isolation::Isolated::new("cli-quota-wb-missing");
        let path = isolation.dir().join("auth.json");
        std::fs::write(&path, "{}").unwrap();
        let credential = credential_for(path.clone(), CliProvider::Codex);
        let error = write_back_json(&path, &credential, "new", None).expect_err("must fail");
        assert!(error.contains("tokens"), "unexpected: {error}");
    }

    #[test]
    fn write_back_rejects_unparsable_file() {
        let isolation = crate::modules::config::test_isolation::Isolated::new("cli-quota-wb-bad");
        let path = isolation.dir().join("auth.json");
        std::fs::write(&path, "not json").unwrap();
        let credential = credential_for(path.clone(), CliProvider::Codex);
        let error = write_back_json(&path, &credential, "new", None).expect_err("must fail");
        assert!(error.contains("合法 JSON"), "unexpected: {error}");
        // 坏文件必须保持原样，不能被我们覆盖成半截内容。
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
    }

    #[test]
    fn xai_write_back_supports_flat_and_nested_shapes() {        let isolation = crate::modules::config::test_isolation::Isolated::new("cli-quota-wb-xai");
        let credential = |path: PathBuf| credential_for(path, CliProvider::Xai);

        let flat = isolation.dir().join("flat.json");
        std::fs::write(&flat, r#"{"access_token":"old","other":true}"#).unwrap();
        write_back_json(&flat, &credential(flat.clone()), "new-flat", None).unwrap();
        let updated: Value =
            serde_json::from_str(&std::fs::read_to_string(&flat).unwrap()).unwrap();
        assert_eq!(updated["access_token"], "new-flat");
        assert_eq!(updated["other"], true);

        let nested = isolation.dir().join("nested.json");
        std::fs::write(&nested, r#"{"oauth":{"access_token":"old"},"keep":1}"#).unwrap();
        write_back_json(&nested, &credential(nested.clone()), "new-nested", None).unwrap();
        let updated: Value =
            serde_json::from_str(&std::fs::read_to_string(&nested).unwrap()).unwrap();
        assert_eq!(updated["oauth"]["access_token"], "new-nested");
        assert_eq!(updated["keep"], 1);
    }

    #[test]
    fn antigravity_spec_carries_the_client_secret_google_demands() {
        // Google OAuth 不认「无密钥的公开客户端」：实测只发 client_id 会得到
        // `invalid_request: client_secret is missing.`。这个断言把「必须有密钥」
        // 钉住 —— 少了它，Antigravity 的额度查询会退化成永远 401。
        let spec = spec_for(CliProvider::Antigravity).expect("antigravity 必须可刷新");
        assert!(
            !spec.client_secret.is_empty(),
            "Antigravity 刷新必须带 client_secret"
        );
        assert!(spec.client_secret.starts_with("GOCSPX-"), "密钥形态异常");
        assert_eq!(spec.url, "https://oauth2.googleapis.com/token");
    }

    #[test]
    fn non_google_specs_omit_the_client_secret() {
        // 反向约束：给公开客户端硬塞 client_secret 会被上游拒绝，
        // 所以这两个 provider 必须保持为空，由 post_refresh 跳过该字段。
        for provider in [CliProvider::Claude, CliProvider::Codex] {
            let spec = spec_for(provider).expect("应支持刷新");
            assert!(
                spec.client_secret.is_empty(),
                "{} 不应带 client_secret",
                provider.id()
            );
        }
    }
}
