# ai-gateway / workbuddy2api —— 账号管理与额度/用量架构技术报告

> 目的：为新增「账号登录额度查询」功能提供与现有约定一致的落点说明。
> 所有结论均取自仓库当前源码（版本 1.0.7），引用处标注了文件与行号。

---

## 0. 总体分层（务必先理解这一层，否则会写错位置）

```
src/                       React 前端（Vite + TS + shadcn/ui + zustand）
  lib/api.ts               双通道适配层：Tauri invoke  ↔  HTTP fetch
src-tauri/src/             Tauri 桌面宿主（薄命令层，禁止写业务逻辑）
  commands.rs              WorkBuddy 系命令
  commands_apps.rs         Trae / 豆包 / 应用切换命令
  commands_proxy.rs        MITM 代理命令（只转发事件）
  lib.rs                   invoke_handler 注册表
crates/ai-gateway-core/    纯 Rust 业务核心（不依赖 Tauri）★ 新功能应落在这里
  src/modules/*.rs         每个业务域一个模块
crates/ai-gateway-server/  axum HTTP 服务（webui 形态）
  src/api.rs               路由 + handler
  src/api/apps_api.rs      Trae/豆包 handler
crates/ai-gateway-router/  内嵌 OpenAI 兼容网关（被 core 拉起）
```

**核心契约（`commands_apps.rs` 头部注释原文）**：

> 这一层只做三件事：**参数校验**、**调用 core**、**把结果转成前端可用的 JSON**。
> 所有业务逻辑都在 `ai_gateway_core::modules` 里 —— core 不依赖 Tauri，
> 因此同一套逻辑也能被 HTTP server 形态复用。
> **不要**把业务逻辑写回这个文件 —— 一旦写回来，webui 就又会漏掉它。

**新增一个功能的完整落点（4 处 + 类型 + 路由）**：

1. `crates/ai-gateway-core/src/modules/<new>.rs` —— 业务逻辑
2. `crates/ai-gateway-core/src/modules/mod.rs` —— `pub mod <new>;`
3. `src-tauri/src/commands.rs` —— `#[tauri::command]` 包装 + `lib.rs` 的 `generate_handler!` 注册
4. `crates/ai-gateway-server/src/api.rs` —— `.route(...)` + handler
5. `src/lib/api.ts` —— `ROUTES` 表加一行 + `export function xxx()`
6. `src/lib/types.ts` —— TS 接口

---

## 1. 账号模型

### 1.1 关键事实：账号**不是** Rust struct，而是无类型 `serde_json::Value`

`crates/ai-gateway-core/src/modules/account.rs` 全文没有任何 `struct Account`。
账号在 Rust 侧一律以 `Value` 传递，字段通过 `get_str(v, "key")` 读取：

```rust
// account.rs:171-177
/// 取非空字符串字段；空/缺失返回 None。
pub fn get_str(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
```

这是刻意的设计：账号库要与 Python 版（`~/.wb-switch` 共享数据目录）以及
客户端写入的认证文件互操作，字段集合在演进中不断增删，因此不做强类型映射。

**新功能若需要新的持久字段（如额度缓存），惯例是直接往账号 JSON 对象里
`insert` 一个 camelCase 或 snake_case 键，并在 `account_meta()` 里决定是否透出。**

### 1.2 账号 JSON 的完整字段（由 `oauth.rs` 的入库构造确定）

`crates/ai-gateway-core/src/modules/oauth.rs:243-264`：

```rust
let account = json!({
    "id": uuid::Uuid::new_v4().to_string(),
    "uid": uid,
    "nickname": nickname,
    "email": email,
    "enterpriseName": acc_data.get("enterpriseName"),
    "enterpriseId": acc_data.get("enterpriseId"),
    "access_token": access_token,
    "refresh_token": data.get("refreshToken").and_then(|v| v.as_str())
        .or_else(|| data.get("refresh_token").and_then(|v| v.as_str()))
        .map(|s| s.to_string()),
    "token_type": data.get("tokenType").and_then(|v| v.as_str())
        .or_else(|| data.get("token_type").and_then(|v| v.as_str()))
        .unwrap_or("Bearer")
        .to_string(),
    "domain": domain.to_string(),
    "expiresAt": expires_at,
    "refreshExpiresAt": refresh_expires_at,
    "auth_raw": data,
    "profile_raw": acc_data,
    "createdAt": now_ms(),
});
```

从本机认证文件导入时（`auth_file.rs:402-424`）构造的是**同一套字段**，
只是 `auth_raw` 存整个认证文件根对象、`profile_raw` 存 `account` 子对象。

其余会被写入的字段：

| 字段 | 写入点 | 说明 |
|---|---|---|
| `note` | `account::set_account_note`（account.rs:108-128） | 用户自定义备注；空串 = **删除该键**而非写空串 |
| `needs_relogin` | `refresh.rs:309`、`rotate` | bool；refresh token 被服务端拒绝 |
| `needs_relogin_reason` | 同上 | 展示用原因文案 |
| `refreshedAt` | `refresh::refresh_account_token` | 最近一次刷新时间（ms） |

> ⚠️ 注意命名**混用**：账号库里既有 camelCase（`expiresAt` / `enterpriseName` /
> `createdAt` / `profile_raw`）也有 snake_case（`access_token` / `refresh_token` /
> `token_type` / `needs_relogin`）。新字段请优先跟随同类语义的既有写法；
> 向**前端**透出的键一律 camelCase（见 1.4）。

### 1.3 落盘位置与 JSON 形态

数据目录（`crates/ai-gateway-core/src/modules/config.rs:204-216`）：

```rust
pub fn store_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("AI_GATEWAY_HOME") {
        let p = PathBuf::from(dir);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    home_dir().join(".wb-switch")
}

pub fn accounts_file() -> PathBuf {
    store_dir().join("accounts.json")
}
```

- **账号库**：`~/.wb-switch/accounts.json`（Windows：`C:\Users\<user>\.wb-switch\accounts.json`）
  - 顶层是 **JSON 数组**，`serde_json::to_string_pretty` 美化输出
  - 写入走 `atomic_write`（临时文件 + rename，`config.rs:943`）
  - 读取容错：文件缺失/损坏/非数组 → 返回 `vec![]`（account.rs:36-43）
  - 写入前过私有 `fn sanitize_accounts(accounts: Vec<Value>) -> Vec<Value>`（account.rs:369）
    按身份去重、剔除事故残留；另有公开的
    `pub fn purge_credentialless_leftovers() -> usize`（account.rs:325）在启动时清理
    「无凭据、从未采集」的僵尸记录

- **本软件其他落盘文件**（同一 `store_dir()`）：

| 文件 | 构造器 | 用途 |
|---|---|---|
| `app_settings.json` | `app_settings_file()` (config.rs:263) | 界面/路径偏好、豆包端点 |
| `auto_checkin_config.json` | `checkin_config_file()` | 签到/保活开关 |
| `auto_checkin_logs.json` | `checkin_logs_file()` | 签到日志（保留 30 天） |
| `credit_usage_snapshots.json` | `credit_usage_snapshots_file()` (config.rs:450) | 积分观察快照 |
| `official_usage_cache.json` | `official_usage_cache_file()` (config.rs:454) | 官方用量投影缓存 |
| `doubao_accounts.json` | `doubao_account::accounts_file()` | 豆包账号池 |
| `backups/` | `backup_dir()` | 认证文件切换前备份 |
| `gateway/` | `gateway::gateway_dir()` | 网关配置与凭证目录 |

- **官方认证文件**（不属于本软件，但是账号来源）：

```rust
// auth_file.rs:15-41
pub fn auth_dir() -> PathBuf {
    let home = crate::modules::config::home_dir();
    #[cfg(target_os = "windows")]
    return home.join("AppData/Local/CodeBuddyExtension/Data/Public/auth");
    ...
}

pub fn auth_file_name_for(region: Region) -> &'static str {
    match region {
        Region::Cn => "workbuddy-desktop.info",
        Region::Intl => "workbuddy-desktop-ai.info",
    }
}
```

认证文件本身是**四段 JSON**：`{ uid, nickname, ..., account: {...}, auth: {...}, allAccounts: [...] }`
（`imported_account_from_root` 读 `account` / `auth` 两个子对象，`write_account_to_auth_file_at` 保留 `allAccounts`）。

### 1.4 区域（provider 判定）与身份键

**区域不是独立字段，而是由 `domain` 后缀推导**（`config.rs:26-111`）：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region { Cn, Intl }

impl Region {
    pub const ALL: [Region; 2] = [Region::Cn, Region::Intl];

