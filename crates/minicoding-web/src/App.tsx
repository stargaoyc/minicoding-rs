import { useCallback } from "react";
import { ListTodo } from "lucide-react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { Sidebar } from "./components/layout/Sidebar";
import { MessageList } from "./components/chat/MessageList";
import { ChatInput } from "./components/chat/ChatInput";
import { PermissionDialog } from "./components/permission/PermissionDialog";
import { TaskPanel } from "./components/tasks/TaskPanel";
import { Button } from "./components/ui/button";
import { useMessages, useSendMessage, useSSEStream } from "./hooks/useChat";
import { usePermissions } from "./hooks/usePermissions";
import { useUIStore } from "./stores/ui";
import { useSessions } from "./hooks/useSessions";
import type { Task } from "./api/generated";

const queryClient = new QueryClient({
  defaultOptions: {
    queries: { staleTime: 5_000, refetchOnWindowFocus: false },
  },
});

export default function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <AppInner />
    </QueryClientProvider>
  );
}

function AppInner() {
  const { activeSessionId, taskPanelOpen, toggleTaskPanel } = useUIStore();
  const permissions = usePermissions();
  const { data: sessions } = useSessions();

  // 从 session list 提取当前会话的任务列表
  const activeSession = sessions?.find((s) => s.id === activeSessionId);
  const tasks: Task[] = activeSession?.tasks ?? [];

  // 消息 + SSE 流（含权限请求回调）
  const { data: messages, isLoading } = useMessages(activeSessionId);
  const { streamingText, isStreaming } = useSSEStream(activeSessionId, {
    onPermissionRequested: (e) => {
      if (activeSessionId) {
        permissions.requestPermission({ sessionId: activeSessionId, ...e });
      }
    },
  });
  const sendMessage = useSendMessage(activeSessionId);

  const handleSend = useCallback(
    (text: string) => {
      if (!activeSessionId) return;
      sendMessage.mutate(text);
    },
    [activeSessionId, sendMessage],
  );

  return (
    <div className="flex h-screen w-screen overflow-hidden">
      <Sidebar />

      {/* Main chat area */}
      <div className="flex flex-1 flex-col">
        {/* Top bar */}
        <div className="flex items-center justify-between border-b border-[var(--color-border)] px-4 py-2.5">
          <div className="flex items-center gap-2">
            {activeSessionId ? (
              <span className="text-sm font-medium">
                会话 {activeSessionId.slice(-8)}
              </span>
            ) : (
              <span className="text-sm text-[var(--color-text-muted)]">
                选择或创建一个会话
              </span>
            )}
          </div>
          <Button
            variant="ghost"
            size="sm"
            onClick={toggleTaskPanel}
            disabled={!activeSessionId || tasks.length === 0}
          >
            <ListTodo className="h-4 w-4" />
            任务
            {tasks.length > 0 && (
              <span className="ml-1 text-xs text-[var(--color-accent-hover)]">
                {tasks.length}
              </span>
            )}
          </Button>
        </div>

        {/* Messages */}
        {activeSessionId ? (
          <>
            <MessageList
              messages={messages}
              streamingText={streamingText}
              isStreaming={isStreaming}
              isLoading={isLoading}
            />
            <ChatInput
              onSend={handleSend}
              isStreaming={isStreaming}
              disabled={sendMessage.isPending}
            />
          </>
        ) : (
          <EmptyState />
        )}
      </div>

      {/* Task panel (right side, collapsible) */}
      <TaskPanel tasks={tasks} open={taskPanelOpen} onClose={toggleTaskPanel} />

      {/* Permission dialog (overlay) */}
      <PermissionDialog pending={permissions.pending} onResolve={permissions.resolve} />
    </div>
  );
}

function EmptyState() {
  return (
    <div className="flex flex-1 flex-col items-center justify-center gap-6">
      <div className="gradient-accent flex h-20 w-20 items-center justify-center rounded-3xl text-3xl font-bold text-white shadow-2xl shadow-indigo-500/20">
        m
      </div>
      <div className="space-y-2 text-center">
        <h1 className="gradient-text text-2xl font-bold">minicoding</h1>
        <p className="text-sm text-[var(--color-text-muted)]">
          AI 编程助手 · Rust 实现 · 终端 / Web / 桌面
        </p>
      </div>
      <div className="flex flex-col gap-1.5 text-center text-xs text-[var(--color-text-muted)]">
        <p>点击左侧"新建会话"开始对话</p>
        <p>支持流式输出、权限确认、任务管理</p>
      </div>
    </div>
  );
}
