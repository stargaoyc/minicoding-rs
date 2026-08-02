//! `serve` 子命令：启动 HTTP/SSE server（T-M8-2，`serve` feature）或
//! MCP stdio server（T-M8-3，`mcp` feature）。
//!
//! 默认等价于独立运行 `minicoding-server`，通过 `minicoding serve` 统一入口。
//! 构造 `ServerConfig` 并调用 `minicoding_server::serve`（阻塞当前 task）。
//!
//! `--as-mcp-server` 时切换为 MCP server 模式：把内置工具通过 stdio 暴露给
//! 外部 MCP client（Claude Desktop 等），不启动 HTTP server。
//!
//! ```text
//! minicoding serve --bind 127.0.0.1:8080
//! minicoding serve --port 8080
//! minicoding serve --as-mcp-server       # stdio MCP server
//! ```

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use clap::Args;
use minicoding_core::tool::{ToolContext, ToolRegistry};
#[cfg(feature = "mcp")]
use {
    minicoding_core::tool::CancellationToken,
    minicoding_tools::{
        register_readonly_tools, register_shell_tools, register_task_tools, register_write_tools,
    },
    std::sync::Arc,
};

/// `serve` 子命令参数。
#[derive(Args, Debug)]
pub struct ServeCommand {
    /// 监听地址（如 `127.0.0.1:8080`）。与 `--port` 互斥。
    #[arg(long, conflicts_with = "port")]
    bind: Option<String>,

    /// 监听端口（绑定 `127.0.0.1:<port>`，便捷写法）。与 `--bind` 互斥。
    #[arg(long, conflicts_with = "bind")]
    port: Option<u16>,

    /// LLM provider 类型（`openai`/`anthropic`/`ollama`）。
    #[arg(long, env = "OPENAI_PROVIDER", default_value = "openai")]
    provider: String,

    /// API base URL（省略时按 provider 选默认）。
    #[arg(long, env = "OPENAI_API_BASE")]
    api_base: Option<String>,

    /// API key（Ollama 可省略）。
    #[arg(long, env = "OPENAI_API_KEY")]
    api_key: Option<String>,

    /// 模型名称。
    #[arg(long, env = "OPENAI_MODEL", default_value = "gpt-4o")]
    model: String,

    /// 工作目录。
    #[arg(long, default_value = ".")]
    workdir: String,

    /// 系统 prompt 覆盖。
    #[arg(long)]
    system: Option<String>,

    /// 权限交互超时（秒）。
    #[arg(long, default_value_t = 300)]
    permission_timeout_sec: u64,

    /// 切换为 MCP stdio server 模式（T-M8-3）：把内置工具通过 MCP 协议暴露给
    /// 外部 MCP client（如 Claude Desktop），不启动 HTTP server。
    ///
    /// 启用后 `--bind`/`--port`/`--provider`/`--api-base`/`--api-key`/`--model`/
    /// `--system`/`--permission-timeout-sec` 均被忽略（MCP server 模式不需要 LLM
    /// provider——只把工具调用转发给本地 `ToolRegistry` 执行）。
    #[cfg(feature = "mcp")]
    #[arg(long, default_value_t = false)]
    as_mcp_server: bool,
}

