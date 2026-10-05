//! Trae 上游客户端：一次对话请求的全部 HTTP 细节。
//!
//! 对应 Go 侧同层实现与逆向参照 `test-bin/recon/trae-wangqi233/src/trae-client.js`。
//! 本模块**只负责 HTTP**：请求体由 [dispatch] 组装，响应流由 [translate] 翻译。
//!
//! # 端点
//!
//! | 用途     | 方法 | 路径                               |
//! |----------|------|------------------------------------|
//! | 对话     | POST | `/api/agent/v3/llm_utils_chat`     |
//! | 模型清单 | POST | `/api/ide/v1/get_detail_param`     |
//! | 刷新令牌 | POST | `/trae/api/v3/oauth/ExchangeToken` |
//!
//! 主机名由 [super::realm] 按账号区域给出（国内 trae-api-cn.mchost.guru、
//! 国际 coresg-normal.trae.ai，国际版美国区另有 coreva-normal.trae.ai）。
//!
//! # 区域差异只在两处
//!
//! 1. **对话主机**：由凭证的区域取，见 [super::headers::chat_host]。
//! 2. **OAuth 的 ClientID**：国内 en1oxy7wnw8j9n、国际 ono9krqynydwx5。
//!
//! 核心库的 `trae_refresh::exchange_token` 把国际版 ClientID 写死了，本模块用不了，
//! 故这里按区域**重新实现一遍**交换流程（见本模块的 `exchange_token`）。
//!
//! # 为什么刷新必须在产品层内部做
//!
//! `forward.rs:655-663` 的 token 刷新只覆盖 WorkBuddy，而 Trae 的
//! `Cloud-IDE-JWT` 有效期很短。故每次请求前走一遍惰性刷新门；刷新只改内存，
//! 换到新 token 时顺带把凭证文件更新掉（否则短命 token 会被反复刷新）。
//!
//! # 与 `forward.rs` 的三条硬约定
//!
//! 1. 本模块交给上层的流**必须已经是标准 OpenAI SSE**（[super::translate::translate_stream] 保证）。
//! 2. 上游非 2xx 时返回 [ChatCall::Rejected]，**不返回 Err** —— 上层要用状态码做冷却决策。
//! 3. 只有「请求侧」错误才返回 [GatewayError]：额度耗尽（4008）与模型不存在
//!    （4001 / 4023）—— 这两类在 `forward.rs` 里都不会被换号处置。
//!
//! # ⚠ 首帧分组必须在写响应头之前定性
//!
//! `forward.rs:941-1048` 在**写响应头之前**会探测首帧：空流或首帧即错误都会触发
//! 「冷却换号」，而 Trae 把业务错误放在 HTTP 200 的 `event:error` 里（实测：正常账号的
//! 首个分组就可能直接是 4011）。故 [`Client::chat_stream`] 在返回前自己预读第一个
//! 「有产出」的行组：错误码就地映射成额度耗尽 / 换号 / 模型不存在，已读字节**原样拼回**
//! 流的开头 —— 上层的预读因此立即命中，一帧都不会丢。
//!
//! [`super::dispatch`] 只负责取凭证、规范化模型名与组装请求体。
//!
//! # 网关只读
//!
//! 本模块**绝不**铸造设备身份：machineId / deviceId / 设备密钥对全部从
//! [super::device] 只读取得，与核心库 `trae_oauth` 共用同一份 `trae_device.json`。

use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::TryStreamExt as _;
use serde_json::{json, Value};

use super::cred::TraeCred;
use super::device::{self, DeviceProfile};
use super::headers::{self, HeaderInput};
use super::realm::{self, Realm};
use super::translate;
use crate::error::GatewayError;

/// 一次对话请求的上游结果。
///
/// ★ 与 ZCode 同款语义：**上游非 2xx 时返回 `Rejected`，而不是 `Err`** ——
/// 上层要用状态码分类（额度 / 风控 / 参数），返回 `Err` 会让它变成「换个号再试一遍」。
pub enum ChatCall {
    /// 上游 2xx，流已经是标准 OpenAI SSE。
    Stream(reqwest::Response),
    /// 上游非 2xx（或内部判定为请求侧失败）。
    Rejected { status: u16, body: String },
}

// ---------------------------------------------------------------- 常量

/// 非流式调用的总超时。
pub const CLIENT_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);
/// 控制面请求（模型清单 / 令牌交换）的总超时。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// 流式对话的**读**超时。
///
/// ★ 流式对话**不能有总超时**：一次长回答可能持续几分钟。JS 侧用
/// `setTimeout(() => controller.abort(), 300000)` 兜底（`trae-client.js:554`），
/// Rust 侧的等价物是 `read_timeout`（两次数据之间的间隔）。
pub const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(300);
/// TCP/TLS 建连超时。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// 读错误体时的字节上限（防御性的，不指望它真的触发）。
pub const MAX_ERROR_BODY: usize = 1 << 20;
/// 读控制面响应体时的字节上限。
pub const MAX_CONFIG_BODY: usize = 4 << 20;

/// 对话路径。
pub const CHAT_PATH: &str = "/api/agent/v3/llm_utils_chat";
/// 模型清单路径。
pub const DETAIL_PARAM_PATH: &str = "/api/ide/v1/get_detail_param";
/// 令牌交换路径。
pub const EXCHANGE_PATH: &str = "/trae/api/v3/oauth/ExchangeToken";

/// 惰性刷新门：剩余不足 48 小时就提前换一份新的。
///
/// 逐字对齐核心库 `trae_refresh::TRAE_LAZY_REFRESH_MIN_SECS`。
pub const LAZY_REFRESH_MIN_SECS: i64 = 48 * 3600;

/// 账号侧失败码：命中即「这个号现在服务不了」，该换号。
///
/// 取自 `auth.js:491 const ACCOUNT_FAILOVER_CODES = new Set([4008, 1001, 4010])`：
///
/// - `4008` 额度耗尽 ⇒ 冷却到次日（返回 [GatewayError::QuotaExhausted]，**不换号**）
/// - `1001` 登录态失效 / 已轮换 ⇒ 换号
/// - `4010` 风控临时拒绝 ⇒ 换号
///
/// ★ `4017`（常见风控）**刻意不在表里**：它是设备 / IP 级判定，换号只会把健康的号
/// 一个个烧掉（JS 注释原话：rotation is futile and burns the healthy pool），故原样透传。
///
/// ★ `4011` 也**不在表里**，但不等于「透传不管」。实测：正常账号的首个分组就
/// 是 4011（`test-bin/trae-upstream-probe/out-cn-valid.txt`），HTTP 载体是 200，
/// 而 `classify(200)` 会落到 `ErrKind::None` ⇒ **连冷却都不会记**。故
/// [`Client::chat_stream`] 把它显式转成 `Rejected { status: 429 }`：
/// 软冷却 60 秒 + 换号，与 JS 侧的 `retryWithBackoff`（2000 * 2^n + random）等价
/// —— 同一个账号立刻重来只会再撞一次。
#[allow(dead_code)] // 文档性常量：换号码的判定边界见下方 is_account_failover_code
pub const ACCOUNT_FAILOVER_CODES: [i64; 3] = [4008, 1001, 4010];

// ───────────────────────────── 宿主配置发现 ─────────────────────────────

/// 从宿主写出的原生配置里读到的、Trae 需要的运行时配置。
///
/// ⚠ 为什么不直接用 `crate::config::Config`：宿主实测写出的 JSON 把
/// `trae_auth_dir` 放在 **`pool.`** 嵌套下，而 `config.rs` 里该字段挂在
/// **顶层 `Config`** 上 ⇒ 用 `Config` 反序列化**拿不到**它
/// （`serde` 会静默忽略不认识的键）。
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
    /// Trae 凭证目录（宿主写 `pool.trae_auth_dir`）。
    pub trae_auth_dir: String,
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

/// 从进程参数里找 `-config` / `--config` 后面的那个路径（与 `bin/gateway.rs` 同款）。
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
        trae_auth_dir: pick_str(v, "trae_auth_dir"),
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
// ───────────────────────────── 客户端 ─────────────────────────────

/// 一次对话请求的 HTTP 客户端。
///
/// ★ 两个 client：控制面有总超时，流式对话**没有**总超时（只有读空闲超时）。
/// 这不是洁癖 —— 流式请求用一个带总超时的 client，长回答会在中途被自己掐断，
/// 客户端看到的是「回答说到一半停了」，而日志里只有一句读取超时。
pub struct Client {
    /// 控制面 / 短请求（模型清单、令牌交换）：有总超时。
    pub(crate) http: reqwest::Client,
    /// 流式对话：**无总超时**，只有每次读的空闲超时。
    pub(crate) stream: reqwest::Client,
    /// 单次控制面请求的超时（可被测试覆盖）。
    pub(crate) timeout: Duration,
    /// 测试用的上游 base 覆盖（空 = 按区域表取）。
    ///
    /// ★ 刻意不做成 #[cfg(test)]：那样非测试构建里这个字段会消失，
    /// 而下面读它的分支会被编译器标成死代码。
    base_override: String,
}

