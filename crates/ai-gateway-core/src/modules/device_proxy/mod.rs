//! Rust 版 MITM 设备身份代理（`device_proxy.py` 迁移，P4）。
//!
//! 架构：hyper 1.x 协议栈自建 —— 为什么不用 hudsucker：其非拦截 CONNECT 隧道
//! 硬编码直连，无法透传用户 VPN 上游（「开代理后外网打不开」的根因）。
//!
//! 模块划分：`ca` 证书签发 / `logger` 请求日志（脱敏）/ `upstream` 上游连接（VPN 链式）/
//! `handler` MITM 改写 / `ws` 桥接 / `bypass` OAuth 直连豁免 / `sys_proxy` 系统代理编排 /
//! `local_capture` 本地登录态兜底捕获。
//! 本文件：代理生命周期（[`ProxyServer`]）+ 主循环（accept → CONNECT 分流 / 明文转发）。
//!
//! ## 与上游（TraeWorkAssistant）的差异（都是本仓库的约定，不是行为变更）
//!
//! - **不依赖 Tauri**：前端事件（`proxy-log` / `account-captured` / `proxy-crashed`）
//!   收敛为 [`ProxyEventSink`] 回调，由宿主自行转发 —— 与
//!   [`crate::modules::switcher::ProgressSink`] 同款约定。
//! - **数据目录来自 [`crate::modules::config::store_dir`]**（尊重 `AI_GATEWAY_HOME`），
//!   不硬编码 `%APPDATA%`；账号 / 凭证 / 设备表一律经既有模块读写（见 `handler`）。
//! - 账号库用本仓库的 `trae_accounts.json`（`user_id` 键），设备指纹用
//!   `trae_device::device_map.json`（抓包真值优先于派生值）。

pub mod bypass;
pub mod ca;
pub mod handler;
pub mod local_capture;
pub mod logger;
pub mod sys_proxy;
pub mod upstream;
pub mod ws;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use http_body_util::{Full, Limited};
use hyper::body::Bytes;
use hyper::header::{HeaderName, HeaderValue};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;

use crate::modules::config;
use crate::modules::device_proxy::ca::{ensure_ca, CaAuthority};
use crate::modules::device_proxy::handler::{serve_mitm, HOP_BY_HOP_REQ, ProxyCtx};
use crate::modules::device_proxy::logger::{ProxyLog, RequestLogger};
use crate::modules::device_proxy::upstream::{
    connect_direct, connect_via_upstream, UpstreamConnector, UpstreamProxy,
};

/// 建连/首读超时（对齐 Python `_CONN_TIMEOUT`）
const CONN_TIMEOUT: Duration = Duration::from_secs(300);
/// 明文转发上游超时（对齐 Python handle_plain 的 socket timeout=30s）
const PLAIN_TIMEOUT: Duration = Duration::from_secs(30);
/// 分发阶段头缓冲上限（Python 无上限仅靠超时兜底，此处防御性 64KB）
const MAX_DISPATCH_HEAD: usize = 64 * 1024;
/// 并发连接上限（对齐 Python `_CONN_SEMAPHORE` 信号量 128）
const MAX_CONNS: usize = 128;
/// CONNECT 200 应答后的 TLS 握手超时（客户端不发 ClientHello 时及时释放连接与并发槽）
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// 默认监听域名（对齐 Python `TARGET_DOMAINS`；宿主可用 [`ProxyConfig::targets`] 覆盖）
pub const DEFAULT_TARGETS: &[&str] = &[
    "trae.cn",
    "trae.com.cn",
    "mchost.guru",
    "zijieapi.com",
    "bytedance.com",
    "volcengine.com",
    "volces.com",
    "treecode.com",
    "doubao.com",
];

// ---------------- 事件桥接（替代 Tauri app.emit） ----------------

/// 代理事件接收器（宿主注入：桌面端转发为 Tauri 事件，HTTP server / CLI 写日志或丢弃）。
///
/// 与 [`crate::modules::switcher::ProgressSink`] 同款约定：core 只认这个 trait，
/// 不认 Tauri —— 这样同一份代理逻辑能被桌面端、server、CLI 三种宿主复用。
pub trait ProxyEventSink: Send + Sync {
    /// 一行操作日志（对应上游 `proxy-log` 事件）。
    fn log(&self, line: &str);

    /// 捕获到一个账号（对应上游 `account-captured` 事件；默认忽略）。
    fn account_captured(&self, uid: &str) {
        let _ = uid;
    }

    /// 代理任务退出（对应上游 `proxy-crashed` 看门狗；默认忽略）。
    ///
    /// `intentional = true` 表示调用方主动 [`ProxyServer::stop`] 过；`false` 表示
    /// accept 循环自己结束了（意外崩溃 / panic）。**只有 `false` 才需要还原系统代理** ——
    /// 主动停止路径由宿主自己还原，重复还原会覆盖用户刚改回去的设置。
    fn exited(&self, intentional: bool) {
        let _ = intentional;
    }
}

/// 丢弃全部事件的接收器（测试与静默场景）。
pub struct NullSink;

impl ProxyEventSink for NullSink {
    fn log(&self, _: &str) {}
}

/// 只写 stderr 的接收器（CLI / 后台任务）。
pub struct LogSink;

impl ProxyEventSink for LogSink {
    fn log(&self, line: &str) {
        eprintln!("[proxy] {line}");
    }
}

/// 记录到内存的接收器（供调用方把日志与捕获事件回传给前端 / 断言）。
#[derive(Default)]
pub struct VecSink {
    lines: std::sync::Mutex<Vec<String>>,
    captured: std::sync::Mutex<Vec<String>>,
    exited: std::sync::Mutex<Vec<bool>>,
}

