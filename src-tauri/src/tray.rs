//! Desktop tray: menu-bar icon, dock visibility, lightweight mode, and check-in.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tauri::menu::{CheckMenuItem, Menu, MenuBuilder, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{
    AppHandle, Emitter, Manager, RunEvent, Runtime, WebviewWindowBuilder, Window, WindowEvent,
};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;
use ai_gateway_core::modules::{checkin, gateway, update};

const TRAY_ID: &str = "main-menu-bar";
const MAIN_WINDOW_LABEL: &str = "main";
/// 托盘悬停提示：用产品显示名而不是可执行文件名，用户看到的是品牌名。
const DEFAULT_TOOLTIP: &str = "AI Gateway";
const CHECKIN_TOOLTIP_RESTORE_SECS: u64 = 8;
/// 等待前端首帧上报的最长时间；超时后无条件显示主窗口（见
/// [`setup_startup_visibility`] 的兜底分支）。
///
/// 取 10 s 的依据：实测冷启动前端首帧约 1 s（`uitest/run-coldstart.ps1`），
/// 慢机器 + 大账号库最坏也就几秒；10 s 足够覆盖，又不至于让异常场景
/// （脚本报错、资源缺失）把窗口藏到用户以为程序没起来。
const FRONTEND_READY_TIMEOUT: Duration = Duration::from_secs(60);

/// 系统自启注册的启动参数：仅携带该精确参数的启动进入静默托盘模式。
pub const SILENT_STARTUP_ARG: &str = "--hidden";

static LIGHTWEIGHT_MODE: AtomicBool = AtomicBool::new(false);
/// 本次启动是否为静默（`--hidden`，即系统自启）。
///
/// # 为什么需要这个标志（2026-10-05 实测回归）
///
/// [`mark_frontend_ready`] 是「前端首帧已画完，窗口可以显示了」的信号，
/// 但它对**静默启动**不成立 —— 自启动的全部意义就是不弹窗。
/// 旧版 silent 分支不调用任何 show，所以没这个问题；⑩b 把
/// 「前端就绪 ⇒ 显示窗口」接进 [`mark_frontend_ready`] 之后，
/// 静默启动也会在首帧画完时把窗口弹出来（iso 实例实测：`--hidden` 下
/// t≈0.9 s 主窗口 `vis=False → True`）。故此处显式记住本次启动是否静默。
static STARTUP_SILENT: AtomicBool = AtomicBool::new(false);
static CHECKIN_BUSY: AtomicBool = AtomicBool::new(false);
/// 前端首帧是否已上报（见 [`mark_frontend_ready`]）。
static FRONTEND_READY: AtomicBool = AtomicBool::new(false);
static TOOLTIP_GENERATION: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "macos")]
static DOCK_ICON_GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn setup(app: &mut tauri::App) -> tauri::Result<()> {
    let menu = build_tray_menu(app)?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(menu_bar_icon())
        .icon_as_template(true)
        .tooltip(DEFAULT_TOOLTIP)
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open-main-window" => show_main_window(app),
            "open-github" => open_github(app),
            "checkin-all" => start_checkin_all(app),
            "lightweight-mode" => toggle_lightweight(app),
            "quit-app" => app.exit(0),
            _ => {}
        })
        .build(app)?;

    Ok(())
}

pub fn on_window_event<R: Runtime>(window: &Window<R>, event: &WindowEvent) {
    if window.label() != MAIN_WINDOW_LABEL {
        return;
    }
    if let WindowEvent::CloseRequested { api, .. } = event {
        api.prevent_close();
        let _ = window.hide();
        apply_dock_visible(window.app_handle(), false);
        emit_main_window_visible(window.app_handle(), false);
    }
}

/// 判断本次启动是否携带精确的 `--hidden` 参数（系统自启触发）。
///
/// 必须整参相等，禁止子串匹配，避免 `--hidden-x`、`x--hidden` 等误入静默模式。
pub fn is_silent_startup(args: impl IntoIterator<Item = impl AsRef<str>>) -> bool {
    args.into_iter()
        .any(|arg| arg.as_ref() == SILENT_STARTUP_ARG)
}