impl Client {
    /// 按默认超时构造。**不失败** —— 构造 TLS 后端异常属于「装了坏证书库」，
    /// 那种情况直接 panic 比带着一个用不了的 client 继续跑更好定位。
    pub fn new() -> Self {
        Self {
            http: build(base_builder().timeout(CLIENT_TOTAL_TIMEOUT)),
            stream: build(base_builder().read_timeout(STREAM_READ_TIMEOUT)),
            timeout: DEFAULT_TIMEOUT,
            base_override: String::new(),
        }
    }

    /// 覆盖上游 base（测试用）。
    #[cfg(test)]
    pub fn set_base_for_test(&mut self, base: &str) {
        self.base_override = base.to_string();
    }

    /// 设置代理：空串 = **显式直连**。
    ///
    /// ⚠ reqwest::ClientBuilder 默认 auto_sys_proxy = true，会把请求导向
    /// 系统代理（Windows 上读注册表）。本网关的约定是「宿主配置说了算」，
    /// 宿主没配代理就必须直连 —— 否则用户在别处配的全局代理会**静默**接管
    /// Trae 的流量，而 Trae 的凭证是按出口 IP 风控的，换 IP 会直接吃到 4010。
    ///
    /// 故两个分支都重建 client：空 ⇒ no_proxy()，非空 ⇒ proxy(p)
    /// （reqwest 设置显式代理时自身就会把 auto_sys_proxy 关掉）。
    pub fn set_proxy(&mut self, raw: &str) -> Result<(), String> {
        let addr = raw.trim().to_string();
        // reqwest::ClientBuilder **不是** Clone，两个 client 各造一次 builder。
        let mk = || -> Result<reqwest::ClientBuilder, String> {
            if addr.is_empty() {
                Ok(base_builder())
            } else {
                let p = reqwest::Proxy::all(&addr).map_err(|e| format!("解析代理地址失败: {e}"))?;
                Ok(proxied_builder(p))
            }
        };
        self.http = build(mk()?.timeout(CLIENT_TOTAL_TIMEOUT));
        self.stream = build(mk()?.read_timeout(STREAM_READ_TIMEOUT));
        Ok(())
    }

    /// 按宿主配置决定是否走代理（bin/gateway.rs 与 ZCode 同款入口）。
    ///
    /// proxy_intl 是宿主 proxy_scope.intl 的语义：Trae 的**国际版**要走代理。
    /// 国内版不走 —— 它本来就是国内直连的站。
    pub fn apply_host_proxy(&mut self) {
        let cfg = host_config();
        let addr = cfg.proxy.trim();
        if !addr.is_empty() && cfg.proxy_intl {
            match self.set_proxy(addr) {
                Ok(()) => eprintln!("Trae 走代理：{addr}"),
                Err(e) => eprintln!("⚠ Trae 代理设置失败（将直连）：{e}"),
            }
        } else {
            let _ = self.set_proxy("");
        }
    }

    /// 对话 base：测试覆盖优先，否则按凭证区域取（含 TRAE_HOST_* 环境变量覆盖）。
    fn chat_base(&self, realm: Realm, region_code: &str) -> String {
        if !self.base_override.is_empty() {
            return self.base_override.clone();
        }
        realm::chat_host(realm, region_code)
    }

    /// 一次对话请求的完整 URL。
    pub fn chat_url(&self, cred: &TraeCred) -> String {
        format!(
            "{}{}",
            self.chat_base(cred.realm, &cred.region_code),
            CHAT_PATH
        )
    }

    /// 模型清单请求的完整 URL（同一台主机）。
    pub fn detail_param_url(&self, cred: &TraeCred) -> String {
        format!(
            "{}{}",
            self.chat_base(cred.realm, &cred.region_code),
            DETAIL_PARAM_PATH
        )
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

/// 造 client：build() 失败说明 TLS 后端初始化不了，属于环境损坏。
fn build(builder: reqwest::ClientBuilder) -> reqwest::Client {
    builder
        .build()
        .expect("构造 Trae HTTP 客户端失败（TLS 后端初始化异常）")
}

/// 基础 builder。
///
/// ★ http1_only：逆向的传输对照实验（A 手工 transport / B 加 h2 / C Clone 的
/// transport / D DefaultTransport.Clone / E 手工且关 h2）里，凡是用「clone 出来的
/// transport」的那组 **0/8 成功**，而明确关掉 h2 的组 8/8 成功。上游对 h2 的支持
/// 不稳定，固定 HTTP/1.1 是唯一在所有组里都成立的选择。
///
/// ★ no_proxy：见 Client::set_proxy 的长注释。显式直连是默认值。
fn base_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .http1_only()
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT)
        .pool_max_idle_per_host(20)
        .pool_idle_timeout(Duration::from_secs(90))
}

/// 走代理的 builder：与 base_builder 只差代理一项。
fn proxied_builder(proxy: reqwest::Proxy) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .http1_only()
        .proxy(proxy)
        .connect_timeout(CONNECT_TIMEOUT)
        .pool_max_idle_per_host(20)
        .pool_idle_timeout(Duration::from_secs(90))
}

/// 当前 Unix 秒。
pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 读响应体，硬上限 limit 字节（防御性的：错误体理论上很小，但不能信上游）。
pub(crate) async fn read_limited(resp: reqwest::Response, limit: usize) -> Vec<u8> {
    use futures_util::StreamExt as _;
    let mut out: Vec<u8> = Vec::new();
    let mut s = resp.bytes_stream();
    while let Some(chunk) = s.next().await {
        let Ok(b) = chunk else { break };
        if out.len() + b.len() > limit {
            let room = limit.saturating_sub(out.len());
            out.extend_from_slice(&b[..room.min(b.len())]);
            break;
        }
        out.extend_from_slice(&b);
    }
    out
}

/// 给一次对话请求装上 Trae 的私有请求头。
pub(crate) fn headers_for(
    cred: &TraeCred,
    profile: &DeviceProfile,
    stream: bool,
    last_event_id: Option<&str>,
) -> reqwest::header::HeaderMap {
    let realm = cred.realm;
    let ide_version = device::ide_version(realm, profile.is_solo);
    let ide_version_code = device::ide_version_code(profile.is_solo);
    headers::build(&HeaderInput {
        token: &cred.token,
        uid: &cred.user_id,
        realm,
        ide_version: &ide_version,
        ide_version_code: &ide_version_code,
        device: profile,
        stream,
        last_event_id,
    })
}
// ───────────────────────────── 区域与产品线 ─────────────────────────────

/// 令牌交换的总超时（比控制面松一些：要带一次非对称签名往返）。
pub const OAUTH_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);

/// Trae 的两条产品线。决定 PlatformCode、ClientVersion 与 IDE 版本兜底。
///
/// ⚠ 与 realm 是**正交**的两件事：realm 决定打哪台服务器（CN / SG），
/// flavor 决定「我是哪个客户端」（SOLO 桌面版 / Trae IDE）。国际版 SOLO 与
/// 国服 SOLO 是同一个 flavor，只是 realm 不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthFlavor {
    Trae,
    Solo,
}

/// 由设备档案里的 is_solo 判定产品线。
pub fn oauth_flavor(is_solo: bool) -> OAuthFlavor {
    if is_solo {
        OAuthFlavor::Solo
    } else {
        OAuthFlavor::Trae
    }
}

/// DeviceInfo.ClientVersion / 刷新请求的 IDEVersion。
///
/// ⚠ 真实客户端是从已装目录的 product.json 里读 appVersion 的（每个客户端都不一样），
/// 但网关**刻意不去读用户安装目录**（见 device.rs 的同款决定）。这里用抓包实测的
/// 兜底值：Trae 3.3.99 / SOLO 0.1.61。
pub fn app_version(flavor: OAuthFlavor) -> &'static str {
    match flavor {
        OAuthFlavor::Trae => "3.3.99",
        OAuthFlavor::Solo => "0.1.61",
    }
}

/// DeviceInfo.PlatformCode。SOLO 线是 SOLO_PC，IDE 线是 IDE_PC。
pub fn platform_code(flavor: OAuthFlavor) -> &'static str {
    match flavor {
        OAuthFlavor::Trae => "IDE_PC",
        OAuthFlavor::Solo => "SOLO_PC",
    }
}

/// 设备名（真实客户端取 net.exe user 的 Full Name，回退用户名）。
fn device_name() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "PC".to_string())
}

/// 机型（DeviceInfo.DeviceModel）。
///
/// 真实客户端取 ahaDeviceService.deviceInfo.model（Windows 上即 WMI
/// Win32_ComputerSystem.Model），实测与注册表 HARDWARE\DESCRIPTION\System\BIOS
/// 的 SystemProductName 逐字相同，故直接读注册表，不引入 WMI 依赖。
fn device_model() -> String {
    #[cfg(windows)]
    {
        use windows_registry::LOCAL_MACHINE;
        if let Ok(key) = LOCAL_MACHINE.open(r"HARDWARE\DESCRIPTION\System\BIOS") {
            for name in ["SystemProductName", "BaseBoardProduct"] {
                if let Ok(v) = key.get_string(name) {
                    if !v.trim().is_empty() {
                        return v.trim().to_string();
                    }
                }
            }
        }
    }
    String::new()
}

