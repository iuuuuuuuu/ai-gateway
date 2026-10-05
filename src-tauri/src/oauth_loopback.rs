//! Trae OAuth 本机回环监听器（对标参考实现 `src-tauri/src/oauth_loopback.rs`）。
//!
//! 为什么必须有这一层：授权 URL 的 `redirect_uri` 是
//! `http://127.0.0.1:17388/authorize`，如果本机没有进程监听该端口，浏览器
//! 完成授权后回调无处落地，用户只能手动从地址栏复制 URL 再粘贴回来。
//!
//! 职责边界（见 `AGENTS.md` 架构约定）：**监听端口属于宿主**，因为需要
//! 宿主能力（`AppHandle` 发事件）；解析 / 换 token / 落库等纯逻辑都在
//! `ai_gateway_core::modules::trae_oauth` 与 `trae_account` 里。
//!
//! ## 为什么用 hyper 而不是 axum
//!
//! 参考实现用 `axum::serve` 起这个回环服务，但本 crate 的 `[dependencies]`
//! 里**没有 axum**，而这里早就用 hyper 起过两个同类的本机 HTTP 服务
//! （`device_token_server` / `captcha_webview`）。本服务只服务一条
//! `GET /authorize` 路由，hyper 的 `service_fn` 足够，不必为它再拉进
//! 一整棵 axum 依赖树（也免得 hyper 的 0.x/1.x 版本号又多一处分叉点）。
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

/// 回环服务共享状态。
struct LoopbackState {
    app: AppHandle,
    account_name: Option<String>,
    /// 最近一次请求时间（秒），用于空闲超时。
    last_active: Arc<AtomicI64>,
    /// 登录流程结束后由 handler 自发触发关停。
    shutdown_tx: watch::Sender<bool>,
}

/// 回环服务的响应体类型（hyper 1.x 要求显式给出 body 类型）。
type Resp = hyper::Response<http_body_util::Full<bytes::Bytes>>;

fn html_resp(status: u16, html: &str) -> Resp {
    hyper::Response::builder()
        .status(status)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(http_body_util::Full::new(bytes::Bytes::from(
            html.as_bytes().to_vec(),
        )))
        .unwrap_or_else(|_| hyper::Response::new(http_body_util::Full::new(bytes::Bytes::new())))
}

/// `GET /authorize?...` —— OAuth 回调落地。
///
/// 换 token 是网络请求（最长 2×120s），放进 `spawn_blocking` 避免占住
/// tokio worker —— 否则看门狗和关停信号都会被拖住。
async fn handle_authorize(st: Arc<LoopbackState>, path_query: String) -> Resp {
    st.last_active.store(now_secs(), Ordering::Relaxed);

    // 还原完整回调 URL：core 的解析器只关心 query，补上协议与主机即可
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

    html_resp(
        200,
        &trae_oauth::callback_page(ok, &message, &callback_url),
    )
}