impl VecSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// 全部操作日志行。
    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().map(|v| v.clone()).unwrap_or_default()
    }

    /// 全部捕获到的 uid。
    pub fn captured(&self) -> Vec<String> {
        self.captured.lock().map(|v| v.clone()).unwrap_or_default()
    }

    /// 退出通知（主动停止 / 崩溃各记一次）。
    pub fn exits(&self) -> Vec<bool> {
        self.exited.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

impl ProxyEventSink for VecSink {
    fn log(&self, line: &str) {
        if let Ok(mut v) = self.lines.lock() {
            v.push(line.to_string());
        }
    }

    fn account_captured(&self, uid: &str) {
        if let Ok(mut v) = self.captured.lock() {
            v.push(uid.to_string());
        }
    }

    fn exited(&self, intentional: bool) {
        if let Ok(mut v) = self.exited.lock() {
            v.push(intentional);
        }
    }
}

// ---------------- 生命周期 ----------------

/// 代理启动配置（由宿主从设置页构造）。
///
/// [`ProxyConfig::new`] 按本仓库约定从 [`config::store_dir`] 推导全部路径；
/// 字段全部 `pub`，宿主可逐项覆盖（对齐上游「设置页可改端口 / 域名 / 日志目录」的语义）。
#[derive(Clone)]
pub struct ProxyConfig {
    /// 监听端口（恒为 127.0.0.1）
    pub port: u16,
    /// 监听域名列表（后缀匹配，构造时统一小写；空则用 [`DEFAULT_TARGETS`]）
    pub targets: Vec<String>,
    /// 自动捕获 JWT 写回账号库（默认开）
    pub auto_capture_jwt: bool,
    /// 数据根目录（本仓库恒为 [`config::store_dir`]）
    pub data_dir: PathBuf,
    /// data/certs（CA 目录）
    pub certs_dir: PathBuf,
    /// logs/proxy.log 操作日志
    pub log_path: PathBuf,
    /// 代理请求抓包日志目录（按日滚动 + 100MB 切分）
    pub req_log_dir: PathBuf,
    /// 上游代理（用户 VPN 梯子；非目标流量经此转出，失败回退直连）
    pub upstream: Option<UpstreamProxy>,
    /// 事件接收器（`None` 等价于 [`NullSink`]）
    pub events: Option<Arc<dyn ProxyEventSink>>,
}

impl std::fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("port", &self.port)
            .field("targets", &self.targets)
            .field("auto_capture_jwt", &self.auto_capture_jwt)
            .field("data_dir", &self.data_dir)
            .field("certs_dir", &self.certs_dir)
            .field("log_path", &self.log_path)
            .field("req_log_dir", &self.req_log_dir)
            .field("upstream", &self.upstream)
            .field("events", &self.events.is_some())
            .finish()
    }
}

impl ProxyConfig {
    /// 按本仓库数据目录约定构造配置（端口 + 默认域名，其余全默认）。
    pub fn new(port: u16) -> Self {
        let store = config::store_dir();
        Self {
            port,
            targets: Vec::new(),
            auto_capture_jwt: true,
            certs_dir: store.join("certs"),
            log_path: store.join("logs").join("proxy.log"),
            req_log_dir: store.join("logs"),
            data_dir: store,
            upstream: None,
            events: None,
        }
    }

    /// 注入事件接收器（链式）。
    pub fn with_events(mut self, sink: Arc<dyn ProxyEventSink>) -> Self {
        self.events = Some(sink);
        self
    }

    /// 注入上游代理（用户 VPN 梯子）。
    pub fn with_upstream(mut self, upstream: Option<UpstreamProxy>) -> Self {
        self.upstream = upstream;
        self
    }

    /// 设置监听域名（逗号分隔，空白项丢弃；空则回落 [`DEFAULT_TARGETS`]）。
    pub fn with_targets_csv(mut self, csv: &str) -> Self {
        self.targets = csv
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        self
    }
}

/// 运行中的代理句柄。
///
/// drop shutdown 发送端即触发停止（`changed()` 出错分支），但建议显式调用
/// [`ProxyServer::stop`] 并按需等待端口释放。
pub struct ProxyServer {
    pub port: u16,
    /// 已捕获账号计数（宿主状态面板展示）
    pub captured: Arc<AtomicI64>,
    shutdown_tx: watch::Sender<bool>,
    exit_rx: watch::Receiver<bool>,
    task: JoinHandle<()>,
}

