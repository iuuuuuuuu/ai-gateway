//! 本地代理的**宿主无关**操作层：进程内单例 + 事件回调抽象。
//!
//! ## 为什么要从宿主里搬出来
//!
//! 这段逻辑原本只写在 Tauri 命令层（`src-tauri/src/commands_proxy.rs`），
//! webui（HTTP 服务形态）根本没有入口 —— 前端 `api.ts` 登记了 `/api/proxy/*`
//! 九条路由，服务端一条都没有，于是设置页的代理面板整块点了就是 404。
//!
//! 「一个进程只允许一个代理实例」这条约束与宿主无关，事件往哪送才是宿主的事。
//! 因此这里保留单例与全部编排顺序，只把「事件送哪」抽象成 [`HostProxySink`]。
//!
//! ## 为什么必须托管单例
//!
//! 代理会改写**系统代理设置**。若允许同一进程开两个实例，第二个会读到
//! 「第一个刚设好的」系统代理并把它当成「用户原有的代理」记下来 ——
//! 停止时就会把用户的真实设置覆盖成指向本地代理的地址，
//! 表现为「关掉应用后全网断网」。
//!
//! 注意这个单例是**每进程一份**：桌面端进程与 webui 服务进程各自持有一个，
//! 互不影响（也正因如此，两个进程同时启代理仍会互相抢系统代理设置，
//! 这点与改动前的桌面端语义一致，本次不扩大治理范围）。
//!
//! ## 启动/停止的顺序是硬约束
//!
//! [`start`] 里「先记下用户原有系统代理 → 再 set_win_proxy」的顺序、
//! [`stop`] 里「先 take 出实例并 stop()（置 intentional 标志）→ 再还原」的顺序
//! **不能调换**：前者调换会把自己刚设的值记成「用户原值」，
//! 后者调换会让看门狗与主动停止路径重复还原、覆盖用户刚改回去的设置。

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};

use super::{sys_proxy, ProxyConfig, ProxyEventSink, ProxyServer};
use crate::modules::config;

/// 进程内唯一的代理实例。
static PROXY: std::sync::OnceLock<tokio::sync::Mutex<Option<ProxyServer>>> =
    std::sync::OnceLock::new();

fn proxy_slot() -> &'static tokio::sync::Mutex<Option<ProxyServer>> {
    PROXY.get_or_init(|| tokio::sync::Mutex::new(None))
}

/// 宿主侧的事件接收方。
///
/// 与 [`ProxyEventSink`] 的区别：后者是 core 内部给 `ProxyServer` 用的回调形态，
/// 前者是「宿主打算把这些事件送到哪里」。分两层是为了让 core 能自己决定
/// 「意外退出时要不要还原系统代理」——那是安全语义，不该交给宿主各写一遍。
pub trait HostProxySink: Send + Sync {
    /// 一行操作日志。
    fn log(&self, line: &str);
    /// 捕获到一个账号。
    fn account_captured(&self, uid: &str) {
        let _ = uid;
    }
    /// 代理退出。`intentional = false` 表示意外崩溃，系统代理需要还原。
    fn crashed(&self, intentional: bool) {
        let _ = intentional;
    }
}

/// 丢弃全部事件的接收方（webui 轮询形态与测试）。
pub struct NullHostSink;

impl HostProxySink for NullHostSink {
    fn log(&self, _line: &str) {}
}

/// 把 [`HostProxySink`] 适配成 [`ProxyEventSink`]，并在此集中处理崩溃看门狗。
struct SinkAdapter {
    host: Arc<dyn HostProxySink>,
    store_dir: PathBuf,
}

impl ProxyEventSink for SinkAdapter {
    fn log(&self, line: &str) {
        self.host.log(line);
    }

    fn account_captured(&self, uid: &str) {
        self.host.account_captured(uid);
    }

    fn exited(&self, intentional: bool) {
        self.host.crashed(intentional);
        // 非主动退出 = 意外崩溃：系统代理还指向本地死端口，必须立刻还原，
        // 否则用户会看到「应用没关但网断了」。
        //
        // 主动停止路径**不**在这里还原 —— 由 [`stop`] 负责，重复还原会覆盖
        // 用户刚改回去的设置。
        if !intentional {
            let detail = match sys_proxy::restore_system_proxy_on_exit(true) {
                Ok(()) => "已还原系统代理".to_string(),
                Err(e) => format!("还原系统代理失败: {e}"),
            };
            let _ = &self.store_dir;
            eprintln!("[proxy] 代理意外退出，{detail}");
            self.host.log(&format!("代理意外退出，{detail}"));
        }
    }
}

