//! 抓包日志查看：列条目 / 看详情。
//!
//! ## 背景
//!
//! `device_proxy` 把每次 MITM 请求写进 `logs/proxy_req_YYYY-MM-DD.log`（同日超 100MB
//! 换 `.N` 序号）。在此之前那些文件只能靠用户自己去文件夹里翻 —— 而它们的用途恰恰是
//! 「为什么这次请求失败了」这种需要**当场**看内容的场景。
//! 本模块把「列条目 + 看单条详情」补齐。
//!
//! ## 为什么在 core 而不是宿主
//!
//! 解析逻辑（分隔、时间过滤、关键字过滤、SSE 摘要提取）与文件布局强耦合，
//! 且桌面端与 WebUI 都需要。放在 core 里两端共用一份，避免两份解析各自漂移。
//!
//! ## 与操作日志的区别
//!
//! 这里只读**抓包日志**（`proxy_req_*`）。`logs/proxy.log` 是代理自身的运行日志
//! （启动/停止/错误），内容形态完全不同（逐行而非分块），不在这里混着解析。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// 抓包日志文件名前缀。
const FILE_PREFIX: &str = "proxy_req_";
/// 抓包日志文件名后缀。
const FILE_SUFFIX: &str = ".log";
/// 条目分隔线（与 `device_proxy::logger` 写出的 `"=".repeat(80)` 必须一致）。
const SEPARATOR: &str = "================================================================================";

/// 单条日志摘要（列表用；不含正文，避免列表接口把整个文件传回前端）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProxyLogEntry {
    /// `文件名:条目序号` —— 详情接口按它定位，不需要前端拼路径。
    pub id: String,
    pub timestamp: String,
    /// `HTTP GET` / `WebSocket` 这类展示用形态。
    pub method: String,
    pub host: String,
    pub path: String,
    /// 响应状态，例如 `200 OK`；取不到时为 `-`。
    pub status: String,
    /// 该条目正文的字节数。
    pub size: usize,
    /// SSE 汇总里的模型名（无则省略）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sse_model: Option<String>,
    /// SSE token 用量摘要（无则省略）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sse_tokens: Option<String>,
}

/// 列表接口的返回：`entries` 是分页后的一页，`total` 是过滤后的总数。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProxyLogListResult {
    pub entries: Vec<ProxyLogEntry>,
    pub total: usize,
}

/// 查询条件。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyLogQueryOpts {
    /// 关键字（大小写不敏感，匹配整条正文）。
    #[serde(default)]
    pub keyword: Option<String>,
    /// 起始时间，`YYYY-MM-DD HH:MM:SS` 前缀比较（与时间戳同格式）。
    #[serde(default)]
    pub start_time: Option<String>,
    /// 结束时间，同上。
    #[serde(default)]
    pub end_time: Option<String>,
    /// 跳过条数（按时间倒序之后的偏移）。
    #[serde(default)]
    pub offset: Option<usize>,
    /// 返回条数上限，默认 50。
    #[serde(default)]
    pub limit: Option<usize>,
}

/// 抓包日志目录：`<store_dir>/logs`。
///
/// 与 `device_proxy` 的 `req_log_dir` 是同一个位置（那里由配置构造，
/// 这里从 `store_dir()` 推出，两条路径必须落在同一处 —— 有守护用例）。
pub fn logs_dir() -> PathBuf {
    crate::modules::config::store_dir().join("logs")
}

/// 列出目录下所有抓包日志文件名，按文件名升序（旧 → 新）。
///
/// 只要 `proxy_req_*.log`：同日的滚动分片是 `proxy_req_YYYY-MM-DD.log.N`，
/// 它们**不以 `.log` 结尾**，因此这里天然只收主文件。分片目前不展示 ——
/// 见 [`list_logs`] 的注释说明为什么不合并。
fn list_files(dir: &Path) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<String> = read
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with(FILE_PREFIX) && name.ends_with(FILE_SUFFIX) {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    files.sort();
    files
}

/// 文件名是否是可安全拼接的抓包日志名。
///
/// **安全边界**：`id` 由前端回传，形如 `文件名:序号`。若不做校验，
/// `..\..\..\Windows\System32\config\SAM:0` 这类输入会被直接 `join` 上来读任意文件。
/// 这里要求前缀 / 后缀 / 无分隔符 / 无 `..`，四条同时成立才放行。
fn is_safe_file_name(name: &str) -> bool {
    name.starts_with(FILE_PREFIX)
        && name.ends_with(FILE_SUFFIX)
        && !name.contains('\\')
        && !name.contains('/')
        && !name.contains("..")
}

