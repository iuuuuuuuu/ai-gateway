//! Trae OAuth 本机回环监听器（F-78，对标参考实现 `commands/oauth_loopback.rs`）。
//!
//! 为什么必须有这一层：授权 URL 的 `redirect_uri` 是
//! `http://127.0.0.1:17388/authorize`，如果本机没有进程监听该端口，浏览器
//! 完成授权后回调无处落地，用户只能手动从地址栏复制 URL 再粘贴回来。
//!
//! 职责边界（见 `AGENTS.md` 架构约定）：**监听端口属于宿主**，因为需要
//! axum + Tauri 事件；解析 / 换 token / 落库等纯逻辑都在
//! `ai_gateway_core::modules::trae_oauth` 与 `apps_ops`。
//!
//! 生命周期：
//! - 发起登录时 [`oauth_start_loopback`] 起服务；
//! - 收到回调 → 换 token → 落库 → 发 `oauth-login-done` 事件 → 自动关停；
//! - 5 分钟无任何请求自动关停（用户中途放弃时不留悬挂端口）；
//! - [`oauth_stop_loopback`] 主动关停（弹窗关闭 / 组件卸载）。
//!
//! 登录完成（无论成败）都会自动关停并释放端口 —— 下一次登录重新起。

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::extract::State as AxumState;
use axum::http::Uri;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tokio::sync::watch;

use ai_gateway_core::modules::trae_oauth;

/// 空闲超时：5 分钟内没有任何请求（含回调）即自动关停。
const IDLE_TIMEOUT_SECS: i64 = 300;
/// 空闲看门狗巡检间隔。
const IDLE_CHECK_INTERVAL_SECS: i64 = 30;

/// 回环监听器运行句柄（全局唯一；重启登录时先停旧的）。
struct LoopbackHandle {
    /// 发送 `true` 触发 graceful shutdown。
    shutdown_tx: watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}

fn loopback_handle() -> &'static Mutex<Option<LoopbackHandle>> {
    static HANDLE: OnceLock<Mutex<Option<LoopbackHandle>>> = OnceLock::new();
    HANDLE.get_or_init(|| Mutex::new(None))
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// axum handler 共享状态。
struct LoopbackState {
    app: AppHandle,
    account_name: Option<String>,
    /// 最近一次请求时间（秒），用于空闲超时。
    last_active: Arc<AtomicI64>,
    /// 登录流程结束后由 handler 自发触发关停。
    shutdown_tx: watch::Sender<bool>,
}

/// `GET /authorize?...` —— OAuth 回调落地。
///
/// 换 token 是网络请求（最长 2×120s），放进 `spawn_blocking` 避免占住
/// tokio worker —— 否则看门狗和关停信号都会被拖住。
async fn handle_authorize(AxumState(st): AxumState<Arc<LoopbackState>>, uri: Uri) -> Html<String> {
    st.last_active.store(now_secs(), Ordering::Relaxed);

    // 还原完整回调 URL：core 的解析器只关心 query，补上协议与主机即可
    let path_query = uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/authorize".to_string());
    let callback_url = format!(
        "http://127.0.0.1:{}{}",
        trae_oauth::OAUTH_LOOPBACK_PORT,
        path_query
    );

    let name = st.account_name.clone();
    let url = callback_url.clone();
    let result = tokio::task::spawn_blocking(move || finish_login(&url, name.as_deref())).await;

    let (ok, message, user_id) = match result {
        Ok(Ok((uid, msg))) => (true, msg, Some(uid)),
        Ok(Err(e)) => (false, e, None),
        Err(e) => (false, format!("登录任务执行异常：{e}"), None),
    };

    // 通知前端自动收尾：成功则关弹窗刷列表，失败则提示走手动粘贴兜底
    let _ = st.app.emit(
        "oauth-login-done",
        json!({ "ok": ok, "message": message, "userId": user_id }),
    );

    // 登录已结束，自动关停监听释放端口
    let _ = st.shutdown_tx.send(true);

    Html(trae_oauth::callback_page(ok, &message, &callback_url))
}

/// 把回调 URL 走完整条登录链路：解析 → 换 token → 取昵称 → 落库。
///
/// 同步实现（内部用 `block_on` 驱动 async 请求），因为它运行在
/// `spawn_blocking` 线程上。
fn finish_login(callback_url: &str, account_name: Option<&str>) -> Result<(String, String), String> {
    let code = trae_oauth::parse_callback(callback_url)?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("创建运行时失败：{e}"))?;

    let (access, refresh) = rt.block_on(trae_oauth::exchange_auth_code(&code))?;

    // 昵称取不到不影响登录：用用户填的备注名，再退到 uid
    let info = rt.block_on(trae_oauth::fetch_user_info(&access));
    let (fetched_name, uid_from_info) = info.unwrap_or((None, None));

    // JWT 形态与「粘贴 JWT」一致，才能复用同一条落库路径与后续的切换逻辑
    let jwt = format!("Cloud-IDE-JWT {access}");
    let uid = ai_gateway_core::modules::trae_account::parse_jwt(&jwt)
        .user_id
        .or(uid_from_info)
        .ok_or_else(|| {
            "登录成功但无法确定账号 id：请把浏览器地址栏的完整 URL 粘贴到「粘贴回调地址」框".to_string()
        })?;

    let name = account_name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or(fetched_name);

    let acc = ai_gateway_core::modules::trae_account::upsert_account(
        &uid,
        name.as_deref(),
        &jwt,
        refresh.as_deref(),
    )?;

    // 登录会话用完即弃，避免旧 state 被复用
    trae_oauth::clear_pending();

    let display = acc
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(&uid)
        .to_string();
    Ok((uid, format!("账号 [{display}] 登录成功")))
}

