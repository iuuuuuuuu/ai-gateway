//! 配置热重载 + 多产品凭证热加载（对应 Go `cmd/server/main.go:841-1073` 的
//! `startConfigWatch`，以及 `internal/server/handler.go:174-306` 的 `DynamicConfig`）。
//!
//! # 为什么需要它（所有者报的现场）
//!
//! Go 侧 `handler.go:176-193` 逐字记录了两个缺陷：
//!
//! > 「兼容网关启动之后，我再添加的 zcode 和 qoder 账号，模型清单路由也没有显示
//! > qoder 和 zcode 支持的账号，应该要自动重启或者热重载的」
//! >
//! > 「而且 /v1/models 接口，也没有返回 qoder 和 zcode 支持的模型，这也是个 bug」
//!
//! 根因链：宿主在**新增/刷新账号**后会重写 `gateway_native_config.json`
//!（`ai-gateway-core` 的 `modules/gateway.rs::resync_native_config` 的调用点
//! `qoder_login.rs` / `zcode_login.rs` / `multi_product_credit_patrol.rs` 都**只重写、
//! 不重启**），把 `pool.product_models` 换成最新清单；而网关只在 `main` 开头读一次
//! 配置，之后再也不读 —— 改文件对运行中的进程完全不可见。于是 `/v1/models` 的
//! channels 里永远没有 Qoder / ZCode，用户只能手动重启整个应用。
//!
//! # 为什么两件事放在同一个循环里
//!
//! 与 Go `main.go:847-856` 同口径：它们的触发源是**同一个用户动作**（「刷新账号」）——
//! 宿主会同时（a）重写配置里的 `product_models`、（b）往 qoder/zcode 的 auths 目录
//! 写新凭证。分成两个循环只会让「清单更新了但账号还没进池」这个中间态持续更久 ——
//! 那正是用户看到的「模型列表里有但选了说账号不可用」。
//!
//! # 为什么凭证热加载不能复用 [`crate::authwatch`]
//!
//! `authwatch` 只监听 **WorkBuddy 的 `auth_dir`**，且其剔除逻辑显式放过非 WorkBuddy
//! 账号（[`crate::authwatch::keep_workbuddy_only`]）。多产品的 qoder/zcode 凭证目录
//! **从来没被监听**。两者的动作也不同：`authwatch` 是「与目录**对齐**」（含剔除），
//! 本模块是「**只增不减**地 upsert」（Go `reloadQoder`/`reloadZcode` 都只 `p.Add`）。
//!
//! # 为什么是轮询
//!
//! 与 `authwatch` 同一口径（`authwatch.rs:9-15`）：零新依赖、容器/NFS 上 fsnotify 会
//! 丢事件、代价可忽略（每 5 秒一次 `metadata` + 一次 `ReadDir`）。
//!
//! # 可热更新的字段是**刻意收窄**的
//!
//! Go `handler.go:195-203` 逐字：「能：**数据类**字段（模型清单、提示词）—— 它们只是
//! 被读取的数据，换掉不影响任何已建立的连接或后台任务。不能：监听地址/端口（要重建
//! listener，属重启语义）、Pool / Upstream / 各回调（持有连接池与后台 goroutine，
//! 换掉会泄漏且让在途请求失去归属）。」
//!
//! ⇒ 本模块**只**更新 [`DynamicConfig`] 的三个字段，绝不重建池、客户端或监听器。

use std::collections::HashMap;
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::auth::Auth;
use crate::config::Config;
use crate::pool::Pool;

/// 默认轮询周期（对应 Go `configWatchInterval`，`main.go:841-845`）。
///
/// 与 Go 同值同理由：「5 秒与 `pool.WatchAuthDir` 的默认值一致：配置变更是低频事件
///（用户点一次"刷新账号"），5 秒延迟无感；而更密只会带来无谓的 IO。」
pub const DEFAULT_CONFIG_WATCH_INTERVAL: Duration = Duration::from_secs(5);

