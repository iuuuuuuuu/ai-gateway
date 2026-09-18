//! 模型计费真值：**哪些模型在哪个区域免费**。
//!
//! 为什么需要这个模块（所有者报的缺陷）：
//!
//! > 「兼容网关，消耗积分与实际不符。国际版账号的 4.1flash 模型，是免费的，
//! > 但是我看到后面居然统计的也有积分」
//!
//! 免费模型的调用**本就不该扣积分**。此前宿主判断不了「哪些模型免费」，
//! 于是把所有调用一律按「余额下降」记成消耗 —— 免费模型的调用混在里面，
//! 表现为「统计出的消耗与实际不符」。
//!
//! # 判定依据：上游权威数据，**不是**本地硬编码清单
//!
//! 上游 `/v3/config` 的每个模型条目带一个 `credits` 字段，值是**计费倍率**。
//! 倍率 0 即免费。这是 Go 网关从上游拿到、再经 `/v1/models/regions`
//! 按区域透出的真值，本模块只做两件事：
//!
//!   1. 读那份按区域的真值（`credit_multiplier`）；
//!   2. 结合**账号所属区域**给出「这个模型对这次调用是否免费」的结论。
//!
//! 为什么不硬编码一张免费清单：同名模型在两区的计费可以**相反**，实测
//! （2026-09-18 直接拉两区 `/v3/config`）：
//!
//! ```text
//! 模型                    国服         国际版
//! deepseek-v4.1-flash     "x0.03"     "x0.00"   ← 国服计费、国际版免费
//! hy3                     "x0.00"     "x0.00"   ← 两区都免费
//! fast-model              "x0.21"     "x0.34 credits"
//! ```
//!
//! 一张全局清单没法表达「同一个名字两区结论相反」，必然在其中一侧判错 ——
//! 不是把该计费的显示成免费，就是把免费的显示成计费（后者正是本缺陷）。
//! 而且上游随时会调整促销（免费期有起止），硬编码表过期后同样是错的。
//!
//! # 三态，绝不把「不知道」当「免费」
//!
//! 上游**只给部分模型**写 `credits`（实测国服 52 个里只有 33 个）。字段缺失
//! 时必须得到「不知道」，而不是「免费」：
//!
//! | 上游数据 | 结论 | 界面 |
//! |---|---|---|
//! | 倍率 0 | 免费（确定） | `0` |
//! | 倍率 > 0 | 计费（确定） | 正常扣减 |
//! | 字段缺失 / 拿不到 | **不知道** | `—` |
//!
//! 把「不知道」当免费会把本该计费的算成 0 —— 与所有者报的 bug 反方向、
//! 但同样是「统计与实际不符」。本项目里「—」与「0」语义不同（「—」= 不知道，
//! 0 = 确定的零），这里严格守住这条线。

use serde_json::{json, Value};

use crate::modules::config::Region;

/// 一个模型在某个区域的计费判定。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Billing {
    /// 该区域的**上游明确声明**这个模型免费（倍率 0）。
    ///
    /// 注意是「上游明确声明」，不是「我们没查到收费信息」——
    /// 后者是 `Unknown`。免费模型产生的调用，消耗积分应如实显示为 0。
    Free,
    /// 该区域的**上游明确声明**这个模型计费，携带倍率（> 0）。
    ///
    /// 倍率目前只用于展示与排查：本项目按「余额快照差」统计消耗，
    /// 不做「token × 倍率」的推算（那需要上游给 token 数，且会与账单口径漂移）。
    Paid(f64),
    /// 上游没给这个模型的计费信息 → **不下结论**。
    ///
    /// 绝不退化成 `Free`：上游只给部分模型写 `credits`，
    /// 把未声明当免费会让计费模型的消耗被吃掉。
    Unknown,
}

impl Default for Billing {
    /// 默认值刻意是 `Unknown` 而**不是** `Free`。
    ///
    /// `ModelBilling::default()` 会用于「这个模型只在一个区域出现过」时补齐
    /// 另一个区域的格子。若默认成免费，那个没出现过的区域就会凭空得到一个
    /// 「免费」结论 —— 又是一次「把不知道当免费」。
    fn default() -> Self {
        Billing::Unknown
    }
}

impl Billing {
    /// 是否**确定**免费。仅 `Free` 为真 —— `Unknown` 不是免费。
    pub fn is_free(self) -> bool {
        matches!(self, Billing::Free)
    }

    /// 对外表达用的键（前端据此区分「0」与「—」）。
    pub fn key(self) -> &'static str {
        match self {
            Billing::Free => "free",
            Billing::Paid(_) => "paid",
            Billing::Unknown => "unknown",
        }
    }
}

/// 一个模型在两区的计费真值。
#[derive(Clone, Debug, Default)]
pub struct ModelBilling {
    pub cn: Billing,
    pub intl: Billing,
}

impl ModelBilling {
    /// 取指定区域的判定。
    pub fn for_region(&self, region: Region) -> Billing {
        match region {
            Region::Cn => self.cn,
            Region::Intl => self.intl,
        }
    }
}

/// 从 `/v1/models/regions` 的响应里解析出「模型 → 两区计费真值」。
///
/// 输入形状（Go 侧 `admin.go` 的 `modelsRegions`）：
///
/// ```json
/// { "regions": [
///     { "region": "cn",   "models": [ {"id": "deepseek-v4.1-flash",
///                                      "credit_multiplier": 0.03,
///                                      "credits_raw": "x0.03"} ] },
///     { "region": "intl", "models": [ {"id": "deepseek-v4.1-flash",
///                                      "credit_multiplier": 0.0,
///                                      "credits_raw": "x0.00"} ] } ] }
/// ```
///
/// **三态必须完整保留**：
///
///   - `credit_multiplier` 是数字 → `Free`（0）或 `Paid`（>0）；
///   - 是 `null` → `Unknown`（上游未声明该字段，**不是**免费）；
///   - 键不存在、该区域整个缺失、整个响应不可解析 → 同样 `Unknown`。
///
/// 也就是说：**任何一处读不到，都只能是「不知道」**。这条是刻意的 ——
/// 本函数的所有失败路径都必须落到 `Unknown`，不能有例外。
pub fn parse_model_billing(value: &Value) -> std::collections::HashMap<String, ModelBilling> {
    use std::collections::HashMap;

    let mut out: HashMap<String, ModelBilling> = HashMap::new();
    let Some(regions) = value.get("regions").and_then(Value::as_array) else {
        // 响应形状不认识（旧版网关没有这个接口/字段）→ 空表 = 全部 Unknown。
        return out;
    };
    for region_view in regions {
        let Some(region) = region_view
            .get("region")
            .and_then(Value::as_str)
            .and_then(region_from_key)
        else {
            continue;
        };
        let Some(models) = region_view.get("models").and_then(Value::as_array) else {
            continue;
        };
        for model in models {
            let Some(id) = model
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
            else {
                continue;
            };
            // 注意用 `get` 的结果判断「有没有这个键」：
            // Value::Null 会走到 billing_of 的 Unknown 分支，与键缺失同结论。
            let billing = billing_of(model.get("credit_multiplier"));
            let entry = out.entry(id.to_string()).or_default();
            match region {
                Region::Cn => entry.cn = billing,
                Region::Intl => entry.intl = billing,
            }
        }
    }
    out
}

