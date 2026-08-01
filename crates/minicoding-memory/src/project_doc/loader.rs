//! `ProjectDocLoader` 实现：从 `repo_root` 到 `cwd` 逐级加载项目文档。
//!
//! 设计要点（见 `design.md` §8.6）：
//! - **分层加载**：从 `repo_root` 逐级向下走到 `cwd`，每级按优先级
//!   （`AGENTS.md` > `CLAUDE.md` > `.cursorrules`）取首个命中文件；
//! - **拼接**：root → leaf 顺序，以 `---` 分隔并标注来源路径；
//! - **截断**：累计超过 `max_bytes`（默认 32 KiB）时静默截断末尾并标注
//!   `[... truncated]`；
//! - **skip**：Explore/Plan 子 Agent 通过 `skip` 跳过加载，保持廉价。
//!
//! C-05：加载内容由 `inject::inject_project_doc` 包裹 `<project_doc>` 边界，
//! 声明项目约定是受信任的用户输入而非工具输出数据。C-23：AGENTS.md 不可被 Agent
//! 自主编辑（由工具层 `Verdict::Ask` 强制），本模块只负责读取。

use crate::project_doc::fallback::find_project_doc;
use camino::{Utf8Path, Utf8PathBuf};
use minicoding_core::memory::ProjectDocLoader;
use minicoding_core::model::MemoryError;
use minicoding_core::provider::BoxFuture;
use tokio::fs;

/// 默认项目文档最大字节数（32 KiB，见 `design.md` §8.6）。
pub const DEFAULT_PROJECT_DOC_MAX_BYTES: usize = 32_768;

/// 截断标注（追加到截断内容末尾）。
const TRUNCATED_MARKER: &str = "\n[... truncated]";

/// 项目文档加载器实现。
///
/// 从 `repo_root` 逐级向下走到 `cwd`，每级目录按优先级
/// （`AGENTS.md` > `CLAUDE.md` > `.cursorrules`）取首个命中文件，拼接为单一字符串。
/// 拼接时以 `---` 分隔并标注来源路径；累计超过 `max_bytes` 时静默截断末尾并标注
/// `[... truncated]`。
///
/// `skip` 为 `true` 时（Explore/Plan 子 Agent，见 `design.md` §8.6「加载时机」）
/// 返回空串，不读取任何文件，保持子 Agent 廉价。
pub struct ProjectDocLoaderImpl {
    /// 仓库根目录（分层加载的起点）。
    repo_root: Utf8PathBuf,
    /// 当前工作目录（分层加载的终点）。
    cwd: Utf8PathBuf,
    /// 累计字节上限（默认 32 KiB）。
    max_bytes: usize,
    /// 子 Agent 跳过标志。
    skip: bool,
}

impl ProjectDocLoaderImpl {
    /// 构造加载器，`max_bytes` 默认 32 KiB，`skip` 默认 `false`。
    ///
    /// `repo_root` 与 `cwd` 应为一致形式（同为规范化或同非规范化路径）；
    /// 若 `cwd` 不在 `repo_root` 之下，加载时退化为仅读取 `cwd` 一级。
    #[must_use]
    pub fn new(repo_root: Utf8PathBuf, cwd: Utf8PathBuf) -> Self {
        Self {
            repo_root,
            cwd,
            max_bytes: DEFAULT_PROJECT_DOC_MAX_BYTES,
            skip: false,
        }
    }

    /// 设置最大字节数（builder）。
    #[must_use]
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// 设置 skip 标志（builder）。`true` 时 `load` 直接返回空串。
    #[must_use]
    pub fn with_skip(mut self, skip: bool) -> Self {
        self.skip = skip;
        self
    }

    /// 计算从 `repo_root` 到 `cwd` 的目录链（含两端，root 在前）。
    ///
    /// `cwd` 等于 `repo_root` 时返回 `[cwd]`；`cwd` 不在 `repo_root` 之下时
    /// 退化为 `[cwd]`（仅读取 cwd 一级，不向上回溯 `repo_root`）。
    fn dir_chain(&self) -> Vec<Utf8PathBuf> {
        if self.cwd.as_str() == self.repo_root.as_str() {
            return vec![self.cwd.clone()];
        }
        if !self.cwd.starts_with(self.repo_root.as_str()) {
            return vec![self.cwd.clone()];
        }
        let mut chain = Vec::new();
        let mut current: &Utf8Path = self.cwd.as_path();
        loop {
            chain.push(current.to_owned());
            if current.as_str() == self.repo_root.as_str() {
                break;
            }
            match current.parent() {
                Some(p) => current = p,
                None => break,
            }
        }
        chain.reverse();
        chain
    }
}

