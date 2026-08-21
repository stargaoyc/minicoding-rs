/**
 * useSSEStream 集成测试（M-14）：mock EventSource 重放 SSE 序列 + MSW 拦截 REST，
 * 覆盖"发消息 → 流式渲染 → 权限确认 → 沙箱拒绝"关键路径（不连真实后端）。
 */
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { useSendMessage, useSSEStream } from "./useChat";
import { installMockEventSource, restoreEventSource, type MockEventSource } from "../test/mockEventSource";
import { permissionFlow, sandboxDenied } from "../test/sseFixtures";
import { TEST_SESSION_ID } from "../test/handlers";

function wrapper({ children }: { children: ReactNode }) {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  return <QueryClientProvider client={qc}>{children}</QueryClientProvider>;
}

let sources: MockEventSource[];

beforeEach(() => {
  sources = installMockEventSource();
});
afterEach(() => restoreEventSource());

/** 等待 hook 订阅建立（EventSource onopen 异步触发）。 */
async function setupStream() {
  const harness = renderHook(() => useSSEStream(TEST_SESSION_ID), { wrapper });
  await waitFor(() => expect(sources.length).toBeGreaterThan(0));
  return harness;
}

describe("useSSEStream + mock EventSource", () => {
  it("权限确认流：permission_requested 弹窗状态 → resolved 清除", async () => {
    const { result } = await setupStream();
    const es = sources[0];

    act(() => es.emit(JSON.stringify(permissionFlow[1]))); // tool_call_started
    act(() => es.emit(JSON.stringify(permissionFlow[2]))); // permission_requested

    await waitFor(() => expect(result.current.waitingPermission?.id).toBe("perm-1"));
    expect(result.current.activeTools[0].status).toBe("running");

    act(() => es.emit(JSON.stringify(permissionFlow[3]))); // permission_resolved allow
    await waitFor(() => expect(result.current.waitingPermission).toBeNull());
    expect(result.current.permissionDeniedMsg).toBeNull();

    act(() => es.emit(JSON.stringify(permissionFlow[4]))); // tool_call_finished ok
    await waitFor(() => expect(result.current.activeTools[0].status).toBe("ok"));
  });

  it("沙箱拒绝：tool_call_finished err + 结构化 sandbox_denied（拒绝卡片数据源）", async () => {
    const { result } = await setupStream();
    const es = sources[0];

    for (const e of sandboxDenied.slice(1, 7)) {
      act(() => es.emit(JSON.stringify(e)));
    }
    await waitFor(() =>
      expect(result.current.activeTools.filter((t) => t.status === "err")).toHaveLength(3),
    );
    const denied = result.current.activeTools[0].result?.metadata.sandbox_denied;
    expect(denied?.kind.kind).toBe("syscall_blocked");
    expect(denied?.detail).toContain("Operation not permitted");
  });

  it("token 增量累积，message_appended 后清空瞬态", async () => {
    const { result } = await setupStream();
    const es = sources[0];
    const happy = (await import("../test/sseFixtures")).happyTurn;

    act(() => es.emit(JSON.stringify(happy[0])));
    act(() => es.emit(JSON.stringify(happy[1])));
    await waitFor(() => expect(result.current.streamingText).toBe("你好"));
    act(() => es.emit(JSON.stringify(happy[2])));
    await waitFor(() => expect(result.current.streamingText).toBe("你好，这是"));

    act(() => es.emit(JSON.stringify(happy[4]))); // message_appended
    await waitFor(() => expect(result.current.streamingText).toBe(""));
    expect(result.current.isStreaming).toBe(false);
  });
});

describe("useSendMessage", () => {
  it("发送消息经 MSW 落库并 invalidate 快照", async () => {
    const { result } = renderHook(() => useSendMessage(TEST_SESSION_ID), { wrapper });
    result.current.mutate("帮我写个函数");
    await waitFor(() => expect(result.current.isSuccess).toBe(true));
  });
});
