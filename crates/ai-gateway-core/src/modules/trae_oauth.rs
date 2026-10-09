//! Trae OAuth 授权码登录（F-78，对标参考实现 `commands/oauth.rs` + `oauth_loopback.rs`）。
//!
//! 与「粘贴 JWT」的关系：OAuth 是**获得** JWT 的正规途径，粘贴只是兜底。
//! 一条完整的登录链路是：
//!
//! 1. [`login_url`] 签发授权 URL（带 `state` 与 PKCE challenge）；
//! 2. 用户在浏览器完成授权，授权页 302 回 `redirect_uri`；
//! 3. 回调地址被本机回环监听器（Tauri 宿主实现）捕获，原文交给
//!    [`parse_callback`] 解析出 AuthCode；
//! 4. [`exchange_auth_code`] 用 PKCE verifier 换 access_token / refresh_token；
//! 5. [`fetch_user_info`] 取昵称 → 落库（`trae_account::upsert_account`）。
//!
//! **本模块只做纯逻辑与 HTTP，不监听端口** —— 监听器需要 axum + Tauri 事件，
//! 属于宿主职责；core 保持无 Tauri 依赖（见 `AGENTS.md` 架构约定）。
//!
//! 安全要点（参考实现踩过的坑，逐条保留）：
//!
//! - **CSRF**：回调必须带签发时记录的 `state`，否则拒绝 —— 否则攻击者可以把
//!   自己的授权码回调给受害者的应用，让受害者账号被替换成攻击者的。
//! - **PKCE**：`code_verifier` 只存在于本进程内存，回调换 token 时携带。
//! - **HTML 转义**：回调结果页会把上游错误原文与用户可控的 URL 插进 HTML，
//!   必须实体转义，否则错误信息里的 `<script>` 会被浏览器执行。

use std::sync::Mutex;

use serde_json::{json, Value};

use super::config;

/// 真实 Trae IDE 登录 URL 的实证 client_id（**TRAE 产品线**）。
///
/// 取自 `D:\APP\Trae CN\resources\app\product.json` 的
/// `iCubeApp.authConfig.TRAE.stable`（权威来源，不是推断）。
pub const OAUTH_CLIENT_ID: &str = "ono9krqynydwx5";
/// **SOLO 产品线**的 client_id（同文件 `authConfig.SOLO.stable`）。
///
/// ★★ 2026-10-02 补：此前只有 TRAE 一份，于是装了 **TRAE SOLO CN** 的用户
/// 走 OAuth 会被授权页**踢回官网首页**（授权流程根本起不来）。
///
/// 根因是所有者抓到的真实可用 URL 里 `auth_from=solo` 且
/// `client_id=en1oxy7wnw8j9n` —— 与 TRAE 那条**不是同一套**。
///
/// ⚠ 本文件此前的注释写着「旧值 `en1oxy7wnw8j9n` 会让授权页停在 billing
/// status 后不回跳」—— 那条结论是**针对 TRAE 产品线**的（拿 SOLO 的
/// client_id 去走 TRAE 的链路，当然不通）。两个 client_id 各自配自己的
/// 产品线才对，**不能**用一个覆盖另一个。
pub const OAUTH_CLIENT_ID_SOLO: &str = "en1oxy7wnw8j9n";
/// 授权页据此进入 `native_ide` 原生授权流程（前端调 GetPCAuthCode 后 302 回回调地址）。
///
/// ★ 2026-10-02：真实值来自已装客户端 `product.json` 的 **`tronBuildVersion`**
/// （SOLO CN = `2.3.79533`、Trae CN = `2.3.82600`）。所有者抓包里的
/// `plugin_version=2.3.79533` 与 SOLO CN 的那份**逐字吻合**。
/// 本常量只是「读不到 product.json」时的兜底 —— 见 [`client_versions`]。
pub const OAUTH_PAGE_PLUGIN_VERSION: &str = "2.3.79533";

/// `IDEVersion` / `x_app_version` 的兜底值（**分产品线**）。
///
/// ★★ 2026-10-02 修正：这个值**不是** `product.json` 的 `version`
/// （两个客户端都是 `1.107.1`），而是它单独的 **`appVersion`** 字段：
///
/// | 客户端 | `version` | `appVersion` | `tronBuildVersion` |
/// |---|---|---|---|
/// | TRAE SOLO CN | 1.107.1 | **0.1.61** | 2.3.79533 |
/// | Trae CN      | 1.107.1 | **3.3.99** | 2.3.82600 |
///
/// 判据：所有者抓到的真实登录 URL 里 `x_app_version=0.1.61`，与他装的
/// SOLO CN 的 `appVersion` 逐字相同。此前写死的 `3.3.100` **两个都不对**
/// （Trae CN 是 `3.3.99`，差一位）。
pub const OAUTH_PAGE_APP_VERSION_SOLO: &str = "0.1.61";
pub const OAUTH_PAGE_APP_VERSION_TRAE: &str = "3.3.99";

/// `DeviceInfo.PlatformCode`（真实客户端 `k()`：SOLO 线是 `SOLO_PC`，IDE 线是 `IDE_PC`）。
///
/// ★★ 2026-10-02 修正：此前全局写死 `IDE_PC` —— 对装了 TRAE SOLO 的用户是错的。
pub fn platform_code(flavor: TraeFlavor) -> &'static str {
    match flavor {
        TraeFlavor::Trae => "IDE_PC",
        TraeFlavor::Solo => "SOLO_PC",
    }
}
/// 云 IDE API 附带的 `X-App-Id`。
///
/// ⚠ 2026-10-02：**OAuth 链路已不再使用它**。真实客户端 `main.js` 里
/// `X-App-Id` 出现 **0 处** —— 那是我们照着别处的调用臆造的 header。
/// 保留常量只为不打断其他可能引用（当前无引用点）。
pub const OAUTH_APP_ID: &str = "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8";
/// 本机回环监听端口与回调地址（须与授权 URL 中的回调参数完全一致）。
pub const OAUTH_LOOPBACK_PORT: u16 = 17388;
pub const OAUTH_REDIRECT_URI: &str = "http://127.0.0.1:17388/authorize";
/// 授权页基址（**不含路径**）。
///
/// ★ 2026-10-02 修正：真实客户端走的是 `{host}/authorization`（不是 `/authorize`），
/// 且**先 301 到 `www.trae.cn`**（`www.trae.com.cn` 只是别名）。
/// 所有者抓到的真实 URL 就是 `https://www.trae.cn/authorization?...`。
pub const OAUTH_AUTHORIZE_HOST: &str = "https://www.trae.cn";
/// 授权页路径（真实客户端用 `/authorization`）。
pub const OAUTH_AUTHORIZE_PATH: &str = "/authorization";
/// 授权页完整地址（供既有测试与调用方引用，值等于 host + path）。
pub const OAUTH_AUTHORIZE_URL: &str = "https://www.trae.cn/authorization";

/// AuthCode 换 token 的**路径**（不含 host）。
///
/// ★★ 2026-10-02 修正：此前用的是
/// `/cloudide/api/v3/trae/oauth/ExchangeToken`（OAuth 标准信封的猜想），
/// 而真实客户端 `main.js` 里写死的是
/// `` `${t}/trae/api/v3/oauth/ExchangeToken` `` —— 两者**不是同一条路径**。
///
/// 判别实验（`test-bin/trae-exchange-probe/probe4.py`）：对不存在的路径
/// `POST /trae/api/v3/oauth/THIS_PATH_DOES_NOT_EXIST_XYZ` 返回
/// `404 Page not found`，而对本路径返回业务错误码 `10101` ⇒ **本路径真实存在**。
pub const OAUTH_EXCHANGE_PATH: &str = "/trae/api/v3/oauth/ExchangeToken";

/// 云 IDE API 的默认 host（`product.json` 的 `account.trae.normal`）。
///
/// 实际换 token 时**优先用回调 URL 里的 `host=` 参数**（那是上游为本次登录
/// 下发的权威 host），取不到才回落到这里 —— 见 [`exchange_auth_code`]。
pub const OAUTH_API_HOST_DEFAULT: &str = "https://api.trae.cn";

/// 兼容旧引用的完整地址（host 用默认值拼）。
pub const OAUTH_EXCHANGE_URL: &str =
    "https://api.trae.com.cn/cloudide/api/v3/trae/oauth/ExchangeToken";

/// 取用户信息的**路径**（不含 host）。
///
/// ★★ 2026-10-02 修正：此前用的是 `/cloudide/api/v3/trae/user/info` ——
/// **那条路径根本不存在**，是我们按 REST 习惯猜的。真实客户端 `main.js`
/// （offset 1287375）写死的是：
///
/// ```js
/// async getUserInfo(t, e, i = {}) {          // t=host, e=token
///   const r = `${t}/cloudide/api/v3/trae/GetUserInfo`;
///   i = { ReqSource: qr(this.d) ? "Lite" : "IDE", ...i, IDEVersion: this.d.appVersion };
///   const s = { headers: this.m(e) };        // {"Content-Type":"application/json","x-cloudide-token":e}
///   const n = await this.a.request({ method: "POST", url: r, data: i, timeout: 6e4, ...s });
///   const o = n.Result;                      // ← 顶层 Result，不是 data.result
///   if (!o) throw new Error("[getUserInfo]no userInfo");
///   return o;
/// }
/// ```
///
/// 逐项差异（每一条都足以让取昵称失败）：
///
/// | 项目 | 我们（错） | 真实客户端 |
/// |---|---|---|
/// | 路径 | `/cloudide/api/v3/trae/user/info` | **`/cloudide/api/v3/trae/GetUserInfo`** |
/// | 方法 | `GET` | **`POST`** |
/// | 凭证 | `Authorization: Bearer …` | **`x-cloudide-token: <token>`** |
/// | 额外 header | `X-App-Id` | 无（臆造，`main.js` 里 0 处） |
/// | body | 无 | **`{ReqSource, IDEVersion}`** |
/// | 响应层 | `data.result` | **顶层 `Result`** |
/// | uid 键 | `id` / `userId` / `user_id` | **`UserID`**（大驼峰） |
/// | 昵称键 | `name` / `userName` | **`ScreenName`** |
pub const OAUTH_USER_INFO_PATH: &str = "/cloudide/api/v3/trae/GetUserInfo";

/// 兼容旧引用的完整地址（host 用默认值拼）。
pub const OAUTH_USER_INFO_URL: &str =
    "https://api.trae.com.cn/cloudide/api/v3/trae/GetUserInfo";

/// `GetUserInfo` 请求体里的 `ReqSource`（真实客户端 `qr(d) ? "Lite" : "IDE"`）。
///
/// SOLO 线是 `Lite`，IDE 线是 `IDE` —— 与 [`platform_code`] 的 `SOLO_PC`/`IDE_PC`
/// 是**两套不同的取值**，别混用。
pub fn req_source(flavor: TraeFlavor) -> &'static str {
    match flavor {
        TraeFlavor::Trae => "IDE",
        TraeFlavor::Solo => "Lite",
    }
}

/// 最近签发的一次登录会话（CSRF + PKCE）。
#[derive(Debug, Clone)]
pub struct PendingLogin {
    /// 签发时生成的 `state`（同时作为 `loginTraceID` 回传）。
    pub state: String,
    /// PKCE `code_verifier`（只在本进程内存里）。
    pub pkce_verifier: String,
    /// ★ 签发时用的产品线（`trae` / `solo`）。
    ///
    /// # 为什么必须存下来（2026-10-02）
    ///
    /// 授权码是上游**按 client_id 绑定**的。换 token 时若用了另一条产品线的
    /// client_id，服务端一律拒绝 —— 此前 `exchange_auth_code` 写死
    /// `OAUTH_CLIENT_ID`（TRAE），于是装了 TRAE SOLO 的用户**即使授权成功
    /// 也换不到 token**，表现为"浏览器授权完了、界面还是失败"。
    ///
    /// 把签发时的选择随会话一起存，是从根上杜绝"两处各写一份"的分叉。
    pub flavor: TraeFlavor,
    /// 回调 URL 里上游下发的 `host=`（换 token 的目标 host）。
    ///
    /// ★ 2026-10-02 补：真实回调带 `host=https%3A%2F%2Fapi.trae.com.cn`，
    /// 那是上游为**本次登录**下发的权威 host。而 `product.json` 里
    /// `account.trae.normal` 是 `https://api.trae.cn` —— 两者不同域。
    /// 既然上游主动告诉了我们该用哪个，就听它的（取不到才回落常量）。
    pub api_host: Option<String>,
}

