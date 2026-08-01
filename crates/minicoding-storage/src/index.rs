//! 会话索引：`index.json` 存储轻量会话元数据（不含消息正文）。
//!
//! 设计意图（见 `docs/features.md` S-02、`rules.md` C-07）：
//! - **快速列出**：万级会话直接读索引文件，无需逐个打开 `.jsonl` 解析首尾消息；
//! - **64KB 窗口**：`list_windowed` 首尾各 32KB 截断，避免大量会话撑爆上下文预算；
//! - **原子写**：先写 `.tmp` 再 `rename`，崩溃时索引文件不会半写。

use camino::{Utf8Path, Utf8PathBuf};
use minicoding_core::storage::{SessionMeta, StorageError};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// 单条会话索引项（轻量，不含消息正文）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionIndexEntry {
    /// 会话 ID（ULID 字符串）。
    pub session_id: String,
    /// 会话摘要（通常取首条用户消息文本，可能为空）。
    pub summary: Option<String>,
    /// 消息条数。
    pub message_count: usize,
    /// 创建时间（RFC3339 字符串，避免索引文件携带 `OffsetDateTime` 序列化歧义）。
    pub created_at: String,
    /// 最后更新时间（RFC3339 字符串）。
    pub updated_at: String,
    /// 父会话 ID（fork 场景，当前未用）。
    pub parent_uuid: Option<String>,
}

impl SessionIndexEntry {
    /// 从会话 ID 与首条消息构造索引项。
    #[must_use]
    pub fn new(
        session_id: impl Into<String>,
        summary: Option<String>,
        now: OffsetDateTime,
    ) -> Self {
        let ts = now.format(&Rfc3339).unwrap_or_default();
        Self {
            session_id: session_id.into(),
            summary,
            message_count: 1,
            created_at: ts.clone(),
            updated_at: ts,
            parent_uuid: None,
        }
    }

    /// 转为 `SessionMeta`（解析 RFC3339 时间字符串为 `OffsetDateTime`）。
    ///
    /// 时间字符串损坏时回退到 UNIX 元年，避免单个损坏项阻断整个列出。
    #[must_use]
    pub fn to_meta(&self) -> SessionMeta {
        let created_at =
            OffsetDateTime::parse(&self.created_at, &Rfc3339).unwrap_or(OffsetDateTime::UNIX_EPOCH);
        let last_message_at =
            OffsetDateTime::parse(&self.updated_at, &Rfc3339).unwrap_or(OffsetDateTime::UNIX_EPOCH);
        SessionMeta {
            id: self.session_id.clone(),
            created_at,
            message_count: self.message_count,
            last_message_at,
        }
    }
}

/// 会话索引：`Vec<SessionIndexEntry>` 的有序集合，提供原子持久化与窗口列出。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionIndex {
    entries: Vec<SessionIndexEntry>,
}

/// 64KB 列出窗口的半窗大小（首尾各 32KB，见 `rules.md` C-07）。
const WINDOW_HALF: usize = 32 * 1024;

/// 窗口截断时的占位标记前缀。
const MORE_MARKER: &str = "[... ";

