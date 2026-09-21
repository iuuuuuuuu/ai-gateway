//! Trae / 豆包 / 应用切换的 HTTP 处理器。
//!
//! ## 这一层补的是什么
//!
//! 前端 `api.ts` 早已为 `trae_*` / `doubao_*` / `apps_*` 登记了 HTTP 路由，
//! 但服务端 router 里一条都没有 —— webui（浏览器形态）下：
//!
//! - `GET /api/trae/accounts` 落到静态回退 → 404 → 页面永远「还没有账号」
//! - 点「导入 / 添加」→ `请求失败 (404)`，账号一个也进不来
//!
//! ## 只做参数搬运
//!
//! 业务逻辑全在 `ai_gateway_core::modules::apps_ops`，与桌面端共用同一份实现
//! （见该模块文档）。这里只负责「取参数 → 调 apps_ops → 转 JSON」，
//! 因此不会出现「桌面端修了、webui 还是坏的」这类漂移。
//!
//! ## 慢操作必须离开 async 线程
//!
//! 关客户端、拷快照、跑签到耗时可达数十秒。直接跑在 axum 的 async 线程上会把
//! 整个服务堵死（连静态页面都发不出去），所以一律 `spawn_blocking`。

use std::collections::HashMap;
use std::sync::Mutex;

use axum::extract::{Json, Query};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};

use ai_gateway_core::modules::switcher::StepStatus;
use ai_gateway_core::modules::{apps_ops, config};

use super::{json_err, json_ok};

/// webui 没有事件总线，切换/签到的进度写进这里供前端轮询。
static APP_PROGRESS: Mutex<Option<String>> = Mutex::new(None);

/// 把一行进度写进轮询缓存。
struct ProgressToCache;

impl apps_ops::ProgressSink for ProgressToCache {
    fn step(&self, stage: &str, status: StepStatus, message: &str) {
        *APP_PROGRESS.lock().unwrap() = Some(format!("{stage}|{}|{message}", status.as_str()));
    }
}

/// 清空进度缓存（动作结束后调用）。
fn clear_progress() {
    *APP_PROGRESS.lock().unwrap() = None;
}

/// 取一个必填的字符串参数（去空白；空串视为缺失）。
fn require_str(body: &Value, key: &str) -> Result<String, String> {
    body.get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("缺少参数 {key}"))
}

