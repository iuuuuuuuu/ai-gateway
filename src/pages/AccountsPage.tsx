import { useCallback, useEffect, useRef, useState } from "react";
import { toast } from "sonner";
import {
  ChevronDown,
  Columns3,
  Download,
  FileDown,
  FileUp,
  Gift,
  GraduationCap,
  ListChecks,
  Loader2,
  MapPin,
  Moon,
  QrCode,
  MessageSquare,
  RefreshCw,
  Rows3,
  Sparkles,
  Terminal,
  Zap,
} from "lucide-react";

import { AccountCard } from "@/components/account-card";
import {
  AutoCareTasksCard,
  AutoCheckinCard,
  AutoRotateCard,
  PermissionCheckCard,
} from "@/components/workbuddy-settings";
import {
  PlatformConfigButton,
  PlatformConfigDialog,
} from "@/components/platform-config-dialog";
import { DemoAction } from "@/components/demo-action";
import { CodeBuddyCnIdeMark, CodeBuddyMark, WorkBuddyMark } from "@/components/product-marks";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Separator } from "@/components/ui/separator";
import { Switch } from "@/components/ui/switch";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { ExportAccountsDialog } from "@/components/export-accounts-dialog";
import { ImportAccountsDialog } from "@/components/import-accounts-dialog";
import { ImportLocalDialog } from "@/components/import-local-dialog";
import { SmsLoginDialog } from "@/components/sms-login-dialog";
import { OAuthLoginDialog } from "@/components/oauth-login-dialog";
import { SwitchAccountDialog } from "@/components/switch-account-dialog";
import { TaskQueuePanel } from "@/components/task-queue-panel";
import * as api from "@/lib/api";
import { useVisibilityInterval } from "@/lib/use-visibility-interval";
import type { AccountMeta, AccountRunningTask, AppStatus, CheckinConfig, CodeBuddyCliStatus, CodeBuddyCnIdeStatus, CreditExpiry, GatewayTaskName, GatewayTaskRuntime, TravelConfig, TravelStatus } from "@/lib/types";
import { cn } from "@/lib/utils";
import { useAccountsStore } from "@/stores/accounts";

/**
 * 按区域过滤账号：自动签到 / 自动旅行仅覆盖国服账号。
 *
 * 国际版（workbuddy.ai）的签到与旅行接口没有真实数据，自动任务永久不覆盖它们；
 * 界面上对应的状态标签也不查、不显示，避免长期停在「未签到 / 未旅行」。
 */
function accountsInScope(accounts: AccountMeta[]): AccountMeta[] {
  return accounts.filter((account) => account.regionKey !== "intl");
}

function expiringSoonAmount(credit?: CreditExpiry): number {
  return credit?.ok ? credit.expiringSoonRemaining ?? 0 : 0;
}

function hasExpiringSoonCredits(credit?: CreditExpiry): boolean {
  return credit?.ok === true && expiringSoonAmount(credit) > 0;
}

function soonestRelevantExpiry(credit?: CreditExpiry): number {
  const soonestExpiringCredit = (credit?.resources ?? [])
    .filter((resource) => resource.remaining > 0 && resource.expiringSoon && resource.expireAt != null)
    .map((resource) => resource.expireAt as number)
    .reduce((soonest, expireAt) => Math.min(soonest, expireAt), Number.POSITIVE_INFINITY);
  return Number.isFinite(soonestExpiringCredit)
    ? soonestExpiringCredit
    : credit?.soonestExpireAt ?? Number.POSITIVE_INFINITY;
}

function creditPriorityRank(credit?: CreditExpiry): number {
  if (!credit?.ok) return 3;
  if (hasExpiringSoonCredits(credit)) return 0;
  if (credit.expired) return 1;
  return 2;
}

function isWorkbuddyCurrent(account: AccountMeta, current: AppStatus["current"] | undefined): boolean {
  if (!current) return false;
  return Boolean(
    (current.uid && (account.uid === current.uid || account.id === current.uid)) ||
      (current.email && account.email === current.email),
  );
}

/**
 * 分批并发执行，限制**同时在途**的请求数。
 *
 * # 为什么必须限流（2026-09-22 所有者现场）
 *
 *	「最近几次打包后每次打开都先要反应一会儿,要不然点击就是无响应」
 *
 * # 根因：启动瞬间的并发风暴
 *
 * 下面两个 `Promise.all` 会对**每个账号**各发一次 IPC：
 *
 *	签到状态  19 个
 *	旅行状态  19 个
 *	         ─────
 *	          38 个并发 Tauri IPC
 *
 * 每个 IPC 都经宿主转发到网关、再打到上游。38 个同时涌入时：
 *   · WebView2 的主线程要处理 38 个 IPC 回调 → 界面卡住（"无响应"）
 *   · 网关/上游连接池被占满 → 后续点击的请求排队
 *
 * 且这发生在**打开页面的瞬间**，正是用户点什么都觉得"没反应"的时刻。
 *
 * # 取 4 并发
 *
 * 19 个账号分 5 批，每批 4 个，比一次 19 个平缓得多；
 * 又比串行（19 轮）快得多。这个数字与网关侧的 `max_in_flight=3`
 * 同量级 —— 再高也只是把压力推给上游。
 *
 * ⚠ 用 `Array.from(index)` 而不是 `map`：下面用索引做游标，
 * 需要精确控制"下一个取谁"，`map` 的迭代器语义在这里不直观。
 */
async function mapWithConcurrency<T, R>(
  items: T[],
  limit: number,
  fn: (item: T) => Promise<R>,
): Promise<R[]> {
  if (items.length === 0) return [];
  const results = new Array<R>(items.length);
  let cursor = 0;
  const workers = Array.from({ length: Math.min(limit, items.length) }, async () => {
    // 每个 worker 反复"取下一个还没被领的任务"，直到取完。
    // 用自增游标而不是切片：切片会让每个 worker 固定负责一段，
    // 某一段慢时会拖住整批，而游标法天然是"谁能干谁接着干"。
    for (;;) {
      const index = cursor++;
      if (index >= items.length) return;
      results[index] = await fn(items[index]);
    }
  });
  await Promise.all(workers);
  return results;
}

/** 并行查询今日签到；失败的账号不写入，由调用方保留原值。 */
async function fetchTodayCheckinMap(
  accountIds: string[],
  isStale?: () => boolean,
): Promise<Record<string, boolean>> {
  const entries = await mapWithConcurrency(accountIds, STARTUP_FETCH_CONCURRENCY, async (id) => {
    try {
      const res = await api.getCheckinStatus(id);
      if (isStale?.() || !res.ok) return null;
      return [id, res.todayCheckedIn] as const;
    } catch {
      return null;
    }
  });
  const next: Record<string, boolean> = {};
  for (const entry of entries) {
    if (entry) next[entry[0]] = entry[1];
  }
  return next;
}

/** 并行查询各账号今日旅行状态；失败的账号不写入，由调用方保留原值。 */
async function fetchTravelMap(
  accountIds: string[],
  isStale?: () => boolean,
): Promise<Record<string, TravelStatus>> {
  const entries = await mapWithConcurrency(accountIds, STARTUP_FETCH_CONCURRENCY, async (id) => {
    try {
      const res = await api.getTravelStatus(id);
      if (isStale?.()) return null;
      return [id, res] as const;
    } catch {
      return null;
    }
  });
  const next: Record<string, TravelStatus> = {};
  for (const entry of entries) {
    if (entry) next[entry[0]] = entry[1];
  }
  return next;
}

/**
 * 启动期拉取账号状态的**在途并发上限**。
 *
 * 见 `mapWithConcurrency` 的说明：不限流时 19 个账号会一次性打出
 * 38 个并发 IPC，把 WebView2 主线程堵住（表现为"打开后一会儿才响应"）。
 */
const STARTUP_FETCH_CONCURRENCY = 4;

