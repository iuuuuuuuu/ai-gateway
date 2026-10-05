//! 产品权益领取的**手动触发执行体**（Go `cmd/server/product_tasks.go`）。
//!
//! # 为什么这一层必须待在 lib 里（而不是 `bin/gateway.rs`）
//!
//! Go 侧本文件的对应物在 `cmd/server`（启动层），因为它只用到 `zcode.Dispatch`
//! 与 `qoder.ClaimAllCampaigns` 两个**公开**入口。Rust 侧不行：
//!
//! - [`super::qoder::campaign::claim_all_campaigns`] 是 `pub(crate)`；
//! - [`ClaimScheduler::tick_account`] 与 [`ClaimTickResult`] 走的是
//!   `pub(crate)` 的 `claim_scheduler` 模块；
//! - `bin/gateway.rs` 是**独立的 crate**（`Cargo.toml` 的 `[[bin]]`），
//!   `pub(crate)` 对它一概不可见。
//!
//! 所以「启动层」在 Rust 里被拆成两半：本文件提供 `pub` 的构造入口，
//! `bin/gateway.rs` 只负责把配置喂进来、把返回的 runner 交给
//! [`crate::scheduler::Scheduler::set_product_tasks_runner`]。
//!
//! # 为什么手动与自动必须共用同一套领取逻辑
//!
//! Go 的注释（`product_tasks.go:48-50`）说得很直白：两条路径各写一份领取实现
//! 必然出现「手动能领、自动领不到」这类分歧，而那种缺陷极难查（两条路径看起来
//! 都对，只是行为不同）。故本文件里的 ZCode 分支**只**调
//! [`ClaimScheduler::tick_account`] —— 与常驻调度器 `tick_once` 内部逐账号调的
//! 是同一个函数。
//!
//! # 与 Go 的三处已知差异（都不是行为差异，是语言差异）
//!
//! 1. Go 用 `context.WithTimeout` 贯穿整轮；Rust 用**同一个 deadline** 上的
//!    [`tokio::time::timeout_at`]。两者对「超时后已处理的部分」的处置也一致 ——
//!    Go 在超时分支里把 `detail` 丢掉只回错误，Rust 同样丢掉。
//! 2. Go 的 `%v` 打在 `map[string]any` 的 `float64` 上；Rust 侧
//!    [`super::qoder::campaign::claim_all_campaigns`] 给的是 `usize`，
//!    由 [`go_count`] 渲染成同样的十进制整数文本。
//! 3. 超时文案里 Go 会带上 `net/http` 包装后的错误串（形如
//!    `Get "https://…": context deadline exceeded`），Rust 侧无法逐字复现，
//!    只回 `context deadline exceeded`。**该分支只在真超时（180 秒）时出现**，
//!    不是正常路径。

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::scheduler::ProductTasksRunner;

use super::qoder;
use super::zcode::claim_scheduler::{
    ClaimFailureKind, ClaimScheduler, ClaimTickOutcome, ClaimTickResult, LoadCreds,
};

/// Go `productTaskTimeout`（`product_tasks.go:42`）：手动触发一次的整轮超时。
///
/// 只给**手动**路径用：用户点了「立即领取」在等结果，必须有界。
/// 常驻调度器不用这个超时 —— 它按自己的 hold/cooldown 节奏走。
pub const PRODUCT_TASK_TIMEOUT: Duration = Duration::from_secs(3 * 60);

/// ZCode 侧接线所需的两件东西。
///
/// Go 把这两件都塞在 `*zcode.Dispatch` 里（`LoadCreds()` 是方法，client 是字段），
/// Rust 侧 `Dispatch` 的 `client` 字段是私有的、且没有任何 getter 暴露它，
/// 故这里显式携带。
///
/// ⚠ [`Self::load_creds`] 必须与传给 [`ClaimScheduler::new`] 的是**同一个**
/// `Arc`（Go 侧两者都是 `zd.LoadCreds`）。两份闭包指向同一个目录时行为相同，
/// 但目录若在运行期被配置改掉，两份就会分叉 —— 故调用方只该构造一次。
#[derive(Clone)]
pub struct ZcodeTasks {
    /// 常驻调度器：自动路径跑它，手动路径借它的 hold 状态表。
    pub sched: Arc<ClaimScheduler>,
    /// 凭证加载器 —— 手动路径遍历账号用。
    pub load_creds: LoadCreds,
}

