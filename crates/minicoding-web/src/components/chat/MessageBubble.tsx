import { motion } from "framer-motion";
import { User, Bot, Wrench } from "lucide-react";
import ReactMarkdown from "react-markdown";
import type { Message, ToolCall } from "../../api/generated";
import { cn, formatTime } from "../../lib/utils";
import { extractText } from "../../lib/message";

const ROLE_CONFIG = {
  user: {
    icon: User,
    label: "你",
    bg: "bg-[var(--color-accent)]/8",
    iconBg: "gradient-accent",
  },
  assistant: {
    icon: Bot,
    label: "AI",
    bg: "bg-[var(--color-surface-2)]/50",
    iconBg: "bg-[var(--color-surface)]",
  },
  system: {
    icon: Wrench,
    label: "系统",
    bg: "bg-amber-500/5",
    iconBg: "bg-amber-500/20",
  },
  tool: {
    icon: Wrench,
    label: "工具",
    bg: "bg-[var(--color-surface-2)]/50",
    iconBg: "bg-[var(--color-surface)]",
  },
} as const;

/** 工具输入参数摘要：优先取关键字段（path/command/target 等），否则 JSON 截断。 */
function summarizeInput(input: unknown): string {
  if (typeof input !== "object" || input === null || Array.isArray(input)) {
    const json = JSON.stringify(input);
    return json.length > 60 ? `${json.slice(0, 60)}…` : json;
  }
  const record = input as Record<string, unknown>;
  for (const key of ["path", "command", "target", "query", "pattern"] as const) {
    const v = record[key];
    if (typeof v === "string" && v.trim()) {
      return v.length > 48 ? `${key}=${v.slice(0, 48)}…` : `${key}=${v}`;
    }
  }
  const json = JSON.stringify(input);
  return json.length > 80 ? `${json.slice(0, 80)}…` : json;
}

/** 工具调用列表（`Message.tool_calls`，LLM 发出的工具请求，见 api.md §2.4）。 */
function ToolCallList({ calls }: { calls: ToolCall[] }) {
  return (
    <div className="space-y-1.5">
      {calls.map((tc) => (
        <div
          key={tc.id}
          className="flex items-start gap-2 rounded-md border border-[var(--color-border)] bg-[var(--color-bg)] px-2.5 py-2"
        >
          <Wrench className="mt-0.5 h-3.5 w-3.5 shrink-0 text-[var(--color-accent)]" />
          <div className="min-w-0">
            <span className="text-xs font-medium text-[var(--color-text)]">{tc.name}</span>
            <span className="ml-2 text-[11px] text-[var(--color-text-muted)]">
              {summarizeInput(tc.input)}
            </span>
          </div>
        </div>
      ))}
    </div>
  );
}

export function MessageBubble({ message, isStreaming }: { message: Message; isStreaming?: boolean }) {
  const config = ROLE_CONFIG[message.role] ?? ROLE_CONFIG.assistant;
  const Icon = config.icon;
  const text = extractText(message);
  const toolCalls = message.tool_calls ?? [];

  return (
    <motion.div
      initial={{ opacity: 0, y: 8 }}
      animate={{ opacity: 1, y: 0 }}
      transition={{ duration: 0.15 }}
      className={cn("flex gap-3 rounded-xl px-4 py-3", config.bg)}
    >
      {/* Avatar */}
      <div
        className={cn(
          "flex h-8 w-8 shrink-0 items-center justify-center rounded-lg",
          config.iconBg,
        )}
      >
        <Icon className="h-4 w-4 text-[var(--color-text)]" />
      </div>

      {/* Content */}
      <div className="flex-1 space-y-1 overflow-hidden">
        <div className="flex items-center gap-2">
          <span className="text-xs font-medium text-[var(--color-text-muted)]">
            {config.label}
          </span>
          <span className="text-[10px] text-[var(--color-text-muted)]/60">
            {formatTime(message.created_at)}
          </span>
        </div>
        <div className="prose prose-invert max-w-none text-sm leading-relaxed">
          {text ? (
            <ReactMarkdown
              components={{
                code: ({ children, className }) => (
                  <code
                    className={cn(
                      "rounded px-1.5 py-0.5 text-xs",
                      className?.includes("language-")
                        ? "block bg-[var(--color-bg)] p-3"
                        : "bg-[var(--color-surface)]",
                    )}
                  >
                    {children}
                  </code>
                ),
              }}
            >
              {text}
            </ReactMarkdown>
          ) : toolCalls.length > 0 ? (
            <ToolCallList calls={toolCalls} />
          ) : (
            <span className="text-[var(--color-text-muted)] italic">
              {message.role === "assistant"
                ? "（无文本内容：工具请求被拒或模型未输出）"
                : "（无文本内容）"}
            </span>
          )}
          {isStreaming && <span className="streaming-cursor" />}
        </div>
      </div>
    </motion.div>
  );
}
