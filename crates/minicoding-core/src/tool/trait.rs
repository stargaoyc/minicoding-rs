//! `Tool` trait + `ToolContext` + `SideEffect`（见 `api.md` §3.3）。
//!
//! 手动 `Pin<Box<dyn Future + Send>>` 返回类型保证 `dyn` 兼容。

use crate::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use crate::provider::BoxFuture;
use camino::Utf8PathBuf;
use std::collections::HashMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// 工具执行上下文（每轮调用时构造）。
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub workdir: Utf8PathBuf,
    pub session_id: String,
    pub canceller: CancellationToken,
    pub env: HashMap<String, String>,
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

impl ToolContext {
    /// 创建默认上下文。
    #[must_use]
    pub fn new(workdir: Utf8PathBuf, session_id: String) -> Self {
        Self {
            workdir,
            session_id,
            canceller: CancellationToken::new(),
            env: HashMap::new(),
            timeout: Duration::from_secs(120),
            max_output_bytes: 1024 * 1024,
        }
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
