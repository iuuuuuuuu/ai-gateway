//! 参考网关：最小可运行实例，仅用于 A/B 对照与手工验证。
//!
//! 生产分发走 `ai-gateway-core` 的 `build.rs` 内嵌路径（见该 crate 文档）。
//! 本二进制单独存在是为了能在不启动 Tauri 桌面的情况下，
//! 与 Go 版 gateway.exe 做同配置的 HTTP 行为对比。
//!
//! 用法：`wb2g-ref [port]`，并用 `WB2A_AUTH_DIR` / `WB2A_API_KEY` 指定配置，
//! 以便与 Go 网关（`-config`）指向同一份凭证目录。
//!
//! 与 `gateway.rs`（生产入口）的区别：本文件**只读环境变量**、不读 `-config`，
//! 且刻意保持最小 —— 它的价值在于「同一份凭证目录、同一个端口约定」下与
//! Go 网关做逐请求对照，所以不能依赖宿主配置文件的任何归一化逻辑。
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ai_gateway_router::auth;
use ai_gateway_router::pool::Pool;
use ai_gateway_router::server::{router, AppState};
use ai_gateway_router::upstream::client::{Client, ClientConfig};

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(7864);

    // 与 Go 参考网关同配置：同一份 auths、同一份 state.json、同一个 api_key。
    let auth_dir = std::env::var("WB2A_AUTH_DIR").unwrap_or_else(|_| "./auths".into());
    let state_file = std::env::var("WB2A_STATE_FILE").unwrap_or_else(|_| "./data/state.json".into());
    let api_key = std::env::var("WB2A_API_KEY").unwrap_or_default();

    // 顺序与 Go 侧 main() 一致：先建池并载入 state（恢复 credits/冷却），
    // 再 SyncToDir 灌入完整凭证 —— 这样 add() 只换凭证、保留已恢复的运行态。
    // `clone()`：state_file 稍后还要用来推导 usage.json 的路径。
    let mut pool = Pool::new(state_file.clone());
    pool.load_state();
    let loaded_state = pool.loaded_from_state();

    let accounts = auth::load_dir(&auth_dir).unwrap_or_default();
    let n = accounts.len();
    for a in accounts {
        pool.add(a);
    }
    if n == 0 {
        eprintln!("warning: 未从 {auth_dir} 加载到任何凭证");
    }
    println!(
        "state restored={loaded_state} (state_file={})",
        std::env::var("WB2A_STATE_FILE").unwrap_or_else(|_| "./data/state.json".into())
    );

    // 出站客户端：A/B 对照时用 WB2A_PROXY 走代理（国际版直连不稳），
    // 基址覆盖留给「把网关指向假上游」的对照测试。
    let client = match Client::new(ClientConfig {
        timeout: Duration::from_secs(120),
        header_timeout: Duration::from_secs(30),
        idle_timeout: Duration::from_secs(60),
        sanitize_fingerprints: true,
        proxy: std::env::var("WB2A_PROXY").unwrap_or_default(),
        chat_base_cn: std::env::var("WB2A_CHAT_BASE_CN").unwrap_or_default(),
        billing_base_cn: std::env::var("WB2A_BILLING_BASE_CN").unwrap_or_default(),
        base_intl: std::env::var("WB2A_BASE_INTL").unwrap_or_default(),
        // A/B 对照工具不需要设备 token（它按单账号跑一次对照，
        // 带上反而会让「同一请求两次不同结果」多一个变量）。
        ..Default::default()
    }) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("构造上游客户端失败: {e}");
            std::process::exit(1);
        }
    };

    // Token 用量统计：与 state.json 同目录的 usage.json（与生产入口同一口径）。
    let usage_path = std::path::Path::new(&state_file)
        .parent()
        .map(|d| d.join("usage.json").to_string_lossy().to_string())
        .unwrap_or_default();

    let state = Arc::new(AppState {
        // 池以 Arc 共享（新 AppState 口径）；本入口不启用会话粘性，故无第二持有者。
        pool: Arc::new(Mutex::new(pool)),
        client,
        // 会话粘性需要一份「读可用账号列表」的回调，属于生产入口的配置驱动行为；
        // 本参考入口只读环境变量、不解析配置，因此固定不启用。若需要带粘性的
        // 对照，请用 `gateway -config <path>`（生产入口）。
        session: None,
        api_key,
        max_rotate: 3,
        soft_cooldown: Duration::from_secs(60),
        refresh_skew: Duration::from_secs(600),
        redis_mode: "noop".into(),
        // 参考入口不做「限制使用的模型」白名单（与不限制的宿主口径一致）。
        allowed_model: Vec::new(),
        usage: Arc::new(ai_gateway_router::usage::Stats::new(usage_path)),
        // 参考入口只跑 WorkBuddy 对照（它读环境变量、不解析产品凭证目录），
        // 故产品上游为空集 —— 产品账号会走 WorkBuddy 默认路径，与引入多产品
        // 之前的行为一致。
        products: ai_gateway_router::products::ProductUpstreams::new(),
        unusual: ai_gateway_router::unusual::UnusualBreakers::new(),
        waf_ip: ai_gateway_router::wafip::WafIpWindow::new(),
        probe_timeout: Duration::from_secs(30),
        models: Arc::new(ai_gateway_router::models_cache::ModelsCache::new()),
        // 参考入口不解析配置，故没有产品模型清单、也没有提示词改写：A/B 对照要的是
        // 「同一请求的原始出站字节」，注入提示词会污染对照结果 ⇒ 固定 passthrough。
        //
        // ★ 用一个**不接监听**的 `DynamicState`（没有配置路径可听）：参考入口本来就
        //   不读配置文件，起了监听反而会去读一个不存在的路径。
        dynamic: ai_gateway_router::cfgwatch::DynamicState::new(
            ai_gateway_router::cfgwatch::DynamicConfig::default(),
        ),
        // 短信登录在参考入口里没有意义（它不解析凭证目录），给空串即可 ——
        // 真调用会在创建目录时报错，而不是悄悄写进某个意外位置。
        auth_dir: String::new(),
        // 参考入口不跑养号任务：它的用途是「同一请求的原始出站字节」A/B 对照，
        // 后台任务会在对照期间自己打上游，把对照结果搅浑。
        // ⇒ `/tasks/run` 在本入口返回 503 `tasks_unavailable`（Go 未注入分支同义）。
        scheduler: None,
        // 同理不接成长任务执行体（它会打上游、还会写账号记录）。
        // ⇒ `/tasks/growth` 在本入口返回 503 `growth_tasks_unavailable`。
        growtask: None,
    });

    let app = router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind failed");
    println!("rust gateway listening on http://127.0.0.1:{port} (accounts={n})");
    axum::serve(listener, app).await.unwrap();
}
