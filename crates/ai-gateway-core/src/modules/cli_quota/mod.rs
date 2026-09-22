//! 本机 AI CLI 账号的**登录额度查询**。
//!
//! # 这个模块解决什么问题
//!
//! 本工具此前只能看到 WorkBuddy / 豆包 / Trae 三家的额度，用户在本机登录的
//! Codex、Claude、Antigravity、Grok、Kimi 这些 CLI **还剩多少额度**是黑盒 ——
//! 只能各自去对应 CLI 里翻。本模块把它们的登录态读出来，直接问各家上游，
//! 归一成统一的窗口列表给界面。
//!
//! 功能对照 EasyCLIProxyAPI 的 `QuotaPage` / `quotaService`：那套实现依赖它自带的
//! Go 内核通过 `/api-call` 以 `$TOKEN$` 占位符代发请求；本仓库没有该内核，因此
//! 改为**直接读本机凭证 + Rust 原生请求**，去掉中间层。
//!
//! # 分层
//!
//! | 子模块             | 职责                                       |
//! |--------------------|--------------------------------------------|
//! | [`credentials`]    | 找凭证、读凭证（只读，不写）               |
//! | [`refresh`]        | access token 过期时用 refresh token 换新并回写 |
//! | [`fetch`]          | 调各家上游额度接口                          |
//! | [`parse`]          | 把各家响应归一成 [`QuotaWindow`]（纯函数）  |
//!
//! # 安全约束（三条硬规则）
//!
//! 1. **token 不出进程**：对外只暴露 [`CliAccountView`]，其中没有任何 token 字段；
//!    完整 token 只在 [`fetch`] 内部拼进请求头。日志里一律只记长度。
//! 2. **未登录不是错误**：本机没登录某个 CLI 是常态，返回 `loggedIn: false` +
//!    可操作提示，不返回红色失败。
//! 3. **回写必须原子**：刷新后的 token 用 `config::atomic_write` 落盘，避免
//!    写一半掉电导致凭证文件损坏（那会让用户下次连 CLI 都登录不上）。

pub mod credentials;
pub mod fetch;
pub mod parse;
pub mod refresh;

use std::path::Path;
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use serde_json::{json, Value};

use crate::modules::config;

/// 支持的 CLI provider。
///
/// 这 5 家都是**实测本机存在凭证或安装**的：Codex / Claude / Antigravity / Grok
/// 有可读凭证，Kimi 目前 CLI 自管凭证（未登录时如实报告）。
/// 刻意不做 Devin —— 它需要 Codeium 的 API key，本机没有该 CLI，接了也无法验证。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CliProvider {
    Codex,
    Claude,
    Antigravity,
    Xai,
    Kimi,
}

impl CliProvider {
    /// 全部 provider；界面按此顺序分组展示。
    pub const ALL: [CliProvider; 5] = [
        CliProvider::Claude,
        CliProvider::Antigravity,
        CliProvider::Codex,
        CliProvider::Xai,
        CliProvider::Kimi,
    ];

    /// 稳定的机器标识（前端键、缓存键、Tauri 入参都用它）。
    pub fn id(&self) -> &'static str {
        match self {
            CliProvider::Codex => "codex",
            CliProvider::Claude => "claude",
            CliProvider::Antigravity => "antigravity",
            CliProvider::Xai => "xai",
            CliProvider::Kimi => "kimi",
        }
    }

    /// 界面展示名。
    pub fn label(&self) -> &'static str {
        match self {
            CliProvider::Codex => "Codex",
            CliProvider::Claude => "Claude",
            CliProvider::Antigravity => "Antigravity",
            CliProvider::Xai => "Grok",
            CliProvider::Kimi => "Kimi",
        }
    }

    /// 未登录时引导用户去哪里登录。
    pub fn login_hint(&self) -> &'static str {
        match self {
            CliProvider::Codex => "在本机运行 `codex login` 完成 ChatGPT 登录后再刷新",
            CliProvider::Claude => "在本机运行 `claude` 并完成登录后再刷新",
            CliProvider::Antigravity => "在本机登录 Antigravity 桌面端后再刷新",
            CliProvider::Xai => "在本机运行 `grok login` 完成登录后再刷新",
            CliProvider::Kimi => "在本机运行 `kimi` 并完成登录后再刷新",
        }
    }

    /// 从字符串解析 provider（Tauri / HTTP 入参）。
    pub fn parse(value: &str) -> Option<CliProvider> {
        let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
        CliProvider::ALL
            .iter()
            .copied()
            .find(|provider| provider.id() == normalized)
    }
}

