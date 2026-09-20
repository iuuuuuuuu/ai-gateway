// 与 Rust 后端命令返回结构对齐的类型定义（对照 server.py 各 API 响应）

/** 账号区域键；与后端 `Region::key()` 对齐。 */
export type AccountRegionKey = "cn" | "intl";

export interface AccountMeta {
  id: string;
  /** 服务区域展示名（"国服" / "国际版"），由 domain 后缀推导。 */
  region?: string;
  /** 区域键（"cn" / "intl"），便于样式与筛选。 */
  regionKey?: AccountRegionKey;
  uid: string | null;
  email: string | null;
  nickname: string | null;
  enterpriseName: string | null;
  expiresAt: number | null;
  refreshExpiresAt: number | null;
  refreshedAt: number | null;
  createdAt: number | null;
  needsRelogin: boolean;
  needsReloginReason: string | null;
  /**
   * 用户自定义备注（如「公司号」「备用」）。
   *
   * 为什么需要：授权进来的账号往往只带邮箱/手机号/随机 uid，光看这些认不出
   * 「这是谁的号、干什么用的」。备注只存本地，不参与登录。
   */
  note?: string | null;
  /**
   * 用户手动禁用：不进网关账号池。
   *
   * 注意：禁用的只是「接流量的资格」，签到 / 旅行 / 领奖等养号任务照跑 ——
   * 号暂时不接流量不等于不要额度与连登天数。这与网关内部因冷却/熔断而
   * 临时不可用完全不同：后者会自行恢复，前者只能由用户显式改回。
   */
  disabled?: boolean;
  /** 原始域名（如 www.workbuddy.ai / copilot.tencent.com）—— 排查时比区域标签更具体。 */
  domain?: string | null;
  /** 手机号（国服账号的真实身份线索；其 email 常为空）。 */
  phoneNumber?: string | null;
  /** 账号类型（personal / enterprise）—— 影响可用模型与额度口径。 */
  accountType?: string | null;
}

export interface AppStatus {
  running: boolean;
  authFile: string;
  current: {
    uid: string | null;
    nickname: string | null;
    email: string | null;
  } | null;
  appPath: string;
  version: string;
}

export interface OAuthStartResult {
  loginId: string;
  verificationUri: string;
  expiresIn: number;
  /** 本次登录会话所属区域；由后端回显，缺省视为国服。 */
  region?: AccountRegionKey;
}

export interface OAuthPollResult {
  done: boolean;
  result?: AccountMeta;
  error?: string;
}

/** 导出文件中的完整账号记录（含 token，仅导出命令返回；字段与账号库原始记录一致）。 */
export interface AccountRecord {
  id?: string;
  uid?: string | null;
  nickname?: string | null;
  email?: string | null;
  access_token?: string | null;
  refresh_token?: string | null;
  token_type?: string | null;
  domain?: string | null;
  expiresAt?: number | null;
  refreshExpiresAt?: number | null;
  auth_raw?: unknown;
  profile_raw?: unknown;
  createdAt?: number | null;
  [key: string]: unknown;
}

/** 导入文件账号的脱敏预览（不含 token）。 */
export interface ImportPreviewAccount {
  index: number;
  uid: string | null;
  nickname: string | null;
  email: string | null;
  hasToken: boolean;
}

/** 导入结果计数。 */
export interface ImportResult {
  ok: boolean;
  imported: number;
  skipped: number;
  overwritten: number;
}

/** 本机候选账号的来源类型。 */
export type LocalAccountSource = "current" | "snapshot" | "backup";

/** 本机候选账号的凭证可用性。 */
export type LocalAccountFreshness = "refreshable" | "access_only" | "expired";

/** 本机扫描发现的单个候选账号（不含 token）。 */
export interface LocalImportCandidate {
  /** 本次扫描结果中的序号。 */
  index: number;
  /** 来源文件绝对路径；导入时以此为准（跨扫描稳定）。 */
  path: string;
  /** 账号元数据（脱敏）。 */
  meta: AccountMeta;
  source: LocalAccountSource;
  /** 来源展示名：当前登录 / 历史快照 / 切换备份。 */
  sourceLabel: string;
  freshness: LocalAccountFreshness;
  /** 凭证可用性展示名。 */
  freshnessLabel: string;
  /** 同一账号在本机共有多少份文件（>1 表示还有更旧的重复快照）。 */
  duplicateCount: number;
  /** 是否已在账号库中。 */
  alreadyImported: boolean;
  /** 库中已有该账号，但本机这份凭证更新：导入会覆盖刷新。 */
  updatesStored: boolean;
  /** 来源文件最后修改时间（毫秒）。 */
  modifiedAt: number;
}

/** GET /api/import-local/scan 响应。 */
export interface LocalScanResult {
  ok: boolean;
  candidates: LocalImportCandidate[];
  total: number;
  /** 识别出的认证文件总数（含被去重掉的旧快照）。 */
  filesScanned: number;
  /** 可导入（凭证未完全过期）的候选数。 */
  usable: number;
  /** 认证文件目录。 */
  authDir: string;
  /** 本工具备份目录。 */
  backupDir: string;
}

/** POST /api/import-local/selected 响应。 */
export interface LocalImportResult {
  ok: boolean;
  imported: number;
  /** 新增账号数。 */
  added: number;
  /** 覆盖刷新既有账号数。 */
  updated: number;
  /** 逐个账号的结果明细。 */
  outcomes: Array<{
    name: string;
    region: string;
    source: LocalAccountSource;
    file: string;
    freshness: LocalAccountFreshness;
    updated: boolean;
  }>;
}

export interface Session {
  id: string;
  title: string;
  cwd: string;
  updatedAt: number;
  hasHistory: boolean;
  /** WorkBuddy playground（侧栏「任务」）；缺省视为空间会话。 */
  isPlayground?: boolean;
}

export interface CopyResult {
  id: string;
  newId: string;
  jsonlCopied: boolean;
  mappingWritten: boolean;
  backup: string;
}

export interface SwitchResult {
  ok: boolean;
  account: string;
  backup: string | null;
  sessionCopy?: {
    sourceUid: string;
    targetUid: string;
    copied: CopyResult[];
    errors?: { id: string; error: string }[];
  };
}

export interface CheckinConfig {
  enabled: boolean;
  /** Legacy persisted fields; accepted by the backend but ignored by scheduling. */
  start_hour?: number;
  end_hour?: number;
  keepalive_days: number;
  lazy_refresh_hours: number;
  /**
   * 历史字段：旧版本曾用 `"cn" | "all"` 控制覆盖区域。
   * 自动签到 / 自动旅行现已硬绑定为「仅国服」，后端忽略此字段，
   * 前端不再读取或写入，仅作为兼容旧配置文件保留类型定义。
   */
  region_scope?: "cn" | "all";
}

export interface CheckinLog {
  ts: number;
  accountId: string | null;
  email: string;
  result: string;
  error?: string;
}

export interface CheckinResult {
  result: string;
  error?: string;
}