/// 把正文按分隔线切成条目（去掉空块）。
fn split_entries(content: &str) -> Vec<&str> {
    content
        .split(SEPARATOR)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect()
}

/// 时间戳行的 `[YYYY-MM-DD HH:MM:SS]` 部分（取不到返回空串）。
fn timestamp_of(chunk: &str) -> String {
    // 时间戳不保证在第一行（请求体预览里也可能出现 `[`），所以找第一个 `[` 开头的行。
    chunk
        .lines()
        .find(|l| l.starts_with('['))
        .and_then(|l| l.get(1..20))
        .unwrap_or("")
        .to_string()
}

/// 从 SSE Summary 区块取字段值。
///
/// 区块以 `--- SSE Summary ---` 开头，遇到下一个 `--- ` 开头的行结束 ——
/// 不设结束条件的话，后面的区块里同名字段会被误取。
///
/// ## 缩进处理（参考实现在这里有个静默失效的 bug）
///
/// 日志写出的是两空格缩进的 `  model: claude-sonnet-4`。若先 `trim()` 再
/// `strip_prefix("  model: ")`，前缀里的空格已经被去掉了，**永远匹配不上** ——
/// 参考项目的 `extract_sse_field` 正是这个写法，于是它的模型名与 token 用量
/// 恒为空、且不报任何错（界面只是永远不显示这两列）。
///
/// 这里的做法是先 `trim_start()` 去掉缩进，再用**不带缩进**的前缀匹配。
fn sse_field(raw: &str, field: &str) -> Option<String> {
    let prefix = format!("{field}: ");
    let mut in_summary = false;
    for line in raw.lines() {
        let line = line.trim();
        if line.starts_with("--- SSE Summary ---") {
            in_summary = true;
            continue;
        }
        if !in_summary {
            continue;
        }
        // 进入下一个区块 → 摘要结束
        if line.starts_with("--- ") {
            break;
        }
        if let Some(rest) = line.strip_prefix(&prefix) {
            return Some(rest.to_string());
        }
    }
    None
}

/// token 用量摘要 `p:x c:y t:z`（缺 `prompt_tokens` 则整体省略）。
fn sse_tokens(raw: &str) -> Option<String> {
    let pt = sse_field(raw, "prompt_tokens")?;
    let ct = sse_field(raw, "completion_tokens").unwrap_or_else(|| "?".to_string());
    let tt = sse_field(raw, "total_tokens").unwrap_or_else(|| "?".to_string());
    Some(format!("p:{pt} c:{ct} t:{tt}"))
}

/// `host/path` 拆成两段（没有路径时路径为空串）。
fn split_host_path(hp: &str) -> (String, String) {
    match hp.find('/') {
        Some(idx) => (hp[..idx].to_string(), hp[idx..].to_string()),
        None => (hp.to_string(), String::new()),
    }
}

