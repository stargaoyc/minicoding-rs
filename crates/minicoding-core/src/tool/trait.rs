//! `Tool` trait + `ToolContext` + `SideEffect`（见 `api.md` §3.3）。
//!
//! 手动 `Pin<Box<dyn Future + Send>>` 返回类型保证 `dyn` 兼容。

use crate::journal::Journal;
use crate::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use crate::provider::BoxFuture;
use crate::sandbox::{SandboxDriver, SandboxPolicy};
use camino::Utf8PathBuf;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// 工具执行上下文（每轮调用时构造）。
///
/// `sandbox_driver` / `sandbox_policy` / `journal` 为可选注入（M4 起）：
/// - `sandbox_driver` + `sandbox_policy`：`shell.run` 在 spawn 子进程前调用
///   `SandboxDriver::apply` 应用内核级沙箱（第二道防线，C-22）；
/// - `journal`：`fs.write`/`fs.edit`/`fs.delete` 成功后调 `Journal::record` 记录
///   文件改动用于 `/undo`（仅 `file-undo` feature 启用时注入，C-28）。
///
/// 未注入时这些能力退化为 no-op（兼容 M1-M3 测试）。
#[derive(Clone)]
pub struct ToolContext {
    pub workdir: Utf8PathBuf,
    pub session_id: String,
    pub canceller: CancellationToken,
    pub env: HashMap<String, String>,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    /// OS 沙箱驱动（可选，`shell.run` 用）。
    pub sandbox_driver: Option<Arc<dyn SandboxDriver>>,
    /// OS 沙箱策略（可选，与 `sandbox_driver` 配套）。
    pub sandbox_policy: Option<SandboxPolicy>,
    /// 文件改动 journal（可选，`fs.write`/`fs.edit`/`fs.delete` 用）。
    pub journal: Option<Arc<dyn Journal>>,
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("workdir", &self.workdir)
            .field("session_id", &self.session_id)
            .field("timeout", &self.timeout)
            .field("has_sandbox", &self.sandbox_driver.is_some())
            .field("has_journal", &self.journal.is_some())
            .finish_non_exhaustive()
    }
}

impl ToolContext {
    /// 创建默认上下文（无沙箱/journal 注入，兼容 M1-M3 测试）。
    #[must_use]
    pub fn new(workdir: Utf8PathBuf, session_id: String) -> Self {
        Self {
            workdir,
            session_id,
            canceller: CancellationToken::new(),
            env: HashMap::new(),
            timeout: Duration::from_secs(120),
            max_output_bytes: 1024 * 1024,
            sandbox_driver: None,
            sandbox_policy: None,
            journal: None,
        }
    }

    /// 链式注入沙箱驱动与策略。
    #[must_use]
    pub fn with_sandbox(mut self, driver: Arc<dyn SandboxDriver>, policy: SandboxPolicy) -> Self {
        self.sandbox_driver = Some(driver);
        self.sandbox_policy = Some(policy);
        self
    }

    /// 链式注入 journal。
    #[must_use]
    pub fn with_journal(mut self, journal: Arc<dyn Journal>) -> Self {
        self.journal = Some(journal);
        self
    }

    /// 链式注入可选 journal（`Option` 版本，便于 `Runtime` 透传 `Option<Arc<...>>`）。
    #[must_use]
    pub fn with_journal_opt(mut self, journal: Option<Arc<dyn Journal>>) -> Self {
        self.journal = journal;
        self
    }
}

/// 工具 trait（可替换能力契约，`dyn` 兼容）。
///
/// 内置工具在 `minicoding-tools` 实现，MCP 工具在 `minicoding-mcp` 包装。
/// `is_read_only()` 默认基于 `side_effect()`，MCP 工具可据 `readOnlyHint` 覆盖。
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> &ToolSchema;
    fn side_effect(&self) -> SideEffect;

    /// 是否只读（用于 Plan 模式硬门，见 `design.md` §16.1）。
    /// 默认实现：`self.side_effect() == SideEffect::None`。
    /// MCP 工具根据 server schema 的 `readOnlyHint` 覆盖。
    fn is_read_only(&self) -> bool {
        self.side_effect() == SideEffect::None
    }

    /// 执行工具调用。
    fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>>;
}
