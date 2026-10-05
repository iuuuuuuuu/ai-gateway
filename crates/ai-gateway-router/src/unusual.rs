//! unusual.rs 上游 3012「unusual activity」的**产品级熔断**：识别出来就别再换号了。
//!
//! 不同产品的熔断状态严格隔离：ZCode 的 3012 不得阻断 Qoder/WorkBuddy。
//!
//! 逐字移植 Go 侧 `internal/server/unusual.go`（339 行），常量、判据、状态机
//! 与文案一字不改。
//!
//! # 要解决的问题（所有者 2026-09-21 明确要求）
//!
//! 所有者原话：
//!
//! > 「2」（选择：加"ZCode 请求频率限制"或"3012 后自动退避一段时间不重试"）
//! > 「就在刚刚这个版本又报错了」（同一晚遇到 3012）
//!
//! # 旧行为为什么会**放大**风控
//!
//! 3012 的 HTTP 载体是 405，而 `upstream.Classify` 里 405 落进通用
//! `ErrClient`（它只对 429/402/404/5xx/若干业务码做了特殊归类，405 不在其中）。
//! `applyErrorPolicy` 对 `ErrClient` 的处理是「只换号不罚，防雪崩」——
//! 于是网关会**把池里 21 个账号挨个打一遍**，每个都吃一次 3012。
//!
//! 对上游风控而言这正是最坏形状：**同一出口 IP 在极短时间内用一批不同凭证
//! 反复触发同一规则**。这与 `wafip.rs` 里记的那条完全同构。
//!
//! # 判据：3012 是**请求级/瞬时**风控，不是账号问题
//!
//! 这一条已经查实（证据在 `zcode/provider.go` 的文件头）：
//!
//! * 同一账号 07:01 是 3012、07:02 就成功
//! * **官方客户端自己也吃 3012**（日志 `turn.failed`，405 code=3012）
//! * 换端点 / 改请求形状 / 三种 deviceMid / system prompt A/B —— 全部被证伪
//!
//! ⇒ 既然与账号无关，**换号就是纯浪费**，而且每次浪费都往风控上再添一笔。
//!
//! # 与 `wafip.rs` 的分工（两者判据不同，不要合并）
//!
//! ```text
//! wafip.rs   判据 = **多个不同账号**都吃 403 ⇒ 出口 IP 被拦
//! unusual.rs 判据 = **单个**账号吃到 3012     ⇒ 上游对本次请求的风控
//! ```
//!
//! # 为什么用滑动窗口而不是永久标记
//!
//! 3012 是**有时长效期**的风控（会自己过期）。永久标记会让网关在解封后
//! 仍然拒绝服务，把「临时风控」变成「我们自己造成的永久故障」。

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// 3012 熔断窗口：这段时间内**不再尝试**上游。
///
/// # 取值依据
///
/// 已知的实测恢复尺度：
///
/// * `provider.go` 记录：同一账号 07:01 是 3012、07:02 就成功（**1 分钟**）
/// * 所有者本机更早一次现场：约 1.5 小时才整体恢复
///
/// 两者差两个数量级，说明它**没有固定恢复时间**。故取一个折中：
/// 90 秒 —— 比「1 分钟就恢复」稍长（避免刚过窗口又立刻撞上），又远小于
/// 「1.5 小时」（那种情况等多久都没用，不如让请求快速失败）。
///
/// ⚠ 这个值是**保守猜测**，不是实测出来的最优点。选它的理由是
/// 「宁可少打几次」：熔断短了只是多撞一次风控，长了才影响可用性。
pub const UNUSUAL_WINDOW: Duration = Duration::from_secs(90);

/// 连续多轮都撞 3012 时，把窗口翻倍的上限。
///
/// 为什么要翻倍：若 90 秒后仍然 3012，说明这次风控持续得比典型情况长。
/// 固定 90 秒会让网关每 90 秒去撞一次，对风控而言是**稳定的周期性行为**
/// —— 比随机重试更像脚本。翻倍 + 上限让它逐步退让而不是规律叩门。
pub const UNUSUAL_MAX_WINDOW: Duration = Duration::from_secs(30 * 60);

