//! A/B 对照：系统提示词注入（`prompt.mode`）的**出站边界**回归。
//!
//! 对应 Go `internal/server/prompt_rewrite_test.go`（305 行 / 11 个用例）。
//!
//! # 为什么断言的是「上游真正收到的 wire body」
//!
//! 只有出站字节才能证明「客户端 system 指纹确实没发出去」。断言 handler 内部的
//! 中间变量会漏掉两类真实缺陷：① 改写结果没被真正用在出站请求上；② 后续环节
//! （模型改写、协议转换）又把改好的 body 覆盖回原样。
//!
//! # 为什么起真服务而不是 mock transport
//!
//! 这条链路上出问题的地方是「HTTP 往返 + 请求体序列化」本身。用 mock 掉 transport
//! 就测不到它们，而组 A 的 `CaptchaParam` 被赋值 0 次那类缺陷正是「单测全绿但
//! 链路不通」。端口一律 `127.0.0.1:0` 由内核分配，避免撞用户机器上的任何服务。

use std::sync::{Arc, Mutex};

use ai_gateway_router::auth::Auth;
use ai_gateway_router::pool::Pool;
use ai_gateway_router::server::{router, AppState};
use ai_gateway_router::upstream::client::{Client, ClientConfig};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

/// 客户端 system 里的指纹句（Go 侧 `claudeFingerprint` 同款）。
const CLIENT_FINGERPRINT: &str = "You are Claude Code, Anthropic's official CLI tool for Claude.";
/// 网关自有提示词（断言它必须出现在 wire 上）。
const GATEWAY_SYS: &str = "GATEWAY-SYS";

