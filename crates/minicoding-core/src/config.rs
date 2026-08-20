//! 配置加载与合并（分层：`MINICODING_HOME` > project > user > 默认）。
//!
//! 支持 `env:VAR_NAME` / `env:VAR:-fallback` 环境变量语法（见 `tech-stack.md` §12）。
//! 支持 last-known-good 回退（解析失败时用上次成功的配置，见 `design.md` §12）。

use crate::hooks::OnHookError;
use crate::paths;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub mod watcher;
pub use watcher::ConfigWatcher;

/// Runtime 配置（分层加载）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeConfig {
    pub provider: ProviderConfig,
    pub context: ContextConfig,
    pub tools: ToolsConfig,
    pub storage: StorageConfig,
    pub hooks: HooksConfig,
}

/// LLM provider 配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub default: String,
    /// 自定义 provider 显示名（用于日志/metrics/UI，不影响协议分派）。
    ///
    /// 未设置（`None`）时回退到 `default`（如 `"openai"`/`"anthropic"`/`"ollama"`）。
    /// 设置后允许用户为 `OpenAI` 兼容 API（`DeepSeek`/`Moonshot`/`vLLM` 等）指定可读名称，
    /// 如 `name = "deepseek"` → 日志显示 `provider=deepseek` 而非 `provider=openai`。
    pub name: Option<String>,
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
            name: None,
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
    /// C-05：压缩前是否保留消息备份（默认 `false`，可选调试功能）。
    ///
    /// `true` 时 `compress_pipeline` 在压缩前 clone 原始消息到 `CompressResult.backup`，
    /// 供调试/回放分析。生产环境默认关闭以减少内存开销。
    pub backup_before_compress: bool,
    /// C-08：预测性压缩（默认 `false`）。
    ///
    /// `true` 时根据历史 turn token 增长估算，在超出窗口前提前 compact，
    /// 与反应式 compact 互补（见 `design.md` §3.9）。
    pub predictive_compact_enabled: bool,
    /// C-08：预测性压缩的基线增长量（默认 `15000` tokens）。
    ///
    /// 当历史增长数据不足时使用此基线估算下一 turn 的 token 增长。
    pub predictive_baseline_growth_tokens: usize,
    /// C-09：post-compact 后重新注入最近读过的文件数量上限（默认 `5`）。
    ///
    /// 压缩后从历史提取最近 read 过的文件路径，按预算截断重新注入，
    /// 避免模型重新 read（见 `design.md` §3.10）。
    pub post_compact_max_files: usize,
    /// C-09：post-compact 重新注入的 token 预算（默认 `50000`）。
    pub post_compact_token_budget: usize,
    /// C-09：post-compact 单文件最大 token 数（默认 `5000`）。
    pub post_compact_max_tokens_per_file: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            budget_ratio: 0.85,
            max_tool_iters: 50,
            turn_timeout_sec: 600,
            compress: true,
            backup_before_compress: false,
            predictive_compact_enabled: false,
            predictive_baseline_growth_tokens: 15_000,
            post_compact_max_files: 5,
            post_compact_token_budget: 50_000,
            post_compact_max_tokens_per_file: 5_000,
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
    /// 重复工具调用升级阈值（M-08，R-03 对齐 dsh）：单工具指纹连续命中
    /// `thresholds[i]` 轮时注入一级 system 提醒（软提醒，不替换输出、不终止）。
    /// 空数组 = 关闭软提醒，仅保留硬停止（整轮签名连续重复 ≥ 3 轮 → `Stopped`）。
    #[serde(default = "default_repeat_thresholds")]
    pub repeat_guard_thresholds: Vec<u32>,
}

fn default_repeat_thresholds() -> Vec<u32> {
    vec![3, 5, 8]
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            enabled_groups: vec!["core".into(), "fs".into()],
            fs_max_read_bytes: 1024 * 1024,
            shell_timeout_sec: 120,
            shell_max_output_bytes: 1024 * 1024,
            repeat_guard_thresholds: default_repeat_thresholds(),
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

/// Hooks 配置（`[hooks]`，见 `hooks.md` §6）。
///
/// 顶层 `[hooks]` 配置全局 `on_hook_error` 与 `default_timeout_sec`；
/// 各事件以 `[[hooks.<EventName>]]` 数组声明，每项为一个外部脚本 Hook。
///
/// ```toml
/// [hooks]
/// on_hook_error = "continue"
/// default_timeout_sec = 30
///
/// [[hooks.PreToolUse]]
/// matcher = "fs.write"
/// command = "prettier --write ${TOOL_INPUT_PATH}"
/// timeout_sec = 10
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HooksConfig {
    /// Hook 错误策略（默认 `continue`，见 `hooks.md` §6）。
    pub on_hook_error: OnHookError,
    /// 默认单 Hook 超时秒数（可被单个 Hook `timeout_sec` 覆盖）。
    pub default_timeout_sec: u64,
    /// `SessionStart` Hooks。
    #[serde(rename = "SessionStart")]
    pub session_start: Vec<HookEntry>,
    /// `UserPromptSubmit` Hooks。
    #[serde(rename = "UserPromptSubmit")]
    pub user_prompt_submit: Vec<HookEntry>,
    /// `PreToolUse` Hooks。
    #[serde(rename = "PreToolUse")]
    pub pre_tool_use: Vec<HookEntry>,
    /// `PostToolUse` Hooks。
    #[serde(rename = "PostToolUse")]
    pub post_tool_use: Vec<HookEntry>,
    /// `PostToolUseFailure` Hooks。
    #[serde(rename = "PostToolUseFailure")]
    pub post_tool_use_failure: Vec<HookEntry>,
    /// `PreCompact` Hooks。
    #[serde(rename = "PreCompact")]
    pub pre_compact: Vec<HookEntry>,
    /// `PostCompact` Hooks。
    #[serde(rename = "PostCompact")]
    pub post_compact: Vec<HookEntry>,
    /// `Stop` Hooks。
    #[serde(rename = "Stop")]
    pub stop: Vec<HookEntry>,
    /// `SubagentStop` Hooks。
    #[serde(rename = "SubagentStop")]
    pub subagent_stop: Vec<HookEntry>,
    /// `PermissionRequest` Hooks。
    #[serde(rename = "PermissionRequest")]
    pub permission_request: Vec<HookEntry>,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            on_hook_error: OnHookError::Continue,
            default_timeout_sec: 30,
            session_start: Vec::new(),
            user_prompt_submit: Vec::new(),
            pre_tool_use: Vec::new(),
            post_tool_use: Vec::new(),
            post_tool_use_failure: Vec::new(),
            pre_compact: Vec::new(),
            post_compact: Vec::new(),
            stop: Vec::new(),
            subagent_stop: Vec::new(),
            permission_request: Vec::new(),
        }
    }
}

