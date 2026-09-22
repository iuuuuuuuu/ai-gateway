import type {
  AccountMeta, AgentBackupItem, AgentClientTarget, AgentDetectionResult, AppStatus, AutoRotateConfig,
  CheckinConfig, CheckinLog,
  CliQuotaAccount, CliQuotaStatusItem,
  CodeBuddyCliStatus, CodeBuddyCliSwitchResult, CreditExpiry, CreditOfficialUsageModel, CreditStatistics,
  GatewayModelItem, GatewayStatus, GatewayUsageGroup, GatewayUsageResult,
  GithubConfig, RotateLog, RotateStatus, TokenStatistics, TokenStatsGroup, TokenStatsSource, TokenStatsTotals,
  TravelConfig, TravelStatus,
} from "./types";
import { demoModeEnabled } from "./demo-mode";
// 只导入类型：`import type` 在编译期被完全擦除，不会与 api.ts 形成运行时循环引用。
// 加这组注解的目的是让「假数据形状与真实返回形状不一致」变成编译错误 ——
// screenshotDemoResponse 的返回值被 `as T` 强转，不注解的话 tsc 查不出任何偏差。
import type {
  AppEnvStatus, ProxyLogList, ProxyLogsOverview, TaskStatusItem,
  TraeCheckinTrends, TraeCreditDetail, TraeUsageHistory,
} from "./api";

export const screenshotDemoEnabled = demoModeEnabled;

const MODEL_NAMES = ["deepseek-v4-flash", "kimi-k3-1", "deepseek-v4-pro", "glm-5.2", "hy3"] as const;

interface ModelSeed {
  model: (typeof MODEL_NAMES)[number];
  requestCount: number;
  credit: number;
}

interface AccountUsageSeed {
  requestCount: number;
  models: ModelSeed[];
}

const accounts: AccountMeta[] = [
  { id: "demo-account-a", uid: "demo-user-001", email: "test-a@example.com", nickname: "测试 A", enterpriseName: "Demo Workspace", expiresAt: 0, refreshExpiresAt: 0, refreshedAt: 0, createdAt: 0, needsRelogin: false, needsReloginReason: null },
  { id: "demo-account-b", uid: "demo-user-002", email: "test-b@example.com", nickname: "测试 B", enterpriseName: "Demo Workspace", expiresAt: 0, refreshExpiresAt: 0, refreshedAt: 0, createdAt: 0, needsRelogin: false, needsReloginReason: null },
  { id: "demo-account-c", uid: "demo-user-003", email: "test-c@example.com", nickname: "测试 C", enterpriseName: "Demo Workspace", expiresAt: 0, refreshExpiresAt: 0, refreshedAt: 0, createdAt: 0, needsRelogin: false, needsReloginReason: null },
];

/** 演示模式中的临时 CLI 当前账号，仅存在于本次页面会话。 */
let demoActiveCliAccountId = accounts[0].id;

// Counts and relative model roles follow anonymous aggregates from the sanitized local cache.
// No upstream request row or identifier is copied into this fixture.
const usageSeeds: AccountUsageSeed[] = [
  {
    requestCount: 2243,
    models: [
      { model: "deepseek-v4-flash", requestCount: 2133, credit: 1794.39 },
      { model: "kimi-k3-1", requestCount: 24, credit: 2497.16 },
      { model: "deepseek-v4-pro", requestCount: 23, credit: 3.63 },
      { model: "glm-5.2", requestCount: 1, credit: 33.63 },
      { model: "hy3", requestCount: 62, credit: 0 },
    ],
  },
  {
    requestCount: 679,
    models: [
      { model: "deepseek-v4-flash", requestCount: 659, credit: 1270.62 },
      { model: "hy3", requestCount: 20, credit: 0 },
    ],
  },
  {
    requestCount: 318,
    models: [
      { model: "deepseek-v4-flash", requestCount: 309, credit: 595.08 },
      { model: "hy3", requestCount: 9, credit: 0 },
    ],
  },
];

const creditPackages = [
  [
    ["CodeBuddy 个人版国内运营裂变包", 5000, 3186.4, 36],
    ["CodeBuddy 个人版积分包", 2400, 1180.75, 18],
    ["CodeBuddy 新用户体验包", 800, 386.4, 5],
    ["CodeBuddy 签到赠送积分", 300, 196.25, 11],
    ["CodeBuddy 活动奖励积分", 600, 428.6, 27],
  ],
  [
    ["CodeBuddy 个人版国内运营裂变包", 3600, 2468.2, 24],
    ["CodeBuddy 个人版积分包", 1800, 905.5, 42],
    ["CodeBuddy 新用户体验包", 500, 128.2, 7],
    ["CodeBuddy 签到赠送积分", 240, 174.35, 15],
    ["CodeBuddy 活动奖励积分", 400, 286.8, 31],
  ],
  [
    ["CodeBuddy 个人版国内运营裂变包", 2400, 1680.4, 29],
    ["CodeBuddy 个人版积分包", 1200, 748.6, 55],
    ["CodeBuddy 新用户体验包", 360, 214.5, 14],
    ["CodeBuddy 签到赠送积分", 180, 96.75, 21],
    ["CodeBuddy 活动奖励积分", 300, 207.9, 38],
  ],
] as const;

function startOfToday(): Date {
  const date = new Date();
  date.setHours(0, 0, 0, 0);
  return date;
}

