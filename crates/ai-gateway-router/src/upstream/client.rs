//! 上游 HTTP 客户端：chat 流式调用、token 刷新、积分查询。
//!
//! 对应 Go 源文件 `internal/upstream/client.go` 的 `Client` 部分。
//!
//! # 两个 client，共享一个连接池
//!
//! - `HTTP`：短 RPC（refresh / billing），有总时长上限
//! - `chat`：聊天 SSE，**无总时长上限**（长回答可能持续很久），
//!   首字节由 `header_timeout` 约束，流中空闲由 `idle_timeout` 约束
//!
//! 两者共享同一个 `reqwest::Client`（连接池不重复），只是超时策略不同 ——
//! 因此这里用「一个 client + 每请求超时」而非两个 client。
//!
//! # 区域路由
//!
//! 国服的 chat 与 billing **分属不同域名**（`copilot.tencent.com` /
//! `www.codebuddy.cn`），国际版两者同域（`www.workbuddy.ai`）。
//! 请求基址一律按账号 `domain` 推导，不能写死。

use std::time::Duration;

use reqwest::StatusCode;

use crate::auth::Auth;
use crate::error::{GatewayError, Result};
use crate::upstream::classify::{classify, ErrKind};
use crate::upstream::headers;
use crate::upstream::payload::prepare_body_for_region;

/// 国服 chat 基址。
pub const CHAT_BASE_CN: &str = "https://copilot.tencent.com";
/// 国服 billing 基址（与 chat 分域）。
pub const BILLING_BASE_CN: &str = "https://www.codebuddy.cn";
/// 国际版基址（chat / billing / 刷新同域）。
pub const BASE_INTL: &str = "https://www.workbuddy.ai";

/// 拉模型配置用的 User-Agent。
///
/// **必须**用 WorkBuddy 前缀（实测）：`WorkBuddy/...` 返回本产品的模型清单，
/// `CLI/...` 返回的是 CodeBuddy 产品的另一套清单，其他 UA 直接 400。
/// 两个清单差异很大且各自都「看起来合理」，用错会静默拿到错误的模型集。
const MODELS_CONFIG_UA: &str = "WorkBuddy/5.5.2 WorkBuddy/5.5.2 CLI/2.137.1";

/// 模型配置接口路径。
///
/// 不用 `/console/enterprises/personal/models`：该接口在国际版恒返回 500。
/// `/v3/config` 两个区域都可用。
const MODELS_CONFIG_PATH: &str = "/v3/config";

/// 单次响应体读取上限（1 MiB），防止上游异常时吃满内存。
const MAX_BODY: usize = 1 << 20;

/// 上游客户端配置。
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// 短 RPC 总时长上限。
    pub timeout: Duration,
    /// chat SSE 首字节前上限。
    pub header_timeout: Duration,
    /// chat SSE 流中空闲上限。
    pub idle_timeout: Duration,
    /// 出站请求体指纹脱敏开关。
    pub sanitize_fingerprints: bool,
    /// 显式代理（空 = 读环境变量）。
    pub proxy: String,
    /// 国服 chat 基址覆盖（空 = 用内置常量）。
    ///
    /// 存在意义与 Go 侧 `Client.ChatBaseCN` 相同：**A/B 对照测试**要把网关指向
    /// 假上游，而内置基址是写死的真实域名。生产路径留空即可。
    pub chat_base_cn: String,
    /// 国服 billing 基址覆盖（空 = 用内置常量）。
    pub billing_base_cn: String,
    /// 国际版基址覆盖（空 = 用内置常量）。
    pub base_intl: String,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            header_timeout: Duration::from_secs(120),
            idle_timeout: Duration::from_secs(300),
            sanitize_fingerprints: true,
            proxy: String::new(),
            chat_base_cn: String::new(),
            billing_base_cn: String::new(),
            base_intl: String::new(),
        }
    }
}

