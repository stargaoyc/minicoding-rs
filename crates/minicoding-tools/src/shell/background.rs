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
//!
//! ## 条目回收（T-8，2026-08-25 审查）
//!
//! store 此前只增不减：长会话中每次 `shell.background` 都累积一条
//! `ShellEntry`（含输出缓冲 Arc），永不回收导致内存缓慢泄漏。现设
//! [`MAX_TRACKED_SHELLS`] 硬上限（128 条）：注册新条目前若已达上限，淘汰
//! **最旧的已完成**条目；若无已完成条目则淘汰全局最旧——保证上限硬约束成立。
//! 被淘汰条目的 `shell_id` 随即失效（`shell.output`/`kill` 返回 `NotFound`）；
//! 运行中进程经 `kill_on_drop` 终止，不会残留孤儿进程。

use minicoding_core::metrics;
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::otel::span_name;
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{RenderIntent, Tool};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;

/// 后台 shell 存储条目上限（T-8，2026-08-25 审查）：超出后淘汰最旧条目。
const MAX_TRACKED_SHELLS: usize = 128;

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
/// 沙箱注入参数（`shell.run` 同款：spawn 前 `apply`，spawn 后 `post_spawn`）。
pub struct SpawnSandbox {
    pub driver: std::sync::Arc<dyn minicoding_core::sandbox::SandboxDriver>,
    pub policy: minicoding_core::sandbox::SandboxPolicy,
}

pub trait BackgroundShellStore: Send + Sync {
    /// Spawn 命令到后台，返回 `shell_id`。`max_output_bytes` 为每路输出缓冲上限。
    fn spawn(
        &self,
        command: String,
        workdir: String,
        env: HashMap<String, String>,
        sandbox: Option<SpawnSandbox>,
        max_output_bytes: usize,
    ) -> BoxFuture<'_, Result<String, ToolError>>;
    /// 非阻塞读取已累积的输出 + 退出状态。
    fn output(&self, shell_id: String) -> BoxFuture<'_, Result<BackgroundShellStatus, ToolError>>;
    /// 终止后台 shell；若已退出则无操作。
    fn kill(&self, shell_id: String) -> BoxFuture<'_, Result<(), ToolError>>;
}

/// 单个后台 shell 条目（缓冲区 + 退出码 + child 句柄）。
///
/// child 句柄经 `Arc<Mutex<Option<Child>>>` 共享：wait task 以 `try_wait` 轮询
/// （临界区极短，不阻塞 kill）；`kill` 取锁后 `killpg`/`start_kill`——此前 store
/// 端不保留句柄导致 kill 是空操作（2026-08-23 审查 §6-P1）。
struct ShellEntry {
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    exit_code: Arc<Mutex<Option<i32>>>,
    child: Arc<Mutex<Option<tokio::process::Child>>>,
    /// 插入序号（单调递增，淘汰"最旧"条目的依据，T-8）。
    seq: u64,
}

/// 内存后台 shell 存储（默认实现，非持久化）。
///
/// 使用 `tokio::sync::Mutex`（见 AGENTS.md §2.4）；`output`/`kill` 临界区内无
/// `await`（仅 clone/kill）。后台读取 task 持有 `Arc<Mutex<String>>` 缓冲区，
/// 与 store 无锁竞争。条目数受 [`MAX_TRACKED_SHELLS`] 约束（T-8，见模块文档）。
pub struct InMemoryBackgroundShellStore {
    shells: Mutex<HashMap<String, ShellEntry>>,
    /// 插入序号发生器（与 `shells` 锁解耦，仅 fetch_add，T-8）。
    next_seq: AtomicU64,
}