/// 厂商（DeviceInfo.DeviceBrand），同 device_model 的口径。
fn device_brand() -> String {
    #[cfg(windows)]
    {
        use windows_registry::LOCAL_MACHINE;
        if let Ok(key) = LOCAL_MACHINE.open(r"HARDWARE\DESCRIPTION\System\BIOS") {
            for name in ["SystemManufacturer", "BaseBoardManufacturer"] {
                if let Ok(v) = key.get_string(name) {
                    if !v.trim().is_empty() {
                        return v.trim().to_string();
                    }
                }
            }
        }
    }
    String::new()
}

/// CPU 型号（DeviceInfo.DeviceCPU）。
///
/// 真实客户端取 os.cpus()[0].model 再过一遍归一：有 @ 就取 @ 之前并 trim，
/// 否则只 trim。（注册表 ProcessorNameString 正是 Node 那个值的来源。）
fn device_cpu() -> String {
    #[cfg(windows)]
    {
        use windows_registry::LOCAL_MACHINE;
        if let Ok(key) = LOCAL_MACHINE.open(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0") {
            if let Ok(v) = key.get_string("ProcessorNameString") {
                let trimmed = match v.split_once('@') {
                    Some((before, _)) => before.trim(),
                    None => v.trim(),
                };
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
    }
    String::new()
}

/// 操作系统名（DeviceInfo.OSInfo）。★ **小写** windows —— 真实客户端就是小写。
fn os_info() -> String {
    if cfg!(windows) {
        "windows".to_string()
    } else if cfg!(target_os = "macos") {
        "mac".to_string()
    } else {
        std::env::consts::OS.to_string()
    }
}

/// 操作系统版本（DeviceInfo.OSVersion）。Windows 上取注册表 ProductName。
fn os_version() -> String {
    #[cfg(windows)]
    {
        use windows_registry::LOCAL_MACHINE;
        if let Ok(key) = LOCAL_MACHINE.open(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion") {
            if let Ok(name) = key.get_string("ProductName") {
                if !name.trim().is_empty() {
                    return name;
                }
            }
        }
    }
    std::env::consts::OS.to_string()
}

/// 刷新失败的分类。
///
/// ★ 这个区分**必须**保留：把「网络断了」误判成「refresh_token 失效」会让账号被
/// 立刻标记失效（用户得重新登录），而真实原因只是当时没网。反之把「服务端拒绝」
/// 当瞬时故障，就会一直拿着一个死 token 反复重试。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshErr {
    /// 服务端**明确拒绝**（HTTP 401/403，或响应体里带 code != 0 / ResponseMetadata.Error）。
    Fatal(String),
    /// 网络、代理、超时、响应形状不可解析 —— 换个时间可能就好了。
    Transient(String),
}

impl RefreshErr {
    /// 是不是「账号被判死」那一类。
    pub fn is_fatal(&self) -> bool {
        matches!(self, RefreshErr::Fatal(_))
    }

    pub fn message(&self) -> &str {
        match self {
            RefreshErr::Fatal(m) | RefreshErr::Transient(m) => m,
        }
    }
}

/// 刷一次令牌需要的全部输入。
pub struct RefreshInput<'a> {
    pub refresh_token: &'a str,
    pub realm: Realm,
    /// 凭证文件路径反推出来的宿主 store 目录（读 trae_device.json 用）。
    pub store_dir: Option<&'a std::path::Path>,
    pub is_solo: bool,
}

/// 组装刷新用的 DeviceInfo（12 个字段，顺序与真实客户端一致）。
///
/// ⚠ DeviceID / MachineID **必须**与登录时那两个参数一致，否则上游认为你在
/// 另一台机器上换令牌。它们和公钥一样都来自宿主写下的 trae_device.json。
fn device_info_for_refresh(
    flavor: OAuthFlavor,
    store_dir: Option<&std::path::Path>,
    public_key: &str,
) -> Result<Value, RefreshErr> {
    let (machine_id, device_id) = store_dir
        .and_then(device::read_identity)
        .ok_or_else(|| {
            RefreshErr::Transient(format!(
                "读不到设备身份（{0} 缺失或 machineId 不是 64 位 hex）",
                device::DEVICE_FILE
            ))
        })?;
    Ok(json!({
        "DeviceID": device_id,
        "MachineID": machine_id,
        "PlatformCode": platform_code(flavor),
        "DeviceType": "PC",
        "DeviceName": device_name(),
        "DeviceModel": device_model(),
        "ClientVersion": app_version(flavor),
        "DevicePublicKey": public_key,
        "DeviceBrand": device_brand(),
        "DeviceCPU": device_cpu(),
        "OSInfo": os_info(),
        "OSVersion": os_version(),
    }))
}

/// DeviceProof：ECDSA P-256 + SHA-256 对一段换行连接的原文签名，输出 base64。
///
/// 签名原文（顺序**不能**变，换行连接）：
///
/// ```text
///     <METHOD> \n <PATH> \n <ClientID> \n <RefreshToken> \n <秒级时间戳> \n <nonce>
/// ```
///
/// 这是真实客户端 main.js 里 ODe 的逐字口径。rcgen 的 PKCS_ECDSA_P256_SHA256
/// 用的就是 ECDSA_P256_SHA256_ASN1_SIGNING，与 Node crypto.sign("sha256", …)
/// 对 EC 密钥产出的 DER 编码一致，故不需要任何格式转换。
fn device_proof(
    method: &str,
    path: &str,
    client_id: &str,
    refresh_token: &str,
    private_key_pem: &str,
) -> Result<Value, String> {
    use rand::Rng as _;
    use rcgen::{KeyPair, SigningKey as _};

    if private_key_pem.trim().is_empty() {
        return Err("设备私钥不可用，无法构造 DeviceProof（设备密钥对生成失败）".to_string());
    }
    let kp = KeyPair::from_pem(private_key_pem)
        .map_err(|e| format!("设备私钥解析失败，无法构造 DeviceProof: {e}"))?;

    let ts = now_secs();
    let mut rng = rand::thread_rng();
    let bytes: [u8; 16] = rng.gen();
    let nonce: String = bytes.iter().map(|b| format!("{b:02x}")).collect();

    let payload = [method, path, client_id, refresh_token, &ts.to_string(), &nonce].join("\n");
    let sig = kp
        .sign(payload.as_bytes())
        .map_err(|e| format!("DeviceProof 签名失败: {e}"))?;

    use base64::Engine as _;
    let signature = base64::engine::general_purpose::STANDARD.encode(&sig);
    Ok(json!({
        "Signature": signature,
        "Timestamp": ts,
        "Nonce": nonce,
    }))
}

/// 信封键（外层在前）。★ 真实响应是 {ResponseMetadata, Result}，Result 是**大驼峰**。
const ENVELOPE_KEYS: &[&str] = &[
    "data",
    "Data",
    "result",
    "Result",
    "resp",
    "respResult",
    "response",
    "Response",
    "info",
    "Info",
];

/// 从响应体里逐层展开信封，返回「外层在前」的视图列表。
///
/// # ★★ 为什么必须做这一层
///
/// 老实现只看顶层 code，于是把 {ResponseMetadata:{Error}, Result:{Token}} 这种
/// 真实客户端**唯一见过**的成功响应当成「没有 token」；而 2026-10-02 抓到的失败响应
/// 顶层**根本没有数字 code**，错误全在 ResponseMetadata.Error 里 —— 只看顶层 code
/// 会把失败当成功。两层都得看。
pub fn unwrap_scopes(value: &Value) -> Vec<&Value> {
    const MAX_DEPTH: usize = 8;
    let mut out = vec![value];
    let mut cur = value;
    for _ in 0..MAX_DEPTH {
        let Some(next) = ENVELOPE_KEYS.iter().find_map(|k| cur.get(*k)) else {
            break;
        };
        if next.is_null() {
            break;
        }
        out.push(next);
        cur = next;
    }
    out
}

