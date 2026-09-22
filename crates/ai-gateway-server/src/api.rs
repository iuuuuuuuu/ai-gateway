//! HTTP API 层：把 ai-gateway-core 暴露为本地 REST 接口，供 webui（浏览器）调用。
//!
//! 路由设计对应 Python 版 server.py 与桌面端 commands.rs。仅绑定 127.0.0.1，
//! token 不出本机。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Query, RawQuery};
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rust_embed::RustEmbed;
use serde_json::{json, Value};

use ai_gateway_core::modules::{
    account, auth_file, checkin, codebuddy_cli, codebuddy_cn_ide, config, credit_usage, credits, export_import,
    oauth, process, qoder_account, qoder_login, refresh, rotate, session, switch, token_stats, travel, update,
    zcode_account, zcode_login,
};

/// WorkBuddy 运行状态缓存：Windows 上检测要跑 tasklist（慢），缓存几秒避免
/// 前端切 tab 频繁触发命令行导致卡顿/闪窗。
static RUNNING_CACHE: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

fn cached_workbuddy_running() -> bool {
    let mut cache = RUNNING_CACHE.lock().unwrap();
    if let Some((t, v)) = cache.as_ref() {
        if t.elapsed() < Duration::from_secs(3) {
            return *v;
        }
    }
    let v = process::is_workbuddy_running();
    *cache = Some((Instant::now(), v));
    v
}

#[derive(RustEmbed)]
#[folder = "../../dist/"]
struct Assets;

/// 切换进度缓存：webui 通过 GET /api/switch/progress 轮询。
static SWITCH_PROGRESS: Mutex<Option<String>> = Mutex::new(None);
static SWITCH_RUNNING: Mutex<bool> = Mutex::new(false);

pub fn router() -> Router {
    Router::new()
        .route("/api/status", get(api_status))
        .route("/api/accounts", get(api_accounts))
        .route("/api/accounts/note", post(api_set_account_note))
        .route("/api/accounts/disabled", post(api_set_account_disabled))
        .route("/api/codebuddy-cli/status", get(api_codebuddy_cli_status))
        .route(
            "/api/codebuddy-cli/install-helper",
            post(api_codebuddy_cli_install_helper),
        )
        .route("/api/codebuddy-cli/switch", post(api_codebuddy_cli_switch))
        .route("/api/codebuddy-cn-ide/status", get(api_codebuddy_cn_ide_status))
        .route("/api/codebuddy-cn-ide/switch", post(api_codebuddy_cn_ide_switch))
        .route("/api/codebuddy-cn-ide/detect", post(api_codebuddy_cn_ide_detect))
        .route("/api/delete", post(api_delete))
        .route("/api/oauth/start", post(api_oauth_start))
        .route("/api/oauth/status", post(api_oauth_status))
        .route("/api/import-local", post(api_import_local))
        .route("/api/import-local/scan", get(api_scan_local))
        .route(
            "/api/import-local/selected",
            post(api_import_local_selected),
        )
        .route("/api/export-accounts", post(api_export_accounts))
        .route(
            "/api/export-accounts-to-path",
            post(api_export_accounts_to_path),
        )
        .route("/api/import/preview", post(api_preview_import))
        .route("/api/import", post(api_import))
        .route("/api/switch", post(api_switch))
        .route("/api/switch/progress", get(api_switch_progress))
        .route("/api/sessions", get(api_sessions))
        .route("/api/sessions/copy", post(api_copy_sessions))
        .route("/api/checkin/status", get(api_checkin_status))
        .route("/api/credits", post(api_credits))
        .route("/api/credits/stats", get(api_credit_statistics))
        .route("/api/token-stats", get(api_token_statistics))
        .route("/api/checkin", post(api_checkin))
        .route("/api/checkin/all", post(api_checkin_all))
        .route(
            "/api/checkin/config",
            get(api_checkin_config).post(api_save_checkin_config),
        )
        .route("/api/checkin/logs", get(api_checkin_logs))
        .route(
            "/api/settings/retention",
            get(api_record_retention).post(api_save_record_retention),
        )
        .route(
            "/api/account-records",
            get(api_account_records).post(api_account_records_query),
        )
        .route(
            "/api/account-records/backfill",
            post(api_backfill_account_records),
        )
        .route("/api/travel/status", get(api_travel_status))
        .route("/api/travel/run", post(api_travel_run))
        .route("/api/travel/adopt", post(api_travel_adopt))
        .route(
            "/api/travel/config",
            get(api_travel_config).post(api_save_travel_config),
        )
        .route(
            "/api/rotate/config",
            get(api_rotate_config).post(api_save_rotate_config),
        )
        .route("/api/rotate/status", get(api_rotate_status))
        .route("/api/rotate/run", post(api_rotate_run))
        .route("/api/rotate/logs", get(api_rotate_logs))
        .route("/api/refresh-token", post(api_refresh_token))
        // ---- 网关（workbuddy2api）集成 ----
        .route("/api/gateway/status", get(api_gateway_status))
        .route("/api/gateway/config", get(api_gateway_config).post(api_save_gateway_config))
        .route("/api/gateway/mode", post(api_switch_gateway_mode))
        .route("/api/gateway/allowed-model", post(api_set_allowed_model))
        // 「模型 → 允许的平台」白名单（所有者的需求：
        // 「平台区分使用哪个平台的模型」）。
        .route("/api/gateway/model-platforms", post(api_set_model_platforms))
        .route("/api/gateway/start", post(api_gateway_start))
        .route("/api/gateway/port-check", post(api_gateway_port_check))
        .route("/api/gateway/port-holder", post(api_gateway_port_holder))
        .route("/api/gateway/port-kill", post(api_gateway_port_kill))
        .route("/api/gateway/stop", post(api_gateway_stop))
        .route("/api/gateway/sync", post(api_gateway_sync))
        .route("/api/gateway/restart", post(api_gateway_restart))
        .route("/api/gateway/models", get(api_gateway_models))
        .route("/api/gateway/usage", get(api_gateway_usage))
        .route("/api/gateway/task-run", post(api_gateway_task_run))
        .route("/api/gateway/growth-task", post(api_gateway_growth_task))
        // ---- 一键导入：接入本机 AI 客户端 ----
        .route("/api/gateway/agents", get(api_agents_detect))
        .route("/api/gateway/agents/import", post(api_agents_import))
        .route("/api/gateway/agents/batch-import", post(api_agents_batch_import))
        .route("/api/gateway/agents/restore", post(api_agents_restore))
        .route("/api/gateway/agents/backups", get(api_agents_backups))
        .route("/api/update/check", get(api_update_check))
        .route(
            "/api/update/config",
            get(api_update_config).post(api_save_update_config),
        )
        // ---- Qoder（QoderWork）：账号元信息 + 登录编排 ----
        // 凭证与 COSY 签名在 Go 侧 internal/qoder；这里只做编排。
        // 有这些路由，dev 模式（浏览器）下 Qoder 页才可用。
        .route("/api/qoder/accounts", get(api_qoder_list_accounts))
        .route("/api/qoder/accounts/save", post(api_qoder_save_account))
        .route("/api/qoder/accounts/delete", post(api_qoder_delete_account))
        .route("/api/qoder/summary", get(api_qoder_summary))
        .route("/api/qoder/login/start", post(api_qoder_login_start))
        .route("/api/qoder/login/poll", post(api_qoder_login_poll))
        .route("/api/qoder/import", post(api_qoder_import))
        // 从**客户端自己的登录态**一键导入（主路径，见 qoder_login 的注释）。
        .route("/api/qoder/import-from-client", post(api_qoder_import_from_client))
        // 刷新额度 / 到期 / 支持模型（此前没有任何生产者调用 FetchQuota）
        .route("/api/qoder/refresh-account", post(api_qoder_refresh_account))
        // 权益活动（「每天领 100 Credits」那类）。**只读**。
        //
        // ⚠ 活动是**每账号专属**的 —— 单账号接口只能看到那一个账号的状态。
        .route("/api/qoder/campaigns", post(api_qoder_campaigns))
        // 批量：所有账号的活动 / 一键领取。所有者的需求：
        // 「每个账号都能领取，可以跟 workbuddy 一样显示一个一键领取（所有账号）」
        .route("/api/qoder/campaigns-all", post(api_qoder_campaigns_all))
        .route("/api/qoder/claim-all-campaigns", post(api_qoder_claim_all_campaigns))
        // 领取权益活动。既支持**用户显式触发**（本路由），
        // 也支持**后台自动领取**（2026-09-22 所有者要求后改为支持）。
        //
        // 此前这里写着「不做自动化（见 campaign.go）」—— 该结论已撤销：
        // 实测该端点不需要验证码，且活动每天 10:00 (UTC+8) 重置、
        // 不领就作废。详见 `go-gateway/internal/qoder/campaign.go` 文件头。
        .route("/api/qoder/claim-campaign", post(api_qoder_claim_campaign))
        // ---- ZCode（Z.AI / 智谱）----
        // 与 Qoder 的差异：凭证是用户可复制的字符串，故导入是主路径。
        .route("/api/zcode/accounts", get(api_zcode_list_accounts))
        .route("/api/zcode/accounts/save", post(api_zcode_save_account))
        .route("/api/zcode/accounts/delete", post(api_zcode_delete_account))
        .route("/api/zcode/summary", get(api_zcode_summary))
        .route("/api/zcode/import-credential", post(api_zcode_import_credential))
        .route("/api/zcode/import-dir", post(api_zcode_import_dir))
        // 扫描本机 ZCode 客户端凭证 → 一键导入（省掉手工粘贴）。
        //
        // ⚠ **没有对应的 Qoder 路由，这是刻意的**：Qoder 的凭证在自定义加密的
        // `auth.v1.dat` 里，本机没有明文副本 —— 拿不到就不给按钮，
        // 否则是个点了没反应的摆设。详见 zcode_scan.rs 的模块注释。
        .route("/api/zcode/scan-local", post(api_zcode_scan_local))
        .route("/api/zcode/import-scanned", post(api_zcode_import_scanned))
        // 刷新额度 / 到期 / 支持模型（此前没有任何生产者调用 FetchQuota）
        .route("/api/zcode/refresh-account", post(api_zcode_refresh_account))
        .route("/api/zcode/login/start", post(api_zcode_login_start))
        .route("/api/zcode/login/poll", post(api_zcode_login_poll))
        .fallback(static_handler)
}

