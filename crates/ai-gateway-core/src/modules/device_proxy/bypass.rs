//! OAuth 登录代理直连豁免（上游 F-78 批次 2，issue #10 根因①）。
//!
//! MITM 代理运行时系统代理指向 `127.0.0.1:<port>`，浏览器 OAuth 登录页流量会被
//! 自签 CA 解密，CA 未受信即报 `ERR_CERT_AUTHORITY_INVALID`。
//! 发起 OAuth 前把登录/鉴权域名追加进系统代理 `ProxyOverride`（直连白名单），
//! 登录结束只移除本次追加的条目，**绝不触碰用户原有配置**。
//!
//! 仅当系统代理确实指向本软件 MITM 端口（`<data_dir>/last_proxy_port.txt` 记录）时才改写；
//! 用户自己的远程代理（VPN 等）不会被本软件解密，无需也绝不改动。

use std::path::Path;
use std::sync::Mutex;

/// OAuth 登录链路需直连的域名（登录页 / 鉴权 API / 本机回调）
pub const OAUTH_BYPASS_ENTRIES: &[&str] = &[
    "www.trae.cn",
    "api.trae.cn",
    "api.trae.com.cn",
    "127.0.0.1",
    "localhost",
];

/// 本次已追加进 `ProxyOverride` 的条目（disable 时只删这些，保证幂等且不误删用户配置）
static ADDED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 崩溃残留标记（enable 时写入 / disable 时删除）：进程异常退出未来得及还原时，
/// 下次启动据此只清理本软件追加过的条目（缺陷13）
pub fn marker_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("oauth_bypass_pending.json")
}

fn save_marker(data_dir: &Path, added: &[String]) {
    if let Ok(json) = serde_json::to_string(added) {
        let _ = std::fs::write(marker_path(data_dir), json);
    }
}

/// 当前生效的 MITM 代理端口（与代理启动路径写入的 `last_proxy_port.txt` 对齐）
pub fn mitm_port(data_dir: &Path) -> Option<u16> {
    std::fs::read_to_string(data_dir.join("last_proxy_port.txt"))
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
}

/// 系统代理是否确实指向本软件 MITM 端口。
///
/// 只有「是」才允许改写 `ProxyOverride` —— 用户自己的 VPN / 远程代理不受本软件
/// 解密，改它的白名单既无必要也可能破坏用户配置。
#[cfg(target_os = "windows")]
fn points_at_our_mitm(data_dir: &Path) -> Option<u16> {
    let port = mitm_port(data_dir)?;
    if win::reg_query_value(win::KEY_PATH, "ProxyEnable").as_deref() != Some("0x1") {
        return None;
    }
    let server = win::reg_query_value(win::KEY_PATH, "ProxyServer").unwrap_or_default();
    (server == format!("127.0.0.1:{port}")).then_some(port)
}

/// 把 `OAUTH_BYPASS_ENTRIES` 合并进现有 `ProxyOverride`，返回 (合并结果, 本次新增)。
///
/// 纯函数（无注册表副作用），便于单测覆盖「不误删用户条目」「大小写不敏感去重」。
pub fn merge_bypass_entries(current: &str) -> (Vec<String>, Vec<String>) {
    let existing: Vec<String> = current
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    let mut merged = existing.clone();
    let mut added: Vec<String> = Vec::new();
    for e in OAUTH_BYPASS_ENTRIES {
        if !existing.iter().any(|x| x.eq_ignore_ascii_case(e)) {
            merged.push((*e).to_string());
            added.push((*e).to_string());
        }
    }
    (merged, added)
}

/// 从 `ProxyOverride` 中移除指定条目，返回剩余列表。
///
/// 纯函数：只删「本次追加过的」条目（大小写不敏感），其余原样保留 ——
/// 这是「绝不触碰用户原有配置」这条约束的落地点。
pub fn remove_bypass_entries(current: &str, added: &[String]) -> Vec<String> {
    current
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty() && !added.iter().any(|a| a.eq_ignore_ascii_case(s)))
        .map(|s| s.to_string())
        .collect()
}

