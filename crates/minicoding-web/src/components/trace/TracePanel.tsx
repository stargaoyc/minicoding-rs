/**
 * 可观测性时间线面板（R10 前端全流程可观测）。
 *
 * 展示 agent loop 的完整运行流程（step 边界、工具调用含 input、思考过程、
 * 权限请求/决策、消息落盘、turn 边界），按事件类型着色，点击条目展开原始载荷。
 */
import { useMemo, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { Activity, X, Trash2 } from "lucide-react";
import { useTraceStore } from "../../stores/trace";
import { Button } from "../ui/button";
import { cn } from "../../lib/utils";

/** 事件类型 → 颜色样式（时间线视觉分区）。 */
const TYPE_COLOR: Record<string, string> = {
  turn_streaming_started: "border-l-[var(--color-accent)]",
  turn_end: "border-l-[var(--color-accent)]",
  step_started: "border-l-[var(--color-accent-hover)]",
  step_ended: "border-l-[var(--color-accent-hover)]",
  tool_call_started: "border-l-[var(--color-risk-medium)]",
  tool_call_finished: "border-l-[var(--color-risk-medium)]",
  permission_requested: "border-l-[var(--color-risk-high)]",
  permission_resolved: "border-l-[var(--color-risk-high)]",
  reasoning_delta: "border-l-[var(--color-text-muted)]",
  token: "border-l-[var(--color-text-muted)]",
  message_appended: "border-l-[var(--color-risk-low)]",
};

export function TracePanel() {
  const { entries, open, setOpen, clear } = useTraceStore();
  const [expanded, setExpanded] = useState<number | null>(null);
  const [filter, setFilter] = useState<string>("all");

  const types = useMemo(
    () => ["all", ...Array.from(new Set(entries.map((e) => e.type)))],
    [entries],
  );
  const filtered = useMemo(
    () => (filter === "all" ? entries : entries.filter((e) => e.type === filter)),
    [entries, filter],
  );

  return (
    <AnimatePresence>
      {open && (
        <motion.div
          initial={{ x: "100%" }}
          animate={{ x: 0 }}
          exit={{ x: "100%" }}
          transition={{ duration: 0.2 }}
          className="fixed right-0 top-0 z-40 flex h-screen w-[420px] max-w-[90vw] flex-col border-l border-[var(--color-border)] bg-[var(--color-surface)]"
        >
      {/* Header */}
      <div className="flex items-center gap-2 border-b border-[var(--color-border)] px-4 py-3">
        <Activity className="h-4 w-4 text-[var(--color-accent-hover)]" />
        <span className="text-sm font-semibold">运行流程（Agent Loop 回放）</span>
        <span className="ml-auto text-[10px] text-[var(--color-text-muted)]">
          {entries.length} 事件
        </span>
        <Button variant="ghost" size="sm" onClick={clear} title="清空日志">
          <Trash2 className="h-3.5 w-3.5" />
        </Button>
        <Button variant="ghost" size="sm" onClick={() => setOpen(false)} title="关闭">
          <X className="h-4 w-4" />
        </Button>
      </div>

      {/* Type filter */}
      <div className="flex flex-wrap gap-1 border-b border-[var(--color-border)] px-4 py-2">
        {types.slice(0, 12).map((t) => (
          <button
            key={t}
            type="button"
            onClick={() => setFilter(t)}
            className={cn(
              "rounded-full px-2 py-0.5 text-[10px] transition-colors",
              filter === t
                ? "bg-[var(--color-accent)] text-white"
                : "bg-[var(--color-surface-2)] text-[var(--color-text-muted)] hover:bg-[var(--color-border)]",
            )}
          >
            {t === "all" ? "全部" : t}
          </button>
        ))}
      </div>

      {/* Timeline */}
      <div className="flex-1 overflow-y-auto px-4 py-3">
        {filtered.length === 0 ? (
          <div className="flex h-full items-center justify-center text-sm text-[var(--color-text-muted)]">
            暂无事件（发消息后自动记录）
          </div>
        ) : (
          <div className="relative space-y-1">
            {/* 时间轴线 */}
            <div className="absolute bottom-2 left-[7px] top-2 w-px bg-[var(--color-border)]" />
            {filtered.map((e) => {
              const isExpanded = expanded === e.seq;
              return (
                <div
                  key={e.seq}
                  className={cn(
                    "relative ml-4 cursor-pointer rounded-md border-l-2 py-1 pl-2 pr-1 transition-colors hover:bg-[var(--color-surface-2)]/50",
                    TYPE_COLOR[e.type] ?? "border-l-[var(--color-border)]",
                  )}
                  onClick={() => setExpanded(isExpanded ? null : e.seq)}
                >
                  <div className="flex items-center gap-1.5 text-[11px] leading-tight">
                    <span className="font-medium text-[var(--color-text)]">{e.summary}</span>
                  </div>
                  <div className="flex items-center gap-2 text-[9px] text-[var(--color-text-muted)]/70">
                    <span>#{e.seq}</span>
                    <span>{new Date(e.ts).toLocaleTimeString()}</span>
                    <span className="font-mono">{e.type}</span>
                  </div>
                  {isExpanded && e.detail && (
                    <pre className="mt-1 max-h-48 overflow-auto whitespace-pre-wrap rounded bg-[var(--color-bg)] p-2 font-mono text-[10px] leading-relaxed text-[var(--color-text-muted)]">
                      {e.detail}
                    </pre>
                  )}
                </div>
              );
            })}
          </div>
        )}
      </div>
        </motion.div>
      )}
    </AnimatePresence>
  );
}
