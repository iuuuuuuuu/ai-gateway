//! 上游错误分类与文案提炼。
//!
//! 对应 Go 源文件 `internal/upstream/client.go` 的 `Classify` / `IsContextTooLong` /
//! `IsModelRateLimited` / `ParseResetTime` / `FriendlyMessage` / `ContextTooLongMessage`。
//!
//! # 分类优先级（必须与 Go 逐条一致）
//!
//! 1. `402` → 硬冷却（余额）
//! 2. `error.data.code == 14018` → 硬冷却（额度耗尽，**嵌套层级**）
//! 3. body 命中 hardMarkers（小写 + 中文原文双通道）→ 硬冷却
//! 4. body 命中 sessionDeadMarkers → 禁用
//! 5. `429` → 模型级限流（6004）优先，否则软冷却
//! 6. 上下文超长（11115）→ 请求侧错误，换号无用
//! 7. 请求被拒（11155 思维链缺失）→ 请求侧错误，原地重试后立即失败
//! 8. 思考档位被拒 → 请求侧错误
//! 9. 模型不在这条通道上（11102）→ 请求侧错误
//! 10. 请求参数被模型提供商拒（11133）→ 请求侧错误
//! 11. `404` → 短冷却
//! 12. 账号不可用（403 code=11140）→ **账号侧**软冷却
//! 13. `>=500` → 服务端错误（喂熔断）
//! 14. `>=400` → 客户端错误（只换号不罚）
//! 15. 其余 → 无错误
//!
//! 四处**非显然的顺序**，改前务必读完：
//!
//! - 第 2 步必须在 `429` 之前：额度耗尽的 HTTP 状态恰恰是 429，若不先拦，
//!   会落进软冷却分支只冷却 60 秒 → 60 秒后重试同一个已耗尽账号 → 无限重试。
//! - 第 6 步必须在通用 `4xx` 之前、但在 `429` 之后：上下文超长是请求侧错误，
//!   需要独立 kind 让调用方「立即失败」而不是把整个请求体对着每个账号重传。
//! - ★★ 第 12 步（`ErrKind::AccountUnusable`）必须在**通用 `4xx` 之前**。
//!   11140 的 HTTP 状态是 **403**，落进通用 `4xx` 就变成 `Client`，而
//!   `apply_error_policy` 对 `Client` 是「只换号不罚」⇒ 这些账号**永不冷却**
//!   ⇒ 每轮换号都被重新选中 ⇒ 请求在「坏号集合」里空转直到换号预算用尽。
//!   这正是 Go 侧修掉的生产缺陷：实测 8 个国际版账号裸名
//!   `deepseek-v4.1-flash` **0/6 全败（全 502）**，把坏号冷却后立刻成功。
//! - ★★ 11140 是**账号侧**错误，不是请求侧。判据（Go `client.go:760-762` 逐字）：
//!   **同一请求在不同账号上结果不同**。实测（2026-09-28，同请求体同 UA 同端点
//!   直连上游）uid `a1b2c3d4` refresh 200 ✅ chat 200 ✅，其余 5 个账号
//!   refresh 200 ✅ 但 chat 403 ❌ 11140 —— 若是请求侧错误，它们应当**全部**失败。
//!   三条否证也都做过：不是 UA（用官方客户端同款 UA 直连结果一致）、
//!   不是 token 过期（8 个账号 AT 都还有 350+ 天，刷新成功后立刻重试仍 11140）、
//!   不是缺 `X-Device-Token`（9 个账号三组对照矩阵，带真 OIDC 登录的 DT 与自造
//!   UUID 都不改变结果 ⇒ 上游的 (设备,账号) 绑定是**服务端记录**，
//!   客户端无法用请求头伪造或迁移）。真实成因是**该账号在本设备上未完成登录**。

use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};

/// 错误分类。驱动账号池的冷却状态机。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrKind {
    /// 成功。
    #[default]
    None,
    /// 余额不足（402 / 14018 / body 关键词）→ 长冷却。
    HardCredit,
    /// 429 软限流（账号级）→ 短冷却。
    SoftRate,
    /// session 失效（401 + 12153）→ 禁用。
    SessionDead,
    /// 404 上游偶发 → 短冷却，防雪崩。
    NotFound,
    /// 5xx 上游故障。
    Server,
    /// 其他 4xx / 业务错误。
    Client,
    /// 模型级限流（429 code=6004）：该账号的**这个模型**额度用尽。
    ///
    /// 与 [`ErrKind::SoftRate`] 的区别是该账号其他模型仍然可用 ——
    /// 上游文案明确写着「您也可以切换其他模型继续使用」。
    ModelRate,
    /// 请求上下文超出模型窗口（400 code=11115）。
    ///
    /// **请求侧**错误：同一请求体发给任何账号都会同样失败，换号无用。
    ContextTooLong,
    /// 请求内容被上游明确拒绝（400 code=11155 思维链缺失）。
    ///
    /// 与 [`ErrKind::ContextTooLong`] 同属**请求侧**错误：同一请求体发给
    /// 任何账号结果都一样，轮转账号不仅无用，还会把「这条请求不合法」伪装成
    /// 「账号全部不可用」，并白白冷却整个账号池。
    RequestRejected,
    /// 上游不接受本次指定的思考档位（400，`reasoning_effort` 不支持）。
    ///
    /// **请求侧**错误。★ **不要**据此在网关侧预校验档位：上游声明的
    /// `supportedEfforts` 不是硬范围（实测 hy3 声明 `[low,high]` 却接受
    /// medium/max/off），网关侧拦截会把合法请求挡在门外。
    EffortRejected,
    /// 该模型在这条通道上不存在（400 code=11102）。
    ///
    /// **请求侧**错误，且与「排队」**完全不同**：排队（10605）是上游主动限流、
    /// 稍后可用；这个是「这条通道压根没有这个模型」，**等多久都没用**，
    /// 必须换模型或换区域。所有者 2026-09-20 现场：裸名 `Qwen3.8-Flash`
    /// 挑到没有该模型的区域 → 11102 → 被展示成「all accounts unavailable」→
    /// 他的反应是「这个模型不能用」，真实原因是**区域没指定**。
    ModelNotInRegion,
    /// 请求参数不符合**当前模型**的要求（400 code=11133）。
    ///
    /// **请求侧**错误。⚠ **不与 [`ErrKind::ContextTooLong`] 合并**：那条让客户端
    /// 触发**自动压缩**，而 11133 的 `param` 是空串、未必与长度有关，误导客户端
    /// 压缩无济于事。所有者 2026-09-28 现场：417 轮 / 458M token 的开发会话报
    /// 400 11133，被当成账号故障逐号轮转 → 503「all accounts unavailable」；
    /// 而真实原因写在 `extError` 里 ——「rejected by the model provider」，
    /// 与账号毫无关系。
    ModelParamInvalid,
    /// 账号在本设备不可用（403 code=11140 `request illegal`）→ **账号侧**软冷却。
    ///
    /// ★★ 与上面四个「请求侧」错误**方向相反**：它由**账号侧**状态决定，
    /// 换个账号就能成功。判据是**同一请求在不同账号上结果不同**。
    ///
    /// 为什么必须独立成型（Go `client.go:86-160` 逐字）：它此前落进通用
    /// `ErrClient`，而 `apply_error_policy` 对 `Client` 是「只换号不罚」
    ///（防雪崩）⇒ 这些账号**永不冷却** ⇒ 每轮换号都被重新选中 ⇒ 请求在
    /// 「坏号集合」里空转直到换号预算用尽。实测 8 个国际版账号裸名
    /// `deepseek-v4.1-flash` 修复前 **0/6 成功（全 502）**。
    ///
    /// ★ 与所有者的调度模型直接相关：「账号不可用就冷却，然后这时候再自动
    /// 降级到（下一个成本档位）」—— **「冷却」是自动降级能发生的前提**，
    /// 不冷却则成本分层永远卡在最便宜但不可用的那一档，降级不下去。
    ///
    /// 处置用 `CoolSoft`（有到期、可自动恢复）而**不是** `Disable`：
    /// 上游根因是「该账号在本设备未完成登录（设备注册失效）」，用户去官方
    /// 客户端重登一次即可恢复 —— 与 [`ErrKind::SessionDead`]「凭证失效必须
    /// 重登」的恢复动作不同，且**绝不永久禁用**。
    AccountUnusable,

    /// 上游说「**这条通道没有该账号的资源包**」——**不等于**账号没额度。
    ///
    /// # 为什么必须与 [`ErrKind::HardCredit`] 分开
    ///
    /// ZCode 的业务码 `1113` 的原文是「余额不足或**无可用资源包**，请充值」，
    /// 但它配的 HTTP 状态是 **429**，真实含义是「这条通道上没有该模型的资源包」
    ///（Go 侧 `go-gateway/internal/zcode/provider.go:182` 逐字）。实测：一个
    /// 剩余 9,977 万 / 1 亿额度的满额账号，因为收到一条 `glm-5.3` 请求
    ///（该账号套餐里只有 `GLM-5.3-Flash`）被判成 `HardCredit` ⇒
    /// `cooldown_until_tomorrow_4am` ⇒ 整号硬冷却到次日 04:00，界面显示
    /// 「余额不足（积分欠费）」——**完全错误的方向**。
    ///
    /// 处置必须是「**软**冷却 + 只封这个模型」：账号级 `until` 让
    /// `healthy()` 为假、`model_until` 让 `model_available()` 为假，
    /// 主路径与全部兜底（含 `pick_earliest_expiry`）都不会再选到它，
    /// 而该账号的其它模型与其它账号完全不受影响。
    NoResourcePack,
}