    pub fn from_domain(domain: &str) -> Region {
        if domain.trim().to_ascii_lowercase().ends_with(".ai") {
            Region::Intl
        } else {
            Region::Cn
        }
    }

    pub fn of(account: &serde_json::Value) -> Region {
        Region::from_domain(account.get("domain").and_then(|v| v.as_str()).unwrap_or(""))
    }

    pub fn from_key(key: &str) -> Region {
        if key.trim().eq_ignore_ascii_case("intl") { Region::Intl } else { Region::Cn }
    }

    pub fn key(self) -> &'static str { /* "cn" | "intl" */ }
    pub fn api_endpoint(self) -> &'static str { /* codebuddy.cn | workbuddy.ai */ }
    pub fn oauth_platform(self) -> &'static str { /* "workbuddy" | "workbuddy-ai" */ }
    pub fn label(self) -> &'static str { /* "国服" | "国际版" */ }
    pub fn auth_domain(self) -> &'static str { /* www.workbuddy.cn | www.workbuddy.ai */ }
}
```

端点常量（config.rs:16-22）：

```rust
pub const WORKBUDDY_API_ENDPOINT: &str = "https://www.codebuddy.cn";
pub const WORKBUDDY_API_ENDPOINT_INTL: &str = "https://www.workbuddy.ai";
pub const WORKBUDDY_WEB_ENDPOINT: &str = "https://www.workbuddy.cn";   // credits.rs:19
pub const WORKBUDDY_API_PREFIX: &str = "/v2/plugin";
pub const WORKBUDDY_PLATFORM: &str = "workbuddy";
pub const WORKBUDDY_PLATFORM_INTL: &str = "workbuddy-ai";
```

**本地身份键 `(region, uid)`**（account.rs:20-34）：

```rust
pub fn account_identity(account: &Value) -> Option<(String, String)> {
    let uid = get_str(account, "uid")?;
    Some((region_key(account).to_string(), uid))
}

pub fn region_key(account: &Value) -> &'static str { /* "cn" | "intl" */ }
```

> 注释明确：国服与国际版的 uid / 邮箱是**相互独立的命名空间**，一切身份比较
> 都必须带上区域，否则跨区域会互相覆盖。新功能做「按账号缓存额度」时，
> **缓存键必须是 `(region, uid)` 或库内 `id`，不能用裸 uid**。

### 1.5 `account_meta()` —— 唯一向前端透出的账号形态

`account.rs:131-169`（**新字段若要给前端看，必须加在这里**）：

```rust
pub fn account_meta(acc: &Value) -> Value {
    let region = crate::modules::config::Region::of(acc);
    json!({
        "region": region.label(),
        "regionKey": match region { Region::Cn => "cn", Region::Intl => "intl" },
        "id": acc.get("id"),
        "uid": acc.get("uid"),
        "email": acc.get("email"),
        "nickname": acc.get("nickname"),
        "enterpriseName": acc.get("enterpriseName"),
        "expiresAt": acc.get("expiresAt"),
        "refreshExpiresAt": acc.get("refreshExpiresAt"),
        "refreshedAt": acc.get("refreshedAt"),
        "createdAt": acc.get("createdAt"),
        "needsRelogin": acc.get("needs_relogin").and_then(|v| v.as_bool()) == Some(true),
        "needsReloginReason": acc.get("needs_relogin_reason"),
        "note": acc.get("note"),
        "domain": acc.get("domain"),
        "phoneNumber": acc.get("profile_raw").and_then(|p| p.get("phoneNumber")).cloned().unwrap_or(Value::Null),
        "accountType": acc.get("profile_raw").and_then(|p| p.get("type")).cloned().unwrap_or(Value::Null),
    })
}
```

有测试钉死「不得泄露 token」（`account_meta_strips_tokens`，account.rs:495-514）。

### 1.6 账号新增/登录入口

**OAuth 流程（`crates/ai-gateway-core/src/modules/oauth.rs`）**：

```rust
/// 发起登录：向官方申请 state，返回 loginId / verificationUri / expiresIn。
pub async fn oauth_start(region: Region) -> Result<Value, String>   // oauth.rs:91

/// 轮询一次官方 token 接口。成功则拉取账号信息并入库。
pub async fn oauth_poll(login_id: &str) -> Value                    // oauth.rs:144
```

- 端点（三处，全部由 `Region` 决定域名）：
  - `{endpoint}/v2/plugin/auth/state?platform={workbuddy|workbuddy-ai}` (POST)
  - `{endpoint}/v2/plugin/auth/token?state={state}` (GET)
  - `{endpoint}/v2/plugin/login/account?state={state}` (GET)
- `oauth_start` 返回：`{ "loginId", "verificationUri", "expiresIn", "region" }`
- `oauth_poll` 返回：`{ "done": bool, "result"?: AccountMeta, "error"?: string }`
- 会话状态存在进程内 `static OAUTH_STATES: OnceLock<Mutex<HashMap<String, OAuthInfo>>>`，
  超时 `OAUTH_TIMEOUT_SECONDS = 600`
- **token 端点一次性消费**：拿到结果必须立刻 `save_collected_account` 入库，
  且账号信息拉取有 3 次重试（`ACCOUNT_FETCH_ATTEMPTS = 3`，间隔 800ms）
- 入库统一走 `account::save_collected_account(account)` → `upsert_collected_account`

**其他入库路径**：

| 函数 | 位置 | 说明 |
|---|---|---|
| `account::import_local_all() -> Result<Vec<Value>, String>` | account.rs:1360 | 探测两个固定认证文件（每区域最多 1 个） |
| `account::scan_local_accounts() -> LocalScanResult` | account.rs:1218 | 扫本机全部历史登录态（当前+快照+备份），按 (区域,uid) 去重 |
| `account::import_local_selected(paths, indexes) -> Result<LocalImportResult, String>` | account.rs:1279 | 按文件路径选择键批量导入 |
| `export_import::import_accounts(file_text, indexes)` | export_import.rs | 从导出文件导入 |
| `doubao_account::upsert_account(...)` | doubao_account.rs:157 | 豆包账号（**独立账号库**，见 1.7） |

> 手动添加账号（token 方式）已下线，见 account.rs:1390-1391 注释。

### 1.7 另外两套**独立**的账号库（provider 概念的真正体现）

本仓库实际有 **3 个互不相通的账号域**：

| 域 | 账号库文件 | 账号主键 | 模块 |
|---|---|---|---|
| WorkBuddy / CodeBuddy（cn+intl） | `~/.wb-switch/accounts.json` | `id`（uuid）/ `uid` | `account.rs` |
| Trae 系（trae / trae-cn / …） | `trae_account::accounts_file()` | `uid`（JWT 解析） | `trae_account.rs` |
| 豆包 | `~/.wb-switch/doubao_accounts.json` | `user_id` | `doubao_account.rs` |

豆包账号池是 `{ "accounts": [...], "last_keepalive_at": ... }` 的**对象**（不是数组）：

```rust
// doubao_account.rs:33-46
pub fn load_pool() -> Value {
    std::fs::read_to_string(accounts_file())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({ "accounts": [], "last_keepalive_at": null }))
}
```

豆包账号的**脱敏视图** `account_view()`（doubao_account.rs:123-151）已经带了额度缓存字段，
这是「额度写回账号库」的现成范式，新功能可直接照抄：

```rust
"quotaLevel": acc.get("quota_level"),
"quotaExpireAt": acc.get("quota_expire_at"),
"quotaSummary": acc.get("quota_summary"),
"quotaCheckedAt": acc.get("quota_checked_at"),
```

---

## 2. 现有额度 / 用量查询能力（现状盘点）

### 2.1 `credits.rs` —— WorkBuddy 积分资源（余额/到期）

**职责**：查询单个账号的**积分资源包**（总量/剩余/到期时间）。

端点常量（credits.rs:18-42）：

```rust
const USER_RESOURCE_PATH: &str = "/v2/billing/meter/get-user-resource";
const WORKBUDDY_WEB_ENDPOINT: &str = "https://www.workbuddy.cn";
const RESOURCE_SUMMARY_PATH: &str = "/billing/meter/get-user-resource-summary";
const RESOURCE_PAID_PACKAGES_PATH: &str = "/billing/meter/get-user-resource-paid-packages";
const RESOURCE_FREE_PACKAGES_PATH: &str = "/billing/meter/get-user-resource-free-packages";
const PRODUCT_CODE: &str = "p_tcaca";
const EXPIRING_SOON_DAYS: i64 = 7;
```

请求链路（credits.rs:480-531）：`fetch_new_resource_responses` 先
`ensure_fresh_token`，再 `tokio::join!` **并行打三个新接口**；任一 401 则
**只刷新一次 token** 然后仅重试该分支（注释：避免三个 future 同时刷新并覆盖账号库中的 token）。

公开签名：

```rust
/// 发起需要账号身份的 JSON POST 请求。资源查询和官方用量查询必须共用这条链路。
pub async fn authenticated_post(account: &Value, url: &str, body: Value) -> Value   // :329

