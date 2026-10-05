//! ZCode 客户端身份与追踪头。
//!
//! 移植自 Go `wt-port\go-gateway\internal\zcode\identity.go`（555 行），逐字对齐。
//!
//! # 核心立场：头是**可选**的，取不到就直接省略
//!
//! 填 `platform="unknown"` 这种假值**比不发更容易被识别** —— 官方客户端在
//! 某个平台上运行时那些头一定存在，而我们发一个"存在但取值是 unknown"的头，
//! 恰好是最容易与真机区分的形状。故本模块所有取值函数在拿不到值时返回空串，
//! 由 [`Identity::headers`] 决定是否发送。
//!
//! 头清单：`HTTP-Referer` / `User-Agent` / `X-ZCode-App-Version` / `X-Title` /
//! `X-Release-Channel` / `X-Client-Language` / `X-Client-Timezone` /
//! `X-ZCode-Agent`（固定 `glm`）/ `X-Platform` / `X-Os-Category` / `X-Os-Version`。
//!
//! ★ LLM 路径**不发** `X-Device-Mid`（3.12.3 起），但**控制面必须发**
//! （缺了回 `400 {"code":3001,"msg":"parameter error"}`；实测任意 UUID 都通过
//! ⇒ 是格式校验，不是设备身份校验）。

use std::collections::BTreeMap;

use rand::RngCore;
use sha2::{Digest, Sha256};

/// 生成一个 UUID v4 形态的 deviceMid。
///
/// ## 为什么随机就够
///
/// 实测（`uitest/probe-zcode-quota-auth.cjs`）：上游**只校验格式**，
/// 随机生成的 UUID 与客户端真实读到的一样能通过（都回 200）。
/// 故不需要假装是某个已注册设备 —— 那反而更可疑。
///
/// ## 为什么要是 v4 形态（第 13 位 = 4）
///
/// 上游会对"看着不像 UUID"的值直接 429/3001。规范的 v4 形态最安全。
pub fn new_device_mid() -> String {
    let mut b = [0u8; 16];
    // 随机源不可用时回退一个固定值 —— 额度查询失败好过整个功能不可用。
    if rand::rngs::OsRng.try_fill_bytes(&mut b).is_err() {
        return "00000000-0000-4000-8000-000000000000".to_string();
    }
    // 版本位（第 7 字节高 4 位 = 4）与变体位（第 9 字节高 2 位 = 10）
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    hex_uuid(&b)
}

/// 把 16 字节按 `8-4-4-4-12` 拼成 UUID 文本。
fn hex_uuid(b: &[u8; 16]) -> String {
    let s = hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &s[0..8],
        &s[8..12],
        &s[12..16],
        &s[16..20],
        &s[20..32]
    )
}

/// 是否是 UUID 形态（控制面请求的硬要求）。
///
/// 宽松判据（只认形状，不校验版本位）：上游就是这么判的 ——
/// 实测非 UUID 字符串会被 429，而任意 UUID（含非 v4）都通过。
pub fn is_uuid(s: &str) -> bool {
    let s = s.trim();
    if s.len() != 36 {
        return false;
    }
    for (i, c) in s.bytes().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if c != b'-' {
                    return false;
                }
            }
            _ => {
                if !(c.is_ascii_digit() || (b'a'..=b'f').contains(&c) || (b'A'..=b'F').contains(&c))
                {
                    return false;
                }
            }
        }
    }
    true
}

/// 默认上报的客户端版本。
///
/// # 版本号必须跟着官方客户端走（2026-09-20 更新）
///
/// 参考实现（`TriDefender/zcode-api`）在提交 `8dca421 "Improved compliance"`
/// 把默认值从 `3.12.3` 升到 **`3.14.0`**，并新增了 3.14.0 对应的 host builder。
/// 作者原话（译）：*"每次客户端发版都要跟上，否则 User-Agent 与
/// X-ZCode-App-Version 会变成可区分的特征。"*
///
/// 即：**报一个过旧的版本本身就是指纹**。
///
/// ⚠ 上游升级后这个值还会过时。参考实现的做法是让它可配置
/// （`identity.appVersion`），本模块同样保留 [`Identity::app_version`] 可覆盖。
/// 排查"为什么被风控"时，**先核对这个版本号是否还是最新的**。
pub const DEFAULT_APP_VERSION: &str = "3.14.0";

/// 官方 User-Agent 的 runtime 后缀（逐字）。
///
/// 官方完整值：`ZCode/3.14.0 ai-sdk/provider-utils/4.0.27 runtime/node.js/24`。
const USER_AGENT_RUNTIME_SUFFIX: &str = " ai-sdk/provider-utils/4.0.27 runtime/node.js/24";

