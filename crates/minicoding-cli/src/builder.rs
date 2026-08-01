//! Runtime 组装：根据 CLI 参数与环境变量构造 `Runtime`。
//!
//! 组装顺序：config → provider → context → storage → tools → policy/prompter/audit
//! → `RuntimeBuilder`。

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use minicoding_context::SimpleContextManager;
use minicoding_core::config::{RuntimeConfig, config_hash};
use minicoding_core::memory::MemoryStore;
use minicoding_core::model::{MemoryError, Message, Session, SessionId, ToolError};
use minicoding_core::policy::{PermissionPolicy, PermissionPrompter};
use minicoding_core::provider::BoxFuture;
use minicoding_core::runtime::{Runtime, RuntimeBuilder};
use minicoding_core::storage::AuditSink;
use minicoding_core::tool::ToolRegistry;
use minicoding_memory::{AutoCategory, AutoMemory, LongTermMemory};
use minicoding_policy::{BuiltinPolicy, InteractivePrompter, NonInteractivePrompter, ReplayPolicy};
use minicoding_providers::OpenAiProvider;
use minicoding_storage::{FileAuditSink, JsonlStorage};
use minicoding_tools::{
    AutoMemoryWriter, MemoryCategory, MemoryWrite, register_readonly_tools, register_shell_tools,
    register_write_tools,
};
use std::io::IsTerminal;
use std::sync::Arc;
use time::OffsetDateTime;

/// 会话加载模式（`--resume`/`--replay`/`--fork-session` 互斥，T-M3-10）。
///
/// - `None`：新建空会话；
/// - `Resume`：加载历史会话继续提问（原 id，原存储文件追加写）；
/// - `Replay`：回放历史会话，默认禁副作用（C-06），`allow_side_effects=true` 时
///   走正常权限流程；
/// - `Fork`：从原会话分叉到新会话（新 id，复制前缀消息到新文件，原文件不变）。
#[derive(Debug, Clone)]
pub enum SessionLoadMode {
    /// 新建空会话（默认）。
    None,
    /// `--resume <id>`：恢复会话继续提问。
    Resume(String),
    /// `--replay <id>`：回放历史，默认禁副作用（C-06）。
    Replay {
        id: String,
        allow_side_effects: bool,
    },
    /// `--fork-session <id>`：从原会话分叉到新会话。
    Fork(String),
}

/// `AutoMemory` → `AutoMemoryWriter` 适配器。
///
/// 桥接 `minicoding-tools` 的 `AutoMemoryWriter` trait 与 `minicoding-memory`
/// 的 `AutoMemory` 具体实现（`MemoryCategory` → `AutoCategory` 转换）。
struct AutoMemoryAdapter {
    inner: Arc<AutoMemory>,
}

impl AutoMemoryWriter for AutoMemoryAdapter {
    fn add_entry(
        &self,
        topic: String,
        content: String,
        category: MemoryCategory,
        confidence: f64,
    ) -> BoxFuture<'_, Result<usize, ToolError>> {
        Box::pin(async move {
            let cat = match category {
                MemoryCategory::Correction => AutoCategory::Correction,
                MemoryCategory::Pitfall => AutoCategory::Pitfall,
                MemoryCategory::Pref => AutoCategory::Pref,
                MemoryCategory::Decision => AutoCategory::Decision,
            };
            self.inner
                .add_entry(topic, content, cat, confidence)
                .await
                .map_err(|e: MemoryError| ToolError::Exec(format!("auto memory: {e}")))
        })
    }
}

