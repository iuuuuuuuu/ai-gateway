//! 参考网关：最小可运行实例，用于 A/B 对照与手工验证。
//!
//! 生产分发走 `ai-gateway-core` 的 `build.rs` 内嵌路径（见该 crate 文档）。
//! 本二进制单独存在是为了能在不启动 Tauri 桌面的情况下，
//! 与 Go 版 gateway.exe 做同配置的 HTTP 行为对比。
//!
//! # 用法
//!
//! ```text
//! wb2g-ref -config <config.json>     # 与 Go 网关一致的启动方式
//! wb2g-ref <port>                    # 旧式：只指定端口，其余读 WB2A_* 环境变量
//! ```
//!
//! **必须支持 `-config`**：`ai-gateway-core` 用 `Command::new(exe).arg("-config").arg(path)`
//! 拉起网关子进程（见 `gateway.rs::start`）。若只认位置参数，`-config` 会被当成端口号
//! 解析失败，配置**根本不会被读取** —— 网关「起来了」但用默认配置（无 API key、
//! 错的凭证目录、错的 state 文件），属于静默失效，比直接崩掉更难排查。
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ai_gateway_router::auth;
use ai_gateway_router::config::Config;
use ai_gateway_router::pool::Pool;
use ai_gateway_router::server::{router, AppState};
use ai_gateway_router::session::Router as SessionRouter;
use ai_gateway_router::upstream::client::{Client, ClientConfig};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    // `-config <path>`（Go 侧 flag 包同款）；也接受 `--config <path>`。
    let config_path = args
        .iter()
        .position(|a| a == "-config" || a == "--config")
        .and_then(|i| args.get(i + 1))
        .cloned();

    let cfg = match &config_path {
        Some(p) => match Config::load(p) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("加载配置失败 {p}: {e}");
                std::process::exit(1);
            }
        },
        None => {
            // 无 -config：用默认值 + WB2A_* 环境变量（保留旧式调用方式）。
            let mut c = Config::default();
            c.apply_env();
            if let Err(e) = c.normalize() {
                eprintln!("配置归一化失败: {e}");
                std::process::exit(1);
            }
            c
        }
    };

    // 旧式调用：`wb2g-ref <port>`（位置参数）覆盖监听端口。
    let port_override: Option<u16> = args
        .get(1)
        .filter(|a| !a.starts_with('-'))
        .and_then(|p| p.parse().ok());

    // 顺序与 Go 侧 main() 一致：先建池并载入 state（恢复 credits/冷却），
    // 再灌入完整凭证 —— 这样 add() 只换凭证、保留已恢复的运行态。
    let mut pool = Pool::new(cfg.state_file.clone());
    pool.load_state();
    let loaded_state = pool.loaded_from_state();
    pool.set_breaker(
        cfg.pool.breaker_threshold,
        cfg.parsed.breaker_cooldown,
        cfg.parsed.breaker_cooldown_max,
    );
    pool.set_max_in_flight(cfg.pool.max_in_flight);
    pool.set_weights(cfg.pool.idle_weight_per_hour, cfg.pool.idle_weight_max);

    let accounts = auth::load_dir(&cfg.auth_dir).unwrap_or_default();
    let n = accounts.len();
    for a in accounts {
        pool.add(a);
    }
    if n == 0 {
        eprintln!("warning: 未从 {} 加载到任何凭证", cfg.auth_dir);
    }

    let client = match Client::new(ClientConfig {
        timeout: Duration::from_secs(cfg.upstream.timeout_seconds.max(1) as u64),
        header_timeout: Duration::from_secs(cfg.upstream.header_timeout_seconds.max(1) as u64),
        idle_timeout: Duration::from_secs(cfg.upstream.idle_timeout_seconds.max(1) as u64),
        sanitize_fingerprints: cfg.features.sanitize_blacklist_fingerprints,
        proxy: std::env::var("WB2A_PROXY").unwrap_or_default(),
    }) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("构造上游客户端失败: {e}");
            std::process::exit(1);
        }
    };

    // 会话粘性：仅在配置开启时启用（与 Go 侧 `session_sticky.enabled` 一致）。
    // 池以 Arc 共享：粘性路由需要一个「读可用账号列表」的回调。
    let pool = Arc::new(Mutex::new(pool));
    let session = if cfg.session_sticky.enabled {
        let avail = Arc::clone(&pool);
        Some(SessionRouter::new(
            cfg.parsed.session_ttl,
            Arc::new(move || {
                avail
                    .lock()
                    .map(|p| p.available_uids())
                    .unwrap_or_default()
            }),
        ))
    } else {
        None
    };

    // 监听地址：位置参数优先，其次配置里的 listen（形如 ":7863" 或 "7863"）。
    let port = port_override.unwrap_or_else(|| {
        cfg.listen
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or(7863)
    });

    let state = Arc::new(AppState {
        pool,
        client,
        session,
        api_key: cfg.api_key.clone(),
        max_rotate: 3,
        soft_cooldown: cfg.parsed.soft_rate,
        refresh_skew: Duration::from_secs(600),
        redis_mode: "noop".into(),
        allowed_model: String::new(),
    });

    println!(
        "state restored={loaded_state} (state_file={})",
        cfg.state_file
    );
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind failed");
    println!("rust gateway listening on http://127.0.0.1:{port} (accounts={n})");
    axum::serve(listener, app).await.unwrap();
}
