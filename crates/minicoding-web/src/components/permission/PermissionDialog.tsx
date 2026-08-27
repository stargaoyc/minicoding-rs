import { motion, AnimatePresence } from "framer-motion";
import { ShieldAlert, ShieldCheck, ShieldX } from "lucide-react";
import { Button } from "../ui/button";
import { Badge } from "../ui/badge";
import type { PendingPermission, PermissionChoice } from "../../hooks/usePermissions";
import type { Risk } from "../../api/generated";
import { cn } from "../../lib/utils";

const RISK_CONFIG: Record<
  Risk,
  {
    icon: typeof ShieldCheck;
    variant: "success" | "warning" | "danger";
    label: string;
    color: string;
  }
> = {
  low: {
    icon: ShieldCheck,
    variant: "success",
    label: "低风险",
    color: "text-[var(--color-risk-low)]",
  },
  medium: {
    icon: ShieldAlert,
    variant: "warning",
    label: "中风险",
    color: "text-[var(--color-risk-medium)]",
  },
  high: {
    icon: ShieldX,
    variant: "danger",
    label: "高风险",
    color: "text-[var(--color-risk-high)]",
  },
};

// 四按钮（2026-08-23 审查遗留#3 贯通）：Always 决策由服务端持久化到
// ~/.minicoding/policy.toml 后折叠为一次性语义执行。
// R4（FE4-1）：按 `pending.options` 过滤渲染——C-23 受保护文件的 prompt 不含
// Always 选项，此前注释声称"按 options 渲染"但代码恒渲染四按钮（服务端
// pending 快照也恒返回空 options），UI 与真实选项集不符。后端 `GET
// /permissions/pending` 现返回真实 options；SSE 实时事件暂不携带（协议 DTO
// 未含字段，roadmap 登记），缺失时回退全按钮 + core SEC-3 折叠兜底。
const ALL_CHOICES: {
  value: PermissionChoice;
  label: string;
  variant: "default" | "secondary" | "danger";
  requires: string;
}[] = [
  { value: "allow", label: "允许", variant: "default", requires: "allow_once" },
  { value: "allow_always", label: "始终允许", variant: "secondary", requires: "allow_always" },
  { value: "deny", label: "拒绝", variant: "danger", requires: "deny_once" },
  { value: "deny_always", label: "始终拒绝", variant: "danger", requires: "deny_always" },
];

interface PermissionDialogProps {
  pending: PendingPermission | null;
  onResolve: (choice: PermissionChoice) => void;
}

export function PermissionDialog({ pending, onResolve }: PermissionDialogProps) {
  return (
    <AnimatePresence>
      {pending && (
        <motion.div
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0 }}
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm"
          onClick={(e) => e.target === e.currentTarget && onResolve("deny")}
        >
          <motion.div
            initial={{ scale: 0.95, opacity: 0, y: 10 }}
            animate={{ scale: 1, opacity: 1, y: 0 }}
            exit={{ scale: 0.95, opacity: 0, y: 10 }}
            transition={{ type: "spring", damping: 20, stiffness: 300 }}
            className="glass w-full max-w-md rounded-2xl p-6 shadow-2xl"
          >
            <PermissionContent pending={pending} onResolve={onResolve} />
          </motion.div>
        </motion.div>
      )}
    </AnimatePresence>
  );
}

function PermissionContent({
  pending,
  onResolve,
}: {
  pending: PendingPermission;
  onResolve: (c: PermissionChoice) => void;
}) {
  const riskConfig = RISK_CONFIG[pending.risk] ?? RISK_CONFIG.medium;
  const RiskIcon = riskConfig.icon;
  // R4（FE4-1）：按后端 options 过滤按钮。option 名称对齐 `PromptOption`
  // 的 snake_case 序列化（allow_once/deny_once/allow_always/deny_always）。
  const choices = ALL_CHOICES.filter(
    (c) => !pending.options?.length || pending.options.includes(c.requires),
  );

  return (
    <>
      {/* Header */}
      <div className="mb-4 flex items-center gap-3">
        <div
          className={cn(
            "flex h-10 w-10 items-center justify-center rounded-xl bg-[var(--color-surface-2)]",
          )}
        >
          <RiskIcon className={cn("h-5 w-5", riskConfig.color)} />
        </div>
        <div>
          <h3 className="text-base font-semibold">权限请求</h3>
          <Badge variant={riskConfig.variant}>{riskConfig.label}</Badge>
        </div>
      </div>

      {/* Tool & summary */}
      <div className="mb-4 space-y-2">
        <div className="flex items-center gap-2 text-sm">
          <span className="text-[var(--color-text-muted)]">工具：</span>
          <code className="rounded bg-[var(--color-surface-2)] px-1.5 py-0.5 text-xs text-[var(--color-accent-hover)]">
            {pending.tool}
          </code>
        </div>
        <div className="rounded-lg bg-[var(--color-surface)] p-3 text-sm leading-relaxed">
          {pending.summary}
        </div>
      </div>

      {/* Actions */}
      <div className="grid grid-cols-2 gap-2">
        {choices.map((c) => (
          <Button
            key={c.value}
            variant={c.variant}
            size="sm"
            onClick={() => onResolve(c.value)}
            className={cn(c.value === "allow" && "col-span-2")}
          >
            {c.label}
          </Button>
        ))}
      </div>
    </>
  );
}
