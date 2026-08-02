//! `shell.run`：执行 shell 命令（受超时、输出截断、env 过滤、OS 沙箱约束）。
//!
//! M4 接入：spawn 子进程前调 `SandboxDriver::apply` 应用内核级沙箱（第二道防线，
//! C-22）。沙箱策略由 `ToolContext::sandbox_policy` 提供（`Runtime` 注入）。
//! 未注入时退化为无 OS 隔离（兼容 M1-M3 测试）。

use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{Tool, ToolContext};
use serde::Deserialize;
use serde_json::json;
use std::fmt::Write as _;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// 合并后输出字符上限（C-07 资源不可耗尽）。
const MAX_OUTPUT_CHARS: usize = 10_000;

/// 子进程 env 白名单（C-04 凭证不下传子进程）。
///
/// 仅传递基础环境变量；`OPENAI_API_KEY`/`ANTHROPIC_API_KEY`/`*_KEY`/`*_TOKEN`/`*_SECRET`
/// 等凭证变量绝不传递（`env_clear` 后只 `env` 插入白名单项）。
const ENV_WHITELIST: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM"];

/// 执行 shell 命令的工具。
pub struct ShellRun {
    schema: ToolSchema,
}

impl ShellRun {
    /// 创建 `shell.run` 工具实例。
    #[must_use]
    pub fn new() -> Self {
        let schema = ToolSchema {
            name: "shell.run".to_string(),
            description:
                "通过 shell 执行命令（Unix 用 sh -c，Windows 用 cmd /C），工作目录为会话工作目录。"
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "要执行的命令。"
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "超时毫秒数（可选，默认由配置决定）。"
                    }
                },
                "required": ["command"]
            }),
        };
        Self { schema }
    }
}

impl Default for ShellRun {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct RunInput {
    command: String,
    timeout_ms: Option<u64>,
}

impl Tool for ShellRun {
    fn name(&self) -> &'static str {
        "shell.run"
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::Command
    }

    fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        let workdir = ctx.workdir.clone();
        let default_timeout = ctx.timeout;
        // 沙箱驱动/策略克隆（Option<Arc<...>> / Option<SandboxPolicy>）
        let sandbox_driver = ctx.sandbox_driver.clone();
        let sandbox_policy = ctx.sandbox_policy.clone();
        Box::pin(async move {
            let args: RunInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;

            // C-07：超时默认取 ToolContext（120s），输入可覆盖。
            let timeout = args
                .timeout_ms
                .map_or(default_timeout, Duration::from_millis);

            let mut command = if cfg!(windows) {
                let mut c = Command::new("cmd");
                c.arg("/C").arg(&args.command);
                c
            } else {
                let mut c = Command::new("sh");
                c.arg("-c").arg(&args.command);
                c
            };
            command
                .current_dir(&workdir)
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            // C-04：仅传递白名单 env，凭证变量绝不下传子进程。
            for name in ENV_WHITELIST {
                if let Ok(value) = std::env::var(name) {
                    command.env(name, value);
                }
            }

            // M4：OS 沙箱（第二道防线，C-22）。`apply` 在 spawn 前注入 landlock/
            // seatbelt 的 pre_exec 钩子。未注入驱动/策略时跳过（兼容测试）。
            if let (Some(driver), Some(policy)) = (sandbox_driver.as_ref(), sandbox_policy.as_ref())
            {
                driver.apply(policy, command.as_std_mut()).map_err(|e| {
                    // 沙箱 apply 失败（如 landlock ruleset 构建失败）视为执行错误，
                    // 由 Runtime 的 denial detector 进一步识别是否为 denial。
                    ToolError::Exec(format!("sandbox apply failed: {e}"))
                })?;
            }

            let child = command
                .spawn()
                .map_err(|e| ToolError::Exec(format!("failed to spawn command: {e}")))?;

            // C-07：超时后 kill 子进程。
            // `wait_with_output` 消耗 `child` 所有权；超时 future 被 drop 时，
            // `kill_on_drop(true)` 触发 `start_kill` 发送 SIGKILL。
            // 注：当前简化为单进程 kill；M4 将接入进程组（killpg）以清理子进程树。
            let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
                Ok(r) => r.map_err(ToolError::Io)?,
                Err(_) => return Err(ToolError::Timeout(timeout)),
            };

            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let mut combined = stdout;
            if !stderr.is_empty() {
                if !combined.is_empty() {
                    combined.push_str("\n--- stderr ---\n");
                }
                combined.push_str(&stderr);
            }

            let (text, truncated) = truncate_chars(combined, MAX_OUTPUT_CHARS);

            let mut result_text = String::new();
            match output.status.code() {
                Some(0) => {}
                Some(code) => {
                    let _ = writeln!(result_text, "[exit code: {code}]");
                }
                None => result_text.push_str("[terminated by signal]\n"),
            }
            result_text.push_str(&text);

            let bytes = result_text.len();
            let mut result = ToolResult::ok_text(result_text);
            result.metadata.truncated = truncated;
            result.metadata.bytes = bytes;
            Ok(result)
        })
    }
}

