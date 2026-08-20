//! Runtime 组装：根据 CLI 参数与环境变量构造 `Runtime`。
//!
//! 组装顺序：config → provider → context → storage → tools → policy/prompter/audit
//! → `RuntimeBuilder`。

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use minicoding_context::{ContextManagerImpl, SimpleContextManager};
use minicoding_core::config::{ProviderConfig, RuntimeConfig, SmallProviderConfig, config_hash};
use minicoding_core::memory::{MemoryStore, SessionSummarizer};
use minicoding_core::model::{MemoryError, Message, Session, SessionId, ToolError};
use minicoding_core::policy::{PermissionMode, PermissionPolicy, PermissionPrompter};
use minicoding_core::provider::{BoxFuture, LlmProvider};
use minicoding_core::runtime::{Runtime, RuntimeBuilder};
use minicoding_core::sandbox::{SandboxDriver, SandboxPolicy};
use minicoding_core::storage::AuditSink;
use minicoding_core::tool::ToolRegistry;
use minicoding_memory::{
    AutoCategory, AutoMemory, LongTermMemory, ProjectDocLoaderImpl, SessionSummarizerImpl,
    inject_project_doc_sync,
};
use minicoding_policy::{BuiltinPolicy, InteractivePrompter, NonInteractivePrompter, ReplayPolicy};
use minicoding_providers::{
    ANTHROPIC_PROVIDER_ID, AnthropicProvider, OLLAMA_DEFAULT_API_BASE, OLLAMA_PROVIDER_ID,
    OllamaProvider, OpenAiProvider, PROVIDER_ID as OPENAI_PROVIDER_ID, RetryConfig, RetryProvider,
    TiktokenTokenizer,
};
use minicoding_storage::{FileAuditSink, JsonlStorage};
use minicoding_tools::{
    AutoMemoryWriter, MemoryCategory, MemoryWrite, register_readonly_tools, register_shell_tools,
    register_task_tools, register_write_tools,
};
use std::io::IsTerminal;
use std::sync::Arc;
use time::OffsetDateTime;

/// 会话加载模式（`--resume`/`--replay`/`--fork-session` 互斥，T-M3-10）。
///
/// - `None`：新建空会话；
/// - `Resume`：加载历史会话继续提问（原 id，原存储文件追加写）；
/// - `Replay`：回放历史会话，默认禁副作用（C-06），`allow_side_effects=true` 时
///   走正常权限流程；
/// - `Fork`：从原会话分叉到新会话（新 id，复制前缀消息到新文件，原文件不变）。
#[derive(Debug, Clone)]
pub enum SessionLoadMode {
    /// 新建空会话（默认）。
    None,
    /// `--resume <id>`：恢复会话继续提问。
    Resume(String),
    /// `--replay <id>`：回放历史，默认禁副作用（C-06）。
    Replay {
        id: String,
        allow_side_effects: bool,
    },
    /// `--fork-session <id>`：从原会话分叉到新会话。
    Fork(String),
}

/// `AutoMemory` → `AutoMemoryWriter` 适配器。
///
/// 桥接 `minicoding-tools` 的 `AutoMemoryWriter` trait 与 `minicoding-memory`
/// 的 `AutoMemory` 具体实现（`MemoryCategory` → `AutoCategory` 转换）。
struct AutoMemoryAdapter {
    inner: Arc<AutoMemory>,
}

impl AutoMemoryWriter for AutoMemoryAdapter {
    fn add_entry(
        &self,
        topic: String,
        content: String,
        category: MemoryCategory,
        confidence: f64,
    ) -> BoxFuture<'_, Result<usize, ToolError>> {
        Box::pin(async move {
            let cat = match category {
                MemoryCategory::Correction => AutoCategory::Correction,
                MemoryCategory::Pitfall => AutoCategory::Pitfall,
                MemoryCategory::Pref => AutoCategory::Pref,
                MemoryCategory::Decision => AutoCategory::Decision,
            };
            self.inner
                .add_entry(topic, content, cat, confidence)
                .await
                .map_err(|e: MemoryError| ToolError::Exec(format!("auto memory: {e}")))
        })
    }
}

