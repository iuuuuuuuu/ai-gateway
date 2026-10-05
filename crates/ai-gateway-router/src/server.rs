//! OpenAI 兼容 HTTP 接口层。
//!
//! 对应 Go 源文件 `internal/server/handler.go`（路由 / 鉴权 / healthz / status / models）
//! 与 `internal/server/logging.go`（请求级表格日志、TTFB / token 统计）。
//!
//! # 路由表（与 Go `handler.go:307-336` 逐条一致，含方法限定）
//!
//! | 方法   | 路径                                | 鉴权 | 说明                         |
//! |--------|-------------------------------------|------|------------------------------|
//! | POST   | `/v1/chat/completions`              | 是   | 聊天（流式/非流式）          |
//! | POST   | `/v1/responses`                     | 是   | OpenAI Responses 协议        |
//! | POST   | `/responses`                        | 是   | 同上（无版本号别名）         |
//! | POST   | `/v1/messages`                      | 是   | Anthropic Messages 协议      |
//! | POST   | `/messages`                         | 是   | 同上（无版本号别名）         |
//! | GET    | `/v1/models`                        | 是   | 模型列表（两区并集）         |
//! | GET    | `/v1/models/regions`                | 是   | **按区域的能力真值**（宿主读）|
//! | GET    | `/status`                           | 是   | 账号池状态                   |
//! | GET    | `/debug/`                           | 是   | 调试视图索引                 |
//! | GET    | `/debug/capability`                 | 是   | 能力索引 / 两区真值对比      |
//! | POST   | `/debug/accounts/:uid/revive`       | 是   | 复活被系统禁用的账号         |
//! | GET    | `/usage`                            | 是   | Token 用量聚合快照           |
//! | POST   | `/tasks/run`                        | 是   | 手动触发养号任务             |
//! | POST   | `/tasks/growth`                     | 是   | 成长任务（list/run/run-all） |
//! | POST   | `/login/sms/send`                   | 是   | 短信登录：发验证码           |
//! | POST   | `/login/sms/verify`                 | 是   | 短信登录：换 token 并登记账号 |
//! | GET    | `/healthz`                          | **否** | 探活（宿主识别用）          |
//!
//! `/healthz` 恒无鉴权：负载均衡/编排探活只需 2xx/503 语义，
//! 身份靠 `service` 字段 + `X-Service` 头双保险。
//!
//! ★ 除 `/healthz` 外**全部**走鉴权，包括 `/debug/*` 与短信两条：
//! `/debug/*` 透出账号 UID 与域名；`/debug/accounts/:uid/revive` 改变选号结果
//! （把自动禁用的号放回流量池）；`/login/sms/*` 会**向真实手机号发短信**
//! （可能触发上游频控甚至计费）。未鉴权暴露等于给任何人一个刷短信/刷活跃度的开关。
//! 这三条在 Go 侧 `handler.go:317-334` 都带 `withAuth`，注释里逐条写明了理由。

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::pool::{Pool, Region};

/// 网关身份标识。
///
/// 经 `/healthz` 响应体 `service` 字段与 `X-Service` 头同时透出：
/// 宿主探测同端口的旧服务/其他服务时，对方即使返回 2xx 也不带本标识，
/// 宿主据此可识别"假成功"。
pub const SERVICE_NAME: &str = "workbuddy2api";

/// 网关共享状态。
pub struct AppState {
    /// 账号池。
    ///
    /// 用 `Arc` 是因为会话粘性路由需要一个「读可用账号列表」的回调，
    /// 而它必须在 `AppState` 之外独立持有池引用（`SessionRouter::new` 时就要给出）。
    pub pool: Arc<Mutex<Pool>>,
    /// 上游客户端（转发用）。
    pub client: crate::upstream::client::Client,
    /// 会话粘性路由；`None` = 不启用。
    pub session: Option<crate::session::Router>,
    /// API Key；空 = 不鉴权。
    pub api_key: String,
    /// 单请求最多换号次数。
    pub max_rotate: usize,
    /// 429 软冷却时长。
    pub soft_cooldown: std::time::Duration,
    /// token 提前刷新窗口。
    pub refresh_skew: std::time::Duration,
    /// Redis 观测模式字符串（`"upstash"` / `"noop"`），供 /status 透出。
    pub redis_mode: String,
    /// 「限制使用的模型」白名单；非空时只放行名单内的模型。
    ///
    /// 由 [`crate::config::Config::normalize`] 归一化后传入（去空白、剥前缀、
    /// 丢空项）。三个工作模式（自动 / 手动 / 积分轮转）共用同一份名单。
    pub allowed_model: Vec<String>,
    /// Token 用量统计（`/usage` 的数据来源）。
    pub usage: Arc<crate::usage::Stats>,
    /// 产品上游（Qoder / ZCode）。
    ///
    /// 未配置的产品是 `None` —— 该产品的账号仍会被池子选中（它们的产品标签
    /// 是真实的），但派发时会**明确失败**而不是错调 WorkBuddy 的端点：
    /// 那是别人的上游，调了必然失败并把这个好账号冷却掉。
    pub products: crate::products::ProductUpstreams,
    /// ★ 3012 熔断（按产品隔离）。
    ///
    /// 进程级共享是**刻意的**：熔断要挡住的正是「一个请求换号继续撞上游」，
    /// 而换号发生在**同一个** `forward_chat` 调用里 —— 但更要紧的是它还得挡住
    /// **下一个**请求（用户重试、agent 连发）。每请求一份实例等于熔断只在
    /// 单次调用内有效，用户每重试一次就再撞一次风控。
    pub unusual: crate::unusual::UnusualBreakers,
    /// ★ 出口 IP 被 WAF 拦的判定窗口（窗口内多个不同账号吃 403）。
    ///
    /// 同样进程级共享：判据是「**多个不同账号**都 403」，而不同账号出现在
    /// **不同请求**里（一个请求内换号也可能，但只靠单请求会判得太迟）。
    pub waf_ip: crate::wafip::WafIpWindow,
    /// 流式首帧预读超时（`upstream.header_timeout_seconds`，未配置回落 30s）。
    pub probe_timeout: std::time::Duration,
    /// ★ 按区域的上游模型清单缓存（`/v1/models`、`/v1/models/regions` 的数据源）。
    ///
    /// 进程级共享，理由与 Go `capability.go:41-60` 的包级 `regionModelCache` 相同：
    /// 「两区真值」是**进程状态**（含失败退避计数），每请求一份实例等于每次请求都
    /// 重新打上游，退避也就完全失效。
    pub models: Arc<crate::models_cache::ModelsCache>,
    /// ★ 可热更新的配置（模型清单 + 系统提示词）。
    ///
    /// 这三项对应 Go 侧 `Handler.dynamic`（`internal/server/handler.go:156-161`）：
    /// 「能：**数据类**字段（模型清单、提示词）—— 它们只是被读取的数据，换掉不影响
    /// 任何已建立的连接或后台任务。不能：监听地址/端口…、Pool / Upstream / 各回调
    ///（持有连接池与后台 goroutine，换掉会泄漏且让在途请求失去归属）。」
    ///
    /// # 为什么必须可变（这是所有者报过的缺陷）
    ///
    /// 宿主在**新增/刷新账号**后会重写 `gateway_native_config.json`（`resync_native_config`
    /// 的三个调用点都**只重写、不重启**），把 `pool.product_models` 换成最新清单。若网关
    /// 只读启动快照，`/v1/models` 的 channels 里就**永远**没有 Qoder / ZCode 的模型 ——
    /// 用户必须手动重启整个应用。Go `handler.go:176-193` 逐字记着这个现场。
    ///
    /// 由 [`crate::cfgwatch::start_config_watch`] 在后台轮询更新；本字段本身是
    /// `Arc`（进程级共享、不可重新赋值），可变性在 [`crate::cfgwatch::DynamicState`]
    /// 内部（`RwLock<Arc<DynamicConfig>>`，读方只做一次 `Arc::clone`）。
    pub dynamic: Arc<crate::cfgwatch::DynamicState>,
    /// 凭证目录（短信登录成功后写 `workbuddy-<uid>.json` 用）。
    ///
    /// 文件名前缀**必须**是 `workbuddy`，否则 [`crate::auth::load_dir`] 的 glob
    /// `workbuddy*.json` 扫不到它 —— 表现为「登录成功但重启后账号不见了」。
    pub auth_dir: String,
    /// ★ 养号任务调度器（`/tasks/run` 的执行体，对应 Go `Config.RunTaskFor`）。
    ///
    /// `None` = 未接线 ⇒ `/tasks/run` 返回 Go 的「未注入」分支 503
    /// `tasks_unavailable`。`bin/gateway.rs` 恒注入，故生产路径非 `None`；
    /// 保留 `Option` 是为了让既有单测（构造 `AppState` 时不关心任务）不必改动。
    pub scheduler: Option<Arc<crate::scheduler::Scheduler>>,
    /// ★ 成长任务执行体（`/tasks/growth` 的执行体，对应 Go `Config.GrowthTasks`）。
    ///
    /// `None` = 未接线 ⇒ `/tasks/growth` 返回 Go 的「未注入」分支 503
    /// `growth_tasks_unavailable`。`bin/gateway.rs` 恒注入，故生产路径非 `None`；
    /// 保留 `Option` 是为了让既有单测（构造 `AppState` 时不关心任务）不必改动。
    pub growtask: Option<Arc<crate::growtask::Runner>>,
}

/// 构建路由。
///
/// 路由表与 Go 侧 `internal/server/handler.go:307-336` 一致，含**无版本号别名**
/// （`/responses`、`/messages`）：部分客户端会省略 `/v1` 前缀，缺了别名会 404。
///
/// ★ `/debug/` 用**两条显式路由**而不是 Go 那样的单条前缀路由：
/// Go 的 `debugHandler` 靠 `strings.TrimPrefix(r.URL.Path, "/debug/")` 手解 `area`，
/// 而 axum 0.7 的 catch-all（`*rest`）只能出现在路径末尾、空捕获的行为也没在
/// 生产里验证过。显式注册语义等价（`/debug/` → 索引，`/debug/capability` → 视图），
/// 且路径写错会在**注册期**就暴露，而不是等到客户端 404。
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses))
        .route("/responses", post(responses))
        .route("/v1/messages", post(messages))
        .route("/messages", post(messages))
        .route("/v1/models", get(models))
        // `/v1/models/regions` 是「按区域的能力真值」对比视图，是 `/v1/models`
        // 的**补充而非替代**（Go `handler.go:313-315` 同款注释）。
        .route("/v1/models/regions", get(models_regions))
        .route("/status", get(status))
        .route("/debug/", get(debug_index))
        .route("/debug/capability", get(debug_capability))
        .route("/debug/accounts/:uid/revive", post(debug_revive_account))
        .route("/usage", get(usage_report))
        .route("/tasks/run", post(tasks_run))
        .route("/tasks/growth", post(tasks_growth))
        .route("/login/sms/send", post(sms_send))
        .route("/login/sms/verify", post(sms_verify))
        .route("/healthz", get(healthz))
        .with_state(state)
}

/// 统一 JSON 响应。
fn json_response(status: StatusCode, v: Value) -> Response {
    let mut res = Json(v).into_response();
    *res.status_mut() = status;
    res.headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    res
}

/// OpenAI 风格错误响应体。
///
/// 与 Go 侧 `writeOpenAIError` 一致：`{"error":{"message","type","code"}}`。
///
/// ★ 这里是 [`crate::clienttext::client_safe_message`] 的**闸门**之一（Go
/// `handler.go:1256`）：任何一条走向客户端的文案都要过它，否则上游原文里一个裸
/// 「403」就会让 DSH 把「上游拒绝了这个账号」读成「API 密钥无效」，把用户引向一个
/// **不可能修好的方向**（反复改密钥）。
fn openai_error(status: StatusCode, code: &str, msg: &str) -> Response {
    json_response(
        status,
        json!({
            "error": {
                "message": crate::clienttext::client_safe_message(msg),
                "type": "api_error",
                "code": code,
            }
        }),
    )
}

/// 客户端凭据校验。
///
/// 与 Go 侧 `withAuth`（`handler.go:343-368`）一致：key 为空则放行；否则接受
/// **两种头部形态**，覆盖不同客户端的认证习惯：
///
/// - `Authorization: Bearer <key>` —— OpenAI SDK、Claude Code 的
///   `ANTHROPIC_AUTH_TOKEN`、Claude Desktop 3P（`inferenceGatewayAuthScheme=bearer`）
/// - `x-api-key: <key>` —— Anthropic SDK、Claude Code 的 `ANTHROPIC_API_KEY`
///
/// ★ 两者都**精确匹配、区分大小写**（与 Go 的 `==` 同口径）。
///
/// ★★ 补 `x-api-key` 是**必须的**，不是「更宽容」：Anthropic SDK 只发
/// `x-api-key`，缺这一条会让 Claude Code 用 `ANTHROPIC_API_KEY` 时对**全部**
/// 路由 401 —— 而报错文案是「missing or invalid API key」，用户会去反复检查
/// 那个明明正确的密钥。
fn check_auth(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if state.api_key.is_empty() {
        return None;
    }
    // ★ 顺序与 Go 一致：先看 `Authorization`，命中即返回；**不**再看 `x-api-key`。
    // 反过来（任一命中即放行）会让两个头给不同值时语义变得难以推理。
    if let Some(v) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(token) = v.strip_prefix("Bearer ") {
            return if token == state.api_key {
                None
            } else {
                Some(unauthorized())
            };
        }
        // 带了 Authorization 但不是 Bearer 形态：继续看 x-api-key
    }
    match headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        Some(v) if v == state.api_key => None,
        _ => Some(unauthorized()),
    }
}

/// 鉴权失败的统一响应（`check_auth` 的两条分支共用，避免文案漂移）。
fn unauthorized() -> Response {
    openai_error(
        StatusCode::UNAUTHORIZED,
        "invalid_api_key",
        "missing or invalid API key",
    )
}

/// `GET /healthz` —— 恒无鉴权。
///
/// 用 `servable_now` 判定：healthy>0 但全占满在途时 chat 会 503，
/// 探活必须同口径，否则负载均衡器会把流量持续打进无法受理的实例。
async fn healthz(State(state): State<Arc<AppState>>) -> Response {
    let (total, healthy, _, _, _) = {
        let p = state.pool.lock().unwrap();
        p.counts_detailed()
    };
    let servable = {
        let p = state.pool.lock().unwrap();
        p.servable_now()
    };
    let status = if servable {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    let mut res = json_response(
        status,
        json!({
            "healthy": healthy,
            "total": total,
            "service": SERVICE_NAME,
        }),
    );
    res.headers_mut()
        .insert("X-Service", SERVICE_NAME.parse().unwrap());
    res
}

/// `GET /status` —— 账号池状态（需鉴权）。
async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(r) = check_auth(&state, &headers) {
        return r;
    }
    let (total, healthy, cooling, disabled, inflight_full) = {
        let p = state.pool.lock().unwrap();
        p.counts_detailed()
    };
    let accounts: Vec<Value> = {
        let p = state.pool.lock().unwrap();
        p.list().iter().map(status_to_json).collect()
    };
    let redis_mode = if state.redis_mode.is_empty() {
        "noop"
    } else {
        &state.redis_mode
    };

    json_response(
        StatusCode::OK,
        json!({
            "accounts": accounts,
            "total": total,
            "healthy": healthy,
            "cooling": cooling,
            "disabled": disabled,
            "in_flight_full": inflight_full,
            "sticky_sessions": state.session.as_ref().map(|s| s.count()).unwrap_or(0),
            "redis_mode": redis_mode,
        }),
    )
}

/// 把 pool Status 转成 /status 的 JSON 对象（字段名与 Go 侧 `Status` tag 一致）。
fn status_to_json(s: &crate::pool::Status) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("uid".into(), s.uid.clone().into());
    if !s.nickname.is_empty() {
        m.insert("nickname".into(), s.nickname.clone().into());
    }
    m.insert("credits".into(), s.credits.into());
    m.insert("cooling".into(), s.cooling.into());
    if !s.cool_kind.is_empty() {
        m.insert("cool_kind".into(), s.cool_kind.clone().into());
    }
    if s.cool_remaining_sec != 0 {
        m.insert("cool_remaining_sec".into(), s.cool_remaining_sec.into());
    }
    if let Some(u) = s.until {
        if let Ok(d) = u.duration_since(std::time::SystemTime::UNIX_EPOCH) {
            m.insert(
                "until".into(),
                chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default()
                    .into(),
            );
        }
    }
    if !s.reason.is_empty() {
        m.insert("reason".into(), s.reason.clone().into());
    }
    m.insert("disabled".into(), s.disabled.into());
    // ── 账号身份：产品 + 区域（所有者 2026-09-20 / 2026-10-01 要求）──
    //
    // 三个产品的账号混在**同一个池**里，而界面上只有昵称 —— 用户看到
    // `acct-a`、`acct-b-…` 这样的名字，**不知道它来自哪个客户端、哪个区域**。
    // 于是排查「为什么 `zcode:` 前缀选不出号」时看不出池里到底有几个 ZCode 号；
    // 想给某个号单独停流量时得先去别的页面确认它是哪家的。
    //
    // ⚠ 两者都**恒输出**（不带 omitempty）：`pool.Status` 里的 `product` / `region`
    // 在构造时就已归一化（`product_of()` 把空串折成 workbuddy；`region()` 永不返回
    // `Any`），因此它们**必然非空** —— 加了 omitempty 反而会让「键不存在」与
    // 「值为空」这两种状态在前端无法区分。Go 侧带 omitempty 只是因为它的零值是
    // 空串，而这里根本不会出现空串。
    m.insert("product".into(), s.product.clone().into());
    m.insert("region".into(), s.region.clone().into());
    // `no_route` 与 `disabled` 是**两种不同的「不接流量」**，前端要如实区分：
    //   no_route → 用户自己关的；签到 / 上报 / 成长任务**仍在跑**
    //   disabled → 网关判定该号已死（session 死 / 额度冻结），任务也会跳过
    // 若都写成「禁用」，用户就无法判断「这个号还在不在养」—— 而
    // 「禁用了但仍在养号」正是本功能的语义。Go 侧同 tag（`no_route,omitempty`）。
    if s.no_route {
        m.insert("no_route".into(), true.into());
    }
    // 「按到期档位现在轮不到它」：不是故障，只表示排队。
    //
    // ⚠ 必须由后端下发：前端既拿不到 healthy / 模型冷却 / 在途 这三套判定，
    // 也没有「当前生效档位」这个池级信息。用 `success_count == 0` 之类近似会
    // 误判 —— 新导入或持续失败的账号同样是 0，但它们不是排队。
    if s.queued {
        m.insert("queued".into(), true.into());
    }
    // 模型级冷却列表（仅这些模型不可用，换模型仍可用）。
    //
    // 与 `cool_kind = "hard_credit"`（账号级：余额欠费，整号不可用）正交，两者
    // 可同时存在（例如余额充足但某模型额度用尽）。前端据此区分两种冷却。
    if !s.model_cooling.is_empty() {
        let list: Vec<Value> = s
            .model_cooling
            .iter()
            .map(|mc| {
                let mut o = serde_json::Map::new();
                o.insert("model".into(), mc.model.clone().into());
                if let Some(u) = mc.until {
                    if let Ok(d) = u.duration_since(std::time::SystemTime::UNIX_EPOCH) {
                        o.insert(
                            "until".into(),
                            chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                                .map(|dt| dt.to_rfc3339())
                                .unwrap_or_default()
                                .into(),
                        );
                    }
                }
                o.insert("remaining_sec".into(), mc.remaining_sec.into());
                if !mc.reason.is_empty() {
                    o.insert("reason".into(), mc.reason.clone().into());
                }
                o.insert("reset_at_parsed".into(), mc.reset_at_parsed.into());
                Value::Object(o)
            })
            .collect();
        m.insert("model_cooling".into(), Value::Array(list));
    }
    if s.success_count != 0 {
        m.insert("success_count".into(), s.success_count.into());
    }
    if s.err_total != 0 {
        m.insert("err_total".into(), s.err_total.into());
    }
    if let Some(t) = s.last_success {
        if let Ok(d) = t.duration_since(std::time::SystemTime::UNIX_EPOCH) {
            m.insert(
                "last_success".into(),
                chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default()
                    .into(),
            );
        }
    }
    if let Some(t) = s.last_err {
        if let Ok(d) = t.duration_since(std::time::SystemTime::UNIX_EPOCH) {
            m.insert(
                "last_err".into(),
                chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default()
                    .into(),
            );
        }
    }
    // 运行态字段（Go 侧无 omitempty，恒输出）
    m.insert("in_flight".into(), s.in_flight.into());
    m.insert("breaker_fails".into(), s.breaker_fails.into());
    if let Some(b) = s.breaker_until {
        if let Ok(d) = b.duration_since(std::time::SystemTime::UNIX_EPOCH) {
            m.insert(
                "breaker_until".into(),
                chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default()
                    .into(),
            );
        }
    }
    if s.soonest_expire_at != 0 {
        m.insert("soonest_expire_at".into(), s.soonest_expire_at.into());
    }
    if !s.expire_day.is_empty() {
        m.insert("expire_day".into(), s.expire_day.clone().into());
    }
    Value::Object(m)
}

// ★ 这里曾有 `image_capability_fields()` / `model_reasoning_fields()` 两个函数，
// 2026-10-01 删除。它们**不是**被遗忘的备用实现，而是 [`crate::capability`] 同名
// 实现的**陈旧副本**：`/v1/models` 的真实路径是
// `models()` → `CapabilityIndex::merged_model_list()` →
// `capability::CapabilityIndex::capability_fields_for()`，从不经过这两个函数
// （`cargo build` 报 `never used` 是唯一线索）。
//
// 留着它们比死代码更糟 —— 旧版 `model_reasoning_fields` 里
// `if efforts.is_empty() { return None; }` 正是所有者 2026-09-18 报的那个缺陷：
// 「有默认档但上游没列出档位数组」的模型（18 个国服 / 8 个国际版）在界面上显示成
// 「—」，用户以为不能思考。正确实现（`capability.rs:725`）分三情形，情形 B
// 会下发**标准阶梯**并显式标注 `reasoning_range_undeclared`，两者行为直接矛盾。
// 谁能看到这份副本，谁就可能照着它「修」回去。
//
// 能力字段的完整语义（多拼写下发、图片三态、`range_undeclared`）见
// `capability.rs` 的 `model_capability_fields` / `model_reasoning_fields`。

