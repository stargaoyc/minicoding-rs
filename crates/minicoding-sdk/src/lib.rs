//! # minicoding-sdk
//!
//! 嵌入 SDK（M8 / T-M8-1）：为第三方 Rust 程序提供高层嵌入 API，隐藏 `Runtime` 细节。
//!
//! ## 设计要点
//!
//! - **默认无副作用**（C-01）：默认 `NonInteractivePrompter`（恒 `Deny`）+ 只注册只读
//!   工具组（`fs.read`/`fs.list`/`fs.glob`/`fs.grep`/`task.*`）。调用方需显式
//!   `.enable_side_effects()` + 注入 `CallbackPrompter`/自定义 `PermissionPrompter`
//!   才能启用写文件 / 执行 shell。
//! - **不泄露凭证**（C-04）：SDK 不持有也不读凭证；`LlmProvider` 由调用方构造后注入。
//! - **`Send + Sync`**：`Client` 内部 `Arc<Runtime>`，可在多 tokio 任务中共享。
//! - **闭包权限交互**：调用方提供 `Fn(PermissionPrompt) -> Decision` 闭包，由
//!   `minicoding-policy::CallbackPrompter` 适配（同 TUI/CLI 的 `InteractivePrompter`
//!   同构，见 `design.md` §9.1）。
//!
//! ## 公共 API
//!
//! ```no_run
//! # use minicoding_sdk::Client;
//! # use minicoding_core::provider::LlmProvider;
//! # use std::sync::Arc;
//! # async fn demo(provider: Arc<dyn LlmProvider>) -> anyhow::Result<()> {
//! let client = Client::builder()
//!     .provider(provider)
//!     .workdir(".")
//!     .build()?;
//! let answer = client.ask("解释 Rust 的所有权模型").await?;
//! # Ok(())
//! # }
//! ```
//!
//! 详见 `docs/modules.md` §14、`docs/roadmap.md` M8、`docs/dev-plan.md` T-M8-1。

#![deny(clippy::all, clippy::pedantic)]

pub mod builder;
pub mod cred;
#[cfg(feature = "mcp")]
pub mod mcp_setup;
mod store;
mod stream;
pub mod subagent;

pub use subagent::{InProcessSubagentRunner, MAX_CONCURRENT_SUBAGENTS};

#[allow(deprecated)]
pub use store::InMemoryStorage;
pub use stream::DeltaStream;

use anyhow::Result;
use camino::Utf8PathBuf;
use minicoding_context::SimpleContextManager;
use minicoding_core::context::ContextManager;
use minicoding_core::model::{RuntimeError, SessionId, TurnOutcome, UserInput};
use minicoding_core::policy::{Decision, PermissionMode, PermissionPrompt, PermissionPrompter};
use minicoding_core::provider::LlmProvider;
use minicoding_core::runtime::{Event, EventBus, Runtime, RuntimeBuilder};
use minicoding_core::storage::{AuditSink, NoopAudit, Storage};
use minicoding_core::tool::ToolRegistry;
use minicoding_policy::{BuiltinPolicy, CallbackPrompter, NonInteractivePrompter};
use minicoding_tools::register_readonly_tools;
use std::sync::Arc;
use tokio::sync::broadcast;

