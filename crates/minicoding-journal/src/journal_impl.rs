//! `FileChangeJournal` 实现：纯内存文件改动账本 + `/undo` 冲突检测（T-M4-9）。
//!
//! 实现 `core::journal::Journal` trait。`undo` 反向遍历 entry，逐文件恢复 `before`
//! 状态；恢复前比对当前文件内容与 `after`，不一致记入 `failed_files`（C-28 不强行
//! 覆盖）。`reset_to_initial` 清空 journal（回到会话启动状态）。
//!
//! 恢复路径经 `validate_restore_path` 校验，拒绝 `..` 越界（C-03/C-28）。
//!
//! 内存上限（D3）：账本最多保留 [`MAX_JOURNAL_ENTRIES`]（200）条 entry，
//! `record` 超限丢弃最旧条目——undo 仅保证最近 200 步。取舍是内存上限换
//! 语义边界：无界账本在长会话中持续增长直至耗尽内存，而交互式 /undo 的
//! 实际回溯窗口远小于该值。

use crate::JournalError;
use minicoding_core::journal::{ChangeEntry, DiffEntry, FileChange, Journal, UndoReport};
use minicoding_core::otel::span_name;
use minicoding_core::provider::BoxFuture;
use std::sync::Mutex;
use tokio::fs;

/// journal entry 数上限（D3）：`record` 后超过此数即丢弃最旧条目。
///
/// 取舍见模块 doc；C-28 行为文档由主控统一收口。
const MAX_JOURNAL_ENTRIES: usize = 200;

/// 计算单条 `ChangeEntry` 的内存占用（近似：路径 + before/after 全文字节）。
fn entry_bytes(entry: &ChangeEntry) -> usize {
    entry
        .files
        .iter()
        .map(|f| {
            let path_len = f.path().as_str().len();
            path_len
                + match f {
                    FileChange::Written { before, after, .. } => {
                        before.as_ref().map_or(0, Vec::len) + after.len()
                    }
                    FileChange::Edited { before, after, .. } => before.len() + after.len(),
                    FileChange::Deleted { content, .. } | FileChange::Created { content, .. } => {
                        content.len()
                    }
                }
        })
        .sum::<usize>()
}

/// journal 总字节上限（ST-5，2026-08-27 R5 审查）：此前仅按条数限 200——
/// 每条 entry 携带 `before`/`after` 全文（`fs.write` 整文件），会话触碰多 MB
/// 文件可占用数百 MB-RAM（"内存上限"文档承诺未兑现）。按字节预算收缩：
/// 32 MiB 足够覆盖交互式 `/undo` 回溯窗口，且防大文件改动撑爆内存。
const MAX_JOURNAL_BYTES: usize = 32 * 1024 * 1024;

/// 文件改动 journal（纯内存，不落盘，C-28）。
///
/// 持有按操作顺序追加的 `ChangeEntry` 列表。`Mutex<Vec<ChangeEntry>>` 保护并发
/// 访问（Runtime 内 `Arc<dyn Journal>` 共享）。`undo` 从尾部反向遍历。
///
/// 容量双约束（D3 + ST-5）：列表长度 ≤ [`MAX_JOURNAL_ENTRIES`] 且累计字节
/// ≤ [`MAX_JOURNAL_BYTES`]，超限丢最旧（尾部 LIFO 撤销序不受影响）。
///
/// `workdir` 用于恢复路径校验（拒绝越界恢复，C-03/C-28）。若 `None`，路径校验
/// 仅拒绝绝对路径外的 `..` 逃逸（保守）。
pub struct FileChangeJournal {
    entries: Mutex<Vec<ChangeEntry>>,
    total_bytes: Mutex<usize>,
    workdir: Option<camino::Utf8PathBuf>,
}

impl FileChangeJournal {
    /// 创建空 journal。
    ///
    /// `workdir` 为恢复路径校验基准（应与 `Runtime::workdir` 一致）。`None` 时
    /// 仅做基本 `..` 逃逸检查。
    #[must_use]
    pub fn new(workdir: Option<camino::Utf8PathBuf>) -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            total_bytes: Mutex::new(0),
            workdir,
        }
    }

    /// 当前已记录的 entry 数（供测试与诊断用）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().map_or(0, |g| g.len())
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for FileChangeJournal {
    fn default() -> Self {
        Self::new(None)
    }
}

