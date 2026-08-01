//! # minicoding-cli
//!
//! CLI frontend：命令行入口。
//!
//! 解析参数、加载配置、构建 `Runtime`、驱动会话、渲染输出。零业务逻辑——所有决策委托
//! `Runtime`；CLI 只做 IO 与渲染。
//!
//! ## 能力
//!
//! - 单次提问模式：`minicoding "你的问题"`（M1）
//! - 交互会话模式：`minicoding --session` 进入多轮 REPL（M2 / T-M2-8）
//! - 恢复会话：`minicoding --resume <id>` 继续历史会话（M3 / T-M3-10a）
//! - 回放会话：`minicoding --replay <id>`（默认禁副作用，C-06，T-M3-10b）
//! - 分叉会话：`minicoding --fork-session <id>`（T-M3-10b）
//! - 会话管理：`minicoding session list`/`delete <id>`（T-M3-10c）
//! - 流式 token 渲染（实时打印到 stdout）
//! - 配置从环境变量或默认值加载（`OPENAI_API_KEY`/`OPENAI_API_BASE`/`OPENAI_MODEL`）
//! - 只读工具组（`fs.read`/`fs.list`/`fs.glob`/`fs.grep`）自动注册
//!
//! ## 退出码
//!
//! 成功 0；运行时错误 1；配置错误 2；中断 130。
//!
//! 详见 `docs/modules.md` §12。

#![deny(clippy::all, clippy::pedantic)]

mod builder;
mod commands;
mod session;

use anyhow::{Context, Result};
use builder::SessionLoadMode;
use clap::{Parser, Subcommand};
use commands::SessionCommand;
use minicoding_core::model::{TurnOutcome, UserInput};
use minicoding_core::runtime::Event;

/// 顶层子命令（除默认运行模式外的独立操作，T-M3-10c）。
///
/// `session list`/`delete` 不构建 `Runtime`，直接复用存储层同步方法。
#[derive(Subcommand, Debug)]
enum Command {
    /// 会话管理（列出 / 删除）。
    #[command(name = "session")]
    Session(SessionCommand),
}

/// minicoding — 终端 AI Coding 助手
#[derive(Parser, Debug)]
#[command(name = "minicoding", version, about, long_about = None)]
struct Cli {
    /// 单次提问内容（无 `--session` 时为单次模式；省略则进入交互 REPL）
    prompt: Option<String>,

    /// 进入交互会话模式（多轮 REPL）
    #[arg(long)]
    session: bool,

    /// 恢复指定会话继续对话（T-M3-10a）。
    #[arg(long, value_name = "SESSION_ID")]
    resume: Option<String>,

    /// 回放指定会话，默认禁用副作用工具（C-06，T-M3-10b）。
    #[arg(long, value_name = "SESSION_ID")]
    replay: Option<String>,

    /// 从指定会话分叉到新会话（原会话不变，T-M3-10b）。
    #[arg(long, value_name = "SESSION_ID")]
    fork_session: Option<String>,

    /// `--replay` 时显式允许副作用工具（每条仍走权限策略，C-06）。
    #[arg(long)]
    allow_side_effects: bool,

    /// 模型名称（覆盖配置/环境变量）
    #[arg(long, env = "OPENAI_MODEL")]
    model: Option<String>,

    /// API base URL（覆盖配置/环境变量）
    #[arg(long, env = "OPENAI_API_BASE")]
    api_base: Option<String>,

    /// API key（建议用环境变量 `OPENAI_API_KEY`）
    #[arg(long, env = "OPENAI_API_KEY")]
    api_key: Option<String>,

    /// 工作目录（默认当前目录）
    #[arg(long, default_value = ".")]
    workdir: String,

    /// 系统 prompt（覆盖默认）
    #[arg(long)]
    system: Option<String>,

    /// 启用详细日志
    #[arg(long, short = 'v')]
    verbose: bool,

