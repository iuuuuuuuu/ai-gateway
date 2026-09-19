/**
 * 「积分包构成」可视化的**纯逻辑层**（无 React、无副作用、无任何导入）。
 *
 * 为什么单独成文件，而不是直接写在组件里：
 *
 *  1. 「同源同色」是本功能的硬要求 —— 同一个包名在所有账号卡、混色条、明细表里
 *     必须是同一个颜色。取色一旦依赖渲染顺序、数组下标或账号 id，跨账号对比就
 *     立刻失效（两个账号的同一个包会显示成两种颜色，用户读不出「差别只在包里」）。
 *     把取色抽成不依赖 React 的纯函数，就能用 node 直接跑断言验证，而不是靠肉眼。
 *
 *  2. 堆叠条必须做除零 / 负数 / 超界钳制（`total` 为 0 时不能出 NaN，
 *     `remaining` 为负或大于 `total` 时不能画出超界宽度）。这些也全是纯计算。
 *
 * 本文件刻意**不导入任何模块**：这样它能被单独编译成 JS 后用 node 跑校验脚本，
 * 校验「同名同色」「不产生 NaN / 不超界」这两条硬约束。
 */

/* -------------------------------------------------------------------------- */
/* 结构类型：局部声明，不依赖 `@/lib/types`                                       */
/* -------------------------------------------------------------------------- */

/**
 * 组件需要的积分包字段子集。
 *
 * 刻意**不**从 `@/lib/types` 导入 `CreditResource`：共享类型文件同时有其他改动
 * 在进行中，这里只按结构（structural typing）描述自己用到的字段，
 * `CreditResource` 天然可赋值过来，互不影响。
 */
export interface PackageResourceView {
  packageCode: string | null;
  packageName: string | null;
  total: number;
  remaining: number;
  /** 后端已算好的「已用」；老后端可能缺失，用 `usedOf` 兜底。 */
  used: number;
  expireAt: number | null;
  expired: boolean;
  expiringSoon: boolean;
}

/** 账号展示所需的字段子集（`AccountMeta` 可赋值过来）。 */
export interface PackageAccountView {
  id: string;
  uid?: string | null;
  nickname?: string | null;
  note?: string | null;
}

/** 账号积分资源查询结果（`CreditExpiry` 可赋值过来）。 */
export interface PackageCreditView {
  ok: boolean;
  error?: string | null;
  resources?: readonly PackageResourceView[] | null;
  /**
   * 后端聚合的账号余额（各包 `remaining` 之和）。
   *
   * 为什么优先用它而不是前端再求和一次：账号卡片（`account-card.tsx`）显示的就是
   * 这个字段，本区块若自己算，遇到「上游返回的包列表不完整」时两处会显示不同余额 ——
   * 同一个账号在一页里两个数字是最不可解释的偏差。
   */
  totalRemaining?: number | null;
}

/* -------------------------------------------------------------------------- */
/* 字段核实结论（写代码前逐个核对过，不是猜的）                                     */
/* -------------------------------------------------------------------------- */

/**
 * 真有 —— `crates/ai-gateway-core/src/modules/credits.rs::resource_summary`
 * 输出的 JSON 就是这几个键（对应 `src/lib/types.ts` 的 `CreditResource`）：
 *
 *   · `packageName` / `packageCode` —— 上游 `PackageName` / `PackageCode`
 *   · `total`     面额（上游 `CycleCapacitySize*` 系列，多个候选键依次回退）
 *   · `remaining` 剩余
 *   · `used`      已用 —— **上游给了就用上游的**；没给时是**后端**按
 *                 `total - remaining` 兜底的（credits.rs:216-218）。
 *                 所以前端直接用 `used`，不必（也不应该）再自己算一遍。
 *   · `expireAt`  到期（上游 `DeductionEndTime` / `ExpiredTime` / `CycleEndTime`）
 *
 * **后端没有** —— 前端一律显示「—」，不编造：
 *
 *   · **发放时间**（`grantedAt` / `createdAt` / `StartTime`）
 *     `resource_summary` 的输出里根本没有这个键；上游只在**请求体**里用过
 *     `SlicePeriodStartTime`（credits.rs:409），那是「查哪个时间窗的免费包」的
 *     查询参数，不是某个包的发放时刻。Go 侧 `resourcePackage` 结构体
 *     （`go-gateway/internal/upstream/client.go:1576`）同样没有该字段。
 *     因此明细表的「发放」列恒为「—」：**不用 expireAt 倒推、不拿当前时间顶替**。
 *   · 包级「已用」的独立数据源：只有 `used`，且与 `total - remaining` 同源。
 *
 * 待后端补充的字段（见交付报告）：`grantedAt`（发放时刻，Unix 毫秒）。
 */

