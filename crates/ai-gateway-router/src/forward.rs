//! 转发核心：选号 → token 刷新 → 转发 → 失败换号的完整轮转。
//!
//! 对应 Go 源文件 `internal/server/forward.go`。
//!
//! # 为什么抽成独立模块
//!
//! 三种协议入口（Chat Completions / Responses / Anthropic Messages）共用本流程：
//! 后两者在进入本流程前把请求体转成 OpenAI Chat 形态，拿到响应后各自再转回去。
//! 这样账号池、粘性会话、熔断冷却、模型冷却、统计日志**只有一份实现**，
//! 协议适配层只负责形状转换，不碰任何调度状态。
//!
//! # 失败语义
//!
//! - 传输层错误（超时/连接被拒/DNS）→ 只换号，**不喂熔断**
//! - 业务错误 → 按 `classify` 结果施加冷却 / 禁用 / 熔断
//! - 上下文超长 → **立即失败**（确定性错误，重试无用）
//! - 请求被上游拒绝（11155 思维链缺失）→ **原地重试**，
//!   预算 2 次；用尽才按请求侧错误落定
//! - 请求侧错误（思考档位被拒 / 11102 模型不在通道 / 11133 参数被拒）
//!   → **立即失败**，不轮转、不罚账号
//! - 账号侧不可用（403 11140）→ **账号级软冷却**，换号继续
//!
//! 最后几类是改变对外状态码的路径；其余失败沿用 503 `no_healthy_account`
//! 契约（语义是「账号池暂时不可用，稍后重试」）。
//!
//! # 为什么「请求被拒」要重试（实测数据，勿凭直觉回退）
//!
//! ★ 适用范围：**仅 11155**。11140 曾与此处共用重试预算，那是错的 ——
//! 11140 是**账号侧**错误（见 `classify::ErrKind::AccountUnusable`），
//! 对坏号原地重试等于把「换个号就能成功」变成「同一个打不通的号被连打三次」，
//! 同时坏号因为不冷却而永远留在候选集里。
//!
//! 实测 11155 在同一请求体上按 **~10~25%** 概率**随机**出现，与账号、模型、
//! 协议都无关（会话粘性钉住单账号仍随机；hy3 与 deepseek 失败率相同；
//! Chat 与 Responses 失败率相同；连「列出三原色」这种无害纯文本也 20 次挂 2 次）。
//!
//! 同一份 body 连发 40 次：首轮成功 30 次，把 10 次失败原样重发（最多 2 次）
//! 后 **10/10 全部救回，0 次重试仍失败** —— 单请求成功率 75% → 100%。
//!
//! 结论：11155 的判定带服务端抖动，**不是**确定的请求侧错误。原实现按
//! 「换号无用」直接短路返回，等于把上游抖动原样透传给用户 —— agent 一个任务
//! 要发几十上百个请求，单次 90% 的成功率会让整轮任务几乎必然失败，
//! 用户看到的就是「OpenAI 格式完全用不了」。


use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use crate::auth::Auth;
use crate::pool::{CoolKind, Pool, Region};
use crate::session::Router;
use crate::upstream::classify::{
    self, context_too_long_message, friendly_message, request_rejected_message, ErrKind,
    UpstreamError,
};
use crate::upstream::client::{ChatOutcome, Client};
use crate::upstream::sse;

/// 失败类别：决定回给客户端的错误码。
///
/// 存在的意义是让**请求侧**错误说实话：旧实现无论什么原因都回
/// `no_healthy_account` + "all accounts unavailable (cooling/disabled)"，
/// 把「这次请求太大」伪装成「账号全挂了」，排查时被直接带偏。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// 其余上游失败：沿用既有契约（503 no_healthy_account）。
    Upstream,
    /// 请求的模型不在「限制使用的模型」白名单内（400 `model_not_allowed`）。
    ///
    /// ★ 与 [`FailureKind::Upstream`] 分开是**必需的**（Go `handler.go:1296-1305`
    /// 逐字）：区分「模型不在白名单」与「账号都不可用」很重要 —— 前者是
    /// **调用方需要改的东西**（换模型或调整「放行模型」），后者是**服务端状态**。
    /// 都报 `no_healthy_account` 会把用户引向排查账号，而真正的原因在请求里。
    ///
    /// ★ 名字沿用 Go 的历史叫法 `modelLockedError`：错误码 `model_not_allowed`
    /// 与各协议的判定链都建在它上面，改名的收益抵不过动这条链的风险。
    ModelNotAllowed,
    /// 请求上下文超出模型窗口：请求侧错误，换号无用。
    ContextTooLong,
    /// 请求内容被上游拒绝（11155 思维链缺失 / 11140 安全审核）：
    /// 已用尽重试预算，按请求侧错误落定，原样回传上游状态与文案。
    RequestRejected,
    /// 带图片的请求需要特定区域的账号，而该区域此刻没有可用账号。
    ImageRegionUnavailable,
    /// 该范围内**全部**可用账号都被用户设成了「不参与选号」（`no_route`）。
    ///
    /// 与 [`FailureKind::Upstream`] 分开：两者都是 503，但出路完全不同 ——
    /// 「账号全挂了」要等冷却，「你自己关了路由开关」只需去界面上打开。
    /// 混为一谈会让用户去等一个永远不会到来的恢复。
    AllNoRoute,
    /// **用户白名单**不允许「该平台 × 该区域」跑这个模型。
    ///
    /// 与「上游没有这个模型」是两回事：这里模型可能存在，只是用户没放行，
    /// 去配置页加上即可。故用 400 而不是 503 —— 503 的语义是「稍后重试就好」，
    /// 而这里等多久都不会出现。
    ModelNotInProduct,
    /// **上游**（Qoder/ZCode）的模型清单里没有这个模型。
    ///
    /// 与 [`FailureKind::ModelNotInProduct`] 的区别：那个是「用户没放行」，
    /// 这个是「上游根本没有」。两者都是请求侧错误（400）且都**绝不轮转、
    /// 绝不罚账号**（Go `forward.go:641-669` 逐字）：
    ///
    /// - 绝不轮转：同一请求体发给任何账号，上游清单都一样没有它，轮转只是
    ///   把同一请求对着每个账号重传一遍。
    /// - 绝不罚账号：这是请求侧错误，账号无辜。罚号会让好账号被冷却，用户
    ///   改好模型名后以为「账号又挂了」，故障面被放大。
    ModelNotInUpstream,
    /// **用户把整个平台**的「参加兼容网关路由」开关关掉了。
    ///
    /// 与 `[FailureKind::AllNoRoute]` 的区别是粒度：那个是账号级 no_route，
    /// 用户会去逐个账号翻开关；这个是平台级，一个开关管一片。
    ///
    /// ★ 必须与 `[FailureKind::ModelNotInProduct]` 分开：两者都是 400、
    /// 都不轮转，但**出路相反** —— 前者要用户去「兼容网关」页改白名单，
    /// 后者要用户回账号页打开平台开关。合并会让用户照着文案改一个
    /// 跟故障毫无关系的配置（Go 侧同款教训见 `handler.go:1103-1110`）。
    ///
    /// ★ 文案必须强调「养号任务照跑」：所有者原话
    /// > 「关闭平台只是不参与兼容网关,并不是不执行任务请注意」
    /// 否则用户会以为签到/成长任务也被停了。
    PlatformNotInRouting,
    /// 账号额度耗尽（ZCode 套餐过期等）：账号级失败，走 402。
    ///
    /// 用 402 而不是 503 的理由：上游会一直回 200 但流是空的，等下去只会
    /// 拖到超时；402 让客户端立刻知道「这个号没钱了，换一个」。
    QuotaExhausted,
    /// 上游说「**这条通道没有该账号的资源包**」（ZCode 业务码 1113，HTTP 429）：
    /// 不是账号没额度，而是这个模型不在该账号的套餐里。
    ///
    /// 与 [`FailureKind::QuotaExhausted`] 的**出路完全不同**：那个要用户去充值 /
    /// 等签到，这个只要**换账号**（或改用在套餐内的模型）—— 因此绝不能共用同一个
    /// 码面值，否则用户会拿着一个额度满格的账号去充值。
    ///
    /// 用 400 而不是 402：402 在客户端侧的语义是「余额不足、充值即可」，而这里
    /// 充值是无效动作；400 + `invalid_request_error` 的语义正是「这条请求在
    /// 当前账号上不成立」。
    NoResourcePack,
    /// 上游要求人机验证（ZCode 3007）：一次性要求，不轮转不罚账号。
    ///
    /// 轮转**必定**无用 —— 验证码是**账号级**要求，换个号打同一个请求只会
    /// 让每个号都被要求一次，把「一个号要验证」放大成「所有号都不可用」。
    CaptchaRequired,
    /// 上游不接受本次指定的思考档位（`reasoning_effort`）：请求侧错误。
    ///
    /// ★ **不要**据此在网关侧预校验档位：上游声明的 `supportedEfforts` 不是硬范围
    ///（实测 hy3 声明 `[low,high]` 却接受 medium/max/off）。网关侧拦截会把
    /// 合法请求挡在门外，而用户恰恰要靠上游原文判断该换哪个档位。
    EffortRejected,
    /// 该模型在当前通道上不存在（400 code=11102）：请求侧错误。
    ///
    /// 与 [`FailureKind::ModelNotInUpstream`] 的区别：那个是「上游清单里没有
    /// 这个名字」，这个是「**这条通道**（平台×区域）上没有它」。同名模型可能只在
    /// 某一侧上游存在，故出路是换模型或显式指定区域。
    ///
    /// ⚠ 与「排队」（10605）**完全不同**：排队稍后可用，这个**等多久都没用**。
    ModelNotInRegion,
    /// 请求参数被模型提供商拒绝（400 code=11133）：请求侧错误。
    ///
    /// ⚠ **不与 [`FailureKind::ContextTooLong`] 合并**：那条让客户端触发**自动
    /// 压缩**，而 11133 的 `param` 是空串、未必与长度有关，误导客户端压缩无济于事。
    ///
    /// ★ 轮转在这里尤其有害：所有者 2026-09-28 现场是一个 417 轮 / 458M token 的
    /// 开发会话，被当成账号故障逐号轮转 ⇒ 大请求体被对着每个账号重传
    ///（11102 那条注释记着实测：1.12M token 的请求被重传 3 次）。
    ModelParamInvalid,
    /// 上游 3012「unusual activity」：**请求级瞬时风控**，按产品熔断。
    ///
    /// ★ 判据与 [`FailureKind::EgressIPBlocked`] 不同，**不要合并**：
    ///
    /// - 这里 = **单个**账号吃到 3012 ⇒ 上游对**本次请求**的风控。
    /// - 那个 = **多个不同账号**都吃 403 ⇒ 出口 IP 被拦。
    ///
    /// 3012 不需要「多个账号才成立」：它已在官方客户端上复现（日志
    /// `turn.failed` 405 code=3012），即与我们的账号池**无关**。再花 20 次请求
    /// 去交叉验证只会加重风控 —— 而那正是本分支要消除的行为。
    ///
    /// ★ 刻意**不罚账号**（Go `forward.go:689-716` 逐字）：3012 与账号无关，
    /// 罚号会让好账号被冷却，用户解封后还会发现号被自己人停了。
    UnusualActivity,
    /// 出口 IP 被 WAF 拦截（窗口内多个不同账号都吃 403）。
    ///
    /// ★★ 必须在**继续换号之前**判定。否则所有号轮一遍全 403 而网关还在换号，
    /// 把请求放大 `MaxRotate` 倍 —— 恰好是 WAF 最想惩罚的行为，封禁越打越重。
    ///
    /// ★ 判定成立时**不动账号状态**：账号是好的，问题在网络出口。把好账号冷却掉
    /// 只会让用户在「IP 解封之后」发现号也被自己人停了。
    EgressIPBlocked,
    /// 上游整体空流（窗口内多个**不同**账号在同一（产品, 区域）上 HTTP 200
    /// 但流里一帧有效数据都没有）。
    ///
    /// 判据与 [`FailureKind::UnusualActivity`]（单号 3012）、
    /// [`FailureKind::EgressIPBlocked`]（多号 403）都不同，不要合并：
    /// 三者都指向「继续换号无用」，但故障主体依次是 账号自身 / 本机出口 IP /
    /// 上游服务 —— 结论不同，出路也不同（见 [`crate::emptystream`]）。
    ///
    /// ★ 判定成立时**不动账号状态**：账号是好的，是上游通道整体故障。2026-10-11
    /// 现场的教训：13 个国际区账号被轮一遍、全部喂进熔断，最终把「上游故障」
    /// 变成「全网关无号可用」。详见 [`crate::emptystream::empty_stream_burst_message`]。
    EmptyStreamBurst,
}

/// 一次需要特殊上报的转发失败。
#[derive(Debug, Clone)]
pub struct ForwardFailure {
    /// 失败类别。
    pub kind: FailureKind,
    /// 回给客户端的 HTTP 状态。
    pub status: u16,
    /// 面向客户端的错误消息。
    pub message: String,
}

impl std::fmt::Display for ForwardFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ForwardFailure {}

/// 一次成功的上游调用结果。
pub enum ChatResult {
    /// 流式：调用方负责按目标协议解析；租约由调用方读完/关闭后释放。
    Stream {
        /// 选中的账号 UID。
        uid: String,
        /// 请求的目标模型。
        model: String,
        /// ★ 实际供应的产品（`""` = WorkBuddy）。
        ///
        /// 与 Go `forward.go:970-973` 同口径：只有**非 WorkBuddy** 的产品账号
        /// 才填，WorkBuddy 留空。用途是让上层（用量统计、日志、`/status`）
        /// 能区分「这次回答是谁供的」—— 多产品池里同一个模型名可能由两个
        /// 产品供应，只看模型名无法归因。
        product: String,
        /// ★ 账号区域（`"cn"`/`"intl"`；`""` = 不适用）。
        ///
        /// ⚠ **只有 WorkBuddy 才取区域**（Go `forward.go:974-981` 逐字）：
        /// 「非 WorkBuddy 产品（Qoder/ZCode）不分区 —— 它们的账号池没有区域
        /// 维度，给它们标一个区域会是**编造**」。
        region: String,
        /// 上游 SSE 响应流。
        response: reqwest::Response,
        /// ★ 首帧预读**已经消费掉**的原始字节，必须由调用方在读取 `response`
        /// 之前交回给帧解析器（见 [`crate::upstream::sse::SseFramesIter::push_prefix`]）。
        ///
        /// 丢掉它就等于丢掉回答的开头几个字（Go `sse.go:383-392` 逐字警告）。
        /// 正常情况下是空串。
        prefix: String,
    },
    /// 非流式：上游 SSE 已被聚合成 OpenAI `chat.completion`。
    Response {
        /// 选中的账号 UID。
        uid: String,
        /// 请求的目标模型。
        model: String,
        /// ★ 实际供应的产品（`""` = WorkBuddy）。
        product: String,
        /// ★ 账号区域（`""` = 非 WorkBuddy，见 [`ChatResult::Stream`]）。
        region: String,
        /// 聚合后的响应体。
        body: serde_json::Map<String, serde_json::Value>,
    },
}

impl ChatResult {
    /// 本次结果关联的账号 UID（失败路径回填最后尝试过的账号，供日志使用）。
    pub fn uid(&self) -> &str {
        match self {
            ChatResult::Stream { uid, .. } | ChatResult::Response { uid, .. } => uid,
        }
    }

    /// 本次请求的目标模型。
    pub fn model(&self) -> &str {
        match self {
            ChatResult::Stream { model, .. } | ChatResult::Response { model, .. } => model,
        }
    }

    /// 实际供应的产品（`""` = WorkBuddy）。
    pub fn product(&self) -> &str {
        match self {
            ChatResult::Stream { product, .. } | ChatResult::Response { product, .. } => product,
        }
    }

    /// 账号区域（`""` = 非 WorkBuddy）。
    pub fn region(&self) -> &str {
        match self {
            ChatResult::Stream { region, .. } | ChatResult::Response { region, .. } => region,
        }
    }
}

