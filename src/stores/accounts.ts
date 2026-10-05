import { create } from "zustand";
import * as api from "@/lib/api";
import type { AccountMeta, AppStatus, CreditExpiry } from "@/lib/types";

/** In-flight credit fetches, shared so a remount does not start a second round. */
const creditInflight = new Set<string>();
let statusInflight: Promise<AppStatus> | undefined;

function fetchStatus(): Promise<AppStatus> {
  if (!statusInflight) {
    statusInflight = api.getStatus().finally(() => {
      statusInflight = undefined;
    });
  }
  return statusInflight;
}

async function fetchCreditExpiry(id: string): Promise<CreditExpiry> {
  try {
    return await api.getCreditExpiry(id);
  } catch (e) {
    return { ok: false, error: api.asError(e) };
  }
}
/**
 * 批量取积分；`null` = 本端没有批量通道（桌面端 / 演示模式）或请求失败。
 *
 * 失败也返回 `null` 而不是抛：调用方退回逐个路径即可，不该因为一次批量
 * 请求失败就让整个积分面板空白。
 */
async function fetchCreditExpiryMap(): Promise<Record<string, CreditExpiry> | null> {
  try {
    return await api.getCreditExpiryMap();
  } catch {
    return null;
  }
}

interface AccountsState {
  accounts: AccountMeta[];
  status: AppStatus | null;
  loading: boolean;
  error: string | null;
  creditMap: Record<string, CreditExpiry>;
  creditLoadingMap: Record<string, boolean>;
  /** 账号 id -> 最近一次积分查询完成时间（成功/失败都记录） */
  creditUpdatedAtMap: Record<string, number>;
  refreshingCredits: boolean;
  lastCreditRefreshAt: number;
  fetchAll: () => Promise<void>;
  refreshStatus: (signal?: AbortSignal) => Promise<void>;
  deleteAccount: (id: string) => Promise<void>;
  /** Fetch credits only for ids not already cached. */
  ensureCredits: (accountIds: string[]) => Promise<void>;
  /** Force-refresh credits. `silent` skips toolbar/card loading flicker (timer). */
  refreshCredits: (accountIds: string[], opts?: { silent?: boolean }) => Promise<void>;
  importLocal: () => Promise<{
    ok: boolean;
    account: AccountMeta | null;
    accounts: AccountMeta[];
    imported: number;
  }>;
  reconcileAccounts: () => Promise<void>;
}

export const useAccountsStore = create<AccountsState>((set, get) => ({
  accounts: [],
  status: null,
  loading: false,
  error: null,
  creditMap: {},
  creditLoadingMap: {},
  creditUpdatedAtMap: {},
  refreshingCredits: false,
  lastCreditRefreshAt: 0,

  async fetchAll() {
    set({ loading: true, error: null });
    try {
      const [status, { accounts }] = await Promise.all([fetchStatus(), api.getAccounts()]);
      set({ status, accounts, loading: false });
    } catch (e) {
      set({ error: api.asError(e), loading: false });
    }
  },

  async refreshStatus(signal) {
    try {
      const status = await fetchStatus();
      if (!signal?.aborted) set({ status });
    } catch {
      // 后台探测失败时保留最后一次成功状态，下一轮轮询继续尝试。
    }
  },

  async deleteAccount(id: string) {
    await api.deleteAccount(id);
    creditInflight.delete(id);
    const { creditMap, creditLoadingMap, creditUpdatedAtMap } = get();
    const nextCredits = { ...creditMap };
    const nextLoading = { ...creditLoadingMap };
    const nextUpdatedAt = { ...creditUpdatedAtMap };
    delete nextCredits[id];
    delete nextLoading[id];
    delete nextUpdatedAt[id];
    set({
      accounts: get().accounts.filter((a) => a.id !== id),
      creditMap: nextCredits,
      creditLoadingMap: nextLoading,
      creditUpdatedAtMap: nextUpdatedAt,
    });
  },

  async ensureCredits(accountIds) {
    await loadCredits(accountIds, false, false);
  },

  async refreshCredits(accountIds, opts) {
    await loadCredits(accountIds, true, opts?.silent === true);
  },

  async importLocal() {
    // 一次性导入所有可发现区域（国服 + 国际版）
    const res = await api.importLocal();
    await get().reconcileAccounts();
    return res;
  },

  async reconcileAccounts() {
    const { accounts } = await api.getAccounts();
    set({ accounts });
  },
}));

