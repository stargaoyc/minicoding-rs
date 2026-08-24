import { useState, useCallback } from "react";
import { resolvePermission } from "../api/client";
import type { Risk } from "../api/generated";

/**
 * 权限请求管理 hook（见 AGENTS.md §8.6、design.md §9.1）。
 *
 * SSE 推送 `PermissionRequested` 事件 → 前端弹 Dialog → 用户决策 →
 * `POST /sessions/{id}/permissions/{pid}` 回传 → 后端继续/中止工具调用。
 *
 * 权限检查在后端强制（C-01），前端仅回传 `Decision`，不短路。
 * 权限选项固定为 Allow / Allow Always / Deny / Deny Always（对齐
 * `core::policy::PromptOption`，前端不需要从后端获取 options 列表）。
 */
export interface PendingPermission {
  sessionId: string;
  id: string;
  tool: string;
  summary: string;
  risk: Risk;
}

export type PermissionChoice = "allow" | "allow_always" | "deny" | "deny_always";

export function usePermissions() {
  const [pending, setPending] = useState<PendingPermission | null>(null);

  const requestPermission = useCallback((p: PendingPermission) => {
    setPending(p);
  }, []);

  const resolve = useCallback(
    async (choice: PermissionChoice) => {
      if (!pending) return;
      // 遗留#3：Always 决策原样回传（服务端持久化到 policy.toml 后折叠执行）
      const decision =
        choice === "allow"
          ? "allow"
          : choice === "allow_always"
            ? "allow_always"
            : choice === "deny_always"
              ? { deny_always: "user denied via dialog" }
              : { deny: `user denied via ${choice}` };
      try {
        await resolvePermission(pending.sessionId, pending.id, decision as never);
      } finally {
        setPending(null);
      }
    },
    [pending],
  );

  const dismiss = useCallback(() => setPending(null), []);

  return { pending, requestPermission, resolve, dismiss };
}
