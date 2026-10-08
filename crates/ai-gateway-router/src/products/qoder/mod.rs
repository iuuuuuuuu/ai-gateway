//! Qoder 产品上游：COSY 签名 + 嵌套 SSE → 标准 OpenAI SSE。
//!
//! # 移植来源（Go 侧，逐字对照）
//!
//! | 文件                          | 职责                                       |
//! |-------------------------------|--------------------------------------------|
//! | `internal/qoder/cred.go`      | 凭证解析/保存（三种形态）、机器指纹         |
//! | `internal/qoder/region.go`    | 区域与端点表                                |
//! | `internal/qoder/cosy.go`      | COSY 签名（RSA + AES-CBC + MD5）            |
//! | `internal/qoder/translate.go` | OpenAI 请求体 → Qoder agent 请求体          |
//! | `internal/qoder/encoding.go`  | 请求体编码辅助                              |
//! | `internal/qoder/client.go`    | HTTP 客户端与端点选择                       |
//! | `internal/qoder/token.go`     | 令牌刷新（OpenAPI 域，不签名）              |
//! | `internal/qoder/sse.go`       | 嵌套 SSE 解析（`{"body":"{...}"}`）          |
//! | `internal/qoder/stream.go`    | 嵌套 SSE → OpenAI SSE（含 usage 拆帧）      |
//! | `internal/qoder/dispatch.go`  | 派发 + 模型清单缓存 + `ModelNotFoundError`  |
//!
//! # 两条鉴权路径**必须分清**（混用会静默失败）
//!
//! - **Gateway 域**（`gateway.qoder.com.cn` / `api3.qoder.sh`）：模型列表、对话 ——
//!   必须 COSY 签名（[`cosy`]）。
//! - **OpenAPI 域**（`openapi.qoder.com.cn` / `openapi.qoder.sh`）：额度、用户信息、
//!   令牌刷新 —— 只需 Bearer DT，**不签名**（[`token`]）。

pub mod billing_cycle;
pub mod cli;
pub mod cred;
pub mod region;

pub(crate) mod campaign;
pub(crate) mod client;
// `qoder-login import-client`：读 Qoder 客户端自己的登录态
// （Electron safeStorage = DPAPI 保护的 AES-GCM）。
pub(crate) mod client_import;
pub(crate) mod cosy;
pub(crate) mod dispatch;
pub(crate) mod encoding;
// `qoder-login url` / `poll`：网页授权会话 + PKCE + 轮询。
pub(crate) mod login;
pub(crate) mod machine_identity;
pub(crate) mod sse;
pub(crate) mod stream;
pub(crate) mod token;
pub(crate) mod translate;
pub(crate) mod userstatus;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::auth::Auth;
use crate::error::Result;
use crate::upstream::client::ChatOutcome;

use self::dispatch::Dispatch;

use super::ProductUpstream;

/// Qoder 上游。
pub struct QoderUpstream {
    /// ⚠ **刻意不使用**传入的 [`crate::upstream::client::Client`]。
    ///
    /// 它的字段全私有、没有配置 getter，且它的端点都是 WorkBuddy 专属的
    /// （`refresh_token` / `fetch_models` 打的是别的地址）—— 拿不到配置也用不上。
    /// Qoder 的 HTTP 层必须自建：两条鉴权路径走不同域名，且流式对话**不能有总超时**
    /// （长回答会被中途掐断，表现为「回答被截断」）。故这里只保留构造签名以维持
    /// 接线稳定，实际客户端由 [`Dispatch`] 自己持有。
    _client: crate::upstream::client::Client,
    dispatch: Dispatch,
}

impl QoderUpstream {
    /// 用网关的上游客户端配置构造（签名保留以维持接线稳定，见 [`Self::_client`]）。
    pub fn new(client: crate::upstream::client::Client) -> Self {
        Self {
            _client: client,
            dispatch: Dispatch::new(),
        }
    }

    /// 包成 `Arc<dyn ProductUpstream>`，供 [`super::ProductUpstreams`] 使用。
    pub fn shared(self) -> Arc<dyn ProductUpstream> {
        Arc::new(self)
    }
}

