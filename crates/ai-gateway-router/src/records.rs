//! 账号记录回写（对应 Go `internal/records/records.go`）。
//!
//! 养号任务的日志只写 stdout，而宿主启动子进程时把 stdout/stderr 丢进了
//! `Stdio::null` —— 界面上一条执行痕迹都没有。改为写宿主已经在读的
//! `account_records.json`，记录就能与签到并列显示。
//!
//! # 设计要点（与 Go 版逐条对齐）
//!
//! 1. **同一个文件、同一种结构**：字段名逐字对齐宿主侧 `AccountRecord`
//!    （`ts` / `accountId` / `accountName` / `kind` / `title` / `result` /
//!    `amount` / `detail`）。宿主按 `accountId` / `kind` / `ts` 取值，任一处
//!    拼错都**不会报错** —— 记录只是安静地不出现在界面上，正是本模块要修
//!    的那个症状。
//! 2. **写入绝不失败主流程**：三个 `task*` 方法都返回 `()`，任何 I/O 与解析
//!    失败只打一行 `eprintln!`。任务照跑。
//! 3. **原子写**（临时文件 + rename，带重试）：宿主用 serde_json 解析
//!    **整个**文件，解析失败就退化成空集，随后宿主的写入会把历史整体覆盖。
//! 4. **路径只有一个来源**：`account_records.json` 的绝对路径由宿主经 native
//!    config 透传，本模块不自己拼路径；未配置时 [`Recorder::enabled`] 为
//!    `false`，一切写入静默降级（独立跑 gateway.exe 的场景）。
//! 5. **既有记录按原始字节透传**：读回的是 [`RawValue`]，只做词法重排（等价
//!    Go 的 `json.Indent`），因此宿主的未知字段（`source` / `backfilled` / …）
//!    一个比特都不动，13 位毫秒时间戳也不会经 `f64` 中转丢精度。**不能**改成
//!    「解析成 `Value` 再 `to_string_pretty`」：那会按字典序重排键、并把
//!    `1e5` 这类数字原文改写掉。

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 记录条数上限，与宿主 `account_records::ACCOUNT_RECORD_MAX` 同一口径。
///
/// 超出后截掉**最旧**的（保留最新的），与宿主的 `normalize_records` 一致。
pub const MAX_RECORDS: usize = 20_000;

/// 保留天数默认值，与宿主 `config::RECORD_RETENTION_DEFAULT_DAYS` 同一口径。
///
/// 配置里 `retention_days <= 0` 时回落到这个值（**不是**清空）。
pub const DEFAULT_RETENTION_DAYS: i64 = 60;

/// 本模块只写任务类记录（对应 Go `KindTask`）。
pub const KIND_TASK: &str = "task";

/// 任务成功（对应 Go `ResultSuccess`，也与宿主 `RESULT_SUCCESS` 一致）。
pub const RESULT_SUCCESS: &str = "success";
/// 任务失败（对应 Go `ResultFailed`，也与宿主 `RESULT_FAILED` 一致）。
pub const RESULT_FAILED: &str = "failed";
/// 任务此前已完成、本次跳过（对应 Go `ResultAlready`）。
pub const RESULT_ALREADY: &str = "already";
/// 纯告知类结果（对应 Go `ResultInfo`）。
pub const RESULT_INFO: &str = "info";

/// 账号身份映射的一项：网关手上只有 `uid`，而界面按账号库的 `id` 过滤记录。
///
/// 必须由宿主透传：网关凭证里没有账号库的 uuid，自己造一个界面就查不到。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    /// 宿主账号库里的 id（uuid）。
    pub id: String,
    /// 界面展示名。
    pub name: String,
}

impl Identity {
    /// 便捷构造，省得调用方写结构体字面量。
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Identity {
            id: id.into(),
            name: name.into(),
        }
    }
}

/// 写进文件的记录。字段顺序与 Go `record` 一致（serde 按声明顺序输出，因此
/// 本模块产出的键序与 Go 版逐字相同）。
///
/// 刻意**没有** `source` 字段：网关只写 task 记录，`source` 只对积分变化有
/// 意义；宿主的既有记录靠 [`RawValue`] 原样透传，不会被这里抹掉。
#[derive(Debug, Clone, Serialize)]
struct Record {
    ts: i64,
    #[serde(rename = "accountId")]
    account_id: String,
    #[serde(rename = "accountName")]
    account_name: String,
    kind: String,
    title: String,
    result: String,
    amount: i64,
    detail: String,
}

/// 从既有记录里取 `ts` 的探针（对应 Go `tsOf` 里的匿名结构体）。
#[derive(Deserialize)]
struct TsProbe {
    #[serde(default)]
    ts: i64,
}

/// 逐条比对「今天是否已经写过」时用的探针（对应 Go 的 `record`）。
///
/// 全字段带默认值：Go 的 `json.Unmarshal` 对**缺失**字段不报错（留零值），
/// 只有**类型不符**才报错 —— `#[serde(default)]` 精确复刻这一点。
///
/// `account_name` / `kind` / `amount` / `detail` 不参与判定，但**必须留着**：
/// Go 是把整条记录反序列化成 `record` 再比对的，任一字段类型不符（例如
/// `"amount": "abc"`）都会让这条记录整体解析失败、被 `continue` 跳过。删掉这些
/// 字段就等于放宽了类型校验，去重结果会与 Go 侧不一致。
#[derive(Deserialize)]
#[allow(dead_code)]
struct RecordProbe {
    #[serde(default)]
    ts: i64,
    #[serde(rename = "accountId", default)]
    account_id: String,
    #[serde(rename = "accountName", default)]
    account_name: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    result: String,
    #[serde(default)]
    amount: i64,
    #[serde(default)]
    detail: String,
}

/// 账号记录回写器（对应 Go `Recorder`）。
///
/// 内部用 [`std::sync::Mutex`] 串行化「读回 → 追加 → 裁剪 → 原子写」这一整段，
/// 因此可以安全地跨线程共享（`Send + Sync`）。
///
/// Go 侧的 `*Recorder` 空指针语义在 Rust 侧由调用方的 `Option<Arc<Recorder>>`
/// 承担，所以这里不需要 `Clone`。
pub struct Recorder {
    /// 目标文件绝对路径；已 `trim`。空串表示未配置 ⇒ 一切写入静默降级。
    path: String,
    retention_days: i64,
    identities: HashMap<String, Identity>,
    /// 时钟注入点（毫秒）。Go 是 `now func() time.Time`，同样**不进** `new`
    /// 的参数表，只在 [`Recorder::with_clock`] 里暴露给测试。
    clock: Box<dyn Fn() -> i64 + Send + Sync>,
    /// 只保护文件的读改写，不保护上面那些构造后即只读的字段。
    mu: Mutex<()>,
}

