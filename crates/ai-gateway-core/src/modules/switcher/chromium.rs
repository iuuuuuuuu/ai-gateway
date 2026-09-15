//! chromium 布局快照（豆包）：多 Profile 精准备份/恢复 + 快照版本校验 + 活跃 Profile 修复。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/switcher/chromium.rs`。
//!
//! ## 白名单依据
//!
//! - **必选** `Local State`（活跃 Profile 指针 `profile.last_used` + cookie 解密密钥
//!   元数据，缺失则恢复后 cookie 无法解密）、`<每个 Profile>/Network/Cookies*`
//!   （登录 cookie，含 journal）、`<每个 Profile>/Local Storage/leveldb/`（web 侧登录 KV）
//! - **建议** `<每个 Profile>/Session Storage/`、`DoubaoStorage/`、`Preferences`、
//!   `saman_shell_db_storage/`，根级 `saman_app_state` / `saman_shell_db_storage` /
//!   `Last Version`
//! - **排除** `<每个 Profile>/IndexedDB/`（体积大，默认排除；可经开关纳入）
//!
//! ## 为什么必须遍历全部 Profile 而不是只抓 Default
//!
//! 豆包自带账号隔离（`saman.account_isolation_config`），登录会话可能位于**任意**
//! Profile。只抓 `Default` 会漏掉活跃会话，恢复后客户端打开的活跃 Profile 未登录 ——
//! 这是实测根因，不是理论担忧。

use std::path::{Path, PathBuf};

use serde_json::json;

use super::copy;
use super::{ProgressSink, Session, StepStatus};

/// 快照元数据文件名（恢复前完整性校验用）。
const SNAPSHOT_META: &str = "snapshot_meta.json";
/// 本工具支持的快照 schema 版本。
const SCHEMA_VERSION: i64 = 1;

/// Profile 目录枚举：`Default` + `Profile *`，按名排序。
///
/// 刻意排除 `System Profile` / `Guest Profile`（Chromium 内部用途，不含用户登录态）。
pub fn profile_dirs(base: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n == "Default" || n.starts_with("Profile "))
                .unwrap_or(false)
        })
        .collect();
    out.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    out
}

/// Cookies 文件收集（新旧布局归一）。
///
/// 新布局 `<profile>\Network\Cookies*`（含 `-journal`）；旧布局兜底
/// `<profile>\Cookies*`。无论来源在哪，快照内统一归位到 `<profile>\Network\`。
fn cookie_files(profile_dir: &Path) -> Vec<PathBuf> {
    let network = profile_dir.join("Network");
    let base = if network.join("Cookies").exists() {
        network
    } else {
        profile_dir.to_path_buf()
    };
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("Cookies"))
                    .unwrap_or(false)
        })
        .collect();
    out.sort();
    out
}

/// 读文本并去掉 BOM 与首尾空白。
fn read_trimmed(path: &Path) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().trim_start_matches('\u{feff}').trim().to_string())
        .unwrap_or_default()
}

