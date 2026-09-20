/**
 * 产品账号卡片 —— ZCode / Qoder 账号页共用的卡片形态。
 *
 * # 为什么要有这个组件
 *
 * 所有者的要求：「ZCODE和Qoder和Workbuddy采用一样的卡片布局」。
 *
 * 此前 ZCode / Qoder 用的是**表格**（`<table>`），而 WorkBuddy 用卡片
 *（`AccountCard`）。三者在同一个应用里，形态不一致；表格在窄屏下还会
 * 横向溢出。
 *
 * # 为什么抽成共用组件，而不是各写一份
 *
 * 两个产品的账号字段几乎一样（昵称/头像/额度/到期/模型/状态/备注），
 * 差异只在于**产品标识与服务商**。各写一份会让「改一处忘一处」必然发生
 *（这一整轮已经踩过多次：`login_start` 漏传 auth-dir 就是这么来的）。
 *
 * # 与 WorkBuddy 卡片的对应关系
 *
 * 刻意复用同一套视觉语言（见 `account-card.tsx`）：
 *
 *   WorkBuddy            本组件
 *   ─────────────────    ──────────────────────────────
 *   rounded-2xl border   同
 *   shadow-[0_1px_…]     同
 *   header（muted/30）   同，左侧换成本产品图标
 *   size-12 圆头像       同
 *   名字 + 身份行        同
 *   状态 chips           同（Badge 变体与语义保持一致）
 *   底部指标区           同（额度为主指标，与 WorkBuddy 的积分对应）
 *
 * 这样两个产品页与 WorkBuddy 账号页放在一起时，用户不必重新学一遍。
 */
import { Loader2, RefreshCw, Trash2, Pencil, AlertTriangle, CheckCircle2, Clock3, ListChecks, PlayCircle } from "lucide-react";
import { toast } from "sonner";

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
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

/** 卡片要展示的账号数据（两个产品归一到这里）。 */
export interface ProductAccountCardData {
  uid: string;
  nickname: string;
  note: string;
  /** 头像 URL；空 = 回退首字母。 */
  avatarUrl?: string;
  /** 额度的显示文本（已格式化）；空 = 未知。 */
  creditsText?: string;
  /** 到期显示文本。 */
  expiryText?: string;
  /** 到是否紧急（7 天内），用于着色。 */
  expiryUrgent?: boolean;
  /** 该账号可用模型数；undefined = 没查过（与 0 个区分）。 */
  modelCount?: number;
  /** 模型名列表，供悬浮显示。 */
  models?: string[];
  /** 凭证文件是否存在；false = 网关里不可用。 */
  hasCredential: boolean;
  /** 用户手动禁用（不接流量）。 */
  disabled: boolean;
  /** 服务商/区域的展示名（ZCode: 智谱/Z.AI；Qoder: 国服/国际版）。 */
  variantLabel?: string;
  /** 服务商/区域的 Badge 变体。 */
  variantKind?: "default" | "secondary" | "outline" | "destructive" | "success";
  /**
   * 套餐类型徽章（如「体验套餐」）。
   *
   * ZCode 用它区分"新账号赠送的体验额度"与"付费套餐" —— 两者的额度
   * 数字可以长得一样，但**含义完全不同**（体验套餐每日重置且到期作废）。
   */
  planBadge?: string;
  /**
   * 套餐整体到期的说明（如「套餐 09-23 23:59 到期」）。
   *
   * ⚠ 与 `expiryText`（每日周期重置）**不是一回事**，故分开显示。
   * 只显示后者，用户会以为"今天用不完就浪费了"。
   */
  planExpiryText?: string;
  /**
   * 额度**剩余**比例（0~1）；undefined = 没有可用的总量信息（**不显示进度条**）。
   *
   * # 为什么需要（所有者 2026-09-20）
   *
   * 原话：「你设计的 qoder 和 zcode 卡片都很难看，不如 workbuddy 的好看，
   * 完全可以借鉴 workbuddy 账号页面的逻辑，**积分进度条** 任务执行菜单」。
   *
   * WorkBuddy 卡片有进度条，而本组件只有一行数字 —— 用户看不出"还剩多少 /
   * 用掉多少"。数字本身不传达紧迫感，进度条传达。
   *
   * # ⚠ 语义是「剩余」不是「已用」—— 我第一版搞反了
   *
   * WorkBuddy 卡的算法（`account-card.tsx:1153`）是：
   *
   *	ratio = remaining / total      ← **剩余**占比
   *	宽度 = ratio%                  ← 用掉越多，条越短
   *
   * 我第一版按"已用占比"实现（用掉越多条越长），并把颜色按用量阈值
   * （>70% 琥珀 / >90% 红）—— **两处都与 WorkBuddy 相反**。
   * 而所有者要的就是"借鉴 workbuddy"，方向搞反反而更糟：
   * 用户看到条快满了会以为额度快用完，实际那是才用了一点。
   *
   * 故现在与 WorkBuddy **逐字一致**：
   *	· 宽度 = 剩余占比
   *	· 颜色由**是否临近过期**决定（`usageWarn`），而不是由用量决定
   *
   * ⚠ undefined 与 0 必须区分：`undefined` = 上游没给总量（无从计算比例），
   * 此时**不画进度条**；`0` = 确实一点没剩，画一条空的。
   * 把前者画成 0% 会让用户以为"额度耗尽"，那是反向误导。
   */
  usageRatio?: number;
  /** 进度条旁边的文字（如「剩余 1.2M / 3M」）；配合 usageRatio 使用。 */
  usageText?: string;
  /**
   * 额度是否**临近过期/已过期**（决定进度条颜色）。
   *
   * 与 WorkBuddy 同款：它用 `resource.expiringSoon || resource.expired`
   * 决定转橙，而**不是**用"用了多少"。额度的问题几乎总是"马上过期用不完"，
   * 而不是"用太多" —— 后者是好事，不该标红。
   */
  usageWarn?: boolean;
  /**
   * 本账号可执行的任务（渲染成右上角「任务」菜单）。
   *
   * # 为什么任务要挂在**账号卡片**上（所有者 2026-09-20）
   *
   * 原话：「qoder这个活动卡片和账号卡片应该合到一起，应该是 这个账号还有
   * 多少任务没运行，而且也没有跟 workbuddy 有个菜单按钮，点击后进行执行
   * qoder 还支持的任务（因为 qoder 活动是动态的，所以这里支持的任务也是动态的），
   * 而且任务也应该自动执行」。
   *
   * 即：任务不是"页面级的批量操作"，而是**每个账号各自的状态与动作** ——
   * 这个号还有哪些没跑、单独跑一个。故挂在卡片上，与 WorkBuddy 一致。
   *
   * 空数组 = 该账号当前没有可执行任务（不渲染菜单按钮，而不是渲染一个空菜单）。
   */
  tasks?: ProductAccountTask[];
}