/// 构造出站代理（`None` = 直连）。
///
/// 顺序与 Go 侧 `newTransport` 一致：**显式配置优先**，否则回落环境变量。
/// 无论来源，一律挂上 `NO_PROXY` 豁免表 —— 见 [`Client::new`] 里那段注释。
fn build_proxy(cfg: &ClientConfig) -> Option<reqwest::Proxy> {
    let explicit = cfg.proxy.trim();
    let raw = if explicit.is_empty() {
        // 环境变量回落。小写优先（curl 传统写法），其次大写；
        // 与 Go 的 ProxyFromEnvironment 取值顺序一致。
        [
            "https_proxy",
            "HTTPS_PROXY",
            "http_proxy",
            "HTTP_PROXY",
            "all_proxy",
            "ALL_PROXY",
        ]
        .iter()
        .find_map(|k| std::env::var(k).ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())?
    } else {
        explicit.to_string()
    };
    let url = if raw.contains("://") {
        raw
    } else {
        format!("http://{raw}")
    };
    // 解析失败时**不静默直连**：那会让「用户填错代理」表现为「网络不通」。
    // 这里打日志并退回直连，至少留下线索。
    match reqwest::Proxy::all(&url) {
        Ok(p) => Some(p.no_proxy(reqwest::NoProxy::from_env())),
        Err(e) => {
            eprintln!("[upstream] 代理地址无效 {url}: {e}，本次直连");
            None
        }
    }
}

/// 上游 HTTP 客户端。
///
/// # 两个 client，与 Go 侧 `HTTP` / `ChatHTTP` 一一对应
///
/// - `http`：短 RPC（refresh / billing / models），有**总时长上限**
/// - `chat`：聊天 SSE，**无总时长上限**（长回答可能持续很久），
///   流中空闲由 [`ClientConfig::idle_timeout`] 约束
///
/// 为什么不能合成一个：Go 侧用两个 `http.Client` 共享同一个 `Transport`
///（连接池不重复）来区分「总超时」与「无总超时」。reqwest 的 `ClientBuilder.timeout`
/// 是**总截止时间**，一旦设了就管所有请求；而聊天流被总超时掐断的表现是
/// 「回答写到一半突然断流」，且错误被归到传输层，很难定位。
///
/// reqwest 没有「共享连接池但超时不同」的等价物（连接池属于 `Client`），
/// 因此这里建两个 `Client`：连接池各一份，代价是连接数翻倍 —— 相对于
/// 聊天流被误掐的风险，这个代价可以接受。
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    chat: reqwest::Client,
    cfg: ClientConfig,
}

/// 一次 chat 调用的结果。
///
/// 二者互斥：
/// - `Stream` 非空 → 流式：调用方负责按目标协议解析
/// - `Body` 非空 → 非 2xx：上游响应体（供 `classify` 判定），此时无流
pub enum ChatOutcome {
    /// 上游返回 2xx：SSE 流。
    Stream(reqwest::Response),
    /// 上游返回非 2xx：状态码 + 响应体。
    Failed { status: u16, body: String },
}

impl Client {
    /// 按配置构造客户端。
    pub fn new(cfg: ClientConfig) -> Result<Self> {
        // 用闭包复用「公共部分」的构造：ClientBuilder 不是 Clone，
        // 两个 client 必须各自从头搭一遍。
        let base = || {
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(30))
                .pool_max_idle_per_host(20)
                .pool_idle_timeout(Duration::from_secs(90))
        };

        // ── 代理 ──
        //
        // 与 Go 侧 `newTransport` 同口径：显式配置优先，否则回落环境变量
        //（`HTTPS_PROXY` / `HTTP_PROXY`），**且一律带上 `NO_PROXY` 豁免表**。
        //
        // 为什么必须显式挂 NoProxy：reqwest 的自动系统代理**只读代理变量、
        // 不读 `NO_PROXY`**。实测本机 `NO_PROXY=127.0.0.1,localhost,...` 且代理在
        // 7897 上监听时，发往 `127.0.0.1:18690`（本机假上游）的请求仍被交给代理，
        // 代理连不上该端口 → 归类成传输层失败 → 只报一句「无法连接上游」，
        // 日志里看不出任何线索。挂上 NoProxy 后环回地址直连，行为与 curl/Go 一致。
        //
        // 生产路径上上游都是公网域名，这条豁免不影响；但它让「把网关指向本机
        // 假上游做 A/B 对照」以及任何本机部署（反向代理、sidecar）都能正常工作，
        // 而不是被用户的全局代理设置静默破坏。
        let proxy = build_proxy(&cfg);