/// 内置的 WorkBuddy 模型名（国服静态表 + 国际版静态表的**并集**，按首次出现顺序去重）。
///
/// # 用途：补全 `pool.product_models` 缺失的 workbuddy 项
///
/// 见 `bin/gateway.rs` 调用处与 [`crate::pool::Pool::pick_for_model_any`] 的注释：
/// 宿主透传的清单来自用户的「限制使用的模型」白名单（默认为空 ⇒ 不写 workbuddy），
/// 缺了它会让**裸名**请求被路由到 WorkBuddy 账号而回 11102。
///
/// # 为什么这里「宁可多列」是安全的
///
/// 这个清单只用于「是否**明确声明**提供该模型」的判定
///（[`crate::pool::Pool::product_declares_model`]），**不用于「否定」**
///（否定走 [`crate::pool::Pool::product_may_serve`]，那里读的是**兜底清单**）。
/// 多列几个模型的最坏后果是「某个模型优先去 WorkBuddy 试一次」，
/// 而不是「某个账号被永久排除」。故并集比精确更重要。
///
/// # ⚠⚠ 这份清单**不能**参与「声明」竞争 —— 但它是兜底清单，不是声明清单
///
/// 2026-09-22 修正：曾经把这份清单直接当作「声明」注入 `product_models`，
/// 于是 `GLM-5.3` / `Auto` 同时被 workbuddy（这份表）与 qoder 声明 ⇒ 两产品
/// 都进 `declared` ⇒ 一起竞争 ⇒ 池里 **19 个 WorkBuddy 账号 vs 1 个 qoder
/// 账号** ⇒ 大概率选到 WorkBuddy，而它其实**没有** `GLM-5.3`（静态表是网关的
/// **猜测**，不是上游的真实能力清单）⇒ 上游回 11102。所有者的现场：
///
/// ```text
/// GLM-5.3        → 400 model_not_in_region
/// Auto           → 400 model_not_in_region
/// Qwen3.8-Flash  → 200 正常
/// ```
///
/// 差别在**重叠**：`Qwen3.8-Flash` 只有 qoder 声明（路由唯一），而
/// `GLM-5.3` / `Auto` 两边都「声明」。⇒ 这份清单**只用于「不排除」**，
/// 由 `Pool::set_product_models_fallback` 注入（与 Go
/// `p.SetProductModelsFallback(fallback)` 同一位置、同一语义）。
///
/// # 与 `/v1/models` 的关系
///
/// ★ 组 F 后：`/v1/models` 由 [`crate::capability::CapabilityIndex::merged_model_list`]
/// 生成，静态表**只补该区域真值没取到的那一侧**（动态真值优先，见那里的文档）。
/// 本函数与那张静态表是**同一份数据的两处消费**：
/// - `merged_model_list` 用它给「没拉到真值」的区域提供**兜底条目**（含元数据）；
/// - 本函数把同样的名字喂给「不排除」判定（见上）。
///
/// 两处都**不得**被当成「上游声明」—— 静态表是网关的猜测。这次修正之前
/// `/v1/models` 曾把静态表无条件当作权威清单下发，后果就是上面那个现场：
/// 客户端按静态表选了 `GLM-5.3`，运行时被上游 11102 拒绝。
pub fn static_workbuddy_model_ids() -> Vec<String> {
    // 国服表 + 国际版表；两张表都用 `&str` 列出，去重顺序与 Go 的
    // `seen` 遍历一致（国服优先）。
    const STATIC_MODELS: &[&str] = &[
        "auto",
        "hy4-preview",
        "hy3",
        "hy3-x",
        "deepseek-v4.1-flash",
        "deepseek-v4-pro",
        "glm-5.3",
        "glm-5.3-flash",
        "glm-5.2",
        "glm-5.1",
        "glm-5v-turbo",
        "minimax-m3",
        "kimi-k3-1",
        "kimi-k2.8-preview",
        "kimi-k2.7",
        "kimi-k2.6",
    ];
    // ⚠ `hy4-preview` **不在**本表（2026-09-21 所有者要求）：实测国服 16 个模型
    // 含 `hy4-preview`，国际版 22 个**不含**（国际版有 `hy4-preview-f`，
    // 是**另一个**模型）。旧代码把它同时写进两表，于是 `/v1/models` 宣称
    // 国际版也能调它，而国际版根本调不到 —— 用户按清单指定国际版就失败。
    // 所有者原话：「国际版既然不支持，为什么你还能跑出来这个模型？接口返回
    // 没有就不要搞出来，懂吗？」
    const STATIC_MODELS_INTL: &[&str] = &[
        "default-model",
        "fast-model",
        "balanced-model",
        "primary-model",
        "deep-model",
        "hy4-preview-f",
        "hy3",
        "deepseek-v4.1-flash",
        "gpt-6-astra",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
        "gpt-5.5",
        "gpt-5.4",
        "gemini-3.5-flash",
        "glm-5.3",
        "glm-5.2",
        "kimi-k3",
        "kimi-k2.8-preview",
        "kimi-k2.6",
    ];

    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::with_capacity(STATIC_MODELS.len() + STATIC_MODELS_INTL.len());
    for id in STATIC_MODELS.iter().chain(STATIC_MODELS_INTL.iter()) {
        if id.is_empty() || !seen.insert(id) {
            continue;
        }
        out.push((*id).to_string());
    }
    out
}

/// Trae 侧客户端可发模型名清单（兜底用；真值见 `products/trae/dispatch.rs` 的别名表）。
///
/// ★ 与 `static_workbuddy_model_ids` 一样，这只是网关的**猜测**，只用于
/// 「不排除」判定（`Pool::set_product_models_fallback`），**不得**当成上游声明。
///
/// 真实事故（2026-10-04）：客户端发裸名 `deepseek-v4.1-flash`（Trae 没有这个模型），
/// 请求被路由到 Trae 账号并成功返回。原因是宿主下发的 `product_models` 里没有
/// trae 键 ⇒ `product_offers_model` 回「不知道」⇒ 在**连兜底清单都没有**时
/// `product_may_serve` 一律 `return true`。补上这份清单后，裸名会被 Trae 排除。
///
/// 数据源刻意选别名表而不是 state.vscdb：后者被「智能体客户端一键导入」写过我们
/// 自己的模型，会把 `deepseek-v4.1-flash` 也声明成 Trae 的模型。
pub fn static_trae_model_ids() -> Vec<String> {
    crate::products::trae::dispatch::static_trae_model_ids()
}

/// `GET /v1/models` —— 模型列表（需鉴权）。
///
/// # 两区并集（与 Go `mergedModelList` 逐条对齐）
///
/// 下发的是**两个区域真值的并集**，而不是某一个账号看到的清单：同名模型可能只在
/// 一个区域存在，跨区调用上游会回 `code=11102 model service info not found`。
/// 客户端只看到一个扁平列表，故网关必须把两区合并后再下发，由
/// `supported_regions` 与组合名（`qoder:模型名` / `国际版:模型名`）承担
/// 「这个模型该去哪一区」的表达。
///
/// 顺序**国服在前**：客户端模型菜单常按返回顺序展示，国服是图片能力与模型覆盖都
/// 更完整的一侧，排前面更容易被选中（Go `capability.go:762-765` 逐字）。
///
/// # 静态表在这里的角色
///
/// 静态表**只补该区域真值没取到的那一侧**，且只提供兜底元数据
///（`created` / `owned_by` / `context_length`）—— 它**不**用来宣称「某模型存在」。
/// 所有者现场（2026-09-21）：「国际版既然不支持，为什么你还能跑出来这个模型？
/// 接口返回没有就不要搞出来，懂吗？」那次正是静态表无条件参与，把 `hy4-preview`
/// 这类国际版确实没有的模型塞进了清单。
///
/// 思考档位（`reasoning.supportedEfforts`）**只有**上游知道，静态表里没有任何档位
/// 信息：只靠静态表下发会让客户端永远看不到档位，用户只能靠猜，猜错就被
/// `normalize_reasoning_effort` 悄悄改写，表现为「我明明调了 max 却没生效」。
/// 故档位只来自动态真值。
///
/// # 永不 503
///
/// 与 Go `handler.go:826-834` 一致：**不检查账号、永不失败**，始终 200。
///
/// 此前这里在拿不到上游清单时返回 503 `models_unavailable`，那个契约的前提是
/// 「只下发上游实时清单」。并集语义下**总有东西可下发**：一个区域拉失败不该让整个
/// `/v1/models` 失败 —— 那会让客户端连另一区可用的模型都看不到，而界面的文案
/// 只会说「无法获取模型清单」，用户不知道该去补账号还是查网络。
///
/// # `?refresh`
///
/// 接受 `1` / `true` / `yes`（大小写不敏感、两侧空白忽略，与 Go
/// `wantsModelRefresh` 同口径），命中则**清空两区缓存**后重新拉取。
///
/// 它存在的理由是一类**具体**的缺陷：某轮国际版拉取因网络抖动失败 ⇒ 近半数模型被
/// 打上「国服未检测到可用真值」，而文案结尾写着「稍后点『刷新』重试即可确认」——
/// 但当时清缓存的函数只被测试调用、生产代码零调用，且失败后 5 分钟内拉取直接返回
/// 空。结论：**文案让他做的事，代码不支持**。现在界面上的「刷新模型列表」真的走这条。
async fn models(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if let Some(r) = check_auth(&state, &headers) {
        return r;
    }
    if wants_model_refresh(uri.query()) {
        state.models.reset();
    }

    let (fetched, has_account) = region_snapshot(&state).await;
    // ★ 走 `DynamicState` 而不是启动快照：宿主新增/刷新账号后会重写配置里的
    // `pool.product_models`，热重载把它带进来（否则这里永远缺 Qoder / ZCode 的渠道）。
    let product_models =
        crate::capability::product_models_from(&state.dynamic.snapshot().product_models);
    let list = crate::capability::CapabilityIndex::merged_model_list(
        &fetched,
        &has_account,
        &product_models,
        |product| product_regions_of(&state, product),
    );

    // ★ 只有两个键（Go `handler.go:826-834` 逐字）：`object` + `data`。
    // 此前多下发了一个 `source: "upstream"` —— Go 没有这个键，且它的值在并集语义下
    // 已经**不成立**（清单是「动态真值 + 静态兜底」的混合，不是纯上游）。
    json_response(StatusCode::OK, json!({ "object": "list", "data": list }))
}

/// `GET /v1/models/regions` —— 按区域的**能力真值**对比视图（需鉴权）。
///
/// # 为什么它是必需的（不是「有更好」）
///
/// 区域路由与能力下发都建立在「上游两区能力不同」这一**实测事实**上，而实测需要
/// 能看见真值。没有这个接口时，用户遇到「同一个模型时好时坏」只能靠猜。
/// Go `admin.go:10-18` 逐字：「本次缺陷本身就是『网关悄悄地用了错区域的真值』而
/// 完全不可诊断 —— 同样的错误不该在修复后重现。」
///
/// # 它还是**宿主**的硬依赖
///
/// 宿主（`ai-gateway-core::modules::model_billing`）读这个接口判断「这个账号调这个
/// 模型该不该扣分」，而不是自己硬编码免费清单。宿主不自己拉上游的理由：网关已经按
/// 区域拉好并缓存，宿主再拉一次等于把同一份数据取两遍，两处解析口径迟早漂移。
///
/// ★ `credit_multiplier` 必须是 **JSON 数字或 null**：宿主侧 `billing_of` 只认
/// 「有限且 > 0 → 计费」「== 0 → 免费」，其余（含 null、负数、**字符串**）一律
/// 判为 Unknown。给人看的原文走 `credits_raw`。
///
/// ★ `last_error` 与 `last_error_message` 必须是**两个**字段：一个回答「什么时候」，
/// 一个回答「为什么」。Go `admin.go:38-43` 逐字记了现场 —— 界面报「国服未检测到可用
/// 真值」，而当时只有时间戳，看不出失败原因；同 exe 同配置的独立探针能拉到 16 个
/// 国服模型，网关却拉不到，正是这个字段的缺失让排查只能靠猜。
async fn models_regions(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(r) = check_auth(&state, &headers) {
        return r;
    }
    let (fetched, _has_account) = region_snapshot(&state).await;
    let regions = vec![
        region_view(&state, Region::CN, &fetched),
        region_view(&state, Region::Intl, &fetched),
    ];
    json_response(
        StatusCode::OK,
        json!({ "regions": regions, "notes": REGION_NOTES }),
    )
}

/// `/v1/models/regions` 的 notes 三条（Go `admin.go:98-105` 逐字）。
const REGION_NOTES: [&str; 3] = [
    "同名模型可能只在一个区域存在；跨区域调用返回 code=11102 model service info not found。\
     网关会按 supported_regions 把该模型的请求路由到它所在的区域。",
    "glm-5.3 / glm-5.2 两区都宣称 supportsImages=true，但实测只有国服后端能读图：\
     国际版会把图片替换成固定占位符（prompt_tokens 增量恒为 +33，与图片体积无关）。\
     带图片的这两个模型请求因此偏好国服账号。",
    "hy3 / kimi-k2.6 实测两区都能读图，故不做区域偏好 —— \
     对它们偏好只会白白放弃一半账号的额度。",
];

/// 单个区域的真值视图（Go `admin.go:25-96` 的 `regionView`）。
fn region_view(
    state: &AppState,
    region: Region,
    fetched: &HashMap<Region, Option<Vec<crate::upstream::ModelInfo>>>,
) -> Value {
    let infos = fetched
        .get(&region)
        .and_then(|v| v.clone())
        .unwrap_or_default();
    let meta = state.models.meta(region);
    let dynamic = !infos.is_empty();

    let models: Vec<Value> = infos
        .iter()
        .map(|mi| {
            // 上游说的与网关实际用的不一致时，把上游原话也列出来。
            // Go `admin.go:73-75` 逐字：「这正是本次缺陷的核心：上游对国际版 glm-5.x
            // 报 true，实测 false。不显式展示的话，看到 supportsImages=false 的人会
            // 以为网关有 bug。」
            let effective = crate::upstream::measured_override(&mi.id, region, mi.supports_images);
            let mut entry = serde_json::Map::new();
            entry.insert("id".into(), json!(mi.id));
            entry.insert("supportsImages".into(), opt_bool_json(effective));
            entry.insert("context_length".into(), json!(mi.context_window));
            entry.insert("max_tokens".into(), json!(mi.max_tokens));
            // ★ 三态：0 = 免费、> 0 = 计费、null = 上游未声明（**不是免费**）。
            entry.insert(
                "credit_multiplier".into(),
                mi.credit_multiplier.map(Value::from).unwrap_or(Value::Null),
            );
            entry.insert("credits_raw".into(), json!(mi.credits_raw));
            if effective != mi.supports_images {
                entry.insert(
                    "upstream_claims_supports_images".into(),
                    opt_bool_json(mi.supports_images),
                );
                entry.insert("overridden_by".into(), json!("measured"));
            }
            Value::Object(entry)
        })
        .collect();

    json!({
        "region": region.as_str(),
        "truth_source": truth_source_for(region, dynamic),
        "fetched_at": meta.fetched.map(fmt_rfc3339),
        "last_error": meta.last_error.map(fmt_rfc3339),
        // 零值 → null（Go 侧同样只在非空时填）。两个字段拆开是为了让「什么时候」
        // 与「为什么」各自可读。
        "last_error_message": if meta.last_error_message.is_empty() {
            Value::Null
        } else {
            json!(meta.last_error_message)
        },
        "model_count": models.len(),
        "models": models,
    })
}

/// 真值来源三态（Go `admin.go:110-118`）：动态拉到的、国服静态兜底、国际版未知。
///
/// 国际版**没有**静态兜底：它的静态表非权威，不能用来判断存在性。
fn truth_source_for(region: Region, dynamic: bool) -> &'static str {
    if dynamic {
        "dynamic"
    } else if region == Region::CN {
        "static_fallback"
    } else {
        "unknown"
    }
}

/// `Option<bool>` → JSON：`None` 序列化成 `null`（= **未声明**，不是「不支持」）。
fn opt_bool_json(v: Option<bool>) -> Value {
    v.map(Value::from).unwrap_or(Value::Null)
}

/// `SystemTime` → RFC3339（带本地偏移，与 Go `time.Format(time.RFC3339)` 同形）。
fn fmt_rfc3339(t: std::time::SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Local> = t.into();
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// `GET /debug/` —— 调试视图索引（需鉴权）。
async fn debug_index(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(r) = check_auth(&state, &headers) {
        return r;
    }
    json_response(StatusCode::OK, json!({ "areas": ["capability"] }))
}

/// `GET /debug/capability` —— 能力索引与两区真值的对照（需鉴权）。
///
/// ★ 刻意把**静态表**也列出来（Go `admin.go:215-218` 逐字）：「本次修复中最难发现的
/// 一类问题是『静态表与上游真值不一致』—— 客户端按静态表选了模型，运行时却失败。」
///
/// `dynamic_*_ids` 来自本轮的拉取结果（可能是缓存，也可能为空）；
/// `*_truth_known` 来自能力索引的 `known_region`（**只对动态真值置位**）。
/// 两者不同是正常的，也正是这个视图要暴露的信息。
async fn debug_capability(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(r) = check_auth(&state, &headers) {
        return r;
    }
    let (fetched, has_account) = region_snapshot(&state).await;
    let idx = crate::capability::CapabilityIndex::build(&fetched, &has_account);

    let ids_of = |region: Region| -> Vec<String> {
        fetched
            .get(&region)
            .and_then(|v| v.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|mi| mi.id)
            .collect()
    };

    json_response(
        StatusCode::OK,
        json!({
            "dynamic_cn_ids": ids_of(Region::CN),
            "dynamic_intl_ids": ids_of(Region::Intl),
            "static_cn_ids": crate::capability::static_model_ids(Region::CN),
            "static_intl_ids": crate::capability::static_model_ids(Region::Intl),
            "cn_truth_known": idx.is_known_region(Region::CN),
            "intl_truth_known": idx.is_known_region(Region::Intl),
            "accounts": debug_accounts(&state),
        }),
    )
}

/// 账号池的 uid / 区域 / 域名 / 昵称清单（Go `admin.go:240-259` 的 `debugAccounts`）。
///
/// doc 逐字：「区域判定依据 domain 后缀（.ai → 国际版），domain 缺失时归国服。
/// 这一条最值得人工核对，因此把原始 domain 一并透出来。」
///
/// ★ 空池必须序列化成 `[]` 而不是 `null`：`Vec` 天然如此（`json!` 对空 `Vec<Value>`
/// 给 `[]`），这与 Go 侧把 `out` 初始化成空切片是同一个理由。
fn debug_accounts(state: &AppState) -> Vec<Value> {
    let pool = match state.pool.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    pool.all_uids()
        .into_iter()
        .filter_map(|uid| {
            let a = pool.auth_by_uid(&uid)?;
            Some(json!({
                "uid": uid,
                "region": a.region().as_str(),
                "domain": a.domain,
                "nickname": a.nickname,
            }))
        })
        .collect()
}

/// `POST /debug/accounts/:uid/revive` —— 复活被**系统**禁用的账号（需鉴权）。
///
/// # 为什么必须有这个入口
///
/// `disabled` 会持久化进 state 文件，而它此前**只有写入方、没有任何清除入口**：
/// 账号一旦被自动判定为死号（会话失效 / 额度冻结），在网关内就永远救不回来 ——
/// 重新导入凭证也无效（池的 upsert 对已存在账号只换凭证、不碰 `disabled`），
/// 用户唯一的出路是手改 state 文件。Go `admin.go:141-147` 逐字：
/// 「**不可诊断、不可恢复的状态本身就是缺陷**，而不是『运维自己想办法』。」
///
/// # 语义边界（一条都不能越）
///
/// - 只清**系统判定位**：`disabled` / `reason`
/// - **不动**用户的手工轴 `no_route`（那是宿主写进凭证文件的「停止接流量」开关）
/// - **不动**熔断与账号级冷却（它们各有自己的到期路径，复活不假装账号健康）
///
/// 因此「复活成功但仍不接流量」有两种**正常**可能，响应里都如实透出并在 `notes`
/// 里解释。不显式说明的话，用户会以为复活没生效。
///
/// 幂等：账号本来就启用 → 200 + `revived: false`（不是错误，客户端可安全重试）。
/// uid 不在池中 → 404。
async fn debug_revive_account(
    State(state): State<Arc<AppState>>,
    Path(uid): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(r) = check_auth(&state, &headers) {
        return r;
    }
    if uid.is_empty() {
        return json_response(StatusCode::BAD_REQUEST, json!({ "error": "缺少账号 uid" }));
    }

    // ★ 先查存在性再复活：`revive_disabled` 的 `false` 有**两种**含义
    //（「不在池中」与「本来就启用」），不先查就没法区分它们该给 404 还是 200。
    let before = {
        let pool = match state.pool.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        pool.status_by_uid(&uid)
    };
    let Some(before) = before else {
        return json_response(
            StatusCode::NOT_FOUND,
            json!({
                "error": format!("账号不在池中：{uid}"),
                "uid": uid,
                // Go `admin.go` 逐字口径：池与凭证目录是**对齐**的（含运行期热加载）。
                // Rust 侧同样有热加载（`authwatch`，缺省每 5s 一轮），故这里的文案与
                // Go 保持一致 —— 不能让用户去等一件代码不支持的事（本仓库踩过这个坑：
                // `/v1/models` 的旧文案就让他「稍后重试」，而 `resetModelsCache` 当时
                // 根本没有生产调用方）。
                "hint": "账号池与凭证目录是对齐的（含运行期热加载）。若刚导入凭证，\
                         稍等一轮目录扫描（默认 5s）后重试；若凭证已被删除，它确实不会在池里。",
            }),
        );
    };

    let revived = {
        let mut pool = match state.pool.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        pool.revive_disabled(&uid)
    };
    let after = {
        let pool = match state.pool.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        pool.status_by_uid(&uid).unwrap_or_default()
    };

    let mut notes: Vec<String> = Vec::new();
    // ★ 三个独立 `if`（Go `admin.go:196-211`），不是 `else if`：三条可以同时出现。
    if !revived {
        notes.push("该账号本来就未被系统禁用，未做任何改动（幂等）。".to_string());
    }
    if after.no_route {
        notes.push(
            "该账号仍被标为「不接流量」(no_route)：这是用户在界面上拨的开关，写在凭证文件里，\
             复活只解系统禁用位、不动它。要恢复流量请在界面上关掉该开关。"
                .to_string(),
        );
    }
    if after.cooling {
        notes.push(
            "该账号当前处于冷却/熔断期，冷却到期或下一次成功请求后才会恢复接流量\
             （复活不清冷却与熔断）。"
                .to_string(),
        );
    }

    let mut resp = serde_json::Map::new();
    resp.insert("uid".into(), json!(uid));
    resp.insert("revived".into(), json!(revived));
    resp.insert("disabled".into(), json!(after.disabled));
    resp.insert("no_route".into(), json!(after.no_route));
    resp.insert("reason".into(), json!(after.reason));
    resp.insert("cooling".into(), json!(after.cooling));
    // ★ 复活后 `reason` 已被清空，故必须在这里留下它 —— 这是排查「它当初为什么被判死」
    // 的**唯一**线索（Go `admin.go:192-194`）。
    resp.insert("disabled_reason_before".into(), json!(before.reason));
    if !notes.is_empty() {
        resp.insert("notes".into(), json!(notes));
    }
    json_response(StatusCode::OK, Value::Object(resp))
}

/// `POST /tasks/run` —— 手动触发养号任务（需鉴权）。
///
/// # 为什么有这个入口
///
/// 养号任务此前只有「按点自动跑」，用户既看不见执行结果、也没法在改完配置后立刻
/// 验证。手动触发是**可观测性**的一部分（Go `handler.go:490-505` 逐字）。
///
/// # 参数优先级（★ 顺序不能反）
///
/// 查询串 `?task=&accountId=` **优先**；只有 `task` 为空时才读 body 里的
/// `{task, accountId}`（且 `accountId` 为空才用 body 的值）。
/// `accountId` 可选：省略 → 作用于全部账号（界面右上角「一键操作」）；
/// 给出 → 只作用于该账号（账号卡片菜单）。加这个参数是因为原来只有「全部」一种
/// 语义，而账号菜单里的入口位置暗示的是「只影响这个账号」—— 用户在某个账号的卡片上
/// 操作，跑的却是整池，与菜单位置传达的意思相反。
///
/// # 执行体
///
/// Go 侧 `RunTaskFor` 是注入的回调（`Config.RunTaskFor`），Rust 侧对应
/// [`AppState::scheduler`]。未接线（`None`）时返回 Go 的「未注入」分支
/// 503 `tasks_unavailable` —— 单测构造 `AppState` 时通常不接任务，此时该分支
/// 让「没接执行体」与「执行体报错」在响应上可区分。
///
/// ★ **先校验再 503**（与 Go 同序）：`name` 为空必须回 400 `missing task name`，
/// 否则用户把任务名打错时看到的是「任务运行器未配置」，会去查一个不存在的问题。
///
/// ★ 503 必须带**可读 message**：宿主（`ai-gateway-core::modules::gateway`）会把它
/// 的 `error.message` 原样透出到界面。裸状态码会让用户看到「网关 /tasks/run 返回
/// HTTP 503」这种既没说清是什么、也没说清该做什么的信息。
///
/// ★★ `ErrTaskRunning` ⇒ **200**，不是 4xx/5xx（Go `handler.go:532-536`）：任务
/// 正在跑不是故障，是防重入。用户连点「立即执行」时若回 500，界面会显示成
/// 「执行失败」，而实际上一轮已经在跑了 —— 那是**正确**的行为被报成错误。
/// 因此这里与成功分支一样回 200 + 结果体，`skip == "already_running"` 才是
/// 给调用方看的信号。
async fn tasks_run(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    request: Request<axum::body::Body>,
) -> Response {
    if let Some(r) = check_auth(&state, &headers) {
        return r;
    }
    let mut name = query_param(uri.query(), "task").trim().to_string();
    let mut account_id = query_param(uri.query(), "accountId").trim().to_string();

    if name.is_empty() {
        let raw = axum::body::to_bytes(request.into_body(), 1 << 16)
            .await
            .unwrap_or_default();
        // ★ 解析失败**不算错**（Go 侧 `if err == nil`）：body 可能是空或非 JSON，
        // 那时按「没给任务名」处理即可。
        if let Ok(v) = serde_json::from_slice::<Value>(&raw) {
            if let Some(t) = v.get("task").and_then(|x| x.as_str()) {
                name = t.trim().to_string();
            }
            if account_id.is_empty() {
                if let Some(a) = v.get("accountId").and_then(|x| x.as_str()) {
                    account_id = a.trim().to_string();
                }
            }
        }
    }

    if name.is_empty() {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing task name",
        );
    }
    let Some(sched) = state.scheduler.as_ref() else {
        return openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "tasks_unavailable",
            "task runner not configured on this gateway instance",
        );
    };
    let (res, err) = sched.run_task_for(&name, &account_id).await;
    // ★ ErrTaskRunning ⇒ 200（见函数 doc）：不是故障，是防重入。
    if err
        .as_ref()
        .is_some_and(crate::scheduler::is_task_running)
    {
        return json_response(StatusCode::OK, serde_json::to_value(&res).unwrap_or(Value::Null));
    }
    if let Some(e) = err {
        return openai_error(StatusCode::BAD_REQUEST, "invalid_request", &e.to_string());
    }
    json_response(StatusCode::OK, serde_json::to_value(&res).unwrap_or(Value::Null))
}

