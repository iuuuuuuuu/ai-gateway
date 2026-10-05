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
/// 国服官网（成长中心）基址。
///
/// 与 [`BILLING_BASE_CN`] 的区别：billing 在国服走 `codebuddy.cn`（计费后台），
/// 而成长中心是**面向用户的 Web 站点**（`workbuddy.cn`）—— 两者不可互相替代。
/// 对应 Go `Client.WebBaseCN`（`client.go:1099`）。
pub const WEB_BASE_CN: &str = "https://www.workbuddy.cn";

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
    /// 国服官网（成长中心）基址覆盖（空 = 用内置常量 [`WEB_BASE_CN`]）。
    ///
    /// 与 [`ClientConfig::chat_base_cn`] 同因：A/B 对照测试要把「Web 域」的请求
    /// （领奖 `/activity/growth/tasks/{code}/claim`、Web 指纹上报 `/v2/report`）
    /// 指向假上游，而内置基址是写死的真实域名。生产路径留空即可。
    /// 对应 Go `Client.WebBaseCN`。
    pub web_base_cn: String,
    /// 国际版官网（成长中心）基址覆盖（空 = 用内置常量 [`BASE_INTL`]）。
    ///
    /// 对应 Go `Client.WebBaseIntl`。生产上 `webBase(intl) == baseIntl`
    /// （都是 `https://www.workbuddy.ai`），Go 也确实是两个独立字段。
    pub web_base_intl: String,
    /// WorkBuddy 设备 token 服务地址（空 = 不启用）。
    ///
    /// 宿主侧本地服务（`src-tauri/src/device_token_server.rs`），对应 Go
    /// `cfg.Pool.WbDeviceTokenURL`。空串 = 不发 `X-Device-Token`，行为与本特性
    /// 引入前**逐字相同**。
    pub device_token_url: String,
    /// 调该服务时的共享密钥（同机 IPC 鉴权）。
    pub device_token_token: String,
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
            web_base_cn: String::new(),
            web_base_intl: String::new(),
            device_token_url: String::new(),
            device_token_token: String::new(),
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
    /// 设备 token 提供者（`None` = 未配置 `wb_device_token_url`）。
    ///
    /// 只对**国际版**发该头，见 [`Client::attach_device_token`]。
    ///
    /// ★ 必须 `Arc` 包一层：`Client` 是 `Clone` 的（`forward.rs` 与 `server.rs`
    /// 都按值克隆它），而 provider 里的串行锁**不能**每克隆一份就分叉 ——
    /// 那等于并发限制失效，宿主侧的 Node 子进程仍会被并发调用干扰。
    device_token: Option<std::sync::Arc<crate::upstream::devicetoken::DeviceTokenProvider>>,
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
        match proxy {
            Some(p) => {
                // `.proxy()` 自身会把 `auto_sys_proxy` 置 false（reqwest
                // `async_impl/client.rs:1416`），故这一支已无系统代理泄漏。
                http_builder = http_builder.proxy(p.clone());
                chat_builder = chat_builder.proxy(p);
            }
            // ★★ 必须**显式**关掉系统代理（2026-10-01 实测发现）。
            //
            // # 缺陷形状：reqwest 默认会读 **Windows 注册表**，Go 不会
            //
            // reqwest 的 `ClientBuilder` 默认 `auto_sys_proxy = true`，`build()` 时
            // 会 `proxies.push(ProxyMatcher::system())`（`client.rs:419-421`），
            // 而 `Matcher::from_system()` 读的是**系统代理设置** —— 在 Windows 上
            // 就是注册表 `HKCU\...\Internet Settings\ProxyEnable/ProxyServer`。
            //
            // Go 的 `http.ProxyFromEnvironment` **只读环境变量、不读注册表**，
            // 且**永不代理 loopback**（`httpproxy` 的既有规则）。两者语义不同 ⇒
            // 「没配代理」在 Go 是**真直连**，在 reqwest 是**偷偷走系统代理**。
            //
            // # 为什么这是真事故（不是测试问题）
            //
            // 本机所有者开着系统代理（`ProxyEnable=1`、`127.0.0.1:7890`、mihomo）。
            // 一旦 `cfg.proxy` 为空且环境变量也没有值（老配置 / 用户没填代理），
            // Rust 网关会把**所有上游流量**交给那个代理，而 Go 版是直连 ——
            // 这正是「换 Rust 之后网络行为变了」这类最难查的差异。
            //
            // 实测症状（`--workspace` 下暴露，`-p` 单独跑时不复现，因为
            // `-p` 那次恰好没有触发系统代理读取）：
            //   · 发往本机假上游的请求被代理**重新发起**，Go 代理补上自己的
            //     `User-Agent: Go-http-client/1.1` ⇒ 「不发 UA」的断言失败
            //   · 连不上时拿到代理的 **502** 而不是 reqwest 的传输层错误
            //
            // # 为什么不是「直连」而是「与 build_proxy 同口径」
            //
            // `build_proxy` 已经自己实现了「显式配置优先、否则回落环境变量」
            // 这套 Go 语义。所以它返回 `None` 的正确含义是「**按 Go 的规则不该走代理**」，
            // 而不是「没想好」。此时显式 `.no_proxy()` 才是等价的。
            //
            // ⚠ 不要改成 `if proxy.is_some() { .proxy() }` 就完事 ——
            // 那样 `None` 分支仍会读注册表，缺陷原样保留。
            None => {
                http_builder = http_builder.no_proxy();
                chat_builder = chat_builder.no_proxy();
            }
        }

        let http = http_builder
            .build()
            .map_err(|e| GatewayError::Other(format!("构造 HTTP 客户端失败: {e}")))?;
        let chat = chat_builder
            .build()
            .map_err(|e| GatewayError::Other(format!("构造聊天 HTTP 客户端失败: {e}")))?;

        Ok(Self {
            http,
            chat,
            device_token: crate::upstream::devicetoken::DeviceTokenProvider::new(
                &cfg.device_token_url,
                &cfg.device_token_token,
            )
            .map(std::sync::Arc::new),
            cfg,
        })
    }

    /// 给**国际版**请求带上 `X-Device-Token`（对应 Go `attachDeviceToken`）。
    ///
    /// # 失败必须静默降级（重要设计决策）
    ///
    /// 拿不到 token 时**不发该头**，请求照常发出。理由：
    ///
    /// - 该头是「锦上添花」（防未来风控标记），**不是准入条件** ——
    ///   实测不带它，健康账号照样 200。
    /// - 若因拿不到就拒绝请求，等于把一个**可选增强**变成**新的单点故障**：
    ///   宿主服务没起来 / node 缺失 / 用户没装官方客户端，都会让
    ///   所有国际版对话不可用。那是拿可用性换完整性，方向错。
    ///
    /// 故这里只记日志（便于排查「为什么没带上」），**绝不上抛错误**。
    ///
    /// # 为什么只给国际版
    ///
    /// 抓包证据只覆盖国际版（`workbuddy.ai`）—— 官方国服客户端走的是另一套
    /// （`codebuddy.cn` / `copilot.tencent.com`），我们没有它的证据，不该凭
    /// 「国际版有」就推断「国服也要」。多发给国服一个它不认的头，风险大于收益
    /// （国服当前实测良好，不动它）。
    ///
    /// 返回 `Some(token)` 供调用方挂到请求上；`None` = 本次不发。
    async fn attach_device_token(&self, a: &Auth) -> Option<String> {
        if !a.is_intl() {
            return None;
        }
        let provider = self.device_token.as_ref()?;
        match provider.fetch(&self.http).await {
            Ok(tok) => Some(tok),
            Err(e) => {
                // 降级：不发该头。日志保留原因，否则「为什么这次没带」无从查起。
                eprintln!(
                    "device_token uid={}: 获取失败（本次不发该头，请求照常）: {e}",
                    a.uid
                );
                None
            }
        }
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

    /// 该账号的官网（成长中心）基址。
    ///
    /// 国服与国际版都是各自站点的主域：国服 `www.workbuddy.cn`、
    /// 国际版 `www.workbuddy.ai`。与 [`Client::billing_base`] 的区别在于
    /// billing 在国服走 `codebuddy.cn`（计费后台），而成长中心是面向用户的
    /// Web 站点 —— 两者不可互相替代。对应 Go `webBase`（`client.go:1538`）。
    ///
    /// 用在两处：领奖（`/activity/growth/tasks/{code}/claim`）与 Web 指纹上报
    /// （`/v2/report`）—— 它们的 Origin/Referer 都必须指向本域，
    /// 否则被上游按「非 Web 来源」拒绝。
    ///
    /// ★ **有意偏离 Go 的一处**（`client.go:1538-1549`）：Go 的国际版分支是
    /// `WebBaseIntl != "" ? WebBaseIntl : "https://www.workbuddy.ai"`，**不看**
    /// `BaseIntl`（两个独立字段）。这里在国际版上多了一层 `base_intl` 回落，
    /// 顺序为 `web_base_intl` → `base_intl` → [`BASE_INTL`]。
    ///
    /// 偏离的理由：本仓的 `base_intl` 是**唯一的国际版基址覆盖入口**
    /// （`chat_base` / `billing_base` 都读它），而 A/B 对照测试正是靠它把网关
    /// 指向本机假上游。若这里不回落，同一场测试里 Web 域的请求会**漏到真实域名**
    /// —— 那不是「多打了一次上游」，而是往真实账号上写了真实数据。
    /// 生产路径 `base_intl` 恒为空（只有 `WB2A_BASE_INTL` 会填它），
    /// 故实际行为与 Go 逐字相同：`https://www.workbuddy.ai`。
    pub fn web_base(&self, a: &Auth) -> &str {
        if a.is_intl() {
            if !self.cfg.web_base_intl.is_empty() {
                return &self.cfg.web_base_intl;
            }
            if !self.cfg.base_intl.is_empty() {
                return &self.cfg.base_intl;
            }
            BASE_INTL
        } else if !self.cfg.web_base_cn.is_empty() {
            &self.cfg.web_base_cn
        } else {
            WEB_BASE_CN
        }
    }

    /// 短 RPC 的 reqwest 客户端（有总时长上限）。
    ///
    /// # 为什么需要这个口子
    ///
    /// 本结构体的字段是**私有**的，而 Rust 的私有性只在「定义它的模块及其
    /// **后代**」可见。`upstream::{tasks, desktop}` 是 `upstream::client` 的
    /// **兄弟**模块，不是后代 ⇒ 它们写 `impl Client` 也拿不到 `self.http`
    ///（`error[E0616]: field 'http' of struct 'Client' is private`）。
    ///
    /// 而 `upstream::tasks` / `upstream::desktop` 里有若干方法需要
    /// **非标准头族 + 非 growth/billing 基址**的请求（Web 域的领奖、
    /// 桌面指纹上报、`chatBase` 域的专家列表），四条 `*_json` 管道覆盖不到，
    /// 又必须复用 [`Client::do_json`] 的信封语义。故开这两个 `pub(crate)` 访问器。
    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// 聊天 SSE 的 reqwest 客户端（**无**总时长上限，只有流中空闲上限）。
    ///
    /// 见 [`Client::http`] 的说明。`ChatWithModel` 需要它：那是 SSE 流式请求，
    /// 用短 RPC 客户端会被总超时掐断。
    pub(crate) fn chat_http(&self) -> &reqwest::Client {
        &self.chat
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
        let mut req = self
            .chat
            .post(&url)
            .headers(headers::chat_headers(a))
            .body(prepared);
        // 设备 token：只对**国际版**发（对应 Go `c.attachDeviceToken(req, a)`）。
        // 拿不到就静默跳过，请求照常发出 —— 见本函数上方那段说明。
        if let Some(tok) = self.attach_device_token(a).await {
            req = req.header("X-Device-Token", tok);
        }
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

    /// 每日签到（对应 Go `client.go:2279 DailyCheckin`）。
    ///
    /// 国服专属：国际版没有签到端点。走 **billing** 域（`billingBase(a)`，
    /// 国服是 `www.codebuddy.cn`），不是 chat 域 —— 与
    /// [`Client::user_resource_detail`] 同一基址，签到后紧接着查余额正是同一个后台。
    ///
    /// 请求体是空对象 `{}` 而非无体：上游按 `Content-Type: application/json`
    /// 解析，缺体会被当成格式错误。
    ///
    /// **不在此处判定业务错误**：已签到、活动未开始等都会以业务码返回，
    /// 而调用方（`scheduler`）刻意把它们当作「继续走余额查询」的信号 ——
    /// 签到失败但余额已恢复的账号同样应该解冻。
    pub async fn daily_checkin(&self, a: &Auth) -> Result<()> {
        self.billing_json(
            a,
            reqwest::Method::POST,
            "/v2/billing/meter/daily-checkin",
            Some(&serde_json::json!({})),
        )
        .await
        .map(|_| ())
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

    /// 短信登录第一步：给手机号发验证码。
    ///
    /// 返回验证码有效期（秒）。上游不给或给非正数时回落 300（与界面提示一致）。
    ///
    /// ★ `base` 由调用方给（`https://www.workbuddy.cn` / `https://www.workbuddy.ai`）：
    /// 这里**没有账号对象**可以判区域 —— 那正是本流程的产物（「先有鸡还是先有蛋」）。
    ///
    /// ★ 用 `self.http`（短 RPC，默认 120s 总时长）而不是自建 30s 客户端：与 Go
    /// `doLoginJSON` 一致 —— 它在 `c.HTTP == nil` 时才兜底 30s，而生产路径上
    /// `HTTP` 始终存在。
    pub async fn send_sms(&self, base: &str, phone: &str) -> Result<i64> {
        let url = format!("{}{}", base, SMS_SEND_PATH);
        let body = serde_json::json!({ "phone": phone });
        let data = self.post_login_json(&url, &body).await?;
        // 上游字段是 snake_case `expires_in`。取不到就按 300 秒展示 ——
        // 这只是给界面的一句提示，不该因为上游少一个可选字段就让整个流程失败。
        let secs = data.get("expires_in").and_then(|v| v.as_i64()).unwrap_or(0);
        Ok(if secs > 0 { secs } else { 300 })
    }

    /// 短信登录第二步：用验证码换 token。
    ///
    /// ★ 两个 token 缺一即报错（Go `LoginWithSMS` 同口径）：只存 accessToken 会在
    /// 一小时后静默失效，而用户刚「成功登录」过，会以为是自己操作错了。
    pub async fn login_with_sms(&self, base: &str, phone: &str, code: &str) -> Result<SmsTokens> {
        let url = format!("{}{}", base, SMS_LOGIN_PATH);
        // ★ 字段名逐字：`login_method` / `sms_code` 是**上游**的约定（snake_case），
        // 与我们自己对外暴露的驼峰 `smsCode` 无关，两者不能混。
        let body = serde_json::json!({
            "login_method": "phone",
            "phone": phone,
            "sms_code": code,
        });
        let data = self.post_login_json(&url, &body).await?;
        let at = data
            .get("accessToken")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let rt = data
            .get("refreshToken")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if at.is_empty() || rt.is_empty() {
            return Err(GatewayError::UpstreamParse(format!(
                "登录成功但响应缺少 accessToken/refreshToken（AT={} 字节，RT={} 字节）",
                at.len(),
                rt.len()
            )));
        }
        Ok(SmsTokens {
            access_token: at,
            refresh_token: rt,
        })
    }

    /// 发一个登录请求并解出 `data`。
    ///
    /// ⚠ 与 [`Client::fetch_models`] 之类的账号级调用**分开**：那些需要一个 `Auth`
    /// 来选区域基址，而登录时还没有账号对象。硬套会把「先有鸡还是先有蛋」塞进签名。
    ///
    /// ★★ **业务码优先于 HTTP 状态码**：上游把「手机号格式错」「验证码错」都放在
    /// `code` 里，而 HTTP 状态可能是 200 也可能是 **500**（实测 send-sms 对无效号码
    /// 回 500）。只看 HTTP 状态会把「验证码错了」报成「服务器异常」，把用户引向
    /// 完全错误的方向（去查网络）。
    async fn post_login_json(&self, url: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let req = self
            .http
            .post(url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            // ★ 必须伪装成官方桌面客户端（见 `SMS_UA` 的说明），否则上游直接拒。
            .header("User-Agent", SMS_UA)
            .json(body);
        let resp = req.send().await.map_err(|e| {
            GatewayError::Transport(format!("请求失败: {e}"))
        })?;
        let status = resp.status();
        let raw = read_capped(resp).await;

        // 连信封都解不出来（如 HTML 错误页）时把原始内容带上 —— 否则用户只看到
        // 「解析失败」，无从判断是网络、风控还是上游改版。
        let v: serde_json::Value = match serde_json::from_slice(raw.as_bytes()) {
            Ok(v) => v,
            Err(_) => {
                return Err(GatewayError::UpstreamParse(format!(
                    "响应解析失败（HTTP {}）：{}",
                    status.as_u16(),
                    crate::upstream::classify::truncate(&raw, 200)
                )))
            }
        };
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("");
        if code != 0 {
            return Err(GatewayError::UpstreamParse(biz_message(code, msg)));
        }
        if !status.is_success() {
            return Err(GatewayError::UpstreamParse(format!(
                "上游返回 HTTP {}：{}",
                status.as_u16(),
                crate::upstream::classify::truncate(&raw, 200)
            )));
        }
        Ok(v.get("data").cloned().unwrap_or(serde_json::Value::Null))
    }
}

/// 短信登录拿到的令牌对。
#[derive(Debug, Clone, Default)]
pub struct SmsTokens {
    /// 访问令牌（后续用它解 uid、写凭证文件）。
    pub access_token: String,
    /// 刷新令牌。
    pub refresh_token: String,
}

/// 短信登录接口路径（相对区域 base）。
const SMS_SEND_PATH: &str = "/v2/plugin/login/send-sms";
const SMS_LOGIN_PATH: &str = "/v2/plugin/login/token";

/// 短信登录用的 User-Agent。
///
/// ★ 必须**逐字**伪装成官方桌面客户端：上游按 UA 分流，非官方 UA 会被风控拒绝
/// 或落到另一套（可能不存在的）登录流程上。Go `internal/upstream/smslogin.go:44-51`
/// 用的是 WorkBuddy 桌面端（Electron）的完整 UA 串。
const SMS_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) WorkBuddy/5.5.4 Chrome/138.0.7204.251 Electron/37.10.3 Safari/537.36";

/// 把上游业务码翻成人话。
///
/// 为什么需要：上游的 `msg` 形如 `10000:keycloak SPI returned status=400:` ——
/// 直接给用户看既看不懂、也指向不了正确动作。已知码给人话，**未知码原样带上**
///（不能吞掉，否则无从排查）。
fn biz_message(code: i64, msg: &str) -> String {
    match code {
        // 实测常见于手机号格式不对 / 号码未注册 / 风控拦下
        10000 => format!(
            "手机号无效或发送过于频繁（上游返回「{}」）；请确认号码无误，若刚发过请稍等一分钟再试",
            crate::upstream::classify::truncate(msg, 120)
        ),
        _ => format!(
            "上游返回错误码 {code}：{}",
            crate::upstream::classify::truncate(msg, 160)
        ),
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
///
/// ★ 刻意**不**派生 `Eq`：`credit_multiplier: Option<f64>` 里是浮点，
/// 而 `f64` 没有 `Eq`（只有 `PartialEq`）。去掉 `Eq` 不会损失任何既有能力 ——
/// 本类型的比较只用于测试与去重，`PartialEq` 已足够。
#[derive(Debug, Clone, Default, PartialEq)]
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
    /// 上游给的**默认**思考档（空 = 未声明）。
    ///
    /// 与 [`ModelInfo::efforts`] 分开：`efforts` 是「允许哪些档」，
    /// `default_effort` 是「不指定时用哪档」。上游同时给了两者，但默认档未必在
    /// `supported_efforts` 里（上游数据未保证），因此**不要**用它去推导
    /// `efforts`，也不要拿 `efforts[0]` 去冒充它。
    ///
    /// # ⚠ 来源有**两个写法**，必须都读（2026-09-18 实测）
    ///
    /// ```text
    /// reasoning.defaultEffort  ← 12（国服）/ 12（国际版）
    /// reasoning.effort         ← 18（国服）/  8（国际版）
    /// ```
    ///
    /// 两者**互斥**：实测 0 个模型同时带这两个键。带 `supportedEfforts` 的模型
    /// 一律用 `defaultEffort`，不带的用 `effort`。所以它们不是两种含义，而是
    /// 同一个东西的两种拼写（推测随上游版本演进）。
    ///
    /// ★ 早先只读了 `effort`，于是那 12 个多档模型的默认档永远读不到 ——
    /// 界面只能列出档位，说不出「默认是哪一档」。
    pub default_effort: String,
    /// 是否接受图片输入。
    ///
    /// `None` **不等于** `Some(false)`：上游只给对话模型写 `supportsImages`，
    /// 补全/图片生成等条目整条缺失该字段。缺失时是「未声明」，不是「不支持」——
    /// 谎报成纯文本会让客户端把本可用的图片能力关掉。
    pub supports_images: Option<bool>,

    /// 上游声明的「该模型是否有思考能力」= `supportsReasoning`。
    ///
    /// `None` = **未声明**（不是 `false`）。上游只给对话模型写这个字段，
    /// 补全/图片生成等条目整条缺失。三态语义与 [`ModelInfo::supports_images`]
    /// 同理：把它当成 `false` 会让客户端把本可用的思考能力关掉。
    ///
    /// ★ 早先只按 `efforts` 是否为空判断「支不支持思考」，于是「只有固定档、
    /// 没有档位数组」的模型被误判成不支持 —— 18 个（国服）/ 8 个（国际版）
    /// 模型因此在界面上显示成「—」。
    pub supports_reasoning: Option<bool>,
    /// 该模型是否**只能**以思考模式运行（`onlyReasoning`）。
    ///
    /// 实测：凡带 `reasoning` 对象的模型都是 `true`。它解释了
    /// [`ModelInfo::can_disable_thinking`] 为 `false` 的模型为何不能传 `off`
    /// —— 即「思考不可关闭」。
    pub only_reasoning: Option<bool>,
    /// 是否允许关闭思考 = `reasoning.canDisableThinking`。
    ///
    /// `None` = 未声明。实测 12 个多档模型都显式给了它（`false` 或 `true`），
    /// 而只有默认档的模型整条缺失 —— 与「它们能否传 `off`」正好对应。
    /// 为 `false` 时客户端**不能传 `off`**，应据此置灰。
    pub can_disable_thinking: Option<bool>,

    /// 该模型的**计费倍率**，来自上游 `/v3/config` 的 `credits` 字段。
    ///
    /// 这是「这个模型免不免费」的**上游权威依据**，不是本地清单：上游对每个
    /// 模型给出倍率字符串，倍率 `0` 即不扣积分。所有者报的「国际版
    /// `deepseek-v4.1-flash` 免费却统计出积分消耗」正是本字段此前被静默丢弃
    /// 导致的 —— `/v3/config` 的解析用的是**手写匿名结构体**，没写进结构体的
    /// 字段会被 json 包直接忽略，所以读代码看不出上游给过它。
    ///
    /// **必须三态**（与 [`ModelInfo::supports_images`] 同一约定，理由也相同）：
    ///
    /// ```text
    /// None            = 上游未声明该字段 → 「不知道」，**不能当作免费**
    /// Some(0.0)       = 明确免费
    /// Some(> 0.0)     = 按该倍率计费
    /// ```
    ///
    /// 把「未声明」当免费是**反方向**的错（把该计费的显示成免费），与本缺陷
    /// 同样属于「统计与实际不符」，因此这里绝不退化成 `bool`。
    ///
    /// 实测（2026-09-18 直接拉两区 `/v3/config` 的真实响应）：
    ///
    /// ```text
    /// 国际版 22 个模型**全部**带该字段，其中倍率 0 的有 4 个：
    ///     default-model ""            deepseek-v4.1-flash "x0.00"
    ///     hy4-preview-f "x0.00"       hy3                 "x0.00"
    ///   计费样例：fast-model "x0.34 credits"、primary-model "x3.31 credits"
    /// 国服 52 个模型里**只有 33 个**带该字段，倍率 0 的只有 1 个（hy3 "x0.00"），
    ///   其余 19 个（auto / deepseek-r1-0528-* / codewise-* 等）整条缺失该字段。
    /// ```
    ///
    /// 同一个模型名在两区结论可能**相反**，这是本字段不能做成全局清单的原因：
    ///
    /// ```text
    /// deepseek-v4.1-flash   国服 "x0.03"（计费）  国际版 "x0.00"（免费）
    /// ```
    ///
    /// 故判定免费必须**同时**看模型 ID 与账号所属区域。
    pub credit_multiplier: Option<f64>,
    /// 上游 `credits` 字段的**原文**（仅诊断用，不参与判定）。
    ///
    /// 为什么把原文也留着：[`ModelInfo::credit_multiplier`] 是解析结果，
    /// `"x0.34 credits"` 与 `"x0.34"` 解析后同为 `0.34`，看不出上游到底怎么写的。
    /// 一旦出现「倍率与实际扣费对不上」，第一个要问的就是「上游原文是什么」——
    /// 把它一并透出，排查时不必再手工拉一次上游。
    ///
    /// 字段缺失时为 `""`（与「有字段且值为空串」同形）：解析结果仍由
    /// `credit_multiplier` 的三态区分，本字段只供人看。
    pub credits_raw: String,
}

/// 从上游 `credits` 字段里抓倍率数字。
///
/// 只认十进制（含小数、可带负号）；不认指数形式 —— 上游给的是 `"x0.34"` 这种
/// 展示串，出现科学计数法说明字段语义已经变了，那时应当人工复核而不是静默解析。
///
/// ★ **负号必须一起抓**：漏掉它时 `"x-1.5"` 会匹配出 `"1.5"` 并被当成正常倍率，
/// 于是「上游语义已变」被静默读成「计费 1.5」。带上负号才能让下面的
/// `value < 0` 判定生效，把这类值按未声明处理。
fn credit_multiplier_pattern() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"-?\d+(?:\.\d+)?").expect("倍率正则固定合法"))
}

