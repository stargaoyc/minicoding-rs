import { useState } from "react";
import { motion, AnimatePresence } from "framer-motion";
import { Settings, Loader2 } from "lucide-react";
import { Button } from "../ui/button";
import { useDesktopStore, type ProviderInput } from "../../stores/desktop";
import { cn } from "../../lib/utils";

/**
 * 首次启动配置弹窗（M9，见 AGENTS.md §8.1、design.md §26.5）。
 *
 * 当 `useDesktopStore.phase === 'needs-config'` 时由 `App.tsx` 渲染。
 * 收集 provider / api_base / model / api_key，提交后由 desktop store 保存
 * 配置（`config.toml` + OS keyring）并启动 sidecar。
 *
 * 视觉风格对齐 `PermissionDialog`（framer-motion overlay + glass panel）。
 */

/** Provider 选项与默认值（与 Rust `ProviderConfig::default` 对齐）。 */
const PROVIDER_OPTIONS: {
  value: string;
  label: string;
  api_base: string;
  model: string;
}[] = [
  { value: "openai", label: "OpenAI", api_base: "https://api.openai.com/v1", model: "gpt-4o" },
  { value: "anthropic", label: "Anthropic", api_base: "https://api.anthropic.com", model: "claude-sonnet-4-5-20250929" },
  { value: "ollama", label: "Ollama（本地）", api_base: "http://localhost:11434/v1", model: "llama3" },
];

const DEFAULTS = PROVIDER_OPTIONS[0];

export function SetupDialog() {
  const phase = useDesktopStore((s) => s.phase);
  const saveConfig = useDesktopStore((s) => s.saveConfig);
  const [saving, setSaving] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);

  const [provider, setProvider] = useState(DEFAULTS.value);
  const [apiBase, setApiBase] = useState(DEFAULTS.api_base);
  const [model, setModel] = useState(DEFAULTS.model);
  const [apiKey, setApiKey] = useState("");

  // 切换 provider 时同步默认 api_base / model（用户可后续手动修改）
  const handleProviderChange = (value: string) => {
    setProvider(value);
    const opt = PROVIDER_OPTIONS.find((o) => o.value === value);
    if (opt) {
      setApiBase(opt.api_base);
      setModel(opt.model);
    }
  };

  const handleSubmit = async (e: React.SubmitEvent<HTMLFormElement>) => {
    e.preventDefault();
    if (saving) return;

    // 简单校验：非 ollama 必须填 API key
    if (provider !== "ollama" && !apiKey.trim()) {
      setFormError("请填写 API Key（ollama 本地模式可留空）");
      return;
    }
    if (!apiBase.trim() || !model.trim()) {
      setFormError("请填写 API Base 和模型名");
      return;
    }

    setFormError(null);
    setSaving(true);
    const input: ProviderInput = {
      default: provider,
      api_base: apiBase.trim(),
      model: model.trim(),
      apiKey: apiKey.trim(),
    };
    try {
      await saveConfig(input);
      // saveConfig 成功后 phase → 'ready'，App.tsx 卸载本弹窗
    } catch (e) {
      setFormError(e instanceof Error ? e.message : String(e));
      setSaving(false);
    }
  };

  return (
    <AnimatePresence>
      {phase === "needs-config" && (
        <motion.div
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0 }}
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm"
        >
          <motion.div
            initial={{ scale: 0.95, opacity: 0, y: 10 }}
            animate={{ scale: 1, opacity: 1, y: 0 }}
            exit={{ scale: 0.95, opacity: 0, y: 10 }}
            transition={{ type: "spring", damping: 20, stiffness: 300 }}
            className="glass w-full max-w-md rounded-2xl p-6 shadow-2xl"
          >
            <form onSubmit={handleSubmit} className="space-y-4">
              {/* Header */}
              <div className="flex items-center gap-3">
                <div className="flex h-10 w-10 items-center justify-center rounded-xl bg-[var(--color-surface-2)]">
                  <Settings className="h-5 w-5 text-[var(--color-accent-hover)]" />
                </div>
                <div>
                  <h3 className="text-base font-semibold">首次启动配置</h3>
                  <p className="text-xs text-[var(--color-text-muted)]">
                    填写 Provider 信息以启动 sidecar
                  </p>
                </div>
              </div>

              {/* Provider */}
              <Field label="Provider">
                <select
                  value={provider}
                  onChange={(e) => handleProviderChange(e.target.value)}
                  disabled={saving}
                  className={cn(INPUT_CLASS, "cursor-pointer")}
                >
                  {PROVIDER_OPTIONS.map((o) => (
                    <option key={o.value} value={o.value}>
                      {o.label}
                    </option>
                  ))}
                </select>
              </Field>

              {/* API Base */}
              <Field label="API Base">
                <input
                  type="text"
                  value={apiBase}
                  onChange={(e) => setApiBase(e.target.value)}
                  disabled={saving}
                  placeholder="https://api.openai.com/v1"
                  className={INPUT_CLASS}
                />
              </Field>

              {/* Model */}
              <Field label="模型">
                <input
                  type="text"
                  value={model}
                  onChange={(e) => setModel(e.target.value)}
                  disabled={saving}
                  placeholder="gpt-4o"
                  className={INPUT_CLASS}
                />
              </Field>

              {/* API Key */}
              <Field label="API Key" hint={provider === "ollama" ? "ollama 本地模式可留空" : "存入 OS keyring，不落明文（C-04）"}>
                <input
                  type="password"
                  value={apiKey}
                  onChange={(e) => setApiKey(e.target.value)}
                  disabled={saving}
                  placeholder="sk-..."
                  className={INPUT_CLASS}
                  autoComplete="off"
                />
              </Field>

              {/* Error */}
              {formError && (
                <div className="rounded-lg bg-[var(--color-risk-high)]/10 px-3 py-2 text-xs text-[var(--color-risk-high)]">
                  {formError}
                </div>
              )}

              {/* Actions */}
              <Button type="submit" disabled={saving} className="w-full">
                {saving ? (
                  <>
                    <Loader2 className="h-4 w-4 animate-spin" />
                    正在保存…
                  </>
                ) : (
                  "保存并启动"
                )}
              </Button>
            </form>
          </motion.div>
        </motion.div>
      )}
    </AnimatePresence>
  );
}

/** 表单字段容器（label + 控件 + 可选 hint）。 */
function Field({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="space-y-1.5">
      <label className="text-xs font-medium text-[var(--color-text-muted)]">{label}</label>
      {children}
      {hint && <p className="text-[10px] text-[var(--color-text-muted)]">{hint}</p>}
    </div>
  );
}

/** 输入控件统一样式（与 ChatInput 风格对齐）。 */
const INPUT_CLASS = cn(
  "w-full rounded-lg border border-[var(--color-border)] bg-[var(--color-surface)] px-3 py-2",
  "text-sm text-[var(--color-text)] outline-none transition-colors",
  "focus:border-[var(--color-accent)]/50 placeholder:text-[var(--color-text-muted)]",
  "disabled:opacity-50",
);
