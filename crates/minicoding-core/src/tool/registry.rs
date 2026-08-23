//! `ToolRegistry`：工具注册与按 `side_effect` 调度。

use crate::model::{ToolCall, ToolError, ToolResult, ToolSchema};
use crate::tool::{Tool, ToolContext};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// 工具组（用于特性门控与批量启用，也作为子 Agent 工具子集的粗粒度开关）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
///
/// `Clone` 实现：内部 `HashMap<String, Arc<dyn Tool>>` 浅拷贝（仅克隆 `Arc`），
/// 用于在异步并行执行（如 `execute_tool_calls` 的只读桶）中把 `tools` 移入
/// `'static` async 块，避免捕获 `&Runtime` 导致 future 非 `'static`。
#[derive(Default, Clone)]
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
    ///
    /// **C-07 超时兜底**（2026-08-23 审查 §4-P2）：以 `ctx.timeout` 为单工具调用
    /// 硬上限统一包装——此前仅靠各工具自律读取 `ctx.timeout`，第三方扩展工具若
    /// 无视之则只剩 turn 级超时兜底（且会丢弃整个 turn）。自律工具（如 `shell.run`
    /// 内部 clamp + 进程组终止）自身的超时 ≤ `ctx.timeout`，先行优雅终止不受影响；
    /// 兜底路径放弃 future 后工具可能仍在后台运行至自然结束（无 kill 句柄可及），
    /// 这是无进程句柄下的最后防线取舍。`ctx.timeout` 为零视为不限制（测试用）。
    ///
    /// # Errors
    /// 工具未注册时返回 `ToolError::NotFound`；工具执行失败或超时时返回对应
    /// `ToolError`（超时为 `ToolError::Timeout`）。
    pub async fn dispatch(
        &self,
        call: &ToolCall,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let tool = self
            .tools
            .get(&call.name)
            .ok_or_else(|| ToolError::NotFound(call.name.clone()))?;
        if ctx.timeout.is_zero() {
            return tool.execute(call.input.clone(), ctx).await;
        }
        match tokio::time::timeout(ctx.timeout, tool.execute(call.input.clone(), ctx)).await {
            Ok(result) => result,
            Err(_) => Err(ToolError::Timeout(ctx.timeout)),
        }
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
