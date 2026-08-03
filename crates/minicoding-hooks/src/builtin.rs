//! 6 个内置示例 Hook（见 `hooks.md` §9）。
//!
//! 这些 Hook 以 Rust 进程内实现提供（非 `ScriptHook`），可作为用户自定义 Hook 的参考
//! 模板，也可由 CLI 直接注册为开箱即用的默认 Hook。
//!
//! | 名称 | 事件 | 用途 |
//! |------|------|------|
//! | `fmt-on-write` | `PostToolUse(fs.write\|fs.edit)` | 写后跑 `cargo fmt`/`prettier` |
//! | `auto-approve-tests` | `PermissionRequest(shell.run)` | 前缀 `cargo test`/`npm test` 自动批准 |
//! | `block-secrets` | `PreToolUse(fs.write)` | 拒绝写入含 `api_key`/`password` 的内容 |
//! | `git-status-inject` | `SessionStart` | 注入 `git status --short` |
//! | `backup-before-compact` | `PreCompact` | 压缩前备份 jsonl 到 `.backup` |
//! | `test-on-stop` | `Stop` | 一轮结束跑测试，失败则要求继续 |
//!
//! # 安全
//!
//! - 凭证隔离（C-04）：子进程 `env_clear()` + 白名单（与 `ScriptHook` 一致）
//! - 资源限制（C-07）：子进程输出截断、超时由 `Hook::run` 内部控制

// trait `Hook::name` 签名要求 `&str`，实现返回 `&'static str` 字面量时
// clippy `unnecessary_literal_bound` 误报，此处整体放行。
#![allow(clippy::unnecessary_literal_bound)]

use crate::script::ENV_WHITELIST;
use minicoding_core::hooks::{Hook, HookError, HookEvent, HookInput, HookMatcher, HookOutput};
use minicoding_core::provider::BoxFuture;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// 子进程默认超时（内置 Hook 跑的外部命令）。
const DEFAULT_CMD_TIMEOUT_SEC: u64 = 30;

/// 子进程输出字符上限（C-07）。
const MAX_OUTPUT_CHARS: usize = 10_000;

// ============================================================================
// 1. fmt-on-write
// ============================================================================

/// `fmt-on-write`：`fs.write`/`fs.edit` 后按文件扩展名跑格式化工具（见 `hooks.md` §9）。
///
/// - `.rs` → `rustfmt --edition 2024 <path>`
/// - `.ts`/`.tsx`/`.js`/`.jsx`/`.json`/`.css` → `prettier --write <path>`
/// - 其他扩展名跳过
///
/// 返回 `Continue`（不干预主流程），格式化失败仅记 `reason` 供审计。
pub struct FmtOnWrite;

impl FmtOnWrite {
    /// 创建 `fmt-on-write` Hook。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for FmtOnWrite {
    fn default() -> Self {
        Self::new()
    }
}

impl Hook for FmtOnWrite {
    fn name(&self) -> &str {
        "fmt-on-write"
    }

    fn matcher(&self) -> &HookMatcher {
        static MATCHER: std::sync::OnceLock<HookMatcher> = std::sync::OnceLock::new();
        MATCHER.get_or_init(|| {
            HookMatcher::for_tools(
                vec![HookEvent::PostToolUse],
                vec!["fs.write".to_string(), "fs.edit".to_string()],
            )
        })
    }

    fn run(&self, input: HookInput) -> BoxFuture<'_, Result<HookOutput, HookError>> {
        Box::pin(async move { run_fmt_on_write(input).await })
    }
}

/// 按扩展名选择格式化命令。
fn formatter_for_path(path: &str) -> Option<(&'static str, Vec<&'static str>)> {
    let ext = path.rsplit('.').next()?;
    match ext {
        "rs" => Some(("rustfmt", vec!["--edition", "2024"])),
        "ts" | "tsx" | "js" | "jsx" | "json" | "css" | "scss" | "html" | "yml" | "yaml" => {
            Some(("prettier", vec!["--write"]))
        }
        _ => None,
    }
}