/// 从 CLI 参数构建 `Runtime`。
///
/// `mode` 控制会话加载方式（`--resume`/`--replay`/`--fork-session`，T-M3-10）。
/// 预加载会话时，调用方需在 `build_runtime` 后调用 `Runtime::restore_history`
/// 将消息注入上下文管理器。
///
/// # Errors
/// API key 缺失、provider 构造失败、存储目录不可用、会话不存在或加载失败时返回错误。
#[allow(clippy::needless_pass_by_value)]
pub fn build_runtime(
    api_base: Option<&str>,
    api_key: Option<&str>,
    model: Option<&str>,
    workdir: &str,
    system: Option<&str>,
    mode: &SessionLoadMode,
) -> Result<Runtime> {
    // 1. 加载配置
    let mut config = RuntimeConfig::default();
    // 环境变量快捷覆盖
    if let Ok(base) = std::env::var("OPENAI_API_BASE") {
        config.provider.api_base = base;
    }
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        config.provider.api_key = key;
    }
    if let Ok(m) = std::env::var("OPENAI_MODEL") {
        config.provider.model = m;
    }
    // CLI 参数覆盖
    if let Some(base) = api_base {
        config.provider.api_base = base.to_string();
    }
    if let Some(key) = api_key {
        config.provider.api_key = key.to_string();
    }
    if let Some(m) = model {
        config.provider.model = m.to_string();
    }

    // 2. 校验 API key
    if config.provider.api_key.is_empty() {
        anyhow::bail!("API key 未配置：请设置 OPENAI_API_KEY 环境变量或使用 --api-key 参数");
    }

    // 3. 构造 provider
    let provider = OpenAiProvider::new(
        &config.provider.api_base,
        &config.provider.api_key,
        &config.provider.model,
    )
    .context("OpenAI provider 构造失败")?;

    // 4. 构造 context manager
    let ctx = match system {
        Some(s) => SimpleContextManager::new(s.to_string()),
        None => SimpleContextManager::with_default_system(),
    };

    // 5. 构造 storage
    let sessions_dir = minicoding_core::paths::sessions_dir().context("无法确定会话存储目录")?;
    let storage = JsonlStorage::new(sessions_dir);

    // 6. 构造 tool registry
    let mut tools = ToolRegistry::new();
    register_readonly_tools(&mut tools);
    register_write_tools(&mut tools);
    register_shell_tools(&mut tools);

    // 6b. 注册 memory.write 工具（T-M3-9：long_term + auto memory）
    //     long_term 走 MemoryStore trait（C-23：经 Ask 权限）；
    //     auto 走 AutoMemoryWriter trait（C-27：默认 Allow，指令性内容降级 Ask）。
    //     LongTermMemory::default / AutoMemory::default 在 home 不可解析时退化为相对路径。
    let long_term_store: Arc<dyn MemoryStore> = Arc::new(LongTermMemory::default());
    let auto_store: Arc<dyn AutoMemoryWriter> = Arc::new(AutoMemoryAdapter {
        inner: Arc::new(AutoMemory::default()),
    });
    tools.register(Arc::new(MemoryWrite::new(long_term_store, auto_store)));

    // 7. 构造权限策略 + 交互器（C-01：副作用必须经权限）
    //    TTY → InteractivePrompter（stdin 读 y/n）；非 TTY → NonInteractivePrompter
    //    （恒 Deny，CI 安全默认，见 design.md §9.2）
    //    `--replay` 无 `--allow-side-effects` → ReplayPolicy 包装 BuiltinPolicy，
    //    强制 Deny 所有副作用工具（C-06）。
    let builtin: Arc<dyn PermissionPolicy> = Arc::new(BuiltinPolicy::new());
    let policy: Arc<dyn PermissionPolicy> = match mode {
        SessionLoadMode::Replay {
            allow_side_effects: false,
            ..
        } => {
            tracing::warn!(
                "--replay 模式：副作用工具已禁用（C-06），使用 --allow-side-effects 显式启用"
            );
            Arc::new(ReplayPolicy::new(builtin))
        }
        _ => builtin,
    };
    let prompter: Arc<dyn PermissionPrompter> = if std::io::stdin().is_terminal() {
        Arc::new(InteractivePrompter::new())
    } else {
        tracing::warn!("stdin 非 TTY，切换为 NonInteractivePrompter（副作用工具将被拒绝）");
        Arc::new(NonInteractivePrompter::new())
    };

    // 8. 构造审计 sink（AGENTS.md §5.5：权限决策必须落 audit.log，0600 权限）
    let audit_path = minicoding_core::paths::audit_log_path().context("无法确定审计日志路径")?;
    let audit: Arc<dyn AuditSink> = Arc::new(FileAuditSink::new(audit_path));

    // 9. 解析 workdir
    let workdir_path = Utf8PathBuf::from(workdir)
        .canonicalize_utf8()
        .unwrap_or_else(|_| Utf8PathBuf::from(workdir));

    // 10. 按 mode 加载会话（T-M3-10a/b：resume/replay/fork）
    //     - Resume/Replay：原 id，原存储文件追加写；
    //     - Fork：新 id，复制前缀消息到新文件（原文件不变）。
    //     消息不在此处注入上下文，由调用方 `restore_history` 完成回填。
    let config_hash_val = config_hash(&config);
    let session = load_session_by_mode(&storage, mode, workdir_path.clone(), config_hash_val)?;

    // 11. 组装 Runtime
    let mut builder = RuntimeBuilder::new()
        .provider(Arc::new(provider))
        .context(Arc::new(ctx))
        .storage(Arc::new(storage))
        .tools(tools)
        .config(config)
        .workdir(workdir_path)
        .policy(policy)
        .prompter(prompter)
        .audit(audit);
    if let Some(s) = session {
        builder = builder.session(s);
    }
    let rt = builder.build().map_err(anyhow::Error::msg)?;

    Ok(rt)
}

