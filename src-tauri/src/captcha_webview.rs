//! captcha_webview —— 用宿主自带的 WebView2 求解阿里云无痕验证。
//!
//! # 为什么要有这个模块（2026-09-21 所有者提出的方案）
//!
//! ZCode 的对话通道要求请求头带 `X-Aliyun-Captcha-Verify-Param`，否则一律回
//! `HTTP 400 {"code":3007,"msg":"captcha verify failed"}`。
//!
//! 此前我们靠**本地起 Node 子进程 + happy-dom 模拟浏览器**来求解。两个硬伤：
//!
//!  1. **要求用户机器装了 Node** —— 否则 ZCode 完全不可用。所有者原话：
//!     「不是每个用户电脑上都有node，你那个求解器，不能期盼所有用户都能满足
//!     运行环境，你需要修复这个问题」。为此外置 node.exe 会让安装包
//!     从 12MB 涨到约 105MB。
//!
//!  2. **模拟环境被风控盯上** —— 实测连续求解成功率仅约 40%
//!     （把失速阈值从 6s 提到 20s 后升到 88%，但依然不稳定）。
//!
//! 而官方 ZCode 客户端用的是**真实浏览器环境**（从 `D:\APP\ZCode\resources\app.asar`
//! 核实）：
//!
//! ```js
//! script.src = "https://o.alicdn.com/captcha-frontend/aliyunCaptcha/AliyunCaptcha.js"
//! window.initAliyunCaptcha({ ... getInstance: inst => inst.startTracelessVerification() })
//! ```
//!
//! 我们的 Tauri 宿主**自带真实 WebView2**（Windows 10/11 预装，零额外体积），
//! 与官方客户端是同一类环境。
//!
//! # 实测数据（`uitest/probe-captcha-in-browser.cjs`，有头 Chrome，无人工点击）
//!
//! ```text
//! [+10ms]   initAliyunCaptcha 已调用
//! [+600ms]  getInstance 触发
//! [+601ms]  调用 startTracelessVerification()
//! [+929ms]  success，param 长度 280
//! ```
//!
//! 不到 1 秒（本地 Node 方案约 3 秒），且无冷却、不占安装包体积。
//!
//! # 架构
//!
//! 网关是**独立进程**，不能直接操控宿主的窗口。故：
//!
//! ```text
//!   网关（Go）  ──HTTP POST /solve──▶  宿主本地服务（本模块）
//!                                          │
//!                                          ├─ 隐藏 WebviewWindow 载入求解页
//!                                          ├─ 页面跑官方 SDK，拿到 param
//!                                          └─ 通过 Tauri IPC 回传
//!   ◀──────────── {"param": "..."} ────────┘
//! ```
//!
//! 方向选择说明：把 HTTP 服务放在**宿主**侧（而不是让网关开端口等宿主来推），
//! 是因为 API Key 之类的东西都在宿主手上，且宿主本来就是网关的"上游"。
//! 令牌校验（`X-Captcha-Token`）用于防止同机其它进程误用这个端口。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
// `Manager` 提供 `try_state` / `get_webview_window` / `manage` —— 不 import 会报
// 「no method named `try_state` found」，而错误信息不会提示缺 trait。
use tauri::Manager;
use tokio::sync::oneshot;

/// 一次求解请求的挂起槽。
///
/// 页面求出 param 后通过 `captcha_webview_result` 命令回填，本槽据此唤醒
/// 等待中的 HTTP 请求。
#[derive(Default)]
struct Pending {
    inner: Mutex<HashMap<u64, oneshot::Sender<SolveResult>>>,
}

impl Pending {
    fn insert(&self, id: u64, tx: oneshot::Sender<SolveResult>) {
        if let Ok(mut m) = self.inner.lock() {
            m.insert(id, tx);
        }
    }
    fn take(&self, id: u64) -> Option<oneshot::Sender<SolveResult>> {
        self.inner.lock().ok().and_then(|mut m| m.remove(&id))
    }
    fn len(&self) -> usize {
        self.inner.lock().map(|m| m.len()).unwrap_or(0)
    }
}

/// 求解结果（页面回报）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolveResult {
    /// 求到的 param；空表示失败。
    #[serde(default)]
    pub param: String,
    /// 区域（随 param 一起给上游）。
    #[serde(default)]
    pub region: String,
    /// 失败原因（`param` 为空时有意义）。
    #[serde(default)]
    pub error: String,
}

