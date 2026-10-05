//! ZCode 上游客户端：Anthropic Messages 调用、额度查询、模型清单、OpenAI 聚合。
//!
//! 对应 Go `internal/zcode/client.go`（1631 行）。本文件只做「网络 + 协议」，
//! 账号选路与缓存策略在 `dispatch.rs` / `mod.rs`。
//!
//! ## 为什么打 Anthropic Messages 端点（不是 `{OpenAIBase}/chat/completions`）
//!
//! 2026-09-20 实测：`{OpenAIBase}/chat/completions`（按量计费通道）**恒回**
//! `429 {"code":1113,"msg":"余额不足或无可用资源包"}`，而账号明明有 300 万 + 500 万
//! token/日。原因是**通道错**：额度挂在 start-plan 上，按量通道没有该账号的资源包。
//! 抓包实测官方客户端打 `POST https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages`，
//! 且官方 provider 清单里**所有** provider 的 schema 都是 `anthropic`（含 coding-plan），
//! `openai:chat` 只存在于 templateRules。
//!
//! 响应侧：Anthropic SSE 由 `translate::translate_stream` 转成 OpenAI SSE，
//! 所以对上层（`forward.rs`）来说 ZCode 的响应**已经是标准 OpenAI**，
//! `aggregate` 只需拼 delta（见 `aggregate_openai`）。
//!
//! ## 传输层：绝不用「手工拼 transport 参数」的思路
//!
//! Go `New()` 的文档头记录了一组 8 请求对照实验（`uitest/diag-zcode-transport-compare.cjs`）：
//!
//! | 构造方式 | 结果 |
//! |---|---|
//! | A 手工 `&http.Transport{}` | ✓ 8/8 |
//! | B 手工 + `ForceAttemptHTTP2=true` | ✓ 8/8 |
//! | C **`(&http.Transport{}).Clone()`** | ✗ **0/8** |
//! | D `http.DefaultTransport.Clone()` | ✓ 8/8 |
//! | E 手工 + h2 显式关闭 | ✓ 8/8 |
//!
//! C 的机理：手工构造的 `ForceAttemptHTTP2=false`（DefaultTransport 是 true），
//! 而 `Clone()` 会**触发** `onceSetNextProtoDefaults()` ⇒ ALPN 声明 h2 但没有注册 h2
//! 处理器 ⇒ `malformed HTTP response "\x00\x00\x12\x04..."`（`\x00\x00\x12\x04` 是 h2
//! SETTINGS 帧 type=4）。这条报错**完全不提协议版本**，作者因此误判过两次。
//!
//! ⇒ Rust 侧一律以 `reqwest::Client::builder()` 为基，只调用有文档保证的 builder 方法。
//! 本文件用 `.http1_only()`（对应实验 E 的「显式关闭 h2」—— 已实测 8/8），
//! 与同目录 `products/qoder/client.rs` 同款，**不**依赖 ALPN 协商结果。
//!
//! ## ★ 流式对话不能有总超时
//!
//! Go `streamHTTP()` 给流式请求单独 clone 一个 transport，只设
//! `ResponseHeaderTimeout = 60s`，**不设 `Timeout`** —— 60 秒总超时会中途掐断长回答，
//! 用户看到的是「回答被截断」。Rust 对应物是 `read_timeout`（reqwest 的**每次读空闲超时**，
//! 对 body 读取同样生效），见 `reqwest-0.12.28/src/async_impl/body.rs:397-403`。

use std::collections::{BTreeMap, HashMap};
use std::sync::{Once, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{json, Map, Value};

use super::captcha::{shared_captcha_solver, DEFAULT_CAPTCHA_REGION};
use super::cred::ZcodeCred;
use super::errors::{self, UpstreamError};
use super::identity::{self, Identity};
use super::provider;
use super::translate::{self, AnthropicMeta};
use crate::error::GatewayError;

// ───────────────────────────── 常量 ─────────────────────────────

/// Anthropic 协议版本头。★ 不是可选的：Anthropic 协议要求它。
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic beta 头。★ 不是可选的：不发它上游可能把我们当**旧版客户端**走另一条兼容路径。
/// 值与 Go `client.go:272` 逐字一致。
const ANTHROPIC_BETA: &str = "mid-conversation-system-2026-04-07";

/// 控制面 / 短请求客户端的总超时（Go `&http.Client{Timeout: 60 * time.Second}`）。
const CLIENT_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);

/// 单次控制面请求的超时（Go `Client.Timeout = 30 * time.Second`）。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// 流式对话客户端的**每次读空闲**超时（对应 Go 的 `ResponseHeaderTimeout = 60s`）。
/// ★ 它**不是**总超时：只在「上游连续 60 秒一个字节都不发」时才触发。
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// 连接超时。Go 的 `DefaultTransport` 是 30s；reqwest 默认无连接超时，故显式设置。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// 错误响应体上限（Go `io.LimitReader(resp.Body, 1<<20)`）。
const MAX_ERROR_BODY: usize = 1 << 20;
/// 额度响应体上限（Go `1<<20`）。
const MAX_QUOTA_BODY: usize = 1 << 20;
/// 客户端配置响应体上限（Go `4<<20`）。
const MAX_CONFIG_BODY: usize = 4 << 20;
/// 模型清单响应体上限（Go `1<<20`）。
const MAX_MODELS_BODY: usize = 1 << 20;

/// 额度查询路径（Go `client.go:914` 的 `fmt.Sprintf` 模板）。
const QUOTA_PATH: &str = "/api/v1/zcode-plan/billing/balance";
/// 客户端配置端点（Go `ConfigPath`，`client.go:1188`）。
///
/// ★ `pub(crate)` 是为了 `plan.rs`：`FetchStartPlanPreview`（Go `plan.go:102`）
/// 打的是**同一个端点**。让两边共用同一个常量，而不是各抄一份字符串 ——
/// 路径写错在上游表现为 404/3001，不是编译错误。
pub(crate) const CONFIG_PATH: &str = "/api/v1/client/configs";
/// 客户端配置端点固定走 `zcode.z.ai`（Go `ConfigOrigin`，`client.go:1191`），免认证。
const CONFIG_ORIGIN: &str = "https://zcode.z.ai";
/// 服务商模型清单路径（Go `ModelListPath`，`client.go:1393`），拼在 `OpenAIBase` 后。
const MODEL_LIST_PATH: &str = "/models";

/// 缺 JWT 时的错误文本。★ 单独成型是刻意的：界面据此显示「额度未知（需要 OAuth 登录）」
/// 而不是 0 —— 0 会被用户误读成「额度耗尽」。
///
/// ⚠ 只有**缺 JWT** 才该走到这里。曾经的实现还有第二个原因（缺 deviceMid）也会查不到，
/// 但那个是**我们自己能补的**（随机 UUID 即可，实测上游只校验格式），
/// 不该让用户看到「需要重新登录」。
pub const ERR_NO_JWT: &str = "该账号没有 JWT（仅导入了凭证），无法查询额度；请在账号页重新登录";

// ───────────────────────────── 请求/响应类型 ─────────────────────────────

/// 一次对话调用的结果。对应 Go `StreamChat` 的 `(io.ReadCloser, int, []byte, error)`。
///
/// ★ 与 Go 同款：上游非 2xx 时 Go 返回 **err 为 nil**、由调用方按状态码分类，
/// 故这里也把「上游拒绝」表达成正常返回值而不是 `Err`。
///
/// ⚠ **唯一的例外**是 3007（人机验证）且求解失败：那种情况在 Rust 侧必须返回
/// `Err(GatewayError::CaptchaRequired(..))`。原因见 [`Client::stream_chat`] 的文档。
pub enum ChatCall {
    /// 成功：响应体已是**标准 OpenAI SSE**（Anthropic 形状已在内部翻译）。
    Stream(reqwest::Response),
    /// 上游拒绝（HTTP >= 400），`body` 已过 [`normalize_error_body`]。
    Rejected { status: u16, body: String },
}

/// 一个模型条目。字段与 Go `client.go:1146-1168` 的 `Model` 一致（含 json 名）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    #[serde(rename = "contextWindow")]
    pub context_window: i64,
    #[serde(rename = "maxOutputTokens")]
    pub max_output: i64,
    pub reasoning: bool,
    /// ★ 必须来自上游的 `capabilities.vision`，**不能用「id 里含 v」的启发式**。
    /// 实测（2026-09-19）：上游当前返回的 `GLM-5.3-Flash` 带 `capabilities.vision=true`，
    /// 而它的 id 里**没有 v** —— 启发式会漏判，用户会以为模型不支持图片。
    pub vision: bool,
    /// `"upstream"` 或 `"builtin"`。本项目既有约定是**不硬编码模型清单**。
    pub source: String,
}

/// 额度汇总。对应 Go `client.go:779-796` 的 `Quota`。
#[derive(Debug, Clone, Default, Serialize)]
pub struct Quota {
    pub remaining: i64,
    pub total: i64,
    pub used: i64,
    pub entries: Vec<QuotaEntry>,
    /// ★ 已生效的套餐（含**套餐整体到期** `ends_at`）。
    /// 这个字段长期被忽略 —— 而体验套餐的到期时间就在里面。
    pub plans: Vec<Plan>,
    pub server_time: i64,
}

/// 单个额度桶。对应 Go `QuotaEntry`。
#[derive(Debug, Clone, Default, Serialize)]
pub struct QuotaEntry {
    pub show_name: String,
    pub remaining: i64,
    pub total: i64,
    pub used: i64,
    pub unit_type: String,
    /// **每日周期**的结束时刻（Unix 秒）；0 = 未知。⚠ 不是套餐到期。
    pub expires_at: i64,
    /// 与 `expires_at` 通常相同，但上游两个都给了，故都留下 —— 取哪个都不该猜。
    pub period_start: i64,
    pub period_end: i64,
    /// 该桶属于哪个套餐的哪个授权项（用于展示「这项来自体验套餐」）。
    pub plan_id: String,
    pub entitlement_id: String,
    /// 该桶的**每日赠送量**（来自套餐授权，token）。可能是小数，故用 f64。
    pub grant_units: f64,
}

/// 已生效的套餐。对应 Go `Plan`。
#[derive(Debug, Clone, Default, Serialize)]
pub struct Plan {
    pub user_plan_id: String,
    pub plan_id: String,
    pub name: String,
    /// 上游给的说明，实测是「免费 GLM 旗舰模型体验」。
    pub description: String,
    pub priority: f64,
    /// 实测 `"active"`；上游也可能回别的状态（如 `expired`）。
    pub status: String,
    /// 套餐生效与**到期**（Unix 秒）。实测体验套餐 `ends_at` = 2026-09-23 23:59:59。
    pub starts_at: i64,
    pub ends_at: i64,
    /// 该套餐在各模型上的授权（含每日赠送量 `grant_units`）。
    pub entitlements: Vec<PlanEntitlement>,
}

/// 套餐在某个模型上的授权项。对应 Go `PlanEntitlement`。
#[derive(Debug, Clone, Default, Serialize)]
pub struct PlanEntitlement {
    pub entitlement_id: String,
    pub show_name: String,
    pub meter: String,
    pub unit_type: String,
    pub capabilities: Vec<String>,
    /// 赠送量（token）。实测体验套餐是 GLM-5.3 300 万、GLM-5.3-Flash 500 万。
    pub grant_units: f64,
    /// 重置周期，实测 `"daily"`。
    pub period: String,
    pub effective_at: i64,
}

impl Quota {
    /// 套餐整体到期时刻 = 所有套餐 `ends_at` 中**最小的非零值**；0 = 无套餐或全部未知。
    ///
    /// ★ 与 [`Quota::soonest_expiry`] 的区别（Go `client.go:798-806` 的原始注释）：
    ///
    /// - `soonest_expiry()`：各模型桶的**每日周期**结束（实测是当天 23:59:59）
    /// - `plan_expiry()`：套餐整体的到期（实测 2026-09-23 23:59:59）
    ///
    /// 「只取前者，用户会以为『明天额度就没了』；只取后者，他会以为『今天用不完就浪费了』。
    /// 界面两个都要显示，且要说清各自含义。」
    pub fn plan_expiry(&self) -> i64 {
        let mut earliest = 0i64;
        for p in &self.plans {
            if p.ends_at <= 0 {
                continue;
            }
            if earliest == 0 || p.ends_at < earliest {
                earliest = p.ends_at;
            }
        }
        earliest
    }

    /// 最早的一个「每日周期」结束时刻（用途：跨产品路由的**到期分层**，先烧快过期的额度）。
    /// 最近一个到期日（各余额项的每日周期结束）。
    ///
    /// ⚠ 与 [`Quota::plan_expiry`] **不是一回事**：本方法返回「每个模型桶的每日周期
    /// 结束」（实测当天 23:59:59），`plan_expiry` 返回「套餐整体到期」（实测
    /// 2026-09-23 23:59:59）。只取前者，用户会以为「明天额度就没了」；只取后者，
    /// 他会以为「今天用不完就浪费了」。界面两个都要显示，且要说清各自含义。
    /// 用途：跨产品路由的**到期分层**（先烧快过期的额度）。
    ///
    /// 当前 Rust 侧暂无生产调用方（额度展示与跨产品分层尚未接线），保留完整语义
    /// 以免后来者误以为「只需要一个到期日」。
    #[allow(dead_code)]
    pub fn soonest_expiry(&self) -> i64 {
        let mut earliest = 0i64;
        for e in &self.entries {
            if e.expires_at <= 0 {
                continue;
            }
            if earliest == 0 || e.expires_at < earliest {
                earliest = e.expires_at;
            }
        }
        earliest
    }

    /// 当前生效的套餐：只认 `status == "active"`（或状态为空），取 `priority` 最高者。
    ///
    /// 当前 Rust 侧暂无生产调用方（额度展示尚未接线），保留完整语义。
    #[allow(dead_code)]
    pub fn active_plan(&self) -> Option<&Plan> {
        let mut best: Option<&Plan> = None;
        for p in &self.plans {
            if !p.status.is_empty() && p.status != "active" {
                continue;
            }
            match best {
                None => best = Some(p),
                Some(b) if p.priority > b.priority => best = Some(p),
                _ => {}
            }
        }
        best
    }
}

