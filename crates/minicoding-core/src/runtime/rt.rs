//! `Runtime` 聚合根 + 单轮 Agent 循环。
//!
//! M2 接入：`PermissionPolicy`/`PermissionPrompter`/`AuditSink`——副作用工具调用
//! 必须经权限检查（C-01），决策落 `audit.log`（AGENTS.md §5.5）。
//! M4 接入：`SandboxDriver`/`SandboxPolicy`/`Journal`——OS 沙箱第二道防线（C-22）。
//! M5 接入：`HookRegistry`——PreToolUse/PostToolUse/PermissionRequest Hook 触发点
//! （见 `hooks.md` §4）。Hook 在 `policy.check` 之后、工具执行前后运行，
//! 可阻断/改写/注入上下文，但不可覆盖内置黑名单 Deny（C-21）。
//! M5 接入：`PermissionMode`（Plan/AcceptEdits/Default/...）+ `PlanModeController`——
//! Plan 模式硬门（C-25）+ `plan.exit` 预批准缓存（见 `design.md` §16）。
//! 工具执行：无副作用并行、有副作用串行（串行段每个工具先过权限 + Hook）。
//!
//! 详见 `design.md` §2、§9、§16、§20。

use crate::config::{ConfigWatcher, RuntimeConfig};
use crate::context::ContextManager;
use crate::hooks::{
    DispatchConfig, HookDecision, HookEvent, HookInput, HookRegistry, VerdictSerde,
};
use crate::journal::Journal;
use crate::memory::SessionSummarizer;
use crate::metrics;
use crate::model::{
    Message, PolicyError, RuntimeError, Session, SessionId, SideEffect, StopReason, ToolCall,
    ToolCallId, ToolResult, TurnOutcome, UserInput,
};
use crate::otel::span_name;
use crate::policy::{
    Decision, PermissionContext, PermissionMode, PermissionPolicy, PermissionPrompt,
    PermissionPrompter, PlanModeController, PlanModeSnapshot, PreApprovedPrompt, Verdict,
};
use crate::provider::{BoxFuture, ChatRequest, Delta, LlmProvider};
use crate::runtime::accumulator::DeltaAccumulator;
use crate::runtime::{Event, EventBus};
use crate::sandbox::{
    BreakerState, DenialDetector, SandboxCircuitBreaker, SandboxDriver, SandboxPolicy,
};
use crate::storage::{
    AuditKind, AuditRecord, AuditSink, EventRecord, EventStore, PersistedEvent, SNAPSHOT_INTERVAL,
    SessionSnapshot, SessionState, SnapshotStore, Storage, try_persist,
};
use crate::tool::{ToolContext, ToolRegistry};
use camino::Utf8PathBuf;
use futures::StreamExt;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use time::OffsetDateTime;
use tokio::sync::{Mutex as TokioMutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

/// 只读工具并发执行的 future 类型（装箱擦除生命周期，避免 HRTB 问题）。
type ToolFuture =
    std::pin::Pin<Box<dyn Future<Output = Result<(ToolCallId, ToolResult), RuntimeError>> + Send>>;

/// Runtime 聚合根（所有可替换能力的持有者）。
///
/// 由 `RuntimeBuilder` 构造，frontend 长期持有。
/// `run_turn` 驱动单轮 Agent 循环（用户输入 → LLM → 工具 → ... → 最终回复）。
pub struct Runtime {
    pub(crate) provider: Arc<dyn LlmProvider>,
    pub(crate) ctx: Arc<dyn ContextManager>,
    pub(crate) storage: Arc<dyn Storage>,
    pub(crate) tools: ToolRegistry,
    pub(crate) config: RuntimeConfig,
    pub(crate) session: Session,
    pub(crate) events: EventBus,
    /// 当前工作目录（W-11 工作区切换：`switch_workdir` 在 `&self` 下更新，需内部可变）。
    ///
    /// 工具每次执行时经 `ToolContext::new(self.workdir.read().await.clone(), ..)`
    /// 读取（见 `dispatch_calls`），切换后下一次工具调用自动生效（C-03 路径沙箱
    /// 同步跟随新 root）。`Session.workdir` 是创建时快照（event sourcing 重建用），
    /// 切换不反写。
    pub(crate) workdir: RwLock<Utf8PathBuf>,
    pub(crate) policy: Arc<dyn PermissionPolicy>,
    pub(crate) prompter: Arc<dyn PermissionPrompter>,
    pub(crate) audit: Arc<dyn AuditSink>,
    /// Ctrl-C 取消 token（graceful stop，C-13：已落盘消息不丢失）。
    pub(crate) cancel_token: CancellationToken,
    /// 会话摘要生成器（可选，T-M3-6）。
    ///
    /// `None` 时 `summarize_session` 为 no-op；`Some` 时由 CLI 在会话退出前
    /// 显式调用 `summarize_session`，将摘要落盘到 `index.json` 供跨会话恢复。
    pub(crate) session_summarizer: Option<Arc<dyn SessionSummarizer>>,
    /// OS 沙箱驱动（M4，`shell.run` 在 spawn 子进程前 `apply`，C-22）。
    pub(crate) sandbox_driver: Arc<dyn SandboxDriver>,
    /// OS 沙箱策略（M4，与 `sandbox_driver` 配套）。
    pub(crate) sandbox_policy: SandboxPolicy,
    /// 文件改动 journal（M4，可选，`fs.write/edit/delete` 成功后 `record`，C-28）。
    pub(crate) journal: Option<Arc<dyn Journal>>,
    /// 沙箱拒绝检测器（无状态，T-M4-5）。
    pub(crate) denial_detector: DenialDetector,
    /// 沙箱拒绝熔断器（单 turn 内有效，C-30 不可被 LLM 绕过）。
    pub(crate) sandbox_breaker: SandboxCircuitBreaker,
    /// Hook 注册表（M5，默认 `NoopHookRegistry` 兜底）。
    ///
    /// PreToolUse/PostToolUse/PermissionRequest Hook 在 `execute_side_effect_call`
    /// 中触发（见 `hooks.md` §4）。未注入时所有 Hook 事件为 no-op。
    pub(crate) hook_registry: Arc<dyn HookRegistry>,
    /// Plan 模式状态（M5，`PermissionMode` + `allowed_prompts` 缓存）。
    ///
    /// `Runtime` 实现 `PlanModeController`，`plan.exit` 工具通过持有的
    /// `Arc<dyn PlanModeController>` 反向调用 `exit_plan` 切换模式 + 缓存预批准。
    /// `execute_side_effect_call` 在构造 `PermissionContext` 时读快照注入。
    /// `tokio::sync::RwLock` 因 `exit_plan/set_mode` 是跨 await 的写操作。
    pub(crate) plan_state: Arc<RwLock<PlanModeSnapshot>>,
    /// 子 Agent runner（M5，默认 `NoopSubagentRunner` 兜底）。
    ///
    /// `task.spawn` 工具持有 `Arc<dyn SubagentRunner>` 反向调用 Runtime 派发子 Agent
    /// （与 `plan.exit` 持有 `Arc<dyn PlanModeController>` 同构）。未注入时
    /// `task.spawn` 调用直接返回 `RuntimeError::Config`（不静默 no-op）。
    pub(crate) subagent_runner: Arc<dyn crate::agent::SubagentRunner>,
    /// 扩展宿主（M8，默认 `NoopExtensionHost` 兜底）。
    ///
    /// Runtime 持有 `Arc<dyn ExtensionHost>` 用于运行期 `unload_extension`/
    /// `on_config_changed`。`shutdown_all` 是 `BundledExtensionHost` 的 inherent 方法，
    /// CLI 在会话退出前通过 `extension_host()` 拿到 `Arc<dyn ExtensionHost>` 后
    /// （或持有原始 `Arc<BundledExtensionHost>`）调用 `shutdown_all` 释放资源。
    pub(crate) extension_host: Arc<dyn crate::extension::ExtensionHost>,
    /// 配置文件监听器（S-22，可选）。
    ///
    /// `Some` 时随 `Runtime` 存活，drop 时自动停止监听并结束后台 task；`None` 表示
    /// 未启用热更新（`ConfigWatcher::start` 监听失败降级或 CLI 未注入）。
    // 仅持有以控制生命周期（Drop 时停止后台 task），不需要读取字段值
    #[allow(dead_code)]
    pub(crate) config_watcher: Option<ConfigWatcher>,
    /// 事件存储（Event Sourcing，可选，见 `design.md` §25）。
    ///
    /// `Some` 时 Runtime 在 `emit(Event)` 同时持久化 `PersistedEvent` 到事件流。
    /// 未注入时（`NoopEventStore`）退化为 no-op，兼容旧会话。
    pub(crate) event_store: Arc<dyn EventStore>,
    /// Snapshot 存储（Event Sourcing，可选，见 `design.md` §25.3）。
    ///
    /// `Some` 时 Runtime 在每 `SNAPSHOT_INTERVAL` 条 `MessageAppended` 事件后
    /// 落盘 snapshot。未注入时（`NoopSnapshotStore`）退化为 no-op。
    pub(crate) snapshot_store: Arc<dyn SnapshotStore>,
    /// 当前会话的事件 seq 计数器（单调递增，从 1 开始）。
    ///
    /// 由 `EventStore::next_seq` 初始化（Runtime 构造时调用一次），此后由
    /// `allocate_seq` 原子递增。`TokioMutex` 因 `allocate_seq` 在 async 上下文中调用。
    pub(crate) event_seq: Arc<TokioMutex<u64>>,
    /// 自上次 snapshot 后的 `MessageAppended` 事件计数（用于触发周期 snapshot）。
    pub(crate) message_since_snapshot: AtomicU64,
    /// 已持久化的最大 seq（`EventStore::append` 成功后更新，供 SSE cursor 协同）。
    ///
    /// `Arc<TokioMutex>` 因 `--replay`/SSE handler 需读取此值判断 `durable_seq`。
    pub(crate) durable_seq: Arc<TokioMutex<u64>>,
}

impl Runtime {
    /// 返回当前会话。
    #[must_use]
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// 返回审计 sink（W-11 只读浏览审计，`workspace.rs` 用）。
    #[must_use]
    pub fn audit(&self) -> Arc<dyn AuditSink> {
        self.audit.clone()
    }

    /// 返回事件总线引用（订阅事件流）。
    #[must_use]
    pub fn events(&self) -> &EventBus {
        &self.events
    }

    /// 返回上下文管理器引用（供 frontend/test 查询 `message_count` 等）。
    #[must_use]
    pub fn context(&self) -> &Arc<dyn ContextManager> {
        &self.ctx
    }

    /// 返回存储引用（供 frontend/test 查询会话消息）。
    #[must_use]
    pub fn storage(&self) -> &Arc<dyn Storage> {
        &self.storage
    }

    /// 返回当前工作目录（`RwLock` 读取，切换工作区后返回新值）。
    #[must_use]
    pub async fn workdir(&self) -> Utf8PathBuf {
        self.workdir.read().await.clone()
    }

    /// 切换工作目录（W-11 工作区切换，需用户显式批准，Ask 级权限）。
    ///
    /// 流程（与副作用工具权限路径一致，见 `design.md` §9）：
    /// 1. 校验 `target` 为绝对路径 + 目标**真实存在且是目录**（`canonicalize`）——
    ///    校验在弹权限窗前完成，目录不存在/不可访问时立即报错（避免用户等待
    ///    审批后才在后续浏览中到处 404）；
    /// 2. 构造 `PermissionPrompt`（`tool: "workspace.switch"`，`Risk::Medium`，
    ///    仅 `AllowOnce`/`DenyOnce`——不允许 `AllowAlways`，切换必须逐次确认）；
    /// 3. 广播 `Event::PermissionRequested`（SSE 推送到前端弹窗，复用 W-03 权限
    ///    弹窗机制）→ `prompter.prompt` 等待决策；
    /// 4. 广播 + 持久化 `Event::PermissionResolved`，落 `audit.log`（C-01 语义：
    ///    工作区切换改变后续所有副作用工具的作用范围，等同副作用决策）；
    /// 5. `Allow` → 更新 `workdir` 为 canonicalize 后的规范化路径（后续工具调用
    ///    自动生效，C-03 跟随新 root）；`Deny` → 保持原目录。
    ///
    /// 调用方（HTTP `POST /sessions/{id}/workspace`）应在持有 turn 锁时调用，
    /// 避免与进行中的 turn 交错（本方法在 Runtime 内不自行加锁）。
    ///
    /// # Errors
    /// `target` 非绝对路径、目录不存在或不可访问时返回 `RuntimeError::Permission`；
    /// 存储持久化失败时返回 `RuntimeError::Storage`。
    ///
    /// # Returns
    /// `true` = 切换成功；`false` = 用户拒绝。
    pub async fn switch_workdir(&self, target: &Utf8PathBuf) -> Result<bool, RuntimeError> {
        if !target.is_absolute() {
            return Err(RuntimeError::Permission(
                "workspace.switch: 目标路径必须是绝对路径".to_string(),
            ));
        }
        // 目标必须真实存在且是目录：canonicalize 失败（不存在/权限不足）直接报错，
        // 不进入权限弹窗（避免"切换成功"后所有浏览 404 的假成功态）。
        let canonical = tokio::fs::canonicalize(target).await.map_err(|e| {
            RuntimeError::Permission(format!(
                "workspace.switch: 目标目录不存在或不可访问 `{target}`: {e}"
            ))
        })?;
        let meta = tokio::fs::metadata(&canonical).await.map_err(|e| {
            RuntimeError::Permission(format!(
                "workspace.switch: 无法读取目标目录 `{target}`: {e}"
            ))
        })?;
        if !meta.is_dir() {
            return Err(RuntimeError::Permission(format!(
                "workspace.switch: 目标不是目录 `{target}`"
            )));
        }
        // canonicalize 保证统一规范化（Windows 盘符/UNC/尾斜杠）；camino 类型保证 UTF-8
        let canonical = Utf8PathBuf::from_path_buf(canonical).map_err(|_| {
            RuntimeError::Permission(format!("workspace.switch: 目标路径非 UTF-8 `{target}`"))
        })?;
        // 权限弹窗展示 canonical 后的路径（用户看到的是真实目标，而非带 `..` 的原始输入）
        let target_display = canonical.as_str();

        let prompt = PermissionPrompt {
            id: format!("ws-{}", ulid::Ulid::new()),
            tool: "workspace.switch".to_string(),
            summary: format!("切换工作区到 {target_display}"),
            risk: crate::policy::Risk::Medium,
            options: vec![crate::policy::PromptOption::AllowOnce],
        };

        self.events.emit(Event::PermissionRequested {
            id: prompt.id.clone(),
            tool: prompt.tool.clone(),
            summary: prompt.summary.clone(),
            risk: prompt.risk,
        });
        let decision = self.prompter.prompt(prompt.clone()).await;
        let prompt_id = prompt.id.clone();
        let event = Event::PermissionResolved {
            id: prompt_id.clone(),
            decision: decision.clone(),
        };
        self.persist_event(&event).await;
        self.events.emit(event);
        self.record_permission_audit("workspace.switch", &decision, Some(prompt_id))
            .await;

        match decision {
            Decision::Allow => {
                *self.workdir.write().await = canonical.clone();
                tracing::info!(
                    session = %self.session.id,
                    workdir = %canonical,
                    "workspace switched"
                );
                Ok(true)
            }
            Decision::Deny(reason) => {
                tracing::info!(
                    session = %self.session.id,
                    reason = %reason,
                    "workspace switch denied"
                );
                Ok(false)
            }
        }
    }

    /// 返回取消 token 的克隆（供 frontend 在 `select!` 中组合等待，如 Ctrl-C handler）。
    #[must_use]
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    /// 触发取消（CLI 的 Ctrl-C handler 调用）。
    ///
    /// 取消是 graceful 的：当前 in-flight 的迭代被丢弃，已落盘的消息保留
    /// （C-13：Ctrl-C 不丢已生成消息），`run_turn` 返回 `TurnOutcome::Interrupted`。
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    /// 返回 `PlanModeController` 引用（`plan.exit` 工具注入用，M5 T-M5-6）。
    ///
    /// `plan.exit` 是 `SideEffect::None` 工具（只读桶并行调度），不经
    /// `execute_side_effect_call`，故需通过持有的 controller 反向调用 Runtime
    /// 切换 `PermissionMode` + 缓存 `allowed_prompts`。
    #[must_use]
    pub fn plan_controller(&self) -> Arc<dyn PlanModeController> {
        // PlanModeController 由 Runtime 实现；这里返回一个共享 plan_state 的适配器。
        // Event Sourcing：注入 event_store/event_seq/durable_seq/session_id，
        // 让 `PermissionModeChanged` 事件持久化（replay 时重建 final_permission_mode）。
        Arc::new(PlanControllerHandle {
            state: self.plan_state.clone(),
            events: self.events.clone(),
            session_id: self.session.id.clone(),
            event_store: self.event_store.clone(),
            event_seq: self.event_seq.clone(),
            durable_seq: self.durable_seq.clone(),
        })
    }

    /// 返回 `SubagentRunner` 引用（`task.spawn` 工具注入用，M5 T-M5-7）。
    ///
    /// `task.spawn` 是 `SideEffect::None` 工具（父 Agent 只接收 `summary`，
    /// 子 Agent 的副作用在其自身的权限检查中处理，C-05），不经
    /// `execute_side_effect_call`，故需通过持有的 runner 反向调用 Runtime 派发。
    #[must_use]
    pub fn subagent_runner(&self) -> Arc<dyn crate::agent::SubagentRunner> {
        self.subagent_runner.clone()
    }

    /// 返回 `ExtensionHost` 引用（CLI 调用 `unload_extension`/`on_config_changed`/
    /// `shutdown_all` 用，M8）。
    ///
    /// `shutdown_all` 是 `BundledExtensionHost` 的 inherent 方法（不在 trait 中），
    /// CLI 需通过持有的原始 `Arc<BundledExtensionHost>` 调用。此 getter 返回 trait
    /// 对象引用，供 CLI 在运行期调用 trait 方法（如 `list_extensions`）。
    #[must_use]
    pub fn extension_host(&self) -> Arc<dyn crate::extension::ExtensionHost> {
        self.extension_host.clone()
    }

    /// 返回 `Journal` 引用（`/undo` REPL 命令用，M5 T-M5-8）。
    ///
    /// `file-undo` feature 未启用时返回 `None`，CLI 据此决定 `/undo` 是否可用。
    #[must_use]
    pub fn journal(&self) -> Option<Arc<dyn Journal>> {
        self.journal.clone()
    }

    /// 注册依赖 Runtime 自身引用的工具（`plan.exit`/`task.spawn`，M5 T-M5-8）。
    ///
    /// 这些工具需要 `plan_controller()`/`subagent_runner()`，只能在 Runtime 构造后
    /// 注册（chicken-and-egg：tools 需要 Runtime 引用，Runtime 需要 tools）。
    /// CLI 在 `build_runtime` 后调用此方法补注册。
    ///
    /// # Panics
    /// 不会 panic；重复注册同名工具会覆盖（与 `ToolRegistry::register` 语义一致）。
    pub fn register_dynamic_tool(&mut self, tool: Arc<dyn crate::tool::Tool>) {
        self.tools.register(tool);
    }

    /// 恢复会话历史到上下文管理器（`--resume`/`--fork-session` 用，T-M3-10a）。
    ///
    /// 将 `self.session.messages` 逐条注入 `ContextManager`，使后续 `run_turn` 能
    /// 基于历史上下文继续对话。仅在 `RuntimeBuilder::session` 设置预加载会话后调用
    /// 一次；对新建会话（空消息）调用是 no-op。
    ///
    /// 消息已在磁盘（首次 `storage.append` 时落盘），此处只回填内存上下文,
    /// **不重复落盘**——后续 `run_turn` 的新消息才走 `storage.append`。
    ///
    /// # Errors
    /// 当前 `ContextManager::append` 不返回错误；保留 `Result` 为未来扩展（如
    /// 压缩管道在回填时触发熔断）预留。
    pub async fn restore_history(&self) -> Result<(), RuntimeError> {
        let count = self.session.messages.len();
        for msg in &self.session.messages {
            self.ctx.append(msg.clone()).await;
        }
        if count > 0 {
            tracing::info!(session = %self.session.id, restored = count, "history restored");
        }
        Ok(())
    }

    /// 初始化事件流（Event Sourcing，见 `design.md` §25.1）。
    ///
    /// **调用时机**：`RuntimeBuilder::build()` 后、首次 `run_turn` 前。CLI/server
    /// 在构造 Runtime 后调用此方法，按 `EventStore` 实际状态修正 seq 计数器。
    ///
    /// ## 行为
    ///
    /// - **新会话**（`EventStore::next_seq == 1` 且无 snapshot）：持久化
    ///   `PersistedEvent::SessionCreated`（携带 `workdir`/`config_hash`/`created_at`，
    ///   供 `replay_session_state` 重建 `Session`），设置 `event_seq = 2`、
    ///   `durable_seq = 1`；
    /// - **恢复会话**（`--resume`/`--replay`，`next_seq > 1`）：设置
    ///   `event_seq = next_seq`；若存在 snapshot，设置 `durable_seq = snapshot.seq`
    ///   并重置 `message_since_snapshot = 0`；
    /// - **`NoopEventStore`**：`next_seq` 恒返回 1，但 append 为 no-op，整个方法
    ///   实际效果为设置 `event_seq = 2`（无副作用）。
    ///
    /// ## 旧会话兼容
    ///
    /// 旧会话（仅有 `{id}.jsonl` 消息日志，无事件流）：`EventStore::next_seq` 返回 1
    /// （事件文件不存在），`SnapshotStore::load` 返回 `None`，方法走"新会话"路径，
    /// 持久化 `SessionCreated` 事件。后续 `run_turn` 的新消息会双写（消息日志 +
    /// 事件流）。`--replay` 时若需历史消息，调用方应回退到 `Storage::load` 路径。
    ///
    /// # Errors
    /// `EventStore::next_seq`/`SnapshotStore::load`/`EventStore::append` 失败时
    /// 返回 `RuntimeError::Storage`。
    pub async fn init_event_stream(&self) -> Result<(), RuntimeError> {
        let next_seq = self
            .event_store
            .next_seq(&self.session.id)
            .await
            .map_err(RuntimeError::Storage)?;

        // 加载最近 snapshot（设置 durable_seq + 重置 message_since_snapshot）
        let snapshot = self
            .snapshot_store
            .load(&self.session.id)
            .await
            .map_err(RuntimeError::Storage)?;
        if let Some(snap) = &snapshot {
            *self.durable_seq.lock().await = snap.seq;
            self.message_since_snapshot.store(0, Ordering::SeqCst);
        }

        if next_seq == 1 && snapshot.is_none() {
            // 新会话：持久化 SessionCreated 事件（携带完整字段，供 replay 重建）
            let seq = 1;
            let persisted = PersistedEvent::SessionCreated {
                id: self.session.id.clone(),
                workdir: self.session.workdir.to_string(),
                config_hash: self.session.config_hash,
                created_at: self.session.created_at,
            };
            let record = EventRecord::new(seq, self.session.id.clone(), persisted);
            self.event_store
                .append(&self.session.id, record)
                .await
                .map_err(RuntimeError::Storage)?;
            *self.event_seq.lock().await = seq + 1;
            *self.durable_seq.lock().await = seq;
            tracing::info!(
                session = %self.session.id,
                seq,
                "SessionCreated event persisted (event sourcing init)"
            );
        } else {
            *self.event_seq.lock().await = next_seq;
            tracing::info!(
                session = %self.session.id,
                next_seq,
                snapshot_seq = snapshot.as_ref().map_or(0, |s| s.seq),
                "event stream initialized (resumed session)"
            );
        }
        Ok(())
    }

    /// 返回当前已持久化的最大 seq（SSE cursor 协同用，见 `protocol::cursor`）。
    ///
    /// SSE handler 在判断 `Last-Event-ID` 是否可从 `EventStore` 重放时读取此值：
    /// `last_seq <= durable_seq` 时可从 `EventStore` 重放（durable recovery）。
    #[must_use]
    pub async fn durable_seq(&self) -> u64 {
        *self.durable_seq.lock().await
    }

    /// 返回 `EventStore` 引用（SSE cursor durable recovery 用，见 `design.md` §25.5）。
    ///
    /// SSE handler 在 `Last-Event-ID` 已从内存 ring buffer evict 但 ≤ `durable_seq`
    /// 时，通过此引用调 `EventStore::load_after` 重放持久化事件，避免发
    /// `RehydrateRequired`（与 E-13/E-14 协同）。
    ///
    /// 返回 `Arc<dyn EventStore>`：未注入 event sourcing 时为 `NoopEventStore`，
    /// `load_after` 返回空 Vec（调用方据此回退到 `RehydrateRequired`）。
    #[must_use]
    pub fn event_store(&self) -> Arc<dyn EventStore> {
        self.event_store.clone()
    }

    /// 持久化事件到 `EventStore` + 触发周期 snapshot（Event Sourcing 核心）。
    ///
    /// 由 `run_turn` 在 `events.emit(event)` 后调用。流程：
    /// 1. `try_persist(&event)` 过滤瞬态事件（返回 `None` 直接返回）；
    /// 2. 分配 seq（`event_seq` mutex 递增）；
    /// 3. 构造 `EventRecord`，调 `EventStore::append`；
    /// 4. 更新 `durable_seq`；
    /// 5. `MessageAppended` 时递增 `message_since_snapshot`，达到
    ///    `SNAPSHOT_INTERVAL` 触发 snapshot 落盘。
    ///
    /// **best-effort 语义**：持久化失败仅记 `warn` 日志，不中断主流程
    /// （与 audit 失败处理一致）。崩溃时磁盘状态为已持久化事件的子集，
    /// replay 可从最近 snapshot + 剩余事件重建。
    async fn persist_event(&self, event: &Event) {
        let Some(persisted) = try_persist(event) else {
            return; // 瞬态事件，跳过
        };

        // 分配 seq（单调递增）
        let seq = {
            let mut guard = self.event_seq.lock().await;
            let s = *guard;
            *guard += 1;
            s
        };

        let record = EventRecord::new(seq, self.session.id.clone(), persisted);

        // 持久化（fsync 后返回，崩溃安全）
        if let Err(e) = self.event_store.append(&self.session.id, record).await {
            tracing::warn!(
                error = %e,
                session = %self.session.id,
                seq,
                "event persist failed (best-effort, continue)"
            );
            return;
        }

        // 更新 durable_seq（供 SSE cursor 协同）
        {
            let mut guard = self.durable_seq.lock().await;
            if seq > *guard {
                *guard = seq;
            }
        }

        // MessageAppended 时触发周期 snapshot
        if matches!(event, Event::MessageAppended(_)) {
            let count = self.message_since_snapshot.fetch_add(1, Ordering::SeqCst) + 1;
            if count >= SNAPSHOT_INTERVAL as u64 {
                self.create_snapshot(seq).await;
                self.message_since_snapshot.store(0, Ordering::SeqCst);
            }
        }
    }

    /// 创建并落盘当前会话状态的 snapshot（见 `design.md` §25.3）。
    ///
    /// 从 `ContextManager::snapshot` 获取当前消息列表，构造 `SessionState` +
    /// `SessionSnapshot`，调 `SnapshotStore::save` 原子落盘（先 `.tmp` 再 `rename`）。
    /// snapshot 失败仅记 `warn` 日志，不中断主流程（best-effort）。
    async fn create_snapshot(&self, seq: u64) {
        let ctx_snap = self.ctx.snapshot().await;
        let state = SessionState {
            id: self.session.id.clone(),
            created_at: self.session.created_at,
            workdir: self.session.workdir.to_string(),
            config_hash: self.session.config_hash,
            messages: ctx_snap.messages,
        };
        let snapshot = SessionSnapshot::new(seq, state);
        if let Err(e) = self.snapshot_store.save(snapshot).await {
            tracing::warn!(
                error = %e,
                session = %self.session.id,
                seq,
                "snapshot save failed (best-effort, continue)"
            );
            return;
        }
        tracing::info!(
            session = %self.session.id,
            seq,
            "snapshot persisted (event sourcing checkpoint)"
        );
    }

    /// 生成会话摘要并落盘 `index.json`（T-M3-6）。
    ///
    /// 在会话退出前调用：从 `ContextManager` 快照消息 → 调注入的
    /// `SessionSummarizer` 生成摘要（降级链：主 provider → 备用 → 启发式兜底，
    /// C-29 永不失败）→ `Storage::update_summary` 落盘。
    ///
    /// `session_summarizer` 未注入或会话无消息时为 no-op。摘要失败仅记 `warn`
    /// 日志，不阻塞会话退出（best effort，与会话生命周期解耦）。
    ///
    /// # Errors
    /// 仅当 `Storage::update_summary` 失败时返回 `RuntimeError::Storage`；
    /// 摘要生成本身永不失败（启发式兜底，C-29）。
    pub async fn summarize_session(&self) -> Result<(), RuntimeError> {
        let Some(summarizer) = &self.session_summarizer else {
            return Ok(());
        };
        let snap = self.ctx.snapshot().await;
        if snap.messages.is_empty() {
            return Ok(());
        }
        let summary = match summarizer.summarize(&snap.messages).await {
            Ok(s) => s,
            Err(e) => {
                // 理论不可达：启发式兜底恒成功（C-29）。但保留兜底以防实现 bug。
                tracing::warn!(
                    error = %e,
                    session = %self.session.id,
                    "会话摘要生成失败（理论不可达，C-29 兜底应保证成功）"
                );
                return Ok(());
            }
        };
        if let Err(e) = self
            .storage
            .update_summary(&self.session.id, &summary)
            .await
        {
            tracing::warn!(
                error = %e,
                session = %self.session.id,
                "会话摘要落盘失败（best effort，不阻塞退出）"
            );
            return Err(RuntimeError::Storage(e));
        }
        tracing::info!(
            session = %self.session.id,
            summary_chars = summary.chars().count(),
            "会话摘要已落盘"
        );
        Ok(())
    }

    /// 驱动单轮对话（用户输入 → 最终回复或失败）。
    ///
    /// 循环不变量（见 `design.md` §2.1）：
    /// 1. 消息先写盘（`storage.append`）再入上下文（`ctx.append`）再广播
    /// 2. 无工具调用时立即 `TurnEnd` 退出
    /// 3. 有工具调用则执行后回到步骤 2
    ///
    /// 三道终止防御（C-13 单轮调用上限的补充）：
    /// - `max_tool_iters`：迭代轮次硬上限（默认 50）
    /// - 重复检测：连续 ≥3 轮相同工具调用集合 → 判定死循环提前终止
    /// - `turn_timeout`：整个 turn 超时（默认 600s）→ `Stopped`
    /// - Ctrl-C cancel：`cancel()` 触发 → `Interrupted`（已落盘消息不丢失）
    /// - 沙箱拒绝熔断（C-30）：单 turn 内 ≥3 次拒绝注入提醒，≥5 次强制 `TurnEnd`
    ///
    /// # Errors
    /// LLM 调用失败、工具执行失败、存储失败等返回 `RuntimeError`。
    #[allow(clippy::too_many_lines)] // Event Sourcing persist 调用扩展了函数体，拆分反而降低可读性
    pub fn run_turn(
        &self,
        user_input: UserInput,
    ) -> impl Future<Output = Result<TurnOutcome, RuntimeError>> + '_ {
        let span = tracing::info_span!("turn", session = %self.session.id);
        // 使用 `.instrument(span)` 而非 `span.enter()`——`Entered` guard 是 `!Send`，
        // 跨 await 持有会导致 future 非 `Send`（axum / `tokio::spawn` 需要 `Send`）。
        async move {
            // turn 开始：重置沙箱拒绝熔断器（单 turn 内有效，C-30）
            self.sandbox_breaker.reset();
            metrics::set_circuit_breaker("sandbox", "closed");

            // 1. 构造用户消息并入库
            let user_msg = Message::user_text(user_input.text);
            if let Err(e) = self.storage.append(&self.session.id, &user_msg).await {
                metrics::record_error("storage");
                return Err(RuntimeError::Storage(e));
            }
            self.ctx.append(user_msg.clone()).await;
            let event = Event::MessageAppended(user_msg);
            self.persist_event(&event).await;
            self.events.emit(event);

            let max_iters = self.config.context.max_tool_iters;
            let turn_timeout = Duration::from_secs(self.config.context.turn_timeout_sec);

            // 主循环封装为 future，由外层 select! 与 timeout/cancel 组合。
            // 使用 `async move` 避免 `async` 捕获 `&self` 的引用（产生 `&&self`），
            // 让 future 类型只借用 `&self`（单层引用），可与 SDK 的 `Box::pin` 配合。
            let turn_fut = async move {
                // 重复检测：记录每轮工具调用签名，连续 3 轮相同 → 死循环
                let mut call_signatures: Vec<String> = Vec::new();

                for _iter in 0..max_iters {
                    // 2. 构建请求（system + tools + 压缩后的历史）
                    let req = match self.ctx.build_chat_request(&self.tools, &self.config).await {
                        Ok(r) => r,
                        Err(e) => {
                            metrics::record_error("context");
                            return Err(e);
                        }
                    };

                    // 3. 流式调用 LLM
                    let assistant_msg = match self.stream_llm(req).await {
                        Ok(msg) => msg,
                        Err(e) => {
                            metrics::record_error("llm");
                            return Ok(TurnOutcome::Failed(e.into()));
                        }
                    };

                    // 4. 落盘 assistant 消息
                    if let Err(e) = self.storage.append(&self.session.id, &assistant_msg).await {
                        metrics::record_error("storage");
                        return Err(RuntimeError::Storage(e));
                    }
                    self.ctx.append(assistant_msg.clone()).await;
                    let event = Event::MessageAppended(assistant_msg.clone());
                    self.persist_event(&event).await;
                    self.events.emit(event);

                    // 5. 无工具调用 → 终止
                    if assistant_msg.tool_calls.is_empty() {
                        let event = Event::TurnEnd {
                            stop_reason: StopReason::EndTurn,
                        };
                        self.persist_event(&event).await;
                        self.events.emit(event);
                        return Ok(TurnOutcome::Finished(assistant_msg));
                    }

                    // 5.1 重复检测：连续 ≥3 轮相同工具调用集合 → 死循环，提前终止
                    //     （C-13 补充：max_tool_iters 之外的早期止损，避免无谓消耗）
                    let sig = Self::tool_calls_signature(&assistant_msg.tool_calls);
                    call_signatures.push(sig);
                    if Self::is_repeating(&call_signatures) {
                        tracing::warn!("turn terminated: repeated tool calls detected");
                        let event = Event::TurnEnd {
                            stop_reason: StopReason::Stopped,
                        };
                        self.persist_event(&event).await;
                        self.events.emit(event);
                        return Ok(TurnOutcome::Finished(Message::assistant_text(
                            "[检测到重复工具调用，已终止以避免死循环]".to_string(),
                        )));
                    }

                    // 6. 执行工具调用
                    let results = match self.execute_tool_calls(&assistant_msg.tool_calls).await {
                        Ok(r) => r,
                        Err(e) => {
                            metrics::record_error("tool");
                            return Ok(TurnOutcome::Failed(e));
                        }
                    };

                    // 7. 落盘 tool_result 并入上下文
                    for (id, result) in &results {
                        let msg = Self::tool_result_message(id.clone(), result.clone());
                        if let Err(e) = self.storage.append(&self.session.id, &msg).await {
                            metrics::record_error("storage");
                            return Err(RuntimeError::Storage(e));
                        }
                        self.ctx.append(msg.clone()).await;
                        let event = Event::MessageAppended(msg);
                        self.persist_event(&event).await;
                        self.events.emit(event);
                    }
                }

                // 达到 max_iters 上限
                tracing::warn!(max_iters, "turn exceeded max tool iterations");
                let event = Event::TurnEnd {
                    stop_reason: StopReason::Stopped,
                };
                self.persist_event(&event).await;
                self.events.emit(event);
                Ok(TurnOutcome::Finished(Message::assistant_text(
                    "[达到最大工具调用轮次上限]".to_string(),
                )))
            };

            // turn_timeout + Ctrl-C cancel（graceful stop；已落盘消息不丢失，C-13）
            // 三路 select：cancel 优先返回 Interrupted；timeout 返回 Finished(Stopped)；
            // turn_fut 正常完成则透传其 outcome（内部已 emit TurnEnd）。
            tokio::select! {
                () = self.cancel_token.cancelled() => {
                    tracing::info!("turn cancelled by user");
                    let event = Event::TurnEnd {
                        stop_reason: StopReason::Interrupted,
                    };
                    self.persist_event(&event).await;
                    self.events.emit(event);
                    Ok(TurnOutcome::Interrupted(Message::assistant_text(
                        "[已取消]".to_string(),
                    )))
                }
                () = tokio::time::sleep(turn_timeout) => {
                    tracing::warn!(
                        timeout_sec = self.config.context.turn_timeout_sec,
                        "turn timed out"
                    );
                    let event = Event::TurnEnd {
                        stop_reason: StopReason::Stopped,
                    };
                    self.persist_event(&event).await;
                    self.events.emit(event);
                    Ok(TurnOutcome::Finished(Message::assistant_text(
                        "[turn 超时终止]".to_string(),
                    )))
                }
                outcome = turn_fut => outcome,
            }
        }
        .instrument(span)
    }

    /// `run_turn` 的 `'static` 变体：取 `Arc<Runtime>` owned，返回 `BoxFuture<'static>`。
    ///
    /// **设计原因**：`run_turn(&self) -> impl Future + '_` 的 future 借用 `&self`，
    /// 当被 server handler（axum `Handler` trait 要求 `'static` + HRTB）`.await` 时，
    /// 生命周期参数泄漏到外层 future 类型，编译器报 "implementation of `FnOnce`
    /// is not general enough"。
    ///
    /// **实现**：`self: Arc<Self>` owned 移入 `async move` 块，`run_turn(&*self)` 的
    /// 借用是块内局部借用，await 后即释放。`Box::pin` 把块装箱为 `BoxFuture<'static>`。
    ///
    /// # Errors
    /// 同 [`Runtime::run_turn`] 的运行时错误。
    pub fn run_turn_owned(
        self: Arc<Self>,
        user_input: UserInput,
    ) -> BoxFuture<'static, Result<TurnOutcome, RuntimeError>> {
        Box::pin(async move { self.run_turn(user_input).await })
    }

    /// 计算一轮工具调用的签名（`name|规范化 input`，多调用排序后拼接）。
    ///
    /// `serde_json` 默认对 `Value::Object` 用 `BTreeMap`（键排序），保证 input
    /// 序列化与键顺序无关，跨轮比较稳定。用于重复检测识别"连续相同工具调用集合"。
    fn tool_calls_signature(calls: &[ToolCall]) -> String {
        let mut sigs: Vec<String> = calls
            .iter()
            .map(|c| {
                let input = serde_json::to_string(&c.input).unwrap_or_else(|_| c.input.to_string());
                format!("{}|{}", c.name, input)
            })
            .collect();
        sigs.sort_unstable();
        sigs.join(";")
    }

    /// 检测最近 3 轮工具调用签名是否完全相同（连续 ≥3 轮 → 死循环）。
    fn is_repeating(signatures: &[String]) -> bool {
        let n = signatures.len();
        if n < 3 {
            return false;
        }
        let last = &signatures[n - 1];
        signatures[n - 3..].iter().all(|s| s == last)
    }

    /// 流式调用 LLM 并聚合为 assistant 消息。
    ///
    /// `OTel`：`llm_call` span 包裹整次 provider 调用（design.md §15.1），字段不含
    /// 凭证（C-04：仅记 model 与消息数，不记 input 原文）。
    async fn stream_llm(&self, req: ChatRequest) -> Result<Message, crate::model::LlmError> {
        let timer = metrics::start_timer();
        let model_name = req.params.model.clone();
        let provider_id = self.provider.id();
        let span = tracing::info_span!(
            "llm_call",
            session.id = %self.session.id,
            llm.provider = %provider_id,
            llm.model = %model_name,
            message_count = req.messages.len(),
            otel.name = span_name::LLM_CHAT_STREAM,
        );
        let _enter = span.enter();

        let mut stream = self.provider.chat_stream(req).await?;
        let mut acc = DeltaAccumulator::new();
        self.events.emit(Event::TurnStreamingStarted);

        while let Some(delta) = stream.next().await {
            let delta = delta?;
            if let Delta::Text(ref s) = delta {
                self.events.emit(Event::Token(s.clone()));
            }
            acc.push(delta);
        }

        // Metrics: 记录 LLM token 消耗
        if let Some(usage) = acc.usage() {
            metrics::record_llm_tokens(&model_name, "input", usage.input_tokens as u64);
            metrics::record_llm_tokens(&model_name, "output", usage.output_tokens as u64);
            if let Some(cached) = usage.cache_read {
                metrics::record_llm_tokens(&model_name, "cached", cached as u64);
            }
        }
        // Metrics: 记录 LLM 调用延迟
        metrics::record_elapsed("llm_call_duration_ms", "model", &model_name, timer);
        Ok(acc.finalize())
    }

    /// 执行工具调用（无副作用并行、有副作用串行 + 权限检查）。
    ///
    /// 副作用工具（`SideEffect != None`）在 dispatch 前必须经
    /// `PermissionPolicy::check` → 必要时 `PermissionPrompter::prompt` → 决策落
    /// `AuditSink`（C-01、AGENTS.md §5.5）。只读工具直接并行执行（BuiltinPolicy
    /// 对 `SideEffect::None` 返回 `Allow`，此处跳过以避免无谓 IO）。
    async fn execute_tool_calls(
        &self,
        calls: &[ToolCall],
    ) -> Result<Vec<(ToolCallId, ToolResult)>, RuntimeError> {
        // 构造 ToolContext：注入沙箱驱动/策略/journal（M4，shell.run/fs 用）
        let ctx = ToolContext::new(self.workdir.read().await.clone(), self.session.id.clone())
            .with_sandbox(self.sandbox_driver.clone(), self.sandbox_policy.clone())
            .with_journal_opt(self.journal.clone());

        // 分桶：无副作用 → 并行；有副作用 → 串行（含权限检查）
        let (readonly, side_effect): (Vec<&ToolCall>, Vec<&ToolCall>) =
            calls.iter().partition(|c| {
                self.tools
                    .get(&c.name)
                    .is_none_or(|t| t.side_effect() == SideEffect::None)
            });

        let mut results: Vec<(ToolCallId, ToolResult)> = Vec::with_capacity(calls.len());

        // 无副作用：并发执行（最多 8 并发）
        // 克隆 `events`/`tools` 到闭包外，让 async 块只捕获 owned 数据，
        // 避免捕获 `&self` 导致 future 非 `'static`（无法被 SDK `tokio::spawn`）。
        //
        // **HRTB 修复**：`readonly.iter().map(|call| async move { ... })` 中 `call`
        // 是 `&&ToolCall`，闭包签名 `fn(&'a &'b ToolCall) -> impl Future + 'a` 不满足
        // `buffer_unordered` 要求的 HRTB（future 类型对任意 `'a` 必须相同）。把每个
        // future 装箱为 `Pin<Box<dyn Future + Send>>`，擦除生命周期参数，统一类型。
        let events = self.events.clone();
        let tools = self.tools.clone();
        let ro_futs: Vec<ToolFuture> = readonly
            .iter()
            .map(|call| {
                let ctx = ctx.clone();
                let call_id = call.id.clone();
                let tool_name = call.name.clone();
                let events = events.clone();
                let tools = tools.clone();
                // `call` 是 `&&ToolCall`（来自 `Vec<&ToolCall>::iter`），需解引用到
                // `ToolCall` 再 clone，否则只克隆引用，async 块仍借用 `readonly`。
                let call: ToolCall = (**call).clone();
                let fut: ToolFuture = Box::pin(async move {
                    // `tool_call` span（design.md §15.1）：只读桶并行执行，每个调用独立 span。
                    let tool_timer = metrics::start_timer();
                    let span = tracing::debug_span!(
                        "tool_call",
                        session.id = %ctx.session_id,
                        tool.name = %tool_name,
                        tool.side_effect = "none",
                        tool.parallel = true,
                        call_id = %call_id,
                        otel.name = span_name::TOOL_CALL,
                    );
                    let _enter = span.enter();
                    events.emit(Event::ToolCallStarted {
                        call_id: call_id.clone(),
                        tool: tool_name.clone(),
                    });
                    let result = match tools.dispatch(&call, &ctx).await {
                        Ok(r) => r,
                        // design.md §4.5：工具错误以 is_error=true 回灌 LLM 自我修正，
                        // 不中止 turn（未知工具/参数不合法等模型可自行纠正）。
                        Err(e) => ToolResult::err_text(format!("tool error: {e}")),
                    };
                    events.emit(Event::ToolCallFinished {
                        call_id: call_id.clone(),
                        result: result.clone(),
                    });
                    // Metrics: 记录工具调用
                    let result_str = if result.is_error { "err" } else { "ok" };
                    metrics::record_tool_call(&tool_name, "none", result_str);
                    metrics::record_elapsed(
                        "tool_call_duration_ms",
                        "tool",
                        &tool_name,
                        tool_timer,
                    );
                    Ok::<_, RuntimeError>((call.id.clone(), result))
                });
                fut
            })
            .collect();
        let mut ro_stream = futures::stream::iter(ro_futs).buffer_unordered(8);
        while let Some(r) = ro_stream.next().await {
            results.push(r?);
        }

        // 有副作用：严格串行，每个工具先过权限（见 execute_side_effect_call）
        for call in &side_effect {
            // 查找工具的 side_effect 类型用于 span 属性
            let call_side_effect = self
                .tools
                .get(&call.name)
                .map_or(SideEffect::None, |t| t.side_effect());
            // `tool_call` span（design.md §15.1）：副作用桶串行执行，包裹权限检查 + dispatch。
            let span = tracing::debug_span!(
                "tool_call",
                session.id = %ctx.session_id,
                tool.name = %call.name,
                tool.side_effect = ?call_side_effect,
                tool.parallel = false,
                call_id = %call.id,
                otel.name = span_name::TOOL_CALL,
            );
            let _enter = span.enter();
            results.push(self.execute_side_effect_call(call, &ctx).await?);
        }

        // 按 LLM 原始顺序回填，保证 tool_result 与 tool_calls 一一对应
        results.sort_by_key(|(id, _)| calls.iter().position(|c| c.id == *id).unwrap_or(usize::MAX));

        Ok(results)
    }

    /// 对单个副作用工具调用执行权限检查 + Hook + 调度（C-01 实现层强制）。
    ///
    /// 流程（见 `hooks.md` §4）：
    /// 1. `policy.check` → `Verdict`（C-02：内置黑名单在此优先级最高）
    /// 2. `PreToolUse` Hook：可 deny/allow（升级 `Ask→Allow`）/`modify_input`/continue
    ///    （C-21：内置黑名单 Deny 时 Hook 的 Allow 被忽略）
    /// 3. 若仍 `Ask` → `PermissionRequest` Hook：可直接给 Decision 跳过 prompter
    /// 4. 若仍 `Ask` → `PermissionPrompter` 交互
    /// 5. 落审计 → 按决策执行或拒绝
    /// 6. 执行成功 → `PostToolUse` Hook；执行失败 → `PostToolUseFailure` Hook
    ///
    /// `Deny`（策略/Hook/用户）返回 `Ok` 带 `is_error=true` 的结果；仅 `dispatch`
    /// 失败返回 `Err`（与原 `?` 传播语义一致）。
    async fn execute_side_effect_call(
        &self,
        call: &ToolCall,
        ctx: &ToolContext,
    ) -> Result<(ToolCallId, ToolResult), RuntimeError> {
        let side_effect = self
            .tools
            .get(&call.name)
            .map_or(SideEffect::None, |t| t.side_effect());

        // `permission` span（design.md §15.1）：包裹权限决策流程（策略判定 →
        // Hook → prompter 交互 → 审计落盘）。字段不含 input 原文（C-04）。
        // `permission.verdict` 在决策确定后通过 `Span::current().record()` 填充。
        let span = tracing::info_span!(
            "permission",
            session.id = %self.session.id,
            tool.name = %call.name,
            tool.side_effect = ?side_effect,
            permission.verdict = tracing::field::Empty,
            otel.name = span_name::PERMISSION_CHECK,
        );
        let _enter = span.enter();

        let plan_snap = self.plan_state.read().await.clone();
        let perm_ctx = PermissionContext {
            session: self.session.id.clone(),
            workdir: self.workdir.read().await.clone(),
            side_effect,
            turn: 0,
            history: Vec::new(),
            permission_mode: plan_snap.mode,
            allowed_prompts: plan_snap.allowed_prompts,
        };

        // 1. 策略判定（C-02：内置黑名单在此优先级最高，不可覆盖）
        let verdict = match self.policy.check(&call.name, &call.input, &perm_ctx).await {
            Ok(v) => v,
            Err(e) => {
                self.record_permission_audit(&call.name, &Decision::Deny(e.to_string()), None)
                    .await;
                tracing::warn!(tool = %call.name, error = %e, "policy check failed");
                return Ok((
                    call.id.clone(),
                    ToolResult::err_text(format!("permission error: {e}")),
                ));
            }
        };

        // 2. 构建 Hook 分发配置 + C-21 builtin_deny 标记
        let dispatch_cfg = self.build_dispatch_config(&verdict);
        let is_builtin_deny = matches!(verdict, Verdict::Deny(_));

        // 3. PreToolUse Hook（policy.check 之后、工具执行前）
        let mut effective_call = call.clone();
        let pre_decision = self
            .run_pre_tool_use_hook(
                call,
                side_effect,
                &verdict,
                &dispatch_cfg,
                &mut effective_call,
            )
            .await?;

        // 4. 解析为最终决策（PreToolUse 直出 / Verdict Allow|Deny / Ask→PermissionRequest Hook→prompter）
        let (decision, prompt_id) = if let Some(d) = pre_decision {
            (d, None)
        } else {
            self.resolve_decision(&verdict, call, side_effect, &dispatch_cfg, is_builtin_deny)
                .await?
        };

        // 5. 落审计（所有副作用权限决策均落盘，AGENTS.md §5.5；
        //    C-04：detail 不含工具输入原文，避免凭证外泄）
        self.record_permission_audit(&call.name, &decision, prompt_id)
            .await;

        // Metrics: 记录权限决策
        let verdict_str = match &decision {
            Decision::Allow => "allow",
            Decision::Deny(_) => "deny",
        };
        metrics::record_permission(verdict_str);

        // Span 属性：动态记录最终 verdict（决策在 span 创建后才确定）
        tracing::Span::current().record("permission.verdict", verdict_str);

        // 6. 按决策执行或拒绝
        let side_effect_str = match side_effect {
            SideEffect::None => "none",
            SideEffect::FileWrite => "file_write",
            SideEffect::Command => "command",
            SideEffect::Network => "network",
        };
        let tool_timer = metrics::start_timer();
        let result = match decision {
            Decision::Deny(msg) => Ok((
                call.id.clone(),
                ToolResult::err_text(format!("permission denied: {msg}")),
            )),
            Decision::Allow => {
                self.execute_allowed_call(call, &effective_call, side_effect, ctx)
                    .await
            }
        };
        // Metrics: 记录副作用工具调用
        let result_str = match &result {
            Ok((_, r)) if r.is_error => "err",
            Ok(_) => "ok",
            Err(_) => "err",
        };
        metrics::record_tool_call(&call.name, side_effect_str, result_str);
        metrics::record_elapsed("tool_call_duration_ms", "tool", &call.name, tool_timer);
        if result.is_err() {
            metrics::record_error("tool");
        }
        result
    }

    /// 构建 Hook 分发配置（来自 `HooksConfig` + 当前 `Verdict`）。
    ///
    /// C-21：policy 返回 `Deny` 时视为内置黑名单 Deny（当前 `BuiltinPolicy` 仅产出
    /// L0 Deny：项目文档保护 C-02、路径越界 C-03），Hook 的 Allow 被忽略。
    fn build_dispatch_config(&self, verdict: &Verdict) -> DispatchConfig {
        let hook_config = &self.config.hooks;
        let builtin_deny = match verdict {
            Verdict::Deny(msg) => Some(msg.clone()),
            _ => None,
        };
        DispatchConfig {
            on_error: hook_config.on_hook_error,
            timeout: Duration::from_secs(hook_config.default_timeout_sec),
            builtin_deny,
        }
    }

    /// 运行 `PreToolUse` Hook（policy.check 之后、工具执行前）。
    ///
    /// 返回 `Some(Decision)` 表示 Hook 直接给出决策（Deny 或 Allow 升级）；
    /// 返回 `None` 表示 Hook 未决策（`Continue`/`Ask`/`Allow` on `builtin_deny`），由
    /// 调用方继续走 `resolve_decision` 解析。
    ///
    /// `effective_call` 在 Hook 返回 `modify_input` 时被就地更新（仍经
    /// `sandbox_path` 校验，由工具 dispatch 时执行）。
    async fn run_pre_tool_use_hook(
        &self,
        call: &ToolCall,
        side_effect: SideEffect,
        verdict: &Verdict,
        dispatch_cfg: &DispatchConfig,
        effective_call: &mut ToolCall,
    ) -> Result<Option<Decision>, RuntimeError> {
        let is_builtin_deny = matches!(verdict, Verdict::Deny(_));
        let hook_input = self
            .build_hook_input(HookEvent::PreToolUse, call, side_effect, Some(verdict))
            .await;
        let result = self
            .hook_registry
            .dispatch(hook_input, dispatch_cfg.clone())
            .await;
        if let Some(fatal) = result.fatal_error {
            return Err(RuntimeError::Hook(fatal.to_string()));
        }
        // C-21：builtin_deny 时 Hook 的 Allow 被忽略（dispatch 已处理）
        let pre_decision = match result.decision {
            HookDecision::Deny => {
                let reason = result
                    .reason
                    .unwrap_or_else(|| "blocked by hook".to_string());
                Some(Decision::Deny(reason))
            }
            HookDecision::Allow if !is_builtin_deny => {
                // Hook 升级 Ask→Allow（不降级已有 Allow）
                Some(Decision::Allow)
            }
            _ => None, // Continue/Ask/Allow(builtin_deny) 不直接给决策
        };
        // 应用 modify_input（仍经 sandbox_path 校验，由工具 dispatch 时执行）
        if let Some(new_input) = result.modify_input {
            effective_call.input = new_input;
        }
        // exit_messages 记日志（供观测）
        for msg in &result.exit_messages {
            tracing::info!(tool = %call.name, hook_msg = %msg, "PreToolUse hook exit message");
        }
        Ok(pre_decision)
    }

    /// 解析为最终决策（PreToolUse 未直出决策时）。
    ///
    /// - `Allow` / `Deny` → 直出
    /// - `Ask` → 先跑 `PermissionRequest` Hook（可能短路）；未短路则走 `prompter`
    ///
    /// 返回 `(Decision, Option<prompt_id>)`：`prompt_id` 为 `Some` 表示经用户交互。
    async fn resolve_decision(
        &self,
        verdict: &Verdict,
        call: &ToolCall,
        side_effect: SideEffect,
        dispatch_cfg: &DispatchConfig,
        is_builtin_deny: bool,
    ) -> Result<(Decision, Option<String>), RuntimeError> {
        match verdict {
            Verdict::Allow => Ok((Decision::Allow, None)),
            Verdict::Deny(msg) => Ok((Decision::Deny(msg.clone()), None)),
            Verdict::Ask(prompt) => {
                // PermissionRequest Hook（Verdict::Ask 时、prompter 前）
                let hook_input = self
                    .build_hook_input(
                        HookEvent::PermissionRequest,
                        call,
                        side_effect,
                        Some(verdict),
                    )
                    .await;
                let result = self
                    .hook_registry
                    .dispatch(hook_input, dispatch_cfg.clone())
                    .await;
                if let Some(fatal) = result.fatal_error {
                    return Err(RuntimeError::Hook(fatal.to_string()));
                }
                match result.decision {
                    HookDecision::Allow if !is_builtin_deny => {
                        // Hook 自动批准，跳过 prompter
                        Ok((Decision::Allow, None))
                    }
                    HookDecision::Deny => {
                        let reason = result
                            .reason
                            .unwrap_or_else(|| "blocked by hook".to_string());
                        Ok((Decision::Deny(reason), None))
                    }
                    _ => {
                        // Hook 未决策 → 走 prompter 交互
                        let prompt_id = prompt.id.clone();
                        self.events.emit(Event::PermissionRequested {
                            id: prompt.id.clone(),
                            tool: prompt.tool.clone(),
                            summary: prompt.summary.clone(),
                            risk: prompt.risk,
                        });
                        let d = self.prompter.prompt(prompt.clone()).await;
                        let event = Event::PermissionResolved {
                            id: prompt_id.clone(),
                            decision: d.clone(),
                        };
                        self.persist_event(&event).await;
                        self.events.emit(event);
                        Ok((d, Some(prompt_id)))
                    }
                }
            }
        }
    }

    /// 执行已 Allow 的工具调用（含沙箱拒绝检测、PostToolUse/PostToolUseFailure Hook）。
    async fn execute_allowed_call(
        &self,
        original_call: &ToolCall,
        effective_call: &ToolCall,
        side_effect: SideEffect,
        ctx: &ToolContext,
    ) -> Result<(ToolCallId, ToolResult), RuntimeError> {
        self.events.emit(Event::ToolCallStarted {
            call_id: original_call.id.clone(),
            tool: original_call.name.clone(),
        });
        let result = match self.tools.dispatch(effective_call, ctx).await {
            Ok(r) => r,
            Err(e) => {
                // 沙箱拒绝检测（T-M4-5）：识别 EPERM/EACCES/landlock 等
                // 内核级硬反馈，更新熔断器（C-30 不可被 LLM 绕过）。
                if let Some(denial_result) =
                    self.handle_sandbox_denial(&original_call.id, &original_call.name, &e)
                {
                    return Ok(denial_result);
                }
                // PostToolUseFailure Hook（非 denial 错误）
                self.run_post_failure_hook(effective_call, side_effect, &e)
                    .await;
                // design.md §4.5：工具错误以 is_error=true 回灌 LLM 自我修正，不中止 turn。
                ToolResult::err_text(format!("tool error: {e}"))
            }
        };
        // PostToolUse Hook（执行成功后）
        self.run_post_success_hook(effective_call, side_effect, &result)
            .await;
        self.events.emit(Event::ToolCallFinished {
            call_id: original_call.id.clone(),
            result: result.clone(),
        });
        Ok((original_call.id.clone(), result))
    }

    /// 构造 `HookInput`（工具相关事件通用）。
    async fn build_hook_input(
        &self,
        event: HookEvent,
        call: &ToolCall,
        side_effect: SideEffect,
        verdict: Option<&Verdict>,
    ) -> HookInput {
        let verdict_serde = verdict.map(|v| match v {
            Verdict::Allow => VerdictSerde::Allow,
            Verdict::Deny(msg) => VerdictSerde::Deny {
                reason: msg.clone(),
            },
            Verdict::Ask(prompt) => VerdictSerde::Ask {
                tool: prompt.tool.clone(),
                summary: prompt.summary.clone(),
            },
        });
        HookInput {
            event,
            session_id: self.session.id.clone(),
            turn: 0,
            tool: Some(call.clone()),
            side_effect: Some(side_effect),
            verdict: verdict_serde,
            cwd: self.workdir.read().await.clone(),
            extras: serde_json::Value::Null,
        }
    }

    /// 运行 `PostToolUse` Hook（工具执行成功后，见 `hooks.md` §4）。
    ///
    /// Hook 可跑 formatter/linter（副作用在 Hook 内部完成），`exit_message` 记日志。
    /// `async_rewake` 暂不处理（AsyncRewakeManager 集成在后续任务）。
    async fn run_post_success_hook(
        &self,
        call: &ToolCall,
        side_effect: SideEffect,
        _result: &ToolResult,
    ) {
        let hook_config = &self.config.hooks;
        if hook_config.post_tool_use.is_empty() {
            return; // 无 PostToolUse Hook，快速跳过
        }
        let dispatch_cfg = DispatchConfig {
            on_error: hook_config.on_hook_error,
            timeout: Duration::from_secs(hook_config.default_timeout_sec),
            builtin_deny: None,
        };
        let hook_input = self
            .build_hook_input(HookEvent::PostToolUse, call, side_effect, None)
            .await;
        let result = self.hook_registry.dispatch(hook_input, dispatch_cfg).await;
        if let Some(fatal) = result.fatal_error {
            tracing::error!(hook_error = %fatal, "PostToolUse hook fatal error");
        }
        for msg in &result.exit_messages {
            tracing::info!(tool = %call.name, hook_msg = %msg, "PostToolUse hook exit message");
        }
    }

    /// 运行 `PostToolUseFailure` Hook（工具执行失败后，见 `hooks.md` §4）。
    ///
    /// Hook 可诊断失败原因、记录错误模式。`exit_message` 记日志。
    async fn run_post_failure_hook(
        &self,
        call: &ToolCall,
        side_effect: SideEffect,
        _error: &crate::model::ToolError,
    ) {
        let hook_config = &self.config.hooks;
        if hook_config.post_tool_use_failure.is_empty() {
            return; // 无 PostToolUseFailure Hook，快速跳过
        }
        let dispatch_cfg = DispatchConfig {
            on_error: hook_config.on_hook_error,
            timeout: Duration::from_secs(hook_config.default_timeout_sec),
            builtin_deny: None,
        };
        let hook_input = self
            .build_hook_input(HookEvent::PostToolUseFailure, call, side_effect, None)
            .await;
        let result = self.hook_registry.dispatch(hook_input, dispatch_cfg).await;
        if let Some(fatal) = result.fatal_error {
            tracing::error!(hook_error = %fatal, "PostToolUseFailure hook fatal error");
        }
        for msg in &result.exit_messages {
            tracing::info!(tool = %call.name, hook_msg = %msg, "PostToolUseFailure hook exit message");
        }
    }

    /// 沙箱拒绝检测与熔断处理（T-M4-5）。
    ///
    /// 检测工具错误是否为沙箱拒绝（EPERM/EACCES/landlock 等）。若是：
    /// - 更新熔断器计数；
    /// - 软熔断（≥3 次）：附加方向提醒返回；
    /// - 硬熔断（≥5 次）：返回带总结的错误；
    /// - 未熔断：返回带 denial 标识的错误，提示 LLM/用户。
    ///
    /// 返回 `Some(ToolResult)` 表示已识别为 denial 并生成回灌结果；
    /// 返回 `None` 表示非 denial，调用方原样传播错误。
    fn handle_sandbox_denial(
        &self,
        call_id: &ToolCallId,
        tool: &str,
        error: &crate::model::ToolError,
    ) -> Option<(ToolCallId, ToolResult)> {
        let error_text = error.to_string();
        let m = self.denial_detector.detect(tool, &error_text)?;
        tracing::warn!(
            tool = %m.tool,
            reason = m.signature.reason,
            platform = m.signature.platform,
            "sandbox denial detected"
        );
        let state = self.sandbox_breaker.record_denial();
        let result = match state {
            BreakerState::HardTripped => {
                let summary = crate::sandbox::hard_trip_summary(self.sandbox_breaker.count());
                tracing::warn!(
                    count = self.sandbox_breaker.count(),
                    "sandbox circuit breaker hard-tripped"
                );
                metrics::set_circuit_breaker("sandbox", "hard_tripped");
                metrics::record_error("sandbox");
                ToolResult {
                    content: crate::model::ToolContent::Text(format!(
                        "{summary}\n原始错误：{error_text}"
                    )),
                    is_error: true,
                    metadata: crate::model::ToolResultMeta::default(),
                }
            }
            BreakerState::SoftTripped => {
                let reminder = crate::sandbox::soft_trip_reminder(self.sandbox_breaker.count());
                tracing::warn!(
                    count = self.sandbox_breaker.count(),
                    "sandbox circuit breaker soft-tripped"
                );
                metrics::set_circuit_breaker("sandbox", "soft_tripped");
                metrics::record_error("sandbox");
                ToolResult {
                    content: crate::model::ToolContent::Text(format!(
                        "沙箱拒绝（{reason}）：{error_text}\n\n{reminder}",
                        reason = m.signature.reason
                    )),
                    is_error: true,
                    metadata: crate::model::ToolResultMeta::default(),
                }
            }
            BreakerState::Closed => {
                metrics::record_error("sandbox");
                ToolResult::err_text(format!(
                    "sandbox denied ({reason}): {error_text}\n\
                     提示：可切换更宽松的沙箱预设（如 --sandbox workspace-write）重试",
                    reason = m.signature.reason
                ))
            }
        };
        Some((call_id.clone(), result))
    }

    /// 记录权限决策审计（C-01 决策可追溯，AGENTS.md §5.5）。
    ///
    /// `prompt_id` 为 `Some` 表示经用户交互（Ask→prompter），`None` 表示策略直出
    /// （Allow/Deny）。审计落盘失败仅记 `warn` 日志，不中断工具执行——审计失败不应
    /// 阻断主流程，但会被运维发现并处理。
    async fn record_permission_audit(
        &self,
        tool: &str,
        decision: &Decision,
        prompt_id: Option<String>,
    ) {
        let (decision_str, detail) = match (decision, prompt_id.is_some()) {
            (Decision::Allow, true) => ("allow", format!("user allowed {tool}")),
            (Decision::Allow, false) => ("allow", format!("policy allowed {tool}")),
            (Decision::Deny(reason), true) => ("deny", format!("user denied {tool}: {reason}")),
            (Decision::Deny(reason), false) => ("deny", format!("policy denied {tool}: {reason}")),
        };
        let rec = AuditRecord {
            ts: OffsetDateTime::now_utc(),
            session: self.session.id.clone(),
            kind: AuditKind::PermissionResolved,
            tool: Some(tool.to_string()),
            decision: Some(decision_str.to_string()),
            detail,
        };
        if let Err(e) = self.audit.record(rec).await {
            tracing::warn!(error = %e, "audit record failed");
        }
    }

    /// 构造 `tool_result` 消息。
    fn tool_result_message(call_id: ToolCallId, result: ToolResult) -> Message {
        use crate::model::{ContentBlock, MessageMeta, MessageSource};
        let content = vec![ContentBlock::ToolResult {
            call_id,
            content: result.content,
            is_error: result.is_error,
        }];
        Message {
            id: ulid::Ulid::new().to_string(),
            role: crate::model::Role::Tool,
            content,
            tool_calls: Vec::new(),
            tool_call_id: None,
            created_at: time::OffsetDateTime::now_utc(),
            metadata: MessageMeta {
                source: MessageSource::Tool,
                ..Default::default()
            },
        }
    }
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("session_id", &self.session.id)
            .field(
                "workdir",
                &self.workdir.try_read().map_or_else(
                    |_| camino::Utf8PathBuf::from("<locked>"),
                    |guard| guard.clone(),
                ),
            )
            .field("tools_count", &self.tools.len())
            .finish_non_exhaustive()
    }
}

