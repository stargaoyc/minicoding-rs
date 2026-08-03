//! `memory.write` 工具：显式"记住 X"，写入长期记忆或 Auto memory。
//!
//! 设计要点（见 `design.md` §8.7、`docs/rules.md` C-23/C-27）：
//! - **路由**：`target: "long_term"` → 全量覆盖 `long_term.md`（经 `Ask` 权限）；
//!   `target: "auto"` → 追加条目到 `auto.md`（默认 `Allow`，指令性内容降级 `Ask`）；
//! - **物理隔离**：`long_term` 与 `auto` 独立存储（C-27），由注入的具体实现保证；
//! - **`SideEffect::FileWrite`**：统一走文件写入权限路径，由 `BuiltinPolicy`
//!   按 `target` 与内容模式细分 `Allow`/`Ask`。
//!
//! ## 存储抽象
//!
//! 与 `task` 模块的 `TaskStore` 同构：`long_term` 复用 `core::memory::MemoryStore`
//! trait；`auto` 由本模块定义 `AutoMemoryWriter` trait，默认提供 `InMemoryAutoMemory`。
//! Runtime 可注入持久化实现（来自 `minicoding-memory`）。

mod write;

pub use write::{
    AutoMemoryWriter, InMemoryAutoMemory, MemoryCategory, MemoryWrite, MemoryWriteTarget,
};

use minicoding_core::tool::ToolRegistry;

/// 注册 `memory.write` 工具到 `registry`。
///
/// 使用 `InMemoryAutoMemory` 作为 Auto memory 默认存储（非持久化）；
/// `long_term` 存储由调用方在构造 `MemoryWrite` 时注入。
/// Runtime 若需注入持久化实现，可直接用 `MemoryWrite::new(long_term, auto)` 构造。
pub fn register_memory_tools(
    registry: &mut ToolRegistry,
    long_term: std::sync::Arc<dyn minicoding_core::memory::MemoryStore>,
) {
    let auto: std::sync::Arc<dyn AutoMemoryWriter> = std::sync::Arc::new(InMemoryAutoMemory::new());
    registry.register(std::sync::Arc::new(MemoryWrite::new(long_term, auto)));
}

#[cfg(test)]
mod tests {
    //! `register_memory_tools` 注册测试（覆盖率补全）。

    use super::*;
    use minicoding_core::memory::MemoryStore;
    use minicoding_core::model::MemoryError;
    use minicoding_core::provider::BoxFuture;
    use time::OffsetDateTime;

    /// 测试用空 `MemoryStore`：load 返回空串，save 返回 Ok。
    struct StubMemoryStore;

    impl MemoryStore for StubMemoryStore {
        fn load(&self) -> BoxFuture<'_, Result<String, MemoryError>> {
            Box::pin(async move { Ok(String::new()) })
        }
        fn save(&self, _content: &str) -> BoxFuture<'_, Result<(), MemoryError>> {
            Box::pin(async move { Ok(()) })
        }
        fn last_mtime(&self) -> Option<OffsetDateTime> {
            None
        }
    }

    #[test]
    fn register_memory_tools_registers_single_tool() {
        let mut registry = ToolRegistry::new();
        let store: std::sync::Arc<dyn MemoryStore> = std::sync::Arc::new(StubMemoryStore);
        register_memory_tools(&mut registry, store);
        assert!(registry.get("memory.write").is_some());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn register_memory_tools_with_empty_store_does_not_panic() {
        let mut registry = ToolRegistry::new();
        let store: std::sync::Arc<dyn MemoryStore> = std::sync::Arc::new(StubMemoryStore);
        register_memory_tools(&mut registry, store);
        // 仅验证注册成功且工具可访问，不调用 execute（避免触发权限/IO）
        let tool = registry
            .get("memory.write")
            .expect("memory.write should be registered");
        assert_eq!(tool.name(), "memory.write");
        assert_eq!(
            tool.side_effect(),
            minicoding_core::model::SideEffect::FileWrite
        );
    }

    // ---- StubMemoryStore 方法调用覆盖 ----

    #[tokio::test]
    async fn stub_memory_store_load_returns_empty() {
        let store = StubMemoryStore;
        let content = store.load().await.expect("load 应成功");
        assert!(content.is_empty());
    }

    #[tokio::test]
    async fn stub_memory_store_save_succeeds() {
        let store = StubMemoryStore;
        store.save("test content").await.expect("save 应成功");
    }

    #[test]
    fn stub_memory_store_last_mtime_is_none() {
        let store = StubMemoryStore;
        assert!(store.last_mtime().is_none());
    }
}