impl Recorder {
    /// 建一个回写器。`path` 为空（或只有空白）时得到「未启用」的实例。
    ///
    /// `retention_days <= 0` 回落到 [`DEFAULT_RETENTION_DAYS`]，与 Go `New` 一致。
    pub fn new(path: &str, retention_days: i64, identities: HashMap<String, Identity>) -> Self {
        Self::with_clock(path, retention_days, identities, Box::new(system_now_ms))
    }

    /// 带时钟注入的构造（Go `Recorder.now` 字段的等价物，供测试固定时间）。
    pub fn with_clock(
        path: &str,
        retention_days: i64,
        identities: HashMap<String, Identity>,
        clock: Box<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Recorder {
            path: path.trim().to_string(),
            retention_days: if retention_days <= 0 {
                DEFAULT_RETENTION_DAYS
            } else {
                retention_days
            },
            identities,
            clock,
            mu: Mutex::new(()),
        }
    }

    /// 是否已配置目标文件。未配置时所有写入都是空操作。
    pub fn enabled(&self) -> bool {
        !self.path.is_empty()
    }

    /// 目标文件路径（已 `trim`）；未配置时是空串。
    pub fn path(&self) -> &str {
        &self.path
    }

    /// 账号身份映射的**副本**（调用方多在并发路径上，不能把内部 map 借出去）。
    pub fn identity_index(&self) -> HashMap<String, Identity> {
        self.identities.clone()
    }

    /// 写一条普通任务记录：每次都写，不做同日去重。
    pub fn task(&self, uid: &str, title: &str, result: &str, detail: &str) {
        self.append(uid, title, result, detail, false);
    }

    /// 写一条「每天最多一条」的任务记录。
    ///
    /// 去重键是 `(accountId, title, result, 当天)`，**含 result**：所以
    /// 「今天先失败、重试后成功」两条都会留下 —— 这正是排查时想看的信息。
    pub fn task_daily(&self, uid: &str, title: &str, result: &str, detail: &str) {
        self.append(uid, title, result, detail, true);
    }

    /// 写一条与具体账号无关的汇总记录（`accountId` 为空、`accountName` 为
    /// 「全部账号」），同样每天最多一条。
    pub fn task_all_daily(&self, title: &str, result: &str, detail: &str) {
        self.append("", title, result, detail, true);
    }

    /// 三个 `task*` 方法的共同实现。对应 Go `Recorder.append`。
    fn append(&self, uid: &str, title: &str, result: &str, detail: &str, daily: bool) {
        if !self.enabled() {
            return;
        }
        let (account_id, account_name) = self.identity(uid);
        let rec = Record {
            ts: (self.clock)(),
            account_id,
            account_name,
            kind: KIND_TASK.to_string(),
            title: title.to_string(),
            result: result.to_string(),
            amount: 0,
            detail: one_line(detail, 300),
        };

        // 锁中毒（别的线程 panic 在临界区里）不该让记录功能整体失效：取回内部值继续。
        let _guard = self.mu.lock().unwrap_or_else(|e| e.into_inner());

        let (mut all, ok) = self.load();
        if !ok {
            crate::logline!("records: {} 不可解析，按空记录集继续写入", self.path);
        }
        if daily && already_today(&all, &rec) {
            return;
        }
        all.push(marshal_record(&rec));
        let kept = normalize(all, rec.ts, self.retention_days);
        if let Err(e) = atomic_write(&self.path, &must_json_array(&kept)) {
            crate::logline!("records: 写入 {} 失败: {e}", self.path);
        }
    }

    /// uid → 宿主的 (id, 展示名)。对应 Go `Recorder.identity`。
    ///
    /// 映射缺失时**退回 uid**：宁可「按 id 查不到」（用户切到「全部账号」还能
    /// 看到），也不要「整条记录消失」。
    fn identity(&self, uid: &str) -> (String, String) {
        if uid.is_empty() {
            return (String::new(), "全部账号".to_string());
        }
        if let Some(id) = self.identities.get(uid) {
            let name = id.name.trim();
            let name = if name.is_empty() { uid } else { name };
            return (id.id.clone(), name.to_string());
        }
        (uid.to_string(), uid.to_string())
    }

    /// 读回既有记录。
    ///
    /// 返回 `(items, ok)`：`ok == false` 表示文件存在但**不可解析**，调用方会
    /// 打一行日志后按空集继续（对应 Go `load`）。文件不存在或全是空白都算正常
    /// （首次写入），`ok == true`。
    fn load(&self) -> (Vec<Box<RawValue>>, bool) {
        let Ok(raw) = std::fs::read(&self.path) else {
            // 读不出来（不存在 / 权限）按「空集」处理，不让记录功能卡住。
            return (Vec::new(), true);
        };
        // 用 lossy 而不是 read_to_string：非法 UTF-8 若被当成「读失败」，就会把
        // 既有记录整体覆盖掉 —— 宁可留几个替换字符也不能丢数据。
        let text = String::from_utf8_lossy(&raw);
        if text.trim().is_empty() {
            return (Vec::new(), true);
        }
        match serde_json::from_str::<Vec<Box<RawValue>>>(&text) {
            Ok(items) => (items, true),
            Err(_) => (Vec::new(), false),
        }
    }
}

/// 系统时钟（毫秒）。对应 Go `time.Now().UnixMilli()`。
fn system_now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 从一条原始记录里取 `ts`；取不到返回 0（对应 Go `tsOf`）。
fn ts_of(item: &RawValue) -> i64 {
    serde_json::from_str::<TsProbe>(item.get())
        .map(|p| p.ts)
        .unwrap_or(0)
}

/// 按天裁剪：丢掉早于 `at_ms - keep_days` 的记录，再截掉最旧的超出部分。
///
/// 对应 Go `normalize`。`keep_days <= 0` 回落到 [`DEFAULT_RETENTION_DAYS`]。
fn normalize(all: Vec<Box<RawValue>>, at_ms: i64, keep_days: i64) -> Vec<Box<RawValue>> {
    let days = if keep_days <= 0 {
        DEFAULT_RETENTION_DAYS
    } else {
        keep_days
    };
    // 用 saturating：Go 的 int64 溢出是回绕，Rust debug 下会 panic。只有荒谬的
    // keep_days（如 i64::MAX）才会走到这里。
    let cutoff = at_ms.saturating_sub(days.saturating_mul(24 * 3600 * 1000));
    let mut kept: Vec<Box<RawValue>> = all
        .into_iter()
        .filter(|item| ts_of(item) >= cutoff)
        .collect();
    if kept.len() > MAX_RECORDS {
        kept.drain(..kept.len() - MAX_RECORDS);
    }
    kept
}

