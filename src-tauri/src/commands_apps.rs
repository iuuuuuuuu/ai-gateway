//! Trae 与豆包的 Tauri 命令层。
//!
//! 这一层只做三件事：**参数校验**、**调用 core**、**把结果转成前端可用的 JSON**。
//! 所有业务逻辑都在 `ai_gateway_core::modules` 里 —— core 不依赖 Tauri，
//! 因此同一套逻辑也能被 HTTP server 形态复用。
//!
//! ## 为什么这些命令要 `async` + `spawn_blocking`
//!
//! 切换账号、备份对话、设备标识重置都会**关闭并启动客户端进程**，耗时可达数十秒。
//! Tauri 的同步命令跑在主线程上，一旦阻塞就会冻结整个窗口消息循环
//! （表现为界面卡死、窗口无法拖动）。凡是会跑子进程或做大文件拷贝的命令
//! 都必须放进 blocking 线程池。

use serde_json::{json, Value};

use ai_gateway_core::modules::{
    app_profile::{profile_for, TargetApp},
    config, doubao_account, doubao_chats, doubao_quota, doubao_session, switcher, trae_account,
    trae_checkin, trae_device, trae_discover,
};

// ---------------------------------------------------------------------------
// 环境检测
// ---------------------------------------------------------------------------

/// 应用安装/运行状态（供界面「环境配置」页）。
#[tauri::command(rename_all = "camelCase")]
pub async fn app_env_check(target_app: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let app = TargetApp::parse(&target_app);
        let store = config::store_dir();
        let prof = profile_for(app, &store);

        // exe 发现失败不是错误 —— 只是「未安装」，界面据此提示用户手动指定路径
        let exe = {
            let args = switcher::RunArgs {
                action: switcher::Action::BackupCurrent,
                target_app: app,
                user_id: None,
                proxy_port: None,
                include_indexeddb: false,
                expected_current_uid: String::new(),
                store_dir: store.clone(),
            };
            let sess = switcher::Session::new(&args);
            switcher::locate::find_exe(&sess).ok()
        };

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

        // 运行状态单独算：复用 Session 的档案，避免重复构造
        let running = {
            let args = switcher::RunArgs {
                action: switcher::Action::BackupCurrent,
                target_app: app,
                user_id: None,
                proxy_port: None,
                include_indexeddb: false,
                expected_current_uid: String::new(),
                store_dir: store.clone(),
            };
            let sess = switcher::Session::new(&args);
            switcher::proc::is_running(&sess)
        };

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
    })
    .await
    .map_err(|e| format!("环境检测失败: {e}"))?
}

/// 保存应用的手动 exe 路径（空串 = 清除）。
#[tauri::command(rename_all = "camelCase")]
pub fn app_set_manual_path(target_app: String, path: String) -> Result<Value, String> {
    let app = TargetApp::parse(&target_app);
    let prof = profile_for(app, &config::store_dir());
    let trimmed = path.trim();
    if !trimmed.is_empty() {
        let p = std::path::Path::new(trimmed);
        if !p.is_file() {
            return Err(format!("路径不存在或不是文件: {trimmed}"));
        }
        if !ai_gateway_core::modules::app_profile::exe_matches(p, &prof) {
            return Err(format!(
                "该文件不是 {} 的可执行文件（期望文件名：{}）",
                prof.app_name,
                prof.exe_names.join(" 或 ")
            ));
        }
    }
    config::set_app_setting(prof.settings_path_key, json!(trimmed)).map_err(|e| e.to_string())?;
    // 路径变了，清掉 exe 缓存，否则下次仍命中旧路径
    let args = switcher::RunArgs {
        action: switcher::Action::BackupCurrent,
        target_app: app,
        user_id: None,
        proxy_port: None,
        include_indexeddb: false,
        expected_current_uid: String::new(),
        store_dir: config::store_dir(),
    };
    let sess = switcher::Session::new(&args);
    switcher::locate::clear_exe_cache(&sess, &prof);
    Ok(json!({ "ok": true }))
}

// ---------------------------------------------------------------------------
// 登录态切换（Trae 系 + 豆包）
// ---------------------------------------------------------------------------

