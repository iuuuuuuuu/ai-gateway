//! Windows 系统代理编排（从上游 `src-tauri/src/commands/proxy.rs` 的编排层搬到 core）。
//!
//! 为什么代理必须接管系统代理：Trae 的鉴权请求（`api.trae.cn`）不走 Electron
//! `--proxy-server` 命令行代理，但会读 Windows 系统代理（WinINet）。启动本地代理时
//! 把系统代理指向本机端口，Trae 全部流量（含鉴权）才汇入 MITM 代理；停止时必须还原，
//! 否则系统代理仍指向死端口 → **本机全局断网**（签到、Trae 流量全部 10061 失败）。
//!
//! ## 上游 VPN 链式（「开代理后外网打不开」的根因修复）
//!
//! 启动前若已存在**启用的外部系统代理**（用户的 Clash / v2rayN 等 VPN 梯子），
//! 说明用户的流量本来经它出去。我们接管系统代理后会把它挤掉 —— 若不把原值捕获为
//! 「上游」并透传（见 [`crate::modules::device_proxy::upstream`]），外网（google/github）
//! 会直接连不通。因此这里在接管前快照 `(enabled, server, override)`，
//! 停止/崩溃时**原样还原**（不是简单清空）。
//!
//! 非 Windows 平台：系统代理设置无实现，本模块退化为显式的空操作（不报错）。

use std::path::Path;
use std::sync::Mutex;

use crate::modules::device_proxy::upstream::{parse_upstream, UpstreamProxy};

/// 启动前捕获的系统代理快照：`(enabled, server, override)`。
///
/// `None` = 用户本来就没有系统代理（或已还原过）。捕获到 `Some` 说明我们覆盖了
/// 用户的 VPN 设置，停止时必须原样写回。
static PREV_SYSTEM_PROXY: Mutex<Option<(bool, String, String)>> = Mutex::new(None);

/// 首次接管系统代理时写入的默认直连白名单。
///
/// `127.0.0.1;localhost` 让本机服务（如本软件自己的 HTTP server）绕过代理直连；
/// `<local>` 是 WinINet 的「本地地址不走代理」通配符。
pub const DEFAULT_OVERRIDE: &str = "127.0.0.1;localhost;<local>";

/// 记录本软件本次使用的 MITM 端口（`<data_dir>/last_proxy_port.txt`）。
///
/// 两个用途：
/// 1. [`crate::modules::device_proxy::bypass`] 据此判断「系统代理是否指向我们」；
/// 2. 崩溃残留清理据此识别「指向已停止本地代理的残留系统代理」，
///    且**只匹配我们自己写下的端口**，不会误伤用户自己的 VPN 本地代理（如 Clash 7890）。
pub fn record_proxy_port(data_dir: &Path, port: u16) -> Result<(), String> {
    std::fs::create_dir_all(data_dir).map_err(|e| format!("创建数据目录失败: {e}"))?;
    std::fs::write(data_dir.join("last_proxy_port.txt"), port.to_string())
        .map_err(|e| format!("写入代理端口记录失败: {e}"))
}

/// 读取本软件上次使用的 MITM 端口。
pub fn last_proxy_port(data_dir: &Path) -> Option<u16> {
    std::fs::read_to_string(data_dir.join("last_proxy_port.txt"))
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
}

/// 读取启动前的系统代理设置。返回 `(enabled, server, override)`。
/// 若不存在或未启用则返回 `None`（表示用户本来就没有系统代理 / VPN）。
#[cfg(target_os = "windows")]
pub fn get_existing_win_proxy() -> Option<(bool, String, String)> {
    let key = win::KEY_PATH;
    let enable = win::reg_query_value(key, "ProxyEnable")
        .map(|v| v.contains('1'))
        .unwrap_or(false);
    if !enable {
        return None;
    }
    let server = win::reg_query_value(key, "ProxyServer").unwrap_or_default();
    if server.is_empty() {
        return None;
    }
    let override_ = win::reg_query_value(key, "ProxyOverride").unwrap_or_default();
    Some((true, server, override_))
}

