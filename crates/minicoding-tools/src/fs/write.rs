//! `fs.write`：整文件覆盖写入。

use crate::fs::journal_helper::record_change;
use crate::util::resolve_path;
use minicoding_core::journal::FileChange;
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{Tool, ToolContext};
use serde::Deserialize;
use serde_json::json;

/// 整文件覆盖写入的工具。
pub struct FsWrite {
    schema: ToolSchema,
}

impl FsWrite {
    /// 创建 `fs.write` 工具实例。
    #[must_use]
    pub fn new() -> Self {
        let schema = ToolSchema {
            name: "fs.write".to_string(),
            description:
                "整文件覆盖写入（路径不可越界，相对路径基于工作目录解析；同名文件会被覆盖）。"
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "文件路径（相对路径基于工作目录解析）。"
                    },
                    "content": {
                        "type": "string",
                        "description": "要写入的完整内容（覆盖原文件）。"
                    }
                },
                "required": ["path", "content"]
            }),
        };
        Self { schema }
    }
}

impl Default for FsWrite {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct WriteInput {
    path: String,
    content: String,
}

impl Tool for FsWrite {
    fn name(&self) -> &'static str {
        "fs.write"
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::FileWrite
    }

    fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        let workdir = ctx.workdir.clone();
        let journal = ctx.journal.clone();
        Box::pin(async move {
            let args: WriteInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
            let path = resolve_path(&workdir, &args.path)?;

            // 读取 before 内容（若存在）用于 journal 撤销恢复（C-28）
            let before = tokio::fs::read(&path).await.ok();

            let after = args.content.clone().into_bytes();
            tokio::fs::write(&path, args.content.as_bytes())
                .await
                .map_err(ToolError::Io)?;

            // 记入 journal（若注入；file-undo feature 启用时由 Runtime 注入）
            let change = match &before {
                None => FileChange::Created {
                    path: path.clone(),
                    content: after.clone(),
                },
                Some(b) => FileChange::Written {
                    path: path.clone(),
                    before: Some(b.clone()),
                    after: after.clone(),
                },
            };
            record_change(journal.as_ref(), change).await;

            Ok(ToolResult::ok_text(format!(
                "wrote {} bytes to {}",
                args.content.len(),
                args.path
            )))
        })
    }
}