/// 在事件循环呈现应用前决定首次启动的主窗口可见性。
///
/// `main` 窗口由 `tauri.conf.json` 配置创建为不可见，此处做出第一次
/// show / hide 决策，静默启动不会先闪出主窗口：
///
/// - 普通启动（无 `--hidden`）：**只恢复 Dock / 任务栏入口，窗口先不 show**，
///   等前端首帧上报（[`mark_frontend_ready`]）再显示。
/// - 静默启动（`--hidden`）：窗口保持隐藏，隐藏 Dock / 任务栏入口，
///   只保留托盘；不设置 `LIGHTWEIGHT_MODE`（WebView 仍然存在）。
///
/// 之后从托盘「打开主界面」仍走 `show_main_window`，与隐藏窗口完全一致。
///
/// # 为什么普通启动不在这里 show（2026-10-05）
///
/// 所有者反馈：「每次启动都要响应好长时间，如果直接点击就会无响应」。
/// 根因不是 Rust 主线程（实测 `Tauri Window` / `WRY_WEBVIEW` 全程不挂起），
/// 而是 **WebView2 浏览器进程**：窗口一 show，桌面上就出现一个真实窗口，
/// 此时渲染进程还在建环境、下载 SDK、编译 1.5MB 的 JS 包，
/// 用户点上去 → Windows 判定消息队列无响应 → 标题栏出现「未响应」灰屏，
/// 而这段时间的长短完全由机器与数据量决定（业主机器上曾到分钟级）。
///
/// 窗口晚 1 秒出现，比出现后卡 1 分钟体验好得多；启动过程本身不受影响
/// （后台循环、网关自动启动都在 setup 里照常跑，与窗口可见性无关）。
pub fn setup_startup_visibility<R: Runtime>(app: &AppHandle<R>, silent: bool) {
    // 启动时**清零**轻量模式（2026-10-06 加固，对齐上游 0.8.13）。
    //
    // 上游那条缺陷是「重启后标记残留 ⇒ 白屏」：他们的轻量模式标记会跨进程
    // 存活。本仓库的 `LIGHTWEIGHT_MODE` 是纯进程内 `AtomicBool`（不落盘），
    // 结构上不可能残留 —— 但**同一进程内**依然可能：先进入轻量模式、
    // 再从托盘以外的地方（例如二次启动）走到这里，标记若仍是 true，
    // `show_main_window` 会立刻又 `exit_lightweight` 一次，窗口闪一下又没。
    // 显式清零是一行成本，换来「这个入口永远从『正常模式』开始」。
    LIGHTWEIGHT_MODE.store(false, Ordering::Release);
    STARTUP_SILENT.store(silent, Ordering::Release);
    eprintln!("[tray] 启动可见性 silent={silent} t={}ms", crate::elapsed_ms());
    if silent {
        apply_dock_visible(app, false);
        emit_main_window_visible(app, false);
        return;
    }
    // 先把 Dock / 任务栏入口恢复，让用户在「窗口还没画出来」时也有可点的图标。
    apply_dock_visible(app, true);
    emit_main_window_visible(app, false);
    // 兜底：前端若因异常没上报（脚本报错、资源缺失、WebView2 初始化失败），
    // 不能把窗口永久藏住 —— 超时后无条件显示，行为退回到旧版。
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FRONTEND_READY_TIMEOUT).await;
        if !FRONTEND_READY.load(Ordering::Acquire) {
            show_main_window(&handle);
        }
    });
}

fn emit_main_window_visible<R: Runtime>(app: &AppHandle<R>, visible: bool) {
    let _ = app.emit("main-window-visible", visible);
}

