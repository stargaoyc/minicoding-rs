import { useCallback, useEffect } from "react";
import { ListTodo, Loader2, AlertCircle, Settings, ShieldAlert, ShieldX, Square, Undo2, Activity } from "lucide-react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { AnimeBackground } from "./components/AnimeBackground";
import { Sidebar } from "./components/layout/Sidebar";
import { MessageList } from "./components/chat/MessageList";
import { PlanPanel } from "./components/chat/PlanPanel";
import { ChatInput } from "./components/chat/ChatInput";
import { PermissionDialog } from "./components/permission/PermissionDialog";
import { TaskPanel } from "./components/tasks/TaskPanel";
import { SetupDialog } from "./components/setup/SetupDialog";
import { FilePreview } from "./components/workspace/FilePreview";
import { TracePanel } from "./components/trace/TracePanel";
import { Button } from "./components/ui/button";
import {
  useMessages,
  useSendMessage,
  useSSEStream,
  useTurnRunning,
} from "./hooks/useChat";
import { usePermissions } from "./hooks/usePermissions";
import { useUIStore } from "./stores/ui";
import { useDesktopStore } from "./stores/desktop";
import { useTraceStore } from "./stores/trace";
import { useSessions } from "./hooks/useSessions";
import { loadServerToken } from "./stores/webSettings";
import {
  useCancelTurn,
  useSetApiBase,
  useSetApiToken,
  useUndoSession,
} from "./hooks/useTurnControl";
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
      <AnimeBackground />
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
  // ARCH-5：连接级副作用经 hook 封装
  const setApiBaseClient = useSetApiBase();
  const setApiTokenClient = useSetApiToken();

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
    setApiBaseClient(apiBase);
  }, [apiBase]);

  // R10-04：Web 直连模式从 localStorage 恢复鉴权 token（Tauri 模式由 sidecar
  // 经 env 注入 `setApiToken`，此处为空串不影响）——此前 Web 形态开箱 401 且
  // 无任何 token 输入口，用户只能重建前端或 `--no-auth`。
  useEffect(() => {
    setApiTokenClient(loadServerToken());
  }, [setApiTokenClient]);

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
  const traceOpen = useTraceStore((s) => s.open);
  const traceSetOpen = useTraceStore((s) => s.setOpen);
  const permissions = usePermissions();
  const { data: sessions } = useSessions();

  // 从 session list 提取当前会话的任务列表
  const activeSession = sessions?.find((s) => s.id === activeSessionId);
  const tasks: Task[] = activeSession?.tasks ?? [];

  // 消息 + SSE 流（含权限请求回调）
  const { data: messages, isLoading } = useMessages(activeSessionId);
  // R9 P3-3：turn 运行状态轮询（SSE 断线/刷新后恢复 isStreaming）
  const { data: turnRunning } = useTurnRunning(activeSessionId);
  const {
    streamingText,
    streamingReasoning,
    reasoningHistory,
    isStreaming,
    activeTools,
    elapsedSec,
    waitingPermission,
    permissionDeniedMsg,
    planActive,
  } = useSSEStream(activeSessionId, {
    onPermissionRequested: (e) => {
      if (activeSessionId) {
        permissions.requestPermission({
          sessionId: activeSessionId,
          id: e.id,
          tool: e.tool,
          summary: e.summary,
          risk: e.risk,
          options: e.options,
        });
      }
    },
    // R8 FE-4：权限已决/回合结束 → 自动关闭弹窗（服务端超时 Deny、他端
    // 已裁决时，pending 不再残留；否则用户点"允许"得 404 且无引导）
    onPermissionResolved: () => permissions.dismiss(),
  }, turnRunning);
  const sendMessage = useSendMessage(activeSessionId);
  // ARCH-5：api 层调用统一经 hooks 封装（AGENTS.md §8.3 分层令）
  const cancelTurnById = useCancelTurn();
  const undoSessionById = useUndoSession();

  const handleSend = useCallback(
    (text: string) => {
      if (!activeSessionId) return;
      sendMessage.mutate(text);
    },
    [activeSessionId, sendMessage],
  );

  // 停止当前 turn（POST cancel；sendMessage 的 POST 阻塞至 turn 结束，取消后返回）
  const handleCancel = useCallback(() => {
    cancelTurnById(activeSessionId);
  }, [cancelTurnById, activeSessionId]);

  // FE-6（2026-08-25 R2 审查）：回滚最近一步文件改动——服务端路由此前已就绪
  // 但 Web 前端零消费（四形态能力矩阵漂移）。失败文件以 toast 式 alert 兜底展示。
  const handleUndo = useCallback(() => {
    if (!activeSessionId) return;
    undoSessionById(activeSessionId)
      .then((r) => {
        if (r.failed_files.length > 0) {
          console.warn("[undo] 冲突未回滚:", r.failed_files);
          window.alert(
            `已回滚 ${r.restored_files.length} 个文件；${r.failed_files.length} 个冲突未回滚（详见控制台）`,
          );
        }
      })
      .catch(() => {
        console.warn("undo failed");
      });
  }, [activeSessionId, undoSessionById]);

  const turnBusy = sendMessage.isPending || isStreaming;

  return (
    <div className="flex h-screen w-screen overflow-hidden">
      <Sidebar />

      {/* Main chat area */}
      <div className="flex flex-1 flex-col">
        {/* Top bar */}
        <div className="flex items-center justify-between border-b border-[var(--color-border)] px-4 py-2.5">
          <div className="flex items-center gap-2">
            {activeSessionId ? (
              <span className="text-sm font-medium">会话 {activeSessionId.slice(-8)}</span>
            ) : (
              <span className="text-sm text-[var(--color-text-muted)]">选择或创建一个会话</span>
            )}
          </div>
          <Button
            variant="ghost"
            size="sm"
            onClick={handleUndo}
            disabled={!activeSessionId}
            title="回滚最近一步文件改动（/undo）"
          >
            <Undo2 className="h-4 w-4" />
            回滚
          </Button>
          {/* R9 P3-3：结束对话——运行中显式停止当前 turn。此前只能靠输入框
              停止按钮（isStreaming 时），用户对"前一个对话未结束就发送下一个
              是否卡死"有疑虑：后端 send_message 已预占取消卡死 turn，此处
              提供显式入口，确认状态后再发送下一条。 */}
          {turnBusy && (
            <Button
              variant="danger"
              size="sm"
              onClick={handleCancel}
              disabled={!activeSessionId}
              title="结束当前对话（停止正在运行的 turn）"
            >
              <Square className="h-4 w-4" />
              结束对话
            </Button>
          )}
          <Button
            variant="ghost"
            size="sm"
            onClick={toggleTaskPanel}
            disabled={!activeSessionId || tasks.length === 0}
          >
            <ListTodo className="h-4 w-4" />
            任务
            {tasks.length > 0 && (
              <span className="ml-1 text-xs text-[var(--color-accent-hover)]">{tasks.length}</span>
            )}
          </Button>
          {/* R10：可观测性——运行流程按钮（打开 TracePanel 时间线） */}
          <Button
            variant="ghost"
            size="sm"
            onClick={() => traceSetOpen(!traceOpen)}
            disabled={!activeSessionId}
            title="Agent Loop 运行流程（可观测性 / 回放）"
          >
            <Activity className="h-4 w-4" />
            运行流程
          </Button>
          {/* 设置按钮：Tauri 模式修改 config.toml + keyring；Web 模式修改 localStorage */}
          <Button variant="ghost" size="sm" onClick={() => setSettingsOpen(true)} title="设置">
            <Settings className="h-4 w-4" />
          </Button>
        </div>

        {/* Messages */}
        {activeSessionId ? (
          <>
            <PlanPanel active={planActive} />
            <MessageList
              sessionId={activeSessionId}
              messages={messages}
              streamingText={streamingText}
              streamingReasoning={streamingReasoning}
              reasoningHistory={reasoningHistory}
              isStreaming={isStreaming}
              isLoading={isLoading}
              activeTools={activeTools}
            />
            {/* 发送错误提示 */}
            {sendMessage.isError && (
              <div className="border-t border-[var(--color-risk-high)]/30 bg-[var(--color-risk-high)]/10 px-4 py-2 text-sm text-[var(--color-risk-high)]">
                发送失败：
                {sendMessage.error instanceof Error
                  ? sendMessage.error.message
                  : String(sendMessage.error)}
              </div>
            )}
            {/* 权限等待横幅：工具正在等待用户在弹窗中确认（替代笼统的"正在思考"） */}
            {waitingPermission && (
              <div className="flex items-center gap-2 border-t border-[var(--color-risk-medium)]/40 bg-[var(--color-risk-medium)]/10 px-4 py-2 text-sm text-[var(--color-risk-medium)]">
                <ShieldAlert className="h-4 w-4 shrink-0" />
                <span>
                  等待权限确认：
                  <code className="rounded bg-[var(--color-surface-2)] px-1.5 py-0.5 text-xs">
                    {waitingPermission.tool}
                  </code>
                  <span className="ml-1">请在权限弹窗中选择允许或拒绝（超时将自动拒绝）</span>
                </span>
              </div>
            )}
            {/* 权限被拒提示（用户拒绝或超时自动拒绝，turn 结束时判定） */}
            {permissionDeniedMsg && !waitingPermission && (
              <div className="flex items-center gap-2 border-t border-[var(--color-risk-high)]/30 bg-[var(--color-risk-high)]/10 px-4 py-2 text-sm text-[var(--color-risk-high)]">
                <ShieldX className="h-4 w-4 shrink-0" />
                <span>{permissionDeniedMsg}</span>
              </div>
            )}
            {/* "运行中" 指示器：POST 请求进行中但 SSE 还未开始流式输出 */}
            {sendMessage.isPending && !isStreaming && (
              <div className="flex items-center gap-2 border-t border-[var(--color-border)] px-4 py-2 text-sm text-[var(--color-text-muted)]">
                <Loader2 className="h-4 w-4 animate-spin" />
                <span>正在思考…</span>
                {elapsedSec > 0 && <span className="text-xs">已等待 {elapsedSec}s</span>}
                {elapsedSec >= 60 && (
                  <span className="text-xs text-[var(--color-risk-medium)]">
                    （长时间无输出：可能等待权限确认，或 LLM 无响应，可点右侧停止）
                  </span>
                )}
              </div>
            )}
            <ChatInput
              onSend={handleSend}
              onCancel={handleCancel}
              isStreaming={turnBusy}
              // 运行中不禁用输入框（可提前输入），仅禁止发送（turn 完成后发送）
              sendDisabled={turnBusy}
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

      {/* 可观测性：Agent Loop 运行流程时间线面板 */}
      <TracePanel />
    </div>
  );
}

function EmptyState() {
  return (
    <div className="flex flex-1 flex-col items-center justify-center gap-6">
      <div className="gradient-accent anime-glow flex h-20 w-20 items-center justify-center rounded-3xl text-3xl font-bold text-white dark:text-[#141418]">
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
