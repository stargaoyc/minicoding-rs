import type { Message, ContentBlock } from "../api/generated";

/** 从消息 content 块提取纯文本（对齐 Rust `Message::text()`）。 */
export function extractText(message: Pick<Message, "content">): string {
  return message.content
    .filter((b): b is Extract<ContentBlock, { type: "text" }> => b.type === "text")
    .map((b) => b.text)
    .join("\n");
}