impl ProxyServer {
    /// 启动进程内代理（对齐 Python `main()`：ensure_ca → 设备标识同步 → 独占绑定 → accept 循环）
    pub async fn start(cfg: ProxyConfig) -> Result<ProxyServer, String> {
        // CA 证书：兼容历史 RSA CA；缺失则生成（数据目录布局不变）
        let ca = Arc::new(ensure_ca(&cfg.certs_dir)?);

        let captured = Arc::new(AtomicI64::new(0));
        let log = ProxyLog::new(
            cfg.log_path.clone(),
            cfg.events.clone(),
            Arc::clone(&captured),
        );
        let req_logger = Arc::new(RequestLogger::new(cfg.req_log_dir.clone()));
        let targets = if cfg.targets.is_empty() {
            DEFAULT_TARGETS.iter().map(|s| s.to_string()).collect()
        } else {
            cfg.targets.iter().map(|d| d.to_ascii_lowercase()).collect()
        };
        let ctx = Arc::new(ProxyCtx {
            log: log.clone(),
            req_logger,
            targets,
            auto_capture_jwt: cfg.auto_capture_jwt,
            data_dir: cfg.data_dir.clone(),
        });

        // 升级历史假占位符设备标识（对齐 Python sync_account_devices，仅自动捕获开启时）
        if ctx.auto_capture_jwt {
            sync_account_devices(&ctx);
        }

        // Windows 独占绑定（SO_EXCLUSIVEADDRUSE，issue #7：防孤儿进程「假启动」）
        let listener = bind_listener(cfg.port).await?;

        // 启动横幅（对齐 Python main() 的日志行，宿主实时代理面板直接可读）
        log.log(&format!(
            "代理已启动: 127.0.0.1:{}  (TRAE 多域 MITM 拦截 + JWT 自动捕获)",
            cfg.port
        ));
        let list: Vec<String> = ctx.targets.iter().map(|d| format!("*.{d}")).collect();
        log.log(&format!("监听 TRAE 域名: {}", list.join(", ")));
        log.log("  → 命中上述域名的请求会在面板中以 [TRAE] 标记；JWT 捕获不限 host（兼容未列出的子域）");
        log.log("  → 未在监听域名列表中的请求将透明转发（不记录日志），不影响其他 App 正常上网");
        log.log(&format!("accounts: {}", cfg.data_dir.join("trae_accounts.json").display()));
        log.log(&format!("代理请求日志: {} (100MB 滚动)", cfg.req_log_dir.display()));
        log.log(&format!(
            "自动捕获 JWT 写回账号库: {}",
            if cfg.auto_capture_jwt { "开" } else { "关" }
        ));
        if let Some(up) = &cfg.upstream {
            log.log(&format!("上游代理(用户VPN)透传: {}", up.addr()));
        }
        log.log("请把 CA 证书 certs/ca.cer 安装到 Windows 受信任根证书颁发机构(管理员)。");

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        // 退出通知：accept 循环结束（无论主动 stop 还是意外崩溃）时置 true，
        // 供宿主看门狗监听并还原系统代理（对齐 Python 版 stdout EOF 看门狗语义）。
        //
        // 同时记录「是否主动停止」并随退出事件一并上报：宿主据此区分
        // 「用户点了停止（宿主自己还原）」与「代理崩了（必须立刻还原，
        // 否则系统代理仍指向死端口 → 本机全局断网）」。看门狗拿到的信号
        // 与 `intentional` 之间存在极小的竞态窗口，但两种情况下的处置都是
        // 「还原系统代理」，误判代价仅为一次冗余还原，可接受。
        let (exit_tx, exit_rx) = watch::channel(false);
        let events = cfg.events.clone();
        let shutdown_probe = shutdown_tx.clone();
        let task = tokio::spawn(async move {
            accept_loop(listener, ctx, ca, cfg.upstream.clone(), shutdown_rx).await;
            let _ = exit_tx.send(true);
            if let Some(sink) = events {
                sink.exited(shutdown_probe.borrow().to_owned());
            }
        });
        Ok(ProxyServer { port: cfg.port, captured, shutdown_tx, exit_rx, task })
    }

    /// 主动停止：accept 循环退出并中止所有在途连接任务
    pub fn stop(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// 任务退出通知（主动 stop 与意外崩溃均会触发；调用方结合「主动停止」标记区分）
    pub fn exit_signal(&self) -> watch::Receiver<bool> {
        self.exit_rx.clone()
    }

    /// 代理任务是否仍在运行（意外崩溃时为 false，供看门狗判定）
    pub fn is_running(&self) -> bool {
        !self.task.is_finished()
    }

    /// 已捕获账号数。
    pub fn captured_count(&self) -> i64 {
        self.captured.load(Ordering::Relaxed)
    }
}

// ---------------- 主循环 ----------------

async fn accept_loop(
    listener: TcpListener,
    ctx: Arc<ProxyCtx>,
    ca: Arc<CaAuthority>,
    upstream: Option<UpstreamProxy>,
    mut shutdown: watch::Receiver<bool>,
) {
    let permits = Arc::new(Semaphore::new(MAX_CONNS));
    // 明文转发双 Client（路由对齐 Python handle_plain）：
    // - plain_client：非目标域名 http 请求经用户 VPN 上游（http 代理绝对形式 / SOCKS5 隧道，
    //   失败回退直连），无上游配置时即直连
    // - direct_client：目标域名 / https 明文请求一律直连（Trae 域国内可达，
    //   Python 版上游仅服务非目标域名 http，不把 TRAE 流量绕行用户梯子）
    let plain_client: Client<UpstreamConnector, Full<Bytes>> =
        Client::builder(TokioExecutor::new()).build(UpstreamConnector::new(upstream.clone(), ctx.log.clone()));
    let direct_client: Client<UpstreamConnector, Full<Bytes>> =
        Client::builder(TokioExecutor::new()).build(UpstreamConnector::new(None, ctx.log.clone()));
    let mut conns: Vec<JoinHandle<()>> = Vec::new();
    // 空闲期定时回收已结束的连接句柄（审查修复：conns 仅在新 accept 时清理，
    // 长连接高频场景下已完成任务的 JoinHandle 会随 Vec 无界增长）
    let mut reap = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            // stop() 或 ProxyServer 整体 drop（发送端析构 → changed() 报错）都会触发退出
            _ = shutdown.changed() => break,
            _ = reap.tick() => {
                conns.retain(|h| !h.is_finished());
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    // 并发超限（try_acquire 失败）直接关闭新连接，保证代理自身不被打挂。
                    // permit 必须移入任务、持有至连接结束（审查修复：原 try_acquire()
                    // 临时值语句结束即析构，MAX_CONNS 上限完全失效、过载分支不可达）
                    let permit = match Arc::clone(&permits).try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => {
                            ctx.log.log(&format!(
                                "[overload] 并发连接已达上限 {MAX_CONNS}，拒绝来自 {peer} 的新连接"
                            ));
                            continue;
                        }
                    };
                    conns.retain(|h| !h.is_finished());
                    let task_ctx = Arc::clone(&ctx);
                    let task_ca = Arc::clone(&ca);
                    let task_up = upstream.clone();
                    let task_client = plain_client.clone();
                    let task_direct = direct_client.clone();
                    conns.push(tokio::spawn(async move {
                        let _guard = permit; // 释放即归还信号量
                        handle_conn(stream, peer, task_ctx, task_ca, task_up, task_client, task_direct).await;
                    }));
                }
                Err(e) => {
                    // 单条 accept 出错不应让整个代理退出（否则系统代理仍指向死端口）。
                    // 记录后短暂退避再重试，保持服务可用。
                    ctx.log.log(&format!("[accept] 异常(已忽略并重试): {e}"));
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        }
    }
    // 停止：中止所有在途连接任务（对齐 Python「进程被杀」的停止语义）
    for h in conns {
        h.abort();
    }
    ctx.log.log("代理已停止");
}

