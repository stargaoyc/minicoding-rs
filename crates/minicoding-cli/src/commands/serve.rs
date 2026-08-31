//! `serve` 子命令：启动 HTTP/SSE server（T-M8-2，`serve` feature）、
//! MCP stdio server（T-M8-3，`mcp` feature）、NDJSON stdio server（T-M8-4）、
//! ACP stdio server（T-M8-7）或 LSP stdio server（T-M8-8，`lsp` feature）。
//!
//! 默认等价于独立运行 `minicoding-server`，通过 `minicoding serve` 统一入口。
//! 构造 `ServerConfig` 并调用 `minicoding_server::serve`（阻塞当前 task）。
//!
//! `--as-mcp-server` 时切换为 MCP server 模式：把内置工具通过 stdio 暴露给
//! 外部 MCP client（Claude Desktop 等），不启动 HTTP server。
//!
//! `--ndjson` 时切换为 NDJSON stdio 模式：编辑器插件通过 stdin/stdout NDJSON 协议
//! 驱动 minicoding，复用 `SessionManager` + `ServerPrompter`，不启动 HTTP server。
//!
//! `--acp` 时切换为 ACP stdio 模式：支持 ACP（Agent Client Protocol）的客户端
//! （如 Zed 编辑器）通过 JSON-RPC over stdio 嵌入 minicoding，复用 `SessionManager`
//! + `ServerPrompter`，不启动 HTTP server。帧格式为 LSP 风格 `Content-Length`。
//!
//! `--lsp` 时切换为 LSP stdio 模式：基于 `tower-lsp`，可被任何支持 LSP 的编辑器
//! （VS Code/Neovim/Emacs/Helix 等）嵌入。权限交互走 `window/showMessageRequest`
//! （`LspPrompter`），事件流走 `minicoding/event` + `$/progress`，codeAction 提供
//! 解释/重构/修复 quick action（T-M8-9）。
//!
//! ```text
//! minicoding serve --bind 127.0.0.1:8080
//! minicoding serve --port 8080
//! minicoding serve --as-mcp-server       # stdio MCP server
//! minicoding serve --ndjson              # stdio NDJSON server（编辑器插件）
//! minicoding serve --acp                 # stdio ACP server（Zed 等支持 ACP 的客户端）
//! minicoding serve --lsp                 # stdio LSP server（VS Code/Neovim 等）
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
///
// 5 个 bool 是互斥的 mode flag（`--as-mcp-server`/`--ndjson`/`--acp`/`--lsp` 之一，
// 默认 HTTP），用 enum 会改变 CLI 接口（`--mode ndjson` vs `--ndjson`），不符合
// 既有用户体验。此处允许 `struct_excessive_bools`。
#[allow(clippy::struct_excessive_bools)]
#[derive(Args, Debug)]
pub struct ServeCommand {
    /// 监听地址（如 `127.0.0.1:8080`）。与 `--port` 互斥。
    #[arg(long, conflicts_with = "port")]
    bind: Option<String>,

    /// 监听端口（绑定 `127.0.0.1:<port>`，便捷写法）。与 `--bind` 互斥。
    #[arg(long, conflicts_with = "bind")]
    port: Option<u16>,

    /// LLM provider 类型（`openai`/`anthropic`/`ollama`，默认从 `config.toml` 读取）。
    #[arg(long, env = "OPENAI_PROVIDER")]
    provider: Option<String>,

    /// Provider 自定义显示名（用于日志/metrics，不影响协议分派）。
    ///
    /// 连接 `OpenAI` 兼容 API（DeepSeek/Moonshot/vLLM 等）时，设置可读名称使日志
    /// 显示 `provider=deepseek` 而非 `provider=openai`。未设置时回退到 `--provider` 值。
    #[arg(long, env = "MINICODING_PROVIDER_NAME")]
    provider_name: Option<String>,

