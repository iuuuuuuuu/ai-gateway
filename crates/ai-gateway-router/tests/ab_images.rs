//! A/B 对照：**Anthropic Messages 入站带图片**的两条真实缺陷（2026-10-11 现场）。
//!
//! 现场：Claude CLI 走网关 /v1/messages 发截图，模型回「无法查看图片」。两个独立
//! 原因叠在一起，本文件各自钉一条：
//!
//! 1. **入站丢图**：protocol/anthropic.rs 的块循环对 "image" 块是空分支 —— 图片在
//!    协议转换处被静默丢掉，上游收到的请求里**根本没有图**（模型只能回「我看不到」）。
//!    修好后 Chat 形态里必须是 [{type:"text"},{type:"image_url",…}] 分片数组，
//!    且 data:image/png;base64,<data> 逐字完整（脱敏、整流、改写都不得动它）。
//! 2. **区域路由**：同名模型两区的图像能力不同（glm-5.3 国服能读、国际版把图换成
//!    固定占位符 ⇒ prompt_tokens 增量恒 +33，模型只能回「无法查看图片」）。带图
//!    请求必须**收紧到能读图的那个区域**；选不出号要如实报「缺哪个区域的账号」，
//!    **绝不**静默跨区降级（那正是「时好时坏」的来源）。
//!
//! 另外钉一条**零误伤对照**与一条**预算回归**：
//!
//! 3. 纯文本请求**不**做区域收紧：为图片放弃一半账号的额度是白损失。
//! 4. 内联图片的 base64 不得被算成上下文 token：旧估算把 1 MB base64 按 4 字符/token
//!    算成 26 万 token ⇒ 带图请求被误判「预算超限」⇒ 上游真回 11133 时被归成
//!    ContextTooLong，客户端于是**反复压缩**一个根本没超限的请求。
//!
//! 为什么起真服务：缺陷 1 出在「请求体字节」上（要看到上游真正收到什么），缺陷 2 出在
//! 「选号 + HTTP 往返」上，两者都 mock 不掉（见 ab_prompt.rs 文件头同款论证）。

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use ai_gateway_router::auth::{Auth, Region};
use ai_gateway_router::models_cache::ModelsCache;
use ai_gateway_router::pool::Pool;
use ai_gateway_router::server::{router, AppState};
use ai_gateway_router::upstream::client::{Client, ClientConfig, ModelInfo};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use serde_json::{json, Value};
use tower::ServiceExt;

/// 健康上游的最小合法 SSE。
const OK_BODY: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";

/// 上游把「输入 + max_tokens 超预算」也报成 11133 时的形状。
///
/// ⚠ 刻意**不带** context / too long 字样：classify 会先判文案
///（CONTEXT_TOO_LONG_MARKERS 与 "too long"/"too large"/"context"），带上就落进
/// ContextTooLong，本用例要测的 overflow_guess 那一支就永远走不到了。
const PARAM_INVALID_BODY: &str = r#"{"code":11133,"message":"model_param_invalid"}"#;

/// 假上游收到的一次请求（记账号是为了确认**哪个区域**的号被打到了）。
struct Seen {
    authorization: String,
    body: Vec<u8>,
}

/// 起一个假上游：记录每次请求（Authorization + 请求体）；param_invalid=true 回
/// 400 + 11133，否则回最小合法 SSE。
async fn spawn_upstream(param_invalid: bool) -> (String, Arc<Mutex<Vec<Seen>>>) {
    use axum::routing::post;

    let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);

    let handler = move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
        let sink = Arc::clone(&sink);
        async move {
            let authorization = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            sink.lock().unwrap().push(Seen {
                authorization,
                body: body.to_vec(),
            });
            if param_invalid {
                (
                    StatusCode::BAD_REQUEST,
                    [("content-type", "application/json")],
                    PARAM_INVALID_BODY,
                )
            } else {
                (
                    StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    OK_BODY,
                )
            }
        }
    };

    // ⚠ axum 的 `Bytes` 提取器默认**只收 2 MB**，超出直接回 413（且处理器根本不执行 ——
    // 表现为"上游一个请求都没收到"）。预算回归用例刻意发 4 MB 级请求体，必须放开：
    // 这里模拟的是上游，而真实上游当然收得下几 MB 的请求。
    let app = axum::Router::new()
        .route("/", post(handler.clone()))
        .route("/*rest", post(handler))
        .layer(axum::extract::DefaultBodyLimit::max(64 << 20));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://127.0.0.1:{}", addr.port()), seen)
}