/// 单连接分发（对齐 Python `handle_client`）：
/// - CONNECT + 目标域名 → 200 应答 → TLS(叶子证书) → [`serve_mitm`] 解密改写
/// - CONNECT + 其他域名 → [`tunnel_raw`] 透明隧道（不解密不记日志）
/// - 其余（明文 HTTP）→ [`handle_plain`] 转发
async fn handle_conn(
    mut stream: TcpStream,
    peer: SocketAddr,
    ctx: Arc<ProxyCtx>,
    ca: Arc<CaAuthority>,
    upstream: Option<UpstreamProxy>,
    plain_client: Client<UpstreamConnector, Full<Bytes>>,
    direct_client: Client<UpstreamConnector, Full<Bytes>>,
) {
    let head = match timeout(CONN_TIMEOUT, read_head(&mut stream)).await {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            ctx.log.log(&format!("[client] {peer} 读头失败: {e}"));
            return;
        }
        Err(_) => {
            ctx.log.log(&format!("[client] {peer} 读头超时 ({}s)", CONN_TIMEOUT.as_secs()));
            return;
        }
    };
    let first = String::from_utf8_lossy(head.split(|&b| b == b'\n').next().unwrap_or(b""))
        .trim()
        .to_string();
    let method = first.split(' ').next().unwrap_or("").to_ascii_uppercase();
    if method == "CONNECT" {
        // CONNECT host:port HTTP/1.1
        let target = first.split(' ').nth(1).unwrap_or("");
        let (host, port) = match target.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(443)),
            None => (target.to_string(), 443),
        };
        // 审查修复：read_head 按块读会超读 head 之后的字节（客户端在 200 应答前
        // 抢发的 ClientHello / pipelined 数据），必须回放给后续 TLS 握手/隧道，
        // 否则握手从空流开始将挂死（明文路径已用 init 注入，此处此前被直接丢弃）
        let overflow = head_after_head_end(&head);
        let mut client = PrefixedStream::new(stream, overflow);
        if !ctx.host_in_targets(&host) {
            tunnel_raw(client, &host, port, &upstream, &ctx.log).await;
            return;
        }
        let matched = ctx
            .targets
            .iter()
            .find(|d| **d == host || host.ends_with(&format!(".{d}")))
            .map(|s| s.as_str())
            .unwrap_or("?");
        // 先握手后记日志（与 Python 一致）：避免日志/证书等任何异常把 CONNECT
        // 握手拖死导致客户端 EOF（issue #7）
        if client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.is_err() {
            return;
        }
        ctx.log.log(&format!("CONNECT {host}:{port}  [TRAE/MITM] 匹配域名: {matched}"));
        let acceptor = TlsAcceptor::from(ca.gen_server_config(&host));
        // 握手超时（审查修复：原无超时，客户端不发 ClientHello 时任务永久挂起）
        match timeout(HANDSHAKE_TIMEOUT, acceptor.accept(client)).await {
            Ok(Ok(tls)) => serve_mitm(tls, host, port, ctx).await,
            Ok(Err(e)) => ctx.log.log(&format!("  [MITM] TLS 握手失败 {host}:{port}: {e}")),
            Err(_) => ctx.log.log(&format!(
                "  [MITM] TLS 握手超时 ({}s) {host}:{port}",
                HANDSHAKE_TIMEOUT.as_secs()
            )),
        }
    } else {
        // 明文 HTTP 请求：全部转发（日志由 handle_plain 内部按目标域名控制）
        handle_plain(stream, head, &plain_client, &direct_client, &ctx).await;
    }
}

/// 分发阶段读请求头（到 \r\n\r\n 或 EOF；EOF 时返回已有内容交由上层判路由，对齐 Python）
async fn read_head<S: AsyncRead + Unpin>(s: &mut S) -> Result<Vec<u8>, String> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        if handler::find_head_end(&buf).is_some() {
            return Ok(buf);
        }
        let n = s.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(buf);
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_DISPATCH_HEAD {
            return Err(format!("请求头超过分发缓冲上限 ({MAX_DISPATCH_HEAD})"));
        }
    }
}

/// 非目标域名 CONNECT 的透明隧道（对齐 Python `tunnel_raw`）：
/// 上游代理（用户 VPN）优先，失败回退直连；全程不解密，仅记隧道级日志。
/// client 为 [`PrefixedStream`]（分发阶段超读字节的回放见 handle_conn 注释）。
async fn tunnel_raw<S>(
    mut client: S,
    host: &str,
    port: u16,
    upstream: &Option<UpstreamProxy>,
    log: &ProxyLog,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut pre: Option<TcpStream> = None;
    if let Some(up) = upstream {
        match connect_via_upstream(host, port, up).await {
            Ok(s) => {
                log.log(&format!("  [raw-tunnel] 经上游代理 {} 建立隧道 {host}:{port}", up.addr()));
                pre = Some(s);
            }
            Err(e) => log.log(&format!("  [raw-tunnel] 上游代理连接失败({e})，回退直连")),
        }
    }
    let mut remote = match pre {
        Some(r) => r,
        None => match connect_direct(host, port).await {
            Ok(s) => s,
            Err(_) => {
                // 上游不可达：明确告知客户端，避免浏览器无限等待
                let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                return;
            }
        },
    };
    // 完成 CONNECT 握手：先回 200，客户端随后才会发送 TLS ClientHello
    if client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.is_err() {
        return;
    }
    // 双向裸转发（copy_bidirectional 自带半关闭传播，对齐 Python pipe + shutdown(SHUT_WR)）
    let _ = tokio::io::copy_bidirectional(&mut client, &mut remote).await;
}

