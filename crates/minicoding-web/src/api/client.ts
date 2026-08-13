/**
 * HTTP/SSE JSON-RPC 客户端（连接 `minicoding-server`）。
 *
 * 设计见 AGENTS.md §8.6、design.md §24：
 * - `POST /sessions` 创建会话
 * - `GET /sessions` 列出会话
 * - `GET /sessions/{id}` 获取消息快照
 * - `POST /sessions/{id}/messages` 发送消息（阻塞至 turn 完成）
 * - `GET /sessions/{id}/events` SSE 事件流（Last-Event-ID 恢复）
 * - `POST /sessions/{id}/permissions/{pid}` 回传权限决策
 *
 * API base 默认空串（同源，开发用 Vite proxy / 生产用 `--web` 托管）；
 * Tauri 桌面模式由 `useDesktopStore` 在 sidecar 启动后调用 `setApiBase`
 * 注入 `http://127.0.0.1:{port}`（design.md §26.5）。
 */

import type {
  EventDto,
  Message,
  SessionConfig,
  SessionMeta,
  Decision,
  PermissionMode,
  WorkspaceDiffEntry,
  WorkspaceDiffResponse,
  WorkspaceFileChange,
  WorkspaceListEntry,
  WorkspaceListResponse,
  WorkspaceReadResponse,
  WorkspaceRoot,
  WorkspaceSwitchResponse,
} from "./generated";

/**
 * 当前 API base（动态）。
 *
 * - Web 模式：`VITE_API_BASE` 环境变量或空串（同源）
 * - Tauri 桌面模式：sidecar 启动后由 `setApiBase` 设为 `http://127.0.0.1:{port}`
 */
let apiBase: string = import.meta.env.VITE_API_BASE ?? "";

/** 读取当前 API base。 */
export function getApiBase(): string {
  return apiBase;
}

/**
 * 设置 API base（Tauri 桌面模式 sidecar 启动后调用）。
 *
 * 传入空串则回退到同源 / `VITE_API_BASE`，便于 Web 模式重置。
 */
export function setApiBase(base: string): void {
  apiBase = base;
}

// ─── HTTP helpers ───────────────────────────────────────────────────────────

async function http<T>(path: string, init?: RequestInit, timeoutMs?: number): Promise<T> {
  const controller = new AbortController();
  const timer =
    timeoutMs != null && Number.isFinite(timeoutMs)
      ? setTimeout(() => controller.abort(new Error(`请求超时（${timeoutMs}ms）`)), timeoutMs)
      : undefined;
  try {
    const resp = await fetch(`${apiBase}${path}`, {
      ...init,
      signal: controller.signal,
      headers: { "Content-Type": "application/json", ...init?.headers },
    });
    if (!resp.ok) {
      const body = await resp.text();
      throw new Error(`HTTP ${resp.status}: ${body}`);
    }
    return resp.json() as Promise<T>;
  } catch (e) {
    // AbortController 中止时统一为超时错误（浏览器原生抛出 AbortError/DOMException）
    if (controller.signal.aborted) {
      throw new Error(`请求超时（${timeoutMs}ms），请重试`);
    }
    throw e;
  } finally {
    if (timer) clearTimeout(timer);
  }
}

// ─── Session API ────────────────────────────────────────────────────────────

export interface CreateSessionBody {
  workdir?: string;
  system?: string;
  provider?: string;
  /** Provider 自定义显示名（用于日志/metrics，不影响协议分派，与后端 `--provider-name` 对齐）。 */
  provider_name?: string;
  /** API base URL（覆盖 server 默认；api_key 不经前端传，C-04 凭证不前端）。 */
  api_base?: string;
  model?: string;
  permission_mode?: PermissionMode;
}

export interface CreateSessionResponse {
  session_id: string;
}

export function createSession(config?: CreateSessionBody): Promise<CreateSessionResponse> {
  return http<CreateSessionResponse>("/sessions", {
    method: "POST",
    body: JSON.stringify(config ?? {}),
  });
}

export function listSessions(): Promise<{ sessions: SessionMeta[] }> {
  return http<{ sessions: SessionMeta[] }>("/sessions");
}

export function getSession(sessionId: string): Promise<{
  session_id: string;
  messages: Message[];
  tasks: import("./generated").Task[];
}> {
  return http<{
    session_id: string;
    messages: Message[];
    tasks: import("./generated").Task[];
  }>(`/sessions/${sessionId}`);
}