function localDate(daysAgo: number): string {
  const date = startOfToday();
  date.setDate(date.getDate() - daysAgo);
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, "0")}-${String(date.getDate()).padStart(2, "0")}`;
}

function atLocalTime(daysAgo: number, hour: number, minute: number): number {
  const date = startOfToday();
  date.setDate(date.getDate() - daysAgo);
  date.setHours(hour, minute, 0, 0);
  return date.getTime();
}

function futureAt(daysAhead: number, hour = 23, minute = 59): number {
  const date = startOfToday();
  date.setDate(date.getDate() + daysAhead);
  date.setHours(hour, minute, 0, 0);
  return date.getTime();
}

function hydratedAccounts(): AccountMeta[] {
  return accounts.map((account, index) => ({
    ...account,
    expiresAt: futureAt(12 + index * 5, 18, 30),
    refreshExpiresAt: futureAt(40 + index * 7),
    refreshedAt: atLocalTime(0, 9, 12 + index * 7),
    createdAt: atLocalTime(45 + index * 19, 10, 0),
  }));
}

function creditExpiry(accountId: string): CreditExpiry {
  const index = Math.max(0, accounts.findIndex((account) => account.id === accountId));
  const account = accounts[index] ?? accounts[0];
  const resources = creditPackages[index].map(([packageName, total, remaining, expireDays], packageIndex) => ({
    packageCode: `demo-package-${index + 1}-${packageIndex + 1}`,
    packageName,
    total,
    remaining,
    used: Number((total - remaining).toFixed(2)),
    status: 1,
    expireAt: futureAt(expireDays),
    expired: false,
    expiringSoon: expireDays <= 7,
  }));
  const totalCapacity = resources.reduce((sum, resource) => sum + resource.total, 0);
  const totalRemaining = resources.reduce((sum, resource) => sum + resource.remaining, 0);
  const expiringSoonRemaining = resources.filter((resource) => resource.expiringSoon).reduce((sum, resource) => sum + resource.remaining, 0);

  return {
    ok: true,
    accountId: account.id,
    accountName: account.nickname ?? account.email ?? account.id,
    updatedAt: Date.now() - (index + 1) * 4 * 60 * 1000,
    totalCapacity,
    totalRemaining: Number(totalRemaining.toFixed(2)),
    expiringSoonRemaining: Number(expiringSoonRemaining.toFixed(2)),
    expiredRemaining: 0,
    soonestExpireAt: Math.min(...resources.map((resource) => resource.expireAt)),
    expiringSoon: expiringSoonRemaining > 0,
    expired: false,
    resources,
  };
}

function dailyWeight(accountIndex: number, dayIndex: number): number {
  const weekdayWave = [0.72, 1.08, 0.93, 1.22, 0.84, 1.16, 1.01][dayIndex % 7];
  const quiet = (dayIndex + accountIndex * 4) % 13 === 0 ? 0.16 : 1;
  return weekdayWave * quiet * (1 + accountIndex * 0.035);
}

function distributeModels(seed: AccountUsageSeed, accountIndex: number) {
  const weights = Array.from({ length: 30 }, (_, dayIndex) => dailyWeight(accountIndex, dayIndex));
  const weightTotal = weights.reduce((sum, weight) => sum + weight, 0);
  const countSeries = seed.models.map((model) => {
    const raw = weights.map((weight) => (model.requestCount * weight) / weightTotal);
    const values = raw.map(Math.floor);
    let remaining = model.requestCount - values.reduce((sum, value) => sum + value, 0);
    const byFraction = raw.map((value, index) => ({ index, fraction: value - Math.floor(value) })).sort((left, right) => right.fraction - left.fraction);
    for (let index = 0; index < remaining; index += 1) values[byFraction[index].index] += 1;
    return values;
  });
  const creditSeries = seed.models.map((model) => {
    const values = weights.map((weight) => Number(((model.credit * weight) / weightTotal).toFixed(2)));
    const drift = Number((model.credit - values.reduce((sum, value) => sum + value, 0)).toFixed(2));
    values[values.length - 1] = Number((values[values.length - 1] + drift).toFixed(2));
    return values;
  });
  return Array.from({ length: 30 }, (_, dayIndex) => {
    const models = seed.models.map((model, modelIndex) => ({
      model: model.model,
      requestCount: countSeries[modelIndex][dayIndex],
      credit: creditSeries[modelIndex][dayIndex],
    }));
    return {
      date: localDate(29 - dayIndex),
      usage: Number(models.reduce((sum, model) => sum + model.credit, 0).toFixed(2)),
      models,
    };
  });
}

function sumModels(rows: { models: CreditOfficialUsageModel[] }[]): CreditOfficialUsageModel[] {
  const totals = new Map<string, CreditOfficialUsageModel>();
  for (const row of rows) {
    for (const model of row.models) {
      const current = totals.get(model.model) ?? { model: model.model, requestCount: 0, credit: 0 };
      current.requestCount += model.requestCount;
      current.credit = Number((current.credit + model.credit).toFixed(2));
      totals.set(model.model, current);
    }
  }
  return [...totals.values()].sort((left, right) => right.credit - left.credit);
}

function visibleRequests(accountIndex: number) {
  const account = accounts[accountIndex];
  const seed = usageSeeds[accountIndex];
  const hours = [16, 15, 17, 14, 1, 0, 3];
  const flashCredits = [0.13, 0.04, 0.2, 1, 0.08, 3.99, 0.45, 8.5, 24.56];
  const kimiCredits = [86.4, 103.2, 112.8, 128.4, 74.6];
  const proCredits = [0.04, 0.13, 0.2, 0.45];
  const weightedModels = seed.models.flatMap((model) =>
    Array.from({ length: Math.max(1, Math.round((model.requestCount / seed.requestCount) * 100)) }, () => model.model),
  );

  return Array.from({ length: 100 }, (_, rowIndex) => {
    const daysAgo = Math.floor(rowIndex / 8);
    const hour = hours[(rowIndex + accountIndex * 2) % hours.length];
    const minute = (rowIndex * 7 + accountIndex * 11) % 60;
    const ts = new Date(atLocalTime(daysAgo, hour, minute));
    const model = rowIndex < seed.models.length
      ? seed.models[rowIndex].model
      : weightedModels[rowIndex % weightedModels.length];
    const credit = model === "hy3"
      ? 0
      : model === "kimi-k3-1"
        ? kimiCredits[(rowIndex + accountIndex) % kimiCredits.length]
        : model === "glm-5.2"
          ? 33.63
          : model === "deepseek-v4-pro"
            ? proCredits[(rowIndex + accountIndex) % proCredits.length]
            : flashCredits[(rowIndex + accountIndex * 3) % flashCredits.length];
    return {
      accountId: account.id,
      accountName: account.nickname ?? account.email ?? account.id,
      requestId: `demo-request-${String(accountIndex + 1).padStart(2, "0")}-${String(rowIndex + 1).padStart(4, "0")}`,
      credit,
      model,
      client: rowIndex % 50 === 0 ? "CodeBuddyIDE" : "CLI",
      requestTime: `${localDate(daysAgo)} ${String(ts.getHours()).padStart(2, "0")}:${String(ts.getMinutes()).padStart(2, "0")}:00`,
    };
  });
}

function buildStatistics(): CreditStatistics {
  const demoAccounts = hydratedAccounts();
  const accountDaily = usageSeeds.map((seed, index) => distributeModels(seed, index));
  const daily = accountDaily[0].map((_, dayIndex) => {
    const models = new Map<string, CreditOfficialUsageModel>();
    for (const rows of accountDaily) {
      for (const model of rows[dayIndex].models) {
        const current = models.get(model.model) ?? { model: model.model, requestCount: 0, credit: 0 };
        current.requestCount += model.requestCount;
        current.credit = Number((current.credit + model.credit).toFixed(2));
        models.set(model.model, current);
      }
    }
    const modelRows = [...models.values()];
    return { date: accountDaily[0][dayIndex].date, usage: Number(modelRows.reduce((sum, model) => sum + model.credit, 0).toFixed(2)), models: modelRows };
  });
  const sumRecent = (rows: { usage: number }[], count: number) => Number(rows.slice(-count).reduce((sum, row) => sum + row.usage, 0).toFixed(2));
  const monthPrefix = localDate(0).slice(0, 7);
  const sumMonth = (rows: { date: string; usage: number }[]) => Number(rows.filter((row) => row.date.startsWith(monthPrefix)).reduce((sum, row) => sum + row.usage, 0).toFixed(2));
  const generatedAt = Date.now() - 3 * 60 * 1000;
  const creditRows = accounts.map((account) => creditExpiry(account.id));
  const officialAccounts = demoAccounts.map((account, index) => ({
    accountId: account.id,
    accountName: account.nickname ?? account.email ?? account.id,
    ok: true,
    requestCount: usageSeeds[index].requestCount,
    detailTruncated: true,
    usageToday: accountDaily[index][accountDaily[index].length - 1]?.usage ?? 0,
    usage7Days: sumRecent(accountDaily[index], 7),
    usageThisMonth: sumMonth(accountDaily[index]),
    reportedTotal: usageSeeds[index].requestCount,
    fetchedCount: usageSeeds[index].requestCount,
    models: sumModels(accountDaily[index]),
    daily: accountDaily[index],
  }));
  const usageToday = daily[daily.length - 1]?.usage ?? 0;
  const usage7Days = sumRecent(daily, 7);
  const usageThisMonth = sumMonth(daily);
  const totalRemaining = creditRows.reduce((sum, credit) => sum + (credit.totalRemaining ?? 0), 0);
  const totalCapacity = creditRows.reduce((sum, credit) => sum + (credit.totalCapacity ?? 0), 0);

  return {
    generatedAt,
    retentionDays: 90,
    coverageStartAt: atLocalTime(29, 0, 0),
    summary: { currentRemaining: Number(totalRemaining.toFixed(2)), currentCapacity: totalCapacity, usageToday, usage7Days, usageThisMonth, todayCheckedInAccounts: 3, todaySuccess: 2, todayAlready: 1, todayFailed: 0 },
    daily,
    accounts: demoAccounts.map((account, index) => ({
      accountId: account.id,
      accountName: account.nickname ?? account.email ?? account.id,
      isCurrent: index === 0,
      currentRemaining: creditRows[index].totalRemaining ?? null,
      totalCapacity: creditRows[index].totalCapacity ?? null,
      lastSnapshotAt: generatedAt - index * 120_000,
      usageToday: officialAccounts[index].usageToday ?? 0,
      usage7Days: officialAccounts[index].usage7Days ?? 0,
      usageThisMonth: officialAccounts[index].usageThisMonth ?? 0,
      checkedInToday: true,
      checkinStatusToday: index === 1 ? "already" : "success",
      lastCheckinAt: atLocalTime(0, 8, 6 + index * 9),
      lastCheckinResult: index === 1 ? "already" : "success",
      daily: accountDaily[index],
    })),
    events: demoAccounts.map((account, index) => ({ kind: "checkin" as const, ts: atLocalTime(0, 8, 6 + index * 9), date: localDate(0), accountId: account.id, accountName: account.nickname ?? account.email ?? account.id, result: index === 1 ? "already" : "success" })),
    officialUsage: {
      status: "complete",
      rangeStart: localDate(29),
      rangeEnd: localDate(0),
      collectedAt: generatedAt,
      summary: { usageToday, usage7Days, usageThisMonth },
      daily,
      accounts: officialAccounts,
      requests: accounts.flatMap((_, index) => visibleRequests(index)),
      models: sumModels(daily),
      detailLimitPerAccount: 100,
      errors: [],
    },
  };
}

function checkinConfig(): CheckinConfig {
  return { enabled: true, keepalive_days: 7, lazy_refresh_hours: 12 };
}

function travelConfig(): TravelConfig {
  return { enabled: true };
}

function travelStatus(accountId: string): TravelStatus {
  const index = Math.max(0, accounts.findIndex((account) => account.id === accountId));
  // 演示三种状态：旅行中 / 已结束 / 无 Buddy
  if (index % 3 === 0) return { label: "traveling", rewardCredit: 7, locationName: "咖啡馆", arriveAt: Math.floor(Date.now() / 1000) + 2 * 3600 + 40 * 60 };
  if (index % 3 === 1) return { label: "finished", rewardCredit: 20, locationName: "健身房" };
  return { label: "no-buddy", rewardCredit: null, locationName: null };
}

function rotateConfig(): AutoRotateConfig {
  return { enabled: true, check_interval_minutes: 15, cooldown_minutes: 120, min_gap_hours: 24, min_urgency_hours: 72, active_guard_minutes: 30, min_remaining_credits: 50 };
}

function checkinLogs(): CheckinLog[] {
  return hydratedAccounts().flatMap((account, accountIndex) => [0, 1, 2].map((daysAgo) => ({ ts: atLocalTime(daysAgo, 8, 6 + accountIndex * 9), accountId: account.id, email: account.nickname ?? account.email ?? account.id, result: accountIndex === 1 && daysAgo === 0 ? "already" : "success" })));
}

function rotateLogs(): RotateLog[] {
  return [
    { ts: atLocalTime(0, 9, 30), action: "skipped", reason: "当前账号仍是积分到期最紧迫的可用账号", from: { id: accounts[0].id, name: accounts[0].nickname }, to: null },
    { ts: atLocalTime(1, 16, 20), action: "switched", reason: "目标账号积分将在 5 天内到期", from: { id: accounts[1].id, name: accounts[1].nickname }, to: { id: accounts[0].id, name: accounts[0].nickname } },
  ];
}

function demoTokenTotals(input: number, output: number, cacheRead: number, cacheWrite: number, records: number): TokenStatsTotals {
  return { total: input + output + cacheWrite, input, output, cacheRead, cacheWrite, uncachedInput: Math.max(0, input - cacheRead), records, cacheHitRate: input > 0 ? cacheRead / input : null };
}

function demoTokenGroup(key: string, input: number, output: number, cacheRead: number, cacheWrite: number, records: number): TokenStatsGroup {
  return { key, ...demoTokenTotals(input, output, cacheRead, cacheWrite, records) };
}

function demoTokenSession(key: string, title: string, project: string, input: number, output: number, cacheRead: number, cacheWrite: number, records: number): TokenStatsGroup {
  const keyParts = key.split(" · ");
  return { ...demoTokenGroup(key, input, output, cacheRead, cacheWrite, records), title, project, sessionId: keyParts[keyParts.length - 1] };
}

function demoTokenSource(source: TokenStatsSource["source"], scale: number): TokenStatsSource {
  const daily = Array.from({ length: 14 }, (_, index) => {
    const wave = [0.62, 0.86, 1.1, 0.72, 1.3, 0.94, 0.38][index % 7] * scale;
    return demoTokenGroup(localDate(13 - index), Math.round(7_600_000 * wave), Math.round(480_000 * wave), Math.round(6_650_000 * wave), Math.round(95_000 * wave), Math.round(24 * wave));
  });
  const summary = daily.reduce((sum, row) => demoTokenTotals(sum.input + row.input, sum.output + row.output, sum.cacheRead + row.cacheRead, sum.cacheWrite + row.cacheWrite, sum.records + row.records), demoTokenTotals(0, 0, 0, 0, 0));
  const hours = Array.from({ length: 7 * 24 }, (_, index) => {
    const day = Math.floor(index / 24); const hour = index % 24;
    const active = Math.max(0.02, Math.exp(-Math.pow(hour - (day >= 5 ? 22 : 15), 2) / 22));
    return demoTokenGroup(`${day}-${hour}`, Math.round(720_000 * active * scale), Math.round(41_000 * active * scale), Math.round(610_000 * active * scale), 0, Math.max(1, Math.round(6 * active * scale)));
  });
  const projects = [
    demoTokenGroup("ai-gateway", 42_800_000 * scale, 2_400_000 * scale, 37_100_000 * scale, 420_000 * scale, Math.round(148 * scale)),
    demoTokenGroup("my-code-teams", 25_600_000 * scale, 1_650_000 * scale, 21_900_000 * scale, 260_000 * scale, Math.round(96 * scale)),
    demoTokenGroup("LetterTotTown", 11_900_000 * scale, 920_000 * scale, 9_700_000 * scale, 110_000 * scale, Math.round(51 * scale)),
  ];
  const models = [
    demoTokenGroup("deepseek-v4-flash", 56_400_000 * scale, 3_200_000 * scale, 49_100_000 * scale, 530_000 * scale, Math.round(210 * scale)),
    demoTokenGroup("kimi-k3-1", 17_300_000 * scale, 1_140_000 * scale, 14_200_000 * scale, 180_000 * scale, Math.round(61 * scale)),
    demoTokenGroup("glm-5.2", 6_600_000 * scale, 630_000 * scale, 5_400_000 * scale, 80_000 * scale, Math.round(24 * scale)),
  ];
  const sessions = [
    demoTokenSession("ai-gateway · token-stats-dashboard", "完善 Token 统计仪表盘与本地用量分析", "ai-gateway", 18_700_000 * scale, 1_050_000 * scale, 16_100_000 * scale, 160_000 * scale, Math.round(72 * scale)),
    demoTokenSession("my-code-teams · settings-agent-acp", "设计 Agent 与 ACP 管理设置", "my-code-teams", 13_200_000 * scale, 890_000 * scale, 11_300_000 * scale, 120_000 * scale, Math.round(55 * scale)),
    demoTokenSession("LetterTotTown · character-audio", "补全角色成语双音频", "LetterTotTown", 8_600_000 * scale, 640_000 * scale, 7_200_000 * scale, 80_000 * scale, Math.round(38 * scale)),
    demoTokenSession("ai-gateway · account-card-redesign", "统一账号卡片视觉和交互", "ai-gateway", 6_300_000 * scale, 410_000 * scale, 5_400_000 * scale, 50_000 * scale, Math.round(29 * scale)),
  ];
  const now = Date.now();
  return { source, summary, models, projects, sessions, daily, hours, filesScanned: source === "workbuddy" ? 63 : source === "codebuddy-ide" ? 17 : 41, parseErrors: 0, coverageStartAt: now - 13 * 86_400_000, coverageEndAt: now };
}

function demoTokenStatistics(days?: number): TokenStatistics {
  return { generatedAt: Date.now(), rangeDays: days ?? null, sources: [demoTokenSource("workbuddy", 1), demoTokenSource("codebuddy-cli", 0.58), demoTokenSource("codebuddy-ide", 0.36)] };
}

// ---------------------------------------------------------------------------
// 智能体管理演示数据
// ---------------------------------------------------------------------------

/** 演示用客户端探测结果：12 类智能体的安装/接入状态。 */
function demoAgentTargets(): AgentClientTarget[] {
  return [
    { id: "claude-code", label: "Claude Code", installed: true, configured: true, configPath: "C:\\Users\\demo\\.claude\\settings.json", note: "检测到 CLI 与配置文件", version: "2.1.4" },
    { id: "claude-desktop", label: "Claude Desktop", installed: true, configured: true, configPath: "C:\\Users\\demo\\AppData\\Roaming\\Claude\\claude_desktop_config.json", note: "第三方网关模式", version: "1.13.0" },
    { id: "codex", label: "Codex", installed: true, configured: true, configPath: "C:\\Users\\demo\\.codex\\config.toml", note: "Responses API 已启用", version: "0.146.2" },
    { id: "dsh", label: "DeepSeek Harness", installed: true, configured: false, configPath: "C:\\Users\\demo\\.dsh\\config.json", note: "已安装，尚未接入", version: "0.9.8" },
    { id: "opencode", label: "OpenCode", installed: true, configured: false, configPath: "C:\\Users\\demo\\.config\\opencode\\opencode.json", note: "已安装，尚未接入", version: "0.7.12" },
    { id: "pi", label: "Pi", installed: false, configured: false, configPath: "C:\\Users\\demo\\.pi\\providers.json", note: "未检测到安装", version: null },
    { id: "grok-build", label: "Grok Build", installed: true, configured: false, configPath: "C:\\Users\\demo\\.grok\\config.toml", note: "已安装，尚未接入", version: "1.2.0" },
    { id: "zcode", label: "ZCode", installed: false, configured: false, configPath: "C:\\Users\\demo\\.zcode\\config.json", note: "未检测到安装", version: null },
    { id: "kimi-code", label: "Kimi Code", installed: true, configured: false, configPath: "C:\\Users\\demo\\.kimi\\config.toml", note: "已安装，尚未接入", version: "0.5.3" },
    { id: "openclaw", label: "OpenClaw", installed: true, configured: false, configPath: "C:\\Users\\demo\\.openclaw\\config.json", note: "已安装，尚未接入", version: "0.3.11" },
    { id: "hermes", label: "Hermes", installed: false, configured: false, configPath: "C:\\Users\\demo\\.hermes\\config.json", note: "未检测到安装", version: null },
    { id: "minimax-code", label: "MiniMax Code", installed: true, configured: false, configPath: "C:\\Users\\demo\\AppData\\Roaming\\MiniMax\\config.json", note: "已安装，尚未接入", version: "0.4.6" },
  ];
}

/** 演示用网关综合状态（已启动、已配置 API Key）。 */
function demoGatewayStatus(): GatewayStatus {
  return {
    running: true,
    reachable: true,
    base: "http://127.0.0.1:7863",
    openaiBase: "http://127.0.0.1:7863/v1",
    port: 7863,
    exePath: "/demo/ai-gateway-server",
    exeFound: true,
    exeSource: "embedded",
    portAvailable: false,
    mode: "balance",
    pinnedUid: null,
    accounts: [{ uid: "demo-user-001", nickname: "测试 A", expiresAt: 0, needsRelogin: false }],
    excludedAccounts: [],
    authDir: "/demo/gateway/auth",
    accountsInLibrary: 3,
    config: {
      enabled: true,
      mode: "balance",
      pinned_uid: null,
      allowed_model: null,
      port: 7863,
      listen: ":7863",
      api_key: "sk-demo-workbuddy-key",
      auto_start: true,
      last_status: "ok",
      last_error: null,
    },
    health: { reachable: true, healthy: true, detail: null },
    pool: { accounts: [], total: 3, healthy: 3, cooling: 0, disabled: 0, in_flight_full: 0, sticky_sessions: 0, redis_mode: "off" },
  };
}

/** 演示用上游模型池。 */
function demoGatewayModels(): GatewayModelItem[] {
  return [
    { id: "claude-sonnet-4-6", name: "Claude Sonnet 4.6", owned_by: "anthropic" },
    { id: "claude-opus-4-2", name: "Claude Opus 4.2", owned_by: "anthropic" },
    { id: "deepseek-v4-pro", name: "DeepSeek V4 Pro", owned_by: "deepseek" },
    { id: "deepseek-v4-flash", name: "DeepSeek V4 Flash", owned_by: "deepseek" },
    { id: "kimi-k3-1", name: "Kimi K3.1", owned_by: "moonshot" },
    { id: "glm-5.2", name: "GLM 5.2", owned_by: "zhipu" },
    { id: "hy3", name: "混元 3", owned_by: "tencent" },
    { id: "gpt-5.4-codex", name: "GPT-5.4 Codex", owned_by: "openai" },
  ];
}

/** 演示用客户端配置备份记录（倒序，最新在前）。 */
function demoAgentBackups(target: string): AgentBackupItem[] {
  const now = Date.now();
  return [0, 1, 2, 3].map((offset) => ({
    id: `${target}-202609${String(21 - offset).padStart(2, "0")}-0${9 + offset}0000`,
    createdAt: Math.floor((now - offset * 26 * 60 * 60 * 1000) / 1000),
    path: `/demo/backups/${target}/20260921-0${9 + offset}0000/config.json`,
  }));
}
/** 演示用网关 Token 用量：与网关 /usage 响应同构。 */
function demoGatewayUsage(days?: number): GatewayUsageResult {
  const rangeDays = days && days > 0 ? days : null;
  const dayCount = Math.min(14, rangeDays ?? 14);
  const waves = [0.85, 1.12, 0.74, 1.28, 0.92, 0.41, 0.63];
  const daily = Array.from({ length: dayCount }, (_, index) => {
    const wave = waves[(dayCount - 1 - index) % 7];
    return {
      key: localDate(dayCount - 1 - index),
      ...demoTokenTotals(Math.round(3_180_000 * wave), Math.round(268_000 * wave), Math.round(2_790_000 * wave), Math.round(43_000 * wave), Math.round(9 * wave)),
    };
  });
  const summary = daily.reduce(
    (sum, row) => demoTokenTotals(sum.input + row.input, sum.output + row.output, sum.cacheRead + row.cacheRead, sum.cacheWrite + row.cacheWrite, sum.records + row.records),
    demoTokenTotals(0, 0, 0, 0, 0),
  );
  const models: GatewayUsageGroup[] = [
    { key: "deepseek-v4-flash", ...demoTokenTotals(30_900_000, 2_430_000, 27_120_000, 410_000, 87) },
    { key: "kimi-k3-1", ...demoTokenTotals(8_640_000, 780_000, 7_390_000, 96_000, 24) },
    { key: "glm-5.2", ...demoTokenTotals(3_180_000, 342_000, 2_610_000, 37_000, 10) },
    { key: "hy3", ...demoTokenTotals(1_090_000, 120_000, 880_000, 12_000, 5) },
  ];
  const usageAccounts: GatewayUsageGroup[] = [
    { key: accounts[0].uid ?? accounts[0].id, ...demoTokenTotals(26_400_000, 2_140_000, 23_180_000, 352_000, 74) },
    { key: accounts[1].uid ?? accounts[1].id, ...demoTokenTotals(11_900_000, 968_000, 10_320_000, 138_000, 33) },
    { key: accounts[2].uid ?? accounts[2].id, ...demoTokenTotals(5_510_000, 564_000, 4_500_000, 65_000, 19) },
  ];
  return {
    running: true,
    reachable: true,
    usage: { enabled: true, generatedAt: Date.now(), rangeDays, summary, models, accounts: usageAccounts, daily },
    error: null,
  };
}

/** Read-only demo response provider. It never reads or mutates real user data. */
export function screenshotDemoResponse(command: string, args?: Record<string, unknown>): unknown {
  const demoAccounts = hydratedAccounts();
  const appStatus: AppStatus = { running: true, authFile: "/demo/workbuddy/auth.json", current: { uid: demoAccounts[0].uid, nickname: demoAccounts[0].nickname, email: demoAccounts[0].email }, appPath: "/demo/WorkBuddy.app", version: "0.1.24" };
  const activeIndex = Math.max(0, demoAccounts.findIndex((account) => account.id === demoActiveCliAccountId));
  const activeAccount = demoAccounts[activeIndex] ?? demoAccounts[0];
  const cliStatus: CodeBuddyCliStatus = { configured: true, authMode: "settings-env", settingsPresent: true, helperPresent: false, helperSupportsAccountIds: true, activeIndex, activeAccountId: activeAccount.id, activeAccountName: activeAccount.nickname, accountCount: demoAccounts.length, statePath: "/demo/codebuddy-cli-state.json" };
  const config = rotateConfig();
  const rotateStatus: RotateStatus = { config, cliConfigured: true, activeAccountId: demoAccounts[0].id, activeAccountName: demoAccounts[0].nickname, lastCheckAt: atLocalTime(0, 9, 30), lastSwitchAt: atLocalTime(1, 16, 20) };
  const githubConfig: GithubConfig = { owner: "zhangjia", repo: "ai-gateway", proxy: "" };
  switch (command) {
    case "get_status": return appStatus;
    case "get_accounts": return { accounts: demoAccounts };
    case "get_codebuddy_cli_status": return cliStatus;
    case "switch_codebuddy_cli_account": {
      const target = demoAccounts.find((account) => account.id === args?.accountId);
      if (!target) throw new Error("账号不存在");
      demoActiveCliAccountId = target.id;
      return { ok: true, configured: true, synced: true, verified: true, activeIndex: demoAccounts.indexOf(target), activeAccountId: target.id, message: "演示切换已完成" } satisfies CodeBuddyCliSwitchResult;
    }
    case "get_checkin_status": return { ok: true, todayCheckedIn: true };
    case "get_credit_expiry": return creditExpiry(String(args?.accountId ?? ""));
    case "get_credit_statistics": return buildStatistics();
    case "get_token_statistics": return demoTokenStatistics(typeof args?.days === "number" ? args.days : undefined);
    case "get_gateway_usage": return demoGatewayUsage(typeof args?.days === "number" ? args.days : undefined);
    case "get_auto_checkin_config": return checkinConfig();
    case "get_checkin_logs": return { logs: checkinLogs() };
    case "get_travel_status": return travelStatus(String(args?.accountId ?? ""));
    case "get_auto_travel_config": return travelConfig();
    case "get_auto_rotate_config": return config;
    case "rotate_status": return rotateStatus;
    case "get_rotate_logs": return { logs: rotateLogs() };
    case "get_github_config": return githubConfig;
    case "check_update": return { ok: true, current: "0.1.24", latest: "0.1.25", latestTag: "v0.1.25", hasUpdate: true, releaseName: "更新提示演示", releaseUrl: "https://github.com/changexbc/workbuddy-switch/releases/tag/v0.1.25" };
    case "get_launch_at_login_enabled": return true;
    case "switch_progress": return { running: false, progress: null };
    case "get_gateway_status": return demoGatewayStatus();
    case "get_gateway_models": return { models: demoGatewayModels() };
    case "detect_agent_clients": return {
      base: "http://127.0.0.1:7863",
      hasApiKey: true,
      targets: demoAgentTargets(),
    } satisfies AgentDetectionResult;
    case "list_agent_backups": return { backups: demoAgentBackups(String(args?.target ?? "claude-code")) };
    case "get_cli_quota_status": return { providers: demoCliQuotaStatus() };
    case "get_cli_quotas": return { accounts: demoCliQuotaAccounts() };
    // ---- Trae ----
    case "trae_list_accounts": return { accounts: demoTraeAccounts() };
    case "trae_credits_stats": return demoTraeCreditsStats(typeof args?.days === "number" ? args.days : 30);
    case "trae_credits_history": return { records: demoTraeCreditsRecords() };
    case "trae_pay_status_cache": return demoTraePayStatusCache();
    case "groups_list": return demoGroups(String(args?.app ?? "trae"));
    // ---- 豆包 ----
    case "doubao_list_accounts": return demoDoubaoAccounts();
    case "doubao_history": return demoDoubaoHistory(typeof args?.days === "number" ? args.days : 14);
    case "doubao_settings": return { doubao_snapshot_include_idb: false };
    // ---- 通用 ----
    case "in_app_schedule_view": return { tasks: demoInAppTasks() };
    case "get_codebuddy_cn_ide_status": return {
      installed: true,
      running: false,
      exePath: "/demo/CodeBuddy CN.app",
      version: "1.0.0",
      dataDir: "/demo/Library/Application Support/CodeBuddy CN",
      loggedIn: true,
      accountMasked: "demo***@example.com",
      error: null,
    };
    case "proxy_logs_list": return demoProxyLogs(typeof args?.limit === "number" ? args.limit : 50);
    case "proxy_logs_overview": return demoProxyLogsOverview();
    // ---- 点击即读：详情 / 凭证弹窗 ----
    case "proxy_log_detail": return demoProxyLogDetail(String(args?.id ?? ""));
    case "trae_credit_detail": return demoTraeCreditDetail(String(args?.userId ?? ""));
    case "trae_account_jwt": return {
      userId: String(args?.userId ?? "7000000000000001"),
      jwt: "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.demo.signature",
    };
    // ---- 挂载即读：缺一个就会让对应卡片整块空白 ----
    case "app_env_check": return demoAppEnv(String(args?.targetApp ?? "TraeWork"));
    case "proxy_config": return {
      port: 8899,
      domains: "api.trae.cn,*.trae.cn,*.doubao.com",
      defaultDomains: "api.trae.cn,*.trae.cn,*.doubao.com",
      lastPort: 8899,
      existingSystemProxy: null,
    };
    case "proxy_status": return { running: true, port: 8899, captured: 3 };
    case "proxy_cert_status": return {
      certsDir: "/demo/certs",
      caCerPath: "/demo/certs/ai-gateway-ca.cer",
      caPemPath: "/demo/certs/ai-gateway-ca.pem",
      caExists: true,
      hint: "演示模式：未安装真实 CA 证书",
    };
    case "task_status": return { tasks: demoTasks() };
    case "trae_checkin_trends": return demoCheckinTrends(typeof args?.days === "number" ? args.days : 30);
    case "trae_usage_history": return demoUsageHistory();
    default: throw new Error(`演示模式缺少只读数据: ${command}`);
  }
}

// ---------------------------------------------------------------------------
// 演示数据：Trae / 豆包 / 日志
// ---------------------------------------------------------------------------

/**
 * 演示模式下的 Trae 账号。
 *
 * 刻意让三行覆盖三种 JWT 状态（正常 / 临近过期 / 已失效），
 * 截图里一眼能看到徽章配色差异，而不是三行全绿。
 */
function demoTraeAccounts() {
  const states = [
    { userId: "7000000000000001", name: "测试 A", jwtStatus: "ok" as const, jwtExpHours: 412, credits: 3280, group: "g-work" },
    { userId: "7000000000000002", name: "测试 B", jwtStatus: "warn" as const, jwtExpHours: 26, credits: 940, group: null },
    { userId: "7000000000000003", name: "测试 C", jwtStatus: "expired" as const, jwtExpHours: -3, credits: null, group: "g-personal" },
  ];
  return states.map((s, i) => ({
    userId: s.userId,
    name: s.name,
    addedAt: localDate(20 - i * 5),
    updatedAt: localDate(i),
    jwtStatus: s.jwtStatus,
    jwtExpHours: s.jwtExpHours,
    jwtExpTimestamp: Math.floor(Date.now() / 1000) + Math.round(s.jwtExpHours * 3600),
    hasRefreshToken: i !== 2,
    refreshTokenInvalid: i === 2,
    refreshTokenFails: i === 2 ? 3 : 0,
    refreshTokenExpiresAt: i === 2 ? null : Math.floor(Date.now() / 1000) + 20 * 86400,
    refreshedAt: i === 2 ? null : localDate(0),
    canRefresh: i !== 2,
    deviceIdMasked: `dc_${(1000 + i * 7).toString(16)}***${i}f`,
    payIdentity: i === 0 ? "Pro" : null,
    payExpireAt: i === 0 ? localDate(-40) : null,
    creditsTotal: s.credits,
    creditsUpdatedAt: s.credits === null ? null : localDate(0),
    group: s.group,
  }));
}

/** 演示模式下的 Trae 积分趋势（含一个缺数据的天，验证断线显示）。 */
function demoTraeCreditsStats(days: number) {
  const count = Math.min(days, 30);
  const waves = [1, 0.96, 0.91, 0.99, 0.94, 0.9, 0.87];
  const daily = Array.from({ length: count }, (_, i) => {
    const daysAgo = count - 1 - i;
    // 第 4 天故意缺数据：界面必须断线而不是连成一条直线
    if (daysAgo === 4) return { date: localDate(daysAgo), total: null, consumed: null };
    const total = Math.round(4200 * waves[i % 7]);
    return {
      date: localDate(daysAgo),
      total,
      consumed: daysAgo === count - 1 ? null : 40 + ((i * 17) % 90),
    };
  });
  const observed = daily.filter((d) => d.total !== null);
  const last = observed[observed.length - 1];
  const first = observed[0];
  return {
    days: count,
    daily,
    summary: {
      latestTotal: last?.total ?? null,
      firstTotal: first?.total ?? null,
      consumed: observed.reduce((sum, d) => sum + (d.consumed ?? 0), 0) || null,
      observedDays: observed.length,
      change: observed.length >= 2 ? last!.total! - first!.total! : null,
    },
    accounts: [
      { userId: "7000000000000001", name: "测试 A", credits: 3280, updatedAt: localDate(0) },
      { userId: "7000000000000002", name: "测试 B", credits: 940, updatedAt: localDate(0) },
      { userId: "7000000000000003", name: "测试 C", credits: null, updatedAt: null },
    ],
  };
}

/** 演示模式下的积分快照原始记录。 */
function demoTraeCreditsRecords() {
  return Array.from({ length: 12 }, (_, i) => {
    const daysAgo = 11 - i;
    const credits = Math.round(4200 - i * 60);
    return {
      date: localDate(daysAgo),
      userId: "7000000000000001",
      credits,
      delta: i === 0 ? 0 : -60,
    };
  });
}

/** 演示模式下的套餐缓存。 */
function demoTraePayStatusCache() {
  return {
    "7000000000000001": {
      identity: "Pro",
      expireAt: localDate(-40),
      checkedAt: localDate(0),
    },
  };
}

/** 演示模式下的分组（含一个空分组，验证空态）。 */
function demoGroups(app: string) {
  return {
    app,
    groups: [
      { id: "g-work", name: "工作", color: "blue", createdAt: localDate(30), count: 1 },
      { id: "g-personal", name: "个人", color: "green", createdAt: localDate(25), count: 1 },
      { id: "g-empty", name: "待整理", color: "slate", createdAt: localDate(3), count: 0 },
    ],
    membership: { "7000000000000001": "g-work", "7000000000000003": "g-personal" },
  };
}

/** 演示模式下的豆包账号（覆盖正常 / 临期 / 已过期三种分层的健康态）。 */
function demoDoubaoAccounts() {
  const days = [18, 4, -2];
  const names = ["测试 A", "测试 B", "测试 C"];
  return {
    lastKeepaliveAt: localDate(1),
    currentUserId: "8000000000000001",
    accounts: days.map((d, i) => ({
      userId: `800000000000000${i + 1}`,
      name: names[i],
      note: null,
      addedAt: localDate(30 - i * 8),
      lastActiveAt: localDate(i),
      sessionIdMasked: `sid_***${i}a2f`,
      sidGuardMasked: `sg_***${i}b7c`,
      ttwidMasked: `tt_***${i}c9d`,
      hasSessionId: d > -2,
      hasTtwid: d > -2,
      sessionExpireAt: localDate(-d),
      expired: d < 0,
      sessionState: d < 0 ? ("expired" as const) : ("ok" as const),
      expiryTier: d < 0 ? ("expired" as const) : d <= 7 ? ("soon" as const) : ("fresh" as const),
      daysLeft: d,
      sessionSource: i === 2 ? null : "抓包自动获取",
      cookiesSyncedAt: localDate(i),
      lastRenewAt: i === 2 ? null : localDate(1),
      quotaLevel: d < 0 ? null : "基础版",
      quotaExpireAt: d < 0 ? null : localDate(-d),
      quotaSummary: d < 0 ? null : "当前时段剩余 62%",
      quotaCheckedAt: d < 0 ? null : localDate(0),
      quotaUsedPercent: d < 0 ? null : 38,
      hasSnapshot: true,
      sizeBytes: 18_400_000 - i * 2_100_000,
      fileCount: 240 - i * 30,
      lastModified: Math.floor(Date.now() / 1000) - i * 86400,
      isCurrent: i === 0,
      orphanSnapshot: false,
    })),
  };
}

/** 演示模式下的豆包运维健康史。 */
function demoDoubaoHistory(days: number) {
  const count = Math.min(days, 14);
  const events = Array.from({ length: count }, (_, i) => {
    const daysAgo = i;
    const failed = daysAgo === 2;
    return {
      kind: failed ? ("renew" as const) : ("keepalive" as const),
      ts: atLocalTime(daysAgo, 9, 30),
      date: localDate(daysAgo),
      ok: !failed,
      message: failed ? "HTTP 续期失败，已回退到客户端保活" : "保活完成，会话已续期",
      userId: "8000000000000001",
    };
  });
  return {
    events,
    trend: Array.from({ length: count }, (_, i) => ({
      date: localDate(count - 1 - i),
      usedPercent: 20 + ((i * 13) % 55),
      level: "基础版",
      uid: "8000000000000001",
    })),
    health: {
      days: count,
      keepalive: count - 1,
      renew: 1,
      quota: count,
      ok: count,
      failed: 1,
      lastKeepaliveAt: localDate(1),
    },
    requestedDays: days,
  };
}

/** 演示模式下的应用内调度任务（含一个从未跑过的，验证空态）。 */
function demoInAppTasks() {
  return [
    { kind: "traeCheckin", label: "Trae 签到", at: "08:30", lastRunDay: localDate(0) },
    { kind: "traeCreditsSnapshot", label: "Trae 积分快照", at: "09:00", lastRunDay: localDate(0) },
    { kind: "doubaoRenewHttp", label: "豆包探活续期", at: "09:30", lastRunDay: localDate(1) },
    { kind: "doubaoQuota", label: "豆包额度巡检", at: "10:00", lastRunDay: null },
  ];
}

/** 演示模式下的抓包日志（两条 HTTP + 一条 WebSocket + 一条流式）。 */
function demoProxyLogs(limit: number): ProxyLogList {
  const all = [
    {
      id: "proxy_req_2025-01-15.log:3",
      timestamp: `${localDate(0)} 09:41:12`,
      method: "HTTP POST",
      host: "api.trae.cn",
      path: "/trae/api/v1/pay/query_user_usage_group_by_session",
      status: "200 OK",
      size: 4128,
    },
    {
      id: "proxy_req_2025-01-15.log:2",
      timestamp: `${localDate(0)} 09:38:04`,
      method: "HTTP POST",
      host: "api.trae.cn",
      path: "/v1/chat/completions",
      status: "200 OK",
      size: 86_240,
      sseModel: "claude-sonnet-4",
      sseTokens: "p:120 c:340 t:460",
    },
    {
      id: "proxy_req_2025-01-15.log:1",
      timestamp: `${localDate(0)} 09:12:55`,
      method: "WebSocket",
      host: "ws.trae.cn",
      path: "/ws/chat",
      status: "101 Upgrade",
      size: 1820,
    },
    {
      id: "proxy_req_2025-01-14.log:0",
      timestamp: `${localDate(1)} 21:07:31`,
      method: "HTTP GET",
      host: "api.trae.cn",
      path: "/trae/api/v1/user/current",
      status: "401 Unauthorized",
      size: 640,
    },
  ];
  return { entries: all.slice(0, limit), total: all.length };
}

/** 演示模式下的抓包日志目录概况。 */
function demoProxyLogsOverview(): ProxyLogsOverview {
  return {
    dir: "/demo/logs",
    fileCount: 2,
    totalBytes: 2_418_000,
    oldest: localDate(1),
    newest: localDate(0),
  };
}

/** 演示模式下的抓包日志详情（与 demoProxyLogs 的条目 id 对应）。 */
function demoProxyLogDetail(id: string): string {
  return [
    "==================== REQUEST ====================",
    "POST /api/v1/chat/completions HTTP/1.1",
    "Host: api.trae.cn",
    "Authorization: Bearer ***REDACTED***",
    "Content-Type: application/json",
    "",
    '{"model":"claude-sonnet-4","stream":true,"messages":[{"role":"user","content":"写一个快排"}]}',
    "",
    "==================== RESPONSE ===================",
    "HTTP/1.1 200 OK",
    "Content-Type: text/event-stream",
    "",
    "data: {\"model\":\"claude-sonnet-4\",\"choices\":[{\"delta\":{\"content\":\"好的\"}}]}",
    "data: {\"usage\":{\"prompt_tokens\":1280,\"completion_tokens\":430}}",
    "data: [DONE]",
    "",
    `# 演示数据：${id}`,
  ].join("\n");
}