/// 启动时清理崩溃残留（宿主 setup 调用）：上次进程未正常还原 `ProxyOverride` 时，
/// 按标记文件只移除本软件追加的条目；无标记则幂等空操作。
#[cfg(target_os = "windows")]
pub fn cleanup_residual_bypass(data_dir: &Path) {
    let marker = marker_path(data_dir);
    let Ok(json) = std::fs::read_to_string(&marker) else {
        return;
    };
    let Ok(added) = serde_json::from_str::<Vec<String>>(&json) else {
        // 标记损坏：删掉即可，残留条目（若有）由用户手动处理，不再二次猜测
        let _ = std::fs::remove_file(&marker);
        return;
    };
    if added.is_empty() {
        let _ = std::fs::remove_file(&marker);
        return;
    }
    let key = win::KEY_PATH;
    if win::reg_query_value(key, "ProxyEnable").as_deref() == Some("0x1") {
        let current = win::reg_query_value(key, "ProxyOverride").unwrap_or_default();
        let remaining = remove_bypass_entries(&current, &added);
        if win::run_reg(key, "ProxyOverride", "REG_SZ", &remaining.join(";")).is_err() {
            // 失败保留标记，下次启动重试
            return;
        }
        let _ = win::notify_wininet_changed();
    }
    let _ = std::fs::remove_file(&marker);
}

#[cfg(not(target_os = "windows"))]
pub fn cleanup_residual_bypass(_data_dir: &Path) {}

/// 发起 OAuth 前启用直连豁免。返回 `Ok(true)` 表示本次实际修改了系统代理白名单。
#[cfg(target_os = "windows")]
pub fn enable_oauth_bypass(data_dir: &Path) -> Result<bool, String> {
    // 本软件未启动过 MITM 代理 / 系统代理不是我们 → 无需豁免
    if points_at_our_mitm(data_dir).is_none() {
        return Ok(false);
    }
    let key = win::KEY_PATH;

    let mut added_guard = ADDED.lock().unwrap_or_else(|e| e.into_inner());
    if !added_guard.is_empty() {
        // 已豁免（幂等）：上次 enable 后未还原
        return Ok(true);
    }
    let current = win::reg_query_value(key, "ProxyOverride").unwrap_or_default();
    let (merged, added) = merge_bypass_entries(&current);
    if added.is_empty() {
        return Ok(false);
    }
    win::run_reg(key, "ProxyOverride", "REG_SZ", &merged.join(";"))?;
    win::notify_wininet_changed();
    // 持久化追加清单：进程崩溃未还原时下次启动清理（缺陷13）
    *added_guard = added.clone();
    save_marker(data_dir, &added);
    Ok(true)
}

