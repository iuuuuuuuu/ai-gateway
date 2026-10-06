import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import {
  AlertTriangle,
  ArrowLeftRight,
  Ban,
  ArrowRight,
  CalendarCheck,
  CheckCircle2,
  CircleCheck,
  Coins,
  Ellipsis,
  Fingerprint,
  Globe,
  History,
  Info,
  Loader2,
  MonitorSmartphone,
  PlayCircle,
  RefreshCw,
  RotateCcw,
  Save,
  ShieldCheck,
  Sparkles,
  Trash2,
  UserPlus,
  Users,
} from "lucide-react";

import { AccountGroupsDialog, GroupSelect, groupColorClass } from "@/components/account-groups-dialog";
import { AccountRecordsView } from "@/components/account-records-view";
import { AppEnvConfigCard } from "@/components/app-env-config-card";
import { CheckinTrends } from "@/components/checkin-trends";
import { formatJwtHours, payIdentityClass } from "@/components/credit-cell";
import { PlatformConfigButton, PlatformConfigDialog } from "@/components/platform-config-dialog";
import { StatusDot, TraeMark } from "@/components/product-marks";
import { TraeCreditsPanel } from "@/components/trae-credits-panel";
import { TraeOAuthDialog } from "@/components/trae-oauth-dialog";
import { TraeTransferDialog } from "@/components/trae-transfer-dialog";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Separator } from "@/components/ui/separator";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import * as api from "@/lib/api";
import { formatCredit } from "@/lib/credit-package-composition";
import { SENSITIVE_STRING_INPUT_PROPS } from "@/lib/sensitive-input";
import {
  BACKGROUND_TASK_DONE_DOM_EVENT,
  type BackgroundTaskDone,
} from "@/lib/use-background-task-refresh";
import type {
  AccountGroup,
  AppEnvStatus,
  SnapshotItem,
  TraeAccountMeta,
  TraeCheckinResult,
  TraeDiscoveredAccount,
} from "@/lib/api";
import { cn } from "@/lib/utils";
import type { ProductAccountTask } from "@/components/product-account-card";

/** 两个 Trae 应用。两者共用同一账号库，只是登录态互相独立。 */
const TRAE_APPS = [
  { kind: "TraeWork", label: "Trae Work", variant: "work" as const },
  { kind: "Trae", label: "Trae", variant: "cn" as const },
];

/**
 * 「配置」弹窗里的应用环境条目：两个 Trae 客户端的安装路径。
 *
 * 只含本平台自己的应用 —— 豆包那条归豆包页，设置页不再集中展示
 * （所有者强约束：平台专属配置下放到各自 tab 的「配置」里）。
 * 写成模块级常量而不是在组件里现建：它进了 `load` 的依赖数组。
 */
const TRAE_ENV_KINDS = [
  { kind: "TraeWork", label: "Trae Work", hint: "TRAE SOLO CN.exe" },
  { kind: "Trae", label: "Trae", hint: "Trae CN.exe" },
] as const;

/**
 * 积分详情的进程内缓存 + 并发闸门。
 *
 * 为什么需要：`trae_credit_detail` 一次要打三条上游账（IDE 积分 / 权益包 / 付费身份），
 * 而卡片是「每张卡进来就查一次」。账号一多，页面一打开就会同时打出 N 组上游请求 ——
 * 既慢又容易触发限流。这里做两件事：
 *   1. 同一账号的结果缓存 5 分钟，`load()` 重跑（切换账号、签到之后都会重跑）不再重查；
 *   2. 同时最多 2 个账号在查，其余排队，避免瞬时并发。
 * 缓存是模块级的，故跨 tab 切换、跨组件实例都有效；页面刷新即失效。
 */
const CREDIT_CACHE_TTL_MS = 5 * 60 * 1000;
const CREDIT_MAX_CONCURRENCY = 2;
const creditCache = new Map<string, { detail: api.TraeCreditDetail; at: number }>();
const creditInflight = new Map<string, Promise<api.TraeCreditDetail>>();
let creditActive = 0;
const creditQueue: (() => void)[] = [];

function creditAcquire(): Promise<void> {
  if (creditActive < CREDIT_MAX_CONCURRENCY) {
    creditActive += 1;
    return Promise.resolve();
  }
  return new Promise<void>((resolve) => {
    creditQueue.push(() => {
      creditActive += 1;
      resolve();
    });
  });
}

function creditRelease(): void {
  creditActive -= 1;
  const next = creditQueue.shift();
  if (next) next();
}

/** 取积分详情：命中缓存直接返回，否则排队查询并回填缓存。 */
function loadTraeCredit(userId: string): Promise<api.TraeCreditDetail> {
  const hit = creditCache.get(userId);
  if (hit && Date.now() - hit.at < CREDIT_CACHE_TTL_MS) return Promise.resolve(hit.detail);
  const running = creditInflight.get(userId);
  if (running) return running;
  const task = creditAcquire()
    .then(() => api.traeCreditDetail(userId))
    .then((detail) => {
      creditCache.set(userId, { detail, at: Date.now() });
      return detail;
    })
    .finally(() => {
      creditInflight.delete(userId);
      creditRelease();
    });
  creditInflight.set(userId, task);
  return task;
}

/**
 * Trae 账号卡片上的「本账号任务」清单。
 *
 * 所有者强约束：每个平台的账号卡片都要有同一形态的任务区 —— 卡上就能看到
 * 本账号有哪些任务、今天做没做、还能不能再点一次。这里与
 * `components/product-account-card.tsx` 的 `ProductAccountTask` 用同一套
 * 状态语义，菜单项的 `data-slot` / `data-task` / `data-state` 也逐字一致，
 * 好让 CDP 探针能用同一个选择器扫全站。
 *
 * 任务 id 直接用**已有的**计划任务键（`scheduler::TaskKind`），不另起名字 ——
 * 否则账号记录里的事件流（`record_checkin_task_records` 写的是
 * `TaskKind::TraeCheckin.label()`）与卡片上显示的会对不上。
 *
 * 状态判定（`item` 即 `traeCheckinStatus()` 里该账号那一行）：
 *   · `success` / `already` / `checked_in` ⇒ done：今天已经签过，置灰。
 *   · `not_checked_in`                     ⇒ ready：上游明确说没签，可以签。
 *   · `skip`                               ⇒ blocked：冷却中，后端本轮会跳过。
 *   · `fail`                               ⇒ ready：失败多半是瞬时网络，点一次
 *     重试正是用户想要的；画成「不可用」会让他以为今天没救了。
 *   · 缺键 / `null`                        ⇒ ready：**不知道 ≠ 没签**，但允许点。
 */
function traeTasksOf(item: api.TraeCheckinToday | undefined): ProductAccountTask[] {
  const status = item?.status ?? "";
  const done = status === "success" || status === "already" || status === "checked_in";
  const cooling = status === "skip";
  const checkin: ProductAccountTask = {
    id: "trae-checkin",
    label: "Trae 每日签到",
    state: done ? "done" : cooling ? "blocked" : "ready",
    reason: done
      ? "今天已经签过到，再点也不会重复发放"
      : cooling
        ? "该账号正在冷却中，本轮会被跳过；可先在「养护」里清除冷却"
        : status === "fail"
          ? "上次签到失败，可再点一次重试"
          : undefined,
  };
  // 积分快照没有「单账号当日状态」可读：`trae_credits_history.json` 里同日同 uid
  // 有行只代表被写过一次，而签到流程也会写它，反推不出「今天采样过没有」。
  // 故恒为 ready —— 再采一次只是覆盖当天同一行，没有副作用。
  const snapshot: ProductAccountTask = {
    id: "trae-credits-snapshot",
    label: "Trae 积分日快照（本账号）",
    state: "ready",
    reason: "向 Trae 上游拉一次本账号的积分总额并写入当日快照",
  };
  return [checkin, snapshot];
}

/** 任务组标题后缀。口径与 `components/product-account-card.tsx` 完全一致。 */
function taskGroupSuffixOf(tasks: ProductAccountTask[]): string {
  const runnable = tasks.filter((t) => t.state === "ready").length;
  if (runnable > 0) return `（还有 ${runnable} 个可执行）`;
  if (tasks.some((t) => t.state === "done")) return "（今日已完成）";
  if (tasks.some((t) => t.state === "detail")) return "（无可领取项）";
  return "（当前不可执行）";
}

