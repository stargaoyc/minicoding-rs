//! `ScriptHook`：把外部可执行命令包装为 `Hook`（见 `hooks.md` §3、§5）。
//!
//! 协议：`HookInput` JSON → 子进程 stdin；子进程 stdout JSON → `HookOutput`；
//! 退出码 `0`=正常、`2`=deny、其他=错误（见 `protocol.rs`）。
//!
//! # 安全
//!
//! - **凭证隔离（C-04）**：`env_clear()` + 白名单，与 `shell.run` 一致
//! - **资源限制（C-07）**：stdout 截断 1 MiB 防 OOM；超时由 `ScriptHook::timeout`
//!   与 `DispatchConfig::timeout` 双重约束（取较短者）
//! - **路径校验（C-03）**：`modify_input` 仍经 `sandbox_path`，Hook 不能借此越界
//! - **命令注入防护**：`${TOOL_INPUT_<KEY>}` 展开值经 POSIX 单引号转义

use crate::protocol;
use minicoding_core::hooks::{Hook, HookError, HookInput, HookMatcher, HookOutput};
use minicoding_core::provider::BoxFuture;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Hook stdout 字节上限（1 MiB，见 `hooks.md` §7 "输出上限"）。
const MAX_STDOUT_BYTES: usize = 1024 * 1024;

/// 子进程 env 白名单（C-04 凭证不下传子进程，与 `shell.run` 一致）。
pub(crate) const ENV_WHITELIST: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM"];

/// 把外部可执行命令包装为 `Hook`（见 `hooks.md` §3、§5）。
///
/// 命令模板支持 `${TOOL_INPUT_<KEY>}` 占位符（按工具 input 字段展开，经 shell 转义）。
/// 子进程通过 stdin 收到完整 `HookInput` JSON，stdout 输出 `HookOutput` JSON。
///
/// # 超时
///
/// `ScriptHook::timeout` 是 Hook 自身超时上限；`DispatchConfig::timeout` 是 dispatch
/// 层的全局超时。两者同时生效，取较短者（per-hook 可比全局更短，不可更长）。
///
/// # 安全
///
/// 见模块级文档。
pub struct ScriptHook {
    name: String,
    matcher: HookMatcher,
    command: String,
    timeout: Duration,
}

impl ScriptHook {
    /// 创建 `ScriptHook`。
    ///
    /// # Arguments
    /// * `name` - Hook 唯一名（审计与日志用）。
    /// * `matcher` - 命中哪些事件与工具。
    /// * `command` - 命令模板，支持 `${TOOL_INPUT_<KEY>}` 占位符。
    /// * `timeout` - 单 Hook 超时（与 `DispatchConfig::timeout` 取较短者）。
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        matcher: HookMatcher,
        command: impl Into<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            name: name.into(),
            matcher,
            command: command.into(),
            timeout,
        }
    }
}

impl Hook for ScriptHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn matcher(&self) -> &HookMatcher {
        &self.matcher
    }

    fn run(&self, input: HookInput) -> BoxFuture<'_, Result<HookOutput, HookError>> {
        let name = self.name.clone();
        let command = self.command.clone();
        let timeout = self.timeout;
        Box::pin(async move { run_script_hook(&name, &command, timeout, input).await })
    }
}

