//! 共享路径与输出工具。

use camino::Utf8PathBuf;
use minicoding_core::model::ToolError;

/// 解析输入路径并确保不越界（C-03 路径不可越界）。
///
/// 相对路径基于 `workdir` 解析，绝对路径原样使用。通过 `canonicalize` 规范化
/// 后校验结果必须位于 `workdir` 之内；对不存在的目标，规范化其父目录后拼接
/// 文件名再校验，以正确解析符号链接。
///
/// # Errors
/// 路径越界返回 `ToolError::PathEscaped`；父目录不存在返回 `ToolError::NotFound`；
/// 其他 IO 失败返回 `ToolError::Io`。
pub fn resolve_path(workdir: &Utf8PathBuf, input: &str) -> Result<Utf8PathBuf, ToolError> {
    let candidate = if std::path::Path::new(input).is_absolute() {
        Utf8PathBuf::from(input)
    } else {
        workdir.join(input)
    };

    let canon_workdir = workdir.canonicalize_utf8().map_err(ToolError::Io)?;

    let resolved = if let Ok(c) = candidate.canonicalize_utf8() {
        c
    } else {
        // 目标不存在：规范化父目录后拼接文件名，确保符号链接被解析
        let parent = candidate
            .parent()
            .ok_or_else(|| ToolError::InvalidInput(format!("invalid path: {input}")))?;
        let file_name = candidate
            .file_name()
            .ok_or_else(|| ToolError::InvalidInput(format!("invalid path: {input}")))?;
        let canon_parent = parent
            .canonicalize_utf8()
            .map_err(|_| ToolError::NotFound(input.to_string()))?;
        canon_parent.join(file_name)
    };

    if !resolved.starts_with(&canon_workdir) {
        return Err(ToolError::PathEscaped(input.to_string()));
    }
    Ok(resolved)
}

/// 确保 `path` 指向一个已存在的目录。
///
/// # Errors
/// 不存在返回 `ToolError::NotFound`；非目录返回 `ToolError::InvalidInput`；
/// 其他 IO 失败返回 `ToolError::Io`。
pub async fn ensure_dir(path: &Utf8PathBuf) -> Result<(), ToolError> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ToolError::NotFound(path.to_string()),
            _ => ToolError::Io(e),
        })?;
    if !metadata.is_dir() {
        return Err(ToolError::InvalidInput(format!("not a directory: {path}")));
    }
    Ok(())
}