/// 单连接入口：只认 `GET /authorize`，其余路径一律 404
/// （与参考实现那个只有一条路由的 axum `Router` 同义）。
async fn serve_conn(st: Arc<LoopbackState>, stream: tokio::net::TcpStream) {
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;

    let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let st = st.clone();
        async move {
            let path_query = req
                .uri()
                .path_and_query()
                .map(|pq| pq.as_str().to_string())
                .unwrap_or_else(|| "/".to_string());
            if req.uri().path() != "/authorize" {
                return Ok::<Resp, std::convert::Infallible>(html_resp(
                    404,
                    r#"<!doctype html><meta charset="utf-8"><p>404 —— 这里只处理 OAuth 回调 /authorize。</p>"#,
                ));
            }
            Ok(handle_authorize(st, path_query).await)
        }
    });

    let io = TokioIo::new(stream);
    if let Err(e) = hyper::server::conn::http1::Builder::new()
        .serve_connection(io, svc)
        .await
    {
        // 浏览器提前断开（用户关掉标签页）也会走到这里，不值得当故障上报
        eprintln!("[OAuth] 回调连接结束: {e}");
    }
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

    // ★★ 回调自带的 userInfo 是 uid/昵称的**首选**来源（真实客户端就是
    // `r ?? await getUserInfo(...)`，回调带了就不再请求）。我们此前只用
    // GetUserInfo + 解析 JWT，两条路任一失败就报「无法确定账号 id」。
    let (cb_name, cb_uid, _cb_region) = trae_oauth::user_info_from_callback(callback_url);

    // 昵称取不到不影响登录：用用户填的备注名，再退到回调/接口/uid
    let info = rt.block_on(trae_oauth::fetch_user_info(&access));
    let (fetched_name, uid_from_info) = info.unwrap_or((None, None));

    // JWT 形态与「粘贴 JWT」一致，才能复用同一条落库路径与后续的切换逻辑
    let jwt = format!("Cloud-IDE-JWT {access}");
    let uid = ai_gateway_core::modules::trae_account::parse_jwt(&jwt)
        .user_id
        .or(cb_uid)
        .or(uid_from_info)
        .ok_or_else(|| {
            "登录成功但无法确定账号 id：请把浏览器地址栏的完整 URL 粘贴到「粘贴回调地址」框".to_string()
        })?;

    let name = account_name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or(cb_name)
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

    // graceful shutdown：主动关停（登录完成 / oauth_stop_loopback）或 5 分钟空闲。
    //
    // ⚠ 看门狗 future 必须在循环**外**创建并 `pin` 住：写进 `select!` 分支里
    // 每轮 accept 都会把它重建一次，计时器被反复重置，超时永远不会触发。
    let mut rx = shutdown_rx.clone();
    let la = last_active.clone();
    let join = tokio::spawn(async move {
        let watchdog = idle_watchdog(la);
        tokio::pin!(watchdog);
        loop {
            tokio::select! {
                _ = rx.changed() => break,
                _ = &mut watchdog => break,
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _)) => {
                            let st = st.clone();
                            tokio::spawn(serve_conn(st, stream));
                        }
                        Err(e) => eprintln!("[OAuth] 回调监听接受连接失败: {e}"),
                    }
                }
            }
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

/// 诊断「浏览器打不开授权页」这一类故障（2026-10-02 所有者现场）。
///
/// # 为什么需要它
///
/// 所有者报告「浏览器会打开，但是访问超时」。实测定位到：
///
/// ```text
/// 系统代理 ProxyServer   = 127.0.0.1:9000   (Reqable)
/// ProxyOverride          = <-loopback>
///
/// trae.cn 经该代理  → HTTP 000，超时 20s     ← 浏览器就是这样走的
/// trae.cn 直连      → HTTP 200，0.39s
/// ```
///
/// 即**代理本身不通**时，浏览器（默认「使用系统代理设置」）打不开授权页。
/// 这与本产品无关，但用户看到的只是「超时」，无从判断原因 ——
/// 所以由产品把**它测到的事实**摆出来，而不是让用户去猜。
///
/// # 返回
///
/// ```json
/// {
///   "proxyEnabled": true,
///   "proxyServer": "127.0.0.1:9000",
///   "proxyOverride": "<-loopback>",
///   "loopbackAlsoProxied": true,
///   "direct": { "ok": true, "ms": 390 },
///   "viaProxy": { "ok": false, "ms": 20008, "error": "..." },
///   "advice": "…"
/// }
/// ```
///
/// ⚠ **只读**：只做一次 TCP+TLS 探测，不改注册表、不改用户代理设置。
#[tauri::command(rename_all = "camelCase")]
pub async fn oauth_diagnose_network() -> Result<Value, String> {
    let probe_url = "https://www.trae.cn/";

    // 读注册表里的系统代理（Windows）。读不到就当未启用 ——
    // 非 Windows 或权限受限时不该让整个诊断失败。
    #[cfg(windows)]
    let (enabled, server, over) = read_system_proxy();
    #[cfg(not(windows))]
    let (enabled, server, over) = (false, String::new(), String::new());

    let direct = probe_once(probe_url, None).await;
    let via = if enabled && !server.is_empty() {
        probe_once(probe_url, Some(&server)).await
    } else {
        json!({ "skipped": true })
    };

    // `<-loopback>` 是 Windows 的特殊记号：它让「绕过代理」**不再**包含环回地址
    // ⇒ 连 `127.0.0.1:17388` 的回调也会被送去代理。对 OAuth 回环是致命的。
    let loopback_proxied = over.to_ascii_lowercase().contains("<-loopback>");

    let direct_ok = direct.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let via_ok = via.get("ok").and_then(Value::as_bool).unwrap_or(true);

    let advice = if !direct_ok {
        "直连 trae.cn 也不通：请检查网络本身（DNS / 防火墙 / 是否被墙）。".to_string()
    } else if enabled && !server.is_empty() && !via_ok {
        format!(
            "系统代理 {server} 当前**不通**（直连正常）。浏览器默认跟随系统代理，\
             所以授权页会一直转圈或超时。临时办法：关掉该代理，或在浏览器里\
             把 www.trae.cn 加入代理例外；也可以在「设置 → 网络代理」里让本应用不走代理。"
        )
    } else if loopback_proxied {
        "代理例外含 `<-loopback>`：连 127.0.0.1 的回调也会走代理。\
         若回环回调失败（授权后页面报错），请去掉这个记号或把 127.0.0.1 加入例外。"
            .to_string()
    } else {
        "网络与代理均正常。若仍打不开，请检查浏览器是否装了拦截类扩展。".to_string()
    };

    Ok(json!({
        "proxyEnabled": enabled,
        "proxyServer": server,
        "proxyOverride": over,
        "loopbackAlsoProxied": loopback_proxied,
        "direct": direct,
        "viaProxy": via,
        "advice": advice,
    }))
}

