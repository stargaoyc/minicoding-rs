import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { createSession, listSessions, type CreateSessionBody } from "../api/client";

/**
 * 会话列表 hook（TanStack Query 管理服务端状态，见 AGENTS.md §8.5）。
 *
 * `useSessions` 拉取列表；`useCreateSession` 创建后自动 invalidate 列表缓存。
 */
export function useSessions() {
  return useQuery({
    queryKey: ["sessions"],
    queryFn: listSessions,
    refetchInterval: 10_000, // 轮询刷新（SSE 仅推送单会话事件，列表需轮询）
    select: (data) => data.sessions,
  });
}

export function useCreateSession() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (config?: CreateSessionBody) => createSession(config),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["sessions"] }),
  });
}