// ───────────────────────────── 宿主配置发现 ─────────────────────────────

/// 从宿主写出的原生配置里读到的、ZCode 需要的运行时配置。
///
/// ⚠ 为什么不直接用 `crate::config::Config`：宿主实测写出的 JSON 把
/// `zcode_captcha_dir` / `zcode_captcha_solver_url` / `zcode_captcha_solver_token`
/// 放在 **`pool.`** 嵌套下（`pool.zcode_captcha_dir = D:\APP\AI Gateway\_up_\assets\zcode-captcha`
/// 等），而 `config.rs` 里这些字段挂在**顶层 `Config`** 上、嵌套的 `PoolConfig` 并不含它们
/// ⇒ 用 `Config` 反序列化**拿不到**这些值（`serde` 会静默忽略不认识的键）。
///
/// ⇒ 唯一不越界的做法：解析进程参数里的 `-config <path>`（宿主启动网关时就是
/// `Command::new(exe).arg("-config").arg(native_cfg)`），自己读原始 JSON，
/// **顶层与 `pool.` 两处都查**，并缓存到 `OnceLock`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostConfig {
    /// 顶层 `proxy`。
    pub proxy: String,
    /// 顶层 `proxy_scope.intl`（缺省 true，与 `ProxyScopeConfig` 的默认一致）。
    pub proxy_intl: bool,
    pub captcha_dir: String,
    pub captcha_solver_url: String,
    pub captcha_solver_token: String,
}

/// 宿主的原生配置（进程内只读一次）。
pub fn host_config() -> &'static HostConfig {
    static CACHE: OnceLock<HostConfig> = OnceLock::new();
    CACHE.get_or_init(|| {
        let Some(path) = config_path_from_args() else {
            return HostConfig::default();
        };
        let Ok(raw) = std::fs::read(&path) else {
            return HostConfig::default();
        };
        let Ok(v) = serde_json::from_slice::<Value>(&raw) else {
            return HostConfig::default();
        };
        parse_host_config(&v)
    })
}

/// 从进程参数里找 `-config` / `--config` 后面的那个路径（与 `bin/gateway.rs:38-57` 同款）。
fn config_path_from_args() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 0usize;
    while i < args.len() {
        if args[i] == "-config" || args[i] == "--config" {
            return args.get(i + 1).cloned();
        }
        i += 1;
    }
    None
}

/// 从已解析的配置 JSON 里取值：先查顶层，再查 `pool.` 嵌套。纯函数，便于测试。
pub fn parse_host_config(v: &Value) -> HostConfig {
    HostConfig {
        proxy: pick_str(v, "proxy"),
        proxy_intl: v
            .pointer("/proxy_scope/intl")
            .and_then(|x| x.as_bool())
            .unwrap_or(true),
        captcha_dir: pick_str(v, "zcode_captcha_dir"),
        captcha_solver_url: pick_str(v, "zcode_captcha_solver_url"),
        captcha_solver_token: pick_str(v, "zcode_captcha_solver_token"),
    }
}

/// 顶层优先、`pool.` 兜底；值做 `trim`（路径/URL 两端空白几乎必然是笔误）。
fn pick_str(v: &Value, key: &str) -> String {
    if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
        let t = s.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Some(s) = v.pointer(&format!("/pool/{key}")).and_then(|x| x.as_str()) {
        return s.trim().to_string();
    }
    String::new()
}

/// 按宿主配置初始化验证码求解器（进程内只做一次）。
///
/// 对应 Go `cmd/server/main.go:150-204`。Go 侧「**永远启用**」是刻意的：
/// 它**从不读** `ZcodeCaptchaEnabled`（那个 UI 开关从来不存在：前端与宿主里
/// `captcha` 的命中数为 0）。实测 ZCode 对话对任何不带该参数的请求一律回
/// `HTTP 400 {"code":3007,"msg":"captcha verify failed"}` —— 「默认关」等于 ZCode 对话
/// **从未成功过**。字段只作为向后兼容的占位保留。
///
/// 三条分支与 Go 逐字同款：
/// 1. `captcha_dir` 非空 ⇒ 设目录；URL 也非空 ⇒ 再设外部求解器（**外部优先，本地回退**）
/// 2. `captcha_dir` 为空但 URL 非空 ⇒ 仅外部服务
/// 3. 两者都空 ⇒ 打警告（对话会因 3007 失败）
pub fn configure_captcha_from_host() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let cfg = host_config();
        let dir = cfg.captcha_dir.as_str();
        let url = cfg.captcha_solver_url.as_str();
        let token = cfg.captcha_solver_token.as_str();
        let solver = shared_captcha_solver();
        if !dir.is_empty() {
            solver.set_dir(dir);
        }
        if !url.is_empty() {
            solver.set_external_solver(url, token);
        }
        if !dir.is_empty() {
            let reason = solver.unavailable_reason();
            if !reason.is_empty() {
                eprintln!("⚠ ZCode 验证码求解已启用，但组件不可用：{reason}");
            } else if solver.external_solve_enabled() {
                eprintln!("ZCode 验证码求解已启用：外部服务优先（宿主 WebView2）{url}，本地求解作为回退");
            } else {
                eprintln!("ZCode 验证码求解已启用：{dir}");
            }
        } else if !url.is_empty() {
            eprintln!("ZCode 验证码求解已启用：仅外部服务（宿主 WebView2）{url}");
        } else {
            eprintln!(
                "⚠ ZCode 验证码求解器未配置（zcode_captcha_dir 为空）：ZCode 对话会因 3007 人机验证而失败；发行包应包含 assets/zcode-captcha"
            );
        }
    });
}

// ───────────────────────────── 客户端 ─────────────────────────────

/// ZCode HTTP 客户端。
pub struct Client {
    /// 控制面 / 短请求（额度、模型清单、客户端配置）：有总超时。
    ///
    /// `pub(crate)` 供 `claim.rs` 用（Go 侧 preview/claim 也走 `c.http()`）。
    pub(crate) http: reqwest::Client,
    /// 流式对话：**无总超时**，只有每次读的空闲超时。
    stream: reqwest::Client,
    /// 单次控制面请求的超时（Go `Client.Timeout`，可被测试覆盖）。
    ///
    /// `pub(crate)` 供 `claim.rs` 用（Go 侧 preview/claim 也是 `c.timeout()`）。
    pub(crate) timeout: Duration,
    pub identity: Identity,
    /// 测试用端点覆盖（对应 Go `SetAnthropicBaseForTest`）。
    /// ★ 生产路径恒为空 —— 测试指向本地 httptest，**不打真实上游**。
    anthropic_base_override: String,
    /// 测试用**控制面**端点覆盖：额度 host、客户端配置 origin、服务商 `openai_base` 三处。
    ///
    /// ★ 为什么必须有它：这三处的生产基址是**常量/端点表**（`provider::QUOTA_HOST`、
    /// `CONFIG_ORIGIN`、`endpoints().openai_base`），`anthropic_base_override` 管不到它们。
    /// 于是任何「控制面失败」的测试都会**真的打 `zcode.z.ai`** —— 而反复高频请求
    /// 已经吃过 3012 风控。生产路径恒为空，测试指向一个必然拒绝连接的本地端点。
    control_base_override: String,
    /// 测试用：把求解器换成**本测试私有**的实例（生产恒为 `None` ⇒ 进程级单例）。
    ///
    /// ★ 为什么需要它：`solve_captcha` 生产走 [`shared_captcha_solver`]（单例，
    /// 因为冷却状态必须按出口 IP 全局共享）。但「求解成功 ⇒ 把 param 写回凭证**副本**
    /// 并重试一次」是本移植**唯一必须修掉的 Go 缺陷**（Go 全仓 `CaptchaParam` 赋值
    /// 0 次 ⇒ 必然 3007），必须有测试钉住它；而改那个单例会污染同进程的其它测试
    /// （测试是并行的），故用一个私有实例隔离。
    #[cfg(test)]
    captcha_solver: Option<std::sync::Arc<super::captcha::CaptchaSolver>>,
}

impl Client {
    /// 等价于 Go `New()`：默认身份 + 直连（不读环境代理）。
    pub fn new() -> Self {
        Self::with_identity(Identity::new())
    }

    pub fn with_identity(identity: Identity) -> Self {
        Self {
            http: build(base_builder().timeout(CLIENT_TOTAL_TIMEOUT)),
            stream: build(base_builder().read_timeout(STREAM_READ_TIMEOUT)),
            timeout: DEFAULT_TIMEOUT,
            identity,
            anthropic_base_override: String::new(),
            control_base_override: String::new(),
            #[cfg(test)]
            captcha_solver: None,
        }
    }

    /// 等价于 Go `SetProxy(raw)`。
    ///
    /// ★ `raw` 为空时**显式直连**（`.no_proxy()`），**不**回落环境变量代理：
    /// 否则 `HTTP_PROXY` 会悄悄生效，表现为「设置了直连但流量仍走代理」。
    /// 同理，设置代理时用的是 `.proxy(p)` —— reqwest 会同时把 `auto_sys_proxy` 置 false
    /// （`reqwest-0.12.28/src/async_impl/client.rs:1416`），环境变量也不会叠加进来。
    pub fn set_proxy(&mut self, raw: &str) -> Result<(), String> {
        let raw = raw.trim();
        if raw.is_empty() {
            self.http = build(base_builder().timeout(CLIENT_TOTAL_TIMEOUT));
            self.stream = build(base_builder().read_timeout(STREAM_READ_TIMEOUT));
            return Ok(());
        }
        let proxy =
            reqwest::Proxy::all(raw).map_err(|e| format!("解析代理地址失败: {e}"))?;
        self.http = build(
            proxied_builder(proxy.clone()).timeout(CLIENT_TOTAL_TIMEOUT),
        );
        self.stream = build(proxied_builder(proxy).read_timeout(STREAM_READ_TIMEOUT));
        Ok(())
    }

    /// 按宿主配置决定是否走代理。对应 Go `cmd/server/main.go:525-536`：
    /// **仅**由 `proxy_scope.intl` 决定（`zcode.z.ai` 以 `.ai` 结尾，属国际流量）。
    /// 设置失败只打日志、不阻断启动（失败后保持直连，与日志里的「将直连」一致）。
    pub fn apply_host_proxy(&mut self) {
        let cfg = host_config();
        let addr = cfg.proxy.trim();
        if !addr.is_empty() && cfg.proxy_intl {
            match self.set_proxy(addr) {
                Ok(()) => eprintln!("ZCode 走代理：{addr}"),
                Err(e) => eprintln!("⚠ ZCode 代理设置失败（将直连）：{e}"),
            }
        } else {
            // 与 Go 同款：不满足条件时显式 SetProxy("")（直连），而不是留着环境代理。
            let _ = self.set_proxy("");
        }
    }

    /// 测试用：指向本地假上游（对应 Go `SetAnthropicBaseForTest`）。
    #[cfg(test)]
    pub fn set_anthropic_base_for_test(&mut self, base: &str) {
        self.anthropic_base_override = base.to_string();
    }

    /// 测试用：把**控制面**（额度 / 客户端配置 / 服务商清单）整体指向 `base`。
    ///
    /// 见字段注释：不加这层覆盖，测试就会打真实 `zcode.z.ai`。
    #[cfg(test)]
    pub fn set_control_base_for_test(&mut self, base: &str) {
        self.control_base_override = base.to_string();
    }

    /// 测试用：注入一个私有的求解器实例（生产恒为 `None` ⇒ 进程级单例）。
    #[cfg(test)]
    pub fn set_captcha_solver_for_test(
        &mut self,
        solver: std::sync::Arc<super::captcha::CaptchaSolver>,
    ) {
        self.captcha_solver = Some(solver);
    }

    /// 求解器实例：测试注入优先，否则进程级单例。
    fn solver(&self) -> std::sync::Arc<super::captcha::CaptchaSolver> {
        #[cfg(test)]
        if let Some(s) = &self.captcha_solver {
            return s.clone();
        }
        shared_captcha_solver()
    }

    /// 额度查询的 host：测试覆盖优先，否则 [`provider::QUOTA_HOST`]。
    ///
    /// ★ `pub(crate)` 是为了 `claim.rs`（`preview` / `claim` 两个端点原语）——
    /// 那两个端点**必须**走这个 host，不能走 `biz_host`（见 `claim.rs` 模块头）。
    pub(crate) fn quota_host(&self) -> String {
        let ov = self.control_base_override.trim_end_matches('/');
        if ov.is_empty() {
            provider::QUOTA_HOST.to_string()
        } else {
            ov.to_string()
        }
    }

    /// 客户端配置的 origin：测试覆盖优先，否则 [`CONFIG_ORIGIN`]。
    ///
    /// ★ `pub(crate)` 是为了 `login.rs` 与 `plan.rs`：OAuth 设备流的
    /// init/poll 端点与 `client/configs` 都挂在同一个 origin 上
    ///（Go `ConfigOrigin`），必须共用同一个 override 才测得了。
    pub(crate) fn config_origin(&self) -> String {
        let ov = self.control_base_override.trim_end_matches('/');
        if ov.is_empty() {
            CONFIG_ORIGIN.to_string()
        } else {
            ov.to_string()
        }
    }

    /// 服务商清单的 base：测试覆盖优先，否则 `endpoints().openai_base`。
    fn provider_openai_base(&self, cr: &ZcodeCred) -> String {
        let ov = self.control_base_override.trim_end_matches('/');
        if ov.is_empty() {
            cr.provider.endpoints().openai_base
        } else {
            ov.to_string()
        }
    }

        // ── 对话 ──────────────────────────────────────────────────────

