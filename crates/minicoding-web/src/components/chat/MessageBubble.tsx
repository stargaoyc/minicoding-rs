import { useState } from "react";
import { motion } from "framer-motion";
import { ChevronDown, ChevronUp, User, Bot, Wrench } from "lucide-react";
import ReactMarkdown from "react-markdown";
import type { Message, ToolCall } from "../../api/generated";
import { cn, formatTime } from "../../lib/utils";
import { extractText, extractToolResultSummary, sandboxDenyLabel } from "../../lib/message";

/** 工具结果超过该长度默认折叠（用户反馈"工具调用结果太长占屏"，可展开查看全文）。 */
const TOOL_RESULT_COLLAPSE_THRESHOLD = 300;

/**
 * 可折叠长文本：超阈值默认折叠（显示前段 + 展开按钮），点击展开/收起。
 *
 * 语义：折叠态展示开头截断 + "展开 N 字符"按钮；展开态全文 + "收起"按钮。
 * 用于工具结果（cat/glob/diff 等大输出）与错误详情。
 */
function CollapsibleText({ text, error }: { text: string; error?: boolean }) {
  const [expanded, setExpanded] = useState(false);
  const needsCollapse = text.length > TOOL_RESULT_COLLAPSE_THRESHOLD;
  const shown = needsCollapse && !expanded ? `${text.slice(0, TOOL_RESULT_COLLAPSE_THRESHOLD)}…` : text;
  const ExpandIcon = expanded ? ChevronUp : ChevronDown;

  return (
    <div className="min-w-0">
      <span
        className={cn("whitespace-pre-wrap break-all", expanded && "block")}
        onClick={needsCollapse ? () => setExpanded((v) => !v) : undefined}
        role={needsCollapse ? "button" : undefined}
        tabIndex={needsCollapse ? 0 : undefined}
        onKeyDown={
          needsCollapse
            ? (e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault();
                  setExpanded((v) => !v);
                }
              }
            : undefined
        }
      >
        {shown}
      </span>
      {needsCollapse && (
        <button
          type="button"
          onClick={() => setExpanded((v) => !v)}
          className={cn(
            "mt-1 inline-flex items-center gap-1 text-[11px] font-medium",
            error ? "text-[var(--color-risk-high)]" : "text-[var(--color-accent)]",
            "hover:underline",
          )}
        >
          <ExpandIcon className="h-3 w-3" />
          {expanded ? "收起" : `展开全文（${text.length} 字符）`}
        </button>
      )}
    </div>
  );
}

const ROLE_CONFIG = {
  user: {
    icon: User,
    label: "你",
    bg: "bg-[var(--color-accent)]/10",
    iconBg: "gradient-accent",
  },
  assistant: {
    icon: Bot,
    label: "AI",
    bg: "bubble-ai",
    iconBg: "gradient-accent",
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

export function MessageBubble({
  message,
  isStreaming,
}: {
  message: Message;
  isStreaming?: boolean;
}) {
  const config = ROLE_CONFIG[message.role] ?? ROLE_CONFIG.assistant;
  const Icon = config.icon;
  const text = extractText(message);
  const toolCalls = message.tool_calls ?? [];

  // 工具结果消息（role=tool）：无任何可显示内容（如纯 JSON `{}`）时不渲染
  // 空白气泡（用户反馈"工具调用输出是空的，不如不显示"）
  const toolResult = message.role === "tool" ? extractToolResultSummary(message) : null;
  // M-09 沙箱拒绝结构化信息（持久化在 tool_result 块 metadata 中）
  const sandboxDenied =
    message.role === "tool"
      ? (message.content.find(
          (b): b is Extract<typeof b, { type: "tool_result" }> => b.type === "tool_result",
        )?.metadata?.sandbox_denied ?? null)
      : null;
  if (message.role === "tool" && !toolResult?.text && !sandboxDenied) {
    return null;
  }

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
        <Icon className="h-4 w-4 text-[var(--color-text)] dark:text-[#141418]" />
      </div>

      {/* Content */}
      <div className="flex-1 space-y-1 overflow-hidden">
        <div className="flex items-center gap-2">
          <span className="text-xs font-medium text-[var(--color-text-muted)]">{config.label}</span>
          <span className="text-[10px] text-[var(--color-text-muted)]/60">
            {formatTime(message.created_at)}
          </span>
        </div>
        <div className="prose prose-invert max-w-none text-sm leading-relaxed">
          {sandboxDenied ? (
            <div className="rounded-md border border-[var(--color-risk-high)]/40 bg-[var(--color-risk-high)]/5 px-2.5 py-2 text-xs">
              <span className="mr-1.5 font-medium text-[var(--color-risk-high)]">
                🛡 沙箱拒绝 · {sandboxDenyLabel(sandboxDenied.kind)}
              </span>
              <span
                className="whitespace-pre-wrap break-all text-[var(--color-risk-high)]/90"
                title={sandboxDenied.detail}
              >
                {sandboxDenied.detail}
              </span>
            </div>
          ) : text ? (
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
          ) : message.role === "tool" && toolResult ? (
            <div
              className={cn(
                "rounded-md border px-2.5 py-2 text-xs",
                toolResult.isError
                  ? "border-[var(--color-risk-high)]/40 bg-[var(--color-risk-high)]/5 text-[var(--color-risk-high)]"
                  : "border-[var(--color-border)] bg-[var(--color-bg)] text-[var(--color-text-muted)]",
              )}
            >
              <span className="mr-1.5 text-[var(--color-text-muted)]">
                {toolResult.isError ? "✗" : "✓"}
              </span>
              {/* R9 P3-1：长工具结果默认折叠（可展开全文），避免大输出占满整个聊天区 */}
              <CollapsibleText text={toolResult.text} error={toolResult.isError} />
            </div>
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