/// 把区域键解析成 `Region`（只认上游实际会给出的两个键）。
///
/// 不做「未知键按国服处理」的兜底：那会把一个我们没见过的区域的真值
/// 写进国服的格子，从而谎报国服的计费结论。宁可跳过（保持 Unknown）。
fn region_from_key(key: &str) -> Option<Region> {
    match key.trim() {
        "cn" => Some(Region::Cn),
        "intl" => Some(Region::Intl),
        _ => None,
    }
}

/// 把 `credit_multiplier` 字段折成三态判定。
///
/// 解析规则（与 Go 侧 `parseCreditMultiplier` 的出口一致，这里只面对数字）：
///
///   `0`    → `Free`
///   `> 0`  → `Paid`
///   其它（`null` / 负数 / 非数字）→ `Unknown`
///
/// 负数按 `Unknown` 而不是 `Free`：上游从未给过负倍率，出现即说明语义变了
/// （可能是「返还」之类）。此时不猜，让界面显示「—」。
fn billing_of(raw: Option<&Value>) -> Billing {
    match raw.and_then(Value::as_f64) {
        Some(value) if value.is_finite() && value > 0.0 => Billing::Paid(value),
        Some(value) if value == 0.0 => Billing::Free,
        _ => Billing::Unknown,
    }
}

/// 判定「这个账号调这个模型，是否确定免费」。
///
/// 必须**同时**看模型与账号区域 —— 这正是本缺陷的核心：
/// `deepseek-v4.1-flash` 在国际版免费、国服计费（实测倍率 0.03）。
/// 只看模型名无法区分两者，只看区域则会误伤国际版上确实计费的模型。
///
/// 账号不在真值表里、区域真值缺失、上游未声明该模型 → `Unknown`。
pub fn billing_for(
    table: &std::collections::HashMap<String, ModelBilling>,
    model: &str,
    region: Region,
) -> Billing {
    let model = normalize_model_name(model);
    if model.is_empty() {
        return Billing::Unknown;
    }
    table
        .get(&model)
        .map(|entry| entry.for_region(region))
        .unwrap_or(Billing::Unknown)
}

/// 归一化模型名，使其能与上游清单里的 id 对上。
///
/// 做两件事，都来自实测：
///
///   1. **剥区域前缀** `cn:` / `global:`。客户端可以带前缀请求
///      （见 Go 侧 `model_allow.go`：前缀是给网关的选号指令，不是模型名的一部分），
///      不剥就会查不到真值 → 免费模型被报成「不知道」。
///   2. **大小写不敏感**。上游清单是小写 id，而客户端可能发
///      `DeepSeek-V4.1-Flash`（实测存在这种写法）。
///
/// 刻意不做「模糊匹配/前缀匹配」：把 `glm-5.3-flash` 匹配到 `glm-5.3`
/// 会让一个计费模型套用另一个模型的免费结论 —— 那比「不知道」危险得多。
pub fn normalize_model_name(model: &str) -> String {
    let trimmed = model.trim();
    let without_prefix = trimmed
        .strip_prefix("cn:")
        .or_else(|| trimmed.strip_prefix("global:"))
        .or_else(|| trimmed.strip_prefix("CN:"))
        .or_else(|| trimmed.strip_prefix("GLOBAL:"))
        .unwrap_or(trimmed);
    without_prefix.trim().to_ascii_lowercase()
}

/// 一个账号在某段窗口内的「免费模型」判定结论。
///
/// 用途：决定该账号的**消耗积分**要不要按 0 计。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FreeOnlyVerdict {
    /// 该账号这个窗口内**用过的每一个模型**都在它所属区域被上游声明为免费。
    ///
    /// 此时真实消耗**必然是 0** —— 免费调用不扣积分。若本地口径算出了正数，
    /// 那个数一定不是这些调用产生的（是读数抖动被误记成了消耗）。
    /// 所有者报的正是这个现象，界面应如实显示 **0**。
    AllFree,
    /// 至少有一个模型明确计费 → 消耗是真的，**不动**原有口径。
    HasPaid,
    /// 有模型判不出来（上游未声明 / 拿不到真值 / 没记录到模型）→ **不下结论**，
    /// 保留原有口径与显示（「—」表示不知道，不是 0）。
    Unknown,
}

impl FreeOnlyVerdict {
    /// 对外表达用的键（前端据此区分「0」与「—」）。
    ///
    /// `all_free` 对应界面上的确定 0，`unknown` 对应「—」。
    pub fn key(self) -> &'static str {
        match self {
            FreeOnlyVerdict::AllFree => "all_free",
            FreeOnlyVerdict::HasPaid => "has_paid",
            FreeOnlyVerdict::Unknown => "unknown",
        }
    }
}

/// 判定「这个账号在窗口内是否只用了免费模型」。
///
/// 这是本修复的**决策函数**，刻意做成纯函数以便逐分支钉死。
///
/// 三条判据，缺一不可：
///
/// 1. **必须同时看区域**：`deepseek-v4.1-flash` 国际版免费、国服计费
///    （实测倍率 0.00 / 0.03）。只看模型名会把国服的消耗也抹成 0。
/// 2. **必须至少有一个模型**：没有任何模型记录时不是「全部免费」，
///    而是「不知道用了什么」—— 空集合上的 `all()` 恒真，直接用它会把
///    「没有数据」误判成「必然免费」，从而把真实消耗清零。
/// 3. **未知不算免费**：只要有一个模型判不出来，整体就是 `Unknown`。
///    局部未知不能推出整体免费 —— 那个未知的模型可能正是消耗来源。
///
/// `models_used` 传该账号在窗口内实际用过的模型名（`None` = 拿不到该数据）。
pub fn free_only_verdict(
    table: &std::collections::HashMap<String, ModelBilling>,
    region: Region,
    models_used: Option<&[String]>,
) -> FreeOnlyVerdict {
    let Some(models) = models_used else {
        return FreeOnlyVerdict::Unknown;
    };
    if models.is_empty() {
        // 见判据 2：空集合不能当成「全部免费」。
        return FreeOnlyVerdict::Unknown;
    }
    let mut has_paid = false;
    for model in models {
        match billing_for(table, model, region) {
            Billing::Free => {}
            Billing::Paid(_) => has_paid = true,
            Billing::Unknown => return FreeOnlyVerdict::Unknown,
        }
    }
    if has_paid {
        FreeOnlyVerdict::HasPaid
    } else {
        FreeOnlyVerdict::AllFree
    }
}

