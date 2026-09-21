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
//! 7. 请求被拒（11155 思维链缺失 / 11140 安全审核）→ 请求侧错误，立即失败
//! 8. `404` → 短冷却
//! 9. `>=500` → 服务端错误（喂熔断）
//! 10. `>=400` → 客户端错误（只换号不罚）
//! 11. 其余 → 无错误
//!
//! 两处**非显然的顺序**，改前务必读完：
//!
//! - 第 2 步必须在 `429` 之前：额度耗尽的 HTTP 状态恰恰是 429，若不先拦，
//!   会落进软冷却分支只冷却 60 秒 → 60 秒后重试同一个已耗尽账号 → 无限重试。
//! - 第 6 步必须在通用 `4xx` 之前、但在 `429` 之后：上下文超长是请求侧错误，
//!   需要独立 kind 让调用方「立即失败」而不是把整个请求体对着每个账号重传。

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
    /// 请求内容被上游明确拒绝（400 code=11155 思维链缺失 /
    /// 403 code=11140 未通过安全审核）。
    ///
    /// 与 [`ErrKind::ContextTooLong`] 同属**请求侧**错误：同一请求体发给
    /// 任何账号结果都一样，轮转账号不仅无用，还会把「这条请求不合法」伪装成
    /// 「账号全部不可用」，并白白冷却整个账号池。
    RequestRejected,
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
/// - `11140`：内容未通过安全审核（request illegal）
const REQUEST_REJECTED_CODES: &[i64] = &[11155, 11140];

/// 「请求被拒绝」的文案兜底（中英双通道）。
///
/// 业务码是主信号；文案兜底用于上游改码不改文案 / 业务码放在 extError.code
/// 字符串里（11155 的实测形状）的场景。
const REQUEST_REJECTED_MARKERS: &[&str] = &[
    "reasoning_content_missing",
    "reasoning content from the previous turn",
    "did not pass the safety review",
    "内容未通过安全审核",
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
/// 三路判定，任一命中即成立：顶层业务码 11155/11140、
/// `extError.code` 为对应语义串（11155 实测放在这里）、或真实文案关键词。
/// 不按 status 门控：两者实测分别为 400 / 403，判定依据是业务语义。
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
    // 同为请求侧错误（11155 思维链缺失 / 11140 安全审核）：立即失败，
    // 不轮转、不罚账号。
    if is_request_rejected(body) {
        return ErrKind::RequestRejected;
    }
    if status == 404 {
        return ErrKind::NotFound;
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
        ErrKind::ModelRate => {
            "该账号在此模型上已达频率上限，已按上游给出的重置时间冷却；同一账号的其他模型仍可用"
                .to_string()
        }
        ErrKind::SoftRate => {
            "账号被上游限流（HTTP 429），已短暂冷却，稍后会自动重试".to_string()
        }
        ErrKind::SessionDead => "账号登录态已失效，需在「账号管理」页重新登录".to_string(),
        ErrKind::NotFound => {
            "上游返回 404（接口或模型不存在），已短暂冷却并切换账号".to_string()
        }
        ErrKind::Server if status > 0 => {
            format!("上游服务异常（HTTP {status}），已切换到其他账号")
        }
        _ => String::new(),
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

/// 把「请求被上游拒绝」（11140 / 11155）的响应体提炼成保留原文的消息。
///
/// 与上下文超长同理：原文里的业务语义（safety review / reasoning content）
/// 对客户端与用户的排查都有用，网关只做信封解包，不改写成「账号不可用」。
pub fn request_rejected_message(body: &str) -> String {
    envelope_display_message(body)
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

    /// 11155 思维链缺失 / 11140 安全审核：请求侧错误，独立成 kind，
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

        // 11140 安全审核（HTTP 403，带中英 displayMsg）
        let body11140 = r#"{"code":11140,"msg":"request illegal","displayMsg":{"en":"The content did not pass the safety review. Please adjust and retry.","zh":"内容未通过安全审核，请调整后重试。"}}"#;
        assert_eq!(classify(403, body11140), ErrKind::RequestRejected);

        // 文案兜底：缺业务码时英文文案也能命中
        assert_eq!(
            classify(
                403,
                "The content did not pass the safety review. Please adjust and retry."
            ),
            ErrKind::RequestRejected
        );

        // 消息保留上游原文 + 中文提示
        let msg = request_rejected_message(body11140);
        assert!(msg.contains("request illegal"), "{msg}");
        assert!(msg.contains("内容未通过安全审核"), "{msg}");
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
