//! exe 发现：6 级回退链。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/switcher/locate.rs`。
//!
//! 级别顺序（命中即返回，全部经 `exe_names` 白名单过滤防串台）：
//! 1. 设置里手动指定的路径（`settings_path_key`）
//! 2. 档案内的候选路径（`exe_candidates`）
//! 3. 开始菜单 / 桌面 `.lnk` 快捷方式
//! 4. 注册表 Uninstall 键
//! 5. 正在运行的进程
//! 6. 上次成功启动的缓存
//!
//! 为什么需要这么多级：客户端安装位置五花八门（`%LOCALAPPDATA%\Programs`、
//! `%ProgramFiles%`、自定义盘符、绿色版），任何单一来源都会在部分机器上落空。

use std::path::{Path, PathBuf};

use super::Session;
use crate::modules::app_profile::{exe_matches, AppProfile};

/// 查找客户端 exe。
pub fn find_exe(sess: &Session) -> Result<PathBuf, String> {
    find_exe_cached(sess, &sess.prof)
}

/// 带缓存写入的查找（供 [`Session`] 持有可变缓存时使用）。
pub fn find_exe_cached(sess: &Session, prof: &AppProfile) -> Result<PathBuf, String> {
    // 1. 设置里的手动路径
    if let Some(path) = manual_path(prof) {
        if path.is_file() && exe_matches(&path, prof) {
            return Ok(path);
        }
    }
    // 2. 档案候选
    for candidate in &prof.exe_candidates {
        if candidate.is_file() && exe_matches(candidate, prof) {
            return Ok(candidate.clone());
        }
    }
    // 3. .lnk 快捷方式
    if let Some(path) = from_lnk(prof) {
        return Ok(path);
    }
    // 4. 注册表 Uninstall 键
    if let Some(path) = from_registry(prof) {
        return Ok(path);
    }
    // 5. 正在运行的进程
    if let Some(path) = super::proc::running_exe_of(prof) {
        if exe_matches(&path, prof) {
            return Ok(path);
        }
    }
    // 6. 上次启动缓存
    if let Some(cached) = read_exe_cache(sess, prof) {
        if cached.is_file() && exe_matches(&cached, prof) {
            return Ok(cached);
        }
    }
    Err(format!(
        "未找到 {} 的可执行文件。请在「设置 → 环境配置」中手动指定其安装路径。",
        prof.app_name
    ))
}

/// exe 缓存文件（按应用各一份）。
pub fn exe_cache_file(sess: &Session, prof: &AppProfile) -> PathBuf {
    sess.store_dir
        .join(format!("{}_exe.json", prof.settings_path_key))
}

/// 记录一次成功发现的 exe（下次优先复用，避免重复走完整回退链）。
pub fn save_exe_cache(sess: &Session, prof: &AppProfile, exe: &Path) {
    let content = serde_json::json!({ "exe": exe.to_string_lossy() }).to_string();
    let path = exe_cache_file(sess, prof);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = crate::modules::config::atomic_write(&path, &content);
}

/// 读 exe 缓存。
pub fn read_exe_cache(sess: &Session, prof: &AppProfile) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(exe_cache_file(sess, prof)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let exe = v.get("exe")?.as_str()?.trim();
    (!exe.is_empty()).then(|| PathBuf::from(exe))
}

/// 清除 exe 缓存（手动改路径后调用，避免旧缓存继续命中）。
pub fn clear_exe_cache(sess: &Session, prof: &AppProfile) {
    let _ = std::fs::remove_file(exe_cache_file(sess, prof));
}

/// 应用设置里手动指定的路径。
fn manual_path(prof: &AppProfile) -> Option<PathBuf> {
    let settings = crate::modules::config::load_app_settings();
    let value = settings.get(prof.settings_path_key)?.as_str()?.trim();
    (!value.is_empty()).then(|| PathBuf::from(value))
}

/// 从开始菜单 / 桌面的 `.lnk` 快捷方式解析目标 exe。
fn from_lnk(prof: &AppProfile) -> Option<PathBuf> {
    let roots = lnk_search_roots();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.to_lowercase().ends_with(".lnk") {
                continue;
            }
            if !prof
                .lnk_patterns
                .iter()
                .any(|pattern| glob_match_ci(pattern, name))
            {
                continue;
            }
            if let Some(target) = resolve_lnk(&path) {
                if target.is_file() && exe_matches(&target, prof) {
                    return Some(target);
                }
            }
        }
    }
    None
}

