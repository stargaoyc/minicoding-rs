import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState, useCallback } from "react";
import { useUIStore } from "../stores/ui";
import { useTraceStore } from "../stores/trace";
import {
  getSession,
  getPendingPermissions,
  sendMessage,
  subscribeEvents,
  type EventDto,
} from "../api/client";
import type { Message } from "../api/generated";
import {
  applyChatEvent,
  initialChatState,
  type ChatEffect,
  type ChatStreamState,
  type WaitingPermission,
} from "./chatReducer";

// ActiveTool/WaitingPermission 类型与 SSE 归约逻辑在 `chatReducer.ts`
// （M-14/R-10 抽出纯函数，供 record/replay 快照测试）；此处再导出保持兼容。
export type { ActiveTool, WaitingPermission } from "./chatReducer";

/**
 * 对话 hook：消息快照 + SSE 流式增量（见 AGENTS.md §8.5）。
 *
 * - `useMessages`：TanStack Query 拉取历史消息快照（`GET /sessions/{id}`）
 * - `useSSEStream`：EventSource 订阅事件流，`Token` 增量追加到当前 streaming
 *   消息，`MessageAppended` 替换整条消息并 invalidate 快照缓存；
 *   `PermissionRequested` / `TaskUpdated` 通过回调通知调用方
 * - `useSendMessage`：发送消息（阻塞至 turn 完成），乐观更新立即显示用户消息
 */
export function useMessages(sessionId: string | null) {
  return useQuery({
    queryKey: ["messages", sessionId],
    queryFn: () => getSession(sessionId!).then((r) => r.messages),
    enabled: !!sessionId,
  });
}

/**
 * 会话 turn 运行状态（`GET /sessions/{id}` 的 `turn_running` 字段）。
 *
 * R9 P3-3：SSE 断线/页面刷新后前端据此恢复 `isStreaming`——此前 turn 卡死
 * 时前端状态与后端失同步（SSE 无事件、`turn_end` 未达），用户再输入消息被
 * 静默排队或 UI 误判空闲。`turn_running=true` 时前端恢复"运行中"指示。
 */
export function useTurnRunning(sessionId: string | null) {
  return useQuery({
    queryKey: ["turn-running", sessionId],
    queryFn: () => getSession(sessionId!).then((r) => r.turn_running),
    enabled: !!sessionId,
    // 轮询兜底：SSE 断线期间无事件驱动，靠轮询保持状态新鲜
    refetchInterval: (query) => (query.state.data ? 5_000 : false),
    refetchIntervalInBackground: false,
  });
}

export function useSendMessage(sessionId: string | null) {
  const qc = useQueryClient();
  // R8 FE-15：乐观消息 id 唯一性计数器（跨会话单调，防同毫秒双发冲突）。
  const optimisticSeq = useRef(0);
  return useMutation({
    // 遗留#4：POST /messages 改 202 Accepted——结果走 SSE，不消费响应体。
    // 2026-08-25 审查 F-202residue：后端已删除残留的空 stop_reason/final_text，
    // 前端同步移除 `as unknown as {...}` 双重断言
    mutationFn: (text: string) => sendMessage(sessionId!, text),
    // 乐观更新：发送时立即在 UI 显示用户消息（不等 POST 完成）
    onMutate: async (text: string) => {
      if (!sessionId) return {};
      // 取消进行中的 refetch，避免覆盖乐观更新
      await qc.cancelQueries({ queryKey: ["messages", sessionId] });
      const prev = qc.getQueryData<Message[]>(["messages", sessionId]);
      // R8 FE-15：`optimistic-${Date.now()}` 同毫秒双发会 key 冲突（乐观占位
      // 被 message_appended 剥除时按 id 前缀过滤）——追加单调计数保证唯一。
      const optimistic: Message = {
        id: `optimistic-${Date.now()}-${optimisticSeq.current++}`,
        role: "user",
        content: [{ type: "text", text }],
        tool_calls: [],
        tool_call_id: null,
        created_at: new Date().toISOString(),
        metadata: { tokens: null, pinned: false, summarized: false, source: "user", compressed_range: null },
      };
      qc.setQueryData<Message[]>(["messages", sessionId], (old) => [...(old ?? []), optimistic]);
      return { prev };
    },
    // POST 失败时回滚
    onError: (_err, _text, context) => {
      if (sessionId && context?.prev) {
        qc.setQueryData(["messages", sessionId], context.prev);
      }
    },
    // POST 成功后 invalidate（SSE message_appended 也会 invalidate，这里兜底）
    onSuccess: () => qc.invalidateQueries({ queryKey: ["messages", sessionId] }),
  });
}

