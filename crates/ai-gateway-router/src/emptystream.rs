//! emptystream.rs 「上游整体空流」的风暴识别与熔断。
//!
//! 逐字移植 Go 侧没有对应物 —— 这是 Rust 侧针对 **2026-10-11 现场**新增的
//! 防线（与 [`crate::wafip`] 同一套「多账号并发故障 ⇒ 换号无用」的判定哲学）。
//!
//! # 要解决的问题（真实缺陷，2026-10-11 02:17 现场）
//!
//! 国际区 `deepseek-v4.1-flash-sg` 上游整体故障：HTTP 200 但流里一帧有效数据
//! 都没有（空流）。网关的既有逻辑是「空流 ⇒ 冷却换号」，于是：
//!
//! ```text
//! 30 行「chat uid=… 上游返回空流（无有效数据帧） — 冷却换号」
//!   = 6 波（客户端 5 路并发重试 × 1 轮转）× 5 个账号
//!   = 13 个国际区账号全部被轮了一遍、全部喂进熔断
//!   ⇒ 最终 503 no_healthy_account，对话死亡，且没有任何账号是真的坏了
//! ```
//!
//! 换号对「上游整体故障」**本来就没用**，而网关的轮转把每一个好账号都
//! 打了一遍还把它们全部冷却 —— 放大了上游的故障，也拖死了用户重试的
//! 5 路并发请求。
//!
//! # 判定与出路
//!
//! 与 [`crate::wafip`] 同款判据：**窗口内多个不同账号**在**同一
//! （产品, 区域）**上吃到同一种上游故障（空流），只能是它们共同的东西出了
//! 问题 —— 那条上游通道本身。此时继续换号无用，应当**中止轮转**并把结论
//! 讲给客户端（「稍等重试」），而不是 `no_healthy_account`（它诱导客户端
//! 立即重试，恰好再放大一轮）。
//!
//! ★ 账号**不被惩罚**：与 3012 / WAF 同理，账号是好的，罚号只会让用户在
//! 上游恢复之后发现号还被自己人停着。这里只喂本模块的窗口，不碰账号池。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// 滑动窗口长度：只看最近这段时间内的空流。
///
/// 60 秒的依据与 [`crate::wafip::WAF_IP_WINDOW`] 相同：一次请求的整个轮转
///（含客户端并发重试）在秒级完成，「多个账号几乎同时空流」这一特征只在
/// 这个尺度内成立。窗口再长会把不相关的偶发空流凑成一次误判。
pub const EMPTY_STREAM_WINDOW: Duration = Duration::from_secs(60);

/// 窗口内**不同账号**在同一（产品, 区域）上空流的数量达到它即判定上游整体故障。
///
/// 取 3：1 个账号的空流可能只是它自己的问题（token 过期、代理闪断），
/// 2 个仍有偶发凑巧的空间（现场 6 波重试里任意两波撞上就够了）；
/// 3 个**不同**账号在 60 秒内接连空流，基本只剩「上游整体故障」一种解释。
///
/// ★★ 这是**上限**，不是固定阈值：实际阈值由 [`effective_threshold`] 按该通道的
/// 账号总数夹小。此前这里是一刀切的常量，注释还把它当成「刻意设计」——
///
/// > ★ 副作用是刻意设计：账号总数少于 3 的小池子**永远触发不了**本判定
///
/// 那个「刻意」是错的。qoder 只有 **2 个账号**，于是 qoder 通道上的风暴熔断
/// **永久失效**：现场（2026-10-20 07:34）1 分钟内 23 次「上游返回空流 ⇒ 换号 ⇒
/// 再空流」，每次都以「轮转耗尽（本请求已试 2 个账号）」收尾，客户端一路 503。
/// 「换号本来就是把每个号试一遍，没有可省的轮转」这个理由只在小池子**健康**时成立；
/// 上游整体故障时，把每个号都试一遍正是要避免的事（每次尝试都加深上游风控）。
pub const EMPTY_STREAM_THRESHOLD: usize = 3;

