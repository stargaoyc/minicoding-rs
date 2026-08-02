//! `shell.kill`：终止后台 shell（T-M8-5）。
//!
//! 若 shell 已退出则无操作（幂等）。`SideEffect::Command`（终止进程属副作用）。

use super::background::BackgroundShellStore;
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::Tool;
use std::sync::Arc;

/// `shell.kill` 工具：终止后台 shell。
pub struct ShellKill {
    schema: ToolSchema,
    store: Arc<dyn BackgroundShellStore>,
}

impl ShellKill {
    /// 创建工具实例，注入共享 [`BackgroundShellStore`]。
    #[must_use]
    pub fn new(store: Arc<dyn BackgroundShellStore>) -> Self {
        let schema = ToolSchema {
            name: "shell.kill".into(),
            description: "终止后台 shell。若已退出则无操作（幂等）。".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "shell_id": {
                        "type": "string",
                        "description": "shell.background 返回的 shell_id"
                    }
                },
                "required": ["shell_id"]
            }),
        };
        Self { schema, store }
    }
}

impl Tool for ShellKill {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::Command
    }

    fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &minicoding_core::tool::ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        let store = Arc::clone(&self.store);
        Box::pin(async move {
            let shell_id: String = params
                .get("shell_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("shell_id 缺失".into()))?
                .to_string();
            store.kill(shell_id).await?;
            Ok(ToolResult::ok_text("后台 shell 已终止"))
        })
    }
}
