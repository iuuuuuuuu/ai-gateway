//! 进程控制：枚举 / 优雅关闭 / 启动，按 [`AppProfile`] 参数化。
//!
//! 来源：TraeWorkAssistant `src-tauri/src/switcher/proc.rs`，改写为复用本仓库
//! 既有的 `process` 模块（tasklist / taskkill 封装），避免重复实现。
//!
//! 关闭是**三层**的：先 `WM_CLOSE`（让客户端有机会落盘 leveldb / cookie / SQLite
//! WAL）→ 等 `graceful_wait_secs` → 仍在则 `taskkill /T`（温和）→ 再等 5 秒 →
//! 仍在则 `taskkill /T /F`（强制）。
//!
//! 为什么必须优雅优先：强杀会留下文件锁与未 checkpoint 的 WAL。前者让快照静默缺文件，
//! 后者被客户端下次启动回放，把切换前账号的登录/使用证据写回新库 —— 表现为
//! 「切换后账号没变」。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{ProgressSink, Session, StepStatus};
use crate::modules::app_profile::AppProfile;
use crate::modules::process;

/// 进程表的一行（`pid` + 映像名）。
#[derive(Clone, Debug)]
pub struct ProcRow {
    pub pid: u32,
    pub name: String,
}

/// 去掉 `.exe` 后缀（大小写不敏感）。
///
/// 关键：`tasklist` / `sysinfo` 返回的映像名**带** `.exe`，而档案里的
/// `proc_names` 白名单**不带**。不归一化就永远匹配不上，客户端永远不会被关闭 ——
/// 这会让「切换」静默变成「什么都没发生」。
pub fn strip_exe_suffix(name: &str) -> &str {
    let name = name.trim();
    if name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(".exe") {
        &name[..name.len() - 4]
    } else {
        name
    }
}

/// 映像名匹配：两侧都先剥掉 `.exe` 再比大小写不敏感。
///
/// 抽成函数是为了让「匹配规则」只有一份实现 —— 单测直接调它，
/// 而不是在测试里抄一份等价逻辑（抄的那份改不动实现，等于没测）。
pub fn image_name_matches(want: &str, actual: &str) -> bool {
    strip_exe_suffix(actual).eq_ignore_ascii_case(strip_exe_suffix(want))
}

/// 枚举所有匹配该档案的进程（按 `proc_names` 精确匹配，大小写不敏感）。
///
/// 实现：**一次** Toolhelp 快照拿到全部进程，再在内存里按 `proc_names` 过滤。
/// 早期实现是「对 `proc_names` 里每个映像名各起一次 `tasklist`」，有两个问题：
///
/// 1. 慢：本机实测 `tasklist /FI ...` 单次 265–474 ms，TraeWork 档案有 3 个
///    映像名 ⇒ 每次 `is_running` 白付 ~0.8 s；`stop_app` 一轮里要调 4 次。
/// 2. **恒空**：`tasklist` 的 `IMAGENAME` 过滤器是精确匹配，而 `proc_names`
///    刻意不带 `.exe`（见 `strip_exe_suffix` 的注释），于是 `IMAGENAME eq Trae CN`
///    永远匹配不到 `Trae CN.exe` ⇒ `list_procs` 恒空 ⇒ `is_running` 恒 false ⇒
///    `stop_app` 直接早退「未在运行，无需关闭」⇒ 旧实例没被关掉、新实例被
///    Electron 单实例拦下 ⇒ 切换静默变成「什么都没发生」。
///
/// 现在匹配统一走 `strip_exe_suffix`，带不带 `.exe` 都能命中。
///
/// 非 Windows 平台返回空：进程枚举走的是 Windows 的 `tasklist`，
/// 而本切换器只服务 Windows 桌面端（Trae / 豆包客户端的登录态布局也是
/// Windows 路径）。返回空而不是编译失败，是为了让 workspace 能在
/// Linux / macOS 上完成 `cargo check` 与单测（CI 三平台都会编译本 crate）。
#[cfg(target_os = "windows")]
pub fn list_procs(prof: &AppProfile) -> Vec<ProcRow> {
    let mut out = Vec::new();
    if let Some(rows) = process::windows_process_rows_snapshot() {
        for row in rows {
            // 两侧都归一化：档案里现在不带后缀，但将来若有人写成
            // 「Trae CN.exe」也必须能命中（见单测「映像名匹配容忍_exe_后缀差异」）。
            if prof
                .proc_names
                .iter()
                .any(|want| image_name_matches(want, &row.name))
            {
                out.push(ProcRow {
                    pid: row.pid,
                    name: row.name.clone(),
                });
            }
        }
    } else {
        // 快照不可用（句柄耗尽等）才退回逐映像名 tasklist。
        for image in prof.proc_names {
            for row in process::windows_tasklist_image_rows(image) {
                out.push(ProcRow {
                    pid: row.pid,
                    name: row.name.clone(),
                });
            }
        }
    }
    // 同一 pid 可能因多个映像名被枚举两次（如 CodeBuddy 与 CodeBuddy CN 同 pid 不会
    // 发生，但通配与精确混用时可能），去重更稳。
    out.sort_by_key(|r| r.pid);
    out.dedup_by_key(|r| r.pid);
    out
}

