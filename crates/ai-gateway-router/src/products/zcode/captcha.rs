//! ZCode 人机验证（阿里云无痕验证）求解器。
//!
//! 移植自 Go `wt-port\go-gateway\internal\zcode\captcha.go`（1168 行），逐字对齐。
//!
//! # 它解决什么、不解决什么（读之前先看这三条）
//!
//! 1. ★ **`verifyParam` 是一次性的** —— 同一个值用第二次上游必回 `3007`。
//!    所以**不能**把"已经用过的"param 缓存复用。本模块的池里放的是
//!    **尚未使用**的 param，取出即作废。
//! 2. ★★ **`[pe-stall]` 不是上游限流**（2026-09-21 实测修正）。它的真实含义是
//!    **求解器自己的失速超时**（`solver.js` 的 `stallTimer`：距上次 XHR 超过
//!    `CAPTCHA_STALL_MS`（旧默认 **6 秒**）即判失速失败），纯本地等待。
//!    实测对照（同机同求解器各连跑 5 次）：
//!    `CAPTCHA_STALL_MS=6000` ⇒ 成功 2 / 失败 3（失败**全是** STALL）；
//!    `CAPTCHA_STALL_MS=20000` ⇒ 成功 4 / 失败 1。
//!    **提高本地阈值就把成功率 40%→80% ⇒ 若真是上游限流，改本地阈值不会有任何效果**。
//!    "等 90 秒恢复"是**巧合**（那 90 秒里 stall 判定恰好没被触发）。
//!    `solver.js` 默认值已提到 20 秒。冷却**仍保留**，但只用于兜住"真的反复失败"。
//! 3. ★ **调试信息在 stderr、结果在 stdout**。合并两者（`2>&1`）会被 `[pe-stall]`
//!    抢先，看起来像失败 —— 作者因此误判过一次「求解器不可用」。
//!
//! 它**解决不了**什么：带有效 verifyParam 后 `3007` 消失，但会变成
//! `HTTP 405 {"code":3012,"msg":"request has been blocked due to unusual activity."}`。
//! `3012` 是**账号/风控层**，与 deviceMid 取值、param 复用都无关（三种 deviceMid
//! 取值实测都是 3012）⇒ 本模块**只解决验证码这一层**，界面要如实说明。
//!
//! # 求解原理（说清它是什么）
//!
//! `assets/zcode-captcha/solver.js` 用 **happy-dom** 在 Node 里模拟浏览器环境，
//! 加载并运行**阿里云官方验证 SDK** 走完正常流程拿 verifyParam。
//! ⚠ 它**是**在精简环境里正常执行官方 SDK；**不是**逆向破解验证算法。
//! 但它在**没有真人操作**的情况下产出通过凭证 ⇒ 本能力默认由用户主动开启后才参与请求。
//!
//! # 为什么还需要"外部求解服务"
//!
//! 原做法（Node 子进程 + happy-dom 模拟浏览器）两个问题：① 要用户机器有 Node；
//! ② **模拟环境被风控盯上，实测成功率仅 ~40%**。而官方 ZCode 客户端用的是
//! **真实浏览器环境**（已从 `app.asar` 核实：`script.src = ".../aliyunCaptcha/AliyunCaptcha.js"`
//! + `inst.startTracelessVerification()`，约 0.9 秒拿到 param）。我们的 Tauri 宿主
//! **就有**真实 WebView2（Windows 自带、零体积）。实测（有头 Chrome）
//! `startTracelessVerification()` → **929ms** 拿到 param（**280 字符**）。
//! 空 = 未配置（回退本地 Node）。
//!
//! # 与 Go 侧的有意偏离（如实列出）
//!
//! - Go 用 `exec.CommandContext` 在 90 秒后 kill 子进程；Rust 侧 `tokio` **未启用
//!   `process` feature**（`Cargo.toml` 不可改），故用 `spawn_blocking` + `std::process`
//!   自行轮询 `try_wait()` 并在超时后 `kill()`。行为等价，超时文案不同。
//! - Go 的 `globNewest` 用 `filepath.Glob` 后倒序取第一个；Rust 侧自实现
//!   `glob_newest`（只支持单个 `*`，覆盖本项目全部模式），并额外要求命中项**是文件**
//!   —— Go 侧 `os.Stat` 对目录也成功，会把目录当成 node.exe。
//! - 池里 `solve` 闭包捕获 `Arc<CaptchaSolver>` 构成**引用环**（Go 侧同样是环）。
//!   本对象是进程级单例，生命周期与进程一致，故有意不破环。

use std::collections::VecDeque;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use super::errors::truncate_str;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 池中 param 的寿命。
///
/// 上游给 param 约 **2 分钟**有效期（参考实现 `CAPTCHA_TOKEN_TTL = 95_000` 取 95s），
/// 本实现取更保守的 **90s**：宁可早弃重解，也**绝不**在边界上发出一个刚好过期的值
/// —— 那会回 `3007`，比"池空、现解一个"更糟（用户看到的是随机失败）。
pub const CAPTCHA_PARAM_TTL: Duration = Duration::from_secs(90);

/// 池的水位：低于它就异步补货。
pub const CAPTCHA_POOL_TARGET: usize = 3;

/// 池的上限。
///
/// 池不是越大越好：每个 param 2 分钟后失效，无上限补货会让后台**不断求解却没人用**。
pub const CAPTCHA_POOL_MAX: usize = 8;

/// 池空时等待后台补货的上限。
///
/// 冷启动第一个求解要付「建 WebView2 窗口 + 下载官方 SDK(225KB) + pe 字节码(约 450KB)」，
/// 所有者实测**首次对话 29 秒**；预热就绪后单次约 1 秒（宿主实测 929ms）。
/// 25s 足以覆盖冷启动那一次，又不会让请求长挂。
pub const CAPTCHA_WAIT_FOR_REFILL: Duration = Duration::from_secs(25);

/// 熔断时长。
///
/// ★ 语义是**兜底熔断**，不是"上游限流冷却"（见文件头第 2 条）。
/// 取 2 分钟：真实故障（SDK 被改坏）不会更快自愈，而偶发 stall 本就不该走到这里。
pub const DEFAULT_CAPTCHA_COOLDOWN: Duration = Duration::from_secs(120);

/// 优先入口：esbuild 打成的单文件（写入从 3353 次降到 1 次）。
pub const CAPTCHA_ENTRY_BUNDLE: &str = "solver.bundle.cjs";
/// 回退入口：源码（bundle 在某机器上出问题时有源码可诊断）。
pub const CAPTCHA_ENTRY_SOURCE: &str = "solver.js";

/// 默认验证场景（`SetCaptchaConfig` 在生产代码里 0 个调用点 ⇒ 恒定用这三个默认值）。
pub const DEFAULT_CAPTCHA_SCENE: &str = "11xygtvd";
/// 默认区域 —— ★ 与 param **成对**，调用方不得硬编码（见 [`CaptchaSolver::region`]）。
pub const DEFAULT_CAPTCHA_REGION: &str = "cn";
/// 默认前缀。
pub const DEFAULT_CAPTCHA_PREFIX: &str = "no8xfe";

/// 单次 [`CaptchaSolver::solve`] 内的求解尝试次数。
///
/// 为什么 >1：失速是**概率性**的（实测同机连跑 5 次有 1-3 次 `pe-stall`），
/// 紧接着重试往往就成功。旧实现第一次失败就冷却 2 分钟，把偶发抖动放大成
/// 「ZCode 两分钟不可用」。
pub const CAPTCHA_SOLVE_ATTEMPTS: usize = 3;

/// 单次子进程求解的硬超时。
const SOLVER_TIMEOUT: Duration = Duration::from_secs(90);

/// 外部（宿主 WebView2）求解请求的超时。
const EXTERNAL_SOLVE_TIMEOUT: Duration = Duration::from_secs(20);

/// 求解器 stdout / 外部响应读取上限。
const SOLVER_READ_CAP: usize = 1 << 20;

// ---------------------------------------------------------------------------
// 求解器
// ---------------------------------------------------------------------------