/// `.lnk` 搜索根目录（用户与全局开始菜单 + 桌面）。
fn lnk_search_roots() -> Vec<PathBuf> {
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    let programdata = std::env::var("ProgramData").unwrap_or_default();
    let userprofile = std::env::var("USERPROFILE").unwrap_or_default();
    let mut roots = Vec::new();
    if !appdata.is_empty() {
        roots.push(PathBuf::from(format!(
            "{appdata}\\Microsoft\\Windows\\Start Menu\\Programs"
        )));
    }
    if !programdata.is_empty() {
        roots.push(PathBuf::from(format!(
            "{programdata}\\Microsoft\\Windows\\Start Menu\\Programs"
        )));
    }
    if !userprofile.is_empty() {
        roots.push(PathBuf::from(format!("{userprofile}\\Desktop")));
        roots.push(PathBuf::from(format!("{programdata}\\Desktop")));
    }
    roots.into_iter().filter(|p| p.is_dir()).collect()
}

/// 解析 `.lnk` 目标路径。
///
/// 优先用 COM（`IShellLinkW`）；失败时退回「读取文件内容并正则找 `.exe` 路径」的
/// 尽力而为方案 —— 快捷方式里通常以 UTF-16 明文内嵌目标路径。
fn resolve_lnk(path: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    if let Some(target) = resolve_lnk_com(path) {
        return Some(target);
    }
    resolve_lnk_scan(path)
}

/// 通过 PowerShell 的 WScript.Shell COM 解析快捷方式（无需额外 crate）。
#[cfg(target_os = "windows")]
fn resolve_lnk_com(path: &Path) -> Option<PathBuf> {
    // 单引号包裹并转义内部单引号，避免路径中的引号破坏脚本。
    let escaped = path.to_string_lossy().replace('\'', "''");
    let script = format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut('{escaped}');\
         if ($s.TargetPath) {{ Write-Output $s.TargetPath }}"
    );
    let out = crate::modules::process::run_cmd_timeout(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", &script],
        10,
    )?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then(|| PathBuf::from(text))
}

