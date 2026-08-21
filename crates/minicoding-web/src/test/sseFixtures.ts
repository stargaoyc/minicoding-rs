/**
 * 录制的 SSE 事件序列 fixture（M-14/R-10）。
 *
 * 覆盖设计文档要求的关键路径：创建会话→发消息→流式渲染→权限确认→沙箱拒绝卡片。
 * 事件结构与 server `sse.rs` 的 `EventDto` wire 格式一致（snake_case 判别字段，
 * 含单调递增 `seq`——EventRecord 序号）。
 */
import type { EventDto } from "../api/client";
import { makeMessage } from "./handlers";

/** 分布式 Omit：EventDto 是交集类型，需逐成员去掉 `seq`。 */
type WithoutSeq<T> = T extends unknown ? Omit<T, "seq"> : never;

/** wire 层事件带单调 seq（EventRecord）；fixture 按顺序编号。 */
let seq = 0;
function ev(e: WithoutSeq<EventDto>): EventDto {
  return { ...e, seq: (seq += 1) } as EventDto;
}

const assistantMsg = makeMessage({
  id: "msg-asst-1",
  role: "assistant",
  content: [{ type: "text", text: "你好，这是回复。" }],
});

/** 场景 1：纯文本流式 turn（发消息 → token 增量 → 落盘 → 结束）。 */
export const happyTurn: EventDto[] = [
  ev({ type: "turn_streaming_started" }),
  ev({ type: "token", text: "你好" }),
  ev({ type: "token", text: "，这是" }),
  ev({ type: "token", text: "回复。" }),
  ev({ type: "message_appended", message: assistantMsg }),
  ev({ type: "turn_end", stop_reason: "end_turn" }),
];

/** 场景 2：权限确认流（工具触发 Ask → 用户 allow → 工具完成 → 继续输出）。 */
export const permissionFlow: EventDto[] = [
  ev({ type: "turn_streaming_started" }),
  ev({ type: "tool_call_started", call_id: "call-1", tool: "fs.write" }),
  ev({
    type: "permission_requested",
    id: "perm-1",
    tool: "fs.write",
    summary: "写入文件 src/main.rs",
    risk: "medium",
  }),
  ev({ type: "permission_resolved", id: "perm-1", decision: "allow" }),
  ev({
    type: "tool_call_finished",
    call_id: "call-1",
    result: {
      content: { type: "text", content: "written" },
      is_error: false,
      metadata: {
        elapsed: { secs: 0, nanos: 0 },
        bytes: 7,
        truncated: false,
        sandbox_denied: null,
      },
    },
  }),
  ev({ type: "token", text: "已写入。" }),
  ev({
    type: "message_appended",
    message: makeMessage({
      id: "msg-asst-2",
      role: "assistant",
      content: [{ type: "text", text: "已写入。" }],
    }),
  }),
  ev({ type: "turn_end", stop_reason: "end_turn" }),
];

/** M-09 沙箱拒绝结果（结构化 metadata.sandbox_denied，syscall_blocked）。 */
const deniedResult = (detail: string) => ({
  content: { type: "text" as const, content: detail },
  is_error: true,
  metadata: {
    elapsed: { secs: 0, nanos: 0 },
    bytes: detail.length,
    truncated: false,
    sandbox_denied: { kind: { kind: "syscall_blocked" as const, syscall: detail }, detail },
  },
});

/** 场景 3：沙箱拒绝（连续 3 次触发软熔断提醒 → 模型放弃 → 正常结束）。 */
export const sandboxDenied: EventDto[] = [
  ev({ type: "turn_streaming_started" }),
  ev({ type: "tool_call_started", call_id: "call-d1", tool: "shell.run" }),
  ev({
    type: "tool_call_finished",
    call_id: "call-d1",
    result: deniedResult("sandbox denied (EPERM): Operation not permitted"),
  }),
  ev({ type: "tool_call_started", call_id: "call-d2", tool: "shell.run" }),
  ev({
    type: "tool_call_finished",
    call_id: "call-d2",
    result: deniedResult("sandbox denied (EPERM): Operation not permitted"),
  }),
  ev({ type: "tool_call_started", call_id: "call-d3", tool: "shell.run" }),
  ev({
    type: "tool_call_finished",
    call_id: "call-d3",
    result: deniedResult("sandbox denied (EPERM): Operation not permitted"),
  }),
  ev({ type: "token", text: "沙箱限制了该操作，我换一种方式。" }),
  ev({
    type: "message_appended",
    message: makeMessage({
      id: "msg-asst-3",
      role: "assistant",
      content: [{ type: "text", text: "沙箱限制了该操作，我换一种方式。" }],
    }),
  }),
  ev({ type: "turn_end", stop_reason: "end_turn" }),
];