/// 前端首帧已绘制完成（`main.tsx` 在双 `requestAnimationFrame` 后 invoke）。
///
/// 这是**唯一**的「可以显示主窗口了」信号：它由真正画完界面的那一侧发出，
/// 所以主窗口不会在 WebView2 还在建环境时暴露给用户点击。
pub fn mark_frontend_ready<R: Runtime>(app: &AppHandle<R>) {
    // 启动耗时可观测：这条与下面的「显示主窗口」之差，就是 ⑩b 闸门真正推迟的时间。
    eprintln!(
        "[tray] 前端首帧已就绪 t={}ms 窗口可见={:?} 静默={}",
        crate::elapsed_ms(),
        app.get_webview_window(MAIN_WINDOW_LABEL)
            .and_then(|w| w.is_visible().ok()),
        STARTUP_SILENT.load(Ordering::Acquire)
    );
    FRONTEND_READY.store(true, Ordering::Release);
    // 静默启动（`--hidden`）不在这里显示窗口：自启动的语义就是「只进托盘」。
    // 用户此时若想打开，走托盘菜单或再启动一次（两者都不经这里）。
    if STARTUP_SILENT.load(Ordering::Acquire) {
        return;
    }
    // 用 request_ 而不是 show_：页面重载（热更新、轻量模式恢复后的重建窗口）
    // 会再次上报，此时窗口往往已经可见，不该被 set_focus 抢走前台焦点。
    request_main_window_show(app);
}

/// 前端首帧是否已上报。供启动期的「后来者」排队等待（见
/// `captcha_webview::warmup_prefetch`：它要等主窗口画完再建第二个 WebView2）。
pub fn frontend_ready_flag() -> bool {
    FRONTEND_READY.load(Ordering::Acquire)
}

/// 需要主窗口可见时统一走这里（托盘点击、第二个实例启动、前端就绪）。
///
/// 与 [`show_main_window`] 的区别：**已经可见时什么都不做**。
/// 前端每次加载都会上报一次 `frontend_ready`，若每次都 `show()` +
/// `unminimize()` + `set_focus()`，用户最小化窗口后切回页面就会被强行拉回
/// 前台（页面重载、热更新、切标签页重挂载都会触发）。
pub fn request_main_window_show<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        if window.is_visible().unwrap_or(false) {
            return;
        }
    }
    show_main_window(app);
}

/// Keep the tray process alive when the last window is destroyed (lightweight mode).
///
/// `code == None` is Tauri's runtime exit after zero windows remain.
/// `code == Some(_)` is an explicit `app.exit()` / restart — let those through.
///
/// `RunEvent::Exit` 时确定性回收网关子进程：正常情况下由 `stop_gateway()` 收尾，
/// 异常结束（崩溃/强杀）则由 core 层的 Job Object 兜底。
pub fn on_run_event(event: RunEvent) {
    match event {
        RunEvent::ExitRequested { api, code, .. } => {
            if should_keep_tray_alive(code) {
                api.prevent_exit();
            }
        }
        RunEvent::Exit => {
            let stopped = gateway::stop_gateway();
            if stopped.get("stopped").and_then(Value::as_bool) == Some(true) {
                gateway::update_runtime_state("stopped", None);
            }
        }
        _ => {}
    }
}

fn should_keep_tray_alive(code: Option<i32>) -> bool {
    code.is_none()
}

fn show_main_window<R: Runtime>(app: &AppHandle<R>) {
    eprintln!("[tray] 显示主窗口 t={}ms", crate::elapsed_ms());
    // Recreate if the WebView is gone even when the flag is already false
    // (e.g. destroy() completed after a failed lightweight toggle).
    if LIGHTWEIGHT_MODE.load(Ordering::Acquire)
        || app.get_webview_window(MAIN_WINDOW_LABEL).is_none()
    {
        exit_lightweight(app);
        return;
    }
    apply_dock_visible(app, true);
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
    emit_main_window_visible(app, true);
}

