//! CLI 任务模式：`<exe> --task-run <key>` 执行后退出。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/tasks/mod.rs` 的 `run_cli_task`。
//!
//! ## 为什么放在 core 而不是 src-tauri
//!
//! 任务模式**刻意不启动 Tauri**（见 `main.rs` 的分流注释）：单实例插件会把
//! 计划任务触发当成「用户又开了一个实例」而立刻退出，导致定时任务静默不执行。
//! 既然不碰 Tauri，逻辑就属于 core —— server 形态与 CLI 也能复用同一套任务。
//!
//! ## 输出契约
//!
//! stdout 最后一行是 JSON（成功为任务结果，失败为 `{"ok":false,"error":...}`）。
//! 便于计划任务日志排查，也便于将来做双轨对照。

use serde_json::json;

/// 已知任务键。
pub const TASK_KEYS: [&str; 3] = ["trae-checkin", "doubao-keepalive", "doubao-quota"];

/// 执行一个 CLI 任务，返回进程退出码。
pub fn run_cli_task(name: &str) -> i32 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            println!("{}", json!({"ok": false, "error": format!("运行时初始化失败: {e}")}));
            return 1;
        }
    };

    let result = runtime.block_on(async { dispatch(name).await });

    match result {
        Ok(value) => {
            println!("{}", serde_json::to_string(&value).unwrap_or_default());
            0
        }
        Err(e) => {
            println!("{}", json!({"ok": false, "error": e}));
            1
        }
    }
}

/// 按任务键分派。
async fn dispatch(name: &str) -> Result<serde_json::Value, String> {
    match name {
        // Trae 每日签到：跑一轮全部账号
        "trae-checkin" => {
            let accounts = crate::modules::trae_account::load_accounts();
            if accounts.is_empty() {
                return Ok(json!({"ok": true, "skipped": true, "reason": "没有 Trae 账号"}));
            }
            let summary = crate::modules::trae_checkin::run_round(&accounts, 1).await;
            Ok(summary.to_json())
        }
        // 豆包会话保活：启动客户端 → 等待 → 关闭，触发服务端滑动续期
        "doubao-keepalive" => {
            use crate::modules::app_profile::TargetApp;
            use crate::modules::switcher::{self, Action, RunArgs};
            let args = RunArgs {
                action: Action::KeepAlive,
                target_app: TargetApp::Doubao,
                user_id: None,
                proxy_port: None,
                include_indexeddb: false,
                expected_current_uid: String::new(),
                store_dir: crate::modules::config::store_dir(),
            };
            let outcome = switcher::run_action(args, &switcher::LogSink)?;
            let _ = crate::modules::doubao_account::set_last_keepalive(
                &crate::modules::config::utc_iso(),
            );
            Ok(json!({"ok": true, "message": outcome}))
        }
        // 豆包额度巡检：批量查询并回写额度缓存
        "doubao-quota" => Ok(crate::modules::doubao_quota::run_batch().await),
        other => Err(format!(
            "未知任务: {other}（可用：{}）",
            TASK_KEYS.join(" / ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 未知任务返回非零并列出可用任务() {
        let code = run_cli_task("no-such-task");
        assert_eq!(code, 1, "未知任务必须返回非零退出码");
    }

    #[test]
    fn 任务键与计划任务模块一一对应() {
        use crate::modules::scheduler::TaskKind;
        for kind in TaskKind::ALL {
            assert!(
                TASK_KEYS.contains(&kind.cli_key()),
                "计划任务 {} 的 CLI 键 {} 未在 TASK_KEYS 中登记",
                kind.task_name(),
                kind.cli_key()
            );
        }
    }
}