/// 官方请求用量接口的 URL：域名跟随账号区域。
pub fn official_usage_url(account: &Value) -> String                                // :443

/// 查询单账号的积分资源及到期时间。
pub async fn get_credit_expiry(account: &Value) -> Value                            // :710
```

`get_credit_expiry` 返回形态（`credit_result`，credits.rs:693-706）：

```json
{
  "ok": true,
  "accountId": "...", "accountName": "...",
  "updatedAt": 1700000000000,
  "totalCapacity": 0.0, "totalRemaining": 0.0,
  "expiringSoonRemaining": 0.0, "expiredRemaining": 0.0,
  "soonestExpireAt": null, "expiringSoon": false, "expired": false,
  "resources": [ { "packageCode", "packageName", "total", "remaining",
                   "used", "status", "expireAt", "expired", "expiringSoon" } ]
}
```

失败形态：`{ "ok": false, "accountId", "accountName", "error" }`（credits.rs:747-752）。

**副作用**：成功时调用 `credit_usage::record_snapshot(...)` 落一条本地观察快照。

请求头（credits.rs:368-386）—— 新功能打同域接口时必须复用：

```rust
let mut headers = build_auth_headers(account);   // Authorization / X-User-Id / X-Enterprise-Id / X-Domain
headers.insert("X-Client-Platform".to_string(), "web".to_string());
headers.insert("Accept".to_string(), "application/json, text/plain, */*".to_string());
headers.insert("Origin".to_string(), origin.to_string());
headers.insert("Referer".to_string(), format!("{origin}/profile/plans-usage"));
```

### 2.2 `official_usage.rs` —— WorkBuddy 官方请求用量（近 31 天）

**职责**：拉官方**逐条请求明细**并投影成统计页需要的聚合结果；**永不把
prompt/input 等敏感字段带出来**。

常量与入口：

```rust
pub const OFFICIAL_USAGE_PAGE_SIZE: usize = 3_000;     // :18
pub const OFFICIAL_USAGE_DETAIL_LIMIT: usize = 100;    // :19
const OFFICIAL_USAGE_MAX_PAGES: usize = 100;

/// 统计页默认读缓存；`refresh = true` 时才重新请求官方用量接口。
pub async fn official_usage_for_statistics(accounts: &[Value], at_ms: i64, refresh: bool) -> Value  // :193

/// 查询全部当前账号并生成官方请求用量投影。
pub async fn collect_official_usage(accounts: &[Value], at_ms: i64) -> Value                        // :575
```

- 上游：`POST {official_usage_url(account)}`，body `{startTime, endTime, pageNum, pageSize}`
  （`startTime`/`endTime` 形如 `"2026-08-24 00:00:00"` / `"23:59:59"`）
- 域名：`official_usage_url` —— 国际版 → `workbuddy.ai`，其余 → `workbuddy.cn`
- 时间范围：`today - Duration::days(30)` 到 `today`（31 个自然日）
- 分页：`should_fetch_next_page`，上限 `OFFICIAL_USAGE_MAX_PAGES`，超限返回 `Err("官方用量分页超过安全上限")`
- 缓存：进程内 `static OFFICIAL_USAGE_MEMORY: Mutex<Option<Value>>` + 磁盘
  `official_usage_cache.json`（形态 `{ "payload": <投影> }`）；读取时**重新 sanitize**
  （`sanitize_cached_payload` 白名单字段，防止缓存里混入历史敏感字段）
- 并发保护：`official_usage_fetch_lock()`（`tokio::sync::Mutex`）+ double-check

返回形态（`collect_official_usage`，official_usage.rs:708-724）：

```json
{
  "status": "complete" | "partial" | "unavailable",
  "rangeStart": "2026-07-26", "rangeEnd": "2026-08-25",
  "collectedAt": 1700000000000,
  "summary": { "usageToday": 0.0, "usage7Days": 0.0, "usageThisMonth": 0.0 },
  "daily":    [ { "date": "2026-08-25", "usage": 0.0, "models": [ {"model","requestCount","credit"} ] } ],
  "accounts": [ { "accountId","accountName","ok","requestCount","detailTruncated",
                  "usageToday","usage7Days","usageThisMonth","error",
                  "reportedTotal","fetchedCount","models","daily" } ],
  "requests": [ { "accountId","accountName","requestId","credit","model","client","requestTime" } ],
  "models":   [ { "model","requestCount","credit" } ],
  "detailLimitPerAccount": 100,
  "errors":   [ { "accountId","accountName","error" } ]
}
```

### 2.3 `credit_usage.rs` —— 本地积分观察快照 + 统一统计投影

**职责**：WorkBuddy **不提供历史账单**，因此把每次成功查到的余额存成快照，
用相邻快照的**正向下降量**推导消耗。

```rust
pub const CREDIT_SNAPSHOT_RETENTION_DAYS: i64 = 90;             // :20
pub const CREDIT_SNAPSHOT_MAX_RECORDS: usize = 5_000;           // :21
pub const CREDIT_SNAPSHOT_DEDUPE_WINDOW_MS: i64 = 5*60*1000;    // :22

/// 读取本地观察快照；文件缺失或损坏时返回空列表。
pub fn load_snapshots() -> Vec<Value>                                                  // :102

/// 记录一次余额观察。返回值表示本次是否实际写入。
pub fn record_snapshot(account_id: &str, account_name: &str, total: f64, remaining: f64) -> bool  // :153

/// 返回本地快照、账号列表、签到日志与官方请求用量的统一统计投影。
pub async fn get_statistics(refresh: bool) -> Value                                    // :560
```

快照元素形态（`snapshot_value`，:85-99）：

```json
{ "ts": 1700000000000, "accountId": "...", "accountName": "...", "total": 100.0, "remaining": 70.0 }
```

`get_statistics` 的组合方式（**新功能若也要并入统计页，照此挂载**）：

```rust
pub async fn get_statistics(refresh: bool) -> Value {
    let at_ms = now_ms();
    let accounts = load_accounts();
    let mut statistics = build_statistics(&load_snapshots(), &load_checkin_logs(), &accounts, at_ms);
    statistics["officialUsage"] =
        official_usage::official_usage_for_statistics(&accounts, at_ms, refresh).await;
    statistics
}
```

顶层返回：`{ generatedAt, retentionDays, coverageStartAt, summary, daily, accounts, events, officialUsage }`
（`build_statistics`，:535-553；`summary` 含 `currentRemaining / currentCapacity /
usageToday / usage7Days / usageThisMonth / todayCheckedInAccounts / todaySuccess /
todayAlready / todayFailed`）。

### 2.4 `doubao_quota.rs` —— 豆包**会员额度**（最接近「额度查询」的现成实现）

**这是全仓库唯一真正的「额度（quota）」模块，新功能应重点参考它的形状。**

```rust
/// 额度接口默认值。
pub const DEFAULT_QUOTA_URL: &str =
    "https://www.doubao.com/alice/commerce/sale/subscription/quota/summary/";   // :21-22

#[derive(Debug, Clone, Default)]
pub struct ParsedQuota {
    pub level: Option<String>,            // 会员等级展示名（如「豆包 Pro」）
    pub expire_at: Option<String>,        // 套餐到期时间
    pub has_subscription: bool,
    pub is_gift: bool,
    pub subscription: Option<Value>,
    pub windows: Vec<Value>,              // 窗口额度（当前时段 / 近 7 天）
    pub items: Vec<Value>,                // 兜底解析出的额度项
}

impl ParsedQuota {
    pub fn has_data(&self) -> bool
    pub fn summary(&self) -> String       // 单行摘要，界面 hover 用
}

/// 解析额度响应（精确解析 → 失败才启用宽容兜底）。
pub fn parse_quota(resp: &Value) -> ParsedQuota                                   // :280

/// 查询单个账号的额度。
pub async fn fetch_single(uid: &str) -> Result<ParsedQuota, String>               // :294

/// 批量查询全部持有凭证的账号，并把结果写回额度缓存。
pub async fn run_batch() -> Value                                                  // :355