/// 按字符数截断输出，超出则附加截断标记（C-07）。
///
/// 返回 `(截断后的文本, 是否发生了截断)`。
#[must_use]
fn truncate_chars(text: String, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text, false);
    }
    let indicator = "\n... [output truncated]";
    let budget = max_chars.saturating_sub(indicator.chars().count());
    let truncated: String = text.chars().take(budget).collect();
    let mut result = String::with_capacity(truncated.len() + indicator.len());
    result.push_str(&truncated);
    result.push_str(indicator);
    (result, true)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use minicoding_core::model::{SideEffect, ToolContent, ToolError};
    use minicoding_core::tool::Tool;
    use tempfile::TempDir;

    /// 创建临时 workdir 并返回 `(TempDir, 规范化后的 workdir 路径)`。
    fn make_workdir() -> (TempDir, camino::Utf8PathBuf) {
        let tmp = TempDir::new().expect("create tempdir");
        let canon =
            camino::Utf8PathBuf::from_path_buf(tmp.path().canonicalize().expect("canonicalize"))
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

    #[cfg(unix)]
    #[tokio::test]
    async fn run_echo_outputs_text() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = ShellRun::new();
        let result = tool
            .execute(json!({"command": "echo hello"}), &ctx)
            .await
            .expect("run ok");
        assert!(!result.is_error);
        assert!(text_of(&result).contains("hello"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_nonexistent_command_exits_nonzero() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = ShellRun::new();
        // sh -c 找不到命令 → 退出码 127（非 spawn 失败）
        let result = tool
            .execute(json!({"command": "this_cmd_does_not_exist_xyz123"}), &ctx)
            .await
            .expect("run returns ok with nonzero exit");
        // 非 0 退出码 → 结果文本包含 [exit code: ...]
        assert!(text_of(&result).contains("[exit code:"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_timeout_returns_timeout_error() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = ShellRun::new();
        // sleep 2 + 100ms 超时
        let err = tool
            .execute(json!({"command": "sleep 2", "timeout_ms": 100}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Timeout(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_long_output_truncated() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = ShellRun::new();
        // seq 1 20000 产生约 100k 字符，超过 MAX_OUTPUT_CHARS(10000)
        let result = tool
            .execute(json!({"command": "seq 1 20000"}), &ctx)
            .await
            .expect("run ok");
        assert!(result.metadata.truncated, "output should be truncated");
        assert!(text_of(&result).contains("... [output truncated]"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_stderr_captured_in_output() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = ShellRun::new();
        // 同时产生 stdout 和 stderr，触发分隔标记逻辑
        let result = tool
            .execute(json!({"command": "echo out_msg; echo err_msg >&2"}), &ctx)
            .await
            .expect("run ok");
        let text = text_of(&result);
        assert!(text.contains("out_msg"));
        assert!(text.contains("err_msg"));
        // stdout 非空时，stderr 部分应有分隔标记
        assert!(text.contains("--- stderr ---"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_stderr_only_no_separator() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = ShellRun::new();
        // 仅 stderr 有内容（stdout 为空）→ 不附加分隔标记，直接拼接 stderr
        let result = tool
            .execute(json!({"command": "echo err_only >&2"}), &ctx)
            .await
            .expect("run ok");
        let text = text_of(&result);
        assert!(text.contains("err_only"));
        assert!(!text.contains("--- stderr ---"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_exit_code_zero_no_prefix() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = ShellRun::new();
        // 退出码 0 → 不附加 [exit code: ...] 前缀
        let result = tool
            .execute(json!({"command": "true"}), &ctx)
            .await
            .expect("run ok");
        assert!(!result.is_error);
        assert!(!text_of(&result).contains("[exit code:"));
    }

    #[tokio::test]
    async fn run_missing_command_returns_invalid_input() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = ShellRun::new();
        let err = tool.execute(json!({}), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[test]
    fn run_side_effect_is_command() {
        let tool = ShellRun::new();
        assert_eq!(tool.side_effect(), SideEffect::Command);
        // Command 副作用 → 非只读
        assert!(!tool.is_read_only());
    }

    #[test]
    fn run_schema_name_correct() {
        let tool = ShellRun::new();
        assert_eq!(tool.name(), "shell.run");
        assert_eq!(tool.schema().name, "shell.run");
    }

    // === truncate_chars 单元测试 ===

    #[test]
    fn truncate_chars_short_text_not_truncated() {
        let (result, truncated) = truncate_chars("hello".to_string(), 100);
        assert_eq!(result, "hello");
        assert!(!truncated);
    }

    #[test]
    fn truncate_chars_exact_length_not_truncated() {
        let text = "hello".to_string();
        let (result, truncated) = truncate_chars(text.clone(), text.chars().count());
        assert_eq!(result, text);
        assert!(!truncated);
    }

    #[test]
    fn truncate_chars_long_text_truncated_with_indicator() {
        // 100 个字符，上限 50
        let text = "a".repeat(100);
        let (result, truncated) = truncate_chars(text, 50);
        assert!(truncated);
        assert!(result.ends_with("\n... [output truncated]"));
    }

    #[test]
    fn truncate_chars_multibyte_at_char_boundary() {
        // 中文字符每个 1 个 char（按 chars().count()），60 个中文字符
        let text = "中".repeat(60);
        let (result, truncated) = truncate_chars(text, 50);
        assert!(truncated);
        assert!(result.ends_with("\n... [output truncated]"));
    }

    #[test]
    fn truncate_chars_empty_text_not_truncated() {
        let (result, truncated) = truncate_chars(String::new(), 10);
        assert_eq!(result, "");
        assert!(!truncated);
    }
}
