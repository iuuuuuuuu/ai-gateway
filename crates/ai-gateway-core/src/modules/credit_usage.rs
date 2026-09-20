//! 本地积分观察快照与统计投影。
//!
//! WorkBuddy 只返回当前资源余额，没有可复用的历史账单序列。因此这里把
//! 成功查询到的余额保存为本地观察值，再用这些观察值推导「观察到的消耗」。
//!
//! # 两条并存的统计口径
//!
//! 1. **逐区间累加**（`credit_delta` / 逐日序列 / `usageToday` 等时间窗）——
//!    把每一对相邻快照的下降量相加。它能切出「今日 / 近 7 天 / 本月」，
//!    也能喂事件流与逐日趋势图。代价是必须**为每一笔扣减猜一个归属**，
//!    因此单次不完整读数就会污染它（见下）。
//! 2. **快照做差**（`compare_snapshots` / 返回值的 `comparison` 与逐账号
//!    `change`）—— `本次余额 − 上次余额`，直接给出这段区间的净变化。
//!    它不需要猜归属，因为它只回答「变了多少」，不回答「为什么变」。
//!
//! 为什么要有第 2 条（所有者原话：「关于积分消耗并不准确，其实只要对比上次积分快照，
//! 就能看到完整的积分变化了，我想了想还是这个靠谱」）：
//! 上游偶发返回**不完整的积分包列表**，聚合 `total`/`remaining` 会凭空下跌再涨回，
//! 第 1 条口径据此记出 `-380` 紧跟 `+380` 的**幻影配对**（详见 `reading_trust`
//! 与 `pair_reading_is_phantom` 的注释）。第 2 条口径对这类抖动天然免疫 ——
//! 不可信的那次读数不会被当作基线。
//!
//! 两者**并存而非取代**：汇总（差值）回答「一共少了多少」，明细（事件流 +
//! `CREDIT_SOURCE_*` 分来源记录）回答「每一笔是什么性质」。所有者的明确要求是
//! 后者必须保留，故 `account_records.rs` 的写入路径原样不动。
//!
//! 边界处理（每条都有单测）：首次运行无基线、余额增加、账号缺席后重现、
//! 上游查询失败、差值区间的时间跨度，均见 `compare_snapshots` 与 tests 模块。

use chrono::{Datelike, Duration as ChronoDuration, Local, NaiveDate, TimeZone};
use serde_json::{json, Value};
use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::modules::account::{account_display_name, load_accounts};
use crate::modules::config::{
    atomic_write, credit_usage_snapshots_file, load_checkin_logs, now_ms, record_retention_days,
    store_dir,
};
use crate::modules::official_usage;

pub const CREDIT_SNAPSHOT_RETENTION_DAYS: i64 = 90;
pub const CREDIT_SNAPSHOT_MAX_RECORDS: usize = 5_000;
pub const CREDIT_SNAPSHOT_DEDUPE_WINDOW_MS: i64 = 5 * 60 * 1000;
/// 相邻快照之间允许的最大观测缺口（2 小时）。
///
/// 为什么需要这个常量：本模块的汇总口径是「末次余额 − 上次余额」，
/// 这个差**只有在两次读数之间一直在观测时**才代表「这段时间的变化」。
/// 中间若有一大段没有快照（账号被禁用、被移除后重新出现、程序没在跑），
/// 两端做差会把**空档里发生的一切**（消耗、发放、整包过期）都算进当前窗口，
/// 而那是无据的归因 —— 正是所有者反馈的「积分消耗不准确」的另一种形态。
///
/// 2 小时的取值依据（所有者真实快照，4974 个相邻间隔实测）：
///   p50 = 30.0 min、p95 = 30.3 min、p99 = 41.8 min、最大 = 61.4 min；
///   **没有任何一个间隔超过 62 分钟**（>60min 的仅 21 个，>180min 的为 0 个）。
/// 也就是说正常运行时缺口恒 < 1.1h，2h 已是它的近 2 倍余量，
/// 而「账号缺席一整天」这类空档会被稳稳挡在外面。
///
/// 宁可截断也不硬算：截断只会让覆盖区间变短（界面会如实标出从哪一刻开始），
/// 而硬算会报出一个用户无法核对、也无法解释的数字。
pub const CREDIT_SNAPSHOT_MAX_GAP_MS: i64 = 2 * 3600 * 1000;
const CREDIT_STATS_MAX_EVENTS: usize = 200;

static SNAPSHOT_WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug)]
struct Snapshot {
    ts: i64,
    account_id: String,
    account_name: String,
    total: f64,
    remaining: f64,
    /// 包子集：`packageCode → remaining`。空 = 该快照没有包级明细（老数据）。
    ///
    /// 为什么必须存：聚合值（total/remaining）分不清「某个包没读到」与「余额真的少了」。
    /// 实测（所有者真实数据）上游会偶发返回**不完整的包列表** ——
    /// 例如某次只返回 1 个 30 的包，聚合 total 就从 380 掉到 30，
    /// 被当成「积分消耗 -350」记了一条；下次读到完整列表又记「+350 增长」，
    /// 而余额其实一整天都是 380（幻影配对）。
    ///
    /// 有了包级明细才能按**包的身份**判断：某 packageCode 这次不在、下次又回来
    /// ⇒ 读数失败（包不会凭空消失再回来），不是消耗。
    packages: std::collections::BTreeMap<String, f64>,
    /// 写入这条快照时，它是否已被判为「不可信读数」。
    ///
    /// 为什么要把判定结果存下来（而不是下次再算）：区分「抖动后的恢复」与
    /// 「真实消耗后的重新发放」靠的是**中间那次的证据** ——
    ///   · 抖动：`p380` 整个不在（包消失）
    ///   · 真消耗：`p380` 在，只是 remaining 变成 30
    /// 两次读数当下就能分辨，但事后只看数值（都回到 380）无法区分。
    /// 故把「上次是可疑读数」记进快照，回升那次据此判定为**恢复**而非发放。
    suspect: bool,
}

#[derive(Clone, Debug)]
struct UsageEvent {
    ts: i64,
    date: String,
    account_id: String,
    amount: f64,
}

#[derive(Clone, Debug)]
struct CheckinEvent {
    ts: i64,
    date: String,
    account_id: Option<String>,
    account_name: String,
    result: String,
    error: Option<String>,
}

fn non_empty_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from)
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(value)) => value.as_f64(),
        Some(Value::String(value)) => value.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn snapshot_from_value(value: &Value) -> Option<Snapshot> {
    let account_id = non_empty_string(value.get("accountId"))?;
    let ts = value.get("ts").and_then(Value::as_i64)?;
    let total = number(value.get("total"))?.max(0.0);
    let remaining = number(value.get("remaining"))?.max(0.0);
    // 包级明细可选：老快照没有这个字段，解析成空 map（= 无判据，退回聚合值比较）。
    let packages = value
        .get("packages")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| number(Some(v)).map(|n| (k.clone(), n)))
                .collect()
        })
        .unwrap_or_default();
    Some(Snapshot {
        ts,
        account_id,
        account_name: non_empty_string(value.get("accountName"))
            .unwrap_or_else(|| "unknown".to_string()),
        total,
        remaining,
        packages,
        // 老快照没有这个字段 → 视为可信（当时没有判据，不该倒推怀疑历史数据）。
        suspect: value.get("suspect").and_then(Value::as_bool).unwrap_or(false),
    })
}

fn snapshot_value(
    ts: i64,
    account_id: &str,
    account_name: &str,
    total: f64,
    remaining: f64,
) -> Value {
    json!({
        "ts": ts,
        "accountId": account_id,
        "accountName": account_name,
        "total": total.max(0.0),
        "remaining": remaining.max(0.0),
    })
}

/// 带包级明细的快照值。
///
/// 包级明细是本次修复的核心依据（见 `reading_trust`），因此新快照一律带上。
/// **包级为空时不写 `packages` 字段** —— 保持与老快照同形，
/// 避免下游把「空 map」误读成「这个账号一个包都没有」。
///
/// `suspect` 为 true 表示这次读数已被判为不完整：写进快照后，
/// 下一次回升时才能区分「抖动恢复」与「真实发放」（见 `reading_trust` 判据 3）。
/// false 时不写该字段，保持文件紧凑（绝大多数快照都是可信的）。
fn snapshot_value_with_packages(
    ts: i64,
    account_id: &str,
    account_name: &str,
    total: f64,
    remaining: f64,
    packages: &std::collections::BTreeMap<String, f64>,
    suspect: bool,
) -> Value {
    let mut value = snapshot_value(ts, account_id, account_name, total, remaining);
    if !packages.is_empty() {
        value["packages"] = json!(packages);
    }
    if suspect {
        value["suspect"] = json!(true);
    }
    value
}

/// 读取本地观察快照；文件缺失或损坏时返回空列表。
pub fn load_snapshots() -> Vec<Value> {
    let path = credit_usage_snapshots_file();
    if let Ok(text) = std::fs::read_to_string(path) {
        if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&text) {
            return items;
        }
    }
    vec![]
}

fn should_suppress_duplicate(
    snapshots: &[Value],
    account_id: &str,
    total: f64,
    remaining: f64,
    at_ms: i64,
) -> bool {
    let Some(latest) = snapshots
        .iter()
        .filter_map(snapshot_from_value)
        .filter(|snapshot| snapshot.account_id == account_id)
        .max_by_key(|snapshot| snapshot.ts)
    else {
        return false;
    };

    latest.ts <= at_ms
        && at_ms - latest.ts <= CREDIT_SNAPSHOT_DEDUPE_WINDOW_MS
        && (latest.total - total).abs() < f64::EPSILON
        && (latest.remaining - remaining).abs() < f64::EPSILON
}

fn normalize_snapshots(snapshots: &[Value], at_ms: i64) -> Vec<Value> {
    // 保留天数来自设置项（默认 60 天，设置页可调）
    let keep_days = record_retention_days();
    let cutoff = at_ms.saturating_sub(keep_days * 24 * 3600 * 1000);
    let mut kept: Vec<Value> = snapshots
        .iter()
        .filter_map(|value| {
            let snapshot = snapshot_from_value(value)?;
            (snapshot.ts >= cutoff && snapshot.ts <= at_ms).then_some(value.clone())
        })
        .collect();
    if kept.len() > CREDIT_SNAPSHOT_MAX_RECORDS {
        kept.drain(..kept.len() - CREDIT_SNAPSHOT_MAX_RECORDS);
    }
    kept
}

/// 相邻快照之间的积分变化，连同它的来源判据。
///
/// 为什么要把「变化量」和「来源」放在一起算：来源的判据是**容量（total）
/// 有没有跟着变**，而容量只存在于前后两个快照里。留在 `record_snapshot`
/// 里就地算，是唯一能同时看到三个数的位置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreditDelta {
    /// 余额变化（正为增长、负为消耗）。
    pub amount: i64,
    /// 容量变化（正为新增额度、负为额度回收/到期）。
    pub capacity: i64,
}

/// 判定一次积分变化的**来源类别**。
///
/// 重要：这里判定的是「余额是怎么动的」这一**可观测量**，不是「哪个任务发的奖励」。
///
/// 为什么不能给出任务名：上游资源接口
///（`resource_summary`，见 `credits.rs`）只返回容量 / 余额 / 到期时间三类数字，
/// 没有任何「这笔积分由哪个任务产生」的字段或账单流水。实测真实数据里
/// 33 条积分增长记录中只有 1 条能在同账号 ±30 分钟内找到任务记录 ——
/// 按时间邻近去「认领」来源，等于把 32 条无据可依的记录也贴上任务名，
/// 那正是**编造**。因此这里只如实记录可验证的判据。
///
/// 判据（全部来自实测真实快照，见交付报告）：
///   - 余额上升且容量同步上升 → `grant`：账号拿到了**新增额度**
///     （实测 158 次上升中 141 次容量与余额增量完全相等，其余差额是到账前
///     已被消耗的部分 —— 例如容量 +1650 而余额 +1620.73）。
///   - 余额下降且容量不变   → `consume`：纯消耗（实测 434 次，容量变化恒为 0）。
///   - 余额下降且容量同降   → `expire`：额度被回收/到期
///     （实测 5 次，例如某积分包容量 -100、余额 -79，即包内还剩 79 分就整包失效）。
///   - 其余                 → `adjust`：无法归入以上三类的调整。
///
/// 把 5000 条真实快照按相邻对回放，597 次变化全部落进前三类、0 次 adjust、
/// 0 次自相矛盾（余额涨却判成 consume/expire 之类）。
///
/// 注：`capacity` 为 0 而 `amount` 为正时归 `adjust` 而不是 `grant` ——
/// 「容量没变但余额涨了」意味着积分是**退回来**的（如失败调用返还），
/// 不是新增额度；把它说成 grant 会让用户以为额度包变多了。
pub fn classify_credit_source(amount: i64, capacity: i64) -> &'static str {
    use crate::modules::account_records::{
        CREDIT_SOURCE_ADJUST, CREDIT_SOURCE_CONSUME, CREDIT_SOURCE_EXPIRE, CREDIT_SOURCE_GRANT,
    };
    if amount > 0 && capacity > 0 && capacity >= amount {
        return CREDIT_SOURCE_GRANT;
    }
    if amount < 0 && capacity == 0 {
        return CREDIT_SOURCE_CONSUME;
    }
    if amount < 0 && capacity < 0 {
        return CREDIT_SOURCE_EXPIRE;
    }
    CREDIT_SOURCE_ADJUST
}

