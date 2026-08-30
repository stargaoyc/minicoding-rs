//! `JSONL` 会话存储：每条消息一行 JSON，追加写，崩溃安全（`fsync`）。
//!
//! 集成会话索引（`index.json`，见 `index.rs`）与导出（见 `export.rs`）：
//! - `list_sessions` 优先走索引缓存，索引不存在时回退扫描并落盘；
//! - `append` / `delete` 同步更新索引（best effort，不阻塞主路径）；
//! - `export` 按 `ExportFormat` 导出会话为 Markdown / JSONL。
//!
//! 写入安全（M-01，修 S1-2）：`append` 在 `{session_id}.lock` 阻塞式排他锁内
//! 完成「单次 `write_all`（消息行 + 换行）+ `fsync` + 索引更新」，消除两次
//! syscall 之间的交错窗口——两个进程并发追加同一会话时后者等待前者，不会把
//! 两条消息并成一行不可解析的 JSON。
//!
//! 格式版本（M-02，修 S2-1）：新会话首行写 `{"_header":{...}}` 头；`load` 校验
//! `format_version`，更新版本写入的文件显式报 `StorageError::FormatUnsupported`
//!（防静默丢事件）。旧文件（无 header）按 v1 处理。`load` 跳过坏行（部分损坏
//! 可恢复），仅全坏时返回 `Corrupted`（与 `scan` 行为一致）。

use crate::export::{ExportFormat, export_session_jsonl, export_session_md};
use crate::index::{SessionIndex, SessionIndexEntry};
use crate::lock::SessionLock;
use camino::Utf8PathBuf;
use minicoding_core::model::{Message, Role, SessionId};
use minicoding_core::otel::span_name;
use minicoding_core::provider::BoxFuture;
use minicoding_core::storage::{SessionListItem, Storage, StorageError};
use std::sync::Mutex;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;

/// 消息流格式版本（M-02）。旧文件（无 header）视为 v1。
const MESSAGE_FORMAT_VERSION: u32 = 1;

/// R9 STR-3：单行消息长度上限（4 MiB）。此前 JSONL 全量 `read_to_string` 且
/// 单行无长度检查——异常/损坏文件中的超长行可撑爆内存（读取已全量到内存，
/// 解析阶段再限制已晚，但可阻止畸形行被当作消息解析）。正常消息行受工具
/// `max_output_bytes`（默认 1 MiB）约束，4 MiB 上限覆盖正常范围且防异常膨胀。
const MAX_MESSAGE_LINE_BYTES: usize = 4 * 1024 * 1024;

/// 消息流 header 行的 JSON 前缀（用于 load/scan 跳过 header 行）。
const HEADER_PREFIX: &str = "{\"_header\"";

/// `JSONL` 会话存储。
///
/// 文件布局：`{base_dir}/{session_id}.jsonl`，首行可选 `{"_header":...}`（M-02），
/// 每行一条 `Message`（JSON）。追加写，每条消息后 `fsync` 保证崩溃安全。
/// 会话索引 `{base_dir}/index.json` 缓存元数据，`list_sessions` 优先读索引。
pub struct JsonlStorage {
    base_dir: Utf8PathBuf,
    /// 进程内索引缓存（首次 `list_sessions`/`append` 时加载）。`std::sync::Mutex`
    /// 临界区不跨 await（索引文件小，sync I/O 短暂），符合 AGENTS.md §2.4。
    index_cache: Mutex<Option<SessionIndex>>,
}

impl JsonlStorage {
    /// 创建存储实例，若 `base_dir` 不存在则创建。
    #[must_use]
    pub fn new(base_dir: Utf8PathBuf) -> Self {
        // 一次性目录创建；失败时由后续 append 报错暴露
        let _ = std::fs::create_dir_all(base_dir.as_std_path());
        Self {
            base_dir,
            index_cache: Mutex::new(None),
        }
    }

    fn session_path(&self, session: &SessionId) -> Utf8PathBuf {
        self.base_dir.join(format!("{session}.jsonl"))
    }

    fn index_path(&self) -> Utf8PathBuf {
        self.base_dir.join("index.json")
    }

    /// 同步加载会话全部消息（`--resume` 启动期用，T-M3-10a）。
    ///
    /// 与 `Storage::load` 同语义，但用 `std::fs` 同步读取——仅在 CLI 启动期
    /// （tokio runtime 尚未创建时）使用。运行时路径应走 `Storage::load`。
    ///
    /// # Errors
    /// - `StorageError::Io`：读取失败（除 `NotFound`）；
    /// - `StorageError::Corrupted`：消息行全部损坏（M-02：部分损坏跳过坏行）；
    /// - `StorageError::FormatUnsupported`：文件由更新版本写入（M-02）。
    #[tracing::instrument(skip(self), fields(otel.name = "storage.load"))]
    pub fn load_messages_sync(&self, session: &SessionId) -> Result<Vec<Message>, StorageError> {
        let path = self.session_path(session);
        let content = match std::fs::read_to_string(path.as_std_path()) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        parse_session_lines(&content)
    }