impl Default for InMemoryBackgroundShellStore {
    fn default() -> Self {
        Self {
            shells: Mutex::new(HashMap::new()),
            next_seq: AtomicU64::new(0),
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
    #[tracing::instrument(skip(self, sandbox), fields(otel.name = span_name::SHELL_BG_SPAWN))]
    fn spawn(
        &self,
        command: String,
        workdir: String,
        env: HashMap<String, String>,
        sandbox: Option<SpawnSandbox>,
        max_output_bytes: usize,
    ) -> BoxFuture<'_, Result<String, ToolError>> {
        Box::pin(async move {
            // 1. 构造命令（与 shell.run 一致：Unix 用 sh -c，Windows 用 cmd /C）
            let mut cmd = if cfg!(windows) {
                let mut c = tokio::process::Command::new("cmd");
                c.arg("/C").arg(&command);
                c
            } else {
                let mut c = tokio::process::Command::new("sh");
                c.arg("-c").arg(&command);
                c
            };
            cmd.current_dir(workdir);
            cmd.env_clear();
            cmd.envs(env);
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());
            // SAFETY: 不依赖 pre_exec hook 的安全不变式（`pre_exec` 未设置）。
            cmd.kill_on_drop(true);

            // S9 对齐（2026-08-23 审查 §6-P1）：后台 spawn 同样自成进程组长，
            // kill 时可 killpg 整树清理（此前仅 run 有，后台路径是旁路）。
            #[cfg(unix)]
            // SAFETY: pre_exec 闭包在 fork 后 exec 前的子进程上下文运行；
            // setpgid(0,0) 仅设置自身进程组，纯 syscall，async-signal-safe。
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setpgid(0, 0) == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                });
            }

            // C-22 对齐（2026-08-23 审查 §6-P1）：后台 spawn 与 run 同权限等级，
            // 必须经同一 OS 沙箱第二道防线——apply 失败视为执行错误上交
            // Runtime 的 denial/fallback 链路处理。
            let has_sandbox = sandbox.as_ref().is_some();
            if let Some(sb) = sandbox.as_ref() {
                let span = tracing::debug_span!(
                    "sandbox.apply",
                    otel.name = span_name::SANDBOX_APPLY,
                    driver = sb.driver.id(),
                );
                let _enter = span.enter();
                sb.driver
                    .apply(&sb.policy, cmd.as_std_mut())
                    .map_err(|e| ToolError::Exec(format!("sandbox apply failed: {e}")))?;
            }

            // 2. Spawn 子进程
            let mut child = cmd
                .spawn()
                .map_err(|e| ToolError::Exec(format!("spawn 失败: {e}")))?;

            // Windows Job Object post_spawn（与 run.rs 一致；Linux/macOS no-op）
            if has_sandbox
                && let Some(sb) = sandbox.as_ref()
                && let Some(pid) = child.id()
            {
                sb.driver.post_spawn(pid).map_err(|e| {
                    let _ = child.start_kill();
                    ToolError::Exec(format!("sandbox post_spawn failed: {e}"))
                })?;
            }

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

            // 5. 共享缓冲区 + 退出码 + child 句柄（kill 用，2026-08-23 审查 §6-P1）
            let stdout_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
            let stderr_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
            let exit_code: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
            let child_handle: Arc<Mutex<Option<tokio::process::Child>>> =
                Arc::new(Mutex::new(Some(child)));

            // 6. 后台 task：持续读 stdout/stderr → 追加缓冲区（带字节上限）
            let stdout_buf_clone = Arc::clone(&stdout_buf);
            tokio::spawn(async move {
                read_capped_to_buffer(stdout, stdout_buf_clone, max_output_bytes).await;
            });
            let stderr_buf_clone = Arc::clone(&stderr_buf);
            tokio::spawn(async move {
                read_capped_to_buffer(stderr, stderr_buf_clone, max_output_bytes).await;
            });