impl Journal for FileChangeJournal {
    #[tracing::instrument(skip(self), fields(otel.name = span_name::JOURNAL_RECORD, journal.op = "record"))]
    fn record(&self, entry: ChangeEntry) -> BoxFuture<'_, Result<(), JournalError>> {
        Box::pin(async move {
            let new_entry_bytes = entry_bytes(&entry);
            let mut guard = self
                .entries
                .lock()
                .map_err(|e| JournalError::Conflict(format!("journal lock poisoned: {e}")))?;
            guard.push(entry);
            // ST-5：累计字节预算追踪（含本条）——超限从头部丢弃最旧条目，
            // 尾部 LIFO 撤销序与 S-7 失败回推尾部原位的语义均不受影响。
            let mut budget = self
                .total_bytes
                .lock()
                .map_err(|e| JournalError::Conflict(format!("journal bytes lock poisoned: {e}")))?;
            *budget += new_entry_bytes;
            let mut dropped = 0usize;
            while guard.len() > MAX_JOURNAL_ENTRIES || *budget > MAX_JOURNAL_BYTES {
                if guard.is_empty() {
                    break;
                }
                let removed = guard.remove(0);
                *budget = budget.saturating_sub(entry_bytes(&removed));
                dropped += 1;
            }
            if dropped > 0 {
                tracing::debug!(
                    journal.dropped = dropped,
                    journal.bytes = *budget,
                    "journal 超容量约束（条数/字节），已丢弃最旧条目"
                );
            }
            Ok(())
        })
    }

    #[tracing::instrument(skip(self), fields(otel.name = span_name::JOURNAL_UNDO, journal.op = "undo"))]
    fn undo(&self, steps: usize) -> BoxFuture<'_, Result<UndoReport, JournalError>> {
        Box::pin(async move {
            // steps = 0 视为 1（撤销最近一次，符合 /undo 默认语义）
            let steps = steps.max(1);
            // 取出待撤销的 entry（从尾部反向）
            let to_undo: Vec<ChangeEntry> = {
                let mut guard = self
                    .entries
                    .lock()
                    .map_err(|e| JournalError::Conflict(format!("journal lock poisoned: {e}")))?;
                let available = guard.len();
                let take = steps.min(available);
                if take == 0 {
                    return Err(JournalError::NoEntries);
                }
                // 从尾部取出 take 个（保留前面 entry）
                let split_at = available - take;
                guard.split_off(split_at)
            };

            let mut report = UndoReport {
                undone_entries: to_undo.len(),
                ..Default::default()
            };

            // 反向遍历：先撤销最近的 entry，再撤销更早的（LIFO）。
            // 失败的 entry 回推账本头部（2026-08-23 审查 §10-P2）：此前
            // split_off 已把条目移出账本，用户解决外部冲突后无法再次 /undo。
            let mut failed_entries: Vec<ChangeEntry> = Vec::new();
            for mut entry in to_undo.into_iter().rev() {
                // entry 内文件也反向（恢复到该 entry 执行前的状态）
                // 全量尝试（保持原"单文件失败不中断整批撤销"语义，C-28）；
                // 失败的 change 收集回 entry——该 entry 整体回推账本，可重试
                let mut retry_changes: Vec<FileChange> = Vec::new();
                for change in entry.files.drain(..).rev() {
                    match restore_file(&change, self.workdir.as_ref()).await {
                        Ok(path) => report.restored_files.push(path),
                        Err(e) => {
                            let path = change.path().clone();
                            // 冲突错误记入 failed_files 不强行覆盖（C-28）
                            report.failed_files.push((path, e.to_string()));
                            retry_changes.push(change);
                        }
                    }
                }
                if !retry_changes.is_empty() {
                    entry.files = retry_changes;
                    failed_entries.push(entry);
                }
            }

            if !failed_entries.is_empty() {
                let mut guard = self
                    .entries
                    .lock()
                    .map_err(|e| JournalError::Conflict(format!("journal lock poisoned: {e}")))?;
                // 失败 entry 回推**尾部**原位（2026-08-25 审查 §6.2-S7）：此前
                // 拼到头部，下次 /undo 从尾弹出的是最老 entry 而非失败的新
                // entry——撤销时序错乱。failed_entries 当前最新在前，反转恢复
                // 账本时间序后 append 到尾部。
                let mut restored: Vec<ChangeEntry> = failed_entries.into_iter().rev().collect();
                let undone = report.undone_entries - restored.len();
                report.undone_entries = undone;
                guard.append(&mut restored);
            }

            Ok(report)
        })
    }

    fn diff(&self) -> BoxFuture<'_, Result<Vec<DiffEntry>, JournalError>> {
        Box::pin(async move {
            let guard = self
                .entries
                .lock()
                .map_err(|e| JournalError::Conflict(format!("journal lock poisoned: {e}")))?;
            Ok(guard
                .iter()
                .map(|e| DiffEntry {
                    op_id: e.op_id.clone(),
                    prompt_snippet: e.prompt_snippet.clone(),
                    files: e.files.clone(),
                })
                .collect())
        })
    }

    fn reset_to_initial(&self) -> BoxFuture<'_, Result<(), JournalError>> {
        Box::pin(async move {
            let mut guard = self
                .entries
                .lock()
                .map_err(|e| JournalError::Conflict(format!("journal lock poisoned: {e}")))?;
            guard.clear();
            Ok(())
        })
    }
}

/// 恢复单个文件到改动前状态（C-28 冲突检测）。
///
/// 恢复前比对当前文件内容与 `after`：
/// - 一致 → 恢复 `before`（写入旧内容 / 删除新建文件 / 恢复删除文件）；
/// - 不一致 → 返回 `JournalError::Conflict`（记入 `failed_files` 不强行覆盖）。
///
/// 路径经 `validate_restore_path` 校验，拒绝 `..` 越界（C-03/C-28）。
async fn restore_file(
    change: &FileChange,
    workdir: Option<&camino::Utf8PathBuf>,
) -> Result<camino::Utf8PathBuf, JournalError> {
    let path = change.path().clone();
    validate_restore_path(&path, workdir)?;
    // SEC-8（2026-08-25 R2 审查）：拒绝符号链接目标——workdir 内文件被外部换成
    // 指向外部（如 ~/.ssh）的 symlink 且内容恰与 after 一致时，恢复写会穿透
    // symlink 出界。`symlink_metadata` 不跟随末段链接，词法校验
    // （validate_restore_path）无法发现这类替换。
    ensure_not_symlink(&path, workdir).await?;

    match change {
        FileChange::Written { before, after, .. } => {
            // 比对当前内容与 after
            verify_current_matches(&path, after).await?;
            match before {
                Some(old) => {
                    // 文件已存在，恢复旧内容
                    fs::write(path.as_std_path(), old).await?;
                }
                None => {
                    // before 为 None → 原本是新建，撤销时删除文件
                    match fs::remove_file(path.as_std_path()).await {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            // 文件已不存在，视为已恢复（幂等）
                        }
                        Err(e) => return Err(JournalError::Io(e)),
                    }
                }
            }
        }
        FileChange::Edited { before, after, .. } => {
            verify_current_matches(&path, after).await?;
            fs::write(path.as_std_path(), before).await?;
        }
        FileChange::Created { content, .. } => {
            // Created 撤销 = 删除文件。先校验当前内容 == content（未被外部改）
            match fs::read(path.as_std_path()).await {
                Ok(current) => {
                    if current != *content {
                        return Err(JournalError::Conflict(format!(
                            "file {path} modified after creation, refusing to delete"
                        )));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // 文件已不存在，视为已恢复（幂等）
                    return Ok(path);
                }
                Err(e) => return Err(JournalError::Io(e)),
            }
            fs::remove_file(path.as_std_path()).await?;
        }
        FileChange::Deleted { content, .. } => {
            // Deleted 撤销 = 恢复文件。Deleted 的 after 状态是"文件不存在"。
            match fs::read(path.as_std_path()).await {
                Ok(_) => {
                    // 文件已存在 → 说明删除后被外部重建，冲突不强行覆盖
                    return Err(JournalError::Conflict(format!(
                        "file {path} recreated after deletion, refusing to overwrite"
                    )));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // 文件确实不存在，符合 after 状态，恢复 content
                }
                Err(e) => return Err(JournalError::Io(e)),
            }
            fs::write(path.as_std_path(), content).await?;
        }
    }
    Ok(path)
}

