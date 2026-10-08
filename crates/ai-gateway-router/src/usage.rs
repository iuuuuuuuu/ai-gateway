//! 网关侧 Token 用量统计。
//!
//! 对应 Go 源文件 `internal/usage/usage.go`。
//!
//! # 数据来源
//!
//! 每次**成功**请求结束后，从上游响应（非流式 `usage` 对象 / 流式 SSE 末帧
//! `usage`）提取 prompt / completion / cache 计量，按「服务器本地日期」聚合。
//!
//! # 三个维度
//!
//! 全部按天存储，导出时可按天数范围过滤：
//! - `days`：全体请求（`summary` / `daily` 由它派生）
//! - `models`：模型 × 日期
//! - `accounts`：账号 × 日期
//!
//! # 持久化
//!
//! 与账号池 `state.json` 同目录的 `usage.json`。`record` 只置脏标志，由调用方
//! 周期落盘（进程退出前补一次）。落盘与导出**只含聚合数字、uid 与模型名**，
//! 不含消息正文或认证信息。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Mutex;

use chrono::{Datelike, TimeZone, Timelike};

/// 聚合用的日期键格式（本地时区），固定宽度保证字符串可直接比较。
const DAY_LAYOUT: &str = "%Y-%m-%d";

/// 一组请求的 Token 计量。
///
/// `input` 已包含 `cache_read`（与上游 `prompt_tokens` 含 cached tokens 的口径
/// 一致），`cache_write` 为单独新增的缓存写入量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Counters {
    /// 输入 token（含缓存命中部分）。
    pub input: i64,
    /// 输出 token。
    pub output: i64,
    /// 缓存读取量。
    #[serde(rename = "cacheRead")]
    pub cache_read: i64,
    /// 缓存写入量。
    #[serde(rename = "cacheWrite")]
    pub cache_write: i64,
    /// 请求条数。
    pub records: i64,
}

impl Counters {
    /// 累加另一组计量。
    fn add(self, o: Counters) -> Counters {
        Counters {
            input: self.input + o.input,
            output: self.output + o.output,
            cache_read: self.cache_read + o.cache_read,
            cache_write: self.cache_write + o.cache_write,
            records: self.records + o.records,
        }
    }

    /// 是否没有任何计量（用于跳过记录与聚合）。
    fn is_empty(&self) -> bool {
        self.input == 0
            && self.output == 0
            && self.cache_read == 0
            && self.cache_write == 0
            && self.records == 0
    }

    /// 导出成与宿主 Token 统计页一致的字段口径。
    ///
    /// `total = input + output + cache_write`（**不含** `cache_read`，
    /// 避免与 `input` 重复计数 —— `input` 已经含了它）。
    fn value(&self) -> serde_json::Value {
        let uncached = (self.input - self.cache_read).max(0);
        let hit_rate = if self.input > 0 {
            serde_json::json!(self.cache_read as f64 / self.input as f64)
        } else {
            serde_json::Value::Null
        };
        serde_json::json!({
            "total": self.input + self.output + self.cache_write,
            "input": self.input,
            "output": self.output,
            "cacheRead": self.cache_read,
            "cacheWrite": self.cache_write,
            "uncachedInput": uncached,
            "records": self.records,
            "cacheHitRate": hit_rate,
        })
    }
}

/// 一次请求的**计费归属**。
///
/// 对应 Go `internal/usage/usage.go` 的 `Billing`。所有者 2026-09-21 原话：
/// 「兼容网关的 token 用量也要显示出这个模型的倍率（如果有多个 则需要拆开显示）」。
///
/// 按 **(平台, 区域, 倍率) 三元组**拆开记账 —— 三元组而不是二元组，因为同一平台
/// 同一区域的倍率也可能随上游调整而变化（实测国服倍率会变）。
///
/// # 三态语义（必须原样保留，界面靠它区分三种完全不同的情况）
///
/// | `has_multiplier` | `multiplier` | 含义 | 界面 |
/// |---|---|---|---|
/// | `false` | 任意 | 该平台**没有**倍率这个概念 | 不显示倍率列 |
/// | `true` | `Some(x)` | 倍率 = x（`0.0` 表示免费） | 显示 x |
/// | `true` | `None` | **未声明**（不知道） | 显示「—」 |
///
/// ★ `None` **不是**免费。把「不知道」渲染成 `0` 会让用户以为这个模型不花钱，
/// 从而放心烧一个其实在计费的账号。
///
/// ★ 宁可不说，也不给一个可能错的数字：取不到倍率时保持 `None`，**不编造**。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Billing {
    /// 平台：`workbuddy` / `qoder` / `zcode`。
    pub product: String,
    /// 区域：`cn` / `intl`；空 = 该平台不分区。
    pub region: String,
    /// 计费倍率；`None` = 未声明（见上表）。
    pub multiplier: Option<f64>,
    /// 是否声明了「有倍率」这一概念（决定界面是否显示倍率列）。
    pub has_multiplier: bool,
}

/// `usage.json` 的磁盘结构。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct FileState {
    #[serde(default)]
    version: i32,
    #[serde(default, rename = "savedAt")]
    saved_at: String,
    #[serde(default)]
    days: HashMap<String, Counters>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    models: HashMap<String, HashMap<String, Counters>>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    accounts: HashMap<String, HashMap<String, Counters>>,
    /// 账号 → 模型 → 日期。用于「计费归属回填」（见 [`backfill_billing`]）。
    #[serde(default, rename = "accountModels", skip_serializing_if = "HashMap::is_empty")]
    account_models: HashMap<String, HashMap<String, HashMap<String, Counters>>>,
    /// 模型 → 计费键 → 日期。
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    billed: HashMap<String, HashMap<String, HashMap<String, Counters>>>,
    /// `模型\x00计费键` → 归属。键里带模型名是因为不同模型可以有同一组归属。
    #[serde(default, rename = "billingMeta", skip_serializing_if = "HashMap::is_empty")]
    billing_meta: HashMap<String, BillingOnDisk>,
    /// 账号 -> 计费周期状态（Go `fileState.BillingCycles`，2026-09-28 新增，N4）。
    ///
    /// 为什么不另建一本账：周期是「每账号一条」的**状态**（与 `billing_meta`
    /// 同构：不可加、需要落盘、需要跨重启稳定），而不是可加的计量。本文件已有
    /// 完整的 load / flush / dirty 管线，另起一套只会让版本号、原子写、容错
    /// 逻辑两边分叉。详见 [`CycleState`]。
    #[serde(default, rename = "billingCycles", skip_serializing_if = "HashMap::is_empty")]
    billing_cycles: HashMap<String, CycleState>,
}

/// `Billing` 的落盘形态（`Option<f64>` 需要区分「未声明」与「0」，用显式标记）。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct BillingOnDisk {
    product: String,
    region: String,
    has_multiplier: bool,
    multiplier: Option<f64>,
}

impl From<&Billing> for BillingOnDisk {
    fn from(b: &Billing) -> Self {
        Self {
            product: b.product.clone(),
            region: b.region.clone(),
            has_multiplier: b.has_multiplier,
            multiplier: b.multiplier,
        }
    }
}

impl From<&BillingOnDisk> for Billing {
    fn from(b: &BillingOnDisk) -> Self {
        Self {
            product: b.product.clone(),
            region: b.region.clone(),
            has_multiplier: b.has_multiplier,
            multiplier: b.multiplier,
        }
    }
}

/// 内存态。
#[derive(Debug, Default)]
struct Inner {
    days: HashMap<String, Counters>,
    models: HashMap<String, HashMap<String, Counters>>,
    accounts: HashMap<String, HashMap<String, Counters>>,
    account_models: HashMap<String, HashMap<String, HashMap<String, Counters>>>,
    billed: HashMap<String, HashMap<String, HashMap<String, Counters>>>,
    billing_meta: HashMap<String, Billing>,
    /// uid -> 计费周期状态（Go `Stats.billingCycles`）。
    ///
    /// 与 `billing_meta` 同样**只增不减**：账号被移除后它的周期状态留在文件里，
    /// 无害（不出现在快照里，除非调用方按 uid 查）。清理由账号池负责，统计器
    /// 不做猜测式删除。
    billing_cycles: HashMap<String, CycleState>,
}

/// 网关 Token 用量聚合器。并发安全；`path` 为空时纯内存（不落盘）。
#[derive(Debug)]
pub struct Stats {
    inner: Mutex<Inner>,
    dirty: AtomicBool,
    path: String,
    /// 测试用时钟覆盖（Unix 毫秒）；`0` = 用真实时钟。
    ///
    /// 对应 Go `Stats.now func() time.Time`（nil => `time.Now`）。用 `AtomicI64`
    /// 而不是 `Option<Arc<dyn Fn() -> ...>>`：`Stats` 派生 `Debug`，而闭包不是
    /// `Debug` —— 换成一个整数就能保留派生，且天然线程安全。
    now_override_ms: AtomicI64,
}

impl Stats {
    /// 构建统计器；`path` 非空时加载旧数据。
    pub fn new(path: impl Into<String>) -> Self {
        let path = path.into();
        let inner = if path.is_empty() {
            Inner::default()
        } else {
            load(&path)
        };
        Self {
            inner: Mutex::new(inner),
            dirty: AtomicBool::new(false),
            path,
            now_override_ms: AtomicI64::new(0),
        }
    }

    /// 快照用的「现在」（注入时钟优先）。
    ///
    /// 对应 Go `Snapshot` 里的 `now := s.clock()`。注入的时钟在 `chrono` 的
    /// `DateTime<Local>` 层面可能无法精确表示（极端的注入值），此时退回真实时钟 ——
    /// 生产路径（`now_override_ms == 0`）恒等于 `chrono::Local::now()`。
    fn snapshot_now(&self) -> chrono::DateTime<chrono::Local> {
        let injected = self.now_override_ms.load(Ordering::Relaxed);
        if injected != 0 {
            if let Some(dt) = chrono::Local.timestamp_millis_opt(injected).single() {
                return dt;
            }
        }
        chrono::Local::now()
    }

    /// 记录一次成功请求的用量；`uid` / `model` 为空时归一成 `-`。
    ///
    /// 不带计费归属 —— 归属未知的请求**不会**出现在「按模型计费」表里（见
    /// [`Stats::record_billed`]）。
    pub fn record(&self, uid: &str, model: &str, c: Counters) {
        self.record_billed_at_ms(uid, model, &Billing::default(), self.clock_ms(), c);
    }

    /// 同 [`Stats::record`]，但显式指定日期键（测试用）。
    pub fn record_at(&self, uid: &str, model: &str, day: &str, c: Counters) {
        self.record_billed_at(uid, model, &Billing::default(), day, c);
    }

    /// 记录一次成功请求的用量，并带上**计费归属**。
    ///
    /// 对应 Go `Stats.RecordBilled`。
    pub fn record_billed(&self, uid: &str, model: &str, b: Billing, c: Counters) {
        self.record_billed_at_ms(uid, model, &b, self.clock_ms(), c);
    }

    /// [`Stats::record_billed`] 的显式日期版本。
    ///
    /// ⚠ 只有日期键、没有时刻 —— 而周期推进需要**时刻**（Go 的 `RecordAt` 收的是
    /// `time.Time`，同时用于日期键与推进判据）。这里按该日本地零点解释，语义确定
    /// 但**同日内的边界跨不过去**；需要与 Go 逐字一致的推进行为时用
    /// [`Stats::record_billed_at_ms`]。生产路径走 [`Stats::record`] /
    /// [`Stats::record_billed`]，它们用真实时钟，不受此限。
    pub fn record_billed_at(&self, uid: &str, model: &str, b: &Billing, day: &str, c: Counters) {
        let at_ms = day_start_ms(day);
        self.record_billed_core(uid, model, b, day, at_ms, c);
    }

    /// 对应 Go `recordAt(uid, model, b, at, c)`：显式时刻版本，日期键由时刻推导。
    ///
    /// 这是与 Go **逐字对齐**的那一个：日期键与周期推进用同一个时刻，故「边界
    /// 当天记录一笔」能正确地翻过周期。
    pub fn record_billed_at_ms(
        &self,
        uid: &str,
        model: &str,
        b: &Billing,
        at_ms: i64,
        c: Counters,
    ) {
        let day = day_of_ms(at_ms);
        let day = if day.is_empty() { local_day_now() } else { day };
        self.record_billed_core(uid, model, b, &day, at_ms, c);
    }

    /// `record*` 家族的公共实现（Go `recordAt` 的主体）。
    fn record_billed_core(
        &self,
        uid: &str,
        model: &str,
        b: &Billing,
        day: &str,
        at_ms: i64,
        c: Counters,
    ) {
        // 上游未返回可用 usage 时**不计数，也不产生空记录**。
        if c.input == 0 && c.output == 0 && c.cache_read == 0 && c.cache_write == 0 {
            return;
        }
        let entry = Counters {
            input: c.input,
            output: c.output,
            cache_read: c.cache_read,
            cache_write: c.cache_write,
            records: 1,
        };
        // ★★ 模型名必须过 `bare_model_name` 归一。
        //
        // 所有者逐字：「不要再把 平台:模型名 这种类似的 单独列一个统计了,这是错误的」。
        // 现场：`国服:deepseek-v4.1-flash` 显示 12 tokens / 1 次，而
        // `deepseek-v4.1-flash · 国服` 显示 1.6B / 4014 次 —— 其实是同一份用量，
        // 被前缀拆成了两行。区域/平台信息由 `Billing` 承载，模型名只留裸名。
        let model_owned = bare_model_name(model);
        let model = if model_owned.is_empty() { "-" } else { &model_owned };
        let uid = if uid.is_empty() { "-" } else { uid };

        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        // ★★ 模型名再按**大小写不敏感**归一（2026-10-05 所有者现场）。
        //
        // 所有者逐字：「一样的模型居然还分了两条记录」—— 他的截图里
        // `GLM-5.3-Flash · ZCode`（11.0M / 98 次）与 `glm-5.3-flash · ZCode`
        // （1.6K）并排两行。
        //
        // 根因：上游目录里的规范名是 `GLM-5.3-Flash`（`gateway_native_config.json`
        // 的 zcode 模型表就是这个写法），而客户端（DSH 的 providers.json）发的是
        // 小写 `zcode:glm-5.3-flash`。两者经 `bare_model_name` 去掉前缀后**逐字**
        // 落键，于是同一个模型在 `models` / `account_models` / `billed` /
        // `billing_meta` 四处各存了一份。
        //
        // 本仓其它**所有**模型比较都是大小写不敏感的（`forward.rs:434`
        // `eq_ignore_ascii_case`、`model_allow.rs:74` 同款、`pool.rs` 一律
        // `to_ascii_lowercase`、`capability.rs:833` 同款），**只有用量统计这一处**
        // 逐字比较 —— 那是遗漏而不是设计：路由认它们是一个模型，账单却当成两个。
        //
        // 归一策略：**沿用已存在的那个拼写**（忽略大小写地找），找不到才用本次传来
        // 的写法。这样 (a) 不再把新记录劈成两行；(b) 已落盘的历史由 `load()` 里的
        // [`merge_case_variants`] 合并后，新记录并入幸存的拼写，界面上的名字不会在
        // 两个写法之间来回跳。
        let model_key = model_key_of(&inner, model);
        let model = model_key.as_str();
        add_to(&mut inner.days, day, entry);
        add_to(nested(&mut inner.models, model), day, entry);
        add_to(nested(&mut inner.accounts, uid), day, entry);
        // 账号 × 模型 × 日期：计费归属回填的唯一依据。
        add_to(nested2(&mut inner.account_models, uid, model), day, entry);
        // ★ 只有**声明了归属**的请求才进计费表。Billing 零值 = 老调用点没给归属，
        // 混进 `""` 平台会让界面出现一行没有来源的用量，比不显示更困惑。
        if !b.product.is_empty() || b.has_multiplier {
            let key = billing_key(b);
            add_to(nested2(&mut inner.billed, model, &key), day, entry);
            inner
                .billing_meta
                .insert(format!("{model}\x00{key}"), b.clone());
        }
        // 写路径推进计费周期（2026-09-28，N4）。
        //
        // ⚠ 只有**写**路径推进（这里与 `set_billing_cycle`）；读路径只投影。
        // 若读路径也推进，一次恰好落在重置点之后的 /usage 轮询就会把推进落盘，
        // 从而在没有任何请求进来时凭空翻过一个周期。
        //
        // ⚠ 用记录本身的时刻 `at_ms` 而不是 `self.clock_ms()`：本方法已经接收了
        // 显式时刻（Go 原注释同款理由），拿另一个时钟会让注入的时刻与推进判据
        // 不一致。`at_ms <= 0` 时 `roll_cycle_locked` 里的 `now_ms >= next_reset_ms`
        // 天然为假（边界总是 > 0）⇒ 不推进，安全。
        roll_cycle_locked(&mut inner, uid, at_ms);
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// 导出一份可 JSON 序列化的聚合快照。`days <= 0` 表示全部历史。
    pub fn snapshot(&self, days: i64) -> serde_json::Value {
        let Ok(inner) = self.inner.lock() else {
            return serde_json::json!({});
        };
        // ★ 必须走 `clock_ms()` 而不是直接 `Local::now()`：Go 的 `Snapshot` 全程用
        // `now := s.clock()`，cutoff / generatedAt / billingCycles 三处**同一个**
        // 「现在」。若这里用真实时钟而周期投影用注入时钟，注入的测试时钟就只对
        // 一半生效 —— 快照里的周期边界会与落盘状态错位（本移植的第一版就是这样，
        // 被 `cycle_projection_does_not_roll_state` 抓到）。
        let now = self.snapshot_now();
        let cutoff = if days > 0 {
            (now - chrono::Duration::days(days - 1))
                .format(DAY_LAYOUT)
                .to_string()
        } else {
            String::new()
        };

        let summary = sum_days(&inner.days, &cutoff);
        let models = group_list(&inner.models, &cutoff);
        let accounts = group_list(&inner.accounts, &cutoff);
        let daily = day_series(&inner.days, &cutoff);

        let mut daily_by_model = serde_json::Map::new();
        for (model, series) in &inner.models {
            daily_by_model.insert(model.clone(), serde_json::Value::Array(day_series(series, &cutoff)));
        }

        // 账号 → 模型：只保留该范围内**仍有计量**的账号与模型，界面据此区分
        // 「这个号这段时间没消耗」与「这个号根本不在统计里」。
        let mut account_models = serde_json::Map::new();
        for (uid, by_model) in &inner.account_models {
            let mut groups = serde_json::Map::new();
            let mut any = false;
            for (model, series) in by_model {
                let c = sum_days(series, &cutoff);
                if c.is_empty() {
                    continue;
                }
                groups.insert(model.clone(), c.value());
                any = true;
            }
            if any {
                account_models.insert(uid.clone(), serde_json::Value::Object(groups));
            }
        }

        // 模型 → 计费明细。倍率用 `creditMultiplier` 键并**允许 null**，与
        // `/v1/models` 的 channels 同名字段同一套三态语义（前端可用同一个渲染函数）。
        let mut model_billing = serde_json::Map::new();
        for (model, by_billing) in &inner.billed {
            let groups = group_list(by_billing, &cutoff);
            if groups.is_empty() {
                continue;
            }
            // ★ 投影层按 (平台, 区域) 收成一行（业主 2026-10-03：同名同区域裂成三份）。
            // 写路径的键与落盘形态都不动，见 `merge_billing_groups`。
            let groups = merge_billing_groups(groups, by_billing, &inner.billing_meta, model, &cutoff);
            let decorated: Vec<serde_json::Value> = groups
                .into_iter()
                .map(|mut g| {
                    let key = g
                        .get("key")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    if let Some(b) = inner.billing_meta.get(&format!("{model}\x00{key}")) {
                        if let Some(obj) = g.as_object_mut() {
                            obj.insert("product".into(), serde_json::json!(b.product));
                            if !b.region.is_empty() {
                                obj.insert("region".into(), serde_json::json!(b.region));
                            }
                            obj.insert("hasMultiplier".into(), serde_json::json!(b.has_multiplier));
                            if b.has_multiplier {
                                // ★★ 必须是 `Option<f64>` 展开后的值，**不能**放指针 ——
                                // Go 侧踩过：直接把 `*float64` 放进 map 会序列化成
                                // 指针地址（实测界面显示 `0x15bb13fb22f8`），用户会把它
                                // 当成一个巨大的倍率。
                                match b.multiplier {
                                    Some(m) => obj.insert("creditMultiplier".into(), serde_json::json!(m)),
                                    None => obj.insert("creditMultiplier".into(), serde_json::Value::Null),
                                };
                            }
                        }
                    }
                    g
                })
                .collect();
            model_billing.insert(model.clone(), serde_json::Value::Array(decorated));
        }

        // 计费归属回填：只补 `product`，**不编造** region 与倍率。
        backfill_billing(&inner.account_models, &cutoff, &mut model_billing);

        serde_json::json!({
            "generatedAt": now.timestamp_millis(),
            "rangeDays": if days > 0 { serde_json::json!(days) } else { serde_json::Value::Null },
            "summary": summary.value(),
            "models": models,
            "accounts": accounts,
            "accountModels": serde_json::Value::Object(account_models),
            "modelBilling": serde_json::Value::Object(model_billing),
            "daily": daily,
            "dailyByModel": serde_json::Value::Object(daily_by_model),
            // 计费周期投影（2026-09-28，N4）。
            //
            // 这里调的是**投影**（`project_cycle_locked`）而不是写路径的推进：
            // 面板每 15 秒拉一次 /usage，若读路径也推进边界，一个恰好落在重置点
            // 之后的轮询就会把推进结果落盘 —— 用户看到周期凭空翻页，而根本原因
            // 是「有人在看界面」。
            "billingCycles": self.cycle_report_locked(&inner, self.clock_ms()),
        })
    }

    /// 同步把内存状态落盘（幂等：无变更或纯内存模式直接返回）。
    pub fn flush(&self) {
        if self.path.is_empty() || !self.dirty.load(Ordering::Relaxed) {
            return;
        }
        let state = {
            let Ok(inner) = self.inner.lock() else {
                return;
            };
            FileState {
                version: 1,
                saved_at: chrono::Local::now().to_rfc3339(),
                days: inner.days.clone(),
                models: inner.models.clone(),
                accounts: inner.accounts.clone(),
                account_models: inner.account_models.clone(),
                billed: inner.billed.clone(),
                billing_meta: inner
                    .billing_meta
                    .iter()
                    .map(|(k, v)| (k.clone(), BillingOnDisk::from(v)))
                    .collect(),
                billing_cycles: inner.billing_cycles.clone(),
            }
        };
        self.dirty.store(false, Ordering::Relaxed);

        let Ok(raw) = serde_json::to_string_pretty(&state) else {
            crate::logline!("usage: 序列化失败");
            self.dirty.store(true, Ordering::Relaxed);
            return;
        };
        if let Some(dir) = std::path::Path::new(&self.path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // 原子写：先写 tmp 再 rename，避免进程中断留下半截文件。
        let tmp = format!("{}.tmp", self.path);
        if let Err(e) = std::fs::write(&tmp, raw) {
            crate::logline!("usage: 落盘失败: {e}");
            self.dirty.store(true, Ordering::Relaxed);
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            crate::logline!("usage: 落盘失败: {e}");
            let _ = std::fs::remove_file(&tmp);
            self.dirty.store(true, Ordering::Relaxed);
        }
    }
}

/// 取当前本地日期键。
fn local_day_now() -> String {
    chrono::Local::now().format(DAY_LAYOUT).to_string()
}

/// 取（必要时创建）内层映射。
fn nested<'a>(
    m: &'a mut HashMap<String, HashMap<String, Counters>>,
    key: &str,
) -> &'a mut HashMap<String, Counters> {
    m.entry(key.to_string()).or_default()
}