            // 7. 后台 task：try_wait 轮询子进程退出码。不用阻塞 `wait()`——那会
            //    长期持有 child 锁导致 kill 无法取到句柄；100ms 轮询对快照式
            //    `shell.output` 语义足够。
            let exit_code_clone = Arc::clone(&exit_code);
            let wait_child = Arc::clone(&child_handle);
            tokio::spawn(async move {
                loop {
                    let done = {
                        let mut guard = wait_child.lock().await;
                        match guard.as_mut() {
                            Some(c) => match c.try_wait() {
                                Ok(Some(status)) => {
                                    // 信号终止（killpg/SIGKILL）无退出码——记 -1，
                                    // 保证"已退出"语义成立（此前 killed 进程永远
                                    // 显示未退出，2026-08-23 审查 §6-P1 测试暴露）
                                    let code = status.code().unwrap_or(-1);
                                    *exit_code_clone.lock().await = Some(code);
                                    true
                                }
                                Ok(None) => false,
                                Err(_) => true, // wait 失败（进程已被杀等）→ 终止轮询
                            },
                            None => true, // 句柄被 kill 端取走（理论不发生）
                        }
                    };
                    if done {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            });

            // 8. 注册到 store：已达上限先淘汰最旧条目（T-8，2026-08-25 审查，
            //    策略见 `evict_oldest` 与模块文档）
            let entry = ShellEntry {
                stdout: stdout_buf,
                stderr: stderr_buf,
                exit_code,
                child: child_handle,
                seq: self.next_seq.fetch_add(1, Ordering::Relaxed),
            };
            let mut shells = self.shells.lock().await;
            evict_oldest(&mut shells);
            shells.insert(shell_id.clone(), entry);
            // Metrics：后台 shell 数 gauge
            metrics::set_background_shells(shells.len() as u64);

            Ok(shell_id)
        })
    }

    #[tracing::instrument(skip(self), fields(otel.name = "shell.bg_output"))]
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
            let exit_code = *entry.exit_code.lock().await;
            if exit_code.is_some() {
                return Ok(()); // 已退出，幂等
            }
            // 真实现（2026-08-23 审查 §6-P1）：store 侧持有 child 句柄。
            // Unix 先 killpg 整树（spawn 前 setpgid(0,0)，pgid == pid），再
            // start_kill 兜底；Windows 直接 start_kill。
            let mut guard = entry.child.lock().await;
            if let Some(child) = guard.as_mut() {
                #[cfg(unix)]
                if let Some(pid) = child.id() {
                    let _ = tokio::task::spawn_blocking(move || unsafe {
                        libc::killpg(i32::try_from(pid).unwrap_or(-1), libc::SIGKILL)
                    })
                    .await;
                }
                let _ = child.start_kill();
            }
            Ok(())
        })
    }
}

/// 淘汰最旧条目使 store 不超过 [`MAX_TRACKED_SHELLS`]（T-8，2026-08-25 审查）。
///
/// 优先淘汰最旧的**已完成**条目（`exit_code` 已记录、缓冲不再增长）；若无已完成
/// 条目则退化为淘汰全局最旧——保证硬上限成立。
///
/// PTM-4（2026-08-25 R2 审查）：淘汰**运行中**条目时必须主动终止进程——此前
/// 仅 `shells.remove`，而 wait task 持有 `Child` 的 Arc 克隆，`kill_on_drop`
/// 永不触发，被淘汰条目退化为无人跟踪的孤儿进程（与模块文档承诺相反）。
/// 终止路径与 `kill` 一致：unix 先 killpg 整树再 `start_kill` 兜底。
fn evict_oldest(shells: &mut HashMap<String, ShellEntry>) {
    if shells.len() < MAX_TRACKED_SHELLS {
        return;
    }
    let victim = shells
        .iter()
        .min_by_key(|(_, e)| {
            let completed = e.exit_code.try_lock().is_ok_and(|g| g.is_some());
            (u8::from(!completed), e.seq)
        })
        .map(|(id, _)| id.clone());
    if let Some(id) = victim {
        if let Some(entry) = shells.get(&id) {
            let running = entry.exit_code.try_lock().is_ok_and(|g| g.is_none());
            if running
                && let Ok(mut guard) = entry.child.try_lock()
                && let Some(child) = guard.as_mut()
            {
                #[cfg(unix)]
                if let Some(pid) = child.id() {
                    // SAFETY: killpg 向目标进程组发送 SIGKILL；pid 来自 Child::id，
                    // spawn 前 setpgid 保证 pgid == pid。同步调用非阻塞。
                    unsafe {
                        let _ = libc::killpg(i32::try_from(pid).unwrap_or(-1), libc::SIGKILL);
                    }
                }
                let _ = child.start_kill();
                tracing::warn!(shell_id = %id, "后台 shell 条目达上限且仍在运行：已终止被淘汰的进程");
            }
        }
        shells.remove(&id);
        tracing::debug!(shell_id = %id, "后台 shell 条目已达上限，淘汰最旧条目");
    }
}