export interface TravelConfig {
  enabled: boolean;
  /**
   * 历史字段：旧版本曾用 `"cn" | "all"` 控制覆盖区域。
   * 自动旅行现已硬绑定为「仅国服」，后端忽略此字段，
   * 前端不再读取或写入，仅作为兼容旧配置文件保留类型定义。
   */
  region_scope?: "cn" | "all";
}

export type TravelStatusLabel = "untraveled" | "no-buddy" | "traveling" | "finished" | "adopted" | "adopt-threshold";

export interface TravelStatus {
  label: TravelStatusLabel;
  rewardCredit: number | null;
  locationName?: string | null;
  arriveAt?: number | null;
  /** 后端给出的具体说明（如「领养需先积累对话轮次」），供卡片直接展示原因。 */
  message?: string | null;
  /** 跳过/结果原因，用于区分细分状态（adopt-threshold / no-buddy / daily-limit 等）。 */
  skip?: string | null;
}

export interface AutoRotateConfig {
  enabled: boolean;
  check_interval_minutes: number;
  cooldown_minutes: number;
  min_gap_hours: number;
  min_urgency_hours: number;
  active_guard_minutes: number;
  min_remaining_credits: number;
}

export interface RotateLog {
  ts: number;
  action: string;
  reason?: string | null;
  from?: { id: string; name?: string | null } | null;
  to?: { id: string; name?: string | null } | null;
}

export interface RotateStatus {
  config: AutoRotateConfig;
  cliConfigured: boolean;
  activeAccountId: string | null;
  activeAccountName: string | null;
  lastCheckAt: number | null;
  lastSwitchAt: number | null;
}

export interface CreditResource {
  packageCode: string | null;
  packageName: string | null;
  total: number;
  remaining: number;
  used: number;
  status: number | null;
  expireAt: number | null;
  expired: boolean;
  expiringSoon: boolean;
}

export interface CreditExpiry {
  ok: boolean;
  accountId?: string | null;
  accountName?: string;
  updatedAt?: number;
  totalCapacity?: number;
  totalRemaining?: number;
  expiringSoonRemaining?: number;
  expiredRemaining?: number;
  soonestExpireAt?: number | null;
  expiringSoon?: boolean;
  expired?: boolean;
  resources?: CreditResource[];
  error?: string;
}

export interface CreditStatsSummary {
  currentRemaining: number;
  currentCapacity: number;
  usageToday: number;
  usage7Days: number;
  usageThisMonth: number;
  todayCheckedInAccounts: number;
  todaySuccess: number;
  todayAlready: number;
  todayFailed: number;
}

export interface CreditStatsDailyPoint {
  date: string;
  usage: number;
  /** 官方用量按模型聚合（全量，不受请求明细条数限制）；本地观察口径下为空 */
  models?: { model: string; requestCount: number; credit: number }[];
}

/**
 * 免费模型判定（`model_billing.rs` 的 `WindowVerdicts`）。
 *
 * 三个窗口**各自独立**判定：某个号可能今天只跑了免费模型、本月早些时候
 * 跑过计费模型，合成一个结论会让今日的「免费」污染本月。
 */
export interface CreditStatsBilling {
  /** 今日窗口：`all_free`（确定免费）/ `has_paid`（有计费调用）/ `unknown`（不知道）。 */
  today: CreditBillingVerdict;
  sevenDays: CreditBillingVerdict;
  month: CreditBillingVerdict;
  /** 三个窗口是否**全部**判定为「只用免费模型」。 */
  allFree: boolean;
}

/**
 * 单个窗口的免费判定结果。
 *
 * `all_free` 是**确定**的（上游明确声明这些模型倍率为 0），因此消耗如实显示 0；
 * `unknown` 是**不知道**，界面必须显示「—」——把「不知道」说成 0 会谎报
 * 「这个号没消耗」，而它可能正在烧积分。
 */
export type CreditBillingVerdict = "all_free" | "has_paid" | "unknown";

export interface CreditStatsAccount {  accountId: string;
  accountName: string;
  isCurrent: boolean;
  currentRemaining: number | null;
  totalCapacity: number | null;
  lastSnapshotAt: number | null;
  usageToday: number;
  usage7Days: number;
  usageThisMonth: number;
  /**
   * 该账号「末次快照 vs 上次快照」的完整变化（汇总口径）；缺省兼容旧后端。
   *
   * 与上面三个 `usage*` 并存：`usage*` 是逐事件累加派生（明细），
   * `change` 是快照做差（可信总量）。总量展示优先用 `change`。
   */
  change?: CreditComparison;
  /**
   * 免费模型判定结果（缺省兼容旧后端）。
   *
   * 为什么界面需要它：`usage*` 为 0 有两种完全不同的来源 ——
   * 「确定免费」（免费模型的调用不扣积分）与「确实没消耗」，
   * 而缺数据时又是「不知道」（界面显示「—」）。只靠数字 0 分不出前两者，
   * 而这三者在界面上必须表达不同（本项目「—」= 不知道，0 = 确定的零）。
   *
   * 判定依据是上游 `/v3/config` 的 `credits` 计费倍率（经网关按区域透出），
   * 见 Rust 侧 `model_billing` 模块头部说明。
   */
  billing?: CreditStatsBilling;
  checkedInToday: boolean | null;
  checkinStatusToday: string | null;
  lastCheckinAt: number | null;
  lastCheckinResult: string | null;
  /** 按账号的逐日观察消耗（缺省兼容旧后端）；官方可用时趋势图优先使用官方 daily */
  daily?: CreditStatsDailyPoint[];
}

export interface CreditStatsUsageEvent {
  kind: "usage";
  ts: number;
  date: string;
  accountId: string;
  accountName: string;
  amount: number;
}

export interface CreditStatsCheckinEvent {
  kind: "checkin";
  ts: number;
  date: string;
  accountId: string | null;
  accountName: string;
  result: string;
  error?: string | null;
}

export type CreditStatsEvent = CreditStatsUsageEvent | CreditStatsCheckinEvent;

export type CreditOfficialUsageStatus = "complete" | "partial" | "unavailable";

export interface CreditOfficialUsageSummary {
  usageToday: number;
  usage7Days: number;
  usageThisMonth: number;
}

export interface CreditOfficialUsageModel {
  model: string;
  requestCount: number;
  credit: number;
}

export interface CreditOfficialUsageAccount {
  accountId: string;
  accountName: string;
  ok: boolean;
  requestCount: number;
  detailTruncated: boolean;
  usageToday: number | null;
  usage7Days: number | null;
  usageThisMonth: number | null;
  error?: string | null;
  reportedTotal?: number | null;
  fetchedCount?: number;
  /** 缺省兼容旧后端响应。 */
  models?: CreditOfficialUsageModel[];
  /** 按账号的逐日官方消耗（全量聚合，不受 requests 明细上限影响；缺省兼容旧后端） */
  daily?: CreditStatsDailyPoint[];
}

export interface CreditOfficialUsageRequest {
  accountId: string;
  accountName: string;
  requestId: string;
  credit: number;
  model: string;
  client: string;
  requestTime: string;
}

