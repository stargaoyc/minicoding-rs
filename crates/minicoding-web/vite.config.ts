import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// Vite 配置（M9 Web 前端，见 AGENTS.md §8.7、design.md §26.7）
// 开发模式默认连接 `http://localhost:8080`（minicoding-server 默认端口）；
// 通过 `VITE_API_BASE` 环境变量可覆盖（如 Tauri sidecar 动态端口）。
export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    port: 5173,
    // 开发模式代理 API/SSE 到 minicoding-server，避免 CORS
    // （2026-08-25 审查：补 /config /metrics——设置面板与监控端点此前直连被 CORS 拦截）
    proxy: {
      "/sessions": { target: "http://localhost:8080", changeOrigin: true },
      "/config": { target: "http://localhost:8080", changeOrigin: true },
      "/metrics": { target: "http://localhost:8080", changeOrigin: true },
      "/health": { target: "http://localhost:8080", changeOrigin: true },
    },
  },
  build: {
    outDir: "dist",
    sourcemap: true,
  },
});