/// 上游计费真值的拉取地址（Go 网关的按区域对比视图）。
///
/// 为什么不自己直连上游 `/v3/config`：网关已经按区域拉好并缓存（1h TTL），
/// 宿主再拉一次等于把同一份上游数据取两遍，两处解析口径迟早漂移
/// （本项目已有太多「两处各算一遍然后对不上」的教训）。
/// 网关的 `/v1/models/regions` 正是为「看见区域真值」而建的观测接口。
pub fn billing_truth_path() -> &'static str {
    "/v1/models/regions"
}

/// 一个账号在三个时间窗上的「只用了免费模型」判定。
///
/// 三个窗口分开存而不是合成一个：某个号可能今天只跑了免费模型（免额度），
/// 但本月早些时候跑过计费模型。合成一个会让「本月」的结论去污染「今日」，
/// 从而在今日这一格给出错误的 0。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WindowVerdicts {
    pub today: FreeOnlyVerdict,
    pub seven_days: FreeOnlyVerdict,
    pub month: FreeOnlyVerdict,
}

impl Default for FreeOnlyVerdict {
    /// 默认 `Unknown`：任何「没算出来」的格子都必须是「不知道」，
    /// 不能默默变成免费。
    fn default() -> Self {
        FreeOnlyVerdict::Unknown
    }
}

impl WindowVerdicts {
    /// 三个窗口是否**全部**判定为「只用免费模型」。
    pub fn all_free(&self) -> bool {
        self.today == FreeOnlyVerdict::AllFree
            && self.seven_days == FreeOnlyVerdict::AllFree
            && self.month == FreeOnlyVerdict::AllFree
    }

    /// 取某个窗口的判定（键为统计页的三个字段名）。
    pub fn of_window(&self, window: &str) -> FreeOnlyVerdict {
        match window {
            "usageToday" => self.today,
            "usage7Days" => self.seven_days,
            "usageThisMonth" => self.month,
            _ => FreeOnlyVerdict::Unknown,
        }
    }
}

/// 统计页三个消耗窗口的字段名，与 `credit_usage.rs` 的输出逐字一致。
pub const USAGE_WINDOWS: [&str; 3] = ["usageToday", "usage7Days", "usageThisMonth"];

/// 把「只用了免费模型」的账号在对应窗口上的消耗**如实归零**。
///
/// 这是本修复对统计投影的唯一写入点，刻意做成纯函数（不碰 IO）以便钉死：
/// 输入是已构建好的 statistics 与逐账号判定，输出是就地改写。
///
/// 归零的语义（为什么是 0 而不是「—」）：
/// 本项目里「—」表示**不知道**，而这里是**确定**的 0 —— 免费模型的调用
/// 本就不扣积分，所以消耗必然是 0。显示「—」会把一个确定的事实说成未知，
/// 与显示错误的数字同样属于「统计与实际不符」。
///
/// 只改这三个 `usage*` 字段，**不动** `change` / `daily` / Token 统计：
///   - `change` 是快照事实（余额确实动过），由另一套口径负责解释；
///   - `daily` 是逐日观察序列，归零单点会让趋势图与合计对不上；
///   - Token 用量与积分是两件事，免费模型照常计 Token（所有者明确要求）。
///
/// 同时写入 `billing` 字段，让前端能区分「0 = 确定免费」与「— = 不知道」。
pub fn zero_out_free_windows(
    statistics: &mut Value,
    verdicts: &std::collections::HashMap<String, WindowVerdicts>,
) {
    // 先记为每个窗口扣掉多少（下面要同步 summary，见函数尾注）。
    let mut deducted: std::collections::HashMap<&str, f64> = std::collections::HashMap::new();

    if let Some(accounts) = statistics.get_mut("accounts").and_then(Value::as_array_mut) {
        for account in accounts.iter_mut() {
            let Some(id) = account
                .get("accountId")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                continue;
            };
            // 没有判定（账号不在当前账号库、或拿不到明细）→ 什么都不做，
            // 保持原有口径。**不是**把它当免费。
            let Some(verdict) = verdicts.get(&id) else {
                continue;
            };
            for window in USAGE_WINDOWS {
                if verdict.of_window(window) == FreeOnlyVerdict::AllFree {
                    // 累加**原来**的值：summary 是各账号之和，逐账号归零后
                    // 汇总必须同步减掉，否则会出现「所有账号都是 0，合计却不是 0」
                    // 这种一眼假的自相矛盾（本项目已有太多「两处各算一遍然后
                    // 对不上」的教训，这里直接同源推导）。
                    if let Some(before) = account.get(window).and_then(Value::as_f64) {
                        *deducted.entry(window).or_insert(0.0) += before;
                    }
                    account[window] = json!(0.0);
                }
            }
            account["billing"] = json!({
                "today": verdict.today.key(),
                "sevenDays": verdict.seven_days.key(),
                "month": verdict.month.key(),
                "allFree": verdict.all_free(),
            });
        }
    }

    // 同步 summary 的同名窗口。
    //
    // 为什么必须同步：统计页顶部的「今日消耗 / 近 7 天 / 本月」取的是
    // `summary.usage*`，而账号行取的是 `accounts[].usage*`。只改后者会让
    // 顶部合计包含已被判定为免费的那部分，于是「所有行都是 0，合计却 > 0」。
    if let Some(summary) = statistics.get_mut("summary").and_then(Value::as_object_mut) {
        for (window, amount) in deducted {
            if amount == 0.0 {
                continue;
            }
            if let Some(current) = summary.get(window).and_then(Value::as_f64) {
                // 钳到 0：理论上各账号之和不会小于其中被判免费者之和，
                // 但浮点累加与旧数据可能让它略为负 —— 负的积分消耗是荒谬值，
                // 显示出来比 0 更难解释。
                summary.insert(window.to_string(), json!((current - amount).max(0.0)));
            }
        }
    }
}

/// 从网关 `/v1/models/regions` 拉取按区域的计费真值。
///
/// 网关未运行 / 未就绪 / 返回异常时返回**空表**（= 全部 Unknown），
/// 而不是报错：统计页不该因为网关没起来就整个失败，退化成「不知道」
/// 即可（原有统计口径照常显示）。这与 `gateway::fetch_usage` 的约定一致。
pub async fn fetch_billing_truth() -> std::collections::HashMap<String, ModelBilling> {
    let empty = std::collections::HashMap::new();
    let cfg = crate::modules::gateway::load_gateway_config();
    let port = cfg.get("port").and_then(Value::as_u64).unwrap_or(7863) as u16;
    let api_key = cfg
        .get("api_key")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let url = format!("http://127.0.0.1:{port}{}", billing_truth_path());
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(2500))
        .build()
    else {
        return empty;
    };
    let mut req = client.get(&url);
    if !api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {api_key}"));
    }
    let Ok(resp) = req.send().await else {
        return empty;
    };
    if !resp.status().is_success() {
        return empty;
    }
    match resp.json::<Value>().await {
        Ok(value) => parse_model_billing(&value),
        Err(_) => empty,
    }
}