impl ErrKind {
    /// 序列化名，与 Go 侧 `String()` 一致。
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrKind::None => "none",
            ErrKind::HardCredit => "hard_credit",
            ErrKind::SoftRate => "soft_rate",
            ErrKind::SessionDead => "session_dead",
            ErrKind::NotFound => "not_found",
            ErrKind::Server => "server",
            ErrKind::Client => "client",
            ErrKind::ModelRate => "model_rate",
            ErrKind::ContextTooLong => "context_too_long",
            ErrKind::RequestRejected => "request_rejected",
            ErrKind::EffortRejected => "effort_rejected",
            ErrKind::ModelNotInRegion => "model_not_in_region",
            ErrKind::ModelParamInvalid => "model_param_invalid",
            ErrKind::AccountUnusable => "account_unusable",
            ErrKind::NoResourcePack => "no_resource_pack",
        }
    }
}

impl std::fmt::Display for ErrKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 带分类的上游错误。
///
/// `Display` 与 Go 侧 `(*Error).Error()` 同格式：
/// `upstream {kind} (http {status}): {msg}`
#[derive(Debug, Clone)]
pub struct UpstreamError {
    /// 错误分类。
    pub kind: ErrKind,
    /// HTTP 状态码。
    pub status: u16,
    /// 原始响应体。
    pub msg: String,
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "upstream {} (http {}): {}", self.kind, self.status, self.msg)
    }
}

impl std::error::Error for UpstreamError {}

/// 余额不足关键词（小写比较 + 中文原文比较双通道）。
///
/// 为什么要收单复数两种写法：上游国际版实际返回的是
/// `Credits exhausted. ...`（**复数** Credits），而早期只登记了单数
/// `credit exhausted`，于是包含匹配恒不命中 → 被判成软冷却 60 秒 →
/// 60 秒后重试同一个已耗尽账号，形成无限重试。
const HARD_MARKERS: &[&str] = &[
    "insufficient credit",
    "insufficient credits",
    "no credit",
    "no credits",
    "credit exhausted",
    "credits exhausted",
    "credit exhaustion",
    "credits exhaustion",
    "out of credit",
    "out of credits",
    "quota exceeded",
    "quota exhaust",
    "payment required",
    "credit not enough",
    "credits not enough",
    "not enough credit",
    "not enough credits",
    "credit used up",
    "credits used up",
    "积分不足",
    "额度不足",
    "余额不足",
    "积分用完",
    "额度用尽",
    "没有积分",
];

/// session 失效标记。
const SESSION_DEAD_MARKERS: &[&str] = &["Offline user session not found", "12153"];

/// 模型级限流的文案兜底（**必须**同时是 429 才成立）。
const MODEL_RATE_MARKERS: &[&str] = &["超出频率限制", "切换其他模型"];

/// 上游「模型级限流」的业务码。
const MODEL_RATE_CODE: i64 = 6004;

/// 上游「上下文超长」的业务码。
const CONTEXT_TOO_LONG_CODE: i64 = 11115;

/// 上下文超长的判定文案（中英双通道兜底）。
///
/// 业务码是主信号；文案兜底用于上游改码不改文案的场景。措辞取自上游真实响应，
/// 刻意含中英两版 displayMsg —— 上游按 Accept-Language 切换语言，只认一种会漏判。
///
/// 注意 msg 有多种写法：实测同一业务码下遇到过 `prompt is too long`（国际版）
/// 与 `input length too long`（国服 glm-5.3），两种都要登记。
const CONTEXT_TOO_LONG_MARKERS: &[&str] = &[
    "context_length_exceeded",
    "prompt is too long",
    "input length too long",
    "exceeds the model context limit",
    "对话内容超出模型长度上限",
    "超出模型长度上限",
];

/// 上游「额度耗尽」的业务码。
///
/// 与 [`MODEL_RATE_CODE`] 的关键区别在**嵌套层级**：6004 在顶层 `code`，
/// 而 14018 藏在 `error.data.code`。
const CREDIT_EXHAUSTED_CODE: i64 = 14018;

/// 上游「请求内容被拒绝」的业务码（请求侧错误，换号无用）：
/// - `11155`：思考模式要求回传上一轮 reasoning_content（reasoning_content_missing）
///
/// ★ `11140` **不在此列** —— 它是**账号侧**错误（见 [`ErrKind::AccountUnusable`]）。
/// 曾经它被收在这里，后果是把「这个号在这台设备上打不通」说成「这条请求不合法」，
/// 于是坏号永不冷却、每轮换号都被重新选中。判据见 [`is_account_unusable`]。
const REQUEST_REJECTED_CODES: &[i64] = &[11155];

/// 「请求被拒绝」的文案兜底（中英双通道）。
///
/// 业务码是主信号；文案兜底用于上游改码不改文案 / 业务码放在 extError.code
/// 字符串里（11155 的实测形状）的场景。
///
/// ★ 不含 `did not pass the safety review` / `内容未通过安全审核`：那是
/// `11140` 的旧文案，而 11140 已改判为**账号侧**（见 [`is_account_unusable`]）。
const REQUEST_REJECTED_MARKERS: &[&str] = &[
    "reasoning_content_missing",
    "reasoning content from the previous turn",
];

/// 上游「账号在当前设备不可用」的业务码。
///
/// ⚠ 只收「确定是账号侧」的码。判据是**同一请求在不同账号上结果不同** ——
/// 若某个码对任何账号都同样失败，那它是**请求侧**错误（应像 11102 那样独立成型、
/// 不冷却账号），加进来只会让健康账号被误冷却。
const ACCOUNT_UNUSABLE_CODES: &[&str] = &["11140"];

/// 思考档位被拒的文案特征（全小写比较）。
///
/// 实测原文（2026-09-18，真实流式调用，HTTP 400）：
/// `the reasoning effort value is not supported by the current model`
const EFFORT_REJECTED_MARKERS: &[&str] = &[
    "reasoning effort value is not supported",
    "reasoning_effort is not supported",
    "unsupported reasoning effort",
];

/// 「模型在这条通道上不存在」的文案特征。
///
/// 只放**足够特异**的片段 —— 通用词（如 `model not found`）会误伤正常报错。
/// 主判据仍是业务码 11102。
const MODEL_NOT_IN_REGION_MARKERS: &[&str] = &[
    "service info not found", // 实测原文：model [X] service info not found
    "model service not found",
    "model info not found",
];