export interface CreditOfficialUsageError {
  accountId: string;
  accountName: string;
  error: string;
}

export interface CreditOfficialUsage {
  status: CreditOfficialUsageStatus;
  rangeStart: string;
  rangeEnd: string;
  /** 官方用量最近一次采集时间；缓存命中时保持采集当时的时间。 */
  collectedAt?: number;
  summary: CreditOfficialUsageSummary;
  daily: CreditStatsDailyPoint[];
  accounts: CreditOfficialUsageAccount[];
  requests: CreditOfficialUsageRequest[];
  /** 官方全部有效请求按模型汇总；不受 requests 明细上限影响。 */
  models?: CreditOfficialUsageModel[];
  detailLimitPerAccount: number;
  errors: CreditOfficialUsageError[];
}

/**
 * 与上一次积分快照做差的结果（**汇总口径**）。
 *
 * 为什么要有这一套而不是继续用 `summary.usageToday` 那几个字段：
 * 那几个是逐事件累加出来的（把每对相邻快照的下降量相加），它必须为每一笔
 * 扣减猜一个归属，一旦有一次读数不完整就会把「没读到的包」算成消耗 ——
 * 所有者实测到过 `-380` 紧跟 `+380` 的幻影配对。而
 * `本次余额 − 上次余额` 不需要猜归属，它天然免疫单次不可信读数。
 *
 * 两者**并存**：`comparison` 给可信的总量，`usageToday` 等继续供时间窗
 * 与明细视图。前端展示总量时优先用 `comparison`。
 *
 * 关键约定：`ok === false` 时必须显示「—」/「暂无对比基准」，**绝不能显示 0**。
 * 0 会被读成「这段时间没有消耗」，而事实是「我们不知道」。
 */
export interface CreditComparison {
  /** 是否有可信差值。false 时 decrease/increase/net 无意义（恒为 0）。 */
  ok: boolean;
  /**
   * 无可信差值的原因：
   * - `no_baseline`：还没有「上一次快照」（首次运行），或中间空档过大
   * - `unreliable`：读数不完整（上游返回的包列表缺失），差值测的是抖动
   * - `ok`：可信
   */
  reason: "ok" | "no_baseline" | "unreliable" | string;
  /** 差值区间起点（上次快照时刻）；无基准时为 null。 */
  fromTs: number | null;
  /** 差值区间终点（本次快照时刻）；无快照时为 null。 */
  toTs: number | null;
  /** 区间内余额下降总量（≥ 0）= 消耗。 */
  decrease: number;
  /** 区间内余额上升总量（≥ 0）= 发放/返还。与消耗分开表达。 */
  increase: number;
  /** 净变化 = increase − decrease。 */
  net: number;
  /**
   * 差值覆盖了几个账号。为 0 时界面必须显示「暂无对比基准」。
   * 只在统计接口的顶层 `comparison` 上出现。
   */
  accounts?: number;
  /**
   * 这份差值已不再反映「此刻」（末次快照距统计时刻太久）。
   *
   * 与 `ok` 正交：算得准的差值也可能陈旧。为 true 时界面应改用
   * 「数据截至 …」措辞，而不是丢弃数字或谎称最新。
   */
  stale?: boolean;
}

export interface CreditStatistics {
  generatedAt: number;
  retentionDays: number;
  coverageStartAt: number | null;
  summary: CreditStatsSummary;
  daily: CreditStatsDailyPoint[];
  accounts: CreditStatsAccount[];
  events: CreditStatsEvent[];
  /** 汇总口径：与上一次积分快照做差。缺省兼容旧后端。 */
  comparison?: CreditComparison;
  /** 官方接口不可用时仍使用上述本地观察字段；缺省兼容旧后端。 */
  officialUsage?: CreditOfficialUsage;
}

export interface TokenStatsTotals { total: number; input: number; output: number; cacheRead: number; cacheWrite: number; uncachedInput: number; records: number; cacheHitRate: number | null; }
export interface TokenStatsGroup extends TokenStatsTotals { key: string; title?: string | null; project?: string; sessionId?: string; }
export interface TokenStatsSource { source: "workbuddy" | "workbuddy-ai" | "codebuddy-cli" | "codebuddy-ide"; summary: TokenStatsTotals; models: TokenStatsGroup[]; projects: TokenStatsGroup[]; sessions: TokenStatsGroup[]; daily: TokenStatsGroup[]; /** Optional model-specific daily series for trend filtering. */ dailyByModel?: Record<string, TokenStatsGroup[]>; hours: TokenStatsGroup[]; filesScanned: number; parseErrors: number; coverageStartAt?: number | null; coverageEndAt?: number | null; }
export interface TokenStatistics { generatedAt: number; rangeDays?: number | null; sources: TokenStatsSource[]; }

export interface CodeBuddyCliStatus {
  configured: boolean;
  authMode?: "settings-env";
  environmentOverride?: boolean;
  settingsPresent: boolean;
  helperPresent: boolean;
  helperSupportsAccountIds: boolean;
  helperCurrent?: boolean;
  migrationRequired?: boolean;
  syncPending?: boolean;
  activeIndex: number | null;
  activeAccountId: string | null;
  activeAccountName: string | null;
  accountCount: number;
  statePath: string;
}

export interface CodeBuddyCliSwitchResult {
  ok: boolean;
  configured: boolean;
  synced: boolean;
  verified?: boolean;
  authMode?: "settings-env";
  activeIndex?: number;
  activeAccountId?: string;
  source?: string;
  skipped?: boolean;
  message?: string;
  error?: string;
}

export interface CodeBuddyCliInstallResult {
  ok: boolean;
  configured: boolean;
  helperPresent: boolean;
  helperSupportsAccountIds: boolean;
  verified?: boolean;
  authMode?: "settings-env";
  message?: string;
  error?: string;
}

/**
 * 代理的**适用范围**（三个独立开关）。
 *
 * 为什么把「一个代理地址」拆成三个开关：同一个地址对不同用途的收益完全不同 ——
 * GitHub（检查更新 / 下载安装包）在国内基本必须走代理；国际版上游
 *（workbuddy.ai）国内直连实测 wsarecv 超时，也需要；而国服上游
 *（codebuddy.cn / copilot.tencent.com）直连即通，绕进代理只会多一跳延迟、
 * 多一个故障面（代理一挂，本来好好的国服账号跟着不可用）。
 *
 * 地址仍然只填一次（用户不该填三遍），三个开关只决定「哪些用途使用它」。
 */
export interface ProxyScope {
  /** 检查更新与下载安装包时是否使用代理。默认开。 */
  github: boolean;
  /**
   * 国服账号（*.workbuddy.cn / *.codebuddy.cn / copilot.tencent.com）的上游请求是否使用代理。
   *
   * 默认关：国内直连通常更快；且关了之后是**真直连**（连 HTTPS_PROXY 也不用）。
   */
  cn: boolean;
  /**
   * 国际版账号（*.workbuddy.ai / *.codebuddy.ai）的上游请求是否使用代理。
   *
   * 默认开：国内直连实测不稳定（wsarecv 超时），不走代理基本用不了。
   */
  intl: boolean;
}

