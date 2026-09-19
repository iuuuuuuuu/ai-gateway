import { invoke } from "@tauri-apps/api/core";
import type {
  AccountMeta,
  AccountRecord,
  AppStatus,
  AutoRotateConfig,
  CodeBuddyCliInstallResult,
  CodeBuddyCliStatus,
  CodeBuddyCliSwitchResult,
  CodeBuddyCnIdeStatus,
  CodeBuddyCnIdeSwitchResult,
  CheckinConfig,
  CheckinLog,
  CheckinResult,
  CreditExpiry,
  CreditStatistics,
  TokenStatistics,
  AccountRegionKey,
  CopyResult,
  AgentBackupItem,
  AgentBatchImportResult,
  AgentDetectionResult,
  AgentImportResult,
  AgentRestoreResult,
  GatewayConfig,
  GatewayConfigResult,
  GatewayMode,
  GatewayModeSwitchResult,
  GatewayModelItem,
  GatewayStartResult,
  GatewayStatus,
  GatewayPortCheck,
  GatewayPortHolderResult,
  GatewaySyncResult,
  GatewayTaskName,
  GrowthTaskResult,
  GatewayTaskRunResult,
  GatewayUsageResult,
  GithubConfig,
  ImportPreviewAccount,
  ImportResult,
  LocalImportResult,
  LocalScanResult,
  OAuthPollResult,
  OAuthStartResult,
  RotateLog,
  RotateStatus,
  Session,
  SwitchResult,
  TravelConfig,
  TravelStatus,
  UpdateInfo,
} from "./types";
import { DEMO_UNAVAILABLE_MESSAGE, demoModeEnabled } from "./demo-mode";
import { screenshotDemoResponse } from "./screenshot-demo";

/**
 * 双通道适配层：
 * - 桌面 App（Tauri）：`invoke` 调用 Rust commands
 * - webui（浏览器）：HTTP fetch 调用本地 ai-gateway 服务（127.0.0.1）
 */
const API_BASE = "http://127.0.0.1:57890";

const DEMO_READ_COMMANDS = new Set([
  "get_status", "get_accounts", "get_codebuddy_cli_status", "get_codebuddy_cn_ide_status", "get_checkin_status",
  "get_credit_expiry", "get_credit_statistics", "get_auto_checkin_config",
  "get_token_statistics",
  "get_checkin_logs", "get_auto_rotate_config", "rotate_status", "get_rotate_logs",
  "get_github_config", "check_update", "get_launch_at_login_enabled", "switch_progress",
  "get_travel_status", "get_auto_travel_config",
  "get_gateway_usage",
]);

export function isDemoMode(): boolean {
  return demoModeEnabled;
}

export function isWebui(): boolean {
  return typeof window !== "undefined" && !("__TAURI_INTERNALS__" in window);
}

/** Tauri mobile 也注入内部 API；用现有平台 UA 约定把桌面宿主与移动宿主区分开。 */
function isMobilePlatform(): boolean {
  if (typeof navigator === "undefined") return false;
  const ua = navigator.userAgent;
  return (
    /Android|iPhone|iPad|iPod/i.test(ua) ||
    (ua.includes("Macintosh") && navigator.maxTouchPoints > 1)
  );
}

/** 是否为提供桌面专属能力的 Tauri 宿主。 */
export function isDesktop(): boolean {
  return !isWebui() && !isMobilePlatform();
}

type Route = { method: "GET" | "POST"; path: string };

/** Tauri command → HTTP 路由映射（webui 模式）。 */
const ROUTES: Record<string, Route> = {
  get_status: { method: "GET", path: "/api/status" },
  get_accounts: { method: "GET", path: "/api/accounts" },
  get_codebuddy_cli_status: { method: "GET", path: "/api/codebuddy-cli/status" },
  install_codebuddy_cli_helper: { method: "POST", path: "/api/codebuddy-cli/install-helper" },
  switch_codebuddy_cli_account: { method: "POST", path: "/api/codebuddy-cli/switch" },
  get_codebuddy_cn_ide_status: { method: "GET", path: "/api/codebuddy-cn-ide/status" },
  switch_codebuddy_cn_ide_account: { method: "POST", path: "/api/codebuddy-cn-ide/switch" },
  detect_codebuddy_cn_ide_account: { method: "POST", path: "/api/codebuddy-cn-ide/detect" },
  delete_account: { method: "POST", path: "/api/delete" },
  set_account_note: { method: "POST", path: "/api/accounts/note" },
  set_account_disabled: { method: "POST", path: "/api/accounts/disabled" },
  oauth_start: { method: "POST", path: "/api/oauth/start" },
  oauth_status: { method: "POST", path: "/api/oauth/status" },
  import_local: { method: "POST", path: "/api/import-local" },
  scan_local_accounts: { method: "GET", path: "/api/import-local/scan" },
  import_local_selected: { method: "POST", path: "/api/import-local/selected" },
  export_accounts: { method: "POST", path: "/api/export-accounts" },
  export_accounts_to_path: { method: "POST", path: "/api/export-accounts-to-path" },
  preview_import_accounts: { method: "POST", path: "/api/import/preview" },
  import_accounts: { method: "POST", path: "/api/import" },
  switch_account: { method: "POST", path: "/api/switch" },
  list_sessions: { method: "GET", path: "/api/sessions" },
  copy_sessions: { method: "POST", path: "/api/sessions/copy" },
  get_checkin_status: { method: "GET", path: "/api/checkin/status" },
  get_credit_expiry: { method: "POST", path: "/api/credits" },
  get_credit_statistics: { method: "GET", path: "/api/credits/stats" },
  get_token_statistics: { method: "GET", path: "/api/token-stats" },
  checkin: { method: "POST", path: "/api/checkin" },
  checkin_all: { method: "POST", path: "/api/checkin/all" },
  get_auto_checkin_config: { method: "GET", path: "/api/checkin/config" },
  save_auto_checkin_config: { method: "POST", path: "/api/checkin/config" },
  get_checkin_logs: { method: "GET", path: "/api/checkin/logs" },
  // 记录保留设置：Tauri 命令与 HTTP 路由同名映射，webui 模式下同样可用
  get_record_retention: { method: "GET", path: "/api/settings/retention" },
  save_record_retention: { method: "POST", path: "/api/settings/retention" },
  get_account_records: { method: "POST", path: "/api/account-records" },
  backfill_account_records: { method: "POST", path: "/api/account-records/backfill" },
  get_travel_status: { method: "GET", path: "/api/travel/status" },
  travel_run: { method: "POST", path: "/api/travel/run" },
  travel_adopt: { method: "POST", path: "/api/travel/adopt" },
  get_auto_travel_config: { method: "GET", path: "/api/travel/config" },
  save_auto_travel_config: { method: "POST", path: "/api/travel/config" },
  get_auto_rotate_config: { method: "GET", path: "/api/rotate/config" },
  save_auto_rotate_config: { method: "POST", path: "/api/rotate/config" },
  rotate_status: { method: "GET", path: "/api/rotate/status" },
  run_rotate: { method: "POST", path: "/api/rotate/run" },
  get_rotate_logs: { method: "GET", path: "/api/rotate/logs" },
  refresh_account_token: { method: "POST", path: "/api/refresh-token" },
  get_github_config: { method: "GET", path: "/api/update/config" },
  save_github_config: { method: "POST", path: "/api/update/config" },
  check_update: { method: "GET", path: "/api/update/check" },
  switch_progress: { method: "GET", path: "/api/switch/progress" },
  // ---- 网关（workbuddy2api）集成 ----
  get_gateway_status: { method: "GET", path: "/api/gateway/status" },
  get_gateway_config: { method: "GET", path: "/api/gateway/config" },
  save_gateway_config: { method: "POST", path: "/api/gateway/config" },
  switch_gateway_mode: { method: "POST", path: "/api/gateway/mode" },
  // 「限制使用的模型」：多值（新界面）与单值（旧界面）共用同一个路由，
  // 后端按 body 里的键名区分（models/allowedModels vs model/allowedModel）。
  set_allowed_models: { method: "POST", path: "/api/gateway/allowed-model" },
  set_allowed_model: { method: "POST", path: "/api/gateway/allowed-model" },
  // 「模型 → 允许的平台」白名单（所有者的需求：
  // 「平台区分使用哪个平台的模型」）。
  //
  // ⚠ 这条**必须**注册：webui 下 `call()` 靠这张表把命令名翻成 HTTP 路径。
  // 漏注册时 `call` 会静默失败（实测：点平台开关毫无反应、不报错、
  // 也没有请求发出 —— 排查了很久）。而 Tauri 桌面端走 IPC 不经此表，
  // 所以只在 webui/测试里暴露。
  set_model_platforms: { method: "POST", path: "/api/gateway/model-platforms" },
  start_gateway: { method: "POST", path: "/api/gateway/start" },
  check_gateway_port: { method: "POST", path: "/api/gateway/port-check" },
  kill_gateway_port_holder: { method: "POST", path: "/api/gateway/port-kill" },
  get_gateway_port_holder: { method: "POST", path: "/api/gateway/port-holder" },
  stop_gateway: { method: "POST", path: "/api/gateway/stop" },
  restart_gateway: { method: "POST", path: "/api/gateway/restart" },
  sync_gateway_accounts: { method: "POST", path: "/api/gateway/sync" },
  get_gateway_models: { method: "GET", path: "/api/gateway/models" },
  get_gateway_usage: { method: "GET", path: "/api/gateway/usage" },
  run_gateway_task: { method: "POST", path: "/api/gateway/task-run" },
  run_growth_task: { method: "POST", path: "/api/gateway/growth-task" },
  // ---- 一键导入：接入本机 AI 客户端 ----
  detect_agent_clients: { method: "GET", path: "/api/gateway/agents" },
  import_agent_client: { method: "POST", path: "/api/gateway/agents/import" },
  batch_import_agent_clients: { method: "POST", path: "/api/gateway/agents/batch-import" },
  restore_agent_client: { method: "POST", path: "/api/gateway/agents/restore" },
  list_agent_backups: { method: "GET", path: "/api/gateway/agents/backups" },
  // ---- Trae / 豆包 多应用支持 ----
  app_env_check: { method: "GET", path: "/api/apps/env" },
  app_set_manual_path: { method: "POST", path: "/api/apps/manual-path" },
  switch_action: { method: "POST", path: "/api/apps/switch" },
  current_account: { method: "GET", path: "/api/apps/current" },
  list_snapshots: { method: "GET", path: "/api/apps/snapshots" },
  delete_snapshot: { method: "POST", path: "/api/apps/snapshots/delete" },
  trae_list_accounts: { method: "GET", path: "/api/trae/accounts" },
  trae_add_account: { method: "POST", path: "/api/trae/accounts/add" },
  trae_delete_account: { method: "POST", path: "/api/trae/accounts/delete" },
  trae_discover_accounts: { method: "GET", path: "/api/trae/discover" },
  trae_entitlement: { method: "GET", path: "/api/trae/entitlement" },
  trae_device_info: { method: "GET", path: "/api/trae/device" },
  trae_checkin_run: { method: "POST", path: "/api/trae/checkin" },
  trae_credits_history: { method: "GET", path: "/api/trae/credits/history" },
  trae_clear_cooldown: { method: "POST", path: "/api/trae/cooldown/clear" },
  doubao_list_accounts: { method: "GET", path: "/api/doubao/accounts" },
  doubao_save_account: { method: "POST", path: "/api/doubao/accounts/save" },
  doubao_delete_account: { method: "POST", path: "/api/doubao/accounts/delete" },
  doubao_get_credential: { method: "GET", path: "/api/doubao/credential" },
  doubao_set_credential: { method: "POST", path: "/api/doubao/credential" },
  doubao_captured_credential: { method: "GET", path: "/api/doubao/credential/captured" },
  doubao_credential_auto_apply: { method: "POST", path: "/api/doubao/credential/apply" },
  doubao_keepalive: { method: "POST", path: "/api/doubao/keepalive" },
  doubao_renew: { method: "POST", path: "/api/doubao/renew" },
  doubao_diagnose: { method: "GET", path: "/api/doubao/diagnose" },
  doubao_fetch_quota: { method: "GET", path: "/api/doubao/quota" },
  doubao_quota_batch: { method: "POST", path: "/api/doubao/quota/batch" },
  doubao_probe_account: { method: "GET", path: "/api/doubao/probe" },
  doubao_backup_chatdata: { method: "POST", path: "/api/doubao/chatdata/backup" },
  doubao_restore_chatdata: { method: "POST", path: "/api/doubao/chatdata/restore" },
  doubao_chatdata_info: { method: "GET", path: "/api/doubao/chatdata/info" },
  doubao_export_chats: { method: "POST", path: "/api/doubao/chats/export" },
  get_app_settings: { method: "GET", path: "/api/apps/settings" },
  // ---- 本地 MITM 代理 ----
  proxy_config: { method: "GET", path: "/api/proxy/config" },
  proxy_status: { method: "GET", path: "/api/proxy/status" },
  proxy_start: { method: "POST", path: "/api/proxy/start" },
  proxy_stop: { method: "POST", path: "/api/proxy/stop" },
  proxy_cert_status: { method: "GET", path: "/api/proxy/cert" },
  proxy_cert_generate: { method: "POST", path: "/api/proxy/cert/generate" },
  proxy_capture_local: { method: "POST", path: "/api/proxy/capture-local" },
  proxy_cleanup_stale: { method: "POST", path: "/api/proxy/cleanup" },
  proxy_parse_upstream: { method: "POST", path: "/api/proxy/parse-upstream" },
  task_status: { method: "GET", path: "/api/tasks/status" },
  task_register: { method: "POST", path: "/api/tasks/register" },
  task_unregister: { method: "POST", path: "/api/tasks/unregister" },
  task_run_now: { method: "POST", path: "/api/tasks/run" },
  save_app_settings: { method: "POST", path: "/api/apps/settings" },
  // ---- Qoder（QoderWork）----
  // 凭证与 COSY 签名在 Go 侧 internal/qoder；这一层只编排账号元信息与登录。
  qoder_list_accounts: { method: "GET", path: "/api/qoder/accounts" },
  qoder_save_account: { method: "POST", path: "/api/qoder/accounts/save" },
  qoder_delete_account: { method: "POST", path: "/api/qoder/accounts/delete" },
  qoder_summary: { method: "GET", path: "/api/qoder/summary" },
  qoder_login_start: { method: "POST", path: "/api/qoder/login/start" },
  qoder_login_poll: { method: "POST", path: "/api/qoder/login/poll" },
  qoder_import_credentials: { method: "POST", path: "/api/qoder/import" },
  qoder_import_from_client: { method: "POST", path: "/api/qoder/import-from-client" },
  // ---- ZCode（Z.AI / 智谱）----
  // 与 Qoder 的差异：凭证是用户可复制的字符串，故导入是主路径。
  zcode_list_accounts: { method: "GET", path: "/api/zcode/accounts" },
  zcode_save_account: { method: "POST", path: "/api/zcode/accounts/save" },
  zcode_delete_account: { method: "POST", path: "/api/zcode/accounts/delete" },
  zcode_summary: { method: "GET", path: "/api/zcode/summary" },
  zcode_import_credential: { method: "POST", path: "/api/zcode/import-credential" },
  zcode_import_from_dir: { method: "POST", path: "/api/zcode/import-dir" },
  zcode_scan_local: { method: "POST", path: "/api/zcode/scan-local" },
  zcode_import_scanned: { method: "POST", path: "/api/zcode/import-scanned" },
  zcode_refresh_account: { method: "POST", path: "/api/zcode/refresh-account" },
  qoder_refresh_account: { method: "POST", path: "/api/qoder/refresh-account" },
  qoder_campaigns: { method: "POST", path: "/api/qoder/campaigns" },
  qoder_claim_campaign: { method: "POST", path: "/api/qoder/claim-campaign" },
  zcode_login_start: { method: "POST", path: "/api/zcode/login/start" },
  zcode_login_poll: { method: "POST", path: "/api/zcode/login/poll" },
};