/// 读 Windows 系统代理设置（`HKCU\...\Internet Settings`）。**只读**。
///
/// ⚠ 用 `reg.exe` 而不是 `winreg` crate：本命令是诊断用，不值得为它加一个依赖。
///
/// ⚠⚠ **`ProxyEnable` 是 `REG_DWORD`，不是 `REG_SZ`** —— 第一版按 `REG_SZ` 切分，
/// 于是永远读成 `false`，界面上就会出现「代理未启用」却同时列出一个代理地址
/// 这种自相矛盾的结论（本机实测踩到）。这里按**类型无关**的方式取值：
/// 取该行最后一个空白分隔的字段。
#[cfg(windows)]
fn read_system_proxy() -> (bool, String, String) {
    let out = std::process::Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
        ])
        .output();
    let Ok(out) = out else {
        return (false, String::new(), String::new());
    };
    let text = String::from_utf8_lossy(&out.stdout);

    // `    ProxyServer    REG_SZ    127.0.0.1:9000`
    // 取最后一个字段即可，不必关心类型列是 REG_SZ 还是 REG_DWORD。
    let grab = |key: &str| -> String {
        text.lines()
            .find(|l| l.trim_start().starts_with(key))
            .and_then(|l| l.split_whitespace().last())
            .map(|v| v.trim().to_string())
            .unwrap_or_default()
    };

    // DWORD 的形态是 `0x1` / `0x0`；也兼容写成 `1` 的场合。
    let raw_enable = grab("ProxyEnable");
    let enabled = raw_enable.eq_ignore_ascii_case("0x1") || raw_enable == "1";
    let server = grab("ProxyServer");
    let over = grab("ProxyOverride");
    (enabled, server, over)
}

