//! device_token_server —— 宿主侧的 WorkBuddy 设备 token 服务。
//!
//! # 为什么要有它（2026-09-28，所有者要求走 B 路线）
//!
//! 官方 WorkBuddy 客户端**每个请求**都带 `X-Device-Token`（`v3:` 形态，
//! 约 1030 字符），而网关此前完全不带。该 token 由官方自带的腾讯
//! TuringShield SDK 生成 —— 那是个 **N-API 原生模块**（`turing_sdk.node`
//! + `TuringShieldSDK.dll`），网关是 Go 进程，**无法直接加载**。
//!
//! 故复用 ZCode 验证码求解的**同款架构**（见 `captcha_webview.rs`）：
//!
//! ```text
//!   网关（Go）  ──HTTP POST /device-token──▶  宿主本地服务（本模块）
//!                                              │
//!                                              └─ Node 子进程加载 turing_sdk.node
//!                                                 返回 v3 token
//!   ◀────────── {"ok":true,"tokens":[...]} ────┘
//! ```
//!
//! # 为什么用 Node 子进程而不是 Rust 直接调 DLL
//!
//! `TuringShieldSDK.dll` **只导出一个 `createTSObject`**（C++ 工厂函数，
//! 返回 COM 风格接口）。Rust 侧要调它得还原整套虚表与接口定义 ——
//! 而官方已经写好了 N-API 包装（`native/turing-sdk/index.cjs`），
//! 直接复用它才是"用官方的东西"，而不是重新逆向一遍。
//!
//! 参数取自官方 `cli/product.json`（已核实并逐字使用）：
//!
//! ```text
//!   channelId      = 400111
//!   sdkVariant     = "overseas"     （isOversea=true → 国际版）
//!   productName    = "workbuddy-ai" （applicationName）
//!   productVersion = "5.6.2"
//! ```
//!
//! # 实测数据（决定本实现的形状）
//!
//! ```text
//!   configure(400111, workbuddy-ai, 5.6.2) → OK
//!   fetchDeviceToken → len=1030 prefix=v3:AAAAAaDle...
//!   耗时：首次 122ms，后续 39~51ms
//!   每次调用都不同（按请求签发）⇒ 网关侧不缓存，逐请求索取
//! ```
//!
//! # ⚠ 它解决什么、不解决什么（务必看清）
//!
//! - ✅ 让网关请求与官方客户端**同形态** —— 长期缺设备凭证会被风控
//!   逐步标记，带上它可避免这一类**新增**标记。
//! - ❌ **不能**救活已被上游标记失效的账号：那是服务端的 (设备,账号)
//!   绑定状态。实测（9 个国际版账号三组对照）：403 11140 的账号
//!   带上任何 device token 仍然 403；只有重新登录才能恢复。
//!
//! # 降级策略
//!
//! 找不到 Node / SDK / 客户端未安装时，服务**如实返回 ok=false**，
//! 网关侧据此**不发该头**、请求照常 —— 绝不因为"拿不到设备凭证"
//! 就让国际版对话不可用。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