/// 转发上下文：所有依赖由调用方注入（便于测试替换）。
///
/// # 为什么 pool 是 `&Mutex<Pool>` 而不是 `&mut Pool`
///
/// 轮转循环里有多次 `.await`（刷新 token、转发上游）。若持有 `&mut Pool`
/// 就必须把锁守卫跨越 `.await` 持有，后果有两条，都是硬伤：
///
/// 1. 守卫不是 `Send`，`chat_completions` 的 future 立刻不再是 `Send`，
///    axum 无法把它交给多线程运行时 —— 编译期直接失败；
/// 2. 即便编译得过，一次慢上游请求（可能数十秒）会独占整个账号池，
///    所有并发请求串行化，网关等于退化成单并发。
///
/// 因此这里改成「每次操作取一次锁」：锁只覆盖纯内存的状态变更（微秒级），
/// 网络等待期间**不持锁**。代价是选号与占用名额之间出现窗口，由
/// `Pool::acquire`（原子占名额）兜住 —— 它返回 false 时换号重试即可。
pub struct ForwardCtx<'a> {
    /// 账号池（每次操作短暂加锁，不跨 `.await` 持有）。
    pub pool: &'a Mutex<Pool>,
    /// 上游客户端。
    pub client: &'a Client,
    /// 会话粘性路由（`None` = 不启用粘性）。
    pub session: Option<&'a Router>,
    /// 单请求最多换号次数。
    pub max_rotate: usize,
    /// 429 / 404 软冷却时长。
    pub soft_cooldown: Duration,
    /// token 提前刷新窗口。
    pub refresh_skew: Duration,
    /// 「限制使用的模型」白名单；非空时只放行名单内的模型。
    ///
    /// 由 [`crate::config::Config::normalize`] 归一化后传入（去空白、剥前缀、
    /// 丢空项），请求路径上只做大小写不敏感比较。
    pub allowed_model: Vec<String>,
    /// ★ 产品上游集合（Qoder / ZCode）。默认空 = 所有产品都按 WorkBuddy 走
    /// （等价于引入多产品之前的行为）。
    pub products: &'a crate::products::ProductUpstreams,
    /// ★ 3012 熔断（按产品隔离）。进程级共享，故由调用方持有并传引用。
    pub unusual: &'a crate::unusual::UnusualBreakers,
    /// ★ 出口 IP 被 WAF 拦的判定窗口（多个不同账号吃 403）。进程级共享。
    pub waf_ip: &'a crate::wafip::WafIpWindow,
    /// ★ 上游整体空流的风暴判定窗口（多个不同账号同通道空流）。进程级共享。
    pub empty_stream: &'a crate::emptystream::EmptyStreamWindow,
    /// 流式请求预读首帧的超时。
    ///
    /// ★ 优先取配置里的 `header_timeout_seconds`（那是用户对「等上游开口」的
    /// **显式表态**），未配置时回落 30s。两者语义相同，故复用同一个配置键而不是
    /// 新加一个 —— 多一个键会让用户面对两个含义几乎一样的数字而不知道该调哪个。
    ///
    /// 为什么不能用硬编码的 30s（Go `forward.go:40-49` 现场）：用户配置里写了
    /// `header_timeout_seconds = 120`，而思考 40 秒才开始输出的模型会被判
    /// 「上游返回空流」→ 换号 → 换过去还是同一个模型、同样超时 → 整轮失败。
    /// 用户看到 `empty upstream stream`，而**上游其实完全正常**，只是思考久了点。
    /// ★ 两区模型缓存（含上游声明的 `credit_multiplier`）。
    ///
    /// 只用于**成本调度**（见 `inject_cost_rates`）：每次选号前把
    /// 「账号 × 当前模型」的倍率灌进池子。`None` = 不启用（单测默认）。
    ///
    /// ★ 只读快照，**绝不**在这里触发网络拉取 —— 本字段在每次 chat 请求的
    /// 关键路径上，见 `inject_cost_rates` 的 doc。
    pub models_cache: Option<&'a crate::models_cache::ModelsCache>,
    /// ★★ 「这次请求是不是**预算超限**」的估算（2026-10-09 业主现场）。
    ///
    /// 上游把「输入 + `max_tokens` > 1,048,576」也报成 `11133 model_param_invalid`，
    /// 而 DSH 按 `输入 ≤ 1,048,576` 估算 ⇒ 它以为还有余量、**不压缩** ⇒ 永久卡死。
    /// 由调用方在**发请求前**算好（那里才知道模型真值与账号区域），这里只读。
    ///
    /// `None` = 算不出（未启用 / 模型未知）⇒ 逐字退回旧行为（不猜）。
    pub overflow_guess: Option<bool>,
    pub probe_timeout: Duration,
}

/// 上游的上下文预算上限（**输入 + `max_tokens`**）。
///
/// 实测（2026-10-09，真上游逐点二分）：`1,042,398 + 16 = 1,042,414` 通过；
/// `1,042,398 + 8,192 = 1,050,590` 被拒（400 code=11133）。⇒ 上限 = `1 << 20`。
///
/// ⚠ 与模型声明的 `context_length`（1,000,000）**不是**同一个数：那个是模型窗口，
/// 这个是网关→上游这条通道的请求预算，后者更宽（多 48,576）。判据用**后者**，
/// 因为上游按后者卡；用前者会让 1,000,000..1,048,576 这一段被误判成超限。
pub const UPSTREAM_REQUEST_TOKEN_BUDGET: i64 = 1 << 20;

/// 估算请求体里的 **token 数**（不精确，只用于「是否逼近上限」的二值判断）。
///
/// 按字符类型分档，实测校准（真上游 `prompt_tokens` 回读）：
///
/// | 内容 | 实测 |
/// |---|---|
/// | CJK | **≈ 2 字符 / token** |
/// | 其它（ASCII 等） | ≈ 3.6 字符 / token |
///
/// 为什么取**偏大**的一侧：本估算只用来判断「是不是撞了上限」。高估会在
/// 真正超限时更早命中（要的就是这个）；代价是极少数**接近**上限但没超的请求
/// 会被当成溢出 —— 而那种请求本来就该压缩了，压缩一次无害。反向（低估）
/// 才是致命的：会让客户端继续发超限请求，就是本 bug 本身。
fn estimate_tokens(bytes: &[u8]) -> i64 {
    let mut cjk: i64 = 0;
    let mut other: i64 = 0;
    let s = String::from_utf8_lossy(bytes);
    for ch in s.chars() {
        let c = ch as u32;
        // CJK 统一表意文字 + 扩展 A + 兼容表意 + 全角标点/假名/谚文。
        let is_cjk = (0x3040..=0x30FF).contains(&c)      // 假名
            || (0x3400..=0x4DBF).contains(&c)             // 扩展 A
            || (0x4E00..=0x9FFF).contains(&c)             // 基本区
            || (0xF900..=0xFAFF).contains(&c)             // 兼容表意
            || (0xFF00..=0xFFEF).contains(&c)             // 全角
            || (0xAC00..=0xD7AF).contains(&c);            // 谚文
        if is_cjk {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    // 向上取整：宁可高估。
    (cjk + 1) / 2 + (other + 3) / 4
}

/// `body` 这次请求**是否会撞上上游的上下文预算**。
///
/// 返回 `None` = 算不出（不该据此改判），`Some(true)` = 会撞。
///
/// # 判据
///
/// `估算输入 + max_tokens > 上限`。
///
/// ★ 必须把 `max_tokens` 计入：业主现场正是「输入 1,042,398 已经贴着上限，
/// 而 DSH 又带 `max_tokens: 128000`」，两者相加 1,170,398 远超 1,048,576 ——
/// DSH 只按输入估算，于是**以为还有余量、不压缩**，永久卡死。
///
/// ★ 只有**输入本身**已经超过上限的 60% 才算 —— 小请求即使 `max_tokens` 很大也
/// 不该判成溢出（那些请求从没失败过）。这个门槛把误判面压到「本来就该压缩」
/// 的区间，同时不影响业主现场（输入 1,042,398 远超 60%）。
///
/// ★★ **例外：`max_tokens` 自己就超过上限**（2026-10-09 业主 VS Code 现场）。
///
/// 业主在 VS Code 里把上下文设置调得很大就稳定复现 11133，调小就好了。
/// 那种请求的形状是「**输入很小 + `max_tokens` 极大**」（他调的就是那个设置）。
/// 此时上面那道 60% 门槛会把请求放行 ⇒ 判据返回 `Some(false)` ⇒ 归
/// `model_param_invalid` ⇒ 客户端**不知道**这是预算问题，只能看到一句
/// 「参数被拒」而反复重试。
///
/// 单独看 `max_tokens` 就够了：它自己都超过上限时，无论输入多小，
/// `输入 + max_tokens` 必然超限 —— 不需要估算输入，也就没有误判风险。
/// 实测（本机 rig，假上游固定回 11133）：输入 ~5,000 + `max_tokens` 1,048,576
/// ⇒ 命中本分支。
///
/// `model` 未在清单里 ⇒ `None`（不猜）。
pub fn budget_exceeded_guess(
    body: &[u8],
    model: &str,
    cache: &crate::models_cache::ModelsCache,
) -> Option<bool> {
    // 模型必须在真值清单里 —— 连它多大都不知道就不该下结论。
    let known = {
        let snap = cache.snapshot();
        snap.values().flatten().flatten().any(|mi| {
            !mi.id.is_empty() && mi.id.trim().eq_ignore_ascii_case(model.trim())
        })
    };
    if !known {
        return None;
    }
    let max_tokens = max_tokens_of(body).unwrap_or(0);
    let input = estimate_tokens(body);
    // ★★ 直接用**实测公式**，不再加「输入须 > 60%」的前置门槛。
    //
    // 旧实现有一道 `input < 上限 * 6/10 ⇒ Some(false)` 的闸门，理由是「小请求从没
    // 失败过」。但业主 VS Code 现场恰恰是**小输入 + 极大 max_tokens**（他调的就是
    // 那个上下文/输出设置）：`输入 5,000 + max_tokens 1,048,576 = 1,053,576 > 上限`
    // —— 公式说超了，闸门却因为「输入太小」把它放行 ⇒ 返回 `Some(false)` ⇒
    // 客户端看不懂真因。**闸门挡掉的正是它该抓的那类**。
    //
    // 为什么可以直接用公式：
    //   · 上限本身就是实测出来的（见本节开头的实测表），公式即上游规则；
    //   · 输入估计的误差是**绝对量**（CJK 2 字符/token、ASCII 3.6~4 字符/token），
    //     小输入时误差极小 ⇒ 小请求上的判断反而更可靠；
    //   · 两个方向的代价不对称：误判成溢出最多让客户端多压缩一次（无害）；
    //     漏判则让客户端**永远**重发超限请求（就是本 bug 本身）。
    //
    // 边界：判据是**严格大于**（`正好等于上限`的成功，见实测 `1,042,398 + 16`）。
    Some(input + max_tokens > UPSTREAM_REQUEST_TOKEN_BUDGET)
}

/// 取请求体里的 `max_tokens` / `maxTokens`（缺失或非数字 ⇒ `None`）。
///
/// 缺失按 0 计：那种请求**确实**没有为输出预留预算，不该因为我们不知道就判超限。
fn max_tokens_of(body: &[u8]) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    for k in ["max_tokens", "maxTokens", "max_completion_tokens"] {
        if let Some(n) = v.get(k).and_then(|x| x.as_i64()) {
            return Some(n);
        }
    }
    None
}

/// 取池锁（中毒时取回内部值：状态是纯内存数据，没有「不一致」风险）。
fn lock_pool(pool: &Mutex<Pool>) -> std::sync::MutexGuard<'_, Pool> {
    match pool.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// 带图片的请求该如何选号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageRoute {
    /// 应使用的区域；`Region::Any` = 不做区域约束。
    pub region: Region,
    /// `true` 时**只**在该区域选号，选不出就报错，绝不跨区降级。
    pub required: bool,
}

impl ImageRoute {
    /// 不约束区域的默认路由。
    pub fn any() -> Self {
        Self {
            region: Region::Any,
            required: false,
        }
    }
}

/// 把一个「账号 × 当前模型」的成本率灌进池子（Go `forward.go:1507-1565` 逐字移植）。
///
/// # 为什么需要它（2026-09-23 所有者需求）
///
/// 所有者原话：「模型路由不但要考虑积分到期,积分总量,还要考虑 实际消耗积分,这些因素
/// 比如 deepseek-v4.1-flash 在国际版是free 0倍率,国内是0.03 … 这时候就应该优先路由
/// 国际账号,因为他是free 其次才是积分快到期的账号」
///
/// 池里的成本维度（`set_cost_rate_known` / `cost_is_free` / `free_tier` /
/// `cost_multiplier`）**早就写好了，但一直没有任何生产代码往它塞值** —— 实测全仓库
/// `set_cost_rate` 只在单测里被调用过。于是 `cost_rate` 恒为 0、乘子恒为 1.0，
/// 成本维度**完全没生效**。本函数就是那次「接线」。
///
/// # 数据来源：只读缓存，绝不触发拉取
///
/// 倍率来自 `ModelsCache::snapshot`（由模型拉取填充，带 1h TTL）。
/// **这里绝不能去发网络请求** —— 本函数在**每次 chat 请求的关键路径**上：
/// 冷启动后的第一个请求会白等一次模型拉取，而用户等的是回答。
/// 缓存空 = 还没有倍率数据 ⇒ 全部 `known=false` ⇒ 中性，行为与本特性引入之前逐字相同。
///
/// # 三态：为什么必须传 known
///
/// 上游 `credit_multiplier` 的 **0 是「免费」而不是「未声明」**，而池里的 0 历史上
/// 表示「未声明」。直接接线的话免费账号会被当成未声明，**「免费优先」会静默失效**
/// （看起来在工作，实际一次都没生效）。故这里按区域查表，查到就 `known=true`
/// （哪怕值是 0），查不到才 `known=false`。
///
/// # 为什么按「账号所在区域」而不是「请求指定区域」
///
/// 倍率是**上游按区域声明的**（同名模型两区计费不同）。账号属于某个区域，它在哪个区
/// 就按哪个区的倍率算 —— 这正是「国际版免费、国服计费」能被区分的原因。
///
/// # 为什么用 `eq_ignore_ascii_case` 而不是建哈希表
///
/// 模型名大小写在上游与客户端之间不一致（实测 `DeepSeek-V4.1-Flash` 与
/// `deepseek-v4.1-flash` 都出现过），而两区模型表各只有几十条 —— 一次线性扫描
/// 既省掉一次分配，又天然做到大小写不敏感（Go 侧自己 `ToLower` 归一，这里更直接）。
///
/// # 位置：必须在选号**之前**、循环**之外**
///
/// 倍率只取决于 (账号区域, 模型名)，与第几次换号无关，每次重试都重算是纯浪费。
/// `tried` 里的账号也照常注入 —— 它们是「本次已试过」，不是「不该有成本」，
/// 注入它们不影响选号（pick 会按 `tried` 排除），但能避免多一条特例分支。
fn inject_cost_rates(ctx: &ForwardCtx<'_>, model: &str) {
    let Some(cache) = ctx.models_cache else {
        return;
    };
    if model.is_empty() {
        return;
    }
    let fetched = cache.snapshot();
    let mut pool = lock_pool(ctx.pool);
    // 先清空上一轮的残留：costRate 是**按当前模型**算的运行态，不复用会让
    // 「上一个请求的模型倍率」渗进这一个请求。
    pool.clear_cost_rates();
    for acct in pool.account_keys() {
        let Some(Some(infos)) = fetched.get(&acct.region) else {
            continue;
        };
        // `None` = 上游**未声明**该字段（不是免费）⇒ 找不到 ⇒ known=false。
        let hit = infos.iter().find(|mi| {
            !mi.id.is_empty()
                && mi.id.trim().eq_ignore_ascii_case(model.trim())
                && mi.credit_multiplier.is_some()
        });
        if let Some(mi) = hit {
            // ⚠ 传 known=true：**哪怕 rate 是 0**（0 = 免费，是已声明的一种）。
            // 这正是「免费优先」能否生效的关键。
            let rate = mi.credit_multiplier.unwrap_or(0.0);
            pool.set_cost_rate_known(&acct.uid, rate, true);
        }
    }
}

