import { create } from "zustand";

import type { PermissionMode } from "../api/generated";

/** 主题类型（W-05 暗色/亮色切换）。 */
type Theme = "dark" | "light";

/** 权限模式选项（4 个常用模式，与 NewSessionDialog 对齐）。 */
export const PERMISSION_MODE_OPTIONS: { key: PermissionMode; label: string }[] = [
  { key: "default", label: "默认" },
  { key: "accept_edits", label: "编辑自动" },
  { key: "plan", label: "规划" },
  { key: "bypass_permissions", label: "全自动" },
];

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
  /** 设置弹窗是否打开（手动触发，与 desktop store 的 needs-config 独立）。 */
  settingsOpen: boolean;
  /** 工作区文件预览（W-11）：当前预览的相对路径（null = 关闭预览）。 */
  previewPath: string | null;
  /** 预览面板是否展开。 */
  previewOpen: boolean;
  /** 当前会话的权限模式（由 SSE `permission_mode_changed` 更新，默认 default）。 */
  permissionMode: PermissionMode;
  setActiveSession: (id: string | null) => void;
  toggleTaskPanel: () => void;
  toggleSidebar: () => void;
  toggleTheme: () => void;
  setTheme: (theme: Theme) => void;
  setSettingsOpen: (open: boolean) => void;
  setPreview: (path: string | null, open?: boolean) => void;
  setPermissionMode: (mode: PermissionMode) => void;
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
  settingsOpen: false,
  previewPath: null,
  previewOpen: false,
  permissionMode: "default",
  setActiveSession: (id) => set({ activeSessionId: id }),
  toggleTaskPanel: () => set((s) => ({ taskPanelOpen: !s.taskPanelOpen })),
  toggleSidebar: () => set((s) => ({ sidebarCollapsed: !s.sidebarCollapsed })),
  setSettingsOpen: (open) => set({ settingsOpen: open }),
  setPreview: (path, open) => set(() => ({ previewPath: path, previewOpen: open ?? path != null })),
  setPermissionMode: (mode) => set({ permissionMode: mode }),
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
