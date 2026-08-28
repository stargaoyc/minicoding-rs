//! `git.apply`：应用 patch 到 worktree（T-M8-5）。
//!
//! 通过 `git apply --whitespace=nowarn` 应用 unified diff。patch 内容经 stdin
//! 传入（避免命令行长度限制）。`SideEffect::FileWrite`（修改文件，经权限审批）。

use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{RenderIntent, Tool};
use tokio::process::Command;

/// 解析 unified diff 的目标路径并校验全部落在 workdir 内（2026-08-23 审查
/// §6-P2：patch 内容 LLM 可控，`../` 相对路径/绝对路径不应直接交给
/// `git apply`——此前仅靠 git 自身行为约束，是路径沙箱的第二道防线缺口）。
///
/// 规则（保守词法校验）：`--- `/`+++ `/`diff --git ` 行提取目标；`/dev/null`
/// （新增/删除文件的空端）放行；其余目标剥 `a/`、`b/` 前缀后必须为相对路径
/// 且不含 `..` 组件与引号包裹。
///
/// TL-R6-2（2026-08-28 R6 审查）：`diff --git a/x b/y` 行此前未校验——git
/// 以该行为准，攻击者可把 `---`/`+++` 行放合法路径、`diff --git` 行放 `../`
/// 越界路径绕过防线。现在对 `diff --git` 行的 `a/` 与 `b/` 两侧目标同样校验。
fn validate_patch_paths(patch: &str) -> Result<(), ToolError> {
    let mut bad: Option<String> = None;
    'outer: for line in patch.lines() {
        let target = if let Some(rest) = line.strip_prefix("--- ") {
            Some(rest.trim())
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            Some(rest.trim())
        } else if let Some(rest) = line.strip_prefix("diff --git ") {
            // diff --git a/x b/y：两侧都须在校验内
            let mut parts = rest.split_whitespace().take(2).map(str::trim);
            let (a, b) = (parts.next(), parts.next());
            // 递归复用单侧校验（a/ 与 b/ 前缀各自剥离）
            let a_ok = a.is_some_and(patch_side_target_valid);
            let b_ok = b.is_some_and(patch_side_target_valid);
            if !a_ok || !b_ok {
                bad = Some(rest.to_string());
                break 'outer;
            }
            continue;
        } else {
            continue;
        };
        let Some(raw) = target else { continue };
        // 时间戳后缀（"--- a/x\t2024-01-01"）截断
        let raw = raw.split('\t').next().unwrap_or(raw);
        if !patch_side_target_valid(raw) {
            bad = Some(raw.to_string());
            break 'outer;
        }
    }
    match bad {
        Some(p) => Err(ToolError::InvalidInput(format!(
            "patch 目标路径越界（拒绝应用）: `{p}`——patch 仅允许修改工作目录内的相对路径"
        ))),
        None => Ok(()),
    }
}