/// 执行「选号 → 刷新 token → 转发 → 失败换号」的完整轮转。
///
/// - `body`：已转换成 OpenAI Chat 形态的请求体，且**已剥掉产品/区域前缀**
///   （由 [`crate::resolve_model::resolve_model`] + [`rewrite_model`] 在
///   `server::run_forward` 里完成；原始字节，发往上游前由
///   `Client::prepare_body` 再做一次改写：强制 stream、归一化 role/tool_choice）
/// - `stream`：调用方是否要求流式。上游恒为流式，非流式时本函数读完后聚合。
/// - `sess_key`：会话粘性键；空串表示不做粘性绑定。
/// - `route`：区域路由约束（带图片请求 + 显式区域前缀）。
/// - `product`：客户端前缀指定的产品；空串 = 不限制。
///
/// 返回 `(结果, 状态码, 失败)`；成功时失败为 `None`。
pub async fn forward_chat(
    ctx: &mut ForwardCtx<'_>,
    body: &[u8],
    stream: bool,
    sess_key: &str,
    route: ImageRoute,
    product: &str,
) -> (Option<ChatResult>, u16, Option<ForwardFailure>) {
    let mut tried: Vec<String> = Vec::new();
    let mut last_uid = String::new();
    // 最后尝试过的账号的产品/区域（互斥，见 `product_labels`）。循环耗尽时
    // 与 `last_uid` 一起回填，让上层能报出「最后打的是谁、哪个产品」。
    let mut last_labels: (String, String) = (String::new(), String::new());
    let mut last_status: u16 = 503;
    let mut last_err: Option<String> = None;
    let mut last_kind = ErrKind::None;
    let mut last_body = String::new();
    let mut last_transport_err: Option<String> = None;

    // 请求的目标模型：用于模型级冷却的选号过滤与记账。
    let model = model_of(body);

    // 「限制使用的模型」白名单：非空时只放行名单内的模型。
    //
    // 为什么在选号之前就拒绝（而不是换个模型重试）：这份名单是**策略**的一部分
    //（轮转模式下尤其如此 —— 语义是「把这个账号的指定模型额度烧干净再换号」）。
    // 若允许其他模型通过，客户端换个模型就能绕过轮转与额度控制，也让「当前烧的
    // 是哪个模型」变得不可预期 —— 因此明确拒绝并说明原因，比静默改写模型
    //（用户以为在用 A、实际用了 B）更安全。
    //
    // 比较用的是剥前缀后的**裸模型名**（[`crate::model_allow::model_allowed`]
    // 内部会再兜一次 `resolve_model`）：用户配的是 `deepseek-v4.1-flash`，
    // 客户端可能带 `cn:` / `qoder:` 前缀请求，两者应视为同一个模型。
    // 大小写不敏感 —— 客户端写法并不统一。名单为空 = 不限制（默认，向后兼容）。
    if !crate::model_allow::model_allowed(&ctx.allowed_model, &model) {
        // 返回非 nil 的 result：调用方会在错误分支里读 `result.uid` 记日志，
        // 返回 nil 会 panic（Go `forward.go:312-313` 逐字记着这条）。
        // uid 留空即可（本次没有选中任何账号）。
        return (
            Some(ChatResult::Response {
                uid: String::new(),
                model: model.clone(),
                // 还没选号就失败 ⇒ 没有产品/区域可言（不是「未知」，是「不适用」）。
                product: String::new(),
                region: String::new(),
                body: serde_json::Map::new(),
            }),
            400,
            Some(ForwardFailure {
                // ★ 走 [`FailureKind::ModelNotAllowed`] 而不是 `Upstream`：
                // 前者映射到 `model_not_allowed` 错误码，后者会被客户端读成
                // 「账号池暂时不可用，稍后重试」—— 而这里等多久都不会变。
                kind: FailureKind::ModelNotAllowed,
                status: 400,
                message: crate::model_allow::model_locked_message(&model, &ctx.allowed_model),
            }),
        );
    }

    let mut sticky_uid = if sess_key.is_empty() {
        None
    } else {
        ctx.session.and_then(|s| s.resolve(sess_key))
    };

    // 在途租约：成功选中即占名额。每条 `continue` 路径都先显式 release，
    // 因此循环正常结束时必然为空；`release` 仍是幂等的（take 语义）。
    let mut held_uid: Option<String> = None;

    // 「请求被拒」的**原地重试**预算（跨整个循环共享，不按账号重置）。
    //
    // 为什么必须重试：实测 11140（安全审核）在同一请求体上按 ~10~25% 概率
    // **随机**出现 —— 同一份 body 连发 40 次，首轮 75% 成功，对失败的原请求
    // 重发最多 2 次后成功率 100%（10/10 全部救回，0 次重试仍失败）。
    // 上游的审核判定显然带服务端抖动，而原实现把它当「请求侧错误、换号无用」
    // 直接短路返回 —— 等于把上游抖动原样透传给用户，表现就是「agent 经常报错」。
    //
    // 11155（思维链缺失）同样纳入重试：它多数是请求侧真问题（重试会再失败），
    // 但实测也存在与 11140 同样的偶发抖动，重试成本仅一次上游调用，
    // 而漏判的代价是用户整个任务失败。
    const REJECT_RETRY_BUDGET: usize = 2;
    let mut reject_retries_left = REJECT_RETRY_BUDGET;

    // 重试与换号**共用同一个循环**，因此循环上界必须是两者之和 ——
    // 否则重试会把换号预算吃光（max_rotate 缺省才 3），
    // 表现为「账号一抖动就没得换了」。

    // ⚠⚠ **必须在选号之前**把成本率灌进池子（对齐 Go `forward.go:386`）。
    // 放在循环**外面**而不是里面：倍率只取决于 (账号区域, 模型名)，与第几次
    // 换号无关，每次重试都重算是纯浪费。
    inject_cost_rates(ctx, &model);

    // ★ 平台总开关：**最前置**的一道闸，早于任何选号动作。
    //
    // 语义（业主原话）：「关闭平台只是不参与兼容网关，并不是不执行任务」——
    // 关掉的平台，它的账号不参与选号；签到/上报/成长任务/额度巡检照常跑。
    //
    // 为什么必须放在**选号之前**（实测踩过的坑）：这条判据最初只写在下面
    // 「选不出号」分支里的白名单闸内，而带产品前缀的请求走
    // `pick_for_model_product_region`，当时选号侧没有平台判据 ⇒
    // 实测关掉 qoder 后 `qoder:国服:GLM-5.3` 依然返回 200（真上游内容），
    // 开关形同虚设。现在选号侧已补齐（`pool.rs` 落点 D/E），这里再前置一道，
    // 让「平台已关闭」永远优先于「这个区域没号」「账号全挂了」等其它失败原因
    //（出路完全不同：一个去账号页打开开关，其余去查账号）。
    if !product.is_empty() && !lock_pool(ctx.pool).platform_enabled(product) {
        return (
            Some(ChatResult::Response {
                uid: String::new(),
                model: model.clone(),
                // 请求带产品前缀，这里填产品有意义（用户是显式指定了它才走到这）。
                product: product.to_string(),
                region: String::new(),
                body: serde_json::Map::new(),
            }),
            400,
            Some(ForwardFailure {
                kind: FailureKind::PlatformNotInRouting,
                status: 400,
                message: platform_disabled_message(product),
            }),
        );
    }

    for _ in 0..(ctx.max_rotate.max(1) + REJECT_RETRY_BUDGET) {
        // ── 选号：优先粘性命中，其次按模型 + 区域挑（短暂持锁）──
        let mut acct: Option<Auth> = None;
        if let Some(uid) = sticky_uid.clone() {
            // ⚠ 粘性路径**也必须**校验产品：选号顺序是「先试会话绑定的账号，
            // 绑不上才走产品过滤」，若产品过滤只加在后一支，那么**只要该会话
            // 此前绑过一个账号**，`zcode:glm-5.3` 就会继续用那个绑定账号 ——
            // 哪怕它是 WorkBuddy 的（实测确认过这个漏洞：`zcode:glm-5.3`
            // 选中了一个 workbuddy 账号，然后报「额度已耗尽」）。
            acct = lock_pool(ctx.pool).pick_by_uid_for_model_product_region(
                &uid,
                &model,
                route.region,
                product,
            );
            if acct.is_none() {
                // 绑定号已冷却/占满/模型限流/产品不符/**成本更贵** → 解绑，落入普通选号。
                //
                // ★ 成本那一条是 2026-10-08 加的：会话绑到计费号后，30 分钟 TTL 内
                // 全部请求都走它，`free_tier` 形同不存在（业主现场：15 个国服号被均匀
                // 摊了 710 条）。解绑后主路径会用完整的 `pick_inner` 重新选。
                if let Some(s) = ctx.session {
                    s.unbind(sess_key);
                }
                sticky_uid = None;
            }
        }
        if acct.is_none() {
            let mut pool = lock_pool(ctx.pool);
            // 产品前缀是**强制**约束（不跨平台兜底），区域是软偏好 —— 两者
            // 由 pool 内部两层分别处理，见 `pick_for_model_product_region`。
            acct = if product.is_empty() {
                if route.region == Region::Any {
                    pool.pick_for_model(&model, &tried)
                } else {
                    pool.pick_for_model_region(&model, &tried, route.region, route.required)
                }
            } else {
                pool.pick_for_model_product_region(&model, &tried, route.region, product)
            };
        }

        let Some(acct) = acct else {
            // 区域受限且选不出号：**不降级**，明确告诉用户缺哪个区域的账号。
            // 静默跨区会让图片被后端换成占位符，模型回「我看不见图片」——
            // 用户完全无从判断是网络、模型还是网关的问题。
            if route.required && route.region != Region::Any {
                return (
                    Some(ChatResult::Response {
                        uid: String::new(),
                        model: model.clone(),
                        // 选不出号 ⇒ 没有产品/区域（区域是**要求**的那个，不是实际用的）。
                        product: String::new(),
                        region: String::new(),
                        body: serde_json::Map::new(),
                    }),
                    503,
                    Some(ForwardFailure {
                        kind: FailureKind::ImageRegionUnavailable,
                        status: 503,
                        message: image_region_unavailable_message(route.region),
                    }),
                );
            }
            last_status = 503;

            // 「账号全被设为不参与选号」：与「账号全挂了」分开报，出路完全不同。
            //
            // 现场：所有者发 `intl:deepseek-v4.1-flash` 得到
            // `503 {"code":"no_healthy_account","message":"all accounts unavailable
            // (cooling/disabled)"}`，但 5 个国际版账号实测 cooling=false
            // disabled=false —— **它们健康得很**，真实原因是这些账号被设了
            // `no_route`。后果很具体：他会去等「冷却恢复」，而实际只要在界面上
            // 把路由开关打开。
            //
            // ⚠ 判据必须是 `no_route == total && total > 0`，**不是** `no_route > 0`：
            // 第一版写成 `> 0` 时，Qoder 有 2 个账号（1 个 no_route、1 个可用），
            // 请求选中那个可用的 → 它自己失败 → 轮转耗尽 → 文案却说「1 个账号都被
            // 设为 no_route」，把用户引向完全错误的排查方向。
            let (no_route, total) = lock_pool(ctx.pool).no_route_excludes_all(
                product,
                &model,
                route.region,
            );
            if total > 0 && no_route == total {
                return (
                    Some(ChatResult::Response {
                        uid: String::new(),
                        model: model.clone(),
                        product: String::new(),
                        region: String::new(),
                        body: serde_json::Map::new(),
                    }),
                    503,
                    Some(ForwardFailure {
                        kind: FailureKind::AllNoRoute,
                        status: 503,
                        message: all_no_route_message(total),
                    }),
                );
            }

            // 「用户白名单不允许这个平台跑这个模型」：**400** 而非 503。
            //
            // 503 的语义是「稍后重试就好」，而这里**等多久都不会出现** ——
            // 要么去白名单放行，要么换个模型。判据是**用户白名单**而不是上游清单：
            // qoder 上游实测 10/10 全 200 能服务 deepseek-v4.1-flash，但
            // 「上游能服务」≠「用户允许」。所有者的要求两句缺一不可：
            // 「在选号侧就是要挡请求」+「给每个平台手动配置支持的模型」。
            if !product.is_empty() {
                // ⚠ 两次取锁必须分开：`std::sync::Mutex` 不可重入，
                // 同一表达式里取两次会自锁死。
                // ⚠ 平台总开关要**单独**取一次，不能只看 `model_allowed_for_product`：
                // 白名单为空时那个函数返回 true（「没配就不拦」），而平台被关掉
                // 恰恰是「用户明确表态了」—— 必须走在白名单判定之前。
                let platform_off = {
                    let pool = lock_pool(ctx.pool);
                    !pool.platform_enabled(product)
                };
                let allowed = lock_pool(ctx.pool).platform_allowed_models(product, route.region);
                let ok = {
                    let pool = lock_pool(ctx.pool);
                    model_allowed_for_product(&pool, product, route, &model)
                };
                if !ok || platform_off {
                    return (
                        Some(ChatResult::Response {
                            uid: String::new(),
                            model: model.clone(),
                            // 用户白名单拒绝：请求**带产品前缀**，所以这里填产品有意义
                            //（用户是显式指定了 qoder/zcode 才走到这条分支的）。
                            product: product.to_string(),
                            region: String::new(),
                            body: serde_json::Map::new(),
                        }),
                        400,
                        Some(ForwardFailure {
                            kind: if platform_off {
                                FailureKind::PlatformNotInRouting
                            } else {
                                FailureKind::ModelNotInProduct
                            },
                            status: 400,
                            // ⚠ 两条文案的**出路相反**，绝不能互换：平台关闭要去
                            // 账号页打开开关，白名单拒绝要去「兼容网关」页加模型。
                            // 见 `platform_disabled_message` 的文档。
                            message: if platform_off {
                                platform_disabled_message(product)
                            } else {
                                model_not_in_product_message(
                                    product,
                                    route.region,
                                    &model,
                                    &allowed,
                                )
                            },
                        }),
                    );
                }
            }
            break;
        };

        tried.push(acct.uid.clone());
        last_uid = acct.uid.clone();

        // 原子占名额：失败说明刚被别的请求抢走，换号重试。
        if !lock_pool(ctx.pool).acquire(&acct.uid) {
            if sticky_uid.as_deref() == Some(acct.uid.as_str()) {
                if let Some(s) = ctx.session {
                    s.unbind(sess_key);
                }
                sticky_uid = None;
            }
            continue;
        }
        held_uid = Some(acct.uid.clone());

        // ── 3012 熔断兜底：**发请求之前**先看上游是否正在风控 ──
        //
        // 放在这里才真正止住放大（Go `forward.go:495-532` 逐字）：一次请求内
        // 第一个账号吃到 3012 后，循环走到下一轮再来这里 —— 熔断已置位，于是
        // 直接返回，**不再拿后面的账号去撞**。
        //
        // ★★ 这里**不**区分是否第一轮（Go 那条注释逐字）：写「第一轮跳过」听起来
        // 合理（「不该被上一轮留下的熔断挡住」），实际却让**熔断期间每个新请求仍会
        // 打一次上游** —— 用户每重试一次就再撞一次风控，而所有者要的正是「别重试」。
        // Go 的 `TestUnusualActivityBreakerBlocksNextRequest` 直接抓到了这个（1 → 2 次）。
        //
        // 熔断期就是要**一个请求都不发**；用户看到的是一条说清原因（与账号无关、
        // 多久恢复）的错误，而不是一次注定失败的尝试。
        //
        // 按**选中账号所属产品**隔离：ZCode 的风控不能阻断 Qoder/WorkBuddy；
        // 同产品内仍共享，避免换号继续放大同一上游风控。
        {
            let breaker = ctx.unusual.for_product(acct.product_of());
            if let Some(wait) = breaker.tripped() {
                crate::logline!(
                    "chat: {} 3012 熔断中（还需 {:?}），不发请求（本请求已试 {} 个账号）",
                    acct.product_of(),
                    wait,
                    tried.len()
                );
                release(ctx.pool, &mut held_uid);
                return (
                    Some(ChatResult::Response {
                        uid: last_uid.clone(),
                        model: model.clone(),
                        // 熔断在**发请求之前**命中 ⇒ 这个账号没被真正使用，
                        // 报产品有意义（熔断是按产品隔离的），区域留给 WorkBuddy 路径。
                        product: last_labels.0.clone(),
                        region: last_labels.1.clone(),
                        body: serde_json::Map::new(),
                    }),
                    503,
                    Some(ForwardFailure {
                        kind: FailureKind::UnusualActivity,
                        status: 503,
                        message: crate::unusual::unusual_activity_message(wait),
                    }),
                );
            }
        }

        //
        // ── 上游整体空流的风暴熔断（2026-10-11 现场：13 个国际区账号被空流轮一遍）──
        //
        // 判定与 3012 同款哲学：熔断期就是要一个请求都不发。窗口内同一（产品, 区域）
        // 已有多个不同账号吃到空流 ⇒ 换号无用（是上游通道整体故障，账号是好的），
        // 再换号只会把剩下的好账号也拖进冷却。判定按 (产品, 区域) 隔离。
        {
            let (burst, distinct) = ctx
                .empty_stream
                .blocked(acct.product_of(), acct.region().as_str());
            if burst {
                crate::logline!(
                    "chat: 空流风暴熔断中（最近 {} 秒内 {} 个不同账号空流），不发请求（本请求已试 {} 个账号）",
                    ctx.empty_stream.window().as_secs(),
                    distinct,
                    tried.len()
                );
                release(ctx.pool, &mut held_uid);
                return (
                    Some(ChatResult::Response {
                        uid: last_uid.clone(),
                        model: model.clone(),
                        // 熔断在**发请求之前**命中 ⇒ 这个账号没被真正使用，
                        // 报产品有意义（判定按产品+区域隔离），区域留给 WorkBuddy 路径。
                        product: last_labels.0.clone(),
                        region: last_labels.1.clone(),
                        body: serde_json::Map::new(),
                    }),
                    503,
                    Some(ForwardFailure {
                        kind: FailureKind::EmptyStreamBurst,
                        status: 503,
                        message: crate::emptystream::empty_stream_burst_message(
                            distinct,
                            ctx.empty_stream.window(),
                        ),
                    }),
                );
            }
        }
        // ── 产品派发判定（对应 Go `forward.go:565-572` 的唯一分叉点）──
        //
        // ★★ 这里必须用 `is_product_account`（只看账号的产品标签），**不能**用
        // `products.dispatch(&acct).is_some()`。理由：一个 Qoder 账号即使产品上游
        // 没配置，也**绝不该**去调 WorkBuddy 的刷新接口 —— 那是别人的端点，调了
        // 必然失败并把这个好账号冷却掉。对应 Go 的 `isProduct` 变量。
        let is_product = crate::products::ProductUpstreams::is_product_account(&acct);
        // 兜底文案路径（循环耗尽）也要能报出「最后试的是哪个产品」。
        last_labels = product_labels(&acct, is_product);

        // ── token 临近过期 → 先刷新（失败则冷却换号）──
        //
        // ★★ 刷新**只对 WorkBuddy 做**（Go `forward.go:574-611` 逐字）。两条理由：
        //
        // 1. 其它产品**没有 WorkBuddy 的刷新接口**。调 `Client::refresh_token` 会
        //    失败并把账号冷却掉，请求永远走不到派发那一步。实测踩到：ZCode 账号
        //    没有 `expiresAt`（凭证长期有效），`needs_refresh` 恒为 true ⇒ 每次
        //    请求都在这里失败退出。
        // 2. 各产品的刷新方式完全不同（Qoder 是 `deviceToken/refresh` + drt 轮换），
        //    刷新必须由**产品自己**负责（在 `chat_stream` 内部按需刷新）。
        //
        // 注意：刷新是网络调用，**不持锁**；仅在其失败时短暂加锁记状态。
        let mut acct = acct;
        if !is_product && acct.needs_refresh(ctx.refresh_skew) {
            if let Err(e) = ctx.client.refresh_token(&mut acct).await {
                let (kind, transport) = classify_refresh_err(&e);
                last_err = Some(e.to_string());
                last_kind = kind;
                last_transport_err = transport;
                last_body.clear();
                {
                    let mut pool = lock_pool(ctx.pool);
                    if kind == ErrKind::SessionDead {
                        pool.disable(&acct.uid, "refresh session dead");
                    } else {
                        pool.note_error(&acct.uid);
                    }
                }
                release(ctx.pool, &mut held_uid);
                continue;
            }
            if let Err(e) = acct.save_atomic() {
                crate::logline!("chat refresh uid={}: save auth failed: {e}", acct.uid);
            }
        }

        // ── 转发上游（网络等待，不持锁）──
        //
        // 按产品选上游：非 WorkBuddy 走 [`crate::products::ProductUpstream`]，
        // 其 `chat_stream` 返回的流**已经翻译成标准 OpenAI SSE**，故下面的
        // `match outcome` 与三个协议消费者都**不需要知道产品差异**。
        let product_up = ctx.products.dispatch(&acct);
        let outcome = {
            let r = match product_up {
                Some(up) => up.chat_stream(&acct, body).await,
                None => ctx.client.chat_stream(&acct, body).await,
            };
            match r {
                Ok(o) => o,
                Err(e) => {
                    // ★★ 三条「请求侧」错误必须先于通用传输层分支处理（Go
                    // `forward.go:632-680` 的 `errors.As` 顺序逐字）：它们都要求
                    // **不轮转、不罚账号**，落进下面的通用分支就会变成「换个号
                    // 再试一遍」—— 把上游的一次性要求放大成「账号全部不可用」。
                    if let Some((fk, status, message)) = product_request_side_failure(&e, &acct, &model)
                    {
                        let (product, region) = product_labels(&acct, is_product);
                        release(ctx.pool, &mut held_uid);
                        return (
                            Some(ChatResult::Response {
                                uid: acct.uid.clone(),
                                model: model.clone(),
                                product,
                                region,
                                body: serde_json::Map::new(),
                            }),
                            status,
                            Some(ForwardFailure {
                                kind: fk,
                                status,
                                message,
                            }),
                        );
                    }
                    // 额度耗尽（ZCode 套餐过期）：账号级失败 → 冷却该账号，
                    // 返回 402，而不是等「上游 200 却一直空流」到超时。
                    if e.is_quota_exhausted() {
                        let msg = e.to_string();
                        {
                            let mut pool = lock_pool(ctx.pool);
                            apply_error_policy(
                                &mut pool,
                                &acct.uid,
                                &model,
                                ErrKind::HardCredit,
                                &msg,
                                ctx.soft_cooldown,
                            );
                        }
                        // ★ 这里**不要**再写 `last_status/last_err/last_kind/
                        // last_transport_err`：本分支下面就是无条件 `return`，
                        // 那几个变量是给循环结束后的错误文案（`friendly_message`）
                        // 用的，写了也不会被读到。曾经的写法留下了 4 条
                        // `value assigned to … is never read` 警告，反而让人以为
                        // 「这里的失败会走通用错误路径」。
                        release(ctx.pool, &mut held_uid);
                        return (
                            None,
                            402,
                            Some(ForwardFailure {
                                kind: FailureKind::QuotaExhausted,
                                status: 402,
                                message: msg,
                            }),
                        );
                    }
                    // 传输层失败（超时/连接被拒/DNS）没有上游业务体，Classify 不适用；
                    // 单独标记，使末尾的错误文案能给出可读原因而不是原始底层报错。
                    last_status = 503;
                    last_err = Some(e.to_string());
                    last_kind = ErrKind::None;
                    last_body.clear();
                    last_transport_err = Some(e.to_string());
                    release(ctx.pool, &mut held_uid);
                    continue;
                }
            }
        };

        match outcome {
            ChatOutcome::Failed { status, body: resp_body } => {
                let kind = classify::classify(status, &resp_body);
                // ★★ ZCode 门控：**先过 ZCode 自己的分类器**，把 1113（429 +
                // 「余额不足或无可用资源包」）改判成 [`ErrKind::NoResourcePack`]。
                //
                // 为什么必须在这里改：通用 [`classify::classify`] 的 HARD_MARKERS
                // 里带「余额不足」，而 ZCode 的 1113 原文正是「余额不足或**无可用
                // 资源包**，请充值」—— 通用分类器会判成 [`HardCredit`] ⇒
                // [`cooldown_until_tomorrow_4am`] ⇒ 一个**一分额度没花**的账号
                // 被整号硬冷却到次日 04:00，界面显示「余额不足（积分欠费）」。
                // 实测：剩余 9,977 万 / 1 亿的满额账号就是这么被冻住的。
                //
                // 只在 ZCode 账号上做这层改写：WorkBuddy / Qoder 的「余额不足」
                // 仍然是**真的**没额度（`matches_go_testclassify_matrix` 里钉死）。
                let kind = zcode_gate(&acct, kind, status, &resp_body);
                last_status = status;
                last_kind = kind;
                last_body = resp_body.clone();
                last_transport_err = None;
                last_err = Some(
                    UpstreamError {
                        kind,
                        status,
                        msg: resp_body.clone(),
                    }
                    .to_string(),
                );

                // ── 3012「unusual activity」：请求级瞬时风控，按产品熔断 ──
                //
                // ★★ 必须早于下面的通用 `apply_error_policy`（Go `forward.go:689-716`）。
                //
                // 为什么：3012 的 HTTP 载体是 **405**，而 `classify` 里 405 落进通用
                // `Client` —— `apply_error_policy` 对 `Client` 是「只换号不罚」（防雪崩），
                // 于是网关会**把池里所有账号挨个打一遍**，每个都吃一次 3012。
                //
                // 对上游风控而言这正是最坏形状：**同一出口 IP 在极短时间内用一批不同
                // 凭证反复触发同一规则**。与 `wafip` 记的那条完全同构。
                //
                // ★ 刻意**不罚账号**：3012 与账号无关（已在官方客户端复现）。罚号会让
                // 好账号被冷却，用户解封后还会发现号被自己人停了。
                if crate::unusual::is_unusual_activity(status, &resp_body) {
                    let breaker = ctx.unusual.for_product(acct.product_of());
                    breaker.note3012();
                    let wait = breaker.tripped().unwrap_or_default();
                    crate::logline!(
                        "chat uid={} product={}: 上游 3012 unusual activity，已熔断至 {:?}\
                         （不再换号重试）",
                        acct.uid,
                        acct.product_of(),
                        wait
                    );
                    let uid = acct.uid.clone();
                    let (product, region) = product_labels(&acct, is_product);
                    release(ctx.pool, &mut held_uid);
                    return (
                        Some(ChatResult::Response {
                            uid,
                            model: model.clone(),
                            product,
                            region,
                            body: serde_json::Map::new(),
                        }),
                        503,
                        Some(ForwardFailure {
                            kind: FailureKind::UnusualActivity,
                            status: 503,
                            message: crate::unusual::unusual_activity_message(wait),
                        }),
                    );
                }

                // ── WAF 403：出口 IP 被拦（判据 = **多个不同账号**都吃 403）──
                //
                // ★★ 必须在**继续换号之前**判定（Go `forward.go:751-791` 逐字）：
                // 拦的可能是出口 IP 而不是账号，若还要继续换号，所有号轮一遍全 403
                // 而网关还在换号 —— 把请求放大 `MaxRotate` 倍，恰好是 WAF 最想惩罚的
                // 行为，封禁越打越重。
                //
                // ★ 判定成立时**不动账号状态**（不调 `apply_error_policy`）：账号是好的，
                // 问题在网络出口。把好账号冷却掉只会让用户在「IP 解封之后」发现号也被
                // 自己人停了。
                //
                // ★ 未跨阈值时**照旧**走下面的通用策略（冷却 + 换号）：单个账号的 403
                // 未必是 IP 问题，也可能真是该账号的业务拒绝。
                if crate::wafip::is_waf_forbidden(status, &resp_body) {
                    let distinct = ctx.waf_ip.note_waf(&acct.uid);
                    let blocked = ctx.waf_ip.blocked();
                    crate::logline!(
                        "chat uid={}: WAF 403（窗口内 {} 个不同账号 403，已判定 IP 被拦={}）",
                        acct.uid, distinct, blocked
                    );
                    if blocked {
                        let uid = acct.uid.clone();
                        let (product, region) = product_labels(&acct, is_product);
                        release(ctx.pool, &mut held_uid);
                        return (
                            Some(ChatResult::Response {
                                uid,
                                model: model.clone(),
                                product,
                                region,
                                body: serde_json::Map::new(),
                            }),
                            503,
                            Some(ForwardFailure {
                                kind: FailureKind::EgressIPBlocked,
                                status: 503,
                                message: crate::wafip::egress_ip_blocked_message(distinct),
                            }),
                        );
                    }
                }

                // 请求侧错误：换号**通常**无用（同一请求体发给任何账号都同样
                // 失败）。但「通常」不足以支撑直接失败 —— 实测 11155 带上游侧
                // 抖动，同一请求体重发即可成功（见循环上方的实测数据）。
                //
                // 因此这里先消耗重试预算**原地重试**；预算用尽才按请求侧错误
                // 落定。上下文超长不在此列：它是确定性的，重试纯属浪费。
                //
                // ★★ **只有 11155 走这条重试路径**。11140 曾是这里的一员，那是错的：
                // 它是**账号侧**错误（`ErrKind::AccountUnusable`），换号恰恰有用，
                // 而原地重试会把「同一个打不通的号」连打三次。它现在落进下面的
                // `apply_error_policy` → 账号级软冷却 → `continue` 换号。
                if kind == ErrKind::RequestRejected && reject_retries_left > 0 {
                    reject_retries_left -= 1;
                    // 记一行再重试：上游抖动是**唯一**只能靠日志发现的故障模式，
                    // 不记的话「重试救回来了」与「上游本来就没抖」无法区分，
                    // 抖动恶化到重试也压不住时没有任何先兆。
                    crate::logline!(
                        "[forward] upstream rejected, retrying (left={reject_retries_left}): \
                         model={model} uid={} status={status} body={}",
                        acct.uid,
                        classify::truncate(&resp_body, 200),
                    );
                    release(ctx.pool, &mut held_uid);
                    // 不 push 进 `tried`：下一个账号很可能是同一个（池子小，
                    // 且抖动与账号无关），排除它只会让「重试」变成「换号」，
                    // 反而绕开刚证明可用的那个账号。
                    continue;
                }
                // 请求侧错误的**落定**路径（按各自的类别与状态码返回）。
                //
                // 顺序与 Go `forward.go:793-883` 一致，且都必须早于下面的通用
                // `apply_error_policy` —— 它们全都要求「不轮转、不罚账号」，
                // 落进通用分支就会变成「换个号再试一遍」，把上游的一次性要求
                // 放大成「账号全部不可用」。
                if let Some((fk, out_status, message)) =
                    request_side_failure(kind, status, &resp_body, &model, ctx.overflow_guess)
                {
                    let uid = acct.uid.clone();
                    let (product, region) = product_labels(&acct, is_product);
                    release(ctx.pool, &mut held_uid);
                    return (
                        Some(ChatResult::Response {
                            uid,
                            model: model.clone(),
                            product,
                            region,
                            body: serde_json::Map::new(),
                        }),
                        out_status,
                        Some(ForwardFailure {
                            kind: fk,
                            status: out_status,
                            message,
                        }),
                    );
                }

                {
                    let mut pool = lock_pool(ctx.pool);
                    apply_error_policy(
                        &mut pool,
                        &acct.uid,
                        &model,
                        kind,
                        &resp_body,
                        ctx.soft_cooldown,
                    );
                }
                release(ctx.pool, &mut held_uid);
                continue;
            }
            ChatOutcome::Stream(resp) => {
                // ── 首帧预读（Go `forward.go:890-951`） ─────────────────────────
                //
                // ⚠ 流式请求必须在**写响应头之前**确认这条流真的有内容。
                //
                // 真实缺陷（2026-09-19 现场：empty upstream stream）：旧实现在这里
                // 直接 `note_success` 然后 return，把 rc 交给调用方转写，但上游有一
                // 种失败形态是 **HTTP 200 + 空流**（ZCode 对话通道对受限账号返回
                // 200，流里一帧有效数据都没有，实测 code=3012 "unusual activity"）。
                // 后果两个：① 用户收到 `empty upstream stream`，而状态码 200 已经写
                // 出去了，上层**再也无法换账号重试**；② `note_success` 已调用 ⇒ 坏
                // 账号被记成「成功」、**永不冷却** ⇒ 每次请求都选中它 ⇒ 用户的对话
                // **持续失败**。
                let mut resp = resp;
                let probe = sse::probe_first_frame(&mut resp, ctx.probe_timeout).await;
                // 预读消费掉的字节必须原样交回下游（流式走 `push_prefix`，非流式
                // 直接拼在 `text()` 前面），否则回答会少开头几个字。
                let prefix = probe.consumed;

                let mut probe_msg = String::new();
                if probe.error.is_some() || probe.first.is_empty() {
                    // Go `forward.go:916`：超时与「读到帧但帧无效」归同一句 —— 对
                    // 用户而言都是「上游没给出内容」，区分它们只会泄露内部细节。
                    probe_msg = "上游返回空流（无有效数据帧）".to_string();
                } else {
                    let (bad, reason) = sse::is_upstream_error_frame(&probe.first);
                    if bad {
                        probe_msg = format!("上游首帧即错误：{reason}");
                    }
                }

                if !probe_msg.is_empty() {
                    // ZCode 特判（Go `forward.go:922-933`）：ZCode 过期额度的常见
                    // 表现就是 **HTTP 200 后长期空流**，不是 Qoder 那种正常长推理。
                    // 先查额度即可确认，但转发层仍需给出快速、明确的失败，不能让用户
                    // 等到客户端超时。
                    if acct.is_zcode() {
                        // ★★ 空流**不再**等同于「额度耗尽」。
                        //
                        // 旧实现硬编码 `ErrKind::HardCredit`（**从不读 body**）⇒
                        // 只要 ZCode 某次回了个空流，该账号就被整号硬冷却到次日 04:00。
                        // 而空流的原因很多（上游抖动 / 网关闪断 / 代理超时），与额度
                        // 毫无关系；把「不知道」当成「没额度」是本缺陷的第二条通道。
                        //
                        // 现在按 `probe_msg` 正常分类（Go `forward.go:936-937` 的
                        // 口径）：空流文案里没有 1113 / 14018 等业务码 ⇒ 落
                        // [`ErrKind::Server`] ⇒ [`pool.note_error`]（喂熔断，指数退避），
                        // 既不会硬冷却也不会把号打死。
                        crate::logline!(
                            "chat uid={} product=zcode: {} — 冷却换号（按 503 上游故障分类）",
                            acct.uid, probe_msg
                        );
                        // 先喂空流风暴窗口（在罚号之前）：多号同通道空流 ⇒ 上游整体故障。
                        ctx.empty_stream.note(&acct.uid, acct.product_of(), acct.region().as_str());
                        let kind = zcode_gate(&acct, classify::classify(503, &probe_msg), 503, &probe_msg);
                        last_status = 503;
                        last_kind = kind;
                        last_body = probe_msg.clone();
                        last_transport_err = None;
                        last_err = Some(probe_msg.clone());
                        {
                            let mut pool = lock_pool(ctx.pool);
                            apply_error_policy(
                                &mut pool,
                                &acct.uid,
                                &model,
                                kind,
                                &probe_msg,
                                ctx.soft_cooldown,
                            );
                        }
                        release(ctx.pool, &mut held_uid);
                        // 对应 Go `fail(uid)` 里的 `Session.Unbind` —— 探测失败说明
                        // 这个账号供不了这一轮，粘性绑定必须解掉。
                        if sticky_uid.as_deref() == Some(acct.uid.as_str()) {
                            if let Some(s) = ctx.session {
                                s.unbind(sess_key);
                            }
                            sticky_uid = None;
                        }
                        continue;
                    }

                    // 其它产品：按上游故障冷却换号（Go `forward.go:934-951`）。
                    crate::logline!(
                        "chat uid={} product={}: {} — 冷却换号",
                        acct.uid,
                        acct.product_of(),
                        probe_msg
                    );
                    // 先喂空流风暴窗口（在罚号之前）：多号同通道空流 ⇒ 上游整体故障。
                    ctx.empty_stream.note(&acct.uid, acct.product_of(), acct.region().as_str());
                    // ★ 用 503 让 `apply_error_policy` 走「服务端/上游故障」分类 ——
                    // 它会据此选择冷却时长与是否停用 —— 而不是「请求侧错误」
                    //（Go `forward.go:936-937` 逐字）。
                    let kind = classify::classify(503, &probe_msg);
                    last_status = 503;
                    last_kind = kind;
                    last_body = probe_msg.clone();
                    last_transport_err = None;
                    last_err = Some(probe_msg.clone());
                    {
                        let mut pool = lock_pool(ctx.pool);
                        apply_error_policy(
                            &mut pool,
                            &acct.uid,
                            &model,
                            kind,
                            &probe_msg,
                            ctx.soft_cooldown,
                        );
                    }
                    release(ctx.pool, &mut held_uid);
                    // 对应 Go `fail(uid)` 里的 `Session.Unbind` —— 探测失败说明这个
                    // 账号供不了这一轮，粘性绑定必须解掉，否则下次请求还会选它。
                    if sticky_uid.as_deref() == Some(acct.uid.as_str()) {
                        if let Some(s) = ctx.session {
                            s.unbind(sess_key);
                        }
                        sticky_uid = None;
                    }
                    continue;
                }

                // ── 探测通过：这才算真正的成功 ───────────────────────────────
                lock_pool(ctx.pool).note_success(&acct.uid);
                // 上游成功 ⇒ 3012 熔断**立刻复位**（Go `forward.go:955-961`）。
                //
                // 为什么成功要清（逐字）：它证明上游此刻不再风控。继续熔断会让用户
                // 在一次短暂抖动之后仍然被**我们自己的网关**挡在门外 —— 那是我们
                // 制造的故障，而 3012 本身是上游临时的、会自己过期的限制。
                ctx.unusual.for_product(acct.product_of()).note_success();
                // 粘性跟随最终成功号。
                if !sess_key.is_empty() {
                    if let Some(s) = ctx.session {
                        s.bind(sess_key, &acct.uid);
                    }
                }
                let uid = acct.uid.clone();
                // 两条路径都不再由本函数持有租约：流式移交给调用方（随 result 走，
                // 由其读完/关闭后释放），非流式在下面就地释放。先 take 出来，
                // 避免函数出口的兜底释放重复归还。
                let _ = held_uid.take();

                // ★ 产品与区域在**成功落定**处取一次，两条路径共用（Go
                // `forward.go:970-981` 同位置）。注意两者互斥 —— 见 `product_labels`。
                let (product, region) = product_labels(&acct, is_product);

                if stream {
                    return (
                        Some(ChatResult::Stream {
                            uid,
                            model,
                            product,
                            region,
                            response: resp,
                            prefix,
                        }),
                        200,
                        None,
                    );
                }

                // 非流式：读完整流后聚合。★ 预读消费掉的前缀要接回来，否则聚合
                // 结果会缺开头的内容（首帧就是第一个 chunk，丢了它回答就少一截）。
                let rest = match resp.text().await {
                    Ok(t) => t,
                    Err(e) => {
                        lock_pool(ctx.pool).release(&uid);
                        return (
                            None,
                            502,
                            Some(ForwardFailure {
                                kind: FailureKind::Upstream,
                                status: 502,
                                message: format!("读取上游流失败: {e}"),
                            }),
                        );
                    }
                };
                let text = if prefix.is_empty() {
                    rest
                } else {
                    let mut joined = String::with_capacity(prefix.len() + rest.len());
                    joined.push_str(&prefix);
                    joined.push_str(&rest);
                    joined
                };
                lock_pool(ctx.pool).release(&uid);
                // ★★ 非流式**按产品选聚合器**（Go `forward.go:987-1009`）。
                //
                // ⚠ 逐字警告（Go `:989-994`）：第一版只判了 `isQoder`，于是
                // **ZCode 的账号会走 WorkBuddy 的聚合器**，表现为「无法连接上游」
                // 这种与真实原因无关的报错。新增产品时这里最容易漏 —— 故这里
                // 用**产品无关**的 `product_up` 变量，而不是逐个产品 if。
                let aggregated = match product_up {
                    Some(up) => up.aggregate(&text, &model).await,
                    None => sse::aggregate(&text),
                };
                match aggregated {
                    Ok(resp_body) => {
                        return (
                            Some(ChatResult::Response {
                                uid,
                                model,
                                product,
                                region,
                                body: resp_body,
                            }),
                            200,
                            None,
                        )
                    }
                    Err(e) => {
                        return (
                            None,
                            502,
                            Some(ForwardFailure {
                                kind: FailureKind::Upstream,
                                status: 502,
                                message: e,
                            }),
                        )
                    }
                }
            }
        }
    }

    // 兜底释放：正常情况下各路径已释放，这里是防御性的一步（release 幂等）。
    release(ctx.pool, &mut held_uid);

    // 2026-10-11 现场的教训：走到这里说明轮转整体耗尽，但旧实现**一个字都不记**，
    // 日志里只有各账号自己的失败行，排障时看不出「这一轮到底试了几个号」。
    crate::logline!(
        "chat: 轮转耗尽（本请求已试 {} 个账号，最后状态 {}）",
        tried.len(),
        last_status
    );

    let mut msg = "all accounts unavailable (cooling/disabled)".to_string();
    // ★ 单号池（或所有号都撞上同一个 1113）时，这里必须说清「是这条通道没有
    // 资源包」而不是「账号池全挂了」—— 否则用户看到 `no_healthy_account`
    // 会以为所有账号都坏了，实际只需换一个模型或换一个平台的账号。
    if last_kind == ErrKind::NoResourcePack {
        let friendly = friendly_message(last_kind, last_status, &last_body);
        let msg = if friendly.is_empty() {
            "该模型在这条通道上没有可用资源包（ZCode 1113）：请改用其它账号或其它模型".to_string()
        } else {
            friendly
        };
        return (
            Some(ChatResult::Response {
                uid: last_uid,
                model,
                product: last_labels.0,
                region: last_labels.1,
                body: serde_json::Map::new(),
            }),
            last_status,
            Some(ForwardFailure {
                kind: FailureKind::NoResourcePack,
                status: last_status,
                message: msg,
            }),
        );
    }
    if last_err.is_some() {
        // 客户端可读性：优先用提炼后的原因，拿不到再用原始文案 ——
        // 原始文案含整段上游 JSON 或底层 tcp 细节，又长又难懂。
        let friendly = friendly_message(last_kind, last_status, &last_body);
        if !friendly.is_empty() {
            msg = friendly;
        } else if let Some(t) = &last_transport_err {
            // ★ 传输层的原文必须留给客户端（2026-10-05）：过去这里是 `let _ = t;`
            // 把它整条丢掉，只给一句中文提示。结果是「网络类报错」全都长成一个样，
            // 业主无法分辨到底是超时、连接被拒、DNS 失败还是 TLS 中断 —— 现场那次
            // 「失败原因：Internal Network Failure」正是这类需要原文才能归因的报错。
            // 中文提示仍然打头（可操作），原文附在后面（可归因）。
            // ★ 必须过 client_safe_message 闸门：原文可能夹带上游响应体片段，
            // 里面若出现裸 401/403 会被 DSH 判成 AUTH 而不再重试。
            let detail = crate::clienttext::client_safe_message(t);
            let detail = detail.trim();
            msg = if detail.is_empty() {
                "无法连接上游（网络超时 / 连接被拒）：请检查本机网络或代理设置后重试".to_string()
            } else {
                format!("无法连接上游（网络超时 / 连接被拒）：请检查本机网络或代理设置后重试（{detail}）")
            };
        } else if let Some(e) = &last_err {
            msg = format!("{msg}: {e}");
        }
    }
    (
        Some(ChatResult::Response {
            uid: last_uid,
            model,
            product: last_labels.0,
            region: last_labels.1,
            body: serde_json::Map::new(),
        }),
        last_status,
        Some(ForwardFailure {
            kind: FailureKind::Upstream,
            status: last_status,
            message: msg,
        }),
    )
}

