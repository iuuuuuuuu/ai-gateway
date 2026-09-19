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
import { Loader2, RefreshCw, Trash2, Pencil, AlertTriangle, CheckCircle2, Clock3 } from "lucide-react";
import { toast } from "sonner";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
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
  compact = false,
}: Props) {
  const name = data.nickname || "（未命名）";
  const refreshing = busyKey === "refresh";
  const toggling = busyKey === "toggle";
  const deleting = busyKey === "delete";

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
      </section>

      {/* ── 底部操作栏（同 WorkBuddy 卡片：右对齐） ── */}
      <footer className="flex items-center justify-end gap-1 border-t border-border/60 px-3 py-2">
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
