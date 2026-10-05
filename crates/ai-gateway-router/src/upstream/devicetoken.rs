//! 向宿主索取 WorkBuddy 设备 token（`X-Device-Token`）。
//!
//! 对应 Go 源文件 `internal/upstream/devicetoken.go`（141 行），逐条对齐。
//!
//! # 背景
//!
//! 官方 WorkBuddy 客户端**每个请求**都带 `X-Device-Token`（`v3:` 形态，
//! 约 1030 字符），而网关此前完全不带。该 token 由官方自带的腾讯
//! TuringShield SDK 生成 —— 网关是独立进程，无法直接加载那个 N-API 模块，
//! 故采用与 ZCode 验证码**完全相同**的架构：宿主开本地服务，网关 HTTP 调用。
//!
//! ```text
//! 网关 ──HTTP POST /device-token──▶ 宿主本地服务（`src-tauri/src/device_token_server.rs`）
//!                                    │
//!                                    └─ Node 子进程加载 turing_sdk.node 返回 v3 token
//!   ◀──────── {"ok":true,"tokens":[...]} ───┘
//! ```
//!
//! # 关键实测数据（决定本实现的形状）
//!
//! - 生成耗时：首次 122ms，后续 39~51ms
//! - **每次调用都不同**（按请求签发）⇒ 不能缓存固定值
//!
//! # 失败必须**静默降级**
//!
//! 拿不到 token 时**不发**该头，请求照常发出 —— 理由：
//!
//! - 该头是「锦上添花」（防未来风控标记），**不是准入条件**
//!   （实测：不带它，健康账号照样 200）
//! - 若因拿不到 token 就拒绝请求，等于把「一个可选增强」
//!   变成「新的单点故障」—— 那是拿可用性换完整性，方向错
//!
//! 故所有错误路径都返回 `Err`（由调用方记日志后跳过），**绝不上抛给用户**。

use std::time::Duration;

use crate::upstream::classify::truncate;

/// 调宿主服务的超时。
///
/// 取 3s：宿主侧实测 40~120ms，留足一个数量级余量；
/// 再长会让「宿主卡住」直接拖慢每个聊天请求。
const FETCH_TIMEOUT: Duration = Duration::from_secs(3);

/// 宿主服务响应体读取上限（1 MiB）。
const MAX_BODY: usize = 1 << 20;

/// 官方形态前缀。
///
/// 形态不符就当作失败 —— 宁可漏发，也不发一个上游不认的怪值。
/// （实测官方形态恒为 `v3:` 前缀、约 1030 字符。）
const TOKEN_PREFIX: &str = "v3:";

/// 向宿主服务索取设备 token。
///
/// `None` 表示该特性未启用（未配置 `wb_device_token_url`）。
#[derive(Debug)]
pub struct DeviceTokenProvider {
    url: String,
    token: String,
    /// 串行化并发请求。
    ///
    /// # 为什么不并发取
    ///
    /// TuringShield SDK 的 `configure` 是**进程级单例**且
    /// 「初始化后不可改 channelId」（实测报错原文：
    /// `Turing SDK channelId cannot change after initialization`）。
    /// 宿主侧那个 Node 子进程同样有单例语义，并发调用只会让它们互相干扰。
    /// 而单次生成仅 40~50ms，串行完全够用。
    ///
    /// 用 `tokio::sync::Mutex` 而非 `std::sync::Mutex`：持有期间要 `.await`，
    /// 标准库的锁跨 await 持有会阻塞整个运行时线程。
    gate: tokio::sync::Mutex<()>,
}

impl DeviceTokenProvider {
    /// 构造 provider；`url` 为空（或全空白）时返回 `None`（= 不启用）。
    ///
    /// 与 Go `NewDeviceTokenProvider` 同口径：**空 URL 不启用**，
    /// 而不是「启用了但每次都失败」——后者会让每个国际版请求白等 3s 超时。
    pub fn new(url: &str, token: &str) -> Option<Self> {
        let url = url.trim();
        if url.is_empty() {
            return None;
        }
        Some(Self {
            url: url.to_string(),
            token: token.to_string(),
            gate: tokio::sync::Mutex::new(()),
        })
    }