/// 逐账号模型用量的来源（网关 `/usage` 的 `accountModels`）。
type UsageByUid = std::collections::HashMap<String, Vec<String>>;

/// 从网关 `/usage?days=N` 取出「uid → 该窗口内用过的模型名」。
///
/// 为什么按窗口各拉一次而不是拉一次全量再前端切分：网关的 `accountModels`
/// 是**按请求范围预先聚合**好的（见 `usage.go` 的 groupList），返回时已经不
/// 带逐日明细，宿主无法从中还原「今日用了哪些模型」。按窗口各问一次是
/// 唯一能与网关口径保持一致的做法（三份都由网关侧同一套累计逻辑产出）。
///
/// 本地回环调用，代价可忽略（统计页本来就会打网关接口拿 Token 用量）。
async fn fetch_usage_models(days: i64) -> Option<UsageByUid> {
    let result = crate::modules::gateway::fetch_usage(Some(days)).await;
    let usage = result.get("usage")?;
    if usage.is_null() {
        // 网关不可达 → 这一窗口**没有明细**（None），调用方据此判 Unknown，
        // 而不是当成「没有用过任何模型」。
        return None;
    }
    let account_models = usage.get("accountModels")?.as_object()?;
    let mut out: UsageByUid = std::collections::HashMap::new();
    for (uid, models) in account_models {
        let names: Vec<String> = models
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m.get("key").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        out.insert(uid.clone(), names);
    }
    Some(out)
}

/// 三个统计窗口默认拉取的天数。
///
/// 「本月」取 31 而不是 30：它是**日历月**的近似上界，31 天必然覆盖任意
/// 日历月的 1 号至今。取超集是**保守方向** —— 多算进来的模型只会让判定
/// 更容易变成「有计费模型」（不归零），而漏算会让本该保留的消耗被清零。
/// 方向的取舍与 `free_only_verdict` 的判据 3 一致：宁可不下结论，不可错判免费。
const WINDOW_FETCH_DAYS: [(&str, i64); 3] = [
    ("usageToday", 1),
    ("usage7Days", 7),
    ("usageThisMonth", 31),
];