/// 上游业务时区（CST，UTC+8），用于解释**无时区后缀**的时间字面量。
///
/// 为什么不用本机时区：上游是国服服务，其重置时刻按 CST 计；用本机时区解释会让
/// 同一份响应在不同时区的机器上得出相差数小时的结果，极端情况下会把尚未到期的
/// 重置点误判成「已过期」而退化成固定软冷却。中国无夏令时，固定 +8 即可。
fn upstream_zone() -> FixedOffset {
    FixedOffset::east_opt(8 * 3600).expect("+08:00 恒定合法")
}

/// 报告响应体是否为「额度耗尽」业务码（含嵌套层级）。
fn is_credit_exhausted_code(body: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    v.get("error")
        .and_then(|e| e.get("data"))
        .and_then(|d| d.get("code"))
        .and_then(|c| c.as_i64())
        == Some(CREDIT_EXHAUSTED_CODE)
}

/// 取上游统一信封的顶层 `code`（非数字或缺失返回 `None`）。
fn envelope_code(body: &str) -> Option<i64> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("code")?
        .as_i64()
}

/// 报告上游响应是否为「这条通道没有该账号的资源包」。
///
/// 与 [`is_credit_exhausted_code`] 的区别是**语义**而不是形状：那个说的是
///「账号真的没钱了」（14018 / 积分不足 / 余额不足），这个说的是「上游认为该
/// 模型不在这个账号的套餐 / 资源包里」——额度可能一分没花。
///
/// 判据（与 [`is_context_too_long`] 同款设计哲学：**不按 status 门控**，
/// 判定依据是业务语义而不是状态码）：
///
/// - body 出现「无可用资源包」⇒ 与状态码无关，直接命中（这句原文只可能来自 ZCode）；
/// - `status == 429` 且 body 出现「余额不足」⇒ 命中。**必须带 429 门控**：
///   WorkBuddy 的 `(400, {"code":1,"msg":"余额不足"})` 是**真的**额度耗尽，
///   那是 `matches_go_testclassify_matrix` 里钉死的断言，不能被这里改判；
/// - `status == 429` 且业务码为 `1113`（只有码、没有中文时兜底）。
fn is_zcode_no_resource_pack(status: u16, body: &str) -> bool {
    if body.contains("无可用资源包") {
        return true;
    }
    // 以下判据都必须带 429 门控：402 / 400 的「余额不足」是真的没额度。
    if status != 429 {
        return false;
    }
    if body.contains("余额不足") {
        return true;
    }
    crate::products::zcode::errors::extract_code(body)
        == crate::products::zcode::errors::code::QUOTA_EXHAUSTED
}

/// 报告上游响应是否为「请求上下文超出模型窗口」。
///
/// 三路判定，任一命中即成立：业务码 11115、`extError.code=context_length_exceeded`、
/// 或真实文案关键词。
///
/// **不按 status 门控**：上游以 400 为主，但判定依据是业务语义而非状态码，
/// 上游若改用 413 也能识别。
pub fn is_context_too_long(body: &str) -> bool {
    if envelope_code(body) == Some(CONTEXT_TOO_LONG_CODE) {
        return true;
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if v.get("extError")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            .map(|c| c.eq_ignore_ascii_case("context_length_exceeded"))
            .unwrap_or(false)
        {
            return true;
        }
    }
    let lower = body.to_lowercase();
    CONTEXT_TOO_LONG_MARKERS
        .iter()
        .any(|m| lower.contains(&m.to_lowercase()))
}

/// 报告响应是否为「请求内容被上游明确拒绝」（请求侧错误，换号无用）。
///
/// 三路判定，任一命中即成立：顶层业务码 11155、
/// `extError.code` 为对应语义串（11155 实测放在这里）、或真实文案关键词。
/// 不按 status 门控：实测为 400，判定依据是业务语义。
pub fn is_request_rejected(body: &str) -> bool {
    if let Some(code) = envelope_code(body) {
        if REQUEST_REJECTED_CODES.contains(&code) {
            return true;
        }
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if v.get("extError")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            .map(|c| c.eq_ignore_ascii_case("reasoning_content_missing"))
            .unwrap_or(false)
        {
            return true;
        }
    }
    let lower = body.to_lowercase();
    REQUEST_REJECTED_MARKERS
        .iter()
        .any(|m| lower.contains(&m.to_lowercase()) || body.contains(m))
}

/// 报告响应是否表达「该账号在当前设备打不通」（403 code=11140）。
///
/// ★ 这是**账号侧**判定，与 [`is_request_rejected`] 的四个「请求侧」判定方向相反。
/// 实测原文（2026-09-27）：它由账号侧状态决定 —— 同一份请求体、同一个模型，
/// 有的账号 200、有的账号 403 11140，且后者在官方客户端上同样打不通。
///
/// 按业务码匹配（`strings.Contains`，与 Go `IsAccountUnusable` 逐字一致）：
/// 上游把 11140 放在顶层 `code` 还是文案里都能命中。
pub fn is_account_unusable(body: &str) -> bool {
    ACCOUNT_UNUSABLE_CODES.iter().any(|c| body.contains(c))
}

/// 报告响应是否为「上游不接受本次指定的思考档位」（请求侧错误）。
///
/// 文案匹配（全小写）。实测原文：
/// `the reasoning effort value is not supported by the current model`。
pub fn is_effort_rejected(body: &str) -> bool {
    let lower = body.to_lowercase();
    EFFORT_REJECTED_MARKERS.iter().any(|m| lower.contains(m))
}

/// 报告响应是否为「该模型在这条通道上不存在」（11102，请求侧错误）。
///
/// 按**业务码 + 文案**双判（与 Go `IsModelNotInRegion` 逐字一致）：
/// 只认业务码会漏（部分上游把这句放在 `error.message` 里而不回 code）；
/// 只认文案会误伤（正常的「模型不存在」也可能出现在别处）。两者取「或」。
///
/// ⚠ 与 10605（排队）**完全不同**：那个是上游主动限流、稍后可用；
/// 这个是「这条通道压根没有这个模型」，**等多久都没用**。
pub fn is_model_not_in_region(body: &str) -> bool {
    if body.contains("11102") {
        return true;
    }
    let lower = body.to_lowercase();
    MODEL_NOT_IN_REGION_MARKERS.iter().any(|m| lower.contains(m))
}

/// 报告响应是否为「请求参数被模型提供商拒绝」（11133，请求侧错误）。
///
/// 与 Go `IsModelParamInvalid` 逐字一致：`strings.Contains(body, "11133")`
/// 或全小写含 `model_param_invalid`。
///
/// ⚠ `param` 字段上游给了**空串**，拿不到具体是哪个参数 —— 故只能如实转达，不猜。
/// 特别提示：**不要**把它归到上下文超长去，那会让客户端误触发压缩，
/// 而压缩并不会解决参数问题。
pub fn is_model_param_invalid(body: &str) -> bool {
    if body.contains("11133") {
        return true;
    }
    body.to_lowercase().contains("model_param_invalid")
}

/// 报告 429 响应体是否为「模型级限流」（该账号该模型额度用尽）。
pub fn is_model_rate_limited(body: &str) -> bool {
    if envelope_code(body) == Some(MODEL_RATE_CODE) {
        return true;
    }
    // 文案兜底：上游改码不改文案时仍能识别。
    MODEL_RATE_MARKERS.iter().any(|m| body.contains(m))
}