        // 短 RPC：总时长上限（refresh / billing / models 都是短请求）。
        let mut http_builder = base().timeout(cfg.timeout);
        // 聊天 SSE：**不设总超时**，只设「流中空闲上限」。
        //
        // 不设总超时是硬要求：长回答可能持续数分钟，总超时会在中途掐断流，
        // 表现为「回答写到一半断了」且被归成传输层错误。
        // `read_timeout` 的语义正好是「每次读操作的空闲上限、读成功后重置」，
        // 与 Go 侧 `idleMonitoringBody` 手工实现的效果一致。
        let mut chat_builder = base();
        if cfg.idle_timeout > Duration::ZERO {
            chat_builder = chat_builder.read_timeout(cfg.idle_timeout);
        }
        if let Some(p) = proxy {
            http_builder = http_builder.proxy(p.clone());
            chat_builder = chat_builder.proxy(p);
        }

        let http = http_builder
            .build()
            .map_err(|e| GatewayError::Other(format!("构造 HTTP 客户端失败: {e}")))?;
        let chat = chat_builder
            .build()
            .map_err(|e| GatewayError::Other(format!("构造聊天 HTTP 客户端失败: {e}")))?;

        Ok(Self { http, chat, cfg })
    }

    /// 该账号的 chat 基址。
    pub fn chat_base(&self, a: &Auth) -> &str {
        if a.is_intl() {
            if !self.cfg.base_intl.is_empty() {
                return &self.cfg.base_intl;
            }
            BASE_INTL
        } else if !self.cfg.chat_base_cn.is_empty() {
            &self.cfg.chat_base_cn
        } else {
            CHAT_BASE_CN
        }
    }

    /// 该账号的 billing 基址。
    pub fn billing_base(&self, a: &Auth) -> &str {
        if a.is_intl() {
            if !self.cfg.base_intl.is_empty() {
                return &self.cfg.base_intl;
            }
            BASE_INTL
        } else if !self.cfg.billing_base_cn.is_empty() {
            &self.cfg.billing_base_cn
        } else {
            BILLING_BASE_CN
        }
    }

    /// 组装出站请求体（脱敏开关 + 区域适配）。
    pub fn prepare_body(&self, body: &[u8], a: &Auth) -> Vec<u8> {
        prepare_body_for_region(body, self.cfg.sanitize_fingerprints, None, a.is_intl())
    }

    /// 发 chat 请求，返回原始 SSE 流或非 2xx 响应体。
    ///
    /// 非 2xx 时**不返回错误**：调用方需要 `classify(status, body)` 来判定
    /// 冷却策略，因此状态与响应体都要交出去。只有传输层失败才是 `Err`。
    pub async fn chat_stream(&self, a: &Auth, body: &[u8]) -> Result<ChatOutcome> {
        let url = format!("{}/v2/chat/completions", self.chat_base(a));
        let prepared = self.prepare_body(body, a);
        let req = self
            .chat
            .post(&url)
            .headers(headers::chat_headers(a))
            .body(prepared);
        // 用 `chat`（无总超时，仅流中空闲上限）：长回答不能被总超时掐断。
        let resp = req.send().await.map_err(transport_err)?;
        let status = resp.status();
        if status.is_success() {
            return Ok(ChatOutcome::Stream(resp));
        }
        let code = status.as_u16();
        let body = read_capped(resp).await;
        Ok(ChatOutcome::Failed { status: code, body })
    }

    /// 刷新 access token；成功时就地更新账号字段（缺省值保留旧值）。
    ///
    /// 调用方负责随后 `save_atomic`。
    pub async fn refresh_token(&self, a: &mut Auth) -> Result<()> {
        if a.refresh_token.trim().is_empty() {
            return Err(GatewayError::Other("no refreshToken".into()));
        }
        let url = format!("{}/v2/plugin/auth/token/refresh", self.chat_base(a));
        let req = self
            .http
            .post(&url)
            .headers(headers::refresh_headers(a))
            .timeout(self.cfg.timeout);
        let resp = req.send().await.map_err(transport_err)?;
        let status = resp.status();
        let raw = read_capped(resp).await;
        let data = unwrap_envelope(status, raw.as_bytes())?;

        let tok: serde_json::Value = serde_json::from_slice(&data).map_err(|e| {
            GatewayError::UpstreamParse(format!("refresh parse: {e}"))
        })?;
        let access = tok.get("accessToken").and_then(|v| v.as_str()).unwrap_or("");
        if access.is_empty() {
            return Err(GatewayError::UpstreamParse(
                "refresh_failed: no accessToken in response — re-login required".into(),
            ));
        }
        a.access_token = access.to_string();
        if let Some(rt) = tok.get("refreshToken").and_then(|v| v.as_str()) {
            if !rt.is_empty() {
                a.refresh_token = rt.to_string();
            }
        }
        if let Some(d) = tok.get("domain").and_then(|v| v.as_str()) {
            if !d.is_empty() {
                a.domain = d.to_string();
            }
        }
        // preserveExpiry：响应缺 expiresIn 时保留旧过期时间，避免刷新风暴。
        if let Some(exp) = tok.get("expiresIn").and_then(|v| v.as_i64()) {
            if exp > 0 {
                a.expires_at = chrono::Utc::now().timestamp() + exp;
            }
        }
        Ok(())
    }

    /// 查询账号积分余额与「最近到期」时刻（一次请求同时取回）。
    pub async fn user_resource_detail(&self, a: &Auth) -> Result<CreditInfo> {
        let url = format!("{}/v2/billing/meter/get-user-resource", self.billing_base(a));
        let now = chrono::Local::now();
        let body = serde_json::json!({
            "PageNumber": 1,
            "PageSize": 100,
            "ProductCode": "p_tcaca",
            "Status": [0, 3],
            "PackageEndTimeRangeBegin": now.format("%Y-%m-%d %H:%M:%S").to_string(),
            "PackageEndTimeRangeEnd": (now + chrono::Duration::days(365 * 101))
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
        });
        let req = self
            .http
            .post(&url)
            .headers(headers::billing_headers(a))
            .json(&body)
            .timeout(self.cfg.timeout);
        let resp = req.send().await.map_err(transport_err)?;
        let status = resp.status();
        let raw = read_capped(resp).await;
        let data = unwrap_envelope(status, raw.as_bytes())?;

        let v: serde_json::Value = serde_json::from_slice(&data)
            .map_err(|e| GatewayError::UpstreamParse(format!("resource parse: {e}")))?;
        let accounts = v
            .get("Response")
            .and_then(|r| r.get("Data"))
            .and_then(|d| d.get("Accounts"))
            .and_then(|a| a.as_array());
        let mut info = CreditInfo::default();
        if let Some(accts) = accounts {
            for pkg in accts {
                let remain = package_remain(pkg);
                info.remain += remain;
                // 只有「还有剩余」的套餐才代表真实到期压力；
                // 已用尽的套餐到期日再早也无意义。
                if remain <= 0 {
                    continue;
                }
                let at = package_expiry_unix(pkg);
                if at > 0 && (info.soonest_expire_at == 0 || at < info.soonest_expire_at) {
                    info.soonest_expire_at = at;
                }
            }
        }
        Ok(info)
    }

    /// 拉取该账号所在区域的可用模型清单。
    ///
    /// 数据来源是 `data.agents[name=="cli"].models`（**不是** `data.models`）：
    /// 前者是 CLI agent 真正可用的子集（客户端选模型时看到的就是它），
    /// 后者是产品全部模型池，含图片/视频生成等不能用于对话的条目。
    pub async fn fetch_models(&self, a: &Auth) -> Result<Vec<ModelInfo>> {
        let url = format!("{}{}", self.chat_base(a), MODELS_CONFIG_PATH);
        let origin = headers::origin_referer_for(a);
        let req = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", a.access_token))
            .header("Accept", "application/json")
            .header("Origin", origin)
            .header("Referer", format!("{origin}/"))
            .header("User-Agent", MODELS_CONFIG_UA)
            .timeout(self.cfg.timeout);
        let resp = req.send().await.map_err(transport_err)?;
        let status = resp.status();
        let raw = read_capped(resp).await;
        if !status.is_success() {
            return Err(GatewayError::UpstreamParse(format!(
                "models api status {}: {}",
                status.as_u16(),
                crate::upstream::classify::truncate(&raw, 120)
            )));
        }
        parse_models(raw.as_bytes())
    }
}

