//! 数据目录迁移：`~/.ai-gateway` → `~/.wb-switch`。
//!
//! ## 为什么方向是反的（2026-09-16 拆分之后）
//!
//! 1.0.0 更名时迁移方向是 `~/.wb-switch` → `~/.ai-gateway`。2026-09-16 起
//! 本仓库（AI Gateway，1.x）与老仓库（workbuddy-switch-gateway，0.8.x）
//! **并行维护**，两版会同时装在同一台机器上，于是**共用同一份账号库**成了
//! 硬需求 —— 见 `config::store_dir` 的注释。
//!
//! 目录统一回 `.wb-switch`（0.8.x 一直在用、且此刻仍在被写入的那个），因此
//! 迁移方向反过来：把 `.ai-gateway` 里**已升级过的那批数据**并回 `.wb-switch`。
//!
//! ## 为什么标记文件名换了（这是关键，别合并成同一个）
//!
//! 老标记 `.migrated-from-wb-switch` 写在**目标目录**（当年的 `.ai-gateway`）里。
//! 已经跑过那次迁移的用户，其 `.ai-gateway` 里就有这个标记。若这次复用同名标记
//! 且仍写在目标目录（现在的 `.wb-switch`），逻辑会完全错乱：`.wb-switch` 里通常
//! 没有该标记（它不是当年那次迁移的目标），于是一次启动后标记被写进 `.wb-switch`
//! ——但用户的真实数据其实在 `.ai-gateway`，判定却已是「迁移完成」。
//!
//! 因此用**新的标记名** `.migrated-from-ai-gateway` 写在新的目标目录
//! （`.wb-switch`）里。它与老标记互相独立，两代迁移各自幂等，且对「已经跑过老
//! 迁移的机器」仍会正确地再执行一次反向合并。
//!
//! ## 为什么是「复制 + 并集合并」而不是「移动」
//!
//! 与初版同样的理由，且在拆分场景下更重要：用户可能仍在用 0.8.x，也可能回退。
//! 移动会掏空一侧，回退即数据丢失。并集合并（按 `(区域, uid)` 去重）保证两侧
//! 的账号汇到一处，且**不删除**来源目录里的任何东西。
//!
//! ## 幂等性
//!
//! 以目标目录里的标记文件为「已迁移」判据：① 重复启动不会反复拷贝；
//! ② 用户之后在某版里删掉的账号，不会被另一版的旧数据「复活」。
//!
//! ## 环境变量覆盖时不迁移
//!
//! 设了 `AI_GATEWAY_HOME` 说明用户在刻意隔离（开发/测试实例），
//! 此时把真实用户数据搬进去反而会污染隔离环境。

use std::path::{Path, PathBuf};

use super::config;

/// 旧数据目录名（更名后、拆分前那一代）。
const LEGACY_DIR_NAME: &str = ".ai-gateway";

/// 迁移结果摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// 本次完成了迁移，携带拷贝的条目数与并集合并进来的账号数。
    Migrated {
        from: PathBuf,
        to: PathBuf,
        entries: usize,
        accounts_merged: usize,
    },
    /// 无需迁移（旧目录不存在 / 已迁移过 / 目标被环境变量覆盖）。
    Skipped(&'static str),
}

impl MigrationOutcome {
    /// 是否发生了实际迁移。
    pub fn migrated(&self) -> bool {
        matches!(self, MigrationOutcome::Migrated { .. })
    }

    /// 人类可读描述。
    pub fn describe(&self) -> String {
        match self {
            MigrationOutcome::Migrated {
                from,
                to,
                entries,
                accounts_merged,
            } => {
                let extra = if *accounts_merged > 0 {
                    format!("，并集合并 {accounts_merged} 个账号")
                } else {
                    String::new()
                };
                format!(
                    "已从旧数据目录迁移 {entries} 个条目{extra}：{} → {}（旧目录保留，可继续用旧版）",
                    from.display(),
                    to.display()
                )
            }
            MigrationOutcome::Skipped(reason) => format!("跳过数据目录迁移：{reason}"),
        }
    }
}