/// 凭证的存放位置；刷新后据此回写。
#[derive(Debug, Clone)]
pub enum CredentialSource {
    /// JSON 文件（Codex / Claude / Grok / Kimi）。
    JsonFile(std::path::PathBuf),
    /// Windows 凭据管理器条目（Antigravity）。
    WindowsCredential(String),
}

impl CredentialSource {
    /// 稳定的来源标识，用于生成账号 id 与缓存键。
    ///
    /// 刻意不用 pid / 时间戳：同一次安装的凭证必须在多次启动间得到**同一个**
    /// 账号 id，否则界面上的缓存与「上次刷新」会每次重启都错位。
    fn key(&self) -> String {
        match self {
            CredentialSource::JsonFile(path) => path.to_string_lossy().to_ascii_lowercase(),
            CredentialSource::WindowsCredential(target) => format!("cred://{target}"),
        }
    }

    /// 面向用户的来源说明（排查用；不含 token）。
    fn describe(&self) -> String {
        match self {
            CredentialSource::JsonFile(path) => path.to_string_lossy().to_string(),
            CredentialSource::WindowsCredential(target) => {
                format!("Windows 凭据管理器 · {target}")
            }
        }
    }
}

/// 单个额度窗口（归一后的统一形态）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaWindow {
    /// 窗口展示名，如「5 小时」「每周」。
    pub label: String,
    /// 剩余百分比（0..100）；`null` = 上游没给出可比口径，界面显示「—」。
    ///
    /// **绝不用 0 代替 null**：0 代表「已用尽」，把未知显示成用尽会误导用户。
    pub remaining_percent: Option<f64>,
    /// 重置时刻（Unix 毫秒）。
    pub reset_at_ms: Option<i64>,
    /// 附注（如「$25.00 / $100.00」）。
    pub detail: Option<String>,
}

/// Codex 的手动重置次数。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetCredit {
    /// 可用重置次数（上游没给则为 None）。
    pub available: Option<u32>,
    /// 对本账号适用的次数。
    pub applicable: Option<u32>,
    /// 最早过期时刻（Unix 毫秒）。
    pub earliest_expiry_ms: Option<i64>,
}

/// 单个 provider 账号的额度视图（**不含任何 token**）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliAccountView {
    /// 稳定账号 id：`<provider>:<来源>`。
    pub id: String,
    pub provider: &'static str,
    /// provider 展示名。
    pub provider_label: &'static str,
    /// 账号展示名（邮箱 / 用户名）；查不到时回落 provider 名。
    pub label: String,
    /// 本机是否已有登录态。
    pub logged_in: bool,
    /// 凭证来源（排查用，不含 token）。
    pub source: String,
    /// 套餐名（如 Max / Pro）。
    pub plan: Option<String>,
    /// 额度窗口列表。
    pub windows: Vec<QuotaWindow>,
    /// 失败原因；成功时为 None。
    pub error: Option<String>,
    /// 本次查询时间（Unix 毫秒）。
    pub fetched_at: i64,
    /// Codex 手动重置次数。
    pub reset_credits: Option<ResetCredit>,
    /// 订阅到期时间（ISO 字符串，上游原样透传）。
    pub subscription_active_until: Option<String>,
}

impl CliAccountView {
    /// 「未登录」视图：provider 存在但本机没有凭证。
    fn not_logged_in(provider: CliProvider) -> Self {
        Self {
            id: format!("{}:none", provider.id()),
            provider: provider.id(),
            provider_label: provider.label(),
            label: provider.label().to_string(),
            logged_in: false,
            source: String::new(),
            plan: None,
            windows: Vec::new(),
            // 未登录用可操作提示，而不是「查询失败」。
            error: Some(format!("未检测到本机登录态：{}", provider.login_hint())),
            fetched_at: config::now_ms(),
            reset_credits: None,
            subscription_active_until: None,
        }
    }

