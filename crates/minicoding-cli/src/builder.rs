//! Runtime 组装：根据 CLI 参数与环境变量构造 `Runtime`。
//!
//! 组装顺序：config → provider → context → storage → tools → policy/prompter/audit
//! → `RuntimeBuilder`。

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use minicoding_context::SimpleContextManager;
use minicoding_core::config::{RuntimeConfig, config_hash};
use minicoding_core::policy::{PermissionPolicy, PermissionPrompter};
use minicoding_core::runtime::{Runtime, RuntimeBuilder};
use minicoding_core::storage::AuditSink;
use minicoding_core::tool::ToolRegistry;
use minicoding_policy::{BuiltinPolicy, InteractivePrompter, NonInteractivePrompter};
use minicoding_providers::OpenAiProvider;
use minicoding_storage::{FileAuditSink, JsonlStorage};
use minicoding_tools::{register_readonly_tools, register_shell_tools, register_write_tools};
use std::io::IsTerminal;
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
    register_write_tools(&mut tools);
    register_shell_tools(&mut tools);

    // 7. 构造权限策略 + 交互器（C-01：副作用必须经权限）
    //    TTY → InteractivePrompter（stdin 读 y/n）；非 TTY → NonInteractivePrompter
    //    （恒 Deny，CI 安全默认，见 design.md §9.2）
    let policy: Arc<dyn PermissionPolicy> = Arc::new(BuiltinPolicy::new());
    let prompter: Arc<dyn PermissionPrompter> = if std::io::stdin().is_terminal() {
        Arc::new(InteractivePrompter::new())
    } else {
        tracing::warn!("stdin 非 TTY，切换为 NonInteractivePrompter（副作用工具将被拒绝）");
        Arc::new(NonInteractivePrompter::new())
    };

    // 8. 构造审计 sink（AGENTS.md §5.5：权限决策必须落 audit.log，0600 权限）
    let audit_path = minicoding_core::paths::audit_log_path().context("无法确定审计日志路径")?;
    let audit: Arc<dyn AuditSink> = Arc::new(FileAuditSink::new(audit_path));

    // 9. 解析 workdir
    let workdir_path = Utf8PathBuf::from(workdir)
        .canonicalize_utf8()
        .unwrap_or_else(|_| Utf8PathBuf::from(workdir));

    // 10. 组装 Runtime
    let config_hash = config_hash(&config);
    let rt = RuntimeBuilder::new()
        .provider(Arc::new(provider))
        .context(Arc::new(ctx))
        .storage(Arc::new(storage))
        .tools(tools)
        .config(config)
        .workdir(workdir_path)
        .policy(policy)
        .prompter(prompter)
        .audit(audit)
        .build()
        .map_err(anyhow::Error::msg)?;

    // config_hash 需通过 builder 设置，但 builder.config() 已计算 hash
    let _ = config_hash; // builder.config() 内部已调用 config_hash
    Ok(rt)
}
