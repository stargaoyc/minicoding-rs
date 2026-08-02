//! Server 端 Runtime 构造（T-M8-2）。
//!
//! 与 `minicoding-cli::builder::build_runtime` 类似但简化——server 端无 TTY，
//! 恒用 `ServerPrompter`（HTTP 权限交互）；不依赖 `minicoding-cli`（依赖方向：
//! cli → server，不可反向）。
//!
//! 与 CLI builder 的差异：
//! - 无 `SessionLoadMode`（server 端每个 session 新建或由客户端指定 id 恢复）；
//! - 无 `ReplayPolicy`（server 端不处理 `--replay`）；
//! - 无 Hook/Journal（M8 MVP 不启用，后续可 feature gate）；
//! - prompter 恒为 `ServerPrompter`（外部注入，不由 builder 构造）。

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use minicoding_context::ContextManagerImpl;
use minicoding_core::config::{RuntimeConfig, config_hash};
use minicoding_core::memory::SessionSummarizer;
use minicoding_core::policy::{PermissionMode, PermissionPolicy, PermissionPrompter};
use minicoding_core::provider::LlmProvider;
use minicoding_core::runtime::{Runtime, RuntimeBuilder};
use minicoding_core::sandbox::{SandboxDriver, SandboxPolicy};
use minicoding_core::storage::AuditSink;
use minicoding_core::tool::ToolRegistry;
use minicoding_memory::SessionSummarizerImpl;
use minicoding_policy::BuiltinPolicy;
use minicoding_providers::{
    ANTHROPIC_PROVIDER_ID, AnthropicProvider, OLLAMA_DEFAULT_API_BASE, OLLAMA_PROVIDER_ID,
    OllamaProvider, OpenAiProvider, PROVIDER_ID as OPENAI_PROVIDER_ID, RetryConfig, RetryProvider,
    TiktokenTokenizer,
};
use minicoding_storage::{FileAuditSink, JsonlStorage};
use minicoding_tools::{
    register_readonly_tools, register_shell_tools, register_task_tools, register_write_tools,
};
use std::sync::Arc;

/// Server 端 Runtime 构造参数。
#[derive(Debug, Clone)]
pub struct ServerRuntimeParams {
    /// LLM provider 类型（`openai`/`anthropic`/`ollama`）。
    pub provider_kind: String,
    /// API base URL。
    pub api_base: String,
    /// API key（Ollama 可为空）。
    pub api_key: String,
    /// 模型名称。
    pub model: String,
    /// 工作目录。
    pub workdir: Utf8PathBuf,
    /// 系统 prompt 覆盖（`None` 用默认）。
    pub system: Option<String>,
    /// 初始权限模式。
    pub permission_mode: PermissionMode,
}