/**
 * 卡片里的积分区。
 *
 * 为什么不读 `account.creditsTotal`：Trae 侧的 `credits_total` **从来没有写入点**
 * （只有 `qoder_account.rs` / `zcode_account.rs` 会写），所以它恒为 null，
 * 直接渲染会把每个账号都画成「没数据」。这里改为按需向 `traeCreditDetail` 要详情，
 * 拿不到就照实显示拿不到，不猜。
 *
 * 形态对齐 `account-card.tsx` 的积分区（大数字 + N 个积分包 + 近期到期列表），
 * 而不是 `CreditCell` 的气泡 —— 卡片里已经有位置，再点一次才看得到没有必要。
 */
function TraeCreditSection({ userId }: { userId: string }) {
  const [detail, setDetail] = useState<api.TraeCreditDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [allOpen, setAllOpen] = useState(false);

  useEffect(() => {
    let alive = true;
    setLoading(true);
    setError(null);
    setDetail(null);
    loadTraeCredit(userId)
      .then((d) => {
        if (alive) setDetail(d);
      })
      .catch((e: unknown) => {
        if (alive) setError(e instanceof Error ? e.message : String(e));
      })
      .finally(() => {
        if (alive) setLoading(false);
      });
    return () => {
      alive = false;
    };
  }, [userId]);

  if (loading) {
    return (
      <div className="flex items-center gap-2 py-3 text-sm text-muted-foreground">
        <Loader2 className="size-4 animate-spin" />
        积分查询中…
      </div>
    );
  }
  if (error) {
    return (
      <div className="flex min-w-0 items-center gap-2 py-3 text-sm text-destructive" title={error}>
        <Coins className="size-4 shrink-0" />
        <span className="min-w-0 truncate">{error}</span>
      </div>
    );
  }
  if (!detail) {
    return <p className="py-3 text-sm text-muted-foreground">等待积分数据…</p>;
  }

  const packs = detail.packs;
  const shown = packs.slice(0, 2);
  return (
    <div className="min-w-0">
      <div className="flex items-baseline gap-x-3 gap-y-1">
        <Sparkles className="size-4 shrink-0 translate-y-0.5 stroke-[1.75] text-muted-foreground" aria-hidden="true" />
        <strong
          className="text-[22px] font-semibold leading-none tabular-nums tracking-[-0.025em]"
          style={{ fontFamily: '"Bricolage Grotesque Variable", "SF Pro Display", ui-sans-serif, sans-serif' }}
        >
          {formatCredit(detail.total)}
        </strong>
        <span className="text-xs text-muted-foreground">{packs.length} 个积分包</span>
        {detail.payIdentity && (
          <Badge variant="outline" className={cn("ml-auto text-[10px]", payIdentityClass(detail.payIdentity))}>
            {detail.payIdentity}
          </Badge>
        )}
      </div>

      {packs.length > 0 ? (
        <div className="mt-3 min-w-0">
          <p className="text-[11px] font-medium text-muted-foreground">近期到期</p>
          <div className="mt-1.5 space-y-2">
            {shown.map((pack) => (
              <div key={pack.name} className="min-w-0">
                <div className="grid min-w-0 grid-cols-[auto_minmax(0,1fr)_auto] items-center gap-3 text-xs">
                  <span className="rounded-lg bg-muted/80 px-2 py-1 font-medium tabular-nums text-foreground">
                    {formatCredit(pack.remaining)} 积分
                  </span>
                  <span className="min-w-0 truncate" title={pack.name}>
                    {pack.name}
                  </span>
                  <span className={cn("shrink-0 tabular-nums", packExpiryClass(pack.expireAt))}>
                    {formatPackExpiry(pack.expireAt)}
                  </span>
                </div>
                <div className="mt-1.5 h-1 overflow-hidden rounded-full bg-muted" aria-hidden="true">
                  <div
                    className={cn(
                      "h-full rounded-full",
                      packExpiryClass(pack.expireAt) === "text-destructive" ? "bg-orange-500" : "bg-primary",
                    )}
                    style={{ width: `${packRatio(pack)}%` }}
                  />
                </div>
              </div>
            ))}
          </div>
          {packs.length > 2 && (
            <button
              type="button"
              className="mt-3 inline-flex w-fit items-center gap-1.5 text-xs font-medium text-primary hover:underline"
              onClick={() => setAllOpen(true)}
            >
              查看全部积分包 <ArrowRight className="size-3.5" />
            </button>
          )}
        </div>
      ) : (
        <p className="mt-3 text-xs text-muted-foreground">暂无可用积分包</p>
      )}

      {detail.payExpireAt && (
        <p className="mt-2 text-[11px] text-muted-foreground">
          套餐到期 {formatPackExpiry(detail.payExpireAt)}
        </p>
      )}

      {/* 部分账没拿到时照实说，而不是把缺失当 0。 */}
      {detail.errors.length > 0 && (
        <div className="mt-2 rounded-md border border-amber-500/40 bg-amber-500/10 px-2 py-1.5 text-[11px] text-amber-700 dark:text-amber-300">
          <p className="font-medium">部分数据未能获取：</p>
          <ul className="mt-0.5 list-inside list-disc">
            {detail.errors.map((e, i) => (
              <li key={i} className="break-all">
                {e}
              </li>
            ))}
          </ul>
        </div>
      )}

      <Dialog open={allOpen} onOpenChange={setAllOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>全部积分包</DialogTitle>
            <DialogDescription>共 {packs.length} 个，按到期时间从近到远。</DialogDescription>
          </DialogHeader>
          <div className="max-h-[60vh] min-w-0 overflow-y-auto divide-y divide-border/60">
            {packs.map((pack) => (
              <div key={pack.name} className="min-w-0 py-2.5 first:pt-0 last:pb-0">
                <div className="flex min-w-0 items-baseline justify-between gap-3">
                  <span className="min-w-0 truncate text-sm" title={pack.name}>
                    {pack.name}
                  </span>
                  <span className="shrink-0 text-sm font-medium tabular-nums">
                    {formatCredit(pack.remaining)}
                    <span className="text-xs font-normal text-muted-foreground"> / {formatCredit(pack.total)}</span>
                  </span>
                </div>
                <div className="mt-2 h-1.5 overflow-hidden rounded-full bg-muted" aria-hidden="true">
                  <div className="h-full rounded-full bg-primary/75" style={{ width: `${packRatio(pack)}%` }} />
                </div>
                <p className={cn("mt-1 text-[11px]", packExpiryClass(pack.expireAt))}>
                  {formatPackExpiry(pack.expireAt)}
                </p>
              </div>
            ))}
          </div>
        </DialogContent>
      </Dialog>
    </div>
  );
}

/**
 * 权益包到期时间的展示。
 *
 * 后端可能给出 2100 年以后的哨兵时间表示「长期有效」（与 `CreditCell` 同口径），
 * 直接显示会变成一串没人看得懂的日期，故单独识别。
 */
function formatPackExpiry(expireAt: string | null | undefined): string {
  if (!expireAt) return "长期有效";
  const year = Number(expireAt.slice(0, 4));
  if (Number.isFinite(year) && year >= 2100) return "长期有效";
  const ts = Date.parse(expireAt);
  if (!Number.isFinite(ts)) return expireAt.slice(0, 10);
  return new Date(ts).toLocaleDateString("zh-CN", { month: "2-digit", day: "2-digit" }) + " 到期";
}

/** 到期临近/已过用颜色区分，正常为次要文字色。 */
function packExpiryClass(expireAt: string | null | undefined): string {
  if (!expireAt) return "text-muted-foreground";
  const ts = Date.parse(expireAt);
  if (!Number.isFinite(ts)) return "text-muted-foreground";
  const now = Date.now();
  if (ts <= now) return "text-destructive";
  if (ts - now < 7 * 24 * 3600 * 1000) return "text-orange-600 dark:text-orange-400";
  return "text-muted-foreground";
}

/** 权益包剩余比例（0–100）。总量取不到时给 0，不假装满格。 */
function packRatio(pack: api.TraeCreditPack): number {
  if (!Number.isFinite(pack.total) || pack.total <= 0) return 0;
  const ratio = (pack.remaining / pack.total) * 100;
  return Math.max(0, Math.min(100, ratio));
}

/** 区域徽标：sg 国际版 / cn 国内版；兜底推断时标注「推断」以示区别。 */
function realmChip(account: TraeAccountMeta) {
  if (!account.realm) return null;
  const isIntl = account.realm === "sg";
  const label = isIntl ? "国际版" : "国内版";
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Badge
          variant="outline"
          className={cn(
            "cursor-help gap-1",
            isIntl
              ? "border-sky-500/30 bg-sky-500/10 text-sky-700 dark:text-sky-300"
              : "border-slate-400/30 bg-slate-500/10 text-slate-700 dark:text-slate-300",
          )}
        >
          <Globe className="size-3" />
          {label}
          {account.realmDetected === false ? "（推断）" : ""}
        </Badge>
      </TooltipTrigger>
      <TooltipContent side="top" className="max-w-xs">
        {account.realmLabel ?? label}
        {account.realmDetected === false
          ? "；该判定为兜底推断，依据不足，可能不准"
          : account.realmEvidence
            ? `；判定依据：${account.realmEvidence}`
            : ""}
      </TooltipContent>
    </Tooltip>
  );
}

