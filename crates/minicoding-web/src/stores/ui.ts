import { create } from "zustand";

/** 主题类型（W-05 暗色/亮色切换）。 */
type Theme = "dark" | "light";

/** UI 全局状态（客户端状态，不进 TanStack Query，见 AGENTS.md §8.5）。 */
interface UIState {
  /** 当前选中的会话 ID。 */
  activeSessionId: string | null;
  /** 任务面板是否展开。 */
  taskPanelOpen: boolean;
  /** 侧边栏是否折叠。 */
  sidebarCollapsed: boolean;
  /** 当前主题（W-05，持久化到 localStorage）。 */
  theme: Theme;
  setActiveSession: (id: string | null) => void;
  toggleTaskPanel: () => void;
  toggleSidebar: () => void;
  toggleTheme: () => void;
  setTheme: (theme: Theme) => void;
}

/** 从 localStorage 读取初始主题（默认暗色）。 */
function getInitialTheme(): Theme {
  if (typeof window === "undefined") return "dark";
  const saved = window.localStorage.getItem("minicoding-theme");
  if (saved === "light" || saved === "dark") return saved;
  // 跟随系统偏好
  return window.matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
}

/** 应用主题到 `<html>` 元素（切换 `dark`/`light` class）。 */
function applyTheme(theme: Theme) {
  if (typeof document === "undefined") return;
  const root = document.documentElement;
  root.classList.remove("dark", "light");
  root.classList.add(theme);
}

export const useUIStore = create<UIState>((set) => ({
  activeSessionId: null,
  taskPanelOpen: false,
  sidebarCollapsed: false,
  theme: getInitialTheme(),
  setActiveSession: (id) => set({ activeSessionId: id }),
  toggleTaskPanel: () => set((s) => ({ taskPanelOpen: !s.taskPanelOpen })),
  toggleSidebar: () => set((s) => ({ sidebarCollapsed: !s.sidebarCollapsed })),
  setTheme: (theme) => {
    applyTheme(theme);
    if (typeof window !== "undefined") {
      window.localStorage.setItem("minicoding-theme", theme);
    }
    set({ theme });
  },
  toggleTheme: () => {
    set((s) => {
      const next: Theme = s.theme === "dark" ? "light" : "dark";
      applyTheme(next);
      if (typeof window !== "undefined") {
        window.localStorage.setItem("minicoding-theme", next);
      }
      return { theme: next };
    });
  },
}));
