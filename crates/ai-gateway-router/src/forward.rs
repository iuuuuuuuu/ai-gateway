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
//! - 请求被上游拒绝（11155 思维链缺失、11140 安全审核）→ **原地重试**，
//!   预算 2 次；用尽才按请求侧错误落定
//!
//! 最后两类是改变对外状态码的路径；其余失败沿用 503 `no_healthy_account`
//! 契约（语义是「账号池暂时不可用，稍后重试」）。
//!
//! # 为什么「请求被拒」要重试（实测数据，勿凭直觉回退）
//!
//! 实测 11140 在同一请求体上按 **~10~25%** 概率**随机**出现，与账号、模型、
//! 协议都无关（会话粘性钉住单账号仍随机；hy3 与 deepseek 失败率相同；
//! Chat 与 Responses 失败率相同；连「列出三原色」这种无害纯文本也 20 次挂 2 次）。
//!
//! 同一份 body 连发 40 次：首轮成功 30 次，把 10 次失败原样重发（最多 2 次）
//! 后 **10/10 全部救回，0 次重试仍失败** —— 单请求成功率 75% → 100%。
//!
//! 结论：上游审核判定带服务端抖动，**不是**确定的请求侧错误。原实现按
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
    /// 请求上下文超出模型窗口：请求侧错误，换号无用。
    ContextTooLong,
    /// 请求内容被上游拒绝（11155 思维链缺失 / 11140 安全审核）：
    /// 已用尽重试预算，按请求侧错误落定，原样回传上游状态与文案。
    RequestRejected,
    /// 带图片的请求需要特定区域的账号，而该区域此刻没有可用账号。
    ImageRegionUnavailable,
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
        /// 上游 SSE 响应流。
        response: reqwest::Response,
    },
    /// 非流式：上游 SSE 已被聚合成 OpenAI `chat.completion`。
    Response {
        /// 选中的账号 UID。
        uid: String,
        /// 请求的目标模型。
        model: String,
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
    /// 「单一模型」锁定；非空时只放行该模型。
    pub allowed_model: String,
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

/// 执行「选号 → 刷新 token → 转发 → 失败换号」的完整轮转。
///
/// - `body`：已转换成 OpenAI Chat 形态的请求体（原始字节；发往上游前由
///   `Client::prepare_body` 再做一次改写：强制 stream、归一化 role/tool_choice）
/// - `stream`：调用方是否要求流式。上游恒为流式，非流式时本函数读完后聚合。
/// - `sess_key`：会话粘性键；空串表示不做粘性绑定。
/// - `route`：区域路由约束（带图片请求用）。
///
/// 返回 `(结果, 状态码, 失败)`；成功时失败为 `None`。
pub async fn forward_chat(
    ctx: &mut ForwardCtx<'_>,
    body: &[u8],
    stream: bool,
    sess_key: &str,
    route: ImageRoute,
) -> (Option<ChatResult>, u16, Option<ForwardFailure>) {
    let mut tried: Vec<String> = Vec::new();
    let mut last_uid = String::new();
    let mut last_status: u16 = 503;
    let mut last_err: Option<String> = None;
    let mut last_kind = ErrKind::None;
    let mut last_body = String::new();
    let mut last_transport_err: Option<String> = None;

    // 请求的目标模型：用于模型级冷却的选号过滤与记账。
    let model = model_of(body);

    // 「单一模型」锁定：非空时只放行该模型。
    //
    // 为什么在选号之前就拒绝（而不是换个模型重试）：轮转模式的语义是
    // 「把这个账号的指定模型额度烧干净再换号」，模型是策略的一部分。
    // 若允许其他模型通过，客户端换个模型就能绕过轮转与额度控制。
    if !ctx.allowed_model.is_empty() && !model.eq_ignore_ascii_case(&ctx.allowed_model) {
        let got = if model.is_empty() { "（未指定）" } else { &model };
        return (
            Some(ChatResult::Response {
                uid: String::new(),
                model: model.clone(),
                body: serde_json::Map::new(),
            }),
            400,
            Some(ForwardFailure {
                kind: FailureKind::Upstream,
                status: 400,
                message: format!(
                    "当前为「单一模型」模式，只允许调用 {}；收到的是 {got}。请在客户端把模型改为 {}，或切换网关的工作模式。",
                    ctx.allowed_model, ctx.allowed_model
                ),
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
    for _ in 0..(ctx.max_rotate.max(1) + REJECT_RETRY_BUDGET) {
        // ── 选号：优先粘性命中，其次按模型 + 区域挑（短暂持锁）──
        let mut acct: Option<Auth> = None;
        if let Some(uid) = sticky_uid.clone() {
            acct = lock_pool(ctx.pool)
                .pick_by_uid_for_model_region(&uid, &model, route.region);
            if acct.is_none() {
                // 绑定号已冷却/占满/模型限流 → 解绑，落入普通选号。
                if let Some(s) = ctx.session {
                    s.unbind(sess_key);
                }
                sticky_uid = None;
            }
        }
        if acct.is_none() {
            let mut pool = lock_pool(ctx.pool);
            acct = if route.region == Region::Any {
                pool.pick_for_model(&model, &tried)
            } else {
                pool.pick_for_model_region(&model, &tried, route.region, route.required)
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

        // ── token 临近过期 → 先刷新（失败则冷却换号）──
        // 注意：刷新是网络调用，**不持锁**；仅在其失败时短暂加锁记状态。
        let mut acct = acct;
        if acct.needs_refresh(ctx.refresh_skew) {
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
                eprintln!("chat refresh uid={}: save auth failed: {e}", acct.uid);
            }
        }

        // ── 转发上游（网络等待，不持锁）──
        let outcome = match ctx.client.chat_stream(&acct, body).await {
            Ok(o) => o,
            Err(e) => {
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
        };

        match outcome {
            ChatOutcome::Failed { status, body: resp_body } => {
                let kind = classify::classify(status, &resp_body);
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

                // 请求侧错误：换号**通常**无用（同一请求体发给任何账号都同样
                // 失败）。但「通常」不足以支撑直接失败 —— 实测 11140 / 11155 都带
                // 上游侧抖动，同一请求体重发即可成功（见循环上方的实测数据）。
                //
                // 因此这里先消耗重试预算**原地重试**；预算用尽才按请求侧错误
                // 落定。上下文超长不在此列：它是确定性的，重试纯属浪费。
                if kind == ErrKind::RequestRejected && reject_retries_left > 0 {
                    reject_retries_left -= 1;
                    // 记一行再重试：上游抖动是**唯一**只能靠日志发现的故障模式，
                    // 不记的话「重试救回来了」与「上游本来就没抖」无法区分，
                    // 抖动恶化到重试也压不住时没有任何先兆。
                    eprintln!(
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
                if kind == ErrKind::ContextTooLong || kind == ErrKind::RequestRejected {
                    let (fk, message) = if kind == ErrKind::ContextTooLong {
                        (
                            FailureKind::ContextTooLong,
                            // 保留上游原文：下游客户端靠文案识别上下文溢出
                            // 并触发自动压缩，只回我们自己的措辞会让它认不出。
                            context_too_long_message(&resp_body),
                        )
                    } else {
                        (
                            FailureKind::RequestRejected,
                            request_rejected_message(&resp_body),
                        )
                    };
                    let uid = acct.uid.clone();
                    release(ctx.pool, &mut held_uid);
                    return (
                        Some(ChatResult::Response {
                            uid,
                            model: model.clone(),
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
                lock_pool(ctx.pool).note_success(&acct.uid);
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

                if stream {
                    return (
                        Some(ChatResult::Stream {
                            uid,
                            model,
                            response: resp,
                        }),
                        200,
                        None,
                    );
                }

                // 非流式：读完整流后聚合。
                let text = match resp.text().await {
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
                lock_pool(ctx.pool).release(&uid);
                match sse::aggregate(&text) {
                    Ok(resp_body) => {
                        return (
                            Some(ChatResult::Response {
                                uid,
                                model,
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

    let mut msg = "all accounts unavailable (cooling/disabled)".to_string();
    if last_err.is_some() {
        // 客户端可读性：优先用提炼后的原因，拿不到再用原始文案 ——
        // 原始文案含整段上游 JSON 或底层 tcp 细节，又长又难懂。
        let friendly = friendly_message(last_kind, last_status, &last_body);
        if !friendly.is_empty() {
            msg = friendly;
        } else if let Some(t) = &last_transport_err {
            let _ = t;
            msg = "无法连接上游（网络超时 / 连接被拒）：请检查本机网络或代理设置后重试".to_string();
        } else if let Some(e) = &last_err {
            msg = format!("{msg}: {e}");
        }
    }
    (
        Some(ChatResult::Response {
            uid: last_uid,
            model,
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

/// 按错误分类对账号施加冷却 / 禁用 / 熔断策略。
///
/// 六条路径，各司其职：
/// - `HardCredit` → 冷却到次日 04:00（等签到恢复）
/// - `ModelRate` → **模型级**冷却到上游给的重置时间；账号其他模型不受影响
/// - `SoftRate` / `NotFound` → 账号级软冷却
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
        ErrKind::SessionDead => {
            pool.disable(uid, "12153 session dead");
        }
        ErrKind::NotFound => {
            pool.cooldown(uid, CoolKind::Soft, soft_cooldown, "upstream 404");
        }
        ErrKind::Server => {
            pool.note_error(uid);
        }
        // 请求侧拒绝（11155 / 11140）在轮转循环里已短路返回，理论上走不到这里；
        // 即便到达也绝不罚账号 —— 账号什么都没做错。
        ErrKind::RequestRejected => {}
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

    fn ctx<'a>(pool: &'a Mutex<Pool>, client: &'a Client) -> ForwardCtx<'a> {
        ForwardCtx {
            pool,
            client,
            session: None,
            max_rotate: 3,
            soft_cooldown: Duration::from_secs(60),
            refresh_skew: Duration::from_secs(600),
            allowed_model: String::new(),
        }
    }

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

    /// 单一模型模式：非目标模型必须 400，且不消耗任何账号。
    #[tokio::test]
    async fn allowed_model_lock_rejects_other_models() {
        let p = pool_with(&["u1"]);
        let client = Client::new(ClientConfig::default()).unwrap();
        let mut c = ctx(&p, &client);
        c.allowed_model = "glm-5.3".into();

        let (_, status, fail) = forward_chat(
            &mut c,
            br#"{"model":"other"}"#,
            false,
            "",
            ImageRoute::any(),
        )
        .await;
        assert_eq!(status, 400);
        let f = fail.expect("应返回失败");
        assert!(f.message.contains("单一模型"), "{}", f.message);
        assert!(f.message.contains("glm-5.3"), "{}", f.message);
    }

    /// 无可用账号 → 503 no_healthy_account 契约不变。
    #[tokio::test]
    async fn no_accounts_returns_503() {
        let p = Mutex::new(Pool::new(String::new()));
        let client = Client::new(ClientConfig::default()).unwrap();
        let mut c = ctx(&p, &client);
        let (_, status, fail) = forward_chat(
            &mut c,
            br#"{"model":"m"}"#,
            false,
            "",
            ImageRoute::any(),
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
        let mut c = ctx(&p, &client);
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
}