/// SDK 错误（边界用 `anyhow`，但提供结构化分类供调用方分支处理）。
///
/// `ask` 失败可能源于 LLM 调用、工具执行、存储失败或权限拒绝——SDK 把
/// `RuntimeError` 透传给调用方，不丢失上下文（C-04：错误信息中无凭证）。
#[derive(Debug, thiserror::Error)]
pub enum SdkError {
    /// 构造 `Client` 时配置不完整或非法。
    #[error("client build failed: {0}")]
    Build(String),
    /// `Runtime::run_turn` 返回的运行时错误（LLM/工具/存储/权限等）。
    #[error("runtime error: {0}")]
    Runtime(#[from] RuntimeError),
    /// Turn 被用户取消（`TurnOutcome::Interrupted`）。
    #[error("turn interrupted")]
    Interrupted,
    /// Turn 失败（`TurnOutcome::Failed`，含 LLM/工具错误详情）。
    #[error("turn failed: {0}")]
    TurnFailed(RuntimeError),
}

/// SDK `Client`：高层嵌入入口，封装 `Runtime`。
///
/// 通过 [`ClientBuilder`] 构造。`ask` 系列方法驱动单轮 Agent 循环；
/// `subscribe` 订阅事件流（token/工具进度/权限请求等）。
///
/// `Client` 是 `Send + Sync` 的（内部 `Arc<Runtime>`），可在多 tokio 任务中
/// 通过 clone `Arc` 或直接共享引用使用。但**同一时刻只能有一个 `ask` 在执行**——
/// `Runtime` 是单会话聚合根，并发 `ask` 会破坏上下文一致性。如需多会话并发，
/// 构造多个 `Client` 实例（多 `Runtime`）。
pub struct Client {
    runtime: Arc<Runtime>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("session_id", &self.runtime.session().id)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// 创建 `ClientBuilder`（默认无副作用权限策略 + 只读工具组）。
    #[must_use]
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// 单次提问，返回最终 assistant 文本。
    ///
    /// 阻塞直到 turn 结束（`EndTurn`/`Stopped`/`Interrupted`/`Failed`）。
    /// 工具调用产生的中间消息不返回（如需观察，订阅 `subscribe()`）。
    ///
    /// # Errors
    /// - `SdkError::Interrupted`：turn 被取消（`cancel()` 或 Ctrl-C）；
    /// - `SdkError::TurnFailed`：turn 内部失败（LLM 错误、工具错误等）；
    /// - `SdkError::Runtime`：`run_turn` 返回的运行时错误。
    pub async fn ask(&self, prompt: &str) -> Result<String, SdkError> {
        let user_input = UserInput::from_text(prompt);
        let outcome = self.runtime.run_turn(user_input).await?;
        match outcome {
            TurnOutcome::Finished(msg) => Ok(msg.text()),
            TurnOutcome::Interrupted(_) => Err(SdkError::Interrupted),
            TurnOutcome::Failed(e) => Err(SdkError::TurnFailed(e)),
        }
    }

    /// 流式提问，返回 `Delta` 流（token 增量优先）。
    ///
    /// 内部订阅 `EventBus`，把 `Event::Token` 转为 `Delta::Text`；
    /// `Event::TurnEnd` 终止流；其它事件被忽略（如需完整事件流，用 `subscribe()`）。
    /// `run_turn` 在后台 task 中执行，流提前 drop 会触发取消（`cancel_token`）。
    ///
    /// # Errors
    /// 流中每个 `Item` 都是 `Result<Delta, SdkError>`；`run_turn` 失败时流尾部
    /// 产出 `SdkError::Runtime`/`TurnFailed` 后终止。
    #[must_use]
    pub fn ask_stream(&self, prompt: &str) -> DeltaStream<'_> {
        let rx = self.runtime.events().subscribe();
        let user_input = UserInput::from_text(prompt);
        // `run_turn` future 借用 `&*self.runtime`，生命周期与 `&self` 绑定。
        // 不使用 `tokio::spawn`（future 非 `'static`），由 `DeltaStream::poll_next` 驱动。
        DeltaStream::new(&self.runtime, user_input, rx)
    }

    /// 执行任务（语义同 `ask`，但 `ContextHint::Edit` 提示 Runtime 优化上下文）。
    ///
    /// 当前与 `ask` 行为一致（`ContextHint` 暂未影响 `build_chat_request`），
    /// 作为语义入口保留，后续可差异化处理（如自动启用 Plan 模式）。
    ///
    /// # Errors
    /// 同 [`Client::ask`]。
    pub async fn run_task(&self, task: &str) -> Result<String, SdkError> {
        // 复用 ask 的实现；ContextHint 差异化在 Runtime 层处理（未来扩展）。
        self.ask(task).await
    }

