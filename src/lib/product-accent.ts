/**
 * **平台**配色（WorkBuddy / Qoder / ZCode）。
 *
 * # 所有者的反馈
 *
 *	「不同平台应该用不同的颜色表示,现在都看不清」
 *
 * ## 此前的问题（实测确认）
 *
 * 三个平台的 chip 共用同一套样式：
 *
 *	点亮：border-primary/40 bg-primary/10 text-primary
 *
 * 于是 `WorkBuddy`、`Qoder`、`ZCode` 全是**同一种淡绿**，只有文字不同 ——
 * 截图上看确实分不出来。加上字号只有 9px，基本读不清。
 *
 * ## 与 client-accent.ts 的关系
 *
 * 那两个是**正交的维度**，刻意分成两个文件：
 *
 *	client-accent.ts   智能体**客户端**（Claude Code / Codex / DSH …）
 *	                   —— "这是哪个客户端"
 *	product-accent.ts  **平台**（WorkBuddy / Qoder / ZCode）
 *	                   —— "这个模型走哪个平台"
 *
 * 一个客户端可以走任意平台，两者没有从属关系。混在一个映射里会让
 * "加了新平台要不要动客户端配色"这种问题变得含糊。
 *
 * ## 色相选择
 *
 * 与既有视觉语言保持一致，且三者拉得开：
 *
 *	WorkBuddy → 绿（emerald）—— 默认主产品，用主色系
 *	Qoder     → 紫（violet）
 *	ZCode     → 青（teal）
 */

/** 一个平台的配色。 */
export interface ProductAccent {
  /** 文字色（亮/暗两套）。 */
  text: string;
  /** 底色。 */
  bg: string;
  /** 边框色。 */
  border: string;
  /** 熄灭态（不允许走这个平台）的样式。 */
  off: string;
}

export const PRODUCT_ACCENTS: Record<string, ProductAccent> = {
  workbuddy: {
    text: "text-emerald-700 dark:text-emerald-300",
    bg: "bg-emerald-500/15",
    border: "border-emerald-500/50",
    off: "border-border/50 bg-muted/20 text-muted-foreground/50",
  },
  qoder: {
    text: "text-violet-700 dark:text-violet-300",
    bg: "bg-violet-500/15",
    border: "border-violet-500/50",
    off: "border-border/50 bg-muted/20 text-muted-foreground/50",
  },
  zcode: {
    text: "text-teal-700 dark:text-teal-300",
    bg: "bg-teal-500/15",
    border: "border-teal-500/50",
    off: "border-border/50 bg-muted/20 text-muted-foreground/50",
  },
};

/**
 * 未收录平台的兜底配色（中性灰）。
 *
 * 用固定的中性色而**不是**随机色：随机色会让同一个平台每次刷新换个颜色，
 * 反而更难认（与 client-accent 同一取舍）。
 */
export const DEFAULT_PRODUCT_ACCENT: ProductAccent = {
  text: "text-slate-700 dark:text-slate-300",
  bg: "bg-slate-500/15",
  border: "border-slate-500/50",
  off: "border-border/50 bg-muted/20 text-muted-foreground/50",
};

/** 取某个平台的配色。 */
export function productAccentOf(product: string): ProductAccent {
  return PRODUCT_ACCENTS[product] ?? DEFAULT_PRODUCT_ACCENT;
}

/** 平台的中文展示名（未收录时原样返回，不编造）。 */
export const PRODUCT_LABELS: Record<string, string> = {
  workbuddy: "WorkBuddy",
  qoder: "Qoder",
  zcode: "ZCode",
};