#[cfg_attr(
    not(any(target_os = "macos", target_os = "windows")),
    allow(unused_variables)
)]
fn apply_dock_visible<R: Runtime>(app: &AppHandle<R>, visible: bool) {
    #[cfg(target_os = "macos")]
    {
        use tauri::ActivationPolicy;
        let policy = if visible {
            ActivationPolicy::Regular
        } else {
            ActivationPolicy::Accessory
        };
        let _ = app.set_dock_visibility(visible);
        let _ = app.set_activation_policy(policy);
        if visible {
            restore_macos_dock_icon(app);
        } else {
            DOCK_ICON_GENERATION.fetch_add(1, Ordering::AcqRel);
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
            let _ = window.set_skip_taskbar(!visible);
        }
    }
}

/// Re-apply the Dock icon after returning to Regular.
///
/// `TransformProcessType` rebuilds the Dock tile from the running executable.
/// Packaged `.app` binaries have no icon of their own, and `tauri dev` is named
/// `exec`, so both need an explicit restore. Tauri only sets
/// `setApplicationIconImage` once on `RunEvent::Ready` (dev only).
/// `TransformProcessType` is asynchronous, so we also re-apply after a short delay.
///
/// Do **not** feed the raw `icon.png` here: it is a full-bleed opaque square.
/// `setApplicationIconImage` then bypasses the system squircle, which is why the
/// Dock icon lost rounded corners and changed size after “打开主窗口”.
#[cfg(target_os = "macos")]
fn restore_macos_dock_icon<R: Runtime>(app: &AppHandle<R>) {
    let generation = DOCK_ICON_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    apply_macos_app_icon();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if DOCK_ICON_GENERATION.load(Ordering::Acquire) != generation {
            return;
        }
        let _ = app.run_on_main_thread(move || {
            if DOCK_ICON_GENERATION.load(Ordering::Acquire) == generation {
                apply_macos_app_icon();
            }
        });
    });
}

#[cfg(target_os = "macos")]
fn apply_macos_app_icon() {
    use objc2::AllocAnyThread;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    // SAFETY: tray / window events and run_on_main_thread all run on the AppKit main thread.
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let app = NSApplication::sharedApplication(mtm);

    if let Some(icon) = macos_bundle_dock_icon() {
        unsafe { app.setApplicationIconImage(Some(&icon)) };
        return;
    }

    // `tauri dev` has no `.app` bundle; keep the PNG fallback so Dock is not "exec".
    const APP_ICON_PNG: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/icons/icon.png"));
    let data = NSData::with_bytes(APP_ICON_PNG);
    let Some(icon) = NSImage::initWithData(NSImage::alloc(), &data) else {
        return;
    };
    unsafe { app.setApplicationIconImage(Some(&icon)) };
}

/// Finder-composited app icon (system squircle already applied).
#[cfg(target_os = "macos")]
fn macos_bundle_dock_icon() -> Option<objc2::rc::Retained<objc2_app_kit::NSImage>> {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::NSString;

    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let bundle = app_bundle_path_from_exe(&exe)?;
    let path = NSString::from_str(&bundle.to_string_lossy());
    Some(NSWorkspace::sharedWorkspace().iconForFile(&path))
}

/// `Foo.app/Contents/MacOS/binary` → `Foo.app`.
#[cfg(any(target_os = "macos", test))]
fn app_bundle_path_from_exe(exe: &std::path::Path) -> Option<&std::path::Path> {
    let macos_dir = exe.parent()?;
    if macos_dir.file_name()?.to_str()? != "MacOS" {
        return None;
    }
    let contents = macos_dir.parent()?;
    if contents.file_name()?.to_str()? != "Contents" {
        return None;
    }
    let bundle = contents.parent()?;
    (bundle.extension()?.to_str()? == "app").then_some(bundle)
}

fn toggle_lightweight<R: Runtime>(app: &AppHandle<R>) {
    if LIGHTWEIGHT_MODE.load(Ordering::Acquire) {
        exit_lightweight(app);
    } else {
        enter_lightweight(app);
    }
}

