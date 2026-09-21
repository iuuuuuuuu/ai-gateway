//! Trae 与豆包的 Tauri 命令层。
//!
//! 这一层只做三件事：**参数校验**、**调用 core**、**把结果转成前端可用的 JSON**。
//! 所有业务逻辑都在 `ai_gateway_core::modules` 里 —— core 不依赖 Tauri，
//! 因此同一套逻辑也能被 HTTP server 形态复用。
//!
//! 具体来说，Trae / 豆包 / 应用切换的业务逻辑都在
//! `ai_gateway_core::modules::apps_ops`：桌面端与 webui 共用同一份实现，
//! 这里只剩「把 Tauri 参数解包 → 调 apps_ops → 把进度转成 Tauri 事件」。
//! **不要**把业务逻辑写回这个文件 —— 一旦写回来，webui 就又会漏掉它
//! （Trae / 豆包的账号导入在浏览器形态长期 404，就是这么来的）。
//!
//! ## 为什么这些命令要 `async` + `spawn_blocking`
//!
//! 切换账号、备份对话、设备标识重置都会**关闭并启动客户端进程**，耗时可达数十秒。
//! Tauri 的同步命令跑在主线程上，一旦阻塞就会冻结整个窗口消息循环
//! （表现为界面卡死、窗口无法拖动）。凡是会跑子进程或做大文件拷贝的命令
//! 都必须放进 blocking 线程池。

use serde_json::{json, Value};

use ai_gateway_core::modules::{apps_ops, config, switcher};

/// 把 `apps_ops` 的进度转发成 Tauri 事件。
struct EventSink<E: Fn(&str, &str) + Send + Sync> {
    emit: E,
}

impl<E: Fn(&str, &str) + Send + Sync> apps_ops::ProgressSink for EventSink<E> {
    fn step(&self, stage: &str, status: switcher::StepStatus, message: &str) {
        (self.emit)(stage, &format!("{}|{message}", status.as_str()));
    }
}

// ---------------------------------------------------------------------------
// 环境检测
// ---------------------------------------------------------------------------

/// 应用安装/运行状态（供界面「环境配置」页）。
#[tauri::command(rename_all = "camelCase")]
pub async fn app_env_check(target_app: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || apps_ops::app_env_check(&target_app))
        .await
        .map_err(|e| format!("环境检测失败: {e}"))?
}

/// 保存应用的手动 exe 路径（空串 = 清除）。
#[tauri::command(rename_all = "camelCase")]
pub async fn app_set_manual_path(target_app: String, path: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || apps_ops::app_set_manual_path(&target_app, &path))
        .await
        .map_err(|e| format!("设置安装路径失败: {e}"))?
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
    let req = apps_ops::SwitchRequest {
        action: action.clone(),
        target_app: target_app.clone(),
        user_id: user_id.clone(),
        proxy_port,
        include_indexeddb,
        expected_current_uid,
    };

    tauri::async_runtime::spawn_blocking(move || {
        use tauri::Emitter;

        // 进度转 Tauri 事件；core 不依赖 Tauri，进度经回调送回宿主
        let progress_app = app.clone();
        let progress_app_done = app.clone();
        let emit_action = action.clone();
        let emit_app = target_app.clone();
        let emit_user_id = user_id.clone();

        let sink = EventSink {
            emit: move |stage: &str, message: &str| {
                let _ = progress_app.emit(
                    "switch-progress",
                    json!({ "stage": stage, "message": message }),
                );
            },
        };

        let payload = apps_ops::switch_action(req, &sink)?;
        let success = payload
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let _ = progress_app_done.emit(
            "switch-done",
            json!({
                "action": emit_action,
                "targetApp": emit_app,
                "userId": emit_user_id,
                "success": success,
                "raw": payload.to_string(),
            }),
        );
        Ok(payload)
    })
    .await
    .map_err(|e| format!("切换任务失败: {e}"))?
}

/// 当前登录态属于哪个账号（读 `current_account.txt`）。
#[tauri::command(rename_all = "camelCase")]
pub fn current_account(target_app: String) -> Value {
    apps_ops::current_account(&target_app)
}

/// 列出某应用的登录态快照。
#[tauri::command(rename_all = "camelCase")]
pub async fn list_snapshots(target_app: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || apps_ops::list_snapshots(&target_app))
        .await
        .map_err(|e| format!("读取快照列表失败: {e}"))?
}

/// 删除某个账号的登录态快照（含 `.bak`）。
#[tauri::command(rename_all = "camelCase")]
pub async fn delete_snapshot(target_app: String, user_id: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || apps_ops::delete_snapshot(&target_app, &user_id))
        .await
        .map_err(|e| format!("删除快照失败: {e}"))?
}

