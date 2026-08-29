import { useCallback } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  cancelTurn,
  getServerConfig,
  setApiBase,
  setPermissionMode,
  undoSession,
} from "../api/client";

/**
 * ARCH-5（2026-08-26 R3 审查）：turn 控制与连接配置的 hooks 封装。
 *
 * AGENTS.md §8.3 分层令：组件不直接调 `api/`，必须经 `hooks/` 封装（缓存/
 * 重试/失效统一管理）。此前 `App.tsx` 直调 `cancelTurn`/`undoSession`/
 * `setApiBase`，`SetupDialog` 直调 `getServerConfig`——本 hook 收口全部
 * turn 生命周期入口。
 */

/** 同步 apiBase 到 HTTP/SSE 客户端（sidecar 启动后注入端口）。
 * 连接级副作用（非服务端状态），不进 TanStack Query 缓存。 */
export function useSetApiBase() {
  return useCallback((base: string) => setApiBase(base), []);
}

/** 取消当前 turn。失败静默降级为 console.warn（取消失败仅影响 UX 不影响正确性）。 */
export function useCancelTurn() {
  return useCallback((sessionId: string | null) => {
    if (!sessionId) return;
    cancelTurn(sessionId).catch(() => {
      console.warn("cancel failed");
    });
  }, []);
}

/** 回滚最近一步文件改动；成功后失效消息与 workspace diff 缓存。
 * 返回冲突文件列表供 UI 提示（空数组 = 全部回滚成功）。 */
export function useUndoSession() {
  const qc = useQueryClient();
  return useCallback(
    async (sessionId: string) => {
      const r = await undoSession(sessionId);
      void qc.invalidateQueries({ queryKey: ["messages", sessionId] });
      void qc.invalidateQueries({ queryKey: ["workspace", "diff", sessionId] });
      return r;
    },
    [qc],
  );
}

/** 拉取 server 端 provider 配置（Setup 流程用；一次性读取，不走缓存键）。
 * 失败抛出由调用方处理（Setup 对话框自行降级默认值）。 */
export function useServerConfig() {
  return useCallback(() => getServerConfig(), []);
}

/** 运行时切换会话权限模式（`POST /sessions/{id}/permission-mode`）。
 * 成功同步 UI store；失败抛错由调用方 toast 展示。
 * `confirmDanger`：升级到 `bypass_permissions` 需 C-22 二次确认（R8 FE-2）。 */
export function useSetPermissionMode() {
  const qc = useQueryClient();
  return useCallback(
    async (
      sessionId: string,
      mode: import("../api/generated").PermissionMode,
      confirmDanger?: boolean,
    ) => {
      await setPermissionMode(sessionId, mode, confirmDanger);
      void qc.invalidateQueries({ queryKey: ["sessions"] });
    },
    [qc],
  );
}
