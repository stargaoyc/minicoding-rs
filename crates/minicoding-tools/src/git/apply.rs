//! `git.apply`：应用 patch 到 worktree（T-M8-5）。
//!
//! 通过 `git apply --whitespace=nowarn` 应用 unified diff。patch 内容经 stdin
//! 传入（避免命令行长度限制）。`SideEffect::FileWrite`（修改文件，经权限审批）。

use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::Tool;
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
}
