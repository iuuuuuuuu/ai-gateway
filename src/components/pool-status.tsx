import { cn } from "@/lib/utils";

/**
 * 账号池的状态展示件：行首 3px 状态色条 + 冷却实时倒计时 + 原因灰字小行。
 *
 * 为什么单独成文件而不是写进 `GatewayPage.tsx`：该页正被并行修改，把「新增展示」
 * 放进独立文件，可以让本轮改动与「表格排序/筛选逻辑」解耦，两处改动互不踩踏。
 *
 * ---------------------------------------------------------------------------
 * 字段核实（写代码前逐条读过后端源码，**不是猜的**）
 * ---------------------------------------------------------------------------
 *
 * 数据源是网关 `/status` 的 `pool.accounts[]`，对应 Go 的 `pool.Status` 结构
 * （`go-gateway/internal/pool/pool.go`）。**不是** `src/lib/api.ts:1232` 的
 * `TraeAccountMeta.cooldown` —— 那是另一个产品页（Trae）的结构，字段名相近
 * （也有 `type` / `until` / `reason` / `error_count`）但语义与单位都不同：
 * 那边 `type` 是 `PlanLimit` / `SoftRate` / `SessionDead` 这类 Rust 错误分类，
 * `until` 是 **Unix 秒**。两套东西绝不能混用，混了倒计时会差 1000 倍。
 *
 * 本文件用到的池侧字段：
 *
 * 1. `cool_kind?: string`（宽松 string，前端未做联合类型收窄）
 *    全部可能取值由 Go `CoolKind.String()`（pool.go:46-54）与
 *    `statusOf()`（pool.go:2355-2360）决定，只有四个：
 *      · `"hard_credit"` —— 余额（积分）不足，冷却到次日 04:00 等签到恢复；
 *      · `"soft_rate"`   —— 429 限流，短冷却（`cooldown.soft_rate`，默认 60s）；
 *      · `"breaker"`     —— 连续失败触发的**指数退避熔断**（`pool.breaker_cooldown`
 *                           默认 30m 起，`breaker_cooldown_max` 封顶 6h）。
 *                           ⚠ 这是**后端真实下发**的值（`st.CoolKind = "breaker"`），
 *                           不是「宿主侧的说法」—— 页面里旧注释写成宿主侧表述是错的。
 *      · `"unknown"`     —— `CoolKind` 零值（理论上不该出现在冷却态）。
 *    另外**未冷却时该字段因 `omitempty` 整个不下发**，前端拿到的是 `undefined`。
 *
 * 2. `until` / `breaker_until` —— **ISO 8601 字符串**（Go `time.Time`），
 *    **不是**毫秒也不是秒级时间戳。
 *    ⚠ 陷阱：`json:"until,omitempty"` 对**结构体无效**（Go 的 omitempty 不认
 *    结构体零值），所以未设置时会下发 `"0001-01-01T00:00:00Z"`。必须把它当
 *    「没有这个截止」处理，否则会算出「还剩 2000 年」。本文件 `parseGoTime`
 *    用年份 <= 1 判定，与同页 `poolLastUsedAgo` 的既有口径一致。
 *    ⚠ 另一处：`until` 在 `src/lib/types.ts` 的 `GatewayPoolAccount` 上**尚未声明**
 *    （只有 `breaker_until`）。该文件正被并行修改，故这里用局部类型描述需求
 *    （见 `PoolStatusFields`），不去动 types.ts。
 *
 * 3. `cool_remaining_sec?: number` —— **秒**，且后端已处理过正负与取整：
 *    `int64(time.Until(deadline).Seconds() + 0.999)`，并夹到 `>= 0`（pool.go:2351）。
 *
 * ---------------------------------------------------------------------------
 * 为什么倒计时不用 `cool_remaining_sec`
 * ---------------------------------------------------------------------------
 *
 * 它是**上次轮询那一刻的快照**，而本页 `/status` 是 5 秒一轮 —— 直接拿它渲染，
 * 倒计时每 5 秒才「跳」一次，看起来像卡住了。两个 ISO 截止是**绝对时刻**，
 * 配合每秒 tick 就是真正的实时倒计时。
 *
 * 取值口径与 Go `recoveryAt()` **逐字一致**：两个截止里「**较晚且在将来**」的那个。
 * 不能随便挑一个 —— 熔断（breaker_until）与即时冷却（until）是**正交**的，
 * 熔断触发时 until 可能早已归零，取错会显示成「提前恢复」（后端注释 pool.go:2338-2349
 * 记录的正是这个历史缺陷）。
 */

