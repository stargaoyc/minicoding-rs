//! SSE 流（Server-Sent Events，T-M8-2）。
//!
//! 把 `EventBus` 的 `Event` 转为 SSE 格式（`id:`/`data:`），
//! 支持 `Last-Event-ID` header cursor 恢复。
//!
//! 事件类型约定：**不使用 `event:` 命名事件字段**，所有事件走默认
//! `message` 类型（浏览器 `EventSource.onmessage` 即可接收）。若发送
//! `event: <kind>` 命名事件，浏览器只触发对应的 `addEventListener(kind)`
//! 回调，`onmessage` 收不到——曾导致前端 token/工具/权限事件全部丢失
//! （权限弹窗不出现 → 300s 超时静默 Deny）。事件类型由 `data` JSON 的
//! `type` 字段区分。
//!
//! SSE 协议格式：
//! ```text
//! id: 42
//! data: {"type":"token","text":"hello"}
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
//! 5. 重放完毕后从已带 seq 的实时通道推送新事件。
//!
//! **seq 单一写者**（2026-08-25 审查 F-seq）：本模块不再调用 `push_event`
//! 分配 seq——此前每个 SSE 连接对同一事件重复分配 seq，导致 ring buffer 中
//! 同一事件出现多份、多客户端 seq 漂移、断线重放重复。订阅端统一从
//! `subscribe_sequenced()`（单一写者 = 会话常驻 sequencer task，见
//! `session_mgr.rs`）/ `replay_after`（ring buffer → durable → Rehydrate）
//! 读取已带 seq 的事件。

use crate::session_mgr::ServerSession;
use minicoding_protocol::event::EventKind;
use minicoding_protocol::rehydrate::RehydrateRequired;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// 解析 `Last-Event-ID` header 为 seq。
///
/// 返回 `None` 表示 header 缺失或**畸形**（非数字）。R8 FE-14：畸形值此前
/// 回退 `0` 触发全量重放——历史 `permission_requested` 事件重放会让前端弹窗
/// pid 错乱（与首次连接 `sse_live` 想避免的场景同源）。畸形 header 视为
/// "无 cursor"，由调用方走 `sse_live`（只推新事件）。
#[must_use]
pub fn parse_last_event_id(header: Option<&str>) -> Option<u64> {
    header?.trim().parse::<u64>().ok()
}

/// 构造单条 SSE 事件块（`id:`/`data:` + 空行终止符）。
///
/// 不发送 `event:` 命名事件字段：浏览器 `onmessage` 只能收到默认
/// `message` 类型事件，命名事件会静默丢失（见模块注释）。
///
/// R8 FE-8：`data:` 载荷除 `EventKind` 外**注入 `seq` 字段**——`EventDto`
/// 类型声明 `seq` 必填，但此前 seq 仅在 `id:` 字段（前端 `dto.seq` 恒
/// `undefined`，与生成类型契约漂移）。`id:` 仍保留（浏览器 `EventSource`
/// 用 `Last-Event-ID` 自动重连恢复），`data:` 补 seq 使载荷与 `EventDto`
/// 序列化形态一致，前端可直接消费 cursor。
fn format_sse_event(seq: u64, kind_json: &serde_json::Value) -> String {
    let mut data = kind_json.clone();
    if let Some(obj) = data.as_object_mut() {
        obj.insert("seq".to_string(), serde_json::json!(seq));
    }
    let data = serde_json::to_string(&data).unwrap_or_default();
    format!("id: {seq}\ndata: {data}\n\n")
}

/// 构造 `RehydrateRequired` SSE 事件块。
///
/// FE-3（2026-08-26 R3 审查）：SSE `id:` 必须携带**当前实际 seq** 而非 0——
/// 固定 `id: 0` 会让浏览器 `EventSource` 自动重连时回传 `Last-Event-ID: 0`，
/// 服务端从 ring buffer 全量重放历史（已决权限弹窗错位复活），cursor 为空
/// 时更形成 Rehydrate→重连→Rehydrate 的无限循环。
fn format_rehydrate(session_id: &str, last_known_seq: u64, current_seq: u64) -> String {
    let rehydrate = RehydrateRequired::new(session_id, last_known_seq);
    let payload = serde_json::to_string(&rehydrate).unwrap_or_default();
    format!("id: {current_seq}\ndata: {payload}\n\n")
}

/// FE-17：SSE 订阅者 guard，Drop 时递减会话的活动计数。
struct SubscriberGuard(Arc<ServerSession>);