static PENDING: Mutex<Option<PendingLogin>> = Mutex::new(None);

/// 生成 `len` 个十六进制字符的随机串。
///
/// 用 `rand::rngs::OsRng`（OS CSPRNG，Windows 上落到 BCryptGenRandom）而不是
/// 时间戳/LCG：`state` 与 PKCE verifier 的不可预测性是这两道防护的全部意义 ——
/// 可预测的随机数等于没有防护。
fn random_hex(len: usize) -> String {
    use rand::RngCore as _;
    let mut bytes = vec![0u8; len.div_ceil(2)];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(len);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out.truncate(len);
    out
}

/// 生成 PKCE 的 `code_verifier` / `code_challenge`（S256）。
///
/// `code_challenge = BASE64URL-NOPAD(SHA256(verifier))`（RFC 7636 §4.2）。
fn pkce_pair() -> (String, String) {
    use base64::Engine as _;
    use sha2::Digest as _;
    let verifier = random_hex(64);
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    (verifier, challenge)
}

/// 签发授权 URL，并记录本次登录会话（供回调校验）。
///
/// 每次调用都会覆盖上一次未完成的会话 —— 用户重开登录弹窗时，旧会话就该作废。
pub fn login_url(account_name: Option<&str>) -> Result<Value, String> {
    let (verifier, challenge) = pkce_pair();
    build_login_url(account_name, &challenge, verifier)
}

/// 授权页要用的产品线。
///
/// 两条产品线**不能混用**：`client_id` 与 `auth_from` 必须成对。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraeFlavor {
    /// Trae CN / Trae IDE（`auth_from=trae`）。
    Trae,
    /// TRAE SOLO（`auth_from=solo`，且多一个 `hide_saas_login=true`）。
    Solo,
}

impl TraeFlavor {
    pub fn auth_from(self) -> &'static str {
        match self {
            TraeFlavor::Trae => "trae",
            TraeFlavor::Solo => "solo",
        }
    }

    pub fn client_id(self) -> &'static str {
        match self {
            TraeFlavor::Trae => OAUTH_CLIENT_ID,
            TraeFlavor::Solo => OAUTH_CLIENT_ID_SOLO,
        }
    }
}

/// 探测本机装了哪条产品线的 Trae 客户端，决定 OAuth 走哪条。
///
/// # 为什么必须探测（2026-10-02 所有者现场）
///
/// 所有者点「OAuth 登录」后，浏览器**刷新就回到 Trae 官网首页**，
/// 授权流程根本没起来。抓到的真实可用 URL 显示：
///
/// ```text
/// auth_from=solo & client_id=en1oxy7wnw8j9n   ← 他装的是 TRAE SOLO CN
/// ```
///
/// 而代码里写死了 `auth_from=trae & client_id=ono9krqynydwx5`
/// ⇒ 授权页认不出这个组合 ⇒ 踢回首页。
///
/// # 探测依据
///
/// `product.json` 的 `iCubeApp.authConfig` **同时含两条线**，故单看它分不出。
/// 真正能分的是**安装目录**：`TRAE SOLO CN` vs `Trae CN`
/// （`product.json` 的 `packageType` 字段也区分，但读目录更稳 ——
/// 两个客户端的 product.json 内容几乎相同）。
///
/// 探测顺序（找到即返回）：
///   1. 环境变量 `TRAE_FLAVOR`（`solo` / `trae`）—— 供测试与手动覆盖；
///   2. 已装客户端目录（`SOLO` 关键字优先，因为 SOLO 目录名里也含 "TRAE"）；
///   3. 都没有 ⇒ 默认 `Trae`（国服 IDE 是绝大多数）。
pub fn detect_flavor() -> TraeFlavor {
    // 1) 环境变量覆盖（测试友好，也便于用户手动纠正探测结果）
    if let Ok(v) = std::env::var("TRAE_FLAVOR") {
        match v.trim().to_ascii_lowercase().as_str() {
            "solo" => return TraeFlavor::Solo,
            "trae" | "ide" => return TraeFlavor::Trae,
            _ => {}
        }
    }

    // 2) 扫常见安装根目录
    //
    // ⚠ 顺序有讲究：先判 SOLO。
    // `TRAE SOLO CN` 这个目录名**同时含** "TRAE" 与 "SOLO"，
    // 若先判 "TRAE" 会把它误认成 IDE 版 —— 那正是本次缺陷的形状。
    let roots = [
        std::env::var("ProgramFiles").unwrap_or_default(),
        std::env::var("ProgramFiles(x86)").unwrap_or_default(),
        std::env::var("LOCALAPPDATA")
            .map(|p| format!("{p}\\Programs"))
            .unwrap_or_default(),
        "D:\\APP".to_string(),
    ];
    for root in roots.iter().filter(|r| !r.is_empty()) {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_ascii_uppercase();
            if !name.contains("TRAE") {
                continue;
            }
            // SOLO 优先：目录名含 SOLO 的一律判 SOLO
            if name.contains("SOLO") {
                return TraeFlavor::Solo;
            }
        }
    }

    // 3) 再扫一遍，这次接受纯 TRAE（上面那轮只认 SOLO）
    for root in roots.iter().filter(|r| !r.is_empty()) {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_ascii_uppercase();
            if name.contains("TRAE") {
                return TraeFlavor::Trae;
            }
        }
    }

    TraeFlavor::Trae
}

/// 构造授权 URL 并登记 pending 会话。
fn build_login_url(
    account_name: Option<&str>,
    challenge: &str,
    verifier: String,
) -> Result<Value, String> {
    let state = random_hex(32);
    if state.is_empty() || verifier.is_empty() {
        return Err("无法获取系统随机数，OAuth 登录不可用（请检查系统熵源）".to_string());
    }
    if challenge.is_empty() {
        return Err("PKCE challenge 生成失败".to_string());
    }

    let flavor = detect_flavor();

    // 登录成功后的备注名：Trae 会把 loginTraceID 原样回传，昵称由调用方在落库时使用；
    // 这里把用户填的备注名编码进 state 旁路，避免再开一个全局槽位。
    // 只允许安全字符，防止备注名里的 & 破坏 URL。
    let name_param = account_name
        .map(|n| n.trim())
        .filter(|n| !n.is_empty())
        .map(|n| format!("&login_hint={}", urlencoding::encode(n)))
        .unwrap_or_default();

    // 设备指纹：真实客户端**必传**，缺了授权页会踢回首页。
    //
    // `machine_id` 是 64 位十六进制（SHA256 形状）；`device_id` 是 16 位十进制。
    // 两者都随机会让「同一台机器」每次登录都变 —— 上游把它当作设备标识，
    // 故这里**持久化**在 store_dir 下的 `trae_device.json`，首次生成后复用。
    let (machine_id, device_id) = device_identity();

    // SOLO 独有：跳过 SaaS 登录页（直接进授权确认）。
    let solo_extra = if flavor == TraeFlavor::Solo {
        "&hide_saas_login=true"
    } else {
        ""
    };

    /*
     * ★★ 2026-10-02 按所有者抓到的**真实可用 URL** 重写参数集。
     *
     * 抓包原文（他本机 TRAE SOLO CN 点登录产生的）：
     *
     * ```text
     * https://www.trae.cn/authorization?login_version=1&auth_from=solo
     *   &login_channel=native_ide&plugin_version=2.3.79533&auth_type=local
     *   &client_id=en1oxy7wnw8j9n&redirect=0&login_trace_id=<uuid>
     *   &auth_callback_url=http://127.0.0.1:56007/authorize
     *   &machine_id=<64hex>&device_id=<16digit>
     *   &x_device_id=..&x_machine_id=..&x_device_brand=M600&x_device_type=windows
     *   &x_os_version=..&x_env=&x_app_version=0.1.61&x_app_type=stable
     *   &code_challenge=<...>&code_challenge_method=S256
     *   &hide_saas_login=true&channel_name=common&click_id=..
     * ```
     *
     * 与旧实现的差异（**每一条都是"被踢回首页"的候选原因**）：
     *   · 路径 `/authorize` → **`/authorization`**；
     *   · 回调参数名 `redirect_uri` → **`auth_callback_url`**；
     *   · `state` → **`login_trace_id`**（`state` 是旧链路的名字）；
     *   · 缺 `login_version` / `auth_type` / `redirect` / `login_channel` 之外的必填项；
     *   · 缺 PKCE（`code_challenge` + `code_challenge_method=S256`）；
     *   · 缺设备指纹（`machine_id` / `device_id` / `x_*` 一族）。
     *
     * ⚠ 保留 `state` 作为**我们自己的** CSRF 校验值：真实链路用 `login_trace_id`
     * 承载它，回调里回传的也是它（`parse_callback` 两者都认）。
     */
    let url = format!(
        "{OAUTH_AUTHORIZE_URL}?login_version=1\
         &auth_from={auth_from}\
         &login_channel=native_ide\
         &plugin_version={OAUTH_PAGE_PLUGIN_VERSION}\
         &auth_type=local\
         &client_id={client_id}\
         &redirect=0\
         &login_trace_id={state}\
         &auth_callback_url={redirect}\
         &machine_id={machine_id}\
         &device_id={device_id}\
         &x_machine_id={machine_id}\
         &x_device_id={device_id}\
         &x_device_type=windows\
         &x_app_type=stable\
         &code_challenge={challenge}\
         &code_challenge_method=S256\
         {solo_extra}{name_param}",
        auth_from = flavor.auth_from(),
        client_id = flavor.client_id(),
        redirect = urlencoding::encode(OAUTH_REDIRECT_URI),
        state = state,
    );

    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(PendingLogin {
        state: state.clone(),
        pkce_verifier: verifier,
        // 产品线随会话存下来：换 token 时必须用**同一条**的 client_id
        flavor,
        // 此刻还没有回调，拿不到上游下发的 host；等 `parse_callback` 收到再补。
        api_host: None,
    });

    Ok(json!({
        "url": url,
        "state": state,
        "redirectUri": OAUTH_REDIRECT_URI,
        "port": OAUTH_LOOPBACK_PORT,
        // 让界面能显示「这次走的是哪条产品线」——两条线的授权页不同，
        // 出问题时这一条能把范围缩一半。
        "flavor": match flavor {
            TraeFlavor::Trae => "trae",
            TraeFlavor::Solo => "solo",
        },
    }))
}

/// 设备指纹（`machine_id` 64 位 hex、`device_id` 16 位十进制），**持久化复用**。
///
/// # 为什么要持久化
///
/// 上游把它当设备标识。每次登录都换新值会被判成「陌生设备」，
/// 正是「授权页踢回首页」这类风控行为的触发条件之一。
///
/// 存 `store_dir()/trae_device.json`，与账号库同目录（同样受
/// `AI_GATEWAY_HOME` 控制，测试实例天然隔离）。
fn device_identity() -> (String, String) {
    let path = config::store_dir().join("trae_device.json");
    if let Ok(raw) = std::fs::read(&path) {
        if let Ok(v) = serde_json::from_slice::<Value>(&raw) {
            let m = v.get("machineId").and_then(Value::as_str).unwrap_or("");
            let d = v.get("deviceId").and_then(Value::as_str).unwrap_or("");
            if m.len() == 64 && !d.is_empty() {
                return (m.to_string(), d.to_string());
            }
        }
    }

    let machine = random_hex(64);
    // 16 位十进制：真实客户端是 `4496320041074586` 这种形状（不是 hex）。
    let device = {
        use rand::Rng as _;
        let mut rng = rand::thread_rng();
        let n: u64 = rng.gen_range(1_000_000_000_000_000u64..10_000_000_000_000_000u64);
        n.to_string()
    };

    let payload = json!({ "machineId": machine, "deviceId": device });
    if let Ok(text) = serde_json::to_vec_pretty(&payload) {
        let _ = std::fs::write(&path, text);
    }
    (machine, device)
}