/// 积分余额与到期信息。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreditInfo {
    /// 所有套餐可花费积分之和（负值钳 0）。
    pub remain: i64,
    /// 仍有剩余积分的套餐中最早的到期时刻（Unix 秒）；0 = 未知。
    pub soonest_expire_at: i64,
}

/// 动态模型信息。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelInfo {
    /// 模型 ID。
    pub id: String,
    /// 展示名。
    pub name: String,
    /// 上下文窗口（= maxInputTokens）。
    pub context_window: i64,
    /// 最大输出（= maxOutputTokens）。
    pub max_tokens: i64,
    /// 支持的 reasoning 档位（空 = 未知 / 固定档）。
    pub efforts: Vec<String>,
    /// 上游给的**默认**思考档 = `reasoning.effort`（空 = 未声明）。
    ///
    /// 与 [`ModelInfo::efforts`] 分开：`efforts` 是「允许哪些档」，
    /// `default_effort` 是「不指定时用哪档」。上游同时给了两者，但默认档未必在
    /// `supported_efforts` 里（上游数据未保证），因此**不要**用它去推导
    /// `efforts`，也不要拿 `efforts[0]` 去冒充它。
    pub default_effort: String,
    /// 是否接受图片输入。
    ///
    /// `None` **不等于** `Some(false)`：上游只给对话模型写 `supportsImages`，
    /// 补全/图片生成等条目整条缺失该字段。缺失时是「未声明」，不是「不支持」——
    /// 谎报成纯文本会让客户端把本可用的图片能力关掉。
    pub supports_images: Option<bool>,
}

