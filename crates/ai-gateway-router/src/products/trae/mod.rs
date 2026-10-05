//! Trae 产品适配器（国服 / 国际版共用一套上游）。
//!
//! # 分层
//!
//! | 文件 | 职责 |
//! | --- | --- |
//! | [`realm`] | 区域表（chat host / auth host / IDE 版本 / OAuth client_id）与区域探测 |
//! | [`cred`] | 凭证文件的读 / 写 / 原子落盘（跨进程契约，字段全可缺省） |
//! | [`device`] | 设备指纹：从真实客户端的数据目录读，读不到才退回宿主身份文件 |
//! | [`headers`] | 请求头拼装（唯一一处决定发哪些头的代码） |
//! | [`translate`] | 上游 SSE 与 OpenAI SSE 的**流式**互转与非流式聚合 |
//! | [`client`] | 网络层：llm_utils_chat、get_detail_param、ExchangeToken 刷新 |
//! | [`dispatch`] | 编排层：取凭证 → 刷新（如需）→ 模型名解析 → 转发 |
//! | 本文件 | 把上面这些接成 [`ProductUpstream`] |
//!
//! # ★ 与 forward.rs 的三条硬约定
//!
//! 1. **chat_stream 交出去的流必须已经是标准 OpenAI SSE。**
//!    forward.rs 在落地前会先探测首帧（sse::probe_first_frame），首帧非法 /
//!    为空 / 是错误帧时，非 ZCode 产品一律按 503 冷却换号。所以翻译**必须在**
//!    [`client`] 内部完成（reqwest::Body::wrap_stream，纯流式、绝不缓冲），
//!    且第一帧就得是合法的 data: {...}。
//! 2. **上游非 2xx 用 Ok(ChatOutcome::Failed { status, body }) 返回，不要 Err。**
//!    Failed 会走 forward.rs 的 request_side_failure 分类（额度 / 风控 / 参数），
//!    而 Err 会走传输层分支并**轮转账号** —— 把「模型名写错了」这种请求侧问题
//!    变成「换一个账号重试」，用户看到的是「换了号还是失败」。
//! 3. **三个特殊错误必须保持可识别**（forward.rs:698-748 按类型分派，不轮转）：
//!    - [`crate::error::GatewayError::QuotaExhausted`] ⇒ 402，不轮转；
//!    - [`crate::error::GatewayError::ModelNotFound`] ⇒ 400，不轮转不罚分；
//!    - [`crate::error::GatewayError::CaptchaRequired`] ⇒ 503，不轮转不罚分。
//!    它们**只能**从 chat_stream 的 Err 分支出来（Failed 分支看不到类型）。
//!
//! # ★ Trae 是第一个需要「在 chat_stream 内部刷新 token」的产品
//!
//! forward.rs:655-663 的 token 刷新**只对 WorkBuddy 生效**
//! （if !is_product && acct.needs_refresh(...)）—— 产品账号被显式跳过，
//! 因为 Qoder 的 token 由宿主换、ZCode 根本没有刷新链路。
//! Trae 的 access token（Cloud-IDE-JWT 前缀 + jwt）会过期，而宿主只在
//! 「用户点登录」时换一次，因此刷新必须落在 [`dispatch`] 里，
//! **不要**去改 forward.rs。
//!
//! 刷新的区域（ClientID）必须跟账号走：国服用 en1oxy7wnw8j9n，
//! 国际版用 ono9krqynydwx5（见 [`realm::oauth_client_id`]）。
//! 核心库的 trae_refresh::exchange_token 内部靠 detect_flavor() 自己判区域，
//! **没法被告知账号属于哪个区域**，所以这里必须自带一份实现。
//!
//! # 与 WorkBuddy 的隔离
//!
//! Trae 的区域语义与 WorkBuddy 的 region_of_domain 完全不同，
//! 因此 [`cred::TraeCred::to_auth`] 刻意把 Auth::domain 留空、
//! 把区域放在 Auth 之外（由 [`dispatch`] 从凭证重新读）。
//! 同理，本模块**不读** AI_GATEWAY_HOME：需要 store_dir 时用
//! [`device::store_dir_of_cred_file`] 从凭证路径反推。

pub mod cred;
pub mod credits;
pub mod device;
pub mod headers;
pub mod realm;
pub mod translate;

pub(crate) mod client;
pub mod dispatch;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::auth::Auth;
use crate::error::Result;
use crate::upstream::client::ChatOutcome;

use super::ProductUpstream;
use client::{ChatCall, Client};
use dispatch::Dispatch;

