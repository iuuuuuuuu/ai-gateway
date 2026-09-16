//! Trae 本机账号自动发现（双应用）+ 套餐信息读取。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/commands/trae_apps.rs`。
//!
//! ## 两套 uid 体系（**最容易踩的坑**）
//!
//! - `storage.json` 里 `iCubeAuthInfo://icube-dc:<uid>` 的 uid 属于**账户中心
//!   （dc）id 空间**
//! - 账号池 / JWT `data.id` 的 uid 属于 **Cloud-IDE id 空间**
//!
//! 实测同一登录账号：`dc=199439841787403` vs `Cloud-IDE=2328112497170937`。
//! 直接用 dc uid 入库会产生**跨体系的重复账号**，且这个号永远签到失败。
//!
//! 因此本机登录账号的 Cloud-IDE uid 必须由**使用痕迹**推导：
//! 1. Trae CN：`storage.json` 的 `icube_gtm.users` 键名
//! 2. Trae Work：`state.vscdb` 里 `solo.mobile.allowControl` 的 per-uid
//!    `updatedTime` 最新者，辅以 `<uid>:*` / `:user:<uid>` 键名证据计数
//! 3. 两应用证据合并，取（最新时间, 证据数）最大者
//!
//! 推导失败时**拒绝入池**并标记 `uidConfident=false` —— 宁可不给候选，
//! 也不产生一个永远无法登录的重复账号。

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{json, Value};

/// `storage.json` 相对应用数据目录的后缀。
const STORAGE_SUFFIX: &str = r"User\globalStorage\storage.json";
/// `state.vscdb` 相对应用数据目录的后缀。
const VSCDB_SUFFIX: &str = r"User\globalStorage\state.vscdb";

/// 应用数据目录候选（按优先级）。
///
/// 每个应用给多个候选：客户端改过目录名（`TRAE SOLO` → `TRAE SOLO CN`），
/// 只认一个会让老安装的用户发现不到账号。
pub fn app_data_dirs(app_kind: &str) -> Vec<PathBuf> {
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    match app_kind {
        "TraeWork" => vec![
            PathBuf::from(&appdata).join("TRAE SOLO CN"),
            PathBuf::from(&appdata).join("TRAE SOLO"),
        ],
        "Trae" => vec![PathBuf::from(&appdata).join("Trae CN")],
        _ => vec![],
    }
}

/// 应用显示名。
pub fn app_label(app_kind: &str) -> &str {
    match app_kind {
        "TraeWork" => "Trae Work",
        "Trae" => "Trae",
        other => other,
    }
}