/* -------------------------------------------------------------------------- */
/* 取色：同源同色                                                                */
/* -------------------------------------------------------------------------- */

/**
 * 调色板：复用既有的 `--data-series-*` 主题 token（亮/暗色各一套，见
 * `src/index.css`）。不新增 token、不引入新依赖，暗色主题自动跟随。
 *
 * 顺序**刻意打散相邻色相**：混色条里相邻两段若是近似色（如 emerald 与 teal、
 * indigo 与 sky），边界一眼看不出来，堆叠条就白画了。这里的排列保证相邻两段
 * 总是来自不同色相家族（绿→紫→琥珀→天蓝→玫红→青柠→靛→青）。
 */
export const PACKAGE_COLOR_PALETTE = [
  "var(--data-series-emerald)",
  "var(--data-series-violet)",
  "var(--data-series-amber)",
  "var(--data-series-sky)",
  "var(--data-series-rose)",
  "var(--data-series-lime)",
  "var(--data-series-indigo)",
  "var(--data-series-teal)",
] as const;

/** 既没有包名也没有包码时的兜底键（见 `packageColorKey` 的说明）。 */
export const UNNAMED_PACKAGE_KEY = "\u0000未命名资源包";

/**
 * 取色键 = **包名**（缺包名时退包码）。
 *
 * 为什么必须是包名而不是下标 / 账号 id：
 *   「国内运营裂变包」在 A 账号是第 3 个包、在 B 账号是第 1 个包，用下标取色就会
 *   两边不同色，跨账号对比直接失效。用包名做键，同一个来源在任何账号、任何
 *   排序下都必然落到同一个颜色。
 *
 * 包名与包码都没有时（防御分支，实测后端 `merge_resources` 总会有其一）：
 * 全部落到同一个 `UNNAMED_PACKAGE_KEY` —— 宁可几个匿名包同色，也不引入
 * 「按下标取色」这种会破坏同源同色的规则。
 */
export function packageColorKey(resource: PackageResourceView): string {
  const name = (resource.packageName ?? "").trim();
  if (name) return name;
  const code = (resource.packageCode ?? "").trim();
  if (code) return code;
  return UNNAMED_PACKAGE_KEY;
}

/** 展示用包名（与取色键同源，保证图例 / 明细表 / 混色条说的是同一个包）。 */
export function packageDisplayName(resource: PackageResourceView): string {
  const name = (resource.packageName ?? "").trim();
  if (name) return name;
  const code = (resource.packageCode ?? "").trim();
  if (code) return code;
  return "未命名资源包";
}

export interface PackageColorEntry {
  key: string;
  color: string;
  /** 该包名在所有账号里的**面额合计**，只用于决定取色顺序。 */
  totalAcrossAccounts: number;
}

/**
 * 由**全部账号的全部积分包**一次性构建「包名 → 颜色」映射。
 *
 * 排序规则（与参考实现一致）：按包名在**所有账号的面额合计**降序取色，
 * 合计相同时按包名字典序打破平局 —— 保证结果是确定的，与账号顺序、
 * 数组下标、渲染次数都无关。
 *
 * 为什么用**面额**（`total`）而不是剩余量排序：剩余量随消耗不断下降，
 * 用它排序会让「哪个包是哪种颜色」在余额变化后整体重排，用户刚记住的
 * 「紫色那个是裂变包」下一秒就变了。面额在包的整个生命周期里不变。
 *
 * 包数超过调色板长度时按 `index % 长度` 循环 —— 会有两个包共用一色。
 * 这是可接受的降级：图例与明细表**始终把包名写在色块旁边**，颜色只是
 * 帮助扫视的辅助线索，不承担唯一标识的职责。
 */
