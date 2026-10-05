//! A/B 对照：`X-Device-Token` 的注入与降级。
//!
//! 对应 Go `internal/upstream/devicetoken_test.go`（含 8 条用例）。
//! 这里锁住三条最关键的语义：
//!
//! 1. 配了宿主服务时，**国际版**请求必须带上该头
//! 2. **国服不发**该头（抓包证据只覆盖国际版，不该凭推测外推）
//! 3. 宿主服务失败/未配置时**静默降级** —— 请求照常发出，不带该头
//!
//! # 为什么要起真服务而不是 mock
//!
//! 这条链路上出问题的地方恰恰是「HTTP 往返」本身：头名大小写、串行锁、
//! 超时、错误码判定。用 mock 掉 transport 就测不到它们，而历史上
//! `CaptchaParam` 被赋值 0 次那类缺陷正是「单测全绿但链路不通」。
//!
//! 端口一律 `127.0.0.1:0` 由内核分配，避免与用户机器上任何服务撞端口。

use std::sync::{Arc, Mutex};

use ai_gateway_router::auth::Auth;
use ai_gateway_router::upstream::client::{ChatOutcome, Client, ClientConfig};

/// 起一个假的上游/服务，返回 (base_url, 记录到的头)。
///
/// `record` 收集每次请求收到的 `x-device-token`（None = 没带）。
///
/// ★ 同时注册 `/` 与 `/*rest` 两条路由：设备 token 服务是 POST 到**裸 URL**
/// （宿主服务监听在 `http://127.0.0.1:PORT`），而 chat 请求会拼上
/// `/v2/chat/completions`。只注册后者会让前者 404。
async fn spawn_recorder(
    body: &'static str,
) -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    use axum::{routing::post, Router};

    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);

    let handler = move |headers: axum::http::HeaderMap| {
        let sink = Arc::clone(&sink);
        async move {
            let tok = headers
                .get("x-device-token")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            sink.lock().unwrap().push(tok);
            ([("content-type", "text/event-stream")], body)
        }
    };

    let app = Router::new()
        .route("/", post(handler.clone()))
        .route("/device-token", post(handler.clone()))
        .route("/*rest", post(handler));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (
        // ★ 与宿主 `device_token_server.rs:475` 逐字同形：
        // `http://127.0.0.1:{port}/device-token`（不是裸地址）。
        format!("http://127.0.0.1:{}/device-token", addr.port()),
        seen,
    )
}

fn intl_account() -> Auth {
    Auth {
        uid: "u-intl".into(),
        access_token: "at-intl".into(),
        domain: "www.workbuddy.ai".into(),
        ..Default::default()
    }
}

fn cn_account() -> Auth {
    Auth {
        uid: "u-cn".into(),
        access_token: "at-cn".into(),
        domain: "www.workbuddy.cn".into(),
        ..Default::default()
    }
}

