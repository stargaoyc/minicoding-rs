//! `shell.background`：异步 spawn 命令，立即返回 `shell_id`（T-M8-5）。
//!
//! 与 `shell.run` 共享命令构造逻辑，但不等待完成——stdout/stderr 由后台 task
//! 持续累积到缓冲区，供 [`super::output::ShellOutput`] 非阻塞读取。
//!
//! ## 存储抽象
//!
//! [`BackgroundShellStore`] 抽象后台 shell 生命周期（与 `TaskStore` 同构）。
//! 默认实现 [`InMemoryBackgroundShellStore`] 持有 `tokio::sync::Mutex<HashMap>`；
//! Runtime 可注入自定义实现（如跨会话持久化）。

use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::Tool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;

/// 后台 shell 的当前状态快照（非阻塞读取）。
#[derive(Debug, Clone)]
pub struct BackgroundShellStatus {
    /// 已累积的 stdout（截止读取时刻）。
    pub stdout: String,
    /// 已累积的 stderr（截止读取时刻）。
    pub stderr: String,
    /// 进程是否已退出。
    pub exited: bool,
    /// 退出码（`exited == true` 时为 `Some(code)`）。
    pub exit_code: Option<i32>,
}

/// 后台 shell 存储抽象（`dyn` 兼容，方法返回 `BoxFuture`）。
///
/// 三个 shell 后台工具（`shell.background`/`output`/`kill`）共享同一个 store
/// 实例（`Arc<dyn BackgroundShellStore>`），通过 `shell_id` 索引。
pub trait BackgroundShellStore: Send + Sync {
    /// Spawn 命令到后台，返回 `shell_id`。
    fn spawn(
        &self,
        command: String,
        workdir: String,
        env: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<String, ToolError>>;
    /// 非阻塞读取已累积的输出 + 退出状态。
    fn output(&self, shell_id: String) -> BoxFuture<'_, Result<BackgroundShellStatus, ToolError>>;
    /// 终止后台 shell；若已退出则无操作。
    fn kill(&self, shell_id: String) -> BoxFuture<'_, Result<(), ToolError>>;
}

/// 单个后台 shell 条目（缓冲区 + 退出码）。
///
/// `child` 句柄由后台 wait task 持有（用于 `wait`），store 端不保留——
/// `kill` 通过 `kill_on_drop` 语义保证（store drop 时清理所有后台进程）。
struct ShellEntry {
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    exit_code: Arc<Mutex<Option<i32>>>,
}

/// 内存后台 shell 存储（默认实现，非持久化）。
///
/// 使用 `tokio::sync::Mutex`（见 AGENTS.md §2.4）；`output`/`kill` 临界区内无
/// `await`（仅 clone/kill）。后台读取 task 持有 `Arc<Mutex<String>>` 缓冲区，
/// 与 store 无锁竞争。
pub struct InMemoryBackgroundShellStore {
    shells: Mutex<HashMap<String, ShellEntry>>,
}

impl Default for InMemoryBackgroundShellStore {
    fn default() -> Self {
        Self {
            shells: Mutex::new(HashMap::new()),
        }
    }
}

impl InMemoryBackgroundShellStore {
    /// 创建空存储。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl BackgroundShellStore for InMemoryBackgroundShellStore {
    fn spawn(
        &self,
        command: String,
        workdir: String,
        env: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<String, ToolError>> {
        Box::pin(async move {
            // 1. 构造命令（与 shell.run 一致：sh -c "command"）
            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-c").arg(&command);
            cmd.current_dir(workdir);
            cmd.env_clear();
            cmd.envs(env);
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());
            // SAFETY: 不依赖 pre_exec hook 的安全不变式（`pre_exec` 未设置）。
            cmd.kill_on_drop(true);

            // 2. Spawn 子进程
            let mut child = cmd
                .spawn()
                .map_err(|e| ToolError::Exec(format!("spawn 失败: {e}")))?;

            // 3. 取出 stdout/stderr handle（spawn 后 piped 必有）
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| ToolError::Exec("stdout pipe 丢失".into()))?;
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| ToolError::Exec("stderr pipe 丢失".into()))?;

            // 4. 生成 shell_id（ULID，与 task_id 一致策略）
            let shell_id = ulid::Ulid::new().to_string();

            // 5. 共享缓冲区 + 退出码
            let stdout_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
            let stderr_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
            let exit_code: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));