/// 取响应状态码（上游 2xx 已是成功分支，这里取真实值以便日志）。
#[allow(dead_code)]
fn status_of(resp: &reqwest::Response) -> u16 {
    resp.status().as_u16()
}

/// 释放在途租约（幂等：`take` 后重复调用无副作用）。
fn release(pool: &Mutex<Pool>, held: &mut Option<String>) {
    if let Some(uid) = held.take() {
        lock_pool(pool).release(&uid);
    }
}

/// 成功结果里的 `(product, region)` 字段对。
///
/// ★★ **两者互斥**，这是刻意的（Go `forward.go:970-981` 逐字）：
///
/// - `product` 只在**非 WorkBuddy** 时非空（WorkBuddy 是默认产品，留空即
///   「就是它」，与 `Auth::product_of` 把空串归一成 workbuddy 的口径一致）。
/// - `region` 只在 **WorkBuddy** 时非空。理由逐字：「非 WorkBuddy 产品
///   （Qoder/ZCode）不分区 —— 它们的账号池没有区域维度，给它们标一个区域
///   会是**编造**」。
///
/// 这不是保守，而是正确性：Qoder 的 `region` 是「接口域名归属」（qoder.sh /
/// qoder.com.cn），ZCode 的是「服务商」（z.ai / 智谱）—— 两者都不是
/// WorkBuddy 那种「同名模型两个后端、能力不同」的区域语义。混用会让
/// `/status` 与用量统计里出现一个**看起来有意义、实际是猜的**标签。
fn product_labels(acct: &Auth, is_product: bool) -> (String, String) {
    if is_product {
        (acct.product_of().to_string(), String::new())
    } else {
        (String::new(), acct.region().as_str().to_string())
    }
}

