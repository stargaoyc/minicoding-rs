/**
 * MessageBubble 沙箱拒绝卡片测试（M-14，覆盖 M-09 结构化透传的前端渲染）。
 */
import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { MessageBubble } from "./MessageBubble";
import { makeMessage } from "../../test/handlers";

const deniedMeta = {
  elapsed: { secs: 0, nanos: 0 },
  bytes: 10,
  truncated: false,
  sandbox_denied: {
    kind: { kind: "syscall_blocked" as const, syscall: "Bad system call" },
    detail: "sandbox denied (EPERM): Operation not permitted",
  },
};

describe("MessageBubble sandbox denial card", () => {
  it("tool_result 带 sandbox_denied → 渲染拒绝卡片（kind 标签 + detail）", () => {
    const msg = makeMessage({
      id: "m1",
      role: "tool",
      tool_call_id: "call-1",
      content: [
        {
          type: "tool_result",
          call_id: "call-1",
          content: { type: "text", content: "sandbox denied (EPERM): Operation not permitted" },
          is_error: true,
          metadata: deniedMeta,
        },
      ],
    });
    render(<MessageBubble message={msg} />);
    expect(screen.getByText(/沙箱拒绝 · 系统调用被拒/)).toBeTruthy();
    expect(screen.getByText(/Operation not permitted/)).toBeTruthy();
  });

  it("无 sandbox_denied 的错误结果不渲染拒绝卡片", () => {
    const msg = makeMessage({
      id: "m2",
      role: "tool",
      tool_call_id: "call-2",
      content: [
        {
          type: "tool_result",
          call_id: "call-2",
          content: { type: "text", content: "file not found" },
          is_error: true,
          metadata: {
            elapsed: { secs: 0, nanos: 0 },
            bytes: 3,
            truncated: false,
            sandbox_denied: null,
          },
        },
      ],
    });
    render(<MessageBubble message={msg} />);
    expect(screen.queryByText(/沙箱拒绝/)).toBeNull();
    expect(screen.getByText(/file not found/)).toBeTruthy();
  });
});