async fn run_fmt_on_write(input: HookInput) -> Result<HookOutput, HookError> {
    let Some(ref tool) = input.tool else {
        return Ok(HookOutput::continue_());
    };
    let Some(path) = tool.input.get("path").and_then(|v| v.as_str()) else {
        return Ok(HookOutput::continue_());
    };

    let Some((cmd, base_args)) = formatter_for_path(path) else {
        // 无对应格式化工具，跳过
        return Ok(HookOutput::continue_());
    };

    // 构造命令：cmd base_args... path
    let mut command = Command::new(cmd);
    command
        .args(&base_args)
        .arg(path)
        .current_dir(&input.cwd)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for env_name in ENV_WHITELIST {
        if let Ok(value) = std::env::var(env_name) {
            command.env(env_name, value);
        }
    }

    let timeout = Duration::from_secs(DEFAULT_CMD_TIMEOUT_SEC);
    match tokio::time::timeout(timeout, command.output()).await {
        Ok(Ok(output)) if output.status.success() => Ok(HookOutput::continue_()),
        Ok(Ok(_output)) => {
            // 格式化失败：不阻断，仅记 reason
            Ok(HookOutput::continue_())
        }
        Ok(Err(e)) => Err(HookError::Internal(format!(
            "fmt-on-write spawn `{cmd}` failed: {e}"
        ))),
        Err(_) => Err(HookError::Timeout {
            name: "fmt-on-write".to_string(),
            timeout_sec: u32::try_from(timeout.as_secs()).unwrap_or(u32::MAX),
        }),
    }
}

// ============================================================================
// 2. auto-approve-tests
// ============================================================================

/// `auto-approve-tests`：`PermissionRequest(shell.run)` 时，若命令以 `cargo test`/
/// `npm test`/`pnpm test`/`yarn test` 开头，自动批准（见 `hooks.md` §9）。
///
/// 仅对 `shell.run` 工具生效；其他工具返回 `Continue`（不干预）。
pub struct AutoApproveTests;

impl AutoApproveTests {
    /// 创建 `auto-approve-tests` Hook。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for AutoApproveTests {
    fn default() -> Self {
        Self::new()
    }
}

impl Hook for AutoApproveTests {
    fn name(&self) -> &str {
        "auto-approve-tests"
    }

    fn matcher(&self) -> &HookMatcher {
        static MATCHER: std::sync::OnceLock<HookMatcher> = std::sync::OnceLock::new();
        MATCHER.get_or_init(|| {
            HookMatcher::for_tools(
                vec![HookEvent::PermissionRequest],
                vec!["shell.run".to_string()],
            )
        })
    }

    fn run(&self, input: HookInput) -> BoxFuture<'_, Result<HookOutput, HookError>> {
        Box::pin(async move {
            let Some(ref tool) = input.tool else {
                return Ok(HookOutput::continue_());
            };
            let Some(cmd) = tool.input.get("command").and_then(|v| v.as_str()) else {
                return Ok(HookOutput::continue_());
            };
            let trimmed = cmd.trim();
            if is_test_command(trimmed) {
                Ok(HookOutput::allow("auto-approve-tests: test command"))
            } else {
                Ok(HookOutput::continue_())
            }
        })
    }
}

/// 判断命令是否为测试命令（前缀匹配，自动 trim 前导空白）。
fn is_test_command(cmd: &str) -> bool {
    let trimmed = cmd.trim_start();
    let prefixes = ["cargo test", "npm test", "pnpm test", "yarn test", "pytest"];
    prefixes.iter().any(|p| trimmed.starts_with(p))
}

// ============================================================================
// 3. block-secrets
// ============================================================================

/// `block-secrets`：`PreToolUse(fs.write)` 时，若写入内容含 `api_key=`/`password=`/
/// `secret=` 等凭证模式，拒绝写入（见 `hooks.md` §9）。
///
/// 仅对 `fs.write` 生效（`fs.edit` 的 `new_string` 也检查）。返回 `Deny` 阻断。
pub struct BlockSecrets;

impl BlockSecrets {
    /// 创建 `block-secrets` Hook。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for BlockSecrets {
    fn default() -> Self {
        Self::new()
    }
}

impl Hook for BlockSecrets {
    fn name(&self) -> &str {
        "block-secrets"
    }

    fn matcher(&self) -> &HookMatcher {
        static MATCHER: std::sync::OnceLock<HookMatcher> = std::sync::OnceLock::new();
        MATCHER.get_or_init(|| {
            HookMatcher::for_tools(
                vec![HookEvent::PreToolUse],
                vec!["fs.write".to_string(), "fs.edit".to_string()],
            )
        })
    }