export interface GithubConfig {
  owner?: string;
  repo?: string;
  proxy?: string;
  /**
   * 三个开关。**允许缺失**：老配置 / 老后端里没有这个字段，
   * 读取方必须用 `proxyScopeOf()` 兜底成默认值，不能自己当 false 处理。
   */
  proxy_scope?: Partial<ProxyScope> | null;
}

export interface UpdateInfo {
  ok: boolean;
  current?: string;
  latest?: string;
  latestTag?: string;
  hasUpdate?: boolean;
  releaseName?: string;
  releaseUrl?: string;
  publishedAt?: string;
  error?: string;
  message?: string;
}

/** CodeBuddy CN IDE（桌面客户端）状态；与 CodeBuddy CLI 独立。 */
export interface CodeBuddyCnIdeStatus {
  installed: boolean;
  running: boolean;
  dataDir: string | null;
  dbPath: string | null;
  dbExists: boolean;
  appPath: string | null;
  activeAccountId: string | null;
  activeAccountName: string | null;
  detectedFrom?: string;
  statePath?: string;
}

export interface CodeBuddyCnIdeSwitchResult {
  ok: boolean;
  account: string;
  accountId: string;
  dbPath?: string;
  restarted?: boolean;
  message?: string;
}


// ---------------------------------------------------------------------------
// 网关（workbuddy2api）集成
// ---------------------------------------------------------------------------

/** 网关配置（持久化在 ~/.wb-switch/gateway/gateway_config.json）。 */
/* 网关工作模式：
 * balance —— 负载均衡（默认）：账号池加权随机选号，自动避开冷却/熔断账号
 * pinned  —— 指定账号：只使用 pinned_uid 对应的那一个账号
 * rotation —— 单一模型 + 积分轮转：只用一个账号烧到不可用，再换按到期日
 *             排序的下一个（仍优先烧最快过期的额度）                      */
/**
 * 网关工作模式。
 *
 * - `balance` 自动：全部（未禁用的）账号参与，池内加权随机 + 到期日分层
 * - `manual`  手动：只使用勾选的账号，池内仍自动均衡
 * - `rotation` 积分轮转：单一模型烧号，按到期日换下一个
 *
 * `pinned` 是历史值，读作 `manual`（后端 `GatewayMode::from_str` 已兼容）。
 */
export type GatewayMode = "balance" | "manual" | "rotation";

export interface GatewayConfig {
  /** 是否已启用（启动过即为 true）。 */
  enabled: boolean;
  /** 网关工作模式。 */
  mode?: GatewayMode;
  /** 手动模式下勾选的账号 uid 列表（可多选）。 */
  manual_uids?: string[];
  /** 指定账号模式下锁定的账号 uid（旧字段，仅向后兼容）。 */
  pinned_uid?: string | null;
  /**
   * 「限制使用的模型」白名单（多选）。**空数组 = 不限制（默认，全部放行）**。
   *
   * 非空时网关**只放行名单内的模型**，其余一律 400 model_not_allowed。
   * **三个工作模式（自动 / 手动 / 积分轮转）都生效** —— 它限制的是「放行哪些
   * 模型」，与「用哪些账号」是正交的两件事。
   *
   * 联合类型里的 `string` 是**向后兼容**，不是冗余：老配置里这个键是单值字符串
   *（实测所有者本机的 gateway_config.json 就是 `"allowed_model":
   * "deepseek-v4.1-flash"`）。声明成 `string[]` 会让读取方以为可以直接
   * `.length` / `.map`，在老配置上运行时炸掉。
   */
  allowed_model?: string[] | string | null;
  /** 服务端口（权威字段，前端口选择器直接编辑它）。 */
  port: number;
  /** 监听地址，由 port 派生，如 ":7863"。 */
  listen: string;
  /** OpenAI 兼容接口的鉴权密钥；空 = 不鉴权。 */
  api_key: string;
  /** 随 App 启动而自动拉起。 */
  auto_start: boolean;
  last_status?: string | null;
  last_error?: string | null;

  // ---- 自动养号任务排程（写进网关 config.json 的 schedule 块）----
  //
  // 这些字段由宿主读取后转写到网关的 native config；网关只认它自己的 config.json，
  // 因此改这里必须重启网关才会生效。

  /** 活跃上报时点（小时列表，默认 [10]）：点亮连登天数并解锁领养前置。 */
  activity_hours?: number[];
  /** 夜猫子任务时点（默认 [1]）：仅在 23:00–08:00 北京时间内计入。 */
  nightowl_hours?: number[];
  /** 开学季活动任务时点（默认 [12]）：限时活动，只领取已达标的奖励。 */
  school_hours?: number[];
  /** 国际版 trial 加油包领取时点（默认 [9, 21]，仅国际版账号）。 */
  trial_hours?: number[];
  activity_enabled?: boolean;
  nightowl_enabled?: boolean;
  school_enabled?: boolean;
  trial_enabled?: boolean;
  /** 每号每日活跃上报条数（默认 3，上限 20）。 */
  activity_report_count?: number;

  // ---- 自定义系统提示词（写进网关 config.json 的 prompt 块）----
  //
  // 由宿主转写到网关 native config 的 prompt 块（Go 侧 Config.Prompt）。
  // 与 features.sanitize_blacklist_fingerprints 是**两层叠加、互不替代**：
  // 那个清洗消息里的指纹串，这个把 system/developer 消息整体替换。

  /**
   * 提示词模式；缺省 `"passthrough"` = 透传客户端原始 system（既有行为不变）。
   *
   * `"custom"` = 用网关自有提示词替换客户端的 system/developer 消息。
   * 缺省刻意不是 custom：老配置没有这个键，若缺省 custom，既有用户升级后
   * system 会被静默替换（人设、项目约定、工具说明全丢）。
   */
  prompt_mode?: "passthrough" | "custom";
  /** 自定义提示词文件路径；空 = 用网关内置默认提示词。 */
  prompt_file?: string;
}

/** 手动触发养号任务的结果（POST /api/gateway/task-run）。 */
export interface GatewayTaskRunResult {
  ok: boolean;
  /** 是否真的执行了一轮；false = 被前置条件挡下（见 skip / message）。 */
  ran: boolean;
  /**
   * 跳过原因码；`ran=true` 时为空。
   *
   * - `outside_window`：不在夜猫子时段（23:00–08:00 北京时间）
   * - `already_running`：该任务上一轮还在执行
   */
  skip?: string | null;
  /** 面向用户的中文说明，可直接显示。 */
  message: string;
  /** 调用失败的原因（网关未启动、任务名不认识等）；成功时为 null。 */
  error?: string | null;
}

