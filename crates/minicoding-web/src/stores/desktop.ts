import { create } from "zustand";
import {
  isTauri,
  startSession,
  getProviderConfig,
  saveProviderConfig,
  storeApiKey,
  loadApiKey,
  type ProviderConfig,
} from "../api/tauri";
import { setApiBase } from "../api/client";

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
}

/** 桌面端状态（客户端状态，不进 TanStack Query，见 AGENTS.md §8.5）。 */
interface DesktopState {
  /** 当前启动阶段。 */
  phase: DesktopPhase;
  /** sidecar 启动后的 API base（`http://127.0.0.1:{port}`）；Web 模式为空串。 */
  apiBase: string;
  /** 错误信息（`phase === 'error'` 时非空）。 */
  error: string | null;
  /** 初始化桌面环境（App mount 时调用）。 */
  init: () => Promise<void>;
  /** 保存 provider 配置 + API key，完成后重新初始化。 */
  saveConfig: (input: ProviderInput) => Promise<void>;
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

  init: async () => {
    // Web 模式（非 Tauri）：直接就绪，API base 走同源 / VITE_API_BASE
    if (!isTauri()) {
      setApiBase("");
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
    if (!isTauri()) {
      set({ phase: "error", error: "Web 模式不支持保存 provider 配置" });
      return;
    }

    try {
      // 构造完整 ProviderConfig（保留默认 timeout/retries，C-04：api_key 留空）
      const provider: ProviderConfig = {
        ...DEFAULT_PROVIDER,
        default: input.default,
        api_base: input.api_base,
        model: input.model,
        api_key: "",
      };
      await saveProviderConfig(provider);

      // API key 单独走 keyring（ollama 无 key 时跳过）
      if (input.apiKey.length > 0) {
        await storeApiKey(input.apiKey);
      }

      // 重新初始化（启动 sidecar）
      await get().init();
    } catch (e) {
      set({
        phase: "error",
        error: e instanceof Error ? e.message : String(e),
      });
    }
  },
}));