/// 兜底：扫描 `.lnk` 原始字节找 `.exe` 路径（UTF-8 与 UTF-16LE 两种编码都试）。
fn resolve_lnk_scan(path: &Path) -> Option<PathBuf> {
    let bytes = std::fs::read(path).ok()?;
    // UTF-16LE：按 2 字节一组取出可打印 ASCII，再找 .exe
    let utf16: String = bytes
        .chunks_exact(2)
        .map(|pair| {
            let code = u16::from_le_bytes([pair[0], pair[1]]);
            if (0x20..0x7f).contains(&code) {
                code as u8 as char
            } else {
                '\0'
            }
        })
        .collect();
    for text in [utf16.as_str(), &String::from_utf8_lossy(&bytes)] {
        if let Some(found) = find_exe_in_text(text) {
            let candidate = PathBuf::from(found);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// 从一段文本里提取第一个形如 `X:\...\name.exe` 的路径。
fn find_exe_in_text(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let end = lower.find(".exe")? + 4;
    // 从 .exe 结尾向前回溯到盘符（`X:`）或 `\0` 分隔
    let head = &text[..end];
    let start = head
        .char_indices()
        .filter(|(_, c)| *c == '\0')
        .map(|(i, c)| i + c.len_utf8())
        .next_back()
        .unwrap_or(0);
    let candidate = head[start..].trim();
    // 必须含盘符与目录分隔符，否则是误命中（如 "foo.exe"）
    (candidate.len() > 3
        && candidate.contains('\\')
        && candidate.chars().nth(1) == Some(':'))
    .then(|| candidate.to_string())
}

/// 从注册表 Uninstall 键找安装目录。
fn from_registry(prof: &AppProfile) -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        use windows_registry::LOCAL_MACHINE;
        const UNINSTALL_KEYS: [&str; 3] = [
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
            r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
        ];
        for key_path in UNINSTALL_KEYS {
            let Ok(key) = LOCAL_MACHINE.open(key_path) else {
                continue;
            };
            let Ok(names) = key.keys() else { continue };
            for sub in names {
                let Ok(entry) = key.open(&sub) else { continue };
                let Ok(display) = entry.get_string("DisplayName") else {
                    continue;
                };
                if !prof
                    .reg_patterns
                    .iter()
                    .any(|pattern| glob_match_ci(pattern, &display))
                {
                    continue;
                }
                // InstallLocation 是目录；DisplayIcon 常是 "path\app.exe,0"
                if let Ok(loc) = entry.get_string("InstallLocation") {
                    let loc = loc.trim().trim_matches('"');
                    if !loc.is_empty() {
                        for name in prof.exe_names {
                            let candidate = Path::new(loc).join(name);
                            if candidate.is_file() && exe_matches(&candidate, prof) {
                                return Some(candidate);
                            }
                        }
                    }
                }
                if let Ok(icon) = entry.get_string("DisplayIcon") {
                    let icon = icon.split(',').next().unwrap_or("").trim().trim_matches('"');
                    let candidate = PathBuf::from(icon);
                    if candidate.is_file() && exe_matches(&candidate, prof) {
                        return Some(candidate);
                    }
                }
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = prof;
    }
    None
}

/// 大小写不敏感的通配匹配（`*` 任意串、`?` 单字符）。
///
/// 不用 `regex`：模式来自本文件的常量表，手写匹配更易审计，也避免把用户可控的
/// 注册表 DisplayName 拼进正则造成回溯风险。
pub fn glob_match_ci(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    glob_rec(&p, &t)
}

fn glob_rec(p: &[char], t: &[char]) -> bool {
    if p.is_empty() {
        return t.is_empty();
    }
    match p[0] {
        '*' => {
            // 尝试用 `*` 吞掉 0..=t.len() 个字符
            (0..=t.len()).any(|skip| glob_rec(&p[1..], &t[skip..]))
        }
        '?' => !t.is_empty() && glob_rec(&p[1..], &t[1..]),
        c => !t.is_empty() && t[0] == c && glob_rec(&p[1..], &t[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 通配匹配大小写不敏感() {
        assert!(glob_match_ci("*Trae*", "TRAE SOLO CN.lnk"));
        assert!(glob_match_ci("*trae*", "Trae CN.lnk"));
        assert!(glob_match_ci("*豆包*", "豆包.lnk"));
        assert!(glob_match_ci("*Doubao*", "doubao.lnk"));
        assert!(glob_match_ci("Trae*", "Trae CN"));
        assert!(!glob_match_ci("*Trae*", "WorkBuddy.lnk"));
        assert!(!glob_match_ci("*Doubao*", "Trae.lnk"));
        // `?` 单字符
        assert!(glob_match_ci("Tra?", "Trae"));
        assert!(!glob_match_ci("Tra?", "Tra"));
        // 空模式只匹配空串
        assert!(glob_match_ci("", ""));
        assert!(!glob_match_ci("", "x"));
    }

    #[test]
    fn 从文本中提取_exe_路径() {
        assert_eq!(
            find_exe_in_text("C:\\Users\\Zhou\\AppData\\Local\\Programs\\Trae CN\\Trae CN.exe"),
            Some("C:\\Users\\Zhou\\AppData\\Local\\Programs\\Trae CN\\Trae CN.exe".to_string())
        );
        // 有 \0 分隔时应从分隔之后开始
        assert_eq!(
            find_exe_in_text("\0\0D:\\Programs\\Doubao\\Doubao.exe"),
            Some("D:\\Programs\\Doubao\\Doubao.exe".to_string())
        );
        // 无盘符的相对名不应被采纳（防误命中）
        assert_eq!(find_exe_in_text("foo.exe"), None);
        // 没有 .exe 时返回 None
        assert_eq!(find_exe_in_text("C:\\no-extension-here"), None);
    }

    #[test]
    fn 找不到_exe_时给出可操作错误() {
        let store = std::env::temp_dir().join(format!("ai-gateway-locate-{}", std::process::id()));
        let args = super::super::RunArgs {
            action: super::super::Action::BackupCurrent,
            target_app: crate::modules::app_profile::TargetApp::Trae,
            user_id: None,
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            store_dir: store.clone(),
        };
        let sess = Session::new(&args);
        // 把候选路径清空，确保走不到第 2 级
        let mut prof = crate::modules::app_profile::profile_for(
            crate::modules::app_profile::TargetApp::Trae,
            &store,
        );
        prof.exe_candidates.clear();
        match find_exe_cached(&sess, &prof) {
            Ok(path) => {
                // 本机真的装了 Trae：只断言结果通过白名单
                assert!(exe_matches(&path, &prof));
            }
            Err(e) => {
                assert!(e.contains("未找到"), "错误信息应说明未找到: {e}");
                assert!(e.contains("设置"), "错误信息应指引用户去设置里指定路径: {e}");
            }
        }
        let _ = std::fs::remove_dir_all(&store);
    }
}
