import { useMemo } from "react";
import { CircleAlert, Coins, Loader2, RefreshCw } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardDescription, CardHeader } from "@/components/ui/card";
import { cn } from "@/lib/utils";
import {
  accountDisplayName,
  accountSecondaryLabel,
  buildPackageColorMap,
  buildStackSegments,
  formatCredit,
  formatFullDate,
  formatMonthDay,
  packageColorKey,
  packageColorOf,
  packageDisplayName,
  resolveBalance,
  safeCredit,
  soonestExpireAt,
  sortPackageRows,
  usedOf,
  type PackageAccountView,
  type PackageCreditView,
  type PackageResourceView,
  type PackageStackSegment,
} from "@/lib/credit-package-composition";

/**
 * 「积分构成对比」——回答「为什么这个账号额度掉得快」。
 *
 * 背景（所有者点名的真实困惑）：
 *
 *   「两个任务完成度完全一致的账号，余额可能差上千 —— 差别只在包里」
 *
 * 账号余额是**若干积分包之和**，包按来源命名（「国内运营裂变包」「拉新权益包」
 * 「个人体验版」…），面额 6~1500 且按次发放。此前界面只有「N 个积分包」+ 一个
 * 逐包列表，看不出**跨账号的差别在哪** —— 缺的正是横向对比。
 *
 * 因此这里做两件事：
 *  1. 上方一排账号对比卡：大号余额 + 堆叠混色条 + 图例；
 *  2. 下方每账号一张明细表：包名 / 面额 / 剩余 / 已用 / 发放 / 到期。
 *
 * 最关键的一条约束是**同源同色**：同一个包名在所有账号、所有卡片、所有表格里
 * 必须是同一个颜色，否则跨账号对比立刻失效。取色逻辑集中在
 * `@/lib/credit-package-composition`（纯函数、无 React 依赖），
 * 以「包名」为键、按包名总量降序分配，与下标 / 账号 id / 渲染顺序全都无关。
 *
 * ── 字段核实结论（动手前逐个核对过后端实现，不是猜的）──────────────
 *
 * 真有（`crates/ai-gateway-core/src/modules/credits.rs::resource_summary`
 * 的输出键，对应 `src/lib/types.ts::CreditResource`）：
 *   · `packageName` / `packageCode`  → 「包名 / 来源」列
 *   · `total`                        → 「面额」列
 *   · `remaining`                    → 「剩余」列
 *   · `used`                         → 「已用」列
 *   · `expireAt`                     → 「到期」列
 *
 * **后端没有**（因此一律显示「—」，绝不编造）：
 *   · **发放时间** —— `resource_summary` 的输出里没有这个键；上游只在**请求体**里
 *     用过 `SlicePeriodStartTime`（credits.rs:409），那是「查哪个时间窗」的查询
 *     参数，不是某个包的发放时刻。Go 侧 `resourcePackage`
 *     （go-gateway/internal/upstream/client.go:1576）同样没有该字段。
 *     所以「发放」列**恒为「—」**：不用 `expireAt` 倒推、不拿当前时间顶替。
 *     → 要补的是后端 `grantedAt`（发放时刻，Unix 毫秒），前端已按此列位预留。
 *   · 包级「已用」的**独立数据源**：只有 `used` 一个，且与 `total - remaining` 同源。
 */

/** 图例最多显示几行；超出部分聚合成一行「其余 N 个包」，避免卡片被长图例撑变形。 */
const MAX_LEGEND_ITEMS = 6;

type CompositionState = "loading" | "missing" | "error" | "empty" | "ok";

interface AccountComposition {
  account: PackageAccountView;
  /** 展示名（nickname → note → uid 前缀 → id 前缀）。 */
  name: string;
  /** 次级标识（uid / id），副标题用。 */
  secondary: string;
  state: CompositionState;
  error: string | null;
  resources: PackageResourceView[];
  segments: PackageStackSegment[];
  /** 账号总余额：优先用后端聚合值，缺失时退化成各包剩余之和。 */
  balance: number;
  /** 各包 `remaining` 之和（**只来自资源列表**）。 */
  resourcesRemaining: number;
  /** 账号总面额（各包 `total` 之和），「已用」占比的分母。 */
  capacity: number;
  soonest: number | null;
}

