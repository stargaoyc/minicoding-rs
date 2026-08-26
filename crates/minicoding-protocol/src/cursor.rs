//! SSE cursor 恢复（event seq 单调递增）。
//!
//! 客户端断连后从 `last_seq` 恢复：Server 检查 `last_seq` 是否仍在内存 ring
//! buffer 中，命中则从 `last_seq+1` 重放；未命中但 ≤ `durable_seq` 则从
//! `EventStore` 重放；否则发 `RehydrateRequired`（见 `rehydrate.rs`）。

use std::collections::VecDeque;

/// FE-1（2026-08-26 R3 审查）：重放请求的三态结果。
///
/// 旧 API 用 `Option<Vec>` 二态表达三语义——"evict 但 ≤ `durable_seq`"返回
/// `Some(vec![])`，调用方提前返回空重放，`EventStore::load_after` 成为死代码，
/// server 重启后断线期间持久化事件静默丢失。分类枚举让 durable recovery
/// 路径真正可达。
#[derive(Debug)]
pub enum ReplayOutcome<'a> {
    /// ring buffer 命中：可直接重放的事件列表（可能为空 = 无新事件）。
    Buffer(Vec<(u64, &'a serde_json::Value)>),
    /// `after_seq` 已 evict 但 ≤ `durable_seq`——调用方应从 `EventStore`
    /// 重放持久化事件。
    NeedsDurable,
    /// `after_seq` > `durable_seq`（或无持久化）——不可恢复，应发
    /// `RehydrateRequired`。
    Unrecoverable,
}

/// 事件 cursor 管理器（每会话一个，内存 ring buffer）。
///
/// 容量有限：超过 `capacity` 后丢弃最旧事件。`durable_seq` 标记已持久化的最大
/// seq，用于判断 `last_seq` 是否可从 `EventStore` 重放（M8 仅内存实现，
/// `durable_seq` 始终为 0，即不持久化）。
#[derive(Debug)]
pub struct EventCursor {
    /// ring buffer（按 seq 升序）。
    buffer: VecDeque<(u64, serde_json::Value)>,
    /// 最大容量。
    capacity: usize,
    /// 当前最大 seq（下一个事件的 seq = `next_seq`）。
    next_seq: u64,
    /// 已持久化的最大 seq（0 = 无持久化）。
    durable_seq: u64,
}

impl EventCursor {
    /// 创建 cursor 管理器。
    ///
    /// `capacity` 为 ring buffer 最大容量，超过后丢弃最旧事件。生产环境建议 ≥1024；
    /// 测试可用小值（如 2）验证 evict 行为。
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self {
            buffer: VecDeque::with_capacity(cap),
            capacity: cap,
            next_seq: 1,
            durable_seq: 0,
        }
    }

    /// 追加事件，返回分配的 seq。
    pub fn push(&mut self, event: serde_json::Value) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        if self.buffer.len() >= self.capacity {
            self.buffer.pop_front();
        }
        self.buffer.push_back((seq, event));
        seq
    }

    /// 从 `after_seq` 之后重放事件（不含 `after_seq`）。
    ///
    /// 返回 `None` 表示 `after_seq` 已从 ring buffer evict 且不可恢复
    /// （需发 `RehydrateRequired`）。
    #[must_use]
    pub fn replay_after(&self, after_seq: u64) -> Option<Vec<&serde_json::Value>> {
        // after_seq = 0 表示从头重放；否则先检查 after_seq 是否已 evict
        if after_seq > 0
            && let Some((oldest, _)) = self.buffer.front()
            && after_seq < *oldest
        {
            // 已 evict，检查是否可从 durable 恢复
            if after_seq <= self.durable_seq {
                // 理论上可从 `EventStore` 重放，但 M8 仅内存实现，返回空
                return Some(vec![]);
            }
            return None;
        }
        let start = after_seq + 1;
        let mut result = Vec::new();
        for (seq, event) in &self.buffer {
            if *seq >= start {
                result.push(event);
            }
        }
        Some(result)
    }

    /// 从 `after_seq` 之后重放事件（含 seq 编号，供 SSE `id:` 字段用，T-M8-2）。
    ///
    /// 与 `replay_after` 的区别：返回 `(seq, &Value)` 元组，SSE 流用 seq 设置
    /// `id:` 字段，客户端 `Last-Event-ID` header 据此恢复。
    ///
    /// 返回 `None` 表示 `after_seq` 已 evict 且不可恢复（需发 `RehydrateRequired`）。
    #[must_use]
    pub fn replay_after_with_seq(&self, after_seq: u64) -> Option<Vec<(u64, &serde_json::Value)>> {
        match self.classify_replay(after_seq) {
            ReplayOutcome::Buffer(v) => Some(v),
            // 兼容旧二态 API：durable 可恢复场景返回空列表
            ReplayOutcome::NeedsDurable => Some(vec![]),
            ReplayOutcome::Unrecoverable => None,
        }
    }

    /// FE-1：三态重放分类（供 server 区分 buffer/durable/unrecoverable）。
    #[must_use]
    pub fn classify_replay(&self, after_seq: u64) -> ReplayOutcome<'_> {
        // after_seq = 0 表示从头重放；否则先检查 after_seq 是否已 evict
        if after_seq > 0
            && let Some((oldest, _)) = self.buffer.front()
            && after_seq < *oldest
        {
            // 已 evict，检查是否可从 durable 恢复
            if after_seq <= self.durable_seq {
                return ReplayOutcome::NeedsDurable;
            }
            return ReplayOutcome::Unrecoverable;
        }
        let start = after_seq + 1;
        let result: Vec<(u64, &serde_json::Value)> = self
            .buffer
            .iter()
            .filter(|(seq, _)| *seq >= start)
            .map(|(seq, event)| (*seq, event))
            .collect();
        ReplayOutcome::Buffer(result)
    }

    /// 当前最大 seq。
    #[must_use]
    pub fn current_seq(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }

    /// 播种 seq 空间（FE-1，2026-08-25 R2 审查：懒恢复会话跨重启续接）。
    ///
    /// 磁盘会话恢复后调用：`persisted_seq` 为持久化事件流的最大 seq。
    /// - `next_seq` 推进到 `max(当前, persisted+1)`——此后新事件在持久化 seq
    ///   之后**连续编号**，不再从 1 重发与重启前记录撞号；
    /// - `durable_seq` 提升到 `max(当前, persisted)`——老客户端携带
    ///   `Last-Event-ID ≤ persisted` 重连时，即使内存 buffer 尚无该区间，
    ///   也正确落入 durable recovery（`EventStore::load_after`）路径，
    ///   而非被误判为不可恢复。
    ///
    /// 取 max 保证重复播种/播种晚于若干 push 的顺序安全（幂等）。
    pub fn seed(&mut self, persisted_seq: u64) {
        self.next_seq = self.next_seq.max(persisted_seq.saturating_add(1));
        self.durable_seq = self.durable_seq.max(persisted_seq);
    }

    /// 更新 durable seq（持久化完成后调用）。
    pub fn set_durable(&mut self, seq: u64) {
        self.durable_seq = seq;
    }
}