#[cfg(not(target_os = "windows"))]
pub fn get_existing_win_proxy() -> Option<(bool, String, String)> {
    None
}

/// 接管系统代理前的准备：快照用户原有代理并返回可作为「上游」透传的规格。
///
/// 返回 `Some(spec)` 表示捕获到用户的外部代理（VPN 梯子），调用方应把它作为
/// [`crate::modules::device_proxy::ProxyConfig::upstream`] 透传；`None` 表示用户
/// 本来就没有系统代理。
///
/// **排除自身**：若当前系统代理已经指向本软件端口（重复启动 / 上次未还原），
/// 不把它当作上游 —— 否则会形成「代理指向自己」的死循环。
#[cfg(target_os = "windows")]
pub fn capture_previous_proxy(proxy_addr: &str, port: u16) -> Option<String> {
    let mut guard = PREV_SYSTEM_PROXY.lock().unwrap_or_else(|e| e.into_inner());
    match get_existing_win_proxy() {
        Some((en, sv, ov)) if sv != proxy_addr && !sv.contains(&format!("127.0.0.1:{port}")) => {
            // 这是外部代理(VPN)，作为上游透传，并在停止时还原
            *guard = Some((en, sv.clone(), ov));
            Some(sv)
        }
        _ => {
            *guard = None;
            None
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub fn capture_previous_proxy(_proxy_addr: &str, _port: u16) -> Option<String> {
    None
}

/// 解析上游代理规格（把 [`capture_previous_proxy`] 的返回值转成连接器入参）。
pub fn upstream_from_capture(spec: Option<String>) -> Option<UpstreamProxy> {
    spec.as_deref().and_then(parse_upstream)
}

/// 把系统代理指向本机 MITM 端口。
#[cfg(target_os = "windows")]
pub fn set_win_proxy(addr: &str) -> Result<(), String> {
    apply_proxy(true, addr, DEFAULT_OVERRIDE)
}

#[cfg(not(target_os = "windows"))]
pub fn set_win_proxy(_addr: &str) -> Result<(), String> {
    Err("仅 Windows 支持系统代理设置".into())
}

/// 关闭系统代理（`ProxyEnable=0`，并清空 server / override）。
#[cfg(target_os = "windows")]
pub fn clear_win_proxy() -> Result<(), String> {
    apply_proxy(false, "", "")
}

#[cfg(not(target_os = "windows"))]
pub fn clear_win_proxy() -> Result<(), String> {
    Err("仅 Windows 支持系统代理设置".into())
}

/// 还原系统代理：启动前存在启用的外部代理（用户 VPN 梯子）→ **原样还原**，
/// 否则清空系统代理（避免本机全局断网）。
///
/// `consume = true` 取走快照（主动停止 / 应用退出，一次性）；
/// `consume = false` 仅窥视（崩溃看门狗不消费，保留给后续 stop 继续还原）。
#[cfg(target_os = "windows")]
pub fn restore_system_proxy(consume: bool) -> Result<(), String> {
    let prev = {
        let mut g = PREV_SYSTEM_PROXY.lock().unwrap_or_else(|e| e.into_inner());
        if consume {
            g.take()
        } else {
            g.clone()
        }
    };
    match prev {
        Some((en, sv, ov)) if en => apply_proxy(true, &sv, &ov),
        _ => clear_win_proxy(),
    }
}

#[cfg(not(target_os = "windows"))]
pub fn restore_system_proxy(_consume: bool) -> Result<(), String> {
    Ok(())
}

/// 应用退出路径专用：仅当「我们曾接管系统代理」时才还原，避免误关用户自己的梯子。
///
/// - 捕获到用户 VPN 原值 → 原样还原（consume，一次性）；
/// - 无 VPN 原值但代理在运行 → 我们曾把系统代理指向本机端口 → 清空；
/// - 两者皆无（代理从未启动 / 已正常停止并还原过）→ **不触碰系统代理**。
#[cfg(target_os = "windows")]
pub fn restore_system_proxy_on_exit(proxy_was_running: bool) -> Result<(), String> {
    let prev = PREV_SYSTEM_PROXY.lock().unwrap_or_else(|e| e.into_inner()).take();
    match prev {
        Some((true, sv, ov)) => apply_proxy(true, &sv, &ov),
        // 防御分支：快照存在但未启用（正常路径不会出现，get_existing 仅存启用项）
        Some(_) => clear_win_proxy(),
        None if proxy_was_running => clear_win_proxy(),
        None => Ok(()),
    }
}

#[cfg(not(target_os = "windows"))]
pub fn restore_system_proxy_on_exit(_proxy_was_running: bool) -> Result<(), String> {
    Ok(())
}

/// 直开应用前的防御：若系统代理仍指向本机「我们上次使用的端口」而本地代理已停止
/// （应用异常退出等场景可能未还原），提前清除，避免 Trae 全部请求 `ERR_CONNECTION_RESET`。
///
/// 只匹配我们自己写盘记录的端口，**不会误伤用户自己的 VPN 本地代理**（如 Clash 7890）。
#[cfg(target_os = "windows")]
pub fn cleanup_stale_local_proxy(data_dir: &Path) -> Option<String> {
    let key = win::KEY_PATH;
    let enable = win::reg_query_value(key, "ProxyEnable")
        .map(|v| v.contains('1'))
        .unwrap_or(false);
    if !enable {
        return None;
    }
    let server = win::reg_query_value(key, "ProxyServer").unwrap_or_default();
    let port = last_proxy_port(data_dir)?;
    let ours = format!("127.0.0.1:{port}");
    if !server.contains(&ours) {
        return None;
    }
    match clear_win_proxy() {
        Ok(()) => Some(format!("已清理指向已停止本地代理的残留系统代理({ours})")),
        Err(e) => Some(format!("清理残留系统代理失败: {e}")),
    }
}

#[cfg(not(target_os = "windows"))]
pub fn cleanup_stale_local_proxy(_data_dir: &Path) -> Option<String> {
    None
}

#[cfg(target_os = "windows")]
fn apply_proxy(enable: bool, server: &str, override_: &str) -> Result<(), String> {
    let key = win::KEY_PATH;
    win::run_reg(key, "ProxyEnable", "REG_DWORD", if enable { "1" } else { "0" })?;
    if enable {
        win::run_reg(key, "ProxyServer", "REG_SZ", server)?;
        // ProxyOverride 一律**原样写回**：还原路径（stop / 看门狗）必须保留捕获到的
        // 用户原值（含空值），否则会把用户原本为空的排除列表改写成默认白名单。
        // 「空串填默认白名单」仅在 set_win_proxy 首次设置路径由调用方显式传入。
        win::run_reg(key, "ProxyOverride", "REG_SZ", override_)?;
    }
    win::notify_wininet_changed();
    Ok(())
}

// ---------------------------------------------------------------------------
// Windows 注册表工具
// ---------------------------------------------------------------------------
#[cfg(target_os = "windows")]
mod win {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    pub(super) const KEY_PATH: &str =
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";

    /// 读注册表值。注意 `reg query` 输出形如 `    ProxyServer    REG_SZ    127.0.0.1:7890`，
    /// 值本身可能含空格（`ProxyServer` 的 `http=...;https=...` 形态），故取「类型列之后的全部」。
    pub(super) fn reg_query_value(key: &str, name: &str) -> Option<String> {
        let out = Command::new("reg")
            .args(["query", key, "/v", name])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let t = line.trim_start();
            let Some(rest) = t.strip_prefix(name) else { continue };
            // 必须紧跟空白，避免 ProxyEnable 命中 ProxyEnableXxx 之类的长键名
            if !rest.starts_with(char::is_whitespace) {
                continue;
            }
            let parts: Vec<&str> = rest.split_whitespace().collect();
            // parts[0] = 类型(REG_SZ/REG_DWORD)，parts[1..] = 值
            if parts.len() >= 2 {
                return Some(parts[1..].join(" "));
            }
        }
        None
    }

    pub(super) fn run_reg(key: &str, name: &str, ty: &str, value: &str) -> Result<(), String> {
        let out = Command::new("reg")
            .args(["add", key, "/v", name, "/t", ty, "/d", value, "/f"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| format!("设置系统代理失败: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "reg add 失败: {name} ({})",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    /// 通知 WinINet 代理设置已变更，让运行中的进程立即生效。
    /// 不调用此函数的话，已有进程会继续使用缓存的旧代理设置。
    pub(super) fn notify_wininet_changed() {
        #[link(name = "wininet")]
        extern "system" {
            fn InternetSetOptionW(
                h_internet: *mut std::ffi::c_void,
                option: u32,
                buffer: *mut std::ffi::c_void,
                buffer_length: u32,
            ) -> i32;
        }
        const INTERNET_OPTION_SETTINGS_CHANGED: u32 = 39;
        const INTERNET_OPTION_REFRESH: u32 = 37;

        unsafe {
            InternetSetOptionW(
                std::ptr::null_mut(),
                INTERNET_OPTION_SETTINGS_CHANGED,
                std::ptr::null_mut(),
                0,
            );
            InternetSetOptionW(
                std::ptr::null_mut(),
                INTERNET_OPTION_REFRESH,
                std::ptr::null_mut(),
                0,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ai_gateway_sysproxy_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// 端口记录：写入后可读回，尾随空白容错，脏值返回 None
    #[test]
    fn proxy_port_roundtrip() {
        let dir = temp_dir("port");
        record_proxy_port(&dir, 8899).unwrap();
        assert_eq!(last_proxy_port(&dir), Some(8899));
        std::fs::write(dir.join("last_proxy_port.txt"), " 8899 \r\n").unwrap();
        assert_eq!(last_proxy_port(&dir), Some(8899));
        std::fs::write(dir.join("last_proxy_port.txt"), "abc").unwrap();
        assert_eq!(last_proxy_port(&dir), None);
        std::fs::remove_file(dir.join("last_proxy_port.txt")).unwrap();
        assert_eq!(last_proxy_port(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 端口记录会创建缺失的数据目录（首次启动场景）
    #[test]
    fn record_proxy_port_creates_data_dir() {
        let dir = temp_dir("mk").join("nested");
        record_proxy_port(&dir, 1).unwrap();
        assert!(dir.join("last_proxy_port.txt").exists());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// 上游捕获结果 → 连接器入参：空 / None 都得到 None（不制造假上游）
    #[test]
    fn upstream_from_capture_handles_empty() {
        assert_eq!(upstream_from_capture(None), None);
        assert_eq!(upstream_from_capture(Some(String::new())), None);
        assert_eq!(
            upstream_from_capture(Some("127.0.0.1:7890".to_string())),
            Some(UpstreamProxy::Http("127.0.0.1:7890".to_string()))
        );
        assert_eq!(
            upstream_from_capture(Some("socks=127.0.0.1:7891".to_string())),
            Some(UpstreamProxy::Socks5("127.0.0.1:7891".to_string()))
        );
    }

    /// 默认白名单必须包含本机回环与 `<local>`（否则本软件自己的 API 会被自己代理）
    #[test]
    fn default_override_covers_loopback() {
        assert!(DEFAULT_OVERRIDE.contains("127.0.0.1"));
        assert!(DEFAULT_OVERRIDE.contains("localhost"));
        assert!(DEFAULT_OVERRIDE.contains("<local>"));
    }

    /// 非 Windows 平台：全部编排函数是显式空操作（不 panic、不报错）
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn orchestration_is_noop_off_windows() {
        assert_eq!(get_existing_win_proxy(), None);
        assert_eq!(capture_previous_proxy("127.0.0.1:8899", 8899), None);
        assert!(restore_system_proxy(true).is_ok());
        assert!(restore_system_proxy_on_exit(true).is_ok());
        assert_eq!(cleanup_stale_local_proxy(Path::new(".")), None);
        assert!(set_win_proxy("127.0.0.1:1").is_err());
        assert!(clear_win_proxy().is_err());
    }
}