/// 查询结果转前端形态。
pub fn to_view(uid: &str, parsed: &ParsedQuota) -> Value                          // :418

/// 会话探活（供界面「诊断」按钮）。
pub async fn probe_account(uid: &str) -> Value                                    // :434
```

- 上游：`POST https://www.doubao.com/alice/commerce/sale/subscription/quota/summary/`
  body `{"product_line":"membership"}`，`Cookie: sessionid=...; sid_guard=...`
- URL 可被设置覆盖：`config::app_setting_str("doubao_quota_url")`
- **不走 `config::http_request`**，而是自建 `reqwest::Client`（`timeout 15s` + `.no_proxy()`，
  注释：本地 MITM 代理未启动时是死端口）
- **两段式解析**（模块头注释原文）：
  1. 精确解析：`current_subscription` / `window_limit_section.window_limit_groups`
  2. 宽容兜底：按「同时含名称键与总量键的对象」深挖（深度上限 10）
- `windows` 元素：`{ "name", "usedPercent", "exhausted", "resetAt" }`
- 业务码：`None | Some(0)` 成功；`doubao_session::SESSION_EXPIRED_CODE` → 「登录态已失效…」
- `run_batch` **每账号之间 `sleep(500ms)`**，并把结果写回账号池：
  `quota_level` / `quota_expire_at` / `quota_summary` / `quota_checked_at`

### 2.5 覆盖矩阵 —— **哪些 provider 已有额度查询，哪些没有**

| Provider | 积分/余额 | 请求用量 | 会员额度/套餐 | 落点模块 |
|---|---|---|---|---|
| WorkBuddy 国服（`.cn`） | ✅ `credits.rs` | ✅ `official_usage.rs` | ❌ **无** | — |
| WorkBuddy 国际版（`.ai`） | ✅ `credits.rs`（域名自动切换） | ✅ `official_usage.rs` | ❌ **无** | — |
| 豆包 | ❌ | ❌ | ✅ `doubao_quota.rs` | doubao_account 池 |
| Trae 系 | ⚠️ 仅签到积分历史 | ❌ | ⚠️ `trae_discover::read_entitlement`（**读本地文件**，非实时） | trae_account |
| CodeBuddy CLI / CN IDE | ❌ | ❌ | ❌ | — |

**结论：WorkBuddy 账号目前只有「积分余额」与「请求用量」，
没有任何「登录后的额度 / 套餐 / 会员等级」查询能力 —— 这正是新功能的空白点。**

### 2.6 Trae 侧可参考的「额度」相关实现（均为本地读取，非实时接口）

```rust
// apps_ops.rs:459
pub fn trae_entitlement(app_kind: &str) -> Value {
    trae_discover::read_entitlement(app_kind).unwrap_or_else(|| json!({}))
}

// apps_ops.rs:465
pub fn trae_credits_history() -> Value {
    json!({ "records": trae_checkin::load_credits_history() })
}
```

Trae 签到接口（唯一实时额度相关端点，`trae_checkin.rs:27-28`）：

```rust
const SIGNIN_URL: &str = "https://api.trae.cn/trae/api/v2/ug/checkin_credits/claim";
const STATUS_URL: &str = "https://api.trae.cn/trae/api/v2/ug/checkin_credits/status";
```

---

## 3. 前后端契约

### 3.1 Tauri 命令声明约定

**命名**：命令函数名 = 前端 `invoke("snake_case")` 的字符串，即
`#[tauri::command] pub async fn get_credit_expiry(...)` → `invoke("get_credit_expiry", { accountId })`。

**参数命名规则（关键坑）**：

- **Tauri v2 的默认参数命名规则就是 camelCase**（`tauri-macros` 源码：
  `argument_case: ArgumentCase::Camel` 是默认值，`ArgumentCase::Camel => key = key.to_lower_camel_case()`）。
  即 `pub fn delete_account(account_id: String)` 在前端要传 `{ accountId }`，
  与是否写 `rename_all` **无关**。
- 证据（前端全部传 camelCase，命令全部是 snake_case 形参）：

```ts
// api.ts:321-323
export function deleteAccount(accountId: string): Promise<{ ok: boolean }> {
  return call("delete_account", { accountId });
}

// api.ts:425-430
export function copySessions(targetAccountId: string, sessionIds: string[]): ... {
  return call("copy_sessions", { targetAccountId, sessionIds });
}

// api.ts:505-507
export function getCreditExpiry(accountId: string): Promise<CreditExpiry> {
  return call("get_credit_expiry", { accountId });
}
```

  对应的 Rust 形参分别是 `account_id`、`target_account_id` / `session_ids`、`account_id`。

- 仓库里显式写 `#[tauri::command(rename_all = "camelCase")]` 的命令（**冗余但无副作用，
  属于既有风格**）：`switch_codebuddy_cli_account`、`switch_codebuddy_cn_ide_account`、
  `switch_account`、`copy_sessions`、`save_gateway_config`、`switch_gateway_mode`、
  `sync_gateway_accounts`、`import_agent_client`、`batch_import_agent_clients`、
  `restore_agent_client`、`proxy_start`、`app_env_check`、`app_set_manual_path` 等。
  **大多数命令不写**（依赖默认值）。
- 前端 `api.ts` 里踩过的真实坑是**另一件事**：业务对象内部的 snake_case 键
  不能直接透传，见 `saveGatewayConfig` 的注释：

```ts
/**
 * 保存网关配置。
 *
 * 显式把 snake_case 字段转成 camelCase：Tauri 的 `invoke` 按
 * `#[tauri::command(rename_all = "camelCase")]` 取值，直接透传
 * `api_key` / `auto_start` 会被静默丢弃（webui 的 HTTP 版则兼容两种写法）。
 */
export function saveGatewayConfig(config: Partial<GatewayConfig>): Promise<{ config: GatewayConfig }> {
  const args: Record<string, unknown> = {};
  if (config.port !== undefined) args.port = config.port;
  if (config.api_key !== undefined) args.apiKey = config.api_key;
  if (config.auto_start !== undefined) args.autoStart = config.auto_start;
  ...
}
```

> **实践结论**：新增命令时，Rust 形参用 snake_case，前端 `call()` 传 **camelCase 键**；
> 想更保险可以像既有命令那样显式写 `rename_all = "camelCase"`。

**参数类型**：一律 `String` / `Option<String>` / `Option<bool>` / `Option<i64>` /
`Vec<String>` / `Option<Vec<String>>`，**没有自定义 DTO 入参**（`SwitchRequest` 是 core 侧结构）。

**返回值**：绝大多数返回 `Result<Value, String>` 或裸 `Value`；
只有 `get_status` 用 `#[derive(Serialize)] struct AppStatus`（commands.rs:15-22）。

**`async` + `spawn_blocking` 规则**（commands.rs 多处注释）：

- 会跑子进程 / 大文件拷贝 / 慢 IO 的 → `pub async fn` + `tauri::async_runtime::spawn_blocking`
  （同步 command 跑在 Tauri 主线程，会冻结窗口消息循环）
- 纯内存 + 网络 await 的 → 直接 `pub async fn`

```rust
// commands.rs:391-401 —— 典型「薄包装」范例
/// POST /api/credits —— 查询单账号积分资源及到期时间。
#[tauri::command]
pub async fn get_credit_expiry(account_id: String) -> Result<Value, String> {
    let acc = account::find_account(&account_id).ok_or("账号不存在")?;
    Ok(credits::get_credit_expiry(&acc).await)
}

/// GET /api/credits/stats —— 本地快照与官方请求用量统计。
/// `refresh = true` 时才重新请求官方用量；默认读缓存。
#[tauri::command]
pub async fn get_credit_statistics(refresh: Option<bool>) -> Value {
    credit_usage::get_statistics(refresh.unwrap_or(false)).await
}
```

**事件推送**（长任务进度）：`app.emit("switch-progress", json!({ "message": msg }))`，
前端 `listen<T>("event-name", cb)`。已有事件名：`switch-progress`、
`proxy-log`、`account-captured`、`proxy-crashed`、`main-window-visible`。
（webui 无事件总线，改为写进程内 `static` 供前端轮询，见 `SWITCH_PROGRESS` / `APP_PROGRESS`。）

### 3.2 命令注册（`src-tauri/src/lib.rs:192-315`）

```rust
.invoke_handler(tauri::generate_handler![
    commands::get_status,
    commands::get_accounts,
    ...
    commands::get_checkin_status,
    commands::get_credit_expiry,
    commands::get_credit_statistics,
    commands::get_token_statistics,
    ...
])
```