/// 便捷方法默认作用的产品。
///
/// 与 Go 的 `forProduct("workbuddy")` 同口径，也与 `auth::PRODUCT_WORKBUDDY`
/// 同值（此处写字面量，让本模块不依赖 `crate::auth`，可独立编译与测试）。
const DEFAULT_PRODUCT: &str = "workbuddy";

/// 3012 响应体的识别特征（小写比较）。
///
/// 上游原文（实测）：
///
/// ```text
/// HTTP 405 {"code":3012,"msg":"request has been blocked due to unusual activity."}
/// ```
///
/// 认**业务码 + 文案**两重，任一命中即可：文案可能被上游改写，
/// 而 code 是稳定的；反过来某些通道可能只回文案不带 code。
const UNUSUAL_ACTIVITY_MARKERS: &[&str] = &[
    "unusual activity",
    "request has been blocked due to unusual activity",
];

/// 时钟注入点：生产用 [`Instant::now`]，单测注入假时钟以断言窗口行为。
///
/// Go 侧是 `now func() time.Time` 字段。这里同样把时钟做成**实例字段**
/// 而不是全局可替换的东西 —— 测试各造各的熔断器，互不干扰。
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

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<clock>")
    }
}

/// 取锁并**忽略中毒**（`Mutex` 中毒是 Rust 特有的机制，Go 没有）。
///
/// 用 `unwrap_or_else(|e| e.into_inner())` 而不是 `unwrap()`：一个用例里
/// 断言失败会让锁中毒，若继续 `unwrap()` 则**后续所有用例**都会连锁 panic，
/// 把「一处断言失败」放大成「整片红灯」，反而看不出真正的问题。
fn lock_or_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 单一产品的 3012 熔断状态。
///
/// 3012 是上游产品接口返回的业务风控码，**不能跨产品传播**：
/// zcode 的 3012 只说明 ZCode 上游当前拒绝请求，不能阻断 Qoder/WorkBuddy。
/// 同一产品内仍跨账号共享，因为换号会放大同一个上游/出口的风控。
#[derive(Debug, Clone)]
pub struct UnusualBreaker {
    state: Arc<Mutex<BreakerState>>,
    now: Clock,
}

#[derive(Debug)]
struct BreakerState {
    /// 熔断截止；`None` 或已过 = 不熔断。
    until: Option<Instant>,
    /// 当前窗口长度（连续命中会翻倍，成功后复位）。
    window: Duration,
    /// 累计命中次数（仅供诊断/文案，不影响判定）。
    hits: u64,
}

impl UnusualBreaker {
    fn new() -> Self {
        Self::with_clock(Clock::system())
    }

    fn with_clock(now: Clock) -> Self {
        Self {
            state: Arc::new(Mutex::new(BreakerState {
                until: None,
                window: UNUSUAL_WINDOW,
                hits: 0,
            })),
            now,
        }
    }

    /// 报告当前是否处于 3012 熔断中，以及还需多久。
    ///
    /// 调用方（`forward.rs` 的轮转循环）在**发起任何上游请求之前**先问它：
    /// 熔断期间直接失败，不再换号 —— 这正是本模块要达成的效果。
    pub fn tripped(&self) -> Option<Duration> {
        let now = self.now.now();
        let st = lock_or_recover(&self.state);
        match st.until {
            // Go：`if b.until.IsZero() || !now.Before(b.until) { return false, 0 }`
            Some(until) if now < until => Some(until - now),
            _ => None,
        }
    }

