/**
 * Web 模式设置存储（localStorage，M9）。
 *
 * Web 模式（非 Tauri）下无 `config.toml` / OS keyring，provider 配置存 localStorage。
 * 创建会话时通过 `CreateSessionBody` 传递给 `minicoding-server`（C-04：API key
 * 由 server 端持有，不经前端传）。
 *
 * 仅存储非敏感配置：`provider` / `api_base` / `model` / 模型参数 / 上下文参数。
 * 鉴权 token（R10-04）另存 `minicoding.serverToken` 键，`setApiToken` 在 App 启动时读取。
 * 各字段可选——缺失时新建会话不传对应 body 字段，由 server 端默认值兜底
 * （与 `GET /config` 返回的 server 默认一致）。
 */

const STORAGE_KEY = "minicoding-web-settings";

/** R10-04：鉴权 token 的 localStorage 键（Web 直连模式，服务端 `SERVER_TOKEN`）。 */
const TOKEN_KEY = "minicoding.serverToken";

/** 读取持久化的鉴权 token（Web 直连模式；缺失返回空串）。 */
export function loadServerToken(): string {
  try {
    return localStorage.getItem(TOKEN_KEY) ?? "";
  } catch {
    return "";
  }
}

/** 持久化鉴权 token（`setApiToken` 由 App 启动时调用）。 */
export function saveServerToken(token: string): void {
  try {
    if (token) {
      localStorage.setItem(TOKEN_KEY, token);
    } else {
      localStorage.removeItem(TOKEN_KEY);
    }
  } catch {
    // localStorage 不可用时仅内存 token 可用，不阻塞
  }
}

/** Web 模式 provider + 模型/上下文参数（子集，不含 api_key，C-04）。 */
export interface WebProviderSettings {
  /** Provider 标识：`openai` / `anthropic` / `ollama`。 */
  default: string;
  /** API base URL。 */
  api_base: string;
  /** 模型名。 */
  model: string;
  /** LLM 请求超时（秒，默认 120）。 */
  timeout_sec?: number;
  /** LLM 请求最大重试（默认 3，C-13）。 */
  max_retries?: number;
  /** 小 LLM 模型名（摘要/压缩降本，`undefined` 不启用，见 design.md §3.8）。 */
  small_model?: string;
  /** 单 turn 超时（秒，默认 600）。 */
  turn_timeout_sec?: number;
  /** 上下文压缩开关（默认开启，C-18 软约束）。 */
  compress?: boolean;
}

/** 默认值（与 `PROVIDER_OPTIONS[0]` 对齐）。 */
const DEFAULTS: WebProviderSettings = {
  default: "openai",
  api_base: "https://api.openai.com/v1",
  model: "gpt-4o",
  timeout_sec: 120,
  max_retries: 3,
  small_model: undefined,
  turn_timeout_sec: 600,
  compress: true,
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
      timeout_sec: parsed.timeout_sec ?? DEFAULTS.timeout_sec,
      max_retries: parsed.max_retries ?? DEFAULTS.max_retries,
      small_model: parsed.small_model ?? DEFAULTS.small_model,
      turn_timeout_sec: parsed.turn_timeout_sec ?? DEFAULTS.turn_timeout_sec,
      compress: parsed.compress ?? DEFAULTS.compress,
    };
  } catch {
    return { ...DEFAULTS };
  }
}

/** 保存 Web 模式设置到 localStorage。 */
export function saveWebSettings(settings: WebProviderSettings): void {
  localStorage.setItem(STORAGE_KEY, JSON.stringify(settings));
}
