//! 网关内核共享错误类型。
//!
//! 与 Go 侧口径对齐：Go 侧大量使用 `fmt.Errorf` + `errors.As` 做分类判定，
//! Rust 侧统一收敛为一个 thiserror 枚举，避免调用点丢失分类信息。

use thiserror::Error;

/// 网关内核错误。
#[derive(Debug, Error)]
pub enum GatewayError {
    /// 配置文件读取/解析失败。
    #[error("read config: {0}")]
    ConfigRead(String),

    /// 配置解析失败（JSON 语法错误）。
    #[error("parse config: {0}")]
    ConfigParse(String),

    /// 配置项取值非法（例如 duration 字符串不可解析、小时越界）。
    #[error("invalid config: {0}")]
    ConfigInvalid(String),

    /// 凭证文件解析失败。
    #[error("auth parse {path}: {msg}")]
    AuthParse { path: String, msg: String },

    /// 凭证文件写回（原子写）失败。
    #[error("auth save {path}: {msg}")]
    AuthSave { path: String, msg: String },

    /// 上游 HTTP 传输层错误（连接失败、超时、TLS 等）。
    ///
    /// 对应 Go 侧的 transport error：只换号不喂熔断。
    #[error("upstream transport: {0}")]
    Transport(String),

    /// 上游返回了无法解析的响应体。
    #[error("upstream parse: {0}")]
    UpstreamParse(String),

    /// 上游按业务语义返回的失败（带分类），驱动账号池冷却策略。
    #[error("{0}")]
    UpstreamError(crate::upstream::classify::UpstreamError),

    /// 无可用账号（全冷却/禁用/占满）。
    #[error("all accounts unavailable (cooling/disabled)")]
    NoHealthyAccount,

    /// 账号级额度耗尽（ZCode 套餐过期 / 无可用资源包）。
    ///
    /// 对应 Go 侧那个小接口 `interface{ QuotaExhausted() bool }`：
    /// 转发层用 `errors.As` 认出它，走 `ErrHardCredit` + **402**，
    /// 而不是等到「上游返回 200 却一直是空流」再超时。
    ///
    /// 单独成型而不是复用 [`GatewayError::UpstreamError`]：判定它的
    /// **不是**状态码分类（ZCode 的额度失败可能伴随 200），而是产品上游
    /// 在请求前/首帧时的明确结论。
    #[error("{0}")]
    QuotaExhausted(String),

    /// 上游清单里根本没有这个模型（Qoder `ModelNotFoundError`）。
    ///
    /// ★ 与 [`crate::forward::FailureKind::ModelNotInProduct`] 是两回事：
    /// 那个是**用户白名单**没放行（模型可能在，是你没放行，去配置页加上即可）；
    /// 这个是**上游根本没有**（谁都没法放行，加白名单也没用）。
    ///
    /// 处置上二者一致：**不轮转、不罚账号、立即 400**。理由是同一个请求体
    /// 发给任何账号，上游清单都一样没有它 —— 轮转只是把同一请求重传 N 遍；
    /// 而账号是无辜的，罚号会让用户改好模型名后以为「账号又挂了」。
    #[error("model not in upstream: {0}")]
    ModelNotFound(Box<ModelNotFoundError>),

    /// 上游要求人机验证（ZCode 3007）。
    ///
    /// ★ 与账号信誉无关：它是上游对**这次请求/这个出口**提出的要求。
    /// 故同样**不轮转、不罚账号**，并给出「等一会儿」的明确文案。
    #[error("{0}")]
    CaptchaRequired(String),

    /// 状态持久化失败。
    #[error("state persist: {0}")]
    StatePersist(String),

    /// Redis / 远端快照读写失败。v0 阶段 Redis 尚未接入，先预留。
    #[error("redis: {0}")]
    Redis(String),

    /// 其他未分类的内部错误。
    #[error("{0}")]
    Other(String),
}

/// `Result` 别名，内核统一使用。
pub type Result<T> = std::result::Result<T, GatewayError>;

