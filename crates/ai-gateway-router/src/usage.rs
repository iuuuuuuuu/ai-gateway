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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

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
}

/// 内存态。
#[derive(Debug, Default)]
struct Inner {
    days: HashMap<String, Counters>,
    models: HashMap<String, HashMap<String, Counters>>,
    accounts: HashMap<String, HashMap<String, Counters>>,
}

/// 网关 Token 用量聚合器。并发安全；`path` 为空时纯内存（不落盘）。
#[derive(Debug)]
pub struct Stats {
    inner: Mutex<Inner>,
    dirty: AtomicBool,
    path: String,
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
        }
    }

    /// 记录一次成功请求的用量；`uid` / `model` 为空时归一成 `-`。
    pub fn record(&self, uid: &str, model: &str, c: Counters) {
        self.record_at(uid, model, &local_day_now(), c);
    }

    /// 同 [`Stats::record`]，但显式指定日期键（测试用）。
    pub fn record_at(&self, uid: &str, model: &str, day: &str, c: Counters) {
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
        let model = if model.is_empty() { "-" } else { model };
        let uid = if uid.is_empty() { "-" } else { uid };

        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        add_to(&mut inner.days, day, entry);
        add_to(nested(&mut inner.models, model), day, entry);
        add_to(nested(&mut inner.accounts, uid), day, entry);
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// 导出一份可 JSON 序列化的聚合快照。`days <= 0` 表示全部历史。
    pub fn snapshot(&self, days: i64) -> serde_json::Value {
        let Ok(inner) = self.inner.lock() else {
            return serde_json::json!({});
        };
        let now = chrono::Local::now();
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

        serde_json::json!({
            "generatedAt": now.timestamp_millis(),
            "rangeDays": if days > 0 { serde_json::json!(days) } else { serde_json::Value::Null },
            "summary": summary.value(),
            "models": models,
            "accounts": accounts,
            "daily": daily,
            "dailyByModel": serde_json::Value::Object(daily_by_model),
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
            }
        };
        self.dirty.store(false, Ordering::Relaxed);

        let Ok(raw) = serde_json::to_string_pretty(&state) else {
            eprintln!("usage: 序列化失败");
            self.dirty.store(true, Ordering::Relaxed);
            return;
        };
        if let Some(dir) = std::path::Path::new(&self.path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // 原子写：先写 tmp 再 rename，避免进程中断留下半截文件。
        let tmp = format!("{}.tmp", self.path);
        if let Err(e) = std::fs::write(&tmp, raw) {
            eprintln!("usage: 落盘失败: {e}");
            self.dirty.store(true, Ordering::Relaxed);
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            eprintln!("usage: 落盘失败: {e}");
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
        eprintln!("usage: 解析 {path} 失败，忽略");
        return Inner::default();
    };
    if fs.version > 1 {
        eprintln!("usage: {path} 版本 {} 高于当前支持，忽略", fs.version);
        return Inner::default();
    }
    Inner {
        days: fs.days,
        models: fs.models,
        accounts: fs.accounts,
    }
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
}
