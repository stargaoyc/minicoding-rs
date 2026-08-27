//! `ProjectDocLoader` 实现：从 `repo_root` 到 `cwd` 逐级加载项目文档。
//!
//! 设计要点（见 `design.md` §8.6）：
//! - **分层加载**：从 `repo_root` 逐级向下走到 `cwd`，每级按优先级
//!   （`AGENTS.md` > `CLAUDE.md` > `.cursorrules`）取首个命中文件；
//! - **全局层**（B4）：可选在分层链头部插入 `$MINICODING_HOME/AGENTS.md`
//!   （见 [`crate::global_agents_path`]），来源标注 `# source: <global>`，
//!   经 `with_global_layer_from_env` 显式启用（测试路径默认关闭，保证封闭性）；
//! - **@import 展开**（B4）：行首 `@import <path>` 替换为所引文件内容，递归
//!   深度 ≤ [`MAX_IMPORT_DEPTH`]，环检测经 canonicalize 后集合判定；
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
use minicoding_core::otel::span_name;
use minicoding_core::provider::BoxFuture;
use std::collections::HashSet;
use tokio::fs;

/// 默认项目文档最大字节数（32 KiB，见 `design.md` §8.6）。
pub const DEFAULT_PROJECT_DOC_MAX_BYTES: usize = 32_768;

/// 截断标注（追加到截断内容末尾）。
const TRUNCATED_MARKER: &str = "\n[... truncated]";

/// @import 行前缀（允许缩进；目标为相对或绝对路径）。
const IMPORT_PREFIX: &str = "@import ";

/// @import 最大递归深度（含顶层文件的直接 import 共 3 层间接展开）。
pub const MAX_IMPORT_DEPTH: usize = 3;

/// @import 超深跳过标注。
const IMPORT_SKIP_DEPTH: &str = "<!-- import skipped: depth -->";
/// @import 环引用跳过标注。
const IMPORT_SKIP_CYCLE: &str = "<!-- import skipped: cycle -->";
/// CT4-3（R4）：`@import` 目标越出 `base_dir`（绝对路径 / `..` 逃逸）跳过标注。
const IMPORT_SKIP_OUTSIDE: &str = "<!-- import skipped: outside base dir -->";
/// @import 目标缺失/不可读跳过标注。
fn import_skip_unreadable(path: &Utf8Path) -> String {
    format!("<!-- import skipped: not found ({path}) -->")
}

/// 解析 @import 行的目标路径；非 import 行返回 `None`。
fn parse_import_line(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix(IMPORT_PREFIX)?.trim();
    (!rest.is_empty()).then(|| rest.trim_matches('"').trim_matches('\'').to_string())
}

/// 规范化 key（环检测用）：优先 filesystem canonicalize，失败退化为词法绝对路径。
fn canonical_key(path: &Utf8Path) -> String {
    match std::fs::canonicalize(path) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) if path.is_absolute() => path.to_string(),
        Err(_) => Utf8PathBuf::from(".").join(path).to_string(),
    }
}

/// 组件级路径包含判定（CTX-5）：`dir` 与 `base` 逐组件比较前缀，
/// 消除裸字符串 `starts_with` 的兄弟目录误判与尾斜杠退化。
///
/// SEC-1（2026-08-27 R5 审查）：比较前先词法消解 `..` 段——此前
/// `repo/.../deep/../../../../etc/passwd` 的组件前缀与 `base_dir` 命中即放行，
/// `..` 逃逸不被察觉，恶意仓库可经 `@import ../../etc/passwd` 把本机任意文件
/// 展开进 `<project_doc>` 外发 LLM 厂商（CT4-3 防护被绕过）。已实测复现。
///
/// 2026-08-27 发布修复（跨平台）：`dir` 与 `base` **都必须**过 `resolve_lexical`——
/// 该规范化会把 Windows 绝对路径 `C:\...` 的驱动前缀并入根（`Prefix` 后被
/// `RootDir` 清空、再补 `/` 根），裸 `base` 保留 `Prefix("C:")`，与规范化后的
/// `dir` 组件首位不一致，Windows 上所有合法 import 都被误判越界、`dir_chain`
/// 退化为单级。两侧对称规范化后比较一致。
fn path_within(dir: &Utf8Path, base: &Utf8Path) -> bool {
    let norm_dir = resolve_lexical(dir);
    let norm_base = resolve_lexical(base);
    let d: Vec<_> = norm_dir.components().collect();
    let b: Vec<_> = norm_base.components().collect();
    d.len() >= b.len() && d[..b.len()] == b[..]
}

