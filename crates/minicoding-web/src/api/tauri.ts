/**
 * Tauri 桌面集成层（M9，见 AGENTS.md §8.1、design.md §26.5）。
 *
 * 仅在 Tauri WebView 中可调用；Web 模式下 `isTauri()` 返回 `false`，
 * 各函数不会被调用（由 `useDesktopStore.init()` 守护）。
 *
 * 桥接的 Rust 命令定义在 `crates/minicoding-desktop/src/main.rs`：
 * - `start_session`：启动 sidecar，返回 `{ port, pid }`
 * - `get_provider_config` / `save_provider_config`：读写 `~/.minicoding/config.toml`
 * - `store_api_key` / `load_api_key` / `delete_api_key`：OS keyring 凭证管理（C-04）
 * - `open_config_file`：用系统文件管理器打开配置目录
 * - `open_workspace_file`：用系统默认编辑器打开工作区文件（W-11）
 */

import { invoke } from "@tauri-apps/api/core";

/**
 * 是否运行在 Tauri WebView 中。
 *
 * Tauri 2.x 注入 `__TAURI_INTERNALS__` 全局对象，作为桥接入口。
 * Web 模式（Vite dev server / `minicoding serve --web`）下不存在该对象。
 */
export function isTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/** sidecar 启动信息（对应 Rust `SessionInfo`）。 */
export interface SessionInfo {
  /** sidecar 监听端口。 */
  port: number;
  /** sidecar 进程 PID。 */
  pid: number;
}

/** 小 LLM 配置（对应 Rust `SmallProviderConfig`，可选）。 */
export interface SmallProviderConfig {
  model: string;
  api_base: string | null;
  api_key: string | null;
}

/**
 * Provider 配置（对应 Rust `ProviderConfig`，`#[serde(default)]`）。
 *
 * **注意（C-04）**：`api_key` 字段不落 `config.toml` 明文，永远为空串；
 * 真实凭证由 `storeApiKey` 写入 OS keyring。
 */
export interface ProviderConfig {
  default: string;
  name: string | null;
  api_base: string;
  /** 凭证不落明文，由 keyring 单独管理（始终为空串）。 */
  api_key: string;
  model: string;
  timeout_sec: number;
  max_retries: number;
  small: SmallProviderConfig | null;
}

/** 启动 sidecar 会话，返回监听端口与 PID。 */
export function startSession(): Promise<SessionInfo> {
  return invoke<SessionInfo>("start_session");
}

/** 读取 `config.toml` 中的 provider 配置（无配置文件时返回默认值）。 */
export function getProviderConfig(): Promise<ProviderConfig> {
  return invoke<ProviderConfig>("get_provider_config");
}

/** 保存 provider 配置到 `config.toml`（原子写入，不含 api_key 明文）。 */
export function saveProviderConfig(provider: ProviderConfig): Promise<void> {
  return invoke<void>("save_provider_config", { provider });
}

/** 写入 API key 到 OS keyring（与 CLI 共享 entry，C-04）。 */
export function storeApiKey(apiKey: string): Promise<void> {
  return invoke<void>("store_api_key", { apiKey });
}

/** 从 OS keyring 读取 API key（`null` 表示未设置）。 */
export function loadApiKey(): Promise<string | null> {
  return invoke<string | null>("load_api_key");
}

/** 删除 keyring 中的 API key。 */
export function deleteApiKey(): Promise<void> {
  return invoke<void>("delete_api_key");
}

/** 用系统文件管理器打开配置文件所在目录，返回配置文件绝对路径。 */
export function openConfigFile(): Promise<string> {
  return invoke<string>("open_config_file");
}

/** 重启应用（编辑模式保存配置后调用，确保新 sidecar 配置生效）。 */
export function restartApp(): Promise<void> {
  return invoke<void>("restart_app");
}

/**
 * 用系统默认编辑器打开工作区文件（W-11 `open_workspace_file` 命令）。
 *
 * `path` 为相对 workdir 的路径（预览面板展示的相对路径）；Rust 侧将其
 * 拼接 workdir 后交给系统 opener（桌面端才可用，Web 模式调用会失败）。
 */
export function openWorkspaceFile(path: string): Promise<void> {
  return invoke<void>("open_workspace_file", { path });
}