/// 全局状态：挂起槽 + 计数器 + 共享令牌。
pub struct CaptchaWebviewState {
    pending: Arc<Pending>,
    next_id: AtomicU64,
    token: String,
    /// 本地服务监听的端口（0 = 未启动）。
    port: Arc<Mutex<u16>>,
}

impl CaptchaWebviewState {
    pub fn new(token: String) -> Self {
        Self {
            pending: Arc::new(Pending::default()),
            next_id: AtomicU64::new(1),
            token,
            port: Arc::new(Mutex::new(0)),
        }
    }

    /// 本地服务地址（未启动时为空）。
    pub fn solver_url(&self) -> String {
        let p = self.port.lock().map(|p| *p).unwrap_or(0);
        if p == 0 {
            String::new()
        } else {
            format!("http://127.0.0.1:{p}/solve")
        }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// 页面回报结果时调用（由 Tauri 命令转发）。
    ///
    /// 返回 false 表示 id 不在挂起表里（已超时被清理，或 id 伪造）——
    /// 这不是错误，如实返回让调用方决定要不要记日志。
    pub fn complete(&self, id: u64, result: SolveResult) -> bool {
        match self.pending.take(id) {
            Some(tx) => {
                // 接收端可能已因超时丢弃 —— 那是正常竞态，不算失败
                let _ = tx.send(result);
                true
            }
            None => false,
        }
    }

    /// 当前挂起的求解数（供诊断/界面展示）。
    pub fn inflight(&self) -> usize {
        self.pending.len()
    }
}

/// 求解页面 HTML（内嵌，避免发行包再多一个资源文件）。
///
/// # 为什么内嵌而不是放 assets/
///
/// 它只有几 KB，且**必须与宿主版本严格一致**（命令名、参数格式都在里面）。
/// 作为独立文件会有"用户升级后旧文件残留"的版本漂移风险，内嵌则不可能不一致。
///
/// # 关键点：`startTracelessVerification()`
///
/// 实测踩过的坑：只调 `inst.show()` 会进入**等人工点击**的模式（有头窗口里
/// 表现为"一直转圈，必须手动点一下"）—— 我因此误以为"每次要 32 秒"。
/// 而 `startTracelessVerification()` 是**无痕自动验证**，0.9 秒出结果。
/// 这正是官方客户端的用法。
/// # 为什么用 `r##"…"##` 而不是 `r#"…"#`
///
/// 页面里含有 `"#cap"` / `"#btn"` 这类 CSS 选择器字符串 —— 其中的 `"#`
/// **会提前终止 `r#"…"#` 原始字符串**，导致编译器把后面的 HTML 当成 Rust 代码
/// 解析（报一堆 `prefix 'btn' is unknown` / `unknown start of token: （`，
/// 而错误位置指向的是 HTML 内部，与"字符串没闭合"这个真因看似无关）。
///
/// 用两个 `#` 做定界符即可 —— 只要内容里不出现 `"##`。
const SOLVER_HTML: &str = r##"<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>captcha</title></head>
<body>
<div id="cap"></div><button id="btn">go</button>
<script src="https://o.alicdn.com/captcha-frontend/aliyunCaptcha/AliyunCaptcha.js"></script>
<script>
(function () {
  // ⚠ 参数从**自己的 URL** 读，不从 initialization_script 注入读。
  //
  // 旧实现把参数经 `initialization_script` 写成 `window.__req`，再靠
  // `document.write` 灌页面 —— 而 `document.write` 会替换整个文档，
  // 连带把 `window.__req` 一起冲掉。详见 SOLVER_PAGE_PATH 的注释。
  var Q = new URLSearchParams(location.search);
  var REQ = {
    id: Number(Q.get("id") || "0"),
    scene: Q.get("scene") || "11xygtvd",
    region: Q.get("region") || "cn",
    prefix: Q.get("prefix") || "no8xfe",
  };

  var reported = false;
  function done(payload) {
    if (reported) return;   // 只回报一次（SDK 可能回调多次）
    reported = true;
    // ⚠ 用**同源 fetch** 回报，不走 Tauri IPC。
    //
    // IPC 会被 capability 的 `windows` 白名单拦住（求解窗口不在其中），
    // 且失败时静默 —— 实测表现为必然超时 25 秒。同源 fetch 没有这层限制。
    try {
      fetch("/result", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          id: REQ.id,
          result: {
            param: payload.param || "",
            region: payload.region || REQ.region,
            error: payload.error || "",
          },
        }),
      }).catch(function (e) {
        console.error("回报失败", e);
      });
    } catch (e) {
      console.error("回报异常", e);
    }
  }

  function start() {
    if (typeof window.initAliyunCaptcha !== "function") {
      done({ param: "", region: REQ.region, error: "SDK 未加载（initAliyunCaptcha 未定义）" });
      return;
    }
    try {
      window.initAliyunCaptcha({
        SceneId: REQ.scene,
        mode: "popup",
        region: REQ.region,
        prefix: REQ.prefix,
        language: "en",
        element: "#cap",
        button: "#btn",
        captchaLogoImg: "",
        showErrorTip: false,
        getInstance: function (inst) {
          try {
            // ⚠ 必须走无痕验证；show() 会等人工点击（实测差 32 倍：32s vs 0.9s）
            var fn = inst.startTracelessVerification || inst.show;
            fn.call(inst);
          } catch (e) {
            done({ param: "", region: REQ.region, error: "启动验证失败: " + e });
          }
        },
        success: function (param) {
          // ⚠ 把原始回调值一并带回（诊断用）。
          //
          // 实测踩到：报告"求解失败"但 error 为空 —— 说明 success **被调用了**，
          // 只是 param 取出来是空。此时只看 error 完全无从判断是
          // "SDK 给的是对象而非字符串"还是"真的是空值"。
          //
          // SDK 不同版本的 success 回调形态不一致（可能是字符串、也可能是
          // 带 verifyParam 字段的对象），故这里做一次归一化并把原值留证。
          var raw = param;
          var p = "";
          if (typeof param === "string") {
            p = param;
          } else if (param && typeof param === "object") {
            p = param.verifyParam || param.verify_param || param.param || "";
            if (!p) {
              // 对象里没有已知字段 —— 把键名带回去，便于对照 SDK 版本修正
              try {
                raw = "keys=" + Object.keys(param).join(",") + " json=" + JSON.stringify(param).slice(0, 300);
              } catch (e) {
                raw = "（对象无法序列化）";
              }
            }
          }
          done({
            param: p,
            region: REQ.region,
            error: p ? "" : "success 回调未给出 param；原始值: " + String(raw).slice(0, 400),
          });
        },
        fail: function (err) {
          done({ param: "", region: REQ.region, error: "验证失败: " + JSON.stringify(err) });
        },
        onError: function (err) {
          done({ param: "", region: REQ.region, error: "SDK 错误: " + JSON.stringify(err) });
        },
      });
    } catch (e) {
      done({ param: "", region: REQ.region, error: "初始化异常: " + e });
    }
  }

  // SDK 是外部 script，可能还没加载完 —— 轮询等到就绪（最多 20 秒）
  var waited = 0;
  var t = setInterval(function () {
    waited += 100;
    if (typeof window.initAliyunCaptcha === "function") { clearInterval(t); start(); }
    else if (waited >= 20000) {
      clearInterval(t);
      done({ param: "", region: REQ.region, error: "等待 SDK 加载超时 20s" });
    }
  }, 100);
})();
</script>
</body></html>"##;

