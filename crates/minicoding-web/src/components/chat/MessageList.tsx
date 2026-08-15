import { useCallback, useEffect, useRef } from "react";
import { AnimatePresence } from "framer-motion";
import { MessageBubble } from "./MessageBubble";
import { ScrollArea } from "../ui/scroll-area";
import type { Message } from "../../api/generated";
import type { ActiveTool } from "../../hooks/useChat";
import { summarizeToolContent } from "../../lib/message";
import type { ToolContent } from "../../api/generated";

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
    const last = messages?.[messages.length - 1];
    const force = last?.role === "user";
    scrollToBottom(true, force);
  }, [messages, activeTools, scrollToBottom]);

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

  return (
    <div className="rounded-lg border border-[var(--color-border)] bg-[var(--color-surface)]/60 px-3 py-2">
      <div className="flex items-center gap-2 text-xs">
        <span className={`${iconClass} text-sm leading-none`}>{icon}</span>
        <span className="font-mono text-[var(--color-text)]">{name}</span>
        <span className="text-[10px] text-[var(--color-text-muted)]">{callId.slice(-6)}</span>
        <span className="ml-auto text-[10px] text-[var(--color-text-muted)]">{statusLabel}</span>
      </div>
      {summary && (
        <p className="mt-1 line-clamp-2 text-[11px] text-[var(--color-text-muted)]">{summary}</p>
      )}
    </div>
  );
}