/// 按通道账号总数把阈值夹到「不超过账号数」。
///
/// - `accounts >= EMPTY_STREAM_THRESHOLD` ⇒ 用常量（大池子行为不变）；
/// - `accounts <  EMPTY_STREAM_THRESHOLD` ⇒ 用 `accounts`，但**至少 2**。
///
/// 下界 2 的理由：阈值 1 会让**单个**账号的一次偶发空流立刻熔断整条通道 ——
/// 那正是本模块开头警告的「单号问题被误判成上游整体故障」。两个不同账号在同一
/// 窗口内都空流，才排除了「单个账号自己的问题」。
///
/// `accounts == 0`（池子里没有该通道的账号）时返回 2：那种情况下根本选不到号，
/// 轮转会在别处失败，不该由这里给出「上游故障」的错误结论。
pub fn effective_threshold(accounts: usize) -> usize {
    if accounts == 0 {
        return 2;
    }
    accounts.clamp(2, EMPTY_STREAM_THRESHOLD)
}

/// 空流风暴的分桶键里「区域」那一段取什么值。
///
/// # 为什么不能一律用账号自带区域（2026-10-20 现场）
///
/// Qoder 的两个账号分属 `qoder.com.cn`（国服）与 `qoder.sh`（国际版），而
/// `qoder:模型名` 的选号**不分区域**（`Region::Any`，一次轮转可以换到另一个
/// 区域的号）。若按账号区域分桶，两个账号会落进**两个各只有 1 个账号的桶** ——
/// 而判据是「窗口内不同账号数」，下界又是 2，于是两边都永远凑不满：现场
/// 08:36 的 8 次「空流 ⇒ 换号 ⇒ 再空流」里熔断一次都没生效。
///
/// ⇒ 桶必须与**轮转范围**一致：
///
/// - 产品前缀请求（`qoder:...`）用**请求的区域**；未指定时是 `any`，即整产品；
/// - 裸名请求的轮转范围就是账号区域，保持原样（WorkBuddy 的国服/国际版是
///   两条不同上游，不能互相连坐）。
pub fn channel_region<'a>(
    is_product_request: bool,
    request_region: &'a str,
    account_region: &'a str,
) -> &'a str {
    if is_product_request {
        request_region
    } else {
        account_region
    }
}

/// 时钟注入点：生产用 [`Instant::now`]，单测注入假时钟以断言「窗口过期」。
///
/// 与 [`crate::wafip`] 的 `Clock` 同构（那边是私有类型，这边单独一份；
/// 两份合计不到 20 行，抽公共模块反而把两个无依赖的判定绑在一起）。
#[derive(Clone)]
struct Clock(Arc<dyn Fn() -> Instant + Send + Sync>);

impl Clock {
    fn system() -> Self {
        Clock(Arc::new(Instant::now))
    }

    fn now(&self) -> Instant {
        (self.0)()
    }
}

impl std::fmt::Debug for Clock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<clock>")
    }
}

/// 取锁并**忽略中毒**（与 [`crate::wafip`] 同款：断言失败不该连锁放大）。
fn lock_or_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 「上游整体空流」的滑动窗口状态。
///
/// 并发安全（`Mutex`）：现场正是**同一时刻**的 5 路并发重试各自独立地吃到
/// 空流，只有进程级共享的计数才能把它们凑成一次判定。
#[derive(Debug)]
pub struct EmptyStreamWindow {
    inner: Mutex<Inner>,
    /// 时钟注入点；生产用 [`Instant::now`]，单测注入假时钟。
    now: Clock,
}

#[derive(Debug)]
struct Inner {
    window: Duration,
    threshold: usize,
    /// (产品, 区域) → 「uid → 最近一次空流时刻」。
    ///
    /// 判据要求空流发生在**同一条上游通道**上，故按 (产品, 区域) 分桶：
    /// 国际区的整体故障不得连坐国服的账号（两者是不同的上游域名）。
    /// 内层 map 的 key 天然去重，「不同账号数」就等于 `hits.len()`。
    hits: HashMap<(String, String), HashMap<String, Instant>>,
}

impl Default for EmptyStreamWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl EmptyStreamWindow {
    /// 构造一个使用默认窗口与阈值、真实时钟的窗口状态。
    pub fn new() -> Self {
        Self::with_limits(EMPTY_STREAM_WINDOW, EMPTY_STREAM_THRESHOLD, Clock::system())
    }

