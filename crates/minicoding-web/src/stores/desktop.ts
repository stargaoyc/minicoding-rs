import { create } from "zustand";
import {
  isTauri,
  startSession,
  getProviderConfig,
  saveProviderConfig,
  getConfigRevision,
  storeApiKey,
  loadApiKey,
  saveContextConfig,
  type ProviderConfig,
} from "../api/tauri";
import { setApiBase, setApiToken } from "../api/client";
import { loadWebSettings, saveWebSettings, type WebProviderSettings } from "./webSettings";

/**
 * 桌面端启动阶段（M9，见 AGENTS.md §8.1、design.md §26.5）。
 *
 * - `loading`：初始化中（检查配置 / 启动 sidecar）
 * - `needs-config`：缺少 provider 配置或 API key，需弹 SetupDialog
 * - `ready`：sidecar 已启动，API base 已注入，可正常使用
 * - `error`：初始化失败（如 sidecar 启动失败、keyring 不可用）
 */
export type DesktopPhase = "loading" | "needs-config" | "ready" | "error";

/** SetupDialog 表单提交时的 provider 输入（API key 单独走 keyring，C-04）。 */
export interface ProviderInput {
  /** Provider 标识：`openai` / `anthropic` / `ollama`。 */
  default: string;
  /** API base URL。 */
  api_base: string;
  /** 模型名（如 `gpt-4o`）。 */
  model: string;
  /** API key（明文，仅用于写入 keyring，不落 config.toml）。 */
  apiKey: string;
  /** LLM 请求超时（秒，默认 120）。 */
  timeout_sec: number;
  /** LLM 请求最大重试（默认 3，C-13）。 */
  max_retries: number;
  /** 小 LLM 模型名（摘要/压缩降本，空串 = 不启用，见 design.md §3.8）。 */
  small_model: string;
  /** 单 turn 超时（秒，默认 600）。 */
  turn_timeout_sec: number;
  /** 上下文压缩开关（默认开启，C-18 软约束）。 */
  compress: boolean;
}

/** 桌面端状态（客户端状态，不进 TanStack Query，见 AGENTS.md §8.5）。 */
interface DesktopState {
  /** 当前启动阶段。 */
  phase: DesktopPhase;
  /** sidecar 启动后的 API base（`http://127.0.0.1:{port}`）；Web 模式为空串。 */
  apiBase: string;
  /** 错误信息（`phase === 'error'` 时非空）。 */
  error: string | null;
  /** 编辑模式保存成功后设置为 true，提示用户需要重启。 */
  restartRequired: boolean;
  /** 初始化桌面环境（App mount 时调用）。 */
  init: () => Promise<void>;
  /** 保存 provider 配置 + API key。首次启动后自动 init；编辑模式仅保存并提示重启。 */
  saveConfig: (input: ProviderInput) => Promise<void>;
  /** 清除 restartRequired 标志（用户确认后）。 */
  clearRestartRequired: () => void;
}

/** 默认 provider 配置（与 Rust `ProviderConfig::default` 对齐）。 */
const DEFAULT_PROVIDER: ProviderConfig = {
  default: "openai",
  name: null,
  api_base: "https://api.openai.com/v1",
  api_key: "",
  model: "gpt-4o",
  timeout_sec: 120,
  max_retries: 3,
  small: null,
};

/**
 * 判定是否需要首次配置。
 *
 * 满足以下任一条件即视为"需要配置"：
 * - `config.toml` 不存在（provider 字段为默认 `openai` + 默认 api_base）
 * - OS keyring 中无 API key
 *
 * 注意：用户主动配置过 `ollama`（无需 API key）时，`loadApiKey` 返回 `null`，
 * 此处不强制要求 key，仅当 provider 仍为默认且 key 缺失时才弹窗。
 */
function isProviderUnconfigured(provider: ProviderConfig, apiKey: string | null): boolean {
  // ollama 本地无需 API key，直接视为已配置
  if (provider.default === "ollama") return false;
  // 非 ollama provider 必须有 API key
  return apiKey === null || apiKey.length === 0;
}

export const useDesktopStore = create<DesktopState>((set, get) => ({
  phase: "loading",
  apiBase: "",
  error: null,
  restartRequired: false,

  clearRestartRequired: () => set({ restartRequired: false }),

  init: async () => {
    // Web 模式（非 Tauri）：直接就绪，API base 走同源 / VITE_API_BASE
    if (!isTauri()) {
      setApiBase("");
      // Web 模式从 localStorage 加载设置（供创建会话时注入）
      loadWebSettings();
      set({ phase: "ready", apiBase: "", error: null });
      return;
    }

    try {
      const provider = await getProviderConfig();
      const apiKey = await loadApiKey();

      if (isProviderUnconfigured(provider, apiKey)) {
        set({ phase: "needs-config", error: null });
        return;
      }

      // 配置就绪，启动 sidecar
      const session = await startSession();
      const base = `http://127.0.0.1:${session.port}`;
      setApiBase(base);
      setApiToken(session.token);
      set({ phase: "ready", apiBase: base, error: null });
    } catch (e) {
      set({
        phase: "error",
        error: e instanceof Error ? e.message : String(e),
        apiBase: "",
      });
    }
  },

  saveConfig: async (input) => {
    // Web 模式：存 localStorage，无需 keyring/sidecar
    if (!isTauri()) {
      try {
        const settings: WebProviderSettings = {
          default: input.default,
          api_base: input.api_base,
          model: input.model,
          timeout_sec: input.timeout_sec,
          max_retries: input.max_retries,
          small_model: input.small_model.trim() || undefined,
          turn_timeout_sec: input.turn_timeout_sec,
          compress: input.compress,
        };
        saveWebSettings(settings);
        // Web 模式无需重启，直接关闭弹窗
      } catch (e) {
        set({
          phase: "error",
          error: e instanceof Error ? e.message : String(e),
        });
      }
      return;
    }

    const wasReady = get().phase === "ready";

    try {
      // 构造完整 ProviderConfig（C-04：api_key 留空，凭证走 keyring）
      const provider: ProviderConfig = {
        ...DEFAULT_PROVIDER,
        default: input.default,
        api_base: input.api_base,
        model: input.model,
        api_key: "",
        timeout_sec: input.timeout_sec,
        max_retries: input.max_retries,
        small: input.small_model.trim()
          ? { model: input.small_model.trim(), api_base: null, api_key: null }
          : null,
      };
      // 保存前锁定 revision 基准（M-10 防陈旧写：并发的另一客户端已保存则 StaleWrite 拒绝）
      const revision = await getConfigRevision();
      await saveProviderConfig(provider, revision);
      // [context] 段：turn 超时 / 压缩开关（sidecar 启动时 `minicoding serve` 读取生效）
      await saveContextConfig({
        turn_timeout_sec: input.turn_timeout_sec,
        compress: input.compress,
      });

      // API key 单独走 keyring（ollama 无 key 时跳过）
      if (input.apiKey.length > 0) {
        await storeApiKey(input.apiKey);
      }

      if (wasReady) {
        // 编辑模式：sidecar 已在运行，旧进程无法安全杀死。
        // 标记 restartRequired，提示用户重启应用以应用新配置。
        // 直接调用 init() 会启动第二个 sidecar（资源泄漏），故不调用。
        set({ restartRequired: true });
      } else {
        // 首次启动：配置就绪，启动 sidecar
        await get().init();
      }
    } catch (e) {
      set({
        phase: "error",
        error: e instanceof Error ? e.message : String(e),
      });
    }
  },
}));
