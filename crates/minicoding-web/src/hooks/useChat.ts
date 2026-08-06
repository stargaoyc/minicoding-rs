import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState, useCallback } from "react";
import {
  getSession,
  sendMessage,
  subscribeEvents,
  type EventDto,
} from "../api/client";
import type { Message } from "../api/generated";

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
    mutationFn: (text: string) => sendMessage(sessionId!, text),
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
        metadata: { tokens: null, pinned: false, summarized: false, source: "user" },
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
  const [streamingText, setStreamingText] = useState("");
  const [isStreaming, setIsStreaming] = useState(false);
  const subRef = useRef<ReturnType<typeof subscribeEvents> | null>(null);
  const optsRef = useRef(options);
  optsRef.current = options;

  const handleEvent = useCallback(
    (event: EventDto) => {
      switch (event.type) {
        case "turn_streaming_started":
          setStreamingText("");
          setIsStreaming(true);
          break;
        case "token":
          setStreamingText((prev) => prev + event.text);
          break;
        case "message_appended":
          setStreamingText("");
          setIsStreaming(false);
          qc.invalidateQueries({ queryKey: ["messages", sessionId] });
          break;
        case "turn_end":
          setIsStreaming(false);
          break;
        case "permission_requested":
          optsRef.current?.onPermissionRequested?.({
            id: event.id,
            tool: event.tool,
            summary: event.summary,
            risk: event.risk,
          });
          break;
        case "task_updated":
          // invalidate sessions 列表以刷新 task 计数
          qc.invalidateQueries({ queryKey: ["sessions"] });
          optsRef.current?.onTaskUpdated?.();
          break;
        case "tool_call_started":
        case "tool_call_finished":
        case "permission_resolved":
        case "permission_mode_changed":
        case "session_created":
        case "config_changed":
        case "sessions_listed":
        case "session_retrieved":
        case "command_error":
          qc.invalidateQueries({ queryKey: ["messages", sessionId] });
          break;
      }
    },
    [qc, sessionId],
  );

  useEffect(() => {
    if (!sessionId) return;
    subRef.current = subscribeEvents(sessionId, handleEvent, () => {
      console.warn(`SSE connection error for session ${sessionId}`);
    });
    return () => subRef.current?.close();
  }, [sessionId, handleEvent]);

  return { streamingText, isStreaming };
}
