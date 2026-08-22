//! `PlanControllerHandle`（A6 自 rt.rs 抽出）：`PlanModeController` 的 Runtime 内部适配器。
//!
//! **为何留在 core 而非领域 crate**（对审查 A6 的处理说明）：本结构仅依赖 core
//! 内部类型——若迁往 `minicoding-policy` 等领域 crate，将制造 policy→storage/runtime
//! 的**新交叉依赖**，恰违反 AGENTS.md §3.2 与 A8 架构守卫。trait 边界已保证
//! `plan.exit` 工具与 Runtime 解耦；此处只是 Runtime 的私有接线件。
//!
//! Event Sourcing：`exit_plan`/`set_mode` 触发 `PermissionModeChanged` 时同步持久化
//! 到事件流（replay 重建 `final_permission_mode`，见 `replay_session_state`）。

use std::sync::Arc;

use tokio::sync::{Mutex as TokioMutex, RwLock};

use super::event::{Event, EventBus};
use crate::model::{PolicyError, SessionId};
use crate::policy::{PermissionMode, PlanModeController, PlanModeSnapshot, PreApprovedPrompt};
use crate::provider::BoxFuture;
use crate::storage::{EventRecord, EventStore, PersistedEvent};

/// `PlanModeController` 适配器（共享 Runtime 的 `plan_state` + `events`）。
///
/// 由 `Runtime::plan_controller` 构造，注入到 `plan.exit` 工具。`plan.exit` 通过
/// 它读写会话级 Plan 状态。设计为独立结构而非 `Runtime impl PlanModeController`，
/// 避免给 Runtime 增加无关方法（`Arc<dyn PlanModeController>` 更显式）。
///
/// Event Sourcing：持有 `event_store`/`event_seq`/`durable_seq`/`session_id`，
/// `exit_plan`/`set_mode` 触发 `PermissionModeChanged` 时同步持久化到事件流
/// （replay 时重建 `final_permission_mode`，见 `replay_session_state`）。
pub(super) struct PlanControllerHandle {
    pub(super) state: Arc<RwLock<PlanModeSnapshot>>,
    pub(super) events: EventBus,
    /// Event Sourcing 持久化字段（与 Runtime 共享 Arc）。
    pub(super) session_id: SessionId,
    pub(super) event_store: Arc<dyn EventStore>,
    pub(super) event_seq: Arc<TokioMutex<u64>>,
    pub(super) durable_seq: Arc<TokioMutex<u64>>,
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