/// 执行 `ScriptHook` 子进程（核心实现）。
async fn run_script_hook(
    name: &str,
    command_template: &str,
    timeout: Duration,
    input: HookInput,
) -> Result<HookOutput, HookError> {
    // 1. 展开 ${TOOL_INPUT_<KEY>} 占位符（经 shell 转义防注入）。
    //
    // Windows 禁用占位符展开（2026-08-23 审查 §9-P1 命令注入修复）：`shell_escape`
    // 是 POSIX 单引号方案，而 `cmd /C` 对单引号无任何特殊语义——`&`/`|`/`^` 保持
    // 活动字符，LLM 可控参数（如 `path = "\" & calc & echo \""`）直接拼接执行。
    // 完整输入已通过 stdin JSON 下发（步骤 5），脚本应从 stdin 取参而非命令行；
    // 模板中残留的占位符按字面传递（由脚本自行处理）。
    #[cfg(windows)]
    {
        if command_template.contains("${TOOL_INPUT_") {
            tracing::warn!(
                hook = %name,
                "hook template contains ${{TOOL_INPUT_*}} placeholders; expansion is disabled on                  Windows (cmd.exe does not honor POSIX quoting). Read input from stdin JSON instead."
            );
        }
    }
    let expanded = expand_placeholders(command_template, &input);

    // 2. 构造子进程命令（Unix: sh -c，Windows: cmd /C）。
    let mut command = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(&expanded);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(&expanded);
        c
    };

    // 3. cwd / env 隔离（C-04）/ stdio 配置。
    command
        .current_dir(&input.cwd)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for env_name in ENV_WHITELIST {
        if let Ok(value) = std::env::var(env_name) {
            command.env(env_name, value);
        }
    }

    // 4. spawn 子进程。
    let mut child = command
        .spawn()
        .map_err(|e| HookError::Internal(format!("spawn hook `{name}` failed: {e}")))?;

    // 5. 序列化 HookInput JSON，spawn 独立 task 写 stdin（避免与 stdout 读写死锁）。
    //    脚本可选读取 stdin（简单脚本可仅靠命令行参数；完整协议脚本读 stdin 取上下文）。
    let json = protocol::encode_input(&input)?;
    let stdin_handle = child.stdin.take();
    let stdin_task = tokio::spawn(async move {
        if let Some(mut stdin) = stdin_handle {
            // 写入失败（脚本已关闭 stdin）忽略：简单脚本不读 stdin。
            let _ = stdin.write_all(json.as_bytes()).await;
            let _ = stdin.write_all(b"\n").await;
            // stdin drop 关闭管道，脚本读到 EOF。
        }
    });

    // 6. 等待子进程退出（带超时）。`wait_with_output` 并发读 stdout/stderr 到 EOF。
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            // 子进程 IO 错误；等待 stdin task 完成。
            let _ = stdin_task.await;
            return Err(HookError::Internal(format!(
                "hook `{name}` wait failed: {e}"
            )));
        }
        Err(_) => {
            // 超时：`child` 被 drop（`wait_with_output` future drop）→ `kill_on_drop` 杀进程。
            let _ = stdin_task.await;
            return Err(HookError::Timeout {
                name: name.to_string(),
                timeout_sec: u32::try_from(timeout.as_secs()).unwrap_or(u32::MAX),
            });
        }
    };

    // 等待 stdin task 完成（确保 stdin 管道关闭）。
    let _ = stdin_task.await;

    // 7. 截断 stdout（C-07 防 OOM，见 `hooks.md` §7 "输出上限"）。
    let stdout = if output.stdout.len() > MAX_STDOUT_BYTES {
        tracing::warn!(
            hook = name,
            bytes = output.stdout.len(),
            max = MAX_STDOUT_BYTES,
            "hook stdout 超过 1 MiB 上限，截断"
        );
        String::from_utf8_lossy(&output.stdout[..MAX_STDOUT_BYTES]).into_owned()
    } else {
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    // 8. 退出码映射（0=正常解析、2=deny、其他=错误）。
    let code = output.status.code().unwrap_or(-1);
    protocol::map_exit_code(code, &stdout, &stderr, name)
}

/// 展开 `${TOOL_INPUT_<KEY>}` 占位符（经 shell 转义防注入）。
///
/// 占位符从 `HookInput::tool.input`（JSON 对象）取键为 `KEY` 的值；
/// 字符串值直接取用，其他类型 JSON 序列化后转义。无对应键时替换为空字符串。
///
/// # 安全
///
/// 所有展开值经 `shell_escape`（POSIX 单引号转义）包裹，防止命令注入。
fn expand_placeholders(template: &str, input: &HookInput) -> String {
    let tool_input = input
        .tool
        .as_ref()
        .map(|t| &t.input)
        .filter(|v| v.is_object());

    let mut result = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("${TOOL_INPUT_") {
        result.push_str(&rest[..start]);
        let after_start = &rest[start + "${TOOL_INPUT_".len()..];
        if let Some(end) = after_start.find('}') {
            let key = &after_start[..end];
            let value = tool_input
                .and_then(|v| v.get(key))
                .map(json_value_to_string)
                .unwrap_or_default();
            result.push_str(&shell_escape(&value));
            rest = &after_start[end + 1..];
        } else {
            // 无闭合 `}`，原样保留未完成占位符。
            result.push_str("${TOOL_INPUT_");
            rest = after_start;
        }
    }
    result.push_str(rest);
    result
}