/// 执行一次切换动作（切换 / 保存登录态 / 只恢复 / 重置设备标识 / 保活）。
///
/// 进度通过 `switch-progress` 事件推给前端；返回的 `steps` 里也有完整步骤，
/// 便于「事件丢失」时兜底展示。
#[tauri::command(rename_all = "camelCase")]
pub async fn switch_action(
    app: tauri::AppHandle,
    action: String,
    target_app: String,
    user_id: Option<String>,
    proxy_port: Option<u16>,
    include_indexeddb: Option<bool>,
    expected_current_uid: Option<String>,
) -> Result<Value, String> {
    use tauri::Emitter;

    let act = match action.trim() {
        "Switch" | "switch" => switcher::Action::Switch,
        "SaveCurrentLogin" | "save" => switcher::Action::SaveCurrentLogin,
        "BackupCurrent" | "backup" => switcher::Action::BackupCurrent,
        "RestoreOnly" | "restore" => switcher::Action::RestoreOnly,
        "ResetDeviceIds" | "resetDeviceIds" => switcher::Action::ResetDeviceIds,
        "KeepAlive" | "keepalive" => switcher::Action::KeepAlive,
        other => return Err(format!("未知动作: {other}")),
    };
    let app_kind = TargetApp::parse(&target_app);

    tauri::async_runtime::spawn_blocking(move || {
        // 事件转发 sink：core 不依赖 Tauri，进度经回调送回宿主
        struct EventSink {
            app: tauri::AppHandle,
        }
        impl switcher::ProgressSink for EventSink {
            fn step(&self, stage: &str, status: switcher::StepStatus, message: &str) {
                let _ = self.app.emit(
                    "switch-progress",
                    json!({
                        "stage": stage,
                        "status": status.as_str(),
                        "message": message,
                    }),
                );
            }
        }

        let collector = switcher::VecSink::new();
        struct Fanout<'a> {
            a: &'a dyn switcher::ProgressSink,
            b: &'a dyn switcher::ProgressSink,
        }
        impl switcher::ProgressSink for Fanout<'_> {
            fn step(&self, stage: &str, status: switcher::StepStatus, message: &str) {
                self.a.step(stage, status, message);
                self.b.step(stage, status, message);
            }
        }

        let sink_events = EventSink { app: app.clone() };
        let sink = Fanout {
            a: &sink_events,
            b: &collector,
        };

        let args = switcher::RunArgs {
            action: act,
            target_app: app_kind,
            user_id: user_id.clone(),
            proxy_port,
            include_indexeddb: include_indexeddb.unwrap_or(false),
            expected_current_uid: expected_current_uid.unwrap_or_default(),
            store_dir: config::store_dir(),
        };

        let outcome = switcher::run_action(args, &sink);
        let payload = match &outcome {
            Ok(message) => json!({
                "success": true,
                "message": message,
                "steps": collector.steps(),
            }),
            Err(error) => json!({
                "success": false,
                "error": error,
                "steps": collector.steps(),
            }),
        };
        let _ = app.emit(
            "switch-done",
            json!({
                "action": action,
                "targetApp": app_kind.as_str(),
                "userId": user_id,
                "success": outcome.is_ok(),
                "raw": payload.to_string(),
            }),
        );
        outcome.map(|message| {
            json!({
                "ok": true,
                "message": message,
                "steps": collector.steps(),
            })
        })
    })
    .await
    .map_err(|e| format!("切换任务失败: {e}"))?
}

/// 当前登录态属于哪个账号（读 `current_account.txt`）。
#[tauri::command(rename_all = "camelCase")]
pub fn current_account(target_app: String) -> Value {
    let app = TargetApp::parse(&target_app);
    let args = switcher::RunArgs {
        action: switcher::Action::BackupCurrent,
        target_app: app,
        user_id: None,
        proxy_port: None,
        include_indexeddb: false,
        expected_current_uid: String::new(),
        store_dir: config::store_dir(),
    };
    let sess = switcher::Session::new(&args);
    json!({ "userId": switcher::get_current_account(&sess) })
}

/// 列出某应用的登录态快照。
#[tauri::command(rename_all = "camelCase")]
pub async fn list_snapshots(target_app: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let app = TargetApp::parse(&target_app);
        let prof = profile_for(app, &config::store_dir());
        let Ok(entries) = std::fs::read_dir(&prof.profiles_dir) else {
            return Ok(json!({ "snapshots": [] }));
        };
        let current = {
            let args = switcher::RunArgs {
                action: switcher::Action::BackupCurrent,
                target_app: app,
                user_id: None,
                proxy_port: None,
                include_indexeddb: false,
                expected_current_uid: String::new(),
                store_dir: config::store_dir(),
            };
            let sess = switcher::Session::new(&args);
            switcher::get_current_account(&sess)
        };

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
    })
    .await
    .map_err(|e| format!("读取快照列表失败: {e}"))?
}