/// 一次积分变化对应的展示文案（标题 + 说明）。
///
/// 为什么标题要区分来源、而不是继续用「积分增长 / 积分消耗」两个词：
/// 那正是用户抱怨的现象 —— 记录里只写「积分增长 +100」，看不出这 100 是
/// 新增额度还是别的。标题里带上来源类别，用户一眼能分清性质；
/// `detail` 再补上**原始判据**（余额与容量各变了多少），
/// 让他能自己核对，而不是只能相信我们的结论。
///
/// detail 里刻意写出容量变化：这正是「为什么判成这一类」的证据。
/// 只给结论不给判据，用户无法区分「系统算错了」和「口径与我想的不同」。
fn credit_record_text(source: &str, delta: &CreditDelta) -> (&'static str, String) {
    use crate::modules::account_records::{
        CREDIT_SOURCE_ADJUST, CREDIT_SOURCE_CONSUME, CREDIT_SOURCE_EXPIRE, CREDIT_SOURCE_GRANT,
    };
    match source {
        CREDIT_SOURCE_GRANT => (
            "积分增长 · 额度发放",
            format!(
                "余额 +{}，额度容量 +{}（新增积分包到账）",
                delta.amount, delta.capacity
            ),
        ),
        CREDIT_SOURCE_CONSUME => (
            "积分消耗 · 调用扣减",
            format!("余额 {}，额度容量不变（纯消耗）", delta.amount),
        ),
        CREDIT_SOURCE_EXPIRE => (
            "积分减少 · 额度到期",
            format!(
                "余额 {}，额度容量 {}（积分包被回收，包内剩余一并失效）",
                delta.amount, delta.capacity
            ),
        ),
        CREDIT_SOURCE_ADJUST => (
            "积分调整",
            format!(
                "余额 {}，额度容量 {}（无法归入发放/消耗/到期）",
                delta.amount, delta.capacity
            ),
        ),
        // 理论上不可达：source 只由 classify_credit_source 产生。
        // 兜底而不 panic —— 记录是旁路观测数据，文案未知不该让主流程崩。
        other => (
            "积分变化",
            format!("余额 {}，来源 {}（未识别）", delta.amount, other),
        ),
    }
}

/// 计算相邻两次快照之间的积分变化；无变化时返回 `None`。
///
/// 从 `record_snapshot` 里抽出来是为了**可被单测直接覆盖**：
/// 原来的判据内联在写盘流程中（需要构造快照文件、锁、数据目录），
/// 想断言「容量同增才算 grant」就得跑一整套 IO。纯函数化之后，
/// 判据本身可以被逐分支钉死，写盘路径只剩下调用。
///
/// 用 `Option` 而不是返回 amount=0：调用方只在 `Some` 时才写记录，
/// 让「没有变化就不写」这件事由类型表达，而不是靠调用方记得判零。
pub fn credit_delta(prev_remaining: f64, prev_total: f64, remaining: f64, total: f64) -> Option<CreditDelta> {
    // 四舍五入到整数：积分通常是整数，浮点误差会造出 -0.0000001 这类噪音
    let amount = (remaining - prev_remaining).round() as i64;
    if amount == 0 {
        return None;
    }
    Some(CreditDelta {
        amount,
        capacity: (total - prev_total).round() as i64,
    })
}

/// 一次读数的可信度判定结果。
#[derive(Debug, PartialEq, Eq)]
enum ReadingTrust {
    /// 读数正常，可按 credit_delta 记为真实变化。
    Ok,
    /// 读数不完整（上游返回的包列表缺了东西），**不可据此记积分变化**。
    /// 携带人话原因，供 detail 使用。
    Incomplete(&'static str),
}

/// 判断本次读数是否可信 —— 用**包级证据**，不靠「数值大小像不像异常」猜。
///
/// 背景（所有者真实数据实测）：上游会偶发返回**不完整的包列表**，
/// 导致聚合 total/remaining 凭空下跌再涨回，记录里出现 `-350` / `+350` 的幻影配对，
/// 而余额其实一整天没变。上游**没有**「积分消耗明细」接口可查，
/// 所以只能靠包的身份来分辨「没读到」与「真的少了」。
///
/// 三条判据，任一成立即判为读数不完整：
///
/// 1. **包凭空消失**：上次存在、这次不见了的 `packageCode`。
///    包不会自己消失又回来 —— 真到期有 `expireAt` 为证，真消耗只减 `remaining` 不删包。
///    故「少包」直接说明这次没读到它们。
///    （只在**两侧都有包级明细**时可用；老快照没有明细则跳过这条。）
///
/// 2. **容量不应因消耗而减少**：`total`（额度容量）是各包的**总额度**，
///    消耗只减 `remaining`，**不会**让 `total` 变小。
///    所以 `total` 下降必然意味着**包集合变了**（少读到包 / 包被回收），而非用掉额度。
///    `total` 上升是正常的（新包到账）；`total` 不变也正常。
///
/// 3. **上一次是不可信读数，本次回升 ⇒ 是恢复而非发放**：
///    只看**紧邻**的上一次会漏掉回升那一侧 —— 序列 `380 → 30 → 380` 里，
///    第三次是「total 上升」，判据 2 放行；而它上一次（30）的包里没有 p380，
///    判据 1 也看不出「消失」。于是 `+350` 那一半照样被记下来。
///    故读 `prev.suspect`：上一次已被判可疑、且本次数值比它高 ⇒ 判定为恢复。
///
///    为什么用「上一次是否可疑」而不是「数值是否回到更早的值」：
///    后者无法区分抖动恢复与「真消耗后又发放」——两者数值都可能回到原处。
///    而这两者在**中间那次读数当下**就能分辨（包消失 = 抖动；包在但余额跌 = 真消耗），
///    所以把判定结果存进快照，回升时直接读，不做事后猜测。
///
/// 注意判据 2 **不依赖**包级明细，因此对老快照也生效 ——
/// 这正是本次幻影记录的直接原因：`total` 与 `remaining` 同时从 380 掉到 30。
///
/// 为什么不用「归零/暴跌幅度」当判据：消耗快时余额确实可能快速下降甚至归零，
/// 那会误杀真实消耗（所有者明确指出过这一点）。上面三条都是**结构性证据**，与幅度无关。
fn reading_trust(
    prev: &Snapshot,
    now_total: f64,
    now_remaining: f64,
    now_packages: &std::collections::BTreeMap<String, f64>,
) -> ReadingTrust {
    let dropped = now_remaining < prev.remaining - f64::EPSILON;

    // 判据 1：有包级明细时，看有没有包凭空消失。
    if !prev.packages.is_empty() {
        let vanished: Vec<&str> = prev
            .packages
            .keys()
            .filter(|code| !now_packages.contains_key(*code))
            .map(String::as_str)
            .collect();
        if !vanished.is_empty() {
            // 只有「余额也在跌」时才需要怀疑；余额不跌而包少了，说明只是没读到。
            // 若余额同时在跌，仍可能是真消耗 + 真到期，故只在**容量也跌**时定性。
            let capacity_dropped = now_total < prev.total - f64::EPSILON;
            if capacity_dropped || dropped {
                return ReadingTrust::Incomplete("上游本次返回的积分包列表不完整");
            }
        }
    }

    // 判据 2：total（额度容量）不应因消耗而减少。
    if now_total < prev.total - f64::EPSILON {
        return ReadingTrust::Incomplete("额度容量意外减少，上游本次返回的积分包列表不完整");
    }

    // 判据 3：上一次已判可疑、本次数值回升 ⇒ 这是**恢复**，不是发放。
    if prev.suspect && (now_total > prev.total + f64::EPSILON || !dropped) {
        return ReadingTrust::Incomplete("上一次为不完整读数，本次为恢复");
    }

    ReadingTrust::Ok
}

/// record_account_id 把**快照用的** id 归一成**记录用的**裸 id。
///
/// # 为什么需要这一步（2026-09-20 实测缺陷）
///
/// 两个产品（Qoder / ZCode）的 uid 命名空间可能与 WorkBuddy 撞，
/// 故 `product_credit_snapshot` 给快照 id 加产品前缀（`zcode:xxx`）——
/// 那对**快照**是正确的隔离。
///
/// 但同一个 id 也被写进了**用户可见的记录**，而界面按**裸 uid** 过滤
///（`ZcodePage` → `fixedAccountId={row.uid}`），且 `query_records`
/// 是精确比较 ⇒ 点开账号只能看到一半记录，**另一半静默消失**。
///
/// 故记录侧统一剥掉前缀。只剥**已知产品**的前缀：
/// 用「首个冒号前是产品名」判定太宽松 —— WorkBuddy 的 uid 本身是
/// UUID（含 `-` 不含 `:`），而用户手填的 id 未必守规矩，
/// 白名单能保证"只有我们自己的前缀会被剥掉"。
fn record_account_id(snapshot_id: &str) -> &str {
    const PRODUCTS: [&str; 3] = ["zcode", "qoder", "workbuddy"];
    for p in PRODUCTS {
        // 前缀后必须还有内容，否则 `"zcode:"` 会被剥成空串
        //（空 id 在 query_records 里是"全部账号"的语义，那会串账号）。
        if let Some(rest) = snapshot_id.strip_prefix(p) {
            if let Some(rest) = rest.strip_prefix(':') {
                if !rest.is_empty() {
                    return rest;
                }
            }
        }
    }
    snapshot_id
}

/// 写入一个成功的资源观察值。
/// 同一账号同一资源值在短时间内只保留一条；资源值发生变化时立即保留，
/// 这样余额下降可以归因到新快照。返回值表示本次是否实际写入。
///
/// `packages` 是本次读到的包子集（`packageCode → remaining`），
/// 用于分辨「包没读到」与「余额真的少了」（见 `reading_trust`）。
/// 传空 map 也能工作，只是失去判据 1（判据 2 仍生效）。
pub fn record_snapshot(
    account_id: &str,
    account_name: &str,
    total: f64,
    remaining: f64,
    packages: std::collections::BTreeMap<String, f64>,
) -> bool {
    let account_id = account_id.trim();
    if account_id.is_empty() {
        return false;
    }

    let at_ms = now_ms();
    let _guard = SNAPSHOT_WRITE_LOCK.lock().unwrap();
    let snapshots = load_snapshots();
    if should_suppress_duplicate(&snapshots, account_id, total, remaining, at_ms) {
        return false;
    }

    // 记一条积分变化事件（供单账号记录视图）。
    //
    // 为什么在这里记而不是在调用方：本函数已经拿到了「上一快照」与「本次余额」，
    // 差值就在这里最自然；调用方（签到、切换、巡检）各自算差值会口径不一。
    // 只在**余额确实变化**时记录，避免每 15 分钟的巡检刷出一堆 amount=0 的噪音。
    //
    // **但变化的前提是这次读数可信**：上游会偶发返回不完整的包列表，
    // 若不判可信度就会记出「-380 消耗」+「+380 发放」的幻影配对（见 reading_trust 注释）。
    // 本次读数是否可疑 —— 供写入快照（下一次回升时据此判定「恢复」而非「发放」）。
    let mut suspect = false;
    if let Some(prev) = snapshots
        .iter()
        .filter_map(snapshot_from_value)
        .filter(|s| s.account_id == account_id)
        .max_by_key(|s| s.ts)
    {
        match reading_trust(&prev, total, remaining, &packages) {
            ReadingTrust::Incomplete(why) => {
                suspect = true;
                // 读数不完整 → **不记积分变化**（记了就是幻影）。
                // 但仍打一行日志：这是上游行为异常，排查时需要线索，
                // 且不能静默 —— 否则「记录里少了一条」会被当成功能坏了。
                eprintln!(
                    "[积分统计] 跳过一次不可信读数（{why}）：账号 {} 上次 total={} remaining={}，本次 total={} remaining={}",
                    &account_id[..account_id.len().min(8)],
                    prev.total,
                    prev.remaining,
                    total,
                    remaining,
                );
            }
            ReadingTrust::Ok => {
                if let Some(delta) = credit_delta(prev.remaining, prev.total, remaining, total) {
                    let source = classify_credit_source(delta.amount, delta.capacity);
                    let (title, detail) = credit_record_text(source, &delta);
                    crate::modules::account_records::add_credit_record(
                        // ⚠ 记录用**裸 id**，不能带产品前缀。
                        //
                        // # 为什么（2026-09-20 实测缺陷）
                        //
                        // `product_credit_snapshot` 为了隔离两个产品的 uid 命名空间，
                        // 把 id 拼成 `zcode:zcode-1b2941c020ef` 再传给本函数
                        //（那对**快照**是对的）。但本函数会把这个 id **写进用户可见
                        // 的记录**，而界面按账号的**裸 uid** 过滤
                        //（`ZcodePage` → `fixedAccountId={row.uid}`）。
                        //
                        // 于是同一个 ZCode 账号出现两种 accountId：
                        //
                        //	zcode:zcode-1b2941c020ef   ← 走本函数（带前缀）
                        //	zcode-1b2941c020ef         ← 走 zcode_login 的额度刷新
                        //
                        // 而 `query_records` 是**精确字符串比较** ⇒ 点开账号只能
                        // 看到不带前缀的那一半，**另一半静默消失**。
                        // 所有者报的「zcode和qoder都无法查看任务执行记录,和积分消耗明细」
                        // 有一部分正是这个。
                        //
                        // 故：**快照**用带前缀的 id（隔离有效），**记录**用裸 id。
                        record_account_id(account_id),
                        account_name.trim(),
                        title,
                        delta.amount,
                        &detail,
                        Some(source),
                    );
                }
            }
        }
    }

    let mut kept = normalize_snapshots(&snapshots, at_ms);
    // 不可信读数**仍然写入快照**：它是「上游这次返回了什么」的事实，
    // 丢掉会让下一次比较失去基准（而且下一次可能才是完整的那次）。
    // 被抑制的只是「据此推断积分变化」这一步。
    kept.push(snapshot_value_with_packages(
        at_ms,
        account_id,
        account_name.trim(),
        total,
        remaining,
        &packages,
        suspect,
    ));
    if kept.len() > CREDIT_SNAPSHOT_MAX_RECORDS {
        kept.drain(..kept.len() - CREDIT_SNAPSHOT_MAX_RECORDS);
    }

    if let Err(error) = std::fs::create_dir_all(store_dir()).and_then(|_| {
        let content = serde_json::to_string_pretty(&kept).unwrap_or_default();
        atomic_write(&credit_usage_snapshots_file(), &content)
    }) {
        eprintln!("[积分统计] 保存积分快照失败: {error}");
        return false;
    }
    true
}

fn local_date(ts: i64) -> Option<NaiveDate> {
    Local
        .timestamp_millis_opt(ts)
        .single()
        .map(|date| date.date_naive())
}

fn local_date_string(ts: i64) -> Option<String> {
    local_date(ts).map(|date| date.format("%Y-%m-%d").to_string())
}

/// 生成逐日观察序列（daily_start..=today，无数据的天补 0）；无快照起点时返回空数组。
fn local_daily_series(
    daily: &HashMap<String, f64>,
    daily_start: Option<NaiveDate>,
    today: NaiveDate,
) -> Vec<Value> {
    let Some(start) = daily_start else {
        return Vec::new();
    };
    let mut points = Vec::new();
    let mut date = start;
    while date <= today {
        let key = date.format("%Y-%m-%d").to_string();
        points.push(json!({
            "date": key,
            "usage": daily.get(&key).copied().unwrap_or(0.0),
        }));
        date += ChronoDuration::days(1);
    }
    points
}

fn parse_checkin_event(value: &Value) -> Option<CheckinEvent> {
    let ts = value
        .get("ts")
        .and_then(Value::as_i64)
        .or_else(|| crate::modules::config::norm_ts(value.get("ts")))?;
    Some(CheckinEvent {
        ts,
        date: local_date_string(ts)?,
        account_id: non_empty_string(value.get("accountId")),
        account_name: non_empty_string(value.get("email")).unwrap_or_else(|| "unknown".to_string()),
        result: non_empty_string(value.get("result")).unwrap_or_else(|| "error".to_string()),
        error: non_empty_string(value.get("error")),
    })
}

fn parse_snapshots(values: &[Value], at_ms: i64) -> Vec<Snapshot> {
    // 保留天数来自设置项（与 normalize_snapshots 同一口径）
    let keep_days = record_retention_days();
    let cutoff = at_ms.saturating_sub(keep_days * 24 * 3600 * 1000);
    let mut snapshots: Vec<Snapshot> = values
        .iter()
        .filter_map(snapshot_from_value)
        .filter(|snapshot| snapshot.ts >= cutoff && snapshot.ts <= at_ms)
        .collect();
    snapshots.sort_by_key(|snapshot| snapshot.ts);
    snapshots
}

// ---------------------------------------------------------------------------
// 汇总口径：与上一次积分快照做差
//
// 为什么在「逐事件累加」之外还要这一套：逐事件那条路（`credit_delta` +
// `add_credit_record`）必须**为每一笔扣减猜一个归属** —— 它只能在两次邻近读数
// 之间把差值归类成发放/消耗/到期/调整。只要有一次读数不完整，那个差值就是错的，
// 而错的那一条会永久留在记录里。所有者实测到过 `-380` 紧跟 `+380` 的幻影配对，
// 就是这条路产生的（`reading_trust` 的注释记录了完整踩坑史）。
//
// 快照做差不需要猜归属：`本次余额 − 上次余额` 就是这段区间的**净变化**，
// 它天然免疫「某一次读数不可信」—— 因为不可信的那次不会被当作基线。
// 代价是它只能给出**汇总值**，给不出「这笔是调用扣减、那笔是签到发放」。
// 所以两者并存而不是互相取代：明细看事件，总量看差值。
// ---------------------------------------------------------------------------

/// 无可用基准：首次运行还没有「上一次快照」。
///
/// 为什么必须是一个**显式状态**而不是 0：0 会被读成「这段时间没有消耗」，
/// 而事实是「我们根本不知道」。0 与「不知道」是两件事。
pub const CREDIT_COMPARISON_NO_BASELINE: &str = "no_baseline";
/// 读数过旧：最后一次快照距今超过 `CREDIT_SNAPSHOT_MAX_GAP_MS`。
///
/// **注意它不与 `ok` 互斥** —— 见 `CreditComparison::stale` 的说明。
/// 保留这个取值是为了让「最近一次对比发生在很久以前」这件事有名字，
/// 供日志与调试使用；界面上的表达由 `stale` 布尔字段承担。
pub const CREDIT_COMPARISON_STALE: &str = "stale";
/// 基准或本次读数已被判为不可信（上游返回了不完整的包列表）。
pub const CREDIT_COMPARISON_UNRELIABLE: &str = "unreliable";
/// 可给出可信差值。
pub const CREDIT_COMPARISON_OK: &str = "ok";

/// 一次「与上次快照做差」的结果。
///
/// 字段设计说明：
/// - `decrease` / `increase` 分开存，而不是只给一个带符号的 `net`：
///   所有者明确要求「余额增加（签到/发放）要与消耗分开表达」。
///   只给 net 的话，`-100 消耗` 与 `+50 发放` 会互相抵消成 `-50`，
///   用户看到「消耗 50」而实际消耗了 100 —— 这正是旧口径被抱怨的形态。
/// - `net` 仍然保留：它是「这段时间积分到底净变了多少」的完整答案，
///   而 `decrease - increase == net` 恒成立，两者可互相校验。
/// - `from_ts` / `to_ts` 是**必填语义**：差值覆盖的区间必须能说清楚是哪一段，
///   否则「消耗 380」无从核对（是今天的？还是这一周的？）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CreditComparison {
    /// 是否有可信差值。false 时 `decrease`/`increase`/`net` 恒为 0 且无意义。
    pub ok: bool,
    /// 无可信差值的原因（`CREDIT_COMPARISON_*`）；ok 时为 `CREDIT_COMPARISON_OK`。
    pub reason: &'static str,
    /// 差值区间起点（上次快照时刻）；无基准时为 None。
    pub from_ts: Option<i64>,
    /// 差值区间终点（本次快照时刻）；无快照时为 None。
    pub to_ts: Option<i64>,
    /// 区间内余额**下降**的总量（≥ 0）= 消耗。
    pub decrease: f64,
    /// 区间内余额**上升**的总量（≥ 0）= 发放/返还。与消耗分开表达。
    pub increase: f64,
    /// 净变化 = to 余额 − from 余额 = increase − decrease。
    pub net: f64,
    /// 这份差值**不是最新的**：末次快照距统计时刻已超过 `CREDIT_SNAPSHOT_MAX_GAP_MS`。
    ///
    /// 为什么不与 `ok` 合并成一个三态枚举：两者是**正交**的事实。
    /// 一份 30 分钟区间的差值即使算得完全正确（`ok = true`），
    /// 只要最近 5 小时没再采集过，它就是**陈旧**的。把陈旧说成「不可信」
    /// 会丢掉一个真实可信的数字；把陈旧当成「当前」又会让用户以为
    /// 那就是此刻的消耗。故分开表达，界面用「数据截至 …」的措辞如实标注。
    pub stale: bool,
}

