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
//! gateway qoder-login <sub> [flags] # 子命令：Qoder 登录/额度/模型/活动
//! gateway zcode-login <sub> [flags] # 子命令：ZCode 登录/额度/模型/套餐/激活
//! ```
//!
//! **必须支持 `-config`**：`ai-gateway-core` 用 `Command::new(exe).arg("-config").arg(path)`
//! 拉起网关子进程（见 `gateway.rs::start`）。若只认位置参数，`-config` 会被当成端口号
//! 解析失败，配置**根本不会被读取** —— 网关「起来了」但用默认配置（无 API key、
//! 错的凭证目录、错的 state 文件），属于静默失效，比直接崩掉更难排查。
//!
//! # ⚠⚠ 子命令必须在解析 `-config` **之前**拦截
//!
//! Go 侧 `cmd/server/main.go:103-122` 就是这么做的，注释给的理由是
//! 「放在 flag.Parse 之前拦截，避免与网关自身的 -config 等参数冲突」。
//!
//! Rust 重写时漏了这一步，后果**不是报错而是挂死**：
//! `gateway qoder-login quota --uid x` 被当成「无 config 启动」，
//! 起了一个 HTTP 服务（默认 `:7863`）并**永不退出**，于是宿主的
//! `Command::output()` 永久阻塞 —— 表现为账号页「刷新」转圈不返回、
//! 20 分钟一次的后台余额巡检整个卡住。
//!
//! 故子命令的派发**必须**是 `main()` 的第一件事。见 [`dispatch_subcommand`]。
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
use ai_gateway_router::authwatch;
use ai_gateway_router::cfgwatch;
use ai_gateway_router::config::Config;
use ai_gateway_router::pool::Pool;
use ai_gateway_router::products::product_tasks::{new_product_tasks_runner, ZcodeTasks};
use ai_gateway_router::products::zcode::claim_scheduler::{
    ClaimScheduler, EnabledFn, LoadCreds, CLAIM_COOLDOWN, CLAIM_POLL_INTERVAL,
};
use ai_gateway_router::server::{router, AppState};
use ai_gateway_router::session::Router as SessionRouter;
use ai_gateway_router::upstream::client::{Client, ClientConfig};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    // ★★ 子命令派发**必须是第一件事**（Go `cmd/server/main.go:103-122`）——
    // 放在 `-config` 解析之前，否则 `qoder-login` 会被当成「无 config 启动」
    // 而起一个永不退出的 HTTP 服务，把宿主的 `Command::output()` 永久挂住。
    if let Some(code) = dispatch_subcommand(&args).await {
        std::process::exit(code);
    }

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
                ai_gateway_router::logline!("加载配置失败 {p}: {e}");
                std::process::exit(1);
            }
        },
        None => {
            // 无 -config：用默认值 + WB2A_* 环境变量（保留旧式调用方式）。
            let mut c = Config::default();
            c.apply_env();
            if let Err(e) = c.normalize() {
                ai_gateway_router::logline!("配置归一化失败: {e}");
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

    // ---- 多产品路由（与 Go `cmd/server/main.go` 的 `if cfg.Pool.MultiProduct` 块对应）----
    //
    // ⚠ 必须在 `auth::load_dir` **之前**接线：Go 侧的 `SetModelPlatforms` 也在
    // 加载账号之前调用。这些 setter 写的是池的**约束字段**（不依赖账号），
    // 先接线可保证账号一进来就受约束 —— 否则「先加载的账号在约束生效前已被
    // 选走一次」这种竞态虽短，但排查起来极难。
    if cfg.multi_product {
        // 成本维度：让各产品按「单位额度消耗率」参与加权（Go 侧同一调用点：
        // `p.SetMultiProduct(true, 0.3)`）。
        pool.set_multi_product(true, 0.3);

        // 「模型 → 允许的平台」白名单（所有者的需求：平台区分使用哪个平台的模型）。
        // 空 = 不限制，行为与加该功能之前逐字相同 —— 这是回滚点。
        pool.set_model_platforms(&cfg.model_platforms);
        if !cfg.model_platforms.is_empty() {
            ai_gateway_router::logline!(
                "模型平台白名单已启用：{} 个模型被限定平台",
                cfg.model_platforms.len()
            );
        }

        // 把「各产品实际提供哪些模型」注入池 —— 用于**无前缀模型名**时排除
        // 「不提供该模型的产品」的账号。
        //
        // ⚠⚠ 顺序不可反：兜底清单**先**注入，宿主清单**后**注入。
        // 兜底清单（网关的猜测）**只用于「不排除」**，不参与「声明」竞争；
        // 宿主清单（来自上游真实查询）才参与「声明」。理由见
        // [`ai_gateway_router::server::static_workbuddy_model_ids`] 的文档。
        let pm = &cfg.product_models;
        let mut fallback: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        // 兜底清单**权威**的产品（未命中即「确定没有」，见
        // [`ai_gateway_router::pool::Pool::product_fallback_authoritative`]）。
        let mut authoritative: Vec<String> = Vec::new();
        if pm.get("workbuddy").map(|v| v.is_empty()).unwrap_or(true) {
            let ids = ai_gateway_router::server::static_workbuddy_model_ids();
            ai_gateway_router::logline!(
                "product_models 缺 workbuddy：已补 {} 个模型作为**兜底**（只用于「不排除」，不参与「声明」竞争）",
                ids.len()
            );
            fallback.insert("workbuddy".to_string(), ids);
        }
        // Trae 同理。★ 少了这一段，**裸名**模型（如 `deepseek-v4.1-flash`）会被
        // 路由到 Trae 账号上：宿主清单里没有 trae 键 ⇒ 判定「不知道」⇒ 在没有任何
        // 兜底清单时 `product_may_serve` 一律放行。2026-10-04 现场实测复现过。
        if pm.get("trae").map(|v| v.is_empty()).unwrap_or(true) {
            let ids = ai_gateway_router::server::static_trae_model_ids();
            ai_gateway_router::logline!(
                "product_models 缺 trae：已补 {} 个模型作为**兜底**（只用于「不排除」，不参与「声明」竞争）",
                ids.len()
            );
            fallback.insert("trae".to_string(), ids);
            // ★★ 2026-10-07：Trae 这份兜底清单是**权威**的。
            //
            // 它不是人工快照，而是派生自 `dispatch::ALIASES` —— 客户端唯一能发的
            // 名字集合，也正是上游 config_name 解析的唯一真相。因此「别名表里没有」
            // ⇒「Trae 上游解析不了这个模型名」是**确定结论**：该产品的账号必须被
            // 硬排除，而不是降级到「最后一档抽签」。
            //
            // 现场（业主 2026-10-07 07:3x，Rust 1.0.32）：`space-bunny` 不在任何一份
            // 兜底清单里 ⇒ 27 个 WorkBuddy 账号与 1 个 Trae 账号一起进最后一档
            // ⇒ 约 3% 概率抽到 Trae ⇒ 业主看到
            // `400 model_not_in_upstream`（「两条产品线的 function 池都拒绝了
            // space-bunny」）。标记权威后 Trae 进 `scoped`，最后一档只剩 WorkBuddy。
            //
            // ⚠ **只**能标记 Trae。WorkBuddy 的静态表是 2026-09-18 快照，把它当权威
            // 会立刻退回 2026-10-06 的 503（`space-bunny` 这类新模型全池挑不出号）。
            authoritative.push("trae".to_string());
            ai_gateway_router::logline!(
                "product_models 缺 trae：兜底清单标记为**权威**（未命中 = 确定没有，其账号不参与裸名兜底抽签）"
            );
        }
        pool.set_product_models_fallback(&fallback);
        pool.set_product_fallback_authoritative(&authoritative);
        pool.set_product_models(pm);

        // 用户白名单（平台 × 区域 → 允许的模型）。
        //
        // 与上面的 `product_models` **分工不同**：那份是上游查询来的**路由提示**，
        // 这份是用户手动配的**准入控制**。只有后者会拦请求 —— 因为
        // 「上游能服务」不等于「用户想让它跑」。
        pool.set_platform_models(&cfg.platform_models);
        if !cfg.platform_models.is_empty() {
            for (plat, by_region) in cfg.platform_models.iter() {
                for (region, models) in by_region.iter() {
                    let label = if region.is_empty() { "不分区域" } else { region.as_str() };
                    ai_gateway_router::logline!("平台白名单：{plat} / {label} → {} 个模型", models.len());
                }
            }
        } else {
            ai_gateway_router::logline!("平台白名单未配置（platform_models 为空）：显式指定平台时不拦模型，行为与引入前一致");
        }

        // 用户**禁用**清单（平台 × 区域 → 不许跑的模型）。
        //
        // ⚠ 它是**否决项**，先于上面那份白名单的并集判定：并集语义下
        // 「从白名单里删掉一个模型」是删不掉的（接口返回/兜底那两支仍会放行
        // 它），故「不许它跑」必须是一条独立的、压过并集的判据。
        //
        // 在 `set_platform_models` 之后调用只是习惯顺序；两者写的是互不相干的
        // 字段，判定顺序由 `platform_allows_model` 保证。
        //
        // `None` 与「配了个空的」在池里等价（都是「不否决任何模型」）——
        // 用 `Option` 只是为了在**日志**里区分「用户没表态」与「用户表了态但没列」。
        match &cfg.platform_models_disabled {
            Some(m) => {
                pool.set_platform_models_disabled(m);
                if !m.is_empty() {
                    for (plat, by_region) in m.iter() {
                        for (region, models) in by_region.iter() {
                            let label = if region.is_empty() { "不分区域" } else { region.as_str() };
                            ai_gateway_router::logline!(
                                "平台禁用清单：{plat} / {label} → {} 个模型（否决项，压过白名单并集）",
                                models.len()
                            );
                        }
                    }
                } else {
                    ai_gateway_router::logline!("平台禁用清单未配置（platform_models_disabled 为空）：不否决任何模型");
                }
            }
            None => {
                pool.set_platform_models_disabled(&std::collections::HashMap::new());
                ai_gateway_router::logline!("平台禁用清单未配置（platform_models_disabled 为空）：不否决任何模型");
            }
        }
    } else {
        ai_gateway_router::logline!("多产品路由已关闭（pool.multi_product=false）：只使用 WorkBuddy 账号");
    }

    // 平台总开关：该平台是否**参加兼容网关路由**。
    //
    // ★ 语义 = 账号级 `no_route` 的平台级版本（所有者原话「关闭平台只是不参与兼容
    // 网关,并不是不执行任务请注意」）：只把该平台的账号从**选号**里排除；
    // 签到 / 上报 / 成长任务 / 额度巡检**照常运行**。所以它绝不写进账号的
    // `disabled`，也**不**影响 `probe_uids` / `available_uids`。
    //
    // ★ 刻意放在 `if cfg.multi_product` **之外**：开关的语义与多产品模式无关 ——
    // 关掉 WorkBuddy 就是「连 WorkBuddy 也不接流量」，此时给出 400
    // `platform_not_in_routing` 比「静默只用 WorkBuddy」更符合用户的点击意图。
    //
    // 在池里「键不存在」= 参加（老配置行为逐字不变），故这里只打印被**关掉**的平台。
    pool.set_platform_enabled(&cfg.platform_enabled.clone().unwrap_or_default());
    match cfg.platform_enabled.as_ref().filter(|m| !m.is_empty()) {
        Some(m) => {
            let mut off: Vec<&String> = m.keys().collect();
            off.sort();
            for plat in off {
                ai_gateway_router::logline!(
                    "平台总开关：{plat} 已关闭「参加兼容网关路由」—— 它的账号不参与选号，养号任务不受影响"
                );
            }
        }
        None => {
            ai_gateway_router::logline!("平台总开关未配置（platform_enabled 为空）：所有平台都参加兼容网关路由");
        }
    }

    // 「限制使用的模型」白名单：**三个工作模式都生效**（不再只在轮转下）。
    //
    // ★ 这条日志是用户排查「为什么客户端被拒」的**第一现场**：网关子进程的
    // stdout 在 GUI 里可能被丢弃，因此把**生效的完整名单**打出来，用户从日志
    // 就能看出自己配的是哪几个，而不是只能看到「被拒了」（Go `main.go:244-248`
    // 逐字）。名单已在 `cfg.normalize()` 里归一化过 —— 所以这里打印的就是
    // 请求路径上实际比较的那一份，不会出现「日志说放行 a、被拒文案却说 b」。
    if !cfg.pool.allowed_model.is_empty() {
        ai_gateway_router::logline!(
            "pool: 已限制可使用的模型，只放行 {}（其他模型一律拒绝；改配置后需重启网关）",
            ai_gateway_router::model_allow::allowed_models_text(
                cfg.pool.allowed_model.as_slice()
            )
        );
    } else if cfg.pool.rotation {
        // 轮转但未限制模型：语义不完整（客户端可换模型绕过额度控制），
        // 但这是合法配置，只提示不拦。
        ai_gateway_router::logline!(
            "pool: 已启用「积分轮转」模式，但未限制模型（pool.allowed_model 为空）—— 建议在界面「放行模型」里选择"
        );
    }

    let accounts = auth::load_dir(&cfg.auth_dir).unwrap_or_default();
    let n = accounts.len();
    // ★ 用 sync_to_dir 而不是逐个 add()（对应 Go `cmd/server/main.go:206 p.SyncToDir(auths)`）。
    //
    // 逐个 add() 只做「加入/换新」，**不会剔除**。而 `load_state()` 会为 state.json 里
    // 的**每个**条目建一个占位条目（`access_token` 为空串，见 pool.rs 的 apply_accounts），
    // 于是「凭证文件已被删除、但 state.json 仍有条目」的账号会**在每次启动时复活**成
    // 空 token 账号 —— 而 `healthy()` 与 `pick_inner()` 都**不检查 access_token 是否为空**
    //（只有 `probe_uids()` 检查），所以它会被正常选中，用空 token 发请求 ⇒ 必然 401 ⇒
    // 可能被当成 ErrSessionDead 禁用掉一个本就不该存在的账号。
    //
    // sync_to_dir 的剔除是**按 uid 全量比对**的，前提是 auths 覆盖池里「归它管」的
    // 全部账号 —— 这里满足：池里此刻**只有** WorkBuddy 的账号（多产品的 pool.add 在
    // 下面 `if cfg.multi_product` 块里才发生）。★ 这个顺序不能颠倒。
    pool.sync_to_dir(&accounts);
    if n == 0 {
        ai_gateway_router::logline!("warning: 未从 {} 加载到任何凭证", cfg.auth_dir);
    }

    // ---- 产品凭证（Qoder / ZCode）：与 WorkBuddy **分开加载** ----
    //
    // 对应 Go `cmd/server/main.go:442-499` 的 `if cfg.Pool.MultiProduct` 块。
    //
    // # 为什么必须分开加载，而不是让 `auth::load_dir` 一起扫
    //
    // 三者的凭证形状**完全不同**：WorkBuddy 是 OAuth 的
    // `{auth:{accessToken,...},account:{...}}`；Qoder 是 `dt`/`drt` 加机器指纹；
    // ZCode 是一个 `{apiKey}.{secret}` 字符串加 JWT。硬塞进一个解析器只会得到
    // 「解析失败静默跳过」—— 而 `auth::load_dir` **刻意**是静默跳过的口径，
    // 于是产品账号会**看起来像没导入**，用户完全无从判断。
    //
    // 故这里沿用 Go 的分工：各自的加载器各自报错（见
    // [`ai_gateway_router::products::qoder::load_credentials`] 的说明）。
    //
    // # 为什么必须在 `auth::load_dir` 之后
    //
    // 池以 `uid` 为键。产品凭证的 `uid` 是派生的（ZCode 是 `zcode-<hash>`），
    // 与 WorkBuddy 的 uid 不会撞；顺序在这里只是为了日志可读。
    let mut qoder_accounts = 0usize;
    let mut zcode_accounts = 0usize;
    let mut trae_accounts = 0usize;

    if cfg.multi_product {
        // ---- Qoder ----
        let (qcreds, qfailed) = ai_gateway_router::products::qoder::load_credentials(
            &cfg.qoder_auth_dir,
        );
        // 解析失败的文件**必须报出来**：用户会以为「账号导入了但没生效」，
        // 而真正原因是格式不对（Qoder 凭证可能是手写的）。
        for f in &qfailed {
            ai_gateway_router::logline!("Qoder 凭证解析失败，已跳过：{f}");
        }
        for a in qcreds {
            pool.add(a);
            qoder_accounts += 1;
        }
        ai_gateway_router::logline!(
            "多产品路由已开启：Qoder 凭证目录 {}，载入 {} 个账号（解析失败 {} 个）",
            cfg.qoder_auth_dir,
            qoder_accounts,
            qfailed.len()
        );

        // ---- ZCode ----
        //
        // 与 Qoder 同款：失败**逐条报出来**，不静默跳过（ZCode 凭证多半是用户
        // 手写粘贴导入的，格式错误必须让他看见，否则表现为「导入了但没生效」）。
        //
        // ★ 刻意**没有**「加载时补派生字段并写盘」那一步（Qoder 有）：ZCode 的
        //   `deviceMid` 铸造与落盘已在 `cred::load_file` 内部完成，`jwt` /
        //   `accountId` 都是直接解析出来的，没有「不写盘就会漂移」的状态。
        //   多写一次盘只会平白引入「网关启动即改写用户凭证文件」的风险。
        let (zcreds, zfailed) =
            ai_gateway_router::products::zcode::load_credentials(&cfg.zcode_auth_dir);
        for f in &zfailed {
            ai_gateway_router::logline!("ZCode 凭证解析失败，已跳过：{f}");
        }
        for a in zcreds {
            pool.add(a);
            zcode_accounts += 1;
        }
        ai_gateway_router::logline!(
            "多产品路由已开启：ZCode 凭证目录 {}，载入 {} 个账号（解析失败 {} 个）",
            cfg.zcode_auth_dir,
            zcode_accounts,
            zfailed.len()
        );

        // ---- Trae ----
        //
        // 与 Qoder / ZCode 同款判据，但**写入面**比两者都紧一档。
        //
        // ★★ Trae 的凭证是**宿主独占**的：`%USERPROFILE%\.wb-switch\trae_accounts.json`
        //    由宿主（DSH 的账号管理器）读写，网关只读。这里刻意**没有**「加载时
        //    补派生字段并写盘」那一步（Qoder 有）：
        //      · `trae_device.json` 里的 EC P-256 设备密钥是宿主的指纹身份，网关注入
        //        自己的 machine_id 会让同一账号在两个进程里呈现两种设备指纹；
        //      · 网关唯一被允许写的路径是 `cred::save_refreshed` —— 它只把**刚换到的**
        //        新令牌合并回原文件（且先读回磁盘以保留宿主写入的 `no_route`），
        //        刷新之外不碰用户的文件。
        let (tcreds, tfailed) =
            ai_gateway_router::products::trae::load_credentials(&cfg.trae_auth_dir);
        for f in &tfailed {
            ai_gateway_router::logline!("Trae 凭证解析失败，已跳过：{f}");
        }
        for a in tcreds {
            pool.add(a);
            trae_accounts += 1;
        }
        ai_gateway_router::logline!(
            "多产品路由已开启：Trae 凭证目录 {}，载入 {} 个账号（解析失败 {} 个）",
            cfg.trae_auth_dir,
            trae_accounts,
            tfailed.len()
        );
    }

    // ---- ZCode 自动领取调度器（Go `main.go:542-566`）----
    //
    // ★★ 这是 P0#2 的核心接线：此前 Rust 侧 `ClaimScheduler` 实现齐备但**没有
    //    任何调用方**，于是「启动即跑 + 5 分钟轮询 + hold 硬闸」这套参考实现的
    //    语义在发行包里等于不存在（ZCode 套餐只有靠宿主每 20 分钟的巡检间接触发
    //    Qoder 领取，ZCode 的分钟级抢购完全没跑）。
    //
    // # 为什么 enabled 是「启动时取一次值」而不是每次读配置
    //
    // Go 的 `enabled` 闭包读的是 `cfg` 变量，而 `startConfigWatch` 的热重载
    // **只**覆盖 `ProductModels` / `PromptMode` / `PromptText`（见
    // `main.go:startConfigWatch` 里的 `h.ReloadDynamic`），`ZcodeClaimOn` 不在
    // 其中 —— 所以 Go 里这个开关实际也是启动时定死的。Rust 侧照搬，
    // 不制造「Go 要重启、Rust 不用」的行为差异。
    //
    // # 为什么 client 传 `None`
    //
    // `None` ⇒ 自建 `Client::new()`，**不带代理**。这与 Go `main.go:552` 的
    // `NewClaimScheduler(zcode.New(), …)` 完全一致（那个 `zcode.New()` 也没装
    // 代理 —— 装代理的是 `:525-536` 那个给对话用的 `zc`）。按等价原则照搬，
    // 不「顺手修好」它。
    let zcode_tasks: Option<ZcodeTasks> = if cfg.multi_product {
        let dir = cfg.zcode_auth_dir.clone();
        // ★ 同一个 `Arc` 同时交给调度器与手动 runner —— 两者必须看同一个目录，
        //   否则运行期改配置会让它们分叉（Go 侧两者都是 `zd.LoadCreds`）。
        let load_creds: LoadCreds = Arc::new(move || {
            Ok(ai_gateway_router::products::zcode::cred::load_dir(&dir))
        });
        let zcode_on = cfg.zcode_claim_active();
        let enabled: EnabledFn = Arc::new(move || zcode_on);
        let sched = Arc::new(ClaimScheduler::new(
            None,
            Some(Arc::clone(&load_creds)),
            Some(enabled),
        ));
        Some(ZcodeTasks { sched, load_creds })
    } else {
        None
    };

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
        // Web（成长中心）域覆盖：与上面三个同因 —— 领奖与 Web 指纹上报打的是
        // `www.workbuddy.cn` / `www.workbuddy.ai`，A/B 对照必须能把它们也指到假上游。
        web_base_cn: std::env::var("WB2A_WEB_BASE_CN").unwrap_or_default(),
        web_base_intl: std::env::var("WB2A_WEB_BASE_INTL").unwrap_or_default(),
        // 设备 token 服务（宿主本地服务）。空 = 不发 `X-Device-Token`。
        //
        // ★ 与 Go 侧 `cfg.Pool.WbDeviceTokenURL` 同口径：**空 URL 即不启用**，
        // 由 `DeviceTokenProvider::new` 归一成 `None`。这里不做 trim 判空，
        // 因为 provider 自己会判（避免两处判据不一致）。
        device_token_url: cfg.wb_device_token_url.clone(),
        device_token_token: cfg.wb_device_token_token.clone(),
    }) {
        Ok(c) => c,
        Err(e) => {
            ai_gateway_router::logline!("构造上游客户端失败: {e}");
            std::process::exit(1);
        }
    };

    // 产品上游（Qoder / ZCode）构造要用它，而它随后被 move 进 `AppState`
    // ⇒ 先留一份。`Client` 是 `Clone`（内含两个 `reqwest::Client`，克隆共享连接池）。
    let product_client = client.clone();

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

    // ---- 凭证目录热加载（对应 Go `internal/pool/watch.go`）----
    //
    // # 为什么位置必须在这里
    //
    // ① 必须在上面「启动对齐」（`auth::load_dir` → `pool.add`）**之后**：监听建立
    //    的是**当前**目录指纹作基线，排在前面的话，两者之间发生的目录变化会被算进
    //    基线而永远不触发重载（Go `main.go:208-217` 逐字记录同一个理由）。
    // ② 必须在 `pool` 变成 `Arc<Mutex<_>>` **之后**：轮询线程要共享同一个池。
    // ③ 多产品账号（Qoder / ZCode）此时**已经**在池里 —— 这不是问题：热加载的剔除
    //    过滤器显式放过「不归本目录管」的账号（见 `authwatch::keep_workbuddy_only`），
    //    两个顺序都对。
    //
    // 句柄绑定到 `_auth_watch`：`main` 结束（含 ctrl-c 优雅退出）时随作用域 drop，
    // `Drop` 会停掉轮询线程。`stop()` 本身幂等。
    let _auth_watch = if cfg.watch_auth_dir {
        authwatch::start_auth_dir_watch(
            Arc::clone(&pool),
            &cfg.auth_dir,
            cfg.parsed.watch_auth_dir_interval,
        )
    } else {
        // 用户主动关掉：这条日志要**明确指向配置键**，否则用户看到的是
        // 「加账号没生效」而不知道是自己关的。
        ai_gateway_router::logline!("凭证目录热加载已禁用（pool.watch_auth_dir=false）：新增账号后需手动重启网关");
        authwatch::AuthDirWatch::inert()
    };

    // ---- ★ 配置热重载 + 多产品凭证热加载 ----
    //
    // 对应 Go `cmd/server/main.go:847-936` 的 `startConfigWatch`。两件事放同一个循环里
    // 是因为触发源是**同一个用户动作**（界面上的「刷新账号」）：宿主会同时（a）重写
    // 配置里的 `pool.product_models`、（b）往 qoder/zcode 的 auths 目录写新凭证。
    //
    // # 为什么必须有它（所有者报过的缺陷）
    //
    // 宿主的 `resync_native_config` 只**重写配置、不重启网关**（调用点见
    // `qoder_login.rs` / `zcode_login.rs` / `multi_product_credit_patrol.rs`），
    // 设计前提就是「运行中的网关会自己读到新配置」。没有这个监听时，用户新增/刷新
    // Qoder 或 ZCode 账号后，`/v1/models` 的 channels 里**永远**没有这些产品的模型，
    // 除非手动重启整个应用 —— 正是 Go `handler.go:176-193` 逐字记着的现场。
    //
    // ★ 位置：**必须**排在前面所有 `pool.add` 之后 —— 基线指纹取的是调用时刻，
    //   排在前面会把「基线建立之前发生的变化」永久漏掉（Go `main.go:208-217` 同款理由）。
    //
    // ★ `_auth_watch` 与本句柄都要活到 `main` 结束（绑定到局部变量，靠 `Drop` 停线程）。
    let dynamic = cfgwatch::DynamicState::new(cfgwatch::DynamicConfig {
        // 初值 = 启动配置，对齐 Go「可热更新字段的初值：与启动配置一致」。
        product_models: cfg.product_models.clone(),
        // ★ 平台总开关也进热更新面：它在账号页是**随手点**的，走
        // 「写配置 → 重启网关」那条路会让每次点击都掐断正在跑的请求。
        platform_enabled: cfg.platform_enabled.clone().unwrap_or_default(),
        prompt_mode: cfg.parsed.prompt_mode.clone(),
        prompt_text: cfg.parsed.prompt_text.clone(),
    });
    let _cfg_watch = cfgwatch::start_config_watch(
        cfgwatch::WatchSpec {
            // 无 `-config` 时（`Config::default()` + 环境变量那条路径）没有可监听的文件，
            // 传空串即整条关闭配置热重载 —— 但三个产品的凭证热加载仍然有效。
            config_path: config_path.clone().unwrap_or_default(),
            // 与 Go `resolveAuthDir(configured, fallback, enabled)` 同口径：未启用多产品
            // 时传空串（= 该产品未启用），不监听它的目录。
            qoder_dir: if cfg.multi_product {
                cfg.qoder_auth_dir.clone()
            } else {
                String::new()
            },
            zcode_dir: if cfg.multi_product {
                cfg.zcode_auth_dir.clone()
            } else {
                String::new()
            },
            trae_dir: if cfg.multi_product {
                cfg.trae_auth_dir.clone()
            } else {
                String::new()
            },
            interval: cfgwatch::DEFAULT_CONFIG_WATCH_INTERVAL,
        },
        Arc::clone(&dynamic),
        Arc::clone(&pool),
    );

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

    // ==================== 养号任务调度器（2026-10-02 接通）====================
    //
    // 对应 Go `cmd/server/main.go:336-395` 的 `scheduler.New(...)` + `go sch.Run(ctx)`
    // + `go sch.RunCreditRefreshLoop(ctx, ...)`。
    //
    // ★ 账号记录回写：整块缺席（老宿主）时 `file` 为空 ⇒ `Recorder` 不启用，
    // 所有写入静默 no-op。这与 Go 一致，也是独立运行 `gateway.exe` 时的正确行为。
    let records = {
        let path = cfg.account_records.file.trim();
        let rec = ai_gateway_router::records::Recorder::new(
            path,
            cfg.account_records.retention_days,
            cfg.record_identities(),
        );
        if rec.enabled() {
            ai_gateway_router::logline!(
                "账号记录回写已启用：{}（保留 {} 天）",
                rec.path(),
                if cfg.account_records.retention_days > 0 {
                    cfg.account_records.retention_days
                } else {
                    ai_gateway_router::records::DEFAULT_RETENTION_DAYS
                }
            );
        } else {
            ai_gateway_router::logline!("账号记录回写未启用（配置缺少 account_records.file）：任务照跑，但界面不会有记录");
        }
        Arc::new(rec)
    };
    // ==================== 成长任务执行体（2026-10-02 接通）====================
    //
    // 对应 Go `cmd/server/growth_tasks.go` 的 `growtask.New(growtask.Options{
    // Upstream: up, Records: recorder})`。
    //
    // ★ 记录器直接复用上面那个 `Arc<Recorder>`：未启用时三个 writer 全是静默
    // no-op，所以**不需要**在这里再判一次空（与 Go 的 `*records.Recorder` nil
    // 语义等价）。
    //
    // ★ 参数全走默认（`Options::new` 填 Go 的 default* 常量）：`mp_chat_gap` 的
    // 45 秒不是保守取值，而是 `Sequential_Tasks_3` 那条链的反作弊阈值 —— 调小
    // 不会更快完成，只会让整轮上报被上游回滚。
    let growtask = Arc::new(ai_gateway_router::growtask::Runner::new(
        ai_gateway_router::growtask::Options::new(client.clone())
            .with_records(Some(Arc::clone(&records))),
    ));
    let sched_cfg = ai_gateway_router::scheduler::Config {
        pool: Some(Arc::clone(&pool)),
        upstream: client.clone(),
        checkin_hours: cfg.schedule.checkin_hours.clone(),
        keepalive_hours: cfg.schedule.keepalive_hours.clone(),
        activity_hours: cfg.schedule.activity_hours.clone(),
        trial_hours: cfg.schedule.trial_hours.clone(),
        school_hours: cfg.schedule.school_hours.clone(),
        night_owl_hours: cfg.schedule.nightowl_hours.clone(),
        // ★ 用「禁用」而不是「启用」命名，与 Go `Config` 逐字一致：零值 Config
        // 即两类任务都启用 —— 与引入开关之前的行为完全相同。
        checkin_disabled: !cfg.schedule.checkin_enabled,
        keepalive_disabled: !cfg.schedule.keepalive_enabled,
        activity_disabled: !cfg.schedule.activity_enabled,
        night_owl_disabled: !cfg.schedule.nightowl_enabled,
        school_disabled: !cfg.schedule.school_enabled,
        trial_disabled: !cfg.schedule.trial_enabled,
        qoder_claim_hours: cfg.qoder_claim_hours.clone(),
        // ★ 两级回落：分产品键优先，缺席回落到总闸 `product_tasks_enabled`。
        // 直接读 `qoder_claim_enabled` 会把老配置（只有总闸）当成关闭。
        qoder_claim_disabled: !cfg.qoder_claim_active(),
        activity_report_count: cfg.schedule.activity_report_count,
        checkin_scope: cfg.schedule.checkin_scope.clone(),
        records: Some(records),
    };
    let scheduler = Arc::new(ai_gateway_router::scheduler::Scheduler::new(sched_cfg));

    // ★★ 接线：把产品任务执行体交给排程器（Go `main.go:572-576`）。
    //
    // 不接的话 `/tasks/run?task=product_tasks` 会回
    // `skipped(..., "该构建未接入产品任务执行体")`，而时点排程（10:00 / 21:00）
    // 触发后**静默 no-op** —— 这正是 P0#2 的症状。
    //
    // ★ 整段（含两条日志）都在 `cfg.multi_product` 门内，与 Go 一致：
    //   Go 的 `SetProductTasksRunner` 位于 `if cfg.Pool.MultiProduct {` 块内
    //   （`main.go:442` 开、`:690` 关），多产品关闭时**根本没有 runner** ——
    //   此时 `/tasks/run?task=product_tasks` 回 `skipped(...)` 才是正确行为，
    //   而不是「去领一个没配置的平台」。这也顺带保证了「多产品关闭 ⇒ 不出现
    //   产品领取日志」。
    if cfg.multi_product {
        let qoder_claim_on = cfg.qoder_claim_active();
        if let Some(runner) = new_product_tasks_runner(
            zcode_tasks.clone(),
            cfg.qoder_auth_dir.clone(),
            qoder_claim_on,
        ) {
            scheduler.set_product_tasks_runner(runner);
        }
        // 两条日志逐字对齐 Go（`main.go:577-580`）。
        if zcode_tasks.is_some() {
            ai_gateway_router::logline!(
                "ZCode 自动领取：启动即跑，之后每 {} 轮询（失败冷却 {}，开关={}）",
                ai_gateway_router::products::product_tasks::go_duration_text(CLAIM_POLL_INTERVAL),
                ai_gateway_router::products::product_tasks::go_duration_text(CLAIM_COOLDOWN),
                cfg.zcode_claim_active()
            );
        }
        ai_gateway_router::logline!(
            "Qoder 权益领取：时点 {}（开关={}，超时 {} 补一轮轮询）",
            ai_gateway_router::products::product_tasks::go_int_slice_text(&cfg.qoder_claim_hours),
            qoder_claim_on,
            cfg.qoder_claim_interval_minutes
        );
    }

    let sched_cancel = ai_gateway_router::scheduler::Cancel::new();
    // ★★ 接线：计费周期刷新（Qoder `/user/status` 的 `nextResetAt`）搭积分巡检的便车。
    // 对应 Go `cmd/server/main.go:757-787`。
    //
    // # 为什么在这里接线而不是写进 `scheduler::Config`
    //
    // `scheduler` 在上面就建好了，而执行体依赖的账号池 / 用量统计器 / `Cancel`
    // 到这一行才都齐备 —— 走 `Config` 会引用尚未赋值的变量。这正是
    // `set_product_tasks_runner` 存在的同一理由。
    //
    // ⚠ **必须**在 `run_credit_refresh_loop`（下面的 `tokio::spawn`）**之前**调用：
    // 那个循环启动时立刻跑一轮，之后再按 interval 跑；顺序反了会漏掉启动那一轮
    //（要等一个 interval）。
    //
    // ⚠ 池里没有 Qoder 账号 / 多产品关闭时返回 `None` ⇒ 回调不装，循环里
    // `run_billing_cycle_refresh` 直接跳过（**不 panic**）。
    if let Some(runner) = ai_gateway_router::products::qoder::billing_cycle::
        new_billing_cycle_refresh_runner(
            Arc::clone(&pool),
            Arc::clone(&usage),
            sched_cancel.clone(),
        )
    {
        scheduler.set_billing_cycle_refresh_runner(runner);
    } else {
        // 逐字对齐 Go `main.go:785-787` 的口径：让用户知道 `/usage` 的
        // `billingCycles` 为什么是空的。
        ai_gateway_router::logline!("计费周期刷新未接线（池里没有 Qoder 账号或未启用多产品）：/usage 的 billingCycles 将为空");
    }

    // ★★ 接线：Trae 账号积分采集（POST /trae/api/v2/pay/ide_user_ent_usage）。
    //
    // 为什么是**独立**一条 tokio::spawn 定时循环，而不是搭上面那条积分巡检的便车：
    // scheduler 的积分巡检带 WorkBuddy 产品闸门（scheduler/mod.rs:981，那条出站请求
    // 打的是 WorkBuddy 端点），Trae 的端点 / 凭证 / 头表口径完全不同，塞进去要改
    // scheduler 的产品语义；也**不**复用 Qoder 的计费周期槽位（scheduler 里那是
    // 另一个单槽回调）。所以这里起第三条循环。
    //
    // ⚠ 池里没有 Trae 账号时 new_credits_refresh_runner 返回 None ⇒ 回调不装、
    // task 不启、**一个出站请求都不发**（与 qoder 那处同口径）。
    if let Some(runner) = ai_gateway_router::products::trae::credits::new_credits_refresh_runner(
        Arc::clone(&pool),
        sched_cancel.clone(),
    ) {
        let interval =
            ai_gateway_router::products::trae::credits::DEFAULT_CREDITS_REFRESH_INTERVAL;
        ai_gateway_router::logline!(
            "Trae 积分采集已接线：每 {} 分钟一轮，每个 Trae 账号每轮 1 次出站（ent_usage）",
            interval.as_secs() / 60
        );
        let cancel = sched_cancel.clone();
        tokio::spawn(async move {
            loop {
                runner().await;
                if !cancel.sleep(interval).await {
                    return;
                }
            }
        });
    }

    // 签到范围日志（与 Go `main.go:380-395` 同口径）。
    if cfg.checkin_scope_all() {
        ai_gateway_router::logline!("签到与猫猫旅行范围：国服 + 国际版（schedule.checkin_scope=all）");
    } else {
        ai_gateway_router::logline!("签到与猫猫旅行范围：仅国服（schedule.checkin_scope=cn，国际版账号自动跳过）");
    }

    let state = Arc::new(AppState {
        pool,
        client,
        session,
        api_key: cfg.api_key.clone(),
        max_rotate: cfg.pool.max_rotate.max(1),
        soft_cooldown: cfg.parsed.soft_rate,
        refresh_skew: Duration::from_secs(600),
        redis_mode: "noop".into(),
        // 「限制使用的模型」白名单：**三个工作模式都生效**。
        //
        // ★ 不再按 `rotation` 过滤（Go `main.go:720-726` 逐字）：限制模型与
        // 「用哪些账号」是正交的两件事，只在轮转下生效会让自动/手动模式完全
        // 无法限制模型 —— 那是**安全方向**的缺陷（用户以为锁住了，实际没锁）。
        //
        // 归一化已在 `cfg.normalize()` 里做完（去空白、剥产品/区域前缀、丢空项），
        // 这里直接透传；请求路径上只做大小写不敏感比较。
        allowed_model: cfg.pool.allowed_model.as_slice().to_vec(),
        usage,
        // 产品上游（Qoder / ZCode）。
        //
        // ★ 空集合 = 所有产品账号都按 WorkBuddy 走 —— 这正是**引入多产品之前**
        // 的行为，也是单产品部署（`multi_product=false`）下的正确取值。
        //
        // ★★ 判据是 `multi_product`（配置开关），**不是**「启动时载入了几个账号」。
        //
        // 早期版本按 `qoder_accounts > 0` 决定装不装，理由是「装了没账号只是白占
        // 内存」。那个取舍在**没有热加载**时成立，加了热加载之后就成了缺陷：宿主
        // 起网关后才导入的账号永远拿不到上游 —— `dispatch()` 返回 `None` ⇒ 回落
        // 到 WorkBuddy 端点 ⇒ 拿 Qoder 的 dt 去打 WorkBuddy 的 API，必然被拒并把
        // 这个好账号冷却掉。这正是下面那段注释警告的症状，只是触发条件从「配置
        // 写错」变成了「账号来得晚一点」。
        //
        // Go 的判据也是开关本身（`main.go:442` 的 `if cfg.Pool.MultiProduct` 里
        // 无条件构造 `qoder.NewDispatch` / `zcode.NewDispatch`），故这里与之一致。
        //
        // 内存代价可忽略：两个上游各自只持一个 HTTP client 与几份配置，不随账号
        // 数量增长。★ `ZcodeUpstream::new` 的两件**进程级**副作用（构造时一次性
        // 完成）：
        //   ① 按宿主写出的原生配置初始化验证码求解器 —— ZCode 对话对任何不带
        //      验证参数的请求一律回 3007，所以这不是可选优化，是「能不能用」的
        //      前提；② 按 `proxy_scope.intl` 决定 ZCode 是否走代理（`zcode.z.ai`
        //      以 `.ai` 结尾，属国际流量）。两件都只打日志、不阻断启动。
        products: {
            let qoder = if cfg.multi_product {
                Some(
                    ai_gateway_router::products::qoder::QoderUpstream::new(
                        product_client.clone(),
                    )
                    .shared(),
                )
            } else {
                None
            };
            // ZCode：与 Qoder 同款判据。
            let zcode = if cfg.multi_product {
                Some(
                    ai_gateway_router::products::zcode::ZcodeUpstream::new(product_client.clone())
                        .shared(),
                )
            } else {
                None
            };
            // Trae：国服与国际版**共用一个上游**（`trae::Realm` 决定 host），
            // 故这里只构造一个实例。判据与 Qoder / ZCode 一致（`multi_product`）。
            let trae = if cfg.multi_product {
                Some(
                    ai_gateway_router::products::trae::TraeUpstream::new(product_client.clone())
                        .shared(),
                )
            } else {
                None
            };
            ai_gateway_router::products::ProductUpstreams {
                qoder,
                zcode,
                trae,
            }
        },
        // 3012 熔断：进程级共享（见 `AppState::unusual` 的说明）。
        unusual: ai_gateway_router::unusual::UnusualBreakers::new(),
        // 出口 IP 被 WAF 拦的判定窗口：同样进程级共享。
        waf_ip: ai_gateway_router::wafip::WafIpWindow::new(),
        // 流式首帧预读超时。
        //
        // ★ 复用 `upstream.header_timeout_seconds`（用户对「等上游开口」的显式表态），
        // 而不是新加一个配置键：两者语义相同（都是「首字节前上限」），多一个键会让
        // 用户面对两个含义几乎一样的数字而不知道该调哪个。
        //
        // 为什么不能用硬编码 30s：思考 40 秒才开始输出的推理模型会被判「上游返回
        // 空流」→ 换号 → 换过去还是同一个模型、同样超时 → 整轮失败，而**上游其实
        // 完全正常**，只是思考久了点。用户看到 `empty upstream stream`。
        probe_timeout: if cfg.upstream.header_timeout_seconds > 0 {
            Duration::from_secs(cfg.upstream.header_timeout_seconds as u64)
        } else {
            Duration::from_secs(30)
        },
        // 按区域的模型清单缓存（`/v1/models`、`/v1/models/regions` 的数据源）。
        // 进程级一份，理由见 `AppState::models`。
        models: Arc::new(ai_gateway_router::models_cache::ModelsCache::new()),
        // ★ 可热更新的配置（模型清单 + 系统提示词）：由上面起的 `_cfg_watch` 在后台
        // 轮询配置文件的指纹，变了就换进这里。池里那份 `Pool::set_product_models`
        // 由监听线程**一并**更新（Go 只换 dynamic 不换池，会让新模型被当成「该产品
        // 不提供」而排除账号 —— 见 `cfgwatch::start_config_watch` 的说明）。
        dynamic,
        // 凭证目录：短信登录成功后往这里写 `workbuddy-<uid>.json`。
        auth_dir: cfg.auth_dir.clone(),
        // ★ 养号任务执行体（`/tasks/run`）。恒注入 —— Go 侧同样恒注入
        // `RunTaskFor`，故生产路径不会走「未配置」那条 503。
        scheduler: Some(Arc::clone(&scheduler)),
        growtask: Some(growtask),
    });

    ai_gateway_router::logline!(
        "state restored={loaded_state} (state_file={})",
        cfg.state_file
    );
    let app = router(state.clone());

    // ★★ 启动即预热上游真值（2026-10-07）。
    //
    // 没有它，进程起来到「第一次有人访问 /v1/models」之间的窗口里，池子既不知道
    // 哪些模型属于哪个区，也不知道上游此刻有哪些模型 —— 裸名请求会选中错区账号，
    // 报「模型在当前通道上不存在」。业主的两次现场都落在窗口内。
    //
    // 放在 `axum::serve` **之前** spawn：不阻塞监听（预热是网络动作，最长可能
    // 几十秒），但也不晚于第一个请求 —— 两个上游请求并行不了（`region_snapshot`
    // 内部按区顺序拉），故它只影响「刚启动那一两秒」的选号质量。
    //
    // ⚠ 失败静默：预热是**优化**不是前提，拉不到就退化成既有行为。
    {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            ai_gateway_router::server::warm_up_live_models(&state).await;
            ai_gateway_router::logline!("[warmup] 上游模型真值预热完成（观测集已填充，裸名请求不再选中错区账号）");
        });
    }

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

    // ==================== 养号任务排程循环 ====================
    //
    // 对应 Go `main.go` 的 `go sch.Run(ctx)` + `go sch.RunCreditRefreshLoop(ctx, ...)`。
    //
    // ★ 两条循环各起一个 task：积分巡检（15 分钟）与任务排程（整点）周期完全不同，
    // 合在一条里会让巡检被任务拖住 —— 巡检是「让选号依据不过时」的只读动作，
    // 被一次长达数分钟的活跃上报阻塞会让分层选号依据过时数据。
    //
    // ★ 取消信号与 `ctrl_c` 联动：`Cancel` 是 `watch` 通道（不是 `Notify`），
    // 排程循环的等待能被立刻唤醒，不会让进程退出时卡在一个长达数小时的
    // `sleep` 里 —— 这正是 axum 的 graceful shutdown 等不到 task 结束的场景。
    {
        let scheduler = Arc::clone(&scheduler);
        let cancel = sched_cancel.clone();
        tokio::spawn(async move {
            scheduler.run(&cancel).await;
        });
    }
    {
        let scheduler = Arc::clone(&scheduler);
        let cancel = sched_cancel.clone();
        tokio::spawn(async move {
            scheduler
                .run_credit_refresh_loop(
                    &cancel,
                    ai_gateway_router::scheduler::DEFAULT_CREDIT_REFRESH_INTERVAL,
                )
                .await;
        });
    }

    // ★★ ZCode 自动领取常驻循环（Go `startProductTasks`，`product_tasks.go:184-198`）。
    //
    // 与上面两条**并列**、互不阻塞：网关排程按小时时点走 WorkBuddy 的签到/上报，
    // 本循环按参考实现的分钟级节奏抢 ZCode 限量套餐。⚠ 为什么不塞进网关排程：
    // 参考实现的节奏是 5 分钟轮询（要抢名额），按小时排会把「抢」变成「捡剩的」。
    if let Some(z) = zcode_tasks.as_ref() {
        let sched = Arc::clone(&z.sched);
        let cancel = sched_cancel.clone();
        tokio::spawn(async move {
            sched.run(&cancel).await;
        });
    }

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind failed");
    ai_gateway_router::logline!("rust gateway listening on http://127.0.0.1:{port} (accounts={n})");

    // 优雅退出：收到 Ctrl+C / 终止信号时先落盘再退出。
    //
    // 为什么必须显式处理：用量与池状态都是「内存累积 + 周期落盘」，
    // 若直接退出会丢掉最后一个周期内的数据（宿主重启网关很频繁 ——
    // 切换工作模式、账号同步都会触发重启）。
    axum::serve(listener, app)
        .with_graceful_shutdown({
            let cancel = sched_cancel.clone();
            async move {
                let _ = tokio::signal::ctrl_c().await;
                // ★ 收到退出信号后必须**主动**取消养号排程循环。
                //
                // 不做这一步的后果：排程循环的等待可以长达数小时（下一个整点），
                // 而 `Cancel` 只有在被 cancel 时才会唤醒它 —— 否则进程会挂在这里
                // 等一条永远不结束的 task。宿主重启网关很频繁（切工作模式、同步
                // 账号都会触发），每次都卡到超时才被杀，会让「重启网关」这件事
                // 时好时坏。
                //
                // ★ 顺序：先 cancel 再返回。排程循环被唤醒后走的是各自的收尾
                // 分支（正在跑的那一轮会停在下一个 `sleep` 检查点），而不是被
                // 硬中断 —— 与 Go 侧 `ctx` 取消的语义一致。
                cancel.cancel();
            }
        })
        .await
        .unwrap();
    state.usage.flush();
    if let Ok(mut pool) = state.pool.lock() {
        pool.flush();
    }
    println!("已落盘，退出");
}