/// 明文 HTTP 转发（对齐 Python `handle_plain`）：
/// 请求行为代理形式绝对 URL；仅「非目标域名 + http」经上游 VPN 转发（hyper 绝对形式），
/// 目标域名 / https 明文请求一律直连（Python 版同款路由，不把 TRAE 流量绕行用户梯子）。
/// 仅目标域名记操作日志与抓包日志；单请求后关闭连接（对齐 Python 语义）。
async fn handle_plain(
    mut stream: TcpStream,
    head: Vec<u8>,
    plain_client: &Client<UpstreamConnector, Full<Bytes>>,
    direct_client: &Client<UpstreamConnector, Full<Bytes>>,
    ctx: &ProxyCtx,
) {
    // 分发阶段已读取的字节作为初始缓冲注入（可能含 body 前缀）
    let init = bytes::BytesMut::from(&head[..]);
    let Some(req) = (match timeout(CONN_TIMEOUT, handler::read_raw_request_buf(&mut stream, init)).await {
        Ok(Ok(Some(r))) => Some(r),
        _ => None,
    }) else {
        return;
    };

    let Ok(uri) = req.path.parse::<hyper::Uri>() else {
        let _ = handler::send_response(&mut stream, 400, "Bad Request", &[], b"Bad Request").await;
        return;
    };
    let Some(host) = uri.host().map(str::to_string) else {
        let _ = handler::send_response(&mut stream, 400, "Bad Request", &[], b"Bad Request").await;
        return;
    };
    let scheme = uri.scheme_str().unwrap_or("http").to_string();
    let default_port = if scheme == "https" { 443 } else { 80 };
    let port = uri.port_u16().unwrap_or(default_port);
    let path = uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let is_target = ctx.host_in_targets(&host);
    if is_target {
        ctx.log.log(&format!("  [plain] {} {scheme}://{host}:{port}{path}", req.method));
    }
    // 上游路由对齐 Python：仅「非目标域名 + http」经用户 VPN（失败回退直连）；
    // 目标域名 / https 明文请求直连
    let client = if !is_target && scheme == "http" { plain_client } else { direct_client };

    // 组装上游请求：过滤跳过头（host/content-length 由 hyper 依 URI/body 重写）
    let mut builder = Request::builder().method(req.method.as_str()).uri(uri.clone());
    for (k, v) in &req.headers {
        if HOP_BY_HOP_REQ.iter().any(|h| k.eq_ignore_ascii_case(h)) {
            continue;
        }
        if let (Ok(name), Ok(val)) = (k.parse::<HeaderName>(), v.parse::<HeaderValue>()) {
            builder = builder.header(name, val);
        }
    }
    // Python：GET 请求不带 body
    let body = if req.method.eq_ignore_ascii_case("GET") { Bytes::new() } else { req.body.clone() };
    let request = builder.body(Full::new(body)).expect("plain upstream request build");

    let resp = match timeout(PLAIN_TIMEOUT, client.request(request)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            if is_target {
                ctx.log.log(&format!("  [plain] 错误: {e} (host={host}, path={path})"));
            }
            let _ = handler::send_response(&mut stream, 502, "Bad Gateway", &[], b"Bad Gateway").await;
            return;
        }
        Err(_) => {
            if is_target {
                ctx.log.log(&format!(
                    "  [plain] 错误: 上游超时 ({}s) (host={host}, path={path})",
                    PLAIN_TIMEOUT.as_secs()
                ));
            }
            let _ = handler::send_response(&mut stream, 504, "Gateway Timeout", &[], b"Gateway Timeout").await;
            return;
        }
    };

    let status = resp.status().as_u16();
    let reason = handler::reason_phrase(status);
    let resp_pairs: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).to_string()))
        .collect();
    // 非流式整体缓冲：Limited 上限 + 逐帧空闲超时（差异修复：对齐 Python handle_plain
    // 30s socket timeout 的逐读语义，防 trickling body 挂住连接）
    match handler::collect_body_with_idle_timeout(
        Limited::new(resp.into_body(), handler::MAX_RESP_BODY),
        PLAIN_TIMEOUT,
    )
    .await
    {
        Ok(resp_body) => {
            if is_target {
                ctx.log.log(&format!(
                    "  [plain] <- {status} {reason} ({}) bytes from {host}{path}",
                    resp_body.len()
                ));
            }
            if handler::send_response(&mut stream, status, reason, &resp_pairs, &resp_body)
                .await
                .is_err()
            {
                return;
            }
            // 仅目标域名记录到代理请求日志（对齐 Python handle_plain）
            if is_target {
                ctx.req_logger.log_request(
                    &req.method,
                    &host,
                    &path,
                    &req.headers,
                    &req.body,
                    status,
                    reason,
                    &resp_pairs,
                    &resp_body,
                );
            }
        }
        Err(e) => {
            if is_target {
                ctx.log.log(&format!("  [plain] 错误: 读上游响应失败: {e} (host={host}, path={path})"));
            }
            let _ = handler::send_response(&mut stream, 502, "Bad Gateway", &[], b"Bad Gateway").await;
        }
    }
}

// ---------------- 启动辅助 ----------------

/// 提取请求头之后超读的字节（客户端在 CONNECT 200 应答前抢发的 ClientHello /
/// pipelined 数据），供 [`PrefixedStream`] 回放给后续 TLS 握手/隧道
fn head_after_head_end(head: &[u8]) -> Vec<u8> {
    match handler::find_head_end(head) {
        Some(pos) => head[pos + 4..].to_vec(),
        None => Vec::new(),
    }
}

/// 带前缀缓冲的流：读操作先耗尽前缀（分发阶段超读字节的回放）再透传底层流，
/// 写操作直接透传。审查修复：此前超读字节被直接丢弃，TLS 握手从空流开始会挂死。
struct PrefixedStream<S> {
    inner: S,
    prefix: Vec<u8>,
    pos: usize,
}