/** 演示模式下的单账号积分明细（含一个拿不到数据的来源错误）。 */
function demoTraeCreditDetail(userId: string): TraeCreditDetail {
  return {
    userId,
    total: 1860,
    packs: [
      { name: "Pro 订阅额度", remaining: 1240, total: 1500, expireAt: localDate(-12) },
      { name: "活动赠送", remaining: 620, total: 600, expireAt: localDate(-3) },
    ],
    payIdentity: "pro_trial",
    payExpireAt: localDate(-25),
    errors: [],
  };
}

/** 演示模式下的应用环境（三种应用都按已装返回，布局各不相同）。 */
function demoAppEnv(targetApp: string): AppEnvStatus {
  const isDoubao = targetApp === "Doubao";
  const isCn = targetApp === "Trae";
  return {
    targetApp,
    appName: isDoubao ? "豆包" : isCn ? "Trae" : "Trae Work",
    layout: isDoubao ? ("chromium" as const) : ("icube" as const),
    installed: true,
    exePath: isDoubao ? "/demo/Doubao.app" : isCn ? "/demo/Trae CN.app" : "/demo/Trae Work.app",
    dataDir: isDoubao ? "/demo/Doubao/User Data" : "/demo/Trae/User",
    dataDirExists: true,
    profilesDir: "/demo/profiles",
    snapshotCount: 3,
    manualPath: null,
    settingsPathKey: isDoubao ? "doubao_path" : isCn ? "trae_cn_path" : "trae_path",
    running: true,
    version: isDoubao ? "2.28.13_win" : "1.0.9",
    versionSource: isDoubao ? "/demo/Doubao/User Data/Local State" : "/demo/Trae/product.json",
    settingsFile: "/demo/app_settings.json",
  };
}