/// 单个套餐的可花费积分。
///
/// 与原聚合口径逐字一致：`Cycle*` 优先，负值钳 0。
fn package_remain(pkg: &serde_json::Value) -> i64 {
    let num = |k: &str| pkg.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    let size = num("CycleCapacitySize");
    let remain = num("CycleCapacityRemain");
    let used = num("CycleCapacityUsed");
    let r = if size > 0 {
        remain
    } else if remain > 0 || used > 0 {
        remain
    } else {
        num("CapacityRemain")
    };
    r.max(0)
}

/// 套餐到期时刻（Unix 秒）；0 = 未知。
fn package_expiry_unix(pkg: &serde_json::Value) -> i64 {
    for key in ["DeductionEndTime", "ExpiredTime", "CycleEndTime"] {
        if let Some(v) = pkg.get(key) {
            let at = parse_expiry_unix(v);
            if at > 0 {
                return at;
            }
        }
    }
    0
}

/// 把秒/毫秒 epoch 统一成秒（上游混用两种精度）。
pub fn normalize_epoch(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    if n > 1_000_000_000_000 {
        n / 1000
    } else {
        n
    }
}

/// 解析上游到期字段，兼容 epoch 秒/毫秒与常见日期字符串。
fn parse_expiry_unix(v: &serde_json::Value) -> i64 {
    match v {
        serde_json::Value::Number(n) => n.as_i64().map(normalize_epoch).unwrap_or(0),
        serde_json::Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return 0;
            }
            if let Ok(n) = s.parse::<i64>() {
                return normalize_epoch(n);
            }
            for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M:%S%.3f"] {
                if let Ok(ts) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
                    return chrono::Local
                        .from_local_datetime(&ts)
                        .single()
                        .map(|d| d.timestamp())
                        .unwrap_or(0);
                }
            }
            if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(s) {
                return ts.timestamp();
            }
            // 仅日期：按当日 23:59:59 计（额度一般用到当天结束）。
            if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                if let Some(dt) = d.and_hms_opt(23, 59, 59) {
                    return chrono::Local
                        .from_local_datetime(&dt)
                        .single()
                        .map(|x| x.timestamp())
                        .unwrap_or(0);
                }
            }
            0
        }
        _ => 0,
    }
}

use chrono::TimeZone;