/** 账号卡片上的一个可执行任务。 */
export interface ProductAccountTask {
  /** 稳定标识（提交给后端用）。 */
  id: string;
  /** 显示名（如「领取体验套餐」）。 */
  label: string;
  /**
   * 当前状态：决定菜单里显示什么。
   *
   *	done    —— 今天已完成（幂等命中），菜单项置灰并标注"已完成"
   *	ready   —— 可以执行
   *	blocked —— 当前不满足条件（如不在活动期），附 reason
   *
   * ⚠ `done` 的项**仍要显示**（置灰）而不是隐藏：用户需要知道
   * "这个任务存在且今天已经做过了"。隐藏会让他以为功能没了。
   */
  state: "done" | "ready" | "blocked";
  /** state=blocked 时的原因（悬浮可见）。 */
  reason?: string;
}

interface Props {
  data: ProductAccountCardData;
  /** 产品图标的渲染函数（各页传入自己的 Mark）。 */
  mark?: (size: number) => React.ReactNode;
  /** 本卡片正在进行的操作标识；用于置灰与转圈。 */
  busyKey?: string | null;
  /** 操作回调；不传则不渲染对应按钮。 */
  onRefresh?: () => void;
  onEditNote?: () => void;
  onToggleDisabled?: () => void;
  onDelete?: () => void;
  /** 执行某个任务（来自卡片上的任务菜单）。 */
  onRunTask?: (taskId: string) => void;
  /** 紧凑模式（窄列）。 */
  compact?: boolean;
}