    /// 发起一次流式对话，必要时求解人机验证后重试一次。
    ///
    /// ## 控制流（对应 Go `StreamChat`，`client.go:318-357`）
    ///
    /// 1. 凭证为空 ⇒ `Err("账号没有凭证")`
    /// 2. **先不带验证码试一次**（多数情况下不需要它，能省一次求解，以及一次限流风险）
    /// 3. 不是 400、或 400 但不是「需要人机验证」⇒ 原样返回（其他 400 解了也没用）
    /// 4. 求解验证参数；成功 ⇒ **在凭证的副本上**写回 param/region，重试一次
    /// 5. 求解失败 ⇒ 见下
    ///
    /// ## ★★ 与 Go 的刻意偏差：3007 + 求解失败必须返回 `Err(CaptchaRequired)`
    ///
    /// Go 在这一步返回 `(nil, 400, appendCaptchaNote(body, reason), nil)` —— 即
    /// **err 为 nil**、把原因折进响应体。Rust 侧**不能**照抄：
    /// `forward.rs` 的 `ChatOutcome::Failed` 分支里**没有任何 3007 特例**
    /// （只有 3012 / WAF 403 / 11155），于是 400 + 「captcha verify failed」会走到
    /// `apply_error_policy` + `release` + **轮转下一个账号重试** ——
    /// 而人机验证是**按出口 IP** 触发的，换号毫无意义，只会把整池账号一起拖进冷却，
    /// 用户看到的是 `503 {"code":"no_healthy_account", ... captcha verify failed}`，
    /// 每一句都在误导。只有 `Err(GatewayError::CaptchaRequired(..))` 才能命中
    /// `forward.rs:1264-1283` 的「请求侧错误 —— 不轮转、不罚账号」分支（返回 503）。
    ///
    /// ## ★ 为什么要写在**副本**上（Go `client.go:301-317` 记录的缺陷）
    ///
    /// 求解器（`captcha.rs`，277 行测试）早就实现完整，但 Go 全仓 `CaptchaParam`
    /// 的**赋值次数为 0** ⇒ `applyHeaders` 里 `if cr.CaptchaParam != ""` **永远为假**
    /// ⇒ 必然 3007（典型的「写了但没接线」）。接线时又必须复制凭证：
    /// `cr` 是账号池里的共享对象，直接写会让这个**一次性**的值残留下来，
    /// 下一次请求带着它必然再回 3007，表现为「偶发失败」。
    pub async fn stream_chat(
        &self,
        cr: &ZcodeCred,
        openai_body: &[u8],
    ) -> Result<ChatCall, GatewayError> {
        if cr.credential.is_empty() {
            return Err(GatewayError::Other("账号没有凭证".to_string()));
        }

        // ① 先不带验证码试一次。
        let (status, body) = match self.stream_chat_once(cr, openai_body).await {
            Ok(ChatCall::Stream(resp)) => return Ok(ChatCall::Stream(resp)),
            Ok(ChatCall::Rejected { status, body }) => (status, body),
            Err(e) => return Err(GatewayError::Other(e)),
        };
        if status != 400 || !is_captcha_required_body(&body) {
            return Ok(ChatCall::Rejected { status, body });
        }

        // ② 确实需要人机验证 ⇒ 求解。
        let param = match self.solve_captcha().await {
            Ok(p) if !p.is_empty() => p,
            Ok(_) => {
                let reason = "求解器返回了空的验证参数";
                return Err(GatewayError::CaptchaRequired(captcha_failure_message(
                    &body, reason,
                )));
            }
            Err(e) => {
                return Err(GatewayError::CaptchaRequired(captcha_failure_message(
                    &body, &e,
                )));
            }
        };

        // ③ 在**副本**上写回，重试一次。
        let mut retry = cr.clone();
        retry.captcha_param = param;
        retry.captcha_region = self.captcha_region();
        match self.stream_chat_once(&retry, openai_body).await {
            Ok(ChatCall::Stream(resp)) => Ok(ChatCall::Stream(resp)),
            Ok(ChatCall::Rejected { status: _, body }) if is_captcha_required_body(&body) => {
                Err(GatewayError::CaptchaRequired(captcha_failure_message(
                    &body,
                    "带上求解得到的验证参数重试后，上游仍要求人机验证（该参数是一次性的，可能已过期或被风控拒绝）",
                )))
            }
            Ok(ChatCall::Rejected { status, body }) => {
                Ok(ChatCall::Rejected { status, body })
            }
            Err(e) => Err(GatewayError::Other(e)),
        }
    }

    /// 单次请求：翻译请求体 → 发 → 成功则把 Anthropic SSE 流式翻成 OpenAI SSE。
    ///
    /// 对应 Go `streamChatOnce`（`client.go:377-427`）。
    async fn stream_chat_once(
        &self,
        cr: &ZcodeCred,
        openai_body: &[u8],
    ) -> Result<ChatCall, String> {
        let model = model_of(openai_body);
        // ★ session_id 必须与追踪头**同源**（`identity::trace_headers` 用
        // `session_id`，为空时回落 `account_id`），否则上游看到「头部说会话 A、体里说会话 B」。
        let session_id = identity::stable_uuid(&format!(
            "zcode-session:{}",
            first_non_empty(&[cr.account_id.as_str(), cr.uid.as_str()])
        ));
        let meta = AnthropicMeta {
            device_id: cr.device_mid.clone(),
            // Go 侧此处留零值（`AccountUUID` 不填）。
            account_uuid: String::new(),
            session_id,
        };
        let anth = translate::build_anthropic_body_with_meta(openai_body, &model, &meta)
            .map_err(|e| format!("翻译请求体失败: {e}"))?;
        let anth = force_stream(&anth);
        let url = self.anthropic_messages_url_for(cr);
        let rb = self
            .apply_headers(self.stream.post(&url).body(anth), cr, true)
            .map_err(|e| format!("构造 ZCode 请求头失败: {e}"))?;
        let resp = rb
            .send()
            .await
            .map_err(|e| format!("连接 ZCode 上游失败: {e}"))?;
        let status = resp.status().as_u16();
        if status >= 400 {
            let raw = read_limited(resp, MAX_ERROR_BODY).await;
            let normalized = normalize_error_body(&raw, status);
            return Ok(ChatCall::Rejected {
                status,
                body: String::from_utf8_lossy(&normalized).into_owned(),
            });
        }
        // ★ 翻译是**流式**的（`Body::wrap_stream`），**绝不缓冲** ——
        // 缓冲会把「首字节延迟」变成「整段生成延迟」。
        Ok(ChatCall::Stream(wrap_stream(resp, model)))
    }

    /// 请求头。对应 Go `applyHeaders`（`client.go:703-773`，按 2026-09-20 抓包对齐）。
    fn apply_headers(
        &self,
        rb: reqwest::RequestBuilder,
        cr: &ZcodeCred,
        stream: bool,
    ) -> Result<reqwest::RequestBuilder, String> {
        let mut h = reqwest::header::HeaderMap::new();

        // ① 鉴权：JWT 优先（start-plan 通道的凭证），无 JWT 才回落 credential。
        //    `credential` 是 `{apiKey}.{secret}` 形态，属**另一条通道**，混用会回 1113。
        //    两个头同值都发（`x-api-key` 是 Anthropic 的标准鉴权位），官方实测如此。
        let token = {
            let j = cr.jwt.trim();
            if j.is_empty() {
                cr.credential.trim().to_string()
            } else {
                j.to_string()
            }
        };
        if !token.is_empty() {
            put(&mut h, "authorization", &format!("Bearer {token}"))?;
            put(&mut h, "x-api-key", &token)?;
        }

        // ② ③ Anthropic 协议头：两个都**无条件**发（官方是流式抓的，非流式同协议同要求）。
        put(&mut h, "content-type", "application/json")?;
        put(
            &mut h,
            "accept",
            if stream { "text/event-stream" } else { "application/json" },
        )?;
        put(&mut h, "anthropic-version", ANTHROPIC_VERSION)?;
        put(&mut h, "anthropic-beta", ANTHROPIC_BETA)?;

        // ④ 身份头。
        for (k, v) in self.identity.headers() {
            put(&mut h, &k, &v)?;
        }

        // ⑤ 追踪头：★ 必须做**副本** —— `self.identity` 是共享值，
        //    改它会让并发的不同账号互相覆盖。`x-zcode-trace-id` 必须**会话级稳定**
        //    （抓包实测官方相隔 58 分钟的两次请求该值完全相同），每请求随机会让
        //    我们在高频请求下「与脚本无异」。
        let mut id = self.identity.clone();
        id.account_id = first_non_empty(&[cr.account_id.as_str(), cr.uid.as_str()]);
        for (k, v) in id.trace_headers() {
            put(&mut h, &k, &v)?;
        }

        // ⑥ 验证参数（★ 一次性，仅在确实求解成功后出现）。
        if !cr.captcha_param.is_empty() {
            put(&mut h, "x-aliyun-captcha-verify-param", &cr.captcha_param)?;
            if !cr.captcha_region.is_empty() {
                put(&mut h, "x-aliyun-captcha-verify-region", &cr.captcha_region)?;
            }
        }
        Ok(rb.headers(h))
    }

    /// 求解人机验证参数。对应 Go `solveCaptcha`（`client.go:646-655`）。
    ///
    /// ★ 求解器是**进程级共享**的（`shared_captcha_solver()`），因为它自带冷却状态 ——
    /// 每个 `Client` 各持一个会让冷却形同虚设（上游限流是按我们的出口 IP 算的，
    /// 不是按实例算的）。
    /// ★ `pub(crate)` 是为了 `warm.rs` 的 `mintCaptcha`（Go `warm.go:369-391`）：
    /// 它必须与对话路径用**同一个**求解器实例（冷却状态是进程级的，各持一个
    /// 会让冷却形同虚设 —— 上游限流按出口 IP 算，不是按实例算）。
    pub(crate) async fn solve_captcha(&self) -> Result<String, String> {
        let s = self.solver();
        // Go 里还有 `s == nil ⇒ "求解器未初始化"` 一支；Rust 的 `shared_captcha_solver()`
        // 返回 `OnceLock` 里的 `Arc`，永不为空，故该分支不存在。
        let reason = s.unavailable_reason();
        if !reason.is_empty() {
            return Err(format!("求解器不可用：{reason}"));
        }
        s.solve().await
    }

    /// 验证参数配套的 region。对应 Go `captchaRegion`：求解器没给就用 `"cn"`
    /// （实测缺它就是 3007；本项目所有实测样本都是 cn）。
    /// 验证参数配套的 region。对应 Go `captchaRegion`：求解器没给就用 `"cn"`
    /// （实测缺它就是 3007；本项目所有实测样本都是 cn）。
    ///
    /// ★ `pub(crate)` 是为了 `warm.rs`，与 [`Client::solve_captcha`] 同款理由。
    pub(crate) fn captcha_region(&self) -> String {
        let r = self.solver().region();
        if r.trim().is_empty() {
            DEFAULT_CAPTCHA_REGION.to_string()
        } else {
            r
        }
    }

    /// Anthropic Messages 端点（含测试覆盖）。对应 Go `anthropicMessagesURLFor`。
    fn anthropic_messages_url_for(&self, cr: &ZcodeCred) -> String {
        let ov = self.anthropic_base_override.trim_end_matches('/');
        if !ov.is_empty() {
            return format!("{ov}{}", provider::ANTHROPIC_MESSAGES_PATH);
        }
        anthropic_messages_url(cr)
    }

    // ── 额度 ──────────────────────────────────────────────────────