/// 持续读取 `AsyncRead` 到共享缓冲区，累计超过 `cap` 字节后**丢弃**后续数据
/// （追加一次性截断标记）但保持读取直到 EOF——不能停止读：管道写端阻塞会让
/// 长驻进程（如 dev server）被 SIGPIPE 杀死或卡住（2026-08-23 审查 §6-P1：
/// 此前无上限累积可 OOM）。
async fn read_capped_to_buffer<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    buf: Arc<Mutex<String>>,
    cap: usize,
) {
    let mut chunk = [0u8; 4096];
    let mut total = 0usize;
    let mut marked = false;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break, // EOF 或读错误（管道关闭等）→ 停止
            Ok(n) => {
                total += n;
                if total <= cap {
                    // SAFETY: 从进程 stdout/stderr 读取的字节流按 UTF-8 解码；
                    // 非完整 UTF-8 边界可能产生 replacement char，可接受（输出展示用）。
                    let text = String::from_utf8_lossy(&chunk[..n]);
                    buf.lock().await.push_str(&text);
                } else if !marked {
                    marked = true;
                    buf.lock()
                        .await
                        .push_str("\n...[output truncated: 超过后台输出上限，进程继续运行]");
                }
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
                        "description": "要执行的 shell 命令（Unix 用 sh -c 语义，Windows 用 cmd /C 语义）"
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
        let sandbox = match (ctx.sandbox_driver.clone(), ctx.sandbox_policy.clone()) {
            (Some(driver), Some(policy)) => Some(SpawnSandbox { driver, policy }),
            _ => None,
        };
        let max_output_bytes = ctx.max_output_bytes;
        Box::pin(async move {
            let command: String = params
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("command 缺失".into()))?
                .to_string();
            let shell_id = store
                .spawn(command, workdir, env, sandbox, max_output_bytes)
                .await?;
            Ok(ToolResult::ok_text(format!(
                "后台 shell 已启动 (shell_id={shell_id})。用 shell.output 读取输出。"
            )))
        })
    }

    /// 渲染意图（R-05，M-11）：启动确认消息，文本直出。
    fn render_output(&self, result: &ToolResult) -> RenderIntent {
        RenderIntent::default_for(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn store_spawn_and_output_basic() {
        let store = InMemoryBackgroundShellStore::new();
        let mut env = HashMap::new();
        if let Ok(path) = std::env::var("PATH") {
            env.insert("PATH".to_string(), path);
        }

        let shell_id = store
            .spawn(
                "echo hello".to_string(),
                "/tmp".to_string(),
                env,
                None,
                1024 * 1024,
            )
            .await
            .expect("spawn");

        // 等待命令完成
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        let status = store.output(shell_id).await.expect("output");
        assert!(
            status.stdout.contains("hello"),
            "stdout should contain hello"
        );
        assert!(status.exited, "should be exited");
        assert_eq!(status.exit_code, Some(0));
    }

    #[tokio::test]
    async fn store_output_nonexistent_returns_not_found() {
        let store = InMemoryBackgroundShellStore::new();
        let result = store.output("nonexistent".to_string()).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn store_kill_nonexistent_returns_not_found() {
        let store = InMemoryBackgroundShellStore::new();
        let result = store.kill("nonexistent".to_string()).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn store_kill_stops_running_process() {
        let store = InMemoryBackgroundShellStore::new();
        let mut env = HashMap::new();
        if let Ok(path) = std::env::var("PATH") {
            env.insert("PATH".to_string(), path);
        }

        // 长驻进程：sleep 30s
        let shell_id = store
            .spawn(
                "sleep 30".to_string(),
                "/tmp".to_string(),
                env,
                None,
                1024 * 1024,
            )
            .await
            .expect("spawn");
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        assert!(
            !store.output(shell_id.clone()).await.expect("output").exited,
            "sleep 应仍在运行"
        );

        store.kill(shell_id.clone()).await.expect("kill");

        // killpg(SIGKILL) 后 wait task 应在轮询间隔内记录退出（2026-08-23 审查 §6-P1）
        for _ in 0..30 {
            if store.output(shell_id.clone()).await.expect("output").exited {
                return;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
        panic!("进程在 kill 后 3s 内仍未退出");
    }

    #[tokio::test]
    async fn store_output_truncates_beyond_cap() {
        let store = InMemoryBackgroundShellStore::new();
        let mut env = HashMap::new();
        if let Ok(path) = std::env::var("PATH") {
            env.insert("PATH".to_string(), path);
        }

        // 输出远超极小 cap；缓冲应截断并带标记（进程本身正常退出）
        let shell_id = store
            .spawn(
                "head -c 20000 /dev/zero | tr '\\0' 'x'".to_string(),
                "/tmp".to_string(),
                env,
                None,
                4096,
            )
            .await
            .expect("spawn");
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        let status = store.output(shell_id).await.expect("output");
        assert!(status.stdout.contains("[output truncated"), "应有截断标记");
        assert!(status.stdout.len() < 20000, "缓冲不应无上限累积");
    }

    #[tokio::test]
    async fn store_kill_exited_is_idempotent() {
        let store = InMemoryBackgroundShellStore::new();
        let mut env = HashMap::new();
        if let Ok(path) = std::env::var("PATH") {
            env.insert("PATH".to_string(), path);
        }

        let shell_id = store
            .spawn(
                "true".to_string(),
                "/tmp".to_string(),
                env,
                None,
                1024 * 1024,
            )
            .await
            .expect("spawn");

        // 等待命令完成
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        // kill 已退出的 shell 应成功（幂等）
        let result = store.kill(shell_id).await;
        assert!(result.is_ok(), "kill exited shell should be idempotent");
    }

    #[tokio::test]
    async fn store_spawn_accumulates_stderr() {
        let store = InMemoryBackgroundShellStore::new();
        let mut env = HashMap::new();
        if let Ok(path) = std::env::var("PATH") {
            env.insert("PATH".to_string(), path);
        }

        let shell_id = store
            .spawn(
                "echo err >&2".to_string(),
                "/tmp".to_string(),
                env,
                None,
                1024 * 1024,
            )
            .await
            .expect("spawn");

        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        let status = store.output(shell_id).await.expect("output");
        assert!(status.stderr.contains("err"), "stderr should contain err");
    }

    #[tokio::test]
    async fn tool_missing_command_returns_invalid_input() {
        let store: Arc<dyn BackgroundShellStore> = Arc::new(InMemoryBackgroundShellStore::new());
        let tool = ShellBackground::new(store);
        let ctx = minicoding_core::tool::ToolContext::new("/tmp".into(), "test".to_string());
        let result = tool.execute(serde_json::json!({}), &ctx).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn tool_spawn_returns_shell_id() {
        let store: Arc<dyn BackgroundShellStore> = Arc::new(InMemoryBackgroundShellStore::new());
        let tool = ShellBackground::new(Arc::clone(&store));
        let ctx = minicoding_core::tool::ToolContext::new("/tmp".into(), "test".to_string());
        let result = tool
            .execute(serde_json::json!({"command": "echo hi"}), &ctx)
            .await
            .expect("execute");
        assert!(!result.is_error);
        let minicoding_core::model::ToolContent::Text(text) = result.content else {
            panic!("expected text content");
        };
        assert!(text.contains("shell_id"), "should contain shell_id: {text}");
    }

    #[tokio::test]
    async fn store_evicts_oldest_beyond_cap() {
        // T-8（2026-08-25 审查）：超过 MAX_TRACKED_SHELLS 后最旧条目被淘汰
        //（output 返回 NotFound），新条目仍可用。
        let store = InMemoryBackgroundShellStore::new();
        let mut env = HashMap::new();
        if let Ok(path) = std::env::var("PATH") {
            env.insert("PATH".to_string(), path);
        }

        let first_id = store
            .spawn(
                "true".to_string(),
                "/tmp".to_string(),
                env.clone(),
                None,
                1024 * 1024,
            )
            .await
            .expect("first spawn");

        // 连续 spawn 超过上限：第 129 次插入必然触发淘汰（无论首个条目是否已
        // 记录退出码，min_by_key 的兜底分支都会选中全局最旧的 first_id）
        let mut last_id = String::new();
        for _ in 0..(MAX_TRACKED_SHELLS + 2) {
            last_id = store
                .spawn(
                    "true".to_string(),
                    "/tmp".to_string(),
                    env.clone(),
                    None,
                    1024 * 1024,
                )
                .await
                .expect("spawn");
        }

        assert!(
            store.output(first_id.clone()).await.is_err(),
            "最旧条目应被淘汰"
        );
        // 最新条目保留且正常完成（退出码由 wait task 100ms 轮询记录，需等待）
        for _ in 0..30 {
            if store
                .output(last_id.clone())
                .await
                .expect("最新条目应保留")
                .exited
            {
                return;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
        panic!("最新条目在 3s 内未完成");
    }

    #[test]
    fn tool_side_effect_is_command() {
        let store: Arc<dyn BackgroundShellStore> = Arc::new(InMemoryBackgroundShellStore::new());
        let tool = ShellBackground::new(store);
        assert_eq!(tool.side_effect(), SideEffect::Command);
    }

    #[test]
    fn tool_schema_has_correct_name() {
        let store: Arc<dyn BackgroundShellStore> = Arc::new(InMemoryBackgroundShellStore::new());
        let tool = ShellBackground::new(store);
        assert_eq!(tool.name(), "shell.background");
    }
}
