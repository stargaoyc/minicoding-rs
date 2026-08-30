//! 会话索引：`index.json` 存储轻量会话元数据（不含消息正文）。
//!
//! 设计意图（见 `docs/features.md` S-02、`rules.md` C-07）：
//! - **快速列出**：万级会话直接读索引文件，无需逐个打开 `.jsonl` 解析首尾消息；
//! - **64KB 窗口**：`list_windowed` 首尾各 32KB 截断，避免大量会话撑爆上下文预算；
//! - **原子写**：先写 `.tmp` 再 `rename`，崩溃时索引文件不会半写。

use camino::{Utf8Path, Utf8PathBuf};
use minicoding_core::storage::{SessionListItem, StorageError};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// 单条会话索引项（轻量，不含消息正文）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionIndexEntry {
    /// 会话 ID（ULID 字符串）。
    pub session_id: String,
    /// 会话摘要（通常取首条用户消息文本，可能为空）。
    pub summary: Option<String>,
    /// 消息条数。
    pub message_count: usize,
    /// 创建时间（`OffsetDateTime` 经 RFC3339 serde 序列化，类型安全避免字符串格式漂移）。
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// 最后更新时间（同 `created_at`）。
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
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
        Self {
            session_id: session_id.into(),
            summary,
            message_count: 1,
            created_at: now,
            updated_at: now,
            parent_uuid: None,
        }
    }

    /// 转为 `SessionListItem`。
    ///
    /// 时间字段已是 `OffsetDateTime`，直接使用无需解析。
    #[must_use]
    pub fn to_meta(&self) -> SessionListItem {
        SessionListItem {
            id: self.session_id.clone(),
            created_at: self.created_at,
            message_count: self.message_count,
            last_message_at: self.updated_at,
            summary: self.summary.clone(),
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
        // 先写 .tmp 并 fsync，再 rename（同文件系统原子）。
        // 0600 创建（2026-08-25 审查 L2）：索引含会话标题/摘要等敏感元数据
        {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut file = opts.open(tmp.as_std_path())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = file.metadata()
                    && meta.permissions().mode() & 0o777 != 0o600
                {
                    let mut perm = meta.permissions();
                    perm.set_mode(0o600);
                    let _ = file.set_permissions(perm);
                }
            }
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(tmp.as_std_path(), path.as_std_path())?;
        // R9 STR-7 父目录 fsync（2026-08-23 审查 §10 同款，snapshot_store.rs 已修）：
        // rename 元数据需随目录项落盘，崩溃极端情况否则可能丢失。
        if let Some(parent) = path.parent()
            && let Ok(dir) = std::fs::File::open(parent.as_std_path())
        {
            let _ = dir.sync_all();
        }
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
                created_at: existing.created_at,
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

    /// 更新指定 `session_id` 的摘要字段（T-M3-6）。
    ///
    /// 会话不在索引中时无操作（调用方应先 `append` 建立索引项）。
    /// 已存在则覆盖原 `summary`（无论原值是否为 `None`），其余字段保留。
    pub fn update_summary(&mut self, session_id: &str, summary: String) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.session_id == session_id) {
            entry.summary = Some(summary);
        }
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
            entry.updated_at = now;
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

    /// 返回全部索引项转换后的 `SessionListItem` 列表。
    #[must_use]
    pub fn to_metas(&self) -> Vec<SessionListItem> {
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

    /// 更新指定会话的摘要（A7：原 memory crate 的 `save_summary` 迁移至此——
    /// 操作 `SessionIndex` 内部状态的逻辑归属 storage，解除领域交叉依赖）。
    ///
    /// `session_id` 不在索引中时无操作；存在则覆盖 `summary`，保留其余字段。
    pub fn set_summary(&mut self, session_id: &str, summary: String) {
        let Some(entry) = self.get(session_id) else {
            return;
        };
        let mut updated = entry.clone();
        updated.summary = Some(summary);
        self.add(updated);
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
    #[test]
    fn set_summary_updates_existing_entry() {
        let mut index = SessionIndex::new();
        let entry = SessionIndexEntry::new("sess-1", None, OffsetDateTime::now_utc());
        index.add(entry);

        index.set_summary("sess-1", "new summary".to_string());

        let updated = index.get("sess-1").unwrap();
        assert_eq!(updated.summary.as_deref(), Some("new summary"));
    }

    #[test]
    fn set_summary_overwrites_existing() {
        let mut index = SessionIndex::new();
        let entry =
            SessionIndexEntry::new("sess-1", Some("old".to_string()), OffsetDateTime::now_utc());
        index.add(entry);
        index.set_summary("sess-1", "fresh".to_string());
        assert_eq!(
            index.get("sess-1").unwrap().summary.as_deref(),
            Some("fresh")
        );
    }

    #[test]
    fn set_summary_missing_session_noop() {
        let mut index = SessionIndex::new();
        index.set_summary("nonexistent", "summary".to_string());
        assert!(index.is_empty());
    }

    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::tempdir;
    use time::format_description::well_known::Rfc3339;

    fn entry(id: &str, count: usize) -> SessionIndexEntry {
        SessionIndexEntry {
            session_id: id.to_string(),
            summary: Some(format!("summary-{id}")),
            message_count: count,
            created_at: OffsetDateTime::parse("2026-01-01T00:00:00Z", &Rfc3339).unwrap(),
            updated_at: OffsetDateTime::parse("2026-01-02T00:00:00Z", &Rfc3339).unwrap(),
            parent_uuid: None,
        }
    }

    #[test]
    fn index_crud_add_remove_list() {
        let mut idx = SessionIndex::new();
        assert!(idx.is_empty(), "expected empty: idx");

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
        assert!(empty.is_empty(), "expected empty: empty");
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
                created_at: OffsetDateTime::parse("2026-01-01T00:00:00Z", &Rfc3339).unwrap(),
                updated_at: OffsetDateTime::parse("2026-01-02T00:00:00Z", &Rfc3339).unwrap(),
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

    // ---- 辅助：解析 RFC3339 时间戳，减少测试中的重复 ----
    fn ts(s: &str) -> OffsetDateTime {
        OffsetDateTime::parse(s, &Rfc3339).expect("valid rfc3339 in test fixture")
    }

    // ---- SessionIndexEntry::new ----

    #[test]
    fn entry_new_sets_all_fields() {
        let now = ts("2026-03-01T00:00:00Z");
        let e = SessionIndexEntry::new("01XYZ", Some("hello".to_string()), now);
        assert_eq!(e.session_id, "01XYZ");
        assert_eq!(e.summary.as_deref(), Some("hello"));
        assert_eq!(e.message_count, 1);
        assert_eq!(e.created_at, now);
        assert_eq!(e.updated_at, now);
        assert!(e.parent_uuid.is_none());
    }

    #[test]
    fn entry_new_accepts_none_summary() {
        let now = ts("2026-03-01T00:00:00Z");
        let e = SessionIndexEntry::new("01ABC", None, now);
        assert!(e.summary.is_none());
        assert_eq!(e.message_count, 1);
    }

    #[test]
    fn entry_new_accepts_string_ref_for_id() {
        // impl Into<String> 应接受 &str 与 String
        let id = String::from("01STR");
        let e = SessionIndexEntry::new(id, None, OffsetDateTime::UNIX_EPOCH);
        assert_eq!(e.session_id, "01STR");
    }

    // ---- SessionIndexEntry::to_meta ----

    #[test]
    fn entry_to_meta_maps_fields_directly() {
        let e = entry("01META", 42);
        let meta = e.to_meta();
        assert_eq!(meta.id, "01META");
        assert_eq!(meta.message_count, 42);
        assert_eq!(meta.created_at, e.created_at);
        assert_eq!(meta.last_message_at, e.updated_at);
    }

    // ---- SessionIndexEntry serde ----

    #[test]
    fn entry_serde_roundtrip_with_parent_uuid() {
        let e = SessionIndexEntry {
            session_id: "01SRT".to_string(),
            summary: Some("a summary".to_string()),
            message_count: 7,
            created_at: ts("2026-01-01T00:00:00Z"),
            updated_at: ts("2026-02-01T00:00:00Z"),
            parent_uuid: Some("01PARENT".to_string()),
        };
        let json = serde_json::to_string(&e).expect("serialize");
        let back: SessionIndexEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.session_id, e.session_id);
        assert_eq!(back.summary, e.summary);
        assert_eq!(back.message_count, e.message_count);
        assert_eq!(back.created_at, e.created_at);
        assert_eq!(back.updated_at, e.updated_at);
        assert_eq!(back.parent_uuid, e.parent_uuid);
    }

    #[test]
    fn entry_serde_roundtrip_none_fields() {
        let e = SessionIndexEntry {
            session_id: "01N".to_string(),
            summary: None,
            message_count: 0,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            parent_uuid: None,
        };
        let json = serde_json::to_string(&e).expect("serialize");
        let back: SessionIndexEntry = serde_json::from_str(&json).expect("deserialize");
        assert!(back.summary.is_none());
        assert!(back.parent_uuid.is_none());
        assert_eq!(back.message_count, 0);
        assert_eq!(back.created_at, OffsetDateTime::UNIX_EPOCH);
    }

    #[test]
    fn entry_serde_uses_rfc3339_format() {
        // 验证时间字段经 rfc3339 序列化为可读字符串（非数字时间戳）
        let e = SessionIndexEntry {
            session_id: "01FMT".to_string(),
            summary: None,
            message_count: 1,
            created_at: ts("2026-06-15T12:30:45Z"),
            updated_at: ts("2026-06-15T12:30:45Z"),
            parent_uuid: None,
        };
        let json = serde_json::to_string(&e).expect("serialize");
        assert!(json.contains("2026-06-15T12:30:45Z"), "json was: {json}");
    }

    // ---- SessionIndex serde / Default ----

    #[test]
    fn index_serde_roundtrip_preserves_entries() {
        let mut idx = SessionIndex::new();
        idx.add(entry("01R1", 3));
        idx.add(entry("01R2", 5));
        let json = serde_json::to_string(&idx).expect("serialize");
        let back: SessionIndex = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.len(), 2);
        assert_eq!(back.get("01R1").unwrap().message_count, 3);
        assert_eq!(back.get("01R2").unwrap().message_count, 5);
    }

    #[test]
    fn index_default_is_empty() {
        let idx = SessionIndex::default();
        assert!(idx.is_empty(), "expected empty: idx");
        assert_eq!(idx.len(), 0);
        assert!(idx.list().is_empty());
        assert!(idx.to_metas().is_empty());
    }

    #[test]
    fn index_clone_is_independent() {
        let mut idx = SessionIndex::new();
        idx.add(entry("01CL", 1));
        let mut cloned = idx.clone();
        cloned.add(entry("01CL2", 2));
        cloned.remove("01CL");
        // 原索引不受 clone 侧修改影响
        assert_eq!(idx.len(), 1);
        assert!(idx.get("01CL").is_some());
        assert!(idx.get("01CL2").is_none());
        assert_eq!(cloned.len(), 1);
        assert!(cloned.get("01CL").is_none());
        assert!(cloned.get("01CL2").is_some());
    }

    // ---- load 边界 ----

    #[test]
    fn index_load_empty_file_returns_empty() {
        let dir = tempdir().unwrap();
        let path: Utf8PathBuf = dir.path().join("empty.json").try_into().unwrap();
        std::fs::write(path.as_std_path(), "   \n  ").unwrap();
        let loaded = SessionIndex::load(&path).unwrap();
        assert!(loaded.is_empty(), "expected empty: loaded");
    }

    #[test]
    fn index_load_corrupted_returns_corrupted_error() {
        let dir = tempdir().unwrap();
        let path: Utf8PathBuf = dir.path().join("bad.json").try_into().unwrap();
        std::fs::write(path.as_std_path(), "{ not valid json").unwrap();
        let err = SessionIndex::load(&path).unwrap_err();
        assert!(
            matches!(err, StorageError::Corrupted(_)),
            "expected Corrupted, got {err:?}"
        );
    }

    // ---- add：created_at 保留语义 ----

    #[test]
    fn index_add_update_preserves_original_created_at() {
        let mut idx = SessionIndex::new();
        idx.add(SessionIndexEntry {
            session_id: "01P".to_string(),
            summary: Some("orig".to_string()),
            message_count: 1,
            created_at: ts("2026-01-01T00:00:00Z"),
            updated_at: ts("2026-01-02T00:00:00Z"),
            parent_uuid: None,
        });
        // 用更晚的 created_at 覆盖 —— 原始 created_at 应被保留
        idx.add(SessionIndexEntry {
            session_id: "01P".to_string(),
            summary: Some("new".to_string()),
            message_count: 10,
            created_at: ts("2099-12-31T00:00:00Z"),
            updated_at: ts("2026-06-01T00:00:00Z"),
            parent_uuid: None,
        });
        assert_eq!(idx.len(), 1);
        let got = idx.get("01P").unwrap();
        assert_eq!(got.created_at, ts("2026-01-01T00:00:00Z"));
        assert_eq!(got.updated_at, ts("2026-06-01T00:00:00Z"));
        assert_eq!(got.message_count, 10);
        assert_eq!(got.summary.as_deref(), Some("new"));
    }

    // ---- update_summary ----

    #[test]
    fn update_summary_sets_summary_on_existing_entry() {
        let mut idx = SessionIndex::new();
        idx.add(entry("01U", 1));
        idx.update_summary("01U", "new summary".to_string());
        assert_eq!(
            idx.get("01U").unwrap().summary.as_deref(),
            Some("new summary")
        );
    }

    #[test]
    fn update_summary_overwrites_existing_summary() {
        let mut idx = SessionIndex::new();
        idx.add(entry("01UO", 1)); // summary = Some("summary-01UO")
        idx.update_summary("01UO", "replaced".to_string());
        assert_eq!(
            idx.get("01UO").unwrap().summary.as_deref(),
            Some("replaced")
        );
    }

    #[test]
    fn update_summary_nonexistent_is_noop() {
        let mut idx = SessionIndex::new();
        idx.add(entry("01UN", 1));
        idx.update_summary("MISSING", "x".to_string());
        assert_eq!(idx.len(), 1);
        assert!(idx.get("MISSING").is_none());
        assert_eq!(
            idx.get("01UN").unwrap().summary.as_deref(),
            Some("summary-01UN"),
        );
    }

    // ---- upsert_on_append ----

    #[test]
    fn upsert_on_append_creates_new_entry_when_absent() {
        let mut idx = SessionIndex::new();
        let now = ts("2026-05-01T00:00:00Z");
        idx.upsert_on_append("01NEW", Some("first".to_string()), now);
        assert_eq!(idx.len(), 1);
        let e = idx.get("01NEW").unwrap();
        assert_eq!(e.message_count, 1);
        assert_eq!(e.summary.as_deref(), Some("first"));
        assert_eq!(e.created_at, now);
        assert_eq!(e.updated_at, now);
        assert!(e.parent_uuid.is_none());
    }

    #[test]
    fn upsert_on_append_existing_increments_count_and_updates_time() {
        let mut idx = SessionIndex::new();
        idx.add(entry("01UP", 5)); // message_count=5, updated_at=2026-01-02
        let now = ts("2026-07-01T00:00:00Z");
        idx.upsert_on_append("01UP", Some("ignored".to_string()), now);
        let e = idx.get("01UP").unwrap();
        assert_eq!(e.message_count, 6);
        assert_eq!(e.updated_at, now);
        // summary 已为 Some，不应被覆盖
        assert_eq!(e.summary.as_deref(), Some("summary-01UP"));
        // created_at 不变
        assert_eq!(e.created_at, ts("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn upsert_on_append_fills_none_summary() {
        let mut idx = SessionIndex::new();
        idx.add(SessionIndexEntry {
            session_id: "01NS".to_string(),
            summary: None,
            message_count: 1,
            created_at: ts("2026-01-01T00:00:00Z"),
            updated_at: ts("2026-01-01T00:00:00Z"),
            parent_uuid: None,
        });
        let now = ts("2026-08-01T00:00:00Z");
        idx.upsert_on_append("01NS", Some("filled".to_string()), now);
        let got = idx.get("01NS").unwrap();
        assert_eq!(got.message_count, 2);
        assert_eq!(got.summary.as_deref(), Some("filled"));
        assert_eq!(got.updated_at, now);
    }

    #[test]
    fn upsert_on_append_none_summary_stays_none() {
        let mut idx = SessionIndex::new();
        idx.add(SessionIndexEntry {
            session_id: "01NS2".to_string(),
            summary: None,
            message_count: 1,
            created_at: ts("2026-01-01T00:00:00Z"),
            updated_at: ts("2026-01-01T00:00:00Z"),
            parent_uuid: None,
        });
        let now = ts("2026-09-01T00:00:00Z");
        idx.upsert_on_append("01NS2", None, now);
        let got = idx.get("01NS2").unwrap();
        assert_eq!(got.message_count, 2);
        assert!(got.summary.is_none());
        assert_eq!(got.updated_at, now);
    }

    // ---- list_windowed 边界 ----

    #[test]
    fn list_windowed_empty_returns_empty_string() {
        let idx = SessionIndex::new();
        assert_eq!(idx.list_windowed(), "");
    }

    #[test]
    fn list_windowed_single_entry_no_truncation() {
        let mut idx = SessionIndex::new();
        idx.add(entry("01SINGLE", 1));
        let out = idx.list_windowed();
        assert!(out.contains("01SINGLE"));
        assert!(!out.contains(MORE_MARKER));
    }

    #[test]
    fn list_windowed_uses_placeholder_for_none_summary() {
        let mut idx = SessionIndex::new();
        idx.add(SessionIndexEntry {
            session_id: "01NSUM".to_string(),
            summary: None,
            message_count: 1,
            created_at: ts("2026-01-01T00:00:00Z"),
            updated_at: ts("2026-01-02T00:00:00Z"),
            parent_uuid: None,
        });
        let out = idx.list_windowed();
        assert!(out.contains("(no summary)"), "out was: {out}");
        assert!(out.contains("01NSUM"));
    }

    // ---- save 边界 ----

    #[test]
    fn index_save_overwrites_existing_file() {
        let dir = tempdir().unwrap();
        let path: Utf8PathBuf = dir.path().join("overwrite.json").try_into().unwrap();
        let mut idx = SessionIndex::new();
        idx.add(entry("01OW1", 1));
        idx.save(&path).unwrap();
        // 用更少条目覆盖写
        let mut idx2 = SessionIndex::new();
        idx2.add(entry("01OW2", 2));
        idx2.save(&path).unwrap();
        let loaded = SessionIndex::load(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded.get("01OW1").is_none());
        assert!(loaded.get("01OW2").is_some());
        // .tmp 仍不应残留
        let tmp: Utf8PathBuf = path.with_extension("json.tmp");
        assert!(!tmp.exists());
    }

    // ---- remove 边界 ----

    #[test]
    fn index_remove_all_entries_yields_empty() {
        let mut idx = SessionIndex::new();
        idx.add(entry("01RA1", 1));
        idx.add(entry("01RA2", 2));
        idx.remove("01RA1");
        idx.remove("01RA2");
        assert!(idx.is_empty(), "expected empty: idx");
        assert_eq!(idx.len(), 0);
        assert_eq!(idx.list_windowed(), "");
    }

    #[test]
    fn index_get_on_empty_returns_none() {
        let idx = SessionIndex::new();
        assert!(idx.get("anything").is_none());
    }
}