/// 取一个可选字符串参数（空串归一为 None）。
fn opt_str(body: &Value, key: &str) -> Option<String> {
    body.get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 取一个查询参数（去空白）。
fn q_param(params: &HashMap<String, String>, key: &str) -> String {
    params.get(key).cloned().unwrap_or_default().trim().to_string()
}

/// 把 blocking 结果统一转成响应。
fn blocking_response(
    result: Result<Result<Value, String>, tokio::task::JoinError>,
) -> Response {
    match result {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// 把 `spawn_blocking` 的结果转成响应（无业务错误的形态）。
fn blocking_plain(result: Result<Value, tokio::task::JoinError>) -> Response {
    match result {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

// ---------------------------------------------------------------------------
// 应用环境 / 登录态切换
// ---------------------------------------------------------------------------

/// GET /api/apps/env?targetApp=TraeWork —— 应用安装/运行状态。
pub async fn api_app_env_check(Query(params): Query<HashMap<String, String>>) -> Response {
    let target = q_param(&params, "targetApp");
    blocking_response(tokio::task::spawn_blocking(move || apps_ops::app_env_check(&target)).await)
}

/// POST /api/apps/manual-path —— 保存手动指定的 exe 路径（空串 = 清除）。
pub async fn api_app_set_manual_path(Json(body): Json<Value>) -> Response {
    let target = require_str(&body, "targetApp").unwrap_or_default();
    let path = body
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    blocking_response(
        tokio::task::spawn_blocking(move || apps_ops::app_set_manual_path(&target, &path)).await,
    )
}

/// GET /api/apps/current?targetApp=Trae —— 当前登录态属于哪个账号。
pub async fn api_app_current_account(Query(params): Query<HashMap<String, String>>) -> Response {
    let target = q_param(&params, "targetApp");
    blocking_plain(tokio::task::spawn_blocking(move || apps_ops::current_account(&target)).await)
}

/// GET /api/apps/snapshots?targetApp=Trae —— 登录态快照列表。
pub async fn api_app_list_snapshots(Query(params): Query<HashMap<String, String>>) -> Response {
    let target = q_param(&params, "targetApp");
    blocking_response(tokio::task::spawn_blocking(move || apps_ops::list_snapshots(&target)).await)
}

/// POST /api/apps/snapshots/delete —— 删除某账号的登录态快照。
pub async fn api_app_delete_snapshot(Json(body): Json<Value>) -> Response {
    let target = require_str(&body, "targetApp").unwrap_or_default();
    let user_id = match require_str(&body, "userId") {
        Ok(v) => v,
        Err(e) => return json_err(e, StatusCode::BAD_REQUEST),
    };
    blocking_response(
        tokio::task::spawn_blocking(move || apps_ops::delete_snapshot(&target, &user_id)).await,
    )
}

/// POST /api/apps/switch —— 执行一次切换动作。
///
/// 同步返回最终结果；进度写进 `APP_PROGRESS` 供轮询。
/// `success:false` 是「动作失败」而非请求失败，返回 200 由界面展示失败细节。
pub async fn api_app_switch_action(Json(body): Json<Value>) -> Response {
    let req = apps_ops::SwitchRequest {
        action: require_str(&body, "action").unwrap_or_default(),
        target_app: require_str(&body, "targetApp").unwrap_or_default(),
        user_id: opt_str(&body, "userId"),
        proxy_port: body
            .get("proxyPort")
            .and_then(Value::as_u64)
            .map(|p| p as u16),
        include_indexeddb: body.get("includeIndexeddb").and_then(Value::as_bool),
        expected_current_uid: opt_str(&body, "expectedCurrentUid"),
    };
    *APP_PROGRESS.lock().unwrap() = Some("开始切换账号…".to_string());
    let result =
        tokio::task::spawn_blocking(move || apps_ops::switch_action(req, &ProgressToCache)).await;
    clear_progress();
    blocking_response(result)
}

/// GET /api/apps/progress —— 轮询切换/签到/保活进度（webui 无事件总线）。
pub async fn api_app_progress() -> Response {
    let p = APP_PROGRESS.lock().unwrap().clone();
    json_ok(json!({ "progress": p }))
}

// ---------------------------------------------------------------------------
// Trae 账号
// ---------------------------------------------------------------------------

/// GET /api/trae/accounts —— 账号列表（JWT 已脱敏）。
pub async fn api_trae_list_accounts() -> Response {
    blocking_plain(tokio::task::spawn_blocking(apps_ops::trae_list_accounts).await)
}

/// POST /api/trae/accounts/add —— 粘贴 JWT 添加/更新账号。
pub async fn api_trae_add_account(Json(body): Json<Value>) -> Response {
    let jwt = match require_str(&body, "jwt") {
        Ok(v) => v,
        Err(e) => return json_err(e, StatusCode::BAD_REQUEST),
    };
    let name = opt_str(&body, "name");
    let refresh_token = opt_str(&body, "refreshToken");
    blocking_response(
        tokio::task::spawn_blocking(move || {
            apps_ops::trae_add_account(&jwt, name.as_deref(), refresh_token.as_deref())
        })
        .await,
    )
}

/// POST /api/trae/accounts/delete —— 删除账号。
pub async fn api_trae_delete_account(Json(body): Json<Value>) -> Response {
    let user_id = match require_str(&body, "userId") {
        Ok(v) => v,
        Err(e) => return json_err(e, StatusCode::BAD_REQUEST),
    };
    blocking_response(
        tokio::task::spawn_blocking(move || apps_ops::trae_delete_account(&user_id)).await,
    )
}

/// GET /api/trae/discover —— 发现本机登录过的 Trae 账号。
pub async fn api_trae_discover_accounts() -> Response {
    blocking_plain(tokio::task::spawn_blocking(apps_ops::trae_discover_accounts).await)
}

/// POST /api/trae/import-local —— 从本机登录态导入 Trae 账号（JWT 在客户端本地 Cookies 里）。
pub async fn api_trae_import_local() -> Response {
    blocking_response(tokio::task::spawn_blocking(apps_ops::trae_import_local).await)
}

/// POST /api/trae/discover/import —— 发现本机 Trae 账号并立即导入凭证。
pub async fn api_trae_discover_and_import() -> Response {
    blocking_plain(tokio::task::spawn_blocking(apps_ops::trae_discover_and_import).await)
}

/// GET /api/trae/entitlement?appKind=TraeWork —— 读取套餐身份。
pub async fn api_trae_entitlement(Query(params): Query<HashMap<String, String>>) -> Response {
    let kind = q_param(&params, "appKind");
    blocking_plain(tokio::task::spawn_blocking(move || apps_ops::trae_entitlement(&kind)).await)
}

/// GET /api/trae/device?userId=...&reset=true —— 读取/重置设备指纹。
pub async fn api_trae_device_info(Query(params): Query<HashMap<String, String>>) -> Response {
    let user_id = q_param(&params, "userId");
    if user_id.is_empty() {
        return json_err("缺少参数 userId".to_string(), StatusCode::BAD_REQUEST);
    }
    let reset = params.get("reset").map(|v| v == "true" || v == "1");
    blocking_plain(
        tokio::task::spawn_blocking(move || apps_ops::trae_device_info(&user_id, reset)).await,
    )
}

/// POST /api/trae/checkin —— 执行一轮签到（可选 body.userIds 限定账号）。
pub async fn api_trae_checkin_run(Json(body): Json<Value>) -> Response {
    let user_ids: Option<Vec<String>> = body.get("userIds").and_then(Value::as_array).map(|a| {
        a.iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect()
    });
    let retry = body.get("retry").and_then(Value::as_u64).map(|v| v as u32);
    *APP_PROGRESS.lock().unwrap() = Some("签到开始…".to_string());
    let result = apps_ops::trae_checkin_run(user_ids, retry, &ProgressToCache).await;
    clear_progress();
    match result {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// GET /api/trae/credits/history —— 积分趋势。
pub async fn api_trae_credits_history() -> Response {
    blocking_plain(tokio::task::spawn_blocking(apps_ops::trae_credits_history).await)
}

/// POST /api/trae/cooldown/clear —— 清除某账号的签到冷却。
pub async fn api_trae_clear_cooldown(Json(body): Json<Value>) -> Response {
    let user_id = match require_str(&body, "userId") {
        Ok(v) => v,
        Err(e) => return json_err(e, StatusCode::BAD_REQUEST),
    };
    blocking_plain(
        tokio::task::spawn_blocking(move || apps_ops::trae_clear_cooldown(&user_id)).await,
    )
}

// ---------------------------------------------------------------------------
// 豆包账号与凭证
// ---------------------------------------------------------------------------

/// GET /api/doubao/accounts —— 账号列表（凭证已脱敏）。
pub async fn api_doubao_list_accounts() -> Response {
    blocking_plain(tokio::task::spawn_blocking(apps_ops::doubao_list_accounts).await)
}

/// POST /api/doubao/accounts/save —— 新增/更新账号（昵称、备注）。
pub async fn api_doubao_save_account(Json(body): Json<Value>) -> Response {
    let user_id = match require_str(&body, "userId") {
        Ok(v) => v,
        Err(e) => return json_err(e, StatusCode::BAD_REQUEST),
    };
    let name = opt_str(&body, "name");
    // 备注要保留空串语义（清空备注），因此不折成 None
    let note = body.get("note").and_then(Value::as_str).map(str::to_string);
    blocking_response(
        tokio::task::spawn_blocking(move || {
            apps_ops::doubao_save_account(&user_id, name.as_deref(), note.as_deref())
        })
        .await,
    )
}

/// POST /api/doubao/accounts/delete —— 删除账号。
pub async fn api_doubao_delete_account(Json(body): Json<Value>) -> Response {
    let user_id = match require_str(&body, "userId") {
        Ok(v) => v,
        Err(e) => return json_err(e, StatusCode::BAD_REQUEST),
    };
    blocking_response(
        tokio::task::spawn_blocking(move || apps_ops::doubao_delete_account(&user_id)).await,
    )
}

/// GET /api/doubao/credential?userId=... —— 读取**明文**凭证（仅供编辑弹窗回填）。
pub async fn api_doubao_get_credential(Query(params): Query<HashMap<String, String>>) -> Response {
    let user_id = q_param(&params, "userId");
    if user_id.is_empty() {
        return json_err("缺少参数 userId".to_string(), StatusCode::BAD_REQUEST);
    }
    blocking_response(
        tokio::task::spawn_blocking(move || apps_ops::doubao_get_credential(&user_id)).await,
    )
}

/// POST /api/doubao/credential —— 设置凭证（空串 = 清除）。
pub async fn api_doubao_set_credential(Json(body): Json<Value>) -> Response {
    let user_id = match require_str(&body, "userId") {
        Ok(v) => v,
        Err(e) => return json_err(e, StatusCode::BAD_REQUEST),
    };
    // 与桌面端一致：空串是「清除凭证」的显式语义，因此保留空串不折成 None
    let field = |key: &str| -> Option<String> {
        body.get(key).and_then(Value::as_str).map(str::to_string)
    };
    let session_id = field("sessionId");
    let sid_guard = field("sidGuard");
    let ttwid = field("ttwid");
    blocking_response(
        tokio::task::spawn_blocking(move || {
            apps_ops::doubao_set_credential(
                &user_id,
                session_id.as_deref(),
                sid_guard.as_deref(),
                ttwid.as_deref(),
            )
        })
        .await,
    )
}

/// GET /api/doubao/credential/captured —— 最近一次抓包凭证。
pub async fn api_doubao_captured_credential() -> Response {
    blocking_plain(tokio::task::spawn_blocking(apps_ops::doubao_captured_credential).await)
}

/// POST /api/doubao/credential/apply —— 把抓包凭证回写账号池（幂等）。
pub async fn api_doubao_credential_auto_apply() -> Response {
    blocking_response(
        tokio::task::spawn_blocking(apps_ops::doubao_credential_auto_apply).await,
    )
}

/// POST /api/doubao/keepalive —— 会话保活（启停客户端，慢操作）。
pub async fn api_doubao_keepalive() -> Response {
    *APP_PROGRESS.lock().unwrap() = Some("保活开始…".to_string());
    let result =
        tokio::task::spawn_blocking(move || apps_ops::doubao_keepalive(&ProgressToCache)).await;
    clear_progress();
    blocking_response(result)
}

/// POST /api/doubao/renew —— HTTP 续期探活（body.syncOnly 为真时只诊断）。
pub async fn api_doubao_renew(Json(body): Json<Value>) -> Response {
    let sync_only = body
        .get("syncOnly")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    json_ok(apps_ops::doubao_renew(sync_only).await)
}

/// GET /api/doubao/diagnose —— 会话与凭证诊断。
pub async fn api_doubao_diagnose() -> Response {
    blocking_plain(tokio::task::spawn_blocking(apps_ops::doubao_diagnose).await)
}

/// GET /api/doubao/quota?userId=... —— 单账号会员额度。
pub async fn api_doubao_fetch_quota(Query(params): Query<HashMap<String, String>>) -> Response {
    let user_id = q_param(&params, "userId");
    if user_id.is_empty() {
        return json_err("缺少参数 userId".to_string(), StatusCode::BAD_REQUEST);
    }
    match apps_ops::doubao_fetch_quota(&user_id).await {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// POST /api/doubao/quota/batch —— 批量巡检额度。
pub async fn api_doubao_quota_batch() -> Response {
    json_ok(apps_ops::doubao_quota_batch().await)
}

/// GET /api/doubao/probe?userId=... —— 会话探活。
pub async fn api_doubao_probe_account(Query(params): Query<HashMap<String, String>>) -> Response {
    let user_id = q_param(&params, "userId");
    if user_id.is_empty() {
        return json_err("缺少参数 userId".to_string(), StatusCode::BAD_REQUEST);
    }
    json_ok(apps_ops::doubao_probe_account(&user_id).await)
}

/// POST /api/doubao/chatdata/backup —— 备份账号的对话状态。
pub async fn api_doubao_backup_chatdata(Json(body): Json<Value>) -> Response {
    let user_id = match require_str(&body, "userId") {
        Ok(v) => v,
        Err(e) => return json_err(e, StatusCode::BAD_REQUEST),
    };
    blocking_response(
        tokio::task::spawn_blocking(move || apps_ops::doubao_backup_chatdata(&user_id)).await,
    )
}

/// POST /api/doubao/chatdata/restore —— 恢复账号的对话状态。
pub async fn api_doubao_restore_chatdata(Json(body): Json<Value>) -> Response {
    let user_id = match require_str(&body, "userId") {
        Ok(v) => v,
        Err(e) => return json_err(e, StatusCode::BAD_REQUEST),
    };
    blocking_response(
        tokio::task::spawn_blocking(move || apps_ops::doubao_restore_chatdata(&user_id)).await,
    )
}

/// GET /api/doubao/chatdata/info?userId=... —— 对话备份信息。
pub async fn api_doubao_chatdata_info(Query(params): Query<HashMap<String, String>>) -> Response {
    let user_id = q_param(&params, "userId");
    if user_id.is_empty() {
        return json_err("缺少参数 userId".to_string(), StatusCode::BAD_REQUEST);
    }
    blocking_plain(
        tokio::task::spawn_blocking(move || apps_ops::doubao_chatdata_info(&user_id)).await,
    )
}

/// POST /api/doubao/chats/export —— 从官方 IM API 导出对话。
pub async fn api_doubao_export_chats(Json(body): Json<Value>) -> Response {
    let user_id = match require_str(&body, "userId") {
        Ok(v) => v,
        Err(e) => return json_err(e, StatusCode::BAD_REQUEST),
    };
    let limit = body.get("limitConvs").and_then(Value::as_u64).map(|v| v as usize);
    let max_pages = body.get("maxPages").and_then(Value::as_u64).map(|v| v as usize);
    match apps_ops::doubao_export_chats(&user_id, limit, max_pages).await {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

// ---------------------------------------------------------------------------
// 应用设置
// ---------------------------------------------------------------------------

/// GET /api/apps/settings —— 读取应用设置。
pub async fn api_get_app_settings() -> Response {
    blocking_plain(tokio::task::spawn_blocking(config::load_app_settings).await)
}

/// POST /api/apps/settings —— 合并写入应用设置。
pub async fn api_save_app_settings(Json(body): Json<Value>) -> Response {
    match tokio::task::spawn_blocking(move || config::save_app_settings(&body)).await {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e.to_string(), StatusCode::BAD_REQUEST),
        Err(e) => json_err(e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}
