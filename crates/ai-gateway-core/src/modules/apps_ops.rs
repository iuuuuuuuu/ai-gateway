//! Trae / 豆包 / 应用切换的**宿主无关**操作层。
//!
//! ## 为什么要有这一层
//!
//! 这些操作本来只写在 Tauri 命令层（`src-tauri/src/commands_apps.rs`），
//! webui（浏览器形态，走 `ai-gateway` HTTP 服务）根本没有对应入口 ——
//! 前端 `api.ts` 里 `trae_*` / `doubao_*` 全都登记了 HTTP 路由，
//! 但服务端 router 里一条都没有，于是 webui 下：
//!
//! - 账号列表请求 404 → 页面永远停在「还没有账号」
//! - 导入按钮点下去 → `请求失败 (404)`，账号一个也进不来
//!
//! 根因不是缺几条路由，而是**业务逻辑只长在宿主里**：只要逻辑还在
//! `commands_apps.rs`，第二个宿主就得再抄一遍，抄漏一处就是又一次「不可用」。
//!
//! 因此把逻辑收敛到这里，两个宿主只做「取参数 → 调这里 → 转 JSON」：
//!
//! | 关注点 | 本层 | 宿主 |
//! |---|---|---|
//! | 业务逻辑、文件读写 | ✅ | ❌ |
//! | 进度事件推送 | 经 [`ProgressSink`] 回调（宿主自己决定发不发事件） | ✅ |
//! | 关客户端/拷快照等慢操作 | ❌ | ✅ 放进 blocking 线程池 |
//!
//! ## 事件推送为什么走回调而不是直接发
//!
//! Tauri 宿主用 `app.emit` 发 `switch-progress`；webui 宿主没有事件总线，
//! 只能把进度写进自己的缓存供前端轮询。core 不依赖任何一方，所以只暴露
//! [`ProgressSink`]，由宿主决定进度往哪去。

use serde_json::{json, Value};

use crate::modules::{
    account_groups,
    app_profile::{self, profile_for, TargetApp},
    cli_task, config, doubao_account, doubao_chats, doubao_health, doubao_quota, doubao_session,
    scheduler, switcher, trae_account, trae_checkin, trae_checkin_results, trae_credits, trae_device,
    trae_discover, trae_refresh, trae_usage_history,
};

/// 切换进度接收方 —— 与 [`switcher::ProgressSink`] 同形，
/// 但语义是「宿主要把这条进度送到哪里去」，因此单独命名以免混淆两者。
///
/// 必须 `Send + Sync`：切换动作会把 sink 借给 blocking 线程池里的子进程操作。
pub trait ProgressSink: Send + Sync {
    fn step(&self, stage: &str, status: switcher::StepStatus, message: &str);
}

/// 丢弃全部进度的 sink（webui 的同步命令、无 UI 场景）。
pub struct NullSink;

impl ProgressSink for NullSink {
    fn step(&self, _stage: &str, _status: switcher::StepStatus, _message: &str) {}
}

/// 汇总步骤的 sink：既转发给外层，又留一份给返回值兜底。
struct CollectingSink<'a> {
    inner: &'a dyn ProgressSink,
    steps: std::sync::Mutex<Vec<Value>>,
}

impl CollectingSink<'_> {
    fn steps(&self) -> Vec<Value> {
        self.steps.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

impl switcher::ProgressSink for CollectingSink<'_> {
    fn step(&self, stage: &str, status: switcher::StepStatus, message: &str) {
        self.inner.step(stage, status, message);
        if let Ok(mut steps) = self.steps.lock() {
            steps.push(json!({
                "stage": stage,
                "status": status.as_str(),
                "message": message,
            }));
        }
    }
}

// ---------------------------------------------------------------------------
// 环境检测
// ---------------------------------------------------------------------------

/// 应用安装/运行状态。
///
/// exe 发现失败**不是错误** —— 只表示「未安装」，界面据此提示用户手动指定路径。
pub fn app_env_check(target_app: &str) -> Result<Value, String> {
    let app = TargetApp::parse(target_app);
    let store = config::store_dir();
    let prof = profile_for(app, &store);

    // 三次 Session 构造共用同一组参数，只差要问的问题
    let session = |action: switcher::Action| {
        switcher::Session::new(&switcher::RunArgs {
            action,
            target_app: app,
            user_id: None,
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            store_dir: store.clone(),
        })
    };

    let exe = switcher::locate::find_exe(&session(switcher::Action::BackupCurrent)).ok();

    let snapshot_count = std::fs::read_dir(&prof.profiles_dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    e.path().is_dir() && name != "last" && !name.ends_with(".bak")
                })
                .count()
        })
        .unwrap_or(0);

    let manual_path = config::app_setting_str(prof.settings_path_key);

    let running = switcher::proc::is_running(&session(switcher::Action::BackupCurrent));

    // 版本与外置配置来源：界面要能回答「我装的是哪个版本、设置从哪读的」，
    // 否则用户改了 conf 却不生效时无从排查（参考实现为此专门暴露了这两项）。
    let (version, version_source) = detect_app_version(&prof.data_dir);

    Ok(json!({
        "targetApp": app.as_str(),
        "appName": prof.app_name,
        "layout": prof.layout.as_str(),
        "installed": exe.is_some(),
        "exePath": exe.map(|p| p.to_string_lossy().to_string()),
        "dataDir": prof.data_dir.to_string_lossy(),
        "dataDirExists": prof.data_dir.is_dir(),
        "profilesDir": prof.profiles_dir.to_string_lossy(),
        "snapshotCount": snapshot_count,
        "manualPath": manual_path,
        "settingsPathKey": prof.settings_path_key,
        "running": running,
        "version": version,
        "versionSource": version_source,
        "settingsFile": config::app_settings_file().to_string_lossy(),
    }))
}

/// 探测客户端的版本号与版本信息来源。
///
/// 版本对排障很关键：切换/快照的布局假设是按版本抓包固化的。界面显示
/// 「未探测到」比显示一个瞎猜的数字有用得多 —— 因此探测不到就是 `None`，
/// **不**回退成我们自己的打包版本号（那会让用户以为客户端就是这个版本）。
///
/// 来源优先级：
/// 1. `User Data\Last Version`（Chromium 系客户端自己写的，改版后仍在）；
/// 2. 数据目录下的 `version` / `VERSION` 文本文件；
/// 3. `resources/app/package.json` 的 `version` 字段（Electron 常见布局）。
fn detect_app_version(data_dir: &std::path::Path) -> (Option<String>, Option<String>) {
    let read_first_line = |p: &std::path::Path| -> Option<String> {
        let text = std::fs::read_to_string(p).ok()?;
        let line = text.lines().next()?.trim().to_string();
        if line.is_empty() {
            None
        } else {
            Some(line)
        }
    };

    let last_version = data_dir.join("Last Version");
    if let Some(v) = read_first_line(&last_version) {
        return (Some(v), Some(last_version.to_string_lossy().to_string()));
    }

    for name in ["version", "VERSION", "Version"] {
        let p = data_dir.join(name);
        if let Some(v) = read_first_line(&p) {
            return (Some(v), Some(p.to_string_lossy().to_string()));
        }
    }

    for rel in ["resources/app/package.json", "app/package.json"] {
        let p = data_dir.join(rel);
        if let Ok(text) = std::fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(ver) = v.get("version").and_then(Value::as_str) {
                    return (Some(ver.to_string()), Some(p.to_string_lossy().to_string()));
                }
            }
        }
    }

    (None, None)
}

/// 保存应用的手动 exe 路径（空串 = 清除）。
pub fn app_set_manual_path(target_app: &str, path: &str) -> Result<Value, String> {
    let app = TargetApp::parse(target_app);
    let prof = profile_for(app, &config::store_dir());
    let trimmed = path.trim();
    if !trimmed.is_empty() {
        let p = std::path::Path::new(trimmed);
        if !p.is_file() {
            return Err(format!("路径不存在或不是文件: {trimmed}"));
        }
        if !crate::modules::app_profile::exe_matches(p, &prof) {
            return Err(format!(
                "该文件不是 {} 的可执行文件（期望文件名：{}）",
                prof.app_name,
                prof.exe_names.join(" 或 ")
            ));
        }
    }
    config::set_app_setting(prof.settings_path_key, json!(trimmed)).map_err(|e| e.to_string())?;
    // 路径变了，清掉 exe 缓存，否则下次仍命中旧路径
    let sess = switcher::Session::new(&switcher::RunArgs {
        action: switcher::Action::BackupCurrent,
        target_app: app,
        user_id: None,
        proxy_port: None,
        include_indexeddb: false,
        expected_current_uid: String::new(),
        store_dir: config::store_dir(),
    });
    switcher::locate::clear_exe_cache(&sess, &prof);
    Ok(json!({ "ok": true }))
}

/// 当前登录态属于哪个账号（读 `current_account.txt`）。
pub fn current_account(target_app: &str) -> Value {
    let app = TargetApp::parse(target_app);
    let sess = switcher::Session::new(&switcher::RunArgs {
        action: switcher::Action::BackupCurrent,
        target_app: app,
        user_id: None,
        proxy_port: None,
        include_indexeddb: false,
        expected_current_uid: String::new(),
        store_dir: config::store_dir(),
    });
    json!({ "userId": switcher::get_current_account(&sess) })
}

// ---------------------------------------------------------------------------
// 登录态切换
// ---------------------------------------------------------------------------

/// 一次切换动作的全部入参（从宿主参数解包后传进来）。
pub struct SwitchRequest {
    pub action: String,
    pub target_app: String,
    pub user_id: Option<String>,
    pub proxy_port: Option<u16>,
    pub include_indexeddb: Option<bool>,
    pub expected_current_uid: Option<String>,
}

/// 切换前的 JWT 失效预检，结果作为一步进度送出（不阻断流程）。
///
/// 用 `block_on` 在同步的 `switch_action` 里跑异步探活：整个 `switch_action`
/// 本身由宿主放在 `spawn_blocking` 里调用，阻塞的是线程池线程而不是事件循环。
fn switch_probe_step(app_kind: TargetApp, uid: &str, sink: &dyn ProgressSink) {
    // 只有 Trae 系账号有可探的签到接口；豆包走另一套凭证语义
    if app_kind != TargetApp::Trae {
        return;
    }
    let Some(acc) = trae_account::find_account(uid) else {
        return;
    };
    let jwt = acc.get("jwt").and_then(Value::as_str).unwrap_or("");
    if jwt.trim().is_empty() {
        sink.step(
            "probe",
            switcher::StepStatus::Warn,
            "该账号没有 JWT，切换后需要重新登录",
        );
        return;
    }
    let dev = trae_device::resolve_device(uid);
    let probe = tauri_less_block_on(trae_checkin::probe_jwt_alive(jwt, &dev));
    match probe {
        trae_checkin::JwtLiveness::Alive => {
            sink.step("probe", switcher::StepStatus::Ok, "JWT 预检通过");
        }
        trae_checkin::JwtLiveness::Dead(msg) => {
            sink.step(
                "probe",
                switcher::StepStatus::Warn,
                &format!("{msg}（仍继续切换，登录后可自动修复）"),
            );
        }
        trae_checkin::JwtLiveness::Unknown(msg) => {
            sink.step(
                "probe",
                switcher::StepStatus::Warn,
                &format!("{msg}（跳过预检，继续切换）"),
            );
        }
    }
}

/// 在同步上下文里驱动一个 future 直到完成。
///
/// 优先复用当前 tokio runtime（宿主通常在 runtime 线程上调进来）；
/// 没有 runtime 时（纯同步宿主 / 测试）临时建一个单线程 runtime。
fn tauri_less_block_on<F: std::future::Future>(fut: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("创建临时 tokio runtime 失败")
            .block_on(fut),
    }
}

/// 解析动作名（界面传字符串，避免前端与 Rust 枚举耦合）。
pub fn parse_action(action: &str) -> Result<switcher::Action, String> {
    match action.trim() {
        "Switch" | "switch" => Ok(switcher::Action::Switch),
        "SaveCurrentLogin" | "save" => Ok(switcher::Action::SaveCurrentLogin),
        "BackupCurrent" | "backup" => Ok(switcher::Action::BackupCurrent),
        "RestoreOnly" | "restore" => Ok(switcher::Action::RestoreOnly),
        "ResetDeviceIds" | "resetDeviceIds" => Ok(switcher::Action::ResetDeviceIds),
        "KeepAlive" | "keepalive" => Ok(switcher::Action::KeepAlive),
        "LaunchOnly" | "launch" => Ok(switcher::Action::LaunchOnly),
        other => Err(format!("未知动作: {other}")),
    }
}