function queryString(args?: Record<string, unknown>): string {
  if (!args) return "";
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(args)) {
    if (value === undefined || value === null) continue;
    params.set(key, String(value));
  }
  const text = params.toString();
  return text ? `?${text}` : "";
}

async function httpCall<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const route = ROUTES[cmd];
  if (!route) throw new Error(`webui 模式暂不支持该操作: ${cmd}`);
  let res: Response;
  try {
    const url =
      route.method === "GET"
        ? `${API_BASE}${route.path}${queryString(args)}`
        : `${API_BASE}${route.path}`;
    res = await fetch(url, {
      method: route.method,
      headers: { "Content-Type": "application/json" },
      body: route.method === "POST" ? JSON.stringify(args ?? {}) : undefined,
    });
  } catch {
    throw new Error(`无法连接 ai-gateway 服务（${API_BASE}），请先运行 \`ai-gateway\``);
  }
  const data = await res.json().catch(() => ({}));
  if (!res.ok) {
    throw new Error(data.message || data.error || `请求失败 (${res.status})`);
  }
  return data as T;
}

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (demoModeEnabled) {
    if (cmd === "get_credit_statistics" && args?.refresh === true) {
      throw new Error(DEMO_UNAVAILABLE_MESSAGE);
    }
    if (!DEMO_READ_COMMANDS.has(cmd)) throw new Error(DEMO_UNAVAILABLE_MESSAGE);
    return screenshotDemoResponse(cmd, args) as T;
  }
  if (!isWebui()) return invoke<T>(cmd, args);
  return httpCall<T>(cmd, args);
}

// ---------------------------------------------------------------------------
// 状态 / 账号
// ---------------------------------------------------------------------------

export function getStatus(): Promise<AppStatus> {
  return call("get_status");
}

export function getAccounts(): Promise<{ accounts: AccountMeta[] }> {
  return call("get_accounts");
}

export function getCodebuddyCliStatus(): Promise<CodeBuddyCliStatus> {
  return call("get_codebuddy_cli_status");
}

export function installCodebuddyCliHelper(): Promise<CodeBuddyCliInstallResult> {
  return call("install_codebuddy_cli_helper");
}

export function switchCodebuddyCliAccount(accountId: string): Promise<CodeBuddyCliSwitchResult> {
  if (demoModeEnabled) {
    return new Promise((resolve, reject) => {
      window.setTimeout(() => {
        try {
          resolve(screenshotDemoResponse("switch_codebuddy_cli_account", { accountId }) as CodeBuddyCliSwitchResult);
        } catch (error) {
          reject(error);
        }
      }, 1200);
    });
  }
  return call("switch_codebuddy_cli_account", { accountId });
}

export function getCodebuddyCnIdeStatus(): Promise<CodeBuddyCnIdeStatus> {
  return call("get_codebuddy_cn_ide_status");
}

export function switchCodebuddyCnIdeAccount(
  accountId: string,
  restart = true,
): Promise<CodeBuddyCnIdeSwitchResult> {
  return call("switch_codebuddy_cn_ide_account", { accountId, restart });
}

export function detectCodebuddyCnIdeAccount(): Promise<{
  ok: boolean;
  found: boolean;
  matched?: boolean;
  accountId?: string;
  message?: string;
}> {
  return call("detect_codebuddy_cn_ide_account");
}


export function deleteAccount(accountId: string): Promise<{ ok: boolean }> {
  return call("delete_account", { accountId });
}

/**
 * 设置账号备注（空串 = 清除）。
 *
 * 备注只存本地账号库，不参与登录；用于认出「这是谁的号、干什么用的」。
 * 返回更新后的 AccountMeta，调用方可直接用它刷新界面。
 */
export function setAccountNote(
  accountId: string,
  note: string,
): Promise<{ ok: boolean; account: AccountMeta }> {
  return call("set_account_note", { accountId, note });
}

/**
 * 发起 OAuth 登录（国服扫码 / 国际版三方授权）。
 *
 * `region` 决定取 state 的域名与平台标识（国服 `workbuddy` / 国际版
 * `workbuddy-ai`）；缺省国服，与旧调用兼容。
 */
export function oauthStart(region: AccountRegionKey = "cn"): Promise<OAuthStartResult> {
  return call("oauth_start", { region });
}

export function oauthStatus(loginId: string): Promise<OAuthPollResult> {
  return call("oauth_status", { loginId });
}

/**
 * 从本机导入账号。
 *
 * 会同时探测国服与国际版两个认证文件，把能读到的账号全部并入账号库；
 * `account` 为首个账号（兼容旧调用方），`accounts` 为本次全部结果。
 */
export function importLocal(): Promise<{
  ok: boolean;
  account: AccountMeta | null;
  accounts: AccountMeta[];
  imported: number;
}> {
  return call("import_local");
}

/**
 * 扫描本机全部历史登录态（当前认证文件 + 客户端快照 + 本工具备份）。
 *
 * 「导入本机账号」原本只看两个固定认证文件，因此每区域最多 1 个账号；
 * 本接口额外扫出历史快照，按「区域 + uid」去重后只保留凭证最新的一份。
 */
export function scanLocalAccounts(): Promise<LocalScanResult> {
  return call("scan_local_accounts", {});
}

/** 按来源文件路径批量导入本机账号（路径跨扫描稳定，优于索引）。 */
export function importLocalSelected(paths: string[]): Promise<LocalImportResult> {
  return call("import_local_selected", { paths, indexes: [] });
}

export function exportAccounts(accountIds: string[]): Promise<{ ok: boolean; accounts: AccountRecord[] }> {
  return call("export_accounts", { accountIds });
}

/** 桌面端：把完整记录写入用户选择的路径（系统保存对话框产物）。 */
export function exportAccountsToPath(
  accountIds: string[],
  path: string,
): Promise<{ ok: boolean; path: string }> {
  return call("export_accounts_to_path", { accountIds, path });
}

export function previewImportAccounts(
  fileText: string,
): Promise<{ accounts: ImportPreviewAccount[]; total: number }> {
  return call("preview_import_accounts", { fileText });
}

