//! 成长任务执行体（对应 Go `internal/growtask/*.go`）。
//!
//! # 职责边界
//!
//! - `upstream` —— 只负责「一次 HTTP 调用长什么样」（端点、头、信封、事件形状）；
//! - `growtask` —— 只负责「按什么顺序、做几次、何时算成功、什么时候领奖」；
//! - `scheduler` —— 只负责「什么时候触发」。
//!
//! 三者分开后，「一键完成」既能被用户手动触发（宿主调 [`runner::Runner::run_one`] /
//! [`runner::Runner::run_all`]），也能被排程复用（scheduler 定时跑），而不必把动作
//! 实现复制两份。
//!
//! # 核心不变量：上报 200 ≠ 计分
//!
//! 服务端的行为计分是**异步**的，且用错客户端指纹时会被**静默丢弃**（HTTP 200 但
//! 进度不动）。因此每个动作完成后都必须回读任务进度，达标才领奖 —— 绝不把「上报
//! 成功」当成「任务完成」。这是本模块存在的根本理由。
//!
//! # 子模块对应关系
//!
//! | 本模块    | Go 源文件      | 职责                                   |
//! |-----------|----------------|----------------------------------------|
//! | `runner`  | `runner.go`    | 任务循环：列出 → 接取 → 执行 → 领取     |
//! | `tasks`   | `tasks.go`     | 任务清单（`TaskView` 视图与文案）       |
//! | `actions` | `actions.go`   | 单个任务的执行动作（会话/事件序列）     |

pub mod actions;
pub mod runner;
pub mod tasks;

use std::time::Duration;

/// 单次执行的预算（对应 Go `cmd/server/growth_tasks.go` 的 `growTaskTimeout`）。
///
/// ★ 到点是**优雅收尾**而不是失败：取消的是「还没开始的动作」，已完成的项保留在
/// 结果里。用 `tokio::time::timeout` 直接报错会把「跑了一半但确实拿到部分奖励」
/// 变成一条失败记录，用户看到的是「白跑了」。
///
/// ★ 宿主侧调 `/tasks/growth` 给的也是 600 秒（`ai-gateway-core` 的
/// `growth_task`），两处一致。
pub const RUN_TIMEOUT: Duration = Duration::from_secs(600);

pub use runner::{
    summarize, AccountGuard, AccountResult, FleetOptions, FleetSummary, ItemResult, Options,
    Runner, ERR_ACCOUNT_BUSY, MAX_FLEET_CONCURRENCY, STATUS_DONE, STATUS_ERROR, STATUS_SKIPPED,
    STATUS_UNSUPPORTED,
};
pub use tasks::TaskView;
