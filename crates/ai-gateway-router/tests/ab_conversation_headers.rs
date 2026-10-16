//! A/B 对照：会话头族真的**发到了线上**。
//!
//! # 为什么必须有这一层（单元测试证明不了的事）
//!
//! `upstream::headers` 的单测只看 `HeaderMap` —— 它证明「构造器把键放进去了」，
//! **证明不了「HTTP 往返之后上游确实收到了」**。两者之间还隔着
//! reqwest 的 header 组装、以及 `Client::chat_stream` 是否真的调用了它。
//!
//! 本仓库对此有明确的血泪约定（见 `ab_devicetoken.rs` 文件头）：
//! 「这条链路上出问题的地方恰恰是 HTTP 往返本身……用 mock 掉 transport 就
//! 测不到它们，而历史上 CaptchaParam 被赋值 0 次那类缺陷正是
//! 『单测全绿但链路不通』」。
//!
//! 故这里起**真** axum 服务、发**真**请求、读它**实际收到**的头。
//!
//! # 守的是什么
//!
//! 上游渠道校验看不到会话 id 时回：
//!
//! ```text
//! HTTP 400 {"code":11-128,"msg":"conversation id missing"}
//! ```
//!
//! 即「这次调用没通过渠道校验，可能不是官方客户端发出的」。
//!
//! 端口一律 `127.0.0.1:0` 由内核分配，避免与用户机器上任何服务撞端口。

use std::sync::{Arc, Mutex};

use ai_gateway_router::auth::Auth;
use ai_gateway_router::upstream::client::{ChatOutcome, Client, ClientConfig};

/// 记录一次 chat 请求**实际收到**的会话头。
#[derive(Clone, Debug, Default)]
struct Seen {
    conversation_id: Option<String>,
    conversation_request_id: Option<String>,
    conversation_message_id: Option<String>,
    request_id: Option<String>,
    user_agent: Option<String>,
    ide_type: Option<String>,
}

/// 起假上游，返回 (base_url, 记录列表)。
async fn spawn_router() -> (String, Arc<Mutex<Vec<Seen>>>) {
    use axum::{routing::post, Router};

    let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);

    let handler = move |headers: axum::http::HeaderMap| {
        let sink = Arc::clone(&sink);
        async move {
            let g = |k: &str| {
                headers
                    .get(k)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string())
            };
            sink.lock().unwrap().push(Seen {
                conversation_id: g("x-conversation-id"),
                conversation_request_id: g("x-conversation-request-id"),
                conversation_message_id: g("x-conversation-message-id"),
                request_id: g("x-request-id"),
                user_agent: g("user-agent"),
                ide_type: g("x-ide-type"),
            });
            (
                [("content-type", "text/event-stream")],
                "data: [DONE]\n\n",
            )
        }
    };

    let app = Router::new().route("/*rest", post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://127.0.0.1:{}", addr.port()), seen)
}

fn cn_account() -> Auth {
    Auth {
        uid: "u-cn".into(),
        access_token: "at-cn".into(),
        domain: "https://copilot.tencent.com".into(),
        ..Default::default()
    }
}

fn intl_account() -> Auth {
    Auth {
        uid: "u-intl".into(),
        access_token: "at-intl".into(),
        domain: "https://www.workbuddy.ai".into(),
        ..Default::default()
    }
}

/// ⚠ **两个区域的基址都必须指向假上游**。
///
/// 只设 `base_intl` 时，国服请求会**静默回落到真实的** `CHAT_BASE_CN`
/// （`https://copilot.tencent.com`）—— 那不是「多打了一次上游」，而是
/// 从测试进程里向真实域名发了一次带账号头的请求。
/// 本测试首次运行时确实漏设了，故在此钉死这个约束。
fn client_for(chat_url: String) -> Client {
    Client::new(ClientConfig {
        base_intl: chat_url.clone(),
        chat_base_cn: chat_url,
        ..Default::default()
    })
    .unwrap()
}

/// 国服：会话头族必须真的到线上。
#[tokio::test]
async fn cn_conversation_headers_reach_the_wire() {
    let (url, seen) = spawn_router().await;
    let c = client_for(url);
    let out = c
        .chat_stream(&cn_account(), br#"{"model":"m"}"#, "sess-1")
        .await
        .unwrap();
    assert!(matches!(out, ChatOutcome::Stream(_)), "应为 2xx 流");

    let got = seen.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "应恰好发出一次请求");
    let s = &got[0];
    assert!(
        s.conversation_id.as_deref().is_some_and(|v| !v.is_empty()),
        "国服请求**线上**必须带 X-Conversation-ID（缺了上游回 400/11-128），实际 {:?}",
        s
    );
    assert!(s.conversation_request_id.is_some(), "缺 X-Conversation-Request-ID");
    assert!(s.conversation_message_id.is_some(), "缺 X-Conversation-Message-ID");
    assert!(s.request_id.is_some(), "缺 X-Request-ID");
    assert_eq!(
        s.conversation_message_id, s.request_id,
        "Message-ID 与 Request-ID 应为同一个值"
    );
}

/// 国际版：会话头族同样必须到线上，且 UA / 身份头是国际版那套。
#[tokio::test]
async fn intl_conversation_and_identity_headers_reach_the_wire() {
    let (url, seen) = spawn_router().await;
    let c = client_for(url);
    let out = c
        .chat_stream(&intl_account(), br#"{"model":"m"}"#, "sess-9")
        .await
        .unwrap();
    assert!(matches!(out, ChatOutcome::Stream(_)), "应为 2xx 流");

    let got = seen.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    let s = &got[0];
    assert!(
        s.conversation_id.as_deref().is_some_and(|v| !v.is_empty()),
        "国际版线上必须带 X-Conversation-ID，实际 {:?}",
        s
    );
    assert!(s.conversation_request_id.is_some());
    assert!(s.conversation_message_id.is_some());
    assert_eq!(s.conversation_message_id, s.request_id);
    // 身份头：国际版必须是 WorkBuddy 客户端那套。
    assert_eq!(s.ide_type.as_deref(), Some("WorkBuddy"));
    assert!(
        s.user_agent.as_deref().is_some_and(|u| u.starts_with("WorkBuddy/")),
        "国际版 UA 应以 WorkBuddy/ 开头（否则 403 11140），实际 {:?}",
        s.user_agent
    );
}

/// 同一会话种子、连发两次 ⇒ 线上看到的会话 id 必须**相同**。
///
/// 这是「多轮对话是同一个会话」的可观测判据。
#[tokio::test]
async fn conversation_id_is_stable_across_requests_on_the_wire() {
    let (url, seen) = spawn_router().await;
    let c = client_for(url);
    for _ in 0..2 {
        c.chat_stream(&cn_account(), br#"{"model":"m"}"#, "sess-same")
            .await
            .unwrap();
    }
    let ids: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .filter_map(|s| s.conversation_id.clone())
        .collect();
    assert_eq!(ids.len(), 2);
    assert_eq!(
        ids[0], ids[1],
        "同一会话的两轮请求必须复用同一个 X-Conversation-ID"
    );
    // 换会话种子则必须不同。
    c.chat_stream(&cn_account(), br#"{"model":"m"}"#, "sess-other")
        .await
        .unwrap();
    let all: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .filter_map(|s| s.conversation_id.clone())
        .collect();
    assert_ne!(all[0], all[2], "不同会话必须得到不同 id");
}