    fn run(&self, input: HookInput) -> BoxFuture<'_, Result<HookOutput, HookError>> {
        Box::pin(async move {
            let Some(ref tool) = input.tool else {
                return Ok(HookOutput::continue_());
            };
            // fs.write 检查 content；fs.edit 检查 new_string
            let content_to_check = tool
                .input
                .get("content")
                .and_then(|v| v.as_str())
                .or_else(|| tool.input.get("new_string").and_then(|v| v.as_str()));
            let Some(content) = content_to_check else {
                return Ok(HookOutput::continue_());
            };
            if let Some(pattern) = detect_secret_pattern(content) {
                Ok(HookOutput::deny(format!(
                    "block-secrets: 检测到凭证模式 `{pattern}`，拒绝写入"
                )))
            } else {
                Ok(HookOutput::continue_())
            }
        })
    }
}

/// 检测内容中是否含凭证模式（简单启发式）。
///
/// 返回匹配到的模式字符串（供 deny reason 展示）。
fn detect_secret_pattern(content: &str) -> Option<&'static str> {
    // 按行扫描，忽略大小写
    let lower = content.to_lowercase();
    let patterns = [
        "api_key=",
        "api_key:",
        "apikey=",
        "apikey:",
        "password=",
        "password:",
        "passwd=",
        "passwd:",
        "secret=",
        "secret:",
        "access_token=",
        "access_token:",
        "private_key=",
        "private_key:",
        "aws_access_key_id=",
        "aws_secret_access_key=",
    ];
    patterns.iter().find(|p| lower.contains(*p)).copied()
}

// ============================================================================
// 4. git-status-inject
// ============================================================================

/// `git-status-inject`：`SessionStart` 时注入 `git status --short` 输出到上下文
/// （见 `hooks.md` §9）。
///
/// 在 git 仓库内运行 `git status --short`，输出包裹在 `<hook_context>` 边界注入
/// （由 Runtime 拼接，C-05）。非 git 仓库静默跳过。
pub struct GitStatusInject;

impl GitStatusInject {
    /// 创建 `git-status-inject` Hook。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for GitStatusInject {
    fn default() -> Self {
        Self::new()
    }
}

impl Hook for GitStatusInject {
    fn name(&self) -> &str {
        "git-status-inject"
    }

    fn matcher(&self) -> &HookMatcher {
        static MATCHER: std::sync::OnceLock<HookMatcher> = std::sync::OnceLock::new();
        MATCHER.get_or_init(|| HookMatcher::for_events(vec![HookEvent::SessionStart]))
    }

    fn run(&self, input: HookInput) -> BoxFuture<'_, Result<HookOutput, HookError>> {
        Box::pin(async move { run_git_status(input).await })
    }
}