/// 备份豆包登录态。
pub fn backup_chromium(sess: &Session, slot: &str, sink: &dyn ProgressSink) -> Result<(), String> {
    let dest = sess.prof.slot_dir(slot);
    let src = &sess.prof.data_dir;
    if !src.exists() {
        sink.step("backup", StepStatus::Skip, "当前数据目录不存在，跳过备份");
        return Ok(());
    }
    // 单代回滚保护（与 icube 布局同理）
    copy::rotate_bak(&dest, slot, sink);
    std::fs::create_dir_all(&dest).map_err(|e| format!("创建快照目录失败: {e}"))?;
    let mut copied = 0usize;

    // 必选 1：Local State（活跃 Profile 指针 + cookie 解密密钥元数据）
    if copy::copy_snapshot_item(&src.join("Local State"), &dest.join("Local State")) {
        copied += 1;
    }

    // 多 Profile 遍历（结构相对 User Data 镜像存放，恢复时对称回写）
    let profiles = profile_dirs(src);
    for p in &profiles {
        let n = p.file_name().unwrap_or_default().to_string_lossy().to_string();
        // 必选 2：Cookies*（统一归位到 <profile>\Network\）
        for ck in cookie_files(p) {
            let name = ck.file_name().unwrap_or_default().to_string_lossy().to_string();
            if copy::copy_snapshot_item(&ck, &dest.join(&n).join("Network").join(&name)) {
                copied += 1;
            }
        }
        // 必选 3：Local Storage\leveldb
        if copy::copy_snapshot_item(
            &p.join("Local Storage").join("leveldb"),
            &dest.join(&n).join("Local Storage").join("leveldb"),
        ) {
            copied += 1;
        }
        // 建议项
        for item in ["Session Storage", "DoubaoStorage", "Preferences"] {
            if copy::copy_snapshot_item(&p.join(item), &dest.join(&n).join(item)) {
                copied += 1;
            }
        }
        // saman 账号体系客户端级数据库 per-profile 存一份 —— 仅抓根级会丢各 Profile
        // 的 shell 侧账号状态，切换后可能触发客户端重建该 Profile 的账号数据。
        if copy::copy_snapshot_item(
            &p.join("saman_shell_db_storage"),
            &dest.join(&n).join("saman_shell_db_storage"),
        ) {
            copied += 1;
        }
        // 可选：IndexedDB（对话历史等完整状态；体积大，默认排除）
        if sess.include_indexeddb
            && copy::copy_snapshot_item(&p.join("IndexedDB"), &dest.join(&n).join("IndexedDB"))
        {
            copied += 1;
        }
    }
    if profiles.is_empty() {
        sink.step(
            "backup",
            StepStatus::Warn,
            "未发现任何 Profile 目录（Default / Profile N），豆包可能从未启动过",
        );
    }

    // 根级 saman 账号体系状态（文件/目录均有，拷贝函数自适应）
    for item in ["saman_app_state", "saman_shell_db_storage"] {
        if copy::copy_snapshot_item(&src.join(item), &dest.join(item)) {
            copied += 1;
        }
    }

    // 快照版本元数据（恢复前校验用，防豆包升级后旧快照损坏）
    let snapshot_ver = if copy::copy_snapshot_item(&src.join("Last Version"), &dest.join("Last Version"))
    {
        copied += 1;
        read_trimmed(&dest.join("Last Version"))
    } else {
        String::new()
    };
    // 必须无 BOM 写入：读取侧用 serde_json 解析此文件，BOM 会让解析失败并静默降级
    // 成 schema_version = 0（于是每次恢复都被判为「不兼容」而中止）。
    let meta = json!({
        "schemaVersion": SCHEMA_VERSION,
        "layout": "chromium",
        "app": sess.prof.app_name,
        "chromiumVersion": snapshot_ver,
        "includeIndexedDB": sess.include_indexeddb,
        "profileCount": profiles.len(),
        "createdAt": crate::modules::config::utc_iso(),
    });
    if let Err(e) = std::fs::write(
        dest.join(SNAPSHOT_META),
        serde_json::to_string(&meta).unwrap_or_default(),
    ) {
        sink.step(
            "backup",
            StepStatus::Warn,
            &format!("快照元数据写入失败（不影响快照本身）: {e}"),
        );
    }

    if copied == 0 {
        sink.step(
            "backup",
            StepStatus::Warn,
            "未发现任何可备份的登录态文件（豆包可能未登录或数据目录为空）",
        );
    } else {
        sink.step(
            "backup",
            StepStatus::Ok,
            &format!(
                "已备份当前登录态到 {slot} ({copied} 项, {} 个 Profile)",
                profiles.len()
            ),
        );
    }
    Ok(())
}