/// 累加到指定日期键。
fn add_to(m: &mut HashMap<String, Counters>, key: &str, c: Counters) {
    let slot = m.entry(key.to_string()).or_default();
    *slot = slot.add(c);
}

/// 按 cutoff 汇总全部日期。
fn sum_days(m: &HashMap<String, Counters>, cutoff: &str) -> Counters {
    let mut total = Counters::default();
    for (day, c) in m {
        // 日期键固定宽度，字符串比较等价于时间比较。
        if !cutoff.is_empty() && day.as_str() < cutoff {
            continue;
        }
        total = total.add(*c);
    }
    total
}

/// 把「键 → 日期 → 计量」两层结构压平成按 `total` 降序的列表。
fn group_list(
    m: &HashMap<String, HashMap<String, Counters>>,
    cutoff: &str,
) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(m.len());
    for (key, series) in m {
        let c = sum_days(series, cutoff);
        if c.is_empty() {
            continue;
        }
        let mut value = c.value();
        if let Some(obj) = value.as_object_mut() {
            obj.insert("key".into(), serde_json::json!(key));
        }
        out.push(value);
    }
    // 稳定排序：total 降序（相等时保持插入序，与 Go 的 SliceStable 一致）。
    out.sort_by(|a, b| {
        let ta = a.get("total").and_then(|v| v.as_i64()).unwrap_or(0);
        let tb = b.get("total").and_then(|v| v.as_i64()).unwrap_or(0);
        tb.cmp(&ta)
    });
    out
}

/// 把日聚合导出成按日期升序的序列（供前端趋势图使用）。
fn day_series(m: &HashMap<String, Counters>, cutoff: &str) -> Vec<serde_json::Value> {
    let mut keys: Vec<&String> = m
        .keys()
        .filter(|day| cutoff.is_empty() || day.as_str() >= cutoff)
        .collect();
    keys.sort();
    keys.into_iter()
        .map(|day| {
            let mut value = m[day].value();
            if let Some(obj) = value.as_object_mut() {
                obj.insert("key".into(), serde_json::json!(day));
            }
            value
        })
        .collect()
}

/// 从磁盘加载。
fn load(path: &str) -> Inner {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Inner::default();
    };
    let Ok(fs) = serde_json::from_str::<FileState>(&raw) else {
        crate::logline!("usage: 解析 {path} 失败，忽略");
        return Inner::default();
    };
    if fs.version > 1 {
        crate::logline!("usage: {path} 版本 {} 高于当前支持，忽略", fs.version);
        return Inner::default();
    }
    let mut inner = Inner {
        days: fs.days,
        models: fs.models,
        accounts: fs.accounts,
        account_models: fs.account_models,
        billed: fs.billed,
        billing_meta: fs
            .billing_meta
            .iter()
            .map(|(k, v)| (k.clone(), Billing::from(v)))
            .collect(),
        billing_cycles: load_billing_cycles(fs.billing_cycles),
    };
    // ★★ 装载即治愈大小写分裂（2026-10-05 所有者现场：「一样的模型居然还分了两条
    // 记录」）。老文件里 `GLM-5.3-Flash` 与 `glm-5.3-flash` 各存一份，这里合并成
    // 一行；因为 `record_billed_core` 之后也用同一套归一，不会再裂开。
    //
    // ⚠ 这里**不**置 `dirty`：合并结果要等下一次真实记录（或既有 flush 路径）
    // 才落盘。若在此强制 flush，只读打开面板的进程也会改写用户的账本文件。
    merge_case_variants(&mut inner);
    inner
}

/// 装载周期状态：**只做形状容错，不判断可信度**。
///
/// 上游的「永不过期」哨兵（9999-12-31）会被原样读回来。那是有意的：参考实现的
/// 现场数据里就存着哨兵，若在装载时直接丢弃，用户要等到下一次 `/user/status`
/// 刷新才能重新看到周期。留着它，[`Stats::set_billing_cycle_at`] 的情形 2 会
/// 因为「已跟踪的值不可信」而放行真实边界，于是**下一个报告就能自愈**，
/// 不需要用户做任何事。
fn load_billing_cycles(raw: HashMap<String, CycleState>) -> HashMap<String, CycleState> {
    let mut out = HashMap::with_capacity(raw.len());
    for (uid, mut cs) in raw {
        if cs.next_reset_ms < 0 {
            cs.next_reset_ms = 0;
        }
        if cs.cycle_start_ms < 0 {
            cs.cycle_start_ms = 0;
        }
        if cs.next_reset_ms == 0 && cs.cycle_start_ms == 0 {
            // 两个都是「未知」：没有可用的信息，不必占一个键。
            continue;
        }
        out.insert(uid, cs);
    }
    out
}

/// 去掉模型名上的 `平台:区域:` 前缀，只留裸模型名。
///
/// 对应 Go `internal/usage/usage.go:338-364` 的 `bareModelName`。
///
/// 所有者逐字（2026-09-21）：「不要再把 平台:模型名 这种类似的 单独列一个统计了,
/// 这是错误的」。现场证据：`国服:deepseek-v4.1-flash` 记了 12 tokens / 1 次，
/// 而 `deepseek-v4.1-flash · 国服` 记了 1.6B / 4014 次 —— 同一份用量被前缀拆成两行。
///
/// 切分规则（`SplitN(s, ":", 3)` 的 Rust 等价实现）：
/// - 无 `:` → 原样返回
/// - 3 段（`平台:区域:模型`）→ 取第 3 段；为空则退第 2 段
/// - 2 段（`区域:模型` 或 `平台:模型`）→ 取第 2 段
/// - 切完为空 → 原样返回（宁可不切，也不能把模型名抹成空串）
pub fn bare_model_name(model: &str) -> String {
    let s = model.trim();
    if !s.contains(':') {
        return s.to_string();
    }
    let parts: Vec<&str> = s.splitn(3, ':').collect();
    let picked = match parts.len() {
        3 => {
            if parts[2].is_empty() {
                parts[1]
            } else {
                parts[2]
            }
        }
        2 => parts[1],
        _ => s,
    };
    if picked.is_empty() {
        s.to_string()
    } else {
        picked.to_string()
    }
}

/// 大小写不敏感地找**已经存在**的模型键；找不到才用 `model` 本身的写法。
///
/// 2026-10-05 所有者现场：同一个模型被拆成 `GLM-5.3-Flash · ZCode`（11.0M /
/// 98 次）与 `glm-5.3-flash · ZCode`（1.6K）两行。原话逐字：「一样的模型居然
/// 还分了两条记录」。
///
/// 为什么是「沿用已有的写法」而不是「一律转小写」：模型的**显示名**要尽量稳定。
/// 若一律转小写，历史里那份 `GLM-5.3-Flash` 会在界面改名，而 `/v1/models`
/// 目录里仍是 `GLM-5.3-Flash` —— 两处对不上又多一个新困惑。沿用已有写法则
/// 只在「首次见到该模型」时定型，之后不再变。
///
/// ⚠ 查找范围必须是 [`all_model_keys`] 那**四张表**，不能只看 `models`：
/// 一份被截断/手改过的文件完全可能只在 `accountModels` 或 `billed` 里留下键
/// （`load()` 正是残缺文件的入口）。若只看 `models`，写入路径就会用本次传来的
/// 新写法落键，而装载治愈按旧写法合并 —— 两边打架，同一个模型又裂开。
fn model_key_of(inner: &Inner, model: &str) -> String {
    // ① 精确命中（任意一张表）——原样沿用，连大小写都不动。
    if inner.models.contains_key(model)
        || inner.billed.contains_key(model)
        || inner.account_models.values().any(|m| m.contains_key(model))
    {
        return model.to_string();
    }
    // ② 忽略大小写命中（任意一张表）——沿用那个已存在的拼写。
    for k in all_model_keys(inner) {
        if k.eq_ignore_ascii_case(model) {
            return k;
        }
    }
    // ③ 全都没有：用本次写法定型。
    model.to_string()
}

/// 收集四个映射里出现过的**所有**模型名（去重、排序）。
///
/// 为什么不能只看 `models`：`load()` 是「残缺文件」的入口 —— 一份被截断或
/// 手改过的 `usage.json` 完全可能只在 `accountModels` 或 `billed` 里有键。
/// 若规范名只从 `models` 推导，那些键就会原样留着，治愈只做了一半。
fn all_model_keys(inner: &Inner) -> Vec<String> {
    let mut keys: Vec<String> = inner.models.keys().cloned().collect();
    for by_model in inner.account_models.values() {
        keys.extend(by_model.keys().cloned());
    }
    keys.extend(inner.billed.keys().cloned());
    for k in inner.billing_meta.keys() {
        if let Some((name, _)) = k.split_once('\u{0}') {
            keys.push(name.to_string());
        }
    }
    keys.sort();
    keys.dedup();
    keys
}

/// 列出合并后**仍然**存在大小写变体的模型（正常情况下恒为空）。
///
/// 为什么要有它：`merge_case_variants` 的合并只按 `canon` 里有的键走，任何
/// 遗漏（新加的第五张表、键被规范化过的映射）都会静默留下分裂。这里用与
/// 治愈完全**独立**的判据（把所有键转小写后数重复）复核结果，只用于断言与
/// 排障，不进生产路径。
#[cfg(test)]
fn leftover_case_variants(inner: &Inner) -> Vec<String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut dup: Vec<String> = Vec::new();
    for k in all_model_keys(inner) {
        if !seen.insert(k.to_ascii_lowercase()) {
            dup.push(k);
        }
    }
    dup
}

/// 为每个「小写形态」挑一个规范拼写：**总 token 最多**者胜，其次记录数多者胜，
/// 再平则取字典序小者（保证跨进程确定，不受 `HashMap` 迭代顺序影响）。
///
/// 为什么 token 量排在记录数前面 —— 用所有者 2026-10-05 的真实数据校验过：
/// `GLM-5.3-Flash` = 21,278,353 输入 / 141 次，`glm-5.3-flash` = 10,833,442
/// 输入 / 157 次。按记录数选会留下小写那份，而**上游目录里模型就叫
/// `GLM-5.3-Flash`**（`gateway_native_config.json` 的 zcode 模型表），界面
/// 显示名会与 `/v1/models` 对不上；按 token 量选则留下 `GLM-5.3-Flash`，
/// 与目录一致。
///
/// 字典序兜底还有个顺带的好处：ASCII 里大写排在小写之前，所以当两个拼写的
/// token 量恰好相等时，留下的仍是与目录一致的那个写法。
///
/// `keys` 里不在 `models` 的（残缺文件才有的情况）权重为 (0, 0)，
/// 一定输给有真实计量的那个拼写。
fn canonical_model_names(
    models: &HashMap<String, HashMap<String, Counters>>,
    keys: &[String],
) -> HashMap<String, String> {
    let mut best: HashMap<String, (String, i64, i64)> = HashMap::new();
    for k in keys {
        let (rec, tot) = match models.get(k) {
            Some(series) => (
                series.values().map(|c| c.records).sum(),
                series
                    .values()
                    .map(|c| c.input + c.output + c.cache_write)
                    .sum(),
            ),
            None => (0, 0),
        };
        let low = k.to_ascii_lowercase();
        let better = match best.get(&low) {
            None => true,
            Some((prev, pr, pt)) => {
                (tot, rec, std::cmp::Reverse(k.as_str()))
                    > (*pt, *pr, std::cmp::Reverse(prev.as_str()))
            }
        };
        if better {
            best.insert(low, (k.clone(), rec, tot));
        }
    }
    best.into_iter().map(|(l, (n, _, _))| (l, n)).collect()
}

/// 把「模型名 → 日期 → 计量」里只差大小写的键合并到规范键上（逐日相加）。
fn fold_case_keys(
    map: &mut HashMap<String, HashMap<String, Counters>>,
    canon: &HashMap<String, String>,
) {
    for k in map.keys().cloned().collect::<Vec<String>>() {
        let Some(c) = canon.get(&k.to_ascii_lowercase()) else {
            continue;
        };
        if c == &k {
            continue;
        }
        let Some(src) = map.remove(&k) else {
            continue;
        };
        let dst = map.entry(c.clone()).or_default();
        for (day, ctr) in src {
            add_to(dst, &day, ctr);
        }
    }
}

/// 三层版（模型 → 计费键 → 日期）：先并模型名，再并计费键下的日期桶。
fn fold_case_keys3(
    map: &mut HashMap<String, HashMap<String, HashMap<String, Counters>>>,
    canon: &HashMap<String, String>,
) {
    for k in map.keys().cloned().collect::<Vec<String>>() {
        let Some(c) = canon.get(&k.to_ascii_lowercase()) else {
            continue;
        };
        if c == &k {
            continue;
        }
        let Some(src) = map.remove(&k) else {
            continue;
        };
        let dst = map.entry(c.clone()).or_default();
        for (sub, series) in src {
            let slot = dst.entry(sub).or_default();
            for (day, ctr) in series {
                add_to(slot, &day, ctr);
            }
        }
    }
}

/// `billingMeta` 的键是 `模型\x00计费键`，模型名归一后必须同步改键，
/// 否则快照里按新键查不到归属（`snapshot` 里 `billing_meta.get(...)` 落空，
/// 界面那一行就丢掉平台/倍率）。
///
/// ⚠ 合并时若两个旧键归到同一个新键（例如同一模型的两种拼写各自带了同一条
/// 计费键），**保留已有的那条**、不互相覆盖：归属信息只增不减，先到者胜比
/// 让 HashMap 迭代顺序决定谁活下来要可预期。真实数据里两者的归属本来就相同
/// （都是 `zcode||?`），所以这一步在实际文件上是恒等的。
fn fold_case_billing_meta(
    map: &mut HashMap<String, Billing>,
    canon: &HashMap<String, String>,
) {
    let items: Vec<(String, Billing)> = map.drain().collect();
    for (k, v) in items {
        let (name, rest) = match k.split_once('\u{0}') {
            Some((m, r)) => (m.to_string(), r.to_string()),
            None => (k.clone(), String::new()),
        };
        let name = canon
            .get(&name.to_ascii_lowercase())
            .cloned()
            .unwrap_or(name);
        // ⚠ 分隔符用 `push` 拼而不是 `format!("...\u{0}...")`：`format!` 的
        // 参数解析会先把 `{0}` 当成位置参数，`\u{0}` 转义在那里报
        // `invalid character in unicode escape`。
        let mut key = name;
        key.push('\u{0}');
        key.push_str(&rest);
        map.entry(key).or_insert(v);
    }
}

/// 装载时把历史上因大小写分裂的模型键**就地治愈**（2026-10-05）。
///
/// 只在 `load()` 里跑一次：此后 `record_billed_core` 的 [`model_key_of`] 保证
/// 不会再产生新的分裂，下一次 `flush()` 就把合并结果落盘。
///
/// ⚠ 合并是**可加**的（日期桶逐日相加、记录数相加），因此不会丢用量 ——
/// 两个拼写各自的用量都保留，只是从此归到同一行。
fn merge_case_variants(inner: &mut Inner) {
    let keys = all_model_keys(inner);
    if keys.is_empty() {
        return;
    }
    let canon = canonical_model_names(&inner.models, &keys);
    if canon.is_empty() {
        return;
    }
    fold_case_keys(&mut inner.models, &canon);
    for by_model in inner.account_models.values_mut() {
        fold_case_keys(by_model, &canon);
    }
    fold_case_keys3(&mut inner.billed, &canon);
    fold_case_billing_meta(&mut inner.billing_meta, &canon);
}

/// 计费归属的稳定键。
///
/// 对应 Go `billingKey`：`product + "|" + region + "|" + mult`。
///
/// ★ 倍率为 `None` 时用 `"?"`，与 `0`（免费）**区分开** —— 那正是三态语义的落点：
/// 「不知道」与「免费」合并会让用户以为一个在计费的模型不花钱。
fn billing_key(b: &Billing) -> String {
    let mult = match b.multiplier {
        Some(m) => format_float(m),
        None => "?".to_string(),
    };
    format!("{}|{}|{}", b.product, b.region, mult)
}

