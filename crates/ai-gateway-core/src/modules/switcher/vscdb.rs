//! Trae 项目列表 / 最近打开跨账号保留（icube 布局 `state.vscdb` 全局键合并）。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/switcher/vscdb.rs`（F-68），
//! 按本仓库「core 不依赖 Tauri」的约定原样移植。
//!
//! ## 问题
//!
//! 切换账号时 `state.vscdb` 随槽位快照**整体回滚**，而项目列表
//!（`solo-lite.local-project-folders`）与最近打开
//!（`history.recentlyOpenedPathsList`）是**全局单键**（非账号分区键）——
//! 回滚后只剩目标账号自己那一份，用户感知为「项目列表消失了」。
//!
//! ## 方案
//!
//! 恢复快照**前**抽出这两个键，恢复**后**按条目合并回写：快照内已有的以快照为准，
//! 仅补入切换前多出来的条目。
//!
//! ## 什么**不**合并（同样重要）
//!
//! 账号分区键（`solo-lite:content-map:<uid>` / `solo-lite-mode-state-map-<uid>`）
//! 一律不碰 —— 跨账号合并会产生服务端归属校验失败的「幽灵会话」。
//! 只碰上面两个明确是全局语义的键，是本模块存在的全部理由。
//!
//! 零新增依赖：`rusqlite` 已是本 crate 依赖（vscdb 读库先例见 `codebuddy_cn_ide.rs`）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// 项目列表（SOLO 本地项目文件夹）。
const KEY_PROJECT_FOLDERS: &str = "solo-lite.local-project-folders";
/// 最近打开（VSCode 系通用键）。
const KEY_RECENT_PATHS: &str = "history.recentlyOpenedPathsList";

/// 单键取值，保留原始存储类型。
///
/// VSCode 系该列多为 BLOB 存 JSON 文本；回写时**必须按原类型写**，
/// 把 BLOB 列改成 TEXT 会让客户端读取路径产生差异（表现为列表读不出来）。
#[derive(Clone, Debug, PartialEq, Eq)]
struct KeyVal {
    text: String,
    blob: bool,
}

/// 切换前抽出的全局键快照。
#[derive(Default, Clone, Debug)]
pub struct GlobalKeys {
    pub project_folders: Option<String>,
    pub recent_paths: Option<String>,
    /// 原值是 BLOB 存储时为 true（回写保持同类型）。
    folders_blob: bool,
    recent_blob: bool,
}

impl GlobalKeys {
    /// 是否没有任何可保留的键（调用方据此跳过整个合并流程）。
    pub fn is_empty(&self) -> bool {
        self.project_folders.is_none() && self.recent_paths.is_none()
    }
}

/// icube 布局的 `state.vscdb` 路径。
pub fn vscdb_path(data_dir: &Path) -> PathBuf {
    data_dir.join("User").join("globalStorage").join("state.vscdb")
}

