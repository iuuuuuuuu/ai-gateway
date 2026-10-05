import type {
  AccountMeta, AppStatus, AutoRotateConfig, CheckinConfig, CheckinLog,
  CliQuotaAccount, CliQuotaStatusItem,
  CodeBuddyCliStatus, CodeBuddyCliSwitchResult, CreditExpiry, CreditOfficialUsageModel, CreditStatistics,
  GatewayUsageGroup, GatewayUsageResult,
  GithubConfig, RotateLog, RotateStatus, TokenStatistics, TokenStatsGroup, TokenStatsSource, TokenStatsTotals,
  TravelConfig, TravelStatus,
} from "./types";
// Qoder / ZCode 的账号类型定义在 api.ts（不在 types.ts）——
// 演示模式要给这两个页面造假数据，故从这里取真实类型。
import type {
  DoubaoHealthEvent,
  DoubaoHistory,
  GroupApp,
  GroupsView,
  ProxyLogEntry,
  ProxyLogList,
  ProxyLogsOverview,
  QoderAccount,
  TraeCheckinTodayMap,
  TraeCheckinTrendPoint,
  TraeCheckinTrends,
  TraeCreditDetail,
  TraeCreditsDay,
  TraeCreditsStats,
  TraeUsageDay,
  TraeUsageHistory,
  ZcodeAccount,
} from "./api";
import { demoModeEnabled } from "./demo-mode";

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

/**
 * 后端运维事件的时间戳形态。
 *
 * ⚠ 是 `YYYY-MM-DDTHH-MM-SSZ`（**时间里是连字符**），来自 `config::utc_iso`，
 * 不是 RFC3339 的冒号形态。演示数据必须照这个写：
 * 后端的 `parse_iso_ts` 两种都认，但如果我们演示里用冒号，
 * 就等于在演示模式下掩盖了"真实数据长什么样"。
 */