/// 把 JSON 值转为字符串（字符串去引号；其他类型 JSON 序列化）。
fn json_value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Shell 单引号转义（POSIX sh 安全）：`value` → `'value'`，内部 `'` → `'\''`。
///
/// 单引号内除 `'` 外所有字符均无特殊含义，是最安全的转义方式。
fn shell_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            // 关闭当前单引号、转义单引号、重开单引号。
            escaped.push_str("'\\''");
        } else {
            escaped.push(ch);
        }
    }
    escaped.push('\'');
    escaped
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use camino::Utf8PathBuf;
    use minicoding_core::hooks::HookEvent;
    use minicoding_core::model::ToolCall;
    use serde_json::json;

    fn make_input_with_tool(tool_input: serde_json::Value) -> HookInput {
        let mut input = HookInput::new(
            HookEvent::PreToolUse,
            "test-session",
            1,
            Utf8PathBuf::from("."),
        );
        input.tool = Some(ToolCall {
            id: "call-1".to_string(),
            name: "fs.write".to_string(),
            input: tool_input,
        });
        input
    }

    #[test]
    fn shell_escape_plain() {
        assert_eq!(shell_escape("hello"), "'hello'");
    }

    #[test]
    fn shell_escape_with_single_quote() {
        // 含单引号：'...'\''...' 形式
        assert_eq!(shell_escape("a'b"), "'a'\\''b'");
    }

    #[test]
    fn shell_escape_empty() {
        assert_eq!(shell_escape(""), "''");
    }

    #[test]
    fn shell_escape_special_chars() {
        // $ ` ; & | 等特殊字符在单引号内无含义
        assert_eq!(shell_escape("$HOME;rm -rf /"), "'$HOME;rm -rf /'");
    }

    #[test]
    fn expand_placeholders_no_placeholders() {
        let input = make_input_with_tool(json!({"path": "/tmp/test.rs"}));
        let result = expand_placeholders("cargo fmt", &input);
        assert_eq!(result, "cargo fmt");
    }

    #[test]
    fn expand_placeholders_string_value() {
        let input = make_input_with_tool(json!({"path": "/tmp/test.rs"}));
        // 占位符 key 与 JSON key 大小写一致（这里都是小写 `path`）
        let result = expand_placeholders("prettier --write ${TOOL_INPUT_path}", &input);
        assert_eq!(result, "prettier --write '/tmp/test.rs'");
    }

    #[test]
    fn expand_placeholders_missing_key_replaced_with_empty() {
        let input = make_input_with_tool(json!({"path": "/tmp/test.rs"}));
        let result = expand_placeholders("cmd ${TOOL_INPUT_MISSING}", &input);
        assert_eq!(result, "cmd ''");
    }

    #[test]
    fn expand_placeholders_no_tool_input() {
        // 非工具事件（无 tool）→ 占位符替换为空
        let input = HookInput::new(HookEvent::SessionStart, "s", 1, Utf8PathBuf::from("."));
        let result = expand_placeholders("git status ${TOOL_INPUT_path}", &input);
        assert_eq!(result, "git status ''");
    }

    #[test]
    fn expand_placeholders_injection_attempt_escaped() {
        // 尝试注入：路径中含 ; rm -rf /，应被单引号包裹
        let input = make_input_with_tool(json!({"path": "foo; rm -rf /"}));
        let result = expand_placeholders("cat ${TOOL_INPUT_path}", &input);
        assert_eq!(result, "cat 'foo; rm -rf /'");
    }

    #[test]
    fn expand_placeholders_non_string_value() {
        // 非字符串值（数字/布尔）JSON 序列化后转义。
        // 注意：占位符外不要再加单引号，否则与转义的单引号叠加产生 `''42''`。
        let input = make_input_with_tool(json!({"line": 42, "flag": true}));
        let result = expand_placeholders("echo ${TOOL_INPUT_line}", &input);
        assert_eq!(result, "echo '42'");
    }

    #[test]
    fn expand_placeholders_unclosed_placeholder_preserved() {
        let input = make_input_with_tool(json!({"path": "/tmp"}));
        let result = expand_placeholders("cmd ${TOOL_INPUT_PATH", &input);
        assert_eq!(result, "cmd ${TOOL_INPUT_PATH");
    }

    // 以下测试使用 sh 语法（echo '...' / read / sed / ${VAR:-default} / true / exit N），
    // Windows cmd /C 不支持，仅在 Unix 上运行。
    #[cfg(unix)]
    #[tokio::test]
    async fn script_hook_echoes_continue_output() {
        // 简单脚本：echo 一行 JSON 到 stdout，退出码 0
        let hook = ScriptHook::new(
            "test-continue",
            HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            r#"echo '{"decision":"continue"}'"#,
            Duration::from_secs(5),
        );
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("."));
        let output = hook.run(input).await.expect("hook should succeed");
        assert_eq!(
            output.decision,
            minicoding_core::hooks::HookDecision::Continue
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn script_hook_exit_two_is_deny() {
        // 退出码 2 → deny，reason 取 stderr
        let hook = ScriptHook::new(
            "test-deny",
            HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            "echo 'blocked by policy' >&2; exit 2",
            Duration::from_secs(5),
        );
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("."));
        let output = hook.run(input).await.expect("exit 2 is Ok(Deny)");
        assert_eq!(output.decision, minicoding_core::hooks::HookDecision::Deny);
        assert_eq!(output.reason.as_deref(), Some("blocked by policy"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn script_hook_exit_one_is_error() {
        // 退出码 1 → HookError::ExitCode
        let hook = ScriptHook::new(
            "test-error",
            HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            "echo 'crashed' >&2; exit 1",
            Duration::from_secs(5),
        );
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("."));
        let result = hook.run(input).await;
        let err = result.expect_err("exit 1 should error");
        match err {
            HookError::ExitCode { name, code, stderr } => {
                assert_eq!(name, "test-error");
                assert_eq!(code, 1);
                assert_eq!(stderr.trim(), "crashed");
            }
            _ => panic!("expected ExitCode error, got {err:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn script_hook_empty_stdout_is_continue() {
        // 空输出 → Continue（默认）
        let hook = ScriptHook::new(
            "test-empty",
            HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            "true",
            Duration::from_secs(5),
        );
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("."));
        let output = hook.run(input).await.expect("true should succeed");
        assert_eq!(
            output.decision,
            minicoding_core::hooks::HookDecision::Continue
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn script_hook_timeout() {
        // 超时：sleep 10s，timeout=100ms
        let hook = ScriptHook::new(
            "test-timeout",
            HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            "sleep 10",
            Duration::from_millis(100),
        );
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("."));
        let result = hook.run(input).await;
        let err = result.expect_err("should timeout");
        assert!(matches!(err, HookError::Timeout { .. }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn script_hook_reads_stdin_json() {
        // 脚本读 stdin JSON，提取 turn 字段 echo 到 stdout。
        // 用 shell 内置工具（read/sed）避免依赖 python3/jq。
        let hook = ScriptHook::new(
            "test-stdin",
            HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            "read line; turn=$(echo \"$line\" | sed -n 's/.*\"turn\":\\([0-9]*\\).*/\\1/p'); echo \"{\\\"decision\\\":\\\"allow\\\",\\\"reason\\\":\\\"turn=$turn\\\"}\"",
            Duration::from_secs(5),
        );
        let input = HookInput::new(HookEvent::PreToolUse, "s", 42, Utf8PathBuf::from("."));
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, minicoding_core::hooks::HookDecision::Allow);
        assert_eq!(output.reason.as_deref(), Some("turn=42"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn script_hook_env_isolation() {
        // 验证 C-04：凭证不下传子进程
        // 设置一个伪凭证环境变量，脚本应读不到
        // 注：std::env::set_var 在 Rust 2024 是 unsafe
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "sk-test-secret-do-not-leak");
        }
        let hook = ScriptHook::new(
            "test-env-iso",
            HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            // 打印 OPENAI_API_KEY（应为空）
            r#"echo "{\"decision\":\"continue\",\"reason\":\"${OPENAI_API_KEY:-empty}\"}""#,
            Duration::from_secs(5),
        );
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("."));
        let output = hook.run(input).await.expect("should succeed");
        // 凭证不应出现在 stdout
        assert_eq!(
            output.decision,
            minicoding_core::hooks::HookDecision::Continue
        );
        // reason 应为 "empty"（env_clear 后 OPENAI_API_KEY 不存在）
        assert_eq!(output.reason.as_deref(), Some("empty"));
        // 清理
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn script_hook_placeholder_expansion_with_real_command() {
        // 集成测试：占位符展开 + 命令执行。
        // 用 shell 变量中转：`var=${TOOL_INPUT_path}` 让单引号转义值被 shell 正确解析。
        let mut input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("."));
        input.tool = Some(ToolCall {
            id: "c1".to_string(),
            name: "fs.write".to_string(),
            input: json!({"path": "hello.txt"}),
        });
        let hook = ScriptHook::new(
            "test-expand",
            HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            // 1) path=${TOOL_INPUT_path} → path='hello.txt'（shell 赋值去掉单引号）
            // 2) echo JSON，$path 展开为 hello.txt
            r#"path=${TOOL_INPUT_path}; echo "{\"decision\":\"continue\",\"reason\":\"got $path\"}""#,
            Duration::from_secs(5),
        );
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(
            output.decision,
            minicoding_core::hooks::HookDecision::Continue
        );
        assert_eq!(output.reason.as_deref(), Some("got hello.txt"));
    }
}