/// 刷新失败的错误分类：区分「服务端明确拒绝」与「传输层失败」。
///
/// 传输层失败（网络/代理不可达）**不算** session 失效 —— 早期版本把两者混为
/// 一谈，结果一次代理抖动就让整批账号被禁用。
fn classify_refresh_err(e: &crate::error::GatewayError) -> (ErrKind, Option<String>) {
    match e {
        crate::error::GatewayError::UpstreamError(u) => (u.kind, None),
        // 非 UpstreamError 的失败基本都是传输层（超时 / 连接被拒）
        _ => (ErrKind::None, Some(e.to_string())),
    }
}

/// 产品上游返回的**请求侧**错误 → `(失败类别, HTTP 状态, 面向客户端的文案)`。
///
/// 对应 Go `forward.go:632-680` 的两条 `errors.As` 分支，**顺序至关重要**：
/// 它们都必须发生在 `fail(acct.UID)` + `continue` **之前**，否则会被降级成
/// 「换个号再试一遍」，把上游的一次性要求放大成「账号全部不可用」。
///
/// 返回 `None` 表示这不是请求侧错误，调用方按通用传输层失败处理。
/// ★★ ZCode 1113 门控：把「**这条通道没有该账号的资源包**」从「真没额度」里择出来。
///
/// # 为什么必须有这一层（实测缺陷）
///
/// ZCode 的业务码 `1113` 原文是「余额不足或**无可用资源包**，请充值」，配 HTTP
/// **429**，真实含义是「这条通道上没有该模型的资源包」—— 而通用
/// [`classify::classify`] 的 `HARD_MARKERS` 里有「余额不足」，会把它判成
/// [`ErrKind::HardCredit`] ⇒ [`cooldown_until_tomorrow_4am`] ⇒ 一个**一分额度
/// 没花**的账号被整号硬冷却到次日 04:00，界面显示「余额不足（积分欠费）」。
///
/// 实测：剩余 9,977 万 / 1 亿额度的满额账号，因为收到一条 `glm-5.3` 请求
///（该账号套餐里只有 `GLM-5.3-Flash`）就是这么被冻住的。
///
/// # 为什么只对 ZCode 账号生效
///
/// WorkBuddy / Qoder 的「余额不足」是**真的**没额度
///（`matches_go_testclassify_matrix` 里钉死的断言），语义不得被本补丁改掉。
fn zcode_gate(acct: &Auth, kind: ErrKind, status: u16, body: &str) -> ErrKind {
    if !acct.is_zcode() {
        return kind;
    }
    match crate::products::zcode::errors::classify(status, body) {
        crate::products::zcode::errors::ErrKind::NoResourcePack => ErrKind::NoResourcePack,
        _ => kind,
    }
}

fn product_request_side_failure(
    e: &crate::error::GatewayError,
    acct: &Auth,
    model: &str,
) -> Option<(FailureKind, u16, String)> {
    // ① 上游模型清单里没有它（Go `forward.go:641-669`）。
    if let Some(mnf) = e.model_not_found() {
        // ★ 记一行：这条错误**不进 last_err**（直接 return），若不在这里落日志，
        // 「用户写错模型名」在网关日志里会完全不可见 —— 而这是最常见的用户
        // 求助原因。日志里带 uid 与清单条数，便于判断是「清单没刷新」还是
        // 「模型真不存在」。
        crate::logline!(
            "chat uid={}: 上游 {} 没有模型 {:?}（清单 {} 个），请求侧错误 —— 不轮转、不罚账号",
            acct.uid,
            if mnf.region.is_empty() { "该产品" } else { &mnf.region },
            mnf.model,
            mnf.available.len()
        );
        return Some((FailureKind::ModelNotInUpstream, 400, mnf.message()));
    }

    // ② 上游要求人机验证（ZCode 3007）。轮转必定无用（见 `CaptchaRequired` 文档）。
    if e.is_captcha_required() {
        crate::logline!(
            "chat uid={} product={}: 上游要求人机验证，请求侧错误 —— 不轮转、不罚账号",
            acct.uid,
            acct.product_of()
        );
        // ★★ 状态码是 **503**，不是 400。Go `forward.go:743-748` 逐字：
        //     `http.StatusServiceUnavailable`
        // 语义是「上游此刻要人机验证，稍后（在官方客户端过验证后）再试」，
        // 而不是「你的请求写错了」。
        //
        // 背景（`FailureKind::CaptchaRequired` 文档里的现场）：用户此前收到
        // `503 {"code":"no_healthy_account","message":"all accounts unavailable
        // (cooling/disabled): ... captcha verify failed"}` —— 那条消息每一句都在
        // 误导：`all accounts unavailable`（账号一个都没问题，本分支刻意不罚号）、
        // `(cooling/disabled)`（没有任何账号因此进入冷却）。现在仍回 503，但
        // 换成了 `captcha_required` + 真实原因。
        return Some((FailureKind::CaptchaRequired, 503, e.to_string()));
    }

    // `model` 目前不参与文案，但保留参数是为了让调用点与 Go 的签名一致，
    // 且后续若要做「按模型给建议」（例如提示该模型的正确写法）无需改签名。
    let _ = model;
    None
}

/// 上游业务错误里的**请求侧**类别 → `(失败类别, HTTP 状态, 面向客户端的文案)`。
///
/// 对应 Go `forward.go:793-883` 的四条 `kind ==` 分支，**顺序与 Go 一致**，
/// 且都必须发生在通用 `apply_error_policy` **之前** —— 它们全都要求
/// 「不轮转、不罚账号」，落进通用分支就会变成「换个号再试一遍」。
///
/// 返回 `None` 表示这不是请求侧错误，调用方按通用策略处理（施加冷却 + 换号）。
///
/// ★ 状态码一律**原样透出上游的**（`status`），不改成 4xx/5xx：
/// 上游用 400 表示「请求有问题」，这个语义必须原样到达客户端。
fn request_side_failure(
    kind: ErrKind,
    status: u16,
    resp_body: &str,
    model: &str,
    // 「这次请求预算超限」的估算（`None` = 算不出）。见 `ForwardCtx::overflow_guess`。
    overflow_guess: Option<bool>,
) -> Option<(FailureKind, u16, String)> {
    match kind {
        // 上下文超长：必须**保留上游原文** —— 下游客户端（如 DeepSeek Harness）
        // 靠文案模式识别上下文溢出（`prompt is too long` /
        // `context_length_exceeded` / `exceeds the model context limit`）触发
        // 自动压缩并重试；只回我们自己的措辞会让它认不出这是溢出，
        // 只会把它当成普通失败。
        ErrKind::ContextTooLong => Some((
            FailureKind::ContextTooLong,
            status,
            context_too_long_message(resp_body),
        )),
        // 11155 思维链缺失：重试预算已在上方用尽，原样透出上游原文。
        ErrKind::RequestRejected => Some((
            FailureKind::RequestRejected,
            status,
            request_rejected_message(resp_body),
        )),
        // 思考档位被拒：文案必须**保留上游原文** —— 用户要据此判断该换哪个档位。
        ErrKind::EffortRejected => Some((
            FailureKind::EffortRejected,
            status,
            format!(
                "上游不接受本次指定的思考档位（reasoning_effort）：{}",
                classify::effort_rejected_detail(resp_body)
            ),
        )),
        // 11102：模型在这条通道上不存在。文案必须**可操作**（说清「等也没用」
        // 并给出出路），上游原文 `model [X] service info not found` 既像服务故障
        // 又没说该怎么办，用户只会反复重试同一个模型。
        ErrKind::ModelNotInRegion => Some((
            FailureKind::ModelNotInRegion,
            status,
            classify::model_not_in_region_message(model, resp_body),
        )),
        // ★★ 11133：多数是「参数被模型提供商拒」，但**超出上下文预算**时上游也回它
        //（2026-10-09 业主现场，见 `is_param_invalid_from_context_overflow` 的实测表）。
        //
        // 预算超限时归 `ContextTooLong` —— 客户端据此触发自动压缩，那**正是**解药；
        // 归 11133 会让它认不出溢出，每轮重发同一个超限请求而永久卡死。
        //
        // 判据来自**发请求前**算好的 `ctx.overflow_guess`（`None` = 算不出 ⇒ 不猜，
        // 逐字退回原来的 11133 行为）。上游文案里若点名了上下文边界也算命中
        //（`classify` 里已先判过一次；这里再判是因为 `overflow_guess` 只有本层拿得到）。
        ErrKind::ModelParamInvalid => {
            if overflow_guess == Some(true)
                || classify::is_param_invalid_from_context_overflow(resp_body)
            {
                crate::logline!(
                    "[forward] 上游 11133 且预算超限估算为真 ⇒ 归上下文超长（让客户端压缩）"
                );
                return Some((
                    FailureKind::ContextTooLong,
                    status,
                    classify::context_too_long_message(resp_body),
                ));
            }
            Some((
                FailureKind::ModelParamInvalid,
                status,
                classify::model_param_invalid_message(resp_body),
            ))
        }
        _ => None,
    }
}

/// 按错误分类对账号施加冷却 / 禁用 / 熔断策略。
///
/// 六条路径，各司其职：
/// - `HardCredit` → 冷却到次日 04:00（等签到恢复）
/// - `ModelRate` → **模型级**冷却到上游给的重置时间；账号其他模型不受影响
/// - `SoftRate` / `NotFound` → 账号级软冷却
/// - `AccountUnusable` → 账号级软冷却（★ 有到期、可自动恢复，**绝不永久禁用**）
/// - `SessionDead` → 永久禁用（需人工重登）
/// - `Server` → 喂熔断计数（指数退避）
/// - 其余（`Client` / `None`）→ 只换号不罚（防雪崩），不喂熔断
pub fn apply_error_policy(
    pool: &mut Pool,
    uid: &str,
    model: &str,
    kind: ErrKind,
    raw_body: &str,
    soft_cooldown: Duration,
) {
    match kind {
        ErrKind::HardCredit => {
            pool.cooldown_until_tomorrow_4am(uid, "余额不足");
        }
        ErrKind::ModelRate => {
            // 上游文案给出确切重置时刻（"将在 2026-09-15 13:25:47 UTC+8 重置"），
            // 优先用它 —— 比固定软冷却更准，既不会过早重试（继续撞限流），
            // 也不会过晚恢复（白等）。解析不出时回退固定软冷却时长。
            let now = chrono::Utc::now();
            let until = match classify::parse_reset_time(raw_body, now) {
                Some(t) => SystemTime::from(t),
                None => SystemTime::now() + soft_cooldown,
            };
            let parsed = classify::parse_reset_time(raw_body, now).is_some();
            pool.cooldown_model(uid, model, until, &model_rate_reason(model, until, parsed));
        }
        ErrKind::SoftRate => {
            pool.cooldown(uid, CoolKind::Soft, soft_cooldown, "429 rate limit");
        }
        // ★★ 11140（账号在当前设备打不通）：账号级**软冷却**。
        //
        // 用 `CoolSoft`（有到期、可自动恢复）而**不是** `Disable`：
        // 上游根因是「该账号在本设备未完成登录（设备注册失效）」，用户去官方
        // 客户端重登一次即可恢复 —— 与 `SessionDead`「凭证失效必须重登」的
        // 恢复动作不同，且**绝不永久禁用**（Go 测试
        // `TestAccountUnusableRotatesToNextAccount:159` 断言 `st.Disabled == false`）。
        //
        // ★ 冷却原因是给人看的排查线索，必须带上业务码 11140
        //（Go 测试 `:207` 逐字：「冷却原因没有带上 11140 归因，事后无法分辨它是
        // 怎么被罚的」）。但**绝不能写 "403"** —— 那个数字会被客户端读成
        // 「API 密钥无效」（见 `classify::friendly_message` 的 AccountUnusable 分支）。
        ErrKind::AccountUnusable => {
            pool.cooldown(
                uid,
                CoolKind::Soft,
                soft_cooldown,
                "上游拒绝该账号（11140 request illegal）",
            );
        }
        ErrKind::SessionDead => {
            pool.disable(uid, "12153 session dead");
        }
        ErrKind::NotFound => {
            pool.cooldown(uid, CoolKind::Soft, soft_cooldown, "upstream 404");
        }
        ErrKind::Server => {
            pool.note_error(uid);
        }
        // 请求侧拒绝（11155 思维链缺失）在轮转循环里已短路返回，理论上走不到这里；
        // 即便到达也绝不罚账号 —— 账号什么都没做错。
        //
        // 其余请求侧错误（EffortRejected / ModelNotInRegion / ModelParamInvalid）
        // 同理：它们都已在上游失败分支里短路落定，且**绝不冷却账号**。
        ErrKind::RequestRejected
        | ErrKind::EffortRejected
        | ErrKind::ModelNotInRegion
        | ErrKind::ModelParamInvalid => {}
        // ZCode 1113「这条通道没有该账号的资源包」：**软**冷却 + **只封这个模型**。
        //
        // ★ 两条都必须写，缺一不可：
        // - 账号级软冷却 ⇒ `healthy()` 为假；
        // - `cooldown_model` ⇒ `model_available()` 为假。
        //
        // `pick_inner` 的候选过滤同时检查这两项，唯一的绕过口是兜底
        // [`pick_earliest_expiry`]（它也过滤 `!model_available`）—— 故两者
        // 叠加后主路径与全部兜底都不会再选到它。
        //
        // ⚠ 绝不能用 `cooldown_until_tomorrow_4am`：那正是本缺陷的病灶。
        ErrKind::NoResourcePack => {
            pool.cooldown(
                uid,
                CoolKind::Soft,
                soft_cooldown,
                "该通道无可用资源包（ZCode 1113）",
            );
            pool.cooldown_model(
                uid,
                model,
                SystemTime::now() + soft_cooldown,
                "该模型不在本账号套餐内（1113 无可用资源包）",
            );
        }
        // 其余（Client / None）：只换号不罚（防雪崩），不喂熔断。
        _ => {}
    }
}

