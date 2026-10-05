//! Trae 积分消耗历史：直连官方会话级用量接口，按本地自然日聚合。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/commands/usage_history.rs`
//! （`usage_history_fetch`），按本仓库「core 不依赖 Tauri」的约定改写。
//!
//! ## 为什么不能只靠余额差值推算消耗
//!
//! 积分看板此前的「消耗」口径是每日快照的余额差值。这个口径把**所有**余额变动
//! 都算成消耗：签到补发、活动赠送、订阅重置都会让余额上升，差值法只能把它们
//! 当成负消耗（或直接抹掉），用户看到的「今天消耗 0」其实是「今天签到补了 200」。
//!
//! 本模块改为直连 `query_user_usage_group_by_session`，拿的是**会话级真实扣费**
//! （`credits_float`）与模型分布，因此：
//!
//! - 消耗只统计真正的扣费，不受签到/赠送干扰；
//! - 能按模型拆分（哪个模型在烧积分）；
//! - 能带上 token 明细（输入 / 输出 / 缓存命中）。
//!
//! ## 增量语义（避免重复计数）
//!
//! - 无缓存：全量拉近一年；
//! - 有缓存且 `fresh`：从「上次拉取的 end_time 所在本地日 00:00」起重拉，
//!   并**替换**该日及之后的日聚合 —— 当天多次拉取不叠加，更早的历史原样保留；
//! - `fresh = false`：纯缓存读取，零网络请求。
//!
//! 替换而不是追加是关键：不替换的话，同一天拉两次就把当天消耗记了两遍。

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::modules::{config, trae_account};

/// 会话级用量接口（`usage_type: [7]` = Cloud-IDE 会话积分消耗）。
pub const USAGE_URL: &str =
    "https://api.trae.cn/trae/api/v1/pay/query_user_usage_group_by_session";

/// 首次拉取的回看窗口（天）。
const FULL_PULL_DAYS: i64 = 365;
/// 单次请求的区间上限（天）。对齐官方控制台粒度，过大会被参数校验拒绝。
const CHUNK_DAYS: i64 = 30;
/// 单块分页安全上限：防止 `total` 异常导致死循环。
const MAX_PAGES: u32 = 50;
/// 单页大小（对齐官方控制台实测值）。
const PAGE_SIZE: u32 = 20;
/// 用量类型：7 = Cloud-IDE 会话积分消耗。
const USAGE_TYPE: i64 = 7;
/// 历史保留天数（与积分快照一致，便于两张图对齐时间轴）。
pub const USAGE_KEEP_DAYS: i64 = 90;

/// 单日聚合。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageDayStat {
    pub date: String,
    /// 当日真实扣费合计。
    pub credits: f64,
    /// 当日会话数。
    pub sessions: u64,
    /// 模型 → 扣费（按扣费降序由上层排序）。
    pub models: BTreeMap<String, f64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}

impl UsageDayStat {
    fn to_json(&self) -> Value {
        json!({
            "date": self.date,
            "credits": self.credits,
            "sessions": self.sessions,
            "models": self.models,
            "inputTokens": self.input_tokens,
            "outputTokens": self.output_tokens,
            "cacheReadTokens": self.cache_read_tokens,
        })
    }
}

fn cache_file() -> std::path::PathBuf {
    config::store_dir().join("trae_usage_history.json")
}

/// 读取缓存（缺失/损坏回退空）。
fn load_cache() -> Value {
    let raw = std::fs::read_to_string(cache_file()).unwrap_or_default();
    let parsed: Value = serde_json::from_str(raw.trim_start_matches('\u{feff}'))
        .unwrap_or_else(|_| json!({ "accounts": {} }));
    if parsed.get("accounts").and_then(Value::as_object).is_some() {
        parsed
    } else {
        json!({ "accounts": {} })
    }
}

fn save_cache(value: &Value) {
    let path = cache_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    let _ = config::atomic_write(&path, &text);
}

/// Unix 秒 → 本地自然日（`YYYY-MM-DD`）。
///
/// 用本地时区：用户看的是自己日历上的「今天消耗了多少」。
pub fn local_date_of(ts: i64) -> Option<String> {
    use chrono::TimeZone;
    let dt = chrono::Local.timestamp_opt(ts, 0).single()?;
    Some(dt.format("%Y-%m-%d").to_string())
}