/// 起一个假上游：记录每次收到的请求体，回一段最小合法 SSE。
///
/// ★ 注册 `/` 与 `/*rest` 两条路由：chat 请求会拼上 `/v2/chat/completions`，
/// 而 `/*rest` 的 catch-all 只能放在**最后**一段（axum 0.7 的 matchit 限制）。
async fn spawn_upstream() -> (String, Arc<Mutex<Vec<Vec<u8>>>>) {
    use axum::routing::post;

    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);

    let handler = move |body: axum::body::Bytes| {
        let sink = Arc::clone(&sink);
        async move {
            sink.lock().unwrap().push(body.to_vec());
            (
                [("content-type", "text/event-stream")],
                "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
            )
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

/// 单账号（国服，`MaxRotate:1` 口径 —— 一次请求只试一个号）。
///
/// ★ `expires_at` 必须给远期值：`Auth::needs_refresh` 把 `expires_at <= 0` 视为
/// 「需要刷新」，而刷新要走真实域名（测试里没有那个上游）⇒ 请求会以「刷新失败」
/// 结束，测不到提示词改写。Go 侧 `newPromptHandler` 同样写 `ExpiresAt: 9999999999`。
fn account() -> Auth {
    Auth {
        uid: "u1".into(),
        access_token: "at1".into(),
        expires_at: 9999999999,
        domain: "www.workbuddy.cn".into(),
        ..Default::default()
    }
}

/// 组装一个指向假上游的 AppState。
async fn state_with(mode: &str, text: &str) -> (Arc<AppState>, Arc<Mutex<Vec<Vec<u8>>>>) {
    let (base, seen) = spawn_upstream().await;
    let mut pool = Pool::new(String::new());
    pool.add(account());

    let state = Arc::new(AppState {
        pool: Arc::new(Mutex::new(pool)),
        client: Client::new(ClientConfig {
            chat_base_cn: base,
            ..Default::default()
        })
        .expect("构造测试客户端"),
        session: None,
        api_key: String::new(),
        max_rotate: 1,
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
        // ★ 提示词与模型清单都从 `DynamicState` 读（配置热重载的落点）：这里直接给
        // 初值，等价于「网关刚启动、还没热重载过」。
        dynamic: ai_gateway_router::cfgwatch::DynamicState::new(
            ai_gateway_router::cfgwatch::DynamicConfig {
                prompt_mode: mode.to_string(),
                prompt_text: text.to_string(),
                ..Default::default()
            },
        ),
        auth_dir: String::new(),
        // 本对照只关心「同一请求的出站字节」：后台养号任务会在对照期间自己打上游，
        // 把观测到的请求序列搅浑 ⇒ 不接执行体。
        scheduler: None,
        // 同上：成长任务执行体也会自己打上游，一并留空。
        growtask: None,
    });
    (state, seen)
}

/// 便捷：发请求并回传上游 body。
async fn roundtrip(
    state: Arc<AppState>,
    seen: &Arc<Mutex<Vec<Vec<u8>>>>,
    path: &str,
    body: &str,
) -> (StatusCode, Vec<u8>) {
    let app = router(state);
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    // 等上游那次请求落进 recorder（本地回环，给一小段余量）。
    for _ in 0..100 {
        if !seen.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let wire = seen.lock().unwrap().first().cloned().unwrap_or_default();
    (status, wire)
}

fn messages_of(wire: &[u8]) -> Vec<Value> {
    let v: Value = serde_json::from_slice(wire)
        .unwrap_or_else(|e| panic!("上游收到的不是合法 JSON ({e}): {}", String::from_utf8_lossy(wire)));
    v["messages"].as_array().cloned().unwrap_or_default()
}

fn roles_of(msgs: &[Value]) -> Vec<String> {
    msgs.iter()
        .map(|m| m["role"].as_str().unwrap_or("").to_string())
        .collect()
}

// ─────────────────────────────────────────────────────────────────────
// custom：整体替换
// ─────────────────────────────────────────────────────────────────────

/// custom 模式下 wire 上不得残留客户端指纹，且首条为自有 system。
#[tokio::test]
async fn custom_replaces_system_on_wire() {
    let (state, seen) =
        state_with(ai_gateway_router::prompt::MODE_CUSTOM, GATEWAY_SYS).await;
    let body = format!(
        r#"{{"model":"m","messages":[{{"role":"system","content":{CLIENT_FINGERPRINT:?}}},{{"role":"user","content":"你好"}}]}}"#
    );
    let (status, wire) = roundtrip(state, &seen, "/v1/chat/completions", &body).await;
    assert_eq!(status, StatusCode::OK);

    let s = String::from_utf8_lossy(&wire);
    assert!(
        !s.contains("Anthropic"),
        "客户端指纹泄漏到上游了：{s}"
    );
    let msgs = messages_of(&wire);
    assert_eq!(roles_of(&msgs), vec!["system", "user"]);
    assert_eq!(msgs[0]["content"], GATEWAY_SYS);
    assert_eq!(msgs[1]["content"], "你好", "user 内容不该被改动");
}

/// developer 角色同样被替换（`isSystemLike` 认 system + developer）。
#[tokio::test]
async fn custom_drops_developer_role_on_wire() {
    let (state, seen) =
        state_with(ai_gateway_router::prompt::MODE_CUSTOM, GATEWAY_SYS).await;
    let body = r#"{"model":"m","messages":[{"role":"developer","content":"DEVELOPER-FINGERPRINT"},{"role":"user","content":"hi"}]}"#;
    let (status, wire) = roundtrip(state, &seen, "/v1/chat/completions", body).await;
    assert_eq!(status, StatusCode::OK);

    let s = String::from_utf8_lossy(&wire);
    assert!(!s.contains("DEVELOPER-FINGERPRINT"), "developer 指纹残留：{s}");
    assert_eq!(roles_of(&messages_of(&wire)), vec!["system", "user"]);
}

// ─────────────────────────────────────────────────────────────────────
// passthrough：逐字不变（本特性的核心保证）
// ─────────────────────────────────────────────────────────────────────

/// passthrough 下客户端 system 必须**逐字**保留。
#[tokio::test]
async fn passthrough_leaves_system_untouched() {
    let (state, seen) =
        state_with(ai_gateway_router::prompt::MODE_PASSTHROUGH, "").await;
    let body = format!(
        r#"{{"model":"m","messages":[{{"role":"system","content":{CLIENT_FINGERPRINT:?}}},{{"role":"user","content":"hi"}}]}}"#
    );
    let (status, wire) = roundtrip(state, &seen, "/v1/chat/completions", &body).await;
    assert_eq!(status, StatusCode::OK);

    let s = String::from_utf8_lossy(&wire);
    assert!(s.contains("Anthropic"), "passthrough 下指纹必须原样发出：{s}");
    assert_eq!(roles_of(&messages_of(&wire)), vec!["system", "user"]);
}

/// ★ 核心保证：passthrough **即使配了文本也不替换**。
///
/// 「mode 才是开关，PromptText 非空不构成替换理由」—— 这是「缺省不改变既有行为」
/// 的落点。Go `prompt_rewrite_test.go:158-161` 逐字同口径。
#[tokio::test]
async fn passthrough_ignores_prompt_text() {
    let (state, seen) =
        state_with(ai_gateway_router::prompt::MODE_PASSTHROUGH, "SHOULD-NOT-APPEAR").await;
    let body = r#"{"model":"m","messages":[{"role":"system","content":"ORIGINAL"},{"role":"user","content":"hi"}]}"#;
    let (status, wire) = roundtrip(state, &seen, "/v1/chat/completions", body).await;
    assert_eq!(status, StatusCode::OK);

    let s = String::from_utf8_lossy(&wire);
    assert!(!s.contains("SHOULD-NOT-APPEAR"), "passthrough 下不得注入：{s}");
    assert!(s.contains("ORIGINAL"), "客户端 system 必须保留：{s}");
}

/// 模式是空串（调用方忘记传）时同样是透传。
///
/// Go `prompt_rewrite_test.go:131` 逐字「故意不传 PromptMode / PromptText：验证
/// 「调用方忘记传」时也是透传」。Rust 侧 `normalize` 会把空串归一成 passthrough，
/// 但即便有人绕过 normalize 直接构造 AppState，`run_forward` 也必须不改写。
#[tokio::test]
async fn empty_mode_is_passthrough() {
    let (state, seen) = state_with("", "SHOULD-NOT-APPEAR").await;
    let body = r#"{"model":"m","messages":[{"role":"system","content":"ORIGINAL"},{"role":"user","content":"hi"}]}"#;
    let (status, wire) = roundtrip(state, &seen, "/v1/chat/completions", body).await;
    assert_eq!(status, StatusCode::OK);
    let s = String::from_utf8_lossy(&wire);
    assert!(!s.contains("SHOULD-NOT-APPEAR"), "空模式不得注入：{s}");
    assert!(s.contains("ORIGINAL"));
}

// ─────────────────────────────────────────────────────────────────────
// append：客户端规则在前、网关提示词在后
// ─────────────────────────────────────────────────────────────────────

/// append 下两条 system 共存，且网关提示词紧跟客户端那条之后。
#[tokio::test]
async fn append_keeps_client_system_on_wire() {
    let (state, seen) =
        state_with(ai_gateway_router::prompt::MODE_APPEND, GATEWAY_SYS).await;
    let body = r#"{"model":"m","messages":[{"role":"system","content":"PROJECT-RULES"},{"role":"user","content":"hi"}]}"#;
    let (status, wire) = roundtrip(state, &seen, "/v1/chat/completions", body).await;
    assert_eq!(status, StatusCode::OK);

    let msgs = messages_of(&wire);
    assert_eq!(roles_of(&msgs), vec!["system", "system", "user"]);
    assert_eq!(msgs[0]["content"], "PROJECT-RULES", "客户端规则必须保留");
    assert_eq!(msgs[1]["content"], GATEWAY_SYS, "网关提示词紧随其后");
}

// ─────────────────────────────────────────────────────────────────────
// 顺序与不丢字段
// ─────────────────────────────────────────────────────────────────────

/// ★ 顺序回归：提示词改写必须排在 model 改写**之前**，两者不能互相吃掉。
///
/// 输入带产品/区域前缀的模型名；断言 wire 上 ① 裸名生效 ② 前缀消失
/// ③ 提示词也生效 ④ 客户端指纹消失。
#[tokio::test]
async fn custom_composes_with_model_rewrite() {
    let (state, seen) =
        state_with(ai_gateway_router::prompt::MODE_CUSTOM, GATEWAY_SYS).await;
    let body = format!(
        r#"{{"model":"cn:glm-5.2","messages":[{{"role":"system","content":{CLIENT_FINGERPRINT:?}}},{{"role":"user","content":"hi"}}]}}"#
    );
    let (status, wire) = roundtrip(state, &seen, "/v1/chat/completions", &body).await;
    assert_eq!(status, StatusCode::OK);

    let s = String::from_utf8_lossy(&wire);
    assert!(s.contains("glm-5.2"), "裸名应出现在 wire 上：{s}");
    assert!(!s.contains("cn:glm-5.2"), "区域前缀应已被剥掉：{s}");
    assert!(!s.contains("Anthropic"), "提示词改写不该被 model 改写覆盖：{s}");
    assert_eq!(messages_of(&wire)[0]["content"], GATEWAY_SYS);
}

/// custom 下 model / max_tokens / tools 三项不丢（RawValue 逐字透传语义）。
#[tokio::test]
async fn custom_keeps_tools_and_model() {
    let (state, seen) =
        state_with(ai_gateway_router::prompt::MODE_CUSTOM, GATEWAY_SYS).await;
    let body = r#"{"model":"glm-5.2","max_tokens":1234,"messages":[{"role":"system","content":"old"},{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}]}"#;
    let (status, wire) = roundtrip(state, &seen, "/v1/chat/completions", body).await;
    assert_eq!(status, StatusCode::OK);

    let v: Value = serde_json::from_slice(&wire).unwrap();
    assert_eq!(v["model"], "glm-5.2");
    assert_eq!(v["max_tokens"], 1234);
    assert_eq!(v["tools"][0]["function"]["name"], "f");
}

// ─────────────────────────────────────────────────────────────────────
// 三种协议都要生效
// ─────────────────────────────────────────────────────────────────────

/// `/v1/chat/completions`、`/v1/messages`、`/v1/responses` 三条路径都要改写。
///
/// ★ 三条协议各有自己的「system 载体」形态（`messages[].role` / 顶层 `system` /
/// 顶层 `instructions`），都先汇合成 OpenAI Chat 形态再进 `run_forward` —— 一处
/// 改写覆盖全部出站路径。本用例正是这条设计前提的回归。
#[tokio::test]
async fn custom_applies_to_all_protocols() {
    let cases: [(&str, String); 3] = [
        (
            "/v1/chat/completions",
            format!(
                r#"{{"model":"m","messages":[{{"role":"system","content":{CLIENT_FINGERPRINT:?}}},{{"role":"user","content":"hi"}}]}}"#
            ),
        ),
        (
            "/v1/messages",
            format!(
                r#"{{"model":"m","max_tokens":64,"system":{CLIENT_FINGERPRINT:?},"messages":[{{"role":"user","content":"hi"}}]}}"#
            ),
        ),
        (
            "/v1/responses",
            format!(
                r#"{{"model":"m","instructions":{CLIENT_FINGERPRINT:?},"input":[{{"role":"user","content":"hi"}}]}}"#
            ),
        ),
    ];

    for (path, body) in cases {
        let (state, seen) =
            state_with(ai_gateway_router::prompt::MODE_CUSTOM, GATEWAY_SYS).await;
        let (status, wire) = roundtrip(state, &seen, path, &body).await;
        assert_eq!(status, StatusCode::OK, "{path} 应 200");

        let s = String::from_utf8_lossy(&wire);
        assert!(!s.contains("Anthropic"), "{path} 上指纹泄漏：{s}");
        let msgs = messages_of(&wire);
        assert!(!msgs.is_empty(), "{path} 上游应收到 messages");
        assert_eq!(msgs[0]["role"], "system", "{path} 首条应为 system");
        assert_eq!(msgs[0]["content"], GATEWAY_SYS, "{path} 首条内容应为自有提示词");
    }
}

// ─────────────────────────────────────────────────────────────────────
// 边界：坏 body 不阻塞请求
// ─────────────────────────────────────────────────────────────────────

/// 非法 JSON 仍要 200（改写失败 = 原样透传，不能让请求失败）。
#[tokio::test]
async fn custom_invalid_body_not_blocked() {
    let (state, seen) =
        state_with(ai_gateway_router::prompt::MODE_CUSTOM, GATEWAY_SYS).await;
    let (status, wire) = roundtrip(state, &seen, "/v1/chat/completions", "{not valid json").await;
    assert_eq!(status, StatusCode::OK, "非法 body 不该让请求失败");
    assert_eq!(
        String::from_utf8_lossy(&wire),
        "{not valid json",
        "改写失败时应逐字透传原始 body"
    );
}

/// 带会话键（`metadata.conversation_id`）仍要 200 —— 防止将来把改写点前移到
/// 会话键提取之前。
#[tokio::test]
async fn custom_sticky_session_still_works() {
    let (state, seen) =
        state_with(ai_gateway_router::prompt::MODE_CUSTOM, GATEWAY_SYS).await;
    let body = r#"{"model":"m","metadata":{"conversation_id":"c1"},"messages":[{"role":"user","content":"hi"}]}"#;
    let (status, _wire) = roundtrip(state, &seen, "/v1/chat/completions", body).await;
    assert_eq!(status, StatusCode::OK);
}