/**
 * 本文件需要的最小字段集合（**局部类型**）。
 *
 * 为什么不直接给 `GatewayPoolAccount` 加 `until`：`src/lib/types.ts` 正被并行修改，
 * 跨文件加字段会把两处改动耦合成一个编译单元。这里只声明「我需要什么」，
 * `GatewayPoolAccount` 因为共有 8 个同名可选字段，可结构化赋值进来（无需断言）。
 */
export interface PoolStatusFields {
  cooling?: boolean;
  cool_kind?: string;
  cool_remaining_sec?: number;
  /** 即时冷却截止（ISO 8601）；types.ts 尚未声明，见文件头说明。 */
  until?: string;
  breaker_until?: string;
  breaker_fails?: number;
  disabled?: boolean;
  no_route?: boolean;
  reason?: string;
}

/** Go `time.Time` 零值（`0001-01-01T00:00:00Z`）的年份。 */
const GO_ZERO_TIME_YEAR = 1;

/**
 * 解析 Go 下发的 ISO 时间串；**零值、空值、非法值一律返回 null**（= 没有这个截止）。
 *
 * 必须单独判零值：Go 的 `omitempty` 对结构体不生效，未设置的 `time.Time`
 * 会老老实实下发成 `0001-01-01T00:00:00Z`，直接 `new Date()` 会得到一个
 * 合法但荒谬的时刻。
 */
function parseGoTime(raw?: string): number | null {
  if (!raw) return null;
  const t = new Date(raw);
  if (Number.isNaN(t.getTime())) return null;
  if (t.getUTCFullYear() <= GO_ZERO_TIME_YEAR) return null;
  return t.getTime();
}

/**
 * 账号「何时恢复可选」——与 Go `recoveryAt()` 同口径：两个生效截止中较晚的那个。
 *
 * 只统计**在将来**的截止：已过期的截止不参与（Go 侧同样要求 `now.Before(deadline)`）。
 * 两个都过期或都没设置 → null（= 此刻不在冷却）。
 */
function poolRecoveryAt(acc: PoolStatusFields, nowMs: number): number | null {
  let deadline: number | null = null;

  const until = parseGoTime(acc.until);
  if (until !== null && until > nowMs) deadline = until;

  const breaker = parseGoTime(acc.breaker_until);
  if (breaker !== null && breaker > nowMs) {
    // 较晚者才是真正恢复的时刻（healthy() 要求两个截止都过期）。
    if (deadline === null || breaker > deadline) deadline = breaker;
  }

  return deadline;
}

/**
 * 倒计时的三种结果。
 *
 * 为什么要区分 `expired` 与 `unknown`（而不是一律返回 `number | null`）：
 * 两者在界面上该说的话**完全不同**。`expired` = 我们确实拿到了截止时刻，
 * 而它已经过去（通常意味着刚到期、`/status` 还没轮到下一轮 5 秒刷新）→
 * 可以如实说「已恢复」；`unknown` = 压根没拿到可用的截止信息 → 什么都**不该说**，
 * 说「已恢复」是在替后端断言一个我们并不知道的事实。
 */
export type PoolCoolState =
  | { kind: "remaining"; sec: number }
  | { kind: "expired" }
  | { kind: "unknown" };

