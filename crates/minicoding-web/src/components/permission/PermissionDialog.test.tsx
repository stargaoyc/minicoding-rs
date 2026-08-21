/**
 * PermissionDialog 交互测试（M-14）。
 *
 * 组件只产出决策回调（`onResolve(choice)`），回传后端由调用方经
 * `resolvePermission` 完成（C-01：前端不短路权限检查）。
 */
import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { PermissionDialog } from "./PermissionDialog";

describe("PermissionDialog", () => {
  it("点击允许 → onResolve('allow')（决策交由后端执行，C-01）", () => {
    const onResolve = vi.fn();
    render(
      <PermissionDialog
        pending={{
        id: "perm-t1",
        sessionId: "sess-test",
        tool: "fs.write",
        summary: "写入 src/main.rs",
        risk: "medium",
      }}
        onResolve={onResolve}
      />,
    );
    const allowBtn = screen.getByRole("button", { name: "允许" });
    allowBtn.click();
    expect(onResolve).toHaveBeenCalledWith("allow");
  });

  it("点击拒绝 → onResolve('deny')", () => {
    const onResolve = vi.fn();
    render(
      <PermissionDialog
        pending={{
        id: "perm-t2",
        sessionId: "sess-test",
        tool: "shell.run",
        summary: "rm -rf /",
        risk: "high",
      }}
        onResolve={onResolve}
      />,
    );
    screen.getByRole("button", { name: "拒绝" }).click();
    expect(onResolve).toHaveBeenCalledWith("deny");
  });

  it("pending 为 null 时不渲染弹窗", () => {
    render(<PermissionDialog pending={null} onResolve={() => {}} />);
    expect(screen.queryByRole("button", { name: "允许" })).toBeNull();
  });
});
