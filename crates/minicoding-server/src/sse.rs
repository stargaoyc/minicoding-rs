//! SSE 流（Server-Sent Events，T-M8-2）。
//!
//! 把 `EventBus` 的 `Event` 转为 SSE 格式（`id:`/`event:`/`data:`），
//! 支持 `Last-Event-ID` header cursor 恢复。
//!
//! SSE 协议格式：
//! ```text
//! id: 42
//! event: token
//! data: {"text":"hello"}
//!
//! ```
//!
//! cursor 恢复流程（见 `design.md` §25.5）：
//! 1. 客户端连接时携带 `Last-Event-ID: <seq>` header；
//! 2. **内存 ring buffer 命中**：Server 从 `EventCursor` 重放 `seq+1..` 的事件；
//! 3. **durable recovery**：若 `seq` 已从 ring buffer evict 但 ≤ `durable_seq`，
//!    Server 从 `EventStore::load_after(seq)` 重放持久化事件（仅状态变更事件子集，
//!    瞬态事件如 `Token` 不可恢复，客户端应容忍缺失）；
//! 4. **不可恢复**：`seq` > `durable_seq`（或 `EventStore` 为 `NoopEventStore`），
//!    发 `RehydrateRequired` 后关闭流（E-14）；
//! 5. 重放完毕后订阅 `EventBus` 推送新事件。

use crate::session_mgr::ServerSession;
use minicoding_protocol::event::EventKind;
use minicoding_protocol::rehydrate::RehydrateRequired;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{StreamExt, wrappers::ReceiverStream};

/// 解析 `Last-Event-ID` header 为 seq。
#[must_use]
pub fn parse_last_event_id(header: Option<&str>) -> u64 {
    header
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// 构造单条 SSE 事件块（`id:`/`event:`/`data:` + 空行终止符）。
fn format_sse_event(seq: u64, kind_json: &serde_json::Value) -> String {
    let kind_str = kind_json
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("event");
    let data = serde_json::to_string(kind_json).unwrap_or_default();
    format!("id: {seq}\nevent: {kind_str}\ndata: {data}\n\n")
}

/// 构造 `RehydrateRequired` SSE 事件块。
fn format_rehydrate(session_id: &str, last_known_seq: u64) -> String {
    let rehydrate = RehydrateRequired::new(session_id, last_known_seq);
    let payload = serde_json::to_string(&rehydrate).unwrap_or_default();
    format!("id: 0\nevent: rehydrate_required\ndata: {payload}\n\n")
}

/// 构造 SSE 流。
///
/// 1. 从 `session.replay_after(last_seq)` 重放历史事件（若 `last_seq` 已 evict，
///    先发 `RehydrateRequired` 再关闭流）；
/// 2. 订阅 `session.runtime.events()` 推送新事件；
/// 3. `BroadcastStream` `Lagged` 时发 `RehydrateRequired`（事件已丢失，客户端应重拉 snapshot）。
///
/// 返回 `ReceiverStream<String>`，每条 item 是完整的 SSE 事件块。
///
/// 内部 spawn 一个 tokio task 驱动事件流，task 结束时关闭 channel（流终止）。
pub fn sse_stream(
    session: Arc<ServerSession>,
    last_seq: u64,
) -> ReceiverStream<Result<String, Infallible>> {
    let (tx, rx) = mpsc::channel::<Result<String, Infallible>>(64);

    tokio::spawn(async move {
        // 1. 重放历史事件
        let replay = session.replay_after(last_seq).await;
        match replay {
            None => {
                // last_seq 已 evict，发 RehydrateRequired 后关闭
                let _ = tx
                    .send(Ok(format_rehydrate(session.session_id(), last_seq)))
                    .await;
                return;
            }
            Some(events) => {
                for (seq, kind_json) in events {
                    let sse = format_sse_event(seq, &kind_json);
                    if tx.send(Ok(sse)).await.is_err() {
                        // 客户端断连，停止推送
                        return;
                    }
                }
            }
        }

        // 2. 订阅 EventBus 推送新事件
        push_new_events(session, tx).await;
    });

    ReceiverStream::new(rx)
}

/// 构造 **只推新事件** 的 SSE 流（首次连接，无 `Last-Event-ID`）。
///
/// 背景：若按 `sse_stream` 从 seq 0 重放，连接建立前的历史事件（如已决的
/// `permission_requested`/`permission_resolved`）会被重新推给前端，导致弹窗
/// 覆盖错位（新请求的权限弹窗被历史 pid 顶掉，审批悬空直到 300s 超时）。
/// 首次连接直接订阅 EventBus，只收连接建立之后的事件；断线重连由浏览器
/// `Last-Event-ID` 走 `sse_stream` 的恢复路径（不丢事件）。
pub fn sse_live(session: Arc<ServerSession>) -> ReceiverStream<Result<String, Infallible>> {
    let (tx, rx) = mpsc::channel::<Result<String, Infallible>>(64);

    tokio::spawn(async move {
        push_new_events(session, tx).await;
    });

    ReceiverStream::new(rx)
}

/// 订阅 `EventBus` 并把新事件转为 SSE 块，直到客户端断连（tx send 失败）。
async fn push_new_events(
    session: Arc<ServerSession>,
    tx: mpsc::Sender<Result<String, Infallible>>,
) {
    let event_rx = session.runtime.events().subscribe();
    let mut stream = BroadcastStream::new(event_rx);

    while let Some(item) = stream.next().await {
        match item {
            Ok(event) => {
                let seq = session.push_event(&event).await;
                let kind = EventKind::from(&event);
                let kind_json = serde_json::to_value(&kind).unwrap_or(serde_json::Value::Null);
                let sse = format_sse_event(seq, &kind_json);
                if tx.send(Ok(sse)).await.is_err() {
                    // 客户端断连
                    break;
                }
            }
            Err(_lagged) => {
                // broadcast 溢出——发 RehydrateRequired
                let _ = tx.send(Ok(format_rehydrate(session.session_id(), 0))).await;
                // 继续推送（客户端收到 RehydrateRequired 后自行决定是否重拉 snapshot）
            }
        }
    }
}