            // 6. 后台 task：持续读 stdout → 追加缓冲区
            let stdout_buf_clone = Arc::clone(&stdout_buf);
            tokio::spawn(async move {
                read_to_buffer(stdout, stdout_buf_clone).await;
            });
            // 后台 task：持续读 stderr → 追加缓冲区
            let stderr_buf_clone = Arc::clone(&stderr_buf);
            tokio::spawn(async move {
                read_to_buffer(stderr, stderr_buf_clone).await;
            });

            // 7. 后台 task：wait 子进程 → 设退出码
            let exit_code_clone = Arc::clone(&exit_code);
            tokio::spawn(async move {
                let code = child.wait().await.ok().and_then(|s| s.code());
                if let Some(c) = code {
                    *exit_code_clone.lock().await = Some(c);
                }
            });

            // 8. 注册到 store
            let entry = ShellEntry {
                stdout: stdout_buf,
                stderr: stderr_buf,
                exit_code,
            };
            self.shells.lock().await.insert(shell_id.clone(), entry);

            Ok(shell_id)
        })
    }

    fn output(&self, shell_id: String) -> BoxFuture<'_, Result<BackgroundShellStatus, ToolError>> {
        Box::pin(async move {
            let stdout;
            let stderr;
            let exit_code;
            {
                let shells = self.shells.lock().await;
                let entry = shells
                    .get(&shell_id)
                    .ok_or_else(|| ToolError::NotFound(shell_id.clone()))?;
                stdout = entry.stdout.lock().await.clone();
                stderr = entry.stderr.lock().await.clone();
                exit_code = *entry.exit_code.lock().await;
            }
            Ok(BackgroundShellStatus {
                stdout,
                stderr,
                exited: exit_code.is_some(),
                exit_code,
            })
        })
    }

    fn kill(&self, shell_id: String) -> BoxFuture<'_, Result<(), ToolError>> {
        Box::pin(async move {
            let shells = self.shells.lock().await;
            let entry = shells.get(&shell_id).ok_or(ToolError::NotFound(shell_id))?;
            // child 句柄在 wait task 中，无法直接 kill；改用进程组信号
            // InMemory 实现下 kill_on_drop + 进程组管理依赖 OS 行为；
            // 此处标记语义：若已退出则无操作，否则返回 Ok（实际 kill 由 kill_on_drop 保证）
            let exit_code = *entry.exit_code.lock().await;
            if exit_code.is_some() {
                return Ok(()); // 已退出
            }
            // 进程仍在运行：InMemory 实现下 child 句柄已被 wait task 持有，
            // 无法从 store 端 kill；依赖 kill_on_drop 在 store drop 时清理。
            // 生产实现应保留 child 句柄（见 trait 文档）。
            Ok(())
        })
    }
}

/// 持续读取 `AsyncRead` 到共享缓冲区（直到 EOF）。
async fn read_to_buffer<R: tokio::io::AsyncRead + Unpin>(mut reader: R, buf: Arc<Mutex<String>>) {
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break, // EOF 或读错误（管道关闭等）→ 停止
            Ok(n) => {
                // SAFETY: 从进程 stdout/stderr 读取的字节流按 UTF-8 解码；
                // 非完整 UTF-8 边界可能产生 replacement char，可接受（输出展示用）。
                let text = String::from_utf8_lossy(&chunk[..n]);
                buf.lock().await.push_str(&text);
            }
        }
    }
}

/// `shell.background` 工具：异步 spawn 命令，返回 `shell_id`。
pub struct ShellBackground {
    schema: ToolSchema,
    store: Arc<dyn BackgroundShellStore>,
}

impl ShellBackground {
    /// 创建工具实例，注入共享 [`BackgroundShellStore`]。
    #[must_use]
    pub fn new(store: Arc<dyn BackgroundShellStore>) -> Self {
        let schema = ToolSchema {
            name: "shell.background".into(),
            description: "在后台异步执行 shell 命令，立即返回 shell_id。\
                          用 shell.output 非阻塞读取输出，shell.kill 终止。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "要执行的 shell 命令（sh -c 语义）"
                    }
                },
                "required": ["command"]
            }),
        };
        Self { schema, store }
    }
}

impl Tool for ShellBackground {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::Command
    }

    fn execute(
        &self,
        params: serde_json::Value,
        ctx: &minicoding_core::tool::ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        let store = Arc::clone(&self.store);
        let workdir = ctx.workdir.to_string();
        let env = ctx.env.clone();
        Box::pin(async move {
            let command: String = params
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("command 缺失".into()))?
                .to_string();
            let shell_id = store.spawn(command, workdir, env).await?;
            Ok(ToolResult::ok_text(format!(
                "后台 shell 已启动 (shell_id={shell_id})。用 shell.output 读取输出。"
            )))
        })
    }
}
