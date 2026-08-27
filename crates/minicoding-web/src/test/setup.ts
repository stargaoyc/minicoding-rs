/**
 * MSW server 生命周期（vitest setup）。
 *
 * - `onUnhandledRequest: "error"`：测试触达未 mock 的端点时显式失败，
 *   防止静默连真实后端（AGENTS.md §5.4 测试不连真实服务）。
 */
import { cleanup } from "@testing-library/react";
import { afterAll, afterEach, beforeAll } from "vitest";
import { server } from "./server";

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => {
  server.resetHandlers();
  cleanup();
});
afterAll(() => server.close());

// jsdom 不实现 `window.matchMedia`——`stores/ui.ts` 初始化主题时调用
//（`useChat.test.tsx` 引入 useUIStore 触发），需显式 stub。
if (typeof window !== "undefined" && !window.matchMedia) {
  Object.defineProperty(window, "matchMedia", {
    writable: true,
    value: (query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addListener: () => {},
      removeListener: () => {},
      addEventListener: () => {},
      removeEventListener: () => {},
      dispatchEvent: () => false,
    }),
  });
}
