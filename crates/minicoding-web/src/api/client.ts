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
 * API base 默认空串（同源，开发用 Vite proxy / 生产用 `--web` 托管），
 * Tauri 桌面模式通过 `VITE_API_BASE` 注入 sidecar 端口。
 */

import type {
  EventDto,
  Message,
  SessionConfig,
  SessionMeta,
  Decision,
  PermissionMode,
} from "./generated";

const API_BASE = import.meta.env.VITE_API_BASE ?? "";

// ─── HTTP helpers ───────────────────────────────────────────────────────────

async function http<T>(path: string, init?: RequestInit): Promise<T> {
  const resp = await fetch(`${API_BASE}${path}`, {
    ...init,
    headers: { "Content-Type": "application/json", ...init?.headers },
  });
  if (!resp.ok) {
    const body = await resp.text();
    throw new Error(`HTTP ${resp.status}: ${body}`);
  }
  return resp.json() as Promise<T>;
}

// ─── Session API ────────────────────────────────────────────────────────────

export interface CreateSessionBody {
  workdir?: string;
  system?: string;
  provider?: string;
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

export function getSession(sessionId: string): Promise<{ session_id: string; messages: Message[] }> {
  return http<{ session_id: string; messages: Message[] }>(`/sessions/${sessionId}`);
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
  const url = `${API_BASE}/sessions/${sessionId}/events`;
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
export type { SessionConfig, SessionMeta, Message, EventDto, Decision, PermissionMode };
