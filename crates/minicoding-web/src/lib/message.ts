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