    /// 记录一次 3012，并开启（或延长）熔断窗口。
    ///
    /// # 为什么命中就「延长」而不是「忽略重复」
    ///
    /// 与 `wafip.rs` 的 `note_waf` 不同：那个判据是「不同账号数」，同一账号
    /// 重复命中不该增加计数（那是账号问题）。而 3012 的判据是「上游正在风控」
    /// —— 每一次命中都说明**此刻仍在风控中**，故重复命中应当把窗口往后推
    /// （并以翻倍方式退让）。
    ///
    /// Go 版本返回本次熔断的截止时刻供调用方写日志；Rust 侧签名固定为
    /// `()`，调用方要写日志时自行用 [`Self::tripped`] 取剩余时长。
    pub fn note3012(&self) {
        let now = self.now.now();
        let mut st = lock_or_recover(&self.state);
        st.hits += 1;
        // 连续命中（上次熔断还没结束就又中）⇒ 窗口翻倍，逐步退让。
        match st.until {
            Some(until) if now < until => {
                // 翻倍后立刻封顶，故中间值最多到 `UNUSUAL_MAX_WINDOW * 2`，
                // 不存在溢出风险（`Duration * u32` 溢出会 panic）。
                st.window *= 2;
                if st.window > UNUSUAL_MAX_WINDOW {
                    st.window = UNUSUAL_MAX_WINDOW;
                }
            }
            _ => {
                if st.window != UNUSUAL_WINDOW {
                    // 上次熔断**已过期**后才再次命中：这是「新一轮风控」，
                    // 窗口从基准重新开始 —— 否则一次长风控会把窗口永久顶在
                    // 上限，之后每次瞬时抖动都要用户等 30 分钟。
                    st.window = UNUSUAL_WINDOW;
                }
            }
        }
        st.until = Some(now + st.window);
    }

    /// 一次上游成功即**清除**熔断。
    ///
    /// 为什么成功要立刻复位：它证明上游此刻不再风控。继续熔断会让用户在一次
    /// 短暂抖动之后仍然被自己的网关挡在门外 —— 那是我们制造的故障。
    pub fn note_success(&self) {
        let mut st = lock_or_recover(&self.state);
        st.until = None;
        st.window = UNUSUAL_WINDOW;
        st.hits = 0;
    }
}

/// 按产品隔离 3012 熔断：zcode / qoder / workbuddy。
///
/// 空产品归一为 workbuddy，兼容历史 WorkBuddy 凭证（与 `auth::ProductOf` 同口径）。
///
/// 用 `Arc<Mutex<..>>` 包每一个熔断器，使 [`Self::for_product`] 能把同一产品的
/// 熔断器**按值交给**调用方（Go 侧返回指针），同时保持跨调用共享状态。
#[derive(Debug, Default)]
pub struct UnusualBreakers {
    by_product: Mutex<HashMap<String, UnusualBreaker>>,
}

impl UnusualBreakers {
    pub fn new() -> Self {
        Self {
            by_product: Mutex::new(HashMap::new()),
        }
    }

    /// 取某产品的熔断器（不存在则创建）。`product` 为空时归一成 `"workbuddy"`。
    pub fn for_product(&self, product: &str) -> UnusualBreaker {
        let product = if product.is_empty() {
            DEFAULT_PRODUCT
        } else {
            product
        };
        let mut map = lock_or_recover(&self.by_product);
        map.entry(product.to_string())
            .or_insert_with(UnusualBreaker::new)
            .clone()
    }

    /// 默认委托 [`Self::for_product`]`("workbuddy")`，与 Go 的便捷方法同义。
    pub fn tripped(&self) -> Option<Duration> {
        self.for_product(DEFAULT_PRODUCT).tripped()
    }

    /// 默认委托 [`Self::for_product`]`("workbuddy")`。
    pub fn note3012(&self) {
        self.for_product(DEFAULT_PRODUCT).note3012();
    }

    /// 默认委托 [`Self::for_product`]`("workbuddy")`。
    pub fn note_success(&self) {
        self.for_product(DEFAULT_PRODUCT).note_success();
    }