/// 解析上游 `/v3/config` 的 `credits` 计费倍率字符串。
///
/// 返回 `(倍率, 是否已声明)`：`declared == false` 表示**上游没给这个字段**，
/// 是「不知道」而不是「免费」—— 调用方必须把两者分开表达（界面上「—」与「0」
/// 语义不同，「—」表示不知道，`0` 是确定值）。
///
/// 实测出现过 4 种形态，都必须吃下（2026-09-18 两区真实响应）：
///
/// ```text
/// ""              空串        → 0（无倍率即不计费）
/// "x0.00"         零倍率      → 0（免费）
/// "x0.34"         纯倍率      → 0.34
/// "x0.34 credits" 带单位后缀  → 0.34（**抓数字，不能整串匹配**）
/// ```
///
/// 为什么用正则抓第一个数字而不是 `trim_start_matches('x')` + `parse::<f64>()`：
/// 带单位后缀那种形态下 `"0.34 credits".parse()` 会失败，而失败会被上层当成
/// 「未声明」→ 计费模型被显示成未知。整串匹配更糟：上游换个后缀
/// （credits / 积分 / 空）就会静默失配。抓数字对这些变体都成立。
///
/// 解析不出数字（如上游改成 `"free"` / `"N/A"` 这类纯文字）时返回
/// `declared = false`：**宁可说不知道，也不要猜**。猜「free」= 把计费的说成
/// 免费，猜「收费」= 把免费的说成计费，两个方向都直接造成本缺陷同类的问题。
fn parse_credit_multiplier(raw: &str) -> (f64, bool) {
    let text = raw.trim();
    if text.is_empty() {
        // 有字段且值为空串 → 上游明确表达「无倍率即不计费」。
        return (0.0, true);
    }
    let Some(m) = credit_multiplier_pattern().find(text) else {
        return (0.0, false);
    };
    let Ok(value) = m.as_str().parse::<f64>() else {
        return (0.0, false);
    };
    if !value.is_finite() {
        return (0.0, false);
    }
    // 负倍率上游从未出现过，出现即说明语义变了（可能是「返还」之类）：
    // 不猜语义，按未声明处理，让上层显示「不知道」而不是把它当免费。
    if value < 0.0 {
        return (0.0, false);
    }
    (value, true)
}