/// OAuth 结束后还原直连豁免：只移除本次追加的条目（未修改过则幂等成功）
#[cfg(target_os = "windows")]
pub fn disable_oauth_bypass(data_dir: &Path) -> Result<(), String> {
    let key = win::KEY_PATH;

    let mut added = ADDED.lock().unwrap_or_else(|e| e.into_inner());
    if added.is_empty() {
        return Ok(());
    }
    // 系统代理已被关闭/还原（如 MITM 代理停止时整体还原了快照）时豁免已随之失效，直接清账
    if win::reg_query_value(key, "ProxyEnable").as_deref() == Some("0x1") {
        let current = win::reg_query_value(key, "ProxyOverride").unwrap_or_default();
        let remaining = remove_bypass_entries(&current, &added);
        win::run_reg(key, "ProxyOverride", "REG_SZ", &remaining.join(";"))?;
        win::notify_wininet_changed();
    }
    added.clear();
    let _ = std::fs::remove_file(marker_path(data_dir));
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn enable_oauth_bypass(_data_dir: &Path) -> Result<bool, String> {
    // 非 Windows 平台暂无 MITM 系统代理设置逻辑，无需豁免
    Ok(false)
}

#[cfg(not(target_os = "windows"))]
pub fn disable_oauth_bypass(_data_dir: &Path) -> Result<(), String> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Windows 注册表工具（与 `sys_proxy` 同款实现；该模块函数为私有，此处复刻）
// ---------------------------------------------------------------------------
#[cfg(target_os = "windows")]
mod win {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    pub(super) const KEY_PATH: &str =
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";

    pub(super) fn reg_query_value(key: &str, name: &str) -> Option<String> {
        let out = Command::new("reg")
            .args(["query", key, "/v", name])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines()
            .find(|l| l.trim_start().starts_with(name))
            .and_then(|l| l.split_whitespace().last().map(|s| s.to_string()))
    }

    pub(super) fn run_reg(key: &str, name: &str, ty: &str, value: &str) -> Result<(), String> {
        let out = Command::new("reg")
            .args(["add", key, "/v", name, "/t", ty, "/d", value, "/f"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).to_string())
        }
    }

    /// 通知 WinINet 系统代理设置已变化（否则浏览器可能继续用旧配置）
    pub(super) fn notify_wininet_changed() {
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

    /// 合并：用户原有条目一个都不能少，本软件条目追加在后且不重复
    #[test]
    fn merge_keeps_user_entries_and_appends_ours() {
        let (merged, added) = merge_bypass_entries("*.local;192.168.*");
        assert_eq!(merged[0], "*.local", "用户原有条目必须保持在最前");
        assert_eq!(merged[1], "192.168.*");
        for e in OAUTH_BYPASS_ENTRIES {
            assert!(merged.iter().any(|m| m == e), "缺少豁免条目 {e}");
            assert!(added.iter().any(|a| a == e));
        }
        assert_eq!(added.len(), OAUTH_BYPASS_ENTRIES.len());
    }

    /// 合并幂等：已在列表中的条目不再追加（大小写不敏感）
    #[test]
    fn merge_is_idempotent_and_case_insensitive() {
        let (merged, added) = merge_bypass_entries("WWW.TRAE.CN;localhost");
        assert!(!added.iter().any(|a| a.eq_ignore_ascii_case("www.trae.cn")));
        assert!(!added.iter().any(|a| a.eq_ignore_ascii_case("localhost")));
        assert_eq!(
            merged.iter().filter(|m| m.eq_ignore_ascii_case("www.trae.cn")).count(),
            1,
            "不得产生重复条目"
        );
        // 二次合并（用第一次结果再走一遍）应零新增
        let (_, added2) = merge_bypass_entries(&merged.join(";"));
        assert!(added2.is_empty(), "二次合并应零新增: {added2:?}");
    }

    /// 空 / 脏输入：不 panic，空段被丢弃
    #[test]
    fn merge_handles_empty_and_dirty_input() {
        let (merged, added) = merge_bypass_entries("");
        assert_eq!(merged.len(), OAUTH_BYPASS_ENTRIES.len());
        assert_eq!(added.len(), OAUTH_BYPASS_ENTRIES.len());

        let (merged2, _) = merge_bypass_entries(";;  ;a;;b;");
        assert_eq!(merged2[0], "a");
        assert_eq!(merged2[1], "b");
    }

    /// 移除：只删本次追加的条目，用户原有条目（含同名不同大小写）原样保留
    #[test]
    fn remove_only_deletes_what_we_added() {
        let (merged, added) = merge_bypass_entries("*.local;192.168.*");
        let remaining = remove_bypass_entries(&merged.join(";"), &added);
        assert_eq!(remaining, vec!["*.local".to_string(), "192.168.*".to_string()]);

        // 用户自己也有 localhost（大小写不同）→ 属于「本次新增清单」命中，按设计删除；
        // 但用户独有的条目必须保留
        let remaining2 = remove_bypass_entries("keep.me;127.0.0.1", &["127.0.0.1".to_string()]);
        assert_eq!(remaining2, vec!["keep.me".to_string()]);
    }

    /// 移除幂等：重复调用不报错、结果稳定
    #[test]
    fn remove_is_idempotent() {
        let added = vec!["localhost".to_string()];
        let once = remove_bypass_entries("a;localhost;b", &added);
        let twice = remove_bypass_entries(&once.join(";"), &added);
        assert_eq!(once, twice);
    }

    /// 端口记录文件：缺失 / 脏值都返回 None（不误判为「系统代理指向我们」）
    #[test]
    fn mitm_port_reads_only_valid_port_file() {
        let dir = std::env::temp_dir().join(format!(
            "ai_gateway_bypass_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(mitm_port(&dir), None, "无文件应为 None");
        std::fs::write(dir.join("last_proxy_port.txt"), "8899\n").unwrap();
        assert_eq!(mitm_port(&dir), Some(8899), "应容忍尾随空白");
        std::fs::write(dir.join("last_proxy_port.txt"), "not-a-port").unwrap();
        assert_eq!(mitm_port(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 标记文件路径落在数据目录下（崩溃残留清理的定位依据）
    #[test]
    fn marker_lives_in_data_dir() {
        let p = marker_path(Path::new("C:\\data"));
        assert_eq!(p, Path::new("C:\\data").join("oauth_bypass_pending.json"));
    }

    /// 非 Windows 平台：豁免是显式空操作（不报错，也不改任何系统设置）
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn bypass_is_noop_off_windows() {
        let dir = std::env::temp_dir();
        assert_eq!(enable_oauth_bypass(&dir).unwrap(), false);
        assert!(disable_oauth_bypass(&dir).is_ok());
        cleanup_residual_bypass(&dir);
    }
}