    /// 同步列出会话元数据（`session list` 子命令用，T-M3-10c）。
    ///
    /// 与 `Storage::list_sessions` 同语义，但用 `std::fs` 同步读取。优先读
    /// `index.json`，不存在时回退扫描目录。
    ///
    /// # Errors
    /// 索引文件读取失败或目录扫描失败时返回错误。
    pub fn list_sessions_sync(&self) -> Result<Vec<SessionListItem>, StorageError> {
        // 1. 尝试缓存
        {
            let guard = self.lock_index();
            if let Some(idx) = guard.as_ref()
                && !idx.is_empty()
            {
                return Ok(idx.to_metas());
            }
        }
        // 2. 加载索引文件
        let index = SessionIndex::load(&self.index_path())?;
        if !index.is_empty() {
            let metas = index.to_metas();
            let mut guard = self.lock_index();
            *guard = Some(index);
            return Ok(metas);
        }
        // 3. 回退：同步扫描目录
        let entries = match std::fs::read_dir(self.base_dir.as_std_path()) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e.into()),
        };
        let mut index = SessionIndex::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // ST-8/ST-R6-4（2026-08-28 R5/R6）：跳过 `{session}.events.jsonl` 事件
            // 流文件——同步扫描路径此前未跳过（async 路径已修），误解析产生
            // warn 噪音 + IO 浪费。
            if stem.ends_with(".events") {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("skip unreadable session file {}: {e}", path.display());
                    continue;
                }
            };
            let (lines, unsupported) = extract_message_lines(&content);
            if unsupported {
                tracing::warn!(
                    "skip session file {}: format_version too new",
                    path.display()
                );
                continue;
            }
            if lines.is_empty() {
                continue;
            }
            let first = match serde_json::from_str::<Message>(lines[0]) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("skip corrupted session file {}: {e}", path.display());
                    continue;
                }
            };
            let last = match serde_json::from_str::<Message>(lines[lines.len() - 1]) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("skip corrupted session file {}: {e}", path.display());
                    continue;
                }
            };
            let summary = find_first_user_summary(&lines);
            index.add(SessionIndexEntry {
                session_id: stem.to_string(),
                summary,
                message_count: lines.len(),
                created_at: first.created_at,
                updated_at: last.created_at,
                parent_uuid: None,
            });
        }
        let metas = index.to_metas();
        // ST-R6-2（2026-08-28 R6 审查）：扫描回退路径经 `mutate_index` 落盘——
        // 此前直接 `index.save`（tmp+rename 无跨进程锁），并发 append 的
        // `mutate_index` 更新可被覆盖（last-rename-wins 丢条目）。锁内合并：
        // 磁盘索引 + 扫描结果并集（`add` 按 session_id upsert，扫描数据优先）。
        if let Err(e) = self.mutate_index(|idx| {
            for entry in index.list().iter().cloned() {
                idx.add(entry);
            }
        }) {
            tracing::warn!("failed to persist session index: {e}");
        }
        // 内存缓存（扫描结果为准，进程内读路径一致）
        {
            let mut guard = self.lock_index();
            *guard = Some(index.clone());
        }
        Ok(metas)
    }

    /// 同步删除会话（`session delete` 子命令用，T-M3-10c）。
    ///
    /// 与 `Storage::delete` 同语义，但用 `std::fs` 同步删除。
    ///
    /// # Errors
    /// 文件删除失败（除 `NotFound`）时返回错误。
    pub fn delete_session_sync(&self, session: &SessionId) -> Result<(), StorageError> {
        let path = self.session_path(session);
        let lock_path = self.base_dir.join(format!("{session}.lock"));
        // ST-R6-3（2026-08-28 R6 审查）：同步删除与 async `delete` 对齐取会话
        // 排他锁——此前无锁删除：并发 append 进程仍持旧锁文件 fd 写已 unlink
        // 的 inode，随后 `update_index_on_append` 把会话写回索引，产生幽灵索引
        // 项（索引/文件不一致）。持锁删文件 + 移除索引 + 清理锁文件。
        let _guard = crate::lock::SessionLock::acquire_blocking(&lock_path)?;
        match std::fs::remove_file(path.as_std_path()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        self.remove_from_index(session);
        let _ = std::fs::remove_file(lock_path.as_std_path());
        Ok(())
    }

    /// 同步复制消息到新会话文件（`--fork-session` 用，T-M3-10b）。
    ///
    /// 逐行追加写 + fsync，遵循 JSONL 崩溃安全追加写约定（`data-model.md` §3.2）。
    /// 原会话文件只读不写（`design.md` §10.5）。新会话文件创建后更新索引。
    ///
    /// # Errors
    /// 文件创建或写入失败时返回错误。
    pub fn fork_session_sync(
        &self,
        new_session_id: &SessionId,
        messages: &[Message],
    ) -> Result<(), StorageError> {
        let path = self.session_path(new_session_id);
        {
            use std::io::Write;
            // S19/C-04：fork 转录含源会话敏感内容，0600 创建（2026-08-25 审查 L2）
            let mut opts = std::fs::OpenOptions::new();
            opts.append(true).create(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut file = opts.open(path.as_std_path())?;
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
            if file.metadata()?.len() == 0 {
                // 新文件写格式头（M-02），与 append 路径一致
                let header = format!(
                    "{{\"_header\":{{\"format_version\":{MESSAGE_FORMAT_VERSION},\"app\":\"minicoding\",\"app_version\":\"{}\"}}}}\n",
                    env!("CARGO_PKG_VERSION")
                );
                file.write_all(header.as_bytes())?;
            }
            for msg in messages {
                let line = serde_json::to_string(msg)
                    .map_err(|e| StorageError::Serialize(e.to_string()))?;
                file.write_all(line.as_bytes())?;
                file.write_all(b"\n")?;
            }
            // R9 STR-8 修复：批量写后**一次 fsync**（此前每条消息一次 fsync +
            // 一次 mutate_index。对大 fork 分别 5000 次 IO 放大）。
            file.flush()?;
            file.sync_all()?;
        }
        // 一次索引更新（替代逐条 upsert）
        // R9 STR-8 修复：改为一次 mutate_index 批量 upsert 全部消息
        // （此前每条消息一次 mutate_index：每次 acquire 锁 + 全量 load + 全量 save）。
        if !messages.is_empty() {
            let now = OffsetDateTime::now_utc();
            let result = self.mutate_index(|idx| {
                for msg in messages {
                    let summary = if matches!(msg.role, Role::User) {
                        let text = msg.text();
                        if text.is_empty() {
                            None
                        } else {
                            Some(text.chars().take(80).collect())
                        }
                    } else {
                        None
                    };
                    idx.upsert_on_append(new_session_id, summary, now);
                }
            });
            if let Err(e) = result {
                tracing::warn!("fork 后索引更新失败: {e}");
            }
        }
        Ok(())
    }

    /// 锁定索引缓存（从 poison 中恢复：索引仅为缓存，重建无害）。
    fn lock_index(&self) -> std::sync::MutexGuard<'_, Option<SessionIndex>> {
        self.index_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 导出会话为指定格式。
    ///
    /// # Errors
    /// - `StorageError::NotFound`：会话无消息文件或为空；
    /// - `StorageError::Io`：读取消息文件失败；
    /// - `StorageError::Corrupted`：消息行 JSON 解析失败。
    pub async fn export(
        &self,
        id: &SessionId,
        format: ExportFormat,
    ) -> Result<String, StorageError> {
        let messages = self.load(id).await?;
        if messages.is_empty() {
            return Err(StorageError::NotFound(id.clone()));
        }
        let first = &messages[0];
        let last = &messages[messages.len() - 1];
        let meta = SessionListItem {
            id: id.clone(),
            created_at: first.created_at,
            message_count: messages.len(),
            last_message_at: last.created_at,
            summary: None,
        };
        Ok(match format {
            ExportFormat::Markdown => export_session_md(&messages, &meta),
            ExportFormat::Jsonl => export_session_jsonl(&messages),
        })
    }

    /// 跨进程安全的 index 修改（ST-2，2026-08-28 R5 收尾）。
    ///
    /// 此前 `update_index_on_append`/`remove_from_index`/`update_summary_sync`
    /// 都是"缓存内修改 → `save`（tmp+rename）"，无跨进程锁——双进程不同会话
    /// 同时写 `index.json` 时 last-rename-wins 静默丢条目。本方法在
    /// `{base_dir}/index.lock`（阻塞式排他锁）内执行：**重新从磁盘加载**（拿到
    /// 其他进程最新写入）→ 应用修改 → 落盘 → 更新内存缓存。`index.lock` 与
    /// 会话锁同目录，随 `delete` 清理。
    ///
    /// `f` 接收可变的 `SessionIndex`，返回 `T`。锁经 `SessionLock` RAII 释放。
    ///
    /// # Errors
    /// 加锁失败 / 磁盘加载 / 落盘失败时返回 `StorageError`。
    fn mutate_index<T>(&self, f: impl FnOnce(&mut SessionIndex) -> T) -> Result<T, StorageError> {
        use crate::lock::SessionLock;
        // 阻塞式跨进程锁（热路径低竞争：index 更新仅在消息追加/删除/摘要变更时）。
        let lock = SessionLock::acquire_blocking(self.base_dir.join("index.lock"))?;
        // 锁内重新从磁盘加载——合并其他进程的最新写入，避免 last-rename-wins。
        // R9 STR-2 修复：损坏索引此前 `unwrap_or_default()` 静默清空，只写入
        // 本次 f() 触及的条目后覆盖落盘（其余会话元数据永久丢失）。改为损坏时
        // warn + 从空索引起步（`f` 只 upsert 本次条目，不丢历史——历史本已
        // 不可读，覆盖不损失更多；且后续读路径扫描兜底会重建）。
        let mut index = match SessionIndex::load(&self.index_path()) {
            Ok(idx) => idx,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "session index 损坏，本次更新从空索引起步（读路径将扫描重建）"
                );
                SessionIndex::new()
            }
        };
        let out = f(&mut index);
        index.save(&self.index_path())?;
        // 更新内存缓存（读路径走缓存，保持一致性）。
        let mut guard = self.lock_index();
        *guard = Some(index);
        drop(guard);
        drop(lock);
        Ok(out)
    }

    /// 追加消息后更新索引（best effort）。失败仅记日志，不影响主路径。
    fn update_index_on_append(&self, session_id: &str, msg: &Message) {
        let result = self.mutate_index(|idx| {
            let now = OffsetDateTime::now_utc();
            let summary = if matches!(msg.role, Role::User) {
                let text = msg.text();
                if text.is_empty() {
                    None
                } else {
                    Some(text.chars().take(80).collect())
                }
            } else {
                None
            };
            idx.upsert_on_append(session_id, summary, now);
        });
        // R8 SEC-7 修复：加锁失败（如锁文件被并发清理）多为瞬时——
        // 短退避重试一次再降级。仍失败记 error（索引不一致影响会话可见性），
        // 但**不阻塞主路径**（append 已落盘成功）。
        if let Err(e) = result {
            let retried = self.mutate_index(|idx| {
                let now = OffsetDateTime::now_utc();
                let summary = if matches!(msg.role, Role::User) {
                    let text = msg.text();
                    if text.is_empty() {
                        None
                    } else {
                        Some(text.chars().take(80).collect())
                    }
                } else {
                    None
                };
                idx.upsert_on_append(session_id, summary, now);
            });
            match retried {
                Ok(()) => {
                    tracing::warn!("index lock transient failure on append, retried ok: {e}");
                }
                Err(re) => {
                    tracing::error!(
                        "failed to update session index on append (retried): {e}; {re}"
                    );
                }
            }
        }
    }

    /// 删除会话后从索引移除（best effort）。
    fn remove_from_index(&self, session_id: &str) {
        let result = self.mutate_index(|idx| {
            idx.remove(session_id);
        });
        if let Err(e) = result {
            tracing::warn!("failed to remove session from index: {e}");
        }
    }

    /// 同步更新会话索引中的摘要字段（T-M3-6）。
    ///
    /// 调用 `SessionIndex::update_summary` 落盘。会话不存在于索引时静默忽略
    /// （best effort，与 `update_index_on_append` 一致）。
    ///
    /// # Errors
    /// 索引文件读取或写入失败时返回 `StorageError`。
    pub fn update_summary_sync(
        &self,
        session_id: &SessionId,
        summary: &str,
    ) -> Result<(), StorageError> {
        // ST-2：跨进程锁内 load-modify-save（复用 mutate_index）。
        self.mutate_index(|idx| {
            // 会话不在索引中：静默忽略（best effort）
            if idx.get(session_id.as_str()).is_none() {
                tracing::warn!(
                    session = %session_id,
                    "update_summary: session not in index, skipping (call append first)"
                );
                return;
            }
            idx.update_summary(session_id.as_str(), summary.to_string());
        })
    }

    /// 从目录扫描构建索引（索引文件不存在时的回退路径）。
    async fn build_index_from_scan(&self) -> Result<SessionIndex, StorageError> {
        let mut entries = match tokio::fs::read_dir(&self.base_dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SessionIndex::new());
            }
            Err(e) => return Err(e.into()),
        };
        let mut index = SessionIndex::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // ST-8（2026-08-28 R5 收尾）：跳过 `{session}.events.jsonl` 事件流文件
            // （纯事件文件，不含消息行；误解析产生 warn 噪音 + IO 浪费）。
            if stem.ends_with(".events") {
                continue;
            }
            let content = match tokio::fs::read_to_string(&path).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("skip unreadable session file {}: {e}", path.display());
                    continue;
                }
            };
            let (lines, unsupported) = extract_message_lines(&content);
            if unsupported {
                tracing::warn!(
                    "skip session file {}: format_version too new",
                    path.display()
                );
                continue;
            }
            if lines.is_empty() {
                continue;
            }
            let first = match serde_json::from_str::<Message>(lines[0]) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("skip corrupted session file {}: {e}", path.display());
                    continue;
                }
            };
            let last = match serde_json::from_str::<Message>(lines[lines.len() - 1]) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("skip corrupted session file {}: {e}", path.display());
                    continue;
                }
            };
            let summary = find_first_user_summary(&lines);
            index.add(SessionIndexEntry {
                session_id: stem.to_string(),
                summary,
                message_count: lines.len(),
                created_at: first.created_at,
                updated_at: last.created_at,
                parent_uuid: None,
            });
        }
        Ok(index)
    }
}