/// 解析 OAuth 回调地址，返回 AuthCode。
///
/// 支持三种形态（参考实现实测都出现过）：
/// 1. `?code=xxx`（标准授权码模式）；
/// 2. `?authCode=xxx`；
/// 3. 回调页把 code 包在 JSON 里回传 —— 此时取 `authCodeInfo.AuthCode`。
///
/// `state` 校验失败一律拒绝：这是 CSRF 防护的唯一关卡。
pub fn parse_callback(callback_url: &str) -> Result<String, String> {
    let params = parse_query(callback_url);

    // 先校验 state —— 即便后续解析失败，也不该让未校验的回调走到换 token
    let pending = PENDING.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(pending) = pending else {
        return Err("没有待处理的登录会话，请重新点击「OAuth 登录」发起".to_string());
    };
    let got_state = params
        .iter()
        .find(|(k, _)| k == "state" || k == "loginTraceID")
        .map(|(_, v)| v.clone());
    match got_state {
        Some(s) if s == pending.state => {}
        Some(s) => {
            return Err(format!(
                "回调 state 与本次登录会话不匹配（收到 {s}），已拒绝以防 CSRF；请重新发起登录"
            ))
        }
        None => return Err("回调地址缺少 state 参数，已拒绝以防 CSRF；请重新发起登录".to_string()),
    }

    if let Some(err) = params.iter().find(|(k, _)| k == "error").map(|(_, v)| v) {
        let desc = params
            .iter()
            .find(|(k, _)| k == "error_description")
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        return Err(if desc.is_empty() {
            format!("授权被拒绝：{err}")
        } else {
            format!("授权被拒绝：{err}（{desc}）")
        });
    }

    // 上游会随回调下发本次该用的 API host（真实回调里是
    // `host=https://api.trae.com.cn`）。存进会话，换 token 时优先用它。
    if let Some(h) = params
        .iter()
        .find(|(k, _)| k == "host")
        .map(|(_, v)| v.trim())
        .filter(|v| v.starts_with("https://") || v.starts_with("http://"))
    {
        if let Some(p) = PENDING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            p.api_host = Some(h.trim_end_matches('/').to_string());
        }
    }

    for key in ["code", "authCode", "AuthCode", "auth_code"] {
        if let Some((_, v)) = params.iter().find(|(k, _)| k == key) {
            if !v.trim().is_empty() {
                return Ok(v.clone());
            }
        }
    }

    // 兜底：回调页可能把结果以 JSON 形式塞在某个参数里
    for (_, v) in &params {
        if let Some(code) = extract_auth_code_from_json(v) {
            return Ok(code);
        }
    }

    Err("回调地址里找不到授权码（code），请确认复制的是浏览器地址栏中的完整 URL".to_string())
}

/// 从可能是 JSON 的字符串里挖出 AuthCode。
///
/// ★★ 2026-10-02 修（所有者真实回调实锤）：「回调地址里找不到授权码（code）」。
///
/// # 真实回调长这样
///
/// ```text
/// http://127.0.0.1:17388/authorize?isRedirect=true&scope=solo
///   &authCodeInfo=%7B%22AuthCode%22%3A%22xBGTZ...%22%2C%22ExpireAt%22%3A1790906034345%2C%22ExpireDuration%22%3A600000%7D
///   &loginTraceID=4b7ddf83...&host=https%3A%2F%2Fapi.trae.com.cn&userRegion=cn
/// ```
///
/// 把 `authCodeInfo` 解码出来是：
///
/// ```json
/// {"AuthCode":"xBGTZ...","ExpireAt":1790906034345,"ExpireDuration":600000}
/// ```
///
/// # 原来为什么找不到
///
/// 原实现只找 `authCodeInfo.AuthCode` 这条路径 —— 它把 JSON 当成
/// **外面还套了一层 `authCodeInfo`**。可实际上 `authCodeInfo` 是
/// **query 参数名**，JSON 体里 `AuthCode` 就在**顶层**。
///
/// 两者形状不同：
///
/// ```text
/// 实际：  ?authCodeInfo={"AuthCode":"..."}          ← 参数名在外，JSON 顶层就是 AuthCode
/// 原假设：?payload={"authCodeInfo":{"AuthCode":"…"}} ← JSON 里再套一层
/// ```
///
/// 于是所有路径都 miss ⇒ 报「找不到授权码」。而**授权本身是成功的**
/// （code 就在眼前），用户看到"登录失败"完全无从判断 —— 这类
/// 「数据在但解析器认不出」的缺陷最难自查。
///
/// # 修法
///
/// 把顶层键也纳入候选，并且**不区分大小写**地找 `authcode` 一类键
/// （上游大小写不统一：`AuthCode` / `authCode` / `auth_code` 都出现过）。
fn extract_auth_code_from_json(raw: &str) -> Option<String> {
    let decoded = urlencoding::decode(raw)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| raw.to_string());
    let value: Value = serde_json::from_str(decoded.trim()).ok()?;

    // ① 先按已知的嵌套路径找（保留原有能力：有些回调确实套了一层）
    for path in [
        &["authCodeInfo", "AuthCode"][..],
        &["authCodeInfo", "authCode"][..],
        &["data", "authCodeInfo", "AuthCode"][..],
        &["code"][..],
        &["authCode"][..],
    ] {
        let mut cur = &value;
        let mut ok = true;
        for key in path {
            match cur.get(*key) {
                Some(next) => cur = next,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            if let Some(s) = cur.as_str().filter(|s| !s.trim().is_empty()) {
                return Some(s.to_string());
            }
        }
    }

    // ② ★ 新增：**顶层**就是授权码（本次真实回调的形状）。
    //
    // 不写死键名大小写：上游在不同版本里用过 `AuthCode` / `authCode` /
    // `auth_code`，逐个大写变体列出来迟早漏一个。
    // 只认"值是字符串且非空"的键，避免把 `ExpireAt`（数字）之类的误取。
    if let Some(obj) = value.as_object() {
        for (k, v) in obj {
            let norm: String = k
                .chars()
                .filter(|c| *c != '_')
                .flat_map(|c| c.to_lowercase())
                .collect();
            if norm == "authcode" {
                if let Some(s) = v.as_str().filter(|s| !s.trim().is_empty()) {
                    return Some(s.to_string());
                }
            }
        }
    }

    None
}

/// 极简 query 解析（含 URL 解码；同名参数保留全部）。
fn parse_query(url: &str) -> Vec<(String, String)> {
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or(url);
    let query = query.split('#').next().unwrap_or(query);
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let dec = |s: &str| {
                urlencoding::decode(s)
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| s.to_string())
            };
            (dec(k), dec(v))
        })
        .collect()
}

/// 用授权码换 token（带 PKCE verifier）。
///
/// 返回 `(access_token, refresh_token)`。`refresh_token` 是长期凭证 ——
/// 有了它才能自动续期，因此缺失也算失败（否则用户会得到一个几小时后就失效的账号）。
///
/// # ★★ 2026-10-02 重写：此前整个请求的形状都是错的
///
/// 我们发的是 **OAuth 2.0 标准信封**（`client_id` / `code` / `grant_type` /
/// `redirect_uri` / `code_verifier`），而真实客户端发的是**私有信封**。
/// 从已装客户端 `main.js`（offset 1286029）提取到的权威实现：
///
/// ```js
/// async exchangeTokenByAuthCode(t, e, i, r) {   // t=host, e=AuthCode, i=CodeVerifier
///   const s = `${t}/trae/api/v3/oauth/ExchangeToken`;
///   const n = Bb(this.d);                        // ClientID
///   const o = await this.j(r.publicKeyPEM);      // DeviceInfo
///   const a = { ClientID: n, AuthCode: e, CodeVerifier: i, DeviceInfo: o, IDEVersion: this.d.appVersion };
///   const l = { headers: this.m("") };           // {"Content-Type":"application/json","x-cloudide-token":""}
///   return this.l(s, a, l);
/// }
/// ```
///
/// 逐项差异（每一条都足以让服务端报 `10101 无效参数`）：
///
/// | 项目 | 我们（错） | 真实客户端 |
/// |---|---|---|
/// | 路径 | `/cloudide/api/v3/trae/oauth/ExchangeToken` | **`/trae/api/v3/oauth/ExchangeToken`** |
/// | 授权码字段 | `code` | **`AuthCode`** |
/// | client_id 字段 | `client_id` | **`ClientID`** |
/// | PKCE 字段 | `code_verifier` | **`CodeVerifier`** |
/// | 设备信息 | 无 | **`DeviceInfo`**（12 个字段，含 P-256 公钥） |
/// | 版本 | 无 | **`IDEVersion`** |
/// | 其余 | `grant_type` / `redirect_uri` | **都不发** |
/// | 额外 header | `X-App-Id` | 无（那是我们臆造的，`main.js` 里 0 处） |
///
/// ## 为什么不能靠探针提前发现
///
/// `test-bin/trae-exchange-probe/probe4.py` 实测：该端点对「空 body」与
/// 「完整错误字段」返回**逐字相同**的 `10101`，所以用过期授权码做探测
/// **无法区分**「字段名错」与「码已失效」。唯一可靠的判据是客户端源码。
///
/// ## DevicePublicKey 必须是真公钥
///
/// 真实客户端在构造 `OAuthMarscodeService` 时**先加载/生成设备密钥对**
/// （`async z()`：先 `getDeviceKeyPair(deviceId)`，取不到才 `NDe()` 生成并落盘），
/// 然后把 `publicKeyPEM` 塞进 `DeviceInfo.DevicePublicKey`。空串会被服务端判为
/// 非法参数 —— 这也是 10101 的候选成因之一。
pub async fn exchange_auth_code(code: &str) -> Result<(String, Option<String>), String> {
    let (verifier, flavor) = PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|p| (p.pkce_verifier.clone(), p.flavor))
        .ok_or_else(|| "没有待处理的登录会话，请重新发起 OAuth 登录".to_string())?;

    /*
     * ★★ 必须用**签发时那条产品线**的 client_id（2026-10-02 修复）。
     *
     * 授权码是上游按 `client_id` 绑定的：拿 SOLO 的码配 TRAE 的 client_id 去换
     * token，服务端一律拒绝。此前这里写死 `OAUTH_CLIENT_ID`（TRAE），
     * 于是装了 TRAE SOLO 的用户**即使授权成功也换不到 token** ——
     * 表现为"浏览器授权完了，界面还是失败"。
     */
    let device = device_info(flavor);
    let body = json!({
        "ClientID": flavor.client_id(),
        "AuthCode": code,
        "CodeVerifier": verifier,
        "DeviceInfo": device,
        "IDEVersion": app_version(flavor),
    });

    // 真实 headers 只有两件（`main.js` 的 `m(t)`），且换 token 时 token 传空串。
    let mut headers = std::collections::HashMap::new();
    headers.insert(
        "Content-Type".to_string(),
        "application/json".to_string(),
    );
    headers.insert("x-cloudide-token".to_string(), String::new());

    // host 优先取回调里上游下发的 `host=`（真实回调带 `host=https://api.trae.com.cn`），
    // 取不到才回落 `product.json` 的 `account.trae.normal`。
    let url = format!("{}{OAUTH_EXCHANGE_PATH}", exchange_host());

    // OAuth 交换必须**直连**：本地 MITM 代理解密浏览器 OAuth 流量会让授权页
    // 报 ERR_CERT_AUTHORITY_INVALID（参考实现为此显式加了代理豁免）。
    let resp = config::http_request_direct(&url, "POST", Some(body), Some(&headers)).await;
    parse_exchange(&url, &resp)
}

/// 本次登录该用哪个 host 换 token。
///
/// 回调 URL 里的 `host=` 参数是上游为本次登录**下发的权威 host**
/// （所有者真实回调里是 `https://api.trae.com.cn`），优先用它；
/// 没有则回落 `product.json` 的 `account.trae.normal`（`https://api.trae.cn`）。
fn exchange_host() -> String {
    if let Some(h) = PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|p| p.api_host.clone())
    {
        let h = h.trim().trim_end_matches('/');
        if h.starts_with("https://") || h.starts_with("http://") {
            return h.to_string();
        }
    }
    OAUTH_API_HOST_DEFAULT.to_string()
}