/// 国际版请求必须带上宿主给的设备 token。
#[tokio::test]
async fn device_token_attached_for_intl() {
    let (svc_url, svc_seen) =
        spawn_recorder(r#"{"ok":true,"tokens":["v3:FAKE-TOKEN-1234"]}"#).await;
    let (chat_url, chat_seen) = spawn_recorder("data: [DONE]\n\n").await;

    let client = Client::new(ClientConfig {
        device_token_url: svc_url,
        device_token_token: "svc-token".into(),
        base_intl: chat_url,
        ..Default::default()
    })
    .unwrap();

    let out = client
        .chat_stream(&intl_account(), br#"{"model":"m"}"#)
        .await
        .unwrap();
    assert!(matches!(out, ChatOutcome::Stream(_)), "应为 2xx 流");

    assert_eq!(svc_seen.lock().unwrap().len(), 1, "应调用一次宿主服务");
    let got = chat_seen.lock().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(
        got[0].as_deref(),
        Some("v3:FAKE-TOKEN-1234"),
        "国际版请求必须带 X-Device-Token"
    );
}

/// 国服**不发**该头：抓包证据只覆盖国际版。
#[tokio::test]
async fn device_token_not_attached_for_cn() {
    let (svc_url, svc_seen) =
        spawn_recorder(r#"{"ok":true,"tokens":["v3:FAKE-TOKEN-1234"]}"#).await;
    let (chat_url, chat_seen) = spawn_recorder("data: [DONE]\n\n").await;

    let client = Client::new(ClientConfig {
        device_token_url: svc_url,
        device_token_token: "svc-token".into(),
        chat_base_cn: chat_url,
        ..Default::default()
    })
    .unwrap();

    let out = client
        .chat_stream(&cn_account(), br#"{"model":"m"}"#)
        .await
        .unwrap();
    assert!(matches!(out, ChatOutcome::Stream(_)));

    assert!(
        svc_seen.lock().unwrap().is_empty(),
        "国服请求根本不该去问宿主服务（白等一次往返）"
    );
    let got = chat_seen.lock().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0], None, "国服不发 X-Device-Token");
}

/// ★ 最关键的一条：宿主服务失败时**不发该头**，但请求照常发出。
///
/// 若这里改成「拿不到就拒绝请求」，就等于把一个可选增强变成新的单点故障：
/// 宿主服务没起来 / node 缺失 / 用户没装官方客户端，都会让所有国际版对话不可用。
#[tokio::test]
async fn device_token_degrades_when_service_fails() {
    // 服务返回 ok:false（模拟「找不到 Node / SDK」）
    let (svc_url, _) = spawn_recorder(r#"{"ok":false,"error":"node 未返回 token"}"#).await;
    let (chat_url, chat_seen) = spawn_recorder("data: [DONE]\n\n").await;

    let client = Client::new(ClientConfig {
        device_token_url: svc_url,
        base_intl: chat_url,
        ..Default::default()
    })
    .unwrap();

    let out = client
        .chat_stream(&intl_account(), br#"{"model":"m"}"#)
        .await
        .unwrap();
    assert!(
        matches!(out, ChatOutcome::Stream(_)),
        "拿不到 token 不能影响请求本身"
    );
    assert_eq!(
        chat_seen.lock().unwrap()[0],
        None,
        "降级路径下不发该头"
    );
}

/// 服务**连不上**时同样降级（端口 1 上不会有服务）。
#[tokio::test]
async fn device_token_degrades_when_service_unreachable() {
    let (chat_url, chat_seen) = spawn_recorder("data: [DONE]\n\n").await;

    let client = Client::new(ClientConfig {
        device_token_url: "http://127.0.0.1:1/device-token".into(),
        base_intl: chat_url,
        ..Default::default()
    })
    .unwrap();

    let out = client
        .chat_stream(&intl_account(), br#"{"model":"m"}"#)
        .await
        .unwrap();
    assert!(matches!(out, ChatOutcome::Stream(_)));
    assert_eq!(chat_seen.lock().unwrap()[0], None);
}

/// 形态不符（不带 `v3:` 前缀）当作失败 —— 宁可漏发，也不发上游不认的怪值。
#[tokio::test]
async fn device_token_rejects_malformed_shape() {
    let (svc_url, _) = spawn_recorder(r#"{"ok":true,"tokens":["not-a-v3-token"]}"#).await;
    let (chat_url, chat_seen) = spawn_recorder("data: [DONE]\n\n").await;

    let client = Client::new(ClientConfig {
        device_token_url: svc_url,
        base_intl: chat_url,
        ..Default::default()
    })
    .unwrap();

    let _ = client
        .chat_stream(&intl_account(), br#"{"model":"m"}"#)
        .await
        .unwrap();
    assert_eq!(
        chat_seen.lock().unwrap()[0],
        None,
        "形态异常的头不该发出去"
    );
}

/// 未配置服务时，行为与本特性引入前**逐字相同**：不调用、不带头。
#[tokio::test]
async fn device_token_not_configured_is_noop() {
    let (chat_url, chat_seen) = spawn_recorder("data: [DONE]\n\n").await;

    let client = Client::new(ClientConfig {
        // device_token_url 留空 = 不启用
        base_intl: chat_url,
        ..Default::default()
    })
    .unwrap();

    let _ = client
        .chat_stream(&intl_account(), br#"{"model":"m"}"#)
        .await
        .unwrap();
    assert_eq!(chat_seen.lock().unwrap()[0], None);
}

/// ★ 串行化：并发请求必须排队（宿主侧 Node 子进程是进程级单例）。
///
/// 若不串行，`Turing SDK channelId cannot change after initialization`
/// 会让并发请求互相干扰。这里用「服务端记录同时在处理的数量」来验证。
#[tokio::test]
async fn device_token_fetch_is_serialized() {
    use axum::{routing::post, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
    static MAX_SEEN: AtomicUsize = AtomicUsize::new(0);
    // 清零：静态量在同一个测试进程内跨运行保留。
    MAX_SEEN.store(0, Ordering::SeqCst);

    let app = Router::new().route(
        "/device-token",
        post(|| async {
            let cur = IN_FLIGHT.fetch_add(1, Ordering::SeqCst) + 1;
            MAX_SEEN.fetch_max(cur, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
            (
                [("content-type", "application/json")],
                r#"{"ok":true,"tokens":["v3:SERIALIZED"]}"#,
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let (chat_url, _) = spawn_recorder("data: [DONE]\n\n").await;
    let client = Client::new(ClientConfig {
        device_token_url: format!("http://127.0.0.1:{}/device-token", addr.port()),
        base_intl: chat_url,
        ..Default::default()
    })
    .unwrap();

    // 并发 4 个国际版请求
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let c = client.clone();
        tasks.push(tokio::spawn(async move {
            let _ = c.chat_stream(&intl_account(), br#"{"model":"m"}"#).await;
        }));
    }
    for t in tasks {
        let _ = t.await;
    }

    assert_eq!(
        MAX_SEEN.load(Ordering::SeqCst),
        1,
        "宿主服务端同时只能看到 1 个请求（provider 的锁必须跨 Client 克隆共享）"
    );
}