/// 删除某个账号的登录态快照（含 `.bak`）。
#[tauri::command(rename_all = "camelCase")]
pub async fn delete_snapshot(target_app: String, user_id: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        doubao_account::ensure_uid_safe(&user_id)?;
        let app = TargetApp::parse(&target_app);
        let prof = profile_for(app, &config::store_dir());
        let slot = prof.slot_dir(&user_id);
        if !slot.exists() {
            return Err(format!("账号 {user_id} 没有快照"));
        }
        std::fs::remove_dir_all(&slot).map_err(|e| format!("删除快照失败: {e}"))?;
        let _ = std::fs::remove_dir_all(prof.profiles_dir.join(format!("{user_id}.bak")));
        Ok(json!({ "ok": true }))
    })
    .await
    .map_err(|e| format!("删除快照失败: {e}"))?
}

// ---------------------------------------------------------------------------
// Trae 账号
// ---------------------------------------------------------------------------

/// Trae 账号列表（JWT 已脱敏）。
#[tauri::command]
pub async fn trae_list_accounts() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let accounts: Vec<Value> = trae_account::load_accounts()
            .iter()
            .map(trae_account::account_meta)
            .collect();
        let cooldowns = trae_checkin::all_cooldowns();
        // 把冷却信息并进账号元数据，界面一次请求就能渲染完整状态
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
    })
    .await
    .map_err(|e| format!("读取 Trae 账号失败: {e}"))
}

/// 手动添加/更新 Trae 账号（粘贴 JWT）。
#[tauri::command(rename_all = "camelCase")]
pub async fn trae_add_account(
    jwt: String,
    name: Option<String>,
    refresh_token: Option<String>,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let info = trae_account::parse_jwt(&jwt);
        let uid = info
            .user_id
            .ok_or_else(|| "无法从 JWT 解析出账号 id，请确认粘贴的是完整的 Cloud-IDE-JWT".to_string())?;
        let acc = trae_account::upsert_account(
            &uid,
            name.as_deref(),
            &jwt,
            refresh_token.as_deref(),
        )?;
        Ok(json!({ "ok": true, "account": trae_account::account_meta(&acc) }))
    })
    .await
    .map_err(|e| format!("添加 Trae 账号失败: {e}"))?
}

/// 删除 Trae 账号。
#[tauri::command(rename_all = "camelCase")]
pub async fn trae_delete_account(user_id: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        trae_account::delete_account(&user_id)?;
        Ok(json!({ "ok": true }))
    })
    .await
    .map_err(|e| format!("删除 Trae 账号失败: {e}"))?
}

/// 发现本机登录过的 Trae 账号（双应用）。
#[tauri::command]
pub async fn trae_discover_accounts() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let discovered = trae_discover::discover_all();
        json!({
            "accounts": discovered,
            "apps": [
                { "kind": "TraeWork", "label": "Trae Work" },
                { "kind": "Trae", "label": "Trae" },
            ],
        })
    })
    .await
    .map_err(|e| format!("发现本机 Trae 账号失败: {e}"))
}

/// 读取某个应用的套餐身份。
#[tauri::command(rename_all = "camelCase")]
pub async fn trae_entitlement(app_kind: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        Ok(trae_discover::read_entitlement(&app_kind).unwrap_or(json!({})))
    })
    .await
    .map_err(|e| format!("读取套餐信息失败: {e}"))
}

/// 读取/重置账号的设备指纹。
#[tauri::command(rename_all = "camelCase")]
pub async fn trae_device_info(user_id: String, reset: Option<bool>) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let device = if reset.unwrap_or(false) {
            trae_device::reset_device_for(&user_id)
        } else {
            trae_device::resolve_device(&user_id)
        };
        Ok(json!({
            "userId": user_id,
            "deviceId": device.device_id,
            "sessionId": device.session_id,
            "marketUserId": device.market_user_id,
        }))
    })
    .await
    .map_err(|e| format!("读取设备指纹失败: {e}"))?
}

// ---------------------------------------------------------------------------
// Trae 签到
// ---------------------------------------------------------------------------

/// 执行一轮 Trae 签到。
#[tauri::command(rename_all = "camelCase")]
pub async fn trae_checkin_run(
    app: tauri::AppHandle,
    user_ids: Option<Vec<String>>,
    retry: Option<u32>,
) -> Result<Value, String> {
    use tauri::Emitter;

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

    let total = accounts.len();
    let _ = app.emit("checkin-progress", json!({ "type": "start", "total": total }));

    let summary = trae_checkin::run_round(&accounts, retry.unwrap_or(1)).await;

    // 逐账号推送进度（前端据此实时刷新列表）
    for (index, outcome) in summary.outcomes.iter().enumerate() {
        let _ = app.emit(
            "checkin-progress",
            json!({
                "type": "account",
                "index": index,
                "userId": outcome.user_id,
                "name": outcome.name,
                "status": outcome.status,
                "code": outcome.code,
                "message": outcome.message,
                "credits": outcome.credits,
                "delta": outcome.delta,
                "errorType": outcome.error_type,
                "cooldownUntil": outcome.cooldown_until,
            }),
        );
    }
    let result = summary.to_json();
    let _ = app.emit(
        "checkin-progress",
        json!({
            "type": "done",
            "ok": summary.ok,
            "already": summary.already,
            "failed": summary.failed,
            "skipped": summary.skipped,
        }),
    );
    Ok(result)
}