    /// 该 provider 是否可用（构造成功即恒 true；保留以对齐 Go 的 `Enabled()`）。
    pub fn enabled(&self) -> bool {
        !self.url.is_empty()
    }

    /// 取一个设备 token。
    ///
    /// **任何失败都返回 `Err`**，调用方应把错误当作「本次不发该头」处理，
    /// 而不是拒绝请求。
    pub async fn fetch(&self, http: &reqwest::Client) -> std::result::Result<String, String> {
        // 串行化：见 `gate` 的字段注释。
        let _guard = self.gate.lock().await;

        let resp = http
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header(
                "X-Device-Token-Service-Token",
                self.token.clone(),
            )
            .timeout(FETCH_TIMEOUT)
            .body(r#"{"count":1}"#)
            .send()
            .await
            .map_err(|e| format!("调用宿主设备 token 服务失败: {e}"))?;

        let status = resp.status();
        let raw = resp
            .bytes()
            .await
            .map_err(|e| format!("读取响应失败: {e}"))?;
        let raw = if raw.len() > MAX_BODY {
            &raw[..MAX_BODY]
        } else {
            &raw[..]
        };

        if !status.is_success() {
            return Err(format!(
                "宿主服务返回 {}: {}",
                status.as_u16(),
                truncate(&String::from_utf8_lossy(raw), 160)
            ));
        }

        let out: serde_json::Value = serde_json::from_slice(raw)
            .map_err(|e| format!("解析响应失败: {e}"))?;

        let ok = out.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        let tokens = out
            .get("tokens")
            .and_then(|v| v.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[]);
        if !ok || tokens.is_empty() {
            let msg = out
                .get("error")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("未返回 token");
            return Err(format!("宿主未给出设备 token: {msg}"));
        }

        let tok = tokens[0].as_str().unwrap_or("").trim();
        if !tok.starts_with(TOKEN_PREFIX) {
            return Err(format!(
                "设备 token 形态异常（应以 {TOKEN_PREFIX} 开头，实得 {} 字节）",
                tok.len()
            ));
        }
        Ok(tok.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_url_is_disabled() {
        assert!(DeviceTokenProvider::new("", "tok").is_none());
        assert!(DeviceTokenProvider::new("   ", "tok").is_none());
        let p = DeviceTokenProvider::new("http://127.0.0.1:1/x", "").unwrap();
        assert!(p.enabled(), "空 token 仍应启用（服务端可能不校验）");
    }

    #[test]
    fn url_is_trimmed() {
        let p = DeviceTokenProvider::new("  http://127.0.0.1:9/x  ", " t ").unwrap();
        assert_eq!(p.url, "http://127.0.0.1:9/x");
        assert_eq!(p.token, " t ", "token 原样保留（可能是含空白的密钥）");
    }

    #[tokio::test]
    async fn unreachable_service_degrades_with_error() {
        // 端口 1 上不会有服务：必须返回 Err 而不是 panic，
        // 也不能挂满 3s 之外（connect 失败立即返回）。
        let p = DeviceTokenProvider::new("http://127.0.0.1:1/device-token", "svc").unwrap();
        let http = reqwest::Client::new();
        let err = p.fetch(&http).await.unwrap_err();
        assert!(
            err.contains("调用宿主设备 token 服务失败"),
            "错误应说明是调用失败：{err}"
        );
    }

    #[test]
    fn prefix_check_rejects_malformed() {
        // 直接验证形态判据：官方恒为 `v3:` 前缀。
        let good = "v3:AAAAAaDleH/S5OaPPFdm5gQXdsnAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        assert!(good.starts_with(TOKEN_PREFIX));
        assert!(!"not-a-token".starts_with(TOKEN_PREFIX));
        assert!(!"V3:uppercase".starts_with(TOKEN_PREFIX), "前缀大小写敏感");
    }
}