/// 解析 `/v3/config` 响应为模型清单。
fn parse_models(raw: &[u8]) -> Result<Vec<ModelInfo>> {
    let v: serde_json::Value = serde_json::from_slice(raw)
        .map_err(|e| GatewayError::UpstreamParse(format!("models parse: {e}")))?;
    if v.get("code").and_then(|c| c.as_i64()).unwrap_or(0) != 0 {
        return Err(GatewayError::UpstreamParse(format!(
            "models api code={}",
            v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1)
        )));
    }
    let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
    let models = data
        .get("models")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();

    // 先按 id 建索引，便于给 cli 清单补元数据。
    // disabled 单独记一份：agents[cli].models 是「该 agent 允许用哪些模型」的白名单，
    // 而 disabled 是模型级停用开关，被停用的模型可以仍留在白名单里 ——
    // 两者都不看会把已停用的模型下发给客户端（选中即报错）。
    let mut meta: std::collections::HashMap<String, ModelInfo> = std::collections::HashMap::new();
    let mut disabled: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in &models {
        let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() || meta.contains_key(id) {
            continue;
        }
        let supports = m.get("supportsImages").and_then(|v| v.as_bool());
        let disabled_multimodal = m
            .get("disabledMultimodal")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        meta.insert(
            id.to_string(),
            ModelInfo {
                id: id.to_string(),
                name: m
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                context_window: m.get("maxInputTokens").and_then(|v| v.as_i64()).unwrap_or(0),
                max_tokens: m.get("maxOutputTokens").and_then(|v| v.as_i64()).unwrap_or(0),
                efforts: m
                    .get("reasoning")
                    .and_then(|r| r.get("supportedEfforts"))
                    .and_then(|e| e.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                // 上游的默认思考档。原 Go 版把它解析进匿名结构体后**全树零消费方**，
                // 客户端在 /v1/models 里看不到档位信息 —— 用户只能靠猜档位名，
                // 猜错就被 normalize_reasoning_effort 悄悄改写。这里接上。
                default_effort: m
                    .get("reasoning")
                    .and_then(|r| r.get("effort"))
                    .and_then(|e| e.as_str())
                    .unwrap_or("")
                    .to_string(),
                // 账号级多模态开关优先：上游用它表达「该账号不能发图片」，
                // 与模型自身能力无关，此时宣称支持会让客户端发出必然失败的请求。
                supports_images: if disabled_multimodal { Some(false) } else { supports },
            },
        );
        if m.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false) {
            disabled.insert(id.to_string());
        }
    }

    // 取 cli agent 的可用清单（客户端真正能选的模型）。
    let cli_models: Vec<String> = data
        .get("agents")
        .and_then(|a| a.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|ag| ag.get("name").and_then(|n| n.as_str()) == Some("cli"))
        })
        .and_then(|ag| ag.get("models"))
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let mut out: Vec<ModelInfo> = Vec::with_capacity(cli_models.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for id in &cli_models {
        if id.is_empty() || seen.contains(id) {
            continue;
        }
        match meta.get(id) {
            Some(mi) => {
                // 上游显式标了 disabled 的不下发。
                if disabled.contains(id) {
                    continue;
                }
                seen.insert(id.clone());
                out.push(mi.clone());
            }
            // cli 清单里有、models 池里没有：仍要返回（它确实可用），只是元数据未知。
            None => {
                seen.insert(id.clone());
                out.push(ModelInfo {
                    id: id.clone(),
                    ..Default::default()
                });
            }
        }
    }

    // 兜底：上游没给 cli agent 时退回全量池（宁可多不可少）。
    if out.is_empty() {
        for m in &models {
            let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if id.is_empty()
                || disabled.contains(id)
                || seen.contains(id)
                || !meta.contains_key(id)
            {
                continue;
            }
            seen.insert(id.to_string());
            out.push(meta[id].clone());
        }
    }
    if out.is_empty() {
        return Err(GatewayError::UpstreamParse(
            "models api returned empty list".into(),
        ));
    }
    Ok(out)
}