/// 造一个账号（domain 决定区域：*.workbuddy.cn = 国服，*.workbuddy.ai = 国际版）。
fn account(uid: &str, domain: &str) -> Auth {
    Auth {
        uid: uid.into(),
        access_token: format!("at-{uid}"),
        enterprise_id: format!("e-{uid}"),
        expires_at: 9999999999,
        domain: domain.into(),
        ..Default::default()
    }
}

/// 组装一个指向假上游、带给定账号的 AppState。
///
/// models 非空时作为真值清单注入（只有预算回归用例需要 —— 模型不在清单里
/// budget_exceeded_guess 直接返回 None，测不出误判）。
async fn state_with(
    accounts: Vec<Auth>,
    param_invalid: bool,
    models: Option<Arc<ModelsCache>>,
) -> (Arc<AppState>, Arc<Mutex<Vec<Seen>>>) {
    let (base, seen) = spawn_upstream(param_invalid).await;
    let mut pool = Pool::new(String::new());
    for a in accounts {
        pool.add(a);
    }

    let state = Arc::new(AppState {
        pool: Arc::new(Mutex::new(pool)),
        // 两区基址都指向假上游：本对照要覆盖国服与国际版两条路径。
        client: Client::new(ClientConfig {
            chat_base_cn: base.clone(),
            base_intl: base,
            ..Default::default()
        })
        .expect("构造测试客户端"),
        session: None,
        api_key: String::new(),
        max_rotate: 1,
        in_flight_wait: Duration::ZERO,
        soft_cooldown: Duration::from_secs(60),
        refresh_skew: Duration::from_secs(600),
        redis_mode: String::new(),
        allowed_model: Vec::new(),
        usage: Arc::new(ai_gateway_router::usage::Stats::new("")),
        products: ai_gateway_router::products::ProductUpstreams::new(),
        unusual: ai_gateway_router::unusual::UnusualBreakers::new(),
        waf_ip: ai_gateway_router::wafip::WafIpWindow::new(),
        empty_stream: ai_gateway_router::emptystream::EmptyStreamWindow::new(),
        probe_timeout: Duration::from_secs(30),
        models: models.unwrap_or_else(|| Arc::new(ModelsCache::new())),
        dynamic: ai_gateway_router::cfgwatch::DynamicState::new(
            ai_gateway_router::cfgwatch::DynamicConfig::default(),
        ),
        auth_dir: String::new(),
        // 本对照期间不让后台任务打上游搅浑观测序列。
        scheduler: None,
        growtask: None,
    });
    (state, seen)
}

/// 发一次 /v1/messages 请求并回 (状态码, 响应 JSON)。
async fn messages_roundtrip(state: Arc<AppState>, body: &str) -> (StatusCode, Value) {
    let app = router(state);
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 24).await.unwrap();
    let json = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// 等待上游收满 want 个请求（本地回环，给一小段余量）。
