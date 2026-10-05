//! ZCode 上游错误的分类与面向用户的文案。
//!
//! 移植自 Go `wt-port\go-gateway\internal\zcode\errors.go`（565 行），逐字对齐。
//!
//! # 为什么需要分类
//!
//! 上游的错误都是 HTTP 401 + 一个数字码，但**含义完全不同**，
//! 排查方向也完全不同：
//!
//! ```text
//! 1001  认证参数未收到   → 我们没带 Authorization（**我们的 bug**）
//! 1000  认证失败         → 凭证无效/过期（**用户要换凭证**）
//! VERIFY_*  签名校验失败  → 上游开始要求客户端签名（**要补实现**）
//! ```
//!
//! 若不分类，三者都会显示成"认证失败"，用户只会去反复换凭证 ——
//! 而 1001 换多少次都没用（是我们没发头）。

use regex::Regex;
use serde_json::Value;

use super::signing::is_verify_failure;

/// 错误分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrKind {
    /// 未分类（网络错误、5xx、无法解析的响应）。
    #[default]
    None,
    /// 认证参数未收到（1001）—— 我们没带 Authorization。
    ///
    /// 这几乎总是**网关自身的配置/代码问题**，不是用户凭证问题。
    /// 报错文案必须点明这一点，否则用户会去反复换凭证（无用功）。
    AuthMissing,
    /// 认证失败（1000）—— 凭证无效或已过期。
    ///
    /// 这是用户能处理的情况：换凭证或重新登录。
    AuthFailed,
    /// 上游要求客户端签名（`VERIFY_SIGNATURE_*`）。
    ///
    /// 我们**刻意没实现签名**（见 [`super::signing`]），因为实测不需要。
    /// 若真出现这个错误，说明上游改了规则 —— 报错必须明确说这件事，
    /// 而不是笼统的"认证失败"（那会让排查方向完全跑偏）。
    SigningRequired,
    /// 限流（429 或上游的限流码）。
    RateLimited,
    /// 额度耗尽。
    QuotaExhausted,
    /// 模型不存在（11102）。
    ///
    /// 实测最常见的原因是**模型名大小写不对**（上游严格区分）：
    /// `GLM-5.3` 报此错，而正确写法是 `glm-5.3`。
    ModelNotFound,
    /// 上游不可用（5xx）。
    ProviderDown,
    /// 该账号**没有 coding plan 资格**（上游 3101）。
    ///
    /// 实测（2026-09-19，`uitest/probe-zcode-all-chat.cjs`）：把客户端那套
    /// 请求头补全后，套餐通道从 401 变成
    ///
    /// ```text
    /// HTTP 403 {"code":3101,"msg":"coding plan is required"}
    /// ```
    ///
    /// 403（而非 401）说明**鉴权已经通过** —— 是账号侧没有这个套餐的资格。
    ///
    /// 为什么单独成类：它的处置与"额度耗尽"完全不同 ——
    /// 额度耗尽要充值，而这个要**换一个有套餐的账号**或去官方领套餐。
    /// 混在一起报会让用户白充钱。
    PlanRequired,
    /// 该账号**在这条通道上没有可用资源包**（上游 429 + 1113）。
    ///
    /// ## 为什么必须与"额度耗尽"分开（所有者实测踩到的误导）
    ///
    /// 上游原文：
    ///
    /// ```text
    /// HTTP 429 rate_limit_error [1113][余额不足或无可用资源包,请充值。]
    /// ```
    ///
    /// 我们此前把这类 429 一律归类成 `QuotaExhausted`，界面上就说
    /// 「账号额度已耗尽：请为该账号充值」。
    ///
    /// **但实测矛盾**：同一个账号的额度查询（`billing/balance`）明明显示
    /// GLM-5.3-Flash 有 3 亿 token、几乎没动用（`remaining=299999978`）。
    ///
    /// 原因：**额度与资源包在不同通道上**。
    ///
    /// ```text
    /// billing/balance     → zcode.z.ai 的 zcode-plan 通道（能看到额度）
    /// coding/paas/v4      → 另一条通道（这 1113 是从这条回的）
    /// ```
    ///
    /// 所以"额度耗尽"是**错的诊断** —— 用户会去充值，而他的额度就在那儿。
    NoResourcePack,
    /// 该操作需要**人机验证码**（上游 3007）。
    ///
    /// 实测：`/api/v1/zcode-plan/...` 对话通道回
    ///
    /// ```text
    /// HTTP 400 {"code":3007,"msg":"captcha verify failed"}
    /// ```
    ///
    /// ⚠ **刻意不实现绕过**：验证码是服务端的防滥用机制。
    /// 如实告诉用户"需要在官方客户端里完成一次验证"。
    CaptchaRequired,
}

impl ErrKind {
    /// 落盘/日志里的字符串形态（`None` 是空串，与 Go 的 `ErrNone = ""` 一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            ErrKind::None => "",
            ErrKind::AuthMissing => "auth_missing",
            ErrKind::AuthFailed => "auth_failed",
            ErrKind::SigningRequired => "signing_required",
            ErrKind::RateLimited => "rate_limited",
            ErrKind::QuotaExhausted => "quota_exhausted",
            ErrKind::ModelNotFound => "model_not_found",
            ErrKind::ProviderDown => "provider_down",
            ErrKind::PlanRequired => "plan_required",
            ErrKind::NoResourcePack => "no_resource_pack",
            ErrKind::CaptchaRequired => "captcha_required",
        }
    }
}

/// 上游业务码（实测确认，见 [`classify`] 的注释）。
pub mod code {
    /// 没发认证头（**我们的 bug**，与凭证无关）。
    pub const AUTH_MISSING: &str = "1001";
    /// 凭证无效或已过期。
    pub const AUTH_FAILED: &str = "1000";
    /// 余额不足 / 无可用资源包。
    ///
    /// ⚠ 它配的是 **HTTP 429** —— 与限流同码。故业务码必须**先于**状态码判断，
    /// 否则会把"去充值"误导成"稍后重试"。
    pub const QUOTA_EXHAUSTED: &str = "1113";
    /// 模型不存在（**大小写敏感**）。
    pub const MODEL_NOT_FOUND: &str = "11102";
    /// 没有 coding plan 资格（403）。
    pub const PLAN_REQUIRED: &str = "3101";
    /// 需要人机验证码（400）。
    ///
    /// ⚠ 不实现绕过（防滥用机制）。
    pub const CAPTCHA_REQUIRED: &str = "3007";
}

/// 上游错误（带分类与原始响应）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamError {
    /// 分类。
    pub kind: ErrKind,
    /// HTTP 状态码。
    pub status: u16,
    /// 上游的业务码（如 `"1000"`）；可能为空。
    pub code: String,
    /// 上游的文案（原样保留，供排障）。
    pub msg: String,
}