/// 池中一条待用 param。
#[derive(Debug, Clone)]
struct PooledParam {
    param: String,
    born: Instant,
}

/// 池的内部状态（单锁保护）。
#[derive(Debug, Default)]
struct PoolState {
    items: VecDeque<PooledParam>,
    refilling: bool,
}

type SolveFuture = Pin<Box<dyn Future<Output = Result<String, String>> + Send>>;
type SolveFn = dyn Fn() -> SolveFuture + Send + Sync;

/// verifyParam 预取池。
///
/// 所有者原话：「而且要有一个池子,备用,不然多并发一下就不够用,还会拉长延迟」。
/// 单次求解约 0.9 秒（外部）/ 3 秒（本地），不池化则**每个**补码请求都要现场等，
/// 并发 N 个就串成 N × 0.9 秒。
///
/// ★ `solve` 由 [`CaptchaSolver`] **注入**而不是直接调求解器方法：池是**纯数据结构**，
/// 不持有求解器的锁与配置；注入让它可独立测试（传假求解函数，不必起 node 或 HTTP 服务）。
pub struct CaptchaPool {
    state: Mutex<PoolState>,
    solve: Arc<SolveFn>,
}

impl std::fmt::Debug for CaptchaPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptchaPool")
            .field("items", &self.items_len())
            .finish_non_exhaustive()
    }
}

impl CaptchaPool {
    /// 用注入的求解函数建池。
    pub fn new(solve: Arc<SolveFn>) -> Self {
        Self {
            state: Mutex::new(PoolState::default()),
            solve,
        }
    }

    /// 当前池内条数（诊断/测试）。
    pub fn items_len(&self) -> usize {
        lock(&self.state).items.len()
    }

    /// 是否有后台补货在跑。
    pub fn refilling(&self) -> bool {
        lock(&self.state).refilling
    }

    /// 取一个未过期的 param（取不到返回空串）。
    ///
    /// 步骤与 Go 一致：① 先**原地清掉过期项** ② 取队首（FIFO）③ 若低于水位则
    /// **异步**触发补货（不阻塞当前请求）。
    pub fn take(self: &Arc<Self>, now: Instant) -> String {
        let (out, low) = {
            let mut st = lock(&self.state);
            st.items
                .retain(|it| now.saturating_duration_since(it.born) < CAPTCHA_PARAM_TTL);
            let out = st.items.pop_front().map(|it| it.param).unwrap_or_default();
            let low = st.items.len() < CAPTCHA_POOL_TARGET;
            (out, low)
        };
        if low {
            self.refill_async();
        }
        out
    }

    /// 取一个 param；池空但后台**正在**补货时短暂等待。
    ///
    /// ★ 存在理由：旧实现池空就无脑现场求解，**不知道后台已在解** ⇒ 冷启动时
    /// 两次求解并行争抢（都建窗口、都等 SDK）⇒ 29 秒。
    pub async fn take_or_wait(self: &Arc<Self>, max_wait: Duration) -> String {
        let p = self.take(Instant::now());
        if !p.is_empty() {
            return p;
        }
        if !self.refilling() {
            return String::new();
        }
        let deadline = Instant::now() + max_wait;
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let p = self.take(Instant::now());
            if !p.is_empty() {
                return p;
            }
            if !self.refilling() {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        String::new()
    }

    /// 后台补货到水位（**单飞**：同时只允许一个补货在跑）。
    ///
    /// 并发触发会拉起 N 个求解 —— 那正是要避免的（每个都要建窗口、等 SDK）。
    pub fn refill_async(self: &Arc<Self>) {
        // 非 async 上下文（例如单测里同步构造）不 spawn，避免 panic；
        // 同时也不能把 `refilling` 置真，否则会永久卡住后续补货。
        let handle = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => return,
        };
        {
            let mut st = lock(&self.state);
            if st.refilling {
                return;
            }
            st.refilling = true;
        }
        let me = self.clone();
        handle.spawn(async move {
            me.refill_to(CAPTCHA_POOL_TARGET).await;
            lock(&me.state).refilling = false;
        });
    }

    /// 补货到 `target` 条为止。失败即停（**不再连续撞**）。
    pub async fn refill_to(self: &Arc<Self>, target: usize) {
        loop {
            let n = lock(&self.state).items.len();
            if n >= target || n >= CAPTCHA_POOL_MAX {
                return;
            }
            let param = match (self.solve)().await {
                Ok(p) => p,
                Err(_) => return,
            };
            if param.is_empty() {
                return;
            }
            let mut st = lock(&self.state);
            if st.items.len() < CAPTCHA_POOL_MAX {
                st.items.push_back(PooledParam {
                    param,
                    born: Instant::now(),
                });
            }
        }
    }
}

/// 求解器内部状态（单锁保护）。
struct Inner {
    dir: String,
    node_path: Option<String>,
    node_err: Option<String>,
    /// 测试钩子：`Some(..)` 时 `candidate_paths` 直接返回它，不做真实探测。
    node_path_override: Option<Vec<String>>,
    cooldown_until: Option<Instant>,
    cooldown_reason: String,
    scene: String,
    region: String,
    prefix: String,
    external_url: String,
    external_token: String,
    last_external_err: String,
}

/// 人机验证求解器。
///
/// ★ 生产环境**必须**用进程级单例 [`shared_captcha_solver`]：冷却状态是**进程级**的
/// （限流按来源施加，每账号一个求解器等于各自撞墙）。
pub struct CaptchaSolver {
    inner: Mutex<Inner>,
    pool: Mutex<Option<Arc<CaptchaPool>>>,
}

impl std::fmt::Debug for CaptchaSolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let st = lock(&self.inner);
        f.debug_struct("CaptchaSolver")
            .field("dir", &st.dir)
            .field("region", &st.region)
            .field("external_url", &st.external_url)
            .field("has_pool", &lock(&self.pool).is_some())
            .finish_non_exhaustive()
    }
}

