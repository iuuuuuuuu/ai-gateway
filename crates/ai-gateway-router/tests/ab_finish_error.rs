//! A/B 对照：上游用【正常帧的形状】宣告本轮失败（2026-10-12 现场）。
//!
//! 业主原话：「为什么这个模型在对话过程中一直在中断?」
//!
//! 现场实测：workbuddy 后端对单轮生成有约 **900s** 的硬上限，到点不回 HTTP 错误，
//! 而是在流尾写一帧
//!
//!   data: {"choices":[{"index":0,"delta":{},"finish_reason":"error"}]}
//!
//! 就收尾（实测跨度 900.572s / 900.701s / 892.7s）。这一帧【没有】 error 字段，
//! 形状与正常收尾帧完全一样，只有取值不同。
//!
//! 旧口径下的去向（两个环节同时失灵）：
//!   1. 网关：is_upstream_error_frame 把「非空 finish_reason」当成「有实际产出
//!      ⇒ 不是错误」⇒ 连一条日志都没有、账号也没喂熔断；
//!   2. 客户端：pi-ai 的 mapStopReason 没有 error 分支，落到 default 得到
//!      Provider finish_reason: error ⇒ DSH 的 classifyPiAiError 兜底成
//!      PI_AI_ERROR ⇒ **不在** DEFAULT_RETRYABLE_CODES ⇒ 不重试、直接
//!      「本轮运行失败」，用户看到的就是「对话一直在中断」。
//!
//! 本对照钉住修复后的客户端可见字节：
//!   1. 上游原文**绝不透传**（透传 = 客户端拿到不可重试的 PI_AI_ERROR）；
//!   2. 换成一帧可读、**可重试**的 error（措辞含 stream ended before ⇒ 客户端
//!      归 TRANSPORT ⇒ 自动换账号重试）；
//!   3. 该账号按「上游故障」喂一次熔断（err_total +1），但不被禁用；
//!   4. 零误伤：正常的 finish_reason:"stop" 流照旧透传、账号不挨罚。
//!
//! 为什么起真服务：缺陷出在「HTTP 往返 + SSE 逐帧透传」本身，mock 掉 transport
//! 就测不到（同 ab_emptystream.rs / ab_prompt.rs 文件头的论证）。

use std::sync::{Arc, Mutex};

use ai_gateway_router::auth::Auth;
use ai_gateway_router::pool::Pool;
use ai_gateway_router::server::{router, AppState};
use ai_gateway_router::upstream::client::{Client, ClientConfig};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

/// 现场复刻：先吐一点内容，再用失败语义的 finish_reason 收尾（**无** [DONE]）。
const FAILURE_BODY: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"写了一半\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"error\"}]}\n\n",
);

/// 零误伤对照：正常收尾（有内容、stop、带 [DONE]）。
const OK_BODY: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

/// 起一个假上游：记录每次收到的请求体，固定回 body。
async fn spawn_upstream(body: &'static str) -> (String, Arc<Mutex<Vec<Vec<u8>>>>) {
    use axum::routing::post;

    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);

    let handler = move |b: axum::body::Bytes| {
        let sink = Arc::clone(&sink);
        async move {
            sink.lock().unwrap().push(b.to_vec());
            ([("content-type", "text/event-stream")], body)
        }
    };

    let app = axum::Router::new()
        .route("/", post(handler.clone()))
        .route("/*rest", post(handler));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://127.0.0.1:{}", addr.port()), seen)
}

/// 国际区账号（uid/token/enterprise 两两不同 ⇒ 窗口按账号去重）。
fn intl_account(i: usize) -> Auth {
    Auth {
        uid: format!("u{i}"),
        access_token: format!("at{i}"),
        enterprise_id: format!("e{i}"),
        expires_at: 9999999999,
        domain: "www.workbuddy.ai".into(),
        ..Default::default()
    }
}

