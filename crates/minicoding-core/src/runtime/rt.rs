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

use super::plan_handle::PlanControllerHandle;
use super::repeat_guard;
use crate::config::{ConfigWatcher, RuntimeConfig};
use crate::context::ContextManager;
use crate::hooks::{
    DispatchConfig, HookDecision, HookEvent, HookInput, HookRegistry, VerdictSerde,
};
use crate::journal::Journal;
use crate::memory::SessionSummarizer;
use crate::metrics;
use crate::model::{
    ContentBlock, Message, Role, RuntimeError, Session, SideEffect, StopReason, ToolCall,
    ToolCallId, ToolResult, TurnOutcome, UserInput,
};
use crate::otel::span_name;
use crate::policy::{
    Decision, PermissionContext, PermissionPolicy, PermissionPrompt, PermissionPrompter,
    PlanModeController, PlanModeSnapshot, Verdict,
};
use crate::provider::{BoxFuture, ChatRequest, Delta, LlmProvider};
use crate::runtime::accumulator::DeltaAccumulator;
use crate::runtime::{Event, EventBus};
use crate::sandbox::{BreakerState, SandboxDriver, SandboxPolicy};
use crate::storage::{
    AuditKind, AuditRecord, AuditSink, EventRecord, EventStore, PersistedEvent, SNAPSHOT_INTERVAL,
    SessionSnapshot, SessionState, SnapshotStore, Storage, try_persist,
};
use crate::tool::{ToolContext, ToolRegistry};
use camino::Utf8PathBuf;
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
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

