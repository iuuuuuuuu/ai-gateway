//! 进程控制：枚举 / 优雅关闭 / 启动，按 [`AppProfile`] 参数化。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/switcher/proc.rs`，改写为复用本仓库
//! 既有的 `process` 模块（tasklist / taskkill 封装），避免重复实现。
//!
//! 关闭是**三层**的：先 `WM_CLOSE`（让客户端有机会落盘 leveldb / cookie / SQLite
//! WAL）→ 等 `graceful_wait_secs` → 仍在则 `taskkill /T`（温和）→ 再等 5 秒 →
//! 仍在则 `taskkill /T /F`（强制）。
//!
//! 为什么必须优雅优先：强杀会留下文件锁与未 checkpoint 的 WAL。前者让快照静默缺文件，
//! 后者被客户端下次启动回放，把切换前账号的登录/使用证据写回新库 —— 表现为
//! 「切换后账号没变」。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{ProgressSink, Session, StepStatus};
use crate::modules::app_profile::AppProfile;
use crate::modules::process;

/// 进程表的一行（`pid` + 映像名）。
#[derive(Clone, Debug)]
pub struct ProcRow {
    pub pid: u32,
    pub name: String,
}

/// 去掉 `.exe` 后缀（大小写不敏感）。
///
/// 关键：`tasklist` / `sysinfo` 返回的映像名**带** `.exe`，而档案里的
/// `proc_names` 白名单**不带**。不归一化就永远匹配不上，客户端永远不会被关闭 ——
/// 这会让「切换」静默变成「什么都没发生」。
pub fn strip_exe_suffix(name: &str) -> &str {
    let name = name.trim();
    if name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(".exe") {
        &name[..name.len() - 4]
    } else {
        name
    }
}

/// 枚举所有匹配该档案的进程（按 `proc_names` 精确匹配，大小写不敏感）。
///
/// 非 Windows 平台返回空：进程枚举走的是 Windows 的 `tasklist`，
/// 而本切换器只服务 Windows 桌面端（Trae / 豆包客户端的登录态布局也是
/// Windows 路径）。返回空而不是编译失败，是为了让 workspace 能在
/// Linux / macOS 上完成 `cargo check` 与单测（CI 三平台都会编译本 crate）。
#[cfg(target_os = "windows")]
pub fn list_procs(prof: &AppProfile) -> Vec<ProcRow> {
    let mut out = Vec::new();
    for image in prof.proc_names {
        for row in process::windows_tasklist_image_rows(image) {
            out.push(ProcRow {
                pid: row.pid,
                name: row.name.clone(),
            });
        }
    }
    // 同一 pid 可能因多个映像名被枚举两次（如 CodeBuddy 与 CodeBuddy CN 同 pid 不会
    // 发生，但通配与精确混用时可能），去重更稳。
    out.sort_by_key(|r| r.pid);
    out.dedup_by_key(|r| r.pid);
    out
}

#[cfg(not(target_os = "windows"))]
pub fn list_procs(prof: &AppProfile) -> Vec<ProcRow> {
    let _ = prof;
    Vec::new()
}

/// 应用是否正在运行。
pub fn is_running(sess: &Session) -> bool {
    !list_procs(&sess.prof).is_empty()
}

/// 取正在运行的客户端 exe 路径（exe 发现的第 5 级回退）。
pub fn running_exe_of(prof: &AppProfile) -> Option<PathBuf> {
    let rows = list_procs(prof);
    let first = rows.first()?;
    process::windows_process_exe_path(first.pid)
}