export function importAccounts(fileText: string, indexes: number[]): Promise<ImportResult> {
  return call("import_accounts", { fileText, indexes });
}

export function switchAccount(args: {
  accountId: string;
  restart?: boolean;
  shareSessions?: boolean;
  copySessionIds?: string[];
}): Promise<SwitchResult> {
  return call("switch_account", args as unknown as Record<string, unknown>);
}

/** 切换进度（webui 轮询用；桌面端走事件，此函数无副作用）。 */
export function switchProgress(): Promise<{ running: boolean; progress: string | null }> {
  return call("switch_progress");
}

export function listSessions(): Promise<{
  sessions: Session[];
  current: string | null;
}> {
  return call("list_sessions");
}

export function copySessions(
  targetAccountId: string,
  sessionIds: string[],
): Promise<{ sourceUid: string; targetUid: string; copied: CopyResult[] }> {
  return call("copy_sessions", { targetAccountId, sessionIds });
}

/** 打开系统设置授权面板（桌面端专用；webui 模式由服务进程权限决定，无操作）。 */
export function openPermissionSettings(
  target?: "app_management" | "all_files",
): Promise<void> {
  if (demoModeEnabled) return Promise.reject(new Error(DEMO_UNAVAILABLE_MESSAGE));
  if (isWebui()) return Promise.resolve();
  return call("open_permission_settings", { target: target ?? "app_management" });
}

/** 权限自检：桌面端写探针；webui 模式由服务进程权限决定。 */
export function checkAuthPermission(): Promise<{
  ok: boolean;
  message?: string;
  error?: string;
  dir?: string;
  hint?: string;
}> {
  if (demoModeEnabled) return Promise.reject(new Error(DEMO_UNAVAILABLE_MESSAGE));
  if (isWebui()) {
    return Promise.resolve({
      ok: true,
      message: "webui 模式由服务进程（终端启动）的权限决定，无需额外授权",
      hint: "",
    });
  }
  return call("check_auth_permission");
}

/** 在 Finder 中显示当前 App（桌面端专用；webui 无操作）。 */
export function revealAppInFinder(): Promise<void> {
  if (demoModeEnabled) return Promise.reject(new Error(DEMO_UNAVAILABLE_MESSAGE));
  if (isWebui()) return Promise.resolve();
  return call("reveal_app_in_finder");
}

// ---------------------------------------------------------------------------
// 阶段 3：签到 + token 刷新
// ---------------------------------------------------------------------------

export async function getCheckinStatus(accountId: string): Promise<{
  ok: boolean;
  todayCheckedIn: boolean;
  error?: string;
  raw?: unknown;
}> {
  if (demoModeEnabled) {
    return screenshotDemoResponse("get_checkin_status", { accountId }) as {
      ok: boolean;
      todayCheckedIn: boolean;
      error?: string;
      raw?: unknown;
    };
  }
  if (isWebui()) {
    // webui 端为批量接口，按 accountId 过滤
    const all = await httpCall<{
      accounts: {
        accountId: string;
        email: string;
        ok: boolean;
        todayCheckedIn: boolean;
        error?: string;
        raw?: unknown;
      }[];
    }>("get_checkin_status");
    const one = all.accounts.find((a) => a.accountId === accountId);
    return one
      ? { ok: one.ok, todayCheckedIn: one.todayCheckedIn, error: one.error, raw: one.raw }
      : { ok: false, todayCheckedIn: false, error: "未找到账号" };
  }
  return call("get_checkin_status", { accountId });
}

export function getCreditExpiry(accountId: string): Promise<CreditExpiry> {
  return call("get_credit_expiry", { accountId });
}

export function getCreditStatistics(refresh = false): Promise<CreditStatistics> {
  return call("get_credit_statistics", refresh ? { refresh: true } : undefined);
}

export function getTokenStatistics(days?: number): Promise<TokenStatistics> { return call("get_token_statistics", days ? { days } : undefined); }

export function checkin(accountId: string): Promise<CheckinResult> {
  return call("checkin", { accountId });
}

export function checkinAll(): Promise<{
  accounts: { accountId: string; email: string; result: string; error?: string }[];
  status?: string;
  reason?: string;
}> {
  return call("checkin_all");
}

/** 一键旅行：全部账号走一趟巡检（含领养、派出、领奖）。不受「自动旅行」开关限制。 */
export function travelRun(): Promise<{
  status: string;
  reason?: string;
  completed?: boolean;
  accounts?: { accountId: string; email: string; result: string; skip?: string | null; message?: string | null }[];
}> {
  return call("travel_run");
}

/** 单账号领养：只领养 Buddy，不派猫、不领奖。 */
export function travelAdopt(accountId: string): Promise<{
  ok: boolean;
  skip?: string;
  message?: string;
}> {
  return call("travel_adopt", { accountId });
}

export function getAutoCheckinConfig(): Promise<CheckinConfig> {
  return call("get_auto_checkin_config");
}

export function saveAutoCheckinConfig(config: CheckinConfig): Promise<CheckinConfig> {
  return call("save_auto_checkin_config", {
    config: config as unknown as Record<string, unknown>,
  });
}

export function getCheckinLogs(): Promise<{ logs: CheckinLog[] }> {
  return call("get_checkin_logs");
}

/** 记录保留天数设置（签到日志 / 积分快照 / 任务记录共用同一口径）。 */
export interface RecordRetentionSetting {
  days: number;
  defaultDays: number;
  minDays: number;
  maxDays: number;
  presets: { days: number; label: string }[];
}

export function getRecordRetention(): Promise<RecordRetentionSetting> {
  return call("get_record_retention");
}

/** 保存保留天数；返回归一化后的实际生效值（越界值会被夹到合法区间）。 */
export function saveRecordRetention(days: number): Promise<{ days: number }> {
  return call("save_record_retention", { days });
}

// ---------------------------------------------------------------------------
// 账号记录（任务 / 积分 / Token 三类事件的统一流水）
// ---------------------------------------------------------------------------

/** 一条账号记录。 */
export interface AccountRecordItem {
  ts: number;
  accountId: string;
  accountName: string;
  /** task | credit | token */
  kind: string;
  title: string;
  /** success | failed | already | info */
  result: string;
  /** 积分增减或 Token 数量；无则为 0 */
  amount: number;
  detail: string;
  /**
   * 积分变化的来源：grant（额度发放）| consume（调用扣减）| expire（额度到期）| adjust。
   *
   * **可选**是刻意的，不是疏漏：这个字段是后加的，历史记录里根本没有它。
   * 后端对老记录不会补写（记录是只追加的事件流，不改写历史），
   * 所以运行时完全可能是 undefined —— 声明成必填只会把 undefined 藏进类型里，
   * 让渲染处忘记兜底。消费方必须自己判空。
   */
  source?: string;
}

export interface AccountRecordsResult {
  records: AccountRecordItem[];
  /** 过滤后的总条数（不受 limit 影响） */
  total: number;
  summary: {
    taskCount: number;
    creditNet: number;
    tokenSum: number;
  };
  retentionDays: number;
}

/** 查询账号记录；accountId 为空表示全部账号。 */
export function getAccountRecords(params: {
  accountId?: string;
  from?: number;
  to?: number;
  kinds?: string[];
  limit?: number;
}): Promise<AccountRecordsResult> {
  return call("get_account_records", {
    accountId: params.accountId ?? null,
    from: params.from ?? null,
    to: params.to ?? null,
    kinds: params.kinds ?? null,
    limit: params.limit ?? 500,
  });
}

/** 把历史签到日志回填为账号记录（幂等，重复调用不产生重复）。 */
export function backfillAccountRecords(): Promise<{ added: number }> {
  return call("backfill_account_records");
}

export async function getTravelStatus(accountId: string): Promise<TravelStatus> {
  if (demoModeEnabled) {
    return screenshotDemoResponse("get_travel_status", { accountId }) as TravelStatus;
  }
  if (isWebui()) {
    // webui 端为批量接口，按 accountId 过滤。
    //
    // message / skip 必须一并带出：后端 `travel_display` 会把「无 Buddy（领养需先
    // 积累对话轮次）」这类**具体原因**放在 message 里（travel.rs::display_record），
    // 而这里原先只映射 4 个字段，把原因丢掉了 —— 卡片菜单因此只能靠 label 反推，
    // 可 label 描述的是**旅行**状态、不是**领养**状态（详见 account-card 里
    // buddyKnowledge 的说明）。少透出这两个字段，界面就只能猜。
    const all = await httpCall<{
      accounts: {
        accountId: string;
        email: string;
        label: TravelStatus["label"];
        rewardCredit: number | null;
        locationName?: string | null;
        arriveAt?: number | null;
        message?: string | null;
        skip?: string | null;
      }[];
    }>("get_travel_status");
    const one = all.accounts.find((a) => a.accountId === accountId);
    return one
      ? {
          label: one.label,
          rewardCredit: one.rewardCredit,
          locationName: one.locationName ?? null,
          arriveAt: one.arriveAt ?? null,
          message: one.message ?? null,
          skip: one.skip ?? null,
        }
      : { label: "untraveled", rewardCredit: null, locationName: null, arriveAt: null };
  }
  return call("get_travel_status", { accountId });
}

export function getAutoTravelConfig(): Promise<TravelConfig> {
  return call("get_auto_travel_config");
}

export function saveAutoTravelConfig(config: TravelConfig): Promise<TravelConfig> {
  return call("save_auto_travel_config", {
    config: config as unknown as Record<string, unknown>,
  });
}

export function getAutoRotateConfig(): Promise<AutoRotateConfig> {
  return call("get_auto_rotate_config");
}

export function saveAutoRotateConfig(config: AutoRotateConfig): Promise<AutoRotateConfig> {
  return call("save_auto_rotate_config", {
    config: config as unknown as Record<string, unknown>,
  });
}

export function getRotateStatus(): Promise<RotateStatus> {
  return call("rotate_status");
}

export function runRotate(): Promise<{ status: string; reason?: string; error?: string; to?: string }> {
  return call("run_rotate");
}

export function getRotateLogs(): Promise<{ logs: RotateLog[] }> {
  return call("get_rotate_logs");
}

export function refreshAccountToken(accountId: string): Promise<AccountMeta> {
  return call("refresh_account_token", { accountId });
}

// ---------------------------------------------------------------------------
// 阶段 4：自动更新
// ---------------------------------------------------------------------------

export function getGithubConfig(): Promise<GithubConfig> {
  return call("get_github_config");
}

/**
 * 保存更新源配置（含代理地址与 proxy_scope 三个开关）。
 *
 * **两个通道的返回形状不同，必须在这里抹平**：
 *   - Tauri command 直接返回配置对象（commands.rs::save_github_config）；
 *   - webui 的 HTTP 路由返回 `{ ok: true, config: {...} }`（api.rs::api_save_update_config）。
 *
 * 不抹平的后果不是「报错」而是「静默清空」：调用方写的是 `saved.proxy`，
 * 在 webui 下读出 undefined → 输入框被清空、三个开关被拨回默认值，而用户
 * 明明刚点了保存。本函数此前没做这层适配，webui（浏览器打开宿主页面）下
 * 保存代理一直是坏的，只是没有用例覆盖到。
 */