/// 常驻的 Node 工作进程（**起一次，复用**）。
///
/// # 为什么要常驻（2026-09-28 所有者报的两个硬伤）
///
/// 第一版是**每请求 spawn 一次 node**。后果：
///
///	① **一直弹 cmd 黑窗** —— 每发一次国际版请求就起一个控制台进程。
///	   `CREATE_NO_WINDOW` 能压住窗口，但"每请求起进程"本身就是错设计。
///	② 每次都要重新加载 turing-sdk、重新 `configure`（实测冷启动 122ms
///	   vs 复用后 39~51ms）。
///
/// 常驻后：起一次，之后每次请求只是往 stdin 写一行 JSON。
struct NodeWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl NodeWorker {
    /// 启动工作进程。Windows 上加 `CREATE_NO_WINDOW`（否则弹黑窗）。
    fn spawn(node: &PathBuf, script: &PathBuf, sdk_dir: &PathBuf) -> Result<Self, String> {
        let mut cmd = tokio::process::Command::new(node);
        cmd.arg(script)
            .arg(sdk_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr 丢弃：worker 只在致命错误时写 stderr 并退出，
            // 那些情况会表现为 stdout 关闭，我们据此重启并报错。
            .stderr(Stdio::null())
            // 父进程退出时一并结束，避免留下孤儿 node。
            .kill_on_drop(true);
        // 不为子进程创建控制台窗口（Windows API `CREATE_NO_WINDOW`）。
        //
        // ⚠ 这是本仓库**既有约定**，我第一版漏了 —— 后果是所有者截图里
        // 「一直弹 cmd」：每发一次国际版请求就弹一个黑窗。
        // `gateway.rs` / `qoder_login.rs` 全都加了，`qoder_login.rs` 里
        // 还有测试专门钉住"不允许出现裸 `Command::new`"。
        //
        // `tokio::process::Command` 在 Windows 上**直接提供** `creation_flags`
        // （不必 use `std::os::windows::process::CommandExt` —— 加了反而报
        // "unused import"，因为 tokio 自己的实现已经覆盖）。
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = cmd.spawn().map_err(|e| format!("启动 node 失败: {e}"))?;
        let stdin = child.stdin.take().ok_or("无法获取 node stdin")?;
        let stdout = child.stdout.take().ok_or("无法获取 node stdout")?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    /// 请求 `count` 个 token（一次往返）。
    async fn request(&mut self, count: usize) -> Result<Vec<String>, String> {
        let line = format!("{{\"count\":{count}}}\n");
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("写入 node 失败（进程可能已退出）: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("flush node stdin 失败: {e}"))?;

        let mut buf = String::new();
        let n = self
            .stdout
            .read_line(&mut buf)
            .await
            .map_err(|e| format!("读取 node 输出失败: {e}"))?;
        if n == 0 {
            return Err("node 工作进程已退出".into());
        }

        let parsed: WorkerOut = serde_json::from_str(buf.trim()).map_err(|e| {
            format!(
                "解析 node 输出失败: {e}（原文前 120 字: {}）",
                buf.chars().take(120).collect::<String>()
            )
        })?;
        if !parsed.ok {
            return Err(parsed.error.unwrap_or_else(|| "未知错误".into()));
        }
        let tokens = parsed.tokens.unwrap_or_default();
        if tokens.is_empty() {
            return Err("node 未返回 token".into());
        }
        Ok(tokens)
    }

    /// 进程是否还活着（用于决定是否需要重启）。
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

/// 设备 token 服务的共享状态。
pub struct DeviceTokenState {
    token: String,
    port: Arc<Mutex<u16>>,
    /// Node 可执行文件（客户端自带优先，回落系统 PATH）。
    node: Option<PathBuf>,
    /// 常驻 worker 脚本（`assets/wb-device-token/wb-device-token-worker.cjs`）。
    script: Option<PathBuf>,
    /// TuringShield SDK 目录（`native/turing-sdk`）。
    sdk_dir: Option<PathBuf>,
    /// 常驻工作进程（懒启动；退出后下次请求自动重启）。
    worker: Mutex<Option<NodeWorker>>,
}

impl DeviceTokenState {
    pub fn new(token: String) -> Self {
        let node = resolve_node();
        let script = resolve_script();
        let sdk_dir = resolve_sdk_dir();
        Self {
            token,
            port: Arc::new(Mutex::new(0)),
            node,
            script,
            sdk_dir,
            worker: Mutex::new(None),
        }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// 三个组件是否齐全（缺任一即无法生成 token）。
    fn ready(&self) -> Result<(&PathBuf, &PathBuf, &PathBuf), String> {
        match (&self.node, &self.script, &self.sdk_dir) {
            (Some(n), Some(s), Some(d)) => Ok((n, s, d)),
            _ => {
                let mut missing = Vec::new();
                if self.node.is_none() {
                    missing.push("node");
                }
                if self.script.is_none() {
                    missing.push("wb-device-token-worker.cjs");
                }
                if self.sdk_dir.is_none() {
                    missing.push("native/turing-sdk");
                }
                Err(format!("缺少组件: {}", missing.join(", ")))
            }
        }
    }

    /// 取 `count` 个设备 token（走**常驻** worker，不再每请求起进程）。
    ///
    /// # 生命周期策略
    ///
    ///	· worker 未启动 → 启动（懒启动：没用过这个功能就不起进程）
    ///	· worker 已退出 → 重启一次再重试（覆盖"node 崩了/被杀了"）
    ///	· 重启后仍失败 → 如实报错，网关据此**降级**（不发该头）
    ///
    /// 整个流程持 `worker` 锁 ⇒ 天然串行。
    /// 这既符合 turing-sdk 的进程级单例语义，也让"重启"不会与在途请求打架。
    async fn fetch(&self, count: usize) -> Result<Vec<String>, String> {
        let (node, script, sdk) = self.ready()?;
        let mut guard = self.worker.lock().await;

        // 首次：懒启动
        if guard.is_none() {
            *guard = Some(NodeWorker::spawn(node, script, sdk)?);
        }

        // 已退出（崩溃/被杀）→ 重启一次
        if let Some(w) = guard.as_mut() {
            if !w.alive() {
                *guard = Some(NodeWorker::spawn(node, script, sdk)?);
            }
        }

        let worker = guard.as_mut().ok_or("worker 不可用")?;
        match worker.request(count).await {
            Ok(tokens) => Ok(tokens),
            Err(e) => {
                // 失败可能是"进程刚死"，重启后重试一次 ——
                // 但只重试一次，避免在真正坏掉时无限循环。
                *guard = Some(NodeWorker::spawn(node, script, sdk)?);
                let worker = guard.as_mut().ok_or("worker 重启后不可用")?;
                worker.request(count).await.map_err(|e2| {
                    format!("{e}；重启 worker 后仍失败: {e2}")
                })
            }
        }
    }
}

#[derive(Deserialize)]
struct WorkerOut {
    ok: bool,
    #[serde(default)]
    tokens: Option<Vec<String>>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize, Default)]
struct FetchReq {
    #[serde(default)]
    count: Option<usize>,
}

/// 解析 Node 可执行文件。
///
/// # ⚠ 关于「不是每台电脑都有 node」（所有者 2026-09-28 提出的缺点）
///
/// 他说得对：**不能期盼用户机器上有 node**。但本功能有个特殊之处 ——
///
///	**turing-sdk 本身就在官方客户端目录里**
///	（`...\WorkBuddyAI\resources\app.asar.unpacked\native\turing-sdk`）
///
/// 即"必须装了官方客户端"是这个功能的**硬前提**（没客户端就没有 SDK，
/// 也就根本无从生成 token）。而官方客户端**自带 node**：
///
///	%USERPROFILE%\.workbuddy-ai\binaries\node\versions\<ver>\node.exe
///
/// ⇒ 两个依赖**同源**：有 SDK 的地方就有 node。
/// 真正的缺口只有"客户端在、但 node 目录被清理或残缺"这一种。
///
/// # 为什么不自己内嵌一份 node
///
/// 实测客户端那份 node.exe **83 MB** —— 内嵌会让安装包从 12MB 涨到
/// 90MB+。这正是 ZCode 验证码那条路被所有者否决的原因，原话：
///
///	「不是每个用户电脑上都有node，你那个求解器，不能期盼所有用户都能
///	  满足运行环境，你需要修复这个问题」
///	（当时为此外置 node 会让包从 12MB 涨到约 105MB）
///
/// ZCode 的解法是用**宿主自带的 WebView2**（零体积）替代 node。
/// 但 turing-sdk 是 **N-API 原生模块**，WebView2 加载不了 ——
/// 那条路在这里不适用（详见文件头注释）。
///
/// # 为什么不改成 Rust 直接调 DLL
///
/// `TuringShieldSDK.dll` **只导出 `createTSObject`** —— 一个 C++ 工厂函数，
/// 返回 COM 风格接口。Rust 侧要调它得还原整套虚表布局与调用约定，
/// 而那是**闭源二进制**、且随版本可能变。收益（省掉一个"同源依赖"）
/// 远小于风险（ABI 猜错即崩溃，且无法用测试保证）。
///
/// 故本实现的立场是：**尽力找到 node；找不到就干净降级**
///（不发该头，请求照常成功 —— 见 `attachDeviceToken` 的降级说明）。
/// 这个功能是"锦上添花"（防未来风控标记），**不是准入条件**，
/// 因此它的不可用**不该**影响任何用户。
///
/// # 查找顺序（从最可靠到最兜底）
///
///  1. 官方客户端自带（ABI 匹配最稳，实测 v22.22.2）
///  2. 客户端安装目录下可能的内置 node
///  3. 环境变量 `WB_NODE`（显式覆盖，便于排查/非常规安装）
///  4. 系统 PATH（用户自己装的 node）
fn resolve_node() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();

    // 1) 客户端自带：%USERPROFILE%\.workbuddy-ai\binaries\node\versions\<ver>\node.exe
    if let Ok(home) = std::env::var("USERPROFILE") {
        let base = PathBuf::from(&home)
            .join(".workbuddy-ai")
            .join("binaries")
            .join("node")
            .join("versions");
        if let Ok(entries) = std::fs::read_dir(&base) {
            let mut versions: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path().join("node.exe"))
                .filter(|p| p.is_file())
                .collect();
            // 版本目录名倒序 → 最新的排最后，reverse 后取第一个
            versions.sort();
            versions.reverse();
            cands.extend(versions);
        }
    }

    // 2) 客户端安装目录下可能的内置 node（不同安装方式位置不一）
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let res = PathBuf::from(&local)
            .join("Programs")
            .join("WorkBuddyAI")
            .join("resources");
        for rel in ["node.exe", "bin/node.exe", "cli/node.exe", "app.asar.unpacked/node.exe"] {
            cands.push(res.join(rel));
        }
    }

    // 3) 显式覆盖（与 `WB_TURING_SDK_DIR` 同款，便于排查）
    if let Ok(p) = std::env::var("WB_NODE") {
        cands.insert(0, PathBuf::from(p));
    }

    // 4) 系统 PATH
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join("node.exe");
            if p.is_file() {
                cands.push(p);
            }
        }
    }

    cands.into_iter().find(|p| p.is_file())
}

