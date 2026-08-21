import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import { dirname, resolve } from "node:path";

// Vitest 配置（M-14/R-10 前端单测基建，见 AGENTS.md §8.8）：
// - jsdom 环境（组件/hook 测试）；
// - MSW 拦截 HTTP/SSE REST 端点（不连真实后端，对齐 Rust 侧 wiremock 原则）；
// - SSE 事件流经 mockEventSource 以 fixture 重放（record/replay 快照三态，
//   对齐 dsh `DSH_SNAPSHOT`：replay 默认比对 / record 重录 / off 跳过）。
export default defineConfig({
  plugins: [react()],
  test: {
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    include: ["src/**/*.test.{ts,tsx}"],
    globals: false,
  },
  resolve: {
    alias: {
      "@": resolve(dirname(import.meta.filename ?? "."), "./src"),
    },
  },
});