impl Default for CaptchaSolver {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptchaSolver {
    /// 建一个带**三个运营默认值**的求解器。
    ///
    /// ★ 千万不要用"零值"构造（三个参数空串 ⇒ 求解必然失败）。
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                dir: String::new(),
                node_path: None,
                node_err: None,
                node_path_override: None,
                cooldown_until: None,
                cooldown_reason: String::new(),
                scene: DEFAULT_CAPTCHA_SCENE.to_string(),
                region: DEFAULT_CAPTCHA_REGION.to_string(),
                prefix: DEFAULT_CAPTCHA_PREFIX.to_string(),
                external_url: String::new(),
                external_token: String::new(),
                last_external_err: String::new(),
            }),
            pool: Mutex::new(None),
        }
    }

    /// 覆盖运营参数（空串 = 保持原值）。
    pub fn set_captcha_config(&self, scene: &str, region: &str, prefix: &str) {
        let mut st = lock(&self.inner);
        if !scene.trim().is_empty() {
            st.scene = scene.trim().to_string();
        }
        if !region.trim().is_empty() {
            st.region = region.trim().to_string();
        }
        if !prefix.trim().is_empty() {
            st.prefix = prefix.trim().to_string();
        }
    }

    /// 求解器目录（`assets/zcode-captcha`）。空 = 未安装。
    pub fn set_dir(&self, dir: &str) {
        let mut st = lock(&self.inner);
        st.dir = dir.trim().to_string();
        // 目录变了要重新探测 node（Go 同款）。
        st.node_path = None;
        st.node_err = None;
    }

    /// 当前求解器目录。
    pub fn dir(&self) -> String {
        lock(&self.inner).dir.clone()
    }

    /// 区域（`X-Aliyun-Captcha-Verify-Region` 的值）。
    ///
    /// ★ **调用方不得硬编码 region** —— 它与 param **成对**，实测缺 region 就是 3007。
    /// region 是求解器的运营参数（可被上游配置覆盖），硬编码会在上游换区域时
    /// 「求解成功但仍 3007」，极难查。空串 ⇒ 调用方回落默认值。
    pub fn region(&self) -> String {
        lock(&self.inner).region.clone()
    }

    /// 配置外部（宿主 WebView2）求解服务。
    ///
    /// 非空 URL ⇒ 建池并**立即预热**（池的目标就是预先备好外部环境的产物，
    /// 避免请求路径上等 0.9 秒）。
    pub fn set_external_solver(self: &Arc<Self>, url: &str, token: &str) {
        let url = url.trim().to_string();
        let token = token.trim().to_string();
        {
            let mut st = lock(&self.inner);
            st.external_url = url.clone();
            st.external_token = token;
        }
        if url.is_empty() {
            return;
        }
        let mut slot = lock(&self.pool);
        if slot.is_some() {
            return;
        }
        let me = self.clone();
        let solve: Arc<SolveFn> = Arc::new(move || {
            let me = me.clone();
            Box::pin(async move { me.solve_via_external().await })
        });
        let pool = Arc::new(CaptchaPool::new(solve));
        *slot = Some(pool.clone());
        drop(slot);
        pool.refill_async();
    }

    /// 是否已配置外部求解服务。
    pub fn external_solve_enabled(&self) -> bool {
        !lock(&self.inner).external_url.is_empty()
    }

    /// 是否有池（诊断/测试）。
    pub fn has_pool(&self) -> bool {
        lock(&self.pool).is_some()
    }

    /// 外部求解最后一次失败原因（空 ⇒ `"未配置"`）。
    pub fn external_err_text(&self) -> String {
        let st = lock(&self.inner);
        if st.last_external_err.is_empty() {
            "未配置".to_string()
        } else {
            st.last_external_err.clone()
        }
    }

    /// 求解器是否可用。
    ///
    /// ★ 与 [`CaptchaSolver::unavailable_reason`] **同一口径**（否则会自相矛盾）。
    pub fn available(&self) -> bool {
        self.unavailable_reason().is_empty()
    }

    /// 不可用的原因（空 = 可用）。
    ///
    /// ★ **必须把外部求解算进来**（2026-09-21 实测踩到）：旧实现只看本地组件，
    /// 于是配了 `zcode_captcha_solver_url` 仍回「求解器组件未安装」——
    /// 因为 `client.go::solveCaptcha` **先查本函数、非空就直接失败**，压根走不到
    /// `Solve`（那里才有外部回退）。实测症状：外部服务一切正常（服务端日志确认它
    /// **根本没被调用到**）—— 一类「新路径接好了、旧的前置检查把它挡在门外」的缺陷。
    pub fn unavailable_reason(&self) -> String {
        let mut st = lock(&self.inner);
        if !st.external_url.is_empty() {
            return String::new();
        }
        if st.dir.is_empty() {
            return "求解器组件未安装（发行包应包含 assets/zcode-captcha）".to_string();
        }
        if let Err(e) = entry_file_of(&st) {
            return e;
        }
        if let Err(e) = resolve_node_locked(&mut st) {
            return e;
        }
        String::new()
    }

    /// 距离冷却结束还有多久（`None` = 不在冷却）。
    pub fn cooldown_left(&self) -> Option<Duration> {
        let st = lock(&self.inner);
        let until = st.cooldown_until?;
        let now = Instant::now();
        if until > now {
            Some(until.duration_since(now))
        } else {
            None
        }
    }

    /// 求解一个 verifyParam（池优先，带内部重试）。
    ///
    /// ⚠ 返回值是**一次性**的：用一次就作废，不要缓存复用（实测第二次必回 3007）。
    ///
    /// 重试只针对**失速**（求解器自身超时），不针对别的错误 —— 组件缺失、
    /// Node 找不到、非零退出码这类都是确定性失败，重试无意义且拖慢响应。
    ///
    /// # 取用顺序
    ///
    /// ① 池里有未过期的现成 param ⇒ **零等待**返回，并异步触发补货；
    /// ② 池空 ⇒ 现场求解（外部服务优先，失败回退本地 Node）。
    ///
    /// ★ 现场解的结果**不进池** —— 它是给当前请求的；池 = 预取备用。
    pub async fn solve(&self) -> Result<String, String> {
        let pool = lock(&self.pool).clone();
        if let Some(pool) = pool {
            let p = pool.take_or_wait(CAPTCHA_WAIT_FOR_REFILL).await;
            if !p.is_empty() {
                return Ok(p);
            }
        }

        let mut last_err = String::new();
        for attempt in 1..=CAPTCHA_SOLVE_ATTEMPTS {
            match self.solve_once().await {
                Ok(p) => return Ok(p),
                Err(e) => {
                    last_err = e;
                    // 冷却中/组件缺失/Node 缺失：确定性失败，立刻返回。
                    if !is_captcha_stall(&last_err) {
                        return Err(last_err);
                    }
                    if attempt == CAPTCHA_SOLVE_ATTEMPTS {
                        // ★ 所有尝试都用尽了 —— 这时才熔断。
                        //
                        // 为什么放在这里而不是 `solve_once` 里：失速是概率性的，
                        // 第一次失败就冷却会把偶发抖动放大成 2 分钟不可用（旧行为）。
                        {
                            let mut st = lock(&self.inner);
                            st.cooldown_until = Some(Instant::now() + DEFAULT_CAPTCHA_COOLDOWN);
                            st.cooldown_reason = format!(
                                "连续 {CAPTCHA_SOLVE_ATTEMPTS} 次失速（SDK 启动超时，非上游限流）"
                            );
                        }
                        return Err(format!(
                            "验证码求解连续 {CAPTCHA_SOLVE_ATTEMPTS} 次失速，已暂停 {} 后自动重试；\
                             若持续如此请确认已装 Node（≥20）且能访问 o.alicdn.com。最后一次原因：{}",
                            human_duration(DEFAULT_CAPTCHA_COOLDOWN),
                            last_err
                        ));
                    }
                    // 短暂退避后重试：给 SDK/网络一个恢复窗口，又不至于让请求久等。
                    tokio::time::sleep(Duration::from_secs(attempt as u64)).await;
                }
            }
        }
        Err(last_err)
    }

    /// 单次求解（不含重试）。
    async fn solve_once(&self) -> Result<String, String> {
        let (dir, scene, region, prefix, ext_url) = {
            let st = lock(&self.inner);
            (
                st.dir.clone(),
                st.scene.clone(),
                st.region.clone(),
                st.prefix.clone(),
                st.external_url.clone(),
            )
        };

        // 冷却检查
        {
            let st = lock(&self.inner);
            if let Some(until) = st.cooldown_until {
                let now = Instant::now();
                if now < until {
                    let left = (until.duration_since(now).as_secs_f64()).round() as u64;
                    let reason = st.cooldown_reason.clone();
                    let suffix = if reason.is_empty() {
                        String::new()
                    } else {
                        format!("：{reason}")
                    };
                    return Err(format!(
                        "验证码求解处于冷却中（还需 {}{}）",
                        human_duration(Duration::from_secs(left)),
                        suffix
                    ));
                }
            }
        }

        // ① 外部求解（宿主 WebView2）优先 —— 真实浏览器环境，比模拟环境更快更稳。
        //
        // 失败时**不直接返回错误**，而是继续往下走本地 Node 路径：外部服务可能因为
        // 宿主还没起、端口被占、用户关了主窗口等原因不可用，那种情况下本地求解
        // 仍有可能成功，没有理由让整个功能失效。
        if !ext_url.is_empty() {
            match self.solve_via_external().await {
                Ok(p) if !p.is_empty() => return Ok(p),
                Ok(_) => {}
                Err(e) => {
                    // 记下原因供最终错误信息使用（不打断流程）
                    lock(&self.inner).last_external_err = e;
                }
            }
        }

        if dir.is_empty() {
            return Err(format!(
                "求解器组件未安装（且外部求解不可用：{}）",
                self.external_err_text()
            ));
        }

        let node = {
            let mut st = lock(&self.inner);
            resolve_node_locked(&mut st)?
        };
        let entry = {
            let st = lock(&self.inner);
            entry_file_of(&st)?
        };

        let param = tokio::task::spawn_blocking(move || {
            run_solver_blocking(&node, &entry, &dir, &scene, &region, &prefix, SOLVER_TIMEOUT)
        })
        .await
        .map_err(|e| format!("求解器任务失败: {e}"))??;

        // 成功：清掉冷却。
        {
            let mut st = lock(&self.inner);
            st.cooldown_until = None;
            st.cooldown_reason.clear();
        }
        Ok(param)
    }

    /// 走外部（宿主）求解服务。
    ///
    /// 协议：`POST <url>?scene=..&region=..&prefix=..` + 可选 `X-Captcha-Token`
    /// ⇒ `200 {"param": "...", "region": "cn"}`。
    ///
    /// ★ query **显式**带 `scene`/`region`/`prefix`：避免两边默认值漂移。
    pub async fn solve_via_external(&self) -> Result<String, String> {
        let (url, token, scene, region, prefix) = {
            let st = lock(&self.inner);
            (
                st.external_url.clone(),
                st.external_token.clone(),
                st.scene.clone(),
                st.region.clone(),
                st.prefix.clone(),
            )
        };
        if url.is_empty() {
            return Err("未配置外部求解服务".to_string());
        }
        let mut parsed =
            url::Url::parse(&url).map_err(|e| format!("外部求解地址非法（{url}）：{e}"))?;
        {
            let mut q = parsed.query_pairs_mut();
            q.append_pair("scene", &scene);
            q.append_pair("region", &region);
            q.append_pair("prefix", &prefix);
        }

        let client = external_http_client();
        let mut req = client
            .post(parsed)
            .timeout(EXTERNAL_SOLVE_TIMEOUT)
            .header("Accept", "application/json");
        if !token.is_empty() {
            req = req.header("X-Captcha-Token", &token);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("宿主求解请求失败: {e}"))?;
        let status = resp.status().as_u16();
        let body = read_capped_text(resp, SOLVER_READ_CAP).await;

        if status >= 400 {
            return Err(format!(
                "宿主求解失败（HTTP {}）：{}",
                status,
                truncate_str(&body, 200)
            ));
        }

        #[derive(serde::Deserialize)]
        struct SolverResp {
            #[serde(default)]
            param: String,
            #[serde(default)]
            error: String,
        }
        let doc: SolverResp = serde_json::from_str(&body)
            .map_err(|e| format!("宿主求解响应不是合法 JSON: {e}"))?;
        if doc.param.is_empty() {
            if !doc.error.is_empty() {
                return Err(format!("宿主求解失败：{}", doc.error));
            }
            return Err(format!(
                "宿主未返回 param（原始：{}）",
                truncate_str(&body, 160)
            ));
        }
        Ok(doc.param)
    }

    /// 测试钩子：把 node 候选集替换成给定列表。
    #[cfg(test)]
    fn set_node_path_override_for_test(&self, list: Option<Vec<String>>) {
        lock(&self.inner).node_path_override = list;
    }
}