/// 启动本地求解服务（后台任务）。
///
/// 端口用 0（让系统分配空闲端口），避免与用户其它程序撞端口 ——
/// 撞了会让整个功能静默失效，而"固定端口更可预测"在这里没有价值
///（地址本来就要透传给网关）。
pub fn spawn_solver_server(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        if let Err(e) = run_server(app).await {
            log_err(&format!("ZCode 验证码求解服务启动失败: {e}"));
        }
    });
}

async fn run_server(app: tauri::AppHandle) -> Result<(), String> {
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind(("127.0.0.1", 0u16))
        .await
        .map_err(|e| format!("绑定本地端口失败: {e}"))?;
    let addr: SocketAddr = listener
        .local_addr()
        .map_err(|e| format!("读取本地端口失败: {e}"))?;

    if let Some(state) = app.try_state::<CaptchaWebviewState>() {
        if let Ok(mut p) = state.port.lock() {
            *p = addr.port();
        }
        // 把地址注册给 `ai-gateway-core` —— 它生成网关配置时会读这个槽
        //（见该处的说明：下层不能反向依赖上层，故用进程级单例桥接）。
        //
        // 必须在**端口确定之后**才注册：早注册会让网关拿到一个还不存在的地址。
        ai_gateway_core::modules::gateway::set_external_solver(
            format!("http://127.0.0.1:{}/solve", addr.port()),
            state.token().to_string(),
        );
    }
    log_info(&format!(
        "ZCode 验证码求解服务已启动：http://127.0.0.1:{}/solve（WebView2 真实浏览器环境）",
        addr.port()
    ));

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                log_err(&format!("接受连接失败: {e}"));
                continue;
            }
        };
        let app2 = app.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req| {
                let app = app2.clone();
                async move { handle_request(app, req).await }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, svc)
                .await;
        });
    }
}

