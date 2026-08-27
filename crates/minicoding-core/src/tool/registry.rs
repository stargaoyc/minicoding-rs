//! `ToolRegistry`：工具注册与按 `side_effect` 调度。

use crate::model::{ToolCall, ToolError, ToolResult, ToolSchema};
use crate::tool::{Tool, ToolContext};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// 工具组（用于特性门控与批量启用，也作为子 Agent 工具子集的粗粒度开关）。
///
/// **reserved（2026-08-23 审查 §6-P2）**：当前注册 API 未消费 group，
/// 子 Agent 工具子集裁剪未落地；保留枚举作为后续 `schemas()` 过滤与
/// `task.spawn` 按类型收敛工具面的契约占位。在接入前不得据此假设
/// "某 group 已被排除"。
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
    ///
    /// TL-3（2026-08-27 R5 审查）：`tool.name()` 是注册键与 dispatch 查找的唯一
    /// 事实源；`schema().name` 若与之不一致（第三方/MCP 工具声明漂移），LLM 会
    /// 按 schema 名字调用而 dispatch 按 name 查找——静默 `NotFound`。注册时校验
    /// 二者一致性并 warn（不拒绝：兼容 schema.name 为空的既有工具）。
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        let schema_name = tool.schema().name.as_str();
        if !schema_name.is_empty() && schema_name != name {
            tracing::warn!(
                tool.name = %name,
                schema.name = schema_name,
                "Tool::name() 与 schema().name 不一致——LLM 将按 schema.name 调用，
                 dispatch 按 Tool::name() 查找，二者不一致会导致 NotFound；
                 schemas() 已统一改写为 Tool::name() 兜底"
            );
        }
        self.tools.insert(name, tool);
    }

    /// 按名查找。
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// 所有已注册工具的 schema（供 LLM 调用参考）。
    ///
    /// TL-3（R5）：schema 的 `name` 字段统一改写为 `Tool::name()`——保证 LLM
    /// 看到的工具名与 dispatch 查找键一致（单一事实源），消除双字段漂移导致
    /// 的"调用 `NotFound`"。
    #[must_use]
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .values()
            .map(|t| {
                let mut schema = t.schema().clone();
                schema.name = t.name().to_string();
                schema
            })
            .collect()
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

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use crate::model::ToolResult;

    /// 测试工具：`name()` 与 `schema().name` 不一致（模拟第三方/MCP 工具漂移）。
    struct DriftingTool {
        schema: ToolSchema,
    }

    impl DriftingTool {
        fn new() -> Self {
            // 注意：schema.name 故意写错为 "fs.write"
            Self {
                schema: ToolSchema {
                    name: "fs.write".to_owned(),
                    description: "test".to_owned(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
            }
        }
    }

    impl Tool for DriftingTool {
        fn name(&self) -> &str {
            "fs.read"
        }
        fn schema(&self) -> &ToolSchema {
            &self.schema
        }
        fn side_effect(&self) -> crate::model::SideEffect {
            crate::model::SideEffect::None
        }
        fn execute(
            &self,
            _params: serde_json::Value,
            _ctx: &crate::tool::ToolContext,
        ) -> crate::provider::BoxFuture<'_, Result<ToolResult, ToolError>> {
            Box::pin(async { Ok(ToolResult::ok_text("ok".to_string())) })
        }
    }

    #[test]
    fn schemas_uses_tool_name_as_single_source_of_truth() {
        // TL-3（R5）：schema.name 与 Tool::name() 不一致时，schemas() 必须以
        // Tool::name() 为准——LLM 按 schema.name 调用、dispatch 按 name 查找，
        // 二者不一致会导致静默 `NotFound`。
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(DriftingTool::new()));
        let schemas = reg.schemas();
        assert_eq!(schemas.len(), 1);
        assert_eq!(
            schemas[0].name, "fs.read",
            "schemas() 应以 Tool::name() 改写 schema.name"
        );
        // dispatch 按 Tool::name() 可命中
        let call = ToolCall {
            id: "c1".into(),
            name: "fs.read".into(),
            input: serde_json::json!({}),
        };
        let ctx = ToolContext::new("/tmp".into(), "t".into());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt.block_on(reg.dispatch(&call, &ctx));
        assert!(r.is_ok(), "按 Tool::name() 查找应命中");
    }
}
