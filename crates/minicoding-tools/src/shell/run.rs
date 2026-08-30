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

// 子进程 env 白名单（C-04）已上移 `minicoding_core::tool::SAFE_ENV_WHITELIST`
// 单一事实来源（2026-08-23 审查 §6-P1：与 `ToolContext::env` 共用）。

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
            // R8 TL-5：`timeout_ms=0` 导致立即超时 kill（schema 允许 minimum 0），
            // 可被 LLM 用作 DoS——钳位到 1ms 最小非零值。
            let timeout = args
                .timeout_ms
                .map(Duration::from_millis)
                .map_or(default_timeout, |t| t.min(default_timeout))
                .max(Duration::from_millis(1));

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
            for name in minicoding_core::tool::SAFE_ENV_WHITELIST {
                if let Ok(value) = std::env::var(name) {
                    command.env(name, value);
                }
            }

            // M4：OS 沙箱（第二道防线，C-22）。`apply` 在 spawn 前注入 landlock/
            // seatbelt 的 pre_exec 钩子（Linux/macOS），或设置 CREATE_SUSPENDED（Windows）。
            // 未注入驱动/策略时跳过（兼容测试）。
            let has_sandbox = sandbox_driver.is_some() && sandbox_policy.is_some();
            // R9 SANDBOX-2：apply 返回 SpawnHandle（Windows 携带策略），
            // 随后的 post_spawn 消费同一句柄——消除并发 spawn 策略错配。
            let mut spawn_handle = None;
            if let (Some(driver), Some(policy)) = (sandbox_driver.as_ref(), sandbox_policy.as_ref())
            {
                let span = tracing::debug_span!(
                    "sandbox.apply",
                    otel.name = span_name::SANDBOX_APPLY,
                    driver = driver.id(),
                );
                let _enter = span.enter();
                spawn_handle = Some(driver.apply(policy, command.as_std_mut()).map_err(|e| {
                    // 沙箱 apply 失败（如 landlock ruleset 构建失败）视为执行错误，
                    // 由 Runtime 的 denial detector 进一步识别是否为 denial。
                    ToolError::Exec(format!("sandbox apply failed: {e}"))
                })?);
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
                let mut handle = spawn_handle.take().unwrap_or_default();
                driver.post_spawn(&mut handle, pid).map_err(|e| {
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
            // R8 TL-1：`Child::id()` 在 `wait()` 完成后返回 None——须在 wait 前
            // 捕获 pid，供 killpg 清理后台持管孙进程（见 `kill_process_tree`）。
            let child_pid = child.id();

            // C-07/S9：超时后先 killpg 整树再 start_kill 兜底。
            // 不用 wait_with_output（消耗所有权）：保留 child 句柄以便取 pid 杀组。
            let status = if let Ok(r) = tokio::time::timeout(timeout, child.wait()).await {
                r.map_err(ToolError::Io)?
            } else {
                kill_process_tree(child_pid).await;
                let _ = child.start_kill();
                // 等 pipe 读任务收尾（SIGKILL 后管道 EOF；setsid 逃逸由
                // drain_pipes 的宽限+强杀兜底，见 R8 TL-1）
                let _ = drain_pipes(stdout_task, child_pid).await;
                let _ = drain_pipes(stderr_task, child_pid).await;
                return Err(ToolError::Timeout(timeout));
            };
            // R8 TL-1 修复：子进程已退出，但后台孙进程（如 `setsid sleep &`）
            // 可能仍持管道写端致 `read` 永不 EOF（C-07 永久挂起）。宽限排空 +
            // 超时强杀兜底后收割已读部分。
            let out_bytes = drain_pipes(stdout_task, child_pid).await;
            let err_bytes = drain_pipes(stderr_task, child_pid).await;

            let stdout = String::from_utf8_lossy(&out_bytes).into_owned();
            let stderr = String::from_utf8_lossy(&err_bytes).into_owned();
            let mut combined = stdout;
            if !stderr.is_empty() {
                if !combined.is_empty() {
                    combined.push_str("\n--- stderr ---\n");
                }
                combined.push_str(&stderr);
            }

            // S19/C-04：敏感模式脱敏——子进程输出中可能含 `.env` 泄露、
            // API key 赋值等（fs.read 有文件名级脱敏，shell 是旁路，此处补齐）。
            // 模式保守：仅匹配明确赋值/声明形态，避免误杀普通输出。
            let combined = redact_secrets(&combined);

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

/// 子进程退出后管道排空宽限（R8 TL-1 修复）。
///
/// 后台孙进程（如 `sh -c 'setsid sleep 100 & echo done'`）可持管道写端不关，
/// `read` 永不 EOF——无宽限则 `drain_pipes` 永久挂起（C-07 资源不可耗尽）。
/// 正常命令管道随子进程退出立即 EOF，本宽限不增加常规延迟。
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(3);

/// 收割管道读取任务：先给 [`PIPE_DRAIN_GRACE`] 排空；超时（后台进程持管）
/// 则 killpg 整树强杀强制关闭写端，再等待任务返回已读部分。
async fn drain_pipes(
    mut task: tokio::task::JoinHandle<std::vec::Vec<u8>>,
    child_pid: Option<u32>,
) -> std::vec::Vec<u8> {
    match tokio::time::timeout(PIPE_DRAIN_GRACE, &mut task).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => Vec::new(),
        Err(_) => {
            kill_process_tree(child_pid).await;
            task.await.unwrap_or_default()
        }
    }
}

/// Unix：killpg 整树强杀——子进程 spawn 前经 `setpgid(0,0)` 自成组长，
/// `pgid == pid`，向整组发 SIGKILL 可清理后台孙进程。
#[cfg(unix)]
async fn kill_process_tree(child_pid: Option<u32>) {
    if let Some(pid) = child_pid {
        // SAFETY: killpg 仅向进程组发信号，不触碰子进程内存；pgid==pid
        // 由 pre_exec 内 setpgid(0,0) 保证；pid 来自 tokio Child（存在性已验证）。
        let _ = tokio::task::spawn_blocking(move || unsafe {
            libc::killpg(i32::try_from(pid).unwrap_or(-1), libc::SIGKILL)
        })
        .await;
    }
}

/// Windows：无 killpg 语义；由 Job Object `KILL_ON_JOB_CLOSE` 与
/// `kill_on_drop`/`start_kill` 兜底（R8 文档化残余：孙进程清理异步）。
#[cfg(not(unix))]
async fn kill_process_tree(_child_pid: Option<u32>) {}

/// S19/C-04：shell 输出中的常见凭证形态脱敏（PTM-14：值边界精确替换）。
///
/// 两类命中：
/// 1. `KEY=value` / `"KEY": value` 形式且 KEY 含敏感关键词
///    （`PASSWORD`/`SECRET`/`TOKEN`/`API_KEY`/`PRIVATE_KEY`/…）——仅替换
///    **值片段**为 `***`，行内其余内容（日志上下文、其他字段）保留；
/// 2. 行内出现知名 key 前缀（sk-/ghp_/AKIA/xoxb-/github_pat_）——仅替换
///    前缀起始的**连续 token**（到空白/引号/行尾为止）为 `***`。
///
/// 此前整行吞 `[REDACTED]`：混有正常日志的行有效信息一并丢失，LLM 排障
/// 能力受损（PTM-14）。保守性说明：值边界按空白/引号截断，含空格的奇异
/// 凭证可能残留尾部——安全方向残余风险低于误杀面，可接受。
fn redact_secrets(text: &str) -> String {
    const SENSITIVE_KEYS: &[&str] = &[
        "password",
        "secret",
        "token",
        "api_key",
        "apikey",
        "private_key",
        "access_key",
    ];
    const KEY_PREFIXES: &[&str] = &["sk-", "ghp_", "github_pat_", "AKIA", "xoxb-"];

    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        out.push_str(&redact_line_precise(line, SENSITIVE_KEYS, KEY_PREFIXES));
        out.push('\n');
    }
    // 原文无尾部换行则不补
    if !text.ends_with('\n') {
        out.pop();
    }
    out
}

/// 单行值边界精确脱敏（PTM-14）：扫描赋值对与前缀 token，逐段拼接。
fn redact_line_precise(line: &str, sensitive_keys: &[&str], key_prefixes: &[&str]) -> String {
    let bytes = line.as_bytes();
    let lower = line.to_ascii_lowercase();
    let mut edits: Vec<(usize, usize)> = Vec::new(); // [start, end) 待替换区段

    let byte_len = |s: &str| s.len();
    for key in sensitive_keys {
        // 匹配 key 出现位置（大小写不敏感），随后要求 `=` 或 `:` 分隔符
        let mut search_from = 0usize;
        while let Some(rel) = lower[search_from..].find(key) {
            let k_start = search_from + rel;
            let k_end = k_start + byte_len(key);
            // 分隔符：跳过 key 后的空白与闭引号（JSON `"key":` 形态），找 '=' 或 ':'
            let mut cursor = k_end;
            while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t' | b'"' | b'\'') {
                cursor += 1;
            }
            // 值起点：分隔符后的第一个非空白字符（可能带引号）
            if cursor < bytes.len() && (bytes[cursor] == b'=' || bytes[cursor] == b':') {
                let mut v_start = cursor + 1;
                while v_start < bytes.len() && (bytes[v_start] == b' ' || bytes[v_start] == b'\t') {
                    v_start += 1;
                }
                // 值终点：行尾或下一个空白（引号包裹则到闭引号）；无值时回退 k_end
                let v_end = if v_start < bytes.len() {
                    match bytes[v_start] {
                        q @ (b'"' | b'\'') => {
                            let close = line[v_start + 1..]
                                .find(q as char)
                                .map_or(line.len(), |rel| v_start + 1 + rel + 1);
                            close.min(line.len())
                        }
                        _ => line[v_start..]
                            .find(char::is_whitespace)
                            .map_or(line.len(), |rel| v_start + rel),
                    }
                } else {
                    v_start
                };
                if v_end > v_start {
                    edits.push((v_start, v_end));
                }
                search_from = v_end.max(k_end);
            } else {
                search_from = k_end;
            }
        }
    }

    // 知名前缀 token：从前缀起至空白/引号/行尾
    for prefix in key_prefixes {
        let mut search_from = 0usize;
        while let Some(rel) = lower[search_from..].find(&prefix.to_ascii_lowercase()) {
            let p_start = search_from + rel;
            // 前缀须处于 token 起点（前一字符为空白/行首/引号），避免误伤 URL 中段
            let at_token_start = p_start == 0
                || (bytes[p_start - 1] as char).is_whitespace()
                || bytes[p_start - 1] == b'"'
                || bytes[p_start - 1] == b'\'';
            if at_token_start {
                let p_end = line[p_start..]
                    .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                    .map_or(line.len(), |rel| p_start + rel);
                if p_end > p_start {
                    edits.push((p_start, p_end));
                }
                search_from = p_end.max(p_start + byte_len(prefix));
            } else {
                search_from = p_start + byte_len(prefix);
            }
        }
    }

    if edits.is_empty() {
        return line.to_string();
    }
    edits.sort_unstable();
    let merged = merge_edits(edits);
    let mut out = String::with_capacity(line.len());
    let mut cursor = 0usize;
    for (start, end) in merged {
        out.push_str(&line[cursor..start]);
        out.push_str("***");
        cursor = end;
    }
    out.push_str(&line[cursor..]);
    out
}