/** 冷却倒计时状态（口径见 `PoolCoolState`）。 */
export function poolCoolState(acc: PoolStatusFields, nowMs: number): PoolCoolState {
  const deadline = poolRecoveryAt(acc, nowMs);
  if (deadline !== null) {
    // 已知截止且仍在将来 → 有明确剩余（`Math.ceil` 保证不会给 0）。
    return { kind: "remaining", sec: Math.max(1, Math.ceil((deadline - nowMs) / 1000)) };
  }
  // 有截止但已过去 → 确实到期了。注意 `parseGoTime` 已把 Go 零值滤成 null，
  // 所以走到这里的一定是「真实存在过的截止」，不是「从未设置」。
  const hasUntil = parseGoTime(acc.until) !== null;
  const hasBreaker = parseGoTime(acc.breaker_until) !== null;
  if (hasUntil || hasBreaker) return { kind: "expired" };
  // 没有任何绝对截止：退回后端快照秒数（它同样是「已知」的信息）。
  if (typeof acc.cool_remaining_sec === "number" && acc.cool_remaining_sec > 0) {
    return { kind: "remaining", sec: acc.cool_remaining_sec };
  }
  return { kind: "unknown" };
}

/**
 * 剩余秒数 → 中文时长。
 *
 * 与 `GatewayPage.tsx` 的 `formatRemaining` **同一口径**（一分钟以内给秒、
 * 一小时以内给分+秒），刻意各写一份而不是互相 import：那两个页面文件正被并行
 * 修改，跨文件引用会把两处改动绑在一起。同页 `creditFormatter` 的注释记录过
 * 同一个取舍。
 *
 * 为什么不按需求示例直接写成「剩余 X 分钟」：本页模型冷却是 42 秒 / 65 秒这种
 * 量级，一律向上取整成「分钟」会让用户系统性高估等待时间（`formatRemaining`
 * 上方的长注释记录过这个实测缺陷）。这里保持既有精度，不引入第二套时间说法。
 */
function formatCoolRemaining(sec: number): string {
  const total = Math.ceil(sec);
  if (total < 60) return `${total} 秒`;
  if (total < 3600) {
    const m = Math.floor(total / 60);
    const s = total % 60;
    return s === 0 ? `${m} 分钟` : `${m} 分 ${s} 秒`;
  }
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  return minutes > 0 ? `${hours} 小时 ${minutes} 分钟` : `${hours} 小时`;
}

/**
 * 冷却倒计时叶子组件（渲染成 ` · 12 分钟`）。
 *
 * 只在 `acc.cooling` 为真时挂载，因此不会有闲置的计时开销。
 *
 * `now` 由调用方从**页面已有的每秒 tick** 传入（`GatewayPage` 的 `nowTick`），
 * 本组件自己**不建定时器**。为什么这样更好：
 *  · 账号池通常十几行、可能同时多行冷却；每行各建一个 `setInterval` 就是
 *    十几个定时器各触发一次重渲染，纯属浪费；
 *  · 而该页本来就有一个每秒 tick 驱动「上次更新 x 秒前」，整张表随页面状态
 *    每秒重渲染一次 —— 复用它等于**零额外开销**就拿到了实时倒计时。
 * 用外部传入而不是内部自建，也让本组件成为纯函数，便于单独断言。
 */
export function PoolCoolCountdown({ acc, now }: { acc: PoolStatusFields; now: number }) {
  const state = poolCoolState(acc, now);
  // 已过期**绝不显示负数**。说「已恢复」而不是留空：截止确实存在且已过去，
  // 通常意味着刚到期、`/status` 还没轮到下一轮刷新（5 秒一轮），
  // 此刻账号已经是可用的了。
  if (state.kind === "expired") return <> · 已恢复</>;
  // 完全拿不到截止信息：**什么都不说**。这里若也写「已恢复」，
  // 就是在替后端断言一个我们并不知道的事实（字段可能只是没下发）。
  if (state.kind === "unknown") return null;
  return <> · {formatCoolRemaining(state.sec)}</>;
}

