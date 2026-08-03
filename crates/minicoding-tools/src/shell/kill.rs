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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::InMemoryBackgroundShellStore;
    use minicoding_core::tool::ToolContext;

    fn make_store() -> Arc<dyn BackgroundShellStore> {
        Arc::new(InMemoryBackgroundShellStore::new())
    }

    fn make_ctx() -> ToolContext {
        ToolContext::new("/tmp".into(), "test".to_string())
    }

    #[tokio::test]
    async fn kill_missing_shell_id_returns_invalid_input() {
        let tool = ShellKill::new(make_store());
        let result = tool.execute(serde_json::json!({}), &make_ctx()).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn kill_nonexistent_returns_not_found() {
        let tool = ShellKill::new(make_store());
        let result = tool
            .execute(serde_json::json!({"shell_id": "nonexistent"}), &make_ctx())
            .await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolError::NotFound(_)));
    }

    #[test]
    fn kill_side_effect_is_command() {
        let tool = ShellKill::new(make_store());
        assert_eq!(tool.side_effect(), SideEffect::Command);
    }

    #[test]
    fn kill_schema_has_correct_name() {
        let tool = ShellKill::new(make_store());
        assert_eq!(tool.name(), "shell.kill");
    }
}