export async function saveGithubConfig(config: GithubConfig): Promise<GithubConfig> {
  const res = await call<GithubConfig & { config?: GithubConfig }>("save_github_config", {
    config: config as unknown as Record<string, unknown>,
  });
  // 只认「带 config 包装层」这一种形态：Tauri 返回的配置对象里没有 config 键
  //（它只有 owner/repo/proxy/proxy_scope），所以这个判断不会误伤。
  return res && typeof res === "object" && res.config ? res.config : res;
}

export function checkUpdate(proxy?: string, force?: boolean): Promise<UpdateInfo> {
  return call("check_update", { proxy: proxy?.trim() || null, force: force ?? false });
}

export function relaunchApp(): Promise<void> {
  return call("relaunch_app");
}

// ---------------------------------------------------------------------------
// 开机自启（仅桌面端；webui 不提供同名接口，卡片也不在 webui 渲染）
// ---------------------------------------------------------------------------

/** 查询系统当前的开机自启注册状态（桌面端）。 */
export function getLaunchAtLoginEnabled(): Promise<boolean> {
  if (demoModeEnabled) return call("get_launch_at_login_enabled");
  if (!isDesktop()) return Promise.resolve(false);
  return call("get_launch_at_login_enabled");
}

/** 注册 / 移除系统开机自启，返回回读后的权威状态（桌面端）。 */
export function setLaunchAtLoginEnabled(enabled: boolean): Promise<boolean> {
  if (demoModeEnabled) return Promise.reject(new Error(DEMO_UNAVAILABLE_MESSAGE));
  if (!isDesktop()) return Promise.resolve(false);
  return call("set_launch_at_login_enabled", { enabled });
}

/** 把 Tauri command / HTTP 抛出的错误统一为 Error。 */
export function asError(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return JSON.stringify(e ?? "未知错误");
}

// ---------------------------------------------------------------------------
// 网关（workbuddy2api）集成
// ---------------------------------------------------------------------------

/** 读取网关运行态（含账号池详情）。 */
export function getGatewayStatus(): Promise<GatewayStatus> {
  return call<GatewayStatus>("get_gateway_status");
}

/** 读取网关配置。 */
export function getGatewayConfig(): Promise<GatewayConfigResult> {
  return call<GatewayConfigResult>("get_gateway_config");
}

/**
 * 手动触发一轮养号任务（活跃上报 / 夜猫子 / 开学季 / trial / 活跃地图）。
 *
 * 这些任务的实现在 Go 网关里，本函数只是把请求转过去。
 *
 * `accountId` 决定**作用范围**（这是一个语义分水岭，不要混用）：
 *   - 省略 → **全部账号**（右上角「一键操作」的入口）
 *   - 给出 uid → **只作用于该账号**（账号卡片菜单里的入口）
 *
 * 注意返回 `ran=false` + `skip` 是**正常结果**（如夜猫子不在 23:00–08:00
 * 窗口内、该任务是国服专属而此号是国际版），调用方应当作说明展示而非报错。
 */
export function runGatewayTask(
  task: GatewayTaskName,
  accountId?: string,
): Promise<GatewayTaskRunResult> {
  // 显式转 camelCase：Tauri 按 rename_all="camelCase" 取值，
  // 传 account_id 会被静默丢弃（于是「只跑这个账号」变成「跑全部账号」——
  // 静默的语义放大，比报错危险得多）。
  return call<GatewayTaskRunResult>("run_gateway_task", {
    task,
    accountId: accountId ?? null,
  });
}

/**
 * 成长任务「一键完成」。
 *
 * `action`：
 *   - `list`：列出该账号的成长任务（只读、秒级）
 *   - `run`：执行该账号的任务；`taskCode` 省略 = 跑全部待办
 *   - `run-all`：所有账号跑一轮
 *
 * **耗时差异极大**：`list` 秒级；`run` 单账号分钟级（可能含**真实对话**，
 * 会消耗 token 与额度）；`run-all` 更久。调用方必须给出「正在执行」反馈并
 * 禁用按钮，否则用户会以为没反应而反复点击 —— 而重复点击会重复消耗。
 */
export function runGrowthTask(
  action: "list" | "run" | "run-all",
  accountId?: string,
  taskCode?: string,
): Promise<GrowthTaskResult> {
  return call<GrowthTaskResult>("run_growth_task", { action, accountId, taskCode });
}

/**
 * 保存网关配置。
 *
 * 显式把 snake_case 字段转成 camelCase：Tauri 的 `invoke` 按
 * `#[tauri::command(rename_all = "camelCase")]` 取值，直接透传
 * `api_key` / `auto_start` 会被静默丢弃（webui 的 HTTP 版则兼容两种写法）。
 */
export function saveGatewayConfig(config: Partial<GatewayConfig>): Promise<{ config: GatewayConfig }> {
  const args: Record<string, unknown> = {};
  if (config.port !== undefined) args.port = config.port;
  if (config.api_key !== undefined) args.apiKey = config.api_key;
  if (config.auto_start !== undefined) args.autoStart = config.auto_start;
  if (config.mode !== undefined) args.mode = config.mode;
  if (config.pinned_uid !== undefined) args.pinnedUid = config.pinned_uid;
  if (config.manual_uids !== undefined) args.manualUids = config.manual_uids;
  // 养号任务排程：同样显式转 camelCase，否则 Tauri 会静默丢弃这些字段
  //（webui 的 HTTP 版兼容两种写法，桌面版只认 camelCase）。
  if (config.activity_hours !== undefined) args.activityHours = config.activity_hours;
  if (config.nightowl_hours !== undefined) args.nightowlHours = config.nightowl_hours;
  if (config.school_hours !== undefined) args.schoolHours = config.school_hours;
  if (config.trial_hours !== undefined) args.trialHours = config.trial_hours;
  if (config.activity_enabled !== undefined) args.activityEnabled = config.activity_enabled;
  if (config.nightowl_enabled !== undefined) args.nightowlEnabled = config.nightowl_enabled;
  if (config.school_enabled !== undefined) args.schoolEnabled = config.school_enabled;
  if (config.trial_enabled !== undefined) args.trialEnabled = config.trial_enabled;
  if (config.activity_report_count !== undefined) {
    args.activityReportCount = config.activity_report_count;
  }
  // 自定义系统提示词：同样显式转 camelCase，否则 Tauri 会静默丢弃
  //（表现为「配了自定义提示词却总被重置」，且看不出是传参被丢）。
  if (config.prompt_mode !== undefined) args.promptMode = config.prompt_mode;
  if (config.prompt_file !== undefined) args.promptFile = config.prompt_file;
  return call<{ config: GatewayConfig }>("save_gateway_config", args);
}

/** 启动网关（会把账号库导出为网关凭证）。传 port 可一步指定端口并保存。 */
export function startGateway(port?: number): Promise<GatewayStartResult> {
  return call<GatewayStartResult>("start_gateway", port ? { port } : {});
}

/** 检测端口是否可用；被占用时返回建议端口与占用进程。 */
export function checkGatewayPort(port: number): Promise<GatewayPortCheck> {
  return call<GatewayPortCheck>("check_gateway_port", { port });
}

/** 结束占用端口的进程，让网关接管该端口。需用户明确确认后调用。 */
export function killGatewayPortHolder(
  port: number,
): Promise<{ ok: boolean; pid: number; name: string; message: string }> {
  return call("kill_gateway_port_holder", { port });
}

/**
 * 查询占用端口的进程（供确认对话框展示）。
 *
 * 与 checkGatewayPort 的分工：后者在页面挂载/端口变化时就会调用（热路径），
 * 因此**不查进程**；本函数只在用户点开对话框时调用，那时才值得付出
 * spawn netstat/tasklist/powershell（macOS 上是 lsof）的开销。
 *
 * 返回值里的 `hint` 是「查不到占用者」时的可操作排查命令，由后端按自身
 * 所在系统生成 —— 前端直接展示即可，不要自己拼平台相关的命令。
 */
export function getGatewayPortHolder(port: number): Promise<GatewayPortHolderResult> {
  return call("get_gateway_port_holder", { port });
}

/** 停止网关。 */
export function stopGateway(): Promise<{ stopped: boolean }> {
  return call<{ stopped: boolean }>("stop_gateway");
}

/** 重启网关，使新配置/新账号生效。 */
export function restartGateway(): Promise<GatewayStartResult> {
  return call<GatewayStartResult>("restart_gateway");
}

/** 手动触发账号双向同步。 */
export function syncGatewayAccounts(autoReload = true): Promise<GatewaySyncResult> {
  return call<GatewaySyncResult>("sync_gateway_accounts", { autoReload });
}

/**
 * 切换网关工作模式并立即生效。
 *
 * 与 saveGatewayConfig 的区别：那个只写配置文件，而网关账号池是启动时建立的，
 * 因此改完必须手动重启才生效。此接口把「保存 + 重导出凭证 + 按需重启」合成一步。
 *
 * @param manualUids 手动模式下勾选的账号列表（可多选）。
 */
export function switchGatewayMode(
  mode: GatewayMode,
  manualUids?: string[] | null,
): Promise<GatewayModeSwitchResult> {
  return call<GatewayModeSwitchResult>("switch_gateway_mode", {
    mode,
    manualUids: manualUids ?? [],
  });
}

/**
 * 设置账号的禁用状态。
 *
 * 语义：禁用 = 不进网关账号池；签到 / 旅行 / 上报等养号任务照跑。
 * 后端会顺带重导出凭证并按需重启网关，做到「点了就生效」。
 */
export function setAccountDisabled(
  accountId: string,
  disabled: boolean,
): Promise<{ ok: boolean; account: AccountMeta; sync?: unknown }> {
  return call("set_account_disabled", { accountId, disabled });
}

/**
 * 设置「限制使用的模型」白名单（多选）；传空数组 = 清除限制（全部放行）。
 *
 * **三个工作模式都生效**：网关会拒绝名单外的模型（400 model_not_allowed）。
 * 网关运行时后端会自动重启它以生效（模型限制由网关启动时读取）。
 *
 * 为什么走 `set_allowed_models` 而不是旧的单值命令：旧命令只表达一个模型，
 * 传数组时后端会解析失败。旧命令仍保留（向后兼容已发布的调用方）。
 */
export function setAllowedModels(models: string[]): Promise<GatewayModeSwitchResult> {
  return call<GatewayModeSwitchResult>("set_allowed_models", { models });
}

/**
 * 设置「模型 → 允许的平台」白名单（所有者的需求：
 * 「平台区分使用哪个平台的模型」）。
 *
 * ## 语义
 *
 *	{"glm-5.2":["zcode"]}          → glm-5.2 只走 ZCode
 *	{"glm-5.2":["zcode","qoder"]}  → 两个都可以
 *	{}                             → 不限制（恢复默认）
 *
 * 某模型**不在**这个 map 里 = 不限制它。
 *
 * ## 为什么空数组要被过滤掉
 *
 * 界面上"全部熄灭"与"从没配置过"是两种操作，但落到网关都该是**不限制**
 * —— 否则用户取消所有勾选会让该模型彻底不可用，那是个陷阱。
 * 后端会过滤空数组（见 `model_platforms_for_gateway`）。
 */