export function buildPackageColorMap(
  resourcesByAccount: readonly (readonly PackageResourceView[])[],
): Map<string, PackageColorEntry> {
  const totals = new Map<string, number>();
  for (const resources of resourcesByAccount) {
    for (const resource of resources) {
      const key = packageColorKey(resource);
      totals.set(key, (totals.get(key) ?? 0) + safeCredit(resource.total));
    }
  }

  const ordered = [...totals.entries()].sort(([leftKey, leftTotal], [rightKey, rightTotal]) =>
    rightTotal === leftTotal ? leftKey.localeCompare(rightKey) : rightTotal - leftTotal,
  );

  const map = new Map<string, PackageColorEntry>();
  ordered.forEach(([key, totalAcrossAccounts], index) => {
    map.set(key, {
      key,
      color: PACKAGE_COLOR_PALETTE[index % PACKAGE_COLOR_PALETTE.length],
      totalAcrossAccounts,
    });
  });
  return map;
}

/**
 * 取某个包名的颜色。
 *
 * 正常路径只查表。兜底分支（表里没有这个键）走**由包名派生的稳定哈希**，
 * 而不是任何形式的序号 —— 这样即使走到兜底分支，「同名必然同色」仍然成立
 * （哈希只依赖包名本身）。
 */
export function packageColorOf(
  colorMap: ReadonlyMap<string, PackageColorEntry>,
  key: string,
): string {
  const entry = colorMap.get(key);
  if (entry) return entry.color;
  return PACKAGE_COLOR_PALETTE[hashKey(key) % PACKAGE_COLOR_PALETTE.length];
}

/** 由字符串派生的稳定哈希（与 `account-card.tsx::avatarTone` 同一套写法）。 */
function hashKey(key: string): number {
  let hash = 0;
  for (let index = 0; index < key.length; index += 1) {
    hash = (hash * 31 + key.charCodeAt(index)) >>> 0;
  }
  return hash;
}

/* -------------------------------------------------------------------------- */
/* 数值钳制：除零 / 负数 / 超界                                                   */
/* -------------------------------------------------------------------------- */

/** 把任意输入收敛成「有限的非负数」；NaN / Infinity / undefined / 负数一律变 0。 */
export function safeCredit(value: number | null | undefined): number {
  if (value === null || value === undefined) return 0;
  if (!Number.isFinite(value)) return 0;
  return value < 0 ? 0 : value;
}

/** 把百分比钳到 [0, 100]；非有限值变 0（**绝不产出 NaN 宽度**）。 */
export function clampPercent(value: number): number {
  if (!Number.isFinite(value)) return 0;
  if (value < 0) return 0;
  if (value > 100) return 100;
  return value;
}

/**
 * 「已用」取值。
 *
 * 优先用后端给的 `used`（上游直接给了就用上游的，credits.rs 已做过这个优先级）；
 * 老后端没有该字段时，退化成 `total - remaining` —— 这是后端自己也在用的口径，
 * 属于**既有字段的算术**，不是编造。两个字段都拿不到时返回 `null`，界面显示「—」。
 */
export function usedOf(resource: PackageResourceView): number | null {
  if (Number.isFinite(resource.used)) return safeCredit(resource.used);
  if (Number.isFinite(resource.total) && Number.isFinite(resource.remaining)) {
    return safeCredit(resource.total - resource.remaining);
  }
  return null;
}

/* -------------------------------------------------------------------------- */
/* 堆叠混色条：按包名聚合成段                                                     */
/* -------------------------------------------------------------------------- */

export interface PackageStackSegment {
  key: string;
  name: string;
  color: string;
  /** 该包名在本账号的剩余合计。 */
  remaining: number;
  /** 该包名在本账号的面额合计。 */
  total: number;
  /** 本账号里有几个包（几张发放记录）归到这个包名 —— 图例显示「×N」。 */
  count: number;
  /** 这一段**全部**来自已到期的包（仍有剩余但已失效）。 */
  allExpired: boolean;
  /** 该包名（本账号内）最早的到期时刻；全部无到期时间时为 `null`。 */
  soonestExpireAt: number | null;
  /** 0..100 的宽度百分比，已钳制；分母为 0 时恒为 0。 */
  percent: number;
}