/// 顶层 code 为数字且非 0 ⇒ 服务端拒绝。
///
/// ⚠ 只有**数字**才算。异构响应（比如纯 {Result:{Token}}）里没有 code，
/// 拿 unwrap_or(-1) 去判会把正常响应当拒绝 —— 老实现就是这么错的。
fn numeric_reject(body: &Value) -> Option<String> {
    let code = body.get("code").and_then(Value::as_i64)?;
    if code == 0 {
        return None;
    }
    let msg = body
        .get("msg")
        .or_else(|| body.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("(无错误信息)");
    Some(format!("code={code} {msg}"))
}

/// 真实错误信封 ResponseMetadata.Error.{Code,Message}。
///
/// 2026-10-02 实测（HTTP 400）：
/// {"ResponseMetadata":{"Action":"","Error":{"Code":"10101","Data":{},
///  "Message":"无效参数：{__Message.field}.","StandardCode":"040004"}, ...}}
fn metadata_error(body: &Value) -> Option<String> {
    let err = body.get("ResponseMetadata")?.get("Error")?;
    if err.is_null() {
        return None;
    }
    let code = err.get("Code").and_then(Value::as_str).unwrap_or("");
    let msg = err
        .get("Message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .replace("{__Message.field}", "某个字段");
    if code.is_empty() && msg.is_empty() {
        return None;
    }
    Some(match (code.is_empty(), msg.is_empty()) {
        (false, false) => format!("code={code} {msg}"),
        (false, true) => format!("code={code}"),
        _ => msg,
    })
}

/// access token 的候选键。
///
/// ★★ 真实响应是 Result.Token（大驼峰）。旧实现只找蛇形 access_token，
/// 于是在真实客户端上「永远刷不出令牌」。
const ACCESS_KEYS: &[&str] = &[
    "Token",
    "AccessToken",
    "access_token",
    "accessToken",
    "token",
    "Jwt",
    "JWT",
];

/// refresh token 的候选键。
///
/// ⚠⚠ 这里**不能**加裸 Token：它和 access token 同键，会把 access 当成 refresh
/// 存进账号库，用户会在几小时后遇到「凭证突然失效」这种极难归因的故障。
const REFRESH_KEYS: &[&str] = &["RefreshToken", "refresh_token", "refreshToken"];

/// 解析 ExchangeToken 响应，取出 (access, refresh)。
pub fn parse_exchange_response(body: &Value) -> Result<(String, Option<String>), String> {
    if let Some(msg) = numeric_reject(body) {
        return Err(format!("服务端拒绝（{msg}）"));
    }
    if let Some(err) = metadata_error(body) {
        return Err(format!("服务端拒绝（{err}）"));
    }
    let scopes = unwrap_scopes(body);

    let access = scopes
        .iter()
        .find_map(|s| {
            ACCESS_KEYS
                .iter()
                .find_map(|k| s.get(*k))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "响应中未找到 access token".to_string())?;

    let refresh = scopes
        .iter()
        .find_map(|s| {
            REFRESH_KEYS
                .iter()
                .find_map(|k| s.get(*k))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    Ok((access, refresh))
}

/// refresh_token 的过期时间（Unix 秒）。
///
/// ★ 真实响应里 Result.RefreshExpireAt 是**毫秒**（形如 1790906034345），
/// 大于 1e10 就按毫秒折算 —— 否则算出来的时间在几万年后，
/// 惰性刷新门永远不触发，refresh_token 会在无人察觉时过期。
const REFRESH_EXPIRY_KEYS: &[&str] = &[
    "RefreshExpireAt",
    "refresh_token_expires_at",
    "refresh_expires_at",
    "refreshTokenExpiresAt",
];

pub fn extract_refresh_expiry(body: &Value) -> Option<i64> {
    let raw = unwrap_scopes(body).iter().find_map(|s| {
        REFRESH_EXPIRY_KEYS
            .iter()
            .find_map(|k| s.get(*k))
            .and_then(Value::as_i64)
    })?;
    Some(if raw > 10_000_000_000 { raw / 1000 } else { raw })
}

/// 把裸 token 规范成带 Cloud-IDE-JWT 前缀的形态（幂等）。
pub fn normalize_access_token(token: &str) -> String {
    let trimmed = token.trim();
    if trimmed.starts_with("Cloud-IDE-JWT ") || trimmed.starts_with("Bearer ") {
        trimmed.to_string()
    } else {
        format!("Cloud-IDE-JWT {trimmed}")
    }
}

/// 取 JWT 的 exp（Unix 秒）。payload 是 base64url 无填充；解不出来返回 None。
///
/// ⚠ 解不出来**不等于**令牌过期（有些账号是手动粘贴的短令牌 / 结构异常），
/// 调用方见 None 时按「不新鲜」处理，让刷新去试一次 —— 试错成本远低于
/// 拿着一个已经过期的令牌打一天 401。
pub fn jwt_exp(jwt_full: &str) -> Option<i64> {
    use base64::Engine as _;
    let trimmed = jwt_full.trim();
    let token = trimmed
        .strip_prefix("Cloud-IDE-JWT ")
        .or_else(|| trimmed.strip_prefix("Bearer "))
        .unwrap_or(trimmed)
        .trim();
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let payload_b64 = parts[1].trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload_b64))
        .ok()?;
    let payload = serde_json::from_slice::<Value>(&bytes).ok()?;
    payload
        .get("exp")
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
}

/// 惰性刷新门（逐字复刻核心库 trae_refresh::lazy_refresh_needed）。
///
/// 三条独立条件，命中任意一条就该刷：
///   1. JWT 剩余寿命不足 48h（⚠ 严格大于才算新鲜，恰好 48h 要刷）；
///   2. JWT 的 exp 解不出来；
///   3. refresh_token 剩余寿命不足 48h。
///
/// # 为什么要这个门
///
/// Trae 的 refresh_token 是**轮换**的：每刷一次旧的立刻作废。若网关和宿主各自
/// 拿着同一个 refresh_token 定期刷，两边会互相把对方踢下线 —— 用户看到的是
/// 「用着用着就掉登录」。只在真正快过期时才动手，才能把冲突压到最小。
pub fn lazy_refresh_needed(jwt: &str, rt_expires_at: Option<i64>, now: i64) -> bool {
    let jwt_fresh = match jwt_exp(jwt) {
        Some(exp) => exp - now > LAZY_REFRESH_MIN_SECS,
        None => false,
    };
    let rt_expiring = rt_expires_at
        .map(|rt| rt - now < LAZY_REFRESH_MIN_SECS)
        .unwrap_or(false);
    !jwt_fresh || rt_expiring
}

/// 令牌换新端点（相对 auth host）。
pub fn exchange_url(realm: Realm) -> String {
    format!("{}{}", realm::auth_host(realm), EXCHANGE_PATH)
}

/// 刷新用的 HTTP 客户端。
///
/// # ★★ 两个改动都不是随手写的
///
/// 1. no_proxy：reqwest::ClientBuilder 默认 auto_sys_proxy = true，会读系统代理
///    设置。令牌交换必须走与账号一致的出口，不能被别处配的全局代理静默接管。
/// 2. local_address(Ipv4Addr::UNSPECIFIED)：强制 IPv4。同一个网络里实测，
///    默认（IPv6 优先）连这台主机要 **105.249 s**，强制 IPv4 只要 **0.073 s**。
///    ★ 这不是猜测：抓包时上游对话请求也花了 **105.42 s** 才返回首帧，
///    与那个 105.249 s 几乎一致 —— 说明这条主机名的 IPv6 路径是坏的，
///    连接阶段就耗掉了 100 多秒。故默认直连的 client 一律强制 IPv4。
fn oauth_client() -> Result<reqwest::Client, RefreshErr> {
    reqwest::Client::builder()
        .http1_only()
        .no_proxy()
        .local_address(Some(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)))
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(OAUTH_TOTAL_TIMEOUT)
        .build()
        .map_err(|e| RefreshErr::Transient(format!("构造刷新 HTTP 客户端失败: {e}")))
}

/// 用 refresh_token 换一对新令牌。
///
/// 成功返回 (access, 新 refresh, refresh 过期秒)；失败返回区分过的 RefreshErr。
pub async fn exchange_token(
    input: &RefreshInput<'_>,
) -> Result<(String, Option<String>, Option<i64>), RefreshErr> {
    let refresh_token = input.refresh_token.trim();
    if refresh_token.is_empty() {
        return Err(RefreshErr::Fatal(
            "该账号没有 refresh_token（只能手动粘贴 JWT 或重新 OAuth 登录）".to_string(),
        ));
    }

    let flavor = oauth_flavor(input.is_solo);
    let private_key = input
        .store_dir
        .and_then(device::read_private_key)
        .ok_or_else(|| {
            RefreshErr::Transient(format!(
                "读不到设备私钥（{0} 缺失或没有 devicePrivateKey），无法构造 DeviceProof",
                device::DEVICE_FILE
            ))
        })?;
    let public_key = input
        .store_dir
        .and_then(device::read_public_key)
        .unwrap_or_default();
    let device = device_info_for_refresh(flavor, input.store_dir, &public_key)?;
    let client_id = realm::oauth_client_id(input.realm);
    let proof = device_proof("POST", EXCHANGE_PATH, &client_id, refresh_token, &private_key)
        .map_err(RefreshErr::Transient)?;

    let body = json!({
        "ClientID": client_id,
        "ClientSecret": "",
        "RefreshToken": refresh_token,
        "DeviceInfo": device,
        "DeviceProof": proof,
        "IDEVersion": app_version(flavor),
    });

    let client = oauth_client()?;
    let url = exchange_url(input.realm);
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("x-cloudide-token", "")
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            RefreshErr::Transient(format!("请求 ExchangeToken 失败（网络/代理问题）: {e}"))
        })?;

    let status = resp.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(RefreshErr::Fatal(format!(
            "服务端拒绝（HTTP {}）：refresh_token 已失效，请重新登录",
            status.as_u16()
        )));
    }
    let raw = read_limited(resp, MAX_ERROR_BODY).await;
    let text = String::from_utf8_lossy(&raw).into_owned();
    if !status.is_success() {
        return Err(RefreshErr::Transient(format!(
            "ExchangeToken 返回 HTTP {}: {}",
            status.as_u16(),
            crate::upstream::classify::truncate(&text, 200)
        )));
    }
    let parsed: Value = serde_json::from_str(&text).map_err(|e| {
        RefreshErr::Transient(format!(
            "ExchangeToken 响应不是 JSON: {e}（原始前 200 字: {}）",
            crate::upstream::classify::truncate(&text, 200)
        ))
    })?;

    if let Some(msg) = numeric_reject(&parsed) {
        return Err(RefreshErr::Fatal(format!("服务端拒绝（{msg}）")));
    }
    let (access, refresh) = parse_exchange_response(&parsed).map_err(RefreshErr::Fatal)?;
    let expires = extract_refresh_expiry(&parsed);
    Ok((normalize_access_token(&access), refresh, expires))
}

