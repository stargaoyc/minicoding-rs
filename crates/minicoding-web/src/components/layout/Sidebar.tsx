import { motion, AnimatePresence } from "framer-motion";
import { useMemo, useState } from "react";
import { Plus, MessageSquare, PanelLeftClose, PanelLeft, Sun, Moon } from "lucide-react";
import { Button } from "../ui/button";
import { Badge } from "../ui/badge";
import { ScrollArea } from "../ui/scroll-area";
import { useSessions, useCreateSession } from "../../hooks/useSessions";
import { useSetPermissionMode } from "../../hooks/useTurnControl";
import { useUIStore, PERMISSION_MODE_OPTIONS } from "../../stores/ui";
import { isTauri } from "../../api/tauri";
import { loadWebSettings } from "../../stores/webSettings";
import type { CreateSessionBody } from "../../api/client";
import { cn, formatTime, truncate } from "../../lib/utils";
import type { SessionMeta, PermissionMode } from "../../api/generated";
import type { SessionModeKey } from "./NewSessionDialog";
import { WorkspacePanel } from "../workspace/WorkspacePanel";
import { NewSessionDialog } from "./NewSessionDialog";

/**
 * R10：新建会话的 SessionModeKey → 切换器 PermissionMode 映射。
 * 创建会话时 `default`/`accept_edits`/`plan`/`full_access` 分别走后端
 * `permission_mode`/`plan_mode`/`preset` 字段，与左侧切换器的四模式
 * 一一对应（`plan_mode: true` 映射为 `plan`；`preset: full-access` 映射为
 * `bypass_permissions`）。
 */
function modeToPermissionMode(mode: SessionModeKey): PermissionMode {
  switch (mode) {
    case "default":
      return "default";
    case "accept_edits":
      return "accept_edits";
    case "plan":
      return "plan";
    case "full_access":
      return "bypass_permissions";
  }
}

