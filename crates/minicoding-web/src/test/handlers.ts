/**
 * MSW 请求处理器（M-14）：拦截 `minicoding-server` REST 端点。
 *
 * 测试不连真实后端（对齐 Rust 侧 wiremock 原则，AGENTS.md §5.4/§8.8）。
 * SSE 事件流不经 MSW（EventSource 由 mockEventSource 桩替换）。
 */
import { http, HttpResponse } from "msw";
import type { Message, SessionMeta } from "../api/generated";

/** 测试会话 id（handlers 与断言共用）。 */
export const TEST_SESSION_ID = "sess-test-0001";

/** 构造一条最小合法 Message（生成类型要求全字段）。 */
export function makeMessage(overrides: Partial<Message> & Pick<Message, "role">): Message {
  return {
    id: overrides.id ?? `msg-${Math.random().toString(36).slice(2, 10)}`,
    content: overrides.content ?? [{ type: "text", text: "" }],
    tool_calls: overrides.tool_calls ?? [],
    tool_call_id: overrides.tool_call_id ?? null,
    created_at: overrides.created_at ?? new Date().toISOString(),
    metadata: overrides.metadata ?? {
      tokens: null,
      pinned: false,
      summarized: false,
      source: "llm",
      compressed_range: null,
    },
    ...overrides,
  };
}

export const testSessionMeta: SessionMeta = {
  id: TEST_SESSION_ID,
  created_at: "2026-01-01T00:00:00Z",
  last_message_at: "2026-01-01T00:00:00Z",
  message_count: 0,
  summary: null,
  tasks: [],
};

/**
 * 已追加消息的内存快照：`POST .../messages` 的 handler 会把用户消息 push 进来，
 * `GET /sessions/{id}` 返回它——模拟后端持久化行为（乐观更新 → 快照一致性）。
 */
export const storedMessages: Message[] = [];

/** 记录权限决策回传（供测试断言 resolvePermission 的请求体）。 */
export const permissionCalls: Array<{ sessionId: string; pid: string; body: unknown }> = [];

export const handlers = [
  // GET /sessions/{id} — 消息快照
  http.get(`*/sessions/${TEST_SESSION_ID}`, () =>
    HttpResponse.json({ session: testSessionMeta, messages: [...storedMessages] }),
  ),
  // POST /sessions/{id}/messages — 发送消息（乐观更新落库）
  http.post(`*/sessions/${TEST_SESSION_ID}/messages`, async ({ request }) => {
    const body = (await request.json()) as { text?: string };
    if (body.text) {
      storedMessages.push(
        makeMessage({ role: "user", content: [{ type: "text", text: body.text }] }),
      );
    }
    return HttpResponse.json({ ok: true });
  }),
  // GET /sessions/{id}/permissions/pending — 未决权限快照
  http.get(`*/sessions/${TEST_SESSION_ID}/permissions/pending`, () =>
    HttpResponse.json({ pending: [] }),
  ),
  // POST /sessions/{id}/permissions/{pid} — 权限决策回传
  http.post(`*/sessions/${TEST_SESSION_ID}/permissions/:pid`, async ({ request, params }) => {
    permissionCalls.push({
      sessionId: TEST_SESSION_ID,
      pid: String(params.pid),
      body: await request.json(),
    });
    return HttpResponse.json({ ok: true });
  }),
];
