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
use minicoding_core::config::{ProviderConfig, RuntimeConfig, SmallProviderConfig, config_hash};
use minicoding_core::memory::SessionSummarizer;
use minicoding_core::model::Session;
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
    /// 自定义 provider 显示名（用于日志/metrics/UI，不影响协议分派）。
    ///
    /// `None` 时回退到 `provider_kind`；设置后允许为 `OpenAI` 兼容 API
    /// （`DeepSeek`/`Moonshot`/`vLLM` 等）指定可读名称。与 CLI `--provider-name` 对齐。
    pub provider_name: Option<String>,
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
    /// 沙箱策略（`--preset` 解析结果，见 `minicoding_policy::Preset`）。
    pub sandbox_policy: SandboxPolicy,
    /// LLM 请求超时（秒，默认 120）。
    pub timeout_sec: u64,
    /// LLM 请求最大重试（默认 3，C-13 bounded retries）。
    pub max_retries: u32,
    /// 小 LLM 模型名（`None` 不启用：摘要/压缩用主 provider，见 `design.md` §3.8）。
    pub small_model: Option<String>,
    /// 单 turn 超时（秒，默认 600）。
    pub turn_timeout_sec: u64,
    /// 上下文压缩开关（默认开启）。
    pub compress: bool,
}

/// 构造 server 端 `Runtime`。
///
/// `prompter` 由 `SessionManager` 提供（`ServerPrompter`，共享 pending map）。
/// `preloaded` 为 `Some` 时构造的 Runtime 使用该会话（恢复历史会话用，
/// `SessionManager::restore_session`），其消息已落盘，后续 `storage.append`
/// 写入同一会话文件；调用方需另行调用 `Runtime::restore_history` 回填上下文。
/// 沙箱策略默认 `WorkspaceWrite { workdir, [] }`。
///
/// # Errors
/// API key 缺失、provider 构造失败、存储目录不可用时返回错误。
#[allow(clippy::too_many_lines)] // 组装流程线性展开
pub fn build_runtime(
    params: &ServerRuntimeParams,
    prompter: Arc<dyn PermissionPrompter>,
    preloaded: Option<Session>,
) -> Result<Runtime> {
    let ServerRuntimeParams {
        provider_kind,
        provider_name,
        api_base,
        api_key,
        model,
        workdir,
        system,
        permission_mode,
        sandbox_policy,
        timeout_sec,
        max_retries,
        small_model,
        turn_timeout_sec,
        compress,
    } = params.clone();

    // 1. 构造 config
    let mut config = RuntimeConfig::default();
    config.provider.default.clone_from(&provider_kind);
    config.provider.name = provider_name;
    config.provider.api_base = api_base;
    config.provider.api_key = api_key;
    config.provider.model = model;
    config.provider.timeout_sec = timeout_sec;
    config.provider.max_retries = max_retries;
    // 小 LLM（摘要/压缩降本，`design.md` §3.8）：api_base/api_key 继承主 provider
    config.provider.small = small_model.map(|m| SmallProviderConfig {
        model: m,
        api_base: None,
        api_key: None,
    });
    config.context.turn_timeout_sec = turn_timeout_sec;
    config.context.compress = compress;

    // 2. 校验 API key（Ollama 免鉴权）
    //    C-04：sidecar 模式下 API key 不通过参数/env 传递，从 OS keyring fallback 读取。
    //    与 CLI `cred.rs` 共享 `KEYRING_SERVICE = "minicoding"` / `KEYRING_ACCOUNT = "openai_api_key"`。
    let is_ollama = provider_kind == OLLAMA_PROVIDER_ID;
    if !is_ollama && config.provider.api_key.is_empty() {
        match load_api_key_from_keyring() {
            Ok(Some(key)) => {
                tracing::info!("API key 从 OS keyring 加载（sidecar/serve 模式 fallback）");
                config.provider.api_key = key;
            }
            Ok(None) => {
                anyhow::bail!(
                    "API key 未配置：CLI `--api-key` / 环境变量 `OPENAI_API_KEY` / OS keyring / `minicoding cred store` 均未提供"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "OS keyring 不可用，API key fallback 失败");
                anyhow::bail!(
                    "API key 未配置且 keyring 不可用：{e}。请用 `minicoding cred store` 或 `--api-key` 提供"
                );
            }
        }
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
        Arc::new(RetryProvider::new(main_provider, retry_config.clone()));

    // 3b. 构造 small provider（gap-4：[provider.small] 独立小 LLM 配置，与 CLI 一致）
    //     未配置时退化为 `None`，由后续逻辑回退到主 provider。
    //     配置 `api_base`/`api_key` 为 `None` 时继承主 provider（典型：同一 OpenAI
    //     账号但换便宜模型做摘要/压缩，降本见 `design.md` §3.8）。
    //     small provider 始终用 OpenAI 兼容协议（与 CLI builder 对齐）。
    let small_provider: Option<Arc<dyn LlmProvider>> = match &config.provider.small {
        Some(small_cfg) => match build_small_provider(small_cfg, &config.provider) {
            Ok(p) => {
                let p: Arc<dyn LlmProvider> = Arc::new(p);
                Some(Arc::new(RetryProvider::new(p, retry_config)))
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "small provider 构造失败，摘要/压缩回退到主 provider"
                );
                None
            }
        },
        None => None,
    };
    // 摘要用 provider：small 优先，回退主 provider（保证 L2 摘要始终可用）。
    let summary_provider: Arc<dyn LlmProvider> = small_provider
        .clone()
        .map_or_else(|| main_provider.clone(), |p| p);

    // 4. 构造 system prompt
    let system_prompt =
        system.unwrap_or_else(|| "You are minicoding, a terminal AI coding assistant.".to_string());

    // 5. 构造 context manager（ContextManagerImpl + TiktokenTokenizer + 4 级压缩）
    //    L2 摘要 provider 用 small（如有）降本，回退到主 provider（与 CLI 一致）。
    let ctx: Arc<dyn minicoding_core::context::ContextManager> =
        match TiktokenTokenizer::new_for_model(&config.provider.model) {
            Ok(tokenizer) => {
                let context_window = 128_000;
                Arc::new(ContextManagerImpl::new(
                    system_prompt,
                    Arc::new(tokenizer),
                    context_window,
                    Some(summary_provider.clone()),
                ))
            }
            Err(e) => {
                tracing::warn!("Tiktoken 构造失败（{e}），降级为 SimpleContextManager（无压缩）");
                Arc::new(minicoding_context::SimpleContextManager::new(
                    "You are minicoding, a terminal AI coding assistant.".to_string(),
                ))
            }
        };

    // 6. 构造 storage（JsonlStorage，崩溃安全）+ EventStore + SnapshotStore（Event Sourcing）
    //    与 CLI 一致，EventStore/SnapshotStore 与 JsonlStorage 共用 sessions_dir，
    //    支持 SSE cursor durable recovery（见 `design.md` §25.5）。
    let sessions_dir = minicoding_core::paths::sessions_dir().context("无法确定会话存储目录")?;
    let storage = Arc::new(JsonlStorage::new(sessions_dir.clone()));
    let event_store: Arc<dyn minicoding_core::storage::EventStore> = Arc::new(
        minicoding_storage::JsonlEventStore::new(sessions_dir.clone()),
    );
    let snapshot_store: Arc<dyn minicoding_core::storage::SnapshotStore> =
        Arc::new(minicoding_storage::JsonlSnapshotStore::new(sessions_dir));

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

    // 10. 构造 SessionSummarizer（gap-5/T-M3-6：会话结束时生成摘要落盘 index.json）
    //     primary = small provider（如有，便宜模型降本），secondary = 主 provider
    //     （降级兜底）。降级链终端为启发式兜底（C-29），永不失败。与 CLI 一致。
    let summarizer: Arc<dyn SessionSummarizer> = Arc::new(SessionSummarizerImpl::new(
        summary_provider.clone(),
        Some(main_provider.clone()),
    ));

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
        .events(event_bus)
        .event_store(event_store)
        .snapshot_store(snapshot_store);

    // 11b. 注入文件改动 journal（W-11 diff 视图，C-28：/undo 语义复用 FileChangeJournal）
    //      与 CLI `file-undo` feature 对齐：`workspace/diff` 端点展示会话内文件改动历史。
    let journal: Arc<dyn minicoding_core::journal::Journal> = Arc::new(
        minicoding_journal::FileChangeJournal::new(Some(workdir.clone())),
    );
    builder = builder.journal(journal);

    // 11c. 预加载会话（恢复历史会话用，见 `SessionManager::restore_session`）。
    //      `RuntimeBuilder::session` 设置会话 id 与已落盘消息，后续 `storage.append`
    //      写入同一会话文件；上下文回填由调用方 `restore_history` 完成。
    if let Some(s) = preloaded {
        builder = builder.session(s);
    }

    // 12. 注入沙箱驱动（与 CLI 一致，Linux Landlock / 降级 NoopDriver）
    //     策略来自 `--preset`（http.rs `CreateSessionBody.preset` 可会话级覆盖）；
    //     `ExternalSandbox`/`DangerFullAccess` 需用户显式选定（C-22）。
    let driver: Arc<dyn SandboxDriver> = Arc::from(minicoding_sandbox::detect_driver());
    builder = builder
        .sandbox_driver(driver)
        .sandbox_policy(sandbox_policy)
        // M-05：注入领域级 denial 检测与熔断（sandbox 签名库 + C-30 熔断）
        .sandbox_denial_detector(Arc::new(minicoding_sandbox::DenialDetector::new()))
        .sandbox_denial_breaker(Arc::new(
            minicoding_sandbox::SandboxCircuitBreaker::default_thresholds(),
        ));

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
        OPENAI_PROVIDER_ID => Ok(Arc::new(OpenAiProvider::with_name(
            cfg.name.clone(),
            &cfg.api_base,
            &cfg.api_key,
            &cfg.model,
        )?)),
        ANTHROPIC_PROVIDER_ID => Ok(Arc::new(AnthropicProvider::with_name(
            cfg.name.clone(),
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
            Ok(Arc::new(OllamaProvider::with_name(
                cfg.name.clone(),
                api_base,
                &cfg.model,
            )?))
        }
        other => Err(minicoding_core::model::LlmError::Client {
            status: 400,
            body: format!("未知 provider `{other}`：支持 `openai`/`anthropic`/`ollama`"),
        }),
    }
}

/// 根据 `[provider.small]` 配置构造 small provider（gap-4，与 CLI builder 一致）。
///
/// `small_cfg.api_base`/`api_key` 为 `None` 时继承主 `[provider]` 配置：
/// 让用户用便宜模型（如 `gpt-4o-mini`）做摘要/压缩/记忆提取，降本见
/// `design.md` §3.8。small provider 始终用 `OpenAI` 兼容协议。
///
/// # Errors
/// `OpenAiProvider::new` 失败时返回 `LlmError` 描述（reqwest 初始化失败、
/// tiktoken 词表加载失败等）。
fn build_small_provider(
    small_cfg: &SmallProviderConfig,
    main_cfg: &ProviderConfig,
) -> Result<OpenAiProvider, minicoding_core::model::LlmError> {
    let api_base = small_cfg
        .api_base
        .clone()
        .unwrap_or_else(|| main_cfg.api_base.clone());
    let api_key = small_cfg
        .api_key
        .clone()
        .unwrap_or_else(|| main_cfg.api_key.clone());
    OpenAiProvider::new(&api_base, &api_key, &small_cfg.model)
}

/// 从 OS keyring 加载 API key（C-04 fallback）。
///
/// 与 CLI `cred.rs` / `minicoding-desktop::config` 共享 `KEYRING_SERVICE`/`KEYRING_ACCOUNT`，
/// 确保三端（CLI / server / desktop）读写同一 keyring entry。
///
/// 返回 `Ok(None)` 表示 keyring 可用但无 entry；`Err` 表示 keyring 不可用。
fn load_api_key_from_keyring() -> Result<Option<String>, anyhow::Error> {
    const KEYRING_SERVICE: &str = "minicoding";
    const KEYRING_ACCOUNT: &str = "openai_api_key";
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)?;
    match entry.get_password() {
        Ok(key) => Ok(Some(key)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("keyring get 失败: {e}")),
    }
}