/// 单侧 patch 目标路径合法性：`/dev/null` 放行；引号包裹、绝对路径、`..`、
/// 反斜杠、空串一律拒绝。
fn patch_side_target_valid(raw: &str) -> bool {
    // 引号包裹路径（含特殊字符时 git 会加引号）一律拒绝——保守处理
    if raw.starts_with('"') {
        return false;
    }
    if raw == "/dev/null" {
        return true;
    }
    let rel = raw
        .strip_prefix("a/")
        .or_else(|| raw.strip_prefix("b/"))
        .unwrap_or(raw);
    !(rel.starts_with('/')
        || rel.split('/').any(|seg| seg == "..")
        || rel.contains('\\')
        || rel.is_empty())
}

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
        let timeout = ctx.timeout;
        Box::pin(async move {
            let patch: String = params
                .get("patch")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("patch 缺失".into()))?
                .to_string();
            validate_patch_paths(&patch)?;
            let mut cmd = Command::new("git");
            cmd.current_dir(workdir.as_std_path());
            cmd.arg("apply").arg("--whitespace=nowarn");
            cmd.env_clear();
            cmd.envs(&env);
            cmd.stdin(std::process::Stdio::piped());
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());
            // T-3（2026-08-25 审查）：git apply 此前无超时——仓库钩子挂起会永久
            // 占用 turn。kill_on_drop 保证超时取消 future 时内部 Child 被终止
            // （C-07 不留孤儿进程）。
            cmd.kill_on_drop(true);

            let mut child = cmd
                .spawn()
                .map_err(|e| ToolError::Exec(format!("git apply spawn 失败: {e}")))?;

            // stdin 写入 + 等待退出统一纳入超时窗口（大 patch 写满管道同样可能卡住）
            let run = async {
                if let Some(mut stdin) = child.stdin.take() {
                    use tokio::io::AsyncWriteExt;
                    stdin
                        .write_all(patch.as_bytes())
                        .await
                        .map_err(|e| ToolError::Exec(format!("stdin 写入失败: {e}")))?;
                }
                child
                    .wait_with_output()
                    .await
                    .map_err(|e| ToolError::Exec(format!("git apply 等待失败: {e}")))
            };

            match tokio::time::timeout(timeout, run).await {
                Err(elapsed) => Ok(ToolResult::err_text(format!(
                    "git apply 执行超时（超过 {elapsed:?}），子进程已终止"
                ))),
                Ok(Err(e)) => Err(e),
                Ok(Ok(output)) => {
                    if !output.status.success() {
                        return Err(ToolError::Exec(format!(
                            "git apply 失败 (exit {}): {}",
                            output.status.code().unwrap_or(-1),
                            String::from_utf8_lossy(&output.stderr)
                        )));
                    }
                    Ok(ToolResult::ok_text("patch 已应用"))
                }
            }
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

    #[test]
    fn patch_with_parent_dir_target_rejected() {
        // ../ 越界目标 → 拒绝（2026-08-23 审查 §6-P2）
        let patch = "--- a/../etc/passwd\n+++ b/../etc/passwd\n@@ -1 +1 @@\n-x\n+y\n";
        assert!(validate_patch_paths(patch).is_err());
    }

    #[test]
    fn patch_with_absolute_target_rejected() {
        let patch = "--- /etc/passwd\n+++ /etc/passwd\n@@ -1 +1 @@\n-x\n+y\n";
        assert!(validate_patch_paths(patch).is_err());
    }

    #[test]
    fn patch_with_devnull_and_relative_targets_accepted() {
        // 新增文件：--- 端为 /dev/null，+++ 端为相对路径 → 放行
        let patch = "--- /dev/null\n+++ b/new_file.txt\n@@ -0,0 +1 @@\n+hello\n";
        assert!(validate_patch_paths(patch).is_ok());
    }

    #[test]
    fn patch_with_escaping_diff_git_line_rejected() {
        // TL-R6-2（2026-08-28 R6 审查）：git 以 `diff --git` 行为准——攻击者
        // 可把 ---/+++ 行放合法路径、diff --git 行放 ../ 越界路径绕过校验。
        let patch = "\
diff --git a/../etc/passwd b/../etc/passwd
--- a/etc/passwd
+++ b/etc/passwd
@@ -1 +1 @@
-root:x:0:0:root:/root:/bin/bash
-root2:x:0:0:root:/root:/bin/bash
";
        assert!(
            validate_patch_paths(patch).is_err(),
            "diff --git 行越界必须拒绝"
        );
    }

    #[test]
    fn patch_with_absolute_diff_git_line_rejected() {
        let patch = "\
diff --git a//etc/passwd b//etc/passwd
--- a/etc/passwd
+++ b/etc/passwd
@@ -1 +1 @@
-a
-b
";
        assert!(validate_patch_paths(patch).is_err(), "绝对路径必须拒绝");
    }

    #[test]
    fn patch_with_normal_diff_git_line_accepted() {
        let patch = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1 +1 @@
-old
-new
";
        assert!(validate_patch_paths(patch).is_ok());
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

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_timeout_returns_error_result() {
        // T-3（2026-08-25 审查）：ctx.timeout 耗尽 → is_error=true 的工具错误文本
        // （而非 Err 或无限挂起）。用 PATH shim 注入一个"永远挂起"的假 git，
        // 保证确定性（真实 git 在小窗口内可能先完成，存在竞态）。
        use std::collections::HashMap;
        use std::os::unix::fs::PermissionsExt;

        let repo = tempfile::tempdir().expect("repo tmpdir");
        init_git_repo(repo.path());
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

        let tool = GitApply::new();
        let result = tool
            .execute(
                serde_json::json!({"patch": "--- /dev/null\n+++ b/x.txt\n@@ -0,0 +1 @@\n+a\n"}),
                &ctx,
            )
            .await
            .expect("超时应返回错误结果而非 Err");
        assert!(result.is_error, "超时结果应标记 is_error=true");
        match result.content {
            minicoding_core::model::ToolContent::Text(t) => {
                assert!(t.contains("超时"), "错误文本应说明超时: {t}");
            }
            other => panic!("expected text content, got {other:?}"),
        }
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