    /// 失败视图。
    fn failed(credential: &credentials::Credential, account_id: String, error: String) -> Self {
        Self {
            id: account_id,
            provider: credential.provider.id(),
            provider_label: credential.provider.label(),
            label: credential
                .account_label
                .clone()
                .unwrap_or_else(|| credential.provider.label().to_string()),
            logged_in: true,
            source: credential.source.describe(),
            plan: None,
            windows: Vec::new(),
            error: Some(error),
            fetched_at: config::now_ms(),
            reset_credits: None,
            subscription_active_until: None,
        }
    }
}

/// 账号 id：`<provider>:<凭证来源>`。
fn account_id(credential: &credentials::Credential) -> String {
    format!(
        "{}:{}",
        credential.provider.id(),
        credential.source.key()
    )
}

// ---------------------------------------------------------------------------
// 缓存（内存 + 磁盘）
// ---------------------------------------------------------------------------

/// 额度缓存文件（`<store_dir>/cli_quota_cache.json`）。
fn cache_file() -> std::path::PathBuf {
    config::store_dir().join("cli_quota_cache.json")
}

fn memory() -> &'static Mutex<Option<Vec<CliAccountView>>> {
    static MEMORY: OnceLock<Mutex<Option<Vec<CliAccountView>>>> = OnceLock::new();
    MEMORY.get_or_init(|| Mutex::new(None))
}

/// 反序列化用的中间形态。
///
/// 与 [`CliAccountView`] 分开定义：写回磁盘的是它自己序列化出来的结构，
/// 读取时字段缺失/多出都必须能容忍（旧版本缓存、手改过的文件）。
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedView {
    #[serde(default)]
    id: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    logged_in: bool,
    #[serde(default)]
    source: String,
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    windows: Vec<CachedWindow>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    fetched_at: i64,
    #[serde(default)]
    reset_credits: Option<ResetCredit>,
    #[serde(default)]
    subscription_active_until: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedWindow {
    #[serde(default)]
    label: String,
    #[serde(default)]
    remaining_percent: Option<f64>,
    #[serde(default)]
    reset_at_ms: Option<i64>,
    #[serde(default)]
    detail: Option<String>,
}

fn load_cache_from(path: &Path) -> Option<Vec<CliAccountView>> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let payload = value.get("accounts").cloned().unwrap_or(value);
    let entries: Vec<CachedView> = serde_json::from_value(payload).ok()?;
    Some(
        entries
            .into_iter()
            .filter(|entry| !entry.id.is_empty())
            .map(|entry| CliAccountView {
                id: entry.id,
                // `provider` 存的是 &'static str；从缓存读回时按 id 反查，
                // 查不到就退回 "kimi" 之外的已知集合首项是不对的 —— 直接用
                // 泄漏的固定字符串集合里的匹配项，保证引用生命周期。
                provider: provider_static(&entry.provider),
                provider_label: provider_label_static(&entry.provider),
                label: entry.label,
                logged_in: entry.logged_in,
                source: entry.source,
                plan: entry.plan,
                windows: entry
                    .windows
                    .into_iter()
                    .map(|window| QuotaWindow {
                        label: window.label,
                        remaining_percent: window.remaining_percent,
                        reset_at_ms: window.reset_at_ms,
                        detail: window.detail,
                    })
                    .collect(),
                error: entry.error,
                fetched_at: entry.fetched_at,
                reset_credits: entry.reset_credits,
                subscription_active_until: entry.subscription_active_until,
            })
            .collect(),
    )
}

/// 把 provider id 映射回 `&'static str`（只接受已知值，防止缓存文件伪造出任意值）。
fn provider_static(id: &str) -> &'static str {
    CliProvider::parse(id)
        .map(|provider| provider.id())
        .unwrap_or("unknown")
}

fn provider_label_static(id: &str) -> &'static str {
    CliProvider::parse(id)
        .map(|provider| provider.label())
        .unwrap_or("未知")
}

fn save_cache_to(path: &Path, views: &[CliAccountView]) {
    let body = json!({ "accounts": views });
    let content = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = config::atomic_write(path, &content);
}

