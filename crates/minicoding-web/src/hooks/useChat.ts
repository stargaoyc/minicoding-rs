import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState, useCallback } from "react";
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

export function useSendMessage(sessionId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    // 遗留#4：POST /messages 改 202 Accepted——结果走 SSE，不消费 final_text
    mutationFn: (text: string) => sendMessage(sessionId!, text).then((r) => r as unknown as { stop_reason: string; final_text: string }),
    // 乐观更新：发送时立即在 UI 显示用户消息（不等 POST 完成）
    onMutate: async (text: string) => {
      if (!sessionId) return {};
      // 取消进行中的 refetch，避免覆盖乐观更新
      await qc.cancelQueries({ queryKey: ["messages", sessionId] });
      const prev = qc.getQueryData<Message[]>(["messages", sessionId]);
      const optimistic: Message = {
        id: `optimistic-${Date.now()}`,
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
  }) => void;
  onTaskUpdated?: () => void;
}

/**
 * SSE 流式 token 订阅。
 *
 * 返回当前正在流式生成的文本（`streamingText`）与是否在流式中（`isStreaming`）。
 * `MessageAppended` 事件 invalidate 消息快照，保证最终一致性。
 */
export function useSSEStream(sessionId: string | null, options?: SSEStreamOptions) {
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
    (e: { id: string; tool: string; summary: string; risk: "low" | "medium" | "high" }) => {
      const w: WaitingPermission = { id: e.id, tool: e.tool, summary: e.summary, risk: e.risk };
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
      if (event.type === "turn_streaming_started") {
        turnStartedAt.current = Date.now();
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
      }
      if (event.type === "task_updated") {
        optsRef.current?.onTaskUpdated?.();
      }
    },
    [runEffects],
  );

  useEffect(() => {
    if (!sessionId) return;
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
