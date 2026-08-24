/**
 * EventDto 运行时校验（Zod，AGENTS.md §8.4 完整落地——2026-08-23 遗留#4）。
 *
 * SSE 数据必须通过本 schema 才进入业务层，防止后端 schema 漂移导致
 * 运行时 undefined。已知 EventKind 变体与 minicoding-protocol event.rs 对齐。
 */
import { z } from "zod";

const KNOWN_EVENT_KINDS = z.enum([
  "token",
  "message_appended",
  "turn_end",
  "task_updated",
  "hook_run",
  "permission_requested",
  "permission_resolved",
  "permission_mode_changed",
  "file_undone",
  "config_changed",
  "tool_call_started",
  "tool_call_finished",
  "compress",
  "turn_streaming_started",
  "rehydrate_required",
]);

export const eventDtoSchema = z.object({
  type: KNOWN_EVENT_KINDS,
  seq: z.number().optional(),
}).passthrough();

export type ParsedEventDto = z.infer<typeof eventDtoSchema> & Record<string, unknown>;

/** 校验并返回类型化事件；失败返回 `null`（调用方 warn 后丢弃）。 */
export function isEventDto(value: unknown): value is { type: string; seq?: number } & Record<string, unknown> {
  return eventDtoSchema.safeParse(value).success;
}