**新命令必须同时**：(a) 在 `commands.rs` 写 `#[tauri::command]`，
(b) 在 `lib.rs` 的 `generate_handler!` 里加一行。漏 (b) 前端会报
`Command xxx not found`。

### 3.3 HTTP 路由声明（`crates/ai-gateway-server/src/api.rs`）

`pub fn router() -> Router`（api.rs:53-270），路径命名约定：

- 资源型：`/api/<域>/<子资源>`，kebab-case（`/api/codebuddy-cli/status`、`/api/import-local/scan`）
- 读用 `get`，写/触发用 `post`；同一路径可 `.get(h).post(h2)`
  （`/api/gateway/config`，api.rs:115）
- 已有的额度相关路由：

```rust
.route("/api/credits", post(api_credits))                    // api.rs:88
.route("/api/credits/stats", get(api_credit_statistics))     // api.rs:89
.route("/api/token-stats", get(api_token_statistics))        // api.rs:90
.route("/api/doubao/quota", get(apps_api::api_doubao_fetch_quota))          // :216
.route("/api/doubao/quota/batch", post(apps_api::api_doubao_quota_batch))   // :217-220
```

**响应信封**（api.rs:272-278）—— **成功响应没有信封**，只有错误有：

```rust
pub fn json_ok(v: Value) -> Response {
    Json(v).into_response()
}

pub fn json_err(e: String, code: StatusCode) -> Response {
    (code, Json(json!({ "ok": false, "error": e }))).into_response()
}
```

即：

- 成功：HTTP 200 + **业务 JSON 原样**（例如 `/api/credits` 直接返回 `credit_result` 的 `{ok, accountId, ...}`）
- 失败：HTTP 4xx/5xx + `{ "ok": false, "error": "..." }`

**Handler 形态**：

```rust
async fn api_credits(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    let Some(acc) = account::find_account(id) else {
        return json_err("账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    json_ok(credits::get_credit_expiry(&acc).await)
}

async fn api_credit_statistics(RawQuery(query): RawQuery) -> Response {
    json_ok(credit_usage::get_statistics(query_flag_enabled(query.as_deref(), "refresh")).await)
}
```

- **GET 参数解析**：`Query<HashMap<String,String>>` 或 `RawQuery`（手写 `key=value` 切分）
- **POST body**：`Json(body): Json<Value>`，然后 `body.get("accountId")`。
  **注意兼容两种写法**（`api_set_account_note`，api.rs:317-323）：

```rust
let id = body.get("accountId")
    .or_else(|| body.get("account_id"))
    .or_else(|| body.get("id"))
    .and_then(Value::as_str)
    .unwrap_or("");
```

- `apps_api.rs` 的辅助函数（新 handler 建议复用）：`q_param(&params, key)`、
  `require_str(&body, key)`、`blocking_response(...)`、`blocking_plain(...)`、
  `json_ok` / `json_err`（`use super::{json_err, json_ok};`）
- 服务端口：`57890`（`main.rs::default_port()`），仅绑定 127.0.0.1

### 3.4 前端 API 包装（`src/lib/api.ts`）

**双通道适配层**（api.ts:54-95）：

```ts
const API_BASE = "http://127.0.0.1:57890";

export function isWebui(): boolean {
  return typeof window !== "undefined" && !("__TAURI_INTERNALS__" in window);
}

type Route = { method: "GET" | "POST"; path: string };

/** Tauri command → HTTP 路由映射（webui 模式）。 */
const ROUTES: Record<string, Route> = {
  get_status: { method: "GET", path: "/api/status" },
  ...
  get_credit_expiry: { method: "POST", path: "/api/credits" },
  get_credit_statistics: { method: "GET", path: "/api/credits/stats" },
  ...
};

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (demoModeEnabled) {
    if (cmd === "get_credit_statistics" && args?.refresh === true) {
      throw new Error(DEMO_UNAVAILABLE_MESSAGE);
    }
    if (!DEMO_READ_COMMANDS.has(cmd)) throw new Error(DEMO_UNAVAILABLE_MESSAGE);
    return screenshotDemoResponse(cmd, args) as T;
  }
  if (!isWebui()) return invoke<T>(cmd, args);
  return httpCall<T>(cmd, args);
}
```

**`httpCall` 的错误提取**（api.ts:246-248）：

```ts
if (!res.ok) {
  throw new Error(data.message || data.error || `请求失败 (${res.status})`);
}
```

**新增一个前端 API 函数的完整步骤**：

1. `ROUTES` 里加一行 `xxx_yyy: { method: "GET"|"POST", path: "/api/..." }`
2. 若该命令在 demo 模式下应可读，加入 `DEMO_READ_COMMANDS`（api.ts:61-70）
   —— 并相应在 `src/lib/screenshot-demo.ts` 的 `screenshotDemoResponse` switch 里加 case
3. 写导出函数：

```ts
export function getCreditExpiry(accountId: string): Promise<CreditExpiry> {
  return call("get_credit_expiry", { accountId });
}

export function getCreditStatistics(refresh = false): Promise<CreditStatistics> {
  return call("get_credit_statistics", refresh ? { refresh: true } : undefined);
}
```

4. `src/lib/types.ts` 加 TS 接口（字段 camelCase，与 Rust 返回严格对齐）
5. `export function asError(e: unknown): string`（api.ts:654-658）用于把任意抛出统一成字符串：

```ts
export function asError(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return JSON.stringify(e ?? "未知错误");
}
```

**特例**：webui 端接口形状与 Tauri 不同时，在 `api.ts` 里做适配，
范例 `getCheckinStatus`（api.ts:471-503，webui 是批量接口 → 前端按 accountId 过滤）。

---

## 4. 前端约定

### 4.1 路由与导航（`src/App.tsx`）

```tsx
export default function App() {
  const Router = pagesDemoHostingEnabled ? HashRouter : BrowserRouter;

  return (
    <TooltipProvider delayDuration={250}>
      <Router>
        <Routes>
          <Route element={<Layout />}>
            <Route path="/" element={<AccountsPage />} />
            <Route path="/credit-stats" element={<CreditStatsPage />} />
            <Route path="/token-stats" element={<TokenStatsPage />} />
            <Route path="/gateway" element={<GatewayPage />} />
            <Route path="/agents" element={<AgentsPage />} />
            <Route path="/trae" element={<TraePage />} />
            <Route path="/doubao" element={<DoubaoPage />} />
            <Route path="/settings" element={<SettingsPage />} />
            <Route path="*" element={<Navigate to="/" replace />} />
          </Route>
        </Routes>
        <Toaster />
      </Router>
    </TooltipProvider>
  );
}
```

导航项写在 `Layout()` 的 `<nav aria-label="主导航">` 里，每个是一个 `<NavLink>`，
className 用 `cn(...)` 三元切换 active 态。**完整复制这段样式即可**（App.tsx:138-152）：

```tsx
<NavLink
  to="/credit-stats"
  className={({ isActive }) =>
    cn(
      "flex items-center gap-2.5 rounded-lg px-3 py-2.5 text-sm outline-none transition-colors focus-visible:ring-2 focus-visible:ring-sidebar-ring/50",
      isActive
        ? "bg-foreground/[0.06] font-medium text-foreground"
        : "text-muted-foreground hover:bg-foreground/[0.04] hover:text-foreground",
    )
  }
>
  <Sparkles className="size-4" />
  积分统计
</NavLink>
```

**页面组件约定**：`src/pages/*.tsx`，`export default function XxxPage()`，
外层容器统一为：

```tsx
<div className="mx-auto w-full max-w-[1180px] min-w-0 px-4 py-6 sm:px-8 sm:py-9">
  <header className="mb-10 flex min-w-0 flex-wrap items-start justify-between gap-4 sm:mb-12">
    <div className="min-w-0">
      <h1 className="text-[28px] font-semibold tracking-tight">积分统计</h1>
      <p className="mt-2 max-w-2xl text-sm leading-6 text-muted-foreground">…</p>
    </div>
    <Button …>刷新统计</Button>
  </header>
  …
</div>
```

（见 `CreditStatsPage.tsx:1288-1309`。）

**演示模式包裹**：任何会改变状态的操作按钮外面套 `<DemoAction>`：

```tsx
<DemoAction>
  <Button onClick={() => void load(true)} disabled={loading}>刷新统计</Button>
</DemoAction>
```

### 4.2 shadcn/ui 组件清单（`src/components/ui/`，共 18 个文件）