    /// 顶层子命令（如 `session list`/`delete`）。出现时跳过 Runtime 构建。
    #[command(subcommand)]
    command: Option<Command>,
}

/// 从 CLI 参数解析会话加载模式（`--resume`/`--replay`/`--fork-session` 互斥）。
fn resolve_session_mode(cli: &Cli) -> Result<SessionLoadMode> {
    let modes: Vec<&str> = [
        cli.resume.as_deref().map(|_| "resume"),
        cli.replay.as_deref().map(|_| "replay"),
        cli.fork_session.as_deref().map(|_| "fork"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if modes.len() > 1 {
        anyhow::bail!("--resume/--replay/--fork-session 互斥，只能指定一个");
    }
    if let Some(id) = &cli.resume {
        return Ok(SessionLoadMode::Resume(id.clone()));
    }
    if let Some(id) = &cli.replay {
        return Ok(SessionLoadMode::Replay {
            id: id.clone(),
            allow_side_effects: cli.allow_side_effects,
        });
    }
    if let Some(id) = &cli.fork_session {
        return Ok(SessionLoadMode::Fork(id.clone()));
    }
    if cli.allow_side_effects {
        anyhow::bail!("--allow-side-effects 仅在 --replay 时有效");
    }
    Ok(SessionLoadMode::None)
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // 初始化日志
    let filter = if cli.verbose { "debug" } else { "warn" };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    // 顶层子命令分派（如 `session list`/`delete`）：不构建 Runtime，无需 API key。
    if let Some(Command::Session(sess_cmd)) = &cli.command {
        commands::run_session_command(sess_cmd).context("session 子命令失败")?;
        return Ok(());
    }

    // 解析会话加载模式（互斥校验）
    let mode = resolve_session_mode(&cli)?;
    let has_preloaded_session = !matches!(mode, SessionLoadMode::None);

    // 分派：`--session` 或无 prompt → 交互 REPL；有 prompt 且无 `--session` → 单次
    let interactive = cli.session || cli.prompt.is_none();

    // 构建 Runtime
    let rt = builder::build_runtime(
        cli.api_base.as_deref(),
        cli.api_key.as_deref(),
        cli.model.as_deref(),
        &cli.workdir,
        cli.system.as_deref(),
        &mode,
    )
    .context("构建 Runtime 失败")?;

    // 运行
    let runtime = tokio::runtime::Runtime::new()?;
    let exit_code = if interactive {
        runtime.block_on(async {
            if has_preloaded_session {
                if let Err(e) = rt.restore_history().await {
                    eprintln!("恢复会话历史失败: {e}");
                    return 1;
                }
            }
            session::run_interactive_session(&rt).await
        })
    } else {
        let prompt = cli.prompt.expect("单次模式 prompt 必为 Some");
        runtime.block_on(async {
            if has_preloaded_session {
                if let Err(e) = rt.restore_history().await {
                    eprintln!("恢复会话历史失败: {e}");
                    return 1;
                }
            }
            run_single_turn(&rt, prompt).await
        })
    };

    std::process::exit(exit_code);
}

/// 运行单轮对话，流式渲染 token。
///
/// 返回退出码：0 成功，1 错误，130 中断。
async fn run_single_turn(rt: &minicoding_core::runtime::Runtime, prompt: String) -> i32 {
    // 订阅事件总线（在 turn 之前订阅，避免错过早期事件）
    let mut rx = rt.events().subscribe();

    // 后台消费 Token 事件，实时打印到 stdout。
    // 注意：`StdoutLock` 不是 `Send`，不能跨 await 持有，故每次写入时重新获取锁。
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
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // 非 Token 事件或落后跳过
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // 执行 turn（产生事件 → 后台渲染）
    let user_input = UserInput::from_text(prompt);
    let result = rt.run_turn(user_input).await;

    // turn 结束后关闭渲染任务（EventBus drop 或不再有事件）
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
