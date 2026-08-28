//! turn 收尾事件排空（FE-R6-2，2026-08-28 R6 审查）。
//!
//! Runtime 发 `TurnEnd` 经 `EventBus` → sequencer task → broadcast 两跳才到达
//! 订阅端；`JoinHandle` 完成与 sequencer 的 send 之间无排序保证——turn 完成后
//! 一次性 `try_recv` drain 可能漏掉仍在途中的尾事件（NDJSON 客户端依赖
//! `TurnEnd` 判定轮次结束，丢失即挂起等待）。`drain_turn_tail` 在 turn 完成后
//! 以短超时持续 recv：收到 `TurnEnd` 即返回（其后无本 turn 事件）；超时则尽力
//! 排空后返回（失败/异常路径不保证有 `TurnEnd`，不硬等）。

use minicoding_protocol::EventKind;
use std::time::Duration;
use tokio::sync::broadcast;

/// 排空 turn 完成后的尾事件，返回按序收集的事件列表。
///
/// `grace` 为最长等待时间（sequencer 在进程内，正常路径为微秒级；500ms 覆盖
/// 极端调度）。返回后调用方按原顺序转发。
pub async fn drain_turn_tail(
    rx: &mut broadcast::Receiver<(u64, EventKind)>,
    grace: Duration,
) -> Vec<(u64, EventKind)> {
    let deadline = tokio::time::Instant::now() + grace;
    let mut items = Vec::new();
    loop {
        tokio::select! {
            item = rx.recv() => {
                match item {
                    Ok((seq, kind)) => {
                        let is_end = matches!(kind, EventKind::TurnEnd { .. });
                        items.push((seq, kind));
                        if is_end {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "drain_turn_tail lagged, skipping batch");
                    }
                }
            }
            () = tokio::time::sleep_until(deadline) => break,
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_when_turn_end_received() {
        use minicoding_core::model::StopReason;
        let (tx, mut rx) = broadcast::channel(64);
        tx.send((
            1,
            EventKind::TurnEnd {
                stop_reason: StopReason::EndTurn,
            },
        ))
        .unwrap();
        let items = drain_turn_tail(&mut rx, Duration::from_secs(1)).await;
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0].1, EventKind::TurnEnd { .. }));
    }

    #[tokio::test]
    async fn returns_empty_on_timeout() {
        let (_tx, mut rx) = broadcast::channel::<(u64, EventKind)>(64);
        let items = drain_turn_tail(&mut rx, Duration::from_millis(50)).await;
        assert!(items.is_empty());
    }
}