/// Trae 上游。
///
/// ★ [`_client`] 的**下划线是刻意的**：crate::upstream::client::Client 的字段
/// 全是私有的、没有任何配置 getter，而它的端点常量都是 WorkBuddy 专用的
/// （CHAT_BASE_CN = copilot.tencent.com 等）—— Trae 的端点、代理、超时全部由
/// 本包的 [`Client`] 独立管理。这里保留一个句柄只是为了将来接「共享连接池」
/// 之类的配置，现在**一个字段都不读**。不加下划线会得到一个
/// field is never read 警告，而那个警告会掩盖真正的死代码。
pub struct TraeUpstream {
    _client: crate::upstream::client::Client,
    dispatch: Dispatch,
}

impl TraeUpstream {
    /// 用网关的上游客户端配置构造。
    ///
    /// 构造时完成进程级接线：[`Client::apply_host_proxy`] 按
    /// proxy_scope.intl 决定 Trae 是否走代理。trae.ai / trae.cn 分属国际
    /// 与国内流量，但**区域由账号决定**（同一个上游服务两边的账号），
    /// 所以这里只能按宿主给的口径统一设置；设置失败只打日志、**不阻断启动**。
    pub fn new(client: crate::upstream::client::Client) -> Self {
        let mut inner = Client::new();
        inner.apply_host_proxy();
        Self {
            _client: client,
            dispatch: Dispatch::new(inner),
        }
    }

    /// 包成 Arc<dyn ProductUpstream>。
    pub fn shared(self) -> Arc<dyn ProductUpstream> {
        Arc::new(self)
    }
}

#[async_trait]
impl ProductUpstream for TraeUpstream {
    async fn chat_stream(&self, a: &Auth, openai_body: &[u8]) -> Result<ChatOutcome> {
        match self.dispatch.chat_stream(a, openai_body).await? {
            // 上游响应**已经是标准 OpenAI**（Trae 的 SSE 在 client 内部流式翻译掉了），
            // 故直接交给上层，不需要任何包装。
            ChatCall::Stream(resp) => Ok(ChatOutcome::Stream(resp)),
            // 上游拒绝 ⇒ 走 Failed 让 forward.rs 按状态码分类（额度/风控/参数）。
            ChatCall::Rejected { status, body } => Ok(ChatOutcome::Failed { status, body }),
        }
    }

    async fn aggregate(
        &self,
        sse: &str,
        model: &str,
    ) -> std::result::Result<Map<String, Value>, String> {
        // ★ 入参是**未翻译的上游原始流**（forward.rs:1116-1125 传的就是它），
        //   不是本模块翻译后的 OpenAI 流 —— 见 [`ProductUpstream::aggregate`] 的文档。
        self.dispatch.aggregate(sse, model)
    }
}

/// 从凭证目录加载 Trae 账号，转成网关账号池用的 [`Auth`]。
///
/// # 与 Qoder / ZCode 同款的两个约定
///
/// 1. **uid 为空 = 无法进池**（池以 uid 为键），直接跳过。
/// 2. **失败必须逐条报出来**，不能静默跳过 —— 凭证由宿主导出，也可能是用户
///    手写的（粘贴导入），格式错误要让用户看见。失败描述直接沿用
///    [`cred::load_dir`] 的文本（形如 trae-bad.json: 不是合法 JSON: …）。
///
/// # ⚠ 刻意**没有** save_atomic 循环
///
/// Qoder 在加载阶段补 COSY 机器指纹并立即写盘，否则「每个请求都现生成一对新
/// 指纹，同一账号在两次请求里表现为两台设备」。Trae 没有这类「加载时派生、
/// 必须持久化」的字段：
///
/// - machineId / deviceId 是**读**真实客户端数据目录得到的（[`device::resolve`]），
///   不是派生物；
/// - 设备密钥对（DeviceProof 用）在 [`client`] 内部按需铸造并缓存到
///   trae_device.json，与核心库 trae_oauth::device_keypair 共用同一份文件
///   （见 [`device::DEVICE_FILE`]），因此也不需要在加载阶段预热。
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
/// 又不足以反查账号。对应 Go main.go 的 maskUID。
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
        let (ok, failed) = load_credentials("D:/definitely-not-a-real-dir-placeholder");
        assert!(ok.is_empty());
        assert!(failed.is_empty());
    }

    #[test]
    fn upstream_implements_the_product_trait() {
        // 编译期断言：必须能包成 Arc<dyn ProductUpstream>。
        fn assert_object_safe(_: fn(crate::upstream::client::Client) -> TraeUpstream) {}
        assert_object_safe(TraeUpstream::new);
        fn assert_upstream<T: ProductUpstream>() {}
        assert_upstream::<TraeUpstream>();
    }

    #[test]
    fn mask_uid_hides_the_tail() {
        assert_eq!(mask_uid("short"), "short");
        assert_eq!(mask_uid("12345678"), "12345678");
        assert_eq!(mask_uid("123456789"), "12345678…");
        assert_eq!(mask_uid("trae-2593120869167228"), "trae-259…");
    }
}
