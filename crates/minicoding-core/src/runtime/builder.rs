//! `RuntimeBuilder`：分步注入可替换能力，构造 `Runtime`。
//!
//! 必填：`provider` / `ctx` / `storage` / `workdir`。
//! 可选：`tools`（默认空）、`config`（默认 `RuntimeConfig::default()`）、`events`（默认新建）、
//! `policy`/`prompter`/`audit`（默认 `NoopPolicy`/`NoopPrompter`/`NoopAudit` 兜底，
//! 真实场景由 frontend 注入 `minicoding-policy`/`minicoding-storage` 实现，见 M2）。

use crate::agent::{NoopSubagentRunner, SubagentRunner};
use crate::config::{ConfigWatcher, RuntimeConfig};
use crate::context::ContextManager;
use crate::extension::{ExtensionHost, NoopExtensionHost};
use crate::hooks::{HookRegistry, NoopHookRegistry};
use crate::journal::Journal;
use crate::memory::SessionSummarizer;
use crate::model::Session;
use crate::policy::{
    NoopPolicy, NoopPrompter, PermissionMode, PermissionPolicy, PermissionPrompter,
};
use crate::provider::LlmProvider;
use crate::runtime::EventBus;
use crate::runtime::Runtime;
use crate::sandbox::{
    DenialDetector, NoopDriver, SandboxCircuitBreaker, SandboxDriver, SandboxPolicy,
};
use crate::storage::{
    AuditSink, EventStore, NoopAudit, NoopEventStore, NoopSnapshotStore, SnapshotStore, Storage,
};
use crate::tool::ToolRegistry;
use camino::Utf8PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tokio::sync::{Mutex as TokioMutex, RwLock};
use tokio_util::sync::CancellationToken;

/// `Runtime` 构造器。
pub struct RuntimeBuilder {
    provider: Option<Arc<dyn LlmProvider>>,
    ctx: Option<Arc<dyn ContextManager>>,
    storage: Option<Arc<dyn Storage>>,
    tools: ToolRegistry,
    config: RuntimeConfig,
    events: EventBus,
    workdir: Option<Utf8PathBuf>,
    config_hash: u64,
    policy: Option<Arc<dyn PermissionPolicy>>,
    prompter: Option<Arc<dyn PermissionPrompter>>,
    audit: Option<Arc<dyn AuditSink>>,
    cancel_token: Option<CancellationToken>,
    /// 预加载会话（`--resume`/`--fork-session` 用，默认 `None` → 新建）。
    session: Option<Session>,
    /// 会话摘要生成器（默认 `None`，会话结束时由 CLI 调用 `Runtime::summarize_session`）。
    session_summarizer: Option<Arc<dyn SessionSummarizer>>,
    /// OS 沙箱驱动（默认 `NoopDriver`，M4 由 CLI 注入 `detect_driver()` 结果）。
    sandbox_driver: Option<Arc<dyn SandboxDriver>>,
    /// OS 沙箱策略（默认 `WorkspaceWrite { workdir, [] }`，由 `--sandbox`/`--preset` 设定）。
    sandbox_policy: Option<SandboxPolicy>,
    /// 文件改动 journal（默认 `None`，仅 `file-undo` feature 启用时注入）。
    journal: Option<Arc<dyn Journal>>,
    /// Hook 注册表（默认 `NoopHookRegistry`，M5 由 CLI 注入 `HookRegistryImpl`）。
    hook_registry: Option<Arc<dyn HookRegistry>>,
    /// 初始权限模式（默认 `Default`，`--plan` 启动时设为 `Plan`）。
    permission_mode: PermissionMode,
    /// 子 Agent runner（默认 `NoopSubagentRunner`，M5 由 CLI 注入实现）。
    subagent_runner: Option<Arc<dyn SubagentRunner>>,
    /// 扩展宿主（默认 `NoopExtensionHost`，启用扩展时由 CLI 注入 `BundledExtensionHost`）。
    ///
    /// Runtime 持有 `Arc<dyn ExtensionHost>` 用于运行期 `unload_extension`/
    /// `on_config_changed`。`shutdown_all` 是 `BundledExtensionHost` 的 inherent 方法，
    /// 由 CLI 在会话退出前通过持有的原始 `Arc<BundledExtensionHost>` 调用。
    extension_host: Option<Arc<dyn ExtensionHost>>,
    /// 配置文件监听器（S-22，默认 `None` → 不启用热更新）。
    ///
    /// CLI 注入 `ConfigWatcher::start(...)` 结果；`ConfigWatcher` 随 `Runtime` 存活，
    /// drop 时自动停止监听并结束后台 task。未注入时不监听配置变更。
    config_watcher: Option<ConfigWatcher>,
    /// 事件存储（Event Sourcing，默认 `NoopEventStore`）。
    ///
    /// 注入后 Runtime 在 `emit(Event)` 同时持久化 `PersistedEvent` 到事件流，
    /// 支持 `--replay` 事件重放与 SSE cursor durable 恢复（见 `design.md` §25）。
    event_store: Option<Arc<dyn EventStore>>,
    /// Snapshot 存储（Event Sourcing，默认 `NoopSnapshotStore`）。
    ///
    /// 注入后 Runtime 在每 `SNAPSHOT_INTERVAL` 条 `MessageAppended` 事件后
    /// 落盘 snapshot，加速 `replay_session_state`（见 `design.md` §25.3）。
    snapshot_store: Option<Arc<dyn SnapshotStore>>,
}

