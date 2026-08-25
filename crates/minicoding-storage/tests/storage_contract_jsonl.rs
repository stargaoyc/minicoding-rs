//! M-13（R-09）：`Storage` 契约测试——JSONL 后端 + 版本拒绝。
//!
//! 与内存后端（`minicoding-core/tests/storage_contract.rs`）运行
//! `testing::storage_contract` 的同一套断言；文件后端特有语义（坏行容错、
//! 更高格式版本显式拒绝）在此补充 trait 级集成覆盖。

use std::sync::Arc;

use camino::Utf8PathBuf;
use minicoding_core::model::{Message, StorageError};
use minicoding_core::storage::Storage;
use minicoding_storage::JsonlStorage;

fn temp_storage() -> (tempfile::TempDir, JsonlStorage) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let storage = JsonlStorage::new(
        Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 tempdir"),
    );
    (dir, storage)
}

#[tokio::test]
async fn jsonl_storage_satisfies_contract() {
    let (_dir, storage) = temp_storage();
    let storage: Arc<dyn Storage> = Arc::new(storage);
    minicoding_core::testing::storage_contract::run_all(&storage).await;
}

/// M-02/M-13：更高格式版本的会话文件在 `load` 显式拒绝（`FormatUnsupported`），
/// 防止把新版文件当旧数据静默截断。
#[tokio::test]
async fn load_rejects_future_format_version_via_trait() {
    let (dir, storage) = temp_storage();
    let sid = "contract-future-version".to_string();
    let path = dir.path().join(format!("{sid}.jsonl"));
    std::fs::write(
        &path,
        "{\"_header\":{\"format_version\":999,\"app\":\"minicoding\",\"app_version\":\"test\"}}\n{\"id\":\"m1\",\"role\":\"user\",\"content\":[],\"tool_calls\":[],\"created_at\":\"2026-01-01T00:00:00Z\",\"metadata\":{}}\n",
    )
    .expect("write future-version file");

    let err = storage.load(&sid).await.expect_err("更高版本应显式拒绝");
    assert!(
        matches!(err, StorageError::FormatUnsupported(_)),
        "应返回 FormatUnsupported: {err:?}"
    );
}

/// M-02/M-13：scan `路径（list_sessions）跳过更高版本会话，不索引新版数据`。
#[tokio::test]
async fn list_sessions_skips_future_format_version() {
    let (dir, storage) = temp_storage();
    // 先正常 append 一个会话（进索引）
    let ok_sid = "contract-ok".to_string();
    storage
        .append(&ok_sid, &Message::user_text("fine"))
        .await
        .expect("append ok session");
    // 手工放一个更高版本文件（不经过 append，模拟新版客户端写入）
    let bad_sid = "contract-future-scan".to_string();
    let path = dir.path().join(format!("{bad_sid}.jsonl"));
    std::fs::write(
        &path,
        "{\"_header\":{\"format_version\":999,\"app\":\"minicoding\",\"app_version\":\"test\"}}\n",
    )
    .expect("write future-version file");

    let metas = storage.list_sessions().await.expect("list_sessions");
    assert!(metas.iter().any(|m| m.id == ok_sid), "正常会话应在列表中");
    assert!(
        metas.iter().all(|m| m.id != bad_sid),
        "更高版本会话应被 scan 跳过"
    );
}

/// M-02/M-13：单坏行容错——坏行跳过、好行保留（trait 级集成覆盖）。
#[tokio::test]
async fn load_tolerates_single_corrupted_line() {
    let (dir, storage) = temp_storage();
    let sid = "contract-bad-line".to_string();
    let good = serde_json::to_string(&Message::user_text("good line")).expect("serialize");
    let path = dir.path().join(format!("{sid}.jsonl"));
    std::fs::write(
        &path,
        format!(
            "{{\"_header\":{{\"format_version\":1,\"app\":\"minicoding\",\"app_version\":\"test\"}}}}\n{good}\n{{broken json\n"
        ),
    )
    .expect("write mixed file");

    let loaded = storage.load(&sid).await.expect("load 应容错");
    assert_eq!(loaded.len(), 1, "坏行应被跳过，好行保留");
    assert_eq!(loaded[0].content.len(), 1);
}
