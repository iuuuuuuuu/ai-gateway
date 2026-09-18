//! 网关可执行文件（Rust 版）：OpenAI 兼容网关的服务端入口。
//!
//! 由宿主 `ai-gateway-core` 以子进程方式托管，构建期被 `build.rs` gzip 压缩后
//! 内嵌进主程序，实现「只需分发一个 exe」。
//!
//! # 用法
//!
//! ```text
//! gateway -config <config.json>     # 宿主拉起网关的方式
//! gateway <port>                    # 手工调试：只指定端口，其余读 WB2A_* 环境变量
//! ```
//!
//! **必须支持 `-config`**：`ai-gateway-core` 用 `Command::new(exe).arg("-config").arg(path)`
//! 拉起网关子进程（见 `gateway.rs::start`）。若只认位置参数，`-config` 会被当成端口号
//! 解析失败，配置**根本不会被读取** —— 网关「起来了」但用默认配置（无 API key、
//! 错的凭证目录、错的 state 文件），属于静默失效，比直接崩掉更难排查。
//!
//! # 与宿主的契约
//!
//! | 端点          | 用途                                   |
//! |---------------|----------------------------------------|
//! | `/healthz`    | 探活 + 身份标识（`service` 字段与 `X-Service` 头） |
//! | `/status`     | 账号池状态（网关页展示）               |
//! | `/v1/models`  | 模型列表（含图片能力字段）             |
//! | `/usage`      | Token 用量聚合（Token 统计页读它）     |
//! | `/v1/chat/completions` `/v1/messages` `/v1/responses` | 三种客户端协议入口 |
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

    // 手工调试：`gateway <port>`（位置参数）覆盖监听端口。
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

    // 出站代理：配置优先（宿主从「设置 → 更新代理」透传），其次环境变量兜底。
    // 国际版（workbuddy.ai）在国内直连不稳定，走代理才稳。
    let proxy = if cfg.proxy.trim().is_empty() {
        std::env::var("WB2A_PROXY").unwrap_or_default()
    } else {
        cfg.proxy.clone()
    };

    let client = match Client::new(ClientConfig {
        timeout: Duration::from_secs(cfg.upstream.timeout_seconds.max(1) as u64),
        header_timeout: Duration::from_secs(cfg.upstream.header_timeout_seconds.max(1) as u64),
        idle_timeout: Duration::from_secs(cfg.upstream.idle_timeout_seconds.max(1) as u64),
        sanitize_fingerprints: cfg.features.sanitize_blacklist_fingerprints,
        proxy,
        // 基址覆盖：仅供 A/B 对照测试把网关指向假上游（生产留空走内置常量）。
        chat_base_cn: std::env::var("WB2A_CHAT_BASE_CN").unwrap_or_default(),
        billing_base_cn: std::env::var("WB2A_BILLING_BASE_CN").unwrap_or_default(),
        base_intl: std::env::var("WB2A_BASE_INTL").unwrap_or_default(),
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

    // Token 用量统计：与 state.json 同目录的 usage.json
    //（独立文件，避免与池状态互相迁移）。
    let usage_path = {
        let p = std::path::Path::new(&cfg.state_file);
        p.parent()
            .map(|d| d.join("usage.json").to_string_lossy().to_string())
            .unwrap_or_default()
    };
    let usage = Arc::new(ai_gateway_router::usage::Stats::new(usage_path));

    let state = Arc::new(AppState {
        pool,
        client,
        session,
        api_key: cfg.api_key.clone(),
        max_rotate: 3,
        soft_cooldown: cfg.parsed.soft_rate,
        refresh_skew: Duration::from_secs(600),
        redis_mode: "noop".into(),
        // 「单一模型」锁定只在轮转模式下生效（与宿主 write_native_config 的口径一致：
        // 负载均衡不限制模型，保持原有行为）。
        allowed_model: if cfg.pool.rotation {
            cfg.pool.allowed_model.trim().to_string()
        } else {
            String::new()
        },
        usage,
    });

    println!(
        "state restored={loaded_state} (state_file={})",
        cfg.state_file
    );
    let app = router(state.clone());

    // 后台落盘：池状态与用量统计每 5 秒 flush 一次（与 Go 侧 flusher 同周期）。
    // 进程退出前还会各补一次（见下面的 shutdown 分支）。
    {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                state.usage.flush();
                if let Ok(mut pool) = state.pool.lock() {
                    pool.flush();
                }
            }
        });
    }

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind failed");
    println!("rust gateway listening on http://127.0.0.1:{port} (accounts={n})");

    // 优雅退出：收到 Ctrl+C / 终止信号时先落盘再退出。
    //
    // 为什么必须显式处理：用量与池状态都是「内存累积 + 周期落盘」，
    // 若直接退出会丢掉最后一个周期内的数据（宿主重启网关很频繁 ——
    // 切换工作模式、账号同步都会触发重启）。
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .unwrap();
    state.usage.flush();
    if let Ok(mut pool) = state.pool.lock() {
        pool.flush();
    }
    println!("已落盘，退出");
}
