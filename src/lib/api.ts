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
  GatewayLogResult,
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
  PlatformModelsState,
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
 * - webui（浏览器）：HTTP fetch 调用本地 ai-gateway 服务（同源）
 */

/** ai-gateway 服务端（`crates/ai-gateway-server`）的默认监听端口。 */
const DEFAULT_API_PORT = 57890;
const DEFAULT_API_BASE = `http://127.0.0.1:${DEFAULT_API_PORT}`;

/**
 * webui 模式下的 HTTP 基址。
 *
 * 页面本身由 ai-gateway 服务内嵌提供（`api.rs` 用 RustEmbed 把 `dist/` 打进
 * 二进制，`GET /` 就是页面），所以**同源**才是正确基址：
 * `ai-gateway serve --port 57900` 时页面在 57900，接口也在 57900。
 *
 * 这里曾写死 `http://127.0.0.1:57890`（即上面的默认端口）。一旦用 `--port`
 * 换端口，页面照旧能打开，但所有数据请求都打到 57890 —— 界面只剩
 * 「无法读取网关状态 / 无法连接 ai-gateway 服务」，看着像后端坏了，实际是
 * 前端打错了地址。改为运行时取当前页面的源即可，默认端口只在拿不到源时兜底。
 *
 * 桌面端（Tauri）不会走这条分支：`call()` 先判 `isWebui()`，桌面端直接 invoke。
 * `file://` 预览、`about:blank` 等场景 `location.origin` 是 `"null"`，此时退回默认端口。
 *
 * 纯浏览器 `npm run dev`（Vite 开发服务器，14200）调试时源是 14200，可用
 * `VITE_API_BASE` 显式指定后端地址。
 */