/**
 * 养号任务标识（与 Go 网关 `/tasks/run` 的 task 参数一一对应）。
 *
 * `school_season`（校园日）**不在**这里 —— 它是成长任务（走 `/tasks/growth`，
 * 见 `runGrowthTask`）：完成条件是「小程序内对话」，需要专门的小程序指纹上报，
 * 与调度器那几个遍历式任务不是一套实现。界面上两者并列显示，
 * 但调用的是不同接口。
 */
export type GatewayTaskName = "activity" | "nightowl" | "school" | "trial" | "growthmap";

/**
 * 成长任务的展示状态（Go 侧 growtask 的 View* 常量）。
 *
 * 与养号任务的 `ran/skip` 不同，成长任务是**逐任务**的结果，
 * 因此每个任务自己带状态与进度。
 */
export type GrowthTaskStatus =
  | "claimable"
  | "in_progress"
  | "not_accepted"
  | "accepted"
  | "claimed"
  | "unsupported"
  | "locked";

/** 成长任务列表里的一项（action=list）。 */
export interface GrowthTaskView {
  task_code: string;
  title?: string;
  /** 客户端操作指引（多用于 `unsupported` 的任务）。 */
  description?: string;
  /** 达成条件简述。 */
  task_desc?: string;
  /** 奖励积分 / 能量。 */
  credit?: number;
  energy?: number;
  status: GrowthTaskStatus;
  /** 状态的中文说明，界面可直接显示。 */
  status_text: string;
  accept_status?: string;
  /** 上游是否下发了进度。**未报名时为 false**（progress 为 null）。 */
  has_progress?: boolean;
  /** "当前/目标"，如 "3/5"；无进度时为空。 */
  progress?: string;
  claimable?: boolean;
  /** 能否被本工具自动完成。false 时只展示指引，不提供「一键完成」。 */
  automatable?: boolean;
  /** 该项需要**真实对话**才能推进（会消耗 token 与额度），界面应提示。 */
  needs_chat?: boolean;
  /** 本工具会执行什么动作（中文说明）。 */
  action_desc?: string;
  /** 无法自动完成时的原因说明。 */
  hint?: string;
}

/** 单项任务的执行结果（action=run）。 */
export interface GrowthTaskItemResult {
  task_code: string;
  title?: string;
  desc?: string;
  status: "done" | "skipped" | "error" | "unsupported";
  message: string;
  /** 动作前后进度（"当前/目标"）——「上报 200 ≠ 计分」的证据。 */
  progress_before?: string;
  progress_after?: string;
  claimed?: boolean;
  credit?: number;
  energy?: number;
  claim_error?: string;
}

/**
 * 成长任务「一键完成」的返回。
 *
 * 三种 action 的返回形状不同（list 带 tasks / run 带 items / run-all 带 results），
 * 故这里是**联合形状**而非各自独立的类型 —— 界面按 action 取用对应字段。
 * 失败时 `ok=false` + `error`，且**不抛 HTTP 错误**：部分成功也要能拿到已完成的部分。
 */
export interface GrowthTaskResult {
  ok: boolean;
  error?: string;
  /** action=list 时返回。 */
  accountId?: string;
  tasks?: GrowthTaskView[];
  total?: number;
  /** action=run（未指定 taskCode）时返回。 */
  items?: GrowthTaskItemResult[];
  /** action=run（指定 taskCode）时返回。 */
  item?: GrowthTaskItemResult;
  /** action=run-all 时返回。 */
  results?: Array<{
    uid: string;
    realm: string;
    skipped?: boolean;
    skip_reason?: string;
    items?: GrowthTaskItemResult[];
    claimed?: number;
    credit?: number;
    energy?: number;
    error?: string;
  }>;
  summary?: {
    accounts?: number;
    claimed?: number;
    credit?: number;
    energy?: number;
  };
}

/** 单个「账号+模型」的冷却记录（来自网关 /status 的 model_cooling）。 */
export interface GatewayModelCooling {
  /** 被限流的模型名。 */
  model: string;
  /** 冷却截止时刻（ISO 8601）。 */
  until?: string;
  /** 距到期的剩余秒数（后端已算好，避免前后端时钟偏差）。 */
  remaining_sec?: number;
  /** 面向用户的说明文案（含模型名与重置时间）。 */
  reason?: string;
  /** true = 到期时间取自上游报错文案；false = 解析失败，回退固定软冷却。 */
  reset_at_parsed?: boolean;
}

/** 网关账号池中的单个账号运行态（来自网关 /status）。 */
export interface GatewayPoolAccount {
  uid: string;
  nickname?: string;
  /**
   * 用户自己在「WorkBuddy 账号」里填的备注（如「公司号」「备用」）。
   *
   * **不是网关下发的**：网关根本不知道备注，池里原本只有 `nickname`。
   * 宿主在 `gateway_status()` 里用本地账号库把它**合并**进池快照
   * （Rust 侧 `merge_account_notes`），所以这里能拿到。
   *
   * 界面取名口径是 **备注 → 昵称 → uid 前缀**（见 `accountLabel`）：
   * 上游昵称对国服账号常为空，uid 又是一串随机串，备注才是分辨
   * 「这是谁的号」的唯一可靠线索。缺省 = 没填（回退到昵称）。
   */
  note?: string;
  credits?: number;
  cooling?: boolean;
  cool_kind?: string;
  cool_remaining_sec?: number;
  /**
   * 网关判定该账号已不可用（连续 3 次 session 死 / 额度冻结）。
   *
   * 与 `no_route` 是**两件事**，必须分开看：
   *   disabled → 该号已死，**养号任务也会跳过它**（跑了也白跑）
   *   no_route → 用户手动关的，**养号任务照跑**，只是不接请求
   * 早先两者在前端都显示成「已禁用」，用户无法判断账号还在不在养。
   */
  disabled?: boolean;
  /**
   * 用户手动标记「不接流量」（Go 侧 `pool.Status.NoRoute`）。
   *
   * 来自凭证里的 `account.no_route`（宿主导出时按账号库的禁用标记写入）。
   * 语义：**只不接流量，养号照跑** —— 这正是「禁用」对用户应有的含义。
   * 凭证仍然存在于网关池里，所以签到/活跃上报/成长任务都能遍历到它。
   */
  no_route?: boolean;
  reason?: string;
  success_count?: number;
  err_total?: number;
  in_flight?: number;
  /**
   * 最近一次成功调用的时刻（ISO 8601；Go 侧 `pool.Status.LastSuccessTime`）。
   *
   * 与 `success_count` 的分工：计数回答「一共成了多少次」，本字段回答「上一次成
   * 是什么时候」—— 后者才能区分「一直在稳定成功」与「早就不再被选中了」
   * （计数是个只增不减的累计值，看不出停滞）。
   */
  last_success?: string;
  /** 最近一次失败的时刻（ISO 8601）；供「最近成功」旁证用。 */
  last_err?: string;
  /** 连续失败计数（熔断器输入；达到阈值即熔断）。 */
  breaker_fails?: number;
  /** 熔断截止时刻（ISO 8601）；非空且未过期 = 正在熔断期。 */
  breaker_until?: string;
  /** 「最近到期积分」的到期时刻（Unix 秒）；缺省 = 未知。 */
  soonest_expire_at?: number;
  /** 到期日（YYYY-MM-DD），即选号分层档位键；同一天的账号同级。 */
  expire_day?: string;
  /**
   * 是否正因「到期档位更晚」而排队等待（当前轮不到它）。
   *
   * 由网关按与选号**完全相同**的档位口径算出。语义是「现在轮不到」，
   * **不是故障** —— 前面档位被消耗或冷却后会自动进入路由。
   */
  queued?: boolean;
  /**
   * 该账号当前因「模型级限流」而冷却的模型（按到期时间升序）。
   *
   * 与 `cooling` 的区别（界面据此区分两种冷却）：
   * - `cooling` = 账号级：余额（积分）欠费或账号被限速，整号不可用
   * - `model_cooling` 非空 = 模型级：仅这些模型不可用，换模型仍可用
   * 两者可同时存在。
   */
  model_cooling?: GatewayModelCooling[];