/// Trae 签到历史（积分趋势）。
#[tauri::command]
pub async fn trae_credits_history() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(|| {
        json!({ "records": trae_checkin::load_credits_history() })
    })
    .await
    .map_err(|e| format!("读取积分历史失败: {e}"))
}

/// 清除某账号的签到冷却。
#[tauri::command(rename_all = "camelCase")]
pub async fn trae_clear_cooldown(user_id: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        trae_checkin::clear_cooldown(&user_id);
        Ok(json!({ "ok": true }))
    })
    .await
    .map_err(|e| format!("清除冷却失败: {e}"))?
}

// ---------------------------------------------------------------------------
// 豆包
// ---------------------------------------------------------------------------

/// 豆包账号列表（凭证已脱敏）。
#[tauri::command]
pub async fn doubao_list_accounts() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let accounts: Vec<Value> = doubao_account::load_accounts()
            .iter()
            .map(doubao_account::account_view)
            .collect();
        json!({
            "accounts": accounts,
            "lastKeepaliveAt": doubao_account::last_keepalive_at(),
        })
    })
    .await
    .map_err(|e| format!("读取豆包账号失败: {e}"))
}

/// 新增/更新豆包账号。
#[tauri::command(rename_all = "camelCase")]
pub async fn doubao_save_account(
    user_id: String,
    name: Option<String>,
    note: Option<String>,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let acc = doubao_account::upsert_account(
            &user_id,
            name.as_deref(),
            note.as_deref(),
            true,
        )?;
        Ok(json!({ "ok": true, "account": doubao_account::account_view(&acc) }))
    })
    .await
    .map_err(|e| format!("保存豆包账号失败: {e}"))?
}

/// 删除豆包账号。
#[tauri::command(rename_all = "camelCase")]
pub async fn doubao_delete_account(user_id: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        doubao_account::delete_account(&user_id)?;
        Ok(json!({ "ok": true }))
    })
    .await
    .map_err(|e| format!("删除豆包账号失败: {e}"))?
}

/// 读取账号的**明文**凭证（仅供编辑弹窗回填；界面需自行脱敏展示）。
#[tauri::command(rename_all = "camelCase")]
pub async fn doubao_get_credential(user_id: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let acc = doubao_account::find_account(&user_id)
            .ok_or_else(|| "账号不存在".to_string())?;
        Ok(json!({
            "userId": user_id,
            "sessionId": acc.get("session_id"),
            "sidGuard": acc.get("sid_guard"),
            "ttwid": acc.get("ttwid"),
        }))
    })
    .await
    .map_err(|e| format!("读取凭证失败: {e}"))?
}

/// 设置账号凭证。
#[tauri::command(rename_all = "camelCase")]
pub async fn doubao_set_credential(
    user_id: String,
    session_id: Option<String>,
    sid_guard: Option<String>,
    ttwid: Option<String>,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let acc = doubao_account::apply_credential(
            &user_id,
            session_id.as_deref(),
            sid_guard.as_deref(),
            ttwid.as_deref(),
            "manual",
            true,
        )?;
        Ok(json!({ "ok": true, "account": doubao_account::account_view(&acc) }))
    })
    .await
    .map_err(|e| format!("设置凭证失败: {e}"))?
}

/// 读取最近一次抓包凭证（供「从代理抓包自动填充」按钮）。
#[tauri::command]
pub async fn doubao_captured_credential() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(|| {
        Ok(match doubao_account::load_captured() {
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
        })
    })
    .await
    .map_err(|e| format!("读取抓包凭证失败: {e}"))
}

/// 把抓包凭证回写到账号池（幂等：无变化返回 `applied: false`）。
#[tauri::command]
pub async fn doubao_credential_auto_apply() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(|| match doubao_account::auto_apply_captured()? {
        Some(account) => Ok(json!({ "applied": true, "account": account })),
        None => Ok(json!({ "applied": false })),
    })
    .await
    .map_err(|e| format!("回写凭证失败: {e}"))?
}