/// 汇总三窗口判定：拉真值 + 拉逐窗口模型用量 + 逐账号判定。
///
/// 账号库的 `uid` 是网关池的键，而 `accountModels` 也按 uid 分桶，
/// 因此这里用 uid 对齐（与 `GatewayPage.tsx` 里 `uidByAccountId` 同一口径）。
///
/// 返回 `accountId → WindowVerdicts`；任何一步拿不到数据时对应窗口是
/// `Unknown`（不归零），绝不退化成「免费」。
pub async fn collect_window_verdicts(
    accounts: &[Value],
) -> std::collections::HashMap<String, WindowVerdicts> {
    use std::collections::HashMap;

    let mut out: HashMap<String, WindowVerdicts> = HashMap::new();
    // 刻意**不**先用 `gateway::is_running()` 短路。
    //
    // 那个标志是**本进程内**的 AtomicBool，只在「宿主自己启动过网关」时为真。
    // 网关由别处拉起（或宿主重启过而网关还在）时它为假 —— 此时短路会让我们
    // 明明能拿到真值却全部按 Unknown 处理，本修复静默失效，表现为
    // 「改完还是统计出积分」，且极难排查。
    //
    // 代价也可忽略：网关没在跑时 127.0.0.1 是**连接被拒**（立即返回错误，
    // 不是等 2.5s 超时），三个用量请求还是并发的。与 `gateway::fetch_usage`
    // 的既有约定一致 —— 那个函数同样不因 `running=false` 放弃请求，
    // 只把它作为回报字段。
    let truth = fetch_billing_truth().await;
    if truth.is_empty() {
        // 拿不到上游真值 → 全 Unknown，省掉后面的用量请求。
        return out;
    }

    // 三个窗口**并发**拉取：它们是彼此独立的只读查询，串行只会把延迟叠加
    // （每份 2.5s 超时，最坏情况三倍）。统计页是交互路径，延迟要压住。
    let (today, seven, month) = tokio::join!(
        fetch_usage_models(WINDOW_FETCH_DAYS[0].1),
        fetch_usage_models(WINDOW_FETCH_DAYS[1].1),
        fetch_usage_models(WINDOW_FETCH_DAYS[2].1),
    );
    let per_uid: HashMap<String, HashMap<&'static str, Option<Vec<String>>>> = HashMap::new();
    let mut per_uid = per_uid;
    for (window, fetched) in [
        (WINDOW_FETCH_DAYS[0].0, today),
        (WINDOW_FETCH_DAYS[1].0, seven),
        (WINDOW_FETCH_DAYS[2].0, month),
    ] {
        // 该窗口整体拿不到（网关不可达 / 老版本没有 accountModels）
        // → 这个窗口对**所有**账号都是 None → Unknown。不能把「没数据」
        // 读成「没用量」，那样会把真实消耗清零。
        let Some(by_uid) = fetched else {
            continue;
        };
        for (uid, models) in by_uid {
            per_uid
                .entry(uid)
                .or_default()
                .insert(window, Some(models));
        }
    }

    for account in accounts {
        let Some(account_id) = account.get("id").and_then(Value::as_str) else {
            continue;
        };
        let uid = account.get("uid").and_then(Value::as_str).unwrap_or("");
        let region = Region::of(account);
        let windows = per_uid.get(uid);
        let mut verdicts = WindowVerdicts::default();
        for (window, _) in WINDOW_FETCH_DAYS {
            // 该窗口没有这个账号的记录 → `None` → Unknown（不是空数组！
            // 空数组会被 free_only_verdict 判成 Unknown，但显式传 None
            // 更能表达「这个窗口没有它的数据」）。
            let models = windows.and_then(|m| m.get(window)).and_then(|m| m.as_deref());
            let verdict = free_only_verdict(&truth, region, models);
            match window {
                "usageToday" => verdicts.today = verdict,
                "usage7Days" => verdicts.seven_days = verdict,
                "usageThisMonth" => verdicts.month = verdict,
                _ => {}
            }
        }
        out.insert(account_id.to_string(), verdicts);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 两区真实形态的响应（取自 2026-09-18 实测的 /v3/config 数据形状）。
    fn regions_payload() -> Value {
        json!({
            "regions": [
                {
                    "region": "cn",
                    "models": [
                        {"id": "deepseek-v4.1-flash", "credit_multiplier": 0.03, "credits_raw": "x0.03"},
                        {"id": "hy3", "credit_multiplier": 0.0, "credits_raw": "x0.00"},
                        {"id": "cn-auto", "credit_multiplier": null, "credits_raw": ""},
                        {"id": "fast-model", "credit_multiplier": 0.21, "credits_raw": "x0.21"}
                    ]
                },
                {
                    "region": "intl",
                    "models": [
                        {"id": "deepseek-v4.1-flash", "credit_multiplier": 0.0, "credits_raw": "x0.00"},
                        {"id": "hy3", "credit_multiplier": 0.0, "credits_raw": "x0.00"},
                        {"id": "hy4-preview-f", "credit_multiplier": 0.0, "credits_raw": "x0.00"},
                        {"id": "intl-fast", "credit_multiplier": 0.34, "credits_raw": "x0.34 credits"}
                    ]
                }
            ]
        })
    }

    // -----------------------------------------------------------------------
    // 免费模型 → 0
    // -----------------------------------------------------------------------

    /// 所有者报的那个模型：国际版 deepseek-v4.1-flash 必须判成免费。
    ///
    /// 这条正是缺陷本身 —— 它此前被统计出积分消耗。
    #[test]
    fn owner_reported_model_is_free_on_intl() {
        let table = parse_model_billing(&regions_payload());
        let got = billing_for(&table, "deepseek-v4.1-flash", Region::Intl);
        assert_eq!(got, Billing::Free, "国际版 ds4.1 是免费模型（上游倍率 x0.00）");
        assert!(got.is_free(), "免费模型必须让消耗显示为 0 —— 这正是所有者报的问题");
    }

    /// 免费模型的判定要能穿透「带区域前缀」与「大小写」两种客户端写法。
    ///
    /// 查不到真值会退化成 Unknown（界面显示「—」），而这里应当是确定的 0。
    #[test]
    fn free_verdict_survives_prefix_and_case() {
        let table = parse_model_billing(&regions_payload());
        for name in [
            "deepseek-v4.1-flash",
            "global:deepseek-v4.1-flash",
            "cn:deepseek-v4.1-flash",
            "DeepSeek-V4.1-Flash",
            "  deepseek-v4.1-flash  ",
        ] {
            assert_eq!(
                billing_for(&table, name, Region::Intl),
                Billing::Free,
                "{name} 应判为国际版免费 —— 前缀/大小写/空白不该让结论退化成「不知道」"
            );
        }
    }

    // -----------------------------------------------------------------------
    // 计费模型 → 正常扣减
    // -----------------------------------------------------------------------

    /// 计费模型必须判成 Paid，且带上倍率（不能因为「不是免费」就一律 Unknown）。
    #[test]
    fn paid_model_reports_multiplier() {
        let table = parse_model_billing(&regions_payload());
        match billing_for(&table, "intl-fast", Region::Intl) {
            Billing::Paid(m) => assert!((m - 0.34).abs() < 1e-9, "倍率应为 0.34，实际 {m}"),
            other => panic!("计费模型应为 Paid，实际 {other:?}"),
        }
        match billing_for(&table, "fast-model", Region::Cn) {
            Billing::Paid(m) => assert!((m - 0.21).abs() < 1e-9, "倍率应为 0.21，实际 {m}"),
            other => panic!("计费模型应为 Paid，实际 {other:?}"),
        }
        assert!(!billing_for(&table, "intl-fast", Region::Intl).is_free());
    }

    // -----------------------------------------------------------------------
    // 同名模型在不同区域的不同结论
    // -----------------------------------------------------------------------

    /// **本修复的核心用例**：同一个模型名，国际版免费、国服计费。
    ///
    /// 这直接证明了判定不能只看模型 ID：任何「全局免费清单」都会在其中
    /// 一侧判错 —— 国服被少算消耗，或国际版被多算（后者是所有者报的现象）。
    #[test]
    fn same_model_differs_by_region() {
        let table = parse_model_billing(&regions_payload());
        let intl = billing_for(&table, "deepseek-v4.1-flash", Region::Intl);
        let cn = billing_for(&table, "deepseek-v4.1-flash", Region::Cn);

        assert_eq!(intl, Billing::Free, "国际版：免费");
        assert!(matches!(cn, Billing::Paid(_)), "国服：计费（倍率 0.03），实际 {cn:?}");
        assert_ne!(intl, cn, "同名模型两区结论必须可以不同");
    }

    /// 反方向也要成立：两区都免费的模型（hy3）不能因为区域就变成计费。
    ///
    /// 防的是「实现走偏成『凡国际版就免费』或『凡国服就计费』」——
    /// 那会让 hy3 的国服调用被凭空算上消耗。
    #[test]
    fn both_regions_can_be_free() {
        let table = parse_model_billing(&regions_payload());
        assert_eq!(billing_for(&table, "hy3", Region::Cn), Billing::Free);
        assert_eq!(billing_for(&table, "hy3", Region::Intl), Billing::Free);
    }

    /// 只在某一个区域存在的免费模型，另一个区域是「不知道」而不是「免费」。
    ///
    /// `hy4-preview-f` 是国际版专有免费模型。拿国服账号去问它，国服清单里
    /// 没有它 —— 此时是「不知道」（那个区域根本没有这个模型名），
    /// 不能顺手沿用国际版的免费结论。
    #[test]
    fn free_in_one_region_is_unknown_in_the_other() {
        let table = parse_model_billing(&regions_payload());
        assert_eq!(billing_for(&table, "hy4-preview-f", Region::Intl), Billing::Free);
        assert_eq!(
            billing_for(&table, "hy4-preview-f", Region::Cn),
            Billing::Unknown,
            "国服清单里没有这个模型 → 不知道，不能沿用国际版的免费结论"
        );
    }

    // -----------------------------------------------------------------------
    // 未声明 ≠ 免费（反方向的错同样要防）
    // -----------------------------------------------------------------------

    /// `credit_multiplier: null`（上游未声明）必须是 Unknown，**不是** Free。
    ///
    /// 上游只给部分模型写 credits（实测国服 52 个里只有 33 个）。
    /// 把 null 读成免费会把本该计费的模型统计成 0 —— 反方向但同样错。
    #[test]
    fn null_multiplier_is_unknown_not_free() {
        let table = parse_model_billing(&regions_payload());
        let got = billing_for(&table, "cn-auto", Region::Cn);
        assert_eq!(got, Billing::Unknown, "上游未声明倍率 → 不知道");
        assert!(!got.is_free(), "「不知道」绝不能被当成免费");
    }

    /// 键**整个缺失**（连 null 都没有）同样是 Unknown。
    ///
    /// 这是与上一条不同的形态：老版本网关切到新接口后可能没有这个键。
    #[test]
    fn missing_multiplier_key_is_unknown_not_free() {
        let payload = json!({
            "regions": [
                {"region": "intl", "models": [{"id": "some-model"}]}
            ]
        });
        let table = parse_model_billing(&payload);
        assert_eq!(billing_for(&table, "some-model", Region::Intl), Billing::Unknown);
    }

    /// 整份响应不可用（旧版网关没有 /v1/models/regions）→ 全部 Unknown。
    ///
    /// 旧版网关没有该接口时，宿主**不能**因此认为「没有免费模型」或
    /// 「全是免费」—— 只能是「不知道」，退回原有统计口径。
    #[test]
    fn unusable_payload_yields_unknown_for_everything() {
        for payload in [
            json!({}),
            json!({"error": "unknown debug view"}),
            json!({"regions": "not-an-array"}),
            json!({"regions": [{"region": "intl"}]}),
        ] {
            let table = parse_model_billing(&payload);
            assert!(table.is_empty(), "形状不认识时不该编造真值：{payload}");
            assert_eq!(
                billing_for(&table, "deepseek-v4.1-flash", Region::Intl),
                Billing::Unknown,
                "拿不到上游真值只能是「不知道」，不能当免费"
            );
        }
    }

    /// 未知区域键不写进 cn / intl 任一格（否则会谎报那两区的结论）。
    #[test]
    fn unknown_region_key_is_ignored() {
        let payload = json!({
            "regions": [
                {"region": "mars", "models": [{"id": "m1", "credit_multiplier": 0.0}]}
            ]
        });
        let table = parse_model_billing(&payload);
        assert!(table.is_empty(), "没见过的区域键不该被塞进国服或国际版");
        assert_eq!(billing_for(&table, "m1", Region::Cn), Billing::Unknown);
        assert_eq!(billing_for(&table, "m1", Region::Intl), Billing::Unknown);
    }

    /// 负数/非数字倍率 → Unknown（上游语义变了就不猜）。
    #[test]
    fn weird_multiplier_values_are_unknown() {
        let payload = json!({
            "regions": [
                {"region": "intl", "models": [
                    {"id": "neg", "credit_multiplier": -1.5},
                    {"id": "str", "credit_multiplier": "x0.00"},
                    {"id": "nan", "credit_multiplier": null}
                ]}
            ]
        });
        let table = parse_model_billing(&payload);
        for id in ["neg", "str", "nan"] {
            assert_eq!(
                billing_for(&table, id, Region::Intl),
                Billing::Unknown,
                "{id} 的倍率形态不认识 → 不猜（不能当免费）"
            );
        }
    }

    /// 找不到模型 / 空模型名 → Unknown。
    #[test]
    fn unknown_model_is_unknown() {
        let table = parse_model_billing(&regions_payload());
        assert_eq!(billing_for(&table, "no-such-model", Region::Intl), Billing::Unknown);
        assert_eq!(billing_for(&table, "", Region::Intl), Billing::Unknown);
        assert_eq!(billing_for(&table, "   ", Region::Intl), Billing::Unknown);
    }

    /// 不做模糊匹配：`glm-5.3-flash` 不能套用 `glm-5.3` 的结论。
    ///
    /// 若做了前缀匹配，一个计费模型会继承另一个模型的免费结论 ——
    /// 那比「不知道」危险得多（凭空把消耗抹掉）。
    #[test]
    fn no_fuzzy_prefix_matching() {
        let payload = json!({
            "regions": [
                {"region": "intl", "models": [
                    {"id": "glm-5.3", "credit_multiplier": 0.0}
                ]}
            ]
        });
        let table = parse_model_billing(&payload);
        assert_eq!(billing_for(&table, "glm-5.3", Region::Intl), Billing::Free);
        assert_eq!(
            billing_for(&table, "glm-5.3-flash", Region::Intl),
            Billing::Unknown,
            "不同模型名不能继承彼此的计费结论"
        );
    }

    /// `Billing::key()` 是前端区分「0」与「—」的依据，三种取值必须稳定。
    #[test]
    fn billing_keys_are_stable() {
        assert_eq!(Billing::Free.key(), "free");
        assert_eq!(Billing::Paid(0.5).key(), "paid");
        assert_eq!(Billing::Unknown.key(), "unknown");
    }

    // -----------------------------------------------------------------------
    // 决策函数：该账号这个窗口是不是「只用了免费模型」
    // -----------------------------------------------------------------------

    /// 归一化函数本身：前缀剥离与大小写。
    #[test]
    fn normalize_strips_prefix_and_lowercases() {
        assert_eq!(normalize_model_name("DeepSeek-V4.1-Flash"), "deepseek-v4.1-flash");
        assert_eq!(normalize_model_name("cn:glm-5.3"), "glm-5.3");
        assert_eq!(normalize_model_name("global:glm-5.3"), "glm-5.3");
        assert_eq!(normalize_model_name("  hy3 "), "hy3");
        // 前缀不在开头时不动（它是模型名的一部分，不是选号指令）
        assert_eq!(
            normalize_model_name("custom-local:deepseek-v4-flash"),
            "custom-local:deepseek-v4-flash"
        );
    }

    fn models(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// 所有者报的场景：国际版账号只用了 ds4.1（免费）→ 必须是 AllFree（消耗记 0）。
    #[test]
    fn intl_account_using_only_free_model_is_all_free() {
        let table = parse_model_billing(&regions_payload());
        assert_eq!(
            free_only_verdict(&table, Region::Intl, Some(&models(&["deepseek-v4.1-flash"]))),
            FreeOnlyVerdict::AllFree,
            "国际版只用免费模型 → 消耗必为 0（所有者报的就是这个没生效）"
        );
    }

    /// 同一个模型在国服是计费的 → 同一个账号区域换成国服就不能判成免费。
    ///
    /// 这条是「必须同时看模型与区域」在**决策层**的锁：若实现只看模型名，
    /// 国服 ds4.1 的真实消耗会被抹成 0。
    #[test]
    fn same_model_on_cn_is_not_all_free() {
        let table = parse_model_billing(&regions_payload());
        assert_eq!(
            free_only_verdict(&table, Region::Cn, Some(&models(&["deepseek-v4.1-flash"]))),
            FreeOnlyVerdict::HasPaid,
            "国服 ds4.1 是计费的（倍率 0.03）→ 消耗是真的，不能清零"
        );
    }

    /// 混用免费与计费模型 → HasPaid，**不做任何削减**。
    ///
    /// 这里刻意不尝试「扣除免费模型那部分」：本地口径只有余额差，
    /// 无法把差额按模型切开（上游账单才有那个粒度）。凭空按比例分摊
    /// 等于编造数字，比「显示原值」更糟。
    #[test]
    fn mixed_usage_is_has_paid() {
        let table = parse_model_billing(&regions_payload());
        assert_eq!(
            free_only_verdict(
                &table,
                Region::Intl,
                Some(&models(&["deepseek-v4.1-flash", "hy3", "intl-fast"]))
            ),
            FreeOnlyVerdict::HasPaid,
        );
    }

    /// 有一个模型判不出来 → 整体 Unknown（不猜）。
    ///
    /// 局部未知不能推出整体免费：那个未知模型可能正是消耗来源。
    #[test]
    fn any_unknown_model_makes_verdict_unknown() {
        let table = parse_model_billing(&regions_payload());
        assert_eq!(
            free_only_verdict(
                &table,
                Region::Intl,
                Some(&models(&["deepseek-v4.1-flash", "cn-auto"]))
            ),
            FreeOnlyVerdict::Unknown,
            "有模型判不出来时不能整体断言免费"
        );
    }

    /// **空集合不是「全部免费」** —— 这条最容易写错（`all()` 在空集合上恒真）。
    ///
    /// 拿不到模型明细时（旧版网关的 accountModels 缺失）必须是 Unknown，
    /// 否则会把有真实消耗的账号整体清零。
    #[test]
    fn empty_or_missing_models_is_unknown_not_all_free() {
        let table = parse_model_billing(&regions_payload());
        assert_eq!(
            free_only_verdict(&table, Region::Intl, Some(&[])),
            FreeOnlyVerdict::Unknown,
            "没有任何模型记录 = 不知道用了什么，不能当成「全部免费」"
        );
        assert_eq!(
            free_only_verdict(&table, Region::Intl, None),
            FreeOnlyVerdict::Unknown,
            "拿不到模型明细 = 不知道，不能清零"
        );
    }

    /// 拿不到上游真值（旧版网关）时，任何账号都不能被判成免费。
    #[test]
    fn no_upstream_truth_never_yields_all_free() {
        let empty = parse_model_billing(&json!({}));
        assert_eq!(
            free_only_verdict(&empty, Region::Intl, Some(&models(&["deepseek-v4.1-flash"]))),
            FreeOnlyVerdict::Unknown,
            "拿不到上游真值只能是不知道 —— 绝不能把消耗清零"
        );
    }

    /// 两区都免费的模型（hy3）：国服账号只用它也是 AllFree。
    ///
    /// 防的是实现走偏成「只有国际版才可能免费」。
    #[test]
    fn cn_account_on_both_region_free_model_is_all_free() {
        let table = parse_model_billing(&regions_payload());
        assert_eq!(
            free_only_verdict(&table, Region::Cn, Some(&models(&["hy3"]))),
            FreeOnlyVerdict::AllFree,
            "hy3 国服也是 x0.00 → 国服账号只用它同样不消耗积分"
        );
    }

    // -----------------------------------------------------------------------
    // 投影改写：把「只用了免费模型」的窗口如实归零
    // -----------------------------------------------------------------------

    /// 构造一份与 `credit_usage.rs::build_statistics` 输出同形的最小 statistics。
    fn statistics_with(accounts: Value) -> Value {
        json!({
            "generatedAt": 0,
            "retentionDays": 60,
            "coverageStartAt": null,
            "comparison": {},
            "summary": {},
            "daily": [],
            "accounts": accounts,
            "events": [],
        })
    }

    fn verdicts(entries: &[(&str, WindowVerdicts)]) -> std::collections::HashMap<String, WindowVerdicts> {
        entries.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn all_free_windows() -> WindowVerdicts {
        WindowVerdicts {
            today: FreeOnlyVerdict::AllFree,
            seven_days: FreeOnlyVerdict::AllFree,
            month: FreeOnlyVerdict::AllFree,
        }
    }

    /// 免费模型账号的三个窗口必须被改成 **0**（而不是「—」/保留原值）。
    ///
    /// 这就是所有者要的那个修复：国际版免费模型的调用不该统计出积分消耗。
    #[test]
    fn free_account_windows_become_zero() {
        let mut stats = statistics_with(json!([
            {"accountId": "intl-free", "usageToday": 350.0, "usage7Days": 1050.0, "usageThisMonth": 1050.0,
             "change": {"decrease": 350.0}, "daily": [{"date": "2026-09-18", "usage": 350.0}]}
        ]));
        zero_out_free_windows(&mut stats, &verdicts(&[("intl-free", all_free_windows())]));

        let acc = &stats["accounts"][0];
        assert_eq!(acc["usageToday"], json!(0.0), "今日消耗必须是确定的 0");
        assert_eq!(acc["usage7Days"], json!(0.0));
        assert_eq!(acc["usageThisMonth"], json!(0.0));

        // 归零的是**积分**窗口。以下三样刻意不动：
        //   change  —— 快照事实（余额确实动过），由另一口径解释；
        //   daily   —— 逐日观察序列，单点归零会让趋势图与合计对不上；
        //   其它字段 —— 一律不碰。
        assert_eq!(acc["change"]["decrease"], json!(350.0), "快照差值不该被改写");
        assert_eq!(acc["daily"][0]["usage"], json!(350.0), "逐日序列不该被改写");

        // 判定结果一并透出，前端才能区分「0 = 确定免费」与「— = 不知道」。
        assert_eq!(acc["billing"]["today"], json!("all_free"));
        assert_eq!(acc["billing"]["allFree"], json!(true));
    }

    /// 计费账号一个字段都不能动。
    #[test]
    fn paid_account_is_untouched() {
        let mut stats = statistics_with(json!([
            {"accountId": "cn-paid", "usageToday": 12.0, "usage7Days": 30.0, "usageThisMonth": 99.0}
        ]));
        let before = stats.clone();
        zero_out_free_windows(&mut stats, &verdicts(&[("cn-paid", WindowVerdicts {
            today: FreeOnlyVerdict::HasPaid,
            seven_days: FreeOnlyVerdict::HasPaid,
            month: FreeOnlyVerdict::HasPaid,
        })]));
        // billing 是新增的说明字段，其余必须逐字不变。
        assert_eq!(stats["accounts"][0]["usageToday"], before["accounts"][0]["usageToday"]);
        assert_eq!(stats["accounts"][0]["usage7Days"], before["accounts"][0]["usage7Days"]);
        assert_eq!(stats["accounts"][0]["usageThisMonth"], before["accounts"][0]["usageThisMonth"]);
        assert_eq!(stats["accounts"][0]["billing"]["today"], json!("has_paid"));
    }

    /// 未知判定同样不动数字 —— 「不知道」既不是 0 也不是 1，是保持原样。
    #[test]
    fn unknown_verdict_keeps_original_numbers() {
        let mut stats = statistics_with(json!([
            {"accountId": "mystery", "usageToday": 7.0, "usage7Days": 8.0, "usageThisMonth": 9.0}
        ]));
        zero_out_free_windows(&mut stats, &verdicts(&[("mystery", WindowVerdicts::default())]));
        let acc = &stats["accounts"][0];
        assert_eq!(acc["usageToday"], json!(7.0), "不知道的时候不能清零");
        assert_eq!(acc["usage7Days"], json!(8.0));
        assert_eq!(acc["usageThisMonth"], json!(9.0));
        assert_eq!(acc["billing"]["today"], json!("unknown"));
        assert_eq!(acc["billing"]["allFree"], json!(false));
    }

    /// **三个窗口分开判定**：今天只跑了免费模型、本月跑过计费模型时，
    /// 只能把「今日」归零，本月必须保留原值。
    ///
    /// 合成一个判定的实现会在这里失败 —— 而它造成的后果是「本月的真实消耗
    /// 被今天的免费结论抹掉」，又一个「统计与实际不符」。
    #[test]
    fn windows_are_judged_independently() {
        let mut stats = statistics_with(json!([
            {"accountId": "mixed-window", "usageToday": 0.0, "usage7Days": 500.0, "usageThisMonth": 900.0}
        ]));
        zero_out_free_windows(&mut stats, &verdicts(&[("mixed-window", WindowVerdicts {
            today: FreeOnlyVerdict::AllFree,
            seven_days: FreeOnlyVerdict::HasPaid,
            month: FreeOnlyVerdict::HasPaid,
        })]));
        let acc = &stats["accounts"][0];
        assert_eq!(acc["usageToday"], json!(0.0), "今日只用免费模型 → 0");
        assert_eq!(acc["usage7Days"], json!(500.0), "近 7 天有计费调用 → 保留");
        assert_eq!(acc["usageThisMonth"], json!(900.0), "本月有计费调用 → 保留");
        assert_eq!(acc["billing"]["allFree"], json!(false), "并非整个窗口都免费");
    }

    /// 判定表里没有这个账号（不在当前账号库 / 拿不到明细）→ 不动它。
    #[test]
    fn account_without_verdict_is_untouched() {
        let mut stats = statistics_with(json!([
            {"accountId": "no-verdict", "usageToday": 5.0, "usage7Days": 5.0, "usageThisMonth": 5.0}
        ]));
        zero_out_free_windows(&mut stats, &verdicts(&[]));
        let acc = &stats["accounts"][0];
        assert_eq!(acc["usageToday"], json!(5.0), "没有判定时不能凭空清零");
        assert!(acc.get("billing").is_none(), "没有判定就不该写 billing 字段");
    }

    /// 形状不对（没有 accounts / accounts 不是数组）时安静返回，不 panic。
    ///
    /// 统计投影是旁路数据，形状异常不该让整个统计接口挂掉。
    #[test]
    fn malformed_statistics_do_not_panic() {
        for mut value in [json!({}), json!({"accounts": "nope"}), json!({"accounts": [1, 2]})] {
            zero_out_free_windows(&mut value, &verdicts(&[("x", all_free_windows())]));
        }
    }

    /// 三个窗口字段名必须与 credit_usage.rs 的输出逐字一致。
    #[test]
    fn usage_window_names_match_the_projection() {
        assert_eq!(USAGE_WINDOWS, ["usageToday", "usage7Days", "usageThisMonth"]);
        let v = all_free_windows();
        assert_eq!(v.of_window("usageToday"), FreeOnlyVerdict::AllFree);
        assert_eq!(v.of_window("usage7Days"), FreeOnlyVerdict::AllFree);
        assert_eq!(v.of_window("usageThisMonth"), FreeOnlyVerdict::AllFree);
        // 不认识的窗口名 → 不知道（不能默认免费）
        assert_eq!(v.of_window("usageAll"), FreeOnlyVerdict::Unknown);
    }

    /// 汇总（summary）必须跟着逐账号归零一起减，否则会出现
    /// 「所有账号行都是 0，顶部合计却不是 0」的自相矛盾。
    #[test]
    fn summary_is_deducted_alongside_accounts() {
        let mut stats = json!({
            "summary": {"usageToday": 350.0, "usage7Days": 1050.0, "usageThisMonth": 1050.0},
            "accounts": [
                {"accountId": "intl-free", "usageToday": 350.0, "usage7Days": 1050.0, "usageThisMonth": 1050.0}
            ],
        });
        zero_out_free_windows(&mut stats, &verdicts(&[("intl-free", all_free_windows())]));
        assert_eq!(stats["summary"]["usageToday"], json!(0.0), "合计要减掉已判定免费的部分");
        assert_eq!(stats["summary"]["usage7Days"], json!(0.0));
        assert_eq!(stats["summary"]["usageThisMonth"], json!(0.0));
    }

    /// 汇总只减掉**被判免费的那些账号**，计费账号的消耗仍留在合计里。
    #[test]
    fn summary_keeps_paid_accounts_contribution() {
        let mut stats = json!({
            "summary": {"usageToday": 100.0, "usage7Days": 100.0, "usageThisMonth": 100.0},
            "accounts": [
                {"accountId": "free", "usageToday": 60.0, "usage7Days": 60.0, "usageThisMonth": 60.0},
                {"accountId": "paid", "usageToday": 40.0, "usage7Days": 40.0, "usageThisMonth": 40.0}
            ],
        });
        zero_out_free_windows(
            &mut stats,
            &verdicts(&[
                ("free", all_free_windows()),
                ("paid", WindowVerdicts {
                    today: FreeOnlyVerdict::HasPaid,
                    seven_days: FreeOnlyVerdict::HasPaid,
                    month: FreeOnlyVerdict::HasPaid,
                }),
            ]),
        );
        assert_eq!(stats["summary"]["usageToday"], json!(40.0), "计费账号的消耗要留着");
        assert_eq!(stats["summary"]["usageThisMonth"], json!(40.0));
        assert_eq!(stats["accounts"][0]["usageToday"], json!(0.0));
        assert_eq!(stats["accounts"][1]["usageToday"], json!(40.0));
    }

    /// 没有判定表（拿不到上游真值）时，汇总**一个数都不能动**。
    #[test]
    fn summary_untouched_without_verdicts() {
        let mut stats = json!({
            "summary": {"usageToday": 77.0},
            "accounts": [{"accountId": "x", "usageToday": 77.0}],
        });
        zero_out_free_windows(&mut stats, &verdicts(&[]));
        assert_eq!(stats["summary"]["usageToday"], json!(77.0));
        assert_eq!(stats["accounts"][0]["usageToday"], json!(77.0));
    }
}
