import { useCallback, useEffect, useRef, type ReactNode } from "react";
import { AnimatePresence } from "framer-motion";
import { MessageBubble } from "./MessageBubble";
import { ScrollArea } from "../ui/scroll-area";
import type { Message, ToolResult } from "../../api/generated";
import type { ActiveTool } from "../../hooks/useChat";
import { sandboxDenyLabel } from "../../lib/message";
import type { SandboxDenyInfo as SandboxDenyInfoDto } from "../../api/generated";
import { summarizeToolContent } from "../../lib/message";
import type { ToolContent } from "../../api/generated";

interface MessageListProps {
  /** 当前会话 ID（用于"打开会话默认跳到底部"的一次性标记）。 */
  sessionId: string;
  messages: Message[] | undefined;
  streamingText: string;
  streamingReasoning: string;
  isStreaming: boolean;
  isLoading: boolean;
  activeTools: ActiveTool[];
}

export function MessageList({
  sessionId,
  messages,
  streamingText,
  streamingReasoning,
  isStreaming,
  isLoading,
  activeTools,
}: MessageListProps) {
  const bottomRef = useRef<HTMLDivElement>(null);
  // 记录已做过"打开即跳底"的会话：切换会话/重新挂载时重置并再次跳底
  const scrolledSessionRef = useRef<string | null>(null);

  // 自动滚动到底部；仅当用户本来就接近底部（<120px）时跟随，
  // 否则会强制把阅读历史中的用户拉到底部（也是"抖动"的一种来源）。
  // `force`（用户刚发送消息）时无视 nearBottom，必定跳转到底部。
  const scrollToBottom = useCallback((smooth: boolean, force = false) => {
    const bottom = bottomRef.current;
    if (!bottom) return;
    const viewport = bottom.closest(".overflow-y-auto");
    if (!viewport) {
      bottom.scrollIntoView({ behavior: smooth ? "smooth" : "auto" });
      return;
    }
    const nearBottom = viewport.scrollHeight - viewport.scrollTop - viewport.clientHeight < 120;
    if (force || nearBottom) {
      bottom.scrollIntoView({ behavior: smooth ? "smooth" : "auto" });
    }
  }, []);

  // 新消息/工具卡片变化时平滑滚动到底部（频率低，动画不被打断）。
  // 最后一条是 user 消息 = 用户刚发送（乐观更新已插入），强制跳转到底部
  // （用户反馈"新输入对话后页面不跳转到最下面"）。
  useEffect(() => {
    if (!messages?.length) return;
    if (scrolledSessionRef.current !== sessionId) {
      // 打开会话：默认跳到最新消息（用户反馈"打开对话停在开头"），无动画直跳
      scrolledSessionRef.current = sessionId;
      scrollToBottom(false, true);
      return;
    }
    const last = messages[messages.length - 1];
    const force = last?.role === "user";
    scrollToBottom(true, force);
  }, [messages, activeTools, sessionId, scrollToBottom]);

  // 流式 token 增量时**瞬时**滚动（无动画）——每 token 触发 smooth 滚动
  // 会导致滚动动画持续被打断重启，视觉上"一抖一抖"（用户反馈）
  useEffect(() => {
    scrollToBottom(false);
  }, [streamingText, scrollToBottom]);

  if (isLoading) {
    return (
      <div className="flex flex-1 items-center justify-center">
        <div className="text-sm text-[var(--color-text-muted)]">加载消息…</div>
      </div>
    );
  }

  const hasMessages = messages && messages.length > 0;
  const hasStreaming = isStreaming && streamingText.length > 0;

  if (!hasMessages && !hasStreaming && activeTools.length === 0) {
    return (
      <div className="flex flex-1 flex-col items-center justify-center gap-4 text-center">
        <div className="gradient-accent flex h-16 w-16 items-center justify-center rounded-2xl text-2xl font-bold text-white">
          m
        </div>
        <div className="space-y-1">
          <h2 className="text-lg font-semibold">开始新对话</h2>
          <p className="text-sm text-[var(--color-text-muted)]">输入消息开始与 AI 编程助手对话</p>
        </div>
      </div>
    );
  }

  return (
    <ScrollArea className="flex-1 px-4 py-4">
      <div className="mx-auto max-w-3xl space-y-3">
        <AnimatePresence initial={false}>
          {messages?.map((msg) => (
            <MessageBubble key={msg.id} message={msg} />
          ))}
        </AnimatePresence>

        {/* 工具调用卡片（本 turn 的进度，见 useChat.ts `activeTools`） */}
        {activeTools.length > 0 && <ToolCallList tools={activeTools} />}

        {/* 思考过程（reasoning/thinking 增量，默认展开；模型不支持时不出现） */}
        {streamingReasoning.length > 0 && <ReasoningBlock text={streamingReasoning} />}

        {/* 流式中暂无任何输出（模型未发文本/思考/工具）时占位，避免误以为卡死 */}
        {isStreaming &&
          streamingText.length === 0 &&
          streamingReasoning.length === 0 &&
          activeTools.length === 0 && (
            <div className="flex items-center gap-2 px-1 text-xs text-[var(--color-text-muted)]">
              <span className="streaming-cursor" />🤔 思考中…
            </div>
          )}

        {/* 流式生成中的临时消息 */}
        {hasStreaming && (
          <MessageBubble
            message={
              {
                id: "streaming",
                role: "assistant",
                content: [{ type: "text", text: streamingText }],
                tool_calls: [],
                tool_call_id: null,
                created_at: new Date().toISOString(),
                metadata: {
                  tokens: null,
                  pinned: false,
                  summarized: false,
                  source: "llm",
                  compressed_range: null,
                },
              } as Message
            }
            isStreaming
          />
        )}

        <div ref={bottomRef} />
      </div>
    </ScrollArea>
  );
}

