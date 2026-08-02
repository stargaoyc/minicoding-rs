import { motion, AnimatePresence } from "framer-motion";
import { Plus, MessageSquare, PanelLeftClose, PanelLeft, Sun, Moon } from "lucide-react";
import { Button } from "../ui/button";
import { Badge } from "../ui/badge";
import { ScrollArea } from "../ui/scroll-area";
import { useSessions, useCreateSession } from "../../hooks/useSessions";
import { useUIStore } from "../../stores/ui";
import { cn, formatTime } from "../../lib/utils";
import type { SessionMeta } from "../../api/generated";

export function Sidebar() {
  const { data: sessions, isLoading } = useSessions();
  const createSession = useCreateSession();
  const { activeSessionId, setActiveSession, sidebarCollapsed, toggleSidebar, theme, toggleTheme } =
    useUIStore();

  const handleNewSession = () => {
    createSession.mutate(undefined, {
      onSuccess: (resp) => setActiveSession(resp.session_id),
    });
  };

  if (sidebarCollapsed) {
    return (
      <div className="flex w-14 flex-col items-center gap-3 border-r border-[var(--color-border)] py-3">
        <Button variant="ghost" size="icon" onClick={toggleSidebar} title="展开侧栏">
          <PanelLeft className="h-4 w-4" />
        </Button>
        <Button variant="ghost" size="icon" onClick={handleNewSession} title="新会话">
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
          <div className="gradient-accent flex h-7 w-7 items-center justify-center rounded-lg text-xs font-bold text-white">
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

      {/* New session button */}
      <div className="px-3 pb-2">
        <Button
          className="w-full justify-start"
          variant="secondary"
          onClick={handleNewSession}
          disabled={createSession.isPending}
        >
          <Plus className="h-4 w-4" />
          新建会话
        </Button>
      </div>

      {/* Session list */}
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
          ? "bg-[var(--color-accent)]/10 ring-1 ring-[var(--color-accent)]/30"
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
          {session.id.slice(-6)}
        </span>
        <span className="text-[10px] text-[var(--color-text-muted)]">
          {formatTime(session.last_message_at)}
        </span>
      </div>
      <div className="flex items-center gap-2 pl-5">
        <Badge variant={session.message_count > 0 ? "accent" : "default"}>
          {session.message_count} 条
        </Badge>
        {session.tasks.length > 0 && (
          <Badge variant="default">{session.tasks.length} 任务</Badge>
        )}
      </div>
    </motion.button>
  );
}
