//! ai-gateway-router —— workbuddy2api 网关内核（Rust 版）。
//!
//! 目标是替代原先由 Go 编译的 `gateway.exe`，通过 `build.rs` 内嵌进
//! ai-gateway 桌面端 / CLI 分发。
//!
//! # 模块与 Go 侧对应关系
//!
//! | 本模块      | Go 源文件                        |
//! |-------------|----------------------------------|
//! | `config`    | `cmd/server/config.go`           |
//! | `auth`      | `internal/auth/auth.go`          |
//! | `capability`| `internal/server/capability.go`  |
//! | `pool`      | `internal/pool/pool.go`          |
//! | `authwatch` | `internal/pool/watch.go`         |
//! | `cfgwatch`  | `cmd/server/main.go`（`startConfigWatch`）|
//! | `prompt`    | `internal/prompt/prompt.go`      |
//! | `upstream`  | `internal/upstream/*.go`         |
//! | `products`  | `internal/qoder/*` + `internal/zcode/*` |
//! | `session`   | `internal/session/session.go`    |
//! | `forward`   | `internal/server/forward.go`     |
//! | `model_allow`| `internal/server/model_allow.go` |
//! | `server`    | `internal/server/handler.go`     |
//! | `unusual`   | `internal/server/unusual.go`     |
//! | `wafip`     | `internal/server/wafip.go`       |
//! | `clienttext`| `internal/server/clienttext.go`  |
//! | `logging`   | `internal/server/logging.go`     |
//! | `records`   | `internal/records/records.go`    |
//! | `scheduler` | `internal/scheduler/*.go`        |
//! | `growtask`  | `internal/growtask/*.go`         |
//! | `cli`       | `cmd/server/main.go` 的子命令派发 |
//!
//! # 迁移策略
//!
//! 逐模块替换，每个模块都保留与 Go 版**完全一致**的对外行为（HTTP 响应体、
//! 状态机语义、日志口径），并用同一套用例做 A/B 对照。

pub mod auth;
pub mod authwatch;
pub mod capability;
pub mod cfgwatch;
pub mod cli;
pub mod clienttext;
pub mod config;
pub mod emptystream;
pub mod error;
pub mod forward;
pub mod growtask;
pub mod logging;
pub mod model_allow;
pub mod models_cache;
pub mod pool;
pub mod products;
pub mod prompt;
pub mod protocol;
pub mod records;
pub mod resolve_model;
pub mod scheduler;
pub mod server;
pub mod session;
// ★★ **测试专用资产**（可编排的 mock 上游，F-09）。
//
// 必须是 `#[cfg(test)]`：本模块只是测试夹具，若做成普通 `pub mod` 它会
// **进入发行产物**（网关 exe 里多一份用不到的 HTTP 假上游代码）。
// `src/testing/` 整棵子树只在 `cargo test` 时参与编译。
#[cfg(test)]
mod testing;
pub mod unusual;
pub mod upstream;
pub mod usage;
pub mod wafip;

pub use error::{GatewayError, Result};