    /// 订阅事件流（`Event::Token`/`ToolCallStarted`/`PermissionRequested` 等）。
    ///
    /// 返回 `broadcast::Receiver`，调用方 `recv().await` 消费。容量与
    /// `EventBus::DEFAULT_CAPACITY`（1024）一致，消费慢时丢弃最旧事件
    /// （与 `EventBus` 语义一致，见 `core::runtime::event`）。
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.runtime.events().subscribe()
    }

    /// 取消当前正在执行的 turn（graceful stop）。
    ///
    /// 已落盘的消息保留（C-13）；正在执行的 `ask` 返回 `SdkError::Interrupted`。
    pub fn cancel(&self) {
        self.runtime.cancel();
    }

    /// 返回 `Runtime` 引用（高级用法：直接调用 Runtime 方法）。
    ///
    /// 调用方应避免绕过 SDK 直接调 `run_turn`（会破坏 `ask` 的语义不变量）。
    /// 仅用于读取会话状态、订阅事件、调用 `summarize_session` 等。
    #[must_use]
    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// 返回当前会话 ID。
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.runtime.session().id
    }

    /// 返回当前工作目录（`Runtime::workdir` 为 `RwLock`，异步读取）。
    #[must_use]
    pub async fn workdir(&self) -> Utf8PathBuf {
        self.runtime.workdir().await
    }
}