impl HooksConfig {
    /// 统计配置中声明的 Hook 总数（诊断/`doctor` 用）。
    #[must_use]
    pub fn total_count(&self) -> usize {
        self.session_start.len()
            + self.user_prompt_submit.len()
            + self.pre_tool_use.len()
            + self.post_tool_use.len()
            + self.post_tool_use_failure.len()
            + self.pre_compact.len()
            + self.post_compact.len()
            + self.stop.len()
            + self.subagent_stop.len()
            + self.permission_request.len()
    }
}

/// 单个外部脚本 Hook 的配置（`[[hooks.<EventName>]]`，见 `hooks.md` §6）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HookEntry {
    /// 工具名 glob（`|` 分隔、`*` 通配），`None` = 所有工具。
    ///
    /// 仅对工具相关事件（`PreToolUse`/`PostToolUse`/`PostToolUseFailure`/
    /// `PermissionRequest`）有效；其他事件忽略此字段。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    /// 命令模板，支持 `${TOOL_INPUT_<KEY>}` 占位符（按工具 input 字段展开，
    /// 经 shell 转义防注入，见 `hooks.md` §6）。
    pub command: String,
    /// 单 Hook 超时秒数（`None` → 用 `[hooks] default_timeout_sec`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_sec: Option<u64>,
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
                if let (Ok(lkg_path), Ok(serialized)) =
                    (paths::last_known_good_path(), toml::to_string(&cfg))
                {
                    let _ = std::fs::write(lkg_path, serialized);
                }
                return Ok(cfg);
            }
            Err(e) => {
                // 解析失败，尝试 last-known-good 回退（let chains 合并嵌套 if）
                if let Ok(lkg_path) = paths::last_known_good_path()
                    && lkg_path.exists()
                    && let Ok(lkg_raw) = std::fs::read_to_string(&lkg_path)
                    && let Ok(mut lkg_cfg) = toml::from_str::<RuntimeConfig>(&lkg_raw)
                {
                    tracing::warn!("config.toml 解析失败 ({e})，回退到 last-known-good");
                    resolve_env_vars(&mut lkg_cfg);
                    return Ok(lkg_cfg);
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

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn hooks_config_default_is_empty() {
        let cfg = HooksConfig::default();
        assert_eq!(cfg.total_count(), 0);
        assert_eq!(cfg.on_hook_error, OnHookError::Continue);
        assert_eq!(cfg.default_timeout_sec, 30);
    }

    #[test]
    fn hooks_config_parses_from_toml() {
        let toml = r#"
[hooks]
on_hook_error = "deny"
default_timeout_sec = 15

[[hooks.PreToolUse]]
matcher = "fs.write"
command = "prettier --write ${TOOL_INPUT_path}"
timeout_sec = 10

[[hooks.PreToolUse]]
matcher = "shell.run"
command = "~/.minicoding/hooks/block-danger.sh"

[[hooks.PostToolUse]]
matcher = "fs.write|fs.edit"
command = "cargo fmt"

[[hooks.SessionStart]]
command = "git status --short"
"#;
        let cfg: RuntimeConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.hooks.on_hook_error, OnHookError::Deny);
        assert_eq!(cfg.hooks.default_timeout_sec, 15);
        assert_eq!(cfg.hooks.total_count(), 4);
        assert_eq!(cfg.hooks.pre_tool_use.len(), 2);
        assert_eq!(
            cfg.hooks.pre_tool_use[0].matcher.as_deref(),
            Some("fs.write")
        );
        assert_eq!(
            cfg.hooks.pre_tool_use[0].command,
            "prettier --write ${TOOL_INPUT_path}"
        );
        assert_eq!(cfg.hooks.pre_tool_use[0].timeout_sec, Some(10));
        assert_eq!(cfg.hooks.post_tool_use.len(), 1);
        assert_eq!(cfg.hooks.session_start.len(), 1);
        assert!(cfg.hooks.session_start[0].matcher.is_none());
    }

    #[test]
    fn hooks_config_empty_toml_uses_defaults() {
        let toml = "";
        let cfg: RuntimeConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.hooks.on_hook_error, OnHookError::Continue);
        assert_eq!(cfg.hooks.default_timeout_sec, 30);
        assert_eq!(cfg.hooks.total_count(), 0);
    }
}