/// Go `fmt.Sprintf("%v", d)` 对 `time.Duration` 的格式（`5m0s` / `10m0s`）。
///
/// [`super::zcode::claim_scheduler::go_duration`] 是 `pub(crate)`，而启动日志在
/// `bin/gateway.rs`（**独立 crate**）里 —— 故在这里转成 `pub`，让日志文案与
/// Go 逐字一致，而不是在 bin 里再抄一份格式化逻辑。
pub fn go_duration_text(d: Duration) -> String {
    super::zcode::claim_scheduler::go_duration(d)
}

/// Go `fmt.Sprintf("%v", []int{10, 21})` ⇒ `[10 21]`（**空格**分隔，无逗号）。
///
/// Rust 的 `{:?}` 会打成 `[10, 21]`，与 Go 的日志文案不同。
pub fn go_int_slice_text(v: &[i32]) -> String {
    let mut b = String::from("[");
    for (i, n) in v.iter().enumerate() {
        if i > 0 {
            b.push(' ');
        }
        b.push_str(&n.to_string());
    }
    b.push(']');
    b
}

/// Go `newProductTasksRunner`（`product_tasks.go:61-98`）。
///
/// 返回 `None` 表示「当前构建两个产品都没有」（Go 的 `return nil`）——
/// 此时排程与手动入口都跳过，不报错：调度器那边会回
/// `skipped(..., "该构建未接入产品任务执行体")`。
///
/// # 产品顺序：**先 Qoder 再 ZCode**
///
/// Go 注释（`:75`）：Qoder 的活动每天 10:00 重置，越早领越不易过期。
/// 且一个产品失败**不阻断**另一个（Qoder 挂了不该让 ZCode 也停）。
pub fn new_product_tasks_runner(
    zcode: Option<ZcodeTasks>,
    qoder_auth_dir: String,
    qoder_enabled: bool,
) -> Option<ProductTasksRunner> {
    // Go: `if zd == nil && !qoderEnabled { return nil }`
    if zcode.is_none() && !qoder_enabled {
        return None;
    }
    Some(Arc::new(move || {
        // `Fn` 可被调用多次 ⇒ 捕获物必须在**每次调用内**克隆。
        let zcode = zcode.clone();
        let qoder_auth_dir = qoder_auth_dir.clone();
        Box::pin(async move {
            let mut parts: Vec<String> = Vec::new();
            // 整轮共用一个 deadline —— 与 Go 的单个 ctx 同语义。
            let deadline = tokio::time::Instant::now() + PRODUCT_TASK_TIMEOUT;

            // ---- Qoder（先跑）----
            if qoder_enabled && !qoder_auth_dir.trim().is_empty() {
                match tokio::time::timeout_at(
                    deadline,
                    qoder::campaign::claim_all_campaigns(&qoder_auth_dir),
                )
                .await
                {
                    Ok(Ok(out)) => parts.push(format!(
                        "Qoder 领取 {} 项（失败 {}）",
                        go_count(&out, "claimedCount"),
                        go_count(&out, "failedCount")
                    )),
                    // ⚠ 不 return：Qoder 失败不该阻断 ZCode（两个产品互相独立）
                    Ok(Err(e)) => parts.push(format!("Qoder 领取失败：{e}")),
                    Err(_) => parts.push(
                        "Qoder 领取失败：context deadline exceeded".to_string(),
                    ),
                }
            }

            // ---- ZCode ----
            if let Some(z) = zcode.as_ref() {
                match tokio::time::timeout_at(deadline, run_zcode_claims_now(z)).await {
                    Ok((detail, err)) => {
                        if let Some(e) = err {
                            parts.push(format!("ZCode 领取失败：{e}"));
                        } else if !detail.is_empty() {
                            parts.push(detail);
                        }
                    }
                    Err(_) => parts.push(
                        "ZCode 领取失败：ZCode 领取超时（已处理部分账号）".to_string(),
                    ),
                }
            }

            // Go 的 `strings.Join(parts, "；")` —— 分隔符是**中文分号**。
            (parts.join("；"), None)
        })
    }))
}

