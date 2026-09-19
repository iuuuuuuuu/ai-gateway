/**
 * 智能体客户端的**配色**（每个客户端一个颜色，便于一眼区分）。
 *
 * # 所有者的要求
 *
 * 「智能体管理,不同的客户端要采用不同的颜色加以区分」。
 *
 * 此前 12 个客户端的卡片除了图标外**外观完全一样**，扫一眼分不出谁是谁；
 * 已接入的还会统一变成绿色边框（`border-emerald-500/40`），
 * 于是**所有卡片看起来都是同一个颜色**。
 *
 * # 设计取舍
 *
 * ## 每个客户端一个**色相**，不是一个相似色
 *
 * 用户的目的是"区分"，所以色相要**拉得开**（红/橙/琥珀/黄绿/绿/青/蓝/靛/紫/粉…）。
 * 用深浅不同的同一个蓝，等于没区分。
 *
 * ## 用 Tailwind 的 **CSS 变量类名**，不用内联十六进制
 *
 * 项目用 shadcn + Rhea 主题令牌，深色模式靠 CSS 变量切换。
 * 内联 `#3b82f6` 在深色模式下会刺眼，而 `text-blue-600 dark:text-blue-400`
 * 是项目既有的做法（见 account-card 的 chip 用法）。
 *
 * 故这里给**成对的亮/暗类名**，由组件拼进 `cn()`。
 *
 * ## 颜色**不承载语义**
 *
 * 唯一的例外是"已接入"状态 —— 那由**边框/背景**表达（既有做法），
 * 而色相只表达"这是哪个客户端"。两者是正交的维度，
 * 不能把"已接入"也编码进色相（否则用户无法同时看出
 * "这是 Codex" 与 "它已接入"）。
 */

/** 一个客户端的配色。 */
export interface ClientAccent {
  /** 文字色（图标/badge 用），含暗色模式。 */
  text: string;
  /** 淡背景。 */
  bg: string;
  /** 边框。 */
  border: string;
  /** 图标徽章底色（更实的色块）。 */
  chip: string;
}

/**
 * 客户端 → 配色。
 *
 * 键与 `CLIENT_METAS` / `CLIENT_ICONS` 一致（都用客户端 id）。
 * 未收录的客户端回落到 `DEFAULT_ACCENT`（中性灰），而不是随机色 ——
 * 随机色会让同一个客户端每次刷新换个颜色，反而更难认。
 */
export const CLIENT_ACCENTS: Record<string, ClientAccent> = {
  // Anthropic 系用暖色（橙/琥珀）—— 与其他系明显区分
  "claude-code": {
    text: "text-orange-600 dark:text-orange-400",
    bg: "bg-orange-500/10",
    border: "border-orange-500/30",
    chip: "bg-orange-500/15",
  },
  "claude-desktop": {
    text: "text-amber-600 dark:text-amber-400",
    bg: "bg-amber-500/10",
    border: "border-amber-500/30",
    chip: "bg-amber-500/15",
  },

  // OpenAI 系用冷绿/青
  codex: {
    text: "text-emerald-600 dark:text-emerald-400",
    bg: "bg-emerald-500/10",
    border: "border-emerald-500/30",
    chip: "bg-emerald-500/15",
  },
  dsh: {
    text: "text-cyan-600 dark:text-cyan-400",
    bg: "bg-cyan-500/10",
    border: "border-cyan-500/30",
    chip: "bg-cyan-500/15",
  },

  // 其余各给一个拉得开的色相
  opencode: {
    text: "text-violet-600 dark:text-violet-400",
    bg: "bg-violet-500/10",
    border: "border-violet-500/30",
    chip: "bg-violet-500/15",
  },
  pi: {
    text: "text-blue-600 dark:text-blue-400",
    bg: "bg-blue-500/10",
    border: "border-blue-500/30",
    chip: "bg-blue-500/15",
  },
  "grok-build": {
    text: "text-sky-600 dark:text-sky-400",
    bg: "bg-sky-500/10",
    border: "border-sky-500/30",
    chip: "bg-sky-500/15",
  },
  zcode: {
    text: "text-teal-600 dark:text-teal-400",
    bg: "bg-teal-500/10",
    border: "border-teal-500/30",
    chip: "bg-teal-500/15",
  },
  "kimi-code": {
    text: "text-emerald-700 dark:text-emerald-300",
    bg: "bg-emerald-600/10",
    border: "border-emerald-600/30",
    chip: "bg-emerald-600/15",
  },
  openclaw: {
    text: "text-red-600 dark:text-red-400",
    bg: "bg-red-500/10",
    border: "border-red-500/30",
    chip: "bg-red-500/15",
  },
  hermes: {
    text: "text-purple-600 dark:text-purple-400",
    bg: "bg-purple-500/10",
    border: "border-purple-500/30",
    chip: "bg-purple-500/15",
  },
  "minimax-code": {
    text: "text-pink-600 dark:text-pink-400",
    bg: "bg-pink-500/10",
    border: "border-pink-500/30",
    chip: "bg-pink-500/15",
  },
  // 未来若新增客户端，键名与 CLIENT_METAS 保持一致即可；
  // 未收录的会回落到中性灰（不随机），见 accentOf。
};

/** 未收录客户端的配色（中性灰）。 */
export const DEFAULT_ACCENT: ClientAccent = {
  text: "text-slate-600 dark:text-slate-400",
  bg: "bg-slate-500/10",
  border: "border-slate-500/30",
  chip: "bg-slate-500/15",
};

/** 取某个客户端的配色（未收录时回落到中性灰，不随机）。 */
export function accentOf(clientId: string): ClientAccent {
  return CLIENT_ACCENTS[clientId] ?? DEFAULT_ACCENT;
}