/// 会话保活（启动客户端 → 等待 → 关闭，触发服务端滑动续期）。
#[tauri::command(rename_all = "camelCase")]
pub async fn doubao_keepalive(app: tauri::AppHandle) -> Result<Value, String> {
    use tauri::Emitter;

    tauri::async_runtime::spawn_blocking(move || {
        struct EventSink {
            app: tauri::AppHandle,
        }
        impl switcher::ProgressSink for EventSink {
            fn step(&self, stage: &str, status: switcher::StepStatus, message: &str) {
                let _ = self.app.emit(
                    "keepalive-progress",
                    json!({ "stage": stage, "status": status.as_str(), "message": message }),
                );
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
        let sink = EventSink { app: app.clone() };
        let outcome = switcher::run_action(args, &sink);
        if outcome.is_ok() {
            let _ = doubao_account::set_last_keepalive(&config::utc_iso());
        }
        let _ = app.emit(
            "keepalive-done",
            json!({ "success": outcome.is_ok(), "raw": format!("{outcome:?}") }),
        );
        outcome.map(|message| json!({ "ok": true, "message": message }))
    })
    .await
    .map_err(|e| format!("保活任务失败: {e}"))?
}

/// HTTP 续期探活（诊断/续期；`syncOnly` 为真时只做诊断）。
#[tauri::command(rename_all = "camelCase")]
pub async fn doubao_renew(sync_only: Option<bool>) -> Result<Value, String> {
    let summary = doubao_session::run_renewal(sync_only.unwrap_or(false)).await;
    Ok(summary.to_json())
}

/// 会话与凭证诊断。
#[tauri::command]
pub async fn doubao_diagnose() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(|| Ok(doubao_session::diagnose()))
        .await
        .map_err(|e| format!("诊断失败: {e}"))?
}

/// 查询单个账号的会员额度。
#[tauri::command(rename_all = "camelCase")]
pub async fn doubao_fetch_quota(user_id: String) -> Result<Value, String> {
    let parsed = doubao_quota::fetch_single(&user_id).await?;
    let view = doubao_quota::to_view(&user_id, &parsed);
    // 写回额度缓存，账号列表无需重复查询
    let level = parsed.level.clone();
    let expire = parsed.expire_at.clone();
    let summary = parsed.summary();
    let _ = doubao_account::mutate_account(&user_id, |obj| {
        obj.insert("quota_level".to_string(), json!(level));
        obj.insert("quota_expire_at".to_string(), json!(expire));
        obj.insert("quota_summary".to_string(), json!(summary));
        obj.insert("quota_checked_at".to_string(), json!(config::utc_iso()));
    });
    Ok(view)
}

/// 批量巡检全部账号的额度。
#[tauri::command]
pub async fn doubao_quota_batch() -> Result<Value, String> {
    Ok(doubao_quota::run_batch().await)
}

/// 账号会话探活。
#[tauri::command(rename_all = "camelCase")]
pub async fn doubao_probe_account(user_id: String) -> Result<Value, String> {
    Ok(doubao_quota::probe_account(&user_id).await)
}

/// 备份账号的客户端对话状态。
#[tauri::command(rename_all = "camelCase")]
pub async fn doubao_backup_chatdata(user_id: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || doubao_chats::backup_chatdata(&user_id))
        .await
        .map_err(|e| format!("备份对话失败: {e}"))?
}

/// 恢复账号的客户端对话状态。
#[tauri::command(rename_all = "camelCase")]
pub async fn doubao_restore_chatdata(user_id: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || doubao_chats::restore_chatdata(&user_id))
        .await
        .map_err(|e| format!("恢复对话失败: {e}"))?
}

/// 查询账号的对话备份信息。
#[tauri::command(rename_all = "camelCase")]
pub async fn doubao_chatdata_info(user_id: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || Ok(doubao_chats::chatdata_info(&user_id)))
        .await
        .map_err(|e| format!("读取备份信息失败: {e}"))?
}

/// 从官方 IM API 导出对话（markdown + json）。
#[tauri::command(rename_all = "camelCase")]
pub async fn doubao_export_chats(
    user_id: String,
    limit_convs: Option<usize>,
    max_pages: Option<usize>,
) -> Result<Value, String> {
    doubao_chats::export_account(
        &user_id,
        limit_convs.unwrap_or(50),
        max_pages.unwrap_or(10),
    )
    .await
}

/// 读取应用设置。
#[tauri::command]
pub fn get_app_settings() -> Value {
    config::load_app_settings()
}

/// 合并写入应用设置。
#[tauri::command]
pub fn save_app_settings(patch: Value) -> Result<Value, String> {
    config::save_app_settings(&patch).map_err(|e| e.to_string())
}
