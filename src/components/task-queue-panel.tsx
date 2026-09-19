import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Loader2, Play, RefreshCw, ScanSearch, Square } from "lucide-react";
import { toast } from "sonner";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import * as api from "@/lib/api";
import type { AccountMeta } from "@/lib/types";
import { cn } from "@/lib/utils";

/**
 * 跨账号任务队列面板（成长任务「一键完成」）。
 *
 * 解决的真实问题：此前只有**逐账号手动**执行（账号卡片菜单里的「校园日」），
 * 19 个 WorkBuddy 账号要一个个点，这是多账号运维的主要时间黑洞。
 * 本面板把「扫描待办 → 按并发档位批量执行 → 实时看进度」做成一条流水线。
 *
 * ---------------------------------------------------------------------------
 * 为什么是**前端编排**而不是调后端的批量接口
 * ---------------------------------------------------------------------------
 * 后端确实有一条批量路径（`/tasks/growth` 的 `action=run-all`），但它**不能**
 * 满足本面板的两个硬需求：
 *
 *   1. 并发档位不可配。`run-all` 的并发在
 *      `go-gateway/cmd/server/growth_tasks.go` 里**硬编码为 2**，
 *      而 HTTP 请求体只解析 `action` / `accountId` / `taskCode`
 *      （见 `internal/server/handler.go` 的 `growthTasks`），
 *      没有任何字段能把并发度传下去。界面上放一个不生效的下拉是欺骗。
 *   2. 作用范围不可选。`run-all` 只认网关**账号池**（且跳过禁用账号），
 *      返回结果按 uid 索引；而界面持有的是账号库 id 与用户当前看到的这批账号。
 *
 * 所以这里用既有的**单账号**接口（`action=run` + `taskCode`）在前端串行编排：
 * 每完成一项就写一次真实状态，进度条因此来自**真实完成数**，不是假动画。
 *
 * ---------------------------------------------------------------------------
 * 并发模型：账号间并发、账号内串行
 * ---------------------------------------------------------------------------
 * 这不是随手选的，是后端强制的：`growthTaskAdapter.runOne` 先抢**账号级互斥锁**
 * （`a.runner.Lock(acc.UID)`），抢不到就直接报「该账号的任务正在执行中，请稍候」。
 * 若把同一账号的两个任务并发发出去，第二个必定失败。
 *
 * 这与 Go 侧自己的批量实现口径完全一致（`growtask.FleetOptions` 的注释：
 * 「账号**内**始终串行」）。因此调度器按**账号分组**发放令牌：
 * 同时在跑的账号数 ≤ 并发档位，每个账号内部逐个任务串行执行。
 * 于是任意时刻在途的 HTTP 请求数 ≤ 并发档位，且同一账号永不重叠。
 *
 * ---------------------------------------------------------------------------
 * 为什么按「账号×任务」拆项，而不是每账号一次 `run`（不传 taskCode）
 * ---------------------------------------------------------------------------
 * 不传 taskCode 时后端会跑完该账号的全部待办（内部含报名、回读进度、自动领奖），
 * 请求数更少。但它的 `items` **要等整个账号跑完才一次性返回** —— 一个账号十几项、
 * 每项可能含真实对话，进度条会连续几分钟纹丝不动，恰恰违背本面板的目的。
 * 拆成逐项后，每项结束都能立刻反映到进度条与行状态上，代价是每项会多一次
 * 任务列表拉取（`RunOne` 内部会重新 list 一次）。
 */

// ---------------------------------------------------------------------------
// 局部类型
//
// 从既有 api 函数的返回类型**推导**，而不是 import `@/lib/types` 里的具名类型：
// 既不改动那个文件，也不会出现「两份定义各自漂移」。
// ---------------------------------------------------------------------------

type GrowthTaskEnvelope = Awaited<ReturnType<typeof api.runGrowthTask>>;
/** action=list 返回的单项任务视图。 */
type GrowthTaskView = NonNullable<GrowthTaskEnvelope["tasks"]>[number];
/** action=run 返回的单项执行结果。 */
type GrowthTaskItemResult = NonNullable<GrowthTaskEnvelope["items"]>[number];

/** 队列项状态（照参考实现的六态语义）。 */
type QueueStatus = "pending" | "queued" | "running" | "done" | "failed" | "skipped";

