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
    app_profile::{profile_for, TargetApp},
    cli_task, config, doubao_account, doubao_chats, doubao_quota, doubao_session, scheduler,
    switcher, trae_account, trae_checkin, trae_device, trae_discover,
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
    }))
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

/// 解析动作名（界面传字符串，避免前端与 Rust 枚举耦合）。
pub fn parse_action(action: &str) -> Result<switcher::Action, String> {
    match action.trim() {
        "Switch" | "switch" => Ok(switcher::Action::Switch),
        "SaveCurrentLogin" | "save" => Ok(switcher::Action::SaveCurrentLogin),
        "BackupCurrent" | "backup" => Ok(switcher::Action::BackupCurrent),
        "RestoreOnly" | "restore" => Ok(switcher::Action::RestoreOnly),
        "ResetDeviceIds" | "resetDeviceIds" => Ok(switcher::Action::ResetDeviceIds),
        "KeepAlive" | "keepalive" => Ok(switcher::Action::KeepAlive),
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

    let collector = CollectingSink {
        inner: sink,
        steps: std::sync::Mutex::new(Vec::new()),
    };
    let args = switcher::RunArgs {
        action: act,
        target_app: app_kind,
        user_id: user_id.clone(),
        proxy_port: req.proxy_port,
        include_indexeddb: req.include_indexeddb.unwrap_or(false),
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
            }))
        })
        .collect();
    snapshots.sort_by(|a, b| {
        b.get("modifiedAt")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .cmp(&a.get("modifiedAt").and_then(Value::as_i64).unwrap_or(0))
    });
    Ok(json!({ "snapshots": snapshots, "currentUserId": current }))
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

/// Trae 账号列表（JWT 已脱敏），并把签到冷却并进元数据。
pub fn trae_list_accounts() -> Value {
    let accounts: Vec<Value> = trae_account::load_accounts()
        .iter()
        .map(trae_account::account_meta)
        .collect();
    let cooldowns = trae_checkin::all_cooldowns();
    // 界面一次请求就能渲染完整状态
    let accounts: Vec<Value> = accounts
        .into_iter()
        .map(|mut acc| {
            if let Some(uid) = acc.get("userId").and_then(Value::as_str) {
                if let Some(cd) = cooldowns.get(uid) {
                    acc["cooldown"] = cd.clone();
                }
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
    Ok(json!({ "ok": true }))
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
    let result = summary.to_json();
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

// ---------------------------------------------------------------------------
// 豆包
// ---------------------------------------------------------------------------

/// 豆包账号列表（凭证已脱敏）。
pub fn doubao_list_accounts() -> Value {
    let accounts: Vec<Value> = doubao_account::load_accounts()
        .iter()
        .map(doubao_account::account_view)
        .collect();
    json!({
        "accounts": accounts,
        "lastKeepaliveAt": doubao_account::last_keepalive_at(),
    })
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

/// 删除豆包账号。
pub fn doubao_delete_account(user_id: &str) -> Result<Value, String> {
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
    outcome.map(|message| json!({ "ok": true, "message": message }))
}

/// HTTP 续期探活（诊断/续期；`sync_only` 为真时只做诊断）。
pub async fn doubao_renew(sync_only: bool) -> Value {
    doubao_session::run_renewal(sync_only).await.to_json()
}

/// 会话与凭证诊断。
pub fn doubao_diagnose() -> Value {
    doubao_session::diagnose()
}

/// 查询单个账号的会员额度（并写回额度缓存，列表无需重复查询）。
pub async fn doubao_fetch_quota(user_id: &str) -> Result<Value, String> {
    let parsed = doubao_quota::fetch_single(user_id).await?;
    let view = doubao_quota::to_view(user_id, &parsed);
    let level = parsed.level.clone();
    let expire = parsed.expire_at.clone();
    let summary = parsed.summary();
    let _ = doubao_account::mutate_account(user_id, |obj| {
        obj.insert("quota_level".to_string(), json!(level));
        obj.insert("quota_expire_at".to_string(), json!(expire));
        obj.insert("quota_summary".to_string(), json!(summary));
        obj.insert("quota_checked_at".to_string(), json!(config::utc_iso()));
    });
    Ok(view)
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
}