function demoUtcIso(daysAgo: number, hour: number, minute: number): string {
  const hh = String(hour).padStart(2, "0");
  const mm = String(minute).padStart(2, "0");
  return `${localDate(daysAgo)}T${hh}-${mm}-00Z`;
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

    // ---- Qoder / ZCode 页面 ----
    //
    // 这两个页面的数据此前**完全没覆盖**，于是演示模式打开它们会走到
    // 下面的 `default` 抛「演示模式缺少只读数据」—— 截图直接崩。
    //
    // 这里的假数据只求**形状正确**（字段齐全、类型对），数值本身无意义：
    // 它的用途是截图与视觉走查，不是功能验证。
    case "qoder_list_accounts":
      return { accounts: [demoQoderAccount()] };
    case "zcode_list_accounts":
      return { accounts: [demoZcodeAccount()] };
    case "qoder_summary":
      return { total: 1, enabled: 1, healthy: 1, cooling: 0 };
    case "zcode_summary":
      return { total: 1, enabled: 1, healthy: 1, cooling: 0 };
    case "qoder_campaigns":
    case "qoder_campaigns_all":
      // 演示数据**照抄上游真实字段名**（`campaignId` / `actionType` /
      // `claimStatus`），不要用 `id`/`name`/`status` 那套自造 schema。
      //
      // ⚠⚠ 这正是我踩过的坑：原演示数据用的是自造字段名，于是它
      // **永远走不到真实的判据分支**（`isClaimedCampaign` 读 `claimStatus`、
      // `isBenefitCampaign` 读 `actionType`，两个都取不到值）
      // ⇒ 演示模式下无论怎么改逻辑，界面都长一个样，
      // 拿它当"实测"等于自欺。
      //
      // 现在三条覆盖全部状态（与所有者账号的真实组合一致）：
      //   ready   —— 可领
      //   done    —— 已领（实测国际版就是这种）
      //   detail  —— 仅详情（实测国际版唯一那条就是它）
      return {
        accounts: [{
          uid: demoQoderAccount().uid,
          nickname: demoQoderAccount().nickname,
          ok: true,
          campaigns: [
            {
              campaignId: "demo-claimable",
              campaignKey: "act-20260923-001",
              actionType: "CLAIM_BENEFIT",
              claimStatus: "CLAIMABLE",
              benefit: { kind: "CREDITS", amount: 100 },
              placements: [{
                type: "POPUP",
                content: { zh: { title: "每天领 100 Credits", description: "每日可领一次" } },
              }],
            },
            {
              campaignId: "demo-claimed",
              campaignKey: "act-20260923-002",
              actionType: "CLAIM_BENEFIT",
              claimStatus: "CLAIMED",
              benefit: { kind: "CREDITS", amount: 100 },
              placements: [{
                type: "POPUP",
                content: { zh: { title: "每日签到奖励（已领）" } },
              }],
            },
            {
              campaignId: "demo-view-only",
              campaignKey: "act-20260901-493",
              actionType: "VIEW_DETAILS",
              claimStatus: "CLAIMED",
              placements: [{
                type: "POPUP",
                content: {
                  zh: {
                    title: "9月限时福利，Pro/Pro+ 首月订阅 Credits 翻倍",
                    description: "首购 Pro 得 4,000，Pro+ 得 12,000。续费、升级加赠 1,000。",
                  },
                },
              }],
            },
          ],
          claimable: 1,
        }],
        totalClaimable: 1,
      };
    case "qoder_claim_campaign":
    case "qoder_claim_all_campaigns":
      // 演示模式**绝不真领**（这是写操作）—— 只回一个成功形状供界面走查
      return {
        ok: true,
        claimed: 1,
        failed: 0,
        skipped: 0,
        results: [{ uid: demoQoderAccount().uid, ok: true, message: "演示模式：未真实领取" }],
      };
    case "zcode_scan_local":
      return { candidates: [], scanned: 0 };
    case "zcode_import_scanned":
      return { imported: 0, failed: 0, enriched: 0, enrichError: "" };
    case "qoder_import_from_client":
      return { imported: 0, failed: 0, enriched: 0, enrichError: "" };

    // ---- 网关配置（Qoder / ZCode 的「配置」弹窗要读它）----
    //
    // 那两个弹窗里的「权益自动领取」开关初始值取自 `get_gateway_config`
    // 的 `product_tasks_enabled`。缺这条会给默认分支抛异常 ⇒
    // **弹窗里的开关显示不出来**（只有一块空卡片），走查时看不到真实样式。
    //
    // 演示值取 `true`：与真实首装默认一致（缺键即 true），
    // 这样截图里看到的是"功能开着"的正常态。
    case "get_gateway_config":
      return {
        config: {
          enabled: true,
          port: 7864,
          listen: ":7864",
          api_key: "sk-demo",
          auto_start: true,
          mode: "multi_product",
          product_tasks_enabled: true,
          activity_report_count: 3,
          last_status: "running",
        },
      };
    case "get_gateway_status":
      return { running: true, port: 7864, pid: 12345, accounts: demoAccounts.length };

    // 「平台 × 区域 → 模型」白名单 + 禁用清单。
    //
    // 演示值刻意**两种状态都有**：qoder 那边放一个"禁用"的
    //（`deepseek-v4.1-flash` —— 正是所有者反馈过的那个被上游清单放行、
    // 光删白名单删不掉的模型），这样走查时能看到禁用条目的样式
    //（划掉 + 红色「禁用」徽标），而不是只有一片"允许"。
    case "get_platform_models":
      return {
        platform_models: {
          qoder: { cn: ["qwen3.8-flash", "glm-5.3"], intl: ["qwen3.8-max"] },
          zcode: { "": ["glm-5.3", "glm-5.3-flash"] },
          workbuddy: { cn: ["deepseek-v4.1-flash"] },
        },
        platform_models_disabled: {
          qoder: { cn: ["deepseek-v4.1-flash"] },
        },
      };

    // 统一账号页的「参加兼容网关路由」开关（只读快照）。
    //
    // 演示值刻意**两种状态都有**：qoder 关掉，其余参加 —— 这样走查时
    // 能同时看到开、关两种 Switch 外观，以及"关掉的平台"在页头提示语
    // 上的表现（而不是一片全开，看不出关掉长什么样）。
    //
    // ⚠ 键缺失 = 参加（后端语义）。这里把 trae 显式写成 true 只是为了
    // 演示数据自解释；界面判断必须用 !== false，别依赖键存在。
    case "get_platform_enabled":
      return {
        platformEnabled: { qoder: false, trae: true },
      };

    // ---- 本机 AI CLI 额度查询（额度查询页）----
    //
    // 这两个命令已登记进 `api.ts` 的 `DEMO_READ_COMMANDS`，**必须**在这里
    // 也有 case —— 否则白名单放行后落到 `default` 抛「演示模式缺少只读数据」，
    // 正是上面 Qoder/ZCode 那一段注释里记录过的坑（数据没写 = 页面空白）。
    //
    // 演示值刻意覆盖**三种展示状态**，这样走查时能一次看到全部分支：
    //   claude      —— 有额度、两个窗口都拿到百分比（正常态）
    //   codex       —— `remainingPercent: null`（**未知 ≠ 用尽**：界面要显示
    //                  「—」而不是 0，否则看起来像额度耗尽）
    //   antigravity —— `loggedIn: false`（**未登录不是错误**：中性提示 + 登录引导）
    case "get_cli_quota_status":
      return {
        providers: [
          { provider: "claude", label: "Claude Code", loggedIn: true, loginHint: "运行 `claude` 完成登录" },
          { provider: "antigravity", label: "Antigravity", loggedIn: false, loginHint: "打开 Antigravity 登录一次" },
          { provider: "codex", label: "Codex", loggedIn: true, loginHint: "运行 `codex login` 完成登录" },
          { provider: "xai", label: "Grok Build", loggedIn: false, loginHint: "运行 `grok login` 完成登录" },
          { provider: "kimi", label: "Kimi Code", loggedIn: false, loginHint: "运行 `kimi` 完成登录" },
        ] satisfies CliQuotaStatusItem[],
      };
    case "get_cli_quotas":
      return {
        accounts: [
          {
            id: "claude:keychain",
            provider: "claude",
            providerLabel: "Claude Code",
            label: "demo@example.com",
            loggedIn: true,
            source: "本机凭据（演示）",
            plan: "Max",
            windows: [
              { label: "5 小时", remainingPercent: 72, resetAtMs: Date.now() + 3 * 3600_000, detail: null },
              { label: "每周", remainingPercent: 41, resetAtMs: Date.now() + 4 * 86400_000, detail: null },
            ],
            error: null,
            fetchedAt: Date.now(),
            resetCredits: null,
            subscriptionActiveUntil: null,
          },
          {
            id: "codex:auth.json",
            provider: "codex",
            providerLabel: "Codex",
            label: "demo@example.com",
            loggedIn: true,
            source: "本机凭据（演示）",
            plan: "Pro",
            windows: [
              // remainingPercent = null：上游没给出可比口径 ⇒ 界面显示「—」
              { label: "每周", remainingPercent: null, resetAtMs: null, detail: "$25.00 / $100.00" },
            ],
            error: null,
            fetchedAt: Date.now(),
            resetCredits: { available: 2, applicable: 1, earliestExpiryMs: Date.now() + 6 * 86400_000 },
            subscriptionActiveUntil: null,
          },
          {
            id: "antigravity:desktop",
            provider: "antigravity",
            providerLabel: "Antigravity",
            label: "未登录",
            loggedIn: false,
            source: "未检测到",
            plan: null,
            windows: [],
            error: null,
            fetchedAt: Date.now(),
            resetCredits: null,
            subscriptionActiveUntil: null,
          },
        ] satisfies CliQuotaAccount[],
      };

    // ---- 分组 / 积分 / 签到趋势 / 用量 / 抓包日志 ----
    //
    // 这 8 条已登记进 `api.ts` 的 `DEMO_READ_COMMANDS`，**必须**在这里也有 case
    // —— 只登记不写数据时 `call()` 放行后落到 `default` 抛
    // 「演示模式缺少只读数据」，页面空白、看起来像"功能没做"。
    //
    // 演示值刻意覆盖各组件注释里点名的**边界展示状态**，走查时能一次看全：
    //   `null` ≠ `0`（这天没采到数据 ≠ 积分归零；当天没签到 ≠ 成功率 0）
    //   `error` 非空（拉取失败但有缓存时，要写明"展示的是缓存"）
    //   `status: "-"`（取不到响应状态）
    //   空分组 / `observedDays` 偏少（趋势可信度提示）
    case "groups_list": {
      const app = (args?.app === "Doubao" ? "Doubao" : "Trae") satisfies GroupApp;
      return {
        app,
        groups: [
          { id: "g-demo-1", name: "主力号", color: "emerald", order: 0, count: 2, uids: ["demo-user-001", "demo-user-002"] },
          { id: "g-demo-2", name: "备用号", color: "sky", order: 1, count: 1, uids: ["demo-user-003"] },
          // 空分组：刚建好还没拖账号进去 —— 界面要能正常显示，
          // 不能因为 count 为 0 就当脏数据藏掉
          { id: "g-demo-3", name: "待归档", color: "slate", order: 2, count: 0, uids: [] },
        ],
        // 只含仍然存在的分组；没有分组的 uid 不出现在这里
        membership: { "demo-user-001": "g-demo-1", "demo-user-002": "g-demo-1", "demo-user-003": "g-demo-2" },
      } satisfies GroupsView;
    }

    case "trae_credit_detail":
      return {
        userId: String(args?.userId ?? "demo-user-001"),
        total: 8420.5,
        packs: [
          { name: "个人版积分包", remaining: 1180.75, total: 2400, expireAt: new Date(futureAt(18)).toISOString() },
          // expireAt 为 null = 不过期（不是"已过期"）
          { name: "新用户体验包", remaining: 386.4, total: 800, expireAt: null },
        ],
        payIdentity: "个人版",
        payExpireAt: new Date(futureAt(96)).toISOString(),
        // 部分来源失败时仍返回已拿到的数据 ⇒ 界面要能"部分数据 + 逐来源说明"，
        // 而不是整块失败
        errors: ["权益包接口超时，上面是上次成功抓到的值"],
      } satisfies TraeCreditDetail;

    case "trae_credits_stats": {
      // 刻意留一天 total/consumed 都是 null：那天没采到数据。
      // 画成 0 会让人以为额度掉了 —— 这是注释里点名的"与积分归零不同"。
      const daily = [6, 5, 4, 2, 1, 0].map((ago, index) => ({
        date: localDate(ago),
        total: index === 1 ? null : 9000 - index * 120,
        consumed: index === 1 ? null : 120 + index * 8,
      })) satisfies TraeCreditsDay[];
      return {
        days: 30,
        daily,
        summary: {
          latestTotal: 9000,
          firstTotal: 8420.5,
          consumed: 720.5,
          // 只采到 6 天 ⇒ 趋势可信度有限，界面要据此提示
          observedDays: daily.length,
          change: 579.5,
        },
        accounts: [
          { userId: "demo-user-001", name: "测试 A", credits: 8420.5, updatedAt: new Date(atLocalTime(0, 9, 12)).toISOString() },
          { userId: "demo-user-002", name: "测试 B", credits: 2210, updatedAt: new Date(atLocalTime(0, 9, 19)).toISOString() },
          // 从没采到过 ⇒ credits/updatedAt 都为 null，界面显示「—」
          { userId: "demo-user-003", name: "测试 C", credits: null, updatedAt: null },
        ],
      } satisfies TraeCreditsStats;
    }

    case "trae_checkin_trends": {
      // 只含实际有记录的日期（**不补 0 高度的柱子**）；
      // 其中一天 successRate 为 null —— 那天没有账号需要签到，
      // 不是"成功率 0"（显示 0% 会被误读成全部失败）
      const points = [5, 4, 3, 1, 0].map((ago, index) => ({
        date: localDate(ago),
        ok: 2,
        already: index === 2 ? 3 : 0,
        failed: index === 3 ? 1 : 0,
        skipped: index === 4 ? 2 : 0,
        total: index === 2 ? 5 : 2,
        successRate: index === 1 ? null : 100,
      })) satisfies TraeCheckinTrendPoint[];
      return {
        days: 30,
        points,
        summary: {
          ok: 10,
          already: 3,
          failed: 1,
          skipped: 2,
          total: 14,
          observedDays: points.length,
          successRate: 92.9,
        },
      } satisfies TraeCheckinTrends;
    }

    case "trae_checkin_status": {
      // 三态都要出现：已签 / 未签 / **不知道**（没有键）。
      // 「不知道」必须是缺键而不是 false —— 演示数据也要示范正确的语义，
      // 否则截图会教人把"没问到"当成"未签到"。
      return {
        date: localDate(0),
        accounts: {
          "demo-user-001": { checkedIn: true, status: "success", name: "测试 A", updatedAt: `${localDate(0)}T09:12:00Z` },
          "demo-user-002": { checkedIn: false, status: "not_checked_in", name: "测试 B", updatedAt: `${localDate(0)}T09:19:00Z` },
          "demo-user-004": { checkedIn: null, status: "fail", name: "测试 D", updatedAt: `${localDate(0)}T09:21:00Z` },
        },
      } satisfies TraeCheckinTodayMap;
    }

    case "trae_usage_history": {
      const daily = [3, 2, 1, 0].map((ago, index) => ({
        date: localDate(ago),
        credits: 128.5 - index * 12,
        sessions: 9 - index,
        models: { "deepseek-v4-flash": 96.5 - index * 10, "kimi-k3-1": 32 - index * 2 } satisfies Record<string, number>,
        inputTokens: 184000 - index * 12000,
        outputTokens: 46000 - index * 3000,
        cacheReadTokens: 920000 - index * 60000,
      })) satisfies TraeUsageDay[];
      return {
        fetchedAt: Date.now(),
        // 演示里走"这是缓存"的样式，避免看起来像刚拉过上游
        cached: true,
        accounts: [
          {
            userId: "demo-user-001",
            name: "测试 A",
            ok: true,
            error: null,
            daily,
            totalCredits: 128.5,
            sessions: 9,
            // 按扣费降序
            models: [
              { model: "deepseek-v4-flash", credits: 96.5 },
              { model: "kimi-k3-1", credits: 32 },
            ],
          },
          {
            // 拉取失败但有缓存 ⇒ error 里要写明"展示的是缓存"，
            // 否则用户会把旧数据当成实时数据
            userId: "demo-user-002",
            name: "测试 B",
            ok: false,
            error: "上游超时，展示的是缓存",
            daily: [],
            totalCredits: 0,
            sessions: 0,
            models: [],
          },
        ],
      } satisfies TraeUsageHistory;
    }

    case "proxy_logs_list": {
      const entries = [
        { id: "2026-09-30.log:412", timestamp: `${localDate(0)} 09:31:04`, method: "HTTP GET", host: "api.workbuddy.ai", path: "/v1/models", status: "200", size: 18432 },
        {
          id: "2026-09-30.log:418",
          timestamp: `${localDate(0)} 09:32:47`,
          method: "HTTP POST",
          host: "api.workbuddy.ai",
          path: "/v1/chat/completions",
          status: "200",
          size: 96421,
          // SSE 流式请求才有这两个字段：模型名 + token 摘要
          sseModel: "deepseek-v4-flash",
          sseTokens: "p:120 c:340 t:460",
        },
        { id: "2026-09-30.log:433", timestamp: `${localDate(0)} 09:35:12`, method: "HTTP POST", host: "copilot.tencent.com", path: "/v2/chat", status: "429", size: 512 },
        // WebSocket 与"取不到响应状态"两种展示形态
        { id: "2026-09-30.log:441", timestamp: `${localDate(0)} 09:38:55`, method: "WebSocket", host: "gateway.example", path: "/ws", status: "-", size: 2048 },
      ] satisfies ProxyLogEntry[];
      return {
        entries,
        // total 是**过滤后的总数**，不是本页条数 —— 演示里刻意远大于本页，
        // 这样分页区的「共 N 条」才看得出与列表长度不是一回事
        total: 137,
      } satisfies ProxyLogList;
    }

    // 详情返回的是 `{ content }`（`proxyLogDetail` 内部取 `.content`）
    case "proxy_log_detail":
      return {
        content: [
          "HTTP GET https://api.workbuddy.ai/v1/models",
          "→ 200 OK (18,432 bytes)",
          "",
          "（演示模式下的假正文）",
        ].join("\n"),
      };

    case "proxy_logs_overview":
      return {
        dir: "/demo/proxy-logs",
        fileCount: 12,
        totalBytes: 8_642_304,
        oldest: `${localDate(29)} 00:12:03`,
        newest: `${localDate(0)} 09:38:55`,
      } satisfies ProxyLogsOverview;

    case "doubao_history": {
      // ★ 趋势**至少 2 天**：`doubao-insights.tsx:14` 里 `trend.length < 2`
      // 会显示「额度趋势需要至少两天的数据」，只给一天就等于演示模式看不到折线。
      // 纵轴固定 0~100，所以 usedPercent 用个位/十位数量级才像真实额度。
      const trend = [6, 5, 4, 3, 2, 1, 0].map((ago) => ({
        date: localDate(ago),
        usedPercent: 8.4 + (6 - ago) * 2.15,
        level: "个人版",
        uid: "demo-doubao-0001",
      }));
      return {
        // 事件按时间升序（组件自己 `reverse()` 后取前 8 条），这里给 6 条覆盖
        // 三种 kind × 成功/失败，走查时能看到「保活/续期/额度」三类标签
        // 与失败态的红色徽标。
        events: [
          { at: demoUtcIso(6, 9, 12), kind: "keepalive", ok: true, uid: "demo-doubao-0001", summary: "会话保活成功" },
          { at: demoUtcIso(5, 9, 8), kind: "quota", ok: true, uid: "demo-doubao-0001", level: "个人版", windows: [{ name: "5小时", usedPercent: 6.2, exhausted: false }] },
          { at: demoUtcIso(3, 21, 40), kind: "renew", ok: false, uid: "demo-doubao-0001", summary: "续期失败：上游返回 503，已保留原 sessionid" },
          { at: demoUtcIso(2, 9, 11), kind: "quota", ok: true, uid: "demo-doubao-0001", level: "个人版", windows: [{ name: "5小时", usedPercent: 7.8, exhausted: false }, { name: "7天", usedPercent: 41.3, exhausted: false }] },
          { at: demoUtcIso(1, 9, 9), kind: "keepalive", ok: true, uid: "demo-doubao-0001", summary: "会话保活成功" },
          { at: demoUtcIso(0, 9, 10), kind: "quota", ok: true, uid: "demo-doubao-0001", level: "个人版", windows: [{ name: "5小时", usedPercent: 8.4, exhausted: false }] },
        ] satisfies DoubaoHealthEvent[],
        trend,
        health: {
          // days 与 requestedDays 刻意不同：健康计数按**固定窗口**统计，
          // 事件/趋势按请求天数截取 —— 界面不能拿一个当另一个用
          days: 7,
          keepalive: 6,
          renew: 1,
          quota: 12,
          ok: 18,
          // 留一个失败：`failed > 0` 才渲染红色徽标，全绿就看不出这条路径
          failed: 1,
          lastKeepaliveAt: demoUtcIso(1, 9, 9),
        },
        requestedDays: 30,
      } satisfies DoubaoHistory;
    }

    default: throw new Error(`演示模式缺少只读数据: ${command}`);
  }
}

