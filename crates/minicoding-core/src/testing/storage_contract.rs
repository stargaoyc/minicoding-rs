//! `Storage` 后端契约测试（M-13，R-09）。
//!
//! 所有 `Storage` 实现（内存 mock / JSONL / 未来 SQLite）必须通过同一套断言，
//! 保证后端可替换时上层行为不变（里氏替换的测试表达）。契约只断言**通用语义**：
//!
//! - `append` → `load` 往返一致（顺序保持）；
//! - `load` 不存在的会话返回空列表（不报错）；
//! - `list_sessions` 报告会话元数据（`message_count`/`summary`）；
//! - `update_summary` 后 `list_sessions` 可见摘要；
//! - `delete` 移除会话且幂等（delete 不存在的会话返回 Ok）；
//! - 同会话并发 `append` 不丢消息（M-01 语义：全部消息可读回）。
//!
//! 文件后端特有语义（坏行容错、格式版本拒绝、header 行为）不属于本契约，
//! 由各后端自己的测试覆盖（见 `minicoding-storage` 集成测试）。

use std::sync::Arc;

use crate::model::{Message, SessionId};
use crate::storage::{SessionListItem, Storage};

use futures::StreamExt;
use futures::stream;

/// 每个契约用例使用独立会话 id，避免后端残留状态互相干扰。
fn fresh_session(label: &str) -> SessionId {
    format!("contract-{label}-{}", ulid::Ulid::new())
}

/// 契约 1：append → load 往返一致（顺序保持）。
///
/// # Panics
/// 任一断言失败时 panic（测试辅助函数语义）。
pub async fn append_load_roundtrip(s: &dyn Storage) {
    let sid = fresh_session("roundtrip");
    let m1 = Message::user_text("first");
    let m2 = Message::user_text("second");
    s.append(&sid, &m1).await.expect("append m1");
    s.append(&sid, &m2).await.expect("append m2");

    let loaded = s.load(&sid).await.expect("load");
    assert_eq!(loaded.len(), 2, "应读回 2 条消息");
    assert_eq!(loaded[0].id, m1.id, "顺序应保持（先 append 先读出）");
    assert_eq!(loaded[1].id, m2.id);
}

/// 契约 2：load 不存在的会话返回空列表（不报错）。
///
/// # Panics
/// 任一断言失败时 panic。
pub async fn load_nonexistent_returns_empty(s: &dyn Storage) {
    let sid = fresh_session("missing");
    let loaded = s.load(&sid).await.expect("load nonexistent 应 Ok");
    assert!(
        loaded.is_empty(),
        "不存在的会话应返回空列表: {}",
        loaded.len()
    );
}

/// 契约 3：`list_sessions` 报告会话元数据（`message_count` 准确）。
///
/// # Panics
/// 任一断言失败时 panic。
pub async fn list_sessions_reports_metadata(s: &dyn Storage) {
    let sid = fresh_session("list");
    for i in 0..3 {
        s.append(&sid, &Message::user_text(format!("msg{i}")))
            .await
            .expect("append");
    }
    let metas = s.list_sessions().await.expect("list_sessions");
    let meta = metas
        .iter()
        .find(|m| m.id == sid)
        .unwrap_or_else(|| panic!("list_sessions 应包含刚追加的会话 {sid}"));
    assert_eq!(meta.message_count, 3, "message_count 应为 3");
}

/// 契约 4：`update_summary` 后 `list_sessions` 可见摘要。
///
/// # Panics
/// 任一断言失败时 panic。
pub async fn update_summary_visible_in_list(s: &dyn Storage) {
    let sid = fresh_session("summary");
    s.append(&sid, &Message::user_text("hello"))
        .await
        .expect("append");
    s.update_summary(&sid, "会话摘要内容")
        .await
        .expect("update_summary");
    let metas: Vec<SessionListItem> = s.list_sessions().await.expect("list_sessions");
    let meta = metas
        .iter()
        .find(|m| m.id == sid)
        .unwrap_or_else(|| panic!("list_sessions 应包含会话 {sid}"));
    assert_eq!(
        meta.summary.as_deref(),
        Some("会话摘要内容"),
        "摘要应在 list 中可见"
    );
}

/// 契约 5：delete 移除会话（load 变空、list 不再包含），且幂等。
///
/// # Panics
/// 任一断言失败时 panic。
pub async fn delete_removes_and_is_idempotent(s: &dyn Storage) {
    let sid = fresh_session("delete");
    s.append(&sid, &Message::user_text("bye"))
        .await
        .expect("append");

    s.delete(&sid).await.expect("delete");
    let loaded = s.load(&sid).await.expect("load after delete");
    assert!(loaded.is_empty(), "delete 后 load 应为空");
    let metas = s.list_sessions().await.expect("list after delete");
    assert!(
        metas.iter().all(|m| m.id != sid),
        "delete 后 list 不应包含该会话"
    );
    // 幂等：删除不存在的会话返回 Ok
    s.delete(&sid).await.expect("重复 delete 应 Ok");
}

/// 契约 6（M-01）：同会话并发 append 不丢消息——N 个并发追加后 load 读回 N 条。
///
/// # Panics
/// 任一断言失败时 panic。
pub async fn concurrent_append_preserves_all(s: &Arc<dyn Storage>) {
    let sid = fresh_session("concurrent");
    let futs = (0..16).map(|i| {
        let s = Arc::clone(s);
        let sid = sid.clone();
        async move { s.append(&sid, &Message::user_text(format!("c{i}"))).await }
    });
    let results: Vec<_> = stream::iter(futs).buffer_unordered(16).collect().await;
    for r in &results {
        r.as_ref().expect("并发 append 不应失败");
    }
    let loaded = s.load(&sid).await.expect("load after concurrent appends");
    assert_eq!(loaded.len(), 16, "并发 append 后应读回全部 16 条消息");
}

/// 运行全部契约断言（后端集成测试入口）。
///
/// # Panics
/// 任一契约断言失败时 panic。
pub async fn run_all(s: &Arc<dyn Storage>) {
    append_load_roundtrip(s.as_ref()).await;
    load_nonexistent_returns_empty(s.as_ref()).await;
    list_sessions_reports_metadata(s.as_ref()).await;
    update_summary_visible_in_list(s.as_ref()).await;
    delete_removes_and_is_idempotent(s.as_ref()).await;
    concurrent_append_preserves_all(s).await;
}
