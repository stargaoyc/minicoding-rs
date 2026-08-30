import { useEffect, useRef, useState, type ChangeEvent, type KeyboardEvent } from "react";
import { Send, Square, TriangleAlert } from "lucide-react";
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

/** 输入历史 localStorage key（跨页面刷新持久化，参考 shell history 语义）。 */
const HISTORY_STORAGE_KEY = "minicoding.inputHistory";
/** 输入历史容量上限（防无限增长）。 */
const HISTORY_LIMIT = 200;

/** 读取持久化输入历史（localStorage 缺失/损坏时回退空数组）。 */
function loadInputHistory(): string[] {
  try {
    const raw = window.localStorage.getItem(HISTORY_STORAGE_KEY);
    if (!raw) return [];
    const parsed: unknown = JSON.parse(raw);
    return Array.isArray(parsed) && parsed.every((x) => typeof x === "string")
      ? (parsed as string[])
      : [];
  } catch {
    return [];
  }
}

/** 追加输入历史并持久化（与上一条相同则不重复，容量超限裁头）。 */
function pushInputHistory(history: string[], text: string): void {
  if (history[history.length - 1] === text) return;
  history.push(text);
  if (history.length > HISTORY_LIMIT) history.splice(0, history.length - HISTORY_LIMIT);
  try {
    window.localStorage.setItem(HISTORY_STORAGE_KEY, JSON.stringify(history));
  } catch {
    // localStorage 不可用（隐私模式/配额满）时仅内存历史可用，不阻塞发送
  }
}

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
  // 跨刷新持久化的输入历史（模块级内存 + localStorage 双层，见 handleSend）
  const historyRef = useRef<string[]>(loadInputHistory());
  // 运行中按 Enter 的提示文本（替代静默丢弃——用户反馈"卡死后再输入无反馈"）
  const [blockedHint, setBlockedHint] = useState<string | null>(null);

  // 会话切换/挂载时重新加载持久化历史（跨刷新恢复）
  useEffect(() => {
    historyRef.current = loadInputHistory();
    setHistIndex(null);
  }, []);

  // 提示自动消退（2s 后清除）
  useEffect(() => {
    if (!blockedHint) return;
    const t = setTimeout(() => setBlockedHint(null), 2_000);
    return () => clearTimeout(t);
  }, [blockedHint]);

  const canSend = text.trim().length > 0 && !sendDisabled && !disabled;

  const handleSend = () => {
    const trimmed = text.trim();
    if (!trimmed) return;
    if (sendDisabled || disabled) {
      // 运行中按 Enter：给出明确反馈（"正在运行"），不再静默丢弃输入——
      // 用户之前反馈"卡死之后再输入对话就立刻结束/不执行"，根因之一是
      // 发送被静默吞掉且无任何提示。提示用户可点停止打断。
      setBlockedHint("任务仍在运行，可点击停止按钮打断后再发送");
      return;
    }
    onSend(trimmed);
    setText("");
    // 发送成功后记入历史（跨刷新持久化，重开对话按 ↑ 可回读），游标复位
    pushInputHistory(historyRef.current, trimmed);
    setHistIndex(null);
    // 发送后重置高度（回到单行）
    const el = textareaRef.current;
    if (el) el.style.height = "auto";
  };

  const handleChange = (e: ChangeEvent<HTMLTextAreaElement>) => {
    setText(e.target.value);
    // 手动编辑时退出历史浏览模式
    setHistIndex(null);
    setBlockedHint(null);
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
      if (historyRef.current.length === 0) return;
      e.preventDefault();
      if (histIndex === null) setDraft(text);
      const next =
        histIndex === null ? historyRef.current.length - 1 : Math.max(0, histIndex - 1);
      setHistIndex(next);
      setText(historyRef.current[next]);
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
        setText(historyRef.current[histIndex - 1]);
      }
    }
  };

  return (
    <div className="border-t border-[var(--color-border)] p-4">
      {blockedHint && (
        <div className="mb-2 flex items-center gap-1.5 rounded-md border border-[var(--color-risk-medium)]/40 bg-[var(--color-risk-medium)]/10 px-2.5 py-1.5 text-[11px] text-[var(--color-risk-medium)]">
          <TriangleAlert className="h-3 w-3 shrink-0" />
          {blockedHint}
        </div>
      )}
      <div
        className={cn(
          "glass flex items-end gap-2 rounded-2xl px-3 py-2 transition-all",
          "focus-within:border-[var(--color-accent)]/60",
          "focus-within:shadow-[0_0_20px_color-mix(in_srgb,var(--color-accent-grad-from)_18%,transparent)]",
        )}
      >
        <textarea
          ref={textareaRef}
          value={text}
          onChange={handleChange}
          onKeyDown={handleKey}
          placeholder={
            sendDisabled
              ? "正在运行…可提前输入下一条消息（完成后发送；或点停止打断）"
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
            title={sendDisabled ? "运行中，可点击停止打断后发送" : "发送"}
            className="anime-glow"
          >
            <Send className="h-4 w-4" />
          </Button>
        )}
      </div>
    </div>
  );
}
