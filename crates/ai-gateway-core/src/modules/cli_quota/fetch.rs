//! 调用各 provider 的额度接口并归一结果。
//!
//! 端点与请求头对照 EasyCLIProxyAPI `src/services/quotaService.ts` 的
//! `endpointByProvider` / `headersByProvider`；差别在于它把请求交给自带内核
//! （`/api-call` + `$TOKEN$` 占位符）代发，而这里由 Rust 直接发 —— 因为本仓库
//! 没有那个内核，token 本来就握在手里，多一层转发只会多一个故障点。
//!
//! # 设计约束
//!
//! - **只读**：本模块只发查询请求，不做任何写操作（重置次数等一律不消费）。
//! - **token 不落日志**：所有错误信息都经过 [`sanitize`] 清洗，防止上游把
//!   回显的 Authorization 头带进错误文案。
//! - **未登录/不支持 = 明确原因**：拿不到额度就返回 `Err(中文原因)`，
//!   绝不返回空窗口列表让界面显示成「没有额度」。

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;

use super::credentials::Credential;
use super::{parse, CliProvider, QuotaWindow, ResetCredit};

/// 单个 provider 的查询结果。
#[derive(Debug, Default)]
pub struct QuotaResult {
    pub windows: Vec<QuotaWindow>,
    pub plan: Option<String>,
    pub reset_credits: Option<ResetCredit>,
    pub subscription_active_until: Option<String>,
}

/// 额度查询的 HTTP 超时。
const QUERY_TIMEOUT_SECONDS: u64 = 25;

/// 各家额度端点。
///
/// Antigravity 有 3 个候选端点（daily / sandbox / prod）：实测 Google 侧会按
/// 环境灰度，单个域名可能返回空，故按顺序逐个尝试。
fn endpoints(provider: CliProvider) -> &'static [&'static str] {
    match provider {
        CliProvider::Codex => &["https://chatgpt.com/backend-api/wham/usage"],
        CliProvider::Claude => &["https://api.anthropic.com/api/oauth/usage"],
        CliProvider::Antigravity => &[
            "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary",
            "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal:retrieveUserQuotaSummary",
            "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary",
        ],
        CliProvider::Xai => &["https://cli-chat-proxy.grok.com/v1/billing"],
        CliProvider::Kimi => &["https://api.kimi.com/coding/v1/usages"],
    }
}

/// 该 provider 的请求方法（Antigravity 用 POST，其余 GET）。
fn uses_post(provider: CliProvider) -> bool {
    matches!(provider, CliProvider::Antigravity)
}

/// 组装请求头。
///
/// User-Agent 与各家 CLI 真实版本对齐：上游对 UA 有校验（实测 Codex 不带
/// 专用 UA 会被拒），这是「能查到额度」的必要条件之一。
fn headers(credential: &Credential) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    headers.insert(
        "Authorization".to_string(),
        format!("Bearer {}", credential.access_token),
    );
    match credential.provider {
        CliProvider::Codex => {
            headers.insert("Content-Type".to_string(), "application/json".to_string());
            headers.insert(
                "User-Agent".to_string(),
                "codex-tui/0.149.1 (Mac OS 26.5.2; arm64) iTerm.app/3.6.11 (codex-tui; 0.149.1)"
                    .to_string(),
            );
            if let Some(account_id) = &credential.extras.account_id {
                headers.insert("Chatgpt-Account-Id".to_string(), account_id.clone());
            }
        }
        CliProvider::Claude => {
            headers.insert("Content-Type".to_string(), "application/json".to_string());
            // Claude 的 OAuth 用量接口要求显式声明 beta。
            headers.insert("anthropic-beta".to_string(), "oauth-2025-04-20".to_string());
        }
        CliProvider::Antigravity => {
            headers.insert("Content-Type".to_string(), "application/json".to_string());
            headers.insert(
                "User-Agent".to_string(),
                "antigravity/cli/1.0.13 (aidev_client; os_type=darwin; arch=arm64)".to_string(),
            );
        }
        CliProvider::Xai => {
            headers.insert("x-xai-token-auth".to_string(), "xai-grok-cli".to_string());
            headers.insert("x-grok-client-version".to_string(), "0.2.91".to_string());
            headers.insert("accept".to_string(), "*/*".to_string());
            headers.insert(
                "user-agent".to_string(),
                "grok-pager/0.2.91 grok-shell/0.2.91 (macos; aarch64)".to_string(),
            );
            if let Some(user_id) = &credential.extras.user_id {
                headers.insert("x-userid".to_string(), user_id.clone());
            }
        }
        CliProvider::Kimi => {}
    }
    headers
}