export function Sidebar() {
  const { data: sessions, isLoading } = useSessions();
  const createSession = useCreateSession();
  const { activeSessionId, setActiveSession, sidebarCollapsed, toggleSidebar, theme, toggleTheme, setPermissionMode } =
    useUIStore();
  const [newSessionOpen, setNewSessionOpen] = useState(false);

  const handleNewSession = (
    workdir?: string,
    mode: SessionModeKey = "accept_edits",
    dangerConfirmed = false,
  ) => {
    // Web 模式：从 localStorage 读取 provider 配置注入会话创建请求
    // Tauri 模式：sidecar 启动时已读 config.toml，无需注入
    let body: CreateSessionBody | undefined;
    if (!isTauri()) {
      const settings = loadWebSettings();
      body = {
        provider: settings.default,
        api_base: settings.api_base,
        model: settings.model,
        timeout_sec: settings.timeout_sec,
        max_retries: settings.max_retries,
        small_model: settings.small_model,
        turn_timeout_sec: settings.turn_timeout_sec,
        compress: settings.compress,
      };
    }
    // 权限模式 → CreateSessionBody（accept_edits 走 permission_mode；plan 走 plan_mode；
    // full-access 走 preset）
    if (mode === "accept_edits") {
      body = { ...body, permission_mode: "accept_edits" };
    } else if (mode === "plan") {
      body = { ...body, plan_mode: true };
    } else if (mode === "full_access") {
      // R8 FE-1 修复：C-22 二次确认贯通——此前只发 preset 不带 confirm_danger，
      // 后端强制校验恒 400，Web/Desktop 上"全自动·沙箱外"永远创建失败。
      body = { ...body, preset: "full-access", confirm_danger: dangerConfirmed };
    }
    if (workdir) {
      body = { ...body, workdir };
    }
    createSession.mutate(body, {
      // 创建成功才关对话框并切换会话；失败时保持打开并显示错误
      // （2026-08-24 用户反馈：此前失败被静默吞掉，表现为"点了确认没反应"）
      onSuccess: (resp) => {
        setNewSessionOpen(false);
        setActiveSession(resp.session_id);
        // R10 修复：新建会话选定的权限模式同步到 UI store（此前左下角切换器
        // 一直显示"默认"——`permission_mode` 只在运行时切换时更新，创建时的
        // `permission_mode: accept_edits`/`plan_mode`/preset 从未回写 store）。
        setPermissionMode(modeToPermissionMode(mode));
      },
    });
  };

  if (sidebarCollapsed) {
    return (
      <div className="flex w-14 flex-col items-center gap-3 border-r border-[var(--color-border)] py-3">
        <Button variant="ghost" size="icon" onClick={toggleSidebar} title="展开侧栏">
          <PanelLeft className="h-4 w-4" />
        </Button>
        <Button variant="ghost" size="icon" onClick={() => setNewSessionOpen(true)} title="新会话">
          <Plus className="h-4 w-4" />
        </Button>
        <Button
          variant="ghost"
          size="icon"
          onClick={toggleTheme}
          title={theme === "dark" ? "切换到亮色" : "切换到暗色"}
        >
          {theme === "dark" ? <Sun className="h-4 w-4" /> : <Moon className="h-4 w-4" />}
        </Button>
      </div>
    );
  }

  return (
    <div className="flex w-72 flex-col border-r border-[var(--color-border)] glass">
      {/* Header */}
      <div className="flex items-center justify-between px-4 py-3">
        <div className="flex items-center gap-2">
          <div className="gradient-accent anime-glow flex h-7 w-7 items-center justify-center rounded-lg text-xs font-bold text-white dark:text-[#141418]">
            m
          </div>
          <span className="gradient-text text-sm font-semibold">minicoding</span>
        </div>
        <div className="flex items-center gap-1">
          <Button
            variant="ghost"
            size="icon"
            onClick={toggleTheme}
            title={theme === "dark" ? "切换到亮色" : "切换到暗色"}
          >
            {theme === "dark" ? <Sun className="h-4 w-4" /> : <Moon className="h-4 w-4" />}
          </Button>
          <Button variant="ghost" size="icon" onClick={toggleSidebar} title="折叠侧栏">
            <PanelLeftClose className="h-4 w-4" />
          </Button>
        </div>
      </div>

      {/* 新建会话：先选目录再建对话（W-11） */}
      <div className="px-3 pb-2">
        <Button
          className="w-full justify-start"
          variant="secondary"
          onClick={() => setNewSessionOpen(true)}
          disabled={createSession.isPending}
        >
          <Plus className="h-4 w-4" />
          {createSession.isPending ? "创建中…" : "新建会话"}
        </Button>
      </div>

      {/* 会话列表 */}
      <ScrollArea className="flex-1 px-2">
        {isLoading ? (
          <div className="px-3 py-8 text-center text-sm text-[var(--color-text-muted)]">
            加载中…
          </div>
        ) : sessions && sessions.length > 0 ? (
          <div className="space-y-1">
            <AnimatePresence initial={false}>
              {sessions.map((s) => (
                <SessionItem
                  key={s.id}
                  session={s}
                  active={s.id === activeSessionId}
                  onClick={() => setActiveSession(s.id)}
                />
              ))}
            </AnimatePresence>
          </div>
        ) : (
          <div className="px-3 py-8 text-center text-sm text-[var(--color-text-muted)]">
            暂无会话
          </div>
        )}
      </ScrollArea>

      {/* 权限模式切换（对话栏旁，运行中切换四个模式） */}
      <PermissionModeSwitcher sessionId={activeSessionId} />

      {/* 项目工作区（W-11：文件树 + 预览 + diff + 切换，见 design.md §26.9） */}
      <WorkspacePanel sessionId={activeSessionId} />

      <NewSessionDialog
        open={newSessionOpen}
        creating={createSession.isPending}
        error={createSession.error instanceof Error ? createSession.error.message : null}
        onConfirm={handleNewSession}
        onClose={() => setNewSessionOpen(false)}
      />
    </div>
  );
}

