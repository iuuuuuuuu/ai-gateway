//! 本地代理（MITM 设备身份代理）的 Tauri 命令层。
//!
//! 这一层只做三件事：
//! 1. **托管代理进程的句柄**（进程内静态变量，桌面端只允许一个代理实例）
//! 2. 把 core 的 [`ProxyEventSink`] 事件转发成 Tauri 事件推给前端
//! 3. 系统代理的开启/还原编排（core 的 `sys_proxy` 提供原语）
//!
//! ## 为什么必须托管单例
//!
//! 代理会改写**系统代理设置**。若允许开两个实例，第二个会读到「第一个刚设好的」
//! 系统代理并把它当成「用户原有的代理」记下来 —— 停止时就会把用户的真实设置
//! 覆盖成指向本地代理的地址，表现为「关掉应用后全网断网」。
//!
//! ## 崩溃看门狗
//!
//! `exited(intentional=false)` 表示 accept 循环自己结束了（panic / 意外错误）。
//! 此时系统代理仍指向本地死端口，必须立刻还原 —— 否则用户会看到「应用没关但网断了」。

use std::sync::Arc;

use serde_json::{json, Value};

use ai_gateway_core::modules::{
    config,
    device_proxy::{
        self,
        sys_proxy::{self},
        ProxyConfig, ProxyEventSink, ProxyServer,
    },
};

/// 进程内唯一的代理实例。
static PROXY: std::sync::OnceLock<tokio::sync::Mutex<Option<ProxyServer>>> =
    std::sync::OnceLock::new();

fn proxy_slot() -> &'static tokio::sync::Mutex<Option<ProxyServer>> {
    PROXY.get_or_init(|| tokio::sync::Mutex::new(None))
}

/// 代理事件 → Tauri 事件桥。
struct TauriSink {
    app: tauri::AppHandle,
}

impl ProxyEventSink for TauriSink {
    fn log(&self, line: &str) {
        use tauri::Emitter;
        let _ = self.app.emit("proxy-log", json!({ "line": line }));
    }

    fn account_captured(&self, uid: &str) {
        use tauri::Emitter;
        let _ = self.app.emit("account-captured", json!({ "uid": uid }));
    }

    fn exited(&self, intentional: bool) {
        use tauri::Emitter;
        let _ = self.app.emit("proxy-crashed", json!({ "intentional": intentional }));
        // 非主动退出 = 意外崩溃：系统代理还指向本地死端口，必须立刻还原，
        // 否则用户会看到「应用没关但网断了」。
        if !intentional {
            let detail = match sys_proxy::restore_system_proxy_on_exit(true) {
                Ok(()) => "已还原系统代理".to_string(),
                Err(e) => format!("还原系统代理失败: {e}"),
            };
            eprintln!("[proxy] 代理意外退出，{detail}");
            let _ = self.app.emit(
                "proxy-log",
                json!({ "line": format!("代理意外退出，{detail}") }),
            );
        }
    }
}

/// 读取代理配置（端口、目标域名、上游代理）。
#[tauri::command]
pub fn proxy_config() -> Value {
    let settings = config::load_app_settings();
    let store = config::store_dir();
    let port = settings
        .get("proxy_port")
        .and_then(Value::as_u64)
        .map(|p| p as u16)
        .unwrap_or(8899);
    let domains = settings
        .get("proxy_domains")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| device_proxy::DEFAULT_TARGETS.join(","));

    json!({
        "port": port,
        "domains": domains,
        "defaultDomains": device_proxy::DEFAULT_TARGETS.join(","),
        "lastPort": sys_proxy::last_proxy_port(&store),
        "existingSystemProxy": sys_proxy::get_existing_win_proxy(),
    })
}

/// 代理运行状态。
#[tauri::command]
pub async fn proxy_status() -> Value {
    let running = {
        let guard = proxy_slot().lock().await;
        guard.as_ref().is_some_and(ProxyServer::is_running)
    };
    json!({
        "running": running,
        "port": sys_proxy::last_proxy_port(&config::store_dir()),
        "captured": 0,
    })
}