/// 可热更新的配置字段（对应 Go `server.DynamicConfig`，`handler.go:174-213`）。
///
/// ★ 用 [`Arc`] 包着存放（见 [`DynamicState`]）：读方是**请求路径**，读一次指针
/// 比深拷贝一份 `HashMap` 便宜得多。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamicConfig {
    /// 宿主透传的「各产品实际提供哪些模型」（`pool.product_models`）。
    ///
    /// 它同时喂两处：`/v1/models` 的渠道标签（[`crate::capability`]）与池的
    /// 「不排除 / 声明」判定（[`Pool::set_product_models`]）。
    pub product_models: HashMap<String, Vec<String>>,
    /// 「平台 → 是否参加兼容网关路由」总开关（宿主透传的 `pool.platform_enabled`）。
    ///
    /// ★ 语义 = `no_route` 的平台级版本（所有者原话「关闭平台只是不参与兼容网关,
    /// 并不是不执行任务请注意」）：只影响选号，签到 / 上报 / 成长任务 / 额度巡检照跑。
    ///
    /// 为什么放进热更新面：这个开关是用户在账号页**随手点**的，走
    /// `set_platform_models` 那条「写配置 → 重启网关」的路会让每次点击都掐断
    /// 正在跑的请求。而池读的就是这份 map，热加载即可生效。
    pub platform_enabled: HashMap<String, bool>,
    /// 每日 Token 上限（每账号；`0` = 关闭）—— [crate::pool::Pool::set_daily_guard] 的第一个旋钮。
    ///
    /// 为什么进热更新面：与 `platform_enabled` 同款理由 —— 它是用户在账号页
    /// **随手改**的旋钮，走「写配置 → 重启网关」会让每次调整都掐断正在跑的请求。
    ///
    /// ★ 用 `i64` 而不是 `f64`：本结构体派生了 `Eq`，浮点进不来。
    pub daily_token_cap: i64,
    /// 保留积分下限（每账号；`0` = 关闭）—— 第二个旋钮。
    pub credit_floor: i64,
    /// 系统提示词模式（已归一化：`passthrough` / `custom` / `append`）。
    pub prompt_mode: String,
    /// 系统提示词文本（`passthrough` 下恒空）。
    pub prompt_text: String,
}

impl Default for DynamicConfig {
    fn default() -> Self {
        Self {
            product_models: HashMap::new(),
            // ★ 缺省空 map = 所有平台都参加路由（与老配置行为逐字一致）。
            platform_enabled: HashMap::new(),
            // ★ 缺省 0 = 每日护栏关闭（同款立场：升级不改行为）。
            daily_token_cap: 0,
            credit_floor: 0,
            // ★ 缺省 passthrough：与 `[crate::config::PromptConfig::default]` 同口径。
            prompt_mode: crate::prompt::MODE_PASSTHROUGH.to_string(),
            prompt_text: String::new(),
        }
    }
}

/// 可热更新的配置状态（对应 Go `Handler.dynamic` 的 `atomic.Pointer[DynamicConfig]`）。
///
/// # 为什么不是 `ArcSwap` / `parking_lot`
///
/// Go 用 `atomic.Pointer` 的理由（`handler.go:156-161` 逐字）是「读方是**请求路径**…
/// 写方是低频的"宿主重写了配置"。atomic 的读开销是一次指针加载，无锁竞争，且天然
/// 不会有撕裂读。」Rust 侧最贴近的等价物是 `arc_swap::ArcSwap`，但**那要加依赖** ——
/// 而「零新依赖」是本仓库的既有约束（`authwatch.rs:13` 明写）。
///
/// 故这里用 `RwLock<Arc<DynamicConfig>>`：读方拿读锁后**克隆一个 `Arc`**（不是深拷贝），
/// 持锁时间是一次 `Arc::clone` + 引用计数自增 —— 与 atomic 的指针加载同阶，且不会有
/// 撕裂读（锁保证了「读到的一定是某个完整版本」）。
///
/// 为什么不是 `Mutex`：读多写极少，读锁之间不该互相排队。
#[derive(Debug)]
pub struct DynamicState {
    inner: RwLock<Arc<DynamicConfig>>,
}