/// 今天是否已经写过同样的记录。对应 Go `alreadyToday`。
///
/// 去重键含 `result`，所以「先失败后成功」两条都保留。
fn already_today(all: &[Box<RawValue>], want: &Record) -> bool {
    let day = cst_day(want.ts);
    all.iter().any(|item| {
        let Ok(probe) = serde_json::from_str::<RecordProbe>(item.get()) else {
            return false;
        };
        probe.account_id == want.account_id
            && probe.title == want.title
            && probe.result == want.result
            && cst_day(probe.ts) == day
    })
}

/// 把毫秒时间戳折成「CST（UTC+8）的第几天」。
///
/// 刻意用 CST 而不是本地时区：夜猫子任务的时间窗判定就是这个口径，两边不一致
/// 会出现「昨天写的今天又写一遍」或反之。对应 Go `cstDay`。
fn cst_day(ms: i64) -> i64 {
    (ms + 8 * 3600 * 1000) / (24 * 3600 * 1000)
}

/// 序列化一条记录。失败兜底为 `{}`（对应 Go `marshalRecord`）：宁可丢这一条，
/// 也不能写坏整个数组 —— 数组坏了宿主会读成空集。
fn marshal_record(rec: &Record) -> Box<RawValue> {
    let text = serde_json::to_string(rec).unwrap_or_else(|_| "{}".to_string());
    RawValue::from_string(text).unwrap_or_else(|_| empty_raw())
}

/// `{}` 一定是合法 JSON，所以这个 expect 走不到。
fn empty_raw() -> Box<RawValue> {
    RawValue::from_string("{}".to_string()).expect("字面量 {} 一定是合法 JSON")
}

/// 把记录拼成宿主能读的 pretty 数组。对应 Go `mustJSONArray`。
///
/// 形状（与宿主 `to_string_pretty` 一致）：
///
/// ```text
/// [
///   {
///     "ts": 1790793517377,
///     …
///   },
///   {
///     …
///   }
/// ]
/// ```
///
/// 每一条都**重新**缩进（[`reindent`]）：读回的原始文本自带上一轮的缩进，原样
/// 拼接再整体缩进一次，缩进就会每轮 +2（2 → 4 → 6 …），几轮后文件形状彻底走样
/// —— 这是端到端实测踩过的真实 bug。
fn must_json_array(items: &[Box<RawValue>]) -> String {
    if items.is_empty() {
        return "[]".to_string();
    }
    let mut out = String::with_capacity(items.len() * 256);
    out.push_str("[\n");
    let mut wrote_any = false;
    for item in items {
        // 单条重排失败就跳过它（对应 Go 里 json.Indent 报错时的 continue）。
        let Some(indented) = reindent(item.get()) else {
            continue;
        };
        out.push_str("  ");
        out.push_str(&indented.replace('\n', "\n  "));
        out.push_str(",\n");
        wrote_any = true;
    }
    if !wrote_any {
        // 理论上走不到（items 来自一次成功的 JSON 解析）。防御一下：绝不能产出
        // `[\n\n]` 这种让宿主整份读不出来的东西（Go 版在这一点上会写坏文件）。
        return "[]".to_string();
    }
    out.truncate(out.len() - 2); // 去掉最后一个 ",\n"
    out.push('\n');
    out.push(']');
    out
}

/// 词法重排：等价于 Go `json.Indent(&mut dst, src, "", "  ")`。
///
/// 为什么不用「解析成 `Value` 再 `to_string_pretty`」：
/// - 键序会被 `BTreeMap` 按字典序重排（Go 侧是原样保留）；
/// - 超出 `i64`/`u64` 的整数会退化成 `f64`，`1e5` 这种写法也会被改写。
///
/// 本函数只按嵌套深度重排空白，其余字节（含未知字段、数字原文、转义写法）逐字
/// 保留。返回 `None` 表示这条不是合法 JSON（调用方跳过它）。
fn reindent(raw: &str) -> Option<String> {
    const INDENT: &str = "  ";
    // 先整体校验一次：等价于 Go 里 json.Indent 对非法输入报错。
    serde_json::from_str::<Box<RawValue>>(raw).ok()?;

    let chars: Vec<char> = raw.trim().chars().collect();
    let mut out = String::with_capacity(raw.len() + 64);
    let mut depth: usize = 0;
    let mut prev_sig: Option<char> = None;
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];

        if c == '"' {
            // 字符串整体逐字拷贝（含转义）：串内的空白不是排版空白。
            out.push(c);
            i += 1;
            while i < chars.len() {
                let ch = chars[i];
                out.push(ch);
                i += 1;
                if ch == '\\' {
                    if i < chars.len() {
                        out.push(chars[i]);
                        i += 1;
                    }
                } else if ch == '"' {
                    break;
                }
            }
            prev_sig = Some('"');
            continue;
        }

        match c {
            '{' | '[' => {
                out.push(c);
                let close = if c == '{' { '}' } else { ']' };
                // 空容器保持紧凑（`{}` / `[]`），与 Go json.Indent 一致。
                if next_significant(&chars, i + 1) != Some(close) {
                    depth += 1;
                    push_break(&mut out, INDENT, depth);
                }
            }
            '}' | ']' => {
                let open = if c == '}' { '{' } else { '[' };
                if prev_sig != Some(open) {
                    depth = depth.saturating_sub(1);
                    push_break(&mut out, INDENT, depth);
                }
                out.push(c);
            }
            ',' => {
                out.push(',');
                push_break(&mut out, INDENT, depth);
            }
            ':' => out.push_str(": "),
            _ => {
                if c.is_whitespace() {
                    i += 1;
                    continue;
                }
                out.push(c);
            }
        }
        prev_sig = Some(c);
        i += 1;
    }
    Some(out)
}

/// 换行并缩进到当前深度。
fn push_break(out: &mut String, indent: &str, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str(indent);
    }
}

/// 从 `from` 起找下一个非空白字符。
fn next_significant(chars: &[char], from: usize) -> Option<char> {
    chars[from.min(chars.len())..]
        .iter()
        .copied()
        .find(|c| !c.is_whitespace())
}