/// **检测代理是否真的能用**（2026-09-22 所有者要求：
/// 「本地代理,配置之后再加上检测按钮」）。
///
/// # 为什么需要"检测"而不是只看"运行中"
///
/// `proxy_status` 只能告诉你**进程在跑**，不能告诉你：
///
///   · 端口是否真的在监听（进程活着但 bind 失败过）
///   · 系统代理是否真的指向了它（用户手工改过、或被别的软件覆盖）
///   · CA 证书是否已装（没装的话 HTTPS 全报证书错，而进程一切正常）
///   · 上游是否能通（配错了目标域名时，请求会 502）
///
/// 这四件事**都会让用户觉得"代理坏了"**，而 `running: true` 一个都答不了。
/// 故本命令逐项检查，并给出**可直接行动**的结论。
///
/// # 返回
///
/// ```json
/// { "ok": true, "checks": [
///     { "name": "端口监听", "ok": true,  "detail": "127.0.0.1:8899 可连接" },
///     { "name": "系统代理", "ok": false, "detail": "指向 127.0.0.1:9999，与本代理不一致" }
/// ]}
/// ```
///
/// ⚠ `ok` 是**所有检查项都通过**才为 true —— 界面据此决定显示绿色还是红色，
/// 不要用"没有失败的"这种含糊口径。
#[tauri::command]
pub async fn proxy_check() -> Result<Value, String> {
    let store = config::store_dir();
    let port = sys_proxy::last_proxy_port(&store);

    let running = {
        let guard = proxy_slot().lock().await;
        guard.as_ref().is_some_and(ProxyServer::is_running)
    };

    let mut checks: Vec<Value> = Vec::new();
    let mut all_ok = true;
    let mut push = |name: &str, ok: bool, detail: String| {
        all_ok = all_ok && ok;
        checks.push(json!({ "name": name, "ok": ok, "detail": detail }));
    };

    // ---- 1. 进程是否在跑 ----
    push(
        "代理进程",
        running,
        if running {
            "正在运行".to_string()
        } else {
            "未运行 —— 点「启动代理」后重试".to_string()
        },
    );

    // ---- 2. 端口是否真的可连接 ----
    //
    // ⚠ 用**真实 TCP 连接**而不是查配置里的端口号：
    // 进程活着但没监听（bind 失败、被防火墙拦）是真实会发生的，
    // 只看数字会把那种情况判成通过。
    match port {
        Some(p) if p > 0 => {
            let addr = format!("127.0.0.1:{p}");
            let connected = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                tokio::net::TcpStream::connect(&addr),
            )
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false);
            push(
                "端口监听",
                connected,
                if connected {
                    format!("{addr} 可连接")
                } else {
                    format!("{addr} 连不上 —— 代理可能没真正监听（防火墙或被占用）")
                },
            );
        }
        _ => push("端口监听", false, "没有记录到端口，请先启动一次代理".to_string()),
    }

    // ---- 3. 系统代理是否指向本代理 ----
    //
    // 这是最常见的"看起来在跑、实际没生效"：系统代理被别的软件改了，
    // 或用户上次停止时没还原干净。
    //
    // ⚠ `get_existing_win_proxy` 返回的是**三元组**
    // `(启用, ProxyServer, ProxyOverride)`，不是单个字符串。
    match sys_proxy::get_existing_win_proxy() {
        Some((_, server, _override)) => {
            let expected = port.map(|p| format!("127.0.0.1:{p}"));
            let matches = expected.as_deref().is_some_and(|e| server.contains(e));
            push(
                "系统代理",
                matches,
                if matches {
                    format!("已指向本代理（{server}）")
                } else {
                    format!("当前是 {server}，与本代理不一致 —— 浏览器流量不会经过本代理")
                },
            );
        }
        None => push(
            "系统代理",
            !running,
            if running {
                "系统里没有代理设置 —— 代理在跑但没接管系统代理".to_string()
            } else {
                "没有代理设置（代理也未运行，符合预期）".to_string()
            },
        ),
    }

    // ---- 4. CA 证书是否已生成 ----
    //
    // 没装证书时 HTTPS 会全部报证书错误，而进程一切正常 ——
    // 用户很容易误判成"代理坏了"。
    //
    // ⚠ 路径口径与 `proxy_cert_status` **必须一致**（都是
    // `store_dir()/certs`）—— 两边各算一次迟早会算出不同的值，
    // 而那种错法表现为"检测说没证书、生成按钮说已生成"。
    let certs = config::store_dir().join("certs");
    let ca_exists = certs.join("ca.pem").exists() || certs.join("ca.cer").exists();
    push(
        "CA 证书",
        ca_exists,
        if ca_exists {
            format!("已生成（{}）", certs.display())
        } else {
            "尚未生成 —— 点「生成证书」；未装证书时 HTTPS 会报证书错误".to_string()
        },
    );

    Ok(json!({
        "ok": all_ok,
        "checks": checks,
        // 检查时间：界面显示"上次检测于 …"，避免用户看着旧结果判断当前状态
        "checkedAt": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    }))
}

