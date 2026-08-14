//! `fs.write`：整文件覆盖写入。

use crate::fs::journal_helper::record_change;
use crate::util::resolve_path;
use camino::Utf8PathBuf;
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
            let path = match resolve_path(&workdir, &args.path) {
                Ok(p) => p,
                // 父目录不存在时 `resolve_path` 返回 NotFound（util.rs：目标不存在需
                // 规范化父目录）；mkdir -p 语义：先创建父目录再重试解析（模型常写
                // 新目录下的文件，不建目录会失败并触发无谓重试/权限询问）
                Err(ToolError::NotFound(_)) => {
                    let candidate = if std::path::Path::new(&args.path).is_absolute() {
                        Utf8PathBuf::from(&args.path)
                    } else {
                        workdir.join(&args.path)
                    };
                    if let Some(parent) = candidate.parent() {
                        tokio::fs::create_dir_all(parent)
                            .await
                            .map_err(ToolError::Io)?;
                    }
                    resolve_path(&workdir, &args.path)?
                }
                Err(e) => return Err(e),
            };

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
    async fn write_new_file_succeeds() {
        let (tmp, workdir) = make_workdir();
        let file_path = tmp.path().join("new.txt");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsWrite::new();
        let result = tool
            .execute(json!({"path": "new.txt", "content": "hello world"}), &ctx)
            .await
            .expect("write ok");
        assert!(!result.is_error);
        assert!(text_of(&result).contains("wrote 11 bytes"));
        // 文件已创建且内容正确
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn write_overwrite_existing_file_succeeds() {
        let (tmp, workdir) = make_workdir();
        let file_path = tmp.path().join("a.txt");
        std::fs::write(&file_path, "old content").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsWrite::new();
        let result = tool
            .execute(json!({"path": "a.txt", "content": "new content"}), &ctx)
            .await
            .expect("write ok");
        assert!(!result.is_error);
        // 文件被覆盖
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "new content");
    }

    #[tokio::test]
    async fn write_to_nonexistent_subdir_creates_parent_dirs() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir.clone(), "test".to_string());
        let tool = FsWrite::new();
        // mkdir -p 语义：父目录不存在时先创建（resolve_path 对 "nodir/file.txt"
        // 会先返回 NotFound，write.rs 建目录后重试解析）
        let result = tool
            .execute(json!({"path": "nodir/deep/file.txt", "content": "x"}), &ctx)
            .await
            .expect("should create parent dirs");
        assert!(text_of(&result).contains("wrote"));
        assert_eq!(
            std::fs::read_to_string(workdir.join("nodir/deep/file.txt")).unwrap(),
            "x"
        );
    }

    #[tokio::test]
    async fn write_path_escaped_rejected() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsWrite::new();
        let err = tool
            .execute(json!({"path": "../escape.txt", "content": "x"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PathEscaped(_)));
    }

    #[tokio::test]
    async fn write_empty_content_succeeds() {
        let (tmp, workdir) = make_workdir();
        let file_path = tmp.path().join("empty.txt");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsWrite::new();
        let result = tool
            .execute(json!({"path": "empty.txt", "content": ""}), &ctx)
            .await
            .expect("write ok");
        assert!(!result.is_error);
        assert!(text_of(&result).contains("wrote 0 bytes"));
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "");
    }

    #[tokio::test]
    async fn write_to_existing_subdir_succeeds() {
        let (tmp, workdir) = make_workdir();
        std::fs::create_dir(tmp.path().join("sub")).expect("mkdir");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsWrite::new();
        let result = tool
            .execute(json!({"path": "sub/a.txt", "content": "data"}), &ctx)
            .await
            .expect("write ok");
        assert!(!result.is_error);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("sub").join("a.txt")).unwrap(),
            "data"
        );
    }

    #[test]
    fn write_side_effect_is_file_write() {
        let tool = FsWrite::new();
        assert_eq!(tool.side_effect(), SideEffect::FileWrite);
        assert!(!tool.is_read_only());
    }

    #[test]
    fn write_schema_name_correct() {
        let tool = FsWrite::new();
        assert_eq!(tool.name(), "fs.write");
        assert_eq!(tool.schema().name, "fs.write");
    }

    #[tokio::test]
    async fn write_missing_fields_returns_invalid_input() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsWrite::new();
        let err = tool
            .execute(json!({"path": "a.txt"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn write_multibyte_content_byte_count_correct() {
        let (tmp, workdir) = make_workdir();
        let file_path = tmp.path().join("zh.txt");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsWrite::new();
        // 中文每个字符 3 字节，"你好" = 6 字节
        let result = tool
            .execute(json!({"path": "zh.txt", "content": "你好"}), &ctx)
            .await
            .expect("write ok");
        assert!(text_of(&result).contains("wrote 6 bytes"));
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "你好");
    }
}