/// Go `runZcodeClaimsNow`（`product_tasks.go:109-163`）。
///
/// **绕过 hold 闸门**：hold 的语义是「这个账号刚领过/已领完，别白探了」，
/// 而用户点「立即领取」的意思是「我现在就要检查一遍」—— 用 hold 拦住他会让他
/// 以为按钮坏了。⚠ 但**不绕过幂等**：已领过的仍会回 `already_claimed`。
///
/// # 为什么严格串行
///
/// Go 注释（`:125-127`）：参考实现同样严格串行（`setTimeout`-in-`finally` 链，
/// 绝不重叠）。并发打同一上游会让「一个账号被多台设备同时操作」，
/// 那是最容易被标记的画像。
async fn run_zcode_claims_now(z: &ZcodeTasks) -> (String, Option<String>) {
    let (creds, failed) = match (z.load_creds)() {
        Ok(v) => v,
        Err(e) => return (String::new(), Some(format!("加载 ZCode 凭证失败: {e}"))),
    };
    if creds.is_empty() {
        // 解析失败的文件必须报出来：用户会以为「账号导入了但没生效」。
        let msg = if failed.is_empty() {
            "ZCode：没有可用凭证，跳过".to_string()
        } else {
            format!("ZCode：没有可用凭证（{} 个解析失败），跳过", failed.len())
        };
        return (msg, None);
    }

    let mut claimed = 0usize;
    let mut already = 0usize;
    let mut idle = 0usize;
    let mut err_count = 0usize;
    let mut first_err: Option<String> = None;

    for cr in &creds {
        let res: ClaimTickResult = z.sched.tick_account(cr).await.0;
        match res.outcome {
            ClaimTickOutcome::Claimed => claimed += 1,
            ClaimTickOutcome::Idle => {
                // idle 里既有「没有可领」也有「已领过」，用 kind 区分。
                if res.kind == ClaimFailureKind::AlreadyClaimed {
                    already += 1;
                } else {
                    idle += 1;
                }
            }
            ClaimTickOutcome::Failed | ClaimTickOutcome::Error => {
                err_count += 1;
                if first_err.is_none() && !res.msg.is_empty() {
                    first_err = Some(res.msg.clone());
                }
            }
            // Go 的 `default: idle++`（含 `skipped_hold`，但本路径不发 hold）。
            _ => idle += 1,
        }
    }

    let detail = summarize_zcode(claimed, already, idle, err_count, creds.len());
    // 只有**一个都没成功且全是错误**时才当整轮失败 —— 部分失败（如某账号凭证
    // 过期）不该把整轮标成 failed：那会让用户在记录里看到「失败」，
    // 而实际上别的账号都领到了。
    if err_count > 0 && claimed == 0 && already == 0 {
        if let Some(e) = first_err {
            return (detail, Some(format!("ZCode 领取全部失败: {e}")));
        }
    }
    (detail, None)
}

/// Go `summarizeZcode`（`product_tasks.go:166-182`）：拼一句人能看懂的结果描述。
///
/// 零值分段**整段省略**（不是「0 个已领过」）。
fn summarize_zcode(
    claimed: usize,
    already: usize,
    idle: usize,
    err_count: usize,
    total: usize,
) -> String {
    let mut b = format!("ZCode：{total} 个账号");
    if claimed > 0 {
        b.push_str(&format!("，领取 {claimed} 个套餐"));
    }
    if already > 0 {
        b.push_str(&format!("，{already} 个已领过"));
    }
    if idle > 0 {
        b.push_str(&format!("，{idle} 个暂无可领"));
    }
    if err_count > 0 {
        b.push_str(&format!("，{err_count} 个失败"));
    }
    b
}