impl Default for EventCursor {
    fn default() -> Self {
        Self::new(1024)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn push_assigns_sequential_ids() {
        let mut cursor = EventCursor::new(64);
        let s1 = cursor.push(serde_json::json!({"a": 1}));
        let s2 = cursor.push(serde_json::json!({"a": 2}));
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);
        assert_eq!(cursor.current_seq(), 2);
    }

    #[test]
    fn replay_after_returns_later_events() {
        let mut cursor = EventCursor::new(64);
        cursor.push(serde_json::json!({"n": 1}));
        cursor.push(serde_json::json!({"n": 2}));
        cursor.push(serde_json::json!({"n": 3}));
        let replay = cursor.replay_after(1).unwrap();
        assert_eq!(replay.len(), 2);
    }

    #[test]
    fn replay_from_zero_returns_all() {
        let mut cursor = EventCursor::new(64);
        cursor.push(serde_json::json!({"n": 1}));
        cursor.push(serde_json::json!({"n": 2}));
        let replay = cursor.replay_after(0).unwrap();
        assert_eq!(replay.len(), 2);
    }

    #[test]
    fn evicted_old_events_unrecoverable() {
        let mut cursor = EventCursor::new(2);
        cursor.push(serde_json::json!({"n": 1}));
        cursor.push(serde_json::json!({"n": 2}));
        cursor.push(serde_json::json!({"n": 3})); // evicts seq=1
        // after_seq=1 已 evict，无 durable → None
        assert!(cursor.replay_after(1).is_none());
    }

    #[test]
    fn fe1_classify_replay_distinguishes_durable_from_unrecoverable() {
        // 场景 A：evict 且 ≤ durable → NeedsDurable（旧 API 返回 Some(vec![])，
        // 调用方提前返回空重放致 EventStore 恢复死代码）
        let mut cursor = EventCursor::new(2);
        cursor.push(serde_json::json!({"n": 1}));
        cursor.push(serde_json::json!({"n": 2}));
        cursor.push(serde_json::json!({"n": 3})); // evicts seq=1
        cursor.set_durable(2);
        assert!(matches!(
            cursor.classify_replay(1),
            ReplayOutcome::NeedsDurable
        ));
        // buffer 命中（含"无新事件"的空列表）
        assert!(matches!(
            cursor.classify_replay(0),
            ReplayOutcome::Buffer(_)
        ));
        assert!(matches!(
            cursor.classify_replay(3),
            ReplayOutcome::Buffer(_)
        ));
        // 场景 B：evict 且 > durable（此处 durable=0）→ Unrecoverable
        let mut cursor2 = EventCursor::new(2);
        cursor2.push(serde_json::json!({"n": 1}));
        cursor2.push(serde_json::json!({"n": 2}));
        cursor2.push(serde_json::json!({"n": 3})); // evicts seq=1
        assert!(matches!(
            cursor2.classify_replay(1),
            ReplayOutcome::Unrecoverable
        ));
    }
}