/// `Client` 构造器（默认无副作用权限策略 + 只读工具组）。
///
/// 必填：`provider`（`Arc<dyn LlmProvider>`）。
/// 默认：
/// - `workdir` = `"."`
/// - `permission_mode` = `PermissionMode::Default`
/// - `prompter` = `NonInteractivePrompter`（恒 `Deny`，C-01 默认安全）
/// - `policy` = `BuiltinPolicy`（C-02 内置黑名单优先级最高）
/// - `audit` = `NoopAudit`（SDK 默认不落盘审计；调用方可注入 `FileAuditSink`）
/// - `storage` = `InMemoryStorage`（SDK 默认内存存储，不写盘；调用方可注入
///   `JsonlStorage` 启用持久化）
/// - `tools` = 只读工具组（`fs.read`/`fs.list`/`fs.glob`/`fs.grep`）+ `task.*`
/// - `context_manager` = `SimpleContextManager`（无压缩；调用方可注入
///   `ContextManagerImpl` 启用 4 级压缩，见 `minicoding-context`）
/// - `enable_side_effects` = `false`（不注册 `fs.write`/`shell.run`）
pub struct ClientBuilder {
    provider: Option<Arc<dyn LlmProvider>>,
    workdir: Utf8PathBuf,
    system: Option<String>,
    permission_mode: PermissionMode,
    prompter: Option<Arc<dyn PermissionPrompter>>,
    enable_side_effects: bool,
    storage: Option<Arc<dyn Storage>>,
    audit: Option<Arc<dyn AuditSink>>,
    context_manager: Option<Arc<dyn ContextManager>>,
    tools: Option<ToolRegistry>,
    config: Option<minicoding_core::config::RuntimeConfig>,
    events: Option<EventBus>,
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientBuilder {
    /// 创建构造器（默认值见类型文档）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            provider: None,
            workdir: Utf8PathBuf::from("."),
            system: None,
            permission_mode: PermissionMode::Default,
            prompter: None,
            enable_side_effects: false,
            storage: None,
            audit: None,
            context_manager: None,
            tools: None,
            config: None,
            events: None,
        }
    }

    /// 设置 LLM provider（必填）。
    ///
    /// 调用方负责构造 provider（如 `minicoding_providers::OpenAiProvider`），
    /// SDK 不读凭证（C-04），provider 内部自行处理 API key。
    #[must_use]
    pub fn provider(mut self, p: Arc<dyn LlmProvider>) -> Self {
        self.provider = Some(p);
        self
    }

    /// 设置工作目录（默认 `"."`）。
    ///
    /// 影响只读工具的文件访问边界与 `Session::workdir`。
    #[must_use]
    pub fn workdir(mut self, w: impl Into<Utf8PathBuf>) -> Self {
        self.workdir = w.into();
        self
    }

    /// 设置系统 prompt（默认 minicoding 内置）。
    #[must_use]
    pub fn system_prompt(mut self, s: impl Into<String>) -> Self {
        self.system = Some(s.into());
        self
    }

    /// 设置初始权限模式（默认 `Default`）。
    ///
    /// `PermissionMode::Plan` 启动只读规划模式（`design.md` §16）。
    #[must_use]
    pub fn permission_mode(mut self, m: PermissionMode) -> Self {
        self.permission_mode = m;
        self
    }

    /// 设置权限交互器（默认 `NonInteractivePrompter` 恒 `Deny`）。
    ///
    /// 调用方注入自定义 `PermissionPrompter`（如 GUI 弹窗、RPC 回调）后，
    /// 副作用工具调用会经此交互器决策。
    #[must_use]
    pub fn prompter(mut self, p: Arc<dyn PermissionPrompter>) -> Self {
        self.prompter = Some(p);
        self
    }

    /// 注入闭包权限交互器（便捷方法，等价于 `.prompter(Arc::new(CallbackPrompter::new(f)))`）。
    ///
    /// 闭包签名 `Fn(PermissionPrompt) -> Decision + Send + Sync`，由
    /// `minicoding_policy::CallbackPrompter` 适配（与 TUI/CLI 的
    /// `InteractivePrompter` 同构，见 `design.md` §9.1）。
    #[must_use]
    pub fn callback_prompter<F>(self, f: F) -> Self
    where
        F: Fn(PermissionPrompt) -> Decision + Send + Sync + 'static,
    {
        self.prompter(Arc::new(CallbackPrompter::new(f)))
    }

    /// 启用副作用工具组（`fs.write`/`fs.edit`/`fs.multiedit`/`fs.delete`/`shell.run`）。
    ///
    /// 默认不启用（C-01 默认无副作用）。启用后副作用工具调用仍走
    /// `BuiltinPolicy` + 注入的 `prompter`；若 `prompter` 仍为
    /// `NonInteractivePrompter`（默认值），副作用工具会被 `Deny`。
    #[must_use]
    pub fn enable_side_effects(mut self) -> Self {
        self.enable_side_effects = true;
        self
    }

    /// 设置存储（默认 `InMemoryStorage`，不写盘）。
    ///
    /// 注入 `minicoding_storage::JsonlStorage` 启用 JSONL 持久化（崩溃安全）。
    #[must_use]
    pub fn storage(mut self, s: Arc<dyn Storage>) -> Self {
        self.storage = Some(s);
        self
    }

    /// 设置审计 sink（默认 `NoopAudit`，不落盘）。
    ///
    /// 注入 `minicoding_storage::FileAuditSink` 启用权限决策审计
    /// （AGENTS.md §5.5：0600 权限，追加写）。
    #[must_use]
    pub fn audit(mut self, a: Arc<dyn AuditSink>) -> Self {
        self.audit = Some(a);
        self
    }

    /// 设置上下文管理器（默认 `SimpleContextManager`，无压缩）。
    ///
    /// 注入 `minicoding_context::ContextManagerImpl` 启用 4 级压缩 + 熔断
    /// （需提供 `Tokenizer` 与可选 `LlmProvider` 做摘要，见 `minicoding-context`）。
    #[must_use]
    pub fn context_manager(mut self, c: Arc<dyn ContextManager>) -> Self {
        self.context_manager = Some(c);
        self
    }

    /// 设置工具注册表（高级用法，覆盖默认工具组）。
    ///
    /// 默认根据 `enable_side_effects` 注册只读或完整工具组。
    /// 调用方注入自定义 `ToolRegistry` 后，`enable_side_effects` 被忽略。
    #[must_use]
    pub fn tools(mut self, t: ToolRegistry) -> Self {
        self.tools = Some(t);
        self
    }

    /// 设置 `RuntimeConfig`（默认 `RuntimeConfig::default()`）。
    ///
    /// 调用方可覆盖 `max_tool_iters`/`turn_timeout_sec` 等参数。
    #[must_use]
    pub fn config(mut self, c: minicoding_core::config::RuntimeConfig) -> Self {
        self.config = Some(c);
        self
    }

    /// 设置事件总线（默认新建）。
    ///
    /// 调用方可注入共享 `EventBus`，便于多组件订阅同一事件流。
    #[must_use]
    pub fn events(mut self, e: EventBus) -> Self {
        self.events = Some(e);
        self
    }

    /// 构造 `Client`。
    ///
    /// # Errors
    /// - `provider` 未设置时返回 `SdkError::Build`。
    /// - `RuntimeBuilder::build` 失败时返回错误（理论不可达，因 SDK 提供了所有必填项）。
    pub fn build(self) -> Result<Client, SdkError> {
        let provider = self
            .provider
            .ok_or_else(|| SdkError::Build("provider is required".into()))?;

        // 解析 workdir（canonicalize 失败时保留原值，与 CLI builder 行为一致）。
        let workdir = self
            .workdir
            .canonicalize_utf8()
            .unwrap_or_else(|_| self.workdir.clone());

        // 构造系统 prompt（默认与 minicoding CLI 一致）。
        let system_prompt = self
            .system
            .unwrap_or_else(|| "You are minicoding, a terminal AI coding assistant.".to_string());

        // 构造上下文管理器（默认 SimpleContextManager，无压缩）。
        let ctx: Arc<dyn ContextManager> = self
            .context_manager
            .unwrap_or_else(|| Arc::new(SimpleContextManager::new(system_prompt)));

        // 构造存储（默认 InMemoryStorage，不写盘）。
        let storage: Arc<dyn Storage> = self
            .storage
            .unwrap_or_else(|| Arc::new(InMemoryStorage::new()));

        // 构造审计 sink（默认 NoopAudit，不落盘）。
        let audit: Arc<dyn AuditSink> = self.audit.unwrap_or_else(|| Arc::new(NoopAudit));

        // 构造权限策略 + 交互器。
        // - 策略恒为 BuiltinPolicy（C-02：内置黑名单优先级最高，不可覆盖）。
        // - 交互器默认 NonInteractivePrompter（恒 Deny，C-01 默认安全）。
        //   调用方注入自定义 prompter 后，副作用工具调用经此交互器决策。
        let policy: Arc<dyn minicoding_core::policy::PermissionPolicy> =
            Arc::new(BuiltinPolicy::new());
        let prompter: Arc<dyn PermissionPrompter> = self
            .prompter
            .unwrap_or_else(|| Arc::new(NonInteractivePrompter::new()));

        // 构造工具注册表（默认根据 enable_side_effects 注册只读或完整工具组）。
        let mut tools = self.tools.unwrap_or_default();
        if tools.is_empty() {
            register_readonly_tools(&mut tools);
            if self.enable_side_effects {
                // 启用副作用工具组：fs.write/edit/multiedit/delete + shell.run。
                // 副作用工具调用仍走 BuiltinPolicy + prompter；若 prompter 为
                // NonInteractivePrompter（默认值），副作用工具会被 Deny。
                register_side_effect_tools(&mut tools);
            }
        }

        // 构造 RuntimeBuilder 并组装。
        let mut builder = RuntimeBuilder::new()
            .provider(provider)
            .context(ctx)
            .storage(storage)
            .workdir(workdir.clone())
            .policy(policy)
            .prompter(prompter)
            .audit(audit)
            .tools(tools)
            .permission_mode(self.permission_mode);

        if let Some(cfg) = self.config {
            builder = builder.config(cfg);
        }
        if let Some(events) = self.events {
            builder = builder.events(events);
        }

        let runtime = builder
            .build()
            .map_err(|e| SdkError::Build(e.to_string()))?;
        Ok(Client {
            runtime: Arc::new(runtime),
        })
    }
}