/// R8 TL-2：重叠区段并集合并（原 `dedup_by` 丢弃后区间，后区间延伸超出前区间
/// 时超出部分漏脱敏）。输入需已按起点排序；重叠区间取覆盖范围最大端。
fn merge_edits(edits: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(edits.len());
    for (start, end) in edits {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
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

    /// R8 TL-5：timeout_ms=0 被钳位到 1ms，不立即超时——schema 允许 minimum=0，
    /// 但 0 毫秒超时在 即 kill 前 pipe 采集无意义，且可被 LLM 用作 DoS。
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_zero_clamped_to_nonzero() {
        let (_tmp, workdir) = make_workdir();
        let mut ctx = ToolContext::new(workdir, "test".to_string());
        ctx.timeout = std::time::Duration::from_millis(500);
        let tool = ShellRun::new();
        // timeout_ms=0 的 sleep 不应立即超时（执行本身 ~0，钳位 1ms 后管道排空宽限）
        let result = tool
            .execute(json!({"command": "echo ok", "timeout_ms": 0}), &ctx)
            .await;
        assert!(
            result.is_ok(),
            "timeout_ms=0 应被钳位而非立即超时: {result:?}"
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
        // killpg 后短暂等待，确认无 `sleep 60` 残留。
        // 真实等待：断言对象是 OS 进程表（killpg 异步生效），虚拟时钟无法
        // 加速真实子进程退出（start_paused 不适用）
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
    #[cfg(unix)]
    #[tokio::test]
    async fn shell_output_redacts_api_keys() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = ShellRun::new();
        let result = tool
            .execute(json!({"command": "echo sk-test-key-12345"}), &ctx)
            .await
            .expect("run ok");
        let text = text_of(&result);
        assert!(text.contains("***"), "API key 应被脱敏: {text}");
        assert!(!text.contains("sk-test-key-12345"), "原始 key 不应出现");
    }

    // ===== PTM-14：值边界精确替换（不再整行吞） =====

    #[test]
    fn redact_preserves_line_context_around_value() {
        use super::redact_secrets;
        let input = "2026-08-25 INFO request id=42 API_KEY=sk-secret-value-xyz done in 3ms";
        let out = redact_secrets(input);
        assert!(out.contains("id=42"), "行内其他字段应保留: {out}");
        assert!(out.contains("done in 3ms"), "日志尾应保留: {out}");
        assert!(!out.contains("sk-secret-value"), "值应替换: {out}");
        assert!(out.contains("API_KEY=***"), "键名与分隔符应保留: {out}");
    }

    #[test]
    fn redact_json_field_and_quoted_value() {
        use super::redact_secrets;
        let out = redact_secrets("{\"api_key\": \"supersecret123\", \"level\": \"info\"}");
        assert!(!out.contains("supersecret123"), "{out}");
        assert!(
            out.contains("\"level\": \"info\""),
            "无关 JSON 字段应保留: {out}"
        );
    }

    #[test]
    fn redact_prefix_token_only_not_whole_line() {
        use super::redact_secrets;
        let out = redact_secrets("token ghp_abcdef123 served 200 OK");
        assert!(out.contains("served 200 OK"), "上下文应保留: {out}");
        assert!(!out.contains("ghp_abcdef"), "{out}");
    }

    #[test]
    fn redact_non_sensitive_lines_untouched() {
        use super::redact_secrets;
        let input = "plain log line\ntokens=42 count\nno secrets here";
        assert_eq!(redact_secrets(input), input, "普通行不应改动");
    }

    // ===== R8 TL-2：重叠区段并集合并 =====

    #[test]
    fn merge_edits_takes_union_of_overlapping_ranges() {
        // 后区间延伸超出前区间（修复前 dedup_by 丢弃后区间 → 延伸部分泄漏）
        assert_eq!(super::merge_edits(vec![(5, 15), (10, 20)]), vec![(5, 20)]);
        // 相邻相接合并
        assert_eq!(
            super::merge_edits(vec![(0, 5), (5, 8), (12, 20)]),
            vec![(0, 8), (12, 20)]
        );
        // 不相交保持
        assert_eq!(
            super::merge_edits(vec![(0, 3), (4, 6)]),
            vec![(0, 3), (4, 6)]
        );
    }

    #[test]
    fn redact_quoted_value_with_prefix_fully_redacts() {
        use super::redact_line_precise;
        let out = redact_line_precise("export TOKEN=\"sk-abcdefghijklmnop\"", &["token"], &["sk-"]);
        assert_eq!(out, "export TOKEN=***", "{out}");
    }

    // ===== R8 TL-1：后台进程持管道不挂起（C-07） =====

    #[cfg(unix)]
    #[tokio::test]
    async fn background_holding_pipe_does_not_hang() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = ShellRun::new();
        let started = std::time::Instant::now();
        // 子进程（sh）秒退，但后台 `sleep 30` 持 stdout 写端不关——
        // 修复前 stdout_task.await 永久挂起；修复后宽限+杀树兜底返回
        let result = tool
            .execute(json!({"command": "sleep 30 & echo done"}), &ctx)
            .await
            .expect("run ok");
        assert!(
            text_of(&result).contains("done"),
            "前台输出应已捕获: {}",
            text_of(&result)
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "后台持管不应阻塞工具返回，实际耗时 {:?}",
            started.elapsed()
        );
        // 后台进程组应已被 killpg 清理（短等待让信号生效）
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg("ps -eo pid,cmd | grep 'sleep 30' | grep -v grep | wc -l")
            .output()
            .expect("ps");
        let count = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(count, "0", "后台持管进程应被清理，实际残留 {count}");
    }
}
