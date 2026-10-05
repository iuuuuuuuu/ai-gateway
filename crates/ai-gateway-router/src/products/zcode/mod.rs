//! ZCode 产品上游：Anthropic Messages 客户端 + OpenAI⇄Anthropic 双向翻译。
//!
//! # 移植来源（Go 侧，逐字对照）
//!
//! | 文件                            | 职责                                        |
//! |---------------------------------|---------------------------------------------|
//! | `internal/zcode/cred.go`        | 凭证解析/保存、`deviceMid` 铸造              |
//! | `internal/zcode/provider.go`    | 服务商（Z.AI / 智谱）与端点表                |
//! | `internal/zcode/identity.go`    | 身份与追踪头（确定性 UUID 派生）             |
//! | `internal/zcode/errors.go`      | 错误分类（**业务码先于状态码**）             |
//! | `internal/zcode/translate.go`   | OpenAI ⇄ Anthropic 双向翻译                  |
//! | `internal/zcode/client.go`      | Anthropic Messages 客户端 + 验证码控制流     |
//! | `internal/zcode/dispatch.go`    | 派发 + 模型名归一 + 额度预检                 |
//! | `internal/zcode/captcha.go`     | 阿里云无痕验证求解（进程级共享 + 池 + 冷却） |
//! | `internal/zcode/systemblock.go` | 官方 system 块（★ **默认开**，见下）         |
//! | `internal/zcode/signing.go`     | ⚠ 刻意不做：`SigningNotImplemented = true`  |
//!
//! ## ⚠ 关于官方 system 块（更正一处历史说法）
//!
//! 本文件早先的表格写着「systemblock.go：刻意不做，官方 system 块（默认关）」——
//! **那两句都不对**，已在移植时核实并更正：
//!
//! - **不是「刻意不做」**：`systemblock.rs` 已完整移植（`build_official_system` /
//!   `normalize_user_system` / `build_context_prefix_message`），并在
//!   `translate::build_anthropic_body_with_meta` 里**无条件接线**。
//! - **不是「默认关」**：只有显式设置 `AI_GATEWAY_ZCODE_SYSTEM_BLOCK=0|false`
//!   才会关闭（见 `systemblock::system_block_enabled`）。
//!
//! ## 两条通道**必须分清**（混用表现为「2.99 亿额度却报余额不足」）
//!
//! - **start-plan**（`https://zcode.z.ai/api/v1/zcode-plan/anthropic`）：凭证是 **JWT**
//! - **coding / paas**（`{provider}/api/anthropic`）：凭证是 `{apiKey}.{secret}`
//!
//! `Auth` 只承载选号字段，故 [`dispatch::cred_of`] 会**回读凭证文件**取
//! `jwt` / `device_mid` / `account_id` —— 少任何一个都会静默走错通道。
//!
//! # ⚠ 3007（人机验证）为何返回 `Err` 而不是 `Ok(Failed)`
//!
//! 详见 [`client::Client::stream_chat`] 的文档。一句话：`forward.rs` 的
//! `ChatOutcome::Failed` 分支没有 3007 特例，走那里会**轮转账号重试**，
//! 而人机验证是按**出口 IP** 触发的 —— 换号毫无意义，只会把整池拖进冷却。

pub mod captcha;
pub mod cli;
pub mod cred;
pub mod errors;
pub mod identity;
pub mod provider;
pub mod signing;
pub mod systemblock;
pub mod translate;

pub(crate) mod claim;
pub(crate) mod client;
pub(crate) mod dispatch;
pub(crate) mod login;
pub(crate) mod plan;
pub(crate) mod warm;
pub mod claim_scheduler;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::auth::Auth;
use crate::error::Result;
use crate::upstream::client::ChatOutcome;

use super::ProductUpstream;
use client::{ChatCall, Client};
use dispatch::Dispatch;

/// ZCode 上游。
///
/// ★ `_client` 的**下划线是刻意的**：`crate::upstream::client::Client` 的字段全是
/// 私有的、没有任何配置 getter，而它的端点常量都是 WorkBuddy 专用的
/// （`CHAT_BASE_CN = copilot.tencent.com` 等）—— ZCode 的端点、代理、超时全部由
/// 本包的 [`Client`] 独立管理。这里保留一个句柄只是为了将来接「共享连接池」之类的
/// 配置，现在**一个字段都不读**。不加下划线会得到一个 `field is never read` 警告，
/// 而那个警告会掩盖真正的死代码。
pub struct ZcodeUpstream {
    _client: crate::upstream::client::Client,
    dispatch: Dispatch,
}

