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
    async fn multiedit_multiple_edits_applied_atomically() {
        let (tmp, workdir) = make_workdir();
        let file_path = tmp.path().join("a.txt");
        std::fs::write(&file_path, "alpha beta gamma").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsMultiEdit::new();
        let result = tool
            .execute(
                json!({
                    "path": "a.txt",
                    "edits": [
                        {"old_string": "alpha", "new_string": "A"},
                        {"old_string": "beta", "new_string": "B"},
                        {"old_string": "gamma", "new_string": "G"}
                    ]
                }),
                &ctx,
            )
            .await
            .expect("multiedit ok");
        assert!(!result.is_error);
        assert!(text_of(&result).contains("applied 3 edit(s)"));
        // 三个替换全部应用
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "A B G");
    }

    #[tokio::test]
    async fn multiedit_one_edit_fails_no_write_back() {
        let (tmp, workdir) = make_workdir();
        let file_path = tmp.path().join("a.txt");
        let original = "alpha beta gamma";
        std::fs::write(&file_path, original).expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsMultiEdit::new();
        // 第一个替换成功，第二个 old_string 不存在 → 软错误，文件保持原状（原子性）
        let result = tool
            .execute(
                json!({
                    "path": "a.txt",
                    "edits": [
                        {"old_string": "alpha", "new_string": "A"},
                        {"old_string": "nonexistent", "new_string": "X"}
                    ]
                }),
                &ctx,
            )
            .await
            .expect("soft error");
        assert!(result.is_error);
        assert!(text_of(&result).contains("edit #2"));
        assert!(text_of(&result).contains("not found"));
        // 文件未被修改（原子性：任一失败不写回）
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), original);
    }

    #[tokio::test]
    async fn multiedit_empty_edits_writes_back_unchanged() {
        let (tmp, workdir) = make_workdir();
        let file_path = tmp.path().join("a.txt");
        std::fs::write(&file_path, "hello").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsMultiEdit::new();
        // 空编辑列表 → 循环不执行，写回相同内容，返回 applied 0 edit(s)
        let result = tool
            .execute(json!({"path": "a.txt", "edits": []}), &ctx)
            .await
            .expect("ok");
        assert!(!result.is_error);
        assert!(text_of(&result).contains("applied 0 edit(s)"));
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "hello");
    }

    #[tokio::test]
    async fn multiedit_non_unique_returns_error_no_write() {
        let (tmp, workdir) = make_workdir();
        let file_path = tmp.path().join("a.txt");
        let original = "foo foo";
        std::fs::write(&file_path, original).expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsMultiEdit::new();
        let result = tool
            .execute(
                json!({
                    "path": "a.txt",
                    "edits": [{"old_string": "foo", "new_string": "x"}]
                }),
                &ctx,
            )
            .await
            .expect("soft error");
        assert!(result.is_error);
        assert!(text_of(&result).contains("not unique"));
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), original);
    }

    #[tokio::test]
    async fn multiedit_nonexistent_file_returns_not_found() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsMultiEdit::new();
        let err = tool
            .execute(
                json!({"path": "no_such.txt", "edits": [{"old_string": "a", "new_string": "b"}]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn multiedit_same_old_new_returns_error() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.txt"), "hello").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsMultiEdit::new();
        let result = tool
            .execute(
                json!({
                    "path": "a.txt",
                    "edits": [{"old_string": "hello", "new_string": "hello"}]
                }),
                &ctx,
            )
            .await
            .expect("soft error");
        assert!(result.is_error);
        assert!(text_of(&result).contains("edit #1"));
        assert!(text_of(&result).contains("nothing to do"));
    }

    #[tokio::test]
    async fn multiedit_path_escaped_rejected() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsMultiEdit::new();
        let err = tool
            .execute(
                json!({"path": "../escape.txt", "edits": [{"old_string": "a", "new_string": "b"}]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PathEscaped(_)));
    }

    #[tokio::test]
    async fn multiedit_sequential_edits_use_prior_result() {
        let (tmp, workdir) = make_workdir();
        let file_path = tmp.path().join("a.txt");
        std::fs::write(&file_path, "a").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsMultiEdit::new();
        // 前一个替换的结果作为下一个替换的输入：a→b→c→d
        let result = tool
            .execute(
                json!({
                    "path": "a.txt",
                    "edits": [
                        {"old_string": "a", "new_string": "b"},
                        {"old_string": "b", "new_string": "c"},
                        {"old_string": "c", "new_string": "d"}
                    ]
                }),
                &ctx,
            )
            .await
            .expect("ok");
        assert!(!result.is_error);
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "d");
    }

    #[test]
    fn multiedit_side_effect_is_file_write() {
        let tool = FsMultiEdit::new();
        assert_eq!(tool.side_effect(), SideEffect::FileWrite);
        assert!(!tool.is_read_only());
    }

    #[test]
    fn multiedit_schema_name_correct() {
        let tool = FsMultiEdit::new();
        assert_eq!(tool.name(), "fs.multiedit");
        assert_eq!(tool.schema().name, "fs.multiedit");
    }

    #[tokio::test]
    async fn multiedit_missing_edits_returns_invalid_input() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsMultiEdit::new();
        let err = tool
            .execute(json!({"path": "a.txt"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }
}
