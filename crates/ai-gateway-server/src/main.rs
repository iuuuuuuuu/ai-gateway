//! ai-gateway CLI：npm 安装形态的入口。
//!
//! ```bash
//! ai-gateway              # 启动本地服务 + 打开浏览器 webui
//! ai-gateway serve        # 只起服务不开浏览器（--port / --no-open）
//! ai-gateway status       # 终端输出当前账号
//! ai-gateway version      # 版本号
//! ```

mod api;

use ai_gateway_core::modules::{
    account, auth_file, checkin, config, process, refresh, rotate, travel, update,
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
    // 与桌面端 / 服务端共用同一处实现（见 `account::current_account_view`）。
    let current = account::current_account_view(auth.as_ref());
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

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("serve");
    match cmd {
        "status" => print_status(),
        "version" | "--version" | "-V" => {
            println!("ai-gateway {}", env!("CARGO_PKG_VERSION"));
        }
        "serve" | _ => serve(&args).await,
    }
}

async fn serve(args: &[String]) {
    let mut port = default_port();
    if let Some(i) = args.iter().position(|a| a == "--port") {
        if let Some(p) = args.get(i + 1).and_then(|p| p.parse::<u16>().ok()) {
            port = p;
        }
    }

    // ★ 监听地址可配（2026-10-17）。默认仍为回环 127.0.0.1，保持旧行为不变；
    // 部署到服务器时用 --host 0.0.0.0 才对外开放。
    let mut host = String::from("127.0.0.1");
    if let Some(i) = args.iter().position(|a| a == "--host") {
        if let Some(h) = args.get(i + 1).filter(|h| !h.starts_with('-')) {
            host = h.clone();
        }
    }

    // ★ 可选访问令牌。不设时不鉴权（与历史行为一致），
    // 但若同时监听非回环地址，启动时会强提醒。
    let mut access_token = String::new();
    if let Some(i) = args.iter().position(|a| a == "--access-token") {
        if let Some(tk) = args.get(i + 1).filter(|s| !s.starts_with('-')) {
            access_token = tk.clone();
        }
    }
    // 也支持环境变量（systemd / Docker 不把令牌写进命令行，避免被 ps 看到）。
    if access_token.is_empty() {
        if let Ok(v) = std::env::var("AI_GATEWAY_ACCESS_TOKEN") {
            if !v.trim().is_empty() {
                access_token = v.trim().to_string();
            }
        }
    }

    let is_loopback = host == "127.0.0.1"
        || host == "localhost"
        || host == "::1"
        || host.starts_with("127.");
    if !is_loopback && access_token.is_empty() {
        eprintln!("⚠⚠ 安全提醒：正在监听 {host}:{port}（非回环），但未设置访问令牌。");
        eprintln!("   任何能访问该端口的人都可以读取你的全部账号凭证、修改网关配置。");
        eprintln!("   建议加 --access-token <令牌> 或设环境变量 AI_GATEWAY_ACCESS_TOKEN。");
        eprintln!("   若确实只在受信任内网（VPN / 安全组）中使用，可忽略本提醒。");
    } else if !is_loopback {
        println!("已启用访问令牌鉴权（非回环监听 {host}:{port}）。");
    }

    let app = api::router(access_token.clone());
    let addr = format!("{host}:{port}");
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("启动失败: 无法监听 {addr}（{e}）。可用 --port / --host 指定其他地址。");
            std::process::exit(1);
        }
    };

    println!("ai-gateway v{}", update::APP_VERSION);
    println!("webui: http://{addr}");
    println!("按 Ctrl+C 停止服务。");

    // 绑定非回环时不自动开浏览器：本机打开 0.0.0.0 这个地址是访问不到的。
    let no_open = args.iter().any(|a| a == "--no-open") || !is_loopback;
    if !no_open {
        open_browser(&addr);
    }

    spawn_background_loops();
    // 预热两个「首次要 spawn 子进程」的状态检测（`/api/status` 的 tasklist、
    // `/api/codebuddy-cn-ide/status` 的 powershell），别让用户请求付这一笔。
    api::warm_caches();

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