impl DynamicState {
    /// 建立初始状态（对应 Go `NewHandler` 里那句 `h.dynamic.Store(&DynamicConfig{…})`：
    /// 「可热更新字段的初值：与启动配置一致」）。
    pub fn new(initial: DynamicConfig) -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(Arc::new(initial)),
        })
    }

    /// 取当前快照（对应 Go `DynamicSnapshot()`，永不为 `None` —— 初始值一定在）。
    ///
    /// 返回 [`Arc`] 而不是克隆整个结构：调用方多半只读一两个字段。
    pub fn snapshot(&self) -> Arc<DynamicConfig> {
        // 锁中毒（写方 panic）时沿用内部值：与 `server.rs` / `authwatch.rs` 同一口径 ——
        // 配置数据仍然自洽，没理由因为一次 panic 让整个网关停摆。
        match self.inner.read() {
            Ok(g) => Arc::clone(&g),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// 换上新的一份（对应 Go `ReloadDynamic`）。
    ///
    /// ★ 整份替换而不是逐字段合并：`DynamicConfig` 只有三个数据字段，逐字段合并会让
    /// 「某字段本次没读到 ⇒ 保持旧值」变成默认行为 —— 而配置解析成功后所有字段都是
    /// 确定的，那只会掩盖 bug。Go 的 `ReloadDynamic` 同样是 `h.dynamic.Store(next)`。
    pub fn reload(&self, next: DynamicConfig) {
        let next = Arc::new(next);
        match self.inner.write() {
            Ok(mut g) => *g = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
    }

    /// 当前模型清单（`/v1/models` 用）。
    pub fn product_models(&self) -> HashMap<String, Vec<String>> {
        self.snapshot().product_models.clone()
    }
}

/// 监听参数。
#[derive(Debug, Clone)]
pub struct WatchSpec {
    /// 配置文件路径（宿主启动网关时给的 `-config <path>`）。
    ///
    /// ★ 空串 = 没走 `-config`（`Config::default()` + 环境变量那条路径）：此时**没有
    /// 可监听的文件**，配置热重载整条关闭（但凭证热加载仍然可用）。
    pub config_path: String,
    /// Qoder 凭证目录；空串 = 该产品未启用（对应 Go `resolveAuthDir` 的 `enabled=false`）。
    pub qoder_dir: String,
    /// ZCode 凭证目录；空串 = 该产品未启用。
    pub zcode_dir: String,
    /// Trae 凭证目录；空串 = 该产品未启用。
    ///
    /// ★ Trae 的凭证文件由宿主写入（`trae_accounts.json`），网关只读；这里监听的
    /// 目的与 Qoder / ZCode 相同：宿主刚导入的账号不该要求用户重启网关。
    pub trae_dir: String,
    /// 轮询周期；零值回落 [`DEFAULT_CONFIG_WATCH_INTERVAL`]。
    pub interval: Duration,
}

impl Default for WatchSpec {
    fn default() -> Self {
        Self {
            config_path: String::new(),
            qoder_dir: String::new(),
            zcode_dir: String::new(),
            trae_dir: String::new(),
            interval: DEFAULT_CONFIG_WATCH_INTERVAL,
        }
    }
}

/// 配置监听句柄（与 [`crate::authwatch::AuthDirWatch`] 同形：幂等 `stop` + `Drop`）。
#[derive(Debug)]
pub struct ConfigWatch {
    stop_tx: Option<Sender<()>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl ConfigWatch {
    /// 空句柄：没起任何线程，`stop()` 是空操作。
    pub fn inert() -> Self {
        Self {
            stop_tx: None,
            join: None,
        }
    }

    /// 停止监听并等轮询线程退出（幂等）。
    pub fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }

    /// 是否真的起了轮询线程。
    pub fn is_running(&self) -> bool {
        self.join.is_some()
    }
}

impl Drop for ConfigWatch {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 启动配置监听（对应 Go `startConfigWatch`）。
///
/// ★ 必须在「启动对齐」**之后**调用：基线取的是**调用时刻**的指纹，排在前面的话，
/// 两者之间发生的变化会被算进基线而永远不触发重载（Go `main.go:208-217` 逐字记录了
/// 同一个理由）。
pub fn start_config_watch(
    spec: WatchSpec,
    dynamic: Arc<DynamicState>,
    pool: Arc<Mutex<Pool>>,
) -> ConfigWatch {
    let interval = if spec.interval.is_zero() {
        DEFAULT_CONFIG_WATCH_INTERVAL
    } else {
        spec.interval
    };

    // 基线。★ 配置路径为空时整条关闭 —— 没有可监听的文件，硬跑一个循环只会每 5 秒
    // 白读一次磁盘。
    let watch_cfg = !spec.config_path.is_empty();
    let mut last_cfg = file_stamp(&spec.config_path);
    let qoder_on = !spec.qoder_dir.is_empty();
    let zcode_on = !spec.zcode_dir.is_empty();
    let trae_on = !spec.trae_dir.is_empty();

    if !watch_cfg && !qoder_on && !zcode_on && !trae_on {
        crate::logline!("[watch] 未启用配置监听（无 -config 路径，且三个产品凭证目录都未配置）");
        return ConfigWatch::inert();
    }

    let mut parts: Vec<String> = Vec::new();
    if watch_cfg {
        parts.push("配置".to_string());
    }
    if qoder_on {
        parts.push("Qoder 凭证".to_string());
    }
    if zcode_on {
        parts.push("ZCode 凭证".to_string());
    }
    if trae_on {
        parts.push("Trae 凭证".to_string());
    }
    crate::logline!(
        "[watch] 配置监听已启用（每 {interval:?} 检查一次：{}；模型清单与提示词改动无需重启）",
        parts.join(" / ")
    );

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let cfg_path = spec.config_path.clone();
    let qoder_dir = spec.qoder_dir.clone();
    let zcode_dir = spec.zcode_dir.clone();
    let trae_dir = spec.trae_dir.clone();

    // ★★ 凭证目录基线**在起线程之前**取（与 `last_cfg` 同处），不是在闭包里取。
    //
    // Go `main.go:869-874` 就是这么写的（`lastCreds := map[string]string{…}` 排在
    // `go func(){…}()` 之前）。差别不是风格问题：若在闭包里取，基线取决于「轮询线程
    // 什么时候真正被调度」—— 调用方在 `start_config_watch` 返回后立刻写入的凭证
    // 会被算进基线，那一轮变化就**永远**检测不到。而宿主恰恰就是这么用的：
    // 起网关、随即导入账号。表现为「加了账号要重启」，正是本模块要修的那个缺陷。
    //
    // 取基线用 `dir_fingerprint`：`None` = 不可读（不当作变化），`Some("")` = 空目录。
    // ★ 容量必须与下面的 `specs` 数组等长：两处不同步会让下标越界 panic
    //   （编译期就能挡住的错，故用常量而不是字面量 3 写两遍）。
    let mut last_creds: [Option<String>; CRED_DIR_COUNT] = [
        if qoder_on {
            crate::authwatch::dir_fingerprint(&qoder_dir)
        } else {
            None
        },
        if zcode_on {
            crate::authwatch::dir_fingerprint(&zcode_dir)
        } else {
            None
        },
        if trae_on {
            crate::authwatch::dir_fingerprint(&trae_dir)
        } else {
            None
        },
    ];

    let join = std::thread::Builder::new()
        .name("config-watch".into())
        .spawn(move || {
            loop {
                match rx.recv_timeout(interval) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                    Err(RecvTimeoutError::Timeout) => {}
                }

                // ── ① 配置热重载（product_models / prompt）──
                if watch_cfg {
                    let stamp = file_stamp(&cfg_path);
                    if stamp != last_cfg {
                        match Config::load(&cfg_path) {
                            Err(e) => {
                                // 解析失败**不覆盖**现有配置（Go `main.go:890-892` 逐字）：
                                // 「宿主可能正写到一半（非原子写）。保持旧配置比装载一份
                                // 残缺的好。」
                                //
                                // ★ 刻意**不更新** `last_cfg`：下一轮还会看到同一个指纹，
                                // 于是自动重试 —— 宿主写完就恢复正常，不需要人工干预。
                                crate::logline!(
                                    "[watch] 配置已变化但解析失败（保留当前配置，稍后重试）：{e}"
                                );
                            }
                            Ok(next) => {
                                let dc = DynamicConfig {
                                    product_models: next.product_models.clone(),
                                    platform_enabled: next.platform_enabled.clone().unwrap_or_default(),
                                    // ★ 每日护栏的两个旋钮也进热更新面（缺省 0 = 关闭，
                                    // 老配置行为逐字不变）。
                                    daily_token_cap: next.daily_token_cap,
                                    credit_floor: next.credit_floor,
                                    prompt_mode: next.parsed.prompt_mode.clone(),
                                    prompt_text: next.parsed.prompt_text.clone(),
                                };
                                let n = dc.product_models.len();
                                let off_platforms = dc.platform_enabled.len();

                                // ★ 池里那份「路由提示」也要一起换。
                                //
                                // Go 的热重载路径**没有**这一步（只 `h.ReloadDynamic`），
                                // 于是池的 `productModelSet` 永远停在启动那一刻。那不会
                                // 报错，但会让「新拿到的模型」被当成「该产品不提供」而
                                // 排除掉它的账号 —— 正是 Go `pool.go:1916-1919` 逐字警告
                                // 的症状：「排除一个其实能服务的账号，用户会看到"模型明明
                                // 存在却报不可用"」。这里一并更新，比 Go 更保守地安全。
                                {
                                    let mut p = lock_pool(&pool);
                                    p.set_product_models(&dc.product_models);
                                    // ★ 平台总开关同样要落进池里 —— 池读的就是这份，
                                    // 只更新 `dynamic` 不改池等于开关毫无效果。
                                    p.set_platform_enabled(&dc.platform_enabled);
                                    // ★ 每日护栏：与平台开关同款，只更新 `dynamic` 不改池
                                    // 等于旋钮毫无效果（池的判据读的是自己那份）。
                                    p.set_daily_guard(dc.daily_token_cap, dc.credit_floor);
                                }
                                dynamic.reload(dc);
                                last_cfg = stamp;
                                if n > 0 {
                                    crate::logline!("[watch] 配置热重载：模型清单已更新（{n} 个产品）");
                                } else {
                                    crate::logline!("[watch] 配置热重载：模型清单已更新（空）");
                                }
                                // ★ 被关掉的平台**必须**打出来：用户报「没有账号可用」
                                // 时，这是第一现场 —— 否则得先去翻配置文件才知道。
                                if off_platforms > 0 {
                                    crate::logline!(
                                        "[watch] 配置热重载：{off_platforms} 个平台已关闭「参加兼容网关路由」（养号任务不受影响）"
                                    );
                                }
                            }
                        }
                    }
                }

                // ── ② 多产品凭证热加载（只增不减的 upsert）──
                //
                // ★ `last_creds[i]` **先更新再加载**（Go `main.go:926` 同序）：加载失败
                // 时下一轮不该反复重试同一个坏目录（宿主写入是原子的，同一个指纹连续
                // 出现只可能是「内容没变」）。
                let specs: [(&str, &str, bool, usize, Loader); CRED_DIR_COUNT] = [
                    (
                        "Qoder",
                        &qoder_dir,
                        qoder_on,
                        0,
                        crate::products::qoder::load_credentials,
                    ),
                    (
                        "ZCode",
                        &zcode_dir,
                        zcode_on,
                        1,
                        crate::products::zcode::load_credentials,
                    ),
                    (
                        "Trae",
                        &trae_dir,
                        trae_on,
                        2,
                        crate::products::trae::load_credentials,
                    ),
                ];
                for (label, dir, on, idx, load) in specs {
                    if !on {
                        continue;
                    }
                    // ★ `None`（不可读）**不当作变化**：与 `authwatch` 同一判据。目录正在
                    // 被原子替换时若把「读不到」当成「全变了」，就会白跑一次加载 ——
                    // 这里虽然不剔除账号（不会清空池），但会打出误导性的「重新载入 0 个账号」。
                    let Some(stamp) = crate::authwatch::dir_fingerprint(dir) else {
                        continue;
                    };
                    if Some(stamp.clone()) == last_creds[idx] {
                        continue;
                    }
                    last_creds[idx] = Some(stamp);

                    let (creds, failed) = load(dir);
                    // 解析失败**逐条报出来**（与启动路径同口径）：Qoder/ZCode 的凭证可能
                    // 是用户手写的，格式错误必须让他看见，否则表现为「导入了但没生效」。
                    // ★ Trae 的凭证由宿主写入，同样必须报出来 —— 静默跳过会表现为
                    //   「宿主里明明有这个账号，网关却说没有」。
                    for f in &failed {
                        crate::logline!("{label} 凭证解析失败，已跳过：{f}");
                    }
                    let n = add_all(&pool, creds);
                    crate::logline!("[watch] {label} 凭证热加载：目录已变化，重新载入 {n} 个账号");
                }
            }
        })
        .ok();

    ConfigWatch {
        stop_tx: Some(tx),
        join,
    }
}

/// 产品凭证加载器签名（三个产品同形）。
type Loader = fn(&str) -> (Vec<Auth>, Vec<String>);

/// 被监听的凭证目录数量（Qoder / ZCode / Trae）。
///
/// ★ 只用一处字面量：`last_creds` 的容量与 `specs` 的长度必须永远相等，
///   否则运行期下标越界 panic。写成常量让两者在编译期就绑在一起。
const CRED_DIR_COUNT: usize = 3;

/// 把一批账号 upsert 进池，返回真正进池的条数。
///
/// ★ **不做剔除**（与 [`crate::authwatch`] 的 `sync_to_dir_keeping` 刻意不同）：Go
/// `reloadQoder`/`reloadZcode` 都只 `p.Add`。产品凭证的删除是低频手工动作，而「按目录
/// 全量剔除」需要一份「哪些账号归哪个目录」的映射 —— 池里 `uid` 是派生的
///（ZCode 是 `zcode-<hash>`），漏一个判据就会误删好账号。
fn add_all(pool: &Arc<Mutex<Pool>>, creds: Vec<Auth>) -> usize {
    let mut p = lock_pool(pool);
    let mut n = 0usize;
    for a in creds {
        // `uid` 为空 = 无法进池（池以 uid 为键），与 Go `main.go:459-461` 同款跳过。
        if a.uid.is_empty() {
            continue;
        }
        p.add(a);
        n += 1;
    }
    n
}

/// 文件指纹（对应 Go `fileStamp`，`main.go:949-963`）。
///
/// Go 的 doc 逐字：「只比 mtime 会漏掉"同一秒内改两次"（mtime 精度到秒时），加上大小
/// 能挡住绝大多数漏判；而真正的原子替换（写临时文件再 rename）两种都会变。取不到文件
/// 时返回空串 —— 与"文件不存在"这一态对应。」
///
/// Rust 侧用 `Option<String>` 表达「取不到」（`None`）：调用方只做相等比较，语义等价。
/// 纳秒精度让「同一秒内改两次」在绝大多数文件系统上也能分辨。
fn file_stamp(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let md = std::fs::metadata(path).ok()?;
    Some(format!("{}:{}", mtime_stamp(md.modified().ok()), md.len()))
}

/// 修改时间戳（纳秒精度）。与 [`crate::authwatch`] 的同名函数同口径：指纹只是不透明
/// 比对值，不对外暴露，因此不追求与 Go 的 `RFC3339Nano` 逐字一致。
fn mtime_stamp(t: Option<SystemTime>) -> String {
    match t.and_then(|t| t.duration_since(UNIX_EPOCH).ok()) {
        Some(d) => format!("{}.{:09}", d.as_secs(), d.subsec_nanos()),
        None => "0.000000000".to_string(),
    }
}

/// 取池锁，中毒时沿用内部值（与 `server.rs` / `authwatch.rs` 同一口径）。
fn lock_pool(pool: &Arc<Mutex<Pool>>) -> MutexGuard<'_, Pool> {
    match pool.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "wbswg-cfgwatch-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_file(path: &std::path::Path, text: &str) {
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
        f.flush().unwrap();
    }

    /// 轮询等待（20ms 步进）—— 监听线程是异步的，不能立刻断言。
    fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        f()
    }

    fn fast() -> WatchSpec {
        WatchSpec {
            interval: Duration::from_millis(120),
            ..Default::default()
        }
    }

    // ═══ file_stamp ═══

    #[test]
    fn file_stamp_is_none_for_empty_or_missing() {
        assert_eq!(file_stamp(""), None);
        assert_eq!(file_stamp("D:\\definitely\\not\\here.json"), None);
    }

    #[test]
    fn file_stamp_tracks_content_size() {
        let d = tmpdir("stamp");
        let p = d.join("c.json");
        write_file(&p, "{}");
        let a = file_stamp(&p.to_string_lossy()).unwrap();
        // 内容变长 ⇒ 指纹必变（即使 mtime 精度不够也挡得住）。
        write_file(&p, "{\"x\":1}");
        let b = file_stamp(&p.to_string_lossy()).unwrap();
        assert_ne!(a, b, "大小变了指纹必须变");
        std::fs::remove_dir_all(&d).ok();
    }

    // ═══ DynamicState ═══

    #[test]
    fn dynamic_state_defaults_to_passthrough() {
        let s = DynamicState::new(DynamicConfig::default());
        let snap = s.snapshot();
        assert_eq!(snap.prompt_mode, crate::prompt::MODE_PASSTHROUGH);
        assert!(snap.prompt_text.is_empty());
        assert!(snap.product_models.is_empty());
    }

    #[test]
    fn dynamic_state_reload_replaces_whole_snapshot() {
        let s = DynamicState::new(DynamicConfig {
            product_models: HashMap::from([("qoder".to_string(), vec!["a".to_string()])]),
            // 平台开关也要显式给一份：它是「整份替换」语义最容易漏掉的字段 ——
            // 漏了会表现为「关掉平台后，随便改点别的配置，平台又自己开了」。
            platform_enabled: HashMap::from([("qoder".to_string(), false)]),
            // 每日护栏同理：不显式给就会被「整份替换」清零（表现为「调好的上限自己没了」）。
            daily_token_cap: 4242,
            credit_floor: 7,
            prompt_mode: crate::prompt::MODE_CUSTOM.to_string(),
            prompt_text: "旧".to_string(),
        });
        // 换上一份**不含** prompt、也不含平台开关的新配置 ⇒ 四个字段都要跟着换（整份替换）。
        s.reload(DynamicConfig {
            product_models: HashMap::from([("zcode".to_string(), vec!["b".to_string()])]),
            ..Default::default()
        });
        let snap = s.snapshot();
        assert!(snap.product_models.contains_key("zcode"));
        assert!(!snap.product_models.contains_key("qoder"));
        assert_eq!(snap.prompt_mode, crate::prompt::MODE_PASSTHROUGH);
        assert!(snap.prompt_text.is_empty(), "整份替换：旧提示词不能残留");
        assert!(
            snap.platform_enabled.is_empty(),
            "整份替换：旧的平台开关不能残留（否则关掉的平台会自己开回来）"
        );
        // ★ 每日护栏是**同一类**风险：`reload` 是整份替换，若热重载路径漏了这两个字段，
        // 症状是「调好的上限在改点别的配置之后自己归零」—— 与平台开关那条一模一样，
        // 但更难发现（护栏归零是**放开**流量，不是报错）。
        assert_eq!(snap.daily_token_cap, 0, "整份替换：旧上限不能残留");
        assert_eq!(snap.credit_floor, 0, "整份替换：旧积分下限不能残留");
    }

    #[test]
    fn dynamic_state_snapshot_is_a_shared_pointer_not_a_deep_copy() {
        let s = DynamicState::new(DynamicConfig::default());
        let a = s.snapshot();
        let b = s.snapshot();
        assert!(Arc::ptr_eq(&a, &b), "未 reload 时两次快照必须指向同一份");
        s.reload(DynamicConfig::default());
        let c = s.snapshot();
        assert!(!Arc::ptr_eq(&b, &c), "reload 后必须是新的一份");
    }

    // ═══ ConfigWatch ═══

    #[test]
    fn watch_is_inert_when_nothing_to_watch() {
        let w = start_config_watch(
            fast(),
            DynamicState::new(DynamicConfig::default()),
            Arc::new(Mutex::new(Pool::new(String::new()))),
        );
        assert!(!w.is_running());
    }

    #[test]
    fn watch_reloads_config_into_dynamic_state() {
        let d = tmpdir("reload");
        let cfg = d.join("gateway_native_config.json");
        write_file(&cfg, r#"{"pool":{"product_models":{"qoder":["Qwen3.8-Flash"]}}}"#);

        let dyn_state = DynamicState::new(DynamicConfig::default());
        let pool = Arc::new(Mutex::new(Pool::new(String::new())));
        let mut w = start_config_watch(
            WatchSpec {
                config_path: cfg.to_string_lossy().to_string(),
                ..fast()
            },
            Arc::clone(&dyn_state),
            Arc::clone(&pool),
        );
        assert!(w.is_running());

        // 宿主「刷新账号」后重写配置：加上 zcode 的清单。
        write_file(
            &cfg,
            r#"{"pool":{"product_models":{"qoder":["Qwen3.8-Flash"],"zcode":["GLM-5.3"]}}}"#,
        );

        assert!(
            wait_until(Duration::from_secs(5), || {
                dyn_state.snapshot().product_models.contains_key("zcode")
            }),
            "配置变了但 DynamicState 没更新（/v1/models 的渠道标签会永远缺 zcode）"
        );
        let snap = dyn_state.snapshot();
        assert_eq!(snap.product_models["qoder"], vec!["Qwen3.8-Flash".to_string()]);
        assert_eq!(snap.product_models["zcode"], vec!["GLM-5.3".to_string()]);

        // ★ 池里那份路由提示也必须一起更新，否则新模型会被当成「该产品不提供」。
        let (offers, known) = {
            let p = pool.lock().unwrap();
            p.product_offers_model("zcode", "GLM-5.3")
        };
        assert!(known, "池的清单没更新 ⇒ 会排除其实能服务的账号");
        assert!(offers);

        w.stop();
        assert!(!w.is_running());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn watch_reloads_prompt_mode_and_text() {
        let d = tmpdir("prompt");
        let cfg = d.join("gateway_native_config.json");
        let sysmd = d.join("sys.md");
        write_file(&sysmd, "GATEWAY-SYS-FROM-FILE\n");

        // ★ 起点 = 网关**启动时**读到的配置（passthrough）。监听器刻意**不做**首次
        // 加载 —— 初值由 `main` 通过 `DynamicState::new` 注入（Go 侧是 `NewHandler`
        // 里那句 `h.dynamic.Store(&DynamicConfig{…})`：「可热更新字段的初值：与启动
        // 配置一致」）。所以这里先摆好启动态，再模拟宿主改配置。
        write_file(&cfg, r#"{"prompt":{"mode":"passthrough","file":""}}"#);

        let dyn_state = DynamicState::new(DynamicConfig::default());
        let mut w = start_config_watch(
            WatchSpec {
                config_path: cfg.to_string_lossy().to_string(),
                ..fast()
            },
            Arc::clone(&dyn_state),
            Arc::new(Mutex::new(Pool::new(String::new()))),
        );

        // 用户在界面里改成「自定义提示词 + 文件」 ⇒ 宿主重写配置。
        write_file(
            &cfg,
            &format!(
                r#"{{"prompt":{{"mode":"custom","file":{}}}}}"#,
                serde_json::to_string(&sysmd.to_string_lossy().to_string()).unwrap()
            ),
        );

        assert!(
            wait_until(Duration::from_secs(5), || {
                dyn_state.snapshot().prompt_mode == crate::prompt::MODE_CUSTOM
            }),
            "prompt.mode 热重载没生效（用户改提示词后仍需重启网关）"
        );
        // ★ 必须真的读盘了：只更新 mode 而文本为空的话，`run_forward` 会走
        //   「prompt_text 为空 ⇒ 不改写」的零成本分支，表现为「配了却没生效」。
        assert!(
            wait_until(Duration::from_secs(5), || {
                dyn_state.snapshot().prompt_text.contains("GATEWAY-SYS-FROM-FILE")
            }),
            "prompt_text 没热重载"
        );

        // 改回 passthrough ⇒ 文本必须清空（否则 mode 改了但改写还在跑）。
        write_file(&cfg, r#"{"prompt":{"mode":"passthrough","file":""}}"#);
        assert!(
            wait_until(Duration::from_secs(5), || {
                let s = dyn_state.snapshot();
                s.prompt_mode == crate::prompt::MODE_PASSTHROUGH && s.prompt_text.is_empty()
            }),
            "切回 passthrough 后 prompt_text 必须清空"
        );

        w.stop();
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn watch_keeps_old_config_when_parse_fails() {
        let d = tmpdir("badcfg");
        let cfg = d.join("gateway_native_config.json");
        // 起点：启动时的配置（空清单）—— 同 `watch_reloads_prompt_mode_and_text`，
        // 初值由 `main` 注入，监听器只处理**变化**。
        write_file(&cfg, "{}");

        let dyn_state = DynamicState::new(DynamicConfig::default());
        let mut w = start_config_watch(
            WatchSpec {
                config_path: cfg.to_string_lossy().to_string(),
                ..fast()
            },
            Arc::clone(&dyn_state),
            Arc::new(Mutex::new(Pool::new(String::new()))),
        );

        // 宿主第一次重写：qoder 清单生效。
        write_file(&cfg, r#"{"pool":{"product_models":{"qoder":["m1"]}}}"#);
        assert!(
            wait_until(Duration::from_secs(5), || {
                dyn_state.snapshot().product_models.contains_key("qoder")
            }),
            "首轮热重载就没生效"
        );

        // 宿主正写到一半：写半截 JSON。
        write_file(&cfg, r#"{"pool":{"product_mod"#);
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            dyn_state.snapshot().product_models.contains_key("qoder"),
            "解析失败时**必须保留**旧配置（清空会让模型清单静默消失）"
        );

        // 宿主写完 ⇒ 下一轮自动恢复（因为失败时没更新指纹）。
        write_file(&cfg, r#"{"pool":{"product_models":{"zcode":["m2"]}}}"#);
        assert!(
            wait_until(Duration::from_secs(5), || {
                dyn_state.snapshot().product_models.contains_key("zcode")
            }),
            "失败后不更新指纹 ⇒ 应当自动重试成功"
        );

        w.stop();
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn watch_loads_new_product_credential_without_restart() {
        let d = tmpdir("creds");
        let cfg = d.join("gateway_native_config.json");
        write_file(&cfg, "{}");
        let qdir = d.join("qoder-auths");
        std::fs::create_dir_all(&qdir).unwrap();

        let pool = Arc::new(Mutex::new(Pool::new(String::new())));
        let mut w = start_config_watch(
            WatchSpec {
                config_path: cfg.to_string_lossy().to_string(),
                qoder_dir: qdir.to_string_lossy().to_string(),
                ..fast()
            },
            DynamicState::new(DynamicConfig::default()),
            Arc::clone(&pool),
        );

        // 宿主新写入一个 Qoder 凭证（扁平形，取自 `products::qoder::cred` 的解析器：
        // 文件名须以 `qoder` 开头，且必须有 `token` 或 `refresh_token` 之一 ——
        // 两个都缺会被判「凭证里既没有 accessToken 也没有 refreshToken」而跳过）。
        let cred = qdir.join("qoder-new.json");
        write_file(
            &cred,
            r#"{"token":"dt-x","refresh_token":"drt-x","expires_at_unix":1900000000,"uid":"q-new-1","nickname":"acct"}"#,
        );

        assert!(
            wait_until(Duration::from_secs(5), || {
                let p = pool.lock().unwrap();
                p.all_uids().iter().any(|u| u == "q-new-1")
            }),
            "新 Qoder 凭证没进池（用户看到的正是「加了账号要手动重启」）"
        );

        // ★ 只增不减：删掉凭证文件后账号**仍留在池里**（与 Go `reloadQoder` 同口径）。
        std::fs::remove_file(&cred).ok();
        std::thread::sleep(Duration::from_millis(400));
        let still = {
            let p = pool.lock().unwrap();
            p.all_uids().iter().any(|u| u == "q-new-1")
        };
        assert!(still, "本模块刻意不做剔除（Go 的 reloadQoder 也只 p.Add）");

        w.stop();
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn watch_stop_is_idempotent() {
        let d = tmpdir("stop");
        let cfg = d.join("c.json");
        write_file(&cfg, "{}");
        let mut w = start_config_watch(
            WatchSpec {
                config_path: cfg.to_string_lossy().to_string(),
                ..fast()
            },
            DynamicState::new(DynamicConfig::default()),
            Arc::new(Mutex::new(Pool::new(String::new()))),
        );
        assert!(w.is_running());
        w.stop();
        w.stop(); // 第二次必须是空操作
        assert!(!w.is_running());
        std::fs::remove_dir_all(&d).ok();
    }
}