/**
 * 思考过程块（reasoning/thinking）。
 *
 * `details` **默认展开**（`open`）：思考过程逐 token 流式更新，用户可实时
 * 看到 AI 推理（用户反馈"AI 输出栏没有显示思考过程"）。为防长思考（如
 * DeepSeek R1 上千字）逐 token 全量重渲染导致页面卡顿，`pre` 限高
 * `max-h-64` + 内部滚动——页面高度不随思考增长，无滚动抖动。
 * 用户可点击 `summary` 手动收起。思考过程为瞬态数据，刷新后不保留。
 */
function ReasoningBlock({ text }: { text: string }) {
  return (
    <details
      open
      className="rounded-lg border border-[var(--color-border)] bg-[var(--color-surface)]/40 px-3 py-2"
    >
      <summary className="cursor-pointer select-none text-xs text-[var(--color-text-muted)]">
        💭 思考过程
      </summary>
      <pre className="mt-2 max-h-64 overflow-y-auto whitespace-pre-wrap font-mono text-[11px] leading-relaxed text-[var(--color-text-muted)]">
        {text}
      </pre>
    </details>
  );
}

/**
 * 工具调用卡片列表：进行中显示 spinner，完成后显示 ✓/✗ 与结果摘要。
 * 让用户能区分"正在执行工具 / 已完成 / 失败"，而不是笼统的"思考中"。
 */
function ToolCallList({ tools }: { tools: ActiveTool[] }) {
  return (
    <div className="space-y-1.5">
      {tools.map((t) => (
        <ToolCallCard key={t.callId} tool={t} />
      ))}
    </div>
  );
}

function ToolCallCard({ tool }: { tool: ActiveTool }) {
  const { callId, tool: name, status, result } = tool;

  let icon: string;
  let iconClass: string;
  let statusLabel: string;
  switch (status) {
    case "running":
      icon = "◌";
      iconClass = "animate-spin text-[var(--color-risk-medium)]";
      statusLabel = "执行中";
      break;
    case "ok":
      icon = "✓";
      iconClass = "text-[var(--color-risk-low)]";
      statusLabel = "完成";
      break;
    case "err":
      icon = "✗";
      iconClass = "text-[var(--color-risk-high)]";
      statusLabel = "失败";
      break;
  }

  // 结果摘要（截断，失败时优先展示错误）
  const summary = summarizeToolContent(
    result && typeof result === "object" && "content" in result
      ? (result as { content?: ToolContent }).content
      : undefined,
  );

  // M-09 沙箱拒绝卡片：结构化 metadata.sandbox_denied 存在时渲染专属样式，
  // 前端只做展示（C-30 判定在后端，不可被前端绕过）
  const denied =
    result && typeof result === "object" && "metadata" in result
      ? ((result as { metadata?: { sandbox_denied?: SandboxDenyInfoDto | null } }).metadata
          ?.sandbox_denied ?? null)
      : null;

  return (
    <div
      className={`rounded-lg border px-3 py-2 ${
        denied
          ? "border-[var(--color-risk-high)]/50 bg-[var(--color-risk-high)]/5"
          : "border-[var(--color-border)] bg-[var(--color-surface)]/60"
      }`}
    >
      <div className="flex items-center gap-2 text-xs">
        <span className={`${iconClass} text-sm leading-none`}>{icon}</span>
        <span className="font-mono text-[var(--color-text)]">{name}</span>
        <span className="text-[10px] text-[var(--color-text-muted)]">{callId.slice(-6)}</span>
        {denied ? (
          <span className="ml-auto rounded bg-[var(--color-risk-high)]/15 px-1.5 py-0.5 text-[10px] font-medium text-[var(--color-risk-high)]">
            🛡 沙箱拒绝 · {sandboxDenyLabel(denied.kind)}
          </span>
        ) : (
          <span className="ml-auto text-[10px] text-[var(--color-text-muted)]">{statusLabel}</span>
        )}
      </div>
      {denied && (
        <p
          className="mt-1 line-clamp-2 break-all text-[11px] text-[var(--color-risk-high)]/90"
          title={denied.detail}
        >
          {denied.detail}
        </p>
      )}
      {/* R-05（M-11）：按工具名本地渲染结构化结果（零协议改动，前端内置 renderer）。
           fs.glob → 文件列表；task.list / plan.list → 表格；其余工具回落文本摘要。 */}
      {status === "ok" && result && renderStructuredToolResult(name, result)}
      {(!result || status !== "ok") && summary && (
        <p className="mt-1 line-clamp-2 text-[11px] text-[var(--color-text-muted)]">{summary}</p>
      )}
    </div>
  );
}

