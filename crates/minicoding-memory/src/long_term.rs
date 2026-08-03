//! 长期记忆双文件实现（`long_term.md` + `index.json`）+ mtime 缓存。
//!
//! 设计要点（见 `design.md` §8.2/§8.3）：
//! - **双文件**：`long_term.md`（人机共读 Markdown 正文）+ `index.json`
//!   （程序化元数据：mtime/size/hash，hash 用于变更检测的补充）；
//! - **mtime 缓存**：`load` 先 `stat` 文件，mtime 未变则直接复用缓存的正文，
//!   零解析、零重复分词（`design.md` §8.3 第 1 条）；
//! - **原子写入**：`save` 写 `.tmp` 后 `rename`，避免读到半截内容；
//! - **边界**：注入内容由 `inject::inject_memory` 包裹 `<long_term_memory>` 边界
//!   （C-05：记忆是数据非指令），本模块只负责读写正文，不负责包裹。
//!
//! C-04：凭证不入记忆——`save` 不做内容过滤（由工具层与权限层保证凭证不被写入）。
//! C-23：对 `long_term.md` 写入走 `Ask`，由工具层 `memory.write` 强制，本模块 `save`
//! 不做权限检查（trait 契约已声明）。

use crate::MemoryStore;
use camino::{Utf8Path, Utf8PathBuf};
use minicoding_core::model::MemoryError;
use minicoding_core::otel::span_name;
use minicoding_core::paths;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write;
use std::sync::{Mutex, MutexGuard, PoisonError};
use time::OffsetDateTime;
use tokio::fs;

/// 长期记忆文件名（正文）。
const LONG_TERM_FILE: &str = "long_term.md";
/// 长期记忆索引文件名。
const INDEX_FILE: &str = "index.json";
/// 原子写入临时文件后缀。
const TMP_SUFFIX: &str = ".tmp";

/// 长期记忆存储（双文件 + mtime 缓存）。
///
/// 实现 [`MemoryStore`]。`load` 在 mtime 未变时返回缓存的正文（零 IO/分词）；
/// `save` 原子写入正文与索引并刷新缓存。
pub struct LongTermMemory {
    /// 正文路径（`~/.minicoding/memory/long_term.md`）。
    path: Utf8PathBuf,
    /// 索引路径（`~/.minicoding/memory/index.json`）。
    index_path: Utf8PathBuf,
    /// 缓存的正文（命中 mtime 时直接复用）。
    cached_content: Mutex<Option<String>>,
    /// 缓存的 mtime（命中判定基准）。
    cached_mtime: Mutex<Option<OffsetDateTime>>,
}

impl LongTermMemory {
    /// 从默认 `MINICODING_HOME/memory/` 目录构造（路径由 `core::paths::memory_dir` 解析）。
    ///
    /// # Errors
    /// 当 home 目录无法确定（`MINICODING_HOME` 未设且 home 不可解析）时返回 `MemoryError`。
    pub fn new() -> Result<Self, MemoryError> {
        let dir = paths::memory_dir()?;
        Ok(Self::with_dir(&dir))
    }

    /// 从指定目录构造，正文与索引分别解析为 `dir/long_term.md` 与 `dir/index.json`。
    #[must_use]
    pub fn with_dir(dir: &Utf8Path) -> Self {
        let path = dir.join(LONG_TERM_FILE);
        let index_path = dir.join(INDEX_FILE);
        Self {
            path,
            index_path,
            cached_content: Mutex::new(None),
            cached_mtime: Mutex::new(None),
        }
    }