export default function AccountsPage() {
  const {
    accounts,
    status,
    loading,
    error,
    fetchAll,
    deleteAccount,
    importLocal,
    creditMap,
    creditLoadingMap,
    creditUpdatedAtMap,
    refreshingCredits,
    ensureCredits,
    refreshCredits,
  } = useAccountsStore();
  const [oauthOpen, setOauthOpen] = useState(false);
  /** WorkBuddy 平台配置弹窗（2026-09-22 从设置页迁来，收进弹窗）。 */
  const [workbuddyConfigOpen, setWorkbuddyConfigOpen] = useState(false);
  const [exportOpen, setExportOpen] = useState(false);
  const [importOpen, setImportOpen] = useState(false);
  /** 跨账号任务队列面板（批量跑成长任务，带实时进度） */
  const [taskQueueOpen, setTaskQueueOpen] = useState(false);
  /** 从本机导入（扫描当前登录态 + 历史快照 + 切换备份）弹框 */
  const [importLocalOpen, setImportLocalOpen] = useState(false);
  /** 手机号 + 短信验证码登录弹窗（2026-09-22 新增）。 */
  const [smsOpen, setSmsOpen] = useState(false);
  const [switchAccount, setSwitchAccount] = useState<AccountMeta | null>(null);
  const [autoCheckinConfig, setAutoCheckinConfig] = useState<CheckinConfig | null>(null);
  const [autoCheckinSaving, setAutoCheckinSaving] = useState(false);
  /** 账号 id -> 今日是否已签到（undefined=查询中/未知） */
  const [checkinMap, setCheckinMap] = useState<Record<string, boolean>>({});
  const [autoTravelConfig, setAutoTravelConfig] = useState<TravelConfig | null>(null);
  const [autoTravelSaving, setAutoTravelSaving] = useState(false);
  /** 账号 id -> 今日旅行状态（undefined=查询中/未知） */
  const [travelMap, setTravelMap] = useState<Record<string, TravelStatus>>({});
  const [codebuddyCli, setCodebuddyCli] = useState<CodeBuddyCliStatus | null>(null);
  const [codebuddyCliSwitchingId, setCodebuddyCliSwitchingId] = useState<string | null>(null);
  const [codebuddyCnIde, setCodebuddyCnIde] = useState<CodeBuddyCnIdeStatus | null>(null);
  const [codebuddyCnIdeSwitchingId, setCodebuddyCnIdeSwitchingId] = useState<string | null>(null);
  const [installingCodebuddyCli, setInstallingCodebuddyCli] = useState(false);
  /** 刷新按钮触发的批量签到进行中 */
  const [checkinAllRunning, setCheckinAllRunning] = useState(false);
  /** 一键旅行进行中（下拉菜单项） */
  const [travelRunning, setTravelRunning] = useState(false);
  /**
   * 正在执行的养号任务名（账号卡片菜单的「养号任务（仅本账号）」组，
   * 以及右上角「一键操作 → 一键养号」的全账号版本）。
   *
   * 单一状态而非按卡片分组：同一时刻只应有一个在跑；按卡片分组反而会让人
   * 以为每个号各跑各的。菜单项据它整体置灰，避免重复触发造成成倍上报。
   */
  const [taskRunning, setTaskRunning] = useState<GatewayTaskName>();
  /**
   * 正在执行的**成长任务**码（当前只有校园日 school_season）。
   *
   * 与 taskRunning 分开：成长任务与养号任务是两套接口
   *（`/tasks/growth` 逐任务 vs `/tasks/run` 整轮），
   * 共用一个状态会让「哪个菜单项该转圈」判断不出来。
   */
  const [growthTaskRunning, setGrowthTaskRunning] = useState<string>();
  /**
   * 正在跑「一键执行本账号全部任务」的账号 id；null = 没有在跑。
   *
   * ⚠ 与 `growthTaskRunning` 分开是必要的：那个是字符串状态，
   * 逐项执行时存 taskCode，整轮执行时存哨兵 `"__all__"` ——
   * 而"整轮"必须知道**是哪个账号**在跑：一次只能有一个账号跑整轮
   *（后端有账号级互斥，两个账号并发整轮会互相抢不到锁），
   * 界面据此只禁用那一张卡片的按钮，而不是全部。
   */
  const [growthAllAccountId, setGrowthAllAccountId] = useState<string | null>(null);
  /**
   * 网关侧正在执行的养号任务（含「哪些账号已跑过」）。
   *
   * 为什么不能复用上面的 `taskRunning`：那个只覆盖**本页这一次请求**，
   * 一旦任务由别处（设置页的「立即执行」）触发、或本页刷新后，它就是空的，
   * 而任务其实还在跑。网关侧状态是唯一权威来源，卡片标记必须读它。
   */
  const [taskRuntime, setTaskRuntime] = useState<GatewayTaskRuntime | null>(null);
  /** 接入/升级 CLI helper 确认框 */
  const [installConfirmOpen, setInstallConfirmOpen] = useState(false);
  /** 删除账号确认目标（null=关闭） */
  const [deleteTarget, setDeleteTarget] = useState<AccountMeta | null>(null);
  /** 紧凑模式：卡片更小、同屏更多列；默认开启，持久化到 localStorage */
  const [compact, setCompact] = useState<boolean>(() => {
    try {
      return localStorage.getItem("ai-gateway.compact") !== "0";
    } catch {
      return true;
    }
  });

  function toggleCompact() {
    setCompact((value) => {
      const next = !value;
      try {
        localStorage.setItem("ai-gateway.compact", next ? "1" : "0");
      } catch {
        /* 存储不可用时静默 */
      }
      return next;
    });
  }

  useEffect(() => {
    void fetchAll();
  }, [fetchAll]);

  /**
   * 今日签到 / 旅行状态只查询范围内账号（默认仅国服）。
   *
   * 国际版账号既不会自动签到也不会有旅行数据，若仍去查状态，卡片会长期显示
   * 「未签到 / 未旅行」，且每轮都为它多打两次无效请求；改为只查范围内账号，
   * 卡片自然不渲染这两个标签。
   */
  const scopedAccounts = accountsInScope(accounts);
  const travelScopedAccounts = accountsInScope(accounts);

  useEffect(() => {
    let cancelled = false;
    void api
      .getAutoCheckinConfig()
      .then((config) => {
        if (!cancelled) setAutoCheckinConfig(config);
      })
      .catch((e) => {
        if (!cancelled) {
          toast.error("自动签到配置加载失败", { description: api.asError(e) });
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    void api
      .getAutoTravelConfig()
      .then((config) => {
        if (!cancelled) setAutoTravelConfig(config);
      })
      .catch((e) => {
        if (!cancelled) {
          toast.error("自动旅行配置加载失败", { description: api.asError(e) });
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  /** 首次启动自动导入本机账号（本会话只尝试一次，无本机账号时静默） */
  const autoImportTried = useRef(false);
  useEffect(() => {
    if (autoImportTried.current || loading || accounts.length > 0) return;
    autoImportTried.current = true;
    void importLocal()
      .then(() => void fetchAll())
      .catch(() => {
        /* 本机无 WorkBuddy 登录态时静默，不打扰用户 */
      });
  }, [accounts.length, loading, importLocal, fetchAll]);

  async function refreshCodebuddyCliStatus() {
    try {
      setCodebuddyCli(await api.getCodebuddyCliStatus());
    } catch {
      setCodebuddyCli(null);
    }
  }

  async function refreshCodebuddyCnIdeStatus() {
    try {
      setCodebuddyCnIde(await api.getCodebuddyCnIdeStatus());
    } catch {
      setCodebuddyCnIde(null);
    }
  }

  useEffect(() => {
    let cancelled = false;
    void refreshCodebuddyCliStatus();
    void (async () => {
      if (!api.isDemoMode()) {
        try {
          await api.detectCodebuddyCnIdeAccount();
        } catch {
          /* 未登录或钥匙串拒绝时静默，下面仍拉安装/运行状态 */
        }
      }
      if (!cancelled) await refreshCodebuddyCnIdeStatus();
    })();
    return () => {
      cancelled = true;
    };
  }, [accounts.length]);

  // 账号列表变化后并行查询各账号今日签到状态（仅查国服账号）
  useEffect(() => {
    if (!scopedAccounts.length) {
      setCheckinMap({});
      return;
    }
    let cancelled = false;
    void fetchTodayCheckinMap(
      scopedAccounts.map((account) => account.id),
      () => cancelled,
    ).then((next) => {
      if (!cancelled) setCheckinMap(next);
    });
    return () => {
      cancelled = true;
    };
  }, [accounts]);

  async function loadTravelMap(accountIds: string[], isStale?: () => boolean) {
    const next = await fetchTravelMap(accountIds, isStale);
    if (!isStale?.() && Object.keys(next).length > 0) {
      setTravelMap((prev) => ({ ...prev, ...next }));
    }
  }

  // 旅行状态轮询（60 秒一轮）。
  //
  // 用 useVisibilityInterval：窗口隐藏 / 收进托盘时**销毁**定时器，不在后台空转。
  // 原先的裸 setInterval 只在组件卸载时清理，隐藏期间仍每 60 秒打一轮请求。
  // onResume 让切回来时立刻补一次，卡片不会停在过期状态。
  const travelScopedIds = travelScopedAccounts.map((account) => account.id);
  useVisibilityInterval(() => void loadTravelMap(travelScopedIds), 60_000, {
    enabled: travelScopedIds.length > 0,
    // 首次加载由下方 useEffect 负责（它还要处理「无账号时清空」），
    // 这里传 false 避免挂载时重复请求一次。
    immediate: false,
    onResume: () => void loadTravelMap(travelScopedIds),
  });

  // 账号列表变化后立刻查一次旅行状态（周期轮询由上面的 hook 负责）。
  // 同样只查国服账号：国际版的旅行接口无数据，查了只会一直显示「未旅行」。
  useEffect(() => {
    if (!travelScopedAccounts.length) {
      setTravelMap({});
      return;
    }
    let cancelled = false;
    const ids = travelScopedAccounts.map((account) => account.id);
    void loadTravelMap(ids, () => cancelled);
    return () => {
      cancelled = true;
    };
  }, [accounts]);

  // 网关任务运行态轮询（5 秒一轮）。
  //
  // 为什么本页要自己拉一次 `gateway_status()`：任务运行态（在跑什么、哪些账号
  // 已跑过）只在这个接口里（`taskRuntime`）。本页此前**没有任何** status 轮询
  // —— 已有的两条轮询分别是 `getTravelStatus`（60 秒）与积分，都不含该字段；
  // 而设置页那个 2 秒轮询只在设置页挂载，切回本页就没了。所以这里必须新增一条，
  // 而不是「复用既有的」（确实没有可复用的）。
  //
  // 周期取 5 秒（与网关页的 status 轮询同档，而不是设置页的 2 秒）：设置页 2 秒
  // 是为了盯着进度条看，而本页只需「一眼看到在跑什么」。5 秒对一轮 40 秒以上的
  // 任务来说最迟 12% 处就能看到标记，同时把这条**较重**的接口（宿主每次都要
  // 探活网关 + 解析账号记录文件）的开销压到可接受范围 —— 本页是默认落地页，
  // 常驻打开，2 秒一轮会长期空转。
  //
  // 用 useVisibilityInterval：窗口隐藏/收进托盘时**销毁**定时器，不在后台空转
  //（与旅行轮询、设置页、网关页同一做法）。
  //
  // 失败静默清空（与设置页 loadRuntime 一致）：这是旁路观测数据，网关没起来时
  // 本来就查不到，为此弹提示只会制造噪音。清空而不是保留旧值 —— 保留会让任务
  // 结束后仍挂着一个不复存在的标记。
  const loadTaskRuntime = useCallback(async () => {
    try {
      const status = await api.getGatewayStatus();
      setTaskRuntime(status.taskRuntime ?? null);
    } catch {
      setTaskRuntime(null);
    }
  }, []);

  useVisibilityInterval(() => void loadTaskRuntime(), 5000, {
    onResume: () => void loadTaskRuntime(),
  });

  // 只给尚未缓存的账号拉积分；切回首页不重复请求。点「刷新积分」才强制更新。
  useEffect(() => {
    if (!accounts.length) return;
    void ensureCredits(accounts.map((account) => account.id));
  }, [accounts, ensureCredits]);

  /** 从本机批量导入完成提示（含新增/覆盖拆分）。 */
  function onLocalImported(result: { imported: number; added: number; updated: number }) {
    const parts = [`新增 ${result.added} 个`];
    if (result.updated > 0) parts.push(`更新 ${result.updated} 个`);
    toast.success(`已导入 ${result.imported} 个账号`, { description: parts.join("，") });
    void fetchAll();
  }

  async function onAutoCheckinChange(enabled: boolean) {
    if (!autoCheckinConfig || autoCheckinSaving) return;
    const previous = autoCheckinConfig;
    const next = { ...previous, enabled };
    setAutoCheckinConfig(next);
    setAutoCheckinSaving(true);
    try {
      setAutoCheckinConfig(await api.saveAutoCheckinConfig(next));
    } catch (e) {
      setAutoCheckinConfig(previous);
      toast.error("自动签到设置保存失败", { description: api.asError(e) });
    } finally {
      setAutoCheckinSaving(false);
    }
  }

  async function onAutoTravelChange(enabled: boolean) {
    if (!autoTravelConfig || autoTravelSaving) return;
    const previous = autoTravelConfig;
    const next = { ...previous, enabled };
    setAutoTravelConfig(next);
    setAutoTravelSaving(true);
    try {
      setAutoTravelConfig(await api.saveAutoTravelConfig(next));
      if (enabled) {
        toast.success("自动旅行已开启", { description: "正在按官方状态派发或领取" });
        window.setTimeout(() => {
          void loadTravelMap(accountsInScope(accounts).map((account) => account.id));
        }, 2500);
      }
    } catch (e) {
      setAutoTravelConfig(previous);
      toast.error("自动旅行设置保存失败", { description: api.asError(e) });
    } finally {
      setAutoTravelSaving(false);
    }
  }

  /** 导出完成提示（含安全提醒）。 */
  function onExported(count: number) {
    const text = `已导出 ${count} 个账号。文件含登录 token，等同密码，请勿上传网盘或发送给他人。`;
    toast.success("导出成功", { description: text });
  }

  /** 导入完成提示：计数 + token 可能过期提醒，并刷新列表。 */
  function onImported(result: { imported: number; skipped: number; overwritten: number }) {
    void fetchAll();
    const overwriteText = result.overwritten > 0 ? `（覆盖 ${result.overwritten} 个）` : "";
    const text = `已导入 ${result.imported} 个${overwriteText}，跳过 ${result.skipped} 个。token 可能已过期，切换后可能需要重新登录。`;
    toast.success("导入成功", { description: text });
  }

  async function onDelete(a: AccountMeta) {
    // 桌面 App（Tauri WebView）不支持 window.confirm，改用 Dialog 确认
    setDeleteTarget(a);
  }

  async function confirmDelete() {
    if (!deleteTarget) return;
    const a = deleteTarget;
    setDeleteTarget(null);
    try {
      await deleteAccount(a.id);
      toast.success("账号已删除");
    } catch (e) {
      toast.error("删除失败", { description: api.asError(e) });
    }
  }

  async function onCheckin(a: AccountMeta) {
    try {
      const res = await api.checkin(a.id);
      const label =
        res.result === "success"
          ? "签到成功"
          : res.result === "already"
            ? "今天已签到"
            : "签到失败";
      const description = `${a.nickname || a.email || a.id}${res.error ? `：${res.error}` : ""}`;
      if (res.result === "error") toast.error(label, { description });
      else toast.success(label, { description });
      // 刷新该账号的今日签到状态
      try {
        const st = await api.getCheckinStatus(a.id);
        if (st.ok) setCheckinMap((prev) => ({ ...prev, [a.id]: st.todayCheckedIn }));
      } catch {
        /* ignore */
      }
      void fetchAll();
      // 签到成功/已签到会带来积分变动，force 刷新该账号积分
      if (res.result !== "error") void refreshCredits([a.id]);
    } catch (e) {
      toast.error("签到失败", { description: api.asError(e) });
    }
  }

  /**
   * 单账号领养 Buddy：只领养，不派猫、不消耗当日派出次数。
   *
   * 各结果的文案要分开 —— 尤其「对话轮次不够」是上游的**预期**门槛，
   * 不该报成失败，否则用户会以为功能坏了。
   */
  async function onAdopt(a: AccountMeta) {
    const who = a.nickname || a.email || a.id;
    const toastId = toast.loading("正在领养 Buddy…", { description: who });
    try {
      const res = await api.travelAdopt(a.id);
      switch (res.skip) {
        case "has-buddy":
          toast.info("已有 Buddy", { id: toastId, description: who });
          break;
        case "adopted":
          toast.success("领养成功", { id: toastId, description: `${who} 已领养，通常赠送 300 分` });
          break;
        case "adopt-threshold":
          toast.info("暂不能领养", {
            id: toastId,
            description: `${who} 需先积累足够的对话轮次，之后可再试`,
          });
          break;
        case "buddy-unknown":
          toast.error("查询失败", { id: toastId, description: `${who}：${res.message || "无法查询 Buddy 状态"}` });
          break;
        default:
          if (res.ok) toast.success("领养成功", { id: toastId, description: who });
          else toast.error("领养失败", { id: toastId, description: `${who}：${res.message || "请稍后重试"}` });
      }
      // 领养会改变旅行状态与积分，回读一次
      void loadTravelMap([a.id]);
      if (res.skip === "adopted") void refreshCredits([a.id]);
    } catch (e) {
      toast.error("领养失败", { id: toastId, description: api.asError(e) });
    }
  }

  async function onRefresh(a: AccountMeta) {
    try {
      const res = await api.refreshAccountToken(a.id);
      const label = a.nickname || a.email || a.id;
      if (res.needsRelogin) {
        toast.error("Token 刷新失败", { description: `${label}：需重新登录${res.needsReloginReason ? `（${res.needsReloginReason}）` : ""}` });
      } else {
        toast.success("Token 已刷新", { description: label });
      }
      void fetchAll();
    } catch (e) {
      toast.error("Token 刷新失败", { description: api.asError(e) });
    }
  }

  /**
   * 手动触发养号任务（活跃上报 / 夜猫子 / 开学季 / trial / 活跃地图）。
   *
   * **只作用于指定账号**（2026-09-18 修正）：原实现是「整轮触发、作用于全部账号」，
   * 而入口长在账号卡片上 —— 用户在某个号上点「活跃上报」，跑的却是整池，
   * 与菜单位置传达的意思相反（所有者明确指出）。
   *
   * 全账号版本在页面右上角「一键操作」里，走 `onRunAllTasks`。
   *
   * `ran=false` 是正常结果（夜猫子不在时段、区域不符、账号不在池中），
   * 按说明展示而非报错 —— 否则用户会把「上游不计入」当成功能坏了。
   */
  async function onRunTask(task: GatewayTaskName, accountId: string) {
    setTaskRunning(task);
    try {
      const res = await api.runGatewayTask(task, accountId);
      if (!res.ok) {
        toast.error("任务执行失败", { description: res.error || "未知错误" });
      } else if (!res.ran) {
        toast.info("本次未执行", { description: res.message || "前置条件不满足" });
      } else {
        toast.success("已执行该账号", { description: res.message || "任务已完成" });
      }
      // 任务会写账号记录与积分，回读一次让「账号记录」立刻反映
      if (res.ok && res.ran) void fetchAll();
    } catch (e) {
      toast.error("任务执行失败", { description: api.asError(e) });
    } finally {
      setTaskRunning(undefined);
    }
  }

  /**
   * 手动执行该账号**全部可自动完成的成长任务**（所有者 2026-09-22 要求）。
   *
   *	「workbuddy每个账号再加一个按钮,就是点击后 可以一键执行
   *	  当前 账号 所能执行的全部任务」
   *
   * # 为什么不需要新接口
   *
   * `action=run` **不传 taskCode** 时后端就是跑该账号的全部待办
   *（见 `growth_tasks.go` 的 `runOne`：`if code == "" { r.RunAll(...) }`）——
   * 它内部含报名、回读进度、自动领奖，比前端逐项循环更省请求也更不容易漏。
   *
   * ⚠ 但它**要等整个账号跑完才一次性返回**（十几项、每项可能含真实对话，
   * 分钟级）。故必须：
   *   · 给出明确的"正在执行"提示（否则用户以为没反应会连点）
   *   · 全程禁用按钮（后端有账号级互斥，连点会让第二次直接失败）
   *
   * ⚠ `"__all__"` 是**哨兵值**，用于让卡片知道是"整轮"在跑而非某项：
   * 逐项执行时这个变量存 taskCode，两者必须能区分开，
   * 否则界面会显示成"正在执行某个不存在的任务"。
   */
  async function onRunAllGrowthTasks(accountId: string) {
    setGrowthTaskRunning("__all__");
    setGrowthAllAccountId(accountId);
    toast.info("正在执行该账号的全部任务", {
      description:
        "十几项任务依次执行，可能包含真实对话，通常需要一到几分钟。请勿重复点击。",
      duration: 8000,
    });
    try {
      const res = await api.runGrowthTask("run", accountId);
      if (!res.ok) {
        toast.error("一键执行失败", { description: res.error || "未知错误" });
      } else {
        // 汇总只给计数，逐项 message 太长；细节交给「查看记录」
        //（本次同时修好了记录的标题粒度，现在每项一条）。
        const items = res.items ?? [];
        const failed = items.filter((i) => i.status === "error");
    const done = items.filter((i) => i.status === "done");
        const skipped = items.filter((i) => i.status === "skipped");
        const desc =
          `共 ${items.length} 项：完成 ${done.length}、跳过 ${skipped.length}、失败 ${failed.length}` +
          (failed.length > 0
            ? `\n失败项：${failed.map((i) => i.task_code).join("、")}`
            : "");
        if (failed.length > 0) {
          toast.error("部分任务未完成", { description: desc, duration: 10000 });
        } else {
          toast.success("本账号任务已执行", { description: desc, duration: 8000 });
        }
      }
      void fetchAll();
    } catch (e) {
      toast.error("一键执行失败", { description: api.asError(e) });
    } finally {
      setGrowthTaskRunning(undefined);
      setGrowthAllAccountId(null);
    }
  }

  /**
   * 手动执行一个**成长任务**（当前只有校园日 school_season）。
   *
   * 与 onRunTask 的区别不只是接口：成长任务是**逐任务**的，且校园日的完成
   * 条件是小程序内对话 —— 它走 `/tasks/growth`（带 taskCode），
   * 而养号任务走 `/tasks/run`（只有任务名）。两者混用会静默调错接口。
   */
  async function onRunGrowthTask(taskCode: string, accountId: string) {
    setGrowthTaskRunning(taskCode);
    try {
      const res = await api.runGrowthTask("run", accountId, taskCode);
      if (!res.ok) {
        toast.error("成长任务执行失败", { description: res.error || "未知错误" });
      } else {
        // 成长任务返回**逐任务**结果：只有把该任务的 message 报出来，
        // 用户才知道到底成了没有（`status` 区分 done/skipped/error/unsupported）。
        const item = res.item ?? res.items?.find((i) => i.task_code === taskCode);
        const detail = item?.message || "任务已执行";
        if (item?.status === "error") {
          toast.error("校园日活动未完成", { description: detail });
        } else if (item?.status === "unsupported") {
          toast.info("该任务不支持自动完成", { description: detail });
        } else {
          toast.success("校园日活动", { description: detail });
        }
      }
      if (res.ok) void fetchAll();
    } catch (e) {
      toast.error("成长任务执行失败", { description: api.asError(e) });
    } finally {
      setGrowthTaskRunning(undefined);
    }
  }

  /**
   * 对**全部账号**跑一轮养号任务（右上角「一键操作」用）。
   *
   * 与 onRunTask 同一接口，只是**不传 accountId** —— 网关据此遍历整池。
   * 保留这两条路径是刻意的：「只跑这个号」与「跑全部」是两种不同意图，
   * 不能只留其一（前者用于单个号出问题时重试，后者用于日常一键养护）。
   */
  async function onRunAllTasks(task: GatewayTaskName) {
    setTaskRunning(task);
    try {
      const res = await api.runGatewayTask(task);
      if (!res.ok) {
        toast.error("任务执行失败", { description: res.error || "未知错误" });
      } else if (!res.ran) {
        toast.info("本次未执行", { description: res.message || "前置条件不满足" });
      } else {
        toast.success("已触发一轮（全部账号）", { description: res.message || "任务已开始执行" });
      }
      if (res.ok && res.ran) void fetchAll();
    } catch (e) {
      toast.error("任务执行失败", { description: api.asError(e) });
    } finally {
      setTaskRunning(undefined);
    }
  }

  /** 刷新按钮：先跑一轮批量签到并重查今日签到状态，再强制刷新全部积分。 */
  async function onRefreshCredits() {
    if (!accounts.length || refreshingCredits || checkinAllRunning) return;
    setCheckinAllRunning(true);
    try {
      try {
        const res = await api.checkinAll();
        const entries = res.accounts ?? [];
        const success = entries.filter((e) => e.result === "success").length;
        const already = entries.filter((e) => e.result === "already").length;
        const failed = entries.filter((e) => e.result === "error").length;
        const parts: string[] = [];
        if (success > 0) parts.push(`${success} 个签到成功`);
        if (already > 0) parts.push(`${already} 个已签到`);
        if (failed > 0) parts.push(`${failed} 个失败`);
        const summary = parts.length > 0 ? parts.join("，") : "无账号需要签到";
        if (entries.length > 0 && failed === entries.length) {
          toast.error("签到失败", { description: summary });
        } else {
          toast.success("签到完成", { description: summary });
        }
        // 批量签到后重查国服账号的今日签到状态，无需切换页面即反映最新结果
        const next = await fetchTodayCheckinMap(scopedAccounts.map((account) => account.id));
        if (Object.keys(next).length > 0) {
          setCheckinMap((prev) => ({ ...prev, ...next }));
        }
      } catch (e) {
        toast.error("批量签到失败", { description: api.asError(e) });
      }
      await refreshCredits(accounts.map((account) => account.id));
      await loadTravelMap(travelScopedAccounts.map((account) => account.id));
      toast.success("积分到期情况已刷新");
    } finally {
      setCheckinAllRunning(false);
    }
  }

  /**
   * 一键旅行：对所有国服账号走一趟巡检（领养 → 派出 → 领奖）。
   *
   * 与「刷新」按钮的区别：刷新跑的是签到，这里跑旅行，两者互不包含。
   * 手动触发不走「自动旅行」开关（后端 `run_travel_now` 同样不检查），
   * 否则用户没开自动旅行时点它会毫无反应。
   */
  async function onTravelRun() {
    if (!accounts.length || travelRunning || checkinAllRunning) return;
    setTravelRunning(true);
    const toastId = toast.loading("正在派猫猫旅行…", { description: "包含领养、派出与领取奖励" });
    try {
      const res = await api.travelRun();
      if (res.status === "skipped") {
        toast.info("旅行正在进行中", { id: toastId, description: "请稍候，上一轮尚未结束" });
        return;
      }
      if (res.status === "no_accounts") {
        toast.info("没有可旅行的账号", { id: toastId, description: "仅国服账号参与" });
        return;
      }
      const entries = res.accounts ?? [];
      const adopted = entries.filter((e) => e.skip === "adopted").length;
      const departed = entries.filter((e) => e.result === "success").length;
      const threshold = entries.filter((e) => e.skip === "adopt-threshold").length;
      const failed = entries.filter((e) => e.result === "error").length;
      const parts: string[] = [];
      if (adopted > 0) parts.push(`${adopted} 个已领养`);
      if (departed > 0) parts.push(`${departed} 个已派出/已领奖`);
      if (threshold > 0) parts.push(`${threshold} 个需先积累对话`);
      if (failed > 0) parts.push(`${failed} 个失败`);
      const summary = parts.length > 0 ? parts.join("，") : "本轮无动作";
      if (entries.length > 0 && failed === entries.length) {
        toast.error("旅行失败", { id: toastId, description: summary });
      } else {
        toast.success("旅行完成", { id: toastId, description: summary });
      }
      // 立刻回读旅行状态，让卡片不必等下一轮轮询
      await loadTravelMap(travelScopedAccounts.map((account) => account.id));
    } catch (e) {
      toast.error("一键旅行失败", { id: toastId, description: api.asError(e) });
    } finally {
      setTravelRunning(false);
    }
  }

  async function onSwitchCodebuddyCli(account: AccountMeta) {
    if (codebuddyCliSwitchingId !== null) return;
    setCodebuddyCliSwitchingId(account.id);
    const toastId = toast.loading("正在切换 CodeBuddy CLI…", {
      description: `正在将默认账号设为 ${account.nickname || account.email || account.id}`,
    });
    try {
      const result = await api.switchCodebuddyCliAccount(account.id);
      await refreshCodebuddyCliStatus();
      toast.success("CodeBuddy CLI 默认账号已更新", {
        id: toastId,
        description: `${account.nickname || account.email || account.id}：${result.message || "配置已更新"}`,
      });
    } catch (error) {
      toast.error("CodeBuddy CLI 切换失败", {
        id: toastId,
        description: api.asError(error),
      });
    } finally {
      setCodebuddyCliSwitchingId(null);
    }
  }

  async function onSwitchCodebuddyCnIde(account: AccountMeta) {
    if (codebuddyCnIdeSwitchingId !== null) return;
    setCodebuddyCnIdeSwitchingId(account.id);
    const toastId = toast.loading("正在切换 CodeBuddy IDE…", {
      description: "将注入凭证并重启 CodeBuddy IDE",
    });
    try {
      const result = await api.switchCodebuddyCnIdeAccount(account.id, true);
      await refreshCodebuddyCnIdeStatus();
      toast.success("CodeBuddy IDE 已切换", {
        id: toastId,
        description: result.message || result.account,
      });
    } catch (error) {
      toast.error("CodeBuddy IDE 切换失败", {
        id: toastId,
        description: api.asError(error),
      });
    } finally {
      setCodebuddyCnIdeSwitchingId(null);
    }
  }

  async function onInstallCodebuddyCli() {
    // 桌面 App（Tauri WebView）不支持 window.confirm，改用 Dialog 确认
    setInstallConfirmOpen(true);
  }

  async function confirmInstallCodebuddyCli() {
    setInstallConfirmOpen(false);
    setInstallingCodebuddyCli(true);
    try {
      const result = await api.installCodebuddyCliHelper();
      toast.success("CodeBuddy CLI 接入已更新", { description: result.message });
      await refreshCodebuddyCliStatus();
    } catch (error) {
      toast.error("CodeBuddy CLI 接入失败", { description: api.asError(error) });
    } finally {
      setInstallingCodebuddyCli(false);
    }
  }

  const current = status?.current;
  const creditOrderingReady =
    accounts.length > 0 &&
    accounts.every((account) => Boolean(creditMap[account.id]) && !creditLoadingMap[account.id]);
  const orderedAccounts = creditOrderingReady
    ? accounts
        .map((account, index) => ({ account, index }))
        .sort((left, right) => {
          const leftCredit = creditMap[left.account.id];
          const rightCredit = creditMap[right.account.id];
          const rankDifference = creditPriorityRank(leftCredit) - creditPriorityRank(rightCredit);
          if (rankDifference !== 0) return rankDifference;

          const leftExpiry = soonestRelevantExpiry(leftCredit);
          const rightExpiry = soonestRelevantExpiry(rightCredit);
          if (leftExpiry !== rightExpiry) return leftExpiry - rightExpiry;

          const amountDifference = expiringSoonAmount(rightCredit) - expiringSoonAmount(leftCredit);
          if (amountDifference !== 0) return amountDifference;
          return left.index - right.index;
        })
        .map(({ account }) => account)
    : accounts;
  const priorityAccountId =
    creditOrderingReady
      ? orderedAccounts.find((account) => hasExpiringSoonCredits(creditMap[account.id]))?.id
      : undefined;
  /**
   * 「本轮已跑」标记：账号库 id → 标记内容。
   *
   * 判定口径必须用 `account.id`（账号库主键），**不能用 uid**：后端
   * `taskRuntime.processedIds` 就是账号库 id（Rust 侧 `task_runtime` 的注释
   * 明确写过「用 uid 会一个都对不上，且不会有任何报错」）。而本页的
   * `AccountMeta` 恰好两者都有，写错在这里不会报错、只会静默不显示。
   *
   * `label` 缺省时**不**给标记：标签文案就是「在跑什么」，没有任务名的标记
   * 是一句无信息量的「本轮已跑」，不值得占卡片位置。
   *
   * 不在本轮范围内的账号（区域不符 / 已禁用 / 需重登）不会出现在 `processedIds`
   * 里 —— 后端 `total` 与 Go 侧各任务的过滤条件一致地排除了它们，因此这些卡片
   * 自然不显示标记，不会被误认为「所有号都在跑」。
   */
  const runningTaskByAccountId = new Map<string, AccountRunningTask>();
  if (taskRuntime?.running && taskRuntime.label) {
    for (const id of taskRuntime.processedIds ?? []) {
      runningTaskByAccountId.set(id, {
        label: taskRuntime.label,
        processed: taskRuntime.processed,
        total: taskRuntime.total,
      });
    }
  }
  const cliCurrentAccountId = codebuddyCli?.activeAccountId;
  const workbuddyCurrentName = current
    ? current.nickname || current.email || current.uid || "未知账号"
    : "未登录";
  const codebuddyCurrentName = codebuddyCli?.configured
    ? codebuddyCli.activeAccountName || "未检测到"
    : "尚未接入";
  const cnIdeCurrentAccountId = codebuddyCnIde?.activeAccountId;
  const cnIdeCurrentName = codebuddyCnIde?.installed
    ? codebuddyCnIde.activeAccountName || "未检测到"
    : "未安装";
  return (
    // 流式布局：内容随窗口铺满（减去侧栏），只保留内边距。
    //
    // 留白的演进（按 2340px 屏、侧栏 220px 计算，每侧留白）：
    //   max-w-3xl (768px)  → 676px
    //   max-w-[1180px]     → 470px
    //   max-w-[1800px]     → 160px   ← 用户反馈"还是留白太多"
    //   去除上限（本版）    → 0（仅 px-8 内边距）
    // 结论：只要保留居中定宽，宽屏上就一定有留白；本页内容（账号卡片、
    // 状态块、操作按钮）都是横向铺开的行式布局，能自然拉伸，故完全放开。
    // 兼容网关页同步做了相同处理，两页留白表现保持一致。
    <div className="w-full space-y-6 px-5 py-6 sm:px-8 sm:py-8">
      <header className="mb-6">
        <div className="flex items-start justify-between gap-4">
          <div className="min-w-0">
            <h1 className="text-[28px] font-semibold tracking-tight">WorkBuddy 账号</h1>
            <p className="mt-2 text-sm leading-6 text-muted-foreground">
              统一管理 WorkBuddy、CodeBuddy IDE 与 CodeBuddy CLI 账号、积分和签到状态。
            </p>
          </div>
          <div className="flex shrink-0 items-center gap-4 pt-1">
            {/* WorkBuddy 专属配置入口（2026-09-22 从设置页迁来）。
                收进弹窗而不是平铺：那四张卡片一千三百多行，
                平铺会把下面的账号列表挤到很远处。 */}
            <PlatformConfigButton onClick={() => setWorkbuddyConfigOpen(true)}>
              配置
            </PlatformConfigButton>
            <div className="flex items-center gap-2.5">
              <span className="group relative inline-flex cursor-default">
                <span
                  className={
                    status?.running
                      ? "inline-flex rounded-[22%] bg-primary p-[2px] shadow-sm shadow-primary/40"
                      : "inline-flex rounded-[22%] bg-muted-foreground/30 p-[2px]"
                  }
                >
                  <WorkBuddyMark size={28} />
                </span>
                <span className="pointer-events-none absolute right-0 top-full z-50 mt-2 hidden whitespace-nowrap rounded-md bg-popover px-2.5 py-1.5 text-xs text-popover-foreground shadow-lg ring-1 ring-black/5 group-hover:block">
                  WorkBuddy：{status?.running ? "运行中" : "未运行"} · 当前账号：{workbuddyCurrentName}
                </span>
              </span>
              <span className="group relative inline-flex cursor-default">
                <span
                  className={
                    codebuddyCnIde?.installed
                      ? "inline-flex rounded-[22%] bg-primary p-[2px] shadow-sm shadow-primary/40"
                      : "inline-flex rounded-[22%] bg-muted-foreground/30 p-[2px]"
                  }
                >
                  <CodeBuddyCnIdeMark size={28} />
                </span>
                <span className="pointer-events-none absolute right-0 top-full z-50 mt-2 hidden whitespace-nowrap rounded-md bg-popover px-2.5 py-1.5 text-xs text-popover-foreground shadow-lg ring-1 ring-black/5 group-hover:block">
                  CodeBuddy IDE：{codebuddyCnIde?.installed ? (codebuddyCnIde.running ? "运行中" : "已接入") : "未接入"} · 当前账号：{cnIdeCurrentName}
                </span>
              </span>
              <span className="group relative inline-flex cursor-default">
                <span
                  className={
                    codebuddyCli?.configured
                      ? "inline-flex rounded-[22%] bg-primary p-[2px] shadow-sm shadow-primary/40"
                      : "inline-flex rounded-[22%] bg-muted-foreground/30 p-[2px]"
                  }
                >
                  <CodeBuddyMark size={28} />
                </span>
                <span className="pointer-events-none absolute right-0 top-full z-50 mt-2 hidden whitespace-nowrap rounded-md bg-popover px-2.5 py-1.5 text-xs text-popover-foreground shadow-lg ring-1 ring-black/5 group-hover:block">
                  CodeBuddy CLI：{codebuddyCli?.migrationRequired ? "需升级" : codebuddyCli?.configured ? "已接入" : "未接入"} · 当前账号：{codebuddyCurrentName}
                </span>
              </span>
            </div>
          </div>
        </div>
      </header>

      <div className="relative mb-6 overflow-visible rounded-2xl border border-border bg-muted/30 px-5 py-5 shadow-[0_6px_20px_rgba(15,23,42,.025)]">
        <div className="pointer-events-none absolute inset-0 overflow-hidden rounded-2xl">
          <div className="absolute -right-12 -top-20 size-44 rounded-full border-[28px] border-slate-400/[0.035]" />
        </div>
        <div className="relative flex flex-wrap items-center gap-x-5 gap-y-4">
          <div className="min-w-[190px] flex-1">
            <h2 className="text-sm font-semibold text-foreground">添加与迁移账号</h2>
            <p className="mt-1 text-xs leading-5 text-muted-foreground">快速接入新账号，或从已有环境恢复</p>
          </div>
          <div className="flex flex-wrap items-center gap-2.5">
            <DemoAction>
              <Button
                className="h-10 bg-primary px-4 text-primary-foreground shadow-sm hover:bg-primary/90"
                onClick={() => setOauthOpen(true)}
              >
                <QrCode />OAuth 登录添加
              </Button>
            </DemoAction>
            {/* 手机号 + 短信验证码：**唯一不依赖桌面端**的添加方式。
                放在 OAuth 右边而不是藏进菜单 —— 没有桌面客户端的用户
                第一眼就该看到它（服务器/远程场景尤其需要）。 */}
            <DemoAction>
              <Button
                className="h-10 px-4"
                variant="outline"
                onClick={() => setSmsOpen(true)}
                title="用手机号 + 短信验证码添加账号，无需安装桌面客户端"
              >
                <MessageSquare />手机号登录
              </Button>
            </DemoAction>
            <DemoAction>
              <Button
                className="h-10 px-4"
                onClick={() => setImportLocalOpen(true)}
                variant="outline"
                title="扫描本机当前登录态、历史登录快照与切换备份，可一次导入多个账号"
              >
                <Download />从本机导入
              </Button>
            </DemoAction>
          </div>
          <div className="flex items-center gap-1">
            <DemoAction>
              <Button variant="ghost" size="sm" className="h-9 px-2.5" onClick={() => setImportOpen(true)} title="从备份文件导入账号">
                <FileUp />导入备份
              </Button>
            </DemoAction>
            <DemoAction>
              <Button variant="ghost" size="sm" className="h-9 px-2.5" onClick={() => setExportOpen(true)} disabled={accounts.length === 0} title="导出账号备份">
                <FileDown />导出
              </Button>
            </DemoAction>
          </div>
        </div>
      </div>

      {error && (
        <Alert variant="destructive" className="mb-4">
          <AlertTitle>加载失败</AlertTitle>
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      {codebuddyCli &&
        (!codebuddyCli.configured ||
          codebuddyCli.migrationRequired ||
          codebuddyCli.syncPending) && (
        <Alert className="mb-4">
          <Terminal />
          <AlertTitle>CodeBuddy CLI 接入</AlertTitle>
          <AlertDescription>
            <p>
              {codebuddyCli.environmentOverride
                ? "检测到进程环境变量 CODEBUDDY_AUTH_TOKEN。它会覆盖 settings.json；请先从 Windows 用户或系统环境变量中删除它，再重启本应用与 CodeBuddy CLI。"
                : codebuddyCli.syncPending
                  ? "CLI 认证配置与当前账号 Token 已脱节。点击更新认证后写入最新 Token；当前运行会话不会切换，请由 ACP 重新加载会话或重启 CLI 后生效。"
                  : codebuddyCli.migrationRequired
                    ? "检测到旧版 helper 配置。接入后会改用 settings.json 的 env.CODEBUDDY_AUTH_TOKEN，不再执行 helper。"
                    : "使用 CodeBuddy settings.json 中的认证 Token；切换或保活刷新后会自动更新。当前运行会话不会切换，请由 ACP 重新加载会话或重启 CLI 后生效。"}
            </p>
            <DemoAction>
              <Button
                className="mt-2"
                size="sm"
                variant="outline"
                onClick={() => void onInstallCodebuddyCli()}
                disabled={installingCodebuddyCli}
              >
                {installingCodebuddyCli && <Loader2 className="animate-spin" />}
                {codebuddyCli.configured ? "更新 CLI 认证" : "接入 CLI"}
              </Button>
            </DemoAction>
          </AlertDescription>
        </Alert>
      )}
      <section className="mt-7 min-w-0" aria-labelledby="accounts-list-title">
        <div className="mb-4 flex flex-wrap items-center justify-between gap-3">
          <div className="flex items-center gap-2">
            <h2 id="accounts-list-title" className="text-base font-semibold tracking-tight">账号</h2>
            <Badge
              variant="secondary"
              className="h-6 min-w-6 rounded-full border-0 px-1.5 text-[11px] tabular-nums text-muted-foreground shadow-none"
              aria-label={`${accounts.length} 个账号`}
            >
              {accounts.length}
            </Badge>
          </div>
          <TooltipProvider delayDuration={400}>
            <div className="ml-auto flex flex-wrap items-center justify-end gap-1">
              <div className="mr-1 flex items-center gap-2.5">
                <label htmlFor="accounts-auto-checkin" className="cursor-pointer text-xs font-medium text-muted-foreground">
                  自动签到
                </label>
                <DemoAction>
                  <Switch
                    id="accounts-auto-checkin"
                    checked={autoCheckinConfig?.enabled ?? false}
                    disabled={!autoCheckinConfig || autoCheckinSaving}
                    onCheckedChange={(enabled) => void onAutoCheckinChange(enabled)}
                    aria-label="自动签到"
                  />
                </DemoAction>
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Badge
                      variant="secondary"
                      className="h-7 cursor-default rounded-md border-0 px-2 text-[11px] font-normal text-muted-foreground shadow-none"
                      aria-label="自动签到仅覆盖国服账号"
                    >
                      仅国服
                    </Badge>
                  </TooltipTrigger>
                  <TooltipContent side="top">自动签到为国服专属，国际版接口暂无数据</TooltipContent>
                </Tooltip>
                {autoCheckinSaving && <Loader2 className="size-3.5 animate-spin text-muted-foreground" aria-label="正在保存自动签到设置" />}
              </div>
              <div className="mr-1 flex items-center gap-2.5">
                <label htmlFor="accounts-auto-travel" className="cursor-pointer text-xs font-medium text-muted-foreground">
                  自动旅行
                </label>
                <DemoAction>
                  <Switch
                    id="accounts-auto-travel"
                    checked={autoTravelConfig?.enabled ?? false}
                    disabled={!autoTravelConfig || autoTravelSaving}
                    onCheckedChange={(enabled) => void onAutoTravelChange(enabled)}
                    aria-label="自动旅行"
                  />
                </DemoAction>
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Badge
                      variant="secondary"
                      className="h-7 cursor-default rounded-md border-0 px-2 text-[11px] font-normal text-muted-foreground shadow-none"
                      aria-label="自动旅行仅覆盖国服账号"
                    >
                      仅国服
                    </Badge>
                  </TooltipTrigger>
                  <TooltipContent side="top">自动旅行为国服专属，国际版暂无 Buddy 数据</TooltipContent>
                </Tooltip>
                {autoTravelSaving && <Loader2 className="size-3.5 animate-spin text-muted-foreground" aria-label="正在保存自动旅行设置" />}
              </div>
              <Separator orientation="vertical" className="mx-2 h-5" />
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="ghost"
                    size="icon"
                    className={cn("size-9 rounded-lg", compact && "bg-accent text-accent-foreground")}
                    onClick={toggleCompact}
                    aria-label={compact ? "切换为宽松模式" : "切换为紧凑模式"}
                  >
                    {compact ? <Rows3 /> : <Columns3 />}
                  </Button>
                </TooltipTrigger>
                <TooltipContent side="top">{compact ? "切换为宽松模式" : "切换为紧凑模式"}</TooltipContent>
              </Tooltip>
              <Tooltip>
                <TooltipTrigger asChild>
                  <span>
                    <DemoAction>
                      <Button
                        variant="ghost"
                        size="icon"
                        className="size-9 rounded-lg"
                        disabled={refreshingCredits || checkinAllRunning || accounts.length === 0}
                        onClick={() => void onRefreshCredits()}
                        aria-label="签到并刷新全部账号积分"
                      >
                        <RefreshCw className={refreshingCredits || checkinAllRunning ? "animate-spin" : undefined} />
                      </Button>
                    </DemoAction>
                  </span>
                </TooltipTrigger>
                <TooltipContent side="top">{api.isDemoMode() ? "演示模式下不可操作" : "签到并刷新全部账号积分"}</TooltipContent>
              </Tooltip>
              {/* 跨账号任务队列：把「逐个账号点成长任务」变成一次批量执行。
                  独立成按钮而不是塞进「一键操作」下拉 —— 多账号运维时
                  「逐个点」是主要时间黑洞，埋在二级菜单里等于没做这个功能。
                  与上面那个刷新按钮同构（Tooltip > span > DemoAction > Button），
                  沿用既有写法，不引入新的 asChild 组合。 */}
              <Tooltip>
                <TooltipTrigger asChild>
                  <span>
                    <DemoAction>
                      <Button
                        variant="outline"
                        size="sm"
                        className="h-9 gap-1.5 rounded-lg px-2.5"
                        disabled={accounts.length === 0}
                        onClick={() => setTaskQueueOpen(true)}
                        aria-label="跨账号任务队列：扫描并批量执行成长任务"
                      >
                        <ListChecks />
                        扫描待办
                      </Button>
                    </DemoAction>
                  </span>
                </TooltipTrigger>
                <TooltipContent side="top">
                  {api.isDemoMode()
                    ? "演示模式下不可操作"
                    : "扫描各账号可自动完成的成长任务，按并发档位批量执行并实时查看进度"}
                </TooltipContent>
              </Tooltip>
              {/* 一键操作：签到与旅行合并成一个下拉，避免工具栏继续横向膨胀。
                  这里刻意**不**套 Tooltip —— 双层 asChild（TooltipTrigger + DropdownMenuTrigger）
                  会在 ref 与事件处理上互相覆盖，属于已知的脆弱组合；按钮本身已有
                  可见文案与 aria-label，不再需要 tooltip。 */}
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <span className="inline-flex">
                    <DemoAction>
                      <Button
                        variant="outline"
                        size="sm"
                        className="h-9 gap-1.5 rounded-lg px-2.5"
                        disabled={accounts.length === 0 || checkinAllRunning || travelRunning}
                        aria-label="一键操作：批量签到或旅行"
                      >
                        {checkinAllRunning || travelRunning ? (
                          <Loader2 className="animate-spin" />
                        ) : (
                          <Sparkles />
                        )}
                        一键操作
                        <ChevronDown className="size-3.5 opacity-60" />
                      </Button>
                    </DemoAction>
                  </span>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end" className="w-56">
                  <DropdownMenuItem
                    disabled={checkinAllRunning || travelRunning}
                    onSelect={() => void onRefreshCredits()}
                  >
                    <RefreshCw />
                    一键签到
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    disabled={checkinAllRunning || travelRunning}
                    onSelect={() => void onTravelRun()}
                  >
                    <Sparkles />
                    一键旅行（含领养）
                  </DropdownMenuItem>
                  {/* 一键养号：作用于**全部账号**（所有者要求 —— 单个账号的入口
                      在账号卡片菜单里，全账号的入口就该在这一排工具栏上）。
                      逐项列出而不是只给一个「全部执行」：
                      用户常常只想补跑某一项（如今天夜猫子漏了），
                      全跑一遍会在多个账号上产生不必要的上游请求。 */}
                  <DropdownMenuSeparator />
                  <DropdownMenuLabel>一键养号（全部账号）</DropdownMenuLabel>
                  <DropdownMenuItem
                    disabled={checkinAllRunning || travelRunning || taskRunning !== undefined}
                    onSelect={() => void onRunAllTasks("activity")}
                  >
                    <Zap />
                    活跃上报
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    disabled={checkinAllRunning || travelRunning || taskRunning !== undefined}
                    onSelect={() => void onRunAllTasks("nightowl")}
                  >
                    <Moon />
                    夜猫子任务
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    disabled={checkinAllRunning || travelRunning || taskRunning !== undefined}
                    onSelect={() => void onRunAllTasks("school")}
                  >
                    <GraduationCap />
                    开学季活动
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    disabled={checkinAllRunning || travelRunning || taskRunning !== undefined}
                    onSelect={() => void onRunAllTasks("trial")}
                  >
                    <Gift />
                    trial 加油包
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    disabled={checkinAllRunning || travelRunning || taskRunning !== undefined}
                    onSelect={() => void onRunAllTasks("growthmap")}
                  >
                    <MapPin />
                    活跃地图
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>
            </div>
          </TooltipProvider>
        </div>
        {loading && accounts.length === 0 ? (
          <div className="flex items-center gap-2 py-16 text-sm text-muted-foreground">
            <Loader2 className="animate-spin" />
            加载账号…
          </div>
        ) : accounts.length === 0 ? (
          <div className="rounded-xl border border-dashed px-4 py-16 text-center text-sm text-muted-foreground">
            暂无账号。点击上方按钮导入本机账号或 OAuth 登录。
          </div>
        ) : (
          /* 卡片网格：auto-rows-fr 让同一排的卡片等高。
             此前用 items-start，每张卡片按自身内容高度渲染，于是「只有 1 个积分包」
             的账号会比同排「有 2 个积分包」的账号矮一截（实测 171px vs 208px）。
             等高后资源包列表的差异只体现为卡片内留白，不再破坏整排对齐。
             注意不要保留 items-start：它会让卡片不拉伸，auto-rows-fr 就失效了。 */
          <div className={cn("grid auto-rows-fr min-w-0 gap-5", compact ? "grid-cols-[repeat(auto-fit,minmax(min(100%,300px),1fr))]" : "grid-cols-[repeat(auto-fit,minmax(min(100%,340px),1fr))]")}>
            {orderedAccounts.map((a) => (
              <AccountCard
                key={a.id}
                account={a}
                compact={compact}
                onDelete={onDelete}
                // 备注改完后重新拉列表：备注存在账号库里，卡片本身不持有列表状态，
                // 不刷新的话关闭弹窗后卡片上仍是旧备注。
                onNoteSaved={() => void fetchAll()}
                // 禁用状态存在账号库里，且后端会重导出凭证并按需重启网关，
                // 因此这里刷新账号列表让界面立刻反映新状态。
                onToggleDisabled={async (target) => {
                  const next = !target.disabled;
                  const label = target.nickname || target.uid || "该账号";
                  try {
                    await api.setAccountDisabled(target.id, next);
                    toast.success(
                      next
                        ? `已禁用「${label}」：不再进入账号池，签到等养号任务继续运行`
                        : `已启用「${label}」：已重新加入账号池`,
                    );
                  } catch (e) {
                    toast.error(api.asError(e));
                  }
                  // 无论成功失败都刷新：失败时界面回到后端真实状态
                  await fetchAll();
                }}
                onSwitch={setSwitchAccount}
                onCheckin={onCheckin}
                onRefresh={onRefresh}
                onAdopt={onAdopt}
                onRunTask={onRunTask}
                taskRunning={taskRunning}
                onRunGrowthTask={onRunGrowthTask}
                growthTaskRunning={growthTaskRunning}
                onRunAllGrowthTasks={onRunAllGrowthTasks}
                growthAllRunningAccountId={growthAllAccountId}
                runningTask={runningTaskByAccountId.get(a.id) ?? null}
                todayCheckedIn={checkinMap[a.id]}
                travelStatus={travelMap[a.id]}
                credit={creditMap[a.id]}
                creditLoading={creditLoadingMap[a.id]}
                creditUpdatedAt={creditUpdatedAtMap[a.id]}
                creditPriority={a.id === priorityAccountId}
                workbuddyActive={isWorkbuddyCurrent(a, current)}
                codebuddyCliConfigured={codebuddyCli?.configured && !codebuddyCli.migrationRequired && !codebuddyCli.syncPending}
                codebuddyCliActive={a.id === cliCurrentAccountId}
                codebuddyCliBusy={codebuddyCliSwitchingId !== null}
                onSwitchCodebuddyCli={onSwitchCodebuddyCli}
                codebuddyCliLoading={codebuddyCliSwitchingId === a.id}
                codebuddyCnIdeAvailable={Boolean(codebuddyCnIde?.installed)}
                codebuddyCnIdeActive={a.id === cnIdeCurrentAccountId}
                codebuddyCnIdeBusy={codebuddyCnIdeSwitchingId !== null}
                codebuddyCnIdeLoading={codebuddyCnIdeSwitchingId === a.id}
                onSwitchCodebuddyCnIde={onSwitchCodebuddyCnIde}
                featuresDisabled={false}
              />
            ))}
          </div>
        )}
      </section>

      {/* ---- WorkBuddy 平台配置（2026-09-22 从设置页迁来，改为弹窗）----
          所有者要求：
            · 「设置页面的每个平台的配置,迁移到每个平台自己的页面去,
               不要留在设置页面」
            · 「这些配置应该单独做到一个按钮上,配置  然后点击弹窗进行配置」

          这四张卡片**只对 WorkBuddy 生效**（签到、养号任务、CodeBuddy CLI
          轮换、认证目录权限检测）。平铺的话一千三百多行会把账号列表淹掉，
          故收进弹窗 —— 日常看账号、偶尔改配置。
          组件体一字未改，见 `components/workbuddy-settings.tsx` 的说明。 */}
      <PlatformConfigDialog
        open={workbuddyConfigOpen}
        onOpenChange={setWorkbuddyConfigOpen}
        title="WorkBuddy 配置"
        description="自动签到、养号任务、CodeBuddy CLI 轮换与认证目录权限。这些配置只对 WorkBuddy 生效。"
      >
        <div className="min-w-0 space-y-12">
          <AutoCheckinCard />
          <AutoCareTasksCard />
          <AutoRotateCard />
          <PermissionCheckCard />
        </div>
      </PlatformConfigDialog>

      <OAuthLoginDialog open={oauthOpen} onOpenChange={setOauthOpen} />
      {/* 手机号 + 短信验证码登录。成功后刷新列表 —— 新账号要立刻出现，
          否则用户会以为没加上而重复操作（那会再发一条短信）。 */}
      <SmsLoginDialog
        open={smsOpen}
        onOpenChange={setSmsOpen}
        onSuccess={(msg) => {
          toast.success(msg, { description: "账号已加入列表" });
          void fetchAll();
        }}
      />
      {/* 跨账号任务队列面板。onFinished 里刷新账号列表与积分：
          成长任务会写账号记录并可能带来领奖积分，不刷新的话卡片上还是旧值。 */}
      <TaskQueuePanel
        open={taskQueueOpen}
        onOpenChange={setTaskQueueOpen}
        accounts={accounts}
        onFinished={() => {
          void fetchAll();
          void refreshCredits(accounts.map((account) => account.id));
        }}
      />
      <ExportAccountsDialog
        open={exportOpen}
        onOpenChange={setExportOpen}
        accounts={accounts}
        onExported={onExported}
      />
      <ImportAccountsDialog
        open={importOpen}
        onOpenChange={setImportOpen}
        onImported={onImported}
      />
      <ImportLocalDialog
        open={importLocalOpen}
        onOpenChange={setImportLocalOpen}
        onImported={onLocalImported}
      />
      <SwitchAccountDialog
        open={switchAccount !== null}
        onOpenChange={(o) => {
          if (!o) setSwitchAccount(null);
        }}
        account={switchAccount}
        onDone={() => {
          void fetchAll();
          void refreshCodebuddyCliStatus();
          void refreshCodebuddyCnIdeStatus();
        }}
      />

      {/* 接入/更新 CLI 认证确认（桌面 App 不支持 window.confirm） */}
      <Dialog open={installConfirmOpen} onOpenChange={setInstallConfirmOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              {codebuddyCli?.configured ? "更新 CodeBuddy CLI 认证" : "接入 CodeBuddy CLI"}
            </DialogTitle>
            <DialogDescription>
              <>
                将把当前账号的认证 Token 写入
                <code className="mx-1 rounded bg-muted px-1">~/.codebuddy/settings.json</code>
                的 <code className="mx-1 rounded bg-muted px-1">env.CODEBUDDY_AUTH_TOKEN</code>。
                其他配置会保留；更新只影响后续加载的会话，当前运行会话不会切换。是否继续？
              </>
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setInstallConfirmOpen(false)}>
              取消
            </Button>
            <Button onClick={() => void confirmInstallCodebuddyCli()}>继续</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* 删除账号确认 */}
      <Dialog open={deleteTarget !== null} onOpenChange={(o) => !o && setDeleteTarget(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>删除账号</DialogTitle>
            <DialogDescription>
              确定删除账号「{deleteTarget?.nickname || deleteTarget?.email || deleteTarget?.id}」？
              此操作不可撤销。
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setDeleteTarget(null)}>
              取消
            </Button>
            <Button variant="destructive" onClick={() => void confirmDelete()}>
              删除
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