/// 恢复前快照完整性校验（任一硬失败即中止恢复）。
///
/// 四项检查：
/// 1. `schemaVersion`：本工具仅支持 1，不兼容直接中止（旧版工具的快照无此文件，仅警告）
/// 2. leveldb 完整性：快照内每个 Profile 的 `Local Storage/leveldb` 的 `CURRENT`
///    必须存在，且它指向的 `MANIFEST` 文件也在快照内（多 Profile 逐个校验）
/// 3. 登录 Cookie 存在性：所有 Profile 均无 Cookies 时警告（可能是未登录态保存）
/// 4. 版本差异：快照 Chromium 版本 ≠ 当前安装版本时警告（继续恢复）
pub fn test_snapshot_integrity(
    sess: &Session,
    src: &Path,
    sink: &dyn ProgressSink,
) -> Result<(), String> {
    // ① schemaVersion
    let snapshot_ver = read_trimmed(&src.join("Last Version"));
    let meta_file = src.join(SNAPSHOT_META);
    if meta_file.exists() {
        let meta: Result<serde_json::Value, _> =
            serde_json::from_str(&std::fs::read_to_string(&meta_file).unwrap_or_default());
        if let Ok(meta) = meta {
            if let Some(v) = meta.get("schemaVersion").and_then(serde_json::Value::as_i64) {
                if v != SCHEMA_VERSION {
                    let msg = format!(
                        "快照 schemaVersion={v}，本工具仅支持 {SCHEMA_VERSION}：快照由不兼容版本生成，已中止恢复（请重新登录该账号并保存登录态）"
                    );
                    sink.step("restore", StepStatus::Error, &msg);
                    return Err(msg);
                }
            }
        }
    } else {
        sink.step(
            "restore",
            StepStatus::Warn,
            "快照缺少版本元数据（旧版本工具生成），已跳过 schemaVersion 校验",
        );
    }

    // ② leveldb 完整性 + ②b Cookies 统计
    let mut has_cookies = false;
    for p in profile_dirs(src) {
        let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
        let ldb = p.join("Local Storage").join("leveldb");
        if ldb.is_dir() {
            let current_file = ldb.join("CURRENT");
            if !current_file.exists() {
                let msg = format!(
                    "快照 {name}/Local Storage/leveldb 缺少 CURRENT 文件，疑似不完整/损坏，已中止恢复（请重新登录该账号并保存登录态）"
                );
                sink.step("restore", StepStatus::Error, &msg);
                return Err(msg);
            }
            let manifest_name = read_trimmed(&current_file);
            if !manifest_name.is_empty() && !ldb.join(&manifest_name).exists() {
                let msg = format!(
                    "快照 {name}/Local Storage/leveldb CURRENT 指向的 {manifest_name} 缺失，疑似不完整/损坏，已中止恢复（请重新登录该账号并保存登录态）"
                );
                sink.step("restore", StepStatus::Error, &msg);
                return Err(msg);
            }
        }
        if p.join("Network").join("Cookies").exists() || p.join("Cookies").exists() {
            has_cookies = true;
        }
    }
    if !has_cookies {
        sink.step(
            "restore",
            StepStatus::Warn,
            "快照内所有 Profile 均未检测到 Cookies 文件——该快照可能保存的是未登录状态，恢复后豆包将未登录",
        );
    }

    // ③ 版本差异警告（不阻断）
    let current_ver = read_trimmed(&sess.prof.data_dir.join("Last Version"));
    if !snapshot_ver.is_empty() && !current_ver.is_empty() && snapshot_ver != current_ver {
        sink.step(
            "restore",
            StepStatus::Warn,
            &format!(
                "豆包版本已从快照的 {snapshot_ver} 升级到 {current_ver}：旧快照通常兼容，若恢复后登录异常请重新登录并保存登录态"
            ),
        );
    }
    Ok(())
}

/// 活跃 Profile 指针修复。
///
/// `Local State` 的 `profile.last_used` 指向快照外 Profile 时，改写为快照内存在的
/// Profile（优先 `Default`）；`last_active_profiles` 同步过滤。
///
/// 为什么需要：恢复后客户端会打开 `last_used` 指定的 Profile。若它不在快照内，
/// 客户端打开的是一个**空 Profile**，表现为「切换成功但没登录」——这是实测根因之一。
///
/// 显式 UTF-8 读取 + 无 BOM 临时文件 + rename 原子替换。
fn repair_local_state_active_profile(
    userdata_dir: &Path,
    snapshot_profiles: &[String],
    sink: &dyn ProgressSink,
) {
    if snapshot_profiles.is_empty() {
        return;
    }
    let ls_path = userdata_dir.join("Local State");
    if !ls_path.exists() {
        return;
    }
    let result = (|| -> Result<(), String> {
        let raw = std::fs::read_to_string(&ls_path).map_err(|e| e.to_string())?;
        let mut j: serde_json::Value = serde_json::from_str(raw.trim_start_matches('\u{feff}'))
            .map_err(|e| e.to_string())?;
        let Some(profile) = j.get_mut("profile").and_then(|v| v.as_object_mut()) else {
            return Ok(());
        };
        let used = profile
            .get("last_used")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if used.is_empty() || snapshot_profiles.iter().any(|s| s == &used) {
            return Ok(());
        }
        let fallback = if snapshot_profiles.iter().any(|s| s == "Default") {
            "Default".to_string()
        } else {
            snapshot_profiles[0].clone()
        };
        profile.insert("last_used".to_string(), json!(fallback));
        // last_active_profiles 同步过滤到快照内存在的 Profile，
        // 避免客户端恢复陈旧的多开列表。
        if let Some(lap) = profile
            .get("last_active_profiles")
            .and_then(|v| v.as_array())
            .cloned()
        {
            let filtered: Vec<serde_json::Value> = lap
                .into_iter()
                .filter(|v| {
                    v.as_str()
                        .map(|s| snapshot_profiles.iter().any(|s2| s2 == s))
                        .unwrap_or(false)
                })
                .collect();
            if !filtered.is_empty() {
                profile.insert("last_active_profiles".to_string(), json!(filtered));
            }
        }
        // 无 BOM UTF-8 写临时文件再 rename 替换（避免 Chromium 解析异常）
        let tmp = userdata_dir.join("Local State.aiwtmp");
        std::fs::write(
            &tmp,
            serde_json::to_string_pretty(&j).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &ls_path).map_err(|e| e.to_string())?;
        sink.step(
            "restore",
            StepStatus::Info,
            &format!(
                "快照活跃 Profile '{used}' 不在快照内，已改写 Local State 指向 '{fallback}'（防客户端启动打开空 Profile 未登录）"
            ),
        );
        Ok(())
    })();
    if let Err(e) = result {
        sink.step(
            "restore",
            StepStatus::Warn,
            &format!("Local State 活跃 Profile 校验/改写失败（忽略）: {e}"),
        );
    }
}

