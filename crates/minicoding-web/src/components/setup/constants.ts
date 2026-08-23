/** Q5 拆分：SetupDialog 共享常量。 */
import { cn } from "../../lib/utils";

export const PROVIDER_OPTIONS: {
  value: string;
  label: string;
  api_base: string;
  model: string;
}[] = [
  { value: "openai", label: "OpenAI", api_base: "https://api.openai.com/v1", model: "gpt-4o" },
  {
    value: "anthropic",
    label: "Anthropic",
    api_base: "https://api.anthropic.com",
    model: "claude-sonnet-4-5-20250929",
  },
  {
    value: "ollama",
    label: "Ollama（本地）",
    api_base: "http://localhost:11434/v1",
    model: "llama3",
  },
];

/** 模型/上下文参数默认值（与 `RuntimeConfig::default()` 对齐）。 */
export const PARAM_DEFAULTS = {
  timeout_sec: 120,
  max_retries: 3,
  turn_timeout_sec: 600,
  compress: true,
};

export const DEFAULTS = PROVIDER_OPTIONS[0];

/** 输入控件统一样式。 */
export const INPUT_CLASS = cn(
  "w-full rounded-lg border border-[var(--color-border)] bg-[var(--color-surface)] px-3 py-2",
  "text-sm text-[var(--color-text)] outline-none transition-colors",
  "focus:border-[var(--color-accent)]/50 placeholder:text-[var(--color-text-muted)]",
  "disabled:opacity-50",
);
