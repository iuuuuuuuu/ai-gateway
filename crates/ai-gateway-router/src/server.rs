//! OpenAI 兼容 HTTP 接口层。
//!
//! 对应 Go 源文件 `internal/server/handler.go`（路由 / 鉴权 / healthz / status / models）
//! 与 `internal/server/logging.go`（请求级表格日志、TTFB / token 统计）。
//!
//! # 路由表（与 Go 完全一致，含方法限定）
//!
//! | 方法   | 路径                  | 鉴权 | 说明                     |
//! |--------|-----------------------|------|--------------------------|
//! | POST   | `/v1/chat/completions`| 是   | 聊天（流式/非流式）      |
//! | GET    | `/v1/models`          | 是   | 模型列表                 |
//! | GET    | `/status`             | 是   | 账号池状态               |
//! | GET    | `/healthz`            | **否** | 探活（宿主识别用）      |
//!
//! `/healthz` 恒无鉴权：负载均衡/编排探活只需 2xx/503 语义，
//! 身份靠 `service` 字段 + `X-Service` 头双保险。

use axum::{
    extract::State,
    http::{header, HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

use crate::pool::Pool;

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
    /// 「单一模型」锁定；非空时只放行该模型。
    pub allowed_model: String,
    /// Token 用量统计（`/usage` 的数据来源）。
    pub usage: Arc<crate::usage::Stats>,
}

/// 构建路由。
///
/// 路由表与 Go 侧 `internal/server/handler.go` 一致，含**无版本号别名**
/// （`/responses`、`/messages`）：部分客户端会省略 `/v1` 前缀，缺了别名会 404。
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/chat/completions", axum::routing::post(chat_completions))
        .route("/v1/responses", axum::routing::post(responses))
        .route("/responses", axum::routing::post(responses))
        .route("/v1/messages", axum::routing::post(messages))
        .route("/messages", axum::routing::post(messages))
        .route("/v1/models", get(models))
        .route("/status", get(status))
        .route("/usage", get(usage_report))
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
fn openai_error(status: StatusCode, code: &str, msg: &str) -> Response {
    json_response(
        status,
        json!({
            "error": {
                "message": msg,
                "type": "api_error",
                "code": code,
            }
        }),
    )
}

/// Bearer 鉴权校验。
///
/// 与 Go 侧 `withAuth` 一致：key 为空则放行；否则要求
/// `Authorization: Bearer <key>` 精确匹配（区分大小写）。
fn check_auth(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if state.api_key.is_empty() {
        return None;
    }
    let expected = format!("Bearer {}", state.api_key);
    match headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        Some(v) if v == expected => None,
        _ => Some(openai_error(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "missing or invalid API key",
        )),
    }
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

/// 生成模型能力字段（图片输入等）。与 Go 侧 `modelCapabilityFields` 保持一致。
///
/// 为什么一次下发**多种拼写**：客户端读的字段名各不相同，且都只在各自的
/// provider 专用解析器里读，没有统一约定（实测 2026-09-16，见各客户端源码）：
///
/// - OpenClaw OpenAI Codex → `input_modalities` / `inputModalities`
/// - OpenClaw Copilot → `capabilities.supports.vision`
/// - OpenClaw HuggingFace → `architecture.input_modalities`
/// - OpenClaw OpenRouter → `architecture.modality`（`"text+image->text"`）
/// - OpenClaw Vercel AI Gateway → `tags` 含 `"vision"`
/// - OpenClaw LM Studio → `capabilities.vision`
/// - ZCode / DSH 的 `/v1/models` 解析器只读 id/context 等，不读能力字段
///
/// 多写几种是安全的：已知解析器都只取自己认识的键，多余键不会报错。
fn image_capability_fields() -> Value {
    json!({
        "supportsImages": true,
        "input_modalities": ["text", "image"],
        "inputModalities": ["text", "image"],
        "capabilities": { "vision": true, "supports": { "vision": true } },
        "architecture": {
            "input_modalities": ["text", "image"],
            "modality": "text+image->text",
        },
        "tags": ["vision"],
        "modalities": { "input": ["text", "image"], "output": ["text"] },
    })
}

/// 给静态表条目补上能力字段。
///
/// 静态表取自 `/v3/config` 的 `agents[cli].models`，实测（2026-09-16，国服 16 个
/// cli 模型 + 国际版 21 个模型池）该清单下**全部**模型 `supportsImages=true`。
///
/// 本 crate 目前只内置静态表（无动态拉取），因此统一标注为支持图片。
/// 不标注的话，客户端会把所有模型当纯文本 —— 图片能力整个消失，而网关
/// 看起来完全正常（返回 200 + 完整列表），是最难排查的一类问题。
fn with_image_capability(mut entries: Vec<Value>) -> Vec<Value> {
    for entry in entries.iter_mut() {
        if let Some(obj) = entry.as_object_mut() {
            if let Some(caps) = image_capability_fields().as_object() {
                for (k, v) in caps {
                    obj.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }
    }
    entries
}

/// 静态 CN 模型表（动态接口失败时的回退）。
///
/// 与 Go 侧 `staticModels` 完全一致（含 created / context_length 取值）。
fn static_models_cn() -> Vec<Value> {
    const IDS: &[&str] = &[
        "glm-5.2",
        "glm-5.1",
        "glm-5v-turbo",
        "kimi-k2.7",
        "minimax-m3",
        "hy3",
        "hy3-preview",
        "hy3-preview-agent",
        "deepseek-v4-pro",
        "deepseek-v4-flash",
    ];
    with_image_capability(
        IDS.iter()
            .map(|id| {
                json!({
                    "id": id,
                    "object": "model",
                    "created": 1753600000,
                    "owned_by": "workbuddy",
                    "context_length": 131072,
                })
            })
            .collect(),
    )
}

/// 国际版静态模型表。
///
/// 国际版的 `/console/enterprises/personal/models` 在当前版本返回 500，
/// 无法动态拉取，因此内置一份。
fn static_models_intl() -> Vec<Value> {
    const SHORT: &[(&str, i64)] = &[
        ("default-model", 200000),
        ("fast-model", 176000),
        ("balanced-model", 176000),
        ("primary-model", 176000),
        ("deep-model", 176000),
    ];
    const STD: &[&str] = &[
        "hy4-preview",
        "hy3",
        "deepseek-v4.1-flash",
        "gpt-6-astra",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
        "gpt-5.5",
        "gpt-5.4",
        "gpt-5.3-codex",
        "gemini-3.5-flash",
        "glm-5.3",
        "glm-5.2",
        "kimi-k3",
        "kimi-k2.6",
    ];
    let mut out = Vec::new();
    for (id, ctx) in SHORT {
        out.push(json!({
            "id": id, "object": "model", "created": 1753600000,
            "owned_by": "workbuddy-intl", "context_length": ctx,
        }));
    }
    for id in STD {
        out.push(json!({
            "id": id, "object": "model", "created": 1753600000,
            "owned_by": "workbuddy-intl", "context_length": 131072,
        }));
    }
    with_image_capability(out)
}

/// 合并两个区域的模型（按 id 去重，国服优先）。
///
/// `/v1/models` 没有账号上下文，因此返回并集：客户端据此得知全部可用名称。
fn static_models_all() -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in static_models_cn().into_iter().chain(static_models_intl()) {
        if let Some(id) = m.get("id").and_then(|v| v.as_str()) {
            if id.is_empty() || !seen.insert(id.to_string()) {
                continue;
            }
            out.push(m);
        }
    }
    out
}

/// `GET /v1/models` —— 模型列表（需鉴权）。
async fn models(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(r) = check_auth(&state, &headers) {
        return r;
    }
    json_response(
        StatusCode::OK,
        json!({
            "object": "list",
            "data": static_models_all(),
        }),
    )
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
        return openai_error(
            StatusCode::from_u16(f.status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            code,
            &f.message,
        );
    }

    match result {
        Some(crate::forward::ChatResult::Stream {
            uid,
            model,
            response,
        }) => chat_stream_response(response, state.clone(), uid, model),
        Some(crate::forward::ChatResult::Response { body, uid, model }) => {
            record_usage(&state, &uid, &model, body.get("usage"));
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
fn record_usage(state: &Arc<AppState>, uid: &str, model: &str, usage: Option<&Value>) {
    if let Some(c) = crate::usage::parse_openai_usage(usage) {
        state.usage.record(uid, model, c);
    }
}

/// 请求体上限。
///
/// 从 8MB 提到 32MB：长对话（Claude Code / Codex 一轮带上大量文件内容与工具结果）
/// 很容易突破 8MB，而**静默截断**会把合法 JSON 切成半截字节透传给上游，
/// 上游报 `unexpected EOF`，表现为「请求参数有误」—— 客户端完全无法定位到是网关截断。
const MAX_REQUEST_BODY: usize = 32 << 20;

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
    fn error(self, status: StatusCode, code: &str, msg: &str) -> Response {
        match self {
            Wire::Messages => {
                json_response(status, crate::protocol::anthropic::anthropic_error(code, msg))
            }
            Wire::Responses => {
                json_response(status, crate::protocol::responses::responses_error(code, msg))
            }
            Wire::Chat => openai_error(status, code, msg),
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
    /// - 单一模型拒绝：Chat 用 `model_not_allowed`（本网关为 chat 形状定的码），
    ///   Responses / Anthropic 用各自的 `invalid_request_error`。
    /// - 其余：Chat 沿用 `no_healthy_account`；另两个协议用 `api_error` / `upstream_error`。
    fn failure_code(self, kind: crate::forward::FailureKind) -> &'static str {
        use crate::forward::FailureKind as F;
        match (self, kind) {
            (Wire::Chat, F::ContextTooLong) => "context_length_exceeded",
            (Wire::Responses, F::ContextTooLong) => "context_length_exceeded",
            (Wire::Messages, F::ContextTooLong) => "invalid_request_error",
            (Wire::Chat, F::ImageRegionUnavailable) => "image_region_unavailable",
            (Wire::Messages, F::ImageRegionUnavailable) => "invalid_request_error",
            (Wire::Responses, F::ImageRegionUnavailable) => "invalid_request_error",
            (Wire::Chat, F::Upstream) => "no_healthy_account",
            (Wire::Messages, F::Upstream) => "api_error",
            (Wire::Responses, F::Upstream) => "upstream_error",
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
    // 区域路由：仅对「图像能力两区不同」的模型 + 带图片的请求生效。
    let model = crate::forward::model_of(body);
    let route = image_route_for(&model, crate::forward::request_has_image(body));

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
    };
    crate::forward::forward_chat(&mut ctx, body, stream, sess_key, route).await
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
            StatusCode::from_u16(f.status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            code,
            &f.message,
        );
    }

    match result {
        Some(crate::forward::ChatResult::Stream { uid, response, .. }) => {
            anthropic_stream_response(response, &req.model, state.clone(), uid)
        }
        Some(crate::forward::ChatResult::Response { body, uid, .. }) => {
            // 非流式：usage 就在聚合后的 body 里，直接记。
            record_usage(&state, &uid, &req.model, body.get("usage"));
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
            StatusCode::from_u16(f.status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            code,
            &f.message,
        );
    }

    match result {
        Some(crate::forward::ChatResult::Stream { uid, response, .. }) => {
            responses_stream_response(response, &req.model, state.clone(), uid)
        }
        Some(crate::forward::ChatResult::Response { body, uid, .. }) => {
            // 非流式：usage 就在聚合后的 body 里，直接记。
            record_usage(&state, &uid, &req.model, body.get("usage"));
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
) -> Response {
    use crate::protocol::anthropic_stream::{AnthropicStreamState, SseOut};
    let mut frames = crate::upstream::sse::SseFramesIter::new(upstream.bytes_stream());
    let model = model.to_string();
    let body = async_stream::stream! {
        let mut state = AnthropicStreamState::new(&model);
        let mut out = SseOut::new();
        if state.start(&mut out).is_err() {
            if let Ok(mut pool) = state_ref.pool.lock() { pool.release(&uid); }
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
                                    if let Ok(mut pool) = state_ref.pool.lock() { pool.release(&uid); }
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
                    if let Ok(mut pool) = state_ref.pool.lock() { pool.release(&uid); }
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
            if let Ok(mut pool) = state_ref.pool.lock() { pool.release(&uid); }
            return;
        }
        if state.finish(&mut out).is_ok() && !out.is_empty() {
            yield Ok(out.take());
        }
        // 收尾：记用量 + 释放租约（流已结束，二者都不再依赖连接）。
        record_usage(&state_ref, &uid, &model, state.usage());
        if let Ok(mut pool) = state_ref.pool.lock() {
            pool.release(&uid);
        }
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
) -> Response {
    use crate::protocol::anthropic_stream::SseOut;
    use crate::protocol::responses_stream::ResponsesStreamState;
    let mut frames = crate::upstream::sse::SseFramesIter::new(upstream.bytes_stream());
    let model = model.to_string();
    let body = async_stream::stream! {
        let mut state = ResponsesStreamState::new(&model);
        let mut out = SseOut::new();
        if state.created_event(&mut out).is_err() {
            if let Ok(mut pool) = state_ref.pool.lock() { pool.release(&uid); }
            return;
        }
        yield Ok::<_, std::io::Error>(out.take());
        loop {
            match frames.next_frame().await {
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
                                    if let Ok(mut pool) = state_ref.pool.lock() { pool.release(&uid); }
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
                    if let Ok(mut pool) = state_ref.pool.lock() { pool.release(&uid); }
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
            if let Ok(mut pool) = state_ref.pool.lock() { pool.release(&uid); }
            return;
        }
        if state.finish(&mut out).is_ok() && !out.is_empty() {
            yield Ok(out.take());
        }
        // 收尾：记用量 + 释放租约。
        record_usage(&state_ref, &uid, &model, state.usage());
        if let Ok(mut pool) = state_ref.pool.lock() {
            pool.release(&uid);
        }
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
fn sse_headers() -> axum::http::response::Builder {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("X-Accel-Buffering", "no")
}

/// 把上游 SSE 响应包成 `text/event-stream`，逐帧规范化后流式透传。
///
/// 空流（0 有效帧）时补一帧 error 与 `[DONE]`，与 Go 侧 `upstream.Stream` 一致。
///
/// 顺带在流末尾抓取上游的 `usage`（它只在末帧到达）记入统计，并释放账号租约 ——
/// 三件事共用同一次流遍历，不需要为了统计把整条流缓存下来。
fn chat_stream_response(
    upstream: reqwest::Response,
    state_ref: Arc<AppState>,
    uid: String,
    model: String,
) -> Response {
    let stream = upstream.bytes_stream();
    let mut frames = crate::upstream::sse::SseFramesIter::new(stream);
    let body = async_stream::stream! {
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
                    yield Ok::<_, std::io::Error>(text);
                }
                Err(e) => {
                    yield Err(std::io::Error::other(e.to_string()));
                    break;
                }
            }
        }
        record_usage(&state_ref, &uid, &model, last_usage.as_ref());
        if let Ok(mut pool) = state_ref.pool.lock() {
            pool.release(&uid);
        }
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
            allowed_model: String::new(),
            usage: Arc::new(crate::usage::Stats::new("")),
        })
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

    #[tokio::test]
    async fn models_returns_union_with_cn_first() {
        let app = router(test_state(""));
        let res = app
            .oneshot(Request::builder().uri("/v1/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["object"], "list");
        let data = v["data"].as_array().unwrap();
        // 国服优先去重：glm-5.2 应出现且 owned_by 为 workbuddy
        let glm = data.iter().find(|m| m["id"] == "glm-5.2").unwrap();
        assert_eq!(glm["owned_by"], "workbuddy");
        assert_eq!(glm["context_length"], 131072);
        assert_eq!(glm["created"], 1753600000);
        // id 唯一
        let mut ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n, "模型 id 应去重");
    }

    /// `/v1/models` 必须下发模型能力（图片输入）。
    ///
    /// 实测缺陷（2026-09-16）：模型列表只有 id/object/created/owned_by/context_length，
    /// 没有任何能力字段，于是「有多模态模型但客户端发不出图片」。能读该字段的
    /// 客户端（OpenClaw 的 Codex/Copilot/HuggingFace/OpenRouter/Vercel/LM Studio
    /// 解析器）拿不到能力信号，只能按纯文本处理 —— 构建期完全看不出来。
    #[tokio::test]
    async fn models_expose_image_capability_spellings() {
        let app = router(test_state(""));
        let res = app
            .oneshot(Request::builder().uri("/v1/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        let data = v["data"].as_array().unwrap();
        let glm = data.iter().find(|m| m["id"] == "glm-5.2").unwrap();

        // 各客户端读的拼写都不同，逐条锁住，避免以后有人"清理重复字段"。
        assert_eq!(glm["supportsImages"], true);
        for key in ["input_modalities", "inputModalities"] {
            let mods = glm[key].as_array().expect("应为数组（OpenClaw Codex）");
            assert!(mods.iter().any(|m| m == "image"), "{key} 应含 image");
        }
        assert_eq!(glm["capabilities"]["vision"], true, "OpenClaw LM Studio");
        assert_eq!(
            glm["capabilities"]["supports"]["vision"], true,
            "OpenClaw Copilot"
        );
        assert!(
            glm["architecture"]["input_modalities"]
                .as_array()
                .is_some_and(|m| m.iter().any(|x| x == "image")),
            "OpenClaw HuggingFace"
        );
        assert!(
            glm["architecture"]["modality"]
                .as_str()
                .is_some_and(|m| m.contains("image")),
            "OpenClaw OpenRouter 形如 text+image->text"
        );
        assert!(
            glm["tags"].as_array().is_some_and(|t| t.iter().any(|x| x == "vision")),
            "OpenClaw Vercel AI Gateway"
        );

        // 国际版静态表条目也要带能力（两个区域共用同一套标注）
        let astra = data.iter().find(|m| m["id"] == "gpt-6-astra").unwrap();
        assert_eq!(astra["supportsImages"], true, "国际版条目也应带能力字段");
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
}
