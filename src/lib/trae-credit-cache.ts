import * as api from "@/lib/api";

/**
 * 积分详情的进程内缓存 + 并发闸门（供「兼容网关」页的 Trae 账号行复用）。
 *
 * 为什么需要：
 *   1. `trae_credit_detail` 一次要打三条上游账（IDE 积分 / 权益包 / 付费身份），
 *      而网关页每秒都有一次重渲染（驱动「上次更新 x 秒前」），若把请求写在
 *      渲染路径上，同一账号会被反复重查；
 *   2. 账号一多，页面一打开就会同时打出 N 组上游请求 —— 既慢又容易触发限流。
 *
 * 这里做两件事：
 *   1. 同一账号的结果缓存 5 分钟，重渲染 / 重挂载不再重查；
 *   2. 同时最多 2 个账号在查，其余排队，避免瞬时并发。
 *
 * 缓存是模块级的，故跨 tab 切换、跨组件实例都有效；页面刷新即失效。
 *
 * 为什么不直接从 `TraePage.tsx` 里 import：那是页面组件模块，把它拉进来会
 * 形成「页面 ↔ 页面」的循环依赖，并顺带把整页组件树拖进网关页的模块图。
 * 这段逻辑与页面无关，故提炼到这里各用一份。
 */
const CREDIT_CACHE_TTL_MS = 5 * 60 * 1000;
const CREDIT_MAX_CONCURRENCY = 2;
const creditCache = new Map<string, { detail: api.TraeCreditDetail; at: number }>();
const creditInflight = new Map<string, Promise<api.TraeCreditDetail>>();
let creditActive = 0;
const creditQueue: (() => void)[] = [];

function creditAcquire(): Promise<void> {
  if (creditActive < CREDIT_MAX_CONCURRENCY) {
    creditActive += 1;
    return Promise.resolve();
  }
  return new Promise<void>((resolve) => {
    creditQueue.push(() => {
      creditActive += 1;
      resolve();
    });
  });
}

function creditRelease(): void {
  creditActive -= 1;
  const next = creditQueue.shift();
  if (next) next();
}

/** 取积分详情：命中缓存直接返回，否则排队查询并回填缓存。 */
export function loadTraeCredit(userId: string): Promise<api.TraeCreditDetail> {
  const hit = creditCache.get(userId);
  if (hit && Date.now() - hit.at < CREDIT_CACHE_TTL_MS) return Promise.resolve(hit.detail);
  const running = creditInflight.get(userId);
  if (running) return running;
  const task = creditAcquire()
    .then(() => api.traeCreditDetail(userId))
    .then((detail) => {
      creditCache.set(userId, { detail, at: Date.now() });
      return detail;
    })
    .finally(() => {
      creditInflight.delete(userId);
      creditRelease();
    });
  creditInflight.set(userId, task);
  return task;
}

/**
 * 积分详情 → 到期档位（`YYYY-MM-DD`）；取不到时返回 `undefined`（= 保持现状「未知」）。
 *
 * 为什么取「最晚」而不是「最早」：档位决定的是「这个号还能用到什么时候」，
 * 只要还有一份没到期的包，这个号就不算到期；取最早会把仍然可用的号误判成快到期。
 * 与宿主侧 `trae_credits.rs` 的 `payExpireAt`（身份到期优先，否则取最晚的包）同口径。
 *
 * 为什么要过滤 >= 2100 年：上游用 `2126-01-01T00:00:00Z` 这类哨兵表示「长期有效」，
 * 直接当日期显示会变成「长期有效」却排在所有账号之前（越晚越靠后），语义反了 ——
 * 故按「无档位」处理，与现有 `undefined` 的排序语义一致。
 */
export function traeExpiryDay(detail: api.TraeCreditDetail | undefined): string | undefined {
  if (!detail) return undefined;
  const candidates: string[] = [];
  if (detail.payExpireAt) candidates.push(detail.payExpireAt);
  for (const pack of detail.packs) {
    if (pack.expireAt) candidates.push(pack.expireAt);
  }
  let best: string | undefined;
  for (const raw of candidates) {
    const day = raw.slice(0, 10);
    if (!/^\d{4}-\d{2}-\d{2}$/.test(day)) continue;
    // 上游的「长期有效」哨兵（>= 2100 年）：不是真的到期日，按无档位处理。
    if (Number(day.slice(0, 4)) >= 2100) continue;
    if (best === undefined || day > best) best = day;
  }
  return best;
}
