//! `fs.multiedit`：同文件多次顺序替换（原子性）。

use crate::util::resolve_path;
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{Tool, ToolContext};
use serde::Deserialize;
use serde_json::json;

/// 同文件多次顺序替换的工具（原子性：中间失败不写回）。
pub struct FsMultiEdit {
    schema: ToolSchema,
}

impl FsMultiEdit {
    /// 创建 `fs.multiedit` 工具实例。
    #[must_use]
    pub fn new() -> Self {
        let schema = ToolSchema {
            name: "fs.multiedit".to_string(),
            description: "对同一文件按顺序执行多次精确字符串替换（每个替换做唯一性校验），全部成功才写回（原子性，任一失败则文件保持原状）。"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "文件路径（相对路径基于工作目录解析）。"
                    },
                    "edits": {
                        "type": "array",
                        "description": "按顺序执行的替换列表（前一个替换的结果作为下一个替换的输入）。",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": {
                                    "type": "string",
                                    "description": "待替换的精确字符串（必须在当前内容中唯一匹配）。"
                                },
                                "new_string": {
                                    "type": "string",
                                    "description": "替换后的新字符串。"
                                }
                            },
                            "required": ["old_string", "new_string"]
                        }
                    }
                },
                "required": ["path", "edits"]
            }),
        };
        Self { schema }
    }
}

impl Default for FsMultiEdit {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct MultiEditInput {
    path: String,
    edits: Vec<Edit>,
}

#[derive(Deserialize)]
struct Edit {
    old_string: String,
    new_string: String,
}

impl Tool for FsMultiEdit {
    fn name(&self) -> &'static str {
        "fs.multiedit"
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
        Box::pin(async move {
            let args: MultiEditInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;

            let path = resolve_path(&workdir, &args.path)?;

            // 原子性：所有替换在内存中进行，任一失败直接返回且不写回，
            // 文件保持原状（磁盘上的内容始终未被修改）。
            let mut content =
                tokio::fs::read_to_string(&path)
                    .await
                    .map_err(|e| match e.kind() {
                        std::io::ErrorKind::NotFound => ToolError::NotFound(args.path.clone()),
                        _ => ToolError::Io(e),
                    })?;

            for (i, edit) in args.edits.iter().enumerate() {
                if edit.old_string == edit.new_string {
                    return Ok(ToolResult::err_text(format!(
                        "edit #{}: old_string equals new_string: nothing to do",
                        i + 1
                    )));
                }
                let count = content.matches(&edit.old_string).count();
                if count == 0 {
                    return Ok(ToolResult::err_text(format!(
                        "edit #{}: old_string not found in {}",
                        i + 1,
                        args.path
                    )));
                }
                if count > 1 {
                    return Ok(ToolResult::err_text(format!(
                        "edit #{}: old_string is not unique ({} matches) in {}: provide more context",
                        i + 1,
                        count,
                        args.path
                    )));
                }
                content = content.replacen(&edit.old_string, &edit.new_string, 1);
            }

            // 全部替换成功，原子写回
            tokio::fs::write(&path, content.as_bytes())
                .await
                .map_err(ToolError::Io)?;

            Ok(ToolResult::ok_text(format!(
                "applied {} edit(s) to {}",
                args.edits.len(),
                args.path
            )))
        })
    }
}