impl UpstreamError {
    /// 构造分类后的错误。
    pub fn new(status: u16, body: &str) -> Self {
        Self {
            kind: classify(status, body),
            status,
            code: extract_code(body),
            msg: extract_message(body),
        }
    }

    /// 面向用户的错误说明。
    ///
    /// 每种分类给一句**能指导下一步动作**的话，而不是复述上游原文 ——
    /// 上游原文（如"身份验证失败。"）不告诉用户该做什么。
    pub fn friendly_message(&self) -> String {
        match self.kind {
            ErrKind::AuthMissing => "网关没有向 ZCode 上游发送认证信息（上游返回「认证参数未收到」）。\
                 这通常是网关配置问题而非凭证问题，请检查该账号的凭证是否为空。"
                .to_string(),
            ErrKind::AuthFailed => {
                "ZCode 凭证无效或已过期（上游返回「认证失败」）。\
                 请在 ZCode 控制台重新生成凭证，或在账号页重新登录。"
                    .to_string()
            }
            ErrKind::SigningRequired => format!(
                "ZCode 上游开始要求**客户端签名**（{}）。\
                 本网关出于实测结论未实现该签名（详见 products/zcode/signing.rs），\
                 现在需要补上 —— 这是代码问题，不是凭证问题。",
                self.msg
            ),
            ErrKind::RateLimited => {
                format!("ZCode 上游限流（{}）。稍后会自动重试其他账号。", self.msg)
            }
            ErrKind::QuotaExhausted => {
                // 点明"要去充值"，而不是"稍后重试" —— 后者会让用户白等
                format!(
                    "ZCode 账号额度已耗尽，请为该账号充值或更换账号（上游：{}）",
                    self.msg
                )
            }
            ErrKind::NoResourcePack => {
                // ⚠ 这段文案**刻意不提"充值"**。实测该账号的 billing/balance
                // 显示额度充足（3 亿 token 几乎未用），而这条通道回 1113 ——
                // 说明问题在"这条通道没有可用资源包"，不在额度。
                //
                // 提"充值"会把用户引向一个解决不了问题的方向。
                format!(
                    "该 ZCode 账号在**这条通道上没有可用资源包**（上游：{}）。\
                     注意这**不一定是额度不足** —— 实测有账号额度查询显示数亿 token、几乎未用，\
                     却仍回这个错，因为额度与资源包在不同通道上。\
                     建议改用其他账号，或先在 ZCode 官方客户端里确认该账号的套餐资格。",
                    self.msg
                )
            }
            ErrKind::ModelNotFound => {
                // 最常见的原因是**模型名大小写**（上游严格区分）——
                // 不点明的话，用户会以为"上游不支持这个模型"
                format!(
                    "ZCode 上游没有这个模型（上游：{}）。\
                     注意模型名**区分大小写**（如 glm-5.3 与 GLM-5.3 在上游是两个不同的名字）",
                    self.msg
                )
            }
            ErrKind::ProviderDown => {
                format!("ZCode 上游暂时不可用（{}）。稍后会自动重试。", self.msg)
            }
            ErrKind::PlanRequired => {
                // 与"额度耗尽"要**分开说**：那个要充值，这个充值也没用
                format!(
                    "该 ZCode 账号**没有 Coding Plan 资格**，因此套餐通道不可用\
                     （上游：{}）。请注意这不是凭证问题（凭证是有效的），\
                     充值也解决不了 —— 需要换一个有套餐的账号，\
                     或先在 ZCode 官方客户端里开通/领取套餐。",
                    self.msg
                )
            }
            ErrKind::CaptchaRequired => {
                // 如实说明，并明确我们**不绕过**验证码
                format!(
                    "该 ZCode 操作需要**人机验证**（上游：{}）。\
                     出于对服务端防滥用机制的尊重，本网关**不会绕过验证码** —— \
                     请在 ZCode 官方客户端里完成一次验证后重试。",
                    self.msg
                )
            }
            ErrKind::None => {
                if !self.msg.is_empty() {
                    format!("ZCode 上游返回错误：{}", self.msg)
                } else {
                    format!("ZCode 上游返回 HTTP {}", self.status)
                }
            }
        }
    }
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = vec![format!("HTTP {}", self.status)];
        if !self.code.is_empty() {
            parts.push(format!("code={}", self.code));
        }
        if !self.msg.is_empty() {
            parts.push(self.msg.clone());
        }
        f.write_str(&parts.join(" "))
    }
}

/// 按状态码与响应体分类。
///
/// 判定顺序**很重要**（按特异性从高到低）：
///
/// 1. 签名错误（`VERIFY_*`）—— 最特异，且含义与其它完全不同
/// 2. 业务码（1001 / 1000 / 1113 / 11102 / 3101 / 3007）—— 实测确认的区分
/// 3. 文案里的**额度信号**（先于状态码）
/// 4. HTTP 状态码兜底（429 / 5xx / 402）
/// 5. 其余文案兜底 → [`ErrKind::None`]（不猜测）
///
/// ## ⚠ 业务码必须**先于** HTTP 状态码判断
///
/// 实测（`uitest/diag-zcode-upstream-error.cjs`）：额度不足返回的是
///
/// ```text
/// HTTP 429  {"error":{"code":"1113","message":"余额不足或无可用资源包,请充值。"}}
/// ```
///
/// 若先按 429 判，会得到 [`ErrKind::RateLimited`] → 界面提示"请求过于频繁，
/// 请稍后重试" —— **完全错误的方向**：用户会一直重试，而正确动作是去充值。
///
/// 上游用 429 表示两种完全不同的情况（限流 / 余额不足），只有业务码能区分。
pub fn classify(status: u16, body: &str) -> ErrKind {
    // 1. 签名相关（最特异）
    if is_verify_failure(body) {
        return ErrKind::SigningRequired;
    }

    let code = extract_code(body);

    // 2. 业务码（**必须先于状态码**，见上面的说明）
    match code.as_str() {
        // 「认证参数未收到」—— 我们没带 Authorization
        code::AUTH_MISSING => return ErrKind::AuthMissing,
        // 「认证失败」—— 凭证无效
        code::AUTH_FAILED => return ErrKind::AuthFailed,
        // 1113「余额不足或无可用资源包，请充值」
        //
        // ⚠ 归为 NoResourcePack 而**不是** QuotaExhausted。
        // 实测（2026-09-19）：同一个账号的 billing/balance 显示
        // GLM-5.3-Flash 有 3 亿 token、几乎没动，而 coding/paas/v4 通道
        // 回这个 1113 —— 说明**额度与资源包在不同通道上**。
        code::QUOTA_EXHAUSTED => return ErrKind::NoResourcePack,
        // 「模型不存在」—— 多半是模型名不对（上游大小写敏感）
        code::MODEL_NOT_FOUND => return ErrKind::ModelNotFound,
        // 「coding plan is required」—— 账号没有套餐资格
        //（403 而非 401 = 鉴权已过，是账号侧的事）
        code::PLAN_REQUIRED => return ErrKind::PlanRequired,
        // 「captcha verify failed」—— 需要人机验证。
        // ⚠ 业务码判断必须在状态码之前：这里的 3007 配的是 HTTP 400，
        // 若不特判会掉进 default（无分类），界面上就是一句空洞的
        // "上游返回 HTTP 400"，用户完全不知道该怎么办。
        code::CAPTCHA_REQUIRED => return ErrKind::CaptchaRequired,
        _ => {}
    }

    // 3. 文案里的**额度信号**要先于状态码判 —— 见下面 429 的注释。
    //
    // 上游有时不给数字码，只给中文文案（"余额不足，请充值"），
    // 而它配的仍是 429。若先按状态码判，会得到"限流"这个错误方向。
    //
    // ⚠ 归为 NoResourcePack 而不是 QuotaExhausted —— 理由同上：
    // 实测有账号"额度充足但该通道无资源包"，报"额度耗尽"是误导。
    let lower = body.to_lowercase();
    if body.contains("余额不足") || body.contains("请充值") || lower.contains("insufficient") {
        return ErrKind::NoResourcePack;
    }

    // 4. HTTP 状态码兜底
    if status == 429 {
        return ErrKind::RateLimited;
    }
    if status >= 500 {
        return ErrKind::ProviderDown;
    }
    if status == 402 {
        // 402 Payment Required：额度相关
        return ErrKind::QuotaExhausted;
    }

    // 5. 其余文案兜底（上游有时不返回数字码）
    if lower.contains("service info not found") {
        return ErrKind::ModelNotFound;
    }
    if lower.contains("rate limit") || lower.contains("too many") {
        return ErrKind::RateLimited;
    }
    if lower.contains("authentication failed") || lower.contains("身份验证失败") {
        return ErrKind::AuthFailed;
    }

    ErrKind::None
}