export function setModelPlatforms(
  platforms: Record<string, string[]>,
): Promise<GatewayModeSwitchResult> {
  return call<GatewayModeSwitchResult>("set_model_platforms", { platforms });
}

/**
 * 设置单个模型限制；传空串清除限制。
 *
 * @deprecated 多选请用 {@link setAllowedModels}。保留这个入口只为向后兼容
 *（脚本、旧版界面自调用）。它现在等价于传一个单元素数组。
 */
export function setAllowedModel(model: string): Promise<GatewayModeSwitchResult> {
  return call<GatewayModeSwitchResult>("set_allowed_model", { model });
}

// ---------------------------------------------------------------------------
// 一键导入：接入本机 AI 客户端
// ---------------------------------------------------------------------------

/** 获取网关可用模型列表（优先动态查询网关，网关未就绪时回退静态并集）。 */
export async function getGatewayModels(): Promise<GatewayModelItem[]> {
  const res = await call<{ models?: GatewayModelItem[] }>("get_gateway_models");
  return res.models ?? [];
}

/**
 * 获取网关累计 Token 用量统计（网关自统计，重启保留）。
 *
 * `days` 省略或非正数 = 全部历史；网关未运行 / 不可达时也不抛错，
 * 由返回的 running / reachable / error 字段区分状态。
 */
export function getGatewayUsage(days?: number): Promise<GatewayUsageResult> {
  return call<GatewayUsageResult>("get_gateway_usage", days && days > 0 ? { days } : undefined);
}

/** 探测本机 AI 客户端（全部 12 类智能体）的安装与配置状态。 */
export function detectAgentClients(): Promise<AgentDetectionResult> {
  return call<AgentDetectionResult>("detect_agent_clients");
}

/** 将本网关配置一键接入指定的客户端（支持单模型或多选模型）。 */
export function importAgentClient(
  target: string,
  models?: string[] | string,
): Promise<AgentImportResult> {
  const modelList = Array.isArray(models)
    ? models
    : typeof models === "string" && models.trim()
      ? [models.trim()]
      : undefined;
  return call<AgentImportResult>("import_agent_client", {
    target,
    models: modelList,
    model: modelList?.[0],
  });
}

/** 批量一键接入/更新多个客户端。若不传 targets，则自动更新所有已检测到安装的客户端。 */
export function batchImportAgentClients(
  targets?: string[],
  models?: string[],
): Promise<AgentBatchImportResult> {
  return call<AgentBatchImportResult>("batch_import_agent_clients", {
    targets,
    models,
  });
}

/** 回滚指定客户端至导入前的配置备份。 */
export function restoreAgentClient(
  target: string,
  backupId?: string,
): Promise<AgentRestoreResult> {
  return call<AgentRestoreResult>("restore_agent_client", { target, backupId });
}

/** 查询指定客户端的历史配置备份列表。 */
export function listAgentBackups(
  target: string,
): Promise<{ backups: AgentBackupItem[] }> {
  return call<{ backups: AgentBackupItem[] }>("list_agent_backups", { target });
}


// ---------------------------------------------------------------------------
// Trae / 豆包 多应用支持
// ---------------------------------------------------------------------------

/** 应用安装与运行状态。 */
export interface AppEnvStatus {
  targetApp: string;
  appName: string;
  layout: "icube" | "chromium" | "authfile";
  installed: boolean;
  exePath: string | null;
  dataDir: string;
  dataDirExists: boolean;
  profilesDir: string;
  snapshotCount: number;
  manualPath: string | null;
  settingsPathKey: string;
  running: boolean;
}

/** 探测某个应用的安装、数据目录与快照状态。 */
export function appEnvCheck(targetApp: string): Promise<AppEnvStatus> {
  return call<AppEnvStatus>("app_env_check", { targetApp });
}

/** 保存应用的手动 exe 路径（空串 = 清除）。 */
export function appSetManualPath(targetApp: string, path: string): Promise<{ ok: boolean }> {
  return call<{ ok: boolean }>("app_set_manual_path", { targetApp, path });
}

/** 登录态动作。 */
export type SwitchActionName =
  | "Switch"
  | "SaveCurrentLogin"
  | "BackupCurrent"
  | "RestoreOnly"
  | "ResetDeviceIds"
  | "KeepAlive";

export interface SwitchStep {
  stage: string;
  status: string;
  message: string;
}

export interface SwitchActionResult {
  ok: boolean;
  message: string;
  steps: SwitchStep[];
}

/**
 * 执行一次登录态动作。
 *
 * `expectedCurrentUid` 是防误覆盖守卫：只有它与快照目录里记录的当前账号一致时，
 * 才会把现场登录态写回来源账号的槽位。
 */
export function switchAction(args: {
  action: SwitchActionName;
  targetApp: string;
  userId?: string | null;
  proxyPort?: number | null;
  includeIndexeddb?: boolean;
  expectedCurrentUid?: string | null;
}): Promise<SwitchActionResult> {
  return call<SwitchActionResult>("switch_action", {
    action: args.action,
    targetApp: args.targetApp,
    userId: args.userId ?? null,
    proxyPort: args.proxyPort ?? null,
    includeIndexeddb: args.includeIndexeddb ?? false,
    expectedCurrentUid: args.expectedCurrentUid ?? null,
  });
}

/** 当前登录态属于哪个账号。 */
export function currentAccount(targetApp: string): Promise<{ userId: string }> {
  return call<{ userId: string }>("current_account", { targetApp });
}

/** 快照条目。 */
export interface SnapshotItem {
  userId: string;
  isCurrent: boolean;
  modifiedAt: number | null;
  hasMeta: boolean;
}

/** 列出某应用的登录态快照。 */
export function listSnapshots(
  targetApp: string,
): Promise<{ snapshots: SnapshotItem[]; currentUserId: string }> {
  return call<{ snapshots: SnapshotItem[]; currentUserId: string }>("list_snapshots", {
    targetApp,
  });
}

/** 删除某个账号的快照（含上一代备份）。 */
export function deleteSnapshot(
  targetApp: string,
  userId: string,
): Promise<{ ok: boolean }> {
  return call<{ ok: boolean }>("delete_snapshot", { targetApp, userId });
}

/** Trae 账号（JWT 已脱敏）。 */
export interface TraeAccountMeta {
  userId: string;
  name: string;
  addedAt: string | null;
  updatedAt: string | null;
  jwtStatus: "ok" | "warn" | "expired" | "unknown";
  jwtExpHours: number | null;
  jwtExpTimestamp: number | null;
  hasRefreshToken: boolean;
  refreshTokenInvalid: boolean;
  refreshTokenFails: number;
  deviceIdMasked: string;
  cooldown?: { type?: string; until?: number; reason?: string; error_count?: number };
}

/** Trae 账号列表。 */
export function traeListAccounts(): Promise<{ accounts: TraeAccountMeta[] }> {
  return call<{ accounts: TraeAccountMeta[] }>("trae_list_accounts");
}

/** 粘贴 JWT 添加/更新 Trae 账号。 */
export function traeAddAccount(args: {
  jwt: string;
  name?: string;
  refreshToken?: string;
}): Promise<{ ok: boolean; account: TraeAccountMeta }> {
  return call<{ ok: boolean; account: TraeAccountMeta }>("trae_add_account", {
    jwt: args.jwt,
    name: args.name ?? null,
    refreshToken: args.refreshToken ?? null,
  });
}

/** 删除 Trae 账号。 */
export function traeDeleteAccount(userId: string): Promise<{ ok: boolean }> {
  return call<{ ok: boolean }>("trae_delete_account", { userId });
}

// ---------------------------------------------------------------------------
// Qoder（QoderWork）
// ---------------------------------------------------------------------------

/** Qoder 账号区域。空串 = 未指定（不猜，让用户自己选）。 */
export type QoderRegion = "cn" | "intl" | "";

/** Qoder 账号元信息。 */
export interface QoderAccount {
  uid: string;
  nickname: string;
  /** 用户自己填的备注。 */
  note: string;
  region: QoderRegion;
  /** 用户手动禁用：只不接流量。 */
  disabled: boolean;
  createdAt: string;
  lastSeenAt: string;
  /** 剩余额度；0 = 未知。 */
  credits: number;
  creditsTotal: number;
  /** 最近到期时刻（Unix 秒）；0 = 未知。 */
  expireAt: number;
  /** 头像 URL（来自客户端登录态 `auth.v1.dat` 的 `user.avatarUrl`）。 */
  avatarUrl?: string;
  /** 该账号实际可用的模型（刷新账号时查得）。 */
  models?: string[];
}

/** 列表项：账号 + 凭证是否存在。 */
export interface QoderAccountRow extends QoderAccount {
  /** 凭证文件在不在。为 false 时界面提示「需重新登录」。 */
  hasCredential: boolean;
}

export interface QoderSummary {
  total: number;
  enabled: number;
  disabled: number;
  withCredits: number;
  totalCredits: number;
  authDir: string;
  storeDir: string;
}

/** Qoder 账号列表。 */
export function qoderListAccounts(): Promise<{
  accounts: QoderAccountRow[];
  /** 有凭证但没登记进账号库的 uid（导入后未登记）。 */
  orphanCredentials: string[];
  summary: QoderSummary;
}> {
  return call("qoder_list_accounts");
}

/** 新增/更新 Qoder 账号（局部更新：只覆盖传入的字段）。 */
export function qoderSaveAccount(
  uid: string,
  patch: Partial<Pick<QoderAccount, "nickname" | "note" | "region" | "disabled" | "credits" | "creditsTotal" | "expireAt">>,
): Promise<{ ok: boolean; account: QoderAccount }> {
  return call("qoder_save_account", { uid, patch });
}

/** 删除 Qoder 账号（连同凭证文件）。 */
export function qoderDeleteAccount(uid: string): Promise<{ ok: boolean }> {
  return call("qoder_delete_account", { uid });
}

/** Qoder 账号库概览。 */
export function qoderSummary(): Promise<QoderSummary> {
  return call("qoder_summary");
}

/** 发起 Qoder 登录，返回授权链接与会话标识。 */
export function qoderLoginStart(region: Exclude<QoderRegion, "">): Promise<{
  authUrl: string;
  sessionId: string;
  region: QoderRegion;
}> {
  return call("qoder_login_start", { region });
}

/** 轮询一次登录结果。 */
export function qoderLoginPoll(sessionId: string): Promise<
  | { status: "pending" }
  | { status: "ok"; uid: string; account: QoderAccount }
> {
  return call("qoder_login_poll", { sessionId });
}