impl ZcodeUpstream {
    /// 用网关的上游客户端配置构造。
    ///
    /// 构造时完成两件**进程级**接线（对应 Go `cmd/server/main.go` 的启动段）：
    ///
    /// 1. [`client::configure_captcha_from_host`]：按宿主写出的原生配置初始化验证码
    ///    求解器。ZCode 对话对任何不带验证参数的请求一律回 3007，所以这一步是
    ///    「对话能不能用」的前提，而不是可选优化。
    /// 2. [`Client::apply_host_proxy`]：按 `proxy_scope.intl` 决定 ZCode 是否走代理。
    ///    `zcode.z.ai` 以 `.ai` 结尾，属国际流量。设置失败只打日志、**不阻断启动**。
    pub fn new(client: crate::upstream::client::Client) -> Self {
        client::configure_captcha_from_host();
        let mut inner = Client::new();
        inner.apply_host_proxy();
        Self {
            _client: client,
            dispatch: Dispatch::new(inner),
        }
    }

    /// 包成 `Arc<dyn ProductUpstream>`。
    pub fn shared(self) -> Arc<dyn ProductUpstream> {
        Arc::new(self)
    }
}

#[async_trait]
impl ProductUpstream for ZcodeUpstream {
    async fn chat_stream(&self, a: &Auth, openai_body: &[u8]) -> Result<ChatOutcome> {
        match self.dispatch.chat_stream(a, openai_body).await? {
            // 上游响应**已经是标准 OpenAI**（Anthropic 形状在 client 内部翻译掉了），
            // 故直接交给上层，不需要任何包装。
            ChatCall::Stream(resp) => Ok(ChatOutcome::Stream(resp)),
            // 上游拒绝 ⇒ 走 `Failed` 让 `forward.rs` 按状态码分类（额度/风控/参数）。
            ChatCall::Rejected { status, body } => Ok(ChatOutcome::Failed { status, body }),
        }
    }

    async fn aggregate(
        &self,
        sse: &str,
        model: &str,
    ) -> std::result::Result<Map<String, Value>, String> {
        self.dispatch.aggregate(sse, model)
    }
}

/// 从凭证目录加载 ZCode 账号，转成网关账号池用的 [`Auth`]。
///
/// # 与 Qoder 同款的两个约定
///
/// 1. **`uid` 为空 = 无法进池**（池以 uid 为键），直接跳过。
/// 2. **失败必须逐条报出来**，不能静默跳过 —— ZCode 的凭证多半是用户手写的
///    （粘贴导入），格式错误要让用户看见。失败描述直接沿用 [`cred::load_dir`]
///    的文本（形如 `zcode-bad.json: 凭证不是合法 JSON: …`）。
///
/// # ⚠ 刻意**没有** `save_atomic` 循环（与 Qoder 的关键差异）
///
/// Qoder 在加载阶段补 COSY 机器指纹并立即写盘，否则「每个请求都现生成一对新指纹，
/// 同一账号在两次请求里表现为两台设备」。ZCode **没有**这类「加载时派生、必须
/// 持久化」的字段：
///
/// - `deviceMid` 的铸造与落盘已经在 [`cred::load_file`] 内部完成
///   （`device_mid_needs_persist` 标记，失败只 `eprintln!`）；
/// - `jwt` / `accountId` 都是从凭证内容直接解析出来的，没有派生状态。
///
/// ⇒ 这里**不该**为了「与 Qoder 形状一致」而多写一次盘：那会平白引入
/// 「网关启动即改写用户凭证文件」的风险，而没有任何收益。
pub fn load_credentials(dir: &str) -> (Vec<Auth>, Vec<String>) {
    let (creds, failed) = cred::load_dir(dir);
    let mut out = Vec::with_capacity(creds.len());
    for cr in creds {
        if cr.uid.is_empty() {
            continue;
        }
        out.push(cr.to_auth());
    }
    (out, failed)
}

/// 脱敏 uid：日志里不出现完整账号标识。
///
/// 本仓库是**公开仓库**，日志可能被用户贴到 issue 里 —— 只留前 8 位足够定位，
/// 又不足以反查账号。对应 Go `main.go` 的 `maskUID`。
///
/// 由 [`dispatch`] 的失败日志使用；测试里也直接钉住它的边界。
pub(crate) fn mask_uid(uid: &str) -> String {
    if uid.len() <= 8 {
        return uid.to_string();
    }
    format!("{}…", &uid[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_credentials_reports_a_missing_directory_as_empty() {
        let (ok, failed) = load_credentials("D:\\definitely-not-a-real-dir-placeholder");
        assert!(ok.is_empty());
        assert!(failed.is_empty());
    }

    #[test]
    fn upstream_implements_the_product_trait() {
        // 编译期断言：必须能包成 `Arc<dyn ProductUpstream>`。
        fn assert_object_safe(_: fn(crate::upstream::client::Client) -> ZcodeUpstream) {}
        assert_object_safe(ZcodeUpstream::new);
        fn assert_upstream<T: ProductUpstream>() {}
        assert_upstream::<ZcodeUpstream>();
    }

    #[test]
    fn mask_uid_hides_the_tail() {
        assert_eq!(mask_uid("short"), "short");
        assert_eq!(mask_uid("12345678"), "12345678");
        assert_eq!(mask_uid("123456789"), "12345678…");
    }
}