/// 请求体（仅 Antigravity 需要，且必须带 project）。
fn body(credential: &Credential) -> Option<Value> {
    match credential.provider {
        CliProvider::Antigravity => credential
            .extras
            .project_id
            .as_ref()
            .map(|project| serde_json::json!({ "project": project })),
        _ => None,
    }
}

/// 清洗上游错误文案，防止把 token 回显出去。
///
/// 上游（尤其网关类）有时会把整个请求头写进错误体，直接透传等于把凭证
/// 打到界面上。这里做保守替换：抹掉任何 `Bearer <token>` 形态，再截断长度。
///
/// 实现上**逐段扫描而非反复 `replace_range`**：后者在替换结果里仍含有
/// `Bearer ` 时会原地打转（曾实测把测试挂死 60 秒以上）。
fn sanitize(message: &str) -> String {
    const MARKER: &str = "Bearer ";
    let mut cleaned = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(index) = rest.find(MARKER) {
        cleaned.push_str(&rest[..index]);
        cleaned.push_str("Bearer ***");
        let after = &rest[index + MARKER.len()..];
        // 跳过 token 本身：直到出现空白/引号/逗号等分隔符为止。
        let end = after
            .find(|character: char| {
                character.is_whitespace() || matches!(character, '"' | '\'' | ',' | '}' | ']')
            })
            .unwrap_or(after.len());
        rest = &after[end..];
    }
    cleaned.push_str(rest);

    let cleaned = cleaned.replace('\n', " ").trim().to_string();
    if cleaned.chars().count() > 200 {
        return cleaned.chars().take(200).collect::<String>() + "…";
    }
    cleaned
}

/// 该错误是否属于「登录态问题」（401/403）。
///
/// 与 [`crate::modules::cli_quota::refresh`] 的刷新失败组合使用：只有登录态
/// 类错误才值得把「刚刚刷新也失败了」的原因摆给用户看，否则会误导。
pub fn is_auth_error(message: &str) -> bool {
    message.contains("HTTP 401") || message.contains("HTTP 403")
}

/// 查询一个 provider 的额度。
pub async fn query(credential: &Credential) -> Result<QuotaResult, String> {    if credential.provider == CliProvider::Antigravity
        && credential.extras.project_id.is_none()
    {
        // 先尝试在线补项目号，再回落到查询。
        if let Some(project) = resolve_antigravity_project(credential).await {
            let mut patched = credential.clone();
            patched.extras.project_id = Some(project);
            return query_with(&patched).await;
        }
        return Err(
            "Antigravity 缺少项目号（project），无法查询额度：请先在本机打开一次 Antigravity 完成初始化"
                .to_string(),
        );
    }
    query_with(credential).await
}