    /// 构造一个自定义窗口/阈值/时钟的窗口状态（单测注入假时钟用）。
    ///
    /// `window == 0` / `threshold == 0` 时取默认值 —— 与 [`crate::wafip`]
    /// 的兜底一致，防止调用方传错参数把判定变成「永远成立」。
    fn with_limits(window: Duration, threshold: usize, now: Clock) -> Self {
        let window = if window.is_zero() {
            EMPTY_STREAM_WINDOW
        } else {
            window
        };
        let threshold = if threshold == 0 {
            EMPTY_STREAM_THRESHOLD
        } else {
            threshold
        };
        Self {
            inner: Mutex::new(Inner {
                window,
                threshold,
                hits: HashMap::new(),
            }),
            now,
        }
    }

    /// 记一次空流，返回该通道窗口内的**不同账号数**。
    ///
    /// 同一账号重复空流只刷新它的时间戳，不累加计数 —— 与 [`crate::wafip`]
    /// 同款理由：按次数计数的话，一个坏账号自己就能凑满阈值，把「单号问题」
    /// 误判成「上游整体故障」，中止掉本来还能用的轮转。
    pub fn note(&self, uid: &str, product: &str, region: &str) -> usize {
        let mut inner = lock_or_recover(&self.inner);
        evict(&mut inner, self.now.now());
        let bucket = inner
            .hits
            .entry((product.to_string(), region.to_string()))
            .or_default();
        bucket.insert(uid.to_string(), self.now.now());
        bucket.len()
    }

    /// 该通道是否已判定「上游整体空流」，返回 `(判定, 窗口内不同账号数)`。
    ///
    /// `accounts_on_channel` 是**该（产品, 区域）通道上一共多少账号**（不看健康度），
    /// 由调用方从账号池取（[`crate::pool::Pool::channel_account_count`]）。阈值按它
    /// 夹小 —— 见 [`effective_threshold`] 与 [`EMPTY_STREAM_THRESHOLD`] 的说明。
    ///
    /// 读路径同样触发过期清理：判定必须随窗口**滑动**失效，而不是一旦
    /// 成立就永久拦下去 —— 上游恢复之后网关不能还在替它「节省轮转」。
    pub fn blocked(&self, product: &str, region: &str, accounts_on_channel: usize) -> (bool, usize) {
        let mut inner = lock_or_recover(&self.inner);
        evict(&mut inner, self.now.now());
        let n = inner
            .hits
            .get(&(product.to_string(), region.to_string()))
            .map(|b| b.len())
            .unwrap_or(0);
        (n >= effective_threshold(accounts_on_channel), n)
    }

    /// 该通道**当前生效**的阈值（供日志与界面说明，不参与判定）。
    pub fn threshold_for(&self, accounts_on_channel: usize) -> usize {
        effective_threshold(accounts_on_channel)
    }

    pub fn window(&self) -> Duration {
        lock_or_recover(&self.inner).window
    }

    pub fn threshold(&self) -> usize {
        lock_or_recover(&self.inner).threshold
    }

    /// 清空全部状态（主要供测试；生产里窗口滑动本来就自净）。
    pub fn reset(&self) {
        lock_or_recover(&self.inner).hits.clear();
    }
}

/// 删除窗口外条目（任何读写路径都要先清，保证判定是**滑动**的）。
fn evict(inner: &mut Inner, now: Instant) {
    for bucket in inner.hits.values_mut() {
        bucket.retain(|_uid, at| now.duration_since(*at) < inner.window);
    }
    inner.hits.retain(|_k, b| !b.is_empty());
}

/// 判定成立时给客户端的中文结论。
///
/// 结构与 [`crate::wafip::egress_ip_blocked_message`] 一致：现象 → 结论
///（换号无用）→ 出路。三条硬性要求：
///
/// 1. **必须**说清「账号是好的」—— 否则用户看到失败会去逐个检查 13 个账号；
/// 2. **不能**出现裸 "401" / "403"（[`crate::clienttext`] 按文案正则判 AUTH，
///    把上游故障显示成「API 密钥无效」）；也不写裸 "503" —— 同样可能被
///    客户端按状态码语义解读成「服务过载，请重试」；
/// 3. **不能**诱导立即重试（现场客户端是 5 路并发重试）：出路写成「稍等
///    片刻」，并给出窗口自动解除的时长，让重试天然错开上游故障期。
pub fn empty_stream_burst_message(distinct: usize, window: Duration) -> String {
    format!(
        "上游疑似整体故障：最近 {} 秒内已有 {} 个**不同账号**在同一通道上返回空流，\
继续换号无用（账号本身是好的，这是上游服务的问题）。\
已中止本次轮转，以免把好账号也拖进冷却。\
请稍等片刻后重试（约 {} 秒后自动恢复正常转发）。",
        window.as_secs(),
        distinct,
        window.as_secs()
    )
}

