//! 登录态切换器：备份 → 关进程 → 恢复 → 启动。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/switcher/`，按本仓库「core 不依赖 Tauri」
//! 的约定改写 —— 进度从 `app.emit` 收敛为 [`ProgressSink`] 回调，宿主（桌面端）
//! 自行把它转发成事件。
//!
//! 覆盖两种快照布局：
//! - [`Layout::Icube`]：Trae Work / Trae
//! - [`Layout::Chromium`]：豆包
//!
//! WorkBuddy / CodeBuddy 的切换由本仓库既有的 `auth_file` / `codebuddy_*` 模块负责，
//! 不在此重复实现（两套布局的登录真源与恢复语义不同，合并只会互相污染）。
//!
//! ## 快照数据兼容
//!
//! `profiles*/<slot>{,.bak}` 结构、`current_account.txt`、`snapshot_meta.json` 格式
//! 与上游一致，新旧版本快照互认。

pub mod chromium;
pub mod copy;
pub mod icube;
pub mod locate;
pub mod machine;
pub mod proc;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::modules::app_profile::{profile_for, AppProfile, Layout, TargetApp};
use crate::modules::config;

/// 一次切换动作的类型。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// 切到目标账号：备份当前登录态 → 恢复目标快照 → 启动。
    Switch,
    /// 把当前客户端的登录态存进指定账号的槽位。
    SaveCurrentLogin,
    /// 只备份当前登录态到 `last`，不改任何账号槽。
    BackupCurrent,
    /// 只恢复指定账号的快照（不备份当前）。
    RestoreOnly,
    /// 重置 6 层设备标识（icube 布局）。
    ResetDeviceIds,
    /// 保活：启动客户端 → 等待落盘 → 优雅关闭（触发服务端会话滑动续期）。
    KeepAlive,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Switch => "Switch",
            Action::SaveCurrentLogin => "SaveCurrentLogin",
            Action::BackupCurrent => "BackupCurrent",
            Action::RestoreOnly => "RestoreOnly",
            Action::ResetDeviceIds => "ResetDeviceIds",
            Action::KeepAlive => "KeepAlive",
        }
    }
}

/// 进度步骤的状态。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StepStatus {
    Info,
    Ok,
    Warn,
    Error,
    Skip,
}

impl StepStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            StepStatus::Info => "info",
            StepStatus::Ok => "ok",
            StepStatus::Warn => "warn",
            StepStatus::Error => "error",
            StepStatus::Skip => "skip",
        }
    }
}

/// 进度接收器（宿主注入：桌面端转发为 Tauri 事件，CLI 写日志）。
pub trait ProgressSink: Send + Sync {
    fn step(&self, stage: &str, status: StepStatus, message: &str);
}

/// 丢弃全部进度的接收器（测试与静默场景）。
pub struct NullSink;

impl ProgressSink for NullSink {
    fn step(&self, _: &str, _: StepStatus, _: &str) {}
}

/// 只写 stderr 的接收器（CLI / 后台任务）。
pub struct LogSink;

impl ProgressSink for LogSink {
    fn step(&self, stage: &str, status: StepStatus, message: &str) {
        eprintln!("[switch] [{stage}] {} {message}", status.as_str());
    }
}

/// 记录到内存的接收器（供调用方把步骤回传给前端）。
#[derive(Default)]
pub struct VecSink {
    steps: std::sync::Mutex<Vec<(String, StepStatus, String)>>,
}