    /// API base URL（省略时按 provider 选默认）。
    #[arg(long, env = "OPENAI_API_BASE")]
    api_base: Option<String>,

    /// API key（Ollama 可省略）。
    #[arg(long, env = "OPENAI_API_KEY")]
    api_key: Option<String>,

    /// 模型名称（默认从 `config.toml` 读取）。
    #[arg(long, env = "OPENAI_MODEL")]
    model: Option<String>,

    /// 工作目录。
    #[arg(long, default_value = ".")]
    workdir: String,

    /// 系统 prompt 覆盖。
    #[arg(long)]
    system: Option<String>,

    /// 权限交互超时（秒）。
    #[arg(long, default_value_t = 300)]
    permission_timeout_sec: u64,

    /// MCP server 模式（--as-mcp-server）下额外暴露**写类工具**
    /// （fs.write/edit/delete、shell.run 等）。默认只暴露只读工具
    /// （遗留#5：stdio 对端无交互审批通道，写操作默认 fail-closed）。
    #[arg(long)]
    expose_write_tools: bool,

    /// 静态资源目录（M9 `--web`，托管前端 SPA，见 `design.md` §26.7）。
    ///
    /// 设为前端构建产物目录（如 `crates/minicoding-web/dist`）时，HTTP server
    /// 用 `ServeDir` 托管静态文件，`GET /` 返回 `index.html`，支持 SPA history
    /// 路由。仅 HTTP 模式生效（`--ndjson`/`--acp`/`--lsp`/`--as-mcp-server` 忽略）。
    #[arg(long)]
    web: Option<String>,

    /// CORS 允许的来源（M9 `--cors-origin`，可多次指定，见 `design.md` §26.6）。
    ///
    /// 默认（不指定）仅允许本机来源（`localhost`/`127.0.0.1`/`[::1]`，S2）；指定后仅允许
    /// 列出的来源精确匹配。桌面模式同源无需配置。仅 HTTP 模式生效。
    #[arg(long = "cors-origin")]
    cors_origins: Vec<String>,

    /// API 鉴权 token（S1）。省略时自动生成并打印 `SERVER_TOKEN=<t>`。仅 HTTP 模式生效。
    #[arg(long)]
    auth_token: Option<String>,

    /// 关闭 API 鉴权（仅限本机隔离环境；任意进程可完全控制 Agent）。仅 HTTP 模式生效。
    #[arg(long, default_value_t = false)]
    no_auth: bool,

    /// 安全预设（`auto`/`read-only`/`external-sandbox`/`full-access`，见
    /// `security.md` §2.6）。`full-access` = 沙箱外全自动（BypassPermissions +
    /// DangerFullAccess），仅受信容器内使用（C-22 red 警告）。
    #[arg(long, default_value = "auto")]
    preset: String,

    /// 切换为 MCP stdio server 模式（T-M8-3）：把内置工具通过 MCP 协议暴露给
    /// 外部 MCP client（如 Claude Desktop），不启动 HTTP server。
    ///
    /// 启用后 `--bind`/`--port`/`--provider`/`--api-base`/`--api-key`/`--model`/
    /// `--system`/`--permission-timeout-sec` 均被忽略（MCP server 模式不需要 LLM
    /// provider——只把工具调用转发给本地 `ToolRegistry` 执行）。
    #[cfg(feature = "mcp")]
    #[arg(long, default_value_t = false)]
    as_mcp_server: bool,

    /// 切换为 NDJSON stdio server 模式（T-M8-4）：编辑器插件通过 stdin/stdout
    /// NDJSON 协议驱动 minicoding，复用 `SessionManager` + `ServerPrompter`。
    ///
    /// 启用后 `--bind`/`--port` 被忽略（不启动 HTTP server）；`--provider`/
    /// `--api-base`/`--api-key`/`--model`/`--workdir`/`--system`/
    /// `--permission-timeout-sec` 仍生效（用于构造默认 `ServerRuntimeParams`）。
    ///
    /// 协议：stdin 每行一个 `Command` JSON；stdout 每行一个 `EventDto` JSON。
    /// 详见 `minicoding_server::ndjson` 模块文档。
    #[arg(long, default_value_t = false)]
    ndjson: bool,