/// 执行一次切换动作，进度经 `sink` 送出，返回界面要的结果 JSON。
///
/// 失败（`outcome.is_err()`）**不**作为 `Err` 上抛：切换失败是「动作结果」，
/// 界面要靠 `success:false` + `steps` 展示失败细节，而不是只弹一条错误。
/// 宿主事件里的 `switch-done` / 响应体共用同一个 payload。
pub fn switch_action(req: SwitchRequest, sink: &dyn ProgressSink) -> Result<Value, String> {
    let act = parse_action(&req.action)?;
    let app_kind = TargetApp::parse(&req.target_app);
    let user_id = req.user_id.clone();

    // JWT 失效预检（仅切到 Trae 的账号时）：**警告不阻断**。
    // 见 `trae_checkin::probe_jwt_alive` 的说明 —— 硬阻断会锁死需要重新登录的账号。
    if matches!(act, switcher::Action::Switch) {
        if let Some(uid) = user_id.as_deref().filter(|s| !s.trim().is_empty()) {
            switch_probe_step(app_kind, uid, sink);
        }
    }

    let collector = CollectingSink {
        inner: sink,
        steps: std::sync::Mutex::new(Vec::new()),
    };
    let args = switcher::RunArgs {
        action: act,
        target_app: app_kind,
        user_id: user_id.clone(),
        proxy_port: req.proxy_port,
        // 未显式传参时读设置：IndexedDB 纳入快照会让快照大一个量级，
        // 因此做成用户可关的开关而不是恒为真
        include_indexeddb: req
            .include_indexeddb
            .unwrap_or_else(|| config::app_setting_bool("doubao_snapshot_include_idb", false)),
        expected_current_uid: req.expected_current_uid.unwrap_or_default(),
        store_dir: config::store_dir(),
    };

    let outcome = switcher::run_action(args, &collector);
    let steps = collector.steps();
    let payload = match &outcome {
        Ok(message) => json!({ "success": true, "message": message, "steps": steps }),
        Err(error) => json!({ "success": false, "error": error, "steps": steps }),
    };
    Ok(json!({
        "ok": outcome.is_ok(),
        "message": payload.get("message").cloned().unwrap_or(Value::Null),
        "error": payload.get("error").cloned().unwrap_or(Value::Null),
        "success": outcome.is_ok(),
        "steps": steps,
        "action": req.action,
        "targetApp": app_kind.as_str(),
        "userId": user_id,
    }))
}

/// 递归统计目录的 (总字节数, 文件数)。
///
/// 不跟随符号链接：跟随会走进客户端目录的软链循环，统计耗时爆炸甚至死循环。
/// 统计失败（权限不足、文件被占用）按 0 计入，不中断整次列表 ——
/// 一个坏文件不该让整个快照列表打不开。
fn dir_size_and_count(root: &std::path::Path) -> (u64, u64) {
    let mut size = 0u64;
    let mut files = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                size += meta.len();
                files += 1;
            }
        }
    }
    (size, files)
}

/// 列出某应用的登录态快照。
pub fn list_snapshots(target_app: &str) -> Result<Value, String> {
    let app = TargetApp::parse(target_app);
    let prof = profile_for(app, &config::store_dir());
    let Ok(entries) = std::fs::read_dir(&prof.profiles_dir) else {
        return Ok(json!({ "snapshots": [] }));
    };
    let sess = switcher::Session::new(&switcher::RunArgs {
        action: switcher::Action::BackupCurrent,
        target_app: app,
        user_id: None,
        proxy_port: None,
        include_indexeddb: false,
        expected_current_uid: String::new(),
        store_dir: config::store_dir(),
    });
    let current = switcher::get_current_account(&sess);
    // 现场可能已被用户直接在客户端里换过号（标记只在走本应用切换时才写）。
    // 标出来让界面提示「当前登录」徽章可能不准，而不是让用户被误导。
    let (resolved, marker_stale) = switcher::resolve_current_uid(&sess);

    let mut snapshots: Vec<Value> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            // `last` 是安全槽、`*.bak` 是上一代备份，都不是「账号快照」
            if name == "last" || name.ends_with(".bak") {
                return None;
            }
            let path = entry.path();
            // 体积与文件数：用户判断「这个快照值不值得留」靠的就是这两个数。
            // 遍历是递归的，快照目录通常几十个文件，代价可接受。
            let (size, files) = dir_size_and_count(&path);
            Some(json!({
                "userId": name,
                "isCurrent": name == current,
                "modifiedAt": entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs()),
                "hasMeta": path.join("snapshot_meta.json").exists(),
                "sizeBytes": size,
                "fileCount": files,
            }))
        })
        .collect();
    snapshots.sort_by(|a, b| {
        b.get("modifiedAt")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .cmp(&a.get("modifiedAt").and_then(Value::as_i64).unwrap_or(0))
    });
    Ok(json!({
        "snapshots": snapshots,
        "currentUserId": current,
        // 标记过期 ⇔ 现场文件比标记新 ⇔ 用户可能已在客户端里直接换号
        "markerStale": marker_stale,
        "resolvedUserId": resolved,
    }))
}

/// 删除某个账号的登录态快照（含 `.bak`）。
pub fn delete_snapshot(target_app: &str, user_id: &str) -> Result<Value, String> {
    doubao_account::ensure_uid_safe(user_id)?;
    let app = TargetApp::parse(target_app);
    let prof = profile_for(app, &config::store_dir());
    let slot = prof.slot_dir(user_id);
    if !slot.exists() {
        return Err(format!("账号 {user_id} 没有快照"));
    }
    std::fs::remove_dir_all(&slot).map_err(|e| format!("删除快照失败: {e}"))?;
    let _ = std::fs::remove_dir_all(prof.profiles_dir.join(format!("{user_id}.bak")));
    Ok(json!({ "ok": true }))
}

// ---------------------------------------------------------------------------
// Trae 账号
// ---------------------------------------------------------------------------

/// Trae 账号列表（JWT 已脱敏），并把签到冷却、套餐身份、分组并进元数据。
pub fn trae_list_accounts() -> Value {
    let accounts: Vec<Value> = trae_account::load_accounts()
        .iter()
        .map(trae_account::account_meta)
        .collect();
    let cooldowns = trae_checkin::all_cooldowns();
    let pay_status = trae_credits::load_pay_status_cache();
    let membership = account_groups::membership("Trae");
    // 界面一次请求就能渲染完整状态
    let accounts: Vec<Value> = accounts
        .into_iter()
        .map(|mut acc| {
            let Some(uid) = acc.get("userId").and_then(Value::as_str).map(str::to_string) else {
                return acc;
            };
            if let Some(cd) = cooldowns.get(&uid) {
                acc["cooldown"] = cd.clone();
            }
            // 付费身份缓存：命中就覆盖（缓存比账号记录里的旧值新）
            if let Some(entry) = pay_status.get(&uid) {
                if let Some(identity) = entry.get("identity") {
                    if !identity.is_null() {
                        acc["payIdentity"] = identity.clone();
                    }
                }
                if let Some(exp) = entry.get("expireAt") {
                    if !exp.is_null() {
                        acc["payExpireAt"] = exp.clone();
                    }
                }
            }
            // 分组：界面据此做筛选与分组选择器回显
            if let Some(gid) = membership.get(&uid).and_then(Value::as_str) {
                acc["group"] = json!(gid);
            }
            acc
        })
        .collect();
    json!({ "accounts": accounts })
}

/// 手动添加/更新 Trae 账号（粘贴 JWT）。
pub fn trae_add_account(
    jwt: &str,
    name: Option<&str>,
    refresh_token: Option<&str>,
) -> Result<Value, String> {
    let info = trae_account::parse_jwt(jwt);
    let uid = info
        .user_id
        .ok_or_else(|| "无法从 JWT 解析出账号 id，请确认粘贴的是完整的 Cloud-IDE-JWT".to_string())?;
    let acc = trae_account::upsert_account(&uid, name, jwt, refresh_token)?;
    Ok(json!({ "ok": true, "account": trae_account::account_meta(&acc) }))
}

/// 删除 Trae 账号。
pub fn trae_delete_account(user_id: &str) -> Result<Value, String> {
    trae_account::delete_account(user_id)?;
    // 顺手清掉分组映射：留着会变成指向不存在账号的孤儿记录，
    // 让分组计数比实际账号数多
    account_groups::forget_account("Trae", user_id);
    Ok(json!({ "ok": true }))
}

/// 编辑 Trae 账号（昵称，可选同时替换 JWT / refresh_token）。
///
/// 只传昵称时不动凭证：界面上「改个名字」不该有覆盖登录态的风险。
/// 传了 JWT 时先解析出新 uid —— 换了账号的 JWT 意味着这其实是另一个账号，
/// 此时**拒绝**（而不是静默把 A 的记录改成 B 的凭证），让用户删掉重建。
pub fn trae_update_account(
    user_id: &str,
    name: Option<&str>,
    jwt: Option<&str>,
    refresh_token: Option<&str>,
) -> Result<Value, String> {
    config::ensure_uid_safe(user_id)?;
    let uid = user_id.trim();

    if let Some(jwt) = jwt.map(str::trim).filter(|s| !s.is_empty()) {
        let info = trae_account::parse_jwt(jwt);
        let new_uid = info.user_id.clone().ok_or_else(|| {
            "无法从 JWT 解析出账号 id，请确认粘贴的是完整的 Cloud-IDE-JWT".to_string()
        })?;
        if new_uid != uid {
            return Err(format!(
                "该 JWT 属于账号 {new_uid}，与当前编辑的 {uid} 不一致；请删除后重新添加"
            ));
        }
        trae_account::update_jwt(uid, jwt)?;
    }
    if let Some(rt) = refresh_token.map(str::trim).filter(|s| !s.is_empty()) {
        trae_account::set_refresh_token(uid, rt)?;
    }
    if let Some(name) = name.map(str::trim).filter(|s| !s.is_empty()) {
        trae_account::rename_account(uid, name)?;
    }
    let acc = trae_account::find_account(uid).ok_or_else(|| "账号不存在".to_string())?;
    Ok(json!({ "ok": true, "account": trae_account::account_meta(&acc) }))
}

/// 读取账号的完整 JWT（仅供「查看/编辑」弹窗回填）。
pub fn trae_account_jwt(user_id: &str) -> Result<Value, String> {
    let jwt = trae_account::account_jwt(user_id)?;
    Ok(json!({ "userId": user_id, "jwt": jwt }))
}

/// 解析一段 JWT（供编辑弹窗实时预览，**不**落库）。
///
/// 解析失败不返回 `Err`：这是输入框的实时校验，用户还没粘完就报错会把界面刷成
/// 一片红。改为返回 `valid: false` + 空字段，由前端自行决定怎么提示。
pub fn trae_jwt_parse(jwt: &str) -> Value {
    let info = trae_account::parse_jwt(jwt);
    let valid = info.user_id.is_some();
    json!({
        "valid": valid,
        "userId": info.user_id,
        "expHours": info.exp_hours,
        "expTimestamp": info.exp_timestamp,
        "status": info.status(),
    })
}

/// 清空全部账号的签到冷却（含只计数未落冷却的残留记录）。
pub fn trae_clear_all_cooldowns() -> Value {
    let cleared = trae_checkin::clear_all_cooldowns();
    json!({ "ok": true, "cleared": cleared })
}

// ---------------------------------------------------------------------------
// Trae 账号导出 / 导入
// ---------------------------------------------------------------------------

/// 导出 Trae 账号为可移植的 JSON 文本。
///
/// **含明文 JWT 与 refresh_token** —— 这是「换台机器接着用」的前提，
/// 因此调用方（界面）必须明确提示用户这是敏感文件。
/// 导出结构自带 `kind` 标记，导入时据此拒绝把 WorkBuddy 账号库的文件灌进来。
pub fn trae_export_accounts(user_ids: Option<Vec<String>>) -> Result<Value, String> {
    let accounts = trae_account::load_accounts();
    let selected: Vec<Value> = match user_ids.filter(|v| !v.is_empty()) {
        Some(ids) => {
            let ids: Vec<String> = ids.into_iter().map(|s| s.trim().to_string()).collect();
            let picked: Vec<Value> = accounts
                .iter()
                .filter(|a| {
                    a.get("user_id")
                        .and_then(Value::as_str)
                        .is_some_and(|uid| ids.iter().any(|i| i == uid))
                })
                .cloned()
                .collect();
            if picked.is_empty() {
                return Err("没有匹配到要导出的账号".to_string());
            }
            picked
        }
        None => accounts,
    };

    let payload = json!({
        "kind": "trae-accounts",
        "version": 1,
        "exportedAt": config::utc_iso(),
        "count": selected.len(),
        "accounts": selected,
    });
    let text = serde_json::to_string_pretty(&payload)
        .map_err(|e| format!("序列化导出内容失败：{e}"))?;
    Ok(json!({ "ok": true, "count": selected.len(), "text": text }))
}

/// 导入 Trae 账号（合并语义：同 uid 覆盖，其余追加）。
///
/// 覆盖而不是跳过：导出文件通常来自「另一台机器上更新的登录态」，
/// 跳过会让用户以为导入成功、实际还用着旧 JWT。
pub fn trae_import_accounts(text: &str) -> Result<Value, String> {
    let parsed: Value =
        serde_json::from_str(text).map_err(|e| format!("导入内容不是合法 JSON：{e}"))?;
    let kind = parsed.get("kind").and_then(Value::as_str);
    if kind != Some("trae-accounts") {
        return Err(
            "这不是 Trae 账号导出文件（缺少 kind=trae-accounts 标记）；\
             请确认没有选错文件"
                .to_string(),
        );
    }
    let incoming = parsed
        .get("accounts")
        .and_then(Value::as_array)
        .ok_or_else(|| "导入内容缺少 accounts 数组".to_string())?;

    let mut existing = trae_account::load_accounts();
    let mut added = 0usize;
    let mut updated = 0usize;
    let mut skipped = 0usize;

    for item in incoming {
        let Some(uid) = item.get("user_id").and_then(Value::as_str).map(str::trim) else {
            skipped += 1;
            continue;
        };
        let jwt = item.get("jwt").and_then(Value::as_str).unwrap_or("").trim();
        // 没有 JWT 的记录导入进来就是僵尸账号：界面能看见、但永远登录不了
        if uid.is_empty() || jwt.is_empty() || config::ensure_uid_safe(uid).is_err() {
            skipped += 1;
            continue;
        }
        match existing
            .iter()
            .position(|a| a.get("user_id").and_then(Value::as_str) == Some(uid))
        {
            Some(idx) => {
                existing[idx] = item.clone();
                updated += 1;
            }
            None => {
                existing.push(item.clone());
                added += 1;
            }
        }
    }

    if added + updated > 0 {
        trae_account::save_accounts(&existing).map_err(|e| format!("保存账号库失败：{e}"))?;
    }
    Ok(json!({
        "ok": true,
        "added": added,
        "updated": updated,
        "skipped": skipped,
    }))
}