/** 队列项：一个「账号 × 任务」的执行单元。 */
interface QueueItem {
  /** 稳定键：accountId::taskCode。 */
  key: string;
  accountId: string;
  /** 账号展示名，扫描时快照下来（账号列表刷新后仍能显示是哪个号）。 */
  accountName: string;
  taskCode: string;
  /** 任务名：扫描时取上游 title，运行时用返回的 title 覆盖。 */
  taskName: string;
  /** 上游进度文案（如 "3/5"）。**仅用于展示**，不参与完成判定。 */
  progressText?: string;
  status: QueueStatus;
  message?: string;
}

/** 扫描失败的账号（拉不到任务列表时无法生成队列项，单独列出）。 */
interface ScanFailure {
  accountId: string;
  accountName: string;
  message: string;
}

/**
 * 扫描单个账号的结果。
 *
 * 写成**显式判别联合**（`ok` 作判别式）而不是让 TS 从回调返回值推断：
 * 后者会得到一个「字段都可选」的合成类型，`"error" in result` 收窄后
 * `result.error` 仍是 `string | undefined`（实测报 TS2322）。
 * 判别式联合则能可靠收窄，且这里的成功/失败本来就该显式区分。
 */
type ScanOutcome =
  | { account: AccountMeta; ok: true; tasks: GrowthTaskView[] }
  | { account: AccountMeta; ok: false; error: string };

/** 按账号分组后的渲染单元。 */
interface AccountGroup {
  accountId: string;
  accountName: string;
  items: QueueItem[];
}

/** 并发档位（与 Go 侧 `maxFleetConcurrency` 同档，上限 3）。 */
const CONCURRENCY_OPTIONS = [1, 2, 3] as const;
/** 默认并发 2：与 Go 侧批量执行自己选的值一致（并发过高会在上游形成异常流量画像）。 */
const DEFAULT_CONCURRENCY = 2;

const STATUS_TEXT: Record<QueueStatus, string> = {
  pending: "待执行",
  queued: "排队",
  running: "执行中",
  done: "完成",
  failed: "失败",
  skipped: "跳过",
};

/** 状态点颜色：灰=未开始，蓝=执行中，绿=完成，红=失败。 */
const STATUS_DOT: Record<QueueStatus, string> = {
  pending: "bg-muted-foreground/40",
  queued: "bg-muted-foreground/40",
  running: "bg-sky-500",
  done: "bg-emerald-500",
  failed: "bg-destructive",
  skipped: "bg-muted-foreground/40",
};

/** 状态徽章配色（沿用既有的 sky / emerald / destructive token 用法）。 */
const STATUS_CHIP: Record<QueueStatus, string> = {
  pending: "border-transparent bg-secondary text-muted-foreground",
  queued: "border-transparent bg-secondary text-muted-foreground",
  running: "border-sky-500/30 bg-sky-500/10 text-sky-700 dark:text-sky-300",
  done: "border-emerald-500/30 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300",
  failed: "border-destructive/30 bg-destructive/10 text-destructive",
  skipped: "border-transparent bg-secondary text-muted-foreground",
};

const TERMINAL_STATUSES: QueueStatus[] = ["done", "failed", "skipped"];

function isTerminal(status: QueueStatus): boolean {
  return TERMINAL_STATUSES.includes(status);
}

/**
 * 账号展示名：备注 → 昵称 → 邮箱 → uid 前 8 位。
 *
 * 与账号池口径（GatewayPage 的 `accountLabel`）保持一致 —— 成长任务跑在网关
 * 账号池里，两处叫法不同会让用户对不上是哪个号。
 */
function accountLabel(account: AccountMeta): string {
  return (
    account.note?.trim() ||
    account.nickname?.trim() ||
    account.email?.trim() ||
    account.uid?.slice(0, 8) ||
    account.id
  );
}

/**
 * 该账号是否参与成长任务。
 *
 * 排除国际版：成长任务的判据是国服 growth 域（连登天数、小程序指纹），
 * 国际版是另一套任务集（无奖励字段、无 progress），跑了等于空转 ——
 * 这与 `account-card.tsx` 里 `schoolSeasonAvailability` 的置灰理由一致。
 *
 * 排除已禁用：禁用语义是「不进网关账号池」，而成长任务必须先在池里查到账号
 * （`growthTaskAdapter.lookup` → `pool.AuthByUID`）。发出去只会得到
 * 「账号不在网关池中」，不如直接标注原因。
 */
function isQueueEligible(account: AccountMeta): boolean {
  return account.regionKey !== "intl" && !account.disabled;
}