/// 单号空流、且轮转已耗尽时的客户端文案（2026-10-11 第二次现场）。
///
/// 现场：zcode 通道 3 个号里只有 1 个在返回空流，另外 2 个健康号恰好被并发
/// 占满在途名额 ⇒ 兜底反复把坏流号捞出来打，每次都以「轮转耗尽」失败。旧实现
/// 此时报 `no_healthy_account` +「上游服务异常（HTTP 503），已切换到其他账号」
/// —— 两个字面都错：账号没有被「切换」成功（耗尽了），账号也不是「不健康」。
/// 更糟的是那个码会诱导客户端 5 路并发重试，把瞬时抖动放大成风暴。
///
/// 文案硬性要求与 [`empty_stream_burst_message`] 相同：
/// 1. 说清「账号是好的，不用去检查账号」；
/// 2. 不出现裸 401/403/503；
/// 3. 不鼓励立即重试（客户端会并发放大）。
pub fn empty_stream_single_message() -> String {
    "上游返回空流（无有效数据帧）：该通道的账号没有给出内容，账号本身是好的，\
通常几十秒内自行恢复。本次已无可切换的账号（其余账号正被在途请求占满）。\
请稍等片刻后重试。"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 可控时钟：基准时刻 + 可推进的毫秒偏移（与 [`crate::wafip`] 同款）。
    struct FakeClock {
        base: Instant,
        offset_ms: AtomicU64,
    }

    impl FakeClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                base: Instant::now(),
                offset_ms: AtomicU64::new(0),
            })
        }

        fn advance(&self, d: Duration) {
            self.offset_ms
                .fetch_add(d.as_millis() as u64, Ordering::SeqCst);
        }

        fn now(&self) -> Instant {
            self.base + Duration::from_millis(self.offset_ms.load(Ordering::SeqCst))
        }

        fn clock(self: &Arc<Self>) -> Clock {
            let c = Arc::clone(self);
            Clock(Arc::new(move || c.now()))
        }
    }

    fn test_window(clk: &Arc<FakeClock>) -> EmptyStreamWindow {
        EmptyStreamWindow::with_limits(EMPTY_STREAM_WINDOW, EMPTY_STREAM_THRESHOLD, clk.clock())
    }

    /// 「大池子」的通道账号数：≥ 阈值 ⇒ 阈值就是 [`EMPTY_STREAM_THRESHOLD`]。
    ///
    /// 绝大多数既有测试关心的是**窗口语义**，与池子大小无关，故统一用这个值，
    /// 让它们的断言保持原样。小池子的行为见 `small_pool_*` 那几条专测。
    const BIG_POOL: usize = 19;

    /// ★★ 小池子（qoder 只有 2 个账号）必须**能**判定上游整体故障。
    ///
    /// 现场缺陷（2026-10-20 07:34）：固定阈值 3 对 2 个账号永远达不到 ⇒ 熔断
    /// 永久失效 ⇒ 1 分钟内 23 次「空流 ⇒ 换号 ⇒ 再空流」，每次「轮转耗尽
    /// （已试 2 个账号）」。两个不同账号都空流，已经排除了「单号问题」。
    #[test]
    fn small_pool_reaches_verdict() {
        let clk = FakeClock::new();
        let s = test_window(&clk);

        // 2 个账号的通道
        assert_eq!(s.note("uid-a", "qoder", ""), 1);
        let (blocked, _) = s.blocked("qoder", "", 2);
        assert!(!blocked, "只有 1 个账号空流不能判定 —— 那可能是这个号自己的问题");

        assert_eq!(s.note("uid-b", "qoder", ""), 2);
        let (blocked, n) = s.blocked("qoder", "", 2);
        assert!(
            blocked,
            "2 个账号的通道上，2 个**不同**账号都空流就该判定上游整体故障"
        );
        assert_eq!(n, 2);
    }

    /// 单账号通道：阈值夹到 2 ⇒ 一个号反复空流**仍然不判定**。
    ///
    /// 这是 [`effective_threshold`] 下界为 2 的理由：阈值 1 会让单个账号的
    /// 一次偶发空流立刻熔断整条通道，把「单号问题」误判成「上游整体故障」。
    #[test]
    fn single_account_channel_never_self_trips() {
        let clk = FakeClock::new();
        let s = test_window(&clk);
        for _ in 0..10 {
            s.note("uid-only", "qoder", "");
            let (blocked, _) = s.blocked("qoder", "", 1);
            assert!(!blocked, "单个账号自己不能凑成「上游整体故障」");
        }
    }

    /// ★★ 分桶键必须与**轮转范围**一致（2026-10-20 现场）。
    ///
    /// Qoder 的两个账号分别在 cn / intl；`qoder:模型名` 的轮转是整产品。
    /// 按账号区域分桶会让两个账号落在两个单号桶里 —— 不同账号数永远到不了 2，
    /// 风暴熔断**永久失效**。
    #[test]
    fn product_request_buckets_by_request_region_not_account_region() {
        // 产品前缀请求：看请求区域（未指定 = any ⇒ 整产品一个桶）
        assert_eq!(channel_region(true, "any", "cn"), "any");
        assert_eq!(channel_region(true, "any", "intl"), "any");
        // 显式区域前缀：仍按请求区域（只轮转该区域）
        assert_eq!(channel_region(true, "cn", "intl"), "cn");
        // 裸名请求：轮转范围就是账号区域，保持原口径
        assert_eq!(channel_region(false, "any", "cn"), "cn");
        assert_eq!(channel_region(false, "intl", "cn"), "cn");
    }

    /// 阈值只被**夹小**，绝不被放大：大池子仍是 3。
    #[test]
    fn threshold_is_clamped_not_raised() {
        assert_eq!(effective_threshold(0), 2, "池子为空时不给出「上游故障」结论");
        assert_eq!(effective_threshold(1), 2, "下界 2");
        assert_eq!(effective_threshold(2), 2);
        assert_eq!(effective_threshold(3), 3);
        assert_eq!(effective_threshold(19), EMPTY_STREAM_THRESHOLD);
        assert_eq!(effective_threshold(1000), EMPTY_STREAM_THRESHOLD);
    }

    /// 同一账号反复空流**不算**上游整体故障。
    #[test]
    fn same_account_repeated_does_not_trigger() {
        let clk = FakeClock::new();
        let s = test_window(&clk);

        for i in 0..10 {
            let n = s.note("uid-a", "workbuddy", "intl");
            assert_eq!(n, 1, "第 {} 次同一账号空流的不同账号数应恒为 1", i + 1);
            let (blocked, _) = s.blocked("workbuddy", "intl", BIG_POOL);
            assert!(
                !blocked,
                "第 {} 次同一账号空流就判定上游故障 —— 这是单号问题，不该中止轮转",
                i + 1
            );
        }
    }

    /// 不同账号空流达到阈值即判定上游整体故障。
    ///
    /// 阈值取 3：1-2 个账号无法区分「单号问题」与「上游问题」，
    /// 3 个不同账号接连空流才能排除账号自身因素。
    #[test]
    fn distinct_accounts_trigger() {
        let clk = FakeClock::new();
        let s = test_window(&clk);

        assert_eq!(s.note("uid-a", "workbuddy", "intl"), 1);
        let (blocked, _) = s.blocked("workbuddy", "intl", BIG_POOL);
        assert!(!blocked, "第 1 个账号空流不该判定（阈值是 3 个不同账号）");
        assert_eq!(s.note("uid-b", "workbuddy", "intl"), 2);
        let (blocked, _) = s.blocked("workbuddy", "intl", BIG_POOL);
        assert!(!blocked, "第 2 个不同账号空流仍不该判定");
        assert_eq!(s.note("uid-c", "workbuddy", "intl"), 3);
        let (blocked, n) = s.blocked("workbuddy", "intl", BIG_POOL);
        assert!(blocked, "第 3 个**不同**账号空流应判定上游整体故障");
        assert_eq!(n, 3);
    }

    /// 跨过阈值后，窗口内的判定持续有效。
    ///
    /// 这条防止的放大是：判定出来了，但**下一个请求**又从 0 开始轮转 ——
    /// 于是每个请求都至少多打几个账号。判定必须在窗口内持续有效。
    #[test]
    fn verdict_persists_within_window() {
        let clk = FakeClock::new();
        let s = test_window(&clk);

        s.note("uid-a", "workbuddy", "intl");
        s.note("uid-b", "workbuddy", "intl");
        s.note("uid-c", "workbuddy", "intl");
        let (blocked, _) = s.blocked("workbuddy", "intl", BIG_POOL);
        assert!(blocked);
        // 窗口内继续：重复账号再来一次也应仍判 blocked。
        s.note("uid-a", "workbuddy", "intl");
        let (blocked, _) = s.blocked("workbuddy", "intl", BIG_POOL);
        assert!(blocked, "判定在窗口内应持续有效（重复账号不撤销判定）");
        // 新通道此时仍不受影响（见 keys_are_isolated 的专测）。
    }

    /// 窗口过期后不再判定 —— 判定必须随窗口**滑动**失效。
    ///
    /// 上游整体故障是**临时**状态。若做成累计计数，一旦过线就永久成立，
    /// 之后哪怕上游早已恢复，网关也会一直拒绝换号（表现为「账号明明恢复了
    /// 却怎么都不好用」）。
    #[test]
    fn window_expiry_resets_verdict() {
        let clk = FakeClock::new();
        let s = test_window(&clk);

        s.note("uid-a", "workbuddy", "intl");
        s.note("uid-b", "workbuddy", "intl");
        s.note("uid-c", "workbuddy", "intl");
        let (blocked, _) = s.blocked("workbuddy", "intl", BIG_POOL);
        assert!(blocked);
        // 时间推进到窗口之外。
        clk.advance(EMPTY_STREAM_WINDOW + Duration::from_secs(1));

        let (blocked, n) = s.blocked("workbuddy", "intl", BIG_POOL);
        assert!(!blocked, "窗口过期后判定必须解除");
        assert_eq!(n, 0, "过期条目必须被清理");
        // 重新凑 3 个不同账号才能再次判定 —— 证明是滑动窗口不是永久封锁。
        assert_eq!(s.note("uid-d", "workbuddy", "intl"), 1);
        assert_eq!(s.note("uid-e", "workbuddy", "intl"), 2);
        let (blocked, _) = s.blocked("workbuddy", "intl", BIG_POOL);
        assert!(!blocked);
        assert_eq!(s.note("uid-f", "workbuddy", "intl"), 3);
        let (blocked, _) = s.blocked("workbuddy", "intl", BIG_POOL);
        assert!(blocked);
    }

    /// 窗口过期后条目被真正删除（内存只增不减的防线）。
    #[test]
    fn window_eviction_keeps_memory_bounded() {
        let clk = FakeClock::new();
        let s = test_window(&clk);

        for uid in ["u1", "u2", "u3", "u4", "u5"] {
            s.note(uid, "workbuddy", "intl");
        }
        assert_eq!(
            lock_or_recover(&s.inner)
                .hits
                .get(&("workbuddy".to_string(), "intl".to_string()))
                .map(|b| b.len())
                .unwrap_or(0),
            5,
            "清理前应有 5 条记录"
        );
        clk.advance(EMPTY_STREAM_WINDOW + Duration::from_secs(1));
        s.note("fresh", "workbuddy", "intl"); // 任何一次写入都会触发清理
        let bucket_len = lock_or_recover(&s.inner)
            .hits
            .get(&("workbuddy".to_string(), "intl".to_string()))
            .map(|b| b.len())
            .unwrap_or(0);
        assert_eq!(
            bucket_len, 1,
            "窗口过期后应只剩 1 条记录（fresh）—— 否则内存只增不减"
        );
        // 空桶必须整体摘除，不能留下空 HashMap 常驻。
        let empty_buckets = lock_or_recover(&s.inner).hits.len();
        assert_eq!(
            empty_buckets, 1,
            "只有 workbuddy|intl 一个通道有记录，其它通道的空桶必须被摘除"
        );
    }

    /// 并发写入不出错（现场正是 5 路并发重试同时吃到空流）。
    #[test]
    fn state_is_concurrency_safe() {
        let clk = FakeClock::new();
        let s = Arc::new(test_window(&clk));

        let mut handles = Vec::new();
        for k in 0..8u8 {
            let s = Arc::clone(&s);
            handles.push(std::thread::spawn(move || {
                for _ in 0..50 {
                    s.note(&format!("uid-{}", (b'a' + k) as char), "workbuddy", "intl");
                    let _ = s.blocked("workbuddy", "intl", BIG_POOL);
                }
            }));
        }
        for h in handles {
            h.join().expect("并发线程不该 panic");
        }
        let (blocked, n) = s.blocked("workbuddy", "intl", BIG_POOL);
        assert_eq!(n, 8, "并发写入后不同账号数应为 8");
        assert!(blocked);
    }

    /// `blocked()` 必须与 `note` 的计数判定逐字一致。
    #[test]
    fn blocked_matches_threshold_comparison() {
        let clk = FakeClock::new();
        let s = test_window(&clk);
        assert_eq!(s.threshold(), EMPTY_STREAM_THRESHOLD);
        assert_eq!(s.window(), EMPTY_STREAM_WINDOW);

        for (i, uid) in ["a", "b", "c", "d"].iter().enumerate() {
            let n = s.note(uid, "workbuddy", "intl");
            let (blocked, _) = s.blocked("workbuddy", "intl", BIG_POOL);
            assert_eq!(
                blocked,
                n >= s.threshold(),
                "第 {} 个账号后 blocked() 与 note 计数判定不一致（n={n}）",
                i + 1
            );
        }
    }

    /// `Default` 与 `new()` 同义，且非法参数走兜底。
    #[test]
    fn default_matches_new() {
        let a = EmptyStreamWindow::default();
        assert_eq!(a.window(), EMPTY_STREAM_WINDOW);
        assert_eq!(a.threshold(), EMPTY_STREAM_THRESHOLD);
        let (blocked, n) = a.blocked("workbuddy", "intl", BIG_POOL);
        assert!(!blocked);
        assert_eq!(n, 0);
        // 非法参数走兜底（与 wafip 同款）。
        let b = EmptyStreamWindow::with_limits(Duration::ZERO, 0, Clock::system());
        assert_eq!(b.window(), EMPTY_STREAM_WINDOW);
        assert_eq!(b.threshold(), EMPTY_STREAM_THRESHOLD);
    }

    /// ★ 判定按（产品, 区域）隔离：国际区的整体故障不得连坐国服。
    ///
    /// 现场只有国际区 `deepseek-v4.1-flash-sg` 故障；若判定不按通道隔离，
    /// 国服账号会被无意义地一起拦下 —— 那是网关自己制造的故障。
    #[test]
    fn keys_are_isolated() {
        let clk = FakeClock::new();
        let s = test_window(&clk);

        s.note("uid-a", "workbuddy", "intl");
        s.note("uid-b", "workbuddy", "intl");
        s.note("uid-c", "workbuddy", "intl");
        let (intl_blocked, _) = s.blocked("workbuddy", "intl", BIG_POOL);
        assert!(intl_blocked, "国际区凑满阈值应判定");
        let (cn_blocked, n) = s.blocked("workbuddy", "cn", BIG_POOL);
        assert!(!cn_blocked, "国际区故障不得连坐国服（cn 窗口应为空）");
        assert_eq!(n, 0);
        let (zcode_blocked, _) = s.blocked("zcode", "intl", BIG_POOL);
        assert!(!zcode_blocked, "产品维度也必须隔离");
    }

    /// 客户端文案必须可读且不踩客户端的正则坑。
    ///
    /// ⚠ 不能出现裸 "401" / "403"（clienttext 按文案正则判 AUTH）；也**不写**
    /// 裸 "503"（客户端可能按状态码语义解读成「过载，请重试」）。必须包含：
    /// 「换号无用」「账号是好的」「稍等」与自动解除时长。
    #[test]
    fn message_readable() {
        let msg = empty_stream_burst_message(5, EMPTY_STREAM_WINDOW);
        assert!(msg.contains("换号无用"), "{msg}");
        assert!(msg.contains("账号本身是好的"), "{msg}");
        assert!(msg.contains("稍等"), "{msg}");
        assert!(msg.contains("60 秒"), "{msg}");
        assert!(msg.contains("5"), "{msg}");
        for bad in ["401", "403", "503"] {
            assert!(
                !msg.contains(bad),
                "文案不得包含裸 {bad}（会被客户端按状态码/密钥语义误读）: {msg}"
            );
        }
    }
}