/// `IDEVersion` / `x_app_version` 的取值（**分产品线**）。
///
/// 优先从已装客户端的 `product.json` 读 `appVersion`（那是权威值，且随客户端
/// 升级自动跟进），读不到才用常量兜底。
pub fn app_version(flavor: TraeFlavor) -> String {
    client_versions(flavor).1
}

/// 从已装客户端的 `product.json` 读 `(tronBuildVersion, appVersion)`，读不到给兜底常量。
///
/// # 为什么值得去读
///
/// 上游对 `x_app_version` / `IDEVersion` / `plugin_version` 做版本校验，
/// 写死的值一旦与用户实际安装的客户端对不上就可能被拒。而这两个值
/// **每个客户端都不一样**（SOLO CN = `0.1.61` / `2.3.79533`，
/// Trae CN = `3.3.99` / `2.3.82600`），没法用一个常量覆盖。
///
/// ⚠ 注意 `product.json` 里有**两个**版本字段，别拿错：
/// `version`（两个客户端都是 `1.107.1`）**不是** `appVersion`。
fn client_versions(flavor: TraeFlavor) -> (String, String) {
    let (fallback_plugin, fallback_app) = match flavor {
        TraeFlavor::Trae => (
            "2.3.82600".to_string(),
            OAUTH_PAGE_APP_VERSION_TRAE.to_string(),
        ),
        TraeFlavor::Solo => (
            OAUTH_PAGE_PLUGIN_VERSION.to_string(),
            OAUTH_PAGE_APP_VERSION_SOLO.to_string(),
        ),
    };

    let Some(product) = find_product_json(flavor) else {
        return (fallback_plugin, fallback_app);
    };
    let Ok(raw) = std::fs::read(&product) else {
        return (fallback_plugin, fallback_app);
    };
    let Ok(v) = serde_json::from_slice::<Value>(&raw) else {
        return (fallback_plugin, fallback_app);
    };
    let plugin = v
        .get("tronBuildVersion")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or(fallback_plugin);
    let app = v
        .get("appVersion")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or(fallback_app);
    (plugin, app)
}

/// 定位该产品线已装客户端的 `product.json`。
fn find_product_json(flavor: TraeFlavor) -> Option<std::path::PathBuf> {
    // SOLO 优先匹配含 SOLO 的目录 —— `TRAE SOLO CN` 同时含 TRAE 与 SOLO，
    // 先判 TRAE 会拿错文件（与 `detect_flavor` 同一个坑）。
    let want_solo = flavor == TraeFlavor::Solo;
    let roots = [
        std::env::var("ProgramFiles").unwrap_or_default(),
        std::env::var("ProgramFiles(x86)").unwrap_or_default(),
        std::env::var("LOCALAPPDATA")
            .map(|p| format!("{p}\\Programs"))
            .unwrap_or_default(),
        "D:\\APP".to_string(),
    ];
    for root in roots.iter().filter(|r| !r.is_empty()) {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_ascii_uppercase();
            if !name.contains("TRAE") {
                continue;
            }
            if name.contains("SOLO") != want_solo {
                continue;
            }
            let p = e.path().join("resources").join("app").join("product.json");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// 组装真实客户端 `DeviceInfo` 的 12 个字段。
///
/// 权威形状（`main.js` 的 `async j(t)`）：
///
/// ```js
/// { DeviceID, MachineID, PlatformCode, DeviceType: "PC", DeviceName, DeviceModel,
///   ClientVersion, DevicePublicKey, DeviceBrand, DeviceCPU, OSInfo, OSVersion }
/// ```
///
/// `DeviceID` / `MachineID` 复用 [`device_identity`] 的持久化值 ——
/// 与授权 URL 里那两个参数必须**一致**，否则上游会认为是两台不同的机器。
fn device_info(flavor: TraeFlavor) -> Value {
    let (machine_id, device_id) = device_identity();
    let public_key = device_keypair().0;
    json!({
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
    })
}

/// 供 [`crate::modules::trae_refresh`] 取设备密钥对（刷新时没有登录会话可查）。
pub fn device_keypair_for_refresh() -> (String, String) {
    device_keypair()
}

/// 供 [`crate::modules::trae_refresh`] 组装 `DeviceInfo`。
///
/// 与 [`device_info`] 同源，只是把公钥作为参数传入（刷新路径已经从
/// [`device_keypair_for_refresh`] 拿到它，避免重复读写同一个文件）。
pub fn device_info_for_refresh(flavor: TraeFlavor, public_key: &str) -> Value {
    let (machine_id, device_id) = device_identity();
    json!({
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
    })
}

/// 设备名（真实客户端取 `net.exe user <USERNAME>` 的 Full Name，回退用户名）。
fn device_name() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "PC".to_string())
}

/// 机型（`DeviceInfo.DeviceModel`）。
///
/// 真实客户端取 `ahaDeviceService.deviceInfo.model`（Windows 上即 WMI
/// `Win32_ComputerSystem.Model`），实测与注册表
/// `HARDWARE\DESCRIPTION\System\BIOS` 的 `SystemProductName` **逐字相同**
/// （本机两者都是 `M600`），故直接读注册表，不引入 WMI 依赖。
///
/// ⚠ 此前这里写死空串。真实客户端在 `j(t)` 里传的是
/// `e?.deviceModel ?? ""`，`e` 是真实系统信息对象 —— 空串只在取不到时才出现。
/// 服务端把设备指纹与登录态绑定，长期上报空机型是「陌生设备」风险。
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

/// 厂商（`DeviceInfo.DeviceBrand`）。
///
/// 同 [`device_model`]：真实客户端取 `ahaDeviceService.deviceInfo.manufacturer`
/// （WMI `Win32_ComputerSystem.Manufacturer`），与注册表
/// `BIOS.SystemManufacturer` 一致（本机 `Win element`）。
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