/**
 * 有界并发地跑一批任务，返回顺序与输入一致。
 *
 * 这是「并发档位」的落地点：最多 limit 个任务同时在途，
 * 而不是 `Promise.all` 把全部请求一次打出去。
 */
async function mapWithConcurrency<T, R>(
  items: T[],
  limit: number,
  fn: (item: T) => Promise<R>,
): Promise<R[]> {
  const results = new Array<R>(items.length);
  if (items.length === 0) return results;
  const width = Math.max(1, Math.min(limit, items.length));
  let cursor = 0;
  const worker = async () => {
    for (;;) {
      // cursor++ 是同步操作：JS 单线程下不会有两个 worker 拿到同一个下标。
      const index = cursor++;
      if (index >= items.length) return;
      results[index] = await fn(items[index]);
    }
  };
  await Promise.all(Array.from({ length: width }, () => worker()));
  return results;
}

/**
 * 把单项执行结果整理成一行中文说明。
 *
 * 特意把**进度前后对比**与**领奖结果**带上：后端 `ItemResult` 的注释写明
 * 「上报 200 ≠ 计分」，只有 `progress_after` 达标才算真的完成。
 * 只报「上报成功」会让用户把空转当成完成。
 */
function describeItem(item: GrowthTaskItemResult): string {
  const parts: string[] = [];
  if (item.message) parts.push(item.message);
  if (item.progress_before || item.progress_after) {
    parts.push(`进度 ${item.progress_before || "—"} → ${item.progress_after || "—"}`);
  }
  if (item.claimed) {
    const reward: string[] = [];
    if (item.credit) reward.push(`${item.credit} 积分`);
    if (item.energy) reward.push(`${item.energy} 能量`);
    parts.push(reward.length > 0 ? `已领奖：${reward.join(" / ")}` : "已领奖");
  }
  if (item.claim_error) parts.push(`领奖失败：${item.claim_error}`);
  return parts.join("；");
}

/**
 * 把队列按账号分组，保持账号首次出现的顺序（与扫描顺序一致）。
 *
 * 抽成纯函数是为了能直接测：分组与顺序错了，界面会把任务挂到别的账号名下，
 * 而这种错误在手工点几下时不一定看得出来。
 */
function groupByAccount(queue: QueueItem[]): AccountGroup[] {
  const out: AccountGroup[] = [];
  const index = new Map<string, AccountGroup>();
  for (const item of queue) {
    let group = index.get(item.accountId);
    if (!group) {
      group = { accountId: item.accountId, accountName: item.accountName, items: [] };
      index.set(item.accountId, group);
      out.push(group);
    }
    group.items.push(item);
  }
  return out;
}

/**
 * 组头文案。
 *
 * 未开始时报「N 项待办」（照参考实现的口径），开始后改报「已结束 x/y」——
 * 跑起来之后「还剩多少没跑完」比「一共多少项」更该被看见。
 */
function groupSummary(group: AccountGroup): string {
  const settled = group.items.filter((item) => isTerminal(item.status)).length;
  if (settled === 0) return `${group.items.length} 项待办`;
  const done = group.items.filter((item) => item.status === "done").length;
  return `已结束 ${settled}/${group.items.length}${done > 0 ? ` · 完成 ${done}` : ""}`;
}

interface TaskQueuePanelProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** 账号库当前账号（面板据此决定扫描谁）。 */
  accounts: AccountMeta[];
  /** 整批结束后回调（用于刷新账号列表与积分）。 */
  onFinished?: () => void;
}

