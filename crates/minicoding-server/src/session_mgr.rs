//! `SessionManager`：多会话管理（T-M8-2）。
//!
//! 每个 `ServerSession` 持有一个 `Arc<Runtime>` + `EventCursor` + `PendingPermissions`。
//! HTTP handler 通过 `SessionManager` 创建/查找/删除会话。
//!
//! **并发模型**：每个 session 同一时刻只允许一个 `run_turn` 在执行（`Runtime` 是
//! 单会话聚合根，C-31 上下文一致性）。`SendUserMessage` handler 在调用 `run_turn`
//! 前获取 session 级 `Mutex`，并发请求排队等待（第二个请求在第一个 turn 完成后才开始）。
//!
//! **事件 seq 分配**（2026-08-25 审查 F-seq 收敛）：`EventCursor` 为每个事件分配
//! 单调递增 `seq`，**只在会话级常驻 sequencer task 一处分配**（见 `insert_session`）；
//! SSE/ACP/LSP 订阅端经 `subscribe_sequenced`/`replay_after` 读取已带 seq 的事件，
//! 不再自行 `push_event`。SSE 流用 `seq` 做 cursor 恢复（见 `sse.rs`）。

use crate::prompter::{PendingPermissions, ServerPrompter};
use crate::runtime_builder::{ServerRuntimeParams, build_runtime};
use minicoding_core::metrics;
use minicoding_core::model::{
    Message, Session, SessionId, SessionMeta, Task, TurnOutcome, UserInput,
};
use minicoding_core::policy::{Decision, PermissionMode};
use minicoding_core::runtime::{Event, Runtime};
use minicoding_core::storage::Storage;
use minicoding_protocol::cursor::EventCursor;
use minicoding_protocol::event::EventKind;
use minicoding_storage::{JsonlEventStore, JsonlSnapshotStore, JsonlStorage, replay_session_state};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use thiserror::Error;
use time::OffsetDateTime;
use tokio::sync::Mutex as TokioMutex;

/// Server 端会话错误。
#[derive(Debug, Error)]
pub enum SessionManagerError {
    /// 会话不存在。
    #[error("session {0} not found")]
    NotFound(SessionId),
    /// 会话已存在（CreateSession 重复 id）。
    #[error("session {0} already exists")]
    AlreadyExists(SessionId),
    /// Runtime 构造失败。
    #[error("runtime build failed: {0}")]
    BuildFailed(String),
    /// 当前已有 turn 在执行（不阻塞模式返回此错误）。
    #[error("session {0} has a turn in progress")]
    TurnInProgress(SessionId),
    /// 权限请求不存在（已 resolved 或超时）。
    #[error("permission {0} not found (already resolved or expired)")]
    PermissionNotFound(String),
}

/// 单个 server 会话的状态。
///
/// 持有 `Arc<Runtime>`（单会话聚合根）、`EventCursor`（seq 分配）、
/// 排队 turn 上限（R8 FE-13，C-07）：超过该数量的并发 POST /messages 直接
/// 429（turn 本身可长跑，排队任务无界堆积会耗尽内存）。运行中 1 个 + 排队
/// 4 个对交互式客户端足够。
const TURN_QUEUE_LIMIT: usize = 4;

/// `PendingPermissions`（权限交互表）、`turn_lock`（turn 串行化）、
/// `task_state`（任务快照，供 `list_sessions`/`get_session` 返回）。
pub struct ServerSession {
    /// Runtime 聚合根（单会话）。
    pub runtime: Arc<Runtime>,
    /// 事件 seq 分配器（SSE cursor 恢复用）。`TokioMutex` 因 `push_event`/`replay_after`
    /// 在 async 上下文中调用，且 lock 持续时间短。
    pub cursor: TokioMutex<EventCursor>,
    /// 已分配 seq 的事件转发通道（SSE/ACP/LSP 订阅端从这里读取**已带 seq** 的
    /// 事件，不再自行分配 seq——2026-08-25 审查 F-seq：seq 必须单一写者，
    /// 否则同一事件被多个订阅端重复 push 进 ring buffer，多客户端 seq 漂移、
    /// 断线重放重复）。
    sequenced_tx: tokio::sync::broadcast::Sender<(u64, EventKind)>,
    /// pending 权限请求表（`ServerPrompter` 共享）。`TokioMutex` 因 `prompt` 在
    /// `timeout().await` 上下文中持有锁。
    pub pending_permissions: PendingPermissions,
    /// turn 串行锁（同一 session 同一时刻只允许一个 turn）。`TokioMutex` 因锁跨
    /// `run_turn().await` 持有（不能跨 await 持有 `std::sync::Mutex`）。
    pub turn_lock: TokioMutex<()>,
    /// 排队 turn 信号量（R8 FE-13）：HTTP 每次 POST /messages spawn 一个 task
    /// 阻塞在 `turn_lock`——无上限可无限堆积（C-07 资源不可耗尽）。容量
    /// [`TURN_QUEUE_LIMIT`]，`try_acquire_owned` 失败即 429（客户端应稍后重试）。
    /// `Arc` 因 permit 需跨 task 边界 owned 持有。
    pub turn_queue: std::sync::Arc<tokio::sync::Semaphore>,
    /// 任务列表快照（由 `Event::TaskUpdated` 订阅者维护，纯内存态）。
    /// `StdMutex` 因仅做 `Vec` 查/改（无 async 上下文）。任务权威源是
    /// `TaskStore`（tools crate），此字段只用于 HTTP 查询返回。
    pub task_state: StdMutex<Vec<Task>>,
    /// SSE 活动订阅者计数（FE-17，2026-08-28 R5 收尾）：空闲驱逐跳过
    /// 有订阅者的会话（开着 Web 标签页的会话不应被驱逐）。
    pub sse_subscribers: std::sync::atomic::AtomicUsize,
    /// 会话已删除标志（R8 FE-6）：`delete` 置位——已排队/已获取 `turn_lock`
    /// 的 task 在真正执行前检查此标志，删除后不再跑 turn（此前 `cancel`
    /// 只置 Runtime 取消 token，排队的 task 仍会执行并写存储）。
    closed: std::sync::atomic::AtomicBool,
}