/** JWT 状态的展示样式。 */
function jwtBadge(status: TraeAccountMeta["jwtStatus"], hours: number | null) {
  const label =
    status === "ok"
      ? "有效"
      : status === "warn"
        ? "即将过期"
        : status === "expired"
          ? "已过期"
          : "未知";
  const variant =
    status === "ok" ? "secondary" : status === "warn" ? "outline" : "destructive";
  const tip =
    hours == null
      ? "无法从 JWT 解析过期时间"
      : status === "ok"
        ? `剩余约 ${Math.floor(hours / 24)} 天`
        : status === "warn"
          ? `仅剩约 ${hours.toFixed(1)} 小时，请尽快重新登录`
          : "JWT 已过期，需要重新登录";
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Badge variant={variant as never} className="cursor-help">
          {label}
        </Badge>
      </TooltipTrigger>
      <TooltipContent side="top">{tip}</TooltipContent>
    </Tooltip>
  );
}

/** 冷却状态展示。 */
function cooldownBadge(cooldown: TraeAccountMeta["cooldown"]) {
  const until = cooldown?.until;
  if (!until || until <= Date.now() / 1000) return null;
  const type = cooldown?.type ?? "";
  // 永久失效（SessionDead）用一个大但有限的时间戳表示，单独识别以免显示成「几万天后」
  const permanent = until > 9_000_000_000;
  const remaining = permanent ? "" : `（剩 ${Math.ceil((until - Date.now() / 1000) / 60)} 分钟）`;
  const label = permanent
    ? "会话失效"
    : type === "PlanLimit"
      ? "套餐限制"
      : type === "SoftRate"
        ? "限流冷却"
        : "冷却中";
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Badge variant="outline" className="cursor-help gap-1 border-amber-500/40 text-amber-600 dark:text-amber-400">
          <AlertTriangle className="size-3" />
          {label}
          {remaining}
        </Badge>
      </TooltipTrigger>
      <TooltipContent side="top" className="max-w-xs">
        {cooldown?.reason || "该账号暂时不可用"}
        {permanent ? "；需重新登录该账号" : ""}
      </TooltipContent>
    </Tooltip>
  );
}

/**
 * Trae 账号卡片。
 *
 * 布局照 `components/account-card.tsx`（WorkBuddy 账号页用的那套）：
 * header（头像 + 名称 + 副标题 + 状态徽标 + 右上「⋯」菜单）
 * → section（积分详情，`flex-1` 吃掉多余高度，让同排卡片等高）
 * → footer（主操作按钮）。
 *
 * `h-full` 是等高对齐的关键：父级 grid 用 `auto-rows-fr`，
 * 卡片不写 `h-full` 就撑不满行高，同排卡片会参差不齐。
 */