/// 路由分发：求解请求（网关调）与求解页通信（页面自己调）。
///
/// # 为什么页面用 HTTP 而不是 Tauri IPC
///
/// 见 `SOLVER_PAGE_PATH` 的长注释（capability 白名单 + document.write
/// 冲掉注入变量，两个坑叠加导致必然超时 25 秒）。
///
/// # 鉴权分两套（故意的）
///
///  - /solve   给**网关**用 —— 要 `X-Captcha-Token`（同机其它进程能扫到端口）
///  - /page    给**求解窗口**用 —— 不要令牌，因为它就是被我们自己导航过去的；
///              且页面在 WebView 沙箱里拿不到令牌（那在宿主内存中）
///  - /result  给**求解窗口**用 —— 不要令牌，但**校验窗口存在**（见其实现）
///
/// 不给 /page 与 /result 加令牌的理由：它们的请求方是**我们自己开的窗口**，
/// 而令牌若写进页面就等于公开（页面内容可被审计）。真正防滥用的门槛放在
/// /solve（那才是会向上游发请求、消耗资源的入口）。
async fn handle_request(
    app: tauri::AppHandle,
    req: hyper::Request<hyper::body::Incoming>,
) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, std::convert::Infallible> {
    let path = req.uri().path().to_string();
    match path.as_str() {
        SOLVER_PAGE_PATH => handle_page(req).await,
        SOLVER_RESULT_PATH => handle_result(app, req).await,
        _ => handle_solve(app, req).await,
    }
}

/// `/page` —— 返回求解页 HTML（给隐藏的求解窗口加载）。
async fn handle_page(
    req: hyper::Request<hyper::body::Incoming>,
) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, std::convert::Infallible> {
    use http_body_util::Full;
    use hyper::body::Bytes;
    let _ = req;
    Ok(hyper::Response::builder()
        .status(200)
        .header("content-type", "text/html; charset=utf-8")
        // 求解页每次都要最新：它含"要怎么求解"的逻辑，缓存住会让升级后行为不一致。
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(SOLVER_HTML)))
        .unwrap())
}