/// 解析**常驻 worker** 脚本：优先发行包 resources，其次开发期仓库路径。
///
/// # ⚠ 用 worker 版而不是 `wb-device-token.cjs`（2026-09-28）
///
/// 两者协议不同，**不可混用**：
///
///	`wb-device-token.cjs`         一次性：`node <script> <sdkDir> <count>` → 一行 JSON
///	`wb-device-token-worker.cjs`  常驻：stdin 收 `{"count":N}`、stdout 回一行 JSON
///
/// 本模块只用 worker 版（起一次、复用）—— 一次性版保留给"手工排查"
/// 场景（命令行直接跑一次看结果），不参与运行期。
///
/// 若哪天误把一次性版接进来，症状是：stdout 立刻 EOF ⇒ 每次请求都"重启 worker"
/// 且读不到响应 ⇒ 设备 token 永远拿不到（但请求仍照常，因为会降级）。
///
/// # ⚠ Tauri 的释放布局（照 `zcode-captcha` 的实测结论，别想当然）
///
/// `tauri.conf.json` 里写 `"../assets/wb-device-token"`，安装后落在
///
///	$INSTDIR\assets\wb-device-token
///
/// **不是** `$INSTDIR\wb-device-token`，也不是 `$INSTDIR\resources\...` ——
/// 这一点在 `zcode-captcha` 上已经踩过一次（当时按后者找，直到解包看到
/// `_up_\assets\zcode-captcha` 才发现）。故这里把两种布局都列上，
/// 免得换个打包方式就静默失效。
fn resolve_script() -> Option<PathBuf> {
    const NAME: &str = "wb-device-token-worker.cjs";
    let mut cands = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for base in [
                dir.to_path_buf(),
                dir.join("resources"),
                dir.join("_up_"),
            ] {
                cands.push(base.join("assets").join("wb-device-token").join(NAME));
                cands.push(base.join("wb-device-token").join(NAME));
            }
        }
    }
    // 开发期：仓库 assets/
    cands.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../assets/wb-device-token")
            .join(NAME),
    );
    cands.into_iter().find(|p| p.is_file())
}

