import { useRef, useState, type ChangeEvent, type KeyboardEvent } from "react";
import { Send, Square } from "lucide-react";
import { Button } from "../ui/button";
import { cn } from "../../lib/utils";

interface ChatInputProps {
  onSend: (text: string) => void;
  onCancel?: () => void;
  /** 流式生成中（显示停止按钮）。 */
  isStreaming: boolean;
  /** 运行中禁止发送（输入框不禁用，可提前输入下一条消息）。 */
  sendDisabled?: boolean;
  /** 输入框硬禁用（会话不可用等场景）。 */
  disabled?: boolean;
}

/** 输入框最大高度（与 `max-h-32` 一致，超出滚动）。 */
const MAX_HEIGHT = 128;

export function ChatInput({ onSend, onCancel, isStreaming, sendDisabled, disabled }: ChatInputProps) {
  const [text, setText] = useState("");
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  const canSend = text.trim().length > 0 && !sendDisabled && !disabled;

  const handleSend = () => {
    const trimmed = text.trim();
    if (!trimmed || sendDisabled || disabled) return;
    onSend(trimmed);
    setText("");
    // 发送后重置高度（回到单行）
    const el = textareaRef.current;
    if (el) el.style.height = "auto";
  };

  const handleChange = (e: ChangeEvent<HTMLTextAreaElement>) => {
    setText(e.target.value);
    // 自适应高度：先复位再取 scrollHeight，受 MAX_HEIGHT 上限约束
    const el = e.target;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, MAX_HEIGHT)}px`;
  };

  const handleKey = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      handleSend();
    }
  };

  return (
    <div className="border-t border-[var(--color-border)] p-4">
      <div
        className={cn(
          "flex items-end gap-2 rounded-xl border bg-[var(--color-surface)] px-3 py-2 transition-colors",
          "border-[var(--color-border)] focus-within:border-[var(--color-accent)]/50",
        )}
      >
        <textarea
          ref={textareaRef}
          value={text}
          onChange={handleChange}
          onKeyDown={handleKey}
          placeholder={
            sendDisabled
              ? "正在运行…可提前输入下一条消息（完成后发送）"
              : "输入消息，Enter 发送，Shift+Enter 换行…"
          }
          rows={1}
          disabled={disabled}
          className={cn(
            "flex-1 resize-none bg-transparent text-sm leading-relaxed outline-none",
            "max-h-32 min-h-[1.5rem] placeholder:text-[var(--color-text-muted)]",
            "disabled:opacity-50",
          )}
          style={{ height: "auto" }}
        />
        {isStreaming ? (
          <Button variant="danger" size="icon" onClick={onCancel} title="取消生成">
            <Square className="h-4 w-4" />
          </Button>
        ) : (
          <Button
            size="icon"
            onClick={handleSend}
            disabled={!canSend}
            title={sendDisabled ? "运行中，完成后可发送" : "发送"}
          >
            <Send className="h-4 w-4" />
          </Button>
        )}
      </div>
    </div>
  );
}
