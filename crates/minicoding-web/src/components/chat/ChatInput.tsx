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

/** 输入历史（模块级内存，非持久化；跨会话共享——参考 shell history 语义）。 */
const inputHistory: string[] = [];

export function ChatInput({
  onSend,
  onCancel,
  isStreaming,
  sendDisabled,
  disabled,
}: ChatInputProps) {
  const [text, setText] = useState("");
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  // 历史浏览游标：null = 正在编辑新内容；否则指向 inputHistory 下标
  const [histIndex, setHistIndex] = useState<number | null>(null);
  // 按 ↑ 前保存的编辑中草稿（按 ↓ 回到末尾时恢复）
  const [draft, setDraft] = useState("");

  const canSend = text.trim().length > 0 && !sendDisabled && !disabled;

  const handleSend = () => {
    const trimmed = text.trim();
    if (!trimmed || sendDisabled || disabled) return;
    onSend(trimmed);
    setText("");
    // 发送成功后记入历史（与上一条相同则不重复），游标复位
    if (inputHistory[inputHistory.length - 1] !== trimmed) {
      inputHistory.push(trimmed);
    }
    setHistIndex(null);
    // 发送后重置高度（回到单行）
    const el = textareaRef.current;
    if (el) el.style.height = "auto";
  };

  const handleChange = (e: ChangeEvent<HTMLTextAreaElement>) => {
    setText(e.target.value);
    // 手动编辑时退出历史浏览模式
    setHistIndex(null);
    // 自适应高度：先复位再取 scrollHeight，受 MAX_HEIGHT 上限约束
    const el = e.target;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, MAX_HEIGHT)}px`;
  };

  const handleKey = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      handleSend();
      return;
    }
    if (e.key === "ArrowUp") {
      // 多行编辑中：光标不在首行时让 ↑ 正常移动光标（不劫持历史浏览）
      const el = textareaRef.current;
      if (el) {
        const caretLine = text.slice(0, el.selectionStart).split("\n").length - 1;
        if (caretLine > 0) return;
      }
      if (inputHistory.length === 0) return;
      e.preventDefault();
      if (histIndex === null) setDraft(text);
      const next = histIndex === null ? inputHistory.length - 1 : Math.max(0, histIndex - 1);
      setHistIndex(next);
      setText(inputHistory[next]);
      return;
    }
    if (e.key === "ArrowDown") {
      if (histIndex === null) return;
      e.preventDefault();
      if (histIndex === 0) {
        // 回到最早一条以下：恢复编辑中草稿
        setHistIndex(null);
        setText(draft);
      } else {
        setHistIndex(histIndex - 1);
        setText(inputHistory[histIndex - 1]);
      }
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
