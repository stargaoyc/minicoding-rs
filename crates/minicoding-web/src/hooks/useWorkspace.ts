import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import {
  getWorkspaceRoot,
  listWorkspace,
  readWorkspaceFile,
  getWorkspaceDiff,
  switchWorkspace,
} from "../api/client";

/**
 * 工作区 hooks（W-11，见 AGENTS.md §8.5：服务端状态走 TanStack Query）。
 *
 * - `useWorkspaceRoot`：当前会话 workdir
 * - `useWorkspaceList`：目录列表（单层，queryKey 含 path）
 * - `useWorkspaceFile`：文件内容预览（点击文件时启用）
 * - `useWorkspaceDiff`：会话内文件改动历史（diff 面板）
 * - `useSwitchWorkspace`：切换工作目录（Ask 审批弹窗由 SSE
 *   `permission_requested` 触发，与工具权限共用 W-03 机制）
 */
export function useWorkspaceRoot(sessionId: string | null) {
  return useQuery({
    queryKey: ["workspace", "root", sessionId],
    queryFn: () => getWorkspaceRoot(sessionId!),
    enabled: !!sessionId,
  });
}

export function useWorkspaceList(sessionId: string | null, path: string | null) {
  return useQuery({
    queryKey: ["workspace", "list", sessionId, path ?? ""],
    queryFn: () => listWorkspace(sessionId!, path ?? undefined),
    enabled: !!sessionId && !!path,
  });
}

export function useWorkspaceFile(sessionId: string | null, path: string | null) {
  return useQuery({
    queryKey: ["workspace", "file", sessionId, path],
    queryFn: () => readWorkspaceFile(sessionId!, path!),
    enabled: !!sessionId && !!path,
    // 预览数据只读快照，无需轮询
    staleTime: 30_000,
  });
}

export function useWorkspaceDiff(sessionId: string | null) {
  return useQuery({
    queryKey: ["workspace", "diff", sessionId],
    queryFn: () => getWorkspaceDiff(sessionId!),
    enabled: !!sessionId,
  });
}

export function useSwitchWorkspace(sessionId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (path: string) => switchWorkspace(sessionId!, path),
    onSuccess: (resp) => {
      // 切换成功（含用户拒绝：switched=false 也刷新 root，路径保持不变）
      qc.invalidateQueries({ queryKey: ["workspace", "root", sessionId] });
      qc.invalidateQueries({ queryKey: ["workspace", "list", sessionId] });
      qc.invalidateQueries({ queryKey: ["workspace", "diff", sessionId] });
      void resp;
    },
  });
}

/**
 * 工具执行后刷新工作区缓存（由 `useChat.ts` 的 SSE `tool_call_finished`
 * 分支调用——文件改动后文件树/预览/diff 需重新拉取）。
 */
export function invalidateWorkspace(
  qc: ReturnType<typeof useQueryClient>,
  sessionId: string | null,
) {
  if (!sessionId) return;
  qc.invalidateQueries({ queryKey: ["workspace", "root", sessionId] });
  qc.invalidateQueries({ queryKey: ["workspace", "list", sessionId] });
  qc.invalidateQueries({ queryKey: ["workspace", "diff", sessionId] });
  qc.invalidateQueries({ queryKey: ["workspace", "file", sessionId] });
}