/// 按端点顺序尝试查询，返回第一个能解析出窗口的结果。
async fn query_with(credential: &Credential) -> Result<QuotaResult, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(QUERY_TIMEOUT_SECONDS))
        .build()
        .map_err(|error| format!("HTTP 客户端创建失败: {error}"))?;

    let headers = headers(credential);
    let body = body(credential);
    let mut last_error = String::new();

    for url in endpoints(credential.provider) {
        let mut request = if uses_post(credential.provider) {
            client.post(*url)
        } else {
            client.get(*url)
        };
        for (name, value) in &headers {
            request = request.header(name, value);
        }
        if let Some(body) = &body {
            request = request.json(body);
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                last_error = format!("请求失败: {}", sanitize(&error.to_string()));
                continue;
            }
        };
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            last_error = format!(
                "上游返回 HTTP {}：{}",
                status.as_u16(),
                sanitize(&text.chars().take(300).collect::<String>())
            );
            continue;
        }

        let payload: Value = match serde_json::from_str(&text) {
            Ok(payload) => payload,
            Err(_) => {
                last_error = format!(
                    "上游响应不是合法 JSON：{}",
                    sanitize(&text.chars().take(200).collect::<String>())
                );
                continue;
            }
        };

        let mut result = normalize(credential.provider, &payload);
        if result.windows.is_empty() {
            last_error =
                "上游未返回可解析的额度数据（可能是套餐不支持该项查询）".to_string();
            continue;
        }
        // Claude / Antigravity 的套餐名要额外打一次接口，失败不影响额度展示。
        if result.plan.is_none() {
            result.plan = detect_plan(&client, credential, &headers).await;
        }
        return Ok(result);
    }

    Err(if last_error.is_empty() {
        "上游无响应".to_string()
    } else {
        last_error
    })
}

/// 把响应归一成 [`QuotaResult`]。
fn normalize(provider: CliProvider, payload: &Value) -> QuotaResult {
    match provider {
        CliProvider::Codex => {
            let (windows, reset_credits) = parse::codex(payload);
            QuotaResult {
                windows,
                plan: plan_from_codex(payload),
                reset_credits,
                subscription_active_until: subscription_from_codex(payload),
            }
        }
        CliProvider::Claude => QuotaResult {
            windows: parse::claude(payload),
            plan: None,
            reset_credits: None,
            subscription_active_until: None,
        },
        CliProvider::Antigravity => QuotaResult {
            windows: parse::antigravity(payload),
            plan: None,
            reset_credits: None,
            subscription_active_until: None,
        },
        CliProvider::Xai => QuotaResult {
            windows: parse::xai(payload),
            plan: None,
            reset_credits: None,
            subscription_active_until: None,
        },
        CliProvider::Kimi => QuotaResult {
            windows: parse::kimi(payload),
            plan: None,
            reset_credits: None,
            subscription_active_until: None,
        },
    }
}

/// Codex 用量响应里直接带的套餐名。
fn plan_from_codex(payload: &Value) -> Option<String> {
    payload
        .get("plan_type")
        .or_else(|| payload.get("planType"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| item.to_ascii_lowercase())
}

/// Codex 订阅到期时间。
fn subscription_from_codex(payload: &Value) -> Option<String> {
    payload
        .get("chatgpt_subscription_active_until")
        .or_else(|| payload.get("chatgptSubscriptionActiveUntil"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty() && *item != "0")
        .map(String::from)
}

/// Claude / Antigravity 的套餐名需要额外一次请求。
///
/// 失败一律返回 None：套餐只是锦上添花的展示，不能因为它让整次额度查询失败。
async fn detect_plan(
    client: &reqwest::Client,
    credential: &Credential,
    headers: &HashMap<String, String>,
) -> Option<String> {
    match credential.provider {
        CliProvider::Claude => {
            let mut request = client.get("https://api.anthropic.com/api/oauth/profile");
            for (name, value) in headers {
                request = request.header(name, value);
            }
            let response = request.send().await.ok()?;
            if !response.status().is_success() {
                return None;
            }
            let payload: Value = response.json().await.ok()?;
            parse::claude_plan(&payload)
        }
        CliProvider::Antigravity => {
            let response = client
                .post("https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist")
                .headers(to_header_map(headers))
                .json(&serde_json::json!({ "metadata": { "ideType": "ANTIGRAVITY" } }))
                .send()
                .await
                .ok()?;
            if !response.status().is_success() {
                return None;
            }
            let payload: Value = response.json().await.ok()?;
            parse::antigravity_plan(&payload).0
        }
        _ => None,
    }
}

/// `HashMap` → `reqwest::header::HeaderMap`。
fn to_header_map(headers: &HashMap<String, String>) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            map.insert(name, value);
        }
    }
    map
}

