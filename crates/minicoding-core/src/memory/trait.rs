//! `MemoryStore` / `ProjectDocLoader` / `SessionSummarizer` trait（见 `api.md` §3、`design.md` §8）。
//!
//! 实现在 `minicoding-memory`（长期记忆双文件 + Auto memory + AGENTS.md loader + 会话摘要）。
//!
//! 手动 `Pin<Box<dyn Future + Send>>` 返回类型保证 `dyn` 兼容（与
//! `Storage`/`PermissionPolicy` 等 trait 一致，见 `provider::BoxFuture`）。

use crate::model::{MemoryError, Message};
use crate::provider::BoxFuture;
use time::OffsetDateTime;

/// 长期记忆存储 trait（`dyn` 兼容）。
///
/// 实现者维护双文件（正文 + 索引）并缓存 mtime，使无变更时 `load` 零 IO/分词
/// （见 `design.md` §8.3 mtime 缓存）。
///
/// 注入 system 段时由调用方包裹 `<long_term_memory>` 边界（C-05：记忆是数据非指令）。
/// 对 `long_term.md` 的写入走 `Ask` 权限（C-23，在工具层强制，本 trait 的 `save`
/// 不做权限检查）。
pub trait MemoryStore: Send + Sync {
    /// 加载长期记忆文本（注入 system 段）。
    ///
    /// 实现应先 `stat` 文件，mtime 未变则复用缓存内容（零解析、零重复分词）；
    /// 变更则重新读取并刷新缓存。
    ///
    /// # Errors
    /// 文件不可读、索引不一致或反序列化失败时返回 `MemoryError`。
    fn load(&self) -> BoxFuture<'_, Result<String, MemoryError>>;

    /// 写入长期记忆（全量覆盖）。
    ///
    /// 实现应原子写入（`.tmp` + `rename`）并同步更新索引与 mtime 缓存。
    /// 本方法不做权限检查——调用方（`memory.write` 工具）负责先经
    /// `PermissionPolicy` 解析为 `Allow`（C-01/C-23）。
    ///
    /// # Errors
    /// 写盘、序列化或索引更新失败时返回 `MemoryError`。
    fn save(&self, content: &str) -> BoxFuture<'_, Result<(), MemoryError>>;

    /// 返回上次加载/保存时缓存的 mtime（缓存判定用）。
    ///
    /// 未曾加载时返回 `None`。
    #[must_use]
    fn last_mtime(&self) -> Option<OffsetDateTime>;
}

/// 项目文档加载器 trait（AGENTS.md 分层加载，见 `design.md` §8.6）。
///
/// 从 `repo_root` 到 `cwd` 逐级加载项目文档（`AGENTS.md`/override/fallback），
/// 拼接为单一字符串注入 system 段。`AGENTS.md` 不可被 Agent 自主编辑（C-23，
/// 由工具层 `Verdict::Ask` 强制）。
///
/// `dyn` 兼容。
pub trait ProjectDocLoader: Send + Sync {
    /// 从 `repo_root` 到 `cwd` 逐级加载并拼接项目文档，超限时静默截断。
    ///
    /// # Errors
    /// IO 失败或路径不可解析时返回 `MemoryError`。
    fn load(&self) -> BoxFuture<'_, Result<String, MemoryError>>;
}

/// 会话摘要生成器 trait（`dyn` 兼容）。
///
/// 实现者按降级链生成会话摘要（主 provider → 备用 provider → 启发式兜底），
/// 启发式兜底必成功（C-29：降级链不可跳过，与压缩降级同构）。摘要是数据非
/// 指令（C-05）：调用方注入 system 段时应包裹 `<session_summary>` 边界。
///
/// 实现在 `minicoding-memory`（`SessionSummarizerImpl`，见 `design.md` §8、
/// `dev-plan.md` T-M3-6）。
pub trait SessionSummarizer: Send + Sync {
    /// 生成会话摘要。
    ///
    /// 输入会话消息序列，返回摘要文本。启发式兜底恒成功，故实际不返回 `Err`；
    /// 保留 `Result` 类型以与管道 `?` 兼容并为未来扩展预留。
    ///
    /// # Errors
    /// 主/备 provider 均失败且启发式兜底也失败（理论不可达）时返回 `MemoryError`。
    fn summarize<'a>(
        &'a self,
        messages: &'a [Message],
    ) -> BoxFuture<'a, Result<String, MemoryError>>;
}