/// 按 HTTP 状态码 + body 判定错误类别。
pub fn classify(status: u16, body: &str) -> ErrKind {
    // ★★ 必须**最先**判定（早于 402 与 HARD_MARKERS）：ZCode 的 1113 原文里带
    // 「余额不足」，会被下面的 HARD_MARKERS 命中并判成 `HardCredit` ⇒ 整号硬
    // 冷却到次日 04:00，而账号其实一分额度没花（详见 [`ErrKind::NoResourcePack`]）。
    if is_zcode_no_resource_pack(status, body) {
        return ErrKind::NoResourcePack;
    }
    if status == 402 {
        return ErrKind::HardCredit;
    }
    // 额度耗尽的业务码优先判定：它的 HTTP 状态是 429，若不先拦，
    // 会落进下面的 429 分支被判成软冷却（仅 60 秒）→ 无限重试。
    if is_credit_exhausted_code(body) {
        return ErrKind::HardCredit;
    }
    let lower = body.to_lowercase();
    for m in HARD_MARKERS {
        // 双通道：小写比较（英文）或原文比较（中文，无大小写）。
        if lower.contains(&m.to_lowercase()) || body.contains(m) {
            return ErrKind::HardCredit;
        }
    }
    for m in SESSION_DEAD_MARKERS {
        if body.contains(m) {
            return ErrKind::SessionDead;
        }
    }
    if status == 429 {
        // 模型级限流优先于账号级软限流：两者的冷却粒度与时长都不同
        //（模型级按 uid+model 冷却到上游给的重置时间）。
        if is_model_rate_limited(body) {
            return ErrKind::ModelRate;
        }
        return ErrKind::SoftRate;
    }
    // 上下文超长必须早于通用 4xx 判定：它是请求侧错误，换号无用。
    // 放在 429 之后是有意的：429 一律按限流归类，保持既有语义不变。
    if is_context_too_long(body) {
        return ErrKind::ContextTooLong;
    }
    // 请求侧错误（11155 思维链缺失）：立即失败，不轮转、不罚账号。
    if is_request_rejected(body) {
        return ErrKind::RequestRejected;
    }
    // 思考档位被拒：请求侧错误。必须早于通用 4xx —— 落进去会被当成账号故障
    // 逐号轮转，而所有者恰恰要靠这条文案判断「我用的档位到底行不行」。
    if is_effort_rejected(body) {
        return ErrKind::EffortRejected;
    }
    // 11102：请求侧错误。必须早于通用 4xx —— 落进去会被逐号轮转，
    // 把「区域没指定」伪装成「账号全挂了」。
    if is_model_not_in_region(body) {
        return ErrKind::ModelNotInRegion;
    }
    // 11133：请求侧错误。必须早于通用 4xx —— 落进去会被当成账号问题逐号轮转，
    // 每个账号都被同一个请求体打一遍，最后包装成 503「all accounts unavailable」。
    if is_model_param_invalid(body) {
        return ErrKind::ModelParamInvalid;
    }
    if status == 404 {
        return ErrKind::NotFound;
    }
    // ★★ 11140（账号侧）必须早于通用 4xx：它的 HTTP 状态是 403，
    // 落进下面就会变成 Client，而 apply_error_policy 对 Client 是「只换号不罚」
    // ⇒ 坏号永不冷却 ⇒ 每轮换号都被重新选中 ⇒ 请求在坏号集合里空转。
    // 实测（8 个国际版账号）：修复前 0/6 成功（全 502）。
    if is_account_unusable(body) {
        return ErrKind::AccountUnusable;
    }
    if status >= 500 {
        return ErrKind::Server;
    }
    if status >= 400 {
        return ErrKind::Client;
    }
    ErrKind::None
}

/// 从上游报错文案里提取重置时刻；解析不出返回 `None`。
///
/// 实测文案：
/// `您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，您也可以切换其他模型继续使用。`
///
/// 时区处理：
/// - `UTC+8` / `UTC+08:00` → 固定偏移（**实测文案用的就是这种**）
/// - `Z` → UTC
/// - 无时区后缀 / 后缀非法 → 按上游业务时区（CST）解释
///
/// 只返回**未来**的时刻：解析出过去的时间说明文案里的重置点已过（如重放旧日志），
/// 此时返回 `None` 交给调用方回退固定冷却，避免写入一个立即失效的冷却。
pub fn parse_reset_time(body: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let (literal, tz) = find_reset_literal(body)?;
    let naive = NaiveDateTime::parse_from_str(&literal, "%Y-%m-%d %H:%M:%S").ok()?;
    let offset = match tz.as_deref() {
        None => upstream_zone(),
        Some("Z") => FixedOffset::east_opt(0).expect("UTC 合法"),
        Some(t) => parse_utc_offset(t).unwrap_or_else(upstream_zone),
    };
    let ts = offset.from_local_datetime(&naive).single()?;
    let ts = ts.with_timezone(&Utc);
    if ts <= now {
        return None;
    }
    Some(ts)
}

/// 在文案里定位第一个 `YYYY-MM-DD[ T]HH:MM:SS` 及其可选的时区后缀。
///
/// 手写扫描而非正则：格式固定且不需要回溯，省掉一个正则引擎依赖。
fn find_reset_literal(body: &str) -> Option<(String, Option<String>)> {
    let b = body.as_bytes();
    let mut i = 0usize;
    while i + 19 <= b.len() {
        if is_date_at(b, i) {
            let sep = b[i + 10];
            if sep == b' ' || sep == b'T' {
                let literal = format!(
                    "{} {}",
                    &body[i..i + 10],
                    &body[i + 11..i + 19]
                );
                // 时区后缀：紧随其后，可选的空白 + ("UTC±H[:MM]" | "Z")
                let rest = &body[i + 19..];
                let trimmed = rest.trim_start();
                let tz = if let Some(stripped) = trimmed.strip_prefix('Z') {
                    let _ = stripped;
                    Some("Z".to_string())
                } else if let Some(stripped) = trimmed.strip_prefix("UTC") {
                    let mut end = 0usize;
                    let sb = stripped.as_bytes();
                    if end < sb.len() && (sb[end] == b'+' || sb[end] == b'-') {
                        end += 1;
                        while end < sb.len() && sb[end].is_ascii_digit() {
                            end += 1;
                        }
                        if end < sb.len() && sb[end] == b':' {
                            end += 1;
                            while end < sb.len() && sb[end].is_ascii_digit() {
                                end += 1;
                            }
                        }
                        Some(format!("UTC{}", &stripped[..end]))
                    } else {
                        None
                    }
                } else {
                    None
                };
                return Some((literal, tz));
            }
        }
        i += 1;
    }
    None
}

/// 判断 `b[i..]` 是否为 `YYYY-MM-DD`（纯数字 + 两个连字符）。
fn is_date_at(b: &[u8], i: usize) -> bool {
    let digits = |s: &[u8]| s.iter().all(|c| c.is_ascii_digit());
    b.len() >= i + 10
        && digits(&b[i..i + 4])
        && b[i + 4] == b'-'
        && digits(&b[i + 5..i + 7])
        && b[i + 7] == b'-'
        && digits(&b[i + 8..i + 10])
}

/// 解析 `UTC+8` / `UTC-05:30` 形式的固定偏移时区；非法返回 `None`。
fn parse_utc_offset(s: &str) -> Option<FixedOffset> {
    let rest = s.strip_prefix("UTC")?;
    let mut chars = rest.chars();
    let sign = match chars.next()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    let rest = chars.as_str();
    let (h, m) = match rest.split_once(':') {
        Some((h, m)) => (h.parse::<i32>().ok()?, m.parse::<i32>().ok()?),
        None => (rest.parse::<i32>().ok()?, 0),
    };
    if h > 23 || m > 59 {
        return None;
    }
    FixedOffset::east_opt(sign * (h * 3600 + m * 60))
}

