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
    async fn edit_simple_replace_succeeds() {
        let (tmp, workdir) = make_workdir();
        let file_path = tmp.path().join("a.txt");
        std::fs::write(&file_path, "hello world").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsEdit::new();
        let result = tool
            .execute(
                json!({"path": "a.txt", "old_string": "hello", "new_string": "goodbye"}),
                &ctx,
            )
            .await
            .expect("edit ok");
        assert!(!result.is_error);
        assert!(text_of(&result).contains("edited a.txt"));
        // 文件内容已替换
        assert_eq!(
            std::fs::read_to_string(&file_path).unwrap(),
            "goodbye world"
        );
    }

    #[tokio::test]
    async fn edit_old_string_not_found_returns_error_result() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.txt"), "hello world").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsEdit::new();
        let result = tool
            .execute(
                json!({"path": "a.txt", "old_string": "nonexistent", "new_string": "x"}),
                &ctx,
            )
            .await
            .expect("soft error returned as ok result");
        // 软错误：is_error=true，文件未被修改
        assert!(result.is_error);
        assert!(text_of(&result).contains("not found"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "hello world"
        );
    }

    #[tokio::test]
    async fn edit_multiple_matches_returns_not_unique_error() {
        let (tmp, workdir) = make_workdir();
        // "foo" 出现两次 → 非唯一
        std::fs::write(tmp.path().join("a.txt"), "foo bar foo").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsEdit::new();
        let result = tool
            .execute(
                json!({"path": "a.txt", "old_string": "foo", "new_string": "x"}),
                &ctx,
            )
            .await
            .expect("soft error");
        assert!(result.is_error);
        assert!(text_of(&result).contains("not unique"));
        // 文件未被修改
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "foo bar foo"
        );
    }

    #[tokio::test]
    async fn edit_nonexistent_file_returns_not_found() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsEdit::new();
        let err = tool
            .execute(
                json!({"path": "no_such.txt", "old_string": "a", "new_string": "b"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn edit_same_old_new_returns_nothing_to_do() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.txt"), "hello").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsEdit::new();
        let result = tool
            .execute(
                json!({"path": "a.txt", "old_string": "hello", "new_string": "hello"}),
                &ctx,
            )
            .await
            .expect("ok");
        // 新旧相同 → 软错误（nothing to do），不执行 IO
        assert!(result.is_error);
        assert!(text_of(&result).contains("nothing to do"));
    }

    #[tokio::test]
    async fn edit_path_escaped_rejected() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsEdit::new();
        let err = tool
            .execute(
                json!({"path": "../escape.txt", "old_string": "a", "new_string": "b"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PathEscaped(_)));
    }

    #[tokio::test]
    async fn edit_unique_match_with_context_replaces_correctly() {
        let (tmp, workdir) = make_workdir();
        // 两次 "foo"，但 "foo bar" 唯一
        std::fs::write(tmp.path().join("a.txt"), "foo bar\nfoo baz").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsEdit::new();
        let result = tool
            .execute(
                json!({"path": "a.txt", "old_string": "foo bar", "new_string": "X bar"}),
                &ctx,
            )
            .await
            .expect("edit ok");
        assert!(!result.is_error);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "X bar\nfoo baz"
        );
    }

    #[test]
    fn edit_side_effect_is_file_write() {
        let tool = FsEdit::new();
        assert_eq!(tool.side_effect(), SideEffect::FileWrite);
        assert!(!tool.is_read_only());
    }

    #[test]
    fn edit_schema_name_correct() {
        let tool = FsEdit::new();
        assert_eq!(tool.name(), "fs.edit");
        assert_eq!(tool.schema().name, "fs.edit");
    }

    #[tokio::test]
    async fn edit_missing_fields_returns_invalid_input() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsEdit::new();
        let err = tool
            .execute(json!({"path": "a.txt"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }
}