/// 生成模型冷却的展示文案（含模型名与重置时间）。
///
/// 前端直接用这条文案，避免两端各自格式化时间导致口径不一致。
fn model_rate_reason(model: &str, until: SystemTime, parsed: bool) -> String {
    let name = if model.is_empty() { "（未知模型）" } else { model };
    let dt: chrono::DateTime<chrono::Local> = until.into();
    if parsed {
        format!("{name} 已达频率上限，{} 重置", dt.format("%m-%d %H:%M"))
    } else {
        format!(
            "{name} 已达频率上限（未取到重置时间，按软冷却 {} 处理）",
            dt.format("%H:%M")
        )
    }
}

/// 生成「带图片请求缺少该区域账号」的说明。
///
/// 必须同时给出**原因**与**出路**：这类失败用户第一次遇到时完全无法自行判断
///（网关返回 503，客户端只显示「服务不可用」），而原因（上游两区同名模型的
/// 图像能力不同）与出路（补一个那个区域的账号）都只有网关知道。
pub fn image_region_unavailable_message(region: Region) -> String {
    let name = if region == Region::Intl { "国际版" } else { "国服" };
    format!(
        "该模型带图片的请求需要{name}账号（实测只有{name}后端能读取图片，\
另一个区域会把图片替换成占位符后交给模型，表现为模型回复「无法查看图片」），\
但账号池里此刻没有可用的{name}账号。请添加/启用一个{name}账号，或去掉图片后重试。"
    )
}

/// 区域的中文显示名（用于面向用户的错误文案）。
///
/// `Region::Any` 返回「该」而不是「不限」：它出现在「平台 %s（%s）的模型白名单」
/// 这种句式里，读起来是「该区域的模型白名单」——「不限区域的模型白名单」不通。
fn region_display_name(region: Region) -> &'static str {
    match region {
        Region::CN => "国服",
        Region::Intl => "国际版",
        Region::Any => "该",
    }
}

/// 该产品 × 该区域是否**允许**跑这个模型。
///
/// 返回 `false` 表示**用户明确禁止**；白名单未配置时返回 `true`
///（向后兼容 —— 老配置没有 `platform_models` 键，升级后不该突然开始拦请求）。
///
/// # 为什么区域判断收在这里
///
/// 请求的 `route.region` 可能是 [`Region::Any`]，而白名单是**按区域**配的。
/// 此时只要**任一**区域允许就放行，因为具体选到哪个区域的账号由选号侧决定，
/// 那里会拿**账号自己的区域**再判一次（`Pool::pick_for_model_product_region`）。
/// 也就是说本函数是「早退优化 + 好文案」，真正的准入判定在 pool 里按账号区域做；
/// **两处判据必须一致，故都走 `Pool::platform_allows_model`**。
fn model_allowed_for_product(pool: &Pool, product: &str, route: ImageRoute, model: &str) -> bool {
    if route.region == Region::Any {
        for r in [Region::CN, Region::Intl] {
            let (allowed, configured) = pool.platform_allows_model(product, r, model);
            if !configured || allowed {
                return true;
            }
        }
        return false;
    }
    let (allowed, configured) = pool.platform_allows_model(product, route.region, model);
    !configured || allowed
}

/// 生成「这个平台被总开关关掉了」的说明。
///
/// ⚠ **不能**复用 `[model_not_in_product_message]`：那条文案会让用户去「兼容网关」
/// 页改白名单，而真实原因是他自己在账号页把整个平台关掉了 —— 照着改白名单
/// 改到天亮也不会好。错误文案指错方向比不写更糟。
///
/// 也不能复用 `[all_no_route_message]`：那条说的是**账号级** no_route，用户会去
/// 逐个账号翻开关；这里关的是整个平台。
///
/// ★ 必须强调「任务照跑」：所有者明确澄清过
///
/// > 「关闭平台只是不参与兼容网关,并不是不执行任务请注意」
///
/// 用户看到「平台已关闭」的第一反应可能是「我的签到 / 养号是不是也停了」。
pub fn platform_disabled_message(product: &str) -> String {
    format!(
        "平台 {product} 已被关闭「参加兼容网关路由」的开关，所以它的账号不会参与选号，这个请求没有账号可以处理。\
这不是故障，也**不影响养号任务** —— 签到、上报、成长任务、额度巡检都照常运行。\
想让它重新接流量，请在账号页的 {product} 标签页里打开「参加兼容网关路由」。"
    )
}

/// 生成「平台白名单里没有这个模型」的说明。
///
/// 为什么要把该平台**实际允许**的模型列出来：用户看到「qoder 不允许
/// deepseek-v4.1-flash」之后，下一个问题必然是「那它允许什么」——
/// 不列出来他就得自己去配置页翻。
///
/// ⚠ 白名单**为空**时不列清单（那说明用户压根没配这个平台/区域），
/// 只给「去配置」的指引 —— **不编造**。
pub fn model_not_in_product_message(
    product: &str,
    region: Region,
    model: &str,
    allowed: &[String],
) -> String {
    let region_name = region_display_name(region);
    let mut b = format!("平台 {product}（{region_name}）的模型白名单里没有 {model}，所以没有账号可以处理这个请求。");
    if !allowed.is_empty() {
        let (shown, suffix) = if allowed.len() > 12 {
            (
                &allowed[..12],
                format!(" 等 {} 个", allowed.len()),
            )
        } else {
            (allowed, String::new())
        };
        b.push_str(&format!("当前允许：{}{suffix}。", shown.join("、")));
    }
    b.push_str(&format!(
        "这是**可配置**的 —— 想用这个模型，请在「兼容网关」页把 {model} 加进平台 {product} 的{region_name}白名单；\
否则请换一个已允许的模型，或去掉「平台:」前缀让网关自动选择平台。"
    ));
    b
}

/// 生成「账号都被设为不参与选号」的说明。
///
/// 必须说清**这不是故障**：用户看到 503 的第一反应是等冷却恢复，
/// 而真实原因是他自己把路由开关关了 —— 等多久都不会好。
pub fn all_no_route_message(total: usize) -> String {
    format!(
        "该产品下的可用账号（共 {total} 个）都被设为「不参与选号」(no_route)，所以你这边没有账号可用了。\
这不是故障 —— 在账号列表里重新打开它们的路由开关即可。"
    )
}

/// 把请求体里的 `model` 字段改写为**剥掉前缀后的裸名**。
///
/// # 为什么必须改写
///
/// `cn:` / `global:` / `qoder:` 这类前缀只是**给网关的选号指令**，上游并不认识它。
/// 原样转发会得到 `400 code=11102 model [cn:glm-5.2] service info not found` ——
/// 即「前缀成功约束了选号，却让请求本身失败」（实测：带前缀的请求全部 11102，
/// 无前缀的同名请求正常）。
///
/// # 为什么可以整份重新序列化
///
/// Go 侧刻意用 `json.RawMessage` 只改一个键、保留其余键的字节；Rust 侧不需要
/// 这个讲究 —— 请求体在发往上游前本来就会被 `prepare_body_for_region` 用
/// `serde_json::Value` 整体往返一次（插入 `stream`、归一化 role/tool_choice、
/// 按区域补 system），键序与空白**本来就不保留**。用 `Value` 实现与既有行为一致。
///
/// 任何一步失败都原样返回 `body`：宁让上游报出诚实的错误，也不要我们改写出一份四不像。
pub fn rewrite_model(body: &[u8], bare: &str) -> Vec<u8> {
    if bare.is_empty() {
        return body.to_vec();
    }
    let Ok(serde_json::Value::Object(mut doc)) = serde_json::from_slice(body) else {
        return body.to_vec();
    };
    let Some(cur) = doc.get("model") else {
        return body.to_vec();
    };
    // 已是裸名（或无 model 字段）→ 不动，省一次整体编码。
    if cur.as_str() == Some(bare) {
        return body.to_vec();
    }
    doc.insert(
        "model".to_string(),
        serde_json::Value::String(bare.to_string()),
    );
    match serde_json::to_vec(&serde_json::Value::Object(doc)) {
        Ok(out) => out,
        Err(_) => body.to_vec(),
    }
}

/// 从 OpenAI Chat 请求体里取 `model` 字段（仅用于日志与路由）。
pub fn model_of(body: &[u8]) -> String {
    #[derive(serde::Deserialize)]
    struct Probe {
        #[serde(default)]
        model: String,
    }
    serde_json::from_slice::<Probe>(body)
        .map(|p| p.model)
        .unwrap_or_default()
}