impl Drop for SubscriberGuard {
    fn drop(&mut self) {
        self.0
            .sse_subscribers
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 构造 SSE 流。
///
/// 1. 从 `session.replay_after(last_seq)` 重放历史事件（若 `last_seq` 已 evict，
///    先发 `RehydrateRequired` 再关闭流）；
/// 2. 从已带 seq 的实时通道（`subscribe_sequenced`）推送新事件；
/// 3. 实时通道 `Lagged` 时发 `RehydrateRequired`（事件已丢失，客户端应重拉 snapshot）。
///
/// 返回 `ReceiverStream<String>`，每条 item 是完整的 SSE 事件块。
///
/// 内部 spawn 一个 tokio task 驱动事件流，task 结束时关闭 channel（流终止）。
pub fn sse_stream(
    session: Arc<ServerSession>,
    last_seq: u64,
) -> ReceiverStream<Result<String, Infallible>> {
    let (tx, rx) = mpsc::channel::<Result<String, Infallible>>(64);

    // FE-17（2026-08-28 R5 收尾）：活动订阅者计数——空闲驱逐据此跳过该会话。
    // task 结束时（流关闭/客户端断开）递减归零。计数增减在 task 边界内完成，
    // 避免流被 drop 后计数残留。
    session
        .sse_subscribers
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let session_sub = session.clone();
    let _ = &session;

    tokio::spawn(async move {
        // 确保无论何种退出路径（replay 失败/客户端断开/正常结束）都递减计数
        let _decrement = SubscriberGuard(session_sub.clone());

        // 0. **先订阅实时通道再重放**：重放快照与订阅之间到达的事件不会丢失
        //    （重复的由下方 seq 去重剔除）。
        let live_rx = session.subscribe_sequenced();

        // 1. 重放历史事件
        let replay = session.replay_after(last_seq).await;
        match replay {
            None => {
                // last_seq 已 evict，发 RehydrateRequired 后关闭
                // （FE-3：id 用 cursor 当前 seq，防 EventSource 重连风暴）
                let current = session.cursor.lock().await.current_seq();
                let _ = tx
                    .send(Ok(format_rehydrate(
                        session.session_id(),
                        last_seq,
                        current,
                    )))
                    .await;
            }
            Some(events) => {
                // 去重基准：已重放的最大 seq（无重放项时为断点本身）
                let mut floor = last_seq;
                for (seq, kind_json) in events {
                    floor = floor.max(seq);
                    let sse = format_sse_event(seq, &kind_json);
                    if tx.send(Ok(sse)).await.is_err() {
                        // 客户端断连，停止推送
                        return;
                    }
                }
                forward_live_events(session, tx, live_rx, floor).await;
            }
        }
    });

    ReceiverStream::new(rx)
}

/// 构造 **只推新事件** 的 SSE 流（首次连接，无 `Last-Event-ID`）。
///
/// 背景：若按 `sse_stream` 从 seq 0 重放，连接建立前的历史事件（如已决的
/// `permission_requested`/`permission_resolved`）会被重新推给前端，导致弹窗
/// 覆盖错位（新请求的权限弹窗被历史 pid 顶掉，审批悬空直到 300s 超时）。
/// 首次连接只收连接建立之后的事件；断线重连由浏览器 `Last-Event-ID` 走
/// `sse_stream` 的恢复路径（不丢事件）。
pub fn sse_live(session: Arc<ServerSession>) -> ReceiverStream<Result<String, Infallible>> {
    let (tx, rx) = mpsc::channel::<Result<String, Infallible>>(64);

    // FE-R6-1（2026-08-28 R6 审查）：首次连接路径此前未计入订阅者计数——
    // 开着 Web 标签页"从未断线"的会话仍可被空闲驱逐（FE-17 修复只覆盖了
    // sse_stream 重连路径）。与 sse_stream 同款 guard。
    session
        .sse_subscribers
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    tokio::spawn(async move {
        let _decrement = SubscriberGuard(session.clone());
        let live_rx = session.subscribe_sequenced();
        forward_live_events(session, tx, live_rx, 0).await;
    });

    ReceiverStream::new(rx)
}

/// 从**已带 seq** 的实时通道转发新事件到 SSE 流，直到客户端断连。
///
/// - `floor`：去重基准——`seq <= floor` 的事件已被重放路径发送过，跳过
///   （订阅先于重放建立时的窗口重叠，2026-08-25 审查 F-seq）；
/// - 不调用 `push_event`：seq 由会话常驻 sequencer task 单一分配；
/// - Lagged 时发 `RehydrateRequired` 并继续（客户端自行决定是否重拉 snapshot）。
async fn forward_live_events(
    session: Arc<ServerSession>,
    tx: mpsc::Sender<Result<String, Infallible>>,
    mut rx: tokio::sync::broadcast::Receiver<(u64, EventKind)>,
    floor: u64,
) {
    let mut floor = floor;
    loop {
        match rx.recv().await {
            Ok((seq, kind)) => {
                if seq <= floor {
                    continue; // 重放窗口重叠，幂等去重
                }
                floor = seq;
                let kind_json = serde_json::to_value(&kind).unwrap_or(serde_json::Value::Null);
                let sse = format_sse_event(seq, &kind_json);
                if tx.send(Ok(sse)).await.is_err() {
                    // 客户端断连
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                // broadcast 溢出——发 RehydrateRequired（FE-3：id 填实际 seq）
                let current = session.cursor.lock().await.current_seq();
                let _ = tx
                    .send(Ok(format_rehydrate(session.session_id(), 0, current)))
                    .await;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn format_sse_event_includes_seq_in_data() {
        // R8 FE-8：data 载荷须含 seq（EventDto 契约），id: 字段保留供
        // EventSource Last-Event-ID 重连。
        let kind = serde_json::json!({"type": "token", "text": "hi"});
        let block = format_sse_event(42, &kind);
        assert!(block.starts_with("id: 42\n"), "{block}");
        let data_line = block.lines().find(|l| l.starts_with("data: ")).unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(data_line.trim_start_matches("data: ")).unwrap();
        assert_eq!(payload["seq"], 42, "data 载荷应含 seq: {payload}");
        assert_eq!(payload["type"], "token");
        assert_eq!(payload["text"], "hi");
        // 序列化形态与 EventDto（seq + flatten kind）一致
        assert!(payload.as_object().unwrap().contains_key("seq"));
    }

    #[test]
    fn parse_last_event_id_returns_none_for_malformed() {
        // R8 FE-14：畸形 header 视为无 cursor（走 sse_live），不回退 0
        assert_eq!(parse_last_event_id(None), None);
        assert_eq!(parse_last_event_id(Some("42")), Some(42));
        assert_eq!(parse_last_event_id(Some(" 7 ")), Some(7));
        assert_eq!(parse_last_event_id(Some("abc")), None);
        assert_eq!(parse_last_event_id(Some("")), None);
    }
}
