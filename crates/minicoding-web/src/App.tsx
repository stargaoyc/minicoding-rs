import { useCallback, useEffect } from "react";
import { ListTodo, Loader2, AlertCircle, Settings } from "lucide-react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { Sidebar } from "./components/layout/Sidebar";
import { MessageList } from "./components/chat/MessageList";
import { ChatInput } from "./components/chat/ChatInput";
import { PermissionDialog } from "./components/permission/PermissionDialog";
import { TaskPanel } from "./components/tasks/TaskPanel";
import { SetupDialog } from "./components/setup/SetupDialog";
import { FilePreview } from "./components/workspace/FilePreview";
import { Button } from "./components/ui/button";
import { useMessages, useSendMessage, useSSEStream } from "./hooks/useChat";
import { usePermissions } from "./hooks/usePermissions";
import { useUIStore } from "./stores/ui";
import { useDesktopStore } from "./stores/desktop";
import { useSessions } from "./hooks/useSessions";
import { setApiBase } from "./api/client";
import type { Task } from "./api/generated";

const queryClient = new QueryClient({
  defaultOptions: {
    queries: { staleTime: 5_000, refetchOnWindowFocus: false },
  },
});

export default function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <DesktopGate />
    </QueryClientProvider>
  );
}

/**
 * 桌面端启动门控（M9，见 AGENTS.md §8.1、design.md §26.5）。
 *
 * 根据桌面初始化阶段决定渲染加载屏 / 配置弹窗 / 错误屏 / 主界面：
 * - `loading`：初始化中（检查配置 / 启动 sidecar）
 * - `needs-config`：缺少 provider 配置或 API key，弹 SetupDialog
 * - `error`：初始化失败，显示错误信息 + 重试按钮
 * - `ready`：sidecar 已启动，渲染主界面
 *
 * SetupDialog 在 `needs-config` / `ready` 阶段保持挂载，由其内部
 * AnimatePresence 根据 phase 控制可见性（保证退出动画播放）。
 */
function DesktopGate() {
  const phase = useDesktopStore((s) => s.phase);
  const apiBase = useDesktopStore((s) => s.apiBase);
  const error = useDesktopStore((s) => s.error);
  const init = useDesktopStore((s) => s.init);

  // mount 时初始化桌面环境（Web 模式直接 ready，Tauri 模式检查配置并启动 sidecar）
  useEffect(() => {
    void init();
  }, [init]);

  // apiBase 变化时同步到 HTTP/SSE 客户端（sidecar 启动后注入端口）
  useEffect(() => {
    setApiBase(apiBase);
  }, [apiBase]);

  if (phase === "loading") {
    return <LoadingScreen />;
  }

  if (phase === "error") {
    return <ErrorScreen message={error ?? "未知错误"} onRetry={() => void init()} />;
  }

  // needs-config / ready：SetupDialog 常驻（内部按 phase 控制可见性）
  return (
    <>
      {phase === "ready" && <AppInner />}
      <SetupDialog />
    </>
  );
}

/** 加载屏（初始化 sidecar 期间显示）。 */
function LoadingScreen() {
  return (
    <div className="flex h-screen w-screen flex-col items-center justify-center gap-4">
      <Loader2 className="h-8 w-8 animate-spin text-[var(--color-accent-hover)]" />
      <p className="text-sm text-[var(--color-text-muted)]">正在初始化…</p>
    </div>
  );
}

/** 错误屏（初始化失败时显示，提供重试按钮）。 */
function ErrorScreen({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div className="flex h-screen w-screen flex-col items-center justify-center gap-4 px-6 text-center">
      <div className="flex h-12 w-12 items-center justify-center rounded-xl bg-[var(--color-risk-high)]/15">
        <AlertCircle className="h-6 w-6 text-[var(--color-risk-high)]" />
      </div>
      <div className="space-y-1">
        <h2 className="text-base font-semibold">启动失败</h2>
        <p className="max-w-md text-sm text-[var(--color-text-muted)]">{message}</p>
      </div>
      <Button variant="secondary" size="sm" onClick={onRetry}>
        重试
      </Button>
    </div>
  );
}

function AppInner() {
  const { activeSessionId, taskPanelOpen, toggleTaskPanel, setSettingsOpen } = useUIStore();
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
          {/* 设置按钮：Tauri 模式修改 config.toml + keyring；Web 模式修改 localStorage */}
          <Button
            variant="ghost"
            size="sm"
            onClick={() => setSettingsOpen(true)}
            title="设置"
          >
            <Settings className="h-4 w-4" />
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
            {/* 发送错误提示 */}
            {sendMessage.isError && (
              <div className="border-t border-[var(--color-risk-high)]/30 bg-[var(--color-risk-high)]/10 px-4 py-2 text-sm text-[var(--color-risk-high)]">
                发送失败：{sendMessage.error instanceof Error ? sendMessage.error.message : String(sendMessage.error)}
              </div>
            )}
            {/* "思考中" 指示器：POST 请求进行中但 SSE 还未开始流式输出 */}
            {sendMessage.isPending && !isStreaming && (
              <div className="flex items-center gap-2 border-t border-[var(--color-border)] px-4 py-2 text-sm text-[var(--color-text-muted)]">
                <Loader2 className="h-4 w-4 animate-spin" />
                正在思考…
              </div>
            )}
            <ChatInput
              onSend={handleSend}
              isStreaming={isStreaming}
              disabled={sendMessage.isPending}
            />
          </>
        ) : (
          <EmptyState />
        )}

        {/* 工作区文件预览（W-11，底部滑出面板） */}
        <FilePreview sessionId={activeSessionId} />
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