/// 报告请求体里是否携带图片分片。
///
/// 三种协议入口最终都会把图片归一成 `{"type":"image_url", ...}` 分片
///（Responses 的 `input_image`、Messages 的 Anthropic image 块），
/// 因此只需在这里认这一种形状。
///
/// 判据用「分片里存在 `image_url` 键」而不是「`type == image_url`」：
/// 上游/客户端对 image 分片的 type 写法不止一种，按 type 精确匹配会漏判，
/// 而漏判的后果是请求被路由到读不到图片的后端 —— 正是本函数要避免的。
pub fn request_has_image(body: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) else {
        return false;
    };
    for m in msgs {
        let Some(content) = m.get("content") else {
            continue;
        };
        // content 可能是字符串（纯文本）或分片数组；只有数组才可能含图片。
        let Some(parts) = content.as_array() else {
            continue;
        };
        for p in parts {
            if p.get("image_url").is_some() {
                return true;
            }
            if let Some(t) = p.get("type").and_then(|t| t.as_str()) {
                if matches!(t, "image_url" | "input_image" | "image") {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::{region_of, Pool};
    use crate::upstream::client::ClientConfig;

    fn pool_with(uids: &[&str]) -> Mutex<Pool> {
        let p = Mutex::new(Pool::new(String::new()));
        {
            let mut pool = p.lock().unwrap();
            for uid in uids {
                pool.add(Auth {
                    uid: (*uid).to_string(),
                    access_token: "at".into(),
                    domain: "https://copilot.tencent.com".into(),
                    ..Default::default()
                });
            }
        }
        p
    }

    /// 与 [`pool_with`] 同款，但可显式指定账号所属产品
    ///（空串 = workbuddy，见 [`Auth::product_of`]）。
    fn pool_with_products(pairs: &[(&str, &str)]) -> Mutex<Pool> {
        let p = Mutex::new(Pool::new(String::new()));
        {
            let mut pool = p.lock().unwrap();
            for (uid, product) in pairs {
                pool.add(Auth {
                    uid: (*uid).to_string(),
                    access_token: "at".into(),
                    domain: "https://copilot.tencent.com".into(),
                    product: (*product).to_string(),
                    ..Default::default()
                });
            }
        }
        p
    }

    fn ctx<'a>(
        pool: &'a Mutex<Pool>,
        client: &'a Client,
        products: &'a crate::products::ProductUpstreams,
        unusual: &'a crate::unusual::UnusualBreakers,
        waf_ip: &'a crate::wafip::WafIpWindow,
        empty_stream: &'a crate::emptystream::EmptyStreamWindow,
    ) -> ForwardCtx<'a> {
        ForwardCtx {
            pool,
            client,
            session: None,
            max_rotate: 3,
            soft_cooldown: Duration::from_secs(60),
            refresh_skew: Duration::from_secs(600),
            allowed_model: Vec::new(),
            products,
            unusual,
            waf_ip,
            empty_stream,
            probe_timeout: Duration::from_secs(30),
            models_cache: None,
        overflow_guess: None,
        }
    }

    /// ★ 测试用：每个用例**各自一份**熔断器与 WAF 窗口。
    ///
    /// 不用进程级共享实例：测试是并行跑的，一个用例触发的 3012 熔断会漂到另一个
    /// 用例里（表现为随机失败，且只在满负载时才复现）。生产侧才需要共享
    /// —— 那里的「同一个出口 IP / 同一个上游风控」是真实共享的状态。
    /// 调用点写法：
    /// ```ignore
    /// let (unusual, waf_ip, empty_stream) = fresh_breakers();
    /// let mut c = ctx(&p, &client, &products, &unusual, &waf_ip, &empty_stream);
    /// ```
    fn fresh_breakers() -> (
        crate::unusual::UnusualBreakers,
        crate::wafip::WafIpWindow,
        crate::emptystream::EmptyStreamWindow,
    ) {
        (
            crate::unusual::UnusualBreakers::new(),
            crate::wafip::WafIpWindow::new(),
            crate::emptystream::EmptyStreamWindow::new(),
        )
    }

    // ---------- ★ ⑪ ZCode 1113「无可用资源包」不得被判成「额度耗尽」 ----------

    fn zcode_auth(uid: &str) -> Auth {
        Auth {
            uid: uid.to_string(),
            access_token: "at".into(),
            domain: "https://zcode.z.ai".into(),
            product: "zcode".into(),
            ..Default::default()
        }
    }

    /// ★★ 本缺陷的**逐字回归护栏**。
    ///
    /// 实测现场：剩余 9,977 万 / 1 亿额度的满额 ZCode 账号，因为收到一条
    /// `glm-5.3` 请求（它的套餐里只有 `GLM-5.3-Flash`），上游回
    /// `HTTP 429 {"error":{"code":"1113","message":"余额不足或无可用资源包,请充值。"}}`，
    /// 通用分类器按「余额不足」判成 [`ErrKind::HardCredit`] ⇒
    /// `cooldown_until_tomorrow_4am` ⇒ 整号硬冷却到次日 04:00（`state.json` 里
    /// 该号 `cool_kind: 1` / `reason: "余额不足"`，而它的额度一分没花）。
    ///
    /// 门控之后必须改判成 [`ErrKind::NoResourcePack`] —— 它只做软冷却 + 封模型。
    #[test]
    fn zcode_1113_is_not_hard_credit() {
        let body = r#"{"error":{"code":"1113","message":"余额不足或无可用资源包,请充值。"}}"#;
        let zc = zcode_auth("z1");

        // ① ZCode 专有措辞「无可用资源包」在**任何状态码**下都不得被判成额度耗尽。
        //    实测上游有时给 429（限流通道）、有时给 402，只看中文文案会误判。
        for status in [400u16, 402, 429, 500] {
            assert_eq!(
                classify::classify(status, body),
                ErrKind::NoResourcePack,
                "status={status}：1113 的「无可用资源包」不得被判成额度耗尽"
            );
        }
        // ② 只有码、没有中文时兜底（必须带 429 门控，见下一条）。
        assert_eq!(
            classify::classify(429, r#"{"code":1113}"#),
            ErrKind::NoResourcePack
        );
        // ③ 门控的意义：WorkBuddy 的 400「余额不足」是**真的**没额度，
        //    不得被改判成「套餐里没这个模型」（`matches_go_testclassify_matrix`
        //    钉死了 `(400, {"code":1,"msg":"余额不足"}, HardCredit)`）。
        assert_eq!(
            classify::classify(400, r#"{"code":1,"msg":"余额不足"}"#),
            ErrKind::HardCredit,
            "400 的「余额不足」不属于 ZCode 1113，必须保持额度耗尽语义"
        );
        // ④ `zcode_gate` 的职责：即使通用分类器**已经**判成 HardCredit
        //    （如 402 + 只有「余额不足」文案），ZCode 账号也要被改判。
        assert_eq!(
            zcode_gate(&zc, ErrKind::HardCredit, 429, body),
            ErrKind::NoResourcePack,
            "ZCode 账号上的 1113 必须改判成 NoResourcePack"
        );
        // ★ 非 ZCode 账号**不得**被改写：WorkBuddy 的「余额不足」是真的没额度。
        let wb = Auth {
            uid: "w1".into(),
            access_token: "at".into(),
            ..Default::default()
        };
        assert_eq!(zcode_gate(&wb, ErrKind::HardCredit, 429, body), ErrKind::HardCredit);
    }

    /// 空流 / 首帧错误的路径（旧实现硬编码 `HardCredit`，**从不读 body**）：
    /// 探测文案里没有 1113 / 14018 等业务码 ⇒ 必须落 [`ErrKind::Server`]
    ///（只喂熔断，指数退避），绝不硬冷却到次日 04:00。
    #[test]
    fn zcode_empty_stream_is_not_hard_credit() {
        let zc = zcode_auth("z1");
        let kind = zcode_gate(
            &zc,
            classify::classify(503, "上游返回空流（无有效数据帧）"),
            503,
            "上游返回空流（无有效数据帧）",
        );
        assert_eq!(kind, ErrKind::Server);
    }

    /// [`apply_error_policy`] 的新臂：**软**冷却 + **只封这个模型**。
    ///
    /// 两条都必须落地，缺一不可：账号级 `until` 让 `healthy()` 为假、
    /// `model_until` 让 `model_available()` 为假。[`pick_inner`] 的候选
    /// 过滤同时检查这两项，兜底 [`pick_earliest_expiry`] 也过滤
    /// `model_available` ⇒ 主路径与全部兜底都不会再选到它。
    #[test]
    fn no_resource_pack_cools_softly_and_model_only() {
        let p = pool_with(&["u1"]);
        {
            let mut pool = p.lock().unwrap();
            apply_error_policy(
                &mut pool,
                "u1",
                "glm-5.3",
                ErrKind::NoResourcePack,
                r#"{"error":{"code":"1113"}}"#,
                Duration::from_secs(60),
            );
        }
        let st = p.lock().unwrap().status_by_uid("u1").unwrap();
        assert!(st.cooling, "应处于冷却中");
        assert_eq!(
            st.cool_kind, "soft_rate",
            "必须是软冷却（soft_rate）—— hard_credit 会把整号冻到次日 04:00"
        );
        assert!(
            st.cool_remaining_sec > 0 && st.cool_remaining_sec <= 60,
            "软冷却时长应取自 soft_cooldown，实际 {}",
            st.cool_remaining_sec
        );
        let mc = &st.model_cooling;
        assert_eq!(mc.len(), 1, "应只封这一个模型：{mc:?}");
        assert_eq!(mc[0].model, "glm-5.3");
        // 别的模型不受影响（同账号换模型仍可用）。
        let pool = p.lock().unwrap();
        assert!(
            pool.status_by_uid("u1").unwrap().model_cooling.len() == 1,
            "只应有 glm-5.3 一条模型冷却"
        );
    }

    // ★ 码面值的断言统一放在 `server.rs::tests::chat_failure_codes_match_go_verbatim`：
    // `Wire` 是 `server.rs` 的**私有**枚举，本模块的测试看不见它（写成
    // `use crate::server::Wire;` 会直接编译失败）。那里同时钉死了
    // `no_resource_pack` 与 `no_healthy_account` / `quota_exhausted` 三者互不相同。

    #[test]
    fn model_of_reads_model_field() {
        assert_eq!(model_of(br#"{"model":"glm-5.3"}"#), "glm-5.3");
        assert_eq!(model_of(br#"{"messages":[]}"#), "");
        assert_eq!(model_of(b"not json"), "");
    }

    #[test]
    fn request_has_image_detects_image_url_key() {
        let with_key = br#"{"messages":[{"content":[{"type":"image_url","image_url":{"url":"x"}}]}]}"#;
        assert!(request_has_image(with_key));
        // 只按 type 判也要命中（image_url / input_image / image）
        let by_type = br#"{"messages":[{"content":[{"type":"input_image","x":1}]}]}"#;
        assert!(request_has_image(by_type));
        let anthropic = br#"{"messages":[{"content":[{"type":"image","source":{}}]}]}"#;
        assert!(request_has_image(anthropic));
    }

    #[test]
    fn request_has_image_false_for_text_only() {
        assert!(!request_has_image(br#"{"messages":[{"content":"plain text"}]}"#));
        assert!(!request_has_image(br#"{"messages":[]}"#));
        assert!(!request_has_image(b"not json"));
    }

    /// 前缀只是给网关的选号指令，上游不认识 —— 必须改写成裸名再转发。
    #[test]
    fn rewrite_model_strips_prefix_from_body() {
        let body = br#"{"model":"cn:glm-5.2","messages":[{"role":"user","content":"hi"}]}"#;
        let out = rewrite_model(body, "glm-5.2");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "glm-5.2");
        // 其余字段必须原样保留（改写只动 model 一个键）
        assert_eq!(v["messages"][0]["content"], "hi");
    }

    /// 已是裸名 → 原样返回同一份字节（不做无谓的整体重新编码）。
    #[test]
    fn rewrite_model_noop_when_already_bare() {
        let body = br#"{"model":"glm-5.2"}"#;
        assert_eq!(rewrite_model(body, "glm-5.2"), body.to_vec());
        // 空裸名（无前缀）同样原样返回
        assert_eq!(rewrite_model(body, ""), body.to_vec());
    }

    /// 任何一步失败都原样返回：宁让上游报诚实的错，也不要改写出一份四不像。
    #[test]
    fn rewrite_model_preserves_body_on_unparsable_input() {
        let not_json = b"not json at all";
        assert_eq!(rewrite_model(not_json, "glm-5.2"), not_json.to_vec());
        // 没有 model 字段 → 无从改写，原样返回
        let no_model = br#"{"messages":[]}"#;
        assert_eq!(rewrite_model(no_model, "glm-5.2"), no_model.to_vec());
    }

    /// 「账号全被设为不参与选号」必须说清**这不是故障** —— 否则用户会去等冷却恢复。
    #[test]
    fn all_no_route_message_says_it_is_config_not_failure() {
        let m = all_no_route_message(5);
        assert!(m.contains("5 个"), "{m}");
        assert!(m.contains("no_route"), "{m}");
        assert!(m.contains("不是故障"), "{m}");
        assert!(m.contains("路由开关"), "{m}");
    }

    /// 白名单拦截文案必须给出**出路**（去配置页加白名单），并列出当前允许的模型。
    #[test]
    fn model_not_in_product_message_lists_allowed_and_points_to_config() {
        let allowed: Vec<String> = (0..15).map(|i| format!("m{i}")).collect();
        let m = model_not_in_product_message("qoder", Region::Intl, "deepseek-v4.1-flash", &allowed);
        assert!(m.contains("qoder"), "{m}");
        assert!(m.contains("国际版"), "{m}");
        assert!(m.contains("deepseek-v4.1-flash"), "{m}");
        // 超过 12 个只列前 12 个 + 「等 N 个」
        assert!(m.contains("当前允许：m0、m1"), "{m}");
        assert!(m.contains("等 15 个"), "{m}");
        assert!(!m.contains("m12"), "超过 12 个不应全列: {m}");
        assert!(m.contains("兼容网关"), "{m}");
    }

    /// 白名单**为空**时不列清单 —— 那说明用户没配这个平台，不编造。
    #[test]
    fn model_not_in_product_message_omits_empty_allowlist() {
        let m = model_not_in_product_message("qoder", Region::Any, "m", &[]);
        assert!(!m.contains("当前允许"), "{m}");
        assert!(m.contains("该"), "{m}"); // RegionAny → 「该」区域的模型白名单
    }

    /// ★ 端到端回归（真机实测抓到的缺陷）：平台总开关关闭后，**带产品前缀**的
    /// 请求必须 400 `platform_not_in_routing`，而不是照常返回上游内容。
    ///
    /// 判据最初只写在「选不出号」分支里，而那时选号侧也没有平台判据 ——
    /// 于是关掉 qoder 后 `qoder:国服:GLM-5.3` 实测仍 200。现在两处都补齐了，
    /// 这条用例锁住「即使选得出号也必须先被平台闸拦下」。
    #[tokio::test]
    async fn platform_off_rejects_product_prefixed_request_before_picking() {
        let p = pool_with_products(&[("q1", "qoder")]);
        let client = Client::new(ClientConfig::default()).unwrap();
        let products = crate::products::ProductUpstreams::new();
        let (unusual, waf_ip, empty_stream) = fresh_breakers();
        let mut c = ctx(&p, &client, &products, &unusual, &waf_ip, &empty_stream);
        p.lock()
            .unwrap()
            .set_platform_enabled(&std::collections::HashMap::from([(
                "qoder".to_string(),
                false,
            )]));

        let (_, status, fail) = forward_chat(
            &mut c,
            br#"{"model":"qoder:glm-5.3"}"#,
            false,
            "",
            ImageRoute::any(),
            "qoder",
        )
        .await;
        assert_eq!(status, 400, "平台关闭必须是 400（不是 503「稍后重试」）");
        let f = fail.expect("应返回失败");
        assert_eq!(f.kind, FailureKind::PlatformNotInRouting);
        // ★ 文案必须给出**正确的出路**（去账号页打开开关），且明确「不影响养号任务」——
        // 否则用户会以为关掉平台连签到都不跑了，或者去白名单页瞎改。
        assert!(f.message.contains("参加兼容网关路由"), "{}", f.message);
        assert!(f.message.contains("不影响养号任务"), "{}", f.message);
        // ★ 平台闸是**平台级**语义：绝不能顺手改账号级的任何标记。
        //（关掉平台只表示「不参加选号」，账号本身既没被禁用、也没被设 no_route。）
        let list = p.lock().unwrap().list();
        assert_eq!(list.len(), 1);
        assert!(!list[0].disabled, "平台开关不得写进账号 disabled");
        assert!(!list[0].no_route, "平台开关不得写进账号 no_route");
    }

    /// 反向保证：开关**没配**或显式 `true` 时，带产品前缀的请求照常走选号。
    #[tokio::test]
    async fn platform_on_does_not_block_product_prefixed_request() {
        let p = pool_with_products(&[("q1", "qoder")]);
        let client = Client::new(ClientConfig::default()).unwrap();
        let products = crate::products::ProductUpstreams::new();
        let (unusual, waf_ip, empty_stream) = fresh_breakers();
        let mut c = ctx(&p, &client, &products, &unusual, &waf_ip, &empty_stream);

        let (_, status, fail) = forward_chat(
            &mut c,
            br#"{"model":"qoder:glm-5.3"}"#,
            false,
            "",
            ImageRoute::any(),
            "qoder",
        )
        .await;
        // 没有真实上游 ⇒ 最终会以 503 落定，但**绝不能**是平台闸的 400。
        assert_ne!(status, 400, "未关闭平台时不该被平台闸拦下");
        if let Some(f) = fail {
            assert_ne!(f.kind, FailureKind::PlatformNotInRouting);
        }
    }

    /// 产品前缀请求**不得**跨平台兜底：池里只有 workbuddy 账号时，
    /// `qoder:xxx` 必须选不出号（否则用户会看到「我指定了 qoder，却报 ZCode 的错」）。
    #[test]
    fn product_prefix_never_crosses_products() {
        let p = pool_with(&["wb1"]);
        assert!(
            p.lock()
                .unwrap()
                .pick_for_model_product_region("glm-5.3", &[], Region::Any, "qoder")
                .is_none(),
            "指定 qoder 时不得选中 workbuddy 账号"
        );
        // 无前缀（空产品）时行为不变，仍能选出号
        assert!(
            p.lock()
                .unwrap()
                .pick_for_model_product_region("glm-5.3", &[], Region::Any, "")
                .is_some(),
            "无产品前缀时应沿用原有区域逻辑"
        );
    }

    /// 粘性会话绑定的账号若产品不符，必须解绑换号 —— 否则 `zcode:glm-5.3`
    /// 会继续用此前绑定的 workbuddy 账号（实测确认过这个漏洞）。
    #[test]
    fn sticky_binding_is_validated_against_product() {
        let p = pool_with(&["wb1"]);
        assert!(
            p.lock()
                .unwrap()
                .pick_by_uid_for_model_product_region("wb1", "glm-5.3", Region::Any, "zcode")
                .is_none(),
            "粘性路径也必须校验产品"
        );
        // 产品相符时正常命中
        assert!(
            p.lock()
                .unwrap()
                .pick_by_uid_for_model_product_region("wb1", "glm-5.3", Region::Any, "workbuddy")
                .is_some(),
            "产品相符时应命中"
        );
    }

    /// 模型级冷却只影响该模型，账号其他模型仍可用（这是本特性的全部意义）。
    #[test]
    fn model_cooldown_is_per_model() {
        let p = pool_with(&["u1"]);
        p.lock().unwrap().cooldown_model(
            "u1",
            "glm-5.3",
            SystemTime::now() + Duration::from_secs(3600),
            "限流",
        );
        // 该模型被过滤
        assert!(p.lock().unwrap().pick_for_model("glm-5.3", &[]).is_none());
        // 其他模型仍可选
        assert!(p.lock().unwrap().pick_for_model("glm-4", &[]).is_some());
        // 账号级健康不受影响（不显示为冷却）
        let st = p.lock().unwrap().list();
        assert!(!st[0].cooling, "模型冷却不应把账号标记为冷却");
    }

    /// 模型级冷却不得喂熔断器：否则会连带封掉整个账号。
    #[test]
    fn model_cooldown_does_not_feed_breaker() {
        let p = pool_with(&["u1"]);
        p.lock().unwrap().set_breaker(3, Duration::from_secs(60), Duration::from_secs(600));
        for _ in 0..5 {
            p.lock().unwrap().cooldown_model(
                "u1",
                "glm-5.3",
                SystemTime::now() + Duration::from_secs(60),
                "限流",
            );
        }
        // 熔断计数未被喂入 → 其他模型依然可用
        assert!(
            p.lock().unwrap().pick_for_model("glm-4", &[]).is_some(),
            "模型冷却不应触发账号熔断"
        );
    }

    #[test]
    fn apply_error_policy_routes_kinds() {
        // HardCredit → 账号级硬冷却
        let p = pool_with(&["u1"]);
        apply_error_policy(
            &mut p.lock().unwrap(),
            "u1",
            "m",
            ErrKind::HardCredit,
            "",
            Duration::from_secs(60),
        );
        assert!(p.lock().unwrap().list()[0].cooling);

        // SessionDead → 禁用
        let p2 = pool_with(&["u1"]);
        apply_error_policy(
            &mut p2.lock().unwrap(),
            "u1",
            "m",
            ErrKind::SessionDead,
            "",
            Duration::from_secs(60),
        );
        assert!(p2.lock().unwrap().list()[0].disabled);

        // Client → 只换号不罚
        let p3 = pool_with(&["u1"]);
        apply_error_policy(
            &mut p3.lock().unwrap(),
            "u1",
            "m",
            ErrKind::Client,
            "",
            Duration::from_secs(60),
        );
        let st = p3.lock().unwrap().list();
        assert!(!st[0].cooling && !st[0].disabled, "Client 不应处罚账号");
    }

    /// 模型限流按上游给的重置时间冷却（而不是固定软冷却）。
    #[test]
    fn apply_error_policy_model_rate_uses_reset_time() {
        let p = pool_with(&["u1"]);
        // 构造一个未来的重置时刻（UTC+8），确保解析成功
        let future = chrono::Utc::now() + chrono::Duration::hours(3);
        let cn = future + chrono::Duration::hours(8);
        let body = format!(
            "{{\"code\":6004,\"msg\":\"您的使用量已超出频率限制，将在 {} UTC+8 重置，您也可以切换其他模型继续使用。\"}}",
            cn.format("%Y-%m-%d %H:%M:%S")
        );
        apply_error_policy(
            &mut p.lock().unwrap(),
            "u1",
            "glm-5.3",
            ErrKind::ModelRate,
            &body,
            Duration::from_secs(60),
        );
        let cooling = p.lock().unwrap().cooling_models("u1");
        assert_eq!(cooling.len(), 1, "应有 1 个模型冷却: {cooling:?}");
        assert_eq!(cooling[0].0, "glm-5.3");
        // 冷却时长应接近 3 小时，而不是 60 秒（证明用了上游重置时间）
        assert!(
            cooling[0].1 > 3600 * 2,
            "冷却应接近上游重置时间而非固定软冷却，实际 {} 秒",
            cooling[0].1
        );
    }

    #[test]
    fn image_region_message_names_region() {
        assert!(image_region_unavailable_message(Region::CN).contains("国服"));
        assert!(image_region_unavailable_message(Region::Intl).contains("国际版"));
    }

    /// 白名单非空时：名单外的模型必须 400，且不消耗任何账号。
    ///
    /// 错误码必须是 `model_not_allowed`（不是 `no_healthy_account`）——
    /// 后者会把用户引去查账号池，而账号一个都没问题。
    #[tokio::test]
    async fn allowed_model_lock_rejects_other_models() {
        let p = pool_with(&["u1"]);
        let client = Client::new(ClientConfig::default()).unwrap();
        let products = crate::products::ProductUpstreams::new();
        let (unusual, waf_ip, empty_stream) = fresh_breakers();
        let mut c = ctx(&p, &client, &products, &unusual, &waf_ip, &empty_stream);
        c.allowed_model = vec!["glm-5.3".to_string()];

        let (_, status, fail) = forward_chat(
            &mut c,
            br#"{"model":"other"}"#,
            false,
            "",
            ImageRoute::any(),
            "",
        )
        .await;
        assert_eq!(status, 400);
        let f = fail.expect("应返回失败");
        assert_eq!(f.kind, FailureKind::ModelNotAllowed);
        assert!(f.message.contains("glm-5.3"), "{}", f.message);
        assert!(f.message.contains("other"), "{}", f.message);
        // ★ 关键：绝不能是「账号不可用」那类文案。
        assert!(!f.message.contains("账号"), "{}", f.message);
    }

    /// 白名单内的模型**放行**（继续走选号）—— 且带前缀的请求也算命中。
    ///
    /// 这条是反向保证：只有「拒绝」被实现而「放行」被写坏时，上面的用例仍会通过。
    #[tokio::test]
    async fn allowed_model_permits_listed_models() {
        for req in [
            &br#"{"model":"glm-5.3"}"#[..],
            &br#"{"model":"GLM-5.3"}"#[..],
            // 客户端带区域/产品前缀指定平台时，仍应命中裸名白名单 ——
            // 否则用户「明明放行了它」却被拒，是最难自查的一类误拒。
            &br#"{"model":"cn:glm-5.3"}"#[..],
            &br#"{"model":"qoder:glm-5.3"}"#[..],
        ] {
            let p = pool_with(&["u1"]);
            let client = Client::new(ClientConfig::default()).unwrap();
            let products = crate::products::ProductUpstreams::new();
            let (unusual, waf_ip, empty_stream) = fresh_breakers();
            let mut c = ctx(&p, &client, &products, &unusual, &waf_ip, &empty_stream);
            c.allowed_model = vec!["glm-5.3".to_string()];

            let (_, status, fail) = forward_chat(
                &mut c,
                req,
                false,
                "",
                ImageRoute::any(),
                "",
            )
            .await;
            // 放行后没有真实上游 ⇒ 会以 503 落定，但**绝不能是 400 白名单拒绝**。
            assert_ne!(status, 400, "请求 {req:?} 不该被白名单拒");
            if let Some(f) = fail {
                assert_ne!(f.kind, FailureKind::ModelNotAllowed, "请求 {req:?}");
            }
        }
    }

    /// 空白名单 = 不限制（向后兼容：老配置没有这个键时必须放行一切）。
    #[tokio::test]
    async fn empty_allowed_model_is_unrestricted() {
        let p = pool_with(&["u1"]);
        let client = Client::new(ClientConfig::default()).unwrap();
        let products = crate::products::ProductUpstreams::new();
        let (unusual, waf_ip, empty_stream) = fresh_breakers();
        let mut c = ctx(&p, &client, &products, &unusual, &waf_ip, &empty_stream);
        assert!(c.allowed_model.is_empty());

        let (_, status, fail) = forward_chat(
            &mut c,
            br#"{"model":"anything-at-all"}"#,
            false,
            "",
            ImageRoute::any(),
            "",
        )
        .await;
        assert_ne!(status, 400);
        if let Some(f) = fail {
            assert_ne!(f.kind, FailureKind::ModelNotAllowed);
        }
    }

    /// 无可用账号 → 503 no_healthy_account 契约不变。
    #[tokio::test]
    async fn no_accounts_returns_503() {
        let p = Mutex::new(Pool::new(String::new()));
        let client = Client::new(ClientConfig::default()).unwrap();
        let products = crate::products::ProductUpstreams::new();
        let (unusual, waf_ip, empty_stream) = fresh_breakers();
        let mut c = ctx(&p, &client, &products, &unusual, &waf_ip, &empty_stream);
        let (_, status, fail) = forward_chat(
            &mut c,
            br#"{"model":"m"}"#,
            false,
            "",
            ImageRoute::any(),
            "",
        )
        .await;
        assert_eq!(status, 503);
        let f = fail.expect("应返回失败");
        assert!(
            f.message.contains("all accounts unavailable"),
            "{}",
            f.message
        );
    }

    /// 区域受限且该区域无账号 → 明确报错，**不跨区降级**。
    #[tokio::test]
    async fn strict_region_without_accounts_reports_clearly() {
        // 池里只有国服账号，却要求国际版
        let p = pool_with(&["cn1"]);
        let client = Client::new(ClientConfig::default()).unwrap();
        let products = crate::products::ProductUpstreams::new();
        let (unusual, waf_ip, empty_stream) = fresh_breakers();
        let mut c = ctx(&p, &client, &products, &unusual, &waf_ip, &empty_stream);
        let route = ImageRoute {
            region: Region::Intl,
            required: true,
        };
        let (_, status, fail) = forward_chat(
            &mut c,
            br#"{"model":"glm-5.3","messages":[{"content":[{"type":"image_url","image_url":{"url":"x"}}]}]}"#,
            false,
            "",
            route,
            "",
        )
        .await;
        assert_eq!(status, 503);
        let f = fail.expect("应返回失败");
        assert_eq!(f.kind, FailureKind::ImageRegionUnavailable);
        assert!(f.message.contains("国际版"), "{}", f.message);
    }

    /// 偏好区域（required=false）：该区域没号时放开全池，而不是失败。
    #[test]
    fn prefer_region_falls_back_to_whole_pool() {
        let p = pool_with(&["cn1"]);
        // 偏好国际版，但池里只有国服 → 仍能选出号
        assert!(
            p.lock().unwrap().pick_for_model_region("m", &[], Region::Intl, false).is_some(),
            "偏好语义下应放开全池"
        );
        // 强制国际版 → 选不出
        assert!(
            p.lock().unwrap().pick_for_model_region("m", &[], Region::Intl, true).is_none(),
            "强制语义下不得跨区"
        );
    }

    #[test]
    fn region_of_derives_from_domain() {
        let cn = Auth {
            domain: "https://copilot.tencent.com".into(),
            ..Default::default()
        };
        let intl = Auth {
            domain: "https://www.workbuddy.ai".into(),
            ..Default::default()
        };
        assert_eq!(region_of(&cn), Region::CN);
        assert_eq!(region_of(&intl), Region::Intl);
    }

    /// ★★ 成本维度**真的接到了选号上**（不是「注入了但没人读」）。
    ///
    /// 现场（所有者 2026-09-23）：「deepseek-v4.1-flash 在国际版是 free 0 倍率，
    /// 国内是 0.03 … 这时候就应该优先路由国际账号」。
    ///
    /// 这条用例覆盖的正是**两段接线**：
    ///   ① `inject_cost_rates` 把 (账号区域, 模型) 的倍率灌进池子；
    ///   ② `pick_inner` 的「免费优先」`free_tier` 与权重乘子真的读它。
    ///
    /// 判据用**确定性**的那一段（免费优先），不用加权抽签：权重只是「倾斜」，
    /// 抽签仍有概率落到贵号上；`free_tier` 则是硬收窄 —— 只要它生效，
    /// 国际号就必然被选中。这也正是「免费优先会静默失效」最容易翻车的地方：
    /// 少传 `known=true` 时 `cost_is_free` 恒 false，本用例会立刻变红。
    #[test]
    fn inject_cost_rates_routes_bare_model_to_the_free_region() {
        let cache = crate::models_cache::ModelsCache::new();
        let now = SystemTime::now();
        // 同一个裸名：国际版**声明免费**（0），国服 0.03。
        cache.record_success(
            Region::Intl,
            vec![
                crate::upstream::client::ModelInfo {
                    id: "deepseek-v4.1-flash".into(),
                    credit_multiplier: Some(0.0),
                    ..Default::default()
                },
            ],
            now,
        );
        cache.record_success(
            Region::CN,
            vec![
                crate::upstream::client::ModelInfo {
                    id: "deepseek-v4.1-flash".into(),
                    credit_multiplier: Some(0.03),
                    ..Default::default()
                },
            ],
            now,
        );

        let mut pool = Pool::new(String::new());
        // 多产品必须开启，成本维度才会介入（`free_tier` 的第一道闸门）。
        pool.set_multi_product(true, 0.3);
        pool.add(Auth {
            uid: "cn1".into(),
            access_token: "at".into(),
            domain: "https://copilot.tencent.com".into(),
            ..Default::default()
        });
        pool.add(Auth {
            uid: "intl1".into(),
            access_token: "at".into(),
            domain: "https://www.workbuddy.ai".into(),
            ..Default::default()
        });
        let p = Mutex::new(pool);
        let client = Client::new(ClientConfig::default()).unwrap();
        let products = crate::products::ProductUpstreams::new();
        let (unusual, waf_ip, empty_stream) = fresh_breakers();
        let mut c = ctx(&p, &client, &products, &unusual, &waf_ip, &empty_stream);
        c.models_cache = Some(&cache);

        inject_cost_rates(&c, "deepseek-v4.1-flash");
        {
            let g = p.lock().unwrap();
            assert!(g.cost_is_free("intl1"), "国际版声明 0 倍率 ⇒ 必须判为免费");
            assert!(!g.cost_is_free("cn1"), "国服 0.03 ⇒ 不是免费");
        }

        // ★ 抽 200 次：只要免费优先生效，就不可能抽到国服号。
        for _ in 0..200 {
            let a = p
                .lock()
                .unwrap()
                .pick_for_model_any("deepseek-v4.1-flash", &[])
                .unwrap();
            assert_eq!(a.uid, "intl1", "裸名必须优先路由到免费的国际版账号");
        }

        // 大小写不敏感：上游与客户端对同一个模型的大小写并不一致。
        inject_cost_rates(&c, "DeepSeek-V4.1-Flash");
        assert!(p.lock().unwrap().cost_is_free("intl1"));
    }

    /// ★ 未声明（`credit_multiplier = None`）**不是免费**，成本维度不得介入。
    ///
    /// 反向保证：把「不知道收不收费」当成免费，会让某个还没查到倍率的平台
    /// 抢走全部流量 —— 而那些平台的倍率表恰恰是最后才补齐的。
    #[test]
    fn inject_cost_rates_treats_missing_multiplier_as_unknown() {
        let cache = crate::models_cache::ModelsCache::new();
        cache.record_success(
            Region::Intl,
            vec![crate::upstream::client::ModelInfo {
                id: "deepseek-v4.1-flash".into(),
                credit_multiplier: None,
                ..Default::default()
            }],
            SystemTime::now(),
        );
        let mut pool = Pool::new(String::new());
        pool.set_multi_product(true, 0.3);
        pool.add(Auth {
            uid: "intl1".into(),
            access_token: "at".into(),
            domain: "https://www.workbuddy.ai".into(),
            ..Default::default()
        });
        let p = Mutex::new(pool);
        let client = Client::new(ClientConfig::default()).unwrap();
        let products = crate::products::ProductUpstreams::new();
        let (unusual, waf_ip, empty_stream) = fresh_breakers();
        let mut c = ctx(&p, &client, &products, &unusual, &waf_ip, &empty_stream);
        c.models_cache = Some(&cache);

        inject_cost_rates(&c, "deepseek-v4.1-flash");
        assert!(
            !p.lock().unwrap().cost_is_free("intl1"),
            "未声明倍率不得被当成免费"
        );
    }

    /// ★★ 业主现场回归：预算是「输入 + max_tokens ≤ 1,048,576」。
    ///
    /// 守的是「DSH 只按输入估算 ⇒ 以为还有余量 ⇒ 不压缩 ⇒ 永久卡死」。
    #[test]
    fn budget_guess_counts_max_tokens_against_the_input() {
        // 业主现场的算术：输入贴着上限，max_tokens 又给满。
        let input = UPSTREAM_REQUEST_TOKEN_BUDGET - 6_000; // 已超 60% 门槛
        // 造一个能估出 ~input token 的 body：CJK ≈ 2 字符/token
        let cjk = "测试文本内容".repeat((input as usize) * 2 / 6);
        let body = serde_json::json!({
            "model": "deepseek-v4.1-flash",
            "max_tokens": 16,
            "messages": [{ "role": "user", "content": cjk }],
        })
        .to_string();
        let cache = crate::models_cache::ModelsCache::new();
        cache.record_success(
            crate::auth::Region::Intl,
            vec![crate::upstream::ModelInfo {
                id: "deepseek-v4.1-flash".into(),
                ..Default::default()
            }],
            std::time::SystemTime::now(),
        );
        let small_mt = budget_exceeded_guess(body.as_bytes(), "deepseek-v4.1-flash", &cache);
        assert_eq!(small_mt, Some(false), "小 max_tokens 时不该判超限");
        // 同一输入 + 128000 输出预算 ⇒ 越界（DSH 实际发的是这个）
        let big = serde_json::json!({
            "model": "deepseek-v4.1-flash",
            "max_tokens": 128000,
            "messages": [{ "role": "user", "content": cjk }],
        })
        .to_string();
        assert_eq!(
            budget_exceeded_guess(big.as_bytes(), "deepseek-v4.1-flash", &cache),
            Some(true),
            "同一输入 + 128000 输出预算就该判超限 —— 这正是业主卡死的那一发"
        );
    }

    /// 小请求 + **正常**的 `max_tokens` ⇒ 不判超限（从没失败过的那类）。
    ///
    /// ★★ 2026-10-09 修正两次，全过程值得记住：
    ///
    /// 1. 这条测试原用 `max_tokens: 128000` 断言 `Some(false)` ——
    ///    而业主 VS Code 现场证明**那个形状真的会 11133**（「把上下文设置调得
    ///    很大」就稳定复现、调小就好了）⇒ 旧断言把一个真实故障形状钉成了
    ///    「不该判超限」。
    /// 2. 我随后加了「`max_tokens` 自己超上限 ⇒ true」的例外，但**保留了**
    ///    「输入须 > 60%」的闸门 —— 而业主的请求是**小输入 + 极大 max_tokens**，
    ///    `5,000 + 1,048,576 = 1,053,576 > 上限` 公式成立却被闸门放行。
    ///    闸门挡掉的正是它该抓的那类。现已**整条移除**，直接用实测公式。
    ///
    /// 本条现在守的是：**输入与输出加起来也没超上限**的小请求 ⇒ `Some(false)`。
    #[test]
    fn budget_guess_is_inert_for_small_requests() {
        let cache = crate::models_cache::ModelsCache::new();
        cache.record_success(
            crate::auth::Region::Intl,
            vec![crate::upstream::ModelInfo {
                id: "m".into(),
                ..Default::default()
            }],
            std::time::SystemTime::now(),
        );
        // 128000 远小于上限，且输入极小 ⇒ 加起来仍远小于上限。
        let body = r#"{"model":"m","max_tokens":128000,"messages":[{"role":"user","content":"hi"}]}"#;
        assert_eq!(
            budget_exceeded_guess(body.as_bytes(), "m", &cache),
            Some(false),
            "输入 + max_tokens 都没超上限 ⇒ 不该判溢出"
        );
    }

    /// ★★ 业主 VS Code 现场：**输入很小，但 `max_tokens` 自己就超过上限**。
    ///
    /// 他「把上下文设置调得很大」就稳定复现 11133、调小就好了 —— 说明那种请求
    /// 的形状是「输入小 + 输出预算极大」。此时旧判据被 60% 输入门槛挡在门外、
    /// 返回 `Some(false)` ⇒ 客户端只看到「参数被拒」，不知道是预算问题。
    ///
    /// 判据：`max_tokens > 上限` ⇒ 无条件 `Some(true)`
    ///（`输入 >= 0` ⇒ `输入 + max_tokens >= max_tokens > 上限`，无需估算输入）。
    #[test]
    fn huge_max_tokens_alone_means_over_budget() {
        let cache = crate::models_cache::ModelsCache::new();
        cache.record_success(
            crate::auth::Region::Intl,
            vec![crate::upstream::ModelInfo {
                id: "m".into(),
                ..Default::default()
            }],
            std::time::SystemTime::now(),
        );
        // 输入极小（"hi" ≈ 1 token），但 max_tokens 自己就超了 1,048,576。
        let body = format!(
            r#"{{"model":"m","max_tokens":{},"messages":[{{"role":"user","content":"hi"}}]}}"#,
            UPSTREAM_REQUEST_TOKEN_BUDGET + 1
        );
        assert_eq!(
            budget_exceeded_guess(body.as_bytes(), "m", &cache),
            Some(true),
            "max_tokens 自己就超过上限 ⇒ 必然超预算，与输入多小无关"
        );
        // ★ 注意 `estimate_tokens` 估算的是**整个 body**（含 JSON 骨架、模型名、
        // 字段名），所以「输入」**永远不为 0** —— 不存在「正好等于上限」的
        // 可构造边界。`max_tokens` 一旦顶到上限，合计就必然**严格大于**上限。
        let at_budget = format!(
            r#"{{"model":"m","max_tokens":{},"messages":[]}}"#,
            UPSTREAM_REQUEST_TOKEN_BUDGET
        );
        assert_eq!(
            budget_exceeded_guess(at_budget.as_bytes(), "m", &cache),
            Some(true),
            "max_tokens 顶到上限，加上 body 自身的 token 就必然超"
        );
        // 严格大于的另一侧：`max_tokens` 与输入都极小 ⇒ 远低于上限，不判超。
        //（实测 `1,042,398 + 16 = 1,042,414 < 上限` 是 200，同理。）
        let small = r#"{"model":"m","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#;
        assert_eq!(
            budget_exceeded_guess(small.as_bytes(), "m", &cache),
            Some(false),
            "小输入 + 小 max_tokens ⇒ 远低于上限，不判溢出"
        );
    }

    /// 模型不在真值清单里 ⇒ `None`（不猜）。
    #[test]
    fn budget_guess_is_unknown_for_unknown_model() {
        let cache = crate::models_cache::ModelsCache::new();
        let body = r#"{"model":"nope","max_tokens":128000,"messages":[]}"#;
        assert_eq!(budget_exceeded_guess(body.as_bytes(), "nope", &cache), None);
    }

    /// `max_tokens` 缺失按 0 计（不为「不知道」扣预算）。
    #[test]
    fn max_tokens_missing_counts_as_zero() {
        assert_eq!(max_tokens_of(br#"{"model":"m"}"#), None);
        assert_eq!(max_tokens_of(br#"{"maxTokens":42}"#), Some(42));
        assert_eq!(max_tokens_of(b"not json"), None);
    }

    /// 估算函数：CJK ≈ 2 字符/token，ASCII ≈ 4 字符/token，且**向上取整**。
    #[test]
    fn estimate_tokens_matches_measured_ratios() {
        // 实测：3000 CJK 字符 → 150210-1502... 见 doc 表（≈2 字符/token）
        assert_eq!(estimate_tokens("测试文本内容".repeat(500).as_bytes()), 1500);
        // 纯 ASCII：4 字符/token，向上取整
        assert_eq!(estimate_tokens(b"abcd"), 1);
        assert_eq!(estimate_tokens(b"abcde"), 2);
        assert_eq!(estimate_tokens(b""), 0);
    }
}