    /// 切换为 ACP stdio server 模式（T-M8-7）：支持 ACP（Agent Client Protocol）
    /// 的客户端（如 Zed 编辑器）通过 JSON-RPC over stdio 嵌入 minicoding。
    ///
    /// 启用后 `--bind`/`--port` 被忽略（不启动 HTTP server）；`--provider`/
    /// `--api-base`/`--api-key`/`--model`/`--workdir`/`--system`/
    /// `--permission-timeout-sec` 仍生效（用于构造默认 `ServerRuntimeParams`）。
    ///
    /// 协议：JSON-RPC 2.0 over stdio，LSP 风格 `Content-Length` 帧分隔。
    /// 方法：`initialize`/`newConversation`/`prompt`/`cancel`/`shutdown`/
    /// `resolvePermission` + `session/update` 通知。
    /// 详见 `minicoding_server::acp` 模块文档。
    #[arg(long, default_value_t = false)]
    acp: bool,

    /// 切换为 LSP stdio server 模式（T-M8-8，`lsp` feature）：基于 `tower-lsp`，
    /// 可被任何支持 LSP 的编辑器（VS Code/Neovim/Emacs/Helix 等）嵌入。
    ///
    /// 启用后 `--bind`/`--port` 被忽略（不启动 HTTP server）；`--provider`/
    /// `--api-base`/`--api-key`/`--model`/`--workdir`/`--system`/
    /// `--permission-timeout-sec` 仍生效（用于构造默认 `ServerRuntimeParams`）。
    ///
    /// 语义映射（见 `design.md` §24）：
    /// - `initialize` → 能力协商（返回 `executeCommand`/`codeAction` 能力）；
    /// - `workspace/executeCommand` → `minicoding.ask`（发送 prompt）/ `minicoding.cancel`（取消 turn）
    ///   / `minicoding.explain`/`refactor`/`fix`（codeAction 触发）；
    /// - `$/progress` → 流式 token / 工具进度（`WorkDoneProgress::Report`）；
    /// - `minicoding/event` → 事件广播（携带 `seq`，与 SSE cursor 一致）；
    /// - `window/showMessageRequest` → 权限确认（`LspPrompter` 点对点）；
    /// - `textDocument/codeAction` → AI 快速操作（解释/重构/修复选中代码）。
    ///
    /// 详见 `minicoding_server::lsp` 模块文档。
    #[cfg(feature = "lsp")]
    #[arg(long, default_value_t = false)]
    lsp: bool,
}

/// 从 `config.toml` + CLI 参数解析最终 provider 配置（CLI > env > `config.toml` > 默认）。
///
/// `serve` 子命令的 4 种模式（HTTP/NDJSON/ACP/LSP）共用此解析逻辑，确保配置一致。
/// 与 CLI 单次/交互模式的 `builder::build_runtime` 优先级对齐：CLI 参数覆盖配置文件。
struct ResolvedServeConfig {
    provider_kind: String,
    provider_name: Option<String>,
    api_base: String,
    api_key: String,
    model: String,
    /// 模型参数/上下文默认值：config.toml > 内置默认
    timeout_sec: u64,
    max_retries: u32,
    small_model: Option<String>,
    turn_timeout_sec: u64,
    compress: bool,
}