#[async_trait]
impl ProductUpstream for QoderUpstream {
    async fn chat_stream(&self, a: &Auth, openai_body: &[u8]) -> Result<ChatOutcome> {
        self.dispatch.chat_stream(a, openai_body).await
    }

    /// 非流式：上游原始嵌套 SSE 全文 → 一个 OpenAI 非流式响应。
    ///
    /// 翻译只发生在流式路径，故这里直接按自家嵌套形状聚合（与 Go
    /// `Dispatch.Aggregate` 转发给 `AggregateQoder` 同构）。
    async fn aggregate(&self, sse: &str, model: &str) -> std::result::Result<Map<String, Value>, String> {
        stream::aggregate(sse, model)
    }
}

/// 加载一个目录下的全部 Qoder 凭证，转成账号池用的 [`Auth`]。
///
/// 对应 Go `cmd/server/main.go:448-472` 的 `qoder.LoadDir` + `qoderAuthOf`：
/// 域名决定区域（未知区域回落国服，见 [`cred_of`](dispatch) 的说明），
/// 指纹等产品专属字段留在凭证文件里，由对话路径自己读回。
///
/// # ★★ 指纹必须在这里落盘，不能留给请求路径
///
/// `machine_id` / `machine_token` 是 COSY 签名的**必需输入**，而上游把它们当作
/// **设备身份**：同一账号的指纹变了，上游看到的就是「一台新设备」。
///
/// Go 在加载阶段就 `EnsureFingerprint()` + `SaveAtomic()`（`main.go:462-467`），
/// 于是指纹**跨重启稳定**。若把补指纹留给请求路径，[`dispatch::cred_of`] 每次都从
/// 文件读回（`Auth` 刻意不承载产品专属字段），文件里又没有 —— 于是**每个请求都
/// 现生成一对新指纹**，同一账号在两次请求里表现为两台设备。上游侧的表现是风控
/// 命中（3012）甚至直接拒绝，而网关日志一切正常，极难定位。
///
/// 故这里与 Go 同款：补齐后立即 `save_atomic`。写盘失败**只打日志不中断**
/// （账号仍可用，只是下次启动会再补一次）—— 与 Go 的 `log.Printf` 同口径。
///
/// 返回 `(成功的账号, 失败描述)`。⚠ 失败**必须报出来**而不是静默跳过：Qoder 的
/// 凭证格式是用户可能手写的，格式错误要让用户看见（与 `auth::load_dir` 的
/// 「静默跳过」是刻意不同的口径）。
pub fn load_credentials(dir: &str) -> (Vec<Auth>, Vec<String>) {
    let (mut creds, failed) = cred::QoderCred::load_dir(dir);
    let mut out = Vec::with_capacity(creds.len());
    for cr in creds.iter_mut() {
        // `uid` 为空 = 无法进池（池以 uid 为键），与 Go `main.go:459-461` 同款跳过。
        if cr.uid.is_empty() {
            continue;
        }
        if cr.ensure_fingerprint() {
            // 只有**变化**才写盘：`LoadFile` 在每个请求路径上都会被调用，
            // 无条件写会让一次对话产生一次磁盘写入（Go 的注释专门警告过）。
            if let Err(e) = cr.save_atomic() {
                crate::logline!(
                    "Qoder 账号 {} 保存机器指纹失败（该账号本次仍可用，下次启动会重试）：{e}",
                    mask_uid(&cr.uid)
                );
            }
        }
        out.push(cr.to_auth());
    }
    (out, failed)
}

/// 脱敏 uid（日志里不出现完整账号标识）。
///
/// 本仓库是公开仓库，日志可能被用户贴到 issue 里 —— 只留前 8 位足够定位，
/// 又不足以反查账号。对应 Go `main.go:96` 的 `maskUID`。
fn mask_uid(uid: &str) -> String {
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
        // 编译期断言：QoderUpstream 必须能包成 Arc<dyn ProductUpstream>。
        fn assert_object_safe(_: fn(crate::upstream::client::Client) -> QoderUpstream) {}
        assert_object_safe(QoderUpstream::new);
        fn assert_upstream<T: ProductUpstream>() {}
        assert_upstream::<QoderUpstream>();
    }
}
