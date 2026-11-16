import { defineConfig } from "vite";

export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  // Tauri 2 把构建信息放在 TAURI_ENV_* 里（Tauri 1 的 TAURI_DEBUG 已不存在）；
  // 前缀收窄到 TAURI_ENV_，避免 TAURI_SIGNING_* 之类的敏感变量被暴露给前端代码。
  envPrefix: ["VITE_", "TAURI_ENV_"],
  build: {
    target: "es2021",
    minify: !process.env.TAURI_ENV_DEBUG ? "esbuild" : false,
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
  },
});
