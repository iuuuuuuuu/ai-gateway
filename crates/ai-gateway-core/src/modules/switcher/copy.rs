//! 快照拷贝原语：整目录/整文件替换语义 + 单代回滚 + 槽位解析。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/switcher/copy.rs`。
//!
//! 三种布局（icube / chromium / authfile）的快照管线都建立在这三个函数上。

use std::path::Path;

use super::{ProgressSink, Session, StepStatus};

/// 拷贝一个快照项（文件或目录），目标已存在时先删除再拷（**替换**语义）。
///
/// 为什么必须先删目标：源文件可能被正在运行的客户端独占（Windows 上拷贝会因共享
/// 冲突失败），而「部分成功」的拷贝比不拷贝更危险 —— 快照会带着一半旧数据被当成
/// 有效快照使用。先删后拷让失败表现为「该项缺失」，恢复侧能通过白名单对称性
/// 发现并删除现场残留。
///
/// 返回是否成功拷贝（源不存在返回 false，不视为错误）。
pub fn copy_snapshot_item(src: &Path, dest: &Path) -> bool {
    if !src.exists() {
        return false;
    }
    if src.is_dir() {
        let _ = std::fs::remove_dir_all(dest);
        if std::fs::create_dir_all(dest).is_err() {
            return false;
        }
        return copy_dir_contents(src, dest).is_ok();
    }
    let _ = std::fs::remove_file(dest);
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::copy(src, dest).is_ok()
}

/// 递归拷贝目录内容（不包含 `src` 自身这一层）。
pub fn copy_dir_contents(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            copy_dir_contents(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// 覆盖槽位前把现有快照整体挪到 `<slot>.bak`（上一代 `.bak` 直接淘汰）。
///
/// 为什么需要单代回滚：`Switch` 的「备份当前登录态到来源槽」依赖
/// `current_account.txt` 与客户端实际登录一致；一旦不一致（用户在客户端手动重登、
/// 或保存到了错误的槽），会把错误状态反复刷进该槽且**不可恢复**。
/// 有 `.bak` 后任何一次覆盖都可回退一代。
pub fn rotate_bak(dest: &Path, slot: &str, sink: &dyn ProgressSink) {
    if !dest.exists() {
        return;
    }
    // 显式拼 `{slot}.bak` 而不是 `with_extension`：槽位名可能含点（如邮箱型 uid），
    // `with_extension` 会把最后一段当成扩展名替换掉。
    let bak = dest
        .parent()
        .map(|p| p.join(format!("{slot}.bak")))
        .unwrap_or_else(|| Path::new(&format!("{slot}.bak")).to_path_buf());
    let _ = std::fs::remove_dir_all(&bak);
    match std::fs::rename(dest, &bak) {
        Ok(()) => sink.step(
            "backup",
            StepStatus::Info,
            &format!("已把上一份快照轮转为 {slot}.bak（单代回滚保护）"),
        ),
        Err(e) => sink.step(
            "backup",
            StepStatus::Warn,
            &format!("上一份快照轮转失败（继续覆盖）: {e}"),
        ),
    }
}

/// 解析要恢复的槽位：主槽位优先，缺失时回退 `<slot>.bak`。
///
/// 返回 `(实际使用的目录, 用于文案的槽位标签)`。
pub fn resolve_slot(
    sess: &Session,
    slot: &str,
    sink: &dyn ProgressSink,
) -> Result<(std::path::PathBuf, String), String> {
    let main = sess.prof.slot_dir(slot);
    if main.exists() {
        return Ok((main, slot.to_string()));
    }
    let bak = sess.prof.profiles_dir.join(format!("{slot}.bak"));
    if bak.exists() {
        sink.step(
            "restore",
            StepStatus::Warn,
            &format!("账号 {slot} 的快照缺失，改用上一代备份 {slot}.bak 恢复"),
        );
        return Ok((bak, format!("{slot}（上一代备份）")));
    }
    Err(format!(
        "账号 {slot} 没有可用快照（{} 与 {slot}.bak 均不存在），请先保存该账号的登录态",
        main.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ai-gateway-copy-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn 拷贝文件与目录并保持替换语义() {
        let base = tmp("replace");
        let src = base.join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), "new").unwrap();
        std::fs::write(src.join("sub").join("b.txt"), "nested").unwrap();

        let dest = base.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        // 目标已有旧内容：替换后不应残留
        std::fs::write(dest.join("a.txt"), "old").unwrap();

        assert!(copy_snapshot_item(&src.join("a.txt"), &dest.join("a.txt")));
        assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "new");

        assert!(copy_snapshot_item(&src.join("sub"), &dest.join("sub")));
        assert_eq!(
            std::fs::read_to_string(dest.join("sub").join("b.txt")).unwrap(),
            "nested"
        );

        // 源不存在：返回 false 且不报错
        assert!(!copy_snapshot_item(&src.join("missing"), &dest.join("x")));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn 目录拷贝为替换而非合并() {
        let base = tmp("merge");
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("new.txt"), "n").unwrap();

        let dest = base.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("old.txt"), "o").unwrap();

        copy_snapshot_item(&src, &dest);
        assert!(dest.join("new.txt").exists());
        assert!(
            !dest.join("old.txt").exists(),
            "替换语义：目录拷贝不得与既有内容合并"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rotate_bak_保留一代且槽位名含点时不被截断() {
        struct Quiet;
        impl ProgressSink for Quiet {
            fn step(&self, _: &str, _: StepStatus, _: &str) {}
        }
        let base = tmp("bak");
        // 槽位名含点：模拟邮箱型 uid
        let slot = "user.name@example.com";
        let dest = base.join(slot);
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("f.txt"), "v1").unwrap();

        rotate_bak(&dest, slot, &Quiet);

        let bak = base.join(format!("{slot}.bak"));
        assert!(bak.is_dir(), ".bak 目录应存在");
        assert_eq!(std::fs::read_to_string(bak.join("f.txt")).unwrap(), "v1");
        assert!(!dest.exists(), "原槽位应已被移走");

        // 再轮转一代：旧 .bak 淘汰，不留 .bak.bak
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("f.txt"), "v2").unwrap();
        rotate_bak(&dest, slot, &Quiet);
        assert_eq!(std::fs::read_to_string(bak.join("f.txt")).unwrap(), "v2");
        assert!(!base.join(format!("{slot}.bak.bak")).exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