fn enter_lightweight<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        if window.destroy().is_err() {
            // CheckMenuItem may have already toggled visually; restore it.
            refresh_tray_menu(app);
            return;
        }
    }
    apply_dock_visible(app, false);
    LIGHTWEIGHT_MODE.store(true, Ordering::Release);
    refresh_tray_menu(app);
}

/// 退出轻量模式：必要时重建 `main` 窗口，然后交给 [`show_main_window`] 显示。
///
/// ⚠ 显示动作刻意**不在这里重复实现**（2026-10-05）：本函数历史上自带一段裸的
/// `show / unminimize / set_focus`，与 [`show_main_window`] 是两份并行逻辑 ——
/// 任何显示策略调整（例如「等前端首帧再显示」）都必须同时改两处，漏一处就会出现
/// 「从托盘打开是等首帧、从轻量模式恢复却是立刻显示」这类不一致。
/// 现在统一收敛到 [`show_main_window`]，它自己会处理 `LIGHTWEIGHT_MODE` 与
/// 窗口缺失的情况（本函数已把窗口建好，走进去就是普通显示分支）。
fn exit_lightweight<R: Runtime>(app: &AppHandle<R>) {
    // Regular / dock first so a from_config window is not created while Accessory.
    apply_dock_visible(app, true);
    if app.get_webview_window(MAIN_WINDOW_LABEL).is_none() {
        let Some(config) = app
            .config()
            .app
            .windows
            .iter()
            .find(|window| window.label == MAIN_WINDOW_LABEL)
            .cloned()
        else {
            apply_dock_visible(app, false);
            refresh_tray_menu(app);
            return;
        };
        if WebviewWindowBuilder::from_config(app, &config)
            .and_then(|builder| builder.build())
            .is_err()
        {
            apply_dock_visible(app, false);
            refresh_tray_menu(app);
            return;
        }
    }
    // Window exists now: Windows skip_taskbar was a no-op before recreate.
    apply_dock_visible(app, true);
    LIGHTWEIGHT_MODE.store(false, Ordering::Release);
    // 显示交给唯一入口（它会再 apply_dock_visible + show/unminimize/focus + emit）。
    show_main_window(app);
    refresh_tray_menu(app);
}

fn open_github<R: Runtime>(app: &AppHandle<R>) {
    let url = format!(
        "https://github.com/{}/{}",
        update::GITHUB_OWNER,
        update::GITHUB_REPO
    );
    let _ = app.opener().open_url(url, None::<&str>);
}

struct CheckinBusyGuard<R: Runtime> {
    app: AppHandle<R>,
}

impl<R: Runtime> Drop for CheckinBusyGuard<R> {
    fn drop(&mut self) {
        CHECKIN_BUSY.store(false, Ordering::Release);
        refresh_tray_menu(&self.app);
    }
}

fn start_checkin_all<R: Runtime>(app: &AppHandle<R>) {
    if crate::is_screenshot_demo() {
        set_tray_tooltip(app, "README 截图演示模式");
        return;
    }
    if CHECKIN_BUSY
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    bump_tooltip_generation();
    refresh_tray_menu(app);
    set_tray_tooltip(app, "正在签到…");

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let _busy = CheckinBusyGuard { app: app.clone() };
        let payload = checkin::run_checkin_all().await;
        let text = format_checkin_tooltip(&payload);
        if checkin_succeeded(&payload) {
            notify_checkin(&app, &text);
        } else {
            let generation = bump_tooltip_generation();
            set_tray_tooltip(&app, &text);
            restore_tooltip_after(app, generation);
        }
    });
}

fn notify_checkin<R: Runtime>(app: &AppHandle<R>, body: &str) {
    let _ = app
        .notification()
        .builder()
        .title("ai-gateway")
        .body(body)
        .show();
}

fn checkin_succeeded(value: &Value) -> bool {
    let Some(accounts) = value.get("accounts").and_then(Value::as_array) else {
        return false;
    };
    !accounts.is_empty()
        && accounts.iter().all(|account| {
            matches!(
                account.get("result").and_then(Value::as_str),
                Some("success" | "already")
            )
        })
}

