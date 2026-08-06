/**
 * Web 模式设置存储（localStorage，M9）。
 *
 * Web 模式（非 Tauri）下无 `config.toml` / OS keyring，provider 配置存 localStorage。
 * 创建会话时通过 `CreateSessionBody` 传递给 `minicoding-server`（C-04：API key
 * 由 server 端持有，不经前端传）。
 *
 * 仅存储非敏感配置：`provider` / `api_base` / `model`。
 */

const STORAGE_KEY = "minicoding-web-settings";

/** Web 模式 provider 配置（子集，不含 api_key/timeout 等服务端字段）。 */
export interface WebProviderSettings {
  /** Provider 标识：`openai` / `anthropic` / `ollama`。 */
  default: string;
  /** API base URL。 */
  api_base: string;
  /** 模型名。 */
  model: string;
}

/** 默认值（与 `PROVIDER_OPTIONS[0]` 对齐）。 */
const DEFAULTS: WebProviderSettings = {
  default: "openai",
  api_base: "https://api.openai.com/v1",
  model: "gpt-4o",
};

/** 从 localStorage 读取 Web 模式设置（无配置时返回默认值）。 */
export function loadWebSettings(): WebProviderSettings {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return { ...DEFAULTS };
    const parsed = JSON.parse(raw) as Partial<WebProviderSettings>;
    return {
      default: parsed.default ?? DEFAULTS.default,
      api_base: parsed.api_base ?? DEFAULTS.api_base,
      model: parsed.model ?? DEFAULTS.model,
    };
  } catch {
    return { ...DEFAULTS };
  }
}

/** 保存 Web 模式设置到 localStorage。 */
export function saveWebSettings(settings: WebProviderSettings): void {
  localStorage.setItem(STORAGE_KEY, JSON.stringify(settings));
}