/// 「上游清单里没有这个模型」的载荷。
///
/// 对应 Go `qoder.ModelNotFoundError{Model, Region, Available}`。三样都要
/// 带出来，因为文案里**同时**要说清「哪个模型」「哪个区域」「上游有哪些」
/// —— 少任何一个，用户都只能去猜（见 Go `modelNotInUpstreamMessage`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelNotFoundError {
    /// 客户端请求的模型名（裸名，已剥掉产品/区域前缀）。
    pub model: String,
    /// 查的是哪个区域的清单（`"cn"` / `"intl"` / 空 = 不分区）。
    pub region: String,
    /// 该区域清单里的模型 key（供文案列出候选）。
    pub available: Vec<String>,
    /// 面向客户端的**替换**说明（可选）。
    ///
    /// 有它时 [message] 的第一句改用它 —— 给那些「上游清单里没有这个模型」不成立
    /// 的场合用：Trae 把 4001 / 4023 也映射到本类型，而那两个码是上游在**收到请求
    /// 之后**退回来的（请求发出去过），说「请求未发出」就是假话。
    pub note: Option<String>,
}

impl ModelNotFoundError {
    /// 构造。
    pub fn new(model: impl Into<String>, region: impl Into<String>, available: Vec<String>) -> Self {
        Self {
            model: model.into(),
            region: region.into(),
            available,
            note: None,
        }
    }

    /// 换掉第一句说明（见 [Self::note]）；其余文案照旧。
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// 面向客户端的说明：给出**原因**与**出路**。
    ///
    /// 与 Go `modelNotInUpstreamMessage`（`internal/server/forward.go:1436`）
    /// 同口径：用户第一反应会是「去白名单里加上它」，而这条路**走不通**
    /// —— 上游清单里没有，加白名单也放行不了。故必须显式说这一点。
    pub fn message(&self) -> String {
        let region = if self.region.is_empty() {
            "该产品".to_string()
        } else {
            format!("该产品「{}」区域", self.region)
        };
        // 有 note 时首句换成它，但「这不是白名单问题」这段劝告照旧保留 ——
        // 用户的第一反应永远是「去白名单里加上它」。
        let mut msg = match &self.note {
            Some(note) => format!("{note}"),
            None => format!("上游 {region} 的模型清单里没有「{}」，请求未发出。", self.model),
        };
        msg.push_str("这不是白名单问题 —— 把它加进白名单也不会生效，请改用上游真实存在的模型。");
        if !self.available.is_empty() {
            let mut list = self.available.clone();
            list.sort();
            let shown = list.iter().take(12).cloned().collect::<Vec<_>>().join("、");
            if list.len() > 12 {
                msg.push_str(&format!("\n{region}可用模型（{} 个，列前 12）：{shown}…", list.len()));
            } else {
                msg.push_str(&format!("\n{region}可用模型（{} 个）：{shown}", list.len()));
            }
        }
        msg
    }
}

/// 供 `thiserror` 的 `#[error("...{0}")]` 使用（该字段是 `Box<Self>`）。
impl std::fmt::Display for ModelNotFoundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl GatewayError {
    /// 把任意 `std::io::Error` 包装为 [`GatewayError::Other`]。
    pub fn io(e: std::io::Error) -> Self {
        GatewayError::Other(e.to_string())
    }

    /// 是不是「上游清单里没有这个模型」。
    pub fn model_not_found(&self) -> Option<&ModelNotFoundError> {
        match self {
            GatewayError::ModelNotFound(e) => Some(e),
            _ => None,
        }
    }

    /// 是不是账号级额度耗尽。
    ///
    /// 对应 Go 的 `errors.As(terr, &quotaErr) && quotaErr.QuotaExhausted()`。
    pub fn is_quota_exhausted(&self) -> bool {
        matches!(self, GatewayError::QuotaExhausted(_))
    }

    /// 是不是「上游要求人机验证」。
    pub fn is_captcha_required(&self) -> bool {
        matches!(self, GatewayError::CaptchaRequired(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_message_keeps_the_original_wording() {
        let e = ModelNotFoundError::new("glm-5.3", "", vec![]);
        let m = e.message();
        assert!(
            m.contains("上游 该产品 的模型清单里没有「glm-5.3」，请求未发出。"),
            "{m}"
        );
        assert!(m.contains("这不是白名单问题"), "{m}");
        assert!(e.note.is_none());
    }

    #[test]
    fn a_note_replaces_the_head_and_keeps_the_advice_and_the_candidates() {
        let e = ModelNotFoundError::new("glm-5.3", "cn", vec!["b".into(), "a".into()])
            .with_note("两条产品线的池都试过了");
        let m = e.message();
        assert!(m.starts_with("两条产品线的池都试过了"), "{m}");
        assert!(!m.contains("请求未发出"), "{m}");
        assert!(m.contains("这不是白名单问题 —— 把它加进白名单也不会生效"), "{m}");
        assert!(m.contains("该产品「cn」区域可用模型（2 个）：a、b"), "{m}");
        assert_eq!(e.to_string(), m, "Display 与 message() 同源");
    }
}