/// 优雅关闭客户端。
pub fn stop_app(sess: &Session, sink: &dyn ProgressSink) -> Result<(), String> {
    let prof = &sess.prof;
    let rows = list_procs(prof);
    if rows.is_empty() {
        sink.step(
            "stop",
            StepStatus::Skip,
            &format!("{} 未在运行，无需关闭", prof.app_name),
        );
        return Ok(());
    }
    let pids: Vec<u32> = rows.iter().map(|r| r.pid).collect();
    sink.step(
        "stop",
        StepStatus::Info,
        &format!(
            "正在关闭 {}（{} 个进程），等待落盘…",
            prof.app_name,
            pids.len()
        ),
    );

    // 第一层：温和关闭（WM_CLOSE / 无 /F 的 taskkill），给客户端落盘机会。
    for pid in &pids {
        process::request_graceful_close(*pid);
    }
    let deadline = Instant::now() + Duration::from_secs(prof.graceful_wait_secs);
    while Instant::now() < deadline {
        if list_procs(prof).is_empty() {
            sink.step(
                "stop",
                StepStatus::Ok,
                &format!("{} 已优雅退出", prof.app_name),
            );
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    // 第二层：温和 taskkill（不带 /F），再等 5 秒。
    let remaining: Vec<u32> = list_procs(prof).iter().map(|r| r.pid).collect();
    sink.step(
        "stop",
        StepStatus::Warn,
        &format!(
            "{} 在 {} 秒内未退出，改为温和终止 {} 个进程",
            prof.app_name,
            prof.graceful_wait_secs,
            remaining.len()
        ),
    );
    for pid in &remaining {
        process::taskkill_pid(*pid, false);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if list_procs(prof).is_empty() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    // 第三层：强制终止（此时 WAL 风险已无法避免，只能保证不残留进程）。
    let stubborn: Vec<u32> = list_procs(prof).iter().map(|r| r.pid).collect();
    if stubborn.is_empty() {
        return Ok(());
    }
    sink.step(
        "stop",
        StepStatus::Warn,
        &format!("强制终止 {} 个残留进程", stubborn.len()),
    );
    for pid in &stubborn {
        process::taskkill_pid(*pid, true);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if list_procs(prof).is_empty() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(format!("无法关闭 {}，请手动退出后重试", prof.app_name))
}

/// 启动客户端；`sess.launch_proxy_port > 0` 时注入 `--proxy-server`。
///
/// 注意：单实例客户端（Electron 系）**忽略**新实例的命令行参数 —— 必须确保
/// 旧实例已退出，否则 `--proxy-server` 不会生效（表现为「一键以账号打开」没走代理）。
pub fn start_app(sess: &Session, sink: &dyn ProgressSink) -> Result<(), String> {
    let prof = &sess.prof;
    if is_running(sess) {
        sink.step(
            "start",
            StepStatus::Warn,
            &format!("{} 已在运行，跳过启动（单实例客户端会忽略新的命令行参数）", prof.app_name),
        );
        return Ok(());
    }
    let exe = super::locate::find_exe(sess)?;
    let mut cmd = process::cmd_builder(&exe);
    if let Some(port) = sess.launch_proxy_port {
        cmd.arg(format!("--proxy-server=http://127.0.0.1:{port}"));
        sink.step(
            "start",
            StepStatus::Info,
            &format!("正在启动 {}（经本地代理 127.0.0.1:{port}）…", prof.app_name),
        );
    } else {
        sink.step(
            "start",
            StepStatus::Info,
            &format!("正在启动 {}…", prof.app_name),
        );
    }
    cmd.spawn()
        .map_err(|e| format!("启动 {} 失败（{}）: {e}", prof.app_name, exe.display()))?;
    sink.step("start", StepStatus::Ok, &format!("{} 已启动", prof.app_name));
    Ok(())
}

/// 路径是否指向该档案的 exe（大小写不敏感，用于防串台）。
pub fn path_is_app_exe(path: &Path, prof: &AppProfile) -> bool {
    crate::modules::app_profile::exe_matches(path, prof)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::app_profile::{profile_for, TargetApp};

    fn prof() -> AppProfile {
        profile_for(TargetApp::TraeWork, Path::new("C:\\store"))
    }

    #[test]
    fn 剥离_exe_后缀大小写不敏感() {
        assert_eq!(strip_exe_suffix("Trae.exe"), "Trae");
        assert_eq!(strip_exe_suffix("Trae.EXE"), "Trae");
        assert_eq!(strip_exe_suffix("TRAE SOLO CN.exe"), "TRAE SOLO CN");
        // 不带后缀的原样返回
        assert_eq!(strip_exe_suffix("Trae"), "Trae");
        // 只有 ".exe" 这 4 个字符本身时不剥离（避免切出空串）
        assert_eq!(strip_exe_suffix(".exe"), ".exe");
        assert_eq!(strip_exe_suffix(""), "");
    }

    #[test]
    fn 进程名白名单不带_exe_因此必须归一化() {
        // 这是「切换静默失效」的根因回归测试：档案里存的是不带后缀的名字，
        // 而 tasklist 返回带后缀的名字。
        let p = prof();
        assert!(p.proc_names.iter().all(|n| !n.ends_with(".exe")));
        assert_eq!(strip_exe_suffix("TRAE SOLO CN.exe"), p.proc_names[0]);
    }

    #[test]
    fn 路径白名单防串台() {
        let p = prof();
        assert!(path_is_app_exe(Path::new("C:\\x\\Trae.exe"), &p));
        assert!(!path_is_app_exe(Path::new("C:\\x\\Trae CN.exe"), &p));
        assert!(!path_is_app_exe(Path::new("C:\\x\\Doubao.exe"), &p));
    }
}