/// 解析 TuringShield SDK 目录（从官方客户端安装位置找）。
fn resolve_sdk_dir() -> Option<PathBuf> {
    let mut cands = Vec::new();
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        cands.push(
            PathBuf::from(&local)
                .join("Programs")
                .join("WorkBuddyAI")
                .join("resources")
                .join("app.asar.unpacked")
                .join("native")
                .join("turing-sdk"),
        );
    }
    // 允许显式覆盖（便于测试/非常规安装位置）
    if let Ok(p) = std::env::var("WB_TURING_SDK_DIR") {
        cands.insert(0, PathBuf::from(p));
    }
    cands.into_iter().find(|p| p.join("index.cjs").is_file())
}

/// 启动设备 token 服务（后台任务）。
pub fn spawn_device_token_server(state: Arc<DeviceTokenState>) {
    tauri::async_runtime::spawn(async move {
        if let Err(e) = run_server(state).await {
            eprintln!("[device-token] 服务启动失败: {e}");
        }
    });
}

async fn run_server(state: Arc<DeviceTokenState>) -> Result<(), String> {
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;

    let listener = TcpListener::bind(("127.0.0.1", 0u16))
        .await
        .map_err(|e| format!("绑定本地端口失败: {e}"))?;
    let addr: SocketAddr = listener
        .local_addr()
        .map_err(|e| format!("读取本地端口失败: {e}"))?;

    *state.port.lock().await = addr.port();

    // 把地址注册给 ai-gateway-core —— 它生成网关配置时读这个槽。
    // 必须在**端口确定之后**才注册（早注册会让网关拿到不存在的地址）。
    let url = format!("http://127.0.0.1:{}/device-token", addr.port());
    ai_gateway_core::modules::gateway::set_device_token_service(
        url.clone(),
        state.token().to_string(),
    );

    match state.ready() {
        Ok((n, s, d)) => eprintln!(
            "[device-token] 服务已启动：{url}\n  node={}\n  script={}\n  sdk={}",
            n.display(),
            s.display(),
            d.display()
        ),
        Err(missing) => eprintln!(
            "[device-token] 服务已启动但**组件不全**（{missing}）—— \
             网关将不发 X-Device-Token，国际版对话不受影响"
        ),
    }

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[device-token] 接受连接失败: {e}");
                continue;
            }
        };
        let st = state.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req| {
                let st = st.clone();
                async move { handle(st, req).await }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, svc)
                .await;
        });
    }
}

