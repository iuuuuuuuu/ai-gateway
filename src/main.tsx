import React from "react";
import ReactDOM from "react-dom/client";
import "@fontsource-variable/bricolage-grotesque";
import App from "./App";
import "./index.css";
import { invoke } from "@tauri-apps/api/core";
import { applyTheme, getThemePreference, watchSystemTheme } from "./lib/theme";

applyTheme(getThemePreference());
const stopWatchingSystemTheme = watchSystemTheme();
if (import.meta.hot) import.meta.hot.dispose(stopWatchingSystemTheme);

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);

// ---- 启动时序：告诉 Rust 侧「第一帧已经画出来了」（2026-10-05）----
//
// 主窗口在 tauri.conf.json 里是 `"visible": false` 创建的，什么时候露面完全由
// Rust 决定。现在的策略是**等这里上报之后才 show**：窗口一旦可见，用户就会去点它，
// 而此时 WebView2 的浏览器进程可能还在初始化 —— 那种点击会被 Windows 判成
// 「未响应」，正是所有者反馈的「直接点击就会无响应」。
//
// 为什么用双 requestAnimationFrame 而不是 render 之后直接 invoke：
// `render()` 返回只代表 React 把 DOM 提交完了，浏览器**还没**把这一帧画到屏幕上。
// 双 rAF 的第二个回调必然排在「当前帧已经渲染」之后，这才是真正的首帧信号。
// （实测：即使窗口还没 show，WebView2 的页面也照常出帧，rAF 13ms 内就回调两次。）
//
// 上报失败不影响界面：Rust 侧有 10 秒兜底，超时会无条件把窗口显示出来。
function reportFrontendReady() {
  if (typeof window === "undefined" || !("__TAURI_INTERNALS__" in window)) return;
  void invoke("frontend_ready").catch(() => {
    /* 忽略：兜底逻辑在 Rust 侧 */
  });
}

requestAnimationFrame(() => {
  requestAnimationFrame(() => {
    reportFrontendReady();
  });
});
