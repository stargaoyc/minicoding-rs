import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

/** shadcn/ui 标准的 `cn()` helper：合并 Tailwind class，后者覆盖前者。 */
export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/** 格式化时间戳为短显示（`14:32` / `昨天` / `8/2`）。无效日期返回空串。 */
export function formatTime(iso: string): string {
  const d = new Date(iso);
  if (isNaN(d.getTime())) return "";
  const now = new Date();
  const sameDay = d.toDateString() === now.toDateString();
  if (sameDay) {
    return d.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" });
  }
  const yesterday = new Date(now);
  yesterday.setDate(yesterday.getDate() - 1);
  if (d.toDateString() === yesterday.toDateString()) return "昨天";
  return `${d.getMonth() + 1}/${d.getDate()}`;
}

/** 截断文本到指定长度。 */
export function truncate(text: string, max: number): string {
  return text.length > max ? text.slice(0, max) + "…" : text;
}

/**
 * 清理路径中的 Windows `\\?\` 前缀（canonicalize 返回的 extended-length path
 * 前缀），并统一为正斜杠。仅用于显示，不改变实际路径值。
 */
export function displayPath(path: string): string {
  // Windows `\\?\E:\...` → `E:\...` → 统一正斜杠
  const cleaned = path.replace(/^\\\\\?\\/, "").replace(/\\/g, "/");
  return cleaned;
}