impl SessionIndex {
    /// 创建空索引。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 从 `index.json` 文件加载；文件不存在时返回空索引。
    ///
    /// # Errors
    /// - `StorageError::Io`：读取失败（除 `NotFound`）；
    /// - `StorageError::Corrupted`：JSON 解析失败；
    /// - `StorageError::Serialize`：不应发生（UTF-8 路径）。
    pub fn load(path: &Utf8Path) -> Result<Self, StorageError> {
        let content = match std::fs::read_to_string(path.as_std_path()) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::new()),
            Err(e) => return Err(e.into()),
        };
        if content.trim().is_empty() {
            return Ok(Self::new());
        }
        serde_json::from_str::<Self>(&content)
            .map_err(|e| StorageError::Corrupted(format!("index.json: {e}")))
    }

    /// 原子写入 `index.json`：先写 `.tmp` 再 `rename`，保证崩溃安全。
    ///
    /// # Errors
    /// 返回 `StorageError::Io`（写或 rename 失败）或 `StorageError::Serialize`。
    pub fn save(&self, path: &Utf8Path) -> Result<(), StorageError> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| StorageError::Serialize(e.to_string()))?;
        let tmp: Utf8PathBuf = path.with_extension("json.tmp");
        // 先写 .tmp 并 fsync，再 rename（同文件系统原子）
        {
            use std::io::Write;
            let mut file = std::fs::File::create(tmp.as_std_path())?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(tmp.as_std_path(), path.as_std_path())?;
        Ok(())
    }

    /// 新增或更新索引项（按 `session_id` 去重，已存在则覆盖）。
    pub fn add(&mut self, entry: SessionIndexEntry) {
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|e| e.session_id == entry.session_id)
        {
            // 保留原 created_at，更新其余字段（update 场景）
            *existing = SessionIndexEntry {
                created_at: existing.created_at.clone(),
                ..entry
            };
        } else {
            self.entries.push(entry);
        }
    }

    /// 移除指定 `session_id` 的索引项；不存在则无操作。
    pub fn remove(&mut self, session_id: &str) {
        self.entries.retain(|e| e.session_id != session_id);
    }

    /// 追加消息时更新索引：存在则递增 `message_count` + 更新 `updated_at`，
    /// 不存在则新增（携带 `summary`）。仅在原 summary 为空时补充。
    pub fn upsert_on_append(
        &mut self,
        session_id: &str,
        summary: Option<String>,
        now: OffsetDateTime,
    ) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.session_id == session_id) {
            entry.message_count += 1;
            entry.updated_at = now.format(&Rfc3339).unwrap_or_default();
            if entry.summary.is_none() {
                entry.summary = summary;
            }
        } else {
            self.entries
                .push(SessionIndexEntry::new(session_id, summary, now));
        }
    }

    /// 返回全部索引项的切片引用。
    #[must_use]
    pub fn list(&self) -> &[SessionIndexEntry] {
        &self.entries
    }

    /// 返回全部索引项转换后的 `SessionMeta` 列表。
    #[must_use]
    pub fn to_metas(&self) -> Vec<SessionMeta> {
        self.entries
            .iter()
            .map(SessionIndexEntry::to_meta)
            .collect()
    }

    /// 按索引项查找指定 `session_id`。
    #[must_use]
    pub fn get(&self, session_id: &str) -> Option<&SessionIndexEntry> {
        self.entries.iter().find(|e| e.session_id == session_id)
    }

    /// 64KB 窗口列出：首尾各 32KB，超出部分以 `[... N more sessions]` 标注。
    ///
    /// 用于 CLI/上下文展示万级会话时避免资源耗尽（`rules.md` C-07）。按行边界
    /// 截断（不会截断半行），首尾各累计不超过 `WINDOW_HALF` 字节。
    #[must_use]
    pub fn list_windowed(&self) -> String {
        if self.entries.is_empty() {
            return String::new();
        }
        let lines: Vec<String> = self
            .entries
            .iter()
            .map(|e| {
                let summary = e.summary.as_deref().unwrap_or("(no summary)");
                format!(
                    "{}  msgs={}  updated={}  {}",
                    e.session_id, e.message_count, e.updated_at, summary
                )
            })
            .collect();
        let joined = lines.join("\n");
        if joined.len() <= 2 * WINDOW_HALF {
            return joined;
        }
        // 首部：从头累计行直到达到 32KB
        let mut head = String::new();
        let mut head_count = 0usize;
        for line in &lines {
            let extra = line.len() + usize::from(!head.is_empty());
            if head.len() + extra > WINDOW_HALF {
                break;
            }
            if !head.is_empty() {
                head.push('\n');
            }
            head.push_str(line);
            head_count += 1;
        }
        // 尾部：从尾累计行直到达到 32KB
        let mut tail = String::new();
        let mut tail_count = 0usize;
        for line in lines.iter().rev() {
            let extra = line.len() + usize::from(!tail.is_empty());
            if tail.len() + extra > WINDOW_HALF {
                break;
            }
            if !tail.is_empty() {
                tail.insert(0, '\n');
            }
            tail.insert_str(0, line);
            tail_count += 1;
        }
        let hidden = self.entries.len().saturating_sub(head_count + tail_count);
        format!("{head}\n{MORE_MARKER}{hidden} more sessions]\n{tail}")
    }

    /// 索引项数量。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::tempdir;

    fn entry(id: &str, count: usize) -> SessionIndexEntry {
        SessionIndexEntry {
            session_id: id.to_string(),
            summary: Some(format!("summary-{id}")),
            message_count: count,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            parent_uuid: None,
        }
    }

    #[test]
    fn index_crud_add_remove_list() {
        let mut idx = SessionIndex::new();
        assert!(idx.is_empty());

        idx.add(entry("01H8", 3));
        idx.add(entry("01H9", 5));
        assert_eq!(idx.len(), 2);
        assert_eq!(idx.list().len(), 2);

        // 去重：相同 session_id 覆盖
        idx.add(entry("01H8", 7));
        assert_eq!(idx.len(), 2);
        assert_eq!(idx.get("01H8").unwrap().message_count, 7);

        // remove
        idx.remove("01H9");
        assert_eq!(idx.len(), 1);
        assert!(idx.get("01H9").is_none());

        // remove 不存在的 id 无副作用
        idx.remove("nope");
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn index_load_save_roundtrip() {
        let dir = tempdir().unwrap();
        let path: Utf8PathBuf = dir.path().join("index.json").try_into().unwrap();

        let mut idx = SessionIndex::new();
        idx.add(entry("01AA", 2));
        idx.add(entry("01BB", 4));
        idx.save(&path).unwrap();

        let loaded = SessionIndex::load(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.get("01AA").unwrap().message_count, 2);
        assert_eq!(loaded.get("01BB").unwrap().message_count, 4);

        // load 不存在的文件返回空索引
        let missing: Utf8PathBuf = dir.path().join("missing.json").try_into().unwrap();
        let empty = SessionIndex::load(&missing).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn index_save_atomic_no_tmp_leftover() {
        let dir = tempdir().unwrap();
        let path: Utf8PathBuf = dir.path().join("index.json").try_into().unwrap();
        let mut idx = SessionIndex::new();
        idx.add(entry("01CC", 1));
        idx.save(&path).unwrap();
        // .tmp 应被 rename 掉，不残留
        let tmp: Utf8PathBuf = path.with_extension("json.tmp");
        assert!(!tmp.exists());
        assert!(path.exists());
    }

    #[test]
    fn index_to_metas_parses_time() {
        let mut idx = SessionIndex::new();
        idx.add(entry("01DD", 9));
        let metas = idx.to_metas();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].id, "01DD");
        assert_eq!(metas[0].message_count, 9);
        // 时间应被解析为非 UNIX 元年
        assert_ne!(metas[0].created_at, OffsetDateTime::UNIX_EPOCH);
    }

    #[test]
    fn list_windowed_truncates_large_list() {
        let mut idx = SessionIndex::new();
        // 构造足够大的索引以超过 64KB（每条约 80 字节，需 ~1000 条）
        for i in 0..1000 {
            idx.add(SessionIndexEntry {
                session_id: format!("01{i:020}"),
                summary: Some("x".repeat(60)),
                message_count: i,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-02T00:00:00Z".to_string(),
                parent_uuid: None,
            });
        }
        let out = idx.list_windowed();
        // 应包含截断标记
        assert!(out.contains(MORE_MARKER), "expected truncation marker");
        assert!(out.contains("more sessions]"));
        // 总长不超过 64KB + 标记行
        assert!(
            out.len() <= 2 * WINDOW_HALF + 64,
            "windowed output too large: {}",
            out.len()
        );
    }

    #[test]
    fn list_windowed_small_list_no_truncation() {
        let mut idx = SessionIndex::new();
        idx.add(entry("01EE", 1));
        idx.add(entry("01FF", 2));
        let out = idx.list_windowed();
        assert!(!out.contains(MORE_MARKER));
        assert!(out.contains("01EE"));
        assert!(out.contains("01FF"));
    }
}