/// `POST /tasks/growth` —— 成长任务（`list` / `run` / `run-all`，需鉴权）。
///
/// 请求 `{"action", "accountId"?, "taskCode"?}` → 成功 200 + 结果**不包裹**。
///
/// # 三个动作共用一条路由（Go `handler.go:409-488` 逐字）
///
/// 三个动作共享同一个账号解析与忙碌判定，拆成三条路由会把「账号没找到 /
/// 账号正忙」的错误映射抄三遍，而它们必须完全一致。
///
/// # 与 `/tasks/run` 的**关键差别**
///
/// Go `handler.go:428-433` 的 nil 检查在**读 body 之前**。所以「未注入」时它是
/// 无条件 503，连 action 都不看，body 也**不读**（否则一个 1 MB 的 body 会被
/// 白读进来再丢掉）。这里照此实现。
///
/// ★ Go 侧 `GrowthTasks` 是三个可独立为 nil 的回调字段（`list` / `run` /
/// `run-all` 各自判 nil 并给不同的 503 文案）；本实例只有一个
/// [`crate::growtask::Runner`] 且三个动作全有 ⇒ 只有「全有」与「全无」两种状态。
///
/// # 10 分钟预算是**优雅收尾**而不是失败
///
/// Go `cmd/server/growth_tasks.go` 的 `growTaskTimeout` 是**宿主侧**常量，
/// `handler.go` 的 HTTP 层本身没有超时。本文件同时承担了 Go 侧适配器
/// （`growthTaskAdapter`）的职责，所以预算落在这里。
///
/// ★ 到点后取消的是「还没开始的动作」，已完成的项保留在结果里（见
/// [`growth_with_deadline`]）。用 `tokio::time::timeout` 直接报错会把「跑了一半
/// 但确实拿到部分奖励」变成一条失败记录，用户看到的是「白跑了」。
async fn tasks_growth(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: Request<axum::body::Body>,
) -> Response {
    if let Some(r) = check_auth(&state, &headers) {
        return r;
    }
    // ★ 未接线 ⇒ 无条件 503，且**在读 body 之前**（见函数 doc）。
    let Some(runner) = state.growtask.as_ref() else {
        return openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "growth_tasks_unavailable",
            "growth task runner not configured on this gateway instance",
        );
    };
    // ★ 上限 1 MB，与 Go `handler.go:434` 的 `io.LimitReader(r.Body, 1<<20)` 一致。
    // 注意**不要**跟 `/tasks/run`（64 KB）看齐：两者在 Go 侧就是不同的数，
    // 这里若改成 1<<16 会让超过 64 KB 的 `run-all` 请求体被静默截断成
    // 「invalid JSON body」。
    let raw = axum::body::to_bytes(request.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    // ★ 解析失败 ⇒ 400（Go `jsonUnmarshal` 报错的分支）。空 body 也走这里 ——
    // Go 的 `growthTasks` 直接 `io.ReadAll` 且**忽略**读错误，所以空 body 不是
    // 另一条分支，就是解析失败。
    let Ok(v) = serde_json::from_slice::<Value>(&raw) else {
        return openai_error(StatusCode::BAD_REQUEST, "invalid_request", "invalid JSON body");
    };
    let action = v
        .get("action")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let account_id = v
        .get("accountId")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let task_code = v
        .get("taskCode")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    let out = match action.as_str() {
        "list" => {
            if account_id.is_empty() {
                return openai_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "missing accountId",
                );
            }
            growth_list(&state, runner, &account_id).await
        }
        "run" => {
            if account_id.is_empty() {
                return openai_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "missing accountId",
                );
            }
            growth_run(&state, runner, &account_id, &task_code).await
        }
        "run-all" => growth_run_all(&state, runner).await,
        _ => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                r#"unknown action (want "list" | "run" | "run-all")"#,
            );
        }
    };
    match out {
        Ok(v) => json_response(StatusCode::OK, v),
        Err(msg) => openai_error(StatusCode::BAD_REQUEST, "growth_task_failed", &msg),
    }
}

/// 带 10 分钟预算地跑一个 future（Go `context.WithTimeout` + `growTaskTimeout`）。
///
/// ★ 到点**不报错**，而是发取消信号后**继续等**同一个 future 收尾：执行体只在
/// 检查点观察 [`crate::scheduler::Cancel`]（对应 Go 的 `select { case <-ctx.Done() }`），
/// 已完成的项保留在结果里。`tokio::time::timeout` 会把「跑了一半但确实拿到部分
/// 奖励」变成失败，用户看到的是「白跑了」。
///
/// ★ 取消后必须继续 await 而不是直接返回：收尾（把已有结果汇总、写账号记录）
/// 本身要时间，提前返回会丢掉那些已经拿到的奖励。
async fn growth_with_deadline<F, T>(cancel: &crate::scheduler::Cancel, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::pin!(fut);
    tokio::select! {
        v = &mut fut => return v,
        _ = tokio::time::sleep(crate::growtask::RUN_TIMEOUT) => {}
    }
    cancel.cancel();
    fut.await
}

/// 账号库 id → uid 翻译 + 池内查找（Go `growthTaskAdapter.lookup` 逐字）。
///
/// ★ 宿主界面按**账号库 id** 发起请求，而池按 **uid** 索引，两者不同（id 是随机
/// 标识，重新采集会变）。不做翻译会**静默**表现为「找不到账号」—— 用户看到的是
/// 「账号不在池中」，而账号明明在列表里。
///
/// 回退顺序：先查 id→uid 映射，再当 uid 直查（为「独立运行网关、没有宿主
/// identities」的场景保留 —— 那种部署下 `accountId` 本来就是 uid）。
fn growth_lookup(
    state: &AppState,
    runner: &crate::growtask::Runner,
    account_id: &str,
) -> Result<crate::auth::Auth, String> {
    let mut key = account_id.to_string();
    if let Some(rec) = runner.records() {
        for (uid, id) in rec.identity_index() {
            if id.id == account_id {
                key = uid;
                break;
            }
        }
    }
    let pool = match state.pool.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    pool.auth_by_uid(&key).ok_or_else(|| {
        format!(
            "账号不在网关池中（id={account_id}）：可能是已禁用、需重新登录，或尚未同步到网关"
        )
    })
}

/// `list` 动作（Go `growthTaskAdapter.list`）。
///
/// ★ 只读：刷新列表**不能**有任何副作用（Go `tasks.go:1-12` 逐字）。若把「列任务」
/// 实现成「顺手报名」，用户每刷新一次页面就会产生写操作。
async fn growth_list(
    state: &AppState,
    runner: &crate::growtask::Runner,
    account_id: &str,
) -> Result<Value, String> {
    let auth = growth_lookup(state, runner, account_id)?;
    let cancel = crate::scheduler::Cancel::new();
    let tasks = growth_with_deadline(&cancel, runner.list_tasks(&auth))
        .await
        .map_err(|e| e.to_string())?;
    let total = tasks.len();
    Ok(json!({ "accountId": account_id, "tasks": tasks, "total": total }))
}

/// `run` 动作（Go `growthTaskAdapter.runOne`）：`taskCode` 空 ⇒ 跑全部任务。
async fn growth_run(
    state: &AppState,
    runner: &crate::growtask::Runner,
    account_id: &str,
    code: &str,
) -> Result<Value, String> {
    let auth = growth_lookup(state, runner, account_id)?;
    // ★ 占用中直接拒绝而**不排队**（Go 注释逐字）：排队会让第二次点击在几十秒后
    // 突然执行，用户早已离开页面，反而更容易「没反应就多点几次」。
    // ★ RAII 守卫：客户端断开时 axum 会 drop 本 future，手动 unlock 写在下面
    // 就不会执行 ⇒ 该账号**永久**卡死（只有重启网关能恢复）。已实机复现，见
    // `growtask::AccountGuard`。不要把这里换回 `runner.unlock(...)`。
    let Some(_guard) = runner.try_lock(&auth.uid) else {
        return Err("该账号的任务正在执行中，请稍候".to_string());
    };
    let cancel = crate::scheduler::Cancel::new();
    let out = if code.is_empty() {
        let items = growth_with_deadline(&cancel, runner.run_all(&cancel, &auth)).await;
        let total = items.len();
        json!({ "accountId": account_id, "items": items, "total": total })
    } else {
        let item = growth_with_deadline(&cancel, runner.run_one(&cancel, &auth, code)).await;
        json!({ "accountId": account_id, "item": item })
    };
    Ok(out)
}

/// `run-all` 动作（Go `growthTaskAdapter.runAll`）。
async fn growth_run_all(
    state: &AppState,
    runner: &crate::growtask::Runner,
) -> Result<Value, String> {
    let accounts = growth_pool_auths(state);
    if accounts.is_empty() {
        return Err("账号池为空，请先启动网关或同步账号".to_string());
    }
    let cancel = crate::scheduler::Cancel::new();
    // ★ 并发 **2**（Go 逐字），不是 [`crate::growtask::MAX_FLEET_CONCURRENCY`] 的
    // 上限 3：每个账号的动作里都可能有真实对话，并发过高会在上游形成异常流量画像。
    let opts = crate::growtask::FleetOptions {
        accounts,
        concurrency: 2,
        include_intl: false,
    };
    let results = growth_with_deadline(&cancel, runner.run_all_accounts(&cancel, &opts)).await;
    let summary = crate::growtask::summarize(&results);
    Ok(json!({ "results": results, "summary": summary }))
}

/// 参与「全部账号」执行的账号（Go `growthTaskAdapter.poolAuths`）。
///
/// ★ 按**产品**过滤而**不是**区域（2026-09-22 修的回归）：区域判据改成完整域名
/// 白名单后，Qoder 的 `qoder.sh` 正确判成国际版 ⇒ 被跳过（方向对但依据错）；而
/// Qoder 国服账号 `qoder.com.cn` 在改前改后**都**会参与，拿 Qoder 令牌打
/// WorkBuddy 端点 —— 一直是错的。区域过滤保留在 growtask 内部作为**第二道**闸。
fn growth_pool_auths(state: &AppState) -> Vec<crate::auth::Auth> {
    let pool = match state.pool.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    pool.list()
        .into_iter()
        .filter(|st| !st.disabled)
        .filter_map(|st| pool.auth_by_uid(&st.uid))
        .filter(|a| a.product_of() == crate::auth::PRODUCT_WORKBUDDY)
        .collect()
}

/// `POST /login/sms/send` —— 给手机号发登录验证码（需鉴权）。
///
/// 请求 `{"phone", "region": "cn|intl"}` → 响应 `{"ok": true, "expiresIn": 300}`。
///
/// ★ 用 **400** 而不是 502 报上游失败（Go `smslogin.go:71-75` 逐字）：这里的失败
/// **绝大多数是用户输入问题**（号码错 / 发得太频繁），把它报成「上游异常」会让用户
/// 去查网络，而正确动作是「检查号码、等一分钟再试」。
///
/// ★ 鉴权是必需的：这个端点会**向真实手机号发短信**（可能触发上游频控甚至计费），
/// 未鉴权暴露等于给任何人一个刷短信的开关。
async fn sms_send(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: Request<axum::body::Body>,
) -> Response {
    if let Some(r) = check_auth(&state, &headers) {
        return r;
    }
    let raw = axum::body::to_bytes(request.into_body(), 1 << 16)
        .await
        .unwrap_or_default();
    let Ok(v) = serde_json::from_slice::<Value>(&raw) else {
        return openai_error(StatusCode::BAD_REQUEST, "invalid_request", "invalid JSON body");
    };
    let phone = v.get("phone").and_then(|x| x.as_str()).unwrap_or("").trim();
    let region = v.get("region").and_then(|x| x.as_str()).unwrap_or("");
    if phone.is_empty() {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "手机号不能为空",
        );
    }

    match state.client.send_sms(sms_base_for(region), phone).await {
        Ok(expires_in) => json_response(StatusCode::OK, json!({ "ok": true, "expiresIn": expires_in })),
        Err(e) => openai_error(
            StatusCode::BAD_REQUEST,
            "sms_send_failed",
            &e.to_string(),
        ),
    }
}

/// `POST /login/sms/verify` —— 用验证码换 token 并**登记账号**（需鉴权）。
///
/// 请求 `{"phone", "smsCode", "region"}` → 响应 `{"ok": true, "account": {...}}`。
///
/// # 为什么分成两步而不是一次调用
///
/// 验证码是**用户从手机上看来的**，天然有两次人机交互。合成一个接口就必须把
/// 「等用户输入」做成服务端会话状态 —— 而网关是无状态转发的，那会引入一个不必要的
/// 内存表（Go `smslogin.go:11-14` 逐字）。
///
/// # 登记为什么在网关侧（与 Go 的分工不同）
///
/// Go 侧登记走宿主回调（`cmd/server/smslogin.go`），理由是「账号库是宿主的数据结构，
/// 网关直接写会绕过宿主的校验与缓存」。**Rust 侧宿主没有这个回调**，而宿主的
/// `modules::gateway::sms_verify` 注释明确写着「登记在网关侧完成（凭证目录归它管），
/// 宿主只转发」—— 那就是本实例的契约，故这里自己写凭证文件并登记进池。
///
/// ★ **先落盘再进池**（Go `cmd/server/smslogin.go:88-92` 逐字）：反过来的话进池成功、
/// 落盘失败 ⇒ 账号能用但**重启即丢**，用户会觉得「昨天加的号今天没了」。
///
/// ★ 失败时**明确报错**而不是「换了 token 但没人存」：后者会让用户以为登录成功了，
/// 重启后发现账号不见了。
async fn sms_verify(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: Request<axum::body::Body>,
) -> Response {
    if let Some(r) = check_auth(&state, &headers) {
        return r;
    }
    let raw = axum::body::to_bytes(request.into_body(), 1 << 16)
        .await
        .unwrap_or_default();
    let Ok(v) = serde_json::from_slice::<Value>(&raw) else {
        return openai_error(StatusCode::BAD_REQUEST, "invalid_request", "invalid JSON body");
    };
    // ★ 驼峰 `smsCode`（宿主发过来的就是这个名字）。
    let phone = v.get("phone").and_then(|x| x.as_str()).unwrap_or("").trim();
    let sms_code = v.get("smsCode").and_then(|x| x.as_str()).unwrap_or("").trim();
    let region = normalize_sms_region(v.get("region").and_then(|x| x.as_str()).unwrap_or(""));
    if phone.is_empty() || sms_code.is_empty() {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "手机号与验证码都不能为空",
        );
    }

    let tokens = match state
        .client
        .login_with_sms(sms_base_for(&region), phone, sms_code)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "sms_login_failed",
                &e.to_string(),
            )
        }
    };

    match register_sms_account(&state, phone, &region, &tokens) {
        Ok(acct) => json_response(StatusCode::OK, json!({ "ok": true, "account": acct })),
        Err(e) => openai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "sms_register_failed",
            &e,
        ),
    }
}

/// 短信登录的 base URL。
///
/// ★ 与 [`crate::upstream::client::Client::chat_base`] 分开：那个需要一个 `Auth`
/// 才能判区域，而登录时**还没有账号对象**（那正是本流程的产物）。
/// Go `smslogin.go:33-40` 逐字。
fn sms_base_for(region: &str) -> &'static str {
    match region.trim().to_ascii_lowercase().as_str() {
        "intl" | "ai" | "international" => "https://www.workbuddy.ai",
        _ => "https://www.workbuddy.cn",
    }
}

/// 归一化短信登录的区域参数。
///
/// ★ 空值默认成**国服**而不是报错：绝大多数用户是国服，界面上默认选中的也是国服。
/// 要求显式传会让不关心区域的调用方白失败一次（Go `smslogin.go:129-137` 逐字）。
fn normalize_sms_region(region: &str) -> String {
    match region.trim().to_ascii_lowercase().as_str() {
        "intl" | "ai" | "international" => "intl".to_string(),
        _ => "cn".to_string(),
    }
}

/// 落凭证文件 + 登记进账号池。
///
/// 返回给界面展示的账号信息（形状与 Go `cmd/server/smslogin.go:95-101` 一致）。
fn register_sms_account(
    state: &AppState,
    phone: &str,
    region: &str,
    tokens: &crate::upstream::SmsTokens,
) -> std::result::Result<Value, String> {
    // ★ 必须自己从 JWT 解 uid：短信登录的响应**只给 token、不给 uid**（与 OAuth
    // 不同），不解析就没法给账号起文件名，也就无法落库。解析失败时**明确报错**
    // 而不是编一个随机 uid —— 随机 uid 会让同一个手机号每次登录都产生一个新账号，
    // 用户看到一堆重复账号，而每个都指向同一份真实凭证。
    let uid = uid_from_access_token(&tokens.access_token)
        .map_err(|e| format!("无法从登录令牌解析账号标识: {e}"))?;

    let domain = if region == "intl" {
        "www.workbuddy.ai"
    } else {
        "www.workbuddy.cn"
    };
    // ★ 文件名前缀必须是 `workbuddy`，否则 `auth::load_dir` 的 glob
    // `workbuddy*.json` 扫不到它 —— 表现为「登录成功但重启后账号不见了」。
    let file = std::path::Path::new(&state.auth_dir)
        .join(format!("workbuddy-{}.json", sanitize_file_part(&uid)));

    let mut a = crate::auth::Auth {
        access_token: tokens.access_token.clone(),
        refresh_token: tokens.refresh_token.clone(),
        uid: uid.clone(),
        domain: domain.to_string(),
        product: crate::auth::PRODUCT_WORKBUDDY.to_string(),
        file_path: file.to_string_lossy().to_string(),
        ..Default::default()
    };
    a.nickname = String::new();

    std::fs::create_dir_all(&state.auth_dir)
        .map_err(|e| format!("创建凭证目录失败: {e}"))?;
    // ★ 先落盘（见函数 doc）。
    a.save_atomic().map_err(|e| format!("写入凭证失败: {e}"))?;

    // 落盘成功后才进池：upsert 语义，同 uid 重复登录只更新凭证，不产生第二个账号。
    {
        let mut pool = match state.pool.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        pool.add(a.clone());
    }

    Ok(json!({
        "uid": uid,
        "phone": phone,
        "region": region,
        "domain": domain,
        "nickname": "",
        "file": a.file_path,
    }))
}

/// 从 accessToken（JWT）里解出账号标识。
///
/// 按序试 `uid` / `userId` / `user_id` / `sub` 四个声明名，取**字符串且 trim 非空**的
/// 第一个（Go `cmd/server/smslogin.go:104-141` 逐字）。
///
/// ★ base64url **不带 padding**：补 `=` 与长度修正都要做，否则偶发号码会解出
/// `illegal base64 data`（Go 侧注释逐字）。故这里先补 padding 试标准 URL-safe 表，
/// 失败再退回 no-pad 表。
fn uid_from_access_token(tok: &str) -> std::result::Result<String, String> {
    use base64::Engine as _;

    let parts: Vec<&str> = tok.split('.').collect();
    if parts.len() < 2 {
        return Err(format!("令牌不是 JWT 格式（{} 段）", parts.len()));
    }
    let payload = parts[1];
    let mut padded = payload.to_string();
    // ★ 不要写成 `if let rem = len % 4`：`%` 永远产出值 ⇒ 该 `if let` 恒真，
    // 编译期报 `irrefutable if let pattern`，而 `rem == 0` 时下面的
    // `repeat(4 - rem)` 会补 4 个 `=`（正确行为是**不补**）。
    let rem = padded.len() % 4;
    if rem != 0 {
        padded.push_str(&"=".repeat(4 - rem));
    }
    let raw = base64::engine::general_purpose::URL_SAFE
        .decode(padded.as_bytes())
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.as_bytes()))
        .map_err(|e| format!("解析令牌负载失败: {e}"))?;

    let claims: Value =
        serde_json::from_slice(&raw).map_err(|e| format!("解析令牌声明失败: {e}"))?;
    for key in ["uid", "userId", "user_id", "sub"] {
        if let Some(s) = claims.get(key).and_then(|v| v.as_str()) {
            let s = s.trim();
            if !s.is_empty() {
                return Ok(s.to_string());
            }
        }
    }
    Err("令牌里没有 uid/userId/user_id/sub 任一声明".to_string())
}

/// 把 uid 里不适合做文件名的字符换掉。
///
/// ★ uid 通常是个 UUID（安全），但**不能假设**：它来自上游返回的 JWT。
/// 含 `/` 或 `..` 的 uid 会把凭证写到目录之外 —— 那是真实的路径穿越风险
///（Go `cmd/server/smslogin.go:134-142` 逐字）。空结果回落 `unknown`。
fn sanitize_file_part(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}