export function TaskQueuePanel({ open, onOpenChange, accounts, onFinished }: TaskQueuePanelProps) {
  const [concurrency, setConcurrency] = useState<number>(DEFAULT_CONCURRENCY);
  const [queue, setQueue] = useState<QueueItem[]>([]);
  const [scanFailures, setScanFailures] = useState<ScanFailure[]>([]);
  const [scanning, setScanning] = useState(false);
  const [running, setRunning] = useState(false);
  /** 本批是否已跑过（用于把进度文案从「待执行」切成「已结束」）。 */
  const [hasRun, setHasRun] = useState(false);

  /**
   * 卸载后不再 setState。
   *
   * React 18 起已移除「setState on unmounted component」的警告，但这里仍要拦：
   * 本面板会连着发几十个请求，卸载后继续写状态既是白费功夫，也可能让
   * 已经过期的结果覆盖新数据。**在 effect 里显式置回 true** 是为了兼容
   * StrictMode 的「挂载 → 卸载 → 再挂载」（同一实例重跑 effect，
   * 只在初始化时置 true 会被那次清理永久压成 false）。
   */
  const mountedRef = useRef(true);
  /** 用户点了「取消执行」或组件卸载；用于停止发放新的任务。 */
  const cancelledRef = useRef(false);
  /** 整批是否在跑（同步判据，避免读到过期的 running state）。 */
  const runningRef = useRef(false);
  /** 扫描是否在进行（防止重复扫描）。 */
  const scanningRef = useRef(false);
  /** 本次批次的分项计数（供结束时的 toast 汇总）。 */
  const statsRef = useRef({ done: 0, failed: 0, skipped: 0 });
  /** 本次打开是否已经自动扫描过。 */
  const autoScannedRef = useRef(false);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      // 卸载即停止派发后续任务：已经在途的请求会自然结束，但不再有新的发出去。
      cancelledRef.current = true;
    };
  }, []);

  /** 只更新一个队列项；卸载后直接丢弃。 */
  const applyItem = useCallback((key: string, patch: Partial<QueueItem>) => {
    if (!mountedRef.current) return;
    setQueue((prev) => prev.map((item) => (item.key === key ? { ...item, ...patch } : item)));
  }, []);

  const eligibleAccounts = useMemo(() => accounts.filter(isQueueEligible), [accounts]);
  /** 未参与扫描的账号数（用于在界面上说清楚「为什么少了几个号」）。 */
  const excluded = useMemo(() => {
    let intl = 0;
    let disabled = 0;
    for (const account of accounts) {
      if (account.regionKey === "intl") intl += 1;
      else if (account.disabled) disabled += 1;
    }
    return { intl, disabled };
  }, [accounts]);

  /**
   * 扫描待办：逐账号拉任务列表（**只读**），筛出可自动完成的未完成项。
   *
   * 筛选口径照抄后端 `growtask.PendingTasks`（`!Claimed && Automatable`）——
   * 界面自己再定一套「什么算待办」只会与后端的作用范围漂移。
   * 顺序也**不重排**：后端 `ListTasks` 已经把待办排前面（可领奖 → 进行中 →
   * 可报名 → 已领奖垫底），排序口径只该有一处。
   */
  const scan = useCallback(async () => {
    if (scanningRef.current || runningRef.current) return;
    scanningRef.current = true;
    // 卸载守卫必须**先于任何 setState**：下面这几行清空动作也在写状态，
    // 放在 await 之后才判 mounted 的话，卸载瞬间仍会写进去一次。
    if (mountedRef.current) {
      setScanning(true);
      setQueue([]);
      setScanFailures([]);
      setHasRun(false);
    }

    const targets = eligibleAccounts;
    const results = await mapWithConcurrency<AccountMeta, ScanOutcome>(
      targets,
      concurrency,
      async (account) => {
        try {
          const res = await api.runGrowthTask("list", account.id);
          if (!res.ok) {
            return { account, ok: false, error: res.error || "网关返回失败但未给出原因" };
          }
          return { account, ok: true, tasks: res.tasks ?? [] };
        } catch (e) {
          return { account, ok: false, error: api.asError(e) };
        }
      },
    );

    // 卸载后直接丢弃本次扫描结果，不再写状态。
    // 注意 scanningRef 的复位放在 finally 语义的位置（这里与上面那个
    // early-return 分支都显式复位），否则卸载后再挂载会卡在「扫描中」永不重扫。
    if (!mountedRef.current) {
      scanningRef.current = false;
      return;
    }

    const nextQueue: QueueItem[] = [];
    const nextFailures: ScanFailure[] = [];
    for (const result of results) {
      const name = accountLabel(result.account);
      if (!result.ok) {
        nextFailures.push({ accountId: result.account.id, accountName: name, message: result.error });
        continue;
      }
      for (const task of result.tasks) {
        if (!isPendingTask(task)) continue;
        nextQueue.push({
          key: `${result.account.id}::${task.task_code}`,
          accountId: result.account.id,
          accountName: name,
          taskCode: task.task_code,
          taskName: task.title?.trim() || task.task_code,
          progressText: task.progress || undefined,
          status: "pending",
        });
      }
    }

    setQueue(nextQueue);
    setScanFailures(nextFailures);
    setScanning(false);
    scanningRef.current = false;
  }, [concurrency, eligibleAccounts]);

  // 打开面板即扫描一次：入口按钮就叫「扫描待办」，再让用户点第二下是多余的。
  // 扫描是只读的（后端 ListTasks 明确「无任何写操作」），19 个账号一轮很快。
  // 用 ref 守卫「每次打开只扫一次」，否则并发档位或账号列表一变就会重复扫。
  useEffect(() => {
    if (!open) {
      autoScannedRef.current = false;
      return;
    }
    if (autoScannedRef.current || runningRef.current) return;
    autoScannedRef.current = true;
    void scan();
  }, [open, scan]);

  /** 结算一项：写状态 + 累计计数。 */
  const settle = useCallback(
    (key: string, status: "done" | "failed" | "skipped", message: string) => {
      statsRef.current[status] += 1;
      applyItem(key, { status, message });
    },
    [applyItem],
  );

  /** 执行一个队列项（单项失败只标红，不抛出，因此不会中断整批）。 */
  const runItem = useCallback(
    async (accountId: string, entry: { key: string; taskCode: string }) => {
      applyItem(entry.key, { status: "running", message: undefined });
      try {
        const res = await api.runGrowthTask("run", accountId, entry.taskCode);
        if (!res.ok) {
          settle(entry.key, "failed", res.error || "网关返回失败但未给出原因");
          return;
        }
        // action=run 带 taskCode 时后端返回 { item }；items 分支是防御性兜底。
        const item = res.item ?? res.items?.find((i) => i.task_code === entry.taskCode);
        if (!item) {
          settle(entry.key, "failed", "网关未返回该任务的结果");
          return;
        }
        const message = describeItem(item);
        if (item.status === "done") {
          settle(entry.key, "done", message || "已完成");
        } else if (item.status === "error") {
          settle(entry.key, "failed", message || "执行失败");
        } else {
          // skipped / unsupported 都归入「跳过」：都不是故障，而是「这个号现在
          // 不该跑这一项」（没这个任务、需客户端人工操作等）。
          settle(entry.key, "skipped", message || "已跳过");
        }
      } catch (e) {
        // 网络/宿主错误：标红继续，绝不向外抛 —— 抛出会让这一批提前结束。
        settle(entry.key, "failed", api.asError(e));
      }
    },
    [applyItem, settle],
  );

  /**
   * 执行全部待办。
   *
   * 调度：按账号分组发放令牌，同时在跑的**账号**数 ≤ 并发档位；
   * 账号内部逐项串行（后端有账号级互斥锁，并发同账号必定失败）。
   * 于是任意时刻在途请求数 ≤ 并发档位，且同一账号永不重叠。
   */
  const startRun = useCallback(async () => {
    if (runningRef.current) return;
    // 卸载守卫放在**任何 setState 之前**：下面紧跟的 setRunning / setQueue
    // 都是写状态。组件已卸载时这一批根本不该启动 —— 既没有界面可更新，
    // 也会让「卸载后不再 setState」这条约束名存实亡。
    if (!mountedRef.current) return;

    // 计划在启动瞬间从当前队列快照出来：跑的过程中不再读 state，
    // 避免闭包读到过期数据。
    const plan: Array<{ accountId: string; items: Array<{ key: string; taskCode: string }> }> = [];
    const grouped = new Map<string, { accountId: string; items: QueueItem[] }>();
    for (const item of queue) {
      const group = grouped.get(item.accountId);
      if (group) group.items.push(item);
      else grouped.set(item.accountId, { accountId: item.accountId, items: [item] });
    }
    for (const group of grouped.values()) {
      plan.push({
        accountId: group.accountId,
        items: group.items.map((item) => ({ key: item.key, taskCode: item.taskCode })),
      });
    }
    const total = plan.reduce((sum, group) => sum + group.items.length, 0);
    if (total === 0) return;

    runningRef.current = true;
    cancelledRef.current = false;
    statsRef.current = { done: 0, failed: 0, skipped: 0 };
    setRunning(true);
    setHasRun(true);
    // 全部先置「排队」：用户一眼能看到这一批要跑多少项，而不是一片「待执行」。
    // 用 key 集合做 O(1) 判定，而不是在 map 里嵌套 some（那是 O(n²)，
    // 19 个账号 × 十几项时会白跑几千次比较）。
    const plannedKeys = new Set(plan.flatMap((group) => group.items.map((entry) => entry.key)));
    setQueue((prev) =>
      prev.map((item) =>
        plannedKeys.has(item.key)
          ? { ...item, status: "queued" as QueueStatus, message: undefined }
          : item,
      ),
    );

    let cursor = 0;
    const worker = async () => {
      for (;;) {
        if (cancelledRef.current) return;
        const index = cursor++;
        if (index >= plan.length) return;
        const group = plan[index];
        // 账号内串行：同一账号的第二个任务不会在第一个还没结束时发出去。
        for (const entry of group.items) {
          if (cancelledRef.current) return;
          await runItem(group.accountId, entry);
        }
      }
    };
    const width = Math.max(1, Math.min(concurrency, plan.length));
    await Promise.all(Array.from({ length: width }, () => worker()));

    const wasCancelled = cancelledRef.current;
    runningRef.current = false;
    if (!mountedRef.current) return;

    // 取消后把还没开始的项目标成「跳过」，不留一堆永远的「排队」。
    //
    // 这些项也要计入 statsRef.skipped：界面计数来自队列状态、toast 计数来自
    // statsRef，两者不一致会让用户对不上账（尤其「跳过」在取消时往往是最大头）。
    //
    // 计数**必须在这里算好**，不能写进下面 updater 的闭包里：React StrictMode
    // 会故意双调用 state updater 来暴露副作用，在 updater 里改 ref 会双倍计数。
    // 未尝试项 = 总项数 − 已结算项（done/failed/skipped 都由 settle 计过）。
    if (wasCancelled) {
      const settledByItems = statsRef.current.done + statsRef.current.failed + statsRef.current.skipped;
      const neverStarted = Math.max(0, total - settledByItems);
      statsRef.current.skipped += neverStarted;
      // updater 保持纯函数：只依赖 prev。
      setQueue((prev) =>
        prev.map((item) =>
          item.status === "queued" || item.status === "pending"
            ? { ...item, status: "skipped" as QueueStatus, message: "已取消，未执行" }
            : item,
        ),
      );
    }
    setRunning(false);

    const { done, failed, skipped } = statsRef.current;
    const detail = [`成功 ${done} 项`, `失败 ${failed} 项`];
    if (skipped > 0) detail.push(`跳过 ${skipped} 项`);
    if (wasCancelled) {
      toast.info("任务队列已取消", { description: detail.join("，") });
    } else if (failed > 0) {
      toast.error("任务队列执行完成，但有失败项", { description: detail.join("，") });
    } else {
      toast.success("任务队列执行完成", { description: detail.join("，") });
    }
    onFinished?.();
  }, [concurrency, onFinished, queue, runItem]);

  /** 取消：停止派发后续任务，在途的让它自然跑完（请求已经发出去了，撤不回来）。 */
  const cancelRun = useCallback(() => {
    cancelledRef.current = true;
  }, []);

  // 派生渲染数据 -------------------------------------------------------------

  const groups: AccountGroup[] = useMemo(() => groupByAccount(queue), [queue]);

  const total = queue.length;
  const settled = queue.filter((item) => isTerminal(item.status)).length;
  const doneCount = queue.filter((item) => item.status === "done").length;
  const failedCount = queue.filter((item) => item.status === "failed").length;
  const skippedCount = queue.filter((item) => item.status === "skipped").length;
  // total=0 时显式返回 0：`0/0*100` 会得到 NaN，宽度写成 "NaN%" 后进度条直接消失。
  const percent = total > 0 ? Math.round((settled / total) * 100) : 0;
  const progressPrefix = running ? "执行中" : hasRun ? "已结束" : "待执行";

  const nothingPending = !scanning && !running && total === 0 && scanFailures.length === 0;
  const allScansFailed = !scanning && !running && total === 0 && scanFailures.length > 0;

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        // 跑批期间不允许关闭：这一批会真实消耗 token 与额度，误关（点遮罩、按 Esc）
        // 会让用户以为任务停了，实际后台还在跑。要停请点「取消执行」。
        if (!next && running) return;
        onOpenChange(next);
      }}
    >
      <DialogContent className="sm:max-w-3xl" showCloseButton={!running}>
        <DialogHeader>
          <DialogTitle>跨账号任务队列</DialogTitle>
          <DialogDescription>
            扫描各账号可自动完成的成长任务，按并发档位批量执行并实时查看进度。
          </DialogDescription>
        </DialogHeader>

        <div className="flex flex-wrap items-center gap-2">
          <div className="flex items-center gap-2">
            <label htmlFor="task-queue-concurrency" className="text-xs font-medium text-muted-foreground">
              并发档位
            </label>
            <Select
              value={String(concurrency)}
              onValueChange={(value) => setConcurrency(Number(value))}
              disabled={running || scanning}
            >
              <SelectTrigger id="task-queue-concurrency" size="sm" className="w-20" aria-label="并发档位">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {CONCURRENCY_OPTIONS.map((option) => (
                  <SelectItem key={option} value={String(option)}>
                    {option}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>

          <Button
            variant="outline"
            size="sm"
            className="h-8 gap-1.5"
            onClick={() => void scan()}
            disabled={scanning || running}
          >
            {scanning ? <Loader2 className="animate-spin" /> : <ScanSearch />}
            {hasRun || total > 0 ? "重新扫描" : "扫描待办"}
          </Button>

          {total > 0 && (
            <Button
              size="sm"
              className="h-8 gap-1.5"
              onClick={() => void startRun()}
              disabled={running || scanning}
            >
              {running ? <Loader2 className="animate-spin" /> : <Play />}
              执行全部待办
            </Button>
          )}

          {running && (
            <Button variant="outline" size="sm" className="h-8 gap-1.5" onClick={cancelRun}>
              <Square />
              取消执行
            </Button>
          )}

          <span className="ml-auto text-[11px] text-muted-foreground">
            同时最多跑 {concurrency} 个账号，账号内逐项串行
          </span>
        </div>

        {(excluded.intl > 0 || excluded.disabled > 0) && (
          <p className="text-[11px] leading-5 text-muted-foreground">
            已排除
            {excluded.intl > 0 ? ` ${excluded.intl} 个国际版账号（成长任务体系不同，无对应接口）` : ""}
            {excluded.intl > 0 && excluded.disabled > 0 ? "、" : ""}
            {excluded.disabled > 0 ? ` ${excluded.disabled} 个已禁用账号（不在网关账号池中）` : ""}
            。
          </p>
        )}

        {total > 0 && (
          <div className="space-y-2">
            <div className="flex items-center justify-between gap-3 text-xs">
              <span className="font-medium tabular-nums">
                {progressPrefix} {settled} / {total}
              </span>
              <span className="tabular-nums text-muted-foreground">
                完成 {doneCount}
                {failedCount > 0 ? ` · 失败 ${failedCount}` : ""}
                {skippedCount > 0 ? ` · 跳过 ${skippedCount}` : ""}
              </span>
            </div>
            <div
              role="progressbar"
              aria-valuemin={0}
              aria-valuemax={total}
              aria-valuenow={settled}
              aria-label={`任务队列进度：${progressPrefix} ${settled} / ${total}`}
              className="h-1.5 overflow-hidden rounded-full bg-muted"
            >
              <div
                className={cn("h-full rounded-full transition-[width] duration-300", failedCount > 0 ? "bg-primary/70" : "bg-primary")}
                style={{ width: `${percent}%` }}
              />
            </div>
          </div>
        )}

        <div className="max-h-[52vh] space-y-4 overflow-y-auto pr-1">
          {scanning && (
            <div className="flex items-center gap-2 py-8 text-sm text-muted-foreground">
              <Loader2 className="animate-spin" />
              正在扫描 {eligibleAccounts.length} 个账号的待办任务…
            </div>
          )}

          {nothingPending && (
            <div className="rounded-xl border border-dashed px-4 py-10 text-center">
              <p className="text-sm text-muted-foreground">
                {eligibleAccounts.length === 0
                  ? "没有可执行成长任务的账号。成长任务仅国服、且账号需在网关账号池中。"
                  : "这些账号当前没有可自动完成的待办任务。"}
              </p>
              <p className="mt-2 text-xs leading-5 text-muted-foreground">
                {eligibleAccounts.length === 0
                  ? "请先在账号列表导入或启用国服账号，再回到这里扫描。"
                  : "任务可能已全部完成，或都已完成领奖。稍后有新活动时再扫描一次即可。"}
              </p>
              <Button variant="outline" size="sm" className="mt-3 gap-1.5" onClick={() => void scan()}>
                <RefreshCw />
                重新扫描
              </Button>
            </div>
          )}

          {allScansFailed && (
            <div className="rounded-xl border border-dashed px-4 py-10 text-center">
              <p className="text-sm text-muted-foreground">所有账号都没能取到任务列表。</p>
              <p className="mt-2 text-xs leading-5 text-muted-foreground">
                常见原因：网关未启动，或账号尚未同步到网关账号池。修好后重新扫描即可。
              </p>
              <Button variant="outline" size="sm" className="mt-3 gap-1.5" onClick={() => void scan()}>
                <RefreshCw />
                重新扫描
              </Button>
            </div>
          )}

          {scanFailures.length > 0 && (
            <section className="space-y-1.5">
              <header className="flex items-center gap-2 px-1">
                <h3 className="text-sm font-semibold tracking-tight">扫描失败</h3>
                <Badge variant="secondary" className="h-5 rounded-full border-0 px-1.5 text-[11px] tabular-nums text-muted-foreground shadow-none">
                  {scanFailures.length}
                </Badge>
              </header>
              <ul className="divide-y divide-border rounded-xl border">
                {scanFailures.map((failure) => (
                  <li key={failure.accountId} className="flex items-start gap-2.5 px-3 py-2">
                    <span className="mt-1.5 size-1.5 shrink-0 rounded-full bg-destructive" aria-hidden="true" />
                    <div className="min-w-0 flex-1">
                      <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
                        <span className="truncate text-sm font-medium">{failure.accountName}</span>
                        <Badge variant="outline" className={cn("ml-auto shrink-0 gap-1 text-[10px]", STATUS_CHIP.failed)}>
                          失败
                        </Badge>
                      </div>
                      <p className="mt-1 text-xs leading-5 text-muted-foreground">{failure.message}</p>
                    </div>
                  </li>
                ))}
              </ul>
            </section>
          )}

          {groups.map((group) => {
            return (
              <section key={group.accountId} className="space-y-1.5">
                <header className="flex flex-wrap items-center gap-2 px-1">
                  <h3 className="text-sm font-semibold tracking-tight">{group.accountName}</h3>
                  <span className="text-[11px] tabular-nums text-muted-foreground">
                    {groupSummary(group)}
                  </span>
                </header>
                <ul className="divide-y divide-border rounded-xl border">
                  {group.items.map((item) => (
                    <li key={item.key} className="flex items-start gap-2.5 px-3 py-2">
                      <span
                        className={cn("mt-1.5 size-1.5 shrink-0 rounded-full", STATUS_DOT[item.status])}
                        aria-hidden="true"
                      />
                      <div className="min-w-0 flex-1">
                        <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
                          <span className="truncate text-sm font-medium">{item.taskName}</span>
                          <code className="rounded bg-muted px-1 py-0.5 text-[10px] text-muted-foreground">
                            {item.taskCode}
                          </code>
                          {item.progressText ? (
                            <span className="text-[11px] tabular-nums text-muted-foreground">
                              {item.progressText}
                            </span>
                          ) : null}
                          <Badge
                            variant="outline"
                            className={cn("ml-auto shrink-0 gap-1 text-[10px]", STATUS_CHIP[item.status])}
                          >
                            {item.status === "running" && <Loader2 className="animate-spin" />}
                            {STATUS_TEXT[item.status]}
                          </Badge>
                        </div>
                        {item.message ? (
                          <p className="mt-1 text-xs leading-5 text-muted-foreground">{item.message}</p>
                        ) : null}
                      </div>
                    </li>
                  ))}
                </ul>
              </section>
            );
          })}
        </div>

        <DialogFooter>
          {running && (
            <p className="mr-auto text-xs text-muted-foreground">
              执行中不能关闭面板，如需停止请点「取消执行」。
            </p>
          )}
          <Button variant="outline" onClick={() => onOpenChange(false)} disabled={running}>
            关闭
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/**
 * 该任务是否属于「待办」。
 *
 * 口径照抄后端 `growtask.PendingTasks`：`!Claimed && Automatable`。
 * 已领奖的不再跑（跑了也是白费上游请求）；不可自动完成的只展示指引、不给入口，
 * 因此这里直接滤掉，避免队列里出现注定「跳过」的行。
 *
 * 「已领奖」用 `status === "claimed"` 判断，而不是读某个布尔字段：Go 侧
 * `TaskView` 里 `claimed` 是个**非 omitempty 的 bool**，但前端 `GrowthTaskView`
 * 只声明了 `claimable` —— 直接读 `task.claimed` 会编译不过。用 status 更稳：
 * 它是后端 `viewStatus` 推导出的稳定枚举（ViewClaimed），语义唯一。
 */
function isPendingTask(task: GrowthTaskView): boolean {
  return task.status !== "claimed" && task.automatable === true;
}