impl CreditComparison {
    fn none(reason: &'static str) -> Self {
        CreditComparison {
            ok: false,
            reason,
            from_ts: None,
            to_ts: None,
            decrease: 0.0,
            increase: 0.0,
            net: 0.0,
            stale: false,
        }
    }
}

/// 这一对相邻快照之间**读数结构上是否不可信**（差值不能当作真实变化）。
///
/// 抽成独立函数的理由：这个判据有**两个**消费方，且两处必须完全一致 ——
///   · 汇总口径 `compare_snapshots`（末次 vs 上次）；
///   · 逐区间累加（逐日序列 / 时间窗 / 事件流）。
/// 若各写一份，早晚会出现「汇总说不可信、累加却照样把 -350 算进今日消耗」
/// 这种自相矛盾的界面（而 GatewayPage 的「消耗积分」列读的正是后者）。
///
/// 两条判据都取自 `reading_trust` 的既有结论，不新增猜测：
///   · `suspect`：写入该快照时已判定上游包列表不完整；
///   · `total` 下降：额度容量不会因消耗而变小，故下降即「包集合变了」。
/// 幅度**不**作为判据 —— 消耗快时余额确实可能快速下跌，那会误杀真实消耗。
///
/// 实测效果（所有者真实快照 5000 条、25 个账号，只读回放）：
/// 旧口径把逐区间下降量全部相加 = 11165.4；套用本判据后 = 9391.2，
/// 即 **剔除 6 个幻影区间、共 1774.2 积分（占原报数 15.9%）**。
/// 那 6 个区间的形态就是 `350→0→350` / `380→30→380`（`total` 同步塌到 0/30）。
fn pair_reading_is_phantom(prev: &Snapshot, latest: &Snapshot) -> bool {
    prev.suspect || latest.suspect || latest.total < prev.total - f64::EPSILON
}

/// 比较两个快照，得出这段区间的积分变化。
///
/// 纯函数，便于逐条边界钉死（写盘路径只剩调用）。
///
/// **为什么区间有上限**（`CREDIT_SNAPSHOT_MAX_GAP_MS`）：
/// 「末次 − 上次」只有在两次读数**之间一直在观测**时才等于「这段时间的变化」。
/// 账号被禁用/移除期间不会有快照，重新出现时若直接与禁用前那次做差，
/// 空档里发生的消耗、发放、整包过期会被整体算进当前窗口 —— 那是无据的归因。
/// 故缺口过大直接判为「无基准」，宁可显示「暂无对比基准」也不报一个假数字。
///
/// **为什么容量下降要判不可信**：`total`（额度容量）是各包总额度之和，
/// 消耗只减 `remaining`，不会让 `total` 变小（见 `reading_trust` 判据 2）。
/// 所以 `total` 下降必然意味着**包集合变了**（少读到包 / 包被回收），
/// 此时两端做差会把「没读到的那些包」算成消耗 —— 所有者那 5 对幻影记录
/// （实测 350→0→350、380→30→380）全部落在这个形态上。
/// 保守处理与 `reading_trust` 保持一致：宁可漏记一次真到期，也不报一个幻影消耗。
///
/// **已知残留**（如实记录，不掩饰）：若某次抖动**恰好没有改变 `total`**
/// （例如只读到部分包且总额度凑巧相同），且该快照又没有 `suspect` 标记
///（本标记是修复幻影配对那一版才引入，历史快照均无），
/// 本函数无法把它与真实变化区分开。`suspect` 一旦写入即可拦住同类情况，
/// 故这是**历史数据**的残留，不是新数据的缺陷。
///
/// 不对外 `pub`：参数类型 `Snapshot` 是本模块的私有解析产物，
/// 暴露出去会让 crate 外无法构造参数（`private_interfaces`）。调用点只有
/// 本模块的 `build_statistics`，单测与本模块同处一个 `mod`，照样能覆盖。
fn compare_snapshots(prev: &Snapshot, latest: &Snapshot) -> CreditComparison {
    // 缺口过大 → 中间没有观测，不构成基准。
    if latest.ts - prev.ts > CREDIT_SNAPSHOT_MAX_GAP_MS {
        return CreditComparison::none(CREDIT_COMPARISON_NO_BASELINE);
    }

    // 任一端被标记为不可信读数（上游包列表不完整）→ 差值测的是抖动，不是现实。
    // 为什么两端都要查：`prev` 可疑时本次是「恢复」（会假报发放），
    // `latest` 可疑时本次是「读数掉了」（会假报消耗）—— 两种都会污染汇总值。
    //
    // 与逐区间累加共用 `pair_reading_is_phantom`，保证两条口径同进同退。
    if pair_reading_is_phantom(prev, latest) {
        return CreditComparison {
            from_ts: Some(prev.ts),
            to_ts: Some(latest.ts),
            ..CreditComparison::none(CREDIT_COMPARISON_UNRELIABLE)
        };
    }

    let net = latest.remaining - prev.remaining;
    CreditComparison {
        ok: true,
        reason: CREDIT_COMPARISON_OK,
        from_ts: Some(prev.ts),
        to_ts: Some(latest.ts),
        // 分开取正负：`net` 为负时 increase 恒为 0，反之亦然。
        // 这样「消耗 100 + 发放 50」报的是 decrease=100 / increase=50（净 −50），
        // 而不是把两者抵消成「消耗 50」—— 后者会让用户以为只烧了 50。
        decrease: (-net).max(0.0),
        increase: net.max(0.0),
        net,
        // 由调用方按统计时刻回填（见 `compare_account_snapshots`）：
        // 本函数刻意不接收 `at_ms`，保持「两个快照 → 一个差值」的纯语义。
        stale: false,
    }
}

/// 为一个账号的全部快照算出「末次 vs 上次」的汇总差值。
///
/// 输入要求按 `ts` 升序（`build_statistics` 已排序）。少于 2 条时即「首次运行」，
/// 返回 `NO_BASELINE` —— 这正是「首次运行不该报出巨额虚假消耗」那条边界的实现点。
///
/// `at_ms` 只用于判定 `stale`（这份差值是不是已经不再反映「当前」）。
/// 之所以不放进 `compare_snapshots`：那个函数是纯的两快照比较，
/// 与「统计发生在什么时候」无关；把 `at_ms` 混进去会让它的语义变模糊，
/// 单测也得凭空造一个与断言无关的时间戳。
fn compare_account_snapshots(snapshots: &[Snapshot], at_ms: i64) -> CreditComparison {
    let Some(latest) = snapshots.last() else {
        return CreditComparison::none(CREDIT_COMPARISON_NO_BASELINE);
    };
    let Some(prev) = snapshots.len().checked_sub(2).map(|index| &snapshots[index]) else {
        // 只有一条快照：它是基线本身，还没有可比的对象。
        return CreditComparison {
            to_ts: Some(latest.ts),
            ..CreditComparison::none(CREDIT_COMPARISON_NO_BASELINE)
        };
    };
    let mut comparison = compare_snapshots(prev, latest);
    // 陈旧判定与可信判定**正交**：算得再准的差值，只要末次读数太旧，
    // 它反映的就不是「此刻」而是「那一刻」。界面据此改用「数据截至 …」措辞。
    //
    // 用 `saturating_sub` 而不是直接相减：`at_ms` 理论上可能早于快照
    // （时钟回拨、测试构造），相减会溢出成负数从而把新鲜数据误判为陈旧。
    comparison.stale = at_ms.saturating_sub(latest.ts) > CREDIT_SNAPSHOT_MAX_GAP_MS;
    comparison
}

