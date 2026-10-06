import { useEffect } from "react";
import { listen } from "@tauri-apps/api/event";

import * as api from "@/lib/api";
import { useAccountsStore } from "@/stores/accounts";

/**
 * 应用内调度跑完一项任务后，宿主 emit 的 Tauri 事件名（见 `src-tauri/src/lib.rs`
 * 的 `spawn_background_loops`）。
 */
export const BACKGROUND_TASK_DONE_EVENT = "background-task-done";

/**
 * 同一条事件在页面内的转发名。
 *
 * 宿主事件只有窗口级监听（`listen`）能收到，而 Trae / 豆包页各自持有自己的
 * 状态（今日签到表、账号列表、趋势图 key），刷新逻辑属于页面 —— 由这个 hook
 * 统一收事件、再往 window 上转发一次，页面订阅它做局部刷新。
 */
export const BACKGROUND_TASK_DONE_DOM_EVENT = "dsh:background-task-done";

export interface BackgroundTaskDone {
  /** 任务键：`trae-checkin` / `doubao-keepalive` / `doubao-renew` / `doubao-quota` 等。 */
  kind: string;
  label: string;
  ok: boolean;
  exitCode: number;
  /** 计划执行时刻（HH:MM）。 */
  scheduledAt: string;
  /** 本轮检查时间（UTC ISO）。 */
  at: string;
}

/**
 * 后台任务跑完 → 立刻刷新界面状态。
 *
 * # 为什么不复用 `useWorkbuddyStatusRefresh`
 *
 * 那个 hook 的启停条件是「文档可见 && 主窗口可见 && 窗口聚焦」
 * （`use-workbuddy-status-refresh.ts` 的 `syncActiveState`）。用户把窗口收进托盘
 * 之后它**完全不轮询** —— 于是后台签到成功了，界面还停在「未签到」，
 * 直到用户把窗口调回前台才补上。业主对此的要求是「后台签到成功或者任务
 * 执行完成当然要刷新状态了」，所以这条路径必须与窗口可见性无关。
 *
 * 事件驱动也顺带省掉了轮询：调度循环每 5 分钟醒一次，只有真跑了任务才 emit。
 */
export function useBackgroundTaskRefresh() {
  useEffect(() => {
    // webui 是浏览器里的同一套前端，没有 Tauri 事件总线；那条轨由 HTTP 侧
    // 自己的轮询负责，这里直接不挂监听。
    if (!api.isDesktop()) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;

    void (async () => {
      try {
        const fn = await listen<BackgroundTaskDone>(BACKGROUND_TASK_DONE_EVENT, (event) => {
          const payload = event.payload;
          // WorkBuddy 侧状态（运行中/账号）与各平台页都挂在同一个 store 上，
          // 先无条件刷一次 —— 这一步与窗口是否可见无关。
          void useAccountsStore.getState().refreshStatus();
          window.dispatchEvent(
            new CustomEvent<BackgroundTaskDone>(BACKGROUND_TASK_DONE_DOM_EVENT, {
              detail: payload,
            }),
          );
        });
        if (disposed) {
          fn();
          return;
        }
        unlisten = fn;
      } catch {
        // 事件总线不可用不该影响应用：界面仍有手动刷新入口。
      }
    })();

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);
}