    /// 读取文件 mtime；文件不存在时返回 `Ok(None)`（视为首次加载，无缓存可比）。
    ///
    /// `OffsetDateTime::from(SystemTime)` 仅做时区换算；文件 mtime 恒在 UNIX epoch
    /// 之后，转换不会失败。`modified()` 在不支持 mtime 的平台返回 `Err`，走错误分支。
    async fn current_mtime(&self) -> Result<Option<OffsetDateTime>, MemoryError> {
        match fs::metadata(&self.path).await {
            Ok(md) => {
                let modified = md.modified()?;
                Ok(Some(OffsetDateTime::from(modified)))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }
}

impl Default for LongTermMemory {
    fn default() -> Self {
        // Default 走默认目录；home 不可解析时退化为相对路径（仅占位，真实使用应走 `new`）。
        let dir = paths::memory_dir().unwrap_or_else(|_| Utf8PathBuf::from("memory"));
        Self::with_dir(&dir)
    }
}

impl MemoryStore for LongTermMemory {
    #[tracing::instrument(skip(self), fields(otel.name = span_name::MEMORY_LOAD, memory.type = "long_term"))]
    fn load(&self) -> minicoding_core::provider::BoxFuture<'_, Result<String, MemoryError>> {
        Box::pin(async move {
            let current = self.current_mtime().await?;
            let cached_mtime = *guard(&self.cached_mtime);

            // mtime 未变且已有缓存：直接复用，零正文 IO/分词。
            if current == cached_mtime
                && let Some(content) = guard(&self.cached_content).clone()
            {
                return Ok(content);
            }

            // 文件不存在：视为空记忆，清空缓存。
            let Some(current) = current else {
                *guard(&self.cached_content) = None;
                *guard(&self.cached_mtime) = None;
                return Ok(String::new());
            };

            // mtime 变更或无缓存：重新读取正文。
            let content = fs::read_to_string(&self.path).await?;

            // 补充变更检测：交叉校验 index.json 的 hash。不一致仅记 warn，不阻塞加载
            // （hash 用于变更检测的补充，见任务说明与 design.md §8.3）。
            if let Ok(index_bytes) = fs::read(&self.index_path).await {
                match serde_json::from_slice::<LongTermIndex>(&index_bytes) {
                    Ok(idx) => {
                        let actual_hash = sha256_hex(content.as_bytes());
                        if idx.hash != actual_hash {
                            tracing::warn!(
                                target: "minicoding::memory",
                                index = %self.index_path,
                                "long_term memory index hash mismatch; content will be used"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "minicoding::memory",
                            error = %e,
                            "long_term memory index corrupted; ignored"
                        );
                    }
                }
            }

            // 刷新缓存。
            *guard(&self.cached_content) = Some(content.clone());
            *guard(&self.cached_mtime) = Some(current);

            Ok(content)
        })
    }

    #[tracing::instrument(skip(self), fields(otel.name = span_name::MEMORY_SAVE, memory.type = "long_term"))]
    fn save(
        &self,
        content: &str,
    ) -> minicoding_core::provider::BoxFuture<'_, Result<(), MemoryError>> {
        // 将借用参数克隆为 owned，避免 async 块跨生命周期捕获 `&str`
        // （`BoxFuture<'_>` 生命周期绑定 `&self`，borrowed content 不可跨越）。
        let content = content.to_owned();
        Box::pin(async move {
            // 确保目录存在（首次写入）。
            if let Some(parent) = self.path.parent() {
                fs::create_dir_all(parent).await?;
            }

            // 原子写入正文：写 .tmp → rename。
            let tmp_path = Utf8PathBuf::from(format!("{}{TMP_SUFFIX}", self.path.as_str()));
            fs::write(&tmp_path, &content).await?;
            fs::rename(&tmp_path, &self.path).await?;

            // 读取刚写入文件的 mtime 作为权威值（覆盖写后 mtime）。
            let mtime = self
                .current_mtime()
                .await?
                .unwrap_or_else(OffsetDateTime::now_utc);
            let index = LongTermIndex {
                mtime,
                size: content.len() as u64,
                hash: sha256_hex(content.as_bytes()),
            };
            let index_bytes = serde_json::to_vec_pretty(&index)
                .map_err(|e| MemoryError::Serialize(e.to_string()))?;

            // 原子写入索引。
            let idx_tmp = Utf8PathBuf::from(format!("{}{TMP_SUFFIX}", self.index_path.as_str()));
            fs::write(&idx_tmp, &index_bytes).await?;
            fs::rename(&idx_tmp, &self.index_path).await?;

            // 刷新缓存。
            *guard(&self.cached_content) = Some(content.clone());
            *guard(&self.cached_mtime) = Some(mtime);

            Ok(())
        })
    }

    fn last_mtime(&self) -> Option<OffsetDateTime> {
        *guard(&self.cached_mtime)
    }
}

/// 锁定缓存 `Mutex`，**忽略 poison**：前序 panic 后复用（可能过期）缓存值优于
/// 直接失败——这是缓存的合理降级语义，且避免在非测试代码使用 `expect`
/// （AGENTS.md §2.3）。`MutexGuard` 同步使用，绝不跨 `.await` 持有。
fn guard<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 索引文件结构（mtime + size + hash）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LongTermIndex {
    /// 正文 mtime（RFC3339 字符串；time 默认序列化为对象，这里显式用 rfc3339）。
    #[serde(with = "time::serde::rfc3339")]
    mtime: OffsetDateTime,
    /// 正文字节数。
    size: u64,
    /// 正文 SHA-256（十六进制），用于变更检测的补充。
    hash: String,
}

/// 计算字节序列的 SHA-256 十六进制摘要。
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    // 32 字节 → 64 hex 字符；`write!` 到 String 比 `format!` 后 `push_str` 高效。
    let mut out = String::with_capacity(64);
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    //! 最小单元测试：验证 mtime 缓存命中/失效逻辑（任务验收要求）。

