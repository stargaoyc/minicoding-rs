import { motion } from "framer-motion";
import { User, Bot, Wrench } from "lucide-react";
import ReactMarkdown from "react-markdown";
import type { Message } from "../../api/generated";
import { cn } from "../../lib/utils";
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

export function MessageBubble({ message, isStreaming }: { message: Message; isStreaming?: boolean }) {
  const config = ROLE_CONFIG[message.role] ?? ROLE_CONFIG.assistant;
  const Icon = config.icon;
  const text = extractText(message);

  return (
    <motion.div
      layout
      initial={{ opacity: 0, y: 8 }}
      animate={{ opacity: 1, y: 0 }}
      transition={{ duration: 0.2 }}
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
            {new Date(message.created_at).toLocaleTimeString("zh-CN", {
              hour: "2-digit",
              minute: "2-digit",
            })}
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
          ) : (
            <span className="text-[var(--color-text-muted)] italic">（无文本内容）</span>
          )}
          {isStreaming && <span className="streaming-cursor" />}
        </div>
      </div>
    </motion.div>
  );
}