impl ServerSession {
    /// 创建新会话状态。
    fn new(runtime: Arc<Runtime>, pending: PendingPermissions) -> Self {
        // 容量与 ring buffer 对齐（1024）：订阅端消费慢时 Lagged → RehydrateRequired，
        // 与 EventBus 默认容量（1024，见 core::runtime::event）同量级，降低慢客户端
        // 误触发重同步的概率
        let (sequenced_tx, _) = tokio::sync::broadcast::channel(1024);
        Self {
            runtime,
            cursor: TokioMutex::new(EventCursor::new(1024)),
            sequenced_tx,
            pending_permissions: pending,
            turn_lock: TokioMutex::new(()),
            turn_queue: std::sync::Arc::new(tokio::sync::Semaphore::new(TURN_QUEUE_LIMIT)),
            task_state: StdMutex::new(Vec::new()),
            sse_subscribers: std::sync::atomic::AtomicUsize::new(0),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 返回会话 ID。
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.runtime.session().id
    }

    /// 分配 seq 并把事件推入 cursor ring buffer，返回分配的 seq。
    ///
    /// `EventKind` 序列化为 JSON 后存入 `EventCursor`，SSE 流用 `replay_after_with_seq`
    /// 重放，把 seq 填入 SSE `id:` 字段。同时把 `(seq, EventKind)` 广播到
    /// `sequenced_tx`，供 SSE/ACP/LSP 订阅端实时消费。
    ///
    /// **seq 单一写者**（2026-08-25 审查 F-seq）：本方法只被会话级常驻 sequencer
    /// task（见 [`SessionManager::insert_session`]）调用；订阅端从
    /// `subscribe_sequenced`/`replay_after` 读取已带 seq 的事件，不得自行分配。
    pub async fn push_event(&self, event: &Event) -> u64 {
        let kind = EventKind::from(event);
        let json = serde_json::to_value(&kind).unwrap_or(serde_json::Value::Null);
        let seq = {
            let mut cursor = self.cursor.lock().await;
            cursor.push(json)
        };
        // FE-R6-3（2026-08-28 R6 审查）：durable recovery 激活——`durable_seq`
        // 此前仅在 `seed_cursor_from_runtime`（恢复时）设置一次，新会话为 0、
        // 运行中不随持久化推进，`classify_replay` 对 evict 后的事件一律判
        // `Unrecoverable` → 长会话断线重连退化为全量 RehydrateRequired
        // （design.md §25.5 的 EventStore 重放路径实际不可达）。这里随 push
        // 同步推进：`Runtime::durable_seq` 是最近一次成功持久化的 seq，订阅端
        // 的 `after_seq ≤ durable_seq` 即可走 `EventStore::load_after` 恢复。
        let durable = self.runtime.durable_seq().await;
        self.cursor.lock().await.set_durable(durable);
        // 先入 buffer 后广播：订阅端收到 (seq, kind) 时重放路径必已可见该 seq
        let _ = self.sequenced_tx.send((seq, kind));
        seq
    }

    /// 按 Runtime 持久化进度播种 cursor（FE-1，2026-08-25 R2 审查）。
    ///
    /// 懒恢复/首次 turn 的 `init_event_stream` 之后调用：cursor 从持久化流
    /// 最大 seq 之后连续编号（不再从 1 撞号），且 `Last-Event-ID ≤ persisted`
    /// 的重连可走 durable recovery。取 max 幂等，重复调用安全。
    pub async fn seed_cursor_from_runtime(&self) {
        let persisted = self.runtime.next_event_seq().await.saturating_sub(1);
        self.cursor.lock().await.seed(persisted);
    }

    /// 订阅已分配 seq 的事件流（实时推送用）。
    ///
    /// 返回 `(seq, EventKind)` 广播 receiver——事件已由单一写者（sequencer task）
    /// 分配 seq 并写入 ring buffer，订阅端直接转发即可，不触碰 `cursor`。
    #[must_use]
    pub fn subscribe_sequenced(&self) -> tokio::sync::broadcast::Receiver<(u64, EventKind)> {
        self.sequenced_tx.subscribe()
    }

    /// 从 `after_seq` 之后重放事件（SSE 恢复用），返回 `(seq, Value)` 列表。
    ///
    /// 重放策略（见 `design.md` §25.5）：
    /// 1. **内存 ring buffer 命中**：`after_seq` 仍在 `EventCursor` buffer 中，
    ///    直接重放 buffer 中 `seq > after_seq` 的事件；
    /// 2. **durable recovery**：`after_seq` 已 evict 但 ≤ `Runtime::durable_seq`，
    ///    调 `EventStore::load_after(after_seq)` 从持久化事件流重放。仅持久化事件
    ///    子集可恢复（瞬态事件如 `Token` 不可恢复，客户端应容忍缺失）；
    /// 3. **不可恢复**：`after_seq` > `durable_seq`（或 `EventStore` 为 `NoopEventStore`），
    ///    返回 `None`——SSE handler 应发 `RehydrateRequired`。
    ///
    /// `EventStore::load_after` 返回 `EventRecord`（含 `PersistedEvent`），需转为
    /// `EventKind` JSON 以与内存路径一致（SSE `data:` payload 格式统一）。
    pub async fn replay_after(&self, after_seq: u64) -> Option<Vec<(u64, serde_json::Value)>> {
        // 1. 内存 ring buffer 命中（FE-1：三态分类，durable 路径可达）
        {
            let cursor = self.cursor.lock().await;
            match cursor.classify_replay(after_seq) {
                minicoding_protocol::cursor::ReplayOutcome::Buffer(replay) => {
                    return Some(replay.into_iter().map(|(s, v)| (s, v.clone())).collect());
                }
                // evict 但 ≤ durable_seq：落入下方 EventStore 重放
                minicoding_protocol::cursor::ReplayOutcome::NeedsDurable => {}
                minicoding_protocol::cursor::ReplayOutcome::Unrecoverable => return None,
            }
        }

        // 2. durable recovery：检查 after_seq 是否 ≤ durable_seq
        let durable_seq = self.runtime.durable_seq().await;
        if after_seq > durable_seq {
            return None; // 不可恢复
        }

        // 从 EventStore 重放持久化事件
        let event_store = self.runtime.event_store();
        let records = event_store
            .load_after(&self.runtime.session().id, after_seq)
            .await
            .ok()?;
        if records.is_empty() {
            // `NoopEventStore` 或事件文件不存在——回退到 RehydrateRequired
            return None;
        }

        // 转为 (seq, EventKind JSON) 格式（与内存路径一致）
        let mut result = Vec::with_capacity(records.len());
        for record in records {
            let kind = minicoding_protocol::event::EventKind::from_persisted(&record.event);
            let json = serde_json::to_value(&kind).unwrap_or(serde_json::Value::Null);
            result.push((record.seq, json));
        }
        Some(result)
    }
}

/// 多会话管理器。
pub struct SessionManager {
    /// 活跃会话表。`std::sync::Mutex` 因仅做 `HashMap` 查/增/删（纯数据，非 IO 资源），
    /// 按 tokio 官方建议用阻塞锁——避免 `async fn(&self, &str)` 的 future 借用
    /// `&self` 与 axum Handler HRTB 冲突。
    sessions: std::sync::Mutex<HashMap<SessionId, Arc<ServerSession>>>,
    /// 每会话最近活跃时刻（FE-8 空闲驱逐用）。访问/发消息时刷新。
    last_activity: std::sync::Mutex<HashMap<SessionId, std::time::Instant>>,
    /// 默认 Runtime 构造参数（从 CLI/env 读取，CreateSession 时使用）。
    default_params: ServerRuntimeParams,
    /// 权限交互超时（默认 300s）。
    permission_timeout: Duration,
    /// 磁盘会话访问（`~/.minicoding/sessions/`，与 `build_runtime` 共用目录）。
    /// `None` 表示会话目录不可用（降级为纯内存会话，不持久化不恢复）。
    disk: Option<DiskSessionStore>,
}

/// 磁盘会话存储访问（列表合并 + 懒恢复用）。
///
/// 与 `runtime_builder::build_runtime` 内部构造的 `JsonlStorage`/`JsonlEventStore`/
/// `JsonlSnapshotStore` 共用同一 `sessions_dir`，因此此处只读访问即看到
/// 所有会话（含重启前的历史会话）的持久化状态。
struct DiskSessionStore {
    storage: JsonlStorage,
    event_store: JsonlEventStore,
    snapshot_store: JsonlSnapshotStore,
}

/// 从快照恢复的会话安全上下文（FE-7，v3 snapshot schema）。
///
/// `permission_mode` 为 `None` 表示旧快照（无字段）或值不可解析——恢复侧回落
/// server 启动默认并告警。`sandbox_preset` 仅用于对比告警（preset 变更是进程级
/// 启动决策，不热切换，C-22）。
struct RestoredSecurityContext {
    permission_mode: Option<PermissionMode>,
    sandbox_preset: Option<String>,
}

/// 快照中的 `permission_mode` 字符串 → `PermissionMode`。
///
/// 写入侧用 serde 规范序列化（`rename_all = "snake_case"`，如 `"plan"`），此处
/// 走同一规范反序列化，未知字符串返回 `None` 由调用方告警回落。
fn parse_permission_mode(raw: &str) -> Option<PermissionMode> {
    serde_json::from_value(serde_json::Value::String(raw.to_string())).ok()
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManager")
            .field("permission_timeout", &self.permission_timeout)
            .finish_non_exhaustive()
    }
}