/// 客户端身份（一次请求的全部身份维度）。
#[derive(Debug, Clone)]
pub struct Identity {
    /// 上报的客户端版本，默认 [`DEFAULT_APP_VERSION`]。
    pub app_version: String,
    /// `X-Title` 里的来源名，默认 `zcode`。
    pub source_title: String,
    /// `HTTP-Referer`，默认 `https://zcode.z.ai`。
    pub referer_origin: String,
    /// `X-Release-Channel`，默认 `production`。
    pub release_channel: String,
    /// `X-Client-Language`，默认 `zh-CN`。
    pub client_language: String,
    /// `X-Client-Timezone`，默认 `Asia/Shanghai`。
    pub client_timezone: String,
    /// 平台（Node.js 命名之前的那一层，如 `windows`/`darwin`/`linux`）。
    pub platform: String,
    /// 架构（如 `x86_64`/`amd64`/`arm64`）。
    pub arch: String,
    /// 操作系统版本；取不到就空着（不硬凑假值）。
    pub os_version: String,
    /// 账号标识（追踪头的会话级稳定值来源）。
    pub account_id: String,
    /// 会话标识；空则回落到 [`Identity::account_id`]。
    pub session_id: String,
}

impl Default for Identity {
    fn default() -> Self {
        Self::new()
    }
}

impl Identity {
    /// 从环境推导默认身份。
    pub fn new() -> Self {
        let lang = match std::env::var("LANG") {
            Ok(v) if !v.trim().is_empty() => v,
            _ => "zh-CN".to_string(),
        };
        // 环境变量里的 locale 形如 "zh_CN.UTF-8"，归一成 "zh-CN"。
        let lang = lang
            .split('.')
            .next()
            .unwrap_or("")
            .replace('_', "-");
        let lang = if lang.trim().is_empty() {
            "zh-CN".to_string()
        } else {
            lang
        };

        let tz = match std::env::var("TZ") {
            Ok(v) if !v.trim().is_empty() => v,
            _ => "Asia/Shanghai".to_string(),
        };

        Self {
            app_version: DEFAULT_APP_VERSION.to_string(),
            source_title: "zcode".to_string(),
            referer_origin: "https://zcode.z.ai".to_string(),
            release_channel: "production".to_string(),
            client_language: lang,
            client_timezone: tz,
            platform: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            os_version: String::new(),
            account_id: String::new(),
            session_id: String::new(),
        }
    }

    /// 上游要求的 `"{platform}-{arch}"` 形状。
    ///
    /// ## ⚠ 必须用 **Node.js 的命名**，不是宿主语言的
    ///
    /// 实测（2026-09-19，`uitest/diag-zcode-platform-value.cjs`）：
    /// `client/configs` 端点**严格校验**这个值，取值错误返回
    /// `400 {"code":3001,"msg":"parameter error"}` —— 而这个错误信息
    /// **完全不提示是哪个参数错**，排查成本很高。
    ///
    /// ```text
    /// 取值              结果
    /// ────────────────  ────────────────────────────────
    /// win32-x64         ✓ HTTP 200（**正确**）
    /// windows-amd64     ✗ 400（Go 的 runtime.GOOS/GOARCH 命名）
    /// win32-amd64       ✗ 400
    /// windows-x64       ✗ 400
    /// win-x64           ✗ 400
    /// win32 / windows   ✗ 400
    /// （不发这个头）     ✓ HTTP 200
    /// ```
    ///
    /// 原因：上游是照 **Node.js 的 `process.platform` / `process.arch`** 校验的
    /// （官方客户端是 Electron 应用）。故必须做这层映射。
    pub fn platform_arch(&self) -> String {
        format!("{}-{}", node_platform(&self.platform), node_arch(&self.arch))
    }