/// 读取应用的 `storage.json`（多候选目录取第一个存在的）。
pub fn read_storage_json(app_kind: &str) -> Option<Value> {
    for dir in app_data_dirs(app_kind) {
        let path = dir.join(STORAGE_SUFFIX);
        if path.is_file() {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(v) = serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// 从 `storage.json` 提取账户中心（dc）uid 列表。
///
/// 键名形如 `iCubeAuthInfo://icube-dc:<uid>`。**这是 dc id 空间**，
/// 不能与账号池的 Cloud-IDE uid 直接比对。
pub fn extract_dc_uids(storage: &Value) -> Vec<String> {
    let mut uids = Vec::new();
    if let Some(obj) = storage.as_object() {
        for key in obj.keys() {
            if let Some(uid) = key.strip_prefix("iCubeAuthInfo://icube-dc:") {
                let uid = uid.trim();
                if !uid.is_empty() && uid.chars().all(|c| c.is_ascii_digit()) {
                    uids.push(uid.to_string());
                }
            }
        }
    }
    uids.sort();
    uids.dedup();
    uids
}

/// 单个 Cloud-IDE uid 的本机使用证据。
#[derive(Default, Clone, Debug)]
struct UidEvidence {
    /// 最新使用时间（Unix 毫秒；无时间戳证据为 0）。
    latest_ts_ms: i64,
    /// 出现次数（键名证据计数）。
    count: i64,
}

/// 是否为合法 uid 形态的纯数字串（Cloud-IDE id 空间）。
///
/// 长度窗口是 **15~19 位**，不是更窄的范围：实测同一台机器上同时存在
/// - 16 位（`icube_gtm.users` 键名、Cloud-IDE JWT `data.id`，如 `1883919207380040`）
/// - 19 位（`solo.mobile.allowControl` 的 uid 键，如 `7679751654497928200`）
///
/// 早期只认 15~16 位，导致 19 位的账号在发现阶段被**静默丢弃**
/// （`solo.mobile.allowControl` 正是最强证据来源，丢掉它等于漏掉整个账号）。
/// 上限取 19 位：账号中心（dc）uid 同样是长数字串，靠长度区分不可靠，
/// 因此这里只做「形态像 uid」的判断，跨体系去重由 [`discover_all`] 的证据排序负责。
fn is_uid_token(token: &str) -> bool {
    (15..=19).contains(&token.chars().count()) && token.chars().all(|c| c.is_ascii_digit())
}

/// 把 `YYYY-MM` 转为近似时间戳（当月 1 日 0 点，毫秒），用于与毫秒时间戳同维度比较。
fn month_to_ts_ms(month: &str) -> i64 {
    let parts: Vec<&str> = month.split('-').collect();
    if parts.len() != 2 {
        return 0;
    }
    let (Ok(year), Ok(m)) = (parts[0].parse::<i32>(), parts[1].parse::<u32>()) else {
        return 0;
    };
    if !(1..=12).contains(&m) {
        return 0;
    }
    chrono::NaiveDate::from_ymd_opt(year, m, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc().timestamp_millis())
        .unwrap_or(0)
}

/// 从 `state.vscdb`（SQLite `ItemTable`）提取 per-uid 使用证据。
///
/// 覆盖的键模式（本机实测）：
/// - `solo.mobile.allowControl`：JSON `{uid: {updatedTime}}`，含精确毫秒时间戳（最强证据）
/// - `solo-lite-mode-state-map-<uid>`
/// - `<uid>:...` 键名前缀（如 `<uid>:AI.agent.model...`）
/// - `*:user:<uid>[:YYYY-MM]`（如 `commercial-banner-popup:...:user:<uid>:2026-09`）
fn vscdb_uid_evidence(app_kind: &str) -> HashMap<String, UidEvidence> {
    let mut out: HashMap<String, UidEvidence> = HashMap::new();
    // **必须遍历全部候选目录**，不能取第一个存在的就收工：客户端改名后
    // （`TRAE SOLO` → `TRAE SOLO CN`）两个目录会**同时留在磁盘上**，各自存着
    // 不同账号的使用痕迹。只读第一个会让另一个目录里的账号在发现阶段整体消失
    // —— 实测本机 `TRAE SOLO CN` 有 16 位 uid、`TRAE SOLO` 有 19 位 uid，
    // 早期实现只发现前者。
    for db_path in app_data_dirs(app_kind)
        .into_iter()
        .map(|d| d.join(VSCDB_SUFFIX))
        .filter(|p| p.is_file())
    {
        merge_vscdb_into(&mut out, &db_path);
    }
    out
}

/// 把单个 `state.vscdb` 的证据并入 `out`（同一 uid 取时间最大值、计数累加）。
fn merge_vscdb_into(out: &mut HashMap<String, UidEvidence>, db_path: &std::path::Path) {
    // 只读打开：客户端可能正在运行并持有写锁
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return;
    };
    let Ok(mut stmt) = conn.prepare("SELECT key, value FROM ItemTable") else {
        return;
    };
    let Ok(rows) = stmt.query_map([], |row| {
        let key: String = row.get(0)?;
        let value: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
        Ok((key, value))
    }) else {
        return;
    };

    for (key, value) in rows.flatten() {
        // 1) solo.mobile.allowControl：JSON {uid: {updatedTime}} —— 精确时间戳
        if key == "solo.mobile.allowControl" {
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&value) {
                for (uid, info) in map {
                    if !is_uid_token(&uid) {
                        continue;
                    }
                    let ts = info.get("updatedTime").and_then(Value::as_i64).unwrap_or(0);
                    let entry = out.entry(uid).or_default();
                    entry.latest_ts_ms = entry.latest_ts_ms.max(ts);
                    // 权重 2：这是最直接的「谁在用」证据
                    entry.count += 2;
                }
            }
            continue;
        }
        // 2) solo-lite-mode-state-map-<uid>
        if let Some(rest) = key.strip_prefix("solo-lite-mode-state-map-") {
            let uid = rest.trim();
            if is_uid_token(uid) {
                out.entry(uid.to_string()).or_default().count += 2;
            }
            continue;
        }
        // 3) 键名冒号分段扫描
        let tokens: Vec<&str> = key.split(':').collect();
        for (i, token) in tokens.iter().enumerate() {
            let t = token.trim();
            if !is_uid_token(t) {
                continue;
            }
            let entry = out.entry(t.to_string()).or_default();
            entry.count += 1;
            // 紧随 uid 的 YYYY-MM 段 → 月度时间证据
            if let Some(next) = tokens.get(i + 1) {
                let nt = next.trim();
                if nt.len() == 7 && nt.chars().nth(4) == Some('-') {
                    let ts = month_to_ts_ms(nt);
                    if ts > entry.latest_ts_ms {
                        entry.latest_ts_ms = ts;
                    }
                }
            }
        }
    }
}

/// 从 `storage.json` 的 `icube_gtm.users` 键名提取 Cloud-IDE uid（Trae CN 专用证据）。
fn gtm_users_evidence(storage: &Value) -> HashMap<String, UidEvidence> {
    let mut out = HashMap::new();
    let Some(users) = storage.get("icube_gtm.users").and_then(Value::as_object) else {
        return out;
    };
    for (uid, info) in users {
        let uid = uid.trim();
        if !is_uid_token(uid) {
            continue;
        }
        let ts = info
            .get("updatedTime")
            .or_else(|| info.get("updated_at"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let entry = out.entry(uid.to_string()).or_insert_with(UidEvidence::default);
        entry.latest_ts_ms = entry.latest_ts_ms.max(ts);
        entry.count += 2;
    }
    out
}

/// 单个应用发现的账号。
#[derive(Debug, Clone)]
pub struct DiscoveredAccount {
    pub app_kind: &'static str,
    pub app_label: &'static str,
    /// 推导出的 Cloud-IDE uid（推导失败时为空）。
    pub user_id: String,
    /// 账户中心 uid（仅供排查展示，**不可入库**）。
    pub dc_uid: Option<String>,
    /// uid 是否可信。为假时**禁止入池**。
    pub uid_confident: bool,
    /// 该账号在本机的使用证据（时间戳毫秒 + 计数）。
    pub evidence_ts_ms: i64,
    pub evidence_count: i64,
    /// 是否已在 Trae 账号库中。
    pub in_pool: bool,
    /// 套餐身份（如 Free / Lite / Pro）。
    pub pay_identity: Option<String>,
}

/// 读取套餐身份：`storage.json` 键 `iCubeServerData://icube.cloudide`（明文 JSON），
/// `entitlementInfo.identityStr` / `identity` 与 `ide_user_pay_status` 接口同源。
pub fn read_entitlement(app_kind: &str) -> Option<Value> {
    let storage = read_storage_json(app_kind)?;
    let raw = storage.get("iCubeServerData://icube.cloudide")?;
    // 该键的值可能是 JSON 字符串（需二次解析）或已经是对象
    let parsed: Value = match raw {
        Value::String(s) => serde_json::from_str(s).ok()?,
        other => other.clone(),
    };
    let info = parsed
        .get("entitlementInfo")
        .or_else(|| parsed.get("entitlement_info"))
        .cloned()
        .unwrap_or(parsed);
    let identity = info
        .get("identityStr")
        .or_else(|| info.get("identity"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(json!({
        "identity": identity,
        "raw": info,
    }))
}

/// 合并全部候选目录的 `icube_gtm.users` 证据。
///
/// 与 [`vscdb_uid_evidence`] 同理：改名后的两个目录会并存且可能各自记录了
/// 不同账号，只看第一个会把另一个目录的账号漏掉。
fn gtm_users_evidence_all(app_kind: &str) -> HashMap<String, UidEvidence> {
    let mut out: HashMap<String, UidEvidence> = HashMap::new();
    for dir in app_data_dirs(app_kind) {
        let path = dir.join(STORAGE_SUFFIX);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(storage) = serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}'))
        else {
            continue;
        };
        for (uid, ev) in gtm_users_evidence(&storage) {
            let entry = out.entry(uid).or_default();
            entry.latest_ts_ms = entry.latest_ts_ms.max(ev.latest_ts_ms);
            entry.count += ev.count;
        }
    }
    out
}

/// 发现指定应用的本机登录账号。
pub fn discover_app(app_kind: &'static str, pool_uids: &[String]) -> Vec<DiscoveredAccount> {
    let mut results = Vec::new();
    let storage = read_storage_json(app_kind);
    let dc_uids = storage
        .as_ref()
        .map(extract_dc_uids)
        .unwrap_or_default();
    let entitlement = read_entitlement(app_kind);
    let pay_identity = entitlement
        .as_ref()
        .and_then(|e| e.get("identity"))
        .and_then(Value::as_str)
        .map(str::to_string);

    // 合并两路证据（各自都已跨全部候选目录）
    let mut evidence: HashMap<String, UidEvidence> = vscdb_uid_evidence(app_kind);
    for (uid, ev) in gtm_users_evidence_all(app_kind) {
        let entry = evidence.entry(uid).or_default();
        entry.latest_ts_ms = entry.latest_ts_ms.max(ev.latest_ts_ms);
        entry.count += ev.count;
    }

    if evidence.is_empty() {
        // 推导失败：回退展示 dc uid，但标记不可信并禁止入池
        if let Some(dc) = dc_uids.first() {
            results.push(DiscoveredAccount {
                app_kind,
                app_label: app_label(app_kind),
                user_id: String::new(),
                dc_uid: Some(dc.clone()),
                uid_confident: false,
                evidence_ts_ms: 0,
                evidence_count: 0,
                in_pool: false,
                pay_identity: pay_identity.clone(),
            });
        }
        return results;
    }

    // 取（最新时间, 证据数）最大者
    let mut ranked: Vec<(String, UidEvidence)> = evidence.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.latest_ts_ms
            .cmp(&a.1.latest_ts_ms)
            .then(b.1.count.cmp(&a.1.count))
    });
    for (uid, ev) in ranked {
        results.push(DiscoveredAccount {
            app_kind,
            app_label: app_label(app_kind),
            in_pool: pool_uids.iter().any(|u| u == &uid),
            user_id: uid,
            dc_uid: dc_uids.first().cloned(),
            uid_confident: true,
            evidence_ts_ms: ev.latest_ts_ms,
            evidence_count: ev.count,
            pay_identity: pay_identity.clone(),
        });
    }
    results
}

/// 发现两个 Trae 应用的全部本机账号。
///
/// 同一 uid 可能被两个应用同时发现（同一账号在 Trae Work 与 Trae 都登录过）——
/// 按 uid 合并，保留证据更强的一份，并记录它出现在哪些应用里。
pub fn discover_all() -> Vec<Value> {
    let pool: Vec<String> = crate::modules::trae_account::load_accounts()
        .iter()
        .filter_map(|a| a.get("user_id").and_then(Value::as_str).map(str::to_string))
        .collect();

    let mut merged: HashMap<String, DiscoveredAccount> = HashMap::new();
    let mut orphans: Vec<DiscoveredAccount> = Vec::new();
    let mut apps_seen: HashMap<String, Vec<String>> = HashMap::new();

    for app_kind in ["TraeWork", "Trae"] {
        for found in discover_app(app_kind, &pool) {
            if !found.uid_confident || found.user_id.is_empty() {
                orphans.push(found);
                continue;
            }
            apps_seen
                .entry(found.user_id.clone())
                .or_default()
                .push(found.app_label.to_string());
            match merged.get(&found.user_id) {
                Some(existing) if existing.evidence_ts_ms >= found.evidence_ts_ms => {}
                _ => {
                    merged.insert(found.user_id.clone(), found);
                }
            }
        }
    }

    let mut out: Vec<Value> = merged
        .into_values()
        .map(|found| {
            json!({
                "userId": found.user_id,
                "dcUid": found.dc_uid,
                "uidConfident": true,
                "appKind": found.app_kind,
                "appLabel": found.app_label,
                "apps": apps_seen.get(&found.user_id).cloned().unwrap_or_default(),
                "evidenceTsMs": found.evidence_ts_ms,
                "evidenceCount": found.evidence_count,
                "inPool": found.in_pool,
                "payIdentity": found.pay_identity,
            })
        })
        .collect();
    // 证据强的排前面
    out.sort_by(|a, b| {
        b.get("evidenceTsMs")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .cmp(&a.get("evidenceTsMs").and_then(Value::as_i64).unwrap_or(0))
    });

    // 推导失败的候选附在末尾（前端据此提示「无法确认账号，请手动登录」）
    for orphan in orphans {
        out.push(json!({
            "userId": "",
            "dcUid": orphan.dc_uid,
            "uidConfident": false,
            "appKind": orphan.app_kind,
            "appLabel": orphan.app_label,
            "apps": [orphan.app_label],
            "evidenceTsMs": 0,
            "evidenceCount": 0,
            "inPool": false,
            "payIdentity": orphan.pay_identity,
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_token_判定_15_到_19_位纯数字() {
        assert!(is_uid_token("2328112497170937"), "16 位（JWT data.id / gtm 键名）");
        assert!(is_uid_token("1883919207380040"), "16 位实测样本");
        assert!(is_uid_token("123456789012345"), "15 位");
        assert!(is_uid_token("12345678901234567"), "17 位");
        assert!(is_uid_token("123456789012345678"), "18 位");
        assert!(
            is_uid_token("7679751654497928200"),
            "19 位（solo.mobile.allowControl 实测样本）—— 早期只认 15~16 位时这个账号会被漏掉"
        );
        assert!(!is_uid_token("12345678901234"), "14 位不是 uid");
        assert!(!is_uid_token("12345678901234567890"), "20 位超出上限");
        assert!(!is_uid_token("12345678901234a"));
        assert!(!is_uid_token(""));
    }

    #[test]
    fn 十九位_uid_的证据能被收集() {
        // 回归：solo.mobile.allowControl 里 19 位 uid 必须能进入证据表，
        // 否则「本机发现账号」会整条漏掉该账号。
        let storage = json!({
            "icube_gtm.users": {
                "7679751654497928200": {"updatedTime": 1788081577344i64},
            }
        });
        let evidence = gtm_users_evidence(&storage);
        assert!(
            evidence.contains_key("7679751654497928200"),
            "19 位 uid 必须被 gtm 证据收集，实际: {evidence:?}"
        );
        assert_eq!(evidence["7679751654497928200"].latest_ts_ms, 1788081577344);
    }

    #[test]
    fn 提取_dc_uid_只认合法键名() {
        let storage = json!({
            "iCubeAuthInfo://icube-dc:199439841787403": {},
            "iCubeAuthInfo://icube-dc:notanumber": {},
            "iCubeAuthInfo://icube-dc:": {},
            "unrelated": {},
        });
        assert_eq!(extract_dc_uids(&storage), vec!["199439841787403"]);
    }

    #[test]
    fn 月份转时间戳() {
        // 独立算出期望值，避免把实现里的错误常量抄进断言
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 9, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis();
        assert_eq!(month_to_ts_ms("2026-09"), expected);
        // 不同月份应给出不同时间戳，且递增
        assert!(month_to_ts_ms("2026-10") > month_to_ts_ms("2026-09"));
        assert!(month_to_ts_ms("2027-01") > month_to_ts_ms("2026-12"));
        // 非法输入返回 0
        assert_eq!(month_to_ts_ms("2026"), 0);
        assert_eq!(month_to_ts_ms("2026-13"), 0);
        assert_eq!(month_to_ts_ms("2026-00"), 0);
        assert_eq!(month_to_ts_ms("abc-de"), 0);
        assert_eq!(month_to_ts_ms(""), 0);
    }

    #[test]
    fn 键名证据扫描_识别_uid_前缀与_user_模式() {
        // 直接验证扫描逻辑的关键分支（不依赖真实 vscdb）
        let key = "2328112497170937:AI.agent.model";
        let tokens: Vec<&str> = key.split(':').collect();
        assert!(is_uid_token(tokens[0].trim()));

        let key2 = "commercial-banner-popup:xxx:user:2328112497170937:2026-09";
        let tokens2: Vec<&str> = key2.split(':').collect();
        let found: Vec<&str> = tokens2.iter().map(|t| t.trim()).filter(|t| is_uid_token(t)).collect();
        assert_eq!(found, vec!["2328112497170937"]);
        // 紧随其后的 YYYY-MM 应被识别为时间证据
        let idx = tokens2.iter().position(|t| t.trim() == "2328112497170937").unwrap();
        assert_eq!(tokens2[idx + 1].trim(), "2026-09");
    }

    #[test]
    fn 应用数据目录候选_覆盖改名前后() {
        let work = app_data_dirs("TraeWork");
        assert_eq!(work.len(), 2, "客户端改过目录名，两个候选都要试");
        assert!(work[0].to_string_lossy().ends_with("TRAE SOLO CN"));
        assert!(work[1].to_string_lossy().ends_with("TRAE SOLO"));

        let cn = app_data_dirs("Trae");
        assert_eq!(cn.len(), 1);
        assert!(cn[0].to_string_lossy().ends_with("Trae CN"));

        assert!(app_data_dirs("unknown").is_empty());
    }

    #[test]
    fn 应用显示名() {
        assert_eq!(app_label("TraeWork"), "Trae Work");
        assert_eq!(app_label("Trae"), "Trae");
        assert_eq!(app_label("other"), "other");
    }

    #[test]
    fn gtm_证据跨全部候选目录合并() {
        // 回归：客户端改名后 `TRAE SOLO` 与 `TRAE SOLO CN` 会**并存**，
        // 各自可能记录不同账号。只读第一个目录会让另一个目录的账号整体消失。
        //
        // 这里不依赖真实 APPDATA：直接构造两个目录，临时把 APPDATA 指过去。
        let base = std::env::temp_dir().join(format!(
            "ai-gateway-trae-gtm-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let write_storage = |dir: &str, uid: &str, ts: i64| {
            let p = base.join(dir).join(r"User\globalStorage");
            std::fs::create_dir_all(&p).unwrap();
            let body = json!({
                "icube_gtm.users": { uid: {"updatedTime": ts} }
            });
            std::fs::write(
                p.join("storage.json"),
                serde_json::to_string(&body).unwrap(),
            )
            .unwrap();
        };
        write_storage("TRAE SOLO CN", "1883919207380040", 1_778_078_246_409);
        write_storage("TRAE SOLO", "7679751654497928200", 1_788_081_577_344);

        let old = std::env::var_os("APPDATA");
        std::env::set_var("APPDATA", &base);
        let merged = gtm_users_evidence_all("TraeWork");
        match old {
            Some(v) => std::env::set_var("APPDATA", v),
            None => std::env::remove_var("APPDATA"),
        }
        let _ = std::fs::remove_dir_all(&base);

        assert_eq!(merged.len(), 2, "两个目录的账号都要被看到，实际: {merged:?}");
        assert_eq!(merged["1883919207380040"].latest_ts_ms, 1_778_078_246_409);
        assert_eq!(merged["7679751654497928200"].latest_ts_ms, 1_788_081_577_344);
    }

    #[test]
    fn 发现结果在无本机数据时不panic() {
        // 本机可能装了也可能没装；两种情况都必须返回结构良好的结果
        let found = discover_all();
        for item in &found {
            assert!(item.get("uidConfident").is_some());
            assert!(item.get("appLabel").is_some());
            // 不可信的候选必须 uid 为空（禁止入池）
            if item["uidConfident"] == false {
                assert_eq!(item["userId"], "");
            }
        }
    }
}
