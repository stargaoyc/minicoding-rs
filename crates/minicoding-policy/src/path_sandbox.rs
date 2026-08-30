//! 应用层路径沙箱（C-03：路径不可越界）。
//!
//! `resolve_under` 是所有文件工具执行前的第一道防线：将任意输入解析为
//! `workdir` 之下的规范绝对路径，越界（含符号链接逃逸、`..` 穿越）直接
//! 返回 `PathSandboxError::Escaped`。内核级 `landlock`/`libseccomp` 是
//! 第二道防线（C-22/C-30），二者互不替代。
//!
//! 设计要点：
//! - 相对输入相对 `workdir` 解析，绝对输入也必须落在 `workdir` 之内；
//! - 通过 `canonicalize` 解析符号链接与 `..`；目标尚不存在时回退到
//!   最长存在祖先进行规范化，再拼接不存在的尾部组件；
//! - 容纳判定用 `Utf8Path::starts_with` 做组件级前缀匹配，避免字符串
//!   前缀误判（如 `/foo/barbaz` 不被误判为在 `/foo/bar` 之下）。

use camino::{Utf8Path, Utf8PathBuf};

/// 路径沙箱错误。
#[derive(thiserror::Error, Debug)]
pub enum PathSandboxError {
    /// 输入路径解析后落在 `workdir` 之外（C-03）。
    #[error("path escapes workdir: {path} (workdir: {workdir})")]
    Escaped { path: String, workdir: String },
    /// 路径或其祖先不存在，无法规范化。
    #[error("path not found: {path}")]
    NotFound { path: String },
}

/// 将 `input` 解析为 `workdir` 之下的规范绝对路径。
///
/// - 相对路径相对 `workdir` 拼接；绝对路径原样使用，但仍须落在 `workdir` 内；
/// - 调用 `canonicalize` 消解符号链接与 `.`/`..`；目标不存在时回退到最长
///   存在祖先规范化后拼接尾部；
/// - 规范化结果不在 `workdir` 之下时返回 [`PathSandboxError::Escaped`]。
///
/// 这是文件副作用执行前的第一道防线（C-03），应在任何文件 IO 之前调用。
///
/// # TOCTOU 边界（SEC-13，2026-08-28 R5 收尾）
///
/// 本函数是 check-then-use 语义：规范化校验通过后，到实际文件 IO 之间存在
/// 竞态窗口（恶意进程可在此期间替换路径/symlink）。Linux/macOS 有 OS 沙箱
/// （landlock/Seatbelt）在**使用**时二次强制 C-03；**Windows 无 OS 兜底**——
/// Job Object 不做文件系统隔离，此窗口是 Windows 上唯一防线，风险如实披露
/// （见 `security.md` §12.4）。
///
/// # Errors
///
/// - [`PathSandboxError::NotFound`]：`workdir` 不存在或不可规范化，或输入路径
///   无法回溯到任何存在的祖先；
/// - [`PathSandboxError::Escaped`]：规范化后的路径落在 `workdir` 之外（C-03），
///   或输入以 `~` 开头（P2-4：策略层不展开 `~`，按相对路径解析会落在 workdir
///   内被误放行——若未来任何路径做了 shell 展开即成为逃逸口，显式拒绝）。
pub fn resolve_under(workdir: &Utf8PathBuf, input: &str) -> Result<Utf8PathBuf, PathSandboxError> {
    // R9 P2-4：显式拒绝 `~` 前缀路径——`~/.ssh/id_rsa` 若不拦，策略层按相对
    // 路径解析成 `<workdir>/~/...`（在边界内，std::fs 也不展开 `~` 所以当前
    // 无害），但任何未来 shell 展开路径都会把 `~` 变绝对逃逸。
    if input.starts_with('~') || input.starts_with("~/") {
        return Err(PathSandboxError::Escaped {
            path: input.to_string(),
            workdir: workdir.to_string(),
        });
    }
    // workdir 自身必须存在并可规范化，作为容纳判定的基准。
    let canon_workdir =
        canonicalize_utf8(workdir.as_std_path()).map_err(|_| PathSandboxError::NotFound {
            path: workdir.to_string(),
        })?;

    // 相对输入相对 workdir 拼接；绝对输入原样保留（容纳判定仍会校验边界）。
    let input_path = Utf8Path::new(input);
    let candidate = if input_path.is_absolute() {
        input_path.to_path_buf()
    } else {
        workdir.join(input_path)
    };

    let resolved = canonicalize_or_parent(&candidate)?;

    if !is_under(&resolved, &canon_workdir) {
        return Err(PathSandboxError::Escaped {
            path: input.to_string(),
            workdir: workdir.to_string(),
        });
    }

    Ok(resolved)
}

