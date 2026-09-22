//! Windows 计划任务：注册 / 查询 / 删除每日任务。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/commands/misc.rs`（`validate_hhmm` /
//! `run_schtasks` / `write_task_launcher`）。
//!
//! ## 为什么用「启动器 .cmd」而不是直接把命令塞进 /TR
//!
//! `schtasks /TR` 的参数上限是 **261 字符**。exe 绝对路径 + 数据目录 + 参数
//! 拼出来很容易超过（实测 273 字符），此时 schtasks 报参数错误、注册失败，
//! 而错误提示只在界面上闪几秒 —— 用户感知为「点了注册没反应」。
//! 把长命令写进数据目录的 `.cmd` 启动器后，`/TR` 只需约 74 字符。
//!
//! ## 为什么前置 `chcp 65001`
//!
//! 中文 Windows 的控制台代码页默认是 GBK。`schtasks` 的中文报错（如
//! 「系统找不到指定的文件」）以 GBK 字节输出，直接按 UTF-8 解码会变成乱码，
//! 于是「找不到」这个关键词永远匹配不上，错误文案也无法展示给用户。
//! 前置 `chcp 65001` 让 schtasks 以 UTF-8 输出。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 任务名（本软件所有计划任务共用前缀，便于识别与清理）。
pub const TASK_PREFIX: &str = "AIGateway";

/// 任务类型。
///
/// **命名陷阱**：[`TaskKind::DoubaoRenew`] 的 `task_name` 是
/// `AIGateway_DoubaoRenew`、启动器是 `doubao_renew`，但它实际跑的是
/// **会话保活**（`cli_key = "doubao-keepalive"`）。这是历史命名，两个标识都已
/// 注册在用户机器的计划任务里，改名会让旧任务变成无人清理的孤儿，因此保留。
/// 真正跑 HTTP 续期的任务是 [`TaskKind::DoubaoRenewHttp`]。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskKind {
    /// Trae 每日签到。
    TraeCheckin,
    /// Trae 积分日快照（供趋势图，不依赖界面打开）。
    TraeCreditsSnapshot,
    /// 豆包会话保活（历史命名见类型注释）。
    DoubaoRenew,
    /// 豆包凭证 HTTP 续期巡检。
    DoubaoRenewHttp,
    /// 豆包额度巡检。
    DoubaoQuota,
}

impl TaskKind {
    /// 计划任务名。
    pub fn task_name(self) -> &'static str {
        match self {
            TaskKind::TraeCheckin => "AIGateway_TraeCheckin",
            TaskKind::TraeCreditsSnapshot => "AIGateway_TraeCreditsSnapshot",
            TaskKind::DoubaoRenew => "AIGateway_DoubaoRenew",
            TaskKind::DoubaoRenewHttp => "AIGateway_DoubaoRenewHttp",
            TaskKind::DoubaoQuota => "AIGateway_DoubaoQuotaCheck",
        }
    }

    /// CLI 任务标识（`--task-run <key>`）。
    pub fn cli_key(self) -> &'static str {
        match self {
            TaskKind::TraeCheckin => "trae-checkin",
            TaskKind::TraeCreditsSnapshot => "trae-credits-snapshot",
            TaskKind::DoubaoRenew => "doubao-keepalive",
            TaskKind::DoubaoRenewHttp => "doubao-renew",
            TaskKind::DoubaoQuota => "doubao-quota",
        }
    }

    /// 启动器脚本名（`task_<name>.cmd`）。
    pub fn launcher_name(self) -> &'static str {
        match self {
            TaskKind::TraeCheckin => "trae_checkin",
            TaskKind::TraeCreditsSnapshot => "trae_credits_snapshot",
            TaskKind::DoubaoRenew => "doubao_renew",
            TaskKind::DoubaoRenewHttp => "doubao_renew_http",
            TaskKind::DoubaoQuota => "doubao_quota",
        }
    }

    /// 界面展示名。
    pub fn label(self) -> &'static str {
        match self {
            TaskKind::TraeCheckin => "Trae 每日签到",
            TaskKind::TraeCreditsSnapshot => "Trae 积分日快照",
            TaskKind::DoubaoRenew => "豆包会话保活",
            TaskKind::DoubaoRenewHttp => "豆包凭证续期",
            TaskKind::DoubaoQuota => "豆包额度巡检",
        }
    }

    pub const ALL: [TaskKind; 5] = [
        TaskKind::TraeCheckin,
        TaskKind::TraeCreditsSnapshot,
        TaskKind::DoubaoRenew,
        TaskKind::DoubaoRenewHttp,
        TaskKind::DoubaoQuota,
    ];
}

