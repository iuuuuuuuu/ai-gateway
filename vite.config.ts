import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "node:path";

// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;
// GitHub Pages serves only the public demo from the repository subpath.
// Normal WebUI and Tauri builds intentionally keep Vite's root base.
// @ts-expect-error process is a nodejs global
const base = process.env.VITE_PAGES_DEMO === "1" ? "/ai-gateway/" : "/";

// https://vite.dev/config/
export default defineConfig(async () => ({
  base,
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 14200,
    strictPort: true,
    host: host || "127.0.0.1",
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 14201,
        }
      : undefined,
    watch: {
      // 忽略 Rust 构建产物、后端源码与 Git 目录，避免 Windows 下文件锁冲突 (EBUSY)
      //
      // ⚠ 还必须忽略 **`**/.*tmpdir*/**`**（2026-09-22 实测踩到）：
      // 编辑工具在写文件时会先建一个 `.<文件名>.<pid>.tmpdir` 临时目录
      // 放中间产物，写完即删。Vite 的 watcher 恰好在这个极短窗口里
      // 打开其中的 `.tmp` 文件 ⇒ `EBUSY: resource busy or locked`
      // ⇒ **整个 dev server 直接退出**（不是警告，是崩掉）。
      //
      // 表现为"改一次代码开发服务器就没了"，且报错里只有 tmpdir 路径、
      // 看不出与自己有关系。
      ignored: [
        "**/src-tauri/**",
        "**/target/**",
        "**/crates/**",
        "**/go-gateway/**",
        "**/.git/**",
        "**/scripts/**",
        "**/.*tmpdir*/**",
        "**/*.tmpdir/**",
      ],
    },
  },
}));