/// 预览导入文件（不落库）：告诉用户文件里有几个账号、分别是谁。
pub fn trae_preview_import(text: &str) -> Result<Value, String> {    let parsed: Value =
        serde_json::from_str(text).map_err(|e| format!("导入内容不是合法 JSON：{e}"))?;
    if parsed.get("kind").and_then(Value::as_str) != Some("trae-accounts") {
        return Err("这不是 Trae 账号导出文件".to_string());
    }
    let existing = trae_account::load_accounts();
    let items: Vec<Value> = parsed
        .get("accounts")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|a| {
                    let uid = a.get("user_id").and_then(Value::as_str).unwrap_or("");
                    json!({
                        "userId": uid,
                        "name": a.get("name").and_then(Value::as_str).unwrap_or(""),
                        "hasRefreshToken": a
                            .get("refresh_token")
                            .and_then(Value::as_str)
                            .is_some_and(|s| !s.trim().is_empty()),
                        // 界面据此提示「会覆盖现有账号」——覆盖是不可逆的
                        "willOverwrite": existing
                            .iter()
                            .any(|e| e.get("user_id").and_then(Value::as_str) == Some(uid)),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "ok": true,
        "count": items.len(),
        "exportedAt": parsed.get("exportedAt").cloned().unwrap_or(Value::Null),
        "accounts": items,
    }))
}

// ---------------------------------------------------------------------------
// Trae 积分历史 / 趋势
// ---------------------------------------------------------------------------

/// 积分趋势：按天汇总全部账号的总量与变化。
///
/// 返回的 `daily` 是**按天补齐**的连续序列 —— 缺天补 0 而不是跳过。
/// 跳过缺天会让折线把两个相隔一周的点连成直线，视觉上像是缓慢下降，
/// 实际那一周根本没采到数据。
pub fn trae_credits_stats(days: Option<i64>) -> Value {
    let days = days.unwrap_or(30).clamp(1, 90);
    let rows = trae_credits::load_credits_history();
    let today = chrono::Utc::now().date_naive();

    // 先按日期聚合每天的总量（同一天多账号相加）
    let mut by_day: std::collections::BTreeMap<String, (i64, i64)> =
        std::collections::BTreeMap::new();
    for row in &rows {
        let Some(day) = row.get("date").and_then(Value::as_str) else {
            continue;
        };
        let credits = row.get("credits").and_then(Value::as_i64).unwrap_or(0);
        let delta = row.get("delta").and_then(Value::as_i64).unwrap_or(0);
        let entry = by_day.entry(day.to_string()).or_insert((0, 0));
        entry.0 += credits;
        // delta 只累加**消耗**（负值），充值/补发属于收入，不该算进消耗曲线
        if delta < 0 {
            entry.1 += delta;
        }
    }

    // 按天补齐：从 days 天前到今天，缺的那天给 null 而不是 0。
    // 用 0 会和「积分真的归零」混淆 —— null 明确表示「这天没采到数据」。
    let mut daily: Vec<Value> = Vec::new();
    for i in (0..days).rev() {
        let day = (today - chrono::Duration::days(i)).format("%Y-%m-%d").to_string();
        match by_day.get(&day) {
            Some((total, consumed)) => daily.push(json!({
                "date": day,
                "total": total,
                "consumed": -consumed,
            })),
            None => daily.push(json!({
                "date": day,
                "total": Value::Null,
                "consumed": Value::Null,
            })),
        }
    }

    // 只有真的有数据的天才计入统计，否则「平均消耗」会被缺天数稀释
    let observed: Vec<&Value> = daily.iter().filter(|d| !d["total"].is_null()).collect();
    let latest = observed.last().copied();
    let earliest = observed.first().copied();

    let total_consumed: i64 = daily
        .iter()
        .filter_map(|d| d["consumed"].as_i64())
        .sum();

    let accounts: Vec<Value> = trae_account::load_accounts()
        .iter()
        .map(|a| {
            let uid = a.get("user_id").and_then(Value::as_str).unwrap_or("");
            let latest_row = rows
                .iter()
                .rev()
                .find(|r| r.get("userId").and_then(Value::as_str) == Some(uid));
            json!({
                "userId": uid,
                "name": trae_account::display_name(a),
                "credits": latest_row.and_then(|r| r.get("credits")).cloned().unwrap_or(Value::Null),
                "updatedAt": latest_row.and_then(|r| r.get("updatedAt")).cloned().unwrap_or(Value::Null),
            })
        })
        .collect();

    json!({
        "days": days,
        "daily": daily,
        "summary": {
            "latestTotal": latest.map(|d| d["total"].clone()).unwrap_or(Value::Null),
            "firstTotal": earliest.map(|d| d["total"].clone()).unwrap_or(Value::Null),
            "consumed": if observed.is_empty() { Value::Null } else { json!(total_consumed) },
            "observedDays": observed.len(),
            // 只有**至少两天**数据才谈得上「变化」：一天数据时 earliest 与 latest 是同一行，
            // 直接相减会得到 0，而 0 会被读成「没变化」—— 真相是「无从得知」。
            "change": match (observed.len(), earliest.and_then(|d| d["total"].as_i64()), latest.and_then(|d| d["total"].as_i64())) {
                (n, Some(a), Some(b)) if n >= 2 => json!(b - a),
                _ => Value::Null,
            },
        },
        "accounts": accounts,
    })
}

/// 手动写入一次积分快照（供「立即采样」按钮 —— 不必等到计划任务）。
pub async fn trae_credits_snapshot() -> Result<Value, String> {
    let accounts = trae_account::load_accounts();
    let mut sampled = 0usize;
    let mut failed = 0usize;
    let mut errors: Vec<String> = Vec::new();

    for acc in &accounts {
        let Some(uid) = acc.get("user_id").and_then(Value::as_str) else {
            continue;
        };
        // 日差 = 今天 total 减昨天 total；昨天没有记录时差值为 0
        let mut delta = 0i64;
        match trae_credits::fetch_credit_total(uid).await {
            Ok(Some(total)) => {
                let yesterday = chrono::Utc::now() - chrono::Duration::days(1);
                let yday = yesterday.format("%Y-%m-%d").to_string();
                let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
                let prev = trae_credits::load_credits_history()
                    .into_iter()
                    .find(|r| {
                        r.get("userId").and_then(Value::as_str) == Some(uid)
                            && matches!(
                                r.get("date").and_then(Value::as_str),
                                Some(d) if d == yday.as_str() || d == today.as_str()
                            )
                    })
                    .and_then(|r| r.get("credits").and_then(Value::as_i64));
                if let Some(prev) = prev {
                    delta = total - prev;
                }
                trae_credits::daily_snapshot(uid, total, delta)?;
                sampled += 1;
            }
            Ok(None) => {
                failed += 1;
                errors.push(format!("{uid}: 上游未返回积分总额"));
            }
            Err(e) => {
                failed += 1;
                errors.push(format!("{uid}: {e}"));
            }
        }
    }

    Ok(json!({
        "ok": failed == 0,
        "sampled": sampled,
        "failed": failed,
        "errors": errors,
    }))
}

/// 发现本机登录过的 Trae 账号（双应用）。
pub fn trae_discover_accounts() -> Value {
    json!({
        "accounts": trae_discover::discover_all(),
        "apps": [
            { "kind": "TraeWork", "label": "Trae Work" },
            { "kind": "Trae", "label": "Trae" },
        ],
    })
}

/// 从本机登录态**导入** Trae 账号（JWT 就在客户端本地 Cookies 里）。
///
/// 与「设置 → 本地代理 → 从本机捕获凭证」是同一套底层能力
/// （[`crate::modules::device_proxy::local_capture::capture_from_local`]）：
/// 解 `Local State` 的 DPAPI 密钥 → 解 cookie → 提 `Cloud-IDE-JWT` → 写回账号库。
/// **不需要启动客户端、不需要开代理、不需要装 CA 证书** —— 只要客户端登录过至少一次，
/// Cookies 文件就在磁盘上。
///
/// 之所以要在 Trae 页单独开一个入口：之前这条能力只挂在「本地代理」设置里，
/// 用户在账号页看到「发现本机账号」后仍然得**手贴一个本机已经有的 JWT**。
pub fn trae_import_local() -> Result<Value, String> {
    let captured = crate::modules::device_proxy::local_capture::capture_from_local()?;
    let summary = captured
        .get("summary")
        .cloned()
        .unwrap_or_else(|| json!({ "appended": 0, "updated": 0, "skipped": 0, "total": 0 }));
    Ok(json!({
        "ok": true,
        "import": summary,
        "accounts": trae_list_accounts().get("accounts").cloned().unwrap_or_default(),
    }))
}

/// 「发现本机账号」+ 立即导入一次，并把导入结果并回发现列表。
///
/// 为什么合成一个入口：只发现不导入，界面上就会出现「识别到了账号却不能自动填写」——
/// 用户得先点「发现」，再点「添加」，然后手动贴一个本机已有的 JWT。
/// 两步合一之后，「发现」本身就是「导入」。
///
/// 导入后要做两件收尾：
/// 1. 用**导入后**的账号库重算 `inPool`，否则界面会把刚导进来的账号仍显示成「未入库」
/// 2. 把已读到的套餐身份填进占位名（`auto_<uid8>` → `<应用> · <套餐>`），
///    让用户一眼认得出是哪个号；用户自定义过的名字不动
pub fn trae_discover_and_import() -> Value {
    let mut accounts = trae_discover::discover_all();

    // 导入可能失败（非 Windows / 本地无 Cookies），失败不该让「发现」整体不可用
    let import = match crate::modules::device_proxy::local_capture::capture_from_local() {
        Ok(captured) => captured
            .get("summary")
            .cloned()
            .unwrap_or_else(|| json!({ "appended": 0, "updated": 0, "skipped": 0, "total": 0 })),
        Err(e) => json!({ "appended": 0, "updated": 0, "skipped": 0, "total": 0, "error": e }),
    };

    for item in accounts.iter_mut() {
        let uid = item
            .get("userId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if uid.is_empty() {
            continue;
        }
        // 1) 用导入后的账号库重算「已在账号库」
        let exists = trae_account::find_account(&uid).is_some();
        if let Some(obj) = item.as_object_mut() {
            obj.insert("inPool".to_string(), json!(exists));
        }
        // 2) 占位名 + 已读到的套餐身份 → 有信息量的名字
        if exists {
            if let Some(label) = item.get("appLabel").and_then(Value::as_str) {
                if let Some(identity) = item
                    .get("payIdentity")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    trae_account::set_name_if_placeholder(&uid, &format!("{label} · {identity}"));
                }
            }
        }
    }

    json!({
        "accounts": accounts,
        "import": import,
        "apps": [
            { "kind": "TraeWork", "label": "Trae Work" },
            { "kind": "Trae", "label": "Trae" },
        ],
    })
}

/// 读取某个应用的套餐身份。
pub fn trae_entitlement(app_kind: &str) -> Value {
    trae_discover::read_entitlement(app_kind).unwrap_or_else(|| json!({}))
}

/// 读取/重置账号的设备指纹。
pub fn trae_device_info(user_id: &str, reset: Option<bool>) -> Value {
    let device = if reset.unwrap_or(false) {
        trae_device::reset_device_for(user_id)
    } else {
        trae_device::resolve_device(user_id)
    };
    json!({
        "userId": user_id,
        "deviceId": device.device_id,
        "sessionId": device.session_id,
        "marketUserId": device.market_user_id,
    })
}

/// Trae 签到历史（积分趋势）。
pub fn trae_credits_history() -> Value {
    json!({ "records": trae_checkin::load_credits_history() })
}

/// 清除某账号的签到冷却。
pub fn trae_clear_cooldown(user_id: &str) -> Value {
    trae_checkin::clear_cooldown(user_id);
    json!({ "ok": true })
}

/// 签到失败重试的轮次与间隔（秒）。
///
/// 逐轮加长而不是固定间隔：第一轮等 30 秒足以跨过大多数瞬时抖动；
/// 还没好的多半是上游在短暂维护，给到 90 秒再试一次，总等待不超过 2 分钟。
const RETRY_DELAYS: [u64; 2] = [30, 90];

/// 逐账号推送签到进度（`checkin:account`）。
fn push_checkin_outcomes(summary: &trae_checkin::RoundSummary, sink: &dyn ProgressSink) {
    for (index, outcome) in summary.outcomes.iter().enumerate() {
        // StepStatus 是三档语义，界面真正要展示的是 outcome.status（success/already/fail/skip）
        let st = match outcome.status {
            "success" => switcher::StepStatus::Ok,
            "already" => switcher::StepStatus::Skip,
            "skip" => switcher::StepStatus::Skip,
            _ => switcher::StepStatus::Error,
        };
        sink.step(
            "checkin:account",
            st,
            &format!(
                "{}|{}|{}|{}",
                index, outcome.user_id, outcome.status, outcome.message
            ),
        );
    }
}