/// 取「两区真值 + 两区是否有可用探测账号」快照。
///
/// ★ 探测账号必须在**短暂持池锁**时取好再发请求：`MutexGuard` 不能跨 `await`，
/// 而探测期间也不该占着池锁（Go `capability.go:187` 同样先解锁再发请求）。
///
/// ★ 用「探测账号」而**不是**「可用账号」：被标 `no_route` 的账号不接流量，却常常是
/// 某区真值的**唯一**来源。Go `pool.go:3369-3373` 实测踩过：改用可用账号后
/// `deepseek-v4.1-flash` 明明两区都有，界面却标「仅国服」。
async fn region_snapshot(
    state: &Arc<AppState>,
) -> (
    HashMap<Region, Option<Vec<crate::upstream::ModelInfo>>>,
    HashMap<Region, bool>,
) {
    let mut fetched: HashMap<Region, Option<Vec<crate::upstream::ModelInfo>>> = HashMap::new();
    let mut has_account: HashMap<Region, bool> = HashMap::new();
    for region in [Region::CN, Region::Intl] {
        let probe = {
            let pool = match state.pool.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            crate::models_cache::probe_accounts_in_region(&pool, region)
        };
        has_account.insert(region, !probe.is_empty());
        let infos =
            crate::models_cache::fetch_models_for_region(&state.models, &state.client, &probe, region)
                .await;
        fetched.insert(region, if infos.is_empty() { None } else { Some(infos) });
    }
    (fetched, has_account)
}

/// 该产品**实际有账号**的区域（用于生成 `qoder:国服:xxx` 这类三段组合名）。
///
/// 无脑给所有区域会造出「没有账号可用」的名字，用户选中后得到「账号不可用」，
/// 比不显示更糟（Go `capability.go:1282-1294` 逐字）。
fn product_regions_of(state: &AppState, product: &str) -> Vec<Region> {
    let pool = match state.pool.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    pool.product_regions(product)
}

/// `?refresh` 判定：`1` / `true` / `yes`（大小写不敏感、两侧空白忽略）。
///
/// 与 Go `wantsModelRefresh` 同口径 —— 三个值都收，因为界面、curl 脚本、旧文档
/// 各自用过其中一种写法，只认 `1` 会让另外两种「看起来没生效」。
fn wants_model_refresh(query: Option<&str>) -> bool {
    let v = query_param(query, "refresh").trim().to_ascii_lowercase();
    v == "1" || v == "true" || v == "yes"
}

/// 取查询参数（与 Go `r.URL.Query().Get` 同口径：`+` 与 `%XX` 都会解码）。
///
/// 用 `url::form_urlencoded` 而不是手切 `strip_prefix`：任务名可以是中文
///（如「夜猫子」），客户端会把它百分号编码 —— 不解码就永远匹配不上。
fn query_param(query: Option<&str>, key: &str) -> String {
    let Some(q) = query else {
        return String::new();
    };
    url::form_urlencoded::parse(q.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

/// `POST /v1/chat/completions` —— OpenAI Chat Completions 入口。
///
/// 流程：读体 → 鉴权 → 取流式标志与会话键 → 交给 [`crate::forward::forward_chat`]
/// 完成「选号 → 刷新 token → 转发 → 失败换号」，再按请求是否流式分别：
///   - 流式：逐帧规范化后透传 SSE
///   - 非流式：上游 SSE 已聚合为 `chat.completion`，直接返回
///
/// 错误码与 Go 侧契约一致：默认 503 `no_healthy_account`，
/// 只有**请求侧**错误（单一模型拒绝、上下文超长）才偏离，让客户端看到真实原因。
async fn chat_completions(
    State(state): State<Arc<AppState>>,
    request: Request<axum::body::Body>,
) -> Response {
    let headers = request.headers().clone();
    if let Some(r) = check_auth(&state, &headers) {
        return r;
    }
    let body = match read_body(request, Wire::Chat).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    #[derive(serde::Deserialize)]
    struct Peek {
        #[serde(default)]
        stream: bool,
    }
    let stream = serde_json::from_slice::<Peek>(&body).map(|p| p.stream).unwrap_or(false);

    // 会话粘性键：OpenAI 侧用 metadata.conversation_id / conversation_id。
    let sess_key = if state.session.is_some() {
        crate::session::extract_key(&body)
    } else {
        String::new()
    };

    let (result, status, failure) = run_forward(&state, &body, stream, &sess_key).await;
    if let Some(f) = failure {
        let code = Wire::Chat.failure_code(f.kind);
        // ★ `client_facing_status`：上游 403 → **502**（Go `forward.go:1035`）。
        //
        // 为什么必须改：DSH 的 `classifyPiAiError` 会把文案里的裸 401/403 读成
        // 「API 密钥无效」，而 pi-ai 会把 `(status): body` 原样拼进 message ⇒
        // 状态码留 403 等于把「上游拒绝了这个账号」说成「你的密钥不对」。
        //
        // ★ 401 **不**改写：那是我们自己的鉴权结果，必须保持 401。
        return openai_error(
            StatusCode::from_u16(crate::clienttext::client_facing_status(f.status))
                .unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            code,
            &f.message,
        );
    }

    match result {
        Some(crate::forward::ChatResult::Stream {
            uid,
            model,
            product,
            region,
            response,
            prefix,
        }) => chat_stream_response(
            response,
            state.clone(),
            uid,
            model,
            product,
            region,
            prefix,
        ),
        Some(crate::forward::ChatResult::Response {
            body,
            uid,
            model,
            product,
            region,
        }) => {
            record_usage(&state, &uid, &model, &product, &region, body.get("usage"));
            json_response(StatusCode::OK, Value::Object(body))
        }
        None => openai_error(
            StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            "no_healthy_account",
            "all accounts unavailable (cooling/disabled)",
        ),
    }
}

/// `GET /usage` —— 网关侧 Token 用量聚合快照（宿主「Token 统计」页读它）。
///
/// 与本地客户端日志统计**相互独立**：这里统计的是网关自己记录的每次成功请求的
/// 上游 usage。`?days=N` 限定最近 N 天；缺省或非正数表示全部历史。
async fn usage_report(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: axum::http::Uri) -> Response {
    if let Some(r) = check_auth(&state, &headers) {
        return r;
    }
    let days = uri
        .query()
        .and_then(|q| {
            q.split('&')
                .find_map(|kv| kv.strip_prefix("days="))
                .and_then(|v| v.parse::<i64>().ok())
        })
        .filter(|d| *d > 0)
        .unwrap_or(0);
    let mut snap = state.usage.snapshot(days);
    if let Some(obj) = snap.as_object_mut() {
        obj.insert("enabled".into(), json!(true));
    }
    json_response(StatusCode::OK, snap)
}

/// 记录一次成功请求的用量。
///
/// 只统计**上游返回了可用 usage** 的请求（失败请求不计入）—— 与 Go 侧
/// `recordUsage` 的 `hasCounters` 门控一致。
///
/// `product` / `region` 是**计费归属**（见 [`crate::usage::Billing`]）：
/// - `product` 空 → 归一成 `workbuddy`（WorkBuddy 在 auth 层就用空串表示产品）
/// - `region` 空 → 该平台不分区，保持空（**不编造**区域）
/// - 倍率只有 WorkBuddy 有：Qoder/ZCode 的 `has_multiplier` 保持 `false`，
///   界面不显示倍率列 —— 宁可不说，也不给一个可能错的数字
fn record_usage(
    state: &Arc<AppState>,
    uid: &str,
    model: &str,
    product: &str,
    region: &str,
    usage: Option<&Value>,
) {
    let Some(c) = crate::usage::parse_openai_usage(usage) else {
        return;
    };
    let product = if product.is_empty() {
        crate::auth::PRODUCT_WORKBUDDY
    } else {
        product
    };
    let mut billing = crate::usage::Billing {
        product: product.to_string(),
        region: region.to_string(),
        ..Default::default()
    };
    if product == crate::auth::PRODUCT_WORKBUDDY {
        billing.has_multiplier = true;
        // ★ 倍率数据源是上游 `/v3/config` 的能力索引（Go `buildCapabilityIndex()`）。
        //
        // ★★ 只用**缓存里的**真值（[`crate::models_cache::ModelsCache::snapshot`]），
        // 不发网络请求：这条路径在请求的关键路径上，用户等的是回答而不是我们的账单
        // 数字。与 Go 的差异只在「缓存恰好过期且退避窗口外」这一种情况下显现，
        // 取不到时一律保持 `None`。
        //
        // ★★ 取不到就是 `None`（= 未声明），**绝不编造数字** —— 编造的倍率会让用户
        // 据它判断该烧哪个账号的额度，而那个判断一旦错就是真金白银。这也正是 Go 的
        // 行为：`fillBilling` 在 `idx == nil` 或该区域没有该模型时同样保持 nil。
        billing.multiplier = billing_multiplier_for(state, region, model);
    }
    state.usage.record_billed(uid, model, billing, c);
}

/// 从缓存的能力索引里取该区域该模型的积分倍率；取不到返回 `None`。
///
/// ★ 区域名对不上（如 `""` = 不分区、或拼错）时返回 `None` 而**不是**回落到国服：
/// 倍率是**按区域**不同的（`deepseek-v4.1-flash` 国服 `x0.03` / 国际版 `x0.00`），
/// 猜一个区域等于给用户一个错到 3 倍的数字。
fn billing_multiplier_for(state: &AppState, region: &str, model: &str) -> Option<f64> {
    let r = crate::auth::Region::parse(region);
    if r == crate::auth::Region::Any {
        return None;
    }
    let fetched = state.models.snapshot();
    let mut has_account = HashMap::new();
    for reg in [Region::CN, Region::Intl] {
        // 复用探测账号判据（含 `no_route`、排除 `disabled`）：与 `/v1/models`
        // 的缺口判定同口径，否则同一条数据会出现两种解释。
        let has = {
            let pool = match state.pool.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            crate::models_cache::has_probe_account(&pool, reg)
        };
        has_account.insert(reg, has);
    }
    let idx = crate::capability::CapabilityIndex::build(&fetched, &has_account);
    idx.multiplier(r, model)
}

/// 请求体上限。
///
/// 从 8MB 提到 32MB：长对话（Claude Code / Codex 一轮带上大量文件内容与工具结果）
/// 很容易突破 8MB，而**静默截断**会把合法 JSON 切成半截字节透传给上游，
/// 上游报 `unexpected EOF`，表现为「请求参数有误」—— 客户端完全无法定位到是网关截断。
const MAX_REQUEST_BODY: usize = 32 << 20;
/// 流式响应的空闲心跳间隔。
///
/// 上游思考阶段会长时间零字节（实测 DeepSeek 推理模型可达 20 s 以上），
/// 而 Codex 自带 5 min 空闲超时、各类反代/负载均衡通常 60~120 s。周期性发一个
/// **真事件**能同时压住这两类超时。
///
/// 为什么不用 SSE 注释帧（`: ping`）：`eventsource-stream` 把注释行直接丢弃，
/// `stream.next()` 根本不返回，定时器**不会**被重置 —— 心跳必须是被解析的真事件。
const STREAM_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// 各协议在「请求体超限 / 读取失败」两种情形下使用的错误码。
///
/// 三家协议的词汇表不同：OpenAI 用 `payload_too_large` / `invalid_request`，
/// Anthropic 用 `request_too_large` / `invalid_request_error`。客户端按自家词汇表
/// 分支处理，混用会让错误提示退化成未知错误。
#[derive(Debug, Clone, Copy)]
struct BodyErrorCodes {
    too_large: &'static str,
    bad_request: &'static str,
}

/// OpenAI 系（chat/completions、responses）的错误码。
const OPENAI_BODY_CODES: BodyErrorCodes = BodyErrorCodes {
    too_large: "payload_too_large",
    bad_request: "invalid_request",
};
/// Anthropic Messages 的错误码。
const ANTHROPIC_BODY_CODES: BodyErrorCodes = BodyErrorCodes {
    too_large: "request_too_large",
    bad_request: "invalid_request_error",
};

/// 协议形状：决定错误体与失败码的词汇表。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wire {
    /// OpenAI Chat Completions。
    Chat,
    /// OpenAI Responses。
    Responses,
    /// Anthropic Messages。
    Messages,
}

impl Wire {
    /// 该协议在请求体出错时使用的码。
    fn body_codes(self) -> BodyErrorCodes {
        match self {
            Wire::Messages => ANTHROPIC_BODY_CODES,
            _ => OPENAI_BODY_CODES,
        }
    }

    /// 生成该协议形状的错误响应。
    ///
    /// ★ 三端共用这一处，故 [`crate::clienttext::client_safe_message`] 的闸门装在
    /// 这里就能覆盖 Messages / Responses 两个协议（Chat 那支由 [`openai_error`]
    /// 自己过闸门）。Go 侧对应 `messages.go:463` / `responses.go:464`。
    ///
    /// 幂等，故 Chat 路径过两道不会改变结果。
    fn error(self, status: StatusCode, code: &str, msg: &str) -> Response {
        let msg = crate::clienttext::client_safe_message(msg);
        match self {
            Wire::Messages => {
                json_response(status, crate::protocol::anthropic::anthropic_error(code, &msg))
            }
            Wire::Responses => {
                json_response(status, crate::protocol::responses::responses_error(code, &msg))
            }
            Wire::Chat => openai_error(status, code, &msg),
        }
    }

    /// 把转发失败翻译成该协议的错误码。
    ///
    /// 三个入口共享「谁来判定失败类别」这条映射链，但**码面值不同** ——
    /// 把码面值也一起统一会让各协议的词汇表互相串味：
    ///
    /// - 上下文超长：Chat / Responses 用 `context_length_exceeded`；
    ///   Anthropic 语义里它是 `invalid_request_error`（其真实文案即
    ///   "prompt is too long: ..."），而非 `request_too_large`（那是字节数超限）。
    /// - 请求被上游拒绝（11155 / 11140）：三端统一用 `invalid_request_error`
    ///   语义；HTTP 状态与原文由调用方保留上游值，客户端据此识别为请求侧失败。
    /// - 单一模型拒绝：Chat 用 `model_not_allowed`（本网关为 chat 形状定的码），
    ///   Responses / Anthropic 用各自的 `invalid_request_error`。
    ///
    ///   ★ 为什么 Responses / Anthropic **不**直接透出 `model_not_allowed`：
    ///   那是本网关给 chat/completions 形状定的码，不属于它们的词汇表
    ///   （Responses 用 `invalid_request` / `payload_too_large`）。三个入口共享的是
    ///   「谁来判定失败类别」这条映射链，**不是同一个码面值** —— 把码面值也一起
    ///   统一会让各协议的词汇表互相串味（Go `handler.go:1197-1204` 逐字）。
    /// - 用户白名单不放行该「平台 × 模型」：Chat 用 `model_not_in_product`
    ///   （与 Go `handler.go:1101` 的 `return "model_not_in_product"` 逐字一致）；
    ///   另两个协议用各自的 `invalid_request_error` —— 它是**请求侧**错误
    ///   （要改白名单或换模型），不是可用性问题，故不能落进 `api_error` /
    ///   `upstream_error`，否则用户会去查账号池，而账号一个都没问题。
    /// - 全部账号被设为不参与选号：**不是请求侧错误**（请求本身没毛病），
    ///   故与 [`crate::forward::FailureKind::Upstream`] 同码 —— Chat 沿用
    ///   `no_healthy_account`（Go 侧 `FailureAllNoRoute` 也落在同一个码上，
    ///   区别只在 message：那里逐字说明了「这不是故障，去打开路由开关」），
    ///   另两个协议用 `api_error` / `upstream_error`。
    /// - **上游**（Qoder/ZCode）清单里没有这个模型：请求侧错误（400），
    ///   Chat 用 `model_not_in_upstream`。
    ///
    ///   ★★ 与 `model_not_in_product` **必须是两个码**（Go `handler.go:1103-1110`
    ///   逐字）：那个是「去白名单里放行」，这个是「白名单放行了也没用，上游就没有
    ///   这个模型」，**出路恰好相反**。Go 原话 —— 「独立码是必需的：混用会让用户
    ///   反复去配置页加一个不存在的模型。」
    ///
    ///   另两个协议用各自的 `invalid_request_error`（同为请求侧，且这两个码不属于
    ///   它们的专属词汇表）。
    /// - 账号额度耗尽（402）：**账号级**问题，不是请求的毛病，也不是可用性
    ///   抖动。Chat 用 `quota_exhausted`（Go `handler.go:1119-1121` 逐字）；
    ///   另两个协议统一用 `invalid_request_error`（Go `handler.go:1168-1170`
    ///   Anthropic 与 `:1219-1221` Responses 都是它）。
    ///
    ///   ★ 为什么不能用 `upstream_error`（Responses 侧曾经的写法）：那会让客户端
    ///   以为「上游暂时坏了，重试即可」，而额度耗尽**重试多少次都一样**，要做的
    ///   是换号或充值。402 状态码本身已传达「要充值」。
    /// - 上游要求人机验证（**503**）：**请求侧**错误（换个号打同一请求也会被要求），
    ///   与 `ModelNotInProduct` 同处置：Chat 用 `captcha_required`，另两个协议
    ///   用各自的 `invalid_request_error`。
    /// - 思考档位被拒 / 11102 模型不在通道 / 11133 参数被拒：三者都是**请求侧**
    ///   错误，但**码面值各不相同**（Go `openAIFailure` 逐条）：
    ///   Chat 分别用 `reasoning_effort_rejected` / `model_not_in_region` /
    ///   `model_param_invalid`；另两个协议统一 `invalid_request_error`。
    ///
    ///   ★ 为什么不能合并成一个 `invalid_request_error`：三者的**出路完全不同**
    ///   —— 换档位 / 换模型或加区域前缀 / 改参数。合并后客户端只能一律提示
    ///   「请求有问题」，用户不知道该改哪一项（Go `handler.go:1080-1117` 逐字）。
    ///   ★ 状态码一律保留**上游原值**（通常 400），网关不改写 —— 上游用 400 表达
    ///   「请求有问题」，这个语义必须原样到达客户端；**绝不走 503**（那会让客户端
    ///   去重试而不是改请求，Go `:1082-1084` 逐字）。
    /// - 3012 熔断 / 出口 IP 被 WAF 拦（均 **503**）：**可用性**问题，但不是
    ///   「等一会儿就好」——Chat 分别用 `unusual_activity` / `egress_ip_blocked`。
    ///
    ///   ★★ **绝不落进 `no_healthy_account`**（Go `handler.go:1325-1331`
    ///   与 `:1122-1132` 逐字）：那个码对客户端意味着「账号池暂时不可用，稍后
    ///   重试」，而这里的结论是「重试（含换号）无用」—— 3012 与账号无关
    ///   （同一账号一分钟后即可用），WAF 拦的是网络出口。Go 原话：「让客户端
    ///   继续重试恰好是当前最该避免的事（会继续放大风控）」；3012 那条更写着
    ///   「客户端据此可以**不要立即重试** —— 这正是所有者要的效果」。
    ///
    ///   另两个协议仍用 `api_error` / `upstream_error`（Go `anthropicFailure`
    ///   `:1164-1166` 与 `responsesFailure` `:1215-1217`）。
    /// - 其余：Chat 沿用 `no_healthy_account`；另两个协议用 `api_error` / `upstream_error`。
    fn failure_code(self, kind: crate::forward::FailureKind) -> &'static str {
        use crate::forward::FailureKind as F;
        match (self, kind) {
            (Wire::Chat, F::ContextTooLong) => "context_length_exceeded",
            (Wire::Responses, F::ContextTooLong) => "context_length_exceeded",
            (Wire::Messages, F::ContextTooLong) => "invalid_request_error",
            (_, F::RequestRejected) => "invalid_request_error",
            // ★ 白名单拒绝：Chat 用专属码 `model_not_allowed`（Go `errorCodeFor`
            // 的 `model_not_allowed`），另两个协议用各自的 invalid_request_error。
            //
            // 为什么必须是专属码而不是沿用 no_healthy_account：后者对客户端意味着
            // 「账号池暂时不可用，稍后重试」，而白名单拒绝**重试多少次都一样** ——
            // 要改的是请求（换模型）或配置（去「放行模型」里加）。把用户引去查
            // 账号池，而账号一个都没问题（Go `handler.go:1296-1305` 逐字）。
            (Wire::Chat, F::ModelNotAllowed) => "model_not_allowed",
            (Wire::Messages, F::ModelNotAllowed) => "invalid_request_error",
            (Wire::Responses, F::ModelNotAllowed) => "invalid_request_error",
            // ★ 请求侧三兄弟，码面值**各不相同**（Go `openAIFailure`
            // `:1080-1117` 逐条）：三者的出路分别是「换档位」「换模型或加区域
            // 前缀」「改参数」。合并成一个 invalid_request_error 会让客户端
            // 只能提示「请求有问题」，用户不知道该改哪一项。
            //
            // ★ 为什么 Chat 用专属码而另两个协议统一 invalid_request_error：
            // 那两个码是本网关给 chat/completions 形状定的，不属于它们的词汇表
            //（Go `anthropicFailure` / `responsesFailure` 各自逐条都归
            // invalid_request_error）。
            (Wire::Chat, F::EffortRejected) => "reasoning_effort_rejected",
            (Wire::Messages, F::EffortRejected) => "invalid_request_error",
            (Wire::Responses, F::EffortRejected) => "invalid_request_error",
            // ★ 11102：所有者 2026-09-20 现场 —— 此前落进 no_healthy_account 显示
            // 「all accounts unavailable (cooling/disabled)」，他判断成「这个模型
            // 不能用/要排队」，而真实原因是**区域没指定**，等多久都不会变。
            (Wire::Chat, F::ModelNotInRegion) => "model_not_in_region",
            (Wire::Messages, F::ModelNotInRegion) => "invalid_request_error",
            (Wire::Responses, F::ModelNotInRegion) => "invalid_request_error",
            // ★ 11133：`param` 是空串、未必与长度有关，故**不**与 ContextTooLong
            // 合并（后者会让客户端触发自动压缩，而压缩在这里无济于事）。
            (Wire::Chat, F::ModelParamInvalid) => "model_param_invalid",
            (Wire::Messages, F::ModelParamInvalid) => "invalid_request_error",
            (Wire::Responses, F::ModelParamInvalid) => "invalid_request_error",
            (Wire::Chat, F::ImageRegionUnavailable) => "image_region_unavailable",
            (Wire::Messages, F::ImageRegionUnavailable) => "invalid_request_error",
            (Wire::Responses, F::ImageRegionUnavailable) => "invalid_request_error",
            (Wire::Chat, F::ModelNotInProduct) => "model_not_in_product",
            (Wire::Messages, F::ModelNotInProduct) => "invalid_request_error",
            (Wire::Responses, F::ModelNotInProduct) => "invalid_request_error",
            // ★★ **上游**清单里没有它：与上面那条**出路相反**（那个去白名单里
            // 放行，这个放行了也没用），故必须是独立码（Go `handler.go:1103-1110`
            // 逐字：「混用会让用户反复去配置页加一个不存在的模型」）。
            (Wire::Chat, F::ModelNotInUpstream) => "model_not_in_upstream",
            (Wire::Messages, F::ModelNotInUpstream) => "invalid_request_error",
            (Wire::Responses, F::ModelNotInUpstream) => "invalid_request_error",
            // ★★ 平台总开关关掉了：与 `model_not_in_product` **出路相反**
            //（那个去「兼容网关」页改白名单，这个回账号页打开平台开关），
            // 故必须是独立码。落进 model_not_in_product 会让用户改一个
            // 与故障毫无关系的配置，改到天亮也不会好（Go 侧同款教训：
            // `handler.go:1103-1110`）。
            (Wire::Chat, F::PlatformNotInRouting) => "platform_not_in_routing",
            (Wire::Messages, F::PlatformNotInRouting) => "invalid_request_error",
            (Wire::Responses, F::PlatformNotInRouting) => "invalid_request_error",
            (Wire::Chat, F::CaptchaRequired) => "captcha_required",
            (Wire::Messages, F::CaptchaRequired) => "invalid_request_error",
            (Wire::Responses, F::CaptchaRequired) => "invalid_request_error",
            // ★ 账号额度耗尽：Chat 用 `quota_exhausted`（Go `handler.go:1119-1121`
            // 逐字）。另两个协议统一 invalid_request_error —— 尤其 Responses
            // **不能**用 upstream_error：那会让客户端以为「上游暂时坏了，重试即可」，
            // 而额度耗尽重试多少次都一样（Go `handler.go:1219-1221` 逐字）。
            (Wire::Chat, F::QuotaExhausted) => "quota_exhausted",
            (Wire::Messages, F::QuotaExhausted) => "invalid_request_error",
            (Wire::Responses, F::QuotaExhausted) => "invalid_request_error",
            // ── 3012 熔断 / 出口 IP 被 WAF 拦 ──
            //
            // ★★ 两者**绝不落进 `no_healthy_account`**（Go `handler.go:1325-1331`
            // 与 `:1122-1132` 逐字）：那个码对客户端意味着「账号池暂时不可用，
            // 稍后重试」，而这里的结论是「重试（含换号）无用」。
            //
            //   3012：与账号无关（实测同一账号一分钟后即可用、官方客户端也吃它）。
            //     Go 原话：「客户端据此可以**不要立即重试** —— 这正是所有者要的
            //     效果（「3012 后自动退避一段时间不重试」）」。
            //   WAF：拦的是网络出口。Go 原话：「让客户端继续重试恰好是当前最该
            //     避免的事（会继续放大风控）」。
            //
            // 分出新码不是「多一个分支做同一件事」：客户端据这两个码**应该做**
            // 的事（停止立即重试 / 去查代理与公网 IP）与 no_healthy_account
            //（等账号恢复）恰好相反。真实原因同时逐字写在 message 里。
            (Wire::Chat, F::UnusualActivity) => "unusual_activity",
            (Wire::Messages, F::UnusualActivity) => "api_error",
            (Wire::Responses, F::UnusualActivity) => "upstream_error",
            (Wire::Chat, F::EgressIPBlocked) => "egress_ip_blocked",
            (Wire::Messages, F::EgressIPBlocked) => "api_error",
            (Wire::Responses, F::EgressIPBlocked) => "upstream_error",
            // ★ 「这条通道没有该账号的资源包」（ZCode 1113）：绝不能落进
            // `no_healthy_account` —— 那个码意味着「账号池暂时不可用，稍后重试」，
            // 而这里的结论是「这个账号在这个模型上永远不行，换号或换模型即可」。
            //
            // 也**不**复用 `quota_exhausted`：那个码的出路是「去充值 / 等签到」，
            // 而本变体的账号往往额度满格，充值是无效动作。
            //
            // 另两个协议用 `invalid_request_error`（与 QuotaExhausted 同款理由：
            // Responses 用 upstream_error 会让客户端以为「上游暂时坏了，重试即可」）。
            (Wire::Chat, F::NoResourcePack) => "no_resource_pack",
            (Wire::Messages, F::NoResourcePack) => "invalid_request_error",
            (Wire::Responses, F::NoResourcePack) => "invalid_request_error",
            (Wire::Chat, F::Upstream) => "no_healthy_account",
            (Wire::Messages, F::Upstream) => "api_error",
            (Wire::Responses, F::Upstream) => "upstream_error",
            (Wire::Chat, F::AllNoRoute) => "no_healthy_account",
            (Wire::Messages, F::AllNoRoute) => "api_error",
            (Wire::Responses, F::AllNoRoute) => "upstream_error",
        }
    }
}