/// 构造 server 端 `Runtime`。
///
/// `prompter` 由 `SessionManager` 提供（`ServerPrompter`，共享 pending map）。
/// 沙箱策略默认 `WorkspaceWrite { workdir, [] }`。
///
/// # Errors
/// API key 缺失、provider 构造失败、存储目录不可用时返回错误。
#[allow(clippy::too_many_lines)] // 组装流程线性展开
pub fn build_runtime(
    params: &ServerRuntimeParams,
    prompter: Arc<dyn PermissionPrompter>,
) -> Result<Runtime> {
    let ServerRuntimeParams {
        provider_kind,
        api_base,
        api_key,
        model,
        workdir,
        system,
        permission_mode,
    } = params.clone();

    // 1. 构造 config
    let mut config = RuntimeConfig::default();
    config.provider.default.clone_from(&provider_kind);
    config.provider.api_base = api_base;
    config.provider.api_key = api_key;
    config.provider.model = model;

    // 2. 校验 API key（Ollama 免鉴权）
    let is_ollama = provider_kind == OLLAMA_PROVIDER_ID;
    if !is_ollama && config.provider.api_key.is_empty() {
        anyhow::bail!("API key 未配置：server 端需在 ServerRuntimeParams.api_key 中提供");
    }

    // 3. 构造 provider（含 RetryProvider 装饰器，C-13 bounded retries）
    let main_provider: Arc<dyn LlmProvider> =
        build_provider(&provider_kind, &config.provider).context("provider 构造失败")?;
    let retry_config = RetryConfig {
        max_retries: config.provider.max_retries,
        initial_backoff_ms: 500,
        max_backoff_ms: 30_000,
        request_timeout: std::time::Duration::from_secs(config.provider.timeout_sec),
    };
    let main_provider: Arc<dyn LlmProvider> =
        Arc::new(RetryProvider::new(main_provider, retry_config));

    // 4. 构造 system prompt
    let system_prompt =
        system.unwrap_or_else(|| "You are minicoding, a terminal AI coding assistant.".to_string());

    // 5. 构造 context manager（ContextManagerImpl + TiktokenTokenizer + 4 级压缩）
    let ctx: Arc<dyn minicoding_core::context::ContextManager> =
        match TiktokenTokenizer::new_for_model(&config.provider.model) {
            Ok(tokenizer) => {
                let context_window = 128_000;
                Arc::new(ContextManagerImpl::new(
                    system_prompt,
                    Arc::new(tokenizer),
                    context_window,
                    Some(main_provider.clone()),
                ))
            }
            Err(e) => {
                tracing::warn!("Tiktoken 构造失败（{e}），降级为 SimpleContextManager（无压缩）");
                Arc::new(minicoding_context::SimpleContextManager::new(
                    "You are minicoding, a terminal AI coding assistant.".to_string(),
                ))
            }
        };

    // 6. 构造 storage（JsonlStorage，崩溃安全）
    let sessions_dir = minicoding_core::paths::sessions_dir().context("无法确定会话存储目录")?;
    let storage = Arc::new(JsonlStorage::new(sessions_dir));

    // 7. 构造 tool registry（readonly + write + shell + task）
    let event_bus = minicoding_core::runtime::EventBus::new();
    let mut tools = ToolRegistry::new();
    register_readonly_tools(&mut tools);
    register_write_tools(&mut tools);
    register_shell_tools(&mut tools);
    register_task_tools(&mut tools, Some(event_bus.clone()));

    // 8. 构造权限策略 + 交互器
    let policy: Arc<dyn PermissionPolicy> = Arc::new(BuiltinPolicy::new());

    // 9. 构造审计 sink（FileAuditSink，0600 权限，AGENTS.md §5.5）
    let audit_path = minicoding_core::paths::audit_log_path().context("无法确定审计日志路径")?;
    let audit: Arc<dyn AuditSink> = Arc::new(FileAuditSink::new(audit_path));

    // 10. 构造 SessionSummarizer（small 未配置，用主 provider 做摘要）
    let summarizer: Arc<dyn SessionSummarizer> =
        Arc::new(SessionSummarizerImpl::new(main_provider.clone(), None));

    // 11. 组装 RuntimeBuilder
    let config_hash_val = config_hash(&config);
    let _ = config_hash_val; // Session::new 内部生成 ULID，config_hash 用于 resume 校验

    let mut builder = RuntimeBuilder::new()
        .provider(main_provider)
        .context(ctx)
        .storage(storage)
        .tools(tools)
        .config(config)
        .workdir(workdir.clone())
        .policy(policy)
        .prompter(prompter)
        .audit(audit)
        .session_summarizer(summarizer)
        .permission_mode(permission_mode)
        .events(event_bus);

    // 12. 注入沙箱驱动（与 CLI 一致，Linux Landlock / 降级 NoopDriver）
    let sandbox_policy = SandboxPolicy::WorkspaceWrite {
        workdir: workdir.clone(),
        writable: Vec::new(),
    };
    let driver: Arc<dyn SandboxDriver> = Arc::from(minicoding_sandbox::detect_driver());
    builder = builder
        .sandbox_driver(driver)
        .sandbox_policy(sandbox_policy);

    let mut rt = builder.build().map_err(anyhow::Error::msg)?;

    // 13. 补注册 plan.exit / task.spawn（依赖 Runtime 自身引用，chicken-and-egg）
    let plan_controller = rt.plan_controller();
    let subagent_runner = rt.subagent_runner();
    rt.register_dynamic_tool(Arc::new(minicoding_tools::PlanExit::new(
        plan_controller.clone(),
    )));
    rt.register_dynamic_tool(Arc::new(minicoding_tools::TaskSpawn::new(
        subagent_runner,
        plan_controller,
    )));

    Ok(rt)
}

/// 按 `provider_kind` 分派构造主 provider（与 CLI builder 一致）。
fn build_provider(
    kind: &str,
    cfg: &minicoding_core::config::ProviderConfig,
) -> Result<Arc<dyn LlmProvider>, minicoding_core::model::LlmError> {
    match kind {
        OPENAI_PROVIDER_ID => Ok(Arc::new(OpenAiProvider::new(
            &cfg.api_base,
            &cfg.api_key,
            &cfg.model,
        )?)),
        ANTHROPIC_PROVIDER_ID => Ok(Arc::new(AnthropicProvider::new(
            &cfg.api_base,
            &cfg.api_key,
            &cfg.model,
        )?)),
        OLLAMA_PROVIDER_ID => {
            let api_base = if cfg.api_base.is_empty() {
                OLLAMA_DEFAULT_API_BASE.to_string()
            } else {
                cfg.api_base.clone()
            };
            Ok(Arc::new(OllamaProvider::new(api_base, &cfg.model)?))
        }
        other => Err(minicoding_core::model::LlmError::Client {
            status: 400,
            body: format!("未知 provider `{other}`：支持 `openai`/`anthropic`/`ollama`"),
        }),
    }
}