/** 演示模式下的计划任务（含一个未注册的，验证空态）。 */
function demoTasks(): TaskStatusItem[] {
  return [
    { kind: "traeCheckin", name: "AIGateway_TraeCheckin", label: "Trae 签到", cliKey: "trae-checkin", registered: true, time: "08:30", error: null },
    { kind: "traeCreditsSnapshot", name: "AIGateway_TraeCreditsSnapshot", label: "Trae 积分快照", cliKey: "trae-credits-snapshot", registered: true, time: "09:00", error: null },
    { kind: "doubaoRenew", name: "AIGateway_DoubaoRenew", label: "豆包保活", cliKey: "doubao-keepalive", registered: true, time: "09:30", error: null },
    { kind: "doubaoQuota", name: "AIGateway_DoubaoQuota", label: "豆包额度巡检", cliKey: "doubao-quota", registered: false, time: "10:00", error: null },
  ];
}

/** 演示模式下的签到成功率趋势（含一天全是「已签到」、一天有失败）。 */
function demoCheckinTrends(days: number): TraeCheckinTrends {
  const count = Math.min(days, 30);
  const points = Array.from({ length: count }, (_, i) => {
    const daysAgo = count - 1 - i;
    // 第 6 天全是「已签到」：算成功，但绿段为空 —— 验证灰段配色
    if (daysAgo === 6) {
      return { date: localDate(daysAgo), ok: 0, already: 3, failed: 0, total: 3, successRate: 100 };
    }
    // 第 3 天有失败 —— 验证红段
    if (daysAgo === 3) {
      return { date: localDate(daysAgo), ok: 2, already: 0, failed: 1, total: 3, successRate: 66.7 };
    }
    return { date: localDate(daysAgo), ok: 3, already: 0, failed: 0, total: 3, successRate: 100 };
  });
  const ok = points.reduce((s, p) => s + p.ok, 0);
  const already = points.reduce((s, p) => s + p.already, 0);
  const failed = points.reduce((s, p) => s + p.failed, 0);
  const total = points.reduce((s, p) => s + p.total, 0);
  return {
    days: count,
    points,
    summary: {
      ok,
      already,
      failed,
      total,
      observedDays: points.length,
      successRate: total === 0 ? null : Math.round(((ok + already) / total) * 1000) / 10,
    },
  };
}