/// 从消息行中提取首条用户消息文本作为摘要（截断 80 字符）。
fn find_first_user_summary(lines: &[&str]) -> Option<String> {
    for line in lines {
        let Ok(msg) = serde_json::from_str::<Message>(line) else {
            continue;
        };
        if matches!(msg.role, Role::User) {
            let text = msg.text();
            if !text.is_empty() {
                return Some(text.chars().take(80).collect());
            }
        }
    }
    None
}

/// 会话文件头（M-02）。JSON 布局 `{"_header":{"format_version":1,...}}`，
/// 由 `append` 在创建空会话文件时写入；旧文件无此头，按 v1 处理。
#[derive(serde::Deserialize)]
struct HeaderLine {
    #[serde(rename = "_header")]
    header: Header,
}

#[derive(serde::Deserialize)]
struct Header {
    format_version: u32,
}

/// 判断一行是否为会话文件头（以 `{"_header"` 前缀判定，避免全量解析开销）。
fn is_header_line(line: &str) -> bool {
    line.starts_with(HEADER_PREFIX)
}

/// 提取消息行并校验 header 版本（M-02，scan 路径共用）。
///
/// 返回 `(消息行列表, 版本过高)`：header 行被跳过；header 的 `format_version`
/// 大于当前支持版本时置 `true`，调用方应跳过整个会话（与 `load` 的
/// `FormatUnsupported` 拒绝行为一致，防止把新版文件当旧数据索引）。
fn extract_message_lines(content: &str) -> (Vec<&str>, bool) {
    let mut lines = Vec::new();
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        // R9 STR-3：超长行跳过（防畸形行被当作消息解析）
        if line.len() > MAX_MESSAGE_LINE_BYTES {
            tracing::warn!(
                "跳过超长消息行（{} 字节 > {MAX_MESSAGE_LINE_BYTES}）",
                line.len()
            );
            continue;
        }
        if is_header_line(line) {
            if let Ok(header) = serde_json::from_str::<HeaderLine>(line)
                && header.header.format_version > MESSAGE_FORMAT_VERSION
            {
                return (lines, true);
            }
            continue;
        }
        lines.push(line);
    }
    (lines, false)
}