/** 行首色条的语义档位（**只有三档**，与需求的枚举逐条对齐）。 */
export type PoolBarTone = "ok" | "cool" | "off";

/**
 * 色条档位。判定顺序与状态列标签**逐条对齐**（`poolStateRank` 同源）：
 * 不接流量 / 已停用 → 红；冷却 → 琥珀；否则绿（正常可用）。
 *
 * **为什么刻意不区分「在途」**（不给正在接流量的行另设一种颜色）：
 * 色条要回答的唯一问题是「这个号现在能不能用」，好让用户**扫一眼就看出池子
 * 有没有问题**。若给在途行再插一种颜色，扫视时就会同时出现三种「没问题」的
 * 颜色（绿 + 在途色），反而需要逐行辨认 —— 与「一眼扫完」的目标相反。
 * 「在途」是**另一个维度**（活跃度），已由既有的行底色、状态列的「使用中 N」
 * 脉冲徽标承担，信息一点没少。
 *
 * ⚠ 已知副作用（如实记录）：`.pool-row-inuse > td:first-child` 那条既有的
 * 3px 蓝竖条会被本组件**覆盖**（本元素绝对定位、绘制在其上）。这是「色条统一
 * 表达可用性」的必然结果 —— 同一块 3px 位置无法同时表达两个维度。
 * 在途信号仍由**整行淡蓝底**与脉冲徽标承载，且行底色在横向滚动时同样可见。
 * 若所有者更希望保留蓝竖条，把 `inUse` 重新加进判定即可（一行改动）。
 */
export function poolBarTone(acc: PoolStatusFields): PoolBarTone {
  if (acc.no_route || acc.disabled) return "off";
  if (acc.cooling) return "cool";
  return "ok";
}

/**
 * 色条颜色：**全部走主题变量**，不硬编码调色板值。
 *
 * 三个 token 与三个档位是**语义一一对应**的，不是随便挑的
 *（token 的用途说明都在 `src/index.css` 里，逐条对得上）：
 *  · `ok`   → `--primary`     品牌绿（亮/暗各 `oklch(0.725/0.696 … 163)`）；
 *  · `cool` → `--warning`     该 token 的既有用途原文就是「账号池的模型冷却角标、
 *                             冷却态标签」—— 正是这一档；
 *  · `off`  → `--destructive` 已注册的语义色，亮/暗各有取值（`bg-destructive`
 *                             在构建产物里确实存在）。
 *
 * **为什么不用 `bg-emerald-500` / `bg-amber-500` 这类调色板值**（需求原文的示例写法）：
 * 它们是固定色，不随主题走。而本项目的 token 恰恰为此存在 —— `index.css` 的注释
 * 明确记录过「此前这些地方散着写 amber-500/600 的 Tailwind 调色板值，主题切换时
 * 无法统一调整，也与 Rhea token 体系脱节」，后来才收敛成 `--warning`。
 * 沿用调色板值等于把那次收敛又倒退回去。
 *
 * **为什么用 `bg-[color:var(--…)]` 而不是 `bg-warning`**：`--warning` 只在
 * `:root` / `.dark` 里定义，**没有**在 `@theme inline` 里注册成 `--color-*`，
 * 因此 Tailwind 生成不出 `bg-warning`（已用构建产物核对：`.bg-warning` 与
 * `--color-warning` 均不存在，而 `.bg-destructive` / `.bg-primary` 存在 ——
 * 所以那两个直接用工具类）。本项目已有 `h-[var(--radix-select-trigger-height)]`
 * 这种任意值写法，模式一致。
 *
 * 暗色可辨性逐条核对（暗色卡片底 `--card: oklch(0.279 0.041 260.031)`）：
 * 三个 token 的暗色取值明度均在 0.696~0.704，是深底上的**亮色**，对比充足；
 * 亮色卡片底为纯白，三个取值的明度在 0.577~0.725，同样可辨。
 */