/** 导入已有凭证（单个文件或目录）。 */
export interface QoderImportResult {
  imported: Array<{ file: string; uid: string; region: QoderRegion }>;
  failed: Array<{ file: string; error: string }>;
  importedCount: number;
  failedCount: number;
}

export function qoderImportCredentials(path: string): Promise<QoderImportResult> {
  return call<QoderImportResult>("qoder_import_credentials", { path });
}

/**
 * 从 **Qoder 客户端自己的登录态**一键导入（**主路径**）。
 *
 * ## 为什么这才是主路径
 *
 * 网页授权在本机走不通：授权链接的 `redirect_uri` 是
 * `qoder-work-cn://` —— 一个**自定义协议**，只有真正的 Qoder 客户端
 * 才会注册它。我们不是它，浏览器授权完成后**无处回调**。
 *
 * 而客户端已经登录了，登录态就在它的数据目录里。读它即可 ——
 * 用户什么都不用点。
 *
 * @param clientDir 客户端数据目录；留空则自动探测
 */
export interface QoderClientImportResult {
  status: string;
  uid: string;
  nickname: string;
  region: QoderRegion;
  /** 实际读取的客户端目录（让用户知道"从哪读的"）。 */
  clientDir: string;
  expiresAt: string | null;
  account: QoderAccount;
  /**
   * 导入后是否成功补全了额度/套餐/模型。
   *
   * 导入流程现在会顺手查一次上游（否则界面显示「未知」，
   * 用户会以为导入失败）。查失败**不影响**导入成功 ——
   * 凭证已落盘可用，只是这几个字段暂时是空的。
   */
  enriched?: boolean;
  /** 补全失败的原因（为空表示成功）。 */
  enrichError?: string | null;
}

export function qoderImportFromClient(clientDir = ""): Promise<QoderClientImportResult> {
  return call<QoderClientImportResult>("qoder_import_from_client", { clientDir });
}

/**
 * 刷新账号的额度 / 到期时间 / 支持模型。
 *
 * ## 为什么需要它
 *
 * 后端的 `FetchQuota` / `FetchModels` 早就实现了，但**没有任何生产者
 * 调用** —— 于是界面上额度恒为 0、到期时间恒为空、看不到支持模型。
 * 这个接口补上那条链路（结果会写回账号库）。
 *
 * `quota.error` 有值表示"查不到额度"（原因在里面），
 * 此时 `quota.remaining` 为 null —— 界面应显示「未知」而不是 0，
 * 因为 0 会被用户误读成"额度耗尽"。
 */
export interface RefreshAccountResult {
  status: string;
  account: Record<string, unknown>;
  quota: {
    remaining: number | null;
    total: number | null;
    expiresAt: number | null;
    /** Qoder：用量百分比（0..1）。 */
    usagePercent?: number;
    /** Qoder：套餐/账号等级。 */
    planTierName?: string;
    /** Qoder：上游是否判定超额（新账号也会为 true，见后端注释）。 */
    exceeded?: boolean;
    /** ZCode：额度明细（可能多项）。 */
    entries?: Array<{
      showName: string;
      remaining: number;
      total: number;
      used: number;
      unitType: string;
      expiresAt: number;
    }>;
    /** 非空表示查不到额度，内容是可读原因。 */
    error: string | null;
  };
  /** 该账号可用的模型（空数组表示没拿到）。 */
  models: AccountModel[];
  /** 非空表示模型清单没拿到。 */
  modelsError: string | null;
}

/** 账号可用模型的统一形状（两个产品的字段归一到这里）。 */
export interface AccountModel {
  /** ZCode：模型 id（如 `glm-5.3`）。 */
  id?: string;
  /** ZCode：展示名。 */
  name?: string;
  /** Qoder：上游模型 key。 */
  key?: string;
  /** Qoder：展示名。 */
  displayName?: string;
  /** ZCode。 */
  contextWindow?: number;
  maxOutput?: number;
  /** Qoder。 */
  maxInputTokens?: number;
  /** 是否推理模型。 */
  reasoning?: boolean;
  isReasoning?: boolean;
  /** 是否视觉模型。 */
  vision?: boolean;
  isVL?: boolean;
  /** ZCode：来源 `upstream` / `builtin`。 */
  source?: string;
  /** Qoder：是否启用。 */
  enable?: boolean;
}

export function qoderRefreshAccount(uid: string): Promise<RefreshAccountResult> {
  return call<RefreshAccountResult>("qoder_refresh_account", { uid });
}

/**
 * Qoder 的一条权益活动（「每天领 100 Credits」那类）。
 *
 * ## 字段语义（取自上游真实响应，见后端 campaign.go）
 *
 * `claimStatus`：
 *   · `CLAIMABLE` —— 可领取（界面应高亮）
 *   · `CLAIMED`   —— 已领取（不要显示成可领）
 *   · 其它/空      —— 未知，按"不可领"处理更安全
 *
 * `actionType`：`CLAIM_BENEFIT`（要领取）/ `VIEW_DETAILS`（只看详情）
 *
 * `endAt` 是 Unix **秒**（不是毫秒）。当毫秒用会得到 1970 年。
 */
export interface QoderCampaign {
  campaignId: string;
  campaignKey: string;
  actionType: "CLAIM_BENEFIT" | "VIEW_DETAILS" | string;
  claimStatus: "CLAIMABLE" | "CLAIMED" | string;
  /** Unix 秒。 */
  startAt: number;
  /** Unix 秒。 */
  endAt: number;
  benefit?: {
    kind: string;
    amount: number;
    validity?: { mode: string; days: number };
  };
  placements?: Array<{
    type: string;
    campaignUrl: string;
    content?: Record<string, { buttonText?: string; description?: string; detailUrl?: string }>;
  }>;
}

export interface QoderCampaignsResult {
  status: string;
  uid: string;
  /** 上游是否要展示活动入口。 */
  showCampaign: boolean;
  /** 是否有**可领取**的活动。 */
  claimable: boolean;
  /** 活动页（服务端渲染的 iframe 页面）。 */
  campaignUrl: string;
  count: number;
  campaigns: QoderCampaign[];
}

/**
 * 查询 Qoder 账号的权益活动。
 *
 * ## 只读，不代领
 *
 * 领取要阿里云验证码（服务端的防滥用机制），故本应用**不提供**领取
 * 动作。界面只展示活动与倒计时，并把 `campaignUrl` 交给用户自己去打开。
 */
export function qoderCampaigns(uid: string): Promise<QoderCampaignsResult> {
  return call<QoderCampaignsResult>("qoder_campaigns", { uid });
}

/** 领取结果。 */
export interface QoderClaimResult {
  status: string;
  uid: string;
  grantId: string;
  campaignId: string;
  campaignKey: string;
  claimed: boolean;
  /**
   * 这次是**重放**（之前已领过），不是新领到。
   *
   * ⚠ 界面必须区分：说"领取成功"会让用户以为又领了一份。
   */
  replayed: boolean;
  claimedAt: string;
  grantedAt: string;
}

/**
 * 领取一个权益活动。
 *
 * ## 只能由用户显式点击触发
 *
 * 领取本身是安全的（用用户自己的令牌打官方接口，与官方客户端点那个
 * 「领取」按钮同构、**不需要人机验证**）。但**不做定时自动领取** ——
 * 那与"用户点一下"不是一回事，且会让账号表现出非人类的活动模式。
 */
export function qoderClaimCampaign(
  uid: string,
  campaignId: string,
): Promise<QoderClaimResult> {
  return call<QoderClaimResult>("qoder_claim_campaign", { uid, campaignId });
}

// ---------------------------------------------------------------------------
// ZCode（Z.AI / 智谱 GLM 编码套餐）
// ---------------------------------------------------------------------------

/**
 * ZCode 服务商。
 *
 * **注意这是"服务商"而不是"区域"** —— 与 Qoder 的国服/国际版不同，
 * Z.AI 与智谱是两家不同公司，各有独立域名与凭证来源。
 * 空串 = 未指定（不猜，让用户自己选）。
 */
export type ZcodeProvider = "zai" | "bigmodel" | "";

/** ZCode 账号元信息。 */
export interface ZcodeAccount {
  uid: string;
  nickname: string;
  note: string;
  provider: ZcodeProvider;
  disabled: boolean;
  createdAt: string;
  lastSeenAt: string;
  /**
   * 剩余额度；0 = 未知。
   *
   * ⚠ ZCode 的额度查询需要 **JWT**（OAuth 登录时才有）。只导入了凭证的
   * 账号查不到额度 —— 界面应显示"未知"而不是 0，因为 0 会被误读成"额度耗尽"。
   */
  credits: number;
  creditsTotal: number;
  /** 最近到期时刻（Unix 秒）；0 = 未知。 */
  expireAt: number;
  /** 头像 URL（来自客户端登录态 `credentials.json`）；空 = 没有。 */
  avatarUrl?: string;
  /**
   * 上游**账号**标识（如 `19331730795565300`）；空 = 未知。
   *
   * 用途：识别「同一账号的多把 API key」—— 它们的 `uid` 不同
   *（uid 是凭证哈希），但 `accountId` 相同，在用户看来就是重复。
   */
  accountId?: string;
  /** 该账号实际可用的模型 id（刷新账号时查得）。 */
  models?: string[];
  /**
   * **套餐整体**到期（Unix 秒）；0 = 无套餐或未知。
   *
   * ⚠ 与 `expireAt` 不是一回事（最容易搞混的一处）：
   *
   *     expireAt      各模型桶的**每日周期**结束（实测当天 23:59:59）→ 明天重置
   *     planExpireAt  套餐**整体**到期（实测 9/23 23:59:59）→ 之后归零
   *
   * 只显示前者，你会以为"明天额度就没了"；只显示后者，你会以为
   * "今天用不完就浪费了"。两个都要显示。
   */
  planExpireAt?: number;
  /**
   * 套餐类型：
   *   · `trial`    体验套餐（新账号赠送，每日重置，**有到期日**）
   *   · `paid`     付费套餐（Coding Plan）
   *   · `api_key`  按量付费（没有预发额度）
   *   · `unknown`  判不出来（拿不到上游配置）—— **不猜**
   *
   * 判据是拿上游静态配置（`startPlanPreview`）的赠送量与实际总量比对，
   * 没有硬编码数字（那 300 万/500 万是运营参数，上游随时可改）。
   */
  planKind?: string;
  /** 生效中的套餐（含名称、说明、各模型每日赠送量）。 */
  plans?: Array<{
    planId: string;
    name: string;
    description: string;
    status: string;
    startsAt: number;
    /** **套餐整体**到期（Unix 秒）。 */
    endsAt: number;
    entitlements?: Array<{
      showName: string;
      /** 每日赠送量（token）。 */
      grantUnits: number;
      /** 重置周期，实测 `daily`。 */
      period: string;
      unitType: string;
    }>;
  }>;
}

/** 列表项：账号 + 凭证是否存在。 */
export interface ZcodeAccountRow extends ZcodeAccount {
  hasCredential: boolean;
}

export interface ZcodeSummary {
  total: number;
  enabled: number;
  disabled: number;
  totalCredits: number;
  authDir: string;
  storeDir: string;
}

