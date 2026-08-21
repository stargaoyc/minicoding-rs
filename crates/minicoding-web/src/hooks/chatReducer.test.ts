/**
 * chatReducer record/replay 快照测试（M-14/R-10）。
 *
 * 三条录制的事件序列（`sseFixtures.ts`）逐条归约，终态与快照比对
 * （`SNAPSHOT_MODE` 三态：replay/record/off，见 `snapshot.ts`）；
 * 另有中间态断言（流式增量、权限出现/清除、拒绝卡片状态机）。
 */
import { describe, expect, it } from "vitest";
import { applyChatEvent, initialChatState } from "./chatReducer";
import { happyTurn, permissionFlow, sandboxDenied } from "../test/sseFixtures";
import { expectMatchesSnapshot } from "../test/snapshot";
import type { EventDto } from "../api/client";

/** 归约整条序列，返回终态与副作用累计。 */
function replay(events: readonly EventDto[]) {
  let state = initialChatState;
  const effects: string[] = [];
  for (const e of events) {
    const r = applyChatEvent(state, e);
    state = r.state;
    effects.push(...r.effects);
  }
  return { state, effects };
}

describe("chatReducer replay", () => {
  it("happy-turn：流式增量累积、message_appended 清空瞬态", () => {
    // 中间态：token 增量拼接
    let s = initialChatState;
    for (const e of happyTurn.slice(0, 4)) {
      s = applyChatEvent(s, e).state;
    }
    expect(s.streamingText).toBe("你好，这是回复。");
    expect(s.isStreaming).toBe(true);

    const { state, effects } = replay(happyTurn);
    expect(state.isStreaming).toBe(false);
    expect(state.streamingText).toBe("");
    expect(effects).toContain("invalidate-messages");
    expectMatchesSnapshot("happy-turn-final", state);
  });

  it("permission-flow：权限请求出现 → allow 清除 → 工具完成", () => {
    let s = initialChatState;
    // tool_call_started → running
    s = applyChatEvent(s, permissionFlow[1]).state;
    expect(s.activeTools).toHaveLength(1);
    expect(s.activeTools[0].status).toBe("running");

    // permission_requested → 弹窗状态
    s = applyChatEvent(s, permissionFlow[2]).state;
    expect(s.waitingPermission).toEqual({
      id: "perm-1",
      tool: "fs.write",
      summary: "写入文件 src/main.rs",
      risk: "medium",
    });

    // resolved(allow) → 清除且无拒绝提示
    s = applyChatEvent(s, permissionFlow[3]).state;
    expect(s.waitingPermission).toBeNull();
    expect(s.permissionDeniedMsg).toBeNull();

    // turn_end 前工具已完成（ok）；turn_end 无条件清空瞬态卡片
    const beforeEnd = permissionFlow.slice(0, -1).reduce(
      (acc, e) => applyChatEvent(acc, e).state,
      initialChatState,
    );
    expect(beforeEnd.activeTools[0].status).toBe("ok");
    const { state } = replay(permissionFlow);
    expect(state.activeTools).toHaveLength(0);
    expectMatchesSnapshot("permission-flow-final", state);
  });

  it("sandbox-denied：三次拒绝后工具卡片 err 且携带结构化 metadata", () => {
    // turn_end 前：3 张拒绝卡片（err + 结构化 metadata，拒绝卡片数据源）
    const beforeEnd = sandboxDenied.slice(0, -1).reduce(
      (acc, e) => applyChatEvent(acc, e).state,
      initialChatState,
    );
    expect(beforeEnd.activeTools).toHaveLength(3);
    for (const t of beforeEnd.activeTools) {
      expect(t.status).toBe("err");
      expect(t.result?.is_error).toBe(true);
      expect(t.result?.metadata.sandbox_denied?.kind.kind).toBe("syscall_blocked");
    }
    const { state } = replay(sandboxDenied);
    expect(state.activeTools).toHaveLength(0);
    expectMatchesSnapshot("sandbox-denied-final", state);
  });

  it("turn_end 未决权限 → 超时自动拒绝提示；interrupted 不提示", () => {
    let s = applyChatEvent(initialChatState, {
      seq: 1,
      type: "permission_requested",
      id: "p",
      tool: "shell.run",
      summary: "s",
      risk: "high",
    }).state;
    s = applyChatEvent(s, { seq: 2, type: "turn_end", stop_reason: "end_turn" }).state;
    expect(s.permissionDeniedMsg).toContain("已自动拒绝");

    // interrupted：无条件清空但不提示
    let s2 = applyChatEvent(initialChatState, {
      seq: 1,
      type: "permission_requested",
      id: "p",
      tool: "shell.run",
      summary: "s",
      risk: "high",
    }).state;
    s2 = applyChatEvent(s2, { seq: 2, type: "turn_end", stop_reason: "interrupted" }).state;
    expect(s2.permissionDeniedMsg).toBeNull();
    // interrupted 不清未决权限（与原 useSSEStream 行为一致：pending 轮询兜底）
    expect(s2.waitingPermission).not.toBeNull();
  });
});