/// 从 CLI 参数构建 `Runtime`。
///
/// `provider_override` 覆盖 `config.provider.default`（`--provider`，T-M6-5），
/// 决定调用哪家 provider 的 API 协议（`openai`/`anthropic`/`ollama`）。
/// `provider_name_override` 覆盖 `config.provider.name`（`--provider-name`），
/// 设置自定义显示名用于日志/metrics，不影响协议分派。
/// `mode` 控制会话加载方式（`--resume`/`--replay`/`--fork-session`，T-M3-10）。
/// `sandbox_override` 为 `Some` 时覆盖默认沙箱策略（`exec --sandbox` 用），
/// 为 `None` 时用默认 `WorkspaceWrite { workdir, [] }`。
/// `start_in_plan_mode = true` 时初始 `PermissionMode::Plan`（`--plan`，T-M5-8）。
/// 预加载会话时，调用方需在 `build_runtime` 后调用 `Runtime::restore_history`
/// 将消息注入上下文管理器。
///
/// # Errors
/// API key 缺失、provider 构造失败、存储目录不可用、会话不存在或加载失败时返回错误。
#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_lines)] // 组装流程线性展开，拆分反而降低可读性
#[allow(clippy::too_many_arguments)] // CLI 组装入口，参数由调用方 CLI flag 决定，聚合为 struct 反而增删不便
pub fn build_runtime(
    provider_override: Option<&str>,
    provider_name_override: Option<&str>,
    api_base: Option<&str>,
    api_key: Option<&str>,
    model: Option<&str>,
    workdir: &str,
    system: Option<&str>,
    mode: &SessionLoadMode,
    sandbox_override: Option<SandboxPolicy>,
    start_in_plan_mode: bool,
    prompter_override: Option<Arc<dyn PermissionPrompter>>,
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
    // 自定义 provider 显示名（env: MINICODING_PROVIDER_NAME）
    if let Ok(name) = std::env::var("MINICODING_PROVIDER_NAME") {
        config.provider.name = Some(name);
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
    // `--provider` 覆盖 `config.provider.default`（T-M6-5）
    if let Some(p) = provider_override {
        config.provider.default = p.to_string();
    }
    // `--provider-name` 覆盖 `config.provider.name`（自定义显示名）
    if let Some(n) = provider_name_override {
        config.provider.name = Some(n.to_string());
    }

    // 2. 校验 API key：CLI/env 未提供时尝试 OS keyring / 文件 fallback（T-M4-11，C-04）
    //    Ollama 本地模型无需 API key（本地 `/api/chat` 不鉴权），跳过校验。
    let provider_kind = config.provider.default.as_str();
    let is_ollama = provider_kind == OLLAMA_PROVIDER_ID;
    if !is_ollama
        && config.provider.api_key.is_empty()
        && let Some(key) = crate::cred::load_api_key().context("加载凭证失败")?
    {
        config.provider.api_key = key;
    }
    if !is_ollama && config.provider.api_key.is_empty() {
        anyhow::bail!(
            "API key 未配置：请设置 OPENAI_API_KEY 环境变量、使用 --api-key 参数，或通过 `minicoding cred store` 写入 keyring"
        );
    }

    // 3. 构造 provider（按 `config.provider.default` 分派，T-M6-5）
    //    三家 provider 统一包装 `RetryProvider` 装饰器（C-13：bounded retries，
    //    `max_retries` 上限防止死循环）。Arc 共享：主推理 + L2 摘要压缩复用同一实例。
    let main_provider: Arc<dyn LlmProvider> =
        build_main_provider(provider_kind, &config.provider).context("provider 构造失败")?;
    // 用 RetryProvider 包装（C-13：bounded retries，指数退避 + 429 Retry-After）
    let retry_config = RetryConfig {
        max_retries: config.provider.max_retries,
        initial_backoff_ms: 500,
        max_backoff_ms: 30_000,
        request_timeout: std::time::Duration::from_secs(config.provider.timeout_sec),
    };
    let main_provider: Arc<dyn LlmProvider> =
        Arc::new(RetryProvider::new(main_provider, retry_config));

    // 主 provider 的 `Arc<dyn LlmProvider>` 视图：供 SessionSummarizer 作为
    // secondary（降级兜底用），以及在 small 未配置时作 primary。
    let main_provider_view: Arc<dyn LlmProvider> = main_provider.clone();

    // 3b. 构造 small provider（gap-4：[provider.small] 独立小 LLM 配置）
    //     未配置时退化为 `None`，由后续逻辑回退到主 provider。
    //     配置 `api_base`/`api_key` 为 `None` 时继承主 provider（典型：同一 OpenAI
    //     账号但换便宜模型做摘要/压缩，降本见 `design.md` §3.8）。
    //     small provider 始终用 OpenAI 兼容协议（摘要/压缩通常用便宜 OpenAI 模型）；
    //     如需用其他 provider 做摘要，后续可在 `[provider.small]` 加 `kind` 字段。
    let small_provider: Option<Arc<dyn LlmProvider>> = match &config.provider.small {
        Some(small_cfg) => match build_small_provider(small_cfg, &config.provider) {
            Ok(p) => {
                let p: Arc<dyn LlmProvider> = Arc::new(p);
                // small provider 也包装 RetryProvider（与主 provider 一致的错误恢复策略）
                Some(Arc::new(RetryProvider::new(
                    p,
                    RetryConfig {
                        max_retries: config.provider.max_retries,
                        initial_backoff_ms: 500,
                        max_backoff_ms: 30_000,
                        request_timeout: std::time::Duration::from_secs(
                            config.provider.timeout_sec,
                        ),
                    },
                )))
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
        .map_or_else(|| main_provider_view.clone(), |p| p);

    // 4. 解析 workdir（提前到 context manager 之前，供 ProjectDocLoader 使用）
    //    canonicalize 失败时（目录不存在或相对路径无法规范化）回退到
    //    `current_dir().join(workdir)`——不能保留原始相对值：后续权限层
    //    `resolve_under` 依赖绝对路径做越界判定，相对路径会与进程 cwd 隐式
    //    绑定，导致"工作目录内的写入被判越界"（见 CLI 单次验证发现）。
    let workdir_path = match Utf8PathBuf::from(workdir).canonicalize_utf8() {
        Ok(p) => p,
        Err(_) => std::env::current_dir()
            .ok()
            .and_then(|cwd| {
                // 目录可能尚不存在（如 `--workdir new-dir` 首次运行）：退化为绝对路径拼接
                let joined = cwd.join(workdir);
                if joined.is_absolute() {
                    Utf8PathBuf::from_path_buf(joined).ok()
                } else {
                    None
                }
            })
            .unwrap_or_else(|| Utf8PathBuf::from(workdir)),
    };

    // 4b. 加载项目文档（AGENTS.md 分层加载，T-M3-7）
    //     从 repo_root 到 cwd 逐级加载，注入 system 段包裹 <project_doc> 边界（C-05）。
    //     加载失败不阻塞启动（best effort，记 warn 日志）。
    //
    //     `extensions` feature 启用时走 PromptPipeline 路径，project_doc 由
    //     `ProjectRulesContributor` 注入（包裹 `<project_doc>` 边界），无需在此
    //     预注入到 system_prompt。`extensions` 未启用时走旧路径（静态注入）。
    let base_system = system.map_or_else(
        || "You are minicoding, a terminal AI coding assistant.".to_string(),
        std::string::ToString::to_string,
    );
    let system_prompt = load_and_inject_project_doc(&workdir_path, &base_system);

    // 4c. (`extensions` feature) 构造 PromptPipeline + PromptContext 模板。
    //     9 个内置 contributor 由 `minicoding-extension-sdk::builtin_contributors` 提供，
    //     CLI 注入 user_rules（long_term.md）+ project_rules（AGENTS.md）+ git_info。
    //     `enabled_tools` 字段在 `build_chat_request` 时由 `tools.schemas()` 动态填充。
    #[cfg(feature = "extensions")]
    let prompt_pipeline: Option<(
        Arc<minicoding_core::prompt::PromptPipeline>,
        minicoding_core::prompt::PromptContext,
    )> = {
        let user_rules = load_long_term_content_sync();
        let project_rules = load_project_doc_sync(&workdir_path);
        let identity = load_identity_content_sync();
        let ctx_template =
            minicoding_core::prompt::PromptContext::new(SessionId::new(), workdir_path.clone())
                .with_user_rules(minicoding_core::prompt::MemoryBlock {
                    content: user_rules,
                    source_path: Some(
                        minicoding_core::paths::memory_dir()
                            .map(|d| d.join("long_term.md").to_string())
                            .unwrap_or_default(),
                    ),
                })
                .with_project_rules(minicoding_core::prompt::ProjectDoc {
                    content: project_rules.content,
                    layers: project_rules.layers,
                });
        let contributors = minicoding_extension_sdk::builtin_contributors(&identity);
        let pipeline = Arc::new(minicoding_core::prompt::PromptPipeline::with_contributors(
            contributors,
        ));
        Some((pipeline, ctx_template))
    };

    // 4b. 构造审计 sink（AGENTS.md §5.5：权限决策必须落 audit.log，0600 权限）。
    //     M-07（R-02）：压缩审计（AuditKind::Compress）同走此 sink，须在 ctx 构造前就绪。
    let audit_path = minicoding_core::paths::audit_log_path().context("无法确定审计日志路径")?;
    let audit: Arc<dyn AuditSink> = Arc::new(FileAuditSink::new(audit_path));

    // 5. 构造 context manager（T-M3-1/2/3：ContextManagerImpl + 4 级压缩 + 熔断）
    //    注入 TiktokenTokenizer 做精确 token 计数；L2 摘要 provider 用 small（如有）
    //    降本，回退到主 provider。分词器构造失败时降级为 SimpleContextManager（无压缩）。
    //
    //    `extensions` feature 启用时注入 PromptPipeline（动态 system prompt，9 段拼接）；
    //    未启用时走静态 `system_prompt`（已含 project_doc 注入）。
    let ctx: Arc<dyn minicoding_core::context::ContextManager> =
        match TiktokenTokenizer::new_for_model(&config.provider.model) {
            Ok(tokenizer) => {
                // 128K 上下文窗口（gpt-4o 系列）；TODO: 按 model 精确查询 context window
                let context_window = 128_000;
                let mut mgr = ContextManagerImpl::new(
                    system_prompt.clone(),
                    Arc::new(tokenizer),
                    context_window,
                    Some(summary_provider.clone()),
                );
                // M-07（R-02）：注入压缩审计 sink
                mgr.set_audit(audit.clone());
                #[cfg(feature = "extensions")]
                let mgr = {
                    if let Some((pipeline, ctx_template)) = prompt_pipeline {
                        mgr.with_prompt_pipeline(pipeline, ctx_template)
                    } else {
                        mgr
                    }
                };
                Arc::new(mgr)
            }
            Err(e) => {
                tracing::warn!(
                    "Tiktoken 分词器构造失败（{e}），降级为 SimpleContextManager（无压缩）"
                );
                Arc::new(SimpleContextManager::new(system_prompt.clone()))
            }
        };

    // 6. 构造 storage
    let sessions_dir = minicoding_core::paths::sessions_dir().context("无法确定会话存储目录")?;
    let storage = JsonlStorage::new(sessions_dir.clone());

    // 6a. 构造 EventStore + SnapshotStore（Event Sourcing，见 `design.md` §25）
    //     与 `JsonlStorage` 共用 `sessions_dir`：`{id}.events.jsonl` + `{id}.snapshot.json`
    //     与消息日志 `{id}.jsonl` 同目录，便于备份/清理。
    //     新会话双写（messages + events）平滑过渡；旧会话无事件流时回退到消息日志。
    //     保留具体类型实例供 `load_session_by_mode` 同步加载（trait 未暴露 sync 方法），
    //     稍后在 `RuntimeBuilder` 注入前 wrap 为 `Arc<dyn trait>`。
    let event_store: minicoding_storage::JsonlEventStore =
        minicoding_storage::JsonlEventStore::new(sessions_dir.clone());
    let snapshot_store: minicoding_storage::JsonlSnapshotStore =
        minicoding_storage::JsonlSnapshotStore::new(sessions_dir.clone());

    // 6. 构造 tool registry
    //    T-M7-4：EventBus 提前创建，clone 给 task store 用于广播 `TaskUpdated`，
    //    原件传入 RuntimeBuilder（EventBus 内部是 broadcast::Sender，clone 共享通道）。
    let event_bus = minicoding_core::runtime::EventBus::new();
    let mut tools = ToolRegistry::new();
    register_readonly_tools(&mut tools);
    register_write_tools(&mut tools);
    register_shell_tools(&mut tools);
    // T-M8-5：git 工具组（git.diff 只读 / git.apply 写入）
    minicoding_tools::register_git_tools(&mut tools);
    // T-M8-5：web 工具组（web.fetch/search，需 `web` feature）
    #[cfg(feature = "web")]
    minicoding_tools::register_web_tools(&mut tools);
    // T-M3-8：任务管理工具（SideEffect::None，单 in_progress 约束 + 依赖图成环检测）
    // T-M7-4：注入 EventBus clone，task.create/update 成功后广播 TaskUpdated
    register_task_tools(&mut tools, Some(event_bus.clone()));

    // 6b. 注册 memory.write 工具（T-M3-9：long_term + auto memory）
    //     long_term 走 MemoryStore trait（C-23：经 Ask 权限）；
    //     auto 走 AutoMemoryWriter trait（C-27：默认 Allow，指令性内容降级 Ask）。
    //     LongTermMemory::default / AutoMemory::default 在 home 不可解析时退化为相对路径。
    let long_term_store: Arc<dyn MemoryStore> = Arc::new(LongTermMemory::default());
    let auto_store: Arc<dyn AutoMemoryWriter> = Arc::new(AutoMemoryAdapter {
        inner: Arc::new(AutoMemory::default()),
    });
    tools.register(Arc::new(MemoryWrite::new(long_term_store, auto_store)));

    // 7. 构造权限策略 + 交互器（C-01：副作用必须经权限）
    //    TTY → InteractivePrompter（stdin 读 y/n）；非 TTY → NonInteractivePrompter
    //    （恒 Deny，CI 安全默认，见 design.md §9.2）
    //    `--replay` 无 `--allow-side-effects` → ReplayPolicy 包装 BuiltinPolicy，
    //    强制 Deny 所有副作用工具（C-06）。
    let builtin: Arc<dyn PermissionPolicy> = Arc::new(BuiltinPolicy::new());
    let policy: Arc<dyn PermissionPolicy> = match mode {
        SessionLoadMode::Replay {
            allow_side_effects: false,
            ..
        } => {
            tracing::warn!(
                "--replay 模式：副作用工具已禁用（C-06），使用 --allow-side-effects 显式启用"
            );
            Arc::new(ReplayPolicy::new(builtin))
        }
        _ => builtin,
    };
    let prompter: Arc<dyn PermissionPrompter> = match prompter_override {
        Some(p) => p,
        None if std::io::stdin().is_terminal() => Arc::new(InteractivePrompter::new()),
        None => {
            tracing::warn!("stdin 非 TTY，切换为 NonInteractivePrompter（副作用工具将被拒绝）");
            Arc::new(NonInteractivePrompter::new())
        }
    };

    // 8. 按 mode 加载会话（T-M3-10a/b：resume/replay/fork）
    //     - Resume/Replay：原 id，原存储文件追加写；
    //     - Fork：新 id，复制前缀消息到新文件（原文件不变）。
    //     消息不在此处注入上下文，由调用方 `restore_history` 完成回填。
    //     Event Sourcing（§25.4）：Resume/Replay 优先走 snapshot + 事件流重放，
    //     旧会话无事件流时回退到消息日志（`session_from_messages`）。
    let config_hash_val = config_hash(&config);
    let session = load_session_by_mode(
        &storage,
        &event_store,
        &snapshot_store,
        mode,
        workdir_path.clone(),
        config_hash_val,
    )?;

    // 10. 构造 SessionSummarizer（gap-5/T-M3-6：会话结束时生成摘要落盘 index.json）
    //     primary = small provider（如有，便宜模型降本），secondary = 主 provider
    //     （降级兜底）。降级链终端为启发式兜底（C-29），永不失败。
    //     small_provider 已 clone 给 summary_provider；此处再用 main_provider_view
    //     作为 secondary。
    let summarizer_primary: Arc<dyn LlmProvider> = summary_provider.clone();
    let summarizer: Arc<dyn SessionSummarizer> = Arc::new(SessionSummarizerImpl::new(
        summarizer_primary,
        Some(main_provider_view),
    ));

    // 11. 组装 Runtime（provider/ctx 已是 Arc，直接传入）
    //     `config` 在此处 move 进 builder，所有需读 `config` 字段的逻辑（如 hooks）
    //     必须在 move 之前完成（见下方 hooks registry 提前构建）。
    let initial_mode = if start_in_plan_mode {
        PermissionMode::Plan
    } else {
        PermissionMode::Default
    };

    // 11a. 预构建 Hook registry（`hooks` feature 启用时，T-M5-8）
    //      必须在 `config` move 进 builder 之前读 `config.hooks`。
    //      未启用 hooks feature 时不构建（RuntimeBuilder 默认 NoopHookRegistry）。
    #[cfg(feature = "hooks")]
    let hook_registry = build_hook_registry(&config.hooks);

    // 11a-2. 启动配置文件监听（S-22 热更新，best-effort：监听失败不阻塞启动）
    //        `ConfigWatcher::start` 内部捕获错误并降级为空壳，故结果直接注入。
    //        复用上方 `event_bus` 的 clone（原件在下方 move 进 builder）。
    let config_watcher = minicoding_core::config::ConfigWatcher::start(
        &minicoding_core::paths::config_path()
            .unwrap_or_else(|_| camino::Utf8PathBuf::from("config.toml")),
        event_bus.clone(),
    );

    let mut builder = RuntimeBuilder::new()
        .provider(main_provider)
        .context(ctx)
        .storage(Arc::new(storage))
        .tools(tools)
        .config(config)
        .workdir(workdir_path.clone())
        .policy(policy)
        .prompter(prompter)
        .audit(audit)
        .session_summarizer(summarizer)
        .permission_mode(initial_mode)
        .events(event_bus)
        .with_config_watcher(config_watcher)
        .event_store(Arc::new(event_store))
        .snapshot_store(Arc::new(snapshot_store));

    // 11b. 注入沙箱驱动 + 策略（T-M4-9，C-22：沙箱为第二道防线）
    //      `sandbox_override` 来自 `exec --sandbox`；默认 `WorkspaceWrite { workdir, [] }`。
    //      沙箱驱动由 `detect_driver()` 探测（Linux Landlock / 降级 NoopDriver）。
    let sandbox_policy = sandbox_override.unwrap_or_else(|| SandboxPolicy::WorkspaceWrite {
        workdir: workdir_path.clone(),
        writable: Vec::new(),
    });
    #[cfg(feature = "sandbox")]
    {
        let driver: Arc<dyn SandboxDriver> = Arc::from(minicoding_sandbox::detect_driver());
        builder = builder
            .sandbox_driver(driver)
            .sandbox_policy(sandbox_policy)
            // M-05：注入领域级 denial 检测与熔断（sandbox 签名库 + C-30 熔断）
            .sandbox_denial_detector(Arc::new(minicoding_sandbox::DenialDetector::new()))
            .sandbox_denial_breaker(Arc::new(
                minicoding_sandbox::SandboxCircuitBreaker::default_thresholds(),
            ));
    }
    #[cfg(not(feature = "sandbox"))]
    {
        // sandbox feature 未启用时不注入（RuntimeBuilder 默认 NoopDriver + WorkspaceWrite）。
        let _ = sandbox_policy;
    }

    // 11c. 注入 journal（`file-undo` feature 启用时，C-28：/undo 可用）
    #[cfg(feature = "file-undo")]
    {
        let journal: Arc<dyn minicoding_core::journal::Journal> = Arc::new(
            minicoding_journal::FileChangeJournal::new(Some(workdir_path.clone())),
        );
        builder = builder.journal(journal);
    }

    // 11d. 注入 Hook 注册表（`hooks` feature 启用时，T-M5-8）
    //      `hook_registry` 在 `config` move 之前预构建（见 11a）。
    #[cfg(feature = "hooks")]
    {
        use minicoding_core::hooks::HookRegistry;
        if hook_registry.count() > 0 {
            tracing::info!(
                hook_count = hook_registry.count(),
                "hooks 已加载并注入 Runtime"
            );
        }
        builder = builder.hook_registry(Arc::new(hook_registry));
    }

    // 11e. 注入 ExtensionHost（`extensions` feature 启用时，T-M8-2）
    //      `BundledExtensionHost` 管理进程内 first-party 扩展生命周期。
    //      当前 M8 阶段仅注入空 host（无扩展加载），后续可从
    //      `~/.minicoding/extensions/` 加载扩展并 `load_with_extension`。
    //      `shutdown_all` 由 CLI 在会话退出前通过持有的 `Arc<BundledExtensionHost>` 调用。
    #[cfg(feature = "extensions")]
    {
        let host: Arc<minicoding_extension_sdk::BundledExtensionHost> =
            Arc::new(minicoding_extension_sdk::BundledExtensionHost::new());
        builder = builder.extension_host(host);
    }

    if let Some(s) = session {
        builder = builder.session(s);
    }
    let mut rt = builder.build().map_err(anyhow::Error::msg)?;

    // 12. 补注册依赖 Runtime 自身引用的工具（T-M5-8）
    //     `plan.exit` 与 `task.spawn` 需 `plan_controller()`/`subagent_runner()`，
    //     只能在 Runtime 构造后注册（chicken-and-egg：tools ↔ Runtime 互依）。
    //     直接构造工具实例并调 `register_dynamic_tool`，绕过 `register_plan_tools`/
    //     `register_spawn_tool` 的 `&mut ToolRegistry` 签名约束。
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

/// 从 `HooksConfig` 构造 `HookRegistryImpl`（T-M5-8）。
///
/// 把每个 `HookEntry` 转为 `ScriptHook`（外部脚本 Hook）并注册。`matcher` 字段
/// 解析为 `HookMatcher::for_tools`（工具相关事件）或 `HookMatcher::for_events`
/// （非工具事件）。`timeout_sec` 缺省时用 `default_timeout_sec`。
#[cfg(feature = "hooks")]
fn build_hook_registry(
    cfg: &minicoding_core::config::HooksConfig,
) -> minicoding_hooks::HookRegistryImpl {
    use minicoding_core::hooks::{HookEvent, HookMatcher, HookRegistry};
    use minicoding_hooks::ScriptHook;
    use std::sync::Arc;
    use std::time::Duration;

    let registry = minicoding_hooks::HookRegistryImpl::new();
    let default_timeout = Duration::from_secs(cfg.default_timeout_sec);

    // 事件 → 对应配置段的映射。非工具事件忽略 `matcher`；工具事件解析 `matcher`。
    // 元组：(events, entries, is_tool_event)
    let groups: [(&[HookEvent], &[minicoding_core::config::HookEntry], bool); 10] = [
        (&[HookEvent::SessionStart], &cfg.session_start, false),
        (
            &[HookEvent::UserPromptSubmit],
            &cfg.user_prompt_submit,
            false,
        ),
        (&[HookEvent::PreToolUse], &cfg.pre_tool_use, true),
        (&[HookEvent::PostToolUse], &cfg.post_tool_use, true),
        (
            &[HookEvent::PostToolUseFailure],
            &cfg.post_tool_use_failure,
            true,
        ),
        (&[HookEvent::PreCompact], &cfg.pre_compact, false),
        (&[HookEvent::PostCompact], &cfg.post_compact, false),
        (&[HookEvent::Stop], &cfg.stop, false),
        (&[HookEvent::SubagentStop], &cfg.subagent_stop, false),
        (
            &[HookEvent::PermissionRequest],
            &cfg.permission_request,
            true,
        ),
    ];

    for (events, entries, is_tool_event) in groups {
        for (idx, entry) in entries.iter().enumerate() {
            let matcher = if is_tool_event {
                match &entry.matcher {
                    Some(patterns) => HookMatcher::for_tools(
                        events.to_vec(),
                        patterns
                            .split('|')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                            .collect(),
                    ),
                    None => HookMatcher::for_events(events.to_vec()),
                }
            } else {
                HookMatcher::for_events(events.to_vec())
            };
            let timeout = entry
                .timeout_sec
                .map_or(default_timeout, Duration::from_secs);
            let name = format!("{}[{idx}]", events[0].as_str());
            let hook = ScriptHook::new(name, matcher, entry.command.clone(), timeout);
            registry.register(Arc::new(hook));
        }
    }

    registry
}

/// 按 `SessionLoadMode` 加载会话（T-M3-10a/b）。
///
/// - `None` → `None`（新建空会话）；
/// - `Resume(id)` / `Replay{id}` → 原会话（原 id，不复制）；
/// - `Fork(id)` → 新会话（新 id，复制原消息前缀到新文件，原文件不变）。
///
/// ## Event Sourcing 集成（§25.4）
///
/// `Resume`/`Replay` 模式优先走事件重放路径：
/// 1. 加载 `JsonlSnapshotStore::load_sync`（如有 snapshot）；
/// 2. 加载 `JsonlEventStore::load_events_sync` 全部事件；
/// 3. 调 `replay_session_state` 重建 `Session`；
/// 4. 旧会话无事件流（`load_events_sync` 返回空且无 snapshot）→ 回退到
///    `load_messages` 消息日志路径（`session_from_messages` 构造）。
///
/// `Fork` 模式不走事件重放（fork 创建新会话，新会话有自己的事件流），
/// 仅从消息日志复制前缀消息。
///
/// # Errors
/// 会话不存在、加载失败或 fork 复制失败时返回错误。
fn load_session_by_mode(
    storage: &JsonlStorage,
    event_store: &minicoding_storage::JsonlEventStore,
    snapshot_store: &minicoding_storage::JsonlSnapshotStore,
    mode: &SessionLoadMode,
    workdir: Utf8PathBuf,
    config_hash_val: u64,
) -> Result<Option<Session>> {
    let source_id = match mode {
        SessionLoadMode::None => return Ok(None),
        SessionLoadMode::Resume(id)
        | SessionLoadMode::Replay { id, .. }
        | SessionLoadMode::Fork(id) => id,
    };

    match mode {
        SessionLoadMode::None => Ok(None),
        SessionLoadMode::Resume(_) | SessionLoadMode::Replay { .. } => {
            // Event Sourcing 路径：尝试 snapshot + 事件流重放
            let session = load_session_via_event_sourcing(
                event_store,
                snapshot_store,
                source_id,
                workdir.clone(),
                config_hash_val,
            )?;
            if let Some(s) = session {
                tracing::info!(
                    session = %source_id,
                    messages = s.messages.len(),
                    "session loaded via event sourcing replay"
                );
                return Ok(Some(s));
            }
            // 回退：消息日志路径（旧会话无事件流）
            tracing::info!(
                session = %source_id,
                "no event stream found, falling back to message log"
            );
            let messages = load_messages(storage, source_id)?;
            let created_at = messages
                .first()
                .map_or_else(OffsetDateTime::now_utc, |m| m.created_at);
            Ok(Some(Session {
                id: source_id.clone(),
                created_at,
                workdir,
                config_hash: config_hash_val,
                messages,
            }))
        }
        SessionLoadMode::Fork(_) => {
            // Fork：新 id，复制前缀消息到新文件（原文件不变，design.md §10.5）
            // 不走事件重放（fork 创建新会话，新会话有自己的事件流）
            let messages = load_messages(storage, source_id)?;
            let created_at = messages
                .first()
                .map_or_else(OffsetDateTime::now_utc, |m| m.created_at);
            let new_id = ulid::Ulid::new().to_string();
            storage
                .fork_session_sync(&new_id, &messages)
                .context("fork 会话复制失败")?;
            tracing::info!(
                from = source_id,
                to = %new_id,
                count = messages.len(),
                "session forked"
            );
            Ok(Some(Session {
                id: new_id,
                created_at,
                workdir,
                config_hash: config_hash_val,
                messages,
            }))
        }
    }
}

/// 通过 Event Sourcing 路径加载会话（snapshot + 事件流重放，见 `design.md` §25.4）。
///
/// 返回 `Ok(None)` 表示无事件流（旧会话），调用方应回退到消息日志路径。
///
/// # Errors
/// 事件流存在但重放失败（schema 不兼容、seq 缺口等）时返回错误。
fn load_session_via_event_sourcing(
    event_store: &minicoding_storage::JsonlEventStore,
    snapshot_store: &minicoding_storage::JsonlSnapshotStore,
    session_id: &str,
    workdir: Utf8PathBuf,
    config_hash_val: u64,
) -> Result<Option<Session>> {
    use minicoding_storage::{replay_session_state, session_from_messages};

    // 加载 snapshot（如有）
    let snapshot = snapshot_store
        .load_sync(&session_id.to_string())
        .context("加载 snapshot 失败")?;

    // 加载全部事件
    let events = event_store
        .load_events_sync(&session_id.to_string())
        .context("加载事件流失败")?;

    // 旧会话：无事件流且无 snapshot → 回退
    if events.is_empty() && snapshot.is_none() {
        return Ok(None);
    }

    // 事件重放重建 Session
    let replayed = replay_session_state(snapshot.as_ref(), events)
        .map_err(|e| anyhow::anyhow!("事件重放失败: {e}"))?;

    // 用重放结果构造 Session（workdir/config_hash 由 CLI 启动参数提供，
    // 覆盖事件流中的值——避免 workdir 跨机器漂移）
    let session = if replayed.session.messages.is_empty() {
        // 边界：事件流存在但无 MessageAppended（仅 SessionCreated），
        // 用 session_from_messages 构造（created_at 取 SessionCreated 事件时间）
        session_from_messages(
            session_id.to_string(),
            workdir,
            config_hash_val,
            replayed.session.messages,
        )
    } else {
        Session {
            id: replayed.session.id,
            created_at: replayed.session.created_at,
            workdir,
            config_hash: config_hash_val,
            messages: replayed.session.messages,
        }
    };

    Ok(Some(session))
}

/// 从存储同步加载会话消息（启动期 tokio runtime 未创建，用 sync 方法）。
fn load_messages(storage: &JsonlStorage, session_id: &str) -> Result<Vec<Message>> {
    let id: SessionId = session_id.to_string();
    let messages = storage
        .load_messages_sync(&id)
        .with_context(|| format!("读取会话 {session_id} 消息失败"))?;
    if messages.is_empty() {
        anyhow::bail!("会话 {session_id} 不存在或无消息");
    }
    Ok(messages)
}

/// 加载项目文档（AGENTS.md 分层加载）并注入 system prompt（T-M3-7）。
///
/// 从 `workdir` 向上探测 `repo_root`（`.git`/`.hg`/`.svn`），再从 `repo_root`
/// 到 `workdir` 逐级加载 `AGENTS.md`/`CLAUDE.md`/`.cursorrules`，拼接后包裹
/// `<project_doc>` 边界注入 system 段末尾（C-05：项目记忆是数据非指令）。
///
/// 加载失败不阻塞启动（best effort）：记 `warn` 日志，返回原 system prompt。
fn load_and_inject_project_doc(workdir: &Utf8PathBuf, base_system: &str) -> String {
    let repo_root = minicoding_memory::find_repo_root(workdir).unwrap_or_else(|| workdir.clone());
    let loader = ProjectDocLoaderImpl::new(repo_root, workdir.clone());
    match loader.load_sync() {
        Ok(doc) => match inject_project_doc_sync(base_system, &doc) {
            Ok(injected) => injected,
            Err(e) => {
                tracing::warn!("注入项目文档失败: {e}");
                base_system.to_string()
            }
        },
        Err(e) => {
            tracing::warn!("加载项目文档失败: {e}");
            base_system.to_string()
        }
    }
}

/// 同步加载 `~/.minicoding/memory/long_term.md` 内容（`extensions` feature 用）。
///
/// 供 `PromptContext.user_rules` 注入。文件不存在或读取失败时返回空串
/// （`UserRulesContributor` 处理空内容为空 section，不阻塞启动）。
///
/// 与 `LongTermMemory::load` 的区别：`load` 是 `async`（`tokio::fs`），此处用
/// `std::fs` 同步读取（CLI 启动阶段尚未进入 async 上下文，且同步 IO 简单可控）。
#[cfg(feature = "extensions")]
fn load_long_term_content_sync() -> String {
    let Ok(dir) = minicoding_core::paths::memory_dir() else {
        return String::new();
    };
    let path = dir.join("long_term.md");
    match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            tracing::warn!(path = %path, error = %e, "加载 long_term.md 失败");
            String::new()
        }
    }
}

/// 同步加载 `~/.minicoding/IDENTITY.md` 内容（P-31，`extensions` feature 用）。
///
/// 供 `IdentityContributor` 注入。文件不存在或读取失败时返回空串
/// （`IdentityContributor` 处理空内容为默认身份，不阻塞启动）。
#[cfg(feature = "extensions")]
fn load_identity_content_sync() -> String {
    let Ok(home) = minicoding_core::paths::minicoding_home() else {
        return String::new();
    };
    let path = home.join("IDENTITY.md");
    match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            tracing::warn!(path = %path, error = %e, "加载 IDENTITY.md 失败");
            String::new()
        }
    }
}

/// 同步加载项目文档（AGENTS.md 分层）并返回 `ProjectDoc`（`extensions` feature 用）。
///
/// 供 `PromptContext.project_rules` 注入。与 `load_and_inject_project_doc` 共用
/// `ProjectDocLoaderImpl::load_sync`，但不做 `inject_project_doc_sync` 包裹——
/// `ProjectRulesContributor` 自行包裹 `<project_doc>` 边界。
///
/// 加载失败时返回空 `ProjectDoc`（contributor 处理空内容为空 section）。
#[cfg(feature = "extensions")]
fn load_project_doc_sync(workdir: &Utf8PathBuf) -> minicoding_core::prompt::ProjectDoc {
    let repo_root = minicoding_memory::find_repo_root(workdir).unwrap_or_else(|| workdir.clone());
    let loader = ProjectDocLoaderImpl::new(repo_root, workdir.clone());
    match loader.load_sync() {
        Ok(content) => {
            // 解析 layers：从 `# source: <path>` 标注提取（loader 输出格式）
            let layers = content
                .lines()
                .filter_map(|line| line.strip_prefix("# source: ").map(str::to_string))
                .collect();
            minicoding_core::prompt::ProjectDoc { content, layers }
        }
        Err(e) => {
            tracing::warn!(error = %e, "加载项目文档失败（extensions 路径）");
            minicoding_core::prompt::ProjectDoc::default()
        }
    }
}

/// 根据 `config.provider.default` 分派构造主 provider（T-M6-5）。
///
/// 支持 `openai`/`anthropic`/`ollama` 三家 provider：
/// - `openai`：`/v1/chat/completions` SSE（也覆盖 DeepSeek/Moonshot/本地 vLLM）
/// - `anthropic`：`/v1/messages` SSE
/// - `ollama`：`/api/chat` NDJSON（本地模型，无需 API key）
///
/// 未知 provider 名称返回 `Err`，列出支持的值供用户纠正。
///
/// # Errors
/// provider 构造失败（reqwest 初始化失败、tiktoken 词表加载失败等）时返回 `LlmError`。
fn build_main_provider(
    kind: &str,
    cfg: &ProviderConfig,
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
            // Ollama 默认 `http://localhost:11434`，无需 API key
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

/// 根据 `[provider.small]` 配置构造 small provider（gap-4，M2 roadmap L90）。
///
/// `small_cfg.api_base`/`api_key` 为 `None` 时继承主 `[provider]` 配置：
/// 让用户用便宜模型（如 `gpt-4o-mini`）做摘要/压缩/记忆提取，降本见
/// `design.md` §3.8。
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