/// 解上游统一信封；非 2xx 或业务 code != 0 时返回带分类的错误。
fn unwrap_envelope(status: StatusCode, raw: &[u8]) -> Result<Vec<u8>> {
    let text = String::from_utf8_lossy(raw).to_string();
    if !status.is_success() {
        let kind = classify(status.as_u16(), &text);
        return Err(GatewayError::UpstreamError(
            crate::upstream::classify::UpstreamError {
                kind,
                status: status.as_u16(),
                msg: crate::upstream::classify::truncate(&text, 200),
            },
        ));
    }
    let v: serde_json::Value = serde_json::from_slice(raw).map_err(|e| {
        GatewayError::UpstreamParse(format!(
            "parse failed: {e} (body: {})",
            crate::upstream::classify::truncate(&text, 120)
        ))
    })?;
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
    if code != 0 {
        let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("");
        let mut kind = classify(status.as_u16(), msg);
        if kind == ErrKind::None {
            kind = ErrKind::Client;
        }
        return Err(GatewayError::UpstreamError(
            crate::upstream::classify::UpstreamError {
                kind,
                status: status.as_u16(),
                msg: format!(
                    "code={code} msg={}",
                    crate::upstream::classify::truncate(msg, 160)
                ),
            },
        ));
    }
    Ok(v
        .get("data")
        .map(|d| serde_json::to_vec(d).unwrap_or_default())
        .unwrap_or_default())
}

/// 读取响应体（带上限），失败时返回空串（调用方按状态码判定）。
async fn read_capped(resp: reqwest::Response) -> String {
    match resp.bytes().await {
        Ok(b) => {
            let cut = b.len().min(MAX_BODY);
            String::from_utf8_lossy(&b[..cut]).to_string()
        }
        Err(_) => String::new(),
    }
}