/// 按 `SessionLoadMode` 加载会话（T-M3-10a/b）。
///
/// - `None` → `None`（新建空会话）；
/// - `Resume(id)` / `Replay{id}` → 原会话（原 id，不复制）；
/// - `Fork(id)` → 新会话（新 id，复制原消息前缀到新文件，原文件不变）。
///
/// # Errors
/// 会话不存在、加载失败或 fork 复制失败时返回错误。
fn load_session_by_mode(
    storage: &JsonlStorage,
    mode: &SessionLoadMode,
    workdir: Utf8PathBuf,
    config_hash_val: u64,
) -> Result<Option<Session>> {
    let source_id = match mode {
        SessionLoadMode::None => return Ok(None),
        SessionLoadMode::Resume(id)
        | SessionLoadMode::Replay { id, .. }
        | SessionLoadMode::Fork(id) => id,
    };
    let messages = load_messages(storage, source_id)?;
    let created_at = messages
        .first()
        .map_or_else(OffsetDateTime::now_utc, |m| m.created_at);

    match mode {
        SessionLoadMode::None => Ok(None),
        SessionLoadMode::Resume(_) | SessionLoadMode::Replay { .. } => {
            // 原会话：保留原 id，后续 run_turn 追加写原文件
            Ok(Some(Session {
                id: source_id.clone(),
                created_at,
                workdir,
                config_hash: config_hash_val,
                messages,
            }))
        }
        SessionLoadMode::Fork(_) => {
            // Fork：新 id，复制前缀消息到新文件（原文件不变，design.md §10.5）
            let new_id = ulid::Ulid::new().to_string();
            storage
                .fork_session_sync(&new_id, &messages)
                .context("fork 会话复制失败")?;
            tracing::info!(
                from = source_id,
                to = %new_id,
                count = messages.len(),
                "session forked"
            );
            Ok(Some(Session {
                id: new_id,
                created_at,
                workdir,
                config_hash: config_hash_val,
                messages,
            }))
        }
    }
}

/// 从存储同步加载会话消息（启动期 tokio runtime 未创建，用 sync 方法）。
fn load_messages(storage: &JsonlStorage, session_id: &str) -> Result<Vec<Message>> {
    let id: SessionId = session_id.to_string();
    let messages = storage
        .load_messages_sync(&id)
        .with_context(|| format!("读取会话 {session_id} 消息失败"))?;
    if messages.is_empty() {
        anyhow::bail!("会话 {session_id} 不存在或无消息");
    }
    Ok(messages)
}