    /// 清空所有产品的熔断状态（Go `(*unusualBreakers).reset`）。
    ///
    /// 生产路径不需要它；留给测试隔离用例之间的状态，以及将来「手动解除风控」
    /// 这类运维动作。
    pub fn reset(&self) {
        lock_or_recover(&self.by_product).clear();
    }
}

/// 报告一次响应是否为 3012 风控（Go `isUnusualActivity`）。
///
/// 判据（任一成立）：
///
/// 1. 响应体里出现独立的业务码 3012；
/// 2. 响应体含 `"unusual activity"` 文案。
///
/// ⚠ 为什么还要看状态码：3012 的载体实测是 **405**（不是 4xx 里常见的
/// 403/429），而 405 在网关里还有别的可能来源（方法不允许）。故要求状态码是
/// 4xx 或 5xx —— 一个 HTTP 200 的正常回复里恰好含这些词（比如用户在聊风控）
/// 不该触发熔断。
pub fn is_unusual_activity(status: u16, body: &str) -> bool {
    if status < 400 {
        return false;
    }
    if has_bare_code(body, "3012") {
        return true;
    }
    let lower = body.to_lowercase();
    UNUSUAL_ACTIVITY_MARKERS
        .iter()
        .any(|m| lower.contains(m))
}

/// 报告响应体里的业务码**恰好等于** `code`（Go `hasBareCode`）。
///
/// # ⚠ 这里必须解析 JSON，不能做子串匹配（Go 侧自己写的测试抓出了两个 bug）
///
/// 第一版是「`strings.Contains(body, "\"code\":" + code)` 加两种引号变体」。
/// 测试立刻红了：
///
/// ```text
/// 带空格   {"code": 3012, ...}   → **漏判**（上游美化输出就读不出来）
/// 别的码   {"code":30120, ...}   → **误判**（"code":3012 是 "code":30120 的前缀！）
/// ```
///
/// 误判更糟：它会因为一个**无关的业务码**把整个网关熔断 90 秒，表现为
/// 「偶发地一大段时间全都不可用」，而用户完全看不出与 30120 有关。
///
/// 改成解析 JSON 后两种情况都自然正确 —— 而且不必再枚举引号/空格变体，
/// 那本身就是「用字符串凑 JSON」的征兆。
///
/// 兜底：JSON 解析失败时（上游偶尔回非标准体）退回**带边界的精确匹配**
/// （`"code":3012` 后必须跟非数字字符，或完整的 `"code": "3012"`），
/// 但绝不用裸子串 —— 那正是 30120 误判的来源。
fn has_bare_code(body: &str, code: &str) -> bool {
    let t = body.trim();
    if t.is_empty() {
        return false;
    }
    // 主路径：正经 JSON。
    if let Ok(serde_json::Value::Object(doc)) = serde_json::from_str::<serde_json::Value>(t) {
        return match doc.get("code") {
            Some(raw) => json_code_equals(raw, code),
            None => false,
        };
    }
    // 兜底：非 JSON 体。用带边界的匹配，避免 `30120` 命中 `3012`。
    let tb = t.as_bytes();
    for shape in [
        format!("\"code\":{code}"),
        format!("\"code\": \"{code}\""),
    ] {
        let Some(i) = t.find(&shape) else {
            continue;
        };
        if shape.ends_with('"') {
            return true; // 已是完整的带引号形式
        }
        // 裸数字形式：后一个字符不能是数字（否则 30120 会被当成 3012）
        let j = i + shape.len();
        if j >= tb.len() {
            return true;
        }
        if !tb[j].is_ascii_digit() {
            return true;
        }
    }
    false
}