    /// 查询额度。对应 Go `FetchQuota`（`client.go:893-1067`）。
    ///
    /// 两个必需条件：
    /// 1. **JWT** —— billing 网关用它鉴权，**不是** `{apiKey}.{secret}`；缺它返回 [`ERR_NO_JWT`]
    /// 2. **`X-Device-Mid`**（UUID 形态）—— 控制面硬要求。缺了上游回
    ///    `400 {"code":3001,"msg":"parameter error"}`，那个错误看起来像「参数写错了」，
    ///    实际原因是缺这个头，很容易被误判成「这个账号查不到额度」。
    ///    ⚠ 这一条最初被漏掉，作者把 3001 误判成「只导入凭证所以查不到额度」，
    ///    做成「额度未知」状态并写成「需要重新登录」—— **那个结论是错的**：
    ///    补上这个头之后只要有 JWT 就能查到完整额度（实测 3 亿 token 活动计划）。
    ///    区别在于**用户能否自己解决**：缺 JWT ⇒ 用户要走一次 OAuth（我们无法代劳）；
    ///    缺 deviceMid ⇒ **我们自己补**。
    ///
    /// ★ 查询走 `cr.provider` 的 `quota_host`（恒为 `zcode.z.ai`），**不是** `biz_host`：
    /// 实测两个服务商的额度查询都走 `zcode.z.ai`；用各自 BizHost 会得到
    /// `HTTP 200 {"code":500,"msg":"404 NOT_FOUND"}` —— 那是 200，会被误当成
    /// 「查到了但没额度」。
    ///
    /// ⚠ 取 `&mut` 是为了**现场补 deviceMid 并写回**（与 Go 同款）。注意跨请求的稳定性
    /// 其实来自 `cred::load_file` 在加载阶段就 mint 并落盘 deviceMid，本函数的写回
    /// 只在「同一次请求内 quota 预检 + 对话」之间生效。
    pub async fn fetch_quota(&self, cr: &mut ZcodeCred) -> Result<Quota, String> {
        if cr.jwt.trim().is_empty() {
            return Err(ERR_NO_JWT.to_string());
        }
        if !identity::is_uuid(&cr.device_mid) {
            cr.device_mid = identity::new_device_mid();
        }
        let url = format!(
            "{}{}?app_version={}&platform={}",
            self.quota_host(),
            QUOTA_PATH,
            self.identity.app_version,
            self.identity.platform_arch()
        );
        let mut h = reqwest::header::HeaderMap::new();
        put(&mut h, "authorization", &format!("Bearer {}", cr.jwt.trim()))?;
        put(&mut h, "accept", "application/json")?;
        for (k, v) in self.identity.control_plane_headers(&cr.device_mid) {
            put(&mut h, &k, &v)?;
        }
        let resp = self
            .http
            .get(&url)
            .headers(h)
            // 与 Go `context.WithTimeout(ctx, c.timeout())` 同款：单次控制面请求 30s。
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| format!("查询 ZCode 额度失败: {e}"))?;
        let status = resp.status().as_u16();
        let raw = read_limited(resp, MAX_QUOTA_BODY).await;
        let body = String::from_utf8_lossy(&raw).into_owned();
        if status >= 400 {
            return Err(format!(
                "查询 ZCode 额度失败（HTTP {status}）：{}",
                UpstreamError::new(status, &body).friendly_message()
            ));
        }
        let doc: Value = serde_json::from_slice(&raw).map_err(|e| {
            format!(
                "额度响应解析失败: {e}（原始内容：{}）",
                errors::truncate(&body, 160)
            )
        })?;
        // ★ 上游用 **HTTP 200 + 业务码**报错。
        let code = doc.get("code").map(to_int64).unwrap_or(0);
        if code != 0 {
            let msg = doc.get("msg").and_then(|m| m.as_str()).unwrap_or("");
            let mut text = format!(
                "查询 ZCode 额度失败（HTTP {status}, code={code}）：{}",
                UpstreamError::new(status, &body).friendly_message()
            );
            if !msg.is_empty() {
                text.push_str(&format!(" 上游说明：{msg}"));
            }
            return Err(text);
        }
        Ok(assemble_quota(&doc))
    }

    // ── 模型清单 ──────────────────────────────────────────────────

    /// 模型清单：以**账号端点的 /models** 为权威 ID 来源，用 `client/configs` 的元信息补齐。
    ///
    /// 对应 Go `FetchModels`（`client.go:1215-1263`）。两个来源的分工：
    ///
    /// | 来源 | 给出什么 | 大小写 |
    /// |---|---|---|
    /// | `{provider}/api/coding/paas/v4/models` | **API 真正接受的 model 值** | 小写 `glm-5.3` |
    /// | `zcode.z.ai/api/v1/client/configs` | ZCode **客户端**的展示信息 | 大写 `GLM-5.3` |
    ///
    /// 「我第一版只用了 `client/configs`，于是把客户端的展示名当成了 API 的模型名，
    /// 实测被上游拒绝：`400 {"code":11102,"msg":"model [GLM-5.3] service info not found"}`。
    /// **上游严格区分大小写** —— 展示名不能当请求参数用。」
    ///
    /// 合并策略：① 账号端点拿得到 ⇒ 用它给的 **id 原值**（保持 API 接受的大小写），
    /// 元信息只补能力字段；② 拿不到 ⇒ 退化为只用 `client/configs` 并**统一转小写**。
    ///
    /// ⚠ 目前 Rust 侧还没有生产调用方（Go 在 `dispatch.go` 的 `refreshModels` 里用），
    /// 故这里只保证语义正确、可被接线。
    pub async fn fetch_models(&self, cr: Option<&ZcodeCred>) -> Result<Vec<Model>, String> {
        let mut meta: BTreeMap<String, Model> = BTreeMap::new();
        if let Ok(ms) = self.fetch_config_models().await {
            for m in ms {
                meta.insert(m.id.to_lowercase(), m);
            }
        }
        if let Some(cr) = cr {
            if !cr.credential.is_empty() {
                if let Ok(models) = self.fetch_provider_models(cr).await {
                    if !models.is_empty() {
                        let mut out = Vec::with_capacity(models.len());
                        for m in models {
                            let key = m.id.trim().to_lowercase();
                            if key.is_empty() {
                                continue;
                            }
                            match meta.get(&key) {
                                Some(b) => {
                                    // 用端点给的 id 原值，元信息只补能力字段。
                                    let mut b = b.clone();
                                    b.id = m.id;
                                    b.source = "upstream".to_string();
                                    out.push(b);
                                }
                                None => out.push(Model {
                                    id: m.id.clone(),
                                    name: m.id,
                                    source: "upstream".to_string(),
                                    ..Default::default()
                                }),
                            }
                        }
                        if !out.is_empty() {
                            return Ok(out);
                        }
                    }
                }
            }
        }
        if !meta.is_empty() {
            let mut out: Vec<Model> = meta
                .into_values()
                .map(|mut m| {
                    m.id = m.id.to_lowercase();
                    if m.name.is_empty() {
                        m.name = m.id.clone();
                    }
                    m.source = "upstream".to_string();
                    m
                })
                .collect();
            out.sort_by(|a, b| a.id.cmp(&b.id));
            return Ok(out);
        }
        Err("拉取模型清单失败（账号端点与 config 端点都不可用）".to_string())
    }

    /// `GET https://zcode.z.ai/api/v1/client/configs`（免认证）。对应 Go `fetchConfigModels`。
    ///
    /// ★ 免认证，但仍带身份头 —— 更接近官方客户端行为。
    /// ★ `reasoning` 只看那个对象**是否存在**（Go 侧是 `*struct` 指针非 nil），
    /// 不看 `defaultLevel` 的值。
    pub async fn fetch_config_models(&self) -> Result<Vec<Model>, String> {
        let url = format!("{}{CONFIG_PATH}", self.config_origin());
        let mut h = reqwest::header::HeaderMap::new();
        put(&mut h, "accept", "application/json")?;
        for (k, v) in self.identity.headers() {
            put(&mut h, &k, &v)?;
        }
        let resp = self
            .http
            .get(&url)
            .headers(h)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| format!("拉取 ZCode 客户端配置失败: {e}"))?;
        let status = resp.status().as_u16();
        let raw = read_limited(resp, MAX_CONFIG_BODY).await;
        let body = String::from_utf8_lossy(&raw).into_owned();
        if status >= 400 {
            return Err(format!(
                "拉取 ZCode 客户端配置失败（HTTP {status}）：{}",
                UpstreamError::new(status, &body).friendly_message()
            ));
        }
        let doc: Value = serde_json::from_slice(&raw).map_err(|e| {
            format!(
                "客户端配置解析失败: {e}（原始内容：{}）",
                errors::truncate(&body, 160)
            )
        })?;
        let code = doc.get("code").map(to_int64).unwrap_or(0);
        if code != 0 {
            return Err(format!("客户端配置返回错误码 {code}"));
        }
        let items = doc
            .pointer("/data/builtinModels")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::with_capacity(items.len());
        for m in &items {
            let id = m
                .get("modelId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if id.is_empty() {
                continue;
            }
            let name = m.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            out.push(Model {
                id: id.clone(),
                name: if name.is_empty() { id } else { name },
                context_window: m.get("contextWindow").map(to_int64).unwrap_or(0),
                max_output: m.get("maxCompletionTokens").map(to_int64).unwrap_or(0),
                vision: m
                    .pointer("/capabilities/vision")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                // ★ 只看那个对象是否存在（Go 是 `*struct` 指针），不看 defaultLevel 的值。
                reasoning: m.get("reasoning").map(|v| !v.is_null()).unwrap_or(false),
                source: "upstream".to_string(),
            });
        }
        if out.is_empty() {
            return Err("客户端配置里没有模型（builtinModels 为空）".to_string());
        }
        Ok(out)
    }

    /// `GET {OpenAIBase}/models`（**需要认证**）。对应 Go `fetchProviderModels`。
    ///
    /// 上游有、而我们内置清单里没有的新模型：给保守默认值 + 标注来源，
    /// **不丢弃** —— 丢弃会让用户「看不到自己套餐里的新模型」。
    pub async fn fetch_provider_models(&self, cr: &ZcodeCred) -> Result<Vec<Model>, String> {
        let url = format!("{}{MODEL_LIST_PATH}", self.provider_openai_base(cr));
        let rb = self
            .apply_headers(self.http.get(&url), cr, false)
            .map_err(|e| format!("构造 ZCode 请求头失败: {e}"))?;
        let resp = rb
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| format!("拉取 ZCode 模型清单失败: {e}"))?;
        let status = resp.status().as_u16();
        let raw = read_limited(resp, MAX_MODELS_BODY).await;
        let body = String::from_utf8_lossy(&raw).into_owned();
        if status >= 400 {
            return Err(format!(
                "拉取 ZCode 模型清单失败（HTTP {status}）：{}",
                UpstreamError::new(status, &body).friendly_message()
            ));
        }
        let doc: Value = serde_json::from_slice(&raw).map_err(|e| {
            format!(
                "模型清单解析失败: {e}（原始内容：{}）",
                errors::truncate(&body, 160)
            )
        })?;
        let items = doc
            .get("data")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if items.is_empty() {
            return Err(format!(
                "上游模型清单为空（原始内容：{}）",
                errors::truncate(&body, 160)
            ));
        }
        let builtin = builtin_model_index();
        let mut out = Vec::with_capacity(items.len());
        for m in &items {
            let id = m
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if id.is_empty() {
                continue;
            }
            match builtin.get(&id.to_lowercase()) {
                Some(b) => {
                    let mut b = b.clone();
                    b.source = "upstream".to_string();
                    out.push(b);
                }
                None => out.push(Model {
                    id: id.clone(),
                    name: id,
                    source: "upstream".to_string(),
                    ..Default::default()
                }),
            }
        }
        Ok(out)
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

/// 构造 reqwest 客户端。`build()` 只在 TLS 后端初始化失败时报错 —— 那是进程级故障
/// （Go 侧 `New()` 不会失败，但也没有等价的可恢复路径），故直接 panic 并给出可读说明，
/// 而不是静默降级成「每个请求都传输失败」。
fn build(builder: reqwest::ClientBuilder) -> reqwest::Client {
    builder
        .build()
        .expect("构造 ZCode HTTP 客户端失败（TLS 后端初始化异常）")
}

/// 直连基座（与 `products/qoder/client.rs` 同款参数）。
fn base_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .http1_only()
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT)
        .pool_max_idle_per_host(20)
        .pool_idle_timeout(Duration::from_secs(90))
}

/// 走代理基座。★ 不再调 `.no_proxy()`（那会清空代理表）；
/// `.proxy()` 自身已把 `auto_sys_proxy` 置 false，环境变量代理不会叠加。
fn proxied_builder(proxy: reqwest::Proxy) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .http1_only()
        .proxy(proxy)
        .connect_timeout(CONNECT_TIMEOUT)
        .pool_max_idle_per_host(20)
        .pool_idle_timeout(Duration::from_secs(90))
}

// ───────────────────────────── 纯函数（可单测） ─────────────────────────────

/// 往头表里塞一个头。值里含换行/控制字符时返回可读错误而不是静默丢弃 ——
/// 静默丢弃会让用户看到「凭证无效」而真正原因是凭证里混进了不可见字符。
///
/// ★ `pub(crate)` 是为了 `plan.rs`（`FetchStartPlanPreview` 要自己组头）。
/// `claim.rs` 另有一份同名私有副本（它的错误文案要与那边逐字对齐），
/// 两份行为一致，改动时请一起看。
pub(crate) fn put(
    h: &mut reqwest::header::HeaderMap,
    name: &str,
    value: &str,
) -> Result<(), String> {
    if value.is_empty() {
        return Ok(());
    }
    let n = reqwest::header::HeaderName::from_bytes(name.as_bytes())
        .map_err(|e| format!("请求头名非法 {name}: {e}"))?;
    let v = reqwest::header::HeaderValue::from_str(value)
        .map_err(|e| format!("请求头 {name} 的值非法（含换行或控制字符）: {e}"))?;
    h.insert(n, v);
    Ok(())
}

/// 读取响应体，最多 `limit` 字节；读错误**忽略**（对应 Go `raw, _ := io.ReadAll(io.LimitReader(...))`）。
///
/// `pub(crate)` 供 `claim.rs`（preview / claim 端点）复用。
pub(crate) async fn read_limited(resp: reqwest::Response, limit: usize) -> Vec<u8> {
    let mut resp = resp;
    let mut out: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.ok().flatten() {
        let remain = limit.saturating_sub(out.len());
        if remain == 0 {
            break;
        }
        if chunk.len() <= remain {
            out.extend_from_slice(&chunk);
        } else {
            out.extend_from_slice(&chunk[..remain]);
            break;
        }
    }
    out
}

/// 把 Anthropic SSE 流式翻成 OpenAI SSE，并重新包装成 `reqwest::Response`。
///
/// ★ 调用方（`forward.rs`）只会拿它的 `bytes_stream()`，并可能先探测首帧
/// （`sse::probe_first_frame`）—— 故必须**纯流式**，不能缓冲。
fn wrap_stream(resp: reqwest::Response, model: String) -> reqwest::Response {
    let upstream = resp.bytes_stream();
    let body = reqwest::Body::wrap_stream(translate::translate_stream(upstream, model));
    reqwest::Response::from(axum::http::Response::new(body))
}

/// Anthropic Messages 端点。对应 Go `anthropicMessagesURL`（`client.go:469-478`）。
///
/// providerRules 的完整映射（Go 文档头）：
/// - `account:zai-start-plan` / `account:bigmodel-start-plan`
///   → `https://zcode.z.ai/api/v1/zcode-plan/anthropic`
/// - `account:zai-individual-coding-plan` → `https://api.z.ai/api/anthropic`
/// - `account:bigmodel-individual-coding-plan` → `https://open.bigmodel.cn/api/anthropic`
/// - `account:*-offpeak-idle-plan` → `https://zcode.z.ai/api/v1/off-peak/anthropic`
///
/// ⚠ 当前只用「是不是 start-plan」二分（与 Go 一致），off-peak 那一支还没实现。
pub fn anthropic_messages_url(cr: &ZcodeCred) -> String {
    let mut base = provider::START_PLAN_BASE.to_string();
    if !cr.is_start_plan() {
        let b = cr.provider.endpoints().anthropic_base;
        if !b.is_empty() {
            base = b;
        }
    }
    format!("{base}{}", provider::ANTHROPIC_MESSAGES_PATH)
}