/** ZCode 账号列表。 */
export function zcodeListAccounts(): Promise<{
  accounts: ZcodeAccountRow[];
  orphanCredentials: string[];
  summary: ZcodeSummary;
}> {
  return call("zcode_list_accounts");
}

/** 新增/更新 ZCode 账号（局部更新）。 */
export function zcodeSaveAccount(
  uid: string,
  patch: Partial<Pick<ZcodeAccount, "nickname" | "note" | "provider" | "disabled" | "credits" | "creditsTotal" | "expireAt">>,
): Promise<{ ok: boolean; account: ZcodeAccount }> {
  return call("zcode_save_account", { uid, patch });
}

/** 删除 ZCode 账号（连同凭证文件）。 */
export function zcodeDeleteAccount(uid: string): Promise<{ ok: boolean }> {
  return call("zcode_delete_account", { uid });
}

/** ZCode 账号库概览。 */
export function zcodeSummary(): Promise<ZcodeSummary> {
  return call("zcode_summary");
}

/**
 * **导入凭证**（ZCode 的主路径）。
 *
 * 用户从控制台复制 `{apiKey}.{secret}` 粘贴进来。
 */
export function zcodeImportCredential(args: {
  credential: string;
  provider?: string;
  nickname?: string;
}): Promise<{ status: string; uid: string; provider: ZcodeProvider; account: ZcodeAccount }> {
  return call("zcode_import_credential", {
    credential: args.credential,
    provider: args.provider ?? null,
    nickname: args.nickname ?? null,
  });
}

/** 从目录批量导入凭证文件。 */
export interface ZcodeImportDirResult {
  imported: Array<{ file: string; uid: string; provider: ZcodeProvider }>;
  failed: Array<{ file: string; error: string }>;
  importedCount: number;
  failedCount: number;
}

export function zcodeImportFromDir(path: string): Promise<ZcodeImportDirResult> {
  return call<ZcodeImportDirResult>("zcode_import_from_dir", { path });
}

/**
 * 扫描本机 ZCode 客户端的登录凭证。
 *
 * ZCode 官方客户端把 apiKey **明文**落在 `~/.zcode/v2/config.json`，
 * 所以我们能直接读出来 —— 用户不必手工去控制台复制。
 *
 * ⚠ 返回的是**掩码**（`masked`），不含凭证本体：前端不需要它，
 * 而它会进 DOM、可能被截图、被 devtools 复制。
 */
export interface ZcodeScannedItem {
  /** 在本次扫描结果里的位置，导入时回传这个。 */
  index: number;
  /** 来源文件（让用户知道"从哪读的"）。 */
  source: string;
  provider: ZcodeProvider;
  providerLabel: string;
  /** 掩码后的凭证（如 `abc123…wxyz（49 字符）`）。 */
  masked: string;
  credentialLength: number;
  suggestedNickname: string;
  /** 客户端里的原始条目名（如 `builtin:bigmodel-coding-plan`）。 */
  originName: string;
  enabled: boolean;
  /**
   * 凭证形态：
   *   `two-part` = `{apiKey}.{secret}`，可直接对话
   *   `jwt`      = 只是**额度查询**用的令牌，不能当对话凭证
   *   `single`   = 单段（智谱的 secret 可选，合法）
   */
  shape: "two-part" | "jwt" | "single";
  /** 由凭证派生的 uid（与 Go 侧一致），用于判断是否已导入。 */
  uid: string;
  alreadyImported: boolean;
  /**
   * 是否配到了额度令牌（JWT）。
   *
   * 额度查询**只认 JWT**，不认对话凭证。ZCode 客户端把两者放在不同的
   * provider 条目里（`*-coding-plan` 是凭证、`*-start-plan` 是 JWT），
   * 扫描时会把同一服务商的 JWT 配对过来。
   *
   * 界面据此说明"导入后能读到额度" —— 没有它时讲清楚"额度会显示未知"。
   */
  hasQuotaToken: boolean;
  /**
   * 账号名（来自客户端登录态，如 `wish`）。
   *
   * ## 为什么它可以和 suggestedNickname 不同
   *
   * `suggestedNickname` 的兜底是客户端配置里的 `provider[*].name`，
   * 那是**套餐名**（"BigModel - Coding Plan"），不是账号名。
   * 真正的账号名在客户端登录态里（`credentials.json` 的 `user_info`，
   * 经 `enc:v1:` 解密）—— 有它时两个字段相同，没有时只用兜底值。
   */
  accountName?: string;
}

/** 客户端登录态（读不到时为 null）。 */
export interface ZcodeIdentity {
  username: string;
  displayName: string;
  id: string;
  avatarUrl: string;
  activeProvider: string;
}

export interface ZcodeScanResult {
  count: number;
  items: ZcodeScannedItem[];
  /** 扫描过程中的问题（如文件不是合法 JSON）。 */
  problems: string[];
  /** 扫过哪些路径 —— 用户能自己判断"是不是没找对地方"。 */
  scannedPaths: string[];
  existingAccountCount: number;
  /** 已登录的账号（界面用来说明"已登录为 xxx"）。 */
  identity: ZcodeIdentity | null;
}

export function zcodeScanLocal(): Promise<ZcodeScanResult> {
  return call<ZcodeScanResult>("zcode_scan_local");
}

/**
 * 导入扫描结果里选中的项。
 *
 * 只传**索引**，凭证本体从不离开进程 —— 详见后端 `import_scanned` 的注释。
 */
export interface ZcodeImportScannedResult {
  imported: Array<{
    index: number;
    uid: string;
    provider: ZcodeProvider;
    origin: string;
    account: ZcodeAccount;
    /**
     * 导入后是否成功补全了额度/套餐/模型。
     *
     * 导入流程现在会顺手查一次上游（否则界面显示「未知」，
     * 用户会以为导入失败）。查失败**不影响**导入成功 ——
     * 凭证已落盘可用，只是这几个字段暂时是空的。
     */
    enriched?: boolean;
    /** 补全失败的原因（为空表示成功）。 */
    enrichError?: string | null;
  }>;
  failed: Array<{ index: number; origin?: string; error: string }>;
  importedCount: number;
  failedCount: number;
}

export function zcodeImportScanned(indices: number[]): Promise<ZcodeImportScannedResult> {
  return call<ZcodeImportScannedResult>("zcode_import_scanned", { indices });
}

/** 刷新 ZCode 账号的额度 / 到期 / 支持模型（形状与 Qoder 侧相同）。 */
export function zcodeRefreshAccount(uid: string): Promise<RefreshAccountResult> {
  return call<RefreshAccountResult>("zcode_refresh_account", { uid });
}

/** 发起 ZCode 登录（OAuth 设备流，备选路径）。 */
export function zcodeLoginStart(provider: Exclude<ZcodeProvider, "">): Promise<{
  authUrl: string;
  sessionId: string;
  provider: ZcodeProvider;
}> {
  return call("zcode_login_start", { provider });
}

/** 轮询一次 ZCode 登录结果。 */
export function zcodeLoginPoll(sessionId: string): Promise<
  | { status: "pending" }
  | { status: "ok"; uid: string; account: ZcodeAccount }
> {
  return call("zcode_login_poll", { sessionId });
}

/** 本机发现到的 Trae 账号候选。 */
export interface TraeDiscoveredAccount {
  userId: string;
  dcUid: string | null;
  /** 为假时**禁止入池**：uid 属于账户中心 id 空间，与账号池不同体系。 */
  uidConfident: boolean;
  appKind: string;
  appLabel: string;
  apps: string[];
  evidenceTsMs: number;
  evidenceCount: number;
  inPool: boolean;
  payIdentity: string | null;
}

/** 发现本机登录过的 Trae 账号（Trae Work + Trae 双应用）。 */
export function traeDiscoverAccounts(): Promise<{
  accounts: TraeDiscoveredAccount[];
  apps: { kind: string; label: string }[];
}> {
  return call<{ accounts: TraeDiscoveredAccount[]; apps: { kind: string; label: string }[] }>(
    "trae_discover_accounts",
  );
}

/** 读取某应用的套餐身份。 */
export function traeEntitlement(
  appKind: string,
): Promise<{ identity?: string | null; raw?: unknown }> {
  return call<{ identity?: string | null; raw?: unknown }>("trae_entitlement", { appKind });
}

/** 读取（或重置）账号的设备指纹。 */
export function traeDeviceInfo(
  userId: string,
  reset = false,
): Promise<{ userId: string; deviceId: string; sessionId: string; marketUserId: string }> {
  return call<{ userId: string; deviceId: string; sessionId: string; marketUserId: string }>(
    "trae_device_info",
    { userId, reset },
  );
}

/** 单账号签到结果。 */
export interface TraeCheckinOutcome {
  userId: string;
  name: string;
  status: "success" | "already" | "fail" | "skip";
  code: number | null;
  message: string;
  credits: number | null;
  delta: number | null;
  errorType: string | null;
  cooldownUntil: number | null;
}

export interface TraeCheckinResult {
  ok: number;
  already: number;
  failed: number;
  skipped: number;
  total: number;
  results: TraeCheckinOutcome[];
}

/** 执行一轮 Trae 签到（不传 userIds = 全部账号）。 */
export function traeCheckinRun(
  userIds?: string[],
  retry = 1,
): Promise<TraeCheckinResult> {
  return call<TraeCheckinResult>("trae_checkin_run", {
    userIds: userIds && userIds.length > 0 ? userIds : null,
    retry,
  });
}

/** Trae 积分历史记录。 */
export function traeCreditsHistory(): Promise<{
  records: { date: string; userId: string; credits: number; delta: number }[];
}> {
  return call<{ records: { date: string; userId: string; credits: number; delta: number }[] }>(
    "trae_credits_history",
  );
}

/** 清除某账号的签到冷却。 */
export function traeClearCooldown(userId: string): Promise<{ ok: boolean }> {
  return call<{ ok: boolean }>("trae_clear_cooldown", { userId });
}

/** 豆包账号视图（凭证已脱敏）。 */
export interface DoubaoAccountView {
  userId: string;
  name: string | null;
  note: string | null;
  addedAt: string | null;
  lastActiveAt: string | null;
  sessionIdMasked: string;
  sidGuardMasked: string;
  ttwidMasked: string;
  hasSessionId: boolean;
  hasTtwid: boolean;
  sessionExpireAt: string | null;
  expired: boolean | null;
  sessionState: "ok" | "expired" | "unknown" | "none";
  sessionSource: string | null;
  cookiesSyncedAt: string | null;
  lastRenewAt: string | null;
  quotaLevel: string | null;
  quotaExpireAt: string | null;
  quotaSummary: string | null;
  quotaCheckedAt: string | null;
}

/** 豆包账号列表。 */
export function doubaoListAccounts(): Promise<{
  accounts: DoubaoAccountView[];
  lastKeepaliveAt: string | null;
}> {
  return call<{ accounts: DoubaoAccountView[]; lastKeepaliveAt: string | null }>(
    "doubao_list_accounts",
  );
}