/// 本地某天 00:00 的 Unix 秒。
fn local_day_start_ts(date: &str) -> Option<i64> {
    use chrono::TimeZone;
    let naive = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let dt = naive.and_hms_opt(0, 0, 0)?;
    chrono::Local
        .from_local_datetime(&dt)
        .single()
        .map(|d| d.timestamp())
}

/// 解析单个会话记录并累加进 `agg`（纯函数，便于测试）。
///
/// 返回是否成功计入（`usage_time` 或日期不可解析时返回 false）。
pub fn accumulate_session(agg: &mut BTreeMap<String, UsageDayStat>, session: &Value) -> bool {
    let ts = session.get("usage_time").and_then(Value::as_i64).unwrap_or(0);
    if ts <= 0 {
        return false;
    }
    let Some(date) = local_date_of(ts) else {
        return false;
    };
    let credits = session
        .get("credits_float")
        .and_then(Value::as_f64)
        // 部分响应把积分放在字符串里
        .or_else(|| {
            session
                .get("credits_float")
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<f64>().ok())
        })
        .unwrap_or(0.0);
    let model = session
        .get("model_name")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("未知模型")
        .to_string();

    let entry = agg.entry(date.clone()).or_insert_with(|| UsageDayStat {
        date,
        ..Default::default()
    });
    entry.credits += credits;
    entry.sessions += 1;
    *entry.models.entry(model).or_insert(0.0) += credits;

    let extra = session.get("extra_info");
    let get = |key: &str| -> u64 {
        extra
            .and_then(|e| e.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    entry.input_tokens += get("input_token");
    entry.output_tokens += get("output_token");
    entry.cache_read_tokens += get("cache_read_token");
    true
}

fn agent() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))
}

/// 会话用量接口的请求头。
///
/// 与积分接口不同，这个接口实测**不需要**设备指纹头（走的是网页控制台同一套接口），
/// 但必须带 `origin` / `referer` —— 缺了会被判定为非官方来源。
fn usage_headers(jwt: &str) -> Vec<(String, String)> {
    let auth = if jwt.starts_with("Cloud-IDE-JWT ") {
        jwt.to_string()
    } else {
        format!("Cloud-IDE-JWT {}", jwt.trim())
    };
    vec![
        ("authorization".to_string(), auth),
        ("content-type".to_string(), "application/json".to_string()),
        (
            "accept".to_string(),
            "application/json, text/plain, */*".to_string(),
        ),
        ("accept-language".to_string(), "zh-CN,zh;q=0.9".to_string()),
        ("origin".to_string(), "https://www.trae.cn".to_string()),
        ("referer".to_string(), "https://www.trae.cn/".to_string()),
        ("sec-fetch-dest".to_string(), "empty".to_string()),
        ("sec-fetch-mode".to_string(), "cors".to_string()),
        ("sec-fetch-site".to_string(), "same-site".to_string()),
    ]
}

async fn post_usage(jwt: &str, body: Value) -> Result<Value, String> {
    let mut req = agent()?.post(USAGE_URL).json(&body);
    for (k, v) in usage_headers(jwt) {
        req = req.header(k, v);
    }
    let resp = req.send().await.map_err(|e| format!("请求失败: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "HTTP {status}: {}",
            text.chars().take(200).collect::<String>()
        ));
    }
    serde_json::from_str(&text).map_err(|e| {
        format!(
            "响应不是 JSON: {e}（原始前 200 字: {}）",
            text.chars().take(200).collect::<String>()
        )
    })
}

/// 拉取单个分块（≤30 天）并分页聚合。
async fn fetch_chunk(
    jwt: &str,
    start_ts: i64,
    end_ts: i64,
    agg: &mut BTreeMap<String, UsageDayStat>,
) -> Result<(), String> {
    let mut page = 1u32;
    let mut got = 0usize;
    let mut total: Option<usize> = None;

    loop {
        let body = json!({
            "start_time": start_ts,
            "end_time": end_ts,
            "page_size": PAGE_SIZE,
            "page_num": page,
            "usage_type": [USAGE_TYPE],
        });
        let resp = post_usage(jwt, body).await?;
        if total.is_none() {
            total = Some(resp.get("total").and_then(Value::as_u64).unwrap_or(0) as usize);
        }
        let arr = resp
            .get("user_usage_group_by_sessions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if arr.is_empty() {
            break;
        }
        for s in &arr {
            accumulate_session(agg, s);
        }
        got += arr.len();

        // 三个终止条件缺一不可：
        // 1) 单页不满 —— 没有下一页了；
        // 2) 已达 total —— 拉完了；
        // 3) 页数上限 —— total 异常大时防死循环。
        if arr.len() < PAGE_SIZE as usize || total.is_some_and(|t| got >= t) {
            break;
        }
        page += 1;
        if page > MAX_PAGES {
            break;
        }
    }
    Ok(())
}