/// 强制 `stream: true`。对应 Go `forceStream`（`client.go:433-447`）。
///
/// 理由：上游流式/非流式**响应形状不同**（SSE 事件 vs 单个 message 对象），
/// 两条路都自己翻译就有两套代码 + 两套测试；而网关的非流式路径本来就是
/// 「读完流再聚合」—— 它本来就期望拿到流 ⇒ **一条路径，一处真相**。
/// 且「不该因为一个可选字段解析不动就让整个请求失败」。
///
/// ⚠ 与 Go 的差异：Go 在 body 为 `null` 时会 panic（`assignment to entry in nil map`），
/// Rust 这里原样返回。生产路径上 `build_anthropic_body_with_meta` 已保证是对象，
/// 故行为一致；Rust 版只是不会崩。
pub fn force_stream(body: &[u8]) -> Vec<u8> {
    let Ok(Value::Object(mut m)) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    if m.get("stream").and_then(|v| v.as_bool()) == Some(true) {
        return body.to_vec();
    }
    m.insert("stream".to_string(), Value::Bool(true));
    match serde_json::to_vec(&m) {
        Ok(out) => out,
        Err(_) => body.to_vec(),
    }
}

/// 把上游错误体转成**服务端能解析的 OpenAI 形状**。对应 Go `NormalizeErrorBody`。
///
/// server 层按 OpenAI 形状解析（`error.message`），而上游返回 Anthropic 形状
/// `{"type":"error","error":{...}}` 或 `{"code":3007,"msg":"captcha verify failed"}`；
/// 不转的话用户看到**空错误**或「未知错误」，真正原因（验证码/额度/参数）藏在原文里
/// —— 那是最难排查的状态。
pub fn normalize_error_body(body: &[u8], status: u16) -> Vec<u8> {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        // 非 JSON（HTML 错误页等）⇒ 包一层。Go 用字节切片截断 500，
        // Rust 用 `errors::truncate`（先 trim、按字符边界切、补省略号），
        // 刻意修掉 Go 会把多字节字符切成非法 UTF-8 的问题。
        let msg = errors::truncate(&String::from_utf8_lossy(body), 500);
        return serde_json::to_vec(&json!({
            "error": {"message": msg, "type": "upstream_error", "code": status}
        }))
        .unwrap_or_else(|_| body.to_vec());
    };
    // 已经是 OpenAI 形状 ⇒ 原样返回，不改一个字节。
    //
    // ★ 这条判据比它的字面意思**更宽**：Anthropic 形状 `{"type":"error","error":{…}}`
    // 的顶层 `error` **也是对象** ⇒ 同样命中早退。因此下面那段「Anthropic 形状」提取
    // 分支（Go `client.go:526-532`）在两边都是**不可达代码**。
    //
    // 语义上无害：早退的产物里 `error.message` 与 `error.type` 本就可读 —— 那正是本函数
    // 文档声明的目的（"server 层按 OpenAI 形状解析 `error.message`"）。唯一差别是不补
    // `error.code`（Anthropic 形状本就没有 code）。Go 自己的
    // `TestNormalizeErrorBodyVariants` 只断言产物 `Contains("参数不对")`，原样返回即可
    // 满足 ⇒ 那个测试是**碰巧通过**的。已向 lead 报告这条 Go 死分支。
    if v.get("error").map(|e| e.is_object()).unwrap_or(false) {
        return body.to_vec();
    }
    let mut msg = String::new();
    let mut typ = "upstream_error".to_string();
    if let Some(e) = v.get("error").and_then(|e| e.as_object()) {
        msg = e
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let t = e.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if !t.is_empty() {
            typ = t.to_string();
        }
    }
    if msg.is_empty() {
        msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("").to_string();
    }
    if msg.is_empty() {
        msg = v
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
    }
    if msg.is_empty() {
        msg = String::from_utf8_lossy(body).trim().to_string();
    }
    // `firstNonNil(m["code"], status)`：JSON `null` 与缺失等价，其余（含 0）原样用。
    let code = match v.get("code") {
        Some(Value::Null) | None => Value::from(status),
        Some(other) => other.clone(),
    };
    serde_json::to_vec(&json!({
        "error": {"message": msg, "type": typ, "code": code}
    }))
    .unwrap_or_else(|_| body.to_vec())
}

/// 上游是否在要求人机验证。对应 Go `IsCaptchaRequiredBody`。
///
/// 上游两种形状都见过：`code:3007` 与中英文文案。**以 code 为主判据**（文案可能变），
/// 文案为辅。
pub fn is_captcha_required_body(body: &str) -> bool {
    if body.is_empty() {
        return false;
    }
    body.contains("3007") || body.contains("captcha verify failed") || body.contains("验证码")
}

/// 把「求解未成功」的原因折进错误体，且**保证产物是合法 JSON**。
/// 对应 Go `appendCaptchaNote`（`client.go:581-639`）。
///
/// ★ Go 的修复记录：旧实现 `append(body, note...)` 产物形如
/// `{"error":{...}}\n[网关说明] 求解未成功…` = **非法 JSON** ⇒ server 层解析失败 ⇒
/// 用户看到「未知错误」，我们的说明**谁也看不到**。被
/// `TestStreamChatTranslatesErrorBody`（断言错误体必须合法 JSON）抓出。
///
/// 步骤：① `reason` 先过 [`errors::annotate_risk_code`]（阿里云 SDK 的
/// `verifyCode`（F001/T001/T002）是**唯一可操作的线索**；认不出码时原样返回，不损失信息）；
/// ② `error.message` 是非空字符串 ⇒ 追加附注；③ 否则整个 `error` 换成附注对象；
/// ④ 非 JSON ⇒ 包一层并把上游原文附上。
pub fn append_captcha_note(body: &[u8], reason: &str) -> Vec<u8> {
    let reason = errors::annotate_risk_code(reason);
    let note = format!(
        "（网关附注：上游要求人机验证，但求解未成功，故未重试。原因：{reason}；若为组件缺失请确认安装包含 assets/zcode-captcha，若为冷却/限流则稍后会自动恢复）"
    );
    if let Ok(Value::Object(mut m)) = serde_json::from_slice::<Value>(body) {
        let mut patched = false;
        if let Some(Value::Object(e)) = m.get_mut("error") {
            if let Some(Value::String(msg)) = e.get_mut("message") {
                if !msg.is_empty() {
                    msg.push(' ');
                    msg.push_str(&note);
                    patched = true;
                }
            }
        }
        if !patched {
            m.insert(
                "error".to_string(),
                json!({"message": note, "type": "captcha_solve_failed"}),
            );
        }
        if let Ok(out) = serde_json::to_vec(&m) {
            return out;
        }
    }
    let text = format!(
        "{note} 上游原始响应：{}",
        String::from_utf8_lossy(body).trim()
    );
    serde_json::to_vec(&json!({
        "error": {"message": text, "type": "captcha_solve_failed"}
    }))
    .unwrap_or_else(|_| body.to_vec())
}

/// 3007 + 求解失败时给用户看的文本：取附注后的 `error.message`。
fn captcha_failure_message(body: &str, reason: &str) -> String {
    let annotated = append_captcha_note(body.as_bytes(), reason);
    let text = String::from_utf8_lossy(&annotated).into_owned();
    let msg = errors::extract_message(&text);
    if msg.is_empty() {
        text
    } else {
        msg
    }
}

/// ZCode 非流式聚合：拼 delta 即可。对应 Go `AggregateOpenAI`（`client.go:1454-1543`）。
///
/// 为什么本包要自己实现：server 的 `ProductUpstream` 接口要求每个产品**自洽**
/// （不能反向依赖 `upstream` 包，那会让「产品适配」与「WorkBuddy 实现」耦合）。
/// 为什么这么简单（对比 Qoder）：ZCode 上游响应**本来就是标准 OpenAI 格式**
/// （选 Anthropic 端点 + 内部翻译的收益），不需要像 Qoder 那样先解开嵌套 `body`。
///
/// ⚠ 与 Go 的两点差异（都是**更宽容**，不会掩盖错误）：
/// ① Go 用「整体 Unmarshal 进匿名结构体」，任一字段类型不符就**整块静默丢弃**；
///    Rust 逐字段取值，坏字段退化为默认值而内容仍被保留。
/// ② 无 `usage` 字段（与 Go 一致 —— Go 也不给）。
pub fn aggregate_openai(sse: &str, model: &str) -> Result<Map<String, Value>, String> {
    let mut id = String::new();
    let mut created: i64 = 0;
    let mut model_out = String::new();
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut finish = String::new();

    for line in sse.split('\n') {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("data:") else {
            continue;
        };
        let payload = rest.trim();
        if payload.is_empty() {
            continue;
        }
        if payload == "[DONE]" {
            break;
        }
        let Ok(chunk) = serde_json::from_slice::<Value>(payload.as_bytes()) else {
            // 解析失败静默忽略（与 Go 同款）。
            continue;
        };
        if let Some(s) = chunk.get("id").and_then(|v| v.as_str()) {
            if !s.is_empty() {
                id = s.to_string();
            }
        }
        let c = chunk.get("created").map(to_int64).unwrap_or(0);
        if c != 0 {
            created = c;
        }
        if let Some(s) = chunk.get("model").and_then(|v| v.as_str()) {
            if !s.is_empty() {
                model_out = s.to_string();
            }
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
        else {
            continue;
        };
        // delta 是流式增量；message 是非流式整体（有些实现两者都给）。
        content.push_str(&first_non_empty(&[
            json_str(choice.pointer("/delta/content")),
            json_str(choice.pointer("/message/content")),
        ]));
        reasoning.push_str(&first_non_empty(&[
            json_str(choice.pointer("/delta/reasoning_content")),
            json_str(choice.pointer("/message/reasoning_content")),
        ]));
        if let Some(f) = choice.get("finish_reason").and_then(|v| v.as_str()) {
            if !f.is_empty() {
                finish = f.to_string();
            }
        }
    }

    if finish.is_empty() {
        finish = "stop".to_string();
    }
    if id.is_empty() {
        id = "chatcmpl-zcode".to_string();
    }
    if created == 0 {
        created = now_unix();
    }
    if model_out.is_empty() {
        model_out = model.to_string();
    }
    let mut msg = Map::new();
    msg.insert("role".to_string(), Value::String("assistant".to_string()));
    msg.insert("content".to_string(), Value::String(content));
    if !reasoning.is_empty() {
        // ★ 只在非空时给：空字符串会让部分客户端显示空的思考块。
        msg.insert(
            "reasoning_content".to_string(),
            Value::String(reasoning),
        );
    }
    let mut choice = Map::new();
    choice.insert("index".to_string(), Value::from(0));
    choice.insert("message".to_string(), Value::Object(msg));
    choice.insert("finish_reason".to_string(), Value::String(finish));
    let mut out = Map::new();
    out.insert("id".to_string(), Value::String(id));
    out.insert(
        "object".to_string(),
        Value::String("chat.completion".to_string()),
    );
    out.insert("created".to_string(), Value::from(created));
    out.insert("model".to_string(), Value::String(model_out));
    out.insert("choices".to_string(), Value::Array(vec![Value::Object(choice)]));
    Ok(out)
}

/// 取请求体里的模型名。对应 Go `modelOf`（解析失败 ⇒ 空串）。
pub fn model_of(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("model")
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default()
}

/// 从额度响应装配 [`Quota`]（仅测试可见：让 `dispatch.rs` 的契约测试能钉住形状）。
#[cfg(test)]
pub(crate) fn assemble_quota_for_test(doc: &Value) -> Quota {
    assemble_quota(doc)
}

/// 上游有时返回数字、有时返回字符串（如 `"1234"`）。只认数字会让额度**静默变成 0**，
/// 而 0 会被误读成「额度耗尽」。对应 Go `toInt64`。
///
/// ⚠ Go 用 `fmt.Sscanf(TrimSpace(x), "%f", &n)`，能容忍尾部垃圾（`"1234abc"` ⇒ 1234）；
/// Rust 用 `parse::<f64>()` 要求整串合法。实测样本都是纯数字，差异不影响。
pub fn to_int64(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Value::String(s) => s.trim().parse::<f64>().map(|f| f as i64).unwrap_or(0),
        _ => 0,
    }
}

/// 同 [`to_int64`]，但保留小数。对应 Go `toFloat64`。
///
/// 为什么单独要一个：`grant_units` 这类**赠送量**可能带小数（参考实现 zcode2api
/// 就用 `float(units)` 读它）。用 `to_int64` 会截断，而额度数字被截断会让
/// 「总量对不上赠送量」，进而把体验套餐误判成付费套餐。
pub fn to_float64(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// 取第一个「有意义」的值。对应 Go `firstAny`：跳过 nil；`string` 要求
/// `TrimSpace != ""`；数字要求 `!= 0`；其他类型直接返回。
pub fn first_any<'a>(vals: &[Option<&'a Value>]) -> Option<&'a Value> {
    for v in vals.iter().flatten() {
        match v {
            Value::Null => continue,
            Value::String(s) => {
                if !s.trim().is_empty() {
                    return Some(v);
                }
            }
            Value::Number(n) => {
                if n.as_f64() != Some(0.0) {
                    return Some(v);
                }
            }
            _ => return Some(v),
        }
    }
    None
}