/// 原子写：临时文件 + rename，失败重试 5 次。对应 Go `atomicWrite`。
///
/// 为什么不用 [`crate::auth::atomic_write`]：它用 `path.with_extension("tmp")`，
/// 会把 `account_records.json` 的临时文件写成 `account_records.tmp`，与 Go 版的
/// `path + ".tmp-" + pid` 不同名，也没有这里的重试循环 —— 而重试是必要的：宿主
/// 同时在读/写同一个文件（见 `concurrent_with_host_writer_does_not_corrupt`）。
fn atomic_write(path: &str, content: &str) -> std::io::Result<()> {
    let target = Path::new(path);
    if let Some(dir) = target.parent() {
        // Go: filepath.Dir 对裸文件名返回 "."，此时不建目录。
        if !dir.as_os_str().is_empty() && dir != Path::new(".") {
            std::fs::create_dir_all(dir)?;
        }
    }
    let tmp = PathBuf::from(format!("{path}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, content.as_bytes())?;

    let mut last_err: Option<std::io::Error> = None;
    for attempt in 0..5u64 {
        match std::fs::rename(&tmp, target) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(20 * (attempt + 1)));
            }
        }
    }
    // 重试到底还是失败：清掉临时文件，别在目标目录里留垃圾。
    let _ = std::fs::remove_file(&tmp);
    Err(last_err.unwrap_or_else(|| std::io::Error::other("rename 失败")))
}