async fn run_git_status(input: HookInput) -> Result<HookOutput, HookError> {
    let mut command = Command::new("git");
    command
        .arg("status")
        .arg("--short")
        .current_dir(&input.cwd)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for env_name in ENV_WHITELIST {
        if let Ok(value) = std::env::var(env_name) {
            command.env(env_name, value);
        }
    }

    let timeout = Duration::from_secs(10);
    let output = match tokio::time::timeout(timeout, command.output()).await {
        Ok(Ok(o)) if o.status.success() => o,
        Ok(Ok(_)) => {
            // git status 失败（非 git 仓库等），静默跳过
            return Ok(HookOutput::continue_());
        }
        Ok(Err(e)) => {
            return Err(HookError::Internal(format!(
                "git-status-inject spawn git failed: {e}"
            )));
        }
        Err(_) => {
            return Err(HookError::Timeout {
                name: "git-status-inject".to_string(),
                timeout_sec: 10,
            });
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = stdout.trim();
    if stdout.is_empty() {
        return Ok(HookOutput::continue_());
    }
    // 截断防 OOM（C-07）
    let truncated = if stdout.len() > MAX_OUTPUT_CHARS {
        &stdout[..MAX_OUTPUT_CHARS]
    } else {
        stdout
    };
    let context = format!("git status --short:\n{truncated}");
    Ok(HookOutput::inject(context))
}

// ============================================================================
// 5. backup-before-compact
// ============================================================================

/// `backup-before-compact`：`PreCompact` 时备份当前会话 JSONL 到 `<session>.backup`
/// （见 `hooks.md` §9）。
///
/// 从 `HookInput::extras.session_path` 取会话文件路径（Runtime 注入）；
/// 缺失时静默跳过（不阻断压缩）。
pub struct BackupBeforeCompact;

impl BackupBeforeCompact {
    /// 创建 `backup-before-compact` Hook。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for BackupBeforeCompact {
    fn default() -> Self {
        Self::new()
    }
}

impl Hook for BackupBeforeCompact {
    fn name(&self) -> &str {
        "backup-before-compact"
    }

    fn matcher(&self) -> &HookMatcher {
        static MATCHER: std::sync::OnceLock<HookMatcher> = std::sync::OnceLock::new();
        MATCHER.get_or_init(|| HookMatcher::for_events(vec![HookEvent::PreCompact]))
    }

    fn run(&self, input: HookInput) -> BoxFuture<'_, Result<HookOutput, HookError>> {
        Box::pin(async move {
            // 从 extras 取 session_path（Runtime 注入）
            let session_path = input.extras.get("session_path").and_then(|v| v.as_str());
            let Some(session_path) = session_path else {
                // 无 session_path，静默跳过
                return Ok(HookOutput::continue_());
            };
            let backup_path = format!("{session_path}.backup");
            // 异步复制文件
            match tokio::fs::copy(session_path, &backup_path).await {
                Ok(_) => Ok(HookOutput::continue_()),
                Err(e) => {
                    // 备份失败不阻断压缩，仅记 warn（通过 reason）
                    tracing::warn!(error = %e, "backup-before-compact 备份失败");
                    Ok(HookOutput::continue_())
                }
            }
        })
    }
}

// ============================================================================
// 6. test-on-stop
// ============================================================================

/// `test-on-stop`：`Stop` 时跑 `cargo test --quiet`，失败则注入上下文要求继续
/// （见 `hooks.md` §9）。
///
/// 检测到 `Cargo.toml` 时跑 `cargo test`；检测到 `package.json` 时跑 `npm test`；
/// 都没有则跳过。测试失败不阻断（返回 `Continue` + 注入失败摘要）。
pub struct TestOnStop;

impl TestOnStop {
    /// 创建 `test-on-stop` Hook。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for TestOnStop {
    fn default() -> Self {
        Self::new()
    }
}

impl Hook for TestOnStop {
    fn name(&self) -> &str {
        "test-on-stop"
    }

    fn matcher(&self) -> &HookMatcher {
        static MATCHER: std::sync::OnceLock<HookMatcher> = std::sync::OnceLock::new();
        MATCHER.get_or_init(|| HookMatcher::for_events(vec![HookEvent::Stop]))
    }

    fn run(&self, input: HookInput) -> BoxFuture<'_, Result<HookOutput, HookError>> {
        Box::pin(async move { run_test_on_stop(input).await })
    }
}

async fn run_test_on_stop(input: HookInput) -> Result<HookOutput, HookError> {
    // 检测项目类型
    let cwd = &input.cwd;
    let has_cargo = cwd.join("Cargo.toml").exists();
    let has_npm = cwd.join("package.json").exists();
    let (cmd, args): (&str, Vec<&str>) = if has_cargo {
        ("cargo", vec!["test", "--quiet"])
    } else if has_npm {
        ("npm", vec!["test"])
    } else {
        // 无项目文件，跳过
        return Ok(HookOutput::continue_());
    };

    let mut command = Command::new(cmd);
    command
        .args(&args)
        .current_dir(cwd)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for env_name in ENV_WHITELIST {
        if let Ok(value) = std::env::var(env_name) {
            command.env(env_name, value);
        }
    }

    let timeout = Duration::from_secs(DEFAULT_CMD_TIMEOUT_SEC);
    let output = match tokio::time::timeout(timeout, command.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Err(HookError::Internal(format!(
                "test-on-stop spawn `{cmd}` failed: {e}"
            )));
        }
        Err(_) => {
            return Err(HookError::Timeout {
                name: "test-on-stop".to_string(),
                timeout_sec: u32::try_from(timeout.as_secs()).unwrap_or(u32::MAX),
            });
        }
    };

    if output.status.success() {
        // 测试通过，不干预
        return Ok(HookOutput::continue_());
    }

    // 测试失败：注入失败摘要（截断），声明非指令（C-05 由 Runtime 包裹边界）
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut summary = String::new();
    if !stdout.is_empty() {
        summary.push_str("stdout:\n");
        summary.push_str(
            &stdout
                .chars()
                .take(MAX_OUTPUT_CHARS / 2)
                .collect::<String>(),
        );
    }
    if !stderr.is_empty() {
        if !summary.is_empty() {
            summary.push('\n');
        }
        summary.push_str("stderr:\n");
        summary.push_str(
            &stderr
                .chars()
                .take(MAX_OUTPUT_CHARS / 2)
                .collect::<String>(),
        );
    }
    let context = format!("test-on-stop: 测试失败，请修复后继续\n{summary}");
    Ok(HookOutput::inject(context))
}