impl VecSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取全部步骤的 JSON 形态（`{stage,status,message}`）。
    pub fn steps(&self) -> Vec<serde_json::Value> {
        self.steps
            .lock()
            .map(|steps| {
                steps
                    .iter()
                    .map(|(stage, status, message)| {
                        serde_json::json!({
                            "stage": stage,
                            "status": status.as_str(),
                            "message": message,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl ProgressSink for VecSink {
    fn step(&self, stage: &str, status: StepStatus, message: &str) {
        if let Ok(mut steps) = self.steps.lock() {
            steps.push((stage.to_string(), status, message.to_string()));
        }
    }
}

/// 一次桥动作的完整入参。
pub struct RunArgs {
    pub action: Action,
    pub target_app: TargetApp,
    /// 目标账号（`Switch` / `SaveCurrentLogin` / `RestoreOnly` 必填）。
    pub user_id: Option<String>,
    /// `>0` 时给客户端注入 `--proxy-server`（一键以账号打开，走 MITM 代理）。
    pub proxy_port: Option<u16>,
    /// 豆包快照是否纳入 `IndexedDB`（对话历史等完整状态；体积代价大，默认排除）。
    pub include_indexeddb: bool,
    /// 防误覆盖守卫：桌面端关闭前检测到的当前登录 uid。
    ///
    /// 只有它与 `current_account.txt` 一致时，才允许把「现场登录态」写回账号槽 ——
    /// 否则一次手动重登就会把错误状态刷进槽位且不可恢复。
    pub expected_current_uid: String,
    /// 本软件数据目录（快照根）。
    pub store_dir: PathBuf,
}

/// 执行会话：一次 `run_action` 内共享的可变状态。
pub struct Session {
    pub prof: AppProfile,
    pub store_dir: PathBuf,
    /// exe 发现结果缓存（Stop 前缓存，Start 时优先复用）。
    pub exe_cache: Option<PathBuf>,
    /// icube 恢复的项数（恢复后校验用；每次恢复先置 -1，仅 icube 结尾写实际值）。
    pub last_restored_count: i64,
    /// 启动时注入的代理端口。
    pub launch_proxy_port: Option<u16>,
    pub include_indexeddb: bool,
}

impl Session {
    pub fn new(args: &RunArgs) -> Self {
        Self {
            prof: profile_for(args.target_app, &args.store_dir),
            store_dir: args.store_dir.clone(),
            exe_cache: None,
            last_restored_count: -1,
            launch_proxy_port: args.proxy_port.filter(|p| *p > 0),
            include_indexeddb: args.include_indexeddb,
        }
    }

    /// 日志文件（`<store_dir>/logs/switcher.log`）。
    pub fn log_file(&self) -> PathBuf {
        self.store_dir.join("logs").join("switcher.log")
    }
}

/// 全局动作互斥：切换 / 保存 / 恢复 / 保活同时只能跑一个。
///
/// 为什么必须串行：多个动作都会「关进程 → 改登录态 → 启进程」，并发执行时
/// A 的关进程会掐掉 B 刚启动的客户端，B 的恢复又会覆盖 A 刚写好的登录态，
/// 最终得到一个两个账号都不认的混合状态。
static ACTION_BUSY: AtomicBool = AtomicBool::new(false);

/// 串行化守卫（RAII：离开作用域自动释放）。
pub struct ActionGate;

impl ActionGate {
    /// 尝试获取；已被占用返回 `None`。
    pub fn try_acquire() -> Option<Self> {
        ACTION_BUSY
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| ActionGate)
    }
}

impl Drop for ActionGate {
    fn drop(&mut self) {
        ACTION_BUSY.store(false, Ordering::SeqCst);
    }
}

/// 读 `current_account.txt`（记录当前登录态属于哪个账号）。
///
/// 三种去 BOM 方式都做：Rust 的 `trim()` **不**去除 U+FEFF，而 PowerShell 5.1
/// 的 `Set-Content -Encoding UTF8` 会写入 BOM —— 上游 PS 桥留下的文件因此带 BOM，
/// 不去掉会得到一个「看不见前缀」的 uid，与账号库里任何 uid 都不相等。
pub fn get_current_account(sess: &Session) -> String {
    let path = sess.prof.current_account_file();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return String::new();
    };
    raw.trim_start_matches('\u{feff}')
        .trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
        .to_string()
}

/// 写 `current_account.txt`（无 BOM，UTF-8）。
pub fn set_current_account(sess: &Session, uid: &str) -> Result<(), String> {
    std::fs::create_dir_all(&sess.prof.profiles_dir)
        .map_err(|e| format!("创建快照目录失败: {e}"))?;
    std::fs::write(sess.prof.current_account_file(), uid.trim())
        .map_err(|e| format!("写入当前账号标记失败: {e}"))
}

/// 把档案里的 **Windows 相对路径**（`User\globalStorage\storage.json`）拼到根目录上。
///
/// 为什么不能直接 `root.join(rel)`：`Path::join` 在非 Windows 平台上**不把 `\`
/// 当分隔符**，于是 `root/User\globalStorage\storage.json` 会被当成一个文件名 ——
/// 快照会写出一个名为 `User\globalStorage\storage.json` 的**单层文件**，
/// 恢复时也找不到真实路径。单测在 Linux / macOS 上就会失败（CI 三平台都跑）。
///
/// 这里按 `\` 与 `/` 逐段拆分后逐级 join，得到平台正确的路径。
pub fn join_windows_rel(root: &std::path::Path, rel: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    for segment in rel.split(['\\', '/']) {
        let segment = segment.trim();
        if !segment.is_empty() {
            path.push(segment);
        }
    }
    path
}

/// 把一步进度同时写日志文件与 sink。
pub fn log_step(sess: &Session, sink: &dyn ProgressSink, stage: &str, status: StepStatus, msg: &str) {
    sink.step(stage, status, msg);
    let line = format!(
        "[{}] [{stage}] {} {msg}\n",
        config::utc_iso(),
        status.as_str()
    );
    let path = sess.log_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// 执行一次切换动作。
pub fn run_action(args: RunArgs, sink: &dyn ProgressSink) -> Result<String, String> {
    let Some(_gate) = ActionGate::try_acquire() else {
        return Err("已有切换/保活动作正在执行，请等待完成".to_string());
    };
    let mut sess = Session::new(&args);

    match args.action {
        Action::SaveCurrentLogin => save_current_login_flow(&sess, args.user_id.as_deref(), sink),
        Action::BackupCurrent => {
            log_step(&sess, sink, "backup", StepStatus::Info, "正在备份当前登录态…");
            backup_current(&sess, "last", sink)
        }
        Action::RestoreOnly => restore_only_flow(&mut sess, args.user_id.as_deref(), sink),
        Action::Switch => switch_flow(&mut sess, &args, sink),
        Action::ResetDeviceIds => machine::reset_device_ids_only(&sess, sink),
        Action::KeepAlive => keepalive_flow(&sess, sink),
    }
}

/// 按布局备份当前登录态到指定槽位。
fn backup_current(sess: &Session, slot: &str, sink: &dyn ProgressSink) -> Result<String, String> {
    match sess.prof.layout {
        Layout::Icube => icube::backup_icube(sess, slot, sink)?,
        Layout::Chromium => chromium::backup_chromium(sess, slot, sink)?,
        Layout::Authfile => {
            return Err(
                "WorkBuddy / CodeBuddy 的登录态由账号管理页的「保存登录态」处理".to_string(),
            )
        }
    }
    Ok(format!("已备份当前登录态到 {slot}"))
}

/// 按布局把指定槽位恢复到现场。
fn restore_profile(sess: &mut Session, slot: &str, sink: &dyn ProgressSink) -> Result<(), String> {
    match sess.prof.layout {
        Layout::Icube => icube::restore_icube(sess, slot, sink),
        Layout::Chromium => chromium::restore_chromium(sess, slot, sink),
        Layout::Authfile => {
            Err("WorkBuddy / CodeBuddy 的登录态由账号管理页的「切换」处理".to_string())
        }
    }
}

/// 「保存当前登录态」：关进程 → 备份到账号槽 → 标记当前账号 → 启进程。
fn save_current_login_flow(
    sess: &Session,
    user_id: Option<&str>,
    sink: &dyn ProgressSink,
) -> Result<String, String> {
    let uid = user_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "缺少账号标识（userId）".to_string())?;

    log_step(sess, sink, "stop", StepStatus::Info, "正在关闭客户端…");
    proc::stop_app(sess, sink)?;

    log_step(sess, sink, "backup", StepStatus::Info, "正在保存当前登录态…");
    backup_current(sess, uid, sink)?;

    set_current_account(sess, uid)?;
    log_step(sess, sink, "mark", StepStatus::Ok, &format!("当前账号已标记为 {uid}"));

    log_step(sess, sink, "start", StepStatus::Info, "正在启动客户端…");
    proc::start_app(sess, sink)?;

    Ok(format!("已把当前登录态保存到账号 {uid}"))
}

/// 「只恢复」：关进程 → 备份现场到 `last` → 恢复目标槽 → 标记 → 启进程。
fn restore_only_flow(
    sess: &mut Session,
    user_id: Option<&str>,
    sink: &dyn ProgressSink,
) -> Result<String, String> {
    let uid = user_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "缺少账号标识（userId）".to_string())?;
    if !sess.prof.slot_dir(uid).exists()
        && !sess.prof.profiles_dir.join(format!("{uid}.bak")).exists()
    {
        return Err(format!("账号 {uid} 没有可用快照，请先保存该账号的登录态"));
    }

    proc::stop_app(sess, sink)?;
    backup_current(sess, "last", sink)?;
    restore_profile(sess, uid, sink)?;
    set_current_account(sess, uid)?;
    proc::start_app(sess, sink)?;
    Ok(format!("已恢复到账号 {uid}"))
}

/// 切换账号（含防误覆盖守卫）。
fn switch_flow(
    sess: &mut Session,
    args: &RunArgs,
    sink: &dyn ProgressSink,
) -> Result<String, String> {
    let uid = args
        .user_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "缺少账号标识（userId）".to_string())?;

    if !sess.prof.slot_dir(uid).exists()
        && !sess.prof.profiles_dir.join(format!("{uid}.bak")).exists()
    {
        return Err(format!("账号 {uid} 没有可用快照，请先保存该账号的登录态"));
    }

    proc::stop_app(sess, sink)?;

    // 先把现场存到 last（安全网：无论后续成败，现场都还在）
    backup_current(sess, "last", sink)?;

    // 防误覆盖守卫：只有「调用方看到的当前账号」与「文件记录的当前账号」一致时，
    // 才把现场写回来源槽。不一致说明用户在客户端手动重登过，此时写回会污染槽位。
    let cur = get_current_account(sess);
    if !cur.is_empty() && cur != uid {
        let expected = args.expected_current_uid.trim();
        if !expected.is_empty() && expected == cur {
            log_step(
                sess,
                sink,
                "backup",
                StepStatus::Info,
                &format!("正在把现场登录态写回来源账号 {cur}…"),
            );
            backup_current(sess, &cur, sink)?;
        } else {
            log_step(
                sess,
                sink,
                "backup",
                StepStatus::Warn,
                &format!(
                    "现场登录态属于 {cur}，与调用方预期（{}）不一致，已跳过写回以免污染该账号快照",
                    if expected.is_empty() { "未提供" } else { expected }
                ),
            );
        }
    }

    restore_profile(sess, uid, sink)?;
    set_current_account(sess, uid)?;

    // icube 布局：恢复后校验，项数为 0 或缺关键文件时回滚到 last
    if sess.prof.layout == Layout::Icube {
        if let Some(reason) = icube::verify_after_restore(sess, uid) {
            log_step(
                sess,
                sink,
                "restore",
                StepStatus::Error,
                &format!("恢复校验未通过（{reason}），正在回滚到切换前状态…"),
            );
            restore_profile(sess, "last", sink)?;
            let _ = set_current_account(sess, &cur);
            proc::start_app(sess, sink)?;
            return Err(format!("恢复校验未通过：{reason}（已回滚到切换前状态）"));
        }
    }

    proc::start_app(sess, sink)?;
    Ok(format!("已切换到账号 {uid}"))
}

/// 保活：启动客户端 → 等待落盘 → 优雅关闭。
///
/// 为什么这是豆包真正有效的续期方式：豆包客户端 cookie 在 Chromium `v10` 之外还有
/// 一层客户端级加密，离线拿不到明文 sessionid，无法用 HTTP 续期。而字节 passport
/// 是 30 天**滑动**续期 —— 只要客户端带着有效会话上线一次，服务端就会顺延。
fn keepalive_flow(sess: &Session, sink: &dyn ProgressSink) -> Result<String, String> {
    if proc::is_running(sess) {
        log_step(
            sess,
            sink,
            "keepalive",
            StepStatus::Skip,
            "客户端已在运行，无需保活（跳过以避免打断正在进行的会话）",
        );
        return Ok("客户端已在运行，已跳过".to_string());
    }
    log_step(sess, sink, "keepalive", StepStatus::Info, "正在启动客户端以刷新会话…");
    proc::start_app(sess, sink)?;
    std::thread::sleep(std::time::Duration::from_secs(8));
    proc::stop_app(sess, sink)?;
    log_step(sess, sink, "keepalive", StepStatus::Ok, "保活完成，会话已续期");
    Ok("保活完成".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_account_读写与_bom_剥离() {
        let dir = std::env::temp_dir().join(format!(
            "ai-gateway-cur-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let args = RunArgs {
            action: Action::BackupCurrent,
            target_app: TargetApp::TraeWork,
            user_id: None,
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            store_dir: dir.clone(),
        };
        let sess = Session::new(&args);

        // 文件不存在 → 空串（不报错）
        assert_eq!(get_current_account(&sess), "");

        set_current_account(&sess, "2117003799429594").unwrap();
        assert_eq!(get_current_account(&sess), "2117003799429594");

        // 模拟上游 PS 桥留下的带 BOM 文件
        std::fs::write(
            sess.prof.current_account_file(),
            "\u{feff}2117003799429594\r\n",
        )
        .unwrap();
        assert_eq!(
            get_current_account(&sess),
            "2117003799429594",
            "BOM 与换行都必须被剥离，否则 uid 与账号库不相等"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 动作闸门互斥且释放后可再次获取() {
        let first = ActionGate::try_acquire();
        assert!(first.is_some());
        assert!(
            ActionGate::try_acquire().is_none(),
            "并发动作必须被拒绝，否则会互相掐进程、互相覆盖登录态"
        );
        drop(first);
        assert!(ActionGate::try_acquire().is_some(), "释放后应可再次获取");
    }

    #[test]
    fn vec_sink_收集步骤() {
        let sink = VecSink::new();
        sink.step("stop", StepStatus::Ok, "已关闭");
        sink.step("start", StepStatus::Warn, "未找到 exe");
        let steps = sink.steps();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0]["stage"], "stop");
        assert_eq!(steps[0]["status"], "ok");
        assert_eq!(steps[1]["status"], "warn");
    }

    #[test]
    fn authfile_布局的切换被明确拒绝而非静默失败() {
        let dir = std::env::temp_dir().join(format!("ai-gateway-af-{}", std::process::id()));
        let args = RunArgs {
            action: Action::Switch,
            target_app: TargetApp::WorkBuddy,
            user_id: Some("u1".into()),
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            store_dir: dir,
        };
        let err = run_action(args, &NullSink).unwrap_err();
        assert!(
            err.contains("没有可用快照"),
            "WorkBuddy 切换应由既有模块处理，这里应报错：{err}"
        );
    }
}
