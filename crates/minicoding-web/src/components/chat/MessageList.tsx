import { useEffect, useRef } from "react";
import { AnimatePresence } from "framer-motion";
import { MessageBubble } from "./MessageBubble";
import { ScrollArea } from "../ui/scroll-area";
import type { Message } from "../../api/generated";
import type { ActiveTool } from "../../hooks/useChat";

interface MessageListProps {
  messages: Message[] | undefined;
  streamingText: string;
  streamingReasoning: string;
  isStreaming: boolean;
  isLoading: boolean;
  activeTools: ActiveTool[];
}

export function MessageList({
  messages,
  streamingText,
  streamingReasoning,
  isStreaming,
  isLoading,
  activeTools,
}: MessageListProps) {
  const bottomRef = useRef<HTMLDivElement>(null);

  // 新消息或流式增量时自动滚动到底部
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages, streamingText, activeTools]);

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
          <p className="text-sm text-[var(--color-text-muted)]">
            输入消息开始与 AI 编程助手对话
          </p>
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

        {/* 思考过程（reasoning/thinking 增量，瞬态展示；模型不支持时不出现） */}
        {streamingReasoning.length > 0 && <ReasoningBlock text={streamingReasoning} />}

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
 * `details` 原生折叠：默认展开（`open`），用户手动收起后流式更新不会强制展开
 *（非受控，React 保留 DOM 开关状态）。思考过程为瞬态数据，刷新后不保留。
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

/**
 * 从 `ToolResult` 提取摘要文本（`ToolContent` 联合类型的文本/JSON 分支）。
 */
function extractToolSummary(result: unknown): string {
  if (!result || typeof result !== "object") return "";
  const r = result as { content?: unknown; is_error?: boolean };
  const content = r.content;
  if (content && typeof content === "object" && "content" in (content as object)) {
    const c = content as { type?: string; content?: unknown };
    if (c.type === "text" && typeof c.content === "string") return c.content;
    if (c.type === "mixed" && Array.isArray(c.content)) {
      return c.content
        .map((part) => extractToolSummary({ content: part, is_error: false } as never))
        .join("\n");
    }
    if (c.type === "json") {
      try {
        return JSON.stringify(c.content).slice(0, 200);
      } catch {
        return "[json]";
      }
    }
  }
  return "";
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
  const summary = extractToolSummary(result);

  return (
    <div className="rounded-lg border border-[var(--color-border)] bg-[var(--color-surface)]/60 px-3 py-2">
      <div className="flex items-center gap-2 text-xs">
        <span className={`${iconClass} text-sm leading-none`}>{icon}</span>
        <span className="font-mono text-[var(--color-text)]">{name}</span>
        <span className="text-[10px] text-[var(--color-text-muted)]">{callId.slice(-6)}</span>
        <span className="ml-auto text-[10px] text-[var(--color-text-muted)]">{statusLabel}</span>
      </div>
      {summary && (
        <p className="mt-1 line-clamp-2 text-[11px] text-[var(--color-text-muted)]">
          {summary}
        </p>
      )}
    </div>
  );
}
