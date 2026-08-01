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