fn json_ok(v: Value) -> Response {
    Json(v).into_response()
}

fn json_err(e: String, code: StatusCode) -> Response {
    (code, Json(json!({ "ok": false, "error": e }))).into_response()
}

// ---------------------------------------------------------------------------
// 状态 / 账号
// ---------------------------------------------------------------------------

async fn api_status() -> Response {
    let auth = auth_file::read_auth_file();
    let current = auth.as_ref().and_then(|a| {
        let acct = a.get("account").cloned().unwrap_or_else(|| json!({}));
        Some(json!({
            "uid": acct.get("uid"),
            "nickname": acct.get("nickname"),
            "email": acct.get("email"),
        }))
    });
    json_ok(json!({
        "running": cached_workbuddy_running(),
        "authFile": auth_file::auth_file_path().to_string_lossy(),
        "current": current,
        "appPath": auth_file::workbuddy_app_path().to_string_lossy(),
        "version": update::APP_VERSION,
    }))
}

async fn api_accounts() -> Response {
    json_ok(json!({
        "accounts": account::load_accounts()
            .iter()
            .map(account::account_meta)
            .collect::<Vec<_>>(),
        "current": auth_file::read_auth_file()
            .and_then(|a| a.get("account").and_then(|x| x.get("uid")).and_then(|x| x.as_str()).map(String::from)),
    }))
}