/// 在线获取 Antigravity 的项目号（`loadCodeAssist`）。
async fn resolve_antigravity_project(credential: &Credential) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .ok()?;
    let response = client
        .post("https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist")
        .headers(to_header_map(&headers(credential)))
        .json(&serde_json::json!({ "metadata": { "ideType": "ANTIGRAVITY" } }))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let payload: Value = response.json().await.ok()?;
    parse::antigravity_plan(&payload).1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::cli_quota::credentials::Extras;
    use crate::modules::cli_quota::CredentialSource;

    fn credential(provider: CliProvider) -> Credential {
        Credential {
            provider,
            source: CredentialSource::JsonFile(std::path::PathBuf::from("/tmp/auth.json")),
            access_token: "secret-token-value".into(),
            refresh_token: None,
            expires_at_ms: None,
            account_label: None,
            extras: Extras::default(),
        }
    }

    #[test]
    fn every_provider_has_at_least_one_endpoint() {
        for provider in CliProvider::ALL {
            assert!(
                !endpoints(provider).is_empty(),
                "{} 没有端点",
                provider.id()
            );
            assert!(endpoints(provider)
                .iter()
                .all(|url| url.starts_with("https://")));
        }
    }

    #[test]
    fn antigravity_has_fallback_endpoints_in_order() {
        let urls = endpoints(CliProvider::Antigravity);
        assert_eq!(urls.len(), 3);
        assert!(urls[0].contains("daily-cloudcode-pa"));
        assert!(urls[2].contains("cloudcode-pa.googleapis.com"));
    }

    #[test]
    fn only_antigravity_uses_post() {
        for provider in CliProvider::ALL {
            assert_eq!(uses_post(provider), provider == CliProvider::Antigravity);
        }
    }

    #[test]
    fn headers_carry_bearer_and_provider_specific_fields() {
        let codex = headers(&credential(CliProvider::Codex));
        assert_eq!(
            codex.get("Authorization").map(String::as_str),
            Some("Bearer secret-token-value")
        );
        assert!(codex.contains_key("User-Agent"));
        // 没有 account_id 时不应写入空头，否则上游会当成非法账号。
        assert!(!codex.contains_key("Chatgpt-Account-Id"));

        let claude = headers(&credential(CliProvider::Claude));
        assert_eq!(
            claude.get("anthropic-beta").map(String::as_str),
            Some("oauth-2025-04-20")
        );

        let xai = headers(&credential(CliProvider::Xai));
        assert_eq!(
            xai.get("x-xai-token-auth").map(String::as_str),
            Some("xai-grok-cli")
        );
        assert!(!xai.contains_key("x-userid"));
    }

    #[test]
    fn codex_account_id_header_is_added_when_present() {
        let mut credential = credential(CliProvider::Codex);
        credential.extras.account_id = Some("acct-1".into());
        let headers = headers(&credential);
        assert_eq!(
            headers.get("Chatgpt-Account-Id").map(String::as_str),
            Some("acct-1")
        );
    }

    #[test]
    fn xai_user_id_header_is_added_when_present() {
        let mut credential = credential(CliProvider::Xai);
        credential.extras.user_id = Some("user-1".into());
        let headers = headers(&credential);
        assert_eq!(headers.get("x-userid").map(String::as_str), Some("user-1"));
    }

    #[test]
    fn body_is_only_built_for_antigravity_with_project() {
        assert!(body(&credential(CliProvider::Codex)).is_none());
        let mut antigravity = credential(CliProvider::Antigravity);
        assert!(body(&antigravity).is_none());
        antigravity.extras.project_id = Some("proj-1".into());
        assert_eq!(body(&antigravity).unwrap()["project"], "proj-1");
    }

    #[test]
    fn sanitize_strips_bearer_tokens() {
        let cleaned = sanitize("denied for Bearer ya29.abcdefg123 and more");
        assert!(!cleaned.contains("ya29.abcdefg123"), "leaked: {cleaned}");
        assert!(cleaned.contains("Bearer ***"));
    }

    #[test]
    fn sanitize_strips_bearer_tokens_in_quotes() {
        let cleaned = sanitize(r#"{"error":"invalid Bearer sk-abc\"}"#);
        assert!(!cleaned.contains("sk-abc"), "leaked: {cleaned}");
    }

    #[test]
    fn sanitize_truncates_long_messages_and_collapses_newlines() {
        let cleaned = sanitize(&format!("line1\nline2 {}", "x".repeat(500)));
        assert!(!cleaned.contains('\n'));
        assert!(cleaned.chars().count() <= 201);
        assert!(cleaned.ends_with('…'));
    }

    #[test]
    fn sanitize_leaves_plain_messages_intact() {
        assert_eq!(sanitize("upstream unavailable"), "upstream unavailable");
    }

    #[test]
    fn normalize_returns_empty_for_unrecognized_shapes() {
        // 解析不出窗口时必须是空列表（由调用方报错），而不是伪造一个 0%。
        for provider in CliProvider::ALL {
            let result = normalize(provider, &serde_json::json!({"unexpected": true}));
            assert!(result.windows.is_empty(), "{}", provider.id());
        }
    }

    #[test]
    fn normalize_codex_extracts_plan_and_subscription() {
        let payload = serde_json::json!({
            "plan_type": "Plus",
            "chatgpt_subscription_active_until": "2030-01-01T00:00:00Z",
            "rate_limit": {
                "primary_window": { "used_percent": 10, "limit_window_seconds": 18000 }
            },
            "rate_limit_reset_credits": { "available_count": 3 }
        });
        let result = normalize(CliProvider::Codex, &payload);
        assert_eq!(result.windows.len(), 1);
        assert_eq!(result.plan.as_deref(), Some("plus"));
        assert_eq!(
            result.subscription_active_until.as_deref(),
            Some("2030-01-01T00:00:00Z")
        );
        assert_eq!(result.reset_credits.unwrap().available, Some(3));
    }

    #[test]
    fn normalize_codex_ignores_zero_subscription_marker() {
        let payload = serde_json::json!({
            "chatgpt_subscription_active_until": "0",
            "rate_limit": { "primary_window": { "used_percent": 1, "limit_window_seconds": 18000 } }
        });
        assert!(normalize(CliProvider::Codex, &payload)
            .subscription_active_until
            .is_none());
    }

    #[test]
    fn normalize_claude_and_antigravity_leave_plan_for_later_detection() {
        let claude = normalize(
            CliProvider::Claude,
            &serde_json::json!({"five_hour": {"utilization": 1}}),
        );
        assert_eq!(claude.windows.len(), 1);
        assert!(claude.plan.is_none());

        let antigravity = normalize(
            CliProvider::Antigravity,
            &serde_json::json!({"groups": [{"display_name": "G", "buckets": [
                {"window": "5h", "remaining_fraction": 0.5}
            ]}]}),
        );
        assert_eq!(antigravity.windows.len(), 1);
    }

    #[test]
    fn header_map_conversion_skips_invalid_entries() {
        let mut headers = HashMap::new();
        headers.insert("X-Ok".to_string(), "fine".to_string());
        headers.insert("Bad Header".to_string(), "value".to_string());
        headers.insert("X-Bad-Value".to_string(), "line\nbreak".to_string());
        let map = to_header_map(&headers);
        assert!(map.contains_key("x-ok"));
        assert!(!map.contains_key("bad header"));
        assert!(!map.contains_key("x-bad-value"));
    }

    #[tokio::test]
    async fn query_without_antigravity_project_reports_actionable_error() {
        // 无项目号且拿不到在线项目号时，必须给出可操作的中文原因。
        let credential = credential(CliProvider::Antigravity);
        let error = query(&credential).await.expect_err("must fail");
        assert!(
            error.contains("项目号") || error.contains("HTTP") || error.contains("请求失败"),
            "unexpected: {error}"
        );
    }
}