export interface CreditPackageBreakdownProps {
  accounts: readonly PackageAccountView[];
  creditMap: Record<string, PackageCreditView | undefined>;
  creditLoadingMap: Record<string, boolean>;
  /** 首次采集进行中（无任何数据可展示时用来渲染骨架文案）。 */
  loading?: boolean;
  /** 采集失败原因（非空时给出重试入口）。 */
  error?: string | null;
  /** 重新采集积分；空状态与错误态的「下一步动作」都指向它。 */
  onRefresh?: () => void;
  /** 刷新进行中（按钮转圈并禁用）。 */
  refreshing?: boolean;
}

/**
 * 把 store 里的账号 + 积分资源摊平成组件直接可渲染的结构。
 *
 * 刻意不做任何跨账号聚合：每个账号的余额、构成、到期各自独立，
 * 跨账号的**唯一**共享状态是颜色表（见组件主体），那才是「可比」的来源。
 */
function toComposition(
  account: PackageAccountView,
  credit: PackageCreditView | undefined,
  loading: boolean,
  colorMap: ReadonlyMap<string, { key: string; color: string; totalAcrossAccounts: number }>,
): AccountComposition {
  const resources: PackageResourceView[] = credit?.ok ? [...(credit.resources ?? [])] : [];
  /**
   * 状态判定顺序里有一处刻意的取舍：**已有可用数据时不回退到 loading**。
   *
   * store 在「强制刷新」期间会把 `creditLoadingMap[id]` 置为 true，而
   * `creditMap[id]` 仍保留上一次的结果。若 loading 优先，用户每次点「刷新统计」
   * 都会看到所有卡片瞬间变成「积分查询中…」再变回来 —— 对比卡片本来就是用来
   * 盯着看的，闪烁会让人以为数据丢了。因此：**有旧数据就先展示旧数据**，
   * 只有确实没有可用数据时才显示加载态。
   */
  const hasUsableData = credit?.ok === true && resources.length > 0;
  const state: CompositionState = hasUsableData
    ? "ok"
    : loading
      ? "loading"
      : !credit
        ? "missing"
        : !credit.ok
          ? "error"
          : "empty";

  const capacity = resources.reduce((sum, resource) => sum + safeCredit(resource.total), 0);
  const resourcesRemaining = resources.reduce(
    (sum, resource) => sum + safeCredit(resource.remaining),
    0,
  );

  return {
    account,
    name: accountDisplayName(account),
    secondary: accountSecondaryLabel(account),
    state,
    error: credit?.error ?? null,
    resources,
    segments: buildStackSegments(resources, colorMap),
    balance: resolveBalance(credit?.totalRemaining, resources),
    resourcesRemaining,
    capacity,
    soonest: soonestExpireAt(resources),
  };
}

/** 堆叠混色条：div + flex 实现（**不用**图表库 —— 一条 10px 高的比例条不值得拉起 recharts）。 */
function PackageMixBar({ segments }: { segments: PackageStackSegment[] }) {
  return (
    // 纯装饰：包名与数值由下方图例以文本给出，读屏用户不会因此丢失信息。
    <div
      data-slot="credit-mixbar"
      className="flex h-2.5 w-full overflow-hidden rounded-full bg-muted"
      aria-hidden="true"
    >
      {segments.map((segment) => (
        <div
          key={segment.key}
          data-slot="credit-mixbar-segment"
          data-package-key={segment.key}
          className="h-full shrink-0"
          style={{ width: `${segment.percent}%`, backgroundColor: segment.color }}
        />
      ))}
    </div>
  );
}

