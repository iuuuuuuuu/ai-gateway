//! 应用档案表（表驱动）：5 应用 × 3 快照布局。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/switcher/profile.rs`，按本仓库的
//! 「core 不依赖 Tauri」约定改写为纯数据表 + 纯函数。
//!
//! 三种快照布局：
//! - [`Layout::Icube`]：Trae Work / Trae（同为 icube 内核的 VSCode fork，登录态
//!   文件结构完全同构，按档案参数化复用同一套切换逻辑）
//! - [`Layout::Chromium`]：豆包（Chromium 壳，多 Profile，快照带版本校验）
//! - [`Layout::Authfile`]：WorkBuddy / CodeBuddy（共享 auth 文件 + 自身 vscdb）
//!
//! 为什么需要这张表：切换流程（备份 → 关进程 → 恢复 → 启动）对 5 个应用是**同一套**
//! 逻辑，差异只在数据目录、进程名、exe 候选与快照布局。把这些差异收敛到一张表，
//! 新增应用就只是加一行，而不是复制一遍流程。

use std::path::{Path, PathBuf};

/// 快照布局。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    Icube,
    Chromium,
    Authfile,
}

impl Layout {
    /// 字符串形态（用于日志/错误消息与前端展示）。
    pub fn as_str(self) -> &'static str {
        match self {
            Layout::Icube => "icube",
            Layout::Chromium => "chromium",
            Layout::Authfile => "authfile",
        }
    }
}

/// 目标应用。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TargetApp {
    TraeWork,
    Trae,
    Doubao,
    WorkBuddy,
    CodeBuddy,
}

impl TargetApp {
    /// 解析前端传入的应用标识。
    ///
    /// 未知值回退 [`TargetApp::TraeWork`]：调用方是自家前端，恒传合法值，
    /// 回退比硬失败更稳（与上游 `TargetApp::parse` 语义一致）。
    pub fn parse(s: &str) -> TargetApp {
        match s.trim() {
            "Trae" | "trae" => TargetApp::Trae,
            "Doubao" | "doubao" | "豆包" => TargetApp::Doubao,
            "WorkBuddy" | "workbuddy" => TargetApp::WorkBuddy,
            "CodeBuddy" | "codebuddy" => TargetApp::CodeBuddy,
            _ => TargetApp::TraeWork,
        }
    }

    /// 规范字符串（与 [`TargetApp::parse`] 互逆）。
    pub fn as_str(self) -> &'static str {
        match self {
            TargetApp::TraeWork => "TraeWork",
            TargetApp::Trae => "Trae",
            TargetApp::Doubao => "Doubao",
            TargetApp::WorkBuddy => "WorkBuddy",
            TargetApp::CodeBuddy => "CodeBuddy",
        }
    }

    /// 全部应用（界面遍历顺序：Trae 系 → 豆包 → Buddy 系）。
    pub const ALL: [TargetApp; 5] = [
        TargetApp::TraeWork,
        TargetApp::Trae,
        TargetApp::Doubao,
        TargetApp::WorkBuddy,
        TargetApp::CodeBuddy,
    ];
}

/// 单个应用的档案。
pub struct AppProfile {
    /// 应用显示名（进入全部进度文案）。
    pub app_name: &'static str,
    pub layout: Layout,
    /// 应用真实数据目录（`%APPDATA%\...` / `%LOCALAPPDATA%\...` / `~\.xxx`）。
    pub data_dir: PathBuf,
    /// 快照槽根目录（本软件数据目录下）。
    pub profiles_dir: PathBuf,
    /// 应用设置里「手动指定 exe 路径」的键名。
    pub settings_path_key: &'static str,
    /// 优雅关闭等待秒数（豆包 8 / Trae 系 8 / WB+CB 5）。
    ///
    /// 为什么是 8 而不是 3：chromium 壳与 VSCode fork 退出前要落盘 leveldb / cookie /
    /// SQLite WAL，3 秒实测经常不够 —— 强杀会留下文件锁，导致备份静默缺文件、
    /// 恢复后登录态丢失，或 WAL 残留被客户端启动重放使旧账号复活。
    pub graceful_wait_secs: u64,
    /// 进程名白名单（不带 `.exe`，精确匹配；Stop 用）。
    pub proc_names: &'static [&'static str],
    /// 进程名通配组（**exe 发现专用**，比 Stop 的精确组更宽，再经 `exe_names`
    /// 白名单过滤防串台）。
    pub proc_patterns: &'static [&'static str],
    /// exe 文件名白名单（lnk/注册表/进程回退防串台）。
    pub exe_names: &'static [&'static str],
    /// `.lnk` 文件名匹配模式。
    pub lnk_patterns: &'static [&'static str],
    /// 注册表 DisplayName 匹配模式。
    pub reg_patterns: &'static [&'static str],
    /// exe 候选路径（环境变量展开后的绝对路径）。
    pub exe_candidates: Vec<PathBuf>,
}