/** 新增/更新豆包账号。 */
export function doubaoPublishAccount(args: {
  userId: string;
  name?: string;
  note?: string;
}): Promise<{ ok: boolean; account: DoubaoAccountView }> {
  return call<{ ok: boolean; account: DoubaoAccountView }>("doubao_save_account", {
    userId: args.userId,
    name: args.name ?? null,
    note: args.note ?? null,
  });
}

/** 删除豆包账号。 */
export function doubaoDeleteAccount(userId: string): Promise<{ ok: boolean }> {
  return call<{ ok: boolean }>("doubao_delete_account", { userId });
}

/** 读取账号的明文凭证（仅编辑弹窗回填用）。 */
export function doubaoGetCredential(userId: string): Promise<{
  userId: string;
  sessionId: string | null;
  sidGuard: string | null;
  ttwid: string | null;
}> {
  return call<{
    userId: string;
    sessionId: string | null;
    sidGuard: string | null;
    ttwid: string | null;
  }>("doubao_get_credential", { userId });
}

/** 设置账号凭证。 */
export function doubaoSetCredential(args: {
  userId: string;
  sessionId?: string | null;
  sidGuard?: string | null;
  ttwid?: string | null;
}): Promise<{ ok: boolean; account: DoubaoAccountView }> {
  return call<{ ok: boolean; account: DoubaoAccountView }>("doubao_set_credential", {
    userId: args.userId,
    sessionId: args.sessionId ?? null,
    sidGuard: args.sidGuard ?? null,
    ttwid: args.ttwid ?? null,
  });
}

/** 读取最近一次代理抓包凭证。 */
export function doubaoCapturedCredential(): Promise<{
  available: boolean;
  uid?: string | null;
  host?: string | null;
  capturedAt?: string | null;
  sessionId?: string | null;
  sidGuard?: string | null;
  ttwid?: string | null;
}> {
  return call<{
    available: boolean;
    uid?: string | null;
    host?: string | null;
    capturedAt?: string | null;
    sessionId?: string | null;
    sidGuard?: string | null;
    ttwid?: string | null;
  }>("doubao_captured_credential");
}

/** 把抓包凭证回写账号池（幂等）。 */
export function doubaoCredentialAutoApply(): Promise<{
  applied: boolean;
  account?: DoubaoAccountView;
}> {
  return call<{ applied: boolean; account?: DoubaoAccountView }>(
    "doubao_credential_auto_apply",
  );
}

/** 会话保活（启动客户端触发服务端滑动续期）。 */
export function doubaoKeepalive(): Promise<{ ok: boolean; message: string }> {
  return call<{ ok: boolean; message: string }>("doubao_keepalive");
}

/** HTTP 续期探活。 */
export function doubaoRenew(syncOnly = false): Promise<{
  ok: number;
  expired: number;
  skipped: number;
  errors: number;
  total: number;
  results: { userId: string; name: string; status: string; message: string }[];
}> {
  return call<{
    ok: number;
    expired: number;
    skipped: number;
    errors: number;
    total: number;
    results: { userId: string; name: string; status: string; message: string }[];
  }>("doubao_renew", { syncOnly });
}

/** 会话与凭证诊断。 */
export function doubaoDiagnose(): Promise<Record<string, unknown>> {
  return call<Record<string, unknown>>("doubao_diagnose");
}

/** 额度窗口项。 */
export interface DoubaoQuotaWindow {
  name: string;
  usedPercent: number | null;
  exhausted: boolean;
  resetAt: string | null;
}

/** 额度查询结果。 */
export interface DoubaoQuotaView {
  userId: string;
  ok: boolean;
  level: string | null;
  expireAt: string | null;
  hasSubscription: boolean;
  isGift: boolean;
  subscription: Record<string, unknown> | null;
  windows: DoubaoQuotaWindow[];
  items: { name: string; total: number; left: number | null; used: number | null }[];
  summary: string;
}

/** 查询单个账号的会员额度。 */
export function doubaoFetchQuota(userId: string): Promise<DoubaoQuotaView> {
  return call<DoubaoQuotaView>("doubao_fetch_quota", { userId });
}

/** 批量巡检全部账号额度。 */
export function doubaoQuotaBatch(): Promise<{
  ok: number;
  failed: number;
  exhausted: string[];
  results: { userId: string; name: string; ok: boolean; summary?: string; error?: string }[];
}> {
  return call<{
    ok: number;
    failed: number;
    exhausted: string[];
    results: { userId: string; name: string; ok: boolean; summary?: string; error?: string }[];
  }>("doubao_quota_batch");
}

/** 账号会话探活。 */
export function doubaoProbeAccount(
  userId: string,
): Promise<{ ok: boolean; status?: string; detail?: string; error?: string }> {
  return call<{ ok: boolean; status?: string; detail?: string; error?: string }>(
    "doubao_probe_account",
    { userId },
  );
}

/** 备份账号的客户端对话状态。 */
export function doubaoBackupChatdata(
  userId: string,
): Promise<{ ok: boolean; userId: string; files: number; path: string }> {
  return call<{ ok: boolean; userId: string; files: number; path: string }>(
    "doubao_backup_chatdata",
    { userId },
  );
}

/** 恢复账号的客户端对话状态。 */
export function doubaoRestoreChatdata(
  userId: string,
): Promise<{ ok: boolean; userId: string; profiles: number }> {
  return call<{ ok: boolean; userId: string; profiles: number }>("doubao_restore_chatdata", {
    userId,
  });
}

/** 对话备份信息。 */
export function doubaoChatdataInfo(userId: string): Promise<{
  backed: boolean;
  files?: number;
  backedAt?: string | null;
  sizeBytes?: number;
  path?: string;
}> {
  return call<{
    backed: boolean;
    files?: number;
    backedAt?: string | null;
    sizeBytes?: number;
    path?: string;
  }>("doubao_chatdata_info", { userId });
}

/** 从官方 IM API 导出对话。 */
export function doubaoExportChats(
  userId: string,
  limitConvs = 50,
  maxPages = 10,
): Promise<{
  ok: boolean;
  conversations: number;
  messages: number;
  jsonPath: string;
  mdPath: string;
}> {
  return call<{
    ok: boolean;
    conversations: number;
    messages: number;
    jsonPath: string;
    mdPath: string;
  }>("doubao_export_chats", { userId, limitConvs, maxPages });
}

/** 读取应用设置。 */
export function getAppSettings(): Promise<Record<string, unknown>> {
  return call<Record<string, unknown>>("get_app_settings");
}

/** 合并写入应用设置。 */
export function saveAppSettings(
  patch: Record<string, unknown>,
): Promise<Record<string, unknown>> {
  return call<Record<string, unknown>>("save_app_settings", { patch });
}

// ---------------------------------------------------------------------------
// 本地 MITM 代理（设备身份隔离 + 凭证抓取）
// ---------------------------------------------------------------------------

/** 代理配置。 */
export interface ProxyConfigView {
  port: number;
  domains: string;
  defaultDomains: string;
  lastPort: number | null;
  /** 用户原有的系统代理 [启用, 地址, 绕过列表]，停止时会原样还原。 */
  existingSystemProxy: [boolean, string, string] | null;
}

/** 读取代理配置。 */
export function proxyConfig(): Promise<ProxyConfigView> {
  return call<ProxyConfigView>("proxy_config");
}

/** 代理运行状态。 */
export function proxyStatus(): Promise<{ running: boolean; port: number | null; captured: number }> {
  return call<{ running: boolean; port: number | null; captured: number }>("proxy_status");
}

/**
 * 启动代理并接管系统代理。
 *
 * 代理会改写系统代理设置；停止时还原为用户原有的值。启动前会先记下原值，
 * 因此不会把「上一次自己设的」误当成用户设置。
 */
export function proxyStart(
  port?: number,
  domains?: string,
): Promise<{ ok: boolean; port: number; domains: string }> {
  return call<{ ok: boolean; port: number; domains: string }>("proxy_start", {
    port: port ?? null,
    domains: domains ?? null,
  });
}

/** 停止代理并还原系统代理。 */
export function proxyStop(): Promise<{ ok: boolean; alreadyStopped?: boolean }> {
  return call<{ ok: boolean; alreadyStopped?: boolean }>("proxy_stop");
}

/** CA 证书状态。 */
export function proxyCertStatus(): Promise<{
  certsDir: string;
  caCerPath: string;
  caPemPath: string;
  caExists: boolean;
  hint: string;
}> {
  return call<{
    certsDir: string;
    caCerPath: string;
    caPemPath: string;
    caExists: boolean;
    hint: string;
  }>("proxy_cert_status");
}

/** 生成自签 CA（已存在则复用）。 */
export function proxyCertGenerate(): Promise<{
  ok: boolean;
  certsDir: string;
  caCerPath: string;
}> {
  return call<{ ok: boolean; certsDir: string; caCerPath: string }>("proxy_cert_generate");
}

/** 从本机离线捕获 Trae 的 Cloud-IDE-JWT（代理抓不到时的兜底）。 */
export function proxyCaptureLocal(): Promise<{
  ok: boolean;
  captured?: number;
  accounts?: { userId: string; source: string }[];
  message?: string;
}> {
  return call<{
    ok: boolean;
    captured?: number;
    accounts?: { userId: string; source: string }[];
    message?: string;
  }>("proxy_capture_local");
}

/** 清理上一次异常退出残留的系统代理设置。 */
export function proxyCleanupStale(): Promise<{ ok: boolean; restored: string | null }> {
  return call<{ ok: boolean; restored: string | null }>("proxy_cleanup_stale");
}

/** 解析上游代理地址（供界面校验输入）。 */
export function proxyParseUpstream(
  spec: string,
): Promise<{ ok: boolean; addr?: string; error?: string }> {
  return call<{ ok: boolean; addr?: string; error?: string }>("proxy_parse_upstream", { spec });
}

// ---------------------------------------------------------------------------
// 计划任务（Windows schtasks）
// ---------------------------------------------------------------------------

/** 计划任务状态。 */
export interface TaskStatusItem {
  kind: string;
  name: string;
  label: string;
  cliKey: string;
  registered: boolean;
  time: string;
  error: string | null;
}

/** 查询全部计划任务的注册状态。 */
export function taskStatus(): Promise<{ tasks: TaskStatusItem[] }> {
  return call<{ tasks: TaskStatusItem[] }>("task_status");
}

/** 注册（或覆盖）一个每日计划任务。时间格式 HH:MM。 */
export function taskRegister(
  kind: string,
  time: string,
): Promise<{ ok: boolean; message: string }> {
  return call<{ ok: boolean; message: string }>("task_register", { kind, time });
}

/** 删除计划任务（不存在也算成功）。 */
export function taskUnregister(kind: string): Promise<{ ok: boolean }> {
  return call<{ ok: boolean }>("task_unregister", { kind });
}

/** 立即执行一次任务（不依赖计划任务，用于验证配置）。 */
export function taskRunNow(kind: string): Promise<{ ok: boolean; exitCode: number }> {
  return call<{ ok: boolean; exitCode: number }>("task_run_now", { kind });
}
