//! `minicoding mcp list/approve/reset-project-choices` 子命令（T-M4-10）。
//!
//! 管理已配置的 MCP server 与 project 作用域批准状态（C-24）。不构建 `Runtime`，
//! 直接读取 `mcp.json` 配置文件与 `mcp_choices.toml` 批准库。
//!
//! ## 配置文件位置
//!
//! - `~/.minicoding/mcp.json`：user/local 作用域 server；
//! - `.minicoding/mcp.json`：project 作用域 server（仓库根，入版本控制）。
//!
//! ## 配置格式（TOML）
//!
//! `~/.minicoding/mcp.json`：
//! ```toml
//! [[user.servers]]
//! name = "github"
//! transport = { transport = "stdio", command = "npx", args = ["-y", "@mcp/server-github"], env = {} }
//!
//! [[local.servers]]
//! name = "filesystem"
//! transport = { transport = "stdio", command = "npx", args = ["-y", "@mcp/server-fs"], env = {} }
//! ```
//!
//! `.minicoding/mcp.json`：
//! ```toml
//! [[servers]]
//! name = "db"
//! transport = { transport = "stdio", command = "node", args = ["db-server.js"], env = {} }
//! ```

use std::collections::HashMap;
use std::io::IsTerminal;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use clap::{Args, Subcommand};
use minicoding_core::mcp::{McpScope, McpTransport};
use minicoding_core::paths;

/// `mcp` 顶层子命令。
#[derive(Args, Debug)]
pub struct McpCommand {
    #[command(subcommand)]
    pub action: McpAction,
}

/// `mcp` 子命令动作。
#[derive(Subcommand, Debug)]
pub enum McpAction {
    /// 列出所有已配置的 MCP server 及其批准状态。
    List,
    /// 批准 project 作用域 server（写入 `mcp_choices.toml`）。
    Approve {
        /// 待批准的 server 名称。
        server: String,
    },
    /// 拒绝 project 作用域 server（写入 `mcp_choices.toml`）。
    Reject {
        /// 待拒绝的 server 名称。
        server: String,
    },
    /// 重置当前项目的所有 MCP 批准记录。
    ResetProjectChoices,
}

/// 执行 `mcp` 子命令。
///
/// # Errors
/// 配置文件读取/解析失败、choices 文件读写失败时返回错误。
pub fn run_mcp_command(cmd: &McpCommand, workdir: &str) -> Result<()> {
    let project_root = Utf8PathBuf::from(workdir)
        .canonicalize_utf8()
        .unwrap_or_else(|_| Utf8PathBuf::from(workdir));

    let choices_path = paths::mcp_choices_path().context("无法确定 mcp_choices.toml 路径")?;
    let store = minicoding_mcp::FileChoicesStore::new(choices_path);

    match &cmd.action {
        McpAction::List => list_servers(&project_root, &store),
        McpAction::Approve { server } => {
            minicoding_mcp::set_project_approval(
                &project_root,
                server,
                minicoding_mcp::ApprovalState::Approved,
                &store,
            )
            .context("写入批准状态失败")?;
            println!("已批准 project server: {server}");
            Ok(())
        }
        McpAction::Reject { server } => {
            minicoding_mcp::set_project_approval(
                &project_root,
                server,
                minicoding_mcp::ApprovalState::Rejected,
                &store,
            )
            .context("写入拒绝状态失败")?;
            println!("已拒绝 project server: {server}");
            Ok(())
        }
        McpAction::ResetProjectChoices => {
            minicoding_mcp::reset_project_choices(&project_root, &store)
                .context("重置批准记录失败")?;
            println!("已重置项目 {project_root} 的所有 MCP 批准记录");
            Ok(())
        }
    }
}

/// 列出所有已配置 server 及批准状态。
fn list_servers(
    project_root: &Utf8PathBuf,
    store: &minicoding_mcp::FileChoicesStore,
) -> Result<()> {
    let configs = minicoding_mcp::config::load_all_configs(project_root)?;

    // 加载 project 批准状态
    let project_choices: HashMap<String, minicoding_mcp::ApprovalState> =
        minicoding_mcp::list_project_choices(project_root, store)
            .context("读取批准状态失败")?
            .into_iter()
            .collect();

    if configs.is_empty() {
        println!("（暂无已配置的 MCP server）");
        return Ok(());
    }

    let is_tty = std::io::stdout().is_terminal();
    if is_tty {
        println!(
            "{:<20}  {:<8}  {:<10}  {:<10}  TRANSPORT",
            "NAME", "SCOPE", "ENABLED", "APPROVAL"
        );
        println!("{}", "-".repeat(78));
    }

    for cfg in &configs {
        let scope = match cfg.scope {
            McpScope::Local => "local",
            McpScope::User => "user",
            McpScope::Project => "project",
        };
        let enabled = if cfg.enabled { "yes" } else { "no" };
        let approval = match cfg.scope {
            McpScope::Project => project_choices
                .get(&cfg.name)
                .map_or("pending", |s| match s {
                    minicoding_mcp::ApprovalState::Approved => "approved",
                    minicoding_mcp::ApprovalState::Rejected => "rejected",
                }),
            _ => "n/a",
        };
        let transport = transport_summary(&cfg.transport);

        if is_tty {
            println!(
                "{:<20}  {:<8}  {:<10}  {:<10}  {}",
                cfg.name, scope, enabled, approval, transport
            );
        } else {
            println!(
                "{}\t{}\t{}\t{}\t{}",
                cfg.name, scope, enabled, approval, transport
            );
        }
    }
    println!();
    println!("共 {} 个 server", configs.len());
    Ok(())
}

/// 传输协议摘要（用于 list 输出）。
fn transport_summary(t: &McpTransport) -> String {
    match t {
        McpTransport::Stdio { command, args, .. } => {
            format!("stdio: {command} {}", args.join(" "))
        }
        McpTransport::Http { url, .. } => format!("http: {url}"),
    }
}
