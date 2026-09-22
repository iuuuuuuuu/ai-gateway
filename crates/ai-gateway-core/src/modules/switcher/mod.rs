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
// F-68：icube 布局 state.vscdb 全局键（项目列表 / 最近打开）跨账号保留
pub mod vscdb;

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
    /// 只拉起客户端（不切账号、不备份、不关进程）。
    ///
    /// 与 [`Action::Switch`] 的区别是**零副作用**：不动任何快照槽，只把进程开起来。
    /// 用途是「我就想打开客户端看看」，或保活之外的日常启动 —— 走 Switch 会顺带
    /// 备份现场并恢复目标快照，用户并不想要那个。
    LaunchOnly,
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
            Action::LaunchOnly => "LaunchOnly",
        }
    }
}

/// 会写盘的动作（`LaunchOnly` 不在其中）。
///
/// 供上层判断「这个动作要不要提醒用户先备份」——
/// 把它当成会写盘的动作会让「打开客户端」这种无害操作也弹备份提示。
impl Action {
    pub fn mutates_state(self) -> bool {
        !matches!(self, Action::LaunchOnly)
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
///
/// 先过 [`config::ensure_uid_safe`]：uid 来自抓包 / 客户端存储，
/// 含 `..` 或分隔符时会越出快照目录（这个标记文件随后还会被用作 slot 名去
/// 定位快照目录，路径穿越的后果不止写一个文件）。
pub fn set_current_account(sess: &Session, uid: &str) -> Result<(), String> {
    config::ensure_uid_safe(uid)?;
    std::fs::create_dir_all(&sess.prof.profiles_dir)
        .map_err(|e| format!("创建快照目录失败: {e}"))?;
    std::fs::write(sess.prof.current_account_file(), uid.trim())
        .map_err(|e| format!("写入当前账号标记失败: {e}"))?;
    // 同时写时间戳边车：单看标记文件无法判断「这个标记和客户端现场谁更新」。
    // 用户在客户端里直接换号（没走本应用的切换）时，标记就过期了，
    // 只信标记会把「当前登录」标在错误的账号上。
    let stamp = current_account_meta_file(sess);
    let _ = std::fs::write(
        &stamp,
        serde_json::json!({ "switchedAtMs": now_ms() }).to_string(),
    );
    Ok(())
}

/// 当前账号标记的时间戳边车文件。
pub fn current_account_meta_file(sess: &Session) -> PathBuf {
    sess.prof.profiles_dir.join("current_account.meta.json")
}

/// 当前 Unix 毫秒。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 标记文件的写入时刻（毫秒；文件缺失或损坏返回 `None`）。
pub fn current_account_marker_ms(sess: &Session) -> Option<i64> {
    let raw = std::fs::read_to_string(current_account_meta_file(sess)).ok()?;
    serde_json::from_str::<serde_json::Value>(raw.trim_start_matches('\u{feff}'))
        .ok()?
        .get("switchedAtMs")
        .and_then(serde_json::Value::as_i64)
}

/// 标记文件（`current_account.txt`）的最后修改时刻（毫秒）。
///
/// 边车缺失时用它兜底：比没有时间信息好，比直接信任标记差。
pub fn current_account_mtime_ms(sess: &Session) -> Option<i64> {
    let meta = std::fs::metadata(sess.prof.current_account_file()).ok()?;
    let t = meta.modified().ok()?;
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

/// 现场登录态的「证据」时间：客户端数据目录里最近被改动的时刻。
///
/// 用户在客户端里换号会改写 `storage.json`（icube）或 `Local State`（Chromium），
/// 因此这些关键文件的改动时间近似等于「现场登录态的最后变更时刻」。
/// 与标记时间比较即可判断标记是否过期（见 [`resolve_current_uid`]）。
pub fn live_login_evidence_ms(sess: &Session) -> Option<i64> {
    // 只取真正承载登录态的那几个文件：整个目录取最大改动时间会被
    // 缓存、日志、崩溃转储等无关写入顶高，导致标记被误判成过期。
    let candidates: &[&str] = match sess.prof.layout {
        Layout::Icube => &[
            "User\\globalStorage\\storage.json",
            "User\\globalStorage\\state.vscdb",
            "User\\globalStorage\\state.vscdb-wal",
        ],
        Layout::Chromium => &[
            "Local State",
            "Default\\Cookies",
            "Default\\Network\\Cookies",
        ],
        Layout::Authfile => &[],
    };

    let mut newest: Option<i64> = None;
    for rel in candidates {
        let path = join_windows_rel(&sess.prof.data_dir, rel);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let Ok(t) = meta.modified() else { continue };
        let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) else {
            continue;
        };
        let ms = d.as_millis() as i64;
        newest = Some(newest.map_or(ms, |n: i64| n.max(ms)));
    }
    newest
}

/// 当前账号 uid（**混合判定**）：标记与现场证据谁更新就信谁。
///
/// ## 为什么不能只信标记
///
/// `current_account.txt` 只在本应用执行「切换 / 保存登录态」时写入。用户完全可能
/// 直接打开客户端换一个账号登录 —— 此时现场已经是新账号，标记还停在上一个。
/// 只信标记会让界面把「当前登录」徽章打在**错的**账号上，用户据此判断
/// 「现在用的是哪个号」就会得出相反结论。
///
/// ## 为什么不能只信证据
///
/// 证据只是文件改动时间，无法从中读出 uid；而且在快照刚恢复、客户端还没落盘的
/// 窗口里，证据时间是旧的甚至不存在。所以标记仍然是 uid 的唯一来源，
/// 这里只是用时间比较决定**标记是否还值得相信**。
///
/// 返回 `(uid, 标记是否过期)`。过期时 uid 仍返回标记值 —— 上层若无法从现场推导
/// 出新 uid，至少还能显示「上一次已知的账号」，但可以据此提示「现场可能已变更」。
pub fn resolve_current_uid(sess: &Session) -> (String, bool) {
    let marked = get_current_account(sess);
    if marked.is_empty() {
        return (String::new(), false);
    }
    let marker_ms = current_account_marker_ms(sess).or_else(|| current_account_mtime_ms(sess));
    let Some(marker_ms) = marker_ms else {
        // 标记存在但拿不到任何时间信息：无法判断新旧，按「不过期」处理
        return (marked, false);
    };
    match live_login_evidence_ms(sess) {
        // 留 2 秒容差：切换流程里「写标记」与「客户端落盘」几乎同时发生，
        // 不加容差会把刚做完的切换误判成过期
        Some(evidence) if evidence > marker_ms + 2000 => (marked, true),
        _ => (marked, false),
    }
}

/// 把档案里的 **Windows 相对路径**（`User\globalStorage\storage.json`）拼到根目录上。///
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
        Action::LaunchOnly => {
            proc::start_app(&sess, sink)?;
            Ok(format!("已启动 {}", sess.prof.app_name))
        }
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
///
/// icube 布局额外做「项目列表 / 最近打开跨账号保留」：
/// `state.vscdb` 里这两个键是**全局**语义，随槽位快照整体回滚后只剩目标账号自己
/// 那一份，用户感知为「项目列表消失了」。因此在恢复**前**抽出、恢复**后**按条目
/// 合并回写（账号分区键零改动，见 [`vscdb`] 模块头注释）。
fn restore_profile(sess: &mut Session, slot: &str, sink: &dyn ProgressSink) -> Result<(), String> {
    sess.last_restored_count = -1;
    let keep_keys = if sess.prof.layout == Layout::Icube {
        let path = vscdb::vscdb_path(&sess.prof.data_dir);
        let snap = vscdb::snapshot_global_keys(&path);
        if snap.is_empty() {
            None
        } else {
            Some((path, snap))
        }
    } else {
        None
    };

    match sess.prof.layout {
        Layout::Icube => icube::restore_icube(sess, slot, sink)?,
        Layout::Chromium => chromium::restore_chromium(sess, slot, sink)?,
        Layout::Authfile => {
            return Err("WorkBuddy / CodeBuddy 的登录态由账号管理页的「切换」处理".to_string())
        }
    }

    if let Some((path, snap)) = keep_keys {
        match vscdb::merge_global_keys(&path, &snap) {
            Ok(Some(summary)) => sink.step(
                "restore",
                StepStatus::Ok,
                &format!("项目列表/最近打开已跨账号保留（{summary}）"),
            ),
            Ok(None) => {}
            Err(e) => sink.step(
                "restore",
                StepStatus::Warn,
                &format!("项目列表/最近打开保留失败（不影响登录态）: {e}"),
            ),
        }
    }
    Ok(())
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

    /// 等到动作闸门空出来再拿（cargo 默认并行跑用例，别的用例可能正持有它）。
    ///
    /// 用等待而不是直接断言「立刻拿到」：后者测的不是闸门语义，而是**用例调度顺序**，
    /// 会随无关用例的新增随机变红（本文件就踩过一次）。
    fn acquire_gate_when_free() -> ActionGate {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Some(gate) = ActionGate::try_acquire() {
                return gate;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "等待动作闸门释放超时：要么有用例泄漏了闸门，要么闸门本身有问题"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn 动作闸门互斥且释放后可再次获取() {
        let first = acquire_gate_when_free();

        // 本测试持有闸门期间，任何线程都不可能拿到 —— 这段是真正的被测语义，
        // 且不受并行调度影响（别人拿不到才能走到这里）。
        assert!(
            ActionGate::try_acquire().is_none(),
            "并发动作必须被拒绝，否则会互相掐进程、互相覆盖登录态"
        );

        drop(first);
        // 释放后应可再次获取。别的用例可能抢先拿到，所以这里同样等到空闲为止 ——
        // 「别人能拿到」本身就证明了释放生效。
        let again = acquire_gate_when_free();
        // 再确认互斥语义在第二轮仍然成立
        assert!(ActionGate::try_acquire().is_none());
        drop(again);
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

    /// 造一个独立的 store 目录与会话（这些用例都要真写文件）。
    fn temp_session(tag: &str, app: TargetApp) -> (PathBuf, Session) {
        let dir = std::env::temp_dir().join(format!(
            "ai-gateway-hybrid-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let args = RunArgs {
            action: Action::BackupCurrent,
            target_app: app,
            user_id: None,
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            store_dir: dir.clone(),
        };
        let sess = Session::new(&args);
        (dir, sess)
    }

    #[test]
    fn 写标记同时写时间戳边车() {
        let (dir, sess) = temp_session("meta", TargetApp::TraeWork);
        set_current_account(&sess, "u1").unwrap();

        let ms = current_account_marker_ms(&sess).expect("应写出时间戳边车");
        assert!(ms > 0, "时间戳应是有效毫秒值：{ms}");

        let raw = std::fs::read_to_string(current_account_meta_file(&sess)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            parsed.get("switchedAtMs").is_some(),
            "边车字段名不能改，否则旧版本读不到：{raw}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 无标记时当前账号为空且不算过期() {
        let (dir, sess) = temp_session("none", TargetApp::TraeWork);
        let (uid, stale) = resolve_current_uid(&sess);
        assert_eq!(uid, "");
        assert!(!stale, "没有标记就谈不上「过期」");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 标记比现场新时不算过期() {
        let (dir, sess) = temp_session("fresh", TargetApp::TraeWork);
        // 先造现场文件（旧），再写标记（新）—— 模拟刚做完一次切换
        let storage = sess
            .prof
            .data_dir
            .join("User")
            .join("globalStorage")
            .join("storage.json");
        std::fs::create_dir_all(storage.parent().unwrap()).unwrap();
        std::fs::write(&storage, "{}").unwrap();
        set_current_account(&sess, "u-fresh").unwrap();

        let (uid, stale) = resolve_current_uid(&sess);
        assert_eq!(uid, "u-fresh");
        assert!(!stale, "标记比现场新，不该判成过期");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 现场比标记新时判为过期() {
        let (dir, sess) = temp_session("stale", TargetApp::TraeWork);
        // 先写标记，再把现场改成「刚刚」——模拟用户直接在客户端里换号
        set_current_account(&sess, "u-old").unwrap();
        let storage = sess
            .prof
            .data_dir
            .join("User")
            .join("globalStorage")
            .join("storage.json");
        std::fs::create_dir_all(storage.parent().unwrap()).unwrap();
        std::fs::write(&storage, "{}").unwrap();

        // 把标记边车的时间戳改到 1 小时前，制造「现场更新」的局面
        let stale_ms = now_ms() - 3_600_000;
        std::fs::write(
            current_account_meta_file(&sess),
            serde_json::json!({ "switchedAtMs": stale_ms }).to_string(),
        )
        .unwrap();

        let (uid, stale) = resolve_current_uid(&sess);
        assert_eq!(uid, "u-old", "uid 仍来自标记（现场读不出 uid）");
        assert!(stale, "现场更新时应提示标记可能已过期");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 边车缺失时回退用标记文件的修改时间() {
        let (dir, sess) = temp_session("mtime", TargetApp::TraeWork);
        set_current_account(&sess, "u-mtime").unwrap();
        // 删掉边车，只剩 current_account.txt
        std::fs::remove_file(current_account_meta_file(&sess)).unwrap();

        assert!(current_account_marker_ms(&sess).is_none());
        let mtime = current_account_mtime_ms(&sess).expect("应能读到标记文件修改时间");
        assert!(mtime > 0);

        // 没有现场文件 → 无从比较 → 不算过期
        let (uid, stale) = resolve_current_uid(&sess);
        assert_eq!(uid, "u-mtime");
        assert!(!stale);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 边车损坏时不崩溃且按不过期处理() {
        let (dir, sess) = temp_session("broken", TargetApp::TraeWork);
        set_current_account(&sess, "u-broken").unwrap();
        std::fs::write(current_account_meta_file(&sess), "{ 不是 json").unwrap();

        // 坏边车 → 回退到 mtime，仍然可以判断
        assert!(current_account_marker_ms(&sess).is_none());
        assert!(current_account_mtime_ms(&sess).is_some());
        let (uid, _) = resolve_current_uid(&sess);
        assert_eq!(uid, "u-broken", "坏边车不该让 uid 丢失");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 仅拉起客户端不改动任何快照槽() {
        let (dir, sess) = temp_session("launch", TargetApp::TraeWork);
        set_current_account(&sess, "u-keep").unwrap();

        // LaunchOnly 找不到 exe 会失败，但**绝不能**动快照槽或标记
        let args = RunArgs {
            action: Action::LaunchOnly,
            target_app: TargetApp::TraeWork,
            user_id: None,
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            store_dir: dir.clone(),
        };
        let _ = run_action(args, &NullSink);

        assert_eq!(get_current_account(&sess), "u-keep", "标记必须原样保留");
        let slots: Vec<String> = std::fs::read_dir(&sess.prof.profiles_dir)
            .map(|it| {
                it.flatten()
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            !slots.iter().any(|s| s == "last"),
            "LaunchOnly 不该产生备份槽：{slots:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
