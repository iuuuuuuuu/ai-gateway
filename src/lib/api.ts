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
  CliQuotaAccount,
  CliQuotaProvider,
  CliQuotaStatusItem,
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
  GatewayModelsResult,
  GatewayStartResult,
  GatewayStatus,
  GatewayPortCheck,
  GatewaySyncResult,
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
  "get_gateway_status", "get_gateway_models", "detect_agent_clients", "list_agent_backups",
  "get_cli_quotas", "get_cli_quota_status",
  // Trae / 豆包 只读查询：演示模式下允许展示，写操作一律拒绝
  "trae_list_accounts", "trae_credits_stats", "trae_credits_history",
  "trae_pay_status_cache", "groups_list", "in_app_schedule_view",
  "doubao_list_accounts", "doubao_history", "doubao_settings",
  "proxy_logs_list", "proxy_logs_overview",
  // 点击即读（详情弹窗 / 查看凭证）：不进演示模式会让点击直接报错
  "proxy_log_detail", "trae_credit_detail", "trae_account_jwt",
  // 挂载即读的命令：页面/组件一进入就调用，缺一个就会让整块数据空白
  "app_env_check", "trae_checkin_trends", "trae_usage_history",
  "proxy_config", "proxy_status", "proxy_cert_status", "task_status",
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
  set_allowed_model: { method: "POST", path: "/api/gateway/allowed-model" },
  start_gateway: { method: "POST", path: "/api/gateway/start" },
  check_gateway_port: { method: "POST", path: "/api/gateway/port-check" },
  stop_gateway: { method: "POST", path: "/api/gateway/stop" },
  restart_gateway: { method: "POST", path: "/api/gateway/restart" },
  sync_gateway_accounts: { method: "POST", path: "/api/gateway/sync" },
  get_gateway_models: { method: "GET", path: "/api/gateway/models" },
  get_gateway_usage: { method: "GET", path: "/api/gateway/usage" },
  // ---- 本机 AI CLI 登录额度查询 ----
  get_cli_quotas: { method: "GET", path: "/api/cli-quota" },
  refresh_cli_quota: { method: "POST", path: "/api/cli-quota/refresh" },
  get_cli_quota_status: { method: "GET", path: "/api/cli-quota/status" },
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
  trae_update_account: { method: "POST", path: "/api/trae/accounts/update" },
  trae_account_jwt: { method: "GET", path: "/api/trae/accounts/jwt" },
  trae_jwt_parse: { method: "POST", path: "/api/trae/jwt/parse" },
  trae_clear_all_cooldowns: { method: "POST", path: "/api/trae/cooldown/clear-all" },
  doubao_settings: { method: "GET", path: "/api/doubao/settings" },
  doubao_set_setting: { method: "POST", path: "/api/doubao/settings" },
  oauth_login_url: { method: "POST", path: "/api/trae/oauth/login-url" },
  oauth_submit_callback: { method: "POST", path: "/api/trae/oauth/callback" },
  oauth_cancel: { method: "POST", path: "/api/trae/oauth/cancel" },
  in_app_schedule_view: { method: "GET", path: "/api/in-app-schedule" },
  run_in_app_due_tasks: { method: "POST", path: "/api/in-app-schedule/run" },
  trae_export_accounts: { method: "POST", path: "/api/trae/export" },
  trae_preview_import: { method: "POST", path: "/api/trae/import/preview" },
  trae_import_accounts: { method: "POST", path: "/api/trae/import" },
  trae_credits_stats: { method: "GET", path: "/api/trae/credits/stats" },
  trae_checkin_trends: { method: "GET", path: "/api/trae/checkin/trends" },
  app_launch: { method: "POST", path: "/api/apps/launch" },
  proxy_logs_list: { method: "GET", path: "/api/proxy/logs" },
  proxy_log_detail: { method: "GET", path: "/api/proxy/logs/detail" },
  proxy_logs_overview: { method: "GET", path: "/api/proxy/logs/overview" },
  proxy_logs_clear: { method: "POST", path: "/api/proxy/logs/clear" },
  trae_usage_history: { method: "GET", path: "/api/trae/usage-history" },
  trae_credits_snapshot: { method: "POST", path: "/api/trae/credits/snapshot" },
  // 回环监听器（17388 端口）只存在于桌面宿主：webui 模式下没有本地进程监听该端口，
  // 因此**故意不登记路由** —— 前端据 isDesktop() 决定是走自动回环还是手动粘贴兜底，
  // 真走到这里会得到「webui 模式暂不支持该操作」而不是一个假的成功。
  trae_discover_accounts: { method: "GET", path: "/api/trae/discover" },
  trae_import_local: { method: "POST", path: "/api/trae/import-local" },
  trae_discover_and_import: { method: "POST", path: "/api/trae/discover/import" },
  trae_entitlement: { method: "GET", path: "/api/trae/entitlement" },
  trae_device_info: { method: "GET", path: "/api/trae/device" },
  trae_checkin_run: { method: "POST", path: "/api/trae/checkin" },
  trae_credits_history: { method: "GET", path: "/api/trae/credits/history" },
  trae_clear_cooldown: { method: "POST", path: "/api/trae/cooldown/clear" },
  // ---- Trae 凭证续期 / 积分 / 套餐身份 ----
  trae_refresh_account: { method: "POST", path: "/api/trae/refresh" },
  trae_refresh_all: { method: "POST", path: "/api/trae/refresh-all" },
  trae_credit_detail: { method: "GET", path: "/api/trae/credits/detail" },
  trae_refresh_pay_status: { method: "POST", path: "/api/trae/pay-status/refresh" },
  trae_pay_status_cache: { method: "GET", path: "/api/trae/pay-status" },
  // ---- 账号分组（Trae / 豆包 分域） ----
  groups_list: { method: "GET", path: "/api/groups" },
  group_create: { method: "POST", path: "/api/groups/create" },
  group_update: { method: "POST", path: "/api/groups/update" },
  group_delete: { method: "POST", path: "/api/groups/delete" },
  group_move: { method: "POST", path: "/api/groups/move" },
  doubao_list_accounts: { method: "GET", path: "/api/doubao/accounts" },
  doubao_save_account: { method: "POST", path: "/api/doubao/accounts/save" },
  doubao_delete_account: { method: "POST", path: "/api/doubao/accounts/delete" },
  doubao_detect_uid: { method: "GET", path: "/api/doubao/detect-uid" },
  doubao_snapshot_meta: { method: "GET", path: "/api/doubao/snapshot-meta" },
  doubao_open_as_account: { method: "POST", path: "/api/doubao/open-as-account" },
  doubao_history: { method: "GET", path: "/api/doubao/history" },
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

