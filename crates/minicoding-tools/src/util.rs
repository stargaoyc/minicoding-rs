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
    use minicoding_policy::PathSandboxError;

    // S15：核心判定委托 `minicoding-policy::path_sandbox`（单一实现，消除双版本
    // 漂移——审查 §1.10）。此处仅保留调用方契约：错误映射 + "父目录不存在 →
    // NotFound"（write.rs 的 mkdir -p 重试依赖该信号）。
    match minicoding_policy::resolve_under(workdir, input) {
        Ok(resolved) => {
            let target_exists = resolved.exists();
            let parent_is_dir = resolved.parent().is_some_and(camino::Utf8Path::is_dir);
            if !target_exists && !parent_is_dir {
                return Err(ToolError::NotFound(input.to_string()));
            }
            Ok(resolved)
        }
        Err(PathSandboxError::Escaped { .. }) => Err(ToolError::PathEscaped(input.to_string())),
        Err(PathSandboxError::NotFound { .. }) => Err(ToolError::NotFound(input.to_string())),
    }
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

/// 原子写文件（TL-R6-5，2026-08-28 R6 审查）。
///
/// 先写同目录临时文件 + `fsync`，再 `rename` 覆盖目标（同文件系统原子）——
/// `tokio::fs::write` 直接覆盖在崩溃/断电时使文件处于截断状态，破坏数据
/// 完整性且与 journal undo 语义冲突（undo 需 after 内容完整可见）。
///
/// 已存在目标文件时保留其权限（如可执行位）；临时文件残留由上层清理。
///
/// # Errors
/// 临时文件写入、fsync、rename 或权限拷贝失败时返回 IO 错误。
pub async fn atomic_write(path: &camino::Utf8PathBuf, content: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    // 同目录临时文件（rename 需同文件系统才原子）。
    // R8 TL-6 修复：固定 `{path}.minicoding.tmp` 在同目标并发写时冲突
    // （create(true) 截断 + 双写交错 → rename 竞态）。追加 pid+原子计数
    // 后缀保证唯一，消除并发写冲突与 metadata/set_permissions TOCTOU 窗口。
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("minicoding.tmp.{}.{n}", std::process::id()));
    let mut opts = tokio::fs::OpenOptions::new();
    // R9 FS-1：`create(true).truncate(true)` 会跟随已存在文件——pid+计数
    // 后缀可被恶意仓库预置同名 symlink 使 `open` 写穿。改 `create_new(true)`
    // （不存在才创建，已存在/symlink 即 AlreadyExists 报错），一次修掉并发写
    // 与 symlink 两类问题。残余：create_new 后到 rename 间若被替换仍可写穿，
    // 但窗口极窄（临时文件在 $MINICODING_HOME/工作区临时目录，非持久暴露面）。
    opts.write(true).create_new(true);
    #[cfg(unix)]
    opts.mode(0o644); // tokio 原生支持 unix mode
    let mut file = opts.open(&tmp).await?;
    file.write_all(content).await?;
    file.sync_all().await?;
    drop(file);
    // 保留已存在目标的权限（覆盖脚本/可执行文件时）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = tokio::fs::metadata(path.as_std_path()).await {
            let mode = meta.permissions().mode() & 0o777;
            let _ = tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).await;
        }
    }
    tokio::fs::rename(&tmp, path.as_std_path()).await?;
    Ok(())
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

    #[cfg(unix)]
    #[test]
    fn resolve_absolute_path_outside_workdir_rejected() {
        let (_tmp, workdir) = make_workdir();
        // /etc/passwd 是 Unix 下越界的绝对路径
        let err = resolve_path(&workdir, "/etc/passwd").unwrap_err();
        assert!(matches!(err, ToolError::PathEscaped(_)));
    }

    #[cfg(windows)]
    #[test]
    fn resolve_absolute_path_outside_workdir_rejected() {
        let (_tmp, workdir) = make_workdir();
        // C:\Windows\System32 是 Windows 下越界的绝对路径
        let err = resolve_path(&workdir, "C:\\Windows\\System32").unwrap_err();
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

/// S16：断言 `candidate` 位于 `workdir` 之内（组件级比较，供 mkdir 前防护）。
///
/// 与 [`resolve_path`] 的差异：不要求路径存在，仅做规范化 + 前缀包容判定。
///
/// # Errors
/// 规范化失败或越界时返回 `ToolError::PathEscaped` 风格的 `Exec` 错误文本。
pub fn assert_within_workdir(
    workdir: &Utf8PathBuf,
    candidate: &Utf8PathBuf,
) -> Result<(), ToolError> {
    let wd = workdir
        .canonicalize_utf8()
        .unwrap_or_else(|_| workdir.clone());
    let cand = candidate.canonicalize_utf8().or_else(|_| {
        // 目标不存在：对最长存在祖先规范化后，把**剩余相对尾部原文**（含 `..`
        // 段）拼回。R9 PATH-1 修复：此前按层级收集 `file_name` 重建会**丢失
        // `..` 段**（`file_name` 对 `..` 返回 `None`），重建路径看似仍以 workdir
        // 开头而放行（mkdir 逃逸）。保留尾部原文后交由 `is_under` 做词法 `..`
        // 规范化比较（见下）。
        let mut ancestor = candidate.clone();
        let mut tail: Option<String> = None;
        loop {
            let parent = ancestor.parent().map(Utf8PathBuf::from);
            let Some(prev) = parent else {
                break;
            };
            tail = Some(
                candidate
                    .strip_prefix(&prev)
                    .map(std::string::ToString::to_string)
                    .unwrap_or_default(),
            );
            ancestor = prev;
            if ancestor.canonicalize_utf8().is_ok() {
                break;
            }
        }
        ancestor
            .canonicalize_utf8()
            .map(|canon| match tail {
                Some(t) if !t.is_empty() => canon.join(t),
                _ => canon,
            })
            .map_err(|e| ToolError::Exec(format!("path normalize failed: {e}")))
    })?;
    // R9 PATH-1 修复：最终包容判定用 policy 的 `is_under`（词法规范化 `..`
    // 后组件级比较）——`starts_with` 不解析 `..`，候选含 `..` 段时仍会
    // 误放行（mkdir 逃逸）。`is_under` 为纯组件操作，路径不必存在。
    if minicoding_policy::is_under(&cand, &wd) {
        Ok(())
    } else {
        // R9 TOOL-6：返回 `PathEscaped` 而非 `Exec`——Runtime 对 `PathEscaped`
        // 有专门的 denial 计数/审计路径，`Exec` 归类为普通执行错误（绕过
        // PathEscaped 的审计归类）。调用方 `write.rs` 的 NotFound 分支（mkdir
        // 前防护）以 `?` 上抛，错误类型不变更调用语义。
        Err(ToolError::PathEscaped(format!(
            "path escapes workdir: {candidate}"
        )))
    }
}

#[cfg(test)]
mod s16_tests {
    use super::*;

    #[test]
    fn within_workdir_ok() {
        let tmp = tempfile::tempdir().expect("tmp");
        let wd =
            Utf8PathBuf::from_path_buf(tmp.path().canonicalize().expect("canon")).expect("utf8");
        assert!(assert_within_workdir(&wd, &wd.join("a/b/c.txt")).is_ok());
        assert!(assert_within_workdir(&wd, &wd.join("x/y/z")).is_ok());
    }

    #[test]
    fn outside_workdir_rejected_even_nonexistent() {
        let tmp = tempfile::tempdir().expect("tmp");
        let wd =
            Utf8PathBuf::from_path_buf(tmp.path().canonicalize().expect("canon")).expect("utf8");
        // 不存在的越界路径（mkdir 前防护场景）
        assert!(assert_within_workdir(&wd, &Utf8PathBuf::from("/tmp/s16-escape/a/b")).is_err());
    }

    /// R9 PATH-1：`nodir/../../evil/f.txt` 形态的 mkdir 逃逸——
    /// 此前 suffix 每层 push 完整相对段，`.rev().fold(acc.join(seg))` 堆叠出
    /// 仍以 workdir 开头的垃圾路径被 `starts_with` 放行，`create_dir_all` 可
    /// 在工作区外创建目录。修复后规范化 `..` 正确拒绝。
    #[test]
    fn mkdir_escape_with_dotdot_rejected() {
        let tmp = tempfile::tempdir().expect("tmp");
        let wd =
            Utf8PathBuf::from_path_buf(tmp.path().canonicalize().expect("canon")).expect("utf8");
        // 外部落点：用 `/tmp/` 下的真实目录（非 wd 下属）
        let outside = std::path::Path::new("/tmp").join("fp_mkdir_escape");
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).expect("外部目录");
        let outside_utf8 =
            Utf8PathBuf::from_path_buf(outside.canonicalize().expect("canon")).expect("utf8");

        // 形态 1：`nodir/../../<external>/f.txt`——`..` 弹出后落点在工作区外
        // 取外部目录相对文件系统根的路径（Windows 根为盘符，不能硬编码 `/`）
        let rel_parent = std::path::Path::new(outside_utf8.as_str())
            .ancestors()
            .last()
            .map(|root| {
                outside_utf8
                    .strip_prefix(root.to_string_lossy().as_ref())
                    .expect("绝对路径")
            })
            .expect("绝对路径根");
        let escape1 = wd.join(format!("nodir/../../{rel_parent}/f.txt"));
        assert!(
            assert_within_workdir(&wd, &escape1).is_err(),
            ".. 逃逸应被拒绝: {escape1}"
        );

        // 形态 2：`../<external>/x.txt`
        let escape2 = wd.join(format!("../{rel_parent}/x.txt"));
        assert!(
            assert_within_workdir(&wd, &escape2).is_err(),
            ".. 逃逸应被拒绝: {escape2}"
        );

        // 正常相对子路径仍放行
        assert!(assert_within_workdir(&wd, &wd.join("sub/new/file.txt")).is_ok());

        let _ = std::fs::remove_dir_all(&outside);
    }
}