#[cfg(not(target_os = "windows"))]
pub fn list_procs(prof: &AppProfile) -> Vec<ProcRow> {
    let _ = prof;
    Vec::new()
}

/// 应用是否正在运行。
pub fn is_running(sess: &Session) -> bool {
    !list_procs(&sess.prof).is_empty()
}

/// 取正在运行的客户端 exe 路径（exe 发现的第 5 级回退）。
///
/// 快照行里本来就能带上 exe 全路径（`WindowsProcessRow::exe_path`），有就直接用，
/// 省掉一次「按 pid 查路径」的往返；快照没带才回退 `windows_process_exe_path`。
pub fn running_exe_of(prof: &AppProfile) -> Option<PathBuf> {
    let rows = list_procs(prof);
    let first = rows.first()?;
    process::windows_process_exe_path(first.pid)
}

/// 测试期是否禁止真实启动/关闭外部客户端。
///
/// 2026-09-30 实测事故：单测 `仅拉起客户端不改动任何快照槽` 断言的是「找不到 exe
/// 会失败」，但本机**确实装了** TRAE SOLO CN，6 级回退链第 5 级（正在运行的进程）
/// 命中，于是 `cargo test` 真的把用户正在用的 IDE 拉了起来（Electron 13 个进程，
/// 测试日志里混进 250 行 `Main->SharedProcess#connect`）。
///
/// 测试机器上装没装客户端、装在哪，是**环境事实**，不该决定测试是否安全。只要某个
/// 测试（现在或将来）走了启动/关闭路径，就会在开发者机器上产生真实副作用：拉起
/// 用户正在用的 IDE，或者更糟 —— 把没存盘的编辑器关掉。因此测试构建里一律拦截，
/// 宁可让测试拿到 `Err`，也不许碰用户的真实进程。
///
/// 用运行时分支而不是 `#[cfg(test)] return`：后者会让函数体后半段变成
/// unreachable 并触发 `unreachable_code` 警告。
#[cfg(test)]
fn real_app_control_blocked() -> bool {
    true
}

#[cfg(not(test))]
fn real_app_control_blocked() -> bool {
    false
}