fn open_ro(path: &Path) -> Option<rusqlite::Connection> {
    if !path.is_file() {
        return None;
    }
    rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

/// 读 `ItemTable` 单键；值列为 BLOB 时按 UTF-8 解码（失败 → None，保守不动）。
fn read_key(conn: &rusqlite::Connection, key: &str) -> Option<KeyVal> {
    let mut stmt = conn.prepare("SELECT value FROM ItemTable WHERE key = ?1").ok()?;
    let mut rows = stmt.query(rusqlite::params![key]).ok()?;
    let row = rows.next().ok()??;
    let v = row.get::<_, rusqlite::types::Value>(0).ok()?;
    match v {
        rusqlite::types::Value::Text(s) => Some(KeyVal { text: s, blob: false }),
        rusqlite::types::Value::Blob(b) => String::from_utf8(b)
            .ok()
            .map(|text| KeyVal { text, blob: true }),
        _ => None,
    }
}

/// 抽出待保留的全局键（恢复快照**前**调用）。
///
/// 文件缺失/读失败 → 空快照，调用方自然跳过（不报错：没有 vscdb 只是说明
/// 该账号从未启动过客户端，不是异常）。
pub fn snapshot_global_keys(vscdb: &Path) -> GlobalKeys {
    let Some(conn) = open_ro(vscdb) else {
        return GlobalKeys::default();
    };
    let folders = read_key(&conn, KEY_PROJECT_FOLDERS);
    let recent = read_key(&conn, KEY_RECENT_PATHS);
    GlobalKeys {
        folders_blob: folders.as_ref().map(|k| k.blob).unwrap_or(false),
        recent_blob: recent.as_ref().map(|k| k.blob).unwrap_or(false),
        project_folders: folders.map(|k| k.text),
        recent_paths: recent.map(|k| k.text),
    }
}

/// 恢复快照**后**把切换前的全局键合并回写。
///
/// - `Ok(None)`：无需写入（无切换前数据 / 快照已含全部条目 / 结构无法解析）
/// - `Ok(Some(摘要))`：已写入，摘要供进度流展示
/// - `Err`：写入失败（调用方按 warn 处理，**不阻断切换** —— 登录态已经恢复好了，
///   为一个项目列表回滚整个切换是得不偿失的）
pub fn merge_global_keys(vscdb: &Path, pre: &GlobalKeys) -> Result<Option<String>, String> {
    if pre.is_empty() || !vscdb.is_file() {
        return Ok(None);
    }
    let cur = snapshot_global_keys(vscdb);

    let mut writes: Vec<(&'static str, String, bool)> = Vec::new();
    let mut folders_added = 0usize;
    let mut recent_added = 0usize;
    if let Some(p) = pre.project_folders.as_deref() {
        if let Some((merged, added)) = merge_value(cur.project_folders.as_deref(), p) {
            folders_added = added;
            let blob = cur.folders_blob || pre.folders_blob;
            writes.push((KEY_PROJECT_FOLDERS, merged, blob));
        }
    }
    if let Some(p) = pre.recent_paths.as_deref() {
        if let Some((merged, added)) = merge_value(cur.recent_paths.as_deref(), p) {
            recent_added = added;
            let blob = cur.recent_blob || pre.recent_blob;
            writes.push((KEY_RECENT_PATHS, merged, blob));
        }
    }
    if writes.is_empty() {
        return Ok(None);
    }

    let conn = rusqlite::Connection::open(vscdb).map_err(|e| format!("打开 vscdb 失败: {e}"))?;
    for (key, value, blob) in &writes {
        let sql = if *blob {
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value"
        } else {
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value"
        };
        let result = if *blob {
            conn.execute(sql, rusqlite::params![key, value.as_bytes()])
        } else {
            conn.execute(sql, rusqlite::params![key, value])
        };
        result.map_err(|e| format!("写入 {key} 失败: {e}"))?;
    }
    drop(conn);

    let mut parts = Vec::new();
    if folders_added > 0 {
        parts.push(format!("项目列表 +{folders_added}"));
    }
    if recent_added > 0 {
        parts.push(format!("最近打开 +{recent_added}"));
    }
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(parts.join("，")))
}

/// 合并两个 JSON 数组文本：以 `current`（快照值）为基础，补入 `previous`
/// （切换前值）中快照没有的条目。
///
/// 返回 `(合并后的 JSON 文本, 新增条目数)`；任一侧不是可解析且两侧同为数组时返回
/// `None`（**保守不动** —— 结构不认识就宁可保留快照原值，也不要写坏客户端数据）。
///
/// 条目按**序列化后的字符串**去重。`serde_json` 未启用 `preserve_order`，对象键在
/// 反序列化时即被排序，因此客户端重排键序不会产生重复条目。
fn merge_value(current: Option<&str>, previous: &str) -> Option<(String, usize)> {
    let prev: serde_json::Value = serde_json::from_str(previous).ok()?;
    let prev_arr = prev.as_array()?;
    let cur: Option<serde_json::Value> = current.and_then(|c| serde_json::from_str(c).ok());
    // 快照侧不是数组（缺失或异形）→ 没有可合并的基线，不写
    let mut cur_arr = cur.as_ref().and_then(|c| c.as_array()).cloned()?;

    let mut seen: HashSet<String> = cur_arr
        .iter()
        .map(|v| serde_json::to_string(v).unwrap_or_default())
        .collect();
    let mut added = 0usize;
    for item in prev_arr {
        let key = serde_json::to_string(item).unwrap_or_default();
        if seen.insert(key) {
            cur_arr.push(item.clone());
            added += 1;
        }
    }
    if added == 0 {
        return None;
    }
    let merged = serde_json::to_string(&serde_json::Value::Array(cur_arr)).ok()?;
    Some((merged, added))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个只有 ItemTable 的最小 vscdb。
    fn tmp_db(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "ai-gateway-vscdb-{tag}-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let conn = rusqlite::Connection::open(&p).unwrap();
        conn.execute(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB)",
            [],
        )
        .unwrap();
        drop(conn);
        p
    }

    fn seed(path: &Path, rows: &[(&str, &str, bool)]) {
        let conn = rusqlite::Connection::open(path).unwrap();
        for (k, v, blob) in rows {
            if *blob {
                conn.execute(
                    "INSERT OR REPLACE INTO ItemTable (key, value) VALUES (?1, ?2)",
                    rusqlite::params![k, v.as_bytes()],
                )
                .unwrap();
            } else {
                conn.execute(
                    "INSERT OR REPLACE INTO ItemTable (key, value) VALUES (?1, ?2)",
                    rusqlite::params![k, v],
                )
                .unwrap();
            }
        }
    }

    fn read_text(path: &Path, key: &str) -> Option<String> {
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .ok()?;
        read_key(&conn, key).map(|k| k.text)
    }

    #[test]
    fn vscdb_路径拼在_global_storage_下() {
        let p = vscdb_path(Path::new("C:/data"));
        assert!(p.ends_with("globalStorage/state.vscdb") || p.ends_with("globalStorage\\state.vscdb"));
    }

    #[test]
    fn 快照抽出两个全局键() {
        let p = tmp_db("snap");
        seed(
            &p,
            &[
                (KEY_PROJECT_FOLDERS, r#"["/a"]"#, false),
                (KEY_RECENT_PATHS, r#"["/b"]"#, false),
                ("solo-lite:content-map:123", r#"{"x":1}"#, false),
            ],
        );
        let snap = snapshot_global_keys(&p);
        assert_eq!(snap.project_folders.as_deref(), Some(r#"["/a"]"#));
        assert_eq!(snap.recent_paths.as_deref(), Some(r#"["/b"]"#));
        assert!(!snap.is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn 文件缺失返回空快照() {
        let snap = snapshot_global_keys(Path::new("C:/definitely/not/here.db"));
        assert!(snap.is_empty());
    }

    #[test]
    fn blob_列保持_blob_写回() {
        let p = tmp_db("blob");
        seed(&p, &[(KEY_PROJECT_FOLDERS, r#"["/a"]"#, true)]);
        let pre = GlobalKeys {
            project_folders: Some(r#"["/a","/b"]"#.into()),
            recent_paths: None,
            folders_blob: true,
            recent_blob: false,
        };
        let summary = merge_global_keys(&p, &pre).unwrap().expect("应写入");
        assert!(summary.contains("项目列表 +1"), "{summary}");
        // 类型仍是 BLOB
        let conn = rusqlite::Connection::open(&p).unwrap();
        let v = conn
            .query_row(
                "SELECT value FROM ItemTable WHERE key = ?1",
                rusqlite::params![KEY_PROJECT_FOLDERS],
                |r| r.get::<_, rusqlite::types::Value>(0),
            )
            .unwrap();
        assert!(matches!(v, rusqlite::types::Value::Blob(_)), "必须保持 BLOB 类型");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn 合并补入切换前多出的条目() {
        let p = tmp_db("merge");
        // 快照只有 /a（目标账号自己的）；切换前有 /a 和 /extra
        seed(&p, &[(KEY_PROJECT_FOLDERS, r#"["/a"]"#, false)]);
        let pre = GlobalKeys {
            project_folders: Some(r#"["/a","/extra"]"#.into()),
            recent_paths: None,
            folders_blob: false,
            recent_blob: false,
        };
        let summary = merge_global_keys(&p, &pre).unwrap().expect("应写入");
        assert!(summary.contains("+1"), "{summary}");
        let text = read_text(&p, KEY_PROJECT_FOLDERS).unwrap();
        assert!(text.contains("/a") && text.contains("/extra"), "{text}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn 快照已含全部条目时不写() {
        let p = tmp_db("same");
        seed(&p, &[(KEY_PROJECT_FOLDERS, r#"["/a","/b"]"#, false)]);
        let pre = GlobalKeys {
            project_folders: Some(r#"["/a","/b"]"#.into()),
            recent_paths: None,
            folders_blob: false,
            recent_blob: false,
        };
        assert!(merge_global_keys(&p, &pre).unwrap().is_none());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn 账号分区键不被触碰() {
        let p = tmp_db("partitioned");
        seed(
            &p,
            &[
                (KEY_PROJECT_FOLDERS, r#"["/a"]"#, false),
                ("solo-lite:content-map:123", r#"{"keep":true}"#, false),
                ("solo-lite-mode-state-map-123", r#"{"m":1}"#, false),
            ],
        );
        let pre = GlobalKeys {
            project_folders: Some(r#"["/a","/b"]"#.into()),
            recent_paths: None,
            folders_blob: false,
            recent_blob: false,
        };
        merge_global_keys(&p, &pre).unwrap();
        assert_eq!(
            read_text(&p, "solo-lite:content-map:123").as_deref(),
            Some(r#"{"keep":true}"#),
            "账号分区键跨账号合并会产生幽灵会话，必须零改动"
        );
        assert_eq!(
            read_text(&p, "solo-lite-mode-state-map-123").as_deref(),
            Some(r#"{"m":1}"#)
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn 非_json_值保持快照值不合并() {
        let p = tmp_db("plain");
        seed(&p, &[(KEY_PROJECT_FOLDERS, "not-json", false)]);
        let pre = GlobalKeys {
            project_folders: Some("also-not-json".into()),
            recent_paths: None,
            folders_blob: false,
            recent_blob: false,
        };
        assert!(merge_global_keys(&p, &pre).unwrap().is_none());
        assert_eq!(read_text(&p, KEY_PROJECT_FOLDERS).as_deref(), Some("not-json"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn 快照行缺失时不写() {
        let p = tmp_db("norow");
        seed(&p, &[("other", "x", false)]);
        let pre = GlobalKeys {
            project_folders: Some(r#"["/a"]"#.into()),
            recent_paths: None,
            folders_blob: false,
            recent_blob: false,
        };
        // 快照侧没有该键（cur 为 None）→ 无从合并基线，保守跳过
        assert!(merge_global_keys(&p, &pre).unwrap().is_none());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn 文件缺失时安全跳过() {
        let p = PathBuf::from("C:/definitely/not/here.db");
        let pre = GlobalKeys {
            project_folders: Some("[]".into()),
            recent_paths: None,
            folders_blob: false,
            recent_blob: false,
        };
        assert!(merge_global_keys(&p, &pre).unwrap().is_none());
    }

    #[test]
    fn 空快照直接跳过() {
        let p = tmp_db("empty");
        assert!(merge_global_keys(&p, &GlobalKeys::default()).unwrap().is_none());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn 完全相同的条目不会重复追加() {
        // 同一批条目：快照侧已有全部，切换前值没有新增 → 返回 None（不写库）。
        // 若这里误判成「全部是新条目」，每次切换都会把列表翻倍。
        assert!(
            merge_value(Some(r#"[{"a":1},{"b":2}]"#), r#"[{"a":1},{"b":2}]"#).is_none(),
            "没有新增条目时不应写库"
        );
    }

    #[test]
    fn 只有真正多出来的条目才新增() {
        let (merged, added) = merge_value(
            Some(r#"[{"a":1}]"#),
            r#"[{"a":1},{"b":2},{"c":3}]"#,
        )
        .unwrap();
        assert_eq!(added, 2);
        assert!(merged.contains("\"b\"") && merged.contains("\"c\""), "{merged}");
        // 原有条目仍在且只出现一次
        assert_eq!(merged.matches("\"a\":1").count(), 1, "{merged}");
    }

    #[test]
    fn 键序不同的同一条目也能正确去重() {
        // serde_json 未启用 preserve_order，`Map` 是 BTreeMap：对象的键在反序列化时
        // 就被排序，因此 `{"a":1,"b":2}` 与 `{"b":2,"a":1}` 序列化后完全相同，
        // 会被正确识别为同一条目 —— 客户端重排键序不会让项目列表每次切换都翻倍。
        assert!(
            merge_value(Some(r#"[{"a":1,"b":2}]"#), r#"[{"b":2,"a":1}]"#).is_none(),
            "键序不同但内容相同的条目应判为重复"
        );
    }

    /// 最近打开列表存的是字符串数组（不是对象数组），必须同样能合并。
    #[test]
    fn 字符串数组条目也能合并() {
        let (merged, added) = merge_value(
            Some(r#"["C:\\p1"]"#),
            r#"["C:\\p1","C:\\p2"]"#,
        )
        .unwrap();
        assert_eq!(added, 1);
        assert!(merged.contains(r"C:\\p2"), "{merged}");
    }

    #[test]
    fn 两侧都不是数组时返回_none() {
        assert!(merge_value(Some(r#"{"a":1}"#), r#"{"b":2}"#).is_none());
        assert!(merge_value(Some("[]"), "not-json").is_none());
        assert!(merge_value(None, r#"["/a"]"#).is_none());
    }
}