// ---------------------------------------------------------------------------
// Trae 账号
// ---------------------------------------------------------------------------

/// Trae 账号列表（JWT 已脱敏）。
#[tauri::command]
pub async fn trae_list_accounts() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(apps_ops::trae_list_accounts)
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
        apps_ops::trae_add_account(&jwt, name.as_deref(), refresh_token.as_deref())
    })
    .await
    .map_err(|e| format!("添加 Trae 账号失败: {e}"))?
}

/// 删除 Trae 账号。
#[tauri::command(rename_all = "camelCase")]
pub async fn trae_delete_account(user_id: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || apps_ops::trae_delete_account(&user_id))
        .await
        .map_err(|e| format!("删除 Trae 账号失败: {e}"))?
}

/// 发现本机登录过的 Trae 账号（双应用）。
#[tauri::command]
pub async fn trae_discover_accounts() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(apps_ops::trae_discover_accounts)
        .await
        .map_err(|e| format!("发现本机 Trae 账号失败: {e}"))
}

/// 从本机登录态导入 Trae 账号（解客户端本地 Cookies，无需开客户端/代理）。
#[tauri::command]
pub async fn trae_import_local() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(apps_ops::trae_import_local)
        .await
        .map_err(|e| format!("导入本机 Trae 登录态失败: {e}"))?
}

/// 发现本机 Trae 账号并立即导入凭证（发现即补全）。
#[tauri::command]
pub async fn trae_discover_and_import() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(apps_ops::trae_discover_and_import)
        .await
        .map_err(|e| format!("发现并导入 Trae 账号失败: {e}"))
}

/// 读取某个应用的套餐身份。
#[tauri::command(rename_all = "camelCase")]
pub async fn trae_entitlement(app_kind: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || apps_ops::trae_entitlement(&app_kind))
        .await
        .map_err(|e| format!("读取套餐信息失败: {e}"))
}

/// 读取/重置账号的设备指纹。
#[tauri::command(rename_all = "camelCase")]
pub async fn trae_device_info(user_id: String, reset: Option<bool>) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || apps_ops::trae_device_info(&user_id, reset))
        .await
        .map_err(|e| format!("读取设备指纹失败: {e}"))
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

    // 逐账号推送进度（前端据此实时刷新列表）
    let sink = EventSink {
        emit: move |stage: &str, message: &str| {
            let _ = app.emit("checkin-progress", json!({ "stage": stage, "data": message }));
        },
    };
    apps_ops::trae_checkin_run(user_ids, retry, &sink).await
}

/// Trae 签到历史（积分趋势）。
#[tauri::command]
pub async fn trae_credits_history() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(apps_ops::trae_credits_history)
        .await
        .map_err(|e| format!("读取积分历史失败: {e}"))
}

/// 清除某账号的签到冷却。
#[tauri::command(rename_all = "camelCase")]
pub async fn trae_clear_cooldown(user_id: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || apps_ops::trae_clear_cooldown(&user_id))
        .await
        .map_err(|e| format!("清除冷却失败: {e}"))
}

// ---------------------------------------------------------------------------
// 豆包
// ---------------------------------------------------------------------------

/// 豆包账号列表（凭证已脱敏）。
#[tauri::command]
pub async fn doubao_list_accounts() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(apps_ops::doubao_list_accounts)
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
        apps_ops::doubao_save_account(&user_id, name.as_deref(), note.as_deref())
    })
    .await
    .map_err(|e| format!("保存豆包账号失败: {e}"))?
}

/// 删除豆包账号。
#[tauri::command(rename_all = "camelCase")]
pub async fn doubao_delete_account(user_id: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || apps_ops::doubao_delete_account(&user_id))
        .await
        .map_err(|e| format!("删除豆包账号失败: {e}"))?
}

/// 读取账号的**明文**凭证（仅供编辑弹窗回填；界面需自行脱敏展示）。
#[tauri::command(rename_all = "camelCase")]
pub async fn doubao_get_credential(user_id: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || apps_ops::doubao_get_credential(&user_id))
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
        apps_ops::doubao_set_credential(
            &user_id,
            session_id.as_deref(),
            sid_guard.as_deref(),
            ttwid.as_deref(),
        )
    })
    .await
    .map_err(|e| format!("设置凭证失败: {e}"))?
}

/// 读取最近一次抓包凭证（供「从代理抓包自动填充」按钮）。
#[tauri::command]
pub async fn doubao_captured_credential() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(apps_ops::doubao_captured_credential)
        .await
        .map_err(|e| format!("读取抓包凭证失败: {e}"))
}