/// 启动代理并接管系统代理。
#[tauri::command(rename_all = "camelCase")]
pub async fn proxy_start(
    app: tauri::AppHandle,
    port: Option<u16>,
    domains: Option<String>,
) -> Result<Value, String> {
    {
        let guard = proxy_slot().lock().await;
        if guard.as_ref().is_some_and(ProxyServer::is_running) {
            return Err("代理已在运行".to_string());
        }
    }

    let settings = config::load_app_settings();
    let port = port
        .or_else(|| {
            settings
                .get("proxy_port")
                .and_then(Value::as_u64)
                .map(|p| p as u16)
        })
        .unwrap_or(8899);
    let domains = domains.unwrap_or_else(|| {
        settings
            .get("proxy_domains")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| device_proxy::DEFAULT_TARGETS.join(","))
    });

    let store = config::store_dir();

    // 先记下「用户原有的系统代理」，停止时才能原样还原。
    // 必须发生在 set_win_proxy 之前 —— 否则记下的是我们自己设的值。
    let previous = sys_proxy::get_existing_win_proxy();
    // `capture_previous_proxy` 返回的是「规格字符串」，需要再解析成结构化上游；
    // 解析失败（用户原有代理格式异常）时回退为 None，即直连。
    let upstream_spec = sys_proxy::capture_previous_proxy(
        previous
            .as_ref()
            .map(|(_, addr, _)| addr.as_str())
            .unwrap_or(""),
        port,
    );
    let upstream = upstream_spec.and_then(|spec| sys_proxy::upstream_from_capture(Some(spec)));

    let cfg = ProxyConfig::new(port)
        .with_events(Arc::new(TauriSink { app: app.clone() }))
        .with_upstream(upstream)
        .with_targets_csv(&domains);

    let server = ProxyServer::start(cfg).await?;
    sys_proxy::set_win_proxy(&format!("127.0.0.1:{port}"))?;
    sys_proxy::record_proxy_port(&store, port)?;

    *proxy_slot().lock().await = Some(server);

    // 端口与目标域名落盘，重启应用后能沿用
    let _ = config::save_app_settings(&json!({
        "proxy_port": port,
        "proxy_domains": domains,
    }));

    Ok(json!({ "ok": true, "port": port, "domains": domains }))
}

/// 停止代理并还原系统代理。
#[tauri::command]
pub async fn proxy_stop() -> Result<Value, String> {
    let server = proxy_slot().lock().await.take();
    match server {
        Some(server) => {
            // `stop()` 会置 shutdown 标志，accept 循环据此把退出事件标为
            // `intentional = true` —— 看门狗不会重复还原系统代理。
            server.stop();
        }
        None => return Ok(json!({ "ok": true, "alreadyStopped": true })),
    }
    sys_proxy::restore_system_proxy(true)?;
    Ok(json!({ "ok": true }))
}

/// CA 证书状态。
#[tauri::command]
pub async fn proxy_cert_status() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let certs = config::store_dir().join("certs");
        // 两个文件名都探测：ca.cer 是给 Windows 双击安装用的 DER，
        // ca.pem 是内部读取用的 PEM。
        let cer = certs.join("ca.cer");
        let pem = certs.join("ca.pem");
        Ok(json!({
            "certsDir": certs.to_string_lossy(),
            "caCerPath": cer.to_string_lossy(),
            "caPemPath": pem.to_string_lossy(),
            "caExists": cer.is_file() || pem.is_file(),
            "hint": "首次使用需把 certs/ca.cer 安装到「受信任的根证书颁发机构」，\
否则 HTTPS 拦截会因证书不受信而失败（浏览器报 ERR_CERT_AUTHORITY_INVALID）。",
        }))
    })
    .await
    .map_err(|e| format!("读取证书状态失败: {e}"))?
}

/// 生成自签 CA（若已存在则复用）。
#[tauri::command]
pub async fn proxy_cert_generate() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let certs = config::store_dir().join("certs");
        std::fs::create_dir_all(&certs).map_err(|e| format!("创建证书目录失败: {e}"))?;
        // 只关心「能不能拿到可用 CA」，不持有它 —— 真正使用时由
        // ProxyServer::start 内部再取一次（同一份文件，结果一致）。
        let _ = device_proxy::ca::ensure_ca(&certs)?;
        Ok(json!({
            "ok": true,
            "certsDir": certs.to_string_lossy(),
            "caCerPath": certs.join("ca.cer").to_string_lossy(),
        }))
    })
    .await
    .map_err(|e| format!("生成 CA 失败: {e}"))?
}

/// 从本机离线捕获 Trae 的 Cloud-IDE-JWT（代理抓不到时的兜底）。
#[tauri::command]
pub async fn proxy_capture_local() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(device_proxy::local_capture::capture_from_local)
        .await
        .map_err(|e| format!("本地捕获失败: {e}"))?
}

/// 清理上一次异常退出残留的系统代理设置。
#[tauri::command]
pub async fn proxy_cleanup_stale() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let store = config::store_dir();
        let restored = sys_proxy::cleanup_stale_local_proxy(&store);
        Ok(json!({ "ok": true, "restored": restored }))
    })
    .await
    .map_err(|e| format!("清理残留代理失败: {e}"))?
}

/// 把上游代理规格字符串解析成结构（供界面校验用户输入）。
#[tauri::command]
pub fn proxy_parse_upstream(spec: String) -> Value {
    match sys_proxy::upstream_from_capture(Some(spec)) {
        Some(upstream) => json!({ "ok": true, "addr": upstream.addr() }),
        None => json!({ "ok": false, "error": "无法解析代理地址，应形如 http://127.0.0.1:7890" }),
    }
}