/// 迁移完成标记文件名（写在目标目录内）。
///
/// **为什么需要显式标记，而不是「目标目录有数据就跳过」**：后者在实测中造成过
/// 真实的数据不可见 —— 一次截图演示模式的运行在空的新目录下建出了一份**不完整**
/// 的账号库（2 个账号），而「目标已有数据」正是跳过条件，于是旧目录里真正的
/// 8 个账号从此再也搬不过来。
///
/// 标记的另一个作用：迁移只在**第一次**启动时发生。之后用户在某一版里删除的
/// 账号，不会被另一版的数据「复活」。
///
/// **注意**：这个文件名与更名前那一代的 `.migrated-from-wb-switch` 刻意不同，
/// 原因见文件头注释 —— 两者若同名会让「已跑过老迁移的机器」被误判为已完成。
const MARKER_FILE: &str = ".migrated-from-ai-gateway";

/// 旧数据目录路径（更名后、拆分前那一代：`~/.ai-gateway`）。
pub fn legacy_store_dir() -> PathBuf {
    config::home_dir().join(LEGACY_DIR_NAME)
}

/// 是否已完成过迁移。
pub fn already_migrated(target: &Path) -> bool {
    target.join(MARKER_FILE).is_file()
}

/// 写迁移完成标记（内容记录来源与时间，便于排查）。
fn write_marker(target: &Path, from: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    config::atomic_write(
        &target.join(MARKER_FILE),
        &format!(
            "migrated_from={}\nmigrated_at={}\n",
            from.display(),
            config::utc_iso()
        ),
    )
}

/// 执行一次迁移（幂等，可重复调用）。
///
/// 方向：`~/.ai-gateway`（更名后那一代）→ `~/.wb-switch`（0.8.x 线一直在用、
/// 且拆分后两版共用）。见文件头注释。
pub fn migrate_store_dir() -> MigrationOutcome {
    // 环境变量覆盖 = 用户刻意隔离，不迁移
    if std::env::var_os("AI_GATEWAY_HOME").is_some_and(|v| !v.is_empty()) {
        return MigrationOutcome::Skipped("已通过 AI_GATEWAY_HOME 指定数据目录");
    }
    migrate_from_to(&legacy_store_dir(), &config::store_dir())
}

/// 迁移的纯函数内核（不读环境变量，因此可被单元测试直接驱动）。
pub fn migrate_from_to(legacy: &Path, target: &Path) -> MigrationOutcome {
    if !legacy.is_dir() {
        return MigrationOutcome::Skipped("未发现旧数据目录");
    }
    if legacy == target {
        return MigrationOutcome::Skipped("新旧数据目录相同");
    }
    // 已迁移过就永不再跑：否则用户在新版本里删掉的账号会被旧目录「复活」
    if already_migrated(target) {
        return MigrationOutcome::Skipped("已完成过迁移");
    }

    // 账号库先做并集合并，再拷贝其余文件。
    // 合并而非跳过：目标目录可能已有一份不完整的账号库（如演示运行留下的），
    // 直接跳过会让旧目录里的真实账号永久不可见。
    let merged = merge_accounts_file(&legacy.join("accounts.json"), &target.join("accounts.json"));

    let entries = match copy_dir_merged(legacy, target) {
        Ok(n) => n,
        // 迁移失败不该阻断启动：用户仍可手动拷贝，或重新登录账号。
        // 不写标记，下次启动会重试。
        Err(e) => {
            eprintln!("[migrate] 数据目录迁移失败（不阻断启动）: {e}");
            return MigrationOutcome::Skipped("迁移过程出错");
        }
    };

    // 标记写失败也视为迁移未完成：下次启动重试（合并是幂等的，重试安全）
    if let Err(e) = write_marker(target, legacy) {
        eprintln!("[migrate] 写入迁移标记失败（下次启动将重试）: {e}");
        return MigrationOutcome::Skipped("迁移标记写入失败");
    }

    MigrationOutcome::Migrated {
        from: legacy.to_path_buf(),
        to: target.to_path_buf(),
        entries,
        accounts_merged: merged,
    }
}