/// 判断 JSON 里的 `code` 字段是否等于期望值（数字或字符串两种形态）。
///
/// Go 侧是 `strings.Trim(string(raw), "\"") == want`（`raw` 是原始 JSON 文本）。
/// 这里按 [`serde_json::Value`] 的形态判：数字取其十进制文本、字符串取其内容，
/// 其余形态（`null` / 布尔 / 对象 / 数组）一律不等 —— 与 Go 逐字同结果
/// （`3012.0` 在 Go 里是 `"3012.0" != "3012"`，这里同样不等）。
fn json_code_equals(raw: &serde_json::Value, want: &str) -> bool {
    match raw {
        serde_json::Value::Number(n) => n.to_string() == want,
        serde_json::Value::String(s) => s == want,
        _ => false,
    }
}

/// 生成 3012 熔断期间给客户端的文案（Go `unusualActivityMessage`）。
///
/// 必须说清三件事（缺一件用户就会走错方向）：
///
/// ```text
/// 现象 —— 上游回了 3012「unusual activity」
/// 结论 —— 这是上游风控，**与账号无关**（所以换号无用）
/// 出路 —— 等窗口过去，或换模型；网关已停止重试以免加重风控
/// ```
///
/// ⚠ 刻意不说「账号被封」：实测同一账号一分钟后就成功，且官方客户端也吃它。
/// 说成封号会把用户引向「去换账号」这个**完全无效**的方向。
pub fn unusual_activity_message(wait: Duration) -> String {
    // Go：`int(wait.Round(time.Second) / time.Second)`，再 `< 1` 抬到 1。
    let mut secs = wait.as_secs_f64().round() as i64;
    if secs < 1 {
        secs = 1;
    }
    format!(
        "上游风控（3012 unusual activity）：这次拒绝是**上游对本次请求**的判定，\
与账号无关 —— 实测同一账号一分钟后即可用，且官方客户端也会被同样拒绝。\
网关已**停止换号重试**（继续试只会加重风控），约 {secs} 秒后自动恢复。\
可换其它模型先用，或稍后重试。"
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
    /// 90 秒 —— Go 侧的做法同样是注入假时钟，见 `unusual_test.go` 的
    /// `newTestBreaker`）。
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

    fn test_breaker(clock: &Arc<FakeClock>) -> UnusualBreaker {
        UnusualBreaker::with_clock(clock.clock())
    }

    // ---- ① 状态机 ----

    #[test]
    fn breaker_trips_and_expires() {
        let clk = FakeClock::new();
        let b = test_breaker(&clk);

        assert!(
            b.tripped().is_none(),
            "初始不该熔断（未熔断时必须返回 None，调用方据此放行）"
        );

        b.note3012();
        let wait = b.tripped().expect("命中 3012 后应熔断");
        assert!(wait > Duration::ZERO, "熔断剩余时长应为正，实际 {wait:?}");
        assert!(wait <= UNUSUAL_WINDOW, "首次熔断窗口不该超过基准窗口");

        // 过了窗口 ⇒ 自动恢复（不靠任何人工干预）
        clk.advance(UNUSUAL_WINDOW + Duration::from_secs(1));
        assert!(
            b.tripped().is_none(),
            "窗口过后应自动恢复 —— 3012 是临时的，不能变成我们自己造成的永久故障"
        );
    }

    #[test]
    fn breaker_escalates_on_repeated_hits() {
        let clk = FakeClock::new();
        let b = test_breaker(&clk);

        b.note3012();
        let first = lock_or_recover(&b.state).window;

        // 窗口内再次命中 ⇒ 窗口翻倍（逐步退让，而不是每 90 秒规律叩门）
        clk.advance(Duration::from_secs(10));
        b.note3012();
        let second = lock_or_recover(&b.state).window;
        assert!(
            second > first,
            "连续命中应放大窗口：首次 {first:?}，再次 {second:?}"
        );
        assert_eq!(second, UNUSUAL_WINDOW * 2, "第二次应正好翻倍");
    }

    #[test]
    fn breaker_resets_window_after_expiry() {
        let clk = FakeClock::new();
        let b = test_breaker(&clk);

        // 连续命中把窗口顶到上限
        for _ in 0..12 {
            clk.advance(Duration::from_secs(1));
            b.note3012();
        }
        let capped = lock_or_recover(&b.state).window;
        assert!(
            capped > UNUSUAL_WINDOW,
            "前置条件不成立：窗口应已放大，实际 {capped:?}"
        );

        // 窗口**过期之后**才再次命中 ⇒ 这是「新一轮风控」，窗口回到基准
        clk.advance(capped + Duration::from_secs(60));
        b.note3012();
        let reset = lock_or_recover(&b.state).window;
        assert_eq!(
            reset, UNUSUAL_WINDOW,
            "过期后重新命中应把窗口复位到基准 —— 否则一次长风控会把窗口永久顶在上限，\
             之后每次瞬时抖动都要用户等很久"
        );
    }

    #[test]
    fn breaker_window_capped() {
        let clk = FakeClock::new();
        let b = test_breaker(&clk);
        for _ in 0..40 {
            clk.advance(Duration::from_secs(1));
            b.note3012();
        }
        let w = lock_or_recover(&b.state).window;
        assert!(
            w <= UNUSUAL_MAX_WINDOW,
            "窗口不得超过上限 {UNUSUAL_MAX_WINDOW:?}，实际 {w:?}"
        );
    }

    #[test]
    fn breaker_success_clears() {
        let clk = FakeClock::new();
        let b = test_breaker(&clk);
        b.note3012();
        assert!(b.tripped().is_some(), "前置条件：应已熔断");

        b.note_success();
        assert!(
            b.tripped().is_none(),
            "上游成功应立刻清除熔断 —— 否则用户在一次短暂抖动后仍被自己网关挡在门外"
        );
        assert_eq!(
            lock_or_recover(&b.state).window,
            UNUSUAL_WINDOW,
            "成功应把窗口复位到基准"
        );
    }

    // ---- ② ★★ 产品隔离（Go unusual_test.go:306-314 的等价用例） ----

    /// 3012 只熔断命中产品，不得阻断其它产品。
    ///
    /// 这是用户现场的核心回归：`zcode:glm-5.3` 失败后，Qoder/WorkBuddy
    /// 仍应可以发起请求，不能因为包级全局 breaker 一起进入风控等待。
    #[test]
    fn unusual_activity_isolated_by_product() {
        let b = UnusualBreakers::new();
        let z = b.for_product("zcode");
        let q = b.for_product("qoder");
        let w = b.for_product("workbuddy");

        z.note3012();
        assert!(z.tripped().is_some(), "ZCode 命中 3012 后应熔断");
        assert!(q.tripped().is_none(), "qoder 不应被 ZCode 的 3012 熔断影响");
        assert!(
            w.tripped().is_none(),
            "workbuddy 不应被 ZCode 的 3012 熔断影响"
        );

        q.note3012();
        assert!(
            w.tripped().is_none(),
            "WorkBuddy 不应被 Qoder 的 3012 熔断影响"
        );
    }

    /// 同一产品必须拿到**同一个**熔断器（产品内跨账号共享）。
    ///
    /// 与上一条互补：那条证明「不跨产品」，这条证明「产品内确实共享」——
    /// 若 `for_product` 每次都新建，则换号就会绕过熔断，本模块直接失效。
    #[test]
    fn for_product_shares_state_within_a_product() {
        let b = UnusualBreakers::new();
        b.for_product("zcode").note3012();
        assert!(
            b.for_product("zcode").tripped().is_some(),
            "同一产品必须拿到同一个熔断器（跨账号共享）"
        );
        assert!(
            b.for_product("qoder").tripped().is_none(),
            "不同产品必须拿到不同的熔断器"
        );
    }

    /// 空产品名归一成 `workbuddy`（与 `for_product("workbuddy")` 看到同一个）。
    #[test]
    fn empty_product_normalises_to_workbuddy() {
        let b = UnusualBreakers::new();
        let empty = b.for_product("");
        let named = b.for_product("workbuddy");
        empty.note3012();
        assert!(
            named.tripped().is_some(),
            "空产品名必须归一成 workbuddy（否则历史 WorkBuddy 凭证会拿到一个独立熔断器）"
        );
        assert!(
            b.tripped().is_some(),
            "便捷方法也必须看到 workbuddy 的熔断（默认委托 for_product(\"workbuddy\")）"
        );

        // 反向：便捷方法记的 3012，空产品名也要看得到。
        let b2 = UnusualBreakers::new();
        b2.note3012();
        assert!(b2.for_product("").tripped().is_some());
        assert!(b2.for_product("workbuddy").tripped().is_some());
    }

    #[test]
    fn convenience_methods_delegate_to_workbuddy() {
        let b = UnusualBreakers::new();
        b.note3012();
        assert!(b.tripped().is_some(), "note3012 后 tripped 应有值");
        b.note_success();
        assert!(b.tripped().is_none(), "note_success 后 tripped 应为 None");
        assert!(
            b.for_product("qoder").tripped().is_none(),
            "便捷方法只该动 workbuddy，不该顺手熔断别的产品"
        );
    }

    #[test]
    fn reset_clears_every_product() {
        let b = UnusualBreakers::new();
        b.for_product("zcode").note3012();
        b.for_product("qoder").note3012();
        b.reset();
        assert!(b.for_product("zcode").tripped().is_none());
        assert!(b.for_product("qoder").tripped().is_none());
        assert!(b.tripped().is_none());
    }

    // ---- ③ 判据 ----

    #[test]
    fn recognises_real_3012_response() {
        // 实测原文（zcode/provider.go 与 captcha.go 都记着这一条）
        let body = r#"{"code":3012,"msg":"request has been blocked due to unusual activity."}"#;
        assert!(is_unusual_activity(405, body), "实测的 3012 原文应被识别");
    }

    #[test]
    fn marker_forms() {
        let cases: &[(&str, u16, &str, bool, &str)] = &[
            (
                "原样",
                405,
                r#"{"code":3012,"msg":"request has been blocked due to unusual activity."}"#,
                true,
                "实测形态",
            ),
            (
                "只有文案",
                405,
                r#"{"msg":"Request has been blocked due to unusual activity."}"#,
                true,
                "code 可能缺失",
            ),
            (
                "大写文案",
                403,
                r#"{"message":"UNUSUAL ACTIVITY detected"}"#,
                true,
                "大小写不敏感",
            ),
            (
                "code 是字符串",
                405,
                r#"{"code":"3012","msg":"blocked"}"#,
                true,
                "某些通道把码序列化",
            ),
            (
                "带空格",
                405,
                r#"{"code": 3012, "msg":"blocked"}"#,
                true,
                "上游可能美化输出",
            ),
            // ---- 不该误判的 ----
            (
                "正常回复",
                200,
                r#"{"choices":[{"message":{"content":"3012 端口"}}]}"#,
                false,
                "HTTP 200 的正文里恰好含 3012 —— 不该熔断",
            ),
            (
                "别的业务码",
                400,
                r#"{"code":30120,"msg":"other"}"#,
                false,
                "30120 不是 3012（裸 Contains 会误命中）",
            ),
            (
                "token 计数含 3012",
                200,
                r#"{"usage":{"prompt_tokens":3012}}"#,
                false,
                "用量数字恰好是 3012",
            ),
            (
                "普通 4xx",
                400,
                r#"{"code":1001,"msg":"bad request"}"#,
                false,
                "无关错误",
            ),
            (
                "空体",
                405,
                "",
                false,
                "空体没有 3012 证据（405 本身不足以判定）",
            ),
        ];
        for (name, status, body, want, why) in cases {
            assert_eq!(
                is_unusual_activity(*status, body),
                *want,
                "{name}：is_unusual_activity({status}, {body:?}) 期望 {want}（{why}）"
            );
        }
    }

    /// 成功状态码一律不判。
    ///
    /// 用户在对话里聊「3012」是完全可能的；把那种正常回复判成风控会让网关
    /// 无故熔断（表现为「聊天聊到一半突然全不可用」）。
    #[test]
    fn ignores_success_status() {
        for s in [200u16, 201, 204, 301, 302] {
            assert!(
                !is_unusual_activity(s, r#"{"code":3012,"msg":"unusual activity"}"#),
                "状态码 {s} 不该触发熔断"
            );
        }
    }

    /// 非 JSON 体走**带边界**的兜底匹配，不能把 30120 当成 3012。
    #[test]
    fn has_bare_code_fallback_is_bounded() {
        assert!(has_bare_code(r#"prefix "code":3012 suffix"#, "3012"));
        assert!(has_bare_code(r#"prefix "code": "3012" suffix"#, "3012"));
        assert!(
            !has_bare_code(r#"prefix "code":30120 suffix"#, "3012"),
            "30120 不得命中 3012（裸子串匹配的经典误判）"
        );
        assert!(!has_bare_code("", "3012"));
        assert!(!has_bare_code("   ", "3012"));
        // JSON 主路径：数字 / 字符串两种形态都认，其它形态都不认。
        assert!(has_bare_code(r#"{"code":3012}"#, "3012"));
        assert!(has_bare_code(r#"{"code":"3012"}"#, "3012"));
        assert!(has_bare_code(r#"{"code": 3012, "msg":"x"}"#, "3012"));
        assert!(!has_bare_code(r#"{"code":30120}"#, "3012"));
        assert!(!has_bare_code(r#"{"code":3012.0}"#, "3012"));
        assert!(!has_bare_code(r#"{"code":null}"#, "3012"));
        assert!(!has_bare_code(r#"{"msg":"3012"}"#, "3012"));
        assert!(!has_bare_code(r#"["3012"]"#, "3012"));
    }

    // ---- ④ 文案 ----

    /// 文案必须说清「与账号无关、换号无用」。
    ///
    /// 历史上这个错误被包装成 `no_healthy_account` +
    /// 「all accounts unavailable (cooling/disabled)」，把所有者引向
    /// 「去查账号池 / 换账号」这个**完全无效**的方向。
    #[test]
    fn message_says_not_account() {
        let msg = unusual_activity_message(UNUSUAL_WINDOW);
        for want in ["3012", "与账号无关", "停止换号"] {
            assert!(msg.contains(want), "文案应含 {want:?}，实际：{msg}");
        }
        // 必须给出剩余秒数（用户据此决定等还是换模型）
        assert!(msg.contains("90"), "文案应含等待秒数 90，实际：{msg}");
        // 不该说成封号 —— 那会把用户引向「换账号」
        for bad in ["账号被封", "封禁", "账号不可用"] {
            assert!(!msg.contains(bad), "文案不该出现 {bad:?}，实际：{msg}");
        }
    }

    #[test]
    fn message_rounds_up_sub_second() {
        // 剩余不足 1 秒时不能显示「约 0 秒」（看起来像没生效）
        let msg = unusual_activity_message(Duration::from_millis(200));
        assert!(
            !msg.contains("约 0 秒"),
            "不足 1 秒应向上取整为 1，实际：{msg}"
        );
        assert!(msg.contains("约 1 秒"), "实际：{msg}");
    }

    #[test]
    fn message_rounds_to_nearest_second() {
        // Go 的 `wait.Round(time.Second)` 是「就近取整」，不是截断。
        assert!(unusual_activity_message(Duration::from_millis(1600)).contains("约 2 秒"));
        assert!(unusual_activity_message(Duration::from_millis(1400)).contains("约 1 秒"));
    }
}