/// 把上游的原始错误体提炼成一句可读的原因，供客户端展示。
///
/// 背景：此前直接把整段上游 JSON 拼进错误体的 message，客户端看到的是
/// 「all accounts unavailable (cooling/disabled): upstream soft_rate (http 429):
/// {"error":{"data":{"code":14018,...}}}」—— 又长又难懂。
///
/// 返回空串表示没有更优的表述，调用方应回退到原始文案。
pub fn friendly_message(kind: ErrKind, status: u16, body: &str) -> String {
    // 业务码优先，但**只有响应体里真的带 14018 时才把该码写进文案**：
    // HardCredit 也可能来自 HTTP 402 或关键词命中，此时硬写「上游 14018」
    // 会让用户拿着一个与响应不符的码去排查。
    if is_credit_exhausted_code(body) {
        return format!(
            "账号额度已耗尽（上游 {CREDIT_EXHAUSTED_CODE}）：请为该账号充值，或等待签到 / 免费额度恢复后重试"
        );
    }
    match kind {
        ErrKind::HardCredit => {
            "账号额度已耗尽：请为该账号充值，或等待签到 / 免费额度恢复后重试".to_string()
        }
        ErrKind::NoResourcePack => {
            // ★ 绝不提「充值」（这不是余额问题），也绝不出现裸 401/403
            //（DSH 用 `/\b(?:401|403)\b/` 判 AUTH，见本函数上方注释）。
            "该 ZCode 账号在这条通道上没有可用资源包（上游业务码 1113）——不一定是额度不足，通常是这个模型不在该账号的套餐里。已仅对该模型做短暂冷却并自动换号；若持续失败，请改用其它账号，或先在 ZCode 官方客户端确认该账号的套餐资格。".to_string()
        }
        ErrKind::ModelRate => {
            "该账号在此模型上已达频率上限，已按上游给出的重置时间冷却；同一账号的其他模型仍可用"
                .to_string()
        }
        ErrKind::SoftRate => {
            "账号被上游限流（HTTP 429），已短暂冷却，稍后会自动重试".to_string()
        }
        ErrKind::SessionDead => "账号登录态已失效，需在「账号管理」页重新登录".to_string(),
        // ★★ 文案里**绝对不能出现 "403" 这三个字符**，也**不透出上游原文**
        //（`request illegal` 那种措辞会让人以为是自己请求写错了，而实际请求完全合法）。
        // 归因改用**业务码 11140** —— 对排查同样唯一，且不含任何会被误判的数字。
        //
        // 背景（Go `client.go:1050-1090` 逐字，2026-09-29 实测）：DSH 客户端判
        // 「密钥无效」靠的不是 HTTP 状态码而是**文案正则**
        //（`classifyPiAiError`：`if (/\b(?:401|403)\b/.test(message)) return "AUTH";`），
        // 而 `formatProviderError` 会把 `(status): body` **原样拼进 message**
        // ⇒ 只要文案里出现裸 "401"/"403"，不管 HTTP 状态码是什么，都被判 AUTH。
        // 上一版只把 403 改写成 502 是**无效**的，因为文案里还留着「上游 403 业务码 11140」。
        ErrKind::AccountUnusable => "账号在本设备尚未完成登录（上游业务码 11140），已冷却该账号并自动切换；若持续失败请在「账号管理」页重新登录该账号".to_string(),
        ErrKind::NotFound => {
            "上游返回 404（接口或模型不存在），已短暂冷却并切换账号".to_string()
        }
        ErrKind::EffortRejected => {
            format!(
                "上游不接受本次指定的思考档位（reasoning_effort）：{}",
                effort_rejected_detail(body)
            )
        }
        ErrKind::ModelParamInvalid => model_param_invalid_message(body),
        ErrKind::ModelNotInRegion => model_not_in_region_message("", body),
        ErrKind::Server if status > 0 => {
            format!(
                "上游服务异常（HTTP {}），已切换到其他账号",
                server_status_label(status)
            )
        }
        _ => String::new(),
    }
}

/// 上游状态码在客户端文案里的安全写法。
///
/// 只有 `500..=599` 原样输出，其余一律收敛成 `"5xx"`。
///
/// ★ 为什么不赌调用方永远传 5xx：一旦写出裸 `401`/`403`，客户端会把
/// 「上游服务异常」读成「API 密钥无效」（见 [`friendly_message`] 里
/// `AccountUnusable` 分支的实测记录）。本函数是导出的纯函数，可拿到任意 status，
/// 所以**在这里兜住**比依赖调用方自觉可靠。
pub fn server_status_label(status: u16) -> String {
    if (500..=599).contains(&status) {
        status.to_string()
    } else {
        "5xx".to_string()
    }
}

/// 把上游统一信封里的 `msg` 与多语言 `displayMsg` 提炼成一条**保留原文**
/// 的客户端消息（`<msg>（<displayMsg>）`，两者皆缺时回退原始 body）。
///
/// 11115 / 11140 / 11155 等请求侧拒绝共用同一个信封形状，因此共用本函数。
fn envelope_display_message(body: &str) -> String {
    let v = serde_json::from_str::<serde_json::Value>(body).unwrap_or(serde_json::Value::Null);
    let primary = v
        .get("msg")
        .and_then(|m| m.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            v.get("extError")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("");
    let hint = v
        .get("displayMsg")
        .and_then(|d| d.get("zh"))
        .and_then(|m| m.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            v.get("displayMsg")
                .and_then(|d| d.get("en"))
                .and_then(|m| m.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("");

    match (primary.is_empty(), hint.is_empty()) {
        (true, true) => truncate(body.trim(), 400),
        (true, false) => hint.to_string(),
        (false, true) => primary.to_string(),
        (false, false) if primary.contains(hint) => primary.to_string(),
        (false, false) => format!("{primary}（{hint}）"),
    }
}

/// 把上下文超长的上游响应体提炼成一条**保留原文**的客户端消息。
///
/// 为什么必须保留上游原文：下游客户端靠文案模式识别上下文溢出
///（`prompt is too long` / `context_length_exceeded` / `exceeds the model context limit`），
/// 据此触发自动压缩并重试。若只回我们自己的措辞，客户端就认不出这是溢出，
/// 只会把它当成普通失败。
pub fn context_too_long_message(body: &str) -> String {
    envelope_display_message(body)
}

/// 把「请求被上游拒绝」（11155）的响应体提炼成保留原文的消息。
///
/// 与上下文超长同理：原文里的业务语义（reasoning content）对客户端与用户的
/// 排查都有用，网关只做信封解包，不改写成「账号不可用」。
pub fn request_rejected_message(body: &str) -> String {
    envelope_display_message(body)
}

/// 从响应体里取一句可读的上游说明（取不到返回空）。
fn upstream_msg_of(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return String::new();
    };
    let pick = |s: Option<&str>| s.map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
    let candidates = [
        pick(v.get("msg").and_then(|m| m.as_str())),
        pick(v.get("message").and_then(|m| m.as_str())),
        pick(v.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str())),
        pick(v.get("error").and_then(|e| e.get("msg")).and_then(|m| m.as_str())),
    ];
    for t in candidates.into_iter().flatten() {
        return truncate(&t, 200);
    }
    String::new()
}

/// 把 11102（模型不在这条通道上）渲染成**可操作**的中文提示。
///
/// # 为什么必须重写而不是透传原文
///
/// 上游原文 `model [Qwen3.8-Flash] service info not found` 有两个问题：
/// - 英文，且 `service info not found` 让人以为是服务故障（要等）；
/// - 完全没说**该怎么办** —— 用户只会反复重试同一个模型。
///
/// 真实成因是**区域不匹配**：同名模型可能只在某一侧上游存在。故提示里给出两条
/// 出路：换一个模型，或用 `平台:区域:模型名` 的写法明确指定。
///
/// 保留了原始模型名与上游文案 —— 排查时仍需要它们。
///
/// ⚠ 措辞上必须与「排队」（10605）区分清楚：那个等一会儿就好，这个**等多久都没用**。
pub fn model_not_in_region_message(model: &str, body: &str) -> String {
    let mut b = String::from("这个模型在当前通道上不存在（不是排队，等多久都不会出现）");
    let m = model.trim();
    if !m.is_empty() {
        b.push_str(&format!("：模型 {m}"));
    }
    b.push_str("。请换一个模型，或用「平台:区域:模型名」的写法明确指定区域");
    b.push_str("（例如 qoder:国服:模型名 / workbuddy:国际版:模型名）。");
    let detail = upstream_msg_of(body);
    if !detail.is_empty() {
        b.push_str(&format!(" 上游原文：{detail}"));
    }
    b
}

/// 生成 11133（请求参数被模型提供商拒）的可读文案。
///
/// 尽量保留 `extError.message`（如 `the request parameters were rejected by
/// the model provider`），让用户知道是**参数**被拒而不是账号故障。
///
/// ★ 上游没给 message 时不能 panic、不能空（Go 测试
/// `TestModelParamInvalidMessageNoUpstreamText` 用 `{"code":11133}` / `not json` /
/// `""` 三种输入钉死）。
pub fn model_param_invalid_message(body: &str) -> String {
    let mut head = String::from("上游拒绝本次请求的参数（code=11133）");
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(ext) = v.get("extError") {
            if let Some(msg) = ext
                .get("message")
                .and_then(|m| m.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                head.push_str(&format!("：{msg}"));
            }
            // `param` 上游给的是**空串**，拿不到具体是哪个参数 —— 有才附上，不猜。
            if let Some(p) = ext
                .get("param")
                .and_then(|p| p.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                head.push_str(&format!("（参数：{p}）"));
            }
        }
    }
    head.push_str("。这不是账号问题，换账号无效；请检查请求参数（如模型名、思考档位、工具定义）后重试。");
    head
}