/// 读缓存：先内存后磁盘；都没有返回 None。
pub fn cached_views() -> Option<Vec<CliAccountView>> {
    if let Ok(guard) = memory().lock() {
        if let Some(views) = guard.as_ref() {
            return Some(views.clone());
        }
    }
    let loaded = load_cache_from(&cache_file())?;
    if let Ok(mut guard) = memory().lock() {
        *guard = Some(loaded.clone());
    }
    Some(loaded)
}

fn remember(views: &[CliAccountView]) {
    if let Ok(mut guard) = memory().lock() {
        *guard = Some(views.to_vec());
    }
    save_cache_to(&cache_file(), views);
}

/// 查询全部 provider 的本机账号额度。
///
/// `refresh = false` 时优先返回上次结果（含磁盘缓存），避免每次打开页面都打上游；
/// `refresh = true` 时强制重查。**未登录的 provider 也会出现在结果里** ——
/// 界面需要按 provider 分组，缺项会让用户以为「这个 provider 不支持」。
pub async fn fetch_all(refresh: bool) -> Vec<CliAccountView> {
    if !refresh {
        if let Some(views) = cached_views() {
            // 缓存里已覆盖全部 provider 才直接返回；否则补齐全集，
            // 否则新版本新增的 provider 会一直不出现。
            if views.len() >= CliProvider::ALL.len() {
                return views;
            }
        }
    }

    let mut views: Vec<CliAccountView> = Vec::new();
    for provider in CliProvider::ALL {
        match credentials::credential_for(provider) {
            Some(credential) => {
                views.push(fetch_one_with(&credential, refresh).await);
            }
            None => views.push(CliAccountView::not_logged_in(provider)),
        }
    }
    remember(&views);
    views
}

/// 只查一个 provider（界面上的单卡片刷新）。
///
/// 返回 None 表示该 provider 本机未登录。
pub async fn fetch_provider(provider: CliProvider, refresh: bool) -> Option<CliAccountView> {
    let credential = credentials::credential_for(provider)?;
    let view = fetch_one_with(&credential, refresh).await;
    // 合并进缓存，保持其它 provider 的既有结果不动。
    let mut views = cached_views().unwrap_or_else(|| {
        CliProvider::ALL
            .iter()
            .map(|item| CliAccountView::not_logged_in(*item))
            .collect()
    });
    upsert(&mut views, view.clone());
    remember(&views);
    Some(view)
}

fn upsert(views: &mut Vec<CliAccountView>, view: CliAccountView) {
    // 同一 provider 只保留一条：凭证变了（换了账号）时旧记录必须被替换，
    // 否则界面会同时列出新旧两个账号。
    views.retain(|item| item.provider != view.provider);
    views.push(view);
    // 按 ALL 顺序重排，保证界面分组顺序稳定。
    views.sort_by_key(|item| {
        CliProvider::ALL
            .iter()
            .position(|provider| provider.id() == item.provider)
            .unwrap_or(usize::MAX)
    });
}

