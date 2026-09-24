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
import { Loader2, RefreshCw, Trash2, Pencil, AlertTriangle, CheckCircle2, Clock3, Ellipsis, Info, PlayCircle, ScrollText } from "lucide-react";
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
  /**
   * 大数字的单位说明（默认「额度」）。
   *
   * 所有者反馈过「看不懂」—— 一个光秃秃的数字不说明它是积分、token
   * 还是次数。各产品传自己的口径（如 ZCode 是 token，Qoder 是 Credits）。
   */
  creditsLabel?: string;
  /**
   * 时钟图标旁那行文字的**语义**，决定 tooltip 怎么解释它。
   *
   * 为什么需要它：同一个位置在两个产品上含义**不同** ——
   *
   *	`quota-cycle`（ZCode）该时刻额度桶会**重置**，是"还剩多久"
   *	`last-updated`（Qoder）数据**上次刷新的时刻**，是"有多新"
   *
   * Qoder 改成后者是所有者要求的（2026-09-21）：
   *
   *	「qoder那个改成上次更新时间吧,你那个过期时间根本不准确」
   *
   * 实测印证了他的判断：该账号 `expire_at = 0`、`nextResetAt` 已过期
   * 68 天，任何"到期"推算都不准；而 `lastSeenAt` 是我们自己写的时间。
   *
   * 缺省按 `quota-cycle`（ZCode 的既有行为不变）。
   */
  expiryKind?: "quota-cycle" | "last-updated";
  /** 到期/更新时间显示文本。 */
  expiryText?: string;
  /** 是否需要注意（过期/陈旧），用于着色。 */
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
   * **按模型拆开**的额度明细（ZCode 专用，2026-09-21 所有者要求）。
   *
   * # 为什么需要它
   *
   * 所有者原话：
   *
   * > 「那八百万额度，是 glm5.3flash 五百万，三百万 glm5.3，zcode 账号
   * >   flash 模型额度我用完了，你优化一下显示，那里应该拆分成两个模型的
   * >   额度，而不是一个的」
   *
   * 上游把额度按**模型**分桶，而 `creditsText` 显示的是它们的**求和**。
   * 求和会把「某个模型已用完」掩盖掉 —— 他遇到的正是这种情况：
   * Flash 用光了，卡片上却还剩 300 万（那是 GLM-5.3 的），看不出问题。
   *
   * 每一项渲染成「模型名 · 剩余/总量」一行。
   * 空数组/undefined = 没有明细 ⇒ 不渲染这个区块（回退到只显示求和值）。
   */
  usageBuckets?: Array<{
    /** 模型名（上游 `show_name`）。 */
    name: string;
    remaining: number;
    total: number;
  }>;  /**
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
   *	detail  —— **只有详情、没有可领的东西**（上游 `VIEW_DETAILS`）。
   *	          置灰显示并标注"仅详情"。
   *
   * ⚠ `done` / `blocked` / `detail` 的项**仍要显示**（置灰）而不是隐藏：
   * 用户需要知道"这个任务存在且今天已经做过了"。隐藏会让他以为功能没了。
   *
   * # ⚠ `detail` 是 2026-09-23 补的状态（所有者报的真实缺陷）
   *
   * 原实现把这类活动**整条滤掉**，于是当某账号只有 `VIEW_DETAILS` 活动时
   * `tasks` 为空 ⇒ 卡片里 `tasks.length > 0` 不成立 ⇒ **整个「本账号任务」
   * 菜单组都不渲染**。
   *
   * 所有者现场（Qoder 国际版）：菜单里只有「刷新/记录/备注/停用/删除」，
   * 他说「qoder国际版账号怎么还是没有本账号任务?明明是有的」——
   * 而那条活动**确实存在**（实测 `showCampaign:true`、活动进行中）。
   *
   * 讽刺的是上面那句"隐藏会让他以为功能没了"的注释**早就写着**，
   * 但过滤发生在更上游，把 `tasks` 变成空数组，等于绕过了这条设计。
   * 故补一个显式状态，而不是继续用"过滤掉"来表达"不可领"。
   */
  state: "done" | "ready" | "blocked" | "detail";
  /** state=blocked / detail 时的原因（悬浮可见）。 */
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
  /**
   * 「记录」按钮：打开任务执行记录 + 额度消耗明细。
   *
   * 所有者 2026-09-20：「zcode和qoder都无法查看任务执行记录,和积分消耗明细」。
   *
   * ⚠ 做成**可选回调**而不是内置开关：卡片不该知道记录数据从哪来
   *（那是各页面的职责），且三个产品（WorkBuddy/Qoder/ZCode）的凭证
   * 与账号库都不同。传了这个 prop 才渲染按钮。
   */
  onViewRecords?: () => void;
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
  onViewRecords,
  compact = false,
}: Props) {
  const name = data.nickname || "（未命名）";
  const refreshing = busyKey === "refresh";
  const toggling = busyKey === "toggle";
  const deleting = busyKey === "delete";
  const tasks = data.tasks || [];
  /** 还有几个任务可跑（用于菜单按钮上的计数徽标）。 */
  const runnable = tasks.filter((t) => t.state === "ready").length;
  /**
   * 菜单组标题里的状态后缀。
   *
   * ⚠ 不能一律写「今日已完成」：`detail`（仅详情）与 `blocked`（不可用）
   * 都**不是**"已完成"。写错会让用户以为今天领过了 —— 而其实没有可领的。
   * 实测现场（Qoder 国际版）：唯一那条活动是 `VIEW_DETAILS`，
   * 若标题写「今日已完成」，用户会误以为已经领到东西了。
   *
   * ⚠ 也别对 `detail` 写「暂无任务」：那条活动**是存在的**（实测
   * `showCampaign:true`、活动期内），只是它没有可领的东西。
   * 写"暂无任务"会与下面那行「仅详情」自相矛盾。
   */
  const taskGroupSuffix =
    runnable > 0
      ? `（还有 ${runnable} 个可执行）`
      : tasks.some((t) => t.state === "done")
        ? "（今日已完成）"
        : tasks.some((t) => t.state === "detail")
          ? "（无可领取项）"
          : "（当前不可执行）";

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
        {/* 产品图标置于右上角做水印（同 WorkBuddy 的 WorkBuddyMark）。
            ⚠ 2026-09-22 两次调整，最终取值见下：

            # 第一次：`opacity-[0.075]` 直接套在自绘 mark 上
            自绘的 `QoderMark` 是「渐变实心方块 + 白色图形」，实心底色
            占满整个面积 ⇒ 0.075 叠出来仍是**一整块可见色斑**，
            像"来历不明的图标"（所有者反馈：「有水印不知道是从哪里来的icon」）。

            # 第二次：换成官方 app icon 后重新定标
            官方图标是「圆角底 + 品牌图形」（与 WorkBuddy 官方图标同构），
            整块都是实色 ⇒ 比自绘 mark 更"重"。实测 `0.075` 会明显压过
            账号名，故取 **0.05** 并叠 `saturate-0` 去色：
            让它是"一层淡淡的印记"，而不是一个看不清的按钮。

            ⚠ 右侧留出 `⋯` 按钮的位置（约 48px），否则水印会被按钮压住；
            这也是此处不用 `right-4` 的原因。 */}
        {mark && (
          <div
            className={cn(
              "pointer-events-none absolute top-1/2 -translate-y-1/2 opacity-[0.05] saturate-0",
              compact ? "right-12" : "right-14",
            )}
            aria-hidden
          >
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

        {/* ── 右上角操作菜单（2026-09-22 改）──
            与 WorkBuddy 账号卡片统一：一个 `⋯` 按钮，点开是所有操作。
            此前是底部一排图标按钮，所有者要求改成与 WorkBuddy 一致的形态
            （原话：「操作按钮都还在下面,应该跟workbuddy一样保持统一,
            在右上角有个操作按钮,点击下拉出来操作菜单才对」）。

            ⚠ 位置用 `absolute` 而非 flex 子项：头部高度随 compact／宽松
            两档变化，绝对定位才能让菜单在两种档位下都贴住右上角，
            且不会把「名字 + uid」挤窄。

            ⚠ 菜单**始终渲染**（即使个别操作不可用）：WorkBuddy 卡片也是
            如此 —— 有的账号没有可跑任务，但"刷新/记录/备注/停用/删除"
            仍然在，藏掉整个入口会让用户以为功能没了。 */}
        <div className="absolute right-2 top-2 z-20">
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button
                variant="ghost"
                size="icon"
                className={cn("rounded-lg text-muted-foreground hover:text-foreground", compact ? "size-7" : "size-8")}
                aria-label={`管理账号 ${name}`}
                title="更多账号操作"
              >
                <Ellipsis />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" className="w-60">
              {/* 任务组：只在有任务时出现（无任务时整组隐藏，
                  而不是显示一个空的"任务"标题） */}
              {tasks.length > 0 && onRunTask && (
                <>
                  <DropdownMenuLabel className="text-xs">
                    本账号任务{taskGroupSuffix}
                  </DropdownMenuLabel>
                  {tasks.map((t) => (
                    <DropdownMenuItem
                      key={t.id}
                      data-slot="product-task-item"
                      data-task={t.id}
                      data-state={t.state}
                      // 已完成 / 不可用 / 仅详情都置灰但**仍然显示**：
                      // 用户需要知道"这个任务存在"以及"它为什么不能点"。
                      // 隐藏会让他以为功能没了（所有者的真实反馈）。
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
                        // 「仅详情」用 Info 而不是三角警告：它不是"出问题了"，
                        // 只是这条活动本身没有可领的东西。
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

              {onRefresh && (
                <DropdownMenuItem
                  disabled={refreshing}
                  onSelect={() => onRefresh()}
                  className="gap-2 text-xs"
                >
                  <RefreshCw className={cn("h-3.5 w-3.5", refreshing && "animate-spin")} />
                  刷新额度与模型
                </DropdownMenuItem>
              )}

              {/* 「记录」入口 —— 任务执行记录 + 积分/额度消耗明细。
                  所有者 2026-09-20：「zcode和qoder都无法查看任务执行记录,
                  和积分消耗明细,都一起修复了」。

                  ⚠ 用 `onViewRecords` 可选 prop 而不是内置开关：
                  卡片本身不该知道"记录数据从哪来"（那是各页面的职责）。
                  `data-slot` 保留 —— 既有测试靠它定位这个入口。 */}
              {onViewRecords && (
                <DropdownMenuItem
                  data-slot="product-records-open"
                  onSelect={() => onViewRecords()}
                  className="gap-2 text-xs"
                >
                  <ScrollText className="h-3.5 w-3.5" />
                  任务记录 · 消耗明细
                </DropdownMenuItem>
              )}

              {onEditNote && (
                <DropdownMenuItem onSelect={() => onEditNote()} className="gap-2 text-xs">
                  <Pencil className="h-3.5 w-3.5" />
                  编辑备注
                </DropdownMenuItem>
              )}

              {onToggleDisabled && (
                <DropdownMenuItem
                  disabled={toggling}
                  onSelect={() => onToggleDisabled()}
                  className="gap-2 text-xs"
                >
                  {toggling ? (
                    <Loader2 className="h-3.5 w-3.5 animate-spin" />
                  ) : data.disabled ? (
                    <CheckCircle2 className="h-3.5 w-3.5" />
                  ) : (
                    <AlertTriangle className="h-3.5 w-3.5" />
                  )}
                  {data.disabled ? "恢复参与路由" : "停止接流量"}
                </DropdownMenuItem>
              )}

              {onDelete && (
                <>
                  <DropdownMenuSeparator />
                  <DropdownMenuItem
                    disabled={deleting}
                    onSelect={() => onDelete()}
                    className="gap-2 text-xs text-destructive focus:bg-destructive/5 focus:text-destructive"
                  >
                    {deleting ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Trash2 className="h-3.5 w-3.5" />}
                    删除账号
                  </DropdownMenuItem>
                </>
              )}
            </DropdownMenuContent>
          </DropdownMenu>
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
          <span className="flex items-baseline gap-1.5">
            {/* 给大数字一个**单位说明** —— 否则用户不知道这个数是积分、
                token 还是次数（所有者反馈过「看不懂」）。 */}
            <span className="text-[11px] leading-4 text-muted-foreground">
              {data.creditsLabel ?? "额度"}
            </span>
            <strong
              className={cn("font-semibold leading-none tabular-nums tracking-[-0.025em]", compact ? "text-[20px]" : "text-[22px]")}
              style={{ fontFamily: '"Bricolage Grotesque Variable", "SF Pro Display", ui-sans-serif, sans-serif' }}
            >
              {data.creditsText ?? "未知"}
            </strong>
          </span>
          {/*
            「额度周期 / 上次更新」—— 两种语义共用这一处，措辞按 `expiryKind` 分。

            ZCode 有两个完全不同的时间：额度桶的每日重置点，与套餐整体到期。
            此前这里只写「0 天后（日期）」，所有者反馈「看不懂，是 token
            到期时间吗」—— 因为「0 天后」听起来像"快没了"，而实际含义是
            "今晚重置、明天还有"，**方向完全相反**。

            Qoder 那边则连一个可靠的时间都没有（实测 `expire_at = 0`、
            `nextResetAt` 早已过期），故按所有者要求改显示**上次更新时间**
            —— 那是我们自己写入的真实时刻，不会过期。
          */}
          <Tooltip>
            <TooltipTrigger asChild>
              <div className="ml-auto flex cursor-help items-center gap-1.5 text-xs text-muted-foreground">
                <Clock3 className="size-3.5 shrink-0" />
                <span className={cn(data.expiryUrgent && "text-amber-600")}>
                  {data.expiryText ?? "额度周期未知"}
                </span>
              </div>
            </TooltipTrigger>
            <TooltipContent className="max-w-xs">
              {data.expiryKind === "last-updated" ? (
                <>
                  <div className="font-medium">数据更新时间</div>
                  <div className="mt-1 text-xs">
                    这是**上次刷新该账号数据**的时刻，不是额度到期时间。
                    点卡片上的「刷新」可重新采集。
                  </div>
                </>
              ) : (
                <>
                  <div className="font-medium">额度周期</div>
                  <div className="mt-1 text-xs">
                    每日额度会在该时刻**重置**，不是"额度到期作废"。
                    套餐整体到期另有标注（见下方橙色文字）。
                  </div>
                </>
              )}
            </TooltipContent>
          </Tooltip>
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
        {/*
          按模型拆开的额度明细（2026-09-21 所有者要求）。

          # 为什么放在进度条**下面**而不是替代它

          进度条答的是「整体还剩多少」，明细答的是「哪个模型快没了」——
          两个问题都要回答，缺一个就会误判：

            · 只有进度条 ⇒ 看到还剩 300 万，以为没事，
              而实际 Flash 已经用光（正是所有者遇到的情况）
            · 只有明细 ⇒ 看不到整体比例

          # 每行显示「模型 · 剩余/总量」

          ⚠ 剩余**已耗尽时标橙**（remaining <= 0）：那是这个区块存在的
          全部意义 —— 让"某个模型用完了"一眼可见，而不是被别的模型的
          剩余量掩盖。

          ⚠ 只在**有明细**时渲染：老账号记录里没有 `quotaEntries`
          （字段是本次新增的），此时不渲染，回退到只显示求和值 ——
          不会显示成空或 0。
        */}
        {data.usageBuckets && data.usageBuckets.length > 0 && (
          <div className="mt-2 space-y-1.5" data-slot="product-usage-buckets">
            {data.usageBuckets.map((b) => {
              const exhausted = b.remaining <= 0;
              /*
               * 每个模型**自己**的剩余比例。
               *
               * 所有者原话：
               *
               *   「上面有一个总量，下面是区分开的，下面的也要有进度条，
               *     不然不知道有多少 用多少」
               *
               * 只给数字看不出"还剩几成" —— 尤其额度是百万量级时，
               * `0 / 5,000,000` 与 `12,345 / 67,890` 哪个更紧张，
               * 光看数字要心算。进度条一眼可见。
               *
               * ⚠ 与顶部总条同款：传的是**剩余**占比（用掉越多条越短），
               * 且只在 total > 0 时画（total 为 0 表示上游没给容量，
               * 画成 0% 会被误读成"已耗尽"）。
               */
              const ratio =
                b.total > 0 ? Math.min(1, Math.max(0, b.remaining / b.total)) : undefined;
              return (
                <div
                  key={b.name}
                  data-slot="product-usage-bucket"
                  data-bucket-name={b.name}
                  data-bucket-exhausted={exhausted ? "true" : undefined}
                  className="space-y-1"
                >
                  <div className="flex items-baseline gap-2 text-[11px] leading-4">
                    <span className="truncate font-medium text-muted-foreground">{b.name}</span>
                    <span
                      className={cn(
                        "ml-auto shrink-0 tabular-nums",
                        // 耗尽 ⇒ 橙色：这是"某模型用完"的唯一视觉信号
                        exhausted ? "font-medium text-orange-600" : "text-muted-foreground",
                      )}
                    >
                      {b.remaining.toLocaleString()} / {b.total.toLocaleString()}
                    </span>
                  </div>
                  {typeof ratio === "number" && (
                    <div
                      className="h-1 overflow-hidden rounded-full bg-muted"
                      data-slot="product-usage-bucket-bar"
                    >
                      <div
                        data-slot="product-usage-bucket-fill"
                        style={{ width: `${ratio * 100}%` }}
                        className={cn(
                          "h-full rounded-full transition-all",
                          // 耗尽用橙色；其余用主色的**浅色变体** ——
                          // 与顶部总条区分开，避免"两条一样重"抢注意力。
                          exhausted ? "bg-orange-500" : "bg-primary/60",
                        )}
                      />
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        )}
      </section>

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
