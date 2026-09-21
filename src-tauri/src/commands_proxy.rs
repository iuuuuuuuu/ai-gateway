//! 本地代理（MITM 设备身份代理）的 Tauri 命令层。
//!
//! 这一层现在只做一件事：**把 core 的进度/事件回调转发成 Tauri 事件**。
//! 单例托管、系统代理开关顺序、崩溃看门狗全部在
//! [`ai_gateway_core::modules::device_proxy::host_ops`] 里 —— 桌面端与 webui
//! 共用同一份实现，避免「桌面端修了、webui 还是坏的」这类漂移。
//!
//! 注意 **不要**把业务逻辑写回这个文件：一旦写回来，webui 就又拿不到它
//! （`/api/proxy/*` 在浏览器形态长期 404，就是这么来的）。

use std::sync::Arc;

use serde_json::{json, Value};

use ai_gateway_core::modules::device_proxy::host_ops::{self, HostProxySink};

/// 代理事件 → Tauri 事件桥。
///
/// 崩溃时的系统代理还原由 core 负责（那是安全语义，不能交给宿主各写一遍），
/// 这里只负责把事件送出去。
struct TauriSink {
    app: tauri::AppHandle,
}

impl HostProxySink for TauriSink {
    fn log(&self, line: &str) {
        use tauri::Emitter;
        let _ = self.app.emit("proxy-log", json!({ "line": line }));
    }

    fn account_captured(&self, uid: &str) {
        use tauri::Emitter;
        let _ = self.app.emit("account-captured", json!({ "uid": uid }));
    }

    fn crashed(&self, intentional: bool) {
        use tauri::Emitter;
        let _ = self
            .app
            .emit("proxy-crashed", json!({ "intentional": intentional }));
    }
}

/// 读取代理配置（端口、目标域名、上游代理）。
#[tauri::command]
pub fn proxy_config() -> Value {
    host_ops::config_view()
}

/// 代理运行状态。
#[tauri::command]
pub async fn proxy_status() -> Value {
    host_ops::status().await
}

/// 启动代理并接管系统代理。
#[tauri::command(rename_all = "camelCase")]
pub async fn proxy_start(
    app: tauri::AppHandle,
    port: Option<u16>,
    domains: Option<String>,
) -> Result<Value, String> {
    host_ops::start(port, domains, Arc::new(TauriSink { app })).await
}

/// 停止代理并还原系统代理。
#[tauri::command]
pub async fn proxy_stop() -> Result<Value, String> {
    host_ops::stop().await
}

/// CA 证书状态。
#[tauri::command]
pub async fn proxy_cert_status() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(host_ops::cert_status)
        .await
        .map_err(|e| format!("读取证书状态失败: {e}"))?
}

/// 生成自签 CA（若已存在则复用）。
#[tauri::command]
pub async fn proxy_cert_generate() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(host_ops::cert_generate)
        .await
        .map_err(|e| format!("生成 CA 失败: {e}"))?
}

/// 从本机离线捕获 Trae 的 Cloud-IDE-JWT（代理抓不到时的兜底）。
#[tauri::command]
pub async fn proxy_capture_local() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(host_ops::capture_local)
        .await
        .map_err(|e| format!("本地捕获失败: {e}"))?
}

/// 清理上一次异常退出残留的系统代理设置。
#[tauri::command]
pub async fn proxy_cleanup_stale() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(host_ops::cleanup_stale)
        .await
        .map_err(|e| format!("清理残留代理失败: {e}"))?
}

/// 把上游代理规格字符串解析成结构（供界面校验用户输入）。
#[tauri::command]
pub fn proxy_parse_upstream(spec: String) -> Value {
    host_ops::parse_upstream(&spec)
}
