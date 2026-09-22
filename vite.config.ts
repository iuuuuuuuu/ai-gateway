import { defineConfig, loadEnv } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "node:path";

/**
 * 是否 GitHub Pages 公开演示构建。
 *
 * 为什么用 `loadEnv` 读 `.env.<mode>` 而不是 `process.env`：npm script 里的
 * `VITE_PAGES_DEMO=1 vite build` 是 POSIX 内联变量写法，在 Windows 下直接失败
 * （仓库运行环境是 Windows，见 AGENTS.md Shell Policy）。
 * 改成 `vite build --mode pages-demo` + 本函数读文件后，两端一致且可复现。
 */
function isPagesDemo(mode: string): boolean {
  // @ts-expect-error process 是 Node 全局（本文件由 Vite 在 Node 侧加载）
  const env = loadEnv(mode, process.cwd(), "VITE_");
  return env.VITE_PAGES_DEMO === "1";
}

// @ts-expect-error process 是 Node 全局（本文件由 Vite 在 Node 侧加载）
const host = process.env.TAURI_DEV_HOST;

// GitHub Pages serves only the public demo from the repository subpath.
// Normal WebUI and Tauri builds intentionally keep Vite's root base.
// https://vite.dev/config/
export default defineConfig(async ({ mode }) => ({
  base: isPagesDemo(mode) ? "/ai-gateway/" : "/",
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
      ignored: [
        "**/src-tauri/**",
        "**/target/**",
        "**/crates/**",
        "**/go-gateway/**",
        "**/.git/**",
        "**/scripts/**",
      ],
    },
  },
}));
