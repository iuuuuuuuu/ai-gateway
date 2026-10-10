//! A/B 对照：**上游整体空流风暴**的熔断与恢复（2026-10-11 现场）。
//!
//! 现场：国际区 `deepseek-v4.1-flash-sg` 整体空流（HTTP 200 + 无有效数据帧），
//! DSH 客户端 5 路并发重试 × 网关全轮转，把 13 个国际区账号全部喂进冷却/熔断，
//! 会话以 503 `no_healthy_account` 死亡 —— 而账号本身都是好的。
//!
//! 本回归钉住三件事：
//! 1. 多个**不同**账号在窗口内先后空流 ⇒ 轮转在凑满阈值后的下一次选号时
//!    **中止**（熔断期内一个请求都不发），报 `upstream_stream_stalled`
//!    （绝不落 `no_healthy_account` —— 那个码会诱导客户端立刻 5 路并发重试，
//!    正是现场的放大器）；
//! 2. 孤立的一次空流（单账号）保持旧语义：按 503 上游故障冷却换号，最终仍报
//!    `no_healthy_account` —— 阈值语义不能误伤单号故障；
//! 3. 健康流零误伤：正常 SSE 照常透传，空流窗口不计数。
//!
//! 为什么起真服务：这条链路的问题出在「HTTP 往返 + SSE 首帧探测」本身，
//! mock 掉 transport 就测不到（见 `ab_prompt.rs` 文件头同款论证）。

use std::sync::{Arc, Mutex};

use ai_gateway_router::auth::Auth;
use ai_gateway_router::pool::Pool;
use ai_gateway_router::server::{router, AppState};
use ai_gateway_router::upstream::client::{Client, ClientConfig};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

/// 假上游的响应体（两套：空流现场 / 健康流对照）。
const EMPTY_BODY: &str = "";
const OK_BODY: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";

/// 起一个假上游：记录每次收到的请求体；`empty`=true 回**完全空**的 SSE（现场复刻），
/// 否则回最小合法 SSE。
///
/// content-length 0 的 event-stream ⇒ `probe_first_frame` 立刻收到流结束
/// （"upstream closed before first frame"）⇒ 判「上游返回空流」。
async fn spawn_upstream(empty: bool) -> (String, Arc<Mutex<Vec<Vec<u8>>>>) {
    use axum::routing::post;

    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);

    let handler = move |body: axum::body::Bytes| {
        let sink = Arc::clone(&sink);
        async move {
            sink.lock().unwrap().push(body.to_vec());
            let ct = [("content-type", "text/event-stream")];
            if empty {
                (ct, EMPTY_BODY)
            } else {
                (ct, OK_BODY)
            }
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
/// 组装一个指向假上游、带 `n` 个国际区账号的 AppState。
async fn state_with(empty: bool, n: usize, max_rotate: usize) -> (Arc<AppState>, Arc<Mutex<Vec<Vec<u8>>>>) {
    let (base, seen) = spawn_upstream(empty).await;
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

/// 发一次 chat 请求并回 (状态码, 响应 JSON)。
async fn roundtrip(state: Arc<AppState>, body: &str) -> (StatusCode, Value) {
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
    let json = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    (status, json)
}

fn chat_body() -> String {
    r#"{"model":"deepseek-v4.1-flash","stream":true,"messages":[{"role":"user","content":"hi"}]}"#.to_string()
}

/// 等待上游收满 `want` 个请求（本地回环，给一小段余量）。
async fn wait_calls(seen: &Arc<Mutex<Vec<Vec<u8>>>>, want: usize) {
    for _ in 0..200 {
        if seen.lock().unwrap().len() >= want {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("等待上游第 {want} 个请求超时（实收 {}）", seen.lock().unwrap().len());
}
/// 【现场回归】多账号先后空流 ⇒ 轮转中止、报 `upstream_stream_stalled`、不罚死账号。
#[tokio::test]
async fn empty_stream_burst_aborts_rotation_and_names_the_real_cause() {
    // 4 个账号、允许轮 6 次：若没有风暴熔断，4 个账号会被全部送进冷却。
    let (state, seen) = state_with(true, 4, 6).await;

    let (status, body) = roundtrip(Arc::clone(&state), &chat_body()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body={body}");
    let err = body.get("error").expect("应为 OpenAI 错误壳");
    assert_eq!(err["code"], "upstream_stream_stalled", "err={err}");
    let msg = err["message"].as_str().unwrap_or("");
    assert!(msg.contains("空流"), "message 应点破空流现象: {msg}");
    assert!(msg.contains("换号无用"), "message 应说明换号无用: {msg}");

    // 上游只该挨 3 次打（= 阈值 3）：第 4 次选号时已判定风暴、**不发请求**。
    wait_calls(&seen, 3).await;
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(seen.lock().unwrap().len(), 3, "风暴判定后不得再打上游");

    // 账号没有被误杀：不该有账号被禁用（它们只是遇上了上游整体故障）。
    for i in 0..4 {
        let uid = format!("u{i}");
        let st = state.pool.lock().unwrap().status_by_uid(&uid).unwrap();
        assert!(!st.disabled, "账号 {uid} 不应被禁用");
    }

    // 立刻再发第二发：仍被风暴熔断拦住，上游请求量**不再增长**。
    let (status2, body2) = roundtrip(Arc::clone(&state), &chat_body()).await;
    assert_eq!(status2, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body2["error"]["code"], "upstream_stream_stalled", "body2={body2}");
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(seen.lock().unwrap().len(), 3, "熔断期内一个请求都不发");
}

/// 【旧语义钉住】孤立单号空流不触发风暴：按 503 上游故障冷却换号，
/// 无号可换时仍报旧的 `no_healthy_account`。
#[tokio::test]
async fn single_empty_stream_keeps_legacy_no_healthy_account() {
    let (state, seen) = state_with(true, 1, 1).await;

    let (status, body) = roundtrip(Arc::clone(&state), &chat_body()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body={body}");
    assert_eq!(body["error"]["code"], "no_healthy_account", "body={body}");

    wait_calls(&seen, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(seen.lock().unwrap().len(), 1);
}

/// 【零误伤对照】健康 SSE 照常透传；空流窗口不计数、不发难。
#[tokio::test]
async fn healthy_stream_passes_without_false_trip() {
    let (state, seen) = state_with(false, 4, 6).await;

    let (status, body) = roundtrip(Arc::clone(&state), &chat_body()).await;
    assert_eq!(status, StatusCode::OK, "body={body}");

    wait_calls(&seen, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(seen.lock().unwrap().len(), 1, "健康流只打一次上游");

    // 且没有账号被误杀。
    for i in 0..4 {
        let uid = format!("u{i}");
        let st = state.pool.lock().unwrap().status_by_uid(&uid).unwrap();
        assert!(!st.disabled, "账号 {uid} 不应被禁用");
    }
}