// ============================================================================
// 工厂函数
// ============================================================================

/// 返回全部 6 个内置示例 Hook（见 `hooks.md` §9）。
///
/// 供 CLI/SDK 一键注册。每个 Hook 为 `Arc<dyn Hook>`，可直接 `register` 到
/// `HookRegistry`。
#[must_use]
pub fn builtin_hooks() -> Vec<std::sync::Arc<dyn Hook>> {
    vec![
        std::sync::Arc::new(FmtOnWrite::new()),
        std::sync::Arc::new(AutoApproveTests::new()),
        std::sync::Arc::new(BlockSecrets::new()),
        std::sync::Arc::new(GitStatusInject::new()),
        std::sync::Arc::new(BackupBeforeCompact::new()),
        std::sync::Arc::new(TestOnStop::new()),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use camino::Utf8PathBuf;
    use minicoding_core::hooks::HookDecision;
    use minicoding_core::model::ToolCall;
    use serde_json::json;

    fn make_tool_input(tool_name: &str, input: serde_json::Value) -> HookInput {
        let mut input_field = HookInput::new(
            HookEvent::PreToolUse,
            "test-session",
            1,
            Utf8PathBuf::from("."),
        );
        input_field.tool = Some(ToolCall {
            id: "call-1".to_string(),
            name: tool_name.to_string(),
            input,
        });
        input_field
    }

    // ---- block-secrets ----

    #[tokio::test]
    async fn block_secrets_detects_api_key() {
        let hook = BlockSecrets::new();
        let input = make_tool_input(
            "fs.write",
            json!({"path": "config.toml", "content": "api_key=sk-1234"}),
        );
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Deny);
        assert!(output.reason.as_deref().unwrap_or("").contains("api_key"));
    }

    #[tokio::test]
    async fn block_secrets_detects_password_case_insensitive() {
        let hook = BlockSecrets::new();
        let input = make_tool_input(
            "fs.write",
            json!({"path": "app.cfg", "content": "Password: hunter2"}),
        );
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Deny);
    }

    #[tokio::test]
    async fn block_secrets_allows_clean_content() {
        let hook = BlockSecrets::new();
        let input = make_tool_input(
            "fs.write",
            json!({"path": "main.rs", "content": "fn main() { println!(\"hello\"); }"}),
        );
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Continue);
    }

    #[tokio::test]
    async fn block_secrets_checks_fs_edit_new_string() {
        let hook = BlockSecrets::new();
        let input = make_tool_input(
            "fs.edit",
            json!({"path": "config.rs", "old_string": "x", "new_string": "secret=topsecret"}),
        );
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Deny);
    }

    #[tokio::test]
    async fn block_secrets_ignores_non_fs_tools() {
        let hook = BlockSecrets::new();
        // matcher 只匹配 fs.write/fs.edit，shell.run 不触发
        assert!(
            !hook
                .matcher()
                .matches(HookEvent::PreToolUse, Some("shell.run"))
        );
    }

    // ---- auto-approve-tests ----

    #[tokio::test]
    async fn auto_approve_cargo_test() {
        let hook = AutoApproveTests::new();
        let input = make_tool_input("shell.run", json!({"command": "cargo test --lib"}));
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn auto_approve_npm_test() {
        let hook = AutoApproveTests::new();
        let input = make_tool_input("shell.run", json!({"command": "npm test"}));
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn auto_approve_does_not_approve_rm() {
        let hook = AutoApproveTests::new();
        let input = make_tool_input("shell.run", json!({"command": "rm -rf /"}));
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Continue);
    }

    #[tokio::test]
    async fn auto_approve_only_matches_shell_run() {
        let hook = AutoApproveTests::new();
        // matcher 只匹配 shell.run
        assert!(
            !hook
                .matcher()
                .matches(HookEvent::PermissionRequest, Some("fs.write"))
        );
    }

    // ---- formatter_for_path ----

    #[test]
    fn formatter_for_rust() {
        let (cmd, args) = formatter_for_path("src/main.rs").expect("rust file");
        assert_eq!(cmd, "rustfmt");
        assert_eq!(args, vec!["--edition", "2024"]);
    }

    #[test]
    fn formatter_for_typescript() {
        let (cmd, args) = formatter_for_path("src/app.tsx").expect("tsx file");
        assert_eq!(cmd, "prettier");
        assert_eq!(args, vec!["--write"]);
    }

    #[test]
    fn formatter_for_unknown_extension() {
        assert!(formatter_for_path("README.md").is_none());
        assert!(formatter_for_path("Makefile").is_none());
    }

    // ---- is_test_command ----

    #[test]
    fn is_test_command_matches() {
        assert!(is_test_command("cargo test --lib"));
        assert!(is_test_command("npm test"));
        assert!(is_test_command("  cargo test  ")); // trim
        assert!(is_test_command("pytest test_foo.py"));
    }

    #[test]
    fn is_test_command_rejects_non_test() {
        assert!(!is_test_command("cargo build"));
        assert!(!is_test_command("rm -rf /"));
        assert!(!is_test_command("npm install"));
    }

    // ---- detect_secret_pattern ----

    #[test]
    fn detect_secret_various_patterns() {
        assert_eq!(detect_secret_pattern("api_key=sk-abc"), Some("api_key="));
        assert_eq!(detect_secret_pattern("PASSWORD: secret"), Some("password:"));
        assert_eq!(
            detect_secret_pattern("aws_access_key_id=AKIA..."),
            Some("aws_access_key_id=")
        );
    }

    #[test]
    fn detect_secret_no_false_positive() {
        assert!(detect_secret_pattern("fn main() {}").is_none());
        assert!(detect_secret_pattern("hello world").is_none());
    }

    // ---- builtin_hooks 工厂 ----

    #[test]
    fn builtin_hooks_returns_six() {
        let hooks = builtin_hooks();
        assert_eq!(hooks.len(), 6);
        let names: Vec<&str> = hooks.iter().map(|h| h.name()).collect();
        assert!(names.contains(&"fmt-on-write"));
        assert!(names.contains(&"auto-approve-tests"));
        assert!(names.contains(&"block-secrets"));
        assert!(names.contains(&"git-status-inject"));
        assert!(names.contains(&"backup-before-compact"));
        assert!(names.contains(&"test-on-stop"));
    }

    // ---- backup-before-compact ----

    #[tokio::test]
    async fn backup_before_compact_skips_without_session_path() {
        let hook = BackupBeforeCompact::new();
        let input = HookInput::new(HookEvent::PreCompact, "s", 1, Utf8PathBuf::from("."));
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Continue);
    }

    #[tokio::test]
    async fn backup_before_compact_copies_file() {
        let hook = BackupBeforeCompact::new();
        let tmp = tempfile::tempdir().expect("tempdir");
        let session_path = tmp.path().join("session.jsonl");
        std::fs::write(&session_path, "test content").expect("write");
        let mut input = HookInput::new(HookEvent::PreCompact, "s", 1, Utf8PathBuf::from("."));
        input.extras = json!({"session_path": session_path.to_str().unwrap()});
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Continue);
        // 验证备份文件存在
        let backup_path = format!("{}.backup", session_path.to_str().unwrap());
        assert!(std::path::Path::new(&backup_path).exists());
        let backup_content = std::fs::read_to_string(&backup_path).expect("read backup");
        assert_eq!(backup_content, "test content");
    }

    // ---- FmtOnWrite ----

    #[tokio::test]
    async fn fmt_on_write_matcher_matches_post_tool_use_fs_write_edit() {
        let hook = FmtOnWrite::new();
        assert!(
            hook.matcher()
                .matches(HookEvent::PostToolUse, Some("fs.write"))
        );
        assert!(
            hook.matcher()
                .matches(HookEvent::PostToolUse, Some("fs.edit"))
        );
        assert!(
            !hook
                .matcher()
                .matches(HookEvent::PreToolUse, Some("fs.write"))
        );
        assert!(
            !hook
                .matcher()
                .matches(HookEvent::PostToolUse, Some("shell.run"))
        );
    }

    #[tokio::test]
    async fn fmt_on_write_no_tool_returns_continue() {
        let hook = FmtOnWrite::new();
        let input = HookInput::new(HookEvent::PostToolUse, "s", 1, Utf8PathBuf::from("."));
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Continue);
    }

    #[tokio::test]
    async fn fmt_on_write_no_path_returns_continue() {
        let hook = FmtOnWrite::new();
        let input = make_tool_input("fs.write", json!({"content": "x"}));
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Continue);
    }

    #[tokio::test]
    async fn fmt_on_write_unsupported_extension_returns_continue() {
        let hook = FmtOnWrite::new();
        let input = make_tool_input("fs.write", json!({"path": "README.md", "content": "# Hi"}));
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Continue);
    }

    #[tokio::test]
    async fn fmt_on_write_rs_file_returns_continue_or_internal_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("main.rs"), "fn main() {}\n").expect("write");
        let hook = FmtOnWrite::new();
        let mut input = HookInput::new(
            HookEvent::PostToolUse,
            "s",
            1,
            Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("utf8"),
        );
        input.tool = Some(ToolCall {
            id: "c1".to_string(),
            name: "fs.write".to_string(),
            input: json!({"path": "main.rs"}),
        });
        let result = hook.run(input).await;
        match result {
            Ok(output) => assert_eq!(output.decision, HookDecision::Continue),
            Err(HookError::Internal(msg)) => assert!(
                msg.contains("rustfmt"),
                "期望错误信息含 rustfmt，实际: {msg}"
            ),
            Err(e) => panic!("意外错误: {e:?}"),
        }
    }

    // ---- AutoApproveTests 补充 ----

    #[tokio::test]
    async fn auto_approve_matcher_only_matches_shell_run_permission_request() {
        let hook = AutoApproveTests::new();
        assert!(
            hook.matcher()
                .matches(HookEvent::PermissionRequest, Some("shell.run"))
        );
        assert!(
            !hook
                .matcher()
                .matches(HookEvent::PermissionRequest, Some("fs.write"))
        );
        assert!(
            !hook
                .matcher()
                .matches(HookEvent::PreToolUse, Some("shell.run"))
        );
    }

    #[tokio::test]
    async fn auto_approve_no_tool_returns_continue() {
        let hook = AutoApproveTests::new();
        let input = HookInput::new(HookEvent::PermissionRequest, "s", 1, Utf8PathBuf::from("."));
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Continue);
    }

    #[tokio::test]
    async fn auto_approve_no_command_returns_continue() {
        let hook = AutoApproveTests::new();
        let input = make_tool_input("shell.run", json!({"path": "x"}));
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Continue);
    }

    #[tokio::test]
    async fn auto_approve_pnpm_yarn_pytest_prefixes() {
        let hook = AutoApproveTests::new();
        for cmd in ["pnpm test", "yarn test", "pytest test_foo.py"] {
            let input = make_tool_input("shell.run", json!({"command": cmd}));
            let output = hook.run(input).await.expect("should succeed");
            assert_eq!(output.decision, HookDecision::Allow, "cmd: {cmd}");
        }
    }

    #[tokio::test]
    async fn auto_approve_trimmed_command_matches() {
        let hook = AutoApproveTests::new();
        // 命令含前导空白也应匹配（is_test_command 内部 trim_start）
        let input = make_tool_input("shell.run", json!({"command": "  cargo test"}));
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Allow);
    }

    // ---- BlockSecrets 补充 ----

    #[tokio::test]
    async fn block_secrets_no_tool_returns_continue() {
        let hook = BlockSecrets::new();
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("."));
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Continue);
    }

    #[tokio::test]
    async fn block_secrets_no_content_returns_continue() {
        let hook = BlockSecrets::new();
        let input = make_tool_input("fs.write", json!({"path": "x.rs"}));
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Continue);
    }

    #[tokio::test]
    async fn block_secrets_detects_various_patterns() {
        let hook = BlockSecrets::new();
        let cases = [
            ("apikey=xxx", "apikey="),
            ("apikey: xxx", "apikey:"),
            ("passwd=xxx", "passwd="),
            ("passwd: xxx", "passwd:"),
            ("secret=xxx", "secret="),
            ("secret: xxx", "secret:"),
            ("access_token=xxx", "access_token="),
            ("access_token: xxx", "access_token:"),
            ("private_key=xxx", "private_key="),
            ("private_key: xxx", "private_key:"),
            ("aws_secret_access_key=xxx", "aws_secret_access_key="),
        ];
        for (content, pattern) in cases {
            let input = make_tool_input("fs.write", json!({"path": "cfg", "content": content}));
            let output = hook.run(input).await.expect("should succeed");
            assert_eq!(output.decision, HookDecision::Deny, "content: {content}");
            assert!(
                output.reason.as_deref().unwrap_or("").contains(pattern),
                "期望 reason 含 {pattern}，content: {content}"
            );
        }
    }

    #[tokio::test]
    async fn block_secrets_case_insensitive() {
        let hook = BlockSecrets::new();
        let input = make_tool_input(
            "fs.write",
            json!({"path": "cfg", "content": "API_KEY=topsecret"}),
        );
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Deny);
    }

    // ---- GitStatusInject ----

    #[tokio::test]
    async fn git_status_inject_matcher_only_matches_session_start() {
        let hook = GitStatusInject::new();
        assert!(hook.matcher().matches(HookEvent::SessionStart, None));
        assert!(!hook.matcher().matches(HookEvent::Stop, None));
        assert!(
            !hook
                .matcher()
                .matches(HookEvent::PreToolUse, Some("fs.write"))
        );
    }

    #[tokio::test]
    async fn git_status_inject_non_git_dir_returns_continue_no_inject() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hook = GitStatusInject::new();
        let input = HookInput::new(
            HookEvent::SessionStart,
            "s",
            1,
            Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("utf8"),
        );
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Continue);
        assert!(output.inject_context.is_none());
    }

    #[tokio::test]
    async fn git_status_inject_in_git_repo_injects_context() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // git init 创建仓库；不可用时跳过（不连真实服务原则的本地工具例外）
        let init = std::process::Command::new("git")
            .arg("init")
            .current_dir(tmp.path())
            .output();
        let init_ok = matches!(init, Ok(o) if o.status.success());
        if !init_ok {
            eprintln!("skipping: git not available or init failed");
            return;
        }
        // 创建 untracked 文件使 git status --short 有输出
        std::fs::write(tmp.path().join("untracked.txt"), "test").expect("write");
        let hook = GitStatusInject::new();
        let input = HookInput::new(
            HookEvent::SessionStart,
            "s",
            1,
            Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("utf8"),
        );
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Continue);
        let ctx = output.inject_context.as_deref().expect("应有注入上下文");
        assert!(ctx.contains("git status"));
    }

    // ---- BackupBeforeCompact matcher ----

    #[tokio::test]
    async fn backup_before_compact_matcher_only_matches_pre_compact() {
        let hook = BackupBeforeCompact::new();
        assert!(hook.matcher().matches(HookEvent::PreCompact, None));
        assert!(!hook.matcher().matches(HookEvent::PostCompact, None));
        assert!(!hook.matcher().matches(HookEvent::SessionStart, None));
    }

    // ---- TestOnStop ----

    #[tokio::test]
    async fn test_on_stop_matcher_only_matches_stop() {
        let hook = TestOnStop::new();
        assert!(hook.matcher().matches(HookEvent::Stop, None));
        assert!(!hook.matcher().matches(HookEvent::SessionStart, None));
        assert!(!hook.matcher().matches(HookEvent::SubagentStop, None));
    }

    #[tokio::test]
    async fn test_on_stop_no_project_returns_continue() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hook = TestOnStop::new();
        let input = HookInput::new(
            HookEvent::Stop,
            "s",
            1,
            Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("utf8"),
        );
        let output = hook.run(input).await.expect("should succeed");
        assert_eq!(output.decision, HookDecision::Continue);
    }

    // ---- detect_secret_pattern 补充 ===

    #[test]
    fn detect_secret_all_patterns() {
        assert_eq!(detect_secret_pattern("apikey=xxx"), Some("apikey="));
        assert_eq!(detect_secret_pattern("apikey: xxx"), Some("apikey:"));
        assert_eq!(detect_secret_pattern("passwd=xxx"), Some("passwd="));
        assert_eq!(detect_secret_pattern("passwd: xxx"), Some("passwd:"));
        assert_eq!(detect_secret_pattern("secret=xxx"), Some("secret="));
        assert_eq!(detect_secret_pattern("secret: xxx"), Some("secret:"));
        assert_eq!(
            detect_secret_pattern("access_token=xxx"),
            Some("access_token=")
        );
        assert_eq!(
            detect_secret_pattern("access_token: xxx"),
            Some("access_token:")
        );
        assert_eq!(
            detect_secret_pattern("private_key=xxx"),
            Some("private_key=")
        );
        assert_eq!(
            detect_secret_pattern("private_key: xxx"),
            Some("private_key:")
        );
    }

    #[test]
    fn detect_secret_case_insensitive() {
        assert_eq!(detect_secret_pattern("API_KEY=topsecret"), Some("api_key="));
        assert_eq!(
            detect_secret_pattern("PASSWORD=mypassword"),
            Some("password=")
        );
        assert_eq!(detect_secret_pattern("Secret=xxx"), Some("secret="));
    }
}
