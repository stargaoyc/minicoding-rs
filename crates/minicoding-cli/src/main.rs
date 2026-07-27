//! # minicoding-cli
//!
//! CLI frontend：命令行入口。
//!
//! 解析参数、加载配置、构建 `Runtime`、驱动会话、渲染输出。零业务逻辑——所有决策委托
//! `Runtime`；CLI 只做 IO 与渲染。
//!
//! ## M1 能力
//!
//! - 单次提问模式：`minicoding "你的问题"`
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

use anyhow::{Context, Result};
use clap::Parser;
use minicoding_core::model::{TurnOutcome, UserInput};
use minicoding_core::runtime::Event;

/// minicoding — 终端 AI Coding 助手
#[derive(Parser, Debug)]
#[command(name = "minicoding", version, about, long_about = None)]
struct Cli {
    /// 单次提问内容（M1 仅支持单次模式）
    prompt: Option<String>,

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
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // 初始化日志
    let filter = if cli.verbose { "debug" } else { "warn" };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    // 检查 prompt
    let prompt = cli
        .prompt
        .clone()
        .context("M1 仅支持单次提问模式：minicoding \"你的问题\"")?;

    // 构建 Runtime
    let rt = builder::build_runtime(
        cli.api_base.as_deref(),
        cli.api_key.as_deref(),
        cli.model.as_deref(),
        &cli.workdir,
        cli.system.as_deref(),
    )
    .context("构建 Runtime 失败")?;

    // 运行
    let runtime = tokio::runtime::Runtime::new()?;
    let exit_code = runtime.block_on(run_single_turn(&rt, prompt));

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