/// 判断 `child` 是否位于 `parent` 之下（含等于 `parent` 自身）。
///
/// 使用 `Utf8Path::starts_with` 做组件级前缀匹配，避免字符串前缀误判：
/// `/foo/bar` 在 `/foo` 之下为真，而 `/foo/barbaz` 在 `/foo/bar` 之下为假。
///
/// **R9 PATH-1 修复**：`starts_with` 是**词法组件匹配，不解析 `..`**——
/// `/tmp/wd/nodir/../../evil/f.txt` 对 `/tmp/wd` 的 `starts_with` 判真（`..` 段不
/// 弹出），连绝对路径逃逸都能通过。先对两侧做词法 `..` 规范化（纯组件栈，
/// 不触碰文件系统），再组件级前缀比较。
#[must_use]
pub fn is_under(child: &Utf8Path, parent: &Utf8Path) -> bool {
    let child_comp = normalize_components(child);
    let parent_comp = normalize_components(parent);
    child_comp.len() >= parent_comp.len() && child_comp[..parent_comp.len()] == parent_comp[..]
}

/// 词法规范化路径组件（消解 `.`/`..`，纯组件栈操作，不触碰文件系统）。
///
/// 与 `std::fs::canonicalize` 不同：不解析符号链接、不要求路径存在——仅消除
/// `.` 与 `..` 段（`..` 弹出上一段；栈空时保留 `..`，相对路径语义不变式，
/// 与 core `normalize_lexical_rel_path` 一致）。用于 `is_under` 的逃逸判定：
/// 含 `..` 的候选路径在规范化后正确落在（或不落在）父目录下。
fn normalize_components(path: &Utf8Path) -> Vec<camino::Utf8Component<'_>> {
    let mut stack: Vec<camino::Utf8Component<'_>> = Vec::new();
    for comp in path.components() {
        match comp {
            camino::Utf8Component::CurDir => {}
            camino::Utf8Component::ParentDir => {
                if stack
                    .last()
                    .is_some_and(|c| matches!(c, camino::Utf8Component::Normal(_)))
                {
                    stack.pop();
                } else {
                    stack.push(comp);
                }
            }
            other => stack.push(other),
        }
    }
    stack
}

/// 规范化一个可能尚不存在的路径：直接 `canonicalize` 失败时，回退到最长
/// 存在祖先规范化，再按原序拼接不存在的尾部组件。
///
/// 这样既能消解已存在部分的符号链接与 `..`，又能保留待创建文件名。
fn canonicalize_or_parent(path: &Utf8Path) -> Result<Utf8PathBuf, PathSandboxError> {
    if let Ok(canon) = canonicalize_utf8(path.as_std_path()) {
        return Ok(canon);
    }

    // 自底向上收集不存在的尾部组件，直到命中存在的祖先。
    let mut existing = path.to_path_buf();
    let mut tail: Vec<String> = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| PathSandboxError::NotFound {
                path: path.to_string(),
            })?
            .to_owned();
        tail.push(name);
        existing = existing
            .parent()
            .ok_or_else(|| PathSandboxError::NotFound {
                path: path.to_string(),
            })?
            .to_path_buf();
    }

    let mut canon =
        canonicalize_utf8(existing.as_std_path()).map_err(|_| PathSandboxError::NotFound {
            path: path.to_string(),
        })?;

    // tail 是自底向上收集的，逆序拼接还原原始相对顺序。
    for comp in tail.into_iter().rev() {
        canon.push(&comp);
    }
    Ok(canon)
}