/// 拉取 `[start_ts, end_ts]` 并按本地日聚合，大区间按 30 天分块。
///
/// 分块边界**无缝不重叠**：`chunk_end = chunk_start - 1`，否则相邻两块会各算一次
/// 边界那一秒的会话，消耗被重复计入。
pub async fn fetch_account_usage(
    jwt: &str,
    start_ts: i64,
    end_ts: i64,
) -> Result<BTreeMap<String, UsageDayStat>, String> {
    let mut agg: BTreeMap<String, UsageDayStat> = BTreeMap::new();
    let mut chunk_end = end_ts;
    loop {
        let chunk_start = (chunk_end - CHUNK_DAYS * 86400 + 1).max(start_ts);
        fetch_chunk(jwt, chunk_start, chunk_end, &mut agg).await?;
        if chunk_start <= start_ts {
            break;
        }
        chunk_end = chunk_start - 1;
    }
    Ok(agg)
}

/// 把日聚合写成 JSON 数组（按日期升序），并裁掉保留期之外的天。
fn daily_to_json(map: &BTreeMap<String, Value>) -> Vec<Value> {
    let cutoff = (chrono::Local::now() - chrono::Duration::days(USAGE_KEEP_DAYS))
        .format("%Y-%m-%d")
        .to_string();
    map.iter()
        .filter(|(d, _)| d.as_str() >= cutoff.as_str())
        .map(|(_, v)| v.clone())
        .collect()
}

