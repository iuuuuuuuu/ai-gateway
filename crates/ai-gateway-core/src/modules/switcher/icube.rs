//! icube 布局快照（Trae Work / Trae）：精准白名单备份/恢复 + `.bak` 单代回滚。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/switcher/icube.rs`。
//!
//! 两个应用同为 icube 内核的 VS Code fork，登录态文件结构完全同构，
//! 因此按 [`AppProfile`] 参数化复用全部逻辑。

use super::copy;
use super::{ProgressSink, Session, StepStatus};

/// 精准白名单（备份与恢复对称）。
///
/// 覆盖 README 所述「9 类核心文件」，实际为 15 个物理路径 —— 每类可能含多个文件，
/// 且 SQLite 的 WAL/SHM 边车必须随主库一起快照。
enum Item {
    File(&'static str),
    Dir(&'static str),
}

impl Item {
    fn rel(&self) -> &'static str {
        match self {
            Item::File(r) | Item::Dir(r) => r,
        }
    }
}

const ICUBE_ITEMS: &[Item] = &[
    // 1. 设备标识 / 遥测 / 认证信息
    Item::File("User\\globalStorage\\storage.json"),
    // 2. 登录令牌数据库
    Item::File("User\\globalStorage\\state.vscdb"),
    // 2b. WAL/SHM 边车随主库快照：优雅关闭超时强杀是常态（实测 TRAE 每次切换都强杀），
    // 最新登录写入可能尚未 checkpoint 进主库——漏拷会丢数据，且恢复侧依赖边车与
    // 主库成对回放。
    Item::File("User\\globalStorage\\state.vscdb-wal"),
    Item::File("User\\globalStorage\\state.vscdb-shm"),
    Item::File("User\\globalStorage\\state.vscdb.backup"),
    // 3. 机器标识
    Item::File("machineid"),
    // 4. 设备认证数据
    Item::Dir("aha"),
    // 5.
    Item::File("Preferences"),
    Item::File("Local State"),
    // 6. web 侧登录/偏好 KV
    Item::Dir("Local Storage\\leveldb"),
    Item::File("Local Storage\\config.db"),
    // 7. Cookie
    Item::Dir("Network"),
    // 8.
    Item::Dir("Partitions\\trae-webview"),
    Item::Dir("Partitions\\icube-web-crawler-shared-session-v1.0"),
    // 9.
    Item::Dir("Session Storage"),
];

/// 精准备份：仅复制登录态关键文件。
pub fn backup_icube(sess: &Session, slot: &str, sink: &dyn ProgressSink) -> Result<(), String> {
    let src = &sess.prof.data_dir;
    if !src.exists() {
        sink.step(
            "backup",
            StepStatus::Skip,
            "当前数据目录不存在，跳过备份",
        );
        return Ok(());
    }
    let dest = sess.prof.slot_dir(slot);
    // 覆盖槽位前把旧快照挪到 .bak（单代回滚保护），防止拷贝中断（断电/杀进程）
    // 永久丢失上一份快照。
    copy::rotate_bak(&dest, slot, sink);
    std::fs::create_dir_all(&dest).map_err(|e| format!("创建快照目录失败: {e}"))?;

    let mut copied = 0usize;
    for item in ICUBE_ITEMS {
        if copy::copy_snapshot_item(
            &super::join_windows_rel(src, item.rel()),
            &super::join_windows_rel(&dest, item.rel()),
        ) {
            copied += 1;
        }
    }
    sink.step(
        "backup",
        StepStatus::Ok,
        &format!("已备份当前登录态到 {slot} ({copied} 项)"),
    );
    Ok(())
}

/// 精准恢复：仅恢复登录态关键文件（与备份对称）。
///
/// 恢复项计数写入 `Session.last_restored_count`，供 [`verify_after_restore`] 校验。
pub fn restore_icube(sess: &mut Session, slot: &str, sink: &dyn ProgressSink) -> Result<(), String> {
    let (src, slot_label) = copy::resolve_slot(sess, slot, sink)?;
    let dest = sess.prof.data_dir.clone();
    std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;

    // 删除 code.lock 防止启动冲突
    let _ = std::fs::remove_file(dest.join("code.lock"));

    // 强杀后现场残留 state.vscdb-wal/-shm，SQLite WAL 模式下客户端启动打开恢复后的
    // 主库会把旧 WAL 回放，把切换前账号的登录/使用证据写回新库。
    // 这是「切换后账号不变 / 本机识别错乱」的根因，恢复前必须先删边车。
    let gs_dir = dest.join("User").join("globalStorage");
    for stale in ["state.vscdb-wal", "state.vscdb-shm"] {
        let _ = std::fs::remove_file(gs_dir.join(stale));
    }

    // 对称恢复：槽位有的项覆盖，槽位没有的项**删除**现场残留 —— 恢复后 Live 恒等于
    // 槽位内容，不携带上一账号的残留（如槽位缺 state.vscdb.backup 而现场有旧账号的）。
    let mut restored = 0usize;
    for item in ICUBE_ITEMS {
        let src_item = super::join_windows_rel(&src, item.rel());
        let dst_item = super::join_windows_rel(&dest, item.rel());
        if src_item.exists() {
            if copy::copy_snapshot_item(&src_item, &dst_item) {
                restored += 1;
            }
        } else if dst_item.is_dir() {
            let _ = std::fs::remove_dir_all(&dst_item);
        } else if dst_item.exists() {
            let _ = std::fs::remove_file(&dst_item);
        }
    }
    sess.last_restored_count = restored as i64;
    sink.step(
        "restore",
        StepStatus::Ok,
        &format!("已恢复账号 {slot_label} 的登录态 ({restored} 项)"),
    );
    Ok(())
}

/// 恢复后校验：返回 `Some(原因)` 表示恢复失败，调用方应回滚到 `last` 槽位。
///
/// 三种缺失来源：
/// 1. 一项都没恢复（快照目录空 / 全部源文件缺失）
/// 2. `storage.json` 缺失（设备标识与认证信息不在）
/// 3. `state.vscdb` 缺失（登录令牌库不在）
///
/// 不做这层校验的后果：切换「成功」返回，但客户端起来后仍是旧账号或未登录，
/// 用户只能看到「切换没生效」而没有任何可排查的线索。
pub fn verify_after_restore(sess: &Session, slot: &str) -> Option<String> {
    if sess.last_restored_count <= 0 {
        return Some(format!("账号 {slot} 的快照为空，未能恢复任何登录态文件"));
    }
    let gs = sess.prof.data_dir.join("User").join("globalStorage");
    if !gs.join("storage.json").exists() {
        return Some("恢复后缺少 storage.json（设备标识与认证信息）".to_string());
    }
    if !gs.join("state.vscdb").exists() {
        return Some("恢复后缺少 state.vscdb（登录令牌数据库）".to_string());
    }
    None
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

    /// 构造一个把「应用数据目录」指向临时目录的会话（绝不触碰真实 %APPDATA%）。
    fn session(tag: &str) -> Session {
        let store = std::env::temp_dir().join(format!(
            "ai-gateway-icube-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&store);
        let args = RunArgs {
            action: Action::Switch,
            target_app: TargetApp::TraeWork,
            user_id: Some("2117003799429594".into()),
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            store_dir: store.clone(),
        };
        let mut sess = Session::new(&args);
        sess.prof.data_dir = store.join("appdata");
        sess
    }

    fn seed_live(sess: &Session) {
        let dd = &sess.prof.data_dir;
        let gs = dd.join("User").join("globalStorage");
        std::fs::create_dir_all(&gs).unwrap();
        std::fs::write(gs.join("storage.json"), "{}").unwrap();
        std::fs::write(gs.join("state.vscdb"), "db").unwrap();
        std::fs::write(dd.join("machineid"), "M").unwrap();
        std::fs::create_dir_all(dd.join("aha")).unwrap();
        std::fs::write(dd.join("aha").join("t"), "a").unwrap();
        std::fs::create_dir_all(dd.join("Network")).unwrap();
        std::fs::write(dd.join("Network").join("Cookies"), "c").unwrap();
    }

    #[test]
    fn 备份恢复往返且白名单外文件不入快照() {
        let mut sess = session("roundtrip");
        let sink = QuietSink;
        seed_live(&sess);
        // 白名单外文件：不应进快照
        std::fs::write(sess.prof.data_dir.join("not-whitelisted.txt"), "x").unwrap();

        backup_icube(&sess, "2117003799429594", &sink).unwrap();
        let slot = sess.prof.slot_dir("2117003799429594");
        assert!(slot.join("User").join("globalStorage").join("storage.json").exists());
        assert!(slot.join("User").join("globalStorage").join("state.vscdb").exists());
        assert!(slot.join("machineid").exists());
        assert!(slot.join("aha").join("t").exists());
        assert!(slot.join("Network").join("Cookies").exists());
        assert!(
            !slot.join("not-whitelisted.txt").exists(),
            "白名单外文件不得进入快照"
        );

        // 破坏现场后恢复：应还原
        std::fs::remove_file(sess.prof.data_dir.join("machineid")).unwrap();
        std::fs::remove_dir_all(sess.prof.data_dir.join("aha")).unwrap();
        restore_icube(&mut sess, "2117003799429594", &sink).unwrap();
        assert!(sess.prof.data_dir.join("machineid").exists());
        assert!(sess.prof.data_dir.join("aha").join("t").exists());
        assert!(sess.last_restored_count > 0);
        assert!(verify_after_restore(&sess, "2117003799429594").is_none());

        let _ = std::fs::remove_dir_all(&sess.store_dir);
    }

    #[test]
    fn 恢复是对称的槽位缺失项会删除现场残留() {
        let mut sess = session("symmetric");
        let sink = QuietSink;
        seed_live(&sess);
        backup_icube(&sess, "u1", &sink).unwrap();

        // 现场多出一个「快照里没有」的残留文件（模拟上一账号遗留）
        let stale = sess
            .prof
            .data_dir
            .join("User")
            .join("globalStorage")
            .join("state.vscdb.backup");
        std::fs::write(&stale, "stale").unwrap();
        assert!(stale.exists());

        restore_icube(&mut sess, "u1", &sink).unwrap();
        assert!(
            !stale.exists(),
            "对称恢复：快照内没有的项必须从现场删除，否则会带着上一账号的残留"
        );
        let _ = std::fs::remove_dir_all(&sess.store_dir);
    }

    #[test]
    fn 恢复前删除_wal_边车防止旧账号回放() {
        let mut sess = session("wal");
        let sink = QuietSink;
        seed_live(&sess);
        backup_icube(&sess, "u1", &sink).unwrap();

        // 模拟强杀后的现场残留 WAL/SHM
        let gs = sess.prof.data_dir.join("User").join("globalStorage");
        std::fs::write(gs.join("state.vscdb-wal"), "old-account-wal").unwrap();
        std::fs::write(gs.join("state.vscdb-shm"), "old-account-shm").unwrap();

        restore_icube(&mut sess, "u1", &sink).unwrap();
        // 快照里没有 WAL（备份时现场也没有），恢复后不应存在
        assert!(
            !gs.join("state.vscdb-wal").exists(),
            "恢复前必须删除 WAL，否则 SQLite 会回放切换前账号的写入"
        );
        let _ = std::fs::remove_dir_all(&sess.store_dir);
    }

    #[test]
    fn 空快照的恢复校验失败() {
        let mut sess = session("empty");
        let sink = QuietSink;
        std::fs::create_dir_all(sess.prof.slot_dir("u1")).unwrap();
        restore_icube(&mut sess, "u1", &sink).unwrap();
        assert_eq!(sess.last_restored_count, 0);
        assert!(
            verify_after_restore(&sess, "u1").is_some(),
            "一项都没恢复时必须报失败，否则用户看到「切换成功」但实际什么都没变"
        );
        let _ = std::fs::remove_dir_all(&sess.store_dir);
    }
}