impl SessionManager {
    /// 创建 `SessionManager`。
    ///
    /// `default_params` 是 `CreateSession` 未指定 provider/model 时的默认参数。
    /// 磁盘会话目录不可用时降级为纯内存会话（不持久化、不恢复），
    /// 避免 `sessions_dir` 失败（如 HOME 未设置）导致 server 无法启动。
    #[must_use]
    pub fn new(default_params: ServerRuntimeParams, permission_timeout: Duration) -> Self {
        let disk = minicoding_core::paths::sessions_dir()
            .ok()
            .map(|dir| DiskSessionStore {
                storage: JsonlStorage::new(dir.clone()),
                event_store: JsonlEventStore::new(dir.clone()),
                snapshot_store: JsonlSnapshotStore::new(dir),
            });
        Self {
            sessions: std::sync::Mutex::new(HashMap::new()),
            last_activity: std::sync::Mutex::new(HashMap::new()),
            default_params,
            permission_timeout,
            disk,
        }
    }

    /// FE-8（2026-08-26 R3 审查）：机会式空闲会话驱逐。
    ///
    /// 长驻 server 的 sessions 表此前只增不减——每会话常驻 task/broadcast/ring
    /// buffer，内存单调上涨。磁盘侧已有懒恢复能力（`get_or_load`），逐出后仍
    /// 可按需恢复。在 `create_session`/`list_sessions` 入口顺带清扫（避免后台
    /// 任务生命周期管理）。跳过 turn 进行中的会话（`turn_lock` 被持有即覆盖
    /// "挂起权限等待中"场景——等待权限的 turn 同样持有该锁）。
    fn evict_idle_sessions(&self) {
        const MAX_IDLE: std::time::Duration = std::time::Duration::from_secs(21_600);
        let candidates: Vec<SessionId> = {
            let guard = self
                .last_activity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard
                .iter()
                .filter(|(_, t)| t.elapsed() > MAX_IDLE)
                .map(|(id, _)| id.clone())
                .collect()
        };
        if candidates.is_empty() {
            return;
        }
        let mut evicted = 0usize;
        for id in &candidates {
            // FE-17：有活动 SSE 订阅者（Web 标签页开着）的会话不驱逐——即便
            // 空闲超时。订阅者断开后计数归零，下次驱逐窗口正常处理。
            let (busy, has_subscriber) = {
                let sessions = self
                    .sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let s = sessions.get(id);
                (
                    s.is_some_and(|s| s.turn_lock.try_lock().is_err()),
                    s.is_some_and(|s| {
                        s.sse_subscribers.load(std::sync::atomic::Ordering::Relaxed) > 0
                    }),
                )
            };
            if busy || has_subscriber {
                continue;
            }
            if self.delete(id) {
                evicted += 1;
            }
            self.last_activity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(id);
        }
        if evicted > 0 {
            tracing::info!(
                evicted,
                max_idle_secs = 6 * 60 * 60,
                "idle sessions evicted"
            );
        }
    }