/// 汇总差值的 JSON 形态（顶层 `comparison` 与逐账号 `change` 共用同一套字段名）。
///
/// 为什么 `decrease` / `increase` 恒为数字而**不用 null**：只有「有没有可信差值」
/// 这一件事需要三态，而它已经由 `ok` + `reason` 表达；再给两个数值字段加 null
/// 会让前端要判三次空。前端只需认 `ok`：false 就显示「—」，绝不显示 0。
fn comparison_value(comparison: &CreditComparison) -> Value {
    json!({
        "ok": comparison.ok,
        "reason": comparison.reason,
        "fromTs": comparison.from_ts,
        "toTs": comparison.to_ts,
        "decrease": comparison.decrease,
        "increase": comparison.increase,
        "net": comparison.net,
        "stale": comparison.stale,
    })
}

fn usage_in_windows(date: &str, today: NaiveDate) -> (bool, bool, bool) {
    let Some(date) = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok() else {
        return (false, false, false);
    };
    let distance = (today - date).num_days();
    (
        distance == 0,
        (0..7).contains(&distance),
        date.year() == today.year() && date.month() == today.month(),
    )
}

fn add_account_name(
    account_ids: &mut Vec<String>,
    account_names: &mut HashMap<String, String>,
    account_id: String,
    account_name: String,
) {
    if !account_ids.contains(&account_id) {
        account_ids.push(account_id.clone());
    }
    let should_replace = account_names
        .get(&account_id)
        .map(|name| name == "unknown" && account_name != "unknown")
        .unwrap_or(true);
    if should_replace {
        account_names.insert(account_id, account_name);
    }
}

fn checkin_identity(event: &CheckinEvent) -> Option<String> {
    if let Some(account_id) = event.account_id.as_ref() {
        return Some(format!("account:{account_id}"));
    }
    (event.account_name != "unknown").then(|| format!("legacy:{}", event.account_name))
}

fn build_statistics(
    snapshot_values: &[Value],
    checkin_values: &[Value],
    accounts: &[Value],
    at_ms: i64,
) -> Value {
    let snapshots = parse_snapshots(snapshot_values, at_ms);
    let today = local_date(at_ms).unwrap_or_else(|| Local::now().date_naive());
    // 与签到日志的清理口径保持一致（同一设置项）
    let checkin_cutoff = at_ms.saturating_sub(record_retention_days() * 24 * 3600 * 1000);
    let checkins: Vec<CheckinEvent> = checkin_values
        .iter()
        .filter_map(parse_checkin_event)
        .filter(|event| event.ts >= checkin_cutoff && event.ts <= at_ms)
        .collect();

    let mut account_ids = Vec::new();
    let mut current_account_ids = Vec::new();
    let mut account_names = HashMap::new();
    for account in accounts {
        if let Some(id) = non_empty_string(account.get("id")) {
            if !current_account_ids.contains(&id) {
                current_account_ids.push(id.clone());
            }
            add_account_name(
                &mut account_ids,
                &mut account_names,
                id,
                account_display_name(account),
            );
        }
    }
    for snapshot in &snapshots {
        add_account_name(
            &mut account_ids,
            &mut account_names,
            snapshot.account_id.clone(),
            snapshot.account_name.clone(),
        );
    }
    for event in &checkins {
        if let Some(account_id) = &event.account_id {
            add_account_name(
                &mut account_ids,
                &mut account_names,
                account_id.clone(),
                event.account_name.clone(),
            );
        }
    }

    let mut by_account: HashMap<String, Vec<Snapshot>> = HashMap::new();
    for snapshot in snapshots {
        by_account
            .entry(snapshot.account_id.clone())
            .or_default()
            .push(snapshot);
    }
    let coverage_start_at = by_account
        .values()
        .flat_map(|snapshots| snapshots.iter().map(|snapshot| snapshot.ts))
        .min();

    let mut usage_events = Vec::new();
    let mut usage_totals: HashMap<String, (f64, f64, f64)> = HashMap::new();
    let mut daily_usage: HashMap<String, HashMap<String, f64>> = HashMap::new();
    let mut latest_snapshots = HashMap::new();
    // 逐账号的「末次 vs 上次」差值，汇总口径的唯一来源（见 `compare_snapshots`）。
    let mut account_comparisons: HashMap<String, CreditComparison> = HashMap::new();
    for (account_id, mut snapshots) in by_account {
        snapshots.sort_by_key(|snapshot: &Snapshot| snapshot.ts);
        if let Some(latest) = snapshots.last() {
            latest_snapshots.insert(account_id.clone(), latest.clone());
        }
        // 汇总口径在这里算：本账号全部快照已按 ts 排序，末次与上次的差就是
        // 「这段区间净变了多少」。与下面的逐事件累加**并行**存在 ——
        // 累加值继续喂逐日趋势图与事件流（明细），差值则作为总量口径。
        account_comparisons.insert(
            account_id.clone(),
            compare_account_snapshots(&snapshots, at_ms),
        );
        for pair in snapshots.windows(2) {
            let previous = &pair[0];
            let current = &pair[1];
            let amount = previous.remaining - current.remaining;
            if amount <= f64::EPSILON {
                continue;
            }
            // 幻影区间不得进入逐日序列 / 时间窗 / 事件流。
            //
            // 为什么这条必须补上（改动前这里是**唯一**漏掉的判据）：
            // 逐事件写记录那条路（`record_snapshot`）已经用 `reading_trust` 拦住了
            // 幻影，但本函数是**从快照文件重新回放**的，它不读 `record_snapshot`
            // 的结论。于是 owner 真实的 `380 → 30 → 380` 序列在这里照样会累加出
            // 「今日消耗 350」—— 而 GatewayPage「消耗积分」列与统计页的
            // 「今日 / 近 7 天 / 本月消耗」读的正是这几个字段。
            // 也就是说：只修记录不修这里，用户看到的数字依旧是错的。
            //
            // 与汇总口径共用同一判据，避免两条口径各说一套。
            if pair_reading_is_phantom(previous, current) {
                continue;
            }
            let Some(date) = local_date_string(current.ts) else {
                continue;
            };
            let entry = usage_totals.entry(account_id.clone()).or_default();
            let (today_usage, week_usage, month_usage) = usage_in_windows(&date, today);
            if today_usage {
                entry.0 += amount;
            }
            if week_usage {
                entry.1 += amount;
            }
            if month_usage {
                entry.2 += amount;
            }
            *daily_usage
                .entry(account_id.clone())
                .or_default()
                .entry(date.clone())
                .or_default() += amount;
            usage_events.push(UsageEvent {
                ts: current.ts,
                date,
                account_id: account_id.clone(),
                amount,
            });
        }
    }

    let mut checkin_today_latest: HashMap<String, (i64, String)> = HashMap::new();
    let mut today_success = 0;
    let mut today_already = 0;
    let mut today_failed = 0;
    for event in &checkins {
        if event.date != today.format("%Y-%m-%d").to_string() {
            continue;
        }
        match event.result.as_str() {
            "success" => today_success += 1,
            "already" => today_already += 1,
            _ => today_failed += 1,
        }
        if let Some(identity) = checkin_identity(event) {
            if checkin_today_latest
                .get(&identity)
                .map(|(ts, _)| *ts <= event.ts)
                .unwrap_or(true)
            {
                checkin_today_latest.insert(identity, (event.ts, event.result.clone()));
            }
        }
    }

    let today_checked_in_accounts = checkin_today_latest
        .values()
        .filter(|(_, result)| result == "success" || result == "already")
        .count();
    let today_key = today.format("%Y-%m-%d").to_string();

    let mut current_remaining = 0.0;
    let mut current_capacity = 0.0;
    for account_id in &current_account_ids {
        if let Some(snapshot) = latest_snapshots.get(account_id) {
            current_remaining += snapshot.remaining;
            current_capacity += snapshot.total;
        }
    }

    let mut last_checkins: HashMap<String, CheckinEvent> = HashMap::new();
    for event in &checkins {
        let Some(account_id) = &event.account_id else {
            continue;
        };
        if last_checkins
            .get(account_id)
            .map(|current: &CheckinEvent| current.ts <= event.ts)
            .unwrap_or(true)
        {
            last_checkins.insert(account_id.clone(), event.clone());
        }
    }

    // 逐日序列起点：全局最早快照与保留窗口下界的较大者；无快照时为 None（返回空序列）
    let daily_start = coverage_start_at.and_then(local_date).map(|coverage_date| {
        let earliest = today - ChronoDuration::days(record_retention_days() - 1);
        coverage_date.max(earliest)
    });
    let empty_account_daily: HashMap<String, f64> = HashMap::new();

    let account_summaries: Vec<Value> = account_ids
        .iter()
        .map(|account_id| {
            let latest = latest_snapshots.get(account_id);
            let is_current = current_account_ids.contains(account_id);
            let (usage_today, usage_week, usage_month) = usage_totals
                .get(account_id)
                .copied()
                .unwrap_or_default();
            let today_checkin = checkins
                .iter()
                .filter(|event| {
                    event.account_id.as_deref() == Some(account_id.as_str())
                        && event.date == today_key
                })
                .max_by_key(|event| event.ts);
            let last_checkin = last_checkins.get(account_id);
            json!({
                "accountId": account_id,
                "accountName": account_names.get(account_id).cloned().unwrap_or_else(|| "unknown".to_string()),
                "isCurrent": is_current,
                "currentRemaining": is_current.then(|| latest.map(|snapshot| snapshot.remaining)).flatten(),
                "totalCapacity": is_current.then(|| latest.map(|snapshot| snapshot.total)).flatten(),
                "lastSnapshotAt": latest.map(|snapshot| snapshot.ts),
                // 汇总口径：该账号「末次快照 vs 上次快照」的完整变化。
                // 与下面三个 usage* 字段**并存**：usage* 是逐事件累加（明细派生的
                // 时间窗切分），change 是快照做差（可信的总量）。前端总量展示优先
                // 用 change，明细与趋势图继续用 usage* / daily。
                //
                // 为什么逐账号也要给：所有者要能看出「是哪个号在烧」，
                // 只给全局汇总的话，一个号的异常会被其他号摊平而看不出来。
                "change": comparison_value(
                    account_comparisons
                        .get(account_id)
                        .unwrap_or(&CreditComparison::none(CREDIT_COMPARISON_NO_BASELINE)),
                ),
                // 账号不在了（被禁用/移除）时，这个差值已不再刷新 —— 如实告诉前端，
                // 否则界面会把一个陈旧数字当成「当前消耗」。
                "usageToday": usage_today,
                "usage7Days": usage_week,
                "usageThisMonth": usage_month,
                "checkedInToday": today_checkin.map(|event| event.result == "success" || event.result == "already"),
                "checkinStatusToday": today_checkin.map(|event| event.result.clone()),
                "lastCheckinAt": last_checkin.map(|event| event.ts),
                "lastCheckinResult": last_checkin.map(|event| event.result.clone()),
                "daily": local_daily_series(
                    daily_usage.get(account_id).unwrap_or(&empty_account_daily),
                    daily_start,
                    today,
                ),
            })
        })
        .collect();

    let mut aggregate_daily: HashMap<String, f64> = HashMap::new();
    for account_daily in daily_usage.values() {
        for (date, amount) in account_daily {
            *aggregate_daily.entry(date.clone()).or_insert(0.0) += amount;
        }
    }
    let daily = local_daily_series(&aggregate_daily, daily_start, today);

    let mut events: Vec<(i64, Value)> = usage_events
        .iter()
        .map(|event| {
            (
                event.ts,
                json!({
                    "kind": "usage",
                    "ts": event.ts,
                    "date": event.date,
                    "accountId": event.account_id,
                    "accountName": account_names.get(&event.account_id).cloned().unwrap_or_else(|| "unknown".to_string()),
                    "amount": event.amount,
                }),
            )
        })
        .collect();
    events.extend(checkins.iter().map(|event| {
        (
            event.ts,
            json!({
                "kind": "checkin",
                "ts": event.ts,
                "date": event.date,
                "accountId": event.account_id,
                "accountName": event.account_id.as_ref().and_then(|id| account_names.get(id)).cloned().unwrap_or_else(|| event.account_name.clone()),
                "result": event.result,
                "error": event.error,
            }),
        )
    }));
    events.sort_by_key(|event| Reverse(event.0));
    let recent_events: Vec<Value> = events
        .into_iter()
        .take(CREDIT_STATS_MAX_EVENTS)
        .map(|(_, event)| event)
        .collect();

    // 全局汇总差值：只累加**可信**的账号差值。
    //
    // 为什么不可信的账号直接跳过而不是当 0 加：当 0 加会让「一个账号缺基准」
    // 静默地把总量算低，用户看不出总量其实只覆盖了一部分账号。
    // 故单独统计可信账号数，前端据此说明「该差值覆盖了几个账号」。
    let mut total_decrease = 0.0;
    let mut total_increase = 0.0;
    let mut comparison_accounts = 0usize;
    // 区间取**可信账号的并集**：不同账号的最后两次快照时刻不必相同，
    // 报一个全局 from/to 才说得清「这个总量是哪段时间的」。
    // 起点取最早、终点取最晚 —— 与「各账号差值之和」实际覆盖的范围一致。
    let mut comparison_from_ts: Option<i64> = None;
    let mut comparison_to_ts: Option<i64> = None;
    // 全局陈旧 = **所有**可信账号的差值都陈旧。只要有一个账号刚采集过，
    // 这份汇总就不是「完全过期」的 —— 用 all 而非 any，避免一个新账号
    // 就把整块数据标成陈旧。
    let mut comparison_all_stale = true;
    for comparison in account_comparisons.values() {
        if !comparison.ok {
            continue;
        }
        comparison_accounts += 1;
        total_decrease += comparison.decrease;
        total_increase += comparison.increase;
        comparison_all_stale &= comparison.stale;
        if let Some(from) = comparison.from_ts {
            comparison_from_ts = Some(comparison_from_ts.map_or(from, |current| current.min(from)));
        }
        if let Some(to) = comparison.to_ts {
            comparison_to_ts = Some(comparison_to_ts.map_or(to, |current| current.max(to)));
        }
    }
    // 全局是否可信：至少要有一个账号有可信差值，否则「无对比基准」。
    // reason 的取值与逐账号同一套，前端 switch 一次即可覆盖两种粒度。
    let comparison_ok = comparison_accounts > 0;
    let comparison_reason = if comparison_ok {
        CREDIT_COMPARISON_OK
    } else if account_comparisons.is_empty() {
        CREDIT_COMPARISON_NO_BASELINE
    } else if account_comparisons
        .values()
        .any(|comparison| comparison.reason == CREDIT_COMPARISON_NO_BASELINE)
    {
        CREDIT_COMPARISON_NO_BASELINE
    } else {
        CREDIT_COMPARISON_UNRELIABLE
    };
    let comparison = json!({
        "ok": comparison_ok,
        "reason": comparison_reason,
        "fromTs": comparison_from_ts,
        "toTs": comparison_to_ts,
        "decrease": total_decrease,
        "increase": total_increase,
        "net": total_increase - total_decrease,
        // 该差值覆盖了几个账号。为 0 时前端必须显示「暂无对比基准」而不是 0。
        "accounts": comparison_accounts,
        // 可信账号全为 0 时 all() 恒真，但那属于「无基准」而非「陈旧」，
        // 故显式要求至少有一个可信账号，避免把两种状态混为一谈。
        "stale": comparison_ok && comparison_all_stale,
    });

    json!({
        "generatedAt": at_ms,
        "retentionDays": record_retention_days(),
        "coverageStartAt": coverage_start_at,
        // 汇总口径（本次修复的核心）：与上一次积分快照做差。
        //
        // 为什么放在**顶层**而不是塞进 summary：它和 summary 里那些字段
        // **不是同一层语义** —— summary.usageToday 等是逐事件累加派生的
        // 时间窗切分，而 comparison 是「末次快照 − 上次快照」的实测差值。
        // 混在一起，调用方迟早会把两者当成同一口径相加或互相校验。
        "comparison": comparison,
        "summary": {
            "currentRemaining": current_remaining,
            "currentCapacity": current_capacity,
            "usageToday": usage_totals.values().map(|value| value.0).sum::<f64>(),
            "usage7Days": usage_totals.values().map(|value| value.1).sum::<f64>(),
            "usageThisMonth": usage_totals.values().map(|value| value.2).sum::<f64>(),
            "todayCheckedInAccounts": today_checked_in_accounts,
            "todaySuccess": today_success,
            "todayAlready": today_already,
            "todayFailed": today_failed,
        },
        "daily": daily,
        "accounts": account_summaries,
        "events": recent_events,
    })
}