fn resolve_provider_config(cmd: &ServeCommand) -> ResolvedServeConfig {
    let file_config = minicoding_core::config::load_config().unwrap_or_default();
    let file_provider = &file_config.provider;

    let provider_kind = cmd
        .provider
        .clone()
        .unwrap_or_else(|| file_provider.default.clone());
    let provider_name = cmd.provider_name.clone().or(file_provider.name.clone());
    let api_key = cmd
        .api_key
        .clone()
        .unwrap_or_else(|| file_provider.api_key.clone());
    let model = cmd
        .model
        .clone()
        .unwrap_or_else(|| file_provider.model.clone());

    // api_base：CLI > config.toml > 按 provider 选默认
    let api_base = cmd.api_base.clone().unwrap_or_else(|| {
        if file_provider.api_base.is_empty() {
            match provider_kind.as_str() {
                "ollama" => "http://localhost:11434".to_string(),
                "anthropic" => "https://api.anthropic.com".to_string(),
                _ => "https://api.openai.com".to_string(),
            }
        } else {
            file_provider.api_base.clone()
        }
    });

    ResolvedServeConfig {
        provider_kind,
        provider_name,
        api_base,
        api_key,
        model,
        timeout_sec: file_provider.timeout_sec,
        max_retries: file_provider.max_retries,
        small_model: file_provider.small.as_ref().map(|s| s.model.clone()),
        turn_timeout_sec: file_config.context.turn_timeout_sec,
        compress: file_config.context.compress,
    }
}

