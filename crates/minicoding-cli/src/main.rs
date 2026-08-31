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
//! - Plan 模式启动：`minicoding --plan` 进入只读规划模式（T-M5-8）
//! - REPL 斜杠命令：`/plan`/`/undo`/`/help`/`/quit`（T-M5-8）
//! - 流式 token 渲染（实时打印到 stdout）
//! - 配置从环境变量或默认值加载（`OPENAI_API_KEY`/`OPENAI_API_BASE`/`OPENAI_MODEL`）
//! - 只读工具组（`fs.read`/`fs.list`/`fs.glob`/`fs.grep`）自动注册
//!
//! ## 退出码
//!
//! 成功 0；运行时错误 1；配置错误 2；中断 130。
//!
//! 详见 `docs/modules.md` §12。

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use minicoding_cli::builder::{self, SessionLoadMode};
#[cfg(feature = "mcp")]
use minicoding_cli::commands::McpCommand;
#[cfg(feature = "serve")]
use minicoding_cli::commands::ServeCommand;
use minicoding_cli::commands::{
    BackupCommand, CredCommand, DoctorCommand, ExecCommand, SessionCommand,
};
use minicoding_cli::{commands, otel_init, session};
use minicoding_core::model::{TurnOutcome, UserInput};
use minicoding_core::policy::PermissionMode;
use minicoding_core::runtime::Event;

/// 顶层子命令（除默认运行模式外的独立操作）。
///
/// `session`/`doctor`/`mcp`/`cred`/`backup` 不构建 `Runtime`，无需 API key。
/// `exec` 构建完整 `Runtime` 但强制非交互（CI 场景）。
/// `serve` 委托 `minicoding_server::serve`（`serve` feature）。
#[derive(Subcommand, Debug)]
enum Command {
    /// 会话管理（列出 / 删除）。
    #[command(name = "session")]
    Session(SessionCommand),
    /// 非交互批量执行（CI/脚本场景，T-M4-10）。
    #[command(name = "exec")]
    Exec(ExecCommand),
    /// 安全自检（沙箱驱动/硬化状态，T-M4-10）。
    #[command(name = "doctor")]
    Doctor(DoctorCommand),
    /// MCP server 管理（list/approve/reject/reset，T-M4-10，`mcp` feature）。
    #[cfg(feature = "mcp")]
    #[command(name = "mcp")]
    Mcp(McpCommand),
    /// 凭证管理（store/load/delete，T-M4-11）。
    #[command(name = "cred")]
    Cred(CredCommand),
    /// 启动 HTTP/SSE server（T-M8-2，`serve` feature）。
    #[cfg(feature = "serve")]
    #[command(name = "serve")]
    Serve(ServeCommand),
    /// 备份管理（create/list，S-05）。
    #[command(name = "backup")]
    Backup(BackupCommand),
    /// 记忆管理（list/read/clear，R10-08；`memory` feature）。
    #[cfg(feature = "memory")]
    #[command(name = "memory")]
    Memory(minicoding_cli::commands::MemoryCommand),
}

/// minicoding — 终端 AI Coding 助手
#[derive(Parser, Debug)]
#[command(name = "minicoding", version, about, long_about = None)]
#[allow(clippy::struct_excessive_bools)] // CLI flag 集合，各 bool 语义独立，无需重构为 enum
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

    /// LLM provider 类型（`openai`/`anthropic`/`ollama`，覆盖 `config.provider.default`，T-M6-5）
    ///
    /// 决定调用哪家 provider 的 API 协议：`openai` 走 `/v1/chat/completions` SSE；
    /// `anthropic` 走 `/v1/messages` SSE；`ollama` 走 `/api/chat` NDJSON。
    /// 配套的 `--api-base`/`--api-key`/`--model` 按所选 provider 解释。
    #[arg(long)]
    provider: Option<String>,

    /// Provider 自定义显示名（用于日志/metrics，不影响协议分派）
    ///
    /// 连接 `OpenAI` 兼容 API（DeepSeek/Moonshot/vLLM 等）时，设置可读名称使日志
    /// 显示 `provider=deepseek` 而非 `provider=openai`。未设置时回退到 `--provider` 值。
    #[arg(long, env = "MINICODING_PROVIDER_NAME")]
    provider_name: Option<String>,

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

    /// 启动时进入 Plan 模式（只读强制 + plan.exit，T-M5-8）。
    ///
    /// 等价于 REPL 内执行 `/plan`：副作用工具被硬门拒绝，模型只能探查与规划，
    /// 调 `plan.exit` 后切换到 Default/AcceptEdits 进入执行阶段（见 `design.md` §16）。
    #[arg(long)]
    plan: bool,

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