export interface SendMessageResponse {
  stop_reason: string;
  final_text: string;
}

export function sendMessage(sessionId: string, text: string): Promise<SendMessageResponse> {
  return http<SendMessageResponse>(`/sessions/${sessionId}/messages`, {
    method: "POST",
    body: JSON.stringify({ text }),
  });
}

export function cancelTurn(sessionId: string): Promise<void> {
  return http<void>(`/sessions/${sessionId}/cancel`, { method: "POST" });
}

export function resolvePermission(sessionId: string, pid: string, decision: Decision): Promise<void> {
  return http<void>(`/sessions/${sessionId}/permissions/${pid}`, {
    method: "POST",
    body: JSON.stringify({ decision }),
  });
}

// ─── Workspace API（W-11 项目工作区，见 design.md §26.9）────────────────────

/** `GET /sessions/{id}/workspace` — 当前工作目录。 */
export function getWorkspaceRoot(sessionId: string): Promise<WorkspaceRoot> {
  return http<WorkspaceRoot>(`/sessions/${sessionId}/workspace`);
}

/** `GET /sessions/{id}/workspace/list?path=` — 目录列表（单层，后端已应用 ignore 过滤）。 */
export function listWorkspace(sessionId: string, path?: string): Promise<WorkspaceListResponse> {
  const q = path ? `?path=${encodeURIComponent(path)}` : "";
  return http<WorkspaceListResponse>(`/sessions/${sessionId}/workspace/list${q}`);
}

/** `GET /sessions/{id}/workspace/read?path=` — 文件内容（≤ 64 KiB 截断）。 */
export function readWorkspaceFile(sessionId: string, path: string): Promise<WorkspaceReadResponse> {
  return http<WorkspaceReadResponse>(
    `/sessions/${sessionId}/workspace/read?path=${encodeURIComponent(path)}`,
  );
}

/** `GET /sessions/{id}/workspace/diff` — 会话内文件改动历史（journal）。 */
export function getWorkspaceDiff(sessionId: string): Promise<WorkspaceDiffResponse> {
  return http<WorkspaceDiffResponse>(`/sessions/${sessionId}/workspace/diff`);
}

/**
 * `POST /sessions/{id}/workspace` — 切换工作目录（Ask 审批：后端广播
 * `permission_requested`，前端弹权限窗，用户允许后生效）。
 *
 * 65s 前端超时：正常审批在弹窗确认后立即返回；若权限弹窗未出现
 * （SSE 断连等），65s 后明确报错而非无限转圈（后端 prompter 300s 超时兜底）。
 */
export function switchWorkspace(sessionId: string, path: string): Promise<WorkspaceSwitchResponse> {
  return http<WorkspaceSwitchResponse>(
    `/sessions/${sessionId}/workspace`,
    {
      method: "POST",
      body: JSON.stringify({ path }),
    },
    65_000,
  );
}

// ─── SSE 事件流 ─────────────────────────────────────────────────────────────

export interface SSESubscription {
  close: () => void;
}

/**
 * 订阅会话 SSE 事件流。
 *
 * 使用原生 `EventSource`（浏览器内置，自动重连 + Last-Event-ID 恢复）。
 * `onEvent` 回调收到每个 `EventDto`；`onError` 在 EventSource 报错时调用。
 */
export function subscribeEvents(
  sessionId: string,
  onEvent: (event: EventDto) => void,
  onError?: (e: Event) => void,
): SSESubscription {
  const url = `${apiBase}/sessions/${sessionId}/events`;
  const source = new EventSource(url);

  source.onmessage = (ev) => {
    try {
      const dto = JSON.parse(ev.data) as EventDto;
      onEvent(dto);
    } catch {
      // 忽略解析失败的事件（如 RehydrateRequired 等非 EventDto 消息）
    }
  };

  source.onerror = (e) => {
    onError?.(e);
  };

  return {
    close: () => source.close(),
  };
}

// Re-export types for convenience
export type {
  SessionConfig,
  SessionMeta,
  Message,
  EventDto,
  Decision,
  PermissionMode,
  WorkspaceRoot,
  WorkspaceListResponse,
  WorkspaceListEntry,
  WorkspaceReadResponse,
  WorkspaceDiffResponse,
  WorkspaceDiffEntry,
  WorkspaceFileChange,
  WorkspaceSwitchResponse,
};
