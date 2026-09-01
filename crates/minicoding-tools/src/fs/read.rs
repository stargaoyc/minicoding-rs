//! `fs.read`：读取文件内容（支持行范围）。
//!
//! 对敏感文件（`.env`、`credentials`、`*.pem` 等）自动应用脱敏（C-04，T-M4-11），
//! 把 `API_KEY=xxx` / `password=xxx` 等字段值替换为 `***`，避免回灌 LLM 上下文。

use crate::util::{resolve_path, truncate_output};
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{RenderIntent, Tool, ToolContext};
use serde::Deserialize;
use serde_json::json;

/// 读取文件内容的只读工具。
pub struct FsRead {
    schema: ToolSchema,
}

impl FsRead {
    /// 创建 `fs.read` 工具实例。
    #[must_use]
    pub fn new() -> Self {
        let schema = ToolSchema {
            name: "fs.read".to_string(),
            description:
                "读取文件内容，支持行范围（offset 为起始行索引 0-based，limit 为返回行数）。"
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "文件路径（相对路径基于工作目录解析）。"
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "起始行索引（0-based），默认 0。"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "返回的最大行数，默认全部。"
                    }
                },
                "required": ["path"]
            }),
        };
        Self { schema }
    }
}

impl Default for FsRead {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct ReadInput {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

impl Tool for FsRead {
    fn name(&self) -> &'static str {
        "fs.read"
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
        // CORE-2（2026-08-25 R2 审查）：读取上限来自 RuntimeConfig.tools 接线
        let max_read_bytes = ctx.max_read_bytes;
        Box::pin(async move {
            let args: ReadInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
            let path = resolve_path(&workdir, &args.path)?;

            // PTM-13（2026-08-25 R2 审查）：先查文件大小再读——此前全量
            // read_to_string 入内存后才截断，超大文本（如误指二进制/日志）
            // 可打爆内存。超限直接报错并提示 offset/limit 分段读取。
            if let Ok(meta) = tokio::fs::metadata(&path).await
                && meta.len() > max_read_bytes as u64
            {
                return Err(ToolError::Io(std::io::Error::other(format!(
                    "file too large: {} bytes > limit {} bytes; \
                     use offset/limit to read in segments",
                    meta.len(),
                    max_read_bytes
                ))));
            }

            let bytes = tokio::fs::read(&path).await.map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => ToolError::NotFound(args.path.clone()),
                _ => ToolError::Io(e),
            })?;

            // R10 可读性修复：二进制文件（.crate/.tar.gz/.so 等）直接 read_to_string
            // 会得到乱码字节流（无效 UTF-8 被替换为 U+FFFD），此前整屏乱码。改为
            // 检测并返回明确提示（带文件大小），不再回灌乱码内容（C-07 输出边界）。
            let content = match std::str::from_utf8(&bytes) {
                Ok(s) => s.to_string(),
                Err(_) => {
                    return Ok(ToolResult::err_text(format!(
                        "文件是二进制（非 UTF-8 文本，{} 字节）：无法直接读取，\
                         请改用 shell 命令（如 `file`/`xxd`）分析",
                        bytes.len()
                    )));
                }
            };

            let lines: Vec<&str> = content.lines().collect();
            let offset = args.offset.unwrap_or(0);
            let limit = args.limit.unwrap_or(lines.len());

            let out: String = lines
                .iter()
                .skip(offset)
                .take(limit)
                .copied()
                .collect::<Vec<_>>()
                .join("\n");

            // 敏感文件脱敏（C-04，T-M4-11）：.env / credentials / *.pem 等
            let out = if is_sensitive_path(&path) {
                let redacted = minicoding_policy::redact(&out);
                tracing::debug!(
                    path = %path,
                    "fs.read 应用脱敏（敏感文件）"
                );
                redacted
            } else {
                out
            };

            let (text, truncated) = truncate_output(out, max_output_bytes);
            let bytes = text.len();
            let mut result = ToolResult::ok_text(text);
            result.metadata.truncated = truncated;
            result.metadata.bytes = bytes;
            Ok(result)
        })
    }

    /// 渲染意图（R-05，M-11）：文件内容 → 代码片段（未知语言）。不提供
    /// `output_schema`（自由文本，R-05：仅 JSON 输出工具提供）。
    fn render_output(&self, result: &ToolResult) -> RenderIntent {
        match &result.content {
            minicoding_core::model::ToolContent::Text(text) => RenderIntent::Code {
                lang: None,
                content: text.clone(),
            },
            _ => RenderIntent::default_for(result),
        }
    }
}