    /// 要发的身份头。
    ///
    /// 取不到的值**直接省略**而不是填 `"unknown"` —— 理由见模块文档。
    pub fn headers(&self) -> BTreeMap<String, String> {
        let mut h = BTreeMap::new();

        // 这四个是固定的（官方客户端必发）
        h.insert(
            "HTTP-Referer".into(),
            first_non_empty(&[&self.referer_origin, "https://zcode.z.ai"]).to_string(),
        );
        h.insert("X-ZCode-Agent".into(), "glm".into());
        h.insert(
            "X-Client-Language".into(),
            first_non_empty(&[&self.client_language, "zh-CN"]).to_string(),
        );
        h.insert(
            "X-Client-Timezone".into(),
            first_non_empty(&[&self.client_timezone, "Asia/Shanghai"]).to_string(),
        );

        // User-Agent 与 App-Version 成对出现
        let ver = self.app_version.trim();
        if ver.is_empty() {
            // 版本取不到时官方会回退成 "ZCode/unknown"（参考实现的 fio 行为）
            h.insert(
                "User-Agent".into(),
                format!("ZCode/unknown{USER_AGENT_RUNTIME_SUFFIX}"),
            );
        } else {
            // ⚠ 必须带 runtime 后缀（官方实测值）。
            h.insert(
                "User-Agent".into(),
                format!("ZCode/{ver}{USER_AGENT_RUNTIME_SUFFIX}"),
            );
            h.insert("X-ZCode-App-Version".into(), ver.to_string());
        }

        h.insert(
            "X-Title".into(),
            format!(
                "Z Code@{}",
                first_non_empty(&[&self.source_title, "zcode"])
            ),
        );
        h.insert(
            "X-Release-Channel".into(),
            first_non_empty(&[&self.release_channel, "production"]).to_string(),
        );

        // 平台相关：platform 与 arch **必须都有**才发（官方是 `${platform}-${arch}`）
        //
        // ⚠ 取值必须用 Node.js 的命名（win32 / x64），不是 Go 的（windows / amd64）
        // —— 实测取值错误会被上游 `400 {"code":3001}` 拒绝。
        let p = self.platform.trim();
        let a = self.arch.trim();
        if !p.is_empty() && !a.is_empty() {
            h.insert(
                "X-Platform".into(),
                format!("{}-{}", node_platform(p), node_arch(a)),
            );
        }
        if !p.is_empty() {
            h.insert("X-Os-Category".into(), os_category(p).to_string());
        }
        let v = self.os_version.trim();
        if !v.is_empty() {
            h.insert("X-Os-Version".into(), v.to_string());
        }
        h
    }

    /// 在 [`Identity::headers`] 之上补上控制面（额度/账单）**必需**的头。
    ///
    /// ## 唯一差异是 `X-Device-Mid`
    ///
    /// 上游对控制面要求一个 **UUID 形态**的 `X-Device-Mid`：
    ///
    /// ```text
    /// 不发 / 空串 / 非 UUID   → 400 {"code":3001,"msg":"parameter error"}
    /// 任意 UUID              → 200（实测随机生成的也通过）
    /// ```
    ///
    /// 故这里不需要"注册设备" —— 生成一个稳定的 UUID 即可。
    /// **稳定**很重要（不是每次请求随机）：同一个账号的额度查询应呈现为同一台设备，
    /// 否则在服务端看来是"一个账号被大量不同设备查询"。
    ///
    /// deviceMid 为空时**不发**这个头（保持原行为），调用方据此可显式关闭它。
    pub fn control_plane_headers(&self, device_mid: &str) -> BTreeMap<String, String> {
        let mut h = self.headers();
        let v = device_mid.trim();
        if !v.is_empty() {
            h.insert("X-Device-Mid".into(), v.to_string());
        }
        h
    }

    /// **claim（领取限量套餐）** 端点的请求头。
    ///
    /// # 为什么不能用 [`Identity::control_plane_headers`]（2026-09-20 按参考实现对齐）
    ///
    /// 参考实现（`TriDefender/zcode-api` `src/claim/client.ts`）的 claim 请求
    /// **只发 6~7 个头**，且与身份头 bundle 是两套：
    ///
    /// ```text
    /// Authorization: Bearer {jwt}
    /// Content-Type: application/json
    /// X-Aliyun-Captcha-Verify-Param: {verifyParam}
    /// [X-Aliyun-Captcha-Verify-Region: {region}]
    /// X-ZCode-App-Version: {appVersion}      ← 我们此前**缺这个**
    /// X-Platform: {platform}                 ← 我们此前**缺这个**
    /// [X-Device-Mid: {uuid}]                 ← 活动期硬要求
    /// ```
    ///
    /// 而我们此前给 claim 发的是控制面**整套身份头**
    /// （User-Agent / X-Title / HTTP-Referer / X-Os-* / X-ZCode-Agent …）。
    ///
    /// # 为什么"多送头"是风险而不是保险
    ///
    /// 直觉上多发头像是"更完整"，但上游的风控判据是**与真机一致**：
    /// 官方客户端在这条路径上只发那几个头，我们多发一堆反而是**可区分的特征**。
    /// 参考实现特意记载过这个反复：早期版本发全套身份头
    /// （2026-08-28 活动期"经验上被接受"），3.12.3 起**改回逐字镜像客户端**。
    ///
    /// 故这里刻意**不**调用 [`Identity::headers`]，而是按参考实现逐字构造。
    ///
    /// ⚠ `X-Device-Mid` 仍然必需（活动期网关硬要求）——
    /// 参考实现 commit `6b7327e "修复自动claim"` 专门修的就是这个：
    /// 缺它必回 `biz 3001 parameter error`。
    pub fn claim_headers(&self, device_mid: &str) -> BTreeMap<String, String> {
        let mut h = BTreeMap::new();
        h.insert(
            "X-ZCode-App-Version".into(),
            self.app_version.clone(),
        );
        h.insert("X-Platform".into(), self.platform_arch());
        let v = device_mid.trim();
        if !v.is_empty() {
            h.insert("X-Device-Mid".into(), v.to_string());
        }
        h
    }