/// `PlanModeController` 适配器（共享 Runtime 的 `plan_state` + `events`）。
///
/// 由 `Runtime::plan_controller` 构造，注入到 `plan.exit` 工具。`plan.exit` 通过
/// 它读写会话级 Plan 状态。设计为独立结构而非 `Runtime impl PlanModeController`，
/// 避免给 Runtime 增加无关方法（`Arc<dyn PlanModeController>` 更显式）。
///
/// Event Sourcing：持有 `event_store`/`event_seq`/`durable_seq`/`session_id`，
/// `exit_plan`/`set_mode` 触发 `PermissionModeChanged` 时同步持久化到事件流
/// （replay 时重建 `final_permission_mode`，见 `replay_session_state`）。
struct PlanControllerHandle {
    state: Arc<RwLock<PlanModeSnapshot>>,
    events: EventBus,
    /// Event Sourcing 持久化字段（与 Runtime 共享 Arc）。
    session_id: SessionId,
    event_store: Arc<dyn EventStore>,
    event_seq: Arc<TokioMutex<u64>>,
    durable_seq: Arc<TokioMutex<u64>>,
}

impl PlanControllerHandle {
    /// 持久化 `PermissionModeChanged` 事件到 EventStore（best-effort，无 snapshot 触发）。
    ///
    /// 与 `Runtime::persist_event` 区别：不触发 snapshot（`PermissionModeChanged`
    /// 非 `MessageAppended`，不计入 `message_since_snapshot`）；不调用
    /// `try_persist`（调用方已知是持久化事件）。
    async fn persist_mode_changed(&self, from: PermissionMode, to: PermissionMode) {
        let seq = {
            let mut guard = self.event_seq.lock().await;
            let s = *guard;
            *guard += 1;
            s
        };
        let record = EventRecord::new(
            seq,
            self.session_id.clone(),
            PersistedEvent::PermissionModeChanged { from, to },
        );
        if let Err(e) = self.event_store.append(&self.session_id, record).await {
            tracing::warn!(
                error = %e,
                session = %self.session_id,
                seq,
                "PermissionModeChanged persist failed (best-effort, continue)"
            );
            return;
        }
        let mut guard = self.durable_seq.lock().await;
        if seq > *guard {
            *guard = seq;
        }
    }
}

