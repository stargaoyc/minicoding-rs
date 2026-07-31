//! `RuntimeBuilder`：分步注入可替换能力，构造 `Runtime`。
//!
//! 必填：`provider` / `ctx` / `storage` / `workdir`。
//! 可选：`tools`（默认空）、`config`（默认 `RuntimeConfig::default()`）、`events`（默认新建）。

use crate::config::RuntimeConfig;
use crate::context::ContextManager;
use crate::model::Session;
use crate::provider::LlmProvider;
use crate::runtime::Runtime;
use crate::runtime::event::EventBus;
use crate::storage::Storage;
use crate::tool::ToolRegistry;
use camino::Utf8PathBuf;
use std::sync::Arc;

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

    /// 构造 `Runtime`。
    ///
    /// # Errors
    /// 必填项缺失时返回错误字符串。
    pub fn build(self) -> Result<Runtime, String> {
        let provider = self.provider.ok_or("provider is required")?;
        let ctx = self.ctx.ok_or("context manager is required")?;
        let storage = self.storage.ok_or("storage is required")?;
        let workdir = self.workdir.ok_or("workdir is required")?;

        let session = Session::new(workdir.clone(), self.config_hash);

        Ok(Runtime {
            provider,
            ctx,
            storage,
            tools: self.tools,
            config: self.config,
            session,
            events: self.events,
            workdir,
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
            .field("tools_count", &self.tools.len())
            .finish_non_exhaustive()
    }
}