/// 返回本地快照、账号列表、签到日志与官方请求用量的统一统计投影。
///
/// `refresh = false` 时官方用量读本地缓存，不打用量接口；
/// 只有统计页「刷新统计」传入 `refresh = true` 才会重新采集。
pub async fn get_statistics(refresh: bool) -> Value {
    let at_ms = now_ms();
    let accounts = load_accounts();
    let mut statistics =
        build_statistics(&load_snapshots(), &load_checkin_logs(), &accounts, at_ms);
    // 免费模型的调用**不扣积分**，因此「只用了免费模型」的账号消耗必为 0。
    //
    // 为什么必须在统计出口做这一步（而不是改 record_snapshot 的归因）：
    // 余额下降是**可观测事实** —— 免费模型也可能伴随余额抖动（实测：读上游
    // 包列表失败时聚合值会短暂掉到 0，被记成 -350 又涨回，即「幻影配对」）。
    // 那是「读数问题」，与「该不该扣分」正交，不能靠删除记录来掩盖。
    // 这里只纠正**展示口径**：既然确定这些调用不产生消耗，就把消耗如实
    // 显示为 0（而不是「—」，因为「—」表示不知道，0 是确定的）。
    //
    // 判定依据是上游权威数据（`/v3/config` 的 credits 倍率，经网关按区域
    // 透出），见 `model_billing` 模块头部的完整说明。拿不到真值 → 全部
    // Unknown → 本步不改任何数字，统计页退化为原有口径。
    let verdicts = crate::modules::model_billing::collect_window_verdicts(&accounts).await;
    crate::modules::model_billing::zero_out_free_windows(&mut statistics, &verdicts);
    statistics["officialUsage"] =
        official_usage::official_usage_for_statistics(&accounts, at_ms, refresh).await;
    statistics
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_local_date(days_ago: i64, hour: u32) -> i64 {
        let today = Local::now().date_naive() - ChronoDuration::days(days_ago);
        Local
            .with_ymd_and_hms(today.year(), today.month(), today.day(), hour, 0, 0)
            .single()
            .expect("valid local test date")
            .timestamp_millis()
    }

    fn snap(ts: i64, remaining: f64) -> Value {
        snapshot_value(ts, "account-1", "one@example.com", 100.0, remaining)
    }

    #[test]
    fn first_observation_does_not_create_usage() {
        let now = at_local_date(0, 12);
        let stats = build_statistics(&[snap(now, 100.0)], &[], &[], now);

        assert_eq!(stats["summary"]["usageToday"], 0.0);
        assert_eq!(stats["daily"][0]["usage"], 0.0);
    }

    #[test]
    fn positive_decreases_are_assigned_to_the_newer_local_day() {
        let now = at_local_date(0, 12);
        let yesterday = at_local_date(1, 12);
        let stats = build_statistics(&[snap(yesterday, 100.0), snap(now, 70.0)], &[], &[], now);

        assert_eq!(stats["summary"]["usageToday"], 30.0);
        assert_eq!(stats["summary"]["usage7Days"], 30.0);
        assert_eq!(
            stats["daily"].as_array().unwrap().last().unwrap()["usage"],
            30.0
        );
        assert_eq!(stats["events"][0]["kind"], "usage");
        assert_eq!(stats["events"][0]["amount"], 30.0);
    }

    #[test]
    fn increases_reset_the_baseline_without_negative_usage() {
        let now = at_local_date(0, 12);
        let earlier = at_local_date(2, 12);
        let yesterday = at_local_date(1, 12);
        let stats = build_statistics(
            &[
                snap(earlier, 100.0),
                snap(yesterday, 125.0),
                snap(now, 115.0),
            ],
            &[],
            &[],
            now,
        );

        assert_eq!(stats["summary"]["usageToday"], 10.0);
        assert_eq!(stats["summary"]["usage7Days"], 10.0);
        assert_eq!(stats["events"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn retention_and_month_windows_use_local_calendar_dates() {
        let now = at_local_date(0, 12);
        let old = at_local_date(CREDIT_SNAPSHOT_RETENTION_DAYS + 1, 12);
        let stats = build_statistics(&[snap(old, 100.0), snap(now, 80.0)], &[], &[], now);

        assert_eq!(stats["summary"]["usage7Days"], 0.0);
        assert_eq!(stats["summary"]["usageThisMonth"], 0.0);
        assert_eq!(stats["coverageStartAt"], now);
    }

    #[test]
    fn checkin_events_are_separate_from_credit_usage() {
        let now = at_local_date(0, 12);
        let logs = vec![json!({
            "ts": now,
            "accountId": "account-1",
            "email": "one@example.com",
            "result": "success",
        })];
        let stats = build_statistics(
            &[snap(now - 60_000, 100.0), snap(now, 90.0)],
            &logs,
            &[],
            now,
        );

        assert_eq!(stats["summary"]["usageToday"], 10.0);
        assert_eq!(stats["summary"]["todayCheckedInAccounts"], 1);
        assert_eq!(stats["events"].as_array().unwrap().len(), 2);
        let event_kinds: Vec<&str> = stats["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|event| event["kind"].as_str())
            .collect();
        assert!(event_kinds.contains(&"checkin"));
        assert!(event_kinds.contains(&"usage"));
    }

    #[test]
    fn checkins_without_identity_are_kept_as_events_but_not_counted_as_accounts() {
        let now = at_local_date(0, 12);
        let logs = vec![json!({
            "ts": now,
            "result": "success",
        })];
        let stats = build_statistics(&[], &logs, &[], now);

        assert_eq!(stats["summary"]["todayCheckedInAccounts"], 0);
        assert_eq!(stats["events"][0]["kind"], "checkin");
    }

    #[test]
    fn historical_only_accounts_do_not_look_current() {
        let now = at_local_date(0, 12);
        let stats = build_statistics(
            &[snap(now - 60_000, 100.0), snap(now, 80.0)],
            &[],
            &[json!({"id": "current-account"})],
            now,
        );

        assert_eq!(stats["summary"]["currentRemaining"], 0.0);
        let historical = stats["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|account| account["accountId"] == "account-1")
            .expect("historical account summary");
        assert_eq!(historical["isCurrent"], false);
        assert!(historical["currentRemaining"].is_null());
        assert_eq!(historical["lastSnapshotAt"], now);
    }

    #[test]
    fn snapshot_normalization_filters_old_values_and_caps_records() {
        let now = at_local_date(0, 12);
        let old = snap(
            now - (CREDIT_SNAPSHOT_RETENTION_DAYS + 1) * 24 * 3600 * 1000,
            100.0,
        );
        let mut values = vec![old];
        values.extend((0..(CREDIT_SNAPSHOT_MAX_RECORDS + 1)).map(|index| {
            snap(
                now - (CREDIT_SNAPSHOT_MAX_RECORDS as i64 - index as i64) * 1_000,
                100.0,
            )
        }));

        let normalized = normalize_snapshots(&values, now);

        assert_eq!(normalized.len(), CREDIT_SNAPSHOT_MAX_RECORDS);
        assert!(normalized.iter().all(|value| {
            snapshot_from_value(value)
                .map(|snapshot| {
                    snapshot.ts >= now - CREDIT_SNAPSHOT_RETENTION_DAYS * 24 * 3600 * 1000
                })
                .unwrap_or(false)
        }));
    }

    #[test]
    fn same_value_is_suppressed_only_inside_the_short_window() {
        let now = at_local_date(0, 12);
        let previous = snap(now - CREDIT_SNAPSHOT_DEDUPE_WINDOW_MS - 1, 100.0);
        assert!(!should_suppress_duplicate(
            std::slice::from_ref(&previous),
            "account-1",
            100.0,
            100.0,
            now,
        ));
        assert!(should_suppress_duplicate(
            &[snap(now - CREDIT_SNAPSHOT_DEDUPE_WINDOW_MS + 1, 100.0)],
            "account-1",
            100.0,
            100.0,
            now,
        ));
    }

    // -----------------------------------------------------------------------
    // 积分来源判定：区分「额度发放 / 调用扣减 / 额度到期 / 其他调整」
    //
    // 这些判据直接来自实测真实快照（见交付报告）：
    //   上升 158 次中 141 次容量与余额增量相等 → grant
    //   下降 433 次容量不变                    → consume
    //   下降   5 次容量同降                    → expire
    // -----------------------------------------------------------------------

    /// 真实样例：新积分包到账，容量 +100、余额 +100。
    #[test]
    fn capacity_and_balance_rising_together_is_a_grant() {
        let delta = credit_delta(1000.0, 1000.0, 1100.0, 1100.0).expect("应有变化");
        assert_eq!(delta.amount, 100);
        assert_eq!(delta.capacity, 100);
        assert_eq!(
            classify_credit_source(delta.amount, delta.capacity),
            crate::modules::account_records::CREDIT_SOURCE_GRANT
        );
    }

    /// 真实样例（acc-6091…，2026-09-16）：容量 +1650 而余额只 +1620.73 ——
    /// 到账与本次快照之间已经消耗掉一部分。这仍是 grant，不是 adjust。
    #[test]
    fn grant_still_holds_when_part_of_the_grant_was_already_spent() {
        let delta = credit_delta(4656.62, 4700.0, 6277.35, 6350.0).expect("应有变化");
        assert_eq!(delta.amount, 1621);
        assert_eq!(delta.capacity, 1650);
        assert_eq!(
            classify_credit_source(delta.amount, delta.capacity),
            crate::modules::account_records::CREDIT_SOURCE_GRANT,
            "容量增量大于余额增量（到账后已消耗）仍属额度发放"
        );
    }

    /// 真实样例：余额降、容量不变 = 纯消耗（实测 433 次全部如此）。
    #[test]
    fn balance_falling_with_stable_capacity_is_consumption() {
        let delta = credit_delta(5000.0, 5000.0, 4969.0, 5000.0).expect("应有变化");
        assert_eq!(delta.amount, -31);
        assert_eq!(delta.capacity, 0);
        assert_eq!(
            classify_credit_source(delta.amount, delta.capacity),
            crate::modules::account_records::CREDIT_SOURCE_CONSUME
        );
    }

    /// 真实样例（acc-da16…，2026-09-14）：某积分包容量 -100、余额 -79 ——
    /// 包内还剩 79 分就整包失效，那些分是**到期蒸发**而不是被调用消耗掉。
    #[test]
    fn balance_and_capacity_falling_together_is_expiry() {
        let delta = credit_delta(3000.0, 3000.0, 2921.0, 2900.0).expect("应有变化");
        assert_eq!(delta.amount, -79);
        assert_eq!(delta.capacity, -100);
        assert_eq!(
            classify_credit_source(delta.amount, delta.capacity),
            crate::modules::account_records::CREDIT_SOURCE_EXPIRE,
            "容量同降说明是额度被回收，不是消耗"
        );
    }

    /// 容量没变而余额涨了：积分是**退回来**的，不是新增额度。
    /// 若判成 grant，用户会以为额度包变多了 —— 那是不实描述。
    #[test]
    fn balance_rising_without_capacity_is_an_adjustment_not_a_grant() {
        let delta = credit_delta(1000.0, 1000.0, 1100.0, 1000.0).expect("应有变化");
        assert_eq!(delta.amount, 100);
        assert_eq!(delta.capacity, 0);
        assert_eq!(
            classify_credit_source(delta.amount, delta.capacity),
            crate::modules::account_records::CREDIT_SOURCE_ADJUST
        );
    }

    #[test]
    fn unchanged_balance_yields_no_delta() {
        // 没有变化就不该记一条 amount=0 的噪音（巡检每 15 分钟一次）
        assert!(credit_delta(100.0, 100.0, 100.0, 100.0).is_none());
        // 浮点误差也要被 round 吸收掉
        assert!(credit_delta(100.0, 100.0, 100.0000001, 100.0).is_none());
    }

    /// 端到端：写入第二个快照后，积分记录必须带上来源，且是**可读回**的。
    #[test]
    fn second_snapshot_writes_a_credit_record_with_source() {
        let _iso = crate::modules::config::test_isolation::Isolated::new("credit-source-e2e");

        // 首个快照只建立基线，不产生记录
        assert!(record_snapshot("acc-src", "n", 1000.0, 1000.0, Default::default()));
        let v = crate::modules::account_records::query_records("acc-src", 0, 0, &[], 100);
        assert_eq!(v.get("total").and_then(Value::as_u64), Some(0), "首个快照不该产生记录");

        // 余额 +100 且容量 +100 → 额度发放
        assert!(record_snapshot("acc-src", "n", 1100.0, 1100.0, Default::default()));
        let v = crate::modules::account_records::query_records(
            "acc-src",
            0,
            0,
            &[crate::modules::account_records::KIND_CREDIT.to_string()],
            100,
        );
        let recs = v.get("records").and_then(Value::as_array).unwrap();
        assert_eq!(recs.len(), 1, "应写入一条积分记录");
        assert_eq!(
            recs[0].get("source").and_then(Value::as_str),
            Some(crate::modules::account_records::CREDIT_SOURCE_GRANT)
        );
        assert_eq!(recs[0].get("amount").and_then(Value::as_i64), Some(100));
        // 标题要能自解释，而不是笼统的「积分增长」
        let title = recs[0].get("title").and_then(Value::as_str).unwrap_or("");
        assert!(title.contains("额度发放"), "标题应含来源，实际: {title}");
        // detail 要给出判据（容量变化），让用户能自行核对
        let detail = recs[0].get("detail").and_then(Value::as_str).unwrap_or("");
        assert!(detail.contains("容量"), "detail 应写明容量判据，实际: {detail}");
    }

    /// 端到端：纯消耗要记成 consume，而不是笼统的「积分消耗」。
    #[test]
    fn pure_consumption_snapshot_records_consume_source() {
        let _iso = crate::modules::config::test_isolation::Isolated::new("credit-consume-e2e");
        assert!(record_snapshot("acc-c", "n", 1000.0, 1000.0, Default::default()));
        assert!(record_snapshot("acc-c", "n", 1000.0, 900.0, Default::default()));

        let v = crate::modules::account_records::query_records(
            "acc-c",
            0,
            0,
            &[crate::modules::account_records::KIND_CREDIT.to_string()],
            100,
        );
        let recs = v.get("records").and_then(Value::as_array).unwrap();
        // 先钉住条数：首条快照只建立基线不产生记录，故这里恰好 1 条。
        // 有条数断言时按下标取值才是确定的（同 ts 的顺序问题只影响多条时）。
        assert_eq!(recs.len(), 1, "首个快照不该产生记录，应恰好 1 条");
        assert_eq!(
            recs[0].get("source").and_then(Value::as_str),
            Some(crate::modules::account_records::CREDIT_SOURCE_CONSUME)
        );
        assert_eq!(recs[0].get("amount").and_then(Value::as_i64), Some(-100));
    }

    // -----------------------------------------------------------------------
    // 幻影积分记录（所有者实测反馈：「发放的积分根本没有变化 但是这里记录
    // 却是 -380 然后又 +380」）
    //
    // 根因：上游偶发返回**不完整的包列表**，聚合 total/remaining 凭空下跌再涨回，
    // 被当成「消耗」+「发放」记成一对。下面用 owner 的**真实数值**做回归。
    // -----------------------------------------------------------------------

    /// 构造一份包子集。
    fn packs(entries: &[(&str, f64)]) -> std::collections::BTreeMap<String, f64> {
        entries.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    /// 判据 2（核心）：`total`（额度容量）不应因消耗而减少。
    ///
    /// owner 真实数据：某次上游只返回 1 个 30 的包 → total 从 380 掉到 30，
    /// 于是被记成「-350 积分消耗」。但**容量不会因为消耗而变小**，
    /// 所以 total 下降本身就证明「这次没读全」。
    #[test]
    fn reading_trust_rejects_dropped_capacity() {
        let prev = snapshot_from_value(&json!({
            "accountId": "a", "ts": 1, "total": 380.0, "remaining": 380.0
        }))
        .unwrap();
        // total 380 → 30（同时 remaining 也跌）⇒ 必须判为不可信
        assert_eq!(
            reading_trust(&prev, 30.0, 30.0, &packs(&[("p30", 30.0)])),
            ReadingTrust::Incomplete("额度容量意外减少，上游本次返回的积分包列表不完整"),
        );
        // total 380 → 0 ⇒ 同样不可信
        assert!(matches!(
            reading_trust(&prev, 0.0, 0.0, &packs(&[])),
            ReadingTrust::Incomplete(_)
        ));
    }

    /// 正常消耗：`total` 不变、只 `remaining` 跌 ⇒ 必须放行（否则会误杀真实消耗）。
    ///
    /// 这是本判据**不能**用「跌幅大小」代替的原因：消耗快时余额掉得快是正常的。
    #[test]
    fn reading_trust_allows_real_consumption() {
        let prev = snapshot_from_value(&json!({
            "accountId": "a", "ts": 1, "total": 1000.0, "remaining": 1000.0
        }))
        .unwrap();
        // 容量没变、余额跌到 0（消耗快）⇒ 真实消耗，放行
        assert_eq!(reading_trust(&prev, 1000.0, 0.0, &packs(&[("p1", 0.0)])), ReadingTrust::Ok);
        // 容量没变、余额小跌 ⇒ 放行
        assert_eq!(reading_trust(&prev, 1000.0, 900.0, &packs(&[("p1", 900.0)])), ReadingTrust::Ok);
    }

    /// 判据 1：包凭空消失（下次又回来）⇒ 读数失败。
    ///
    /// 包不会自己消失再回来 —— 真到期有 expireAt 为证，真消耗只减 remaining 不删包。
    #[test]
    fn reading_trust_rejects_vanished_package() {
        let prev = snapshot_from_value(&json!({
            "accountId": "a", "ts": 1, "total": 380.0, "remaining": 380.0,
            "packages": { "p380": 380.0 }
        }))
        .unwrap();
        // 这次只剩 p30，p380 不见了，且余额也跌 ⇒ 不可信
        assert!(matches!(
            reading_trust(&prev, 30.0, 30.0, &packs(&[("p30", 30.0)])),
            ReadingTrust::Incomplete(_)
        ));
    }

    /// 真实到期（包消失 + 容量跌）在**没有 expireAt 证据**时也会被判不可信 ——
    /// 这是刻意的保守：宁可漏记一次真到期，也不要刷出一对幻影记录。
    /// （真到期会在下一次读数稳定后由「容量确实少了且不再回来」体现。）
    #[test]
    fn reading_trust_is_conservative_when_package_disappears() {
        let prev = snapshot_from_value(&json!({
            "accountId": "a", "ts": 1, "total": 1000.0, "remaining": 900.0,
            "packages": { "p1": 500.0, "p2": 400.0 }
        }))
        .unwrap();
        // p2 消失、总容量 1000→500
        assert!(matches!(
            reading_trust(&prev, 500.0, 500.0, &packs(&[("p1", 500.0)])),
            ReadingTrust::Incomplete(_)
        ));
    }

    /// 新包到账（total 上升）必须放行 —— 那是真实的额度发放。
    #[test]
    fn reading_trust_allows_grant() {
        let prev = snapshot_from_value(&json!({
            "accountId": "a", "ts": 1, "total": 380.0, "remaining": 100.0,
            "packages": { "p380": 100.0 }
        }))
        .unwrap();
        assert_eq!(
            reading_trust(&prev, 760.0, 480.0, &packs(&[("p380", 100.0), ("p380b", 380.0)])),
            ReadingTrust::Ok
        );
    }

    /// 老快照没有包级明细时判据 2 仍生效（向后兼容：老数据也能被保护）。
    #[test]
    fn reading_trust_works_without_package_detail() {
        let prev = snapshot_from_value(&json!({
            "accountId": "a", "ts": 1, "total": 380.0, "remaining": 380.0
        }))
        .unwrap();
        assert!(prev.packages.is_empty(), "老快照应解析成空包集");
        // 无包级明细 → 判据 1 跳过，判据 2 仍拦下 total 下降
        assert!(matches!(
            reading_trust(&prev, 30.0, 30.0, &packs(&[])),
            ReadingTrust::Incomplete(_)
        ));
        // 无包级明细 + total 不变 → 放行
        assert_eq!(reading_trust(&prev, 380.0, 200.0, &packs(&[])), ReadingTrust::Ok);
    }

    /// 端到端：owner 的幻影配对**不再产生任何积分记录**。
    ///
    /// 复现真实序列：380 → 30（只读到 1 个包）→ 380（读全了）。
    /// 修复前会记「-350 消耗」+「+350 增长」；修复后应为 0 条。
    #[test]
    fn phantom_pair_is_not_recorded() {
        let _iso = crate::modules::config::test_isolation::Isolated::new("credit-phantom-pair");

        // 基线：完整读到 380 的包
        assert!(record_snapshot("acc-ph", "n", 380.0, 380.0, packs(&[("p380", 380.0)])));
        // 上游这次只返回了 30 的包（不完整）—— 修复前会记「-350 消耗」
        assert!(record_snapshot("acc-ph", "n", 30.0, 30.0, packs(&[("p30", 30.0)])));
        // 下次读全了 —— 修复前会记「+350 增长」
        assert!(record_snapshot("acc-ph", "n", 380.0, 380.0, packs(&[("p380", 380.0)])));

        let v = crate::modules::account_records::query_records(
            "acc-ph",
            0,
            0,
            &[crate::modules::account_records::KIND_CREDIT.to_string()],
            100,
        );
        let recs = v.get("records").and_then(Value::as_array).unwrap();
        assert_eq!(
            recs.len(),
            0,
            "幻影配对不该产生任何积分记录，实际产生了 {} 条: {:?}",
            recs.len(),
            recs.iter()
                .map(|r| (r.get("amount"), r.get("title")))
                .collect::<Vec<_>>()
        );
    }

    /// 对照：真实消耗在同样流程下**仍要**被记录（不能因为修幻影而把真消耗也吞掉）。
    #[test]
    fn real_consumption_still_recorded_after_fix() {
        let _iso = crate::modules::config::test_isolation::Isolated::new("credit-real-consume");

        assert!(record_snapshot("acc-rc", "n", 380.0, 380.0, packs(&[("p380", 380.0)])));
        // 容量不变、余额跌 350 ⇒ 真实消耗，必须记
        assert!(record_snapshot("acc-rc", "n", 380.0, 30.0, packs(&[("p380", 30.0)])));

        let v = crate::modules::account_records::query_records(
            "acc-rc",
            0,
            0,
            &[crate::modules::account_records::KIND_CREDIT.to_string()],
            100,
        );
        let recs = v.get("records").and_then(Value::as_array).unwrap();
        assert_eq!(recs.len(), 1, "真实消耗必须被记录");
        assert_eq!(recs[0].get("amount").and_then(Value::as_i64), Some(-350));
        assert_eq!(
            recs[0].get("source").and_then(Value::as_str),
            Some(crate::modules::account_records::CREDIT_SOURCE_CONSUME)
        );
    }

    /// **最容易误伤的一条**：真实消耗之后又有真实发放，必须**两条都记**。
    ///
    /// 序列：`380 → 30（真消耗，包还在）→ 760（真发放，来了新包）`
    /// 若判据 3 写成「数值回升 = 恢复」就会把第二条件吞掉 —— 这正是我第一版
    /// 用「回到旧值」做判据时的错误。用 `suspect` 标志才能区分：
    ///   · 真消耗那次**不可疑**（包在、只是余额少）→ 下一次回升是**发放**，要记
    ///   · 抖动那次**可疑**（包消失）→ 下一次回升是**恢复**，不记
    #[test]
    fn real_grant_after_real_consumption_is_still_recorded() {
        let _iso =
            crate::modules::config::test_isolation::Isolated::new("credit-grant-after-consume");

        assert!(record_snapshot("acc-gc", "n", 380.0, 380.0, packs(&[("p380", 380.0)])));
        // 真消耗：包还在，余额跌到 30（不可疑）
        assert!(record_snapshot("acc-gc", "n", 380.0, 30.0, packs(&[("p380", 30.0)])));
        // 真发放：来了一个新包，总容量 380 → 760
        assert!(record_snapshot(
            "acc-gc",
            "n",
            760.0,
            410.0,
            packs(&[("p380", 30.0), ("p380b", 380.0)])
        ));

        let v = crate::modules::account_records::query_records(
            "acc-gc",
            0,
            0,
            &[crate::modules::account_records::KIND_CREDIT.to_string()],
            100,
        );
        let recs = v.get("records").and_then(Value::as_array).unwrap();
        assert_eq!(
            recs.len(),
            2,
            "真消耗 + 真发放都要记，实际 {:?}",
            recs.iter()
                .map(|r| (r.get("amount"), r.get("title")))
                .collect::<Vec<_>>()
        );

        // **不要按下标断言顺序**：三条快照在同一毫秒内写完时 `ts` 相同，
        // 而 query_records 用稳定排序按 ts 降序 —— 同 ts 会保留插入顺序，
        // 于是 recs[0] 是 -350 而不是 +380。
        //
        // 这一点在 Linux CI 上暴露（本地 Windows 通过）：
        // `real_grant_after_real_consumption_is_still_recorded` 报
        // 「最新一条应是发放 +380，left: Some(-350)」。
        // 顺序在同 ts 下不是被测语义（两条记录都写对了才是），
        // 所以按**内容**定位，让断言与平台时钟精度无关。
        let find = |amount: i64| {
            recs.iter()
                .find(|r| r.get("amount").and_then(Value::as_i64) == Some(amount))
                .unwrap_or_else(|| panic!("找不到 amount={amount} 的记录，实际 {recs:?}"))
        };
        let consumed = find(-350);
        assert!(
            consumed
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .contains("调用扣减"),
            "消耗那条应是「调用扣减」，实际 {:?}",
            consumed.get("title")
        );
        let granted = find(380);
        assert!(
            granted
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .contains("额度发放"),
            "发放那条应是「额度发放」，实际 {:?}",
            granted.get("title")
        );
    }

    /// 新快照要带包级明细（否则判据 1 永远用不上）。
    #[test]
    fn new_snapshot_carries_package_detail() {
        let _iso = crate::modules::config::test_isolation::Isolated::new("credit-snapshot-packages");
        assert!(record_snapshot(
            "acc-pk",
            "n",
            380.0,
            380.0,
            packs(&[("p380", 380.0)])
        ));
        let snapshots = load_snapshots();
        let mine = snapshots
            .iter()
            .filter_map(snapshot_from_value)
            .find(|s| s.account_id == "acc-pk")
            .expect("应写入快照");
        assert_eq!(mine.packages.get("p380"), Some(&380.0));
    }

    // =======================================================================
    // 汇总口径：与上一次积分快照做差
    //
    // 所有者原话：「关于积分消耗并不准确，其实只要对比上次积分快照，就能看到
    // 完整的积分变化了，我想了想还是这个靠谱」。
    //
    // 下面按**他点名的每一条边界**逐一钉死。这些断言之所以写成独立单测而不是
    // 藏在端到端里：每一条都对应一种会把数字算错的具体情形，坏了要能一眼看出
    // 是哪一种坏的（端到端只会报「总数不对」，还得再猜）。
    // =======================================================================

    /// 带 `total` / `packages` / `suspect` 的快照值，供汇总口径的单测构造输入。
    ///
    /// 与 `snap` 分开：`snap` 固定 total=100，用来测逐事件那条路；
    /// 汇总口径必须能独立控制 total（容量下降是它的一条判据）与 suspect。
    /// 包级明细默认给一个与 remaining 同值的包 —— 让两侧包集合一致，
    /// 免得判据 1（包凭空消失）在无关用例里意外生效。
    fn full_snap(ts: i64, total: f64, remaining: f64) -> Value {
        json!({
            "ts": ts,
            "accountId": "account-1",
            "accountName": "one@example.com",
            "total": total,
            "remaining": remaining,
            "packages": { "p0": remaining },
        })
    }

    fn snap_of(ts: i64, total: f64, remaining: f64) -> Snapshot {
        snapshot_from_value(&full_snap(ts, total, remaining)).expect("valid snapshot")
    }

    /// 边界一：**首次运行没有「上次快照」→ 不报虚假巨额消耗**。
    ///
    /// 这是所有者最担心的一种错法：一条快照就把它的余额整份报成「消耗」。
    /// 故必须 `ok = false` 且 decrease/increase 都是 0，界面据此显示「—」。
    #[test]
    fn first_run_without_baseline_reports_nothing() {
        let now = at_local_date(0, 12);
        let stats = build_statistics(&[snap(now, 380.0)], &[], &[], now);

        assert_eq!(stats["comparison"]["ok"], false, "首次运行不该给出可信差值");
        assert_eq!(
            stats["comparison"]["reason"],
            CREDIT_COMPARISON_NO_BASELINE
        );
        assert_eq!(stats["comparison"]["decrease"], 0.0);
        assert_eq!(stats["comparison"]["increase"], 0.0);
        assert_eq!(stats["comparison"]["accounts"], 0);

        // 逐账号那一份同样是「无基准」，前端不会拿它显示 0。
        assert_eq!(stats["accounts"][0]["change"]["ok"], false);
        assert_eq!(
            stats["accounts"][0]["change"]["reason"],
            CREDIT_COMPARISON_NO_BASELINE
        );
    }

    /// 完全没有任何快照时也不能崩，同样报「无基准」。
    #[test]
    fn no_snapshots_at_all_reports_no_baseline() {
        let now = at_local_date(0, 12);
        let stats = build_statistics(&[], &[], &[], now);

        assert_eq!(stats["comparison"]["ok"], false);
        assert_eq!(stats["comparison"]["reason"], CREDIT_COMPARISON_NO_BASELINE);
        assert_eq!(stats["comparison"]["accounts"], 0);
    }

    /// 边界二：**余额增加（签到/发放）要与消耗分开表达**。
    ///
    /// 序列 `380 → 430`：这是纯发放。若只给一个带符号的 net，界面会把
    /// 「发放 50」显示成「消耗 -50」；故 decrease=0 / increase=50 必须分开。
    #[test]
    fn balance_increase_is_reported_as_increase_not_negative_usage() {
        let now = at_local_date(0, 12);
        let prev = now - 30 * 60 * 1000;
        let stats = build_statistics(
            &[full_snap(prev, 380.0, 380.0), full_snap(now, 430.0, 430.0)],
            &[],
            &[],
            now,
        );

        assert_eq!(stats["comparison"]["ok"], true);
        assert_eq!(stats["comparison"]["decrease"], 0.0, "发放不该被算成消耗");
        assert_eq!(stats["comparison"]["increase"], 50.0);
        assert_eq!(stats["comparison"]["net"], 50.0);
        // 区间必须说清是哪一段（所有者点名要求）
        assert_eq!(stats["comparison"]["fromTs"], prev);
        assert_eq!(stats["comparison"]["toTs"], now);
    }

    /// 同一区间内**又消耗又发放**：两者都要如实报出，净额是它们的差。
    ///
    /// 序列 `500 → 400 → 450`：消耗 100、发放 50。若只报净额 −50，
    /// 用户会以为只烧了 50 —— 而实际烧了 100（这正是旧口径被抱怨的形态）。
    #[test]
    fn consumption_and_grant_in_the_same_window_are_both_reported() {
        let now = at_local_date(0, 12);
        let mid = now - 30 * 60 * 1000;
        let start = now - 60 * 60 * 1000;
        let stats = build_statistics(
            &[
                full_snap(start, 500.0, 500.0),
                full_snap(mid, 500.0, 400.0),
                full_snap(now, 500.0, 450.0),
            ],
            &[],
            &[],
            now,
        );

        // 汇总口径只看**末次 vs 上次**：400 → 450 是发放 50，消耗 0。
        // 这是刻意的：差值只覆盖最后一次快照区间，不把更早的消耗重复计入。
        assert_eq!(stats["comparison"]["fromTs"], mid);
        assert_eq!(stats["comparison"]["increase"], 50.0);
        assert_eq!(stats["comparison"]["decrease"], 0.0);

        // 而更早那次（500 → 400）的消耗仍完整保留在**明细**口径里，
        // 证明「汇总」没有吃掉「明细」—— 两者并存正是本次设计的要求。
        assert_eq!(stats["summary"]["usageToday"], 100.0, "逐事件累加仍是全量");
        let usage_amounts: Vec<f64> = stats["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["kind"] == "usage")
            .map(|event| event["amount"].as_f64().unwrap())
            .collect();
        assert!(usage_amounts.contains(&100.0), "明细里应留着那 100 的消耗");
    }

    /// 边界三：**账号被禁用/移除后重新出现 → 中间空档不算消耗**。
    ///
    /// 构造：上次快照在 10 小时前（远超 `CREDIT_SNAPSHOT_MAX_GAP_MS`），
    /// 余额从 1000 掉到 400。若直接做差就会报「消耗 600」，
    /// 但那 600 是账号缺席这段时间里发生的，不是本区间可归因的消耗。
    /// 必须判为无基准。
    #[test]
    fn account_reappearing_after_a_gap_does_not_count_the_gap_as_usage() {
        let now = at_local_date(0, 12);
        let stale = now - 10 * 3600 * 1000;
        let stats = build_statistics(
            &[full_snap(stale, 1000.0, 1000.0), full_snap(now, 1000.0, 400.0)],
            &[],
            &[],
            now,
        );

        assert_eq!(
            stats["comparison"]["ok"], false,
            "空档过大时不该报出一个假消耗"
        );
        assert_eq!(stats["comparison"]["decrease"], 0.0);
        assert_eq!(stats["accounts"][0]["change"]["ok"], false);
    }

    /// 空档的**边界值**：刚好等于上限放行，超过 1ms 就拦下。
    ///
    /// 为什么测这个而不是只测「10 小时」：只有钉住阈值两侧，
    /// 才算证明是**这个**判据在起作用，而不是碰巧因为别的原因通过。
    #[test]
    fn gap_threshold_is_inclusive_at_the_limit() {
        let now = at_local_date(0, 12);
        let at_limit = now - CREDIT_SNAPSHOT_MAX_GAP_MS;
        let over_limit = now - CREDIT_SNAPSHOT_MAX_GAP_MS - 1;

        let ok = compare_snapshots(
            &snap_of(at_limit, 1000.0, 1000.0),
            &snap_of(now, 1000.0, 900.0),
        );
        assert!(ok.ok, "恰好等于上限应放行");
        assert_eq!(ok.decrease, 100.0);

        let rejected = compare_snapshots(
            &snap_of(over_limit, 1000.0, 1000.0),
            &snap_of(now, 1000.0, 900.0),
        );
        assert!(!rejected.ok, "超过上限 1ms 就该判无基准");
        assert_eq!(rejected.reason, CREDIT_COMPARISON_NO_BASELINE);
    }

    /// 边界四：**上游查询失败 → 不得把失败写成 0**。
    ///
    /// 「查询失败」在本模块的体现是：那次读数**根本没有写进快照**
    ///（`credits::get_credit_expiry` 失败时直接返回 ok:false，不调 record_snapshot）。
    /// 所以快照序列里只会看到「成功的那几次」，中间没有 0。
    ///
    /// 这里用两条真实存在的快照做差时，若上一次快照已经**过旧**
    ///（说明中间多次查询都没成功），必须报无基准而不是硬算 —— 这正是
    /// 「0 和不知道是两件事」在汇总口径上的落点。
    #[test]
    fn failed_upstream_queries_do_not_become_zero_usage() {
        let now = at_local_date(0, 12);
        // 上一次成功采集在 5 小时前；中间若干次查询失败 → 没有快照。
        let last_success = now - 5 * 3600 * 1000;
        let stats = build_statistics(
            &[
                full_snap(last_success, 800.0, 800.0),
                full_snap(now, 800.0, 750.0),
            ],
            &[],
            &[],
            now,
        );

        // 关键断言：要么给可信值，要么明确「不知道」；**绝不允许**是 0 却 ok=true。
        let ok = stats["comparison"]["ok"].as_bool().unwrap();
        let decrease = stats["comparison"]["decrease"].as_f64().unwrap();
        assert!(!ok, "中途大量查询失败时不该声称知道区间消耗");
        assert_ne!(
            (ok, decrease),
            (true, 0.0),
            "不得把失败静默写成 0 消耗"
        );
        assert_eq!(decrease, 0.0);
        assert_eq!(stats["comparison"]["accounts"], 0, "可信账号数必须为 0");
    }

    /// 查询失败后恢复：下一次成功采集（间隔正常）应立刻给出可信差值。
    ///
    /// 证明「无基准」不是粘性状态 —— 它是数据不足，不是功能坏了。
    #[test]
    fn comparison_recovers_after_a_successful_follow_up() {
        let now = at_local_date(0, 12);
        let stats = build_statistics(
            &[
                full_snap(now - 30 * 60 * 1000, 800.0, 800.0),
                full_snap(now, 800.0, 750.0),
            ],
            &[],
            &[],
            now,
        );

        assert_eq!(stats["comparison"]["ok"], true);
        assert_eq!(stats["comparison"]["decrease"], 50.0);
        assert_eq!(stats["comparison"]["accounts"], 1);
    }

    /// 边界五：**时间跨度要说清** —— 差值覆盖的是「两次快照之间」。
    ///
    /// 断言 fromTs/toTs 恰好等于最后两条快照的时刻，而不是「今天 0 点」或
    /// 「统计时刻」之类想当然的区间。前端就是拿这两个值渲染
    /// 「MM-DD HH:mm 至 MM-DD HH:mm」的。
    #[test]
    fn comparison_reports_the_exact_window_it_covers() {
        let now = at_local_date(0, 12);
        let prev = now - 47 * 60 * 1000;
        let stats = build_statistics(
            &[full_snap(prev, 380.0, 380.0), full_snap(now, 380.0, 300.0)],
            &[],
            &[],
            now,
        );

        assert_eq!(stats["comparison"]["fromTs"], prev);
        assert_eq!(stats["comparison"]["toTs"], now);
        assert_eq!(stats["comparison"]["decrease"], 80.0);
        // 逐账号那一份的区间必须与全局一致（同一份数据，不该有两个说法）
        assert_eq!(stats["accounts"][0]["change"]["fromTs"], prev);
        assert_eq!(stats["accounts"][0]["change"]["toTs"], now);
    }

    /// 多条快照时，汇总口径只认**最后两条**，不是首尾做差。
    ///
    /// 为什么这是对的：区间被定义为「上一次快照到现在」。若拿首尾做差，
    /// 区间会横跨整个保留期，与界面标的 from/to 不符。
    #[test]
    fn comparison_uses_the_last_two_snapshots_only() {
        let now = at_local_date(0, 12);
        let stats = build_statistics(
            &[
                full_snap(now - 90 * 60 * 1000, 1000.0, 1000.0),
                full_snap(now - 30 * 60 * 1000, 1000.0, 600.0),
                full_snap(now, 1000.0, 550.0),
            ],
            &[],
            &[],
            now,
        );

        // 末次 vs 上次 = 600 → 550，消耗 50（而不是首尾的 450）
        assert_eq!(stats["comparison"]["decrease"], 50.0);
        assert_eq!(stats["comparison"]["fromTs"], now - 30 * 60 * 1000);
    }

    /// 抖动读数（`suspect`）不得进入汇总 —— 两端任一可疑就判不可信。
    ///
    /// 这是幻影配对（`-380` / `+380`）在汇总口径上的防复发：
    /// 逐事件那条路由 `reading_trust` 拦，汇总这条路由 `suspect` 拦。
    #[test]
    fn suspect_readings_are_excluded_from_the_comparison() {
        let now = at_local_date(0, 12);
        let mut suspicious = full_snap(now, 380.0, 380.0);
        suspicious["suspect"] = json!(true);

        let result = compare_snapshots(
            &snap_of(now - 30 * 60 * 1000, 380.0, 380.0),
            &snapshot_from_value(&suspicious).unwrap(),
        );

        assert!(!result.ok, "可疑读数不该产生可信差值");
        assert_eq!(result.reason, CREDIT_COMPARISON_UNRELIABLE);
        assert_eq!(result.decrease, 0.0);
        assert_eq!(result.increase, 0.0);
        // 区间仍要如实给出 —— 用户需要知道「哪一段没测准」
        assert_eq!(result.from_ts, Some(now - 30 * 60 * 1000));
        assert_eq!(result.to_ts, Some(now));
    }

    /// 容量下降（包集合变了）不得算成消耗 —— owner 真实幻影数据的回归。
    ///
    /// 实测：`350 → 0 → 350`（total 同步 350→0→350）与 `380 → 30 → 380`。
    /// `total` 不会因消耗而变小，故 total 下降即是「没读全」。
    #[test]
    fn dropped_capacity_is_not_counted_as_consumption() {
        let now = at_local_date(0, 12);
        let result = compare_snapshots(
            &snap_of(now - 30 * 60 * 1000, 350.0, 350.0),
            &snap_of(now, 0.0, 0.0),
        );

        assert!(!result.ok, "容量从 350 掉到 0 是读数不完整，不是消耗 350");
        assert_eq!(result.reason, CREDIT_COMPARISON_UNRELIABLE);
        assert_eq!(result.decrease, 0.0);
    }

    /// 真实消耗（容量不变、余额跌）必须照常报出 —— 不能因修幻影而把真消耗吞掉。
    #[test]
    fn real_consumption_with_stable_capacity_is_still_reported() {
        let now = at_local_date(0, 12);
        let result = compare_snapshots(
            &snap_of(now - 30 * 60 * 1000, 380.0, 380.0),
            &snap_of(now, 380.0, 30.0),
        );

        assert!(result.ok);
        assert_eq!(result.decrease, 350.0);
        assert_eq!(result.increase, 0.0);
        assert_eq!(result.net, -350.0);
    }

    /// 端到端：`record_snapshot` 落盘后，`build_statistics` 给出的差值
    /// 必须与真实写入的快照一致（证明读路径与写路径口径对齐）。
    #[test]
    fn recorded_snapshots_produce_a_matching_comparison() {
        let _iso = crate::modules::config::test_isolation::Isolated::new("credit-comparison-e2e");

        // 首次采集：只建立基线，差值应为「无基准」
        assert!(record_snapshot("acc-cmp", "n", 1000.0, 1000.0, packs(&[("p1", 1000.0)])));
        let after_first = build_statistics(&load_snapshots(), &[], &[], now_ms());
        assert_eq!(
            after_first["comparison"]["ok"], false,
            "只有一条快照时不该给出差值"
        );

        // 第二次采集：余额跌 120、容量不变 → 消耗 120
        assert!(record_snapshot("acc-cmp", "n", 1000.0, 880.0, packs(&[("p1", 880.0)])));
        let after_second = build_statistics(&load_snapshots(), &[], &[], now_ms());
        assert_eq!(after_second["comparison"]["ok"], true);
        assert_eq!(after_second["comparison"]["decrease"], 120.0);
        assert_eq!(after_second["comparison"]["accounts"], 1);

        let account = after_second["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|account| account["accountId"] == "acc-cmp")
            .expect("应有该账号的统计");
        assert_eq!(account["change"]["decrease"], 120.0);
        assert_eq!(account["change"]["ok"], true);
    }

    /// 汇总口径与明细口径**并存**：`comparison` 存在的同时，
    /// `summary.usage*` 与 `events` 必须照旧可用（所有者的明确要求）。
    #[test]
    fn summary_and_detail_sources_are_preserved_alongside_the_comparison() {
        let now = at_local_date(0, 12);
        let stats = build_statistics(
            &[
                full_snap(now - 60 * 60 * 1000, 500.0, 500.0),
                full_snap(now - 30 * 60 * 1000, 500.0, 400.0),
            ],
            &[],
            &[],
            now,
        );

        // 汇总口径给出末次区间
        assert_eq!(stats["comparison"]["ok"], true);
        // 明细口径（逐事件累加）同时保留，且给得出同一个消耗
        assert_eq!(stats["summary"]["usageToday"], 100.0);
        assert_eq!(stats["events"][0]["kind"], "usage");
        assert_eq!(stats["events"][0]["amount"], 100.0);
        // 逐日序列也照旧
        assert!(stats["daily"].as_array().is_some_and(|daily| !daily.is_empty()));
    }

    /// 多个账号时，全局差值 = 各**可信**账号之和，且区间取并集。
    ///
    /// 同时钉住：不可信的账号（这里让它容量下降）被跳过，
    /// 且不会被当成 0 混进总数。
    #[test]
    fn aggregate_sums_only_trustworthy_accounts() {
        let now = at_local_date(0, 12);
        let prev = now - 30 * 60 * 1000;
        let snapshots = vec![
            // 账号 A：真实消耗 60
            json!({"ts": prev, "accountId": "a", "accountName": "A", "total": 100.0, "remaining": 100.0}),
            json!({"ts": now,  "accountId": "a", "accountName": "A", "total": 100.0, "remaining": 40.0}),
            // 账号 B：真实发放 25
            json!({"ts": prev, "accountId": "b", "accountName": "B", "total": 50.0, "remaining": 50.0}),
            json!({"ts": now,  "accountId": "b", "accountName": "B", "total": 75.0, "remaining": 75.0}),
            // 账号 C：容量下降（读数不完整）→ 必须被跳过
            json!({"ts": prev, "accountId": "c", "accountName": "C", "total": 350.0, "remaining": 350.0}),
            json!({"ts": now,  "accountId": "c", "accountName": "C", "total": 0.0, "remaining": 0.0}),
        ];

        let stats = build_statistics(&snapshots, &[], &[], now);

        assert_eq!(stats["comparison"]["accounts"], 2, "只应统计 A 与 B");
        assert_eq!(stats["comparison"]["decrease"], 60.0);
        assert_eq!(stats["comparison"]["increase"], 25.0);
        assert_eq!(stats["comparison"]["net"], -35.0);
        // 区间取可信账号的并集
        assert_eq!(stats["comparison"]["fromTs"], prev);
        assert_eq!(stats["comparison"]["toTs"], now);
    }

    /// 账号不可信时，全局 reason 要如实说明「不可信」而不是「无基准」。
    ///
    /// 两者处置不同：无基准要等下一次采集，不可信只要上游恢复正常即可。
    #[test]
    fn global_reason_distinguishes_unreliable_from_no_baseline() {
        let now = at_local_date(0, 12);
        let prev = now - 30 * 60 * 1000;
        let stats = build_statistics(
            &[
                json!({"ts": prev, "accountId": "a", "accountName": "A", "total": 350.0, "remaining": 350.0}),
                json!({"ts": now,  "accountId": "a", "accountName": "A", "total": 0.0, "remaining": 0.0}),
            ],
            &[],
            &[],
            now,
        );

        assert_eq!(stats["comparison"]["ok"], false);
        assert_eq!(stats["comparison"]["reason"], CREDIT_COMPARISON_UNRELIABLE);
    }

    /// **回归：幻影区间不得进入逐日序列 / 时间窗 / 事件流。**
    ///
    /// 改动前这里是唯一漏判据的地方：`record_snapshot` 已用 `reading_trust`
    /// 拦住了幻影**记录**，但 `build_statistics` 是**从快照文件重新回放**的，
    /// 不读那个结论，于是 owner 真实的 `350 → 0 → 350` 仍会累加出
    /// 「今日消耗 350」—— 而 GatewayPage「消耗积分」列、统计页
    /// 「今日 / 近 7 天 / 本月消耗」读的正是这几个字段。
    /// 只修记录不修这里，用户看到的数字依旧是错的。
    #[test]
    fn phantom_pair_does_not_leak_into_daily_or_window_totals() {
        let now = at_local_date(0, 12);
        let stats = build_statistics(
            &[
                // 基线：完整读到 350
                full_snap(now - 60 * 60 * 1000, 350.0, 350.0),
                // 上游只返回了 0（total 同步掉到 0）→ 读数不完整，不是消耗
                full_snap(now - 30 * 60 * 1000, 0.0, 0.0),
                // 读全了，回到 350 → 是恢复，不是发放
                full_snap(now, 350.0, 350.0),
            ],
            &[],
            &[],
            now,
        );

        assert_eq!(stats["summary"]["usageToday"], 0.0, "幻影不得计入今日消耗");
        assert_eq!(stats["summary"]["usage7Days"], 0.0);
        assert_eq!(stats["summary"]["usageThisMonth"], 0.0);
        assert_eq!(
            stats["events"].as_array().unwrap().len(),
            0,
            "幻影不该产生任何事件，实际 {:?}",
            stats["events"]
        );
        let daily_sum: f64 = stats["daily"]
            .as_array()
            .unwrap()
            .iter()
            .map(|point| point["usage"].as_f64().unwrap_or(0.0))
            .sum();
        assert_eq!(daily_sum, 0.0, "逐日序列里也不该有幻影");
    }

    /// 对照：真实的「消耗后再发放」序列**仍要**照常累加，
    /// 不能因为修幻影把真实变化一起吞掉。
    ///
    /// 序列 `380 → 30（真消耗，包还在）→ 410（真发放，来了新包）`：
    /// 消耗那一段必须留在今日消耗里；发放那一段不是消耗，故不计入。
    #[test]
    fn real_consumption_still_accumulates_after_the_phantom_guard() {
        let now = at_local_date(0, 12);
        let stats = build_statistics(
            &[
                full_snap(now - 90 * 60 * 1000, 380.0, 380.0),
                // 容量不变、余额跌 → 真实消耗 350
                full_snap(now - 60 * 60 * 1000, 380.0, 30.0),
                // 容量上升到 760、余额 410 → 新包到账（发放），不是消耗
                full_snap(now - 30 * 60 * 1000, 760.0, 410.0),
            ],
            &[],
            &[],
            now,
        );

        assert_eq!(stats["summary"]["usageToday"], 350.0, "真实消耗必须被累加");
        let amounts: Vec<f64> = stats["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["kind"] == "usage")
            .map(|event| event["amount"].as_f64().unwrap())
            .collect();
        assert_eq!(amounts, vec![350.0]);
    }

    /// `stale` 与 `ok` **正交**：算得准的差值也可能已经不再反映「此刻」。
    ///
    /// 构造：两条快照间隔 30 分钟（差值本身完全可信），但统计时刻在
    /// 末次快照 5 小时之后（很久没再采集）。此时 `ok` 仍为 true
    ///（那个差值是真发生过的），同时 `stale` 为 true（它不是当前的）。
    /// 界面据 `stale` 改用「数据截至 …」措辞，而不是把数字丢掉或谎称最新。
    #[test]
    fn a_trustworthy_but_old_comparison_is_marked_stale_not_discarded() {
        let now = at_local_date(0, 12);
        let last_scan = now - 5 * 3600 * 1000;
        let stats = build_statistics(
            &[
                full_snap(last_scan - 30 * 60 * 1000, 800.0, 800.0),
                full_snap(last_scan, 800.0, 750.0),
            ],
            &[],
            &[],
            now,
        );

        assert_eq!(stats["comparison"]["ok"], true, "差值本身是可信的");
        assert_eq!(stats["comparison"]["decrease"], 50.0);
        assert_eq!(stats["comparison"]["stale"], true, "但它已不是当前读数");
        // 间隔正常时不应被误标为陈旧
        let fresh = build_statistics(
            &[
                full_snap(now - 30 * 60 * 1000, 800.0, 800.0),
                full_snap(now, 800.0, 750.0),
            ],
            &[],
            &[],
            now,
        );
        assert_eq!(fresh["comparison"]["stale"], false);
    }

    /// 时钟回拨（`at_ms` 早于末次快照）不得把新鲜数据误判为陈旧。
    ///
    /// 分两层验证，因为这两件事是**分开**成立的：
    ///
    /// 1. 走 `build_statistics` 时，`parse_snapshots` 的 `ts <= at_ms` 过滤
    ///    已经把「未来」快照挡在外面，所以根本到不了陈旧判定 ——
    ///    这里钉住这个上游保证（同时说明为什么会写 `saturating_sub` 仍不多余）。
    /// 2. 直接在函数边界上验证：一旦有未来快照真的传到
    ///    `compare_account_snapshots`（例如将来有人放宽了那条过滤），
    ///    `saturating_sub` 必须把它当「刚刚」而不是「很久以前」。
    ///    若这里写成普通相减，`at_ms - latest.ts` 是负数，虽不触发阈值，
    ///    但换写成 `latest.ts - at_ms` 就会溢出 —— 这一步锁的就是那个语义。
    #[test]
    fn clock_skew_does_not_mark_a_fresh_comparison_stale() {
        let now = at_local_date(0, 12);
        // 1) 未来快照被 parse_snapshots 过滤 → 只剩一条 → 无基准（而非陈旧）
        let filtered = build_statistics(
            &[
                full_snap(now - 30 * 60 * 1000, 800.0, 800.0),
                full_snap(now + 10 * 60 * 1000, 800.0, 750.0),
            ],
            &[],
            &[],
            now,
        );
        assert_eq!(
            filtered["comparison"]["reason"],
            CREDIT_COMPARISON_NO_BASELINE,
            "未来快照应被上游过滤，不该参与对比"
        );

        // 2) 直接喂给函数：末次快照在未来 → 不得判为陈旧
        let future = at_local_date(0, 13);
        let comparison = compare_account_snapshots(
            &[
                snap_of(now, 800.0, 800.0),
                snap_of(future, 800.0, 750.0),
            ],
            now,
        );
        assert!(comparison.ok);
        assert!(
            !comparison.stale,
            "末次快照晚于统计时刻时应视为最新，而不是过旧"
        );
        assert_eq!(comparison.decrease, 50.0);
    }
}