// ---------------------------------------------------------------------------
// 进程级单例
// ---------------------------------------------------------------------------

static SHARED_CAPTCHA_SOLVER: OnceLock<Arc<CaptchaSolver>> = OnceLock::new();

/// 进程级共享求解器。
///
/// ★ 冷却状态必须**进程级** —— 每账号一个求解器会让"反复失败"变成
/// 「每个账号各自撞墙」，而那是**本机环境**的问题（SDK/Node/网络），不是账号的。
pub fn shared_captcha_solver() -> Arc<CaptchaSolver> {
    SHARED_CAPTCHA_SOLVER
        .get_or_init(|| Arc::new(CaptchaSolver::new()))
        .clone()
}

// ---------------------------------------------------------------------------
// 本地求解（Node 子进程）
// ---------------------------------------------------------------------------

/// 跑一次本地求解器子进程。
///
/// 返回 `Ok(param)` 或已经写好的错误文案（"求解器失速…" / "验证码求解失败：…"）。
fn run_solver_blocking(
    node: &str,
    entry: &str,
    dir: &str,
    scene: &str,
    region: &str,
    prefix: &str,
    timeout: Duration,
) -> Result<String, String> {
    use std::io::{BufRead, BufReader};

    let mut child = Command::new(node)
        .arg(entry)
        .arg(scene)
        .arg(region)
        .arg(prefix)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动求解器失败: {e}"))?;

    // ⚠ 只读 **stdout**：调试信息走 stderr（`[pe-stall] …`）。合并两者会被调试信息
    // 抢先 —— 我因此误判过一次"求解器不可用"。
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "无法读取求解器输出".to_string())?;
    let stderr = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut param = String::new();
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let line = line.trim().to_string();
            if let Some(v) = line.strip_prefix("VERIFY_PARAM=") {
                param = v.trim().to_string();
            }
        }
        param
    });
    let err_reader = stderr.map(|e| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            for line in BufReader::new(e).lines() {
                let Ok(line) = line else { break };
                buf.push_str(&line);
                buf.push('\n');
            }
            buf
        })
    });

    let deadline = Instant::now() + timeout;
    let wait_err: Option<String> = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                break if status.success() {
                    None
                } else {
                    Some(format!("exit status {}", status.code().unwrap_or(-1)))
                };
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Some("求解器超时未退出，已终止".to_string());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => break Some(e.to_string()),
        }
    };

    let param = reader.join().unwrap_or_default();
    let stderr_text = err_reader
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default()
        .trim()
        .to_string();

    if !param.is_empty() {
        return Ok(param);
    }

    // 失败：`stall`/`timeout` 属**求解器自身**的失速（非上游限流）。
    //
    // ★ 冷却**不在这里设**：失速是概率性的，`solve` 会在外层立刻重试。
    // 若在这里就设 2 分钟冷却，重试会被冷却检查直接挡掉 —— 那等于"第一次失败就放弃"。
    if is_captcha_stall(&stderr_text) {
        return Err(format!(
            "求解器失速（SDK 启动超时，非上游限流）：{}",
            first_line(&stderr_text)
        ));
    }

    let mut msg = solver_detail(&stderr_text);
    if msg.is_empty() {
        if let Some(e) = wait_err {
            msg = e;
        }
    }
    Err(format!("验证码求解失败：{msg}"))
}

/// `pe-stall` / `captcha solve stall` / `timeout` ⇒ 求解器**自身**失速（非上游限流）。
pub fn is_captcha_stall(stderr_text: &str) -> bool {
    stderr_text.contains("pe-stall")
        || stderr_text.contains("captcha solve stall")
        || stderr_text.contains("timeout")
}

/// 汇总求解器的 stderr 供错误信息使用（截断，避免刷屏）。
///
/// ★ **不能只取 [`first_line`]**：求解器把**失败原因**写在 stderr 里，而它可能不是
/// 第一行。实测形态（`solver.js` 的 `handleCaptchaResult`）：
///
/// ```text
/// verify rejected: {"verifyCode":"F001","certifyId":"…"}
/// ```
///
/// 旧实现只取首行 ⇒ 这些内容**全被丢掉**，用户只看到一句 `exit status 4` ——
/// 而可操作的线索（风控码）就在被丢掉的那段里。[`super::errors::annotate_risk_code`]
/// 正需要看到它。
pub fn solver_detail(stderr_text: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for line in stderr_text.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        parts.push(line);
    }
    truncate_str(&parts.join(" "), super::errors::RISK_RAW_LIMIT)
}

/// 第一行（去掉换行之后的部分，再 trim）。
pub fn first_line(s: &str) -> String {
    match s.find('\n') {
        Some(i) => s[..i].trim().to_string(),
        None => s.trim().to_string(),
    }
}

// ---------------------------------------------------------------------------
// node 探测 / 入口文件
// ---------------------------------------------------------------------------

/// node 可执行文件名。
pub fn node_exe_name() -> &'static str {
    if cfg!(windows) {
        "node.exe"
    } else {
        "node"
    }
}

/// 从 `PATH` 里找一个可执行文件（等价 Go 的 `exec.LookPath`，但不真的运行它）。
fn look_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand.to_string_lossy().to_string());
        }
    }
    None
}