function SessionItem({
  session,
  active,
  onClick,
}: {
  session: SessionMeta;
  active: boolean;
  onClick: () => void;
}) {
  // 会话显示名：优先 summary（首条消息摘要）；为空时用第一个任务的内容
  //（截断）；都为空才回退到 ID 后 6 位（用户反馈：不要用 DWG8H47J 这类代码
  // 当名称，用任务摘要）。
  const name = useMemo(() => {
    if (session.summary && session.summary.trim().length > 0) return session.summary;
    const firstTask = session.tasks[0]?.content;
    if (firstTask && firstTask.trim().length > 0) return truncate(firstTask.trim(), 30);
    return `会话 ${session.id.slice(-6)}`;
  }, [session]);

  return (
    <motion.button
      layout
      initial={{ opacity: 0, y: -4 }}
      animate={{ opacity: 1, y: 0 }}
      exit={{ opacity: 0 }}
      onClick={onClick}
      className={cn(
        "group flex w-full flex-col gap-1 rounded-lg px-3 py-2.5 text-left transition-all",
        active
          ? "bg-[var(--color-accent)]/15 ring-1 ring-[var(--color-accent)]/40 shadow-[0_0_16px_color-mix(in_srgb,var(--color-accent-grad-mid)_15%,transparent)]"
          : "hover:bg-[var(--color-surface-2)]",
      )}
    >
      <div className="flex items-center gap-2">
        <MessageSquare
          className={cn(
            "h-3.5 w-3.5 shrink-0",
            active ? "text-[var(--color-accent-hover)]" : "text-[var(--color-text-muted)]",
          )}
        />
        <span className={cn("flex-1 truncate text-sm", active && "text-[var(--color-text)]")}>
          {name}
        </span>
        <span className="text-[10px] text-[var(--color-text-muted)]">
          {formatTime(session.last_message_at)}
        </span>
      </div>
      <div className="flex items-center gap-2 pl-5">
        {session.tasks.length > 0 && <Badge variant="default">{session.tasks.length} 任务</Badge>}
      </div>
    </motion.button>
  );
}

/**
 * 权限模式切换器（对话栏旁，运行中切换四个权限模式）。
 *
 * 四模式与 NewSessionDialog 对齐：默认 / 编辑自动 / 规划 / 全自动（沙箱外）。
 * 当前模式来自 UI store（由 SSE `permission_mode_changed` 事件同步）；
 * 点击后调 `POST /sessions/{id}/permission-mode` 切换，成功同步 store，
 * 失败 toast 展示错误。
 */
function PermissionModeSwitcher({ sessionId }: { sessionId: string | null }) {
  const { permissionMode, setPermissionMode } = useUIStore();
  const setMode = useSetPermissionMode();
  const [switching, setSwitching] = useState(false);

  if (!sessionId) return null;

  const handleClick = async (mode: PermissionMode) => {
    if (mode === permissionMode || switching) return;
    // R8 FE-2：切换到 bypass_permissions 需 C-22 二次确认（红色警告弹窗）
    if (mode === "bypass_permissions") {
      const confirmed = window.confirm(
        "⚠ 全自动模式：所有副作用免弹窗自动放行，沙箱被绕过。\n\n请仅在受信隔离容器内启用。确认切换？",
      );
      if (!confirmed) return;
    }
    setSwitching(true);
    try {
      await setMode(sessionId, mode, mode === "bypass_permissions" ? true : undefined);
      // 乐观同步（后端成功回传前先高亮；SSE permission_mode_changed 会再次同步）
      setPermissionMode(mode);
    } catch (e) {
      console.error("permission mode switch failed:", e);
    } finally {
      setSwitching(false);
    }
  };

  return (
    <div className="border-t border-[var(--color-border)] px-3 py-2">
      <div className="mb-1.5 text-[10px] font-medium text-[var(--color-text-muted)]">权限模式</div>
      <div className="grid grid-cols-4 gap-1">
        {PERMISSION_MODE_OPTIONS.map((opt) => {
          const active = opt.key === permissionMode;
          return (
            <button
              key={opt.key}
              disabled={switching}
              onClick={() => void handleClick(opt.key)}
              className={cn(
                "rounded-md px-1 py-1 text-[10px] font-medium transition-colors",
                active
                  ? "bg-[var(--color-accent)]/20 text-[var(--color-accent-hover)] ring-1 ring-[var(--color-accent)]/40"
                  : "text-[var(--color-text-muted)] hover:bg-[var(--color-surface-2)] hover:text-[var(--color-text)]",
              )}
              title={opt.label}
            >
              {opt.label}
            </button>
          );
        })}
      </div>
    </div>
  );
}