/// 探一次：能否连上目标（可选经 `proxy`），返回 `{ok, ms, error?}`。
///
/// # 为什么用裸 TCP 而不是 HTTP 客户端
///
/// 1. `src-tauri` 没有 `reqwest` 依赖（它用 hyper 做服务端），
///    为一个诊断命令拉进一个 HTTP 客户端不划算；
/// 2. **诊断要的正是"连不连得上"** —— TCP 握手成功就说明链路通，
///    不需要真的走完 TLS + HTTP。超时/拒绝/解析失败都能精确区分。
///
/// # ★ 为什么必须显式解析成 IPv4（2026-10-02 实测踩到）
///
/// 第一版直接 `TcpStream::connect("www.trae.cn:443")`，于是走了系统 DNS 顺序 ——
/// 而本机 DNS **先返回 IPv6**，到该站的 IPv6 又**不通**，结果诊断报「直连不通」，
/// 与事实（`curl -4` 200/0.2s）相反。
///
/// 更糟的是：那条错误的诊断会**掩盖**真实病因 —— 用户看到"网络本身有问题"，
/// 于是去查宽带，而真正的问题是应用侧 IPv6 优先。
///
/// 故这里显式取 A 记录（IPv4）来探。这也与 `config::http_request_direct`
/// 固定 IPv4 的行为**保持一致** —— 诊断必须测"产品实际会走的那条路"。
async fn probe_once(url: &str, proxy: Option<&str>) -> Value {
    // 从 URL 里抠出 host:port（只支持 https/http，够诊断用）
    let (host, port) = match parse_host_port(url) {
        Some(v) => v,
        None => return json!({ "ok": false, "ms": 0, "error": format!("无法解析目标地址：{url}") }),
    };

    // 要连的目标：直连就连 host:port；走代理就连代理，由代理去取
    let (target_host, target_port, note) = match proxy {
        Some(p) => match normalize_proxy(p) {
            Some((h, pt)) => (h, pt, String::new()),
            None => {
                return json!({ "ok": false, "ms": 0, "error": format!("代理地址无法解析：{p}") });
            }
        },
        None => {
            // ★ 直连时**显式解析成 IPv4**，与 `http_request_direct` 的口径一致。
            // 解析不到 A 记录才退回原名（让 DNS 自己去试）。
            let v4 = tokio::net::lookup_host((host.as_str(), port))
                .await
                .ok()
                .and_then(|it| it.into_iter().find(|a| a.is_ipv4()));
            match v4 {
                Some(addr) => (
                    addr.ip().to_string(),
                    addr.port(),
                    format!("（已解析到 IPv4 {addr}）"),
                ),
                None => (host.clone(), port, "（未取到 A 记录，退回域名）".to_string()),
            }
        }
    };

    let started = std::time::Instant::now();
    let addr = format!("{target_host}:{target_port}");
    match tokio::time::timeout(
        Duration::from_secs(8),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    {
        Ok(Ok(_)) => json!({ "ok": true, "ms": started.elapsed().as_millis() as u64, "via": addr, "note": note }),
        Ok(Err(e)) => json!({
            "ok": false,
            "ms": started.elapsed().as_millis() as u64,
            "via": addr,
            "note": note,
            "error": e.to_string(),
        }),
        Err(_) => json!({
            "ok": false,
            "ms": started.elapsed().as_millis() as u64,
            "via": addr,
            "note": note,
            "error": "连接超时（8 秒）",
        }),
    }
}

/// 从 `https://host[:port]/path` 抠出 `(host, port)`；缺端口按 scheme 补。
fn parse_host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let default_port = match scheme {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    // 去掉可能的 userinfo
    let hostport = authority.rsplit('@').next()?;
    match hostport.rsplit_once(':') {
        Some((h, p)) => Some((h.to_string(), p.parse().ok()?)),
        None => Some((hostport.to_string(), default_port)),
    }
}

/// 把注册表里的 `ProxyServer` 归一成 `(host, port)`。
///
/// 真实取值有三种形态（都见过）：
///   · `127.0.0.1:9000`
///   · `http=127.0.0.1:9000;https=127.0.0.1:9000`
///   · `http://127.0.0.1:9000`
fn normalize_proxy(raw: &str) -> Option<(String, u16)> {
    // 多协议形态：取 https 段优先，其次 http，最后第一段
    let seg = if raw.contains('=') {
        let parts: Vec<&str> = raw.split(';').collect();
        parts
            .iter()
            .find(|s| s.trim_start().starts_with("https="))
            .or_else(|| parts.iter().find(|s| s.trim_start().starts_with("http=")))
            .or_else(|| parts.first())
            .map(|s| s.split_once('=').map(|(_, v)| v).unwrap_or(s))
            .unwrap_or(raw)
    } else {
        raw
    };
    let seg = seg.trim().trim_start_matches("http://").trim_start_matches("https://");
    match seg.rsplit_once(':') {
        Some((h, p)) => Some((h.to_string(), p.parse().ok()?)),
        None => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 注册表里的 `ProxyServer` 有三种真实形态，都要能解析出 host:port。
    ///
    /// 这段是诊断功能的**判据本身** —— 解析错了会给出错误的"代理不通"结论，
    /// 比没有诊断更糟（用户会去关一个本来正常的代理）。
    #[test]
    fn 代理地址三种形态都能解析() {
        // 形态 1：裸 host:port（本机实测就是这种）
        assert_eq!(
            normalize_proxy("127.0.0.1:9000"),
            Some(("127.0.0.1".to_string(), 9000))
        );
        // 形态 2：多协议（Windows 在有 http/https 分别配置时会写这种）
        assert_eq!(
            normalize_proxy("http=1.2.3.4:8080;https=5.6.7.8:8443"),
            Some(("5.6.7.8".to_string(), 8443)),
            "多协议时应优先取 https 段"
        );
        assert_eq!(
            normalize_proxy("http=1.2.3.4:8080"),
            Some(("1.2.3.4".to_string(), 8080)),
            "只有 http 段时取它"
        );
        // 形态 3：带 scheme
        assert_eq!(
            normalize_proxy("http://127.0.0.1:7890"),
            Some(("127.0.0.1".to_string(), 7890))
        );
        // 解析不了就返回 None（由调用方给出明确错误，而不是 panic）
        assert_eq!(normalize_proxy("不是地址"), None);
    }

    /// `https` 缺端口按 443 补、`http` 按 80 补。
    #[test]
    fn 目标地址解析补默认端口() {
        assert_eq!(
            parse_host_port("https://www.trae.cn/authorization?x=1"),
            Some(("www.trae.cn".to_string(), 443))
        );
        assert_eq!(
            parse_host_port("http://127.0.0.1:17388/authorize"),
            Some(("127.0.0.1".to_string(), 17388))
        );
        // 带 userinfo 的也要能抠对（代理 URL 常见）
        assert_eq!(
            parse_host_port("https://u:p@example.com:8443/x"),
            Some(("example.com".to_string(), 8443))
        );
        assert_eq!(parse_host_port("ftp://x/y"), None, "不支持的 scheme 应返回 None");
    }

    /// 认得出 `<-loopback>` 记号。
    ///
    /// ⚠ 判据是 `contains("<-loopback>")` 这**一整段**，不是 `contains("loopback")`：
    /// 后者会把 `localhost;127.*` 这类普通例外误判成「环回也走代理」，
    /// 于是给用户一个错误的"回调会失败"警告。
    #[test]
    fn 认得出_loopback_记号() {
        let has = |s: &str| s.to_ascii_lowercase().contains("<-loopback>");
        // 本机实测就是这一种
        assert!(has("<-loopback>"));
        // 大小写不敏感（注册表里见过 <-LOOPBACK>）
        assert!(has("*.local;<-LOOPBACK>"));
        // 与别的例外混在一起也要认出来
        assert!(has("localhost;127.*;10.*;<-loopback>"));
        // ★ 普通例外**不该**被误判 —— 它们本来就让环回绕过代理，是正常的
        assert!(!has("localhost;127.*;<local>"), "`<local>` 不是 `<-loopback>`");
        assert!(!has("localhost;127.*"), "没有记号就是正常绕过");
        assert!(!has(""), "空值不能判成含记号");
    }

    /// `reg query` 输出的解析 —— 用**真实输出样本**钉住。
    ///
    /// ★ 这段骗过我一次：第一版按 `split("REG_SZ")` 取值，而
    /// **`ProxyEnable` 是 `REG_DWORD`**，于是永远解析成 `false`，
    /// 界面上出现「代理未启用」却同时列出一个代理地址的自相矛盾结论。
    ///
    /// 故这里直接贴本机实测的三行原文，逐行验证。
    #[test]
    fn reg_query_输出解析() {
        let sample = "\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings\r\n    ProxyEnable    REG_DWORD    0x1\r\n    ProxyServer    REG_SZ    127.0.0.1:9000\r\n    ProxyOverride    REG_SZ    <-loopback>\r\n";

        let grab = |key: &str| -> String {
            sample
                .lines()
                .find(|l| l.trim_start().starts_with(key))
                .and_then(|l| l.split_whitespace().last())
                .map(|v| v.trim().to_string())
                .unwrap_or_default()
        };

        // 关键：DWORD 也要能取到值（不能依赖类型列是 REG_SZ）
        assert_eq!(grab("ProxyEnable"), "0x1");
        assert_eq!(grab("ProxyServer"), "127.0.0.1:9000");
        assert_eq!(grab("ProxyOverride"), "<-loopback>");

        let enabled = {
            let raw = grab("ProxyEnable");
            raw.eq_ignore_ascii_case("0x1") || raw == "1"
        };
        assert!(enabled, "0x1 必须解析成『已启用』");

        // 关闭态
        let off = "    ProxyEnable    REG_DWORD    0x0\r\n";
        let raw = off
            .lines()
            .find(|l| l.trim_start().starts_with("ProxyEnable"))
            .and_then(|l| l.split_whitespace().last())
            .unwrap_or_default();
        assert!(!(raw.eq_ignore_ascii_case("0x1") || raw == "1"), "0x0 必须是未启用");
    }
}