impl Default for RuntimeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeBuilder {
    /// 创建空构造器。
    #[must_use]
    pub fn new() -> Self {
        Self {
            provider: None,
            ctx: None,
            storage: None,
            tools: ToolRegistry::new(),
            config: RuntimeConfig::default(),
            events: EventBus::new(),
            workdir: None,
            config_hash: 0,
            policy: None,
            prompter: None,
            audit: None,
            cancel_token: None,
            session: None,
            session_summarizer: None,
            sandbox_driver: None,
            sandbox_policy: None,
            journal: None,
            hook_registry: None,
            permission_mode: PermissionMode::Default,
            subagent_runner: None,
            extension_host: None,
            config_watcher: None,
            event_store: None,
            snapshot_store: None,
        }
    }

    /// 设置 LLM provider（必填）。
    #[must_use]
    pub fn provider(mut self, p: Arc<dyn LlmProvider>) -> Self {
        self.provider = Some(p);
        self
    }

    /// 设置上下文管理器（必填）。
    #[must_use]
    pub fn context(mut self, c: Arc<dyn ContextManager>) -> Self {
        self.ctx = Some(c);
        self
    }

    /// 设置存储（必填）。
    #[must_use]
    pub fn storage(mut self, s: Arc<dyn Storage>) -> Self {
        self.storage = Some(s);
        self
    }

    /// 设置工具注册表（默认空）。
    #[must_use]
    pub fn tools(mut self, t: ToolRegistry) -> Self {
        self.tools = t;
        self
    }

    /// 设置配置（默认 `RuntimeConfig::default()`）。
    #[must_use]
    pub fn config(mut self, c: RuntimeConfig) -> Self {
        self.config_hash = crate::config::config_hash(&c);
        self.config = c;
        self
    }

    /// 设置事件总线（默认新建）。
    #[must_use]
    pub fn events(mut self, e: EventBus) -> Self {
        self.events = e;
        self
    }

    /// 设置工作目录（必填）。
    #[must_use]
    pub fn workdir(mut self, w: impl Into<Utf8PathBuf>) -> Self {
        self.workdir = Some(w.into());
        self
    }

    /// 设置权限策略（默认 `NoopPolicy` 恒 `Allow`，真实场景应注入
    /// `minicoding-policy::BuiltinPolicy`，见 C-01）。
    #[must_use]
    pub fn policy(mut self, p: Arc<dyn PermissionPolicy>) -> Self {
        self.policy = Some(p);
        self
    }