function TraeAccountCard({
  account,
  groups,
  variant,
  todayCheckedIn,
  tasks,
  onRunTask,
  onMoveGroup,
  onDelete,
  onClearCooldown,
  onResetDevice,
  onSwitch,
  onSaveLogin,
  onToggleDisabled,
  busy,
}: {
  account: TraeAccountMeta;
  groups: AccountGroup[];
  variant: "work" | "cn";
  /**
   * 今天签到了吗：`true` 已签 / `false` 未签 / `undefined` **不知道**。
   *
   * ★ 三态是刻意的：`undefined` 时**不渲染**徽标。落库表里没有这个 uid 就
   * 说明没问到上游（网络异常 / 上游 9074），画成「未签到」是假信息 ——
   * 用户会照着假信息重复点签到。
   */
  todayCheckedIn?: boolean;
  /**
   * 本账号任务（见 `traeTasksOf`）。不传 / 空数组 ⇒ 不渲染任务组。
   *
   * ★ done / blocked 项**仍要显示（置灰）**，不能隐藏 —— 用户需要看到
   * 「这个任务存在、今天已经做过了」。藏起来会让他以为功能没了。
   */
  tasks?: ProductAccountTask[] | undefined;
  onRunTask?: ((taskId: string) => void) | undefined;
  onMoveGroup: (groupId: string | null) => void;
  onDelete: () => void;
  onClearCooldown: () => void;
  onResetDevice: () => void;
  onSwitch: () => void;
  onSaveLogin: () => void;
  /** 切换「是否进入网关账号池」；不弹确认 —— 它是可逆的开关，不是删除。 */
  onToggleDisabled: () => void;
  busy: boolean;
}) {
  // 账号所属分组：后端下发的是分组 id（`group_id`），
  // 这里换成组对象以取到名称与颜色；查不到即「未分组」。
  const groupId = account.group ?? null;
  const group = groups.find((g) => g.id === groupId) ?? null;
  const cooling = Boolean(account.cooldown?.until && account.cooldown.until > Date.now() / 1000);
  // 「查看记录」弹窗：与 WorkBuddy（account-card.tsx）/ ZCode / Qoder 同一入口形态。
  const [recordsOpen, setRecordsOpen] = useState(false);
  // 本账号任务（不传 ⇒ 空数组 ⇒ 整组不渲染）。
  const taskItems = tasks ?? [];

  // 卡片上的品牌图标按**账号自身**的区域选，而不是按当前标签页：
  //   `realm === "sg"` → 国际版 work 图标（白底黑字形）；
  //   其余（`cn` / 后端没给 realm）→ 国内版 cn 图标（#1A1B1D 底 + #32F08C 薄荷绿）。
  //
  // ★ 为什么不能直接用 `variant`（= `activeApp.variant`）：两个标签页共用同一份
  // 账号列表（`activeApp` 只影响环境检测与「当前账号」查询），所以在「Trae Work」
  // 标签下也会列出国内版账号 —— 用 `variant` 会让国内版账号顶着白底黑字形的
  // 国际版图标，且作为水印时白底黑字形在浅色 header 上只剩一块灰斑，
  // 正是所有者反馈的「卡片背景不是他的 icon」。
  const markVariant: "work" | "cn" =
    account.realm === "sg" ? "work" : account.realm === "cn" ? "cn" : variant;

  const chips = (
    <>
      {/* 禁用标记放在最前（与 WorkBuddy 卡片 account-card.tsx 同一套语义）：
          它是「这个号当前不接流量」的最强信号。★ 禁用的只是进网关账号池的
          资格，签到 / 上报等养号任务照跑 —— 文案别写成「已停用账号」。 */}
      {account.disabled ? (
        <Badge
          variant="outline"
          className="gap-1 rounded-md border-destructive/40 px-1.5 py-0 text-[11px] font-medium text-destructive"
          title="已禁用：不进入网关账号池（签到等养号任务仍在运行）"
        >
          <Ban className="size-3 shrink-0" />
          已禁用
        </Badge>
      ) : null}
      {jwtBadge(account.jwtStatus, account.jwtExpHours)}
      {cooldownBadge(account.cooldown)}
      {realmChip(account)}
      {/* 今日签到状态：与 WorkBuddy 账号卡同一套三态语义（见 account-card.tsx）。
          `undefined` 不渲染 —— 「没问到」不是「未签到」。 */}
      {todayCheckedIn !== undefined && (
        <Badge
          variant={todayCheckedIn ? "success" : "secondary"}
          className={cn("rounded-md px-1.5 py-0 text-[11px] font-medium", !todayCheckedIn && "text-muted-foreground")}
        >
          <CircleCheck className="size-3" />
          {todayCheckedIn ? "已签到" : "未签到"}
        </Badge>
      )}
      {account.refreshTokenInvalid && (
        <Badge variant="destructive" className="text-[10px]">
          refresh token 已失效
        </Badge>
      )}
      {/* 分组色徽标：颜色由分组自身决定，用与分组管理弹窗同一套色表，
          两处颜色一致才认得出是同一个分组。 */}
      {group && (
        <Badge variant="outline" className={cn("text-[10px]", groupColorClass(group.color))}>
          {group.name}
        </Badge>
      )}
    </>
  );

  return (
    <>
    <article
      className="flex h-full min-w-0 flex-col overflow-hidden rounded-2xl border border-border bg-card shadow-[0_1px_2px_rgba(15,23,42,.025),0_10px_28px_rgba(15,23,42,.035)] transition-shadow hover:shadow-[0_2px_4px_rgba(15,23,42,.04),0_14px_34px_rgba(15,23,42,.055)]"
      data-slot="trae-account-card"
    >
      <header className="relative flex min-h-[104px] items-center border-b border-border bg-muted/30 px-5 py-3">
        {/* 装饰层：右下角一枚半透明大 mark，作为卡片的品牌底纹 */}
        <div className="pointer-events-none absolute inset-0 overflow-hidden" aria-hidden="true">
          <div className="absolute -right-10 -top-16 size-24 rounded-full bg-primary/5 blur-2xl" />
          {/* 2026-10-05 重新定标（所有者反馈「trae 的卡片背景不是他的 icon」）。
              旧写法 `-bottom-6 -right-3 rotate-12 opacity-[0.05] saturate-0` 是三重削弱叠加，
              渲染出来只是一块斜置灰斑，根本认不出是 Trae 官方图标。
              iso 实例 + CDP 逐变体实测结论（`test-bin/iso-new/runs/wm-*.png`）：
                · 旋转必须去掉 —— 斜置后矩形轮廓不再可辨；
                · `saturate-0` 必须去掉 —— Trae CN 图标的识别度几乎全靠 #32F08C 薄荷绿；
                · 0.05 太淡；0.12 在浅色 header 上清晰可辨，又不至于抢内容；
                · 位置收回 header 内（right 6 / bottom 4）：旧写法右下各出界一截，
                  被 `overflow-hidden` 裁掉，只剩残缺一角 —— 这才是「灰斑」的主因。
              注：官方图标是整块实色圆角方（无透明边距），不能靠加大尺寸找存在感。 */}
          <TraeMark
            size={96}
            variant={markVariant}
            className="absolute bottom-1 right-1.5 opacity-[0.12]"
          />
        </div>

        <div className="absolute right-3.5 top-3.5 z-20">
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button
                variant="ghost"
                size="icon"
                className="size-8 rounded-lg text-muted-foreground hover:text-foreground"
                aria-label={`${account.name} 的更多操作`}
                title="更多账号操作"
              >
                <Ellipsis />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" className="w-64">
              {taskItems.length > 0 && onRunTask && (
                <>
                  <DropdownMenuLabel className="text-xs">
                    本账号任务{taskGroupSuffixOf(taskItems)}
                  </DropdownMenuLabel>
                  {taskItems.map((t) => (
                    <DropdownMenuItem
                      key={t.id}
                      data-slot="product-task-item"
                      data-task={t.id}
                      data-state={t.state}
                      disabled={t.state !== "ready"}
                      onSelect={() => {
                        if (t.state === "ready") onRunTask(t.id);
                      }}
                      title={t.reason}
                      className="gap-2 text-xs"
                    >
                      {t.state === "done" ? (
                        <CheckCircle2 className="h-3.5 w-3.5 shrink-0 text-emerald-600" />
                      ) : t.state === "blocked" ? (
                        <AlertTriangle className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                      ) : t.state === "detail" ? (
                        <Info className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                      ) : (
                        <PlayCircle className="h-3.5 w-3.5 shrink-0" />
                      )}
                      <span className="min-w-0 flex-1 truncate">{t.label}</span>
                      {t.state === "done" && (
                        <span className="shrink-0 text-[10px] text-muted-foreground">已完成</span>
                      )}
                      {t.state === "blocked" && (
                        <span className="shrink-0 text-[10px] text-muted-foreground">不可用</span>
                      )}
                      {t.state === "detail" && (
                        <span className="shrink-0 text-[10px] text-muted-foreground">仅详情</span>
                      )}
                    </DropdownMenuItem>
                  ))}
                  <DropdownMenuSeparator />
                </>
              )}
              <DropdownMenuLabel>本账号操作</DropdownMenuLabel>
              <DropdownMenuItem className="items-start" disabled={busy} onSelect={onSwitch}>
                <span className="mt-0.5 flex shrink-0">
                  <MonitorSmartphone className="size-4" />
                </span>
                <span className="min-w-0 flex-1">
                  <span className="block truncate">切换到此账号</span>
                  <span className="mt-0.5 block whitespace-normal text-[11px] leading-4 text-muted-foreground">
                    备份当前登录态，再恢复该账号的快照
                  </span>
                </span>
              </DropdownMenuItem>
              <DropdownMenuItem className="items-start" disabled={busy} onSelect={onSaveLogin}>
                <span className="mt-0.5 flex shrink-0">
                  <Save className="size-4" />
                </span>
                <span className="min-w-0 flex-1">
                  <span className="block truncate">保存登录态</span>
                  <span className="mt-0.5 block whitespace-normal text-[11px] leading-4 text-muted-foreground">
                    把当前登录态存成该账号的快照
                  </span>
                </span>
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuLabel>养护</DropdownMenuLabel>
              <DropdownMenuItem className="items-start" disabled={busy} onSelect={onResetDevice}>
                <span className="mt-0.5 flex shrink-0">
                  <RotateCcw className="size-4" />
                </span>
                <span className="min-w-0 flex-1">
                  <span className="block truncate">重置设备指纹</span>
                  <span className="mt-0.5 block whitespace-normal text-[11px] leading-4 text-muted-foreground">
                    为下次登录生成新的设备标识
                  </span>
                </span>
              </DropdownMenuItem>
              {cooling && (
                <DropdownMenuItem className="items-start" disabled={busy} onSelect={onClearCooldown}>
                  <span className="mt-0.5 flex shrink-0">
                    <RefreshCw className="size-4" />
                  </span>
                  <span className="min-w-0 flex-1">
                    <span className="block truncate">清除冷却</span>
                    <span className="mt-0.5 block whitespace-normal text-[11px] leading-4 text-muted-foreground">
                      立即让该账号重新参与调度
                    </span>
                  </span>
                </DropdownMenuItem>
              )}
              <DropdownMenuItem className="items-start" onSelect={() => setRecordsOpen(true)}>
                <span className="mt-0.5 flex shrink-0">
                  <History className="size-4" />
                </span>
                <span className="min-w-0 flex-1">
                  <span className="block truncate">查看记录</span>
                  <span className="mt-0.5 block whitespace-normal text-[11px] leading-4 text-muted-foreground">
                    该账号的签到、积分变化与任务记录
                  </span>
                </span>
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuLabel>路由</DropdownMenuLabel>
              <DropdownMenuItem className="items-start" disabled={busy} onSelect={onToggleDisabled}>
                <span className="mt-0.5 flex shrink-0">
                  {account.disabled ? <CircleCheck className="size-4" /> : <Ban className="size-4" />}
                </span>
                <span className="min-w-0 flex-1">
                  <span className="block truncate">
                    {account.disabled ? "启用（重新加入账号池）" : "禁用（不进入账号池）"}
                  </span>
                  <span className="mt-0.5 block whitespace-normal text-[11px] leading-4 text-muted-foreground">
                    签到、上报等养号任务照常运行
                  </span>
                </span>
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem
                className="items-start text-destructive focus:text-destructive"
                disabled={busy}
                onSelect={onDelete}
              >
                <span className="mt-0.5 flex shrink-0">
                  <Trash2 className="size-4" />
                </span>
                <span className="min-w-0 flex-1">
                  <span className="block truncate">删除账号</span>
                  <span className="mt-0.5 block whitespace-normal text-[11px] leading-4">
                    连同该账号的登录态快照一起移除
                  </span>
                </span>
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>

        <div className="relative z-10 flex w-full min-w-0 items-center gap-3 pr-10">
          <div className="relative shrink-0">
            <TraeMark size={44} variant={markVariant} className="ring-4 ring-white/65" />
            <StatusDot on={account.jwtStatus === "ok"} className="absolute -bottom-0.5 -right-0.5 ring-2 ring-card" />
          </div>
          <div className="min-w-0 flex-1">
            <h3 className="truncate text-sm font-semibold leading-5" title={account.name}>
              {account.name}
            </h3>
            <p className="mt-0.5 flex min-w-0 items-center gap-1.5 truncate text-xs leading-5 text-muted-foreground">
              <span className="font-mono">{account.userId.slice(0, 8)}</span>
              <span aria-hidden="true">·</span>
              <Fingerprint className="size-3 shrink-0" />
              <span className="truncate">{account.deviceIdMasked}</span>
            </p>
            <div className="mt-1.5 flex min-w-0 flex-wrap items-center gap-1.5">{chips}</div>
          </div>
        </div>
      </header>

      <section className="flex min-w-0 flex-1 flex-col px-5 pb-4 pt-4">
        <TraeCreditSection userId={account.userId} />

        <div className="mt-auto flex flex-wrap items-center gap-2 pt-3 text-xs text-muted-foreground">
          <span>JWT 剩余 {formatJwtHours(account.jwtExpHours)}</span>
          {account.canRefresh === false && <span>· 无法自动续期</span>}
          {account.hasRefreshToken && account.canRefresh !== false && <span>· 可自动续期</span>}
        </div>

        <div className="mt-3 flex flex-wrap items-center gap-2">
          <GroupSelect groups={groups} value={groupId} disabled={busy} onChange={onMoveGroup} />
        </div>
      </section>

      <footer className="flex flex-wrap items-center gap-2.5 border-t px-5 py-2.5">
        <Button
          variant="outline"
          size="sm"
          className="h-7 rounded-full px-2.5 pr-3.5 text-xs"
          disabled={busy}
          onClick={onSwitch}
        >
          <MonitorSmartphone className="size-4" />
          切换
        </Button>
        <Button
          variant="outline"
          size="sm"
          className="h-7 rounded-full px-2.5 pr-3.5 text-xs"
          disabled={busy}
          onClick={onSaveLogin}
        >
          <Save className="size-4" />
          保存登录态
        </Button>
      </footer>
    </article>
    {/* 记录入口放在账号菜单里而不是单独一页：用户想知道「这个号昨天干了什么」时，
        视线就在这张卡片上，不该再去别处找（与 account-card.tsx 同一决策）。
        fixedAccountId 用 Trae uid：后端写记录时 accountId 就是它（Trae 账号不在宿主账号库里）。 */}
    <Dialog open={recordsOpen} onOpenChange={setRecordsOpen}>
      <DialogContent className="sm:max-w-4xl">
        <DialogHeader>
          <DialogTitle>账号记录</DialogTitle>
          <DialogDescription>
            {account.name} 的签到、积分变化与任务记录；可按日期区间筛选。
          </DialogDescription>
        </DialogHeader>
        <div className="min-w-0 max-h-[70vh] overflow-y-auto pr-1">
          <AccountRecordsView accounts={[]} fixedAccountId={account.userId} compact />
        </div>
      </DialogContent>
    </Dialog>
    </>
  );
}