/// 恢复豆包登录态。
pub fn restore_chromium(sess: &Session, slot: &str, sink: &dyn ProgressSink) -> Result<(), String> {
    let (src, _slot_label) = copy::resolve_slot(sess, slot, sink)?;
    // 恢复前完整性校验（schemaVersion / leveldb CURRENT→MANIFEST / Cookies / 版本差异）
    test_snapshot_integrity(sess, &src, sink)?;
    let dest = sess.prof.data_dir.clone();
    std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
    let mut restored = 0usize;

    // 顶层：Local State / saman_* / Last Version
    for item in [
        "Local State",
        "saman_app_state",
        "saman_shell_db_storage",
        "Last Version",
    ] {
        if copy::copy_snapshot_item(&src.join(item), &dest.join(item)) {
            restored += 1;
        }
    }

    // 多 Profile 对称回写：快照里有哪些 Profile 就恢复哪些（旧版快照只有 Default 也适用）
    let snap_profiles = profile_dirs(&src);
    let mut profile_names: Vec<String> = Vec::new();
    for p in &snap_profiles {
        let n = p.file_name().unwrap_or_default().to_string_lossy().to_string();
        // Cookies*（快照统一存于 <profile>\Network\；兼容旧布局 <profile>\Cookies*）
        for ck in cookie_files(p) {
            let name = ck.file_name().unwrap_or_default().to_string_lossy().to_string();
            if copy::copy_snapshot_item(&ck, &dest.join(&n).join("Network").join(&name)) {
                restored += 1;
            }
        }
        if copy::copy_snapshot_item(
            &p.join("Local Storage").join("leveldb"),
            &dest.join(&n).join("Local Storage").join("leveldb"),
        ) {
            restored += 1;
        }
        for item in [
            "Session Storage",
            "DoubaoStorage",
            "Preferences",
            "saman_shell_db_storage",
        ] {
            if copy::copy_snapshot_item(&p.join(item), &dest.join(&n).join(item)) {
                restored += 1;
            }
        }
        // 快照内含 IndexedDB 时一并恢复（无论当前开关状态，保证快照内容完整回写）
        if p.join("IndexedDB").exists()
            && copy::copy_snapshot_item(&p.join("IndexedDB"), &dest.join(&n).join("IndexedDB"))
        {
            restored += 1;
        }
        profile_names.push(n);
    }

    // 快照活跃 Profile 指针修复（防客户端启动打开快照外的空 Profile → 未登录）
    repair_local_state_active_profile(&dest, &profile_names, sink);

    sink.step(
        "restore",
        StepStatus::Ok,
        &format!(
            "已恢复账号 {slot} 的登录态 ({restored} 项, {} 个 Profile)",
            snap_profiles.len()
        ),
    );
    Ok(())
}