    /// 设置权限交互器（默认 `NoopPrompter` 恒 `Allow`，真实场景应注入
    /// `minicoding-policy::InteractivePrompter`/`NonInteractivePrompter`）。
    #[must_use]
    pub fn prompter(mut self, p: Arc<dyn PermissionPrompter>) -> Self {
        self.prompter = Some(p);
        self
    }

    /// 设置审计 sink（默认 `NoopAudit` 空操作，真实场景应注入
    /// `minicoding-storage::FileAuditSink`，见 AGENTS.md §5.5）。
    #[must_use]
    pub fn audit(mut self, a: Arc<dyn AuditSink>) -> Self {
        self.audit = Some(a);
        self
    }

    /// 设置取消 token（默认新建）。
    ///
    /// CLI 可注入共享 token，以便 Ctrl-C handler 调用 `token.cancel()` 触发
    /// `run_turn` graceful stop（C-13）。不设置时 Runtime 内部新建一个独立 token，
    /// 通过 `Runtime::cancel()` 仍可触发。
    #[must_use]
    pub fn cancel_token(mut self, t: CancellationToken) -> Self {
        self.cancel_token = Some(t);
        self
    }

    /// 设置预加载会话（`--resume`/`--fork-session` 用）。
    ///
    /// 未设置时 `build()` 新建空会话。设置后 `Runtime` 使用该会话的 `id` 与
    /// `messages`，后续 `run_turn` 的 `storage.append` 写入同一会话文件。
    /// 调用方需另行调用 `Runtime::restore_history` 将消息注入上下文管理器。
    #[must_use]
    pub fn session(mut self, s: Session) -> Self {
        self.session = Some(s);
        self
    }

    /// 设置会话摘要生成器（默认 `None`）。
    ///
    /// 注入后 CLI 可调用 `Runtime::summarize_session` 在会话结束时生成摘要
    /// 并落盘 `index.json`（T-M3-6）。未注入时 `summarize_session` 为 no-op。
    #[must_use]
    pub fn session_summarizer(mut self, s: Arc<dyn SessionSummarizer>) -> Self {
        self.session_summarizer = Some(s);
        self
    }

    /// 设置 OS 沙箱驱动（默认 `NoopDriver`，M4 由 CLI 注入 `detect_driver()`）。
    ///
    /// 注入后 `shell.run` 在 spawn 子进程前调 `SandboxDriver::apply` 应用内核级
    /// 沙箱（第二道防线，C-22）。未注入时退化为 `NoopDriver`（无 OS 隔离）。
    #[must_use]
    pub fn sandbox_driver(mut self, d: Arc<dyn SandboxDriver>) -> Self {
        self.sandbox_driver = Some(d);
        self
    }

    /// 设置 OS 沙箱策略（默认 `WorkspaceWrite { workdir, [] }`）。
    ///
    /// 由 `--sandbox`/`--preset` 解析后注入。与 `sandbox_driver` 配套使用。
    #[must_use]
    pub fn sandbox_policy(mut self, p: SandboxPolicy) -> Self {
        self.sandbox_policy = Some(p);
        self
    }

    /// 设置文件改动 journal（默认 `None`，仅 `file-undo` feature 启用时注入）。
    ///
    /// 注入后 `fs.write`/`fs.edit`/`fs.delete` 成功后调 `Journal::record` 记录
    /// 改动用于 `/undo`（C-28）。未注入时不记录，`/undo` 不可用。
    #[must_use]
    pub fn journal(mut self, j: Arc<dyn Journal>) -> Self {
        self.journal = Some(j);
        self
    }