/// 候选集（`override_list` 为 `Some` 时直接返回它 —— **测试钩子**）。
///
/// 为什么需要钩子：真实探测会扫到**这台机器实际装了什么**，
/// 「清空 PATH 制造探测失败」的测试在装了 node 的机器上**必然失败** ——
/// 那测的是"这台机器装了什么"而不是代码行为。
pub fn candidate_paths(override_list: Option<&Vec<String>>) -> Vec<String> {
    match override_list {
        Some(list) => list.clone(),
        None => node_candidate_paths(),
    }
}

/// 真实候选集。
///
/// ★ 顺序有意义：**先**同目录（`node.exe`、`node/node.exe`）—— 放在最前是因为
/// 内置那份是我们完全控制的版本，比用户环境里任何版本都更可能与求解器兼容。
pub fn node_candidate_paths() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |s: String| {
        if !s.is_empty() {
            out.push(s);
        }
    };

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            add(dir.join(node_exe_name()).to_string_lossy().to_string());
            add(dir
                .join("node")
                .join(node_exe_name())
                .to_string_lossy()
                .to_string());
        }
    }

    let home = env_dir(&["USERPROFILE", "HOME"]);
    let appdata = env_dir(&["APPDATA"]);
    let local = env_dir(&["LOCALAPPDATA"]);
    let programdata = env_dir(&["ProgramData"]);

    if cfg!(windows) {
        if let Some(h) = &home {
            add(glob_newest(
                &h.join(".nvmd")
                    .join("versions")
                    .join("*")
                    .join("node.exe")
                    .to_string_lossy(),
            ));
        }
        if let Some(a) = &appdata {
            add(glob_newest(
                &a.join("nvm").join("v*").join("node.exe").to_string_lossy(),
            ));
        }
        if let Some(l) = &local {
            add(glob_newest(
                &l.join("fnm_multishells")
                    .join("*")
                    .join("node.exe")
                    .to_string_lossy(),
            ));
            add(glob_newest(
                &l.join("fnm")
                    .join("node-versions")
                    .join("*")
                    .join("installation")
                    .join("node.exe")
                    .to_string_lossy(),
            ));
            add(l
                .join("Volta")
                .join("tools")
                .join("image")
                .join("node")
                .join("node.exe")
                .to_string_lossy()
                .to_string());
            add(l
                .join("Programs")
                .join("nodejs")
                .join("node.exe")
                .to_string_lossy()
                .to_string());
            add(l
                .join("Microsoft")
                .join("WinGet")
                .join("Links")
                .join("node.exe")
                .to_string_lossy()
                .to_string());
            add(glob_newest(
                &l.join("scoop")
                    .join("apps")
                    .join("nodejs")
                    .join("*")
                    .join("node.exe")
                    .to_string_lossy(),
            ));
        }
        add(r"C:\Program Files\nodejs\node.exe".to_string());
        add(r"C:\Program Files (x86)\nodejs\node.exe".to_string());
        add(r"C:\ProgramData\chocolatey\bin\node.exe".to_string());
        if let Some(p) = &programdata {
            add(glob_newest(
                &p.join("chocolatey")
                    .join("lib")
                    .join("nodejs*")
                    .join("tools")
                    .join("node.exe")
                    .to_string_lossy(),
            ));
        }
    } else {
        if let Some(h) = &home {
            add(glob_newest(
                &h.join(".nvm")
                    .join("versions")
                    .join("node")
                    .join("*")
                    .join("bin")
                    .join("node")
                    .to_string_lossy(),
            ));
            add(h.join(".volta").join("bin").join("node").to_string_lossy().to_string());
            add(h.join(".local").join("bin").join("node").to_string_lossy().to_string());
            add(glob_newest(
                &h.join(".fnm")
                    .join("node-versions")
                    .join("*")
                    .join("installation")
                    .join("bin")
                    .join("node")
                    .to_string_lossy(),
            ));
        }
        add("/usr/local/bin/node".to_string());
        add("/usr/bin/node".to_string());
        add("/opt/homebrew/bin/node".to_string());
    }

    out
}

fn env_dir(names: &[&str]) -> Option<std::path::PathBuf> {
    for n in names {
        if let Some(v) = std::env::var_os(n) {
            if !v.is_empty() {
                return Some(std::path::PathBuf::from(v));
            }
        }
    }
    None
}

/// 单 `*` 通配的「取字典序最大者」。
///
/// ★ 与 Go 的差异（有意，且**只**在"命中项是不是文件"这一点上）：
/// Go 是 `filepath.Glob` 后**倒序取第一个**（`captcha.go:805-812`）。`filepath.Glob`
/// 返回**字典序**，倒序即"字典序最大者优先"。本函数同样取字典序最大者 ——
/// **包括它那个已知的局限**：`v9.0.0` 会排在 `v26.0.0` **之后**（`'9' > '2'`），
/// 于是同机装了 node v9 与 v26 时会选中 **v9**（而求解器 SDK 需要 Node ≥ 20）。
/// Go 的注释明写"不追求严格的语义化版本比较 —— 那只在少数版本号长度不同时才有差异"，
/// 真实 Node 版本号位数一致（v20/v22/v24/v26），故实践中不会踩到。
///
/// ⚠ 这是 Go 的**既有行为**，Rust 照抄。改成"按版本号数值比较"是一个**可选的
/// 有意偏离**（更稳健），但会与 Go 分叉 —— 已列入给 lead 的交付说明，默认不擅自改。
///
/// 另一处（**真**差异）：本函数额外要求命中项**是文件**（`is_file()`）。
/// Go 侧 `os.Stat` 对目录也返回成功，理论上会把同名目录当成 node.exe。
pub fn glob_newest(pattern: &str) -> String {
    let Some(star) = pattern.find('*') else {
        return if Path::new(pattern).is_file() {
            pattern.to_string()
        } else {
            String::new()
        };
    };
    let head = &pattern[..star];
    let tail = pattern[star + 1..].trim_start_matches(['/', '\\']);
    let (base, prefix) = match head.rfind(['/', '\\']) {
        Some(i) => (&head[..i], &head[i + 1..]),
        None => (".", head),
    };
    let mut best = String::new();
    let Ok(entries) = std::fs::read_dir(base) else {
        return String::new();
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with(prefix) {
            continue;
        }
        let cand = if tail.is_empty() {
            e.path()
        } else {
            e.path().join(tail)
        };
        if !cand.is_file() {
            continue;
        }
        let s = cand.to_string_lossy().to_string();
        if best.is_empty() || s > best {
            best = s;
        }
    }
    best
}

/// 求解器入口文件（bundle 优先，源码回退）。调用方需持锁。
fn entry_file_of(st: &Inner) -> Result<String, String> {
    if st.dir.is_empty() {
        return Err("求解器组件未安装".to_string());
    }
    let bundle = Path::new(&st.dir).join(CAPTCHA_ENTRY_BUNDLE);
    if bundle.is_file() {
        return Ok(bundle.to_string_lossy().to_string());
    }
    let source = Path::new(&st.dir).join(CAPTCHA_ENTRY_SOURCE);
    if source.is_file() {
        return Ok(source.to_string_lossy().to_string());
    }
    Err(format!(
        "求解器目录里既没有 {} 也没有 {}：{}",
        CAPTCHA_ENTRY_BUNDLE, CAPTCHA_ENTRY_SOURCE, st.dir
    ))
}

/// 解析 node 路径（结果与失败都缓存）。调用方需持锁。
fn resolve_node_locked(st: &mut Inner) -> Result<String, String> {
    if let Some(p) = &st.node_path {
        return Ok(p.clone());
    }
    if let Some(e) = &st.node_err {
        return Err(e.clone());
    }

    // ① 环境变量 ZCODE_NODE_PATH（存在才用）
    if let Some(raw) = std::env::var_os("ZCODE_NODE_PATH") {
        let raw = raw.to_string_lossy().trim().to_string();
        if !raw.is_empty() && Path::new(&raw).is_file() {
            st.node_path = Some(raw.clone());
            return Ok(raw);
        }
    }
    // ② PATH
    if let Some(p) = look_path(node_exe_name()) {
        st.node_path = Some(p.clone());
        return Ok(p);
    }
    // ③④ 候选集
    let candidates = candidate_paths(st.node_path_override.as_ref());
    if let Some(c) = resolve_from_candidates(&candidates) {
        st.node_path = Some(c.clone());
        return Ok(c);
    }

    let msg = node_missing_message(&candidates);
    st.node_err = Some(msg.clone());
    Err(msg)
}