/// 组装一个指向假上游、带 n 个国际区账号的 AppState。
async fn state_with(
    body: &'static str,
    n: usize,
    max_rotate: usize,
) -> (Arc<AppState>, Arc<Mutex<Vec<Vec<u8>>>>) {
    let (base, seen) = spawn_upstream(body).await;
    let mut pool = Pool::new(String::new());
    for i in 0..n {
        pool.add(intl_account(i));
    }

    let state = Arc::new(AppState {
        pool: Arc::new(Mutex::new(pool)),
        client: Client::new(ClientConfig {
            base_intl: base,
            ..Default::default()
        })
        .expect("构造测试客户端"),
        session: None,
        api_key: String::new(),
        max_rotate,
        in_flight_wait: std::time::Duration::ZERO,
        soft_cooldown: std::time::Duration::from_secs(60),
        refresh_skew: std::time::Duration::from_secs(600),
        redis_mode: String::new(),
        allowed_model: Vec::new(),
        usage: Arc::new(ai_gateway_router::usage::Stats::new("")),
        products: ai_gateway_router::products::ProductUpstreams::new(),
        unusual: ai_gateway_router::unusual::UnusualBreakers::new(),
        waf_ip: ai_gateway_router::wafip::WafIpWindow::new(),
        empty_stream: ai_gateway_router::emptystream::EmptyStreamWindow::new(),
        probe_timeout: std::time::Duration::from_secs(30),
        models: Arc::new(ai_gateway_router::models_cache::ModelsCache::new()),
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

/// 发一次流式 chat 请求，回 (状态码, 响应体**原文**)。
///
/// 刻意取原文而不是解析 JSON：这条链路的缺陷恰恰是「SSE 帧被逐字透传」，
/// 解析成 JSON 就看不见「客户端实际读到了哪些字节」。
async fn stream_roundtrip(state: Arc<AppState>, body: &str) -> (StatusCode, String) {
    let app = router(state);
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

fn chat_body() -> String {
    r#"{"model":"deepseek-v4.1-flash","stream":true,"messages":[{"role":"user","content":"hi"}]}"#
        .to_string()
}

/// 等待上游收满 want 个请求（本地回环，给一小段余量）。
async fn wait_calls(seen: &Arc<Mutex<Vec<Vec<u8>>>>, want: usize) {
    for _ in 0..200 {
        if seen.lock().unwrap().len() >= want {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("等待上游第 {want} 个请求超时（实收 {}）", seen.lock().unwrap().len());
}

/// 【现场回归】上游以失败语义的 finish_reason 收尾 ⇒ 换掉原文、发可重试 error、
/// 并按上游故障喂一次熔断。
#[tokio::test]
async fn failure_finish_reason_becomes_a_retryable_error() {
    let (state, seen) = state_with(FAILURE_BODY, 1, 1).await;

    let (status, body) = stream_roundtrip(Arc::clone(&state), &chat_body()).await;
    // 上游已经回了 200 并吐了内容，网关无法改状态码（这正是必须靠帧内 error 的原因）。
    assert_eq!(status, StatusCode::OK, "body={body}");
    wait_calls(&seen, 1).await;

    // 1. 上游原文绝不透传：透传 = 客户端 pi-ai 的 mapStopReason 默认分支
    //    ⇒ Provider finish_reason: error ⇒ PI_AI_ERROR ⇒ 不重试。
    assert!(
        !body.contains("\"finish_reason\":\"error\""),
        "失败语义的 finish_reason 不得到达客户端: {body}"
    );

    // 2. 换成一帧可读、**可重试**的 error：措辞必须能被客户端归成 TRANSPORT。
    assert!(
        body.to_lowercase().contains("stream ended before"),
        "必须给出可重试的措辞（客户端据此自动换账号）: {body}"
    );
    assert!(
        body.contains("upstream_stream_truncated"),
        "错误码要能一眼看出是网关判定的上游截断: {body}"
    );

    // 3. 帧序硬契约：error 帧必须排在 [DONE] 之前，且整条流只有一个 [DONE]
    //    （两个 [DONE] 时客户端读到第一个就把这轮标成「正常完成」）。
    let done_at = body.find("data: [DONE]").expect("必须有终止帧");
    let err_at = body.find("upstream_stream_truncated").expect("必须有 error 帧");
    assert!(err_at < done_at, "error 帧必须排在 [DONE] 之前: {body}");
    assert_eq!(body.matches("data: [DONE]").count(), 1, "{body}");

    // 4. 该账号按「上游故障」记了一次账（与「流中 error 帧」同一口径），
    //    但**不被禁用** —— 900s 截断是上游整体行为，罚死账号等于自断服务。
    let st = state.pool.lock().unwrap().status_by_uid("u0").unwrap();
    assert_eq!(st.err_total, 1, "必须喂一次 note_error（否则熔断永远看不到上游故障）");
    assert!(!st.disabled, "单次上游故障不得禁用账号");
}

/// 【零误伤对照】正常的 finish_reason:"stop" 流照旧透传，账号不挨罚。
#[tokio::test]
async fn normal_stop_stream_passes_untouched() {
    let (state, seen) = state_with(OK_BODY, 1, 1).await;

    let (status, body) = stream_roundtrip(Arc::clone(&state), &chat_body()).await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    wait_calls(&seen, 1).await;

    assert!(body.contains("hi"), "内容必须透传: {body}");
    assert!(
        body.contains("\"finish_reason\":\"stop\""),
        "正常收尾的 finish_reason 必须原样透传: {body}"
    );
    assert!(
        !body.contains("upstream_stream_truncated"),
        "正常流不得被补 error 帧: {body}"
    );
    let st = state.pool.lock().unwrap().status_by_uid("u0").unwrap();
    assert_eq!(st.err_total, 0, "正常收尾不得喂 note_error");
    assert!(!st.disabled);
}