/// 读取请求体；超限或为空时返回该协议形状的错误响应。
///
/// 超限返回 413 并给出明确原因：客户端据此知道要缩减历史，而不是收到一个
/// 「请求参数有误」然后无从下手（静默截断透传时的表现）。
async fn read_body(
    request: Request<axum::body::Body>,
    wire: Wire,
) -> Result<bytes::Bytes, Response> {
    match axum::body::to_bytes(request.into_body(), MAX_REQUEST_BODY).await {
        Ok(b) if b.is_empty() => Err(wire.error(
            StatusCode::BAD_REQUEST,
            wire.body_codes().bad_request,
            "empty request body",
        )),
        Ok(b) => Ok(b),
        Err(_) => Err(wire.error(
            StatusCode::PAYLOAD_TOO_LARGE,
            wire.body_codes().too_large,
            &format!(
                "request body exceeds {} MB limit; reduce the conversation history or attachment size",
                MAX_REQUEST_BODY >> 20
            ),
        )),
    }
}

/// 执行转发（三个协议入口共用）。
///
/// 返回 `(结果, 状态码, 失败)`。
///
/// # 产品/区域前缀在这里解析（而不是在 `forward_chat` 内部）
///
/// 三种协议（chat / messages / responses）的请求体都会先汇合成 OpenAI Chat
/// 形态再进本函数，因此**一处解析即可覆盖所有出站路径**，不会漏掉任何一条。
/// 解析结果分三路使用：
///
/// - `bare`：剥掉前缀的裸模型名 —— 既用于改写请求体（上游不认识 `cn:` 前缀，
///   原样转发会得 `400 code=11102 model [cn:glm-5.2] service info not found`），
///   也用于图像能力查表（能力索引的键是真实模型名）。
/// - `product`：**强制**约束（不跨平台兜底），交给 `forward_chat`。
/// - `realm`：区域**偏好**，覆盖 `image_route_for` 的自动推断。
async fn run_forward(
    state: &Arc<AppState>,
    body: &[u8],
    stream: bool,
    sess_key: &str,
) -> (
    Option<crate::forward::ChatResult>,
    u16,
    Option<crate::forward::ForwardFailure>,
) {
    // ── 产品/区域前缀解析 ──
    //
    // 最多剥两层，顺序固定为「产品 → 区域」（`qoder:国际版:glm-5.3`）；
    // 认不出的前缀原样保留在裸名里（`deepseek:v3` 的 `deepseek` 不是前缀，
    // 继续剥会把模型名本身吃掉）。无前缀时 product/realm 都是空串 ——
    // 空串表示**不限制**，而不是默认成 cn（默认成 cn 会让所有国际版账号
    // 静默从候选里消失）。
    let raw_model = crate::forward::model_of(body);
    let resolved = crate::resolve_model::resolve_model(&raw_model);

    // ── 系统提示词改写（`prompt.mode`）──
    //
    // ★ 必须排在下面的 model 改写**之前**（Go `forward.go:236-249` 逐字）：两者都会
    // 重新序列化请求体，先做提示词替换可以少一次整体编码，也避免「模型已改写成裸名、
    // 提示词却没换」这种半改状态的中间结果出现在日志/排查视野里。
    //
    // ★ `passthrough`（缺省）时**完全不调用**任何改写函数：既有行为必须逐字不变
    // （不重新序列化、不动 messages），因此这里是**显式分支**而不是「传空串让它空转」。
    //
    // ★ 放在本函数而不是各协议入口：三种协议（chat / messages / responses）的请求体
    // 都在这里汇合成 OpenAI Chat 形态，在此改写只需一处，也不会漏掉任何一条出站路径。
    // ★ 取一次快照（`Arc` 克隆，不是深拷贝）：下面要读两个字段，分开取会在「两次读取
    // 之间发生热重载」时读到**不同版本**的组合（例如新 mode 配旧 text）—— 那正是
    // Go 用 `DynamicSnapshot()` 一次取整个结构要避免的撕裂读。
    let dc = state.dynamic.snapshot();
    let prompted: Option<Vec<u8>> = if dc.prompt_text.is_empty() {
        None
    } else {
        match dc.prompt_mode.as_str() {
            crate::prompt::MODE_CUSTOM => Some(crate::prompt::rewrite(body, &dc.prompt_text)),
            crate::prompt::MODE_APPEND => Some(crate::prompt::append(body, &dc.prompt_text)),
            _ => None,
        }
    };
    let body: &[u8] = prompted.as_deref().unwrap_or(body);

    // 改写请求体：把 `model` 换成裸名，其余字段不动。
    let rewritten;
    let body: &[u8] = if resolved.bare.is_empty() || resolved.bare == raw_model {
        body
    } else {
        rewritten = crate::forward::rewrite_model(body, &resolved.bare);
        &rewritten
    };

    // 区域路由：仅对「图像能力两区不同」的模型 + 带图片的请求生效。
    let mut route = image_route_for(&resolved.bare, crate::forward::request_has_image(body));

    // 显式区域前缀**优先于**自动推断：用户写 `cn:` / `global:` / `国际版:`
    // 就是明确指令，不该被 `image_route_for` 的推断覆盖 —— 那是「猜」，用户是
    // 「说」；二者冲突时以用户为准，推断只在用户没表态时兜底。
    //
    // ⚠ 必须把结果**写回 route**：选号与随后的错误分支用的都是 `route.region`，
    // 只改一个局部变量的话前缀就完全没有效果（Go 侧踩过这个坑：算了正确的值
    // 却没用它，症状是「写了区域前缀却选中另一个区域的账号」）。
    let prefer = crate::resolve_model::realm_to_region(&resolved.realm);
    if prefer != Region::Any {
        route.region = prefer;
    }

    // 池锁只在 forward 内部按操作短暂持有（网络等待期间不持锁），
    // 因此这里不需要先取锁 —— 否则会把整个请求串行化。
    let mut ctx = crate::forward::ForwardCtx {
        pool: &state.pool,
        client: &state.client,
        session: state.session.as_ref(),
        max_rotate: state.max_rotate,
        soft_cooldown: state.soft_cooldown,
        refresh_skew: state.refresh_skew,
        allowed_model: state.allowed_model.clone(),
        products: &state.products,
        unusual: &state.unusual,
        waf_ip: &state.waf_ip,
        probe_timeout: state.probe_timeout,
        // ★ 成本调度：让 forward 在选号前按 (账号区域, 模型名) 把上游声明的
        // 倍率灌进池子（见 `inject_cost_rates`）。这里传的就是
        // `billing_multiplier_for` 用的同一份缓存，口径完全一致。
        models_cache: Some(&state.models),
    };
    crate::forward::forward_chat(&mut ctx, body, stream, sess_key, route, &resolved.product).await
}

/// `POST /v1/messages` —— Anthropic Messages 入口（Claude Code / Claude Desktop）。
///
/// 流程：读体 → 鉴权 → 转成 Chat 形态 → 转发 → 转回 Anthropic 形状。
/// 流式走 [`crate::protocol::anthropic_stream`]，非流式走 `chat_to_anthropic`。
async fn messages(State(state): State<Arc<AppState>>, request: Request<axum::body::Body>) -> Response {
    let headers = request.headers().clone();
    if let Some(r) = check_auth(&state, &headers) {
        return r;
    }
    let body = match read_body(request, Wire::Messages).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    // 先解析出 stream 标志（失败也走同一错误路径）。
    let req = match crate::protocol::anthropic::parse_request(&body) {
        Ok(r) => r,
        Err(e) => {
            return Wire::Messages.error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &e,
            )
        }
    };

    let (chat_body, sess_key) = match crate::protocol::anthropic::anthropic_to_chat(&body) {
        Ok(v) => v,
        Err(e) => {
            return Wire::Messages.error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &e,
            )
        }
    };

    let (result, status, failure) = run_forward(&state, &chat_body, req.stream, &sess_key).await;
    if let Some(f) = failure {
        let code = Wire::Messages.failure_code(f.kind);
        return Wire::Messages.error(
            // ★ 上游 403 → 502（见 `chat_completions` 处的说明）。
            StatusCode::from_u16(crate::clienttext::client_facing_status(f.status))
                .unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            code,
            &f.message,
        );
    }

    match result {
        Some(crate::forward::ChatResult::Stream {
            uid,
            product,
            region,
            response,
            prefix,
            ..
        }) => anthropic_stream_response(
            response,
            &req.model,
            state.clone(),
            uid,
            product,
            region,
            prefix,
        ),
        Some(crate::forward::ChatResult::Response {
            body,
            uid,
            product,
            region,
            ..
        }) => {
            // 非流式：usage 就在聚合后的 body 里，直接记。
            record_usage(&state, &uid, &req.model, &product, &region, body.get("usage"));
            let out = crate::protocol::anthropic::chat_to_anthropic(&body, &req.model);
            json_response(StatusCode::OK, out)
        }
        None => Wire::Messages.error(
            StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            "api_error",
            "all accounts unavailable (cooling/disabled)",
        ),
    }
}

/// `POST /v1/responses` —— OpenAI Responses 入口（Codex CLI）。
///
/// Codex 0.146 起只接受 `wire_api = "responses"`，缺这个入口它无论怎么配都连不上。
async fn responses(State(state): State<Arc<AppState>>, request: Request<axum::body::Body>) -> Response {
    let headers = request.headers().clone();
    if let Some(r) = check_auth(&state, &headers) {
        return r;
    }
    let body = match read_body(request, Wire::Responses).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    let req = match crate::protocol::responses::parse_request(&body) {
        Ok(r) => r,
        Err(e) => {
            return Wire::Responses.error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                &e,
            )
        }
    };

    let (chat_body, sess_key) = match crate::protocol::responses::responses_to_chat(&body) {
        Ok(v) => v,
        Err(e) => {
            return Wire::Responses.error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                &e,
            )
        }
    };

    let (result, status, failure) = run_forward(&state, &chat_body, req.stream, &sess_key).await;
    if let Some(f) = failure {
        let code = Wire::Responses.failure_code(f.kind);
        return Wire::Responses.error(
            // ★ 上游 403 → 502（见 `chat_completions` 处的说明）。
            StatusCode::from_u16(crate::clienttext::client_facing_status(f.status))
                .unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            code,
            &f.message,
        );
    }

    match result {
        Some(crate::forward::ChatResult::Stream {
            uid,
            product,
            region,
            response,
            prefix,
            ..
        }) => responses_stream_response(
            response,
            &req.model,
            state.clone(),
            uid,
            product,
            region,
            prefix,
        ),
        Some(crate::forward::ChatResult::Response {
            body,
            uid,
            product,
            region,
            ..
        }) => {
            // 非流式：usage 就在聚合后的 body 里，直接记。
            record_usage(&state, &uid, &req.model, &product, &region, body.get("usage"));
            let out = crate::protocol::responses::chat_to_responses(&body, &req.model);
            json_response(StatusCode::OK, out)
        }
        None => Wire::Responses.error(
            StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            "upstream_error",
            "all accounts unavailable (cooling/disabled)",
        ),
    }
}

/// 在途名额的 RAII 守卫：**无论流怎么结束，名额一定会还回去**。
///
/// # 为什么不能只靠显式 `release`
///
/// 三个流式入口的用量记账与释放都写在 `async_stream` 生成器的**尾部**，
/// 而生成器的尾部只有在流被完整驱动到底时才会执行。客户端中途断连
/// （关页面 / 断网 / 客户端自己超时放弃）时 axum 直接 drop 响应体，
/// 生成器随之被 drop —— 尾部的 `pool.release` 一次都不会跑，
/// 这个账号的名额就**永久**占着。实测表现：兼容网关页上这个号一直挂着
/// 「正在使用」，而池子 `max_in_flight` 占满之后该号再也不接新请求。
///
/// 所以把释放挪进 `Drop`：生成器被 drop 时守卫也会被 drop，名额照样归还。
/// [`InFlightGuard::release`] 是 `take` 语义（释放过就把 uid 清空），
/// 显式释放与 Drop 兜底**不会重复扣减**。
struct InFlightGuard {
    pool: Arc<Mutex<Pool>>,
    uid: Option<String>,
}

impl InFlightGuard {
    fn new(pool: Arc<Mutex<Pool>>, uid: &str) -> Self {
        Self {
            pool,
            uid: Some(uid.to_string()),
        }
    }

    /// 显式释放（流正常收尾时调用）；重复调用无副作用。
    fn release(&mut self) {
        if let Some(uid) = self.uid.take() {
            if let Ok(mut pool) = self.pool.lock() {
                pool.release(&uid);
            }
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// 把上游 SSE 包成 Anthropic SSE 流，并在流结束后记录用量、释放账号租约。
///
/// 事件序列与顺序约束见 [`crate::protocol::anthropic_stream`] 的模块文档。
///
/// 为什么用量与租约都在这里收尾：流式响应的 usage 只在上游**末帧**才到，
/// 而租约也要等流真正结束才能释放。两者共用同一次流遍历，避免为了统计
/// 把整条流缓存下来（那会让首字节延迟退化）。
fn anthropic_stream_response(
    upstream: reqwest::Response,
    model: &str,
    state_ref: Arc<AppState>,
    uid: String,
    product: String,
    region: String,
    prefix: String,
) -> Response {
    use crate::protocol::anthropic_stream::{AnthropicStreamState, SseOut};
    let mut frames = crate::upstream::sse::SseFramesIter::new(upstream.bytes_stream());
    // ★ 首帧预读消费掉的字节必须最先交回，否则回答会少开头几个字。
    frames.push_prefix(&prefix);
    let model = model.to_string();
    let body = async_stream::stream! {
        // ★ 名额守卫必须在生成器**内部**构造：它随生成器一起被 drop，
        // 客户端断连（生成器被 drop）时也一定会跑。
        let mut guard = InFlightGuard::new(Arc::clone(&state_ref.pool), &uid);
        let mut state = AnthropicStreamState::new(&model);
        let mut out = SseOut::new();
        if state.start(&mut out).is_err() {
            guard.release();
            return;
        }
        yield Ok::<_, std::io::Error>(out.take());
        loop {
            match frames.next_frame().await {
                Some(Ok(text)) => {
                    // 上游帧是 OpenAI 形状；取出 data: 后的 JSON 交给状态机。
                    for line in text.lines() {
                        if let Some(payload) = line.strip_prefix("data: ") {
                            if payload == "[DONE]" {
                                continue;
                            }
                            if let Ok(serde_json::Value::Object(chunk)) =
                                serde_json::from_str::<serde_json::Value>(payload)
                            {
                                if state.consume(&mut out, &chunk).is_err() {
                                    guard.release();
                                    return;
                                }
                            }
                        }
                    }
                    if !out.is_empty() {
                        yield Ok(out.take());
                    }
                }
                Some(Err(e)) => {
                    // 非 EOF 的读错误 = 上游中途断流。必须发 error 并**返回**：
                    // 往下走会照常发 message_stop，而它在 Anthropic 协议里表示
                    // 「本轮正常结束」，客户端会把截断的半截回复当成完整回答收下。
                    let _ = state.write_error(&mut out, &format!("upstream stream error: {e}"));
                    yield Ok(out.take());
                    guard.release();
                    return;
                }
                None => break,
            }
        }
        // 收尾前的失败判定：上游中途发过终止性 error 帧，或吐了内容却没见过
        // [DONE] 就断了（空闲超时掐断 / 上游提前关连接）。
        //
        // 这两种情况都**不能**继续走正常收尾 —— 补一个 stop_reason=end_turn 的
        // message_delta + message_stop 等于向客户端宣告「模型正常答完了」，
        // Claude Code 会把空/截断的回答当成一次成功回合继续推进对话，
        // 用户只看到模型不回答或答了半截，而网关侧 /usage 还记成成功。
        //
        // Anthropic 规范里 error 事件本身就是终止事件，发出后不要再补
        // message_delta / message_stop（那会被理解为正常完成）。
        if let Some(msg) = state.failure_message(frames.saw_done(), frames.saw_any_frame()) {
            let _ = state.close_open_blocks(&mut out);
            let _ = out.write(
                "error",
                &serde_json::json!({
                    "type": "error",
                    "error": {"type": "api_error", "message": msg},
                }),
            );
            yield Ok(out.take());
            guard.release();
            return;
        }
        if state.finish(&mut out).is_ok() && !out.is_empty() {
            yield Ok(out.take());
        }
        // 收尾：记用量 + 释放租约（流已结束，二者都不再依赖连接）。
        record_usage(&state_ref, &uid, &model, &product, &region, state.usage());
        guard.release();
    };
    sse_headers().body(axum::body::Body::from_stream(body)).unwrap_or_else(|_| {
        Wire::Messages.error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            "构造流式响应失败",
        )
    })
}

/// 把上游 SSE 包成 Responses SSE 流，并在流结束后记录用量、释放账号租约。
fn responses_stream_response(
    upstream: reqwest::Response,
    model: &str,
    state_ref: Arc<AppState>,
    uid: String,
    product: String,
    region: String,
    prefix: String,
) -> Response {
    use crate::protocol::anthropic_stream::SseOut;
    use crate::protocol::responses_stream::ResponsesStreamState;
    let mut frames = crate::upstream::sse::SseFramesIter::new(upstream.bytes_stream());
    // ★ 首帧预读消费掉的字节必须最先交回，否则回答会少开头几个字。
    frames.push_prefix(&prefix);
    let model = model.to_string();
    let body = async_stream::stream! {
        // ★ 同上：名额守卫随生成器 drop，客户端断连也会归还名额。
        let mut guard = InFlightGuard::new(Arc::clone(&state_ref.pool), &uid);
        let mut state = ResponsesStreamState::new(&model);
        let mut out = SseOut::new();
        if state.created_event(&mut out).is_err() {
            guard.release();
            return;
        }
        yield Ok::<_, std::io::Error>(out.take());
        loop {
            // 空闲心跳：上游思考阶段可能长时间零字节（实测 20 s+）。这里用
            // timeout 包住「取下一帧」，超时就发一个真事件，把客户端与中间层
            // 的空闲计时器一起压住（见 STREAM_HEARTBEAT_INTERVAL）。
            //
            // 取消是安全的：next_frame 只在收到完整字节片后才推进内部缓冲，
            // 被 timeout 丢弃不会丢数据。
            let next = match tokio::time::timeout(
                STREAM_HEARTBEAT_INTERVAL,
                frames.next_frame(),
            )
            .await
            {
                Ok(v) => v,
                Err(_) => {
                    if state.heartbeat(&mut out).is_ok() && !out.is_empty() {
                        yield Ok(out.take());
                    }
                    continue;
                }
            };
            match next {
                Some(Ok(text)) => {
                    for line in text.lines() {
                        if let Some(payload) = line.strip_prefix("data: ") {
                            if payload == "[DONE]" {
                                continue;
                            }
                            if let Ok(serde_json::Value::Object(chunk)) =
                                serde_json::from_str::<serde_json::Value>(payload)
                            {
                                if state.consume(&mut out, &chunk).is_err() {
                                    guard.release();
                                    return;
                                }
                            }
                        }
                    }
                    if !out.is_empty() {
                        yield Ok(out.take());
                    }
                }
                Some(Err(e)) => {
                    // 上游中途断流：只发 response.failed 并返回，**绝不**再补
                    // response.completed（两者自相矛盾，客户端只认最后一个）。
                    let _ = state.fail(&mut out, &format!("upstream stream error: {e}"));
                    yield Ok(out.take());
                    guard.release();
                    return;
                }
                None => break,
            }
        }
        // 收尾前的失败判定：上游中途发过终止性 error 帧，或吐了内容却没见过
        // [DONE] 就断了（空闲超时掐断 / 上游提前关连接）。
        //
        // 这两种情况都**不能**继续走正常收尾 —— 补一个 stop_reason=end_turn 的
        // message_delta + message_stop 等于向客户端宣告「模型正常答完了」，
        // Claude Code 会把空/截断的回答当成一次成功回合继续推进对话，
        // 用户只看到模型不回答或答了半截，而网关侧 /usage 还记成成功。
        //
        // Responses 协议里终止事件是 response.failed，**不是** Anthropic 的
        // error 事件；发出后同样不能再补 response.completed（两者自相矛盾，
        // 客户端只认最后一个）。
        if let Some(msg) = state.failure_message(frames.saw_done(), frames.saw_any_frame()) {
            let _ = state.fail(&mut out, &msg);
            yield Ok(out.take());
            guard.release();
            return;
        }
        if state.finish(&mut out).is_ok() && !out.is_empty() {
            yield Ok(out.take());
        }
        // 收尾：记用量 + 释放租约。
        record_usage(&state_ref, &uid, &model, &product, &region, state.usage());
        guard.release();
    };
    sse_headers().body(axum::body::Body::from_stream(body)).unwrap_or_else(|_| {
        Wire::Responses.error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            "构造流式响应失败",
        )
    })
}

/// SSE 响应头（三个协议入口共用）。
///
/// `Connection: keep-alive` 与 `X-Accel-Buffering: no` 都是「别缓冲」信号：
/// 少了它们，中间的反代（nginx 默认 proxy_buffering on）会把整条流攒起来，
/// 表现为「等很久、然后一次性刷出」或长时间无字节被判定为空闲超时。
fn sse_headers() -> axum::http::response::Builder {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .header("X-Accel-Buffering", "no")
}

/// 上游吐过内容却没按约定写 `[DONE]` 就断了（空闲超时掐断 / 上游提前关连接）时，
/// 交给客户端的终止文案。
///
/// ★ 措辞是有约束的，别顺手改：DSH 侧 `classifyPiAiError` 用
/// `/stream ended (?:before|without)\b/i` 把这类失败归为 `TRANSPORT`，而
/// `TRANSPORT` 在 `DEFAULT_RETRYABLE_CODES` 里 ⇒ 会自动换账号重试（实测重试
/// 能拿到完整回答）。换个说法就会被归成 `PI_AI_ERROR`（不可重试），用户只能
/// 对着一个残缺的回答发呆 —— 比现在更糟。
const UPSTREAM_TRUNCATED_MSG: &str =
    "upstream stream ended before a completion event; this turn may be incomplete";

/// 构造一帧终止性的 OpenAI 协议 error 帧（自带 `[DONE]`）。
///
/// ★ 为什么是 error 帧而不是补一帧 `choices[0].finish_reason`：
/// 补 `finish_reason` 等于替上游宣告「模型答完了」，客户端会把**被从中间截断**
/// 的半截回答当成一次正常完成收下（DSH 的 `mapStopReason` 见 `"stop"` 就归为
/// 正常停止），用户只看到回答少了半段、网关侧 `/usage` 还记成成功。
/// error 帧则被 openai SDK 直接抛成 `APIError`（`core/streaming.js`：
/// `if (data && data.error) throw new APIError(...)`），客户端拿到明确原因并
/// 按可重试错误处理。
///
/// ★ 文案必须过 [`crate::clienttext::client_safe_message`]：它会进客户端错误
/// 分类器的正则链，裸 401/403 会被误判成鉴权失败。
fn chat_stream_error_frame(msg: &str) -> String {
    let safe = crate::clienttext::client_safe_message(msg);
    let frame = json!({
        "error": {
            "message": safe,
            "type": "upstream_error",
            "code": "upstream_stream_truncated",
        }
    });
    format!("data: {frame}\n\ndata: [DONE]\n\n")
}

/// `text` 是不是 [`crate::upstream::sse::SseFramesIter`] 补出来的**收尾块**。
///
/// ★ 必须能把它和内容帧区分开：`premature_end()` 一旦有值（读到过内容却还没见到
/// `[DONE]`）就**一直**有值，若拿它当唯一判据，error 帧会在**第一个内容帧**上就
/// 发出去，后面的回答内容全被丢掉。收尾块的形态是固定的（见 `finish`）：
/// 截断时 `valid_frames > 0` ⇒ 内容恒为 `data: [DONE]\n\n`。
fn is_tail_block(text: &str) -> bool {
    text.ends_with("data: [DONE]\n\n")
}

/// 把上游 SSE 响应包成 `text/event-stream`，逐帧规范化后流式透传。
///
/// 空流（0 有效帧）时补一帧 error 与 `[DONE]`，与 Go 侧 `upstream.Stream` 一致。
///
/// ★ 异常结束（吐过内容却没见到 `[DONE]`，或读到一半断流）时**必须**补一帧
/// 终止性的 error —— 见 [`chat_stream_error_frame`]。兄弟入口
/// `anthropic_stream_response` / `responses_stream_response` 早就做了同样的判定
/// （`failure_message` + 协议内的终止事件），只有这条 OpenAI 路径漏了，
/// 用户看到的就是「会话中断」。
///
/// 顺带在流末尾抓取上游的 `usage`（它只在末帧到达）记入统计，并释放账号租约 ——
/// 三件事共用同一次流遍历，不需要为了统计把整条流缓存下来。
fn chat_stream_response(
    upstream: reqwest::Response,
    state_ref: Arc<AppState>,
    uid: String,
    model: String,
    product: String,
    region: String,
    prefix: String,
) -> Response {
    let stream = upstream.bytes_stream();
    let mut frames = crate::upstream::sse::SseFramesIter::new(stream);
    // ★ 首帧预读消费掉的字节必须最先交回，否则回答会少开头几个字。
    frames.push_prefix(&prefix);
    let body = async_stream::stream! {
        // ★ 同上：名额守卫随生成器 drop，客户端断连也会归还名额。
        let mut guard = InFlightGuard::new(Arc::clone(&state_ref.pool), &uid);
        let mut last_usage: Option<Value> = None;
        while let Some(item) = frames.next_frame().await {
            match item {
                Ok(text) => {
                    // 抓末帧 usage：只有含 usage 字段的帧才覆盖（避免被后续无
                    // usage 的帧清空）。
                    for line in text.lines() {
                        if let Some(payload) = line.strip_prefix("data: ") {
                            if let Ok(Value::Object(obj)) =
                                serde_json::from_str::<Value>(payload)
                            {
                                if let Some(u) = obj.get("usage") {
                                    if u.is_object() {
                                        last_usage = Some(u.clone());
                                    }
                                }
                            }
                        }
                    }
                    // ★ 上游吐过内容却没按约定写 [DONE]：迭代器会替它补一帧
                    // [DONE] 保证客户端能收尾，但补出来的 [DONE] 会掩盖「回答被
                    // 从中间截断」—— 客户端把它当一次正常完成，用户只看到半截
                    // 回答，网关侧 /usage 还记成成功。必须在 [DONE] **之前**插一帧
                    // error，把这轮显式判成失败。
                    // ★★ 帧序是硬契约，别为了「读起来顺」把它调回来：
                    //
                    // `text` 是迭代器补出来的收尾块，内容恒为 `data: [DONE]\n\n`
                    // （见 `SseFramesIter::finish`）。error 帧若排在它**后面**，
                    // 客户端在读到 `[DONE]` 时就已经把这次流标成「正常完成」，
                    // 后面那帧 error 只会被当成多余数据丢掉 —— 用户看到半截回答，
                    // 网关侧 /usage 还记成成功，等于修复完全没生效。
                    //
                    // 实测（2026-10-04，独立实例 :7873 + 本地假上游）：error 帧排在
                    // 后面时，响应体尾部是 `…\n\ndata: [DONE]\n\ndata: [DONE]\n\n`，
                    // 终端错误码根本没有到达客户端。
                    if is_tail_block(&text) && frames.premature_end().is_some() {
                        // ★★ error 帧**自带** `[DONE]`，所以这里必须 `continue`，
                        // 不能再把 `text` 透传出去 —— 否则响应体里会出现两个
                        // `data: [DONE]`（实测过一次，客户端读到第一个就把这轮标成
                        // 正常完成，后面的 error 被丢掉）。
                        yield Ok::<_, std::io::Error>(chat_stream_error_frame(
                            UPSTREAM_TRUNCATED_MSG,
                        ));
                        continue;
                    }
                    yield Ok::<_, std::io::Error>(text);
                }
                Err(e) => {
                    // 上游读到一半断流。**不能** yield Err：axum 的
                    // `Body::from_stream` 遇到 Err 会直接中止响应体，客户端拿到的
                    // 是一个「没有终止帧的半截 SSE 流」，只能靠自己猜 —— 实测 DSH
                    // 侧的 billion-context 插件会替它合成一帧含糊的
                    // 「upstream stream ended before a completion event; this turn
                    // may be incomplete」，用户看到的是「会话中断」而拿不到真实原因。
                    // 改为发一帧 OpenAI 协议 error + [DONE]，让流在协议层正常收尾，
                    // 客户端按明确的错误码自动换账号重试（实测重试能拿到完整回答）。
                    yield Ok::<_, std::io::Error>(chat_stream_error_frame(&format!(
                        "upstream stream ended before the completion event: {e}"
                    )));
                    break;
                }
            }
        }
        record_usage(&state_ref, &uid, &model, &product, &region, last_usage.as_ref());
        guard.release();
    };
    sse_headers()
        .body(axum::body::Body::from_stream(body))
        .unwrap_or_else(|_| {
            openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "构造流式响应失败",
            )
        })
}