/// 将 `std::path::Path` 规范化并转回 `Utf8PathBuf`，非 UTF-8 路径视为不可解析。
fn canonicalize_utf8(path: &std::path::Path) -> std::io::Result<Utf8PathBuf> {
    let canon = std::fs::canonicalize(path)?;
    Utf8PathBuf::from_path_buf(canon).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "non-utf8 canonical path")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    /// C-03：`..` 穿越到 workdir 之外必须被拒绝（T-M1-9 验收）。
    #[test]
    fn rejects_path_escape_via_dotdot() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let workdir =
            Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("tempdir path is utf8");

        let err =
            resolve_under(&workdir, "../../etc/passwd").expect_err("escape should be rejected");
        assert!(
            matches!(err, PathSandboxError::Escaped { .. }),
            "expected Escaped, got {err:?}"
        );
    }

    /// 绝对路径越界同样被拒绝。
    #[test]
    fn rejects_absolute_escape() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let workdir =
            Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("tempdir path is utf8");

        let outside = if cfg!(unix) {
            "/etc/passwd"
        } else {
            "C:\\Windows\\System32\\drivers\\etc\\hosts"
        };
        let err = resolve_under(&workdir, outside).expect_err("escape should be rejected");
        assert!(
            matches!(err, PathSandboxError::Escaped { .. }),
            "expected Escaped, got {err:?}"
        );
    }

    /// R9 P2-4：`~` 前缀路径显式拒绝（策略层不展开 `~`，按相对解析会误放行；
    /// 防未来 shell 展开路径变绝对逃逸口）。
    #[test]
    fn rejects_tilde_prefix_paths() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let workdir =
            Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("tempdir path is utf8");

        for input in ["~/.ssh/id_rsa", "~/config", "~someone/x"] {
            let err = resolve_under(&workdir, input).expect_err("~ 路径应被拒绝");
            assert!(
                matches!(err, PathSandboxError::Escaped { .. }),
                "expected Escaped for `{input}`, got {err:?}"
            );
        }
    }

    /// workdir 内的相对路径应正常解析。
    #[test]
    fn allows_path_under_workdir() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let workdir =
            Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("tempdir path is utf8");

        // 先在 workdir 下创建一个文件，确保 canonicalize 能成功
        let file_path = workdir.join("test_file.txt");
        std::fs::write(&file_path, "hello").expect("write file");
        let resolved = resolve_under(&workdir, "test_file.txt").expect("path under workdir");
        assert!(resolved.starts_with(workdir.canonicalize_utf8().unwrap()));
    }

    /// `is_under` 组件级前缀匹配：`/foo/barbaz` 不在 `/foo/bar` 之下。
    #[test]
    fn is_under_component_prefix() {
        let parent = Utf8Path::new("/foo/bar");
        let child_in = Utf8Path::new("/foo/bar/baz");
        let child_out = Utf8Path::new("/foo/barbaz");
        assert!(is_under(child_in, parent));
        assert!(!is_under(child_out, parent));
    }

    /// R9 PATH-1：`is_under` 必须规范化 `..`——词法 `starts_with` 会把
    /// `/tmp/wd/nodir/../../evil/f.txt` 判为在 `/tmp/wd` 之下（`..` 段不弹出）。
    #[test]
    fn is_under_resolves_parent_segments() {
        let parent = Utf8Path::new("/tmp/wd");
        // `..` 弹出后实际落在 /tmp 之下，不在 /tmp/wd 之下
        assert!(!is_under(
            Utf8Path::new("/tmp/wd/nodir/../../evil/f.txt"),
            parent
        ));
        assert!(!is_under(Utf8Path::new("/tmp/wd/../../etc/passwd"), parent));
        // 同级 `..` 归位后仍在父目录内
        assert!(is_under(Utf8Path::new("/tmp/wd/a/../b.txt"), parent));
        assert!(is_under(Utf8Path::new("/tmp/wd/sub/../c.txt"), parent));
        // 父目录自身 + 直接子路径
        assert!(is_under(Utf8Path::new("/tmp/wd"), parent));
        assert!(is_under(Utf8Path::new("/tmp/wd/x/y.txt"), parent));
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use camino::Utf8PathBuf;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// C-03：任意输入路径不应使 `resolve_under` panic（结果为 Ok 或 Err）。
        #[test]
        fn resolve_under_never_panics(input in "[a-zA-Z0-9_./ -]{0,64}") {
            let tmp = tempfile::tempdir().expect("create tempdir");
            let workdir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
                .expect("tempdir path is utf8");
            let result = resolve_under(&workdir, &input);
            // 仅断言不 panic：Ok 或 Err(Escaped)/Err(NotFound) 均合法
            let ok_or_expected_err = result.is_ok()
                || matches!(
                    result,
                    Err(PathSandboxError::Escaped { .. } | PathSandboxError::NotFound { .. })
                );
            prop_assert!(ok_or_expected_err);
        }

        /// C-03：若 `resolve_under` 返回 Ok，解析结果必然落在 workdir 之下（越界不可绕过）。
        #[test]
        fn resolved_path_always_under_workdir(input in "[a-zA-Z0-9_./]{0,64}") {
            let tmp = tempfile::tempdir().expect("create tempdir");
            let workdir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
                .expect("tempdir path is utf8");
            let canon_workdir =
                std::fs::canonicalize(workdir.as_std_path()).expect("canonicalize workdir");
            let canon_workdir = Utf8PathBuf::from_path_buf(canon_workdir)
                .expect("canonical workdir path is utf8");
            if let Ok(resolved) = resolve_under(&workdir, &input) {
                prop_assert!(
                    resolved.starts_with(&canon_workdir),
                    "resolved path '{}' must be under workdir '{}'",
                    resolved,
                    canon_workdir
                );
            }
        }

        /// C-03：绝对路径越界（workdir 之外）应始终被拒绝（Escaped）。
        #[test]
        fn absolute_outside_path_rejected(input in "/[a-zA-Z0-9_]{1,40}") {
            let tmp = tempfile::tempdir().expect("create tempdir");
            let workdir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
                .expect("tempdir path is utf8");
            let canon_workdir =
                std::fs::canonicalize(workdir.as_std_path()).expect("canonicalize workdir");
            let canon_workdir = Utf8PathBuf::from_path_buf(canon_workdir)
                .expect("canonical workdir path is utf8");
            if let Ok(resolved) = resolve_under(&workdir, &input) {
                // 若 Ok 则必在 workdir 下（巧合命中 workdir 内绝对路径的极小概率情形）
                prop_assert!(
                    resolved.starts_with(&canon_workdir),
                    "absolute path '{}' resolved to '{}' outside workdir '{}'",
                    input,
                    resolved,
                    canon_workdir
                );
            }
        }
    }
}