/** 演示模式下的官方积分消耗历史（含一个失败账号，验证告警条）。 */
function demoUsageHistory(): TraeUsageHistory {
  const daily = Array.from({ length: 14 }, (_, i) => {
    const daysAgo = 13 - i;
    const credits = 40 + Math.round(38 * Math.abs(Math.sin(i)));
    return {
      date: localDate(daysAgo),
      credits,
      sessions: 2 + (i % 4),
      models: { "claude-sonnet-4": Math.round(credits * 0.7), "gpt-5": Math.round(credits * 0.3) },
      inputTokens: 12_400 + i * 320,
      outputTokens: 3_100 + i * 90,
      cacheReadTokens: 8_200 + i * 140,
    };
  });
  const total = daily.reduce((s, d) => s + d.credits, 0);
  return {
    fetchedAt: Date.now(),
    cached: true,
    accounts: [
      {
        userId: "7000000000000001",
        name: "测试 A",
        ok: true,
        error: null,
        daily,
        totalCredits: total,
        sessions: daily.reduce((s, d) => s + d.sessions, 0),
        models: [
          { model: "claude-sonnet-4", credits: Math.round(total * 0.7) },
          { model: "gpt-5", credits: Math.round(total * 0.3) },
        ],
      },
      {
        userId: "7000000000000002",
        name: "测试 B",
        ok: false,
        error: "刷新凭证已失效，请重新登录该账号",
        daily: [],
        totalCredits: 0,
        sessions: 0,
        models: [],
      },
    ],
  };
}

