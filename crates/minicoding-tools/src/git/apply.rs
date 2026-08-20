//! `git.apply`：应用 patch 到 worktree（T-M8-5）。
//!
//! 通过 `git apply --whitespace=nowarn` 应用 unified diff。patch 内容经 stdin
//! 传入（避免命令行长度限制）。`SideEffect::FileWrite`（修改文件，经权限审批）。

use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{RenderIntent, Tool};
use tokio::process::Command;

/// `git.apply` 工具。
pub struct GitApply {
    schema: ToolSchema,
}

impl GitApply {
    #[must_use]
    pub fn new() -> Self {
        let schema = ToolSchema {
            name: "git.apply".into(),
            description:
                "应用 unified diff patch 到 worktree（git apply）。patch 内容经 stdin 传入。".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "unified diff patch 内容"
                    }
                },
                "required": ["patch"]
            }),
        };
        Self { schema }
    }
}

impl Default for GitApply {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for GitApply {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::FileWrite
    }

    fn execute(
        &self,
        params: serde_json::Value,
        ctx: &minicoding_core::tool::ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        let workdir = ctx.workdir.clone();
        let env = ctx.env.clone();
        Box::pin(async move {
            let patch: String = params
                .get("patch")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("patch 缺失".into()))?
                .to_string();
            let mut cmd = Command::new("git");
            cmd.current_dir(workdir.as_std_path());
            cmd.arg("apply").arg("--whitespace=nowarn");
            cmd.env_clear();
            cmd.envs(&env);
            cmd.stdin(std::process::Stdio::piped());
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());

            let mut child = cmd
                .spawn()
                .map_err(|e| ToolError::Exec(format!("git apply spawn 失败: {e}")))?;

            // 写 patch 到 stdin
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                stdin
                    .write_all(patch.as_bytes())
                    .await
                    .map_err(|e| ToolError::Exec(format!("stdin 写入失败: {e}")))?;
                // drop stdin 触发 EOF
            }

            let output = child
                .wait_with_output()
                .await
                .map_err(|e| ToolError::Exec(format!("git apply 等待失败: {e}")))?;

            if !output.status.success() {
                return Err(ToolError::Exec(format!(
                    "git apply 失败 (exit {}): {}",
                    output.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            Ok(ToolResult::ok_text("patch 已应用"))
        })
    }

    /// 渲染意图（R-05，M-11）：应用确认消息，文本直出。
    fn render_output(&self, result: &ToolResult) -> RenderIntent {
        RenderIntent::default_for(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::tool::ToolContext;
    use std::collections::HashMap;
    use std::process::Command;

    fn make_ctx(workdir: &str) -> ToolContext {
        let mut env = HashMap::new();
        // git 需要 PATH 来找到二进制
        if let Ok(path) = std::env::var("PATH") {
            env.insert("PATH".to_string(), path);
        }
        if let Ok(home) = std::env::var("HOME") {
            env.insert("HOME".to_string(), home);
        }
        ToolContext {
            workdir: workdir.into(),
            session_id: "test".to_string(),
            env,
            ..ToolContext::new(workdir.into(), "test".to_string())
        }
    }

    fn init_git_repo(dir: &std::path::Path) {
        Command::new("git")
            .arg("init")
            .current_dir(dir)
            .output()
            .expect("git init");
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir)
            .output()
            .expect("git config email");
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .output()
            .expect("git config name");
    }

    #[tokio::test]
    async fn apply_missing_patch_returns_invalid_input() {
        let tool = GitApply::new();
        let result = tool.execute(serde_json::json!({}), &make_ctx("/tmp")).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn apply_valid_patch_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_git_repo(dir.path());

        // 创建初始文件并提交
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "hello\n").expect("write");
        Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(dir.path())
            .output()
            .expect("git add");
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .expect("git commit");

        // 修改文件后生成 patch
        std::fs::write(&file_path, "hello world\n").expect("write modified");
        let diff_output = Command::new("git")
            .args(["diff"])
            .current_dir(dir.path())
            .output()
            .expect("git diff");
        let patch = String::from_utf8_lossy(&diff_output.stdout).to_string();

        // 重置文件到原始状态
        std::fs::write(&file_path, "hello\n").expect("write reset");

        // 应用 patch
        let tool = GitApply::new();
        let ctx = make_ctx(dir.path().to_str().unwrap());
        let result = tool
            .execute(serde_json::json!({"patch": patch}), &ctx)
            .await;
        assert!(result.is_ok(), "apply should succeed: {result:?}");

        // 验证文件已修改（规范化行尾：Windows 下 git 可能把 LF 转 CRLF）
        let content = std::fs::read_to_string(&file_path).expect("read");
        let normalized = content.replace("\r\n", "\n");
        assert_eq!(normalized, "hello world\n");
    }

    #[test]
    fn apply_side_effect_is_file_write() {
        let tool = GitApply::new();
        assert_eq!(tool.side_effect(), SideEffect::FileWrite);
    }

    #[test]
    fn apply_schema_has_correct_name() {
        let tool = GitApply::new();
        assert_eq!(tool.name(), "git.apply");
    }
}
