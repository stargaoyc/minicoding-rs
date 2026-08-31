import { useState, useEffect } from "react";
import { motion, AnimatePresence } from "framer-motion";
import { Settings, Loader2, CheckCircle2 } from "lucide-react";
import { Button } from "../ui/button";
import { useDesktopStore, type ProviderInput } from "../../stores/desktop";
import { useUIStore } from "../../stores/ui";
import {
  getProviderConfig,
  getContextConfig,
  loadApiKey,
  isTauri,
  restartApp,
} from "../../api/tauri";
import { useServerConfig, useSetApiToken } from "../../hooks/useTurnControl";
import { loadServerToken, loadWebSettings, saveServerToken } from "../../stores/webSettings";
import { cn } from "../../lib/utils";
// Q5 拆分：常量与子组件外置
import { DEFAULTS, INPUT_CLASS, PARAM_DEFAULTS, PROVIDER_OPTIONS } from "./constants";
import { Field } from "./Field";
import { SectionTitle } from "./SectionTitle";

/**
 * 配置弹窗（M9，见 AGENTS.md §8.1、design.md §26.5）。
 *
 * 两种触发场景：
 * 1. **首次启动**（`phase === 'needs-config'`）：desktop store 检测到缺少配置时自动弹出
 * 2. **手动修改**（`settingsOpen === true`）：用户点击顶栏"设置"按钮手动打开
 *
 * 设置项分三组（W-19，见 features.md）：
 * - **模型**：provider / api_base / model / api_key（Tauri 模式）
 * - **模型参数**：请求超时 / 最大重试 / 小 LLM（摘要压缩降本）
 * - **上下文**：turn 超时 / 压缩开关
 *
 * 保存落点：Web 模式 → localStorage（新建会话经 `CreateSessionBody` 注入）；
 * Tauri 模式 → `config.toml`（`[provider]` + `[context]` 段）+ OS keyring。
 *
 * 编辑模式下会加载当前配置作为初始值；首次启动用 PROVIDER_OPTIONS 默认值。
 *
 * 视觉风格对齐 `PermissionDialog`（framer-motion overlay + glass panel）。
 */

/** Provider 选项与默认值（与 Rust `ProviderConfig::default` 对齐）。 */


