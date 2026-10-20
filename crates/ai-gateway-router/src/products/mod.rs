//! 产品上游层：把「非 WorkBuddy 产品」的对话请求接进同一套转发流程。
//!
//! # 为什么需要这一层（而不是在 `forward.rs` 里 `if product == "qoder"`）
//!
//! 网关内部有**三个**协议消费者（`/v1/chat/completions`、`/v1/messages`、
//! `/v1/responses`），它们的入站形状不同、但出站都汇合成 OpenAI SSE 交给
//! [`crate::upstream::sse::SseFramesIter`]。若在每个消费者里都写一遍
//! 「如果这次命中的是 Qoder 就换种读法」，就会有**三处分支、三处可能漏改**
//! —— 实测代价：第一版只判了 `isQoder`，于是 ZCode 的账号走了 WorkBuddy 的
//! 聚合器，表现为「无法连接上游」这种与真实原因完全无关的报错。
//!
//! 故本模块的契约是：**产品上游负责把自家协议翻译成标准 OpenAI 形状**，
//! 消费者永远不需要知道产品差异。
//!
//! # 与 Go 侧的对应关系
//!
//! | Go                                          | Rust                          |
//! |---------------------------------------------|-------------------------------|
//! | `internal/server/dispatch.go` `ProductUpstream` | 本文件的 [`ProductUpstream`] |
//! | `dispatchUpstream(a *auth.Auth)`            | [`ProductUpstreams::dispatch`] |
//! | `internal/qoder`（COSY 签名 + 嵌套 SSE）     | [`qoder`]                     |
//! | `internal/zcode`（Anthropic 双向翻译）       | [`zcode`]                     |
//!
//! # WorkBuddy 刻意**不走**这里
//!
//! WorkBuddy 是默认路径，直调 [`crate::upstream::client::Client`]。它的路径
//! 最热，不为「对称」加一层间接 —— 与 Go 侧同款取舍。

pub mod product_tasks;
pub mod qoder;
pub mod trae;
pub mod zcode;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::auth::Auth;
use crate::error::Result;
use crate::upstream::client::ChatOutcome;

/// 一个产品上游能做的事：发一次对话、把结果聚合回来。
///
/// 对应 Go `internal/server/dispatch.go:50-63` 的两方法接口。**只有两个方法**
/// 是刻意的最小面：
///
/// - `chat_stream` 返回的流**必须已经是标准 OpenAI SSE**。这是本 trait 的
///   核心约定 —— 否则「三个消费者各自判产品」的缺陷会立刻回来。
/// - `aggregate` 收到的**是上游原始流的全文**（未翻译），因为 Qoder 的非流式
///   聚合要按自家嵌套形状解析；翻译只发生在流式路径。
#[async_trait]
pub trait ProductUpstream: Send + Sync {
    /// 发起一次对话。
    ///
    /// 返回语义（与 [`crate::upstream::client::Client::chat_stream`] 一致，
    /// 这样 [`crate::forward`] 的 `match outcome` 段可以零改动）：
    ///
    /// - `Ok(ChatOutcome::Stream(_))`：成功，流内容已翻译成 OpenAI SSE。
    /// - `Ok(ChatOutcome::Failed { status, body })`：**上游拒绝**（非 2xx）。
    ///   不返回 `Err`，因为调用方需要 `classify(status, body)` 决定冷却策略。
    /// - `Err(e)`：传输层/构造层失败。★ 三条特殊错误要在调用方被认出：
    ///   [`crate::error::GatewayError::QuotaExhausted`]（额度耗尽 → 402，
    ///   不轮转）、[`crate::error::GatewayError::ModelNotFound`]（上游清单
    ///   没有它 → 400，不轮转不罚号）、
    ///   [`crate::error::GatewayError::CaptchaRequired`]（要求人机验证 → 不轮转
    ///   不罚号）。
    async fn chat_stream(&self, a: &Auth, openai_body: &[u8]) -> Result<ChatOutcome>;

    /// 把上游原始流聚合成一个 OpenAI **非流式**响应。
    ///
    /// 参数 `sse` 是上游流全文（Qoder 为嵌套 SSE、ZCode 为标准 OpenAI SSE），
    /// `model` 是客户端请求的裸模型名（已剥掉产品/区域前缀），用于回填
    /// 响应里的 `model` 字段。
    async fn aggregate(&self, sse: &str, model: &str) -> std::result::Result<Map<String, Value>, String>;

    /// 该模型在本产品上的**计费倍率**（`None` = 本产品没有这个概念或未声明）。
    ///
    /// # 为什么放在 trait 上而不是各调用点自己查
    ///
    /// 倍率的**数据源因产品而异**：WorkBuddy 来自上游 `/v3/config` 的能力索引
    /// （按区域、由 [`crate::capability`] 提供），Qoder 来自模型清单里的
    /// `price_factor`（不分区、由账号凭证自带的清单提供）。若让
    /// [`crate::server::record_usage`] 自己分支，它就得知道每个产品内部的
    /// 数据放在哪 —— 那是把产品实现细节泄进计费层。
    ///
    /// 默认实现返回 `None`（= 该产品没有倍率概念，界面不显示倍率列）。
    /// 这是**保守的正确默认**：宁可不说，也不给一个可能错的数字。
    ///
    /// ⚠ 返回 `Some(0.0)` 与 `None` **语义完全不同**：前者是「免费」（确定），
    /// 后者是「不知道」。见 [`crate::usage::Billing`] 的三态语义。
    fn model_multiplier(&self, _model: &str) -> Option<f64> {
        None
    }

