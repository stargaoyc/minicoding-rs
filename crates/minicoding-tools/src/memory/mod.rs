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
