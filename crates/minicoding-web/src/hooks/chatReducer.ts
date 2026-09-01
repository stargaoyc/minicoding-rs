import type { EventDto } from "../api/client";
import type { JsonValue } from "../api/generated/bindings/serde_json/JsonValue";
import type { ToolResult } from "../api/generated";

/** 进行中的工具调用（供 UI 渲染工具卡片，见 AGENTS.md §8.5 流式状态）。 */
export interface ActiveTool {
  callId: string;
  tool: string;
  /** R10：工具输入参数（`tool_call_started` 事件携带），用于卡片显示命令/路径等。 */
  input?: JsonValue;
  status: "running" | "ok" | "err";
  result?: ToolResult;
}

/** 等待用户确认的权限请求（`permission_requested` → resolved/turn_end 之间）。 */
export interface WaitingPermission {
  id: string;
  tool: string;
  summary: string;
  risk: "low" | "medium" | "high";
  /** R4（FE4-1）：prompt 提供的决策选项（`GET /permissions/pending` 快照携带；
   * 实时 SSE 事件 DTO 暂无此字段，缺失时前端按全按钮渲染、core 折叠兜底）。 */
  options?: string[];
}

/**
 * SSE 事件流的纯状态（M-14/R-10：从 `useSSEStream` 抽出，供 record/replay
 * 快照测试——同一事件序列在任何环境下归约出相同终态）。
 */
export interface ChatStreamState {
  streamingText: string;
  streamingReasoning: string;
  /** R10：已完成的思考过程存档（每轮 turn 的 reasoning 留存，不随 message_appended 清空）。
   *  元素 = 一轮的完整思考文本。此前 reasoning 是瞬态，消息落盘即消失。 */
  reasoningHistory: string[];
  isStreaming: boolean;
  activeTools: ActiveTool[];
  waitingPermission: WaitingPermission | null;
  permissionDeniedMsg: string | null;
  /** Plan 模式是否激活（`permission_mode_changed` 驱动，遗留：Plan 可视化） */
  planActive: boolean;
}

export const initialChatState: ChatStreamState = {
  streamingText: "",
  streamingReasoning: "",
  reasoningHistory: [],
  isStreaming: false,
  activeTools: [],
  waitingPermission: null,
  permissionDeniedMsg: null,
  planActive: false,
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
      // R10：思考过程留存——把本 turn 的 reasoning 归档到 history 再清空瞬态
      //（此前 streamingReasoning 直接清空，思考过程一闪即消失）。
      set({
        streamingText: "",
        streamingReasoning: "",
        reasoningHistory:
          next.streamingReasoning.trim() && !next.reasoningHistory.includes(next.streamingReasoning)
            ? [...next.reasoningHistory, next.streamingReasoning]
            : next.reasoningHistory,
        isStreaming: false,
      });
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
        // R10：思考过程留存——turn_end 时兜底归档（若 message_appended 已归档则去重）
        reasoningHistory:
          next.streamingReasoning.trim() && !next.reasoningHistory.includes(next.streamingReasoning)
            ? [...next.reasoningHistory, next.streamingReasoning]
            : next.reasoningHistory,
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
      } else {
        // R8 FE-12：中断后清理 waitingPermission 横幅（此前残留，UI 一直显示
        // "等待权限确认"而 turn 已被取消；用户无操作可消除该状态）。
        set({ waitingPermission: null });
      }
      break;
    }
    case "tool_call_started":
      set({
        activeTools: [
          ...next.activeTools.filter((t) => t.callId !== event.call_id),
          {
            callId: event.call_id,
            tool: event.tool,
            input: event.input,
            status: "running",
          },
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
        // Decision = "allow" | "allow_always" | { deny: string } | { deny_always: string }
        const reason =
          typeof event.decision === "object"
            ? ("deny" in event.decision
              ? event.decision.deny
              : event.decision.deny_always)
            : null;
        set({
          waitingPermission: null,
          ...(reason !== null
            ? { permissionDeniedMsg: `权限请求已被拒绝：${w.tool}（${reason}）` }
            : {}),
        });
      }
      break;
    }
    case "task_updated":
      effects.push("invalidate-sessions");
      break;
    case "permission_mode_changed": {
      // Plan 模式可视化（遗留）：跟踪 mode 切换。
      // 2026-08-25 审查：EventDto 是 `{seq} & (…|…)` 的判别联合，switch 已把
      // event 窄化为 permission_mode_changed 变体——直接读 `to` 字段
      //（原 `as { to?: string }` 把强类型 PermissionMode 弱化成 string）
      if (event.to === "plan") {
        set({ planActive: true });
      } else {
        set({ planActive: false });
        effects.push("invalidate-messages");
      }
      break;
    }
    case "session_created":
    case "config_changed":
      effects.push("invalidate-messages");
      break;
    // P3：sessions_listed/session_retrieved/command_error 为 NDJSON 专用
    //（NdjsonCommandKind），SSE 流永不出现——死分支已随类型拆分移除
  }
  return { state: next, effects };
}
