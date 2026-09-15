import { useEffect, useRef } from "react";

/**
 * 可见性感知的定时器：窗口/标签页隐藏时**销毁**定时器，恢复可见时重建。
 *
 * 为什么不能只靠 `useEffect` 的清理函数：那只在组件卸载时触发。用户把窗口最小化、
 * 切到别的标签页、或把 App 收进托盘时组件**并没有卸载**，定时器会继续空转 ——
 * 白打网络请求、白占 CPU 与内存；多个页面各自持有时还会叠加。
 *
 * 本仓库已有两处正确范例（`use-credit-auto-refresh` / `use-workbuddy-status-refresh`），
 * 它们各自手写了同一套 stop/start 逻辑。这里抽成通用 hook，避免每处再写一遍、
 * 也避免像 gateway 页那样漏掉可见性判断。
 *
 * 语义要点：
 * - `hidden` 时**清掉**定时器（而非在回调里 return）—— 这样隐藏期间没有任何
 *   定时器存活，是真正的零开销，而不只是"不发请求"。
 * - 恢复可见时立即跑一次 `onResume`（若提供），让界面立刻追上最新状态，
 *   不必再等一整个周期。
 * - `enabled` 为 false 时同样不建定时器（如网关未运行就无需轮询）。
 *
 * @param callback 每个周期要执行的动作。用 ref 持有最新引用，
 *                 因此不必把它写进依赖数组（否则每次渲染都会重建定时器）。
 * @param intervalMs 周期毫秒数。
 * @param options.enabled 总开关，默认 true。
 * @param options.immediate 挂载（及 enabled 由 false 转 true）时是否**立刻**先执行一次，
 *                          默认 **true**。多数轮询场景需要它 —— 否则首次数据要等
 *                          一整个周期才出现（页面会先空白一段）。若调用方自己已有
 *                          独立的首次加载 effect，可传 false 避免重复请求。
 * @param options.onResume 由隐藏转回可见时立刻执行一次的动作（可选）。
 */
export function useVisibilityInterval(
  callback: () => void,
  intervalMs: number,
  options: { enabled?: boolean; immediate?: boolean; onResume?: () => void } = {},
): void {
  const { enabled = true, immediate = true, onResume } = options;

  // 用 ref 持有最新回调，避免调用方每次渲染都重建定时器
  const callbackRef = useRef(callback);
  callbackRef.current = callback;

  const onResumeRef = useRef(onResume);
  onResumeRef.current = onResume;

  // immediate 只影响「启用那一刻」，之后翻转不应重新定义 effect 行为
  const immediateRef = useRef(immediate);
  immediateRef.current = immediate;

  useEffect(() => {
    if (!enabled) return;

    let timer: number | undefined;

    function stop() {
      if (timer !== undefined) {
        window.clearInterval(timer);
        timer = undefined;
      }
    }

    function start() {
      stop();
      if (document.visibilityState === "hidden") return;
      timer = window.setInterval(() => {
        // 双保险：某些环境下 visibilitychange 可能未及时触发
        if (document.visibilityState === "hidden") return;
        callbackRef.current();
      }, intervalMs);
    }

    function onVisibilityChange() {
      if (document.visibilityState === "hidden") {
        stop();
      } else {
        // 恢复可见：先立即补一次（onResume 优先，否则用 callback 本身），再重建周期。
        // 两者取其一，避免同一次恢复触发两次请求。
        if (onResumeRef.current) onResumeRef.current();
        else callbackRef.current();
        start();
      }
    }

    // 启用即先跑一次：否则首次数据要等满一个周期才出现（页面先空白）。
    // 恢复可见时由 onVisibilityChange 负责补跑。
    if (document.visibilityState !== "hidden" && immediateRef.current) {
      callbackRef.current();
    }
    start();
    document.addEventListener("visibilitychange", onVisibilityChange);
    return () => {
      stop();
      document.removeEventListener("visibilitychange", onVisibilityChange);
    };
  }, [enabled, intervalMs]);
}