/// 与 Go `strconv.FormatFloat(v, 'f', -1, 64)` 等价的紧凑浮点格式。
/// 与 Go `strconv.FormatFloat(v, 'f', -1, 64)` 等价的紧凑浮点格式。
///
/// ★ 下面三个小函数（`billing_identity` / `key_multiplier` / `billing_rank`）是
/// **投影层合并**（[`merge_billing_groups`]）的判据，与写路径的 [`billing_key`]
/// 严格分开：写路径的键**永远**保留倍率段，历史账单不因上游调价而被改写。
///
/// 计费键的 `(平台, 区域)` 前缀 —— `product|region|mult` 去掉最后一段。
///
/// ★★ 用**键本身**而不是 `billing_meta` 里的 `product`/`region`：老数据里同一个
/// 键前缀可能配着两条 meta（一条是 `product`/`region` 都为空的早期记录），
/// 按 meta 分组会把本该合并的两行分到两个桶里 —— 那正是要修的 bug。
fn billing_identity(key: &str) -> &str {
    match key.rfind('|') {
        Some(i) => &key[..i],
        None => key,
    }
}

/// 计费键的倍率段：`?` = **未声明**（不是免费），数字 = 已知倍率。
///
/// 与 [`billing_key`] 里 `None => "?"` 的那一段互逆。
fn key_multiplier(key: &str) -> Option<f64> {
    match key.rsplit('|').next() {
        Some("?") | None => None,
        Some(s) => s.parse::<f64>().ok(),
    }
}

/// 同一 `(平台, 区域)` 里「谁更能代表**当前**计费」的排序键。
///
/// 字段顺序就是优先级（`derive(PartialOrd)` 按声明序比较）：
/// 1. `last_day` —— 窗口内**最后仍有计量**的日期。倍率是时效性信息，界面上该显示
///    现在生效的那个数字，而不是某个早已作废的历史值。
/// 2. `meta_ok` —— 该键的 `billing_meta` 记着平台与区域。缺了它的键只是早期
///    记录（`backfill_billing` 也补不回区域），不该当选代表。
/// 3. `known` —— 已知倍率胜过「未声明」。
/// 4. `multiplier` —— 同一天出现多个倍率时取**较大**的那个：宁可让用户觉得贵
///    一点，也不要让他在以为便宜的情况下把额度烧光。
#[derive(PartialEq, PartialOrd)]
struct BillingRank {
    last_day: String,
    meta_ok: bool,
    known: bool,
    multiplier: f64,
}

/// 算一条计费键的 [`BillingRank`]；窗口内没有任何计量时返回 `None`。
fn billing_rank(
    series: &HashMap<String, Counters>,
    meta_ok: bool,
    key: &str,
    cutoff: &str,
) -> Option<BillingRank> {
    let mut last_day = String::new();
    for (day, c) in series {
        if c.is_empty() {
            continue;
        }
        if !cutoff.is_empty() && day.as_str() < cutoff {
            continue;
        }
        if day.as_str() > last_day.as_str() {
            last_day = day.clone();
        }
    }
    if last_day.is_empty() {
        return None;
    }
    let mult = key_multiplier(key);
    Some(BillingRank {
        last_day,
        meta_ok,
        known: mult.is_some(),
        multiplier: mult.unwrap_or(0.0),
    })
}

/// 从计费行的 JSON 里还原 [`Counters`]（合并时要相加）。
fn counters_of(v: &serde_json::Value) -> Counters {
    let n = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    Counters {
        input: n("input"),
        output: n("output"),
        cache_read: n("cacheRead"),
        cache_write: n("cacheWrite"),
        records: n("records"),
    }
}

/// 把**投影层**同一归属的计费行合并成一行（业主 2026-10-03：
///「一样的模型,一样的区域,居然啊划分出来三份,这是严重bug」）。
///
/// # 一行 = 一个 `(模型, 平台, 区域)`
///
/// 写路径按 `(平台, 区域, 倍率)` **三元组**分账（见 [`Billing`] 的文档），因为
/// 倍率会随上游调整而变（实测国服 `deepseek-v4.1-flash` 从 `x0.03` 调到 `x0.11`）。
/// 那是对的账本，但直接铺到界面上就成了三条同名同区域的行 —— 用户看到的不是
/// 「历史」，而是「这个模型被拆成了三份」。倍率列回答的是「现在烧这个账号贵不贵」，
/// 所以这里按 `(模型, 平台, 区域)` 收成一行，倍率取**最近仍在计量**的那条键。
///
/// # 为什么改在投影层而不是写路径
///
/// 写路径的键一旦归一化，就没法还原「那天到底是按哪个倍率记的」—— 等于篡改
/// 历史账单。所以 [`billing_key`] 与 `FileState.billed` 的键**一个字节都不动**，
/// 只在导出 `/usage` 时合并。
///
/// # 用量必须**相加**
///
/// 合并后的那一行代表该归属的**全部**用量。若只保留代表键的用量，当同一归属
/// 之外还有别的归属（多行并排显示）时，被合并掉的那部分用量会凭空消失 ——
/// 界面上的数字比实际小，而且没有任何提示。
fn merge_billing_groups(
    groups: Vec<serde_json::Value>,
    by_billing: &HashMap<String, HashMap<String, Counters>>,
    meta: &HashMap<String, Billing>,
    model: &str,
    cutoff: &str,
) -> Vec<serde_json::Value> {
    if groups.len() < 2 {
        return groups;
    }
    // ① 每个归属挑一个代表键 —— 界面显示的倍率取自它。
    let mut rep: HashMap<String, (BillingRank, String)> = HashMap::new();
    for (key, series) in by_billing {
        let meta_ok = meta
            .get(&format!("{model}\x00{key}"))
            .is_some_and(|b| !b.product.is_empty());
        let Some(rank) = billing_rank(series, meta_ok, key, cutoff) else {
            continue;
        };
        let identity = billing_identity(key).to_string();
        match rep.get(&identity) {
            Some((best, _)) if *best >= rank => {}
            _ => {
                rep.insert(identity, (rank, key.clone()));
            }
        }
    }
    // ② 按归属把用量相加。取不到归属的行原样保留（宁可多一行，也不能把两条
    //    不知道归属的用量并成一条假的）。
    let mut order: Vec<String> = Vec::new();
    let mut sums: HashMap<String, (Counters, String)> = HashMap::new();
    let mut loose: Vec<serde_json::Value> = Vec::new();
    for g in groups {
        let key = g
            .get("key")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if !meta.contains_key(&format!("{model}\x00{key}")) {
            loose.push(g);
            continue;
        }
        let identity = billing_identity(&key).to_string();
        let c = counters_of(&g);
        match sums.get_mut(&identity) {
            None => {
                order.push(identity.clone());
                sums.insert(identity, (c, key));
            }
            Some(slot) => slot.0 = slot.0.add(c),
        }
    }
    // ③ 组装：代表键决定归属，用量取合并后的总和。
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(order.len() + loose.len());
    for identity in order {
        let Some((c, first_key)) = sums.remove(&identity) else {
            continue;
        };
        let key = rep
            .get(&identity)
            .map(|(_, k)| k.clone())
            .unwrap_or(first_key);
        let mut value = c.value();
        if let Some(obj) = value.as_object_mut() {
            obj.insert("key".into(), serde_json::json!(key));
        }
        out.push(value);
    }
    out.extend(loose);
    // 合并改变了 total，重排一次（稳定排序，与 `group_list` 同口径）。
    out.sort_by(|a, b| {
        let ta = a.get("total").and_then(|v| v.as_i64()).unwrap_or(0);
        let tb = b.get("total").and_then(|v| v.as_i64()).unwrap_or(0);
        tb.cmp(&ta)
    });
    out
}

fn format_float(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        // 整数倍率（0 / 1 / 3）不带小数点 —— 与 Go 的 'f' -1 精度一致。
        format!("{}", v as i64)
    } else {
        let mut s = format!("{v}");
        if s.contains('e') || s.contains('E') {
            // 极值回退到定点，避免与 Go 的 'f' 格式分叉出不同的键。
            s = format!("{v:.10}");
            while s.ends_with('0') {
                s.pop();
            }
            if s.ends_with('.') {
                s.pop();
            }
        }
        s
    }
}

/// 三层映射取（必要时创建）内层。
fn nested2<'a>(
    m: &'a mut HashMap<String, HashMap<String, HashMap<String, Counters>>>,
    key: &str,
    sub: &str,
) -> &'a mut HashMap<String, Counters> {
    m.entry(key.to_string())
        .or_default()
        .entry(sub.to_string())
        .or_default()
}

/// 依据「账号 × 模型 × 日期」交叉，给没有归属的计费明细**补上平台**。
///
/// 对应 Go `backfillBillingLocked`。所有者 2026-09-21 逐字：「这个应该持久化平台
/// 显示啊,这要不刚开始都没办法区分」—— 他的截图里按模型 `deepseek-v4.1-flash`
/// 是 1.7B / 4223 次，而同一模型的计费明细只有 3 次，因为计费归属此前只存在内存里，
/// 网关一重启就清零。落盘只解决「今后」，已丢的归属靠这里回填。
///
/// # 判据的优先级：**先看计费键，再看 uid 形态**
///
/// 计费键本身就是按 `平台|区域|倍率` 造出来的（见 [`billing_key`]），所以键的
/// 第一段**就是**那次请求的真实平台，比从 uid 形态反推可靠得多。
///
/// ★★★ 2026-10-05 修：这里原先**只用** uid 形态，而且是「一个 uid 命中了某模型，
/// 就把该模型**所有**计费行都盖上这个 uid 的平台」。于是只要某模型被一个裸 UUID
/// （= WorkBuddy）账号用过，它那些 `qoder||?` / `zcode||?` 的行会被一起写成
/// `workbuddy`；而 `account_models` 是 `HashMap`，**哪个 uid 先被迭代到是不确定的**
/// —— 实测同一个 `usage.json` 连起 4 次，`product` 的分布分别是
/// 9/1、8/2、9/1、10/0（`workbuddy`/`zcode`），界面上那几行「· Qoder」「· ZCode」
/// 会随网关重启变来变去。键前缀是**逐行**属性，改用它之后结果与迭代顺序无关。
///
/// 依据是 uid 的**形态**本身带平台信息（仅在键没有前缀时兜底）：
/// - `zcode-xxxx` → `zcode`
/// - `qoder-xxxx` → `qoder`
/// - 裸 UUID → `workbuddy`（WorkBuddy 在 auth 层用**空串**表示产品，落进 usage
///   就是裸 UUID；与 `accounts.json` 的 uid 形态一致，实测 11/13 能对上）
///
/// ★★ 只补 `product`，**不编造** `region` 与倍率 —— 编一个会让界面显示错误计费，
/// 比显示「未知」更糟（用户会据它判断该烧哪个账号的额度）。
fn backfill_billing(
    account_models: &HashMap<String, HashMap<String, HashMap<String, Counters>>>,
    cutoff: &str,
    model_billing: &mut serde_json::Map<String, serde_json::Value>,
) {
    for (uid, by_model) in account_models {
        // uid 形态只作兜底（键没有平台前缀时）。★ 不再拿它当主判据 —— 见函数文档。
        let uid_product = product_from_uid(uid);
        for (model, series) in by_model {
            if sum_days(series, cutoff).is_empty() {
                continue;
            }
            let Some(groups) = model_billing.get_mut(model).and_then(|v| v.as_array_mut()) else {
                continue;
            };
            for g in groups.iter_mut() {
                let Some(obj) = g.as_object_mut() else {
                    continue;
                };
                // 已有归属的不动 —— 回填只是补空缺。
                let missing = obj
                    .get("product")
                    .and_then(|v| v.as_str())
                    .map(|s| s.is_empty())
                    .unwrap_or(true);
                if !missing {
                    continue;
                }
                // ★★ 主判据：计费键的第一段就是那次请求的真实平台。
                //    必须先把键拷成 owned，`obj` 下面还要可变借用。
                let key = obj
                    .get("key")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let from_key = key.split('|').next().unwrap_or_default();
                let product = if from_key.is_empty() {
                    match uid_product {
                        Some(p) => p,
                        None => continue,
                    }
                } else {
                    from_key
                };
                obj.insert("product".into(), serde_json::json!(product));
            }
        }
    }
}

/// 从 uid 形态推断平台。裸 UUID（含 `-` 的 36 字符十六进制）→ `workbuddy`。
///
/// 对应 Go `productFromUID`。返回 `None` 表示形态不认识 —— 此时**不猜**，
/// 让那一行保持无归属。
fn product_from_uid(uid: &str) -> Option<&'static str> {
    if uid.starts_with("zcode-") {
        return Some("zcode");
    }
    if uid.starts_with("qoder-") {
        return Some("qoder");
    }
    if uid.starts_with("trae-") {
        return Some("trae");
    }
    if is_bare_uuid(uid) {
        return Some("workbuddy");
    }
    None
}

/// 是否是裸 UUID（8-4-4-4-12 的十六进制）。
fn is_bare_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    let lens = [8usize, 4, 4, 4, 12];
    if groups.len() != lens.len() {
        return false;
    }
    groups
        .iter()
        .zip(lens.iter())
        .all(|(g, n)| g.len() == *n && g.chars().all(|c| c.is_ascii_hexdigit()))
}

/// 从上游 `usage` 对象提取计量。
///
/// 返回 `None` 表示该对象没有任何可识别的 token 字段（例如上游漏发 `usage`），
/// 调用方应跳过本次统计。字段兼容 OpenAI 标准命名与 CodeBuddy 实际会返回的别名：
///
/// - input：`prompt_tokens` / `input_tokens`
/// - output：`completion_tokens` / `output_tokens` / `output_text_tokens`
/// - cacheRead：`prompt_cache_hit_tokens` > `prompt_tokens_details.cached_tokens`
///   > `input_tokens_details[].cached_tokens`
/// - cacheWrite：`prompt_cache_write_tokens` / `cache_write_input_tokens` /
///   `cache_creation_input_tokens`
pub fn parse_openai_usage(u: Option<&serde_json::Value>) -> Option<Counters> {
    let u = u?.as_object()?;
    let (input, has_input) = first_number(u, &["prompt_tokens", "input_tokens"]);
    let (output, has_output) = first_number(
        u,
        &["completion_tokens", "output_tokens", "output_text_tokens"],
    );
    // 兜底：只认明确出现的 token 字段，避免把无关对象记成 0 记录。
    if !has_input && !has_output {
        let any = ["prompt_tokens", "input_tokens", "completion_tokens", "output_tokens"]
            .iter()
            .any(|k| u.contains_key(*k));
        if !any {
            return None;
        }
    }

    let (mut read, _) = first_number(u, &["prompt_cache_hit_tokens", "cache_read_input_tokens"]);
    if read == 0 {
        if let Some(details) = u.get("prompt_tokens_details").and_then(|d| d.as_object()) {
            let (n, _) = first_number(details, &["cached_tokens"]);
            read = n;
        }
    }
    if read == 0 {
        if let Some(details) = u.get("input_tokens_details").and_then(|d| d.as_array()) {
            for item in details {
                if let Some(m) = item.as_object() {
                    let (n, ok) = first_number(m, &["cached_tokens"]);
                    if ok && n > 0 {
                        read = n;
                        break;
                    }
                }
            }
        }
    }
    let (write, _) = first_number(
        u,
        &[
            "prompt_cache_write_tokens",
            "cache_write_input_tokens",
            "cache_creation_input_tokens",
        ],
    );

    Some(Counters {
        input,
        output,
        cache_read: read,
        cache_write: write,
        records: 0,
    })
}

/// 取第一个存在的数字字段。
fn first_number(m: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> (i64, bool) {
    for key in keys {
        if let Some(v) = m.get(*key) {
            return (number_value(v), true);
        }
    }
    (0, false)
}

/// 把 JSON 值转成整数（兼容数字与数字字符串）。
fn number_value(v: &serde_json::Value) -> i64 {
    match v {
        serde_json::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0),
        serde_json::Value::String(s) => s.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

// ─────────────────────────── 计费周期（Go `usage/cycle.go`，N4） ───────────────────────────

/// 可信边界的上限：2100-01-01T00:00:00Z（Unix 毫秒）。
///
/// 对应 Go `MaxPlausibleResetMs`。与 `products/qoder` 的同名常量**同值但各自一份**：
/// 本模块是 stdlib-only 的统计层，qoder 是协议层，二者不互相依赖（qoder 依赖
/// usage 才是正确的方向，而本任务连那个方向都不需要）。两边各自注释指向对方，
/// 改一处时另一处要一起改。
pub const MAX_PLAUSIBLE_RESET_MS: i64 = 4_102_444_800_000;

/// 单次推进最多跨多少个月。
///
/// 2400 个月 = 200 年：正常账号永远碰不到（一个月一次），而一个**损坏的**远过去
/// 边界（例如 1970）不会让推进循环空转上百万次 —— 那个循环是**持锁**跑的，
/// 空转等于把整个网关卡死。与参考实现同值。
const MAX_CYCLE_CATCH_UPS: usize = 2400;

/// 报告一个重置时刻是否可信。
///
/// 不可信的有两类，都必须拒绝：
///
/// ```text
/// <= 0    —— 上游没报（或报了 0），语义是「未知」，不是「立刻重置」
/// >= 上限 —— 上游的「永不过期」哨兵（9999-12-31），不是真实时刻
/// ```
pub fn plausible_reset_ms(ms: i64) -> bool {
    ms > 0 && ms < MAX_PLAUSIBLE_RESET_MS
}

/// 单个账号的计费周期**状态**（落盘）。
///
/// 只有两个字段：本周期用量是从 `accounts` 投影出来的，不在这里存。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CycleState {
    /// 当前周期的结束时刻（下一次重置）。`0` = 未知。
    #[serde(rename = "nextResetAtMs", skip_serializing_if = "is_zero_i64")]
    pub next_reset_ms: i64,
    /// 当前周期的开始时刻。
    ///
    /// 由 `add_months_ms(next_reset_ms, -1)` 推导（套餐是月度的），而不是由上游
    /// 直接给出 —— 上游只报「下次什么时候重置」。推进边界时它等于被跨过的那个
    /// 旧边界（比「再减一个月」更准：跨过的边界就是事实）。
    #[serde(rename = "cycleStartAtMs", skip_serializing_if = "is_zero_i64")]
    pub cycle_start_ms: i64,
}

/// `serde` 的 `omitempty` 等价物（Go 的 `json:",omitempty"` 对 int64 即「0 省略」）。
fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}

/// 某账号当前周期的**只读投影**。
///
/// 读路径返回它而不是 [`CycleState`]：投影出来的边界可能已经推进过（时间过了
/// 重置点），但那个推进**没有**被写回。这样面板随时可读，而「落盘的状态」
/// 只由写路径改变。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CycleProjection {
    /// 投影后的下一次重置时刻；`0` = 未知（无周期信息或边界不可推进）。
    pub next_reset_ms: i64,
    /// 投影后的当前周期开始时刻；`0` = 未知。
    pub cycle_start_ms: i64,
    /// 本周期已记录的用量（按周期窗口从 `accounts` 投影）。
    ///
    /// 窗口是**日期粒度**的：周期边界是毫秒时刻，而用量按本地日期聚合。因此
    /// 边界当天的用量整体归入该边界所属的那一侧。这是刻意的取舍 —— 为了把精度
    /// 做到秒级而另存一份逐请求账本，代价远大于收益。
    pub used: Counters,
    /// 是否知道「下一次重置在什么时候」（= `next_reset_ms > 0`）。
    ///
    /// `false` 时 `used` **不是**「本周期用量」：它只保证「从 `cycle_start_ms`
    /// 起」（`cycle_start_ms` 也为 0 时就是全部历史）。界面必须据此显示「未知」，
    /// 而不是把它当成一个周期用量 —— 那会把全部历史说成一个月的量。
    pub known: bool,
}

impl Stats {
    /// 取当前时刻（Unix 毫秒）。
    ///
    /// 对应 Go `Stats.clock()`：测试注入的时钟优先（[`Stats::set_now_for_test`]），
    /// 否则用真实本地时钟。
    pub fn clock_ms(&self) -> i64 {
        let injected = self.now_override_ms.load(Ordering::Relaxed);
        if injected != 0 {
            return injected;
        }
        chrono::Local::now().timestamp_millis()
    }

