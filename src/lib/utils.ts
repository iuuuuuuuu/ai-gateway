import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/**
 * 整串**只由不可见字符**组成时的匹配式：空白 / 控制 / 格式 / 代理 / 私有区 /
 * Unicode 标签字符（U+E0000–U+E007F）。
 *
 * 为什么不只用 `trim()`：`String.prototype.trim()` 只剥 ECMAScript 定义的
 * WhiteSpace + LineTerminator，**不含** Unicode 里其它「不可见」类别。
 */
const INVISIBLE_ONLY = /^[\s\p{Cc}\p{Cf}\p{Cs}\p{Co}\u{E0000}-\u{E007F}]*$/u;

/**
 * 该字符串里是否存在**会渲染出可见笔画**的字符。
 *
 * # 为什么需要它（真实缺陷，不是假想）
 *
 * 所有者账号库里有一个号（`acct-c-…`）的上游昵称是**单个 U+E0000**——
 * 一个 Unicode 标签字符（TAG，用于给文本打隐形水印，肉眼不可见）。
 * 而 `"\u{E0000}".trim()` **返回原串**（长度仍是 2），于是
 * 「`note?.trim() || nickname?.trim() || uid.slice(0, 8)`」这条回退链
 * 认定「有名字」，不再退到 uid 前缀。
 *
 * 实测后果：账号池表格里该行名字的渲染宽度是 **0px** —— 账号池、指定账号
 * 勾选列表、用量列表里都是一片空白，用户完全无法识别这是哪个号。
 *
 * # 判据
 *
 * **整串都不可见**才算「没有可见内容」。`"\u{E0000}abc\u{E007F}"` 这种
 * 夹了可见字的串仍算可见 —— TAG 字符常被当隐形水印使用，不该让整串作废。
 * 私有区（`\p{Co}`）与孤立代理（`\p{Cs}`）也归为不可见：昵称若是单个图标
 * 字体码位，退到 uid 前缀反而更能让用户认出是哪个号。
 */
export function hasVisibleText(value: string | null | undefined): boolean {
  if (typeof value !== "string" || value.length === 0) return false;
  return !INVISIBLE_ONLY.test(value);
}

/**
 * 账号显示名的回退链：取第一个**有可见内容**的候选，各自先 `trim()`。
 *
 * 收敛这条链的理由见 `hasVisibleText` —— 只写 `a || b` 会让不可见昵称
 * 冒充成有效名字。全站取名口径都走它，避免各处手写副本再次走偏。
 */
export function firstVisibleText(...candidates: (string | null | undefined)[]): string {
  for (const candidate of candidates) {
    if (typeof candidate !== "string") continue;
    const trimmed = candidate.trim();
    if (hasVisibleText(trimmed)) return trimmed;
  }
  return "";
}