    use super::*;
    use camino::Utf8PathBuf;

    fn make(dir: &std::path::Path) -> LongTermMemory {
        LongTermMemory::with_dir(
            &Utf8PathBuf::from_path_buf(dir.to_owned())
                .expect("tempdir path is UTF-8 on linux test env"),
        )
    }

    #[tokio::test]
    async fn load_missing_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = make(tmp.path());
        let content = mem.load().await.unwrap();
        assert!(content.is_empty());
        assert!(mem.last_mtime().is_none());
    }

    #[tokio::test]
    async fn save_then_load_roundtrip_and_cache_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = make(tmp.path());

        // 首次保存 + 读取。
        mem.save("hello memory").await.unwrap();
        let loaded = mem.load().await.unwrap();
        assert_eq!(loaded, "hello memory");
        assert!(mem.last_mtime().is_some());

        // 二次加载命中缓存：mtime 未变，返回相同内容。
        let cached = mem.load().await.unwrap();
        assert_eq!(cached, "hello memory");
    }

    #[tokio::test]
    async fn cache_invalidates_on_external_change() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = make(tmp.path());

        mem.save("v1").await.unwrap();
        let _ = mem.load().await.unwrap(); // 填充缓存

        // 外部直接覆写正文（绕过 save），并推进 mtime 以触发缓存失效。
        let path = tmp.path().join(LONG_TERM_FILE);
        std::fs::write(&path, "v2-external").unwrap();

        // 同秒内多次写可能 mtime 不变，这里 sleep 后再写一次以推进 mtime。
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&path, "v2-external").unwrap();

        let loaded = mem.load().await.unwrap();
        assert_eq!(
            loaded, "v2-external",
            "cache should invalidate on mtime change"
        );
    }

    #[tokio::test]
    async fn save_updates_index_with_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = make(tmp.path());
        mem.save("hashed content").await.unwrap();

        let idx_bytes = std::fs::read(tmp.path().join(INDEX_FILE)).unwrap();
        let idx: serde_json::Value = serde_json::from_slice(&idx_bytes).unwrap();
        assert_eq!(idx["size"].as_u64(), Some(14));
        assert_eq!(idx["hash"].as_str().unwrap().len(), 64);
        assert!(idx["mtime"].as_str().is_some());
    }
}
