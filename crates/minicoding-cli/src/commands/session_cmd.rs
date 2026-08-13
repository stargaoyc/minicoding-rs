//! `minicoding session list`/`delete` 子命令（T-M3-10c）。
//!
//! 直接复用 `JsonlStorage` 的同步方法（`list_sessions_sync`/`delete_session_sync`），
//! 不构建 `Runtime`，无需 API key——子命令只做存储 IO 与终端渲染。
//!
//! ## 输出格式
//!
//! `list` 输出表格（无 TTY 时降级为 tab 分隔，便于脚本管道）：
//! ```text
//! SESSION ID                          CREATED              MESSAGES  LAST ACTIVITY
//! 01HXXXXXXXXXXXXXXXXXXXXXXXXXXX      2026-08-01 12:00:00        42  2026-08-01 12:05:33
//! ```
//!
//! `delete <id>` 删除会话文件 + 索引项，原文件不可恢复（不进回收站）。

use std::io::IsTerminal;

use anyhow::{Context, Result};
use minicoding_storage::JsonlStorage;
use time::format_description::well_known::Rfc3339;

/// `session` 子命令动作。
#[derive(clap::Subcommand, Debug)]
pub enum SessionAction {
    /// 列出所有会话（按最近活动倒序）。
    List,
    /// 删除指定会话（原文件不可恢复）。
    Delete {
        /// 待删除会话 ID。
        id: String,
    },
}

/// `session` 顶层子命令。
#[derive(clap::Args, Debug)]
pub struct SessionCommand {
    #[command(subcommand)]
    pub action: SessionAction,
}

/// 执行 `session` 子命令。
///
/// 不构建 `Runtime`，直接构造 `JsonlStorage` 调用同步方法。
///
/// # Errors
/// 存储目录不可解析、列表/删除失败时返回错误。
pub fn run_session_command(cmd: &SessionCommand) -> Result<()> {
    let sessions_dir = minicoding_core::paths::sessions_dir().context("无法确定会话存储目录")?;
    let storage = JsonlStorage::new(sessions_dir);
    match &cmd.action {
        SessionAction::List => list_sessions(&storage),
        SessionAction::Delete { id } => delete_session(&storage, id),
    }
}

/// 列出会话并按最近活动倒序渲染。
fn list_sessions(storage: &JsonlStorage) -> Result<()> {
    let mut metas = storage.list_sessions_sync().context("列出会话失败")?;
    // 按最近活动倒序（最新在最前），便于用户快速找到近期会话。
    metas.sort_by_key(|m| std::cmp::Reverse(m.last_message_at));

    if metas.is_empty() {
        println!("（暂无会话）");
        return Ok(());
    }

    if std::io::stdout().is_terminal() {
        // TTY：表格头部 + 对齐列
        println!(
            "{:<38}  {:<19}  {:>8}  {:<19}",
            "SESSION ID", "CREATED", "MESSAGES", "LAST ACTIVITY"
        );
        println!("{}", "-".repeat(86));
        for m in &metas {
            println!(
                "{:<38}  {:<19}  {:>8}  {:<19}",
                m.id,
                fmt_short(&m.created_at),
                m.message_count,
                fmt_short(&m.last_message_at),
            );
        }
        println!();
        println!("共 {} 个会话", metas.len());
    } else {
        // 非 TTY：tab 分隔，便于 awk/jq 处理
        for m in &metas {
            println!(
                "{}\t{}\t{}\t{}",
                m.id,
                m.created_at.format(&Rfc3339).unwrap_or_default(),
                m.message_count,
                m.last_message_at.format(&Rfc3339).unwrap_or_default(),
            );
        }
    }
    Ok(())
}

/// 删除会话文件 + 索引项。
fn delete_session(storage: &JsonlStorage, id: &str) -> Result<()> {
    storage
        .delete_session_sync(&id.to_string())
        .with_context(|| format!("删除会话 {id} 失败"))?;
    println!("已删除会话 {id}");
    Ok(())
}

/// 格式化为 `YYYY-MM-DD HH:MM:SS`（UTC，带 `Z` 后缀）。
///
/// 不依赖 `time` 的 `local-offset` feature（该 feature 在多线程下需 unsafe），
/// 统一显示 UTC，用户可通过 `TZ` 环境变量在 shell 层做时区转换。
fn fmt_short(t: &time::OffsetDateTime) -> String {
    use time::macros::format_description;
    let fmt = format_description!("[year]-[month]-[day] [hour]:[minute]:[second]Z");
    t.format(fmt).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::model::{Message, SessionId};
    use minicoding_core::storage::Storage;
    use minicoding_storage::JsonlStorage;
    use tempfile::TempDir;
    use time::OffsetDateTime;
    use time::macros::format_description;

    fn setup_storage() -> (TempDir, JsonlStorage) {
        let dir = TempDir::new().expect("tempdir");
        let storage = JsonlStorage::new(
            camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8"),
        );
        (dir, storage)
    }

    #[tokio::test]
    async fn list_returns_appended_sessions() {
        let (_dir, storage) = setup_storage();
        let id_a: SessionId = "01AAA00000000000000000000".to_string();
        let id_b: SessionId = "01BBB00000000000000000000".to_string();
        storage
            .append(&id_a, &Message::user_text("a"))
            .await
            .unwrap();
        storage
            .append(&id_b, &Message::user_text("b"))
            .await
            .unwrap();
        let mut metas = storage.list_sessions_sync().unwrap();
        // 按 last_message_at 倒序，与 list_sessions 一致
        metas.sort_by_key(|m| std::cmp::Reverse(m.last_message_at));
        assert_eq!(metas.len(), 2);
        assert!(metas.iter().any(|m| m.id == id_a));
        assert!(metas.iter().any(|m| m.id == id_b));
    }

    #[test]
    fn delete_removes_session_file_and_index_entry() {
        let (_dir, storage) = setup_storage();
        let id: SessionId = "01DEL00000000000000000000".to_string();
        // 同步写入：用 fork_session_sync 复制一条消息作为初始文件
        let msg = Message::user_text("hello");
        storage
            .fork_session_sync(&id, std::slice::from_ref(&msg))
            .unwrap();
        // 文件存在
        let metas = storage.list_sessions_sync().unwrap();
        assert!(metas.iter().any(|m| m.id == id));
        // 删除
        storage.delete_session_sync(&id).unwrap();
        // 文件已删除
        let metas = storage.list_sessions_sync().unwrap();
        assert!(!metas.iter().any(|m| m.id == id));
    }

    /// 验证 `fmt_short` 不 panic 且返回非空字符串。
    #[test]
    fn fmt_short_returns_non_empty_string() {
        let t = OffsetDateTime::now_utc();
        let s = fmt_short(&t);
        assert!(!s.is_empty(), "expected non-empty: s");
    }

    /// 验证 `format_description!` 宏在当前 `time` 配置下可用。
    #[test]
    fn format_description_macro_is_usable() {
        let _fmt = format_description!("[year]-[month]-[day]");
    }
}