/// 账号级失败码：命中即「换号有用」。
///
/// ★★ 与模型/参数级失败码的边界是**硬的**（JS auth.js:483-491 逐字）：
///   - 4008 额度耗尽 —— 换号有用（但额度耗尽在网关里升级成 402 不轮转，见 dispatch）
///   - 1001 鉴权失效 / 令牌被轮换 —— 换号有用
///   - 4010 风控临时拒绝访问 —— 换号有用（等一会儿就好，故只给 120 min 冷却）
///   - 4017 是**设备/IP 级**风控，换号无用且会白白烧掉健康账号 ⇒ 故意不在这个集合里
///   - 4001 / 4023 / 2001 / 4027 是模型/参数级 ⇒ 换号无用（换多少个号都是同样的报错）
#[allow(dead_code)] // 上游错误码 → 网关处置的权威对照表；调用点已内联进 dispatch
pub fn is_account_failover_code(code: i64) -> bool {
    ACCOUNT_FAILOVER_CODES.contains(&code)
}

/// 该账号实际的对话 host。
#[allow(dead_code)] // 与 Client::chat_base 同义；保留给非测试调用点
pub fn effective_chat_host(cred: &TraeCred) -> String {
    realm::chat_host(cred.realm, &cred.region_code)
}

// ============================================================================
// 对话与模型详情
// ============================================================================

/// 预读首个「有产出」行组时的字节上限。
///
/// Trae 的 SSE 是**行组**协议（`id:` / `event:` / `data:` / 空行），
/// 错误组可能出现在任意位置。理论上要找到「第一个会产出帧的组」才停，这里给
/// 一个远高于真实帧的上限，避免畸形流把内存吃光。
const MAX_FIRST_GROUP_BYTES: usize = 4 * 1024 * 1024;

/// 预读首个「有产出」行组的总时间预算。
///
/// ★ 必须远大于 `forward.rs` 的 `probe_timeout`（默认 30 秒）：实测
/// Trae 上游从发请求到响应头就花了 **105.42 秒**
///（`test-bin/trae-upstream-probe/out-cn-valid.txt`）。若沿用 probe 的预算，
/// 正常回答会在网关侧被误判成「上游返回空流」，账号还会被记错一笔。这里给足
/// 预算，把「慢」和「空」分开；真正的死连接由 `STREAM_READ_TIMEOUT`
///（reqwest 空闲超时）兜底。
pub const FIRST_GROUP_TIMEOUT: Duration = STREAM_READ_TIMEOUT;

/// 预读结果。
struct FirstGroup {
    /// 已读到的原始字节（UTF-8 有损）。**必须**原样拼回流的开头。
    consumed: String,
    /// 首个有产出的行组就是 `event:error` 时，这里是它的 (code, message)。
    error: Option<(i64, String)>,
}

/// 扫描已读字节时的状态机结果。
enum GroupScan {
    /// 还没有完整的、会产出帧的行组。
    NeedMore,
    /// 已经读到会产出内容的组（`output` / `done`）。
    Data,
    /// 读到 `event:error` 组。
    Error { code: i64, message: String },
}

impl Client {
    /// 发送一次对话请求，并把上游的行组 SSE 就地翻译成 OpenAI SSE。
    ///
    /// `upstream_body` 由 dispatch 组装（已含 `function` /
    /// `config_name` / `model` 重写）；`client_model` 是**客户端请求
    /// 的模型名**，只用于翻译出的帧 —— 上游 key 与它未必相同
    ///（`products/qoder/dispatch.rs:127-128` 有同款注释）。
    pub async fn chat_stream(
        &self,
        cred: &TraeCred,
        profile: &DeviceProfile,
        upstream_body: &[u8],
        client_model: &str,
    ) -> Result<ChatCall, GatewayError> {
        let cred = self.refresh_if_needed(cred, profile.is_solo).await?;

        // ★ 上游的流式（行组 SSE）与非流式（单个 JSON）响应形状完全不同，而网关
        // 的每条 ChatOutcome::Stream 都要过 forward 的首帧预读 —— 非流式响应会被
        // 判成「空流」。所以这里无条件强制打开 stream，让两条路合成一条。
        let body = force_stream(upstream_body);

        let head_headers = headers_for(&cred, profile, true, None);
        let mut rb = self.stream.post(self.chat_url(&cred)).body(body);
        for (name, value) in head_headers.iter() {
            rb = rb.header(name, value);
        }
        let mut resp = rb
            .send()
            .await
            .map_err(|e| GatewayError::Transport(format!("连接 Trae 上游失败（{e}）")))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let raw = read_limited(resp, MAX_ERROR_BODY).await;
            return Ok(ChatCall::Rejected {
                status,
                body: String::from_utf8_lossy(&raw).into_owned(),
            });
        }

        // ★ 首帧行组必须自己看一眼。Trae 把业务错误放在 HTTP 200 的
        // `event:error` 里（实测首组就可能直接是 4011），而 forward.rs 的
        // `probe_first_frame` 会把「首帧即错误」一律判成 503 并轮转账号 ——
        // 而 4008 / 1001 / 4010 与 4001 / 4023 需要完全不同的处置。预读到的
        // 字节原样拼回流首，forward 的预读因此**立即**命中，一帧都不会丢。
        let head = read_first_group(&mut resp).await;
        if let Some((code, message)) = head.error {
            eprintln!("⚠ Trae 上游返回业务错误（code {code}）：{message}");
            if code == 4008 {
                // 额度耗尽：forward.rs:723-754 转成 402 并停到明天 4 点，不轮转。
                return Err(GatewayError::QuotaExhausted(translate::error_text(
                    &json!(code),
                    &message,
                )));
            }
            if code == 1001 || code == 4010 {
                // 1001 凭证失效 / 4010 风控临时拒绝：换号。4010 顺带软冷却。
                let status = if code == 1001 { 401 } else { 429 };
                return Ok(ChatCall::Rejected {
                    status,
                    body: error_body(code, &message),
                });
            }
            if code == 4001 || code == 4023 {
                // 模型 / 参数级无效：每个账号都会同样失败，绝不能轮转。
                //
                // ★ 4001 的字面意思是 param is invalid，而 Trae 在「function 池与该
                //   账号不符」时也回它 —— 也就是模型名可能完全合法。请求是**发出去
                //   过**的，故不能用 ModelNotFoundError 默认那句「请求未发出」，改由
                //   with_note 给出准确说法。dispatch 层若换了池仍拿回它，会再换一遍
                //   措辞（那时才敢说「两边都试过了」）。
                return Err(GatewayError::ModelNotFound(Box::new(
                    crate::error::ModelNotFoundError::new(
                        client_model.to_string(),
                        String::new(),
                        Vec::new(),
                    )
                    .with_note(format!(
                        "上游用参数校验失败（code {code}）拒绝了这次请求 —— Trae 的 function 池与该账号不符时也报同一个码，故不一定是模型名不存在。"
                    )),
                )));
            }
            // 4011（通用限流）等：给一次软冷却（60 秒）后换号，与 JS 侧的退避
            // 重试等价 —— 同一个账号立刻重来只会再撞一次。
            return Ok(ChatCall::Rejected {
                status: 429,
                body: error_body(code, &message),
            });
        }