    /// 注入一个固定时钟（测试用）。对应 Go 直接赋值 `s.now = func() ...`。
    #[cfg(test)]
    pub(crate) fn set_now_for_test(&self, ms: i64) {
        self.now_override_ms.store(ms, Ordering::Relaxed);
    }

    /// 读落盘的周期状态（测试与诊断用）。
    ///
    /// Go 侧测试直接读未导出的 `s.billingCycles`；Rust 的字段在 `Mutex<Inner>` 里，
    /// 故给一个受限的读取器。
    #[cfg(test)]
    pub(crate) fn cycle_state_for_test(&self, uid: &str) -> Option<CycleState> {
        self.inner.lock().ok()?.billing_cycles.get(uid).copied()
    }

    /// 全部落盘的周期状态（测试用）。
    #[cfg(test)]
    pub(crate) fn cycle_states_for_test(&self) -> HashMap<String, CycleState> {
        match self.inner.lock() {
            Ok(inner) => inner.billing_cycles.clone(),
            Err(_) => HashMap::new(),
        }
    }

    /// 直接塞一份周期状态（测试用；模拟「旧文件 / 手改」的形态）。
    #[cfg(test)]
    pub(crate) fn force_cycle_state_for_test(
        &self,
        uid: &str,
        next_reset_ms: i64,
        cycle_start_ms: i64,
    ) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.billing_cycles.insert(
                uid.to_string(),
                CycleState { next_reset_ms, cycle_start_ms },
            );
        }
    }

    /// 清脏标志（测试用；模拟「刚落过盘」）。
    #[cfg(test)]
    pub(crate) fn clear_dirty_for_test(&self) {
        self.dirty.store(false, Ordering::Relaxed);
    }

    /// 读脏标志（测试用）。
    #[cfg(test)]
    pub(crate) fn dirty_for_test(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    /// 采纳上游报告的周期边界（**写路径**）。
    ///
    /// 返回是否改变了已跟踪的状态（供调用方统计「本轮更新了几个账号」）。
    /// 详见 [`Stats::set_billing_cycle_at`] 的四情形说明。
    pub fn set_billing_cycle(&self, uid: &str, next_reset_ms: i64) -> bool {
        self.set_billing_cycle_at(uid, next_reset_ms, self.clock_ms())
    }

    /// 同 [`Stats::set_billing_cycle`]，但显式指定「现在」（测试用）。
    pub fn set_billing_cycle_at(&self, uid: &str, next_reset_ms: i64, now_ms: i64) -> bool {
        if uid.is_empty() {
            return false;
        }
        if !plausible_reset_ms(next_reset_ms) {
            // ⓪ 上游没报 / 报了哨兵。保留既有状态 —— 不要用一个不可信的值覆盖一个
            // 可信的值（那会让情形 ② 的判据失效）。
            return false;
        }

        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };

        // ★ 先推进再比较。
        //
        // 已跟踪的边界可能**已经过去**（网关很久没刷新，或上游一直没报）。那种
        // 情况下先 roll 到「现在所属的周期」，再拿新报告去比，才比得准：否则一个
        // 本应等于「推进后的边界」的报告会被情形 ② 当成陈旧报告丢掉。
        let changed = roll_cycle_locked(&mut inner, uid, now_ms);

        let cur = inner.billing_cycles.get(uid).copied().unwrap_or_default();
        if cur.next_reset_ms == next_reset_ms {
            // ① 同一个周期：幂等。**不置脏** —— 否则每轮刷新都会写一次盘，
            // 而这个值根本没变。
            return changed;
        }
        if plausible_reset_ms(cur.next_reset_ms) && next_reset_ms <= cur.next_reset_ms {
            // ② 陈旧报告：上游（或缓存的响应）报了一个不晚于当前边界的时刻。
            // 保留较新的那个。判据要求 `cur` **可信** —— 若 `cur` 是不可信的
            // 哨兵（从旧版本文件读回来的），这里必须放行，否则真实边界永远进不来。
            return changed;
        }

        // ③ 真新周期：采纳边界，并按「月度套餐」推导周期起点。
        inner.billing_cycles.insert(
            uid.to_string(),
            CycleState {
                next_reset_ms,
                cycle_start_ms: add_months_ms(next_reset_ms, -1),
            },
        );
        self.dirty.store(true, Ordering::Relaxed);
        true
    }

    /// 读某账号当前周期的**只读投影**（读路径，绝不修改状态）。
    ///
    /// 返回 `None` 表示该账号**没有周期信息**（从没收到过边界，此时用量是全部
    /// 历史，不能当「本周期」用）。
    pub fn cycle_of(&self, uid: &str) -> Option<CycleProjection> {
        if uid.is_empty() {
            return None;
        }
        let Ok(inner) = self.inner.lock() else {
            return None;
        };
        let p = self.project_cycle_locked(&inner, uid, self.clock_ms());
        if p.known {
            Some(p)
        } else {
            None
        }
    }

    /// 在**局部拷贝**上推进周期并投影本周期用量。
    ///
    /// 调用方必须已持有 `inner` 的锁。**绝不回写** `inner.billing_cycles` ——
    /// 面板轮询恰好落在重置点之后时，回写会让「推进」被落盘。
    fn project_cycle_locked(&self, inner: &Inner, uid: &str, now_ms: i64) -> CycleProjection {
        // 值拷贝：推进只发生在这份拷贝上。
        let cur = inner.billing_cycles.get(uid).copied().unwrap_or_default();
        let mut start_ms = cur.cycle_start_ms;
        let mut next_ms = cur.next_reset_ms;

        if next_ms <= 0 {
            // 没有可信边界（从未刷新过 / 被 load 归一成 0 / 被推进清空）⇒ 我们
            // **不知道**这个账号的周期。返回零值而不是「从某个起点算起」：把全部
            // 历史当成一个周期报出去，界面会显示一个错的数字，比不显示更糟。
            return CycleProjection::default();
        }
        if start_ms <= 0 {
            // 有边界但缺起点：只可能来自旧文件或手改（set_billing_cycle_at 总会
            // 一起写起点）。按月度契约推导，与写入路径保持一致 —— 若这里留 0，
            // 窗口就没有下界，本周期用量会把**全部历史**算进来。
            start_ms = add_months_ms(next_ms, -1);
        }

        let mut i = 0usize;
        while next_ms > 0 && now_ms >= next_ms && i < MAX_CYCLE_CATCH_UPS {
            // 跨过一个边界：新周期的起点就是刚跨过的那个边界。
            start_ms = next_ms;
            let adv = next_cycle_boundary(next_ms);
            if adv == 0 {
                // 该时刻的下一个月步进**不前进**（DST 异常时区）。再推下去就是
                // 死循环 ⇒ 报「未知」，让调用方显示「无周期信息」。
                next_ms = 0;
                break;
            }
            next_ms = adv;
            i += 1;
        }
        if next_ms > 0 && now_ms >= next_ms {
            // 追上了上限仍未越过 now：边界不可信，报「未知」。
            next_ms = 0;
        }

        CycleProjection {
            next_reset_ms: next_ms,
            cycle_start_ms: start_ms,
            used: self.cycle_usage_locked(inner, uid, start_ms, next_ms),
            // `known` 在**推进之后**判定：边界不可推进（DST 异常）或超出追赶上限
            // 时 next_ms 被清成 0，此时我们确实不知道「下次什么时候重置」。
            known: next_ms > 0,
        }
    }

    /// 把周期窗口 `[start_ms, next_ms)` 投影到按日聚合的用量上。
    ///
    /// 调用方必须已持有 `inner` 的锁。
    ///
    /// # 为什么可以这么做（而不是另存一个累加器）
    ///
    /// `inner.accounts[uid]` 已经是「账号 × 本地日期」的完整账本，而周期窗口
    /// 天然落在日期区间上：把窗口两端各映射成一个日期键，落在区间内的日期求和
    /// 即可。窗口两端是毫秒，映射到日期是**有损**的（见 [`CycleProjection::used`]）。
    ///
    /// # 边界情况
    ///
    /// ```text
    /// start_ms <= 0 —— 起点未知：不设下界（能确定的上界仍然生效）
    /// next_ms  <= 0 —— 终点未知：不设上界
    /// 两者都未知    —— 返回全部历史（调用方据 known=false 区分语义）
    /// ```
    fn cycle_usage_locked(&self, inner: &Inner, uid: &str, start_ms: i64, next_ms: i64) -> Counters {
        let Some(per_day) = inner.accounts.get(uid) else {
            return Counters::default();
        };
        // 日期键是定宽字符串（`%Y-%m-%d`），故可直接按字典序比较。
        let start_day = if start_ms > 0 {
            day_of_ms(start_ms)
        } else {
            String::new()
        };
        let end_day = if next_ms > 0 {
            day_of_ms(next_ms)
        } else {
            String::new()
        };

        let mut total = Counters::default();
        for (day, c) in per_day {
            if !start_day.is_empty() && day.as_str() < start_day.as_str() {
                continue;
            }
            if !end_day.is_empty() && day.as_str() >= end_day.as_str() {
                // 上界是**开区间**：重置当天的用量属于新周期。
                continue;
            }
            total = total.add(*c);
        }
        total
    }

    /// 导出全部账号的周期投影，供 `/usage` 展示（对应 Go `cycleReportLocked`）。
    ///
    /// 调用方必须已持有 `inner` 的锁。
    ///
    /// # 形状
    ///
    /// `uid -> { nextResetAtMs, cycleStartAtMs, known, input, output, … }`
    ///
    /// `used` 用 [`Counters::value`] 的既有口径（与 `models` / `accounts` 里的行
    /// **同一套字段**），前端不需要为周期另写一套渲染逻辑。
    ///
    /// # 为什么只输出**有周期信息**的账号
    ///
    /// 没有任何周期信息的账号若也输出一行 `known:false` + 全部历史用量，界面会把
    /// 「这个号历史上用过的总量」显示成「本周期用量」—— 那是**错的数字**，比不
    /// 显示更糟。故这里只输出真正知道边界的账号。
    fn cycle_report_locked(&self, inner: &Inner, now_ms: i64) -> serde_json::Value {
        let mut uids: Vec<&String> = inner.billing_cycles.keys().collect();
        // 排序只为让输出稳定（Go 的 map 无序，JSON 对象键序无语义）。
        uids.sort();
        let mut out = serde_json::Map::new();
        for uid in uids {
            let p = self.project_cycle_locked(inner, uid, now_ms);
            if !p.known {
                continue;
            }
            let mut row = p.used.value();
            if let Some(obj) = row.as_object_mut() {
                obj.insert("nextResetAtMs".into(), serde_json::json!(p.next_reset_ms));
                obj.insert("cycleStartAtMs".into(), serde_json::json!(p.cycle_start_ms));
                obj.insert("known".into(), serde_json::json!(p.known));
            }
            out.insert(uid.clone(), row);
        }
        serde_json::Value::Object(out)
    }
}

/// 在**真实状态**上推进周期（写路径）。返回是否改变了状态。
///
/// 调用方必须已持有 `inner` 的锁。
///
/// 与 [`Stats::project_cycle_locked`] 同构，但改的是 `inner.billing_cycles` 本身
/// —— 只有写路径（有请求进来、或刷新拿到了新边界）才允许这么做。
fn roll_cycle_locked(inner: &mut Inner, uid: &str, now_ms: i64) -> bool {
    let Some(mut cur) = inner.billing_cycles.get(uid).copied() else {
        return false;
    };
    if cur.next_reset_ms <= 0 {
        return false;
    }
    let mut changed = false;
    let mut i = 0usize;
    while cur.next_reset_ms > 0 && now_ms >= cur.next_reset_ms && i < MAX_CYCLE_CATCH_UPS {
        cur.cycle_start_ms = cur.next_reset_ms;
        let adv = next_cycle_boundary(cur.next_reset_ms);
        if adv == 0 {
            // 不可推进的边界：清成「未知」而不是留在原地 —— 留在原地会让每次写
            // 路径都再试一遍同一个死循环（虽然被上限兜住，但那是每写一次白跑
            // 2400 次）。清掉后，下一个真实边界会重新建立周期。
            cur.next_reset_ms = 0;
            changed = true;
            break;
        }
        cur.next_reset_ms = adv;
        changed = true;
        i += 1;
    }
    if cur.next_reset_ms > 0 && now_ms >= cur.next_reset_ms {
        // 上限用尽仍未越过 now：这个边界不可信，同样清成「未知」。
        cur.next_reset_ms = 0;
        changed = true;
    }
    if changed {
        inner.billing_cycles.insert(uid.to_string(), cur);
    }
    changed
}

/// 把毫秒时刻按「日历月」加减 `n` 个月（本地时区）。
///
/// # 为什么不能直接用「加天数」或朴素的溢出归一（这条是真踩过的坑）
///
/// Go 的 `time.AddDate` 遇到「月末没有那一天」时**不钳制，而是溢出**：
///
/// ```text
/// 3 月 31 日 减一个月 → 3 月 3 日（2 月 31 日溢出成 3 月 3 日）
/// 5 月 31 日 减一个月 → 5 月 1 日（4 月 31 日溢出成 5 月 1 日）
/// ```
///
/// 而这里的返回值要用来推导「周期起点」，起点一旦落到月初附近，就会让按窗口
/// 投影出的「本周期用量」错得离谱（把上一周期整月的用量算进来，或者把本周期
/// 的头几天漏掉）。故必须**钳制到当月最后一天**。
fn add_months_ms(ms: i64, n: i32) -> i64 {
    add_months_ms_in(ms, n, &chrono::Local)
}

/// 同 [`add_months_ms`]，但显式指定时区（测试用）。
///
/// 返回 `0` 表示**算不出来**（`ms <= 0`、或本地时刻落在 DST 空洞里）。
/// Go 那边 `time.Date` 对越界值做归一化、永不失败；Rust 的 `LocalResult` 在
/// DST 空洞里是 `None`，这里选择如实返回 `0`（调用方 [`advance_boundary_ms`]
/// 会把它当成「不可推进」），而不是编一个时刻出来。
fn add_months_ms_in<Tz: chrono::TimeZone>(ms: i64, n: i32, tz: &Tz) -> i64 {
    if ms <= 0 {
        return 0;
    }
    let Some(t) = tz.timestamp_millis_opt(ms).single() else {
        return 0;
    };
    // 先定位到目标月的 1 号（避免「先算日再溢出」），再求该月最后一天，最后把日
    // 钳制进去。时分秒与纳秒原样保留 —— 重置时刻的「几点几分」是上游给的，
    // 不能因为跨月就丢掉。
    let (ty, tm) = normalize_year_month(t.year(), t.month() as i32 + n);
    let last = days_in_month(ty, tm);
    let day = t.day().min(last);
    let Some(naive) = chrono::NaiveDate::from_ymd_opt(ty, tm, day).and_then(|d| {
        d.and_hms_nano_opt(t.hour(), t.minute(), t.second(), t.nanosecond())
    }) else {
        return 0;
    };
    // DST 重叠取**较早**的那个（Go 的 time.Date 同样是确定性的，只是取哪个由
    // 其内部规则定；这里取较早者并写进注释，避免「看起来随机」）。
    match tz.from_local_datetime(&naive).earliest() {
        Some(dt) => dt.timestamp_millis(),
        None => 0,
    }
}

/// 求「下一个周期边界」；`0` 表示不可推进。
fn next_cycle_boundary(next_ms: i64) -> i64 {
    advance_boundary_ms(next_ms, &chrono::Local)
}

/// 同 [`next_cycle_boundary`]，但显式指定时区（测试用）。
///
/// 返回 `0` 表示**这一步没有前进**（`adv <= next_ms`）。这在正常时区不会发生，
/// 但 DST 异常的历史时区里存在「某个月步进后时刻反而更早」的瞬间（Go 侧实测
/// `America/Asuncion` 1972-09、`America/Havana`、`America/St_Johns`）。那种时刻
/// 若不返回 0，推进循环就会**永远不前进**，而它是持锁跑的 —— 等于把网关卡死。
/// 故这里必须显式拒绝，由调用方把边界清成「未知」。
fn advance_boundary_ms<Tz: chrono::TimeZone>(next_ms: i64, tz: &Tz) -> i64 {
    let adv = add_months_ms_in(next_ms, 1, tz);
    if adv <= next_ms {
        return 0;
    }
    adv
}

/// 把「年 + 可能越界的月」归一成合法的 `(年, 月)`。
///
/// `month` 允许 0 或 13 这种值（调用方会传 `month - 1` / `month + 1`）。
fn normalize_year_month(year: i32, month: i32) -> (i32, u32) {
    let zero_based = month - 1;
    let y = year + zero_based.div_euclid(12);
    let m = zero_based.rem_euclid(12) as u32 + 1;
    (y, m)
}

/// 该年该月的天数（`month` 必须是 1..=12）。
fn days_in_month(year: i32, month: u32) -> u32 {
    let (ny, nm) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    let Some(first_next) = chrono::NaiveDate::from_ymd_opt(ny, nm, 1) else {
        return 31;
    };
    match first_next.pred_opt() {
        Some(last) => last.day(),
        None => 31,
    }
}

/// 毫秒时刻 → 本地日期键（`%Y-%m-%d`）。
///
/// 越界到无法表示的毫秒（`i64` 极端值）返回空串，调用方据此退化成「不设边界」。
fn day_of_ms(ms: i64) -> String {
    match chrono::Local.timestamp_millis_opt(ms).single() {
        Some(dt) => dt.format(DAY_LAYOUT).to_string(),
        None => String::new(),
    }
}