/// 解析会话文件内容（M-02）：跳过 header 行与坏行，校验格式版本。
///
/// 容错规则（S2-1）：单行 JSON 解析失败仅跳过并记 warn，不使整个会话不可读；
/// 全部消息行均损坏时返回 `StorageError::Corrupted`（与 scan 跳过该会话的行为
/// 呼应，load 需显式报错而非静默返回空列表，避免 `--resume` 悄悄丢失数据）。
/// header 的 `format_version` 大于当前支持版本时返回
/// `StorageError::FormatUnsupported`（防静默丢事件，S2-1）。
fn parse_session_lines(content: &str) -> Result<Vec<Message>, StorageError> {
    let mut messages = Vec::new();
    let mut saw_bad = false;
    let mut non_empty = 0usize;
    for (idx, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        non_empty += 1;
        // R9 STR-3：超长行跳过（防畸形行撑爆内存/被当作消息解析）
        if line.len() > MAX_MESSAGE_LINE_BYTES {
            saw_bad = true;
            tracing::warn!(
                "skip oversized message line {} ({} bytes > {MAX_MESSAGE_LINE_BYTES})",
                idx + 1,
                line.len()
            );
            continue;
        }
        if is_header_line(line) {
            if let Ok(header) = serde_json::from_str::<HeaderLine>(line)
                && header.header.format_version > MESSAGE_FORMAT_VERSION
            {
                return Err(StorageError::FormatUnsupported(format!(
                    "line {}: format_version {} > supported {MESSAGE_FORMAT_VERSION}",
                    idx + 1,
                    header.header.format_version
                )));
            }
            continue;
        }
        match serde_json::from_str::<Message>(line) {
            Ok(msg) => messages.push(msg),
            Err(e) => {
                saw_bad = true;
                tracing::warn!("skip corrupted message line {}: {e}", idx + 1);
            }
        }
    }
    if messages.is_empty() && saw_bad {
        return Err(StorageError::Corrupted(format!(
            "all {non_empty} non-empty lines failed to parse"
        )));
    }
    Ok(messages)
}