/// 执行一轮 Trae 签到。
///
/// 每个账号签到完立刻经 `sink` 送一条进度（stage 固定为 `checkin`），
/// 宿主据此实时刷新列表；`start` / `done` 两条用 `stage` 区分。
pub async fn trae_checkin_run(
    user_ids: Option<Vec<String>>,
    retry: Option<u32>,
    sink: &dyn ProgressSink,
) -> Result<Value, String> {
    let accounts: Vec<Value> = match user_ids.filter(|ids| !ids.is_empty()) {
        Some(ids) => trae_account::load_accounts()
            .into_iter()
            .filter(|a| {
                a.get("user_id")
                    .and_then(Value::as_str)
                    .is_some_and(|uid| ids.iter().any(|i| i == uid))
            })
            .collect(),
        None => trae_account::load_accounts(),
    };
    if accounts.is_empty() {
        return Err("没有可签到的 Trae 账号".to_string());
    }

    sink.step(
        "checkin:start",
        switcher::StepStatus::Info,
        &format!("共 {} 个账号", accounts.len()),
    );

    let summary = trae_checkin::run_round(&accounts, retry.unwrap_or(1)).await;

    // 逐账号推送进度（前端据此实时刷新列表）
    push_checkin_outcomes(&summary, sink);

    // ---- 失败重试轮 ----
    //
    // 只重试这一轮真失败的账号，间隔逐轮加长（30s / 90s）。为什么值得等：
    // 上游抖动导致的失败占相当比例，重试能把「当天没签到」变成「晚两分钟签到」。
    //
    // 两类**不**重试：
    // - 会话永久失效（JWT 被吊销）：再打必然还是 401，白等 30+90 秒还加重风控；
    // - 业务失败（如已签到、次数用尽）：服务端已明确答复，重试只会再失败一次。
    let mut summary = summary;
    for (round, delay) in RETRY_DELAYS.iter().enumerate() {
        let round = round + 1;
        let retryable: Vec<Value> = summary
            .outcomes
            .iter()
            .filter(|o| o.status == "fail")
            .filter(|o| {
                // 永久失效与业务失败都不进来；只有瞬时故障（Server/Client/网络）值得重试
                !matches!(
                    o.error_type.as_deref(),
                    Some("SessionDead") | Some("Business") | Some("AlreadyChecked")
                )
            })
            .filter_map(|o| {
                accounts
                    .iter()
                    .find(|a| a.get("user_id").and_then(Value::as_str) == Some(o.user_id.as_str()))
                    .cloned()
            })
            .collect();
        if retryable.is_empty() {
            break;
        }

        sink.step(
            "checkin:retry",
            switcher::StepStatus::Info,
            &format!("第 {round} 轮重试：{} 个账号，{delay} 秒后开始", retryable.len()),
        );
        sink.step(
            "checkin:retry-delay",
            switcher::StepStatus::Info,
            &round.to_string(),
        );
        tokio::time::sleep(std::time::Duration::from_secs(*delay)).await;

        let retry_summary = trae_checkin::run_round(&retryable, 0).await;

        // 用重试结果**覆盖**同 uid 的旧结果：否则界面会同时显示「失败」与
        // 「成功」两条记录，用户不知道该信哪个
        for outcome in retry_summary.outcomes {
            match summary
                .outcomes
                .iter_mut()
                .find(|o| o.user_id == outcome.user_id)
            {
                Some(slot) => *slot = outcome,
                None => summary.outcomes.push(outcome),
            }
        }
        // 计数按覆盖后的最终状态重算，避免出现「失败 3 / 成功 2」但总数只有 4 个
        summary.recount();
        push_checkin_outcomes(&summary, sink);
    }

    let result = summary.to_json();

    // 按日落库（同 uid 覆盖，重试轮自然合并为最终态），供「签到成功率趋势」查询。
    // 跳过的不记：skip 是「冷却中本轮没打请求」，不是一次失败，
    // 记进去会让趋势图上出现一堆本不该存在的失败。
    trae_checkin_results::record_today(
        summary
            .outcomes
            .iter()
            .filter(|o| o.status != "skip")
            .map(|o| (o.user_id.clone(), o.name.clone(), o.status.to_string())),
    );

    sink.step(
        "checkin:done",
        switcher::StepStatus::Ok,
        &format!(
            "成功 {} / 已签 {} / 失败 {} / 跳过 {}",
            summary.ok, summary.already, summary.failed, summary.skipped
        ),
    );
    Ok(result)
}

/// 签到成功率趋势（最近 N 天，按日期升序）。
pub fn trae_checkin_trends(days: Option<u32>) -> Value {
    trae_checkin_results::trends(days)
}

/// 积分消耗历史（官方会话级用量，按本地日聚合）。
///
/// `fresh = false` 时零网络请求，只回缓存 —— 界面切页不该每次都打上游。
pub async fn trae_usage_history(fresh: bool) -> Value {
    trae_usage_history::fetch_usage_history(fresh).await
}

// ---------------------------------------------------------------------------
// 账号分组（Trae / 豆包 分域）
// ---------------------------------------------------------------------------

/// 某应用的分组视图（定义 + 成员 + 计数）。
pub fn groups_list(app: &str) -> Value {
    account_groups::list_view(app)
}

/// 新建分组，返回新分组 id。
pub fn group_create(app: &str, name: &str, color: &str) -> Result<Value, String> {
    let id = account_groups::create_group(app, name, color)?;
    Ok(json!({ "ok": true, "id": id, "groups": account_groups::list_view(app) }))
}

/// 更新分组。
pub fn group_update(
    app: &str,
    id: &str,
    name: Option<&str>,
    color: Option<&str>,
    order: Option<i32>,
) -> Result<Value, String> {
    account_groups::update_group(app, id, name, color, order)?;
    Ok(json!({ "ok": true, "groups": account_groups::list_view(app) }))
}

/// 删除分组（连带清成员映射）。
pub fn group_delete(app: &str, id: &str) -> Result<Value, String> {
    account_groups::delete_group(app, id)?;
    Ok(json!({ "ok": true, "groups": account_groups::list_view(app) }))
}

/// 把账号移入/移出分组（`group_id = None` 表示移出）。
pub fn group_move(app: &str, user_id: &str, group_id: Option<&str>) -> Result<Value, String> {
    account_groups::move_account(app, user_id, group_id)?;
    Ok(json!({ "ok": true, "groups": account_groups::list_view(app) }))
}

// ---------------------------------------------------------------------------
// Trae 凭证续期 / 积分
// ---------------------------------------------------------------------------

/// 刷新单个 Trae 账号的 `Cloud-IDE-JWT`。
///
/// 流程：惰性门（新鲜就不刷，避免 `refresh_token` 轮换互相踢下线）
/// → `ExchangeToken` → 成功写回并解冻 / 失败按分类记冷却。
pub async fn trae_refresh_account(user_id: &str, force: bool) -> Result<Value, String> {
    let acc = trae_account::find_account(user_id)
        .ok_or_else(|| format!("账号 {user_id} 不存在"))?;
    let jwt = acc.get("jwt").and_then(Value::as_str).unwrap_or("");
    let refresh_token = acc
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if refresh_token.is_empty() {
        return Err(
            "该账号没有 refresh_token（只能手动粘贴 JWT 或重新 OAuth 登录）".to_string(),
        );
    }
    if acc
        .get("refresh_token_invalid")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err("该账号的 refresh_token 已被判定失效，请重新登录".to_string());
    }

    let now = chrono::Utc::now().timestamp();
    let rt_exp = acc.get("refresh_token_expires_at").and_then(Value::as_i64);
    if !force && !trae_refresh::lazy_refresh_needed(jwt, rt_exp, now) {
        let info = trae_account::parse_jwt(jwt);
        return Ok(trae_refresh::refresh_result_json(
            user_id,
            true,
            "JWT 仍然新鲜，跳过刷新（避免 refresh_token 轮换互相踢下线）",
            info.exp_hours,
        ));
    }

    match trae_refresh::exchange_token(&refresh_token).await {
        Ok((access, new_rt, rt_expires)) => {
            trae_refresh::apply_refresh_success(
                user_id,
                &access,
                new_rt.as_deref(),
                rt_expires,
            )?;
            let info = trae_account::parse_jwt(&access);
            Ok(trae_refresh::refresh_result_json(
                user_id,
                true,
                "JWT 已刷新",
                info.exp_hours,
            ))
        }
        Err(e) => {
            // 传输层失败（网络/代理）与「服务端明确拒绝」走不同分类：
            // 前者只计数，连续 3 次才兜底置失效；后者立即置失效。
            let kind = if e.starts_with("服务端拒绝") {
                trae_refresh::RefreshFailureKind::Rejected
            } else {
                trae_refresh::RefreshFailureKind::Transient
            };
            let (fails, invalid) = trae_refresh::record_refresh_failure(user_id, kind);
            Err(if invalid {
                format!("{e}（已连续失败 {fails} 次，refresh_token 判定失效，请重新登录）")
            } else {
                format!("{e}（连续失败 {fails} 次）")
            })
        }
    }
}

/// 批量刷新全部 Trae 账号的 JWT，返回 `{ok, failed, skipped, results}`。
pub async fn trae_refresh_all() -> Value {
    let accounts = trae_account::load_accounts();
    let mut ok = 0usize;
    let mut failed = 0usize;
    let mut results = Vec::new();
    for acc in &accounts {
        let Some(uid) = acc.get("user_id").and_then(Value::as_str) else {
            continue;
        };
        match trae_refresh_account(uid, false).await {
            Ok(v) => {
                ok += 1;
                results.push(v);
            }
            Err(e) => {
                failed += 1;
                results.push(json!({ "userId": uid, "ok": false, "message": e }));
            }
        }
    }
    json!({ "ok": ok, "failed": failed, "total": accounts.len(), "results": results })
}

/// 读取单个账号的三条积分账（IDE / 权益包 / 付费身份）。
pub async fn trae_credit_detail(user_id: &str) -> Value {
    trae_credits::fetch_credit_detail(user_id).await
}

/// 刷新全部账号的付费身份缓存。
pub async fn trae_refresh_pay_status() -> Value {
    let ok = trae_credits::refresh_all_pay_status().await;
    json!({ "ok": ok, "cache": trae_credits::load_pay_status_cache() })
}

/// 读取付费身份缓存。
pub fn trae_pay_status_cache() -> Value {
    trae_credits::load_pay_status_cache()
}

// ---------------------------------------------------------------------------
// 豆包
// ---------------------------------------------------------------------------

/// 豆包账号列表（凭证已脱敏）。
///
/// 合并快照槽信息：账号行需要展示「有没有快照 / 多大 / 几个文件 / 最后修改」，
/// 而快照是按 uid 命名的独立目录，与账号池是两份数据。分两次请求让前端自己
/// join 会漏掉「只有快照、没有池条目」的账号（那种账号在列表里根本不会出现，
/// 但它的磁盘占用是真实存在的）。
pub fn doubao_list_accounts() -> Value {
    let app = TargetApp::Doubao;
    let prof = profile_for(app, &config::store_dir());
    let current = switcher::get_current_account(&switcher::Session::new(&switcher::RunArgs {
        action: switcher::Action::BackupCurrent,
        target_app: app,
        user_id: None,
        proxy_port: None,
        include_indexeddb: false,
        expected_current_uid: String::new(),
        store_dir: config::store_dir(),
    }));

    let mut accounts: Vec<Value> = doubao_account::load_accounts()
        .iter()
        .map(|acc| {
            let uid = acc.get("user_id").and_then(Value::as_str).unwrap_or("");
            let mut view = doubao_account::account_view(acc);
            merge_snapshot_stats(&mut view, &prof, uid, uid == current);
            view
        })
        .collect();

    // 只有快照、没有池条目的账号也补一行，否则它占的空间在界面上无法解释
    let known: std::collections::HashSet<String> = accounts
        .iter()
        .filter_map(|a| a.get("userId").and_then(Value::as_str).map(str::to_string))
        .collect();
    for (uid, stats) in snapshot_slots(&prof) {
        if known.contains(&uid) {
            continue;
        }
        accounts.push(json!({
            "userId": uid,
            "name": uid,
            "hasSessionId": false,
            "sessionState": "none",
            "expiryTier": "unknown",
            "orphanSnapshot": true,
            "sizeBytes": stats.0,
            "fileCount": stats.1,
            "lastModified": stats.2,
            "hasSnapshot": true,
            "isCurrent": uid == current,
        }));
    }

    json!({
        "accounts": accounts,
        "lastKeepaliveAt": doubao_account::last_keepalive_at(),
        "currentUserId": current,
    })
}

/// 扫描快照槽位 → `{uid: (sizeBytes, fileCount, lastModified)}`。
///
/// 跳过 `last`（安全槽）与 `*.bak`（上一代备份）：它们不是账号快照，
/// 计进统计会让「账号数量」与「磁盘占用」都对不上。
fn snapshot_slots(
    prof: &app_profile::AppProfile,
) -> std::collections::BTreeMap<String, (u64, u64, Option<i64>)> {
    let mut out = std::collections::BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(&prof.profiles_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "last" || name.ends_with(".bak") {
            continue;
        }
        let (size, files) = dir_stats(&entry.path());
        let modified = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        out.insert(name, (size, files, modified));
    }
    out
}

/// 递归统计目录的 `(总字节数, 文件数)`。
fn dir_stats(path: &std::path::Path) -> (u64, u64) {
    let mut size = 0u64;
    let mut files = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return (0, 0);
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            let (s, f) = dir_stats(&entry.path());
            size += s;
            files += f;
        } else {
            size += meta.len();
            files += 1;
        }
    }
    (size, files)
}