/// 优雅关闭客户端。
pub fn stop_app(sess: &Session, sink: &dyn ProgressSink) -> Result<(), String> {
    let prof = &sess.prof;
    let rows = list_procs(prof);
    if rows.is_empty() {
        sink.step(
            "stop",
            StepStatus::Skip,
            &format!("{} 未在运行，无需关闭", prof.app_name),
        );
        return Ok(());
    }
    // 「没在运行」的早退保留在上方：那条路径零副作用，测试可以照常断言。
    // 真正会动进程的路径在这里拦住。
    if real_app_control_blocked() {
        return Err(format!(
            "测试环境禁止关闭 {}（已拦截真实进程操作）",
            prof.app_name
        ));
    }
    let pids: Vec<u32> = rows.iter().map(|r| r.pid).collect();
    sink.step(
        "stop",
        StepStatus::Info,
        &format!(
            "正在关闭 {}（{} 个进程），等待落盘…",
            prof.app_name,
            pids.len()
        ),
    );

    // 第一层：温和关闭（WM_CLOSE / 无 /F 的 taskkill），给客户端落盘机会。
    for pid in &pids {
        process::request_graceful_close(*pid);
    }
    let deadline = Instant::now() + Duration::from_secs(prof.graceful_wait_secs);
    while Instant::now() < deadline {
        if list_procs(prof).is_empty() {
            sink.step(
                "stop",
                StepStatus::Ok,
                &format!("{} 已优雅退出", prof.app_name),
            );
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    // 第二层：温和 taskkill（不带 /F），再等 5 秒。
    let remaining: Vec<u32> = list_procs(prof).iter().map(|r| r.pid).collect();
    sink.step(
        "stop",
        StepStatus::Warn,
        &format!(
            "{} 在 {} 秒内未退出，改为温和终止 {} 个进程",
            prof.app_name,
            prof.graceful_wait_secs,
            remaining.len()
        ),
    );
    for pid in &remaining {
        process::taskkill_pid(*pid, false);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if list_procs(prof).is_empty() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    // 第三层：强制终止（此时 WAL 风险已无法避免，只能保证不残留进程）。
    let stubborn: Vec<u32> = list_procs(prof).iter().map(|r| r.pid).collect();
    if stubborn.is_empty() {
        return Ok(());
    }
    sink.step(
        "stop",
        StepStatus::Warn,
        &format!("强制终止 {} 个残留进程", stubborn.len()),
    );
    for pid in &stubborn {
        process::taskkill_pid(*pid, true);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if list_procs(prof).is_empty() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(format!("无法关闭 {}，请手动退出后重试", prof.app_name))
}

/// 启动客户端；`sess.launch_proxy_port > 0` 时注入 `--proxy-server`。
///
/// 注意：单实例客户端（Electron 系）**忽略**新实例的命令行参数 —— 必须确保
/// 旧实例已退出，否则 `--proxy-server` 不会生效（表现为「一键以账号打开」没走代理）。
///
/// `sess.hide_window` 为真时，spawn 之后把该进程的顶层窗口藏起来（见
/// [`hide_started_window`]）。
pub fn start_app(sess: &Session, sink: &dyn ProgressSink) -> Result<(), String> {
    let prof = &sess.prof;
    if is_running(sess) {
        sink.step(
            "start",
            StepStatus::Warn,
            &format!("{} 已在运行，跳过启动（单实例客户端会忽略新的命令行参数）", prof.app_name),
        );
        return Ok(());
    }
    let exe = super::locate::find_exe(sess)?;
    let mut cmd = process::cmd_builder(&exe);
    if let Some(port) = sess.launch_proxy_port {
        cmd.arg(format!("--proxy-server=http://127.0.0.1:{port}"));
        sink.step(
            "start",
            StepStatus::Info,
            &format!("正在启动 {}（经本地代理 127.0.0.1:{port}）…", prof.app_name),
        );
    } else {
        sink.step(
            "start",
            StepStatus::Info,
            &format!("正在启动 {}…", prof.app_name),
        );
    }
    // 拦在**真正 spawn 之前**，而不是函数开头：上面那段 exe 发现链（候选路径、
    // .lnk、注册表、进程、缓存）全是只读的，测试依赖它产生「未找到 exe」这类
    // 正常错误。只有这一行才会改变机器状态，所以只有它需要拦。
    if real_app_control_blocked() {
        return Err(format!(
            "测试环境禁止启动 {}（已拦截真实进程操作，发现路径：{}）",
            prof.app_name,
            exe.display()
        ));
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("启动 {} 失败（{}）: {e}", prof.app_name, exe.display()))?;
    let pid = child.id();
    // 子进程句柄交还给系统：客户端要活到本次动作之外，这里不能持有它。
    drop(child);
    if sess.hide_window {
        hide_started_window(pid, prof, sink);
    }
    sink.step("start", StepStatus::Ok, &format!("{} 已启动", prof.app_name));
    Ok(())
}

/// 隐藏刚启动客户端的窗口，并等到「藏干净了」为止。
///
/// 为什么要**等**、不能 spawn 完就走：窗口是客户端进程启动之后才创建的
/// （Chromium 系先起 browser 进程，再建主窗口），spawn 那一刻这个 pid
/// 一个顶层窗口都还没有。只能轮询。
///
/// 为什么要等「**没有可见窗口**」而不是「隐藏数 > 0」：客户端建窗口时会先
/// 显示再调整（实测会短暂出现一个可见窗口），只等第一个可见窗口会在它之后
/// 又冒出第二个可见窗口时漏掉 —— 表现为后台任务把客户端窗口弹到用户脸上。
fn hide_started_window(pid: u32, prof: &AppProfile, sink: &dyn ProgressSink) {
    const DEADLINE: Duration = Duration::from_secs(20);
    const STEP: Duration = Duration::from_millis(120);
    // 窗口出现的宽限期：到点还没有任何顶层窗口就不再等（客户端可能压根没界面，
    // 或已在别处失败）—— 白等 20 秒只会拖长保活动作。
    const APPEAR_GRACE: Duration = Duration::from_secs(8);

    let started = Instant::now();
    let mut hidden = 0usize;
    // 判据是「出现过**可见**窗口」，不是「出现过任何顶层窗口」。
    //
    // ★ 2026-10-06 真机实测踩到的坑：Chromium / GDI 系客户端在创建主窗口**之前**
    // 就会挂出一批不可见的顶层辅助窗口（`Default IME`、`MSCTFIME UI`、
    // `GDI+ Window`）。用「有顶层窗口就算出现过」的话，第一次轮询（spawn 后 ~50ms）
    // 就命中这些辅助窗口、又看不到可见窗口，于是立刻判定「已经藏干净了」并退出 ——
    // 而真正的主窗口 200ms 后才显示出来，全程无人隐藏。实测日志：窗口从 t=0.27s
    // 一直 `vis=True` 到保活结束，且因为 hidden=0 连一条步骤都没留。
    let mut seen_visible = false;
    loop {
        if process::has_visible_window(pid) {
            seen_visible = true;
        }
        hidden += process::hide_process_windows(pid);
        // 出现过可见窗口、现在又没有可见窗口 ⇒ 藏干净了。
        if seen_visible && !process::has_visible_window(pid) {
            break;
        }
        if started.elapsed() >= DEADLINE {
            break;
        }
        // 一直没出现过可见窗口：可能客户端本来就无界面，别白等满超时。
        if !seen_visible && started.elapsed() >= APPEAR_GRACE {
            break;
        }
        std::thread::sleep(STEP);
    }

    if hidden > 0 {
        sink.step(
            "start",
            StepStatus::Info,
            &format!("已在后台隐藏 {} 的 {} 个窗口", prof.app_name, hidden),
        );
    } else if !seen_visible {
        // 不算失败：客户端可能还在建窗口，或本来就无界面。留一条 info 便于排查
        // 「后台任务把窗口弹出来了」这类反馈。
        sink.step(
            "start",
            StepStatus::Info,
            &format!("{} 未出现可见窗口，无需隐藏", prof.app_name),
        );
    }
}

/// 路径是否指向该档案的 exe（大小写不敏感，用于防串台）。
pub fn path_is_app_exe(path: &Path, prof: &AppProfile) -> bool {
    crate::modules::app_profile::exe_matches(path, prof)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{Action, RunArgs};
    use crate::modules::app_profile::{profile_for, TargetApp};
    use crate::modules::config;

    fn prof() -> AppProfile {
        profile_for(TargetApp::TraeWork, Path::new("C:\\store"))
    }

    #[test]
    fn 剥离_exe_后缀大小写不敏感() {
        assert_eq!(strip_exe_suffix("Trae.exe"), "Trae");
        assert_eq!(strip_exe_suffix("Trae.EXE"), "Trae");
        assert_eq!(strip_exe_suffix("TRAE SOLO CN.exe"), "TRAE SOLO CN");
        // 不带后缀的原样返回
        assert_eq!(strip_exe_suffix("Trae"), "Trae");
        // 只有 ".exe" 这 4 个字符本身时不剥离（避免切出空串）
        assert_eq!(strip_exe_suffix(".exe"), ".exe");
        assert_eq!(strip_exe_suffix(""), "");
    }

    #[test]
    fn 进程名白名单不带_exe_因此必须归一化() {
        // 这是「切换静默失效」的根因回归测试：档案里存的是不带后缀的名字，
        // 而 tasklist 返回带后缀的名字。
        let p = prof();
        assert!(p.proc_names.iter().all(|n| !n.ends_with(".exe")));
        assert_eq!(strip_exe_suffix("TRAE SOLO CN.exe"), p.proc_names[0]);
    }

    #[test]
    fn 路径白名单防串台() {
        let p = prof();
        assert!(path_is_app_exe(Path::new("C:\\x\\Trae.exe"), &p));
        assert!(!path_is_app_exe(Path::new("C:\\x\\Trae CN.exe"), &p));
        assert!(!path_is_app_exe(Path::new("C:\\x\\Doubao.exe"), &p));
    }

    /// 映像名匹配必须容忍 `.exe` 的三种组合。
    ///
    /// 回归背景：早期实现把档案里**不带** `.exe` 的 `proc_names` 直接塞给
    /// `tasklist /FI "IMAGENAME eq X"`，而该过滤器是精确匹配 —— 永远匹配不到
    /// `X.exe`，于是 `list_procs` 恒空、「关闭旧实例」静默退化成什么都没做。
    /// 现在匹配在内存里做，两种写法都必须命中。
    #[test]
    fn 映像名匹配容忍_exe_后缀差异() {
        // 直接调实现，不在测试里另抄一份等价逻辑。
        let hits = image_name_matches;
        // 档案不带后缀，进程带后缀（真实情况）
        assert!(hits("TRAE SOLO CN", "TRAE SOLO CN.exe"));
        assert!(hits("TRAE SOLO CN", "trae solo cn.EXE"));
        // 两边都不带 / 两边都带
        assert!(hits("TRAE SOLO CN", "TRAE SOLO CN"));
        assert!(hits("TRAE SOLO CN.exe", "TRAE SOLO CN.exe"));
        // 前后空白（tasklist CSV 偶发带空格）
        assert!(hits("TRAE SOLO CN", " TRAE SOLO CN.exe "));
        // 不相关进程不能误命中
        assert!(!hits("TRAE SOLO CN", "TRAE SOLO.exe"));
        assert!(!hits("Trae", "Trae CN.exe"));
    }

    /// 需求 C：窗口隐藏开关必须只对「无人值守」的动作生效。
    ///
    /// 判据集中在 [`Action::hides_client_window`]，而 [`Session::new`] 在它之上
    /// 再与用户设置求与。两条都不能退化：
    ///   * 若 KeepAlive 不再隐藏 → 计划任务到点仍会把客户端窗口弹到用户脸上（原始缺陷）；
    ///   * 若 LaunchOnly / Switch 也开始隐藏 → 用户亲手点的「打开客户端」会看不见窗口。
    #[test]
    fn 只有保活动作会隐藏客户端窗口() {
        assert!(Action::KeepAlive.hides_client_window());
        for action in [
            Action::Switch,
            Action::SaveCurrentLogin,
            Action::BackupCurrent,
            Action::RestoreOnly,
            Action::ResetDeviceIds,
            Action::LaunchOnly,
        ] {
            assert!(
                !action.hides_client_window(),
                "{} 是用户亲手触发的动作，不该隐藏窗口",
                action.as_str()
            );
        }
    }

    /// 需求 C：设置页的开关要真的改变保活会话的隐藏行为。
    ///
    /// 用 `Isolated` 换掉 AI_GATEWAY_HOME，避免读写用户真实的 app_settings.json；
    /// 键值本身用 `config::set_app_setting` 走正式写盘路径，不是直接造 JSON。
    #[test]
    fn 隐藏开关决定保活会话是否隐藏窗口() {
        let _iso = config::test_isolation::Isolated::new("hide-window-setting");
        let args = RunArgs {
            action: Action::KeepAlive,
            target_app: TargetApp::Doubao,
            user_id: None,
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            store_dir: config::store_dir(),
        };

        // 未设置过 → 取默认值（默认隐藏，否则后台保活会把窗口弹到桌面）
        assert!(
            config::CLIENT_HIDE_WINDOW_DEFAULT,
            "默认值必须是隐藏：后台保活弹窗是原始缺陷"
        );
        assert!(Session::new(&args).hide_window, "未设置过时应取默认值（隐藏）");

        // 用户在设置页关掉开关
        config::set_app_setting(config::CLIENT_HIDE_WINDOW_KEY, serde_json::json!(false)).unwrap();
        assert!(
            !Session::new(&args).hide_window,
            "关掉开关后保活不得再隐藏窗口"
        );

        // 打开回来
        config::set_app_setting(config::CLIENT_HIDE_WINDOW_KEY, serde_json::json!(true)).unwrap();
        assert!(Session::new(&args).hide_window);

        // 非保活动作永远不隐藏，哪怕开关是开的
        let launch = RunArgs {
            action: Action::LaunchOnly,
            target_app: TargetApp::Doubao,
            user_id: None,
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            store_dir: config::store_dir(),
        };
        assert!(
            !Session::new(&launch).hide_window,
            "「打开客户端」是用户亲手点的，窗口必须可见"
        );
    }

    /// 需求 C 回归守卫：隐藏循环不能再用「有顶层窗口」当出现过窗口的判据。
    ///
    /// 真机实测（2026-10-06）证明这是**唯一**会让隐藏完全失效的写法：客户端在
    /// 主窗口之前先挂出 `Default IME` / `MSCTFIME UI` / `GDI+ Window` 这批不可见
    /// 顶层窗口，循环第一轮就误判「已经藏干净」并退出，主窗口随后照常弹出来。
    /// 当时的表现是：窗口全程 `vis=True`，而日志里连「已隐藏」都没留一条。
    ///
    /// 这类「代码看着对、跑起来完全没生效」的缺陷单测抓不到（真实窗口只有真机有），
    /// 所以这里退回源码锚点：钉住 `hide_started_window` 用的是 `has_visible_window`。
    #[test]
    fn 隐藏循环必须以可见窗口为判据() {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/modules/switcher/proc.rs"
        ))
        .expect("应能读到本文件源码");
        let src = src.replace("\r\n", "\n");
        let head = src
            .find("fn hide_started_window(")
            .expect("hide_started_window 必须存在");
        let body = &src[head..];
        let end = body.find("\n}\n").expect("函数体应当闭合");
        let body = &body[..end];

        assert!(
            body.contains("has_visible_window"),
            "隐藏循环必须用 has_visible_window 判断窗口是否已藏干净"
        );
        assert!(
            !body.contains("has_top_level_window"),
            "不要用 has_top_level_window 当「出现过窗口」的判据：不可见的辅助窗口会让循环立刻退出"
        );
    }

    /// `proc_names` 里的每一项都必须能匹配到自己的「带后缀」形态。
    ///
    /// 这是上面那条的档案级版本：以后有人往档案里加映像名却写错拼写，
    /// 这里会立刻炸，而不是等到用户报「点了切换什么都没发生」。
    #[test]
    fn 档案白名单每一项都能匹配带后缀的映像名() {
        use crate::modules::app_profile::TargetApp;
        for app in [TargetApp::Trae, TargetApp::TraeWork, TargetApp::Doubao, TargetApp::WorkBuddy, TargetApp::CodeBuddy] {
            let p = profile_for(app, Path::new("C:\\store"));
            for name in p.proc_names {
                assert!(!name.ends_with(".exe"), "proc_names 不该带后缀: {name}");
                assert!(
                    image_name_matches(name, &format!("{name}.exe")),
                    "{} 的 proc_names 项 {name} 无法匹配自己的映像名",
                    p.app_name
                );
            }
        }
    }
}
