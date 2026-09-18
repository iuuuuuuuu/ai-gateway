import type { InputHTMLAttributes } from "react";

/**
 * 「大小写敏感的机器串」输入框必须带的一组属性。
 *
 * ## 为什么需要它（真实故障形态）
 *
 * 移动端浏览器与部分桌面浏览器/输入法会对文本输入框做**自动首字母大写**与
 * **自动纠错**：API Key 这种随机串一旦被擅自改掉一个字符的大小写，用户是
 * **看不出来**的（随机串没有"拼写"可言，眼睛无法察觉 a 变成了 A）。表现
 * 就是所有者反馈的「明明粘贴对了却 401」—— 而且因为看不到，用户会反复
 * 重贴，永远查不出原因。
 *
 * 这组属性把三件事分别关掉，缺一不可：
 *   - `autoCapitalize="none"` —— 关「句首/词首自动大写」（iOS Safari、部分安卓输入法）
 *   - `autoCorrect="off"`     —— 关「输入法自动纠错/自动更正」
 *   - `spellCheck={false}`    —— 关拼写检查（会画红波浪线并有替换建议，误点即改值）
 * 另外 `autoComplete="off"` 关掉浏览器的自动填充下拉：那同样会**静默覆盖**
 * 用户已经填好的值，属于同一类「值被悄悄改掉」的故障。
 *
 * ## 判断标准（哪些输入框该用、哪些**不**该用）
 *
 * **该用**——值本身是大小写敏感的机器串，不存在"用户想让它变大写"的语义：
 *   API Key / token / JWT / sessionid / sid_guard / ttwid / 账号 uid（机器标识）。
 *
 * **不该用**——内容是人类语言、或含有专有名词/人名/品牌名：
 *   账号备注、显示名、昵称、系统提示词。对这些输入框关掉自动大写会**让移动端
 *   更难用**：用户写中文/英文句子时要自己按 shift，句首大写也没了；而它们本来
 *   就允许任意大小写，改错了用户一眼能看出来。它们最多只值得 `spellCheck={false}`
 *   （见 `src/components/account-card.tsx` 的账号备注输入框），**绝不**套用本常量。
 *
 * 数字/端口类输入框也不该套用：那边该管的是键盘类型（`type="number"` /
 * `inputMode="numeric"`），自动大写对数字键位本来就不生效。
 *
 * 用法：`<Input {...SENSITIVE_STRING_INPUT_PROPS} />`。属性是**透传**到底层
 * `<input>` 的（见 `src/components/ui/input.tsx` 的 `{...props}`），因此不必
 * 改动共享组件 —— 那是全站复用的，动它影响面远大于在调用处传参。
 */
export const SENSITIVE_STRING_INPUT_PROPS: Pick<
  InputHTMLAttributes<HTMLInputElement>,
  "autoCapitalize" | "autoCorrect" | "spellCheck" | "autoComplete"
> = {
  autoCapitalize: "none",
  autoCorrect: "off",
  spellCheck: false,
  autoComplete: "off",
};