  /**
   * 该账号属于**哪个客户端产品**：`workbuddy` / `qoder` / `zcode`。
   *
   * # 为什么需要它（所有者 2026-09-20 要求）
   *
   * 原话：「在兼容网关哪里的账号池,也要标记上进入池子的账号属于那个客户端」。
   *
   * 三个产品的账号混在**同一个池**里（多产品路由开启时），而池表上
   * 只有昵称/备注 —— 用户看到 `wish`、`aliyun-…` 这样的名字，
   * **判断不出它来自哪个客户端**。于是：
   *
   *   · 排查「`zcode:` 前缀为什么选不出号」时，看不出池里有几个 ZCode 号
   *   · 想单独给某个号停流量，得先去别的页面确认它是哪家的
   *
   * ⚠ 网关侧已把空 `Product` **归一成 `workbuddy`**（Go 的 `productOf()`），
   * 故这里不必再兜底空串；但仍标 `?` 以兼容**老网关**（不返回该字段时
   * 界面应显示"未知"而不是错标成 WorkBuddy —— 错标比不标更误导）。
   */
  product?: string;
}

/** 网关 /status 响应。 */
export interface GatewayPool {
  accounts?: GatewayPoolAccount[];
  total?: number;
  healthy?: number;
  cooling?: number;
  disabled?: number;
  in_flight_full?: number;
  sticky_sessions?: number;
  redis_mode?: string;
}

/** 网关状态里「可选账号」一项（手动模式勾选列表的数据源）。 */
export interface GatewayStatusAccount {
  uid: string;
  nickname?: string;
  /**
   * 用户自己在「WorkBuddy 账号」里填的备注。
   *
   * 界面取名口径是 **备注 → 昵称 → uid 前缀**（见 `accountLabel`）：
   * 上游昵称对国服账号常为空，uid 又是一串随机串，备注才是分辨
   * 「这是谁的号」的唯一可靠线索。缺省 = 没填（回退到昵称）。
   */
  note?: string;
  expiresAt?: number;
  needsRelogin?: boolean;
  /** 用户手动禁用：勾选列表里不再展示（勾了也不会进池）。 */
  disabled?: boolean;
}

/** 网关综合状态。 */
export interface GatewayStatus {
  running: boolean;
  reachable: boolean;
  base: string;
  openaiBase: string;
  port: number;
  exePath: string | null;
  exeFound: boolean;
  /** 网关来源：embedded=内嵌在单个 exe 内 / env=环境变量指定 / external=外部文件。 */
  exeSource?: "embedded" | "env" | "external";
  /** 配置端口当前是否空闲（网关运行时该端口被自己占用，属正常）。 */
  portAvailable?: boolean;
  /** 当前工作模式。 */
  mode?: GatewayMode;
  /** 指定账号模式锁定的 uid。 */
  pinnedUid?: string | null;
  /** 可选账号列表（供手动模式的勾选列表使用）。 */
  accounts?: GatewayStatusAccount[];
  /**
   * 因「需重新登录」而被排除出网关账号池的账号。
   *
   * 这些账号的 refresh token 已被服务端拒绝，继续留在池里只会每次请求白跑一轮，
   * 因此同步时不会写入网关凭证目录；重新登录成功后会自动恢复。
   */
  excludedAccounts?: Array<{
    uid: string;
    nickname?: string;
    /**
     * 备注，与账号池/勾选列表同一取名口径（**备注 → 昵称 → uid 前缀**）。
     * 界面提示里用它标识账号，缺了会退到 uid（用户认不出是哪个号）。
     */
    note?: string;
    reason?: string | null;
  }>;
  authDir: string;
  accountsInLibrary: number;
  config: GatewayConfig;
  /**
   * 正在执行的养号任务（含进度）。
   *
   * 所有者明确要求「账号卡片上要能看到正在执行的任务」—— 此前点「立即执行」
   * 只有一个按钮转圈，看不到在跑什么、跑到哪、哪些账号在跑。
   */
  taskRuntime?: GatewayTaskRuntime;
  health: { reachable?: boolean; healthy?: boolean; detail?: unknown } | null;
  pool: GatewayPool | null;
}

/**
 * 正在执行的养号任务的运行态（来自 `GET /api/gateway/status` 的 `taskRuntime`）。
 *
 * 进度是**近似值**，口径如下（见 Rust 侧 `task_runtime`）：
 *   - `total` 是按账号库 + 任务区域规则算出的**预计**账号数；
 *   - `processed` / `processedIds` 来自统一事件流的**实际**已记录账号。
 * 两者可能短暂不等（网关账号池与账号库有极小时差），因此文案写成
 * 「已记录 N / M」而不是断言性的「已完成」。
 */
export interface GatewayTaskRuntime {
  /** false = 当前没有任务在跑；此时其余字段不保证存在。 */
  running: boolean;
  /** 任务标识（与 `GatewayTaskName` 对应）。 */
  task?: GatewayTaskName;
  /** 面向用户的任务中文名，可直接显示。 */
  label?: string;
  /** 开始时刻（毫秒时间戳）。 */
  startedAt?: number;
  /** 已运行毫秒数（后端算好，避免前后端时钟偏差）。 */
  elapsedMs?: number;
  /** 本轮预计遍历的账号数（进度分母）。 */
  total?: number;
  /** 已留下记录的账号数（进度分子）。 */
  processed?: number;
  /**
   * 已留下记录的账号在**宿主账号库里的 id**（不是网关 uid）。
   * 供账号卡片标记「这个号正在跑」。
   */
  processedIds?: string[];
}

/**
 * 账号卡片上的「本轮已跑」标记（由 `AccountsPage` 从 `taskRuntime` 推导后下发）。
 *
 * 为什么措辞是「已跑」而不是「正在跑」：后端只透出 `processedIds` —— 它是
 * **已经留下记录**的账号集合（见 Rust 侧 `task_runtime`），而 Go 侧记录是在
 * **处理完一个账号之后**才写（`scheduler/activity.go` 等）。也就是说，
 * 本轮**当前正在处理**的那个号还没进集合，后端也没有「当前是哪个号」这个字段。
 * 因此界面照实说「本轮已跑」，不编造一个后端并不提供的「正在跑这个号」。
 */