    /// 启动时预热「模型清单」缓存（默认什么也不做）。
    ///
    /// # 为什么需要它（2026-10-20 倍率现场）
    ///
    /// 倍率来自产品自己的模型清单缓存。缓存只在**第一次对话**时才填充，
    /// 于是「网关刚启动、还没发过对话」时，模型路由清单里所有倍率都是空的 ——
    /// 用户会以为这个功能没做。这里在启动后（后台任务）拉一次清单，
    /// 让页面首次打开就能看到真实倍率。
    ///
    /// 失败**不得**影响启动：由实现自己打日志并返回。
    async fn warm_model_cache(&self, _accounts: &[Auth]) {}
}

/// 已配置的产品上游集合。
///
/// 对应 Go `server.Config` 里的 `Qoder *qoder.Dispatch` / `Zcode *zcode.Dispatch`
/// 两个字段，以及 `dispatchUpstream` 的判定逻辑。
///
/// # 产品未知时**回退 WorkBuddy**，而不是报错
///
/// 生产环境里绝大多数账号没有 `product` 字段（老凭证文件里根本没有这个概念），
/// 报错会让既有用户**立刻不可用**。故 [`Self::dispatch`] 对未知产品返回
/// `None`，调用方按 WorkBuddy 走。
///
/// ⚠ 反向风险：把「其它产品的凭证」误判成 WorkBuddy 会造成
/// **用别人的 token 去打 WorkBuddy 端点**。所以凭证加载层必须正确设置
/// [`crate::auth::Auth::product`]（见 [`crate::auth::Auth::product_of`] 的
/// 归一化说明）—— 这个前提不成立时本函数无法补救。
#[derive(Default)]
pub struct ProductUpstreams {
    /// Qoder 上游（`None` = 未配置，该产品账号会被当成 WorkBuddy）。
    pub qoder: Option<Arc<dyn ProductUpstream>>,
    /// ZCode 上游（`None` = 未配置）。
    pub zcode: Option<Arc<dyn ProductUpstream>>,
    /// Trae 上游（`None` = 未配置）。国服与国际版**共用一个上游**，
    /// 区域由账号凭证的 `domain` 决定（见 `trae::Realm`）。
    pub trae: Option<Arc<dyn ProductUpstream>>,
}

impl ProductUpstreams {
    /// 空的集合：所有产品都按 WorkBuddy 走（等价于引入多产品之前的行为）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 按账号的产品选上游。
    ///
    /// 返回 `None` 表示「这不是产品账号」或「该产品的上游没配置」——
    /// 两种情况调用方都走 WorkBuddy 默认路径（与 Go `dispatchUpstream` 的
    /// `(nil, false)` 同语义）。
    pub fn dispatch(&self, a: &Auth) -> Option<&Arc<dyn ProductUpstream>> {
        if a.is_qoder() {
            return self.qoder.as_ref();
        }
        if a.is_zcode() {
            return self.zcode.as_ref();
        }
        if a.is_trae() {
            return self.trae.as_ref();
        }
        None
    }

    /// 该账号是否**应当**走产品上游路径。
    ///
    /// ⚠ 与 [`Self::dispatch`] 的区别很重要：本函数只看账号的产品标签，
    /// 不看上游是否配置。转发层要用**本函数**（而不是 `dispatch().is_some()`）
    /// 来决定「要不要跳过 WorkBuddy 的 token 刷新」—— 因为一个 Qoder 账号
    /// 即使上游没配置，也**绝不该**去调 WorkBuddy 的刷新接口（那是别人的
    /// 端点，调了必然失败并把这个好账号冷却掉）。对应 Go
    /// `forward.go:565-572` 的 `isProduct`。
    pub fn is_product_account(a: &Auth) -> bool {
        a.is_qoder() || a.is_zcode() || a.is_trae()
    }

    /// 按**产品名**选上游（不是按账号）。
    ///
    /// # 为什么需要它（与 [`Self::dispatch`] 的分工）
    ///
    /// [`Self::dispatch`] 回答的是「这个**账号**该走哪个上游」—— 转发路径用它。
    /// 本函数回答的是「这个**产品**的上游在哪」—— 计费路径用它：
    /// [`crate::server::record_usage`] 手上只有 `product` 字符串（计费归属），
    /// 没有 `Auth`。
    ///
    /// 传 `workbuddy` 或未知产品名返回 `None`（WorkBuddy 的倍率走能力索引，
    /// 与本集合无关）。
    pub fn dispatch_by_product(&self, product: &str) -> Option<&Arc<dyn ProductUpstream>> {
        match product {
            crate::auth::PRODUCT_QODER => self.qoder.as_ref(),
            crate::auth::PRODUCT_ZCODE => self.zcode.as_ref(),
            crate::auth::PRODUCT_TRAE => self.trae.as_ref(),
            _ => None,
        }
    }
}