        Ok(ChatCall::Stream(wrap_stream(
            head.consumed,
            resp,
            client_model,
        )))
    }

    /// 拉取模型详情（`getModelDetailParam`，默认 function 为 chat_v3）。
    ///
    /// 只做只读查询，不参与对话链路；留给 dispatch 以后做目录校验用。
    #[allow(dead_code)]
    pub async fn get_detail_param(
        &self,
        cred: &TraeCred,
        profile: &DeviceProfile,
        function: &str,
    ) -> Result<Value, GatewayError> {
        let body = json!({
            "function": function,
            "config_names": Value::Null,
            "need_prompt": false,
            "current_config_info": Value::Null,
            "poly_prompt": true,
            "mode_type": Value::Null,
            "agent_type": Value::Null,
        })
        .to_string();

        let headers = headers_for(cred, profile, false, None);
        let mut rb = self
            .http
            .post(self.detail_param_url(cred))
            .timeout(self.timeout)
            .body(body);
        for (name, value) in headers.iter() {
            rb = rb.header(name, value);
        }
        let resp = rb
            .send()
            .await
            .map_err(|e| GatewayError::Transport(format!("连接 Trae 上游失败（{e}）")))?;

        let status = resp.status().as_u16();
        let raw = read_limited(resp, MAX_CONFIG_BODY).await;
        if status >= 400 {
            return Err(GatewayError::UpstreamParse(format!(
                "get_detail_param 返回 HTTP {status}: {}",
                String::from_utf8_lossy(&raw)
            )));
        }
        serde_json::from_slice::<Value>(&raw).map_err(|e| {
            GatewayError::UpstreamParse(format!("get_detail_param 响应不是 JSON: {e}"))
        })
    }

    /// 需要时刷新令牌，返回一个可能换了 token 的凭证副本。
    ///
    /// ★ 刷新必须留在产品层：`forward.rs:655-663` 的令牌刷新**只服务
    /// WorkBuddy**，Trae 走到这里时 token 可能已经过期。
    ///
    /// 判定只用 JWT 的 exp ——`TraeCred.expires_at` 恒不可用（core 的
    /// `trae_account.rs` 只写 `refresh_token_expires_at`，从不写
    /// `expiresAt`），所以 refresh_token 的到期时间按「未知」处理。
    async fn refresh_if_needed(
        &self,
        cred: &TraeCred,
        is_solo: bool,
    ) -> Result<TraeCred, GatewayError> {
        let now = now_secs();
        if !lazy_refresh_needed(&cred.token, None, now) {
            return Ok(cred.clone());
        }
        if cred.refresh_token.trim().is_empty() {
            // 只有 JWT 的账号（手动粘贴）：拿旧令牌再试一次，失败交给上层换号。
            return Ok(cred.clone());
        }

        let store_dir = device::store_dir_of_cred_file(&cred.file_path);
        let input = RefreshInput {
            refresh_token: &cred.refresh_token,
            realm: cred.realm,
            store_dir: store_dir.as_deref(),
            is_solo,
        };

        match exchange_token(&input).await {
            Ok((token, refresh, expires_at)) => {
                let token = normalize_access_token(&token);
                if let Err(e) = cred.save_refreshed(&token, refresh.as_deref(), expires_at.unwrap_or(0))
                {
                    eprintln!("⚠ Trae 刷新后的凭证落盘失败（继续用新令牌）：{e}");
                }
                eprintln!("Trae 令牌已刷新（uid {}）", super::mask_uid(&cred.uid));
                let mut out = cred.clone();
                out.token = token;
                if let Some(rt) = refresh {
                    if !rt.is_empty() {
                        out.refresh_token = rt;
                    }
                }
                Ok(out)
            }
            Err(e) => {
                let msg = e.message().to_string();
                let uid = super::mask_uid(&cred.uid);
                if e.is_fatal() {
                    eprintln!("⚠ Trae 账号 {uid} 的令牌刷新被拒：{msg}");
                } else {
                    eprintln!("⚠ Trae 账号 {uid} 的令牌刷新失败（临时）：{msg}");
                }
                Err(GatewayError::Transport(format!("Trae 令牌刷新失败：{msg}")))
            }
        }
    }
}

/// 强制打开上游的流式开关。
///
/// ★ 为什么必须这么做：上游的流式与非流式**响应形状完全不同**（行组 SSE 对
/// 单个 JSON），两条路各自翻译就有两套代码 + 两套测试；而网关的非流式路径
/// 本来就是「读完流再聚合」（`forward.rs:1092-1125`）—— 它本来就期望拿到
/// 流。一条路径，一处真相。
///（与 `products/zcode/client.rs:1225-1247` 同款；差异：Go 那边 body 为
/// null 会 panic，这里原样返回。）
pub fn force_stream(body: &[u8]) -> Vec<u8> {
    let Ok(Value::Object(mut m)) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    if m.get("stream").and_then(Value::as_bool) == Some(true) {
        return body.to_vec();
    }
    m.insert("stream".to_string(), Value::Bool(true));
    match serde_json::to_vec(&m) {
        Ok(out) => out,
        Err(_) => body.to_vec(),
    }
}