    /// 设置 Hook 注册表（默认 `NoopHookRegistry`，M5 由 CLI 注入 `HookRegistryImpl`）。
    ///
    /// 注入后 `execute_side_effect_call` 在 `policy.check` 之后触发
    /// `PreToolUse`/`PermissionRequest`/`PostToolUse`/`PostToolUseFailure` Hook
    /// （见 `hooks.md` §4）。Hook 不可覆盖内置黑名单 Deny（C-21）。
    #[must_use]
    pub fn hook_registry(mut self, h: Arc<dyn HookRegistry>) -> Self {
        self.hook_registry = Some(h);
        self
    }

    /// 设置初始权限模式（默认 `Default`，`--plan` 启动时设为 `Plan`）。
    ///
    /// Runtime 启动时按此值初始化 `plan_state`。运行时切换通过
    /// `PlanModeController::set_mode`（CLI `/plan`）或 `exit_plan`（`plan.exit` 工具）。
    #[must_use]
    pub fn permission_mode(mut self, m: PermissionMode) -> Self {
        self.permission_mode = m;
        self
    }

    /// 设置子 Agent runner（默认 `NoopSubagentRunner`，M5 由 CLI 注入实现）。
    ///
    /// 注入后 `task.spawn` 工具可通过 `Runtime::subagent_runner()` 获取引用并派发
    /// 子 Agent（见 `design.md` §7.3）。未注入时 `task.spawn` 调用直接返回
    /// `RuntimeError::Config`（不静默 no-op，避免模型误以为已派发）。
    #[must_use]
    pub fn subagent_runner(mut self, r: Arc<dyn SubagentRunner>) -> Self {
        self.subagent_runner = Some(r);
        self
    }

    /// 设置扩展宿主（默认 `NoopExtensionHost`，启用扩展时由 CLI 注入
    /// `BundledExtensionHost`）。
    ///
    /// 注入后 Runtime 通过 `extension_host()` 暴露引用，CLI 可在会话退出前调用
    /// `shutdown_all`（`BundledExtensionHost` inherent 方法）释放扩展资源。
    /// 扩展注册的工具/Hook/PromptContributor 由 CLI 在 `load_extension` 后提取
    /// bundle 并提交到 Runtime 各注册表（`ToolRegistry`/`HookRegistry`/`PromptPipeline`）。
    #[must_use]
    pub fn extension_host(mut self, h: Arc<dyn ExtensionHost>) -> Self {
        self.extension_host = Some(h);
        self
    }

    /// 设置配置文件监听器（S-22，默认 `None` → 不启用热更新）。
    ///
    /// 注入后 `ConfigWatcher` 随 `Runtime` 存活，drop 时自动停止监听并结束后台 task。
    /// 监听失败由 `ConfigWatcher::start` 内部 best-effort 处理（记 warn，返回空壳），
    /// 此处直接接收其结果。
    #[must_use]
    pub fn with_config_watcher(mut self, w: ConfigWatcher) -> Self {
        self.config_watcher = Some(w);
        self
    }

    /// 设置事件存储（Event Sourcing，默认 `NoopEventStore`）。
    ///
    /// 注入后 Runtime 在 `emit(Event)` 同时持久化 `PersistedEvent` 到事件流，
    /// 支持 `--replay` 事件重放与 SSE cursor durable 恢复（见 `design.md` §25）。
    /// 未注入时退化为 `NoopEventStore`（不持久化），兼容旧会话。
    #[must_use]
    pub fn event_store(mut self, s: Arc<dyn EventStore>) -> Self {
        self.event_store = Some(s);
        self
    }

    /// 设置 Snapshot 存储（Event Sourcing，默认 `NoopSnapshotStore`）。
    ///
    /// 注入后 Runtime 在每 `SNAPSHOT_INTERVAL` 条 `MessageAppended` 事件后
    /// 落盘 snapshot，加速 `replay_session_state`（见 `design.md` §25.3）。
    /// 未注入时退化为 `NoopSnapshotStore`（不落盘 snapshot）。
    #[must_use]
    pub fn snapshot_store(mut self, s: Arc<dyn SnapshotStore>) -> Self {
        self.snapshot_store = Some(s);
        self
    }