impl PlanModeController for PlanControllerHandle {
    fn snapshot(&self) -> BoxFuture<'_, PlanModeSnapshot> {
        let state = self.state.clone();
        Box::pin(async move { state.read().await.clone() })
    }

    fn exit_plan(
        &self,
        allowed_prompts: Vec<PreApprovedPrompt>,
        target_mode: PermissionMode,
    ) -> BoxFuture<'_, Result<(), PolicyError>> {
        let state = self.state.clone();
        let events = self.events.clone();
        let persister_session = self.session_id.clone();
        let persister_store = self.event_store.clone();
        let persister_seq = self.event_seq.clone();
        let persister_durable = self.durable_seq.clone();
        Box::pin(async move {
            let mut snap = state.write().await;
            if snap.mode != PermissionMode::Plan {
                return Err(PolicyError::Policy(format!(
                    "plan.exit 仅在 Plan 模式下可调用（当前：{:?}）",
                    snap.mode
                )));
            }
            let from = snap.mode;
            snap.mode = target_mode;
            snap.allowed_prompts = allowed_prompts;
            drop(snap);
            // Event Sourcing：持久化 PermissionModeChanged（best-effort）
            let handle = PlanControllerHandle {
                state,
                events: events.clone(),
                session_id: persister_session,
                event_store: persister_store,
                event_seq: persister_seq,
                durable_seq: persister_durable,
            };
            handle.persist_mode_changed(from, target_mode).await;
            events.emit(Event::PermissionModeChanged {
                from,
                to: target_mode,
            });
            tracing::info!(from = ?from, to = ?target_mode, "PermissionMode switched by plan.exit");
            Ok(())
        })
    }

    fn set_mode(&self, mode: PermissionMode) -> BoxFuture<'_, ()> {
        let state = self.state.clone();
        let events = self.events.clone();
        let persister_session = self.session_id.clone();
        let persister_store = self.event_store.clone();
        let persister_seq = self.event_seq.clone();
        let persister_durable = self.durable_seq.clone();
        Box::pin(async move {
            let mut snap = state.write().await;
            let from = snap.mode;
            snap.mode = mode;
            // set_mode 是 CLI 显式切换，不重置 allowed_prompts（保留先前 plan.exit 缓存）
            drop(snap);
            // Event Sourcing：持久化 PermissionModeChanged（best-effort）
            let handle = PlanControllerHandle {
                state,
                events: events.clone(),
                session_id: persister_session,
                event_store: persister_store,
                event_seq: persister_seq,
                durable_seq: persister_durable,
            };
            handle.persist_mode_changed(from, mode).await;
            events.emit(Event::PermissionModeChanged { from, to: mode });
            tracing::info!(from = ?from, to = ?mode, "PermissionMode switched by CLI");
        })
    }
}

// 静态断言：`Runtime` 是 `Send + Sync`（多线程 runtime / axum 需要）。
// 通过 `const` 绑定强制 trait bound 检查，无需运行时调用。
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Runtime>();
};