/// 把快照统计并入账号视图（无快照时给 `false`/`0`，不伪造）。
fn merge_snapshot_stats(
    view: &mut Value,
    prof: &app_profile::AppProfile,
    uid: &str,
    is_current: bool,
) {
    let Some(obj) = view.as_object_mut() else { return };
    let slot = prof.slot_dir(uid);
    let has = slot.is_dir();
    let (size, files) = if has { dir_stats(&slot) } else { (0, 0) };
    let modified = if has {
        std::fs::metadata(&slot)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
    } else {
        None
    };
    obj.insert("hasSnapshot".to_string(), json!(has));
    obj.insert("sizeBytes".to_string(), json!(size));
    obj.insert("fileCount".to_string(), json!(files));
    obj.insert("lastModified".to_string(), json!(modified));
    obj.insert("isCurrent".to_string(), json!(is_current));
}

/// 读取账号快照的版本元数据（`snapshot_meta.json`）。
///
/// 无 meta 文件时返回 `schemaVersion: 0` 并尝试从槽内 `Last Version` 补内核版本 ——
/// 「没有元数据」与「元数据说版本是 0」在界面上要能区分（前者是旧快照/手工拷贝）。
pub fn doubao_snapshot_meta(user_id: &str) -> Result<Value, String> {
    config::ensure_uid_safe(user_id)?;
    let sess = switcher::Session::new(&switcher::RunArgs {
        action: switcher::Action::BackupCurrent,
        target_app: TargetApp::Doubao,
        user_id: None,
        proxy_port: None,
        include_indexeddb: false,
        expected_current_uid: String::new(),
        store_dir: config::store_dir(),
    });
    let slot = sess.prof.slot_dir(user_id);
    if !slot.is_dir() {
        return Err(format!("账号 {user_id} 没有快照"));
    }
    let meta = switcher::chromium::snapshot_meta(&sess, user_id);
    let chromium_version = std::fs::read_to_string(slot.join("Last Version"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok(match meta {
        Some(m) => json!({
            "userId": user_id,
            "schemaVersion": m.get("schemaVersion").cloned().unwrap_or(json!(1)),
            "createdAt": m.get("createdAt").cloned().unwrap_or(Value::Null),
            "chromiumVersion": m
                .get("chromiumVersion")
                .cloned()
                .or_else(|| chromium_version.clone().map(Value::String))
                .unwrap_or(Value::Null),
            "includeIndexedDB": m.get("includeIndexedDB").cloned().unwrap_or(json!(false)),
            "hasMeta": true,
        }),
        None => json!({
            "userId": user_id,
            "schemaVersion": 0,
            "createdAt": Value::Null,
            "chromiumVersion": chromium_version,
            "includeIndexedDB": false,
            "hasMeta": false,
        }),
    })
}

/// 探测当前登录的豆包 uid（供「保存当前登录态」预填）。
///
/// 三源按可靠性排序，任一命中即返回：
/// 1. 代理抓包落盘的 uid（`doubao_captured_credential.json`）—— 唯一由服务端
///    真实会话佐证的来源；
/// 2. 客户端 `Local State` 的 `profile.info_cache[*].saman`；
/// 3. `current_account.txt`（本项目自己写的标记）。
///
/// 参考项目还有「解密客户端 Cookies」与「手写 leveldb 解析」两源，本仓库不移植：
/// 前者解出来仍是二次加密的密文（客户端对 cookie 值又加了一层），后者是约 300 行
/// 易碎的二进制解析。两者都属于「成本高、结果不可用」。
pub fn doubao_detect_uid() -> Value {
    let mut sources: Vec<Value> = Vec::new();

    if let Some(captured) = doubao_account::load_captured() {
        if let Some(uid) = captured
            .get("uid")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        {
            sources.push(json!({ "source": "captured", "uid": uid }));
        }
    }

    let prof = profile_for(TargetApp::Doubao, &config::store_dir());
    if let Some(uid) = detect_uid_from_local_state(&prof.data_dir) {
        sources.push(json!({ "source": "local_state", "uid": uid }));
    }

    let sess = switcher::Session::new(&switcher::RunArgs {
        action: switcher::Action::BackupCurrent,
        target_app: TargetApp::Doubao,
        user_id: None,
        proxy_port: None,
        include_indexeddb: false,
        expected_current_uid: String::new(),
        store_dir: config::store_dir(),
    });
    let marker = switcher::get_current_account(&sess);
    if !marker.trim().is_empty() {
        sources.push(json!({ "source": "current_marker", "uid": marker }));
    }

    let uid = sources
        .first()
        .and_then(|s| s.get("uid").cloned())
        .unwrap_or(Value::Null);
    json!({ "uid": uid, "sources": sources })
}

/// 从客户端 `Local State` 的 `profile.info_cache` 里挑当前登录 uid。
///
/// 挑选策略：优先 `last_active_profiles` 里列出的 profile，否则取 `active_time`
/// 最大的那个 —— 多开/多 profile 时「最近活跃」才代表用户现在用的是谁。
fn detect_uid_from_local_state(data_dir: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(data_dir.join("Local State")).ok()?;
    let root: Value = serde_json::from_str(&text).ok()?;
    let cache = root.get("profile")?.get("info_cache")?.as_object()?;

    let preferred = root
        .get("profile")
        .and_then(|p| p.get("last_active_profiles"))
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(Value::as_str)
        .map(str::to_string);

    let pick = |key: &str| -> Option<String> {
        cache
            .get(key)?
            .get("saman")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    if let Some(k) = preferred {
        if let Some(uid) = pick(&k) {
            return Some(uid);
        }
    }
    cache
        .iter()
        .filter_map(|(k, v)| {
            let ts = v.get("active_time").and_then(Value::as_f64).unwrap_or(0.0);
            pick(k).map(|uid| (ts, uid))
        })
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(_, uid)| uid)
}

/// 删除豆包账号（可选连带删除其登录态快照）。
///
/// 默认 `delete_snapshot = true`：只删账号不删快照会留下**孤儿快照**，
/// 它既占磁盘，又会在下次列表里以「只有快照没有账号」的形态复现，
/// 用户会以为删除没生效。
pub fn doubao_delete_account(user_id: &str, delete_snapshot: bool) -> Result<Value, String> {
    doubao_account::delete_account(user_id)?;
    let mut snapshot_removed = false;
    if delete_snapshot {
        let prof = profile_for(TargetApp::Doubao, &config::store_dir());
        let slot = prof.slot_dir(user_id);
        if slot.is_dir() {
            std::fs::remove_dir_all(&slot).map_err(|e| format!("删除快照失败: {e}"))?;
            snapshot_removed = true;
        }
        let _ = std::fs::remove_dir_all(prof.profiles_dir.join(format!("{user_id}.bak")));
    }
    Ok(json!({ "ok": true, "snapshotRemoved": snapshot_removed }))
}

/// 一键以该账号打开豆包客户端（恢复快照 → 拉起客户端）。
///
/// 与「切换账号」是同一件事，但语义面向用户：用户想的是「用这个账号打开豆包」，
/// 而不是「执行一次 switch 动作」。代理运行中时由 `switcher` 自动注入
/// `--proxy-server`（见 `switcher::proc::start_app`）。
pub fn doubao_open_as_account(
    user_id: &str,
    proxy_port: Option<u16>,
    sink: &dyn ProgressSink,
) -> Result<Value, String> {
    config::ensure_uid_safe(user_id)?;
    let prof = profile_for(TargetApp::Doubao, &config::store_dir());
    if !prof.slot_dir(user_id).is_dir() {
        return Err(format!(
            "账号 {user_id} 没有可用快照，请先「保存当前登录态」再以该账号打开"
        ));
    }
    let collector = CollectingSink {
        inner: sink,
        steps: std::sync::Mutex::new(Vec::new()),
    };
    let outcome = switcher::run_action(
        switcher::RunArgs {
            action: switcher::Action::Switch,
            target_app: TargetApp::Doubao,
            user_id: Some(user_id.to_string()),
            proxy_port,
            include_indexeddb: config::app_setting_bool("doubao_snapshot_include_idb", false),
            expected_current_uid: String::new(),
            store_dir: config::store_dir(),
        },
        &collector,
    );
    let steps = collector.steps();
    Ok(json!({
        "ok": outcome.is_ok(),
        "message": outcome.as_deref().unwrap_or_default(),
        "error": outcome.as_ref().err().cloned().unwrap_or_default(),
        "steps": steps,
    }))
}

/// 拉起客户端（**不**切账号、不备份、不关进程）。
///
/// 对应参考项目的 `open_trae_app` / `open_doubao_app`：用户只是想打开客户端。
/// 走 [`Action::Switch`] 会顺带备份现场并恢复目标快照，那是另一件事。
pub fn app_launch(
    target_app: &str,
    proxy_port: Option<u16>,
    sink: &dyn ProgressSink,
) -> Result<Value, String> {
    struct SinkProxy<'a>(&'a dyn ProgressSink);
    impl switcher::ProgressSink for SinkProxy<'_> {
        fn step(&self, stage: &str, status: switcher::StepStatus, message: &str) {
            self.0.step(stage, status, message);
        }
    }

    let app = TargetApp::parse(target_app);
    let outcome = switcher::run_action(
        switcher::RunArgs {
            action: switcher::Action::LaunchOnly,
            target_app: app,
            user_id: None,
            proxy_port,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            store_dir: config::store_dir(),
        },
        &SinkProxy(sink),
    );
    match outcome {
        Ok(message) => Ok(json!({ "ok": true, "message": message })),
        Err(e) => Err(e),
    }
}

/// 新增/更新豆包账号。
pub fn doubao_save_account(
    user_id: &str,
    name: Option<&str>,
    note: Option<&str>,
) -> Result<Value, String> {
    let acc = doubao_account::upsert_account(user_id, name, note, true)?;
    Ok(json!({ "ok": true, "account": doubao_account::account_view(&acc) }))
}

/// 删除豆包账号（**不**连带快照；保留给需要单独控制快照的调用方）。
pub fn doubao_delete_account_only(user_id: &str) -> Result<Value, String> {
    doubao_account::delete_account(user_id)?;
    Ok(json!({ "ok": true }))
}

/// 读取账号的**明文**凭证（仅供编辑弹窗回填；界面需自行脱敏展示）。
pub fn doubao_get_credential(user_id: &str) -> Result<Value, String> {
    let acc = doubao_account::find_account(user_id).ok_or_else(|| "账号不存在".to_string())?;
    Ok(json!({
        "userId": user_id,
        "sessionId": acc.get("session_id"),
        "sidGuard": acc.get("sid_guard"),
        "ttwid": acc.get("ttwid"),
    }))
}

/// 设置账号凭证。
pub fn doubao_set_credential(
    user_id: &str,
    session_id: Option<&str>,
    sid_guard: Option<&str>,
    ttwid: Option<&str>,
) -> Result<Value, String> {
    let acc = doubao_account::apply_credential(
        user_id, session_id, sid_guard, ttwid, "manual", true,
    )?;
    Ok(json!({ "ok": true, "account": doubao_account::account_view(&acc) }))
}

/// 读取最近一次抓包凭证（供「从代理抓包自动填充」按钮）。
pub fn doubao_captured_credential() -> Value {
    match doubao_account::load_captured() {
        Some(captured) => {
            let sid = captured
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            json!({
                "available": true,
                "uid": captured.get("uid"),
                "host": captured.get("host"),
                "capturedAt": captured.get("captured_at"),
                // 回填按钮需要明文才能填进输入框；仅本机、仅此一处
                "sessionId": sid,
                "sidGuard": captured.get("sid_guard"),
                "ttwid": captured.get("ttwid"),
            })
        }
        None => json!({ "available": false }),
    }
}

/// 把抓包凭证回写到账号池（幂等：无变化返回 `applied: false`）。
pub fn doubao_credential_auto_apply() -> Result<Value, String> {
    match doubao_account::auto_apply_captured()? {
        Some(account) => Ok(json!({ "applied": true, "account": account })),
        None => Ok(json!({ "applied": false })),
    }
}

/// 会话保活（启动客户端 → 等待 → 关闭，触发服务端滑动续期）。
///
/// 进度经 `sink` 送出（stage 固定为 `keepalive`）。成功时刷新池级保活时间戳。
pub fn doubao_keepalive(sink: &dyn ProgressSink) -> Result<Value, String> {
    struct SinkProxy<'a>(&'a dyn ProgressSink);
    impl switcher::ProgressSink for SinkProxy<'_> {
        fn step(&self, stage: &str, status: switcher::StepStatus, message: &str) {
            self.0.step(stage, status, message);
        }
    }

    let args = switcher::RunArgs {
        action: switcher::Action::KeepAlive,
        target_app: TargetApp::Doubao,
        user_id: None,
        proxy_port: None,
        include_indexeddb: false,
        expected_current_uid: String::new(),
        store_dir: config::store_dir(),
    };
    let outcome = switcher::run_action(args, &SinkProxy(sink));
    if outcome.is_ok() {
        let _ = doubao_account::set_last_keepalive(&config::utc_iso());
    }
    // 健康史：保活是静默后台动作，不记事件的话用户看不到它跑没跑、跑成没跑成
    let _ = doubao_health::record_keepalive(outcome.is_ok(), outcome.as_deref().unwrap_or("保活失败"));
    outcome.map(|message| json!({ "ok": true, "message": message }))
}

/// HTTP 续期探活（诊断/续期；`sync_only` 为真时只做诊断）。
///
/// `sync_only` 与 `fallback_to_keepalive` 的区别：前者是**只看看不动手**，
/// 后者是「HTTP 路子走不通就换成拉起客户端」。
pub async fn doubao_renew(sync_only: bool) -> Value {
    doubao_renew_with(sync_only, false).await
}