function resolveApiBase(): string {
  const fromEnv = import.meta.env.VITE_API_BASE;
  if (typeof fromEnv === "string" && fromEnv.trim()) return fromEnv.trim().replace(/\/+$/, "");
  if (typeof window === "undefined") return DEFAULT_API_BASE;
  const origin = window.location?.origin;
  if (!origin || origin === "null" || !/^https?:\/\//i.test(origin)) return DEFAULT_API_BASE;
  return origin.replace(/\/+$/, "");
}

const API_BASE = resolveApiBase();

const DEMO_READ_COMMANDS = new Set([
  "get_status", "get_accounts", "get_codebuddy_cli_status", "get_codebuddy_cn_ide_status", "get_checkin_status",
  "get_credit_expiry", "get_credit_statistics", "get_auto_checkin_config",
  "get_token_statistics",
  "get_checkin_logs", "get_auto_rotate_config", "rotate_status", "get_rotate_logs",
  "get_github_config", "check_update", "get_launch_at_login_enabled", "switch_progress",
  "get_travel_status", "get_auto_travel_config",
  "get_gateway_usage",
  // ⚠ Qoder / ZCode 的页面数据（2026-09-22 补登记）。
  //
  // `screenshot-demo.ts` 里**早就写好**了这些命令的假数据（11 个 case），
  // 但白名单里一个都没登记 —— 于是演示模式下打开这两页，
  // `call()` 在 `!DEMO_READ_COMMANDS.has(cmd)` 那一行直接抛
  // 「演示模式下不可操作」⇒ **页面空白、看不到卡片样式**。
  //
  // 所有者反馈：「你这个演示模式,没加zcode和qoder账号,我看不到样式」。
  //
  // 教训：新增 demo 数据时**两处都要改**（数据 + 白名单）。
  // 漏登记是静默的 —— 数据在、但永远读不到，看起来像"数据没写"。
  "qoder_list_accounts", "qoder_summary", "qoder_campaigns", "qoder_campaigns_all",
  "zcode_list_accounts", "zcode_summary",
  "zcode_scan_local",
  // 网关配置（Qoder/ZCode 的「配置」弹窗要读它才显示自动领取开关）
  "get_gateway_config", "get_gateway_status",
  // 三平台「配置」弹窗里的「可用的模型」表单要读它
  "get_platform_models",
  // 统一账号页的「参加兼容网关路由」开关要读它（只读）。
  // 写开关的 set_platform_enabled 刻意不登记 —— 演示模式不该产生副作用，
  // 与 refresh_cli_quota / trae_checkin_refresh 同理。
  // ⚠ 登记即须在 screenshot-demo.ts 配 case，两处齐改。
  "get_platform_enabled",
  // 本机 AI CLI 额度查询页（额度查询）
  //
  // 只登记两条**只读**命令：`get_cli_quotas`（含磁盘缓存）与
  // `get_cli_quota_status`（纯本地读凭证、不触网）。
  // `refresh_cli_quota` **刻意不登记** —— 它会对上游发真实请求，
  // 演示模式下不该产生任何外部副作用。
  "get_cli_quotas", "get_cli_quota_status",
  // Trae 分组 / 积分 / 签到趋势 / 用量、抓包日志（与 PR#2 的登记范围逐条对齐）
  //
  // ⚠ 登记白名单与写演示数据**必须同时做**。只登记不写数据时，`call()`
  // 放行后落到 `screenshotDemoResponse` 的 `default` 分支抛
  // 「演示模式缺少只读数据」—— 页面空白，看起来像"功能没做"。
  // 这个坑本项目已经踩过一次（见上面 Qoder/ZCode 那段注释）。
  //
  // 只登记**读取类**：`group_create/update/delete`、`oauth_*`、
  // `proxy_logs_clear`、`trae_credits_snapshot`、`trae_export/import/preview`
  // 一律不登记 —— 它们是写操作或会返回明文凭证，演示模式不该"看起来能跑"。
  "groups_list",
  "trae_credit_detail", "trae_credits_stats", "trae_checkin_trends", "trae_checkin_status",
  "trae_usage_history",
  // `trae_checkin_refresh` **刻意不登记** —— 它会逐账号探测上游 status 端点，
  // 演示模式下不该产生任何外部请求（与 `trae_credits_snapshot` 同理）。
  "proxy_logs_list", "proxy_log_detail", "proxy_logs_overview",
  // 豆包运维健康史（DoubaoInsights 的数据源）。只读，登记即须配套 demo 数据。
  "doubao_history",
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
  get_credit_expiry_batch: { method: "GET", path: "/api/credits/batch" },
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
  // 「平台 × 区域 → 允许的模型」白名单（方向与上一条相反）。
  //
  // 读走 GET、写走 POST，同一条路径 —— 与 Rust 侧
  // `api_get_platform_models` / `api_set_platform_models` 对应。
  // 同样**必须**注册：漏了在 webui 下是静默失败（详见上一条注释）。
  get_platform_models: { method: "GET", path: "/api/gateway/platform-models" },
  set_platform_models: { method: "POST", path: "/api/gateway/platform-models" },
  // 「平台 → 是否参加兼容网关路由」开关（统一账号页）。
  //
  // ⚠ 同样必须注册：漏了在 webui 下是静默失败 —— 点平台开关毫无反应、
  // 不报错、也没有请求发出（与上面两条同型，本项目踩过）。
  //
  // 语义（业主 m01323）：关闭平台只是不参与兼容网关，并不是不执行任务
  // ⇒ 关掉的平台不参与选号，但签到 / 上报 / 成长任务 / 额度巡检照跑。
  // 故它不是账号级的 disabled（那个是「账号已死」），网关侧另有配置键。
  get_platform_enabled: { method: "GET", path: "/api/gateway/platform-enabled" },
  set_platform_enabled: { method: "POST", path: "/api/gateway/platform-enabled" },
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
  // 手机号 + 短信验证码登录（2026-09-22 新增）。
  //
  // ⚠ 路径与网关侧的 `/login/sms/*` 不同：这里多一层 `/api/gateway/`
  // 前缀，那是**宿主 WebUI** 的命名空间（它再转发给网关）。
  // 直接抄 `/login/sms/send` 在 webui 模式下会 404。
  sms_send: { method: "POST", path: "/api/gateway/sms/send" },
  sms_verify: { method: "POST", path: "/api/gateway/sms/verify" },
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
  // Trae 账号的部分更新（启用 / 禁用；后端也支持改名与换 JWT）。
  //
  // 与 add 分开是刻意的：add 必须带 JWT 才能新建，而「启用 / 禁用」
  // 只改一个布尔；复用 add 会把 JWT 语义搅进来。
  trae_update_account: { method: "POST", path: "/api/trae/accounts/update" },
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
  // 豆包运维健康史（事件 + 额度趋势 + 健康计数）。只读，无副作用。
  doubao_history: { method: "GET", path: "/api/doubao/history" },
  get_app_settings: { method: "GET", path: "/api/apps/settings" },
  // ---- 本地 MITM 代理 ----
  proxy_config: { method: "GET", path: "/api/proxy/config" },
  proxy_status: { method: "GET", path: "/api/proxy/status" },
  proxy_check: { method: "GET", path: "/api/proxy/check" },
  proxy_start: { method: "POST", path: "/api/proxy/start" },
  proxy_stop: { method: "POST", path: "/api/proxy/stop" },
  proxy_cert_status: { method: "GET", path: "/api/proxy/cert" },
  proxy_cert_generate: { method: "POST", path: "/api/proxy/cert/generate" },
  proxy_capture_local: { method: "POST", path: "/api/proxy/capture-local" },
  proxy_cleanup_stale: { method: "POST", path: "/api/proxy/cleanup" },
  proxy_parse_upstream: { method: "POST", path: "/api/proxy/parse-upstream" },
  // 上游代理连通性自检（2026-09-22 新增）。
  // 所有者：「设置里面的配置代理，也没有测试按钮 可以用来测试是否连通」
  proxy_test_upstream: { method: "POST", path: "/api/proxy/test-upstream" },
  // 本机代理**节点**探测（2026-09-23 新增）。
  // 所有者：「探测本机配置的代理的速度那个更快更好」
  // ⚠ 仅桌面端可用：要读本机代理软件的控制接口（命名管道 / unix socket）。
  proxy_probe_nodes: { method: "POST", path: "/api/proxy/probe-nodes" },
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
  // 激活（写操作）：报官方激活事件 + 领上游给出的套餐。
  // ⚠ 只在用户点按钮时调用，**不进任何轮询** —— 理由见 zcodeWarmAccount 的注释。
  // 宿主启动时的自动激活不走这里（它直接调后端），故这里保持纯手动。
  zcode_warm_account: { method: "POST", path: "/api/zcode/warm-account" },
  qoder_refresh_account: { method: "POST", path: "/api/qoder/refresh-account" },
  qoder_campaigns: { method: "POST", path: "/api/qoder/campaigns" },
  // 批量：所有账号的活动 / 一键领取（活动是每账号专属的）
  qoder_campaigns_all: { method: "POST", path: "/api/qoder/campaigns-all" },
  qoder_claim_all_campaigns: { method: "POST", path: "/api/qoder/claim-all-campaigns" },
  qoder_claim_campaign: { method: "POST", path: "/api/qoder/claim-campaign" },
  zcode_login_start: { method: "POST", path: "/api/zcode/login/start" },
  zcode_login_poll: { method: "POST", path: "/api/zcode/login/poll" },
  // 本机 AI CLI 额度查询（与 PR#2 的 `src/api.rs` 路由逐条对齐）
  get_cli_quotas: { method: "GET", path: "/api/cli-quota" },
  refresh_cli_quota: { method: "POST", path: "/api/cli-quota/refresh" },
  get_cli_quota_status: { method: "GET", path: "/api/cli-quota/status" },
  // ---- Trae 账号分组 ----
  groups_list: { method: "GET", path: "/api/groups" },
  group_create: { method: "POST", path: "/api/groups/create" },
  group_update: { method: "POST", path: "/api/groups/update" },
  group_delete: { method: "POST", path: "/api/groups/delete" },
  group_move: { method: "POST", path: "/api/groups/move" },
  // ---- Trae OAuth 的三条手动路径 ----
  oauth_login_url: { method: "POST", path: "/api/trae/oauth/login-url" },
  oauth_submit_callback: { method: "POST", path: "/api/trae/oauth/callback" },
  oauth_cancel: { method: "POST", path: "/api/trae/oauth/cancel" },
  // ⚠ `oauth_start_loopback` / `oauth_stop_loopback` **刻意不登记**：
  // 它们是 Tauri 专属命令（本机回环监听器由 `src-tauri/src/oauth_loopback.rs`
  // 起 axum 服务收回调），上游 PR#2 也没有对应 HTTP 路由。
  // 登记了会让 webui 打到不存在的路径 —— 与 `set_model_platforms` 那种
  // "漏登记"是相反方向的坑。webui 下调用它们会在 `httpCall` 的 `!route`
  // 分支明确报错，这正是想要的行为（桌面专属能力就该在浏览器里说清）。
  // ---- Trae 积分 / 用量 / 签到趋势 ----
  trae_credit_detail: { method: "GET", path: "/api/trae/credits/detail" },
  trae_credits_stats: { method: "GET", path: "/api/trae/credits/stats" },
  trae_credits_snapshot: { method: "POST", path: "/api/trae/credits/snapshot" },
  trae_checkin_trends: { method: "GET", path: "/api/trae/checkin/trends" },
  trae_checkin_status: { method: "GET", path: "/api/trae/checkin/status" },
  trae_checkin_refresh: { method: "POST", path: "/api/trae/checkin/refresh" },
  trae_usage_history: { method: "GET", path: "/api/trae/usage-history" },
  // ---- Trae 账号导入导出 ----
  trae_export_accounts: { method: "POST", path: "/api/trae/export" },
  trae_preview_import: { method: "POST", path: "/api/trae/import/preview" },
  trae_import_accounts: { method: "POST", path: "/api/trae/import" },
  // ---- 抓包日志 ----
  proxy_logs_list: { method: "GET", path: "/api/proxy/logs" },
  proxy_log_detail: { method: "GET", path: "/api/proxy/logs/detail" },
  proxy_logs_overview: { method: "GET", path: "/api/proxy/logs/overview" },
  proxy_logs_clear: { method: "POST", path: "/api/proxy/logs/clear" },
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

/**
 * 今日签到状态的**批量**读取；返回 `null` 表示本端没有批量通道。
 *
 * # 为什么必须有这个函数（2026-10-03 实测）
 *
 * webui 的 `GET /api/checkin/status` 是**批量**接口 —— 服务端
 * （`crates/ai-gateway-server/src/api.rs:1378`）遍历**全部**账号、逐个串行
 * 打上游，再把整张表返回。上面的 `getCheckinStatus(accountId)` 走的是
 * 「整批拉一次 → `find` 出自己那一条」，单个账号用它没问题。
 *
 * 但账号页是**按账号循环**调用的（`fetchTodayCheckinMap`）⇒ 27 个账号
 * 就是 **27 次全量请求**，每次 8–27 秒（上游限流时第二次更慢），
 * 在途 4 并发。实测启动后 30 秒窗口内 `/api/checkin/status` 累计占用
 * 80 秒，期间页面上任何操作都在排队 —— 这正是所有者反馈的
 * 「每次启动都要响应好长时间，要等一段时间之后才能恢复」。
 *
 * 故新增本函数：**一次请求换一张按 id 索引的表**。调用方拿 map 按 id 取用，
 * 语义与逐个调用完全一致（拉不到的账号**不出现在 map 里**，由调用方保留原值）。
 *
 * 桌面端（Tauri）没有批量命令（`get_checkin_status` 收 `account_id`），
 * 这里返回 `null`，调用方自行退回逐个调用 —— 桌面端是本地调用，没有这个问题。
 */
export async function getCheckinStatusMap(): Promise<Record<
  string,
  { ok: boolean; todayCheckedIn: boolean; error?: string; raw?: unknown }
> | null> {
  // 演示模式：没有对应的假数据，交回调用方走逐个路径（那里有 case）。
  if (demoModeEnabled) return null;
  if (!isWebui()) return null;
  const all = await httpCall<{
    accounts: {
      accountId: string;
      ok: boolean;
      todayCheckedIn: boolean;
      error?: string;
      raw?: unknown;
    }[];
  }>("get_checkin_status");
  const map: Record<
    string,
    { ok: boolean; todayCheckedIn: boolean; error?: string; raw?: unknown }
  > = {};
  for (const one of all.accounts ?? []) {
    if (!one || !one.accountId) continue;
    map[one.accountId] = {
      ok: one.ok,
      todayCheckedIn: one.todayCheckedIn,
      error: one.error,
      raw: one.raw,
    };
  }
  return map;
}

export function getCreditExpiry(accountId: string): Promise<CreditExpiry> {
  return call("get_credit_expiry", { accountId });
}

/**
 * 积分与到期时间的**批量**读取；返回 `null` 表示本端没有批量通道。
 *
 * # 为什么必须有这个函数（2026-10-03 实测，问题 ⑩-2 的最后一个热路径）
 *
 * `stores/accounts.ts:122` `loadCredits` 是**按账号循环**调 `getCreditExpiry`
 * 的：27 个账号、前端 4 并发，而每个账号在上游要 3~4 次往返（实测 0.4~0.8 s）
 * ⇒ 累计 11~22 秒的上游流量全压在启动那一刻。前端那个 4 并发挡得住 WebView2
 * 主线程，挡不住上游总量。
 *
 * 服务端 `GET /api/credits/batch` 一次把全部账号查完（4 并发），本函数把结果
 * 按 id 索引。语义与逐个调用一致：**拉不到的账号不出现在 map 里**，
 * 由调用方保留原值。
 *
 * 桌面端（Tauri）没有对应的批量命令，这里返回 `null`，调用方退回逐个调用 ——
 * 桌面端是本地 IPC，没有这个问题。
 */
export async function getCreditExpiryMap(): Promise<Record<string, CreditExpiry> | null> {
  // 演示模式：没有对应的假数据，交回调用方走逐个路径（那里有 case）。
  if (demoModeEnabled) return null;
  if (!isWebui()) return null;
  const all = await httpCall<{ accounts: CreditExpiry[] }>("get_credit_expiry_batch");
  const map: Record<string, CreditExpiry> = {};
  for (const one of all.accounts ?? []) {
    const id = one?.accountId;
    if (!id) continue;
    map[id] = one;
  }
  return map;
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

/**
 * 今日旅行状态的**批量**读取；返回 `null` 表示本端没有批量通道。
 *
 * 与 `getCheckinStatusMap` 同一回事（见那里的长注释）：webui 的
 * `GET /api/travel/status`（`crates/ai-gateway-server/src/api.rs:1584`）也是
 * 批量接口，而 `getTravelStatus(accountId)` 是「整批拉一次 → find」，
 * 账号页按账号循环调用时就是 N 次全量请求。
 *
 * 旅行接口本身很便宜（实测空闲 9 ms，只读本地文件、不碰上游），
 * 但风暴里它 15 次累计占用 83 秒（中位 4.6 秒）—— 那些时间全花在
 * **排队**上：重接口把宿主/浏览器的连接与在途额度占满后，便宜接口也得等。
 * 所以它必须与签到一起改成批量，才能把启动期的请求条数真正压下来。
 *
 * `message` / `skip` 与单个版本一样必须带出（原因见上面的注释）。
 */
export async function getTravelStatusMap(): Promise<Record<string, TravelStatus> | null> {
  if (demoModeEnabled) return null;
  if (!isWebui()) return null;
  const all = await httpCall<{
    accounts: {
      accountId: string;
      label: TravelStatus["label"];
      rewardCredit: number | null;
      locationName?: string | null;
      arriveAt?: number | null;
      message?: string | null;
      skip?: string | null;
    }[];
  }>("get_travel_status");
  const map: Record<string, TravelStatus> = {};
  for (const one of all.accounts ?? []) {
    if (!one || !one.accountId) continue;
    map[one.accountId] = {
      label: one.label,
      rewardCredit: one.rewardCredit,
      locationName: one.locationName ?? null,
      arriveAt: one.arriveAt ?? null,
      message: one.message ?? null,
      skip: one.skip ?? null,
    };
  }
  return map;
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

/** 读取网关日志尾部（移植上游 0.8.9 的查看入口）。 */
export function readGatewayLog(bytes?: number): Promise<GatewayLogResult> {
  return call<GatewayLogResult>("read_gateway_log", bytes && bytes > 0 ? { bytes } : undefined);
}

/** 在文件管理器里选中网关日志文件。 */
export function revealGatewayLog(): Promise<{ ok: boolean; path?: string; error?: string }> {
  return call<{ ok: boolean; path?: string; error?: string }>("reveal_gateway_log");
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
 * 发送短信登录验证码（2026-09-22 新增）。
 *
 * ⚠ 这是**真实发短信**的写操作：会消耗上游配额、触发频控。
 * 界面必须做倒计时与防连点（见 sms-login-dialog.tsx）。
 */
export function smsSend(
  phone: string,
  region: "cn" | "intl",
): Promise<{ ok: boolean; expiresIn: number }> {
  return call<{ ok: boolean; expiresIn: number }>("sms_send", { phone, region });
}

/** 用验证码换 token 并登记账号。成功返回账号摘要（形状由宿主决定）。 */
export function smsVerify(
  phone: string,
  smsCode: string,
  region: "cn" | "intl",
): Promise<{ ok: boolean; account?: { uid?: string; phone?: string; region?: string } }> {
  return call<{ ok: boolean; account?: { uid?: string; phone?: string; region?: string } }>(
    "sms_verify",
    { phone, smsCode, region },
  );
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
  // Qoder / ZCode 权益自动领取（2026-09-22 拆分）。
  // ⚠ 同样必须显式转 camelCase —— 桌面版 Tauri 只认 camelCase，
  // 写 snake_case 会被**静默丢弃**，表现为「关掉了却还在自动领」。
  //
  // ⚠ 三个键都只在**非 undefined** 时才传：
  // `product_tasks_enabled` 是老配置的兜底总闸，两个分产品键是三态
  //（undefined = 没配 ⇒ 回落总闸）。传一个 `false` 进去就等于替用户
  // 做了决定，而活动不领就过期作废。
  if (config.product_tasks_enabled !== undefined) {
    args.productTasksEnabled = config.product_tasks_enabled;
  }
  if (config.qoder_claim_enabled !== undefined) {
    args.qoderClaimEnabled = config.qoder_claim_enabled;
  }
  if (config.zcode_claim_enabled !== undefined) {
    args.zcodeClaimEnabled = config.zcode_claim_enabled;
  }
  // Qoder 领取时点：空数组是合法值（关掉时点制），故不能用 `?.length`
  // 之类判断"有没有值"—— 只判 undefined。
  if (config.qoder_claim_hours !== undefined) {
    args.qoderClaimHours = config.qoder_claim_hours;
  }
  if (config.qoder_claim_interval_minutes !== undefined) {
    args.qoderClaimIntervalMinutes = config.qoder_claim_interval_minutes;
  }
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

/**
 * 读取「平台 × 区域 → 允许的模型」白名单**与**逐条禁用清单。
 *
 * 两个清单一次读回（宿主侧同一份快照），故界面不会渲染出
 * "白名单是新的、禁用项是旧的"这种现实中从未存在过的组合。
 */
export function getPlatformModels(): Promise<PlatformModelsState> {
  return call<PlatformModelsState>("get_platform_models");
}

/**
 * 写入「平台 × 区域 → 允许的模型」白名单，可选同时写禁用清单。
 *
 * ## 方向（与 {@link setModelPlatforms} 相反）
 *
 *	setModelPlatforms   模型 → 允许的平台
 *	setPlatformModels   平台(+区域) → 允许的模型   ← 本函数
 *
 * ## `disabled` 的三态（别当成"可选参数"随手传）
 *
 *	数组（可为空）→ 连同禁用清单一起写
 *	undefined      → **不动**已有的禁用清单，而不是清空
 *
 * 为什么 undefined 是"不动"：老界面 / 只改白名单的调用方不该因为
 * 没传这个字段就**静默清掉**用户配好的禁用项 —— 那正是本功能最不该
 * 出现的失败模式。要清空得显式传 `[]`（或 `{}`）。
 *
 * ## ⚠ 为什么"禁用"不能靠"从白名单里删掉"来实现
 *
 * 放行判据是「手动配置 ∪ 接口返回 ∪ 兜底」的**并集**，只要还有
 * **任何一个**来源放行就通过 —— 所以删掉手动项删不掉它
 *（`qoder:deepseek-v4.1-flash` 就是被"接口返回"那份放行的）。
 * 禁用必须是并集**之外**的独立否决项，故它有单独一个配置键。
 */
export function setPlatformModels(
  platforms: Record<string, Record<string, string[]>>,
  disabled?: Record<string, Record<string, string[]>>,
): Promise<GatewayModeSwitchResult> {
  // ⚠ 必须显式构造 args：Tauri 侧 `disabled` 是 `Option<Value>`，
  // 传 `undefined` 时**不能**让键出现在 payload 里（出现即被当成
  // "要写这个键"，与"不触碰"语义相反）。故这里用条件展开，
  // 而不是 `{ platforms, disabled }`。
  return call<GatewayModeSwitchResult>("set_platform_models", {
    platforms,
    ...(disabled === undefined ? {} : { disabled }),
  });
}

// ---------------------------------------------------------------------------
// 平台 → 是否参加兼容网关路由
// ---------------------------------------------------------------------------

/**
 * 参加兼容网关路由的平台（小写稳定标识）。
 *
 * 与网关侧的产品标识同义（ProductOf）。**豆包不在其中** ——
 * 它不是路由平台（无产品模块、账号不进网关池），故统一账号页的
 * 豆包 tab 上不显示这个开关。
 */
export type RoutingPlatform = "workbuddy" | "qoder" | "zcode" | "trae";

/**
 * get_platform_enabled 的返回：平台键 → 是否参加路由。
 *
 * ⚠ **键缺失 = 参加**（不是"未知"）。后端刻意只写显式关掉的那些键，
 * 这样新增平台时默认就是参加，不会因为老配置文件里没有它而被当成关闭。
 * 判断一律用 platformEnabled[p] !== false，**别用** !!platformEnabled[p]
 * —— 后者会把"没配过"读成"关掉了"。
 */
export interface PlatformEnabledState {
  platformEnabled: Record<string, boolean>;
}

/** 读取各平台是否参加兼容网关路由。 */
export function getPlatformEnabled(): Promise<PlatformEnabledState> {
  return call<PlatformEnabledState>("get_platform_enabled");
}

/**
 * 写入「平台 → 是否参加兼容网关路由」。
 *
 * ## 语义（业主原话，别理解错）
 *
 * 关闭平台**只是不参与兼容网关**，并不是不执行任务 —— 签到、上报、
 * 成长任务、额度巡检**照跑**。故它**不是**账号级的 disabled
 * （那个是"这个号已死，任务也跳过"），网关侧是独立的一个配置键。
 *
 * ## 传参
 *
 * 传的是**整键替换**的那份 map：调用方要么传全部平台，要么只传
 * 要改的那个键（后端按 merge 处理 —— 键缺失 = 不动该键，
 * 故不要用 false 去"补全"没打算动的平台）。
 */
export function setPlatformEnabled(
  enabled: Record<string, boolean>,
): Promise<{ ok: boolean; reloaded?: boolean; platformEnabled: Record<string, boolean> }> {
  return call<{ ok: boolean; reloaded?: boolean; platformEnabled: Record<string, boolean> }>(
    "set_platform_enabled",
    { enabled },
  );
}

// ---------------------------------------------------------------------------
// 一键导入：接入本机 AI 客户端
// ---------------------------------------------------------------------------

/**
 * 获取网关可用模型列表（优先动态查询网关，网关未就绪时回退静态并集）。
 *
 * `refresh` = true 时**强制重拉**：让网关清掉按区域的失败负缓存。
 *
 * ⚠ 界面上的「刷新模型列表」必须传 true —— 那段「国服未检测到可用真值…
 * 稍后点刷新重试即可确认」的文案承诺了"刷新能重新确认"，而带缓存的路径
 * 在失败后 5 分钟内**连试都不试**，点了不会有任何变化（2026-09-20 实测）。
 */
export async function getGatewayModels(refresh = false): Promise<GatewayModelItem[]> {
  const res = await call<{ models?: GatewayModelItem[] }>(
    "get_gateway_models",
    refresh ? { refresh: true } : undefined,
  );
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

/**
 * Trae 账号（JWT 已脱敏）。
 *
 * 字段清单与 `crates/ai-gateway-core/src/modules/trae_account.rs` 的
 * `pub fn account_meta(acc: &Value) -> Value` 逐字对齐（22 个键）。
 *
 * 可空字段一律声明成 `| null`：后端**刻意**用 null 而不是 0 表示「不知道」，
 * 界面才能区分「没查过」与「确实是 0」——把 null 当成 0 会把没查过的账号画成零积分。
 */
export interface TraeAccountMeta {
  userId: string;
  name: string;
  addedAt: string | null;
  updatedAt: string | null;
  jwtStatus: "ok" | "warn" | "expired" | "unknown";
  jwtExpHours: number | null;
  jwtExpTimestamp: number | null;
  /** 区域：`"cn"` 国内版 / `"sg"` 国际版。 */
  realm?: string | null;
  /** 区域中文标签（后端 `Realm::label()`）。 */
  realmLabel?: string | null;
  /** 区域判定的依据来源（`trae_realm::RealmEvidence`）。 */
  realmEvidence?: string | null;
  /** 区域是否为实测判定；false = 兜底推断。 */
  realmDetected?: boolean | null;
  hasRefreshToken: boolean;
  refreshTokenInvalid: boolean;
  refreshTokenFails: number;
  refreshTokenExpiresAt?: string | null;
  refreshedAt?: string | null;
  /** 有非空 refresh token 且未被标记失效 —— 即「能否自动续期」。 */
  canRefresh?: boolean | null;
  deviceIdMasked: string;
  /** 付费身份（来自客户端发现痕迹），未发现时为 null。 */
  payIdentity?: string | null;
  /** 套餐到期时间。 */
  payExpireAt?: string | null;
  /**
   * 积分总额。★ Trae 侧**从来没有写入点**（`qoder_account.rs` / `zcode_account.rs`
   * 才有 `credits_total = n`），故实际恒为 null；要真实数字请读
   * `traeCreditsStats().accounts[].credits`（本地采样历史，不连上游）。
   */
  creditsTotal?: number | null;
  creditsUpdatedAt?: string | null;
  /** 所属分组 id。 */
  group?: string | null;
  cooldown?: { type?: string; until?: number; reason?: string; error_count?: number };
  /**
   * 是否被禁用（不进入网关账号池）。
   *
   * ⚠ 三态：`undefined` = 后端还没补这个键（老版本返回）⇒ 界面按"未禁用"
   * 显示即可，**不要**把它当成 false 之外的东西 —— 但也不能反过来把
   * undefined 当"确定启用"，故这里声明成可选而不是 `boolean`。
   * 与账号级 disabled 同义：禁用只影响"进不进网关选号池"，
   * 签到等养号任务照跑。
   */
  disabled?: boolean;
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

/**
 * 部分更新 Trae 账号（启用 / 禁用；后端也支持改名与换 JWT）。
 *
 * ## ⚠ 必须显式构造 args
 *
 * 后端每个字段都是"键缺失 = 不动"，而 `disabled: false` 是**明确启用** ——
 * 两者语义相反。若写成 `{ userId, ...patch }`，patch 里没提到的键
 * 确实不会出现（对象展开天然如此），但一旦有人图省事写成
 * `{ userId, disabled: patch.disabled }`，就会把"不改"变成"设成 undefined"，
 * 在后端那边可能被读成 false（= 禁用）。所以这里逐键判断，
 * 让「缺失」永远保持缺失。
 *
 * 现有调用点只有统一账号页的 Trae 卡片开关（传 `{ disabled }`）。
 */
export function traeUpdateAccount(
  userId: string,
  patch: { name?: string; jwt?: string; refreshToken?: string; disabled?: boolean },
): Promise<{ ok: boolean; account: TraeAccountMeta }> {
  return call<{ ok: boolean; account: TraeAccountMeta }>("trae_update_account", {
    userId,
    ...(patch.name === undefined ? {} : { name: patch.name }),
    ...(patch.jwt === undefined ? {} : { jwt: patch.jwt }),
    ...(patch.refreshToken === undefined ? {} : { refreshToken: patch.refreshToken }),
    ...(patch.disabled === undefined ? {} : { disabled: patch.disabled }),
  });
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
 * ## ⚠ 活动是**每账号专属**的
 *
 * 这个接口只返回**一个账号**的活动。A 账号领了 100 Credits，B 账号可能
 * 还有没领的 —— 把单个账号的结果当全局，会让其他账号的活动永远发现不了。
 *
 * 要看全部账号请用 `qoderCampaignsAll()`。
 *
 * ## 领取
 *
 * 领取**不需要**人机验证（用用户自己的令牌打官方接口，与官方客户端点
 * 那个「领取」按钮同构）。单账号领取用 `qoderClaimCampaign`，
 * 全部账号用 `qoderClaimAllCampaigns`。
 */
export function qoderCampaigns(uid: string): Promise<QoderCampaignsResult> {
  return call<QoderCampaignsResult>("qoder_campaigns", { uid });
}

/** 一个账号的活动查询结果（批量接口的条目）。 */
export interface QoderAccountCampaigns {
  uid: string;
  nickname: string;
  /** "ok" 查到 / "error" 该账号查失败（原因在 message） */
  status: string;
  /** 该账号**可领取**的活动数（0 = 已领完或没有） */
  claimable: number;
  campaigns?: QoderCampaign[];
  campaignUrl?: string | null;
  /** status == "error" 时的原因 */
  message?: string;
}

/** 所有账号的活动查询结果。 */
export interface QoderCampaignsAllResult {
  accounts: QoderAccountCampaigns[];
  /** 所有账号里可领取的活动总数 */
  claimableTotal: number;
  /** 有几个账号有可领取的活动 */
  accountsWithClaimable: number;
}

/**
 * 查询**所有账号**的 Qoder 权益活动。
 *
 * ## 为什么需要它（所有者的反馈）
 *
 *	「qoder 那个任务跟 workbuddy 一样都属于每个账号的专属任务,
 *	  每个账号都能领取」
 *
 * 活动是每账号专属的，只查一个账号会让其他账号的活动**永远发现不了**。
 *
 * 单个账号失败不影响其余 —— 结果里逐账号带 `status`。
 */
export function qoderCampaignsAll(): Promise<QoderCampaignsAllResult> {
  return call<QoderCampaignsAllResult>("qoder_campaigns_all", {});
}

/** 一个账号的领取结果（批量一键领取的条目）。 */
export interface QoderAccountClaimResult {
  uid: string;
  nickname: string;
  /** "ok" 有处理 / "nothing" 无可领 / "error" 该账号失败 */
  status: string;
  message?: string;
  claimed: Array<{
    campaignId: string;
    ok: boolean;
    /** true = 上游说"之前已领过"（不是新领到） */
    replayed?: boolean;
    benefit?: { kind: string; amount: number } | null;
    error?: string;
  }>;
}

/** 一键领取（所有账号）的结果。 */
export interface QoderClaimAllResult {
  accounts: QoderAccountClaimResult[];
  /** 本次**新领到**的数量（不含 replayed） */
  claimedCount: number;
  /** 失败数 */
  failedCount: number;
  /** 无可领活动的账号数 */
  nothingCount: number;
}

/**
 * **一键领取所有账号**的可领取权益活动。
 *
 * ⚠ 写操作，只由用户显式点击触发 —— 不做定时自动领取
 *（那与"用户点一下"不是一回事，且会让账号表现出非人类的活动模式）。
 *
 * 单个账号/活动失败不中断其余：用户要的是"能领的都领到"。
 */
export function qoderClaimAllCampaigns(): Promise<QoderClaimAllResult> {
  return call<QoderClaimAllResult>("qoder_claim_all_campaigns", {});
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
   * 上游**账号**标识（如 `12345678901234567`）；空 = 未知。
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
  /**
   * **各模型桶**的额度明细（上游 `billing/balance` 的 `balances[]`）。
   *
   * # ⚠⚠ 为什么必须有它（2026-09-21 所有者报的缺陷）
   *
   * 所有者原话：
   *
   * > 「那八百万额度，是 glm5.3flash 五百万，三百万 glm5.3，zcode 账号
   * >   flash 模型额度我用完了，你优化一下显示，那里应该拆分成两个模型的
   * >   额度，而不是一个的」
   *
   * 上游把额度按**模型**分成独立的桶（实测样本见
   * `uitest/zcode-config-samples/billing-balance.json`）：
   *
   * ```text
   * showName = "GLM-5.3"        total = 3,000,000
   * showName = "GLM-5.3-Flash"  total = 5,000,000
   * ```
   *
   * 而 `credits` / `creditsTotal` 是它们的**求和**（800 万）。
   * 只看求和会把「某个模型已用完」完全掩盖 —— 这正是所有者遇到的情况：
   * Flash 用光了，但卡片上还剩 300 万（那是 GLM-5.3 的），看不出问题。
   *
   * 空数组 = 没有明细（老账号记录、或上游没给）⇒ 界面回退到显示求和值。
   */
  quotaEntries?: Array<{
    /** 模型名（上游 `show_name`），如 `GLM-5.3-Flash`。 */
    showName: string;
    remaining: number;
    total: number;
    used: number;
    unitType: string;
    /** **每日周期**结束（Unix 秒）；不是套餐到期。 */
    expiresAt?: number;
  }>;
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
  /**
   * 最近一次「激活」（上报官方启动事件 + 领取套餐）的时刻（Unix 秒）；
   * 0 = 从未激活。
   *
   * # 为什么界面需要它
   *
   * 宿主**启动后会对每个账号自动激活一次**（幂等：距上次 ≥ 7 天才再报一次），
   * 所以「刚导入但还没激活」的账号会自己好起来。界面据此把状态说清楚，
   * 而不是让用户看到一个按钮猜它要不要点：
   *
   *   · 0        → 「从未激活」+ 提示「网关启动时会自动激活」
   *   · 非 0    → 显示激活时刻，用户知道不必再点
   *
   * ⚠ 老账号记录没有这个键 ⇒ 后端补 0 ⇒ 升级后第一次启动会被认作
   * 「从未激活」并激活一次，这正是期望行为（见后端 `warmed_at` 的说明）。
   */
  warmedAt?: number;
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
   * 账号名（来自客户端登录态，如 `acct-a`）。
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

/**
 * 激活 ZCode 账号（**写操作**）：报官方激活事件 + 领取上游给出的所有套餐。
 *
 * 复刻官方客户端**首次启动**时做的两件事（`app_launch` / `app_daily_active`），
 * 让刚导入的账号在上游看来「活动过」，而不是「从未出现过」。
 *
 * # 触发时机（两个入口）
 *
 * 1. 用户点账号卡片的「激活」菜单项（本函数）；
 * 2. 宿主**启动后每账号一次**的自动激活（Rust 侧
 *    `multi_product_credit_patrol::warm_zcode_accounts`，幂等标记 = 账号库的
 *    `warmedAt`，距上次超过 7 天才再报一次）。
 *
 * # ⚠ 但**仍然**不要把它放进 `useEffect` / 定时器 / 批量流程里
 *
 * 理由不是"礼貌"，而是：**高频**自动「激活 + 领取」会让账号表现出非人类的
 * 活动模式，而上游正是按活动模式判风控的 —— 那会把本来好好的账号推进风控。
 * 自动入口刻意做成"每次启动最多一次"，不是"每隔几分钟一次"。
 *
 * # ⚠ 三个阶段字段要分开显示，不要压平成一句话
 *
 * `events` / `claim` / `skipped` **空串 = 该阶段成功**（口径见 Go 侧 `WarmReport`）：
 * - `events` 非空 → 激活事件就没发出去，**后面根本没领**
 * - `skipped` 非空 → 事件发了，但没执行领取（上游没给可领套餐等）
 * - `claim` 非空 → 领取过程部分失败
 * - 三个都空且 `claimed` 非空 → 真的领到了
 *
 * 压平成「激活成功/失败」会丢掉「事件到底发了没」这个关键区分 —— 而它决定了
 * 用户下一步该做什么（事件没发是凭证/JWT 问题，没套餐是上游没活动）。
 */
export function zcodeWarmAccount(uid: string): Promise<{
  status: "ok";
  uid: string;
  /** 激活事件阶段的结果，**空串 = 成功**。 */
  events: string;
  /** 领取阶段的结果，**空串 = 成功**。 */
  claim: string;
  /** 未执行领取的原因（空串 = 已执行或无需执行）。 */
  skipped: string;
  /** 逐个套餐的领取明细。 */
  plans: Array<{
    planId: string;
    name?: string;
    priority?: number;
    ok: boolean;
    alreadyClaimed?: boolean;
    message?: string;
  }>;
  /** 成功领到的套餐 id（含此前已领过的）。 */
  claimed: string[];
  /** 后端拼的汇总句，供直接展示。 */
  message: string;
  /** 激活后刷新得到的账号视图（激活的收益要刷新才可见）。 */
  account?: ZcodeAccount | null;
  /** 激活后的刷新失败原因（**不影响激活本身已成功**）。 */
  refreshError?: string | null;
}> {
  return call("zcode_warm_account", { uid });
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

/**
 * 只对**一个**账号跑续期探活（账号卡片上的「本账号任务」重跑）。
 *
 * 返回的是批量结果摊平后的单账号形态：顶层直接带 `status` / `message`，
 * 前端不必再去 `results` 数组里按 userId 找。
 */
export function doubaoRenewOne(userId: string): Promise<{
  uid: string;
  ok: number;
  expired: number;
  skipped: number;
  errors: number;
  total: number;
  status: string;
  message: string;
  results: { userId: string; name: string; status: string; message: string }[];
  error?: string;
}> {
  return call<{
    uid: string;
    ok: number;
    expired: number;
    skipped: number;
    errors: number;
    total: number;
    status: string;
    message: string;
    results: { userId: string; name: string; status: string; message: string }[];
    error?: string;
  }>("doubao_renew_one", { userId });
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

/**
 * 只巡检**一个**账号的额度（账号卡片上的「本账号任务」重跑）。
 *
 * 返回 `DoubaoQuotaView` 形态（与批量结果里的逐账号结果不同，
 * 这里带完整的 windows/items，可直接喂给额度弹窗）。
 */
export function doubaoQuotaOne(userId: string): Promise<DoubaoQuotaView & { exhausted?: boolean }> {
  return call<DoubaoQuotaView & { exhausted?: boolean }>("doubao_quota_one", { userId });
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
 * **检测代理是否真的能用**（2026-09-22 所有者要求：
 * 「本地代理,配置之后再加上检测按钮」）。
 *
 * 与 `proxyStatus` 的区别：后者只回答"进程在不在跑"，
 * 而"能不能用"还要看端口真在监听、系统代理真指向它、CA 证书已生成 ——
 * 这三件任何一件不成立，用户都会觉得"代理坏了"，而 `running: true` 一个都答不了。
 */
export function proxyCheck(): Promise<{
  /** 所有检查项都通过才为 true（界面据此显示绿/红）。 */
  ok: boolean;
  checks: { name: string; ok: boolean; detail: string }[];
  /** 检测时刻（Unix 毫秒），界面显示"上次检测于 …"。 */
  checkedAt: number;
}> {
  return call<{
    ok: boolean;
    checks: { name: string; ok: boolean; detail: string }[];
    checkedAt: number;
  }>("proxy_check");
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

/** 上游代理连通性自检的单项结果。 */
export interface ProxyProbeItem {
  name: string;
  ok: boolean;
  /** 只有成功时才有：HTTP 状态码（任何状态码都算"到达了"）。 */
  status?: number;
  elapsedMs?: number;
  /**
   * 这个域名是干什么的（如「WorkBuddy 国际版对话与任务」）。
   *
   * 用户看到某一项不通时，需要立刻知道**影响哪个功能** ——
   * 否则他不知道要不要管它（`api3.qoder.sh` 不通意味着 Qoder 用不了，
   * 而他可能根本不用 Qoder）。
   */
  purpose?: string;
  detail: string;
}

/** 上游代理连通性自检的完整结果。 */
export interface ProxyProbeResult {
  /**
   * **全部**域名都通过才为 true。
   *
   * ⚠ 不要把它当"至少通一个"用（2026-09-23 修）：这份清单里每一项都对应
   * 一个真实功能，任一不通就是那个功能坏了。曾按"任一通过即 true"汇总，
   * 结果 github 挂了却显示「可以连通外网」—— 而自动更新恰恰依赖 github。
   */
  ok: boolean;
  /** 是否**至少有一个**通过。用于区分"全坏"与"坏了一项"。 */
  anyOk?: boolean;
  /** 不通的域名清单（空 = 全通）。界面据此指出具体哪一项坏了。 */
  failed?: string[];
  /** `github.com` 是否不通 —— 它单独坏了会让「检查更新」失败。 */
  githubBlocked?: boolean;
  /** 解析出的代理地址（如 `127.0.0.1:7890`）。 */
  addr?: string;
  /** 地址为空/无法解析时的原因。 */
  error?: string;
  /** 逐项探测结果；地址非法时不存在。 */
  results?: ProxyProbeItem[];
  /** 代理全不通时，直连的结论 —— 用来区分"网络问题"还是"代理问题"。 */
  directNote?: string | null;
}

/**
 * 测试上游代理能否连通外网（**只读**，不改任何配置）。
 *
 * 会真的发请求，逐个探测**本应用实际用到的国外域名**：
 * `github.com` / `www.workbuddy.ai` / `openapi.qoder.sh` / `api3.qoder.sh` /
 * `zcode.z.ai` / `api.z.ai`。每一项都对应一个功能，故 `ok` 要求**全部通过**。
 */
export function proxyTestUpstream(spec: string): Promise<ProxyProbeResult> {
  return call<ProxyProbeResult>("proxy_test_upstream", { spec });
}

/** 一个代理节点的探测结果。 */
export interface ProxyNodeItem {
  /** 节点名（原样保留，可能含 emoji/中文）。 */
  name: string;
  /** 所属分组名。 */
  group: string;
  /** 延迟毫秒；`null` = 不通/超时。 */
  delayMs: number | null;
  /** 是否连通。 */
  ok: boolean;
  /** 是否当前正在使用。 */
  current: boolean;
}

/** 本机代理节点探测的完整结果。 */
export interface ProxyNodesResult {
  ok: boolean;
  /** 找不到/连不上控制接口时的原因（**可操作**的说明，不是空泛一句话）。 */
  error?: string;
  /** 用到的控制接口（如「命名管道 MihomoParty\…」），便于确认探测的是哪个软件。 */
  endpoint?: string;
  /** 探测用的目标域名（本应用真实依赖的那个）。 */
  testUrl?: string;
  nodes?: ProxyNodeItem[];
  total?: number;
  usable?: number;
}

/**
 * 探测**本机代理软件里的所有节点**，找出哪个对 `workbuddy.ai` 最快。
 *
 * # 为什么这个功能有用（实测根因）
 *
 * 国际版走代理而首字节要 4–8 秒；但同一代理打 `api.z.ai` 只要 0.34 秒
 * —— 即"代理不慢，是走错了节点"。本函数据此排序，直接告诉你换哪个。
 *
 * ⚠ **只在桌面端可用**：它要读本机代理软件的控制接口
 *（Windows 命名管道 / macOS·Linux unix socket / 常见 TCP 端口），
 * 浏览器模式没有这个能力。
 *
 * ⚠ 支持 Clash / mihomo 系（Clash Verge、mihomo-party、ClashX、FlClash…）。
 * v2rayN / Shadowsocks 等没有同级控制接口，会返回**说明原因**的错误而不是空列表。
 */
export function proxyProbeNodes(): Promise<ProxyNodesResult> {
  return call<ProxyNodesResult>("proxy_probe_nodes");
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

// ---------------------------------------------------------------------------
// 本机 AI CLI 登录额度查询（Codex / Claude / Antigravity / Grok / Kimi）
// ---------------------------------------------------------------------------

/**
 * 查询全部 provider 的本机账号额度。
 *
 * `refresh = false`（默认）时读**磁盘缓存**，不打上游；界面首屏用这个，
 * 避免打开页面就产生 5 次网络往返。
 *
 * **未登录的 provider 也会出现在结果里**（`loggedIn: false` + 可操作提示），
 * 界面据此按 provider 分组展示，而不是整块消失。
 */
export function getCliQuotas(refresh = false): Promise<{ accounts: CliQuotaAccount[] }> {
  return call<{ accounts: CliQuotaAccount[] }>("get_cli_quotas", { refresh });
}

/**
 * 只刷新一个 provider 的额度（界面上的单卡片刷新）。
 *
 * ⚠ 这个调用会**真的打上游**。失败时返回 `{ ok: false, error }` 而不是抛异常
 * —— 单个 provider 查不到额度是常态（没登录、上游改了口径），不该让整页崩。
 */
export function refreshCliQuota(
  provider: CliQuotaProvider,
): Promise<{ ok: boolean; account?: CliQuotaAccount; error?: string }> {
  return call<{ ok: boolean; account?: CliQuotaAccount; error?: string }>("refresh_cli_quota", {
    provider,
  });
}

/**
 * 探测本机各 CLI 的登录态（**不触网**、不查额度）。
 *
 * 与 [`getCliQuotas`] 分开：登录态是本地文件/凭据读取（毫秒级），额度查询要打
 * 上游（秒级）。界面先用它把「已登录 / 未登录」渲染出来，再让用户决定要不要
 * 真正查询 —— 否则首屏会被 5 次网络往返拖住。
 */
export function getCliQuotaStatus(): Promise<{ providers: CliQuotaStatusItem[] }> {
  return call<{ providers: CliQuotaStatusItem[] }>("get_cli_quota_status");
}

// ---------------------------------------------------------------------------
// Trae / 豆包 分组、积分、用量、签到趋势、OAuth 回环、抓包日志
//
// 以下类型与函数自上游 PR#2（aigw-fork 0697b89）原样并入，参数名与返回形状
// 与 `src-tauri` 命令层、`crates/ai-gateway-server` 的 HTTP 路由逐条对齐 ——
// 改形状必须三边同改。类型定义在 api.ts（而非 types.ts）是因为组件用
// `api.XxxType` 命名空间取值，只放在 types.ts 里取不到。
// ---------------------------------------------------------------------------

// ---- 类型 ----

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
  /**
   * 权益包列表。
   *
   * ★ 恒为数组，**永不为 `null`** —— 后端在拿不到权益包时下发 `[]`。
   * 这里若写成 `TraeCreditPack[] | null`，消费方（`CreditCell`）就会有人
   * 忘了判空而直接 `.length`，把「拿不到权益包」变成整页白屏。
   * 契约由 `crates/ai-gateway-core/src/modules/trae_credits.rs`
   * 的 `fetch_credit_detail` 保证。
   */
  packs: TraeCreditPack[];
  payIdentity: string | null;
  payExpireAt: string | null;
  /** 逐来源错误（部分失败时仍返回已拿到的数据）。恒为数组，理由同 `packs`。 */
  errors: string[];
}

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

/** 一天的签到汇总。 */
export interface TraeCheckinTrendPoint {
  date: string;
  ok: number;
  already: number;
  failed: number;
  /** 冷却中跳过（本轮没打上游）；不计入 `total`，也不进成功率的分母。 */
  skipped: number;
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
    /** 冷却中跳过；不计入 `total`，也不进成功率的分母。 */
    skipped: number;
    total: number;
    observedDays: number;
    /** 区间成功率；无样本时为 `null`（不是 0）。 */
    successRate: number | null;
  };
}

/** 单个账号「今天签到了吗」。 */
export interface TraeCheckinToday {
  /** `true` 已签 / `false` 未签 / `null` **没问到**（网络异常或上游繁忙）。 */
  checkedIn: boolean | null;
  /** 落库用的原始状态串（success/already/checked_in/not_checked_in/fail）。 */
  status: string;
  name: string;
  /** 落库时间（ISO）。 */
  updatedAt: string;
}

/** 今日签到状态表。 */
export interface TraeCheckinTodayMap {
  /** 本地日期（YYYY-MM-DD）。 */
  date: string;
  /** uid → 状态。★ 缺键表示「不知道」，不是「未签到」。 */
  accounts: Record<string, TraeCheckinToday>;
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

/** Trae 账号导出结果（`text` 含明文 JWT 与 refresh_token）。 */
export interface TraeExportResult {
  ok: boolean;
  count: number;
  text: string;
}

/** 导入预览里的一项账号。 */
export interface TraeImportPreviewItem {
  userId: string;
  name: string;
  hasRefreshToken: boolean;
  /** 账号库里已有同 uid —— 导入会覆盖它。 */
  willOverwrite: boolean;
}

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

export interface ProxyLogList {
  entries: ProxyLogEntry[];
  /** 过滤后的**总数**（不是本页条数），用于分页。 */
  total: number;
}

/** 一个分组及其成员。 */
export interface AccountGroup {
  id: string;
  name: string;
  color: string;
  order: number;
  count: number;
  uids: string[];
}

/** 分组所属应用。 */
export type GroupApp = "Trae" | "Doubao";

/** 分组视图。 */
export interface GroupsView {
  app: GroupApp;
  groups: AccountGroup[];
  /** uid → groupId（只含仍然存在的分组）。 */
  membership: Record<string, string>;
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
  /** 调用方请求的天数。与 `health.days` 可能不同：健康计数按固定窗口统计，
   *  而事件/趋势按请求天数截取，故两个值都要下发，界面不能拿一个当另一个用。 */
  requestedDays: number;
}

/** 豆包运维健康史（事件 + 额度趋势 + 健康计数）。 */
export function doubaoHistory(days = 30): Promise<DoubaoHistory> {
  return call<DoubaoHistory>("doubao_history", { days });
}

// ---- 分组 ----

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

/**
 * 把账号移入/移出分组（`groupId` 为空 = 移出）。
 *
 * ⚠ `groupId` 必须**显式**传 `null` 而不是省略：后端 `apps_api.rs:645` 把
 * 「字段缺失」与「字段为 null」都当作移出，但省略字段会让请求体里少一个键，
 * 与参考实现的线格式不一致（`test-bin/recon/pr2-api/api.ts:1414` 也是显式传）。
 */
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

// ---- Trae OAuth（回环 + 手动回调）----

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
 *
 * ⚠ 与 `oauthStartLoopback` / `oauthStopLoopback` 同类：**没有 ROUTES 条目**。
 * 上游 PR#2 同样只有 Tauri 命令、没有 HTTP 路由，webui 下调它会在
 * `httpCall` 的 `!route` 分支明确报错 —— 桌面专属能力就该这样表现。
 */
export function oauthPending(): Promise<{ pending: boolean }> {
  return call("oauth_pending");
}

/**
 * 诊断「浏览器打不开授权页」（桌面专属）。
 *
 * # 为什么需要它（2026-10-02 所有者现场）
 *
 * 所有者报告「浏览器会打开，但是访问超时」。实测定位到**系统代理不通**：
 *
 * ```text
 * 系统代理 ProxyServer = 127.0.0.1:9000（Reqable）
 * trae.cn 经该代理 → 超时 20s
 * trae.cn 直连     → 200 / 0.39s
 * ```
 *
 * 浏览器默认「使用系统代理设置」，于是授权页打不开 —— 与本产品无关，
 * 但用户看到的只是「超时」，无从判断原因。这条命令把**实测事实**摆出来。
 *
 * 同时它会认出 `ProxyOverride` 里的 `<-loopback>` 记号 ——
 * 那个记号让 `127.0.0.1` 的回调**也走代理**，对 OAuth 回环是致命的。
 *
 * ⚠ **只读**：只做 TCP 探测 + 读注册表，不改用户任何设置。
 */
export interface OAuthNetworkProbe {
  ok: boolean;
  ms: number;
  via?: string;
  status?: number;
  error?: string;
}

export interface OAuthNetworkDiagnosis {
  proxyEnabled: boolean;
  proxyServer: string;
  proxyOverride: string;
  loopbackAlsoProxied: boolean;
  direct: OAuthNetworkProbe;
  viaProxy: OAuthNetworkProbe & { skipped?: boolean };
  advice: string;
}

export function oauthDiagnoseNetwork(): Promise<OAuthNetworkDiagnosis> {
  return call("oauth_diagnose_network");
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

// ---- Trae 积分 / 用量 / 签到趋势 ----

/**
 * 读取某账号的三条积分账。
 *
 * ★ 在这里（数据进入前端的**唯一入口**）把两个数组字段钉成数组。
 * 起因是一个真实的崩溃：后端曾经把 `packs` 下发成 `null`（拿不到权益包时），
 * 而 `CreditCell` 直接读 `detail.packs.length` ⇒
 * `TypeError: Cannot read properties of null (reading 'length')`，
 * 整个 Trae 账号页被错误边界接住变成一页报错。
 *
 * 后端契约已经修好（恒下发 `[]`），这里再做一层**入口归一化**：
 * 只要旧版网关还在跑、或者将来又有人改了后端字段形状，
 * 界面最多是少显示一块内容，不会整页崩掉。
 * 这与本仓库「前端不能假设后端一定给对」的既有口径一致。
 */
export async function traeCreditDetail(userId: string): Promise<TraeCreditDetail> {
  const raw = await call<TraeCreditDetail>("trae_credit_detail", { userId });
  return {
    ...raw,
    packs: Array.isArray(raw?.packs) ? raw.packs : [],
    errors: Array.isArray(raw?.errors) ? raw.errors : [],
  };
}

/** 积分趋势统计（`daily` 按天补齐，缺天为 null）。 */
export function traeCreditsStats(days = 30): Promise<TraeCreditsStats> {
  return call<TraeCreditsStats>("trae_credits_stats", { days });
}

/** 签到成功率趋势（同 uid 同天只记最终态）。 */
export function traeCheckinTrends(days = 30): Promise<TraeCheckinTrends> {
  return call<TraeCheckinTrends>("trae_checkin_trends", { days });
}

/**
 * 今日各账号的签到状态（本地落库快照，**零网络请求**）。
 *
 * ★ 只读本地表：每次刷新页面调它都不会打上游。要让表变新，调
 * [`traeCheckinRefresh`]（那才会逐账号探测上游 status 端点）。
 */
export function traeCheckinStatus(): Promise<TraeCheckinTodayMap> {
  return call<TraeCheckinTodayMap>("trae_checkin_status");
}

/**
 * 主动刷新今日签到状态（逐账号只读探测上游，返回被确认的账号数）。
 *
 * 界面在挂载时**静默**调用一次：用户没点过签到、但当天其实早已签过的情况下，
 * 靠它才能把「已签到」徽标显示出来。失败不该打扰用户（返回 `updated: 0` 即可）。
 */
export function traeCheckinRefresh(): Promise<{ updated: number }> {
  return call<{ updated: number }>("trae_checkin_refresh");
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

/**
 * 立即采样一次积分快照。
 *
 * `userIds` 省略 / 空数组 ⇒ 采样**全部** Trae 账号（设置页的「立即采样」）；
 * 给了 uid 列表 ⇒ 只采样这些账号，供账号卡片上的「本账号任务 · 积分日快照」
 * 按单个账号执行。
 */
export function traeCreditsSnapshot(userIds?: string[]): Promise<{
  ok: boolean;
  sampled: number;
  failed: number;
  errors: string[];
}> {
  return call("trae_credits_snapshot", {
    userIds: userIds && userIds.length > 0 ? userIds : null,
  });
}

// ---- Trae 账号导入导出 ----

/**
 * 导出 Trae 账号。
 *
 * 不传 `userIds` 即导出全部。返回的 `text` 含**明文凭证**，
 * 界面必须明确提示用户妥善保管。
 */
export function traeExportAccounts(userIds?: string[]): Promise<TraeExportResult> {
  return call<TraeExportResult>("trae_export_accounts", { userIds: userIds ?? null });
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

// ---- 抓包日志 ----

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