/// 判断路径是否为敏感文件（应脱敏）——委托 `fs::is_sensitive_path`（R10-12 共享）。
fn is_sensitive_path(path: &camino::Utf8Path) -> bool {
    crate::fs::is_sensitive_path(path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use camino::Utf8Path;

    #[test]
    fn env_file_is_sensitive() {
        assert!(is_sensitive_path(Utf8Path::new(".env")));
        assert!(is_sensitive_path(Utf8Path::new("/home/user/.env")));
        assert!(is_sensitive_path(Utf8Path::new("project/.env.local")));
    }

    #[test]
    fn credentials_file_is_sensitive() {
        assert!(is_sensitive_path(Utf8Path::new("credentials")));
        assert!(is_sensitive_path(Utf8Path::new(
            "/home/user/.minicoding/credentials"
        )));
        assert!(is_sensitive_path(Utf8Path::new("creds")));
    }

    #[test]
    fn pem_key_files_are_sensitive() {
        assert!(is_sensitive_path(Utf8Path::new("server.pem")));
        assert!(is_sensitive_path(Utf8Path::new("id_rsa.key")));
        assert!(is_sensitive_path(Utf8Path::new("cert.pfx")));
        assert!(is_sensitive_path(Utf8Path::new("keystore.p12")));
    }

    #[test]
    fn keyword_files_are_sensitive() {
        assert!(is_sensitive_path(Utf8Path::new("my_secret.toml")));
        assert!(is_sensitive_path(Utf8Path::new("password.txt")));
        assert!(is_sensitive_path(Utf8Path::new("api_token.json")));
        // 大小写不敏感
        assert!(is_sensitive_path(Utf8Path::new("SECRET_CONFIG")));
    }

    #[test]
    fn normal_files_not_sensitive() {
        assert!(!is_sensitive_path(Utf8Path::new("README.md")));
        assert!(!is_sensitive_path(Utf8Path::new("src/main.rs")));
        assert!(!is_sensitive_path(Utf8Path::new("Cargo.toml")));
        assert!(!is_sensitive_path(Utf8Path::new("config.yaml")));
    }

    // `fs.read` execute 路径测试（覆盖率补全）。
    use minicoding_core::model::ToolContent;
    use minicoding_core::tool::ToolContext;
    use tempfile::tempdir;

    fn make_ctx(workdir: &camino::Utf8Path) -> ToolContext {
        ToolContext::new(workdir.to_owned(), "test-session".to_string())
    }

    /// 提取 `ToolResult` 文本内容（测试辅助）。
    fn result_text(result: &ToolResult) -> &str {
        match &result.content {
            ToolContent::Text(t) => t,
            _ => "",
        }
    }

    #[tokio::test]
    async fn execute_reads_file_content() {
        let dir = tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        let file_path = workdir.join("test.txt");
        tokio::fs::write(file_path.as_std_path(), "line1\nline2\nline3\n")
            .await
            .unwrap();

        let tool = FsRead::new();
        let input = serde_json::json!({"path": "test.txt"});
        let result = tool.execute(input, &make_ctx(&workdir)).await.unwrap();
        assert!(!result.is_error);
        let text = result_text(&result);
        assert!(text.contains("line1"));
        assert!(text.contains("line2"));
        assert!(text.contains("line3"));
    }

    #[tokio::test]
    async fn execute_reads_absolute_path() {
        let dir = tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        let file_path = workdir.join("abs.txt");
        tokio::fs::write(file_path.as_std_path(), "absolute path content")
            .await
            .unwrap();

        let tool = FsRead::new();
        let input = serde_json::json!({"path": file_path.as_str()});
        let result = tool.execute(input, &make_ctx(&workdir)).await.unwrap();
        assert!(result_text(&result).contains("absolute path content"));
    }

    #[tokio::test]
    async fn execute_returns_not_found_for_missing_file() {
        let dir = tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();

        let tool = FsRead::new();
        let input = serde_json::json!({"path": "does_not_exist.txt"});
        let result = tool.execute(input, &make_ctx(&workdir)).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn execute_returns_invalid_input_for_missing_path_field() {
        let dir = tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();

        let tool = FsRead::new();
        let input = serde_json::json!({"offset": 0});
        let result = tool.execute(input, &make_ctx(&workdir)).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn execute_with_offset_and_limit_returns_subset() {
        let dir = tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        let file_path = workdir.join("lines.txt");
        let content = "l0\nl1\nl2\nl3\nl4\nl5\n";
        tokio::fs::write(file_path.as_std_path(), content)
            .await
            .unwrap();

        let tool = FsRead::new();
        // 从第 2 行（0-based offset=2）取 2 行
        let input = serde_json::json!({"path": "lines.txt", "offset": 2, "limit": 2});
        let result = tool.execute(input, &make_ctx(&workdir)).await.unwrap();
        let text = result_text(&result);
        assert!(text.contains("l2"));
        assert!(text.contains("l3"));
        assert!(!text.contains("l0"));
        assert!(!text.contains("l4"));
    }

    #[tokio::test]
    async fn execute_with_offset_beyond_end_returns_empty() {
        let dir = tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        let file_path = workdir.join("small.txt");
        tokio::fs::write(file_path.as_std_path(), "only one line\n")
            .await
            .unwrap();

        let tool = FsRead::new();
        // offset 远超行数
        let input = serde_json::json!({"path": "small.txt", "offset": 100});
        let result = tool.execute(input, &make_ctx(&workdir)).await.unwrap();
        assert_eq!(result_text(&result), "");
    }

    #[tokio::test]
    async fn execute_redacts_sensitive_env_file() {
        let dir = tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        let file_path = workdir.join(".env");
        tokio::fs::write(
            file_path.as_std_path(),
            "API_KEY=sk-secret-12345\nPASSWORD=mypass\n",
        )
        .await
        .unwrap();

        let tool = FsRead::new();
        let input = serde_json::json!({"path": ".env"});
        let result = tool.execute(input, &make_ctx(&workdir)).await.unwrap();
        let text = result_text(&result);
        // 敏感字段值应被脱敏（不直接出现在输出中）
        assert!(!text.contains("sk-secret-12345"));
        assert!(!text.contains("mypass"));
    }

    #[tokio::test]
    async fn execute_truncates_large_output() {
        let dir = tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        let file_path = workdir.join("large.txt");
        // 写入远超 max_output_bytes 的内容
        let large_content = "x".repeat(10 * 1024);
        tokio::fs::write(file_path.as_std_path(), &large_content)
            .await
            .unwrap();

        let mut ctx = make_ctx(&workdir);
        ctx.max_output_bytes = 1024; // 1KB 上限
        let tool = FsRead::new();
        let input = serde_json::json!({"path": "large.txt"});
        let result = tool.execute(input, &ctx).await.unwrap();
        // 应被截断（truncated 标志为 true，且文本长度不超过上限）
        assert!(result.metadata.truncated, "should be truncated");
        assert!(result_text(&result).len() <= 1024);
    }

    #[tokio::test]
    async fn execute_metadata_includes_byte_count() {
        let dir = tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        let file_path = workdir.join("bytes.txt");
        tokio::fs::write(file_path.as_std_path(), "hello")
            .await
            .unwrap();

        let tool = FsRead::new();
        let input = serde_json::json!({"path": "bytes.txt"});
        let result = tool.execute(input, &make_ctx(&workdir)).await.unwrap();
        assert_eq!(result.metadata.bytes, 5); // "hello" = 5 bytes
        assert!(!result.metadata.truncated);
    }

    #[test]
    fn fs_read_default_equals_new() {
        let a = FsRead::new();
        let b = FsRead::default();
        assert_eq!(a.name(), b.name());
        assert_eq!(a.schema().name, b.schema().name);
    }

    #[test]
    fn fs_read_side_effect_is_none() {
        let tool = FsRead::new();
        assert_eq!(tool.side_effect(), SideEffect::None);
        assert!(tool.is_read_only());
    }

    #[test]
    fn render_output_projects_text_to_code() {
        // R-05（M-11）：fs.read 文本内容 → Code 渲染（灵感语言未知）。
        let tool = FsRead::new();
        let result = ToolResult::ok_text("fn main() {\n}");
        match tool.render_output(&result) {
            RenderIntent::Code { lang, content } => {
                assert_eq!(lang, None);
                assert_eq!(content, "fn main() {\n}");
            }
            other => panic!("expected Code, got {other:?}"),
        }
    }

    #[test]
    fn render_output_non_text_falls_back_to_default() {
        // 非文本内容（如 JSON）走默认路径，保持行为与 M-11 前一致。
        let tool = FsRead::new();
        let result = ToolResult::ok_json(serde_json::json!({"unexpected": true}));
        assert!(matches!(
            tool.render_output(&result),
            RenderIntent::Json { .. }
        ));
    }
}