/// 读取快照元数据（供界面展示「快照由哪个版本、何时创建」）。
pub fn snapshot_meta(sess: &Session, slot: &str) -> Option<serde_json::Value> {
    let path = sess.prof.slot_dir(slot).join(SNAPSHOT_META);
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(raw.trim_start_matches('\u{feff}')).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::app_profile::TargetApp;
    use crate::modules::switcher::{Action, RunArgs};

    struct QuietSink;
    impl ProgressSink for QuietSink {
        fn step(&self, _: &str, _: StepStatus, _: &str) {}
    }

    fn session(tag: &str) -> Session {
        let store = std::env::temp_dir().join(format!(
            "ai-gateway-chromium-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&store);
        let args = RunArgs {
            action: Action::Switch,
            target_app: TargetApp::Doubao,
            user_id: Some("123".into()),
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            store_dir: store.clone(),
        };
        let mut sess = Session::new(&args);
        sess.prof.data_dir = store.join("appdata");
        sess
    }

    #[test]
    fn profile_枚举仅_default_与_profile_n_并排序() {
        let base = std::env::temp_dir().join(format!(
            "ai-gateway-pdirs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        for d in ["Profile 10", "Default", "Profile 2", "System Profile", "Guest Profile"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let names: Vec<String> = profile_dirs(&base)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            names,
            vec!["Default", "Profile 10", "Profile 2"],
            "System/Guest Profile 是 Chromium 内部用途，不应纳入"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn cookies_新旧布局归一() {
        let base = std::env::temp_dir().join(format!(
            "ai-gateway-ck-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let prof = base.join("Default");
        std::fs::create_dir_all(prof.join("Network")).unwrap();
        std::fs::write(prof.join("Network").join("Cookies"), "c").unwrap();
        std::fs::write(prof.join("Network").join("Cookies-journal"), "j").unwrap();
        // 旧布局残留：不应被采用
        std::fs::write(prof.join("Cookies"), "stale").unwrap();

        let files = cookie_files(&prof);
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.parent().unwrap().ends_with("Network")));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn 完整性校验_损坏的_manifest_中止恢复() {
        let sess = session("integrity");
        let sink = QuietSink;
        let slot = sess.prof.slot_dir("123");
        let ldb = slot.join("Default").join("Local Storage").join("leveldb");
        std::fs::create_dir_all(&ldb).unwrap();
        std::fs::write(ldb.join("CURRENT"), "MANIFEST-000001\n").unwrap();

        // MANIFEST 缺失 → 中止
        assert!(test_snapshot_integrity(&sess, &slot, &sink).is_err());
        // 补齐 MANIFEST → 通过（无 Cookies 仅 warn）
        std::fs::write(ldb.join("MANIFEST-000001"), "m").unwrap();
        assert!(test_snapshot_integrity(&sess, &slot, &sink).is_ok());
        let _ = std::fs::remove_dir_all(&sess.store_dir);
    }

    #[test]
    fn 不兼容的_schema_版本中止恢复() {
        let sess = session("schema");
        let sink = QuietSink;
        let slot = sess.prof.slot_dir("123");
        std::fs::create_dir_all(&slot).unwrap();
        std::fs::write(
            slot.join(SNAPSHOT_META),
            r#"{"schemaVersion":99,"layout":"chromium"}"#,
        )
        .unwrap();
        let err = test_snapshot_integrity(&sess, &slot, &sink).unwrap_err();
        assert!(err.contains("schemaVersion"), "错误信息应说明版本不兼容: {err}");
        let _ = std::fs::remove_dir_all(&sess.store_dir);
    }

    #[test]
    fn 备份恢复往返含_meta_与活跃_profile_修复() {
        let sess = session("roundtrip");
        let sink = QuietSink;
        let ud = sess.prof.data_dir.clone();
        // 现场：Default Profile + Local State 指向快照外的 "Profile 9"
        std::fs::create_dir_all(ud.join("Default").join("Network")).unwrap();
        std::fs::write(ud.join("Default").join("Network").join("Cookies"), "ck").unwrap();
        let ldb = ud.join("Default").join("Local Storage").join("leveldb");
        std::fs::create_dir_all(&ldb).unwrap();
        std::fs::write(ldb.join("CURRENT"), "MANIFEST-1\n").unwrap();
        std::fs::write(ldb.join("MANIFEST-1"), "m").unwrap();
        std::fs::write(
            ud.join("Local State"),
            r#"{"profile":{"last_used":"Profile 9","last_active_profiles":["Profile 9","Default"]}}"#,
        )
        .unwrap();

        backup_chromium(&sess, "123", &sink).unwrap();
        let slot = sess.prof.slot_dir("123");
        let meta = snapshot_meta(&sess, "123").expect("应写入 snapshot_meta.json");
        assert_eq!(meta["schemaVersion"], 1);
        assert_eq!(meta["profileCount"], 1);
        assert_eq!(meta["layout"], "chromium");
        assert!(slot.join("Default").join("Network").join("Cookies").exists());

        // 破坏现场后恢复：Local State 的 last_used 应被改回 Default
        std::fs::remove_dir_all(&ud).unwrap();
        std::fs::create_dir_all(&ud).unwrap();
        restore_chromium(&sess, "123", &sink).unwrap();
        let ls: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(ud.join("Local State")).unwrap()).unwrap();
        assert_eq!(
            ls["profile"]["last_used"], "Default",
            "活跃 Profile 指向快照外时必须改写，否则客户端打开空 Profile 表现为未登录"
        );
        assert!(ud.join("Default").join("Network").join("Cookies").exists());
        let _ = std::fs::remove_dir_all(&sess.store_dir);
    }
}
