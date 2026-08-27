import { useState, useCallback } from "react";
import { resolvePermission } from "../api/client";
import type { Decision, Risk } from "../api/generated";

/**
 * 权限请求管理 hook（见 AGENTS.md §8.6、design.md §9.1）。
 *
 * SSE 推送 `PermissionRequested` 事件 → 前端弹 Dialog → 用户决策 →
 * `POST /sessions/{id}/permissions/{pid}` 回传 → 后端继续/中止工具调用。
 *
 * 权限检查在后端强制（C-01），前端仅回传 `Decision`，不短路。
 * 权限选项由后端 `options` 字段指明（`GET /permissions/pending` 返回），
 * 前端据此渲染按钮（C-23 受限 prompt 不含 AllowAlways，前端不显示"始终允许"）。
 */
export interface PendingPermission {
  sessionId: string;
  id: string;
  tool: string;
  summary: string;
  risk: Risk;
  /** R4（FE4-1）：prompt 提供的决策选项，前端据此渲染按钮。 */
  options?: string[];
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
      // 遗留#3：Always 决策原样回传（服务端持久化到 policy.toml 后折叠执行）。
      // 2026-08-25 审查：按生成类型 Decision 判别联合逐支构造（原 `as never`
      // 绕过类型检查，决策形状漂移时编译期无法发现）
      const decision: Decision =
        choice === "allow"
          ? "allow"
          : choice === "allow_always"
            ? "allow_always"
            : choice === "deny_always"
              ? { deny_always: "user denied via dialog" }
              : { deny: `user denied via ${choice}` };
      try {
        await resolvePermission(pending.sessionId, pending.id, decision);
      } finally {
        setPending(null);
      }
    },
    [pending],
  );

  const dismiss = useCallback(() => setPending(null), []);

  return { pending, requestPermission, resolve, dismiss };
}