/// turn 运行标记 guard：drop 时复位 `turn_active`（覆盖 `?` 早退与 panic 路径）。
struct TurnActiveGuard<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for TurnActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Runtime 聚合根（所有可替换能力的持有者）。
///
/// 由 `RuntimeBuilder` 构造，frontend 长期持有。
/// `run_turn` 驱动单轮 Agent 循环（用户输入 → LLM → 工具 → ... → 最终回复）。
pub struct Runtime {
    pub(crate) provider: Arc<dyn LlmProvider>,
    pub(crate) ctx: Arc<dyn ContextManager>,
    pub(crate) storage: Arc<dyn Storage>,
    pub(crate) tools: ToolRegistry,
    /// 运行期配置（M-12 起锁保护，见 `tech-stack.md` §13 决策记录）。
    ///
    /// turn 边界 `reload_safe_config` 做白名单热更新（`provider.model`/
    /// `context.turn_timeout_sec`/`tools.parallel_reads`），其余字段变更仅告警
    /// 提示重启。用 `std::sync::RwLock`（非 tokio）：`build_dispatch_config`
    /// 是同步 fn 无法 `read().await`；所有读取点均为短临界区，guard 不跨 await
    /// （满足 clippy `await_holding_lock`）。
    pub(crate) config: std::sync::RwLock<RuntimeConfig>,
    /// config.toml 路径（M-12：`Some` 时 turn 边界白名单热更新启用）。
    ///
    /// CLI 注入 `paths::config_path()`；server 不注入（配置全部来自参数，保持不启用）。
    pub(crate) config_path: Option<Utf8PathBuf>,
    /// 上次文件版本的非白名单签名（`reload_safe_config` 用：首次加载不告警，
    /// 检测到后续非白名单变更时 warn 提示重启）。
    pub(crate) last_non_whitelist_sig: std::sync::Mutex<Option<u64>>,
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
    ///
    /// `CancellationToken` 取消后**永久 cancelled**，无法复位；若一次取消
    /// 永久生效，会话会被"砖化"（之后所有 turn 秒取消）。故每次 `run_turn`
    /// 结束（含取消/超时）时重建 token；`turn_active` 标记使 `cancel()` 仅
    /// 对**运行中的 turn** 生效——turn 间隙的取消调用不毒化下一轮。
    /// （std Mutex 临界区无 await；`turn_active` 用原子避免锁。）
    pub(crate) cancel_token: std::sync::Mutex<CancellationToken>,
    /// 当前是否有 turn 在运行（`cancel()` 的生效条件，原子读写）。
    pub(crate) turn_active: std::sync::atomic::AtomicBool,
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
    /// 沙箱拒绝检测器（M-05 抽象注入，默认 `NoopDenialDetector` 兜底）。
    pub(crate) denial_detector: Arc<dyn crate::sandbox::SandboxDenialDetector>,
    /// 沙箱拒绝熔断器（单 turn 内有效，C-30 不可被 LLM 绕过；M-05 抽象注入）。
    pub(crate) sandbox_breaker: Arc<dyn crate::sandbox::SandboxDenialTracker>,
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
        self.cancel_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 触发取消（CLI 的 Ctrl-C handler 调用）。
    ///
    /// 取消是 graceful 的：当前 in-flight 的迭代被丢弃，已落盘的消息保留
    /// （C-13：Ctrl-C 不丢已生成消息），`run_turn` 返回 `TurnOutcome::Interrupted`。
    /// **仅当有 turn 正在运行时生效**：turn 间隙调用（如用户点取消但 turn
    /// 已结束）不取消任何 token，避免毒化下一轮（turn 结束时 token 会重建）。
    pub fn cancel(&self) {
        if !self.turn_active.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        self.cancel_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancel();
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
        // 防御修复（M-03，D-05）：历史中仍悬空的 tool_calls 补合成错误结果
        // （防"崩溃发生在 persist 之前"的极端情况）。磁盘消息不动，仅在 ctx 层
        // 修复——每次 resume 幂等重建，保证发给 provider 的历史对严格 provider 合法。
        let repaired = crate::model::repair_dangling_tool_calls(self.session.messages.clone());
        for msg in &repaired {
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
        // 详细日志：turn 总耗时（跨多次 LLM 调用 + 工具执行，见 observability.md §7.2）
        let turn_start = std::time::Instant::now();
        // 使用 `.instrument(span)` 而非 `span.enter()`——`Entered` guard 是 `!Send`，
        // 跨 await 持有会导致 future 非 `Send`（axum / `tokio::spawn` 需要 `Send`）。
        async move {
            // turn 开始：标记运行中（`cancel()` 仅在 turn 运行时生效，
            // 见字段注释）；guard drop 时复位（含 `?` 早退/panic 路径）。
            self.turn_active.store(true, std::sync::atomic::Ordering::SeqCst);
            let _turn_guard = TurnActiveGuard(&self.turn_active);

            // turn 开始：重置沙箱拒绝熔断器（单 turn 内有效，C-30）
            self.sandbox_breaker.reset();
            metrics::set_circuit_breaker("sandbox", "closed");

            // M-12（R-04）：turn 边界白名单配置热更新（ConfigWatcher 仅探测变更并
            // 广播 `Event::ConfigChanged`，具体应用在本方法执行，见 tech-stack.md §13）。
            self.reload_safe_config().await;

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

            // 配置快照：锁内短读后克隆（turn 循环内多处读取不再重复取锁）。
            // `max_iters`/`turn_timeout` 为 Copy 值，供外层 select! 超时分支复用。
            let config_snapshot = {
                let cfg = self.config.read().unwrap_or_else(std::sync::PoisonError::into_inner);
                cfg.clone()
            };
            let max_iters = config_snapshot.context.max_tool_iters;
            let turn_timeout = Duration::from_secs(config_snapshot.context.turn_timeout_sec);

            // 主循环封装为 future，由外层 select! 与 timeout/cancel 组合。
            // 使用 `async move` 避免 `async` 捕获 `&self` 的引用（产生 `&&self`），
            // 让 future 类型只借用 `&self`（单层引用），可与 SDK 的 `Box::pin` 配合。
            let turn_fut = async move {
                // 重复检测（M-08，R-03）：整轮签名用于硬停止（连续 ≥ 末级阈值），
                // 单工具指纹用于软提醒（逐级阈值 [3,5,8]）。
                let mut call_signatures: Vec<String> = Vec::new();
                // 指纹 → 本 turn 内连续出现次数
                let mut fingerprint_streaks: HashMap<String, u32> = HashMap::new();
                // 指纹 → 已提醒的最高阈值级（避免同一级重复提醒）
                let mut reminded_levels: HashMap<String, u32> = HashMap::new();

                for iter in 0..max_iters {
                    // 2. 构建请求（system + tools + 压缩后的历史）
                    let req = match self.ctx.build_chat_request(&self.tools, &config_snapshot).await {
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

                    // 5.1 重复检测（M-08，R-03）：先软提醒，后硬停止。
                    //     硬停止阈值 = 配置末级（非空）或默认 3（空数组 = 关闭软提醒）。
                    let thresholds = &config_snapshot.tools.repeat_guard_thresholds;
                    let hard_stop_count = thresholds.last().copied().unwrap_or(3);
                    let sig = repeat_guard::tool_calls_signature(&assistant_msg.tool_calls);

                    // 5.1a 单工具指纹软提醒：对每轮出现的指纹计数递增，未出现的清零
                    //     （"连续"语义：中间隔一轮未调用即视为中断）。命中中间级阈值
                    //     且未提醒过该级时，向上下文注入 system 级提醒（不替换工具输出、
                    //     不 return——模型可见历史不失真）。
                    let mut current_fingerprints: HashSet<String> = HashSet::new();
                    for c in &assistant_msg.tool_calls {
                        let fp = repeat_guard::tool_fingerprint(c);
                        current_fingerprints.insert(fp.clone());
                        let streak = fingerprint_streaks.entry(fp).or_insert(0);
                        *streak += 1;
                    }
                    for fp in fingerprint_streaks.keys().cloned().collect::<Vec<_>>() {
                        if !current_fingerprints.contains(&fp) {
                            fingerprint_streaks.remove(&fp);
                        }
                    }
                    if !thresholds.is_empty() {
                        let mut reminder_ctx = None;
                        for (fp, streak) in &fingerprint_streaks {
                            if *streak > 1 {
                                for lvl in thresholds.iter().copied() {
                                    if *streak == lvl && lvl < hard_stop_count {
                                        let already = reminded_levels.get(fp).copied().unwrap_or(0);
                                        if lvl > already {
                                            reminder_ctx = Some((fp.clone(), lvl));
                                            reminded_levels.insert(fp.clone(), lvl);
                                        }
                                    }
                                }
                            }
                        }
                        if let Some((fp, lvl)) = reminder_ctx {
                            let reminder = Message::system_text(format!(
                                "[系统提醒] 检测到重复工具调用 {fp} 已达 {lvl} 次。若陷入死循环，请改变策略或调用 'stop' 结束本轮。"
                            ));
                            self.ctx.append(reminder).await;
                            tracing::warn!(fingerprint = %fp, lvl, "injected soft repeat reminder");
                        }
                    }

                    // 5.1b 硬停止：整轮签名连续 ≥ 末级阈值 → 死循环，提前终止
                    //     （C-13 补充：max_tool_iters 之外的早期止损，避免无谓消耗）
                    call_signatures.push(sig);
                    if repeat_guard::is_repeating(&call_signatures, hard_stop_count) {
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

                    // 6. 执行工具调用前：记录 step 边界开始（M-06，定位压缩点/中断点）。
                    //    log-only 事件（C-05：不进 transcript），携带将执行的
                    //    tool_call_ids 供回放/审计定位。
                    let step_ids: Vec<String> = assistant_msg
                        .tool_calls
                        .iter()
                        .map(|c| c.id.clone())
                        .collect();
                    let event = Event::StepStarted {
                        iter,
                        tool_call_ids: step_ids,
                    };
                    self.persist_event(&event).await;
                    self.events.emit(event);

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

                    // 7.1 step 边界结束（M-06）：结果已全部回灌。
                    //     cancel/timeout 中断时此事件缺失，可据此定位中断点。
                    let event = Event::StepEnded { iter };
                    self.persist_event(&event).await;
                    self.events.emit(event);
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
            // 取消 future：锁内克隆当前 token（guard 即刻释放，无锁跨 await），
            // 克隆体绑定为具名局部变量，随 `cancelled()` future 存活。
            let cancel_token_now = self
                .cancel_token
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let cancel_fut = cancel_token_now.cancelled();
            let result: Result<TurnOutcome, RuntimeError> = tokio::select! {
                () = cancel_fut => {
                    tracing::info!("turn cancelled by user");
                    // M-03（D-05）：取消可能发生在工具执行中途，回填悬空 tool_calls
                    // 的合成错误结果，保证 resume 后历史对严格 provider 合法。
                    self.backfill_missing_tool_results().await;
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
                        timeout_sec = turn_timeout.as_secs(),
                        "turn timed out"
                    );
                    // M-03（D-05）：超时同样可能留下悬空 tool_calls，回填合成结果。
                    self.backfill_missing_tool_results().await;
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
            };
            // 详细日志：turn 结果摘要（重复工具循环/超时/取消均可从日志定位）
            match &result {
                Ok(o) => tracing::info!(
                    turn.elapsed_ms = u64::try_from(turn_start.elapsed().as_millis()).unwrap_or(u64::MAX),
                    turn.outcome = ?o,
                    "turn finished"
                ),
                Err(e) => tracing::warn!(
                    turn.elapsed_ms = u64::try_from(turn_start.elapsed().as_millis()).unwrap_or(u64::MAX),
                    error = %e,
                    "turn failed"
                ),
            }
            // 每次 `run_turn` 结束时重建取消 token：`CancellationToken` 一旦 cancel
            // 永久 cancelled，不重建则后续 turn 全部秒取消（会话被砖化，用户
            // 反馈"手动终止后无法再回复"）。重建对 CLI Ctrl-C 无影响——handler
            // 每轮经 `cancel_token()` 重新获取当前 token。
            *self
                .cancel_token
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = CancellationToken::new();
            result
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

    /// 为会话中"有 `tool_calls` 但缺 `tool_result`"的 assistant 消息补合成错误结果
    /// （M-03，D-05）。
    ///
    /// cancel/timeout 可能发生在工具执行中途：assistant 消息（含 `tool_calls`）已
    /// 落盘，但部分/全部 `tool_result` 未落盘，留下悬空调用——严格 provider（如
    /// Anthropic）要求每个 `tool_use` 必有 `tool_result`，resume 后请求会 400。
    /// 本方法对最后一个含 `tool_calls` 的 assistant 消息中**尚无结果**的调用补一条
    /// `is_error=true` 的合成 Tool 消息（落盘 + 入上下文 + 广播）。幂等：已齐的
    /// 调用跳过。
    ///
    /// 注意：事实源是 `storage` 而非 `self.session.messages`——后者仅含预加载
    /// 历史，运行期新增消息只写 storage/ctx，不更新 `session.messages`。
    async fn backfill_missing_tool_results(&self) {
        let Ok(msgs) = self.storage.load(&self.session.id).await else {
            return;
        };
        let Some(asst) = msgs
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
        else {
            return;
        };
        // 收集本 turn 已落盘（含 before-history）的 tool_result call_id
        let answered: std::collections::HashSet<&str> = msgs
            .iter()
            .filter_map(|m| {
                m.content.iter().find_map(|b| {
                    if let ContentBlock::ToolResult { call_id, .. } = b {
                        Some(call_id.as_str())
                    } else {
                        None
                    }
                })
            })
            .collect();
        for call in &asst.tool_calls {
            if answered.contains(call.id.as_str()) {
                continue;
            }
            let msg = Self::tool_result_message(
                call.id.clone(),
                ToolResult::err_text("[interrupted] 工具调用未执行（turn 被取消/超时）"),
            );
            if self.storage.append(&self.session.id, &msg).await.is_err() {
                // 回填失败不阻塞中断返回（已尽力；防御层 restore 时会再修）
                continue;
            }
            self.ctx.append(msg.clone()).await;
            let event = Event::MessageAppended(msg);
            self.persist_event(&event).await;
            self.events.emit(event);
        }
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

        // 详细日志（排查用）：请求元信息 + 响应统计。字段不含 input 原文与凭证（C-04）。
        tracing::info!(
            llm.provider = %provider_id,
            llm.model = %model_name,
            llm.message_count = req.messages.len(),
            phase = "request",
            "llm_call started"
        );

        let mut stream = self.provider.chat_stream(req).await?;
        let mut acc = DeltaAccumulator::new();
        self.events.emit(Event::TurnStreamingStarted);

        let mut text_chars = 0usize;
        let mut reasoning_chars = 0usize;
        let mut tool_calls = 0usize;
        while let Some(delta) = stream.next().await {
            let delta = delta?;
            match &delta {
                Delta::Text(s) => {
                    text_chars += s.chars().count();
                    self.events.emit(Event::Token(s.clone()));
                }
                Delta::Reasoning(s) => {
                    reasoning_chars += s.chars().count();
                    self.events.emit(Event::ReasoningDelta(s.clone()));
                }
                Delta::ToolCall(_) => tool_calls += 1,
                _ => {}
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
            // 详细日志：响应统计（模型调用过程可观测，见 observability.md §7.2）
            tracing::info!(
                llm.elapsed_ms = u64::try_from(timer.elapsed().as_millis()).unwrap_or(u64::MAX),
                llm.input_tokens = usage.input_tokens,
                llm.output_tokens = usage.output_tokens,
                llm.cache_read_tokens = usage.cache_read.unwrap_or(0),
                llm.text_chars = text_chars,
                llm.reasoning_chars = reasoning_chars,
                llm.tool_calls = tool_calls,
                llm.stop_reason = ?acc.stop_reason(),
                phase = "response",
                "llm_call finished"
            );
        } else {
            tracing::info!(
                llm.elapsed_ms = u64::try_from(timer.elapsed().as_millis()).unwrap_or(u64::MAX),
                llm.text_chars = text_chars,
                llm.reasoning_chars = reasoning_chars,
                llm.tool_calls = tool_calls,
                phase = "response",
                "llm_call finished (no usage)"
            );
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
    /// 分桶 + 权限 + 并行/串行调度的完整循环体，行数由分支复杂度决定
    /// （分桶、权限决策、Hook、沙箱拒绝、fallback 五层逻辑无法再安全拆分）。
    #[allow(clippy::too_many_lines)]
    async fn execute_tool_calls(
        &self,
        calls: &[ToolCall],
    ) -> Result<Vec<(ToolCallId, ToolResult)>, RuntimeError> {
        // 构造 ToolContext：注入沙箱驱动/策略/journal（M4，shell.run/fs 用）
        let ctx = ToolContext::new(self.workdir.read().await.clone(), self.session.id.clone())
            .with_sandbox(self.sandbox_driver.clone(), self.sandbox_policy.clone())
            .with_journal_opt(self.journal.clone());

        // 分桶：无副作用 → 并行；有副作用 → 串行（含权限检查）
        // S14：查不到的工具归入副作用桶（fail-closed）——走权限链后再由 dispatch
        // 报 NotFound，避免懒注册语义变化演变为免检绕过
        let (readonly, side_effect): (Vec<&ToolCall>, Vec<&ToolCall>) =
            calls.iter().partition(|c| {
                self.tools
                    .get(&c.name)
                    .is_some_and(|t| t.side_effect() == SideEffect::None)
            });

        let mut results: Vec<(ToolCallId, ToolResult)> = Vec::with_capacity(calls.len());

        // 无副作用：按 `tools.parallel_reads` 并发执行（0 = 串行，见 tech-stack.md §13）。
        // 克隆 `events`/`tools` 到闭包外，让 async 块只捕获 owned 数据，
        // 避免捕获 `&self` 导致 future 非 `'static`（无法被 SDK `tokio::spawn`）。
        //
        // **HRTB 修复**：`readonly.iter().map(|call| async move { ... })` 中 `call`
        // 是 `&&ToolCall`，闭包签名 `fn(&'a &'b ToolCall) -> impl Future + 'a` 不满足
        // `buffer_unordered` 要求的 HRTB（future 类型对任意 `'a` 必须相同）。把每个
        // future 装箱为 `Pin<Box<dyn Future + Send>>`，擦除生命周期参数，统一类型。
        let events = self.events.clone();
        let tools = self.tools.clone();
        let denial_detector = self.denial_detector.clone();
        let sandbox_breaker = self.sandbox_breaker.clone();
        let ro_futs: Vec<ToolFuture> = readonly
            .iter()
            .map(|call| {
                let ctx = ctx.clone();
                let call_id = call.id.clone();
                let tool_name = call.name.clone();
                let events = events.clone();
                let tools = tools.clone();
                let denial_detector = denial_detector.clone();
                let sandbox_breaker = sandbox_breaker.clone();
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
                    tracing::info!(
                        tool.name = %tool_name,
                        call_id = %call_id,
                        phase = "start",
                        "tool_call started"
                    );
                    events.emit(Event::ToolCallStarted {
                        call_id: call_id.clone(),
                        tool: tool_name.clone(),
                    });
                    let result = match tools.dispatch(&call, &ctx).await {
                        Ok(r) => r,
                        // 沙箱拒绝检测（M-09：只读桶与副作用路径共用，C-30 不可绕过）。
                        Err(e) => {
                            if let Some(r) = Self::build_denial_result(
                                denial_detector.as_ref(),
                                sandbox_breaker.as_ref(),
                                &tool_name,
                                &e,
                            ) {
                                r
                            } else {
                                // design.md §4.5：工具错误以 is_error=true 回灌 LLM
                                // 自我修正，不中止 turn（未知工具/参数不合法等模型可自行纠正）。
                                ToolResult::err_text(format!("tool error: {e}"))
                            }
                        }
                    };
                    events.emit(Event::ToolCallFinished {
                        call_id: call_id.clone(),
                        result: result.clone(),
                    });
                    tracing::info!(
                        tool.name = %tool_name,
                        call_id = %call_id,
                        tool.elapsed_ms = u64::try_from(tool_timer.elapsed().as_millis()).unwrap_or(u64::MAX),
                        tool.is_error = result.is_error,
                        tool.output_bytes = result.metadata.bytes,
                        phase = "finish",
                        "tool_call finished"
                    );
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
        let parallel_reads = {
            let cfg = self
                .config
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cfg.tools.parallel_reads
        };
        // M-12：`parallel_reads = 0` 时串行执行（顺序与 LLM 原始顺序一致，便于定位）。
        // 并行分支用 `buffer_unordered`，结果随后按原始顺序回填（见下方 sort）。
        if parallel_reads == 0 {
            for fut in ro_futs {
                results.push(fut.await?);
            }
        } else {
            let mut ro_stream =
                futures::stream::iter(ro_futs).buffer_unordered(parallel_reads as usize);
            while let Some(r) = ro_stream.next().await {
                results.push(r?);
            }
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
    #[allow(clippy::too_many_lines)] // 权限决策 + Hook + 执行 + 审计 + 详细日志，拆分反而降低因果链可读性
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
        tracing::info!(
            tool.name = %call.name,
            call_id = %call.id,
            phase = "start",
            "tool_call started (side effect)"
        );

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

        // 2. 构建 Hook 分发配置（S4：Hook 改写输入后此处会基于合并 verdict 重建）
        let dispatch_cfg = self.build_dispatch_config(&verdict);

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

        // 3.1 S4/C-01/C-21：Hook `modify_input` 修改了输入时，对**修改后**的输入重跑
        //     策略检查并与原 verdict 取严（Deny > Ask > Allow）——用户批准的是原始
        //     输入，Hook 改写后的输入必须重新过黑名单/路径策略，否则批准 A 执行 B。
        let input_modified = effective_call.input != call.input;
        let verdict = if input_modified {
            match self
                .policy
                .check(&call.name, &effective_call.input, &perm_ctx)
                .await
            {
                Ok(rechecked) => merge_verdicts_stricter(&verdict, rechecked),
                Err(e) => {
                    self.record_permission_audit(&call.name, &Decision::Deny(e.to_string()), None)
                        .await;
                    tracing::warn!(tool = %call.name, error = %e, "policy recheck on modified input failed");
                    return Ok((
                        call.id.clone(),
                        ToolResult::err_text(format!("permission error: {e}")),
                    ));
                }
            }
        } else {
            verdict
        };
        // 合并后 verdict 可能升级为 Deny：重建 dispatch_cfg/is_builtin_deny，
        // 保证 C-21（builtin Deny 不被 Hook Allow 覆盖）对改写后输入同样成立
        let dispatch_cfg = if input_modified {
            self.build_dispatch_config(&verdict)
        } else {
            dispatch_cfg
        };
        let is_builtin_deny = matches!(verdict, Verdict::Deny(_));

        // PreToolUse 直出决策与合并 verdict 冲突时取严（Hook Allow 不能越过重查 Deny）
        let pre_decision = match (&pre_decision, &verdict) {
            (Some(Decision::Allow), Verdict::Deny(reason)) => Some(Decision::Deny(format!(
                "输入被 Hook 修改后未通过策略复查: {reason}"
            ))),
            _ => pre_decision,
        };

        // 4. 解析为最终决策（PreToolUse 直出 / Verdict Allow|Deny / Ask→PermissionRequest Hook→prompter）。
        //    Ask 场景传 effective_call——弹窗展示的是实际将执行的（可能被 Hook 改写的）输入。
        let (decision, prompt_id) = if let Some(d) = pre_decision {
            (d, None)
        } else {
            self.resolve_decision(
                &verdict,
                if input_modified {
                    &effective_call
                } else {
                    call
                },
                side_effect,
                &dispatch_cfg,
                is_builtin_deny,
            )
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
        // 详细日志：副作用工具执行结果（含权限决策）
        match &result {
            Ok((_, r)) => {
                tracing::info!(
                    tool.name = %call.name,
                    call_id = %call.id,
                    tool.elapsed_ms = u64::try_from(tool_timer.elapsed().as_millis()).unwrap_or(u64::MAX),
                    tool.is_error = r.is_error,
                    tool.output_bytes = r.metadata.bytes,
                    permission.verdict = verdict_str,
                    phase = "finish",
                    "tool_call finished (side effect)"
                );
            }
            Err(e) => {
                tracing::warn!(
                    tool.name = %call.name,
                    call_id = %call.id,
                    tool.elapsed_ms = u64::try_from(tool_timer.elapsed().as_millis()).unwrap_or(u64::MAX),
                    error = %e,
                    permission.verdict = verdict_str,
                    phase = "finish",
                    "tool_call failed"
                );
            }
        }
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
        // 短临界区读取两个 Copy 字段（guard 不跨 await，`&self` 同步 fn 亦可用）
        let (on_error, default_timeout_sec) = {
            let cfg = self
                .config
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (cfg.hooks.on_hook_error, cfg.hooks.default_timeout_sec)
        };
        let builtin_deny = match verdict {
            Verdict::Deny(msg) => Some(msg.clone()),
            _ => None,
        };
        DispatchConfig {
            on_error,
            timeout: Duration::from_secs(default_timeout_sec),
            builtin_deny,
        }
    }

    /// M-12（R-04）：turn 边界白名单配置热更新。
    ///
    /// [`ConfigWatcher`]（`paths::config_path()` 监听）仅广播 `Event::ConfigChanged`，
    /// 具体应用由本方法在 `run_turn` 开头执行（`tech-stack.md` §13 决策记录）：
    /// - **不做全量热重载**：C-29 压缩熔断状态机与 provider 重建依赖构造时配置，
    ///   热换不安全；白名单外的字段变更仅 warn 提示重启。
    /// - 白名单字段（`provider.model`/`context.turn_timeout_sec`/
    ///   `tools.parallel_reads`）**仅当文件中显式存在**该 key 时应用
    ///   （`toml::Value` presence 判断），避免 serde default（文件缺字段补默认值）
    ///   覆盖 CLI/env 传入的覆盖值。
    /// - 文件缺失/解析失败时静默保留当前配置（best-effort，与 `load_config` 的
    ///   last-known-good 机制正交：此处不写 LKG）。
    async fn reload_safe_config(&self) {
        let Some(path) = &self.config_path else {
            return;
        };
        let Ok(raw) = tokio::fs::read_to_string(path).await else {
            return; // 无配置文件：CLI 未配置时的正常路径，静默跳过
        };
        let fresh: RuntimeConfig = match toml::from_str(&raw) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    path = %path,
                    error = %e,
                    "config reload: parse failed, keeping current config"
                );
                return;
            }
        };
        let file_val: toml::Value = match toml::from_str(&raw) {
            Ok(v) => v,
            Err(_) => return, // 与上方同源解析，理论不可达
        };

        // 非白名单签名（白名单字段 + revision 剥除后）：
        // 先读当前运行期签名，供下方「变更提示重启」比对。
        let sig_current = {
            let cfg = self
                .config
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Self::config_non_whitelist_sig(&cfg)
        };
        let sig_fresh = Self::config_non_whitelist_sig(&fresh);

        // 应用白名单字段（写锁临界区仅字段赋值，无 await）。
        let mut cfg = self
            .config
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut applied: Vec<&'static str> = Vec::new();
        if Self::toml_has(&file_val, &["provider", "model"]) {
            cfg.provider.model.clone_from(&fresh.provider.model);
            applied.push("provider.model");
        }
        if Self::toml_has(&file_val, &["context", "turn_timeout_sec"]) {
            cfg.context.turn_timeout_sec = fresh.context.turn_timeout_sec;
            applied.push("context.turn_timeout_sec");
        }
        if Self::toml_has(&file_val, &["tools", "parallel_reads"]) {
            cfg.tools.parallel_reads = fresh.tools.parallel_reads;
            applied.push("tools.parallel_reads");
        }
        drop(cfg);

        if !applied.is_empty() {
            tracing::info!(
                path = %path,
                applied = ?applied,
                "config reload: applied whitelist fields at turn boundary"
            );
        }

        // 非白名单变更检测：与上次文件版本比对（首次加载 `last == None` 不告警）。
        let mut last = self
            .last_non_whitelist_sig
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let needs_warn = last.is_some() && sig_current != sig_fresh && *last != Some(sig_fresh);
        *last = Some(sig_fresh);
        if needs_warn {
            tracing::warn!(
                path = %path,
                "config reload: detected non-whitelist changes (restart required to take effect); whitelist fields applied"
            );
        }
    }

    /// 在 `toml::Value` 中按路径查找 key 是否存在（M-12 白名单 presence 判断）。
    fn toml_has(v: &toml::Value, path: &[&str]) -> bool {
        let mut cur = v;
        for key in path {
            match cur.get(key) {
                Some(next) => cur = next,
                None => return false,
            }
        }
        true
    }

    /// 配置的「非白名单签名」：序列化 JSON 后剔除白名单路径与 `revision`，
    /// 对剩余字符串做 hash。用于检测「需重启生效」的配置变更（M-12）。
    fn config_non_whitelist_sig(cfg: &RuntimeConfig) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut v = serde_json::to_value(cfg).unwrap_or_default();
        if let Some(o) = v.as_object_mut() {
            o.remove("revision");
        }
        if let Some(p) = v.pointer_mut("/provider")
            && let Some(o) = p.as_object_mut()
        {
            o.remove("model");
        }
        if let Some(c) = v.pointer_mut("/context")
            && let Some(o) = c.as_object_mut()
        {
            o.remove("turn_timeout_sec");
        }
        if let Some(t) = v.pointer_mut("/tools")
            && let Some(o) = t.as_object_mut()
        {
            o.remove("parallel_reads");
        }
        let mut h = DefaultHasher::new();
        v.to_string().hash(&mut h);
        h.finish()
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
                // 沙箱初始化失败（apply/post_spawn，如 Windows Job Object 恢复线程
                // 竞态）：询问用户是否在沙箱外重试一次（C-22 用户显式选定）。
                if let Some(fallback_ctx) =
                    self.maybe_sandbox_fallback(original_call, &e, ctx).await
                {
                    // 沙箱外重试：仅重试一次，不再二次询问（避免询问循环）
                    match self.tools.dispatch(effective_call, &fallback_ctx).await {
                        Ok(r) => r,
                        Err(e2) => {
                            // PostToolUseFailure Hook（重试仍失败，非 denial 错误）
                            self.run_post_failure_hook(effective_call, side_effect, &e2)
                                .await;
                            ToolResult::err_text(format!("tool error: {e2}"))
                        }
                    }
                } else {
                    // PostToolUseFailure Hook（非 denial 错误）
                    self.run_post_failure_hook(effective_call, side_effect, &e)
                        .await;
                    // design.md §4.5：工具错误以 is_error=true 回灌 LLM 自我修正，不中止 turn。
                    ToolResult::err_text(format!("tool error: {e}"))
                }
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

    /// 判断工具错误是否为"沙箱初始化失败"（`apply`/`post_spawn`）。
    ///
    /// 与沙箱拒绝（EPERM/EACCES，`handle_sandbox_denial`）区分：初始化失败是沙箱
    /// 机制本身故障（如 Windows Job Object 恢复线程快照竞态），不是被沙箱拦下的
    /// 行为，可通过沙箱外重试规避。
    fn is_sandbox_setup_failure(error: &crate::model::ToolError) -> bool {
        match error {
            crate::model::ToolError::Exec(msg) => {
                msg.starts_with("sandbox apply failed")
                    || msg.starts_with("sandbox post_spawn failed")
            }
            _ => false,
        }
    }

    /// 沙箱初始化失败时询问用户是否在沙箱外重试（C-22：用户显式选定 + High risk 警告）。
    ///
    /// 允许 → 返回以 `DangerFullAccess` 策略构造的重试上下文（同一 driver，该策略下
    /// `apply`/`post_spawn` 均为 no-op）；拒绝或非沙箱初始化错误 → `None`（调用方按原
    /// 错误处理）。询问与决策经 `PermissionRequested`/`PermissionResolved` 事件广播
    /// （前端弹窗复用 W-03 权限链路）并落 `audit.log`（AGENTS.md §5.5）。
    async fn maybe_sandbox_fallback(
        &self,
        call: &ToolCall,
        error: &crate::model::ToolError,
        ctx: &ToolContext,
    ) -> Option<ToolContext> {
        if !Self::is_sandbox_setup_failure(error) {
            return None;
        }
        tracing::warn!(
            tool = %call.name,
            call_id = %call.id,
            error = %error,
            "sandbox setup failed, prompting user for out-of-sandbox retry"
        );
        let prompt = PermissionPrompt {
            id: format!("sbx-{}", uuid::Uuid::new_v4()),
            tool: call.name.clone(),
            summary: format!(
                "OS 沙箱初始化失败（{error}）。\n是否在沙箱外运行此命令？\n\
                 ⚠ 沙箱外运行 = 放弃 OS 级隔离（C-22），仅限受信环境！"
            ),
            risk: crate::policy::Risk::High,
            options: vec![
                crate::policy::PromptOption::AllowOnce,
                crate::policy::PromptOption::DenyOnce,
            ],
        };
        let prompt_id = prompt.id.clone();
        self.events.emit(Event::PermissionRequested {
            id: prompt.id.clone(),
            tool: prompt.tool.clone(),
            summary: prompt.summary.clone(),
            risk: prompt.risk,
        });
        let decision = self.prompter.prompt(prompt.clone()).await;
        let event = Event::PermissionResolved {
            id: prompt_id.clone(),
            decision: decision.clone(),
        };
        self.persist_event(&event).await;
        self.events.emit(event);
        // 审计：沙箱外回退决策必须落盘（与普通权限决策同等对待，AGENTS.md §5.5）
        self.record_permission_audit(
            &format!("{} sandbox-fallback", call.name),
            &decision,
            Some(prompt_id),
        )
        .await;
        match decision {
            Decision::Allow => {
                let mut fallback = ctx.clone();
                fallback.sandbox_policy = Some(SandboxPolicy::DangerFullAccess);
                Some(fallback)
            }
            Decision::Deny(_) => None,
        }
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
        // 短临界区读取（guard 在首个 await 前释放）+ 快速跳过
        let (on_error, default_timeout_sec, has_post_tool_use) = {
            let cfg = self
                .config
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                cfg.hooks.on_hook_error,
                cfg.hooks.default_timeout_sec,
                !cfg.hooks.post_tool_use.is_empty(),
            )
        };
        if !has_post_tool_use {
            return; // 无 PostToolUse Hook，快速跳过
        }
        let dispatch_cfg = DispatchConfig {
            on_error,
            timeout: Duration::from_secs(default_timeout_sec),
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
        // 短临界区读取（guard 在首个 await 前释放）+ 快速跳过
        let (on_error, default_timeout_sec, has_post_failure_hook) = {
            let cfg = self
                .config
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                cfg.hooks.on_hook_error,
                cfg.hooks.default_timeout_sec,
                !cfg.hooks.post_tool_use_failure.is_empty(),
            )
        };
        if !has_post_failure_hook {
            return; // 无 PostToolUseFailure Hook，快速跳过
        }
        let dispatch_cfg = DispatchConfig {
            on_error,
            timeout: Duration::from_secs(default_timeout_sec),
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
        Self::build_denial_result(
            self.denial_detector.as_ref(),
            self.sandbox_breaker.as_ref(),
            tool,
            error,
        )
        .map(|r| (call_id.clone(), r))
    }

    /// 沙箱拒绝检测（M-09 起为静态辅助：只读并行桶与副作用串行路径共用）。
    ///
    /// 检测工具错误是否为沙箱拒绝（EPERM/EACCES/landlock 等）。若是：
    /// - 更新熔断器计数；
    /// - 软熔断（≥3 次）：附加方向提醒返回；
    /// - 硬熔断（≥5 次）：返回带总结的错误；
    /// - 未熔断：返回带 denial 标识的错误，提示 LLM/用户。
    ///
    /// 返回 `Some(ToolResult)` 表示已识别为 denial 并生成回灌结果；
    /// 返回 `None` 表示非 denial，调用方原样传播错误。
    fn build_denial_result(
        detector: &dyn crate::sandbox::SandboxDenialDetector,
        breaker: &dyn crate::sandbox::SandboxDenialTracker,
        tool: &str,
        error: &crate::model::ToolError,
    ) -> Option<ToolResult> {
        let error_text = error.to_string();
        let m = detector.detect(tool, &error_text)?;
        tracing::warn!(
            tool = %m.tool,
            reason = m.signature.reason,
            platform = m.signature.platform,
            "sandbox denial detected"
        );
        let state = breaker.record_denial();
        Some(match state {
            BreakerState::HardTripped => {
                let summary = crate::sandbox::hard_trip_summary(breaker.count());
                tracing::warn!(
                    count = breaker.count(),
                    "sandbox circuit breaker hard-tripped"
                );
                metrics::set_circuit_breaker("sandbox", "hard_tripped");
                metrics::record_error("sandbox");
                ToolResult {
                    content: crate::model::ToolContent::Text(format!(
                        "{summary}\n原始错误：{error_text}"
                    )),
                    is_error: true,
                    metadata: crate::model::ToolResultMeta {
                        sandbox_denied: Some(crate::model::SandboxDenyInfo {
                            kind: m.kind.clone(),
                            detail: error_text.clone(),
                        }),
                        ..Default::default()
                    },
                }
            }
            BreakerState::SoftTripped => {
                let reminder = crate::sandbox::soft_trip_reminder(breaker.count());
                tracing::warn!(
                    count = breaker.count(),
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
                    metadata: crate::model::ToolResultMeta {
                        sandbox_denied: Some(crate::model::SandboxDenyInfo {
                            kind: m.kind.clone(),
                            detail: error_text.clone(),
                        }),
                        ..Default::default()
                    },
                }
            }
            BreakerState::Closed => {
                metrics::record_error("sandbox");
                let mut result = ToolResult::err_text(format!(
                    "sandbox denied ({reason}): {error_text}\n\
                     提示：可切换更宽松的沙箱预设（如 --sandbox workspace-write）重试",
                    reason = m.signature.reason
                ));
                result.metadata.sandbox_denied = Some(crate::model::SandboxDenyInfo {
                    kind: m.kind,
                    detail: error_text,
                });
                result
            }
        })
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
            metadata: result.metadata,
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
                compressed_range: None,
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

// 静态断言：`Runtime` 是 `Send + Sync`（多线程 runtime / axum 需要）。
// 通过 `const` 绑定强制 trait bound 检查，无需运行时调用。
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Runtime>();
};

/// S4：合并两个 Verdict 取较严者（Deny > Ask > Allow）。
///
/// 用途：Hook `modify_input` 改写工具入参后，原始输入与修改后输入的策略判定
/// 需同时满足（任一 Deny 即 Deny；任一要求 Ask 则升级为 Ask）。
fn merge_verdicts_stricter(a: &Verdict, b: Verdict) -> Verdict {
    use Verdict::{Allow, Ask, Deny};
    fn rank(v: &Verdict) -> u8 {
        match v {
            Allow => 0,
            Ask(_) => 1,
            Deny(_) => 2,
        }
    }
    if rank(a) >= rank(&b) {
        match a {
            Deny(_) | Ask(_) => a.clone(),
            Allow => b,
        }
    } else {
        b
    }
}