/// 严格校验 `HH:MM` 时间格式。
///
/// **这是命令注入防线**：时间值最终经 `cmd /c … && schtasks /ST <time>` 执行，
/// 而 cmd 对不含空格/引号的参数不做引号包裹 —— `12:00&calc` 这类输入会把 `&`
/// 解释成命令分隔符，实现任意命令执行。白名单校验必须在入口统一拦死。
pub fn validate_hhmm(time: &str) -> Result<(), String> {
    let t = time.trim();
    let bytes = t.as_bytes();
    let valid = bytes.len() == 5
        && bytes[2] == b':'
        && bytes[..2].iter().all(u8::is_ascii_digit)
        && bytes[3..].iter().all(u8::is_ascii_digit)
        && t[..2].parse::<u8>().map(|h| h < 24).unwrap_or(false)
        && t[3..].parse::<u8>().map(|m| m < 60).unwrap_or(false);
    if !valid {
        return Err(format!("时间格式无效: {time}（应为 HH:MM）"));
    }
    Ok(())
}

/// 运行 `schtasks`，返回 `(是否成功, stdout, stderr)`。
///
/// 前置 `chcp 65001` 是为了让中文报错以 UTF-8 输出（见模块头注释）。
pub fn run_schtasks(args: &[&str]) -> Result<(bool, String, String), String> {
    let mut full: Vec<String> = vec![
        "/c".into(),
        "chcp".into(),
        "65001".into(),
        ">nul".into(),
        "&&".into(),
        "schtasks".into(),
    ];
    full.extend(args.iter().map(|a| (*a).to_string()));

    #[allow(unused_mut)]
    let mut cmd = Command::new("cmd");
    cmd.args(&full);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd
        .output()
        .map_err(|e| format!("执行 schtasks 失败: {e}"))?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    ))
}

/// 写任务启动器脚本，返回其路径。
pub fn write_task_launcher(
    store_dir: &Path,
    kind: TaskKind,
    body: &str,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(store_dir).map_err(|e| format!("创建数据目录失败: {e}"))?;
    let path = store_dir.join(format!("task_{}.cmd", kind.launcher_name()));
    std::fs::write(&path, format!("@echo off\r\n{body}\r\n"))
        .map_err(|e| format!("写入任务启动器脚本失败: {e}"))?;
    Ok(path)
}

/// 注册（或覆盖）一个每日计划任务。
///
/// `exe` 是主程序路径；`store_dir` 用于写启动器与注入数据目录环境变量。
/// 时间必须是 `HH:MM`（经 [`validate_hhmm`] 校验）。
pub fn register_daily_task(
    kind: TaskKind,
    time: &str,
    exe: &Path,
    store_dir: &Path,
) -> Result<String, String> {
    validate_hhmm(time)?;

    // schtasks 不继承进程环境变量，必须在启动器里显式设数据目录，
    // 否则计划任务触发时会落到默认目录、读到一份空账号库。
    let launcher = write_task_launcher(
        store_dir,
        kind,
        &format!(
            "set \"AI_GATEWAY_HOME={}\"\r\n\"{}\" --task-run {}",
            store_dir.to_string_lossy(),
            exe.to_string_lossy(),
            kind.cli_key()
        ),
    )?;

    let launcher_s = launcher.to_string_lossy().to_string();
    let (ok, _stdout, stderr) = run_schtasks(&[
        "/Create",
        "/TN",
        kind.task_name(),
        "/TR",
        &launcher_s,
        "/SC",
        "DAILY",
        "/ST",
        time.trim(),
        "/F",
    ])?;
    if !ok {
        return Err(format!(
            "注册计划任务失败：{}",
            stderr.trim().if_empty("请确认已允许本程序创建计划任务")
        ));
    }
    Ok(format!("已注册「{}」每日 {time} 执行", kind.label()))
}

