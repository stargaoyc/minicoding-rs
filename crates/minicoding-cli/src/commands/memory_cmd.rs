//! `minicoding memory list`/`read`/`clear` 子命令（R10-08）。
//!
//! 记忆系统此前只有 `memory.write` 工具（模型可写、用户不可读不可删），
//! 用户查看/纠正/删除记忆只能手改 `~/.minicoding/memory/*.json`。本子命令
//! 补齐读取/列出/清空入口。

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use minicoding_core::memory::MemoryStore;
use minicoding_memory::{AutoMemory, LongTermMemory};
use std::io::IsTerminal;

/// `memory` 子命令动作。
#[derive(clap::Subcommand, Debug)]
pub enum MemoryAction {
    /// 列出记忆（auto + `long_term` 的渲染正文）。
    List,
    /// 打印记忆全文（auto + `long_term`）。
    Read,
    /// 清空 auto 记忆；`--long-term` 一并清空长期记忆。
    Clear {
        /// 一并清空长期记忆（破坏性操作，需二次确认）。
        #[arg(long, default_value_t = false)]
        long_term: bool,
        /// 跳过二次确认（CI/脚本用）。
        #[arg(long, default_value_t = false)]
        force: bool,
    },
}

/// `memory` 顶层子命令。
#[derive(clap::Args, Debug)]
pub struct MemoryCommand {
    #[command(subcommand)]
    pub action: MemoryAction,
}

/// 执行 `memory` 子命令。
///
/// 不构建 `Runtime`、无需 API key——只做记忆存储 IO 与终端渲染。
///
/// # Errors
/// 记忆目录不可解析、读取/清空失败时返回错误。
pub fn run_memory_command(cmd: &MemoryCommand) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move { run_memory_command_async(cmd).await })
}

/// async 执行体（`tokio::runtime::block_on` 内运行，提供 tokio reactor）。
async fn run_memory_command_async(cmd: &MemoryCommand) -> Result<()> {
    match &cmd.action {
        MemoryAction::List => list_memory().await,
        MemoryAction::Read => read_memory().await,
        MemoryAction::Clear { long_term, force } => clear_memory(*long_term, *force).await,
    }
}

/// 记忆目录（与 `minicoding-memory` 默认一致：`MINICODING_HOME/memory`）。
fn memory_dir() -> Result<Utf8PathBuf> {
    minicoding_core::paths::memory_dir().context("无法确定记忆目录")
}

/// 读取 auto + `long_term` 记忆正文，返回 `(auto, long_term)`。
async fn load_both() -> Result<(String, String)> {
    let dir = memory_dir()?;
    let auto = AutoMemory::with_dir(&dir);
    let auto_text = auto.load_rendered().await.context("读取 auto 记忆失败")?;
    let long = LongTermMemory::with_dir(&dir);
    let long_text = long.load().await.context("读取 long_term 记忆失败")?;
    Ok((auto_text, long_text))
}

/// 列出记忆。
async fn list_memory() -> Result<()> {
    let (auto_text, long_text) = load_both().await?;
    println!("== auto memory ==");
    if auto_text.trim().is_empty() {
        println!("(空)");
    } else {
        println!("{auto_text}");
    }
    println!("\n== long_term ==");
    if long_text.trim().is_empty() {
        println!("(空)");
    } else {
        println!("{long_text}");
    }
    Ok(())
}

/// 打印记忆全文（与 `list` 相同，语义更明确）。
async fn read_memory() -> Result<()> {
    list_memory().await
}

/// 清空 auto 记忆（可选一并清空 `long_term`）。
async fn clear_memory(long_term: bool, force: bool) -> Result<()> {
    let dir = memory_dir()?;
    let scope = if long_term {
        "auto + long_term"
    } else {
        "auto"
    };
    if !force {
        let input = prompt_confirm(&format!("确认清空记忆（{scope}）？此操作不可恢复 [y/N]: "))?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("已取消");
            return Ok(());
        }
    }

    let auto = AutoMemory::with_dir(&dir);
    auto.clear().await.context("清空 auto 记忆失败")?;
    println!("auto 记忆已清空");

    if long_term {
        let long = LongTermMemory::with_dir(&dir);
        long.save("").await.context("清空 long_term 记忆失败")?;
        println!("long_term 记忆已清空");
    }
    Ok(())
}

/// 交互确认（stdin 非 TTY 时要求 `--force`）。
fn prompt_confirm(prompt: &str) -> Result<String> {
    use std::io::Write;
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("stdin 非交互终端：请显式使用 --force 跳过确认");
    }
    print!("{prompt}");
    std::io::stdout().flush().ok();
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    Ok(buf)
}