| 文件 | 导出的组件 |
|---|---|
| `alert.tsx` | `Alert`, `AlertTitle`, `AlertDescription` |
| `badge.tsx` | `Badge`, `badgeVariants` |
| `button.tsx` | `Button`, `buttonVariants` |
| `card.tsx` | `Card`, `CardHeader`, `CardFooter`, `CardTitle`, `CardAction`, `CardDescription`, `CardContent` |
| `chart.tsx` | `ChartContainer`, `ChartTooltip`, `ChartTooltipContent`, `ChartLegend`, `ChartLegendContent`, `ChartStyle` |
| `checkbox.tsx` | `Checkbox` |
| `dialog.tsx` | `Dialog`, `DialogClose`, `DialogContent`, `DialogDescription`, `DialogFooter`, `DialogHeader`, `DialogOverlay`, `DialogPortal`, `DialogTitle`, `DialogTrigger` |
| `dropdown-menu.tsx` | `DropdownMenu`, `DropdownMenuContent`, `DropdownMenuItem`, `DropdownMenuSeparator`, `DropdownMenuTrigger` |
| `input.tsx` | `Input` |
| `label.tsx` | `Label` |
| `popover.tsx` | `Popover`, `PopoverAnchor`, `PopoverContent`, `PopoverTrigger` |
| `select.tsx` | `Select`, `SelectContent`, `SelectGroup`, `SelectItem`, `SelectLabel`, `SelectScrollDownButton`, `SelectScrollUpButton`, `SelectSeparator`, `SelectTrigger`, `SelectValue` |
| `separator.tsx` | `Separator` |
| `skeleton.tsx` | `Skeleton` |
| `sonner.tsx` | `Toaster` |
| `switch.tsx` | `Switch` |
| `tabs.tsx` | `Tabs`, `TabsList`, `TabsTrigger`, `TabsContent` |
| `tooltip.tsx` | `Tooltip`, `TooltipTrigger`, `TooltipContent`, `TooltipProvider` |

**`Badge` 的 variant 有仓库自定义扩展**（badge.tsx:11-22）：
`default | secondary | destructive | outline | success | warning`
（`success` = `bg-primary/15 text-primary`，`warning` 带 dark 覆盖）。
额度状态标签建议直接用 `success` / `warning` / `outline`。

**注意缺失**：**没有** `table`、`progress`、`scroll-area`、`alert-dialog`、
`sheet`、`form`、`calendar`。若需要，按 AGENTS.md 的 UI Component Policy
「补上匹配的 shadcn/Radix 组件并包在 `src/components/ui/` 下」。

非 `ui/` 的业务组件：`src/components/*.tsx`（`account-card.tsx`、
`oauth-login-dialog.tsx`、`demo-action.tsx`、`product-marks.tsx` …），
通用件在 `src/components/common/`（`full-screen-panel.tsx`、`list-item-row.tsx`、
`management-list-search.tsx`）。

### 4.3 主题 token（Rhea）

`components.json`：

```json
{
  "$schema": "https://ui.shadcn.com/schema.json",
  "style": "radix-rhea",
  "tailwind": { "config": "", "css": "src/index.css", "baseColor": "neutral", "cssVariables": true },
  "iconLibrary": "lucide",
  "aliases": { "components": "@/components", "utils": "@/lib/utils", "ui": "@/components/ui", "lib": "@/lib", "hooks": "@/hooks" }
}
```

> 注意 `aliases.hooks` 指向 `@/hooks`，但**该目录不存在**；现有 hooks 全部放在
> `src/lib/use-*.ts`，新 hook 请跟随实际约定放 `src/lib/`。

`src/index.css` 定义全部 token（Tailwind v4，`@theme inline` 映射）：

- 基础：`--background --foreground --card --card-foreground --popover --popover-foreground`
  `--primary --primary-foreground --secondary --secondary-foreground --muted --muted-foreground`
  `--accent --accent-foreground --destructive --destructive-foreground --border --input --ring`
- 品牌：`--brand` / `--brand-foreground`（= 绿 `oklch(0.725 0.133 163.03)`，暗色 `oklch(0.696 0.17 162.48)`）
- 图表：`--chart-1..5`
- 侧栏：`--sidebar --sidebar-foreground --sidebar-primary --sidebar-accent --sidebar-border --sidebar-ring`
- **数据序列色（做图表务必用它）**：`--data-series-emerald / teal / indigo / violet / amber / rose / sky / lime`
- 圆角：`--radius: 0.75rem`，派生 `--radius-sm/md/lg/xl`
- 暗色通过 `.dark` class 切换；`@custom-variant dark (&:is(.dark *));`
- 主题偏好存 `localStorage` key `ai-gateway.theme`（`system|light|dark`），
  见 `src/lib/theme.ts`，在 `main.tsx` 挂载前 `applyTheme(getThemePreference())`

**颜色使用约定**：只用 token 语义类（`text-muted-foreground`、`bg-primary/10`、
`border-border`），不写死十六进制。数字用 `tabular-nums`，等宽用 `font-mono`。
大字标题字体族 `"Bricolage Grotesque Variable"`（`@fontsource-variable/bricolage-grotesque`）。

### 4.4 状态管理（zustand）

**只有 1 个 store**：`src/stores/accounts.ts`（143 行），`create<AccountsState>((set, get) => ({...}))`。

```ts
interface AccountsState {
  accounts: AccountMeta[];
  status: AppStatus | null;
  loading: boolean;
  error: string | null;
  creditMap: Record<string, CreditExpiry>;
  creditLoadingMap: Record<string, boolean>;
  /** 账号 id -> 最近一次积分查询完成时间（成功/失败都记录） */
  creditUpdatedAtMap: Record<string, number>;
  refreshingCredits: boolean;
  lastCreditRefreshAt: number;
  fetchAll: () => Promise<void>;
  refreshStatus: (signal?: AbortSignal) => Promise<void>;
  deleteAccount: (id: string) => Promise<void>;
  /** Fetch credits only for ids not already cached. */
  ensureCredits: (accountIds: string[]) => Promise<void>;
  /** Force-refresh credits. `silent` skips toolbar/card loading flicker (timer). */
  refreshCredits: (accountIds: string[], opts?: { silent?: boolean }) => Promise<void>;
  importLocal: () => Promise<{...}>;
  reconcileAccounts: () => Promise<void>;
}
```

**关键范式（新功能的「按账号查询 + 缓存 + 并发去重」应完全照抄这套）**：

1. **模块级 in-flight 集合**防重复请求：

```ts
/** In-flight credit fetches, shared so a remount does not start a second round. */
const creditInflight = new Set<string>();
let statusInflight: Promise<AppStatus> | undefined;
```

2. **单账号查询失败不抛，转成 `{ok:false, error}` 存进 map**：

```ts
async function fetchCreditExpiry(id: string): Promise<CreditExpiry> {
  try {
    return await api.getCreditExpiry(id);
  } catch (e) {
    return { ok: false, error: api.asError(e) };
  }
}
```

3. **`ensureXxx`（只补未缓存）与 `refreshXxx`（强制）分离**，
   以及 `silent` 选项（定时器刷新时不闪 loading）：

```ts
const toFetch = force
  ? ids
  : ids.filter((id) => state.creditMap[id] === undefined && !creditInflight.has(id));
```

4. `Promise.all` 并行 + 逐个写回 map（**每次 `set` 用展开新建对象**，保证引用变化触发重渲染）：

```ts
useAccountsStore.setState((s) => ({
  creditMap: { ...s.creditMap, [id]: result },
  creditUpdatedAtMap: { ...s.creditUpdatedAtMap, [id]: Date.now() },
  creditLoadingMap: silent ? s.creditLoadingMap : { ...s.creditLoadingMap, [id]: false },
}));
```

5. 删除账号时**连带清理**该账号的所有缓存键（`deleteAccount`，stores/accounts.ts:83-99）。

6. 页面级缓存（跨挂载存活）在 `CreditStatsPage.tsx:1215-1230` 用**模块级变量**实现：

```ts
let cachedStatistics: CreditStatistics | null = null;
let statisticsInflight: Promise<CreditStatistics> | null = null;
let lastStatisticsRefreshAt = 0;
```

### 4.5 数据拉取 / 刷新 hooks（`src/lib/use-*.ts`）

| 文件 | 导出 | 用途 |
|---|---|---|
| `use-visibility-interval.ts` | `useVisibilityInterval(callback, intervalMs, { enabled?, immediate?, onResume? })` | **通用可见性感知定时器（首选）** |
| `use-credit-auto-refresh.ts` | `useCreditAutoRefresh()`, `CREDIT_REFRESH_INTERVAL_MS = 30*60*1000` | 积分 30 分钟自动刷新（挂在 App 根） |
| `use-workbuddy-status-refresh.ts` | `useWorkbuddyStatusRefresh()`, `WORKBUDDY_STATUS_REFRESH_INTERVAL_MS = 60*1000` | 运行状态 60 秒轮询（需可见 **且** 窗口聚焦） |