/** 图例一行：色块 + 包名 + 剩余量。色块颜色与混色条、明细表行首色块**同源**。 */
function LegendRow({
  segment,
  compact = false,
}: {
  segment: PackageStackSegment;
  compact?: boolean;
}) {
  /**
   * 图例里的到期提示：这是「为什么这个账号额度掉得快」的第二条线索 ——
   * 同样两个包，一个 3 天后到期、一个 60 天后到期，用户的处置完全不同。
   * 到期时间来自后端真实字段（`expireAt`），取不到就什么都不显示（**不编造**）。
   */
  const expiryHint = segment.allExpired
    ? "已到期"
    : segment.soonestExpireAt === null
      ? "长期有效"
      : `${formatMonthDay(segment.soonestExpireAt)} 到期`;

  return (
    <div
      className={cn("flex min-w-0 items-center gap-2", compact ? "text-[11px]" : "text-xs")}
      title={`${segment.name} · 剩余 ${formatCredit(segment.remaining)} · 面额 ${formatCredit(
        segment.total,
      )} · ${expiryHint}${segment.count > 1 ? ` · ${segment.count} 个包合并` : ""}`}
    >
      <span
        data-slot="credit-legend-swatch"
        data-package-key={segment.key}
        className="size-2.5 shrink-0 rounded-[3px] ring-1 ring-inset ring-black/10 dark:ring-white/15"
        style={{ backgroundColor: segment.color }}
        aria-hidden="true"
      />
      <span className="min-w-0 flex-1 truncate">{segment.name}</span>
      {segment.count > 1 && (
        <span className="shrink-0 text-[10px] text-muted-foreground tabular-nums">
          ×{segment.count}
        </span>
      )}
      <span
        className={cn(
          "shrink-0 whitespace-nowrap text-[10px] tabular-nums",
          segment.allExpired ? "text-destructive" : "text-muted-foreground",
        )}
      >
        {expiryHint}
      </span>
      <span className="shrink-0 font-medium tabular-nums">{formatCredit(segment.remaining)}</span>
    </div>
  );
}

