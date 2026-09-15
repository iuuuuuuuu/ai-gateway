//! 6 层设备标识重置（icube 布局：Trae Work / Trae）。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/switcher/machine.rs`。
//!
//! ## 为什么需要「多账号」就必须重置设备标识
//!
//! Trae 系客户端把设备指纹散落在 6 个位置，服务端据此判定「这些账号来自同一台机器」。
//! 多账号共用一份设备标识会触发风控关联，因此切换账号前需要把机器级指纹换成新的。
//!
//! ## 与「账号级设备指纹」的区别（重要）
//!
//! 本模块重置的是**机器级**指纹。另有一套**账号级**指纹派生
//! （见 [`crate::modules::trae_device`]）：由 uid 确定性派生 `device_id` /
//! `session_id` / `market_user_id`，随每个账号走、由请求头与 MITM 代理注入。
//! 两者目的不同，不可互相替代。
//!
//! ## 6 层
//!
//! | # | 位置 | 动作 |
//! |---|---|---|
//! | 1 | `<data>\machineid` 文件 | 写入新的 hex32 |
//! | 2 | `storage.json` → `telemetry.machineId` / `telemetry.sqmId` | 改写 |
//! | 3 | `storage.json` → `aha.device.device_id` | 改写，并删除 `has_device_id_updated_to_aha` 标记 |
//! | 4 | `<data>\aha\TinyStorage\**` | 删除内容含 `device_id` 的文件 |
//! | 5 | 注册表 `HKLM\...\Cryptography\MachineGuid` | 改写（需管理员，失败仅跳过） |
//! | 6 | `<data>\Partitions\trae-webview\{Network,Local Storage,Session Storage}` | 删除 |
//!
//! 达成 ≥4 层即视为成功（第 5 层需管理员权限，普通用户常拿不到）。

use std::path::Path;

use super::{ProgressSink, Session, StepStatus};
use crate::modules::app_profile::Layout;

/// 生成 32 位小写十六进制随机串（用作 machineid）。
fn random_hex32() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// 生成 n 位十进制随机数字。
fn random_digits(n: usize) -> String {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let mut out = String::with_capacity(n);
    for i in 0..n {
        out.push(char::from(b'0' + (bytes[i % bytes.len()] % 10)));
    }
    out
}