async fn handle(
    state: Arc<DeviceTokenState>,
    req: hyper::Request<hyper::body::Incoming>,
) -> Result<hyper::Response<http_body_util::Full<bytes::Bytes>>, std::convert::Infallible> {
    use http_body_util::BodyExt;

    if req.method() != hyper::Method::POST {
        return Ok(json_resp(405, &serde_json::json!({"ok": false, "error": "method not allowed"})));
    }
    // 同机 IPC 鉴权：防止同机其它进程误用这个端口。
    if let Some(expect) = Some(state.token()).filter(|t| !t.is_empty()) {
        let got = req
            .headers()
            .get("X-Device-Token-Service-Token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if got != expect {
            return Ok(json_resp(401, &serde_json::json!({"ok": false, "error": "unauthorized"})));
        }
    }

    let body = match req.into_body().collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => {
            return Ok(json_resp(400, &serde_json::json!({"ok": false, "error": format!("读请求体失败: {e}")})));
        }
    };
    let parsed: FetchReq = serde_json::from_slice(&body).unwrap_or_default();
    let count = parsed.count.unwrap_or(1).clamp(1, 16);

    match state.fetch(count).await {
        Ok(tokens) => Ok(json_resp(200, &serde_json::json!({"ok": true, "tokens": tokens}))),
        Err(e) => {
            eprintln!("[device-token] 生成失败: {e}");
            // 失败也回 200 + ok:false —— 让网关把它当"本次不发该头"处理，
            // 而不是当成传输层故障去重试（重试也不会成功）。
            Ok(json_resp(200, &serde_json::json!({"ok": false, "error": e})))
        }
    }
}

fn json_resp(
    status: u16,
    value: &serde_json::Value,
) -> hyper::Response<http_body_util::Full<bytes::Bytes>> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    hyper::Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(http_body_util::Full::new(bytes::Bytes::from(body)))
        .unwrap_or_else(|_| hyper::Response::new(http_body_util::Full::new(bytes::Bytes::new())))
}
