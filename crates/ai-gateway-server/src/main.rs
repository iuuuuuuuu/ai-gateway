//! ai-gateway CLI：npm 安装形态的入口。
//!
//! ```bash
//! ai-gateway              # 启动本地服务 + 打开浏览器 webui
//! ai-gateway serve        # 只起服务不开浏览器（--port / --no-open）
//! ai-gateway status       # 终端输出当前账号
//! ai-gateway version      # 版本号
//! ai-gateway --task-run <key>   # 计划任务模式：跑一个任务后退出
//! ```
//!
//! ## 为什么要有 `--task-run`
//!
//! 设置页的「计划任务」注册的启动器指向本 exe。schtasks 触发时以
//! `--task-run <key>` 调用，必须**跑完即退出**，不能顺手起一个服务 ——
//! 否则每次定时触发都会占住端口、并在浏览器里弹出新标签页。
//! 因此在 `serve()` 之前分流。

mod api;

use serde_json::json;

use ai_gateway_core::modules::{
    account, auth_file, checkin, cli_task, config, process, refresh, rotate, travel, update,
};

fn default_port() -> u16 {
    57890
}

/// 后台任务：自动签到启动即核验、每 30 分钟补签；自动轮换按配置间隔执行。
fn spawn_background_loops() {
    // 启动时清理历史误报的「需重新登录」标记：旧版本把传输层失败（网络/代理
    // 不可达）也写成 needs_relogin，会让这些账号被排除出网关账号池。
    // 真正失效的凭证不受影响。
    let repaired = refresh::repair_false_relogin_flags();
    if repaired > 0 {
        eprintln!("[refresh] 已清除 {repaired} 个账号因网络失败误报的「需重新登录」标记");
    }

    // 启动时清理无凭据的残留记录（实测：一次测试隔离失效把种子数据写进了
    // 用户真实账号库，又被数据目录迁移原样搬运，界面上出现永远登录不了的僵尸
    // 账号）。判据窄且带日志，见 account::purge_credentialless_leftovers。
    let purged = account::purge_credentialless_leftovers();
    if purged > 0 {
        eprintln!("[account] 已清理 {purged} 条无凭据的残留账号记录");
    }

    tokio::spawn(async move {
        if let Err(error) = config::compact_checkin_logs() {
            eprintln!("[签到] 历史日志整理失败: {error}");
        }
        let _ = checkin::run_checkin_cycle(checkin::CheckinCycleMode::StartupVerify).await;
        loop {
            tokio::time::sleep(checkin::CHECKIN_RECOVERY_INTERVAL).await;
            let _ = checkin::run_checkin_cycle(checkin::CheckinCycleMode::PeriodicRecovery).await;
        }
    });

    tokio::spawn(async move {
        let mut last_cycle_at: i64 = 0;
        loop {
            let cfg = config::load_auto_rotate_config();
            if cfg.get("enabled").and_then(|v| v.as_bool()) == Some(true) {
                let interval_minutes = cfg
                    .get("check_interval_minutes")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(5)
                    .max(1);
                let now = config::now_ms();
                if now - last_cycle_at >= interval_minutes * 60_000 {
                    last_cycle_at = now;
                    let _ = rotate::run_rotate_cycle().await;
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });

    // 派猫猫旅行：启动即派发，之后周期性补派（并重试 no-buddy / 瞬时错误）。
    tokio::spawn(async move {
        let _ = travel::run_travel_cycle().await;
        loop {
            tokio::time::sleep(travel::TRAVEL_RETRY_INTERVAL).await;
            let _ = travel::run_travel_cycle().await;
        }
    });

    // 账号库 → 网关 自动同步（同 GUI 版行为）。
    tokio::spawn(async move {
        ai_gateway_core::modules::gateway::run_auto_sync_loop(30).await;
    });

    // 旅行领取：启动立刻查一轮（避免重启后空等 15 分钟漏领），之后按周期检查。
    tokio::spawn(async move {
        let _ = travel::run_travel_claim_cycle().await;
        loop {
            tokio::time::sleep(travel::TRAVEL_CLAIM_INTERVAL).await;
            let _ = travel::run_travel_claim_cycle().await;
        }
    });
}

fn print_status() {
    let auth = auth_file::read_auth_file();
    let current = auth.as_ref().and_then(|a| {
        let acct = a.get("account").cloned().unwrap_or_else(|| json!({}));
        Some(json!({
            "uid": acct.get("uid"),
            "nickname": acct.get("nickname"),
            "email": acct.get("email"),
        }))
    });
    let running = process::is_workbuddy_running();
    println!("ai-gateway v{}", update::APP_VERSION);
    println!("WorkBuddy 运行中: {}", if running { "是" } else { "否" });
    match current {
        Some(c) => {
            let name = c
                .get("nickname")
                .and_then(|v| v.as_str())
                .or_else(|| c.get("email").and_then(|v| v.as_str()))
                .unwrap_or("未知");
            println!("当前账号: {name}");
        }
        None => println!("当前账号: 未登录"),
    }
    println!("账号数: {}", account::load_accounts().len());
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // 计划任务模式必须在 `#[tokio::main]` 之前分流：`cli_task::run_cli_task`
    // 会自建一个 tokio runtime 并在里面 `block_on`，而本函数已被
    // `#[tokio::main]` 的 runtime 包着 —— 在 runtime 里再起 runtime 会直接
    // panic（"Cannot start a runtime from within a runtime"）。
    // 复用桌面端的写法：main 保持同步，只在 serve 分支里手建 runtime。
    if let Some(task) = parse_task_mode(&args) {
        std::process::exit(cli_task::run_cli_task(&task));
    }

    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("serve");
    match cmd {
        "status" => print_status(),
        "version" | "--version" | "-V" => {
            println!("ai-gateway {}", env!("CARGO_PKG_VERSION"));
        }
        // serve 分支才需要 async：`serve()` 自己 block_on 一个多线程 runtime
        "serve" | _ => {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("运行时初始化失败: {e}");
                    std::process::exit(1);
                }
            };
            runtime.block_on(serve(&args));
        }
    }
}

/// 解析 `--task-run <name>`（必须是第一个参数）。
///
/// 与桌面端 `src-tauri/src/main.rs` 的同名函数保持一致：都要求任务名非空、
/// 且位置固定，避免把别的子命令误判成任务模式。
fn parse_task_mode(args: &[String]) -> Option<String> {
    if args.len() >= 3 && args[1] == "--task-run" {
        return Some(args[2].clone()).filter(|s| !s.is_empty());
    }
    None
}

async fn serve(args: &[String]) {
    let mut port = default_port();
    if let Some(i) = args.iter().position(|a| a == "--port") {
        if let Some(p) = args.get(i + 1).and_then(|p| p.parse::<u16>().ok()) {
            port = p;
        }
    }

    let app = api::router();
    let addr = format!("127.0.0.1:{port}");
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("启动失败: 端口 {port} 被占用或不可用（{e}）。可用 --port 指定其他端口。");
            std::process::exit(1);
        }
    };

    println!("ai-gateway v{}", update::APP_VERSION);
    println!("webui: http://{addr}");
    println!("按 Ctrl+C 停止服务。");

    let no_open = args.iter().any(|a| a == "--no-open");
    if !no_open {
        open_browser(&addr);
    }

    spawn_background_loops();

    axum::serve(listener, app).await.unwrap();
}

fn open_browser(addr: &str) {
    let url = format!("http://{addr}");
    #[cfg(target_os = "windows")]
    {
        let mut c = std::process::Command::new("cmd");
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW：开浏览器不闪 cmd 窗
        }
        let _ = c.args(["/C", "start", &url]).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(&url).spawn();
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
    }
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
            parse_task_mode(&args(&["ai-gateway", "--task-run", "trae-checkin"])),
            Some("trae-checkin".to_string())
        );
        // 缺任务名
        assert_eq!(parse_task_mode(&args(&["ai-gateway", "--task-run"])), None);
        // 空任务名
        assert_eq!(
            parse_task_mode(&args(&["ai-gateway", "--task-run", ""])),
            None
        );
        // 不是第一个参数（避免把别的子命令误判成任务模式）
        assert_eq!(
            parse_task_mode(&args(&["ai-gateway", "serve", "--task-run", "x"])),
            None
        );
        // 无参数 = 正常启动服务，不是任务模式
        assert_eq!(parse_task_mode(&args(&["ai-gateway"])), None);
    }
}
