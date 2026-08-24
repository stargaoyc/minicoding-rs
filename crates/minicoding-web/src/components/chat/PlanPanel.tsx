import { Badge } from "@/components/ui/badge";

/**
 * Plan 模式进度面板（遗留：对标 CC Plan Mode 可视化）。
 *
 * 当 `permission_mode_changed` 事件切换到 `plan` 时显示；展示当前模式
 * 与提示文案。`plan.exit` 后切回 default 自动隐藏。
 */
export function PlanPanel({ active }: { active: boolean }) {
  if (!active) return null;
  return (
    <div className="rounded-lg border border-yellow-500/50 bg-yellow-500/5 p-3">
      <div className="flex items-center gap-2 text-sm font-medium">
        <Badge variant="warning">Plan</Badge>
        只读模式已激活
      </div>
      <p className="mt-1 text-xs text-muted-foreground">
        副作用工具被硬门拒绝。调用 <code className="rounded bg-muted px-1">plan.exit</code> 提交计划后进入执行阶段。
      </p>
    </div>
  );
}