function SnapshotList({
  appKind,
  appLabel,
  onChanged,
}: {
  appKind: string;
  appLabel: string;
  onChanged: () => void;
}) {
  const [snapshots, setSnapshots] = useState<SnapshotItem[]>([]);
  const [currentUserId, setCurrentUserId] = useState("");
  const [loading, setLoading] = useState(true);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const res = await api.listSnapshots(appKind);
      setSnapshots(res.snapshots);
      setCurrentUserId(res.currentUserId);
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setLoading(false);
    }
  }, [appKind]);

  useEffect(() => {
    void load();
  }, [load]);

  const remove = async (userId: string) => {
    try {
      await api.deleteSnapshot(appKind, userId);
      toast.success(`已删除 ${userId} 的登录态快照`);
      await load();
      onChanged();
    } catch (e) {
      toast.error(api.asError(e));
    }
  };

  if (loading) {
    return (
      <div className="space-y-2">
        <Skeleton className="h-12 w-full" />
        <Skeleton className="h-12 w-full" />
      </div>
    );
  }
  if (snapshots.length === 0) {
    return (
      <p className="rounded-lg border border-dashed border-border/60 px-4 py-6 text-center text-sm text-muted-foreground">
        还没有 {appLabel} 的登录态快照。在客户端登录账号后点「保存登录态」即可创建。
      </p>
    );
  }
  return (
    <div className="space-y-2">
      <p className="text-xs text-muted-foreground">
        当前登录账号：
        <span className="font-mono">{currentUserId || "未知"}</span>
        {currentUserId ? "（切换时会把现场登录态写回它）" : ""}
      </p>
      {snapshots.map((snap) => (
        <div
          key={snap.userId}
          className="flex items-center gap-3 rounded-lg border border-border/60 px-4 py-2.5"
        >
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2">
              <span className="truncate font-mono text-sm">{snap.userId}</span>
              {snap.isCurrent && (
                <Badge variant="secondary" className="text-[10px]">
                  当前登录
                </Badge>
              )}
              {!snap.hasMeta && (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Badge variant="outline" className="cursor-help text-[10px]">
                      旧格式
                    </Badge>
                  </TooltipTrigger>
                  <TooltipContent side="top">
                    该快照由旧版本创建，缺少版本元数据，恢复时不做版本校验
                  </TooltipContent>
                </Tooltip>
              )}
            </div>
            {snap.modifiedAt && (
              <span className="text-xs text-muted-foreground">
                {new Date(snap.modifiedAt * 1000).toLocaleString("zh-CN")}
              </span>
            )}
          </div>
          <Button size="sm" variant="ghost" onClick={() => void remove(snap.userId)}>
            <Trash2 className="size-3.5 text-destructive" />
          </Button>
        </div>
      ))}
    </div>
  );
}

/**
 * Trae 账号页。
 *
 * `embedded`：被「账号管理」页（AccountHubPage）以 tab 内容嵌进来时为 true ——
 * 去掉最外层留白容器与页面级大标题，只保留自己的操作按钮行，避免同屏出现
 * 两个页面标题。默认 false 时行为与合并前**逐字一致**。
 */