/**
 * 把一个账号的积分包聚合成混色条的分段。
 *
 * 聚合口径：**按包名（取色键）合并**。同一个包名可能有多张发放记录
 * （各自 `packageCode` 不同、到期时间不同），在混色条里它们本来就是同色相邻的，
 * 合并后宽度才正确，图例也能一一对上（「×N」表示合并了几个包）。
 *
 * 除零保护：分母是各段剩余量之和。分母 ≤ 0 时每段 `percent` 都是 0，
 * 混色条退化成一条空的底色条 —— **不会出现 NaN 宽度**。
 */
export function buildStackSegments(
  resources: readonly PackageResourceView[],
  colorMap: ReadonlyMap<string, PackageColorEntry>,
): PackageStackSegment[] {
  interface Group {
    key: string;
    name: string;
    remaining: number;
    total: number;
    count: number;
    /** 未到期部分贡献的剩余量，用来判断这一段是不是「整段都已到期」。 */
    liveRemaining: number;
    /** 未到期部分的最早到期时刻（已到期的包不参与，否则图例会显示过去的日期）。 */
    soonestExpireAt: number | null;
  }

  const groups = new Map<string, Group>();
  for (const resource of resources) {
    const key = packageColorKey(resource);
    const remaining = safeCredit(resource.remaining);
    const group = groups.get(key) ?? {
      key,
      name: packageDisplayName(resource),
      remaining: 0,
      total: 0,
      count: 0,
      liveRemaining: 0,
      soonestExpireAt: null,
    };
    group.remaining += remaining;
    group.total += safeCredit(resource.total);
    group.count += 1;
    if (!resource.expired) {
      group.liveRemaining += remaining;
      const expireAt = resource.expireAt;
      if (expireAt !== null && Number.isFinite(expireAt) && expireAt > 0) {
        if (group.soonestExpireAt === null || expireAt < group.soonestExpireAt) {
          group.soonestExpireAt = expireAt;
        }
      }
    }
    groups.set(key, group);
  }

  const denominator = [...groups.values()].reduce((sum, group) => sum + group.remaining, 0);

  return [...groups.values()]
    .sort((left, right) =>
      right.remaining === left.remaining
        ? left.key.localeCompare(right.key)
        : right.remaining - left.remaining,
    )
    .map((group) => ({
      key: group.key,
      name: group.name,
      color: packageColorOf(colorMap, group.key),
      remaining: group.remaining,
      total: group.total,
      count: group.count,
      allExpired: group.remaining > 0 && group.liveRemaining <= 0,
      soonestExpireAt: group.soonestExpireAt,
      percent:
        denominator > 0 ? clampPercent((group.remaining / denominator) * 100) : 0,
    }));
}

/**
 * 明细表行序：先按取色顺序（同一包名的行必然相邻），再按剩余量降序。
 * 剩余量相同时保持后端返回的原始顺序，避免同一份数据每次渲染跳来跳去。
 */
export function sortPackageRows(
  resources: readonly PackageResourceView[],
  colorMap: ReadonlyMap<string, PackageColorEntry>,
): PackageResourceView[] {
  const order = new Map<string, number>();
  let index = 0;
  for (const key of colorMap.keys()) {
    order.set(key, index);
    index += 1;
  }

  return [...resources]
    .map((resource, original) => ({ resource, original }))
    .sort((left, right) => {
      const leftOrder = order.get(packageColorKey(left.resource)) ?? Number.MAX_SAFE_INTEGER;
      const rightOrder = order.get(packageColorKey(right.resource)) ?? Number.MAX_SAFE_INTEGER;
      if (leftOrder !== rightOrder) return leftOrder - rightOrder;
      const remainingDiff =
        safeCredit(right.resource.remaining) - safeCredit(left.resource.remaining);
      if (remainingDiff !== 0) return remainingDiff;
      return left.original - right.original;
    })
    .map(({ resource }) => resource);
}