/// 截断输出至 `max_bytes`，在 UTF-8 字符边界上截断并附加截断标记。
///
/// 返回 `(截断后的文本, 是否发生了截断)`。
#[must_use]
pub fn truncate_output(text: String, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let indicator = "\n...[output truncated]";
    let budget = max_bytes.saturating_sub(indicator.len());
    let mut end = budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut result = String::with_capacity(end + indicator.len());
    result.push_str(&text[..end]);
    result.push_str(indicator);
    (result, true)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use minicoding_core::model::ToolError;
    use tempfile::TempDir;

    /// 创建临时 workdir 并返回 `(TempDir, 规范化后的 workdir 路径)`。
    /// 保留 `TempDir` 句柄以防止临时目录在测试结束前被清理。
    fn make_workdir() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().expect("create tempdir");
        let canon = Utf8PathBuf::from_path_buf(tmp.path().canonicalize().expect("canonicalize"))
            .expect("utf-8 path");
        (tmp, canon)
    }

    // === resolve_path 测试（C-03 路径不可越界） ===

    #[test]
    fn resolve_relative_path_joins_workdir() {
        let (_tmp, workdir) = make_workdir();
        // 相对路径解析为 workdir/input（input 不存在但父目录 workdir 存在）
        let resolved = resolve_path(&workdir, "somefile.txt").expect("resolve ok");
        assert_eq!(resolved, workdir.join("somefile.txt"));
    }

    #[test]
    fn resolve_absolute_path_inside_workdir_passes() {
        let (tmp, workdir) = make_workdir();
        // 在 workdir 内创建一个文件，传入其绝对路径
        let file_path = tmp.path().join("inside.txt");
        std::fs::write(&file_path, "hello").expect("write file");
        let abs = Utf8PathBuf::from_path_buf(file_path).expect("utf-8 path");
        let resolved = resolve_path(&workdir, abs.as_str()).expect("resolve ok");
        assert_eq!(resolved, workdir.join("inside.txt"));
    }

    #[test]
    fn resolve_absolute_path_outside_workdir_rejected() {
        let (_tmp, workdir) = make_workdir();
        // /etc/passwd 是越界的绝对路径
        let err = resolve_path(&workdir, "/etc/passwd").unwrap_err();
        assert!(matches!(err, ToolError::PathEscaped(_)));
    }

    #[test]
    fn resolve_dotdot_escape_rejected() {
        let (_tmp, workdir) = make_workdir();
        // ../escape 越界
        let err = resolve_path(&workdir, "../escape").unwrap_err();
        assert!(matches!(err, ToolError::PathEscaped(_)));
    }

    #[test]
    fn resolve_nonexistent_file_with_existing_parent_ok() {
        let (_tmp, workdir) = make_workdir();
        // 父目录存在（workdir 本身），文件不存在 → 规范化父目录后拼接文件名
        let resolved = resolve_path(&workdir, "newfile.txt").expect("resolve ok");
        assert_eq!(resolved, workdir.join("newfile.txt"));
    }

    #[test]
    fn resolve_nonexistent_parent_returns_not_found() {
        let (_tmp, workdir) = make_workdir();
        // 父目录不存在 → 无法规范化父目录 → NotFound
        let err = resolve_path(&workdir, "no_such_dir/file.txt").unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_symlink_escaping_workdir_rejected() {
        use std::os::unix::fs::symlink;
        let outside = TempDir::new().expect("create outside tempdir");
        let (tmp, workdir) = make_workdir();
        // 在 workdir 内创建符号链接指向 workdir 外的目录
        let link_path = tmp.path().join("evil_link");
        symlink(outside.path(), &link_path).expect("create symlink");
        let err = resolve_path(&workdir, "evil_link").unwrap_err();
        assert!(matches!(err, ToolError::PathEscaped(_)));
    }

    // === ensure_dir 测试 ===

    #[tokio::test]
    async fn ensure_dir_existing_directory_ok() {
        let tmp = TempDir::new().expect("create tempdir");
        let path = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("utf-8 path");
        ensure_dir(&path).await.expect("dir exists");
    }

    #[tokio::test]
    async fn ensure_dir_nonexistent_returns_not_found() {
        let path = Utf8PathBuf::from("/nonexistent/path/that/does/not/exist");
        let err = ensure_dir(&path).await.unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn ensure_dir_file_returns_invalid_input() {
        let tmp = TempDir::new().expect("create tempdir");
        let file_path = tmp.path().join("not_a_dir.txt");
        tokio::fs::write(&file_path, "data")
            .await
            .expect("write file");
        let path = Utf8PathBuf::from_path_buf(file_path).expect("utf-8 path");
        let err = ensure_dir(&path).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    // === truncate_output 测试 ===

    #[test]
    fn truncate_output_short_text_not_truncated() {
        let (result, truncated) = truncate_output("hello".to_string(), 100);
        assert_eq!(result, "hello");
        assert!(!truncated);
    }

    #[test]
    fn truncate_output_exact_length_not_truncated() {
        let text = "hello".to_string();
        let (result, truncated) = truncate_output(text.clone(), text.len());
        assert_eq!(result, text);
        assert!(!truncated);
    }

    #[test]
    fn truncate_output_long_text_truncated_with_indicator() {
        let text = "a".repeat(100);
        let (result, truncated) = truncate_output(text, 50);
        assert!(truncated);
        assert!(result.ends_with("\n...[output truncated]"));
        // 截断后的总长度不超过 max_bytes
        assert!(result.len() <= 50);
    }

    #[test]
    fn truncate_output_multibyte_at_char_boundary() {
        // 中文字符每个 3 字节，构造在截断边界需要回退到字符边界的场景
        let text = "中".repeat(20); // 60 字节
        let (result, truncated) = truncate_output(text, 50);
        assert!(truncated);
        let indicator = "\n...[output truncated]";
        assert!(result.ends_with(indicator));
        // 截断点必须在 UTF-8 字符边界（不会产生半个字符）
        let prefix_len = result.len() - indicator.len();
        assert_eq!(prefix_len % 3, 0, "prefix should be whole 中 chars");
    }

    #[test]
    fn truncate_output_empty_text_not_truncated() {
        let (result, truncated) = truncate_output(String::new(), 10);
        assert_eq!(result, "");
        assert!(!truncated);
    }
}