/// CLI 子命令派发（Go `cmd/server/main.go:103-122` 的对应物）。
///
/// 返回 `Some(退出码)` 表示**这是一次子命令调用，调用方应立即 `exit`**；
/// 返回 `None` 表示不是子命令，按常规的 HTTP 网关启动流程继续。
///
/// # 为什么返回值而不是直接 `exit` 在这里
///
/// 让「是不是子命令」的判定与「退出码」的产生分开：子命令实现里有很多
/// 提前返回点（参数错误 2、业务失败 0、上游错误 1），集中在这里 exit
/// 可以保证**无论哪条路径都真的会退出** —— 这一点至关重要，
/// 因为漏掉任何一个「不退出」的分支都会让宿主的 `Command::output()` 永久挂住。
///
/// # 为什么带 `async`
///
/// 子命令要发 HTTP（额度/模型/活动/设备流），而这些实现都是 `async fn`
/// （reqwest 客户端）。`main` 是 `#[tokio::main]`，故这里可以直接 await，
/// 比 Go 侧再起一个 runtime 更简单。
///
/// # ⚠ 退出码契约
///
/// 见 [`ai_gateway_router::cli`] 的模块头。一句话：**业务失败返回 0**
/// （宿主据此读 stdout 的 `{"status":"error"}`），**参数错误返回 2**。
async fn dispatch_subcommand(args: &[String]) -> Option<i32> {
    // `args[0]` 是可执行文件路径，子命令在 `args[1]`。
    let sub = args.get(1)?.as_str();
    let rest = &args[2..];

    Some(match sub {
        // Qoder：设备流登录、客户端凭证导入、额度、模型、权益活动。
        // 默认凭证目录与宿主一致（`~/.wb-switch/qoder/auths`），
        // 这样宿主不传 `--auth-dir` 时也能落到正确位置
        //（Go 侧 `main.go:112` 传的就是 `qoder.DefaultAuthDir()`）。
        "qoder-login" => ai_gateway_router::products::qoder::cli::run(rest).await,
        // ZCode：OAuth / 凭证导入、额度、模型、套餐、激活（warm）。
        "zcode-login" => ai_gateway_router::products::zcode::cli::run(rest).await,
        _ => return None,
    })
}