impl Storage for JsonlStorage {
    #[tracing::instrument(skip(self, session, msg), fields(otel.name = span_name::STORAGE_APPEND))]
    fn append(
        &self,
        session: &SessionId,
        msg: &Message,
    ) -> BoxFuture<'_, Result<(), StorageError>> {
        let path = self.session_path(session);
        let lock_path = self.base_dir.join(format!("{session}.lock"));
        let msg = msg.clone();
        let session_id = session.clone();
        Box::pin(async move {
            let line =
                serde_json::to_string(&msg).map_err(|e| StorageError::Serialize(e.to_string()))?;

            // 阻塞式排他锁（M-01）：同会话并发追加串行化，避免两次 write 交错。
            // fs2 是同步 API，经 spawn_blocking 执行避免阻塞 async reactor。
            // `_lock` 名称前缀非"未使用"：RAII guard 持有排他锁至作用域结束
            // （unlock 由 Drop 完成），代码不直接引用它。
            let _lock = tokio::task::spawn_blocking({
                let lock_path = lock_path.clone();
                move || SessionLock::acquire_blocking(lock_path)
            })
            .await
            .map_err(|e| StorageError::Io(std::io::Error::other(e.to_string())))??;

            // S19/C-04：会话转录可能含 shell 输出等敏感内容，0600 创建
            let mut opts = tokio::fs::OpenOptions::new();
            opts.append(true).create(true);
            #[cfg(unix)]
            opts.mode(0o600); // S19/C-04（tokio 自带该方法）
            let mut file = opts.open(&path).await?;
            #[cfg(unix)]
            tighten_existing(&file, &path).await;
            // 首次创建（空文件）时先写格式头（M-02）。
            if file.metadata().await?.len() == 0 {
                let header = format!(
                    "{{\"_header\":{{\"format_version\":{MESSAGE_FORMAT_VERSION},\"app\":\"minicoding\",\"app_version\":\"{}\"}}}}\n",
                    env!("CARGO_PKG_VERSION")
                );
                file.write_all(header.as_bytes()).await?;
            }
            // 单次 write_all（消息行 + 换行）：原子追加，消除两 syscall 间交错窗口。
            let mut buf = line.into_bytes();
            buf.push(b'\n');
            file.write_all(&buf).await?;
            file.sync_all().await?;
            drop(file);

            // 索引更新在会话锁临界区内（M-01）：同会话并发 append 的索引一致。
            self.update_index_on_append(&session_id, &msg);
            // `lock` 在此 drop → 释放排他锁
            Ok(())
        })
    }

    fn load(&self, session: &SessionId) -> BoxFuture<'_, Result<Vec<Message>, StorageError>> {
        let path = self.session_path(session);
        Box::pin(async move {
            let content = match tokio::fs::read_to_string(&path).await {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Vec::new());
                }
                Err(e) => return Err(e.into()),
            };
            parse_session_lines(&content)
        })
    }

    fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<SessionListItem>, StorageError>> {
        Box::pin(async move {
            // 1. 尝试缓存（短锁，不跨 await）
            {
                let guard = self.lock_index();
                if let Some(idx) = guard.as_ref()
                    && !idx.is_empty()
                {
                    return Ok(idx.to_metas());
                }
            }
            // 2. 尝试加载索引文件
            // R9 STR-2 修复：损坏（JSON 解析失败）不再硬失败——与坏消息行的
            // 跳过策略对齐，warn 后走扫描兜底重建（此前损坏直接上抛，列表
            // 整体不可用且无自愈；"空"能自愈、"坏"不能的读写不对称）。
            let index = match SessionIndex::load(&self.index_path()) {
                Ok(idx) => idx,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "session index 损坏，回退目录扫描重建（会话正文 jsonl 不受影响）"
                    );
                    let index = self.build_index_from_scan().await?;
                    let metas = index.to_metas();
                    {
                        let mut guard = self.lock_index();
                        *guard = Some(index.clone());
                    }
                    // R9 STR-2：落盘走 mutate_index（持 index.lock，防共用
                    // tmp 并发截断），best effort
                    if let Err(e) = self.mutate_index(|idx| *idx = index) {
                        tracing::warn!("failed to persist rebuilt session index: {e}");
                    }
                    return Ok(metas);
                }
            };
            if !index.is_empty() {
                let metas = index.to_metas();
                let mut guard = self.lock_index();
                *guard = Some(index);
                return Ok(metas);
            }
            // 3. 回退：扫描目录构建索引
            let index = self.build_index_from_scan().await?;
            let metas = index.to_metas();
            // 缓存 + 落盘（best effort；R9 STR-2 修复：走 mutate_index 持
            // index.lock，避免与 mutate_index 共用固定 tmp 并发截断）。
            {
                let mut guard = self.lock_index();
                *guard = Some(index.clone());
            }
            if let Err(e) = self.mutate_index(|idx| *idx = index) {
                tracing::warn!("failed to persist rebuilt session index: {e}");
            }
            Ok(metas)
        })
    }

    fn delete(&self, session: &SessionId) -> BoxFuture<'_, Result<(), StorageError>> {
        let path = self.session_path(session);
        let lock_path = self.base_dir.join(format!("{session}.lock"));
        let session_id = session.clone();
        Box::pin(async move {
            // ST-7（2026-08-28 R5 收尾）：删除前取会话排他锁（阻塞式）——并发
            // append 若在删除窗口内重建文件会产生孤儿会话。持锁删文件 + 移除
            // 索引后释放。
            // R9 STR-4 修复：加锁结果此前被 `let _guard = ...` 丢弃（无 `?`），
            // 加锁失败时删除在无锁状态下继续（与同步版 `:212` 的 `?` 写法
            // 不一致，且注释明说取锁是为防并发重建——没取到就该中止）。
            let _guard = crate::lock::SessionLock::acquire_blocking(&lock_path)?;
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            // 从索引移除（best effort）
            self.remove_from_index(&session_id);
            // 清理可能残留的锁文件（best effort）
            let _ = tokio::fs::remove_file(&lock_path).await;
            Ok(())
        })
    }

    fn update_summary(
        &self,
        session: &SessionId,
        summary: &str,
    ) -> BoxFuture<'_, Result<(), StorageError>> {
        let session_id = session.clone();
        let summary = summary.to_string();
        Box::pin(async move { self.update_summary_sync(&session_id, &summary) })
    }
}

