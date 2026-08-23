import type { ReactNode } from "react";

/** 分组标题（表单分区，W-19）。 */
export function SectionTitle({ children }: { children: ReactNode }) {
  return (
    <div className="pt-1 text-[11px] font-semibold uppercase tracking-wider text-[var(--color-text-muted)]">
      {children}
    </div>
  );
}
