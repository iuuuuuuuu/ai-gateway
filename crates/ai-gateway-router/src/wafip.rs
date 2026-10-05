//! wafip.rs 「WAF 403 拦的是出口 IP，不是账号」的识别与判定。
//!
//! 逐字移植 Go 侧 `internal/server/wafip.go`（323 行），常量、判据与文案一字不改。
//!
//! # 要解决的问题（真实缺陷）
//!
//! 此前网关对 403 只有**账号级**软冷却，没有「这可能是出口 IP 被拦」的判断。
//! 后果：所有号轮一遍全 403，而网关还在继续换号，把请求放大 `MaxRotate` 倍 ——
//! 恰好是 WAF 最想惩罚的行为，于是封禁被越打越重。
//!
//! 上游 Sliverkiss/workbuddy2api 的实测记录：
//!
//! ```text
//! WAF 403 拦的是网关出口 IP 而非账号 —— 3 个账号 1 秒内全 403
//! ```
//!
//! 这正是本模块要捕捉的特征：**同一账号**反复 403 是账号问题（该罚该换），
//! 而**多个不同账号**在短时间内一起 403，只能是它们共同的东西出了问题 ——
//! 共用的出口 IP。账号本身是好的。
//!
//! # 为什么阈值是「不同账号数」而不是「403 次数」
//!
//! 单个坏账号在一分钟内可以轻易打出几十次 403。若按次数判定，一个账号就足以
//! 让我们误判「整个出口 IP 被拦」，从而把好账号的请求也一起中止掉。
//! 按**不同账号**计数则要求证据来自多个独立凭证，这才排除了账号自身的因素。
//!
//! # 为什么是滑动窗口而不是累计计数
//!
//! 出口 IP 被拦是**临时**状态（几分钟到几十分钟）。累计计数一旦过线就永久成立，
//! 之后哪怕 IP 早已解封，网关也会一直拒绝换号。滑动窗口让判定自动过期。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// 滑动窗口长度：只看最近这段时间内的 403。
///
/// 60 秒的依据：一次请求的整个轮转过程（含退避）在秒级完成，同一个出口 IP 上
/// 「多个账号几乎同时 403」这一特征只在这个尺度内成立。窗口再长会把不相关的
/// 偶发 403 凑成一次误判。
pub const WAF_IP_WINDOW: Duration = Duration::from_secs(60);

/// 窗口内**不同账号**吃到 WAF 403 的数量达到它即判定 IP 被拦。
///
/// 取 2：一个账号的 403 无法区分「账号问题」与「IP 问题」，两个不同账号同时
/// 403 才能排除账号自身因素。这也是上游用的口径。
pub const WAF_IP_THRESHOLD: usize = 2;

/// 时钟注入点：生产用 [`Instant::now`]，单测注入假时钟以断言「窗口过期」。
///
/// Go 侧是 `now func() time.Time` 字段。测试各造各的窗口状态，互不干扰 ——
/// 这也是「不许用 `sleep` 验窗口过期」的落点。
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

/// 取锁并**忽略中毒**（`Mutex` 中毒是 Rust 特有的机制，Go 没有）。
///
/// 一个用例里断言失败会让锁中毒，若继续 `unwrap()` 则后续所有用例都会连锁
/// panic，把「一处断言失败」放大成「整片红灯」，反而看不出真正的问题。
fn lock_or_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 出口 IP 级 WAF 403 的滑动窗口状态。
///
/// 并发安全（`Mutex`）：同一进程内多个请求会同时轮转，各自独立地 403，
/// 正是它们的**并集**才能说明 IP 被拦 —— 故这份状态必须是进程级共享的。
/// 这也意味着它**不该**挂在某个 handler 上：出口 IP 是整个进程的属性，
/// 不是某个 handler 实例的属性（进程级实例见 [`global`]）。
#[derive(Debug)]
pub struct WafIpWindow {
    inner: Mutex<Inner>,
    /// 时钟注入点；生产用 [`Instant::now`]，单测注入假时钟。
    now: Clock,
}

#[derive(Debug)]
struct Inner {
    window: Duration,
    threshold: usize,
    /// uid → 该账号**最近一次** WAF 403 的时刻。
    ///
    /// 用 map 而不是 `Vec`：map 的 key 天然去重，「不同账号数」就等于
    /// `hits.len()`，不必再写一遍去重逻辑（那份逻辑最容易在「同一账号多次
    /// 403」上出错）。
    hits: HashMap<String, Instant>,
}