/// 把旧账号库里的账号并入新账号库（按 `uid` 去重，已有条目保持不变）。
///
/// 返回新增的账号数。任一侧读取/解析失败时**不改动目标文件**（返回 0）——
/// 账号库是唯一真源，宁可少合并也不能写坏。
fn merge_accounts_file(legacy: &Path, target: &Path) -> usize {
    let Ok(legacy_text) = std::fs::read_to_string(legacy) else {
        return 0;
    };
    let Ok(legacy_accounts) = serde_json::from_str::<Vec<serde_json::Value>>(&legacy_text) else {
        return 0;
    };
    if legacy_accounts.is_empty() {
        return 0;
    }

    // 目标不存在或为空：直接由 copy_dir_merged 拷贝，这里不重复处理
    let mut target_accounts: Vec<serde_json::Value> = match std::fs::read_to_string(target) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => return 0,
    };

    // 身份键必须与账号库自身的判重口径一致：`(区域, uid)`。
    //
    // 只用 uid 而不是「uid → id 回退」：`id` 是账号库内部的随机标识，同一个账号
    // 被应用重新采集/富化后（如补上 `needs_relogin`）会换一个 `id`。若回退到 id，
    // 同一个账号会被算成两个不同的键，合并后出现重复条目 —— 实测踩到过。
    let existing: std::collections::HashSet<(String, String)> = target_accounts
        .iter()
        .filter_map(|a| account_identity(a))
        .collect();

    let mut added = 0usize;
    for acc in legacy_accounts {
        match account_identity(&acc) {
            Some(key) if !existing.contains(&key) => {
                target_accounts.push(acc);
                added += 1;
            }
            // 无 uid 的条目无法判重：跳过，避免把同一个账号重复塞进去
            _ => {}
        }
    }
    if added == 0 {
        return 0;
    }

    let Ok(content) = serde_json::to_string_pretty(&target_accounts) else {
        return 0;
    };
    match config::atomic_write(target, &content) {
        Ok(()) => added,
        Err(e) => {
            eprintln!("[migrate] 合并账号库失败（保留原文件）: {e}");
            0
        }
    }
}

/// 账号身份键：`(区域, uid)`，与 `account::upsert_collected_account` 的判重口径一致。
///
/// 区域来自 `domain` 后缀（`.cn` → 国服，其余 → 国际版）：两个区域的身份命名空间
/// **相互独立**，同一串 uid 可以同时存在于国服与国际版，跨区域永不合并。
fn account_identity(acc: &serde_json::Value) -> Option<(String, String)> {
    let uid = acc.get("uid").and_then(|v| v.as_str())?.trim();
    if uid.is_empty() {
        return None;
    }
    let region = acc
        .get("domain")
        .and_then(|v| v.as_str())
        .map(|d| {
            if d.trim_end().ends_with(".cn") {
                "cn"
            } else {
                "intl"
            }
        })
        .unwrap_or("cn");
    Some((region.to_string(), uid.to_string()))
}

