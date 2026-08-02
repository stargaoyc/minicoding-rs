import { useEffect, useRef } from "react";
import { AnimatePresence } from "framer-motion";
import { MessageBubble } from "./MessageBubble";
import { ScrollArea } from "../ui/scroll-area";
import type { Message } from "../../api/generated";

interface MessageListProps {
  messages: Message[] | undefined;
  streamingText: string;
  isStreaming: boolean;
  isLoading: boolean;
}

export function MessageList({ messages, streamingText, isStreaming, isLoading }: MessageListProps) {
  const bottomRef = useRef<HTMLDivElement>(null);

  // 新消息或流式增量时自动滚动到底部
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages, streamingText]);

  if (isLoading) {
    return (
      <div className="flex flex-1 items-center justify-center">
        <div className="text-sm text-[var(--color-text-muted)]">加载消息…</div>
      </div>
    );
  }

  const hasMessages = messages && messages.length > 0;
  const hasStreaming = isStreaming && streamingText.length > 0;

  if (!hasMessages && !hasStreaming) {
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