impl Default for WafIpWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl WafIpWindow {
    /// 构造一个使用默认窗口与阈值、真实时钟的窗口状态。
    pub fn new() -> Self {
        Self::with_limits(WAF_IP_WINDOW, WAF_IP_THRESHOLD, Clock::system())
    }

    /// 构造一个自定义窗口/阈值/时钟的窗口状态（Go `newWafIPState`）。
    ///
    /// `window <= 0` / `threshold <= 0` 时取默认值 —— 与 Go 的兜底一致。
    fn with_limits(window: Duration, threshold: usize, now: Clock) -> Self {
        let window = if window.is_zero() {
            WAF_IP_WINDOW
        } else {
            window
        };
        let threshold = if threshold == 0 {
            WAF_IP_THRESHOLD
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

    /// 记录「账号 `uid` 刚吃到一次 WAF 403」，返回窗口内**不同账号**数。
    ///
    /// 调用方据返回值（或 [`Self::blocked`]）决定是否立即中止轮转 —— 继续换号
    /// 只会让 WAF 看到更多账号被打，放大风控。
    ///
    /// ⚠ 一旦跨过阈值，在窗口内的**后续调用**也会返回 ≥ 阈值的计数（包括那
    /// 第一个账号再次 403 时）。这是刻意的：判定结果在窗口内持续有效，正是它
    /// 阻止了「判定出来了但下一个请求又从 0 开始轮转」这种放大。窗口过后自动恢复。
    ///
    /// 同一账号重复 403 **只刷新它自己的时间戳**，不会增加不同账号数 ——
    /// 那是账号问题，不该被算成 IP 问题。
    pub fn note_waf(&self, uid: &str) -> usize {
        let now = self.now.now();
        let mut inner = lock_or_recover(&self.inner);
        // 先清理过期条目：内存不会只增不减。
        //
        // 为什么用「顺手清理」而不是起一个后台定时器：条目上限本来就是账号总数
        //（key 是 uid，天然去重），量级极小；为它维护一个后台任务的生命周期
        //（启动 / 退出 / 测试里的泄漏）得不偿失。每次 `note_waf` 都会清一次，
        // 而 `note_waf` 正是唯一写入路径 —— 不存在「写了很多次却没清」的窗口。
        evict(&mut inner, now);
        inner.hits.insert(uid.to_string(), now);
        inner.hits.len()
    }

    /// 当前窗口内吃到过 WAF 403 的**不同账号**数（顺带清理过期条目）。
    ///
    /// 仅用于把数量写进给用户看的错误文案。
    pub fn distinct(&self) -> usize {
        let now = self.now.now();
        let mut inner = lock_or_recover(&self.inner);
        evict(&mut inner, now);
        inner.hits.len()
    }

    /// 是否已达「出口 IP 被拦」判定阈值。
    ///
    /// 与 Go `noteWaf` 的返回值**逐字同义**：
    /// `len(hits) >= threshold` 就等价于 `distinct() >= threshold`。
    /// 单独拆出来是为了让 [`Self::note_waf`] 能同时把计数交给调用方写日志，
    /// 而判定本身仍有一处唯一定义。
    pub fn blocked(&self) -> bool {
        let now = self.now.now();
        let mut inner = lock_or_recover(&self.inner);
        evict(&mut inner, now);
        inner.hits.len() >= inner.threshold
    }

    /// 当前生效的窗口长度（供调用方写日志 / 文案对账）。
    pub fn window(&self) -> Duration {
        lock_or_recover(&self.inner).window
    }

    /// 当前生效的阈值（供调用方自行比较，见 [`Self::note_waf`] 的说明）。
    pub fn threshold(&self) -> usize {
        lock_or_recover(&self.inner).threshold
    }

    /// 清空窗口（Go `(*wafIPState).reset`）。
    ///
    /// 生产路径不需要它；留给测试隔离用例之间的状态。
    pub fn reset(&self) {
        lock_or_recover(&self.inner).hits.clear();
    }
}

/// 丢弃窗口外的条目。
fn evict(inner: &mut Inner, now: Instant) {
    // Go：`if now.Sub(at) >= s.window { delete }` —— 恰好等于窗口长度即算过期。
    let window = inner.window;
    inner.hits.retain(|_, at| now.duration_since(*at) < window);
}

/// 进程级共享的出口 IP 判定状态（Go `var wafIP = newWafIPState(...)`）。
///
/// 用进程级单例而不是让调用方自己持有：出口 IP 属于**整个进程**（同一个网关
/// 进程只有一个出口），跨请求、跨 handler 实例共享才是正确语义。
/// 本 crate 已有同类先例（`products` 里的区域模型缓存）。
pub fn global() -> &'static WafIpWindow {
    static GLOBAL: OnceLock<WafIpWindow> = OnceLock::new();
    GLOBAL.get_or_init(WafIpWindow::new)
}

/// 拦截页 / 边缘节点的**强特征**（小写比较）。
///
/// ⚠ 这里**刻意不收敛**的泛词：`forbidden` / `denied` / 单独的「拦截」在账号被
/// 封的业务文案里同样常见（`"your account has been blocked"`、「账号已被拦截」）。
/// 收进来会把账号问题误判成 IP 问题 —— 那正好是本模块要区分开的两件事。
/// 故只登记**只可能出自边缘节点**的特征。
///
/// 这份列表只在「响应体看起来是业务信封」时才需要发力（见 [`is_waf_forbidden`]）：
/// 非信封的 403 本来就已判定为 WAF。它要覆盖的真实形态是边缘节点返回的 JSON，例如：
///
/// ```text
/// 403 {"code":403,"msg":"Request blocked by WAF"}
/// ```
///
/// 这条看着像信封（有 code），但说的是 WAF —— 靠列表才能识别出来。
const WAF_FORBIDDEN_MARKERS: &[&str] = &[
    "cloudflare",
    "<html",
    "<!doctype html",
    "安全策略",
    "异常流量",
    "访问被拦截",
    "请求被拦截",
];

/// 报告一次 403 是否**像是边缘 WAF 的拦截**（而非上游业务拒绝）。
///
/// 判据（任一成立）：
///
/// 1. 响应体**不是**上游的业务 JSON 信封（主判据，见下）；
/// 2. 响应体含拦截页强特征（见 [`WAF_FORBIDDEN_MARKERS`]）。
///
/// 为什么第 1 条是主判据：上游业务拒绝（如 ZCode 的
/// `403 {"code":3101,"msg":"coding plan is required"}`）一律是
/// `{code, msg, data}` 形状的信封，能解析出业务码与文案 —— 那说明请求
/// **已经到达应用层**，被拦的与该账号/该请求有关，与出口 IP 无关。
/// 反过来，拿不到业务信封的 403（HTML 拦截页、纯文本、空体）说明请求在应用层
/// 之前就被挡下了，那才是出口 IP 层面的问题。
///
/// # 判据的**顺序**很关键（防误判）
///
/// 先看信封、再看特征，而不是反过来。若先扫特征，`{"code":1,"msg":"wafer
/// quota exceeded"}` 这种正文里恰好含 `waf` 子串的业务响应就会被误判成 IP 问题，
/// 让整个网关停止轮转。反过来「先判信封、再让特征覆盖」则两边都安全：
/// 业务信封默认按业务处理，只有真的带边缘节点特征时才改判。
///
/// ⚠ 空体 403 也算「像 WAF」：上游应用层的拒绝从不空体，空体只出现在请求被
/// 边缘节点直接掐断时。这一条是判断而非事实，故最终给用户的文案用的是
/// 「**可能**被拦」，且只有跨过「2 个不同账号」的阈值才会触发。
pub fn is_waf_forbidden(status: u16, body: &str) -> bool {
    if status != 403 {
        return false;
    }
    if !is_business_envelope(body) {
        return true;
    }
    // 是业务信封：只有带边缘节点特征时才改判为 WAF。
    let lower = body.to_lowercase();
    if WAF_FORBIDDEN_MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    // "waf" 单独成一个词才算（"Request blocked by WAF" 命中，
    // 而 "wafer" / "dwarf" 这类只是含子串的单词不命中）。
    contains_word(&lower, "waf")
}

/// 报告 `s` 中是否**以独立单词**的形式出现 `word`（`s` 须已小写）。
///
/// 用边界判定而不是裸 `contains`：后者会把 `"wafer"`、`"dwarf"` 也当成命中，
/// 而本函数的调用点在 403 判定路径上 —— 一次误判就会让整个网关停止轮转，
/// 代价远大于多写这几行。
///
/// 逐字对齐 Go 的字节级实现（`containsWord` / `byteAt` / `isWordByte`）：
/// 索引一律走**字节**而非字符，且非 ASCII 字节一律视为边界。
pub fn contains_word(s: &str, word: &str) -> bool {
    let hay = s.as_bytes();
    let needle = word.as_bytes();
    let mut i = 0usize;
    loop {
        let Some(rel) = find_from(hay, needle, i) else {
            return false;
        };
        let j = i + rel;
        if !is_word_byte(byte_at(hay, j as isize - 1))
            && !is_word_byte(byte_at(hay, (j + needle.len()) as isize))
        {
            return true;
        }
        i = j + 1;
        if i >= hay.len() {
            return false;
        }
    }
}

/// 在 `hay[from..]` 里找 `needle`，返回**相对 `from`** 的偏移（Go `strings.Index`）。
///
/// 空 `needle` 返回 `Some(0)` —— 与 Go `strings.Index(s, "") == 0` 同语义。
fn find_from(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from > hay.len() {
        return None;
    }
    if needle.is_empty() {
        return Some(0);
    }
    let rest = &hay[from..];
    if needle.len() > rest.len() {
        return None;
    }
    rest.windows(needle.len()).position(|w| w == needle)
}

/// 取 `s[i]`，越界返回 `0`（调用方用它表示「没有相邻字符」= 边界）。
fn byte_at(s: &[u8], i: isize) -> u8 {
    if i < 0 || i as usize >= s.len() {
        return 0;
    }
    s[i as usize]
}

/// 报告 `b` 是否为「ASCII 单词字符」（字母/数字/下划线）。
///
/// ⚠ 非 ASCII 一律返回 `false`（= 视为边界），这个方向是刻意的：
///
/// * 中文等文字**不用空格分词**，`"WAF拦截"` 里的 WAF 必须能被认出来，
///   把汉字当单词字符会让它漏判；
/// * 而需要防的误判（`"wafer"` / `"dwarf"`）其相邻字符必然是 ASCII 字母，
///   与本分支无关。
///
/// 两个方向的收益不对称：漏判只是回到改动前的行为（继续轮转），
/// 误判会让整个网关停止轮转。
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// 报告响应体是否为上游的**业务 JSON 信封**。
///
/// 认两种形状（网关上游实际用到的全部）：
///
/// ```text
/// {"code":3101,"msg":"coding plan is required","data":...}   上游统一信封
/// {"error":{"code":...,"message":...,"type":...}}             OpenAI 形状
/// ```
///
/// 空体、非 JSON、JSON 数组/标量都返回 `false`（= 不是业务信封）。
pub fn is_business_envelope(body: &str) -> bool {
    let t = body.trim();
    if t.is_empty() || !t.starts_with('{') {
        return false;
    }
    let Ok(serde_json::Value::Object(doc)) = serde_json::from_str::<serde_json::Value>(t) else {
        return false;
    };
    // 上游统一信封：顶层有 code 即认定（msg 允许缺失）。
    if doc.contains_key("code") {
        return true;
    }
    // OpenAI 形状：error 是**对象**且带任一业务字段。
    // error 为字符串（如 {"error":"Forbidden"}）不算 —— 那是边缘节点的写法。
    if let Some(serde_json::Value::Object(e)) = doc.get("error") {
        return ["code", "message", "msg", "type", "data"]
            .iter()
            .any(|k| e.contains_key(*k));
    }
    false
}

/// 生成「出口 IP 可能被 WAF 拦截」的客户端文案（Go `egressIPBlockedMessage`）。
///
/// 必须同时说清三件事，缺一件用户就会走错排查方向：
///
/// ```text
/// 现象   —— 多个不同账号在短时间内一起被拒
/// 结论   —— 问题在出口 IP，不在账号（所以**换号无用**）
/// 出路   —— 查本机网络出口（代理 / VPN / 公网 IP）后重试
/// ```
///
/// 刻意用「可能」而不是断定：网关能看到的是「多个账号同时被拒」这个事实，
/// 至于它是 WAF 拦 IP、还是上游整体故障，从响应体上无法完全区分。
///
/// ⚠ 文案里**不能出现裸 "403"**（2026-09-29 实测）：客户端按文案正则判 AUTH
///（`/\b(?:401|403)\b/` → 「API 密钥无效」），这里原本写了两处「403」，会让
/// 「出口 IP 被封」这条本该引导用户去查网络的提示，反而显示成密钥问题。
/// 见 `clienttext.rs`。此处改用「被拒」表述 —— 对用户而言信息量相同，
/// 而「403」这个数字本来也不是他能操作的东西。
pub fn egress_ip_blocked_message(distinct: usize) -> String {
    format!(
        "上游疑似 WAF 拦截：最近 {} 秒内已有 {} 个**不同账号**被上游拒绝，\
被拦的可能是本机出口 IP 而非账号（同一账号反复被拒才是账号问题）。\
继续换号无用，已中止本次轮转以免放大风控。\
请检查本机网络出口（代理 / VPN / 公网 IP）后重试。",
        WAF_IP_WINDOW.as_secs(),
        distinct
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 可控时钟：基准时刻 + 可推进的毫秒偏移。
    ///
    /// 用「基准 + 偏移」而不是直接构造 `Instant`：`Instant` 无法凭空造出任意
    /// 时刻，而测试需要把时间**推快**（否则验证「窗口过期」只能真的 sleep
    /// 60 秒 —— Go 侧的做法同样是注入假时钟，见 `wafip_test.go` 的
    /// `newTestWafIP`）。
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

    fn test_window(clk: &Arc<FakeClock>) -> WafIpWindow {
        WafIpWindow::with_limits(WAF_IP_WINDOW, WAF_IP_THRESHOLD, clk.clock())
    }

    /// 同一账号反复 403 **不算** IP 问题。
    ///
    /// 这是本模块最容易被写错的一条：按「403 次数」计数的话，一个坏账号
    /// 一分钟内就能轻易打出几十次 403，从而误判「整个出口 IP 被拦」，
    /// 把好账号的请求也一起中止掉。判定必须按**不同账号**计数。
    #[test]
    fn same_account_repeated_does_not_trigger() {
        let clk = FakeClock::new();
        let s = test_window(&clk);

        for i in 0..10 {
            let n = s.note_waf("uid-a");
            assert_eq!(n, 1, "第 {} 次同一账号 403 的不同账号数应恒为 1", i + 1);
            assert!(
                !s.blocked(),
                "第 {} 次同一账号 403 就判定 IP 被拦 —— 这是账号问题，不该升级成 IP 问题",
                i + 1
            );
        }
        assert_eq!(s.distinct(), 1, "重复 403 不该累加不同账号数");
    }

    /// 不同账号 403 达到阈值即判定 IP 被拦。
    ///
    /// 阈值取 2：一个账号的 403 无法区分「账号问题」与「IP 问题」，
    /// 两个不同账号同时 403 才能排除账号自身因素。
    #[test]
    fn distinct_accounts_trigger() {
        let clk = FakeClock::new();
        let s = test_window(&clk);

        assert_eq!(s.note_waf("uid-a"), 1);
        assert!(
            !s.blocked(),
            "第 1 个账号 403 不该立刻判定 IP 被拦（阈值是 2 个不同账号）"
        );
        assert_eq!(s.note_waf("uid-b"), 2);
        assert!(s.blocked(), "第 2 个**不同**账号 403 应判定 IP 被拦");
        assert_eq!(s.distinct(), 2);
    }

    /// 跨过阈值后，窗口内的后续调用仍返回已判定。
    ///
    /// 这条防止的放大是：判定出来了，但**下一个请求**又从 0 开始轮转
    ///（那时窗口里只有一个新账号的 403，计数 < 阈值）—— 于是每个请求都至少
    /// 多打一个账号。判定结果必须在窗口内持续有效。
    #[test]
    fn verdict_persists_within_window() {
        let clk = FakeClock::new();
        let s = test_window(&clk);

        s.note_waf("uid-a");
        s.note_waf("uid-b");
        assert!(s.blocked(), "第 2 个不同账号 403 应判定 IP 被拦");
        // 窗口内继续：一个新账号的 403 也应立即判 blocked。
        assert_eq!(s.note_waf("uid-c"), 3);
        assert!(s.blocked(), "已判定 IP 被拦后，窗口内新账号 403 应继续判 blocked");
        // 连第一个账号再来一次，也应 blocked（判定在窗口内持续有效）。
        assert!(s.note_waf("uid-a") >= WAF_IP_THRESHOLD);
        assert!(
            s.blocked(),
            "已判定 IP 被拦后，窗口内重复账号 403 也应判 blocked"
        );
    }

    /// 窗口过期后不再判定 IP 被拦。
    ///
    /// 出口 IP 被拦是**临时**状态。若做成累计计数，一旦过线就永久成立 ——
    /// 之后哪怕 IP 早已解封，网关也会一直拒绝换号（表现为「账号明明恢复了
    /// 却怎么都不好用」）。滑动窗口让判定自动过期。
    #[test]
    fn window_expiry_resets_verdict() {
        let clk = FakeClock::new();
        let s = test_window(&clk);

        assert_eq!(s.note_waf("uid-a"), 1);
        assert!(!s.blocked());
        // 时间推进到窗口之外：uid-a 的 403 应被清掉。
        clk.advance(WAF_IP_WINDOW + Duration::from_secs(1));

        assert_eq!(s.distinct(), 0, "窗口过期后不同账号数应为 0（过期条目必须被清理）");
        // 此时只有 1 个「不同账号」在窗口内，不该判定。
        assert_eq!(s.note_waf("uid-b"), 1);
        assert!(!s.blocked(), "窗口过期后单个账号 403 不该判定 IP 被拦");
        // 再凑一个不同账号才应判定 —— 证明窗口是**滑动**的而不是被永久重置。
        assert_eq!(s.note_waf("uid-c"), 2);
        assert!(s.blocked(), "窗口内凑够 2 个不同账号后应判定 IP 被拦");
    }

    /// 窗口过期后条目被真正删除（内存只增不减的防线）。
    ///
    /// `hits` 的 key 是 uid，若不清理，长时间运行的网关会为每个曾经 403 过的
    /// 账号永久保留一条记录。
    #[test]
    fn window_eviction_keeps_memory_bounded() {
        let clk = FakeClock::new();
        let s = test_window(&clk);

        for uid in ["u1", "u2", "u3", "u4", "u5"] {
            s.note_waf(uid);
        }
        assert_eq!(lock_or_recover(&s.inner).hits.len(), 5, "清理前应有 5 条记录");
        clk.advance(WAF_IP_WINDOW + Duration::from_secs(1));
        s.note_waf("fresh"); // 任何一次写入都会触发清理
        assert_eq!(
            lock_or_recover(&s.inner).hits.len(),
            1,
            "窗口过期后应只剩 1 条记录（fresh）—— 否则内存只增不减"
        );
        // `distinct()` / `blocked()` 也必须顺带清理，不能只有写入路径清。
        s.note_waf("u1");
        s.note_waf("u2");
        clk.advance(WAF_IP_WINDOW + Duration::from_secs(1));
        assert_eq!(s.distinct(), 0, "读路径也应触发清理");
        assert!(!s.blocked());
    }

    /// 并发写入不出错（同一进程内多个请求会同时轮转、各自独立地 403）。
    ///
    /// 正是它们的**并集**才能说明 IP 被拦 —— 故这份状态必须并发安全。
    #[test]
    fn state_is_concurrency_safe() {
        let clk = FakeClock::new();
        let s = Arc::new(test_window(&clk));

        let mut handles = Vec::new();
        for k in 0..8u8 {
            let s = Arc::clone(&s);
            handles.push(std::thread::spawn(move || {
                for _ in 0..50 {
                    s.note_waf(&format!("uid-{}", (b'a' + k) as char));
                    s.distinct();
                    s.blocked();
                }
            }));
        }
        for h in handles {
            h.join().expect("并发线程不该 panic");
        }
        assert_eq!(s.distinct(), 8, "并发写入后不同账号数应为 8");
    }

    /// `blocked()` 必须与 `note_waf` 的计数判定逐字一致。
    ///
    /// 判定散成两处是最容易漂移的写法（一处改了另一处没改），故单列一条钉住。
    #[test]
    fn blocked_matches_threshold_comparison() {
        let clk = FakeClock::new();
        let s = test_window(&clk);
        assert_eq!(s.threshold(), WAF_IP_THRESHOLD);
        assert_eq!(s.window(), WAF_IP_WINDOW);

        for (i, uid) in ["a", "b", "c", "d"].iter().enumerate() {
            let n = s.note_waf(uid);
            assert_eq!(
                s.blocked(),
                n >= s.threshold(),
                "第 {} 个账号后 blocked() 与 note_waf 计数判定不一致（n={n}）",
                i + 1
            );
        }
    }

    /// `Default` 与 `new()` 同义，且默认窗口/阈值就是 Go 的常量。
    #[test]
    fn default_matches_new() {
        let a = WafIpWindow::default();
        assert_eq!(a.window(), WAF_IP_WINDOW);
        assert_eq!(a.threshold(), WAF_IP_THRESHOLD);
        assert_eq!(a.distinct(), 0);
        assert!(!a.blocked());
        // 非法参数走兜底（Go `newWafIPState` 的 `<= 0` 分支）。
        let b = WafIpWindow::with_limits(Duration::ZERO, 0, Clock::system());
        assert_eq!(b.window(), WAF_IP_WINDOW);
        assert_eq!(b.threshold(), WAF_IP_THRESHOLD);
    }

    /// 进程级单例是**同一个**实例（跨请求共享才是正确语义）。
    #[test]
    fn global_is_shared() {
        let a = global();
        let b = global();
        assert!(std::ptr::eq(a, b), "global() 每次必须返回同一个实例");
        // 不在此处写状态：它是全局的，会串到别的用例上。
    }

    /// 判定「这次 403 是不是边缘 WAF 的拦截」。
    ///
    /// 主判据是「响应体不是上游业务 JSON 信封」：业务拒绝能解析出业务码与文案，
    /// 说明请求**已经到达应用层**，被拦的与该账号/该请求有关，与出口 IP 无关；
    /// 拿不到业务信封的 403 说明请求在应用层之前就被挡下了。
    #[test]
    fn is_waf_forbidden_table() {
        let cases: &[(&str, u16, &str, bool)] = &[
            // —— 业务拒绝：必须判 false，否则会把账号/套餐问题误升级成 IP 问题 ——
            (
                "上游业务信封（ZCode 缺套餐）",
                403,
                r#"{"code":3101,"msg":"coding plan is required"}"#,
                false,
            ),
            (
                "上游业务信封（中文）",
                403,
                r#"{"code":403,"msg":"账号已被禁用","data":null}"#,
                false,
            ),
            (
                "OpenAI 形状业务错误",
                403,
                r#"{"error":{"message":"no permission","type":"invalid_request_error","code":"403"}}"#,
                false,
            ),
            (
                "账号被 block 的业务文案不该误判（markers 刻意不收敛的泛词）",
                403,
                r#"{"code":1001,"msg":"your account has been blocked"}"#,
                false,
            ),
            (
                "业务信封正文恰好含 wafer 子串（单词边界判定）",
                403,
                r#"{"code":1,"msg":"wafer quota exceeded"}"#,
                false,
            ),
            (
                "业务信封正文含 dwarf 子串",
                403,
                r#"{"code":1,"msg":"dwarf plan not available"}"#,
                false,
            ),
            (
                "业务信封含「账号已被拦截」（泛词刻意不收录）",
                403,
                r#"{"code":1,"msg":"账号已被拦截，请联系客服"}"#,
                false,
            ),
            // —— 边缘拦截：判 true ——
            (
                "HTML 拦截页",
                403,
                r#"<!DOCTYPE html><html><head><title>403 Forbidden</title></head></html>"#,
                true,
            ),
            (
                "Cloudflare 拦截页",
                403,
                r#"<html><body>Attention Required! | Cloudflare</body></html>"#,
                true,
            ),
            (
                "带 WAF 标识的 JSON（非信封）",
                403,
                r#"{"waf":"blocked","reason":"malicious traffic"}"#,
                true,
            ),
            (
                "信封形状但文案说 WAF（靠特征列表识别）",
                403,
                r#"{"code":403,"msg":"Request blocked by WAF"}"#,
                true,
            ),
            (
                "信封形状 + WAF 紧邻汉字（单词边界含非 ASCII）",
                403,
                r#"{"code":403,"msg":"WAF拦截了本次请求"}"#,
                true,
            ),
            ("中文安全策略页", 403, "您的请求触发安全策略，访问被拦截", true),
            ("空体 403（应用层拒绝从不空体）", 403, "", true),
            ("纯文本 403", 403, "Forbidden", true),
            ("JSON 数组（非信封）", 403, r#"["forbidden"]"#, true),
            // —— 非 403 一律不判 ——
            ("401 不判", 401, "<html>unauthorized</html>", false),
            ("429 不判", 429, "<html>too many requests</html>", false),
            ("500 不判", 500, "<html>oops</html>", false),
            ("200 不判", 200, "", false),
        ];
        for (name, status, body, want) in cases {
            assert_eq!(
                is_waf_forbidden(*status, body),
                *want,
                "{name}：is_waf_forbidden({status}, {body:?}) 期望 {want}"
            );
        }
    }

    /// 业务信封识别的边界。
    ///
    /// 这一层判错的方向性后果不对称：
    /// * 把信封判成「不是信封」→ 账号/套餐问题被升级成 IP 问题 → 停止轮转、报错给用户；
    /// * 把非信封判成「是信封」→ IP 拦截被当成账号问题 → 继续放大风控（本缺陷本身）。
    ///
    /// 两者都要避免，故边界值逐个钉住。
    #[test]
    fn is_business_envelope_table() {
        let cases: &[(&str, bool)] = &[
            (r#"{"code":0,"msg":"ok"}"#, true),
            (r#"{"code":3101}"#, true),            // msg 可缺
            (r#"  {"code":1,"msg":"x"}  "#, true), // 前后空白
            (r#"{"error":{"message":"x"}}"#, true), // OpenAI 形状
            (r#"{"error":{"code":"403"}}"#, true), //
            (r#"{"error":"Forbidden"}"#, false),   // error 是字符串：边缘节点写法
            (r#"{"waf":"blocked"}"#, false),       // 无 code / error 对象
            (r#"{"msg":"forbidden"}"#, false),     // 只有 msg：不是统一信封
            ("", false),                           // 空体
            ("   ", false),                        // 全空白
            ("<html></html>", false),              // HTML
            (r#"["a"]"#, false),                   // 数组
            (r#""str""#, false),                   // 标量
            ("{not json", false),                  // 坏 JSON
            (r#"{"code":0,"msg":"ok""#, false),    // 截断的 JSON
            ("null", false),                       //
        ];
        for (body, want) in cases {
            assert_eq!(
                is_business_envelope(body),
                *want,
                "is_business_envelope({body:?}) 期望 {want}"
            );
        }
    }

    /// 单词边界判定的边界值。
    ///
    /// 这一层是防误判的最后一道：`is_waf_forbidden` 在 403 路径上，
    /// 一次误判会让整个网关停止轮转，故裸 `contains` 不能用。
    #[test]
    fn contains_word_word_boundary() {
        let cases: &[(&str, &str, bool)] = &[
            ("request blocked by waf", "waf", true),
            ("waf", "waf", true),
            ("waf blocked", "waf", true),
            ("blocked by waf.", "waf", true), // 句号结尾 = 边界
            ("[waf]", "waf", true),           // 方括号 = 边界
            ("waf拦截了本次请求", "waf", true), // 汉字算边界：中文不用空格分词，必须能认出
            ("由waf拦截", "waf", true),        // 同理
            ("wafer quota", "waf", false),    // 子串但不是单词
            ("dwarf plan", "waf", false),     //
            ("swift", "waf", false),          //
            ("", "waf", false),               //
            ("no marker here", "waf", false), //
            // 多次出现时不能只看第一处：第一处是子串、第二处是独立单词 → 应命中。
            ("wafer and waf", "waf", true),
            // 下划线是单词字符（Go `isWordByte` 含 `_`）。
            ("waf_blocked", "waf", false),
            ("waf_blocked and waf", "waf", true),
        ];
        for (s, word, want) in cases {
            assert_eq!(
                contains_word(s, word),
                *want,
                "contains_word({s:?}, {word:?}) 期望 {want}"
            );
        }
    }

    /// 错误文案必须说清「换号无用」与出路。
    ///
    /// 这是本缺陷修复的**用户可见**部分：此前用户只看到
    /// 「all accounts unavailable (cooling/disabled)」，会去查账号池 ——
    /// 而真正要改的是网络出口。
    #[test]
    fn egress_message_readable() {
        let msg = egress_ip_blocked_message(2);
        for want in ["WAF", "不同账号", "出口 IP", "换号无用", "代理"] {
            assert!(msg.contains(want), "文案应包含 {want:?}，实际 {msg:?}");
        }
        assert!(msg.contains("60 秒"), "文案应说明窗口长度（60 秒），实际 {msg:?}");
        // 数量要随实际值变化，不能写死。
        assert!(
            egress_ip_blocked_message(3).contains("3 个"),
            "文案应带上实际的不同账号数，实际 {:?}",
            egress_ip_blocked_message(3)
        );
        // ★ 文案里不得出现裸 403/401：客户端按文案正则判 AUTH（见 clienttext.rs）。
        for code in ["403", "401"] {
            assert!(
                !msg.contains(code),
                "文案不该出现裸 {code}（会被客户端读成「API 密钥无效」）：{msg:?}"
            );
        }
    }
}