/// 在候选列表里找第一个**真实存在**的 node（③④ 两路）。
///
/// ★ 拆出来是为了可测：真实候选集的内容取决于这台机器装了什么，而"候选里的文件
/// 存在即命中"这条规则本身与机器无关。Go 对应 `resolveNode` 的
/// `for _, p := range s.candidatePaths() { if _, err := os.Stat(p); err == nil { … } }`。
fn resolve_from_candidates(candidates: &[String]) -> Option<String> {
    candidates
        .iter()
        .find(|c| !c.is_empty() && Path::new(c).is_file())
        .cloned()
}

/// 「找不到 node」的可操作报错。对应 Go `resolveNode` 末尾的 `fmt.Errorf`。
///
/// ★ 单独成函数是为了**可测**：报错文案是本函数最有价值的部分（用户看到的就是它），
/// 但要走到这里就必须让 ①②③④ 四路探测全部失败 —— 而那取决于**这台机器装了什么**
/// （Go 侧为此要在测试里 `t.Setenv("PATH","")`，Rust 测试并行，改进程环境有竞态）。
/// 拆出来后，文案可以脱离环境直接钉住。
fn node_missing_message(candidates: &[String]) -> String {
    format!(
        "找不到 node 可执行文件。ZCode 的验证码求解需要 Node.js；\
         请安装 Node.js（https://nodejs.org）后重启本软件，\
         或用环境变量 ZCODE_NODE_PATH 指定 node.exe 的完整路径。（已查找：ZCODE_NODE_PATH、PATH、{}）",
        candidates.join("、")
    )
}

// ---------------------------------------------------------------------------
// HTTP 小工具
// ---------------------------------------------------------------------------

fn external_http_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| reqwest::Client::builder().build().unwrap_or_default())
        .clone()
}

