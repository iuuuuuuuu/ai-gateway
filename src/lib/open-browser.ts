/**
 * 在**系统默认浏览器**里打开链接。
 *
 * # 为什么需要这个（所有者的反馈）
 *
 * 「都还无法主动打开默认浏览器打开链接(是默认浏览器,我默认浏览器是火狐,
 * 不要打开错了)」
 *
 * # 根因
 *
 * ZCode 与 Qoder 页此前用的是：
 *
 *     window.open(url, "_blank", "noopener,noreferrer")
 *
 * 在 Tauri 的 WebView 里，`window.open` **不会**交给系统浏览器 ——
 * 它要么在 WebView 内部新开一个窗口，要么被 WebView 静默拦掉。
 * 所以「点了没反应」或「在应用里开了个空白页」，而不是打开 Firefox。
 *
 * WorkBuddy 那条登录链路（`oauth-login-dialog.tsx`）用的是正确做法：
 * Tauri 环境下走 `@tauri-apps/plugin-opener` 的 `openUrl`，
 * 它调系统 shell 的 open 语义 → 走**默认浏览器**。
 *
 * 这里把它抽成公用函数，避免三个页面各写一份、再各自漏掉一半。
 *
 * # 为什么 WebUI 环境仍用 window.open
 *
 * dev 模式（浏览器预览）里没有 Tauri 注入，`plugin-opener` 会直接抛错。
 * 此时 `window.open` 才是对的（浏览器的新标签页）。
 * 这与 `api.isWebui()` 的判据一致。
 *
 * # 失败时不要把异常吞掉
 *
 * 调用方需要知道"没打开成"，才能在界面上显示可点的原生链接作为兜底。
 * 故这里**向上抛**，由调用方决定怎么提示（WorkBuddy 的做法是弹窗里
 * 始终显示可复制的链接）。
 */
export async function openInDefaultBrowser(url: string): Promise<void> {
  const target = String(url || "").trim();
  if (!target) {
    throw new Error("链接为空，无法打开");
  }

  // WebUI（dev 预览）：没有 Tauri 注入，用浏览器新标签页
  const isWebui = typeof window !== "undefined" && !("__TAURI_INTERNALS__" in window);
  if (isWebui) {
    // 被弹窗拦截器挡掉时 window.open 返回 null（不抛错），故要检查返回值
    const w = window.open(target, "_blank", "noopener,noreferrer");
    if (!w) {
      throw new Error("浏览器拦截了新标签页，请手动复制链接访问");
    }
    return;
  }

  // Tauri：走系统 opener → 默认浏览器
  const { openUrl } = await import("@tauri-apps/plugin-opener");
  await openUrl(target);
}