/// FE-7：`--resume` 时从快照还原持久化的 `permission_mode`（安全上下文跨重启）。
///
/// 仅 Resume 模式还原：Replay 保持 C-06 默认禁副作用语义、Fork 是新会话，均不
/// 还原。必须在 `init_event_stream` 之后调用——`set_mode` 触发的
/// `PermissionModeChanged` 会追加到事件流，seq 计数器需已初始化。
/// `sandbox_preset` 仅记录诊断日志：CLI 侧 preset 由 builder 进程级决策，
/// 不做热切换（C-22）。无快照（旧会话无事件流/新会话）时静默保持启动默认。
async fn apply_restored_security_context(
    rt: &minicoding_core::runtime::Runtime,
    mode: &SessionLoadMode,
) {
    let SessionLoadMode::Resume(session_id) = mode else {
        return;
    };
    let Ok(dir) = minicoding_core::paths::sessions_dir() else {
        return;
    };
    let store = minicoding_storage::JsonlSnapshotStore::new(dir);
    let Ok(Some(snapshot)) = store.load_sync(session_id) else {
        return;
    };
    if let Some(raw) = snapshot.state.permission_mode.as_deref() {
        // 与写入侧同规范：serde `snake_case` 字符串 ↔ `PermissionMode`
        if let Ok(restored) =
            serde_json::from_value::<PermissionMode>(serde_json::Value::String(raw.to_string()))
        {
            rt.plan_controller().set_mode(restored).await;
            tracing::info!(
                session = %session_id,
                mode = ?restored,
                "permission_mode restored from snapshot (--resume)"
            );
        } else {
            tracing::warn!(
                session = %session_id,
                raw,
                "unknown permission_mode in snapshot; keeping startup default"
            );
        }
    }
    if let Some(preset) = snapshot.state.sandbox_preset.as_deref() {
        tracing::info!(
            session = %session_id,
            snapshot_preset = preset,
            "sandbox preset recorded in snapshot (process-level decision, not hot-switched)"
        );
    }
}