/** 单张账号对比卡。 */
function AccountCompositionCard({ item }: { item: AccountComposition }) {
  const legend = item.segments.slice(0, MAX_LEGEND_ITEMS);
  const rest = item.segments.slice(MAX_LEGEND_ITEMS);
  const restRemaining = rest.reduce((sum, segment) => sum + segment.remaining, 0);
  /**
   * 「已用占比」的分子分母必须来自**同一个数据源**（都取自资源列表）。
   *
   * 不能拿大号余额（可能来自后端聚合值 `totalRemaining`）去减面额：上游偶发
   * 返回不完整的包列表时，聚合值与逐包求和会对不上，两者相减会算出超过 100%
   * 或为负的「已用」—— 这正是后端 `credit_usage` 里记录过的幻影配对成因。
   */
  const usedTotal = Math.max(0, item.capacity - item.resourcesRemaining);
  const usedPercent = item.capacity > 0 ? Math.min(100, (usedTotal / item.capacity) * 100) : 0;

  return (
    <article
      data-slot="credit-composition-card"
      className="flex w-[240px] shrink-0 snap-start flex-col gap-3 rounded-xl border bg-card/70 p-4 sm:w-[264px]"
      aria-label={`${item.name} 积分构成`}
    >
      <header className="min-w-0">
        <div className="truncate text-[13px] font-medium" title={item.name}>
          {item.name}
        </div>
        {item.secondary && item.secondary !== item.name && (
          <div className="mt-0.5 truncate text-[11px] text-muted-foreground" title={item.secondary}>
            {item.secondary}
          </div>
        )}
      </header>

      {item.state === "loading" ? (
        <div className="flex items-center gap-2 py-6 text-xs text-muted-foreground">
          <Loader2 className="size-3.5 animate-spin" />
          积分查询中…
        </div>
      ) : item.state === "missing" ? (
        <div className="py-6 text-xs text-muted-foreground">等待积分数据…</div>
      ) : item.state === "error" ? (
        <div className="flex min-w-0 items-start gap-2 py-6 text-xs text-destructive">
          <CircleAlert className="mt-0.5 size-3.5 shrink-0" />
          <span className="min-w-0">{item.error || "积分查询失败"}</span>
        </div>
      ) : item.state === "empty" ? (
        <div className="py-6 text-xs text-muted-foreground">
          该账号当前没有积分包，余额为 0。
        </div>
      ) : (
        <>
          <div className="min-w-0">
            <div className="flex items-baseline gap-1.5">
              <Coins className="size-3.5 shrink-0 translate-y-0.5 text-muted-foreground" aria-hidden="true" />
              <span
                className="text-[26px] font-semibold leading-8 tracking-[-0.025em] tabular-nums"
                style={{ fontFamily: '"Bricolage Grotesque Variable", "SF Pro Display", ui-sans-serif, sans-serif' }}
              >
                {formatCredit(item.balance)}
              </span>
              <span className="text-[11px] text-muted-foreground">积分</span>
            </div>
            <div className="mt-1 text-[11px] text-muted-foreground">
              {item.resources.length} 个积分包 · 面额 {formatCredit(item.capacity)}
              {item.capacity > 0 && ` · 已用 ${usedPercent.toFixed(0)}%`}
            </div>
          </div>

          <PackageMixBar segments={item.segments} />

          <div className="flex min-w-0 flex-col gap-1.5">
            {legend.map((segment) => (
              <LegendRow key={segment.key} segment={segment} compact />
            ))}
            {rest.length > 0 && (
              /*
                溢出图例**不**用一个灰色小方块 —— 那个位置在其它行都是「某个包的
                颜色」，放一个假色块会被读成「还有一个灰包」。这里改成把剩余几段的
                真实颜色并排画出来，形状上就明确表示「这里还有好几种颜色」。
              */
              <div className="flex min-w-0 items-center gap-2 text-[11px] text-muted-foreground">
                <span className="flex size-2.5 shrink-0 overflow-hidden rounded-[3px]" aria-hidden="true">
                  {rest.map((segment) => (
                    <span
                      key={segment.key}
                      className="h-full flex-1"
                      style={{ backgroundColor: segment.color }}
                    />
                  ))}
                </span>
                <span className="min-w-0 flex-1 truncate">其余 {rest.length} 个包</span>
                <span className="shrink-0 font-medium tabular-nums">{formatCredit(restRemaining)}</span>
              </div>
            )}
          </div>

          {/*
            底部信息**紧跟图例**，不用 `mt-auto` 把它推到卡片底部。

            为什么：卡片行是等高对齐的（这样各卡的混色条处在同一水平线上，
            横向扫视才能对比），而包数少的账号（如 2 个包）卡片底部会空出一大截。
            若把这一行用 `mt-auto` 钉在底部，那片空白就被**夹在图例与底栏之间** ——
            上下都有边界，看起来像渲染断层而不是留白。让底栏紧跟内容，
            多余空间自然落到卡片边缘，读起来才是「卡片有内边距」。
          */}
          <div className="border-t border-border/60 pt-2 text-[11px] text-muted-foreground">
            {item.soonest === null ? "当前积分长期有效" : `最近到期 ${formatFullDate(item.soonest)}`}
          </div>
        </>
      )}
    </article>
  );
}