/// 运行 `serve` 子命令：根据 `--as-mcp-server`/`--ndjson`/`--acp`/`--lsp` 分派到对应模式。
///
/// # Errors
/// - bind 地址解析失败（HTTP 模式）；
/// - server 运行时错误（bind 冲突、IO 错误等）；
/// - MCP server 模式下 rmcp 握手失败或 stdio IO 错误；
/// - NDJSON 模式下 stdin 读取或 stdout 写入错误；
/// - ACP 模式下帧解析或 stdio IO 错误；
/// - LSP 模式下 tower-lsp IO 错误。
pub async fn run_serve_command(cmd: &ServeCommand) -> Result<()> {
    // T-M8-3：`--as-mcp-server` 切换为 MCP stdio server 模式
    #[cfg(feature = "mcp")]
    if cmd.as_mcp_server {
        return run_as_mcp_server(cmd).await;
    }

    // T-M8-4：`--ndjson` 切换为 NDJSON stdio server 模式
    if cmd.ndjson {
        return run_as_ndjson_server(cmd).await;
    }

    // T-M8-7：`--acp` 切换为 ACP stdio server 模式
    if cmd.acp {
        return run_as_acp_server(cmd).await;
    }

    // T-M8-8：`--lsp` 切换为 LSP stdio server 模式
    #[cfg(feature = "lsp")]
    if cmd.lsp {
        return run_as_lsp_server(cmd).await;
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

    // R9 SRV-1：无鉴权 + 非本机绑定 = 无鉴权的远程 Agent 控制面（可发消息触发
    // 工具链、代答权限弹窗、create_session workdir 由客户端任意指定）。
    // Docker 部署必须 bind 0.0.0.0（容器内 127.0.0.1 不可外部访问），文档又未提
    // auth——组合一旦发生即失控。**拒绝启动**（默认 127.0.0.1 不触发；要远程
    // 必须先启用鉴权 `--auth-token`/`MINICODING_AUTH_TOKEN` 再绑定非本机地址）。
    if cmd.no_auth && !bind.ip().is_loopback() {
        anyhow::bail!(
            "拒绝启动：`--no-auth`（关闭鉴权）与 `--bind {bind_str}`（非本机地址）组合 = \
             无鉴权远程控制面。请提供 `--auth-token`/`MINICODING_AUTH_TOKEN` 启用鉴权后 \
             再绑定非本机地址。"
        );
    }

    // 解析 provider 配置（CLI > env > config.toml > 默认）
    let resolved = resolve_provider_config(cmd);

    // S1：鉴权 token——显式指定 > 自动生成（打印 SERVER_TOKEN=）> --no-auth 关闭。
    // FE-5（2026-08-25 R2 审查）：token 经 MINICODING_AUTH_TOKEN env 下传时
    // （desktop sidecar 场景）不回显明文——stdout 可能被父进程落日志。
    let token_from_env = if cmd.auth_token.is_none() {
        std::env::var("MINICODING_AUTH_TOKEN").ok()
    } else {
        None
    };
    let auth_token = if cmd.no_auth {
        eprintln!(
            "WARNING: API 鉴权已禁用（--no-auth）：本机任意进程可读取会话、代答权限、执行命令"
        );
        None
    } else if let Some(t) = token_from_env {
        // R8 ARCH-5：掩码统一走 core `mask_token`（前 4 字符 + ***）
        println!(
            "SERVER_TOKEN={} (from MINICODING_AUTH_TOKEN)",
            minicoding_core::util::mask_token(&t)
        );
        Some(t)
    } else {
        Some(
            cmd.auth_token
                .clone()
                .unwrap_or_else(minicoding_server::generate_auth_token),
        )
    };
    if let Some(t) = &auth_token
        && cmd.auth_token.is_none()
        && std::env::var("MINICODING_AUTH_TOKEN").as_deref() != Ok(t.as_str())
    {
        println!("SERVER_TOKEN={t}");
    }

    let cfg = minicoding_server::ServerConfig {
        bind,
        auth_token,
        provider_kind: resolved.provider_kind,
        provider_name: resolved.provider_name,
        api_base: resolved.api_base,
        api_key: resolved.api_key,
        model: resolved.model,
        workdir: Utf8PathBuf::from(&cmd.workdir),
        system: cmd.system.clone(),
        permission_timeout_sec: cmd.permission_timeout_sec,
        web_dir: cmd.web.as_deref().map(Utf8PathBuf::from),
        cors_origins: cmd.cors_origins.clone(),
        preset: cmd.preset.clone(),
        timeout_sec: resolved.timeout_sec,
        max_retries: resolved.max_retries,
        small_model: resolved.small_model,
        turn_timeout_sec: resolved.turn_timeout_sec,
        compress: resolved.compress,
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
    //
    //    PTM-1/FE-2（2026-08-25 R2 审查）：只读暴露的判据统一为
    //    `SideEffect::None`——此前旗标只 gate 了 fs 写工具，shell.run（Command）、
    //    git.apply（FileWrite）、web.fetch/search（Network）无条件注册且
    //    prompter=None 直通 execute，"默认只读"名不副实。现在凡有副作用的
    //    工具组一律需要 `--expose-write-tools`。
    let mut tools = ToolRegistry::new();
    register_readonly_tools(&mut tools);
    register_task_tools(&mut tools, None);
    if cmd.expose_write_tools {
        // 遗留#5：MCP server 模式默认只读暴露（对端无审批通道，写 fail-closed）。
        // 仅在显式传入 `--expose-write-tools` 时注册副作用工具——此前旗标之后
        // 紧跟一条无条件注册调用，导致 fail-closed 完全失效（2026-08-25 审查修复）。
        register_write_tools(&mut tools);
        register_shell_tools(&mut tools);
        // T-M8-5：git + web 工具组（git.diff 只读但与 apply 同组注册；
        // web.* 为 Network 副作用，同属需显式暴露）
        minicoding_tools::register_git_tools(&mut tools);
        #[cfg(feature = "web")]
        minicoding_tools::register_web_tools(&mut tools);
    } else {
        // 只读暴露保留 git.diff（SideEffect::None）
        let mut git_all = ToolRegistry::new();
        minicoding_tools::register_git_tools(&mut git_all);
        if let Some(diff) = git_all.get("git.diff") {
            tools.register(diff.clone());
        }
    }

    // 3. 构造 ToolContext 模板（每轮 call_tool clone 一份）
    //    - 不注入 sandbox_driver / sandbox_policy：shell.run 退化为无 OS 沙箱（MCP
    //      server 模式下，OS 沙箱由调用方进程负责；本 server 仅做工具执行）；
    //    - 不注入 journal：fs.write/edit/delete 不记录改动（无 /undo 需求）；
    //    - timeout 用 `--permission-timeout-sec`（复用既有 flag，避免新增参数）；
    //    - max_output_bytes 用默认 1 MiB（与 Runtime 默认一致）；
    //    - env 用白名单子集（C-04）：此前 `std::env::vars().collect()` 全量透传，
    //      会把凭证变量经 ctx.env 下传 git/shell 子进程（2026-08-23 审查 §6-P1
    //      单一来源化时发现的旁路，一并修复）。
    let ctx_template = ToolContext {
        workdir: workdir.clone(),
        session_id: "mcp-stdio".to_string(),
        canceller: CancellationToken::new(),
        env: minicoding_core::tool::sanitized_env(),
        timeout: std::time::Duration::from_secs(cmd.permission_timeout_sec),
        max_output_bytes: 1024 * 1024,
        max_read_bytes: 1024 * 1024,
        sandbox_driver: None,
        sandbox_policy: None,
        journal: None,
        prompter: None,
        events: None,
        audit: None,
        sandbox_fail_closed: false,
    };

    // 4. 启动 MCP server（阻塞至 client 断开 stdin）
    //    `serve_as_mcp_server` 内部用 `Implementation::new(name, version)` 构造
    //    握手信息，调用方只需传字符串，不接触 `rmcp` 类型。
    //    R8 ARCH-3：注入 FileAuditSink——MCP 暴露的破坏性工具（--expose-write-tools
    //    时）此前零审计，违背 AGENTS.md §5.5。
    let registry = Arc::new(tools);
    let audit_path = minicoding_core::paths::audit_log_path().ok().map(|p| {
        std::sync::Arc::new(minicoding_storage::FileAuditSink::new(p))
            as std::sync::Arc<dyn minicoding_core::storage::AuditSink>
    });
    minicoding_mcp::serve_as_mcp_server(
        registry,
        ctx_template,
        "minicoding",
        env!("CARGO_PKG_VERSION"),
        audit_path,
    )
    .await
    .map_err(|e| anyhow::anyhow!("MCP server 运行失败: {e}"))
}

/// 启动 NDJSON stdio server（T-M8-4）：构造 `SessionManager` + 默认 `ServerRuntimeParams`，
/// 调 `minicoding_server::serve_ndjson` 阻塞当前 task。
///
/// 与 HTTP 模式共享 `ServerRuntimeParams` 构造（`provider`/`api_base`/`api_key`/`model`/
/// `workdir`/`system`/`permission_timeout_sec`），但不绑定 TCP 端口——通过 stdin/stdout 与
/// 编辑器插件通信。
///
/// 协议：stdin 每行一个 `Command` JSON；stdout 每行一个 `EventDto` JSON。
/// 详见 `minicoding_server::ndjson` 模块文档。
async fn run_as_ndjson_server(cmd: &ServeCommand) -> Result<()> {
    use minicoding_server::ServerRuntimeParams;

    // 1. 解析 provider 配置（CLI > env > config.toml > 默认，与 HTTP 模式一致）
    let resolved = resolve_provider_config(cmd);

    // 2. 构造默认 ServerRuntimeParams（CreateSession 命令未指定覆盖时用此默认）
    let (preset_mode, preset_policy) = resolve_stdio_preset(cmd)?;
    let params = ServerRuntimeParams {
        provider_kind: resolved.provider_kind,
        provider_name: resolved.provider_name,
        api_base: resolved.api_base,
        api_key: resolved.api_key,
        model: resolved.model,
        workdir: Utf8PathBuf::from(&cmd.workdir),
        system: cmd.system.clone(),
        // FE-11（2026-08-26 R3 审查）：stdio 三分支此前硬编码 Default +
        // WorkspaceWrite，完全忽略 `--preset`（IDE 无法选 read-only）。复用
        // HTTP 模式同款 build_preset_policy 解析（full-access 属高危预设，
        // stdio 场景同样要求显式 --preset full-access 传入即视为选定，警告
        // 经日志展示）。
        permission_mode: preset_mode,
        sandbox_policy: preset_policy,
        timeout_sec: resolved.timeout_sec,
        max_retries: resolved.max_retries,
        small_model: resolved.small_model,
        turn_timeout_sec: resolved.turn_timeout_sec,
        compress: resolved.compress,
    };

    // 3. 构造 SessionManager
    let permission_timeout = std::time::Duration::from_secs(cmd.permission_timeout_sec);
    let mgr = std::sync::Arc::new(minicoding_server::SessionManager::new(
        params,
        permission_timeout,
    ));

    // 4. 启动 NDJSON server（阻塞至 stdin EOF）
    minicoding_server::serve_ndjson(mgr)
        .await
        .map_err(|e| anyhow::anyhow!("NDJSON server 运行失败: {e}"))
}

/// 启动 ACP stdio server（T-M8-7）：构造 `SessionManager` + 默认 `ServerRuntimeParams`，
/// 调 `minicoding_server::serve_acp` 阻塞当前 task。
///
/// 与 NDJSON 模式共享 `ServerRuntimeParams` 构造（`provider`/`api_base`/`api_key`/`model`/
/// `workdir`/`system`/`permission_timeout_sec`），但通过 LSP 风格 `Content-Length` 帧与
/// ACP 兼容客户端（如 Zed）通信。
///
/// 协议：JSON-RPC 2.0 over stdio，方法 `initialize`/`newConversation`/`prompt`/
/// `cancel`/`shutdown`/`resolvePermission` + `session/update` 通知。
/// 详见 `minicoding_server::acp` 模块文档。
async fn run_as_acp_server(cmd: &ServeCommand) -> Result<()> {
    use minicoding_server::ServerRuntimeParams;

    // 1. 解析 provider 配置（CLI > env > config.toml > 默认，与 HTTP/NDJSON 模式一致）
    let resolved = resolve_provider_config(cmd);

    // 2. 构造默认 ServerRuntimeParams（newConversation 未指定覆盖时用此默认）
    let (preset_mode, preset_policy) = resolve_stdio_preset(cmd)?;
    let params = ServerRuntimeParams {
        provider_kind: resolved.provider_kind,
        provider_name: resolved.provider_name,
        api_base: resolved.api_base,
        api_key: resolved.api_key,
        model: resolved.model,
        workdir: Utf8PathBuf::from(&cmd.workdir),
        system: cmd.system.clone(),
        // FE-11（2026-08-26 R3 审查）：stdio 三分支此前硬编码 Default +
        // WorkspaceWrite，完全忽略 `--preset`（IDE 无法选 read-only）。复用
        // HTTP 模式同款 build_preset_policy 解析（full-access 属高危预设，
        // stdio 场景同样要求显式 --preset full-access 传入即视为选定，警告
        // 经日志展示）。
        permission_mode: preset_mode,
        sandbox_policy: preset_policy,
        timeout_sec: resolved.timeout_sec,
        max_retries: resolved.max_retries,
        small_model: resolved.small_model,
        turn_timeout_sec: resolved.turn_timeout_sec,
        compress: resolved.compress,
    };

    // 3. 构造 SessionManager
    let permission_timeout = std::time::Duration::from_secs(cmd.permission_timeout_sec);
    let mgr = std::sync::Arc::new(minicoding_server::SessionManager::new(
        params,
        permission_timeout,
    ));

    // 4. 启动 ACP server（阻塞至 stdin EOF 或 shutdown）
    minicoding_server::serve_acp(mgr)
        .await
        .map_err(|e| anyhow::anyhow!("ACP server 运行失败: {e}"))
}

/// 启动 LSP stdio server（T-M8-8）：构造 `SessionManager` + 默认 `ServerRuntimeParams`，
/// 调 `minicoding_server::serve_lsp` 阻塞当前 task。
///
/// 与 ACP 模式共享 `ServerRuntimeParams` 构造，但基于 `tower-lsp` 框架。会话惰性
/// 创建于首次 `workspace/executeCommand`；权限交互走 `LspPrompter`（`window/showMessageRequest`）；
/// 事件流走 `minicoding/event` + `$/progress`；codeAction 提供解释/重构/修复。
///
/// 协议：LSP over stdio，方法 `initialize`/`shutdown`/`workspace/executeCommand`/
/// `textDocument/codeAction` + `$/progress`/`minicoding/event`/`window/showMessageRequest`。
/// 详见 `minicoding_server::lsp` 模块文档。
#[cfg(feature = "lsp")]
async fn run_as_lsp_server(cmd: &ServeCommand) -> Result<()> {
    use minicoding_server::ServerRuntimeParams;

    // 1. 解析 provider 配置（CLI > env > config.toml > 默认，与 HTTP/NDJSON/ACP 模式一致）
    let resolved = resolve_provider_config(cmd);

    // 2. 构造默认 ServerRuntimeParams（executeCommand 创建会话时用）
    let (preset_mode, preset_policy) = resolve_stdio_preset(cmd)?;
    let params = ServerRuntimeParams {
        provider_kind: resolved.provider_kind,
        provider_name: resolved.provider_name,
        api_base: resolved.api_base,
        api_key: resolved.api_key,
        model: resolved.model,
        workdir: Utf8PathBuf::from(&cmd.workdir),
        system: cmd.system.clone(),
        // FE-11（2026-08-26 R3 审查）：stdio 三分支此前硬编码 Default +
        // WorkspaceWrite，完全忽略 `--preset`（IDE 无法选 read-only）。复用
        // HTTP 模式同款 build_preset_policy 解析（full-access 属高危预设，
        // stdio 场景同样要求显式 --preset full-access 传入即视为选定，警告
        // 经日志展示）。
        permission_mode: preset_mode,
        sandbox_policy: preset_policy,
        timeout_sec: resolved.timeout_sec,
        max_retries: resolved.max_retries,
        small_model: resolved.small_model,
        turn_timeout_sec: resolved.turn_timeout_sec,
        compress: resolved.compress,
    };

    // 3. 构造 SessionManager
    let permission_timeout = std::time::Duration::from_secs(cmd.permission_timeout_sec);
    let mgr = std::sync::Arc::new(minicoding_server::SessionManager::new(
        params,
        permission_timeout,
    ));

    // 4. 启动 LSP server（阻塞至 stdin EOF 或 shutdown）
    minicoding_server::serve_lsp(mgr, permission_timeout)
        .await
        .map_err(|e| anyhow::anyhow!("LSP server 运行失败: {e}"))
}

/// FE-11（2026-08-26 R3 审查）：stdio 三分支的 `--preset` 解析。
///
/// 复用 server `build_preset_policy`（与 HTTP 模式同一映射表，消除双事实源）；
/// full-access 警告经 tracing 展示。未知预设返回配置错误。
fn resolve_stdio_preset(
    cmd: &ServeCommand,
) -> anyhow::Result<(
    minicoding_core::policy::PermissionMode,
    minicoding_core::sandbox::SandboxPolicy,
)> {
    let workdir = Utf8PathBuf::from(&cmd.workdir);
    let preset = cmd.preset.as_str();
    let (mode, policy, warning) = minicoding_server::http::build_preset_policy(preset, &workdir)
        .map_err(|e| anyhow::anyhow!("{} (status {})", e.message, e.status))?;
    if let Some(w) = warning {
        tracing::warn!("{w}");
    }
    Ok((mode, policy))
}