/// [`doubao_renew`] 的完整形态：可选在 HTTP 续期全军覆没时回退到客户端保活。
///
/// 为什么需要回退：HTTP 续期依赖本地凭证池里的 sessionid 有效。凭证一旦被
/// 客户端刷新过、而池里还是旧值，HTTP 路径会整批失败 —— 此时**唯一**还能
/// 续上会话的办法是拉起客户端让它自己刷新。不回退的话用户看到的是「续期全失败」，
/// 而实际上换个路子就能救回来。
pub async fn doubao_renew_with(sync_only: bool, fallback_to_keepalive: bool) -> Value {
    let summary = doubao_session::run_renewal(sync_only).await;
    let mut json = summary.to_json();
    // 诊断模式不算一次运维动作，不记事件（否则健康卡会被手动诊断刷满）
    if !sync_only {
        let _ = doubao_health::record_renew(&json);
    }

    if sync_only || !fallback_to_keepalive {
        return json;
    }

    // 判定「全军覆没」：一个成功的都没有，而且确实有账号可续
    let ok = json.get("ok").and_then(Value::as_i64).unwrap_or(0);
    let total = json.get("total").and_then(Value::as_i64).unwrap_or(0);
    if ok > 0 || total == 0 {
        return json;
    }

    let message = match doubao_keepalive(&NullSink) {
        Ok(v) => {
            let detail = v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("已拉起客户端")
                .to_string();
            format!("HTTP 续期全部失败，已回退到客户端保活：{detail}")
        }
        Err(e) => format!("HTTP 续期全部失败，且客户端保活也失败：{e}"),
    };
    // 回退结果必须如实写回响应：否则界面只看到「续期失败」，
    // 不知道后台其实已经用另一条路救过了
    if let Some(obj) = json.as_object_mut() {
        obj.insert("fallbackUsed".to_string(), json!(true));
        obj.insert("fallbackMessage".to_string(), json!(message));
    }
    json
}

/// 会话与凭证诊断。
pub fn doubao_diagnose() -> Value {
    doubao_session::diagnose()
}

/// 豆包运维健康史（事件 + 14 天额度趋势 + 7 天健康计数）。
pub fn doubao_history(days: Option<i64>) -> Value {
    doubao_health::snapshot(days.unwrap_or(30))
}

/// 豆包相关设置的键与默认值。
///
/// 只暴露真正影响行为的开关；把 `app_settings.json` 整个暴露给前端会让
/// 界面能改到别的模块的键，边界就没了。
const DOUBAO_SETTING_KEYS: [(&str, bool); 1] = [("doubao_snapshot_include_idb", false)];

/// 读取豆包设置（当前只有「快照是否包含 IndexedDB」）。
pub fn doubao_settings() -> Value {
    let mut out = serde_json::Map::new();
    for (key, default) in DOUBAO_SETTING_KEYS {
        out.insert(
            key.to_string(),
            json!(config::app_setting_bool(key, default)),
        );
    }
    json!(out)
}

/// 写入一项豆包设置。
pub fn doubao_set_setting(key: &str, value: bool) -> Result<Value, String> {
    if !DOUBAO_SETTING_KEYS.iter().any(|(k, _)| *k == key) {
        return Err(format!("未知的豆包设置项：{key}"));
    }
    config::set_app_setting(key, json!(value)).map_err(|e| format!("写入设置失败：{e}"))?;
    Ok(doubao_settings())
}

/// 查询单个账号的会员额度（并写回额度缓存，列表无需重复查询）。
pub async fn doubao_fetch_quota(user_id: &str) -> Result<Value, String> {
    let parsed = doubao_quota::fetch_single(user_id).await?;
    let view = doubao_quota::to_view(user_id, &parsed);
    write_quota_cache(user_id, &parsed);
    let _ = doubao_health::record_quota(
        true,
        user_id,
        parsed.level.as_deref().unwrap_or(""),
        &parsed.summary(),
        &parsed.windows,
    );
    Ok(view)
}

/// 把一次额度查询结果写回账号缓存。
///
/// `quota_used_percent` 必须从 `parsed.windows` 取「当前时段」窗口的 `usedPercent`：
/// `parsed.items` 是宽松兜底路径（只保证等级/名称），**不含百分比** ——
/// 从那里取会恒为 `None`，字段变成死值。
pub fn write_quota_cache(user_id: &str, parsed: &doubao_quota::ParsedQuota) {
    let level = parsed.level.clone();
    let expire = parsed.expire_at.clone();
    let summary = parsed.summary();
    let used = doubao_health::current_window_used_percent(&parsed.windows);
    let _ = doubao_account::mutate_account(user_id, |obj| {
        obj.insert("quota_level".to_string(), json!(level));
        obj.insert("quota_expire_at".to_string(), json!(expire));
        obj.insert("quota_summary".to_string(), json!(summary));
        obj.insert("quota_checked_at".to_string(), json!(config::utc_iso()));
        obj.insert("quota_used_percent".to_string(), json!(used));
    });
}

/// 批量巡检全部账号的额度。
pub async fn doubao_quota_batch() -> Value {
    doubao_quota::run_batch().await
}

/// 账号会话探活。
pub async fn doubao_probe_account(user_id: &str) -> Value {
    doubao_quota::probe_account(user_id).await
}

/// 备份账号的客户端对话状态。
pub fn doubao_backup_chatdata(user_id: &str) -> Result<Value, String> {
    doubao_chats::backup_chatdata(user_id)
}

/// 恢复账号的客户端对话状态。
pub fn doubao_restore_chatdata(user_id: &str) -> Result<Value, String> {
    doubao_chats::restore_chatdata(user_id)
}

/// 查询账号的对话备份信息。
pub fn doubao_chatdata_info(user_id: &str) -> Value {
    doubao_chats::chatdata_info(user_id)
}

/// 从官方 IM API 导出对话（markdown + json）。
pub async fn doubao_export_chats(
    user_id: &str,
    limit_convs: Option<usize>,
    max_pages: Option<usize>,
) -> Result<Value, String> {
    doubao_chats::export_account(
        user_id,
        limit_convs.unwrap_or(50),
        max_pages.unwrap_or(10),
    )
    .await
}

// ---------------------------------------------------------------------------
// 计划任务（Windows schtasks）
// ---------------------------------------------------------------------------

/// 把界面传来的任务标识解析成 [`scheduler::TaskKind`]。
///
/// 界面可能传启动器名（`trae_checkin`）或计划任务名（`AIGateway_TraeCheckin`），
/// 两者都接受 —— 前者是前端常量，后者便于用户核对已注册的任务。
pub fn parse_task_kind(kind: &str) -> Result<scheduler::TaskKind, String> {
    scheduler::TaskKind::ALL
        .iter()
        .find(|k| k.launcher_name() == kind.trim() || k.task_name() == kind.trim())
        .copied()
        .ok_or_else(|| format!("未知任务: {kind}"))
}

/// 查询全部计划任务的注册状态。
pub fn task_status() -> Value {
    // schtasks 是子进程调用（可达数秒），调用方必须放进 blocking 线程。
    json!({ "tasks": scheduler::all_task_status() })
}

/// 注册（或覆盖）一个每日计划任务。
///
/// `exe` 是执行任务的主程序：桌面端传自己的 exe，webui 传 `ai-gateway` 服务
/// 本体（它支持 `--task-run <key>`）。两者都走同一个 `cli_task::run_cli_task`。
pub fn task_register(kind: &str, time: &str, exe: &std::path::Path) -> Result<Value, String> {
    let task = parse_task_kind(kind)?;
    let message = scheduler::register_daily_task(task, time, exe, &config::store_dir())?;
    Ok(json!({ "ok": true, "message": message }))
}

/// 删除计划任务。
pub fn task_unregister(kind: &str) -> Result<Value, String> {
    let task = parse_task_kind(kind)?;
    scheduler::unregister_task(task)?;
    Ok(json!({ "ok": true }))
}

/// 立即执行一次任务（不依赖计划任务，用于验证配置是否正确）。
///
/// 任务内部会跑 HTTP 请求或启停客户端，必须在 blocking 线程里同步跑完。
pub fn task_run_now(kind: &str) -> Result<Value, String> {
    let task = parse_task_kind(kind)?;
    let code = cli_task::run_cli_task(task.cli_key());
    Ok(json!({ "ok": code == 0, "exitCode": code }))
}

// ---------------------------------------------------------------------------
// 应用内调度（对标参考实现的进程内 scheduler 线程）
// ---------------------------------------------------------------------------

/// 应用内调度要跑的任务（都是豆包/Trae 的日常运维动作）。
///
/// 与 `scheduler` 的 schtasks 路线并存而非替代：schtasks 在应用没开着时也能跑，
/// 但注册需要管理员权限、且用户改了设置要等下次注册才生效；应用内调度只在运行
/// 期间生效，好处是「今天该做的事立刻会做」。两条路都指向同一个 `cli_task`，
/// 因此执行语义完全一致，不会出现两套行为。
const IN_APP_TASKS: [(scheduler::TaskKind, &str); 3] = [
    (scheduler::TaskKind::DoubaoRenew, "09:10"),
    (scheduler::TaskKind::TraeCheckin, "08:30"),
    (scheduler::TaskKind::DoubaoQuota, "21:00"),
];

/// 应用内调度的状态文件（记录每个任务最近一次实际执行的**日期**）。
fn in_app_schedule_file() -> std::path::PathBuf {
    config::store_dir().join("in_app_schedule.json")
}

