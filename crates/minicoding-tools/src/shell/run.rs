//! `shell.run`：执行 shell 命令（受超时、输出截断、env 过滤、OS 沙箱约束）。
//!
//! M4 接入：spawn 子进程前调 `SandboxDriver::apply` 应用内核级沙箱（第二道防线，
//! C-22）。沙箱策略由 `ToolContext::sandbox_policy` 提供（`Runtime` 注入）。
//! 未注入时退化为无 OS 隔离（兼容 M1-M3 测试）。

use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::otel::span_name;
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{RenderIntent, Tool, ToolContext};
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

    #[allow(clippy::too_many_lines)] // spawn+沙箱+进程组+流式读取的线性执行流
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
        // S10：输出字节上限（clone 避免 ctx 引用进入 future）
        let max_output_bytes = ctx.max_output_bytes;
        Box::pin(async move {
            let args: RunInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;

            // C-07/S8：超时默认取 ToolContext（120s）；工具入参只能**缩短**不能超过
            // 上限——防 LLM 传 `timeout_ms: u64::MAX` 使超时约束形同虚设。
            let timeout = args
                .timeout_ms
                .map(Duration::from_millis)
                .map_or(default_timeout, |t| t.min(default_timeout));

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
            // S9/C-07（unix）：子进程自成进程组长（pgid == pid），超时后可 killpg
            // 整树清理后台孤儿。与沙箱驱动的 pre_exec 钩子可共存（按序执行）。
            #[cfg(unix)]
            // SAFETY: pre_exec 闭包在 fork 后 exec 前的子进程上下文运行；
            // setpgid(0,0) 仅设置自身进程组，纯 syscall，async-signal-safe。
            unsafe {
                command.pre_exec(|| {
                    if libc::setpgid(0, 0) == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                });
            }
            // C-04：仅传递白名单 env，凭证变量绝不下传子进程。
            for name in ENV_WHITELIST {
                if let Ok(value) = std::env::var(name) {
                    command.env(name, value);
                }
            }

            // M4：OS 沙箱（第二道防线，C-22）。`apply` 在 spawn 前注入 landlock/
            // seatbelt 的 pre_exec 钩子（Linux/macOS），或设置 CREATE_SUSPENDED（Windows）。
            // 未注入驱动/策略时跳过（兼容测试）。
            let has_sandbox = sandbox_driver.is_some() && sandbox_policy.is_some();
            if let (Some(driver), Some(policy)) = (sandbox_driver.as_ref(), sandbox_policy.as_ref())
            {
                let span = tracing::debug_span!(
                    "sandbox.apply",
                    otel.name = span_name::SANDBOX_APPLY,
                    driver = driver.id(),
                );
                let _enter = span.enter();
                driver.apply(policy, command.as_std_mut()).map_err(|e| {
                    // 沙箱 apply 失败（如 landlock ruleset 构建失败）视为执行错误，
                    // 由 Runtime 的 denial detector 进一步识别是否为 denial。
                    ToolError::Exec(format!("sandbox apply failed: {e}"))
                })?;
            }

            let mut child = command
                .spawn()
                .map_err(|e| ToolError::Exec(format!("failed to spawn command: {e}")))?;

            // Windows Job Object：spawn 后需 post_spawn 分配 Job Object + 恢复线程。
            // Linux/macOS 的 post_spawn 为 no-op（沙箱在 pre_exec 内一次性完成）。
            if has_sandbox
                && let Some(driver) = sandbox_driver.as_ref()
                && let Some(pid) = child.id()
            {
                driver.post_spawn(pid).map_err(|e| {
                    // post_spawn 失败（如 Job Object 分配失败）：kill 子进程并报错
                    let _ = child.start_kill();
                    ToolError::Exec(format!("sandbox post_spawn failed: {e}"))
                })?;
            }

            // S10/C-07：流式读取 + 每路字节上限——不再 `wait_with_output` 全量缓冲
            // （超时窗口内 `cat /dev/urandom` 可打爆内存）。上限取 ToolContext 的
            // max_output_bytes（默认 1 MiB），显示层仍有 MAX_OUTPUT_CHARS 二次截断。
            let cap = max_output_bytes;
            let mut stdout_pipe = child
                .stdout
                .take()
                .ok_or_else(|| ToolError::Exec("stdout 管道丢失".into()))?;
            let mut stderr_pipe = child
                .stderr
                .take()
                .ok_or_else(|| ToolError::Exec("stderr 管道丢失".into()))?;
            let stdout_task = tokio::spawn(async move { read_capped(&mut stdout_pipe, cap).await });
            let stderr_task = tokio::spawn(async move { read_capped(&mut stderr_pipe, cap).await });

            // C-07/S9：超时后先 killpg 整树再 start_kill 兜底。
            // 不用 wait_with_output（消耗所有权）：保留 child 句柄以便取 pid 杀组。
            let status = if let Ok(r) = tokio::time::timeout(timeout, child.wait()).await {
                r.map_err(ToolError::Io)?
            } else {
                #[cfg(unix)]
                if let Some(pid) = child.id() {
                    // 子进程是组长（setpgid(0,0)），pgid == pid
                    let _ = tokio::task::spawn_blocking(move || unsafe {
                        libc::killpg(i32::try_from(pid).unwrap_or(-1), libc::SIGKILL)
                    })
                    .await;
                }
                let _ = child.start_kill();
                // 等 pipe 读任务收尾（SIGKILL 后管道 EOF）
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return Err(ToolError::Timeout(timeout));
            };
            let out_bytes = stdout_task.await.unwrap_or_default();
            let err_bytes = stderr_task.await.unwrap_or_default();

            let stdout = String::from_utf8_lossy(&out_bytes).into_owned();
            let stderr = String::from_utf8_lossy(&err_bytes).into_owned();
            let mut combined = stdout;
            if !stderr.is_empty() {
                if !combined.is_empty() {
                    combined.push_str("\n--- stderr ---\n");
                }
                combined.push_str(&stderr);
            }

            // S10：流式上限截断标记（cap 触顶即视为截断）+ 显示层二次截断
            let stream_truncated = out_bytes.len() >= cap || err_bytes.len() >= cap;
            let (text, truncated) = {
                let (t, tr) = truncate_chars(combined, MAX_OUTPUT_CHARS);
                (t, tr || stream_truncated)
            };

            let mut result_text = String::new();
            match status.code() {
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

    /// 渲染意图（R-05，M-11）：命令输出 → 代码片段（语言未知，可能是 shell 输出或日志）。
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

/// S10：带字节上限的异步读取——达到上限后继续消费管道（防 SIGPIPE 干扰子进程）
/// 但不再累积内存，返回实际保留的字节。
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(r: &mut R, cap: usize) -> std::vec::Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::with_capacity(cap.min(64 * 1024));
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let take = n.min(cap - buf.len());
                    buf.extend_from_slice(&chunk[..take]);
                }
            }
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    #[cfg(unix)]
    use minicoding_core::model::ToolContent;
    use minicoding_core::model::{SideEffect, ToolError};
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
    #[cfg(unix)]
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

    /// S8：timeout_ms 超过 ctx 上限时被 clamp——sleep 5 + timeout_ms=u64::MAX
    /// 应在 ctx.timeout（默认 120s？测试用短超时覆盖）内返回 Timeout。
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_clamped_to_context_limit() {
        let (_tmp, workdir) = make_workdir();
        let mut ctx = ToolContext::new(workdir, "test".to_string());
        ctx.timeout = std::time::Duration::from_millis(300);
        let tool = ShellRun::new();
        let started = std::time::Instant::now();
        let err = tool
            .execute(
                json!({"command": "sleep 5", "timeout_ms": 18446744073709551615u64}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Timeout(_)),
            "应返回 Timeout: {err:?}"
        );
        // 300ms 超时 + 杀树余量，远小于 sleep 5
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "clamp 生效"
        );
    }

    /// S9（unix）：超时后进程组整树清理——后台孤儿不残留。
    #[cfg(unix)]
    #[tokio::test]
    async fn background_orphans_killed_on_timeout() {
        let (_tmp, workdir) = make_workdir();
        let mut ctx = ToolContext::new(workdir.clone(), "test".to_string());
        ctx.timeout = std::time::Duration::from_millis(400);
        let tool = ShellRun::new();
        // 后台 sleep 60 孤儿 + 前台长任务
        let err = tool
            .execute(json!({"command": "sleep 60 & sleep 30"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Timeout(_)));
        // killpg 后短暂等待，确认无 `sleep 60` 残留
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg("ps -eo pid,cmd | grep 'sleep 60' | grep -v grep | wc -l")
            .output()
            .expect("ps");
        let count = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(count, "0", "后台孤儿应被 killpg 清理，实际残留 {count}");
    }

    /// S10：流式字节上限——大输出在 cap 处截断，metadata.truncated 置位。
    #[cfg(unix)]
    #[tokio::test]
    async fn output_capped_at_max_output_bytes() {
        let (_tmp, workdir) = make_workdir();
        let mut ctx = ToolContext::new(workdir, "test".to_string());
        ctx.max_output_bytes = 4096;
        let tool = ShellRun::new();
        // ~3.4MB 输出，远超 4KB cap
        let result = tool
            .execute(json!({"command": "seq 1 500000"}), &ctx)
            .await
            .expect("run ok");
        assert!(result.metadata.truncated);
        assert!(
            result.metadata.bytes < 8192,
            "保留字节数应在 cap 附近而非全量: {}",
            result.metadata.bytes
        );
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
