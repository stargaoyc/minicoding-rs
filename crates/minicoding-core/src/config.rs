//! 配置加载与合并（分层：`MINICODING_HOME` > project > user > 默认）。
//!
//! 支持 `env:VAR_NAME` / `env:VAR:-fallback` 环境变量语法（见 `tech-stack.md` §12）。
//! 支持 last-known-good 回退（解析失败时用上次成功的配置，见 `design.md` §12）。

use crate::paths;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Runtime 配置（分层加载）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeConfig {
    pub provider: ProviderConfig,
    pub context: ContextConfig,
    pub tools: ToolsConfig,
    pub storage: StorageConfig,
}

/// LLM provider 配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub default: String,
    pub api_base: String,
    /// API key，支持 `env:VAR_NAME` / `env:VAR:-fallback` 语法。
    pub api_key: String,
    pub model: String,
    pub timeout_sec: u64,
    pub max_retries: u32,
    /// 独立小 LLM 配置（摘要/compact/memory 提取用，见 `design.md` §3.8、`modules.md` §10.3）。
    ///
    /// 未设置（`None`）时与主 provider 相同；设置后可配更便宜模型降本。
    /// `api_base`/`api_key` 为 `None` 时继承主 provider。
    pub small: Option<SmallProviderConfig>,
}

/// 小 LLM 配置（`[provider.small]`，M2 roadmap L90）。
///
/// 仅 `model` 必填；`api_base`/`api_key` 为 `None` 时继承主 `[provider]` 配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmallProviderConfig {
    /// 小模型名称（如 `gpt-4o-mini`）。
    pub model: String,
    /// API base URL（`None` → 继承主 provider）。
    pub api_base: Option<String>,
    /// API key（`None` → 继承主 provider）。
    pub api_key: Option<String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            default: "openai".into(),
            api_base: "https://api.openai.com/v1".into(),
            api_key: String::new(),
            model: "gpt-4o-mini".into(),
            timeout_sec: 120,
            max_retries: 3,
            small: None,
        }
    }
}

/// 上下文配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    pub budget_ratio: f32,
    pub max_tool_iters: u32,
    pub turn_timeout_sec: u64,
    /// 是否启用压缩管道（默认 `true`）。
    ///
    /// `false` 时 `build_chat_request` 跳过压缩直通（见 `docs/design.md` §3.3、
    /// AGENTS.md C-18 上下文经济软约束）。配置项 `[context] compress = false`。
    pub compress: bool,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            budget_ratio: 0.85,
            max_tool_iters: 50,
            turn_timeout_sec: 600,
            compress: true,
        }
    }
}

/// 工具配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub enabled_groups: Vec<String>,
    pub fs_max_read_bytes: usize,
    pub shell_timeout_sec: u64,
    pub shell_max_output_bytes: usize,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            enabled_groups: vec!["core".into(), "fs".into()],
            fs_max_read_bytes: 1024 * 1024,
            shell_timeout_sec: 120,
            shell_max_output_bytes: 1024 * 1024,
        }
    }
}

/// 存储配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub dir: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            dir: "~/.minicoding/sessions".into(),
        }
    }
}

/// 解析 `env:VAR_NAME` 或 `env:VAR:-fallback` 语法。
/// 返回解析后的值；若变量缺失且无 fallback，返回 `None`。
fn resolve_env_syntax(s: &str) -> Option<String> {
    if let Some(rest) = s.strip_prefix("env:") {
        // env:VAR:-fallback
        if let Some((var, fallback)) = rest.split_once(":-") {
            return Some(std::env::var(var).unwrap_or_else(|_| fallback.to_string()));
        }
        // env:VAR_NAME
        return std::env::var(rest).ok();
    }
    // 兼容 ${VAR_NAME} 语法（MCP env 段）
    if let Some(rest) = s.strip_prefix("${").and_then(|r| r.strip_suffix('}')) {
        return std::env::var(rest).ok();
    }
    Some(s.to_string())
}

/// 解析配置中所有 `env:` 语法的字段。
pub fn resolve_env_vars(config: &mut RuntimeConfig) {
    if let Some(key) = resolve_env_syntax(&config.provider.api_key) {
        config.provider.api_key = key;
    }
    if let Some(base) = resolve_env_syntax(&config.provider.api_base) {
        config.provider.api_base = base;
    }
}

/// 加载配置（分层：config.toml > last-known-good > 默认）。
///
/// 解析成功时原子写入 last-known-good；失败时回退。
///
/// # Errors
/// 仅当配置文件存在但解析失败 **且** last-known-good 也不可用时返回错误。
pub fn load_config() -> Result<RuntimeConfig, String> {
    let config_path = paths::config_path().map_err(|e| e.to_string())?;

    if config_path.exists() {
        let raw = std::fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
        match toml::from_str::<RuntimeConfig>(&raw) {
            Ok(mut cfg) => {
                resolve_env_vars(&mut cfg);
                // 原子写入 last-known-good
                if let Ok(lkg_path) = paths::last_known_good_path() {
                    if let Ok(serialized) = toml::to_string(&cfg) {
                        let _ = std::fs::write(lkg_path, serialized);
                    }
                }
                return Ok(cfg);
            }
            Err(e) => {
                // 解析失败，尝试 last-known-good 回退
                if let Ok(lkg_path) = paths::last_known_good_path() {
                    if lkg_path.exists() {
                        if let Ok(lkg_raw) = std::fs::read_to_string(&lkg_path) {
                            if let Ok(mut lkg_cfg) = toml::from_str::<RuntimeConfig>(&lkg_raw) {
                                tracing::warn!(
                                    "config.toml 解析失败 ({e})，回退到 last-known-good"
                                );
                                resolve_env_vars(&mut lkg_cfg);
                                return Ok(lkg_cfg);
                            }
                        }
                    }
                }
                return Err(format!("config.toml 解析失败且无 last-known-good: {e}"));
            }
        }
    }

    // 无配置文件，用默认值
    let mut cfg = RuntimeConfig::default();
    resolve_env_vars(&mut cfg);
    Ok(cfg)
}

/// 计算 config hash（用于 resume 时校验一致性）。
#[must_use]
pub fn config_hash(cfg: &RuntimeConfig) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    cfg.provider.model.hash(&mut h);
    cfg.provider.api_base.hash(&mut h);
    cfg.context.max_tool_iters.hash(&mut h);
    h.finish()
}

/// 从环境变量构建 provider 配置的快捷方法（用于 CLI `--provider` 覆盖）。
#[must_use]
pub fn provider_from_env() -> ProviderConfig {
    let mut p = ProviderConfig::default();
    if let Ok(base) = std::env::var("OPENAI_API_BASE") {
        p.api_base = base;
    }
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        p.api_key = key;
    } else {
        p.api_key = "env:OPENAI_API_KEY".into();
    }
    if let Ok(model) = std::env::var("OPENAI_MODEL") {
        p.model = model;
    }
    p
}

/// 占位：环境变量映射（用于 Hook/MCP 子进程环境）。
#[must_use]
// 返回 HashMap 需固定 hasher 以保证子进程 env 收集语义确定
#[allow(clippy::implicit_hasher)]
pub fn sanitize_env(env: &HashMap<String, String>) -> HashMap<String, String> {
    env.iter()
        .filter(|(k, _)| !k.contains("API_KEY") && !k.contains("TOKEN") && !k.contains("SECRET"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}
