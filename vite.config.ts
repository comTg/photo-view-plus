import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path from "node:path";

// Tauri 桌面应用配置：固定端口、不打开浏览器、resource 走相对路径
// 详见 docs/00-architecture.md § 6
export default defineConfig(async () => ({
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(import.meta.dirname, "src"),
    },
  },
  // Tauri 期望前端跑在固定端口，dev 模式由 tauri.conf devUrl 引用
  server: {
    port: 5173,
    strictPort: true,
    host: "127.0.0.1",
    // tauri 自己开窗口，禁止 vite 打开浏览器
    open: false,
    // 防止 HMR 在 Tauri 内嵌环境下尝试连不存在的 ws
    hmr: { host: "127.0.0.1" },
  },
  // build 输出由 tauri.conf 的 frontendDist 指向
  build: {
    target: "es2022",
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: true,
    chunkSizeWarningLimit: 1024,
  },
  // 让 import.meta.env.PVP_PROFILE 在前端可读（由 cross-env 注入）
  define: {
    "import.meta.env.PVP_PROFILE": JSON.stringify(process.env.PVP_PROFILE ?? "dev"),
  },
}));