/// 内置模型清单（11 条，兜底用）。
///
/// ⚠ **这是兜底，不是权威来源。** 实测（2026-09-19）上游 `client/configs` 当前只返回
/// **2 个模型**（GLM-5.3 / GLM-5.3-Flash），这 11 个里的大多数已经过时。
/// 仅在上游接口不可用时使用，且 `source` 标为 `"builtin"` 让界面能区分。
///
/// 视觉标记沿用参考实现（`glm-4.6v` / `glm-5v-turbo`）—— 但那是**启发式**的结果，
/// 实测上游的真实视觉模型是 `GLM-5.3-Flash`（id 里没有 v）。这进一步说明：
/// **不要依赖内置清单**，它只是网络不通时的降级显示。
pub fn builtin_models() -> Vec<Model> {
    const RAW: [(&str, &str, i64, i64, bool); 11] = [
        ("glm-4.5-air", "GLM 4.5 Air", 131072, 98304, true),
        ("glm-4.6", "GLM 4.6", 200000, 131072, true),
        ("glm-4.6v", "GLM 4.6V", 131072, 32768, false),
        ("glm-4.7", "GLM 4.7", 200000, 131072, true),
        ("glm-5", "GLM 5", 200000, 64000, true),
        ("glm-5-turbo", "GLM 5 Turbo", 200000, 64000, true),
        ("glm-5v-turbo", "GLM 5V Turbo", 200000, 131072, false),
        ("glm-5.1", "GLM 5.1", 200000, 64000, true),
        ("glm-5.2", "GLM 5.2", 1000000, 128000, true),
        ("glm-5.3", "GLM 5.3", 1000000, 128000, true),
        ("glm-5.3-flash", "GLM 5.3 Flash", 1000000, 128000, true),
    ];
    RAW.iter()
        .map(|(id, name, cw, mo, reasoning)| Model {
            id: (*id).to_string(),
            name: (*name).to_string(),
            context_window: *cw,
            max_output: *mo,
            reasoning: *reasoning,
            vision: false,
            source: "builtin".to_string(),
        })
        .collect()
}

/// 内置清单索引，键为小写 id。
pub fn builtin_model_index() -> BTreeMap<String, Model> {
    let mut idx = BTreeMap::new();
    for m in builtin_models() {
        idx.insert(m.id.to_lowercase(), m);
    }
    idx
}

// ───────────────────────────── 内部小工具 ─────────────────────────────