export function ProductAccountCard({
  data,
  mark,
  busyKey,
  onRefresh,
  onEditNote,
  onToggleDisabled,
  onDelete,
  onRunTask,
  compact = false,
}: Props) {
  const name = data.nickname || "（未命名）";
  const refreshing = busyKey === "refresh";
  const toggling = busyKey === "toggle";
  const deleting = busyKey === "delete";
  const tasks = data.tasks || [];
  /** 还有几个任务可跑（用于菜单按钮上的计数徽标）。 */
  const runnable = tasks.filter((t) => t.state === "ready").length;

  return (
    <article
      data-slot="product-account-card"
      data-uid={data.uid}
      className={cn(
        "flex h-full min-w-0 flex-col overflow-hidden rounded-2xl border border-border bg-card",
        "shadow-[0_1px_2px_rgba(15,23,42,.025),0_10px_28px_rgba(15,23,42,.035)]",
        "transition-shadow hover:shadow-[0_2px_4px_rgba(15,23,42,.04),0_14px_34px_rgba(15,23,42,.055)]",
      )}
    >
      {/* ── 头部：产品图标 + 头像 + 名字 + 状态 ──
          与 WorkBuddy 卡片同构：底色 muted/30、底部有分隔线。 */}
      <header
        className={cn(
          "relative flex items-center gap-3 border-b border-border bg-muted/30",
          compact ? "min-h-[52px] px-3.5 py-2" : "px-4 py-3",
        )}
      >
        {/* 产品图标置于右上角做水印（同 WorkBuddy 的 WorkBuddyMark） */}
        {mark && (
          <div className="pointer-events-none absolute right-4 top-1/2 -translate-y-1/2 opacity-[0.075]">
            {mark(compact ? 40 : 56)}
          </div>
        )}

        {/* 头像：有 URL 用图，否则首字母回退（同 WorkBuddy） */}
        {data.avatarUrl ? (
          <img
            src={data.avatarUrl}
            alt=""
            className={cn(
              "relative z-10 shrink-0 rounded-full object-cover ring-4 ring-white/65",
              compact ? "size-9" : "size-12",
            )}
            onError={(e) => {
              (e.currentTarget as HTMLImageElement).style.display = "none";
            }}
          />
        ) : (
          <div
            className={cn(
              "relative z-10 flex shrink-0 items-center justify-center rounded-full bg-muted font-semibold text-muted-foreground ring-4 ring-white/65",
              compact ? "size-9 text-sm" : "size-12 text-base",
            )}
          >
            {name.charAt(0).toUpperCase()}
          </div>
        )}

        <div className="relative z-10 min-w-0 flex-1">
          <h3 className={cn("truncate font-semibold", compact ? "text-[13px] leading-5" : "text-sm leading-5")} title={name}>
            {name}
          </h3>
          <p className="mt-0.5 truncate font-mono text-xs leading-5 text-muted-foreground" title={data.uid}>
            {data.uid}
          </p>
        </div>
      </header>

      {/* ── 状态 chips 行 ── */}
      <div className="flex flex-wrap items-center gap-1.5 border-b border-border/60 px-4 py-2">
        {data.variantLabel && (
          <Badge variant={data.variantKind ?? "secondary"}>{data.variantLabel}</Badge>
        )}

        {/* 套餐类型 —— 「体验套餐」必须显眼。
            体验额度**每日重置且到期作废**，与付费套餐的"余额"含义不同；
            不标出来的话，用户会把 800 万当成攒着的存款。

            ⚠ 这里**不用 Tooltip 包裹**：TooltipTrigger 的 asChild 会把自己的
            `data-slot="tooltip-trigger"` 覆盖到徽章上，令徽章失去
            `data-slot="badge"` —— 测试与样式选择器都会因此找不到它
           （实测踩到过）。详细说明见下面 planExpiryText 那一行。 */}
        {data.planBadge && (
          <Badge
            variant="outline"
            className="gap-1 border-amber-500/40 text-amber-700 dark:text-amber-400"
            title="体验套餐：新账号赠送的额度，每日重置，且到期后未用完的部分会失效（不是被用完了）"
          >
            <Clock3 className="size-3" />
            {data.planBadge}
          </Badge>
        )}

        {!data.hasCredential ? (
          <Tooltip>
            <TooltipTrigger asChild>
              <Badge variant="destructive">凭证缺失</Badge>
            </TooltipTrigger>
            <TooltipContent>
              凭证文件不存在，该账号在网关里已不可用，需重新导入
            </TooltipContent>
          </Tooltip>
        ) : data.disabled ? (
          <Tooltip>
            <TooltipTrigger asChild>
              <Badge variant="outline">已停用</Badge>
            </TooltipTrigger>
            <TooltipContent>不参与网关选号；凭证与额度信息仍然保留</TooltipContent>
          </Tooltip>
        ) : (
          <Badge variant="secondary">正常</Badge>
        )}

        {data.note && (
          <Badge variant="outline" className="chip-note max-w-[12rem] gap-1" title={`备注：${data.note}`}>
            <span className="truncate">{data.note}</span>
          </Badge>
        )}
      </div>

      {/* ── 指标区：额度（主）+ 到期 + 模型 ──
          与 WorkBuddy 卡片同构：主指标用大号字、副信息用 muted 小字。 */}
      <section className="flex min-w-0 flex-1 flex-col px-4 py-3">
        <div className="flex items-baseline gap-x-3 gap-y-1">
          <span className="flex items-center gap-1.5">
            <strong
              className={cn("font-semibold leading-none tabular-nums tracking-[-0.025em]", compact ? "text-[20px]" : "text-[22px]")}
              style={{ fontFamily: '"Bricolage Grotesque Variable", "SF Pro Display", ui-sans-serif, sans-serif' }}
            >
              {data.creditsText ?? "未知"}
            </strong>
          </span>
          <div className="ml-auto flex items-center gap-1.5 text-xs text-muted-foreground">
            <Clock3 className="size-3.5 shrink-0" />
            <span className={cn(data.expiryUrgent && "text-amber-600")}>{data.expiryText ?? "到期未知"}</span>
          </div>
        </div>

        {/* 支持模型：没查过（undefined）与"查过但 0 个"是两回事，措辞要分开 */}
        <div className="mt-2 text-xs text-muted-foreground">
          {data.models && data.models.length > 0 ? (
            <Tooltip>
              <TooltipTrigger asChild>
                <span className="cursor-help underline decoration-dotted">
                  支持 {data.models.length} 个模型
                </span>
              </TooltipTrigger>
              <TooltipContent className="max-w-sm">
                <div className="font-medium">可用模型</div>
                <div className="mt-1 font-mono text-xs">{data.models.join("、")}</div>
              </TooltipContent>
            </Tooltip>
          ) : (
            <span title="点「刷新」可查询该账号实际可用的模型">支持模型：未查询</span>
          )}
        </div>

        {/* 套餐整体到期 —— 与上面那行的"每日周期重置"是**两件事**。
            分两行写，并各自标清含义，避免用户把"明天重置"误读成"明天没了"。 */}
        {data.planExpiryText && (
          <div className="mt-1 text-xs text-amber-700 dark:text-amber-400">{data.planExpiryText}</div>
        )}

        {/*
          额度进度条 —— 借鉴 WorkBuddy 账号卡（所有者要求「积分进度条」）。

          ⚠ 只在 usageRatio 有值时才渲染：undefined 表示"上游没给总量"，
          此时画成 0% 会让用户以为额度耗尽（反向误导）。

          ⚠ 语义是**剩余**占比（与 WorkBuddy 逐字一致）：用掉越多，条越短。
          颜色由"是否临近过期"决定，不是由用量 —— 额度的问题是
          "马上过期用不完"，而不是"用太多"。
        */}
        {typeof data.usageRatio === "number" && (
          <div className="mt-2 space-y-1" data-slot="product-usage-bar">
            <div className="h-1 overflow-hidden rounded-full bg-muted">
              <div
                data-slot="product-usage-fill"
                // 宽度 = 剩余比例（钳到 0~100%：上游偶尔给 >1 的比例，
                // 容量刚变更时会出现，不钳会让进度条溢出容器）。
                style={{ width: `${Math.min(100, Math.max(0, data.usageRatio * 100))}%` }}
                className={cn(
                  "h-full rounded-full transition-all",
                  // 与 WorkBuddy 同款：临近过期/已过期转橙，否则主色。
                  data.usageWarn ? "bg-orange-500" : "bg-primary",
                )}
              />
            </div>
            {data.usageText && (
              <div className="text-[11px] leading-4 text-muted-foreground">{data.usageText}</div>
            )}
          </div>
        )}
      </section>

      {/* ── 底部操作栏（同 WorkBuddy 卡片：右对齐） ── */}
      <footer className="flex items-center justify-end gap-1 border-t border-border/60 px-3 py-2">
        {/*
          任务菜单 —— 与 WorkBuddy 账号卡同款交互（所有者要求
          「也没有跟 workbuddy 有个菜单按钮，点击后进行执行 qoder 还支持的任务」）。

          ⚠ 只在确实有任务时渲染：`tasks` 为空说明该账号当前没有可执行任务，
          渲染一个空的"任务"按钮会让用户以为点了没反应。
        */}
        {tasks.length > 0 && onRunTask && (
          <DropdownMenu>
            <Tooltip>
              <TooltipTrigger asChild>
                <DropdownMenuTrigger asChild>
                  <Button
                    variant="outline"
                    size="sm"
                    data-slot="product-task-menu"
                    aria-label={`任务：${name}`}
                    className="h-7 gap-1 px-2 text-[11px]"
                  >
                    <ListChecks className="h-3.5 w-3.5" />
                    任务
                    {/* 还有几个可跑 —— 一眼看出"这个号还有事没做" */}
                    {runnable > 0 && (
                      <Badge
                        variant="secondary"
                        className="h-4 min-w-4 justify-center px-1 text-[9.5px] font-normal"
                      >
                        {runnable}
                      </Badge>
                    )}
                  </Button>
                </DropdownMenuTrigger>
              </TooltipTrigger>
              <TooltipContent>
                {runnable > 0
                  ? `还有 ${runnable} 个任务可执行`
                  : "本账号今日任务都已完成"}
              </TooltipContent>
            </Tooltip>
            <DropdownMenuContent align="end" className="w-56">
              <DropdownMenuLabel className="text-xs">
                {data.nickname || "本账号"}的任务
              </DropdownMenuLabel>
              <DropdownMenuSeparator />
              {tasks.map((t) => (
                <DropdownMenuItem
                  key={t.id}
                  data-slot="product-task-item"
                  data-task={t.id}
                  data-state={t.state}
                  // 已完成 / 不可用都置灰但**仍然显示**：用户需要知道
                  // "这个任务存在且今天已经做过了"。隐藏会让他以为功能没了。
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
                </DropdownMenuItem>
              ))}
            </DropdownMenuContent>
          </DropdownMenu>
        )}
        {onRefresh && (
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                variant="ghost"
                size="icon"
                disabled={refreshing}
                // 图标按钮**必须**有可访问名：Tooltip 内容要悬停才进 DOM，
                // 屏幕阅读器与自动化测试都拿不到。
                aria-label={`刷新额度与模型：${name}`}
                onClick={onRefresh}
              >
                <RefreshCw className={cn("h-4 w-4", refreshing && "animate-spin")} />
              </Button>
            </TooltipTrigger>
            <TooltipContent>刷新额度、到期与支持模型</TooltipContent>
          </Tooltip>
        )}
        {onEditNote && (
          <Tooltip>
            <TooltipTrigger asChild>
              <Button variant="ghost" size="icon" aria-label={`编辑备注：${name}`} onClick={onEditNote}>
                <Pencil className="h-4 w-4" />
              </Button>
            </TooltipTrigger>
            <TooltipContent>编辑备注</TooltipContent>
          </Tooltip>
        )}
        {onToggleDisabled && (
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                variant="ghost"
                size="icon"
                disabled={toggling}
                aria-label={`${data.disabled ? "恢复参与路由" : "停止接流量"}：${name}`}
                onClick={onToggleDisabled}
              >
                {toggling ? (
                  <Loader2 className="h-4 w-4 animate-spin" />
                ) : data.disabled ? (
                  <CheckCircle2 className="h-4 w-4" />
                ) : (
                  <AlertTriangle className="h-4 w-4" />
                )}
              </Button>
            </TooltipTrigger>
            <TooltipContent>{data.disabled ? "恢复参与路由" : "停止接流量"}</TooltipContent>
          </Tooltip>
        )}
        {onDelete && (
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                variant="ghost"
                size="icon"
                disabled={deleting}
                aria-label={`删除账号：${name}`}
                className="text-destructive hover:text-destructive"
                onClick={onDelete}
              >
                <Trash2 className="h-4 w-4" />
              </Button>
            </TooltipTrigger>
            <TooltipContent>删除账号</TooltipContent>
          </Tooltip>
        )}
      </footer>
    </article>
  );
}

/** 统一的失效提示：卡片列表为空时显示。 */
export function ProductAccountEmpty({ product }: { product: string }) {
  return (
    <div className="rounded-2xl border border-dashed border-border px-6 py-10 text-center text-sm text-muted-foreground">
      还没有 {product} 账号。用上方的「从客户端导入」或「登录」添加。
    </div>
  );
}

/** 供页面用的网格容器（与 WorkBuddy 账号页同一套栅格）。 */
export function ProductAccountGrid({ children }: { children: React.ReactNode }) {
  return (
    <div className="grid auto-rows-fr grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-3">{children}</div>
  );
}

/** 便于页面统一提示（避免各页各写一套 toast 文案）。 */
export function toastOpFailed(e: unknown) {
  toast.error(e instanceof Error ? e.message : String(e));
}