// demoQoderAccount / demoZcodeAccount 演示模式用的账号形状。
//
// ⚠ 字段名必须与 `src/lib/api.ts` 里的真实类型**逐字一致** ——
// 我第一版凭印象写成 `remaining` / `total` / `planTierName`，
// 而真实字段是 `credits` / `creditsTotal` / `expireAt`。
// 形状错了界面会渲染出「undefined」或直接崩，**比没有演示数据更糟**。
// 故这里用 `satisfies` 而不是 `as`：类型不匹配会在编译期报错，
// 而不是静默地把坏数据喂给界面。
//
// 若将来类型变了，这两处会编译失败 —— 那正是我们想要的提醒。
function demoQoderAccount(): QoderAccount {
  return {
    uid: "demo-qoder-0001",
    nickname: "演示 Qoder 账号",
    note: "演示用",
    region: "cn",
    disabled: false,
    createdAt: new Date(Date.now() - 30 * 86400_000).toISOString(),
    lastSeenAt: new Date().toISOString(),
    credits: 860,
    creditsTotal: 1000,
    expireAt: Math.floor(Date.now() / 1000) + 30 * 86400,
    models: ["qmodel_38max", "qmodel_38b"],
  } satisfies QoderAccount;
}

function demoZcodeAccount(): ZcodeAccount {
  return {
    uid: "demo-zcode-0001",
    nickname: "演示 ZCode 账号",
    note: "演示用",
    provider: "bigmodel",
    disabled: false,
    createdAt: new Date(Date.now() - 30 * 86400_000).toISOString(),
    lastSeenAt: new Date().toISOString(),
    credits: 299999978,
    creditsTotal: 300000000,
    expireAt: Math.floor(Date.now() / 1000) + 30 * 86400,
    // 演示数据按「已激活」渲染，避免截图里出现「从未激活」的提示
    warmedAt: Math.floor(Date.now() / 1000) - 3600,
  } satisfies ZcodeAccount;
}