/// 带凭证的查询：按需刷新 token，再调上游。
async fn fetch_one_with(credential: &credentials::Credential, _refresh: bool) -> CliAccountView {
    let account_id = account_id(credential);
    let now = config::now_ms();

    // 先按需刷新，再查询。刷新失败不直接判死：有些上游的 access token
    // 仍然可用（只是本地记录的过期时间不准），让查询自己去撞 401 更省一次往返。
    // 但要**记下失败原因** —— 否则用户只看到「HTTP 401」，不知道是刷新挂了。
    let (credential, refresh_error) = match refresh::ensure_fresh(credential.clone(), now).await {
        Ok(updated) => (updated, None),
        Err(error) => {
            eprintln!("[cli-quota] {account_id} token 刷新失败: {error}");
            (credential.clone(), Some(error))
        }
    };

    match fetch::query(&credential).await {
        Ok(result) => {
            // 空窗口 = 上游没返回可用口径。必须报错而不是显示空卡片，
            // 否则用户以为「额度是空的」而不是「没查到」。
            let error = result
                .windows
                .is_empty()
                .then(|| "上游未返回可解析的额度数据（可能是套餐不支持该项查询）".to_string());
            CliAccountView {
                id: account_id,
                provider: credential.provider.id(),
                provider_label: credential.provider.label(),
                label: credential
                    .account_label
                    .clone()
                    .unwrap_or_else(|| credential.provider.label().to_string()),
                logged_in: true,
                source: credential.source.describe(),
                plan: result.plan,
                windows: result.windows,
                error,
                fetched_at: now,
                reset_credits: result.reset_credits,
                subscription_active_until: result.subscription_active_until,
            }
        }
        Err(error) => {
            // 401/403 且我们刚刚刷新失败时，把刷新原因一并说出来 —— 只说
            // 「HTTP 401」会让用户以为是额度查询接口的问题，实际是登录态失效。
            let combined = match (&refresh_error, fetch::is_auth_error(&error)) {
                (Some(cause), true) => format!("登录态已失效（{cause}），请重新登录后重试"),
                _ => error,
            };
            CliAccountView::failed(&credential, account_id, combined)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_round_trip_and_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for provider in CliProvider::ALL {
            assert!(
                seen.insert(provider.id()),
                "provider id 必须唯一: {}",
                provider.id()
            );
            assert_eq!(CliProvider::parse(provider.id()), Some(provider));
            assert!(!provider.label().is_empty());
            assert!(!provider.login_hint().is_empty());
        }
        assert_eq!(seen.len(), 5);
    }

    #[test]
    fn provider_parse_tolerates_case_and_snake_case() {
        assert_eq!(CliProvider::parse("XAI"), Some(CliProvider::Xai));
        assert_eq!(CliProvider::parse(" grok "), None);
        assert_eq!(
            CliProvider::parse("anti_gravity_"),
            None,
            "尾部下划线不属于已知 id"
        );
        assert_eq!(CliProvider::parse("codex"), Some(CliProvider::Codex));
        assert_eq!(CliProvider::parse("devin"), None);
    }

    #[test]
    fn not_logged_in_view_is_actionable_and_has_no_windows() {
        let view = CliAccountView::not_logged_in(CliProvider::Codex);
        assert!(!view.logged_in);
        assert!(view.windows.is_empty());
        let error = view.error.expect("提示文案");
        assert!(error.contains("未检测到本机登录态"));
        // 必须给出可操作指引，而不是一句「失败」。
        assert!(error.contains("codex login"));
    }

    #[test]
    fn credential_source_key_is_stable_across_calls() {
        let path = std::path::PathBuf::from(r"C:\Users\x\.codex\auth.json");
        let source = CredentialSource::JsonFile(path.clone());
        assert_eq!(source.key(), source.key());
        // 大小写不同必须归一，否则同一文件会算出两个账号 id。
        let upper = CredentialSource::JsonFile(std::path::PathBuf::from(r"C:\USERS\X\.CODEX\AUTH.JSON"));
        assert_eq!(source.key(), upper.key());
    }

    #[test]
    fn credential_source_describe_never_leaks_a_token() {
        let source = CredentialSource::WindowsCredential("gemini:antigravity".to_string());
        let text = source.describe();
        assert!(text.contains("Windows 凭据管理器"));
        assert!(text.contains("gemini:antigravity"));
    }

    #[test]
    fn account_id_is_namespaced_by_provider() {
        let credential = credentials::Credential {
            provider: CliProvider::Codex,
            source: CredentialSource::JsonFile(std::path::PathBuf::from("/tmp/auth.json")),
            access_token: "secret-token-value".into(),
            refresh_token: None,
            expires_at_ms: None,
            account_label: Some("a@b.com".into()),
            extras: credentials::Extras::default(),
        };
        let id = account_id(&credential);
        assert!(id.starts_with("codex:"));
        // 账号 id 里绝不能出现 token，否则会随缓存文件落盘、随返回值出前端。
        assert!(!id.contains("secret-token-value"));
    }

    #[test]
    fn cache_round_trips_through_disk() {
        let isolation = config::test_isolation::Isolated::new("cli-quota-cache");
        let views = vec![CliAccountView {
            id: "codex:test".into(),
            provider: "codex",
            provider_label: "Codex",
            label: "user@example.com".into(),
            logged_in: true,
            source: "auth.json".into(),
            plan: Some("Plus".into()),
            windows: vec![QuotaWindow {
                label: "5 小时".into(),
                remaining_percent: Some(42.5),
                reset_at_ms: Some(1_800_000_000_000),
                detail: Some("x".into()),
            }],
            error: None,
            fetched_at: 123,
            reset_credits: Some(ResetCredit {
                available: Some(2),
                applicable: Some(1),
                earliest_expiry_ms: Some(1_900_000_000_000),
            }),
            subscription_active_until: Some("2030-01-01T00:00:00Z".into()),
        }];
        let path = isolation.dir().join("cache.json");
        save_cache_to(&path, &views);
        let loaded = load_cache_from(&path).expect("cache");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "codex:test");
        assert_eq!(loaded[0].provider, "codex");
        assert_eq!(loaded[0].windows[0].remaining_percent, Some(42.5));
        assert_eq!(loaded[0].reset_credits.as_ref().unwrap().available, Some(2));
    }

    #[test]
    fn cache_parser_rejects_corrupt_and_unknown_provider() {
        let isolation = config::test_isolation::Isolated::new("cli-quota-cache-corrupt");
        let path = isolation.dir().join("bad.json");
        std::fs::write(&path, "not-json").unwrap();
        assert!(load_cache_from(&path).is_none());

        // 伪造的 provider 必须被归一成 unknown，不能原样透传。
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "accounts": [{ "id": "fake:1", "provider": "<script>", "loggedIn": true }]
            }))
            .unwrap(),
        )
        .unwrap();
        let loaded = load_cache_from(&path).expect("cache");
        assert_eq!(loaded[0].provider, "unknown");
        assert_eq!(loaded[0].provider_label, "未知");
    }

    #[test]
    fn cache_parser_skips_entries_without_id() {
        let isolation = config::test_isolation::Isolated::new("cli-quota-cache-noid");
        let path = isolation.dir().join("noid.json");
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "accounts": [{ "provider": "codex" }, { "id": "ok:1", "provider": "codex" }]
            }))
            .unwrap(),
        )
        .unwrap();
        let loaded = load_cache_from(&path).expect("cache");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "ok:1");
    }

    #[test]
    fn cache_reads_bare_array_as_well_as_wrapped_object() {
        let isolation = config::test_isolation::Isolated::new("cli-quota-cache-bare");
        let path = isolation.dir().join("bare.json");
        std::fs::write(
            &path,
            serde_json::to_string(&json!([{ "id": "a:1", "provider": "claude" }])).unwrap(),
        )
        .unwrap();
        let loaded = load_cache_from(&path).expect("cache");
        assert_eq!(loaded[0].provider, "claude");
    }

    #[test]
    fn upsert_replaces_same_provider_and_keeps_order() {
        let mut views = vec![
            CliAccountView::not_logged_in(CliProvider::Claude),
            CliAccountView::not_logged_in(CliProvider::Codex),
        ];
        let replacement = CliAccountView {
            id: "codex:new".into(),
            provider: "codex",
            provider_label: "Codex",
            label: "new".into(),
            logged_in: true,
            source: "s".into(),
            plan: None,
            windows: vec![],
            error: None,
            fetched_at: 1,
            reset_credits: None,
            subscription_active_until: None,
        };
        upsert(&mut views, replacement);
        // 同一 provider 只留一条。
        assert_eq!(views.iter().filter(|v| v.provider == "codex").count(), 1);
        assert_eq!(views.iter().find(|v| v.provider == "codex").unwrap().id, "codex:new");
        // 顺序按 ALL（Claude 在 Codex 之前）。
        assert_eq!(views[0].provider, "claude");
        assert_eq!(views[1].provider, "codex");
    }

    #[test]
    fn upsert_orders_new_provider_by_canonical_list() {
        let mut views = vec![CliAccountView::not_logged_in(CliProvider::Kimi)];
        views.push(CliAccountView {
            id: "claude:1".into(),
            provider: "claude",
            provider_label: "Claude",
            label: "c".into(),
            logged_in: true,
            source: "s".into(),
            plan: None,
            windows: vec![],
            error: None,
            fetched_at: 1,
            reset_credits: None,
            subscription_active_until: None,
        });
        views.sort_by_key(|item| {
            CliProvider::ALL
                .iter()
                .position(|provider| provider.id() == item.provider)
                .unwrap_or(usize::MAX)
        });
        assert_eq!(views[0].provider, "claude");
        assert_eq!(views[1].provider, "kimi");
    }
}
