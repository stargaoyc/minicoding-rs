import { cva, type VariantProps } from "class-variance-authority";
import type { HTMLAttributes } from "react";
import { cn } from "../../lib/utils";

const badgeVariants = cva(
  "inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-xs font-medium",
  {
    variants: {
      variant: {
        default: "bg-[var(--color-surface-2)] text-[var(--color-text-muted)]",
        success: "bg-[var(--color-risk-low)]/15 text-[var(--color-risk-low)]",
        warning: "bg-[var(--color-risk-medium)]/15 text-[var(--color-risk-medium)]",
        danger: "bg-[var(--color-risk-high)]/15 text-[var(--color-risk-high)]",
        accent: "bg-[var(--color-accent)]/15 text-[var(--color-accent-hover)]",
      },
    },
    defaultVariants: { variant: "default" },
  },
);

export interface BadgeProps
  extends HTMLAttributes<HTMLSpanElement>, VariantProps<typeof badgeVariants> {}

export function Badge({ className, variant, ...props }: BadgeProps) {
  return <span className={cn(badgeVariants({ variant }), className)} {...props} />;
}