/// 词法规范化路径（SEC-1）：消解 `.`/`..` 段，不触碰文件系统、不解 symlink。
///
/// `..` 弹出上一段（栈空时保留 `..`，与 `core::util::normalize_lexical_rel_path`
/// 语义一致）；`RootDir` 重置栈并标记绝对路径。仅用于包含判定，不改变
/// `read_to_string` 的目标路径语义（读失败仍走 not-found 分支）。
fn resolve_lexical(path: &Utf8Path) -> Utf8PathBuf {
    use camino::Utf8Component;
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.components() {
        match comp {
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else {
                    parts.push("..");
                }
            }
            Utf8Component::Normal(s) => parts.push(s),
            Utf8Component::RootDir => parts.clear(),
            // Windows 前缀（如 `C:\`）：保留前缀
            Utf8Component::Prefix(p) => {
                parts.clear();
                // 前缀作为第一段——保留原始前缀语义
                parts.push(p.as_str());
            }
        }
    }
    let joined = parts.join("/");
    if path.is_absolute() {
        Utf8PathBuf::from("/").join(&joined)
    } else {
        Utf8PathBuf::from(&joined)
    }
}

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
    /// 全局层文件（B4，默认 `None`；`with_global_layer_from_env` 启用）。
    ///
    /// 显式 opt-in 而非构造即读 env：保证既有测试封闭性（不受开发机
    /// `~/.minicoding/AGENTS.md` 存在与否影响），生产路径由 builder 打开。
    global_layer: Option<Utf8PathBuf>,
}

impl ProjectDocLoaderImpl {
    /// 构造加载器，`max_bytes` 默认 32 KiB，`skip` 默认 `false`，全局层默认关闭。
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
            global_layer: None,
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

    /// 设置全局层文件（B4 builder）。`Some(path)` 时作为分层链头部注入，
    /// 来源标注 `# source: <global>`。
    #[must_use]
    pub fn with_global_layer(mut self, path: Option<Utf8PathBuf>) -> Self {
        self.global_layer = path;
        self
    }

    /// 从环境解析全局层（B4 builder）：`$MINICODING_HOME/AGENTS.md` 存在才启用。
    #[must_use]
    pub fn with_global_layer_from_env(self) -> Self {
        self.with_global_layer(crate::global_agents_path())
    }