export default function TraePage({ embedded = false }: { embedded?: boolean } = {}) {
  const [activeApp, setActiveApp] = useState(TRAE_APPS[0]);
  const [accounts, setAccounts] = useState<TraeAccountMeta[]>([]);
  const [env, setEnv] = useState<AppEnvStatus | null>(null);
  const [discovered, setDiscovered] = useState<TraeDiscoveredAccount[]>([]);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [checkinResult, setCheckinResult] = useState<TraeCheckinResult | null>(null);
  // 今日签到状态（本地落库快照，零网络请求）。
  //
  // ★ 为什么要有这个 state：过去「今天签没签」只存在于 `checkinResult` 里 ——
  // 那是**本次点击**的返回值，页面一刷新就没了。于是用户打开 Trae 页看到的是
  // 「不知道签没签」，即使当天早上客户端早就自动签过了。落库表 + 这个 state
  // 才能把「今天已签到」稳定显示出来。
  const [todayMap, setTodayMap] = useState<api.TraeCheckinTodayMap | null>(null);
  const [addOpen, setAddOpen] = useState(false);
  const [addJwt, setAddJwt] = useState("");
  const [addName, setAddName] = useState("");
  const [addRefresh, setAddRefresh] = useState("");
  const [groups, setGroups] = useState<AccountGroup[]>([]);
  const [groupsOpen, setGroupsOpen] = useState(false);
  const [oauthOpen, setOauthOpen] = useState(false);
  // 「配置」弹窗：应用环境（两个 Trae 客户端的安装路径）。
  const [configOpen, setConfigOpen] = useState(false);
  const [transferOpen, setTransferOpen] = useState(false);
  // 签到趋势图的刷新信号：签到成功后 +1，让趋势图重新拉数据。
  // 用递增计数器而不是把数据提上来，是因为趋势图有自己的区间选择
  // （14/30/90 天）与缓存，父组件不该替它决定什么时候重取。
  const [checkinTrendKey, setCheckinTrendKey] = useState(0);
  // 切换前记录的当前账号：作为防误覆盖守卫传给后端
  const expectedUidRef = useRef<string>("");

  // 分组读不到不该让整页报错：账号列表本身仍然可用，
  // 只是分组徽标与下拉暂时空着。
  const loadGroups = useCallback(async () => {
    try {
      const res = await api.groupsList("Trae");
      setGroups(res.groups);
    } catch {
      setGroups([]);
    }
  }, []);

  // 只读本地落库表（不触网）。失败静默：拿不到就显示「不知道」，
  // 不该因为一张辅助状态表读不到而给用户弹错误。
  const loadToday = useCallback(async () => {
    try {
      setTodayMap(await api.traeCheckinStatus());
    } catch {
      setTodayMap(null);
    }
  }, []);

  // 挂载时**静默**探测一次上游：用户当天可能早就签过了（在客户端里签的、
  // 或计划任务签的），只读本地表只会显示「不知道」。
  //
  // 用 ref 保证整页只跑一次 —— 它与 `activeApp` 无关（Trae 两个应用共用同一
  // 账号库），跟着 `load` 跑会在切换 Trae Work/Trae 时重复打一遍上游。
  const todayRefreshedRef = useRef(false);
  useEffect(() => {
    if (todayRefreshedRef.current) return;
    todayRefreshedRef.current = true;
    api
      .traeCheckinRefresh()
      .then(() => loadToday())
      .catch(() => {
        // 静默失败：探测不到就继续显示「不知道」，不打扰用户
      });
  }, [loadToday]);

  const load = useCallback(async () => {
    try {
      const [list, envStatus] = await Promise.all([
        api.traeListAccounts(),
        api.appEnvCheck(activeApp.kind),
      ]);
      setAccounts(list.accounts);
      setEnv(envStatus);
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setLoading(false);
    }
  }, [activeApp.kind]);

  // 后台签到跑完 → 立刻刷新本页（今日状态 + 账号列表 + 趋势图）。
  //
  // 事件由宿主 `spawn_background_loops` emit、`useBackgroundTaskRefresh` 转成
  // DOM 事件（那个 hook 挂在根组件上，永不卸载）。本页是 keep-alive 的 tab：
  // 常驻之后 `load`/`loadToday` 只在依赖变化时跑，没有这条订阅，后台签到成功
  // 后用户切回本页看到的仍是「未签到」。
  useEffect(() => {
    const onDone = (event: Event) => {
      const detail = (event as CustomEvent<BackgroundTaskDone>).detail;
      if (!detail?.kind?.startsWith("trae")) return;
      void load();
      void loadToday();
      // 趋势图是签到的结果视图，与手动签到后一样要重取一次
      setCheckinTrendKey((k) => k + 1);
    };
    window.addEventListener(BACKGROUND_TASK_DONE_DOM_EVENT, onDone);
    return () => window.removeEventListener(BACKGROUND_TASK_DONE_DOM_EVENT, onDone);
  }, [load, loadToday]);

  useEffect(() => {
    setLoading(true);
    void load();
    void loadGroups();
    void loadToday();
  }, [load, loadGroups, loadToday]);

  // 刷新当前登录账号（守卫依据），页面切换应用时也要重新读
  useEffect(() => {
    let disposed = false;
    api
      .currentAccount(activeApp.kind)
      .then((res) => {
        if (!disposed) expectedUidRef.current = res.userId;
      })
      .catch(() => {
        // 读不到不算错误：守卫会退化为「跳过写回来源槽」，不影响切换本身
      });
    return () => {
      disposed = true;
    };
  }, [activeApp.kind, accounts.length]);

  const runAction = async (
    action: api.SwitchActionName,
    userId?: string,
    successMessage?: string,
  ) => {
    setBusy(true);
    try {
      const res = await api.switchAction({
        action,
        targetApp: activeApp.kind,
        userId: userId ?? null,
        expectedCurrentUid: expectedUidRef.current || null,
      });
      toast.success(successMessage ?? res.message);
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doCheckin = async () => {
    setBusy(true);
    try {
      const res = await api.traeCheckinRun();
      setCheckinResult(res);
      const parts = [`成功 ${res.ok}`];
      if (res.already) parts.push(`已签 ${res.already}`);
      if (res.skipped) parts.push(`跳过 ${res.skipped}`);
      if (res.failed) parts.push(`失败 ${res.failed}`);
      if (res.failed > 0) toast.warning(`签到完成：${parts.join(" · ")}`);
      else toast.success(`签到完成：${parts.join(" · ")}`);
      // 趋势图是签到的结果视图，签到完必须重取一次，
      // 否则用户刚签完却看不到今天那一格变化，会以为没签上。
      setCheckinTrendKey((k) => k + 1);
      await load();
      // 签到刚落了库，徽标要跟着变（否则「已签到」要等下次刷新才出现）
      await loadToday();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  /** 手动重探一次今日签到状态（用户看到「不知道」时点）。 */
  const doRefreshToday = async () => {
    setBusy(true);
    try {
      const res = await api.traeCheckinRefresh();
      await loadToday();
      if (res.updated === 0) {
        toast.warning("没能从上游确认任何账号的状态，稍后再试");
      } else {
        toast.success(`已确认 ${res.updated} 个账号的今日签到状态`);
      }
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  /**
   * 跑一条「本账号任务」（账号卡片菜单里的那一组）。
   *
   * 与页面顶部「立即签到」的区别：那是**全量**签到，这里是单账号 ——
   * 用户在某张卡上点，就只动这个账号，不会顺带把别的账号也签一遍。
   * 执行完必须回写 `checkinResult` / 重载今日状态：卡片上的「已完成 /
   * 可执行」与页面顶部的汇总徽标都读这两份数据，不刷新的话用户点完
   * 看不出发生了什么（这正是所有者报「看不到具体账号的任务执行结果」的成因）。
   */
  const doRunTask = async (account: TraeAccountMeta, taskId: string) => {
    if (busy) return;
    setBusy(true);
    try {
      if (taskId === "trae-checkin") {
        const res = await api.traeCheckinRun([account.userId]);
        setCheckinResult(res);
        setCheckinTrendKey((k) => k + 1);
        await load();
        await loadToday();
        const one = res.results.find((r) => r.userId === account.userId) ?? res.results[0];
        const detail = one?.message ? `：${one.message}` : "";
        if (res.failed > 0) {
          toast.warning(`${account.name} 签到失败${detail}`);
        } else if (res.skipped > 0) {
          toast.info(`${account.name} 本轮被跳过${detail}`);
        } else {
          toast.success(`${account.name} 签到完成${detail}`);
        }
      } else if (taskId === "trae-credits-snapshot") {
        const res = await api.traeCreditsSnapshot([account.userId]);
        if (res.sampled === 0) {
          const why = res.errors[0] ?? "上游没有返回积分总额";
          toast.warning(`${account.name} 积分采样未完成：${why}`);
        } else if (res.failed > 0) {
          toast.warning(`${account.name} 积分采样部分失败：${res.errors.join("；")}`);
        } else {
          toast.success(`已写入 ${account.name} 的当日积分快照`);
        }
        await loadToday();
      }
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  // uid → 今日签到三态。只取布尔：`checkedIn === null` 是「没问到」，
  // 与「表里没这个 uid」等价，都让卡片不渲染徽标。
  const todayCheckedInMap = useMemo(() => {
    const map: Record<string, boolean> = {};
    for (const [uid, item] of Object.entries(todayMap?.accounts ?? {})) {
      if (item.checkedIn !== null) map[uid] = item.checkedIn;
    }
    return map;
  }, [todayMap]);

  // 今日汇总（用于「每日签到」卡片在没有本次结果时也能显示当天进度）。
  // ★ 冷却中（skip）单列一档：它同样是「今天没签」，但原因不是网络问不到 ——
  // 把它并进「未确认」会让用户以为只是没探到，从而反复点「刷新状态」白跑。
  const todaySummary = useMemo(() => {
    const items = Object.values(todayMap?.accounts ?? {});
    const skipped = items.filter((i) => i.status === "skip").length;
    return {
      total: items.length,
      checkedIn: items.filter((i) => i.checkedIn === true).length,
      skipped,
      unknown: items.filter((i) => i.checkedIn === null).length - skipped,
    };
  }, [todayMap]);

  const doMoveGroup = async (account: TraeAccountMeta, groupId: string | null) => {
    try {
      await api.groupMove("Trae", account.userId, groupId);
      await load();
      await loadGroups();
    } catch (e) {
      toast.error(api.asError(e));
    }
  };

  const doDiscover = async () => {
    setBusy(true);
    try {
      const res = await api.traeDiscoverAccounts();
      setDiscovered(res.accounts);
      if (res.accounts.length === 0) {
        toast.info("未在本机发现 Trae 登录痕迹。请先启动客户端并登录一次。");
      }
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doAdd = async () => {
    if (!addJwt.trim()) {
      toast.error("请粘贴 JWT");
      return;
    }
    setBusy(true);
    try {
      const res = await api.traeAddAccount({
        jwt: addJwt.trim(),
        name: addName.trim() || undefined,
        refreshToken: addRefresh.trim() || undefined,
      });
      toast.success(`已保存账号 ${res.account.name}`);
      setAddOpen(false);
      setAddJwt("");
      setAddName("");
      setAddRefresh("");
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doDelete = async (account: TraeAccountMeta) => {
    if (!window.confirm(`确认删除账号 ${account.name}（${account.userId}）？`)) return;
    setBusy(true);
    try {
      await api.traeDeleteAccount(account.userId);
      toast.success("已删除");
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  // 切换「是否进入网关账号池」。
  //
  // ★ 刻意**不弹确认**：它是可逆的开关，不是删除；弹窗只会让常用操作变重。
  // ★ 与 doDelete 不同，这里不 await load() 之外的乐观更新 —— 列表刷新由
  //   load() 统一负责，卡片上的徽标与菜单文案都跟着 account.disabled 走。
  const doToggleDisabled = async (account: TraeAccountMeta) => {
    setBusy(true);
    try {
      await api.traeUpdateAccount(account.userId, { disabled: !account.disabled });
      toast.success(account.disabled ? "已启用，重新加入账号池" : "已禁用，不再进入账号池（养号任务照跑）");
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doResetDevice = async (account: TraeAccountMeta) => {
    setBusy(true);
    try {
      const res = await api.traeDeviceInfo(account.userId, true);
      toast.success(`已为 ${account.name} 重新派生设备指纹（${res.deviceId}）`);
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <TooltipProvider delayDuration={250}>
      <div className={cn(embedded ? "space-y-5" : "w-full space-y-6 px-5 py-6 sm:px-8 sm:py-8")}>
        <header className={cn("flex flex-wrap items-start justify-between gap-4", !embedded && "mb-6")}>
          {!embedded && (
            <div className="min-w-0">
              <h1 className="flex items-center gap-2.5 text-[28px] font-semibold tracking-tight">
                <TraeMark size={30} variant={activeApp.variant} />
                Trae 账号
              </h1>
              <p className="mt-2 text-sm leading-6 text-muted-foreground">
                Trae Work 与 Trae 共用同一份账号库，但登录态快照互相独立。
              </p>
            </div>
          )}
          <div className={cn("flex shrink-0 flex-wrap items-center gap-2 pt-1", embedded && "ml-auto")}>
            <PlatformConfigButton onClick={() => setConfigOpen(true)}>配置</PlatformConfigButton>
            <Button variant="outline" size="sm" onClick={() => setGroupsOpen(true)}>
              <Users className="size-3.5" />
              分组管理
            </Button>
            <Button variant="outline" size="sm" onClick={() => setTransferOpen(true)}>
              <ArrowLeftRight className="size-3.5" />
              导出/导入
            </Button>
            <Button variant="outline" size="sm" disabled={busy} onClick={() => void doDiscover()}>
              <UserPlus className="size-3.5" />
              发现本机账号
            </Button>
            <Button size="sm" disabled={busy} onClick={() => setOauthOpen(true)}>
              <ShieldCheck className="size-3.5" />
              OAuth 登录
            </Button>
            <Button variant="outline" size="sm" disabled={busy} onClick={() => setAddOpen(true)}>
              <UserPlus className="size-3.5" />
              粘贴 JWT 添加
            </Button>
          </div>
        </header>

        <Tabs
          value={activeApp.kind}
          onValueChange={(value) => {
            const next = TRAE_APPS.find((app) => app.kind === value);
            if (next) setActiveApp(next);
          }}
        >
          {/* Trae Work / Trae 是**账号来源**切换，不是平台切换。嵌入到账号管理页时
              它要和页面级的平台 Tabs 同屏出现，若都用灰底胶囊会让人以为有两层同级
              切换 ⇒ 嵌入模式下改成描边分段控件，视觉上明确「这是页内的次级筛选」。 */}
          <TabsList className={embedded ? "border border-border bg-transparent" : undefined}>
            {TRAE_APPS.map((app) => (
              <TabsTrigger key={app.kind} value={app.kind} className="gap-2">
                <TraeMark size={16} variant={app.variant} />
                {app.label}
              </TabsTrigger>
            ))}
          </TabsList>

          {TRAE_APPS.map((app) => (
            <TabsContent key={app.kind} value={app.kind} className="space-y-5">
              {/* 环境状态 */}
              {env && (
                <Card>
                  <CardHeader className="pb-3">
                    <CardTitle className="text-base">{app.label} 环境</CardTitle>
                    <CardDescription>
                      {env.installed
                        ? `已检测到客户端：${env.exePath}`
                        : "未检测到客户端，请在设置中手动指定安装路径。"}
                    </CardDescription>
                  </CardHeader>
                  <CardContent className="flex flex-wrap items-center gap-2 text-xs">
                    <Badge variant={env.running ? "secondary" : "outline"}>
                      {env.running ? "运行中" : "未运行"}
                    </Badge>
                    <Badge variant="outline">快照 {env.snapshotCount} 个</Badge>
                    <Badge variant={env.dataDirExists ? "outline" : "destructive"}>
                      {env.dataDirExists ? "数据目录存在" : "数据目录不存在"}
                    </Badge>
                    <span className="font-mono text-muted-foreground">{env.dataDir}</span>
                  </CardContent>
                </Card>
              )}

              {/* 签到 */}
              <Card>
                <CardHeader className="flex-row items-center justify-between space-y-0 pb-3">
                  <div>
                    <CardTitle className="text-base">每日签到</CardTitle>
                    <CardDescription>
                      按账号顺序执行，已签的会跳过；失败账号按错误类型落冷却，避免反复撞限流。
                    </CardDescription>
                  </div>
                  <div className="flex shrink-0 items-center gap-2">
                    <Button
                      variant="outline"
                      size="sm"
                      disabled={busy || accounts.length === 0}
                      title="重新向 Trae 查询每个账号今天签到了吗（只读探测，不会签到）"
                      onClick={() => void doRefreshToday()}
                    >
                      <RefreshCw className={cn("size-3.5", busy && "animate-spin")} />
                      刷新状态
                    </Button>
                    <Button size="sm" disabled={busy || accounts.length === 0} onClick={() => void doCheckin()}>
                      {busy ? (
                        <Loader2 className="size-3.5 animate-spin" />
                      ) : (
                        <CalendarCheck className="size-3.5" />
                      )}
                      一键签到
                    </Button>
                  </div>
                </CardHeader>
                {/* 今日进度：来自落库表，**不依赖本次点击**。
                    没有这一段时，用户刚打开页面（或刚重启应用）看到的是空白，
                    即使当天早就签过了也看不出「今天签没签」。 */}
                {todaySummary.total > 0 && (
                  <CardContent className="space-y-2 pt-0">
                    <Separator />
                    <div className="flex flex-wrap items-center gap-2 text-xs">
                      <span className="text-muted-foreground">今日</span>
                      <Badge variant="success">已签到 {todaySummary.checkedIn}</Badge>
                      {todaySummary.total - todaySummary.checkedIn - todaySummary.unknown > 0 && (
                        <Badge variant="outline">
                          未签到 {todaySummary.total - todaySummary.checkedIn - todaySummary.unknown}
                        </Badge>
                      )}
                      {todaySummary.skipped > 0 && (
                        <Badge
                          variant="outline"
                          className="text-amber-600 dark:text-amber-400"
                          title="账号在冷却中，本轮没有请求上游；具体原因见账号卡片"
                        >
                          冷却中 {todaySummary.skipped}
                        </Badge>
                      )}
                      {todaySummary.unknown > 0 && (
                        <Badge
                          variant="secondary"
                          className="text-muted-foreground"
                          title="已发起探测但没拿到结果（网络异常或上游繁忙），点「刷新状态」可重试"
                        >
                          未确认 {todaySummary.unknown}
                        </Badge>
                      )}
                      <span className="text-muted-foreground">· {todayMap?.date}</span>
                    </div>
                  </CardContent>
                )}
                {checkinResult && (
                  <CardContent className="space-y-2 pt-0">
                    <Separator />
                    <div className="flex flex-wrap gap-2 text-xs">
                      <Badge variant="secondary">成功 {checkinResult.ok}</Badge>
                      <Badge variant="outline">已签 {checkinResult.already}</Badge>
                      {checkinResult.skipped > 0 && (
                        <Badge variant="outline">跳过 {checkinResult.skipped}</Badge>
                      )}
                      {checkinResult.failed > 0 && (
                        <Badge variant="destructive">失败 {checkinResult.failed}</Badge>
                      )}
                    </div>
                    <div className="space-y-1">
                      {checkinResult.results
                        .filter((r) => r.status !== "success" && r.status !== "already")
                        .map((r) => (
                          <div
                            key={r.userId}
                            className="flex items-start gap-2 rounded border border-border/60 px-3 py-1.5 text-xs"
                          >
                            <span className="shrink-0 font-medium">{r.name}</span>
                            <span className="text-muted-foreground">{r.message}</span>
                          </div>
                        ))}
                    </div>
                  </CardContent>
                )}
              </Card>

              {/* 签到趋势：签到是「每天都要做」的事，光看今天的结果看不出
                  账号是不是在持续掉签，故把历史趋势放在结果下面。 */}
              <CheckinTrends refreshKey={checkinTrendKey} />

              {/* 账号列表 */}
              <section className="space-y-3">
                <div className="flex items-center justify-between">
                  <h2 className="text-sm font-medium">
                    账号（{accounts.length}）
                  </h2>
                  <Button variant="ghost" size="sm" disabled={busy} onClick={() => void load()}>
                    <RefreshCw className={cn("size-3.5", busy && "animate-spin")} />
                    刷新
                  </Button>
                </div>

                {loading ? (
                  <div className="grid auto-rows-fr min-w-0 gap-5 grid-cols-[repeat(auto-fit,minmax(min(100%,340px),1fr))]">
                    <Skeleton className="h-72 w-full rounded-2xl" />
                    <Skeleton className="h-72 w-full rounded-2xl" />
                  </div>
                ) : accounts.length === 0 ? (
                  <Alert>
                    <Coins className="size-4" />
                    <AlertTitle>还没有 Trae 账号</AlertTitle>
                    <AlertDescription>
                      可以点「发现本机账号」从客户端读取，或粘贴 Cloud-IDE-JWT 手动添加。
                    </AlertDescription>
                  </Alert>
                ) : (
                  /* 卡片网格：auto-rows-fr 让同一排的卡片等高。
                     注意不要加 items-start —— 它会让卡片不拉伸，auto-rows-fr 随之失效，
                     同排卡片就会因为积分包数量不同而参差不齐。 */
                  <div className="grid auto-rows-fr min-w-0 gap-5 grid-cols-[repeat(auto-fit,minmax(min(100%,340px),1fr))]">
                    {accounts.map((account) => (
                      <TraeAccountCard
                        key={account.userId}
                        account={account}
                        groups={groups}
                        variant={activeApp.variant}
                        todayCheckedIn={todayCheckedInMap[account.userId]}
                        tasks={traeTasksOf(todayMap?.accounts[account.userId])}
                        onRunTask={(taskId) => void doRunTask(account, taskId)}
                        busy={busy}
                        onMoveGroup={(groupId) => void doMoveGroup(account, groupId)}
                        onSwitch={() => void runAction("Switch", account.userId)}
                        onSaveLogin={() =>
                          void runAction(
                            "SaveCurrentLogin",
                            account.userId,
                            `已把当前登录态保存到 ${account.name}`,
                          )
                        }
                        onToggleDisabled={() => void doToggleDisabled(account)}
                        onResetDevice={() => void doResetDevice(account)}
                        onClearCooldown={async () => {
                          await api.traeClearCooldown(account.userId);
                          toast.success("已清除冷却");
                          await load();
                        }}
                        onDelete={() => void doDelete(account)}
                      />
                    ))}
                  </div>
                )}
              </section>

              {/* 快照 */}
              <Card>
                <CardHeader className="pb-3">
                  <CardTitle className="text-base">登录态快照</CardTitle>
                  <CardDescription>
                    每个账号一份独立快照。切换时先备份现场、再恢复目标账号的快照，
                    并保留一代备份可回退。
                  </CardDescription>
                </CardHeader>
                <CardContent>
                  <SnapshotList
                    appKind={app.kind}
                    appLabel={app.label}
                    onChanged={() => void load()}
                  />
                </CardContent>
              </Card>

              {/* 本机发现结果 */}
              {discovered.length > 0 && (
                <Card>
                  <CardHeader className="pb-3">
                    <CardTitle className="text-base">本机发现</CardTitle>
                    <CardDescription>
                      uid 由客户端使用痕迹推导。标记为「无法确认」的候选**不会**入池 ——
                      它的 uid 属于账户中心编号体系，与账号库不是同一套编号。
                    </CardDescription>
                  </CardHeader>
                  <CardContent className="space-y-2">
                    {discovered.map((item, index) => (
                      <div
                        key={`${item.userId || item.dcUid}-${index}`}
                        className="flex flex-wrap items-center gap-3 rounded-lg border border-border/60 px-4 py-2.5"
                      >
                        <div className="min-w-0 flex-1">
                          <div className="flex items-center gap-2">
                            <span className="truncate font-mono text-sm">
                              {item.uidConfident ? item.userId : "无法确认账号"}
                            </span>
                            <Badge variant="outline" className="text-[10px]">
                              {item.appLabel}
                            </Badge>
                            {item.inPool && (
                              <Badge variant="secondary" className="text-[10px]">
                                已在账号库
                              </Badge>
                            )}
                            {item.payIdentity && (
                              <Badge variant="outline" className="text-[10px]">
                                {item.payIdentity}
                              </Badge>
                            )}
                          </div>
                          {item.dcUid && (
                            <span className="text-xs text-muted-foreground">
                              账户中心编号 {item.dcUid}
                              {item.uidConfident ? "" : "（不能直接入库）"}
                            </span>
                          )}
                        </div>
                        {item.uidConfident && !item.inPool && (
                          <Button
                            size="sm"
                            variant="outline"
                            disabled={busy}
                            onClick={async () => {
                              // 本机发现只给出 uid，凭证仍需用户粘贴 JWT ——
                              // 客户端把 JWT 存在加密的 vscdb 里，离线取不出明文
                              setAddName(item.appLabel);
                              setAddOpen(true);
                              toast.info(
                                "请粘贴该账号的 Cloud-IDE-JWT（客户端登录态是加密存储的，无法离线读取）",
                              );
                            }}
                          >
                            添加
                          </Button>
                        )}
                      </div>
                    ))}
                  </CardContent>
                </Card>
              )}
              {/* 积分面板：跨账号的积分总览（趋势 + 消耗 + 各账号余额）。
                  放在账号列表与快照之后：先看单个账号，再看全局账。 */}
              <TraeCreditsPanel />
            </TabsContent>
          ))}
        </Tabs>

        <Dialog open={addOpen} onOpenChange={setAddOpen}>
          <DialogContent className="sm:max-w-lg">
            <DialogHeader>
              <DialogTitle>添加 Trae 账号</DialogTitle>
              <DialogDescription>
                粘贴 Cloud-IDE-JWT（可带 `Cloud-IDE-JWT ` 前缀，也可只贴 token 本体）。
                账号 id 会从 JWT 里自动解析。
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-3">
              <div className="space-y-1.5">
                <Label htmlFor="trae-jwt">JWT</Label>
                <Input
                  id="trae-jwt"
                  value={addJwt}
                  onChange={(e) => setAddJwt(e.target.value)}
                  placeholder="Cloud-IDE-JWT eyJ..."
                  className="font-mono text-xs"
                  // JWT 是 Base64 大小写敏感的随机串：被浏览器自动大写/纠错后
                  // 解析出的账号 id 会变，而用户看不出是哪一位被改了。
                  {...SENSITIVE_STRING_INPUT_PROPS}
                />
              </div>
              <div className="space-y-1.5">
                <Label htmlFor="trae-name">备注名（可选）</Label>
                {/* 备注名是**人类语言**（「主号」「公司号」），刻意**不**套
                    SENSITIVE_STRING_INPUT_PROPS：关掉自动大写会让移动端写
                    中文/英文句子时更难用，而这里改错大小写用户一眼能看出来。 */}
                <Input
                  id="trae-name"
                  value={addName}
                  onChange={(e) => setAddName(e.target.value)}
                  placeholder="例如：主号"
                />
              </div>
              <div className="space-y-1.5">
                <Label htmlFor="trae-refresh">refresh token（可选）</Label>
                <Input
                  id="trae-refresh"
                  value={addRefresh}
                  onChange={(e) => setAddRefresh(e.target.value)}
                  placeholder="填写后可自动续期"
                  className="font-mono text-xs"
                  // 同为大小写敏感的机器串，理由同上。
                  {...SENSITIVE_STRING_INPUT_PROPS}
                />
              </div>
            </div>
            <DialogFooter>
              <Button variant="outline" onClick={() => setAddOpen(false)}>
                取消
              </Button>
              <Button disabled={busy} onClick={() => void doAdd()}>
                {busy && <Loader2 className="size-3.5 animate-spin" />}
                保存
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>

        {/* 分组管理：增删改分组本身（改名、换色、排序、删除）。
            `onChanged` 要同时刷账号与分组 —— 删掉一个分组会让账号的
            归属变化，只刷分组会留下「账号还挂着已删除的组」的假象。 */}
        <AccountGroupsDialog
          open={groupsOpen}
          onOpenChange={setGroupsOpen}
          app="Trae"
          onChanged={() => {
            void load();
            void loadGroups();
          }}
        />

        {/* OAuth 登录：走官方授权码流程添加账号，成功后重载账号列表。 */}
        <TraeOAuthDialog open={oauthOpen} onOpenChange={setOauthOpen} onSuccess={() => void load()} />

        {/* 导出/导入：把账号在机器之间搬运（导出为文本，导入前先预览）。 */}
        <TraeTransferDialog
          open={transferOpen}
          onOpenChange={setTransferOpen}
          accounts={accounts}
          onImported={() => void load()}
        />

        {/* 平台配置：两个 Trae 客户端的安装路径（原先在设置页的「应用环境」）。 */}
        <PlatformConfigDialog
          open={configOpen}
          onOpenChange={setConfigOpen}
          title="Trae 配置"
          description="Trae Work 与 Trae 的客户端安装路径。自动探测失败时可在此手动指定。"
        >
          <AppEnvConfigCard kinds={TRAE_ENV_KINDS} id="trae-app-env" />
        </PlatformConfigDialog>
      </div>
    </TooltipProvider>
  );
}
