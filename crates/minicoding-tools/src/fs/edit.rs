//! `fs.edit`：精确字符串替换 + 唯一性校验。

use crate::fs::journal_helper::record_change;
use crate::util::resolve_path;
use minicoding_core::journal::FileChange;
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{Tool, ToolContext};
use serde::Deserialize;
use serde_json::json;

/// 精确字符串替换的工具（带唯一性校验）。
pub struct FsEdit {
    schema: ToolSchema,
}

impl FsEdit {
    /// 创建 `fs.edit` 工具实例。
    #[must_use]
    pub fn new() -> Self {
        let schema = ToolSchema {
            name: "fs.edit".to_string(),
            description: "精确字符串替换：在文件中查找 old_string 并替换为 new_string，要求 old_string 在文件中唯一匹配（提供足够上下文以消除歧义）。"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "文件路径（相对路径基于工作目录解析）。"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "待替换的精确字符串（必须在文件中唯一匹配）。"
                    },
                    "new_string": {
                        "type": "string",
                        "description": "替换后的新字符串。"
                    }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        };
        Self { schema }
    }
}

impl Default for FsEdit {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct EditInput {
    path: String,
    old_string: String,
    new_string: String,
}

impl Tool for FsEdit {
    fn name(&self) -> &'static str {
        "fs.edit"
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
            let args: EditInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;

            // 无意义操作：新旧字符串相同，提前拒绝避免无谓 IO
            if args.old_string == args.new_string {
                return Ok(ToolResult::err_text(
                    "old_string equals new_string: nothing to do",
                ));
            }

            let path = resolve_path(&workdir, &args.path)?;

            let content = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => ToolError::NotFound(args.path.clone()),
                    _ => ToolError::Io(e),
                })?;

            let count = content.matches(&args.old_string).count();
            if count == 0 {
                return Ok(ToolResult::err_text(format!(
                    "old_string not found in {}",
                    args.path
                )));
            }
            if count > 1 {
                return Ok(ToolResult::err_text(format!(
                    "old_string is not unique ({} matches) in {}: provide more context",
                    count, args.path
                )));
            }

            let before_bytes = content.as_bytes().to_vec();
            let new_content = content.replacen(&args.old_string, &args.new_string, 1);
            let after_bytes = new_content.as_bytes().to_vec();
            tokio::fs::write(&path, new_content.as_bytes())
                .await
                .map_err(ToolError::Io)?;

            // 记入 journal（若注入；C-28）
            record_change(
                journal.as_ref(),
                FileChange::Edited {
                    path: path.clone(),
                    before: before_bytes,
                    after: after_bytes,
                },
            )
            .await;

            Ok(ToolResult::ok_text(format!("edited {}", args.path)))
        })
    }
}