/// 启动回环监听器（`redirect_uri = http://127.0.0.1:17388/authorize`）。
///
/// 幂等：重复调用先停旧监听再重启；端口被占用返回明确错误
/// （前端据此降级为「手动粘贴回调地址」兜底）。
#[tauri::command]
pub async fn oauth_start_loopback(
    app: AppHandle,
    account_name: Option<String>,
) -> Result<Value, String> {
    stop_loopback().await;

    let addr = format!("127.0.0.1:{}", trae_oauth::OAUTH_LOOPBACK_PORT);
    let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            format!(
                "OAuth 回调端口 {} 已被其他程序占用（{e}）；可改用「手动粘贴回调地址」完成登录",
                trae_oauth::OAUTH_LOOPBACK_PORT
            )
        } else {
            format!("OAuth 回调监听启动失败：{e}")
        }
    })?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let last_active = Arc::new(AtomicI64::new(now_secs()));
    let st = Arc::new(LoopbackState {
        app: app.clone(),
        account_name,
        last_active: last_active.clone(),
        shutdown_tx: shutdown_tx.clone(),
    });

    let router = Router::new()
        .route("/authorize", get(handle_authorize))
        .with_state(st);

    // graceful shutdown：主动关停（登录完成 / oauth_stop_loopback）或 5 分钟空闲
    let mut rx = shutdown_rx.clone();
    let la = last_active.clone();
    let server = axum::serve(listener, router).with_graceful_shutdown(async move {
        tokio::select! {
            _ = rx.changed() => {}
            _ = idle_watchdog(la) => {}
        }
    });
    let join = tokio::spawn(async move {
        if let Err(e) = server.await {
            eprintln!("[OAuth] 回环监听器异常退出: {e}");
        }
    });

    *loopback_handle().lock().unwrap_or_else(|e| e.into_inner()) = Some(LoopbackHandle {
        shutdown_tx,
        join,
    });

    Ok(json!({
        "ok": true,
        "port": trae_oauth::OAUTH_LOOPBACK_PORT,
        "redirectUri": trae_oauth::OAUTH_REDIRECT_URI,
        "idleTimeoutSecs": IDLE_TIMEOUT_SECS,
    }))
}

/// 停止回环监听器（未启动时幂等成功）。
#[tauri::command]
pub async fn oauth_stop_loopback() -> Result<Value, String> {
    stop_loopback().await;
    Ok(json!({ "ok": true }))
}

/// 停止运行中的监听：发送 shutdown 并最多等 2 秒让端口释放。
async fn stop_loopback() {
    let Some(h) = loopback_handle()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    else {
        return;
    };
    let _ = h.shutdown_tx.send(true);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !h.join.is_finished() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // 端口可能还没完全释放（TIME_WAIT），下一次 bind 的明确报错会兜住
}

/// 空闲看门狗：每 30 秒巡检一次，距最近请求超过 5 分钟即触发 graceful shutdown。
async fn idle_watchdog(last_active: Arc<AtomicI64>) {
    loop {
        tokio::time::sleep(Duration::from_secs(IDLE_CHECK_INTERVAL_SECS as u64)).await;
        if now_secs() - last_active.load(Ordering::Relaxed) >= IDLE_TIMEOUT_SECS {
            break;
        }
    }
}

/// 签发授权 URL（同时登记 PKCE / state 会话）。
#[tauri::command(rename_all = "camelCase")]
pub async fn oauth_login_url(account_name: Option<String>) -> Result<Value, String> {
    trae_oauth::login_url(account_name.as_deref())
}

/// 手动兜底：把浏览器地址栏里的回调 URL 粘进来完成登录。
///
/// 端口被占用、用户换了浏览器、回调页没能自动关停时都靠这条路径。
#[tauri::command(rename_all = "camelCase")]
pub async fn oauth_submit_callback(
    callback_url: String,
    account_name: Option<String>,
) -> Result<Value, String> {
    let url = callback_url.trim().to_string();
    if url.is_empty() {
        return Err("请粘贴浏览器地址栏中的完整回调 URL".to_string());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let (uid, message) = finish_login(&url, account_name.as_deref())?;
        Ok(json!({ "ok": true, "userId": uid, "message": message }))
    })
    .await
    .map_err(|e| format!("登录任务执行异常：{e}"))?
}

/// 当前是否仍在等待 OAuth 回调（界面据此显示「等待浏览器授权」）。
#[tauri::command]
pub async fn oauth_pending() -> Result<Value, String> {
    Ok(json!({ "pending": trae_oauth::has_pending() }))
}

/// 放弃当前登录会话。
#[tauri::command]
pub async fn oauth_cancel() -> Result<Value, String> {
    trae_oauth::clear_pending();
    stop_loopback().await;
    Ok(json!({ "ok": true }))
}
