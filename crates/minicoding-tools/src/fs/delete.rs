//! `fs.delete`：删除文件。

use crate::fs::journal_helper::record_change;
use crate::util::resolve_path;
use minicoding_core::journal::FileChange;
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{RenderIntent, Tool, ToolContext};
use serde::Deserialize;
use serde_json::json;

/// 删除文件的工具。
pub struct FsDelete {
    schema: ToolSchema,
}

impl FsDelete {
    /// 创建 `fs.delete` 工具实例。
    #[must_use]
    pub fn new() -> Self {
        let schema = ToolSchema {
            name: "fs.delete".to_string(),
            description: "删除文件（路径不可越界，相对路径基于工作目录解析；文件不存在则报错）。"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "文件路径（相对路径基于工作目录解析）。"
                    }
                },
                "required": ["path"]
            }),
        };
        Self { schema }
    }
}

impl Default for FsDelete {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct DeleteInput {
    path: String,
}

impl Tool for FsDelete {
    fn name(&self) -> &'static str {
        "fs.delete"
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
            let args: DeleteInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
            let path = resolve_path(&workdir, &args.path)?;

            // 读取待删除文件内容用于 journal 撤销恢复（C-28）
            let content = tokio::fs::read(&path).await.map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => ToolError::NotFound(args.path.clone()),
                _ => ToolError::Io(e),
            })?;

            tokio::fs::remove_file(&path).await.map_err(ToolError::Io)?;

            // 记入 journal（若注入；C-28）
            record_change(
                journal.as_ref(),
                FileChange::Deleted {
                    path: path.clone(),
                    content,
                },
            )
            .await;

            Ok(ToolResult::ok_text(format!("deleted {}", args.path)))
        })
    }

    /// 渲染意图（R-05，M-11）：删除确认消息，文本直出。
    fn render_output(&self, result: &ToolResult) -> RenderIntent {
        RenderIntent::default_for(result)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use camino::Utf8PathBuf;
    use minicoding_core::model::{SideEffect, ToolContent, ToolError};
    use minicoding_core::tool::Tool;
    use tempfile::TempDir;

    /// 创建临时 workdir 并返回 `(TempDir, 规范化后的 workdir 路径)`。
    fn make_workdir() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().expect("create tempdir");
        let canon = Utf8PathBuf::from_path_buf(tmp.path().canonicalize().expect("canonicalize"))
            .expect("utf-8 path");
        (tmp, canon)
    }

    /// 从 `ToolResult` 提取文本内容。
    fn text_of(result: &ToolResult) -> &str {
        match &result.content {
            ToolContent::Text(t) => t,
            _ => panic!("expected text content"),
        }
    }

    #[tokio::test]
    async fn delete_file_succeeds_and_file_removed() {
        let (tmp, workdir) = make_workdir();
        let file_path = tmp.path().join("target.txt");
        std::fs::write(&file_path, "data").expect("write");
        assert!(file_path.exists());
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsDelete::new();
        let result = tool
            .execute(json!({"path": "target.txt"}), &ctx)
            .await
            .expect("delete ok");
        assert!(!result.is_error);
        assert!(text_of(&result).contains("deleted target.txt"));
        // 文件已被删除
        assert!(!file_path.exists());
    }

    #[tokio::test]
    async fn delete_nonexistent_file_returns_not_found() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsDelete::new();
        let err = tool
            .execute(json!({"path": "no_such_file.txt"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_path_escaped_rejected() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsDelete::new();
        let err = tool
            .execute(json!({"path": "../escape.txt"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PathEscaped(_)));
    }

    #[test]
    fn delete_side_effect_is_file_write() {
        let tool = FsDelete::new();
        assert_eq!(tool.side_effect(), SideEffect::FileWrite);
        // 写入副作用 → 非只读
        assert!(!tool.is_read_only());
    }

    #[test]
    fn delete_schema_name_correct() {
        let tool = FsDelete::new();
        assert_eq!(tool.name(), "fs.delete");
        assert_eq!(tool.schema().name, "fs.delete");
    }

    #[tokio::test]
    async fn delete_missing_path_field_returns_invalid_input() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsDelete::new();
        let err = tool.execute(json!({}), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn delete_directory_returns_io_error() {
        let (tmp, workdir) = make_workdir();
        // FsDelete 调用 read（文件读取）+ remove_file；对目录 read 会失败
        std::fs::create_dir(tmp.path().join("adir")).expect("mkdir");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsDelete::new();
        // read 目录 → Io 错误（IsADirectory / 其他 IO）
        let err = tool
            .execute(json!({"path": "adir"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Io(_)));
    }
}