/// Event Sourcing 初始化 + 恢复会话历史（交互/单次两分支共用的前置步骤）。
///
/// 返回 `false` 表示初始化失败（已输出 stderr，调用方返回退出码 1）。
async fn init_event_stream_and_history(
    rt: &minicoding_core::runtime::Runtime,
    has_preloaded_session: bool,
) -> bool {
    // Event Sourcing：初始化事件流（新会话持久化 SessionCreated，
    // 恢复会话加载 seq 计数器 + snapshot，见 `design.md` §25.1）。
    // 必须在 `restore_history` 之前调用（`init_event_stream` 设置
    // `durable_seq`/`event_seq`，`restore_history` 不依赖这些字段，
    // 但语义上事件流应先于 turn 初始化）。
    if let Err(e) = rt.init_event_stream().await {
        eprintln!("初始化事件流失败: {e}");
        return false;
    }
    if has_preloaded_session && let Err(e) = rt.restore_history().await {
        eprintln!("恢复会话历史失败: {e}");
        return false;
    }
    true
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // 初始化日志/trace（T-M0-4：OTLP 导出 + 降级 fmt）
    // `OTEL_EXPORTER_OTLP_ENDPOINT` 配置且 `otel` feature 启用时安装 OTLP layer；
    // 否则降级为纯本地 fmt 日志。guard drop 时 flush OTLP exporter。
    let _otel_guard = otel_init::init_tracing(cli.verbose);

    // 顶层子命令分派：session/doctor/mcp 不构建 Runtime；exec 构建 Runtime 但非交互。
    match &cli.command {
        Some(Command::Session(sess_cmd)) => {
            commands::run_session_command(sess_cmd).context("session 子命令失败")?;
            return Ok(());
        }
        Some(Command::Exec(exec_cmd)) => {
            let code = commands::exec::run_exec_command(exec_cmd).context("exec 子命令失败")?;
            std::process::exit(code);
        }
        Some(Command::Doctor(doctor_cmd)) => {
            commands::doctor::run_doctor_command(doctor_cmd);
            return Ok(());
        }
        #[cfg(feature = "mcp")]
        Some(Command::Mcp(mcp_cmd)) => {
            commands::mcp::run_mcp_command(mcp_cmd, &cli.workdir).context("mcp 子命令失败")?;
            return Ok(());
        }
        Some(Command::Cred(cred_cmd)) => {
            commands::run_cred_command(cred_cmd).context("cred 子命令失败")?;
            return Ok(());
        }
        Some(Command::Backup(backup_cmd)) => {
            commands::run_backup_command(backup_cmd).context("backup 子命令失败")?;
            return Ok(());
        }
        #[cfg(feature = "memory")]
        Some(Command::Memory(mem_cmd)) => {
            commands::run_memory_command(mem_cmd).context("memory 子命令失败")?;
            return Ok(());
        }
        #[cfg(feature = "serve")]
        Some(Command::Serve(serve_cmd)) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(commands::serve::run_serve_command(serve_cmd))?;
            return Ok(());
        }
        None => {}
    }

    // 解析会话加载模式（互斥校验）
    let mode = resolve_session_mode(&cli)?;
    let has_preloaded_session = !matches!(mode, SessionLoadMode::None);

    // 分派：`--session` 或无 prompt → 交互 REPL；有 prompt 且无 `--session` → 单次
    let interactive = cli.session || cli.prompt.is_none();

    // 构建 Runtime（默认沙箱策略 WorkspaceWrite，由 builder 内部注入）
    #[cfg_attr(not(feature = "mcp"), allow(unused_mut))]
    let (mut rt, memory_slot) = builder::build_runtime_with_memory_slot(
        cli.provider.as_deref(),
        cli.provider_name.as_deref(),
        cli.api_base.as_deref(),
        cli.api_key.as_deref(),
        cli.model.as_deref(),
        &cli.workdir,
        cli.system.as_deref(),
        &mode,
        None,
        cli.plan,
        None,
    )
    .context("构建 Runtime 失败")?;

    if cli.plan {
        anstream::eprintln!(
            "\x1b[2m已启动 Plan 模式：副作用工具被硬门拒绝，调 plan.exit 后进入执行阶段\x1b[0m",
        );
    }

    // 运行
    let runtime = tokio::runtime::Runtime::new()?;
    let exit_code = if interactive {
        runtime.block_on(async {
            // MCP client 接线（§7-P0）：加载配置 → C-24 批准 → 启动 → 注册工具
            #[cfg(feature = "mcp")]
            {
                let prompter = minicoding_policy::InteractivePrompter::new();
                if let Err(e) = minicoding_sdk::mcp_setup::attach_mcp_tools(
                    &mut rt,
                    &camino::Utf8PathBuf::from(&cli.workdir),
                    std::sync::Arc::new(prompter),
                )
                .await
                {
                    eprintln!("MCP 工具注册失败: {e}");
                }
            }
            // Event Sourcing：初始化事件流 + 恢复会话历史（共用前置步骤，
            // 见 `init_event_stream_and_history`；`--resume` 的安全上下文还原
            // 依赖已初始化的 seq 计数器，故置于其后）。
            if !init_event_stream_and_history(&rt, has_preloaded_session).await {
                return 1;
            }
            apply_restored_security_context(&rt, &mode).await;
            session::run_interactive_session_with_memory_slot(&rt, Some(memory_slot)).await
        })
    } else {
        let prompt = cli.prompt.expect("单次模式 prompt 必为 Some");
        runtime.block_on(async {
            if !init_event_stream_and_history(&rt, has_preloaded_session).await {
                return 1;
            }
            apply_restored_security_context(&rt, &mode).await;
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
