import type { EventDto } from "../api/client";
import type { ToolResult } from "../api/generated";

/** 进行中的工具调用（供 UI 渲染工具卡片，见 AGENTS.md §8.5 流式状态）。 */
export interface ActiveTool {
  callId: string;
  tool: string;
  status: "running" | "ok" | "err";
  result?: ToolResult;
}

/** 等待用户确认的权限请求（`permission_requested` → resolved/turn_end 之间）。 */
export interface WaitingPermission {
  id: string;
  tool: string;
  summary: string;
  risk: "low" | "medium" | "high";
}

/**
 * SSE 事件流的纯状态（M-14/R-10：从 `useSSEStream` 抽出，供 record/replay
 * 快照测试——同一事件序列在任何环境下归约出相同终态）。
 */
export interface ChatStreamState {
  streamingText: string;
  streamingReasoning: string;
  isStreaming: boolean;
  activeTools: ActiveTool[];
  waitingPermission: WaitingPermission | null;
  permissionDeniedMsg: string | null;
}

export const initialChatState: ChatStreamState = {
  streamingText: "",
  streamingReasoning: "",
  isStreaming: false,
  activeTools: [],
  waitingPermission: null,
  permissionDeniedMsg: null,
};

/**
 * 归约产生的副作用（由 hook 层映射到 TanStack Query invalidate，纯函数不触网）。
 * - `messages`：消息快照失效（message_appended / tool_call_finished 等）
 * - `workspace`：工作区树/预览/diff 失效（文件改动后，W-11）
 * - `sessions`：会话列表失效（task 计数刷新）
 */
export type ChatEffect = "invalidate-messages" | "invalidate-workspace" | "invalidate-sessions";

/**
 * 归约单条 SSE 事件到下一状态（纯函数，M-14 record/replay 的核心）。
 *
 * 逻辑与原 `useSSEStream.handleEvent` 一致：
 * - `token`/`reasoning_delta` 增量追加；
 * - `message_appended` 清空瞬态渲染并失效消息快照；
 * - `turn_end` 无条件清空本 turn 瞬态（interrupted 残留清理），未决权限提示超时拒绝；
 * - 工具卡片 running → ok/err；完成后失效消息与工作区缓存；
 * - 权限请求弹窗联动 / resolved 后清除（deny 时留提示）。
 */
export function applyChatEvent(
  state: ChatStreamState,
  event: EventDto,
): { state: ChatStreamState; effects: ChatEffect[] } {
  const effects: ChatEffect[] = [];
  let next = state;
  const set = (patch: Partial<ChatStreamState>) => {
    next = { ...next, ...patch };
  };

  switch (event.type) {
    case "turn_streaming_started":
      set({
        streamingText: "",
        streamingReasoning: "",
        isStreaming: true,
        activeTools: [],
        waitingPermission: null,
        permissionDeniedMsg: null,
      });
      break;
    case "token":
      set({ streamingText: next.streamingText + event.text });
      break;
    case "reasoning_delta":
      set({ streamingReasoning: next.streamingReasoning + event.text });
      break;
    case "message_appended":
      set({ streamingText: "", streamingReasoning: "", isStreaming: false });
      effects.push("invalidate-messages");
      break;
    case "turn_end": {
      set({
        isStreaming: false,
        // 无条件清空本 turn 的瞬态渲染：interrupted（用户终止）时后端不会补发
        // message_appended 之外的清理事件，残留的流式文本/工具卡片会一直停在
        // 列表最底部。无条件清空最安全。
        streamingText: "",
        streamingReasoning: "",
        activeTools: [],
      });
      // 权限请求未获响应（后端默认 300s 超时自动 Deny，不发 resolved 事件）：
      // 在 turn 结束时提示原因，避免"静默失败"
      if (event.stop_reason !== "interrupted") {
        const w = next.waitingPermission;
        if (w) {
          set({
            permissionDeniedMsg: `权限请求未及时确认，已自动拒绝：${w.tool}（超过响应时限）`,
            waitingPermission: null,
          });
        }
      }
      break;
    }
    case "tool_call_started":
      set({
        activeTools: [
          ...next.activeTools.filter((t) => t.callId !== event.call_id),
          { callId: event.call_id, tool: event.tool, status: "running" },
        ],
      });
      break;
    case "tool_call_finished": {
      set({
        activeTools: next.activeTools.map((t) =>
          t.callId === event.call_id
            ? {
                ...t,
                status: event.result.is_error ? ("err" as const) : ("ok" as const),
                result: event.result,
              }
            : t,
        ),
      });
      // 工具完成后刷新消息 + 工作区（文件改动后树/预览/diff 失效，W-11）
      effects.push("invalidate-messages", "invalidate-workspace");
      break;
    }
    case "permission_requested":
      set({
        waitingPermission: {
          id: event.id,
          tool: event.tool,
          summary: event.summary,
          risk: event.risk,
        },
        permissionDeniedMsg: null,
      });
      break;
    case "permission_resolved": {
      const w = next.waitingPermission;
      if (w && w.id === event.id) {
        // Decision = "allow" | { deny: string }
        set({
          waitingPermission: null,
          ...(typeof event.decision === "object"
            ? { permissionDeniedMsg: `权限请求已被拒绝：${w.tool}（${event.decision.deny}）` }
            : {}),
        });
      }
      break;
    }
    case "task_updated":
      effects.push("invalidate-sessions");
      break;
    case "permission_mode_changed":
    case "session_created":
    case "config_changed":
    case "sessions_listed":
    case "session_retrieved":
    case "command_error":
      effects.push("invalidate-messages");
      break;
  }
  return { state: next, effects };
}