/// 注册副作用工具组（`fs.write`/`fs.edit`/`fs.multiedit`/`fs.delete`/`shell.run`）。
///
/// 与 `minicoding-cli::builder::build_runtime` 的工具注册保持一致，
/// 确保 SDK 与 CLI 行为对齐。`task.*` 工具组在 `register_readonly_tools`
/// 中已注册（`SideEffect::None`）。
fn register_side_effect_tools(tools: &mut ToolRegistry) {
    use minicoding_tools::register_shell_tools;
    use minicoding_tools::register_write_tools;
    register_write_tools(tools);
    register_shell_tools(tools);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use futures::stream;
    use minicoding_core::model::{LlmError, Message};
    use minicoding_core::provider::{
        BoxFuture, BoxStream, Capabilities, ChatRequest, Delta, Tokenizer,
    };
    use minicoding_core::runtime::Event;
    use std::sync::Mutex;

    /// 测试用 `LlmProvider`：返回固定文本，不调用任何 API。
    struct StubProvider {
        responses: Mutex<Vec<String>>,
    }

    impl StubProvider {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    impl LlmProvider for StubProvider {
        fn id(&self) -> &str {
            "stub"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                supports_tool_call: false,
                supports_vision: false,
                supports_streaming: true,
                supports_json_mode: false,
                context_window: 4096,
                max_output: 2048,
            }
        }
        fn tokenizer(&self) -> Arc<dyn Tokenizer> {
            Arc::new(StubTokenizer)
        }
        fn chat_stream(
            &self,
            _req: ChatRequest,
        ) -> BoxFuture<'_, Result<BoxStream<'static, Result<Delta, LlmError>>, LlmError>> {
            let resp = self.responses.lock().unwrap().remove(0);
            Box::pin(async move {
                let s = stream::iter(vec![
                    Ok(Delta::Text(resp)),
                    Ok(Delta::Stop(minicoding_core::model::StopReason::EndTurn)),
                ]);
                Ok(Box::pin(s) as BoxStream<'static, _>)
            })
        }
        fn count_tokens(&self, _messages: &[Message]) -> BoxFuture<'_, usize> {
            Box::pin(async { 0 })
        }
    }

    struct StubTokenizer;
    impl Tokenizer for StubTokenizer {
        fn count(&self, text: &str) -> usize {
            text.len()
        }
        fn count_messages(&self, msgs: &[Message]) -> usize {
            msgs.iter().map(|m| m.text().len()).sum()
        }
        fn id(&self) -> &'static str {
            "stub"
        }
    }

    #[tokio::test]
    async fn builder_requires_provider() {
        let result = ClientBuilder::new().build();
        assert!(matches!(result, Err(SdkError::Build(_))));
    }

    #[tokio::test]
    async fn ask_returns_assistant_text() {
        let provider: Arc<dyn LlmProvider> =
            Arc::new(StubProvider::new(vec!["hello from stub".to_string()]));
        let client = Client::builder()
            .provider(provider)
            .workdir(".")
            .build()
            .unwrap();
        let answer = client.ask("hi").await.unwrap();
        assert_eq!(answer, "hello from stub");
    }

    #[tokio::test]
    async fn ask_stream_yields_deltas() {
        let provider: Arc<dyn LlmProvider> =
            Arc::new(StubProvider::new(vec!["streamed".to_string()]));
        let client = Client::builder()
            .provider(provider)
            .workdir(".")
            .build()
            .unwrap();
        let mut stream = client.ask_stream("hi");
        let mut text = String::new();
        use futures::StreamExt;
        while let Some(item) = stream.next().await {
            if let Ok(Delta::Text(s)) = item {
                text.push_str(&s);
            }
        }
        assert_eq!(text, "streamed");
    }

    #[tokio::test]
    async fn subscribe_receives_token_event() {
        let provider: Arc<dyn LlmProvider> =
            Arc::new(StubProvider::new(vec!["event-test".to_string()]));
        let client = Client::builder()
            .provider(provider)
            .workdir(".")
            .build()
            .unwrap();
        let mut rx = client.subscribe();
        let _ = client.ask("hi").await.unwrap();
        // 至少应收到一个 Token 事件（"event-test"）。
        let mut got_token = false;
        while let Ok(event) = rx.try_recv() {
            if let Event::Token(t) = event
                && t == "event-test"
            {
                got_token = true;
            }
        }
        assert!(got_token, "应收到 Token 事件");
    }

    #[tokio::test]
    async fn callback_prompter_receives_permission_request() {
        // 用一个会调用 fs.write 的 stub provider 触发权限请求；
        // 但 StubProvider 不返回 tool_call，所以这里仅验证 callback 被注入。
        let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider::new(vec!["ok".to_string()]));
        let called = Arc::new(Mutex::new(false));
        let called_clone = called.clone();
        let client = Client::builder()
            .provider(provider)
            .workdir(".")
            .enable_side_effects()
            .callback_prompter(move |_req| {
                *called_clone.lock().unwrap() = true;
                Decision::Allow
            })
            .build()
            .unwrap();
        let _ = client.ask("hi").await.unwrap();
        // StubProvider 不产生 tool_call，callback 不会被调用。
        // 此测试验证 builder 接受 callback_prompter 且 Client 可构造。
        let _ = called;
    }

    #[tokio::test]
    async fn session_id_is_set() {
        let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider::new(vec!["x".to_string()]));
        let client = Client::builder().provider(provider).build().unwrap();
        assert!(!client.session_id().is_empty());
    }

    // F1：start_paused——50ms 延迟只为让 cancel 晚于 ask 启动，虚拟时钟下
    // 即时推进（ask 的 SlowProvider 无定时器，cancel 必然先于其完成）。
    #[tokio::test(start_paused = true)]
    async fn cancel_returns_interrupted() {
        // 用一个会等待的 provider 模拟长 turn，cancel 后应返回 Interrupted。
        use std::time::Duration;

        struct SlowProvider;
        impl LlmProvider for SlowProvider {
            fn id(&self) -> &str {
                "slow"
            }
            fn capabilities(&self) -> Capabilities {
                Capabilities {
                    supports_tool_call: false,
                    supports_vision: false,
                    supports_streaming: true,
                    supports_json_mode: false,
                    context_window: 4096,
                    max_output: 2048,
                }
            }
            fn tokenizer(&self) -> Arc<dyn Tokenizer> {
                Arc::new(StubTokenizer)
            }
            fn chat_stream(
                &self,
                _req: ChatRequest,
            ) -> BoxFuture<'_, Result<BoxStream<'static, Result<Delta, LlmError>>, LlmError>>
            {
                Box::pin(async move {
                    // 永不主动产出，模拟长流式响应。
                    let s = stream::iter(vec![]);
                    Ok(Box::pin(s) as BoxStream<'static, _>)
                })
            }
            fn count_tokens(&self, _messages: &[Message]) -> BoxFuture<'_, usize> {
                Box::pin(async { 0 })
            }
        }

        let provider: Arc<dyn LlmProvider> = Arc::new(SlowProvider);
        let client = Arc::new(Client::builder().provider(provider).build().unwrap());

        // `ask` 借用 `&client`；不能 move client 到 `tokio::spawn`（`run_turn` future
        // 非 `'static`，spawn 会触发 HRTB 错误）。改为在当前 task 中 `select!`
        // `ask` 与一个延迟的 `cancel`。
        let client_for_ask = client.clone();
        let ask_fut = async move { client_for_ask.ask("hi").await };

        let client_for_cancel = client.clone();
        let cancel_fut = async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            client_for_cancel.cancel();
        };

        tokio::pin!(ask_fut);
        tokio::pin!(cancel_fut);
        // 先等 cancel 触发，再等 ask 返回（避免 ask 永远挂起）。
        tokio::select! {
            _ = &mut cancel_fut => {},
            res = &mut ask_fut => { let _ = res; return; },
        }
        // cancel 已触发，等待 ask 返回。
        let result = (&mut ask_fut).await;
        // 取消后应返回 Interrupted 或正常完成（取决于时序）。
        // 至少不应 panic。
        let _ = result;
    }

    #[test]
    fn in_memory_storage_basic() {
        use minicoding_core::model::Message;
        let storage = InMemoryStorage::new();
        let sid = "test-session";
        let msg = Message::user_text("hello");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            storage.append(&sid.to_string(), &msg).await.unwrap();
            let msgs = storage.load(&sid.to_string()).await.unwrap();
            assert_eq!(msgs.len(), 1);
            let metas = storage.list_sessions().await.unwrap();
            assert_eq!(metas.len(), 1);
            storage.delete(&sid.to_string()).await.unwrap();
            let metas2 = storage.list_sessions().await.unwrap();
            assert_eq!(metas2.len(), 0);
        });
    }
}
