import type React from "react";

/** 表单字段容器（label + 控件 + 可选 hint）。 */
export function Field({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="space-y-1.5">
      <label className="text-xs font-medium text-[var(--color-text-muted)]">{label}</label>
      {children}
      {hint && <p className="text-[10px] text-[var(--color-text-muted)]">{hint}</p>}
    </div>
  );
}

/** 输入控件统一样式（与 ChatInput 风格对齐）。 */