/// 运行 `serve` 子命令：根据 `--as-mcp-server` 分派到 HTTP server 或 MCP stdio server。
///
/// # Errors
/// - bind 地址解析失败；
/// - server 运行时错误（bind 冲突、IO 错误等）；
/// - MCP server 模式下 rmcp 握手失败或 stdio IO 错误。
pub async fn run_serve_command(cmd: &ServeCommand) -> Result<()> {
    // T-M8-3：`--as-mcp-server` 切换为 MCP stdio server 模式
    #[cfg(feature = "mcp")]
    if cmd.as_mcp_server {
        return run_as_mcp_server(cmd).await;
    }

    // 默认：HTTP/SSE server
    let bind_str = cmd
        .bind
        .clone()
        .or_else(|| cmd.port.map(|p| format!("127.0.0.1:{p}")))
        .unwrap_or_else(|| "127.0.0.1:8080".to_string());
    let bind: std::net::SocketAddr = bind_str
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid bind address `{bind_str}`: {e}"))?;

    // 默认 API base（按 provider 选择默认值，与 `minicoding-server` 一致）
    let api_base = cmd
        .api_base
        .clone()
        .unwrap_or_else(|| match cmd.provider.as_str() {
            "ollama" => "http://localhost:11434".to_string(),
            "anthropic" => "https://api.anthropic.com".to_string(),
            _ => "https://api.openai.com".to_string(),
        });

    let api_key = cmd.api_key.clone().unwrap_or_default();

    let cfg = minicoding_server::ServerConfig {
        bind,
        provider_kind: cmd.provider.clone(),
        api_base,
        api_key,
        model: cmd.model.clone(),
        workdir: Utf8PathBuf::from(&cmd.workdir),
        system: cmd.system.clone(),
        permission_timeout_sec: cmd.permission_timeout_sec,
    };

    minicoding_server::serve(cfg)
        .await
        .context("HTTP server 运行失败")
}

/// 启动 MCP stdio server（T-M8-3）：构造 `ToolRegistry` + `ToolContext` 模板，
/// 调 `minicoding_mcp::serve_as_mcp_server` 阻塞当前 task。
///
/// 不构建完整 `Runtime`——MCP server 模式仅做工具执行转发，不需要 LLM provider、
/// 上下文管理、权限策略（权限由调用方 client 自行决定，见 `expose.rs` 文档）。
#[cfg(feature = "mcp")]
async fn run_as_mcp_server(cmd: &ServeCommand) -> Result<()> {
    // 1. 解析 workdir
    let workdir = Utf8PathBuf::from(&cmd.workdir)
        .canonicalize_utf8()
        .unwrap_or_else(|_| Utf8PathBuf::from(&cmd.workdir));

    // 2. 构造 ToolRegistry：注册全部内置工具（fs.read/write/edit/delete/glob/grep +
    //    shell.run + task.create/update/list/spawn + plan.exit）。
    //    不注入 EventBus（task 工具退化为本地状态，不广播 TaskUpdated 事件——MCP
    //    server 模式下没有 Runtime 事件总线消费者）。
    let mut tools = ToolRegistry::new();
    register_readonly_tools(&mut tools);
    register_write_tools(&mut tools);
    register_shell_tools(&mut tools);
    register_task_tools(&mut tools, None);

    // 3. 构造 ToolContext 模板（每轮 call_tool clone 一份）
    //    - 不注入 sandbox_driver / sandbox_policy：shell.run 退化为无 OS 沙箱（MCP
    //      server 模式下，OS 沙箱由调用方进程负责；本 server 仅做工具执行）；
    //    - 不注入 journal：fs.write/edit/delete 不记录改动（无 /undo 需求）；
    //    - timeout 用 `--permission-timeout-sec`（复用既有 flag，避免新增参数）；
    //    - max_output_bytes 用默认 1 MiB（与 Runtime 默认一致）。
    let ctx_template = ToolContext {
        workdir: workdir.clone(),
        session_id: "mcp-stdio".to_string(),
        canceller: CancellationToken::new(),
        env: std::env::vars().collect(),
        timeout: std::time::Duration::from_secs(cmd.permission_timeout_sec),
        max_output_bytes: 1024 * 1024,
        sandbox_driver: None,
        sandbox_policy: None,
        journal: None,
    };

    // 4. 启动 MCP server（阻塞至 client 断开 stdin）
    //    `serve_as_mcp_server` 内部用 `Implementation::new(name, version)` 构造
    //    握手信息，调用方只需传字符串，不接触 `rmcp` 类型。
    let registry = Arc::new(tools);
    minicoding_mcp::serve_as_mcp_server(
        registry,
        ctx_template,
        "minicoding",
        env!("CARGO_PKG_VERSION"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("MCP server 运行失败: {e}"))
}
