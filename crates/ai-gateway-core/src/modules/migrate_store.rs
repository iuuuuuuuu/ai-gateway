//! 数据目录迁移：`~/.wb-switch` → `~/.ai-gateway`。
//!
//! ## 为什么是「复制」而不是「移动」
//!
//! 更名后旧版本可能仍在用户的另一台机器/另一个目录里使用，甚至用户会回退到旧版。
//! 移动会把旧版的数据直接掏空，回退即数据丢失。复制迁移下新旧两版可并存，
//! 用户确认新版无误后再自行删除旧目录。
//!
//! ## 幂等性
//!
//! 以「目标目录里已存在账号库或网关配置」作为「已迁移」的判据：这样
//! ① 重复启动不会反复拷贝；② 用户已经在新版里新建过数据时不会被旧数据覆盖。
//!
//! ## 环境变量覆盖时不迁移
//!
//! 设了 `AI_GATEWAY_HOME` 说明用户在刻意隔离（开发/测试实例），
//! 此时把真实用户数据搬进去反而会污染隔离环境。

use std::path::{Path, PathBuf};

use super::config;

/// 旧数据目录名（更名前）。
const LEGACY_DIR_NAME: &str = ".wb-switch";

/// 迁移结果摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// 本次完成了迁移，携带拷贝的条目数。
    Migrated { from: PathBuf, to: PathBuf, entries: usize },
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
            MigrationOutcome::Migrated { from, to, entries } => format!(
                "已从旧数据目录迁移 {entries} 个条目：{} → {}（旧目录保留，可继续用旧版）",
                from.display(),
                to.display()
            ),
            MigrationOutcome::Skipped(reason) => format!("跳过数据目录迁移：{reason}"),
        }
    }
}

/// 旧数据目录路径。
pub fn legacy_store_dir() -> PathBuf {
    config::home_dir().join(LEGACY_DIR_NAME)
}

/// 目标目录是否已有数据（有则视为已迁移 / 用户已在用新版）。
///
/// 只探测**标志性文件**而不是「目录非空」：新版启动时会自己创建 `logs/`、
/// `gateway/` 等空目录，用「非空」判断会让迁移在首次启动后永远跳过。
fn target_has_data(dir: &Path) -> bool {
    const MARKERS: [&str; 6] = [
        "accounts.json",
        "gateway",
        "auto_checkin_config.json",
        "app_settings.json",
        "trae_accounts.json",
        "doubao_accounts.json",
    ];
    MARKERS.iter().any(|marker| dir.join(marker).exists())
}

/// 执行一次迁移（幂等，可重复调用）。
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
    if target_has_data(target) {
        return MigrationOutcome::Skipped("新数据目录已有数据");
    }

    match copy_dir_merged(legacy, target) {
        Ok(entries) => MigrationOutcome::Migrated {
            from: legacy.to_path_buf(),
            to: target.to_path_buf(),
            entries,
        },
        // 迁移失败不该阻断启动：用户仍可手动拷贝，或重新登录账号
        Err(e) => {
            eprintln!("[migrate] 数据目录迁移失败（不阻断启动）: {e}");
            MigrationOutcome::Skipped("迁移过程出错")
        }
    }
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

    #[test]
    fn 目标已有数据时判定为已迁移() {
        let dir = std::env::temp_dir().join(format!(
            "ai-gateway-mig-target-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!target_has_data(&dir), "空目录不算已有数据");

        // 只有空子目录（新版启动自建的 logs/）也不算 —— 否则迁移会被永久跳过
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        assert!(
            !target_has_data(&dir),
            "仅存在自建空目录时不得判定为已迁移，否则真实数据永远搬不过来"
        );

        // 有标志性文件才算
        std::fs::write(dir.join("accounts.json"), "[]").unwrap();
        assert!(target_has_data(&dir));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 合并拷贝不覆盖已存在文件() {
        let base = std::env::temp_dir().join(format!(
            "ai-gateway-mig-copy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let src = base.join("src");
        let dest = base.join("dest");
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
        let base = std::env::temp_dir().join(format!(
            "ai-gateway-mig-e2e-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let legacy = base.join(".wb-switch");
        let target = base.join(".ai-gateway");
        std::fs::create_dir_all(legacy.join("gateway")).unwrap();
        std::fs::write(legacy.join("accounts.json"), r#"[{"uid":"u1"}]"#).unwrap();
        std::fs::write(legacy.join("gateway").join("gateway_config.json"), "{}").unwrap();

        let outcome = migrate_from_to(&legacy, &target);
        assert!(outcome.migrated(), "应完成迁移：{outcome:?}");
        assert_eq!(
            std::fs::read_to_string(target.join("accounts.json")).unwrap(),
            r#"[{"uid":"u1"}]"#
        );
        assert!(target.join("gateway").join("gateway_config.json").exists());
        assert!(
            legacy.join("accounts.json").exists(),
            "旧目录必须保留，用户回退旧版时数据还在"
        );

        // 幂等：再跑一次应跳过（目标已有数据）
        let again = migrate_from_to(&legacy, &target);
        assert_eq!(again, MigrationOutcome::Skipped("新数据目录已有数据"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn 旧目录不存在时跳过() {
        let base = std::env::temp_dir().join(format!(
            "ai-gateway-mig-absent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let outcome = migrate_from_to(&base.join("nope"), &base.join("target"));
        assert_eq!(outcome, MigrationOutcome::Skipped("未发现旧数据目录"));
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
    fn 描述文案包含关键信息() {
        let outcome = MigrationOutcome::Migrated {
            from: PathBuf::from("C:\\old"),
            to: PathBuf::from("C:\\new"),
            entries: 5,
        };
        let text = outcome.describe();
        assert!(text.contains("5"));
        assert!(text.contains("C:\\old"));
        assert!(text.contains("C:\\new"));
        assert!(text.contains("旧目录保留"), "必须说明旧目录未被删除");

        let skipped = MigrationOutcome::Skipped("未发现旧数据目录");
        assert!(skipped.describe().contains("未发现旧数据目录"));
        assert!(!skipped.migrated());
    }

    #[test]
    fn 旧目录名常量与更名前一致() {
        assert_eq!(LEGACY_DIR_NAME, ".wb-switch");
        assert!(legacy_store_dir().ends_with(".wb-switch"));
    }
}