/**
 * 演示用的 CLI 登录态。
 *
 * 覆盖三种界面分支：正常有额度、有登录但查询失败、未登录 —— 截图里能看到
 * 全部状态，而不是只有happy path。
 */
function demoCliQuotaStatus(): CliQuotaStatusItem[] {
  return [
    { provider: "claude", label: "Claude", loggedIn: true, loginHint: "在终端运行 claude 完成登录" },
    { provider: "antigravity", label: "Antigravity", loggedIn: true, loginHint: "登录 Google Antigravity 客户端" },
    { provider: "codex", label: "Codex", loggedIn: true, loginHint: "运行 codex login 完成登录" },
    { provider: "xai", label: "Grok", loggedIn: false, loginHint: "运行 grok login 完成登录" },
    { provider: "kimi", label: "Kimi", loggedIn: false, loginHint: "在 Kimi Code 中登录后重试" },
  ];
}

/** 演示用的 CLI 额度（含一个失败态与一个未知百分比窗口）。 */
function demoCliQuotaAccounts(): CliQuotaAccount[] {
  const now = Date.now();
  return [
    {
      id: "claude:demo",
      provider: "claude",
      providerLabel: "Claude",
      label: "demo@example.com",
      loggedIn: true,
      source: "~/.claude/.credentials.json",
      plan: "Max",
      windows: [
        { label: "5 小时", remainingPercent: 72, resetAtMs: now + 42 * 60 * 1000, detail: null },
        { label: "7 天", remainingPercent: 38, resetAtMs: now + 3 * 86400 * 1000, detail: null },
        { label: "Sonnet 7 天", remainingPercent: null, resetAtMs: null, detail: "上游未提供该窗口" },
      ],
      error: null,
      fetchedAt: now,
      resetCredits: null,
      subscriptionActiveUntil: null,
    },
    {
      id: "antigravity:demo",
      provider: "antigravity",
      providerLabel: "Antigravity",
      label: "demo@example.com",
      loggedIn: true,
      source: "Windows 凭据管理器 · gemini:antigravity",
      plan: "Pro",
      windows: [
        { label: "Gemini 5h", remainingPercent: 64, resetAtMs: now + 5 * 3600 * 1000, detail: null },
      ],
      error: null,
      fetchedAt: now,
      resetCredits: null,
      subscriptionActiveUntil: null,
    },
    {
      id: "codex:demo",
      provider: "codex",
      providerLabel: "Codex",
      label: "demo@example.com",
      loggedIn: true,
      source: "~/.codex/auth.json",
      plan: "Plus",
      windows: [
        { label: "5 小时", remainingPercent: 91, resetAtMs: now + 3 * 3600 * 1000, detail: null },
        { label: "每周", remainingPercent: 22, resetAtMs: now + 4 * 86400 * 1000, detail: null },
      ],
      error: null,
      fetchedAt: now,
      resetCredits: { available: 2, applicable: 1, earliestExpiryMs: now + 20 * 86400 * 1000 },
      subscriptionActiveUntil: new Date(now + 26 * 86400 * 1000).toISOString(),
    },
    {
      id: "xai:demo",
      provider: "xai",
      providerLabel: "Grok",
      label: "Grok",
      loggedIn: false,
      source: "",
      plan: null,
      windows: [],
      error: null,
      fetchedAt: 0,
      resetCredits: null,
      subscriptionActiveUntil: null,
    },
    {
      id: "kimi:demo",
      provider: "kimi",
      providerLabel: "Kimi",
      label: "Kimi",
      loggedIn: false,
      source: "",
      plan: null,
      windows: [],
      error: null,
      fetchedAt: 0,
      resetCredits: null,
      subscriptionActiveUntil: null,
    },
  ];
}