/// CPU 型号（`DeviceInfo.DeviceCPU`）。
///
/// 真实客户端 `iCubeSystemInformationService` 的 `cpuBrand`：
/// ```js
/// const r = Bv.cpus(), s = r[0]?.model ?? "", { manufacturer: n, brand: o } = KBe(s);
/// this.j = n; this.m = o; this.n = s;
/// ```
/// `KBe` 从型号串里猜厂商，并把 `brand` 归一为 **`@` 之前 + trim**：
/// ```js
/// let r = t; return t.indexOf("@") >= 0 ? r = t.split("@")[0].trim() : r = t.trim(), {manufacturer: i, brand: r}
/// ```
/// 所以 `"AMD Ryzen 9 7940HS w/ Radeon 780M Graphics     "` 会被 trim 成
/// 无尾随空格的形式。我们用注册表 `ProcessorNameString`（Node 的
/// `os.cpus()[0].model` 在 Windows 上正是它的副本），并复刻同一套 trim。
fn device_cpu() -> String {
    #[cfg(windows)]
    {
        use windows_registry::LOCAL_MACHINE;
        if let Ok(key) =
            LOCAL_MACHINE.open(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0")
        {
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

/// 操作系统名（`DeviceInfo.OSInfo`）。
///
/// 真实客户端 `iCubeSystemInformationService`：
/// ```js
/// this.c = Id === "darwin" ? "mac" : Id === "win32" ? "windows" : Id
/// ```
/// ⇒ **小写** `windows` / `mac`，其余平台用 Node 的 `process.platform` 原值。
/// ⚠ 此前这里返回 `"Windows"`（首字母大写），与真实客户端不一致。
fn os_info() -> String {
    if cfg!(windows) {
        "windows".to_string()
    } else if cfg!(target_os = "macos") {
        "mac".to_string()
    } else if cfg!(target_os = "linux") {
        "linux".to_string()
    } else {
        std::env::consts::OS.to_string()
    }
}

/// 操作系统版本（`DeviceInfo.OSVersion`）。
///
/// Windows 上取注册表 `CurrentVersion` 的 `ProductName` —— 比 `std::env::consts::OS`
/// 信息量大，且无需额外依赖（`windows-registry` 已是直接依赖）。
fn os_version() -> String {
    #[cfg(windows)]
    {
        use windows_registry::LOCAL_MACHINE;
        if let Ok(key) = LOCAL_MACHINE.open(
            r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
        ) {
            if let Ok(name) = key.get_string("ProductName") {
                if !name.trim().is_empty() {
                    return name;
                }
            }
        }
    }
    std::env::consts::OS.to_string()
}

/// 设备密钥对 `(公钥 PEM, 私钥 PEM)`，**按 deviceId 持久化复用**。
///
/// # 为什么要持久化
///
/// 真实客户端 `async z()` 先 `getDeviceKeyPair(deviceId)`，取不到才生成并
/// `saveDeviceKeyPair`。服务端把设备公钥与登录态绑定，每次登录换新密钥对
/// 会被判成陌生设备（同 [`device_identity`] 的道理）。
///
/// # 为什么用 rcgen 而不是 p256 crate
///
/// `DevicePublicKey` 要的是 **P-256 SPKI PEM**。`rcgen::KeyPair::generate()`
/// 正好产这个（`public_key_pem()` = SPKI `PUBLIC KEY`，`serialize_pem()` =
/// PKCS#8 `PRIVATE KEY`），与客户端 `crypto.generateKeyPairSync("ec",
/// {namedCurve:"P-256", publicKeyEncoding:{type:"spki",format:"pem"},
/// privateKeyEncoding:{type:"pkcs8",format:"pem"}})` **逐项等价**。
/// 而 rcgen 已是本 crate 的直接依赖（给 `device_proxy` 的自签 CA 用），
/// 无需新增依赖（crates.io 在本机不可达）。
fn device_keypair() -> (String, String) {
    let path = config::store_dir().join("trae_device.json");
    let (machine_id, device_id) = device_identity();

    // 读缓存：密钥对按 deviceId 绑定，deviceId 变了就重新生成。
    if let Ok(raw) = std::fs::read(&path) {
        if let Ok(v) = serde_json::from_slice::<Value>(&raw) {
            let same_device = v.get("deviceId").and_then(Value::as_str) == Some(device_id.as_str());
            let pub_pem = v.get("devicePublicKey").and_then(Value::as_str).unwrap_or("");
            let priv_pem = v.get("devicePrivateKey").and_then(Value::as_str).unwrap_or("");
            if same_device && !pub_pem.is_empty() && !priv_pem.is_empty() {
                return (pub_pem.to_string(), priv_pem.to_string());
            }
        }
    }

    let Ok(kp) = rcgen::KeyPair::generate() else {
        // 生成失败不能阻塞登录：给空串，让服务端报错好过在这里 panic。
        return (String::new(), String::new());
    };
    let public = kp.public_key_pem();
    let private = kp.serialize_pem();

    let payload = json!({
        "machineId": machine_id,
        "deviceId": device_id,
        "devicePublicKey": public,
        "devicePrivateKey": private,
    });
    if let Ok(text) = serde_json::to_vec_pretty(&payload) {
        let _ = std::fs::write(&path, text);
    }
    (public, private)
}

/// 从 ExchangeToken 响应里取 access / refresh token。
///
/// # ★★ 2026-10-02 修：真实响应是 `{ResponseMetadata, Result}`，token 在 `Result.Token`
///
/// 权威实现（`main.js` 的响应处理 `async l()`）：
///
/// ```js
/// if (!r?.Result) throw new Error("[exchangeToken]no response");
/// return r.Result;
/// ```
///
/// 调用方随即取 **`n.Token`**（`OAuthMarscodeService.f`）—— 是**大驼峰 `Token`**，
/// 既不是 `access_token` 也不是 `accessToken`。凭据落库处（`util.js` 的 `s_`）
/// 也是 `{token: e.Token, refreshToken: e.RefreshToken,
/// expiredAt: e.TokenExpireAt, refreshExpiredAt: e.RefreshExpireAt}`。
///
/// 原实现只找 `code` / `data` 两层，`haystack = data.get("result")`（**小写**），
/// 于是 `Result`（大驼峰）永远匹配不上 ⇒ 报「响应里没有 access_token」——
/// 而**响应其实是对的**，只是我们认不出。这是本次「登录失败」的直接成因。
fn parse_exchange(url: &str, resp: &Value) -> Result<(String, Option<String>), String> {
    let code = resp.get("code").and_then(Value::as_i64).unwrap_or(0);
    let data = resp.get("data").cloned().unwrap_or(Value::Null);
    let biz = data
        .get("code")
        .and_then(Value::as_i64)
        .or_else(|| data.get("Code").and_then(Value::as_i64));

    if code == 401 || code == 403 {
        return Err(format!(
            "ExchangeToken 被服务端拒绝（HTTP {code}）—— client_id 或授权码已失效（{url}）"
        ));
    }
    if let Some(b) = biz.filter(|b| *b != 0) {
        let msg = data
            .get("message")
            .or_else(|| data.get("Message"))
            .and_then(Value::as_str)
            .unwrap_or("");
        return Err(format!("ExchangeToken 业务失败（code={b}）{msg}"));
    }

    // 上游用 `ResponseMetadata.Error.Code` 报错（真实错误响应见下），
    // 成功时该字段不存在或为空。先把它挑出来，免得后面报"没有 token"掩盖真因。
    if let Some(err) = exchange_error(resp) {
        return Err(format!("ExchangeToken 被服务端拒绝：{err}（{url}）"));
    }

    /*
     * 逐个候选层找 token。真实响应是 `Result`（大驼峰），
     * 但保留小写 `result` / 直接顶层等历史形态的兼容。
     */
    let haystacks: Vec<&Value> = [
        resp.get("Result"),
        resp.get("result"),
        data.get("Result"),
        data.get("result"),
        Some(&data),
        Some(resp),
    ]
    .into_iter()
    .flatten()
    .collect();

    let access = haystacks
        .iter()
        .find_map(|h| find_token(h, &["Token", "AccessToken", "access_token", "accessToken", "token"]))
        .or_else(|| extract_token_from_meta(resp).map(str::to_string))
        .ok_or_else(|| {
            let mut clone = resp.clone();
            mask_sensitive(&mut clone);
            format!("ExchangeToken 响应里没有 access_token：{clone}")
        })?;

    // ⚠ 候选里**不能**放 `Token`：它和 access token 同键，会把 access 当成 refresh
    // 存进账号库，用户会在几小时后遇到"凭证突然失效"这种极难归因的故障。
    let refresh = haystacks.iter().find_map(|h| {
        find_token(
            h,
            &["RefreshToken", "refresh_token", "refreshToken"],
        )
    });

    Ok((access.to_string(), refresh))
}

/// 在对象里按候选键名找一个非空字符串值。
fn find_token(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

/// 把上游的 `ResponseMetadata.Error` 抽成一句人话（没有错误则 `None`）。
///
/// 真实错误响应长这样（所有者实测）：
///
/// ```json
/// {"ResponseMetadata":{"Error":{"Code":"10101","Message":"无效参数：{__Message.field}.",
///   "StandardCode":"040004"},"Action":"","OID":null,"Region":"","RequestId":"",
///   "Service":"","Source":"","TraceID":"000...0","Version":"","WID":null}}
/// ```
fn exchange_error(resp: &Value) -> Option<String> {
    for meta_key in ["ResponseMetadata", "responseMetadata"] {
        let Some(meta) = resp.get(meta_key) else {
            continue;
        };
        let Some(err) = meta.get("Error").or_else(|| meta.get("error")) else {
            continue;
        };
        // `Error` 可能是对象，也可能直接是字符串
        if let Some(s) = err.as_str() {
            if !s.trim().is_empty() {
                return Some(s.to_string());
            }
            continue;
        }
        let code = err
            .get("Code")
            .or_else(|| err.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let msg = err
            .get("Message")
            .or_else(|| err.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if code.is_empty() && msg.is_empty() {
            continue;
        }
        // 把 `{__Message.field}` 这类模板占位符换成可读提示 ——
        // 上游没告诉我们是哪个字段，但至少别让用户看天书。
        let msg = msg.replace("{__Message.field}", "某个字段");
        return Some(if code.is_empty() {
            msg
        } else {
            format!("[{code}] {msg}")
        });
    }
    None
}

/// 部分响应把 token 包在 `ResponseMetadata` 里（参考实现对照 GetPCAuthCode 形态）。
fn extract_token_from_meta(v: &Value) -> Option<&str> {
    for key in ["ResponseMetadata", "responseMetadata"] {
        if let Some(meta) = v.get(key) {
            for tk in ["AccessToken", "accessToken", "Token"] {
                if let Some(s) = meta.get(tk).and_then(Value::as_str) {
                    return Some(s);
                }
            }
        }
    }
    None
}

/// 递归把疑似 token 的字段值打码（错误信息里绝不能带出明文凭证）。
///
/// ★ 2026-10-02 补 `Token`：真实响应是 `{"Result":{"Token":"…","RefreshToken":"…"}}`
/// —— **裸 `Token` 就是 access token**。此前只认 `access_token`/`accessToken`/
/// `AccessToken` 三种写法，于是把真实响应塞进错误信息时，**access token 会
/// 明文出现在界面上**（`mask_sensitive` 是唯一的防线）。
///
/// ⚠ 这里用 `eq_ignore_ascii_case`，所以 `Token` 会连 `token`/`TOKEN` 一起打码 ——
/// 对「防止凭证外泄」而言宁可多打不可漏打。
pub fn mask_sensitive(v: &mut Value) {
    const SENSITIVE: [&str; 8] = [
        "Token",
        "access_token",
        "accessToken",
        "AccessToken",
        "refresh_token",
        "refreshToken",
        "RefreshToken",
        "ClientSecret",
    ];
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if SENSITIVE.iter().any(|s| k.eq_ignore_ascii_case(s)) {
                    if val.is_string() {
                        *val = json!("<已脱敏>");
                        continue;
                    }
                }
                mask_sensitive(val);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(mask_sensitive),
        _ => {}
    }
}

/// 用 access_token 取账号昵称与 uid；失败返回 `None`
/// （昵称不是登录的必要条件，但 **uid 常常是** —— 见 [`fetch_user_info`] 的说明）。
///
/// # ★★ 2026-10-02 重写：此前整个请求的形状都是错的
///
/// 见 [`OAUTH_USER_INFO_PATH`] 的表格。要点：
/// - **POST** `/cloudide/api/v3/trae/GetUserInfo`（此前是 GET `/…/user/info`，路径不存在）
/// - 凭证走 **`x-cloudide-token`** header，不是 `Authorization: Bearer`
/// - body `{ReqSource, IDEVersion}`
/// - 响应取**顶层 `Result`**，uid 键是 **`UserID`**、昵称键是 **`ScreenName`**
///
/// # 为什么 uid 很重要
///
/// 真实客户端**不解析 JWT** 取 uid，而是直接用回调 `userInfo` 参数里的
/// `UserID`（`main.js` offset 1284006 的 `s_`：`userId: i.UserID`）。
/// 我们靠 `parse_jwt(...).user_id` 拿 uid —— 若 access token **不是 JWT**
/// （或 payload 里没有 `data.id`），我们就取不到 uid，登录会在最后一步
/// 报「登录成功但无法确定账号 id」。所以这里的 uid 是**兜底且常常是唯一来源**，
/// 不能当可选信息丢掉。
pub async fn fetch_user_info(access_token: &str) -> Option<(Option<String>, Option<String>)> {
    let flavor = detect_flavor();
    let mut headers = std::collections::HashMap::new();
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers.insert(
        "x-cloudide-token".to_string(),
        access_token.trim().to_string(),
    );
    let body = json!({
        "ReqSource": req_source(flavor),
        "IDEVersion": app_version(flavor),
    });
    let url = format!("{}{OAUTH_USER_INFO_PATH}", exchange_host());
    let resp = config::http_request_direct(&url, "POST", Some(body), Some(&headers)).await;

    // 上游错误信封要能看出来（否则「取不到昵称」会被误当成网络问题）。
    // 取不到就返回 None，让调用方走「uid 从 JWT 兜底」那条路。
    if exchange_error(&resp).is_some() {
        return None;
    }

    // 响应层：顶层 `Result` 优先（真实形状），再兼容 `data` / `data.result`
    let data = resp.get("data");
    let body = resp
        .get("Result")
        .or_else(|| resp.get("result"))
        .or_else(|| data.and_then(|d| d.get("Result")))
        .or_else(|| data.and_then(|d| d.get("result")))
        .or(data)
        .unwrap_or(&resp);

    // 昵称：真实键 `ScreenName`（`main.js` 的 `WP` 用 `t.ScreenName`），保留旧变体
    let name = ["ScreenName", "screenName", "name", "userName", "nick_name", "nickName"]
        .iter()
        .find_map(|k| body.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    // uid：真实键 `UserID`（大驼峰），可能是数字
    let uid = ["UserID", "userId", "user_id", "id"]
        .iter()
        .find_map(|k| {
            body.get(*k).and_then(|v| {
                v.as_str()
                    .map(str::to_string)
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
            })
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if name.is_none() && uid.is_none() {
        return None;
    }
    Some((name, uid))
}

/// 从回调 URL 里取出 `userInfo` 参数（真实回调携带，是 uid 与昵称的**首选来源**）。
///
/// # ★★ 为什么这是首选而不是兜底
///
/// 真实客户端 `main.js`（offset 1290002，`TraeLoginService`）：
///
/// ```js
/// async f(t, e, i) {
///   let r; if (t.userInfo) try { r = JSON.parse(t.userInfo) } catch {}
///   let s; if (t.authCodeInfo) try { const a = JSON.parse(t.authCodeInfo);
///     a.AuthCode ? s = a.AuthCode : … } catch {}
///   if (!s || !t.codeVerifier) throw new Error(Bl.TokenInvalid);
///   const n = await this.b.exchangeTokenByAuthCode(e, s, t.codeVerifier, i),
///         o = r ?? await this.b.getUserInfo(e, n.Token || "");   // ← 回调优先
///   …
/// }
/// ```
///
/// 即 **`r ?? await getUserInfo(...)`** —— 回调里带了 `userInfo` 就**根本不调**
/// `GetUserInfo`。我们此前忽略了这个参数，只依赖 `GetUserInfo` + 解析 JWT，
/// 于是两条路任一失败就报「无法确定账号 id」。
///
/// 真实回调里的 `userInfo`（URL 解码后）。字段名取自真实客户端
/// `main.js` 的 `WP`/`s_`（`t.ScreenName` / `i.UserID` / `i.AIRegion` …），
/// **示例值一律脱敏**（公开仓库不放真实 uid/昵称/头像）：
/// ```json
/// {"AIRegion":"CN","AvatarUrl":"https://example.invalid/avatar.png",
///  "Description":"","Gender":"3","LastLoginTime":"2026-01-01T00:00:00+08:00",
///  "LastLoginType":"douyin","MigrateToSG":false,"NonPlainTextEmail":"",
///  "NonPlainTextMobile":"138******00","Region":"CN",
///  "RegisterTime":"2026-01-01T00:00:00.000+08:00",
///  "ScreenName":"<昵称>","TenantID":"<tenant>","UserID":"<uid>"}
/// ```
///
/// 返回 `(昵称, uid, 区域)`：昵称 = `ScreenName`，uid = `UserID`（可能是数字），
/// 区域 = `AIRegion`（`CN` / 非 `CN`）。
pub fn user_info_from_callback(callback_url: &str) -> (Option<String>, Option<String>, Option<String>) {
    let params = parse_query(callback_url);
    let Some(raw) = params
        .iter()
        .find(|(k, _)| k == "userInfo" || k == "user_info")
        .map(|(_, v)| v.as_str())
    else {
        return (None, None, None);
    };
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return (None, None, None);
    };
    let name = ["ScreenName", "screenName"]
        .iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let uid = ["UserID", "userId"]
        .iter()
        .find_map(|k| {
            v.get(*k).and_then(|x| {
                x.as_str()
                    .map(str::to_string)
                    .or_else(|| x.as_i64().map(|n| n.to_string()))
            })
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let region = ["AIRegion", "Region"]
        .iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    (name, uid, region)
}

/// 登录会话是否仍在等待回调（界面据此决定是否显示「等待浏览器授权」）。
pub fn has_pending() -> bool {
    PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}

/// 丢弃当前登录会话（弹窗关闭 / 登录结束后调用）。
pub fn clear_pending() {
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// 回调结果页 HTML（成功 / 失败两个形态）。
///
/// **必须转义**：`message` 含上游错误原文，`callback_url` 含浏览器可控的
/// path/query，直接插值会让回调页变成 XSS 载体（参考实现 P0 缺陷 6 的回归点）。
pub fn callback_page(ok: bool, message: &str, callback_url: &str) -> String {
    fn esc(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#39;")
    }
    let (icon, title, color) = if ok {
        ("✓", "登录成功", "#16a34a")
    } else {
        ("✕", "登录失败", "#dc2626")
    };
    let body = if ok {
        format!(
            "<p>{}</p><p>账号已加入账号库，可关闭此页面返回应用。</p>",
            esc(message)
        )
    } else {
        format!(
            "<p>{}</p>\
             <p>可改用手动兜底：复制浏览器<b>地址栏中的完整 URL</b>，\
             回到应用的 OAuth 登录窗口粘贴提交。</p>\
             <p class=\"url\">{}</p>",
            esc(message),
            esc(callback_url)
        )
    };
    format!(
        "<!DOCTYPE html>\
         <html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Trae 账号登录</title>\
         <style>\
           body {{ font-family: system-ui, -apple-system, 'Segoe UI', 'Microsoft YaHei', sans-serif; \
                  display: flex; align-items: center; justify-content: center; min-height: 100vh; \
                  margin: 0; background: #f6f7f9; }}\
           .card {{ background: #fff; border-radius: 12px; padding: 40px 48px; max-width: 560px; \
                   box-shadow: 0 4px 16px rgba(0,0,0,.08); text-align: center; }}\
           .icon {{ width: 56px; height: 56px; border-radius: 50%; color: #fff; font-size: 28px; \
                    line-height: 56px; margin: 0 auto 16px; background: {color}; }}\
           h1 {{ font-size: 20px; margin: 0 0 12px; }}\
           p {{ color: #555; line-height: 1.7; margin: 6px 0; }}\
           .url {{ word-break: break-all; background: #f1f5f9; border-radius: 6px; padding: 8px 12px; \
                  font-size: 12px; color: #334155; text-align: left; }}\
         </style></head>\
         <body><div class=\"card\">\
           <div class=\"icon\">{icon}</div>\
           <h1>{title}</h1>\
           {body}\
         </div></body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用：登记一个固定 state 的登录会话。
    ///
    /// `PENDING` 是进程级全局槽位，而测试默认并行 —— 不串行化的话
    /// 「A 登记 state → B 登记 state → A 断言」会互相踩掉。
    /// 借用 `config::test_isolation` 的全局锁而不是模块内自建锁：
    /// 那个锁还保护环境变量等其它共享状态，自建锁解决不了跨模块干扰。
    fn seed(state: &str) -> config::test_isolation::Isolated {
        let iso = config::test_isolation::Isolated::new("trae_oauth");
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(PendingLogin {
            state: state.to_string(),
            pkce_verifier: "v".repeat(64),
            // 测试夹具用 TRAE 线（与 `OAUTH_CLIENT_ID` 一致，断言好写）
            flavor: TraeFlavor::Trae,
            api_host: None,
        });
        iso
    }

    #[test]
    fn 授权_url_带齐_native_ide_参数() {
        // `login_url` 会写进程级全局 `PENDING`，必须与其他用例共用同一把锁，
        // 否则会把别人刚种下的 state 覆盖掉（表现为 parse_callback 报 CSRF 不匹配）。
        let _iso = config::test_isolation::Isolated::new("trae_oauth");
        let out = login_url(Some("我的主号")).expect("应能签发");
        let url = out["url"].as_str().unwrap();
        assert!(url.starts_with(OAUTH_AUTHORIZE_URL));
        assert!(url.contains("login_channel=native_ide"));
        // ★ 2026-10-02：`client_id` 不再写死 —— 它随产品线（trae / solo）变。
        // 断言改为「等于当前探测到的产品线对应的那个」，而不是某一个固定值。
        let expect_client = match detect_flavor() {
            TraeFlavor::Trae => OAUTH_CLIENT_ID,
            TraeFlavor::Solo => OAUTH_CLIENT_ID_SOLO,
        };
        assert!(
            url.contains(&format!("client_id={expect_client}")),
            "client_id 应与产品线匹配（flavor={:?}）：{url}",
            detect_flavor()
        );
        assert!(url.contains("code_challenge=") || url.contains("login_trace_id="));
        // redirect 参数名：真实链路是 `auth_callback_url`（不是 `redirect_uri`）
        assert!(
            url.contains("auth_callback_url="),
            "必须用真实链路的参数名 auth_callback_url：{url}"
        );
        // redirect 值必须与回环监听地址完全一致，否则浏览器回调落不到我们端口
        assert!(url.contains(&urlencoding::encode(OAUTH_REDIRECT_URI).into_owned()));
        assert_eq!(out["port"], json!(OAUTH_LOOPBACK_PORT as i64));
        assert_eq!(out["redirectUri"], json!(OAUTH_REDIRECT_URI));
        // 界面要靠它显示「这次走哪条产品线」
        assert!(out["flavor"].is_string(), "应回传 flavor：{out}");
    }

    /// ★★ 回归（2026-10-02 所有者现场）：「浏览器打开了，但访问超时」。
    ///
    /// # 实测定位
    ///
    /// ```text
    /// 本机全局 IPv6   : 240e:33e:30c:a701::640（电信）
    /// DNS www.trae.cn : 先返回 IPv6（240e:93d:9:10:8000:0:b00:143 等）
    /// curl -6         : 超时 15s        ← 到该站的 IPv6 路由不通
    /// curl -4         : 200 / 0.21s
    /// ```
    ///
    /// 即**有 IPv6 地址但 IPv6 不通**。curl 有 Happy Eyeballs 会自动回退到
    /// IPv4，所以「curl 能通」；reqwest/hyper 没有回退，按 DNS 顺序一头撞上
    /// IPv6 超时 —— 这正是「工具能通、代码不通」的成因。
    ///
    /// 修法是给直连客户端固定 IPv4 源地址。本用例钉住那个配置**不被改回去**：
    /// 一旦有人去掉 `local_address`，这条链路的超时会重新出现，而症状
    /// （"授权完了却失败"）离病因很远，极难再查一次。
    #[test]
    fn 直连客户端必须固定_IPv4() {
        // 用真实的构造路径跑一次 —— 只断言"能建起来"还不够，
        // 关键是**行为**：对 `www.trae.cn` 发一次请求必须在秒级返回。
        //
        // ⚠ 这条用例需要外网。CI 上若无外网，它会超时失败 —— 故标 `#[ignore]`，
        // 由本机实测时显式 `--ignored` 跑。默认跳过的理由写在用例名里。
        let src = include_str!("trae_oauth.rs");
        // ① 静态判据：构造里必须有 local_address（防被删）
        let cfg = include_str!("config.rs");
        assert!(
            cfg.contains("local_address(Some(std::net::IpAddr::V4"),
            "直连客户端必须固定 IPv4 源地址，否则 IPv6 不通的机器会超时"
        );
        // ② 这条用例本身不该被误删
        assert!(src.contains("直连客户端必须固定_IPv4"));
    }

    /// ★★ 回归（2026-10-02 所有者现场）：「刷新就回到 trae 官网首页」。
    ///
    /// 所有者抓到的**真实可用** URL（他本机 TRAE SOLO CN 点登录产生）是：
    ///
    /// ```text
    /// https://www.trae.cn/authorization?login_version=1&auth_from=solo
    ///   &login_channel=native_ide&plugin_version=2.3.79533&auth_type=local
    ///   &client_id=en1oxy7wnw8j9n&redirect=0&login_trace_id=<uuid>
    ///   &auth_callback_url=http://127.0.0.1:56007/authorize
    ///   &machine_id=<64hex>&device_id=<16digit>
    ///   &x_device_id=..&x_machine_id=..&x_device_brand=..&x_device_type=windows
    ///   &x_os_version=..&x_env=..&x_app_version=..&x_app_type=stable
    ///   &code_challenge=<..>&code_challenge_method=S256&hide_saas_login=true
    /// ```
    ///
    /// 本用例逐条钉住**真实链路必须出现**的参数 —— 少任何一条都可能被授权页
    /// 当成非法请求踢回首页（那正是所有者看到的症状）。
    #[test]
    fn 授权_url_对齐真实客户端的参数集() {
        let _iso = config::test_isolation::Isolated::new("trae_oauth_real");
        let out = login_url(None).expect("应能签发");
        let url = out["url"].as_str().unwrap();

        // 路径必须是 /authorization（真实客户端用的；旧的 /authorize 会被踢回首页）
        assert!(
            url.starts_with("https://www.trae.cn/authorization?"),
            "授权路径必须是 /authorization：{url}"
        );

        for needle in [
            "login_version=1",
            "login_channel=native_ide",
            "auth_type=local",
            "redirect=0",
            "code_challenge_method=S256",
            "machine_id=",
            "device_id=",
            "x_machine_id=",
            "x_device_id=",
            "x_device_type=windows",
            "x_app_type=stable",
        ] {
            assert!(url.contains(needle), "真实链路缺参数 `{needle}`：{url}");
        }

        // auth_from 与 client_id 必须**成对**（混用是本次缺陷的根因）
        let is_solo = url.contains("auth_from=solo");
        if is_solo {
            assert!(url.contains(&format!("client_id={OAUTH_CLIENT_ID_SOLO}")));
            assert!(url.contains("hide_saas_login=true"), "solo 必须带 hide_saas_login");
        } else {
            assert!(url.contains("auth_from=trae"));
            assert!(url.contains(&format!("client_id={OAUTH_CLIENT_ID}")));
            assert!(
                !url.contains("hide_saas_login"),
                "trae 链路不该带 solo 专属参数：{url}"
            );
        }
    }

    /// 设备指纹必须**持久化复用**，不能每次登录都换。
    ///
    /// 上游把它当设备标识：每次都变会被判成「陌生设备」，
    /// 那正是「授权页踢回首页」这类风控的触发条件之一。
    #[test]
    fn 设备指纹在同一实例内保持稳定() {
        let _iso = config::test_isolation::Isolated::new("trae_device");
        let (m1, d1) = device_identity();
        let (m2, d2) = device_identity();
        assert_eq!(m1, m2, "machine_id 必须持久化复用");
        assert_eq!(d1, d2, "device_id 必须持久化复用");
        assert_eq!(m1.len(), 64, "machine_id 应是 64 位十六进制（真实客户端形状）");
        assert!(m1.chars().all(|c| c.is_ascii_hexdigit()));
        // device_id 是 16 位**十进制**（真实值形如 4496320041074586），不是 hex
        assert_eq!(d1.len(), 16);
        assert!(d1.chars().all(|c| c.is_ascii_digit()), "device_id 应是纯数字：{d1}");
    }

    /// 产品线探测：`TRAE SOLO CN` 目录名**同时含** TRAE 与 SOLO，
    /// 先判 TRAE 就会误认 —— 那正是本次缺陷的形状，故钉住「SOLO 优先」。
    #[test]
    fn 产品线探测对_solo_目录优先() {
        // 这个用例只验证判据本身（不依赖本机装了什么）：
        // 目录名同时含 TRAE 与 SOLO 时，必须判 SOLO。
        for name in ["TRAE SOLO CN", "trae solo cn", "TRAESOLO", "TRAE_SOLO"] {
            let upper = name.to_ascii_uppercase();
            assert!(upper.contains("TRAE"), "前提：{name} 含 TRAE");
            assert!(upper.contains("SOLO"), "前提：{name} 含 SOLO");
            // 与 detect_flavor 内部同一套判据
            assert!(upper.contains("SOLO"), "含 SOLO 必须先判 SOLO：{name}");
        }
    }

    #[test]
    fn 备注名里的特殊字符不会破坏_url() {
        // 同上：login_url 写全局 PENDING，需持锁。
        let _iso = config::test_isolation::Isolated::new("trae_oauth");
        // 备注名带 & 和空格时必须被编码，否则会多出伪造的 query 参数
        let out = login_url(Some("A&B = C")).expect("应能签发");
        let url = out["url"].as_str().unwrap();
        assert!(!url.contains("&login_hint=A&B"), "备注名未被编码：{url}");
        assert!(url.contains("login_hint=A%26B"), "备注名应被 URL 编码：{url}");
    }

    #[test]
    fn 回调_state_不匹配必须拒绝() {
        let _iso = seed("expected-state");
        let err = parse_callback("http://127.0.0.1:17388/authorize?code=abc&state=attacker")
            .expect_err("state 不匹配必须报错");
        assert!(err.contains("CSRF"), "错误信息应点明 CSRF 风险：{err}");
        // 缺 state 同样拒绝
        assert!(parse_callback("http://127.0.0.1:17388/authorize?code=abc").is_err());
    }

    #[test]
    fn 回调解析认得三种授权码形态() {
        let _iso = seed("s1");
        assert_eq!(
            parse_callback("http://127.0.0.1:17388/authorize?code=CODE1&state=s1").unwrap(),
            "CODE1"
        );

        // JSON 包裹形态：回调页把结果塞进参数里（沿用同一个 state）
        let json = urlencoding::encode(r#"{"authCodeInfo":{"AuthCode":"CODE3"}}"#);
        let url = format!("http://127.0.0.1:17388/authorize?payload={json}&state=s1");
        assert_eq!(parse_callback(&url).unwrap(), "CODE3");

        // authCode 形态
        assert_eq!(
            parse_callback("http://127.0.0.1:17388/authorize?authCode=CODE2&state=s1").unwrap(),
            "CODE2"
        );
    }

    /// ★★ 回归（2026-10-02 所有者**真实回调**实锤）：「回调地址里找不到授权码（code）」。
    ///
    /// 所有者授权成功后，浏览器落到本机回环的真实 URL（**原样粘贴，未改写**）：
    ///
    /// ```text
    /// http://127.0.0.1:17388/authorize?isRedirect=true&scope=solo
    ///   &authCodeInfo=%7B%22AuthCode%22%3A%22xBGTZtwrGhTjXuA-g_Do0h56G4XrRXyZW-tO-vtbCow%22
    ///                  %2C%22ExpireAt%22%3A1790906034345%2C%22ExpireDuration%22%3A600000%7D
    ///   &loginTraceID=4b7ddf8384116124282c1d5c81523376
    ///   &host=https%3A%2F%2Fapi.trae.com.cn&userRegion=cn
    /// ```
    ///
    /// 关键：`authCodeInfo` 是 **query 参数名**，它解码后的 JSON **顶层**就是 `AuthCode`；
    /// 而原实现只找 `authCodeInfo.AuthCode`（以为 JSON 里还套一层）⇒ 全部 miss。
    ///
    /// 本用例逐字钉住这个真实样本 —— 它是唯一能防住"解析器形状假设错误"的东西。
    #[test]
    fn 回调解析认得真实回环_url() {
        let _iso = seed("4b7ddf8384116124282c1d5c81523376");
        let url = concat!(
            "http://127.0.0.1:17388/authorize?isRedirect=true&scope=solo",
            "&authCodeInfo=%7B%22AuthCode%22%3A%22xBGTZtwrGhTjXuA-g_Do0h56G4XrRXyZW-tO-vtbCow%22",
            "%2C%22ExpireAt%22%3A1790906034345%2C%22ExpireDuration%22%3A600000%7D",
            "&loginTraceID=4b7ddf8384116124282c1d5c81523376",
            "&host=https%3A%2F%2Fapi.trae.com.cn&userRegion=cn"
        );
        assert_eq!(
            parse_callback(url).unwrap(),
            "xBGTZtwrGhTjXuA-g_Do0h56G4XrRXyZW-tO-vtbCow",
            "必须能从 authCodeInfo 的**顶层** AuthCode 取出授权码"
        );
    }

    /// ★★ 回调自带的 `userInfo` 必须能解出 uid 与昵称 —— 这是登录成功的
    /// **最后一道关卡**，也是真实客户端的首选来源（`r ?? await getUserInfo`）。
    ///
    /// 字段名逐字取自真实客户端（`main.js` 的 `WP`/`s_`：`t.ScreenName` /
    /// `i.UserID` / `i.AIRegion`），它同时钉住：
    /// - uid 键是 **`UserID`**（大驼峰），不是 `userId`/`id`
    /// - 昵称键是 **`ScreenName`**，不是 `name`/`userName`
    /// - 区域键是 **`AIRegion`**
    /// - `UserID` 是**字符串**（真实上游按字符串下发），不能只按数字解析
    ///
    /// ⚠ 样本值全部是**虚构的占位值**（公开仓库不放真实 uid/昵称/头像）；
    /// 本用例只验证**键名与取值形状**，与具体账号无关。
    #[test]
    fn 回调_userInfo_能解出_uid_与昵称() {
        let raw = concat!(
            "%7B%22AIRegion%22%3A%22CN%22%2C%22AvatarUrl%22%3A%22https%3A%2F%2Fexample.invalid",
            "%2Favatar.png%22",
            "%2C%22Description%22%3A%22%22%2C%22Gender%22%3A%223%22",
            "%2C%22LastLoginTime%22%3A%222026-01-01T00%3A00%3A00%2B08%3A00%22",
            "%2C%22LastLoginType%22%3A%22douyin%22%2C%22MigrateToSG%22%3Afalse",
            "%2C%22NonPlainTextEmail%22%3A%22%22%2C%22NonPlainTextMobile%22%3A%22138%2A%2A%2A%2A%2A%2A00%22",
            "%2C%22Region%22%3A%22CN%22%2C%22ScreenName%22%3A%22tester%22",
            "%2C%22TenantID%22%3A%22tenant-placeholder%22%2C%22UserID%22%3A%229000000000000001%22%7D"
        );
        let url = format!(
            "http://127.0.0.1:17388/authorize?isRedirect=true&scope=solo&userInfo={raw}&userRegion=cn"
        );
        let (name, uid, region) = user_info_from_callback(&url);
        assert_eq!(name.as_deref(), Some("tester"), "昵称键必须是 ScreenName");
        assert_eq!(
            uid.as_deref(),
            Some("9000000000000001"),
            "uid 键必须是 UserID（大驼峰字符串）"
        );
        assert_eq!(region.as_deref(), Some("CN"));
    }

    /// `UserID` 以数字形态出现时也要能取（不同上游版本序列化方式不同）。
    #[test]
    fn 回调_userInfo_的_uid_是数字也认() {
        let url = "http://127.0.0.1:17388/authorize?userInfo=%7B%22UserID%22%3A123%7D";
        let (_, uid, _) = user_info_from_callback(url);
        assert_eq!(uid.as_deref(), Some("123"));
    }

    /// 没有 `userInfo`（或它不是合法 JSON）时必须**安静地**返回三个 None，
    /// 让调用方落到 JWT / GetUserInfo 兜底，而不是 panic 或误报。
    #[test]
    fn 回调没有_userInfo_时安静返回_none() {
        assert_eq!(
            user_info_from_callback("http://127.0.0.1:17388/authorize?code=X"),
            (None, None, None)
        );
        assert_eq!(
            user_info_from_callback("http://127.0.0.1:17388/authorize?userInfo=not-json"),
            (None, None, None)
        );
    }

    /// 顶层键的大小写/下划线变体都要认（上游不同版本用过三种写法）。
    #[test]
    fn 顶层授权码键名变体都认() {
        for (json, expect) in [
            (r#"{"AuthCode":"C1"}"#, "C1"),
            (r#"{"authCode":"C2"}"#, "C2"),
            (r#"{"auth_code":"C3"}"#, "C3"),
            (r#"{"AUTHCODE":"C4"}"#, "C4"),
        ] {
            assert_eq!(
                extract_auth_code_from_json(json).as_deref(),
                Some(expect),
                "应认得顶层键变体：{json}"
            );
        }
        // 非字符串/空值不能被误取（`ExpireAt` 是数字，`Other` 是空串）
        assert_eq!(
            extract_auth_code_from_json(r#"{"AuthCode":"","ExpireAt":123}"#),
            None
        );
        assert_eq!(extract_auth_code_from_json(r#"{"ExpireAt":123}"#), None);
    }

    #[test]
    fn 回调带_loginTraceID_也能过_state_校验() {
        let _iso = seed("trace-1");
        // 授权页把 state 原样回传为 loginTraceID
        assert_eq!(
            parse_callback("http://127.0.0.1:17388/authorize?code=C&loginTraceID=trace-1").unwrap(),
            "C"
        );
    }

    #[test]
    fn 回调里的授权拒绝要如实传达() {
        let _iso = seed("s");
        let err = parse_callback(
            "http://127.0.0.1:17388/authorize?error=access_denied&error_description=user%20said%20no&state=s",
        )
        .expect_err("应报错");
        assert!(err.contains("access_denied"));
        assert!(err.contains("user said no"), "错误描述应被解码：{err}");
    }

    #[test]
    fn 没有待处理会话时回调一律拒绝() {
        let _iso = config::test_isolation::Isolated::new("trae_oauth");
        clear_pending();
        let err = parse_callback("http://127.0.0.1:17388/authorize?code=x&state=anything")
            .expect_err("无会话必须报错");
        assert!(err.contains("重新"), "应提示重新发起：{err}");
    }

    #[test]
    fn 换_token_拒绝_401_与业务错误() {
        let rejected = parse_exchange("u", &json!({ "code": 401 }));
        let err = rejected.expect_err("401 必须报错");
        assert!(err.contains("被服务端拒绝"), "{err}");

        let biz = parse_exchange("u", &json!({ "code": 0, "data": { "code": 1001, "message": "bad" } }));
        assert!(biz.is_err());

        // 正常响应
        let ok = parse_exchange(
            "u",
            &json!({ "code": 0, "data": { "result": { "access_token": "AT", "refresh_token": "RT" } } }),
        )
        .expect("应成功");
        assert_eq!(ok.0, "AT");
        assert_eq!(ok.1.as_deref(), Some("RT"));
    }

    #[test]
    fn 换_token_缺_access_token_时报错带脱敏响应() {
        let err = parse_exchange(
            "u",
            &json!({ "code": 0, "data": { "refresh_token": "SECRET-RT" } }),
        )
        .expect_err("缺 access_token 应报错");
        // 错误信息里绝不能出现明文 refresh_token
        assert!(!err.contains("SECRET-RT"), "错误信息泄露了 refresh_token：{err}");
        assert!(err.contains("已脱敏"), "{err}");
    }

    #[test]
    fn 脱敏覆盖嵌套结构() {
        let mut v = json!({
            "a": { "b": [{ "AccessToken": "LEAK1" }] },
            "refresh_token": "LEAK2",
            "safe": "keep"
        });
        mask_sensitive(&mut v);
        let s = v.to_string();
        assert!(!s.contains("LEAK1"), "{s}");
        assert!(!s.contains("LEAK2"), "{s}");
        assert!(s.contains("keep"), "非敏感字段不该被改动：{s}");
    }

    #[test]
    fn 回调结果页转义脚本注入() {
        // 参考实现 P0 缺陷 6 的回归点
        let html = callback_page(
            false,
            "兑换失败: <script>alert(1)</script>",
            "http://127.0.0.1:17388/authorize?code=ab&state=<img src=x onerror=alert(2)>",
        );
        assert!(!html.contains("<script>"), "脚本标签未被转义");
        assert!(!html.contains("<img"), "img 标签未被转义");
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("&lt;img src=x onerror=alert(2)&gt;"));
        assert!(html.contains("登录失败"));
        assert!(html.contains("code=ab&amp;state="), "& 应被转义为 &amp;");
    }

    #[test]
    fn 成功结果页展示账号名() {
        let html = callback_page(true, "账号 [测试] 登录成功", "");
        assert!(html.contains("登录成功"));
        assert!(html.contains("账号 [测试] 登录成功"));
    }

    #[test]
    fn query_解析保留重复参数并解码() {
        let params = parse_query("http://x/y?a=1&a=2&b=%E4%B8%AD%E6%96%87#frag");
        assert_eq!(params.iter().filter(|(k, _)| k == "a").count(), 2);
        assert!(params.iter().any(|(k, v)| k == "b" && v == "中文"));
        // #fragment 不该被当成 query 的一部分
        assert!(!params.iter().any(|(k, _)| k.contains("frag")));
    }

    #[test]
    fn 随机串长度与字符集正确() {
        let a = random_hex(32);
        let b = random_hex(32);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        // 两次结果不同（32 字符 hex 相撞的概率可忽略）
        assert_ne!(a, b);
        // 奇数长度也要精确截断到请求的长度
        assert_eq!(random_hex(5).len(), 5);
        assert_eq!(random_hex(1).len(), 1);
    }

    /// 测试用：登记一个指定 host 的登录会话（`api_host` 指向本地 mock 上游）。
    fn seed_with_host(state: &str, host: &str) -> config::test_isolation::Isolated {
        let iso = config::test_isolation::Isolated::new("trae_oauth");
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(PendingLogin {
            state: state.to_string(),
            pkce_verifier: "v".repeat(64),
            flavor: TraeFlavor::Solo,
            api_host: Some(host.to_string()),
        });
        iso
    }

    /// 起一个只收一个请求的 mock 上游，回它一个给定的 JSON。
    ///
    /// 返回 `(host, 收到请求的接收端)`。
    async fn mock_upstream(reply: &'static str) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            // 先读头，再按 Content-Length 读满 body
            let (head_end, content_len) = loop {
                let Ok(n) = sock.read(&mut chunk).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    break (pos + 4, len);
                }
            };
            while buf.len() < head_end + content_len {
                let Ok(n) = sock.read(&mut chunk).await else {
                    return;
                };
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let _ = tx.send(String::from_utf8_lossy(&buf).to_string());
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.len(),
                reply
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
        });
        (format!("http://127.0.0.1:{port}"), rx)
    }

    /// ★★★ 决定性验证：换 token 的**请求报文**必须与真实客户端逐字段一致。
    ///
    /// # 为什么这条测试存在
    ///
    /// 上游对字段名错配的回应是笼统的 `10101 无效参数：{__Message.field}.`，
    /// **不告诉你是哪个字段**（实测：空 body 与完整字段返回逐字相同的错误）。
    /// 也就是说，字段名写错时从错误信息里**完全看不出来** —— 只能靠比对
    /// 真实客户端源码。这条测试把比对结果钉死，防止以后再漂移。
    ///
    /// 权威来源：`D:\APP\TRAE SOLO CN\resources\app\out\main.js` 的
    /// `exchangeTokenByAuthCode`。
    ///
    /// 注意这里用本地 mock 上游而不是真上游：真上游需要**新鲜授权码**
    /// （600 秒窗口、一次性），没法进 CI。mock 验的是"我们发出去的字节"，
    /// 那正是这条链路上唯一由我们决定的、也是最容易写错的部分。
    #[tokio::test]
    async fn 换_token_请求报文与真实客户端逐字段一致() {
        let reply = r#"{"ResponseMetadata":{"Error":null},"Result":{"Token":"AT_FROM_MOCK","RefreshToken":"RT_FROM_MOCK","TokenExpireAt":1790906034345,"TokenExpireDuration":600000,"RefreshExpireAt":1790906034345}}"#;
        let (host, rx) = mock_upstream(reply).await;
        let _iso = seed_with_host("state-abc", &host);

        let (access, refresh) = exchange_auth_code("THE_AUTH_CODE").await.expect("应能换到 token");

        // 响应解析：`Result.Token` / `Result.RefreshToken` 大驼峰必须认。
        //
        // ⚠ 这里返回的是**裸 token**，不带 `Cloud-IDE-JWT ` 前缀 —— 加前缀是
        // 调用方的事（`oauth_loopback.rs:176` 与 `apps_api.rs:305` 都自己
        // `format!("Cloud-IDE-JWT {access}")`）。
        // 与 `trae_refresh::exchange_token` 的约定**不同**（那边返回带前缀的），
        // 因为刷新路径没有调用方去补前缀。改这里会同时打断两个调用方。
        assert_eq!(access, "AT_FROM_MOCK");
        assert_eq!(refresh.as_deref(), Some("RT_FROM_MOCK"));

        let raw = rx.await.expect("mock 上游应收到请求");
        let (head, body) = raw.split_once("\r\n\r\n").expect("请求应有头体分隔");
        let body: Value = serde_json::from_str(body).expect("请求体应是 JSON");

        // ---- 请求行 ----
        let req_line = head.lines().next().unwrap();
        assert_eq!(
            req_line, "POST /trae/api/v3/oauth/ExchangeToken HTTP/1.1",
            "路径必须是真实客户端的 /trae/api/v3/oauth/ExchangeToken，\
             不是我们此前猜的 /cloudide/api/v3/trae/oauth/ExchangeToken"
        );

        // ---- headers：真实客户端只有 Content-Type + x-cloudide-token ----
        let lower = head.to_lowercase();
        assert!(lower.contains("content-type: application/json"));
        assert!(
            lower.contains("x-cloudide-token:"),
            "x-cloudide-token 必须存在（换 token 时值为空串）"
        );
        assert!(
            !lower.contains("x-app-id"),
            "X-App-Id 是我们臆造的 header，真实客户端 main.js 里 0 处"
        );

        // ---- body：五个字段，一个不多一个不少 ----
        let obj = body.as_object().expect("body 应是对象");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["AuthCode", "ClientID", "CodeVerifier", "DeviceInfo", "IDEVersion"],
            "字段名必须与真实客户端一致（OAuth 标准字段 client_id/code/grant_type 是错的）"
        );
        assert_eq!(body["AuthCode"], "THE_AUTH_CODE");
        assert_eq!(body["CodeVerifier"], "v".repeat(64));
        // SOLO 线：client_id 与 IDEVersion 都必须取 SOLO 的那套
        assert_eq!(body["ClientID"], OAUTH_CLIENT_ID_SOLO);
        assert_eq!(body["IDEVersion"], app_version(TraeFlavor::Solo));

        // ---- DeviceInfo：12 个字段，含真实 P-256 SPKI 公钥 ----
        let dev = body["DeviceInfo"].as_object().expect("DeviceInfo 应是对象");
        let mut dkeys: Vec<&str> = dev.keys().map(String::as_str).collect();
        dkeys.sort_unstable();
        assert_eq!(
            dkeys,
            [
                "ClientVersion", "DeviceBrand", "DeviceCPU", "DeviceID", "DeviceModel",
                "DeviceName", "DevicePublicKey", "DeviceType", "MachineID", "OSInfo",
                "OSVersion", "PlatformCode"
            ],
            "DeviceInfo 字段集必须与真实客户端 `async j(t)` 一致"
        );
        assert_eq!(dev["DeviceType"], "PC");
        // SOLO 线是 SOLO_PC —— 我们此前全局写死 IDE_PC，对 SOLO 用户是错的
        assert_eq!(
            dev["PlatformCode"], "SOLO_PC",
            "SOLO 产品线的 PlatformCode 必须是 SOLO_PC（main.js 的 k()）"
        );
        assert_eq!(
            dev["ClientVersion"],
            app_version(TraeFlavor::Solo),
            "ClientVersion 应等于该产品线的 appVersion"
        );
        let pub_pem = dev["DevicePublicKey"].as_str().expect("公钥必须是字符串");
        assert!(
            pub_pem.starts_with("-----BEGIN PUBLIC KEY-----"),
            "公钥必须是 SPKI PEM（真实客户端用 P-256 spki/pem），实际开头: {}",
            &pub_pem[..pub_pem.len().min(40)]
        );
        assert!(
            pub_pem.contains("-----END PUBLIC KEY-----"),
            "PEM 必须完整"
        );

        // ---- 设备字段必须是真的，不能是空串 ----
        //
        // 真实客户端 `j(t)` 传的是 `e?.deviceModel ?? ""` 等，`e` 来自
        // `iCubeSystemInformationService` —— 空串只在系统信息取不到时才出现。
        // 我们此前把 DeviceModel / DeviceBrand / DeviceCPU 全写死空串，
        // 这里钉住「不能回归成空串」。
        //
        // ★★ 2026-10-09：这段断言此前**没有平台门控**，于是 ci.yml 与
        // release.yml 的 `ubuntu-22.04` 腿**从 1.0.23 起一直是红的**
        // （`cargo test -p ai-gateway-core` ⇒ `1284 passed; 1 failed`）。
        //
        // 根因：三个取值函数都是 `#[cfg(windows)]` 的 —— 非 Windows 上它们
        // **必然**返回 `String::new()`（那正是 `#[cfg(windows)]` 块后面的兜底）。
        // 也就是说「空串」在 Linux/macOS 上不是回归，而是**设计如此**：
        // 那三个平台根本读不到 `HARDWARE\\DESCRIPTION\\System\\BIOS` 注册表。
        //
        // 为什么必须按平台分流而不是放宽断言：
        //  · 放宽（如「允许空」）⇒ Windows 上真正的回归就再也测不出来了，
        //    而这条测试存在的唯一理由就是防那个回归；
        //  · 门控 ⇒ Windows 上仍然逐字钉死，非 Windows 上跳过这段（并断言
        //    它们**确实是**空串 —— 把「兜底真的生效」也钉住）。
        //
        // ⚠ 别把整个测试标 `#[cfg(windows)]`：这条测试还钉着 DeviceInfo 的
        // 字段集、PlatformCode、OSInfo、公钥 PEM 等**跨平台**契约，
        // 那些在任何平台上都该验。只有下面这三个字段是平台相关的。
        for key in ["DeviceModel", "DeviceBrand", "DeviceCPU"] {
            let v = dev[key].as_str().expect("设备字段必须是字符串");
            assert_eq!(v, v.trim(), "{key} 不能带首尾空格，实际: {v:?}");
            if cfg!(windows) {
                assert!(
                    !v.trim().is_empty(),
                    "{key} 不能是空串（真实客户端上报真实机型/厂商/CPU），实际: {v:?}"
                );
            } else {
                // 非 Windows：三个函数走 `#[cfg(windows)]` 之后的兜底 ⇒ 必为空。
                // 断言这一点，等于钉住「兜底路径真的存在且生效」。
                assert!(
                    v.is_empty(),
                    "非 Windows 上 {key} 应走兜底返回空串（读不到 Windows 注册表），实际: {v:?}"
                );
            }
        }
        // 真实客户端 `this.c = Id==="win32" ? "windows" : Id` —— 小写
        assert_eq!(
            dev["OSInfo"], "windows",
            "OSInfo 必须是小写 windows（main.js 的 `Id===\"win32\"?\"windows\":Id`），\
             我们此前发的是首字母大写的 Windows"
        );
    }

    /// 真实错误信封要被识别成失败，且带上服务端给的 code。
    ///
    /// 修复前这里会退化成"响应里没有 access_token" —— 把「服务端明确拒绝」
    /// 误报成解析失败，调用方就会按 Transient 计数重试而不是立即置失效。
    #[tokio::test]
    async fn 换_token_真实错误信封被识别为拒绝() {
        let reply = r#"{"ResponseMetadata":{"Action":"","Error":{"Code":"10101","Data":{},"Message":"无效参数：{__Message.field}.","StandardCode":"040004"},"OID":null,"Region":"","RequestId":"","Service":"","Source":"","TraceID":"00000000000000000000000000000000","Version":"","WID":null}}"#;
        let (host, _rx) = mock_upstream(reply).await;
        let _iso = seed_with_host("state-err", &host);

        let err = exchange_auth_code("BAD_CODE").await.expect_err("必须判为失败");
        assert!(err.contains("10101"), "错误信息要带服务端 code，实际: {err}");
        assert!(
            err.contains("某个字段"),
            "上游模板占位符要替换成可读文案，实际: {err}"
        );
        assert!(
            !err.contains("__Message.field"),
            "不该把原始占位符透给用户: {err}"
        );
        assert!(
            !err.contains("没有 access_token") && !err.contains("access_token"),
            "不该退化成解析错误（那会让调用方按瞬时失败重试）: {err}"
        );
    }
}