    /// 刷新会话活跃时刻（FE-8）。
    fn touch_activity(&self, session_id: &str) {
        self.last_activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.to_string(), std::time::Instant::now());
    }

    /// 返回默认 Runtime 构造参数的引用（HTTP `CreateSession` handler 用）。
    #[must_use]
    pub fn default_params(&self) -> &ServerRuntimeParams {
        &self.default_params
    }

    /// 创建新会话。
    ///
    /// `params_override` 为 `Some` 时覆盖默认参数（如客户端指定 provider/model）。
    /// 构造 `ServerPrompter` + `Runtime`，注册到 sessions map。
    ///
    /// `build_runtime` 是同步函数（构造 Runtime 不涉及 await），同步返回。
    ///
    /// # Errors
    /// Runtime 构造失败时返回 `SessionManagerError::BuildFailed`。
    ///
    /// # Panics
    /// 内部 `sessions` Mutex poisoned 时 panic（理论不可达，除非另一线程 panic）。
    pub fn create_session(
        &self,
        params_override: Option<ServerRuntimeParams>,
    ) -> Result<Arc<ServerSession>, SessionManagerError> {
        // FE-8：入口机会式清扫空闲会话
        self.evict_idle_sessions();
        let params = params_override.unwrap_or_else(|| self.default_params.clone());
        let pending: PendingPermissions = Arc::new(TokioMutex::new(HashMap::new()));
        let prompter: Arc<dyn minicoding_core::policy::PermissionPrompter> = Arc::new(
            ServerPrompter::new(pending.clone(), self.permission_timeout),
        );
        self.build_and_insert(&params, prompter, pending, None)
    }

    /// 创建新会话并注入自定义 `PermissionPrompter`（T-M8-9，LSP 端用 `LspPrompter`）。
    ///
    /// 与 `create_session` 的区别：prompter 由调用方提供（如 `LspPrompter`，通过
    /// `window/showMessageRequest` 完成权限交互），不使用 `ServerPrompter`。
    /// `pending_permissions` 仍创建（空 map，LSP 端不经 HTTP resolve 路径），
    /// 保持 `ServerSession` 结构一致。
    ///
    /// # Errors
    /// Runtime 构造失败时返回 `SessionManagerError::BuildFailed`。
    ///
    /// # Panics
    /// 内部 `sessions` Mutex poisoned 时 panic。
    pub fn create_session_with_prompter(
        &self,
        params_override: Option<ServerRuntimeParams>,
        prompter: Arc<dyn minicoding_core::policy::PermissionPrompter>,
    ) -> Result<Arc<ServerSession>, SessionManagerError> {
        let params = params_override.unwrap_or_else(|| self.default_params.clone());
        // LSP 端 `pending_permissions` 不使用（权限交互走 showMessageRequest，
        // 不经 HTTP `resolve_permission` 路径），但 `ServerSession` 结构需要此字段。
        let pending: PendingPermissions = Arc::new(TokioMutex::new(HashMap::new()));
        self.build_and_insert(&params, prompter, pending, None)
    }

    /// 构造 Runtime 并注册到会话表（`create_session`/`restore_session` 共用）。
    ///
    /// `preloaded` 为 `Some` 时构造的 Runtime 使用该会话（恢复历史会话用，
    /// 见 `restore_session`）。调用方需在恢复路径另行调用 `restore_history`/
    /// `init_event_stream`。
    ///
    /// # Errors
    /// Runtime 构造失败时返回 `SessionManagerError::BuildFailed`。
    fn build_and_insert(
        &self,
        params: &ServerRuntimeParams,
        prompter: Arc<dyn minicoding_core::policy::PermissionPrompter>,
        pending: PendingPermissions,
        preloaded: Option<Session>,
    ) -> Result<Arc<ServerSession>, SessionManagerError> {
        let runtime = build_runtime(params, prompter, preloaded)
            .map_err(|e| SessionManagerError::BuildFailed(e.to_string()))?;
        Ok(self.insert_session(Arc::new(runtime), pending))
    }

    /// 内部：把构造好的 Runtime 注册到会话表（含 `TaskUpdated` 订阅镜像）。
    ///
    /// 先查重（并发恢复竞争时复用已注册会话，避免重复 spawn 订阅循环），
    /// 再 spawn 常驻 `Event::TaskUpdated` 订阅，把任务快照写入 `task_state`
    /// （供 HTTP 查询）。任务权威源是 `TaskStore`（task.create/update 工具），
    /// 此处仅镜像其变更；会话删除后广播 sender drop，receiver 收到 closed，
    /// task 自然退出。
    /// `Lagged` 必须续跑：总线承载大量 `Token` 事件，订阅者必然周期性落后
    ///（`broadcast` 容量耗尽时丢历史事件），若退出则任务快照永久停更。
    ///
    /// # Panics
    /// 内部 `sessions` Mutex poisoned 时 panic。
    fn insert_session(
        &self,
        runtime: Arc<Runtime>,
        pending: PendingPermissions,
    ) -> Arc<ServerSession> {
        let session_id = runtime.session().id.clone();
        {
            let guard = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(existing) = guard.get(&session_id) {
                return existing.clone();
            }
        }
        let session = Arc::new(ServerSession::new(runtime, pending));

        // FE-3（2026-08-25 R2 审查）：最终插入用 entry 语义**原子判定**——此前
        // 开头查重与末尾无条件 insert 之间存在 TOCTOU：并发 get_or_load 同一
        // 未加载会话会产生双 Runtime/双 sequencer，败者的订阅 task 环永不退出、
        // 双写同一会话 jsonl。冲突时丢弃本实例（task 尚未 spawn，无泄漏）。
        {
            let mut guard = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(existing) = guard.get(&session_id) {
                let existing = existing.clone();
                drop(guard);
                return existing;
            }
            guard.insert(session_id, session.clone());
            // Metrics：活跃会话数 gauge
            metrics::set_active_sessions(guard.len() as u64);
        }

        Self::spawn_session_tasks(&session);
        session
    }

    /// 为已确认注册的会话 spawn 常驻 task（`TaskUpdated` 订阅镜像 + seq sequencer）。
    ///
    /// 仅在会话进入会话表之后调用（FE-3：保证失败方不遗留孤儿 task）。
    fn spawn_session_tasks(session: &Arc<ServerSession>) {
        let subscriber = session.clone();
        tokio::spawn(async move {
            let mut rx = subscriber.runtime.events().subscribe();
            loop {
                // 总线被 `Token` 事件冲满时丢历史事件，续跑即可（任务事件量少，不会丢关键快照）
                let event = match rx.recv().await {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                if let Event::TaskUpdated { task } = event {
                    let mut state = subscriber
                        .task_state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(existing) = state.iter_mut().find(|t| t.id == task.id) {
                        *existing = task;
                    } else {
                        state.push(task);
                    }
                }
            }
        });

        // 常驻 sequencer：唯一调用 `push_event` 的地方（2026-08-25 审查 F-seq）。
        // 此前 seq 分配分散在每个 turn 的消费 task + 各 SSE/ACP/LSP 连接——同一事件
        // 被多次 push 进 ring buffer（seq 重复、多客户端漂移、断线重放重复）。
        // 收敛到单写者后，订阅端经 `subscribe_sequenced` 拿到已带 seq 的事件。
        // turn 外发出的事件（如 HTTP set_permission_mode 触发的 PermissionModeChanged）
        // 也能获得 seq 并可重放。
        let sequencer = session.clone();
        tokio::spawn(async move {
            let mut rx = sequencer.runtime.events().subscribe();
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        sequencer.push_event(&event).await;
                    }
                    // Lagged 必须续跑：seq 分配停摆会导致所有订阅端永久失联
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "SessionManager sequencer lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    /// 查找会话（同步——`std::sync::Mutex` 的 `HashMap` 查找不需 await）。
    ///
    /// 同步函数消除 `async fn(&self, &str)` 的 future 生命周期参数，
    /// 避免 axum Handler HRTB 冲突。
    ///
    /// # Panics
    /// 内部 `sessions` Mutex poisoned 时 panic。
    #[must_use]
    pub fn get(&self, session_id: &str) -> Option<Arc<ServerSession>> {
        let guard = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.get(session_id).cloned()
    }

    /// 列出所有会话（内存活跃 + 磁盘历史合并）。
    ///
    /// **计数/摘要以磁盘 `index.json` 为准**：`Runtime.session().messages` 是
    /// 上下文快照，不随 `run_turn` 更新（消息写 storage + 广播 `MessageAppended`），
    /// 而 `JsonlStorage::append` 会同步 upsert index（含 summary/计数）。
    /// 磁盘历史会话（重启前的）也在此列出，点击时经 `get_or_load` 懒恢复。
    ///
    /// # Panics
    /// 内部 `sessions` Mutex poisoned 时 panic。
    pub fn list_sessions(&self) -> Vec<SessionMeta> {
        // FE-8：入口机会式清扫空闲会话
        self.evict_idle_sessions();
        // 磁盘历史会话 meta（count/summary 实时，`append` 时更新 index）
        let disk_metas = self
            .disk
            .as_ref()
            .and_then(|d| d.storage.list_sessions_sync().ok())
            .unwrap_or_default();

        let mut metas = Vec::new();
        let guard = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for session in guard.values() {
            let runtime = &session.runtime;
            let session_model = runtime.session();
            let disk = disk_metas.iter().find(|m| m.id == session_model.id);
            metas.push(SessionMeta {
                id: session_model.id.clone(),
                created_at: session_model.created_at,
                message_count: disk.map_or(0, |m| m.message_count),
                last_message_at: disk.map_or(session_model.created_at, |m| m.last_message_at),
                summary: disk.and_then(|m| m.summary.clone()),
                // 任务快照来自 `task_state`（TaskUpdated 订阅镜像，见 `ServerSession`）
                tasks: session
                    .task_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            });
        }
        drop(guard);

        // 磁盘历史会话（内存未加载）
        for m in disk_metas {
            if !metas.iter().any(|x| x.id == m.id) {
                metas.push(SessionMeta {
                    id: m.id,
                    created_at: m.created_at,
                    message_count: m.message_count,
                    last_message_at: m.last_message_at,
                    summary: m.summary,
                    tasks: Vec::new(),
                });
            }
        }

        // 按最后消息时间倒序（与 CLI `session list` 一致）
        metas.sort_by_key(|m| std::cmp::Reverse(m.last_message_at));
        metas
    }

    /// 获取会话；内存未加载时从磁盘懒恢复（重启后历史会话可见）。
    ///
    /// 所有会话访问入口（HTTP/NDJSON/ACP/workspace）应走此方法而非 `get`，
    /// 否则重启前的会话将 404。
    ///
    /// # Errors
    /// 会话不存在（内存 + 磁盘均无）时返回 `NotFound`；恢复失败时返回
    /// `BuildFailed`（磁盘数据损坏、Runtime 构造失败等）。
    pub async fn get_or_load(
        &self,
        session_id: &str,
    ) -> Result<Arc<ServerSession>, SessionManagerError> {
        if let Some(session) = self.get(session_id) {
            self.touch_activity(session_id);
            return Ok(session);
        }
        self.restore_session(session_id).await.inspect(|_| {
            self.touch_activity(session_id);
        })
    }

    /// 从磁盘恢复历史会话（懒加载，见 `design.md` §25 事件流重放）。
    ///
    /// 流程：磁盘加载 `Session`（snapshot + 事件流重放优先，消息日志回退）→
    /// `build_runtime` 预加载该会话 → `restore_history` 回填上下文 →
    /// `init_event_stream`（幂等，新/旧会话均安全）→ 注册到会话表。
    /// 恢复后 `Runtime` 的 `workdir` 为会话原工作目录（事件流/Snapshot 中的值），
    /// 使文件工具/sandbox/journal 与创建时一致。
    ///
    /// FE-7（2026-08-25 R2 审查遗留，已实现持久化）：快照带 `permission_mode`
    /// 时经 `plan_controller().set_mode` 还原原权限模式（plan/full-access 等语义
    /// 跨重启保持）；旧快照（v3 前）无该字段则回落 server 启动默认并显式 warn。
    /// `sandbox_preset` 仅对比告警——preset 变更是进程级启动决策（C-22 需显式
    /// 选定 + 二次确认），不做热切换。
    ///
    /// # Errors
    /// 会话不存在时返回 `NotFound`；磁盘加载或 Runtime 构造失败时返回
    /// `BuildFailed`。
    async fn restore_session(
        &self,
        session_id: &str,
    ) -> Result<Arc<ServerSession>, SessionManagerError> {
        let Some(disk) = &self.disk else {
            return Err(SessionManagerError::NotFound(session_id.to_string()));
        };
        // 1. 从磁盘加载会话（事件流重放优先，消息日志回退）
        let (session, security) = self.load_session_from_disk(disk, session_id).await?;
        // 2. 构造 Runtime（预加载会话；workdir 覆盖为会话原工作目录）
        let mut params = self.default_params.clone();
        params.workdir = session.workdir.clone();
        // FE-R6-1（2026-08-28 R6 审查）：恢复会话同样重锚定 OS 沙箱策略——
        // 默认策略内嵌服务端 workdir，跨机器/多目录会话恢复时 landlock 可写根
        // 与应用层 C-03 失配。
        params.sandbox_policy = params.sandbox_policy.with_workdir(&params.workdir);
        let pending: PendingPermissions = Arc::new(TokioMutex::new(HashMap::new()));
        let prompter: Arc<dyn minicoding_core::policy::PermissionPrompter> = Arc::new(
            ServerPrompter::new(pending.clone(), self.permission_timeout),
        );
        let runtime = build_runtime(&params, prompter, Some(session))
            .map_err(|e| SessionManagerError::BuildFailed(e.to_string()))?;
        let runtime = Arc::new(runtime);
        // 3. 回填上下文 + 初始化事件流（幂等：新/旧会话路径见 `init_event_stream`）
        runtime
            .init_event_stream()
            .await
            .map_err(|e| SessionManagerError::BuildFailed(e.to_string()))?;
        // 4. 注册（insert_session 内部查重，并发恢复安全）
        let session = self.insert_session(runtime, pending);
        // 5. FE-7：还原快照持久化的安全上下文。在 insert 之后应用——并发竞争时
        //    `insert_session` 可能返回已注册实例，对注册实例操作才有效；在
        //    seed_cursor 之前应用——set_mode 触发的 `PermissionModeChanged` 已
        //    持久化并计入 seq，cursor 播种保持连续。经 sequencer task 分配 seq，
        //    SSE 订阅端可见模式恢复事件。
        if let Some(sec) = &security {
            if let Some(mode) = sec.permission_mode {
                session.runtime.plan_controller().set_mode(mode).await;
                tracing::info!(
                    session = %session_id,
                    mode = ?mode,
                    "permission_mode restored from snapshot"
                );
            } else {
                tracing::warn!(
                    session = %session_id,
                    "snapshot has no persisted permission_mode (pre-v3 snapshot); \
                     falling back to server default"
                );
            }
            if let Some(preset) = &sec.sandbox_preset {
                let current = params.sandbox_policy.preset_tag();
                if preset != current {
                    tracing::warn!(
                        session = %session_id,
                        snapshot_preset = %preset,
                        current_preset = current,
                        "sandbox preset differs from snapshot; keeping process-level preset \
                         (preset changes require process restart, no hot switch)"
                    );
                }
            }
        }
        // 6. FE-1（2026-08-25 R2 审查）：按持久化进度播种 SSE cursor——
        //    此后新事件 seq 与重启前连续，Last-Event-ID 重连可走 durable
        //    recovery 而非误判不可恢复。
        session.seed_cursor_from_runtime().await;
        Ok(session)
    }

    /// 从磁盘加载 `Session`（snapshot + 事件流重放优先，消息日志回退）。
    ///
    /// 与 CLI `--resume` 的 `load_session_via_event_sourcing` 同构
    /// （见 `docs/design.md` §25.4）；事件重放失败（schema 不兼容等）时回退
    /// 消息日志路径，保证旧会话始终可恢复。
    ///
    /// 同时返回快照持久化的会话安全上下文（FE-7）：仅事件流/snapshot 路径有值，
    /// 消息日志回退路径为 `None`（旧格式无该信息）。
    ///
    /// # Errors
    /// 会话不存在（无事件流且无消息）时返回 `NotFound`；读取失败时返回
    /// `BuildFailed`。
    async fn load_session_from_disk(
        &self,
        disk: &DiskSessionStore,
        session_id: &str,
    ) -> Result<(Session, Option<RestoredSecurityContext>), SessionManagerError> {
        // 1. Event Sourcing 路径：snapshot + 事件流重放
        let snapshot = disk
            .snapshot_store
            .load_sync(&session_id.to_string())
            .map_err(|e| SessionManagerError::BuildFailed(format!("snapshot 加载失败: {e}")))?;
        // FE-7：replay 消费 snapshot 前先捕获安全上下文（旧快照字段缺省 → None）
        let security = snapshot.as_ref().map(|s| RestoredSecurityContext {
            permission_mode: s.state.permission_mode.as_deref().and_then(|raw| {
                let parsed = parse_permission_mode(raw);
                if parsed.is_none() {
                    tracing::warn!(
                        session = %session_id,
                        raw,
                        "unknown permission_mode in snapshot; ignoring"
                    );
                }
                parsed
            }),
            sandbox_preset: s.state.sandbox_preset.clone(),
        });
        let events = disk
            .event_store
            .load_events_sync(&session_id.to_string())
            .map_err(|e| SessionManagerError::BuildFailed(format!("事件流加载失败: {e}")))?;
        if !events.is_empty() || snapshot.is_some() {
            match replay_session_state(snapshot.as_ref(), events) {
                Ok(replayed) => return Ok((replayed.session, security)),
                Err(e) => {
                    tracing::warn!(
                        session = %session_id,
                        error = %e,
                        "事件重放失败，回退消息日志路径"
                    );
                }
            }
        }
        // 2. 回退：消息日志路径（旧会话无事件流）
        let messages = disk
            .storage
            .load(&session_id.to_string())
            .await
            .map_err(|e| SessionManagerError::BuildFailed(e.to_string()))?;
        if messages.is_empty() {
            return Err(SessionManagerError::NotFound(session_id.to_string()));
        }
        let created_at = messages
            .first()
            .map_or_else(OffsetDateTime::now_utc, |m| m.created_at);
        // 消息日志无 workdir 信息，用 server 默认工作目录（与 CLI 回退路径一致）
        Ok((
            Session {
                id: session_id.to_string(),
                created_at,
                workdir: self.default_params.workdir.clone(),
                config_hash: 0,
                messages,
            },
            None,
        ))
    }

    /// 删除会话（同步——仅从 `HashMap` 移除）。
    ///
    /// R8 FE-6：删除前置位 `closed` 标志——已排队/已持 `turn_lock` 的 task 在
    /// 真正执行前检查（`send_message_boxed`），删除后不再跑 turn（此前
    /// `cancel` 只置 Runtime 取消 token，排队的 task 仍执行并写存储）。
    ///
    /// # Panics
    /// 内部 `sessions` Mutex poisoned 时 panic。
    pub fn delete(&self, session_id: &str) -> bool {
        let mut guard = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(session) = guard.get(session_id) {
            session
                .closed
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let removed = guard.remove(session_id).is_some();
        if removed {
            // Metrics：活跃会话数 gauge
            metrics::set_active_sessions(guard.len() as u64);
        }
        removed
    }

    /// 解析权限请求（HTTP `POST /permissions/{pid}` 调用）。
    ///
    /// 查找 pending map，发送决策到 `ServerPrompter` 的 oneshot channel。
    ///
    /// # Errors
    /// 权限请求不存在（已 resolved 或超时）时返回 `PermissionNotFound`。
    pub async fn resolve_permission(
        &self,
        session_id: &str,
        permission_id: &str,
        decision: Decision,
    ) -> Result<(), SessionManagerError> {
        let session = self.get_or_load(session_id).await?;
        self.resolve_in_loaded(&session, permission_id, decision)
            .await
    }

    /// 仅在**已加载**会话中解析权限（不触发磁盘懒恢复）。
    ///
    /// R8 FE-10：NDJSON/ACP 的 `ResolvePermission` 无 `session_id` 字段，需遍历
    /// 全部会话——逐会话 `get_or_load` 会为每个磁盘会话触发完整事件流重放
    /// （N 次放大）。本方法只查内存会话表，未加载的会话不可能有 pending
    /// permission（pending 是运行期状态，磁盘恢复的会话无活跃权限请求）。
    pub async fn resolve_permission_loaded_only(
        &self,
        permission_id: &str,
        decision: Decision,
    ) -> bool {
        let sessions: Vec<Arc<ServerSession>> = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for session in sessions {
            if self
                .resolve_in_loaded(&session, permission_id, decision.clone())
                .await
                .is_ok()
            {
                return true;
            }
        }
        false
    }

    /// 在指定（已加载）会话中解析 pending permission。
    async fn resolve_in_loaded(
        &self,
        session: &Arc<ServerSession>,
        permission_id: &str,
        decision: Decision,
    ) -> Result<(), SessionManagerError> {
        let mut guard = session.pending_permissions.lock().await;
        match guard.remove(permission_id) {
            Some(entry) => {
                let _ = entry.tx.send(decision);
                Ok(())
            }
            None => Err(SessionManagerError::PermissionNotFound(
                permission_id.to_string(),
            )),
        }
    }

    /// 发送用户消息并驱动 turn（阻塞至 turn 完成）。
    ///
    /// **设计原因**：关联函数（非 `&self` 方法）+ owned `Arc<SessionManager>` 参数，
    /// 返回的 future 无外部借用（`'static`），避免 `async fn(&self, ..)` 与 axum
    /// `Handler` trait HRTB 的冲突。
    ///
    /// 内部获取 `turn_lock`（串行化），调用 `Runtime::run_turn_owned`。
    /// 事件 seq 分配由会话级常驻 sequencer task 负责（见 `insert_session`），
    /// 此处不再临时订阅 EventBus——2026-08-25 审查 F-seq：turn 内再开一个
    /// 消费 task 会与订阅端各自 `push_event`，同一事件在 ring buffer 中出现多份。
    ///
    /// # Errors
    /// - 会话不存在：`NotFound`；
    /// - `run_turn` 失败：透传 `RuntimeError`。
    pub async fn send_message_boxed(
        mgr: Arc<SessionManager>,
        session_id: String,
        text: String,
    ) -> Result<TurnOutcome, SessionManagerError> {
        let session = mgr.get_or_load(&session_id).await?;

        // 获取 turn 锁（串行化：同一 session 同时只有一个 turn）
        let _turn_guard = session.turn_lock.lock().await;

        // R8 FE-6：已删除会话不再执行 turn（DELETE 后置位 closed；此前排队的
        // task 拿到锁后仍会跑完整 turn 并写存储——"已删除"后消息仍在产生）。
        if session.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(SessionManagerError::NotFound(session_id));
        }

        // Clone `Arc<Runtime>` 断开 `session.runtime` 的 Arc-deref 借用链。
        let runtime = session.runtime.clone();

        // Event Sourcing：首次 turn 前初始化事件流（新会话持久化 SessionCreated，
        // 恢复会话加载 seq + snapshot，见 `design.md` §25.1）。
        // 延迟到首次 send_message 而非 create_session 调用，因 create_session 是
        // 同步函数（HTTP handler 直接返回），init_event_stream 是 async。
        // 幂等：`init_event_stream` 内部按 `next_seq` 判断新/旧会话，重复调用安全。
        runtime
            .init_event_stream()
            .await
            .map_err(|e| SessionManagerError::BuildFailed(e.to_string()))?;
        // FE-1：播种 cursor（新会话 SessionCreated seq=1 后同样适用；取 max
        // 幂等，与 restore 路径的播种叠加安全）
        session.seed_cursor_from_runtime().await;

        // 驱动 turn。`runtime` 是 owned `Arc<Runtime>`，`run_turn(&self)` 借用
        // `&*runtime`（局部借用，future 自包含）。
        let user_input = UserInput::from_text(text);
        let result = runtime.run_turn(user_input).await;

        match result {
            Ok(outcome) => Ok(outcome),
            Err(e) => Err(SessionManagerError::BuildFailed(e.to_string())),
        }
    }

    /// 取消当前 turn（`Runtime::cancel` 仅触发 `CancellationToken`，无 await）。
    ///
    /// # Errors
    /// 会话不存在时返回 `NotFound`。
    pub async fn cancel(&self, session_id: &str) -> Result<(), SessionManagerError> {
        let session = self.get_or_load(session_id).await?;
        session.runtime.cancel();
        Ok(())
    }

    /// 获取会话消息快照（`GET /sessions/{id}` 用）。
    ///
    /// # Errors
    /// - 会话不存在：`NotFound`；
    /// - `Storage::load` 失败：`BuildFailed`。
    pub async fn get_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<Message>, SessionManagerError> {
        let session = self.get_or_load(session_id).await?;
        // 从 storage 加载完整消息历史
        let storage = session.runtime.storage();
        let messages = storage
            .load(&session.runtime.session().id)
            .await
            .map_err(|e| SessionManagerError::BuildFailed(e.to_string()))?;
        Ok(messages)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use camino::Utf8PathBuf;
    use minicoding_core::sandbox::SandboxPolicy;

    fn test_params() -> ServerRuntimeParams {
        ServerRuntimeParams {
            provider_kind: "openai".to_string(),
            provider_name: None,
            api_base: "http://localhost:8080/v1".to_string(),
            api_key: "sk-test".to_string(),
            model: "gpt-4o".to_string(),
            workdir: Utf8PathBuf::from("."),
            system: None,
            permission_mode: minicoding_core::policy::PermissionMode::Default,
            sandbox_policy: SandboxPolicy::WorkspaceWrite {
                workdir: Utf8PathBuf::from("."),
                writable: Vec::new(),
            },
            timeout_sec: 120,
            max_retries: 3,
            small_model: None,
            turn_timeout_sec: 600,
            compress: true,
        }
    }

    #[tokio::test]
    async fn resolve_nonexistent_permission_errors() {
        let mgr = SessionManager::new(test_params(), Duration::from_secs(5));
        // 会话不存在
        let result = mgr
            .resolve_permission("nonexistent", "perm-1", Decision::Allow)
            .await;
        assert!(matches!(result, Err(SessionManagerError::NotFound(_))));
    }

    #[tokio::test]
    async fn get_nonexistent_session_returns_none() {
        let mgr = SessionManager::new(test_params(), Duration::from_secs(5));
        let result = mgr.get("nonexistent");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn list_sessions_returns_empty() {
        let _g = ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("创建临时目录");
        let dir_str = dir
            .path()
            .to_str()
            .expect("tempdir 路径应为 UTF-8")
            .to_string();
        let _guard = EnvGuard::set(&dir_str);
        let mgr = SessionManager::new(test_params(), Duration::from_secs(5));
        let list = mgr.list_sessions();
        assert!(list.is_empty(), "expected empty: list");
    }

    // ── 磁盘会话列表合并 + 懒恢复 ────────────────────────────────────────────

    // `ENV_LOCK`/`EnvGuard` 上移至 `crate::test_support`（2026-08-25 审查 F-routes）：
    // http 端点测试同样需要 `MINICODING_HOME` 隔离，跨模块共用同一把锁防并行竞争。

    use crate::test_support::{ENV_LOCK, EnvGuard};

    /// 预写一个磁盘会话（模拟重启前的历史会话：仅有消息日志，无事件流）。
    async fn seed_disk_session(dir: &Utf8PathBuf, id: &str, texts: &[&str]) {
        let storage = JsonlStorage::new(dir.clone());
        for t in texts {
            storage
                .append(&id.to_string(), &Message::user_text(*t))
                .await
                .expect("append 应成功");
        }
    }

    #[tokio::test]
    async fn list_sessions_merges_disk_history() {
        let _g = ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("创建临时目录");
        let dir_str = dir
            .path()
            .to_str()
            .expect("tempdir 路径应为 UTF-8")
            .to_string();
        let _guard = EnvGuard::set(&dir_str);

        // 预写磁盘会话（重启前的历史）
        let disk_id = "01DISKLIST".to_string();
        seed_disk_session(
            &Utf8PathBuf::from(&dir_str).join("sessions"),
            &disk_id,
            &["创建计算器项目"],
        )
        .await;

        let mgr = SessionManager::new(test_params(), Duration::from_secs(5));
        let list = mgr.list_sessions();
        // 磁盘会话出现在列表中（无需内存注册）
        let meta = list
            .iter()
            .find(|m| m.id == disk_id)
            .expect("磁盘会话应被列出");
        assert_eq!(meta.message_count, 1);
        assert_eq!(meta.summary.as_deref(), Some("创建计算器项目"));
        assert!(meta.tasks.is_empty());
    }

    #[tokio::test]
    async fn get_or_load_restores_disk_session() {
        let _g = ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("创建临时目录");
        let dir_str = dir
            .path()
            .to_str()
            .expect("tempdir 路径应为 UTF-8")
            .to_string();
        let _guard = EnvGuard::set(&dir_str);

        let disk_id = "01DISKRESTORE".to_string();
        seed_disk_session(
            &Utf8PathBuf::from(&dir_str).join("sessions"),
            &disk_id,
            &["第一条消息", "第二条消息"],
        )
        .await;

        let mgr = SessionManager::new(test_params(), Duration::from_secs(5));
        // 懒恢复：内存未注册，get_or_load 从磁盘恢复
        let session = mgr.get_or_load(&disk_id).await.expect("应恢复成功");
        assert_eq!(session.session_id(), &disk_id);
        // 消息可读（存储文件）
        let messages = mgr.get_messages(&disk_id).await.expect("消息应可读");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text(), "第一条消息");
        // 恢复后列表计数来自磁盘 index
        let list = mgr.list_sessions();
        let meta = list
            .iter()
            .find(|m| m.id == disk_id)
            .expect("恢复后仍应列出");
        assert_eq!(meta.message_count, 2);
    }

    #[tokio::test]
    async fn get_or_load_restores_permission_mode_from_snapshot() {
        // FE-7：快照持久化的 permission_mode 在懒恢复后还原（plan 模式跨重启保持）
        use minicoding_core::storage::{SessionSnapshot, SessionState};

        let _g = ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("创建临时目录");
        let dir_str = dir
            .path()
            .to_str()
            .expect("tempdir 路径应为 UTF-8")
            .to_string();
        let _guard = EnvGuard::set(&dir_str);

        let disk_id = "01SNAPMODE".to_string();
        let sessions_dir = Utf8PathBuf::from(&dir_str).join("sessions");
        seed_disk_session(&sessions_dir, &disk_id, &["快照前消息"]).await;
        // 预写带安全上下文的快照（模拟重启前 plan 模式会话）
        let state = SessionState {
            id: disk_id.clone(),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            workdir: ".".to_string(),
            config_hash: 0,
            messages: vec![Message::user_text("快照内消息")],
            permission_mode: Some("plan".to_string()),
            sandbox_preset: Some("workspace-write".to_string()),
        };
        JsonlSnapshotStore::new(sessions_dir.clone())
            .save_sync(&SessionSnapshot::new(1, state))
            .expect("预写快照应成功");

        // server 默认参数为 Default 模式；恢复后应回到快照记录的 Plan
        assert_eq!(test_params().permission_mode, PermissionMode::Default);
        let mgr = SessionManager::new(test_params(), Duration::from_secs(5));
        let session = mgr.get_or_load(&disk_id).await.expect("应恢复成功");
        let restored = session.runtime.plan_controller().snapshot().await;
        assert_eq!(
            restored.mode,
            PermissionMode::Plan,
            "恢复后权限模式应来自快照而非 server 默认"
        );
    }

    #[tokio::test]
    async fn get_or_load_missing_returns_notfound() {
        let _g = ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("创建临时目录");
        let dir_str = dir
            .path()
            .to_str()
            .expect("tempdir 路径应为 UTF-8")
            .to_string();
        let _guard = EnvGuard::set(&dir_str);

        let mgr = SessionManager::new(test_params(), Duration::from_secs(5));
        let result = mgr.get_or_load("01MISSING").await;
        assert!(matches!(result, Err(SessionManagerError::NotFound(_))));
    }

    // R8 FE-6：delete 置位 closed——已持会话 Arc 的排队 task 检查后不再跑 turn
    #[tokio::test]
    async fn delete_marks_session_closed() {
        let _g = ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("创建临时目录");
        let dir_str = dir
            .path()
            .to_str()
            .expect("tempdir 路径应为 UTF-8")
            .to_string();
        let _guard = EnvGuard::set(&dir_str);

        let mgr = SessionManager::new(test_params(), Duration::from_secs(5));
        let session = mgr.create_session(None).expect("创建会话应成功");
        let sid = session.session_id().clone();
        let arc = mgr.get(&sid).expect("已创建的会话应在内存表");
        assert!(
            !arc.closed.load(std::sync::atomic::Ordering::SeqCst),
            "创建后未删除，closed 应为 false"
        );
        assert!(mgr.delete(&sid), "delete 应移除会话");
        // 已持 Arc 的调用方可观察到 closed 置位（send_message_boxed 的关门检查）
        assert!(
            arc.closed.load(std::sync::atomic::Ordering::SeqCst),
            "delete 后 closed 应置位"
        );
    }
}
