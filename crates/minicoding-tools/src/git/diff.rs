//! `git.diff`：返回 worktree diff（T-M8-5）。
//!
//! 默认 `git diff HEAD`（staged + unstaged vs HEAD）；可指定 `ref`（如 `main`）
//! 和 `path`（相对 workdir 的子路径，经 `sandbox_path` 校验，C-03）。

use crate::util::resolve_path;
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::Tool;
use tokio::process::Command;

/// `git.diff` 工具。
pub struct GitDiff {
    schema: ToolSchema,
}

impl GitDiff {
    #[must_use]
    pub fn new() -> Self {
        let schema = ToolSchema {
            name: "git.diff".into(),
            description: "返回 git diff（默认 vs HEAD）。可指定 ref 和 path（相对 workdir）。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "ref": {
                        "type": "string",
                        "description": "对比的 git ref（默认 HEAD）"
                    },
                    "path": {
                        "type": "string",
                        "description": "限制 diff 范围的相对路径（可选）"
                    }
                }
            }),
        };
        Self { schema }
    }
}

impl Default for GitDiff {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for GitDiff {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::None
    }

    fn execute(
        &self,
        params: serde_json::Value,
        ctx: &minicoding_core::tool::ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        let workdir = ctx.workdir.clone();
        let env = ctx.env.clone();
        let git_ref = params
            .get("ref")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let path = params
            .get("path")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Box::pin(async move {
            let mut cmd = Command::new("git");
            cmd.current_dir(workdir.as_std_path());
            cmd.arg("diff");
            cmd.arg(git_ref.as_deref().unwrap_or("HEAD"));
            // 路径沙箱（C-03）：校验 path 在 workdir 内
            if let Some(p) = &path {
                let safe = resolve_path(&workdir, p)?;
                cmd.arg("--").arg(safe.as_str());
            }
            cmd.env_clear();
            cmd.envs(&env);
            let output = cmd
                .output()
                .await
                .map_err(|e| ToolError::Exec(format!("git diff 执行失败: {e}")))?;
            if !output.status.success() {
                return Err(ToolError::Exec(format!(
                    "git diff 失败 (exit {}): {}",
                    output.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            Ok(ToolResult::ok_text(String::from_utf8_lossy(&output.stdout)))
        })
    }
}