/// 本地日期键 → 该日本地零点的毫秒时刻；解析不了返回 `0`。
///
/// 只给「只有日期键、没有时刻」的调用路径用（[`Stats::record_billed_at`]）。
/// 生产路径走 [`Stats::record`] / [`Stats::record_billed`]，它们有真实时刻。
fn day_start_ms(day: &str) -> i64 {
    match chrono::NaiveDate::parse_from_str(day, DAY_LAYOUT) {
        Ok(d) => d
            .and_hms_opt(0, 0, 0)
            .and_then(|naive| chrono::Local.from_local_datetime(&naive).earliest())
            .map(|dt| dt.timestamp_millis())
            .unwrap_or(0),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn counters_value_matches_host_contract() {
        let c = Counters {
            input: 100,
            output: 20,
            cache_read: 30,
            cache_write: 5,
            records: 2,
        };
        let v = c.value();
        // total 不含 cacheRead（input 已含它），否则会重复计数
        assert_eq!(v["total"], json!(125));
        assert_eq!(v["input"], json!(100));
        assert_eq!(v["output"], json!(20));
        assert_eq!(v["cacheRead"], json!(30));
        assert_eq!(v["cacheWrite"], json!(5));
        assert_eq!(v["uncachedInput"], json!(70));
        assert_eq!(v["records"], json!(2));
        assert_eq!(v["cacheHitRate"], json!(0.3));
    }

    #[test]
    fn cache_hit_rate_null_when_no_input() {
        let c = Counters::default();
        assert_eq!(c.value()["cacheHitRate"], serde_json::Value::Null);
    }

    #[test]
    fn uncached_input_never_negative() {
        let c = Counters {
            input: 10,
            cache_read: 50,
            ..Default::default()
        };
        assert_eq!(c.value()["uncachedInput"], json!(0));
    }

    #[test]
    fn record_aggregates_three_dimensions() {
        let s = Stats::new("");
        s.record_at("u1", "m1", "2026-09-18", Counters { input: 10, output: 5, ..Default::default() });
        s.record_at("u1", "m2", "2026-09-18", Counters { input: 20, output: 1, ..Default::default() });
        s.record_at("u2", "m1", "2026-09-17", Counters { input: 7, output: 2, ..Default::default() });

        let snap = s.snapshot(0);
        assert_eq!(snap["summary"]["input"], json!(37));
        assert_eq!(snap["summary"]["output"], json!(8));
        assert_eq!(snap["summary"]["records"], json!(3));
        // 模型维度
        let models = snap["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        // 账号维度
        let accounts = snap["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 2);
        // 日维度按日期升序
        let daily = snap["daily"].as_array().unwrap();
        assert_eq!(daily.len(), 2);
        assert_eq!(daily[0]["key"], json!("2026-09-17"));
        assert_eq!(daily[1]["key"], json!("2026-09-18"));
    }

    /// 上游未返回 usage 时不得产生空记录。
    #[test]
    fn empty_counters_are_not_recorded() {
        let s = Stats::new("");
        s.record_at("u1", "m1", "2026-09-18", Counters::default());
        let snap = s.snapshot(0);
        assert_eq!(snap["summary"]["records"], json!(0));
        assert!(snap["models"].as_array().unwrap().is_empty());
    }

    #[test]
    fn empty_uid_and_model_normalize_to_dash() {
        let s = Stats::new("");
        s.record_at("", "", "2026-09-18", Counters { input: 1, ..Default::default() });
        let snap = s.snapshot(0);
        assert_eq!(snap["models"][0]["key"], json!("-"));
        assert_eq!(snap["accounts"][0]["key"], json!("-"));
    }

    /// 天数过滤：cutoff 之前的日期不参与聚合。
    #[test]
    fn days_filter_excludes_old_entries() {
        let s = Stats::new("");
        // 用一个很久以前的日期，确保任何 days>0 都会把它排除
        s.record_at("u1", "m1", "2000-01-01", Counters { input: 100, ..Default::default() });
        s.record_at("u1", "m1", &local_day_now(), Counters { input: 5, ..Default::default() });

        let all = s.snapshot(0);
        assert_eq!(all["summary"]["input"], json!(105));
        assert_eq!(all["rangeDays"], serde_json::Value::Null);

        let recent = s.snapshot(7);
        assert_eq!(recent["summary"]["input"], json!(5), "老数据应被排除");
        assert_eq!(recent["rangeDays"], json!(7));
    }

    /// models 列表按 total 降序。
    #[test]
    fn group_list_sorted_by_total_desc() {
        let s = Stats::new("");
        s.record_at("u", "small", "2026-09-18", Counters { input: 1, ..Default::default() });
        s.record_at("u", "big", "2026-09-18", Counters { input: 100, ..Default::default() });
        let snap = s.snapshot(0);
        let models = snap["models"].as_array().unwrap();
        assert_eq!(models[0]["key"], json!("big"));
        assert_eq!(models[1]["key"], json!("small"));
    }

    #[test]
    fn flush_and_reload_roundtrip() {
        let dir = std::env::temp_dir().join(format!("wbswg-usage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("usage.json");
        let path_s = path.to_string_lossy().to_string();

        {
            let s = Stats::new(&path_s);
            s.record_at("u1", "m1", "2026-09-18", Counters { input: 42, output: 8, ..Default::default() });
            s.flush();
        }
        // 新实例应能读回
        let s2 = Stats::new(&path_s);
        let snap = s2.snapshot(0);
        assert_eq!(snap["summary"]["input"], json!(42));
        assert_eq!(snap["summary"]["output"], json!(8));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flush_without_changes_is_noop() {
        let s = Stats::new("");
        s.flush(); // 纯内存模式：不应 panic
    }

    #[test]
    fn parse_usage_standard_fields() {
        let c = parse_openai_usage(Some(&json!({
            "prompt_tokens": 100, "completion_tokens": 20
        })))
        .unwrap();
        assert_eq!(c.input, 100);
        assert_eq!(c.output, 20);
        assert_eq!(c.records, 0, "records 由 record 时置 1");
    }

    #[test]
    fn parse_usage_aliases() {
        let c = parse_openai_usage(Some(&json!({
            "input_tokens": 5, "output_tokens": 3, "output_text_tokens": 9
        })))
        .unwrap();
        assert_eq!(c.input, 5);
        assert_eq!(c.output, 3, "output_tokens 优先于 output_text_tokens");
    }

    #[test]
    fn parse_usage_cache_read_precedence() {
        // prompt_cache_hit_tokens 优先
        let c = parse_openai_usage(Some(&json!({
            "prompt_tokens": 10, "prompt_cache_hit_tokens": 7,
            "prompt_tokens_details": {"cached_tokens": 3}
        })))
        .unwrap();
        assert_eq!(c.cache_read, 7);

        // 回退 prompt_tokens_details.cached_tokens
        let c2 = parse_openai_usage(Some(&json!({
            "prompt_tokens": 10, "prompt_tokens_details": {"cached_tokens": 4}
        })))
        .unwrap();
        assert_eq!(c2.cache_read, 4);

        // 回退 input_tokens_details[].cached_tokens
        let c3 = parse_openai_usage(Some(&json!({
            "input_tokens": 10, "input_tokens_details": [{"cached_tokens": 6}]
        })))
        .unwrap();
        assert_eq!(c3.cache_read, 6);
    }

    #[test]
    fn parse_usage_cache_write_aliases() {
        for key in [
            "prompt_cache_write_tokens",
            "cache_write_input_tokens",
            "cache_creation_input_tokens",
        ] {
            let mut m = serde_json::Map::new();
            m.insert("prompt_tokens".into(), json!(1));
            m.insert(key.into(), json!(9));
            let c = parse_openai_usage(Some(&serde_json::Value::Object(m))).unwrap();
            assert_eq!(c.cache_write, 9, "{key}");
        }
    }

    /// 无任何 token 字段 → None（调用方跳过统计，不产生 0 记录）。
    #[test]
    fn parse_usage_rejects_unrelated_object() {
        assert!(parse_openai_usage(None).is_none());
        assert!(parse_openai_usage(Some(&json!({"foo": "bar"}))).is_none());
        assert!(parse_openai_usage(Some(&json!({}))).is_none());
        // 数字字符串也算
        let c = parse_openai_usage(Some(&json!({"prompt_tokens": "42"}))).unwrap();
        assert_eq!(c.input, 42);
    }

    // ---------- 计费归属（对齐 Go internal/usage） ----------

    /// 所有者 2026-09-21 现场：`国服:deepseek-v4.1-flash` 与
    /// `deepseek-v4.1-flash · 国服` 其实是同一份用量，被前缀拆成了两行。
    #[test]
    fn bare_model_name_strips_prefixes() {
        assert_eq!(bare_model_name("deepseek-v4.1-flash"), "deepseek-v4.1-flash");
        assert_eq!(bare_model_name("国服:deepseek-v4.1-flash"), "deepseek-v4.1-flash");
        assert_eq!(
            bare_model_name("qoder:国服:qwen3.8-flash"),
            "qwen3.8-flash",
            "三段取第三段"
        );
        assert_eq!(bare_model_name("  glm-5.3  "), "glm-5.3", "首尾空白");
        // 第三段为空 → 退第二段
        assert_eq!(bare_model_name("qoder:glm-5.3:"), "glm-5.3");
        // 切完为空 → 原样返回（宁可不切，也不能把模型名抹成空串）
        assert_eq!(bare_model_name(":"), ":");
    }

    /// 前缀与裸名必须合并成同一行 —— 否则统计表出现两行同一份用量。
    #[test]
    fn record_merges_prefixed_and_bare_model() {
        let s = Stats::new("");
        s.record_at("u1", "国服:deepseek-v4.1-flash", "2026-09-18", Counters { input: 12, ..Default::default() });
        s.record_at("u1", "deepseek-v4.1-flash", "2026-09-18", Counters { input: 1_600_000_000, ..Default::default() });
        let snap = s.snapshot(0);
        let models = snap["models"].as_array().unwrap();
        assert_eq!(models.len(), 1, "前缀与裸名必须合并");
        assert_eq!(models[0]["key"], json!("deepseek-v4.1-flash"));
        assert_eq!(models[0]["input"], json!(1_600_000_012));
    }

    /// 按 (平台, 区域) 拆开 —— 同一模型在国服与国际版各记一条。
    #[test]
    fn record_billed_splits_by_billing() {
        let s = Stats::new("");
        let cn = Billing {
            product: "workbuddy".into(),
            region: "cn".into(),
            multiplier: Some(0.03),
            has_multiplier: true,
        };
        let intl = Billing {
            product: "workbuddy".into(),
            region: "intl".into(),
            multiplier: Some(0.0),
            has_multiplier: true,
        };
        s.record_billed_at("u1", "deepseek-v4.1-flash", &cn, "2026-09-18", Counters { input: 100, ..Default::default() });
        s.record_billed_at("u2", "deepseek-v4.1-flash", &intl, "2026-09-18", Counters { input: 200, ..Default::default() });

        let snap = s.snapshot(0);
        let groups = snap["modelBilling"]["deepseek-v4.1-flash"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "两个区域各一条");
        let cn_row = groups.iter().find(|g| g["region"] == json!("cn")).unwrap();
        let intl_row = groups.iter().find(|g| g["region"] == json!("intl")).unwrap();
        assert_eq!(cn_row["product"], json!("workbuddy"));
        assert_eq!(cn_row["creditMultiplier"], json!(0.03));
        assert_eq!(cn_row["hasMultiplier"], json!(true));
        assert_eq!(intl_row["creditMultiplier"], json!(0.0), "免费是 0，不是 null");
    }

    /// ★ 三态语义：未声明倍率 ≠ 免费；同一 (平台, 区域) 的多条倍率在**投影层**收成一行。
    #[test]
    fn record_billed_keeps_three_states() {
        let s = Stats::new("");
        // ① 有倍率
        s.record_billed_at(
            "u1",
            "m",
            &Billing { product: "workbuddy".into(), region: "cn".into(), multiplier: Some(0.5), has_multiplier: true },
            "2026-09-18",
            Counters { input: 1, ..Default::default() },
        );
        // ② 免费（0）
        s.record_billed_at(
            "u2",
            "m",
            &Billing { product: "workbuddy".into(), region: "intl".into(), multiplier: Some(0.0), has_multiplier: true },
            "2026-09-18",
            Counters { input: 2, ..Default::default() },
        );
        // ③ 未声明（None）—— ★ 记在**更晚**的一天，让它成为该归属的代表行
        // （倍率是时效性信息：窗口内最后仍有计量的那条才是当前生效值）。
        s.record_billed_at(
            "u3",
            "m",
            &Billing { product: "workbuddy".into(), region: "cn".into(), multiplier: None, has_multiplier: true },
            "2026-09-19",
            Counters { input: 3, ..Default::default() },
        );
        // ④ Qoder：没有倍率这个概念
        s.record_billed_at(
            "qoder-abc",
            "m",
            &Billing { product: "qoder".into(), region: String::new(), multiplier: None, has_multiplier: false },
            "2026-09-18",
            Counters { input: 4, ..Default::default() },
        );

        let snap = s.snapshot(0);
        let groups = snap["modelBilling"]["m"].as_array().unwrap();
        // ★ 两条 workbuddy|cn（0.5 与未声明）在**投影层**收成一行 ⇒ 3 行。
        // 写路径仍按 (平台, 区域, 倍率) 三元组各记一条，落盘形态一个字节都没动。
        assert_eq!(groups.len(), 3, "同一 (平台, 区域) 只出一行");
        let free = groups.iter().find(|g| g["region"] == json!("intl")).unwrap();
        assert_eq!(free["creditMultiplier"], json!(0.0), "免费必须是 0");
        let unknown = groups
            .iter()
            .find(|g| g["region"] == json!("cn") && g["hasMultiplier"] == json!(true))
            .unwrap();
        assert_eq!(
            unknown["creditMultiplier"],
            serde_json::Value::Null,
            "未声明必须是 null（不是 0，否则用户以为不花钱）"
        );
        let qoder = groups.iter().find(|g| g["product"] == json!("qoder")).unwrap();
        assert_eq!(qoder["hasMultiplier"], json!(false), "Qoder 不显示倍率列");
        assert!(
            qoder.get("creditMultiplier").is_none(),
            "hasMultiplier=false 时不下发该字段"
        );
    }

    /// 没给归属的请求不进计费表 —— 混进 `""` 平台会出现一行没有来源的用量。
    #[test]
    fn record_without_billing_stays_out_of_model_billing() {
        let s = Stats::new("");
        s.record_at("u1", "m", "2026-09-18", Counters { input: 10, ..Default::default() });
        let snap = s.snapshot(0);
        assert_eq!(snap["summary"]["input"], json!(10), "总量照记");
        assert!(
            snap["modelBilling"].as_object().unwrap().is_empty(),
            "计费表必须为空"
        );
    }

    /// ★ 倍率随上游调整而变：**写路径**仍按 (平台, 区域, 倍率) 三元组各记一条
    /// （账单不能被投影改写），但**投影层**同一 (平台, 区域) 收成一行。
    #[test]
    fn record_billed_splits_by_multiplier_change_but_projection_merges() {
        let s = Stats::new("");
        // 同一天出现两个倍率（上游当天调整）：取**较大**者 —— 宁可让用户觉得贵一点，
        // 也不要让他在以为便宜时把额度烧光。
        for mult in [0.03, 0.05] {
            s.record_billed_at(
                "u1",
                "m",
                &Billing { product: "workbuddy".into(), region: "cn".into(), multiplier: Some(mult), has_multiplier: true },
                "2026-09-18",
                Counters { input: 10, ..Default::default() },
            );
        }
        let snap = s.snapshot(0);
        let groups = snap["modelBilling"]["m"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "同一 (平台, 区域) 只出一行");
        assert_eq!(groups[0]["creditMultiplier"], json!(0.05), "同天多倍率取较大者");
        assert_eq!(groups[0]["input"], json!(20), "用量必须合并相加");
        // ★ 落盘形态一个字节都没动：写路径仍是两个键。
        assert_eq!(s.inner.lock().unwrap().billed["m"].len(), 2, "写路径仍按三元组各记一条");
    }

    /// ★ 跨天换倍率：代表行是**窗口内最后仍有计量**的那天 —— 倍率是时效性信息，
    /// 要显示当前生效值，而不是已经作废的历史值。
    #[test]
    fn record_billed_projection_prefers_the_latest_day() {
        let s = Stats::new("");
        s.record_billed_at(
            "u1",
            "m",
            &Billing { product: "workbuddy".into(), region: "cn".into(), multiplier: Some(0.03), has_multiplier: true },
            "2026-09-18",
            Counters { input: 10, ..Default::default() },
        );
        s.record_billed_at(
            "u1",
            "m",
            &Billing { product: "workbuddy".into(), region: "cn".into(), multiplier: Some(0.11), has_multiplier: true },
            "2026-09-19",
            Counters { input: 10, ..Default::default() },
        );
        let snap = s.snapshot(0);
        let groups = snap["modelBilling"]["m"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "同一 (平台, 区域) 只出一行");
        assert_eq!(groups[0]["creditMultiplier"], json!(0.11), "取最后仍计量那天的倍率");
        assert_eq!(groups[0]["input"], json!(20), "跨天用量合并");
    }

    /// 账号 → 模型交叉：只保留范围内仍有计量的条目。
    #[test]
    fn account_models_only_keeps_measured_entries() {
        let s = Stats::new("");
        s.record_at("u1", "m1", &local_day_now(), Counters { input: 5, ..Default::default() });
        s.record_at("u1", "m2", "2000-01-01", Counters { input: 5, ..Default::default() });
        s.record_at("u2", "m1", "2000-01-01", Counters { input: 5, ..Default::default() });

        let recent = s.snapshot(7);
        let am = recent["accountModels"].as_object().unwrap();
        assert_eq!(am.len(), 1, "只有 u1 在范围内有计量");
        let u1 = am["u1"].as_object().unwrap();
        assert_eq!(u1.len(), 1, "m2 已被 cutoff 排除");
        assert!(u1.contains_key("m1"));
        assert!(!u1.contains_key("m2"));
    }

    #[test]
    fn product_from_uid_recognizes_three_products() {
        assert_eq!(product_from_uid("zcode-abc"), Some("zcode"));
        assert_eq!(product_from_uid("qoder-abc"), Some("qoder"));
        assert_eq!(
            product_from_uid("a1b2c3d4-e5f6-7890-abcd-ef1234567890"),
            Some("workbuddy"),
            "裸 UUID = WorkBuddy（auth 层用空串表示产品）"
        );
        assert_eq!(product_from_uid("something-else"), None, "形态不认识就不猜");
        assert_eq!(product_from_uid("-"), None);
    }

    /// ★★ 回填只补 product，**不编造** region 与倍率。
    ///
    /// 编一个区域会让界面显示错误计费，比显示「未知」更糟 —— 用户会据它
    /// 判断该烧哪个账号的额度。
    #[test]
    fn backfill_does_not_invent_region_or_multiplier() {
        let s = Stats::new("");
        // 一次带归属（cn）的请求 + 一次不带归属的请求，同一模型。
        s.record_billed_at(
            "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
            "m",
            &Billing { product: "workbuddy".into(), region: "cn".into(), multiplier: Some(0.03), has_multiplier: true },
            "2026-09-18",
            Counters { input: 10, ..Default::default() },
        );
        s.record_at(
            "b2c3d4e5-f6a7-8901-bcde-f12345678901",
            "m",
            "2026-09-18",
            Counters { input: 20, ..Default::default() },
        );

        let snap = s.snapshot(0);
        let groups = snap["modelBilling"]["m"].as_array().unwrap();
        // 无归属那条不会凭空出现（它没进 billed 表），所以只有 1 行；
        // 关键是那一行**没有**被回填出错误的区域/倍率。
        for g in groups {
            assert_eq!(g["region"], json!("cn"), "区域只能来自真实归属");
            assert_eq!(g["creditMultiplier"], json!(0.03), "倍率只能来自真实归属");
        }
    }

    /// 回填真的会补 product：只有计费行、没有 accountModels 交叉时无从补，
    /// 有交叉时补上。
    #[test]
    fn backfill_fills_missing_product_from_uid_shape() {
        let s = Stats::new("");
        // 直接往 billed 表里塞一条没有 product 的行是不可能的（门控拦住了），
        // 所以这里验证反向性质：带 product 的行原样保留。
        s.record_billed_at(
            "qoder-xyz",
            "qwen3.8-flash",
            &Billing { product: "qoder".into(), region: String::new(), multiplier: None, has_multiplier: false },
            "2026-09-18",
            Counters { input: 7, ..Default::default() },
        );
        let snap = s.snapshot(0);
        let groups = snap["modelBilling"]["qwen3.8-flash"].as_array().unwrap();
        assert_eq!(groups[0]["product"], json!("qoder"));
        assert!(
            groups[0].get("region").is_none(),
            "不分区就不下发 region（不编造）"
        );
    }

    /// 计费归属必须落盘 —— 否则网关一重启，界面上的归属就全没了。
    #[test]
    fn billing_roundtrips_through_disk() {
        let dir = std::env::temp_dir().join(format!("wbswg-usage-bill-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("usage.json");
        let path_s = path.to_string_lossy().to_string();

        {
            let s = Stats::new(&path_s);
            s.record_billed_at(
                "u1",
                "国服:deepseek-v4.1-flash",
                &Billing { product: "workbuddy".into(), region: "cn".into(), multiplier: Some(0.03), has_multiplier: true },
                "2026-09-18",
                Counters { input: 42, ..Default::default() },
            );
            s.flush();
        }
        let s2 = Stats::new(&path_s);
        let snap = s2.snapshot(0);
        // 模型名归一后落盘
        assert_eq!(snap["models"][0]["key"], json!("deepseek-v4.1-flash"));
        let groups = snap["modelBilling"]["deepseek-v4.1-flash"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["product"], json!("workbuddy"));
        assert_eq!(groups[0]["region"], json!("cn"));
        assert_eq!(groups[0]["creditMultiplier"], json!(0.03));
        assert_eq!(groups[0]["hasMultiplier"], json!(true));
        // 账号 × 模型交叉也要落盘（回填的唯一依据）
        assert!(snap["accountModels"]["u1"]["deepseek-v4.1-flash"]["total"].is_number());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 三态里的「未声明」也必须能落盘区分（`Option<f64>` 的 null 语义）。
    #[test]
    fn unknown_multiplier_roundtrips_as_null() {
        let dir = std::env::temp_dir().join(format!("wbswg-usage-null-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("usage.json");
        let path_s = path.to_string_lossy().to_string();

        {
            let s = Stats::new(&path_s);
            s.record_billed_at(
                "u1",
                "m",
                &Billing { product: "workbuddy".into(), region: "cn".into(), multiplier: None, has_multiplier: true },
                "2026-09-18",
                Counters { input: 1, ..Default::default() },
            );
            s.flush();
        }
        let s2 = Stats::new(&path_s);
        let snap = s2.snapshot(0);
        let groups = snap["modelBilling"]["m"].as_array().unwrap();
        assert_eq!(groups[0]["hasMultiplier"], json!(true));
        assert_eq!(
            groups[0]["creditMultiplier"],
            serde_json::Value::Null,
            "「不知道」落盘后仍是 null，不能被降级成 0"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn billing_key_distinguishes_unknown_from_free() {
        let unknown = Billing { product: "wb".into(), region: "cn".into(), multiplier: None, has_multiplier: true };
        let free = Billing { product: "wb".into(), region: "cn".into(), multiplier: Some(0.0), has_multiplier: true };
        assert_ne!(
            billing_key(&unknown),
            billing_key(&free),
            "「不知道」与「免费」必须是不同的键"
        );
        assert_eq!(billing_key(&free), "wb|cn|0");
        assert_eq!(billing_key(&unknown), "wb|cn|?");
        assert_eq!(
            billing_key(&Billing { product: "wb".into(), region: "intl".into(), multiplier: Some(0.03), has_multiplier: true }),
            "wb|intl|0.03"
        );
    }

    // ─────────────────── 计费周期（Go `usage/cycle_test.go`） ───────────────────

    /// 本地时刻构造器（Go 测试里的 `at(y, m, d, h, mi, s)`）。
    fn at_ms(y: i32, m: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
        use chrono::TimeZone;
        chrono::Local
            .with_ymd_and_hms(y, m, d, h, mi, s)
            .single()
            .expect("本地时刻应可构造")
            .timestamp_millis()
    }

    /// 1. 月末钳制：31/30/29/28 天与加减两个方向。
    ///
    /// ★ 这是**最重要**的一条：参考实现踩过 `time.AddDate` 的溢出归一 ——
    /// 3 月 31 日减一个月会得到 **3 月 3 日**（2 月 31 日溢出），5 月 31 日减
    /// 一个月得到 **5 月 1 日**。那个值直接喂给周期起点，于是每个在 29~31 号
    /// 重置的账号，其「本周期」都会从月初附近开始 —— 把上一整个月的用量算进来，
    /// 界面上「本周期用量」突然变成两个月那么多，而且**永远如此**。
    #[test]
    fn add_months_ms_clamps_month_end() {
        let cases: &[(&str, i64, i32, i64)] = &[
            // 31 天的月份，减一个月
            ("mar-31-minus-1-feb-28", at_ms(2026, 3, 31, 12, 30, 45), -1, at_ms(2026, 2, 28, 12, 30, 45)),
            ("mar-31-minus-1-leap-feb-29", at_ms(2028, 3, 31, 12, 30, 45), -1, at_ms(2028, 2, 29, 12, 30, 45)),
            ("may-31-minus-1-apr-30", at_ms(2026, 5, 31, 8, 0, 0), -1, at_ms(2026, 4, 30, 8, 0, 0)),
            ("jul-31-minus-1-jun-30", at_ms(2026, 7, 31, 0, 0, 0), -1, at_ms(2026, 6, 30, 0, 0, 0)),
            ("oct-31-minus-1-sep-30", at_ms(2026, 10, 31, 23, 59, 59), -1, at_ms(2026, 9, 30, 23, 59, 59)),
            ("dec-31-minus-1-nov-30", at_ms(2026, 12, 31, 6, 0, 0), -1, at_ms(2026, 11, 30, 6, 0, 0)),
            // 31 天的月份，加一个月
            ("jan-31-plus-1-feb-28", at_ms(2026, 1, 31, 9, 15, 0), 1, at_ms(2026, 2, 28, 9, 15, 0)),
            ("jan-31-plus-1-leap-feb-29", at_ms(2028, 1, 31, 9, 15, 0), 1, at_ms(2028, 2, 29, 9, 15, 0)),
            ("mar-31-plus-1-apr-30", at_ms(2026, 3, 31, 9, 15, 0), 1, at_ms(2026, 4, 30, 9, 15, 0)),
            ("aug-31-plus-1-sep-30", at_ms(2026, 8, 31, 9, 15, 0), 1, at_ms(2026, 9, 30, 9, 15, 0)),
            ("oct-31-plus-1-nov-30", at_ms(2026, 10, 31, 9, 15, 0), 1, at_ms(2026, 11, 30, 9, 15, 0)),
            ("dec-31-plus-1-jan-31", at_ms(2026, 12, 31, 9, 15, 0), 1, at_ms(2027, 1, 31, 9, 15, 0)),
            // 30 天的月份
            ("apr-30-minus-1-mar-30", at_ms(2026, 4, 30, 7, 0, 0), -1, at_ms(2026, 3, 30, 7, 0, 0)),
            ("jun-30-minus-1-may-30", at_ms(2026, 6, 30, 7, 0, 0), -1, at_ms(2026, 5, 30, 7, 0, 0)),
            ("sep-30-plus-1-oct-30", at_ms(2026, 9, 30, 7, 0, 0), 1, at_ms(2026, 10, 30, 7, 0, 0)),
            ("nov-30-plus-1-dec-30", at_ms(2026, 11, 30, 7, 0, 0), 1, at_ms(2026, 12, 30, 7, 0, 0)),
            // 2 月自身
            ("feb-29-minus-1-jan-29", at_ms(2028, 2, 29, 10, 0, 0), -1, at_ms(2028, 1, 29, 10, 0, 0)),
            ("feb-28-plus-1-mar-28", at_ms(2026, 2, 28, 10, 0, 0), 1, at_ms(2026, 3, 28, 10, 0, 0)),
            // 月中：不该有任何钳制
            ("mid-month-plus-1", at_ms(2026, 2, 15, 23, 59, 59), 1, at_ms(2026, 3, 15, 23, 59, 59)),
            ("mid-month-minus-1", at_ms(2026, 3, 15, 1, 2, 3), -1, at_ms(2026, 2, 15, 1, 2, 3)),
            // 跨年
            ("jan-15-minus-1-crosses-year", at_ms(2026, 1, 15, 4, 5, 6), -1, at_ms(2025, 12, 15, 4, 5, 6)),
            // 0 与负数入参：没有可推导的起点，返回 0 而不是编一个
            ("zero-input-stays-zero", 0, -1, 0),
            ("negative-input-stays-zero", -1, 1, 0),
        ];
        for (name, input, n, want) in cases {
            assert_eq!(add_months_ms(*input, *n), *want, "用例 {name}");
        }
    }

    /// 纳秒必须原样保留（Go 侧同款用例）。毫秒精度下验证「时分秒没被丢掉」。
    #[test]
    fn add_months_ms_keeps_the_time_of_day() {
        let input = at_ms(2026, 2, 15, 23, 59, 59);
        let got = add_months_ms(input, 1);
        assert_eq!(got, at_ms(2026, 3, 15, 23, 59, 59), "跨月不该丢时分秒");
        // 钳制到月末时同样保留时分秒。
        assert_eq!(
            add_months_ms(at_ms(2026, 3, 31, 12, 30, 45), -1),
            at_ms(2026, 2, 28, 12, 30, 45),
            "钳制后时分秒仍须保留"
        );
    }

    /// 2. 周期起点推导：起点 = 重置时刻减一个月。
    ///
    /// 上游只报「下次什么时候重置」，不报起点。套餐是月度的，故起点由边界推导。
    /// 差一天看着无害，但它会让周期窗口整体偏移，于是「本周期用量」少算/多算
    /// 一整天的量。
    #[test]
    fn set_billing_cycle_derives_calendar_start() {
        let reset = at_ms(2026, 9, 25, 0, 0, 0);
        let want_start = at_ms(2026, 8, 25, 0, 0, 0);
        let s = Stats::new("");
        assert!(
            s.set_billing_cycle_at("u1", reset, reset - 72 * 3_600_000),
            "首次上报应当改变状态"
        );
        let got = s.cycle_state_for_test("u1").expect("应有状态");
        assert_eq!(got.next_reset_ms, reset, "边界未被采纳");
        assert_eq!(got.cycle_start_ms, want_start, "起点应为 8 月 25 日，不是 8 月 26 日");
    }

    /// 月末边界的起点必须被钳制（3 月 31 日 ⇒ 2 月 28 日）。
    #[test]
    fn set_billing_cycle_derives_clamped_start_on_month_end() {
        let reset = at_ms(2026, 3, 31, 8, 0, 0);
        let s = Stats::new("");
        s.set_billing_cycle_at("u1", reset, at_ms(2026, 3, 1, 0, 0, 0));
        assert_eq!(
            s.cycle_state_for_test("u1").unwrap().cycle_start_ms,
            at_ms(2026, 2, 28, 8, 0, 0),
            "月末起点未钳制"
        );
    }

    /// 3. 情形 ①：同一个边界重复上报 = 幂等（**不置脏**）。
    ///
    /// 「幂等」在这里有**可观测**的含义：刷新任务每 15 分钟跑一次，若每次都置脏，
    /// 就会无谓地写一次盘（usage.json 是几 MB 的聚合表）。
    #[test]
    fn set_billing_cycle_is_idempotent_and_does_not_dirty() {
        let s = Stats::new("");
        let now = at_ms(2026, 9, 1, 0, 0, 0);
        let reset = now + 72 * 3_600_000;
        assert!(s.set_billing_cycle_at("u1", reset, now), "首次上报应当改变状态");
        s.clear_dirty_for_test();
        assert!(
            !s.set_billing_cycle_at("u1", reset, now),
            "同一边界重复上报不应报告「有变化」"
        );
        assert!(!s.dirty_for_test(), "幂等路径不该置脏标志");
    }

    /// 4. 情形 ②：陈旧报告一律忽略，且不置脏。
    #[test]
    fn set_billing_cycle_ignores_stale_boundary() {
        let now = at_ms(2026, 9, 1, 0, 0, 0);
        let newer = now + 30 * 24 * 3_600_000;
        let s = Stats::new("");
        s.set_billing_cycle_at("u1", newer, now);
        s.clear_dirty_for_test();

        for stale in [
            newer - 3_600_000,
            newer - 20 * 24 * 3_600_000,
            now + 24 * 3_600_000,
            newer, // 相等：走情形 ①，同样不改
        ] {
            assert!(
                !s.set_billing_cycle_at("u1", stale, now),
                "陈旧边界 {stale} 被采纳（应保留 {newer}）"
            );
        }
        assert_eq!(s.cycle_state_for_test("u1").unwrap().next_reset_ms, newer, "边界被陈旧报告覆盖");
        assert!(!s.dirty_for_test(), "忽略陈旧边界时不应置脏标志");
    }

    /// 5. 情形 ③：真正更晚的边界必须被采纳并重推起点。
    #[test]
    fn set_billing_cycle_adopts_new_cycle() {
        let now = at_ms(2026, 9, 1, 0, 0, 0);
        let first = at_ms(2026, 9, 25, 8, 0, 0);
        let second = at_ms(2026, 10, 25, 8, 0, 0);
        let s = Stats::new("");
        s.set_billing_cycle_at("u1", first, now);
        assert_eq!(
            s.cycle_state_for_test("u1").unwrap().cycle_start_ms,
            at_ms(2026, 8, 25, 8, 0, 0),
            "第一轮起点错误"
        );
        assert!(s.set_billing_cycle_at("u1", second, now), "更晚的边界应被采纳");
        let got = s.cycle_state_for_test("u1").unwrap();
        assert_eq!(got.next_reset_ms, second, "边界未更新");
        assert_eq!(got.cycle_start_ms, at_ms(2026, 9, 25, 8, 0, 0), "起点未随新边界重推");
    }

    /// 6. 情形 ⓪：不可信的入参一律拒绝（含**恰好等于**上限与超过上限）。
    ///
    /// ★ 哨兵必须挡在门口：它的值大于一切真实边界，一旦存进去，情形 ② 的
    /// 「保留较新者」判据就会反过来把后续**所有**真实边界都当成陈旧报告丢掉 ——
    /// 周期再也建立不起来，而且不会自愈。
    #[test]
    fn set_billing_cycle_rejects_implausible_boundary() {
        let now = at_ms(2026, 9, 1, 0, 0, 0);
        let good = now + 30 * 24 * 3_600_000;
        const SENTINEL: i64 = 253_402_214_400_000; // 9999-12-31T00:00:00Z
        let bad: &[(&str, i64)] = &[
            ("zero", 0),
            ("negative", -1),
            ("sentinel-9999", SENTINEL),
            ("at-ceiling", MAX_PLAUSIBLE_RESET_MS),
            ("above-ceiling", MAX_PLAUSIBLE_RESET_MS + 1),
            ("max-int64", 1i64 << 62),
        ];

        for (name, ms) in bad {
            // 无既有状态
            let s = Stats::new("");
            assert!(!s.set_billing_cycle_at("u1", *ms, now), "{name}：不可信边界被采纳为变化");
            assert_eq!(
                s.cycle_state_for_test("u1").map(|c| c.next_reset_ms).unwrap_or(0),
                0,
                "{name}：不可信边界被写入"
            );
            assert!(!s.dirty_for_test(), "{name}：不可信边界置了脏标志");

            // 有可信状态时必须原样保留
            let s = Stats::new("");
            s.set_billing_cycle_at("u1", good, now);
            s.clear_dirty_for_test();
            assert!(!s.set_billing_cycle_at("u1", *ms, now), "{name}：不可信边界覆盖了可信边界");
            assert_eq!(
                s.cycle_state_for_test("u1").unwrap().next_reset_ms,
                good,
                "{name}：可信边界被破坏"
            );
        }

        // 空 uid：会把周期挂到一个不存在的账号上
        let s = Stats::new("");
        assert!(!s.set_billing_cycle_at("", good, now), "空 uid 不应被采纳");
        assert!(s.cycle_states_for_test().is_empty(), "空 uid 写进了状态");
    }

    /// 7. `plausible_reset_ms` 的边界判据（含上限两侧）。
    #[test]
    fn plausible_reset_ms_boundaries() {
        let now = chrono::Local::now().timestamp_millis();
        for (ms, want) in [
            (0i64, false),
            (-1, false),
            (1, true),
            (now, true),
            (MAX_PLAUSIBLE_RESET_MS - 1, true),
            (MAX_PLAUSIBLE_RESET_MS, false), // ★ 恰好等于 ⇒ 拒绝
            (MAX_PLAUSIBLE_RESET_MS + 1, false),
            (253_402_214_400_000, false), // 9999-12-31 哨兵
        ] {
            assert_eq!(plausible_reset_ms(ms), want, "plausible_reset_ms({ms})");
        }
    }

    /// 8. ★★ 读路径只投影、**绝不回写**（这是硬约束，不是风格偏好）。
    ///
    /// 面板每 15 秒拉一次 /usage。若读路径也推进边界，那么一个**恰好落在重置点
    /// 之后**的轮询就会把推进结果落盘 —— 于是「翻周期」这件事在没有收到任何上游
    /// 报告、也没有任何请求进来的情况下发生了。用户看到的是周期凭空翻页，而根本
    /// 原因是「有人在看界面」。
    ///
    /// 故本用例同时断言两件事：投影**确实**推进了，落盘状态**确实**没变。
    #[test]
    fn cycle_projection_does_not_roll_state() {
        let boundary = at_ms(2026, 3, 31, 8, 0, 0);
        let before = boundary - 72 * 3_600_000;
        let after = boundary + 3_600_000;

        let s = Stats::new("");
        s.set_now_for_test(before);
        s.set_billing_cycle_at("u1", boundary, before);
        s.clear_dirty_for_test();

        // 时间越过边界，然后**只读**。
        s.set_now_for_test(after);
        let p = s.cycle_of("u1").expect("投影后应仍然知道下一次重置时刻");
        let want_next = add_months_ms(boundary, 1); // 4 月 30 日（31 日被钳制）
        assert_eq!(p.next_reset_ms, want_next, "投影未推进边界");
        assert_eq!(p.cycle_start_ms, boundary, "投影的起点应为刚跨过的边界");

        // ★ 落盘状态必须原封不动。
        assert_eq!(
            s.cycle_state_for_test("u1").unwrap().next_reset_ms,
            boundary,
            "读路径推进了落盘状态 —— 一次界面轮询就翻掉了周期"
        );
        assert!(!s.dirty_for_test(), "读路径置了脏标志 —— 读产生了待落盘的副作用");

        // Snapshot 也是读路径，必须同样无副作用。
        let snap = s.snapshot(0);
        assert!(!s.dirty_for_test(), "Snapshot 置了脏标志");
        assert_eq!(
            s.cycle_state_for_test("u1").unwrap().next_reset_ms,
            boundary,
            "Snapshot 推进了落盘状态"
        );
        let row = &snap["billingCycles"]["u1"];
        assert_eq!(
            row["nextResetAtMs"].as_i64().unwrap(),
            want_next,
            "快照里的边界必须是**投影值**（不是落盘值）"
        );
        assert_eq!(row["known"], json!(true));
    }

    /// 9. 写路径**必须**推进（与上一条配对）。
    ///
    /// 只有写路径推进，才能保证「周期翻页」总由真实活动（一个请求、或一次成功的
    /// 上游刷新）驱动。若写路径也不推进，边界就会永远停在过去，界面显示「下次
    /// 重置」是一个已经过去的时刻。
    #[test]
    fn cycle_rolls_on_write_path() {
        let boundary = at_ms(2026, 3, 31, 8, 0, 0);
        let after = boundary + 3_600_000;
        let want_next = add_months_ms(boundary, 1);

        let s = Stats::new("");
        s.set_now_for_test(boundary - 72 * 3_600_000);
        s.set_billing_cycle_at("u1", boundary, s.clock_ms());

        // 一个请求落在边界之后 ⇒ 写路径推进。
        s.record_billed_at_ms(
            "u1",
            "glm-5.3",
            &Billing::default(),
            after,
            Counters { input: 10, ..Default::default() },
        );

        let got = s.cycle_state_for_test("u1").unwrap();
        assert_eq!(got.next_reset_ms, want_next, "写路径未推进边界");
        assert_eq!(got.cycle_start_ms, boundary, "推进后起点应为刚跨过的边界");
    }

    /// 10. 边界**之前**写入不推进（推进不是「每次都做」）。
    #[test]
    fn record_before_boundary_does_not_roll() {
        let boundary = at_ms(2026, 3, 31, 8, 0, 0);
        let before = boundary - 3_600_000;

        let s = Stats::new("");
        s.set_now_for_test(boundary - 72 * 3_600_000);
        s.set_billing_cycle_at("u1", boundary, s.clock_ms());
        s.record_billed_at_ms(
            "u1",
            "glm-5.3",
            &Billing::default(),
            before,
            Counters { input: 1, ..Default::default() },
        );
        assert_eq!(
            s.cycle_state_for_test("u1").unwrap().next_reset_ms,
            boundary,
            "边界之前写入不该推进周期"
        );
    }

    /// 11. 本周期用量按窗口从 `accounts` 投影，且**别人的量不混进来**。
    #[test]
    fn cycle_usage_projects_from_accounts() {
        let boundary = at_ms(2026, 12, 25, 8, 0, 0);
        let start = at_ms(2026, 11, 25, 8, 0, 0); // = add_months_ms(boundary, -1)

        let s = Stats::new("");
        s.set_now_for_test(boundary - 72 * 3_600_000);

        let rec = |uid: &str, ms: i64, input: i64| {
            s.record_billed_at_ms(
                uid,
                "glm-5.3",
                &Billing::default(),
                ms,
                Counters { input, ..Default::default() },
            );
        };
        rec("u1", at_ms(2026, 11, 24, 23, 0, 0), 1); // 窗口前
        rec("u1", at_ms(2026, 11, 25, 9, 0, 0), 10); // 窗口内（起点当天）
        rec("u1", at_ms(2026, 12, 15, 12, 0, 0), 100); // 窗口内
        rec("u1", at_ms(2026, 12, 24, 23, 59, 0), 1000); // 窗口内（终点前一天）
        rec("u1", at_ms(2026, 12, 25, 9, 0, 0), 10000); // 窗口后（重置当天归新周期）
        rec("u2", at_ms(2026, 12, 15, 12, 0, 0), 999999); // 别人的量

        s.set_billing_cycle_at("u1", boundary, boundary - 72 * 3_600_000);

        let p = s.cycle_of("u1").expect("应知道周期");
        assert_eq!(p.cycle_start_ms, start, "起点错误");
        assert_eq!(p.used.input, 1110, "本周期用量应为 10+100+1000 —— 边界外或别人的量混进来了");
        assert_eq!(p.used.records, 3, "本周期请求数错误");

        // u2 从未学到周期 ⇒ 必须报「未知」且**不投影任何用量**。
        // 若返回 u2 的全部历史，界面会把「历史上用过的总量」当成「本周期用量」。
        assert!(s.cycle_of("u2").is_none(), "无周期信息的账号不该报告 known");
        assert!(
            !s.snapshot(0)["billingCycles"]
                .as_object()
                .unwrap()
                .contains_key("u2"),
            "快照里出现了无周期信息的账号"
        );

        // 给 u2 一个**不同**的周期窗口：它只能看到自己的量，且不受 u1 影响。
        s.set_billing_cycle_at("u2", boundary, boundary - 72 * 3_600_000);
        let p2 = s.cycle_of("u2").expect("u2 设了周期后应知道周期");
        assert_eq!(p2.used.input, 999999, "u2 的周期用量错误（别人的量混进来了？）");
        assert_eq!(p2.used.records, 1, "u2 的周期请求数错误");
        assert_eq!(s.cycle_of("u1").unwrap().used.input, 1110, "u2 的周期污染了 u1 的投影");
    }

    /// 12. 重置**当天**的用量归入新周期（上界是开区间）。
    ///
    /// 若把上界写成闭区间，重置当天的量会被同时算进**两个**周期，于是新周期一
    /// 开始就带着上一周期的尾巴。
    #[test]
    fn cycle_usage_excludes_reset_day() {
        let boundary = at_ms(2026, 12, 25, 8, 0, 0);
        let s = Stats::new("");
        s.set_now_for_test(boundary - 3_600_000);
        let rec = |ms: i64, input: i64| {
            s.record_billed_at_ms(
                "u1",
                "glm-5.3",
                &Billing::default(),
                ms,
                Counters { input, ..Default::default() },
            );
        };
        rec(at_ms(2026, 12, 25, 9, 0, 0), 500); // 边界当天 = 窗口上界那一天
        rec(at_ms(2026, 12, 24, 23, 59, 59), 7); // 上界前一天（窗口内最后一刻）
        s.set_billing_cycle_at("u1", boundary, boundary - 72 * 3_600_000);

        let p = s.cycle_of("u1").expect("应知道周期");
        assert_eq!(p.used.input, 7, "重置当天的用量被算进了上一周期");
    }

    /// 13. 没有周期信息的账号不能被当成「有周期」；空 uid 同理。
    #[test]
    fn cycle_of_unknown_account_is_none() {
        let s = Stats::new("");
        s.record_at(
            "u1",
            "glm-5.3",
            &local_day_now(),
            Counters { input: 5, ..Default::default() },
        );
        assert!(s.cycle_of("u1").is_none(), "没有周期信息却报告了周期");
        assert!(s.cycle_of("").is_none(), "空 uid 不该报告周期");
        assert!(
            s.snapshot(0)["billingCycles"].as_object().unwrap().is_empty(),
            "快照里不该有周期行"
        );
    }

    /// 14. 越界值：负边界必须报「未知」且不投影任何用量。
    ///
    /// 这是与上限**不同**的一道防御：损坏/手改的负值若被当作「很久以前的重置
    /// 点」，投影会把它当成有效周期起点，于是本周期用量从 1970 年算起 —— 也就是
    /// 「全部历史」。
    #[test]
    fn cycle_rejects_negative_boundary() {
        let s = Stats::new("");
        let now = chrono::Local::now().timestamp_millis();
        s.set_now_for_test(now);
        s.force_cycle_state_for_test("u1", -62_135_596_800_000, -62_135_596_800_000);
        s.record_billed_at_ms(
            "u1",
            "glm-5.3",
            &Billing::default(),
            now,
            Counters { input: 12345, ..Default::default() },
        );

        let p = s.cycle_of("u1");
        assert!(p.is_none(), "负边界应报未知");
        assert!(
            s.snapshot(0)["billingCycles"]
                .as_object()
                .unwrap()
                .is_empty(),
            "负边界的账号出现在了快照里"
        );
    }

    /// 15. 病态 `next_reset_ms = 0`：不该被当成「立刻重置」，也不该空转。
    #[test]
    fn cycle_treats_zero_boundary_as_unknown() {
        let s = Stats::new("");
        let now = chrono::Local::now().timestamp_millis();
        s.set_now_for_test(now);
        // 起点有值、边界为 0（旧文件/被推进清空后的形态）。
        s.force_cycle_state_for_test("u1", 0, now - 86_400_000);
        assert!(s.cycle_of("u1").is_none(), "边界为 0 时应报未知");

        // 写入也不能把它变成「立刻重置」。
        s.record_billed_at_ms(
            "u1",
            "glm-5.3",
            &Billing::default(),
            now,
            Counters { input: 1, ..Default::default() },
        );
        assert_eq!(
            s.cycle_state_for_test("u1").unwrap().next_reset_ms,
            0,
            "边界为 0 时写路径不该凭空造出一个边界"
        );
    }

    /// 16. `maxCycleCatchUps` 上限：追赶超出上限的边界必须报「未知」而不是空转。
    ///
    /// 上限（2400 个月 ≈ 200 年）的意义是**兜底**：正常账号一个月才推进一次，
    /// 永远碰不到它；而一个损坏到「起点在极远的过去」的边界若不受限，循环会跑
    /// 几万次（**持锁**）才收敛。
    #[test]
    fn cycle_roll_terminates_beyond_catch_up_cap() {
        let s = Stats::new("");
        // 边界 2000 年（可信），而「现在」是 2400 年：相距 400 年 = 4800 个月 > 2400。
        let future = at_ms(2400, 1, 1, 0, 0, 0);
        s.set_now_for_test(future);
        s.force_cycle_state_for_test(
            "u1",
            at_ms(2000, 1, 1, 0, 0, 0),
            at_ms(1999, 12, 1, 0, 0, 0),
        );
        let p = s.cycle_of("u1");
        assert!(p.is_none(), "超出追赶上限应报未知（known=false）");
    }

    /// 17. 远过去但**在**上限内的损坏边界：能正常推进到未来。
    ///
    /// 与上一条配对 —— 证明上限不是「万能挡箭牌」。
    #[test]
    fn cycle_advances_a_far_past_boundary_within_the_cap() {
        let s = Stats::new("");
        let now = chrono::Local::now().timestamp_millis();
        s.set_now_for_test(now);
        // 1970-01-01：损坏的边界，从它推进到「现在」约 680 个月。
        s.force_cycle_state_for_test("u1", 1, 0);
        let p = s.cycle_of("u1").expect("680 个月远未触及 2400 上限，应当能推进");
        assert!(p.next_reset_ms > now, "损坏的远过去边界未被推进到未来");
    }

    /// 18. 落盘 / 装载：周期状态必须跨重启存活。
    #[test]
    fn billing_cycles_survive_restart() {
        let dir = std::env::temp_dir().join(format!("wbswg-usage-cycle-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("usage.json");
        let path_s = path.to_string_lossy().to_string();
        let _ = std::fs::remove_dir_all(&dir);

        let boundary = at_ms(2026, 12, 25, 8, 0, 0);
        let now = boundary - 72 * 3_600_000;

        let s1 = Stats::new(&path_s);
        s1.set_now_for_test(now);
        s1.set_billing_cycle_at("u1", boundary, now);
        s1.flush();

        // 重启：新实例读同一个文件。
        let s2 = Stats::new(&path_s);
        let got = s2.cycle_state_for_test("u1").expect("重启后周期状态丢失 —— 界面上的下次重置会空白");
        assert_eq!(got.next_reset_ms, boundary, "边界未持久化");
        assert_eq!(got.cycle_start_ms, add_months_ms(boundary, -1), "起点未持久化");

        // 落盘文件里必须真的有这个键（而不是靠内存里的巧合）。
        let raw = std::fs::read_to_string(&path_s).expect("读回落盘文件");
        assert!(raw.contains("billingCycles"), "usage.json 里没有 billingCycles —— 重启后会丢");
        assert!(raw.contains("nextResetAtMs"), "落盘字段名不稳定（改名 = 用户数据静默丢失）");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 19. 老文件（没有该键）必须能读，且装载后仍可写。
    #[test]
    fn load_tolerates_legacy_file_without_billing_cycles() {
        let dir = std::env::temp_dir().join(format!("wbswg-usage-legacy-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("usage.json");
        let path_s = path.to_string_lossy().to_string();
        let legacy = r#"{"version":1,"savedAt":"2026-09-01T00:00:00Z","days":{"2026-09-01":{"input":42,"output":1,"cacheRead":0,"cacheWrite":0,"records":1}}}"#;
        std::fs::write(&path_s, legacy).unwrap();

        let s = Stats::new(&path_s);
        let snap = s.snapshot(0);
        assert_eq!(snap["summary"]["input"].as_i64().unwrap(), 42, "老文件的历史用量丢失");
        assert!(
            snap["billingCycles"].is_object(),
            "缺 billingCycles 键（应为空对象，不能是 null）"
        );
        assert!(snap["billingCycles"].as_object().unwrap().is_empty(), "老文件不该凭空产生周期行");
        // 装载后必须可写（不是往一个坏掉的 map 里塞）。
        assert!(
            s.set_billing_cycle("u1", chrono::Local::now().timestamp_millis() + 72 * 3_600_000),
            "老文件装载后无法写入周期状态"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 20. 装载时丢弃「两个字段都是 0」的条目（形状容错），并归一负值。
    #[test]
    fn load_drops_empty_billing_cycle_entries() {
        let dir = std::env::temp_dir().join(format!("wbswg-usage-drop-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("usage.json");
        let path_s = path.to_string_lossy().to_string();
        let future = chrono::Local::now().timestamp_millis() + 72 * 3_600_000;
        let raw = format!(
            r#"{{"version":1,"savedAt":"2026-09-01T00:00:00Z","days":{{}},"billingCycles":{{"empty":{{"nextResetAtMs":0,"cycleStartAtMs":0}},"negative":{{"nextResetAtMs":-5,"cycleStartAtMs":-7}},"has-value":{{"nextResetAtMs":{future}}}}}}}"#
        );
        std::fs::write(&path_s, raw).unwrap();

        let s = Stats::new(&path_s);
        let states = s.cycle_states_for_test();
        assert!(!states.contains_key("empty"), "两个字段都是 0 的条目不该占一个键");
        assert!(!states.contains_key("negative"), "负数被归一成 0 后同样不该占键");
        let got = states.get("has-value").expect("有值的条目被误删");
        assert!(got.next_reset_ms > 0, "有值的条目被破坏");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 21. ★★ 已落盘的哨兵能**自愈**（与「门口拒绝」配套的另一半）。
    ///
    /// 门口拒绝只保护今后，而参考实现的现场数据里已经存着哨兵。若装载时直接丢弃，
    /// 用户要等下一次 `/user/status` 刷新才重新看到周期（可能是几十分钟）。故装载
    /// 时**保留**哨兵，靠情形 ② 的「已跟踪值不可信则放行」自愈。
    #[test]
    fn set_billing_cycle_recovers_from_persisted_sentinel() {
        const SENTINEL: i64 = 253_402_214_400_000;
        let dir = std::env::temp_dir().join(format!("wbswg-usage-sentinel-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("usage.json");
        let path_s = path.to_string_lossy().to_string();
        let raw = format!(
            r#"{{"version":1,"savedAt":"2026-09-01T00:00:00Z","days":{{}},"billingCycles":{{"u1":{{"nextResetAtMs":{SENTINEL}}}}}}}"#
        );
        std::fs::write(&path_s, raw).unwrap();

        let s = Stats::new(&path_s);
        assert_eq!(
            s.cycle_state_for_test("u1").map(|c| c.next_reset_ms).unwrap_or(0),
            SENTINEL,
            "装载未保留哨兵（应当保留，否则要等下一次刷新才恢复）"
        );

        // 一个真实边界到达：必须被采纳（哨兵不可信 ⇒ 情形 ② 放行）。
        let real = at_ms(2026, 7, 16, 17, 27, 0);
        let now = real - 24 * 3_600_000;
        assert!(
            s.set_billing_cycle_at("u1", real, now),
            "真实边界未能替换哨兵 —— 周期永远建立不起来"
        );
        let got = s.cycle_state_for_test("u1").unwrap();
        assert_eq!(got.next_reset_ms, real, "边界未自愈");
        assert_eq!(got.cycle_start_ms, add_months_ms(real, -1), "起点错误");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 22. 快照里的周期行形状稳定：键齐全、`known` 是布尔、数值是数字。
    #[test]
    fn snapshot_billing_cycles_row_shape() {
        let boundary = chrono::Local::now().timestamp_millis() + 72 * 3_600_000;
        let s = Stats::new("");
        s.set_billing_cycle("u1", boundary);
        s.record_at(
            "u1",
            "glm-5.3",
            &local_day_now(),
            Counters { input: 10, output: 2, ..Default::default() },
        );

        let snap = s.snapshot(0);
        let row = &snap["billingCycles"]["u1"];
        assert!(row.is_object(), "周期行类型错误");
        for k in ["nextResetAtMs", "cycleStartAtMs", "known", "input", "output", "records"] {
            assert!(row.get(k).is_some(), "周期行缺少键 {k}");
        }
        assert!(row["nextResetAtMs"].is_i64() || row["nextResetAtMs"].is_u64(), "nextResetAtMs 不是数字");
        assert_eq!(row["known"], json!(true), "known 应为 true");
        assert_eq!(row["input"].as_i64().unwrap(), 10);
        assert_eq!(row["output"].as_i64().unwrap(), 2);
        // 不得泄漏内部键（与既有前端形状约定一致）。
        for k in row.as_object().unwrap().keys() {
            assert!(!k.starts_with('_'), "快照里泄漏了内部键 {k}");
        }
    }

    /// 23. 落盘字段名必须稳定（改名 = 用户数据静默丢失），零值要被省略。
    #[test]
    fn billing_cycle_json_round_trip() {
        let input = CycleState { next_reset_ms: 1_790_000_000_000, cycle_start_ms: 1_787_000_000_000 };
        let raw = serde_json::to_string(&input).unwrap();
        let out: CycleState = serde_json::from_str(&raw).unwrap();
        assert_eq!(out, input, "往返不一致");
        assert!(raw.contains("nextResetAtMs") && raw.contains("cycleStartAtMs"), "字段名变了：{raw}");

        // 0 值应被省略（老文件的形状不被新键污染）。
        assert_eq!(serde_json::to_string(&CycleState::default()).unwrap(), "{}", "零值未被省略");
    }

    /// 24. 记录与周期更新都不做同步磁盘 IO（热路径不碰盘）。
    ///
    /// 热路径是每个请求的收尾。若这里同步写盘，磁盘抖动会直接变成请求延迟。
    /// 既有设计是「置脏 + 调用方周期落盘」，本任务新增的周期字段**必须**沿用同一
    /// 条路径，不能自己写盘。
    #[test]
    fn record_and_cycle_update_do_not_touch_disk() {
        let dir = std::env::temp_dir().join(format!("wbswg-usage-nodisk-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("usage.json");
        let path_s = path.to_string_lossy().to_string();
        let _ = std::fs::remove_dir_all(&dir);

        let s = Stats::new(&path_s);
        s.set_billing_cycle("u1", chrono::Local::now().timestamp_millis() + 72 * 3_600_000);
        s.record_billed_at_ms(
            "u1",
            "glm-5.3",
            &Billing::default(),
            chrono::Local::now().timestamp_millis(),
            Counters { input: 1, ..Default::default() },
        );

        assert!(!path.exists(), "记录/周期更新做了同步落盘 —— 热路径不该有磁盘 IO");
        assert!(s.dirty_for_test(), "写路径必须置脏，否则永远不会被落盘");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 25. 读投影不得让陈旧周期「复活」，随后的等值报告仍应被采纳。
    #[test]
    fn report_and_set_billing_cycle_do_not_resurrect_stale_cycle() {
        let boundary = at_ms(2026, 3, 31, 8, 0, 0);
        let now = boundary + 3_600_000;
        let s = Stats::new("");
        s.set_now_for_test(now);
        s.set_billing_cycle_at("u1", boundary, boundary - 72 * 3_600_000);

        // 只读一次：投影推进到 4 月 30 日，但落盘仍是 3 月 31 日。
        let p = s.cycle_of("u1").expect("应知道周期");
        assert_eq!(p.next_reset_ms, add_months_ms(boundary, 1));
        assert_eq!(s.cycle_state_for_test("u1").unwrap().next_reset_ms, boundary);

        // 一个恰好等于「推进后边界」的报告到达 ⇒ 必须被采纳。
        let advanced = add_months_ms(boundary, 1);
        assert!(
            s.set_billing_cycle_at("u1", advanced, now),
            "推进后的边界被当成陈旧报告丢掉了（周期再也无法前进）"
        );
        assert_eq!(s.cycle_state_for_test("u1").unwrap().next_reset_ms, advanced);
    }

    /// 26. `advance_boundary_ms` 对**不可推进**的时刻返回 0，但普通时刻照常推进。
    ///
    /// 「返回 0」不能变成万能挡箭牌 —— 否则正常账号的周期也永远不推进。
    #[test]
    fn advance_boundary_rejects_only_non_advancing_steps() {
        use chrono::TimeZone;
        let utc = chrono::Utc;
        let normal = utc.with_ymd_and_hms(2026, 6, 15, 12, 0, 0).single().unwrap().timestamp_millis();
        let want = add_months_ms_in(normal, 1, &utc);
        assert!(want > normal, "测试自身的假设不成立：普通时刻的加月结果未前进");
        assert_eq!(advance_boundary_ms(normal, &utc), want, "普通时刻被误判为不可推进");

        // 人造的「不前进」：入参已经是 0 ⇒ add_months_ms_in 返回 0 ⇒ adv <= next。
        assert_eq!(advance_boundary_ms(0, &utc), 0, "0 应被拒绝");
        // 负数同理（add_months_ms_in 对 ms<=0 直接返回 0）。
        assert_eq!(advance_boundary_ms(-1, &utc), 0, "负值应被拒绝");
    }

    /// 27. 时区显式版本：`add_months_ms_in` 在 UTC 下的月末钳制与本地一致。
    #[test]
    fn add_months_ms_in_is_timezone_explicit() {
        use chrono::TimeZone;
        let utc = chrono::Utc;
        let march31 = utc.with_ymd_and_hms(2026, 3, 31, 12, 0, 0).single().unwrap().timestamp_millis();
        let feb28 = utc.with_ymd_and_hms(2026, 2, 28, 12, 0, 0).single().unwrap().timestamp_millis();
        assert_eq!(add_months_ms_in(march31, -1, &utc), feb28, "UTC 下的月末钳制不一致");
        assert_eq!(add_months_ms_in(0, 1, &utc), 0, "0 入参应返回 0");
        assert_eq!(add_months_ms_in(-1, 1, &utc), 0, "负入参应返回 0");
    }

    /// 28. 投影在遇到「不可推进」边界时必须**收敛**（不能空转）。
    ///
    /// 卡死是这一族唯一的真正失败模式：循环是**持锁**跑的，空转等于把网关挂住。
    /// 这里用 `advance_boundary_ms` 的显式 0 返回值 + 上限共同保证收敛，故只需断言
    /// 「调用会返回」且结果为「未知」。
    #[test]
    fn cycle_projection_terminates_on_unadvanceable_boundary() {
        let s = Stats::new("");
        let now = chrono::Local::now().timestamp_millis();
        s.set_now_for_test(now);
        // 边界为 1（1970）且起点也为 1：远在过去但可推进 ⇒ 应能收敛到未来。
        s.force_cycle_state_for_test("u1", 1, 1);
        let p = s.cycle_of("u1").expect("可推进的边界应收敛出结果");
        assert!(p.next_reset_ms > now, "未收敛到未来");
        assert!(p.known, "可推进时应报 known");
    }

    // ---------- 模型名大小写归一（2026-10-05 所有者现场） ----------

    /// 所有者逐字：「一样的模型居然还分了两条记录」。他的截图里
    /// `GLM-5.3-Flash · ZCode`（11.0M / 98 次）与 `glm-5.3-flash · ZCode`
    /// （1.6K）并排两行 —— 其实是同一个模型。
    ///
    /// 这里复刻真实数据的形状：上游目录规范名 `GLM-5.3-Flash`（token 量大）
    /// 与客户端发来的小写 `glm-5.3-flash`（次数略多）各记一份，治愈后必须
    /// **只剩一行**，且留下的拼写与目录一致。
    #[test]
    fn load_heals_case_split_model_keys() {
        let dir = std::env::temp_dir().join(format!("wbswg-casefold-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("usage.json");
        let path_s = path.to_string_lossy().to_string();

        // 手工写一份「分裂」的文件：正是所有者现场的形状。
        let split = serde_json::json!({
            "version": 1,
            "days": { "2026-10-05": { "input": 300, "output": 10, "records": 2 } },
            "models": {
                // 目录拼写：输入大得多（21,278,353），但次数少（141）
                "GLM-5.3-Flash": { "2026-10-05": { "input": 21_000_000, "output": 80_000, "records": 141 } },
                // 客户端拼写：输入小（10,833,442），次数多（157）
                "glm-5.3-flash": { "2026-10-05": { "input": 10_000_000, "output": 200_000, "records": 157 } }
            },
            "accounts": { "zcode-1b2941c020ef": { "2026-10-05": { "input": 300, "records": 2 } } },
            "accountModels": {
                "zcode-1b2941c020ef": {
                    "GLM-5.3-Flash": { "2026-10-05": { "input": 200, "records": 94 } },
                    "glm-5.3-flash": { "2026-10-05": { "input": 100, "records": 1 } }
                }
            },
            "billed": {
                "GLM-5.3-Flash": { "zcode||?": { "2026-10-05": { "input": 200, "records": 94 } } },
                "glm-5.3-flash": { "zcode||?": { "2026-10-05": { "input": 100, "records": 1 } } }
            },
            "billingMeta": {
                "GLM-5.3-Flash\u{0}zcode||?": { "product": "zcode", "region": "", "has_multiplier": false },
                "glm-5.3-flash\u{0}zcode||?": { "product": "zcode", "region": "", "has_multiplier": false }
            }
        });
        std::fs::write(&path, serde_json::to_vec(&split).unwrap()).unwrap();

        let s = Stats::new(&path_s);
        let snap = s.snapshot(0);

        // ① 只剩一行，且用的是目录拼写（token 量大者胜）。
        let models = snap["models"].as_array().unwrap();
        let keys: Vec<&str> = models.iter().filter_map(|m| m["key"].as_str()).collect();
        assert_eq!(keys, vec!["GLM-5.3-Flash"], "两个拼写必须合并成一行：{keys:?}");
        // ② 用量是**相加**的，不是丢弃其中一份。
        assert_eq!(models[0]["input"], json!(31_000_000));
        assert_eq!(models[0]["output"], json!(280_000));
        assert_eq!(models[0]["records"], json!(298), "141 + 157");
        // ③ accountModels 也并了（否则按账号收窄时又裂开）。
        let am = &snap["accountModels"]["zcode-1b2941c020ef"];
        assert_eq!(am.as_object().unwrap().len(), 1, "账号维度也必须只剩一个键");
        assert_eq!(am["GLM-5.3-Flash"]["input"], json!(300));
        assert_eq!(am["GLM-5.3-Flash"]["records"], json!(95), "94 + 1");
        // ④ 计费表并了，且归属还在（billingMeta 的键改了名，快照里查得到）。
        let mb = snap["modelBilling"]["GLM-5.3-Flash"].as_array().unwrap();
        assert_eq!(mb.len(), 1);
        assert_eq!(mb[0]["product"], json!("zcode"), "归一后仍要能查到归属");
        assert_eq!(mb[0]["input"], json!(300));
        // 旧拼写在计费表里必须消失。
        assert!(snap["modelBilling"].get("glm-5.3-flash").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 治愈只在装载时做，且**不置 dirty** —— 只读打开面板的进程不得改写账本。
    #[test]
    fn load_heal_does_not_mark_dirty() {
        let dir = std::env::temp_dir().join(format!("wbswg-healdirty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("usage.json");
        let path_s = path.to_string_lossy().to_string();
        let split = serde_json::json!({
            "version": 1,
            "models": {
                "GLM-5.3-Flash": { "2026-10-05": { "input": 10, "records": 1 } },
                "glm-5.3-flash": { "2026-10-05": { "input": 20, "records": 2 } }
            }
        });
        let bytes = serde_json::to_vec(&split).unwrap();
        std::fs::write(&path, &bytes).unwrap();

        let s = Stats::new(&path_s);
        s.flush(); // 没有 dirty 时应是 no-op
        let after = std::fs::read(&path).unwrap();
        assert_eq!(after, bytes, "装载治愈不得自行落盘（文件字节应原样不动）");

        let _ = std::fs::remove_dir_all(&dir);
    }
    /// ★★★ 回归（2026-10-05 补）：**写入路径**也必须查全部四张表，不能只看 `models`。
    ///
    /// 反例形状：文件里 `models` 根本没有该模型（只在 `accountModels` 里有旧拼写），
    /// 此时若 `model_key_of` 只看 `models`，就会用本次传来的**新写法**落键 —— 而
    /// 装载治愈只认 `accountModels` 里的旧写法，两边各写一份，同一个模型又裂开。
    #[test]
    fn record_key_lookup_covers_tables_missing_from_models() {
        let mut inner = Inner::default();
        inner.account_models.insert(
            "u1".into(),
            [("GLM-5.3-Flash".to_string(), HashMap::new())]
                .into_iter()
                .collect(),
        );
        // 只用「小写」写法写入：必须沿用它，而不是新造一个 `glm-5.3-flash`。
        assert_eq!(model_key_of(&inner, "glm-5.3-flash"), "GLM-5.3-Flash");
        // `billed` 里的键同样要认。
        let mut inner2 = Inner::default();
        inner2.billed.insert("GLM-5.3".into(), HashMap::new());
        assert_eq!(model_key_of(&inner2, "glm-5.3"), "GLM-5.3");
        // 四张表都没有时才用本次写法。
        assert_eq!(model_key_of(&Inner::default(), "GLM-5.3"), "GLM-5.3");
    }

    /// 治愈后**不得**再残留任何大小写变体（用独立判据复核，防止新表被漏掉）。
    #[test]
    fn heal_leaves_no_leftover_case_variants() {
        let mut inner = Inner::default();
        inner.models.insert(
            "GLM-5.3-Flash".into(),
            HashMap::from([("d".into(), Counters { input: 9, records: 1, ..Default::default() })]),
        );
        inner.models.insert(
            "glm-5.3-flash".into(),
            HashMap::from([("d".into(), Counters { input: 1, records: 1, ..Default::default() })]),
        );
        inner.account_models.insert(
            "u1".into(),
            [("glm-5.3-flash".to_string(), HashMap::new())]
                .into_iter()
                .collect(),
        );
        inner.billed.insert("GLM-5.3-Flash".into(), HashMap::new());
        inner.billing_meta.insert(
            "glm-5.3-flash\u{0}zcode||?".into(),
            Billing { product: "zcode".into(), ..Default::default() },
        );
        merge_case_variants(&mut inner);
        assert!(
            leftover_case_variants(&inner).is_empty(),
            "合并后仍有大小写变体：{:?}",
            leftover_case_variants(&inner)
        );
        assert_eq!(inner.models["GLM-5.3-Flash"]["d"].input, 10, "9 + 1");
    }

    /// `model_key_of`：沿用**已存在**的拼写，而不是把新记录写成另一种大小写。
    #[test]
    fn record_reuses_existing_case_spelling() {
        let s = Stats::new("");
        s.record_at("u1", "GLM-5.3-Flash", "2026-10-05", Counters { input: 100, ..Default::default() });
        // 客户端发小写：必须并进上面那一行，不能新开一行。
        s.record_at("u1", "glm-5.3-flash", "2026-10-05", Counters { input: 1, ..Default::default() });
        s.record_at("u1", "GLM-5.3-FLASH", "2026-10-05", Counters { input: 2, ..Default::default() });
        let snap = s.snapshot(0);
        let models = snap["models"].as_array().unwrap();
        assert_eq!(models.len(), 1, "三种大小写必须归成一行");
        assert_eq!(models[0]["key"], json!("GLM-5.3-Flash"), "沿用首次出现的拼写");
        assert_eq!(models[0]["input"], json!(103));
    }

    /// 前缀 + 大小写同时出现（`zcode:glm-5.3-flash` 对 `GLM-5.3-Flash`）。
    #[test]
    fn bare_then_case_fold_compose() {
        let s = Stats::new("");
        s.record_at("u1", "GLM-5.3-Flash", "2026-10-05", Counters { input: 100, ..Default::default() });
        s.record_at("u1", "zcode:glm-5.3-flash", "2026-10-05", Counters { input: 1, ..Default::default() });
        let snap = s.snapshot(0);
        let models = snap["models"].as_array().unwrap();
        assert_eq!(models.len(), 1, "先剥前缀、再折大小写");
        assert_eq!(models[0]["key"], json!("GLM-5.3-Flash"));
    }

    /// 残缺文件：模型键只出现在 `accountModels` / `billingMeta` 里也要治。
    #[test]
    fn heal_covers_keys_missing_from_models() {
        let mut inner = Inner::default();
        inner.account_models.insert(
            "u1".into(),
            [
                ("GLM-5.3-Flash".to_string(), HashMap::from([("d".to_string(), Counters { input: 5, ..Default::default() })])),
                ("glm-5.3-flash".to_string(), HashMap::from([("d".to_string(), Counters { input: 7, ..Default::default() })])),
            ]
            .into_iter()
            .collect(),
        );
        inner.billing_meta.insert(
            "glm-5.3-flash\u{0}zcode||?".to_string(),
            Billing { product: "zcode".into(), ..Default::default() },
        );
        merge_case_variants(&mut inner);
        let by_model = inner.account_models.get("u1").unwrap();
        assert_eq!(by_model.len(), 1, "只出现在 accountModels 的键也要合并");
        assert_eq!(by_model["GLM-5.3-Flash"]["d"].input, 12, "5 + 7");
        assert!(
            inner.billing_meta.contains_key("GLM-5.3-Flash\u{0}zcode||?"),
            "billingMeta 的键必须跟着改名"
        );
    }

    /// 大小写不同的模型**不得**被误并（`Auto` 与 `auto` 只差大小写但同属一个
    /// 小写形态 —— 这是有意的：它们本来就该是一行）。真正要防的是「不同模型」
    /// 被并：这里断言折叠只按大小写，不做任何模糊匹配。
    #[test]
    fn case_fold_is_not_fuzzy_matching() {
        let s = Stats::new("");
        s.record_at("u1", "glm-5.3-flash", "2026-10-05", Counters { input: 1, ..Default::default() });
        s.record_at("u1", "glm-5.3", "2026-10-05", Counters { input: 2, ..Default::default() });
        let snap = s.snapshot(0);
        let models = snap["models"].as_array().unwrap();
        assert_eq!(models.len(), 2, "`-flash` 与不带 `-flash` 是两个模型，不能并");
    }

    /// ★★★ **所有者真机数据回归**（默认忽略；用环境变量指到他的账本才跑）。
    ///
    /// ```text
    /// $env:WBSWG_REAL_USAGE = "C:\Users\<user>\.wb-switch\gateway\gateway_data\usage.json"
    /// cargo test -p ai-gateway-router --lib usage::tests::heal_real_usage_file -- --ignored --nocapture
    /// ```
    ///
    /// 为什么要有这一条：合成数据的形状是我猜的，而这一条直接吃**他的**文件。
    /// 断言写法上刻意不硬编码数字（他的用量每天都在涨），而是「把原文件里所有
    /// 只差大小写的键加起来」再比对治愈结果 —— 这样断言既严格又不会明天就红。
    #[test]
    #[ignore = "需要 WBSWG_REAL_USAGE 指向真实 usage.json"]
    fn heal_real_usage_file() {
        let Ok(src) = std::env::var("WBSWG_REAL_USAGE") else {
            return;
        };
        let raw = std::fs::read(&src).expect("读取真实 usage.json");
        let before: serde_json::Value = serde_json::from_slice(&raw).expect("解析真实 usage.json");

        // ① 先算出「按大小写折叠后，每个小写形态的合计」。
        let mut want: HashMap<String, (i64, i64, i64)> = HashMap::new();
        for (k, series) in before["models"].as_object().expect("models 必须是对象") {
            let mut t = (0i64, 0i64, 0i64);
            for c in series.as_object().unwrap().values() {
                t.0 += c["input"].as_i64().unwrap_or(0);
                t.1 += c["output"].as_i64().unwrap_or(0);
                t.2 += c["records"].as_i64().unwrap_or(0);
            }
            let e = want.entry(k.to_ascii_lowercase()).or_default();
            e.0 += t.0;
            e.1 += t.1;
            e.2 += t.2;
        }
        let split_before = want.len();
        let raw_keys = before["models"].as_object().unwrap().len();
        println!("原文件模型键 {raw_keys} 个 → 折叠后 {split_before} 个");

        // ② 走真实装载路径（`Stats::new` 会 `load()`）。
        let s = Stats::new(src.clone());
        let snap = s.snapshot(0);
        let models = snap["models"].as_array().unwrap();

        // ③ 每个小写形态恰好一行。
        assert_eq!(
            models.len(),
            split_before,
            "治愈后模型行数应等于折叠后的形态数：{:?}",
            models.iter().map(|m| m["key"].clone()).collect::<Vec<_>>()
        );
        // ④ 逐行核对：合计必须与手工折叠完全一致（不丢、不重）。
        for m in models {
            let key = m["key"].as_str().unwrap();
            let (i, o, r) = want[key.to_ascii_lowercase().as_str()];
            assert_eq!(m["input"].as_i64().unwrap(), i, "{key} 输入量对不上");
            assert_eq!(m["output"].as_i64().unwrap(), o, "{key} 输出量对不上");
            assert_eq!(m["records"].as_i64().unwrap(), r, "{key} 记录数对不上");
        }
        // ⑤ `modelBilling` 里不得再有两个只差大小写的键。
        if let Some(mb) = snap["modelBilling"].as_object() {
            let mut seen: HashMap<String, String> = HashMap::new();
            for k in mb.keys() {
                if let Some(prev) = seen.insert(k.to_ascii_lowercase(), k.clone()) {
                    panic!("modelBilling 仍有大小写分裂：{prev} / {k}");
                }
            }
        }
        // ⑥ 原文件必须**没被改写**（装载不得落盘）。
        assert_eq!(std::fs::read(&src).unwrap(), raw, "装载治愈不得改写原文件");
    }

    // ---------- 计费归属回填：键前缀优先（2026-10-05 真机现场） ----------

    /// ★★★ 回填的判据必须是**计费键的第一段**，不能是「某个用过该模型的 uid 的形态」。
    ///
    /// 现场（pkg1027 真机）：`GLM-5.3` 的两行标签在界面上是「· WorkBuddy」，而它们的
    /// 计费键明明是 `zcode||?` 与 `qoder||?`。根因是 `billing_meta` 里那两条的
    /// `product` 是空串（早期记录），回填时被「该模型有裸 UUID 账号用过 ⇒ 算 workbuddy」
    /// 覆盖；而 `account_models` 是 HashMap，谁先迭代到**不确定** —— 实测同一份文件
    /// 连起 4 次，`product` 分布是 9/1、8/2、9/1、10/0。
    ///
    /// 这里把两种账号都放进 `account_models`，并让裸 UUID **先**出现：修好后结果
    /// 与迭代顺序无关，且必须由键前缀决定。
    #[test]
    fn backfill_prefers_billing_key_over_uid_shape() {
        let dir = std::env::temp_dir().join(format!("wbswg-backfill-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("usage.json");
        let path_s = path.to_string_lossy().to_string();

        // 手工造文件：`m` 有两条计费行（zcode / qoder），meta 的 product 都是空串；
        // 而 `accountModels` 里既有裸 UUID（WorkBuddy）又有 zcode- 账号。
        let file = serde_json::json!({
            "version": 1,
            "models": { "m": { "2026-10-05": { "input": 30, "records": 2 } } },
            "accountModels": {
                // ★ 裸 UUID 放前面：旧实现在 HashMap 迭代顺序下会先命中它。
                "019f1772-4976-7393-b4d4-b93ab04b7fe7": { "m": { "2026-10-05": { "input": 20, "records": 1 } } },
                "zcode-1b2941c020ef": { "m": { "2026-10-05": { "input": 10, "records": 1 } } },
            },
            "billed": {
                "m": {
                    "zcode||?": { "2026-10-05": { "input": 10, "records": 1 } },
                    "qoder||?": { "2026-10-05": { "input": 20, "records": 1 } },
                },
            },
            "billingMeta": {
                "m\u{0}zcode||?": { "product": "", "region": "", "has_multiplier": false },
                "m\u{0}qoder||?": { "product": "", "region": "", "has_multiplier": false },
            },
        });
        std::fs::write(&path, serde_json::to_string_pretty(&file).unwrap()).unwrap();

        let s = Stats::new(&path_s);
        let snap = s.snapshot(0);
        let groups = snap["modelBilling"]["m"].as_array().unwrap();
        let mut got: Vec<(String, String)> = groups
            .iter()
            .map(|g| {
                (
                    g["key"].as_str().unwrap().to_string(),
                    g["product"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("qoder||?".to_string(), "qoder".to_string()),
                ("zcode||?".to_string(), "zcode".to_string()),
            ],
            "回填必须由计费键前缀决定，而不是由某个用过该模型的 uid 形态决定"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 键**没有**平台前缀（空串）时，仍按 uid 形态兜底 —— 回填不能因此失效。
    #[test]
    fn backfill_falls_back_to_uid_shape_when_key_has_no_product() {
        let dir = std::env::temp_dir().join(format!("wbswg-backfill-fallback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("usage.json");
        let path_s = path.to_string_lossy().to_string();

        let file = serde_json::json!({
            "version": 1,
            "models": { "m": { "2026-10-05": { "input": 10, "records": 1 } } },
            "accountModels": {
                "zcode-1b2941c020ef": { "m": { "2026-10-05": { "input": 10, "records": 1 } } },
            },
            // 键的第一段是空串（老数据 / 平台未声明的调用点）。
            "billed": { "m": { "||?": { "2026-10-05": { "input": 10, "records": 1 } } } },
            "billingMeta": {
                "m\u{0}||?": { "product": "", "region": "", "has_multiplier": false },
            },
        });
        std::fs::write(&path, serde_json::to_string_pretty(&file).unwrap()).unwrap();

        let s = Stats::new(&path_s);
        let snap = s.snapshot(0);
        let groups = snap["modelBilling"]["m"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0]["product"],
            serde_json::json!("zcode"),
            "键没有平台前缀时仍应按 uid 形态兜底"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