export async function getTravelStatus(accountId: string): Promise<TravelStatus> {
  if (demoModeEnabled) {
    return screenshotDemoResponse("get_travel_status", { accountId }) as TravelStatus;
  }
  if (isWebui()) {
    // webui 端为批量接口，按 accountId 过滤
    const all = await httpCall<{
      accounts: { accountId: string; email: string; label: TravelStatus["label"]; rewardCredit: number | null; locationName?: string | null; arriveAt?: number | null }[];
    }>("get_travel_status");
    const one = all.accounts.find((a) => a.accountId === accountId);
    return one
      ? { label: one.label, rewardCredit: one.rewardCredit, locationName: one.locationName ?? null, arriveAt: one.arriveAt ?? null }
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

export function saveGithubConfig(config: GithubConfig): Promise<GithubConfig> {
  return call("save_github_config", {
    config: config as unknown as Record<string, unknown>,
  });
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
  return call<{ config: GatewayConfig }>("save_gateway_config", args);
}

/** 启动网关（会把账号库导出为网关凭证）。传 port 可一步指定端口并保存。 */
export function startGateway(port?: number): Promise<GatewayStartResult> {
  return call<GatewayStartResult>("start_gateway", port ? { port } : {});
}

/** 检测端口是否可用；被占用时返回建议端口。 */
export function checkGatewayPort(port: number): Promise<GatewayPortCheck> {
  return call<GatewayPortCheck>("check_gateway_port", { port });
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
 */
export function switchGatewayMode(
  mode: GatewayMode,
  pinnedUid?: string | null,
): Promise<GatewayModeSwitchResult> {
  return call<GatewayModeSwitchResult>("switch_gateway_mode", {
    mode,
    pinnedUid: pinnedUid ?? null,
  });
}

/**
 * 设置「单一模型 + 积分轮转」的目标模型；传空串清除锁定。
 *
 * 网关运行时后端会自动重启它以生效（模型锁定由网关启动时读取）。
 */
export function setAllowedModel(model: string): Promise<GatewayModeSwitchResult> {
  return call<GatewayModeSwitchResult>("set_allowed_model", { model });
}

// ---------------------------------------------------------------------------
// 一键导入：接入本机 AI 客户端
// ---------------------------------------------------------------------------

/**
 * 获取网关可用模型列表。
 *
 * **只返回上游实时清单**：网关侧不再有内置静态表，拉不到就明确失败。
 * `error` 非空时 `models` 必为空数组 —— 调用方必须把 `error` 显示出来，
 * 不要渲染成「空的模型选择器」，否则用户分不清是没账号、代理不通还是真没模型。
 */
export async function getGatewayModels(): Promise<GatewayModelsResult> {
  const res = await call<{ models?: GatewayModelItem[]; error?: string | null }>("get_gateway_models");
  return { models: res.models ?? [], error: res.error ?? null };
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

// ---------------------------------------------------------------------------
// 本机 AI CLI 登录额度查询
// ---------------------------------------------------------------------------

/**
 * 查询全部 provider 的本机账号额度。
 *
 * `refresh` 缺省为 false：返回上次结果（含磁盘缓存），避免每次打开页面都打
 * 上游（5 个 provider 就是 5 次网络往返）。用户点「刷新」时才传 true。
 */
export function getCliQuotas(refresh = false): Promise<{ accounts: CliQuotaAccount[] }> {
  return call<{ accounts: CliQuotaAccount[] }>("get_cli_quotas", { refresh });
}

/** 刷新单个 provider 的额度；未登录时返回 `{ok:false, error}` 而不是抛错。 */
export function refreshCliQuota(
  provider: CliQuotaProvider,
): Promise<{ ok: boolean; account?: CliQuotaAccount; error?: string }> {
  return call<{ ok: boolean; account?: CliQuotaAccount; error?: string }>("refresh_cli_quota", {
    provider,
  });
}

/** 探测本机各 CLI 的登录态（不触网，毫秒级）。 */
export function getCliQuotaStatus(): Promise<{ providers: CliQuotaStatusItem[] }> {
  return call<{ providers: CliQuotaStatusItem[] }>("get_cli_quota_status");
}

/** 探测本机 AI 客户端（全部 12 类智能体）的安装与配置状态。 */
export function detectAgentClients(): Promise<AgentDetectionResult> {
  return call<AgentDetectionResult>("detect_agent_clients");
}

/** 导入模型：可只给 id，也可携带上游真实上下文窗口。 */
export interface AgentImportModel {
  id: string;
  /** 上游声明的真实上下文窗口（token）；未知时省略，不得编造。 */
  context_window?: number;
}

/**
 * 将本网关配置一键接入指定的客户端（支持单模型或多选模型）。
 *
 * 同时兼容 `string[]`（上下文窗口未知）与 `AgentImportModel[]`
 * （携带 `context_window`，Codex 等客户端据此显示正确的上下文容量）。
 */
export function importAgentClient(
  target: string,
  models?: Array<string | AgentImportModel> | AgentImportModel | string,
): Promise<AgentImportResult> {
  const modelList = Array.isArray(models)
    ? models
        .map((m) =>
          typeof m === "string" ? { id: m.trim() } : { ...m, id: m.id?.trim() },
        )
        .filter((m) => Boolean(m.id))
    : typeof models === "string" && models.trim()
      ? [{ id: models.trim() }]
      : models && typeof models === "object" && models.id?.trim()
        ? [{ ...models, id: models.id.trim() }]
        : undefined;
  return call<AgentImportResult>("import_agent_client", {
    target,
    models: modelList,
    model: modelList?.[0]?.id,
  });
}

/** 批量一键接入/更新多个客户端。若不传 targets，则自动更新所有已检测到安装的客户端。 */
export function batchImportAgentClients(
  targets?: string[],
  models?: Array<string | AgentImportModel>,
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
  /** 客户端版本号（探测不到为 null，不回退成本应用版本）。 */
  version?: string | null;
  /** 版本号的读取来源路径（排障用）。 */
  versionSource?: string | null;
  /** 应用设置的落盘路径。 */
  settingsFile?: string;
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
  /** 快照占用的总字节数（递归统计）。 */
  sizeBytes: number;
  /** 快照内的文件数。 */
  fileCount: number;
}

/** 列出某应用的登录态快照。 */
export function listSnapshots(targetApp: string): Promise<{
  snapshots: SnapshotItem[];
  currentUserId: string;
  /**
   * 标记是否可能已过期 —— 现场文件比标记更新，说明用户可能直接在客户端里换过号。
   * 为真时界面应提示「当前登录」徽章可能不准。
   */
  markerStale: boolean;
  /** 解析出的当前 uid（无标记时为空串）。 */
  resolvedUserId: string;
}> {
  return call<{
    snapshots: SnapshotItem[];
    currentUserId: string;
    markerStale: boolean;
    resolvedUserId: string;
  }>("list_snapshots", {
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
  /** refresh token 过期时间（Unix 秒）。 */
  refreshTokenExpiresAt?: number | null;
  /** 上次刷新成功时间。 */
  refreshedAt?: string | null;
  /** 是否具备自动续期条件（有 refresh token 且未失效）。 */
  canRefresh?: boolean;
  deviceIdMasked: string;
  cooldown?: { type?: string; until?: number; reason?: string; error_count?: number };
  /** 套餐身份展示名（付费缓存命中时才有）。 */
  payIdentity?: string | null;
  /** 套餐到期时间。 */
  payExpireAt?: string | null;
  /** 积分总额缓存（`null` = 未查询过，不等同于 0）。 */
  creditsTotal?: number | null;
  creditsUpdatedAt?: string | null;
  /** 所属分组 id（未分组为 null）。 */
  group?: string | null;
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

/** Trae 账号的编辑入参（只填要改的字段）。 */
export interface TraeAccountPatch {
  userId: string;
  name?: string;
  jwt?: string;
  refreshToken?: string;
}

/**
 * 编辑 Trae 账号。
 *
 * 只改昵称时不要传 `jwt` —— 传空串会被后端忽略，但传错账号的 JWT 会被拒绝，
 * 两者都不该发生；干净的做法是让调用方只传真正要改的字段。
 */
export function traeUpdateAccount(
  patch: TraeAccountPatch,
): Promise<{ ok: boolean; account: TraeAccountMeta }> {
  return call<{ ok: boolean; account: TraeAccountMeta }>("trae_update_account", {
    userId: patch.userId,
    name: patch.name ?? null,
    jwt: patch.jwt ?? null,
    refreshToken: patch.refreshToken ?? null,
  });
}

/** 读取账号的完整 JWT（仅查看/编辑弹窗回填用）。 */
export function traeAccountJwt(userId: string): Promise<{ userId: string; jwt: string }> {
  return call<{ userId: string; jwt: string }>("trae_account_jwt", { userId });
}

/** JWT 解析结果（`valid: false` 时其余字段为 null，不抛错）。 */
export interface TraeJwtInfo {
  valid: boolean;
  userId: string | null;
  expHours: number | null;
  expTimestamp: number | null;
  status: string;
}

/** 解析一段 JWT（编辑弹窗实时预览，不落库）。 */
export function traeJwtParse(jwt: string): Promise<TraeJwtInfo> {
  return call<TraeJwtInfo>("trae_jwt_parse", { jwt });
}

/** 清空全部账号的签到冷却。 */
export function traeClearAllCooldowns(): Promise<{ ok: boolean; cleared: number }> {
  return call<{ ok: boolean; cleared: number }>("trae_clear_all_cooldowns");
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

/** 一次本机登录态导入的结果计数（见 Rust 侧 CaptureSummary）。 */
export interface TraeImportSummary {
  /** 新入库的账号数。 */
  appended: number;
  /** 凭证变新而更新的账号数。 */
  updated: number;
  /** 因本机凭证比账号库更旧、被防降级拦下的次数。 */
  skipped: number;
  /** appended + updated。 */
  total: number;
  /** 读取本机登录态失败时的原因（此时各计数均为 0）。 */
  error?: string;
}

/** 发现本机登录过的 Trae 账号；顺带把本机登录态里的 JWT 一并导入账号库。 */
export function traeDiscoverAndImport(): Promise<{
  accounts: TraeDiscoveredAccount[];
  import: TraeImportSummary;
  apps: { kind: string; label: string }[];
}> {
  return call<{
    accounts: TraeDiscoveredAccount[];
    import: TraeImportSummary;
    apps: { kind: string; label: string }[];
  }>("trae_discover_and_import");
}

/** 单独从本机登录态导入 Trae 账号（不需要客户端在运行，也无需开代理/装 CA）。 */
export function traeImportLocal(): Promise<{
  ok: boolean;
  import: TraeImportSummary;
  accounts: TraeAccountMeta[];
}> {
  return call<{ ok: boolean; import: TraeImportSummary; accounts: TraeAccountMeta[] }>(
    "trae_import_local",
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

/** 单账号 JWT 刷新结果。 */
export interface TraeRefreshResult {
  userId: string;
  ok: boolean;
  message: string;
  /** 刷新后 JWT 的剩余有效小时数（`-1` 表示解析不出）。 */
  expHours?: number;
}

/** 刷新某账号的 Trae JWT（`force` 跳过惰性门强制刷新）。 */
export function traeRefreshAccount(
  userId: string,
  force = false,
): Promise<TraeRefreshResult> {
  return call<TraeRefreshResult>("trae_refresh_account", { userId, force });
}

/** 批量刷新全部 Trae 账号的 JWT。 */
export function traeRefreshAll(): Promise<{
  ok: number;
  failed: number;
  total: number;
  results: TraeRefreshResult[];
}> {
  return call<{ ok: number; failed: number; total: number; results: TraeRefreshResult[] }>(
    "trae_refresh_all",
  );
}

/** 单个权益包。 */
export interface TraeCreditPack {
  name: string;
  remaining: number;
  total: number;
  expireAt: string | null;
}

/** 三条积分账（IDE 积分 / 权益包 / 付费身份）。 */
export interface TraeCreditDetail {
  userId: string;
  /** IDE 侧可用积分总数。 */
  total: number | null;
  packs: TraeCreditPack[];
  payIdentity: string | null;
  payExpireAt: string | null;
  /** 逐来源错误（部分失败时仍返回已拿到的数据）。 */
  errors: string[];
}

/** 读取某账号的三条积分账。 */
export function traeCreditDetail(userId: string): Promise<TraeCreditDetail> {
  return call<TraeCreditDetail>("trae_credit_detail", { userId });
}

/** 刷新全部账号的付费身份缓存。 */
export function traeRefreshPayStatus(): Promise<{ ok: number; cache: unknown }> {
  return call<{ ok: number; cache: unknown }>("trae_refresh_pay_status");
}

/** 读取付费身份缓存（不发网络请求）。 */
export function traePayStatusCache(): Promise<unknown> {
  return call<unknown>("trae_pay_status_cache");
}

// ---------------------------------------------------------------------------
// 账号分组（Trae / 豆包 分域）
// ---------------------------------------------------------------------------

/** 分组所属应用。 */
export type GroupApp = "Trae" | "Doubao";

/** 一个分组及其成员。 */
export interface AccountGroup {
  id: string;
  name: string;
  color: string;
  order: number;
  count: number;
  uids: string[];
}

/** 分组视图。 */
export interface GroupsView {
  app: GroupApp;
  groups: AccountGroup[];
  /** uid → groupId（只含仍然存在的分组）。 */
  membership: Record<string, string>;
}

/** 某应用的分组列表。 */
export function groupsList(app: GroupApp): Promise<GroupsView> {
  return call<GroupsView>("groups_list", { app });
}

/** 新建分组。 */
export function groupCreate(
  app: GroupApp,
  name: string,
  color = "slate",
): Promise<{ ok: boolean; id: string; groups: GroupsView }> {
  return call<{ ok: boolean; id: string; groups: GroupsView }>("group_create", {
    app,
    name,
    color,
  });
}

/** 更新分组（只改传入的字段）。 */
export function groupUpdate(args: {
  app: GroupApp;
  id: string;
  name?: string;
  color?: string;
  order?: number;
}): Promise<{ ok: boolean; groups: GroupsView }> {
  return call<{ ok: boolean; groups: GroupsView }>("group_update", {
    app: args.app,
    id: args.id,
    name: args.name ?? null,
    color: args.color ?? null,
    order: args.order ?? null,
  });
}

/** 删除分组（连带清掉成员映射）。 */
export function groupDelete(
  app: GroupApp,
  id: string,
): Promise<{ ok: boolean; groups: GroupsView }> {
  return call<{ ok: boolean; groups: GroupsView }>("group_delete", { app, id });
}

/** 把账号移入/移出分组（`groupId` 为空 = 移出）。 */
export function groupMove(
  app: GroupApp,
  userId: string,
  groupId: string | null,
): Promise<{ ok: boolean; groups: GroupsView }> {
  return call<{ ok: boolean; groups: GroupsView }>("group_move", {
    app,
    userId,
    groupId,
  });
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
  /** 到期分层：界面据此上色（fresh 常态 / soon 黄 / expired 红）。 */
  expiryTier?: "unknown" | "fresh" | "soon" | "expired";
  /** 距到期天数（向上取整；已过期为负数；无凭证为 null）。 */
  daysLeft?: number | null;
  sessionSource: string | null;
  cookiesSyncedAt: string | null;
  lastRenewAt: string | null;
  quotaLevel: string | null;
  quotaExpireAt: string | null;
  quotaSummary: string | null;
  quotaCheckedAt: string | null;
  /** 当前时段额度已用百分比（无缓存/无该窗口时为 null）。 */
  quotaUsedPercent?: number | null;
  // ---- 快照统计（由 doubao_list_accounts 合并） ----
  hasSnapshot?: boolean;
  sizeBytes?: number;
  fileCount?: number;
  /** Unix 秒；无快照为 null。 */
  lastModified?: number | null;
  /** 是否为当前登录账号。 */
  isCurrent?: boolean;
  /** 只有快照、账号池里已无该条目。 */
  orphanSnapshot?: boolean;
}

/** 豆包账号列表。 */
export function doubaoListAccounts(): Promise<{
  accounts: DoubaoAccountView[];
  lastKeepaliveAt: string | null;
  currentUserId?: string;
}> {
  return call<{
    accounts: DoubaoAccountView[];
    lastKeepaliveAt: string | null;
    currentUserId?: string;
  }>("doubao_list_accounts");
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

/** 删除豆包账号（`deleteSnapshot` 默认 true，避免留下孤儿快照）。 */
export function doubaoDeleteAccount(
  userId: string,
  deleteSnapshot = true,
): Promise<{ ok: boolean; snapshotRemoved?: boolean }> {
  return call<{ ok: boolean; snapshotRemoved?: boolean }>("doubao_delete_account", {
    userId,
    deleteSnapshot,
  });
}

/** 当前登录 uid 的探测结果（含各来源明细，便于排查为什么探测不到）。 */
export function doubaoDetectUid(): Promise<{
  uid: string | null;
  sources: { source: string; uid: string }[];
}> {
  return call<{ uid: string | null; sources: { source: string; uid: string }[] }>(
    "doubao_detect_uid",
  );
}

/** 快照版本元数据（`schemaVersion: 0` 表示无 meta 文件的旧快照）。 */
export interface DoubaoSnapshotMeta {
  userId: string;
  schemaVersion: number;
  createdAt: string | null;
  chromiumVersion: string | null;
  includeIndexedDB: boolean;
  hasMeta: boolean;
}

/** 读取账号快照的版本元数据。 */
export function doubaoSnapshotMeta(userId: string): Promise<DoubaoSnapshotMeta> {
  return call<DoubaoSnapshotMeta>("doubao_snapshot_meta", { userId });
}

/** 一键以该账号打开豆包客户端（恢复快照 → 拉起客户端）。 */
export function doubaoOpenAsAccount(
  userId: string,
  proxyPort?: number,
): Promise<{ ok: boolean; message: string; error: string; steps: unknown[] }> {
  return call<{ ok: boolean; message: string; error: string; steps: unknown[] }>(
    "doubao_open_as_account",
    { userId, proxyPort: proxyPort ?? null },
  );
}

/** 豆包运维历史事件。 */
export interface DoubaoHealthEvent {
  at: string;
  kind: "keepalive" | "renew" | "quota";
  ok: boolean;
  uid?: string | null;
  level?: string | null;
  summary?: string;
  windows?: { name?: string; usedPercent?: number; exhausted?: boolean; resetAt?: string }[];
}

/** 豆包运维健康史的完整返回体。 */
export interface DoubaoHistory {
  events: DoubaoHealthEvent[];
  trend: { date: string; usedPercent: number; level: string | null; uid: string | null }[];
  health: {
    days: number;
    keepalive: number;
    renew: number;
    quota: number;
    ok: number;
    failed: number;
    lastKeepaliveAt: string | null;
  };
  requestedDays: number;
}

/** 豆包运维健康史（事件 + 14 天额度趋势 + 7 天健康计数）。 */
export function doubaoHistory(days = 30): Promise<DoubaoHistory> {
  return call<DoubaoHistory>("doubao_history", { days });
}

/** 豆包设置。 */
export interface DoubaoAppSettings {
  /** 快照是否纳入 IndexedDB（体积大一个量级，默认关）。 */
  doubao_snapshot_include_idb: boolean;
}

/** 读取豆包设置。 */
export function doubaoGetSettings(): Promise<DoubaoAppSettings> {
  return call<DoubaoAppSettings>("doubao_settings");
}

/** 写入一项豆包设置。 */
export function doubaoSetSetting<K extends keyof DoubaoAppSettings>(
  key: K,
  value: DoubaoAppSettings[K],
): Promise<DoubaoAppSettings> {
  return call<DoubaoAppSettings>("doubao_set_setting", { key, value });
}

// ---------------------------------------------------------------------------
// Trae OAuth 授权码登录
// ---------------------------------------------------------------------------

/** 授权 URL 响应。 */
export interface TraeOAuthLoginUrl {
  url: string;
  state: string;
  redirectUri: string;
  port: number;
}

/** 登录完成事件负载（桌面宿主通过 `oauth-login-done` 推送）。 */
export interface TraeOAuthDoneEvent {
  ok: boolean;
  message: string;
  userId: string | null;
}

/**
 * 签发 OAuth 授权 URL。
 *
 * 调用后应立刻在浏览器打开 `url`（桌面宿主还会先启动 17388 回环监听）。
 */
export function oauthLoginUrl(accountName?: string): Promise<TraeOAuthLoginUrl> {
  return call<TraeOAuthLoginUrl>("oauth_login_url", { accountName: accountName ?? null });
}

/**
 * 启动本机回环监听器（桌面专属）。
 *
 * 端口被占用时抛错 —— 此时界面应降级为「手动粘贴回调地址」，
 * 而不是让用户对着一个永远等不到回调的弹窗发呆。
 */
export function oauthStartLoopback(accountName?: string): Promise<{
  ok: boolean;
  port: number;
  redirectUri: string;
  idleTimeoutSecs: number;
}> {
  return call("oauth_start_loopback", { accountName: accountName ?? null });
}

/** 停止本机回环监听器（桌面专属；未启动时幂等成功）。 */
export function oauthStopLoopback(): Promise<{ ok: boolean }> {
  return call("oauth_stop_loopback");
}

/** 放弃当前登录会话。 */
export function oauthCancel(): Promise<{ ok: boolean }> {
  return call("oauth_cancel");
}

/**
 * 当前是否仍在等待 OAuth 回调（桌面专属）。
 *
 * 界面重开弹窗时用它判断「后端是否还有一个未完成的会话」——
 * 直接假定没有会让用户重开后拿到一个 state 已经对不上的登录。
 */
export function oauthPending(): Promise<{ pending: boolean }> {
  return call("oauth_pending");
}

/**
 * 手动提交浏览器回调 URL（兜底路径）。
 *
 * 端口被占用、自动收尾失败、或用户在别的浏览器里完成授权时都靠这条路径。
 */
export function oauthSubmitCallback(
  callbackUrl: string,
  accountName?: string,
): Promise<{ ok: boolean; userId: string; message: string }> {
  return call("oauth_submit_callback", { callbackUrl, accountName: accountName ?? null });
}

// ---------------------------------------------------------------------------
// 应用内调度
// ---------------------------------------------------------------------------

/** 应用内调度的一项任务。 */
export interface InAppScheduleTask {
  kind: string;
  label: string;
  /** 触发时间 `HH:MM`。 */
  at: string;
  /** 最近一次成功执行的日期（`YYYY-MM-DD`），从未跑过为 null。 */
  lastRunDay: string | null;
}

/** 应用内调度的任务清单。 */
export function inAppScheduleView(): Promise<{ tasks: InAppScheduleTask[] }> {
  return call<{ tasks: InAppScheduleTask[] }>("in_app_schedule_view");
}

/** 手动触发一轮应用内调度。 */
export function runInAppDueTasks(): Promise<{
  ran: { kind: string; label: string; scheduledAt: string; ok: boolean; exitCode: number }[];
  checkedAt: string;
}> {
  return call("run_in_app_due_tasks");
}

// ---------------------------------------------------------------------------
// Trae 账号导出 / 导入
// ---------------------------------------------------------------------------

/** Trae 账号导出结果（`text` 含明文 JWT 与 refresh_token）。 */
export interface TraeExportResult {
  ok: boolean;
  count: number;
  text: string;
}

/**
 * 导出 Trae 账号。
 *
 * 不传 `userIds` 即导出全部。返回的 `text` 含**明文凭证**，
 * 界面必须明确提示用户妥善保管。
 */
export function traeExportAccounts(userIds?: string[]): Promise<TraeExportResult> {
  return call<TraeExportResult>("trae_export_accounts", { userIds: userIds ?? null });
}

/** 导入预览里的一项账号。 */
export interface TraeImportPreviewItem {
  userId: string;
  name: string;
  hasRefreshToken: boolean;
  /** 账号库里已有同 uid —— 导入会覆盖它。 */
  willOverwrite: boolean;
}

/** 预览 Trae 账号导入文件（不落库）。 */
export function traePreviewImport(fileText: string): Promise<{
  ok: boolean;
  count: number;
  exportedAt: string | null;
  accounts: TraeImportPreviewItem[];
}> {
  return call("trae_preview_import", { fileText });
}

/** 导入 Trae 账号（同 uid 覆盖，其余追加）。 */
export function traeImportAccounts(fileText: string): Promise<{
  ok: boolean;
  added: number;
  updated: number;
  skipped: number;
}> {
  return call("trae_import_accounts", { fileText });
}

// ---------------------------------------------------------------------------
// Trae 积分趋势
// ---------------------------------------------------------------------------

/** 一天的积分汇总（`null` = 这天没有采到数据，与「积分归零」不同）。 */
export interface TraeCreditsDay {
  date: string;
  total: number | null;
  consumed: number | null;
}

/** 单个账号的最新积分快照。 */
export interface TraeCreditsAccount {
  userId: string;
  name: string;
  credits: number | null;
  updatedAt: string | null;
}

/** 积分趋势统计。 */
export interface TraeCreditsStats {
  days: number;
  daily: TraeCreditsDay[];
  summary: {
    latestTotal: number | null;
    firstTotal: number | null;
    /** 区间内消耗合计（仅统计有数据的天）。 */
    consumed: number | null;
    /** 实际采到数据的天数 —— 据此判断趋势可不可信。 */
    observedDays: number;
    /** 首末总量差（`null` = 数据不足）。 */
    change: number | null;
  };
  accounts: TraeCreditsAccount[];
}

/** 积分趋势统计（`daily` 按天补齐，缺天为 null）。 */
export function traeCreditsStats(days = 30): Promise<TraeCreditsStats> {
  return call<TraeCreditsStats>("trae_credits_stats", { days });
}

// ---------------------------------------------------------------------------
// 客户端启动
// ---------------------------------------------------------------------------

/**
 * 拉起客户端（**不**切账号、不备份、不关进程）。
 *
 * 与 `switchAccount` 的区别是零副作用：不动任何快照槽。
 * 用户说「打开 Trae」时该走这里，而不是走切换。
 */
export function appLaunch(
  targetApp: string,
  proxyPort?: number,
): Promise<{ ok: boolean; message: string }> {
  return call<{ ok: boolean; message: string }>("app_launch", {
    targetApp,
    proxyPort: proxyPort ?? null,
  });
}

// ---------------------------------------------------------------------------
// 抓包日志查看
// ---------------------------------------------------------------------------

/** 抓包日志的一条摘要（列表用，不含正文）。 */
export interface ProxyLogEntry {
  /** `文件名:条目序号`，详情接口按它定位。 */
  id: string;
  timestamp: string;
  /** `HTTP GET` / `WebSocket` 这类展示用形态。 */
  method: string;
  host: string;
  path: string;
  /** 响应状态；取不到时为 `-`。 */
  status: string;
  size: number;
  /** SSE 汇总里的模型名（非流式请求没有这个字段）。 */
  sseModel?: string;
  /** SSE token 用量摘要，形如 `p:120 c:340 t:460`。 */
  sseTokens?: string;
}

export interface ProxyLogList {
  entries: ProxyLogEntry[];
  /** 过滤后的**总数**（不是本页条数），用于分页。 */
  total: number;
}

/** 抓包日志目录概况。 */
export interface ProxyLogsOverview {
  dir: string;
  fileCount: number;
  totalBytes: number;
  oldest: string | null;
  newest: string | null;
}

export interface ProxyLogQuery {
  keyword?: string;
  /** `YYYY-MM-DD HH:MM:SS`，含端点。 */
  startTime?: string;
  endTime?: string;
  offset?: number;
  limit?: number;
}

/** 列出抓包日志条目（时间倒序，新的在前）。 */
export function proxyLogsList(query: ProxyLogQuery = {}): Promise<ProxyLogList> {
  return call<ProxyLogList>("proxy_logs_list", {
    keyword: query.keyword ?? null,
    startTime: query.startTime ?? null,
    endTime: query.endTime ?? null,
    offset: query.offset ?? 0,
    limit: query.limit ?? 50,
  });
}

/** 取单条日志的完整正文。 */
export async function proxyLogDetail(id: string): Promise<string> {
  const res = await call<{ content: string }>("proxy_log_detail", { id });
  return res.content;
}

/** 日志目录概况（文件数 / 体积 / 日期范围）。 */
export function proxyLogsOverview(): Promise<ProxyLogsOverview> {
  return call<ProxyLogsOverview>("proxy_logs_overview");
}

/**
 * 删除抓包日志。
 *
 * `keepDays` 有值时只删该天数以前的；不传则全删。
 */
export function proxyLogsClear(keepDays?: number): Promise<{ removed: number }> {
  return call<{ removed: number }>("proxy_logs_clear", {
    keepDays: keepDays ?? null,
  });
}

// ---------------------------------------------------------------------------
// Trae 签到趋势
// ---------------------------------------------------------------------------

/** 一天的签到汇总。 */
export interface TraeCheckinTrendPoint {
  date: string;
  ok: number;
  already: number;
  failed: number;
  total: number;
  /** 成功率百分比；「已签到」算成功。 */
  successRate: number | null;
}

/** 签到成功率趋势。 */
export interface TraeCheckinTrends {
  days: number;
  /** 只含实际有记录的日期（不补 0 高度的柱子）。 */
  points: TraeCheckinTrendPoint[];
  summary: {
    ok: number;
    already: number;
    failed: number;
    total: number;
    observedDays: number;
    /** 区间成功率；无样本时为 `null`（不是 0）。 */
    successRate: number | null;
  };
}

/** 签到成功率趋势（同 uid 同天只记最终态）。 */
export function traeCheckinTrends(days = 30): Promise<TraeCheckinTrends> {
  return call<TraeCheckinTrends>("trae_checkin_trends", { days });
}

/** 单日积分消耗（官方会话级用量聚合）。 */
export interface TraeUsageDay {
  date: string;
  credits: number;
  sessions: number;
  /** 模型 → 当日扣费。 */
  models: Record<string, number>;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
}

/** 单个账号的消耗历史。 */
export interface TraeUsageAccount {
  userId: string;
  name: string;
  ok: boolean;
  /** 拉取失败时的说明（有缓存时为「展示的是缓存」）。 */
  error: string | null;
  daily: TraeUsageDay[];
  totalCredits: number;
  sessions: number;
  /** 模型消耗排行（按扣费降序）。 */
  models: { model: string; credits: number }[];
}

/** 积分消耗历史。 */
export interface TraeUsageHistory {
  fetchedAt: number;
  /** true = 纯缓存读取，本次没有发起网络请求。 */
  cached: boolean;
  accounts: TraeUsageAccount[];
}

/**
 * 积分消耗历史（官方会话级用量）。
 *
 * 与 `traeCreditsStats` 的口径不同：那个是**余额差值**推算，会被签到补发干扰；
 * 这个是接口返回的**真实扣费**，能按模型拆分。两个都看才能分清「消耗」与「补发」。
 *
 * `fresh = false` 时零网络请求，只回缓存。
 */
export function traeUsageHistory(fresh = true): Promise<TraeUsageHistory> {
  return call<TraeUsageHistory>("trae_usage_history", { fresh });
}

/** 立即采样一次积分快照。 */
export function traeCreditsSnapshot(): Promise<{
  ok: boolean;
  sampled: number;
  failed: number;
  errors: string[];
}> {
  return call("trae_credits_snapshot");
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

/** HTTP 续期探活的结果。 */
export interface DoubaoRenewResult {
  ok: number;
  expired: number;
  skipped: number;
  errors: number;
  total: number;
  results: { userId: string; name: string; status: string; message: string }[];
  /** HTTP 全部失败时是否已回退到客户端保活。 */
  fallbackUsed?: boolean;
  /** 回退的说明文案（`fallbackUsed` 为真时才有）。 */
  fallbackMessage?: string;
}

/**
 * HTTP 续期探活。
 *
 * `fallbackToKeepalive` 为真时，若 HTTP 路径**全部**失败则自动回退到拉起客户端保活
 * —— 凭证池里的 sessionid 被客户端刷新过时，这是唯一还能救回会话的路子。
 */
export function doubaoRenew(
  syncOnly = false,
  fallbackToKeepalive = false,
): Promise<DoubaoRenewResult> {
  return call<DoubaoRenewResult>("doubao_renew", { syncOnly, fallbackToKeepalive });
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