/// 缓存里某账号的日聚合（`date → stat`）。
fn cached_daily(cache: &Value, uid: &str) -> BTreeMap<String, Value> {
    cache
        .get("accounts")
        .and_then(|a| a.get(uid))
        .and_then(|a| a.get("daily"))
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

fn last_fetch_end(cache: &Value, uid: &str) -> Option<i64> {
    cache
        .get("accounts")
        .and_then(|a| a.get(uid))
        .and_then(|a| a.get("lastFetchEndTs"))
        .and_then(Value::as_i64)
}

/// 拉取（或纯读缓存）全部 Trae 账号的积分消耗历史。
///
/// `fresh = false` 时零网络请求，只回缓存 —— 界面切页时不该每次都打上游。
pub async fn fetch_usage_history(fresh: bool) -> Value {
    let accounts = trae_account::load_accounts();
    let mut cache = load_cache();
    let now_ts = config::now_secs();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    let mut out_accounts: Vec<Value> = Vec::new();
    let mut any_fetched = false;

    for acc in &accounts {
        let uid = acc
            .get("user_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let name = trae_account::display_name(acc);
        let jwt = acc.get("jwt").and_then(Value::as_str).unwrap_or("").to_string();

        let mut daily = cached_daily(&cache, &uid);
        let mut ok = true;
        let mut error: Option<String> = None;

        // 无 JWT 的占位账号（本机发现入池但还没抓到凭证）跳过网络请求
        if fresh && !jwt.trim().is_empty() {
            // 增量起点：上次拉取 end_time 所在本地日的 00:00。
            // 回到当天 00:00 而不是精确到秒：当天已经产生的会话要重拉一遍，
            // 否则「上午拉了、下午再拉」会漏掉上午那批的后续增量。
            let start_ts = match last_fetch_end(&cache, &uid) {
                Some(prev) => local_date_of(prev)
                    .and_then(|d| local_day_start_ts(&d))
                    .unwrap_or(now_ts - FULL_PULL_DAYS * 86400),
                None => now_ts - FULL_PULL_DAYS * 86400,
            };
            match fetch_account_usage(&jwt, start_ts, now_ts).await {
                Ok(fetched) => {
                    any_fetched = true;
                    // 替换该日及之后：更早的历史保持不动（不重复计数）
                    let replace_from = local_date_of(start_ts).unwrap_or_else(|| today.clone());
                    daily.retain(|d, _| d.as_str() < replace_from.as_str());
                    for (date, stat) in fetched {
                        daily.insert(date, stat.to_json());
                    }
                }
                Err(e) => {
                    ok = false;
                    // 有缓存就沿用，把失败原因作为提示带上
                    error = Some(if daily.is_empty() {
                        e
                    } else {
                        format!("本次增量拉取失败，展示的是缓存数据：{e}")
                    });
                }
            }
        } else if jwt.trim().is_empty() {
            ok = false;
            error = Some("账号没有 JWT，无法查询消耗".to_string());
        }

        // 回写缓存（含失败时的 lastFetchEndTs 保持原值）
        if any_fetched && ok {
            if let Some(accounts) = cache.get_mut("accounts").and_then(Value::as_object_mut) {
                let entry = accounts
                    .entry(uid.clone())
                    .or_insert_with(|| json!({ "daily": {} }));
                if let Some(obj) = entry.as_object_mut() {
                    obj.insert("name".to_string(), json!(name));
                    obj.insert("lastFetchEndTs".to_string(), json!(now_ts));
                    obj.insert(
                        "daily".to_string(),
                        Value::Object(daily.clone().into_iter().collect()),
                    );
                }
            }
        }

        let daily_arr = daily_to_json(&daily);
        // 汇总：区间内总消耗 + 模型排行
        let mut total = 0.0f64;
        let mut sessions = 0u64;
        let mut models: BTreeMap<String, f64> = BTreeMap::new();
        for v in &daily_arr {
            total += v.get("credits").and_then(Value::as_f64).unwrap_or(0.0);
            sessions += v.get("sessions").and_then(Value::as_u64).unwrap_or(0);
            if let Some(m) = v.get("models").and_then(Value::as_object) {
                for (k, val) in m {
                    *models.entry(k.clone()).or_insert(0.0) += val.as_f64().unwrap_or(0.0);
                }
            }
        }
        let mut ranking: Vec<(String, f64)> = models.into_iter().collect();
        ranking.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        out_accounts.push(json!({
            "userId": uid,
            "name": name,
            "ok": ok,
            "error": error,
            "daily": daily_arr,
            "totalCredits": total,
            "sessions": sessions,
            "models": ranking.into_iter().map(|(k, v)| json!({ "model": k, "credits": v })).collect::<Vec<_>>(),
        }));
    }

    if any_fetched {
        if let Some(obj) = cache.as_object_mut() {
            obj.insert("fetchedAt".to_string(), json!(now_ts));
        }
        save_cache(&cache);
    }

    json!({
        "fetchedAt": now_ts,
        // true = 纯缓存读取（本次没发起任何网络请求）
        "cached": !fresh,
        "accounts": out_accounts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::config::test_isolation::Isolated;

    fn seed(tag: &str) -> Isolated {
        Isolated::new(tag)
    }

    #[test]
    fn 会话按本地日聚合() {
        let mut agg = BTreeMap::new();
        // 两个会话落在同一天（2026-09-13 本地时间附近）
        let base = local_day_start_ts("2026-09-13").expect("算当天 00:00") + 3600;
        accumulate_session(
            &mut agg,
            &json!({
                "usage_time": base,
                "credits_float": 1.5,
                "model_name": "claude-sonnet",
                "extra_info": { "input_token": 100, "output_token": 50, "cache_read_token": 10 }
            }),
        );
        accumulate_session(
            &mut agg,
            &json!({
                "usage_time": base + 60,
                "credits_float": 0.5,
                "model_name": "gpt-5",
                "extra_info": { "input_token": 20, "output_token": 5 }
            }),
        );

        let day = agg.get("2026-09-13").expect("应有 2026-09-13");
        assert_eq!(day.sessions, 2);
        assert!((day.credits - 2.0).abs() < 1e-9, "扣费应累加：{}", day.credits);
        assert!((day.models["claude-sonnet"] - 1.5).abs() < 1e-9);
        assert!((day.models["gpt-5"] - 0.5).abs() < 1e-9);
        assert_eq!(day.input_tokens, 120);
        assert_eq!(day.output_tokens, 55);
        assert_eq!(day.cache_read_tokens, 10);
    }

    #[test]
    fn 跨天的会话分到各自那天() {
        let mut agg = BTreeMap::new();
        let d1 = local_day_start_ts("2026-03-01").unwrap() + 3600;
        let d2 = local_day_start_ts("2026-03-02").unwrap() + 3600;
        accumulate_session(&mut agg, &json!({ "usage_time": d1, "credits_float": 1.0, "model_name": "m" }));
        accumulate_session(&mut agg, &json!({ "usage_time": d2, "credits_float": 2.0, "model_name": "m" }));
        assert_eq!(agg.len(), 2);
        assert!((agg["2026-03-01"].credits - 1.0).abs() < 1e-9);
        assert!((agg["2026-03-02"].credits - 2.0).abs() < 1e-9);
    }

    #[test]
    fn 时间戳缺失或非正的会话被跳过() {
        let mut agg = BTreeMap::new();
        assert!(!accumulate_session(&mut agg, &json!({ "credits_float": 5.0 })));
        assert!(!accumulate_session(&mut agg, &json!({ "usage_time": 0, "credits_float": 5.0 })));
        assert!(!accumulate_session(&mut agg, &json!({ "usage_time": -1, "credits_float": 5.0 })));
        assert!(agg.is_empty(), "不该凭空造出日期");
    }

    #[test]
    fn 缺模型名归到未知而不是丢掉() {
        let mut agg = BTreeMap::new();
        let ts = local_day_start_ts("2026-04-01").unwrap() + 100;
        accumulate_session(&mut agg, &json!({ "usage_time": ts, "credits_float": 3.0 }));
        let day = &agg["2026-04-01"];
        assert_eq!(day.sessions, 1, "缺模型名的会话仍要计入消耗");
        assert!((day.models["未知模型"] - 3.0).abs() < 1e-9);
    }

    #[test]
    fn 积分为字符串时也能解析() {
        let mut agg = BTreeMap::new();
        let ts = local_day_start_ts("2026-04-02").unwrap() + 100;
        accumulate_session(
            &mut agg,
            &json!({ "usage_time": ts, "credits_float": "2.25", "model_name": "m" }),
        );
        assert!((agg["2026-04-02"].credits - 2.25).abs() < 1e-9);
    }

    #[test]
    fn 保留期之外的天被裁掉() {
        let _iso = seed("usage_history_trim");
        let mut map: BTreeMap<String, Value> = BTreeMap::new();
        map.insert(
            "2000-01-01".to_string(),
            json!({ "date": "2000-01-01", "credits": 9.0, "sessions": 1, "models": {} }),
        );
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        map.insert(
            today.clone(),
            json!({ "date": today, "credits": 1.0, "sessions": 1, "models": {} }),
        );
        let out = daily_to_json(&map);
        assert_eq!(out.len(), 1, "只应留下保留期内的那天：{out:?}");
        assert_eq!(out[0]["date"], json!(today));
    }

    #[test]
    fn 缓存损坏时按空缓存处理() {
        let _iso = seed("usage_history_broken");
        let path = cache_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        let cache = load_cache();
        assert!(cache["accounts"].as_object().unwrap().is_empty());

        // 结构不对（缺 accounts）也要能恢复
        std::fs::write(&path, json!({ "oops": 1 }).to_string()).unwrap();
        assert!(load_cache()["accounts"].as_object().unwrap().is_empty());
    }

    #[test]
    fn 纯缓存读取不发起网络请求() {
        let _iso = seed("usage_history_cached_only");
        // 没有任何账号：即便 fresh=false 也必须正常返回空结构而不是报错
        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt.block_on(fetch_usage_history(false));
        assert_eq!(out["cached"], json!(true));
        assert_eq!(out["accounts"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn 无_jwt_的占位账号被标为不可用() {
        let _iso = seed("usage_history_no_jwt");
        // 造一个没有 jwt 的账号
        let mut accounts = crate::modules::trae_account::load_accounts();
        accounts.push(json!({ "user_id": "999000111", "name": "占位" }));
        crate::modules::trae_account::save_accounts(&accounts).expect("写账号库");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt.block_on(fetch_usage_history(true));
        let accs = out["accounts"].as_array().unwrap();
        assert_eq!(accs.len(), 1);
        assert_eq!(accs[0]["ok"], json!(false));
        assert!(
            accs[0]["error"].as_str().unwrap().contains("没有 JWT"),
            "应说明缺 JWT：{}",
            accs[0]["error"]
        );
    }

    #[test]
    fn 本地日与当天起点互为逆运算() {
        let d = "2026-07-15";
        let ts = local_day_start_ts(d).expect("解析当天 00:00");
        assert_eq!(local_date_of(ts).as_deref(), Some(d));
        // 当天最后一秒仍属同一天
        assert_eq!(local_date_of(ts + 86399).as_deref(), Some(d));
        // 次日 00:00 归次日
        assert_eq!(local_date_of(ts + 86400).as_deref(), Some("2026-07-16"));
    }
}
