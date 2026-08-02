//! `RehydrateRequired` 信号（broadcast 溢出时通知客户端重拉 snapshot）。
//!
//! 当 broadcast channel 溢出（前端消费慢于生产）且事件已从 ring buffer evict 时，
//! Server 发 `RehydrateRequired` 通知客户端"事件流已不完整，请重拉 snapshot"。
//! 客户端收到后调用 `GetSession` 拉取当前完整状态重建本地视图。

use serde::{Deserialize, Serialize};

/// Rehydrate 信号（作为特殊事件推送给客户端）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RehydrateRequired {
    /// 会话 ID。
    pub session_id: String,
    /// 最后可确认的 seq（客户端重连时不应使用此 seq 之后的本地状态）。
    pub last_known_seq: u64,
    /// 原因说明。
    pub reason: String,
}

impl RehydrateRequired {
    /// 构造 Rehydrate 信号。
    #[must_use]
    pub fn new(session_id: impl Into<String>, last_known_seq: u64) -> Self {
        Self {
            session_id: session_id.into(),
            last_known_seq,
            reason: "broadcast channel overflow, events evicted from ring buffer".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn rehydrate_serialization() {
        let r = RehydrateRequired::new("01JTEST", 42);
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"session_id\":\"01JTEST\""));
        assert!(json.contains("\"last_known_seq\":42"));
    }
}