    /// **preview（查询可领套餐）** 端点的请求头。
    ///
    /// # 比 [`Identity::claim_headers`] 更简（参考实现实测）
    ///
    /// 参考实现的 preview 是**极简头**：有 JWT 时只发 `Authorization`
    /// （+ 活动期的 `X-Device-Mid`），**连 Content-Type / Accept / 身份头都不发**；
    /// 匿名时**一个头都不发**。
    ///
    /// ⚠ 那为什么不干脆复用 claim 的头集？
    /// 因为 preview 与 claim 在参考实现里就是**两个不同的头集**，
    /// 而风控按路径比对。既然已知真机行为，就照它来。
    ///
    /// 返回的头由调用方再补 `Authorization` —— 那取决于凭证是否存在，
    /// 而本函数不该依赖凭证（它只是身份信息的投影）。
    pub fn preview_headers(&self, device_mid: &str) -> BTreeMap<String, String> {
        let mut h = BTreeMap::new();
        let v = device_mid.trim();
        if !v.is_empty() {
            // 活动期网关对 preview 也要求 UUID 形态的 X-Device-Mid
            //（与 claim 同一个实测结论：缺它 3001）。
            h.insert("X-Device-Mid".into(), v.to_string());
        }
        h
    }

    /// **追踪头**（对话通道用）。
    ///
    /// # ⚠⚠ 2026-09-21：差点按一条**已被证伪**的注释改错，记录在此
    ///
    /// 所有者原话：「还有 zcode 为什么在官方就不触发，在你这里就触发，你好好看看
    /// 官方代码，还有参考实现，好好排查」。
    ///
    /// 照着这条线索查参考实现（`test-bin/ref-zcode2api/app/identity.py` 的
    /// `build_trace_headers`），它写着：
    ///
    /// ```text
    /// 渠道差异（关键，**误发会触发上游 3012 "unusual activity"**）：
    ///   · start-plan（JWT 通道）：只发 3 个头，**不发** x-query-id / x-session-id
    ///   · coding-plan（API Key 通道）：额外发这两个
    /// ```
    ///
    /// 看起来完全解释了"官方不触发、我们触发"。**但它是错的。**
    ///
    /// 本仓库的 `CAPTURED-SPEC.md` 记着一次 Reqable 抓包，抓的正是
    /// start-plan 端点：`POST https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages`
    /// （`providerId = account:zai-start-plan → zcode-plan/anthropic`），
    /// 而官方客户端在该请求里**确实发了**这两个头（逐字值都在文档里）：
    ///
    /// ```text
    /// x-query-id:    01a0bc8b-d86e-7e99-9808-73c0d0a52642
    /// x-session-id:  8fc6b5b0-fb13-4801-b1de-988f41d14eed
    /// ```
    ///
    /// 即：**start-plan 也发这两个头**。参考实现那条注释要么针对别的版本，
    /// 要么本身就是错的。
    ///
    /// 结论：**不按参考实现改**。抓包是唯一可靠判据（文档原话：
    /// 「这条测试的价值不在于锁住 3 或 5，而在于锁住『与抓包一致』」）。
    ///
    /// 教训：参考实现的注释是**二手信息**，可能过时或针对别的通道/版本；
    /// 本项目自己的抓包才是一手证据。
    ///
    /// # 生命周期（2026-09-20 抓包实测）
    ///
    /// 对比官方相隔 58 分钟的两次成功请求：
    ///
    /// ```text
    /// x-query-id        每条消息**不同**
    /// x-request-id      每请求**不同**
    /// x-session-id      **完全相同**（会话级稳定）
    /// x-zcode-trace-id  **完全相同**（会话级稳定）
    /// ```
    pub fn trace_headers(&self) -> BTreeMap<String, String> {
        // 会话级稳定值：优先用调用方给的会话 id，否则按账号派生。
        //
        // ⚠ 没有账号标识时**退回随机**（而不是退回固定串）：固定串会让
        // 所有账号共用一个 session-id，那比随机更糟（上游会看到"一个会话
        // 在给所有账号发请求"）。
        let mut session_id = self.session_id.trim().to_string();
        if session_id.is_empty() {
            session_id = self.account_id.trim().to_string();
        }
        let mut h = BTreeMap::new();
        if session_id.is_empty() {
            // 无账号信息：退回每请求随机，至少不比原来差。
            h.insert("x-request-id".into(), new_trace_id());
            h.insert("x-zcode-session-type".into(), "main".into());
            h.insert("x-zcode-trace-id".into(), new_trace_id());
            h.insert("x-query-id".into(), new_trace_id());
            h.insert("x-session-id".into(), new_trace_id());
            return h;
        }
        // 用账号标识**派生**两个稳定 id（同一账号恒定，不同账号不同）。
        //
        // 为什么不直接用 AccountID 本身：官方这两个值是 **UUID 形态**
        //（上游会对"看着不像 UUID"的值回 429/3001）。而账号标识可能是
        // `zcode-abcdef123456` 或长数字 —— 直接发会被拒。
        let trace_id = stable_uuid(&format!("zcode-trace:{session_id}"));
        let mut stable_session = stable_uuid(&format!("zcode-session:{session_id}"));
        // 若调用方给的就是 UUID 形态的会话 id，直接用它作为 session-id
        //（更贴近官方：官方就是用会话 uuid）。
        if looks_like_uuid(&session_id) {
            stable_session = session_id;
        }
        h.insert("x-request-id".into(), new_trace_id()); // 每请求新（官方如此）
        h.insert("x-zcode-session-type".into(), "main".into());
        h.insert("x-zcode-trace-id".into(), trace_id); // 每会话稳定
        h.insert("x-query-id".into(), new_trace_id()); // 每条消息新（官方如此）
        h.insert("x-session-id".into(), stable_session); // 每会话稳定
        h
    }
}

