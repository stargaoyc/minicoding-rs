//! 项目文档查找辅助：在目录中查找首个 fallback 文件，向上探测仓库根。
//!
//! 详见 `design.md` §8.6：fallback 文件名让 minicoding 无需改名即可复用其他工具
//! （Claude Code / Cursor）已写好的项目约定文件。

use camino::{Utf8Path, Utf8PathBuf};

/// fallback 文件名优先级（高 → 低）。
///
/// `AGENTS.md` 优先；未命中则回退至 `CLAUDE.md`/`.cursorrules`，复用其他工具
/// 已写好的项目约定文件。每级目录至多命中一个文件（不重复加载同目录多文件）。
pub const FALLBACK_FILENAMES: &[&str] = &["AGENTS.md", "CLAUDE.md", ".cursorrules"];

/// 仓库根探测标记（命中其一即视为仓库根）。
const REPO_MARKERS: &[&str] = &[".git", ".hg", ".svn"];

/// 在指定目录按优先级查找首个存在的项目文档文件。
///
/// 查找顺序为 `AGENTS.md` → `CLAUDE.md` → `.cursorrules`，返回首个存在的文件路径。
/// 同目录至多命中一个文件（命中即返回，不继续探测低优先级文件，避免重复加载）。
///
/// # Examples
/// ```no_run
/// use camino::Utf8Path;
/// use minicoding_memory::project_doc::fallback::find_project_doc;
/// let path = find_project_doc(Utf8Path::new("/repo"));
/// ```
#[must_use]
pub fn find_project_doc(dir: &Utf8Path) -> Option<Utf8PathBuf> {
    for &name in FALLBACK_FILENAMES {
        let path = dir.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// 从 `start` 向上探测仓库根目录。
///
/// 逐级向上检查 `.git`/`.hg`/`.svn` 目录，命中即返回该级目录；探到文件系统根仍未
/// 命中则返回 `None`，调用方可退化为以 `start` 作为仓库根。
///
/// # Examples
/// ```no_run
/// use camino::Utf8Path;
/// use minicoding_memory::project_doc::fallback::find_repo_root;
/// let root = find_repo_root(Utf8Path::new("/repo/sub"));
/// ```
#[must_use]
pub fn find_repo_root(start: &Utf8Path) -> Option<Utf8PathBuf> {
    let mut current = start;
    loop {
        for &marker in REPO_MARKERS {
            if current.join(marker).is_dir() {
                return Some(current.to_owned());
            }
        }
        current = current.parent()?;
    }
}

#[cfg(test)]
mod tests {
    //! 验证 fallback 优先级与 `repo_root` 向上探测。

    use super::*;
    use camino::Utf8PathBuf;

    fn utf8(dir: &std::path::Path) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(dir.to_owned()).expect("tempdir path is UTF-8 on linux test env")
    }

    #[test]
    fn find_project_doc_prefers_agents_md() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "agents").unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "claude").unwrap();
        std::fs::write(tmp.path().join(".cursorrules"), "cursor").unwrap();

        let found = find_project_doc(&utf8(tmp.path())).unwrap();
        assert!(found.as_str().ends_with("AGENTS.md"));
    }

    #[test]
    fn find_project_doc_falls_back_to_claude_md() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "claude").unwrap();
        std::fs::write(tmp.path().join(".cursorrules"), "cursor").unwrap();

        let found = find_project_doc(&utf8(tmp.path())).unwrap();
        assert!(found.as_str().ends_with("CLAUDE.md"));
    }

    #[test]
    fn find_project_doc_falls_back_to_cursorrules() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".cursorrules"), "cursor").unwrap();

        let found = find_project_doc(&utf8(tmp.path())).unwrap();
        assert!(found.as_str().ends_with(".cursorrules"));
    }

    #[test]
    fn find_project_doc_returns_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find_project_doc(&utf8(tmp.path())).is_none());
    }

    #[test]
    fn find_repo_root_finds_git_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let deep = root.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();

        let found = find_repo_root(&utf8(&deep)).unwrap();
        assert_eq!(found, utf8(root));
    }

    #[test]
    fn find_repo_root_returns_none_without_marker() {
        // tempdir 位于 /tmp 下，祖先路径无 .git/.hg/.svn。
        let tmp = tempfile::tempdir().unwrap();
        assert!(find_repo_root(&utf8(tmp.path())).is_none());
    }
}