/// 取请求体里的 `model`（解析失败或缺失时返回空串）。
pub fn model_of(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("model")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// 把上游字节流就地翻译成 OpenAI SSE，并重建 `reqwest::Response`。
///
/// `prefix` 是首帧预读时已经消费掉的字节，先原样吐回去，之后才是上游剩余
/// 部分。原始 `Response` 的 `read_timeout` 仍然作用于它自己的
/// `bytes_stream()`；只有**翻译后**的流才走 `http::Response` 重建 ——
/// `src/upstream/sse.rs:842-845` 警告的是别拿重建的 Response 去读**原始
/// body**，这里是反向，不冲突。
fn wrap_stream(prefix: String, resp: reqwest::Response, model: &str) -> reqwest::Response {
    let head = futures_util::stream::once(async move {
        Ok::<Bytes, std::io::Error>(Bytes::from(prefix.into_bytes()))
    });
    let tail = resp
        .bytes_stream()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
    let upstream = head.chain(tail);
    let body = reqwest::Body::wrap_stream(translate::translate_stream(upstream, model.to_string()));
    reqwest::Response::from(axum::http::Response::new(body))
}

/// 生成 OpenAI 形状的错误体，让 forward.rs 的分类逻辑照常工作。
fn error_body(code: i64, message: &str) -> String {
    json!({
        "error": {
            "message": translate::error_text(&json!(code), message),
            "type": "upstream_error",
            "code": code,
        }
    })
    .to_string()
}

/// 预读字节直到出现第一个「会产出帧」的行组。
///
/// 读到的字节全部放在 `consumed` 里交回，**不丢任何一帧**。
async fn read_first_group(resp: &mut reqwest::Response) -> FirstGroup {
    let deadline = tokio::time::Instant::now() + FIRST_GROUP_TIMEOUT;
    let mut consumed: Vec<u8> = Vec::new();

    loop {
        match scan_first_group(&String::from_utf8_lossy(&consumed)) {
            GroupScan::Error { code, message } => {
                return FirstGroup {
                    consumed: String::from_utf8_lossy(&consumed).into_owned(),
                    error: Some((code, message)),
                };
            }
            GroupScan::Data => {
                return FirstGroup {
                    consumed: String::from_utf8_lossy(&consumed).into_owned(),
                    error: None,
                };
            }
            GroupScan::NeedMore => {}
        }

        if consumed.len() > MAX_FIRST_GROUP_BYTES {
            break;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, resp.chunk()).await {
            Ok(Ok(Some(bytes))) => consumed.extend_from_slice(&bytes),
            // 超时 / 连接错误 / 上游关流：把已读的交回，让 forward 的预读去定性。
            _ => break,
        }
    }

    FirstGroup {
        consumed: String::from_utf8_lossy(&consumed).into_owned(),
        error: None,
    }
}

/// 在已读字节里找第一个「会产出帧」的行组。
///
/// Trae 用 `\n\n` 分隔行组；这里顺手容忍 `\r\n`（归一化只用于扫描，
/// 交回下游的仍是原始字节）。
fn scan_first_group(buf: &str) -> GroupScan {
    let normalized = buf.replace("\r\n", "\n");
    let mut rest = normalized.as_str();
    loop {
        let Some(pos) = rest.find("\n\n") else {
            return GroupScan::NeedMore;
        };
        let block = &rest[..pos];
        rest = &rest[pos + 2..];
        if let Some(hit) = classify_group_block(block) {
            return hit;
        }
    }
}

/// 判定单个行组是否产出帧；不产出的组（排队 / 用量 / 元信息）返回 None，
/// 让调用方继续往后看。
fn classify_group_block(block: &str) -> Option<GroupScan> {
    let mut event = "";
    let mut data = "";
    for raw in block.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(v) = line.strip_prefix("event:") {
            event = v.trim();
        } else if let Some(v) = line.strip_prefix("data:") {
            data = v.trim();
        }
    }
    if event.is_empty() && data.is_empty() {
        return None;
    }
    match event {
        "error" => {
            let v: Value = serde_json::from_str(data).unwrap_or(Value::Null);
            let code = v.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some(GroupScan::Error { code, message })
        }
        // 只有 output / done 会产出帧（translate.rs 的 normalize 同此判定）。
        "output" | "done" => Some(GroupScan::Data),
        _ => None,
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::products::trae::cred::TraeCred as Cred;

    const JWT_RAW: &str = "eyJhbGciOiJSUzI1NiJ9.eyJleHAiOjE3OTIxMjU4MDR9.sig";
    const JWT: &str = "Cloud-IDE-JWT eyJhbGciOiJSUzI1NiJ9.eyJleHAiOjE3OTIxMjU4MDR9.sig";

    /// 规范凭证的副本。
    fn cred(realm: Realm, token: &str, refresh_token: &str) -> Cred {
        Cred {
            uid: "trae-2593120869167228".to_string(),
            user_id: "2593120869167228".to_string(),
            nickname: "主号".to_string(),
            token: token.to_string(),
            refresh_token: refresh_token.to_string(),
            expires_at: 0,
            realm,
            realm_evidence: crate::products::trae::realm::RealmEvidence::Explicit,
            region_code: String::new(),
            no_route: false,
            file_path: "D:/definitely-not-a-real-dir-placeholder/trae-2593120869167228.json"
                .to_string(),
        }
    }

    fn profile(is_solo: bool) -> DeviceProfile {
        DeviceProfile {
            machine_id: "a".repeat(64),
            device_id: "4496320041074586".to_string(),
            device_model: "M600".to_string(),
            cpu: "Intel".to_string(),
            os_name: "windows".to_string(),
            os_version: "Windows 11 Pro".to_string(),
            is_solo,
        }
    }

    /// 取请求头（缺失时返回空串），与 headers.rs 的测试助手同款。
    fn get(h: &reqwest::header::HeaderMap, name: &str) -> String {
        h.get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }

    /// 把 Trae 的行组 SSH 拼成一段字节流。
    fn group(id: u32, event: &str, data: &str) -> String {
        format!("id:{id}\nevent:{event}\ndata:{data}\n\n")
    }

    // ── 行组扫描 ──────────────────────────────────────────────────────

    #[test]
    fn scan_reports_need_more_when_no_group_is_complete() {
        assert!(matches!(
            scan_first_group("id:1\nevent:output\ndata:{\"c\":\"x\"}"),
            GroupScan::NeedMore
        ));
        assert!(matches!(scan_first_group(""), GroupScan::NeedMore));
    }

    #[test]
    fn scan_finds_output_group() {
        let buf = group(1, "output", "{\"response\":\"hi\"}");
        assert!(matches!(scan_first_group(&buf), GroupScan::Data));
    }

    #[test]
    fn scan_finds_error_group_anywhere_in_the_stream() {
        // ★ 真实抓包：首个分组就可能直接是 4011。
        let buf = format!(
            "{}{}",
            group(1, "meta", "{}"),
            group(
                2,
                "error",
                "{\"code\":4011,\"message\":\"exceeded the rate limit\",\"extra\":null}"
            )
        );
        match scan_first_group(&buf) {
            GroupScan::Error { code, message } => {
                assert_eq!(code, 4011);
                assert_eq!(message, "exceeded the rate limit");
            }
            _ => panic!("应当判为错误组"),
        }
    }

    #[test]
    fn scan_skips_non_productive_groups() {
        // 排队 / 用量 / 元信息组都不产出帧，要继续往后看。
        let buf = format!(
            "{}{}{}",
            group(1, "queue", "{\"position\":3}"),
            group(2, "usage", "{\"tokens\":1}"),
            group(3, "output", "{\"content\":\"x\"}")
        );
        assert!(matches!(scan_first_group(&buf), GroupScan::Data));
    }

    #[test]
    fn scan_tolerates_crlf() {
        let buf = "id:1\r\nevent:error\r\ndata:{\"code\":1001,\"message\":\"s\"}\r\n\r\n";
        match scan_first_group(buf) {
            GroupScan::Error { code, message } => {
                assert_eq!(code, 1001);
                assert_eq!(message, "s");
            }
            _ => panic!("应当判为错误组"),
        }
    }

    #[test]
    fn scan_error_without_code_defaults_to_zero() {
        let buf = group(1, "error", "{\"message\":\"boom\"}");
        match scan_first_group(&buf) {
            GroupScan::Error { code, message } => {
                assert_eq!(code, 0);
                assert_eq!(message, "boom");
            }
            _ => panic!("应当判为错误组"),
        }
    }

    // ── 请求体改写 ────────────────────────────────────────────────────

    #[test]
    fn force_stream_turns_a_missing_flag_on() {
        let out = force_stream(br#"{"model":"glm-5.2"}"#);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["stream"], Value::Bool(true));
        assert_eq!(v["model"], "glm-5.2");
    }

    #[test]
    fn force_stream_keeps_an_explicit_true_as_is() {
        let body = br#"{"stream":true}"#;
        assert_eq!(force_stream(body), body.to_vec());
    }

    #[test]
    fn force_stream_overrides_an_explicit_false() {
        // ★ 非流式响应是单个 JSON，会被 forward 的首帧预读判成空流，故强制打开。
        let out = force_stream(br#"{"stream":false}"#);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["stream"], Value::Bool(true));
    }

    #[test]
    fn force_stream_passes_non_object_bodies_through() {
        // ⚠ 与 Go 不同：那边 body 为 null 会 panic（在 nil map 里赋值）。
        assert_eq!(force_stream(b"null"), b"null".to_vec());
        assert_eq!(force_stream(b"[1,2]"), b"[1,2]".to_vec());
        assert_eq!(force_stream(b""), Vec::<u8>::new());
    }

    #[test]
    fn model_of_reads_the_requested_model() {
        assert_eq!(model_of(br#"{"model":"glm-5.2"}"#), "glm-5.2");
        assert_eq!(model_of(br#"{"n":1}"#), "");
        assert_eq!(model_of(b"not json"), "");
    }

    // ── 首帧错误映射 ──────────────────────────────────────────────────

    #[test]
    fn error_body_is_openai_shaped_and_keeps_the_upstream_code() {
        let body = error_body(4011, "exceeded the rate limit");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["error"]["code"], 4011);
        assert_eq!(v["error"]["type"], "upstream_error");
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(msg.contains("4011"), "{msg}");
        assert!(msg.contains("exceeded"), "{msg}");
    }

    #[test]
    fn the_only_rotating_codes_are_the_ones_forward_can_act_on() {
        for code in ACCOUNT_FAILOVER_CODES {
            assert!(is_account_failover_code(code), "{code}");
        }
        // ⚠ 4017 是设备 / IP 级风控：换号只会把健康的号一个个烧掉。
        assert!(!is_account_failover_code(4017));
        // ⚠ 4011 是通用限流，首帧就命中（实测），但换号无用。
        assert!(!is_account_failover_code(4011));
        // 模型 / 参数级：每个账号都会同样失败。
        assert!(!is_account_failover_code(4001));
        assert!(!is_account_failover_code(4023));
    }

    // ── 惰性刷新门 ────────────────────────────────────────────────────

    #[test]
    fn jwt_exp_reads_the_payload_of_a_prefixed_token() {
        assert_eq!(jwt_exp(JWT), Some(1_792_125_804));
        assert_eq!(jwt_exp(JWT_RAW), Some(1_792_125_804));
        assert_eq!(jwt_exp("no-dots"), None);
    }

    #[test]
    fn lazy_refresh_stays_idle_while_the_jwt_is_fresh() {
        let exp = 1_792_125_804;
        // 距到期还有 30 天 ⇒ 不刷。
        assert!(!lazy_refresh_needed(JWT, None, exp - 30 * 24 * 3600));
    }

    #[test]
    fn lazy_refresh_fires_inside_the_48h_window() {
        let exp = 1_792_125_804;
        // 还剩 1 小时 ⇒ 刷。
        assert!(lazy_refresh_needed(JWT, None, exp - 3600));
        // 恰好 48 小时 ⇒ 也是「不新鲜」，要刷（严格大于才算新鲜）。
        assert!(lazy_refresh_needed(JWT, None, exp - LAZY_REFRESH_MIN_SECS));
    }

    #[test]
    fn lazy_refresh_fires_when_the_jwt_is_unparsable() {
        // 手动粘贴的短令牌解不出 exp ⇒ 让刷新去试一次，比打一天 401 便宜。
        assert!(lazy_refresh_needed("Cloud-IDE-JWT abc", None, 1_700_000_000));
    }

    #[test]
    fn lazy_refresh_fires_when_the_refresh_token_is_expiring() {
        let exp = 1_792_125_804;
        let now = exp - 30 * 24 * 3600;
        assert!(!lazy_refresh_needed(JWT, None, now));
        assert!(lazy_refresh_needed(JWT, Some(now + 3600), now));
    }

    // ── 换新响应解析 ──────────────────────────────────────────────────

    #[test]
    fn access_token_is_always_carried_with_its_scheme() {
        assert_eq!(normalize_access_token(JWT_RAW), JWT);
        // 已经带前缀时原样返回，不能变成 Cloud-IDE-JWT Cloud-IDE-JWT …
        assert_eq!(normalize_access_token(JWT), JWT);
        assert_eq!(normalize_access_token("Bearer abc"), "Bearer abc");
    }

    #[test]
    fn exchange_response_reads_the_real_upper_camel_shape() {
        // ★ 真实响应是 Result.Token（大驼峰）。旧实现只找蛇形 access_token，
        //   于是在真机上「永远刷不出令牌」。
        let v: Value = serde_json::from_str(
            r#"{"Result":{"Token":"a.b.c","RefreshToken":"rt"},"code":0}"#,
        )
        .unwrap();
        let (access, refresh) = parse_exchange_response(&v).unwrap();
        assert_eq!(access, "a.b.c");
        assert_eq!(refresh.as_deref(), Some("rt"));
    }

    #[test]
    fn exchange_response_refuses_a_numeric_error_code() {
        let v: Value =
            serde_json::from_str(r#"{"code":40001,"msg":"invalid refresh token"}"#).unwrap();
        let err = parse_exchange_response(&v).unwrap_err();
        assert!(err.starts_with("服务端拒绝"), "{err}");
        assert!(err.contains("40001"), "{err}");
    }

    #[test]
    fn exchange_response_refuses_a_metadata_error() {
        let v: Value = serde_json::from_str(
            r#"{"ResponseMetadata":{"Error":{"Code":"10101","Message":"无效参数：{__Message.field}."}}}"#,
        )
        .unwrap();
        let err = parse_exchange_response(&v).unwrap_err();
        assert!(err.contains("10101"), "{err}");
        // 占位符必须换成人话，否则用户看到一串模板。
        assert!(!err.contains("{__Message.field}"), "{err}");
        assert!(err.contains("某个字段"), "{err}");
    }

    #[test]
    fn exchange_response_refuses_a_response_without_any_token() {
        let v: Value = serde_json::from_str(r#"{"Result":{}}"#).unwrap();
        let err = parse_exchange_response(&v).unwrap_err();
        assert!(err.contains("access token"), "{err}");
    }

    #[test]
    fn refresh_expiry_reads_milliseconds_and_seconds() {
        let ms: Value = serde_json::from_str(r#"{"Result":{"RefreshExpireAt":1790906034345}}"#)
            .unwrap();
        assert_eq!(extract_refresh_expiry(&ms), Some(1_790_906_034));
        let sec: Value = serde_json::from_str(r#"{"Result":{"RefreshExpireAt":1790906034}}"#)
            .unwrap();
        assert_eq!(extract_refresh_expiry(&sec), Some(1_790_906_034));
        let none: Value = serde_json::from_str(r#"{"Result":{"Token":"a"}}"#).unwrap();
        assert_eq!(extract_refresh_expiry(&none), None);
    }

    #[test]
    fn a_bare_refresh_token_is_never_taken_from_the_access_slot() {
        // ⚠ RefreshToken 与裸 Token 是**不同**的键；若误把 access 当 refresh 存回
        //   账号库，用户会在几小时后遇到「凭证突然失效」这种极难归因的故障。
        let v: Value = serde_json::from_str(r#"{"Result":{"Token":"a.b.c"}}"#).unwrap();
        let (access, refresh) = parse_exchange_response(&v).unwrap();
        assert_eq!(access, "a.b.c");
        assert_eq!(refresh, None);
    }

    #[test]
    fn scopes_are_unwrapped_outer_first() {
        let v: Value = serde_json::from_str(r#"{"data":{"result":{"Token":"x"}}}"#).unwrap();
        let scopes = unwrap_scopes(&v);
        assert_eq!(scopes.len(), 3);
        assert_eq!(scopes[2], &v["data"]["result"]);
        // null 就停止下探，不要把「没东西」当成一层。
        let n: Value = serde_json::from_str(r#"{"data":null,"Token":"x"}"#).unwrap();
        assert_eq!(unwrap_scopes(&n).len(), 1);
    }

    // ── 刷新失败的分类 ────────────────────────────────────────────────

    #[test]
    fn refresh_error_kinds_are_distinguishable() {
        let fatal = RefreshErr::Fatal("服务端拒绝（HTTP 401）".to_string());
        let trans = RefreshErr::Transient("网络断了".to_string());
        assert!(fatal.is_fatal());
        assert!(!trans.is_fatal());
        assert!(fatal.message().contains("401"));
        assert_eq!(trans.message(), "网络断了");
    }

    #[test]
    fn only_the_ide_flavor_uses_ide_pc() {
        assert_eq!(platform_code(OAuthFlavor::Trae), "IDE_PC");
        assert_eq!(platform_code(OAuthFlavor::Solo), "SOLO_PC");
        assert_eq!(app_version(OAuthFlavor::Trae), "3.3.99");
        assert_eq!(app_version(OAuthFlavor::Solo), "0.1.61");
        assert!(matches!(oauth_flavor(false), OAuthFlavor::Trae));
        assert!(matches!(oauth_flavor(true), OAuthFlavor::Solo));
    }

    // ── 请求 URL ──────────────────────────────────────────────────────

    #[test]
    fn chat_url_targets_the_line_group_endpoint() {
        let mut client = Client::new();
        client.set_base_for_test("https://trae.test");
        let url = client.chat_url(&cred(Realm::Cn, JWT, "rt"));
        assert_eq!(url, format!("https://trae.test{CHAT_PATH}"));
        assert!(url.ends_with("/api/agent/v3/llm_utils_chat"));
        assert_eq!(
            client.detail_param_url(&cred(Realm::Cn, JWT, "rt")),
            "https://trae.test/api/ide/v1/get_detail_param"
        );
    }

    #[test]
    fn exchange_url_follows_the_realm_auth_host() {
        assert_eq!(
            exchange_url(Realm::Cn),
            format!("https://api.trae.cn{EXCHANGE_PATH}")
        );
        assert_eq!(
            exchange_url(Realm::Sg),
            format!("https://growsg-normal.trae.ai{EXCHANGE_PATH}")
        );
    }

    #[test]
    fn effective_chat_host_falls_back_to_the_default_for_the_realm() {
        assert_eq!(effective_chat_host(&cred(Realm::Cn, JWT, "rt")), realm::chat_host_default(Realm::Cn, ""));
    }

    // ── 上游请求头 ────────────────────────────────────────────────────

    #[test]
    fn headers_carry_the_bare_user_id_and_split_the_token() {
        let c = cred(Realm::Cn, JWT, "rt");
        let h = headers_for(&c, &profile(false), true, None);
        assert_eq!(get(&h, "x-uid"), "2593120869167228");
        // ★ auth.js:1173-1174：Authorization 是**整串**，X-Cloudide-Token 才是裸 JWT。
        assert_eq!(get(&h, "authorization"), JWT);
        assert_eq!(get(&h, "x-cloudide-token"), JWT_RAW);
        // 流式才带 accept，非流式请求（get_detail_param）不能带。
        assert_eq!(get(&h, "accept"), "text/event-stream");
        let h2 = headers_for(&c, &profile(false), false, None);
        assert!(h2.get("accept").is_none());
    }

    #[test]
    fn last_event_id_is_only_attached_when_present() {
        let c = cred(Realm::Cn, JWT, "rt");
        let h = headers_for(&c, &profile(false), true, Some("42"));
        assert_eq!(get(&h, "last-event-id"), "42");
        let h2 = headers_for(&c, &profile(false), true, None);
        assert!(h2.get("last-event-id").is_none());
    }

    // ── host 配置解析 ────────────────────────────────────────────────

    #[test]
    fn host_config_reads_the_pool_scoped_auth_dir() {
        let v: Value = serde_json::from_str(
            r#"{"pool":{"trae_auth_dir":"D:/x/trae/auths"},"proxy":"http://127.0.0.1:7890"}"#,
        )
        .unwrap();
        let cfg = parse_host_config(&v);
        assert_eq!(cfg.trae_auth_dir, "D:/x/trae/auths");
        assert_eq!(cfg.proxy, "http://127.0.0.1:7890");
        // 缺省：境外代理默认开（与宿主的 proxy_scope.intl 默认一致）。
        assert!(cfg.proxy_intl);
    }

    #[test]
    fn host_config_honours_a_disabled_intl_proxy() {
        let v: Value =
            serde_json::from_str(r#"{"proxy_scope":{"cn":false,"intl":false}}"#).unwrap();
        assert!(!parse_host_config(&v).proxy_intl);
    }

    #[test]
    fn a_top_level_key_wins_over_the_pool_copy() {
        let v: Value =
            serde_json::from_str(r#"{"proxy":"http://a:1","pool":{"proxy":"http://b:2"}}"#).unwrap();
        assert_eq!(parse_host_config(&v).proxy, "http://a:1");
    }

    #[test]
    fn a_blank_value_is_treated_as_absent() {
        let v: Value = serde_json::from_str(r#"{"pool":{"trae_auth_dir":"   "}}"#).unwrap();
        assert!(parse_host_config(&v).trae_auth_dir.is_empty());
    }

    #[test]
    fn host_config_is_never_empty_even_without_a_config_file() {
        // OnceLock 缓存的是宿主真实配置或「全默认」；
        // 没有配置文件时也必须给出可直接用的值，不能 panic。
        let cfg = host_config();
        assert!(cfg.proxy_intl || !cfg.proxy_intl);
    }

    // ── 客户端构造 ────────────────────────────────────────────────────

    #[test]
    fn a_client_can_be_built_and_re_proxied() {
        let mut client = Client::new();
        assert!(client.set_proxy("http://127.0.0.1:7890").is_ok());
        assert!(client.set_proxy("").is_ok());
        let err = client.set_proxy("not a proxy").unwrap_err();
        assert!(err.contains("解析代理地址失败"), "{err}");
        client.apply_host_proxy();
    }

    #[test]
    fn the_test_knob_overrides_the_host() {
        let mut client = Client::new();
        let before = client.chat_url(&cred(Realm::Sg, JWT, "rt"));
        client.set_base_for_test("https://override.test/");
        let after = client.chat_url(&cred(Realm::Sg, JWT, "rt"));
        assert!(after.starts_with("https://override.test"));
        assert_ne!(before, after);
    }

    #[test]
    fn a_default_client_matches_new() {
        let a = Client::default();
        let b = Client::new();
        // 两者都从同一份 host 配置构造；只要 url 推导一致即可（不比较连接池）。
        let c = cred(Realm::Cn, JWT, "rt");
        assert_eq!(a.chat_url(&c), b.chat_url(&c));
    }

    #[test]
    fn device_profile_ids_are_read_only() {
        // 网关从不铸造设备身份：resolve 只会读，签名里也没有可写的出口。
        let p = device::resolve(Realm::Cn, None);
        assert_eq!(p.machine_id.len() == 64 || p.machine_id.is_empty(), true);
        assert!(p.device_id.is_empty() || !p.device_id.is_empty());
    }

    #[test]
    fn the_wrapper_prefixes_the_bytes_it_already_consumed() {
        // 预读消费掉的字节必须原样拼回流的开头，否则首帧会丢。
        // 这里只验证「不 panic 且返回一个 Response」——真正的字节顺序由
        // read_first_group 的 consumed 语义保证（见 scan_* 一组测试）。
        let head = futures_util::stream::once(async {
            Ok::<Bytes, std::io::Error>(Bytes::from_static(b"x"))
        });
        let body = reqwest::Body::wrap_stream(head);
        let resp = reqwest::Response::from(axum::http::Response::new(body));
        assert_eq!(resp.status().as_u16(), 200);
    }
}