impl AppProfile {
    /// `current_account.txt` 路径（记录当前登录态属于哪个账号）。
    pub fn current_account_file(&self) -> PathBuf {
        self.profiles_dir.join("current_account.txt")
    }

    /// 指定账号的快照槽目录。
    pub fn slot_dir(&self, slot: &str) -> PathBuf {
        self.profiles_dir.join(slot)
    }

    /// 应用数据目录是否已存在（未安装/从未启动过则为假）。
    pub fn data_dir_exists(&self) -> bool {
        self.data_dir.is_dir()
    }
}

/// 路径文件名是否属于该应用（防 lnk/注册表/进程回退解析到另一个应用；大小写不敏感）。
pub fn exe_matches(path: &Path, prof: &AppProfile) -> bool {
    match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => prof
            .exe_names
            .iter()
            .any(|e| e.eq_ignore_ascii_case(name)),
        None => false,
    }
}

/// 取环境变量（缺失返回空串，交由调用方 `PathBuf::from` 处理）。
fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_default()
}

/// 档案表：按应用返回其数据目录、进程名与快照布局。
///
/// `store_dir` 是本软件的数据目录（各应用的快照都放在它下面，互相隔离）。
pub fn profile_for(app: TargetApp, store_dir: &Path) -> AppProfile {
    let appdata = env("APPDATA");
    let local = env("LOCALAPPDATA");
    let home = env("USERPROFILE");
    let program_files = env("ProgramFiles");
    let data = store_dir;

    match app {
        TargetApp::Trae => AppProfile {
            app_name: "Trae",
            layout: Layout::Icube,
            data_dir: PathBuf::from(format!("{appdata}\\Trae CN")),
            profiles_dir: data.join("profiles_trae"),
            settings_path_key: "trae_cn_path",
            graceful_wait_secs: 8,
            proc_names: &["Trae CN"],
            proc_patterns: &["Trae*", "TRAE*"],
            exe_names: &["Trae CN.exe"],
            lnk_patterns: &["*TRAE*", "*Trae*"],
            reg_patterns: &["*TRAE*", "*Trae*"],
            exe_candidates: vec![
                PathBuf::from(format!("{local}\\Programs\\Trae CN\\Trae CN.exe")),
                PathBuf::from(format!("{program_files}\\Trae CN\\Trae CN.exe")),
                PathBuf::from("D:\\Programs\\Trae CN\\Trae CN.exe"),
            ],
        },
        TargetApp::TraeWork => AppProfile {
            app_name: "Trae Work",
            layout: Layout::Icube,
            data_dir: PathBuf::from(format!("{appdata}\\TRAE SOLO CN")),
            profiles_dir: data.join("profiles"),
            settings_path_key: "trae_path",
            graceful_wait_secs: 8,
            proc_names: &["TRAE SOLO CN", "TRAE SOLO", "Trae"],
            proc_patterns: &["Trae*", "TRAE*"],
            exe_names: &["TRAE SOLO CN.exe", "TRAE SOLO.exe", "Trae.exe"],
            lnk_patterns: &["*TRAE*", "*Trae*"],
            reg_patterns: &["*TRAE*", "*Trae*"],
            exe_candidates: vec![
                PathBuf::from(format!("{local}\\Programs\\TRAE SOLO CN\\TRAE SOLO CN.exe")),
                PathBuf::from(format!("{local}\\Programs\\TRAE SOLO\\TRAE SOLO.exe")),
                PathBuf::from(format!("{program_files}\\TRAE SOLO CN\\TRAE SOLO CN.exe")),
                PathBuf::from(format!("{program_files}\\TRAE SOLO\\TRAE SOLO.exe")),
                PathBuf::from(format!("{local}\\Programs\\Trae\\Trae.exe")),
                PathBuf::from(format!("{program_files}\\Trae\\Trae.exe")),
                PathBuf::from("D:\\Programs\\TRAE SOLO CN\\TRAE SOLO CN.exe"),
            ],
        },
        TargetApp::Doubao => AppProfile {
            app_name: "豆包",
            layout: Layout::Chromium,
            data_dir: PathBuf::from(format!("{local}\\Doubao\\User Data")),
            profiles_dir: data.join("profiles_doubao"),
            settings_path_key: "doubao_path",
            graceful_wait_secs: 8,
            proc_names: &["Doubao"],
            proc_patterns: &["Doubao*"],
            exe_names: &["Doubao.exe"],
            lnk_patterns: &["*Doubao*", "*豆包*"],
            reg_patterns: &["*Doubao*", "*豆包*"],
            exe_candidates: vec![
                PathBuf::from(format!("{local}\\Doubao\\Application\\Doubao.exe")),
                PathBuf::from(format!("{program_files}\\Doubao\\Application\\Doubao.exe")),
            ],
        },
        TargetApp::WorkBuddy => AppProfile {
            app_name: "WorkBuddy",
            layout: Layout::Authfile,
            data_dir: PathBuf::from(format!("{home}\\.workbuddy")),
            profiles_dir: data.join("profiles_workbuddy"),
            settings_path_key: "workbuddy_path",
            graceful_wait_secs: 5,
            // auth 文件虽与 CodeBuddy 共用同一物理文件，但实测 CodeBuddy 从不回写
            // 共享 auth 文件（登录真源在自身 vscdb）——切/存 WorkBuddy 不关停
            // CodeBuddy，两端完全独立。
            proc_names: &["WorkBuddy"],
            proc_patterns: &["WorkBuddy*"],
            exe_names: &["WorkBuddy.exe"],
            lnk_patterns: &["*WorkBuddy*"],
            reg_patterns: &["*WorkBuddy*"],
            exe_candidates: vec![PathBuf::from(format!(
                "{local}\\Programs\\WorkBuddy\\WorkBuddy.exe"
            ))],
        },
        TargetApp::CodeBuddy => AppProfile {
            app_name: "CodeBuddy",
            layout: Layout::Authfile,
            data_dir: PathBuf::from(format!("{home}\\.codebuddy")),
            profiles_dir: data.join("profiles_codebuddy"),
            settings_path_key: "codebuddy_path",
            graceful_wait_secs: 5,
            // CodeBuddy 登录真源在自身 state.vscdb（%APPDATA%\CodeBuddy CN），
            // 不消费共享 auth 文件——切/存 CodeBuddy 不关停在跑的 WorkBuddy。
            proc_names: &["CodeBuddy", "CodeBuddy CN"],
            proc_patterns: &["CodeBuddy*"],
            exe_names: &["CodeBuddy.exe", "CodeBuddy CN.exe"],
            lnk_patterns: &["*CodeBuddy*"],
            reg_patterns: &["*CodeBuddy*"],
            exe_candidates: vec![
                PathBuf::from(format!("{local}\\Programs\\CodeBuddy\\CodeBuddy.exe")),
                PathBuf::from(format!("{local}\\Programs\\CodeBuddy CN\\CodeBuddy CN.exe")),
            ],
        },
    }
}

