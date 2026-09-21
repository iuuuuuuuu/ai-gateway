//! 本地代理与计划任务的 HTTP 处理器（webui 形态）。
//!
//! 与 [`super::apps_api`] 同样的思路：前端 `api.ts` 早已为 `/api/proxy/*`
//! 与 `/api/tasks/*` 登记了路由，但服务端一条都没有 —— webui 下设置页的
//! 「本地代理」与「计划任务」两块面板点了就是 404。
//!
//! 业务逻辑全在 core：代理在
//! [`ai_gateway_core::modules::device_proxy::host_ops`]，
//! 计划任务在 [`ai_gateway_core::modules::apps_ops`]。
//! 这里只做「取参数 → 调用 → 转 JSON」。
//!
//! ## 代理跑在服务进程内
//!
//! 与桌面端同构：一个进程一个实例。webui 点「启动代理」后，代理归
//! `ai-gateway` 服务进程管；关掉服务进程代理随之消失（属意外退出，
//! 由 core 的崩溃看门狗负责还原系统代理）。
//!
//! ## 计划任务的启动器指向服务自己
//!
//! `task_register` 把 `exe` 写成 `ai-gateway` 服务本体的路径 ——
//! 它是 `--task-run <key>` 的宿主（见 `main.rs`），不依赖桌面端是否安装。

use std::sync::{Arc, Mutex};

use axum::extract::Json;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};

use ai_gateway_core::modules::apps_ops;
use ai_gateway_core::modules::device_proxy::host_ops::{self, HostProxySink};

use super::{json_err, json_ok};

/// webui 没有事件总线，代理日志写进这里供前端轮询。
static PROXY_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 轮询缓存最多保留的行数（防止长时间运行把内存吃满）。
const PROXY_LOG_CAP: usize = 200;

/// 把代理事件写进轮询缓存。
struct CacheSink;

impl HostProxySink for CacheSink {
    fn log(&self, line: &str) {
        if let Ok(mut log) = PROXY_LOG.lock() {
            log.push(line.to_string());
            // 超出上限时丢掉最旧的
            if log.len() > PROXY_LOG_CAP {
                let drop = log.len() - PROXY_LOG_CAP;
                log.drain(..drop);
            }
        }
    }

    fn account_captured(&self, uid: &str) {
        self.log(&format!("捕获账号 {uid}"));
    }

    fn crashed(&self, intentional: bool) {
        if !intentional {
            self.log("代理意外退出");
        }
    }
}

/// 取一个必填的字符串参数。
fn require_str(body: &Value, key: &str) -> Result<String, String> {
    body.get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("缺少参数 {key}"))
}