export interface AccountRunningTask {
  /** 任务中文名（取自 `taskRuntime.label`，如「活跃上报」）。 */
  label: string;
  /** 本轮已留下记录的账号数（分子）。近似值，口径见卡片悬停提示。 */
  processed?: number;
  /** 本轮预计遍历的账号数（分母）。 */
  total?: number;
}

/** GET /api/gateway/config 响应。 */
export interface GatewayConfigResult {
  config: GatewayConfig;
  exeFound: boolean;
  exePath: string | null;
  authDir: string;
}

/** POST /api/gateway/{start,restart} 响应。 */
export interface GatewayStartResult {
  started?: boolean;
  base?: string;
  port?: number;
  accounts?: number;
  health?: unknown;
}

/** POST /api/gateway/sync 响应。 */
export interface GatewaySyncResult {
  ok: boolean;
  accounts?: number;
  changed?: string[];
  updatedFromGateway?: string[];
  reloaded?: boolean;
  error?: string;
}

/** POST /api/gateway/mode 响应（切换模式并立即生效）。 */
export interface GatewayModeSwitchResult {
  ok: boolean;
  mode?: GatewayMode;
  pinnedUid?: string | null;
  /** 重导出后的账号数。 */
  accounts?: number;
  changed?: string[];
  /** 是否因模式变更重启了网关（未运行时为 false）。 */
  reloaded?: boolean;
  config?: GatewayConfig;
  error?: string;
}

/** POST /api/gateway/port-check 响应。 */
export interface GatewayPortCheck {
  port: number;
  available: boolean;
  /** 是否为 1024 以下的特权端口。 */
  reserved: boolean;
  /** 该端口当前是否被本网关自身占用。 */
  inUseByGateway: boolean;
  /** 端口被占用时给出的可用建议端口。 */
  suggest: number | null;
  /** 占用该端口的进程；查不到时为 null（权限不足或进程已退出）。 */
  holder: GatewayPortHolder | null;
}

/** 占用端口的进程信息。 */
export interface GatewayPortHolder {
  pid: number;
  name: string;
  /** 可执行文件完整路径；权限不足时为空串。 */
  path: string;
  /**
   * 是否为本项目自己的进程（网关 / 宿主 GUI）。
   *
   * 前端据此调整提示措辞：清理自己的旧进程是常见操作，
   * 而结束第三方进程需要更强的警告。
   */
  ours: boolean;
}

/**
 * 查询端口占用者的响应。
 *
 * `hint` 是查不到占用者时给用户的**可操作**排查命令（由后端按自身所在系统
 * 生成，例如 macOS 给 `sudo lsof -nP -iTCP:<port> -sTCP:LISTEN`）。
 * 为什么不由前端按 UA 拼：WebUI 模式下浏览器与后端可能不在同一台机器上，
 * 决定「用哪条命令」的是后端所在的系统。
 */
export interface GatewayPortHolderResult {
  port: number;
  holder: GatewayPortHolder | null;
  /** 查不到占用者时展示的排查提示；后端始终会填。 */
  hint?: string;
}

/** 网关 Token 用量中的一组计量（口径与本地 Token 统计页一致）。 */
export interface GatewayUsageTotals {
  /** input + output + cacheWrite（不含 cacheRead，避免重复计数）。 */
  total: number;
  /** 输入 token，已包含缓存读取。 */
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
  uncachedInput: number;
  /** 计入统计的成功请求数。 */
  records: number;
  /** 缓存命中率 = cacheRead / input；无输入时为 null。 */
  cacheHitRate: number | null;
}

/** 带分组键的用量（模型名 / 账号 uid / 日期）。 */
export interface GatewayUsageGroup extends GatewayUsageTotals {
  key: string;
}

/** 网关 /usage 响应（enabled=false 表示该网关未启用统计）。 */
export interface GatewayUsageSnapshot {
  enabled: boolean;
  generatedAt: number;
  /** 统计范围（近 N 天）；null = 全部历史。 */
  rangeDays?: number | null;
  summary?: GatewayUsageTotals;
  models?: GatewayUsageGroup[];
  accounts?: GatewayUsageGroup[];
  /**
   * 账号 → 该账号用过的模型明细（键 = 池 uid）。
   *
   * 为什么不能由 `models` 与 `accounts` 前端现算：这两个维度各自聚合后，
   * 交叉关系已经丢失 —— 只知道「甲账号共 3 万」「glm-5.2 共 4 万」，
   * 无法还原「甲账号的 glm-5.2 用了多少」。由网关侧记录时直接累计，
   * 界面「按账号筛选看用了哪些模型」才有可信数据。
   *
   * 缺失（老版本网关）时按「无明细」处理，不回退到假的交叉结果。
   */
  accountModels?: Record<string, GatewayUsageGroup[]>;
  /** 按日期升序的日聚合。 */
  daily?: GatewayUsageGroup[];
  dailyByModel?: Record<string, GatewayUsageGroup[]>;
}

/** get_gateway_usage 的统一响应：网关不可达时 usage 为 null 且带 error。 */
export interface GatewayUsageResult {
  running: boolean;
  reachable: boolean;
  usage: GatewayUsageSnapshot | null;
  error: string | null;
}

// ---------------------------------------------------------------------------
// 智能体客户端一键导入（agent_import）
// ---------------------------------------------------------------------------

/** 单个 AI 客户端的探测状态。 */
export interface AgentClientTarget {
  /** 客户端标识：dsh / claude-code / claude-desktop / codex */
  id: string;
  /** 显示名称 */
  label: string;
  /** 是否检测到已安装 */
  installed: boolean;
  /** 是否已接入本网关 */
  configured: boolean;
  /** 主要配置文件的绝对路径 */
  configPath: string;
  /** 补充说明信息 */
  note: string;
  /** 探测到的版本号 */
  version?: string | null;
}

/** GET /api/gateway/agents 探测响应。 */
export interface AgentDetectionResult {
  /** 本机网关根地址，如 http://127.0.0.1:7863 */
  base: string;
  /** 网关配置中是否已设置 API Key */
  hasApiKey: boolean;
  /** 探测到的客户端列表 */
  targets: AgentClientTarget[];
}

/**
 * 网关模型项（`GET /v1/models` 的一条，由 Rust `fetch_models` 原样透传）。
 *
 * 能力字段**全部可选**，且可选性本身就是语义的一部分：网关只在有真值时才下发
 * 某个键（见 go-gateway/internal/server/capability.go 的
 * `modelCapabilityFields` / `modelReasoningFields`）。因此
 * `字段缺失` = 上游未声明（不知道）≠ `false` / `[]` = 上游明确否定。
 * 前端必须把这两种情况渲染成**不同**的表达，否则「不知道」会被谎报成「不支持」。
 *
 * 同一语义的字段在响应里有**多种拼写**（各客户端解析器读的键名不统一），
 * 因此下面按「主拼写 + 容错拼写」成对声明，取用时优先主拼写。
 */