/// 是否需要为 authfile 布局额外覆盖 VS Code fork 的 `globalStorage`
/// （CodeBuddy CN 的登录真源在那里，不在共享 auth 文件）。
pub fn extra_global_storage_dir(app: TargetApp) -> Option<PathBuf> {
    match app {
        TargetApp::CodeBuddy => Some(PathBuf::from(format!(
            "{}\\CodeBuddy CN\\User\\globalStorage",
            env("APPDATA")
        ))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> PathBuf {
        PathBuf::from("C:\\test-store")
    }

    #[test]
    fn 五应用档案的关键字段与上游常量表一致() {
        let s = store();

        let tw = profile_for(TargetApp::TraeWork, &s);
        assert_eq!(tw.app_name, "Trae Work");
        assert_eq!(tw.layout, Layout::Icube);
        assert_eq!(tw.settings_path_key, "trae_path");
        assert_eq!(tw.graceful_wait_secs, 8);
        assert_eq!(tw.proc_names, &["TRAE SOLO CN", "TRAE SOLO", "Trae"]);
        assert_eq!(tw.exe_candidates.len(), 7);
        assert_eq!(tw.profiles_dir, s.join("profiles"));

        let trae = profile_for(TargetApp::Trae, &s);
        assert_eq!(trae.app_name, "Trae");
        assert_eq!(trae.layout, Layout::Icube);
        assert_eq!(trae.settings_path_key, "trae_cn_path");
        assert_eq!(trae.profiles_dir, s.join("profiles_trae"));
        assert_eq!(trae.exe_candidates.len(), 3);

        let db = profile_for(TargetApp::Doubao, &s);
        assert_eq!(db.app_name, "豆包");
        assert_eq!(db.layout, Layout::Chromium);
        assert_eq!(db.graceful_wait_secs, 8);
        assert_eq!(db.profiles_dir, s.join("profiles_doubao"));

        let wb = profile_for(TargetApp::WorkBuddy, &s);
        assert_eq!(wb.layout, Layout::Authfile);
        assert_eq!(wb.graceful_wait_secs, 5);
        assert_eq!(wb.proc_names, &["WorkBuddy"]);

        let cb = profile_for(TargetApp::CodeBuddy, &s);
        assert_eq!(cb.proc_names, &["CodeBuddy", "CodeBuddy CN"]);
    }

    #[test]
    fn 两个_trae_应用快照目录互不冲突() {
        let s = store();
        assert_ne!(
            profile_for(TargetApp::TraeWork, &s).profiles_dir,
            profile_for(TargetApp::Trae, &s).profiles_dir,
            "Trae Work 与 Trae 是两套登录态，快照必须分开存"
        );
    }

    #[test]
    fn current_account_file_位于快照根() {
        let s = store();
        let tw = profile_for(TargetApp::TraeWork, &s);
        assert_eq!(
            tw.current_account_file(),
            s.join("profiles").join("current_account.txt")
        );
    }

    #[test]
    fn exe_matches_大小写不敏感且白名单外拒绝() {
        let tw = profile_for(TargetApp::TraeWork, &store());
        assert!(exe_matches(Path::new("C:\\x\\trae solo cn.EXE"), &tw));
        assert!(exe_matches(Path::new("D:\\a\\Trae.exe"), &tw));
        // 白名单外的 exe（Trae CN.exe 属于 Trae 档案）拒绝——防串台
        assert!(!exe_matches(Path::new("C:\\x\\Trae CN.exe"), &tw));
        assert!(!exe_matches(Path::new("C:\\x\\Doubao.exe"), &tw));
    }

    #[test]
    fn parse_未知值回退_trae_work() {
        assert_eq!(TargetApp::parse("Doubao"), TargetApp::Doubao);
        assert_eq!(TargetApp::parse("doubao"), TargetApp::Doubao);
        assert_eq!(TargetApp::parse("Trae"), TargetApp::Trae);
        assert_eq!(TargetApp::parse("WorkBuddy"), TargetApp::WorkBuddy);
        assert_eq!(TargetApp::parse("CodeBuddy"), TargetApp::CodeBuddy);
        assert_eq!(TargetApp::parse("nonsense"), TargetApp::TraeWork);
        assert_eq!(TargetApp::parse(""), TargetApp::TraeWork);
    }

    #[test]
    fn parse_与_as_str_互逆() {
        for app in TargetApp::ALL {
            assert_eq!(TargetApp::parse(app.as_str()), app);
        }
    }

    #[test]
    fn 仅_codebuddy_需要额外_global_storage() {
        assert!(extra_global_storage_dir(TargetApp::CodeBuddy).is_some());
        assert!(extra_global_storage_dir(TargetApp::WorkBuddy).is_none());
        assert!(extra_global_storage_dir(TargetApp::TraeWork).is_none());
        assert!(extra_global_storage_dir(TargetApp::Doubao).is_none());
    }
}