/// 把 blocking 结果统一转成响应。
fn blocking_response(result: Result<Result<Value, String>, tokio::task::JoinError>) -> Response {
    match result {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

// ---------------------------------------------------------------------------
// 本地代理
// ---------------------------------------------------------------------------

/// GET /api/proxy/config —— 代理配置。
pub async fn api_proxy_config() -> Response {
    match tokio::task::spawn_blocking(host_ops::config_view).await {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// GET /api/proxy/status —— 代理运行状态。
pub async fn api_proxy_status() -> Response {
    json_ok(host_ops::status().await)
}

/// POST /api/proxy/start —— 启动代理并接管系统代理。
pub async fn api_proxy_start(Json(body): Json<Value>) -> Response {
    let port = body.get("port").and_then(Value::as_u64).map(|p| p as u16);
    let domains = body
        .get("domains")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty());
    match host_ops::start(port, domains, Arc::new(CacheSink)).await {
        // 端口被占用、已在运行等都属于「参数/状态问题」，用 400 让界面展示原因
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
        Ok(v) => json_ok(v),
    }
}

/// POST /api/proxy/stop —— 停止代理并还原系统代理。
pub async fn api_proxy_stop() -> Response {
    match host_ops::stop().await {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// GET /api/proxy/cert —— CA 证书状态。
pub async fn api_proxy_cert_status() -> Response {
    blocking_response(tokio::task::spawn_blocking(host_ops::cert_status).await)
}

/// POST /api/proxy/cert/generate —— 生成自签 CA（已存在则复用）。
pub async fn api_proxy_cert_generate() -> Response {
    blocking_response(tokio::task::spawn_blocking(host_ops::cert_generate).await)
}

/// POST /api/proxy/capture-local —— 从本机离线捕获 Trae 的 Cloud-IDE-JWT。
pub async fn api_proxy_capture_local() -> Response {
    blocking_response(tokio::task::spawn_blocking(host_ops::capture_local).await)
}

/// POST /api/proxy/cleanup —— 清理上一次异常退出残留的系统代理设置。
pub async fn api_proxy_cleanup() -> Response {
    blocking_response(tokio::task::spawn_blocking(host_ops::cleanup_stale).await)
}

/// POST /api/proxy/parse-upstream —— 解析上游代理地址（供界面校验输入）。
pub async fn api_proxy_parse_upstream(Json(body): Json<Value>) -> Response {
    let spec = body.get("spec").and_then(Value::as_str).unwrap_or("");
    json_ok(host_ops::parse_upstream(spec))
}

/// GET /api/proxy/progress —— 轮询代理日志（webui 无事件总线）。
pub async fn api_proxy_progress() -> Response {
    let lines = PROXY_LOG.lock().map(|l| l.clone()).unwrap_or_default();
    // 取走即清空：前端每次拿到的是「上次以来新增的行」
    if let Ok(mut guard) = PROXY_LOG.lock() {
        guard.clear();
    }
    json_ok(json!({ "lines": lines }))
}

// ---------------------------------------------------------------------------
// 计划任务
// ---------------------------------------------------------------------------

/// GET /api/tasks/status —— 查询全部计划任务的注册状态。
pub async fn api_task_status() -> Response {
    // schtasks 是子进程调用（可达数秒），必须离开 async 线程
    match tokio::task::spawn_blocking(apps_ops::task_status).await {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/tasks/register —— 注册（或覆盖）一个每日计划任务。
pub async fn api_task_register(Json(body): Json<Value>) -> Response {
    let kind = match require_str(&body, "kind") {
        Ok(v) => v,
        Err(e) => return json_err(e, StatusCode::BAD_REQUEST),
    };
    let time = match require_str(&body, "time") {
        Ok(v) => v,
        Err(e) => return json_err(e, StatusCode::BAD_REQUEST),
    };
    blocking_response(
        tokio::task::spawn_blocking(move || {
            // 启动器指向 ai-gateway 服务本体：它支持 `--task-run <key>`，
            // 因此浏览器形态不需要桌面端也能跑定时任务。
            let exe = std::env::current_exe()
                .map_err(|e| format!("获取主程序路径失败: {e}"))?;
            apps_ops::task_register(&kind, &time, &exe)
        })
        .await,
    )
}

/// POST /api/tasks/unregister —— 删除计划任务。
pub async fn api_task_unregister(Json(body): Json<Value>) -> Response {
    let kind = match require_str(&body, "kind") {
        Ok(v) => v,
        Err(e) => return json_err(e, StatusCode::BAD_REQUEST),
    };
    blocking_response(
        tokio::task::spawn_blocking(move || apps_ops::task_unregister(&kind)).await,
    )
}

/// POST /api/tasks/run —— 立即执行一次任务。
///
/// 任务内部会跑 HTTP 请求或启停客户端，可能耗时数十秒，必须 blocking。
pub async fn api_task_run_now(Json(body): Json<Value>) -> Response {
    let kind = match require_str(&body, "kind") {
        Ok(v) => v,
        Err(e) => return json_err(e, StatusCode::BAD_REQUEST),
    };
    blocking_response(tokio::task::spawn_blocking(move || apps_ops::task_run_now(&kind)).await)
}

/// 供测试与诊断：清空轮询缓存。
#[allow(dead_code)]
pub fn clear_proxy_log() {
    if let Ok(mut log) = PROXY_LOG.lock() {
        log.clear();
    }
}
