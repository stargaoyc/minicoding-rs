import { useState, type KeyboardEvent } from "react";
import { Send, Square } from "lucide-react";
import { Button } from "../ui/button";
import { cn } from "../../lib/utils";

interface ChatInputProps {
  onSend: (text: string) => void;
  onCancel?: () => void;
  isStreaming: boolean;
  disabled?: boolean;
}

export function ChatInput({ onSend, onCancel, isStreaming, disabled }: ChatInputProps) {
  const [text, setText] = useState("");

  const handleSend = () => {
    const trimmed = text.trim();
    if (!trimmed || disabled) return;
    onSend(trimmed);
    setText("");
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
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={handleKey}
          placeholder="输入消息，Enter 发送，Shift+Enter 换行…"
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
            disabled={!text.trim() || disabled}
            title="发送"
          >
            <Send className="h-4 w-4" />
          </Button>
        )}
      </div>
    </div>
  );
}
