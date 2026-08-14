import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState, useCallback } from "react";
import {
  getSession,
  getPendingPermissions,
  sendMessage,
  subscribeEvents,
  type EventDto,
} from "../api/client";
import type { Message, ToolResult } from "../api/generated";

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
  // 当前 turn 的思考过程（reasoning/thinking 增量，瞬态展示，不持久化）
  const [streamingReasoning, setStreamingReasoning] = useState("");
  const [isStreaming, setIsStreaming] = useState(false);
  const [activeTools, setActiveTools] = useState<ActiveTool[]>([]);
  // 等待用户确认的权限请求（弹窗联动 + 底部横幅提示，解决"权限等待不可见"问题）
  const [waitingPermission, setWaitingPermission] = useState<WaitingPermission | null>(null);
  // 权限被拒提示（用户拒绝或 300s 超时自动拒绝），下个 turn 开始时清除
  const [permissionDeniedMsg, setPermissionDeniedMsg] = useState<string | null>(null);
  const waitingRef = useRef<WaitingPermission | null>(null);
  // turn 开始时刻（`turn_streaming_started` 或首个 `tool_call_started`），供 elapsed 计时
  const turnStartedAt = useRef<number | null>(null);
  const subRef = useRef<ReturnType<typeof subscribeEvents> | null>(null);
  const optsRef = useRef(options);
  optsRef.current = options;

  /** 处理一条权限请求（SSE 实时事件或 pending 快照恢复共用）。 */
  const handlePermissionRequested = useCallback(
    (e: { id: string; tool: string; summary: string; risk: "low" | "medium" | "high" }) => {
      const w = { id: e.id, tool: e.tool, summary: e.summary, risk: e.risk };
      waitingRef.current = w;
      setWaitingPermission(w);
      setPermissionDeniedMsg(null);
      optsRef.current?.onPermissionRequested?.(w);
    },
    [],
  );

  const handleEvent = useCallback(
    (event: EventDto) => {
      switch (event.type) {
        case "turn_streaming_started":
          setStreamingText("");
          setStreamingReasoning("");
          setIsStreaming(true);
          setActiveTools([]);
          setWaitingPermission(null);
          setPermissionDeniedMsg(null);
          turnStartedAt.current = Date.now();
          break;
        case "token":
          setStreamingText((prev) => prev + event.text);
          break;
        case "reasoning_delta":
          // 思考过程增量：追加到独立文本（与正文分开渲染）
          setStreamingReasoning((prev) => prev + event.text);
          break;
        case "message_appended":
          setStreamingText("");
          setStreamingReasoning("");
          setIsStreaming(false);
          qc.invalidateQueries({ queryKey: ["messages", sessionId] });
          break;
        case "turn_end":
          setIsStreaming(false);
          // 权限请求未获响应（后端默认 300s 超时自动 Deny，不发 resolved 事件）：
          // 在 turn 结束时提示原因，避免"静默失败"（工具卡片空 + 无文本内容）
          if (event.stop_reason !== "interrupted") {
            const w = waitingRef.current;
            if (w) {
              setPermissionDeniedMsg(
                `权限请求未及时确认，已自动拒绝：${w.tool}（超过响应时限）`,
              );
              waitingRef.current = null;
              setWaitingPermission(null);
            }
          }
          break;
        case "tool_call_started":
          // 工具开始：加入 active 列表（UI 显示 spinner + 工具名）
          setActiveTools((prev) => [
            ...prev.filter((t) => t.callId !== event.call_id),
            { callId: event.call_id, tool: event.tool, status: "running" },
          ]);
          break;
        case "tool_call_finished": {
          setActiveTools((prev) =>
            prev.map((t) =>
              t.callId === event.call_id
                ? {
                    ...t,
                    status: event.result.is_error ? "err" : "ok",
                    result: event.result,
                  }
                : t,
            ),
          );
          // 工具完成后刷新消息 + 工作区（文件改动后树/预览/diff 失效，W-11）
          qc.invalidateQueries({ queryKey: ["messages", sessionId] });
          qc.invalidateQueries({ queryKey: ["workspace", "root", sessionId] });
          qc.invalidateQueries({ queryKey: ["workspace", "list", sessionId] });
          qc.invalidateQueries({ queryKey: ["workspace", "diff", sessionId] });
          qc.invalidateQueries({ queryKey: ["workspace", "file", sessionId] });
          break;
        }
        case "permission_requested": {
          handlePermissionRequested({
            id: event.id,
            tool: event.tool,
            summary: event.summary,
            risk: event.risk,
          });
          break;
        }
        case "permission_resolved": {
          // 用户已在弹窗中决策；后端会继续/中止工具调用。
          const w = waitingRef.current;
          if (w && w.id === event.id) {
            // Decision = "allow" | { deny: string }
            if (typeof event.decision === "object") {
              setPermissionDeniedMsg(`权限请求已被拒绝：${w.tool}（${event.decision.deny}）`);
            }
            waitingRef.current = null;
            setWaitingPermission(null);
          }
          break;
        }
        case "task_updated":
          // invalidate sessions 列表以刷新 task 计数
          qc.invalidateQueries({ queryKey: ["sessions"] });
          optsRef.current?.onTaskUpdated?.();
          break;
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
    // 连接建立/重连成功后拉取未决权限快照，恢复断线期间丢失的弹窗
    // （`PermissionRequested` 是瞬态事件，重连重放不可用，见 server `sse.rs`）。
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
  };
}
