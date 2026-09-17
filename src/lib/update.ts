// 更新源必须是**本 fork**：写错会让自动更新静默 404，或把用户切到别人的版本上去。
// 后端 `update.rs` 的 GITHUB_OWNER/GITHUB_REPO 必须与此一致 —— 有单测钉住这一点。
export const GITHUB_OWNER = "iuuuuuuuu";
export const GITHUB_REPO = "ai-gateway";
export const GITHUB_REPOSITORY_URL = `https://github.com/${GITHUB_OWNER}/${GITHUB_REPO}`;
export const GITHUB_RELEASE_URL = `${GITHUB_REPOSITORY_URL}/releases/latest`;

/** 在桌面端通过 Tauri opener 打开，在 webui 端打开新标签页。 */
export async function openReleaseUrl(url = GITHUB_RELEASE_URL): Promise<void> {
  const isWebui = typeof window !== "undefined" && !("__TAURI_INTERNALS__" in window);
  if (isWebui) {
    window.open(url, "_blank", "noopener,noreferrer");
    return;
  }
  const { openUrl } = await import("@tauri-apps/plugin-opener");
  await openUrl(url);
}