/// POST /api/accounts/note —— 设置账号备注（空串 = 清除）。
///
/// body: `{ "accountId": "...", "note": "公司号" }`
async fn api_set_account_note(Json(body): Json<Value>) -> Response {
    let id = body
        .get("accountId")
        .or_else(|| body.get("account_id"))
        .or_else(|| body.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if id.trim().is_empty() {
        return json_err("缺少账号 id".to_string(), StatusCode::BAD_REQUEST);
    }
    let note = body.get("note").and_then(Value::as_str).unwrap_or("");
    match account::set_account_note(id, note) {
        Ok(acc) => json_ok(json!({ "ok": true, "account": account::account_meta(&acc) })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

// ---------------------------------------------------------------------------
// Qoder（QoderWork）
// ---------------------------------------------------------------------------
//
// 与 Tauri 命令层一一对应（commands_apps.rs 的 qoder_* 系列）。
// 两者都只做编排：凭证与 COSY 签名在 Go 侧 internal/qoder。
//
// 为什么 HTTP 形态也要有：dev 模式（浏览器预览）走的是 HTTP，
// 缺这些路由会让 Qoder 页在 dev 下整页 404 —— 而 dev 正是迭代最快的路径。

/// GET /api/qoder/accounts
async fn api_qoder_list_accounts() -> Response {
    match tokio::task::spawn_blocking(qoder_account::list_with_credentials).await {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(format!("读取 Qoder 账号失败: {e}"), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// GET /api/qoder/summary
async fn api_qoder_summary() -> Response {
    match tokio::task::spawn_blocking(|| qoder_account::summary()).await {
        Ok(v) => json_ok(v),
        Err(e) => json_err(format!("读取 Qoder 概览失败: {e}"), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/qoder/accounts/save —— body: `{ "uid": "...", "patch": {...} }`
async fn api_qoder_save_account(Json(body): Json<Value>) -> Response {
    let uid = body
        .get("uid")
        .or_else(|| body.get("userId"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if uid.trim().is_empty() {
        return json_err("缺少账号 uid".to_string(), StatusCode::BAD_REQUEST);
    }
    let patch = body.get("patch").cloned().unwrap_or_else(|| json!({}));
    match tokio::task::spawn_blocking(move || qoder_account::upsert_account(&uid, &patch)).await {
        Ok(Ok(acc)) => json_ok(json!({ "ok": true, "account": acc.to_view() })),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(format!("保存 Qoder 账号失败: {e}"), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/qoder/accounts/delete —— body: `{ "uid": "..." }`
async fn api_qoder_delete_account(Json(body): Json<Value>) -> Response {
    let uid = body
        .get("uid")
        .or_else(|| body.get("userId"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if uid.trim().is_empty() {
        return json_err("缺少账号 uid".to_string(), StatusCode::BAD_REQUEST);
    }
    match tokio::task::spawn_blocking(move || qoder_account::delete_account(&uid)).await {
        Ok(Ok(removed)) => json_ok(json!({ "ok": removed })),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(format!("删除 Qoder 账号失败: {e}"), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/qoder/login/start —— body: `{ "region": "cn" | "intl" }`
async fn api_qoder_login_start(Json(body): Json<Value>) -> Response {
    let region = body.get("region").and_then(Value::as_str).unwrap_or("").to_string();
    match tokio::task::spawn_blocking(move || qoder_login::login_start(&region)).await {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(format!("发起 Qoder 登录失败: {e}"), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/qoder/login/poll —— body: `{ "sessionId": "..." }`
async fn api_qoder_login_poll(Json(body): Json<Value>) -> Response {
    let sid = body
        .get("sessionId")
        .or_else(|| body.get("session_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match tokio::task::spawn_blocking(move || qoder_login::login_poll(&sid)).await {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(format!("轮询 Qoder 登录失败: {e}"), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/qoder/campaigns —— body: `{ "uid": "..." }`
///
/// 查询**该账号**的权益活动（「每天领 100 Credits」那类）。**只读**。
///
/// ⚠ 活动是**每账号专属**的 —— 查一个账号只能得到那一个账号的状态。
/// 要拿到全部账号的状态请用 `POST /api/qoder/campaigns-all`。
/// （此前前端只查一个账号就把结果当全局，导致其他账号的活动发现不了。）
async fn api_qoder_campaigns(Json(body): Json<Value>) -> Response {
    let uid = body.get("uid").and_then(Value::as_str).unwrap_or("").to_string();
    match tokio::task::spawn_blocking(move || qoder_login::fetch_campaigns(&uid)).await {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(
            format!("查询 Qoder 权益活动失败: {e}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

/// POST /api/qoder/campaigns-all —— 查询**所有账号**的权益活动。
///
/// # 为什么需要批量接口
///
/// 活动是每账号专属的：A 账号领了，B 账号还有没领的。只查一个账号
/// 会把 B 的活动**永远藏起来**（所有者反馈的正是这个）。
///
/// 单个账号失败不影响其余 —— 结果里逐账号带 `status`。
async fn api_qoder_campaigns_all() -> Response {
    match tokio::task::spawn_blocking(qoder_login::fetch_campaigns_all).await {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(
            format!("批量查询 Qoder 权益活动失败: {e}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

/// POST /api/qoder/claim-all-campaigns —— **一键领取所有账号**的权益活动。
///
/// # 语义
///
///	· 遍历每个账号，各自领各自的活动（活动是每账号专属的）
///	· 单个账号/活动失败不中断其余 —— 用户要的是"能领的都领到"
///	· 逐账号逐活动返回结果，界面据此如实展示
///
/// # ⚠ 写操作，只能由用户显式点击触发
///
/// 不做定时自动领取 —— 那与"用户点一下"不是一回事，
/// 且会让账号表现出非人类的活动模式。
async fn api_qoder_claim_all_campaigns() -> Response {
    match tokio::task::spawn_blocking(qoder_login::claim_all_campaigns).await {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(
            format!("批量领取 Qoder 权益活动失败: {e}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

/// POST /api/qoder/claim-campaign —— body: `{ "uid": "...", "campaignId": "..." }`
///
/// 领取一个权益活动。**只能由用户显式点击触发** —— 不做定时自动领取
///（那与"用户点一下"不是一回事，且会让账号表现出非人类的活动模式）。
async fn api_qoder_claim_campaign(Json(body): Json<Value>) -> Response {
    let uid = body.get("uid").and_then(Value::as_str).unwrap_or("").to_string();
    let cid = body
        .get("campaignId")
        .or_else(|| body.get("campaign_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match tokio::task::spawn_blocking(move || qoder_login::claim_campaign(&uid, &cid)).await {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(
            format!("领取 Qoder 权益失败: {e}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

/// POST /api/qoder/import —— body: `{ "path": "..." }`
async fn api_qoder_import(Json(body): Json<Value>) -> Response {
    let path = body.get("path").and_then(Value::as_str).unwrap_or("").to_string();
    match tokio::task::spawn_blocking(move || qoder_login::import_credentials(&path)).await {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(format!("导入 Qoder 凭证失败: {e}"), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/qoder/import-from-client —— body: `{ "clientDir": "..." }`（可空）
/// 读 Qoder 客户端自己的登录态（**主路径**）。clientDir 为空时自动探测。
async fn api_qoder_import_from_client(Json(body): Json<Value>) -> Response {
    let dir = body
        .get("clientDir")
        .or_else(|| body.get("client_dir"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match tokio::task::spawn_blocking(move || qoder_login::import_from_client(&dir)).await {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(
            format!("从 Qoder 客户端导入失败: {e}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

/// POST /api/qoder/refresh-account —— body: `{ "uid": "..." }`
///
/// 刷新该账号的额度 / 到期时间 / 支持模型，并把结果写回账号库。
async fn api_qoder_refresh_account(Json(body): Json<Value>) -> Response {
    let uid = body.get("uid").and_then(Value::as_str).unwrap_or("").to_string();
    match tokio::task::spawn_blocking(move || qoder_login::refresh_account(&uid)).await {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(
            format!("刷新 Qoder 账号信息失败: {e}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

// ---------------------------------------------------------------------------
// ZCode（Z.AI / 智谱）
// ---------------------------------------------------------------------------
//
// 与 Tauri 命令层一一对应（commands_apps.rs 的 zcode_* 系列）。
// 两者都只做编排：协议实现（OAuth 流程 / 凭证兑换）在 Go 侧 internal/zcode。
//
// 为什么 HTTP 形态也要有：dev 模式（浏览器预览）走的是 HTTP，
// 缺这些路由会让 ZCode 页在 dev 下整页 404 —— 而 dev 正是迭代最快的路径。

/// GET /api/zcode/accounts
async fn api_zcode_list_accounts() -> Response {
    match tokio::task::spawn_blocking(zcode_account::list_with_credentials).await {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(format!("读取 ZCode 账号失败: {e}"), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// GET /api/zcode/summary
async fn api_zcode_summary() -> Response {
    match tokio::task::spawn_blocking(|| zcode_account::summary()).await {
        Ok(v) => json_ok(v),
        Err(e) => json_err(format!("读取 ZCode 概览失败: {e}"), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/zcode/accounts/save —— body: `{ "uid": "...", "patch": {...} }`
async fn api_zcode_save_account(Json(body): Json<Value>) -> Response {
    let uid = body
        .get("uid")
        .or_else(|| body.get("userId"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if uid.trim().is_empty() {
        return json_err("缺少账号 uid".to_string(), StatusCode::BAD_REQUEST);
    }
    let patch = body.get("patch").cloned().unwrap_or_else(|| json!({}));
    match tokio::task::spawn_blocking(move || zcode_account::upsert_account(&uid, &patch)).await {
        Ok(Ok(acc)) => json_ok(json!({ "ok": true, "account": acc.to_view() })),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(format!("保存 ZCode 账号失败: {e}"), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/zcode/accounts/delete —— body: `{ "uid": "..." }`
async fn api_zcode_delete_account(Json(body): Json<Value>) -> Response {
    let uid = body
        .get("uid")
        .or_else(|| body.get("userId"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if uid.trim().is_empty() {
        return json_err("缺少账号 uid".to_string(), StatusCode::BAD_REQUEST);
    }
    match tokio::task::spawn_blocking(move || zcode_account::delete_account(&uid)).await {
        Ok(Ok(removed)) => json_ok(json!({ "ok": removed })),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(format!("删除 ZCode 账号失败: {e}"), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/zcode/import-credential
/// body: `{ "credential": "...", "provider": "zai", "nickname": "..." }`
async fn api_zcode_import_credential(Json(body): Json<Value>) -> Response {
    let credential = body.get("credential").and_then(Value::as_str).unwrap_or("").to_string();
    let provider = body.get("provider").and_then(Value::as_str).unwrap_or("").to_string();
    let nickname = body.get("nickname").and_then(Value::as_str).unwrap_or("").to_string();
    match tokio::task::spawn_blocking(move || {
        zcode_login::import_credential(&credential, &provider, &nickname)
    })
    .await
    {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(format!("导入 ZCode 凭证失败: {e}"), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/zcode/import-dir —— body: `{ "path": "..." }`
async fn api_zcode_import_dir(Json(body): Json<Value>) -> Response {
    let path = body.get("path").and_then(Value::as_str).unwrap_or("").to_string();
    match tokio::task::spawn_blocking(move || zcode_login::import_from_dir(&path)).await {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(format!("批量导入 ZCode 凭证失败: {e}"), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/zcode/scan-local —— 无 body。
///
/// 扫描本机 ZCode 客户端的登录凭证，返回**可导入项**（掩码，不含凭证本体）。
async fn api_zcode_scan_local() -> Response {
    match tokio::task::spawn_blocking(zcode_login::scan_local).await {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(format!("扫描本机 ZCode 凭证失败: {e}"), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/zcode/import-scanned —— body: `{ "indices": [0, 1] }`
///
/// 只传**索引**，不传凭证本体 —— 那样凭证就不必在浏览器里往返
///（会进 DOM、可能被截图、被 devtools 复制）。
async fn api_zcode_import_scanned(Json(body): Json<Value>) -> Response {
    let indices: Vec<usize> = match body.get("indices").and_then(Value::as_array) {
        Some(a) => a.iter().filter_map(Value::as_u64).map(|n| n as usize).collect(),
        None => {
            return json_err(
                "缺少 indices（要导入哪些扫描结果）".to_string(),
                StatusCode::BAD_REQUEST,
            )
        }
    };
    match tokio::task::spawn_blocking(move || zcode_login::import_scanned(&indices)).await {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(format!("导入扫描到的凭证失败: {e}"), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/zcode/refresh-account —— body: `{ "uid": "..." }`
///
/// 刷新该账号的额度 / 到期时间 / 支持模型，并把结果写回账号库。
///
/// 此前 `Client.FetchQuota` / `FetchModels` **没有任何生产者调用** ——
/// 界面上的额度与到期时间因此恒为空。见 `zcode_login::refresh_account`。
async fn api_zcode_refresh_account(Json(body): Json<Value>) -> Response {
    let uid = body.get("uid").and_then(Value::as_str).unwrap_or("").to_string();
    match tokio::task::spawn_blocking(move || zcode_login::refresh_account(&uid)).await {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(
            format!("刷新 ZCode 账号信息失败: {e}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

/// POST /api/zcode/login/start —— body: `{ "provider": "zai" | "bigmodel" }`
async fn api_zcode_login_start(Json(body): Json<Value>) -> Response {
    let provider = body.get("provider").and_then(Value::as_str).unwrap_or("").to_string();
    match tokio::task::spawn_blocking(move || zcode_login::login_start(&provider)).await {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(format!("发起 ZCode 登录失败: {e}"), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/zcode/login/poll —— body: `{ "sessionId": "..." }`
async fn api_zcode_login_poll(Json(body): Json<Value>) -> Response {
    let sid = body
        .get("sessionId")
        .or_else(|| body.get("session_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match tokio::task::spawn_blocking(move || zcode_login::login_poll(&sid)).await {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(format!("轮询 ZCode 登录失败: {e}"), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/accounts/disabled —— 设置账号禁用状态。
///
/// body: `{ "accountId": "...", "disabled": true }`
///
/// 语义：禁用 = 不进网关账号池；签到 / 旅行 / 上报等养号任务照跑。
/// 改完立刻重导出凭证并按需重启网关，做到「点了就生效」。
async fn api_set_account_disabled(Json(body): Json<Value>) -> Response {
    let id = body
        .get("accountId")
        .or_else(|| body.get("account_id"))
        .or_else(|| body.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if id.trim().is_empty() {
        return json_err("缺少账号 id".to_string(), StatusCode::BAD_REQUEST);
    }
    // 缺省视为「禁用」：调用方通常是想禁用它，漏传字段时按更保守的语义处理
    let disabled = body.get("disabled").and_then(Value::as_bool).unwrap_or(true);
    let acc = match account::set_account_disabled(id, disabled) {
        Ok(a) => a,
        Err(e) => return json_err(e, StatusCode::BAD_REQUEST),
    };
    let sync = ai_gateway_core::modules::gateway::sync_and_reload(true).await;
    json_ok(json!({
        "ok": true,
        "account": account::account_meta(&acc),
        "sync": sync,
    }))
}

async fn api_codebuddy_cli_status() -> Response {
    json_ok(codebuddy_cli::status())
}

async fn api_codebuddy_cli_install_helper() -> Response {
    match codebuddy_cli::install_helper() {
        Ok(result) => json_ok(result),
        Err(error) => json_err(error, StatusCode::BAD_REQUEST),
    }
}

async fn api_codebuddy_cli_switch(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    match codebuddy_cli::set_active_account(id) {
        Ok(result) => json_ok(result),
        Err(error) => json_err(error, StatusCode::BAD_REQUEST),
    }
}

async fn api_codebuddy_cn_ide_status() -> Response {
    json_ok(codebuddy_cn_ide::status())
}

async fn api_codebuddy_cn_ide_switch(Json(body): Json<Value>) -> Response {
    let account_id = body
        .get("accountId")
        .or_else(|| body.get("account_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let restart = body.get("restart").and_then(|v| v.as_bool()).unwrap_or(true);
    match codebuddy_cn_ide::switch_account(account_id, restart) {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_codebuddy_cn_ide_detect() -> Response {
    match codebuddy_cn_ide::detect_current_account() {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}


async fn api_delete(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    match account::delete_account(id) {
        Ok(()) => json_ok(json!({ "ok": true })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_import_local() -> Response {
    match account::import_local_all() {
        Ok(list) => json_ok(json!({
            "ok": true,
            "imported": list.len(),
            "accounts": list,
            "account": list.first().cloned(),
        })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// GET /api/import-local/scan —— 扫描本机全部历史登录态（当前 + 快照 + 备份）。
async fn api_scan_local() -> Response {
    let scan = account::scan_local_accounts();
    json_ok(json!({
        "ok": true,
        "candidates": scan.candidates,
        "total": scan.candidates.len(),
        "filesScanned": scan.files_scanned,
        "usable": scan.usable,
        "authDir": scan.auth_dir,
        "backupDir": scan.backup_dir,
    }))
}

/// POST /api/import-local/selected —— 按路径/索引批量导入本机账号。
async fn api_import_local_selected(Json(body): Json<Value>) -> Response {
    let paths: Vec<String> = body
        .get("paths")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let indexes: Vec<usize> = body
        .get("indexes")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_u64().map(|n| n as usize))
                .collect()
        })
        .unwrap_or_default();
    match account::import_local_selected(&paths, &indexes) {
        Ok(result) => json_ok(json!({
            "ok": true,
            "imported": result.imported,
            "added": result.added,
            "updated": result.updated,
            "outcomes": result.outcomes,
        })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

// ---------------------------------------------------------------------------
// 导出 / 导入账号
// ---------------------------------------------------------------------------

async fn api_export_accounts(Json(body): Json<Value>) -> Response {
    let ids: Vec<String> = body
        .get("accountIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    match export_import::export_accounts(&ids) {
        Ok(records) => json_ok(json!({ "ok": true, "accounts": records })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_export_accounts_to_path(Json(body): Json<Value>) -> Response {
    let ids: Vec<String> = body
        .get("accountIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let path = body
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match export_import::export_accounts_to_path(&ids, &path) {
        Ok(path) => json_ok(json!({ "ok": true, "path": path })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_preview_import(Json(body): Json<Value>) -> Response {
    let text = body
        .get("fileText")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match export_import::preview_accounts(&text) {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_import(Json(body): Json<Value>) -> Response {
    let text = body
        .get("fileText")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let indexes: Vec<usize> = body
        .get("indexes")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_u64().map(|n| n as usize))
                .collect()
        })
        .unwrap_or_default();
    match export_import::import_accounts(&text, &indexes) {
        Ok(result) => json_ok(json!({
            "ok": true,
            "imported": result.imported,
            "skipped": result.skipped,
            "overwritten": result.overwritten,
        })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

// ---------------------------------------------------------------------------
// OAuth 登录
// ---------------------------------------------------------------------------

/// `region` 放在 query 里，缺省国服；body 可选，避免老前端（无 body）被拒。
async fn api_oauth_start(
    Query(params): Query<HashMap<String, String>>,
    body: Option<Json<Value>>,
) -> Response {
    let from_body = body
        .as_ref()
        .and_then(|Json(v)| v.get("region"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let key = if from_body.is_empty() {
        params.get("region").map(String::as_str).unwrap_or("")
    } else {
        from_body
    };
    match oauth::oauth_start(ai_gateway_core::modules::config::Region::from_key(key)).await {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_oauth_status(Json(body): Json<Value>) -> Response {
    let login_id = body
        .get("loginId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    json_ok(oauth::oauth_poll(&login_id).await)
}

// ---------------------------------------------------------------------------
// 切换
// ---------------------------------------------------------------------------

async fn api_switch(Json(body): Json<Value>) -> Response {
    let account_id = body
        .get("accountId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if account_id.trim().is_empty() {
        return json_err("缺少 accountId".to_string(), StatusCode::BAD_REQUEST);
    }
    let restart = body
        .get("restart")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let share_sessions = body
        .get("shareSessions")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let copy_ids: Vec<String> = body
        .get("copySessionIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    {
        let mut running = SWITCH_RUNNING.lock().unwrap();
        if *running {
            return json_err("已有切换任务进行中".to_string(), StatusCode::CONFLICT);
        }
        *running = true;
        *SWITCH_PROGRESS.lock().unwrap() = Some("开始切换账号…".to_string());
    }

    let progress: switch::ProgressFn = Box::new(|msg| {
        *SWITCH_PROGRESS.lock().unwrap() = Some(msg.to_string());
    });

    let result = tokio::task::spawn_blocking(move || {
        switch::switch_account(
            Some(&progress),
            &account_id,
            restart,
            share_sessions,
            &copy_ids,
        )
    })
    .await;

    *SWITCH_RUNNING.lock().unwrap() = false;

    match result {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn api_switch_progress() -> Response {
    let p = SWITCH_PROGRESS.lock().unwrap().clone();
    let running = *SWITCH_RUNNING.lock().unwrap();
    json_ok(json!({ "running": running, "progress": p }))
}

// ---------------------------------------------------------------------------
// 会话
// ---------------------------------------------------------------------------

async fn api_sessions() -> Response {
    match session::current_user_uid() {
        Some(uid) => json_ok(json!({
            "sessions": session::list_sessions_for_user(&uid),
            "current": uid,
        })),
        None => json_ok(json!({ "sessions": [], "current": null })),
    }
}

async fn api_copy_sessions(Json(body): Json<Value>) -> Response {
    let target_account_id = body
        .get("targetAccountId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let session_ids: Vec<String> = body
        .get("sessionIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let Some(target) = account::find_account(&target_account_id) else {
        return json_err("目标账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    let source_uid = session::current_user_uid();
    let result = session::copy_sessions_for_switch(&target, &session_ids);
    json_ok(json!({
        "sourceUid": source_uid,
        "targetUid": target.get("uid"),
        "copied": result,
    }))
}

// ---------------------------------------------------------------------------
// 签到 / 保活
// ---------------------------------------------------------------------------

async fn api_checkin_status() -> Response {
    let list = account::load_accounts();
    let mut items = Vec::new();
    for acc in &list {
        let status = checkin::get_checkin_status(acc).await;
        items.push(checkin_status_item(acc, status));
    }
    json_ok(json!({ "accounts": items }))
}

fn checkin_status_item(account: &Value, mut status: Value) -> Value {
    status["accountId"] = account.get("id").cloned().unwrap_or(Value::Null);
    status["email"] = json!(account::account_display_name(account));
    status
}

async fn api_credits(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    let Some(acc) = account::find_account(id) else {
        return json_err("账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    json_ok(credits::get_credit_expiry(&acc).await)
}

fn query_flag_enabled(query: Option<&str>, name: &str) -> bool {
    query.unwrap_or("").split('&').any(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, "true"));
        key == name && matches!(value, "" | "1" | "true" | "yes")
    })
}

async fn api_credit_statistics(RawQuery(query): RawQuery) -> Response {
    json_ok(credit_usage::get_statistics(query_flag_enabled(query.as_deref(), "refresh")).await)
}

async fn api_token_statistics(RawQuery(query): RawQuery) -> Response {
    let days = query.as_deref().and_then(|value| {
        value.split('&').find_map(|part| {
            part.strip_prefix("days=")?.parse::<i64>().ok()
        })
    });
    match tokio::task::spawn_blocking(move || token_stats::get_statistics(days)).await {
        Ok(statistics) => json_ok(statistics),
        Err(error) => json_err(
            format!("扫描 Token 统计失败: {error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

async fn api_checkin(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    let Some(acc) = account::find_account(id) else {
        return json_err("账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    json_ok(checkin::checkin_account(&acc).await)
}

async fn api_checkin_all() -> Response {
    json_ok(checkin::run_checkin_all().await)
}

/// 一键旅行：全部账号走一趟巡检。手动触发不检查「自动旅行」开关。
async fn api_travel_run() -> Response {
    json_ok(travel::run_travel_now().await)
}

/// 单账号领养：只领养，不派猫、不领奖。
async fn api_travel_adopt(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    let Some(acc) = account::find_account(id) else {
        return json_err("账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    json_ok(travel::adopt_for_account(&acc).await)
}

async fn api_checkin_config() -> Response {
    json_ok(config::load_checkin_config())
}

async fn api_save_checkin_config(Json(body): Json<Value>) -> Response {
    let submitted = body.get("config").unwrap_or(&body);
    match config::save_checkin_config(submitted) {
        Ok(()) => json_ok(config::load_checkin_config()),
        Err(e) => json_err(e.to_string(), StatusCode::BAD_REQUEST),
    }
}

async fn api_checkin_logs() -> Response {
    json_ok(json!({ "logs": config::load_checkin_logs() }))
}

/// GET /api/settings/retention —— 记录保留天数设置。
///
/// 同时返回预设档位，避免前端硬编码选项（两处各写一份容易不一致）。
async fn api_record_retention() -> Response {
    json_ok(json!({
        "days": config::record_retention_days(),
        "defaultDays": config::RECORD_RETENTION_DEFAULT_DAYS,
        "minDays": config::RECORD_RETENTION_MIN_DAYS,
        "maxDays": config::RECORD_RETENTION_MAX_DAYS,
        "presets": config::RECORD_RETENTION_PRESETS
            .iter()
            .map(|(d, label)| json!({ "days": d, "label": label }))
            .collect::<Vec<_>>(),
    }))
}

/// POST /api/settings/retention —— 保存记录保留天数。
///
/// 返回归一化后的实际生效值：用户填越界值时立刻看到真实数字，
/// 而不是「界面显示 0、实际按 60 天清理」。
async fn api_save_record_retention(Json(body): Json<Value>) -> Response {
    let Some(days) = body.get("days").and_then(Value::as_i64) else {
        return json_err(
            "缺少 days 字段或类型不是整数".to_string(),
            StatusCode::BAD_REQUEST,
        );
    };
    match config::set_record_retention_days(days) {
        Ok(applied) => json_ok(json!({ "days": applied })),
        Err(e) => json_err(e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// 把查询参数解析成 (account_id, from, to, kinds, limit)。
///
/// GET 走 query string、POST 走 JSON body，两者解析逻辑一致 ——
/// 抽出来避免两处口径不一致（例如一处认 kinds=csv、另一处只认数组）。
fn parse_record_query(
    account_id: Option<&str>,
    from: Option<i64>,
    to: Option<i64>,
    kinds: Option<&Value>,
    limit: Option<i64>,
) -> (String, i64, i64, Vec<String>, usize) {
    let kinds_vec: Vec<String> = match kinds {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(|s| s.to_string())
            .collect(),
        // 逗号分隔字符串同样接受：便于 curl / 浏览器直接拼 URL
        Some(Value::String(s)) => s
            .split(',')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(|x| x.to_string())
            .collect(),
        _ => Vec::new(),
    };
    let limit = match limit.unwrap_or(500) {
        n if n <= 0 => 500,
        n => n.min(5000) as usize,
    };
    (
        account_id.unwrap_or("").to_string(),
        from.unwrap_or(0),
        to.unwrap_or(0),
        kinds_vec,
        limit,
    )
}

/// GET /api/account-records —— 按账号 / 日期 / 类型查询账号记录。
///
/// query: accountId=xxx&from=<ms>&to=<ms>&kinds=task,credit&limit=500
async fn api_account_records(Query(q): Query<HashMap<String, String>>) -> Response {
    let kinds_val = q
        .get("kinds")
        .map(|s| Value::String(s.clone()))
        .unwrap_or(Value::Null);
    let (account_id, from, to, kinds, limit) = parse_record_query(
        q.get("accountId").map(String::as_str),
        q.get("from").and_then(|v| v.parse::<i64>().ok()),
        q.get("to").and_then(|v| v.parse::<i64>().ok()),
        Some(&kinds_val),
        q.get("limit").and_then(|v| v.parse::<i64>().ok()),
    );
    json_ok(ai_gateway_core::modules::account_records::query_records(
        &account_id, from, to, &kinds, limit,
    ))
}

/// POST /api/account-records —— 同上，但参数走 JSON body（便于传数组 kinds）。
async fn api_account_records_query(Json(body): Json<Value>) -> Response {
    let (account_id, from, to, kinds, limit) = parse_record_query(
        body.get("accountId").and_then(Value::as_str),
        body.get("from").and_then(Value::as_i64),
        body.get("to").and_then(Value::as_i64),
        body.get("kinds"),
        body.get("limit").and_then(Value::as_i64),
    );
    json_ok(ai_gateway_core::modules::account_records::query_records(
        &account_id, from, to, &kinds, limit,
    ))
}

/// POST /api/account-records/backfill —— 把历史签到日志回填为账号记录（幂等）。
async fn api_backfill_account_records() -> Response {
    match ai_gateway_core::modules::account_records::backfill_from_checkin_logs() {
        Ok(added) => json_ok(json!({ "added": added })),
        Err(e) => json_err(e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn api_travel_status() -> Response {
    travel::reconcile_due_travel(None).await;
    let items = account::load_accounts()
        .iter()
        .map(|acc| {
            let id = acc.get("id").and_then(Value::as_str).unwrap_or("");
            let mut value = travel::travel_display(id);
            value["accountId"] = acc.get("id").cloned().unwrap_or(Value::Null);
            value["email"] = json!(account::account_display_name(acc));
            value
        })
        .collect::<Vec<_>>();
    json_ok(json!({ "accounts": items }))
}

async fn api_travel_config() -> Response {
    json_ok(config::load_travel_config())
}

async fn api_save_travel_config(Json(body): Json<Value>) -> Response {
    let submitted = body.get("config").unwrap_or(&body);
    match config::save_travel_config(submitted) {
        Ok(()) => {
            let saved = config::load_travel_config();
            if saved.get("enabled").and_then(Value::as_bool) == Some(true) {
                tokio::spawn(async {
                    let _ = travel::run_travel_cycle().await;
                    let _ = travel::run_travel_claim_cycle().await;
                });
            }
            json_ok(saved)
        }
        Err(e) => json_err(e.to_string(), StatusCode::BAD_REQUEST),
    }
}

async fn api_refresh_token(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    let Some(acc) = account::find_account(id) else {
        return json_err("账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    json_ok(refresh::refresh_account_token(acc).await)
}

// ---------------------------------------------------------------------------
// 自动轮换（CodeBuddy CLI）
// ---------------------------------------------------------------------------

async fn api_rotate_config() -> Response {
    json_ok(config::load_auto_rotate_config())
}

async fn api_save_rotate_config(Json(body): Json<Value>) -> Response {
    match config::save_auto_rotate_config(&body) {
        Ok(()) => json_ok(json!({ "ok": true, "config": config::load_auto_rotate_config() })),
        Err(e) => json_err(e.to_string(), StatusCode::BAD_REQUEST),
    }
}

async fn api_rotate_status() -> Response {
    json_ok(rotate::rotate_status())
}

async fn api_rotate_run() -> Response {
    json_ok(rotate::run_rotate_cycle().await)
}

async fn api_rotate_logs() -> Response {
    json_ok(json!({ "logs": rotate::rotate_logs() }))
}

// ---------------------------------------------------------------------------
// 更新
// ---------------------------------------------------------------------------

async fn api_update_check() -> Response {
    json_ok(update::update_check(None, false).await)
}

async fn api_update_config() -> Response {
    json_ok(update::load_github_config())
}

/// 从 POST body 里取出真正的配置对象（剥掉 `{config: {...}}` 包装层）。
///
/// **必须剥掉包装层**：前端（src/lib/api.ts::saveGithubConfig）与 Tauri command
/// 的约定是把配置放在 `config` 键里，而 webui 的 POST body 就是这个调用参数本身。
/// 不剥的话 `save_github_config` 读到的是一份**没有 owner/repo/proxy/proxy_scope
/// 的壳**，于是：地址被写成空串、proxy_scope 回落默认值 —— 用户点「保存代理」后
/// 配置反而被清空，且返回 200 毫无报错（本轮加三个开关时实测发现）。
///
/// 三种形状都要能吃：
///   - `{"config": {...}}` → 内层（前端 / Tauri 的真实调用形状）
///   - 裸配置 `{...}`       → 整个 body（其它调用方 / 手工 curl）
///   - `{"config": null}`   → 回落整个 body
///
/// 第三种用 `is_object()` 而不是「键存在就用」：`get()` 对 null 返回
/// `Some(Null)`，若直接 unwrap_or 会得到一个 Value::Null 当配置 —— 同样是把用户
/// 的设置清掉，只是换成另一种错法。
fn submitted_update_config(body: &Value) -> &Value {
    match body.get("config") {
        Some(inner) if inner.is_object() => inner,
        _ => body,
    }
}

async fn api_save_update_config(Json(body): Json<Value>) -> Response {
    let submitted = submitted_update_config(&body);
    match update::save_github_config(submitted) {
        Ok(()) => json_ok(json!({ "ok": true, "config": update::load_github_config() })),
        Err(e) => json_err(e.to_string(), StatusCode::BAD_REQUEST),
    }
}

// ---------------------------------------------------------------------------
// 静态前端
// ---------------------------------------------------------------------------

fn content_type(path: &str) -> &'static str {
    if path.ends_with(".js") || path.ends_with(".mjs") {
        "text/javascript"
    } else if path.ends_with(".css") {
        "text/css"
    } else if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".json") {
        "application/json"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else {
        "application/octet-stream"
    }
}

async fn static_handler(uri: Uri) -> Response {
    let mut path = uri.path().trim_start_matches('/').to_string();
    if path.is_empty() || path == "index.html" {
        path = "index.html".to_string();
    }
    // 前端路由回退到 index.html
    let data = Assets::get(&path).or_else(|| Assets::get("index.html"));
    match data {
        Some(f) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type(&path))
            .body(Body::from(f.data.into_owned()))
            .unwrap(),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("not found"))
            .unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::checkin_status_item;
    use serde_json::json;

    #[test]
    fn web_checkin_status_keeps_account_identity() {
        let item = checkin_status_item(
            &json!({"id": "account-1", "email": "user@example.com"}),
            json!({"ok": true, "todayCheckedIn": true}),
        );

        assert_eq!(item["accountId"], "account-1");
        assert_eq!(item["email"], "user@example.com");
        assert_eq!(item["todayCheckedIn"], true);
    }

    #[test]
    fn web_checkin_status_preserves_failure_state() {
        let item = checkin_status_item(
            &json!({"id": "account-2"}),
            json!({"ok": false, "todayCheckedIn": false, "error": "status failed"}),
        );

        assert_eq!(item["accountId"], "account-2");
        assert_eq!(item["ok"], false);
        assert_eq!(item["error"], "status failed");
    }
}

// ---------------------------------------------------------------------------
// 网关（workbuddy2api）集成
// ---------------------------------------------------------------------------

/// GET /api/gateway/status —— 网关运行态 + 账号池详情 + 可执行文件探测结果。
async fn api_gateway_status() -> Response {
    json_ok(ai_gateway_core::modules::gateway::gateway_status().await)
}

/// GET /api/gateway/config —— 读取网关配置（含可执行文件是否存在）。
async fn api_gateway_config() -> Response {
    let cfg = ai_gateway_core::modules::gateway::load_gateway_config();
    let exe = ai_gateway_core::modules::gateway::resolve_gateway_exe();
    json_ok(json!({
        "config": cfg,
        "exeFound": exe.is_some(),
        "exePath": exe.map(|p| p.to_string_lossy().to_string()),
        "authDir": ai_gateway_core::modules::gateway::gateway_auth_dir().to_string_lossy(),
    }))
}

/// 从请求体里取出「手动模式勾选的账号」。
///
/// 兼容三种写法（都指向同一语义，只是历史版本不同）：
///   - `manualUids: ["uid-1", "uid-2"]`  ← 新格式
///   - `manual_uids: [...]`              ← snake_case
///   - `pinnedUid: "uid-1"`              ← 旧版单值，等价于「只勾了那一个」
///
/// 抽成函数是为了让两个入口（配置保存、模式切换）口径一致 ——
/// 分别解析容易出现「一边认新格式、一边只认旧格式」的诡异差异。
fn extract_manual_uids(body: &Value) -> Vec<String> {
    for key in ["manualUids", "manual_uids"] {
        if let Some(items) = body.get(key).and_then(Value::as_array) {
            let list: Vec<String> = items
                .iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            return list;
        }
    }
    body.get("pinnedUid")
        .or_else(|| body.get("pinned_uid"))
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .into_iter()
        .collect()
}

/// 前端 camelCase → 配置 snake_case（`POST /api/gateway/config` 的入参归一化）。
///
/// 必须逐个显式映射而不是「原样透传」：原样写下去会在配置文件里留下
/// camelCase 键，而读取方（write_native_config / 前端 applyConfig）找的是
/// snake_case —— 表现为「保存成功但值丢了」。
///
/// **这张表必须与 `src/lib/api.ts::saveGatewayConfig` 的转发键逐条对齐**：
/// 那边把 snake_case 的配置字段显式改成 camelCase 再发给宿主（桌面版 Tauri
/// 按 `rename_all = "camelCase"` 取值），WebUI 版收下同一份 camelCase JSON
/// 后，就靠这里的表转回 snake_case。**表里漏一个键，那一个字段就静默丢失**
/// 且保存接口照样返回 200。
///
/// 抽成独立函数是为了能单测：内联在 handler 里就只能起 HTTP 服务才验得到，
/// 而这类「键名对不上」的缺陷恰恰是最容易漏测、后果又最隐蔽的一种
///（保存看似成功，实际值从来没进过配置文件）。
fn normalize_gateway_config_body(body: &Value) -> Value {
    let mut body = body.clone();
    let aliases: &[(&str, &str)] = &[
        // ---- 与本轮「接口配置自动保存」直接相关：漏掉这两个键，
        // 界面上会显示「已自动保存」，磁盘上却是空的（或残留一个无用的
        // camelCase 键），用户重启网关后 API Key 归零、客户端全部 401。
        ("apiKey", "api_key"),
        ("autoStart", "auto_start"),
        ("pinnedUid", "pinned_uid"),
        ("manualUids", "manual_uids"),
        ("activityHours", "activity_hours"),
        ("nightowlHours", "nightowl_hours"),
        ("schoolHours", "school_hours"),
        ("trialHours", "trial_hours"),
        ("activityEnabled", "activity_enabled"),
        ("nightowlEnabled", "nightowl_enabled"),
        ("schoolEnabled", "school_enabled"),
        ("trialEnabled", "trial_enabled"),
        ("activityReportCount", "activity_report_count"),
        // 自定义系统提示词：与 save_gateway_config 的 Tauri 版同一组字段，
        // 漏掉会让 WebUI 用户「配了提示词却总被重置」。
        ("promptMode", "prompt_mode"),
        ("promptFile", "prompt_file"),
    ];
    for (camel, snake) in aliases {
        if let Some(map) = body.as_object_mut() {
            if let Some(v) = map.get(*camel).cloned() {
                map.insert((*snake).to_string(), v);
                // 删掉 camelCase 键：否则它会被浅合并原样写进配置文件，
                // 下次读取时既无用又会让人误以为配置生效了。
                map.remove(*camel);
            }
        }
    }
    body
}

/// POST /api/gateway/config —— 保存网关配置。
async fn api_save_gateway_config(Json(body): Json<Value>) -> Response {
    let body = normalize_gateway_config_body(&body);
    match ai_gateway_core::modules::gateway::save_gateway_config(&body) {
        Ok(v) => json_ok(json!({ "config": v })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// POST /api/gateway/mode —— 切换工作模式并立即生效（重导出凭证 + 按需重启）。
///
/// 与 /api/gateway/config 的区别：后者只写配置文件，而网关账号池是启动时
/// 建立的，改完必须手动重启才生效。此接口把三步合成一步。
///
/// body：`{ mode, manualUids: [...] }`。
/// 兼容旧的 `pinnedUid` 单值形式（等价于「只勾那一个」）。
async fn api_switch_gateway_mode(Json(body): Json<Value>) -> Response {
    let mode = body.get("mode").and_then(Value::as_str).unwrap_or("balance");
    let uids = extract_manual_uids(&body);
    let mode = ai_gateway_core::modules::gateway::GatewayMode::from_str(mode);
    let result = ai_gateway_core::modules::gateway::switch_mode(mode, uids).await;
    if result.get("ok").and_then(Value::as_bool) == Some(false) {
        let msg = result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("切换模式失败")
            .to_string();
        return json_err(msg, StatusCode::BAD_REQUEST);
    }
    json_ok(result)
}

/// POST /api/gateway/allowed-model —— 设置「限制使用的模型」白名单。
///
/// body 同时接受**两种形状**（向后兼容）：
///   - `{ "models": ["a","b"] }` / `{ "allowedModels": ["a","b"] }` —— 新界面（多选）
///   - `{ "model": "a" }` / `{ "allowedModel": "a" }`                —— 旧界面（单选）
///
/// 空数组 / 空串 = 清除限制（= 全部放行）。网关运行时自动重启以生效
///（模型限制由网关启动时读取）。
async fn api_set_allowed_model(Json(body): Json<Value>) -> Response {
    let models = allowed_models_from_body(&body);
    let result = ai_gateway_core::modules::gateway::set_allowed_models(&models).await;
    if result.get("ok").and_then(Value::as_bool) == Some(false) {
        let msg = result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("设置模型限制失败")
            .to_string();
        return json_err(msg, StatusCode::BAD_REQUEST);
    }
    json_ok(result)
}

/// POST /api/gateway/model-platforms —— 设置「模型 → 允许的平台」白名单。
///
/// body：`{ "platforms": { "glm-5.2": ["zcode"] } }`
/// 空对象 = 恢复不限制。网关运行时自动重启以生效
///（白名单由网关启动时读取，光落盘不会改变正在运行的进程）。
///
/// 需求原文（所有者）：「我希望可以加上 **平台区分使用哪个平台的模型**」。
async fn api_set_model_platforms(Json(body): Json<Value>) -> Response {
    // 兼容两种写法：`platforms` 与 `modelPlatforms`
    let raw = body
        .get("platforms")
        .or_else(|| body.get("modelPlatforms"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let result = ai_gateway_core::modules::gateway::set_model_platforms(&raw).await;
    if result.get("ok").and_then(Value::as_bool) == Some(false) {
        let msg = result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("设置模型平台限制失败")
            .to_string();
        return json_err(msg, StatusCode::BAD_REQUEST);
    }
    json_ok(result)
}

/// 从请求体里读出模型名单，兼容「多值数组」与「单值字符串」两种写法。
///
/// 抽成独立函数（而不是内联在 handler 里）是为了能单测这段兼容逻辑：
/// 它是本路由向后兼容的全部依据，内联后只能靠起 HTTP 服务才能验证。
///
/// 顺序上**数组优先于单值**：两者同时出现在请求体里时，数组是新界面的意图，
/// 单值多半是旧字段残留；取数组更符合用户当下的操作。
fn allowed_models_from_body(body: &Value) -> Vec<String> {
    let pick_array = |key: &str| -> Option<Vec<String>> {
        body.get(key).and_then(Value::as_array).map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
    };
    if let Some(list) = pick_array("models").or_else(|| pick_array("allowedModels")) {
        return list;
    }
    // 单值形状：老界面的 `set_allowed_model(model: String)` 走的是这条。
    body.get("model")
        .or_else(|| body.get("allowedModel"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| vec![s.to_string()])
        .unwrap_or_default()
}

/// POST /api/gateway/start —— 启动网关（可选 body.port 指定端口）。
///
/// body 为空时沿用已保存的配置；带 port 时先落盘再启动，这样
/// 「前端选端口 → 启动」一步完成，不需要用户先手动保存。
async fn api_gateway_start(Json(body): Json<Value>) -> Response {
    if let Some(port) = body.get("port").and_then(Value::as_u64) {
        if !(1..=65535).contains(&port) {
            return json_err("端口号需在 1-65535 之间".to_string(), StatusCode::BAD_REQUEST);
        }
        let patch = json!({ "port": port as u16 });
        if let Err(e) = ai_gateway_core::modules::gateway::save_gateway_config(&patch) {
            return json_err(e, StatusCode::BAD_REQUEST);
        }
    }
    let cfg = ai_gateway_core::modules::gateway::load_gateway_config();
    match ai_gateway_core::modules::gateway::start_gateway(&cfg).await {
        Ok(v) => {
            ai_gateway_core::modules::gateway::update_runtime_state("started", None);
            json_ok(v)
        }
        Err(e) => {
            ai_gateway_core::modules::gateway::update_runtime_state("failed", Some(e.clone()));
            json_err(e, StatusCode::BAD_REQUEST)
        }
    }
}

/// POST /api/gateway/stop —— 停止网关。
async fn api_gateway_stop() -> Response {
    let r = ai_gateway_core::modules::gateway::stop_gateway();
    ai_gateway_core::modules::gateway::update_runtime_state("stopped", None);
    json_ok(r)
}

/// POST /api/gateway/restart —— 重启网关（应用新配置/新账号）。
///
/// ⚠ 走 `restart_gateway_serialized`（带互斥），不要在这里各写一份 stop+start：
/// 后台自动同步与 Tauri 命令也走同一条路径，分散写会让它们互相插队 ——
/// 用户点「重启」时看到「网关已在运行」（2026-09-22 所有者现场）。
async fn api_gateway_restart() -> Response {
    match ai_gateway_core::modules::gateway::restart_gateway_serialized().await {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// POST /api/gateway/sync —— 双向同步账号；body.autoReload=true 时按需重启网关。
async fn api_gateway_sync(Json(body): Json<Value>) -> Response {
    let reload = body.get("autoReload").and_then(Value::as_bool).unwrap_or(true);
    json_ok(ai_gateway_core::modules::gateway::sync_and_reload(reload).await)
}

/// POST /api/gateway/port-check —— 检测端口可用性，并给出建议端口。
///
/// body: { "port": 7863 }
async fn api_gateway_port_check(Json(body): Json<Value>) -> Response {
    let port = body
        .get("port")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if port == 0 || port > 65535 {
        return json_err("端口号需在 1-65535 之间".to_string(), StatusCode::BAD_REQUEST);
    }
    json_ok(ai_gateway_core::modules::gateway::inspect_port(port as u16))
}

/// POST /api/gateway/port-holder —— 查询占用端口的进程。
///
/// body: { "port": 7863 }
///
/// 与 port-check 的分工：port-check 在页面挂载/端口变化时就会被调用（热路径），
/// 因此**不查进程**；本接口由用户主动点开「结束占用进程」对话框时才调用，
/// 此时才值得付出 spawn netstat/tasklist/powershell（macOS 上是 lsof）的开销。
///
/// 响应里的 `hint` 是「查不到占用者」时给用户的**可操作**排查命令（按服务端
/// 所在系统生成）—— WebUI 模式下浏览器与网关可能不在同一台机器上，因此不能
/// 由前端按 UA 猜平台。
async fn api_gateway_port_holder(Json(body): Json<Value>) -> Response {
    let port = body.get("port").and_then(Value::as_u64).unwrap_or(0);
    if port == 0 || port > 65535 {
        return json_err("端口号需在 1-65535 之间".to_string(), StatusCode::BAD_REQUEST);
    }
    let holder = ai_gateway_core::modules::gateway::port_holder(port as u16);
    json_ok(json!({
        "port": port,
        "holder": holder.unwrap_or(Value::Null),
        "hint": ai_gateway_core::modules::gateway::port_holder_manual_hint(port as u16),
    }))
}

/// POST /api/gateway/port-kill —— 结束占用端口的进程，让本网关接管该端口。
///
/// body: { "port": 7863 }
///
/// 由前端在用户**明确确认**后调用；后端仍会拒绝几类危险目标
///（自身进程、本程序启动的网关），因此误调用不会造成状态不一致。
async fn api_gateway_port_kill(Json(body): Json<Value>) -> Response {
    let port = body.get("port").and_then(Value::as_u64).unwrap_or(0);
    if port == 0 || port > 65535 {
        return json_err("端口号需在 1-65535 之间".to_string(), StatusCode::BAD_REQUEST);
    }
    match ai_gateway_core::modules::gateway::kill_port_holder(port as u16) {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

// ---------------------------------------------------------------------------
// 一键导入：接入本机 AI 客户端
// ---------------------------------------------------------------------------

/// 读取网关根地址（不带 /v1）与 API Key。
fn gateway_root_and_key() -> (String, String) {
    let cfg = ai_gateway_core::modules::gateway::load_gateway_config();
    let port = cfg.get("port").and_then(Value::as_u64).unwrap_or(7863);
    let api_key = cfg
        .get("api_key")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    (format!("http://127.0.0.1:{port}"), api_key)
}

/// GET /api/gateway/models?refresh=1 —— 返回网关支持的模型列表（优先动态查询）。
///
/// `refresh=1/true` 时**强制重拉**：让网关清掉按区域的失败负缓存。
/// 界面上的「刷新模型列表」用它 —— 那段「国服未检测到可用真值…稍后点刷新
/// 重试即可确认」的文案承诺了刷新能重新确认，而带缓存的路径在失败后
/// 5 分钟内连试都不试 ⇒ 点了没变化（2026-09-20 实测缺陷）。
///
/// ⚠ 必须与 Tauri 命令 `get_gateway_models(refresh)` 行为一致：
/// 同一个界面按钮在桌面版与 webui 版下要走同一条路径，否则"刷新无效"
/// 只会在其中一种形态下复现 —— 那种不一致极难查。
async fn api_gateway_models(Query(params): Query<HashMap<String, String>>) -> Response {
    let force = params
        .get("refresh")
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "1" || v == "true" || v == "yes"
        })
        .unwrap_or(false);
    let models = if force {
        ai_gateway_core::modules::gateway::fetch_models_refreshed().await
    } else {
        ai_gateway_core::modules::gateway::fetch_models().await
    };
    json_ok(json!({ "models": models }))
}

/// GET /api/gateway/usage?days=7 —— 网关累计 Token 用量（days 省略 = 全部历史）。
async fn api_gateway_usage(Query(params): Query<HashMap<String, String>>) -> Response {
    let days = params
        .get("days")
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|d| *d > 0);
    json_ok(ai_gateway_core::modules::gateway::fetch_usage(days).await)
}

/// POST /api/gateway/task-run —— 手动触发网关侧一轮养号任务。
///
/// body: { "task": "activity" | "nightowl" | "school" | "trial" | "growthmap",
///         "accountId": "<uid>" }   ← 可选
///
/// `accountId` 决定作用范围：
///   - 省略 / 空 → **全部账号**（右上角「一键操作」的入口）
///   - 给出 uid  → **只作用于该账号**（账号卡片菜单里的入口）
///
/// 加这个参数是因为原实现只有「全部账号」一种语义，而账号菜单里的入口位置
/// 暗示的是一对一 —— 用户在某个账号的卡片上点「活跃上报」，跑的却是整池。
///
/// 与 /api/checkin/all 的分工：那个是宿主自己实现的签到，本接口只是把请求
/// 转给网关的 /tasks/run —— 这些任务的实现（上报事件形状、夜猫时间窗、
/// 只领已达标奖励的边界）都在网关里且已有测试覆盖，宿主不复制业务逻辑。
async fn api_gateway_task_run(Json(body): Json<Value>) -> Response {
    let task = body
        .get("task")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if task.is_empty() {
        return json_err("缺少 task 参数".to_string(), StatusCode::BAD_REQUEST);
    }
    let account_id = body
        .get("accountId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    json_ok(ai_gateway_core::modules::gateway::run_task_for(&task, &account_id).await)
}

/// POST /api/gateway/growth-task —— 成长任务「一键完成」。
///
/// body: { "action": "list" | "run" | "run-all", "accountId": "...", "taskCode": "..." }
///
/// 与 /api/gateway/task-run 的分工：那个触发的是「网关侧的养号任务」
///（活跃上报 / 夜猫子 / 开学季 / trial，作用于账号池）；本接口操作的是
/// **成长任务体系**（18 个任务，按账号逐个推进 + 领奖），二者是不同的东西。
///
/// 直接在返回值里带 `ok` 字段而不抛 HTTP 错误：成长任务的失败往往是
/// 「部分账号成功、部分失败」，整条请求变红会让界面拿不到已完成的部分。
async fn api_gateway_growth_task(Json(body): Json<Value>) -> Response {
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if action.is_empty() {
        return json_err(
            "缺少 action 参数（list / run / run-all）".to_string(),
            StatusCode::BAD_REQUEST,
        );
    }
    let account_id = body
        .get("accountId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let task_code = body
        .get("taskCode")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    json_ok(ai_gateway_core::modules::gateway::growth_task(&action, &account_id, &task_code).await)
}

/// GET /api/gateway/agents —— 探测全部客户端的安装与配置状态。
async fn api_agents_detect() -> Response {
    let (base, api_key) = gateway_root_and_key();
    let targets = ai_gateway_core::modules::agent_import::detect_all(&base, &api_key);
    json_ok(json!({
        "base": base,
        "hasApiKey": !api_key.is_empty(),
        "targets": targets.iter().map(|t| json!({
            "id": t.id,
            "label": t.label,
            "installed": t.installed,
            "configured": t.configured,
            "configPath": t.config_path,
            "note": t.note,
            "version": t.version,
        })).collect::<Vec<_>>(),
    }))
}

/// POST /api/gateway/agents/import —— 把网关接入指定客户端（支持多模型）。
///
/// body: { "target": "codex", "models": ["glm-5.2", "deepseek-v4-flash"] }
async fn api_agents_import(Json(body): Json<Value>) -> Response {
    let target = body
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if target.is_empty() {
        return json_err("缺少 target 参数".to_string(), StatusCode::BAD_REQUEST);
    }

    let (base, api_key) = gateway_root_and_key();
    if api_key.trim().is_empty() {
        return json_err(
            "请先在网关设置里填写 API Key：客户端需要凭据才能鉴权".to_string(),
            StatusCode::BAD_REQUEST,
        );
    }

    let models: Vec<String> = match body.get("models").and_then(Value::as_array) {
        Some(arr) => arr
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .collect(),
        None => match body.get("model").and_then(Value::as_str) {
            Some(m) if !m.trim().is_empty() => vec![m.trim().to_string()],
            _ => vec!["deepseek-v4-flash".to_string()],
        },
    };

    match ai_gateway_core::modules::agent_import::import_target(&target, &base, &api_key, &models) {
        Ok(outcome) => json_ok(json!({
            "ok": true,
            "target": outcome.target,
            "backupDir": outcome.backup_dir,
            "files": outcome.files,
            "models": outcome.models,
        })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// POST /api/gateway/agents/batch-import —— 批量一键接入/更新客户端配置。
///
/// body: { "targets": ["claude-code", "codex"], "models": [...] }
async fn api_agents_batch_import(Json(body): Json<Value>) -> Response {
    let (base, api_key) = gateway_root_and_key();
    if api_key.trim().is_empty() {
        return json_err(
            "请先在网关设置里填写 API Key：客户端需要凭据才能鉴权".to_string(),
            StatusCode::BAD_REQUEST,
        );
    }

    let models: Vec<String> = match body.get("models").and_then(Value::as_array) {
        Some(arr) => arr
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .collect(),
        None => vec!["deepseek-v4-flash".to_string()],
    };

    let target_ids: Option<Vec<String>> = body.get("targets").and_then(Value::as_array).map(|arr| {
        arr.iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    });

    let res = match target_ids {
        Some(ref ids) if !ids.is_empty() => {
            ai_gateway_core::modules::agent_import::import_targets(ids, &base, &api_key, &models)
        }
        _ => ai_gateway_core::modules::agent_import::import_all_installed(&base, &api_key, &models),
    };

    match res {
        Ok(outcomes) => json_ok(json!({
            "ok": true,
            "count": outcomes.len(),
            "outcomes": outcomes.iter().map(|o| json!({
                "target": o.target,
                "backupDir": o.backup_dir,
                "files": o.files,
                "models": o.models,
            })).collect::<Vec<_>>(),
            "models": models,
        })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// POST /api/gateway/agents/restore —— 回滚到导入前的配置。
///
/// body: { "target": "codex", "backupId": "1757..." }（backupId 省略时取最近一次）
async fn api_agents_restore(Json(body): Json<Value>) -> Response {
    let target = body
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if target.is_empty() {
        return json_err("缺少 target 参数".to_string(), StatusCode::BAD_REQUEST);
    }

    let backups = ai_gateway_core::modules::agent_import::list_backups(&target);
    let id = match body
        .get("backupId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(id) => id.to_string(),
        None => match backups.first().and_then(|b| b.get("id")).and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => {
                return json_err(
                    format!("没有找到 {target} 的备份记录"),
                    StatusCode::BAD_REQUEST,
                )
            }
        },
    };

    match ai_gateway_core::modules::agent_import::restore_backup(&target, &id) {
        Ok(restored) => json_ok(json!({
            "ok": true,
            "restored": restored,
            "backupId": id,
        })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// GET /api/gateway/agents/backups?target=codex —— 列出备份。
async fn api_agents_backups(Query(params): Query<HashMap<String, String>>) -> Response {
    let target = params.get("target").cloned().unwrap_or_default();
    if target.trim().is_empty() {
        return json_err("缺少 target 参数".to_string(), StatusCode::BAD_REQUEST);
    }
    json_ok(json!({
        "backups": ai_gateway_core::modules::agent_import::list_backups(&target),
    }))
}

// ---------------------------------------------------------------------------
// 「限制使用的模型」请求体兼容
//
// 这段逻辑是本路由向后兼容的**全部依据**（新界面发数组、旧界面发单个字符串），
// 因此单独抽出来做成可单测的纯函数，而不是内联在 handler 里只能靠起 HTTP 服务验证。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod allowed_models_body_tests {
    use super::allowed_models_from_body;
    use serde_json::json;
    // 新界面（多选）的两种键名都要认。
    #[test]
    fn reads_array_from_both_key_names() {
        assert_eq!(
            allowed_models_from_body(&json!({"models": ["a", "b"]})),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            allowed_models_from_body(&json!({"allowedModels": ["a", "b"]})),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    // 旧界面（单选）的两种键名都要继续工作 —— 这是向后兼容的硬要求。
    #[test]
    fn reads_legacy_single_string() {
        assert_eq!(
            allowed_models_from_body(&json!({"model": "a"})),
            vec!["a".to_string()]
        );
        assert_eq!(
            allowed_models_from_body(&json!({"allowedModel": "a"})),
            vec!["a".to_string()]
        );
    }

    // 空值 = 清除限制（不是「限制一个叫空串的模型」）。
    #[test]
    fn empty_means_clear_restriction() {
        for body in [
            json!({}),
            json!({"models": []}),
            json!({"model": ""}),
            json!({"model": "   "}),
            json!({"models": ["", "  "]}),
        ] {
            assert!(
                allowed_models_from_body(&body).is_empty(),
                "空值应清除限制，实际输入 {body}"
            );
        }
    }

    // 数组优先于单值：两者同时出现时取数组（新界面的意图），单值多半是旧字段残留。
    #[test]
    fn array_wins_over_single_value() {
        assert_eq!(
            allowed_models_from_body(&json!({"models": ["a", "b"], "model": "z"})),
            vec!["a".to_string(), "b".to_string()],
            "同时出现时应以数组为准"
        );
    }

    // 元素逐个 trim 并丢弃空项：前端多选传来的值理论上干净，但接口是公开的。
    #[test]
    fn trims_items_and_drops_blanks() {
        assert_eq!(
            allowed_models_from_body(&json!({"models": ["  a  ", "", "b"]})),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    // 类型不对不应 panic，退化成「不限制」。
    #[test]
    fn wrong_types_degrade_without_panic() {
        assert!(allowed_models_from_body(&json!({"models": 123})).is_empty());
        assert!(allowed_models_from_body(&json!({"model": 42})).is_empty());
        assert!(allowed_models_from_body(&json!({"models": [1, 2, 3]})).is_empty());
    }
}

// ---------------------------------------------------------------------------
// POST /api/update/config 的 {config: {...}} 包装层
//
// 缺陷背景（本轮加三个开关时实测发现）：前端把配置放在 `config` 键里提交
//（与 Tauri command 的调用约定一致），而 webui 的 POST body 就是调用参数本身。
// 本路由此前直接把整个 body 交给 save_github_config —— 它读到一份**没有
// owner/repo/proxy/proxy_scope 的壳**，于是把地址写成空串、proxy_scope 回落
// 默认值。表现是「用户点保存代理，配置反而被清空」，而接口返回 200 毫无报错。
//
// 与 api_save_checkin_config / api_save_gateway_config 是同一个坑（那两处早已
// 用 `body.get("config").unwrap_or(&body)` 处理），这里补上并把契约钉住。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod update_config_body_tests {
    use super::submitted_update_config;
    use serde_json::json;

    // 带包装（前端 / Tauri 调用约定的真实形状）：必须取出内层配置。
    #[test]
    fn unwraps_config_envelope() {
        let body = json!({"config": {
            "owner": "momo0410",
            "repo": "ai-gateway",
            "proxy": "http://127.0.0.1:7897",
            "proxy_scope": {"github": true, "cn": true, "intl": false},
        }});
        let inner = submitted_update_config(&body);
        assert_eq!(
            inner.get("proxy").and_then(|v| v.as_str()),
            Some("http://127.0.0.1:7897"),
            "必须取到内层 proxy，否则地址会被写成空串（保存 = 清空）"
        );
        assert_eq!(
            inner.pointer("/proxy_scope/cn"),
            Some(&json!(true)),
            "必须取到内层 proxy_scope，否则三个开关会被静默重置成默认值"
        );
    }

    // 不带包装（裸配置 / 其它调用方）：原样使用整个 body。
    //
    // 不能无脑只看 `config` 键：那会让裸形状被读成空配置 —— 同样是把用户
    // 的设置清掉。两种形状都必须工作。
    #[test]
    fn accepts_bare_config_too() {
        let body = json!({"proxy": "http://127.0.0.1:7897", "proxy_scope": {"intl": true}});
        let inner = submitted_update_config(&body);
        assert_eq!(
            inner.get("proxy").and_then(|v| v.as_str()),
            Some("http://127.0.0.1:7897")
        );
        assert_eq!(inner.pointer("/proxy_scope/intl"), Some(&json!(true)));
    }

    // `config` 键存在但为 null：回落整个 body（而不是把 null 当配置）。
    #[test]
    fn null_config_falls_back_to_body() {
        let body = json!({"config": null, "proxy": "http://127.0.0.1:1"});
        let inner = submitted_update_config(&body);
        assert_eq!(
            inner.get("proxy").and_then(|v| v.as_str()),
            Some("http://127.0.0.1:1"),
            "config=null 时应回落整个 body（此时 body 自己就是配置）"
        );
    }
}

/// `POST /api/gateway/config` 的 camelCase → snake_case 归一化。
///
/// 回归背景（本轮实测发现，且**真实存在过**）：
/// ALIASES 表里漏了 `apiKey` / `autoStart` / `promptMode` / `promptFile`
/// 四个键，而前端 `saveGatewayConfig` 一律发 camelCase。后果是 WebUI 模式下
/// 保存 API Key **看似成功（HTTP 200）**，但配置文件里写进去的是一个没人读的
/// `"apiKey"` 键，`api_key` 仍是空串 —— 重启网关后鉴权丢失，所有客户端 401。
/// 桌面版（Tauri 按 `rename_all = "camelCase"` 取值）不受影响，因此这个缺陷
/// 只在 WebUI 模式下出现，更难被发现。
///
/// ui 用例此前也没能拦住它：`uitest/mock-host-api.cjs` 自己实现了一套别名映射
/// 且**包含了**这几个键，于是 mock 的"正确行为"把真实后端的缺陷整个盖住了。
#[cfg(test)]
mod gateway_config_body_tests {
    use super::normalize_gateway_config_body;
    use serde_json::json;

    // 本轮的核心：API Key 必须落到 `api_key`，且 camelCase 键要被删掉。
    #[test]
    fn maps_api_key_and_auto_start() {
        let out = normalize_gateway_config_body(&json!({"apiKey": "sk-x", "autoStart": true}));
        assert_eq!(
            out.get("api_key").and_then(|v| v.as_str()),
            Some("sk-x"),
            "apiKey 必须归一化成 api_key，实际: {out}"
        );
        assert_eq!(out.get("auto_start"), Some(&json!(true)));
        assert!(
            out.get("apiKey").is_none(),
            "camelCase 键必须被删掉，否则会被原样写进配置文件: {out}"
        );
        assert!(out.get("autoStart").is_none());
    }

    // 「接口配置自动保存」发的就是这两个键的最小补丁：只传端口时不能顺带
    // 塞进 api_key（那会把用户已有的 key 清成空串）。
    #[test]
    fn partial_patch_stays_partial() {
        let out = normalize_gateway_config_body(&json!({ "port": 7868 }));
        assert_eq!(out.get("port"), Some(&json!(7868)));
        assert!(
            out.get("api_key").is_none(),
            "只改端口时不得凭空出现 api_key: {out}"
        );
    }

    // 自定义提示词同样是 WebUI 下曾经丢失的字段。
    #[test]
    fn maps_prompt_fields() {
        let out = normalize_gateway_config_body(
            &json!({"promptMode": "custom", "promptFile": "C:/p.txt"}),
        );
        assert_eq!(
            out.get("prompt_mode").and_then(|v| v.as_str()),
            Some("custom")
        );
        assert_eq!(
            out.get("prompt_file").and_then(|v| v.as_str()),
            Some("C:/p.txt")
        );
        assert!(out.get("promptMode").is_none());
    }

    // snake_case 入参（旧前端 / 脚本 / curl）必须原样透传，不能被这次归一化改坏。
    #[test]
    fn snake_case_passes_through() {
        let out = normalize_gateway_config_body(&json!({"api_key": "sk-y", "port": 7863}));
        assert_eq!(out.get("api_key").and_then(|v| v.as_str()), Some("sk-y"));
        assert_eq!(out.get("port"), Some(&json!(7863)));
    }

    // camelCase 与 snake_case 同时出现时，以 **camelCase（新前端）** 为准 ——
    // 与「显式传入的界面意图优先于历史残留字段」一致。
    #[test]
    fn camel_case_wins_over_legacy_snake() {
        let out = normalize_gateway_config_body(&json!({"apiKey": "new", "api_key": "old"}));
        assert_eq!(out.get("api_key").and_then(|v| v.as_str()), Some("new"));
    }

    // 非对象入参不能让服务端 panic（真实请求可能是任意 JSON）。
    #[test]
    fn non_object_body_is_safe() {
        for body in [json!(null), json!([]), json!("x"), json!(42)] {
            let out = normalize_gateway_config_body(&body);
            assert_eq!(out, body, "非对象应原样返回");
        }
    }
}
