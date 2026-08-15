import { forwardRef, type HTMLAttributes } from "react";
import { cn } from "../../lib/utils";

/**
 * 轻量滚动区域（原生 `overflow-y-auto` + 自定义滚动条样式，见 index.css）。
 *
 * 未引入 `@radix-ui/react-scroll-area` 以减少 bundle 体积——原生滚动在 SPA
 * 场景足够，Radix ScrollArea 主要为旧版浏览器兼容性设计。
 */
export const ScrollArea = forwardRef<HTMLDivElement, HTMLAttributes<HTMLDivElement>>(
  ({ className, children, ...props }, ref) => (
    <div ref={ref} className={cn("overflow-y-auto overflow-x-hidden", className)} {...props}>
      {children}
    </div>
  ),
);
ScrollArea.displayName = "ScrollArea";
