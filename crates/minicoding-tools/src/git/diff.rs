//! `git.diff`：返回 worktree diff（T-M8-5）。
//!
//! 默认 `git diff HEAD`（staged + unstaged vs HEAD）；可指定 `ref`（如 `main`）
//! 和 `path`（相对 workdir 的子路径，经 `sandbox_path` 校验，C-03）。

use crate::util::resolve_path;
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{RenderIntent, Tool};
use tokio::process::Command;

/// PTM-6：git ref 安全校验（`git-check-ref-format` 安全子集）。
///
/// 拒绝：空值、选项形态（`-` 开头）、`..`（区间语法可携带 `--output` 等变体）、
/// 空白/控制字符、ref 规则禁用字符（`~^:?*[\` 与 `@{`）。
/// 注入样本（`--output=/x`、`--no-index /etc/passwd /dev/null`、`HEAD --output=x`）
/// 均以 `-` 开头或含空白，全部拦截。
fn validate_git_ref(r: &str) -> Result<(), ToolError> {
    let invalid = r.is_empty()
        || r.starts_with('-')
        || r.starts_with('/')
        || r.ends_with('/')
        || r.ends_with('.')
        || r.contains("..")
        || r.contains("@{")
        || r.chars().any(|c| {
            c.is_whitespace()
                || c.is_control()
                || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        });
    if invalid {
        return Err(ToolError::InvalidInput(format!(
            "非法 git ref `{r}`：ref 不能以 `-` 开头，且不得包含空白或 `~^:?*[\\ .. @{{` 等字符"
        )));
    }
    Ok(())
}

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
        let timeout = ctx.timeout;
        let git_ref = params
            .get("ref")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let path = params
            .get("path")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Box::pin(async move {
            // PTM-6（2026-08-26 R3 审查）：ref 直通 argv 存在 option 注入面
            // （`--output=<file>` 沙箱外任意写、`--no-index /etc/passwd` 跨边界
            // 读、`--ext-diff` 外部程序执行），且本工具 side_effect=None 进只读
            // 桶免审批、不接 OS 沙箱——三层防线全部旁路。按 git-check-ref-format
            // 的安全子集校验：拒绝选项形态与 ref 非法字符，注入即报错。
            if let Some(r) = &git_ref {
                validate_git_ref(r)?;
            }
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
            // T-3（2026-08-25 审查）：diff 在大仓库/慢盘上可能长时间运行，
            // 此前无超时会永久占用 turn。kill_on_drop 保证超时取消 future 时
            // 子进程被终止（C-07）。
            cmd.kill_on_drop(true);
            match tokio::time::timeout(timeout, cmd.output()).await {
                Err(elapsed) => Ok(ToolResult::err_text(format!(
                    "git diff 执行超时（超过 {elapsed:?}），子进程已终止"
                ))),
                Ok(Err(e)) => Err(ToolError::Exec(format!("git diff 执行失败: {e}"))),
                Ok(Ok(output)) => {
                    if !output.status.success() {
                        return Err(ToolError::Exec(format!(
                            "git diff 失败 (exit {}): {}",
                            output.status.code().unwrap_or(-1),
                            String::from_utf8_lossy(&output.stderr)
                        )));
                    }
                    Ok(ToolResult::ok_text(String::from_utf8_lossy(&output.stdout)))
                }
            }
        })
    }

    /// 渲染意图（R-05，M-11）：diff 文本 → 代码片段（`lang: "diff"` 语法高亮）。
    fn render_output(&self, result: &ToolResult) -> RenderIntent {
        match &result.content {
            minicoding_core::model::ToolContent::Text(text) => RenderIntent::Code {
                lang: Some("diff".to_string()),
                content: text.clone(),
            },
            _ => RenderIntent::default_for(result),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::model::ToolContent;
    use minicoding_core::tool::ToolContext;
    use std::collections::HashMap;
    use std::process::Command;

    fn make_ctx(workdir: &str) -> ToolContext {
        let mut env = HashMap::new();
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
    async fn diff_returns_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_git_repo(dir.path());

        // 创建初始文件并提交
        std::fs::write(dir.path().join("a.txt"), "original\n").expect("write");
        Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(dir.path())
            .output()
            .expect("git add");
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .expect("git commit");

        // 修改文件（unstaged change）
        std::fs::write(dir.path().join("a.txt"), "modified\n").expect("write modified");

        let tool = GitDiff::new();
        let ctx = make_ctx(dir.path().to_str().unwrap());
        let result = tool.execute(serde_json::json!({}), &ctx).await;
        assert!(result.is_ok(), "diff should succeed: {result:?}");
        let ToolContent::Text(text) = result.unwrap().content else {
            panic!("expected text content");
        };
        assert!(text.contains("modified"), "diff should contain changes");
    }

    #[tokio::test]
    async fn diff_with_path_filter() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_git_repo(dir.path());

        std::fs::write(dir.path().join("a.txt"), "original\n").expect("write a");
        std::fs::write(dir.path().join("b.txt"), "original\n").expect("write b");
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .expect("git add");
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .expect("git commit");

        std::fs::write(dir.path().join("a.txt"), "modified_a\n").expect("write a mod");
        std::fs::write(dir.path().join("b.txt"), "modified_b\n").expect("write b mod");

        let tool = GitDiff::new();
        let ctx = make_ctx(dir.path().to_str().unwrap());
        let result = tool
            .execute(serde_json::json!({"path": "a.txt"}), &ctx)
            .await;
        assert!(result.is_ok(), "diff with path should succeed: {result:?}");
        let ToolContent::Text(text) = result.unwrap().content else {
            panic!("expected text content");
        };
        assert!(text.contains("modified_a"), "should contain a.txt changes");
        assert!(
            !text.contains("modified_b"),
            "should not contain b.txt changes"
        );
    }

    #[tokio::test]
    async fn diff_path_escape_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_git_repo(dir.path());

        let tool = GitDiff::new();
        let ctx = make_ctx(dir.path().to_str().unwrap());
        // 路径越界（../）应被 sandbox_path 拒绝
        let result = tool
            .execute(serde_json::json!({"path": "../../../etc/passwd"}), &ctx)
            .await;
        assert!(result.is_err(), "path escape should be rejected");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn diff_timeout_returns_error_result() {
        // T-3（2026-08-25 审查）：ctx.timeout 耗尽 → is_error=true 的工具错误文本。
        // 用 PATH shim 注入一个"永远挂起"的假 git，保证确定性（真实 git 在小
        // 窗口内可能先完成，存在竞态；空仓库则 HEAD 128 直接失败）。
        use std::collections::HashMap;
        use std::os::unix::fs::PermissionsExt;

        let repo = tempfile::tempdir().expect("repo tmpdir");
        let shim = tempfile::tempdir().expect("shim tmpdir");
        let fake_git = shim.path().join("git");
        std::fs::write(&fake_git, "#!/bin/sh\nwhile true; do :; done\n").expect("write shim");
        std::fs::set_permissions(&fake_git, std::fs::Permissions::from_mode(0o755))
            .expect("chmod shim");

        let mut env = HashMap::new();
        env.insert(
            "PATH".to_string(),
            shim.path().to_string_lossy().to_string(),
        );
        let mut ctx = minicoding_core::tool::ToolContext::new(
            repo.path().to_str().unwrap().into(),
            "t".into(),
        );
        ctx.timeout = std::time::Duration::from_millis(300);
        ctx.env = env;

        let tool = GitDiff::new();
        let result = tool.execute(serde_json::json!({}), &ctx).await;
        let r = result.expect("超时应返回错误结果而非 Err");
        assert!(r.is_error, "超时结果应标记 is_error=true");
        match r.content {
            ToolContent::Text(t) => assert!(t.contains("超时"), "应说明超时: {t}"),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn diff_side_effect_is_none() {
        let tool = GitDiff::new();
        assert_eq!(tool.side_effect(), SideEffect::None);
    }

    #[test]
    fn diff_schema_has_correct_name() {
        let tool = GitDiff::new();
        assert_eq!(tool.name(), "git.diff");
    }

    // PTM-6（2026-08-26 R3 审查）：ref 注入面回归锁
    #[test]
    fn diff_rejects_option_injection_refs() {
        for bad in [
            "--output=/home/u/.bashrc",
            "--no-index /etc/passwd /dev/null",
            "-U0",
            "HEAD --output=x",
            "main..other --ext-diff",
            "a b",
            "",
            "@{upstream}",
            "feature/../evil",
        ] {
            assert!(validate_git_ref(bad).is_err(), "应拒绝: {bad}");
        }
    }

    #[test]
    fn diff_accepts_legit_refs() {
        for good in ["HEAD", "main", "origin/main", "v1.2.3", "feature/x_y-z"] {
            assert!(validate_git_ref(good).is_ok(), "应接受: {good}");
        }
    }
}