/// 把抓包凭证回写到账号池（幂等：无变化返回 `applied: false`）。
#[tauri::command]
pub async fn doubao_credential_auto_apply() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(apps_ops::doubao_credential_auto_apply)
        .await
        .map_err(|e| format!("回写凭证失败: {e}"))?
}

/// 会话保活（启动客户端 → 等待 → 关闭，触发服务端滑动续期）。
#[tauri::command]
pub async fn doubao_keepalive(app: tauri::AppHandle) -> Result<Value, String> {
    use tauri::Emitter;

    let app_done = app.clone();
    let sink = EventSink {
        emit: move |stage: &str, message: &str| {
            let _ = app.emit(
                "keepalive-progress",
                json!({ "stage": stage, "message": message }),
            );
        },
    };
    tauri::async_runtime::spawn_blocking(move || {
        let outcome = apps_ops::doubao_keepalive(&sink);
        let _ = app_done.emit(
            "keepalive-done",
            json!({ "success": outcome.is_ok(), "raw": format!("{outcome:?}") }),
        );
        outcome
    })
    .await
    .map_err(|e| format!("保活任务失败: {e}"))?
}

/// HTTP 续期探活（诊断/续期；`syncOnly` 为真时只做诊断）。
#[tauri::command(rename_all = "camelCase")]
pub async fn doubao_renew(sync_only: Option<bool>) -> Result<Value, String> {
    Ok(apps_ops::doubao_renew(sync_only.unwrap_or(false)).await)
}

/// 会话与凭证诊断。
#[tauri::command]
pub async fn doubao_diagnose() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || Ok(apps_ops::doubao_diagnose()))
        .await
        .map_err(|e| format!("诊断失败: {e}"))?
}

/// 查询单个账号的会员额度。
#[tauri::command(rename_all = "camelCase")]
pub async fn doubao_fetch_quota(user_id: String) -> Result<Value, String> {
    apps_ops::doubao_fetch_quota(&user_id).await
}

/// 批量巡检全部账号的额度。
#[tauri::command]
pub async fn doubao_quota_batch() -> Result<Value, String> {
    Ok(apps_ops::doubao_quota_batch().await)
}

/// 账号会话探活。
#[tauri::command(rename_all = "camelCase")]
pub async fn doubao_probe_account(user_id: String) -> Result<Value, String> {
    Ok(apps_ops::doubao_probe_account(&user_id).await)
}

/// 备份账号的客户端对话状态。
#[tauri::command(rename_all = "camelCase")]
pub async fn doubao_backup_chatdata(user_id: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || apps_ops::doubao_backup_chatdata(&user_id))
        .await
        .map_err(|e| format!("备份对话失败: {e}"))?
}

/// 恢复账号的客户端对话状态。
#[tauri::command(rename_all = "camelCase")]
pub async fn doubao_restore_chatdata(user_id: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || apps_ops::doubao_restore_chatdata(&user_id))
        .await
        .map_err(|e| format!("恢复对话失败: {e}"))?
}

/// 查询账号的对话备份信息。
#[tauri::command(rename_all = "camelCase")]
pub async fn doubao_chatdata_info(user_id: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || Ok(apps_ops::doubao_chatdata_info(&user_id)))
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
    apps_ops::doubao_export_chats(&user_id, limit_convs, max_pages).await
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

// ---------------------------------------------------------------------------
// 计划任务（Windows schtasks）
// ---------------------------------------------------------------------------

/// 查询全部计划任务的注册状态。
#[tauri::command]
pub async fn task_status() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(apps_ops::task_status)
        .await
        .map_err(|e| format!("查询计划任务失败: {e}"))
}

/// 注册（或覆盖）一个每日计划任务。
#[tauri::command(rename_all = "camelCase")]
pub async fn task_register(kind: String, time: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        // 桌面端用**自己的** exe：它以 `--task-run <key>` 触发任务
        let exe = std::env::current_exe().map_err(|e| format!("获取主程序路径失败: {e}"))?;
        apps_ops::task_register(&kind, &time, &exe)
    })
    .await
    .map_err(|e| format!("注册计划任务失败: {e}"))?
}

/// 删除计划任务。
#[tauri::command(rename_all = "camelCase")]
pub async fn task_unregister(kind: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || apps_ops::task_unregister(&kind))
        .await
        .map_err(|e| format!("删除计划任务失败: {e}"))?
}

/// 立即执行一次任务（不依赖计划任务，用于验证配置是否正确）。
#[tauri::command(rename_all = "camelCase")]
pub async fn task_run_now(kind: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || apps_ops::task_run_now(&kind))
        .await
        .map_err(|e| format!("执行任务失败: {e}"))?
}