async function loadCredits(accountIds: string[], force: boolean, silent: boolean) {
  const ids = [...new Set(accountIds.filter(Boolean))];
  if (ids.length === 0) return;

  const state = useAccountsStore.getState();
  const toFetch = force
    ? ids
    : ids.filter((id) => state.creditMap[id] === undefined && !creditInflight.has(id));
  if (toFetch.length === 0) return;

  for (const id of toFetch) creditInflight.add(id);
  if (!silent) {
    useAccountsStore.setState((s) => {
      const creditLoadingMap = { ...s.creditLoadingMap };
      for (const id of toFetch) creditLoadingMap[id] = true;
      return {
        creditLoadingMap,
        refreshingCredits: force ? true : s.refreshingCredits,
      };
    });
  }

  // ★ 批量优先（2026-10-03，问题 ⑩-2 的最后一个热路径）。
  //
  // webui 下服务端有 `GET /api/credits/batch`：一次 HTTP 把全部账号查完
  // （服务端 4 并发）。上面这套逐个调用是「N 次 HTTP × 每次上游 3~4 次
  // 往返」，27 个账号实测累计 11~22 秒，全压在启动那一刻。
  //
  // ⚠ 只在账号数够多时走批量：`AccountsPage.tsx:806/845` 会在切换/接管成功后
  // 单独刷新**一个**账号（`refreshCredits([a.id])`），为它去查全部 27 个是
  // 纯浪费。`BATCH_MIN_ACCOUNTS` 就是这条分界。
  //
  // 桌面端没有批量命令 ⇒ `fetchCreditExpiryMap()` 返回 `null` ⇒ 自动退回
  // 下面的逐个路径（桌面端是本地 IPC，本来就没有这个问题）。
  const BATCH_MIN_ACCOUNTS = 4;
  if (toFetch.length >= BATCH_MIN_ACCOUNTS) {
    const batched = await fetchCreditExpiryMap();
    if (batched) {
      for (const id of toFetch) creditInflight.delete(id);
      const now = Date.now();
      useAccountsStore.setState((s) => {
        const creditMap = { ...s.creditMap };
        const creditUpdatedAtMap = { ...s.creditUpdatedAtMap };
        const creditLoadingMap = { ...s.creditLoadingMap };
        for (const id of toFetch) {
          const result = batched[id];
          // 服务端没给这个账号（查询失败）⇒ 保留上一次的结果，
          // 不把它写成 `undefined`（那会让卡片显示成「没有积分」）。
          if (result) {
            creditMap[id] = result;
            creditUpdatedAtMap[id] = now;
          }
          if (!silent) creditLoadingMap[id] = false;
        }
        return {
          creditMap,
          creditUpdatedAtMap,
          creditLoadingMap,
          lastCreditRefreshAt: now,
          refreshingCredits: silent ? s.refreshingCredits : force ? false : s.refreshingCredits,
        };
      });
      return;
    }
  }

  // ⚠ 限流到 4 并发（2026-09-22 所有者的"打开后无响应"）。
  //
  // 这里原本是裸 `Promise.all`：19 个账号各发一次 IPC 查积分，
  // 而调用它的 `useCreditAutoRefresh` 在**页面可见即触发**
  // （见 `refreshOnShow`：lastCreditRefreshAt === 0 时立刻 ensureCredits）——
  // 于是它与签到/旅行那 38 个请求叠在同一时刻，
  // 打开界面瞬间有 **57 个并发 IPC** 涌向 WebView2 主线程。
  //
  // 4 与 AccountsPage 的 STARTUP_FETCH_CONCURRENCY 取同一个量级：
  // 再高只是把压力推给上游（网关 max_in_flight=3）。
  //
  // ⚠ 不能用 `mapWithConcurrency`（那个在 AccountsPage 里、不导出）：
  // 本文件是 store，不该依赖页面模块。就地写一份 5 行的游标法
  // 比跨层 import 更干净 —— 两者的语义差异只在注释里，不在行为上。
  const CONCURRENCY = 4;
  let cursor = 0;
  const workers = Array.from({ length: Math.min(CONCURRENCY, toFetch.length) }, async () => {
    for (;;) {
      const index = cursor++;
      if (index >= toFetch.length) return;
      const id = toFetch[index];
      const result = await fetchCreditExpiry(id);
      creditInflight.delete(id);
      useAccountsStore.setState((s) => ({
        creditMap: { ...s.creditMap, [id]: result },
        creditUpdatedAtMap: { ...s.creditUpdatedAtMap, [id]: Date.now() },
        creditLoadingMap: silent ? s.creditLoadingMap : { ...s.creditLoadingMap, [id]: false },
      }));
    }
  });
  await Promise.all(workers);

  useAccountsStore.setState((s) => ({
    lastCreditRefreshAt: Date.now(),
    refreshingCredits: silent ? s.refreshingCredits : force ? false : s.refreshingCredits,
  }));
}