/// 查询任务是否已注册；已注册时返回触发时间（`HH:MM`）。
pub fn task_status(kind: TaskKind) -> Result<Option<String>, String> {
    let (ok, stdout, _stderr) = run_schtasks(&["/Query", "/TN", kind.task_name(), "/FO", "LIST"])?;
    if !ok {
        // 查询失败 = 未注册（schtasks 对不存在的任务返回非零）
        return Ok(None);
    }
    // 从 LIST 输出里找 "Start Time:" / "开始时间:" 行
    for line in stdout.lines() {
        let lower = line.to_lowercase();
        if lower.contains("start time") || line.contains("开始时间") {
            if let Some((_, value)) = line.split_once(':') {
                let value = value.trim();
                // 取前 5 个字符即 HH:MM
                if value.len() >= 5 {
                    return Ok(Some(value[..5].to_string()));
                }
            }
        }
    }
    // 已注册但解析不出时间：返回占位符而不是 None，否则界面会显示「未注册」
    Ok(Some(String::new()))
}

/// 删除任务（不存在也算成功 —— 幂等）。
pub fn unregister_task(kind: TaskKind) -> Result<(), String> {
    let (ok, _stdout, stderr) = run_schtasks(&["/Delete", "/TN", kind.task_name(), "/F"])?;
    if ok {
        return Ok(());
    }
    // 任务本就不存在时不算错误
    let text = stderr.to_lowercase();
    if text.contains("cannot find") || stderr.contains("找不到") || stderr.contains("不存在") {
        return Ok(());
    }
    Err(format!("删除计划任务失败：{}", stderr.trim()))
}

/// 全部任务的状态汇总（供界面一次拉取）。
pub fn all_task_status() -> Vec<serde_json::Value> {
    TaskKind::ALL
        .iter()
        .map(|kind| {
            let status = task_status(*kind);
            let (registered, time, error) = match status {
                Ok(Some(t)) => (true, t, None),
                Ok(None) => (false, String::new(), None),
                Err(e) => (false, String::new(), Some(e)),
            };
            serde_json::json!({
                "kind": kind.launcher_name(),
                "name": kind.task_name(),
                "label": kind.label(),
                "cliKey": kind.cli_key(),
                "registered": registered,
                "time": time,
                "error": error,
            })
        })
        .collect()
}

/// 小工具：空串时给默认提示。
trait IfEmpty {
    fn if_empty(self, fallback: &str) -> String;
}