/// 校验当前文件内容与 `after` 一致（冲突检测核心，C-28）。
///
/// 文件不存在时：`after` 为空视为一致（删除操作的 after 状态）；否则冲突。
async fn verify_current_matches(
    path: &camino::Utf8PathBuf,
    after: &[u8],
) -> Result<(), JournalError> {
    match fs::read(path.as_std_path()).await {
        Ok(current) => {
            if current.as_slice() == after {
                Ok(())
            } else {
                Err(JournalError::Conflict(format!(
                    "file {path} modified externally, refusing to overwrite"
                )))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // 文件不存在：after 为空则一致（删除态），否则冲突
            if after.is_empty() {
                Ok(())
            } else {
                Err(JournalError::Conflict(format!(
                    "file {path} missing, expected content present"
                )))
            }
        }
        Err(e) => Err(JournalError::Io(e)),
    }
}

/// 拒绝恢复路径中**任何**组件为符号链接（SEC-8 升级，2026-08-29 R8 审查）。
///
/// 原实现仅检查末段（`symlink_metadata` 不跟随末段链接，见 `restore_file` 内注释）；
/// 中段目录若为指向外部的符号链接（如 `workdir/link/secret.txt`，`link`→`~/.ssh`），
/// 恢复写会穿透中段链接出界（C-03/C-28 逃逸）。逐组件检查每个前缀，任一组件是
/// 符号链接即拒绝。某前缀不存在时后续更深组件必不存在（无 symlink 风险），提前
/// 返回通过（对应 `Deleted` 变体的"文件不存在"幂等恢复路径）。
async fn ensure_not_symlink(
    path: &camino::Utf8PathBuf,
    workdir: Option<&camino::Utf8PathBuf>,
) -> Result<(), JournalError> {
    // 校验范围限定在用户可控路径（R9 CI 修复：macOS 上 `/tmp`、`/var` 是系统级
    // symlink——`/tmp`→`/private/tmp`、`/var`→`/private/var`——journal 记录的是
    // workdir 内文件，遍历组件时把系统路径当 symlink 误判为 PathEscaped 导致
    // undo 全挂）。有 workdir 时只检查 workdir 之后的组件；无 workdir（测试/
    // 兜底）时跳过根与根下第一级（系统挂载点所在层）。
    let skip = if path.is_absolute() {
        match workdir {
            Some(wd) if wd.is_absolute() && path.starts_with(wd) => wd.components().count(),
            _ => 2, // RootDir + 根下第一级（系统挂载点 /tmp、/var 等）
        }
    } else {
        0 // 相对路径全组件检查
    };
    let mut prefix = camino::Utf8PathBuf::new();
    for (i, comp) in path.components().enumerate() {
        prefix.push(comp.as_str());
        if i < skip {
            continue;
        }
        match fs::symlink_metadata(prefix.as_std_path()).await {
            Ok(md) if md.file_type().is_symlink() => {
                return Err(JournalError::PathEscaped(format!(
                    "{prefix} is a symbolic link; refusing to restore through it"
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(JournalError::Io(e)),
        }
    }
    Ok(())
}

/// 校验恢复路径不越界（C-03/C-28）。
///
/// 拒绝包含 `..` 的路径段逃逸（绝对路径不受限，因为 fs 工具写入时已校验过 workdir；
/// journal 恢复的是已记录的合法路径，此处只防 `..` 注入）。若提供 `workdir`，相对
/// 路径会规范化后检查是否仍在 workdir 下。
fn validate_restore_path(
    path: &camino::Utf8PathBuf,
    workdir: Option<&camino::Utf8PathBuf>,
) -> Result<(), JournalError> {
    // 拒绝任何 `..` 段（保守：即使合法的 `a/../b` 也拒绝，因为 journal 记录的
    // 应是已规范化的路径）
    for comp in path.components() {
        if comp.as_str() == ".." {
            return Err(JournalError::PathEscaped(path.to_string()));
        }
    }
    // 若有 workdir：组件级前缀包容检查（S18 升级——
    // ①字符串 starts_with 有 `/tmp/abc` vs `/tmp/abc-evil` 边界误判；
    // ②原实现仅覆盖相对路径，绝对路径完全绕过包容检查）
    if let Some(wd) = workdir {
        let joined = if path.is_relative() {
            wd.join(path)
        } else {
            path.clone()
        };
        let wd_components: Vec<_> = wd.components().collect();
        let joined_components: Vec<_> = joined.components().collect();
        if joined_components.len() < wd_components.len()
            || joined_components[..wd_components.len()] != wd_components[..]
        {
            return Err(JournalError::PathEscaped(path.to_string()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use camino::Utf8PathBuf;
    use minicoding_core::journal::{ChangeEntry, FileChange, Journal};
    use tempfile::TempDir;
    use time::OffsetDateTime;

    fn entry(op: &str, files: Vec<FileChange>) -> ChangeEntry {
        ChangeEntry {
            op_id: op.into(),
            ts: OffsetDateTime::now_utc(),
            prompt_snippet: format!("prompt-{op}"),
            files,
        }
    }

    #[tokio::test]
    async fn record_and_diff() {
        let j = FileChangeJournal::new(None);
        assert!(j.is_empty(), "expected empty: j");
        j.record(entry("op1", vec![])).await.unwrap();
        j.record(entry("op2", vec![])).await.unwrap();
        assert_eq!(j.len(), 2);
        let d = j.diff().await.unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].op_id, "op1");
    }

    #[tokio::test]
    async fn undo_write_restores_before() {
        let tmp = TempDir::new().unwrap();
        let file = Utf8PathBuf::from_path_buf(tmp.path().join("a.txt")).unwrap();
        // 初始内容
        fs::write(file.as_std_path(), b"old").await.unwrap();
        let j = FileChangeJournal::new(None);
        j.record(entry(
            "op1",
            vec![FileChange::Written {
                path: file.clone(),
                before: Some(b"old".to_vec()),
                after: b"new".to_vec(),
            }],
        ))
        .await
        .unwrap();
        // 改为 new
        fs::write(file.as_std_path(), b"new").await.unwrap();
        let report = j.undo(1).await.unwrap();
        assert_eq!(report.undone_entries, 1);
        assert_eq!(report.restored_files, vec![file.clone()]);
        assert!(
            report.failed_files.is_empty(),
            "expected empty: report.failed_files"
        );
        let restored = fs::read(file.as_std_path()).await.unwrap();
        assert_eq!(restored, b"old");
        // undo 后 journal 清空
        assert!(j.is_empty(), "expected empty: j");
    }

    #[tokio::test]
    async fn undo_conflict_records_failed_files() {
        let tmp = TempDir::new().unwrap();
        let file = Utf8PathBuf::from_path_buf(tmp.path().join("b.txt")).unwrap();
        let j = FileChangeJournal::new(None);
        j.record(entry(
            "op1",
            vec![FileChange::Written {
                path: file.clone(),
                before: Some(b"old".to_vec()),
                after: b"new".to_vec(),
            }],
        ))
        .await
        .unwrap();
        // 模拟外部编辑：内容不是 after
        fs::write(file.as_std_path(), b"externally-changed")
            .await
            .unwrap();
        let report = j.undo(1).await.unwrap();
        // 语义更新（2026-08-23 审查 §10-P2）：失败 entry 回推账本可重试，
        // 故未计入 undone_entries（此前 split_off 后不可重试，记 1）
        assert_eq!(report.undone_entries, 0);
        assert!(!j.is_empty(), "失败的 entry 应保留在账本中供再次 /undo");
        assert!(
            report.restored_files.is_empty(),
            "expected empty: report.restored_files"
        );
        assert_eq!(report.failed_files.len(), 1);
        assert_eq!(report.failed_files[0].0, file);
        // 文件未被覆盖
        let cur = fs::read(file.as_std_path()).await.unwrap();
        assert_eq!(cur, b"externally-changed");
        // 解决冲突后可重试：恢复为 after 内容后再次 /undo 成功
        fs::write(file.as_std_path(), b"new").await.unwrap();
        let retry = j.undo(1).await.unwrap();
        assert_eq!(retry.undone_entries, 1);
        assert!(retry.failed_files.is_empty());
        let restored = fs::read(file.as_std_path()).await.unwrap();
        assert_eq!(restored, b"old");
        assert!(j.is_empty());
    }

    #[tokio::test]
    async fn undo_created_deletes_file() {
        let tmp = TempDir::new().unwrap();
        let file = Utf8PathBuf::from_path_buf(tmp.path().join("c.txt")).unwrap();
        let j = FileChangeJournal::new(None);
        j.record(entry(
            "op1",
            vec![FileChange::Created {
                path: file.clone(),
                content: b"created".to_vec(),
            }],
        ))
        .await
        .unwrap();
        fs::write(file.as_std_path(), b"created").await.unwrap();
        let report = j.undo(1).await.unwrap();
        assert_eq!(report.restored_files, vec![file.clone()]);
        assert!(!file.as_std_path().exists());
    }

    #[tokio::test]
    async fn undo_deleted_restores_content() {
        let tmp = TempDir::new().unwrap();
        let file = Utf8PathBuf::from_path_buf(tmp.path().join("d.txt")).unwrap();
        let j = FileChangeJournal::new(None);
        j.record(entry(
            "op1",
            vec![FileChange::Deleted {
                path: file.clone(),
                content: b"was-deleted".to_vec(),
            }],
        ))
        .await
        .unwrap();
        // 删除操作后文件不存在
        let report = j.undo(1).await.unwrap();
        assert_eq!(report.restored_files, vec![file.clone()]);
        let restored = fs::read(file.as_std_path()).await.unwrap();
        assert_eq!(restored, b"was-deleted");
    }

    #[tokio::test]
    async fn undo_multiple_steps_lifo() {
        let tmp = TempDir::new().unwrap();
        let f1 = Utf8PathBuf::from_path_buf(tmp.path().join("x.txt")).unwrap();
        let f2 = Utf8PathBuf::from_path_buf(tmp.path().join("y.txt")).unwrap();
        fs::write(f1.as_std_path(), b"v1").await.unwrap();
        fs::write(f2.as_std_path(), b"v1").await.unwrap();
        let j = FileChangeJournal::new(None);
        j.record(entry(
            "op1",
            vec![FileChange::Written {
                path: f1.clone(),
                before: Some(b"v1".to_vec()),
                after: b"v2".to_vec(),
            }],
        ))
        .await
        .unwrap();
        fs::write(f1.as_std_path(), b"v2").await.unwrap();
        j.record(entry(
            "op2",
            vec![FileChange::Written {
                path: f2.clone(),
                before: Some(b"v1".to_vec()),
                after: b"v2".to_vec(),
            }],
        ))
        .await
        .unwrap();
        fs::write(f2.as_std_path(), b"v2").await.unwrap();
        let report = j.undo(2).await.unwrap();
        assert_eq!(report.undone_entries, 2);
        assert_eq!(report.restored_files.len(), 2);
        assert!(j.is_empty(), "expected empty: j");
    }

    #[tokio::test]
    async fn undo_no_entries_returns_err() {
        let j = FileChangeJournal::new(None);
        let res = j.undo(1).await;
        assert!(matches!(res, Err(JournalError::NoEntries)));
    }

    #[tokio::test]
    async fn failed_entry_retry_targets_newest_first() {
        // 2026-08-25 审查 §6.2-S7：失败 entry 应回推尾部原位——批量撤销中
        // 新 entry 冲突、旧 entry 成功后，再次 /undo 应首先重试**新** entry
        let tmp = TempDir::new().unwrap();
        let f_old = Utf8PathBuf::from_path_buf(tmp.path().join("old.txt")).unwrap();
        let f_new = Utf8PathBuf::from_path_buf(tmp.path().join("new.txt")).unwrap();
        fs::write(f_old.as_std_path(), b"v1").await.unwrap();
        fs::write(f_new.as_std_path(), b"v1").await.unwrap();
        let j = FileChangeJournal::new(None);
        // 旧 entry（先入账本）
        j.record(entry(
            "op-old",
            vec![FileChange::Written {
                path: f_old.clone(),
                before: Some(b"v1".to_vec()),
                after: b"v2".to_vec(),
            }],
        ))
        .await
        .unwrap();
        fs::write(f_old.as_std_path(), b"v2").await.unwrap();
        // 新 entry（后入账本，制造外部冲突）
        j.record(entry(
            "op-new",
            vec![FileChange::Written {
                path: f_new.clone(),
                before: Some(b"v1".to_vec()),
                after: b"v2".to_vec(),
            }],
        ))
        .await
        .unwrap();
        fs::write(f_new.as_std_path(), b"externally-changed")
            .await
            .unwrap();

        // 批量撤销 2 条：新 entry 冲突保留，旧 entry 撤销成功
        let report = j.undo(2).await.unwrap();
        assert_eq!(report.undone_entries, 1);
        assert_eq!(report.failed_files.len(), 1);
        assert_eq!(report.failed_files[0].0, f_new);
        assert_eq!(
            fs::read(f_old.as_std_path()).await.unwrap(),
            b"v1",
            "旧 entry 应已撤销"
        );

        // 重试 /undo(1)：必须命中回推到尾部的**新** entry（修复前会误取空/旧序）
        fs::write(f_new.as_std_path(), b"v2").await.unwrap();
        let retry = j.undo(1).await.unwrap();
        assert_eq!(retry.undone_entries, 1);
        assert_eq!(retry.failed_files.len(), 0, "expected empty");
        assert_eq!(
            fs::read(f_new.as_std_path()).await.unwrap(),
            b"v1",
            "重试应撤销新 entry"
        );
        assert!(j.is_empty());
    }

    #[tokio::test]
    async fn reset_to_initial_clears() {
        let j = FileChangeJournal::new(None);
        j.record(entry("op1", vec![])).await.unwrap();
        j.reset_to_initial().await.unwrap();
        assert!(j.is_empty(), "expected empty: j");
    }

    #[test]
    fn validate_restore_path_rejects_sibling_prefix_and_absolute_escape() {
        // S18（升级）：①绝对路径不再绕过包容检查；②组件级比较消除
        // 字符串 starts_with 的兄弟目录边界误判
        let wd = Utf8PathBuf::from("/tmp/abc");

        // 绝对越界（此前完全绕过检查）
        let err = super::validate_restore_path(&Utf8PathBuf::from("/tmp/abc-evil/x"), Some(&wd));
        assert!(err.is_err(), "绝对兄弟目录应被判越界");

        // 相对形式等价命中（含 .. 前置拒绝之外的组件级判定）
        let err = super::validate_restore_path(&Utf8PathBuf::from("../abc-evil/x"), Some(&wd));
        assert!(err.is_err(), "兄弟目录应被判越界");

        // 真实子路径放行（相对与绝对两种形态）
        assert!(
            super::validate_restore_path(&Utf8PathBuf::from("sub/file.txt"), Some(&wd)).is_ok()
        );
        assert!(
            super::validate_restore_path(&Utf8PathBuf::from("/tmp/abc/sub/file.txt"), Some(&wd))
                .is_ok()
        );
    }

    #[tokio::test]
    async fn validate_path_rejects_dotdot() {
        let path = Utf8PathBuf::from("../escape.txt");
        let res = validate_restore_path(&path, None);
        assert!(matches!(res, Err(JournalError::PathEscaped(_))));
    }

    #[tokio::test]
    async fn undo_zero_steps_treated_as_one() {
        let tmp = TempDir::new().unwrap();
        let file = Utf8PathBuf::from_path_buf(tmp.path().join("z.txt")).unwrap();
        fs::write(file.as_std_path(), b"old").await.unwrap();
        let j = FileChangeJournal::new(None);
        j.record(entry(
            "op1",
            vec![FileChange::Written {
                path: file.clone(),
                before: Some(b"old".to_vec()),
                after: b"new".to_vec(),
            }],
        ))
        .await
        .unwrap();
        fs::write(file.as_std_path(), b"new").await.unwrap();
        // steps=0 视为 1
        let report = j.undo(0).await.unwrap();
        assert_eq!(report.undone_entries, 1);
    }

    // === Default 实现：创建空 journal ===

    #[tokio::test]
    async fn default_creates_empty_journal() {
        let j = FileChangeJournal::default();
        assert!(j.is_empty(), "expected empty: j");
        assert_eq!(j.len(), 0);
    }

    // === diff 返回完整字段（op_id / prompt_snippet / files）===

    #[tokio::test]
    async fn diff_returns_full_fields() {
        let j = FileChangeJournal::new(None);
        let file = Utf8PathBuf::from("/tmp/test-diff.txt");
        j.record(entry(
            "op-xyz",
            vec![FileChange::Created {
                path: file.clone(),
                content: b"hello".to_vec(),
            }],
        ))
        .await
        .unwrap();
        let d = j.diff().await.unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].op_id, "op-xyz");
        assert_eq!(d[0].prompt_snippet, "prompt-op-xyz");
        assert_eq!(d[0].files.len(), 1);
        assert_eq!(d[0].files[0].path(), &file);
    }

    // === steps > entries 时撤销全部可用 entry ===

    #[tokio::test]
    async fn undo_more_steps_than_entries_undoes_all() {
        let j = FileChangeJournal::new(None);
        j.record(entry("op1", vec![])).await.unwrap();
        j.record(entry("op2", vec![])).await.unwrap();
        // steps=10 > 2 entries
        let report = j.undo(10).await.unwrap();
        assert_eq!(report.undone_entries, 2);
        assert!(j.is_empty(), "expected empty: j");
    }

    // === Edited 撤销恢复 before 内容 ===

    #[tokio::test]
    async fn undo_edited_restores_before() {
        let tmp = TempDir::new().unwrap();
        let file = Utf8PathBuf::from_path_buf(tmp.path().join("e.txt")).unwrap();
        fs::write(file.as_std_path(), b"v1").await.unwrap();
        let j = FileChangeJournal::new(None);
        j.record(entry(
            "op1",
            vec![FileChange::Edited {
                path: file.clone(),
                before: b"v1".to_vec(),
                after: b"v2".to_vec(),
            }],
        ))
        .await
        .unwrap();
        // 修改为 v2
        fs::write(file.as_std_path(), b"v2").await.unwrap();
        let report = j.undo(1).await.unwrap();
        assert_eq!(report.undone_entries, 1);
        assert_eq!(report.restored_files, vec![file.clone()]);
        let restored = fs::read(file.as_std_path()).await.unwrap();
        assert_eq!(restored, b"v1");
    }

    // === Written before=None（新建）撤销删除文件 ===

    #[tokio::test]
    async fn undo_written_with_none_before_deletes_file() {
        let tmp = TempDir::new().unwrap();
        let file = Utf8PathBuf::from_path_buf(tmp.path().join("w.txt")).unwrap();
        let j = FileChangeJournal::new(None);
        j.record(entry(
            "op1",
            vec![FileChange::Written {
                path: file.clone(),
                before: None,
                after: b"new".to_vec(),
            }],
        ))
        .await
        .unwrap();
        // 创建文件
        fs::write(file.as_std_path(), b"new").await.unwrap();
        assert!(file.as_std_path().exists());
        let report = j.undo(1).await.unwrap();
        assert_eq!(report.undone_entries, 1);
        assert_eq!(report.restored_files, vec![file.clone()]);
        assert!(
            !file.as_std_path().exists(),
            "before=None 的 Written 撤销应删除文件"
        );
    }

    // === Created 撤销时文件已不存在（幂等）===

    #[tokio::test]
    async fn undo_created_already_deleted_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let file = Utf8PathBuf::from_path_buf(tmp.path().join("c2.txt")).unwrap();
        let j = FileChangeJournal::new(None);
        j.record(entry(
            "op1",
            vec![FileChange::Created {
                path: file.clone(),
                content: b"created".to_vec(),
            }],
        ))
        .await
        .unwrap();
        // 文件不存在（模拟已删除）
        let report = j.undo(1).await.unwrap();
        assert_eq!(report.undone_entries, 1);
        // 文件已不存在，视为已恢复（幂等）
        assert!(report.restored_files.contains(&file));
        assert!(!file.as_std_path().exists());
    }

    // === Created 撤销时文件被外部修改（冲突，不删除）===

    #[tokio::test]
    async fn undo_created_modified_file_is_conflict() {
        let tmp = TempDir::new().unwrap();
        let file = Utf8PathBuf::from_path_buf(tmp.path().join("c3.txt")).unwrap();
        let j = FileChangeJournal::new(None);
        j.record(entry(
            "op1",
            vec![FileChange::Created {
                path: file.clone(),
                content: b"original".to_vec(),
            }],
        ))
        .await
        .unwrap();
        // 文件被外部修改
        fs::write(file.as_std_path(), b"modified").await.unwrap();
        let report = j.undo(1).await.unwrap();
        assert_eq!(report.undone_entries, 0);
        assert!(
            report.restored_files.is_empty(),
            "expected empty: report.restored_files"
        );
        assert_eq!(report.failed_files.len(), 1);
        assert!(!j.is_empty(), "失败 entry 回推账本可重试");
        // 文件未被删除
        assert!(file.as_std_path().exists());
        let cur = fs::read(file.as_std_path()).await.unwrap();
        assert_eq!(cur, b"modified");
    }

    // === Deleted 撤销时文件已存在（冲突，不覆盖）===

    #[tokio::test]
    async fn undo_deleted_when_file_exists_is_conflict() {
        let tmp = TempDir::new().unwrap();
        let file = Utf8PathBuf::from_path_buf(tmp.path().join("d2.txt")).unwrap();
        let j = FileChangeJournal::new(None);
        j.record(entry(
            "op1",
            vec![FileChange::Deleted {
                path: file.clone(),
                content: b"was-deleted".to_vec(),
            }],
        ))
        .await
        .unwrap();
        // 文件已存在（模拟删除后被重建）
        fs::write(file.as_std_path(), b"recreated").await.unwrap();
        let report = j.undo(1).await.unwrap();
        assert_eq!(report.undone_entries, 0);
        assert!(
            report.restored_files.is_empty(),
            "expected empty: report.restored_files"
        );
        assert_eq!(report.failed_files.len(), 1);
        assert!(!j.is_empty(), "失败 entry 回推账本可重试");
        // 文件未被覆盖
        let cur = fs::read(file.as_std_path()).await.unwrap();
        assert_eq!(cur, b"recreated");
    }

    // === R8 SEC-1：中段目录符号链接逃逸（C-03/C-28）===

    #[tokio::test]
    async fn undo_rejects_mid_path_symlink() {
        let tmp = TempDir::new().unwrap();
        let outside = Utf8PathBuf::from_path_buf(tmp.path().join("outside")).unwrap();
        fs::create_dir_all(outside.as_std_path()).await.unwrap();
        // 在 journal 工作目录内建指向外部目录的符号链接
        let workdir = Utf8PathBuf::from_path_buf(tmp.path().join("workdir")).unwrap();
        fs::create_dir_all(workdir.as_std_path()).await.unwrap();
        let link = workdir.join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.as_std_path(), link.as_std_path()).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(outside.as_std_path(), link.as_std_path()).unwrap();

        let victim = link.join("secret.txt");
        fs::write(victim.as_std_path(), b"new").await.unwrap();
        let j = FileChangeJournal::new(Some(workdir.clone()));
        j.record(entry(
            "op1",
            vec![FileChange::Written {
                path: victim.clone(),
                before: Some(b"old".to_vec()),
                after: b"new".to_vec(),
            }],
        ))
        .await
        .unwrap();
        let report = j.undo(1).await.unwrap();
        // 中段 symlink 必须拒绝恢复，且 entry 保留可重试
        assert_eq!(report.undone_entries, 0);
        assert_eq!(report.failed_files.len(), 1);
        assert!(!j.is_empty(), "失败 entry 回推账本可重试");
        // 外部文件未被穿透修改（仍为 new）
        let cur = fs::read(victim.as_std_path()).await.unwrap();
        assert_eq!(cur, b"new");
    }

    #[tokio::test]
    async fn ensure_not_symlink_rejects_any_component() {
        let tmp = TempDir::new().unwrap();
        let outside = Utf8PathBuf::from_path_buf(tmp.path().join("out")).unwrap();
        fs::create_dir_all(outside.as_std_path()).await.unwrap();
        let link = Utf8PathBuf::from_path_buf(tmp.path().join("link")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.as_std_path(), link.as_std_path()).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(outside.as_std_path(), link.as_std_path()).unwrap();

        // 末段是 symlink：拒绝
        let res = ensure_not_symlink(&link, None).await;
        assert!(matches!(res, Err(JournalError::PathEscaped(_))));
        // 中段是 symlink（link/x.txt 不存在）：拒绝
        let res = ensure_not_symlink(&link.join("x.txt"), None).await;
        assert!(matches!(res, Err(JournalError::PathEscaped(_))));
        // 普通路径（文件不存在）：通过（幂等路径）
        let ok = ensure_not_symlink(
            &Utf8PathBuf::from_path_buf(tmp.path().join("plain.txt")).unwrap(),
            None,
        )
        .await;
        assert!(ok.is_ok());
    }

    // === Edited 撤销时 after 不匹配（冲突）===

    #[tokio::test]
    async fn undo_edited_conflict_records_failed() {
        let tmp = TempDir::new().unwrap();
        let file = Utf8PathBuf::from_path_buf(tmp.path().join("e2.txt")).unwrap();
        fs::write(file.as_std_path(), b"v1").await.unwrap();
        let j = FileChangeJournal::new(None);
        j.record(entry(
            "op1",
            vec![FileChange::Edited {
                path: file.clone(),
                before: b"v1".to_vec(),
                after: b"v2".to_vec(),
            }],
        ))
        .await
        .unwrap();
        // 外部修改为 v3（不是 after=v2）
        fs::write(file.as_std_path(), b"v3").await.unwrap();
        let report = j.undo(1).await.unwrap();
        assert_eq!(report.undone_entries, 0);
        assert!(
            report.restored_files.is_empty(),
            "expected empty: report.restored_files"
        );
        assert_eq!(report.failed_files.len(), 1);
        assert!(!j.is_empty(), "失败 entry 回推账本可重试");
        // 文件未被覆盖
        let cur = fs::read(file.as_std_path()).await.unwrap();
        assert_eq!(cur, b"v3");
    }

    // === 单 entry 多文件撤销：全部恢复 ===

    #[tokio::test]
    async fn undo_entry_with_multiple_files() {
        let tmp = TempDir::new().unwrap();
        let f1 = Utf8PathBuf::from_path_buf(tmp.path().join("a.txt")).unwrap();
        let f2 = Utf8PathBuf::from_path_buf(tmp.path().join("b.txt")).unwrap();
        fs::write(f1.as_std_path(), b"v1").await.unwrap();
        fs::write(f2.as_std_path(), b"v1").await.unwrap();
        let j = FileChangeJournal::new(None);
        j.record(entry(
            "op1",
            vec![
                FileChange::Written {
                    path: f1.clone(),
                    before: Some(b"v1".to_vec()),
                    after: b"v2".to_vec(),
                },
                FileChange::Written {
                    path: f2.clone(),
                    before: Some(b"v1".to_vec()),
                    after: b"v2".to_vec(),
                },
            ],
        ))
        .await
        .unwrap();
        fs::write(f1.as_std_path(), b"v2").await.unwrap();
        fs::write(f2.as_std_path(), b"v2").await.unwrap();
        let report = j.undo(1).await.unwrap();
        assert_eq!(report.undone_entries, 1);
        assert_eq!(report.restored_files.len(), 2);
        // 两文件均恢复为 v1
        assert_eq!(fs::read(f1.as_std_path()).await.unwrap(), b"v1");
        assert_eq!(fs::read(f2.as_std_path()).await.unwrap(), b"v1");
    }

    // === 单 entry 部分冲突部分成功 ===

    #[tokio::test]
    async fn undo_partial_conflict_partial_success() {
        let tmp = TempDir::new().unwrap();
        let f1 = Utf8PathBuf::from_path_buf(tmp.path().join("ok.txt")).unwrap();
        let f2 = Utf8PathBuf::from_path_buf(tmp.path().join("conflict.txt")).unwrap();
        fs::write(f1.as_std_path(), b"v1").await.unwrap();
        fs::write(f2.as_std_path(), b"v1").await.unwrap();
        let j = FileChangeJournal::new(None);
        j.record(entry(
            "op1",
            vec![
                FileChange::Written {
                    path: f1.clone(),
                    before: Some(b"v1".to_vec()),
                    after: b"v2".to_vec(),
                },
                FileChange::Written {
                    path: f2.clone(),
                    before: Some(b"v1".to_vec()),
                    after: b"v2".to_vec(),
                },
            ],
        ))
        .await
        .unwrap();
        // f1 改为 v2（正常），f2 改为 v3（冲突）
        fs::write(f1.as_std_path(), b"v2").await.unwrap();
        fs::write(f2.as_std_path(), b"v3").await.unwrap();
        let report = j.undo(1).await.unwrap();
        assert_eq!(report.undone_entries, 0);
        // 部分失败的 entry 整体回推账本（含已恢复的文件），可整体重试
        // f1 恢复成功，f2 冲突
        assert_eq!(report.restored_files.len(), 1);
        assert!(report.restored_files.contains(&f1));
        assert_eq!(report.failed_files.len(), 1);
        assert_eq!(report.failed_files[0].0, f2);
        // f1 恢复为 v1
        assert_eq!(fs::read(f1.as_std_path()).await.unwrap(), b"v1");
        // f2 未被覆盖，仍为 v3
        assert_eq!(fs::read(f2.as_std_path()).await.unwrap(), b"v3");
    }

    // === reset_to_initial 后可继续 record ===

    #[tokio::test]
    async fn reset_to_initial_allows_new_record() {
        let j = FileChangeJournal::new(None);
        j.record(entry("op1", vec![])).await.unwrap();
        j.reset_to_initial().await.unwrap();
        assert!(j.is_empty(), "expected empty: j");
        // reset 后可继续 record
        j.record(entry("op2", vec![])).await.unwrap();
        assert_eq!(j.len(), 1);
        let d = j.diff().await.unwrap();
        assert_eq!(d[0].op_id, "op2");
    }

    // === 路径校验：绝对路径无 `..` 通过 ===

    #[tokio::test]
    async fn validate_path_absolute_without_dotdot_is_ok() {
        let path = Utf8PathBuf::from("/tmp/abc/file.txt");
        let res = validate_restore_path(&path, None);
        assert!(res.is_ok());
    }

    // === 路径校验：workdir 内相对路径通过 ===

    #[tokio::test]
    async fn validate_path_workdir_relative_inside_is_ok() {
        let wd = Utf8PathBuf::from("/tmp/abc");
        let path = Utf8PathBuf::from("subdir/file.txt");
        let res = validate_restore_path(&path, Some(&wd));
        assert!(res.is_ok());
    }

    // === 路径校验：无 workdir 时相对路径无 `..` 通过 ===

    #[tokio::test]
    async fn validate_path_workdir_none_relative_without_dotdot_is_ok() {
        let path = Utf8PathBuf::from("subdir/file.txt");
        let res = validate_restore_path(&path, None);
        assert!(res.is_ok());
    }

    // === 路径校验：嵌套 `..` 段被拒绝 ===

    #[tokio::test]
    async fn validate_path_nested_dotdot_rejected() {
        let path = Utf8PathBuf::from("a/../b.txt");
        let res = validate_restore_path(&path, None);
        assert!(matches!(res, Err(JournalError::PathEscaped(_))));
    }

    // === D3 内存上限：写入 MAX+5 条，超限丢最旧 ===

    #[tokio::test]
    async fn record_evicts_oldest_beyond_max() {
        let j = FileChangeJournal::new(None);
        // 每条挂一个 Created change（文件不存在，undo 走幂等路径，无需建文件）
        let mut paths = Vec::new();
        for i in 0..MAX_JOURNAL_ENTRIES + 5 {
            let file = Utf8PathBuf::from(format!("/tmp/evict-m{i}.txt"));
            j.record(entry(
                &format!("op-{i}"),
                vec![FileChange::Created {
                    path: file.clone(),
                    content: b"x".to_vec(),
                }],
            ))
            .await
            .unwrap();
            paths.push(file);
        }
        // len 封顶为 MAX；最旧 5 条（op-0..op-4）已被丢弃
        assert_eq!(j.len(), MAX_JOURNAL_ENTRIES);
        let d = j.diff().await.unwrap();
        assert_eq!(d.len(), MAX_JOURNAL_ENTRIES);
        assert_eq!(d[0].op_id, "op-5", "最旧的 op-0..op-4 应已被淘汰");
        assert_eq!(
            d[d.len() - 1].op_id,
            format!("op-{}", MAX_JOURNAL_ENTRIES + 4),
            "最新条目应保留在尾部"
        );
        // undo 一次弹的是最新条目（LIFO），被淘汰的最旧条目不可 undo
        let report = j.undo(1).await.unwrap();
        assert_eq!(report.undone_entries, 1);
        assert_eq!(
            report.restored_files,
            vec![paths[paths.len() - 1].clone()],
            "应撤销最新条目而非最旧条目"
        );
        assert_eq!(j.len(), MAX_JOURNAL_ENTRIES - 1);
    }

    // === D3 边界：恰好 MAX 条不丢弃 ===

    #[tokio::test]
    async fn record_at_exact_max_keeps_all_entries() {
        let j = FileChangeJournal::new(None);
        for i in 0..MAX_JOURNAL_ENTRIES {
            j.record(entry(&format!("op-{i}"), vec![])).await.unwrap();
        }
        assert_eq!(j.len(), MAX_JOURNAL_ENTRIES);
        let d = j.diff().await.unwrap();
        assert_eq!(d.len(), MAX_JOURNAL_ENTRIES);
        assert_eq!(d[0].op_id, "op-0", "恰好达到上限不应丢弃任何条目");
        assert_eq!(
            d[d.len() - 1].op_id,
            format!("op-{}", MAX_JOURNAL_ENTRIES - 1)
        );
    }
}
