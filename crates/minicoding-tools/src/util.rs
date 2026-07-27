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