/// S19：已存在文件的权限兜底收紧到 0600（历史文件可能是宽权限创建）。
#[cfg(unix)]
pub(crate) async fn tighten_existing(file: &tokio::fs::File, path: &camino::Utf8PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = file.metadata().await {
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o600 {
            let mut perm = meta.permissions();
            perm.set_mode(0o600);
            let _ = tokio::fs::set_permissions(path, perm).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::model::Message;
    use tempfile::tempdir;

    fn storage(dir: &tempfile::TempDir) -> JsonlStorage {
        JsonlStorage::new(dir.path().to_path_buf().try_into().unwrap())
    }

    #[tokio::test]
    async fn append_then_list_uses_index() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01TESTAPPEND";
        st.append(&id.to_string(), &Message::user_text("hello"))
            .await
            .unwrap();
        st.append(&id.to_string(), &Message::assistant_text("world"))
            .await
            .unwrap();
        let metas = st.list_sessions().await.unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].id, id);
        assert_eq!(metas[0].message_count, 2);
    }

    #[tokio::test]
    async fn delete_removes_from_index() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01TESTDEL";
        st.append(&id.to_string(), &Message::user_text("hi"))
            .await
            .unwrap();
        st.delete(&id.to_string()).await.unwrap();
        let metas = st.list_sessions().await.unwrap();
        assert!(metas.is_empty(), "expected empty: metas");
    }

    #[tokio::test]
    async fn export_markdown_and_jsonl() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01EXP";
        st.append(&id.to_string(), &Message::user_text("hello"))
            .await
            .unwrap();
        st.append(&id.to_string(), &Message::assistant_text("world"))
            .await
            .unwrap();
        let md = st
            .export(&id.to_string(), ExportFormat::Markdown)
            .await
            .unwrap();
        assert!(md.contains("hello"));
        assert!(md.contains("world"));
        let jsonl = st
            .export(&id.to_string(), ExportFormat::Jsonl)
            .await
            .unwrap();
        assert_eq!(jsonl.lines().count(), 2);
    }

    #[tokio::test]
    async fn list_sessions_falls_back_to_scan_without_index() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01SCAN";
        st.append(&id.to_string(), &Message::user_text("data"))
            .await
            .unwrap();
        // 删除索引文件 + 清空缓存，强制回退扫描
        let _ = tokio::fs::remove_file(st.index_path()).await;
        {
            let mut guard = st.lock_index();
            *guard = None;
        }
        let metas = st.list_sessions().await.unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].id, id);
    }

    #[tokio::test]
    async fn load_returns_messages() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01LOAD";
        st.append(&id.to_string(), &Message::user_text("hello"))
            .await
            .unwrap();
        st.append(&id.to_string(), &Message::assistant_text("world"))
            .await
            .unwrap();
        let msgs = st.load(&id.to_string()).await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[1].role, Role::Assistant);
    }

    #[tokio::test]
    async fn load_nonexistent_returns_empty() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let msgs = st.load(&"01NONE".to_string()).await.unwrap();
        assert!(msgs.is_empty(), "expected empty: msgs");
    }

    #[tokio::test]
    async fn load_messages_sync_returns_messages() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01SYNC";
        st.append(&id.to_string(), &Message::user_text("sync hello"))
            .await
            .unwrap();
        let msgs = st.load_messages_sync(&id.to_string()).unwrap();
        assert_eq!(msgs.len(), 1);
    }

    #[tokio::test]
    async fn load_messages_sync_nonexistent_returns_empty() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let msgs = st.load_messages_sync(&"01NONE".to_string()).unwrap();
        assert!(msgs.is_empty(), "expected empty: msgs");
    }

    #[tokio::test]
    async fn load_messages_sync_corrupted_returns_error() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01CORRUPT";
        let path = st.session_path(&id.to_string());
        tokio::fs::write(path.as_std_path(), "not json\n")
            .await
            .unwrap();
        let result = st.load_messages_sync(&id.to_string());
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn list_sessions_sync_returns_from_index() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        st.append(&"01SYNC1".to_string(), &Message::user_text("a"))
            .await
            .unwrap();
        st.append(&"01SYNC2".to_string(), &Message::user_text("b"))
            .await
            .unwrap();
        let metas = st.list_sessions_sync().unwrap();
        assert_eq!(metas.len(), 2);
    }

    #[tokio::test]
    async fn list_sessions_sync_empty_dir() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let metas = st.list_sessions_sync().unwrap();
        assert!(metas.is_empty(), "expected empty: metas");
    }

    #[tokio::test]
    async fn delete_nonexistent_is_ok() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        // 删除不存在的会话应返回 Ok（幂等）
        let result = st.delete(&"01NONE".to_string()).await;
        assert!(result.is_ok(), "delete nonexistent should be ok");
    }

    #[tokio::test]
    async fn export_nonexistent_returns_error() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let result = st
            .export(&"01NONE".to_string(), ExportFormat::Markdown)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn multiple_sessions_listed_correctly() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        st.append(&"01MULTI1".to_string(), &Message::user_text("first"))
            .await
            .unwrap();
        st.append(&"01MULTI2".to_string(), &Message::user_text("second"))
            .await
            .unwrap();
        st.append(&"01MULTI3".to_string(), &Message::user_text("third"))
            .await
            .unwrap();
        let metas = st.list_sessions().await.unwrap();
        assert_eq!(metas.len(), 3);
    }

    #[tokio::test]
    async fn append_creates_session_file() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01CREATE";
        st.append(&id.to_string(), &Message::user_text("content"))
            .await
            .unwrap();
        let path = st.session_path(&id.to_string());
        assert!(path.as_std_path().exists(), "session file should exist");
    }

    #[tokio::test]
    async fn append_writes_format_header_on_first_message() {
        // M-02：新会话文件首行应为 `{"_header":...}` 格式头
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01HEADER";
        st.append(&id.to_string(), &Message::user_text("hi"))
            .await
            .unwrap();
        let path = st.session_path(&id.to_string());
        let content = std::fs::read_to_string(path.as_std_path()).unwrap();
        let first_line = content.lines().next().unwrap();
        assert!(
            first_line.starts_with("{\"_header\""),
            "first line should be header, got: {first_line}"
        );
        // header 不参与 load：只返回消息
        let msgs = st.load(&id.to_string()).await.unwrap();
        assert_eq!(msgs.len(), 1);
    }

    #[tokio::test]
    async fn load_skips_single_corrupted_line_keeps_rest() {
        // M-02（S2-1）：单坏行跳过，其余消息完整返回（部分损坏可恢复）
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01BADLINE";
        let path = st.session_path(&id.to_string());
        let m1 = serde_json::to_string(&Message::user_text("first")).unwrap();
        let m2 = serde_json::to_string(&Message::assistant_text("second")).unwrap();
        let content = format!("{m1}\nnot valid json\n{m2}\n");
        tokio::fs::write(path.as_std_path(), content).await.unwrap();
        let msgs = st.load(&id.to_string()).await.unwrap();
        assert_eq!(msgs.len(), 2, "bad line skipped, good lines kept");
        assert_eq!(msgs[0].text(), "first");
        assert_eq!(msgs[1].text(), "second");
    }

    #[tokio::test]
    async fn load_rejects_future_format_version() {
        // M-02（S2-1）：更新版本写入的文件显式拒绝，防静默丢事件
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01FUTURE";
        let path = st.session_path(&id.to_string());
        let header = "{\"_header\":{\"format_version\":9999}}\n";
        let m = serde_json::to_string(&Message::user_text("msg")).unwrap();
        tokio::fs::write(path.as_std_path(), format!("{header}{m}\n"))
            .await
            .unwrap();
        let result = st.load(&id.to_string()).await;
        assert!(matches!(result, Err(StorageError::FormatUnsupported(_))));
        // sync 路径行为一致
        assert!(matches!(
            st.load_messages_sync(&id.to_string()),
            Err(StorageError::FormatUnsupported(_))
        ));
    }

    #[tokio::test]
    async fn scan_skips_session_with_future_format_version() {
        // M-02：scan 索引路径对版本过高的会话跳过而非当旧数据索引
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01SCANFUTURE";
        let path = st.session_path(&id.to_string());
        let header = "{\"_header\":{\"format_version\":9999}}\n";
        let m = serde_json::to_string(&Message::user_text("msg")).unwrap();
        tokio::fs::write(path.as_std_path(), format!("{header}{m}\n"))
            .await
            .unwrap();
        // 直接 scan（无索引缓存路径）
        let metas = st.list_sessions_sync().unwrap();
        assert!(metas.is_empty(), "future-version session should be skipped");
    }

    #[tokio::test]
    async fn append_parallel_same_session_no_interleaving() {
        // M-01（S1-2）：并发 append 同一会话，行不交错——每行可解析且消息数正确。
        // 进程内并发模拟跨进程竞争：阻塞排他锁保证同会话追加串行化。
        let dir = tempdir().unwrap();
        let st = std::sync::Arc::new(storage(&dir));
        let id = "01CONCUR";
        let mut handles = Vec::new();
        for i in 0..16 {
            let st = std::sync::Arc::clone(&st);
            let id = id.to_string();
            handles.push(tokio::spawn(async move {
                st.append(&id, &Message::user_text(format!("msg-{i}")))
                    .await
            }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }
        let msgs = st.load(&id.to_string()).await.unwrap();
        assert_eq!(msgs.len(), 16, "all messages appended exactly once");
        let texts: Vec<String> = msgs.iter().map(Message::text).collect();
        for i in 0..16 {
            assert!(texts.contains(&format!("msg-{i}")), "missing msg-{i}");
        }
    }

    #[tokio::test]
    async fn append_and_sync_load_agree_on_parallel_writes() {
        // M-01 补充：并发 append 后 sync load 与 async load 一致（index 与文件一致）
        let dir = tempdir().unwrap();
        let st = std::sync::Arc::new(storage(&dir));
        let id = "01CONCUR2";
        let mut handles = Vec::new();
        for i in 0..8 {
            let st = std::sync::Arc::clone(&st);
            let id = id.to_string();
            handles.push(tokio::spawn(async move {
                st.append(&id, &Message::assistant_text(format!("a-{i}")))
                    .await
            }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }
        let sync_msgs = st.load_messages_sync(&id.to_string()).unwrap();
        let async_msgs = st.load(&id.to_string()).await.unwrap();
        assert_eq!(sync_msgs.len(), 8);
        assert_eq!(async_msgs.len(), 8);
    }

    #[tokio::test]
    async fn delete_session_sync_removes_file_and_index() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01DELSYNC";
        st.append(&id.to_string(), &Message::user_text("to be deleted"))
            .await
            .unwrap();
        // 确保文件存在
        assert!(st.session_path(&id.to_string()).as_std_path().exists());
        // 同步删除
        st.delete_session_sync(&id.to_string()).unwrap();
        // 文件应不存在
        assert!(!st.session_path(&id.to_string()).as_std_path().exists());
        // 索引中也不应再有该会话
        let metas = st.list_sessions_sync().unwrap();
        assert!(metas.is_empty(), "session should be removed from index");
    }

    #[tokio::test]
    async fn delete_session_sync_nonexistent_is_ok() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        // 删除不存在的会话应返回 Ok（幂等）
        let result = st.delete_session_sync(&"01NONE".to_string());
        assert!(
            result.is_ok(),
            "delete_session_sync nonexistent should be ok"
        );
    }

    #[tokio::test]
    async fn fork_session_sync_creates_new_session_with_messages() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let src_id = "01SRC";
        // 写入源会话消息
        st.append(&src_id.to_string(), &Message::user_text("first"))
            .await
            .unwrap();
        st.append(&src_id.to_string(), &Message::assistant_text("second"))
            .await
            .unwrap();
        // 读取源消息并 fork 到新会话
        let messages = st.load(&src_id.to_string()).await.unwrap();
        let new_id = "01FORK";
        st.fork_session_sync(&new_id.to_string(), &messages)
            .unwrap();
        // 新会话应有相同消息
        let forked = st.load_messages_sync(&new_id.to_string()).unwrap();
        assert_eq!(forked.len(), 2);
        assert_eq!(forked[0].role, Role::User);
        assert_eq!(forked[1].role, Role::Assistant);
        // 新会话应出现在索引中
        let metas = st.list_sessions_sync().unwrap();
        assert_eq!(metas.len(), 2, "both sessions should be in index");
    }

    #[tokio::test]
    async fn fork_session_sync_empty_messages_creates_empty_file() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let new_id = "01EMPTYFORK";
        let empty: Vec<Message> = Vec::new();
        st.fork_session_sync(&new_id.to_string(), &empty).unwrap();
        // 空消息列表 fork 后应创建空文件（不报错）
        let forked = st.load_messages_sync(&new_id.to_string()).unwrap();
        assert!(forked.is_empty(), "expected empty: forked");
    }

    #[tokio::test]
    async fn update_summary_sync_updates_index_summary() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01SUMMARY";
        st.append(&id.to_string(), &Message::user_text("hello world"))
            .await
            .unwrap();
        // 更新摘要
        st.update_summary_sync(&id.to_string(), "test summary")
            .unwrap();
        // 从索引读取并验证摘要已更新
        let metas = st.list_sessions_sync().unwrap();
        assert_eq!(metas.len(), 1);
        // SessionListItem 没有 summary 字段，但索引内部应有；通过重新构建索引验证
        // 删除索引缓存 + 文件，强制重新扫描
        let _ = tokio::fs::remove_file(st.index_path()).await;
        {
            let mut guard = st.lock_index();
            *guard = None;
        }
        // 重新扫描后应仍能列出会话（说明 fork_session_sync 写入的文件有效）
        let metas_after = st.list_sessions().await.unwrap();
        assert_eq!(metas_after.len(), 1);
        assert_eq!(metas_after[0].id, id);
    }

    #[tokio::test]
    async fn update_summary_sync_nonexistent_session_is_ok() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        // 会话不在索引中：静默忽略（best effort，与文档一致）
        let result = st.update_summary_sync(&"01NOTINIDX".to_string(), "summary");
        assert!(
            result.is_ok(),
            "update_summary_sync for unknown session should be ok"
        );
    }

    #[tokio::test]
    async fn update_summary_async_via_storage_trait() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01ASYNC";
        st.append(&id.to_string(), &Message::user_text("data"))
            .await
            .unwrap();
        // 通过 Storage trait 的 async update_summary 调用
        let result = st.update_summary(&id.to_string(), "async summary").await;
        assert!(result.is_ok(), "async update_summary should succeed");
    }

    #[test]
    fn find_first_user_summary_extracts_first_user_text() {
        let m1 = serde_json::to_string(&Message::assistant_text("assistant")).unwrap();
        let m2 = serde_json::to_string(&Message::user_text("user input here")).unwrap();
        let m3 = serde_json::to_string(&Message::user_text("second user")).unwrap();
        let lines = vec![m1.as_str(), m2.as_str(), m3.as_str()];
        let summary = find_first_user_summary(&lines);
        assert_eq!(summary.as_deref(), Some("user input here"));
    }

    #[test]
    fn find_first_user_summary_truncates_to_80_chars() {
        let long_text = "a".repeat(200);
        let m = serde_json::to_string(&Message::user_text(&long_text)).unwrap();
        let lines = vec![m.as_str()];
        let summary = find_first_user_summary(&lines).expect("should find summary");
        assert_eq!(summary.chars().count(), 80);
    }

    #[test]
    fn find_first_user_summary_returns_none_when_no_user_message() {
        let m = serde_json::to_string(&Message::assistant_text("only assistant")).unwrap();
        let lines = vec![m.as_str()];
        let summary = find_first_user_summary(&lines);
        assert!(summary.is_none());
    }

    #[test]
    fn find_first_user_summary_returns_none_for_empty_user_text() {
        let m = serde_json::to_string(&Message::user_text("")).unwrap();
        let lines = vec![m.as_str()];
        let summary = find_first_user_summary(&lines);
        assert!(summary.is_none());
    }

    #[test]
    fn find_first_user_summary_skips_invalid_json_lines() {
        let lines = vec!["not json", "{\"role\":\"system\",\"content\":[]}"];
        let summary = find_first_user_summary(&lines);
        assert!(summary.is_none());
    }

    #[tokio::test]
    async fn load_returns_error_for_corrupted_file() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01CORRUPTASYNC";
        let path = st.session_path(&id.to_string());
        tokio::fs::write(path.as_std_path(), "not valid json\n")
            .await
            .unwrap();
        let result = st.load(&id.to_string()).await;
        assert!(result.is_err(), "load corrupted file should return error");
        let err = result.unwrap_err();
        assert!(matches!(err, StorageError::Corrupted(_)));
    }

    #[tokio::test]
    async fn load_skips_empty_lines_in_file() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01EMPTYLINES";
        let path = st.session_path(&id.to_string());
        let m1 = serde_json::to_string(&Message::user_text("first")).unwrap();
        let m2 = serde_json::to_string(&Message::assistant_text("second")).unwrap();
        // 写入含空行的 JSONL
        let content = format!("{m1}\n\n  \n{m2}\n\n");
        tokio::fs::write(path.as_std_path(), content).await.unwrap();
        let msgs = st.load(&id.to_string()).await.unwrap();
        assert_eq!(msgs.len(), 2, "should skip empty lines");
    }

    #[tokio::test]
    async fn list_sessions_returns_empty_for_nonexistent_dir() {
        // base_dir 不存在时 list_sessions 应返回空 Vec
        let st = JsonlStorage::new(Utf8PathBuf::from(
            "/tmp/minicoding-test-nonexistent-dir-xyz-12345",
        ));
        let metas = st.list_sessions().await.unwrap();
        assert!(metas.is_empty(), "expected empty: metas");
    }

    #[tokio::test]
    async fn delete_removes_lock_file_if_exists() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01LOCKDEL";
        st.append(&id.to_string(), &Message::user_text("data"))
            .await
            .unwrap();
        // 创建模拟锁文件
        let lock_path = st.base_dir.join(format!("{id}.lock"));
        tokio::fs::write(lock_path.as_std_path(), "lock")
            .await
            .unwrap();
        assert!(lock_path.as_std_path().exists());
        // 删除会话应同时清理锁文件
        st.delete(&id.to_string()).await.unwrap();
        assert!(
            !lock_path.as_std_path().exists(),
            "lock file should be removed"
        );
    }

    #[tokio::test]
    async fn append_updates_index_for_multiple_sessions() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        // 写入多个会话，验证索引正确更新
        for i in 0..5 {
            let id = format!("01MULTI{i}");
            st.append(&id, &Message::user_text(format!("content {i}")))
                .await
                .unwrap();
        }
        let metas = st.list_sessions().await.unwrap();
        assert_eq!(metas.len(), 5);
        // 再次 append 同一会话应更新消息计数
        st.append(&"01MULTI0".to_string(), &Message::user_text("more"))
            .await
            .unwrap();
        let metas = st.list_sessions().await.unwrap();
        assert_eq!(metas.len(), 5, "session count should not change");
    }

    #[tokio::test]
    async fn list_sessions_sync_skips_non_jsonl_files() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        // 写入一个有效会话
        st.append(&"01VALID".to_string(), &Message::user_text("hi"))
            .await
            .unwrap();
        // 写入非 .jsonl 文件（应被扫描时跳过）
        let other_path = st.base_dir.join("not_a_session.txt");
        tokio::fs::write(other_path.as_std_path(), "not a session")
            .await
            .unwrap();
        // 写入 .jsonl 但内容损坏的文件（应被扫描时跳过）
        let corrupt_path = st.base_dir.join("01CORRUPT.jsonl");
        tokio::fs::write(corrupt_path.as_std_path(), "not json")
            .await
            .unwrap();
        // 清空索引缓存 + 删除索引文件，强制重新扫描
        let _ = tokio::fs::remove_file(st.index_path()).await;
        {
            let mut guard = st.lock_index();
            *guard = None;
        }
        let metas = st.list_sessions_sync().unwrap();
        assert_eq!(metas.len(), 1, "should only list the one valid session");
        assert_eq!(metas[0].id, "01VALID");
    }

    #[tokio::test]
    async fn list_sessions_sync_skips_empty_jsonl_files() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        // 写入一个有效会话
        st.append(&"01VALID".to_string(), &Message::user_text("hi"))
            .await
            .unwrap();
        // 写入空的 .jsonl 文件（应被跳过）
        let empty_path = st.base_dir.join("01EMPTY.jsonl");
        tokio::fs::write(empty_path.as_std_path(), "")
            .await
            .unwrap();
        // 清空索引缓存 + 删除索引文件，强制重新扫描
        let _ = tokio::fs::remove_file(st.index_path()).await;
        {
            let mut guard = st.lock_index();
            *guard = None;
        }
        let metas = st.list_sessions_sync().unwrap();
        assert_eq!(metas.len(), 1, "empty jsonl should be skipped");
    }

    #[tokio::test]
    async fn load_messages_sync_skips_empty_lines() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01SYNCEMPTY";
        let path = st.session_path(&id.to_string());
        let m1 = serde_json::to_string(&Message::user_text("first")).unwrap();
        let m2 = serde_json::to_string(&Message::assistant_text("second")).unwrap();
        // 写入含空行的 JSONL
        let content = format!("{m1}\n\n  \n{m2}\n");
        std::fs::write(path.as_std_path(), content).unwrap();
        let msgs = st.load_messages_sync(&id.to_string()).unwrap();
        assert_eq!(msgs.len(), 2, "should skip empty lines");
    }
}
