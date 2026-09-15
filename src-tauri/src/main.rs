// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! 入口：区分「GUI 模式」与「CLI 任务模式」。
//!
//! ## 为什么任务模式必须绕开 Tauri
//!
//! 计划任务触发时以 `--task-run <name>` 调用同一个 exe。若走 GUI 路径：
//! - 单实例插件会把这次触发当成「用户又开了一个实例」而立刻退出，
//!   任务静默不执行（表现为「定时签到从来不生效」）
//! - 还会弹出一个窗口打断用户
//!
//! 因此在 `Builder` 之前分流：任务模式跑完即退出，从不触碰 Tauri。

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if let Some(task) = parse_task_mode(&args) {
        std::process::exit(ai_gateway_core::modules::cli_task::run_cli_task(&task));
    }

    ai_gateway_lib::run()
}

/// 解析 `--task-run <name>`（必须是第一个参数）。
fn parse_task_mode(args: &[String]) -> Option<String> {
    if args.len() >= 3 && args[1] == "--task-run" {
        return Some(args[2].clone()).filter(|s| !s.is_empty());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn 解析任务模式参数() {
        assert_eq!(
            parse_task_mode(&args(&["app.exe", "--task-run", "trae-checkin"])),
            Some("trae-checkin".to_string())
        );
        // 缺任务名
        assert_eq!(parse_task_mode(&args(&["app.exe", "--task-run"])), None);
        // 空任务名
        assert_eq!(
            parse_task_mode(&args(&["app.exe", "--task-run", ""])),
            None
        );
        // 不是第一个参数（避免把别的子命令误判成任务）
        assert_eq!(
            parse_task_mode(&args(&["app.exe", "--debug", "--task-run", "x"])),
            None
        );
        // 无参数
        assert_eq!(parse_task_mode(&args(&["app.exe"])), None);
    }
}