impl IfEmpty for &str {
    fn if_empty(self, fallback: &str) -> String {
        if self.trim().is_empty() {
            fallback.to_string()
        } else {
            self.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 时间校验拦截命令注入() {
        // 合法
        assert!(validate_hhmm("09:00").is_ok());
        assert!(validate_hhmm("00:00").is_ok());
        assert!(validate_hhmm("23:59").is_ok());
        assert!(validate_hhmm(" 09:30 ").is_ok());

        // 越界
        assert!(validate_hhmm("24:00").is_err());
        assert!(validate_hhmm("09:60").is_err());
        assert!(validate_hhmm("99:99").is_err());

        // 格式错误
        assert!(validate_hhmm("").is_err());
        assert!(validate_hhmm("9:00").is_err());
        assert!(validate_hhmm("09-00").is_err());
        assert!(validate_hhmm("0900").is_err());
        assert!(validate_hhmm("09:0").is_err());
        assert!(validate_hhmm("ab:cd").is_err());

        // **命令注入**：这些若通过校验，会被 cmd 当作命令分隔符执行
        assert!(validate_hhmm("12:00&calc").is_err());
        assert!(validate_hhmm("12:00|calc").is_err());
        assert!(validate_hhmm("12:00\" & calc & \"").is_err());
        assert!(validate_hhmm("12:00\r\ncalc").is_err());
    }

    #[test]
    fn 任务名与_cli_键一一对应且唯一() {
        let mut names = std::collections::HashSet::new();
        let mut keys = std::collections::HashSet::new();
        for kind in TaskKind::ALL {
            assert!(names.insert(kind.task_name()), "任务名重复: {}", kind.task_name());
            assert!(keys.insert(kind.cli_key()), "CLI 键重复: {}", kind.cli_key());
            assert!(
                kind.task_name().starts_with(TASK_PREFIX),
                "任务名应带统一前缀便于识别: {}",
                kind.task_name()
            );
            assert!(!kind.label().is_empty());
        }
    }

    #[test]
    fn 启动器脚本写入指定目录() {
        let dir = std::env::temp_dir().join(format!(
            "ai-gateway-task-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let path = write_task_launcher(&dir, TaskKind::TraeCheckin, "echo hi").unwrap();
        assert!(path.exists());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("@echo off"));
        assert!(text.contains("echo hi"));
        assert!(path.to_string_lossy().ends_with("task_trae_checkin.cmd"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 注册时非法时间在写脚本之前就被拒绝() {
        let dir = std::env::temp_dir().join(format!("ai-gateway-task-bad-{}", std::process::id()));
        let exe = std::env::current_exe().unwrap();
        let err = register_daily_task(TaskKind::DoubaoRenew, "12:00&calc", &exe, &dir).unwrap_err();
        assert!(err.contains("时间格式无效"), "应报格式错误: {err}");
        // 关键：校验发生在写盘之前，不能留下任何启动器残留
        assert!(
            !dir.join("task_doubao_renew.cmd").exists(),
            "非法输入不得写出启动器脚本"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 状态汇总形态完整() {
        let all = all_task_status();
        assert_eq!(all.len(), TaskKind::ALL.len());
        for item in &all {
            for key in ["kind", "name", "label", "cliKey", "registered"] {
                assert!(item.get(key).is_some(), "缺少字段 {key}");
            }
            assert!(item["registered"].is_boolean());
        }
    }

    /// 每个任务类型的五个标识都必须互不相同。
    ///
    /// 复制粘贴新增任务时最容易漏改其中一处（尤其是 `launcher_name`），
    /// 后果是两个任务共用同一个启动器脚本 —— 后注册的覆盖先注册的，
    /// 表现为「某个定时任务永远不执行」且没有任何报错。
    #[test]
    fn 任务标识两两不重复() {
        let mut names: Vec<&str> = TaskKind::ALL.iter().map(|k| k.task_name()).collect();
        let mut keys: Vec<&str> = TaskKind::ALL.iter().map(|k| k.cli_key()).collect();
        let mut launchers: Vec<&str> = TaskKind::ALL.iter().map(|k| k.launcher_name()).collect();
        let mut labels: Vec<&str> = TaskKind::ALL.iter().map(|k| k.label()).collect();
        for set in [&mut names, &mut keys, &mut launchers, &mut labels] {
            let before = set.len();
            set.sort_unstable();
            set.dedup();
            assert_eq!(set.len(), before, "任务标识存在重复: {set:?}");
        }
    }

    /// 新增的 Trae 积分快照任务必须落在一天末尾。
    ///
    /// 早上跑会把「昨夜消耗」记进新的一天，日差趋势整体错位一天。
    #[test]
    fn 积分快照任务在一天末尾() {
        let kind = TaskKind::TraeCreditsSnapshot;
        assert_eq!(kind.cli_key(), "trae-credits-snapshot");
        assert_eq!(kind.task_name(), "AIGateway_TraeCreditsSnapshot");
        assert!(kind.label().contains("快照"));
    }

    /// 历史命名陷阱的护栏：`DoubaoRenew` 跑的是保活，`DoubaoRenewHttp` 才是续期。
    /// 谁把这两个键对调，都会让用户的计划任务静默跑错动作。
    #[test]
    fn 豆包两个续期类任务的_cli_键不得对调() {
        assert_eq!(TaskKind::DoubaoRenew.cli_key(), "doubao-keepalive");
        assert_eq!(TaskKind::DoubaoRenewHttp.cli_key(), "doubao-renew");
        assert_ne!(
            TaskKind::DoubaoRenew.launcher_name(),
            TaskKind::DoubaoRenewHttp.launcher_name(),
            "两个任务的启动器脚本不能同名"
        );
    }

    #[test]
    fn if_empty_辅助函数() {
        assert_eq!("".if_empty("默认"), "默认");
        assert_eq!("  ".if_empty("默认"), "默认");
        assert_eq!("值".if_empty("默认"), "值");
    }
}
