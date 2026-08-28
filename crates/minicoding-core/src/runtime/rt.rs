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
use crate::hooks::HookRegistry;
use crate::journal::Journal;
use crate::memory::SessionSummarizer;
use crate::metrics;
use crate::model::{
    ContentBlock, Message, Role, RuntimeError, Session, SideEffect, StopReason, ToolCall,
    ToolCallId, ToolResult, TurnOutcome, UserInput,
};
use crate::otel::span_name;
use crate::policy::{PermissionPolicy, PermissionPrompter, PlanModeController, PlanModeSnapshot};
use crate::provider::{BoxFuture, ChatRequest, Delta, LlmProvider};
use crate::runtime::accumulator::DeltaAccumulator;
use crate::runtime::{Event, EventBus};
use crate::sandbox::{SandboxDriver, SandboxPolicy};
use crate::storage::{AuditSink, EventStore, SnapshotStore, Storage};
use crate::tool::{ToolContext, ToolRegistry};
use camino::Utf8PathBuf;
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
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
    /// 显式覆盖字段集合（R3 RT-5：替代原"热更新基线"）。
    ///
    /// CLI flag/env 覆盖（builder 登记）与 `/model` 运行期切换（`set_model`
    /// 登记）的字段名在此集合内——turn 边界热更新永不回退这些字段，
    /// 维持"CLI 参数 > 环境变量 > config.toml > 默认"优先级。std Mutex：
    /// 临界区仅集合读写、不跨 await。
    pub(crate) explicit_overrides: std::sync::Mutex<HashSet<&'static str>>,
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
    /// 当前 turn 内 LLM 迭代下标（0-based，每次循环迭代覆写）。
    ///
    /// CORE-8（2026-08-25 R2 审查）：字段文档此前声称 "`run_turn` 入口自增的会话
    /// 轮次号"与实现相反——实际存的是 `for iter in 0..max_iters` 的迭代值，
    /// `HookInput.turn` / `PermissionContext.turn` 拿到的是**本 turn 第几次工具
    /// 循环**而非第几个用户轮次。按实现如实修正语义描述；跨 turn 轮次号无
    /// 消费方，如未来需要应另立 `session_turn` 计数器而非复用此字段。
    pub(crate) current_turn: std::sync::atomic::AtomicU32,
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
    /// Hook 注入上下文缓冲（2026-08-23 审查遗留#6 接线）：PreToolUse Hook 的
    /// `inject_context` 在工具调用间隙**不能**直接 append（会插入
    /// `assistant(tool_calls)` 与 `tool_result` 之间，破坏配对）——先缓冲，
    /// 下一请求构建时包裹 `<hook_context>` 边界并入 system 段头部。
    pub(crate) pending_hook_contexts: std::sync::Mutex<Vec<String>>,
    /// AllowAlways/DenyAlways 持久化存储（遗留#3；`None` 时 Always 决策折叠
    /// 但不落盘）。sdk 默认注入 `~/.minicoding/policy.toml`。
    pub(crate) policy_persist: Option<Arc<crate::policy::PolicyPersist>>,
    /// 会话级 Allow 缓存（2026-08-25 审查 S-1）。
    ///
    /// 无路径输入的工具（`shell.run`/`web.fetch` 等）用户选择 `AllowAlways` 时
    /// **只做会话级放行**，不再落盘为跨会话/跨项目的工具级全局规则（一次按键
    /// 即永久放行的放大效应，见审查报告 §6.1-S1）。带路径工具仍按目录粒度
    /// 持久化。std Mutex：临界区仅集合读写、不跨 await。
    pub(crate) session_allows: std::sync::Mutex<HashSet<String>>,
    /// asyncRewake 调度器（遗留#6 全量接线；默认 Noop 拒绝 spawn）。
    pub(crate) rewake: Arc<dyn crate::hooks::AsyncRewakeScheduler>,
    /// 沙箱拒绝权威标记防伪 nonce（SEC-6，2026-08-27 R5 审查）。
    ///
    /// 每次 `Runtime` 构造随机生成（UUID v4）。`build_denial_result` 合成权威
    /// 标记时嵌入：`\x01MINICODING_DENIED_ERRNO=<errno>:<nonce>\x02`——子进程
    /// stderr 可打印裸 `\x01MINICODING_DENIED_ERRNO=` 前缀（此前检测器对含前缀
    /// 文本直接置 authoritative，恶意命令可伪造标记触发 C-30 熔断 DoS），但
    /// **不知道本 nonce**，无法伪造完整标记；权威判定由 `build_denial_result`
    /// 验证"Runtime 自己追加的标记行"决定，不再信任检测器的裸前缀匹配。
    pub(crate) denial_nonce: String,
    /// `SessionStart` Hook 是否已派发（每会话恰一次）。
    pub(crate) session_start_done: std::sync::atomic::AtomicBool,
    /// 单 turn 门闩（2026-08-23 审查 §4-P2）：`run_turn` 入口 `try_lock`，
    /// 并发第二个 turn 返回 `RuntimeError::TurnInProgress`。tokio Mutex（无锁
    /// 争用时零开销）；guard 持有至 turn 结束（含取消/超时路径，随 future drop 释放）。
    pub(crate) turn_gate: TokioMutex<()>,
}