/// 生成标准 UUID v4 字符串（用作 sqmId / MachineGuid）。
fn new_sqm_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 重置全部 6 层设备标识。
pub fn reset_device_ids_only(sess: &Session, sink: &dyn ProgressSink) -> Result<String, String> {
    let prof = &sess.prof;
    match prof.layout {
        Layout::Icube => {}
        Layout::Chromium => {
            sink.step(
                "device-reset",
                StepStatus::Skip,
                "豆包（chromium 布局）不使用这套机器指纹，已跳过",
            );
            return Ok("豆包不需要重置设备标识".to_string());
        }
        Layout::Authfile => {
            return Err(format!(
                "{} 不支持设备标识重置（该功能仅适用于 Trae 系客户端）",
                prof.app_name
            ));
        }
    }

    let data_dir = &prof.data_dir;
    if !data_dir.exists() {
        sink.step(
            "device-reset",
            StepStatus::Error,
            &format!("{} 数据目录不存在：{}", prof.app_name, data_dir.display()),
        );
        return Ok(format!("{} 数据目录不存在，未做任何修改", prof.app_name));
    }

    let mut reset_count = 0usize;

    // ── 第 1 层：machineid 文件 ─────────────────────────────────────────────
    let machineid = data_dir.join("machineid");
    let new_machineid = random_hex32();
    match std::fs::write(&machineid, &new_machineid) {
        Ok(()) => {
            reset_count += 1;
            sink.step(
                "device-reset",
                StepStatus::Ok,
                &format!("[1/6] machineid 已重置为 {new_machineid}"),
            );
        }
        Err(e) => sink.step(
            "device-reset",
            StepStatus::Skip,
            &format!("[1/6] machineid 写入失败，跳过: {e}"),
        ),
    }

    // ── 第 2、3 层：storage.json 的 telemetry 与 aha 设备 ──────────────────
    let storage = data_dir.join("User").join("globalStorage").join("storage.json");
    match edit_storage_device_ids(&storage, &random_hex32(), &new_sqm_id(), &random_digits(15)) {
        Ok(true) => {
            reset_count += 2;
            sink.step(
                "device-reset",
                StepStatus::Ok,
                "[2/6] storage.json telemetry.machineId / sqmId 已重置",
            );
            sink.step(
                "device-reset",
                StepStatus::Ok,
                "[3/6] storage.json aha.device.device_id 已重置（并清除 aha 更新标记）",
            );
        }
        Ok(false) => sink.step(
            "device-reset",
            StepStatus::Skip,
            "[2-3/6] storage.json 不存在，跳过",
        ),
        Err(e) => sink.step(
            "device-reset",
            StepStatus::Skip,
            &format!("[2-3/6] storage.json 改写失败，跳过: {e}"),
        ),
    }

    // ── 第 4 层：aha\TinyStorage 内容含 device_id 的文件 ────────────────────
    let removed = clear_tiny_storage(&data_dir.join("aha").join("TinyStorage"));
    if removed > 0 {
        reset_count += 1;
        sink.step(
            "device-reset",
            StepStatus::Ok,
            &format!("[4/6] aha\\TinyStorage 已清除 {removed} 个含设备标识的文件"),
        );
    } else {
        sink.step(
            "device-reset",
            StepStatus::Skip,
            "[4/6] aha\\TinyStorage 无待清除文件",
        );
    }

    // ── 第 5 层：注册表 MachineGuid（需管理员）─────────────────────────────
    if reset_machine_guid(&new_sqm_id()) {
        reset_count += 1;
        sink.step(
            "device-reset",
            StepStatus::Ok,
            "[5/6] 注册表 MachineGuid 已重置",
        );
    } else {
        sink.step(
            "device-reset",
            StepStatus::Skip,
            "[5/6] 注册表 MachineGuid 重置失败（需以管理员身份运行），已跳过",
        );
    }

    // ── 第 6 层：trae-webview 追踪数据 ─────────────────────────────────────
    let webview = data_dir.join("Partitions").join("trae-webview");
    let mut removed_any = false;
    for sub in ["Network", "Local Storage", "Session Storage"] {
        let target = webview.join(sub);
        if target.exists() && std::fs::remove_dir_all(&target).is_ok() {
            removed_any = true;
        }
    }
    if removed_any {
        reset_count += 1;
        sink.step(
            "device-reset",
            StepStatus::Ok,
            "[6/6] trae-webview 追踪数据已清除",
        );
    } else {
        sink.step(
            "device-reset",
            StepStatus::Skip,
            "[6/6] trae-webview 无待清除数据",
        );
    }

    // 阈值 4：第 5 层常因权限失败，要求全部 6 层会让功能在普通用户下永远「失败」
    let status = if reset_count >= 4 {
        StepStatus::Ok
    } else {
        StepStatus::Info
    };
    let message = format!("设备标识重置完成：{reset_count}/6 层已生效");
    sink.step("device-reset", status, &message);
    Ok(message)
}

/// 只重置注册表 MachineGuid（对应上游的 `ResetMachineId` 动作）。
pub fn reset_machine_id(sink: &dyn ProgressSink) -> Result<String, String> {
    let guid = new_sqm_id();
    if reset_machine_guid(&guid) {
        sink.step("device-reset", StepStatus::Ok, "注册表 MachineGuid 已重置");
        Ok(guid)
    } else {
        Err("注册表 MachineGuid 重置失败（需以管理员身份运行）".to_string())
    }
}

/// 改写 `storage.json` 里的设备标识。
///
/// 注意这些是**扁平的带点键名**（`"telemetry.machineId"`），不是嵌套对象 ——
/// 按嵌套结构去改会静默写进一个客户端不读的位置。
///
/// 同时删除 `has_device_id_updated_to_aha` 标记，让客户端重新走一遍 aha 设备注册。
///
/// 返回 `Ok(false)` 表示文件不存在。
fn edit_storage_device_ids(
    path: &Path,
    machine_id: &str,
    sqm_id: &str,
    aha_device_id: &str,
) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    // BOM 容错：上游 PowerShell 版本曾写入 BOM，直接 serde 解析会失败。
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut j: serde_json::Value =
        serde_json::from_str(raw.trim_start_matches('\u{feff}')).map_err(|e| e.to_string())?;
    let Some(obj) = j.as_object_mut() else {
        return Err("storage.json 顶层不是对象".to_string());
    };
    obj.insert(
        "telemetry.machineId".to_string(),
        serde_json::json!(machine_id),
    );
    obj.insert("telemetry.sqmId".to_string(), serde_json::json!(sqm_id));
    obj.insert(
        "aha.device.device_id".to_string(),
        serde_json::json!(aha_device_id),
    );
    obj.remove("has_device_id_updated_to_aha");
    std::fs::write(
        path,
        serde_json::to_string_pretty(&j).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(true)
}

