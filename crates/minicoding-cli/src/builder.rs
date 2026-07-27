//! Runtime 组装：根据 CLI 参数与环境变量构造 `Runtime`。
//!
//! 组装顺序：config → provider → context → storage → tools → `RuntimeBuilder`。

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use minicoding_context::SimpleContextManager;
use minicoding_core::config::{RuntimeConfig, config_hash};
use minicoding_core::runtime::{Runtime, RuntimeBuilder};
use minicoding_core::tool::ToolRegistry;
use minicoding_providers::OpenAiProvider;
use minicoding_storage::JsonlStorage;
use minicoding_tools::register_readonly_tools;
use std::sync::Arc;

/// 从 CLI 参数构建 `Runtime`。
///
/// # Errors
/// API key 缺失、provider 构造失败、存储目录不可用时返回错误。
#[allow(clippy::needless_pass_by_value)]
pub fn build_runtime(
    api_base: Option<&str>,
    api_key: Option<&str>,
    model: Option<&str>,
    workdir: &str,
    system: Option<&str>,
) -> Result<Runtime> {
    // 1. 加载配置
    let mut config = RuntimeConfig::default();
    // 环境变量快捷覆盖
    if let Ok(base) = std::env::var("OPENAI_API_BASE") {
        config.provider.api_base = base;
    }
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        config.provider.api_key = key;
    }
    if let Ok(m) = std::env::var("OPENAI_MODEL") {
        config.provider.model = m;
    }
    // CLI 参数覆盖
    if let Some(base) = api_base {
        config.provider.api_base = base.to_string();
    }
    if let Some(key) = api_key {
        config.provider.api_key = key.to_string();
    }
    if let Some(m) = model {
        config.provider.model = m.to_string();
    }

    // 2. 校验 API key
    if config.provider.api_key.is_empty() {
        anyhow::bail!("API key 未配置：请设置 OPENAI_API_KEY 环境变量或使用 --api-key 参数");
    }

    // 3. 构造 provider
    let provider = OpenAiProvider::new(
        &config.provider.api_base,
        &config.provider.api_key,
        &config.provider.model,
    )
    .context("OpenAI provider 构造失败")?;

    // 4. 构造 context manager
    let ctx = match system {
        Some(s) => SimpleContextManager::new(s.to_string()),
        None => SimpleContextManager::with_default_system(),
    };

    // 5. 构造 storage
    let sessions_dir = minicoding_core::paths::sessions_dir().context("无法确定会话存储目录")?;
    let storage = JsonlStorage::new(sessions_dir);

    // 6. 构造 tool registry
    let mut tools = ToolRegistry::new();
    register_readonly_tools(&mut tools);

    // 7. 解析 workdir
    let workdir_path = Utf8PathBuf::from(workdir)
        .canonicalize_utf8()
        .unwrap_or_else(|_| Utf8PathBuf::from(workdir));

    // 8. 组装 Runtime
    let config_hash = config_hash(&config);
    let rt = RuntimeBuilder::new()
        .provider(Arc::new(provider))
        .context(Arc::new(ctx))
        .storage(Arc::new(storage))
        .tools(tools)
        .config(config)
        .workdir(workdir_path)
        .build()
        .map_err(anyhow::Error::msg)?;

    // config_hash 需通过 builder 设置，但 builder.config() 已计算 hash
    let _ = config_hash; // builder.config() 内部已调用 config_hash
    Ok(rt)
}