    /// 计算从 `repo_root` 到 `cwd` 的目录链（含两端，root 在前）。
    ///
    /// `cwd` 等于 `repo_root` 时返回 `[cwd]`；`cwd` 不在 `repo_root` 之下时
    /// 退化为 `[cwd]`（仅读取 cwd 一级，不向上回溯 `repo_root`）。
    ///
    /// CTX-5（2026-08-25 R2 审查）：包含性判定用**组件级**比较——裸字符串
    /// `starts_with` 会把 `/repo2`（兄弟目录）误判为 `/repo` 的子目录，且
    /// 尾斜杠形态不匹配时退化到 cwd 单级；最坏情况沿 parent 链一路加载到
    /// 文件系统根的 AGENTS.md。
    fn dir_chain(&self) -> Vec<Utf8PathBuf> {
        if self.cwd.as_str() == self.repo_root.as_str() {
            return vec![self.cwd.clone()];
        }
        if !path_within(&self.cwd, &self.repo_root) {
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

    /// 同步加载项目文档（builder 启动期用，tokio runtime 未创建）。
    ///
    /// 与 `load` 同语义，但用 `std::fs` 同步读取。失败时返回 `MemoryError`，
    /// 单个文件读取失败时跳过（best effort，与 async 版一致）。全局层（如启用）
    /// 在分层链之前注入，来源标注 `# source: <global>`；各文件内容先经
    /// `expand_imports_sync` 预处理。
    ///
    /// # Errors
    /// 路径解析失败时返回错误；单个文件读取失败时跳过而非报错。
    pub fn load_sync(&self) -> Result<String, MemoryError> {
        if self.skip {
            return Ok(String::new());
        }

        let mut parts: Vec<String> = Vec::new();

        // B4 全局层：链头注入（root 之前），来源标注固定为 `<global>`。
        if let Some(global_path) = &self.global_layer {
            match std::fs::read_to_string(global_path) {
                Ok(content) => {
                    let mut visited = HashSet::new();
                    visited.insert(canonical_key(global_path));
                    let global_dir = global_path.parent().unwrap_or_else(|| Utf8Path::new("."));
                    let expanded =
                        expand_imports_sync(&content, global_dir, 0, &mut visited, global_dir);
                    push_part(&mut parts, &expanded, "<global>", self.repo_root.as_str());
                }
                // 全局层不可读不阻塞项目层加载（best effort）。
                Err(e) => tracing::warn!("skip unreadable global AGENTS.md {}: {e}", global_path),
            }
        }

        for dir in self.dir_chain() {
            let Some(path) = find_project_doc(&dir) else {
                continue;
            };
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("skip unreadable project doc {}: {e}", path);
                    continue;
                }
            };
            let mut visited = HashSet::new();
            visited.insert(canonical_key(&path));
            let expanded = expand_imports_sync(
                &content,
                path.parent().unwrap_or_else(|| Utf8Path::new(".")),
                0,
                &mut visited,
                &self.repo_root,
            );
            push_part(
                &mut parts,
                &expanded,
                path.as_str(),
                self.repo_root.as_str(),
            );
        }

        Ok(merge_parts(&parts, self.max_bytes))
    }
}

/// 展开一节内容并追加到 parts（来源标注优先相对 `repo_root`）。
fn push_part(parts: &mut Vec<String>, content: &str, source: &str, repo_root: &str) {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return;
    }
    let rel = Utf8Path::new(source)
        .strip_prefix(repo_root)
        .map_or_else(|_| source.to_string(), ToString::to_string);
    parts.push(format!("# source: {rel}\n\n{trimmed}"));
}

/// 合并 parts 并按 `max_bytes` 截断（char 边界安全）。
fn merge_parts(parts: &[String], max_bytes: usize) -> String {
    if parts.is_empty() {
        return String::new();
    }
    let merged = parts.join("\n\n---\n\n");
    if merged.len() <= max_bytes {
        return merged;
    }
    let mut end = max_bytes.min(merged.len());
    while end > 0 && !merged.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = String::from(&merged[..end]);
    truncated.push_str(TRUNCATED_MARKER);
    truncated
}