impl ProjectDocLoader for ProjectDocLoaderImpl {
    fn load(&self) -> BoxFuture<'_, Result<String, MemoryError>> {
        Box::pin(async move {
            if self.skip {
                return Ok(String::new());
            }

            let chain = self.dir_chain();
            let mut parts: Vec<String> = Vec::new();
            for dir in &chain {
                let Some(path) = find_project_doc(dir) else {
                    continue;
                };
                let content = fs::read_to_string(&path).await?;
                let trimmed = content.trim();
                if trimmed.is_empty() {
                    continue;
                }
                // 标注来源路径：优先相对 `repo_root`，否则用绝对路径。
                let rel = path
                    .strip_prefix(self.repo_root.as_str())
                    .map_or_else(|_| path.as_str(), Utf8Path::as_str);
                let entry = format!("# source: {rel}\n\n{trimmed}");
                parts.push(entry);
            }

            if parts.is_empty() {
                return Ok(String::new());
            }

            let merged = parts.join("\n\n---\n\n");
            if merged.len() <= self.max_bytes {
                return Ok(merged);
            }

            // 截断到 char 边界，末尾标注（标注本身不计入 max_bytes）。
            let mut end = self.max_bytes.min(merged.len());
            while end > 0 && !merged.is_char_boundary(end) {
                end -= 1;
            }
            let mut truncated = String::from(&merged[..end]);
            truncated.push_str(TRUNCATED_MARKER);
            Ok(truncated)
        })
    }
}

#[cfg(test)]
mod tests {
    //! 最小单元测试：验证分层加载、fallback、截断与 skip（任务验收要求）。

    use super::*;
    use camino::Utf8PathBuf;

    fn utf8(dir: &std::path::Path) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(dir.to_owned()).expect("tempdir path is UTF-8 on linux test env")
    }

    fn write(dir: &std::path::Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    #[tokio::test]
    async fn loads_single_level_agents_md() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "AGENTS.md", "# Root\nroot rules");

        let dir = utf8(tmp.path());
        let loader = ProjectDocLoaderImpl::new(dir.clone(), dir);
        let doc = loader.load().await.unwrap();
        assert!(doc.contains("root rules"));
        assert!(doc.contains("AGENTS.md"));
    }

    #[tokio::test]
    async fn loads_hierarchical_with_separator_and_source() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "AGENTS.md", "root rules");
        let sub = root.join("pkg");
        std::fs::create_dir_all(&sub).unwrap();
        write(&sub, "CLAUDE.md", "sub rules");

        let loader = ProjectDocLoaderImpl::new(utf8(root), utf8(&sub));
        let doc = loader.load().await.unwrap();

        assert!(doc.contains("root rules"));
        assert!(doc.contains("sub rules"));
        // 用 --- 分隔。
        assert!(doc.contains("\n---\n"));
        // 标注两个来源路径。
        assert!(doc.contains("AGENTS.md"));
        assert!(doc.contains("CLAUDE.md"));
        // root 在 sub 之前。
        assert!(doc.find("root rules").unwrap() < doc.find("sub rules").unwrap());
    }

    #[tokio::test]
    async fn skips_empty_files() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "AGENTS.md", "   \n\t  ");

        let dir = utf8(tmp.path());
        let loader = ProjectDocLoaderImpl::new(dir.clone(), dir);
        let doc = loader.load().await.unwrap();
        assert!(doc.is_empty());
    }

    #[tokio::test]
    async fn returns_empty_when_no_doc_found() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = utf8(tmp.path());
        let loader = ProjectDocLoaderImpl::new(dir.clone(), dir);
        let doc = loader.load().await.unwrap();
        assert!(doc.is_empty());
    }

    #[tokio::test]
    async fn truncates_over_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let big = "x".repeat(200);
        write(tmp.path(), "AGENTS.md", &big);

        let dir = utf8(tmp.path());
        let loader = ProjectDocLoaderImpl::new(dir.clone(), dir).with_max_bytes(50);
        let doc = loader.load().await.unwrap();
        assert!(doc.ends_with("[... truncated]"));
        // 截断后内容小于原始。
        assert!(doc.len() < big.len());
    }

    #[tokio::test]
    async fn no_truncation_under_limit() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "AGENTS.md", "short content");

        let dir = utf8(tmp.path());
        let loader = ProjectDocLoaderImpl::new(dir.clone(), dir).with_max_bytes(1024);
        let doc = loader.load().await.unwrap();
        assert!(!doc.contains("[... truncated]"));
    }

    #[tokio::test]
    async fn skip_flag_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "AGENTS.md", "should not load");

        let dir = utf8(tmp.path());
        let loader = ProjectDocLoaderImpl::new(dir.clone(), dir).with_skip(true);
        let doc = loader.load().await.unwrap();
        assert!(doc.is_empty());
    }

    #[tokio::test]
    async fn cwd_outside_repo_root_degrades_to_cwd_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "AGENTS.md", "root rules");
        // 另一个独立目录（不在 repo_root 下）。
        let other = tempfile::tempdir().unwrap();
        write(other.path(), "AGENTS.md", "other rules");

        let loader = ProjectDocLoaderImpl::new(utf8(root), utf8(other.path()));
        let doc = loader.load().await.unwrap();
        assert!(doc.contains("other rules"));
        assert!(!doc.contains("root rules"));
    }

    #[tokio::test]
    async fn fallback_to_claude_md_in_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "AGENTS.md", "root via agents");
        let sub = root.join("src");
        std::fs::create_dir_all(&sub).unwrap();
        // 子目录只有 CLAUDE.md，验证 fallback 命中。
        write(&sub, "CLAUDE.md", "sub via claude");

        let loader = ProjectDocLoaderImpl::new(utf8(root), utf8(&sub));
        let doc = loader.load().await.unwrap();
        assert!(doc.contains("root via agents"));
        assert!(doc.contains("sub via claude"));
    }
}