/// 读取 `{ "<cli_key>": "2026-09-22" }` 形态的最近执行日期表。
fn load_in_app_done() -> serde_json::Map<String, Value> {
    std::fs::read_to_string(in_app_schedule_file())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// 记录某任务今天已执行。
///
/// 写失败只影响「重启后可能重复执行一次」，不影响正确性 —— 因此不向调用方报错，
/// 但保留返回值以便上层在需要时记日志。
fn mark_in_app_done(cli_key: &str, day: &str) -> Result<(), String> {
    let mut map = load_in_app_done();
    map.insert(cli_key.to_string(), json!(day));
    let text = serde_json::to_string_pretty(&Value::Object(map))
        .map_err(|e| format!("序列化应用内调度状态失败：{e}"))?;
    config::atomic_write(&in_app_schedule_file(), &text)
        .map_err(|e| format!("写入应用内调度状态失败：{e}"))
}

/// 判断某个任务此刻是否该跑：今天还没跑过，且当前时间已过它的触发点。
///
/// `now_hhmm` 与 `today` 由调用方传入（便于测试，也避免一次循环里读多次时钟
/// 导致跨分钟判断不一致）。
fn in_app_task_due(kind: scheduler::TaskKind, now_hhmm: &str, today: &str, done: &Value) -> bool {
    let Some((_, at)) = IN_APP_TASKS.iter().find(|(k, _)| *k == kind) else {
        return false;
    };
    if done.get(kind.cli_key()).and_then(Value::as_str) == Some(today) {
        return false;
    }
    // HH:MM 是定宽字典序可比的时间串（同一补零格式下 "09:10" < "21:00" 成立）
    now_hhmm >= *at
}

/// 跑一轮应用内调度：把到点且今天未跑的任务各跑一次。
///
/// 返回本轮实际执行的任务列表（供界面/日志展示）。任务内部是网络请求，
/// 必须由调用方放进 blocking 线程。
pub fn run_in_app_due_tasks() -> Value {
    let now = chrono::Local::now();
    let now_hhmm = now.format("%H:%M").to_string();
    let today = now.format("%Y-%m-%d").to_string();
    let done = load_in_app_done();

    let mut ran: Vec<Value> = Vec::new();
    for (kind, at) in IN_APP_TASKS {
        if !in_app_task_due(kind, &now_hhmm, &today, &Value::Object(done.clone())) {
            continue;
        }
        let code = cli_task::run_cli_task(kind.cli_key());
        // 只有成功才记账：失败要留给下一轮重试，否则一次网络抖动就等于当天不再尝试
        if code == 0 {
            let _ = mark_in_app_done(kind.cli_key(), &today);
        }
        ran.push(json!({
            "kind": kind.cli_key(),
            "label": kind.label(),
            "scheduledAt": at,
            "ok": code == 0,
            "exitCode": code,
        }));
    }

    json!({ "ran": ran, "checkedAt": config::utc_iso() })
}

/// 应用内调度的当前配置（供界面展示「哪些任务会在应用运行期间自动执行」）。
pub fn in_app_schedule_view() -> Value {
    let done = load_in_app_done();
    let tasks: Vec<Value> = IN_APP_TASKS
        .iter()
        .map(|(kind, at)| {
            json!({
                "kind": kind.cli_key(),
                "label": kind.label(),
                "at": at,
                "lastRunDay": done.get(kind.cli_key()).cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    json!({ "tasks": tasks })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 动作名解析覆盖界面全部取值() {
        for name in [
            "Switch",
            "switch",
            "SaveCurrentLogin",
            "save",
            "BackupCurrent",
            "backup",
            "RestoreOnly",
            "restore",
            "ResetDeviceIds",
            "resetDeviceIds",
            "KeepAlive",
            "keepalive",
        ] {
            assert!(parse_action(name).is_ok(), "{name} 应可解析");
        }
        assert!(parse_action("Unknown").is_err());
        assert!(parse_action("").is_err());
    }

    #[test]
    fn 环境检测对未知应用名退化而不panic() {
        // 未知名字走 TargetApp::parse 的兜底分支，不应 panic
        let _ = app_env_check("NotARealApp");
    }

    #[test]
    fn null_sink_吞掉全部进度() {
        let sink = NullSink;
        sink.step("x", switcher::StepStatus::Ok, "msg");
    }

    #[test]
    fn 豆包设置只认得白名单里的键() {
        let _iso = config::test_isolation::Isolated::new("apps_ops_doubao_settings");
        // 未知键必须报错：放行等于把整个 app_settings.json 暴露成可写
        assert!(doubao_set_setting("some_other_module_key", true).is_err());
        assert!(doubao_set_setting("", true).is_err());

        let before = doubao_settings();
        assert_eq!(before["doubao_snapshot_include_idb"], json!(false));
        assert!(doubao_set_setting("doubao_snapshot_include_idb", true).is_ok());
        assert_eq!(doubao_settings()["doubao_snapshot_include_idb"], json!(true));
    }

    #[test]
    fn 豆包设置返回体只含白名单键() {
        let _iso = config::test_isolation::Isolated::new("apps_ops_doubao_settings_shape");
        // 先塞一个不属于豆包的键，确认不会被带出去
        let _ = config::set_app_setting("unrelated_key", json!("secret"));
        let settings = doubao_settings();
        let obj = settings.as_object().expect("设置应是对象");
        assert_eq!(obj.len(), DOUBAO_SETTING_KEYS.len());
        assert!(!obj.contains_key("unrelated_key"));
    }

    #[test]
    fn jwt_解析对垃圾输入不报错只标无效() {
        // 编辑弹窗的实时预览：用户可能还没粘完，必须是 valid=false 而不是 Err
        for input in ["", "  ", "not-a-jwt", "a.b", "a.b.c"] {
            let parsed = trae_jwt_parse(input);
            assert_eq!(parsed["valid"], json!(false), "{input:?} 不该被当作有效 JWT");
        }
    }

    #[test]
    fn 编辑账号拒绝换成另一个账号的_jwt() {
        let _iso = config::test_isolation::Isolated::new("apps_ops_update_account");
        // 构造两个 uid 不同的合法 JWT（解析器读 sub / auth_id / data.id）
        let make = |uid: &str| {
            let payload = json!({ "sub": uid, "exp": 4_000_000_000i64 });
            let enc = |v: &Value| {
                base64::Engine::encode(
                    &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                    serde_json::to_vec(v).unwrap(),
                )
            };
            format!("{}.{}.sig", enc(&json!({"alg":"HS256"})), enc(&payload))
        };
        let jwt_a = make("user-aaa");
        let jwt_b = make("user-bbb");

        trae_add_account(&jwt_a, Some("A"), None).expect("应先能添加 A");
        // 用 B 的 JWT 去改 A：必须被拒绝，否则 A 的记录会被写成 B 的凭证
        let err = trae_update_account("user-aaa", None, Some(&jwt_b), None)
            .expect_err("换成别的账号的 JWT 必须报错");
        assert!(err.contains("user-bbb"), "错误信息应点明冲突的账号：{err}");

        // 同一个账号的 JWT 可以替换
        assert!(trae_update_account("user-aaa", None, Some(&jwt_a), None).is_ok());
        // 只改昵称
        let renamed = trae_update_account("user-aaa", Some("新名字"), None, None).expect("改名应成功");
        assert_eq!(renamed["account"]["name"], json!("新名字"));
    }

    #[test]
    fn 删除账号会清掉分组归属() {
        let _iso = config::test_isolation::Isolated::new("apps_ops_delete_group");
        let payload = json!({ "sub": "user-del", "exp": 4_000_000_000i64 });
        let enc = |v: &Value| {
            base64::Engine::encode(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                serde_json::to_vec(v).unwrap(),
            )
        };
        let jwt = format!("{}.{}.sig", enc(&json!({"alg":"HS256"})), enc(&payload));
        trae_add_account(&jwt, Some("待删"), None).expect("添加账号");

        let gid = account_groups::create_group("Trae", "临时组", "blue").expect("建组");
        account_groups::move_account("Trae", "user-del", Some(&gid)).expect("入组");
        assert_eq!(
            account_groups::membership("Trae").get("user-del").cloned(),
            Some(json!(gid)),
        );

        trae_delete_account("user-del").expect("删除账号");
        // 孤儿映射会让分组计数比实际账号数多，必须一并清掉
        assert!(
            !account_groups::membership("Trae").contains_key("user-del"),
            "删除账号后不该留下分组映射"
        );
    }

    #[test]
    fn 账号编辑拒绝路径穿越的_uid() {
        let _iso = config::test_isolation::Isolated::new("apps_ops_uid_guard");
        for bad in ["../evil", "a/b", "", "  "] {
            assert!(
                trae_update_account(bad, Some("x"), None, None).is_err(),
                "{bad:?} 不该被接受为 uid"
            );
        }
    }

    #[test]
    fn 应用内调度只在到点且当天未跑时触发() {
        let kind = scheduler::TaskKind::DoubaoRenew;
        let today = "2026-09-22";
        let empty = json!({});

        // 早于触发点（09:10）不跑
        assert!(!in_app_task_due(kind, "09:09", today, &empty));
        // 到点即跑
        assert!(in_app_task_due(kind, "09:10", today, &empty));
        assert!(in_app_task_due(kind, "23:59", today, &empty));
        // 今天已跑过就不再跑
        let done = json!({ "doubao-keepalive": today });
        assert!(!in_app_task_due(kind, "23:59", today, &done));
        // 昨天跑过、今天还没跑 → 要跑
        let stale = json!({ "doubao-keepalive": "2026-09-21" });
        assert!(in_app_task_due(kind, "23:59", today, &stale));
    }

    #[test]
    fn 应用内调度不含未登记的任务() {
        // 未登记的任务永远不 due —— 防止将来新增 TaskKind 时被误当成日常任务
        let empty = json!({});
        for kind in scheduler::TaskKind::ALL {
            let listed = IN_APP_TASKS.iter().any(|(k, _)| *k == kind);
            let due = in_app_task_due(kind, "23:59", "2026-09-22", &empty);
            assert_eq!(
                due, listed,
                "{} 的 due 结果与是否登记在 IN_APP_TASKS 不一致",
                kind.cli_key()
            );
        }
    }

    #[test]
    fn 应用内调度记账按_cli_键隔离() {
        let _iso = config::test_isolation::Isolated::new("apps_ops_in_app");
        // 每个任务各自记账：豆包续期跑过不该让 Trae 签到也跳过
        mark_in_app_done("doubao-keepalive", "2026-09-22").expect("记账应成功");
        let done = Value::Object(load_in_app_done());
        assert_eq!(done["doubao-keepalive"], json!("2026-09-22"));
        assert!(
            in_app_task_due(
                scheduler::TaskKind::TraeCheckin,
                "23:59",
                "2026-09-22",
                &done
            ),
            "另一个任务的记账不该影响 Trae 签到"
        );
    }

    #[test]
    fn 应用内调度视图列出全部登记任务() {
        let _iso = config::test_isolation::Isolated::new("apps_ops_in_app_view");
        let view = in_app_schedule_view();
        let tasks = view["tasks"].as_array().expect("tasks 应是数组");
        assert_eq!(tasks.len(), IN_APP_TASKS.len());
        // 每个任务都要有时间与标签，界面才能渲染
        for t in tasks {
            assert!(t["at"].as_str().is_some_and(|s| !s.is_empty()), "{t}");
            assert!(t["label"].as_str().is_some_and(|s| !s.is_empty()), "{t}");
            // 触发时间必须是合法的 HH:MM，否则字典序比较不成立
            assert!(scheduler::validate_hhmm(t["at"].as_str().unwrap()).is_ok(), "{t}");
        }
    }

    #[test]
    fn 应用内调度时间点都是合法_hhmm() {
        // 定宽字典序比较的前提：全部补零且合法
        for (kind, at) in IN_APP_TASKS {
            assert!(
                scheduler::validate_hhmm(at).is_ok(),
                "{} 的触发时间 {at} 不是合法 HH:MM",
                kind.cli_key()
            );
            assert_eq!(at.len(), 5, "{} 的时间必须是定宽 5 字符", kind.cli_key());
        }
    }

    #[test]
    fn 版本探测按优先级读取并保留来源() {        let iso = config::test_isolation::Isolated::new("apps_ops_version");
        let dir = iso.dir().to_path_buf();

        // 什么都没有 → 明确返回「探测不到」，不回退成我们自己的版本号
        assert_eq!(detect_app_version(&dir), (None, None));

        // resources/app/package.json（Electron 布局）
        let app_dir = dir.join("resources").join("app");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(app_dir.join("package.json"), r#"{"version":"1.2.3"}"#).unwrap();
        let (v, src) = detect_app_version(&dir);
        assert_eq!(v.as_deref(), Some("1.2.3"));
        assert!(src.unwrap().contains("package.json"));

        // version 文本文件优先级更高
        std::fs::write(dir.join("version"), "2.0.0\n").unwrap();
        assert_eq!(detect_app_version(&dir).0.as_deref(), Some("2.0.0"));

        // Last Version 优先级最高
        std::fs::write(dir.join("Last Version"), "3.3.100\n").unwrap();
        assert_eq!(detect_app_version(&dir).0.as_deref(), Some("3.3.100"));

        // 空文件不算命中（避免界面显示空白的版本号）
        let empty_dir = dir.join("empty");
        std::fs::create_dir_all(&empty_dir).unwrap();
        std::fs::write(empty_dir.join("version"), "\n\n").unwrap();
        assert_eq!(detect_app_version(&empty_dir), (None, None));

        // 损坏的 package.json 不 panic
        let broken = dir.join("broken").join("resources").join("app");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("package.json"), "{ not json").unwrap();
        assert_eq!(detect_app_version(&dir.join("broken")), (None, None));
    }

    /// 造一个 uid 可控的合法 JWT。
    fn make_jwt(uid: &str) -> String {
        let enc = |v: &Value| {
            base64::Engine::encode(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                serde_json::to_vec(v).unwrap(),
            )
        };
        format!(
            "{}.{}.sig",
            enc(&json!({"alg":"HS256"})),
            enc(&json!({ "sub": uid, "exp": 4_000_000_000i64 }))
        )
    }

    #[test]
    fn trae_导出导入能往返() {
        let _iso = config::test_isolation::Isolated::new("apps_ops_export_roundtrip");
        trae_add_account(&make_jwt("uid-1"), Some("一号"), Some("rt-1")).expect("加账号 1");
        trae_add_account(&make_jwt("uid-2"), Some("二号"), None).expect("加账号 2");

        let exported = trae_export_accounts(None).expect("导出应成功");
        assert_eq!(exported["count"], json!(2));
        let text = exported["text"].as_str().unwrap().to_string();
        // 导出的必须是完整凭证：这是「换台机器接着用」的前提
        assert!(text.contains("uid-1"), "导出内容应含账号");
        assert!(text.contains("rt-1"), "导出内容应含 refresh_token");

        // 清空后导入，应恢复两个账号
        trae_delete_account("uid-1").expect("删 1");
        trae_delete_account("uid-2").expect("删 2");
        assert_eq!(
            trae_list_accounts()["accounts"].as_array().unwrap().len(),
            0
        );

        let result = trae_import_accounts(&text).expect("导入应成功");
        assert_eq!(result["added"], json!(2));
        assert_eq!(result["updated"], json!(0));
        assert_eq!(
            trae_list_accounts()["accounts"].as_array().unwrap().len(),
            2
        );
    }

    #[test]
    fn trae_导入同_uid_覆盖而不是跳过() {
        let _iso = config::test_isolation::Isolated::new("apps_ops_import_overwrite");
        trae_add_account(&make_jwt("uid-ov"), Some("旧名"), Some("old-rt")).expect("加账号");

        // 造一份「另一台机器」的导出：同 uid、新名字、新 refresh_token
        let text = json!({
            "kind": "trae-accounts",
            "version": 1,
            "accounts": [{
                "user_id": "uid-ov",
                "name": "新名",
                "jwt": make_jwt("uid-ov"),
                "refresh_token": "new-rt",
            }]
        })
        .to_string();

        let result = trae_import_accounts(&text).expect("导入应成功");
        assert_eq!(result["updated"], json!(1));
        assert_eq!(result["added"], json!(0));

        // 覆盖必须真的生效：否则用户以为换了新登录态、实际还在用旧的
        let accounts = trae_list_accounts();
        let acc = &accounts["accounts"].as_array().unwrap()[0];
        assert_eq!(acc["name"], json!("新名"));
        assert!(acc["hasRefreshToken"].as_bool().unwrap());
    }

    #[test]
    fn trae_导入拒绝错误的文件类型() {
        let _iso = config::test_isolation::Isolated::new("apps_ops_import_kind");
        // WorkBuddy 的导出文件（没有 trae-accounts 标记）必须被拒绝
        let wrong = json!({ "accounts": [{ "id": "x", "jwt": "y" }] }).to_string();
        let err = trae_import_accounts(&wrong).expect_err("应拒绝");
        assert!(err.contains("trae-accounts"), "{err}");

        assert!(trae_import_accounts("not json").is_err());
        assert!(trae_preview_import(&wrong).is_err());
    }

    #[test]
    fn trae_导入跳过无效记录() {
        let _iso = config::test_isolation::Isolated::new("apps_ops_import_invalid");
        let text = json!({
            "kind": "trae-accounts",
            "version": 1,
            "accounts": [
                // 缺 jwt → 导入进来就是永远登录不了的僵尸账号，必须跳过
                { "user_id": "u-nojwt", "name": "无凭证" },
                // 路径穿越 uid
                { "user_id": "../evil", "jwt": make_jwt("x") },
                // 正常
                { "user_id": "u-ok", "jwt": make_jwt("u-ok"), "name": "正常" },
            ]
        })
        .to_string();

        let result = trae_import_accounts(&text).expect("导入应成功");
        assert_eq!(result["added"], json!(1), "{result}");
        assert_eq!(result["skipped"], json!(2), "{result}");
        assert_eq!(
            trae_list_accounts()["accounts"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn trae_导出可选账号且空选报错() {
        let _iso = config::test_isolation::Isolated::new("apps_ops_export_select");
        trae_add_account(&make_jwt("uid-a"), Some("A"), None).expect("加 A");
        trae_add_account(&make_jwt("uid-b"), Some("B"), None).expect("加 B");

        let one = trae_export_accounts(Some(vec!["uid-a".to_string()])).expect("导出应成功");
        assert_eq!(one["count"], json!(1));
        assert!(one["text"].as_str().unwrap().contains("uid-a"));
        assert!(!one["text"].as_str().unwrap().contains("uid-b"));

        // 空账号库时导出全部应返回 0 条而不是报错
        let empty = trae_export_accounts(Some(vec![])).expect("空选择=全部");
        assert_eq!(empty["count"], json!(2));

        // 选了不存在的账号要报错，而不是静默导出空文件
        assert!(trae_export_accounts(Some(vec!["nope".to_string()])).is_err());
    }

    #[test]
    fn trae_导入预览标出会被覆盖的账号() {        let _iso = config::test_isolation::Isolated::new("apps_ops_import_preview");
        trae_add_account(&make_jwt("uid-exist"), Some("已有"), None).expect("加账号");

        let text = json!({
            "kind": "trae-accounts",
            "version": 1,
            "accounts": [
                { "user_id": "uid-exist", "jwt": make_jwt("uid-exist"), "refresh_token": "r" },
                { "user_id": "uid-new", "jwt": make_jwt("uid-new") },
            ]
        })
        .to_string();

        let preview = trae_preview_import(&text).expect("预览应成功");
        assert_eq!(preview["count"], json!(2));
        let list = preview["accounts"].as_array().unwrap();
        let exist = list.iter().find(|a| a["userId"] == json!("uid-exist")).unwrap();
        let new = list.iter().find(|a| a["userId"] == json!("uid-new")).unwrap();
        // 覆盖是不可逆的，界面必须能事先标出来
        assert_eq!(exist["willOverwrite"], json!(true));
        assert_eq!(new["willOverwrite"], json!(false));
        assert_eq!(exist["hasRefreshToken"], json!(true));
    }

    #[test]
    fn 积分统计按天补齐且缺天为_null() {
        let _iso = config::test_isolation::Isolated::new("apps_ops_credits_stats");
        // 只写今天一条：其余天必须补成 null，而不是 0
        trae_credits::daily_snapshot("u1", 500, -20).expect("写快照");

        let stats = trae_credits_stats(Some(7));
        assert_eq!(stats["days"], json!(7));
        let daily = stats["daily"].as_array().expect("daily 应是数组");
        assert_eq!(daily.len(), 7, "必须按天补齐成 7 条");

        // 最后一天（今天）有数据，其余为 null
        assert_eq!(daily[6]["total"], json!(500));
        for (i, d) in daily.iter().enumerate().take(6) {
            assert!(
                d["total"].is_null(),
                "第 {i} 天没有数据时 total 必须是 null 而不是 0：{d}"
            );
            assert!(d["consumed"].is_null(), "第 {i} 天 consumed 也应是 null：{d}");
        }

        // 缺天不该被算进统计
        assert_eq!(stats["summary"]["observedDays"], json!(1));
        assert_eq!(stats["summary"]["latestTotal"], json!(500));
        assert_eq!(stats["summary"]["consumed"], json!(20));
        // 只有一天数据时无法算变化量 → null，而不是 0（0 会被读成「没变化」）
        assert!(
            stats["summary"]["change"].is_null(),
            "单天数据算不出变化量：{}",
            stats["summary"]
        );
    }

    #[test]
    fn 积分统计把充值排除在消耗之外() {
        let _iso = config::test_isolation::Isolated::new("apps_ops_credits_income");
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        // 直接构造历史：一天消耗 30，一天充值 +100
        let rows = json!([
            { "date": today, "userId": "u1", "credits": 470, "delta": -30, "updatedAt": "" },
            { "date": today, "userId": "u2", "credits": 1000, "delta": 100, "updatedAt": "" },
        ]);
        let path = trae_credits::credits_history_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, rows.to_string()).unwrap();

        let stats = trae_credits_stats(Some(3));
        // 只有负 delta 算消耗：充值那 +100 不该抵消消耗
        assert_eq!(stats["summary"]["consumed"], json!(30), "{}", stats["summary"]);
        // 总量是当天全部账号相加
        let daily = stats["daily"].as_array().unwrap();
        assert_eq!(daily[2]["total"], json!(1470));
    }

    #[test]
    fn 积分统计对空历史不报错() {
        let _iso = config::test_isolation::Isolated::new("apps_ops_credits_empty");
        let stats = trae_credits_stats(Some(5));
        assert_eq!(stats["summary"]["observedDays"], json!(0));
        assert!(stats["summary"]["latestTotal"].is_null());
        assert!(stats["summary"]["consumed"].is_null());
        assert_eq!(stats["daily"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn 积分统计天数被限制在合理区间() {
        let _iso = config::test_isolation::Isolated::new("apps_ops_credits_clamp");
        // 越界值要被夹住：传 100000 会生成 10 万条 JSON，界面直接卡死
        assert_eq!(trae_credits_stats(Some(100_000))["days"], json!(90));
        assert_eq!(trae_credits_stats(Some(0))["days"], json!(1));
        assert_eq!(trae_credits_stats(Some(-5))["days"], json!(1));
        assert_eq!(trae_credits_stats(None)["days"], json!(30));
    }

    #[test]
    fn 积分统计列出账号与最新余额() {
        let _iso = config::test_isolation::Isolated::new("apps_ops_credits_accounts");
        trae_add_account(&make_jwt("uid-credit"), Some("有积分"), None).expect("加账号");
        trae_credits::daily_snapshot("uid-credit", 888, 0).expect("写快照");

        let stats = trae_credits_stats(Some(3));
        let accounts = stats["accounts"].as_array().expect("accounts 应是数组");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0]["userId"], json!("uid-credit"));
        assert_eq!(accounts[0]["name"], json!("有积分"));
        assert_eq!(accounts[0]["credits"], json!(888));
    }

    #[test]
    fn 积分统计对损坏的历史文件不崩溃() {
        let _iso = config::test_isolation::Isolated::new("apps_ops_credits_broken");
        let path = trae_credits::credits_history_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ this is not json").unwrap();
        // 坏文件应被当成空历史，而不是 panic
        let stats = trae_credits_stats(Some(3));
        assert_eq!(stats["summary"]["observedDays"], json!(0));
    }

    #[test]
    fn 界面任务清单里的每一项都能被解析回来() {
        // 界面拿 all_task_status 的 kind（= launcher_name）回传给 task_register /
        // task_run_now / task_unregister。任何一项解析不了，界面上那个按钮就是死的。
        let status = task_status();
        let tasks = status["tasks"].as_array().expect("tasks 应是数组");
        assert_eq!(tasks.len(), scheduler::TaskKind::ALL.len(), "任务数应与枚举一致");

        for t in tasks {
            let kind = t["kind"].as_str().expect("kind 应是字符串");
            let parsed = parse_task_kind(kind)
                .unwrap_or_else(|e| panic!("界面任务 {kind} 无法解析：{e}"));
            // 计划任务名也要能解析（用户核对时可能直接复制任务名）
            parse_task_kind(parsed.task_name())
                .unwrap_or_else(|e| panic!("任务名 {} 无法解析：{e}", parsed.task_name()));
            // 每个任务都要有界面要用的 label / cliKey
            assert!(t["label"].as_str().is_some_and(|s| !s.is_empty()), "{t}");
            assert!(t["cliKey"].as_str().is_some_and(|s| !s.is_empty()), "{t}");
        }
    }

    #[test]
    fn 计划任务名与启动器名互不冲突() {        // 两者只要有一对相同，parse_task_kind 就会解析到错误的任务
        let mut names: Vec<&str> = Vec::new();
        for kind in scheduler::TaskKind::ALL {
            names.push(kind.task_name());
            names.push(kind.launcher_name());
            names.push(kind.cli_key());
        }
        let mut sorted = names.clone();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            before,
            "任务标识有重复，解析会命中错的任务：{sorted:?}"
        );
    }

    #[test]
    fn 目录统计递归累加文件与字节() {
        let _iso = config::test_isolation::Isolated::new("apps_ops_dir_stats");
        let root = config::store_dir().join("dir-stats-test");
        std::fs::create_dir_all(root.join("nested/deep")).unwrap();
        std::fs::write(root.join("a.txt"), b"12345").unwrap();
        std::fs::write(root.join("nested/b.txt"), b"123").unwrap();
        std::fs::write(root.join("nested/deep/c.txt"), b"12").unwrap();

        let (size, files) = dir_size_and_count(&root);
        assert_eq!(files, 3, "嵌套层里的文件也要数进去");
        assert_eq!(size, 10, "总字节数应是 5 + 3 + 2");
    }

    #[test]
    fn 目录统计对不存在的路径返回零而不是报错() {
        // 快照目录被删掉时列表仍要能打开，不能因为统计失败整个报错
        let (size, files) = dir_size_and_count(&config::store_dir().join("绝对不存在-xyz"));
        assert_eq!((size, files), (0, 0));
    }

    #[test]
    fn 快照列表带体积与文件数() {        let _iso = config::test_isolation::Isolated::new("apps_ops_snapshot_stats");
        let prof = profile_for(TargetApp::TraeWork, &config::store_dir());
        let slot = prof.profiles_dir.join("900100200");
        std::fs::create_dir_all(&slot).unwrap();
        std::fs::write(slot.join("storage.json"), b"abcdefghij").unwrap();
        std::fs::write(slot.join("state.vscdb"), b"xy").unwrap();

        let out = list_snapshots("trae-work").expect("列快照");
        let snaps = out["snapshots"].as_array().expect("snapshots 是数组");
        let mine = snaps
            .iter()
            .find(|s| s["userId"] == json!("900100200"))
            .expect("应能找到刚建的快照槽");
        assert_eq!(mine["sizeBytes"], json!(12), "{mine}");
        assert_eq!(mine["fileCount"], json!(2), "{mine}");
    }

    #[test]
    fn 拉起客户端是零副作用动作() {
        // LaunchOnly 不该被当成会写盘的动作：否则界面会给「打开客户端」弹备份提示
        assert!(!switcher::Action::LaunchOnly.mutates_state());
        for a in [
            switcher::Action::Switch,
            switcher::Action::SaveCurrentLogin,
            switcher::Action::BackupCurrent,
            switcher::Action::RestoreOnly,
            switcher::Action::ResetDeviceIds,
            switcher::Action::KeepAlive,
        ] {
            assert!(a.mutates_state(), "{:?} 会写盘，不应被标成零副作用", a);
        }
    }

    #[test]
    fn 动作名解析覆盖全部动作且双向可逆() {
        // 界面传字符串 → 枚举 → 字符串，必须能往返；漏一个就会让那个按钮永远报「未知动作」
        let all = [
            switcher::Action::Switch,
            switcher::Action::SaveCurrentLogin,
            switcher::Action::BackupCurrent,
            switcher::Action::RestoreOnly,
            switcher::Action::ResetDeviceIds,
            switcher::Action::KeepAlive,
            switcher::Action::LaunchOnly,
        ];
        for a in all {
            let name = a.as_str();
            let parsed = parse_action(name)
                .unwrap_or_else(|e| panic!("动作 {name} 无法解析回来：{e}"));
            assert_eq!(parsed, a, "动作 {name} 往返后变成了 {parsed:?}");
        }
        assert!(parse_action("不存在的动作").is_err());
    }

    #[test]
    fn 每个动作名都是唯一字符串() {
        // 两个动作同名会让 parse_action 永远解析到前一个
        let names: Vec<&str> = [
            switcher::Action::Switch,
            switcher::Action::SaveCurrentLogin,
            switcher::Action::BackupCurrent,
            switcher::Action::RestoreOnly,
            switcher::Action::ResetDeviceIds,
            switcher::Action::KeepAlive,
            switcher::Action::LaunchOnly,
        ]
        .iter()
        .map(|a| a.as_str())
        .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), before, "动作名有重复：{sorted:?}");
    }

    #[test]
    fn 拉起不存在的客户端会报错而不是静默成功() {
        let _iso = config::test_isolation::Isolated::new("apps_ops_launch_missing");
        struct NullSink2;
        impl ProgressSink for NullSink2 {
            fn step(&self, _s: &str, _st: switcher::StepStatus, _m: &str) {}
        }
        // 测试机上通常没装 Trae，找不到 exe 就该报错并给出指引；
        // 装了的话启动会成功（同样算通过，这里只断言不 panic 且返回结构正确）
        match app_launch("trae-work", None, &NullSink2) {
            Err(e) => assert!(
                e.contains("未找到") || e.contains("可执行文件") || e.contains("已有"),
                "错误信息应给出可操作指引：{e}"
            ),
            Ok(v) => assert_eq!(v["ok"], json!(true)),
        }
    }
}