/// 递归拷贝目录内容（已存在的文件不覆盖，返回拷贝的顶层条目数）。
///
/// 不覆盖是关键：迁移中途失败后重试时，已拷贝的文件保持原样，
/// 不会用旧数据盖掉本轮已经写入的新内容。
fn copy_dir_merged(src: &Path, dest: &Path) -> std::io::Result<usize> {
    std::fs::create_dir_all(dest)?;
    let mut count = 0usize;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if to.exists() {
            continue;
        }
        if from.is_dir() {
            copy_dir_merged(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个独立的测试目录对（旧 / 新）。
    fn pair(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "ai-gateway-mig-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // 方向：legacy = 更名后那一代（.ai-gateway），target = 共用的 .wb-switch。
        let legacy = base.join(".ai-gateway");
        let target = base.join(".wb-switch");
        (base, legacy, target)
    }

    #[test]
    fn 合并拷贝不覆盖已存在文件() {
        let (base, src, dest) = pair("copy");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("a.txt"), "old-a").unwrap();
        std::fs::write(src.join("b.txt"), "b").unwrap();
        std::fs::write(src.join("nested").join("c.txt"), "c").unwrap();

        // 目标已有 a.txt：必须保留目标内容
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("a.txt"), "new-a").unwrap();

        let count = copy_dir_merged(&src, &dest).unwrap();
        assert_eq!(count, 2, "只应拷贝 b.txt 与 nested");
        assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "new-a");
        assert_eq!(std::fs::read_to_string(dest.join("b.txt")).unwrap(), "b");
        assert_eq!(
            std::fs::read_to_string(dest.join("nested").join("c.txt")).unwrap(),
            "c"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn 端到端迁移把旧目录内容搬到新目录且保留旧目录() {
        let (base, legacy, target) = pair("e2e");
        std::fs::create_dir_all(legacy.join("gateway")).unwrap();
        std::fs::write(legacy.join("accounts.json"), r#"[{"uid":"u1"}]"#).unwrap();
        std::fs::write(legacy.join("gateway").join("gateway_config.json"), "{}").unwrap();

        let outcome = migrate_from_to(&legacy, &target);
        assert!(outcome.migrated(), "应完成迁移：{outcome:?}");
        let accounts = std::fs::read_to_string(target.join("accounts.json")).unwrap();
        assert!(accounts.contains("u1"));
        assert!(target.join("gateway").join("gateway_config.json").exists());
        assert!(
            legacy.join("accounts.json").exists(),
            "旧目录必须保留，用户回退旧版时数据还在"
        );

        // 幂等：再跑一次应跳过（已写完成标记）
        assert_eq!(
            migrate_from_to(&legacy, &target),
            MigrationOutcome::Skipped("已完成过迁移")
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// **回归测试**：这是实测踩到的真实数据不可见事故。
    ///
    /// 一次演示模式的运行在空的新目录下建出了一份**不完整**的账号库（2 个账号），
    /// 而旧版本用「目标已有数据就跳过」作判据 —— 于是旧目录里真正的 8 个账号
    /// 从此再也搬不过来。现在改为「并集合并 + 显式完成标记」。
    #[test]
    fn 目标已有不完整账号库时仍能并入旧账号() {
        let (base, legacy, target) = pair("partial");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&target).unwrap();

        // 旧目录：8 个账号
        let legacy_accounts: Vec<serde_json::Value> = (1..=8)
            .map(|i| serde_json::json!({"uid": format!("u{i}"), "nickname": format!("号{i}")}))
            .collect();
        std::fs::write(
            legacy.join("accounts.json"),
            serde_json::to_string(&legacy_accounts).unwrap(),
        )
        .unwrap();

        // 新目录：演示运行留下的 2 个账号（是旧库的子集）
        std::fs::write(
            target.join("accounts.json"),
            r#"[{"uid":"u1","nickname":"号1"},{"uid":"u2","nickname":"号2"}]"#,
        )
        .unwrap();

        let outcome = migrate_from_to(&legacy, &target);
        assert!(outcome.migrated(), "目标已有数据也必须完成迁移：{outcome:?}");

        let merged: Vec<serde_json::Value> = serde_json::from_str(
            &std::fs::read_to_string(target.join("accounts.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(merged.len(), 8, "8 个旧账号必须全部可见，实际 {}", merged.len());
        for i in 1..=8 {
            assert!(
                merged.iter().any(|a| a["uid"] == format!("u{i}")),
                "账号 u{i} 丢失"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn 迁移完成后新版本里删除的账号不会被旧目录复活() {
        let (base, legacy, target) = pair("no-resurrect");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(
            legacy.join("accounts.json"),
            r#"[{"uid":"u1"},{"uid":"u2"}]"#,
        )
        .unwrap();

        assert!(migrate_from_to(&legacy, &target).migrated());

        // 用户在新版本里删掉了 u2
        std::fs::write(target.join("accounts.json"), r#"[{"uid":"u1"}]"#).unwrap();

        // 再次启动：不得把 u2 搬回来
        assert_eq!(
            migrate_from_to(&legacy, &target),
            MigrationOutcome::Skipped("已完成过迁移")
        );
        let after = std::fs::read_to_string(target.join("accounts.json")).unwrap();
        assert!(!after.contains("u2"), "已删除的账号不得被旧目录复活");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn 账号合并按_uid_去重且保留新库里的条目() {
        let (base, legacy, target) = pair("dedup");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(
            legacy.join("accounts.json"),
            r#"[{"uid":"u1","nickname":"旧名"},{"uid":"u2"}]"#,
        )
        .unwrap();
        // 新库里 u1 已被用户改过备注：不得被旧值覆盖
        std::fs::write(
            target.join("accounts.json"),
            r#"[{"uid":"u1","nickname":"新名"}]"#,
        )
        .unwrap();

        let added = merge_accounts_file(
            &legacy.join("accounts.json"),
            &target.join("accounts.json"),
        );
        assert_eq!(added, 1, "只有 u2 是新增的");

        let merged: Vec<serde_json::Value> = serde_json::from_str(
            &std::fs::read_to_string(target.join("accounts.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(merged.len(), 2);
        let u1 = merged.iter().find(|a| a["uid"] == "u1").unwrap();
        assert_eq!(u1["nickname"], "新名", "新库里的条目不得被旧值覆盖");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn 账号库损坏时不写坏目标文件() {
        let (base, legacy, target) = pair("corrupt");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(legacy.join("accounts.json"), "not json at all").unwrap();
        std::fs::write(target.join("accounts.json"), r#"[{"uid":"keep"}]"#).unwrap();

        let added = merge_accounts_file(
            &legacy.join("accounts.json"),
            &target.join("accounts.json"),
        );
        assert_eq!(added, 0);
        let text = std::fs::read_to_string(target.join("accounts.json")).unwrap();
        assert!(text.contains("keep"), "目标文件必须保持原样");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn 无_uid_的条目不入库避免重复() {
        let (base, legacy, target) = pair("nouid");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(legacy.join("accounts.json"), r#"[{"nickname":"无标识"}]"#).unwrap();
        std::fs::write(target.join("accounts.json"), r#"[{"uid":"u1"}]"#).unwrap();

        let added = merge_accounts_file(
            &legacy.join("accounts.json"),
            &target.join("accounts.json"),
        );
        assert_eq!(added, 0, "无 uid 无法判重，宁可不并入");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn 旧目录不存在时跳过() {
        let (base, _, target) = pair("absent");
        let outcome = migrate_from_to(&base.join("nope"), &target);
        assert_eq!(outcome, MigrationOutcome::Skipped("未发现旧数据目录"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn 新旧目录相同时跳过() {
        let base = std::env::temp_dir().join(format!("ai-gateway-mig-same-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        let outcome = migrate_from_to(&base, &base);
        assert_eq!(outcome, MigrationOutcome::Skipped("新旧数据目录相同"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn 完成标记的读写() {
        let (base, legacy, target) = pair("marker");
        std::fs::create_dir_all(&legacy).unwrap();
        assert!(!already_migrated(&target), "标记未写时应为假");
        write_marker(&target, &legacy).unwrap();
        assert!(already_migrated(&target));
        let text = std::fs::read_to_string(target.join(MARKER_FILE)).unwrap();
        assert!(text.contains("migrated_from="));
        assert!(text.contains("migrated_at="));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn 描述文案包含关键信息() {
        let outcome = MigrationOutcome::Migrated {
            from: PathBuf::from("C:\\old"),
            to: PathBuf::from("C:\\new"),
            entries: 5,
            accounts_merged: 3,
        };
        let text = outcome.describe();
        assert!(text.contains('5'));
        assert!(text.contains("3 个账号"));
        assert!(text.contains("C:\\old"));
        assert!(text.contains("C:\\new"));
        assert!(text.contains("旧目录保留"), "必须说明旧目录未被删除");

        let skipped = MigrationOutcome::Skipped("未发现旧数据目录");
        assert!(skipped.describe().contains("未发现旧数据目录"));
        assert!(!skipped.migrated());
    }

    #[test]
    fn 旧目录名常量为更名后那一代() {
        assert_eq!(LEGACY_DIR_NAME, ".ai-gateway");
        assert!(legacy_store_dir().ends_with(".ai-gateway"));
    }

    /// 迁移标记文件名必须与更名前那一代**不同**。
    ///
    /// 同名会让「已跑过老迁移的机器」被误判为已完成：老标记当年写在
    /// `.ai-gateway`，而现在的目标目录是 `.wb-switch`，两者不可混用。
    #[test]
    fn 迁移标记名与更名前那一代不冲突() {
        assert_ne!(MARKER_FILE, ".migrated-from-wb-switch");
        assert_eq!(MARKER_FILE, ".migrated-from-ai-gateway");
    }

    #[test]
    fn 账号身份键为_区域_加_uid() {
        // 国服
        assert_eq!(
            account_identity(&serde_json::json!({"uid":"a","domain":"www.workbuddy.cn"})),
            Some(("cn".to_string(), "a".to_string()))
        );
        // 国际版
        assert_eq!(
            account_identity(&serde_json::json!({"uid":"a","domain":"www.workbuddy.ai"})),
            Some(("intl".to_string(), "a".to_string()))
        );
        // 缺 domain：按国服（与 Region::of 的默认口径一致）
        assert_eq!(
            account_identity(&serde_json::json!({"uid":"a"})),
            Some(("cn".to_string(), "a".to_string()))
        );
        // 无 uid / 空 uid：无法判重
        assert_eq!(account_identity(&serde_json::json!({"id":"x"})), None);
        assert_eq!(account_identity(&serde_json::json!({"uid":"  "})), None);
    }

    /// **回归测试**：同一个账号被应用重新采集/富化后 `id` 会变，
    /// 若判重回退到 `id`，合并后就会出现重复条目（实测踩到过）。
    #[test]
    fn 同一账号_id_变化时仍判为同一个不重复合并() {
        let (base, legacy, target) = pair("id-drift");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&target).unwrap();

        // 旧库：原始条目，无 needs_relogin
        std::fs::write(
            legacy.join("accounts.json"),
            r#"[{"uid":"u1","id":"old-id","email":"a@b.c"}]"#,
        )
        .unwrap();
        // 新库：应用富化过同一账号（id 变了、多了 needs_relogin）
        std::fs::write(
            target.join("accounts.json"),
            r#"[{"uid":"u1","id":"new-id","email":"a@b.c","needs_relogin":true}]"#,
        )
        .unwrap();

        let added = merge_accounts_file(
            &legacy.join("accounts.json"),
            &target.join("accounts.json"),
        );
        assert_eq!(added, 0, "同一 (区域, uid) 不应被重复并入");

        let merged: Vec<serde_json::Value> = serde_json::from_str(
            &std::fs::read_to_string(target.join("accounts.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(merged.len(), 1, "不得出现重复账号，实际 {} 条", merged.len());
        // 富化后的字段必须保留（新库条目优先）
        assert_eq!(merged[0]["needs_relogin"], true);
        assert_eq!(merged[0]["id"], "new-id");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn 跨区域的同名_uid_视为不同账号() {
        let (base, legacy, target) = pair("region");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(
            legacy.join("accounts.json"),
            r#"[{"uid":"same","domain":"www.workbuddy.ai"}]"#,
        )
        .unwrap();
        std::fs::write(
            target.join("accounts.json"),
            r#"[{"uid":"same","domain":"www.workbuddy.cn"}]"#,
        )
        .unwrap();

        let added = merge_accounts_file(
            &legacy.join("accounts.json"),
            &target.join("accounts.json"),
        );
        assert_eq!(added, 1, "两区域的身份命名空间相互独立，不得互相覆盖");
        let _ = std::fs::remove_dir_all(&base);
    }
}