async fn wait_calls(seen: &Arc<Mutex<Vec<Seen>>>, want: usize) {
    for _ in 0..200 {
        if seen.lock().unwrap().len() >= want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("等待上游第 {want} 个请求超时（实收 {}）", seen.lock().unwrap().len());
}

/// 造一张 PNG 的 base64（raw_len 控制体积：base64 长度 = 4/3 × raw_len）。
///
/// 只有 PNG 文件头是真的（image_dimensions 读偏移 16..24 的宽高），其余补零 ——
/// 网关只解前 512 字节读宽高，不需要一张能显示的图。
fn png_b64(w: u32, h: u32, raw_len: usize) -> String {
    let mut b = vec![0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    b.extend_from_slice(&13u32.to_be_bytes());
    b.extend_from_slice(b"IHDR");
    b.extend_from_slice(&w.to_be_bytes());
    b.extend_from_slice(&h.to_be_bytes());
    b.extend_from_slice(&[8, 6, 0, 0, 0]);
    if b.len() < raw_len {
        b.resize(raw_len, 0u8);
    }
    base64::engine::general_purpose::STANDARD.encode(&b)
}

/// 一条 Anthropic 请求：单个 user 消息，content 为给定分片数组。
fn messages_body(content: Value, max_tokens: i64) -> String {
    json!({
        "model": "glm-5.3",
        "max_tokens": max_tokens,
        "messages": [{"role": "user", "content": content}],
    })
    .to_string()
}

const TEXT: &str = "你能看到图片吗？";

/// 【缺陷 1】Anthropic 的 image 块必须活着到达上游，且 base64 逐字完整。
///
/// 池里同时放国服与国际版账号：带图请求必须落在**国服**号上（缺陷 2 的收紧），
/// 且请求体里是 [{type:"text"},{type:"image_url",…}] 分片数组。
#[tokio::test]
async fn image_blocks_reach_upstream_and_stay_region_pinned() {
    let (state, seen) = state_with(
        vec![
            account("u-intl", "www.workbuddy.ai"),
            account("u-cn", "www.workbuddy.cn"),
        ],
        false,
        None,
    )
    .await;

    let img_a = png_b64(800, 600, 1200);
    let img_b = png_b64(1200, 900, 1500);
    let body = messages_body(
        json!([
            {"type": "text", "text": TEXT},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": img_a}},
            {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": img_b}},
        ]),
        64,
    );

    let (status, resp) = messages_roundtrip(Arc::clone(&state), &body).await;
    assert_eq!(status, StatusCode::OK, "resp={resp}");

    wait_calls(&seen, 1).await;
    let calls = seen.lock().unwrap();
    assert_eq!(calls.len(), 1, "只该打一次上游");
    let call = &calls[0];

    // 缺陷 2：带图请求只能落在国服号上（池里那个国际版号必须被排除）。
    assert_eq!(
        call.authorization, "Bearer at-u-cn",
        "带图片的 glm-5.3 请求必须选中国服账号"
    );

    // 缺陷 1：图片必须原样在请求体里。
    let wire: Value = serde_json::from_slice(&call.body).expect("上游收到的应是 JSON");
    let msgs = wire["messages"].as_array().expect("messages 数组");
    let user = msgs
        .iter()
        .find(|m| m["role"] == "user")
        .expect("用户消息必须还在");
    let parts = user["content"]
        .as_array()
        .expect("带图消息的 content 必须是分片数组（旧实现这里是纯字符串，图片已被丢掉）");
    assert_eq!(parts.len(), 3, "text + 两张图：parts={parts:?}");
    assert_eq!(parts[0]["type"], "text");
    let got_text = parts[0]["text"].as_str().unwrap_or("");
    assert!(got_text.contains("你能看到图片吗"), "文本分片不该被改写: {got_text}");
    assert_eq!(parts[1]["type"], "image_url");
    assert_eq!(
        parts[1]["image_url"]["url"],
        format!("data:image/png;base64,{img_a}"),
        "PNG 分片必须逐字完整（脱敏/整流都不得改动它）"
    );
    assert_eq!(parts[2]["type"], "image_url");
    assert_eq!(
        parts[2]["image_url"]["url"],
        format!("data:image/jpeg;base64,{img_b}"),
        "media_type 必须跟着分片走，不能一律当成 png"
    );
}

/// 【缺陷 2】池里没有能读图的区域账号 ⇒ 503 + 如实说明缺哪个区域，且**零上游请求**。
///
/// 静默跨区降级（旧行为）会让国际版后端把图换成占位符，模型回「无法查看图片」——
/// 用户完全无从判断是网络、模型还是网关的问题。
#[tokio::test]
async fn image_request_without_readable_region_names_the_cause() {
    let (state, seen) = state_with(vec![account("u-intl", "www.workbuddy.ai")], false, None).await;

    let body = messages_body(
        json!([
            {"type": "text", "text": TEXT},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": png_b64(800, 600, 900)}},
        ]),
        64,
    );

    let (status, resp) = messages_roundtrip(Arc::clone(&state), &body).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "resp={resp}");
    // Anthropic 入口的区域码归 invalid_request_error（请求侧：要么补号，要么去掉图片）。
    assert_eq!(resp["error"]["type"], "invalid_request_error", "resp={resp}");
    let msg = resp["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("国服"), "文案必须点明缺哪个区域: {msg}");
    assert!(msg.contains("无法查看图片"), "文案必须解释另一区域的现象: {msg}");
    assert!(msg.contains("去掉图片后重试"), "文案必须给出出路: {msg}");

    // 零上游请求：宁可明确失败，也不能把图交给读不到图的后端。
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(seen.lock().unwrap().len(), 0, "区域不可用时一个上游请求都不该发");
}

/// 【零误伤对照】纯文本请求不做区域收紧：国际版号照样能用 glm-5.3。
#[tokio::test]
async fn text_request_is_not_region_pinned() {
    let (state, seen) = state_with(vec![account("u-intl", "www.workbuddy.ai")], false, None).await;

    let body = messages_body(json!("hi"), 64);
    let (status, resp) = messages_roundtrip(Arc::clone(&state), &body).await;
    assert_eq!(status, StatusCode::OK, "resp={resp}");

    wait_calls(&seen, 1).await;
    let calls = seen.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].authorization, "Bearer at-u-intl");
    let wire: Value = serde_json::from_slice(&calls[0].body).unwrap();
    let user = wire["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .unwrap();
    assert_eq!(user["content"], "hi", "无图时 content 保持字符串（不无谓改形状）");
}

/// 【预算回归】内联图片的 base64 **不得**被算成上下文 token。
///
/// 形状刻意复刻业主现场：小文本 + 巨量图片 base64（4 × 1 MB）+ 极大 max_tokens。
/// 旧估算把 4 MB base64 按 4 字符/token 算成约 105 万 token ⇒ 输入 + 900000 远超
/// 1,048,576 上限 ⇒ overflow_guess = Some(true) ⇒ 上游真回 11133 时被归成
/// ContextTooLong（文案「请压缩或清空会话…后重试」）⇒ 客户端反复压缩一个没超限的请求。
/// 修好后图片按宽高另计（每张 800×600 ⇒ 640 token）⇒ 估算为假 ⇒ 如实回 11133。
#[tokio::test]
async fn inline_images_do_not_fake_context_overflow() {
    let models = Arc::new(ModelsCache::new());
    // 真值清单：模型不在清单里 budget_exceeded_guess 返回 None（不猜），测不出误判。
    models.record_success(
        Region::CN,
        vec![ModelInfo {
            id: "glm-5.3".into(),
            ..Default::default()
        }],
        SystemTime::now(),
    );

    let (state, seen) = state_with(
        vec![account("u-cn", "www.workbuddy.cn")],
        true,
        Some(Arc::clone(&models)),
    )
    .await;

    // 4 × 1 MB base64（每张 800×600 ⇒ 640 token），合计约 4.2 MB 请求体。
    let big: Vec<Value> = (0..4)
        .map(|_| {
            json!({
                "type": "image",
                "source": {"type": "base64", "media_type": "image/png", "data": png_b64(800, 600, 786_432)},
            })
        })
        .collect();
    let mut content = vec![json!({"type": "text", "text": TEXT})];
    content.extend(big);
    let body = messages_body(Value::Array(content), 900_000);
    assert!(body.len() > 4_000_000, "用例前提：请求体确实带 4 MB 级 base64");

    let (status, resp) = messages_roundtrip(Arc::clone(&state), &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "resp={resp}");
    assert_eq!(resp["error"]["type"], "invalid_request_error", "resp={resp}");
    let msg = resp["error"]["message"].as_str().unwrap_or("");
    // model_param_invalid_message 的首句带 code=11133；context_too_long_message 不带。
    assert!(msg.contains("code=11133"), "应如实回 11133: {msg}");
    assert!(msg.contains("换账号无效"), "应说清这不是账号问题: {msg}");
    // 只有 context_too_long_message 用「…后重试」收尾（model_param_invalid 用「请任选一种方式重试」）。
    assert!(
        !msg.contains("后重试"),
        "图片 base64 不得让估算误判超限（那会让客户端反复压缩）: {msg}"
    );

    wait_calls(&seen, 1).await;
    assert_eq!(seen.lock().unwrap().len(), 1);
}