`useVisibilityInterval` 的语义（文件头注释原文）：

> - `hidden` 时**清掉**定时器（而非在回调里 return）—— 这样隐藏期间没有任何
>   定时器存活，是真正的零开销，而不只是"不发请求"。
> - 恢复可见时立即跑一次 `onResume`（若提供），让界面立刻追上最新状态。
> - `enabled` 为 false 时同样不建定时器（如网关未运行就无需轮询）。
> - `options.immediate` 默认 **true**；若调用方已有独立首次加载 effect，传 `false`。

**实际用法**（AccountsPage.tsx:338-345）：

```tsx
const travelScopedIds = travelScopedAccounts.map((account) => account.id);
useVisibilityInterval(() => void loadTravelMap(travelScopedIds), 60_000, {
  enabled: travelScopedIds.length > 0,
  immediate: false,               // 首次加载由下方 useEffect 负责
  onResume: () => void loadTravelMap(travelScopedIds),
});
```

> **禁止裸 `setInterval`**：仓库注释多次强调窗口最小化/收托盘时组件不卸载，
> 裸定时器会在后台空转。

**页面加载范式**（AccountsPage.tsx:214-216 + 363-366）：

```tsx
useEffect(() => { void fetchAll(); }, [fetchAll]);

// 只给尚未缓存的账号拉积分；切回首页不重复请求。点「刷新积分」才强制更新。
useEffect(() => {
  if (!accounts.length) return;
  void ensureCredits(accounts.map((account) => account.id));
}, [accounts, ensureCredits]);
```

**取消语义**：用 `let cancelled = false; … return () => { cancelled = true; }`，
或 `AbortController`（`use-workbuddy-status-refresh.ts`）。

**Toast**：`import { toast } from "sonner"` → `toast.success/error/warning/info`，
错误描述统一 `{ description: api.asError(e) }`。

### 4.6 i18n 现状

**没有任何 i18n**。

- `package.json` 无 `i18next` / `react-intl` / `react-i18next` / `@formatjs/*`
- 全仓库 `src/` 下搜索 `i18n|react-intl|useTranslation` 只命中
  `String.prototype.localeCompare`（CreditStatsPage.tsx:809、TokenStatsPage.tsx:179）
- **所有界面文案是硬编码简体中文字面量**（`"账号管理"`、`"积分统计"`、`"暂无账号。点击上方按钮导入本机账号或 OAuth 登录。"`）
- 数字/日期本地化用 `Intl.NumberFormat("zh-CN", {...})` /
  `date.toLocaleString("zh-CN", {...})`（CreditStatsPage.tsx:74-96、account-card.tsx:40-57）

**新功能的文案直接写中文即可，不要引入 i18n 框架。**

---

## 5. 错误处理与日志约定

### 5.1 错误类型：**不用 anyhow / thiserror，全仓库无自定义 error enum**

- **没有** `anyhow` / `thiserror` / `thiserror` 依赖（core 的 `Cargo.toml` 无此二者）
- core 的公开函数签名统一两种：

| 形态 | 使用场景 | 示例 |
|---|---|---|
| `Result<T, String>` | 会失败的同步操作 | `account::delete_account(&str) -> Result<(), String>` |
| `Result<T, std::io::Result<...>>` | 落盘 | `account::save_accounts(&[Value]) -> std::io::Result<()>` |
| 裸 `Value` / `String` | **查询类接口**（失败信息塞进返回值） | `credits::get_credit_expiry(&Value) -> Value`（`{ok:false,error}`） |
| `Result<Value, String>` | 查询但有「参数非法/不存在」前置校验 | `doubao_quota::fetch_single(&str) -> Result<ParsedQuota, String>` |

- **查询类接口的惯例**：能查到就 `{ "ok": true, ... }`，查不到就把
  `{ "ok": false, "error": "..." }` 当**正常返回值**返回，而不是 `Err`。
  理由（见 `GatewayModelsResult` 注释）：让 UI 能把具体原因显示出来，
  而不是渲染一个空界面让用户猜。
- **Tauri 命令层**：`Result<Value, String>`，错误消息一律中文、面向用户、可操作：

```rust
return Err("账号不存在".to_string());
return Err("缺少 accountId".to_string());
return Err("该账号未配置 sessionid，无法查询额度".to_string());
return Err("登录态已失效，请开启代理重新抓取凭证或重新保存该账号登录态".to_string());
return Err(format!("官方请求失败（code={code}）：{message}"));
```

- **`?` 与 map_err 的写法**：

```rust
let acc = account::find_account(&account_id).ok_or("账号不存在")?;
account::save_accounts(&accounts).map_err(|e| e.to_string())?;
tauri::async_runtime::spawn_blocking(f)
    .await
    .map_err(|error| format!("查询应用状态失败: {error}"))?
```

- **HTTP 层错误**：`json_err(e: String, code: StatusCode)` → `{ok:false, error}`；
  业务校验失败用 `StatusCode::BAD_REQUEST`，并发冲突用 `CONFLICT`，
  `spawn_blocking` join 失败用 `INTERNAL_SERVER_ERROR`。
- **前端**：`api.asError(e)` 统一转字符串；store 里 catch 后写入 `error` 字段，
  或把 `{ok:false,error}` 存进 map（见 4.4）；UI 用 `<Alert variant="destructive">`
  显示并给「重试」按钮（CreditStatsPage.tsx:1311-1322）。

### 5.2 日志：**只用 `eprintln!` / `println!`，无 `log` / `tracing`**

全仓库 `crates/` 下搜索 `tracing::|log::(info|warn|error|debug)|use tracing|use log`
→ **零命中**。

约定格式：`eprintln!("[<域>] <中文消息>: {error}")`。

```rust
eprintln!("[account] 清理 {} 条无凭据残留记录（无法登录、从未采集）：{}", removed.len(), uids.join(", "));
eprintln!("[积分统计] 保存积分快照失败: {error}");
eprintln!("[签到] 历史日志整理失败: {error}");
eprintln!("[refresh] 已清除 {repaired} 个账号因网络失败误报的「需重新登录」标记");
eprintln!("[auth] write_account: existing is_object={} allAccounts_len={}", ...);
```

已知域标签：`[account]`、`[auth]`、`[atomic]`、`[refresh]`、`[签到]`、`[积分统计]`。

> **安全约定（doubao_account.rs 头注释原文）**：
> 凭证就是密码 … 界面展示一律脱敏（`account_view` 只给掩码）；
> **日志里只记录长度，绝不记录值**。
> 新增额度查询时，**token / sessionid / cookie 绝不能进日志或返回值**。

---

## 6. 构建与测试

### 6.1 构建 / 类型检查

**前端（`package.json` scripts）**：

```json
"dev": "vite",
"build": "tsc && vite build",          // ★ 类型检查 + 构建（CI 用它）
"build:demo": "tsc && VITE_DEMO_MODE=1 VITE_PAGES_DEMO=1 vite build",
"preview": "vite preview",
"tauri": "tauri"
```

- **类型检查就是 `tsc`**（`npm run build` 的第一步）。`tsconfig.json` 开了
  `"strict": true`、`"noUnusedLocals": true`、`"noUnusedParameters": true`、
  `"noFallthroughCasesInSwitch": true` —— **未使用的 import/变量会直接编译失败**。
- 路径别名 `@/* → ./src/*`（tsconfig `paths` + `vite.config.ts` `resolve.alias` 两处都有）。
- Vite dev server 端口固定 **14200**（`strictPort: true`）。

**Rust（workspace，根 `Cargo.toml`）**：

```toml
[workspace]
members = ["crates/ai-gateway-core", "crates/ai-gateway-router", "crates/ai-gateway-server", "src-tauri"]
resolver = "2"

[profile.release]
lto = "thin"
codegen-units = 4
strip = "debuginfo"

[profile.test]
opt-level = 0
```

常用命令（**Windows 下必须用 PowerShell**，见 AGENTS.md Shell Policy）：

```powershell
npm run build                                        # 前端类型检查 + 构建
cargo test -p ai-gateway-core                        # 核心单测（CI 主门）
cargo check --workspace --all-targets --profile test # 全 workspace 编译校验（CI 用它）
cargo build -p ai-gateway-server --release           # release 二进制校验
pwsh scripts/build-single.ps1                        # 单文件产物 dist-single/ai-gateway.exe
pwsh scripts/build-signed.ps1                        # 带 updater 签名的安装包
```