/// 读取代理配置（端口、目标域名、上游代理）。
pub fn config_view() -> Value {
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
        .unwrap_or_else(|| crate::modules::device_proxy::DEFAULT_TARGETS.join(","));

    json!({
        "port": port,
        "domains": domains,
        "defaultDomains": crate::modules::device_proxy::DEFAULT_TARGETS.join(","),
        "lastPort": sys_proxy::last_proxy_port(&store),
        "existingSystemProxy": sys_proxy::get_existing_win_proxy(),
    })
}

/// 代理运行状态。
pub async fn status() -> Value {
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
/// [`status`] 只能告诉你**进程在跑**，不能告诉你：
///
///   · 端口是否真的在监听（进程活着但 bind 失败过）
///   · 系统代理是否真的指向了它（用户手工改过、或被别的软件覆盖）
///   · CA 证书是否已装（没装的话 HTTPS 全报证书错，而进程一切正常）
///
/// 这几件事**都会让用户觉得"代理坏了"**，而 `running: true` 一个都答不了。
/// 故本函数逐项检查，并给出**可直接行动**的结论。
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
///
/// # 为什么在 core 而不是只在 Tauri 命令层
///
/// 这段逻辑原本**只**写在 `src-tauri/src/commands_proxy.rs`，于是 webui（HTTP
/// 服务形态）下点设置页的「检测」按钮必然失败 —— 前端 `api.ts` 把它映射成
/// `proxy_check` 命令，而 `ROUTES` 里没有它，`httpCall` 直接抛
/// 「webui 模式暂不支持该操作: proxy_check」，界面把这条异常渲染成
/// 「检测失败：…」。桌面端走 Tauri IPC 所以看不出问题。
///
/// 这与 `proxy_test_upstream` / `proxy_probe_nodes` 是**同一个缺陷的第三次出现**
/// （见 `crates/ai-gateway-server/src/api/proxy_api.rs` 里那两条的注释）。
/// 因此这次不再"两边各写一份"，而是把唯一的实现放这里，Tauri 命令与 HTTP
/// 处理器都只做转发 —— 两处各写一份必然分叉，而分叉的错法表现为
/// 「桌面端说没问题、浏览器说检测失败」。
pub async fn check() -> Value {
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
    // ⚠ 路径口径与 [`cert_status`] **必须一致**（都是 `store_dir()/certs`）——
    // 两边各算一次迟早会算出不同的值，而那种错法表现为
    // "检测说没证书、生成按钮说已生成"。
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

    json!({
        "ok": all_ok,
        "checks": checks,
        // 检查时间：界面显示"上次检测于 …"，避免用户看着旧结果判断当前状态
        "checkedAt": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    })
}

/// 启动代理并接管系统代理。
pub async fn start(
    port: Option<u16>,
    domains: Option<String>,
    host: Arc<dyn HostProxySink>,
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
            .unwrap_or_else(|| crate::modules::device_proxy::DEFAULT_TARGETS.join(","))
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
        .with_events(Arc::new(SinkAdapter {
            host,
            store_dir: store.clone(),
        }))
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
pub async fn stop() -> Result<Value, String> {
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
pub fn cert_status() -> Result<Value, String> {
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
}

/// 生成自签 CA（若已存在则复用）。
pub fn cert_generate() -> Result<Value, String> {
    let certs = config::store_dir().join("certs");
    std::fs::create_dir_all(&certs).map_err(|e| format!("创建证书目录失败: {e}"))?;
    // 只关心「能不能拿到可用 CA」，不持有它 —— 真正使用时由
    // ProxyServer::start 内部再取一次（同一份文件，结果一致）。
    let _ = crate::modules::device_proxy::ca::ensure_ca(&certs)?;
    Ok(json!({
        "ok": true,
        "certsDir": certs.to_string_lossy(),
        "caCerPath": certs.join("ca.cer").to_string_lossy(),
    }))
}

/// 从本机离线捕获 Trae 的 Cloud-IDE-JWT（代理抓不到时的兜底）。
pub fn capture_local() -> Result<Value, String> {
    crate::modules::device_proxy::local_capture::capture_from_local()
}

/// 清理上一次异常退出残留的系统代理设置。
pub fn cleanup_stale() -> Result<Value, String> {
    let store = config::store_dir();
    let restored = sys_proxy::cleanup_stale_local_proxy(&store);
    Ok(json!({ "ok": true, "restored": restored }))
}

/// 把上游代理规格字符串解析成结构（供界面校验用户输入）。
pub fn parse_upstream(spec: &str) -> Value {
    match sys_proxy::upstream_from_capture(Some(spec.to_string())) {
        Some(upstream) => json!({ "ok": true, "addr": upstream.addr() }),
        None => json!({ "ok": false, "error": "无法解析代理地址，应形如 http://127.0.0.1:7890" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析上游代理规格() {
        let ok = parse_upstream("http://127.0.0.1:7890");
        assert_eq!(ok["ok"], true, "合法地址应解析成功：{ok}");
        assert_eq!(
            ok["addr"], "http://127.0.0.1:7890",
            "addr 原样返回规格串（scheme 在连接阶段再剥）"
        );

        // 只有**空串**会被判为无法解析：`parse_upstream` 把裸值一律当
        // `host:port`（端口缺失时在连接阶段用默认值补），因此这里不能拿
        // 「看起来不像地址」的字符串当失败用例。
        let empty = parse_upstream("");
        assert_eq!(empty["ok"], false, "空串应判为无法解析");
        assert!(
            empty["error"].is_string(),
            "失败时应给出可读原因：{empty}"
        );
    }

    #[test]
    fn null_host_sink_吞掉全部事件() {
        let sink = NullHostSink;
        sink.log("x");
        sink.account_captured("u");
        sink.crashed(false);
    }

    #[test]
    fn 配置视图含界面需要的全部字段() {
        let v = config_view();
        for key in [
            "port",
            "domains",
            "defaultDomains",
            "lastPort",
            "existingSystemProxy",
        ] {
            assert!(v.get(key).is_some(), "配置视图缺少字段 {key}");
        }
    }

    /// ★ 钉住 webui 缺陷的修复：`check()` 必须**在 core 里可调用**，
    /// 且返回前端 `proxyCheck()` 声明的那三个字段。
    ///
    /// 修复前这段逻辑只存在于 `src-tauri/src/commands_proxy.rs`，
    /// HTTP 服务形态没有入口 ⇒ 浏览器里点「检测」永远失败。
    /// 这条测试保证它不会再被搬回宿主层（搬回去就编译不过）。
    ///
    /// ⚠ 这是**不触网、不改系统设置**的只读检查：它读配置里的端口、
    /// 尝试一次本地 TCP 连接（3s 超时）、读注册表里的系统代理、
    /// 探测证书文件。测试里代理必然未运行，所以只断言**形状**，
    /// 不断言具体某一项的 ok —— 那取决于跑测试的机器。
    #[tokio::test]
    async fn 检测返回界面需要的三个字段且每项形状正确() {
        let v = check().await;

        assert!(
            v.get("ok").and_then(Value::as_bool).is_some(),
            "ok 必须是布尔（界面据此显示绿/红）：{v}"
        );
        assert!(
            v.get("checkedAt").and_then(Value::as_u64).is_some_and(|t| t > 0),
            "checkedAt 必须是正的 Unix 毫秒（界面显示「上次检测于 …」）：{v}"
        );

        let checks = v
            .get("checks")
            .and_then(Value::as_array)
            .expect("checks 必须是数组");
        assert_eq!(
            checks.len(),
            4,
            "检查项固定四项（代理进程 / 端口监听 / 系统代理 / CA 证书）：{v}"
        );
        for c in checks {
            assert!(
                c.get("name").and_then(Value::as_str).is_some_and(|s| !s.is_empty()),
                "每项必须有非空 name：{c}"
            );
            assert!(
                c.get("ok").and_then(Value::as_bool).is_some(),
                "每项必须有布尔 ok：{c}"
            );
            assert!(
                c.get("detail")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty()),
                "每项必须有非空 detail —— 界面直接显示它，空串等于没说：{c}"
            );
        }
    }

    /// `ok` 的口径：**所有检查项都通过**才为 true。
    ///
    /// 这条把「没有失败的」那种含糊口径钉死：构造一个必有一项失败的场景
    /// （代理未运行 ⇒「代理进程」项必失败），断言 `ok == false`。
    #[tokio::test]
    async fn 只要有任一项不通过则总判定为不通过() {
        let v = check().await;
        let all_ok = v["checks"]
            .as_array()
            .expect("checks 是数组")
            .iter()
            .all(|c| c["ok"].as_bool().unwrap_or(false));

        assert_eq!(
            v["ok"].as_bool(),
            Some(all_ok),
            "ok 必须等于「所有项都通过」，不能是别的口径：{v}"
        );
    }
}
