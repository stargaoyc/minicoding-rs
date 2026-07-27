//! `ToolRegistry`：工具注册与按 `side_effect` 调度。

use crate::model::{SideEffect, ToolCall, ToolError, ToolResult, ToolSchema};
use crate::tool::{Tool, ToolContext};
use std::collections::HashMap;
use std::sync::Arc;

/// 工具组（用于特性门控与批量启用）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ToolGroup {
    Core,
    Fs,
    Shell,
    Web,
    Git,
    Task,
    Plan,
    Mcp,
}

/// 工具注册表。
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// 创建空注册表。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册工具（同名覆盖）。
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.insert(name, tool);
    }

    /// 按名查找。
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// 所有已注册工具的 schema（供 LLM 调用参考）。
    #[must_use]
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(|t| t.schema().clone()).collect()
    }

    /// 派发工具调用。
    ///
    /// M1 简化：不包含权限检查与沙箱应用（M2 接入 `PermissionPolicy`/`SandboxDriver`）。
    /// 工具执行超时由 `ctx.timeout` 控制。
    ///
    /// # Errors
    /// 工具未注册时返回 `ToolError::NotFound`；工具执行失败时返回对应 `ToolError`。
    pub async fn dispatch(
        &self,
        call: &ToolCall,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let tool = self
            .tools
            .get(&call.name)
            .ok_or_else(|| ToolError::NotFound(call.name.clone()))?;
        let _ = tool.side_effect(); // M2 用于调度策略
        let _ = SideEffect::None; // 标记 M2 将使用
        tool.execute(call.input.clone(), ctx).await
    }

    /// 已注册工具数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("count", &self.tools.len())
            .finish()
    }
}