fn restore_tooltip_after<R: Runtime>(app: AppHandle<R>, generation: u64) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(CHECKIN_TOOLTIP_RESTORE_SECS)).await;
        if TOOLTIP_GENERATION.load(Ordering::Acquire) == generation {
            set_tray_tooltip(&app, DEFAULT_TOOLTIP);
        }
    });
}

fn bump_tooltip_generation() -> u64 {
    TOOLTIP_GENERATION.fetch_add(1, Ordering::AcqRel) + 1
}

fn set_tray_tooltip<R: Runtime>(app: &AppHandle<R>, text: &str) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(text));
    }
}

fn refresh_tray_menu<R: Runtime>(app: &AppHandle<R>) {
    let Ok(menu) = build_tray_menu(app) else {
        return;
    };
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_menu(Some(menu));
    }
}

fn build_tray_menu<R: Runtime, M: Manager<R>>(app: &M) -> tauri::Result<Menu<R>> {
    let open_item = MenuItem::with_id(app, "open-main-window", "打开主界面", true, None::<&str>)?;
    let github_item = MenuItem::with_id(app, "open-github", "打开 GitHub", true, None::<&str>)?;
    let checked_in = checkin::scoped_accounts_checked_in_today();
    let (checkin_label, checkin_enabled) = if CHECKIN_BUSY.load(Ordering::Acquire) {
        ("一键签到", false)
    } else if checked_in {
        ("已签到", false)
    } else {
        ("一键签到", true)
    };
    let checkin_item = MenuItem::with_id(
        app,
        "checkin-all",
        checkin_label,
        checkin_enabled,
        None::<&str>,
    )?;
    let lightweight_item = CheckMenuItem::with_id(
        app,
        "lightweight-mode",
        "轻量模式",
        true,
        LIGHTWEIGHT_MODE.load(Ordering::Acquire),
        None::<&str>,
    )?;
    let quit_item = MenuItem::with_id(app, "quit-app", "退出应用", true, None::<&str>)?;

    MenuBuilder::new(app)
        .item(&open_item)
        .item(&github_item)
        .item(&checkin_item)
        .separator()
        .item(&lightweight_item)
        .separator()
        .item(&quit_item)
        .build()
}

fn menu_bar_icon() -> tauri::image::Image<'static> {
    const ICON: &[u8; 36 * 36 * 4] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/icons/tray-icon-template.rgba"
    ));
    tauri::image::Image::new(ICON, 36, 36)
}

fn format_checkin_tooltip(value: &Value) -> String {
    if value.get("status").and_then(Value::as_str) == Some("skipped")
        && value.get("reason").and_then(Value::as_str) == Some("already_running")
    {
        return "签到任务正在进行，请稍后再试".to_string();
    }
    let Some(accounts) = value.get("accounts").and_then(Value::as_array) else {
        return "没有可签到的账号".to_string();
    };
    if accounts.is_empty() {
        return "没有可签到的账号".to_string();
    }

    let mut ok = 0;
    let mut already = 0;
    let mut err = 0;
    for account in accounts {
        match account.get("result").and_then(Value::as_str) {
            Some("success") => ok += 1,
            Some("already") => already += 1,
            Some("error") => err += 1,
            _ => {}
        }
    }
    format!("签到完成：成功 {ok}，已签 {already}，失败 {err}")
}

#[cfg(test)]
mod tests {
    use super::{format_checkin_tooltip, is_silent_startup, menu_bar_icon, should_keep_tray_alive};
    use serde_json::json;

    #[test]
    fn runtime_exit_with_no_code_keeps_tray() {
        assert!(should_keep_tray_alive(None));
        assert!(!should_keep_tray_alive(Some(0)));
    }

