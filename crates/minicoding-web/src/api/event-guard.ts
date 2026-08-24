/**
 * EventDto 运行时守卫（AGENTS.md §8.4 等价实现，2026-08-23 审查遗留#4）。
 *
 * Zod 依赖待评审引入前，以手写 discriminated-union 校验兜底：SSE 数据必须
 * 通过本守卫才进入业务层，防止后端 schema 漂移静默产生 undefined。
 * 已知 EventKind 变体集合与 minicoding-protocol event.rs 对齐。
 */
const KNOWN_EVENT_KINDS = new Set([
  "token", "message_appended", "turn_end", "task_updated", "hook_run",
  "permission_requested", "permission_resolved", "permission_mode_changed",
  "file_undone", "config_changed", "tool_call_started", "tool_call_finished",
  "compress", "turn_streaming_started", "rehydrate_required",
]);

export function isEventDto(value: unknown): value is { type: string; seq?: number } & Record<string, unknown> {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Record<string, unknown>;
  return typeof v.type === "string" && KNOWN_EVENT_KINDS.has(v.type);
}
