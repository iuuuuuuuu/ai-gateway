//! 上游交互层：错误分类、请求体改写、指纹脱敏、SSE 解析、HTTP 客户端。
//!
//! 对应 Go 包 `internal/upstream/`，逐文件对照：
//!
//! | 本模块     | Go 源文件          | 职责                                 |
//! |------------|--------------------|--------------------------------------|
//! | `classify` | `client.go` 分类区 | 错误分类、重置时间解析、文案提炼     |
//! | `payload`  | `payload.go`       | 出站请求体改写（stream/role/effort） |
//! | `sanitize` | `sanitize.go`      | 出站请求体黑名单指纹脱敏             |
//! | `sse`      | `sse.go`           | SSE 聚合与逐帧规范化透传             |
//! | `headers`  | `headers.go`       | 四类上游请求头                       |
//! | `client`   | `client.go`        | HTTP 客户端、token 刷新、模型清单    |
//!
//! # 为什么分类单独成文件
//!
//! 分类优先级有**两处非显然的顺序**（额度耗尽的 429 必须先于软冷却、
//! 上下文超长必须先于通用 4xx），是最容易在重构中被改错的地方。集中在一处
//! 并用用例矩阵（`tests/ab_classify.rs`）逐行对照 Go，可降低漂移风险。

pub mod classify;
pub mod client;
pub mod headers;
pub mod measured;
pub mod payload;
pub mod sanitize;
pub mod sse;

pub use classify::{
    classify, context_too_long_message, friendly_message, is_context_too_long,
    is_model_rate_limited, parse_reset_time, ErrKind, UpstreamError,
};
pub use client::{ChatOutcome, Client, ClientConfig, CreditInfo, ModelInfo};
pub use measured::{measured_image_capability, measured_override, ImageCapability};
pub use payload::{prepare_body, prepare_body_for_region};
pub use sse::{aggregate, normalize_frame, normalize_payload, SseFrames, SseFramesIter};