export interface GatewayModelItem {
  id: string;
  name?: string;
  /** 上下文窗口（token 数）。 */
  context_length?: number;
  max_output_tokens?: number;
  owned_by?: string;
  /** 是否支持图片输入（三态：缺失 = 未声明）。 */
  supportsImages?: boolean;
  input_modalities?: string[];
  inputModalities?: string[];
  capabilities?: {
    vision?: boolean;
    supports?: { vision?: boolean };
  };
  architecture?: {
    input_modalities?: string[];
    modality?: string;
  };
  /**
   * 该模型**支持思考**（上游声明；缺失 = 未声明，**不是** false）。
   *
   * 与 supported_efforts 正交：未声明档位范围的模型这里是 true，
   * 但它的 supported_efforts 来自标准阶梯而非上游逐模型声明。
   */
  supports_reasoning?: boolean;
  supportsReasoning?: boolean;
  /**
   * 该模型的档位**可选范围未声明**（上游没列 supportedEfforts，但有默认档）。
   *
   * ⚠ 语义是「不知道确切范围」，**不是**「不可选」。实测这类模型接受整个
   * 标准阶梯且档位真的生效（deepseek-v4.1-flash 推理长度单调递增
   * low 397 → medium 420 → high 646 → max 776），所以界面应鼓励尝试。
   */
  reasoning_range_undeclared?: boolean;
  reasoningRangeUndeclared?: boolean;
  /**
   * 该模型是**固定单档**。
   *
   * @deprecated 实测（2026-09-18）并不存在「档位不可选」的模型；上一版据此
   * 标成固定是错的（会让用户以为调档没用）。保留字段只为兼容旧网关；
   * 新代码一律用 reasoning_range_undeclared。
   */
  reasoning_fixed?: boolean;
  reasoningFixed?: boolean;
  /** 是否允许关闭思考。false 时不能传 off（上游会拒）。 */
  can_disable_thinking?: boolean;
  canDisableThinking?: boolean;
  /** 支持的思考档位（主拼写）。缺失或空数组 = 未声明。 */
  supported_efforts?: string[];
  supportedEfforts?: string[];
  reasoning_efforts?: string[];
  reasoningEfforts?: string[];
  reasoning?: {
    supported_efforts?: string[];
    default_effort?: string;
    /** 档位范围未声明（与顶层 reasoning_range_undeclared 同义）。 */
    range_undeclared?: boolean;
    /** @deprecated 见顶层 reasoning_fixed。 */
    fixed?: boolean;
    supports_reasoning?: boolean;
  };
  /** 默认思考档（缺失 = 未声明；**不要**用档位列表首项猜）。 */
  default_effort?: string;
  defaultEffort?: string;
  default_reasoning_effort?: string;
  /**
   * 该模型名**确有真值**的上游区域（`"cn"` / `"intl"`）。
   *
   * 只在单区可用时下发。「只在一侧存在」**不等于**「不支持图片」——
   * 网关会按区域把请求路由到它所在的那一侧，所以能力照常展示，
   * 这个字段只用来解释 `11102 model service info not found`。
   */
  supported_regions?: string[];
  /**
   * 该模型**未验证**的区域 —— 那些区域这一轮**没拿到真值**
   *（无可用账号 / 上游暂时不可达），故无法确认该区是否也有此模型。
   *
   * # ⚠ 与 `supported_regions` 的区别（两者含义相反）
   *
   *	supported_regions   「**确认**该区有这个模型」
   *	unverified_regions  「**不知道**该区有没有」——既不是有，也不是没有
   *
   * 前端用它做一件事：**判断信息是否完整**。只要还有 unverified，
   * 界面就该自动重试到补齐为止 —— 所有者 2026-09-20：
   * 「这应该是自动的,而不是需要人手动同步」。
   */
  unverified_regions?: string[];
  /** 面向人的区域说明（网关生成，客户端不读时至少人能看见）。 */
  region_note?: string;
  /**
   * 该模型**来自哪些平台**（网关生成）。
   *
   * 使用者的需求：「哪里显示出来的模型，现在可以加一个渠道，是来自于哪个
   * 平台，如果重叠，就显示多个平台」。
   *
   * 数组而非单值 —— 同一个模型名可能同时由多个平台提供（例如 `glm-5.3`
   * 既在 ZCode 套餐里、也在 WorkBuddy 的清单里），此时数组有多个元素。
   *
   * 旧网关不带这个字段（缺省 = 未声明），界面按"未声明"处理而不是
   * 当成"没有平台"——后者会给每个模型标一个错误来源。
   */
  channels?: ModelChannel[];
  /**
   * 该模型的**带前缀组合名**（网关生成），如
   * `["qoder:qwen3.8-flash", "qoder:国际版:qwen3.8-flash"]`。
   *
   * # 它解决什么（所有者 2026-09-20 反馈）
   *
   * 原话：「我通过 models 接口 并没有返回 平台:国际:模型名、平台:模型名，
   * 这两个组合的模型名，只有单独的 模型名」。
   *
   * 前缀是用户**指定平台/区域的唯一手段**。只给裸模型名时，用户想
   * "在 Qoder 上跑某模型"就没法表达，只能写裸名 —— 而裸名走
   * "哪个账号可用就用哪个"，会误路由（实测 `deepseek-v4.1-flash`
   * 被路由到 Qoder 账号并失败两次）。
   *
   * # 为什么由网关下发而不是前端自己拼
   *
   * 只有网关知道**该产品确实有账号的区域**。前端若按 `channels[].regions`
   * 自行拼，会造出 `qoder:国服:xxx` 这类没有账号可用的名字，
   * 用户选中后得到"账号不可用"，比不显示更糟。
   *
   * 旧网关不带这个字段（缺省 = 未声明），前端退回本地按 channels 推导 ——
   * 那是向后兼容路径，不是主路径。
   */
  aliases?: string[];
}

/** 模型的一个来源平台。 */
export interface ModelChannel {
  /** 稳定标识：`workbuddy` / `qoder` / `zcode`。用于过滤与分组。 */
  product: string;
  /** 显示名：`WorkBuddy` / `Qoder` / `ZCode`。界面直接显示。 */
  label: string;
  /** 该平台在哪些区域提供此模型（可能缺省）。 */
  regions?: string[];
}

/** POST /api/gateway/agents/import 接入响应。 */
export interface AgentImportResult {
  ok: boolean;
  target: string;
  backupDir: string;
  files: string[];
  models?: string[];
  model?: string;
}

/** 批量接入/一键更新响应。 */
export interface AgentBatchImportResult {
  ok: boolean;
  count: number;
  outcomes: Array<{
    target: string;
    backupDir: string;
    files: string[];
    models?: string[];
  }>;
  models: string[];
}

/** POST /api/gateway/agents/restore 恢复响应。 */
export interface AgentRestoreResult {
  ok: boolean;
  restored: number;
  backupId: string;
}

/** 备份记录项。 */
export interface AgentBackupItem {
  id: string;
  createdAt: number;
  path: string;
}

