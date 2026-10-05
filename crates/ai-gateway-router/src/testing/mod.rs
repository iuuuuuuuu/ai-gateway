//! 测试专用资产（**绝不进发行产物**）。
//!
//! `lib.rs` 以 `#[cfg(test)] mod testing;` 引入本模块 —— 整个子树只在
//! `cargo test` 时参与编译，`cargo build` 的产物里一行都不会有。
//! ⚠ **不要**把它改成非 `cfg(test)` 的 `pub mod`：那是把测试夹具打进发行包。

pub(crate) mod mock_upstream;