/// 判断是否 UUID 形态（8-4-4-4-12 的十六进制）。
pub fn looks_like_uuid(s: &str) -> bool {
    // 与 is_uuid 同判据（Go 侧 looksLikeUUID 逐字一致：不 TrimSpace）。
    if s.len() != 36 {
        return false;
    }
    for (idx, c) in s.bytes().enumerate() {
        match idx {
            8 | 13 | 18 | 23 => {
                if c != b'-' {
                    return false;
                }
            }
            _ => {
                if !(c.is_ascii_digit() || (b'a'..=b'f').contains(&c) || (b'A'..=b'F').contains(&c))
                {
                    return false;
                }
            }
        }
    }
    true
}

/// 从一个任意字符串**确定性地**派生一个 UUIDv4 形态的 id。
///
/// 用途：需要"同一输入恒定、不同输入不同、且形态必须是 UUID"的标识。
/// 用 SHA-256 截断 —— 确定性来自哈希，不需要额外状态（网关重启后依旧一样，
/// 这点很重要：重启即换 session-id 会再次暴露"非真实会话"）。
pub fn stable_uuid(seed: &str) -> String {
    let sum = Sha256::digest(seed.as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&sum[..16]);
    // 设版本位（v4）与变体位 —— 与 new_device_mid 一致，否则形态不像 UUID
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    hex_uuid(&b)
}

/// 生成一个 UUIDv4 形态的追踪 id。
///
/// 复用 [`new_device_mid`]（它已经处理了 v4 的版本位与变体位）—— 追踪头与
/// 设备标识对**形态**的要求相同：上游会对"看着不像 UUID"的值直接拒绝
/// （参考实现用 `str(uuid.uuid4())`）。
///
/// 单独包一层只是为了语义清晰：这里是"每次请求一个新 id"，
/// 而 `new_device_mid` 的调用点语义是"一个稳定的设备标识"。
pub fn new_trace_id() -> String {
    new_device_mid()
}

/// 把宿主 OS 名映射成 Node.js 的 `process.platform` 取值。
///
/// ```text
/// 宿主名    Node.js process.platform
/// ────────  ────────────────────────
/// windows   "win32"     ← 注意不是 "windows"（这是最容易踩的坑）
/// darwin    "darwin"
/// macos     "darwin"
/// linux     "linux"
/// ```
pub fn node_platform(os: &str) -> String {
    match os.trim().to_ascii_lowercase().as_str() {
        "windows" | "win32" => "win32".to_string(),
        "darwin" | "macos" => "darwin".to_string(),
        "" => node_platform(std::env::consts::OS),
        _ => "linux".to_string(),
    }
}

/// 把宿主架构名映射成 Node.js 的 `process.arch` 取值。
///
/// ```text
/// 宿主名    Node.js process.arch
/// ─────────  ─────────────────────
/// amd64     "x64"       ← 注意不是 "amd64"
/// x86_64    "x64"       ← ★ Rust 的 std::env::consts::ARCH 是这个值
/// arm64     "arm64"
/// 386       "ia32"
/// ```
///
/// ★ `x86_64` 那条是本移植**新增**的：Go 的 `runtime.GOARCH` 是 `amd64`，
/// 而 Rust 的 `std::env::consts::ARCH` 是 `x86_64`。若照抄 Go 的 match 表，
/// `x86_64` 会落到 default 分支被原样发出 ⇒ `win32-x86_64` ⇒ 上游
/// `400 {"code":3001}`，而且那个错误信息**完全不提示是哪个参数错**。
pub fn node_arch(arch: &str) -> String {
    match arch.trim().to_ascii_lowercase().as_str() {
        "amd64" | "x64" | "x86_64" => "x64".to_string(),
        "arm64" | "aarch64" => "arm64".to_string(),
        "386" | "ia32" | "x86" => "ia32".to_string(),
        "" => node_arch(std::env::consts::ARCH),
        other => other.to_string(),
    }
}