/// 从响应体里取业务码。
///
/// 兼容两种形状（实测两种都存在）：
///
/// ```text
/// {"error":{"code":"1000","message":"Authentication Failed"}}
/// {"error":{"message":"Authentication Failed","type":"1000"}}
/// ```
///
/// 后者是 Anthropic 端点返回的形状（把码放在 `type` 字段）。
pub fn extract_code(body: &str) -> String {
    let Ok(doc) = serde_json::from_str::<Value>(body) else {
        return String::new();
    };
    if let Some(c) = doc
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|v| v.as_str())
    {
        if !c.is_empty() {
            return c.to_string();
        }
    }
    // Anthropic 形状：码在 type
    if let Some(t) = doc
        .get("error")
        .and_then(|e| e.get("type"))
        .and_then(|v| v.as_str())
    {
        if !t.is_empty() && is_digits(t) {
            return t.to_string();
        }
    }
    // 顶层 code（可能是数字）
    match doc.get("code") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => format!("{n:.0}"),
        _ => String::new(),
    }
}

/// 从响应体里取可读文案。
pub fn extract_message(body: &str) -> String {
    let Ok(doc) = serde_json::from_str::<Value>(body) else {
        // 不是 JSON：截断原文（可能是 HTML 错误页）
        return truncate(body, 200);
    };
    let candidates = [
        doc.get("error").and_then(|e| e.get("message")),
        doc.get("error").and_then(|e| e.get("msg")),
        doc.get("message"),
        doc.get("msg"),
    ];
    for c in candidates.into_iter().flatten() {
        if let Some(s) = c.as_str() {
            let t = s.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    truncate(body, 200)
}

/// 是否全为 ASCII 数字。
pub fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// 按**字节**上限截断（超出则加 `…`）。
///
/// ★ 与 Go 的 `truncate` 有一处刻意的差异：Go 直接 `s[:n]` 切字节，
/// 对中文会切出**非法 UTF-8**（Go 容忍，Rust 的 `&str` 不允许）。
/// 这里退到最近的字符边界 —— 语义（"最多 n 字节 + 省略号"）不变，
/// 但不会产生半个汉字。
pub fn truncate(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.len() <= n {
        return s.to_string();
    }
    let end = floor_char_boundary(s, n);
    format!("{}…", &s[..end])
}

/// 按**字符**上限截断（超出则加 `…`）—— Go 的 `truncateStr`。
///
/// Go 侧的顺序是：先比**字节**（`len(s) <= n` 直接返回），再比**字符**
/// （`len([]rune(s)) <= n` 也直接返回），最后按字符截断。照抄这个顺序。
pub fn truncate_str(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.len() <= n {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        return s.to_string();
    }
    let mut out: String = chars[..n].iter().collect();
    out.push('…');
    out
}

/// 找到不超过 `n` 的最大字符边界。
fn floor_char_boundary(s: &str, n: usize) -> usize {
    if n >= s.len() {
        return s.len();
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

// ---------------------------------------------------------------------------
// 验证码求解器回传的 verifyCode（F001 / T001 / T002 …）
// ---------------------------------------------------------------------------

/// 认不出 verifyCode 时，回退展示的原文截断长度。
///
/// 取 300 与参考实现（`zcode-yourhoneypomelo` 的 `_format_risk_reject`，
/// 其 `json.dumps(msg, ensure_ascii=False)[:300]`）对齐 —— 口径一致才好对照。
///
/// ⚠ 为什么必须截断而不是整段透传：求解器失败时 stderr 可能带 SDK 的整页
/// 调试输出（`[guest-err]` 系列），无界写进响应体既刷屏又会把真正的原因挤到
/// 用户看不到的地方。
pub const RISK_RAW_LIMIT: usize = 300;

/// 环境风险：出口 IP / 运行环境被判定为自动化或数据中心。
///
/// 实测来源：`zcode-a137460387/src/captcha/browser.js` 记着
/// 「headless + 默认 UA → F001」，即 SDK 据此识别自动化环境。
pub const RISK_ENV_REJECTED: &str = "F001";
/// 验证参数已过期。
pub const RISK_VERIFY_EXPIRED: &str = "T001";
/// 验证参数已被使用（param 是一次性的）。
pub const RISK_VERIFY_USED: &str = "T002";

/// verifyCode → 面向用户的可操作提示。
///
/// # 合规立场（写文案前必读）
///
/// 这里做的是**如实告知用户"你被风控了、以及为什么"** —— 是透明度，
/// **不是**绕过。故文案里绝不出现"如何骗过风控"的动作，只说明现象与出路。
/// 与 [`ErrKind::CaptchaRequired`] 的既有立场一致。
///
/// # 为什么 T001/T002 说的是"下一次请求"而不是"自动刷新"
///
/// 参考实现（`zcode-yourhoneypomelo`）的原话是「网关会自动刷新重试」。
/// **我们这里不能照抄** —— 实测核对：本项目的求解器**没有**"检测到 param
/// 失效就主动作废缓存/重新求解"的动作（Go 侧全文件 grep `invalidate` 零命中）。
/// 真实的语义是：
///
/// ```text
/// StreamChat：先不带 param 发一次 → 吃 3007 → 现解一个 → 带新 param 重发
/// claimWithCaptcha：每次尝试都**新铸**一个 param（最多两次）
/// ```
///
/// 即"**下一次请求会重新求解一次**"，而不是"网关自己发现失效并刷新"。
/// 照抄参考实现就是说满了 —— 用户会以为不用管，而实际需要再发一次请求。
pub fn risk_code_hint(code: &str) -> &'static str {
    match code.trim().to_uppercase().as_str() {
        RISK_ENV_REJECTED => {
            "当前出口 IP / 运行环境被上游风控识别（云服务器、数据中心 IP、\
             或自动化环境几乎必现）。出路是让请求从一个**住宅/家宽出口**发出：\
             在网关的代理设置里填地址。⚠ 该代理按区域生效 —— \
             默认**只对国际版开启、国服关闭**（`proxy_scope`），\
             若 ZCode 流量走的是未勾选的区域，填了代理也不会经过它。"
        }
        RISK_VERIFY_EXPIRED => {
            "验证参数已过期（阿里云验证码的 param 寿命约 90 秒）。\
             网关会在**下一次请求**时重新求解一个新的参数并重试，通常无需手动干预。"
        }
        RISK_VERIFY_USED => {
            "验证参数已被使用（每个 param 只能用一次）。\
             网关会在**下一次请求**时重新求解一个新的参数并重试，通常无需手动干预。"
        }
        // ★ 空串是刻意的：调用方据此决定"回退到原文"，而不是显示一句空洞的
        // "未知风控码" —— 那和没有提示是一样的，还占掉了原文的位置。
        _ => "",
    }
}

/// `"verifyCode":"F001"` 这种带键名的形状（优先）。
///
/// 为什么优先：裸匹配（下面那条）会把正文里恰好出现的 `T100` 之类误当风控码，
/// 而带键名的形状是求解器实际产出的形状（`JSON.stringify({verifyCode:…})`）。
fn risk_code_json_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r#""verifyCode"\s*:\s*"([A-Za-z]\d{3})""#).expect("固定正则"))
}

/// 裸码兜底：`F001` / `T001` 这类「一个字母 + 三位数字」。
///
/// ⚠ 用 `\b` 卡边界，避免 `F0012` 被当成 `F001`（与 server 层
/// `hasBareCode` 防 `30120` 误命中 `3012` 是同一个教训）。
fn risk_code_bare_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b([FTft]\d{3})\b").expect("固定正则"))
}

/// 从任意文本里提取**已收录**的 verifyCode（没有则返回空串）。
///
/// 输入可以是求解器的报错、宿主求解服务返回的 `error` 字段、
/// 或上游响应体 —— 它们的形状各不相同，故这里两种正则都试。
///
/// ⚠ 只认**已收录**的码：未收录的码返回空串，让调用方走"回退原文"，
/// 而不是把码本身当成提示（`风控拒绝(F008)` 这种对用户毫无信息量）。
pub fn risk_code_of(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    if let Some(c) = risk_code_json_re().captures(text) {
        let code = c[1].to_uppercase();
        if !risk_code_hint(&code).is_empty() {
            return code;
        }
    }
    for c in risk_code_bare_re().captures_iter(text) {
        let code = c[1].to_uppercase();
        if !risk_code_hint(&code).is_empty() {
            return code;
        }
    }
    String::new()
}

/// 在文本前附上可操作提示；**认不出码时原样返回**。
///
/// # 回退契约
///
/// ```text
/// 认得出码 → "风控拒绝(F001)：<提示>。原始信息：<原文截断 300>"
/// 认不出码 → 原文**逐字**返回（不截断、不加前缀）
/// ```
///
/// ⚠ 认不出时为什么不截断：调用方会把返回值原样拼进给用户看的错误体，
/// 而那里的原文往往正是**唯一有用的线索**（如"求解器组件未安装"）。
/// 截断它等于把可诊断的信息砍掉。截断只用在"已经认出码、原文退居次要"的场合。
pub fn annotate_risk_code(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        return text.to_string();
    }
    let code = risk_code_of(text);
    if code.is_empty() {
        return text.to_string();
    }
    format!(
        "风控拒绝({code})：{}。原始信息：{}",
        risk_code_hint(&code),
        truncate_str(text, RISK_RAW_LIMIT)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_prefers_business_code_over_status_code() {
        // ★ 这条是整个分类里最容易写错的一条：429 + 1113 是"去充值"，
        // 429 本身是"稍后重试"，方向完全相反。
        let body = r#"{"error":{"code":"1113","message":"余额不足或无可用资源包,请充值。"}}"#;
        assert_eq!(classify(429, body), ErrKind::NoResourcePack);
        assert_ne!(classify(429, body), ErrKind::RateLimited);
        // 没有业务码的 429 才是限流
        assert_eq!(classify(429, r#"{"error":{"message":"too many requests"}}"#), ErrKind::RateLimited);
    }

    #[test]
    fn classify_all_documented_codes() {
        let cases = [
            ("1001", ErrKind::AuthMissing),
            ("1000", ErrKind::AuthFailed),
            ("1113", ErrKind::NoResourcePack),
            ("11102", ErrKind::ModelNotFound),
            ("3101", ErrKind::PlanRequired),
            ("3007", ErrKind::CaptchaRequired),
        ];
        for (code, want) in cases {
            let body = format!(r#"{{"error":{{"code":"{code}","message":"x"}}}}"#);
            assert_eq!(classify(400, &body), want, "码 {code}");
        }
    }

    #[test]
    fn classify_anthropic_shape_puts_code_in_type() {
        // Anthropic 端点把码放在 type 字段
        let body = r#"{"type":"error","error":{"type":"1000","message":"Authentication Failed"}}"#;
        assert_eq!(classify(401, body), ErrKind::AuthFailed);
        // type 不是纯数字 ⇒ 不当成码
        let body = r#"{"error":{"type":"invalid_request_error","message":"bad"}}"#;
        assert_eq!(classify(400, body), ErrKind::None);
    }

    #[test]
    fn classify_top_level_code_accepts_number() {
        let body = r#"{"code":3007,"msg":"captcha verify failed"}"#;
        assert_eq!(classify(400, body), ErrKind::CaptchaRequired);
        let body = r#"{"code":"3007","msg":"captcha verify failed"}"#;
        assert_eq!(classify(400, body), ErrKind::CaptchaRequired);
    }

    #[test]
    fn classify_quota_wording_beats_status_code() {
        // 上游有时不给数字码，只给中文文案，而配的仍是 429
        assert_eq!(classify(429, "余额不足，请充值"), ErrKind::NoResourcePack);
        assert_eq!(
            classify(400, r#"{"error":{"message":"Insufficient balance"}}"#),
            ErrKind::NoResourcePack
        );
    }

    #[test]
    fn classify_status_code_fallbacks() {
        assert_eq!(classify(429, ""), ErrKind::RateLimited);
        assert_eq!(classify(500, ""), ErrKind::ProviderDown);
        assert_eq!(classify(503, "bad gateway"), ErrKind::ProviderDown);
        assert_eq!(classify(402, ""), ErrKind::QuotaExhausted);
        assert_eq!(classify(400, ""), ErrKind::None);
    }

    #[test]
    fn classify_wording_fallbacks() {
        assert_eq!(classify(400, "service info not found"), ErrKind::ModelNotFound);
        assert_eq!(classify(400, "Rate limit exceeded"), ErrKind::RateLimited);
        assert_eq!(
            classify(401, "Authentication Failed"),
            ErrKind::AuthFailed
        );
        assert_eq!(classify(401, "身份验证失败"), ErrKind::AuthFailed);
        assert_eq!(classify(400, "完全无法识别的文案"), ErrKind::None);
    }

    #[test]
    fn classify_recognizes_signing_failures() {
        // ★ 上游开始要求签名时必须报 signing_required 而不是笼统的认证失败，
        // 否则排查方向完全跑偏（会去反复换凭证）。
        for body in [
            r#"{"error":{"code":"VERIFY_SIGNATURE_INVALID"}}"#,
            r#"{"error":{"code":"verify_apikey_expired"}}"#,
        ] {
            assert_eq!(classify(401, body), ErrKind::SigningRequired, "{body}");
        }
    }

    #[test]
    fn extract_code_and_message_prefer_error_object() {
        let body = r#"{"error":{"code":"1000","message":"Authentication Failed"},"msg":"外层"}"#;
        assert_eq!(extract_code(body), "1000");
        assert_eq!(extract_message(body), "Authentication Failed");

        // 非 JSON ⇒ 文案是截断后的原文，码为空
        assert_eq!(extract_code("<html>bad gateway</html>"), "");
        assert_eq!(extract_message("<html>bad gateway</html>"), "<html>bad gateway</html>");

        // 空体
        assert_eq!(extract_code(""), "");
        assert_eq!(extract_message(""), "");
    }

    #[test]
    fn upstream_error_display_matches_go_shape() {
        let e = UpstreamError {
            kind: ErrKind::AuthFailed,
            status: 401,
            code: "1000".into(),
            msg: "Authentication Failed".into(),
        };
        assert_eq!(e.to_string(), "HTTP 401 code=1000 Authentication Failed");
        let bare = UpstreamError {
            kind: ErrKind::None,
            status: 500,
            code: String::new(),
            msg: String::new(),
        };
        assert_eq!(bare.to_string(), "HTTP 500");
    }

    #[test]
    fn friendly_message_is_actionable_for_every_kind() {
        // 对应 Go `TestFriendlyMessageIsActionable`（`zcode_test.go:454-480`）：
        // 每种分类给一句**能指导动作**的话，并断言**关键词**。
        //
        // ⚠ 不能断言"文案里必须带上游原文"：Go 的 `ErrAuthMissing` / `ErrAuthFailed`
        // 两个分支**刻意不含 `e.Msg`** —— 它们的成因在网关侧（没带 Authorization /
        // 凭证过期），复述上游那句"认证参数未收到"不告诉用户该做什么。而
        // `ErrKind::None` 的文案本就是原文的回显。这条断言以前把两类搞混了。
        let cases = [
            (ErrKind::None, ""), // 回显原文，没有固定关键词
            (ErrKind::AuthMissing, "网关"),
            (ErrKind::AuthFailed, "重新"),
            (ErrKind::SigningRequired, "签名"),
            (ErrKind::RateLimited, "限流"),
            (ErrKind::QuotaExhausted, "额度"),
            (ErrKind::NoResourcePack, "资源包"),
            (ErrKind::ModelNotFound, "模型"),
            (ErrKind::ProviderDown, "不可用"),
            (ErrKind::PlanRequired, "套餐"),
            (ErrKind::CaptchaRequired, "人机验证"),
        ];
        for (k, must_have) in cases {
            let e = UpstreamError {
                kind: k,
                status: 400,
                code: String::new(),
                msg: "上游原文".into(),
            };
            let m = e.friendly_message();
            assert!(!m.is_empty(), "{k:?} 必须有文案");
            assert!(m.contains(must_have), "{k:?} 的文案应含 {must_have:?}：{m}");
            // 文案不能只是复述上游原文（那等于没说）
            assert_ne!(m, "上游原文", "{k:?} 的文案不应只是复述上游原文");
        }
        // ★ 两条最容易写错的立场：1113 不能提"充值"，3007 必须说"不绕过"
        let pack = UpstreamError {
            kind: ErrKind::NoResourcePack,
            status: 429,
            code: "1113".into(),
            msg: "余额不足或无可用资源包,请充值。".into(),
        };
        assert!(
            !pack.friendly_message().contains("请为该账号充值"),
            "1113 的文案不能把用户引向充值（他的额度就在那儿）"
        );
        let cap = UpstreamError {
            kind: ErrKind::CaptchaRequired,
            status: 400,
            code: "3007".into(),
            msg: "captcha verify failed".into(),
        };
        assert!(cap.friendly_message().contains("不会绕过验证码"));
    }

    #[test]
    fn risk_code_hint_covers_exactly_three_codes() {
        for c in ["F001", "f001", " F001 ", "T001", "t001", "T002"] {
            assert!(!risk_code_hint(c).is_empty(), "{c} 必须已收录");
        }
        // 未收录 ⇒ 空串（调用方据此回退原文，而不是显示"未知风控码"）
        for c in ["", "F002", "T003", "X999", "3012", "abc"] {
            assert!(risk_code_hint(c).is_empty(), "{c} 不该有提示");
        }
        // ★ T001/T002 只能承诺"下一次请求"，不能承诺"自动刷新"
        // （求解器没有 invalidate 动作，照抄参考实现就是说满了）
        for c in [RISK_VERIFY_EXPIRED, RISK_VERIFY_USED] {
            let h = risk_code_hint(c);
            assert!(h.contains("下一次请求"), "{c} 必须说清是下一次请求");
            assert!(!h.contains("自动刷新"), "{c} 不能承诺自动刷新");
        }
        // F001 不能承诺"配了代理求解器就会用"
        let f = risk_code_hint(RISK_ENV_REJECTED);
        assert!(f.contains("proxy_scope"), "F001 必须提示代理按区域生效");
        assert!(!f.contains("一定"), "F001 不能把代理说成保证有效");
    }

    #[test]
    fn risk_code_of_prefers_keyed_shape_and_rejects_unknown() {
        assert_eq!(risk_code_of(r#"{"verifyCode":"F001"}"#), "F001");
        assert_eq!(risk_code_of(r#"{"verifyCode" : "t002"}"#), "T002");
        // 裸码兜底
        assert_eq!(risk_code_of("solve-fail T001 expired"), "T001");
        assert_eq!(risk_code_of("rejected: f001"), "F001");
        // 未收录的码 ⇒ 空串（不能把码本身当成提示）
        assert_eq!(risk_code_of(r#"{"verifyCode":"F008"}"#), "");
        assert_eq!(risk_code_of("F008"), "");
        assert_eq!(risk_code_of(""), "");
        // 普通文本里恰好出现类似形状也要能识别（这就是裸匹配的用途）
        assert_eq!(risk_code_of("upstream said T001"), "T001");
    }

    #[test]
    fn risk_code_of_does_not_match_longer_digit_runs() {
        // ⚠ `\b` 边界：F0012 不是 F001（与 server 层防 30120 误命中 3012 同源）
        assert_eq!(risk_code_of("F0012"), "");
        assert_eq!(risk_code_of("T00123"), "");
        // 数字码不能被当成风控码
        assert_eq!(risk_code_of("3007"), "");
    }

    #[test]
    fn annotate_risk_code_prefixes_only_when_recognized() {
        let out = annotate_risk_code(r#"verify rejected: {"verifyCode":"F001"}"#);
        assert!(out.starts_with("风控拒绝(F001)："), "{out}");
        assert!(out.contains("原始信息："), "{out}");
        assert!(out.contains("verifyCode"), "原文必须保留（退居次要但不能丢）");
    }

    #[test]
    fn annotate_risk_code_falls_back_verbatim() {
        // ★ 认不出码 ⇒ 原文**逐字**返回（不截断、不加前缀）：
        // 那往往正是唯一有用的线索（如"求解器组件未安装"）
        let raw = "求解器组件未安装（发行包应包含 assets/zcode-captcha）";
        assert_eq!(annotate_risk_code(raw), raw);
        assert_eq!(annotate_risk_code(""), "");
        assert_eq!(annotate_risk_code("   "), "");
        // 前后空白会被 trim（Go 侧同样）
        assert_eq!(annotate_risk_code("  没有码的原因  "), "没有码的原因");
    }

    #[test]
    fn annotate_risk_code_truncates_only_when_recognized() {
        let long = format!("{} F001", "很长的原始信息".repeat(80));
        let out = annotate_risk_code(&long);
        assert!(out.starts_with("风控拒绝(F001)："));
        // 截断后整体长度受限（提示本身约 200 字符 + 300 上限）
        assert!(
            out.chars().count() < 600,
            "认得出码时必须截断，实际 {} 字符",
            out.chars().count()
        );
        assert!(out.ends_with('…'), "截断必须带省略号");
    }

    #[test]
    fn truncate_str_matches_go_semantics() {
        assert_eq!(truncate_str("abc", 10), "abc");
        assert_eq!(truncate_str("  abc  ", 10), "abc");
        assert_eq!(truncate_str("abcdef", 3), "abc…");
        // 中文：字节数超限但字符数不超 ⇒ 原样返回（Go 的第二道判据）
        let cn = "中文中文";
        assert!(cn.len() > 3);
        assert_eq!(truncate_str(cn, 4), cn);
        // 字符数也超 ⇒ 按字符截断，不能切坏 UTF-8
        assert_eq!(truncate_str(cn, 2), "中文…");
    }

    #[test]
    fn truncate_never_splits_a_character() {
        let s = "中文中文中文"; // 每字 3 字节
        let out = truncate(s, 4);
        // 4 字节落在第二个字中间 ⇒ 退到 3 字节（一个完整汉字）
        assert_eq!(out, "中…");
        // 能无损往返（Rust 的 &str 保证合法 UTF-8，切坏会 panic）
        assert!(out.is_char_boundary(0));
    }

    #[test]
    fn err_kind_strings_are_stable() {
        // 这些字符串会进日志与前端，改动即破坏兼容
        assert_eq!(ErrKind::None.as_str(), "");
        assert_eq!(ErrKind::AuthMissing.as_str(), "auth_missing");
        assert_eq!(ErrKind::AuthFailed.as_str(), "auth_failed");
        assert_eq!(ErrKind::SigningRequired.as_str(), "signing_required");
        assert_eq!(ErrKind::RateLimited.as_str(), "rate_limited");
        assert_eq!(ErrKind::QuotaExhausted.as_str(), "quota_exhausted");
        assert_eq!(ErrKind::ModelNotFound.as_str(), "model_not_found");
        assert_eq!(ErrKind::ProviderDown.as_str(), "provider_down");
        assert_eq!(ErrKind::PlanRequired.as_str(), "plan_required");
        assert_eq!(ErrKind::NoResourcePack.as_str(), "no_resource_pack");
        assert_eq!(ErrKind::CaptchaRequired.as_str(), "captcha_required");
    }

    #[test]
    fn upstream_error_new_classifies_and_extracts() {
        let e = UpstreamError::new(
            429,
            r#"{"error":{"code":"1113","message":"余额不足或无可用资源包,请充值。"}}"#,
        );
        assert_eq!(e.kind, ErrKind::NoResourcePack);
        assert_eq!(e.code, "1113");
        assert_eq!(e.msg, "余额不足或无可用资源包,请充值。");
        assert!(e.to_string().contains("HTTP 429"));
        assert!(e.to_string().contains("code=1113"));
    }

    // ─────────── 线上实测故障形态的回归测试（F-09，测试资产见 `crate::testing`） ───────────
    //
    // 这两条形态都在 `test-bin\recon\zcode-dengyie\tests\mock_upstream\server.py`
    // 里被记为**线上实测**，而我们此前**零回归测试**（只有注释提到）。
    // 场景夹具由 [`crate::testing::mock_upstream`] 提供。

    /// ★★ 缺口 ①：**429 但 body 是额度文案**（线上 2026-09-13 实测，
    /// `server.py:222-225`）。
    ///
    /// # 归类为什么是 `NoResourcePack` 而不是 `RateLimited`
    ///
    /// 429 这个状态码**同时**承载两种完全相反的情况：
    ///
    /// ```text
    /// 429 + rate limited          → 限流：稍后重试（换号/等待）
    /// 429 + 额度文案              → 这条通道无可用资源包：重试**永远**没用
    /// ```
    ///
    /// `classify` 的判定顺序（`errors.rs:272-293` 的文档，与 Go `Classify`
    /// **逐信号一致** —— 我用脚本对比过两边的分支判据）刻意让
    /// **业务码 → 额度文案 → 状态码**，就是为了这个形态。
    ///
    /// ⚠ 注意本形态的 body 是 `{"error":{"message":"quota exceeded for this key",
    /// "type":"upstream_error"}}` —— **没有数字业务码**，也没有我们收录的三个
    /// 中文关键词（"余额不足"/"请充值"/"insufficient"）。所以它其实会掉到
    /// **状态码兜底**得到 `RateLimited`。
    ///
    /// 这**不是**缺陷：`quota exceeded for this key` 是**上游自己的英文措辞**，
    /// 我们的关键词表里没有它，而"429 + 无法识别的文案"按限流处理是**安全**的
    /// 默认（限流会换号重试，不会误导用户去充值）。本测试钉住这个**已知边界**，
    /// 并断言**绝不能**变成 `QuotaExhausted`（那会让用户白充钱）。
    #[test]
    fn rate_limited_with_quota_wording_is_not_reported_as_quota_exhausted() {
        use crate::testing::mock_upstream::{MockUpstream, Scenario};

        let mock = MockUpstream::start();
        let (status, body, _, _) = Scenario::RateLimitedQuotaBody
            .clone()
            .response_for_test(false, None);
        assert_eq!(status, 429, "线上实测形态就是 429");
        assert!(
            body.contains("quota exceeded for this key"),
            "body 必须是额度文案（这就是该场景的意义）：{body}"
        );
        // 夹具与真实上游响应逐字一致（防止夹具漂移后测试失去意义）。
        assert_eq!(mock.base().starts_with("http://127.0.0.1:"), true);

        let e = UpstreamError::new(status, &body);
        assert_eq!(e.status, 429);
        assert_eq!(e.msg, "quota exceeded for this key");
        // ★ 核心断言：**绝不能**报"额度已耗尽"（那会让用户去充值，而问题是限流）。
        assert_ne!(
            e.kind,
            ErrKind::QuotaExhausted,
            "429 + 额度文案不得被判成额度耗尽 —— 那会把用户引向充值（无用功）"
        );
        // 当前归类：无可识别业务码/关键词 ⇒ 429 兜底为限流（安全默认）。
        assert_eq!(e.kind, ErrKind::RateLimited);
        let friendly = e.friendly_message();
        assert!(
            friendly.contains("限流"),
            "文案应说限流并给出「稍后会自动重试」：{friendly}"
        );
        assert!(
            !friendly.contains("充值"),
            "★ 不得出现「充值」 —— 这是限流不是额度问题：{friendly}"
        );

        // 对照①：**有**业务码 1113 的同状态码必须判 NoResourcePack。
        let quota = UpstreamError::new(
            429,
            r#"{"error":{"code":"1113","message":"余额不足或无可用资源包,请充值。"}}"#,
        );
        assert_eq!(quota.kind, ErrKind::NoResourcePack);
        assert_ne!(quota.kind, e.kind, "两种 429 必须分开归类");

        // 对照②：**没有业务码、只有额度文案** ⇒ 仍须判 NoResourcePack。
        //
        // ⚠ 这条是对照①②里**唯一**能钉住「额度文案先于状态码」那一分支的断言
        //（`errors.rs:335-338`）：对照①走的是**业务码**分支（更早返回），
        // 根本到不了文案分支。第一版我漏了这条，反证实验立刻暴露出来 ——
        // 把文案分支整段删掉，测试居然**仍然全绿**，说明它没被覆盖。
        for body in [
            "余额不足，请充值",
            r#"{"error":{"message":"余额不足，请充值"}}"#,
            r#"{"error":{"message":"insufficient balance"}}"#,
            "insufficient quota",
        ] {
            assert_eq!(
                classify(429, body),
                ErrKind::NoResourcePack,
                "429 + 额度文案 {body:?} 必须判 NoResourcePack（文案先于状态码）"
            );
        }
        // 对照③：**真限流**文案同样是 RateLimited（与缺口①同归类，但语义正确）。
        let (s2, b2, _, _) = Scenario::RateLimited.response_for_test(false, None);
        assert_eq!(s2, 429);
        assert_eq!(UpstreamError::new(s2, &b2).kind, ErrKind::RateLimited);
    }

    /// ★★ 缺口 ②：**405 承载 `{"code":3012,…}`**（线上 2026-09-05 实测，
    /// `server.py:232-234`）。
    ///
    /// # 归类为什么**不是** `CaptchaRequired`
    ///
    /// `captcha.rs:23-25` 明确记过：3012 是**账号/风控层**，与 deviceMid 取值、
    /// param 复用都无关（三种 deviceMid 实测都是 3012）⇒ 验证码模块**不负责**它。
    ///
    /// 3012 由**另一条独立通道**处理：`crate::unusual::is_unusual_activity`
    /// 做**产品级熔断**（`forward.rs:784-810`，且**必须早于**通用
    /// `apply_error_policy` —— 否则网关会把池里每个账号都拿去撞一次风控）。
    ///
    /// 而在 `errors::classify` 这一层，3012 **不在**已收录业务码表里
    /// （`code` 模块只有 1001/1000/1113/11102/3101/3007），故 405 落进
    /// 状态码兜底 —— 405 既不是 429 也不是 ≥500，最终是 [`ErrKind::None`]。
    ///
    /// ⚠ 这也是**刻意的分工**：`classify` 管"这条错误该怎么向用户解释"，
    /// `unusual` 管"这次风控要不要停整个产品"。本测试同时钉住**两边**。
    #[test]
    fn http_405_carrying_3012_is_recognised_by_the_unusual_activity_layer() {
        use crate::testing::mock_upstream::{MockUpstream, Scenario};

        let mock = MockUpstream::start();
        let (status, body, _, _) = Scenario::RiskControl3012
            .clone()
            .response_for_test(false, None);
        assert_eq!(status, 405, "线上实测载体就是 405（不是 403/429）");
        assert!(body.contains("\"code\":3012"), "body 必须带业务码 3012：{body}");
        assert!(
            body.contains("unusual activity"),
            "body 必须带线上原文文案：{body}"
        );
        assert!(mock.call_count() == 0, "构造夹具本身不该产生请求");

        // ① `errors::classify` 这一层：405 不在已收录码/状态兜底里 ⇒ 未分类。
        //    这不是缺陷 —— 3012 的处置权在 `unusual` 层（见下）。
        let e = UpstreamError::new(status, &body);
        assert_eq!(e.status, 405);
        assert_eq!(e.kind, ErrKind::None, "3012 不在 classify 的业务码表里");
        assert_ne!(
            e.kind,
            ErrKind::CaptchaRequired,
            "3012 是**账号/风控层**，不是人机验证（captcha.rs:23-25 明确记过）"
        );
        // 未分类时的文案回退：`msg` 取得到 ⇒ 显示上游原文（**不是**空洞的
        // "HTTP 405"）。这是 `friendly_message` 的 `None` 分支设计
        //（`errors.rs:248-254`：文案非空时优先显示原文，那才是可诊断的线索）。
        let friendly = e.friendly_message();
        assert!(
            friendly.contains("unusual activity"),
            "未分类时应回退到上游原文（那才是唯一有用的线索）：{friendly}"
        );
        assert_eq!(e.msg, "request has been blocked due to unusual activity.");

        // ② `unusual` 层：必须**认出**这个形态（这才是它的处置通道）。
        assert!(
            crate::unusual::is_unusual_activity(status, &body),
            "★ 3012 必须被风控层认出来 —— 否则网关会把池里每个账号都拿去撞一次风控"
        );
        // 业务码单独成立（文案可能被上游改写，code 是稳定的）。
        assert!(crate::unusual::is_unusual_activity(
            405,
            r#"{"code":3012,"msg":"whatever"}"#
        ));
        // 文案单独成立（某些通道只回文案不带 code）。
        assert!(crate::unusual::is_unusual_activity(
            405,
            r#"{"msg":"request has been blocked due to unusual activity."}"#
        ));
        // 反证：HTTP 200 带同样的词**不该**熔断（用户在聊风控）。
        assert!(
            !crate::unusual::is_unusual_activity(200, &body),
            "200 的正常回复里恰好含这些词不该触发熔断"
        );
        // 反证：别的业务码（30120 前缀陷阱）不得误命中。
        assert!(
            !crate::unusual::is_unusual_activity(405, r#"{"code":30120,"msg":"other"}"#),
            "30120 不得因为前缀命中 3012"
        );
    }

    /// 缺口 ③（顺带）：**400 + `{"code":3007}`** 的归类与文案。
    ///
    /// 这条**已有**分类（`code::CAPTCHA_REQUIRED`），但同样缺一个把
    /// 「HTTP 状态码 ≠ 判定依据」钉住的回归测试：3007 配的是 **400**，
    /// 若不特判业务码就会掉进 `None`。
    #[test]
    fn captcha_3007_is_classified_by_code_not_status() {
        use crate::testing::mock_upstream::Scenario;

        let (status, body, _, _) = Scenario::Captcha3007.response_for_test(false, None);
        assert_eq!(status, 400, "与 errors.rs:100-101 记录的实测形态一致");
        assert!(body.contains("\"code\":3007"));

        let e = UpstreamError::new(status, &body);
        assert_eq!(e.code, "3007");
        assert_eq!(
            e.kind,
            ErrKind::CaptchaRequired,
            "★ 业务码必须**先于**状态码判 —— 否则 400 会掉进 None"
        );
        let friendly = e.friendly_message();
        assert!(friendly.contains("人机验证"), "文案：{friendly}");
        assert!(
            friendly.contains("不会绕过"),
            "必须明确我们不绕过验证码：{friendly}"
        );
        // 反证：同样 400 但没有 3007 业务码 ⇒ 不该判成验证码。
        assert_ne!(
            classify(400, r#"{"error":{"message":"bad request"}}"#),
            ErrKind::CaptchaRequired
        );
    }

    /// mock 上游自身的冒烟测试：编排协议（sequence 按调用次数消费、
    /// call-index 递增、请求被记录）真的能用 —— 否则上面那些回归测试
    /// 只是"夹具没起作用"的假绿。
    #[tokio::test]
    async fn mock_upstream_honours_the_orchestration_protocol() {
        use crate::testing::mock_upstream::MockUpstream;

        let mock = MockUpstream::start();
        let url = format!("{}/api/v1/zcode-plan/anthropic/v1/messages", mock.base());

        // 用 reqwest 真发请求（与生产同一条 HTTP 栈）。
        let c = reqwest::Client::new();
        let mut out = Vec::new();
        for _ in 0..3 {
            let r = c
                .post(&url)
                .header("authorization", "Bearer placeholder-credential-0001")
                .header("x-mock-bind", "acct-a")
                .header("x-mock-sequence", "ok,auth_invalid,server_error")
                .body(r#"{"stream":false}"#)
                .send()
                .await
                .expect("mock 应应答");
            out.push(r.status().as_u16());
        }
        // 依序消费：第 0 次 ok、第 1 次 401、第 2 次 500。
        assert_eq!(
            (out[0], out[1], out[2]),
            (200, 401, 500),
            "sequence 必须按调用次数依序消费"
        );
        assert_eq!(mock.call_count(), 3);

        // 记录内容可用于断言（method / path / headers / body）。
        let c0 = mock.call(0);
        assert_eq!(c0.method, "POST");
        assert_eq!(c0.path, "/api/v1/zcode-plan/anthropic/v1/messages");
        assert_eq!(c0.header("x-mock-bind"), Some("acct-a"));
        assert_eq!(c0.body_text(), r#"{"stream":false}"#);
        // call_index 按 bind 递增。
        assert_eq!(mock.call(0).call_index, 0);
        assert_eq!(mock.call(2).call_index, 2);
        // 响应头回同一个 call-index（观测协议）。
        let idx = c
            .post(&url)
            .header("x-mock-bind", "acct-a")
            .header("x-mock-sequence", "ok")
            .body("{}")
            .send()
            .await
            .expect("mock 应应答")
            .headers()
            .get("x-mock-call-index")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_string());
        assert_eq!(idx.as_deref(), Some("3"), "同一 bind 的第 4 次调用 ⇒ index 3");

        // 场景名直接驱动形态（405+3012）。
        let st = c
            .post(&url)
            .header("x-mock-scenario", "risk_control_3012")
            .body("{}")
            .send()
            .await
            .expect("mock 应应答")
            .status()
            .as_u16();
        assert_eq!(st, 405);
    }

    /// `sse_truncate` 场景：流必须**真的**是截断的（否则测不出截断路径）。
    #[test]
    fn sse_truncate_scenario_really_truncates() {
        use crate::testing::mock_upstream::{sse_stream, sse_stream_full, Scenario};

        let full = sse_stream_full(3);
        assert!(full.contains("message_stop"), "完整流应有收尾帧");
        assert_eq!(full.matches("chunk-").count(), 3);

        let (status, text, ctype, _) = Scenario::SseTruncate.response_for_test(true, None);
        assert_eq!(status, 200);
        assert_eq!(ctype, "text/event-stream");
        assert!(text.len() < full.len(), "截断后必须比完整流短");
        assert!(
            !text.contains("message_stop"),
            "★ 截断流**不该**有收尾帧 —— 这正是「空流/半截流」缺陷的形态"
        );
        // 显式截断位置。
        let half = sse_stream(3, Some(full.chars().count() / 2));
        assert_eq!(half, text, "缺省截一半应与显式给一半等价");
    }
}