/// 把 reqwest 错误归一为传输层错误（只换号、不喂熔断）。
fn transport_err(e: reqwest::Error) -> GatewayError {
    GatewayError::Transport(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn base_urls_follow_region() {
        let c = Client::new(ClientConfig::default()).unwrap();
        let cn = Auth {
            domain: "https://copilot.tencent.com".into(),
            ..Default::default()
        };
        let intl = Auth {
            domain: "https://www.workbuddy.ai".into(),
            ..Default::default()
        };
        assert_eq!(c.chat_base(&cn), CHAT_BASE_CN);
        assert_eq!(c.billing_base(&cn), BILLING_BASE_CN);
        // 国际版 chat 与 billing 同域
        assert_eq!(c.chat_base(&intl), BASE_INTL);
        assert_eq!(c.billing_base(&intl), BASE_INTL);
    }

    #[test]
    fn package_remain_cycle_precedence() {
        // CycleCapacitySize > 0 → 取 CycleCapacityRemain
        assert_eq!(
            package_remain(&json!({"CycleCapacitySize":100,"CycleCapacityRemain":30,"CapacityRemain":999})),
            30
        );
        // Size=0 但 remain/used 非 0 → 仍取 CycleCapacityRemain
        assert_eq!(
            package_remain(&json!({"CycleCapacitySize":0,"CycleCapacityUsed":5,"CycleCapacityRemain":7,"CapacityRemain":999})),
            7
        );
        // 全零 → 回落 CapacityRemain
        assert_eq!(package_remain(&json!({"CapacityRemain":42})), 42);
        // 负值钳 0
        assert_eq!(package_remain(&json!({"CapacityRemain":-5})), 0);
    }

    #[test]
    fn expiry_prefers_deduction_end_time() {
        // 毫秒 → 秒
        assert_eq!(
            package_expiry_unix(&json!({"DeductionEndTime": 1735689600000i64})),
            1735689600
        );
        // 缺 DeductionEndTime 时回落 ExpiredTime 字符串
        assert_eq!(
            package_expiry_unix(&json!({"ExpiredTime": "1735689600"})),
            1735689600
        );
        // 仅日期 → 当日 23:59:59（**本地时区**，见 parse_expiry_unix 的实现）
        let at = package_expiry_unix(&json!({"ExpiredTime": "2026-09-15"}));
        assert!(at > 0, "日期字符串应解析成功");
        // 必须按本机时区算出期望值，不能写死偏移。
        // 曾写死 `- 8 * 3600`（北京时间），于是该用例只在 UTC+8 的机器上通过、
        // 在 CI（UTC）上必挂 —— 断言的是作者的时区，不是被测行为。
        let expected = chrono::Local
            .with_ymd_and_hms(2026, 9, 15, 23, 59, 59)
            .single()
            .expect("本地时间应唯一")
            .timestamp();
        assert_eq!(at, expected, "应为本地 23:59:59");
    }

    #[test]
    fn parse_models_prefers_cli_agent_list() {
        let raw = json!({
            "code": 0,
            "data": {
                "models": [
                    {"id":"a","name":"A","maxInputTokens":100,"maxOutputTokens":10,"supportsImages":true,
                     "reasoning":{"supportedEfforts":["low","high"]}},
                    {"id":"b","name":"B"},
                    {"id":"image-gen","name":"IMG"}
                ],
                "agents": [{"name":"cli","models":["a","b"]}]
            }
        });
        let out = parse_models(raw.to_string().as_bytes()).unwrap();
        let ids: Vec<&str> = out.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"], "只返回 cli 清单内的模型");
        assert_eq!(out[0].context_window, 100);
        assert_eq!(out[0].efforts, vec!["low", "high"]);
        assert_eq!(out[0].supports_images, Some(true));
        // b 无元数据 → 仍返回，字段为默认
        assert_eq!(out[1].supports_images, None);
    }

    #[test]
    fn parse_models_drops_disabled() {
        let raw = json!({
            "code": 0,
            "data": {
                "models": [{"id":"a","disabled":true},{"id":"b"}],
                "agents": [{"name":"cli","models":["a","b"]}]
            }
        });
        let out = parse_models(raw.to_string().as_bytes()).unwrap();
        let ids: Vec<&str> = out.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["b"], "disabled 模型不得下发");
    }

    /// 账号级多模态关闭时，即便模型声明支持也要降级为 false。
    #[test]
    fn parse_models_account_multimodal_overrides() {
        let raw = json!({
            "code": 0,
            "data": {
                "models": [{"id":"a","supportsImages":true,"disabledMultimodal":true}],
                "agents": [{"name":"cli","models":["a"]}]
            }
        });
        let out = parse_models(raw.to_string().as_bytes()).unwrap();
        assert_eq!(out[0].supports_images, Some(false));
    }

    #[test]
    fn parse_models_falls_back_to_full_pool() {
        let raw = json!({
            "code": 0,
            "data": {"models": [{"id":"a"},{"id":"b"}], "agents": []}
        });
        let out = parse_models(raw.to_string().as_bytes()).unwrap();
        assert_eq!(out.len(), 2, "无 cli 清单时退回全量池");
    }

    #[test]
    fn parse_models_empty_is_error() {
        let raw = json!({"code":0,"data":{"models":[],"agents":[]}});
        assert!(parse_models(raw.to_string().as_bytes()).is_err());
    }

    #[test]
    fn parse_models_nonzero_code_is_error() {
        let raw = json!({"code":123,"data":{}});
        assert!(parse_models(raw.to_string().as_bytes()).is_err());
    }

    #[test]
    fn normalize_epoch_matches_go() {
        assert_eq!(normalize_epoch(0), 0);
        assert_eq!(normalize_epoch(-5), 0);
        assert_eq!(normalize_epoch(1735689600), 1735689600);
        assert_eq!(normalize_epoch(1735689600000), 1735689600);
    }

    #[test]
    fn unwrap_envelope_returns_data() {
        let raw = br#"{"code":0,"msg":"ok","data":{"accessToken":"x"}}"#;
        let out = unwrap_envelope(StatusCode::OK, raw).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["accessToken"], json!("x"));
    }

    #[test]
    fn unwrap_envelope_nonzero_code_classified() {
        let raw = br#"{"code":10001,"msg":"\u79ef\u5206\u4e0d\u8db3"}"#;
        let err = unwrap_envelope(StatusCode::OK, raw).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("code=10001"), "{msg}");
    }

    #[test]
    fn unwrap_envelope_http_error_classified() {
        let raw = br#"{"msg":"credits exhausted"}"#;
        let err = unwrap_envelope(StatusCode::TOO_MANY_REQUESTS, raw).unwrap_err();
        assert!(
            matches!(&err, GatewayError::UpstreamError(u) if u.kind == ErrKind::HardCredit),
            "{err:?}"
        );
    }
}