/// 递归删除 `aha\TinyStorage` 下内容含 `device_id` 字样的文件。
///
/// 按**原始字节**做窗口匹配而不是文本匹配：这些文件是二进制存储，
/// 按 UTF-8 解码可能失败或错位，字节级 `windows(9)` 才是可靠判据。
fn clear_tiny_storage(root: &Path) -> usize {
    let mut removed = 0usize;
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            removed += clear_tiny_storage(&path);
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if contains_window(&bytes, b"device_id") && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 字节序列里是否包含给定窗口。
fn contains_window(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// 改写注册表 `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid`。
///
/// 需要管理员权限；失败返回 `false`（调用方按「跳过」处理，不阻断整体流程）。
fn reset_machine_guid(guid: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        use windows_registry::LOCAL_MACHINE;
        return LOCAL_MACHINE
            .open(r"SOFTWARE\Microsoft\Cryptography")
            .and_then(|key| key.set_string("MachineGuid", guid))
            .is_ok();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = guid;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 随机串形态正确() {
        let hex = random_hex32();
        assert_eq!(hex.len(), 32);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));

        let digits = random_digits(15);
        assert_eq!(digits.len(), 15);
        assert!(digits.chars().all(|c| c.is_ascii_digit()));

        let guid = new_sqm_id();
        assert_eq!(guid.len(), 36);
        assert_eq!(guid.chars().filter(|c| *c == '-').count(), 4);
    }

    #[test]
    fn storage_改写的是扁平点号键而非嵌套对象() {
        let dir = std::env::temp_dir().join(format!(
            "ai-gateway-machine-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("storage.json");
        std::fs::write(
            &path,
            r#"{"telemetry.machineId":"old","telemetry.sqmId":"old-sqm","aha.device.device_id":"old-dev","has_device_id_updated_to_aha":true,"keep":"me"}"#,
        )
        .unwrap();

        assert!(edit_storage_device_ids(&path, "NEWHEX", "NEWSQM", "NEWDEV").unwrap());
        let j: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

        assert_eq!(j["telemetry.machineId"], "NEWHEX");
        assert_eq!(j["telemetry.sqmId"], "NEWSQM");
        assert_eq!(j["aha.device.device_id"], "NEWDEV");
        assert!(
            j.get("has_device_id_updated_to_aha").is_none(),
            "必须删除 aha 更新标记，否则客户端不会重新注册设备"
        );
        assert_eq!(j["keep"], "me", "无关字段必须原样保留");
        // 不得写成嵌套对象
        assert!(j.get("telemetry").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn storage_文件缺失时返回_false_而不报错() {
        let missing = std::env::temp_dir().join("ai-gateway-machine-missing.json");
        let _ = std::fs::remove_file(&missing);
        assert!(!edit_storage_device_ids(&missing, "a", "b", "c").unwrap());
    }

    #[test]
    fn storage_带_bom_也能解析() {
        let dir = std::env::temp_dir().join(format!(
            "ai-gateway-machine-bom-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("storage.json");
        std::fs::write(&path, "\u{feff}{\"telemetry.machineId\":\"old\"}").unwrap();
        assert!(
            edit_storage_device_ids(&path, "N", "S", "D").unwrap(),
            "上游 PS 版本写入过 BOM，必须容错否则重置静默失效"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tiny_storage_按字节窗口清除() {
        let dir = std::env::temp_dir().join(format!(
            "ai-gateway-tiny-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let nested = dir.join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        // 含 device_id 的二进制文件（非合法 UTF-8）
        std::fs::write(dir.join("a.bin"), b"\xff\xfe\x00device_id\x00\xff").unwrap();
        // 不含的：应保留
        std::fs::write(dir.join("keep.bin"), b"nothing here").unwrap();
        // 嵌套目录内的也应被清除
        std::fs::write(nested.join("b.bin"), b"xx device_id yy").unwrap();

        let removed = clear_tiny_storage(&dir);
        assert_eq!(removed, 2);
        assert!(dir.join("keep.bin").exists());
        assert!(!dir.join("a.bin").exists());
        assert!(!nested.join("b.bin").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 字节窗口匹配() {
        assert!(contains_window(b"abcdef", b"cde"));
        assert!(contains_window(b"device_id", b"device_id"));
        assert!(!contains_window(b"abc", b"abcd"));
        assert!(!contains_window(b"", b"a"));
        assert!(!contains_window(b"abc", b""));
    }

    #[test]
    fn authfile_布局明确拒绝设备重置() {
        use crate::modules::app_profile::TargetApp;
        use crate::modules::switcher::{Action, RunArgs};
        let store = std::env::temp_dir().join(format!("ai-gateway-mr-{}", std::process::id()));
        let args = RunArgs {
            action: Action::ResetDeviceIds,
            target_app: TargetApp::WorkBuddy,
            user_id: None,
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            store_dir: store,
        };
        let err = super::super::run_action(args, &super::super::NullSink).unwrap_err();
        assert!(err.contains("不支持设备标识重置"), "应明确拒绝: {err}");
    }
}