/// `/result` —— 求解页回报 param。
///
/// 页面用**同源 fetch** POST 到这里（不走 IPC）。
async fn handle_result(
    app: tauri::AppHandle,
    req: hyper::Request<hyper::body::Incoming>,
) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, std::convert::Infallible> {
    use http_body_util::{BodyExt, Full};
    use hyper::body::Bytes;

    let json = |code: u16, body: String| -> Result<_, std::convert::Infallible> {
        Ok(hyper::Response::builder()
            .status(code)
            .header("content-type", "application/json; charset=utf-8")
            .body(Full::new(Bytes::from(body)))
            .unwrap())
    };

    let state = match app.try_state::<CaptchaWebviewState>() {
        Some(s) => s,
        None => return json(500, r#"{"error":"求解器状态未初始化"}"#.into()),
    };

    let body = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => return json(400, format!(r#"{{"error":"读取请求体失败: {e}"}}"#)),
    };
    let doc: ResultReport = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return json(400, format!(r#"{{"error":"请求体不是合法 JSON: {e}"}}"#)),
    };

    // 关掉那个一次性窗口（无论成功失败都不再需要）。
    {
        use tauri::Manager;
        let label = format!("captcha-solve-{}", doc.id);
        if let Some(w) = app.get_webview_window(&label) {
            let _ = w.close();
        }
    }

    let ok = state.complete(doc.id, doc.result.clone());
    // 记录每次回报（含失败原因）—— 排查"为什么解不出来"时，这是**唯一**
    // 能看到页面内部状态的途径（WebView 的 console 默认不在宿主日志里）。
    log_info(&format!(
        "求解回报 id={} param长度={} region={} error={}",
        doc.id,
        doc.result.param.len(),
        doc.result.region,
        if doc.result.error.is_empty() {
            "（无）"
        } else {
            &doc.result.error
        }
    ));
    // 页面不关心业务结果，只要"收到了"。故即便 id 已失效也返回 200 ——
    // 那通常是超时后的迟到回报，属正常竞态，不该让页面报错。
    json(200, format!(r#"{{"ok":{ok}}}"#))
}

/// `/result` 的请求体。
///
/// # ⚠ 字段是**嵌套**的，不是扁平的（2026-09-21 实测踩到的静默 bug）
///
/// 页面发的是：
///
///  - {"id": 1, "result": {"param": "...", "region": "cn", "error": ""}}
///
/// 而第一版这里写的是 `#[serde(flatten)] result: SolveResult` —— 那要求
/// param/region/error 与 id **同级**。于是：
///
///  - · `id` 解析正常 ✓
///  - · `param`/`region`/`error` 在顶层**找不到** ⇒ 因有 `#[serde(default)]`
///  -   而**静默取默认空值**，不报任何错
///
/// 表现为宿主日志 `求解回报 id=1 param长度=0 region= error=（无）` ——
/// 看起来像"SDK 给了空 param"，实际是**解析没对上**。
///
/// 这个组合（`flatten` + `#[serde(default)]`）会**吞掉结构不匹配**：
/// 没有 default 时至少会报 missing field，而 default 让错误彻底消失。
/// 教训：跨语言（JS↔Rust）的报文结构要**对着实际发送方**写，
/// 不能凭"看起来应该扁平"推断。
#[derive(Debug, serde::Deserialize)]
struct ResultReport {
    id: u64,
    result: SolveResult,
}

/// 处理一次 `/solve` 请求。
async fn handle_solve(
    app: tauri::AppHandle,
    req: hyper::Request<hyper::body::Incoming>,
) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, std::convert::Infallible> {
    use http_body_util::Full;
    use hyper::body::Bytes;

    let json = |code: u16, body: String| -> Result<_, std::convert::Infallible> {
        Ok(hyper::Response::builder()
            .status(code)
            .header("content-type", "application/json; charset=utf-8")
            .body(Full::new(Bytes::from(body)))
            .unwrap())
    };

    let state = match app.try_state::<CaptchaWebviewState>() {
        Some(s) => s,
        None => return json(500, r#"{"error":"求解器状态未初始化"}"#.into()),
    };

    // ---- 鉴权 ----
    //
    // 同机 IPC 也要校验：本地其它程序（含恶意脚本）能扫到这个端口，
    // 而每次求解都会向上游发请求。没有令牌就可能被人当免费求解器刷。
    let got = req
        .headers()
        .get("x-captcha-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if got != state.token() {
        return json(403, r#"{"error":"令牌不匹配"}"#.into());
    }

    // ---- 取参数（gate 需要 scene/region/prefix）----
    let q: HashMap<String, String> = req
        .uri()
        .query()
        .map(|s| {
            url::form_urlencoded::parse(s.as_bytes())
                .into_owned()
                .collect()
        })
        .unwrap_or_default();
    let scene = q.get("scene").cloned().unwrap_or_else(|| "11xygtvd".into());
    let region = q.get("region").cloned().unwrap_or_else(|| "cn".into());
    let prefix = q.get("prefix").cloned().unwrap_or_else(|| "no8xfe".into());

    // ---- 建隐藏窗口，等页面回报 ----
    let id = state.next_id.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel::<SolveResult>();
    state.pending.insert(id, tx);

    if let Err(e) = spawn_solver_window(&app, id, &scene, &region, &prefix) {
        state.pending.take(id); // 建窗口失败，清掉挂起槽
        return json(500, format!(r#"{{"error":{}}}"#, json_str(&e)));
    }

    // 超时上限：实测 ~0.9 秒成功。给 25 秒余量（首次要下载 SDK 与 pe 脚本）。
    match tokio::time::timeout(Duration::from_secs(25), rx).await {
        Ok(Ok(res)) => {
            if res.param.is_empty() {
                json(
                    502,
                    format!(r#"{{"error":{}}}"#, json_str(&format!("求解失败: {}", res.error))),
                )
            } else {
                json(
                    200,
                    format!(
                        r#"{{"param":{},"region":{}}}"#,
                        json_str(&res.param),
                        json_str(&res.region)
                    ),
                )
            }
        }
        Ok(Err(_)) => json(500, r#"{"error":"求解通道被关闭"}"#.into()),
        Err(_) => {
            state.pending.take(id); // 超时：清掉，避免泄漏
            json(504, r#"{"error":"求解超时（25s）"}"#.into())
        }
    }
}

/// 求解页与宿主通信的路径（都由宿主自己的 HTTP 服务提供）。
///
/// # 为什么不让页面走 Tauri IPC（2026-09-21 实测踩到的两个坑）
///
/// ## 坑一：capability 会拦下非 `main` 窗口的 IPC
///
/// `capabilities/default.json` 原本写的是 `"windows": ["main"]` ——
/// 而求解窗口叫 `captcha-solve-{id}`，**不在授权列表里**，
/// 页面里的 `window.__TAURI_INTERNALS__.invoke(...)` 会被权限系统拒绝。
/// 更糟的是它**静默失败**：我的页面代码把异常吞在 `catch` 里只 `console.error`，
/// 宿主永远收不到回报 ⇒ 只能等满 25 秒超时。
///
/// ## 坑二：`document.write` 会把 `initialization_script` 冲掉
///
/// 旧实现先开窗口（`http://127.0.0.1/`，**没有服务器在听**），
/// 再 `eval("document.open();document.write(HTML);document.close()")`。
/// 而 `document.write` **替换整个文档**，连带把初始化脚本注入的
/// `window.__req` 一起冲掉 ⇒ 页面读不到 scene/region/prefix。
///
/// 实测症状（窗口创建只花 0.5~7 毫秒，远低于真实建 WebView2 的开销，
/// 说明页面根本没跑起来）：
///
///  - [captcha-webview] 求解窗口已创建（7.1423ms）id=1
///  - [captcha-webview] 求解窗口已创建（672.9µs） id=2
///  - （然后全部等满 25 秒超时）
///
/// # 现在的方案：页面由宿主自己的 HTTP 服务提供
///
/// 求解窗口加载 `http://127.0.0.1:{port}/page?id=N&scene=..&region=..&prefix=..`
/// —— 这是一个**真实可加载的地址**，页面正常执行；参数从**自己的 URL** 读，
/// 不依赖任何会被冲掉的注入。
///
/// 求出结果后，页面 `fetch("/result", {method:POST, ...})` 回报。
/// 这个 fetch 是**同源**的（页面与服务同一个 origin），不受任何跨域限制，
/// 也**完全不经过 Tauri IPC** ⇒ capability 配错也影响不到它。
///
/// 净效果：少一层易错的桥（IPC + 注入），多一条直路（同源 HTTP）。
const SOLVER_PAGE_PATH: &str = "/page";
const SOLVER_RESULT_PATH: &str = "/result";
///
/// # ⚠ 性能：这里是首次对话 29 秒的主要来源（2026-09-21 所有者反馈）
///
/// 所有者原话：「我首次对话就耗费了29秒，你需要优化这个」。
///
/// 拆解那 29 秒（每一项都是首次才付的成本）：
///
///  - 1. **建 WebView2 窗口** —— 首次要初始化整个渲染环境（数秒）
///  - 2. **下载官方 SDK**（225KB）+ **pe 字节码**（450KB）—— 首次无缓存
///  - 3. **池预热是串行的**（见 captchaPool.refillTo）—— 补 3 个 = 3 × 单次耗时
///
/// 故优化方向是「**把 1 与 2 提前到用户还没发请求时**」—— 见
/// `warmup_prefetch`：宿主启动后立刻建一次常驻窗口并加载 SDK，
/// 让渲染环境与 SDK 缓存都就绪（那两项是主要开销，且**只付一次**）。
///
/// 之后每次求解只需重建页面（毫秒级）—— 窗口本身复用。
///
/// # 为什么仍然每次新建窗口（而不是完全复用）
///
/// param 是**一次性**的，且页面的 SDK 实例状态无法安全重置
///（`initAliyunCaptcha` 没有对应的 destroy API，重复 init 的行为未定义）。
/// 而窗口复用能省下的只是"渲染进程创建"，那部分已由
/// `warmup_prefetch` 的常驻预热窗口承担。
fn spawn_solver_window(
    app: &tauri::AppHandle,
    id: u64,
    scene: &str,
    region: &str,
    prefix: &str,
) -> Result<(), String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};

    let t0 = std::time::Instant::now();
    let label = format!("captcha-solve-{id}");

    // 端口由求解服务在启动时写入 state（动态分配）。
    let port = app
        .try_state::<CaptchaWebviewState>()
        .and_then(|s| s.port.lock().ok().map(|p| *p))
        .unwrap_or(0);
    if port == 0 {
        return Err("求解服务尚未就绪（端口未知）".into());
    }

    // ⚠ 加载**真实可访问的地址**，参数走查询串。
    //
    // 旧实现用 `http://127.0.0.1/`（无服务器）+ eval 注入 → 页面从不执行、
    // 注入的 __req 又被 document.write 冲掉。详见 SOLVER_PAGE_PATH 的注释。
    let url = format!(
        "http://127.0.0.1:{port}{SOLVER_PAGE_PATH}?id={id}&scene={}&region={}&prefix={}",
        url_encode(scene),
        url_encode(region),
        url_encode(prefix)
    );

    let win = WebviewWindowBuilder::new(
        app,
        &label,
        WebviewUrl::External(url.parse().map_err(|e| format!("URL 解析失败: {e}"))?),
    )
    .title("captcha")
    .visible(false) // 隐藏：用户不该看到求解过程
    .skip_taskbar(true)
    .decorations(false)
    .inner_size(480.0, 360.0)
    .build()
    .map_err(|e| format!("创建求解窗口失败: {e}"))?;
    // 计时日志：让"优化有没有效果"有据可查，而不是靠感觉。
    //
    // ⚠ 这个耗时**不代表页面已就绪**（实测只在 0.5~7ms，那只是登记了窗口）。
    // 真正的就绪信号是页面 fetch 到 /result —— 见 handle_result。
    log_info(&format!("求解窗口已创建（{:?}）id={id}", t0.elapsed()));

    let _ = win;
    Ok(())
}

/// 极简的 URL 查询串转义（参数都是 scene/region/prefix 这类短标识）。
///
/// 不引 `urlencoding` crate：本模块只需转义极少数保留字符，
/// 而为一行逻辑加一个依赖不划算（`url` crate 已用于解析，但它的
/// form-urlencoded 编码器面向表单，这里手写更直观）。
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// ⚠ 已删除的 `captcha_webview_result` 命令（2026-09-21）
///
/// 它原本是求解页回报 param 的入口（页面走 `window.__TAURI_INTERNALS__.invoke`）。
/// 现在页面改用**同源 `fetch("/result")`**，那条路已不存在，命令一并删除。
///
/// # 为什么必须删而不是留着
///
/// 它是**能用的**（若页面还调它），但保留两个入口会让"结果从哪来"变得含糊；
/// 更实际的是：它会诱使后来者继续用 IPC，而 IPC 在这里有两个坑
///（capability 白名单静默拦截、`document.write` 冲掉注入变量，详见
/// `SOLVER_PAGE_PATH` 的注释）。删掉入口，就不太可能再走回去。
///
/// 注意 `lib.rs` 的 `invoke_handler` 里也必须同步移除，否则编译失败 ——
/// 这也是一种保护：漏删会立刻暴露。

/// Tauri 命令：查询求解服务状态（供界面/诊断展示）。
#[tauri::command]
pub fn captcha_webview_status(state: tauri::State<'_, CaptchaWebviewState>) -> serde_json::Value {
    serde_json::json!({
        "url": state.solver_url(),
        "inflight": state.inflight(),
        // ⚠ 不返回 token：前端不需要它，暴露只会增加泄漏面
        "ready": !state.solver_url().is_empty(),
    })
}

/// 预热：在后台建一个隐藏窗口并加载官方 SDK，让"冷启动成本"提前付掉。
///
/// # 为什么需要它（2026-09-21 所有者反馈「首次对话 29 秒」）
///
/// 那 29 秒由三段**首次才付**的成本组成：
///
///  - 1. 建 WebView2 窗口 —— 初始化整个渲染环境（最长的一段）
///  - 2. 下载官方 SDK（225KB）+ pe 字节码（约 450KB）—— 首次无 HTTP 缓存
///  - 3. 池预热是串行的（补 3 个 = 3 × 单次耗时）
///
/// 1 与 2 **只付一次**：WebView2 运行时与同一 app 的 HTTP 缓存都是共享的
///（实测缓存目录 `%LOCALAPPDATA%\com.momo0410.aigateway\EBWebView\Default\Cache`
/// 里已有 4.1MB 的 SDK 数据）。故只要**在用户发请求之前**先跑一次，
/// 之后的求解就只付"重建页面"的毫秒级成本。
///
/// 本函数在 setup 里调用（网关启动前），与用户的首次对话形成竞速 ——
/// 通常用户要点开界面、选模型、发消息，几秒内不会发出请求，够预热跑完。
///
/// # 为什么用独立窗口而不是与求解共用
///
/// 求解窗口是**一次性**的（求出 param 就关，见 spawn_solver_window）。
/// 预热窗口若共用，会在第一次求解后被关掉，预热效果只生效一次。
/// 故用独立常驻窗口（label `captcha-warmup`），它只负责把 SDK 拉进缓存。
///
/// 预热窗口**失败不影响功能** —— 它只是优化，求解路径本身不依赖它。
pub fn warmup_prefetch(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        // 稍等：让主窗口先完成创建，避免启动瞬间抢资源（用户看到的是主界面先出来）。
        tokio::time::sleep(Duration::from_millis(800)).await;
        if let Err(e) = do_warmup(&app) {
            // 只记日志，不报错 —— 预热失败时求解仍会在首次请求时正常走一遍
            log_info(&format!("预热未成功（不影响功能，首次求解会稍慢）：{e}"));
        }
    });
}

fn do_warmup(app: &tauri::AppHandle) -> Result<(), String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};

    let t0 = std::time::Instant::now();
    let win = WebviewWindowBuilder::new(
        app,
        "captcha-warmup",
        WebviewUrl::External(
            "http://127.0.0.1/".parse().map_err(|e| format!("URL 解析失败: {e}"))?,
        ),
    )
    .title("captcha-warmup")
    .visible(false)
    .skip_taskbar(true)
    .decorations(false)
    .inner_size(320.0, 240.0)
    .build()
    .map_err(|e| format!("创建预热窗口失败: {e}"))?;

    // 只加载 SDK 本身（不 init、不求解）。
    //
    // # 为什么连页面一起 write 进去，而不是只 appendChild 一个 script
    //
    // 窗口 URL 是 `http://127.0.0.1/`（**一个没有服务器在听的地址**），
    // 导航会失败，此时 `document.head` / `document.body` 未必存在 ——
    // 直接 `document.head.appendChild` 会抛异常，预热就白做了。
    //
    // 与求解窗口同一手法：用 `document.open/write/close` 铺一个最小页面，
    // 再把 SDK 的 script 标签写进去（写 HTML 里的 `<script src>` 会真的发起请求）。
    //
    // 为什么不连 init 一起做：init 会真的发起一次验证（产生上游请求），
    // 而预热阶段我们还没有 scene/region/prefix（那是上游下发的运营参数，
    // 网关拿到后才会随请求传过来）。且无谓的验证请求本身就是风控关注点。
    //
    // 只把 SDK 拉进 HTTP 缓存，就足以消掉"首次下载"那段成本。
    let js = r##"
      (function(){
        try {
          document.open();
          document.write('<!DOCTYPE html><html><head><meta charset="utf-8"></head><body>' +
            '<script src="https://o.alicdn.com/captcha-frontend/aliyunCaptcha/AliyunCaptcha.js"><\/script>' +
            '</body></html>');
          document.close();
        } catch (e) {}
      })();
    "##;
    win.eval(js)
        .map_err(|e| format!("注入预热脚本失败: {e}"))?;

    log_info(&format!(
        "预热窗口已创建（{:?}）—— 官方 SDK 开始进缓存，后续求解无需再等待下载",
        t0.elapsed()
    ));
    // 窗口**不关**：它常驻以保留渲染环境与缓存预热效果。
    // 资源占用很小（隐藏窗口 + 一个已加载的 script），远小于每次求解重建。
    Ok(())
}

/// 把字符串转成 JSON 字面量（含转义）。
fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn log_info(msg: &str) {
    eprintln!("[captcha-webview] {msg}");
}
fn log_err(msg: &str) {
    eprintln!("[captcha-webview][ERROR] {msg}");
}