interface SSEStreamOptions {
  onPermissionRequested?: (e: {
    id: string;
    tool: string;
    summary: string;
    risk: "low" | "medium" | "high";
    /** R4（FE4-1）：pending 快照携带的 options，SSE 实时事件无此字段 */
    options?: string[];
  }) => void;
  /** R8 FE-4：权限已决/本 turn 结束时回调——前端弹窗须自动关闭
   * （服务端超时 Deny 或他端已裁决时，pending 弹窗不再残留）。 */
  onPermissionResolved?: () => void;
  onTaskUpdated?: () => void;
}

/**
 * SSE 流式 token 订阅。
 *
 * 返回当前正在流式生成的文本（`streamingText`）与是否在流式中（`isStreaming`）。
 * `MessageAppended` 事件 invalidate 消息快照，保证最终一致性。
 */
export function useSSEStream(
  sessionId: string | null,
  options?: SSEStreamOptions,
  /** R9 P3-3：外部 turn 运行信号（`useTurnRunning` 轮询结果）。SSE 断线/
   * 刷新后据此恢复 isStreaming——服务端 turn 仍在跑而 SSE 无事件时，
   * 前端此前误判空闲导致新消息排队/UI 无运行指示。 */
  turnRunning?: boolean,
) {
  const qc = useQueryClient();
  // SSE 归约状态集中在单一对象（M-14：归约逻辑在 chatReducer.ts，可 record/replay 测试）
  const [chatState, setChatState] = useState<ChatStreamState>(initialChatState);
  const { streamingText, streamingReasoning, isStreaming, activeTools, waitingPermission, permissionDeniedMsg, planActive } =
    chatState;
  // 归约用最新状态引用（handleEvent 闭包稳定，不随状态重建订阅）
  const chatStateRef = useRef<ChatStreamState>(initialChatState);
  const waitingRef = useRef<WaitingPermission | null>(null);
  // turn 开始时刻（`turn_streaming_started` 或首个 `tool_call_started`），供 elapsed 计时
  const turnStartedAt = useRef<number | null>(null);
  const subRef = useRef<ReturnType<typeof subscribeEvents> | null>(null);
  const optsRef = useRef(options);
  optsRef.current = options;

  /** 处理一条权限请求（SSE 实时事件或 pending 快照恢复共用）。 */
  const handlePermissionRequested = useCallback(
    (e: {
      id: string;
      tool: string;
      summary: string;
      risk: "low" | "medium" | "high";
      options?: string[];
    }) => {
      const w: WaitingPermission = {
        id: e.id,
        tool: e.tool,
        summary: e.summary,
        risk: e.risk,
        options: e.options,
      };
      waitingRef.current = w;
      setChatState((prev) => {
        const next = { ...prev, waitingPermission: w, permissionDeniedMsg: null };
        chatStateRef.current = next;
        return next;
      });
      optsRef.current?.onPermissionRequested?.(w);
    },
    [],
  );

  /** 副作用映射：reducer 产出的失效指令 → TanStack Query invalidate。 */
  const runEffects = useCallback(
    (effects: ChatEffect[]) => {
      for (const e of effects) {
        switch (e) {
          case "invalidate-messages":
            qc.invalidateQueries({ queryKey: ["messages", sessionId] });
            break;
          case "invalidate-workspace":
            qc.invalidateQueries({ queryKey: ["workspace", "root", sessionId] });
            qc.invalidateQueries({ queryKey: ["workspace", "list", sessionId] });
            qc.invalidateQueries({ queryKey: ["workspace", "diff", sessionId] });
            qc.invalidateQueries({ queryKey: ["workspace", "file", sessionId] });
            break;
          case "invalidate-sessions":
            qc.invalidateQueries({ queryKey: ["sessions"] });
            break;
        }
      }
    },
    [qc, sessionId],
  );

  const handleEvent = useCallback(
    (event: EventDto) => {
      // R10：可观测性——记录事件到 trace store（Agent Loop 全流程回放）
      useTraceStore.getState().push(event);
      if (event.type === "turn_streaming_started") {
        turnStartedAt.current = Date.now();
      }
      // 权限模式切换（SSE 事件）同步到 UI store——侧栏模式切换器据此刻画高亮
      if (event.type === "permission_mode_changed") {
        useUIStore.getState().setPermissionMode(event.to);
      }
      const { state, effects } = applyChatEvent(chatStateRef.current, event);
      chatStateRef.current = state;
      setChatState(state);
      // P8：真实用户消息已落盘——先剥乐观占位再失效，消除短暂重复渲染窗口
      if (event.type === "message_appended") {
        qc.setQueryData<import("../api/generated").Message[]>(
          ["messages", sessionId],
          (old) => old?.filter((m) => !m.id.startsWith("optimistic-")),
        );
      }
      runEffects(effects);
      if (event.type === "permission_requested") {
        waitingRef.current = state.waitingPermission;
        optsRef.current?.onPermissionRequested?.(state.waitingPermission!);
      } else if (event.type === "permission_resolved" || event.type === "turn_end") {
        waitingRef.current = state.waitingPermission;
        // R8 FE-4：权限已决/回合结束 → 前端弹窗自动关闭（服务端超时 Deny、
        // 他端已裁决等场景，避免弹窗残留致用户点"允许"得 404）。
        optsRef.current?.onPermissionResolved?.();
      }
      if (event.type === "task_updated") {
        optsRef.current?.onTaskUpdated?.();
      }
    },
    [runEffects],
  );

  useEffect(() => {
    if (!sessionId) return;
    // R10：切换会话时清空上一条会话的可观测性日志
    useTraceStore.getState().clear();
    // 拉取未决权限快照，恢复丢失的弹窗（`PermissionRequested` 是瞬态事件，
    // SSE 断线/事件丢失时重放不可用，见 server `sse.rs`）。幂等：同 pid
    // 重复设置 waitingPermission 无副作用；已决请求会从快照消失。
    const refreshPending = () => {
      getPendingPermissions(sessionId)
        .then(({ pending }) => {
          for (const p of pending) {
            handlePermissionRequested({
              id: p.id,
              tool: p.tool,
              summary: p.summary,
              risk: p.risk,
              // R4（FE4-1）：快照携带真实 options，前端据此渲染按钮
              options: p.options,
            });
          }
        })
        .catch(() => {
          // 快照拉取失败不影响后续事件流
        });
    };
    subRef.current = subscribeEvents(
      sessionId,
      handleEvent,
      () => {
        console.warn(`SSE connection error for session ${sessionId}`);
      },
      refreshPending,
      // FE-4（2026-08-25 R2 审查）：RehydrateRequired → 重拉消息 snapshot。
      // 此前该信号被静默丢弃，broadcast 溢出/断线丢失的事件区间永久缺失。
      () => {
        qc.invalidateQueries({ queryKey: ["messages", sessionId] });
        qc.invalidateQueries({ queryKey: ["sessions"] });
        refreshPending();
      },
    );
    return () => subRef.current?.close();
  }, [sessionId, handleEvent, handlePermissionRequested]);

  // 权限弹窗兜底轮询：即使 SSE 事件流整体丢失（断线/异常/事件被吞），
  // 每 5s 拉一次 pending 快照，未决权限请求的弹窗仍会出现——否则
  // 300s 超时静默 Deny，任务失败且无提示。请求极小（本地 server），
  // 无未决请求时返回空数组即无开销。
  useEffect(() => {
    if (!sessionId) return;
    const pollTimer = setInterval(() => {
      getPendingPermissions(sessionId)
        .then(({ pending }) => {
          for (const p of pending) {
            handlePermissionRequested({
              id: p.id,
              tool: p.tool,
              summary: p.summary,
              risk: p.risk,
              options: p.options,
            });
          }
        })
        .catch(() => {
          // 快照拉取失败静默（下次轮询重试）
        });
    }, 5_000);
    return () => clearInterval(pollTimer);
  }, [sessionId, handlePermissionRequested]);

  // turn 进行中 elapsed 秒数（1s tick；无事件期间用户可感知"还在运行"而非"卡死"）
  const [elapsedSec, setElapsedSec] = useState(0);
  useEffect(() => {
    if (!isStreaming || turnStartedAt.current === null) {
      setElapsedSec(0);
      return;
    }
    const timer = setInterval(() => {
      setElapsedSec(Math.floor((Date.now() - (turnStartedAt.current ?? Date.now())) / 1000));
    }, 1000);
    return () => clearInterval(timer);
  }, [isStreaming]);

  // R9 P3-3：外部 turn 运行信号同步——SSE 断线/刷新后 `turn_running` 轮询
  // 为 true 而前端 isStreaming 为 false（turn_streaming_started 事件丢失），
  // 恢复"运行中"指示，避免用户误以为空闲再发消息排队。SSE 事件驱动的
  // turn_end 仍优先（reducer 置 false），此同步只做 true 方向的恢复。
  useEffect(() => {
    if (turnRunning && !chatStateRef.current.isStreaming) {
      setChatState((prev) => {
        if (prev.isStreaming) return prev;
        const next = { ...prev, isStreaming: true };
        chatStateRef.current = next;
        return next;
      });
    }
  }, [turnRunning]);

  return {
    streamingText,
    streamingReasoning,
    isStreaming,
    activeTools,
    elapsedSec,
    waitingPermission,
    permissionDeniedMsg,
    planActive,
  };
}
