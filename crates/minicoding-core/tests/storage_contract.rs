//! M-13（R-09）：`Storage` 契约测试——内存后端。
//!
//! `InMemoryStorage` 与 JSONL 后端（`minicoding-storage` 集成测试）运行
//! `testing::storage_contract` 的同一套断言，保证后端可替换时上层行为不变。

#[allow(dead_code)] // 本目标只用 InMemoryStorage，common 其余工具未引用
mod common;

use std::sync::Arc;

use common::InMemoryStorage;
use minicoding_core::storage::Storage;

#[tokio::test]
async fn in_memory_storage_satisfies_contract() {
    let storage: Arc<dyn Storage> = Arc::new(InMemoryStorage::new());
    minicoding_core::testing::storage_contract::run_all(&storage).await;
}