/// 把上游 `credits` 字段的可选原始串折成三态倍率。
///
/// 为什么要单独一层而不是直接调 [`parse_credit_multiplier`]：
///
/// ```text
/// None（字段缺失） → 未声明
/// Some(s)          → 交给 parse_credit_multiplier（其中 "" → 0 = 免费）
/// ```
///
/// 两者结果都是 `None`，但**语义不同**（一个是上游没说，一个是说了但说得无法
/// 解析）。当前对消费方而言两者都等于「不知道」，故合并成同一个返回值；
/// 若将来需要区分（例如排查上游字段变更），在这里分流即可。
fn credit_multiplier_of(raw: Option<&str>) -> Option<f64> {
    let raw = raw?;
    let (value, declared) = parse_credit_multiplier(raw);
    if !declared {
        return None;
    }
    Some(value)
}

/// 返回第一个**非空白**字符串（都空则空串）。
///
/// 用途：上游把同一个语义写成两个键（如 `reasoning.defaultEffort` 与
/// `reasoning.effort`），实测互斥但写入顺序无保证；这里给出确定性的取值顺序。
fn first_non_empty(values: &[&str]) -> String {
    for v in values {
        if !v.trim().is_empty() {
            return (*v).to_string();
        }
    }
    String::new()
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
                //
                // ★ 两个写法都读，`defaultEffort` 优先：实测二者**互斥**
                //（0 个模型同时有），所以「优先」只是防御上游将来同时下发时的
                // 确定性选择，不代表它们会冲突。只读 `effort` 会让那 12 个
                // 多档模型的默认档永远读不到。
                default_effort: {
                    let r = m.get("reasoning");
                    let de = r
                        .and_then(|r| r.get("defaultEffort"))
                        .and_then(|e| e.as_str())
                        .unwrap_or("");
                    let ef = r
                        .and_then(|r| r.get("effort"))
                        .and_then(|e| e.as_str())
                        .unwrap_or("");
                    first_non_empty(&[de, ef])
                },
                // 账号级多模态开关优先：上游用它表达「该账号不能发图片」，
                // 与模型自身能力无关，此时宣称支持会让客户端发出必然失败的请求。
                supports_images: if disabled_multimodal { Some(false) } else { supports },
                // 思考能力三态：透传上游声明，缺失保持 None（未声明 ≠ 不支持）。
                //
                // ⚠ `supportsReasoning` / `onlyReasoning` 是 `reasoning` 对象
                // **之外**的兄弟字段（早期误以为在 reasoning 里）。
                supports_reasoning: m.get("supportsReasoning").and_then(|v| v.as_bool()),
                only_reasoning: m.get("onlyReasoning").and_then(|v| v.as_bool()),
                can_disable_thinking: m
                    .get("reasoning")
                    .and_then(|r| r.get("canDisableThinking"))
                    .and_then(|v| v.as_bool()),
                // 计费倍率：字段缺失 → 保持 None（未声明）；有值但解析不出数字 →
                // 同样保持 None（见 parse_credit_multiplier 的「宁可说不知道」）。
                //
                // ⚠ 必须用 `Option<&str>` 区分「字段缺失」与「字段是空串」——
                // 实测国服 52 个模型里 19 个整条没有该字段，而国际版
                // default-model 则是**有字段且值为空串**：
                //
                //   字段缺失 → 不知道（不能当免费）
                //   ""      → 免费
                //
                // 用非 Option 的 &str 会把那 19 个模型全部判成免费 ——
                // 正是「把该计费的算成免费」。
                credit_multiplier: credit_multiplier_of(
                    m.get("credits").and_then(|v| v.as_str()),
                ),
                credits_raw: m
                    .get("credits")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
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

// ──────────────────────── growth / billing 域公共请求管道 ────────────────────────
//
// 对应 Go `travel.go` 的 `growthJSON*` 与 `report.go` 的 `billingJSON`：
// 四个变体只差「基址」与「User-Agent / 额外头」，信封解析完全共用 `doJSON`。
//
// ★ 为什么放在 `client.rs` 而不是新文件：`Client` 的 `http` / `chat` / `cfg`
// 都是**私有字段**，私有可见性只覆盖定义它的模块及其子模块 —— 兄弟模块
//（`upstream::growth` 等）读不到。把管道留在这里，各域文件就只需
// `impl Client` 调用这些 `pub` 方法。

/// Go 的 `net/http` 在请求未显式设置 User-Agent 时会**自动补**上这个值。
///
/// ★ 必须显式补回：reqwest **不会**自动加 UA（实测未调 `.user_agent()` 的
/// `reqwest::Client` 发出的请求上不带该头）。若不补，Rust 发出的 growth /
/// billing 请求就不带该头，上游看到的客户端指纹与线上长期验证过的 Go 版不同 ——
/// 而 Go `growthJSON` 的注释明确写着「全局去掉 UA 会改变它们的指纹，
/// 属无谓的改动面扩大」。故按原口径补回同一个值。
const GO_HTTP_UA: &str = "Go-http-client/1.1";

/// 小程序口径的 `X-Client-Platform` 取值（对应 Go `mpPlatform`）。
///
/// ⚠ **不要**与事件上报的头族混用：`report.rs` 的小程序上报走 report 域，
/// 那里的取值是 `mp-weixin`。两个值分属 growth 域任务接口与 report 域事件通道，
/// 互换任一方向都会让上游按另一种来源解析（表现为 200 但不计分 / 任务 not found）。
pub const MP_PLATFORM: &str = "miniprogram";

impl Client {
    /// 发一次域请求并解上游统一信封（对应 Go `doJSON`）。
    ///
    /// 错误语义与 Go 完全一致：HTTP 非 2xx 或业务 `code != 0` → `UpstreamError`。
    /// 成功时返回 `data` 段的序列化字节（`data` 缺失时为空 `Vec`）。
    ///
    /// ★ `pub(crate)` 而非私有：兄弟模块 `upstream::{tasks, desktop}` 需要复用它
    /// （它们的请求是自定义头族 + 自定义基址，四条 `*_json` 管道覆盖不到），
    /// 而在兄弟模块里重抄一遍信封语义会埋下「两处口径漂移」的隐患。
    /// 可见性说明见 [`Client::http`]。
    pub(crate) async fn do_json(&self, req: reqwest::RequestBuilder) -> Result<Vec<u8>> {
        let resp = req
            .timeout(self.cfg.timeout)
            .send()
            .await
            .map_err(transport_err)?;
        let status = resp.status();
        let raw = read_capped(resp).await;
        unwrap_envelope(status, raw.as_bytes())
    }

    /// 组装一次域请求（不发送）。
    ///
    /// `base` 决定域：growth 用 [`Client::chat_base`]，billing 用
    /// [`Client::billing_base`]（国服两者不同域，国际版同域）。
    /// `body` 为 `None` 时不带请求体（与 Go `body == nil` 同口径）。
    fn domain_req(
        &self,
        a: &Auth,
        base: &str,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
        ua: Option<&'static str>,
        extra: &[(&'static str, &'static str)],
    ) -> reqwest::RequestBuilder {
        let url = format!("{base}{path}");
        let mut req = self
            .http
            .request(method, &url)
            .headers(headers::billing_headers(a));
        if let Some(ua) = ua {
            req = req.header(reqwest::header::USER_AGENT, ua);
        }
        for (k, v) in extra {
            req = req.header(*k, *v);
        }
        match body {
            // `serde_json::Value` 的序列化不会失败，`unwrap_or_default` 只是
            // 满足类型要求；真出现空体与 Go 的 marshal 失败等价（都发不出去）。
            Some(v) => req.body(serde_json::to_vec(v).unwrap_or_default()),
            None => req,
        }
    }

    /// growth 域请求：`chatBase(a) + path` + BillingHeaders + 统一信封。
    pub async fn growth_json(
        &self,
        a: &Auth,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Vec<u8>> {
        let req = self.domain_req(
            a,
            self.chat_base(a),
            method,
            path,
            body,
            Some(GO_HTTP_UA),
            &[],
        );
        self.do_json(req).await
    }

    /// 同 [`Client::growth_json`]，但**不发 User-Agent**。
    ///
    /// 用途只有一个：任务列表。上游按 User-Agent 判定「请求来自哪个客户端平台」
    /// 并据此裁剪任务清单 —— 实测（2026-09-18，真实账号逐 UA 对照）：
    /// `Go-http-client/1.1` 与 WorkBuddy CLI UA 都只给 18 项（缺两个小程序限定任务），
    /// 小程序 UA 只给 10 项，**不发 UA 才拿到完整 20 项**。
    ///
    /// 服务端对「未声明平台」的请求返回的是**全量**清单，反而是声明了平台才会被裁剪。
    /// 只对列表用：其它 growth 端点（签到、旅行、领奖）没有这个行为。
    ///
    /// ★ 与 Go 的等价性：Go 侧写的是 `Set("User-Agent", "")`，但 Go 的
    /// `Request.write` 在 UA 为空串时**不写该头** —— 也就是说 Go 的「no-UA」
    /// 在 wire 上就是「没有 User-Agent」。Rust 这里直接不发，行为一致。
    pub async fn growth_json_no_ua(
        &self,
        a: &Auth,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Vec<u8>> {
        let req = self.domain_req(a, self.chat_base(a), method, path, body, None, &[]);
        self.do_json(req).await
    }

    /// 小程序口径的 growth 域请求：比 [`Client::growth_json`] 多一个
    /// `X-Client-Platform: miniprogram`。
    ///
    /// 为什么必须有它：小程序限定任务（`school_season`、`Sequential_Tasks_1/_2/_3`）
    /// 的**报名**缺这个头时上游返回 HTTP 200 + code=0，但逐条结果是
    /// `{"status":"error","message":"task not found"}` —— 200 的壳里装着
    /// 「任务不存在」，最容易误判成「任务已下线」而放弃。
    ///
    /// 刻意**不**让批量报名自动补头：批次里通常混着普通任务，给整批加 mp 头会把
    /// 普通任务也按小程序口径处理（实测 mp 列表是默认口径的**子集**，不是超集）。
    pub async fn growth_json_mp(
        &self,
        a: &Auth,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Vec<u8>> {
        let req = self.domain_req(
            a,
            self.chat_base(a),
            method,
            path,
            body,
            Some(GO_HTTP_UA),
            &[("X-Client-Platform", MP_PLATFORM)],
        );
        self.do_json(req).await
    }

    /// billing 域请求：`billingBase(a) + path` + BillingHeaders + 统一信封。
    ///
    /// 与 [`Client::growth_json`] 的唯一差别是基址（国服 billing 与 chat 不同域）。
    /// 签到、礼包、补偿、活跃上报、开学季活动都走这里。
    pub async fn billing_json(
        &self,
        a: &Auth,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Vec<u8>> {
        let req = self.domain_req(
            a,
            self.billing_base(a),
            method,
            path,
            body,
            Some(GO_HTTP_UA),
            &[],
        );
        self.do_json(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// ★★ 回归：未配置代理时必须**显式直连**，不得回落 reqwest 的「系统代理」。
    ///
    /// # 缺陷形状：reqwest 会读 **Windows 注册表**，Go 不会
    ///
    /// reqwest 的 `ClientBuilder` 默认 `auto_sys_proxy = true`，`build()` 时会
    /// `proxies.push(ProxyMatcher::system())`（`async_impl/client.rs:419-421`）。
    /// 注意 `Matcher::system()` **没有 `cfg(feature = "system-proxy")` 门** ——
    /// 它经由 `hyper-util` 的 `client-proxy-system` feature 解析系统代理，
    /// 在 Windows 上读的是注册表 `HKCU\...\Internet Settings\ProxyEnable/ProxyServer`。
    ///
    /// Go 的 `http.ProxyFromEnvironment` **只读环境变量、不读注册表** ⇒
    /// 「没配代理」在 Go 是真直连，在 reqwest 是**偷偷走系统代理**。
    ///
    /// # 为什么 `-p` 跑绿、`--workspace` 跑红（本轮排查的关键）
    ///
    /// `hyper-util` 在整个 workspace 里只有**一个版本**（0.1.20），而
    /// `src-tauri` 的 `tauri-plugin-updater` 会拉 reqwest 0.13.4 → 打开
    /// `system-proxy` → 即 `hyper-util/client-proxy-system`。
    /// Cargo 的 feature 统一是**按包版本**做的 ⇒ 我们自己的 reqwest 0.12.28
    /// 在 `--workspace` 构建里**也**拿到了系统代理能力。实测：
    ///
    /// ```text
    /// cargo tree -p ai-gateway-router  -e features   → 无 client-proxy-system
    /// cargo tree --workspace           -e features   → 有 client-proxy-system
    /// ```
    ///
    /// ⚠ 这**不是**测试环境问题：`cargo build -p ai-gateway`（Tauri 宿主）
    /// 同样带上了它。只有 `--bin gateway`（发行的那份）不受影响。
    ///
    /// # 判据为什么是「假上游看不到 User-Agent」
    ///
    /// 本机所有者开着系统代理（`127.0.0.1:7890`，mihomo）。它**能**转发到
    /// 本机假上游，所以「请求是否到达」**区分不出**走没走代理 —— 我第一版
    /// 就是只断言了到达，结果把修复还原后测试照样绿（假绿）。
    ///
    /// 真正的判据是**代理会改请求**：mihomo 是 Go 程序，会给转发出去的请求
    /// 补上自己的 `User-Agent: Go-http-client/1.1`。因此用
    /// [`Client::growth_json_no_ua`]（本就**故意不发 UA** 的那条路径）打假上游，
    /// 再看假上游收到什么：直连 ⇒ 没有 UA 头；走代理 ⇒ 被补上 UA。
    ///
    /// 这与线上那次真实失败是**同一条断言**
    ///（`list_growth_tasks_uses_no_ua_and_parses_both_shapes`）。
    #[tokio::test]
    async fn unconfigured_proxy_means_direct_connection_not_system_proxy() {
        use std::io::{Read, Write};

        // 假上游：记录**原始请求头**，回一个最小 JSON。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 8192];
                let n = s.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                sink.lock().unwrap().push(head);
                let body = r#"{"code":0,"data":{"tasks":[]}}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
                let _ = s.flush();
            }
        });

        // `proxy` 留空 ⇒ 走 `build_proxy` 的 `None` 分支（本测试的核心）。
        let client = Client::new(ClientConfig {
            chat_base_cn: base.clone(),
            billing_base_cn: base.clone(),
            web_base_cn: base.clone(),
            proxy: String::new(),
            timeout: std::time::Duration::from_secs(10),
            ..Default::default()
        })
        .unwrap();

        let a = Auth {
            access_token: "at".into(),
            uid: "u".into(),
            enterprise_id: "e".into(),
            ..Default::default()
        };
        let out = client.list_growth_tasks(&a).await;
        assert!(
            out.is_ok(),
            "未配置代理时应能直连本机假上游，实际 {out:?}"
        );

        let got = seen.lock().unwrap();
        assert_eq!(got.len(), 1, "应恰好一次请求");
        let lower = got[0].to_ascii_lowercase();
        assert!(
            !lower.contains("user-agent:"),
            "★ 假上游收到了 User-Agent ⇒ 请求被交给了系统代理（代理补了自己的 UA）。\n\
             未配置代理时必须 `.no_proxy()` 直连。实际请求头：\n{}",
            got[0]
        );
    }

    /// 显式配置代理时，`.proxy()` 分支同样不得叠加系统代理。
    ///
    /// 与上一条互补：reqwest 的 `ClientBuilder::proxy()` 自身会把
    /// `auto_sys_proxy` 置 false（`async_impl/client.rs:1416`），故这一支本就安全 ——
    /// 这条测试把它**钉住**，防止将来有人把 `match` 改回 `if let Some`。
    #[tokio::test]
    async fn explicit_proxy_does_not_also_enable_system_proxy() {
        // 指向一个死端口：请求必定失败，但**必须**是传输层错误
        //（说明确实尝试了那个代理），而不是被系统代理截走。
        let client = Client::new(ClientConfig {
            chat_base_cn: "http://127.0.0.1:1".into(),
            proxy: "http://127.0.0.1:1".into(),
            timeout: std::time::Duration::from_secs(2),
            ..Default::default()
        })
        .unwrap();
        let a = Auth {
            access_token: "at".into(),
            uid: "u".into(),
            enterprise_id: "e".into(),
            ..Default::default()
        };
        let err = client.list_growth_tasks(&a).await.unwrap_err();
        assert!(
            matches!(err, GatewayError::Transport(_)),
            "显式代理不可达应为传输层错误，实际 {err:?}"
        );
    }

    #[test]
    fn base_urls_follow_region() {        let c = Client::new(ClientConfig::default()).unwrap();
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