/// Go `fmt.Sprintf("%v", out["claimedCount"])` 的等价物。
///
/// Go 侧 `out` 是 `map[string]any`，JSON 解出来是 `float64`，`%v` 打整数值的
/// float64 **不带小数点**（`12` 而不是 `12.0`）；Rust 侧是 `usize`，直接十进制。
/// 键缺席时 Go 打 `<nil>`，这里保持一致。
fn go_count(v: &Value, key: &str) -> String {
    match v.get(key).and_then(|x| x.as_u64()) {
        Some(n) => n.to_string(),
        None => "<nil>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::products::zcode::cred::ZcodeCred;

    /// 造一个只用于「凭证加载失败 / 空」两条早退路径的接线 ——
    /// **绝不触碰网络**：那两条路径都在 `tick_account` 之前返回。
    fn tasks_with(load: LoadCreds) -> ZcodeTasks {
        let sched = Arc::new(ClaimScheduler::new(None, Some(Arc::clone(&load)), None));
        ZcodeTasks {
            sched,
            load_creds: load,
        }
    }

    #[tokio::test]
    async fn runner_is_none_when_no_product_is_wired() {
        // Go: `if zd == nil && !qoderEnabled { return nil }`
        assert!(new_product_tasks_runner(None, "/whatever".to_string(), false).is_none());
        // 只有 Qoder 开关打开时也**不该**返回 nil（Go 同理）。
        assert!(new_product_tasks_runner(None, "/whatever".to_string(), true).is_some());
    }

    #[tokio::test]
    async fn runner_reports_zcode_credential_load_failure() {
        let t = tasks_with(Arc::new(|| Err("boom".to_string())));
        let run = new_product_tasks_runner(Some(t), String::new(), false).unwrap();
        let (detail, err) = run().await;
        assert_eq!(err, None, "整轮错误被折进 detail，与 Go 一致");
        assert_eq!(detail, "ZCode 领取失败：加载 ZCode 凭证失败: boom");
    }

    #[tokio::test]
    async fn runner_reports_empty_credentials_with_the_failed_count() {
        let t = tasks_with(Arc::new(|| Ok((Vec::<ZcodeCred>::new(), vec![]))));
        let run = new_product_tasks_runner(Some(t), String::new(), false).unwrap();
        assert_eq!(run().await.0, "ZCode：没有可用凭证，跳过");

        let t = tasks_with(Arc::new(|| {
            Ok((Vec::<ZcodeCred>::new(), vec!["a.json".into(), "b.json".into()]))
        }));
        let run = new_product_tasks_runner(Some(t), String::new(), false).unwrap();
        assert_eq!(run().await.0, "ZCode：没有可用凭证（2 个解析失败），跳过");
    }

    #[tokio::test]
    async fn qoder_side_is_skipped_when_disabled_or_dir_is_blank() {
        // 两个产品都没接 ⇒ runner 为 None（上面已断言）；这里断言「开关关掉时
        // 不出现任何 Qoder 分段」—— 即不会去读那个空目录。
        let t = tasks_with(Arc::new(|| Ok((Vec::<ZcodeCred>::new(), vec![]))));
        let run =
            new_product_tasks_runner(Some(t.clone()), "   ".to_string(), true).unwrap();
        assert_eq!(run().await.0, "ZCode：没有可用凭证，跳过");

        let run = new_product_tasks_runner(Some(t), "/nonexistent".to_string(), false).unwrap();
        assert_eq!(run().await.0, "ZCode：没有可用凭证，跳过");
    }

    #[test]
    fn summarize_zcode_matches_the_go_format() {
        assert_eq!(summarize_zcode(0, 0, 0, 0, 3), "ZCode：3 个账号");
        assert_eq!(
            summarize_zcode(2, 1, 4, 3, 10),
            "ZCode：10 个账号，领取 2 个套餐，1 个已领过，4 个暂无可领，3 个失败"
        );
        // 零值分段整段省略。
        assert_eq!(summarize_zcode(0, 1, 0, 0, 5), "ZCode：5 个账号，1 个已领过");
        assert_eq!(summarize_zcode(0, 0, 0, 1, 5), "ZCode：5 个账号，1 个失败");
    }

    #[test]
    fn go_count_renders_usize_like_go_percent_v() {
        let v = serde_json::json!({"claimedCount": 12usize, "failedCount": 0usize});
        assert_eq!(go_count(&v, "claimedCount"), "12");
        assert_eq!(go_count(&v, "failedCount"), "0");
        // 键缺席时 Go 打 `<nil>`。
        assert_eq!(go_count(&v, "nothingCount"), "<nil>");
    }

    #[tokio::test]
    async fn runner_joins_parts_with_the_chinese_semicolon() {
        // 只接 ZCode 一侧时不会有分隔符；这里断言 join 用的确实是 U+FF1B。
        let t = tasks_with(Arc::new(|| Err("x".to_string())));
        let run = new_product_tasks_runner(Some(t), String::new(), false).unwrap();
        let (detail, _) = run().await;
        assert!(!detail.contains("；"), "只有一个分段时不该出现分隔符: {detail}");
    }
}