    #[test]
    fn menu_bar_icon_has_transparency_and_antialiasing() {
        let icon = menu_bar_icon();
        assert_eq!((icon.width(), icon.height()), (36, 36));
        assert!(icon.rgba().chunks_exact(4).any(|pixel| pixel[3] == 0));
        assert!(icon
            .rgba()
            .chunks_exact(4)
            .any(|pixel| (1..=254).contains(&pixel[3])));
    }

    #[test]
    fn silent_startup_matches_exact_hidden_arg() {
        assert!(is_silent_startup(["--hidden"]));
        assert!(is_silent_startup(["ai-gateway", "--hidden"]));
        assert!(is_silent_startup(["ai-gateway", "--hidden", "--debug"]));
    }

    #[test]
    fn silent_startup_rejects_unrelated_and_substring_args() {
        assert!(!is_silent_startup(Vec::<&str>::new()));
        assert!(!is_silent_startup(["ai-gateway"]));
        assert!(!is_silent_startup(["ai-gateway", "--debug"]));
        assert!(!is_silent_startup(["ai-gateway", "--hidden=true"]));
        assert!(!is_silent_startup(["ai-gateway", "-hidden"]));
        assert!(!is_silent_startup(["ai-gateway", "--hidden-x"]));
        assert!(!is_silent_startup(["ai-gateway", "x--hidden"]));
    }

    #[test]
    fn tooltip_when_no_accounts() {
        assert_eq!(
            format_checkin_tooltip(&json!({"accounts": []})),
            "没有可签到的账号"
        );
    }

    #[test]
    fn tooltip_when_accounts_missing() {
        assert_eq!(format_checkin_tooltip(&json!({})), "没有可签到的账号");
    }

    #[test]
    fn tooltip_reports_overlapping_checkin_as_busy() {
        assert_eq!(
            format_checkin_tooltip(&json!({
                "accounts": [],
                "status": "skipped",
                "reason": "already_running"
            })),
            "签到任务正在进行，请稍后再试"
        );
    }

    #[test]
    fn tooltip_summarizes_success_already_error() {
        let payload = json!({
            "accounts": [
                {"result": "success"},
                {"result": "success"},
                {"result": "already"},
                {"result": "error"},
                {"result": "error"},
                {"result": "error"}
            ]
        });
        assert_eq!(
            format_checkin_tooltip(&payload),
            "签到完成：成功 2，已签 1，失败 3"
        );
    }

    #[test]
    fn checkin_succeeded_requires_all_ok() {
        use super::checkin_succeeded;
        assert!(!checkin_succeeded(&json!({})));
        assert!(!checkin_succeeded(&json!({"accounts": []})));
        assert!(checkin_succeeded(&json!({
            "accounts": [{"result": "success"}, {"result": "already"}]
        })));
        assert!(!checkin_succeeded(&json!({
            "accounts": [{"result": "success"}, {"result": "error"}]
        })));
    }

    #[test]
    fn app_bundle_path_from_packaged_exe() {
        use std::path::Path;
        let exe = Path::new("/Applications/ai-gateway.app/Contents/MacOS/ai-gateway");
        assert_eq!(
            super::app_bundle_path_from_exe(exe),
            Some(Path::new("/Applications/ai-gateway.app"))
        );
    }

    #[test]
    fn app_bundle_path_none_outside_app_bundle() {
        use std::path::Path;
        assert!(super::app_bundle_path_from_exe(Path::new("/tmp/exec")).is_none());
        assert!(
            super::app_bundle_path_from_exe(Path::new("/Users/x/target/debug/ai-gateway"))
                .is_none()
        );
        assert!(super::app_bundle_path_from_exe(Path::new(
            "/Applications/ai-gateway.app/Contents/Resources/icon.icns"
        ))
        .is_none());
    }

    #[test]
    fn tooltip_ignores_unknown_results() {
        let payload = json!({
            "accounts": [
                {"result": "success"},
                {"result": "skipped"},
                {"result": null}
            ]
        });
        assert_eq!(
            format_checkin_tooltip(&payload),
            "签到完成：成功 1，已签 0，失败 0"
        );
    }
}