/// 从上游响应体里取出可读的原始说明（思考档位被拒时用）。
///
/// 保留原文很重要：用户要据此判断该换哪个档位，而我们的猜测未必对
///（实测不同模型接受的范围并不相同：`off` 在 14/16 个模型被接受，
/// 却被两个 deepseek 模型拒绝 —— 没有任何一张通用表能替他判断）。
///
/// 取不到 `msg` 字段时回退到原始 body（截断），而不是编一句像是我们判断的话。
pub fn effort_rejected_detail(body: &str) -> String {    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        let candidates = [
            trim_nonempty(v.get("msg").and_then(|m| m.as_str())),
            trim_nonempty(v.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str())),
            trim_nonempty(v.get("error").and_then(|e| e.get("msg")).and_then(|m| m.as_str())),
        ];
        for t in candidates.into_iter().flatten() {
            return t.to_string();
        }
    }
    let t = truncate(body.trim(), 300);
    if t.is_empty() {
        return "（上游未给出说明）".to_string();
    }
    t
}

/// 按字符边界截断（Go 侧按字节，这里按字符以免切碎 UTF-8）。
pub(crate) fn truncate(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() > n {
        s.chars().take(n).collect()
    } else {
        s.to_string()
    }
}

/// 取字符串的首尾空白后仍非空的值（用于「候选字段取第一个非空」的判定）。
fn trim_nonempty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn status_based_classification() {
        assert_eq!(classify(402, ""), ErrKind::HardCredit);
        assert_eq!(classify(429, ""), ErrKind::SoftRate);
        assert_eq!(classify(404, ""), ErrKind::NotFound);
        assert_eq!(classify(500, ""), ErrKind::Server);
        assert_eq!(classify(503, ""), ErrKind::Server);
        assert_eq!(classify(400, ""), ErrKind::Client);
        assert_eq!(classify(200, ""), ErrKind::None);
    }

    #[test]
    fn hard_markers_override_status() {
        assert_eq!(classify(200, "insufficient credit"), ErrKind::HardCredit);
        assert_eq!(classify(200, "Insufficient Credit"), ErrKind::HardCredit);
        assert_eq!(classify(500, "quota exceeded"), ErrKind::HardCredit);
        assert_eq!(classify(200, "余额不足"), ErrKind::HardCredit);
        assert_eq!(classify(200, "您的积分不足"), ErrKind::HardCredit);
        assert_eq!(classify(200, "额度用尽"), ErrKind::HardCredit);
    }

    /// 国际版实测响应：复数 Credits 必须命中，否则会退化成软冷却 → 无限重试。
    #[test]
    fn plural_credits_exhausted_is_hard() {
        let body = "Credits exhausted. Please visit the link below to purchase add-on packs and get more credits: https://x";
        assert_eq!(classify(429, body), ErrKind::HardCredit);
        assert_eq!(classify(200, body), ErrKind::HardCredit);
    }

    /// 额度耗尽的业务码藏在 error.data.code，且 HTTP 状态是 429 —— 必须先于 429 分支判定。
    #[test]
    fn nested_credit_exhausted_code_beats_429() {
        let body = r#"{"error":{"data":{"code":14018,"msg":"Credits exhausted. ..."}}}"#;
        assert_eq!(classify(429, body), ErrKind::HardCredit);
        assert!(friendly_message(ErrKind::HardCredit, 429, body).contains("14018"));
    }

    #[test]
    fn session_dead_markers() {
        assert_eq!(
            classify(401, r#"{"code":12153,"msg":"Offline user session not found"}"#),
            ErrKind::SessionDead
        );
        assert_eq!(classify(200, "12153"), ErrKind::SessionDead);
    }

    #[test]
    fn hard_markers_take_priority_over_session_dead() {
        assert_eq!(classify(401, "12153 余额不足"), ErrKind::HardCredit);
    }

    /// 模型级限流：429 + 6004 → ModelRate，而不是 SoftRate。
    #[test]
    fn model_rate_limited_by_code_and_text() {
        let by_code = r#"{"code":6004,"msg":"模型限流"}"#;
        assert_eq!(classify(429, by_code), ErrKind::ModelRate);

        let by_text = "您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，您也可以切换其他模型继续使用。";
        assert_eq!(classify(429, by_text), ErrKind::ModelRate);

        // 非 429 时不得判成模型限流（文案里有「频率限制」也不行）
        assert_ne!(classify(200, by_text), ErrKind::ModelRate);
    }

    /// 上下文超长是请求侧错误，必须独立成 kind 且早于通用 4xx。
    #[test]
    fn context_too_long_detection() {
        let body = r#"{"code":11115,"msg":"prompt is too long: 1119655 tokens > 1048576 maximum","extError":{"code":"context_length_exceeded","type":"invalid_request_error"},"displayMsg":{"zh":"对话内容超出模型长度上限，请精简对话或减少附件后重试。"}}"#;
        assert_eq!(classify(400, body), ErrKind::ContextTooLong);

        // 国服 glm-5.3 的另一种 msg 写法
        assert_eq!(
            classify(400, r#"{"code":11115,"msg":"input length too long"}"#),
            ErrKind::ContextTooLong
        );
        // 文案兜底（上游改码不改文案）
        assert_eq!(
            classify(400, "prompt is too long"),
            ErrKind::ContextTooLong
        );
    }

    /// 上下文超长的消息必须**保留上游原文**，否则客户端认不出溢出、无法自动压缩。
    #[test]
    fn context_too_long_message_keeps_upstream_text() {
        let body = r#"{"code":11115,"msg":"prompt is too long: 1119655 tokens > 1048576 maximum","displayMsg":{"zh":"对话内容超出模型长度上限，请精简对话或减少附件后重试。"}}"#;
        let msg = context_too_long_message(body);
        // 英文特征串来自上游 msg（客户端靠它识别溢出并触发自动压缩）
        assert!(msg.contains("prompt is too long"), "{msg}");
        // 中文提示来自 displayMsg.zh，拼在括号里给人类看
        assert!(msg.contains("对话内容超出模型长度上限"), "{msg}");
    }

    /// 11155 思维链缺失：请求侧错误，独立成 kind，
    /// 绝不能落进通用 4xx 去轮转全部账号。
    #[test]
    fn request_rejected_detection() {
        // 11155 实测完整信封（业务码在顶层 code，语义码在 extError.code）
        let body11155 = r#"{"code":11155,"msg":"the reasoning content from the previous turn must be passed back in thinking mode","extError":{"code":"reasoning_content_missing","type":"invalid_request_error","StatusCode":400}}"#;
        assert_eq!(classify(400, body11155), ErrKind::RequestRejected);
        assert!(is_request_rejected(body11155));

        // 只有 extError 语义码（上游改码不改文案时仍能识别）
        assert!(is_request_rejected(
            r#"{"extError":{"code":"reasoning_content_missing"}}"#
        ));

        // 消息保留上游原文
        let msg = request_rejected_message(body11155);
        assert!(msg.contains("the reasoning content from the previous turn"), "{msg}");
    }

    /// ★★ 11140 是**账号侧**错误，不是请求侧。
    ///
    /// 判据（Go `client.go:760-762` 逐字）：**同一请求在不同账号上结果不同**。
    /// 实测（2026-09-28，同请求体同 UA 同端点直连上游）：uid `a1b2c3d4`
    /// refresh 200 ✅ chat 200 ✅，其余 5 个账号 refresh 200 ✅ 但 chat 403 ❌
    /// 11140 —— 若是请求侧错误，它们应当**全部**失败。
    ///
    /// 这条测试钉死的是 Go 侧修掉的生产缺陷：11140 落进通用 `ErrClient` ⇒
    /// 「只换号不罚」⇒ 坏号永不冷却 ⇒ 每轮换号都被重新选中 ⇒ 请求在坏号集合里
    /// 空转。实测 8 个国际版账号裸名 `deepseek-v4.1-flash` 修复前 **0/6 成功（全 502）**。
    #[test]
    fn account_unusable_is_account_side_not_request_side() {
        // Go 测试用的最小样本（`account_unusable_rotation_test.go` 的
        // `accountUnusableBody`），HTTP 状态是 **403**。
        const BODY: &str = r#"{"code":11140,"msg":"request illegal"}"#;
        assert_eq!(classify(403, BODY), ErrKind::AccountUnusable);
        // ★ 绝不能是 RequestRejected：那会让坏号被反复重试（原地重试预算）
        // 而不是换号，用户看到的是「同一个打不通的号被连打三次」。
        assert_ne!(classify(403, BODY), ErrKind::RequestRejected);
        // ★ 绝不能落进通用 4xx：那正是「坏号永不冷却」的根因。
        assert_ne!(classify(403, BODY), ErrKind::Client);
        assert!(is_account_unusable(BODY));

        // 旧文案（带安全审核 displayMsg）也必须归账号侧 —— 业务码是主判据。
        let with_display = r#"{"code":11140,"msg":"request illegal","displayMsg":{"en":"The content did not pass the safety review. Please adjust and retry.","zh":"内容未通过安全审核，请调整后重试。"}}"#;
        assert_eq!(classify(403, with_display), ErrKind::AccountUnusable);
    }

    /// ★★ 文案里**绝对不能出现 "403"**，也**不透出上游原文**。
    ///
    /// 背景（Go `client.go:1050-1090` 逐字，2026-09-29 实测）：DSH 客户端判
    /// 「密钥无效」靠的不是 HTTP 状态码而是**文案正则**
    ///（`if (/\b(?:401|403)\b/.test(message)) return "AUTH";`），而
    /// `formatProviderError` 会把 `(status): body` 原样拼进 message
    /// ⇒ 只要文案里出现裸 "401"/"403"，不管 HTTP 状态码是什么，都被判 AUTH。
    /// 上一版只把 403 改写成 502 是**无效**的，因为文案里还留着「上游 403 业务码 11140」。
    ///
    /// 归因改用**业务码 11140** —— 对排查同样唯一，且不含任何会被误判的数字。
    #[test]
    fn account_unusable_message_never_leaks_auth_codes() {
        let body = r#"{"code":11140,"msg":"request illegal"}"#;
        let msg = friendly_message(ErrKind::AccountUnusable, 403, body);
        assert!(!msg.contains("403"), "文案不得含裸 403：{msg}");
        assert!(!msg.contains("401"), "文案不得含裸 401：{msg}");
        // 也不得透出上游原文 —— "request illegal" 会让人以为是自己请求写错了，
        // 而实际请求完全合法。
        assert!(!msg.contains("request illegal"), "不得透出上游原文：{msg}");
        // 但必须带上业务码 11140（唯一且不会误判的归因）。
        assert!(msg.contains("11140"), "{msg}");
    }

    /// `server_status_label` 只放行 5xx 原样输出，其余收敛成 "5xx"。
    ///
    /// 为什么不赌调用方永远传 5xx：一旦写出裸 401/403，客户端会把
    /// 「上游服务异常」读成「API 密钥无效」。
    #[test]
    fn server_status_label_never_leaks_auth_codes() {
        assert_eq!(server_status_label(502), "502");
        assert_eq!(server_status_label(500), "500");
        assert_eq!(server_status_label(599), "599");
        // 非 5xx 一律收敛，绝不输出裸 401/403
        assert_eq!(server_status_label(403), "5xx");
        assert_eq!(server_status_label(401), "5xx");
        assert_eq!(server_status_label(429), "5xx");
        assert_eq!(server_status_label(0), "5xx");
    }

    /// 11102：模型在这条通道上不存在（请求侧）。所有者 2026-09-20 现场。
    #[test]
    fn model_not_in_region_detection() {
        let body = r#"{"code":11102,"msg":"model [Qwen3.8-Flash] service info not found"}"#;
        assert_eq!(classify(400, body), ErrKind::ModelNotInRegion);
        // 必须早于通用 4xx：落进去会被逐号轮转，把「区域没指定」伪装成「账号全挂了」
        assert_ne!(classify(400, body), ErrKind::Client);
        assert!(is_model_not_in_region(body));

        // 文案兜底（上游把这句放在 error.message 里而不回 code）
        assert!(is_model_not_in_region(
            r#"{"error":{"message":"model service not found"}}"#
        ));
        // ⚠ 通用词不得误伤：正常的 400 不能被判成 11102
        assert!(!is_model_not_in_region(
            r#"{"code":11101,"msg":"Unmarshal chat params failed with error: unexpected EOF"}"#
        ));
    }

    /// 11102 的文案必须**可操作**：说清「等也没用」并给出出路。
    #[test]
    fn model_not_in_region_message_is_actionable() {
        let body = r#"{"code":11102,"msg":"model [Qwen3.8-Flash] service info not found"}"#;
        let msg = model_not_in_region_message("Qwen3.8-Flash", body);
        // 与「排队」区分清楚
        assert!(msg.contains("不是排队"), "{msg}");
        assert!(msg.contains("Qwen3.8-Flash"), "{msg}");
        // 给出两条出路
        assert!(msg.contains("换一个模型"), "{msg}");
        assert!(msg.contains("平台:区域:模型名"), "{msg}");
        // 保留上游原文供排查
        assert!(msg.contains("service info not found"), "{msg}");
    }

    /// 11133：请求参数被模型提供商拒（请求侧）。所有者 2026-09-28 现场。
    #[test]
    fn model_param_invalid_detection() {
        let body = r#"{"code":11133,"msg":"Invalid request parameters","extError":{"code":"model_param_invalid","message":"the request parameters were rejected by the model provider"}}"#;
        assert_eq!(classify(400, body), ErrKind::ModelParamInvalid);
        // ★ 不能是 ErrClient —— 那正是本次缺陷的根因（被当成账号故障逐号轮转）
        assert_ne!(classify(400, body), ErrKind::Client);
        // ★ 不能是 ErrContextTooLong —— 那会让客户端误触发压缩，
        // 而压缩并不会解决参数问题。
        assert_ne!(classify(400, body), ErrKind::ContextTooLong);
        assert!(is_model_param_invalid(body));
        // 反向：11102 不该被识别为 11133
        assert!(!is_model_param_invalid(
            r#"{"code":11102,"msg":"model [X] service info not found"}"#
        ));
    }

    /// 11133 文案必须保留 `extError.message`，且上游没给 message 时
    /// 不 panic、不为空。
    #[test]
    fn model_param_invalid_message_keeps_upstream_text() {
        let body = r#"{"code":11133,"msg":"Invalid request parameters","extError":{"code":"model_param_invalid","message":"the request parameters were rejected by the model provider"}}"#;
        let msg = model_param_invalid_message(body);
        assert!(msg.contains("11133"), "{msg}");
        assert!(
            msg.contains("the request parameters were rejected by the model provider"),
            "{msg}"
        );

        // 上游没给 message 时：不能 panic、不能空（Go 测试用这三种输入钉死）
        for input in [r#"{"code":11133}"#, "not json", ""] {
            let got = model_param_invalid_message(input);
            assert!(got.contains("11133"), "输入 {input:?} → {got}");
            assert!(got.contains("这不是账号问题"), "输入 {input:?} → {got}");
        }
    }

    /// 思考档位被拒（请求侧）：必须早于通用 4xx —— 落进去会被当成账号故障
    /// 逐号轮转，而所有者恰恰要靠这条文案判断「我用的档位到底行不行」。
    #[test]
    fn effort_rejected_detection() {
        let body = r#"{"code":400,"msg":"the reasoning effort value is not supported by the current model"}"#;
        assert_eq!(classify(400, body), ErrKind::EffortRejected);
        assert_ne!(classify(400, body), ErrKind::Client);
        assert!(is_effort_rejected(body));
        assert!(is_effort_rejected("unsupported reasoning effort"));
        assert!(!is_effort_rejected(r#"{"code":11133,"msg":"Invalid request parameters"}"#));
    }

    /// 档位被拒的文案必须**保留上游原文** —— 用户要据此判断该换哪个档位，
    /// 而我们的猜测未必对（实测 `off` 在 14/16 个模型被接受，
    /// 却被两个 deepseek 模型拒绝）。
    #[test]
    fn effort_rejected_detail_keeps_upstream_text() {
        let body = r#"{"msg":"the reasoning effort value is not supported by the current model"}"#;
        assert_eq!(
            effort_rejected_detail(body),
            "the reasoning effort value is not supported by the current model"
        );
        // error.message / error.msg 也能取到
        assert_eq!(
            effort_rejected_detail(r#"{"error":{"message":"nope"}}"#),
            "nope"
        );
        assert_eq!(effort_rejected_detail(r#"{"error":{"msg":"nope2"}}"#), "nope2");
        // 取不到 msg 字段时回退原始 body（截断），不编一句像是我们判断的话
        assert_eq!(effort_rejected_detail("plain text failure"), "plain text failure");
        // 全空 → 明确的占位，不能是空串
        assert_eq!(effort_rejected_detail("   "), "（上游未给出说明）");
    }

    #[test]
    fn reset_time_parsing_utc8() {
        let now = Utc.with_ymd_and_hms(2026, 9, 15, 5, 0, 0).unwrap();
        let body = "您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，您也可以切换其他模型继续使用。";
        let got = parse_reset_time(body, now).expect("应解析出重置时刻");
        // 13:25:47 UTC+8 == 05:25:47 UTC
        assert_eq!(got, Utc.with_ymd_and_hms(2026, 9, 15, 5, 25, 47).unwrap());
    }

    /// 只接受未来时刻：重放旧日志时不得写入一个立即失效的冷却。
    #[test]
    fn reset_time_rejects_past() {
        let now = Utc.with_ymd_and_hms(2026, 9, 16, 0, 0, 0).unwrap();
        let body = "将在 2026-09-15 13:25:47 UTC+8 重置";
        assert!(parse_reset_time(body, now).is_none());
    }

    #[test]
    fn reset_time_without_timezone_uses_cst() {
        // 无后缀 → 按 CST(+8) 解释，而非本机时区
        let now = Utc.with_ymd_and_hms(2026, 9, 15, 0, 0, 0).unwrap();
        let got = parse_reset_time("重置时间 2026-09-15 13:25:47", now).unwrap();
        assert_eq!(got, Utc.with_ymd_and_hms(2026, 9, 15, 5, 25, 47).unwrap());
    }

    #[test]
    fn reset_time_variants() {
        let now = Utc.with_ymd_and_hms(2026, 9, 15, 0, 0, 0).unwrap();
        // Z
        assert_eq!(
            parse_reset_time("2026-09-15T13:25:47Z", now).unwrap(),
            Utc.with_ymd_and_hms(2026, 9, 15, 13, 25, 47).unwrap()
        );
        // UTC-5
        assert_eq!(
            parse_reset_time("2026-09-15 13:25:47 UTC-5", now).unwrap(),
            Utc.with_ymd_and_hms(2026, 9, 15, 18, 25, 47).unwrap()
        );
        // 无时间字面量
        assert!(parse_reset_time("没有时间", now).is_none());
    }

    /// 逐行对照 Go 侧 `TestClassify` 的用例矩阵（`internal/upstream/client_test.go`）。
    ///
    /// 这是本模块最重要的回归防线：分类顺序一旦改错，表现是「冷却时长不对」——
    /// 不报错、不崩溃，只是静默地反复重试已耗尽的账号。因此矩阵必须与 Go 同步。
    #[test]
    fn matches_go_testclassify_matrix() {
        let cases: &[(u16, &str, ErrKind)] = &[
            (402, "", ErrKind::HardCredit),
            (400, r#"{"code":1,"msg":"余额不足"}"#, ErrKind::HardCredit),
            (403, "insufficient credits", ErrKind::HardCredit),
            (200, r#"{"code":10001,"msg":"积分不足，请充值"}"#, ErrKind::HardCredit),
            (400, r#"{"code":1,"msg":"额度用尽"}"#, ErrKind::HardCredit),
            (429, "", ErrKind::SoftRate),
            // ★ ZCode 1113：HTTP 429 + 「余额不足或无可用资源包」——语义是
            // 「这条通道没有该账号的资源包」而**不是**账号没额度 ⇒ 必须先于
            // 429 分支命中，且**绝不能**判成 HardCredit（否则整号硬冷却到
            // 次日 04:00，而该账号额度一分没花）。反向护栏见上面 400 那条：
            // WorkBuddy 的「余额不足」必须仍然是 HardCredit。
            (
                429,
                r#"{"error":{"code":"1113","message":"余额不足或无可用资源包,请充值。"}}"#,
                ErrKind::NoResourcePack,
            ),
            // 只有业务码、没有中文文案时的兜底（实测 ZCode 偶发只回码）。
            (429, r#"{"code":1113}"#, ErrKind::NoResourcePack),
            (401, "Offline user session not found", ErrKind::SessionDead),
            (
                401,
                r#"{"code":12153,"msg":"Offline user session not found"}"#,
                ErrKind::SessionDead,
            ),
            (401, r#"{"code":9999,"msg":"bad token"}"#, ErrKind::Client),
            (500, "boom", ErrKind::Server),
            (503, "unavailable", ErrKind::Server),
            (200, "", ErrKind::None),
        ];
        for (status, body, want) in cases {
            let got = classify(*status, body);
            assert_eq!(
                got, *want,
                "Classify({status}, {body:?}) = {got}, Go 侧为 {want}"
            );
        }
    }

    #[test]
    fn err_kind_display_matches_go() {
        assert_eq!(ErrKind::HardCredit.as_str(), "hard_credit");
        assert_eq!(ErrKind::SoftRate.as_str(), "soft_rate");
        assert_eq!(ErrKind::SessionDead.as_str(), "session_dead");
        assert_eq!(ErrKind::NotFound.as_str(), "not_found");
        assert_eq!(ErrKind::Server.as_str(), "server");
        assert_eq!(ErrKind::Client.as_str(), "client");
        assert_eq!(ErrKind::None.as_str(), "none");
        assert_eq!(ErrKind::ModelRate.as_str(), "model_rate");
        assert_eq!(ErrKind::ContextTooLong.as_str(), "context_too_long");
        assert_eq!(ErrKind::RequestRejected.as_str(), "request_rejected");
        assert_eq!(ErrKind::NoResourcePack.as_str(), "no_resource_pack");
    }

    #[test]
    fn upstream_error_display_format() {
        let e = UpstreamError {
            kind: ErrKind::SoftRate,
            status: 429,
            msg: "rate limited".into(),
        };
        assert_eq!(e.to_string(), "upstream soft_rate (http 429): rate limited");
    }

    #[test]
    fn friendly_message_fallbacks() {
        assert_eq!(friendly_message(ErrKind::None, 200, ""), "");
        assert!(friendly_message(ErrKind::SoftRate, 429, "").contains("429"));
        assert!(friendly_message(ErrKind::Server, 502, "").contains("502"));
    }
}