/* -------------------------------------------------------------------------- */
/* 展示格式化                                                                    */
/* -------------------------------------------------------------------------- */

/** 与 `CreditStatsPage.tsx::formatCredits` 同一口径（「—」= 不知道，0 = 确定的零）。 */
export function formatCredit(value: number | null | undefined): string {
  if (value === null || value === undefined || !Number.isFinite(value)) return "—";
  return new Intl.NumberFormat("zh-CN", { maximumFractionDigits: 2 }).format(value);
}

/** `MM-DD`；取不到时「—」（**不编造**）。 */
export function formatMonthDay(ts: number | null | undefined): string {
  if (ts === null || ts === undefined || !Number.isFinite(ts) || ts <= 0) return "—";
  const date = new Date(ts);
  if (Number.isNaN(date.getTime())) return "—";
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
}

/** `YYYY/MM/DD`；取不到时「—」。 */
export function formatFullDate(ts: number | null | undefined): string {
  if (ts === null || ts === undefined || !Number.isFinite(ts) || ts <= 0) return "—";
  const date = new Date(ts);
  if (Number.isNaN(date.getTime())) return "—";
  return date.toLocaleDateString("zh-CN", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
  });
}

/** uid 太长时截断成前缀，避免把卡片标题挤爆。 */
export function uidPrefix(uid: string, length = 10): string {
  const trimmed = uid.trim();
  if (trimmed.length <= length) return trimmed;
  return `${trimmed.slice(0, length)}…`;
}

/**
 * 账号展示名，回退顺序：`nickname` → `note` → `uid` 前缀 → 账号 id 前缀。
 *
 * 为什么这么排：昵称是上游给的「真名」，备注是用户自己写的「这是谁的号」，
 * 两者都比一串哈希 uid 可读；都没有时才退回 uid。
 */
export function accountDisplayName(account: PackageAccountView): string {
  const nickname = (account.nickname ?? "").trim();
  if (nickname) return nickname;
  const note = (account.note ?? "").trim();
  if (note) return note;
  const uid = (account.uid ?? "").trim();
  if (uid) return uidPrefix(uid);
  const id = (account.id ?? "").trim();
  if (id) return uidPrefix(id);
  return "未命名账号";
}

/** 账号的次级标识（卡片副标题用）：优先 uid，其次 id。 */
export function accountSecondaryLabel(account: PackageAccountView): string {
  const uid = (account.uid ?? "").trim();
  if (uid) return uid;
  return (account.id ?? "").trim();
}

/** 一组包的最早到期时刻；全都没有到期时间时返回 `null`。 */
export function soonestExpireAt(resources: readonly PackageResourceView[]): number | null {
  let soonest: number | null = null;
  for (const resource of resources) {
    const expireAt = resource.expireAt;
    if (expireAt === null || !Number.isFinite(expireAt) || expireAt <= 0) continue;
    if (soonest === null || expireAt < soonest) soonest = expireAt;
  }
  return soonest;
}

/**
 * 账号总余额的取值口径。
 *
 * **优先用后端聚合值**（`CreditExpiry.totalRemaining`）：账号卡片
 * （`account-card.tsx`）显示的就是它。本区块若一律自己把各包 `remaining` 求和，
 * 遇到「上游返回的包列表不完整」时两处会给出不同余额 —— 同一个账号在一页里
 * 出现两个数字是最不可解释的偏差，而积分又是选号分层与轮转的判据。
 *
 * 只有老后端缺该字段（非有限数）时才退化成求和 —— 那是既有字段的算术，
 * 不是编造。注意：`0` 是**合法值**，不能被当成「缺失」而回退。
 */
export function resolveBalance(
  backendTotalRemaining: number | null | undefined,
  resources: readonly PackageResourceView[],
): number {
  if (typeof backendTotalRemaining === "number" && Number.isFinite(backendTotalRemaining)) {
    return safeCredit(backendTotalRemaining);
  }
  return resources.reduce((sum, resource) => sum + safeCredit(resource.remaining), 0);
}