/**
 * R-05（M-11）工具结果本地渲染器：按工具名 + 内容类型投影为结构化卡片。
 * 与后端 `Tool::render_output`（`RenderIntent`）语义一致；后端未提供该工具的
 * renderer 时（理想是 MCP 三方可扩展）回落文本摘要。见 `design.md` §7。
 */
function renderStructuredToolResult(name: string, result: ToolResult): ReactNode {
  const content = result && "content" in result ? result.content : undefined;
  if (!content) return null;

  // fs.glob：文本每行一个相对路径 → 文件列表
  if (name === "fs.glob" && content.type === "text") {
    const lines = content.content.split("\n").filter((l) => l.length > 0);
    if (lines.length === 0) return null;
    return (
      <ul className="mt-1.5 space-y-0.5 border-t border-[var(--color-border)]/60 pt-1.5">
        {lines.slice(0, 20).map((line) => (
          <li key={line} className="truncate font-mono text-[11px] text-[var(--color-text-muted)]">
            📄 {line}
          </li>
        ))}
        {lines.length > 20 && (
          <li className="text-[10px] text-[var(--color-text-muted)]">… 共 {lines.length} 项</li>
        )}
      </ul>
    );
  }

  // task.list / plan.list：JSON 数组 → 表格
  if ((name === "task.list" || name === "plan.list") && content.type === "json") {
    const value = content.content as Record<string, unknown> | null;
    if (!value) return null;
    const headers =
      name === "task.list"
        ? ["id", "status", "content"]
        : ["tool", "prompt"];
    const rows = extractTableRows(name, value);
    if (rows.length === 0) return null;
    return (
      <table className="mt-1.5 w-full border-collapse border-t border-[var(--color-border)]/60 text-left text-[11px]">
        <thead>
          <tr>
            {headers.map((h) => (
              <th
                key={h}
                className="px-1.5 py-0.5 font-medium text-[var(--color-text-muted)]"
              >
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.slice(0, 50).map((row, i) => (
            <tr key={i} className="border-t border-[var(--color-border)]/40">
              {row.map((cell, j) => (
                <td key={j} className="px-1.5 py-0.5 font-mono text-[var(--color-text-muted)]">
                  {cell}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    );
  }

  return null; // 回落外层文本摘要
}

/** 从 JSON 结果提取表格行（task.list: tasks[] / plan.list: allowed_prompts[]）。 */
function extractTableRows(name: string, value: Record<string, unknown>): string[][] {
  const list =
    name === "task.list"
      ? value.tasks
      : name === "plan.list"
        ? value.allowed_prompts
        : undefined;
  if (!Array.isArray(list)) return [];
  if (name === "task.list") {
    return (list as Array<Record<string, unknown>>)
      .map((t) => [
        String(t.id ?? ""),
        String(t.status ?? ""),
        String(t.content ?? ""),
      ]);
  }
  return (list as Array<Record<string, unknown>>).map((p) => [
    String(p.tool ?? ""),
    String(p.prompt ?? ""),
  ]);
}