/** 单账号明细表：列 = 包名 / 面额 / 剩余 / 已用 / 发放 / 到期。 */
function AccountPackageTable({
  item,
  colorMap,
}: {
  item: AccountComposition;
  colorMap: ReadonlyMap<string, { key: string; color: string; totalAcrossAccounts: number }>;
}) {
  const rows = sortPackageRows(item.resources, colorMap);

  return (
    <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
      <CardHeader className="gap-1 border-b px-4 py-3 sm:px-5">
        <div className="flex min-w-0 flex-wrap items-baseline gap-x-2 gap-y-1">
          <span className="min-w-0 truncate text-[13px] font-medium" title={item.name}>
            {item.name}
          </span>
          {item.secondary && item.secondary !== item.name && (
            <span className="min-w-0 truncate text-[11px] text-muted-foreground">
              {item.secondary}
            </span>
          )}
          <span className="ml-auto shrink-0 text-[11px] text-muted-foreground tabular-nums">
            余额 {formatCredit(item.balance)} · 共 {item.resources.length} 个包
          </span>
        </div>
        <CardDescription className="text-[11px]">
          行首色块与上方混色条同色：同一个包名在任何账号里都是同一个颜色。
          「发放」列后端暂无该字段，一律显示「—」。
        </CardDescription>
      </CardHeader>

      {item.state === "ok" ? (
        <div className="min-w-0 overflow-x-auto">
          <table className="w-full min-w-[620px] text-left text-xs">
            <thead className="bg-muted/45 text-muted-foreground">
              <tr>
                <th scope="col" className="px-4 py-2.5 font-medium sm:px-5">
                  包名 / 来源
                </th>
                <th scope="col" className="px-3 py-2.5 text-right font-medium">
                  面额
                </th>
                <th scope="col" className="px-3 py-2.5 text-right font-medium">
                  剩余
                </th>
                <th scope="col" className="px-3 py-2.5 text-right font-medium">
                  已用
                </th>
                <th scope="col" className="px-3 py-2.5 text-right font-medium">
                  发放
                </th>
                <th scope="col" className="px-4 py-2.5 text-right font-medium sm:px-5">
                  到期
                </th>
              </tr>
            </thead>
            <tbody>
              {rows.map((resource, index) => {
                const key = packageColorKey(resource);
                const color = packageColorOf(colorMap, key);
                const used = usedOf(resource);
                return (
                  <tr
                    key={`${resource.packageCode ?? key}-${resource.expireAt ?? "none"}-${index}`}
                    className="border-t border-border/60"
                  >
                    <td className="max-w-[260px] px-4 py-2.5 sm:px-5">
                      <span className="flex min-w-0 items-center gap-2">
                        <span
                          data-slot="credit-row-swatch"
                          data-package-key={key}
                          className="size-2.5 shrink-0 rounded-[3px] ring-1 ring-inset ring-black/10 dark:ring-white/15"
                          style={{ backgroundColor: color }}
                          aria-hidden="true"
                        />
                        <span className="min-w-0 truncate" title={packageDisplayName(resource)}>
                          {packageDisplayName(resource)}
                        </span>
                      </span>
                    </td>
                    <td className="px-3 py-2.5 text-right tabular-nums">
                      {formatCredit(resource.total)}
                    </td>
                    <td className="px-3 py-2.5 text-right font-medium tabular-nums">
                      {formatCredit(resource.remaining)}
                    </td>
                    <td className="px-3 py-2.5 text-right tabular-nums">
                      {used === null ? "—" : formatCredit(used)}
                    </td>
                    {/*
                      发放时间：后端 `resource_summary` 没有这个字段，也没有任何
                      可推导的来源（expireAt 是抵扣截止，不是发放时刻）。
                      显示「—」并在 title 里说明原因 —— 宁可留白，不编造。
                    */}
                    <td
                      className="px-3 py-2.5 text-right tabular-nums text-muted-foreground"
                      title="后端未提供发放时间字段（resource_summary 无 grantedAt），此处不推测"
                    >
                      —
                    </td>
                    <td className="px-4 py-2.5 text-right sm:px-5">
                      <span className="inline-flex items-center justify-end gap-1.5">
                        {resource.expired ? (
                          <Badge variant="destructive" className="px-1.5 py-0 text-[10px]">
                            已到期
                          </Badge>
                        ) : resource.expiringSoon ? (
                          <Badge variant="warning" className="px-1.5 py-0 text-[10px]">
                            7 天内
                          </Badge>
                        ) : null}
                        <span
                          className={cn(
                            "tabular-nums",
                            resource.expired ? "text-destructive" : "text-muted-foreground",
                          )}
                        >
                          {formatFullDate(resource.expireAt)}
                        </span>
                      </span>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      ) : (
        <div className="px-4 py-6 text-xs text-muted-foreground sm:px-5">
          {item.state === "loading"
            ? "正在加载资源包…"
            : item.state === "missing"
              ? "尚未采集该账号的积分包，点击上方「刷新统计」立即采集。"
              : item.state === "error"
                ? item.error || "积分资源查询失败，点击上方「刷新统计」重试。"
                : "该账号当前没有积分包。若刚领取过权益，点击上方「刷新统计」重新采集。"}
        </div>
      )}
    </Card>
  );
}

export function CreditPackageBreakdown({
  accounts,
  creditMap,
  creditLoadingMap,
  loading = false,
  error = null,
  onRefresh,
  refreshing = false,
}: CreditPackageBreakdownProps) {
  /** 第一阶段：只摊平账号与资源，不涉及颜色。 */
  const views = useMemo(
    () =>
      accounts.map((account) => ({
        account,
        credit: creditMap[account.id],
        loading: creditLoadingMap[account.id] === true,
      })),
    [accounts, creditMap, creditLoadingMap],
  );

  /**
   * 第二阶段：**跨全部账号**一次性构建颜色表。
   *
   * 这是「同源同色」的落点 —— 颜色表只有一份，账号卡、混色条、明细表全部查它。
   * 只要每个账号都查同一张表，同一个包名就不可能显示成两种颜色。
   */
  const colorMap = useMemo(
    () =>
      buildPackageColorMap(
        views.map((view) => (view.credit?.ok ? (view.credit.resources ?? []) : [])),
      ),
    [views],
  );

  /** 第三阶段：用颜色表生成每个账号的构成（含混色条分段）。 */
  const items = useMemo(
    () => views.map((view) => toComposition(view.account, view.credit, view.loading, colorMap)),
    [views, colorMap],
  );

  const hasAnyPackage = items.some((item) => item.state === "ok");
  const anyLoading = items.some((item) => item.state === "loading") || loading;

  return (
    <section className="min-w-0 space-y-2.5" aria-labelledby="credit-composition-title">
      <div className="flex min-w-0 flex-wrap items-baseline justify-between gap-x-3 gap-y-1 px-1">
        <h2 id="credit-composition-title" className="text-[13px] font-medium leading-5">
          积分构成对比
        </h2>
        <p className="text-[11px] text-muted-foreground">
          余额 = 各积分包之和；包按来源发放，面额与到期各不相同 —— 这正是同任务量账号余额却相差上千的原因。
        </p>
      </div>

      {accounts.length === 0 ? (
        <Card className="min-w-0 gap-0 rounded-xl py-0 shadow-none">
          <div className="flex flex-col items-start gap-3 px-4 py-8 sm:px-5">
            <div className="text-sm text-muted-foreground">
              暂无账号，无法对比积分构成。请先到「WorkBuddy 账号」页导入本机账号或 OAuth 登录，
              再回到本页查看。
            </div>
            {onRefresh && (
              <Button size="sm" variant="outline" onClick={onRefresh} disabled={refreshing}>
                {refreshing ? <Loader2 className="animate-spin" /> : <RefreshCw />}
                刷新统计
              </Button>
            )}
          </div>
        </Card>
      ) : !hasAnyPackage ? (
        <Card className="min-w-0 gap-0 rounded-xl py-0 shadow-none">
          <div className="flex flex-col items-start gap-3 px-4 py-8 sm:px-5">
            <div className="text-sm text-muted-foreground">
              {error
                ? `积分包采集失败：${error}`
                : anyLoading
                  ? "正在采集各账号的积分包构成…"
                  : "尚未采集到任何积分包。点击下方按钮立即采集一次，或稍后自动刷新。"}
            </div>
            {onRefresh && !anyLoading && (
              <Button size="sm" variant="outline" onClick={onRefresh} disabled={refreshing}>
                {refreshing ? <Loader2 className="animate-spin" /> : <RefreshCw />}
                刷新统计
              </Button>
            )}
          </div>
        </Card>
      ) : (
        <>
          {/* 横向可滚动卡片行：窄窗口下卡片保持固定宽度、横向滚动，而不是被压扁
              （压扁后混色条的比例就看不出来了，对比也就失去意义）。 */}
          <div className="flex min-w-0 snap-x snap-mandatory gap-3 overflow-x-auto pb-2">
            {items.map((item) => (
              <AccountCompositionCard key={item.account.id} item={item} />
            ))}
          </div>

          <div className="flex min-w-0 flex-col gap-3 pt-1">
            {items.map((item) => (
              <AccountPackageTable key={item.account.id} item={item} colorMap={colorMap} />
            ))}
          </div>
        </>
      )}
    </section>
  );
}