/// 压成单行并限长。对应 Go `oneLine`。
///
/// 界面把 `detail` 渲染成一行；带换行的详情会把卡片撑破。
fn one_line(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let replaced = trimmed
        .replace("\r\n", " ")
        .replace('\n', " ")
        .replace('\r', " ")
        .replace('\t', " ");
    let joined = replaced
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let chars: Vec<char> = joined.chars().collect();
    if chars.len() > max {
        let mut out: String = chars[..max].iter().collect();
        out.push('…');
        return out;
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    /// 夹具用的固定「现在」（2026-09-21 前后），把夹具时间戳钉死在保留窗口内，
    /// 免得测试随真实系统时钟漂移而变红。
    const FIXTURE_NOW_MS: i64 = 1_790_000_002_000;

    /// 临时目录守卫：测试结束（含 panic）时自动清理。
    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir_tagged(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{tag}-{}", uuid::Uuid::new_v4()))
    }

    fn ids(pairs: &[(&str, &str, &str)]) -> HashMap<String, Identity> {
        pairs
            .iter()
            .map(|(uid, id, name)| (uid.to_string(), Identity::new(*id, *name)))
            .collect()
    }

    fn fixed_clock(ms: i64) -> Box<dyn Fn() -> i64 + Send + Sync> {
        Box::new(move || ms)
    }

    fn recorder_at(
        retention_days: i64,
        identities: HashMap<String, Identity>,
        clock: Box<dyn Fn() -> i64 + Send + Sync>,
    ) -> (Recorder, PathBuf, TempDir) {
        let dir = temp_dir_tagged("wbswg-records");
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let path = dir.join("account_records.json");
        let rec = Recorder::with_clock(
            path.to_str().expect("临时路径是 UTF-8"),
            retention_days,
            identities,
            clock,
        );
        (rec, path, TempDir(dir))
    }

    fn new_recorder(
        retention_days: i64,
        identities: HashMap<String, Identity>,
    ) -> (Recorder, PathBuf, TempDir) {
        recorder_at(retention_days, identities, Box::new(system_now_ms))
    }

    /// 读回记录文件，并断言它是宿主能解析的 JSON 数组。
    fn read_all(path: &Path) -> Vec<Value> {
        let raw = std::fs::read_to_string(path).expect("读取记录文件失败");
        serde_json::from_str(&raw).unwrap_or_else(|e| {
            panic!("记录文件不是合法 JSON 数组（界面会读不出来）: {e}\n内容: {raw}")
        })
    }

    fn read_text(path: &Path) -> String {
        std::fs::read_to_string(path).expect("读取记录文件失败")
    }

    fn titles(all: &[Value]) -> Vec<String> {
        all.iter()
            .map(|v| v["title"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    #[test]
    fn recorder_is_send_and_sync() {
        // 调用方（scheduler）会把 Recorder 放进 Arc 跨 tokio 任务持有，这里把
        // 约束钉在类型系统上，将来加字段忘了 Send/Sync 会直接编译失败。
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Recorder>();
        assert_send_sync::<Identity>();
    }

    /// 字段名必须与宿主侧 `AccountRecord::to_json` 逐字一致。
    ///
    /// 这是最关键的一条：宿主按 `accountId` / `kind` / `ts` 取值，任一处拼错都
    /// 不会报错 —— 记录会安静地不出现在界面上，正是本 bug 的原症状。
    #[test]
    fn task_writes_host_compatible_shape() {
        let (r, path, _dir) = new_recorder(60, ids(&[("uid-1", "acct-uuid-1", "用户甲")]));
        r.task("uid-1", "活跃上报", RESULT_SUCCESS, "已上报 3/3 条");

        let all = read_all(&path);
        assert_eq!(all.len(), 1, "应写入 1 条记录");
        let got = &all[0];

        assert_eq!(got["accountId"], json!("acct-uuid-1"));
        assert_eq!(got["accountName"], json!("用户甲"));
        assert_eq!(got["kind"], json!("task"));
        assert_eq!(got["title"], json!("活跃上报"));
        assert_eq!(got["result"], json!("success"));
        assert_eq!(got["amount"].as_i64(), Some(0));
        assert_eq!(got["detail"], json!("已上报 3/3 条"));

        // ts 必须是毫秒级（界面按毫秒与日期区间比较，秒级会被判成 1970 年）
        let ts = got["ts"].as_i64().expect("ts 缺失或不是数字");
        assert!(ts >= 1_000_000_000_000, "ts={ts} 不是毫秒时间戳");

        // 不多写字段：界面按 kind 分组，多余顶层字段只会让两边结构漂移
        let obj = got.as_object().expect("记录应是对象");
        assert_eq!(obj.len(), 8, "字段数应为 8（与宿主 AccountRecord 一致）: {got}");
    }

    /// 记录里的 `accountId` 必须是宿主的 uuid，而不是网关的 uid。
    ///
    /// 界面「账号卡片 → 查看记录」按账号库的 id 过滤；写 uid 的话用户点开账号
    /// 永远查不到这些任务，且不会有任何报错。
    #[test]
    fn identity_mapping_uses_host_ids() {
        let (r, path, _dir) = new_recorder(60, ids(&[("gw-uid-9", "host-id-9", "用户乙")]));
        r.task("gw-uid-9", "夜猫子任务", RESULT_SUCCESS, "");

        let got = &read_all(&path)[0];
        assert_eq!(got["accountId"], json!("host-id-9"));
        assert_eq!(got["accountName"], json!("用户乙"));
    }

    /// 宿主尚未同步该账号时退回 uid，记录不丢。
    #[test]
    fn unknown_uid_falls_back_to_uid() {
        let (r, path, _dir) = new_recorder(60, HashMap::new());
        r.task("orphan-uid", "trial 领取", RESULT_FAILED, "boom");

        let got = &read_all(&path)[0];
        assert_eq!(got["accountId"], json!("orphan-uid"));
        assert_eq!(got["accountName"], json!("orphan-uid"));
    }

    /// 未配置路径时静默不写（独立跑 gateway.exe 的场景）：不 panic、不建文件。
    #[test]
    fn disabled_recorder_is_noop() {
        let (r, path, _dir) = new_recorder(60, HashMap::new());
        let disabled = Recorder::new("", 60, HashMap::new());
        assert!(!disabled.enabled());
        disabled.task("u", "t", RESULT_SUCCESS, "d");
        disabled.task_daily("u", "t", RESULT_SUCCESS, "d");
        disabled.task_all_daily("t", RESULT_INFO, "d");
        assert!(!path.exists(), "未配置时不该建文件");

        // 只有空白也算未配置
        assert!(!Recorder::new("   ", 60, HashMap::new()).enabled());

        // 有路径的实例不受影响
        assert!(r.enabled());
        r.task_all_daily("汇总", RESULT_INFO, "照常写");
        assert_eq!(read_all(&path).len(), 1);
    }

    /// 目标路径不可写（父级是个普通文件）时不得 panic —— 任务照跑。
    #[test]
    fn write_failure_does_not_panic() {
        let dir = temp_dir_tagged("wbswg-records-fail");
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let _guard = TempDir(dir.clone());

        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("建占位文件");
        let bad = blocker.join("sub").join("account_records.json");
        let r = Recorder::new(bad.to_str().expect("UTF-8"), 60, HashMap::new());

        r.task("u1", "活跃上报", RESULT_SUCCESS, "x");
        r.task_all_daily("汇总", RESULT_INFO, "y");
        // 走到这里没 panic 即通过；顺带确认没有产出半截文件。
        assert!(!bad.exists());
    }

    /// 宿主积分记录的 `source` 字段必须原样保住。
    #[test]
    fn preserves_host_credit_source_field() {
        let (r, path, _dir) = recorder_at(3650, HashMap::new(), fixed_clock(FIXTURE_NOW_MS));
        std::fs::write(
            &path,
            r#"[
  {
    "ts": 1790000000000,
    "accountId": "a1",
    "accountName": "甲",
    "kind": "credit",
    "title": "积分消耗 · 调用扣减",
    "result": "success",
    "amount": -12,
    "detail": "余额 -12，额度容量不变（纯消耗）",
    "source": "consume"
  },
  {
    "ts": 1790000001000,
    "accountId": "a1",
    "accountName": "甲",
    "kind": "credit",
    "title": "积分到账 · 新增",
    "result": "success",
    "amount": 100,
    "detail": "余额 +100，额度容量 +100（新增积分包到账）",
    "source": "grant"
  }
]"#,
        )
        .expect("预置宿主记录");

        r.task("a1", "活跃上报", RESULT_SUCCESS, "");

        let all = read_all(&path);
        assert_eq!(all.len(), 3, "既有两条 + 新增一条");
        assert_eq!(all[0]["source"], json!("consume"));
        assert_eq!(all[1]["source"], json!("grant"));
        assert_eq!(all[0]["title"], json!("积分消耗 · 调用扣减"));
        assert_eq!(all[0]["detail"], json!("余额 -12，额度容量不变（纯消耗）"));
        assert_eq!(all[1]["detail"], json!("余额 +100，额度容量 +100（新增积分包到账）"));
    }

    /// 老记录没有 `source` 时**不得补出**这个键（前端会渲染成空徽标）。
    #[test]
    fn preserves_legacy_credit_without_source() {
        let (r, path, _dir) = recorder_at(3650, HashMap::new(), fixed_clock(FIXTURE_NOW_MS));
        std::fs::write(
            &path,
            r#"[{"ts": 1790000000000, "accountId": "a1", "kind": "credit", "title": "积分消耗", "result": "success", "amount": -1, "detail": "旧记录"}]"#,
        )
        .expect("预置老记录");

        r.task("a1", "活跃上报", RESULT_SUCCESS, "");

        let all = read_all(&path);
        assert_eq!(all.len(), 2);
        assert!(
            all[0].as_object().expect("对象").get("source").is_none(),
            "不该给没有 source 的老记录补键: {}",
            all[0]
        );
    }

    /// 宿主的未知字段一个都不能丢（`backfilled` 丢了会让签到回填重复导入）。
    #[test]
    fn preserves_existing_records_from_host() {
        let (r, path, _dir) = recorder_at(3650, HashMap::new(), fixed_clock(FIXTURE_NOW_MS));
        std::fs::write(
            &path,
            r#"[{"ts": 1790000000000, "accountId": "a1", "kind": "credit", "title": "积分消耗", "result": "success", "amount": -12, "detail": "d", "backfilled": true, "hostOnlyField": {"nested": 1}}]"#,
        )
        .expect("预置宿主记录");

        r.task("a1", "活跃上报", RESULT_SUCCESS, "");

        let all = read_all(&path);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0]["kind"], json!("credit"));
        assert_eq!(all[0]["title"], json!("积分消耗"));
        assert_eq!(all[0]["backfilled"], json!(true));
        assert_eq!(all[0]["amount"].as_i64(), Some(-12));
        assert_eq!(all[0]["hostOnlyField"]["nested"].as_i64(), Some(1));
    }

    /// 键序与数字原文逐字保留（词法重排的核心证据）。
    ///
    /// 若改成「解析成 Value 再 pretty」，键会变字典序、`1e5` 会被改写成 `100000.0`。
    #[test]
    fn key_order_and_number_text_survive_rewrite() {
        let (r, path, _dir) = recorder_at(3650, HashMap::new(), fixed_clock(FIXTURE_NOW_MS));
        std::fs::write(
            &path,
            r#"[{"ts": 1789540074441, "accountId": "a1", "accountName": "甲", "kind": "credit", "title": "t", "result": "success", "amount": 1e5, "detail": "d"}]"#,
        )
        .expect("预置记录");

        r.task("a1", "活跃上报", RESULT_SUCCESS, "");

        let text = read_text(&path);
        // 键序保持 ts → accountId → …（Go 侧 record 的声明顺序），缩进 4 空格
        assert!(
            text.contains("    \"ts\": 1789540074441,\n    \"accountId\": \"a1\","),
            "键序/缩进被改写:\n{text}"
        );
        // 数字原文不重写（Value 往返会变成 100000.0）
        assert!(text.contains("\"amount\": 1e5"), "数字原文被改写:\n{text}");
    }

    /// 损坏文件按空集恢复并重建，不阻断后续写入。
    #[test]
    fn corrupt_file_is_recovered_not_fatal() {
        let (r, path, _dir) = new_recorder(60, HashMap::new());
        std::fs::write(&path, b"not json at all").expect("预置损坏文件");

        r.task("a1", "活跃上报", RESULT_SUCCESS, "");

        let all = read_all(&path);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0]["title"], json!("活跃上报"));
    }

    /// 空白文件等同空集（不能因为全是空白就报错）。
    #[test]
    fn empty_file_is_treated_as_empty() {
        let (r, path, _dir) = new_recorder(60, HashMap::new());
        std::fs::write(&path, "   \n").expect("预置空白文件");

        r.task("a1", "活跃上报", RESULT_SUCCESS, "");

        assert_eq!(read_all(&path).len(), 1);
    }

    /// 超过保留天数的记录被裁掉。
    #[test]
    fn normalize_drops_expired_records() {
        const DAY: i64 = 24 * 3600 * 1000;
        // 2026-09-16T12:00:00Z
        let now = 1_789_560_000_000i64;
        let (r, path, _dir) = recorder_at(7, HashMap::new(), fixed_clock(now));
        let old = now - 8 * DAY;
        let fresh = now - 6 * DAY;
        std::fs::write(
            &path,
            format!(
                r#"[{{"ts": {old}, "accountId": "a", "kind": "task", "title": "旧", "result": "success", "amount": 0, "detail": ""}},
  {{"ts": {fresh}, "accountId": "a", "kind": "task", "title": "新", "result": "success", "amount": 0, "detail": ""}}]"#
            ),
        )
        .expect("预置记录");

        r.task("a", "活跃上报", RESULT_SUCCESS, "");

        let got = titles(&read_all(&path));
        assert!(!got.contains(&"旧".to_string()), "过期记录应被裁掉: {got:?}");
        assert!(got.contains(&"新".to_string()), "未过期记录应保留: {got:?}");
        assert!(got.contains(&"活跃上报".to_string()), "新写入应在: {got:?}");
    }

    /// 超出上限时截掉最旧的。
    #[test]
    fn normalize_truncates_to_max_keeping_newest() {
        let now = 1_789_540_074_441i64;
        let items: Vec<Box<RawValue>> = (0..MAX_RECORDS + 50)
            .map(|i| {
                marshal_record(&Record {
                    ts: now - MAX_RECORDS as i64 + i as i64,
                    account_id: "a".to_string(),
                    account_name: "a".to_string(),
                    kind: KIND_TASK.to_string(),
                    title: "t".to_string(),
                    result: RESULT_SUCCESS.to_string(),
                    amount: 0,
                    detail: String::new(),
                })
            })
            .collect();

        let kept = normalize(items.clone(), now, 3650);
        assert_eq!(kept.len(), MAX_RECORDS);
        assert_eq!(ts_of(&kept[0]), ts_of(&items[50]), "应截掉最旧的 50 条");
    }

    /// `retention_days <= 0` 回落默认值，而不是把记录清空。
    #[test]
    fn normalize_non_positive_retention_falls_back() {
        let now = system_now_ms();
        let (r, path, _dir) = new_recorder(0, HashMap::new());
        std::fs::write(
            &path,
            format!(
                r#"[{{"ts": {}, "accountId": "a", "kind": "task", "title": "一小时前", "result": "success", "amount": 0, "detail": ""}}]"#,
                now - 3600 * 1000
            ),
        )
        .expect("预置记录");

        r.task("a", "活跃上报", RESULT_SUCCESS, "");

        assert_eq!(read_all(&path).len(), 2, "0 应回落 60 天而不是清空");
    }

    /// `task_daily` 同账号/标题/结果当天只写一条。
    #[test]
    fn task_daily_dedupes_same_day() {
        let (r, path, _dir) = new_recorder(60, HashMap::new());
        for _ in 0..3 {
            r.task_daily("u1", "夜猫子任务", RESULT_INFO, "不在时间窗");
        }
        assert_eq!(read_all(&path).len(), 1);
    }

    /// 去重键含 result：先失败后成功两条都留（排查时想看的信息）。
    #[test]
    fn task_daily_keeps_different_results() {
        let (r, path, _dir) = new_recorder(60, HashMap::new());
        r.task_daily("u1", "夜猫子任务", RESULT_FAILED, "网络抖动");
        r.task_daily("u1", "夜猫子任务", RESULT_SUCCESS, "重试成功");
        assert_eq!(read_all(&path).len(), 2);
    }

    /// 去重按账号隔离。
    #[test]
    fn task_daily_is_per_account() {
        let (r, path, _dir) = new_recorder(60, HashMap::new());
        r.task_daily("u1", "活跃上报", RESULT_SUCCESS, "");
        r.task_daily("u2", "活跃上报", RESULT_SUCCESS, "");
        assert_eq!(read_all(&path).len(), 2);
    }

    /// 跨天后重新写一条。
    #[test]
    fn task_daily_resets_next_day() {
        let now = std::sync::Arc::new(Mutex::new(1_789_540_074_441i64));
        let clock = std::sync::Arc::clone(&now);
        let (r, path, _dir) = recorder_at(
            60,
            HashMap::new(),
            Box::new(move || *clock.lock().unwrap()),
        );

        r.task_daily("u1", "夜猫子任务", RESULT_INFO, "不在时间窗");
        *now.lock().unwrap() += 24 * 3600 * 1000;
        r.task_daily("u1", "夜猫子任务", RESULT_INFO, "不在时间窗");

        assert_eq!(read_all(&path).len(), 2);
    }

    /// 普通 `task` 不做同日去重。
    #[test]
    fn task_always_writes() {
        let (r, path, _dir) = new_recorder(60, HashMap::new());
        r.task("u1", "活跃上报", RESULT_SUCCESS, "第一次");
        r.task("u1", "活跃上报", RESULT_SUCCESS, "第二次");
        assert_eq!(read_all(&path).len(), 2);
    }

    /// 与具体账号无关的汇总记录用占位账号。
    #[test]
    fn task_all_daily_uses_placeholder_account() {
        let (r, path, _dir) = new_recorder(60, HashMap::new());
        r.task_all_daily("开学季活动", RESULT_INFO, "活动已下线");

        let got = &read_all(&path)[0];
        assert_eq!(got["accountId"], json!(""));
        assert_eq!(got["accountName"], json!("全部账号"));
    }

    /// detail 必须压成单行且限长（界面渲染成一行，带换行会撑破卡片）。
    #[test]
    fn detail_is_single_line_and_bounded() {
        let (r, path, _dir) = new_recorder(60, HashMap::new());
        r.task("u1", "t", RESULT_SUCCESS, "line1\nline2\r\n\tline3   with   spaces");

        let all = read_all(&path);
        let detail = all[0]["detail"].as_str().expect("detail 是字符串");
        assert!(!detail.contains('\n'), "不应含换行: {detail:?}");
        assert!(!detail.contains('\r'), "不应含回车: {detail:?}");
        assert!(!detail.contains('\t'), "不应含制表符: {detail:?}");
        assert!(detail.contains("line1 line2 line3"), "空白应压成单空格: {detail:?}");

        r.task("u1", "t", RESULT_SUCCESS, &"中".repeat(1000));
        let all = read_all(&path);
        let last = all.last().expect("末条").clone();
        let detail = last["detail"].as_str().expect("detail 是字符串");
        assert!(
            detail.chars().count() <= 302,
            "限长失效，实际 {} 字符",
            detail.chars().count()
        );
    }

    /// 并发追加：一条不丢、一条不坏。
    #[test]
    fn concurrent_writes_keep_all_records() {
        const WRITERS: usize = 8;
        const PER_WRITER: usize = 25;
        let (r, path, _dir) = new_recorder(60, HashMap::new());
        let r = std::sync::Arc::new(r);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(WRITERS));

        let handles: Vec<_> = (0..WRITERS)
            .map(|w| {
                let r = std::sync::Arc::clone(&r);
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait(); // 同时起跑，尽量制造竞争
                    for i in 0..PER_WRITER {
                        r.task(&format!("uid-{w}"), &format!("任务-{w}-{i}"), RESULT_SUCCESS, "");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("线程不该 panic");
        }

        let all = read_all(&path);
        assert_eq!(all.len(), WRITERS * PER_WRITER, "并发写入丢了记录");
        let mut seen: Vec<String> = Vec::new();
        for rec in &all {
            assert_eq!(rec["kind"], json!("task"));
            let title = rec["title"].as_str().expect("title 非空").to_string();
            assert!(!title.is_empty());
            seen.push(title);
        }
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), WRITERS * PER_WRITER, "出现重复标题");
    }

    /// 并发 `task_daily` 的去重是精确的（锁必须罩住「读回→判定→写入」）。
    #[test]
    fn concurrent_daily_dedupe_is_exact() {
        const THREADS: usize = 16;
        let (r, path, _dir) = new_recorder(60, HashMap::new());
        let r = std::sync::Arc::new(r);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));

        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let r = std::sync::Arc::clone(&r);
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    r.task_daily("u1", "夜猫子任务", RESULT_INFO, "不在时间窗");
                })
            })
            .collect();
        for h in handles {
            h.join().expect("线程不该 panic");
        }

        assert_eq!(read_all(&path).len(), 1, "并发去重失效");
    }

    /// 宿主同时在原子替换同一文件时，我们的写入不能让文件变成坏 JSON。
    ///
    /// 这正是 `atomic_write` 需要重试的原因（Windows 上 rename 会被占用挡住）。
    #[test]
    fn concurrent_with_host_writer_does_not_corrupt() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        const WRITERS: usize = 40;
        let (r, path, _dir) = new_recorder(60, HashMap::new());
        let r = Arc::new(r);
        let stop = Arc::new(AtomicBool::new(false));

        let host_path = path.clone();
        let host_stop = Arc::clone(&stop);
        let host = std::thread::spawn(move || {
            let tmp = PathBuf::from(format!("{}.host-tmp", host_path.display()));
            while !host_stop.load(Ordering::Relaxed) {
                // 模拟宿主：写临时文件再 rename 覆盖（内容不重要，形状才重要）
                if std::fs::write(&tmp, b"[]").is_ok() {
                    let _ = std::fs::rename(&tmp, &host_path);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let _ = std::fs::remove_file(&tmp);
        });

        let handles: Vec<_> = (0..WRITERS)
            .map(|i| {
                let r = Arc::clone(&r);
                std::thread::spawn(move || {
                    r.task(&format!("u{i}"), "活跃上报", RESULT_SUCCESS, "");
                })
            })
            .collect();
        for h in handles {
            h.join().expect("线程不该 panic");
        }
        stop.store(true, Ordering::Relaxed);
        host.join().expect("宿主线程不该 panic");

        // 内容可能被宿主整体覆盖（预期），但绝不能是坏 JSON。
        let raw = read_text(&path);
        let parsed: Result<Value, _> = serde_json::from_str(&raw);
        assert!(
            parsed.map(|v| v.is_array()).unwrap_or(false),
            "文件被写坏了（宿主会读成空集）:\n{raw}"
        );
    }

    /// 不留临时文件。
    #[test]
    fn no_temp_files_left_behind() {
        let (r, path, _dir) = new_recorder(60, HashMap::new());
        r.task("u1", "活跃上报", RESULT_SUCCESS, "");

        let dir = path.parent().expect("有父目录");
        let leftovers: Vec<String> = std::fs::read_dir(dir)
            .expect("读目录")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "残留临时文件: {leftovers:?}");
    }

    /// 目录不存在时自动创建（宿主透传的路径可能还没建出来）。
    #[test]
    fn creates_missing_directories() {
        let dir = temp_dir_tagged("wbswg-records-mkdir");
        let _guard = TempDir(dir.clone());
        let nested = dir.join("a").join("b").join("account_records.json");
        let r = Recorder::new(nested.to_str().expect("UTF-8"), 60, HashMap::new());
        assert!(r.enabled());

        r.task("u1", "活跃上报", RESULT_SUCCESS, "");

        assert!(nested.exists(), "应自动创建多级目录");
        assert_eq!(read_all(&nested).len(), 1);
    }

    /// `enabled()` / `path()` 的口径。
    #[test]
    fn enabled_and_path() {
        let (r, path, _dir) = new_recorder(60, HashMap::new());
        assert!(r.enabled());
        assert_eq!(r.path(), path.to_str().expect("UTF-8"));

        // 空白路径视同未配置，且首尾空白会被 trim
        assert!(!Recorder::new("   ", 60, HashMap::new()).enabled());
        let padded = Recorder::new("  C:\\x\\account_records.json  ", 60, HashMap::new());
        assert_eq!(padded.path(), "C:\\x\\account_records.json");
    }

    /// CST 天数折算与夜猫子时间窗同口径。
    #[test]
    fn cst_day_matches_night_owl_window() {
        const HOUR: i64 = 3600 * 1000;
        // 2026-09-15T16:00:00Z == CST 2026-09-16 00:00
        let base = 1_789_488_000_000i64;
        assert_eq!(cst_day(base), cst_day(base + 23 * HOUR + 59 * 60 * 1000));
        assert_ne!(cst_day(base), cst_day(base + 24 * HOUR));
        // 差一毫秒跨天
        assert_ne!(cst_day(base - 1), cst_day(base));
    }

    /// 与宿主 `serde_json` 的编码形状一致：不转义 HTML、pretty 数组、整体缩进。
    #[test]
    fn encoding_matches_serde_json_shape() {
        let (r, path, _dir) = new_recorder(60, HashMap::new());
        // 刻意放入 serde_json 不转义、而 Go 默认会转义的字符
        r.task(
            "u1",
            "trial 加油包",
            RESULT_FAILED,
            "upstream: <html><h1>401</h1></html> a & b",
        );

        let text = read_text(&path);
        assert!(!text.contains("\\u003c"), "不应做 HTML 转义:\n{text}");
        assert!(!text.contains("\\u0026"), "不应做 HTML 转义:\n{text}");
        assert!(!text.contains("\\u003e"), "不应做 HTML 转义:\n{text}");
        assert!(text.contains("<html>"), "原始尖括号应原样保留:\n{text}");
        assert!(text.starts_with("[\n  {"), "应为 pretty 数组格式:\n{text}");
        assert!(!text.contains("\n\""), "多行元素未整体缩进:\n{text}");

        let back = read_all(&path);
        assert_eq!(back.len(), 1);
        assert!(back[0]["detail"].as_str().unwrap_or_default().contains("<html>"));
    }

    /// 自己写的文件再读回来必须无损（含多行 detail、身份字段）。
    #[test]
    fn round_trip_after_own_write() {
        let (r, path, _dir) = new_recorder(60, ids(&[("u1", "id-1", "甲")]));
        r.task("u1", "活跃上报", RESULT_SUCCESS, "第一轮");
        r.task("u1", "活跃上报", RESULT_SUCCESS, "第二轮 <b>加粗</b>");

        let all = read_all(&path);
        assert_eq!(all.len(), 2, "应有 2 条记录");
        assert_eq!(all[0]["detail"], json!("第一轮"));
        assert_eq!(all[1]["detail"], json!("第二轮 <b>加粗</b>"));
        for rec in &all {
            assert_eq!(rec["accountId"], json!("id-1"));
            assert_eq!(rec["accountName"], json!("甲"));
        }
    }

    /// 反复写入不得让缩进逐轮累加（端到端实测踩过的真实 bug）。
    #[test]
    fn repeated_writes_do_not_compound_indentation() {
        let (r, path, _dir) = new_recorder(60, ids(&[("u1", "id-1", "甲")]));
        for i in 0..5 {
            r.task("u1", "活跃上报", RESULT_SUCCESS, &format!("第 {i} 轮"));
        }

        let text = read_text(&path);
        for line in text.split('\n') {
            if !line.trim_start().starts_with("\"ts\"") {
                continue;
            }
            let indent = line.len() - line.trim_start_matches(' ').len();
            assert_eq!(indent, 4, "ts 行缩进应恒为 4 空格（缩进在逐轮累加）:\n{text}");
        }
        assert_eq!(read_all(&path).len(), 5);
    }

    /// 重排不得把 13 位毫秒写成科学计数法或丢精度。
    #[test]
    fn indent_preserves_large_timestamps() {
        let ts = 1_789_540_074_441i64;
        let (r, path, _dir) = recorder_at(3650, HashMap::new(), fixed_clock(ts));
        r.task("u1", "活跃上报", RESULT_SUCCESS, "");
        // 再写一条，触发一次「读回 → 重排 → 写回」，这才是有风险的路径
        r.task("u1", "活跃上报", RESULT_SUCCESS, "");

        let text = read_text(&path);
        assert!(!text.contains("e+") && !text.contains("E+"), "被写成科学计数法:\n{text}");
        for rec in read_all(&path) {
            assert_eq!(rec["ts"].as_i64(), Some(ts), "ts 精度受损");
        }
    }

    /// 空 identities / 既有记录缺字段或类型不符，都不该让功能失效。
    #[test]
    fn empty_identities_and_missing_fields_are_tolerated() {
        let now = system_now_ms();
        let (r, path, _dir) = new_recorder(60, HashMap::new());
        // 前三条有合法 ts（后两条缺字段/类型不符），后三条是退化条目（数字/字符串/null）
        std::fs::write(
            &path,
            format!(
                r#"[{{"ts": {}}}, {{"ts": {}, "title": "缺字段的老记录"}}, {{"ts": {}, "amount": "abc", "title": "类型不符"}}, 42, "字符串", null]"#,
                now - 2000,
                now - 1000,
                now - 500
            ),
        )
        .expect("预置容错数据");

        // 空 identities ⇒ 退回 uid；daily 路径还要逐条解析既有记录（类型不符的
        // 条目要跳过而不是让整份文件被放弃）
        r.task_daily("u1", "夜猫子任务", RESULT_INFO, "不在时间窗");

        let all = read_all(&path);
        let got = titles(&all);
        assert_eq!(all.len(), 4, "ts 取不到 0 的退化条目被裁掉，其余保留: {got:?}");
        assert!(got.contains(&"缺字段的老记录".to_string()), "缺字段记录应保留: {got:?}");
        assert!(got.contains(&"类型不符".to_string()), "类型不符的条目不该被删: {got:?}");
        assert!(got.contains(&"夜猫子任务".to_string()), "新记录应写入: {got:?}");
        assert_eq!(
            all.last().expect("末条")["accountId"],
            json!("u1"),
            "空 identities 应退回 uid"
        );
    }

    /// `identity_index` 返回副本，外部改动不影响内部。
    #[test]
    fn identity_index_is_a_copy() {
        let (r, _path, _dir) = new_recorder(60, ids(&[("u1", "id-1", "甲")]));
        let mut idx = r.identity_index();
        assert_eq!(idx.get("u1").map(|i| i.id.as_str()), Some("id-1"));
        idx.insert("u1".to_string(), Identity::new("tampered", "tampered"));
        assert_eq!(
            r.identity_index().get("u1").map(|i| i.id.as_str()),
            Some("id-1")
        );
    }

    /// 身份映射里 `name` 只有空白时退回 uid（Go `strings.TrimSpace(id.Name)`）。
    #[test]
    fn blank_identity_name_falls_back_to_uid() {
        let (r, path, _dir) = new_recorder(60, ids(&[("u1", "id-1", "   ")]));
        r.task("u1", "活跃上报", RESULT_SUCCESS, "");

        let got = &read_all(&path)[0];
        assert_eq!(got["accountId"], json!("id-1"));
        assert_eq!(got["accountName"], json!("u1"));
    }
}