impl Runtime {
    /// 返回当前会话。
    #[must_use]
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// 当前模型名（`/model` 查看用）。
    #[must_use]
    pub fn model(&self) -> String {
        self.config
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .provider
            .model
            .clone()
    }

    /// 运行期切换模型（遗留：`/model <name>` 命令）。
    ///
    /// 写运行期配置锁，下一 `build_chat_request` 即生效（M-12 起 model 取自
    /// `req.params.model`）。会话级生效，不回写 config.toml；同时刷新热更新
    /// 基线，避免 turn 边界被文件值回退覆盖。
    pub fn set_model(&self, model: &str) {
        // R3 RT-5：改运行期配置并登记显式覆盖——此后 turn 边界热更新不再用
        // config.toml 回退模型（原实现同步"基线"方向恰好相反，同步后守卫
        // 放行文件值回退，`/model` 选择只存活到当前 turn 结束）。
        let mut cfg = self
            .config
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cfg.provider.model.clone_from(&model.to_string());
        drop(cfg);
        if let Ok(mut o) = self.explicit_overrides.lock() {
            o.insert("provider.model");
        }
        tracing::info!(model, "runtime model switched");
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

    /// 返回沙箱拒绝标记 nonce（供 `build_denial_result` 防伪，SEC-6）。
    #[must_use]
    pub fn denial_nonce(&self) -> &str {
        &self.denial_nonce
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
        let repaired = super::repair::repair_dangling_tool_calls(self.session.messages.clone());
        for msg in &repaired {
            self.ctx.append(msg.clone()).await;
        }
        if count > 0 {
            tracing::info!(session = %self.session.id, restored = count, "history restored");
        }
        Ok(())
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

    /// 生成会话摘要并落盘 `index.json`（T-M3-6）。
    ///
    /// 在会话退出前调用：从 `ContextManager` 快照消息 → 调注入的
    /// `SessionSummarizer` 生成摘要（降级链：主 provider → 备用 → 启发式兜底，
    /// C-29 永不失败）→ `Storage::update_summary` 落盘。
    ///
    /// CTX-5（2026-08-27 R5 审查，如实记录）：摘要的**消费方**仅会话列表展示
    /// （`Storage::list_sessions` 的 `summary` 字段，server 侧 `session_mgr` 用）——
    /// "跨会话恢复"（新会话 system 段注入 `session_context` 块，见 rules.md §5
    /// `[Context]`）的注入半边**未实现**。注入涉及工作目录作用域设计决策
    /// （`SessionListItem` 无 workdir 字段，无法按项目过滤摘要来源），
    /// 误注入会把无关项目上下文带入新会话——修复列为设计决策项，不在此
    /// 半实现（避免行为惊吓）。
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
    /// - 重复检测：整轮签名连续 ≥ 末级阈值轮（默认配置 [3,5,8] 下为 8 轮；
    ///   空阈值数组回退 3 轮）相同 → 判定死循环提前终止（R3 RT-6 口径修正）
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
            // 单 turn 不变量（2026-08-23 审查 §4-P2）：同一 Runtime 任意时刻至多
            // 一个 turn 在执行——event_seq/durable_seq/turn_active 等状态均非并发
            // 安全设计。此前安全性完全依赖 server 的 session 级 turn_lock 或 REPL
            // 串行调用，SDK 直连两次 spawn 即可交错破坏。`try_lock` 失败立即报错
            // 而非排队：并发发起第二个 turn 是编程错误，静默等待会掩盖竞态。
            let _turn_gate = self
                .turn_gate
                .try_lock()
                .map_err(|_| RuntimeError::TurnInProgress)?;

            // turn 开始：标记运行中（`cancel()` 仅在 turn 运行时生效，
            // 见字段注释）；guard drop 时复位（含 `?` 早退/panic 路径）。
            self.turn_active.store(true, std::sync::atomic::Ordering::SeqCst);
            let turn_guard = TurnActiveGuard(&self.turn_active);

            // turn 开始：重置沙箱拒绝熔断器（单 turn 内有效，C-30）
            self.sandbox_breaker.reset();
            metrics::set_circuit_breaker("sandbox", "closed");

            // M-12（R-04）：turn 边界白名单配置热更新（ConfigWatcher 仅探测变更并
            // 广播 `Event::ConfigChanged`，具体应用在本方法执行，见 tech-stack.md §13）。
            self.reload_safe_config().await;

            // 遗留#6 全量接线：SessionStart（每会话恰一次）
            if !self
                .session_start_done
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                self.run_lifecycle_hook(crate::hooks::HookEvent::SessionStart, serde_json::Value::Null)
                    .await;
            }

            // 遗留#6 全量接线：UserPromptSubmit（携带 prompt 文本）
            self.run_lifecycle_hook(
                crate::hooks::HookEvent::UserPromptSubmit,
                serde_json::json!({ "prompt": user_input.text }),
            )
            .await;

            // 遗留#6 全量接线：asyncRewake 完成结果注入下一请求 system 头部
            for r in self.rewake.poll_completed().await {
                self.pending_hook_contexts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(format!(
                        "<async_rewake hook=\"{}\">\n{}\n</async_rewake>",
                        r.hook_name, r.context
                    ));
            }

            // 1. 构造用户消息并入库
            // CORE-11（2026-08-25 R2 审查）：`attachments`/`context_hint` 字段
            // 尚无消费方（图片/文件附件管道未实现）——显式 warn 拒绝静默丢失，
            // 提醒调用方当前不生效；实现接线后移除本告警。
            if !user_input.attachments.is_empty() {
                tracing::warn!(
                    attachments = user_input.attachments.len(),
                    context_hint = ?user_input.context_hint,
                    "UserInput.attachments/context_hint are not yet consumed and will be dropped"
                );
            }
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
                    self.current_turn
                        .store(iter, std::sync::atomic::Ordering::Relaxed);
                    // 2. 构建请求（system + tools + 压缩后的历史）
                    let mut req = match self.ctx.build_chat_request(&self.tools, &config_snapshot).await {
                        Ok(r) => r,
                        Err(e) => {
                            metrics::record_error("context");
                            return Err(e);
                        }
                    };

                    // 3. 流式调用 LLM
                    // Hook 注入上下文消费（遗留#6）：包裹 `<hook_context>` 边界
                    // （声明非指令，C-05 精神）并入 system 头部；不落盘不进历史。
                    {
                        let drained: Vec<String> = self
                            .pending_hook_contexts
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .drain(..)
                            .collect();
                        for c in drained.iter().rev() {
                            req.system.insert_str(
                                0,
                                &format!("<hook_context>\n{c}\n</hook_context>\n"),
                            );
                        }
                    }

                    // 配对完整性最后防线（§8-P0-2）：压缩管道丢弃边界可能切断
                    // tool_use/tool_result 配对 → 严格 provider 400 死局。幂等
                    // 纯函数，仅作用于本次请求副本，不回写 storage/ctx。
                    req.messages = super::repair::repair_request_messages(req.messages);
                    // PT4-3（2026-08-28 R8 审查）：`ContextLength` 紧急压缩联动——
                    // LlmError::ContextLength（真实 400 超窗）此前只回灌 LLM 自修正
                    // （compress 永不触发）。改为：首次命中触发一次 `ctx.compress()`
                    // + 重建请求 + 重试一次；再失败才回灌（防循环）。
                    let mut emergency_compressed = false;
                    let (assistant_msg, llm_stop_reason, llm_usage) = loop {
                        match self.stream_llm(req).await {
                            Ok(msg) => break msg,
                            Err(e)
                                if !emergency_compressed
                                    && matches!(&e, crate::model::LlmError::ContextLength(_)) =>
                            {
                                tracing::warn!(
                                    error = %e,
                                    "LLM 400 上下文超长：紧急压缩后重试一次"
                                );
                                emergency_compressed = true;
                                if self.ctx.force_compress().await.is_err() {
                                    metrics::record_error("context");
                                    return Ok(TurnOutcome::Failed(e.into()));
                                }
                                match self
                                    .ctx
                                    .build_chat_request(&self.tools, &config_snapshot)
                                    .await
                                {
                                    Ok(new_req) => req = new_req,
                                    Err(ce) => {
                                        metrics::record_error("context");
                                        return Ok(TurnOutcome::Failed(ce));
                                    }
                                }
                                // 与正常路径一致：Hook 注入上下文并入 system 头部
                                {
                                    let drained: Vec<String> = self
                                        .pending_hook_contexts
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .drain(..)
                                        .collect();
                                    for c in drained.iter().rev() {
                                        req.system.insert_str(
                                            0,
                                            &format!("<hook_context>\n{c}\n</hook_context>\n"),
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                metrics::record_error("llm");
                                return Ok(TurnOutcome::Failed(e.into()));
                            }
                        }
                    };

                    // 4. 落盘 assistant 消息
                    if let Err(e) = self.storage.append(&self.session.id, &assistant_msg).await {
                        metrics::record_error("storage");
                        return Err(RuntimeError::Storage(e));
                    }
                    self.ctx.append(assistant_msg.clone()).await;
                    // count_tokens 校准（遗留#2）：真实 input_tokens 回灌本地估算
                    if let Some(u) = llm_usage {
                        self.ctx.calibrate(u.input_tokens);
                    }
                    let event = Event::MessageAppended(assistant_msg.clone());
                    self.persist_event(&event).await;
                    self.events.emit(event);

                    // 5. 无工具调用 → 终止。stop_reason 优先采用 provider 报告值
                    //    （如 MaxTokens 截断），缺省回退 EndTurn——前端/审计不再把
                    //    截断误读为正常结束（2026-08-23 审查 §4-P1）。
                    if assistant_msg.tool_calls.is_empty() {
                        let event = Event::TurnEnd {
                            stop_reason: llm_stop_reason.unwrap_or(StopReason::EndTurn),
                        };
                        self.persist_event(&event).await;
                        self.events.emit(event);
                        return Ok(TurnOutcome::Finished(assistant_msg));
                    }

                    // 5.1 重复检测（M-08，R-03）：先软提醒，后硬停止。
                    //     硬停止阈值 = thresholds 末级（默认配置 [3,5,8] 下为
                    //     **8**；空数组时回退 3——RT-6 口径修正，与 config 注释
                    //     一致）。中间各级仅软提醒。
                    let thresholds = &config_snapshot.tools.repeat_guard_thresholds;
                    let hard_stop_count = thresholds.last().copied().unwrap_or(3);
                    let sig = repeat_guard::tool_calls_signature(&assistant_msg.tool_calls);

                    // 5.1a 单工具指纹软提醒：**每轮**出现的指纹计数 +1（一轮内
                    //     多个相同调用只算一轮，RT-6 轮次语义），未出现的清零
                    //     （"连续"语义：中间隔一轮未调用即视为中断）。命中中间级
                    //     阈值且未提醒过该级时，缓冲提醒并入下一请求 system
                    //     头部（不替换工具输出、不 return——模型可见历史不失真）。
                    let mut current_fingerprints: HashSet<String> = HashSet::new();
                    for c in &assistant_msg.tool_calls {
                        current_fingerprints.insert(repeat_guard::tool_fingerprint(c));
                    }
                    {
                        for fp in &current_fingerprints {
                            *fingerprint_streaks.entry(fp.clone()).or_insert(0) += 1;
                        }
                        // 清理本轮未出现的指纹
                        fingerprint_streaks
                            .retain(|fp, _| current_fingerprints.contains(fp));
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
                            // RT-2（2026-08-26 R3 审查）：此前以 System 消息直接
                            // `ctx.append`——插入 `assistant(tool_calls)` 与
                            // `tool_result` **之间**，破坏严格 provider 配对：
                            // OpenAI 要求 role=tool 紧跟 tool_calls；Anthropic
                            // 要求 tool_result 位于紧随 tool_use 的 user 消息内
                            // ——两家均持续 400。且压缩管道永不丢弃 System 消息
                            // （rolling/hard_truncate/summarize 均跳过），污染
                            // 不可自愈、resume 后仍在。改走 `pending_hook_contexts`
                            // 同款缓冲（与上方注释的 hook_context 机制一致）：
                            // 下一请求构建时包裹边界并入 system 头部，不进消息
                            // 历史。
                            self.pending_hook_contexts
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push(format!(
                                    "<repeat_warning level=\"{lvl}\">\n\
                                     [系统提醒] 检测到重复工具调用 {fp} 已达 {lvl} 次。\
                                     若陷入死循环，请改变策略或调用 'stop' 结束本轮。\n\
                                     </repeat_warning>"
                                ));
                            tracing::warn!(fingerprint = %fp, lvl, "buffered soft repeat reminder");
                        }
                    }

                    // 5.1b 硬停止：整轮签名连续 ≥ 末级阈值 → 死循环，提前终止
                    //     （C-13 补充：max_tool_iters 之外的早期止损，避免无谓消耗）
                    call_signatures.push(sig);
                    if repeat_guard::is_repeating(&call_signatures, hard_stop_count) {
                        tracing::warn!("turn terminated: repeated tool calls detected");
                        // 终态消息先落盘再广播（A-P2，见 append_terminal_notice）
                        self.append_terminal_notice("[检测到重复工具调用，已终止以避免死循环]")
                            .await;
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

                    // 7.2 C-30 硬熔断强制 TurnEnd（CORE-1，2026-08-25 R2 审查）：
                    //     沙箱拒绝熔断器进入 HardTripped 后本轮立即终止——此前仅
                    //     回灌劝阻文案，循环照常继续，LLM 可无视劝阻重试到
                    //     max_iters（"显示器没接刹车"）。拒绝结果仍可见于模型/用户
                    //     （上方已落盘），但不再发起下一轮 LLM 调用。副作用串行与
                    //     只读并行桶在此汇合，单点检查覆盖两条路径。
                    if matches!(
                        self.sandbox_breaker.state(),
                        crate::sandbox::BreakerState::HardTripped
                    ) {
                        tracing::warn!("sandbox denial breaker hard-tripped: forcing turn end");
                        metrics::record_error("sandbox");
                        self.append_terminal_notice("[沙箱拒绝硬熔断：已强制终止本轮]").await;
                        let event = Event::TurnEnd {
                            stop_reason: StopReason::Stopped,
                        };
                        self.persist_event(&event).await;
                        self.events.emit(event);
                        return Ok(TurnOutcome::Finished(Message::assistant_text(
                            "[沙箱拒绝硬熔断：连续多次被内核级拒绝，已强制终止本轮]".to_string(),
                        )));
                    }
                }

                // 达到 max_iters 上限
                tracing::warn!(max_iters, "turn exceeded max tool iterations");
                self.append_terminal_notice("[达到最大工具调用轮次上限]").await;
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
                    // 终态消息先落盘再广播（A-P2）：UI 已展示的 [已取消] 需入
                    // transcript，否则 resume 后 UI 与历史永久分歧
                    self.append_terminal_notice("[已取消]").await;
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
                    self.append_terminal_notice("[turn 超时终止]").await;
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
            // CORE-6（2026-08-25 R2 审查）：Failed/Err 路径补发 TurnEnd——此前仅
            // Finished/Interrupted 发终结事件，事件流消费者无从感知终结（CLI 被
            // 迫加 500ms 兜底超时、LSP 进度条悬挂在 Begin 态）。run_turn 入口的
            // 早退 Err（如用户消息落盘失败）不经过此处：彼时尚未广播任何流开始
            // 事件，补发反而产生孤儿 TurnEnd。
            if matches!(&result, Ok(TurnOutcome::Failed(_)) | Err(_)) {
                let event = Event::TurnEnd {
                    stop_reason: StopReason::Stopped,
                };
                self.persist_event(&event).await;
                self.events.emit(event);
            }
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
            // CORE-9（2026-08-25 R2 审查）：先释放 turn_active 再重建——若顺序
            // 相反，两步之间到达的 `cancel()` 会取消**新** token，下一次 run_turn
            // 秒取消一轮（再下一轮自愈）。窗口内新 run_turn 被 `_turn_gate` 排除，
            // 无其他竞态。显式 drop 复位 guard（`_` 前缀仅为避免未读告警）。
            drop(turn_guard);
            *self
                .cancel_token
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = CancellationToken::new();
            // R4（RT4-7）：清空本轮未消费的 Hook 注入缓冲——软重复提醒/
            // inject_context 在第 N 轮迭代末尾入缓冲，若随后硬停止/超时/取消，
            // 缓冲残留到下一个用户轮次的首个请求 system 头部（用户看到针对
            // 上一轮已终结问题的陈旧提醒）。
            self.pending_hook_contexts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
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

    /// 终态合成消息落盘（A-P2，2026-08-25 审查）：取消/超时/上限/重复终止的
    /// `[...]` 提示此前只返回给前端、不入 transcript——违反"先落盘再广播"
    /// 自定不变量（design.md §2.1），resume 后 UI 所见与会话历史永久分歧。
    /// 落盘失败仅告警不阻断（中断路径不应因存储故障而失败）。
    async fn append_terminal_notice(&self, text: &str) {
        let msg = Message::assistant_text(text.to_string());
        if let Err(e) = self.storage.append(&self.session.id, &msg).await {
            metrics::record_error("storage");
            tracing::warn!(error = %e, "terminal notice persist failed");
        }
        self.ctx.append(msg.clone()).await;
        let event = Event::MessageAppended(msg);
        self.persist_event(&event).await;
        self.events.emit(event);
    }

    /// 流式调用 LLM 并聚合为 assistant 消息。
    ///
    /// 返回 `(Message, Option<StopReason>)`：`stop_reason` 为 provider 报告的停止
    /// 原因（`MaxTokens` 截断等），供 `run_turn` 透传到 `Event::TurnEnd`——不再
    /// 硬编码 `EndTurn`（2026-08-23 审查 §4-P1）。
    ///
    /// `OTel`：`llm_call` span 包裹整次 provider 调用（design.md §15.1），字段不含
    /// 凭证（C-04：仅记 model 与消息数，不记 input 原文）。
    async fn stream_llm(
        &self,
        req: ChatRequest,
    ) -> Result<(Message, Option<StopReason>, Option<crate::provider::Usage>), crate::model::LlmError>
    {
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

        // 详细日志（排查用）：请求元信息 + 响应统计。字段不含 input 原文与凭证（C-04）。
        tracing::info!(
            llm.provider = %provider_id,
            llm.model = %model_name,
            llm.message_count = req.messages.len(),
            phase = "request",
            "llm_call started"
        );

        // `.instrument(span)` 替代 `span.enter()` guard（2026-08-25 审查 A-P3）：
        // entered guard 跨 `.await` 持有依赖线程局部语义，与 `run_turn` 的
        // instrument 风格自相矛盾且 future 搬移时 span 归属错误。整个流式消费
        // 体作为 inner future 被 instrument，span 跟随执行位置。
        async move {
        let mut stream = self.provider.chat_stream(req).await?;
        let mut acc = DeltaAccumulator::new();
        self.events.emit(Event::TurnStreamingStarted);

        let mut text_chars = 0usize;
        let mut reasoning_chars = 0usize;
        let mut tool_calls = 0usize;
        while let Some(delta) = stream.next().await {
            // 流中错误（2026-08-23 审查 §5-P1）：若已累积**纯文本**内容（UI 已通过
            // Token 事件展示），直接上抛会把它连同 acc 一起丢弃——重放/审计与会话
            // 不一致。此时记错误指标后降级为 `StopReason::Stopped` 的部分消息返回；
            // 工具调用中途出错则参数完整性无法保证，维持原语义上抛。
            let delta = match delta {
                Ok(d) => d,
                Err(e) => {
                    if !acc.has_tool_calls() && !acc.text().is_empty() {
                        metrics::record_error("llm");
                        tracing::warn!(
                            error = %e,
                            salvaged_chars = text_chars,
                            "llm stream error mid-turn; salvaging partial text output (stop_reason=Stopped)"
                        );
                        let usage = acc.usage().cloned();
                        return Ok((acc.finalize(), Some(StopReason::Stopped), usage));
                    }
                    return Err(e);
                }
            };
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
        let stop_reason = acc.stop_reason().cloned();
        let usage = acc.usage().cloned();
        Ok((acc.finalize(), stop_reason, usage))
        }
        .instrument(span)
        .await
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
        // 构造 ToolContext：注入沙箱驱动/策略/journal（M4，shell.run/fs 用）+
        // 点对点 prompter（`ui.ask` 主动提问用，与权限链同一实例）。
        // CORE-2/CORE-3（2026-08-25 R2 审查）：执行限制来自 `RuntimeConfig.tools`
        // （此前死配置）；cancel_token 下传（此前孤立 token，协作式取消空转）。
        let (timeout_sec, max_out, max_read) = {
            let cfg = self
                .config
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                Duration::from_secs(cfg.tools.shell_timeout_sec),
                cfg.tools.shell_max_output_bytes,
                cfg.tools.fs_max_read_bytes,
            )
        };
        let cancel_token = self
            .cancel_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let ctx = ToolContext::new(self.workdir.read().await.clone(), self.session.id.clone())
            .with_limits(timeout_sec, max_out, max_read)
            .with_canceller(cancel_token)
            .with_sandbox(self.sandbox_driver.clone(), self.sandbox_policy.clone())
            .with_journal_opt(self.journal.clone())
            .with_prompter_opt(Some(self.prompter.clone()))
            .with_events_opt(Some(self.events.clone()))
            .with_audit_opt(Some(self.audit.clone()));

        // 分桶：无副作用 → 并行；有副作用 → 串行（含权限检查）
        // S14：查不到的工具归入副作用桶（fail-closed）——走权限链后再由 dispatch
        // 报 NotFound，避免懒注册语义变化演变为免检绕过
        let readonly_of = |c: &ToolCall| {
            self.tools
                .get(&c.name)
                .is_some_and(|t| t.side_effect() == SideEffect::None)
        };

        // R8 波次调度（2026-08-28 R8 审查，替代 A-P1 的"全读在前才并行"判定）：
        // 按 LLM 原始调用序扫描，把**相邻的只读调用**聚为"读块"整体并行执行，
        // 副作用调用保持严格串行。严格优于旧逻辑：
        //   - 旧逻辑仅在"全部只读位于全部副作用之前"时并行读，混合顺序
        //     （写→读→读→写→读）退化为全串行；
        //   - 新逻辑把任意位置的相邻读并行化（读块与前后副作用按原始序隔离），
        //     且**不引入启发式依赖判定**——顺序由原始调用序保证，无 DAG 误判
        //     风险（启发式 DAG 对 shell.run 等 opaque 工具的路径依赖无法覆盖，
        //     误判"独立"会在真实文件系统上造成数据竞争，C-11 序错误不可检测）。
        //   只读块内部并行上限仍受 `tools.parallel_reads` 约束（0 = 串行）。
        //   结果最终按原始顺序统一回填（sort），两条路径一致。
        let mut results: Vec<(ToolCallId, ToolResult)> = Vec::with_capacity(calls.len());
        let mut pending_reads: Vec<&ToolCall> = Vec::new();
        for call in calls {
            if readonly_of(call) {
                pending_reads.push(call);
                continue;
            }
            // 遇到副作用调用：先并行执行已累积的读块（读块内只读、相互独立），
            // 再串行执行副作用（含权限 + Hook）。读块在副作用**之后**于原始序
            // 中出现时同样聚合并行——被写文件已由前序副作用完成，序正确。
            if !pending_reads.is_empty() {
                results.extend(self.run_readonly_bucket(&pending_reads, &ctx).await?);
                pending_reads.clear();
            }
            let call_side_effect = self.tool_side_effect(&call.name);
            // `tool_call` span（design.md §15.1）：副作用桶串行执行，包裹权限检查 + dispatch。
            // RT-7（2026-08-26 R3 审查）：用 `.instrument` 而非跨 await 持有
            // `enter()` guard——多线程 runtime 下 future 会被 worker 迁移，
            // 线程局部 span 上下文在迁移后失真（与并行路径 CORE-4 同理）。
            let span = tracing::debug_span!(
                "tool_call",
                session.id = %ctx.session_id,
                tool.name = %call.name,
                tool.side_effect = ?call_side_effect,
                tool.parallel = false,
                call_id = %call.id,
                otel.name = span_name::TOOL_CALL,
            );
            results.push(
                self.execute_side_effect_call(call, &ctx)
                    .instrument(span)
                    .await?,
            );
        }
        // 尾部只读块
        if !pending_reads.is_empty() {
            results.extend(self.run_readonly_bucket(&pending_reads, &ctx).await?);
        }

        // 按 LLM 原始顺序回填，保证 tool_result 与 tool_calls 一一对应
        results.sort_by_key(|(id, _)| calls.iter().position(|c| c.id == *id).unwrap_or(usize::MAX));

        Ok(results)
    }

    /// 工具副作用分级查询（查不到返回 `SideEffect::None`，span 属性用）。
    fn tool_side_effect(&self, name: &str) -> SideEffect {
        self.tools
            .get(name)
            .map_or(SideEffect::None, |t| t.side_effect())
    }

    /// 执行只读桶：按 `tools.parallel_reads` 并发（0 = 串行）。
    ///
    /// 从 `execute_tool_calls` 提取（2026-08-25 审查 A-P1 保序回退需要按单调用
    /// 复用该路径）。克隆 `events`/`tools` 到闭包外，让 async 块只捕获 owned
    /// 数据，避免捕获 `&self` 导致 future 非 `'static`（无法被 SDK `tokio::spawn`）。
    ///
    /// **HRTB 修复**：`readonly.iter().map(|call| async move { ... })` 中 `call`
    /// 是 `&&ToolCall`，闭包签名 `fn(&'a &'b ToolCall) -> impl Future + 'a` 不满足
    /// `buffer_unordered` 要求的 HRTB（future 类型对任意 `'a` 必须相同）。把每个
    /// future 装箱为 `Pin<Box<dyn Future + Send>>`，擦除生命周期参数，统一类型。
    async fn run_readonly_bucket(
        &self,
        readonly: &[&ToolCall],
        ctx: &crate::tool::ToolContext,
    ) -> Result<Vec<(ToolCallId, ToolResult)>, RuntimeError> {
        let events = self.events.clone();
        let tools = self.tools.clone();
        let denial_detector = self.denial_detector.clone();
        let sandbox_breaker = self.sandbox_breaker.clone();
        // R4（RT4-2）：SEC-11 审计补口——熔断计数两条路径共用，审计此前
        // 只覆盖副作用串行路径，只读工具（含 MCP SideEffect::None）的权威
        // 拒绝不落 audit.log，最值得取证的事件无痕。
        let audit = self.audit.clone();
        let session_id = self.session.id.clone();
        // SEC-6（R5）：防伪 nonce 克隆进闭包（子进程不可知的随机值）
        let denial_nonce = self.denial_nonce.clone();
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
                let audit = audit.clone();
                let session_id = session_id.clone();
                let denial_nonce = denial_nonce.clone();
                // `call` 是 `&&ToolCall`（来自 `Vec<&ToolCall>::iter`），需解引用到
                // `ToolCall` 再 clone，否则只克隆引用，async 块仍借用 `readonly`。
                let call: ToolCall = (**call).clone();
                // `tool_call` span（design.md §15.1）：只读桶并行执行，每个调用独立
                // span。CORE-4（2026-08-25 R2 审查）：`.instrument()` 替代
                // `span.enter()`——buffer_unordered 下 future 会被 tokio 在 worker
                // 线程间迁移，`Entered` 的线程局部语义在迁移后失真。
                let span = tracing::debug_span!(
                    "tool_call",
                    session.id = %ctx.session_id,
                    tool.name = %tool_name,
                    tool.side_effect = "none",
                    tool.parallel = true,
                    call_id = %call_id,
                    otel.name = span_name::TOOL_CALL,
                );
                let body = async move {
                    let tool_timer = metrics::start_timer();
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
                                &denial_nonce,
                            ) {
                                // R4（RT4-2）：SEC-11 审计与副作用路径同格式落盘
                                if let Some(info) = &r.metadata.sandbox_denied {
                                    let rec = Self::sandbox_denial_audit_record(
                                        &session_id,
                                        sandbox_breaker.count(),
                                        &tool_name,
                                        &e,
                                        info,
                                    );
                                    Self::record_sandbox_denial_audit(audit.clone(), rec);
                                }
                                r
                            } else {
                                // design.md §4.5：工具错误以 is_error=true 回灌 LLM
                                // 自我修正，不中止 turn（未知工具/参数不合法等模型可自行纠正）。
                                ToolResult::err_text(format!("tool error: {e}"))
                            }
                        }
                    };
                    Self::emit_readonly_finished(
                        &events, &tool_name, &call_id, &result, tool_timer,
                    );
                    Ok::<_, RuntimeError>((call.id.clone(), result))
                };
                let fut: ToolFuture = Box::pin(body.instrument(span));
                fut
            })
            .collect();
        let mut results: Vec<(ToolCallId, ToolResult)> = Vec::with_capacity(readonly.len());
        let parallel_reads = {
            let cfg = self
                .config
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cfg.tools.parallel_reads
        };
        // M-12：`parallel_reads = 0` 时串行执行（顺序与 LLM 原始顺序一致，便于定位）。
        // 并行分支用 `buffer_unordered`；结果由调用方按原始顺序统一回填（sort）。
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
        Ok(results)
    }

    /// 只读桶调用收尾：Finished 事件 + 耗时日志 + metrics（R4 自闭包提取，
    /// 收敛 `run_readonly_bucket` 行数）。
    fn emit_readonly_finished(
        events: &EventBus,
        tool_name: &str,
        call_id: &ToolCallId,
        result: &ToolResult,
        tool_timer: std::time::Instant,
    ) {
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
        metrics::record_tool_call(tool_name, "none", result_str);
        metrics::record_elapsed("tool_call_duration_ms", "tool", tool_name, tool_timer);
    }
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
