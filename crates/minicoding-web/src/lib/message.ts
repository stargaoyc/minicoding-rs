import type { Message, ContentBlock, ToolContent } from "../api/generated";

/** 从消息 content 块提取纯文本（对齐 Rust `Message::text()`）。 */
export function extractText(message: Pick<Message, "content">): string {
  return message.content
    .filter((b): b is Extract<ContentBlock, { type: "text" }> => b.type === "text")
    .map((b) => b.text)
    .join("\n");
}

/** 从 `ToolContent` 提取摘要文本（text/mixed/json 分支，截断 200 字符）。 */
export function summarizeToolContent(content: ToolContent | undefined): string {
  if (!content) return "";
  switch (content.type) {
    case "text":
      return content.content;
    case "mixed":
      return content.content.map((part) => summarizeToolContent(part)).join("\n");
    case "json":
      try {
        return JSON.stringify(content.content).slice(0, 200);
      } catch {
        return "[json]";
      }
    case "image":
      return "[图片]";
  }
}

/**
 * 提取工具结果消息（`role=tool`）的可显示摘要（`tool_result` 块）。
 * 无任何可显示内容时返回空字符串（调用方据此隐藏空白气泡）。
 */
export function extractToolResultSummary(message: Pick<Message, "content">): {
  text: string;
  isError: boolean;
} {
  const block = message.content.find(
    (b): b is Extract<ContentBlock, { type: "tool_result" }> => b.type === "tool_result",
  );
  if (!block) return { text: "", isError: false };
  const text = summarizeToolContent(block.content);
  // 空 JSON（如 `{}`）视为无可显示内容
  return { text: text.trim() === "{}" ? "" : text, isError: block.is_error };
}

/** 沙箱拒绝类型标签（M-09，与 Rust `SandboxDenyKind` 对齐）。 */
export type SandboxDenyKindDto = import("../api/generated").SandboxDenyKind;

/** `SandboxDenyKind` → 中文标签（前端拒绝卡片用）。 */
export function sandboxDenyLabel(kind: SandboxDenyKindDto): string {
  switch (kind.kind) {
    case "path_escape":
      return "路径越界";
    case "syscall_blocked":
      return "系统调用被拒";
    case "write_forbidden":
      return "写入被拒";
    case "resource_limit":
      return "资源受限";
    case "external":
      return "沙箱拒绝";
  }
}

/**
 * R10：工具结果可读化摘要——识别常见工具结果文本模式，转成更易读的形式。
 * - `fs.write`/`fs.edit`/`fs.delete`：`wrote N bytes to PATH` → `已写入 PATH`
 * - `shell.run`：多行输出截断 + 摘要
 * - 二进制/乱码内容检测：替换字符 U+FFFD 或控制字符比例过高时显示摘要
 * - 其他：原文（保留给 CollapsibleText）
 */
export function formatToolResultSummary(text: string, isError: boolean): string {
  const t = text.trim();
  if (isError) return t;
  // R10 乱码防护：检测非打印字符比例（不可见字符 >5% 视为二进制）
  let nonPrintable = 0;
  for (const c of t) {
    if (c === '\uFFFD' || (c.charCodeAt(0) < 32 && c !== '\n' && c !== '\r' && c !== '\t')) {
      nonPrintable++;
    }
  }
  if (nonPrintable > t.length * 0.05) {
    return `[二进制内容 ${t.length} 字节]（文件不是文本，无法直接显示）`;
  }
  // fs.write/fs.edit/fs.delete
  const wrote = /^wrote (\d+) bytes to (.+)$/.exec(t);
  if (wrote) return `已写入 ${wrote[2]}（${wrote[1]} 字节）`;
  const edited = /^edited (\d+) bytes in (.+)$/.exec(t);
  if (edited) return `已修改 ${edited[2]}（${edited[1]} 字节）`;
  const deleted = /^deleted (.+)$/.exec(t);
  if (deleted) return `已删除 ${deleted[1]}`;
  const created = /^created (.+)$/.exec(t);
  if (created) return `已创建 ${created[1]}`;
  return t;
}