    /// 构造 `Runtime`。
    ///
    /// **Event Sourcing 初始化**：`event_seq` 默认初始化为 1（新会话起始 seq），
    /// `durable_seq` 初始化为 0（无持久化进度）。`--resume`/`--replay` 场景下，
    /// 调用方需在 `build()` 后调用 [`Runtime::init_event_stream`] 异步加载
    /// `EventStore::next_seq` 与最近 snapshot 的 seq，并持久化 `SessionCreated`
    /// 事件（新会话首事件）。
    ///
    /// # Errors
    /// 必填项缺失时返回错误字符串。
    pub fn build(self) -> Result<Runtime, String> {
        let provider = self.provider.ok_or("provider is required")?;
        let ctx = self.ctx.ok_or("context manager is required")?;
        let storage = self.storage.ok_or("storage is required")?;
        let workdir = self.workdir.ok_or("workdir is required")?;

        let session = self
            .session
            .unwrap_or_else(|| Session::new(workdir.clone(), self.config_hash));

        // 沙箱策略默认 `WorkspaceWrite { workdir, [] }`（auto 预设，C-22 默认隔离）。
        let sandbox_policy = self
            .sandbox_policy
            .unwrap_or_else(|| SandboxPolicy::WorkspaceWrite {
                workdir: workdir.clone(),
                writable: Vec::new(),
            });

        Ok(Runtime {
            provider,
            ctx,
            storage,
            tools: self.tools,
            config: self.config,
            session,
            events: self.events,
            workdir,
            policy: self.policy.unwrap_or_else(|| Arc::new(NoopPolicy)),
            prompter: self.prompter.unwrap_or_else(|| Arc::new(NoopPrompter)),
            audit: self.audit.unwrap_or_else(|| Arc::new(NoopAudit)),
            cancel_token: self.cancel_token.unwrap_or_default(),
            session_summarizer: self.session_summarizer,
            sandbox_driver: self.sandbox_driver.unwrap_or_else(|| Arc::new(NoopDriver)),
            sandbox_policy,
            journal: self.journal,
            denial_detector: DenialDetector::new(),
            sandbox_breaker: SandboxCircuitBreaker::default_thresholds(),
            hook_registry: self
                .hook_registry
                .unwrap_or_else(|| Arc::new(NoopHookRegistry)),
            plan_state: Arc::new(RwLock::new(crate::policy::PlanModeSnapshot {
                mode: self.permission_mode,
                allowed_prompts: Vec::new(),
            })),
            subagent_runner: self
                .subagent_runner
                .unwrap_or_else(|| Arc::new(NoopSubagentRunner::new())),
            extension_host: self
                .extension_host
                .unwrap_or_else(|| Arc::new(NoopExtensionHost::new())),
            config_watcher: self.config_watcher,
            event_store: self.event_store.unwrap_or_else(|| Arc::new(NoopEventStore)),
            snapshot_store: self
                .snapshot_store
                .unwrap_or_else(|| Arc::new(NoopSnapshotStore)),
            // 默认值：新会话起始 seq=1；`init_event_stream` 会按 EventStore 实际情况修正。
            event_seq: Arc::new(TokioMutex::new(1)),
            message_since_snapshot: AtomicU64::new(0),
            durable_seq: Arc::new(TokioMutex::new(0)),
        })
    }
}

impl std::fmt::Debug for RuntimeBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeBuilder")
            .field("has_provider", &self.provider.is_some())
            .field("has_context", &self.ctx.is_some())
            .field("has_storage", &self.storage.is_some())
            .field("has_workdir", &self.workdir.is_some())
            .field("has_policy", &self.policy.is_some())
            .field("has_prompter", &self.prompter.is_some())
            .field("has_audit", &self.audit.is_some())
            .field("tools_count", &self.tools.len())
            .finish_non_exhaustive()
    }
}