const POOL_BAR_CLASS: Record<PoolBarTone, string> = {
  // `bg-[color:var(--…)]` 里的 `color:` 是 Tailwind 的**类型提示**：
  // `bg-` 前缀既可能是 background-color 也可能是 background-image，
  // 而 `var()` 自身无法被推断出类型（`h-[var(--radix-…)]` 那种能推断是因为
  // `h-` 只有一种含义）。显式写 `color:` 消除歧义，不依赖推断的默认值。
  //
  // 这两种任意值写法都已用 `vite build` 的产物确认编译成了真实规则
  //（`.bg-\[color\:var\(--warning\)\]{background-color:var(--warning)}`）。
  ok: "bg-primary",
  cool: "bg-[color:var(--warning)]",
  off: "bg-destructive",
};

/**
 * 行首 3px 状态色条。
 *
 * **零额外列宽**：绝对定位铺在「展开」单元格内部（该单元格加 `relative`），
 * 不参与布局，也不会把行内容右移 —— 与 `.pool-row-inuse` 用 `box-shadow: inset`
 * 而不用 `border-left` 是同一个理由。
 */
export function PoolStatusBar({ acc }: { acc: PoolStatusFields }) {
  const tone = poolBarTone(acc);
  return (
    <span
      aria-hidden="true"
      data-slot="pool-status-bar"
      data-tone={tone}
      className={cn("pointer-events-none absolute inset-y-0 left-0 w-[3px]", POOL_BAR_CLASS[tone])}
    />
  );
}

/**
 * 状态列下方的灰字小行：后端下发的 `reason` **原文**。
 *
 * 只在真有原因时渲染（返回 null 不占位）—— 每行都补一条空白既浪费行高，
 * 又会让「有原因的账号」不再突出。
 *
 * ⚠ **必须按状态门控，不能「有 reason 就显示」**（这是本函数最容易写错的地方）：
 * Go 侧的 `e.reason` 只在两处被清空 —— `ReviveDisabled()`（手动复活）与
 * `reviveCoolingLocked()`（签到解冻，pool.go:1990-1995）。而冷却**自然到期**
 * （`until` 过期）时 `reason` **不会被清**，`NoteSuccess()` 也不清它
 *（pool.go:2027-2042 只清 fails/retryCount/breakerUntil）。
 * 于是「几分钟前限流、现在早已恢复」的健康账号仍然带着 `reason = "429 rate limit"`。
 * 若无脑显示，用户会在一个绿条、显示「健康」的账号下面读到「429 rate limit」，
 * 合理推断成「它现在正被限流」—— 那是**假警报**，比不显示更糟。
 * 因此只在账号**确实处于该 reason 所解释的状态**（冷却 / 已停用 / 不接流量）时才显示。
 *
 * 刻意**不翻译、不加工、也不编兜底句**：`reason` 是网关按真实失败原因写下的原文
 *（如 `429 rate limit` / `余额不足`），上方徽标已有分类短标签，这里要的正是
 * 「原始依据」，好让用户能直接拿去搜索或对日志。
 *
 * 为什么不给「没有 reason」的账号补一句说明（如「已手动设为不接流量」）：
 *  · 那句话与上方徽标的 `state.label`（「不接流量」/「已停用」）**语义重复**，
 *    同一格里说两遍等于白占一行；
 *  · 更糟的是它会让这一行的含义变得**不可信** —— 用户无法分辨「这是网关写的原文」
 *    还是「界面自己补的话」。本行的全部价值就在于它是原文。
 * 完整解释仍由徽标的悬浮提示（`coolReasonText` / `queuedReasonText`）承担。
 *
 * `trim()` 挡掉纯空白：网关理论上可能写入空白串，那与「没有原因」等价。
 */
export function poolReasonLine(acc: PoolStatusFields): string | null {
  const inExplainedState = Boolean(acc.no_route || acc.disabled || acc.cooling);
  if (!inExplainedState) return null;
  return acc.reason?.trim() || null;
}