/// 「带图片的该模型请求」应使用的区域。
///
/// 背景（实测逐账号 × 逐模型发图验证）：`glm-5.3` / `glm-5.2` 是**两区共有**的
/// 模型名，但两区是**不同的后端模型** —— 国服后端能读图，国际版把图片替换成
/// 固定占位符（`prompt_tokens` 增量恒为 +33，与图片体积无关），模型只能回
/// 「无法查看图片」。池里两区账号混用而选号只看到期日与冷却、不看区域，
/// 于是同一个模型名会随机命中两个后端 —— 用户看到「时好时坏」。
///
/// 三种情形：
/// - 不带图片 → 不约束。两区文本能力都正常，没必要为纯文本放弃一半账号的额度。
/// - 带图片 + 该模型**只在一区**可读 → 强制该区域，选不出就报错。
/// - 带图片 + 两区都能读 / 无实测结论 → 不约束（不做偏好，偏好只会白损失一半额度）。
fn image_route_for(model: &str, has_image: bool) -> crate::forward::ImageRoute {
    use crate::upstream::measured::{measured_image_capability, ImageCapability};
    if !has_image {
        return crate::forward::ImageRoute::any();
    }
    let cn = measured_image_capability(model, crate::pool::Region::CN) == ImageCapability::Supported;
    let intl =
        measured_image_capability(model, crate::pool::Region::Intl) == ImageCapability::Supported;
    match (cn, intl) {
        (true, false) => crate::forward::ImageRoute {
            region: crate::pool::Region::CN,
            required: true,
        },
        (false, true) => crate::forward::ImageRoute {
            region: crate::pool::Region::Intl,
            required: true,
        },
        // 两区都能读，或都没有实测结论 → 不约束。
        // 「没实测过」不等于「不行」，把没验过的模型一律拒掉会误伤本可用的图片能力；
        // 两区都能读时做偏好只会白损失一半账号的额度。
        _ => crate::forward::ImageRoute::any(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_state(api_key: &str) -> Arc<AppState> {
        // 缺省 passthrough：既有测试断言的是「请求体原样出站」，注入提示词会让它们
        // 全部失效 —— 需要提示词的用例走 `test_state_with_prompt`。
        test_state_with(api_key, crate::cfgwatch::DynamicConfig::default())
    }

    /// 带指定热重载初值的 `AppState`。
    ///
    /// 模型清单与提示词都从 [`crate::cfgwatch::DynamicState`] 读（热重载的落点），
    /// 故这里只换初值即可。
    fn test_state_with(api_key: &str, dc: crate::cfgwatch::DynamicConfig) -> Arc<AppState> {
        Arc::new(AppState {
            pool: Arc::new(Mutex::new(Pool::new(String::new()))),
            client: crate::upstream::client::Client::new(
                crate::upstream::client::ClientConfig::default(),
            )
            .expect("构造测试客户端"),
            session: None,
            api_key: api_key.into(),
            max_rotate: 3,
            soft_cooldown: std::time::Duration::from_secs(60),
            refresh_skew: std::time::Duration::from_secs(600),
            redis_mode: String::new(),
            allowed_model: Vec::new(),
            usage: Arc::new(crate::usage::Stats::new("")),
            products: crate::products::ProductUpstreams::new(),
            unusual: crate::unusual::UnusualBreakers::new(),
            waf_ip: crate::wafip::WafIpWindow::new(),
            probe_timeout: std::time::Duration::from_secs(30),
            models: Arc::new(crate::models_cache::ModelsCache::new()),
            dynamic: crate::cfgwatch::DynamicState::new(dc),
            auth_dir: String::new(),
            // ★ 未接任务执行体：既有用例断言的是「/tasks/run → 503 tasks_unavailable」，
            // 接了调度器就变成真跑任务（会打上游）。需要真实调度的用例走
            // `test_state_with_scheduler`。
            scheduler: None,
            // ★ 同上：未接成长任务执行体，`/tasks/growth` 走「未注入」分支 503。
            // 需要真实执行的用例走 `test_state_with_growtask`。
            growtask: None,
        })
    }

    /// 带提示词配置的 `AppState`（`tests/ab_prompt.rs` 那批用例的库内对应物）。
    #[allow(dead_code)]
    fn test_state_with_prompt(mode: &str, text: &str) -> Arc<AppState> {
        test_state_with(
            "",
            crate::cfgwatch::DynamicConfig {
                prompt_mode: mode.to_string(),
                prompt_text: text.to_string(),
                ..Default::default()
            },
        )
    }

    /// 接了养号任务执行体的 `AppState`（`/tasks/run` 的真实派发路径）。
    fn test_state_with_scheduler(
        api_key: &str,
        sched: Arc<crate::scheduler::Scheduler>,
    ) -> Arc<AppState> {
        let mut state = test_state_with(api_key, crate::cfgwatch::DynamicConfig::default());
        std::sync::Arc::get_mut(&mut state).expect("测试中独占持有").scheduler = Some(sched);
        state
    }

    /// 接了成长任务执行体的 `AppState`（`/tasks/growth` 的真实派发路径）。
    ///
    /// ★ 不挂记录器（`with_records(None)`）：`Recorder` 未启用时所有写入都是静默
    /// no-op，测试里挂一个假的只会掩盖「记录器缺席」这条路径。而 `lookup` 的
    /// id→uid 翻译在无记录器时**退化成直接把入参当 uid**，正是「独立运行网关、
    /// 没有宿主 identities」的场景 —— 用它测 400 分支最干净。
    fn test_state_with_growtask(api_key: &str) -> Arc<AppState> {
        let mut state = test_state_with(api_key, crate::cfgwatch::DynamicConfig::default());
        let runner = crate::growtask::Runner::new(crate::growtask::Options::new(
            state.client.clone(),
        ));
        std::sync::Arc::get_mut(&mut state).expect("测试中独占持有").growtask = Some(Arc::new(runner));
        state
    }

    async fn body_json(res: Response) -> Value {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    #[tokio::test]
    async fn healthz_is_unauthenticated_and_reports_service() {
        let app = router(test_state("secret"));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // 无账号 → 503（ServableNow=false），但必须带身份标识
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(res.headers().get("X-Service").unwrap(), SERVICE_NAME);
        let v = body_json(res).await;
        assert_eq!(v["service"], SERVICE_NAME);
        assert_eq!(v["total"], 0);
        assert_eq!(v["healthy"], 0);
    }

    #[tokio::test]
    async fn healthz_200_when_account_available() {
        let state = test_state("");
        {
            let mut p = state.pool.lock().unwrap();
            p.add(crate::auth::Auth {
                uid: "u1".into(),
                access_token: "at".into(),
                ..Default::default()
            });
        }
        let app = router(state);
        let res = app
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["healthy"], 1);
        assert_eq!(v["total"], 1);
    }

    #[tokio::test]
    async fn status_requires_auth_when_key_set() {
        // 无 Authorization 头 → 401
        let app = router(test_state("secret"));
        let res = app
            .oneshot(Request::builder().uri("/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let v = body_json(res).await;
        assert_eq!(v["error"]["code"], "invalid_api_key");
        assert_eq!(v["error"]["type"], "api_error");
    }

    #[tokio::test]
    async fn status_accepts_valid_bearer() {
        let app = router(test_state("secret"));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header("Authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert!(v.get("accounts").is_some());
        assert_eq!(v["redis_mode"], "noop"); // 空 → 回落 noop
        assert_eq!(v["sticky_sessions"], 0);
    }

    /// `/status` 的 `accounts[]` 必须下发**产品与区域**两个身份字段。
    ///
    /// # 为什么钉住这两个字段
    ///
    /// 所有者 2026-09-20 原话：「在兼容网关哪里的账号池,也要标记上进入池子的
    /// 账号属于那个客户端」；2026-10-01 又补了「平台 国内国外 区分不明显,
    /// 你改的明显一点」。三个产品的账号混在**同一个池**里，界面此前只有一个
    /// 9.5px 的缩写 chip，区域**完全没显示** —— 用户看到两个同名账号只能靠猜。
    ///
    /// 本用例锁的是**后端契约**（前端据此渲染徽标）：字段名是 `product` /
    /// `region`，值是 `product_of()` / `region()` 的归一化结果，且**恒输出**
    /// （不带 omitempty）—— 否则前端无法区分「键不存在」与「值为空」。
    ///
    /// ⚠ 空池也必须有 `accounts` 键：前端在池为空时仍要渲染表头与空态。
    #[tokio::test]
    async fn status_accounts_carry_product_and_region() {
        let state = test_state("secret");
        // 灌一个 WorkBuddy 国服账号（空 product ⇒ 归一成 workbuddy）。
        if let Ok(mut pool) = state.pool.lock() {
            pool.add(crate::auth::Auth {
                uid: "u-cn".into(),
                nickname: "国服号".into(),
                domain: "www.codebuddy.cn".into(),
                ..Default::default()
            });
            pool.add(crate::auth::Auth {
                uid: "u-intl".into(),
                nickname: "国际号".into(),
                domain: "workbuddy.ai".into(),
                ..Default::default()
            });
        }
        let app = router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header("Authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        let accounts = v["accounts"].as_array().expect("accounts 必须是数组");
        assert_eq!(accounts.len(), 2, "两个账号都应下发");

        // 逐个断言：产品归一化 + 区域按域名判定。
        let mut by_uid: std::collections::HashMap<&str, &Value> = Default::default();
        for a in accounts {
            by_uid.insert(a["uid"].as_str().unwrap_or_default(), a);
        }
        let cn = by_uid["u-cn"];
        assert_eq!(cn["product"], "workbuddy", "空 product 必须归一成 workbuddy");
        assert_eq!(cn["region"], "cn", "codebuddy.cn 是国服");
        let intl = by_uid["u-intl"];
        assert_eq!(intl["product"], "workbuddy");
        assert_eq!(intl["region"], "intl", "workbuddy.ai 是国际版");

        // 恒输出：即使值为空串（此处不可能），键也必须在 —— 前端据此区分
        // 「老网关不下发」与「值就是空」。用 contains_key 而非 get().is_some()
        // 的等价写法即可，但显式断言更直白。
        assert!(cn.as_object().unwrap().contains_key("product"));
        assert!(cn.as_object().unwrap().contains_key("region"));
    }

    #[tokio::test]
    async fn status_rejects_wrong_key() {
        let app = router(test_state("secret"));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header("Authorization", "Bearer wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn chat_requires_auth() {
        let app = router(test_state("secret"));
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// `/v1/models` **永不失败**：空池也必须 200 + `object`/`data` 两个键。
    ///
    /// 这条契约替换了此前「拿不到上游清单就 503」的版本。改动理由（Go
    /// `handler.go:826-834` 同款）：并集语义下**总有东西可下发** —— 静态兜底表至少
    /// 能给出 `id` 与元数据。而 503 的代价是客户端连另一区可用的模型都看不到，
    /// 界面文案只会说「无法获取模型清单」，用户不知道该去补账号还是查网络。
    #[tokio::test]
    async fn models_never_fails_and_has_exactly_two_keys() {
        // 池为空 + 上游不可达（测试环境无网络）⇒ 仍必须 200。
        let app = router(test_state(""));
        let res = app
            .oneshot(Request::builder().uri("/v1/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["object"], "list");
        assert!(v["data"].is_array(), "data 必须是数组: {v}");

        // ★ 只有两个键 —— Go 没有 `source`，而它的值在并集语义下也已不成立
        //（清单是「动态真值 + 静态兜底」的混合，不是纯上游）。
        let obj = v.as_object().unwrap();
        let keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
        assert_eq!(keys.len(), 2, "只应有 object + data，实际: {keys:?}");
        assert!(obj.contains_key("object") && obj.contains_key("data"));
        assert!(v.get("source").is_none(), "不得再下发 source 键");
        assert!(v.get("error").is_none(), "不得返回错误体");
    }

    /// 空池时仍应有静态兜底条目，且国服模型在前（`merged_model_list` 的顺序契约）。
    #[tokio::test]
    async fn models_falls_back_to_static_when_no_accounts() {
        let app = router(test_state(""));
        let res = app
            .oneshot(Request::builder().uri("/v1/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let v = body_json(res).await;
        let data = v["data"].as_array().unwrap();
        assert!(!data.is_empty(), "空池也必须给出静态兜底清单");
        // 静态表里的国服专属模型必须出现（`hy3-x` 只在 CN 静态表里）
        let ids: Vec<&str> = data.iter().filter_map(|m| m["id"].as_str()).collect();
        assert!(ids.contains(&"hy3-x"), "应含国服静态模型: {ids:?}");
        // 每条都必须有 object/created/owned_by 三个兜底元数据。
        // ★ `owned_by` 三态：国服静态表 `workbuddy`、国际版静态表 `workbuddy-intl`
        //（Go `infosFromStatic` 逐字）、组合名/别名条目 `gateway-route`
        //（带 `route_for`，客户端据此知道这是网关的别名而非上游原生模型）。
        for m in data {
            assert_eq!(m["object"], "model", "{m}");
            assert!(m["created"].is_i64(), "{m}");
            let owner = m["owned_by"].as_str().unwrap_or("");
            assert!(
                owner == "workbuddy" || owner == "workbuddy-intl" || owner == "gateway-route",
                "owned_by 超出三态: {m}"
            );
            if owner == "gateway-route" {
                assert!(m["route_for"].is_string(), "别名条目必须带 route_for: {m}");
            }
        }
        // 国服静态模型必须标国服
        let cn_entry = data.iter().find(|m| m["id"] == "hy3-x").unwrap();
        assert_eq!(cn_entry["owned_by"], "workbuddy");
    }

    /// `?refresh` 必须真的清缓存 —— 否则就是 Go 记录过的那次缺陷：
    /// 「文案让他做的事，代码不支持」（`handler.go:801-825`）。
    #[tokio::test]
    async fn models_refresh_clears_cache() {
        let state = test_state("");
        // 先塞一份假真值，模拟「上次拉到了」。
        state.models.record_success(
            Region::CN,
            vec![crate::upstream::ModelInfo {
                id: "refresh-probe-model".into(),
                ..Default::default()
            }],
            std::time::SystemTime::now(),
        );
        assert!(state.models.has_truth(Region::CN));

        // 不带 refresh：缓存仍在。
        let app = router(state.clone());
        let _ = app
            .oneshot(Request::builder().uri("/v1/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        // ★ 注意：`region_snapshot` 会拉取，空池 ⇒ 无账号 ⇒ 记一次失败，但
        // `record_failure` **不动**已有清单，故真值仍在。
        assert!(state.models.has_truth(Region::CN));

        // 带 `?refresh=true`：缓存必须先被清空。
        let app = router(state.clone());
        let _ = app
            .oneshot(
                Request::builder()
                    .uri("/v1/models?refresh=true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // 清空后 `region_snapshot` 又拉了一次（仍失败），故真值不再存在。
        assert!(!state.models.has_truth(Region::CN), "refresh 必须清掉缓存");
    }

    /// `?refresh` 的取值口径必须与 Go `wantsModelRefresh` 一致。
    #[test]
    fn wants_model_refresh_accepts_three_spellings() {
        for q in ["refresh=1", "refresh=true", "refresh=YES", "refresh=%20True%20"] {
            assert!(wants_model_refresh(Some(q)), "{q}");
        }
        for q in ["refresh=0", "refresh=false", "refresh=", "days=7", ""] {
            assert!(!wants_model_refresh(Some(q)), "{q}");
        }
        assert!(!wants_model_refresh(None));
    }

    /// 中文任务名必须能解出来（客户端会百分号编码）。
    #[test]
    fn query_param_decodes_percent_encoding() {
        assert_eq!(query_param(Some("task=sign_in"), "task"), "sign_in");
        assert_eq!(query_param(Some("a=1&task=b&c=2"), "task"), "b");
        // `夜猫子` 的 UTF-8 百分号编码
        assert_eq!(
            query_param(Some("task=%E5%A4%9C%E7%8C%AB%E5%AD%90"), "task"),
            "夜猫子"
        );
        // `+` 是空格（表单编码口径，与 Go `url.Values` 一致）
        assert_eq!(query_param(Some("task=a+b"), "task"), "a b");
        assert_eq!(query_param(Some("task=a%2Bb"), "task"), "a+b");
        assert_eq!(query_param(None, "task"), "");
        assert_eq!(query_param(Some("x=1"), "task"), "");
    }

    /// `/v1/models/regions` 的形状是**宿主硬依赖**（`model_billing.rs` 读它判倍率）。
    #[tokio::test]
    async fn models_regions_shape_matches_host_contract() {
        let app = router(test_state(""));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/v1/models/regions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;

        let regions = v["regions"].as_array().expect("regions 必须是数组");
        assert_eq!(regions.len(), 2, "必须恰好两区");
        assert_eq!(regions[0]["region"], "cn", "顺序固定：国服在前");
        assert_eq!(regions[1]["region"], "intl");
        for r in regions {
            // ★ 宿主 `region_from_key` 只认 "cn"/"intl"，其它一律**跳过**
            let name = r["region"].as_str().unwrap();
            assert!(name == "cn" || name == "intl", "区域名必须精确: {name}");
            // 三态：dynamic / static_fallback / unknown
            let ts = r["truth_source"].as_str().unwrap();
            assert!(
                ts == "dynamic" || ts == "static_fallback" || ts == "unknown",
                "truth_source 三态: {ts}"
            );
            assert!(r["models"].is_array());
            assert!(r["model_count"].is_u64());
            // ★ 键必须存在（值可以是 null）—— 宿主与界面都按存在性读
            assert!(r.as_object().unwrap().contains_key("fetched_at"));
            assert!(r.as_object().unwrap().contains_key("last_error"));
            assert!(r.as_object().unwrap().contains_key("last_error_message"));
        }
        // 空池 + 无网络 ⇒ 国服走静态兜底、国际版 unknown
        assert_eq!(regions[0]["truth_source"], "static_fallback");
        assert_eq!(regions[1]["truth_source"], "unknown");
        assert_eq!(v["notes"].as_array().unwrap().len(), 3, "notes 三条");
    }

    /// ★ 倍率必须是 **JSON 数字或 null**：宿主 `billing_of` 只认数字，
    /// 字符串会被判成 Unknown（= 界面显示「未声明」而不是那个真实倍率）。
    #[tokio::test]
    async fn region_view_multiplier_is_number_or_null() {
        let state = test_state("");
        state.models.record_success(
            Region::CN,
            vec![
                crate::upstream::ModelInfo {
                    id: "free-model".into(),
                    credit_multiplier: Some(0.0),
                    credits_raw: "x0.00".into(),
                    ..Default::default()
                },
                crate::upstream::ModelInfo {
                    id: "paid-model".into(),
                    credit_multiplier: Some(0.03),
                    credits_raw: "x0.03".into(),
                    ..Default::default()
                },
                crate::upstream::ModelInfo {
                    id: "undeclared-model".into(),
                    credit_multiplier: None,
                    credits_raw: String::new(),
                    ..Default::default()
                },
            ],
            std::time::SystemTime::now(),
        );
        let fetched = state.models.snapshot();
        let view = region_view(&state, Region::CN, &fetched);
        assert_eq!(view["truth_source"], "dynamic");
        let models = view["models"].as_array().unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0]["credit_multiplier"], 0.0, "0 = 免费（数字）");
        assert_eq!(models[1]["credit_multiplier"], 0.03);
        assert_eq!(models[2]["credit_multiplier"], Value::Null, "null = 未声明");
        assert_eq!(models[2]["credits_raw"], "");
        assert_eq!(view["model_count"], 3);
    }

    /// 上游宣称与实测不一致时必须**两个都透出**（这是那次缺陷的可诊断性来源）。
    #[tokio::test]
    async fn region_view_exposes_measured_override() {
        let state = test_state("");
        // 国际版 glm-5.3：上游说 supportsImages=true，实测 false。
        state.models.record_success(
            Region::Intl,
            vec![crate::upstream::ModelInfo {
                id: "glm-5.3".into(),
                supports_images: Some(true),
                ..Default::default()
            }],
            std::time::SystemTime::now(),
        );
        let fetched = state.models.snapshot();
        let view = region_view(&state, Region::Intl, &fetched);
        let m = &view["models"][0];
        assert_eq!(m["id"], "glm-5.3");
        assert_eq!(m["supportsImages"], false, "实测值覆盖上游声明");
        assert_eq!(m["upstream_claims_supports_images"], true);
        assert_eq!(m["overridden_by"], "measured");
    }

    /// `/debug/` 与 `/debug/capability` 都必须鉴权，且形状固定。
    #[tokio::test]
    async fn debug_routes_require_auth_and_have_fixed_shape() {
        // 无 key → 401
        for uri in ["/debug/", "/debug/capability"] {
            let app = router(test_state("secret"));
            let res = app
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }
        // 带 key → 200
        let app = router(test_state("secret"));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/debug/")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_json(res).await["areas"][0], "capability");

        let app = router(test_state(""));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/debug/capability")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v = body_json(res).await;
        for k in [
            "dynamic_cn_ids",
            "dynamic_intl_ids",
            "static_cn_ids",
            "static_intl_ids",
            "cn_truth_known",
            "intl_truth_known",
            "accounts",
        ] {
            assert!(v.as_object().unwrap().contains_key(k), "缺键 {k}: {v}");
        }
        // ★ 空池必须是 `[]` 而不是 null
        assert_eq!(v["accounts"], json!([]));
        // 静态表必须列出来（供人工核对「静态 vs 真值」的差异）
        assert!(!v["static_cn_ids"].as_array().unwrap().is_empty());
        assert!(!v["static_intl_ids"].as_array().unwrap().is_empty());
    }

    /// `revive` 三态：uid 为空 → 400；不在池中 → 404；在池中 → 200 + 幂等语义。
    #[tokio::test]
    async fn revive_account_handles_three_states() {
        let state = test_state("");
        // 400：空 uid（路径 `/debug/accounts//revive` 不会命中路由，故直接测 handler）
        let res = debug_revive_account(
            State(state.clone()),
            Path(String::new()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        // 404：不在池中
        let res = debug_revive_account(
            State(state.clone()),
            Path("no-such-uid".to_string()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let v = body_json(res).await;
        assert_eq!(v["uid"], "no-such-uid");
        // Rust 侧也有目录热加载（`authwatch`），hint 必须与 Go 一样指向「等一轮扫描」，
        // 而不是让用户去等一件代码不支持的事（旧文案是「请重启网关」）。
        let hint = v["hint"].as_str().unwrap();
        assert!(hint.contains("热加载"), "hint 应说明池与目录是对齐的：{hint}");
        assert!(!hint.contains("重启"), "有了热加载就不该再让用户重启：{hint}");

        // 200：在池中（先禁用，再复活）
        {
            let mut pool = state.pool.lock().unwrap();
            pool.add(crate::auth::Auth {
                uid: "u1".into(),
                access_token: "at".into(),
                domain: "www.workbuddy.cn".into(),
                ..Default::default()
            });
            pool.disable("u1", "12153 session dead");
        }
        let res = debug_revive_account(
            State(state.clone()),
            Path("u1".to_string()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["revived"], true);
        assert_eq!(v["disabled"], false);
        // ★ 复活后 reason 已被清空 ⇒ `disabled_reason_before` 是唯一线索
        assert_eq!(v["disabled_reason_before"], "12153 session dead");

        // 幂等：再来一次 → 200 + revived:false + 说明
        let res = debug_revive_account(
            State(state.clone()),
            Path("u1".to_string()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["revived"], false);
        let notes = v["notes"].as_array().expect("幂等时必须给出说明");
        assert!(notes[0].as_str().unwrap().contains("幂等"), "{notes:?}");
    }

    /// ★ `revive` 只清系统禁用位：`no_route` 与冷却都必须原样保留并给出说明。
    #[tokio::test]
    async fn revive_does_not_touch_no_route_or_cooldown() {
        let state = test_state("");
        {
            let mut pool = state.pool.lock().unwrap();
            pool.add(crate::auth::Auth {
                uid: "u2".into(),
                access_token: "at".into(),
                domain: "www.workbuddy.ai".into(),
                no_route: true,
                ..Default::default()
            });
            pool.disable("u2", "session dead");
            pool.cooldown("u2", crate::pool::CoolKind::Soft, std::time::Duration::from_secs(600), "429");
        }
        let res = debug_revive_account(
            State(state.clone()),
            Path("u2".to_string()),
            HeaderMap::new(),
        )
        .await;
        let v = body_json(res).await;
        assert_eq!(v["revived"], true);
        assert_eq!(v["no_route"], true, "no_route 是用户的开关，复活不能动");
        assert_eq!(v["cooling"], true, "复活不清冷却");
        // ★ 三条 notes 可以**同时**出现（Go `admin.go:196-211` 是三个独立 if）
        let notes = v["notes"].as_array().unwrap();
        assert_eq!(notes.len(), 2, "no_route + cooling 各一条: {notes:?}");
    }

    /// `/tasks/run`：任务名为空 → 400；**未接执行体**时给了名字 → 503 且带可读 message。
    ///
    /// ★ 顺序不能反：把 400 放到 503 后面，用户把任务名打错时看到的是「任务运行器
    /// 未配置」，会去查一个不存在的问题。
    ///
    /// ★ 503 分支只在 `AppState::scheduler == None` 时可达 —— 生产路径恒注入，
    /// 故这是「单测/降级构建」的语义；接了执行体的行为见
    /// [`tasks_run_dispatches_to_the_scheduler`]。
    #[tokio::test]
    async fn tasks_run_validates_before_reporting_unavailable() {
        // 空 body + 无查询串 → 400 missing task name
        let app = router(test_state(""));
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tasks/run")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(res).await["error"]["message"], "missing task name");

        // 查询串给名字 → 503
        let app = router(test_state(""));
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tasks/run?task=sign_in")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        let v = body_json(res).await;
        assert_eq!(v["error"]["code"], "tasks_unavailable");
        // 宿主会把 message 原样透出到界面 ⇒ 必须可读，不能是裸状态码
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(msg.contains("task runner"), "必须给出可读原因: {msg}");

        // body 里的 task 名（查询串为空时才读）
        let app = router(test_state(""));
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tasks/run")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"task":"travel","accountId":"u1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// 接了执行体后，`/tasks/run` 真的派发给调度器。
    ///
    /// 断言的是**接线本身**（不是任务语义 —— 那些在 `scheduler::tests` 里）：
    /// 未知任务名必须变成 400 + 错误原文，而不是静默 200。Go
    /// `scheduler.go:288` 的 doc 逐字：「未知任务名返回错误而非静默成功：
    /// 宿主拼错名字时必须能看见。」
    #[tokio::test]
    async fn tasks_run_dispatches_to_the_scheduler() {
        let sched = std::sync::Arc::new(crate::scheduler::Scheduler::new(
            crate::scheduler::Config::new(
                crate::upstream::client::Client::new(
                    crate::upstream::client::ClientConfig::default(),
                )
                .expect("构造测试客户端"),
            ),
        ));
        let state = test_state_with_scheduler("", sched);

        // 未知任务名 → 400 + 原文（不是 503，也不是静默 200）
        let app = router(state.clone());
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tasks/run?task=bogus")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let v = body_json(res).await;
        assert_eq!(v["error"]["code"], "invalid_request");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("unknown task"),
            "必须透出未知任务名: {v}"
        );

        // 已知任务名 + 空池 → 200 且 ran=true（没有账号可跑不是错误）
        let app = router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tasks/run?task=activity")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["task"], "activity");
        assert_eq!(v["ran"], true);
    }

    /// `/tasks/growth` 在**未接线**时无条件 503（Go 的 nil 检查在读 body **之前**）。
    ///
    /// ★ 断言的是「顺序」而不只是「503」：body 里的 action 非法也仍然先给 503，
    /// 因为 Go 侧 nil 检查在读 body 之前，压根不会去看 action。
    #[tokio::test]
    async fn tasks_growth_is_unconditionally_unavailable() {
        let app = router(test_state(""));
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tasks/growth")
                    .header("content-type", "application/json")
                    // 即使 body 里的 action 非法，也必须先给 503（顺序与 Go 一致）
                    .body(Body::from(r#"{"action":"bogus"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body_json(res).await["error"]["code"],
            "growth_tasks_unavailable"
        );
    }

    /// `/tasks/growth` 的三条**请求侧** 400 分支（Go `handler.go:428-462`）。
    ///
    /// ★ 这三条都发生在**打上游之前**：非法 action、缺 accountId、body 不是 JSON。
    /// 断言它们意味着「参数错不会变成一次真实的上游调用」—— 否则用户手滑打错一个
    /// 字，网关就去跑了。
    #[tokio::test]
    async fn tasks_growth_validates_before_calling_upstream() {
        // 鉴权优先于一切（含未接线）
        let app = router(test_state_with_growtask("secret"));
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tasks/growth")
                    .body(Body::from(r#"{"action":"list","accountId":"u1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // body 不是 JSON → 400 invalid JSON body
        let app = router(test_state_with_growtask(""));
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tasks/growth")
                    .header("content-type", "application/json")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let v = body_json(res).await;
        assert_eq!(v["error"]["code"], "invalid_request");
        assert_eq!(v["error"]["message"], "invalid JSON body");

        // 空 body 也是「解析失败」而不是另一条分支：Go 的 growthTasks 直接
        // `io.ReadAll` 且**忽略**读错误，空 body 落到 jsonUnmarshal 报错。
        let app = router(test_state_with_growtask(""));
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tasks/growth")
                    .header("content-type", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(res).await["error"]["message"], "invalid JSON body");

        // 非法 action → 400，且文案逐字（宿主侧 `growth_task` 会原样透出）
        let app = router(test_state_with_growtask(""));
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tasks/growth")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"action":"bogus"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let v = body_json(res).await;
        assert_eq!(v["error"]["code"], "invalid_request");
        assert_eq!(
            v["error"]["message"],
            r#"unknown action (want "list" | "run" | "run-all")"#
        );

        // list / run 缺 accountId → 400（`run-all` **不需要** accountId）
        for action in ["list", "run"] {
            let app = router(test_state_with_growtask(""));
            let res = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/tasks/growth")
                        .header("content-type", "application/json")
                        .body(Body::from(format!(r#"{{"action":"{action}"}}"#)))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{action}");
            let v = body_json(res).await;
            assert_eq!(v["error"]["code"], "invalid_request", "{action}");
            assert_eq!(v["error"]["message"], "missing accountId", "{action}");
        }

        // 纯空白 accountId 等同缺失（Go 用 `strings.TrimSpace(...) == ""` 判）
        let app = router(test_state_with_growtask(""));
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tasks/growth")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"action":"list","accountId":"   "}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(res).await["error"]["message"], "missing accountId");
    }

    /// `run-all` 在空池上给 400 `growth_task_failed` + 逐字文案（Go `runAll`）。
    ///
    /// ★ 空池是**用户可自行解决**的状态（还没同步账号），文案必须说清怎么办，
    /// 而不是一句「执行失败」。
    #[tokio::test]
    async fn tasks_growth_run_all_rejects_empty_pool() {
        let app = router(test_state_with_growtask(""));
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tasks/growth")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"action":"run-all"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let v = body_json(res).await;
        assert_eq!(v["error"]["code"], "growth_task_failed");
        assert_eq!(v["error"]["message"], "账号池为空，请先启动网关或同步账号");
    }

    /// 账号不在池中 → 400 `growth_task_failed` + 逐字文案（Go `lookup`）。
    ///
    /// ★ 这条覆盖的是「id→uid 翻译后仍找不到」：宿主界面按**账号库 id** 发起
    /// 请求，池按 **uid** 索引，两者不同。翻译失败时若不给出这条提示，用户看到
    /// 的会是「找不到账号」，而账号明明就在列表里 —— 无从排查。
    #[tokio::test]
    async fn tasks_growth_reports_account_not_in_pool() {
        let app = router(test_state_with_growtask(""));
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tasks/growth")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"action":"list","accountId":"nope"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let v = body_json(res).await;
        assert_eq!(v["error"]["code"], "growth_task_failed");
        assert_eq!(
            v["error"]["message"],
            "账号不在网关池中（id=nope）：可能是已禁用、需重新登录，或尚未同步到网关"
        );
    }

    /// `run-all` 只收 **WorkBuddy 产品**的账号（Go `poolAuths`）。
    ///
    /// ★ 这条是 2026-09-22 修的回归：按 `IsIntl()` 过滤时 Qoder 的
    /// `qoder.com.cn`（国服）**会**参与，拿 Qoder 令牌打 WorkBuddy 端点 ——
    /// 一直是错的。
    ///
    /// ★ 同时放一个 Qoder 与一个 WorkBuddy 账号：只断言「Qoder 被排除」的话，
    /// 「过滤把所有账号都排除掉」也能通过。必须**两边都断言**才证明过滤器是按
    /// 产品分的。
    #[tokio::test]
    async fn tasks_growth_run_all_only_takes_workbuddy_accounts() {
        let state = test_state_with_growtask("");
        {
            let mut p = state.pool.lock().unwrap();
            // 两个账号都**无令牌** ⇒ `run_account` 在第一步就判 skipped，
            // 不会打上游。本用例只关心「谁进了列表」。
            p.add(crate::auth::Auth {
                uid: "qoder1".into(),
                access_token: String::new(),
                domain: "qoder.com.cn".into(),
                // ★ 必须显式写产品：`product_of()` 把**空串**归一成 workbuddy，
                // 只填 domain 的话这个假账号会被当成 WorkBuddy 账号而通过过滤
                // —— 那样测的就不是过滤逻辑了。
                product: crate::auth::PRODUCT_QODER.into(),
                ..Default::default()
            });
            p.add(crate::auth::Auth {
                uid: "wb1".into(),
                access_token: String::new(),
                domain: "workbuddy.cn".into(),
                ..Default::default()
            });
        }
        let app = router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tasks/growth")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"action":"run-all"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        let results = v["results"].as_array().expect("results 必须是数组");
        assert_eq!(results.len(), 1, "只应有 WorkBuddy 账号参与: {v}");
        assert_eq!(results[0]["uid"], "wb1");
        // 无令牌 ⇒ skipped 并带上逐字原因（Go `runAccount`）
        assert_eq!(results[0]["skipped"], true);
        assert_eq!(results[0]["skip_reason"], "账号无访问令牌（需重新登录）");
        assert_eq!(results[0]["realm"], "cn");
        // ★ 跳过的账号**不计入** `accounts`（Go `Summarize` 逐字）
        assert_eq!(v["summary"]["accounts"], 0);
        assert_eq!(v["summary"]["skipped"], 1);
        assert_eq!(v["summary"]["failed"], 0);
    }

    /// 短信两条：必须鉴权；空手机号/空验证码 → 400（**不**发请求）。
    #[tokio::test]
    async fn sms_routes_require_auth_and_validate_input() {
        for uri in ["/login/sms/send", "/login/sms/verify"] {
            let app = router(test_state("secret"));
            let res = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }

        // 空手机号 → 400（且**不会**真的去发短信）
        let app = router(test_state(""));
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login/sms/send")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"phone":"  "}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(body_json(res).await["error"]["message"]
            .as_str()
            .unwrap()
            .contains("手机号"));

        // 空验证码 → 400
        let app = router(test_state(""));
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login/sms/verify")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"phone":"13800138000","smsCode":""}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        // 非 JSON body → 400
        let app = router(test_state(""));
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login/sms/send")
                    .header("content-type", "application/json")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// 区域基址与归一化必须与 Go `smsBaseFor` / `normalizeSMSRegion` 逐字一致。
    #[test]
    fn sms_base_and_region_normalization_match_go() {
        assert_eq!(sms_base_for("intl"), "https://www.workbuddy.ai");
        assert_eq!(sms_base_for("ai"), "https://www.workbuddy.ai");
        assert_eq!(sms_base_for("International"), "https://www.workbuddy.ai");
        assert_eq!(sms_base_for("cn"), "https://www.workbuddy.cn");
        // ★ 空值默认国服（不报错）
        assert_eq!(sms_base_for(""), "https://www.workbuddy.cn");
        assert_eq!(sms_base_for("  国际 "), "https://www.workbuddy.cn");

        assert_eq!(normalize_sms_region("intl"), "intl");
        assert_eq!(normalize_sms_region(" AI "), "intl");
        assert_eq!(normalize_sms_region(""), "cn");
        assert_eq!(normalize_sms_region("whatever"), "cn");
    }

    /// ★ JWT 解析：按序试四个声明名；base64url 无 padding 也要能解。
    #[test]
    fn uid_from_access_token_tries_four_claims() {
        // `encode`/`decode` 是 `base64::Engine` 的 trait 方法，必须显式引入。
        use base64::Engine as _;
        // 造一个不带 padding 的 base64url JWT（与上游实测形态一致）
        let mk = |claims: &str| {
            let payload =
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.as_bytes());
            format!("header.{payload}.sig")
        };
        assert_eq!(
            uid_from_access_token(&mk(r#"{"uid":"abc-123"}"#)).unwrap(),
            "abc-123"
        );
        assert_eq!(
            uid_from_access_token(&mk(r#"{"userId":"from-userid"}"#)).unwrap(),
            "from-userid"
        );
        assert_eq!(
            uid_from_access_token(&mk(r#"{"user_id":"from-user-id"}"#)).unwrap(),
            "from-user-id"
        );
        assert_eq!(
            uid_from_access_token(&mk(r#"{"sub":"from-sub"}"#)).unwrap(),
            "from-sub"
        );
        // 优先级：uid 先于 userId
        assert_eq!(
            uid_from_access_token(&mk(r#"{"userId":"b","uid":"a"}"#)).unwrap(),
            "a"
        );
        // 空串与纯空白都跳过，继续看下一个声明
        assert_eq!(
            uid_from_access_token(&mk(r#"{"uid":"  ","sub":"fallback"}"#)).unwrap(),
            "fallback"
        );
        // 带 padding 的形态也要能解（标准 base64url）
        let padded = {
            let p = base64::engine::general_purpose::URL_SAFE
                .encode(br#"{"uid":"padded-uid"}"#);
            format!("header.{p}.sig")
        };
        assert_eq!(uid_from_access_token(&padded).unwrap(), "padded-uid");

        // 失败：不是 JWT / 没有可用的声明
        assert!(uid_from_access_token("not-a-jwt").is_err());
        assert!(uid_from_access_token("h.@@@invalid@@@.s").is_err());
        assert!(uid_from_access_token(&mk(r#"{"other":"x"}"#)).is_err());
    }

    /// ★ 路径穿越防护：uid 里带 `/` 或 `..` 不能逃出凭证目录。
    #[test]
    fn sanitize_file_part_blocks_path_traversal() {
        assert_eq!(sanitize_file_part("abc-123_X"), "abc-123_X");
        // ★ `.` 与 `/` 都在白名单外 ⇒ 一并替换（比"只挡斜杠"更严：`..` 段也失去语义）
        assert_eq!(sanitize_file_part("../../etc/passwd"), "______etc_passwd");
        assert_eq!(sanitize_file_part("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_file_part("a b"), "a_b");
        // 全非法字符也不能产出空文件名
        assert_eq!(sanitize_file_part("///"), "___");
        assert_eq!(sanitize_file_part(""), "unknown");
    }

    /// 非 WorkBuddy 产品**不写倍率**：宁可不说，也不给一个可能错的数字。
    #[tokio::test]
    async fn record_usage_only_sets_multiplier_for_workbuddy() {
        let state = test_state("");
        // 先把真值塞进缓存（国服 deepseek-v4.1-flash = x0.03）
        state.models.record_success(
            Region::CN,
            vec![crate::upstream::ModelInfo {
                id: "deepseek-v4.1-flash".into(),
                credit_multiplier: Some(0.03),
                ..Default::default()
            }],
            std::time::SystemTime::now(),
        );

        // WorkBuddy：倍率来自缓存
        assert_eq!(
            billing_multiplier_for(&state, "cn", "deepseek-v4.1-flash"),
            Some(0.03)
        );
        // ★ 区域为空（不分区）⇒ None，**不回落**到国服
        assert_eq!(
            billing_multiplier_for(&state, "", "deepseek-v4.1-flash"),
            None
        );
        // 该区域没有这个模型 ⇒ None（绝不编造）
        assert_eq!(
            billing_multiplier_for(&state, "intl", "deepseek-v4.1-flash"),
            None
        );
        // 缓存里根本没有的模型 ⇒ None
        assert_eq!(billing_multiplier_for(&state, "cn", "no-such-model"), None);
    }

    /// `x-api-key` 必须与 `Authorization: Bearer` 等价 —— Anthropic SDK 只发前者。
    #[tokio::test]
    async fn x_api_key_header_is_accepted() {
        // `x-api-key` 正确 → 放行
        let app = router(test_state("secret"));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header("x-api-key", "secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // 错的 → 401
        let app = router(test_state("secret"));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header("x-api-key", "wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // 两个都给且矛盾时：`Authorization` 优先（与 Go 同序）
        let app = router(test_state("secret"));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header("authorization", "Bearer wrong")
                    .header("x-api-key", "secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "Bearer 优先且不回落");

        // `Authorization` 不是 Bearer 形态时，回落到 x-api-key
        let app = router(test_state("secret"));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header("authorization", "Basic abc")
                    .header("x-api-key", "secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// 七条新路由都必须鉴权（缺一条就是一个未授权的开关）。
    #[tokio::test]
    async fn group_f_routes_require_auth() {
        let routes: [(&str, &str); 7] = [
            ("GET", "/v1/models/regions"),
            ("GET", "/debug/"),
            ("GET", "/debug/capability"),
            ("POST", "/debug/accounts/u1/revive"),
            ("POST", "/tasks/run"),
            ("POST", "/tasks/growth"),
            ("POST", "/login/sms/send"),
        ];
        for (method, uri) in routes {
            let app = router(test_state("secret"));
            let res = app
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{method} {uri}");
        }
    }

    /// 鉴权仍先于模型拉取：未带 key 时必须 401，而不是 503。
    #[tokio::test]
    async fn models_requires_auth_before_upstream() {
        let app = router(test_state("secret"));
        let res = app
            .oneshot(Request::builder().uri("/v1/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn no_auth_when_key_empty() {
        let app = router(test_state(""));
        let res = app
            .oneshot(Request::builder().uri("/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// 两个新协议入口都必须要求鉴权（与 chat 一致）。
    #[tokio::test]
    async fn new_protocol_routes_require_auth() {
        for uri in ["/v1/messages", "/messages", "/v1/responses", "/responses"] {
            let app = router(test_state("secret"));
            let res = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }
    }

    /// 无版本号别名必须与带版本号的路径行为一致（部分客户端省略 `/v1`）。
    #[tokio::test]
    async fn versionless_aliases_are_routed() {
        for uri in ["/messages", "/responses"] {
            let app = router(test_state(""));
            let res = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            // 不是 404 即说明路由存在（空账号池 → 503）
            assert_ne!(res.status(), StatusCode::NOT_FOUND, "{uri} 未注册路由");
        }
    }

    /// Anthropic 入口的错误体必须是 Anthropic 形状（`{"type":"error",...}`）。
    #[tokio::test]
    async fn messages_errors_use_anthropic_shape() {
        let app = router(test_state(""));
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("content-type", "application/json")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let v = body_json(res).await;
        assert_eq!(v["type"], json!("error"));
        assert_eq!(v["error"]["type"], json!("invalid_request_error"));
    }

    /// Responses 入口的错误体必须是 Responses 形状（`{"error":{...}}`）。
    #[tokio::test]
    async fn responses_errors_use_responses_shape() {
        let app = router(test_state(""));
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let v = body_json(res).await;
        assert_eq!(v["error"]["type"], json!("api_error"));
        assert_eq!(v["error"]["code"], json!("invalid_request"));
    }

    /// 空请求体按各协议词汇表报错。
    #[tokio::test]
    async fn empty_body_rejected_per_protocol() {
        let app = router(test_state(""));
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let v = body_json(res).await;
        // Anthropic 词汇表
        assert_eq!(v["error"]["type"], json!("invalid_request_error"));
    }

    // ★ 这里曾有 4 条思考档位测试（`reasoning_fields_expose_multiple_spellings` /
    // `reasoning_fields_omitted_when_unmeasured` / `default_effort_not_fabricated` /
    // `default_effort_is_trimmed`），2026-10-01 随同上面那两个死函数一起删除。
    //
    // 它们测的是 `server.rs` 里那份**陈旧副本**，其中
    // `reasoning_fields_omitted_when_unmeasured` 断言的
    // `model_reasoning_fields(&[], "medium").is_none()`（「无档位不下发」）
    // 与 `capability.rs` 的**正确**行为直接矛盾 —— 有默认档时必须下发标准阶梯
    // 并标注 `range_undeclared`，否则就是所有者 2026-09-18 报的那个缺陷。
    //
    // 等价且正确的覆盖已全部在 `capability.rs` 的测试模块里：
    // `reasoning_case_a_lists_declared_range_verbatim` /
    // `reasoning_case_b_uses_standard_ladder_and_marks_undeclared` /
    // `reasoning_case_c_emits_nothing` /
    // `reasoning_is_orthogonal_to_image_capability` /
    // `default_effort_is_trimmed` / `effort_list_is_copied_not_aliased` /
    // `default_effort_is_not_fabricated`（唯一新增，补上原副本覆盖的那一情形）。

    // ── 失败码面值：与 Go `handler.go` 逐字对齐 ──────────────────────────────
    //
    // 这一组测试的存在理由：这些码**曾经**被写成「能合并就合并」的形状
    //（EffortRejected/ModelNotInRegion/ModelParamInvalid 全落
    // invalid_request_error，UnusualActivity/EgressIPBlocked 全落
    // no_healthy_account），而 Go 侧对每一条都写了「为什么必须是独立码」。
    // 合并的代价是真实的：用户拿到 no_healthy_account 会去查账号池，
    // 而账号一个都没问题；拿到 model_not_in_product 会反复去白名单里加一个
    // 上游根本不存在的模型。
    //
    // 故这里**逐个码面值**钉死，而不是「测一下函数不 panic」。

    /// Chat 侧的专属码逐条对照 Go `handler.go`。
    #[test]
    fn chat_failure_codes_match_go_verbatim() {
        use crate::forward::FailureKind as F;
        // (FailureKind, 期望码, Go 行号)
        let cases: &[(F, &str, &str)] = &[
            (F::ContextTooLong, "context_length_exceeded", "handler.go:1078"),
            (F::RequestRejected, "invalid_request_error", "handler.go:1146"),
            (F::ModelNotAllowed, "model_not_allowed", "handler.go:1304"),
            (F::EffortRejected, "reasoning_effort_rejected", "handler.go:1085"),
            (F::EgressIPBlocked, "egress_ip_blocked", "handler.go:1090"),
            (F::ModelNotInRegion, "model_not_in_region", "handler.go:1096"),
            (F::ModelNotInProduct, "model_not_in_product", "handler.go:1101"),
            (F::ModelNotInUpstream, "model_not_in_upstream", "handler.go:1109"),
            (F::ModelParamInvalid, "model_param_invalid", "handler.go:1117"),
            (F::QuotaExhausted, "quota_exhausted", "handler.go:1120"),
            (F::UnusualActivity, "unusual_activity", "handler.go:1131"),
            (F::CaptchaRequired, "captcha_required", "handler.go:1142"),
            (F::ImageRegionUnavailable, "image_region_unavailable", "handler.go:1317"),
            // 本变体是 Rust 侧新增（Go 没有平台级总开关）⇒ Go 行号填本仓库的
            // 出处，不编造 handler.go 的行号。
            (F::PlatformNotInRouting, "platform_not_in_routing", "forward.rs:639"),
            // 同样是 Rust 侧新增（Go 侧 1113 与「余额不足」共用同一套通用分类，
            // 这正是缺陷本身）⇒ Go 行号栏填本仓库出处。
            (F::NoResourcePack, "no_resource_pack", "forward.rs:921"),
            (F::Upstream, "no_healthy_account", "handler.go:1366"),
            (F::AllNoRoute, "no_healthy_account", "handler.go:1366"),
        ];
        for (kind, want, go) in cases {
            assert_eq!(
                Wire::Chat.failure_code(*kind),
                *want,
                "Chat + {kind:?} 的码面值必须与 Go {go} 逐字一致"
            );
        }
    }

    /// ★★ ⑪ 新增码 `no_resource_pack` 与两个近邻码必须互不相同。
    ///
    /// 这三者的出路**方向完全不同**，合并任意两个都会把用户带错路：
    /// - `quota_exhausted`（402）：账号真的没额度了 ⇒ 出路是**充值 / 等签到**；
    /// - `no_healthy_account`（502）：整池都不可用 ⇒ 用户会去查账号池，而账号一个都没坏；
    /// - `no_resource_pack`（400）：只是**这个模型**不在该账号的套餐里 ⇒ 出路是换模型或换号。
    ///
    /// 这条正是 ⑪ 缺陷的码面值护栏：满额 9,977 万 / 1 亿的 ZCode 账号收到
    /// `HTTP 429 {"error":{"code":"1113","message":"余额不足或无可用资源包,请充值。"}}`
    /// 时，通用分类器按「余额不足」判成额度耗尽并把整号硬冷却到次日 04:00。
    #[test]
    fn no_resource_pack_code_is_distinct_from_its_neighbours() {
        use crate::forward::FailureKind as F;
        let chat = Wire::Chat.failure_code(F::NoResourcePack);
        assert_eq!(chat, "no_resource_pack");
        assert_ne!(
            chat,
            Wire::Chat.failure_code(F::QuotaExhausted),
            "绝不能与「额度耗尽」共用码面值：那个的出路是充值，这个不是"
        );
        assert_ne!(
            chat,
            Wire::Chat.failure_code(F::Upstream),
            "绝不能落进 no_healthy_account（会让用户以为账号池全挂了）"
        );
        // Messages / Responses 两条线没有这个码 ⇒ 必须降级成通用请求错误，
        // 而不是把它们也变成 no_healthy_account 触发客户端重试。
        for w in [Wire::Messages, Wire::Responses] {
            assert_eq!(w.failure_code(F::NoResourcePack), "invalid_request_error");
        }
    }

    /// ★★ 两条最容易被「顺手合并」的码，单独钉一次。
    ///
    /// 它们各自的 Go 注释都写着「不能合并」，且理由**方向相反**：
    /// - `model_not_in_upstream`：出路是「换成上游真实存在的模型名」，
    ///   而 `model_not_in_product` 是「去白名单里放行」。混用会让用户
    ///   反复去配置页加一个不存在的模型（Go `handler.go:1103-1110`）。
    /// - `egress_ip_blocked` / `unusual_activity`：落进 `no_healthy_account`
    ///   会让客户端**继续重试**，而那正是当前最该避免的事（Go
    ///   `handler.go:1325-1331`：「会继续放大风控」）。
    #[test]
    fn the_two_never_merge_codes_are_distinct() {
        use crate::forward::FailureKind as F;
        assert_ne!(
            Wire::Chat.failure_code(F::ModelNotInUpstream),
            Wire::Chat.failure_code(F::ModelNotInProduct),
            "「上游没有」与「用户没放行」的出路相反，绝不能同码"
        );
        assert_ne!(
            Wire::Chat.failure_code(F::EgressIPBlocked),
            "no_healthy_account",
            "出口 IP 被拦重试无用，落 no_healthy_account 会诱导客户端继续打"
        );
        assert_ne!(
            Wire::Chat.failure_code(F::UnusualActivity),
            "no_healthy_account",
            "3012 与账号无关且要「退避不重试」，落 no_healthy_account 会把用户引去查账号"
        );
    }

    /// 三个请求侧码**各不相同** —— 合并会让客户端只能提示「请求有问题」，
    /// 而用户要改的是三件不同的事（换档位 / 换模型 / 改参数）。
    #[test]
    fn request_side_codes_are_not_collapsed() {
        use crate::forward::FailureKind as F;
        let codes = [
            Wire::Chat.failure_code(F::EffortRejected),
            Wire::Chat.failure_code(F::ModelNotInRegion),
            Wire::Chat.failure_code(F::ModelParamInvalid),
        ];
        for (i, a) in codes.iter().enumerate() {
            for b in &codes[i + 1..] {
                assert_ne!(a, b, "三个请求侧码的出路不同，不得合并成同一个面值");
            }
        }
        assert_eq!(codes[0], "reasoning_effort_rejected");
        assert_eq!(codes[1], "model_not_in_region");
        assert_eq!(codes[2], "model_param_invalid");
    }

    /// ★ Responses 的额度耗尽**不能**用 `upstream_error`。
    ///
    /// 那会让客户端以为「上游暂时坏了，重试即可」，而额度耗尽重试多少次都一样
    ///（Go `handler.go:1219-1221` 逐字）。Anthropic 同理。
    #[test]
    fn quota_exhausted_is_request_side_in_all_three_wires() {
        use crate::forward::FailureKind as F;
        assert_eq!(Wire::Chat.failure_code(F::QuotaExhausted), "quota_exhausted");
        assert_eq!(
            Wire::Responses.failure_code(F::QuotaExhausted),
            "invalid_request_error",
            "Responses 用 upstream_error 会诱导客户端重试空流"
        );
        assert_eq!(
            Wire::Messages.failure_code(F::QuotaExhausted),
            "invalid_request_error"
        );
    }

    /// ★ 另两个协议**不**透出 Chat 的专属码（词汇表不能互相串味）。
    ///
    /// Go `handler.go:1197-1204` 逐字：三个入口共享的是「谁来判定失败类别」
    /// 这条映射链，**不是同一个码面值**；把码面值也一起统一会让各协议的
    /// 词汇表互相串味。
    #[test]
    fn chat_only_codes_never_leak_into_other_wires() {
        use crate::forward::FailureKind as F;
        let chat_only = [
            F::ModelNotAllowed,
            F::EffortRejected,
            F::ModelNotInRegion,
            F::ModelParamInvalid,
            F::ModelNotInProduct,
            F::ModelNotInUpstream,
            F::PlatformNotInRouting,
            F::CaptchaRequired,
            F::QuotaExhausted,
            F::UnusualActivity,
            F::EgressIPBlocked,
            F::ImageRegionUnavailable,
            F::NoResourcePack,
        ];
        for kind in chat_only {
            let chat = Wire::Chat.failure_code(kind);
            for wire in [Wire::Messages, Wire::Responses] {
                let other = wire.failure_code(kind);
                assert_ne!(
                    other, chat,
                    "{wire:?} 不应透出 Chat 的专属码 {chat}（{kind:?}）"
                );
                // 另两个协议的码面值必须落在各自的词汇表里。
                assert!(
                    ["invalid_request_error", "api_error", "upstream_error"]
                        .contains(&other),
                    "{wire:?} + {kind:?} 的码 {other} 不属于该协议词汇表"
                );
            }
        }
    }
}

/// ★ 会话中断（业主 2026-10-03 报的「会话中断」）的回归护栏。
///
/// 上游吐过内容却**没**按约定写 `[DONE]` 就断流时，旧实现只补一帧 `[DONE]`
/// 就结束响应体。DSH 侧的 billion-context 插件据此判定 `sawTerminal == false`，
/// 合成一帧含糊的「upstream stream ended before a completion event」error
/// （`finished=false` 分支），客户端把它归成 `TRANSPORT` 后重试并弹出
/// 「会话中断」。修法是**先**发一帧终止性的 error（见 [`chat_stream_error_frame`]）再收尾。
#[cfg(test)]
mod chat_stream_error_tests {
    use super::*;

    /// 终止帧必须自带 `[DONE]`，且 `error.code` 让客户端一眼看出是上游截断。
    #[test]
    fn truncated_stream_error_frame_is_terminal_and_typed() {
        let text = chat_stream_error_frame(UPSTREAM_TRUNCATED_MSG);
        assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
        assert_eq!(text.matches("data: [DONE]").count(), 1, "{text}");
        let (first, second) = text
            .split_once("\n\n")
            .expect("error 帧与 [DONE] 之间必须空行分隔");
        assert_eq!(second, "data: [DONE]\n\n", "{text}");
        let payload = first.strip_prefix("data: ").expect("首行必须是 data: ");
        let v: Value = serde_json::from_str(payload).expect("error 帧必须是合法 JSON");
        assert_eq!(
            v["error"]["code"],
            serde_json::json!("upstream_stream_truncated"),
            "{v}"
        );
        assert_eq!(v["error"]["type"], serde_json::json!("upstream_error"), "{v}");
    }

    /// ★ 措辞是**功能性的**，不是文案：DSH 的 `classifyPiAiError` 用
    /// `/stream ended (?:before|without)\b/i` 把这类失败归为 `TRANSPORT`，
    /// 而 `TRANSPORT` 在 `DEFAULT_RETRYABLE_CODES` 里 ⇒ 才会自动换账号重试。
    /// 改成别的说法会落到 `PI_AI_ERROR`（不可重试），用户只能对着残缺回答发呆。
    #[test]
    fn truncated_message_stays_retryable_for_the_client() {
        let m = UPSTREAM_TRUNCATED_MSG;
        let lower = m.to_ascii_lowercase();
        assert!(lower.contains("stream ended before"), "{m}");
        // 别让分类器把它误判成鉴权失败 / 限流 / 服务器错误。
        for bad in ["401", "403", "429", "500", "502", "503"] {
            assert!(!lower.contains(bad), "{bad} 会改变客户端的错误归类：{m}");
        }
        assert!(
            chat_stream_error_frame(m).contains("stream ended before"),
            "{m}"
        );
    }

    /// ★ 文案要过 [`crate::clienttext::client_safe_message`]：裸 401/403 会被
    /// 客户端分类器当成鉴权失败（`AUTH`，不可重试）。
    #[test]
    fn truncated_error_frame_rewrites_bare_auth_codes() {
        let text =
            chat_stream_error_frame("upstream stream ended before the completion event: 403 Forbidden");
        assert!(text.contains("4xx"), "{text}");
        assert!(!text.contains("403"), "{text}");
    }

    /// ★★★ 2026-10-04 实测抓到的帧序缺陷：第一版把 error 帧**插在**收尾块前面，
    /// 又把收尾块照原样透传出去，响应体变成 `… error 帧 / data: [DONE] / data: [DONE]`。
    /// 客户端读到**第一个** `[DONE]` 就把这轮标成正常完成，后面那帧 error 直接被丢掉
    /// ⇒ 修复等于没修。所以：收尾块必须被识别出来并**替换**掉，不能追加。
    #[test]
    fn truncated_tail_block_is_replaced_not_appended() {
        let content = "data: {\"choices\":[{\"delta\":{\"content\":\"half\"}}]}\n\n";
        let tail = "data: [DONE]\n\n";
        assert!(!is_tail_block(content), "内容帧不能被误判成收尾块");
        assert!(is_tail_block(tail));
        // 替换语义：content 帧 + error 帧，收尾块本身不再出现。
        let body = format!(
            "{content}{}",
            chat_stream_error_frame(UPSTREAM_TRUNCATED_MSG)
        );
        assert_eq!(body.matches("data: [DONE]").count(), 1, "{body}");
        assert!(body.ends_with("data: [DONE]\n\n"), "{body}");
        let err_at = body.find("upstream_stream_truncated").expect("error 帧丢了");
        let done_at = body.rfind("data: [DONE]").expect("缺少收尾");
        assert!(err_at < done_at, "error 帧必须排在 [DONE] 之前：{body}");
    }
}

/// ★★★ 2026-10-04 实测钉死的缺陷：三个流式入口把 `pool.release` 写在 `async_stream`
/// 生成器的尾部，客户端中途断连时 axum drop 响应体 ⇒ 生成器被 drop ⇒ 尾部永不执行 ⇒
/// 名额永久占用（兼容网关页上表现为该号一直「正在使用」）。
///
/// 这一组用例直接对着 [`InFlightGuard`] 断言，不依赖真实的 HTTP 断连：
/// 守卫的语义就是「drop 时归还」，而生成器被 drop 时会连带 drop 它。
#[cfg(test)]
mod in_flight_guard_tests {
    use super::*;

    /// 建一个只有一条账号、名额上限为 1 的池，并占掉那唯一的在途名额。
    ///
    /// 刻意只造 [`Pool`]（不造 `AppState`）：本组用例只关心名额计数，
    /// 而 `AppState` 的测试构造器是 `mod tests` 的私有件。
    fn pool_with_one_held_slot() -> Arc<Mutex<Pool>> {
        let mut pool = Pool::new(String::new());
        let mut a = crate::auth::Auth::default();
        a.uid = "u1".to_string();
        pool.add(a);
        pool.set_max_in_flight(1);
        assert!(pool.acquire("u1"), "首次占用必须成功");
        assert!(!pool.acquire("u1"), "上限为 1 时第二次必须被拒");
        Arc::new(Mutex::new(pool))
    }

    fn in_flight(pool: &Arc<Mutex<Pool>>) -> i64 {
        pool.lock()
            .expect("池锁")
            .status_by_uid("u1")
            .expect("账号应在池里")
            .in_flight
    }

    /// 生成器**没有被驱动到底**（模拟客户端断连）时，名额也必须归还。
    #[test]
    fn dropped_guard_returns_the_slot() {
        let pool = pool_with_one_held_slot();
        assert_eq!(in_flight(&pool), 1);
        {
            // 只构造、不显式 release，出作用域即 drop —— 这正是断连时发生的事。
            let _guard = InFlightGuard::new(Arc::clone(&pool), "u1");
            assert_eq!(in_flight(&pool), 1, "构造守卫本身不该动名额");
        }
        assert_eq!(in_flight(&pool), 0, "守卫被 drop 后名额必须归还");
        assert!(
            pool.lock().expect("池锁").acquire("u1"),
            "归还后该账号必须能重新接流量（否则就是「正在使用」卡死）"
        );
    }

    /// 正常收尾路径：显式 release 之后 Drop 兜底**不能**再扣一次。
    #[test]
    fn explicit_release_then_drop_is_idempotent() {
        let pool = pool_with_one_held_slot();
        {
            let mut guard = InFlightGuard::new(Arc::clone(&pool), "u1");
            guard.release();
            assert_eq!(in_flight(&pool), 0);
            // 重复调用同样无副作用。
            guard.release();
            assert_eq!(in_flight(&pool), 0, "重复 release 不该把计数打成负数");
        }
        assert_eq!(in_flight(&pool), 0, "Drop 兜底不该二次扣减");
    }

    /// 账号已被移出池子（或 uid 写错）时，释放必须是空操作而不是 panic。
    #[test]
    fn releasing_an_unknown_uid_is_a_no_op() {
        let pool = pool_with_one_held_slot();
        let mut guard = InFlightGuard::new(Arc::clone(&pool), "nobody");
        guard.release();
        assert_eq!(in_flight(&pool), 1, "释放别人的名额不该动到自己的");
    }
}
