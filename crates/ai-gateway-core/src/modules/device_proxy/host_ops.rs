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
}
