//! 事件溯源（A-2026-08 自 rt.rs 抽出，见 `design.md` §25）。
//!
//! seq 分配 / `EventStore` 追加 / `durable_seq` 推进 / 周期 snapshot 的完整
//! 持久化闭环。`persist_event` 的 best-effort 语义（失败仅 warn）与消息主链路
//! "`storage.append` → `ctx.append` → `persist_event` → `events.emit`" 不变量在此收口。
//!
//! ## 消息/事件双写对账（ST-4 决策记录，2026-08-28 R5 收尾）
//!
//! 消息日志（`Storage`，人读）与事件流（`EventStore`，溯源）**双写无对账**：
//! 瞬时 IO 失败只影响事件流（best-effort），resume（读消息日志）与 replay
//! （读事件流）可能产出不同内容且无校验。取舍：引入对账需在每次 append 后
//! 比对两侧（双写主链路的额外 IO/复杂度），而单写者的 `seq` 严格单调（ST-1
//! 修复后无缺口）+ 消息日志为权威人读源——分歧仅在事件流落盘失败的极短窗口
//! 出现，`--replay` 面向审计/回放场景，接受此差异（以事件流为准，缺的事件
//! 由 `persist_event` 失败日志可追踪）。若未来需要强一致，在 `init_event_stream`
//! 增加"消息数 vs 事件数"启动对账（列入 roadmap）。

use super::Event;
use super::rt::Runtime;
use crate::model::RuntimeError;
use crate::storage::{
    EventRecord, PersistedEvent, SNAPSHOT_INTERVAL, SessionSnapshot, SessionState, try_persist,
};
use std::sync::atomic::Ordering;

impl Runtime {
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
            // RT-8（2026-08-26 R3 审查）：无 snapshot 的恢复会话此前 durable_seq
            // 保持 0——事件实际已持久化到 `next_seq-1`，SSE durable 恢复判断
            // `last_seq <= durable_seq` 永假，断线重连退化为全量 RehydrateRequired。
            // 以已持久化最大 seq 为基线。
            if snapshot.is_none() && next_seq > 1 {
                *self.durable_seq.lock().await = next_seq - 1;
            }
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

    /// 返回下一个待分配的事件 seq（`init_event_stream` 后 = 持久化最大 seq + 1）。
    ///
    /// FE-1（2026-08-25 R2 审查）：server 懒恢复会话据此播种 SSE cursor，
    /// 使新事件的 seq 与重启前持久化记录连续，断线重连跨重启可恢复。
    #[must_use]
    pub async fn next_event_seq(&self) -> u64 {
        *self.event_seq.lock().await
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
    ///
    /// ST-1（2026-08-27 R5 审查）：append 失败时**回滚已分配的 seq**——此前
    /// seq 先递增后落盘失败（如磁盘满），留下持久化缺口；`replay_session_state`
    /// 要求严格连续 seq，一次瞬时 IO 故障即让该会话 `--replay` 永久报废且无
    /// 自愈路径。回滚后下一次 `persist_event` 重试同一 seq（单 turn 串行调用，
    /// 无并发抢号），缺口不再产生。`durable_seq` 仅在成功后更新，语义不变。
    pub(crate) async fn persist_event(&self, event: &Event) {
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
                "event persist failed (best-effort, rollback seq)"
            );
            // ST-1：回滚 seq——避免持久化缺口把 `--replay` 永久报废
            // （append 单次原子写，失败即未落盘；单 turn 串行无并发抢号）。
            let mut guard = self.event_seq.lock().await;
            if *guard > seq {
                *guard = seq;
            }
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
    ///
    /// FE-7（2026-08-25 R2 审查遗留）：同时持久化会话安全上下文——
    /// `plan_state.mode`（serde `snake_case` 字符串）与 `sandbox_policy.preset_tag()`，
    /// 供重启恢复时还原权限语义（见 server `restore_session`/CLI `--resume`）。
    async fn create_snapshot(&self, seq: u64) {
        let ctx_snap = self.ctx.snapshot().await;
        // 安全上下文：mode 用 serde 规范序列化（rename_all = "snake_case"），
        // 与恢复侧 `from_value` 互逆；preset 只存类别标识不含路径参数
        let mode = self.plan_state.read().await.mode;
        let permission_mode = serde_json::to_value(mode)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string));
        let state = SessionState {
            id: self.session.id.clone(),
            created_at: self.session.created_at,
            workdir: self.session.workdir.to_string(),
            config_hash: self.session.config_hash,
            messages: ctx_snap.messages,
            permission_mode,
            sandbox_preset: Some(self.sandbox_policy.preset_tag().to_string()),
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
}