/// 同步 `@import` 展开（B4）。
///
/// 行首（允许缩进）`@import <path>` 替换为所引文件内容并递归展开：
/// - 相对路径基于当前文件所在目录；
/// - 环检测：canonicalize 后的 key 已在 `visited` → 插入 cycle 标注；
/// - 深度防护：当前深度 ≥ [`MAX_IMPORT_DEPTH`] 时插入 depth 标注；
/// - 目标缺失/不可读插入 not found 标注（不 panic、不中断其余行）。
///
/// CT4-3（R4）：`base_dir` 包含约束——import 目标必须落在 `base_dir` 内
/// （组件级包含，拒绝 `..` 逃逸与任意绝对路径）。此前 `@import /home/u/.aws/
/// credentials` 这类恶意仓库指令可把任意本机文件展开进 `<project_doc>` 随
/// system prompt 外发（数据外泄通道；`post_compact` 已有同款检查，`@import` 缺位）。
pub fn expand_imports_sync<S: std::hash::BuildHasher>(
    content: &str,
    cur_dir: &Utf8Path,
    depth: usize,
    visited: &mut HashSet<String, S>,
    base_dir: &Utf8Path,
) -> String {
    let mut out = String::with_capacity(content.len());
    for line in content.lines() {
        match parse_import_line(line) {
            None => {
                out.push_str(line);
                out.push('\n');
            }
            Some(target) => {
                if depth >= MAX_IMPORT_DEPTH {
                    out.push_str(IMPORT_SKIP_DEPTH);
                    out.push('\n');
                    continue;
                }
                let target_path = if Utf8Path::new(&target).is_absolute() {
                    Utf8PathBuf::from(&target)
                } else {
                    cur_dir.join(&target)
                };
                // CT4-3：包含约束（组件级，防 `..` 逃逸；与 journal S18 同款语义）
                if !path_within(&target_path, base_dir) {
                    out.push_str(IMPORT_SKIP_OUTSIDE);
                    out.push('\n');
                    continue;
                }
                let key = canonical_key(&target_path);
                if !visited.insert(key) {
                    out.push_str(IMPORT_SKIP_CYCLE);
                    out.push('\n');
                    continue;
                }
                if let Ok(inner) = std::fs::read_to_string(&target_path) {
                    let inner_dir = target_path.parent().unwrap_or_else(|| Utf8Path::new("."));
                    out.push_str(&expand_imports_sync(
                        inner.trim_end(),
                        inner_dir,
                        depth + 1,
                        visited,
                        base_dir,
                    ));
                    out.push('\n');
                } else {
                    out.push_str(&import_skip_unreadable(&target_path));
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// 异步 `@import` 展开（B4）：与同步版同语义，读文件用 `tokio::fs`。
///
/// 环/深度判定与标注格式与 [`expand_imports_sync`] 完全一致——两版必须同步演化，
/// 否则启动期（sync）与会话内（async）加载结果漂移。`base_dir` 包含约束同
/// 同步版（CT4-3）。
async fn expand_imports_async<S: std::hash::BuildHasher>(
    content: &str,
    cur_dir: &Utf8Path,
    depth: usize,
    visited: &mut HashSet<String, S>,
    base_dir: &Utf8Path,
) -> String {
    let mut out = String::with_capacity(content.len());
    for line in content.lines() {
        match parse_import_line(line) {
            None => {
                out.push_str(line);
                out.push('\n');
            }
            Some(target) => {
                if depth >= MAX_IMPORT_DEPTH {
                    out.push_str(IMPORT_SKIP_DEPTH);
                    out.push('\n');
                    continue;
                }
                let target_path = if Utf8Path::new(&target).is_absolute() {
                    Utf8PathBuf::from(&target)
                } else {
                    cur_dir.join(&target)
                };
                if !path_within(&target_path, base_dir) {
                    out.push_str(IMPORT_SKIP_OUTSIDE);
                    out.push('\n');
                    continue;
                }
                let key = canonical_key(&target_path);
                if !visited.insert(key) {
                    out.push_str(IMPORT_SKIP_CYCLE);
                    out.push('\n');
                    continue;
                }
                if let Ok(inner) = fs::read_to_string(&target_path).await {
                    let inner_dir = target_path.parent().unwrap_or_else(|| Utf8Path::new("."));
                    // 递归 async fn 需显式 boxing（E0733）。
                    let expanded = Box::pin(expand_imports_async(
                        inner.trim_end(),
                        inner_dir,
                        depth + 1,
                        visited,
                        base_dir,
                    ))
                    .await;
                    out.push_str(&expanded);
                    out.push('\n');
                } else {
                    out.push_str(&import_skip_unreadable(&target_path));
                    out.push('\n');
                }
            }
        }
    }
    out
}

impl ProjectDocLoader for ProjectDocLoaderImpl {
    #[tracing::instrument(skip(self), fields(otel.name = span_name::MEMORY_LOAD, memory.type = "project_doc"))]
    fn load(&self) -> BoxFuture<'_, Result<String, MemoryError>> {
        Box::pin(async move {
            if self.skip {
                return Ok(String::new());
            }

            let mut parts: Vec<String> = Vec::new();

            // B4 全局层：链头注入（与 load_sync 同语义）。
            if let Some(global_path) = &self.global_layer {
                match fs::read_to_string(global_path).await {
                    Ok(content) => {
                        let mut visited = HashSet::new();
                        visited.insert(canonical_key(global_path));
                        let global_dir = global_path.parent().unwrap_or_else(|| Utf8Path::new("."));
                        let expanded =
                            expand_imports_async(&content, global_dir, 0, &mut visited, global_dir)
                                .await;
                        push_part(&mut parts, &expanded, "<global>", self.repo_root.as_str());
                    }
                    Err(e) => {
                        tracing::warn!("skip unreadable global AGENTS.md {}: {e}", global_path);
                    }
                }
            }

            for dir in self.dir_chain() {
                let Some(path) = find_project_doc(&dir) else {
                    continue;
                };
                // CTX-2（2026-08-25 R2 审查）：单个不可读文件 warn+跳过，
                // 与 `load_sync` 同语义——此前 async 版 `?` 向上传播使整条
                // 分层链失败，doc comment 却声称两版一致。
                let content = match fs::read_to_string(&path).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("skip unreadable project doc {}: {e}", path);
                        continue;
                    }
                };
                let mut visited = HashSet::new();
                visited.insert(canonical_key(&path));
                let expanded = expand_imports_async(
                    &content,
                    path.parent().unwrap_or_else(|| Utf8Path::new(".")),
                    0,
                    &mut visited,
                    &self.repo_root,
                )
                .await;
                push_part(
                    &mut parts,
                    &expanded,
                    path.as_str(),
                    self.repo_root.as_str(),
                );
            }

            Ok(merge_parts(&parts, self.max_bytes))
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
        assert!(doc.is_empty(), "expected empty: doc");
    }

    #[tokio::test]
    async fn returns_empty_when_no_doc_found() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = utf8(tmp.path());
        let loader = ProjectDocLoaderImpl::new(dir.clone(), dir);
        let doc = loader.load().await.unwrap();
        assert!(doc.is_empty(), "expected empty: doc");
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
        assert!(doc.is_empty(), "expected empty: doc");
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

    // === B4：全局层注入 ===

    #[tokio::test]
    async fn global_layer_is_prepended_with_source_marker() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "AGENTS.md", "project rules");
        let global_dir = tempfile::tempdir().unwrap();
        write(global_dir.path(), "AGENTS.md", "global rules");
        let global_path = Utf8PathBuf::from_path_buf(global_dir.path().join("AGENTS.md")).unwrap();

        let dir = utf8(tmp.path());
        let loader =
            ProjectDocLoaderImpl::new(dir.clone(), dir).with_global_layer(Some(global_path));
        for doc in [loader.load().await.unwrap(), loader.load_sync().unwrap()] {
            assert!(doc.contains("# source: <global>"), "应标注全局来源: {doc}");
            assert!(doc.contains("global rules"));
            assert!(doc.contains("project rules"));
            // 全局层必须在项目层之前。
            assert!(doc.find("global rules").unwrap() < doc.find("project rules").unwrap());
        }
    }

    #[tokio::test]
    async fn no_global_layer_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "AGENTS.md", "project rules");
        let dir = utf8(tmp.path());
        let loader = ProjectDocLoaderImpl::new(dir.clone(), dir);
        let doc = loader.load().await.unwrap();
        assert!(!doc.contains("<global>"), "默认不启用全局层: {doc}");
    }

    #[tokio::test]
    async fn unreadable_global_layer_degrades_gracefully() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "AGENTS.md", "project rules");
        // 指向不存在的全局文件 → warn 跳过，不影响项目层。
        let missing =
            Utf8PathBuf::from_path_buf(tmp.path().join("nope").join("AGENTS.md")).unwrap();
        let dir = utf8(tmp.path());
        let loader = ProjectDocLoaderImpl::new(dir.clone(), dir).with_global_layer(Some(missing));
        let doc = loader.load().await.unwrap();
        assert!(doc.contains("project rules"));
        assert!(!doc.contains("<global>"));
    }

    // === B4：@import 展开（sync/async 同语义） ===

    #[tokio::test]
    async fn import_basic_expansion() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        write(root, "AGENTS.md", "top\n@import docs/shared.md\ntail");
        write(&root.join("docs"), "shared.md", "shared rules 中文");

        let loader = ProjectDocLoaderImpl::new(utf8(root), utf8(root));
        let sync_doc = loader.load_sync().unwrap();
        let async_doc = loader.load().await.unwrap();
        for doc in [sync_doc, async_doc] {
            assert!(doc.contains("shared rules 中文"), "import 应展开: {doc}");
            assert!(
                doc.contains("top") && doc.contains("tail"),
                "宿主行保留: {doc}"
            );
            assert!(!doc.contains("@import"), "指令行不应残留: {doc}");
        }
    }

    #[tokio::test]
    async fn import_cycle_is_detected() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "AGENTS.md", "a\n@import b.md");
        write(tmp.path(), "b.md", "b\n@import AGENTS.md");

        let loader = ProjectDocLoaderImpl::new(utf8(tmp.path()), utf8(tmp.path()));
        let sync_doc = loader.load_sync().unwrap();
        let async_doc = loader.load().await.unwrap();
        for doc in [sync_doc, async_doc] {
            assert!(doc.contains(IMPORT_SKIP_CYCLE), "环引用应标注: {doc}");
            assert!(
                doc.contains('a') && doc.contains('b'),
                "两层内容保留: {doc}"
            );
        }
    }

    #[tokio::test]
    async fn import_depth_limit_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // A → B → C → D → E：E 在第 4 层间接，超 MAX_IMPORT_DEPTH 应跳过。
        write(root, "AGENTS.md", "A\n@import b.md");
        write(root, "b.md", "B\n@import c.md");
        write(root, "c.md", "C\n@import d.md");
        write(root, "d.md", "D\n@import e.md");
        write(root, "e.md", "E");

        let loader = ProjectDocLoaderImpl::new(utf8(root), utf8(root));
        let sync_doc = loader.load_sync().unwrap();
        let async_doc = loader.load().await.unwrap();
        for doc in [sync_doc, async_doc] {
            assert!(
                doc.contains('A') && doc.contains('B') && doc.contains('C') && doc.contains('D'),
                "深度 3 层内全部展开: {doc}"
            );
            assert!(doc.contains(IMPORT_SKIP_DEPTH), "第 4 层应超深标注: {doc}");
            assert!(!doc.lines().any(|l| l == "E"), "E 不应被加载: {doc}");
        }
    }

    #[tokio::test]
    async fn import_missing_target_marks_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "AGENTS.md", "@import ghost.md\nafter");

        let loader = ProjectDocLoaderImpl::new(utf8(tmp.path()), utf8(tmp.path()));
        let sync_doc = loader.load_sync().unwrap();
        let async_doc = loader.load().await.unwrap();
        for doc in [sync_doc, async_doc] {
            assert!(
                doc.contains("import skipped: not found"),
                "缺失目标应标注: {doc}"
            );
            assert!(doc.contains("after"));
        }
    }

    #[test]
    fn expand_imports_relative_paths_resolve_against_current_file() {
        // 相对 import 基于当前文件目录而非 cwd：子目录文件引同目录兄弟文件。
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("pkg");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("inner.md"), "inner\n@import sibling.md").unwrap();
        std::fs::write(sub.join("sibling.md"), "sibling content").unwrap();

        let inner = Utf8PathBuf::from_path_buf(sub.join("inner.md")).unwrap();
        let mut visited = HashSet::new();
        visited.insert(canonical_key(&inner));
        let base = Utf8PathBuf::from_path_buf(sub.parent().unwrap().to_path_buf()).unwrap();
        let out = expand_imports_sync(
            "@import sibling.md",
            inner.parent().unwrap(),
            0,
            &mut visited,
            &base,
        );
        assert!(
            out.contains("sibling content"),
            "相对路径应基于当前文件目录: {out}"
        );
    }

    #[test]
    fn import_outside_base_dir_skipped() {
        // CT4-3：@import 目标越出 base_dir（绝对路径 / `..` 逃逸）必须跳过——
        // 恶意仓库 `@import /home/u/.aws/credentials` 不能把凭证拉进 system prompt
        let root_dir = tempfile::tempdir().expect("root tempdir");
        let secret_dir = tempfile::tempdir().expect("secret tempdir");
        let root = Utf8PathBuf::from_path_buf(root_dir.path().to_path_buf()).unwrap();
        let secret = secret_dir.path().join("secret.txt");
        std::fs::write(&secret, "TOP-SECRET").expect("write secret");

        // 绝对路径越界（secret 在 root 之外）
        let mut visited = HashSet::new();
        let out = expand_imports_sync(
            &format!("@import {}", secret.to_string_lossy()),
            &root,
            0,
            &mut visited,
            &root,
        );
        assert!(
            !out.contains("TOP-SECRET"),
            "绝对路径 import 不得越出 base_dir: {out}"
        );
        assert!(out.contains(IMPORT_SKIP_OUTSIDE), "越界应插入 skip 标注");

        // `..` 相对逃逸（secret 位于 root 的父级之外）
        let mut visited = HashSet::new();
        let out2 = expand_imports_sync("@import ../secret.txt", &root, 0, &mut visited, &root);
        assert!(
            !out2.contains("TOP-SECRET"),
            ".. 逃逸不得越出 base_dir: {out2}"
        );
    }

    #[test]
    fn import_parent_escape_with_real_file_is_blocked() {
        // SEC-1（2026-08-27 R5 审查）：回归测试——`..` 逃逸指向**真实存在**、
        // 位于 base_dir 之外的敏感文件。此前 `path_within` 只做组件级前缀比较，
        // `repo/deep/../../secret.txt` 的前缀组件命中 base_dir 即放行，
        // 恶意仓库可 `@import ../../secret.txt` 把本机任意文件展开进
        // `<project_doc>` 外发 LLM 厂商（CT4-3 防护被绕过）。
        // 旧测试仅覆盖"逃逸目标不存在"（走 not-found 分支），从未命中逃逸路径。
        let tmp = tempfile::tempdir().expect("tmpdir");
        let base = Utf8PathBuf::from_path_buf(tmp.path().join("repo")).unwrap();
        std::fs::create_dir_all(base.join("deep")).expect("create repo/deep");
        // 敏感文件位于 base_dir（repo）之外，但可由 repo/deep 经 `../..` 到达
        let secret_path = tmp.path().join("secret.txt");
        std::fs::write(&secret_path, "TOP-SECRET-REAL-FILE").expect("write secret");

        let mut visited = HashSet::new();
        let out = expand_imports_sync(
            "@import ../../secret.txt",
            &base.join("deep"),
            0,
            &mut visited,
            &base,
        );
        assert!(
            !out.contains("TOP-SECRET-REAL-FILE"),
            "`..` 逃逸指向真实越界文件必须被拦截: {out}"
        );
        assert!(out.contains(IMPORT_SKIP_OUTSIDE), "越界应插入 skip 标注");
    }
}