impl<S> PrefixedStream<S> {
    fn new(inner: S, prefix: Vec<u8>) -> Self {
        Self { inner, prefix, pos: 0 }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.pos < this.prefix.len() {
            let n = (this.prefix.len() - this.pos).min(buf.remaining());
            buf.put_slice(&this.prefix[this.pos..this.pos + n]);
            this.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// 把账号库中各账号的设备指纹刷新为当前算法派生值
///（对齐 Python `sync_account_devices`：升级历史记录中由旧算法生成的假占位符
/// device_id 全 '2' / session_id 全 '5'；仅当确实需要变更时才写盘）。
///
/// 本仓库设备指纹独立存放在 `device_map.json`（见 [`crate::modules::trae_device`]）：
/// - 表内**缺失** → [`trae_device::resolve_device`] 派生并落盘；
/// - 表内是**旧占位符** → [`trae_device::reset_device_for`] 强制重派生；
/// - 表内是真值（MITM 抓包写入）→ 原样保留，抓到的真值比派生值可靠。
pub fn sync_account_devices(ctx: &ProxyCtx) {
    let _g = handler::accounts_lock().lock().unwrap_or_else(|e| e.into_inner());
    let map = crate::modules::trae_device::load_device_map();
    let mut refreshed = 0usize;
    for acc in crate::modules::trae_account::load_accounts() {
        let Some(uid) = acc.get("user_id").and_then(|v| v.as_str()) else {
            continue;
        };
        if uid.trim().is_empty() {
            continue;
        }
        match map.get(uid) {
            None => {
                crate::modules::trae_device::resolve_device(uid);
                refreshed += 1;
            }
            Some(entry) if is_placeholder_device(entry) => {
                crate::modules::trae_device::reset_device_for(uid);
                refreshed += 1;
            }
            Some(_) => {}
        }
    }
    if refreshed > 0 {
        ctx.log
            .log(&format!("  [sync] 已刷新 {refreshed} 个账号的设备标识（旧算法占位符/缺失）"));
    }
}

/// 判定设备表条目是否为旧算法的假占位符（device_id 全 '2' 或 session_id 全 '5'）。
fn is_placeholder_device(entry: &serde_json::Value) -> bool {
    let all_same = |v: Option<&str>, ch: char| {
        v.map(|s| !s.is_empty() && s.chars().all(|c| c == ch)).unwrap_or(false)
    };
    all_same(entry.get("device_id").and_then(|v| v.as_str()), '2')
        || all_same(entry.get("session_id").and_then(|v| v.as_str()), '5')
}

/// Windows：WSASocketW + SO_EXCLUSIVEADDRUSE 独占绑定（选项必须在 bind 前设置，
/// 对齐 Python `srv.setsockopt(SOL_SOCKET, SO_EXCLUSIVEADDRUSE, 1)`，issue #7）；
/// 其他平台：常规绑定（不设 SO_REUSEADDR，同样拒绝同端口重复绑定）。
///
/// 这里直接 `extern "system"` 声明 ws2_32 的四个函数而不是引 windows crate：
/// 本 crate 的 `windows` 依赖没有开 `Win32_Networking_WinSock` feature，
/// 而为了一个 setsockopt 去开一整块 WinSock 绑定会牵动全工作区的 feature 统一。
#[cfg(target_os = "windows")]
fn bind_exclusive(port: u16) -> Result<std::net::TcpListener, String> {
    use std::os::windows::io::FromRawSocket;

    #[repr(C)]
    struct SockAddrIn {
        sin_family: u16,
        sin_port: u16,
        sin_addr: [u8; 4],
        sin_zero: [u8; 8],
    }

    #[link(name = "ws2_32")]
    extern "system" {
        fn WSASocketW(af: i32, ty: i32, protocol: i32, info: *const u8, group: u32, flags: u32) -> usize;
        fn setsockopt(s: usize, level: i32, optname: i32, optval: *const u8, optlen: i32) -> i32;
        fn bind(s: usize, name: *const SockAddrIn, namelen: i32) -> i32;
        fn listen(s: usize, backlog: i32) -> i32;
        fn closesocket(s: usize) -> i32;
        fn WSAGetLastError() -> i32;
    }

    const AF_INET: i32 = 2;
    const SOCK_STREAM: i32 = 1;
    const IPPROTO_TCP: i32 = 6;
    const WSA_FLAG_OVERLAPPED: u32 = 0x01;
    const INVALID_SOCKET: usize = usize::MAX;
    const SOL_SOCKET: i32 = 0xffff;
    /// WinSock 头文件里就是 `(~SO_REUSEADDR)`，即 -5
    const SO_EXCLUSIVEADDRUSE: i32 = -5;

    unsafe {
        let sock = WSASocketW(
            AF_INET,
            SOCK_STREAM,
            IPPROTO_TCP,
            std::ptr::null(),
            0,
            WSA_FLAG_OVERLAPPED,
        );
        if sock == INVALID_SOCKET {
            return Err(format!("创建监听 socket 失败: WSA错误 {}", WSAGetLastError()));
        }
        // 独占绑定：多个 socket 绑定同一端口将明确失败（Python 版同款修复）
        let on: u32 = 1;
        if setsockopt(
            sock,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            &on as *const u32 as *const u8,
            std::mem::size_of::<u32>() as i32,
        ) != 0
        {
            let err = WSAGetLastError();
            closesocket(sock);
            return Err(format!("设置 SO_EXCLUSIVEADDRUSE 失败: WSA错误 {err}"));
        }
        let addr = SockAddrIn {
            sin_family: AF_INET as u16,
            sin_port: port.to_be(),
            sin_addr: [127, 0, 0, 1],
            sin_zero: [0; 8],
        };
        if bind(sock, &addr, std::mem::size_of::<SockAddrIn>() as i32) != 0 {
            let err = WSAGetLastError();
            closesocket(sock);
            return Err(format!("绑定 127.0.0.1:{port} 失败: WSA错误 {err}"));
        }
        if listen(sock, 128) != 0 {
            let err = WSAGetLastError();
            closesocket(sock);
            return Err(format!("listen 失败: WSA错误 {err}"));
        }
        // fd 所有权转交 std（后续转 tokio 异步轮询）
        Ok(std::net::TcpListener::from_raw_socket(sock as u64))
    }
}

#[cfg(not(target_os = "windows"))]
fn bind_exclusive(port: u16) -> Result<std::net::TcpListener, String> {
    // 必须显式设为非阻塞：tokio 的 `TcpListener::from_std` 拒绝注册阻塞套接字，
    // 会 panic「Registering a blocking socket with the tokio runtime is unsupported」。
    //
    // Windows 分支不受影响：它走 `from_raw_socket`，而 `from_raw_socket` 与
    // `from_std` 不同，不做阻塞检查（这也是为什么本问题只在 Linux/macOS 暴露）。
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("绑定 127.0.0.1:{port} 失败: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("设置非阻塞模式失败: {e}"))?;
    Ok(listener)
}

async fn bind_listener(port: u16) -> Result<TcpListener, String> {
    let std_listener = bind_exclusive(port)?;
    TcpListener::from_std(std_listener).map_err(|e| format!("监听器初始化失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::config::test_isolation::Isolated;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ai_gateway_proxy_test_{}_{}", tag, std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    fn test_ctx(dir: &PathBuf) -> ProxyCtx {
        let captured = Arc::new(AtomicI64::new(0));
        ProxyCtx {
            log: ProxyLog::new(dir.join("proxy.log"), None, captured),
            req_logger: Arc::new(RequestLogger::new(dir.clone())),
            targets: DEFAULT_TARGETS.iter().map(|s| s.to_string()).collect(),
            auto_capture_jwt: true,
            data_dir: dir.clone(),
        }
    }

    /// 默认域名表：后缀匹配（`api.trae.cn` 命中，`nottrae.cn` 不命中）
    #[test]
    fn default_targets_suffix_matching() {
        let dir = temp_dir("targets");
        let ctx = test_ctx(&dir);
        assert!(ctx.host_in_targets("api.trae.cn"));
        assert!(ctx.host_in_targets("TRAE.CN"), "大小写不敏感");
        assert!(ctx.host_in_targets("trae.cn"), "裸域名命中");
        assert!(ctx.host_in_targets("www.doubao.com"));
        assert!(!ctx.host_in_targets("nottrae.cn"), "后缀必须以点分界");
        assert!(!ctx.host_in_targets("evil-trae.cn"));
        assert!(!ctx.host_in_targets("example.com"));
    }

    /// 假占位符（旧算法）设备字段应被识别，真值不误判
    #[test]
    fn placeholder_device_detection() {
        assert!(is_placeholder_device(&serde_json::json!({"device_id": "222222222222222"})));
        assert!(is_placeholder_device(&serde_json::json!({
            "device_id": "123456789012345",
            "session_id": "55555555555555555555555555555555"
        })));
        assert!(!is_placeholder_device(&serde_json::json!({
            "device_id": "123456789012345",
            "session_id": "abcdef0123456789abcdef0123456789"
        })));
        // 空值不算占位符（避免把「抓到了但为空」反复重写）
        assert!(!is_placeholder_device(&serde_json::json!({"device_id": ""})));
        assert!(!is_placeholder_device(&serde_json::json!({})));
    }

    /// 缺失设备条目的账号应被补齐（走 store_dir 隔离锁）
    #[test]
    fn sync_fills_missing_and_refreshes_placeholder() {
        let iso = Isolated::new("proxy-sync");
        let dir = iso.dir().to_path_buf();
        let ctx = test_ctx(&dir);
        crate::modules::trae_account::upsert_account("4487568582777872", None, "Cloud-IDE-JWT a.b.c", None)
            .unwrap();
        // 预置一个旧占位符条目
        let mut map = crate::modules::trae_device::load_device_map();
        map.insert(
            "4487568582777872".to_string(),
            serde_json::json!({"device_id": "222222222222222", "session_id": "5".repeat(32).as_str()}),
        );
        crate::modules::trae_device::save_device_map(&map);

        sync_account_devices(&ctx);

        let after = crate::modules::trae_device::load_device_map();
        let dev = after.get("4487568582777872").expect("应已写入");
        assert_ne!(dev.get("device_id").and_then(|v| v.as_str()), Some("222222222222222"));
        // 二次同步幂等：真值不再被改写
        let snapshot = after.get("4487568582777872").cloned();
        sync_account_devices(&ctx);
        let again = crate::modules::trae_device::load_device_map();
        assert_eq!(again.get("4487568582777872").cloned(), snapshot);
    }

    /// 空账号库时静默返回，不报错不写库
    #[test]
    fn sync_tolerates_missing_accounts() {
        let iso = Isolated::new("proxy-sync-empty");
        let dir = iso.dir().to_path_buf();
        let ctx = test_ctx(&dir);
        sync_account_devices(&ctx); // 空表
        sync_account_devices(&ctx); // 再次（幂等）
    }

    /// Windows 独占绑定：同端口二次 bind 必须失败（issue #7 防孤儿进程假启动）
    #[cfg(target_os = "windows")]
    #[test]
    fn exclusive_bind_rejects_double_bind() {
        ensure_winsock_ready();
        let first = bind_exclusive(0).expect("首次绑定(临时端口)应成功");
        let port = first.local_addr().unwrap().port();
        assert!(bind_exclusive(port).is_err(), "同端口二次绑定应失败");
    }

    /// head 之后的超读字节必须完整提取（空 head / 无超读 / 带超读三态）
    #[test]
    fn head_after_head_end_extracts_overflow() {
        assert_eq!(head_after_head_end(b"CONNECT a.com:443 HTTP/1.1\r\n\r\n"), b"");
        assert_eq!(
            head_after_head_end(b"GET / HTTP/1.1\r\nHost: a\r\n\r\nEXTRA-BYTES"),
            b"EXTRA-BYTES"
        );
        assert_eq!(head_after_head_end(b"partial-no-head-end"), b"");
    }

    /// PrefixedStream：先耗尽前缀（超读字节回放）再透传底层流（审查修复回归）
    #[tokio::test]
    async fn prefixed_stream_replays_overflow_then_inner() {
        let (mut client, server) = tokio::io::duplex(64);
        client.write_all(b"flow").await.unwrap();
        let mut s = PrefixedStream::new(server, b"over".to_vec());
        let mut buf = [0u8; 16];
        let n = s.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"over", "前缀字节优先回放");
        let n = s.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"flow", "前缀耗尽后透传底层流");
    }

    /// 事件接收器：日志行内含 `user=<纯数字>` 时派生 account-captured 并累加计数
    #[test]
    fn vec_sink_records_logs_and_capture_events() {
        let sink = Arc::new(VecSink::new());
        let dir = temp_dir("sink");
        let captured = Arc::new(AtomicI64::new(0));
        let log = ProxyLog::new(dir.join("proxy.log"), Some(sink.clone()), Arc::clone(&captured));
        log.log("  [JWT 自动更新] user=4487568582777872 exp=...");
        log.log("  [签到改写] uid=4487568582777872 -> x-device-id=1");
        log.log("no user here");
        assert_eq!(captured.load(Ordering::Relaxed), 1, "仅 user= 记法计入捕获");
        assert_eq!(sink.captured(), vec!["4487568582777872".to_string()]);
        assert_eq!(sink.lines().len(), 3);
    }

    /// 配置构造：路径全部落在 store_dir 下，不硬编码 %APPDATA%
    #[test]
    fn proxy_config_paths_come_from_store_dir() {
        let iso = Isolated::new("proxy-config");
        let cfg = ProxyConfig::new(8899);
        assert_eq!(cfg.data_dir, iso.dir());
        assert_eq!(cfg.certs_dir, iso.dir().join("certs"));
        assert_eq!(cfg.log_path, iso.dir().join("logs").join("proxy.log"));
        assert_eq!(cfg.req_log_dir, iso.dir().join("logs"));
        assert!(cfg.auto_capture_jwt);
        assert!(cfg.targets.is_empty(), "空表示回落 DEFAULT_TARGETS");
    }

    /// 域名列表 CSV 解析：空白项丢弃
    #[test]
    fn config_targets_csv_parsing() {
        let cfg = ProxyConfig::new(1).with_targets_csv(" trae.cn , ,doubao.com, ");
        assert_eq!(cfg.targets, vec!["trae.cn".to_string(), "doubao.com".to_string()]);
    }

    /// 确保当前进程已初始化 WinSock。
    ///
    /// `WSASocketW` 要求进程先调用过 `WSAStartup`，否则返回 **10093
    /// WSANOTINITIALISED**。Rust 标准库在首次创建套接字时会隐式完成初始化，
    /// 因此「整套测试一起跑」时通常已被别的用例触发；但**单独运行**本模块的
    /// 用例时无人触发，就会以「创建监听 socket 失败: WSA错误 10093」失败 ——
    /// 表现为「全量跑绿、单跑变红」这种最难排查的形态。
    ///
    /// 这里显式建一个 std 套接字来触发进程级初始化（非 Windows 上是空操作）。
    fn ensure_winsock_ready() {
        #[cfg(target_os = "windows")]
        {
            drop(std::net::TcpListener::bind("127.0.0.1:0"));
        }
    }

    /// 端到端生命周期：启动 → 独占绑定端口 → stop() → 退出事件上报为「主动停止」
    #[tokio::test]
    async fn server_starts_binds_and_reports_intentional_stop() {
        ensure_winsock_ready();
        // 守卫必须活到测试结束（持有全局锁 + AI_GATEWAY_HOME 指向临时目录），
        // 故用命名绑定而非 `_`（`_` 会立即析构，隔离随即失效）
        let _iso = Isolated::new("proxy-lifecycle");
        let sink = Arc::new(VecSink::new());
        let cfg = ProxyConfig::new(0) // 0 = 让内核分配临时端口，避免与真实代理抢 8899
            .with_events(sink.clone());
        let server = ProxyServer::start(cfg).await.expect("代理应能启动");
        assert!(server.is_running());
        assert_eq!(server.captured_count(), 0);

        // 启动横幅必须已经落到操作日志（宿主面板依赖这些行）
        let lines = sink.lines();
        assert!(
            lines.iter().any(|l| l.contains("代理已启动")),
            "缺少启动横幅: {lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("监听 TRAE 域名")));

        server.stop();
        // 等 accept 循环退出并上报（watch 信号 + 事件在同一任务内顺序发出）
        let mut exit_rx = server.exit_signal();
        let _ = tokio::time::timeout(Duration::from_secs(5), exit_rx.changed()).await;
        for _ in 0..100 {
            if !sink.exits().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(sink.exits(), vec![true], "stop() 后应上报「主动停止」");
        assert!(sink.lines().iter().any(|l| l.contains("代理已停止")));
    }

    /// 启动时自动生成 CA 三件套（否则宿主无法提示用户安装证书）
    #[tokio::test]
    async fn server_start_generates_ca_files() {
        ensure_winsock_ready();
        let iso = Isolated::new("proxy-ca");
        let cfg = ProxyConfig::new(0);
        let certs = cfg.certs_dir.clone();
        let server = ProxyServer::start(cfg).await.expect("代理应能启动");
        assert!(certs.join("ca.crt").exists(), "启动应生成 ca.crt");
        assert!(certs.join("ca.key").exists(), "启动应生成 ca.key");
        assert!(certs.join("ca.cer").exists(), "启动应生成 ca.cer（供 certutil 安装）");
        server.stop();
        let _ = iso;
    }
}
