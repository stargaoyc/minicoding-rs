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

import { isEventDto } from "./event-guard";
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

/**
 * 当前 API 鉴权 token（S1）。
 *
 * - Tauri 桌面模式：sidecar 启动后由 `setApiToken` 注入（desktop 生成并内存传递）；
 * - Web 直连模式：`VITE_API_TOKEN` 环境变量；
 * - Vite dev proxy 同源模式：proxy 侧注入，前端留空。
 */
let authToken: string = import.meta.env.VITE_API_TOKEN ?? "";

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

/** 设置 API 鉴权 token（S1，桌面模式 sidecar 启动后调用；空串回退 env）。 */
export function setApiToken(token: string): void {
  authToken = token || (import.meta.env.VITE_API_TOKEN ?? "");
}

// ─── HTTP helpers ───────────────────────────────────────────────────────────

async function http<T>(path: string, init?: RequestInit, timeoutMs?: number): Promise<T> {
  const controller = new AbortController();
  const timer =
    timeoutMs != null && Number.isFinite(timeoutMs)
      ? setTimeout(() => controller.abort(new Error(`请求超时（${timeoutMs}ms）`)), timeoutMs)
      : undefined;
  const method = init?.method ?? "GET";
  // 排查用日志（浏览器 devtools console；桌面 WebView 不可见时看后端 server.log）
  console.debug(`[api] ${method} ${path}`);
  try {
    const resp = await fetch(`${apiBase}${path}`, {
      ...init,
      signal: controller.signal,
      headers: {
        "Content-Type": "application/json",
        // S1：鉴权 token（桌面/直连模式携带；同源 proxy 模式为空不发送）
        ...(authToken ? { Authorization: `Bearer ${authToken}` } : {}),
        ...init?.headers,
      },
    });
    if (!resp.ok) {
      const body = await resp.text();
      console.debug(`[api] ${method} ${path} -> HTTP ${resp.status}`);
      throw new Error(`HTTP ${resp.status}: ${body}`);
    }
    console.debug(`[api] ${method} ${path} -> OK`);
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
  /** 安全预设：`auto`（默认）/ `read-only` / `external-sandbox` / `full-access`（沙箱外全自动，仅受信容器内）。 */
  preset?: "auto" | "read-only" | "external-sandbox" | "full-access";
  /** Plan 模式（C-25：先写 plan.md 拆分任务，批准后执行，仅只读工具可用）。 */
  plan_mode?: boolean;
  /** C-22 二次确认：高危预设（full-access/external-sandbox）或 bypass_permissions
   * 必须携带 `confirm_danger: true`（UI 红色警告确认后回传）。 */
  confirm_danger?: boolean;
  /** LLM 请求超时（秒，覆盖 server 默认 120）。 */
  timeout_sec?: number;
  /** LLM 请求最大重试（覆盖 server 默认 3，C-13）。 */
  max_retries?: number;
  /** 小 LLM 模型名（摘要/压缩降本，见 design.md §3.8；`undefined` 继承 server 默认）。 */
  small_model?: string;
  /** 单 turn 超时（秒，覆盖 server 默认 600）。 */
  turn_timeout_sec?: number;
  /** 上下文压缩开关（覆盖 server 默认，C-18 软约束）。 */
  compress?: boolean;
}

/** `GET /config` 响应（server 当前默认配置，不含 API key，C-04）。 */
export interface ServerConfigResponse {
  provider_kind: string;
  provider_name: string | null;
  api_base: string;
  model: string;
  timeout_sec: number;
  max_retries: number;
  small_model: string | null;
  turn_timeout_sec: number;
  compress: boolean;
  permission_timeout_sec: number;
  preset: string;
  /** 配置修订号（M-10 防陈旧写：保存前锁定基准，与 `save_provider_config` 的 expected_revision 配套）。 */
  config_revision: number;
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

/** 读取 server 当前默认配置（`GET /config`，设置面板编辑模式加载真实默认值用）。 */
export function getServerConfig(): Promise<ServerConfigResponse> {
  return http<ServerConfigResponse>("/config");
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

// 遗留#4：POST /messages 改 202 Accepted——结果走 SSE 推送。
// 2026-08-25 审查 F-202residue：后端响应体已瘦身为 `{accepted: true}`
// （删除无消费方的空 stop_reason/final_text），前端类型同步。
export interface SendMessageResponse {
  accepted: boolean;
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

/** FE-6（2026-08-25 R2 审查）：`POST /sessions/{id}/undo` — 回滚最近 N 步文件改动。 */
export interface UndoResponse {
  undone_entries: number;
  restored_files: string[];
  failed_files: { path: string; reason: string }[];
}

export function undoSession(sessionId: string, steps = 1): Promise<UndoResponse> {
  return http<UndoResponse>(`/sessions/${sessionId}/undo`, {
    method: "POST",
    body: JSON.stringify({ steps }),
  });
}

/** FE-6：`POST /sessions/{id}/permission-mode` — 运行中切换权限模式。
 * `confirm_danger`：升级到 bypass_permissions 需 C-22 二次确认（UI 红色警告后回传）。 */
export function setPermissionMode(
  sessionId: string,
  mode: import("./generated").PermissionMode,
  confirmDanger?: boolean,
): Promise<{ ok: boolean; mode: import("./generated").PermissionMode }> {
  return http(`/sessions/${sessionId}/permission-mode`, {
    method: "POST",
    body: JSON.stringify({ mode, ...(confirmDanger !== undefined ? { confirm_danger: confirmDanger } : {}) }),
  });
}

export function resolvePermission(
  sessionId: string,
  pid: string,
  decision: Decision,
): Promise<void> {
  return http<void>(`/sessions/${sessionId}/permissions/${pid}`, {
    method: "POST",
    body: JSON.stringify({ decision }),
  });
}

/**
 * `GET /sessions/{id}/permissions/pending` — 未决权限请求快照。
 *
 * SSE 断线/页面刷新后调用，恢复权限弹窗（`PermissionRequested` 为瞬态事件，
 * 重连重放不可用，见后端 `sse.rs`）。返回空数组表示无未决请求。
 */
export function getPendingPermissions(
  sessionId: string,
): Promise<{ pending: PendingPermissionDto[] }> {
  return http<{ pending: PendingPermissionDto[] }>(`/sessions/${sessionId}/permissions/pending`);
}

/** 未决权限请求（与 SSE `PermissionRequested` 事件结构一致）。 */
export interface PendingPermissionDto {
  id: string;
  tool: string;
  summary: string;
  risk: "low" | "medium" | "high";
  /** R4（FE4-1）：prompt 提供的决策选项（`allow_once`/`deny_once`/`allow_always`/`deny_always`）。 */
  options: string[];
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
 * `onEvent` 回调收到每个 `EventDto`；`onError` 在 EventSource 报错时调用；
 * `onOpen` 在连接建立/重连成功时调用（前端借此拉取未决权限快照，
 * 恢复断线期间丢失的权限弹窗）。
 */
export function subscribeEvents(
  sessionId: string,
  onEvent: (event: EventDto) => void,
  onError?: (e: Event) => void,
  onOpen?: () => void,
  onRehydrate?: () => void,
): SSESubscription {
  // S1：EventSource 不能自定义请求头，token 走查询参数（服务端仅接受该端点的 query 形式）
  const authQuery = authToken ? `?token=${encodeURIComponent(authToken)}` : "";
  const url = `${apiBase}/sessions/${sessionId}/events${authQuery}`;
  const source = new EventSource(url);

  // 排查用：SSE 事件统计（token/reasoning_delta 高频事件按计数合并输出）
  let highFreqCount = 0;

  // FE-4（2026-08-25 R2 审查）：RehydrateRequired 检测——服务端在 broadcast
  // 溢出/重放不可恢复时发送 `{session_id, last_known_seq, reason}`（无 `type`
  // 字段，非 EventDto）。此前被 isEventDto guard 静默丢弃 → UI 与服务端
  // 永久失同步。识别后回调 onRehydrate 由 hooks 层重拉 snapshot。
  const isRehydratePayload = (v: unknown): boolean => {
    if (typeof v !== "object" || v === null) return false;
    const o = v as Record<string, unknown>;
    return (
      "session_id" in o &&
      "last_known_seq" in o &&
      "reason" in o &&
      !("type" in o)
    );
  };

  source.onmessage = (ev) => {
    try {
      const parsed: unknown = JSON.parse(ev.data);
      if (isRehydratePayload(parsed)) {
        console.warn("[sse] 收到 RehydrateRequired：本地事件流不完整，重拉 snapshot");
        onRehydrate?.();
        return;
      }
      if (!isEventDto(parsed)) {
        console.warn("[sse] 未知事件类型，已丢弃:", ev.data.slice(0, 120));
        return;
      }
      const dto = parsed as EventDto;
      if (dto.type === "token" || dto.type === "reasoning_delta") {
        highFreqCount += 1;
        if (highFreqCount % 100 === 0) {
          console.debug(`[sse] ...累计 ${highFreqCount} 个 ${dto.type} 增量`);
        }
      } else {
        console.debug(`[sse] event: ${dto.type}`);
      }
      onEvent(dto);
    } catch {
      // 忽略解析失败的事件
      console.debug("[sse] 无法解析的事件数据已忽略");
    }
  };

  source.onerror = (e) => {
    console.debug("[sse] EventSource error（将自动重连）", e);
    onError?.(e);
  };

  source.onopen = () => {
    console.debug("[sse] 连接已建立");
    onOpen?.();
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