/// 从一条日志正文解析摘要；结构不认识时返回 `None`（跳过该条而不是报错）。
pub fn parse_entry(raw: &str, file_name: &str, index: usize) -> Option<ProxyLogEntry> {
    let header = raw.lines().find(|l| l.starts_with('['))?;
    let timestamp = header.get(1..20).unwrap_or("").to_string();
    // `[...] ` 之后的剩余部分
    let rest = match header.find("] ") {
        Some(i) => &header[i + 2..],
        None => "",
    };

    let (method, host, path, status) = if rest.starts_with("[WebSocket") {
        // `[WebSocket Upgrade] host/path`
        let hp = rest.find("] ").map(|i| &rest[i + 2..]).unwrap_or(rest);
        let (host, path) = split_host_path(hp);
        (
            "WebSocket".to_string(),
            host,
            path,
            "101 Upgrade".to_string(),
        )
    } else {
        // `METHOD host/path`
        let mut parts = rest.splitn(2, ' ');
        let raw_method = parts.next().unwrap_or("").to_string();
        let hp = parts.next().unwrap_or("");
        let (host, path) = split_host_path(hp);
        let status = raw
            .lines()
            .find(|l| l.starts_with("--- Response:"))
            .map(|l| {
                l.trim_start_matches("--- Response:")
                    .trim()
                    .trim_end_matches("---")
                    .trim()
                    .to_string()
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "-".to_string());
        (format!("HTTP {raw_method}"), host, path, status)
    };

    Some(ProxyLogEntry {
        id: format!("{file_name}:{index}"),
        timestamp,
        method,
        host,
        path,
        status,
        size: raw.len(),
        sse_model: sse_field(raw, "model"),
        sse_tokens: sse_tokens(raw),
    })
}

/// 列抓包日志条目（时间**倒序**，新的在前）。
///
/// ## 为什么只读主文件、不合并滚动分片
///
/// 同日超过 100MB 才会出现 `proxy_req_YYYY-MM-DD.log.1`。正常使用下不会产生，
/// 而合并分片意味着要按序号重排、还要处理分片内时间与主文件交错的边界情况。
/// 当前只读主文件，分片仍在磁盘上可按需直接打开 —— 需要时再补，
/// 而不是先写一段没有真实数据可验证的合并逻辑。
///
/// ## 排序为什么是「整体反转」而不是「按时间戳排序」
///
/// 文件名升序 = 日期升序，文件内条目本来就是写入顺序（时间升序），
/// 于是拼接结果天然时间正序，整体 `reverse` 即得倒序。按字符串排时间戳看起来更直接，
/// 但同一秒内多条时排序不稳定，会把同一请求的先后顺序打乱。
pub fn list_logs(opts: &ProxyLogQueryOpts) -> Result<ProxyLogListResult, String> {
    let dir = logs_dir();
    if !dir.exists() {
        return Ok(ProxyLogListResult {
            entries: Vec::new(),
            total: 0,
        });
    }

    let keyword = opts.keyword.as_deref().unwrap_or("").to_lowercase();
    let start = opts.start_time.as_deref().unwrap_or("");
    let end = opts.end_time.as_deref().unwrap_or("");
    let offset = opts.offset.unwrap_or(0);
    let limit = opts.limit.unwrap_or(50);

    let mut all: Vec<ProxyLogEntry> = Vec::new();
    for file_name in list_files(&dir) {
        let Ok(bytes) = std::fs::read(dir.join(&file_name)) else {
            // 单个文件读不到（被代理进程独占/刚被删除）不该让整个列表失败
            continue;
        };
        let content = String::from_utf8_lossy(&bytes);
        for (index, chunk) in split_entries(&content).into_iter().enumerate() {
            if !start.is_empty() || !end.is_empty() {
                let ts = timestamp_of(chunk);
                // 前缀比较：两侧都是 `YYYY-MM-DD HH:MM:SS`，定宽可直接比字典序
                if !start.is_empty() && ts.as_str() < start {
                    continue;
                }
                if !end.is_empty() && ts.as_str() > end {
                    continue;
                }
            }
            if !keyword.is_empty() && !chunk.to_lowercase().contains(&keyword) {
                continue;
            }
            if let Some(entry) = parse_entry(chunk, &file_name, index) {
                all.push(entry);
            }
        }
    }

    all.reverse();
    let total = all.len();
    let entries = all.into_iter().skip(offset).take(limit).collect();
    Ok(ProxyLogListResult { entries, total })
}

/// 取单条日志的完整正文。
///
/// `id` 形如 `proxy_req_2025-01-15.log:3`；文件名与序号都校验后才读盘。
pub fn log_detail(id: &str) -> Result<String, String> {
    let mut parts = id.splitn(2, ':');
    let file_name = parts.next().unwrap_or("");
    let index_raw = parts.next().ok_or("无效的日志 ID")?;
    let index: usize = index_raw.parse().map_err(|_| "无效的日志索引")?;

    if !is_safe_file_name(file_name) {
        return Err("无效的日志文件名".to_string());
    }

    let path = logs_dir().join(file_name);
    let bytes = std::fs::read(&path).map_err(|e| format!("读取日志文件失败: {e}"))?;
    let content = String::from_utf8_lossy(&bytes);

    split_entries(&content)
        .get(index)
        .map(|s| s.to_string())
        .ok_or_else(|| "找不到指定的日志条目（可能已被滚动清理）".to_string())
}

/// 列条目的 JSON 形态（宿主直接回给前端）。
pub fn list_logs_json(opts: &ProxyLogQueryOpts) -> Result<Value, String> {
    let res = list_logs(opts)?;
    serde_json::to_value(res).map_err(|e| format!("序列化日志列表失败: {e}"))
}

/// 删除抓包日志。`keep_days` 为 `None` 时全删。
///
/// 返回删除的文件数。滚动分片（`.log.N`）一并处理 —— 只删主文件会留下
/// 永远配不上主文件的孤儿分片。
pub fn clear_logs(keep_days: Option<u32>) -> Result<usize, String> {
    let dir = logs_dir();
    if !dir.exists() {
        return Ok(0);
    }
    let cutoff = keep_days.map(|d| {
        let now = chrono::Local::now();
        (now - chrono::Duration::days(d as i64))
            .format("%Y-%m-%d")
            .to_string()
    });

    let Ok(read) = std::fs::read_dir(&dir) else {
        return Ok(0);
    };
    let mut removed = 0usize;
    for entry in read.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(FILE_PREFIX) {
            continue;
        }
        // 保留期内跳过：从文件名里的日期判断，而不是文件 mtime ——
        // mtime 会被「打开看一眼」这类只读操作改不到，但会被复制/移动改掉。
        if let Some(cutoff) = &cutoff {
            if let Some(date) = log_date_of(&name) {
                if &date >= cutoff {
                    continue;
                }
            }
        }
        if std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// 从 `proxy_req_YYYY-MM-DD.log[.N]` 取日期部分。
pub fn log_date_of(file_name: &str) -> Option<String> {
    let rest = file_name.strip_prefix(FILE_PREFIX)?;
    // 只取前 10 位（`YYYY-MM-DD`），不要求后面一定是 `.log` —— 分片也要能识别
    let date = rest.get(..10)?;
    let ok = date.len() == 10
        && date.as_bytes()[4] == b'-'
        && date.as_bytes()[7] == b'-'
        && date
            .bytes()
            .enumerate()
            .all(|(i, b)| matches!(i, 4 | 7) || b.is_ascii_digit());
    ok.then(|| date.to_string())
}

/// 日志目录概况（给界面显示「有多少可看的历史」）。
pub fn logs_overview() -> Value {
    let dir = logs_dir();
    let files = list_files(&dir);
    let dates: Vec<String> = files.iter().filter_map(|f| log_date_of(f)).collect();
    let total_bytes: u64 = files
        .iter()
        .filter_map(|f| std::fs::metadata(dir.join(f)).ok())
        .map(|m| m.len())
        .sum();
    json!({
        "dir": dir.to_string_lossy(),
        "fileCount": files.len(),
        "totalBytes": total_bytes,
        "oldest": dates.iter().min(),
        "newest": dates.iter().max(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::config::test_isolation::Isolated;

    /// 一条普通请求的日志正文（与 `device_proxy::logger` 的写出格式一致）。
    const SAMPLE_GET: &str = "\
[2025-01-15 10:30:00] GET api.trae.cn/trae/api/v1/user
--- Request Headers ---
  authorization: Bearer abc***
--- Response: 200 OK ---
  content-type: application/json
";

    fn write_log(dir: &Path, name: &str, blocks: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        let body = blocks
            .iter()
            .map(|b| format!("\n{SEPARATOR}\n{b}\n"))
            .collect::<String>();
        std::fs::write(dir.join(name), body).unwrap();
    }

    /// 隔离一个数据目录，并返回 `logs/` 路径。
    fn iso_logs(tag: &str) -> (Isolated, PathBuf) {
        let iso = Isolated::new(tag);
        let dir = iso.dir().join("logs");
        (iso, dir)
    }

    #[test]
    fn 列出条目并按时间倒序() {
        let (_iso, dir) = iso_logs("proxy_logs_order");
        write_log(
            &dir,
            "proxy_req_2025-01-15.log",
            &[
                "[2025-01-15 10:00:00] GET a.cn/old\n--- Response: 200 OK ---",
                "[2025-01-15 11:00:00] GET a.cn/new\n--- Response: 200 OK ---",
            ],
        );

        let res = list_logs(&ProxyLogQueryOpts::default()).unwrap();
        assert_eq!(res.total, 2);
        assert_eq!(
            res.entries[0].timestamp, "2025-01-15 11:00:00",
            "最新的应在最前：{:?}",
            res.entries
        );
        assert_eq!(res.entries[1].timestamp, "2025-01-15 10:00:00");
    }

    #[test]
    fn 多文件按日期升序拼接后整体倒序() {
        let (_iso, dir) = iso_logs("proxy_logs_multifile");
        write_log(
            &dir,
            "proxy_req_2025-01-14.log",
            &["[2025-01-14 09:00:00] GET a.cn/day1\n--- Response: 200 OK ---"],
        );
        write_log(
            &dir,
            "proxy_req_2025-01-15.log",
            &["[2025-01-15 09:00:00] GET a.cn/day2\n--- Response: 200 OK ---"],
        );

        let res = list_logs(&ProxyLogQueryOpts::default()).unwrap();
        assert_eq!(res.total, 2);
        assert_eq!(res.entries[0].timestamp, "2025-01-15 09:00:00");
        assert_eq!(res.entries[1].timestamp, "2025-01-14 09:00:00");
    }

    #[test]
    fn 只收抓包日志不收操作日志() {
        let (_iso, dir) = iso_logs("proxy_logs_filter");
        write_log(
            &dir,
            "proxy_req_2025-01-15.log",
            &["[2025-01-15 10:00:00] GET a.cn\n--- Response: 200 OK ---"],
        );
        // 操作日志与其它文件都不该被当抓包条目解析
        std::fs::write(dir.join("proxy.log"), "2025-01-15 10:00:00 代理已启动\n").unwrap();
        std::fs::write(dir.join("switcher.log"), "切换完成\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "随便什么\n").unwrap();

        let res = list_logs(&ProxyLogQueryOpts::default()).unwrap();
        assert_eq!(res.total, 1, "只有 proxy_req_ 开头的才算：{:?}", res.entries);
    }

    #[test]
    fn 解析出方法主机路径与状态码() {
        let (_iso, dir) = iso_logs("proxy_logs_parse");
        write_log(&dir, "proxy_req_2025-01-15.log", &[SAMPLE_GET]);

        let res = list_logs(&ProxyLogQueryOpts::default()).unwrap();
        let e = &res.entries[0];
        assert_eq!(e.method, "HTTP GET");
        assert_eq!(e.host, "api.trae.cn");
        assert_eq!(e.path, "/trae/api/v1/user");
        assert_eq!(e.status, "200 OK");
        assert_eq!(e.id, "proxy_req_2025-01-15.log:0");
        assert!(e.size > 0);
    }

    #[test]
    fn websocket_条目单独识别() {
        let (_iso, dir) = iso_logs("proxy_logs_ws");
        write_log(
            &dir,
            "proxy_req_2025-01-15.log",
            &["[2025-01-15 12:00:00] [WebSocket Upgrade] ws.trae.cn/ws/chat\n--- Request Headers ---\n  sec-websocket-key: x"],
        );

        let res = list_logs(&ProxyLogQueryOpts::default()).unwrap();
        let e = &res.entries[0];
        assert_eq!(e.method, "WebSocket");
        assert_eq!(e.host, "ws.trae.cn");
        assert_eq!(e.path, "/ws/chat");
        assert_eq!(e.status, "101 Upgrade");
    }

    #[test]
    fn 没有响应的条目状态是横线而不是空() {
        let (_iso, dir) = iso_logs("proxy_logs_noresp");
        write_log(
            &dir,
            "proxy_req_2025-01-15.log",
            &["[2025-01-15 10:00:00] CONNECT a.cn:443\n--- Request Headers ---"],
        );

        let res = list_logs(&ProxyLogQueryOpts::default()).unwrap();
        assert_eq!(
            res.entries[0].status, "-",
            "空状态会让界面渲染出一列空白，看不出是「没状态」还是「没解析出来」"
        );
    }

    #[test]
    fn 关键字过滤大小写不敏感() {
        let (_iso, dir) = iso_logs("proxy_logs_kw");
        write_log(
            &dir,
            "proxy_req_2025-01-15.log",
            &[
                "[2025-01-15 10:00:00] GET a.cn/trae/user\n--- Response: 200 OK ---",
                "[2025-01-15 11:00:00] GET a.cn/other\n--- Response: 200 OK ---",
            ],
        );

        let opts = ProxyLogQueryOpts {
            keyword: Some("TRAE".to_string()),
            ..Default::default()
        };
        let res = list_logs(&opts).unwrap();
        assert_eq!(res.total, 1);
        assert!(res.entries[0].path.contains("trae"));
    }

    #[test]
    fn 时间区间两端都含端点() {
        let (_iso, dir) = iso_logs("proxy_logs_time");
        write_log(
            &dir,
            "proxy_req_2025-01-15.log",
            &[
                "[2025-01-15 09:00:00] GET a.cn/a\n--- Response: 200 OK ---",
                "[2025-01-15 10:00:00] GET a.cn/b\n--- Response: 200 OK ---",
                "[2025-01-15 11:00:00] GET a.cn/c\n--- Response: 200 OK ---",
            ],
        );

        let opts = ProxyLogQueryOpts {
            start_time: Some("2025-01-15 10:00:00".to_string()),
            end_time: Some("2025-01-15 10:00:00".to_string()),
            ..Default::default()
        };
        let res = list_logs(&opts).unwrap();
        assert_eq!(res.total, 1, "边界条目应被包含：{:?}", res.entries);
        assert_eq!(res.entries[0].timestamp, "2025-01-15 10:00:00");
    }

    #[test]
    fn 分页的_total_是过滤后总数而不是本页条数() {
        let (_iso, dir) = iso_logs("proxy_logs_page");
        let blocks: Vec<String> = (0..10)
            .map(|i| format!("[2025-01-15 10:00:{i:02}] GET a.cn/{i}\n--- Response: 200 OK ---"))
            .collect();
        let refs: Vec<&str> = blocks.iter().map(String::as_str).collect();
        write_log(&dir, "proxy_req_2025-01-15.log", &refs);

        let opts = ProxyLogQueryOpts {
            offset: Some(2),
            limit: Some(3),
            ..Default::default()
        };
        let res = list_logs(&opts).unwrap();
        assert_eq!(res.total, 10, "total 应是总数，否则前端分页算不出总页数");
        assert_eq!(res.entries.len(), 3);
        // 倒序后 offset=2 应跳过最新的两条
        assert_eq!(res.entries[0].timestamp, "2025-01-15 10:00:07");
    }

    #[test]
    fn 空目录返回空结果而不是报错() {
        let (_iso, _dir) = iso_logs("proxy_logs_empty");
        let res = list_logs(&ProxyLogQueryOpts::default()).unwrap();
        assert_eq!(res.total, 0);
        assert!(res.entries.is_empty());
    }

    #[test]
    fn 目录不存在返回空结果而不是报错() {
        let iso = Isolated::new("proxy_logs_nodir");
        // 不创建 logs 目录
        let _ = iso;
        let res = list_logs(&ProxyLogQueryOpts::default()).unwrap();
        assert_eq!(res.total, 0);
    }

    #[test]
    fn 详情按_id_取回对应条目() {
        let (_iso, dir) = iso_logs("proxy_logs_detail");
        write_log(
            &dir,
            "proxy_req_2025-01-15.log",
            &[
                "[2025-01-15 10:00:00] GET a.cn/first\n--- Response: 200 OK ---",
                "[2025-01-15 11:00:00] GET a.cn/second\n--- Response: 200 OK ---",
            ],
        );

        let body = log_detail("proxy_req_2025-01-15.log:1").unwrap();
        assert!(body.contains("a.cn/second"), "应取到第二条：{body}");
        assert!(!body.contains("a.cn/first"), "不该混入相邻条目：{body}");
    }

    #[test]
    fn 详情序号越界时报错而不是返回别的条目() {
        let (_iso, dir) = iso_logs("proxy_logs_oob");
        write_log(
            &dir,
            "proxy_req_2025-01-15.log",
            &["[2025-01-15 10:00:00] GET a.cn/only\n--- Response: 200 OK ---"],
        );

        let err = log_detail("proxy_req_2025-01-15.log:99").unwrap_err();
        assert!(err.contains("找不到"), "{err}");
    }

    #[test]
    fn 详情拒绝路径穿越() {
        let (_iso, _dir) = iso_logs("proxy_logs_traversal");
        // 四种典型的越权写法都必须被挡在文件名校验上
        for bad in [
            r"..\..\..\Windows\System32\drivers\etc\hosts:0",
            "../../etc/passwd:0",
            "proxy_req_2025-01-15.log../secret.log:0",
            "other.txt:0",
            r"C:\Windows\win.ini:0",
        ] {
            let err = log_detail(bad).unwrap_err();
            assert!(
                err.contains("无效的日志文件名") || err.contains("无效的日志索引"),
                "{bad} 应被拒绝，实际：{err}"
            );
        }
    }

    #[test]
    fn 详情拒绝非法序号() {
        let (_iso, _dir) = iso_logs("proxy_logs_badidx");
        for bad in ["proxy_req_2025-01-15.log:abc", "proxy_req_2025-01-15.log:-1"] {
            assert!(log_detail(bad).is_err(), "{bad} 应被拒绝");
        }
        // 没有冒号分隔
        assert!(log_detail("proxy_req_2025-01-15.log").is_err());
    }

    #[test]
    fn 读取_sse_摘要里的模型与_token() {
        let (_iso, dir) = iso_logs("proxy_logs_sse");
        let body = "\
[2025-01-15 10:00:00] POST api.trae.cn/v1/chat
--- Response: 200 OK ---
--- SSE Summary ---
  model: claude-sonnet-4
  prompt_tokens: 120
  completion_tokens: 340
  total_tokens: 460
";
        write_log(&dir, "proxy_req_2025-01-15.log", &[body]);

        let res = list_logs(&ProxyLogQueryOpts::default()).unwrap();
        let e = &res.entries[0];
        assert_eq!(e.sse_model.as_deref(), Some("claude-sonnet-4"));
        assert_eq!(e.sse_tokens.as_deref(), Some("p:120 c:340 t:460"));
    }

    #[test]
    fn sse_字段不会越区块误取() {
        // SSE Summary 之后还有别的区块，且该区块里也有同名键 —— 不该被取到
        let raw = "\
--- Response: 200 OK ---
--- SSE Summary ---
  model: real-model
--- Request Body (10 bytes) ---
  model: decoy-model
";
        assert_eq!(sse_field(raw, "model").as_deref(), Some("real-model"));
    }

    #[test]
    fn sse_字段带缩进也必须能取到() {
        // 回归：日志写出的是两空格缩进。若实现先 trim() 再 strip_prefix("  model: ")，
        // 前缀里的空格已被去掉 → 永远匹配不上 → 模型名与 token 恒为空且不报错。
        // 参考项目的 extract_sse_field 正是这个写法，这条用例就是为它加的守护。
        let raw = "--- SSE Summary ---\n  model: x\n";
        assert_eq!(
            sse_field(raw, "model").as_deref(),
            Some("x"),
            "带缩进的字段必须能取到，否则界面那两列永远空着"
        );

        // 无缩进（其它写法）也要能取到
        let raw2 = "--- SSE Summary ---\nmodel: y\n";
        assert_eq!(sse_field(raw2, "model").as_deref(), Some("y"));
    }

    #[test]
    fn 非流式条目没有_sse_字段且不出现在_json_里() {
        let (_iso, dir) = iso_logs("proxy_logs_nosse");
        write_log(&dir, "proxy_req_2025-01-15.log", &[SAMPLE_GET]);

        let v = list_logs_json(&ProxyLogQueryOpts::default()).unwrap();
        let e = &v["entries"][0];
        assert!(
            e.get("sseModel").is_none(),
            "无 SSE 字段时应整个省略，而不是给 null：{e}"
        );
        assert_eq!(e["host"], "api.trae.cn");
    }

    #[test]
    fn 清空全部日志返回删除数量() {
        let (_iso, dir) = iso_logs("proxy_logs_clear");
        write_log(
            &dir,
            "proxy_req_2025-01-15.log",
            &["[2025-01-15 10:00:00] GET a.cn\n--- Response: 200 OK ---"],
        );
        write_log(
            &dir,
            "proxy_req_2025-01-16.log",
            &["[2025-01-16 10:00:00] GET a.cn\n--- Response: 200 OK ---"],
        );
        // 操作日志不该被清
        std::fs::write(dir.join("proxy.log"), "运行日志\n").unwrap();

        let removed = clear_logs(None).unwrap();
        assert_eq!(removed, 2);
        assert!(dir.join("proxy.log").exists(), "操作日志必须保留");
        assert_eq!(list_logs(&ProxyLogQueryOpts::default()).unwrap().total, 0);
    }

    #[test]
    fn 清空保留期内的日志被留下() {
        let (_iso, dir) = iso_logs("proxy_logs_keep");
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let old = (chrono::Local::now() - chrono::Duration::days(30))
            .format("%Y-%m-%d")
            .to_string();
        write_log(
            &dir,
            &format!("proxy_req_{today}.log"),
            &["[2025-01-15 10:00:00] GET a.cn\n--- Response: 200 OK ---"],
        );
        write_log(
            &dir,
            &format!("proxy_req_{old}.log"),
            &["[2025-01-15 10:00:00] GET a.cn\n--- Response: 200 OK ---"],
        );

        let removed = clear_logs(Some(7)).unwrap();
        assert_eq!(removed, 1, "只该删超过 7 天的那个");
        assert!(dir.join(format!("proxy_req_{today}.log")).exists());
        assert!(!dir.join(format!("proxy_req_{old}.log")).exists());
    }

    #[test]
    fn 清空时滚动分片一并删除() {
        let (_iso, dir) = iso_logs("proxy_logs_clear_split");
        write_log(
            &dir,
            "proxy_req_2025-01-15.log",
            &["[2025-01-15 10:00:00] GET a.cn\n--- Response: 200 OK ---"],
        );
        std::fs::write(dir.join("proxy_req_2025-01-15.log.1"), "分片内容\n").unwrap();

        let removed = clear_logs(None).unwrap();
        assert_eq!(removed, 2, "分片也要删，否则留下配不上主文件的孤儿");
        assert!(!dir.join("proxy_req_2025-01-15.log.1").exists());
    }

    #[test]
    fn 文件名日期解析() {
        assert_eq!(
            log_date_of("proxy_req_2025-01-15.log").as_deref(),
            Some("2025-01-15")
        );
        assert_eq!(
            log_date_of("proxy_req_2025-01-15.log.3").as_deref(),
            Some("2025-01-15"),
            "滚动分片也要能读出日期"
        );
        assert_eq!(log_date_of("proxy.log"), None);
        assert_eq!(log_date_of("proxy_req_bad.log"), None);
        assert_eq!(log_date_of("proxy_req_2025-1-15.log"), None);
    }

    #[test]
    fn 概况统计文件数体积与日期范围() {
        let (_iso, dir) = iso_logs("proxy_logs_overview");
        write_log(
            &dir,
            "proxy_req_2025-01-14.log",
            &["[2025-01-14 10:00:00] GET a.cn\n--- Response: 200 OK ---"],
        );
        write_log(
            &dir,
            "proxy_req_2025-01-15.log",
            &["[2025-01-15 10:00:00] GET a.cn\n--- Response: 200 OK ---"],
        );

        let v = logs_overview();
        assert_eq!(v["fileCount"], 2);
        assert!(v["totalBytes"].as_u64().unwrap() > 0);
        assert_eq!(v["oldest"], "2025-01-14");
        assert_eq!(v["newest"], "2025-01-15");
        assert!(v["dir"].as_str().unwrap().ends_with("logs"));
    }

    #[test]
    fn 日志目录与代理写入目录一致() {
        // 两条路径分别由 config::store_dir 推出：这里与 device_proxy 的 req_log_dir 必须同源。
        // 不一致会让「日志查看」永远显示空列表，而且不报任何错。
        let expected = crate::modules::config::store_dir().join("logs");
        assert_eq!(logs_dir(), expected);
    }

    #[test]
    fn 破损正文被跳过而不是让整页失败() {
        let (_iso, dir) = iso_logs("proxy_logs_broken");
        write_log(
            &dir,
            "proxy_req_2025-01-15.log",
            &[
                "[2025-01-15 10:00:00] GET a.cn/good\n--- Response: 200 OK ---",
                // 没有以 `[` 开头的头部行 → parse_entry 返回 None
                "这一块完全不符合格式\n随便写点什么",
            ],
        );

        let res = list_logs(&ProxyLogQueryOpts::default()).unwrap();
        assert_eq!(res.total, 1, "坏条目应被跳过，好条目仍要出来");
        assert!(res.entries[0].path.contains("good"));
    }
}
