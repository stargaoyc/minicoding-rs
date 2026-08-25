//! `fs.glob`：glob 模式匹配文件。

use crate::util::{ensure_dir, resolve_path, truncate_output};
use camino::Utf8PathBuf;
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{ListItem, ListKind, RenderIntent, Tool, ToolContext};
use serde::Deserialize;
use serde_json::json;

/// glob 模式匹配文件的只读工具。
pub struct FsGlob {
    schema: ToolSchema,
}

impl FsGlob {
    /// 创建 `fs.glob` 工具实例。
    #[must_use]
    pub fn new() -> Self {
        let schema = ToolSchema {
            name: "fs.glob".to_string(),
            description: "按 glob 模式匹配文件（尊重 .gitignore），返回匹配路径列表。".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "glob 模式（如 \"**/*.rs\"、\"src/*.toml\"）。"
                    },
                    "path": {
                        "type": "string",
                        "description": "搜索根目录（相对路径基于工作目录解析），默认工作目录。"
                    }
                },
                "required": ["pattern"]
            }),
        };
        Self { schema }
    }
}

impl Default for FsGlob {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct GlobInput {
    pattern: String,
    path: Option<String>,
}

impl Tool for FsGlob {
    fn name(&self) -> &'static str {
        "fs.glob"
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::None
    }

    fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        let workdir = ctx.workdir.clone();
        let max_output_bytes = ctx.max_output_bytes;
        Box::pin(async move {
            let args: GlobInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;

            let matcher = globset::Glob::new(&args.pattern)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?
                .compile_matcher();

            let base: Utf8PathBuf = match &args.path {
                Some(p) => resolve_path(&workdir, p)?,
                None => workdir.clone(),
            };
            ensure_dir(&base).await?;

            // PTM-8（2026-08-25 R2 审查）：遍历移入阻塞线程池（与 fs.grep 的
            // T-5 修复同型）——`ignore::WalkBuilder` 同步遍历大目录会长时间
            // 占用 executor 线程饿死其他 task。
            let out = tokio::task::spawn_blocking(move || -> Result<String, ToolError> {
                let mut matched_paths = Vec::new();
                let walker = ignore::WalkBuilder::new(&base).build();
                for entry in walker {
                    let entry =
                        entry.map_err(|e| ToolError::Io(std::io::Error::other(e.to_string())))?;
                    let Some(ft) = entry.file_type() else {
                        continue;
                    };
                    if !ft.is_file() {
                        continue;
                    }
                    let rel = match entry.path().strip_prefix(base.as_std_path()) {
                        Ok(p) => p.to_string_lossy().to_string(),
                        Err(_) => entry.path().to_string_lossy().to_string(),
                    };
                    if matcher.is_match(&rel) {
                        matched_paths.push(rel);
                    }
                }
                Ok(matched_paths.join("\n"))
            })
            .await
            .map_err(|e| ToolError::Exec(format!("glob task join 失败: {e}")))??;

            let (text, truncated) = truncate_output(out, max_output_bytes);
            let bytes = text.len();
            let mut result = ToolResult::ok_text(text);
            result.metadata.truncated = truncated;
            result.metadata.bytes = bytes;
            Ok(result)
        })
    }

    /// 渲染意图（R-05，M-11）：文本路径列表 → 文件列表卡片。
    ///
    /// `fs.glob` 输出为每行一个相对路径的文本；投影为 `List { kind: Files }`
    /// 供前端渲染文件列表（对齐 dsh presentResult 的文件树卡片）。
    fn render_output(&self, result: &ToolResult) -> RenderIntent {
        match &result.content {
            minicoding_core::model::ToolContent::Text(text) => {
                let items = text
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(|l| ListItem {
                        label: l.to_string(),
                        hint: None,
                    })
                    .collect();
                RenderIntent::List {
                    items,
                    kind: ListKind::Files,
                }
            }
            _ => RenderIntent::default_for(result),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
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

    /// 把结果文本按行拆分为路径列表。
    fn lines_of(result: &ToolResult) -> Vec<String> {
        let text = text_of(result);
        if text.is_empty() {
            Vec::new()
        } else {
            text.lines().map(String::from).collect()
        }
    }

    #[tokio::test]
    async fn glob_star_rs_matches_top_level_rs_files() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.rs"), "").expect("write");
        std::fs::write(tmp.path().join("b.rs"), "").expect("write");
        std::fs::write(tmp.path().join("c.txt"), "").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGlob::new();
        let result = tool
            .execute(json!({"pattern": "*.rs", "path": "."}), &ctx)
            .await
            .expect("glob ok");
        assert!(!result.is_error);
        let mut paths = lines_of(&result);
        paths.sort();
        // *.rs 不跨目录边界，只匹配顶层 a.rs, b.rs
        assert_eq!(paths, vec!["a.rs".to_string(), "b.rs".to_string()]);
    }

    #[tokio::test]
    async fn glob_double_star_matches_recursively() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.rs"), "").expect("write");
        std::fs::create_dir(tmp.path().join("sub")).expect("mkdir");
        std::fs::write(tmp.path().join("sub").join("b.rs"), "").expect("write");
        std::fs::write(tmp.path().join("sub").join("c.txt"), "").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGlob::new();
        let result = tool
            .execute(json!({"pattern": "**/*.rs", "path": "."}), &ctx)
            .await
            .expect("glob ok");
        // 规范化路径分隔符：Windows 下 strip_prefix 返回反斜杠，统一为正斜杠以跨平台断言
        let mut paths: Vec<String> = lines_of(&result)
            .iter()
            .map(|p| p.replace('\\', "/"))
            .collect();
        paths.sort();
        // **/*.rs 递归匹配所有 .rs 文件
        assert!(paths.contains(&"a.rs".to_string()));
        assert!(paths.contains(&"sub/b.rs".to_string()));
        // .txt 不匹配
        assert!(!paths.iter().any(|p| p.contains("c.txt")));
    }

    #[tokio::test]
    async fn glob_no_match_returns_empty() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.txt"), "").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGlob::new();
        let result = tool
            .execute(json!({"pattern": "*.rs", "path": "."}), &ctx)
            .await
            .expect("glob ok");
        assert!(!result.is_error);
        assert!(text_of(&result).is_empty());
    }

    #[tokio::test]
    async fn glob_nonexistent_path_returns_not_found() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGlob::new();
        let err = tool
            .execute(json!({"pattern": "*.rs", "path": "no_such_dir"}), &ctx)
            .await
            .unwrap_err();
        // ensure_dir 对不存在目录返回 NotFound
        assert!(
            matches!(err, ToolError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
    }

    #[tokio::test]
    async fn glob_invalid_pattern_returns_invalid_input() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGlob::new();
        // 未闭合的字符类是非法 glob
        let err = tool
            .execute(json!({"pattern": "[unclosed", "path": "."}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn glob_default_path_is_workdir() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.rs"), "").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGlob::new();
        // 省略 path → 默认 workdir
        let result = tool
            .execute(json!({"pattern": "*.rs"}), &ctx)
            .await
            .expect("glob ok");
        assert_eq!(lines_of(&result), vec!["a.rs".to_string()]);
    }

    #[test]
    fn glob_side_effect_is_none() {
        let tool = FsGlob::new();
        assert_eq!(tool.side_effect(), SideEffect::None);
        assert!(tool.is_read_only());
    }

    #[test]
    fn glob_schema_name_correct() {
        let tool = FsGlob::new();
        assert_eq!(tool.name(), "fs.glob");
        assert_eq!(tool.schema().name, "fs.glob");
    }

    #[tokio::test]
    async fn glob_missing_pattern_returns_invalid_input() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGlob::new();
        let err = tool.execute(json!({"path": "."}), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[test]
    fn render_output_projects_text_paths_to_file_list() {
        let tool = FsGlob::new();
        let result = ToolResult::ok_text("src/a.rs\nsrc/b.rs");
        match tool.render_output(&result) {
            RenderIntent::List { items, kind } => {
                assert_eq!(kind, ListKind::Files);
                let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
                assert_eq!(labels, vec!["src/a.rs", "src/b.rs"]);
            }
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn render_output_skips_empty_lines() {
        let tool = FsGlob::new();
        let result = ToolResult::ok_text("a.rs\n\nb.rs\n");
        match tool.render_output(&result) {
            RenderIntent::List { items, .. } => {
                let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
                assert_eq!(labels, vec!["a.rs", "b.rs"]);
            }
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn output_schema_is_none_for_text_tool() {
        // fs.glob 输出为自由文本，不声明 JSON schema（R-05：仅 JSON 工具提供）
        let tool = FsGlob::new();
        assert!(tool.output_schema().is_none());
    }
}