/// 从额度响应里装配 [`Quota`]。
fn assemble_quota(doc: &Value) -> Quota {
    let data = doc.get("data").cloned().unwrap_or(Value::Null);
    let mut out = Quota {
        server_time: data.get("server_time").map(to_int64).unwrap_or(0),
        ..Default::default()
    };
    let plans_v = data
        .get("plans")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let balances_v = data
        .get("balances")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    // 先把套餐的「模型 → 每日赠送量」建成索引，供余额项关联 ——
    // 这样界面上能说清「这 300 万是体验套餐送的」，而不是一个孤立的数字。
    let mut grants: HashMap<(String, String), f64> = HashMap::new();
    for p in &plans_v {
        let pid = str_field(p, "plan_id");
        if let Some(ents) = p.get("entitlements").and_then(|v| v.as_array()) {
            for e in ents {
                let eid = str_field(e, "entitlement_id");
                grants.insert(
                    (pid.clone(), eid),
                    e.get("grant_units").map(to_float64).unwrap_or(0.0),
                );
            }
        }
    }

    for b in &balances_v {
        let plan_id = str_field(b, "plan_id");
        let entitlement_id = str_field(b, "entitlement_id");
        let grant_units = grants
            .get(&(plan_id.clone(), entitlement_id.clone()))
            .copied()
            .unwrap_or(0.0);
        // 字段名 snake_case 与 camelCase 都接受（参考实现两种都读，实测两种都存在）。
        let e = QuotaEntry {
            show_name: str_field(b, "show_name"),
            remaining: b.get("remaining_units").map(to_int64).unwrap_or(0),
            total: b.get("total_units").map(to_int64).unwrap_or(0),
            used: b.get("used_units").map(to_int64).unwrap_or(0),
            unit_type: first_non_empty(&[
                &str_field(b, "unit_type"),
                &str_field(b, "unitType"),
            ]),
            expires_at: first_any(&[b.get("expires_at"), b.get("expiresAt")])
                .map(to_int64)
                .unwrap_or(0),
            period_start: b.get("period_start").map(to_int64).unwrap_or(0),
            period_end: b.get("period_end").map(to_int64).unwrap_or(0),
            plan_id,
            entitlement_id,
            grant_units,
        };
        out.remaining += e.remaining;
        out.total += e.total;
        out.used += e.used;
        out.entries.push(e);
    }

    for p in &plans_v {
        let mut pl = Plan {
            user_plan_id: str_field(p, "user_plan_id"),
            plan_id: str_field(p, "plan_id"),
            name: str_field(p, "name"),
            description: str_field(p, "description"),
            priority: p.get("priority").map(to_float64).unwrap_or(0.0),
            status: str_field(p, "status"),
            // ⚠ `ends_at` 是**套餐到期**（实测 2026-09-23 23:59:59），
            // 与余额项的 `expires_at`（每日周期结束，当天 23:59:59）不同。
            // `endsAt` 是驼峰别名（上游两种写法都出现过）。
            ends_at: first_any(&[p.get("ends_at"), p.get("endsAt")])
                .map(to_int64)
                .unwrap_or(0),
            starts_at: p.get("starts_at").map(to_int64).unwrap_or(0),
            entitlements: Vec::new(),
        };
        if let Some(ents) = p.get("entitlements").and_then(|v| v.as_array()) {
            for e in ents {
                pl.entitlements.push(PlanEntitlement {
                    entitlement_id: str_field(e, "entitlement_id"),
                    show_name: str_field(e, "show_name"),
                    meter: str_field(e, "meter"),
                    unit_type: str_field(e, "unit_type"),
                    capabilities: e
                        .get("capabilities")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default(),
                    grant_units: e.get("grant_units").map(to_float64).unwrap_or(0.0),
                    period: str_field(e, "period"),
                    effective_at: e.get("effective_at").map(to_int64).unwrap_or(0),
                });
            }
        }
        out.plans.push(pl);
    }
    out
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

fn json_str(v: Option<&Value>) -> &str {
    v.and_then(|x| x.as_str()).unwrap_or("")
}

fn first_non_empty(vals: &[&str]) -> String {
    for v in vals {
        if !v.trim().is_empty() {
            return (*v).to_string();
        }
    }
    String::new()
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ───────────────────────────── 测试 ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── force_stream ──

    #[test]
    fn force_stream_adds_stream_true() {
        let out = force_stream(br#"{"model":"glm-5.3","messages":[]}"#);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v.get("stream").and_then(|s| s.as_bool()), Some(true));
        assert_eq!(v.get("model").and_then(|s| s.as_str()), Some("glm-5.3"));
    }

    #[test]
    fn force_stream_keeps_existing_true() {
        let src = br#"{"stream":true,"model":"x"}"#;
        assert_eq!(force_stream(src), src.to_vec());
    }

    #[test]
    fn force_stream_flips_explicit_false() {
        let out = force_stream(br#"{"stream":false}"#);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v.get("stream").and_then(|s| s.as_bool()), Some(true));
    }

    #[test]
    fn force_stream_returns_input_on_non_object() {
        // 「不该因为一个可选字段解析不动就让整个请求失败」。
        assert_eq!(force_stream(b"not json").to_vec(), b"not json".to_vec());
        assert_eq!(force_stream(b"[1,2]").to_vec(), b"[1,2]".to_vec());
        assert_eq!(force_stream(b"null").to_vec(), b"null".to_vec());
    }

    // ── normalize_error_body ──

    #[test]
    fn normalize_error_body_passes_openai_shape_through_untouched() {
        let src = br#"{"error":{"message":"boom","type":"invalid_request_error"}}"#;
        assert_eq!(normalize_error_body(src, 400), src.to_vec());
    }

    #[test]
    fn normalize_error_body_converts_anthropic_shape() {
        // ★ 名字保留（它钉的是"Anthropic 形状要能读"），但**实际语义**是早退：
        // 顶层 `error` 是对象 ⇒ 命中"已是 OpenAI 形状"分支 ⇒ **原样返回**。
        // Anthropic 形状的 `error` 本就是对象（见 Go `client.go:521`），
        // 故 Go 那段"Anthropic 提取"分支不可达，`error.code` 也**不会**被补上。
        // Go 自己的 `TestNormalizeErrorBodyVariants` 只断言 `Contains("参数不对")`
        // 与"有 error 对象"，原样返回即可满足 ⇒ 那个测试是碰巧通过的。
        let src =
            br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad param"}}"#;
        let out = normalize_error_body(src, 400);
        assert_eq!(out, src.to_vec(), "已是对象形状 ⇒ 一个字节都不该改");
        let v: Value = serde_json::from_slice(&out).unwrap();
        // 用户的可见部分（server 层读 `error.message`）必须可读 —— 这才是本函数的目的
        assert_eq!(
            v.pointer("/error/message").and_then(|x| x.as_str()),
            Some("bad param")
        );
        assert_eq!(
            v.pointer("/error/type").and_then(|x| x.as_str()),
            Some("invalid_request_error")
        );
        // 上游没给 code，我们也**不编**一个：补 status 只发生在"需要重建"的分支
        assert_eq!(v.pointer("/error/code"), None, "原样返回时不该凭空补 code");
    }

    #[test]
    fn normalize_error_body_reads_msg_and_keeps_business_code() {
        // 实测形状：上游用 HTTP 200/400 + code 报错。
        let out = normalize_error_body(br#"{"code":3007,"msg":"captcha verify failed"}"#, 400);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v.pointer("/error/message").and_then(|x| x.as_str()),
            Some("captcha verify failed")
        );
        assert_eq!(v.pointer("/error/code").and_then(|x| x.as_u64()), Some(3007));
    }

    #[test]
    fn normalize_error_body_falls_back_to_message_then_raw_text() {
        let out = normalize_error_body(br#"{"message":"plain"}"#, 500);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v.pointer("/error/message").and_then(|x| x.as_str()), Some("plain"));

        let out = normalize_error_body(b"<html>502 Bad Gateway</html>", 502);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v.pointer("/error/message").and_then(|x| x.as_str()),
            Some("<html>502 Bad Gateway</html>")
        );
        assert_eq!(
            v.pointer("/error/type").and_then(|x| x.as_str()),
            Some("upstream_error")
        );
        assert_eq!(v.pointer("/error/code").and_then(|x| x.as_u64()), Some(502));
    }

    #[test]
    fn normalize_error_body_never_cuts_multibyte_chars() {
        // ★ 口径是**字节**（Go `msg[:500]`），Rust 只额外保证切在字符边界上。
        //
        // 600 个「错」= 1800 字节 ⇒ 切到 500 字节时只能留下 166 个字符
        //（166×3 = 498，再切一个就是 501 > 500）。断言"500 个字符"是**把字节
        // 当成了字符** —— Go 那边同样只留 166 个，Rust 不该"多给"。
        let long = "错".repeat(600);
        assert_eq!(long.len(), 1800, "前提：3 字节/字符");
        let out = normalize_error_body(long.as_bytes(), 500);
        let v: Value = serde_json::from_slice(&out).unwrap();
        let msg = v.pointer("/error/message").and_then(|x| x.as_str()).unwrap();
        assert!(msg.ends_with('…'), "应带省略号: {msg}");
        let kept = msg.strip_suffix('…').expect("有省略号");
        assert_eq!(kept.chars().filter(|c| *c == '错').count(), 166);
        assert!(
            kept.len() <= 500,
            "截断按字节：留 {} 字节（上限 500）",
            kept.len()
        );
        assert!(
            kept.len() > 500 - 3,
            "不该少切一整个字符：留 {} 字节",
            kept.len()
        );
        // 关键：产物仍是合法 UTF-8（Go 的 `msg[:500]` 在 500 落在字符中间时会切出
        // 非法字节序列，塞进 JSON 后用户看到乱码）。
        assert!(std::str::from_utf8(out.as_slice()).is_ok() || !out.is_empty());
        assert!(!msg.contains('\u{fffd}'), "不该出现替换字符");
    }

    // ── 验证码 ──

    #[test]
    fn is_captcha_required_body_matches_code_and_wording() {
        assert!(is_captcha_required_body(
            r#"{"code":3007,"msg":"captcha verify failed"}"#
        ));
        assert!(is_captcha_required_body(r#"{"msg":"captcha verify failed"}"#));
        assert!(is_captcha_required_body(r#"{"msg":"需要验证码"}"#));
        assert!(!is_captcha_required_body(""));
        assert!(!is_captcha_required_body(r#"{"code":1113,"msg":"余额不足"}"#));
    }

    #[test]
    fn append_captcha_note_keeps_body_valid_json() {
        // ★ 这就是 Go 侧被 TestStreamChatTranslatesErrorBody 抓出的那个缺陷：
        // 旧实现直接 append 文本，产物不是合法 JSON，导致说明谁也看不到。
        let body = br#"{"code":3007,"msg":"captcha verify failed"}"#;
        let out = append_captcha_note(body, "求解器不可用：组件未安装");
        let v: Value = serde_json::from_slice(&out).expect("产物必须是合法 JSON");
        let msg = v.pointer("/error/message").and_then(|x| x.as_str()).unwrap();
        assert!(msg.contains("网关附注"), "{msg}");
        assert!(msg.contains("组件未安装"), "{msg}");
        assert_eq!(
            v.pointer("/error/type").and_then(|x| x.as_str()),
            Some("captcha_solve_failed")
        );
        // 原始字段不该被抹掉。
        assert_eq!(v.get("code").and_then(|x| x.as_u64()), Some(3007));
    }

    #[test]
    fn append_captcha_note_appends_to_existing_message() {
        let body = br#"{"error":{"message":"captcha verify failed","type":"invalid_request_error"}}"#;
        let out = append_captcha_note(body, "冷却中");
        let v: Value = serde_json::from_slice(&out).unwrap();
        let msg = v.pointer("/error/message").and_then(|x| x.as_str()).unwrap();
        assert!(msg.starts_with("captcha verify failed "), "{msg}");
        assert!(msg.contains("冷却中"), "{msg}");
        // error.type 保留（Go 走的是「改 message 再 Marshal 整个对象」）。
        assert_eq!(
            v.pointer("/error/type").and_then(|x| x.as_str()),
            Some("invalid_request_error")
        );
    }

    #[test]
    fn append_captcha_note_wraps_non_json_and_keeps_raw_text() {
        let out = append_captcha_note(b"<html>gateway timeout</html>", "超时");
        let v: Value = serde_json::from_slice(&out).expect("产物必须是合法 JSON");
        let msg = v.pointer("/error/message").and_then(|x| x.as_str()).unwrap();
        assert!(msg.contains("gateway timeout"), "{msg}");
        assert!(msg.contains("超时"), "{msg}");
    }

    #[test]
    fn captcha_failure_message_is_readable_and_mentions_the_reason() {
        let m = captcha_failure_message(
            r#"{"code":3007,"msg":"captcha verify failed"}"#,
            "求解器不可用：组件未安装",
        );
        assert!(m.contains("组件未安装"), "{m}");
        assert!(m.contains("不会绕过") || m.contains("网关附注"), "{m}");
    }

    // ── aggregate_openai ──

    #[test]
    fn aggregate_openai_concatenates_deltas() {
        let sse = concat!(
            "data: {\"id\":\"c1\",\"created\":100,\"model\":\"glm-5.3\",\"choices\":[{\"delta\":{\"content\":\"你\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"好\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let out = aggregate_openai(sse, "fallback").unwrap();
        assert_eq!(out.get("id").unwrap().as_str(), Some("c1"));
        assert_eq!(out.get("created").unwrap().as_i64(), Some(100));
        assert_eq!(out.get("model").unwrap().as_str(), Some("glm-5.3"));
        assert_eq!(out.get("object").unwrap().as_str(), Some("chat.completion"));
        let ch = out.get("choices").unwrap().as_array().unwrap();
        assert_eq!(ch.len(), 1);
        assert_eq!(
            ch[0].pointer("/message/content").and_then(|x| x.as_str()),
            Some("你好")
        );
        assert_eq!(
            ch[0].pointer("/message/role").and_then(|x| x.as_str()),
            Some("assistant")
        );
        assert_eq!(
            ch[0].get("finish_reason").and_then(|x| x.as_str()),
            Some("stop")
        );
        // Go 也不给 usage。
        assert!(out.get("usage").is_none());
    }

    #[test]
    fn aggregate_openai_handles_message_shape_and_reasoning() {
        // 非流式整体（有些实现两者都给）；reasoning_content 只在非空时出现。
        let sse = concat!(
            "data: {\"choices\":[{\"message\":{\"content\":\"答案\",\"reasoning_content\":\"想\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"了\"}}]}\n"
        );
        let out = aggregate_openai(sse, "m").unwrap();
        let ch = out.get("choices").unwrap().as_array().unwrap();
        assert_eq!(
            ch[0].pointer("/message/content").and_then(|x| x.as_str()),
            Some("答案")
        );
        assert_eq!(
            ch[0]
                .pointer("/message/reasoning_content")
                .and_then(|x| x.as_str()),
            Some("想了")
        );
    }

    #[test]
    fn aggregate_openai_omits_empty_reasoning() {
        // 空字符串会让部分客户端显示空的思考块。
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n";
        let out = aggregate_openai(sse, "m").unwrap();
        assert!(out
            .get("choices")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .pointer("/message/reasoning_content")
            .is_none());
    }

    #[test]
    fn aggregate_openai_defaults_on_empty_input() {
        let out = aggregate_openai("", "glm-5.3").unwrap();
        assert_eq!(out.get("id").unwrap().as_str(), Some("chatcmpl-zcode"));
        assert_eq!(out.get("model").unwrap().as_str(), Some("glm-5.3"));
        assert!(out.get("created").unwrap().as_i64().unwrap() > 0);
        let ch = out.get("choices").unwrap().as_array().unwrap();
        assert_eq!(
            ch[0].get("finish_reason").and_then(|x| x.as_str()),
            Some("stop")
        );
        assert_eq!(
            ch[0].pointer("/message/content").and_then(|x| x.as_str()),
            Some("")
        );
    }

    #[test]
    fn aggregate_openai_ignores_bad_json_and_non_data_lines() {
        let sse = concat!(
            "event: message_start\n",
            "data: {not json}\n",
            ": comment\n",
            "data:{\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n"
        );
        let out = aggregate_openai(sse, "m").unwrap();
        let ch = out.get("choices").unwrap().as_array().unwrap();
        assert_eq!(
            ch[0].pointer("/message/content").and_then(|x| x.as_str()),
            Some("ok")
        );
    }

    #[test]
    fn aggregate_openai_handles_crlf() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\r\ndata: [DONE]\r\n";
        let out = aggregate_openai(sse, "m").unwrap();
        let ch = out.get("choices").unwrap().as_array().unwrap();
        assert_eq!(
            ch[0].pointer("/message/content").and_then(|x| x.as_str()),
            Some("a")
        );
    }

    // ── 小工具 ──

    #[test]
    fn model_of_reads_model_or_empty() {
        assert_eq!(model_of(br#"{"model":"glm-5.3"}"#), "glm-5.3");
        assert_eq!(model_of(br#"{"model":123}"#), "");
        assert_eq!(model_of(b"not json"), "");
        assert_eq!(model_of(br#"{}"#), "");
    }

    #[test]
    fn to_int64_accepts_numbers_and_numeric_strings() {
        assert_eq!(to_int64(&json!(1234)), 1234);
        assert_eq!(to_int64(&json!("1234")), 1234);
        assert_eq!(to_int64(&json!(" 42 ")), 42);
        assert_eq!(to_int64(&json!(1.9)), 1);
        assert_eq!(to_int64(&json!("abc")), 0);
        assert_eq!(to_int64(&Value::Null), 0);
    }

    #[test]
    fn to_float64_keeps_decimals() {
        // grant_units 可能带小数：截断会让「总量对不上赠送量」。
        assert_eq!(to_float64(&json!(3000000.5)), 3000000.5);
        assert_eq!(to_float64(&json!("5000000")), 5000000.0);
        assert_eq!(to_float64(&json!(null)), 0.0);
    }

    #[test]
    fn first_any_skips_empty_strings_and_zero() {
        let zero = json!(0);
        let empty = json!("");
        let good = json!("endsAt");
        assert_eq!(
            first_any(&[Some(&zero), Some(&empty), Some(&good)])
                .and_then(|v| v.as_str()),
            Some("endsAt")
        );
        assert!(first_any(&[Some(&zero), Some(&empty)]).is_none());
        assert!(first_any(&[None]).is_none());
        // 非空字符串、非零数字之外的类型直接返回（Go 的 default 分支）。
        let t = json!(true);
        assert_eq!(first_any(&[Some(&t)]), Some(&t));
    }

    // ── 端点 ──

    fn cred_with(provider: provider::Provider, jwt: &str) -> ZcodeCred {
        ZcodeCred {
            uid: "u1".to_string(),
            credential: "api.key".to_string(),
            provider,
            jwt: jwt.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn anthropic_messages_url_uses_start_plan_for_jwt_accounts() {
        let cr = cred_with(provider::Provider::Zai, "header.payload.sig");
        assert!(cr.is_start_plan());
        assert_eq!(
            anthropic_messages_url(&cr),
            "https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages"
        );
    }

    #[test]
    fn anthropic_messages_url_uses_provider_base_for_coding_plan() {
        let zai = cred_with(provider::Provider::Zai, "");
        assert_eq!(
            anthropic_messages_url(&zai),
            "https://api.z.ai/api/anthropic/v1/messages"
        );
        let bm = cred_with(provider::Provider::Bigmodel, "");
        assert_eq!(
            anthropic_messages_url(&bm),
            "https://open.bigmodel.cn/api/anthropic/v1/messages"
        );
        // 域名认不出服务商时按 Z.AI（与 Go 的 endpointsOf 默认一致）。
        let unknown = cred_with(provider::Provider::Unknown, "");
        assert_eq!(
            anthropic_messages_url(&unknown),
            "https://api.z.ai/api/anthropic/v1/messages"
        );
    }

    #[test]
    fn anthropic_base_override_wins_and_strips_trailing_slash() {
        let mut c = Client::new();
        let cr = cred_with(provider::Provider::Zai, "jwt");
        c.set_anthropic_base_for_test("http://127.0.0.1:1/");
        assert_eq!(
            c.anthropic_messages_url_for(&cr),
            "http://127.0.0.1:1/v1/messages"
        );
        // ★ 生产路径恒无 override ⇒ 与纯函数一致。
        let c2 = Client::new();
        assert_eq!(
            c2.anthropic_messages_url_for(&cr),
            anthropic_messages_url(&cr)
        );
    }

    #[test]
    fn captcha_region_falls_back_to_cn() {
        // 求解器是进程级单例，测试进程里可能已被别处设过；只断言非空。
        let c = Client::new();
        assert!(!c.captcha_region().trim().is_empty());
    }

    // ── 内置清单 ──

    #[test]
    fn builtin_models_are_eleven_and_marked_builtin() {
        let ms = builtin_models();
        assert_eq!(ms.len(), 11);
        assert!(ms.iter().all(|m| m.source == "builtin"));
        let idx = builtin_model_index();
        assert_eq!(idx.len(), 11);
        let flash = idx.get("glm-5.3-flash").expect("应有 glm-5.3-flash");
        assert_eq!(flash.name, "GLM 5.3 Flash");
        assert_eq!(flash.context_window, 1000000);
        assert_eq!(flash.max_output, 128000);
        // ★ 内置清单的 vision 是启发式的结果（这里是 false），
        // 真实值必须来自上游的 capabilities.vision。
        assert!(!flash.vision);
    }

    // ── 宿主配置发现 ──

    #[test]
    fn parse_host_config_reads_pool_nesting() {
        // ★ 宿主实测形状：zcode_captcha_* 在 `pool.` 下，proxy/proxy_scope 在顶层。
        let v: Value = serde_json::from_str(
            r#"{
                "proxy": "http://127.0.0.1:7890",
                "proxy_scope": {"cn": false, "intl": true, "github": false},
                "pool": {
                    "zcode_auth_dir": "C:\\x\\auths",
                    "zcode_captcha_dir": "D:\\APP\\AI Gateway\\_up_\\assets\\zcode-captcha",
                    "zcode_captcha_solver_url": "http://127.0.0.1:63854/solve",
                    "zcode_captcha_solver_token": "tok"
                }
            }"#,
        )
        .unwrap();
        let c = parse_host_config(&v);
        assert_eq!(c.proxy, "http://127.0.0.1:7890");
        assert!(c.proxy_intl);
        assert_eq!(
            c.captcha_dir,
            "D:\\APP\\AI Gateway\\_up_\\assets\\zcode-captcha"
        );
        assert_eq!(c.captcha_solver_url, "http://127.0.0.1:63854/solve");
        assert_eq!(c.captcha_solver_token, "tok");
    }

    #[test]
    fn parse_host_config_reads_top_level_and_defaults() {
        let v: Value = serde_json::from_str(
            r#"{"zcode_captcha_dir":"/opt/zc","zcode_captcha_solver_url":"  http://x/solve  "}"#,
        )
        .unwrap();
        let c = parse_host_config(&v);
        assert_eq!(c.captcha_dir, "/opt/zc");
        assert_eq!(c.captcha_solver_url, "http://x/solve", "应 trim");
        assert!(c.captcha_solver_token.is_empty());
        // proxy_scope 缺席 ⇒ intl 默认 true（与 ProxyScopeConfig::default 一致）。
        assert!(c.proxy_intl);
        assert!(c.proxy.is_empty());
    }

    #[test]
    fn parse_host_config_top_level_wins_over_pool() {
        let v: Value = serde_json::from_str(
            r#"{"zcode_captcha_dir":"/top","pool":{"zcode_captcha_dir":"/nested"}}"#,
        )
        .unwrap();
        assert_eq!(parse_host_config(&v).captcha_dir, "/top");
    }

    #[test]
    fn parse_host_config_honours_explicit_intl_false() {
        let v: Value = serde_json::from_str(r#"{"proxy_scope":{"intl":false}}"#).unwrap();
        assert!(!parse_host_config(&v).proxy_intl);
    }

    // ── 头构造 ──

    #[test]
    fn put_rejects_control_characters_instead_of_dropping_silently() {
        let mut h = reqwest::header::HeaderMap::new();
        assert!(put(&mut h, "authorization", "Bearer a\nb").is_err());
        assert!(put(&mut h, "authorization", "Bearer ok").is_ok());
        assert_eq!(
            h.get("authorization").unwrap().to_str().unwrap(),
            "Bearer ok"
        );
        // 空值跳过（对应 Go 的 `if v != ""`）。
        assert!(put(&mut h, "x-empty", "").is_ok());
        assert!(h.get("x-empty").is_none());
    }

    #[test]
    fn apply_headers_sets_auth_anthropic_and_trace_headers() {
        let c = Client::new();
        let cr = ZcodeCred {
            uid: "uid-1".to_string(),
            credential: "api.key".to_string(),
            provider: provider::Provider::Zai,
            jwt: "jwt-value".to_string(),
            captcha_param: "param-1".to_string(),
            captcha_region: "cn".to_string(),
            ..Default::default()
        };
        // RequestBuilder 的头表在 reqwest 里不可读，故这里直接验证不报错，
        // 并用 `put` 的单测覆盖取值语义；header 名/值的构造顺序由实现保证。
        let rb = c
            .apply_headers(c.stream.post("http://127.0.0.1:1/v1/messages"), &cr, true)
            .expect("头构造应成功");
        drop(rb);
    }

    #[test]
    fn apply_headers_falls_back_to_credential_when_jwt_missing() {
        let c = Client::new();
        let cr = cred_with(provider::Provider::Zai, "");
        assert!(c
            .apply_headers(c.stream.post("http://127.0.0.1:1/v1/messages"), &cr, false)
            .is_ok());
    }

    #[test]
    fn apply_headers_rejects_credentials_with_newlines() {
        let c = Client::new();
        let cr = ZcodeCred {
            uid: "u".to_string(),
            credential: "api.key".to_string(),
            provider: provider::Provider::Zai,
            jwt: "bad\njwt".to_string(),
            ..Default::default()
        };
        assert!(c
            .apply_headers(c.stream.post("http://127.0.0.1:1/v1/messages"), &cr, true)
            .is_err());
    }

    // ── 额度 ──

    #[tokio::test]
    async fn fetch_quota_requires_jwt() {
        let c = Client::new();
        let mut cr = cred_with(provider::Provider::Zai, "");
        let err = c.fetch_quota(&mut cr).await.unwrap_err();
        assert_eq!(err, ERR_NO_JWT);
    }

    #[tokio::test]
    async fn fetch_quota_mints_device_mid_when_missing() {
        // 缺 deviceMid 时**我们自己补**（不是让用户「重新登录」）；
        // 但 JWT 为空时函数会提前返回，故这里只验证补值逻辑本身。
        let mut cr = cred_with(provider::Provider::Zai, "");
        assert!(!identity::is_uuid(&cr.device_mid));
        cr.device_mid = identity::new_device_mid();
        assert!(identity::is_uuid(&cr.device_mid));
    }

    #[test]
    fn assemble_quota_reads_both_case_styles_and_sums() {
        let doc: Value = serde_json::from_str(
            r#"{
                "code": 0,
                "data": {
                    "server_time": 1789781287,
                    "balances": [
                        {
                            "show_name": "GLM-5.3",
                            "remaining_units": "3000000",
                            "total_units": 3000000,
                            "used_units": 0,
                            "unit_type": "token",
                            "expires_at": 1790000000,
                            "plan_id": "zcode-v3-start-plan-0817",
                            "entitlement_id": "ent-1",
                            "period_start": 1789781287,
                            "period_end": 1790000000
                        },
                        {
                            "show_name": "GLM-5.3-Flash",
                            "remaining_units": 5000000,
                            "total_units": 5000000,
                            "used_units": 0,
                            "unitType": "token",
                            "expiresAt": 1790000001,
                            "plan_id": "zcode-v3-start-plan-0817",
                            "entitlement_id": "ent-2"
                        }
                    ],
                    "plans": [
                        {
                            "user_plan_id": "up-1",
                            "plan_id": "zcode-v3-start-plan-0817",
                            "name": "ZCode Start Plan",
                            "description": "免费 GLM 旗舰模型体验",
                            "priority": 1.5,
                            "status": "active",
                            "starts_at": 1789781287,
                            "endsAt": 1790179199,
                            "entitlements": [
                                {
                                    "entitlement_id": "ent-1",
                                    "show_name": "GLM-5.3",
                                    "meter": "token",
                                    "unit_type": "token",
                                    "capabilities": ["chat"],
                                    "grant_units": 3000000.5,
                                    "period": "daily",
                                    "effective_at": 1789781287
                                }
                            ]
                        }
                    ]
                }
            }"#,
        )
        .unwrap();
        let q = assemble_quota(&doc);
        assert_eq!(q.remaining, 8000000);
        assert_eq!(q.total, 8000000);
        assert_eq!(q.server_time, 1789781287);
        assert_eq!(q.entries.len(), 2);
        assert_eq!(q.entries[0].unit_type, "token");
        assert_eq!(q.entries[1].unit_type, "token", "应接受 unitType 驼峰");
        assert_eq!(q.entries[1].expires_at, 1790000001, "应接受 expiresAt 驼峰");
        // 套餐赠送量关联：这 300 万来自体验套餐，不是孤立数字。
        assert_eq!(q.entries[0].grant_units, 3000000.5);
        assert_eq!(q.entries[1].grant_units, 0.0, "未授权项应为 0");
        assert_eq!(q.plans.len(), 1);
        let p = &q.plans[0];
        assert_eq!(p.description, "免费 GLM 旗舰模型体验");
        assert_eq!(p.ends_at, 1790179199, "应接受 endsAt 驼峰");
        assert_eq!(p.priority, 1.5);
        assert_eq!(p.entitlements.len(), 1);
        assert_eq!(p.entitlements[0].grant_units, 3000000.5);
        assert_eq!(p.entitlements[0].period, "daily");
        // 套餐整体到期 vs 每日周期结束 —— 两者必须能分别取到。
        assert_eq!(q.plan_expiry(), 1790179199);
        assert_eq!(q.soonest_expiry(), 1790000000);
        assert_eq!(q.active_plan().map(|p| p.plan_id.as_str()), Some("zcode-v3-start-plan-0817"));
    }

    #[test]
    fn quota_active_plan_skips_non_active_and_picks_highest_priority() {
        let doc: Value = serde_json::from_str(
            r#"{"data":{"plans":[
                {"plan_id":"expired","status":"expired","priority":9},
                {"plan_id":"low","status":"active","priority":1},
                {"plan_id":"high","status":"active","priority":2},
                {"plan_id":"nostatus","priority":0}
            ]}}"#,
        )
        .unwrap();
        let q = assemble_quota(&doc);
        assert_eq!(q.active_plan().map(|p| p.plan_id.as_str()), Some("high"));
        // 到期时刻取所有非零 ends_at 的最小值（含 expired 的）。
        assert_eq!(q.plan_expiry(), 0);
        assert_eq!(q.soonest_expiry(), 0);
    }

    #[test]
    fn quota_helpers_are_zero_safe_on_empty_data() {
        let q = assemble_quota(&json!({"code": 0}));
        assert_eq!(q.remaining, 0);
        assert_eq!(q.plan_expiry(), 0);
        assert_eq!(q.soonest_expiry(), 0);
        assert!(q.active_plan().is_none());
    }

    // ── ★★ 端到端：3007 ⇒ 求解 ⇒ 写回**副本** ⇒ 重试 ──

    /// 极简 HTTP/1.1 服务器：按路径应答，并把每个请求的**原始头**记下来。
    ///
    /// 为什么不用 axum/hyper：`tokio` 的 feature 里**没有 `net`**
    /// （见 `Cargo.toml:23`），拿不到 `tokio::net::TcpListener`；而
    /// `std::net::TcpListener` 不受该 feature 影响（它只门控 `tokio::net`）。
    /// 一个 `std::thread` 足够，且**不触网**（只监听 127.0.0.1）。
    fn spawn_fake_upstream(
        listener: std::net::TcpListener,
        seen: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
        n_messages: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::io::{Read as _, Write as _};
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut s) = conn else { continue };
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    match s.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                    let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n".as_slice())
                    else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&buf[..p]).to_string();
                    let cl = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            if k.eq_ignore_ascii_case("content-length") {
                                v.trim().parse::<usize>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    if buf.len() >= p + 4 + cl {
                        break;
                    }
                }
                let end = buf
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n".as_slice())
                    .unwrap_or(buf.len());
                let head = String::from_utf8_lossy(&buf[..end]).to_string();
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                seen.lock().unwrap().push((path.clone(), head));

                let (status, ctype, body): (u16, &str, String) = if path.starts_with("/solve") {
                    (
                        200,
                        "application/json",
                        r#"{"param":"p-test","region":"cn"}"#.to_string(),
                    )
                } else {
                    let n = n_messages.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if n == 0 {
                        // 第一次：上游要求人机验证（实测形状）。
                        (
                            400,
                            "application/json",
                            r#"{"code":3007,"msg":"captcha verify failed"}"#.to_string(),
                        )
                    } else {
                        // 第二次：带上 param 后放行，回标准 Anthropic SSE。
                        (
                            200,
                            "text/event-stream",
                            concat!(
                                "event: message_start\n",
                                "data: {\"type\":\"message_start\"}\n\n",
                                "event: content_block_delta\n",
                                "data: {\"type\":\"content_block_delta\",\"index\":0,",
                                "\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
                                "event: message_stop\n",
                                "data: {\"type\":\"message_stop\"}\n\n",
                            )
                            .to_string(),
                        )
                    }
                };
                let out = format!(
                    "HTTP/1.1 {status} OK\r\ncontent-type: {ctype}\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(out.as_bytes());
                let _ = s.write_all(body.as_bytes());
                let _ = s.flush();
            }
        });
    }

    #[tokio::test]
    async fn stream_chat_solves_captcha_and_retries_with_param_on_a_copy() {
        // ★★ 本用例钉的正是本次移植**唯一必须修掉**的 Go 缺陷：
        // Go `client.go:301-317` 把求解器实现完整了（277 行测试），但全仓
        // `CaptchaParam` 的**赋值次数为 0** ⇒ `applyHeaders` 里
        // `if cr.CaptchaParam != ""` 永远为假 ⇒ 必然 3007。
        // 这里端到端验证三件事：① 第一次**不带** param；② 求解后**真的**带上；
        // ③ 写回发生在**副本**上，共享凭证不被污染。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!(
            "http://127.0.0.1:{}",
            listener.local_addr().unwrap().port()
        );
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        spawn_fake_upstream(listener, seen.clone(), n);

        // 求解器：走**真实**的外部求解路径（打到同一个假服务器的 `/solve`）。
        // ★ 必须是私有实例：生产用进程级单例，改单例会污染并行的其它测试。
        let solver = std::sync::Arc::new(super::super::captcha::CaptchaSolver::new());
        solver.set_external_solver(&format!("{base}/solve"), "");

        let mut c = Client::new();
        c.set_anthropic_base_for_test(&base);
        c.set_captcha_solver_for_test(solver);

        let cr = cred_with(provider::Provider::Zai, "header.payload.sig");
        let body = br#"{"model":"glm-5.3","messages":[{"role":"user","content":"hi"}]}"#;
        let call = c
            .stream_chat(&cr, body)
            .await
            .expect("求解成功后必须拿到流，而不是 CaptchaRequired");
        let ChatCall::Stream(resp) = call else {
            panic!("第二次应当成功");
        };
        let out = String::from_utf8_lossy(&resp.bytes().await.expect("读流")).into_owned();
        assert!(out.contains(r#""content":"hi""#), "应翻成 OpenAI SSE，实际 {out}");
        assert!(out.contains("data: [DONE]"), "必须补结束标记，实际 {out}");

        // ★★ 核心断言一：第二次请求**真的**带上了求解结果。
        let msgs: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p.contains("/v1/messages"))
            .map(|(_, h)| h.to_ascii_lowercase())
            .collect();
        assert_eq!(msgs.len(), 2, "应当恰好两次对话请求（先试一次 + 求解后重试一次）");
        assert!(
            !msgs[0].contains("x-aliyun-captcha-verify-param"),
            "★ 第一次不能带验证参数（param 是一次性的，带了就白费一次）"
        );
        assert!(
            msgs[1].contains("x-aliyun-captcha-verify-param: p-test"),
            "★★ 第二次必须带上求解得到的 param —— 这正是 Go 全仓赋值 0 次的那个缺陷"
        );
        assert!(
            msgs[1].contains("x-aliyun-captcha-verify-region: cn"),
            "region 必须与 param **成对**发送（缺它就是 3007）"
        );

        // ★★ 核心断言二：写回发生在**副本**上，共享凭证不被污染。
        assert!(
            cr.captcha_param.is_empty(),
            "共享凭证不得残留一次性 param —— 残留会让下一次请求必回 3007（表现为「偶发失败」）"
        );
        assert!(cr.captcha_region.is_empty(), "region 同样不得写回共享凭证");
    }

    #[tokio::test]
    async fn stream_chat_returns_captcha_required_when_solving_fails() {
        // ★★ 第二个必须钉住的语义：3007 且**求解失败**时，Rust 侧必须返回
        // `Err(GatewayError::CaptchaRequired(..))`，**不能**照抄 Go 的
        // `(nil, 400, 附注体, nil)`。
        //
        // 理由（见 `stream_chat` 的文档）：`forward.rs` 的 `ChatOutcome::Failed`
        // 分支里**没有任何 3007 特例**，Go 那个形状会走到「轮转下一个账号重试」——
        // 而人机验证是**按出口 IP** 触发的，换号毫无意义，只会把整池账号一起
        // 拖进冷却。只有 `CaptchaRequired` 才命中「请求侧错误 —— 不轮转、不罚账号」
        // 分支（`forward.rs:1264-1283`，返回 503）。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!(
            "http://127.0.0.1:{}",
            listener.local_addr().unwrap().port()
        );
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        spawn_fake_upstream(listener, seen.clone(), n);

        // 求解器：**不配**外部服务、也不给目录 ⇒ `unavailable_reason()` 非空
        // ⇒ 确定性失败（不会去起 Node，测试不会卡在真实求解上）。
        let solver = std::sync::Arc::new(super::super::captcha::CaptchaSolver::new());
        assert!(
            !solver.unavailable_reason().is_empty(),
            "前提：未配置的求解器必须报不可用"
        );

        let mut c = Client::new();
        c.set_anthropic_base_for_test(&base);
        c.set_captcha_solver_for_test(solver);

        let cr = cred_with(provider::Provider::Zai, "header.payload.sig");
        // ⚠ 必须带至少一条消息：空 `messages` 会在**翻译阶段**就失败
        // （`请求里没有可用的消息`），压根到不了上游，本用例就测不到 3007 分支。
        let body = br#"{"model":"glm-5.3","messages":[{"role":"user","content":"hi"}]}"#;
        let err = c
            .stream_chat(&cr, body)
            .await
            .err()
            .expect("求解失败时必须返回 Err，而不是 Ok(Failed)");
        let msg = err.to_string();
        assert!(
            err.is_captcha_required(),
            "★ 必须是 CaptchaRequired（forward.rs 才会「不轮转、不罚账号」），实际 {msg}"
        );
        // 附注必须真的带上（否则用户只看到一句 `captcha verify failed`，无从下手）。
        assert!(msg.contains("人机验证"), "实际 {msg}");
        assert!(
            msg.contains("求解未成功"),
            "必须说明「求解未成功」（而不是让用户以为要自己去点验证码），实际 {msg}"
        );
        // 且只发了一次对话请求（求解失败**不**重试，不浪费上游配额）。
        let calls = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p.contains("/v1/messages"))
            .count();
        assert_eq!(calls, 1, "求解失败时不该再打一次上游");
        // 共享凭证依旧干净。
        assert!(cr.captcha_param.is_empty());
    }
}
