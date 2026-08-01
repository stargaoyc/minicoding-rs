//! `fs.read`：读取文件内容（支持行范围）。
//!
//! 对敏感文件（`.env`、`credentials`、`*.pem` 等）自动应用脱敏（C-04，T-M4-11），
//! 把 `API_KEY=xxx` / `password=xxx` 等字段值替换为 `***`，避免回灌 LLM 上下文。

use crate::util::{resolve_path, truncate_output};
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{Tool, ToolContext};
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
        Box::pin(async move {
            let args: ReadInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
            let path = resolve_path(&workdir, &args.path)?;

            let content = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => ToolError::NotFound(args.path.clone()),
                    _ => ToolError::Io(e),
                })?;

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
}

/// 判断路径是否为敏感文件（应脱敏）。
///
/// 匹配规则：
/// - 文件名为 `.env` 或以 `.env.` 开头（`.env.local`/`.env.production`）；
/// - 文件名等于 `credentials` / `creds`；
/// - 扩展名为 `.pem` / `.key` / `.pfx` / `.p12`；
/// - 文件名含 `secret` / `password` / `token`（不区分大小写）。
fn is_sensitive_path(path: &camino::Utf8Path) -> bool {
    // 常量前置，避免 `items_after_statements` 警告。
    const EXACT: &[&str] = &["credentials", "creds"];
    const SENSITIVE_EXT: &[&str] = &["pem", "key", "pfx", "p12"];
    const KEYWORDS: &[&str] = &["secret", "password", "token"];

    let Some(file_name) = path.file_name() else {
        return false;
    };
    let lower = file_name.to_lowercase();

    // .env 系列精确/前缀匹配
    if lower == ".env" || lower.starts_with(".env.") {
        return true;
    }

    // 精确匹配
    if EXACT.contains(&lower.as_str()) {
        return true;
    }

    // 扩展名匹配
    if let Some(ext) = path.extension() {
        if SENSITIVE_EXT.contains(&ext) {
            return true;
        }
    }

    // 关键词包含匹配
    KEYWORDS.iter().any(|kw| lower.contains(kw))
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
}