export function SetupDialog() {
  const fetchServerConfig = useServerConfig();
  const setApiTokenClient = useSetApiToken();
  const phase = useDesktopStore((s) => s.phase);
  const saveConfig = useDesktopStore((s) => s.saveConfig);
  const restartRequired = useDesktopStore((s) => s.restartRequired);
  const clearRestartRequired = useDesktopStore((s) => s.clearRestartRequired);
  const settingsOpen = useUIStore((s) => s.settingsOpen);
  const setSettingsOpen = useUIStore((s) => s.setSettingsOpen);

  const [saving, setSaving] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [restarting, setRestarting] = useState(false);

  const [provider, setProvider] = useState(DEFAULTS.value);
  const [apiBase, setApiBase] = useState(DEFAULTS.api_base);
  const [model, setModel] = useState(DEFAULTS.model);
  const [apiKey, setApiKey] = useState("");
  // R10-04：Web 直连模式鉴权 token（`SERVER_TOKEN`，服务端启动时打印）
  const [serverToken, setServerToken] = useState(loadServerToken());
  // 模型参数 / 上下文参数（W-19，见 features.md）
  const [timeoutSec, setTimeoutSec] = useState(PARAM_DEFAULTS.timeout_sec);
  const [maxRetries, setMaxRetries] = useState(PARAM_DEFAULTS.max_retries);
  const [smallModel, setSmallModel] = useState("");
  const [turnTimeoutSec, setTurnTimeoutSec] = useState(PARAM_DEFAULTS.turn_timeout_sec);
  const [compress, setCompress] = useState(PARAM_DEFAULTS.compress);

  // 显示条件：首次启动（needs-config）或手动打开（settingsOpen）
  const visible = phase === "needs-config" || settingsOpen;
  const isEditMode = settingsOpen && phase === "ready";
  const webMode = !isTauri();

  const handleRestart = async () => {
    setRestarting(true);
    try {
      await restartApp();
    } catch {
      // restartApp 成功时进程已退出，catch 仅在出错时触发
      setRestarting(false);
      setFormError("重启失败，请手动关闭并重新打开应用");
    }
  };

  const handleClose = () => {
    if (restartRequired) {
      clearRestartRequired();
    }
    setSettingsOpen(false);
    setFormError(null);
  };

  // 编辑模式（settingsOpen）下，弹窗显示时加载当前配置作为初始值
  useEffect(() => {
    if (!settingsOpen) return;

    // Web 模式：localStorage 为主，参数缺失时用 server 默认兜底
    if (!isTauri()) {
      const settings = loadWebSettings();
      setProvider(settings.default || DEFAULTS.value);
      setApiBase(settings.api_base || DEFAULTS.api_base);
      setModel(settings.model || DEFAULTS.model);
      setApiKey(""); // Web 模式无 API key 字段
      setTimeoutSec(settings.timeout_sec ?? PARAM_DEFAULTS.timeout_sec);
      setMaxRetries(settings.max_retries ?? PARAM_DEFAULTS.max_retries);
      setSmallModel(settings.small_model ?? "");
      setTurnTimeoutSec(settings.turn_timeout_sec ?? PARAM_DEFAULTS.turn_timeout_sec);
      setCompress(settings.compress ?? PARAM_DEFAULTS.compress);
      // server 兜底：localStorage 缺失时读取 server 真实默认值
      fetchServerConfig()
        .then((cfg) => {
          setTimeoutSec((prev) => (settings.timeout_sec ? prev : cfg.timeout_sec));
          setMaxRetries((prev) => (settings.max_retries ? prev : cfg.max_retries));
          setSmallModel((prev) => (prev || cfg.small_model) ?? "");
          setTurnTimeoutSec((prev) => (settings.turn_timeout_sec ? prev : cfg.turn_timeout_sec));
          setCompress((prev) => (settings.compress !== undefined ? prev : cfg.compress));
        })
        .catch(() => {
          // server 不可达时保持 localStorage/默认值（设置面板仍可编辑）
        });
      return;
    }

    // Tauri 模式：config.toml（provider + context）+ keyring 加载
    setLoading(true);
    Promise.all([getProviderConfig(), getContextConfig(), loadApiKey()])
      .then(([cfg, ctx, key]) => {
        setProvider(cfg.default || DEFAULTS.value);
        setApiBase(cfg.api_base || DEFAULTS.api_base);
        setModel(cfg.model || DEFAULTS.model);
        setTimeoutSec(cfg.timeout_sec || PARAM_DEFAULTS.timeout_sec);
        setMaxRetries(cfg.max_retries || PARAM_DEFAULTS.max_retries);
        setSmallModel(cfg.small?.model ?? "");
        setTurnTimeoutSec(ctx.turn_timeout_sec || PARAM_DEFAULTS.turn_timeout_sec);
        setCompress(ctx.compress);
        setApiKey(key ?? "");
      })
      .catch((e) => {
        setFormError(`加载当前配置失败: ${e instanceof Error ? e.message : String(e)}`);
      })
      .finally(() => setLoading(false));
  }, [settingsOpen]);

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

    // Web 模式无需 API key（由 server 端持有，C-04）
    // Tauri 模式：非 ollama 必须填 API key（首次启动时），编辑模式允许留空
    if (!webMode && provider !== "ollama" && !apiKey.trim() && !isEditMode) {
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
      apiKey: webMode ? "" : apiKey.trim(),
      timeout_sec: timeoutSec,
      max_retries: maxRetries,
      small_model: smallModel,
      turn_timeout_sec: turnTimeoutSec,
      compress: compress,
    };
    try {
      // R10-04：Web 模式保存鉴权 token（localStorage）并应用到 HTTP/SSE 客户端
      if (webMode) {
        saveServerToken(serverToken.trim());
        setApiTokenClient(serverToken.trim());
      }
      await saveConfig(input);
      // saveConfig 成功后：
      // - Web 模式：已存 localStorage，直接关闭弹窗
      // - Tauri 首次启动：phase → 'ready'，App.tsx 卸载本弹窗
      // - Tauri 编辑模式：saveConfig 设置 restartRequired=true，显示重启提示
      if (webMode || (isEditMode && !useDesktopStore.getState().restartRequired)) {
        setSettingsOpen(false);
      }
    } catch (e) {
      setFormError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <AnimatePresence>
      {visible && (
        <motion.div
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0 }}
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm"
          onClick={isEditMode ? handleClose : undefined}
        >
          <motion.div
            initial={{ scale: 0.95, opacity: 0, y: 10 }}
            animate={{ scale: 1, opacity: 1, y: 0 }}
            exit={{ scale: 0.95, opacity: 0, y: 10 }}
            transition={{ type: "spring", damping: 20, stiffness: 300 }}
            className="glass w-full max-w-lg rounded-2xl p-6 shadow-2xl"
            onClick={(e) => e.stopPropagation()}
          >
            <form onSubmit={handleSubmit} className="space-y-4">
              {/* Header */}
              <div className="flex items-center gap-3">
                <div className="flex h-10 w-10 items-center justify-center rounded-xl bg-[var(--color-surface-2)]">
                  <Settings className="h-5 w-5 text-[var(--color-accent-hover)]" />
                </div>
                <div>
                  <h3 className="text-base font-semibold">
                    {isEditMode ? "设置" : "首次启动配置"}
                  </h3>
                  <p className="text-xs text-[var(--color-text-muted)]">
                    {webMode && isEditMode
                      ? "修改后将应用于新创建的会话"
                      : isEditMode
                        ? "修改 Provider 信息后需重启 sidecar 生效"
                        : "填写 Provider 信息以启动 sidecar"}
                  </p>
                </div>
              </div>

              {/* 编辑模式保存成功：提示重启 */}
              {restartRequired ? (
                <div className="space-y-4">
                  <div className="flex items-center gap-3 rounded-lg bg-[var(--color-accent)]/10 px-4 py-3">
                    <CheckCircle2 className="h-5 w-5 flex-shrink-0 text-[var(--color-accent-hover)]" />
                    <div className="text-sm">
                      <p className="font-medium">配置已保存</p>
                      <p className="text-xs text-[var(--color-text-muted)]">
                        需要重启应用以应用新的 sidecar 配置
                      </p>
                    </div>
                  </div>
                  <div className="flex gap-2">
                    <Button
                      type="button"
                      variant="secondary"
                      onClick={handleClose}
                      disabled={restarting}
                      className="flex-1"
                    >
                      稍后手动重启
                    </Button>
                    <Button
                      type="button"
                      onClick={handleRestart}
                      disabled={restarting}
                      className="flex-1"
                    >
                      {restarting ? (
                        <>
                          <Loader2 className="h-4 w-4 animate-spin" />
                          正在重启…
                        </>
                      ) : (
                        "立即重启"
                      )}
                    </Button>
                  </div>
                </div>
              ) : loading ? (
                <div className="flex items-center justify-center py-8">
                  <Loader2 className="h-6 w-6 animate-spin text-[var(--color-accent-hover)]" />
                  <span className="ml-2 text-sm text-[var(--color-text-muted)]">加载配置中…</span>
                </div>
              ) : (
                <>
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

                  {/* R10-04：Web 模式鉴权 token（`SERVER_TOKEN`，服务端启动时打印；
                      Tauri 模式由 sidecar 经 env 注入，无需手动填写） */}
                  {webMode && (
                    <Field
                      label="Server Token"
                      hint="服务端启动时打印的 SERVER_TOKEN=...（未配置则所有 API 返回 401）"
                    >
                      <input
                        type="password"
                        value={serverToken}
                        onChange={(e) => setServerToken(e.target.value)}
                        disabled={saving}
                        placeholder="粘贴 SERVER_TOKEN"
                        className={INPUT_CLASS}
                      />
                    </Field>
                  )}

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

                  {/* API Key（Web 模式不显示：API key 由 server 端持有，C-04） */}
                  {!webMode && (
                    <Field
                      label="API Key"
                      hint={
                        provider === "ollama"
                          ? "ollama 本地模式可留空"
                          : isEditMode
                            ? "存入 OS keyring，不落明文（C-04）。留空表示不修改"
                            : "存入 OS keyring，不落明文（C-04）"
                      }
                    >
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
                  )}

                  {/* 模型参数（W-19：请求超时 / 最大重试 / 小 LLM） */}
                  <SectionTitle>模型参数</SectionTitle>
                  <div className="grid grid-cols-2 gap-3">
                    <Field
                      label="请求超时（秒）"
                      hint="LLM 请求超时上限（默认 120s，C-07）"
                    >
                      <input
                        type="number"
                        min={10}
                        max={3600}
                        value={timeoutSec}
                        onChange={(e) => setTimeoutSec(Number(e.target.value))}
                        disabled={saving}
                        className={INPUT_CLASS}
                      />
                    </Field>
                    <Field label="最大重试" hint="LLM 请求失败重试次数（默认 3，C-13）">
                      <input
                        type="number"
                        min={0}
                        max={10}
                        value={maxRetries}
                        onChange={(e) => setMaxRetries(Number(e.target.value))}
                        disabled={saving}
                        className={INPUT_CLASS}
                      />
                    </Field>
                  </div>
                  <Field
                    label="小 LLM 模型（可选）"
                    hint="摘要/压缩降本（design.md §3.8）。留空表示不启用；Tauri 模式另需对应 API Base"
                  >
                    <input
                      type="text"
                      value={smallModel}
                      onChange={(e) => setSmallModel(e.target.value)}
                      disabled={saving}
                      placeholder={webMode ? "gpt-4o-mini" : "不启用（留空）"}
                      className={INPUT_CLASS}
                    />
                  </Field>

                  {/* 上下文参数（W-19：turn 超时 / 压缩开关） */}
                  <SectionTitle>上下文</SectionTitle>
                  <div className="grid grid-cols-2 gap-3 items-start">
                    <Field label="Turn 超时（秒）" hint="单轮任务超时（默认 600s）">
                      <input
                        type="number"
                        min={30}
                        max={7200}
                        value={turnTimeoutSec}
                        onChange={(e) => setTurnTimeoutSec(Number(e.target.value))}
                        disabled={saving}
                        className={INPUT_CLASS}
                      />
                    </Field>
                    <div className="space-y-1.5">
                      <label className="text-xs font-medium text-[var(--color-text-muted)]">
                        上下文压缩
                      </label>
                      <label className="flex cursor-pointer items-center justify-between rounded-lg border border-[var(--color-border)] bg-[var(--color-surface)] px-3 py-2">
                        <span className="text-sm">启用</span>
                        <input
                          type="checkbox"
                          checked={compress}
                          onChange={(e) => setCompress(e.target.checked)}
                          disabled={saving}
                          className="h-4 w-4 accent-[var(--color-accent)]"
                        />
                      </label>
                      <p className="text-[10px] text-[var(--color-text-muted)]">
                        超窗时自动压缩历史（C-18）
                      </p>
                    </div>
                  </div>

                  {/* Error */}
                  {formError && (
                    <div className="rounded-lg bg-[var(--color-risk-high)]/10 px-3 py-2 text-xs text-[var(--color-risk-high)]">
                      {formError}
                    </div>
                  )}

                  {/* Actions */}
                  <div className="flex gap-2">
                    {isEditMode && (
                      <Button
                        type="button"
                        variant="secondary"
                        onClick={handleClose}
                        disabled={saving}
                        className="flex-1"
                      >
                        取消
                      </Button>
                    )}
                    <Button type="submit" disabled={saving} className="flex-1">
                      {saving ? (
                        <>
                          <Loader2 className="h-4 w-4 animate-spin" />
                          正在保存…
                        </>
                      ) : webMode && isEditMode ? (
                        "保存"
                      ) : isEditMode ? (
                        "保存并重启 sidecar"
                      ) : (
                        "保存并启动"
                      )}
                    </Button>
                  </div>
                </>
              )}
            </form>
          </motion.div>
        </motion.div>
      )}
    </AnimatePresence>
  );
}