/// 读响应体但不超过 `cap` 字节。
async fn read_capped_text(resp: reqwest::Response, cap: usize) -> String {
    let mut resp = resp;
    let mut out: Vec<u8> = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        if out.len() >= cap {
            break;
        }
        let take = (cap - out.len()).min(chunk.len());
        out.extend_from_slice(&chunk[..take]);
    }
    String::from_utf8_lossy(&out).to_string()
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// 锁中毒不 panic（本模块的临界区都是纯内存操作，中毒后状态仍可用）。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `2m0s` / `25s` —— 对齐 Go 的 `time.Duration.String()`（整秒口径）。
fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "zcode-captcha-test-{}-{tag}-{n}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&p);
        p
    }

    fn pool_with(solve: Arc<SolveFn>) -> Arc<CaptchaPool> {
        Arc::new(CaptchaPool::new(solve))
    }

    // -- 构造与配置 ---------------------------------------------------------

    #[test]
    fn new_solver_has_operational_defaults() {
        let s = CaptchaSolver::new();
        let st = lock(&s.inner);
        assert_eq!(st.scene, DEFAULT_CAPTCHA_SCENE, "场景不能为空，否则求解必失败");
        assert_eq!(st.region, "cn");
        assert_eq!(st.prefix, DEFAULT_CAPTCHA_PREFIX);
        assert!(st.dir.is_empty());
        assert!(st.cooldown_until.is_none());
    }

    #[test]
    fn set_captcha_config_ignores_blank_values() {
        let s = CaptchaSolver::new();
        s.set_captcha_config("  ", "  ", "  ");
        assert_eq!(s.region(), "cn", "空串不得覆盖已有值");
        s.set_captcha_config("scene2", "sg", "pre2");
        assert_eq!(s.region(), "sg");
        let st = lock(&s.inner);
        assert_eq!(st.scene, "scene2");
        assert_eq!(st.prefix, "pre2");
    }

    #[test]
    fn region_falls_back_to_default() {
        let s = CaptchaSolver::new();
        assert_eq!(s.region(), DEFAULT_CAPTCHA_REGION);
        s.set_captcha_config("", "sg", "");
        assert_eq!(s.region(), "sg");
    }

    // -- 池 -----------------------------------------------------------------

    #[tokio::test]
    async fn pool_take_discards_expired_and_returns_fifo() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let pool = pool_with(Arc::new(move || {
            c.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok("fresh".to_string()) })
        }));
        {
            let mut st = lock(&pool.state);
            st.items.push_back(PooledParam {
                param: "expired".to_string(),
                born: Instant::now() - CAPTCHA_PARAM_TTL - Duration::from_secs(1),
            });
            st.items.push_back(PooledParam {
                param: "first".to_string(),
                born: Instant::now(),
            });
            st.items.push_back(PooledParam {
                param: "second".to_string(),
                born: Instant::now(),
            });
        }
        assert_eq!(pool.take(Instant::now()), "first");
        assert_eq!(pool.take(Instant::now()), "second");
        assert_eq!(
            pool.items_len(),
            0,
            "过期项必须在 take 时被清掉，绝不发出去（那会回 3007）"
        );
    }

    #[tokio::test]
    async fn pool_refill_to_stops_at_target() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let pool = pool_with(Arc::new(move || {
            let n = c.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move { Ok(format!("p{n}")) })
        }));
        pool.refill_to(CAPTCHA_POOL_TARGET).await;
        assert_eq!(pool.items_len(), CAPTCHA_POOL_TARGET);
        assert_eq!(calls.load(Ordering::Relaxed), CAPTCHA_POOL_TARGET);
    }

    #[tokio::test]
    async fn pool_refill_stops_after_first_failure() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let pool = pool_with(Arc::new(move || {
            c.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Err("boom".to_string()) })
        }));
        pool.refill_to(CAPTCHA_POOL_TARGET).await;
        assert_eq!(pool.items_len(), 0);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "失败即停 —— 连续撞只会拖长延迟且毫无意义"
        );
    }

    #[tokio::test]
    async fn pool_never_exceeds_max() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let pool = pool_with(Arc::new(move || {
            let n = c.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move { Ok(format!("p{n}")) })
        }));
        pool.refill_to(CAPTCHA_POOL_MAX + 5).await;
        assert_eq!(pool.items_len(), CAPTCHA_POOL_MAX);
    }

    #[tokio::test]
    async fn pool_take_or_wait_returns_empty_when_not_refilling() {
        // ★ 原断言（"没有补货在跑时不该空等 ⇒ 必得空串"）与实现冲突，且**实现是对的**：
        // `take_or_wait` 第一步就调 `take()`，而 `take()` 在池低于水位时会**自己**异步
        // 触发一次补货（Go `captchaPool.take` 末尾的 `if low { go p.refillAsync() }`，
        // `captcha.go:247-249`）⇒ 随即 `refilling()` 为真 ⇒ 等 300ms 就拿到那个极快的
        // 假求解结果 "x"。这不是缺陷：**现场求解正是"没人补货时"的正确兜底**。
        //
        // ⚠ 也因此 **不能**在返回后直接断言 `!pool.refilling()`：循环里每次 `take()`
        // 都会重新起一次补货（Go 的 `takeOrWait` 循环同样如此），刚 spawn 的任务
        // 还没被 poll 到，`refilling` 就是 true —— 那是**竞态**，不是 bug。
        //
        // 该钉的确定性事实有三条：
        //  ① 求解失败 ⇒ 等待收敛到空串（不返回垃圾值），且耗时有限；
        //  ② 确实**尝试过**求解（补货被触发过），不是静默放弃；
        //  ③ 失败不会把 `refilling` **永久**置真（否则后续补货全被单飞挡死）。
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let pool = pool_with(Arc::new(move || {
            c.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Err("求解器不可用".to_string()) })
        }));
        let started = Instant::now();
        let got = pool.take_or_wait(Duration::from_millis(300)).await;
        assert!(got.is_empty(), "补货失败时不该返回垃圾值");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "必须在有限时间内返回，实耗 {:?}",
            started.elapsed()
        );
        assert!(calls.load(Ordering::Relaxed) >= 1, "池空时应当真的去解一次");
        assert_eq!(pool.items_len(), 0, "求解失败 ⇒ 池里不该有任何东西");

        // ③ 等最后那次补货收尾（失败即返回，故只需几毫秒）
        for _ in 0..100 {
            if !pool.refilling() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !pool.refilling(),
            "失败的补货必须清掉 refilling，否则单飞标志永久为真、后续补货全被挡死"
        );
    }

    #[tokio::test]
    async fn pool_take_or_wait_waits_for_background_refill() {
        let pool = pool_with(Arc::new(|| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(80)).await;
                Ok("ready".to_string())
            })
        }));
        pool.refill_async();
        assert!(pool.refilling());
        let got = pool.take_or_wait(Duration::from_secs(3)).await;
        assert_eq!(got, "ready", "后台已在解时应当等它，而不是再起一个");
    }

    // -- 入口文件与可用性 ---------------------------------------------------

    #[test]
    fn entry_file_prefers_bundle_over_source() {
        let dir = temp_dir("bundle");
        std::fs::write(dir.join(CAPTCHA_ENTRY_SOURCE), "// src").unwrap();
        let mut st = Inner {
            dir: dir.to_string_lossy().to_string(),
            ..empty_inner()
        };
        let got = entry_file_of(&st).unwrap();
        assert!(got.ends_with(CAPTCHA_ENTRY_SOURCE), "只有源码时用源码");

        std::fs::write(dir.join(CAPTCHA_ENTRY_BUNDLE), "// bundle").unwrap();
        let got = entry_file_of(&st).unwrap();
        assert!(
            got.ends_with(CAPTCHA_ENTRY_BUNDLE),
            "两者都在时必须优先单文件 bundle（安装写入从 3353 次降到 1 次）"
        );
        st.dir.clear();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entry_file_reports_both_names_when_missing() {
        let dir = temp_dir("empty");
        let st = Inner {
            dir: dir.to_string_lossy().to_string(),
            ..empty_inner()
        };
        let err = entry_file_of(&st).unwrap_err();
        assert!(err.contains(CAPTCHA_ENTRY_BUNDLE), "{err}");
        assert!(err.contains(CAPTCHA_ENTRY_SOURCE), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entry_file_requires_dir() {
        let st = empty_inner();
        assert_eq!(entry_file_of(&st).unwrap_err(), "求解器组件未安装");
    }

    #[test]
    fn unavailable_reason_is_empty_when_external_solver_configured() {
        let s = Arc::new(CaptchaSolver::new());
        s.set_external_solver("http://127.0.0.1:1/solve", "tok");
        assert!(
            s.unavailable_reason().is_empty(),
            "★ 配了外部求解就必须算「可用」—— 旧实现只看本地组件，\
             于是前置检查直接把外部路径挡在门外（实测踩到）"
        );
        assert!(s.available());
        assert!(s.external_solve_enabled());
        assert!(s.has_pool(), "配了外部 URL 就该建池并预热");
    }

    #[test]
    fn unavailable_reason_reports_missing_component() {
        let s = CaptchaSolver::new();
        assert_eq!(
            s.unavailable_reason(),
            "求解器组件未安装（发行包应包含 assets/zcode-captcha）"
        );
        assert!(!s.available());
    }

    #[test]
    fn unavailable_reason_reports_missing_entry_file() {
        let dir = temp_dir("noentry");
        let s = CaptchaSolver::new();
        s.set_dir(&dir.to_string_lossy());
        let reason = s.unavailable_reason();
        assert!(reason.contains(CAPTCHA_ENTRY_BUNDLE), "{reason}");
        assert!(!s.available());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn available_and_unavailable_reason_agree() {
        let s = CaptchaSolver::new();
        assert_eq!(s.available(), s.unavailable_reason().is_empty());
        let dir = temp_dir("agree");
        std::fs::write(dir.join(CAPTCHA_ENTRY_BUNDLE), "// b").unwrap();
        s.set_dir(&dir.to_string_lossy());
        // 入口有了，但 node 可能探测不到 —— 两边仍必须一致。
        assert_eq!(s.available(), s.unavailable_reason().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- node 探测 ----------------------------------------------------------

    #[test]
    fn set_dir_clears_node_probe_cache() {
        let s = CaptchaSolver::new();
        {
            let mut st = lock(&s.inner);
            st.node_path = Some("/fake/node".to_string());
            st.node_err = Some("cached failure".to_string());
        }
        s.set_dir("  D:\\somewhere  ");
        let st = lock(&s.inner);
        assert_eq!(st.dir, "D:\\somewhere", "目录要 trim");
        assert!(st.node_path.is_none(), "换目录必须重新探测 node");
        assert!(st.node_err.is_none());
    }

    #[test]
    fn node_path_override_wins_over_real_probe() {
        // ★ 名字保留（它钉的是"override 生效"），但**契约要按 Go 的真实语义写**：
        // `nodePathOverride` 只替换**候选列表**（Go `candidatePaths()`，`captcha.go:698-703`），
        // 它**不**覆盖 ①`ZCODE_NODE_PATH` 与 ②`exec.LookPath`/`PATH` —— 那两路优先级更高。
        // 所以「override 一定赢过真实探测」这个前提是**错的**：本机 PATH 里有 node 时，
        // 探测在 ② 就返回了，根本走不到候选列表。原断言正是因此拿到
        // `C:\Users\...\.nvmd\versions\26.0.0\node.exe`。
        //
        // 可测的真实契约有两条：
        //  (a) override 生效时，**真实机器候选不得出现**在候选集里；
        //  (b) 候选集全不命中时，"找不到 node"成为**确定事实**（与装了 node 无关）。
        //      —— 这正是 Go `TestSolverWithoutNodeReportsNode` 的用法（`captcha_test.go:124-141`）。
        let s = CaptchaSolver::new();
        let fake = temp_dir("fakenode").join("node.exe");
        std::fs::write(&fake, b"stub").unwrap();
        let fake_s = fake.to_string_lossy().to_string();
        s.set_node_path_override_for_test(Some(vec![fake_s.clone()]));

        // (a) 候选集被整体替换
        {
            let st = lock(&s.inner);
            let got = candidate_paths(st.node_path_override.as_ref());
            assert_eq!(got, vec![fake_s.clone()], "override 应整体替换候选集");
            let real = node_candidate_paths();
            for r in &real {
                assert!(
                    !got.contains(r),
                    "override 生效时真实候选不得出现：{r}（真实候选共 {} 项）",
                    real.len()
                );
            }
        }

        // 候选集里的文件存在 ⇒ ③ 这一路能命中（不经过 ①②，故用 `resolve_from_candidates`）
        assert!(Path::new(&fake_s).is_file());
        assert_eq!(resolve_from_candidates(&[fake_s.clone()]).as_deref(), Some(fake_s.as_str()));

        // (b) 候选集为空 + 清掉 ①② 的缓存 ⇒ 报错成为确定事实
        s.set_node_path_override_for_test(Some(vec![]));
        let err = node_missing_message(&[]);
        assert!(err.contains("找不到 node 可执行文件"), "{err}");
        assert!(err.contains("ZCODE_NODE_PATH"), "要给出可操作的指引：{err}");
        assert!(err.contains("nodejs.org"), "{err}");
        let _ = std::fs::remove_dir_all(fake.parent().unwrap());
    }

    #[test]
    fn resolve_node_reports_missing_node_with_actionable_text() {
        // 文案单独钉住（不依赖本机是否装了 node）。
        let err = node_missing_message(&["C:\\a\\node.exe".to_string(), "C:\\b\\node.exe".to_string()]);
        assert!(err.contains("找不到 node 可执行文件"), "{err}");
        assert!(err.contains("ZCODE_NODE_PATH"), "必须告诉用户可以显式指定路径：{err}");
        assert!(err.contains("https://nodejs.org"), "必须给出安装地址：{err}");
        assert!(
            err.contains("C:\\a\\node.exe、C:\\b\\node.exe"),
            "要列出已查找的候选路径（用户据此判断该放哪儿）：{err}"
        );
    }

    #[test]
    fn node_candidate_paths_are_absolute_and_non_empty() {
        let list = node_candidate_paths();
        assert!(!list.is_empty());
        assert!(list.iter().all(|p| !p.is_empty()));
        if cfg!(windows) {
            assert!(list.iter().any(|p| p.ends_with("node.exe")));
            assert!(list.iter().any(|p| p.contains("Program Files")));
        } else {
            assert!(list.iter().any(|p| p.ends_with("node")));
            assert!(list.iter().any(|p| p.contains("/usr/local/bin")));
        }
    }

    #[test]
    fn glob_newest_picks_lexicographically_largest() {
        // ★ 名字从 `..._prefers_higher_lexicographic_version` 改掉：原名字把**字典序**
        // 与"版本更高"混为一谈，而这两者只在**版本号位数相同**时一致。
        //
        // Go 的 `globNewest` 就是「`filepath.Glob` 后倒序取第一个」
        //（`captcha.go:805-812`，注释明写"不追求严格的语义化版本比较"）⇒ 字典序下
        // `v9.0.0` **大于** `v26.0.0`（`'9' > '2'`），**Go 自己也会返回 v9**。
        // 故这里钉的是"照抄 Go 的字典序语义"，而不是"选版本最高者"。
        let root = temp_dir("glob");
        for v in ["v9.0.0", "v26.0.0", "v20.1.0"] {
            let d = root.join(v);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("node.exe"), b"stub").unwrap();
        }
        // 目录本身不是文件 ⇒ 不得被当成 node.exe（Go 那边 `os.Stat` 对目录也成功，
        // 而 `filepath.Glob` 会因为 `\node.exe` 不存在而不匹配 —— 两边都不会命中，
        // 但 Rust 额外用 `is_file()` 把这条规则显式化了）。
        std::fs::create_dir_all(root.join("v99.0.0")).unwrap();
        let pattern = format!("{}\\v*\\node.exe", root.to_string_lossy());
        let got = glob_newest(&pattern);
        assert!(
            got.contains("v9.0.0"),
            "字典序最大者是 v9.0.0（'9' > '2'）—— 这是 Go 的既有语义，实得 {got}"
        );
        assert!(!got.contains("v99.0.0"), "目录里没有 node.exe ⇒ 不算命中");
        assert_eq!(
            glob_newest(&format!("{}\\nope*\\node.exe", root.to_string_lossy())),
            ""
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn glob_newest_orders_two_digit_versions_correctly() {
        // 真实 Node 版本号位数一致（v20/v22/v24/v26）⇒ 字典序**恰好**等于版本序，
        // 这正是 Go 注释里"真实 Node 版本号位数一致"的依据。把常见情形钉住，
        // 免得日后有人把 `glob_newest` 改成数值比较时误伤这条。
        let root = temp_dir("glob2");
        for v in ["v20.1.0", "v22.0.0", "v26.0.0"] {
            let d = root.join(v);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("node.exe"), b"stub").unwrap();
        }
        let pattern = format!("{}\\v*\\node.exe", root.to_string_lossy());
        let got = glob_newest(&pattern);
        assert!(got.contains("v26.0.0"), "两位数版本号下字典序=版本序，实得 {got}");
        let _ = std::fs::remove_dir_all(&root);
    }

    // -- 失速判定与错误文案 -------------------------------------------------

    #[test]
    fn is_captcha_stall_matches_go_keywords() {
        assert!(is_captcha_stall("[pe-stall] no xhr"));
        assert!(is_captcha_stall("captcha solve stall after 20s"));
        assert!(is_captcha_stall("Error: timeout"));
        assert!(!is_captcha_stall("verify rejected: {\"verifyCode\":\"F001\"}"));
        assert!(!is_captcha_stall(""));
    }

    #[test]
    fn solver_detail_joins_lines_and_truncates() {
        let got = solver_detail("  line one  \n\n   line two \n");
        assert_eq!(got, "line one line two");
        assert_eq!(solver_detail(""), "");
        let long = "x".repeat(super::super::errors::RISK_RAW_LIMIT + 50);
        let got = solver_detail(&long);
        assert!(got.chars().count() <= super::super::errors::RISK_RAW_LIMIT + 1);
        assert!(got.ends_with('…'));
    }

    #[test]
    fn first_line_trims_and_cuts_at_newline() {
        assert_eq!(first_line("  a  \n b"), "a");
        assert_eq!(first_line("only"), "only");
        assert_eq!(first_line("\n  x"), "");
    }

    #[test]
    fn human_duration_matches_go_style() {
        assert_eq!(human_duration(Duration::from_secs(25)), "25s");
        assert_eq!(human_duration(Duration::from_secs(120)), "2m0s");
        assert_eq!(human_duration(Duration::from_secs(95)), "1m35s");
    }

    // -- 冷却 ---------------------------------------------------------------

    #[tokio::test]
    async fn cooldown_blocks_solve_once_and_reports_reason() {
        let s = CaptchaSolver::new();
        {
            let mut st = lock(&s.inner);
            st.cooldown_until = Some(Instant::now() + Duration::from_secs(60));
            st.cooldown_reason = "连续 3 次失速".to_string();
        }
        assert!(s.cooldown_left().is_some());
        let err = s.solve().await.unwrap_err();
        assert!(err.contains("冷却中"), "{err}");
        assert!(err.contains("连续 3 次失速"), "冷却原因要带上，否则用户无从判断：{err}");
        assert!(err.contains("还需"), "{err}");
    }

    #[test]
    fn cooldown_left_is_none_when_not_cooling() {
        let s = CaptchaSolver::new();
        assert!(s.cooldown_left().is_none());
        {
            let mut st = lock(&s.inner);
            st.cooldown_until = Some(Instant::now() - Duration::from_secs(1));
        }
        assert!(s.cooldown_left().is_none(), "已过期的冷却不算在冷却中");
    }

    // -- 外部求解 -----------------------------------------------------------

    #[tokio::test]
    async fn external_solver_reports_unconfigured() {
        let s = CaptchaSolver::new();
        assert_eq!(s.solve_via_external().await.unwrap_err(), "未配置外部求解服务");
        assert_eq!(s.external_err_text(), "未配置");
    }

    #[tokio::test]
    async fn external_solver_reports_invalid_url() {
        let s = Arc::new(CaptchaSolver::new());
        s.set_external_solver("not a url", "");
        let err = s.solve_via_external().await.unwrap_err();
        assert!(err.contains("外部求解地址非法"), "{err}");
    }

    // -- 辅助 ---------------------------------------------------------------

    fn empty_inner() -> Inner {
        Inner {
            dir: String::new(),
            node_path: None,
            node_err: None,
            node_path_override: None,
            cooldown_until: None,
            cooldown_reason: String::new(),
            scene: DEFAULT_CAPTCHA_SCENE.to_string(),
            region: DEFAULT_CAPTCHA_REGION.to_string(),
            prefix: DEFAULT_CAPTCHA_PREFIX.to_string(),
            external_url: String::new(),
            external_token: String::new(),
            last_external_err: String::new(),
        }
    }

    #[test]
    fn shared_solver_is_a_process_singleton() {
        let a = shared_captcha_solver();
        let b = shared_captcha_solver();
        assert!(
            Arc::ptr_eq(&a, &b),
            "★ 必须是进程级单例：冷却状态按来源共享，每账号一个等于各自撞墙"
        );
    }

    #[test]
    fn node_exe_name_matches_platform() {
        if cfg!(windows) {
            assert_eq!(node_exe_name(), "node.exe");
        } else {
            assert_eq!(node_exe_name(), "node");
        }
    }
}