/// 把宿主 OS 名归一成官方客户端的 `X-Os-Category` 取值。
///
/// 官方取值（参考实现 `identity.ts` 的 `normalizeOsCategory`）：
///
/// ```text
/// darwin → "macos"
/// win32  → "windows"
/// 其它   → "linux"
/// ```
pub fn os_category(os: &str) -> String {
    match os.trim().to_ascii_lowercase().as_str() {
        "darwin" | "macos" => "macos".to_string(),
        "windows" | "win32" => "windows".to_string(),
        _ => "linux".to_string(),
    }
}

/// 首个 trim 后非空的候选（返回的是 **trim 后**的值）。
fn first_non_empty<'a>(vals: &[&'a str]) -> &'a str {
    for v in vals {
        let t = v.trim();
        if !t.is_empty() {
            return t;
        }
    }
    ""
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_device_mid_is_v4_shaped_and_unique() {
        let a = new_device_mid();
        let b = new_device_mid();
        assert!(is_uuid(&a), "生成的 deviceMid 必须是 UUID 形态：{a}");
        assert_ne!(a, b, "两次生成必须不同（否则所有账号共用一个设备）");
        // 版本位 = 4、变体位 ∈ {8,9,a,b}
        assert_eq!(&a[14..15], "4", "第 13 位必须是 4（v4 形态）");
        assert!(
            ["8", "9", "a", "b"].contains(&&a[19..20]),
            "变体位必须是 8/9/a/b：{a}"
        );
    }

    #[test]
    fn is_uuid_accepts_any_shape_rejects_bad_shape() {
        assert!(is_uuid("00000000-0000-4000-8000-000000000000"));
        // 宽松判据：不校验版本位（上游就是这么判的）
        assert!(is_uuid("00000000-0000-0000-0000-000000000000"));
        assert!(is_uuid("ABCDEF01-2345-6789-ABCD-EF0123456789"));
        // 前后空白容忍（Go 侧 TrimSpace）
        assert!(is_uuid("  00000000-0000-4000-8000-000000000000  "));

        assert!(!is_uuid(""));
        assert!(!is_uuid("zcode-abcdef123456"));
        // 长度对但分隔符位置错
        assert!(!is_uuid("000000000-000-4000-8000-000000000000"));
        // 含非十六进制字符
        assert!(!is_uuid("00000000-0000-4000-8000-00000000000z"));
    }

    #[test]
    fn looks_like_uuid_does_not_trim() {
        assert!(looks_like_uuid("8fc6b5b0-fb13-4801-b1de-988f41d14eed"));
        assert!(!looks_like_uuid(" 8fc6b5b0-fb13-4801-b1de-988f41d14eed"));
    }

    #[test]
    fn stable_uuid_is_deterministic_and_seed_sensitive() {
        let a1 = stable_uuid("zcode-trace:acct-1");
        let a2 = stable_uuid("zcode-trace:acct-1");
        let b = stable_uuid("zcode-trace:acct-2");
        assert_eq!(a1, a2, "同一 seed 必须恒定（网关重启后不变）");
        assert_ne!(a1, b, "不同 seed 必须不同");
        assert!(is_uuid(&a1));
        assert_eq!(&a1[14..15], "4");
    }

    #[test]
    fn node_platform_and_arch_use_node_naming() {
        // ★ 这张表就是「400 code=3001 不提示哪个参数错」的那个坑。
        assert_eq!(node_platform("windows"), "win32");
        assert_eq!(node_platform("win32"), "win32");
        assert_eq!(node_platform("darwin"), "darwin");
        assert_eq!(node_platform("macos"), "darwin");
        assert_eq!(node_platform("linux"), "linux");
        assert_eq!(node_platform("freebsd"), "linux");

        assert_eq!(node_arch("amd64"), "x64");
        assert_eq!(node_arch("x64"), "x64");
        // ★ Rust 的 std::env::consts::ARCH 取值，Go 的 match 表里没有这一条。
        assert_eq!(node_arch("x86_64"), "x64");
        assert_eq!(node_arch("arm64"), "arm64");
        assert_eq!(node_arch("aarch64"), "arm64");
        assert_eq!(node_arch("386"), "ia32");
        assert_eq!(node_arch("ia32"), "ia32");

        assert_eq!(os_category("darwin"), "macos");
        assert_eq!(os_category("macos"), "macos");
        assert_eq!(os_category("windows"), "windows");
        assert_eq!(os_category("win32"), "windows");
        assert_eq!(os_category("linux"), "linux");
        assert_eq!(os_category("freebsd"), "linux");
    }

    #[test]
    fn empty_platform_falls_back_to_real_host() {
        // 空值回退到真实值（不能拼出 "-x64" 这种形状）
        let p = node_platform("");
        let a = node_arch("");
        assert!(!p.is_empty());
        assert!(!a.is_empty());
        assert!(is_uuid(&stable_uuid("x")), "空平台也必须能拼出合法 UUID");
    }

    #[test]
    fn platform_arch_matches_the_real_host_in_node_naming() {
        let id = Identity::new();
        let pa = id.platform_arch();
        let (p, a) = pa.split_once('-').expect("必须有 '-'");
        assert!(
            ["win32", "darwin", "linux"].contains(&p),
            "平台必须是 Node 命名：{pa}"
        );
        assert!(
            ["x64", "arm64", "ia32"].contains(&a),
            "架构必须是 Node 命名：{pa}"
        );
        // ★ 回归：绝不能出现 Go/Rust 的原生命名（那会被上游 400 拒绝）
        assert!(!pa.contains("windows-"), "不能发 windows-amd64：{pa}");
        assert!(!pa.contains("x86_64"), "不能发 x86_64：{pa}");
        assert!(!pa.contains("amd64"), "不能发 amd64：{pa}");
    }

    #[test]
    fn headers_omit_unknown_values_instead_of_faking_them() {
        let mut id = Identity::new();
        id.app_version = String::new();
        id.os_version = String::new();
        id.platform = String::new();
        id.arch = String::new();
        let h = id.headers();
        // 版本取不到 ⇒ 官方会回退成 ZCode/unknown，且**不发** App-Version
        assert_eq!(
            h.get("User-Agent").map(String::as_str),
            Some("ZCode/unknown ai-sdk/provider-utils/4.0.27 runtime/node.js/24")
        );
        assert!(!h.contains_key("X-ZCode-App-Version"));
        // platform/arch 取不到 ⇒ 两个头都省略（不填 "unknown"）
        assert!(!h.contains_key("X-Platform"));
        assert!(!h.contains_key("X-Os-Category"));
        assert!(!h.contains_key("X-Os-Version"));
        // 固定四个仍在
        assert_eq!(h.get("X-ZCode-Agent").map(String::as_str), Some("glm"));
        assert_eq!(
            h.get("HTTP-Referer").map(String::as_str),
            Some("https://zcode.z.ai")
        );
    }

    #[test]
    fn headers_full_set_for_a_normal_identity() {
        let mut id = Identity::new();
        id.app_version = "3.14.0".into();
        id.platform = "windows".into();
        id.arch = "amd64".into();
        let h = id.headers();
        assert_eq!(
            h.get("User-Agent").map(String::as_str),
            Some("ZCode/3.14.0 ai-sdk/provider-utils/4.0.27 runtime/node.js/24")
        );
        assert_eq!(h.get("X-ZCode-App-Version").map(String::as_str), Some("3.14.0"));
        assert_eq!(h.get("X-Platform").map(String::as_str), Some("win32-x64"));
        assert_eq!(h.get("X-Os-Category").map(String::as_str), Some("windows"));
        assert_eq!(h.get("X-Title").map(String::as_str), Some("Z Code@zcode"));
        assert_eq!(
            h.get("X-Release-Channel").map(String::as_str),
            Some("production")
        );
        assert_eq!(h.get("X-Client-Language").map(String::as_str), Some("zh-CN"));
        assert_eq!(
            h.get("X-Client-Timezone").map(String::as_str),
            Some("Asia/Shanghai")
        );
        // ★ LLM 路径不发 X-Device-Mid（3.12.3 起）
        assert!(!h.contains_key("X-Device-Mid"));
    }

    #[test]
    fn control_plane_headers_add_device_mid_only() {
        let id = Identity::new();
        let base = id.headers();
        let with = id.control_plane_headers("00000000-0000-4000-8000-000000000000");
        assert_eq!(
            with.get("X-Device-Mid").map(String::as_str),
            Some("00000000-0000-4000-8000-000000000000")
        );
        // 其余头与 Headers() 完全一致
        for (k, v) in &base {
            assert_eq!(with.get(k), Some(v), "控制面头不能改动 {k}");
        }
        assert_eq!(with.len(), base.len() + 1);
        // deviceMid 为空 ⇒ 不发（调用方据此可显式关闭）
        let none = id.control_plane_headers("   ");
        assert!(!none.contains_key("X-Device-Mid"));
        assert_eq!(none, base);
    }

    #[test]
    fn claim_headers_are_deliberately_minimal() {
        let id = Identity::new();
        let h = id.claim_headers("00000000-0000-4000-8000-000000000000");
        // 逐字镜像官方客户端：只有这几条
        assert_eq!(h.len(), 3);
        assert_eq!(h.get("X-ZCode-App-Version").map(String::as_str), Some("3.14.0"));
        assert_eq!(h.get("X-Platform").map(String::as_str), Some(&id.platform_arch()[..]));
        assert!(h.contains_key("X-Device-Mid"));
        // ★ 多发身份头反而是**可区分的特征**（风控按路径比对真机）
        for k in [
            "User-Agent",
            "X-Title",
            "HTTP-Referer",
            "X-ZCode-Agent",
            "X-Os-Category",
            "X-Os-Version",
            "X-Client-Language",
            "X-Client-Timezone",
            "X-Release-Channel",
        ] {
            assert!(!h.contains_key(k), "claim 不该发 {k}（那是可区分特征）");
        }
        // 没有 deviceMid 时只剩两条
        let bare = id.claim_headers("");
        assert_eq!(bare.len(), 2);
    }

    #[test]
    fn preview_headers_carry_only_device_mid() {
        let id = Identity::new();
        let h = id.preview_headers("00000000-0000-4000-8000-000000000000");
        assert_eq!(h.len(), 1);
        assert!(h.contains_key("X-Device-Mid"));
        assert!(id.preview_headers("").is_empty(), "匿名时一个头都不发");
    }

    #[test]
    fn trace_headers_are_session_stable_and_request_fresh() {
        let mut id = Identity::new();
        id.account_id = "zcode-abcdef123456".into();

        let a = id.trace_headers();
        let b = id.trace_headers();

        // 会话级稳定：同一账号两次调用必须相同
        assert_eq!(a.get("x-session-id"), b.get("x-session-id"));
        assert_eq!(a.get("x-zcode-trace-id"), b.get("x-zcode-trace-id"));
        assert_eq!(a.get("x-zcode-session-type").map(String::as_str), Some("main"));
        // 每请求新：这两个必须不同（否则与脚本无异）
        assert_ne!(a.get("x-request-id"), b.get("x-request-id"));
        assert_ne!(a.get("x-query-id"), b.get("x-query-id"));

        // 形态必须是 UUID（上游对"不像 UUID"的值回 429/3001）
        for k in [
            "x-request-id",
            "x-zcode-trace-id",
            "x-query-id",
            "x-session-id",
        ] {
            assert!(is_uuid(a.get(k).unwrap()), "{k} 必须是 UUID 形态");
        }
        // 五个头一个不少（★ 不按参考实现那条已被证伪的"start-plan 只发 3 个"改）
        assert_eq!(a.len(), 5);
    }

    #[test]
    fn trace_headers_are_per_account_and_uuid_session_is_kept() {
        let mut a = Identity::new();
        a.account_id = "acct-a".into();
        let mut b = Identity::new();
        b.account_id = "acct-b".into();
        assert_ne!(
            a.trace_headers().get("x-session-id"),
            b.trace_headers().get("x-session-id"),
            "不同账号不能共用 session-id"
        );

        // 调用方给的就是 UUID ⇒ 直接用它（更贴近官方）
        let sid = "8fc6b5b0-fb13-4801-b1de-988f41d14eed";
        let mut c = Identity::new();
        c.session_id = sid.into();
        assert_eq!(
            c.trace_headers().get("x-session-id").map(String::as_str),
            Some(sid)
        );
    }

    #[test]
    fn trace_headers_fall_back_to_random_without_any_account_id() {
        let id = Identity::new();
        let a = id.trace_headers();
        let b = id.trace_headers();
        // 无账号信息 ⇒ 退回每请求随机（而不是退回固定串：固定串比随机更糟）
        assert_ne!(a.get("x-session-id"), b.get("x-session-id"));
        assert_eq!(a.len(), 5);
        for k in ["x-request-id", "x-zcode-trace-id", "x-query-id", "x-session-id"] {
            assert!(is_uuid(a.get(k).unwrap()));
        }
    }

    #[test]
    fn default_identity_uses_documented_defaults() {
        let id = Identity::new();
        assert_eq!(id.app_version, "3.14.0");
        assert_eq!(id.source_title, "zcode");
        assert_eq!(id.referer_origin, "https://zcode.z.ai");
        assert_eq!(id.release_channel, "production");
        // LANG/TZ 缺失时的默认值（CI 上通常都缺）
        if std::env::var("TZ").map(|v| v.trim().is_empty()).unwrap_or(true) {
            assert_eq!(id.client_timezone, "Asia/Shanghai");
        }
        assert!(!id.client_language.is_empty());
        assert!(!id.platform.is_empty());
        assert!(!id.arch.is_empty());
        assert!(id.os_version.is_empty(), "取不到就省略（不硬凑假值）");
    }
}