> `--profile test` **不可省**：`cargo test` 默认在 `test` profile 构建，
> `cargo check` 默认 `dev`，不加会同一 crate 编两遍（Windows 实测多 19–21s）。

### 6.2 测试组织

**Rust 单元测试（主战场）**：`#[cfg(test)] mod tests { ... }` **内联在模块文件末尾**。
`crates/ai-gateway-core/src/modules/*.rs` 里共 **38 处 `#[cfg(test)]` 模块、
409 个 `#[test]` 用例**（`Select-String -Pattern '#\[test\]'` 实测计数）。
运行：

```powershell
cargo test -p ai-gateway-core                       # 全部
cargo test -p ai-gateway-core credits               # 按名字过滤
cargo test --workspace                              # 含 src-tauri / server
```

测试命名：**既有英文 snake_case，也有中文函数名**（合法标识符），例如：

```rust
#[test] fn account_meta_strips_tokens() { ... }          // account.rs:495
#[test] fn 窗口类型码映射() { ... }                       // doubao_quota.rs:577
#[test] fn 额度视图形态() { ... }                         // doubao_quota.rs:584
#[test] fn store_dir_不指向用户真实账号库() { ... }        // tests/store_isolation.rs
```

**测试数据目录隔离（极其重要，踩过真实数据损坏事故）**：

- `config::test_isolation`（`#[cfg(test)]`）+ `#[ctor]` 在**任何线程创建前**
  把 `AI_GATEWAY_HOME` 定死到进程级临时目录
- 集成测试必须显式调用对外入口（`#[cfg(test)]` 不覆盖集成测试）：

```rust
// crates/ai-gateway-core/tests/store_isolation.rs
#[ctor::ctor]
fn isolate_store_dir() {
    ai_gateway_core::modules::config::pin_store_dir_for_tests("isolation-guard");
}
```

- `dev-dependencies` 里的 `ctor = "0.2"` 就是为此存在。
- **写测试时若会碰账号读写路径，必须确保隔离生效**，否则会写进用户真实
  `~/.wb-switch`（`account.rs:311-316` 记录了 16 条 `legacy-user` 僵尸账号的事故）。

**集成测试**：只有 `crates/ai-gateway-core/tests/store_isolation.rs` 一个文件
（`crates/*/tests/` 下别无他物）。

**其他 crate 的测试**（实测 `#[test]` 计数）：

| 位置 | 用例数 |
|---|---|
| `crates/ai-gateway-router/**/*.rs` | **268**（`anthropic.rs` 21、`responses.rs` 22、`classify.rs` 18 等） |
| `src-tauri/src/tray.rs` | 12（托盘菜单/图标/静默启动） |
| `crates/ai-gateway-server/src/api.rs:900` | 2（`web_checkin_status_*`） |
| `crates/ai-gateway-server/src/main.rs:229` | 1 |
| `src-tauri/src/commands.rs:607` | 1（`#[cfg(all(test, desktop))] mod relaunch_tests`） |
| `crates/ai-gateway-server/src/api/apps_api.rs`、`api/proxy_api.rs` | 0 |
| `src-tauri/src/commands_apps.rs`、`commands_proxy.rs` | 0 |

**JS/TS 测试：完全没有。**

- `package.json` 无 `vitest` / `jest` / `@testing-library/*` / `test` script
- 仓库根无 `vitest.config.*` / `jest.config.*`
- **前端唯一的自动化门禁是 `tsc`（`npm run build`）**

### 6.3 CI（`.github/workflows/ci.yml`）

三个 job：

1. **`check`**（矩阵 `ubuntu-22.04 / macos-14 / windows-latest`）：
   `npm ci` → `npm run build` → `cargo test -p ai-gateway-core`
   → `cargo check --workspace --all-targets --profile test`
   →（仅 macOS）`cargo check --workspace --all-targets --target x86_64-apple-darwin`
2. **`gateway`**：`sh scripts/build-gateway.sh`（Rust 网关 release 构建）
3. **`release-bin`**：`sh scripts/build-gateway.sh` → `npm ci` → `npm run build`
   → `cargo build -p ai-gateway-server --release`

> ⚠️ CI 在 Linux 上用 `sh`，但**本机开发环境是 Windows，必须用 PowerShell**（AGENTS.md Shell Policy）。

---

## 7. 给「账号登录额度查询」新功能的落地建议（基于以上约定）

**落点清单**：

1. **core**：新建 `crates/ai-gateway-core/src/modules/account_quota.rs`
   （或按 provider 命名，如 `workbuddy_quota.rs`），
   在 `modules/mod.rs` 加 `pub mod account_quota;`
   - 复用 `credits::authenticated_post(account, url, body)` 发请求（已含 token 惰性刷新 + 401 重试一次）
   - 复用 `credits::official_usage_url(account)` 的域名选择模式（`domain.ends_with(".ai")` → intl）
   - 返回 `{ "ok": bool, ..., "error": Option<String> }` 形态的 `Value`，
     **不要**用 `Err` 表达「上游查不到」
   - 若需要缓存：照 `doubao_account` 的 `quota_level/quota_expire_at/quota_summary/quota_checked_at`
     范式，或新建 `<store_dir>/xxx_quota_cache.json`（用 `config::atomic_write`）
   - 加 `#[cfg(test)] mod tests`，至少覆盖：解析正常响应、上游错误码、跨区域域名选择

2. **Tauri**：`src-tauri/src/commands.rs` 加

```rust
/// GET /api/account-quota —— 查询单账号的登录额度。
#[tauri::command]
pub async fn get_account_quota(account_id: String) -> Result<Value, String> {
    let acc = account::find_account(&account_id).ok_or("账号不存在")?;
    Ok(account_quota::get_quota(&acc).await)
}
```

   并在 `src-tauri/src/lib.rs` 的 `generate_handler!` 注册 `commands::get_account_quota`。

3. **HTTP**：`crates/ai-gateway-server/src/api.rs`
   - `.route("/api/account-quota", post(api_account_quota))`（或 `.get(...)` + `Query`）
   - handler 用 `json_ok` / `json_err`，账号不存在 → `json_err("账号不存在".to_string(), StatusCode::BAD_REQUEST)`

4. **前端**：
   - `src/lib/types.ts` 加 `AccountQuota` 接口（camelCase）
   - `src/lib/api.ts` 的 `ROUTES` 加 `get_account_quota: { method: "POST", path: "/api/account-quota" }`
     （只读的话加入 `DEMO_READ_COMMANDS` 并在 `screenshot-demo.ts` 加 case）
   - `export function getAccountQuota(accountId: string): Promise<AccountQuota> { return call("get_account_quota", { accountId }); }`
   - `src/stores/accounts.ts` 加 `quotaMap` / `quotaLoadingMap` / `quotaUpdatedAtMap` /
     `ensureQuotas` / `refreshQuotas`，完全照 `creditMap` 那一套（含 `quotaInflight` 去重集合、
     失败转 `{ok:false,error}`、`deleteAccount` 时清理）
   - `src/components/account-card.tsx` 加一个额度展示区（参考 `creditLoading` /
     `!credit.ok` / `credit.totalRemaining` 三态渲染，account-card.tsx:583-602），
     并在 `Props` 里加 `quota?: AccountQuota; quotaLoading?: boolean; quotaUpdatedAt?: number;`
   - 若要做独立页面：`src/pages/AccountQuotaPage.tsx` + `App.tsx` 的
     `<Route path="/account-quota" .../>` + 侧栏 `<NavLink to="/account-quota">`，
     容器/表头样式直接抄 `CreditStatsPage.tsx:1288-1309`
   - 定时刷新用 `useVisibilityInterval(cb, 30*60*1000, { enabled, immediate, onResume })`，
     **不要裸 `setInterval`**

**必须避开的坑**：

- 不要在任何位置写静态模型表（AGENTS.md 强制）；额度查询若涉及模型，只能实时拉
- 缓存键必须带区域（`(region, uid)` 或库内 `id`），裸 uid 会跨区域串号
- 日志与返回值里绝不能出现 token / sessionid / cookie
- 中文文案、`toast` + `api.asError(e)`、`Alert variant="destructive"` + 重试按钮
- 新字段若要让前端看见，必须加进 `account::account_meta()`
- 操作类按钮套 `<DemoAction>`；命令名/路由名必须同时加进 `ROUTES` 与
  `generate_handler!`，HTTP 侧还要加 `.route(...)`，三处缺一不可
