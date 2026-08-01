//! `minicoding exec --sandbox <policy> "prompt"` 子命令（T-M4-10）。
//!
//! 非交互批量执行：单轮对话，沙箱策略由 `--sandbox` 指定，无 TTY 交互。
//! 适用于 CI/脚本场景：`minicoding exec --sandbox read-only "读 README 并总结"`。
//!
//! 与默认单次模式（`minicoding "prompt"`）的区别：
//! - 显式 `--sandbox` 指定沙箱策略（不依赖默认 `WorkspaceWrite`）；
//! - 强制非交互（`NonInteractivePrompter`，副作用工具被拒绝，CI 安全默认）；
//! - 退出码语义与单次模式一致（0 成功 / 1 错误 / 130 中断）。

use anyhow::{Context, Result};
use clap::Args;
use minicoding_core::model::{TurnOutcome, UserInput};
use minicoding_core::runtime::Event;

use crate::builder::SessionLoadMode;

/// 沙箱策略字符串（CLI 解析用）。
#[derive(clap::ValueEnum, Debug, Clone, Copy)]
pub enum SandboxArg {
    /// 只读：workdir 只读，所有写操作被沙箱拦截。
    ReadOnly,
    /// 工作区可写：workdir 可写，其余只读（默认）。
    WorkspaceWrite,
    /// 外部沙箱：依赖 CI/容器隔离，minicoding 不应用内核沙箱。
    ExternalSandbox,
    /// 全访问：不应用任何隔离（需 red 警告 + 二次确认，C-22）。
    DangerFullAccess,
}

impl SandboxArg {
    /// 转换为 `SandboxPolicy`（需要 workdir 填充 `WorkspaceWrite`）。
    #[must_use]
    fn to_policy(self, workdir: camino::Utf8PathBuf) -> minicoding_core::sandbox::SandboxPolicy {
        use minicoding_core::sandbox::SandboxPolicy;
        match self {
            Self::ReadOnly => SandboxPolicy::ReadOnly,
            Self::WorkspaceWrite => SandboxPolicy::WorkspaceWrite {
                workdir,
                writable: Vec::new(),
            },
            Self::ExternalSandbox => SandboxPolicy::ExternalSandbox,
            Self::DangerFullAccess => SandboxPolicy::DangerFullAccess,
        }
    }
}

/// `exec` 子命令选项。
#[derive(Args, Debug)]
pub struct ExecCommand {
    /// 沙箱策略（默认 `workspace-write`）。
    #[arg(long, value_enum, default_value_t = SandboxArg::WorkspaceWrite)]
    pub sandbox: SandboxArg,

    /// 提问内容（必填，非交互）。
    #[arg(required = true)]
    pub prompt: String,

    /// 工作目录（默认当前目录）。
    #[arg(long, default_value = ".")]
    pub workdir: String,

    /// LLM provider 类型（`openai`/`anthropic`/`ollama`，覆盖 `config.provider.default`，T-M6-5）。
    #[arg(long)]
    pub provider: Option<String>,

    /// 模型名称（覆盖配置/环境变量）。
    #[arg(long, env = "OPENAI_MODEL")]
    pub model: Option<String>,

    /// API base URL（覆盖配置/环境变量）。
    #[arg(long, env = "OPENAI_API_BASE")]
    pub api_base: Option<String>,

    /// API key（建议用环境变量 `OPENAI_API_KEY`）。
    #[arg(long, env = "OPENAI_API_KEY")]
    pub api_key: Option<String>,
}

/// 执行 `exec` 子命令。
///
/// 构建 Runtime（注入指定沙箱策略）→ 单轮对话 → 流式渲染 → 返回退出码。
///
/// # Errors
/// Runtime 构建失败、turn 执行失败时返回错误。
pub fn run_exec_command(cmd: &ExecCommand) -> Result<i32> {
    let workdir_path = camino::Utf8PathBuf::from(&cmd.workdir)
        .canonicalize_utf8()
        .unwrap_or_else(|_| camino::Utf8PathBuf::from(&cmd.workdir));

    let sandbox_policy = cmd.sandbox.to_policy(workdir_path.clone());

    // DangerFullAccess 需 red 警告 + 二次确认（C-22）
    if matches!(cmd.sandbox, SandboxArg::DangerFullAccess) {
        eprintln!("\x1b[31m警告: --sandbox danger-full-access 已选定，不应用任何隔离。\x1b[0m");
        eprintln!("副作用工具（文件写、shell 执行）将不受 OS 沙箱约束。");
        eprintln!("仅限受信环境使用。");
    }

    let rt = crate::builder::build_runtime(
        cmd.provider.as_deref(),
        cmd.api_base.as_deref(),
        cmd.api_key.as_deref(),
        cmd.model.as_deref(),
        &cmd.workdir,
        None,
        &SessionLoadMode::None,
        Some(sandbox_policy),
        false, // exec 子命令不支持 --plan（非交互场景 Plan 模式无意义）
    )
    .context("构建 Runtime 失败")?;

    let runtime = tokio::runtime::Runtime::new()?;
    let exit_code = runtime.block_on(run_single_turn(&rt, cmd.prompt.clone()));
    Ok(exit_code)
}

/// 运行单轮对话，流式渲染 token（复用 main.rs 逻辑）。
async fn run_single_turn(rt: &minicoding_core::runtime::Runtime, prompt: String) -> i32 {
    let mut rx = rt.events().subscribe();

    let render_task = tokio::spawn(async move {
        use std::io::Write;
        loop {
            match rx.recv().await {
                Ok(Event::Token(text)) => {
                    let stdout = std::io::stdout();
                    let mut lock = stdout.lock();
                    let _ = write!(lock, "{text}");
                    let _ = lock.flush();
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let user_input = UserInput::from_text(prompt);
    let result = rt.run_turn(user_input).await;
    render_task.abort();

    match result {
        Ok(TurnOutcome::Finished(msg)) => {
            if !msg.text().is_empty() {
                println!();
            }
            0
        }
        Ok(TurnOutcome::Interrupted(_)) => 130,
        Ok(TurnOutcome::Failed(e)) => {
            eprintln!("错误: {e}");
            1
        }
        Err(e) => {
            eprintln!("运行时错误: {e}");
            1
        }
    }
}
