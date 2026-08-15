//! `PromptContext`：`PromptContributor::build` 的入参，聚合各 contributor 需要的会话级输入。
//!
//! 设计意图（§22）：避免 contributor 各自重新加载文件——如 `ProjectRules` 不再自己读
//! AGENTS.md，而是从 `ctx.project_rules` 取。`PromptContext` 由 Runtime 在
//! `build_chat_request` 时构造，传入 `PromptPipeline::build`。

use crate::model::{SessionId, ToolSchema};
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

/// 工作区/平台/git 信息（Environment contributor 用）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitInfo {
    /// 当前分支名（如 `main`）。
    pub branch: Option<String>,
    /// HEAD commit short hash（如 `a1b2c3d`）。
    pub head: Option<String>,
    /// 工作区是否有未提交改动。
    pub dirty: bool,
}

/// 平台信息（Environment contributor 用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Linux,
    Macos,
    Windows,
    Other,
}

impl Platform {
    /// 从 `std::env::consts::OS` 推断。
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::consts::OS {
            "linux" => Self::Linux,
            "macos" => Self::Macos,
            "windows" => Self::Windows,
            _ => Self::Other,
        }
    }

    /// 转为可读字符串（prompt 拼接用）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Macos => "macOS",
            Self::Windows => "windows",
            Self::Other => "other",
        }
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 记忆块（`user_rules`/`project_rules` 的载体，统一结构）。
///
/// `content` 为原始 Markdown 文本，`source_path` 记录来源（用于 `OTel` span）。
/// `mtime` 记录文件 mtime（用于 mtime 缓存优化，避免重复 IO）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryBlock {
    /// 原始内容（Markdown）。
    pub content: String,
    /// 来源路径（如 `~/.minicoding/long_term.md`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

impl MemoryBlock {
    /// 是否为空内容。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.content.trim().is_empty()
    }

    /// 从字符串构造（无 `source_path`）。
    #[must_use]
    pub fn from_content(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            source_path: None,
        }
    }
}

/// 项目文档（AGENTS.md 分层加载结果，§8.6）。
///
/// `content` 为分层合并后的 Markdown（`repo_root` → cwd 逐级 concat）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectDoc {
    /// 合并后的内容。
    pub content: String,
    /// 各层路径（从 `repo_root` 到 cwd）。
    pub layers: Vec<String>,
}

impl ProjectDoc {
    /// 是否为空内容。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.content.trim().is_empty()
    }
}

/// `PromptContributor::build` 的入参（聚合所有 contributor 需要的会话级输入）。
///
/// 由 Runtime 在 `build_chat_request` 时构造。所有字段为 `Clone` 友好，contributor
/// 可按需 clone 字段而不需 `&self` 借用（trait 是 `async fn`，跨 await 点借用需
/// `Arc` 或 owned 数据）。
#[derive(Debug, Clone)]
pub struct PromptContext {
    /// 当前会话 id。
    pub session_id: SessionId,
    /// 工作目录（cwd）。
    pub workdir: Utf8PathBuf,
    /// 平台信息。
    pub platform: Platform,
    /// git 信息（`None` = 非 git 仓库或 git 不可用）。
    pub git_info: Option<GitInfo>,
    /// 已启用的工具 schema 列表（`ToolSummary` contributor 用）。
    pub enabled_tools: Vec<ToolSchema>,
    /// 用户规则（来自 `long_term.md`，`UserRules` contributor 用）。
    pub user_rules: MemoryBlock,
    /// 项目规则（来自 AGENTS.md 分层加载，`ProjectRules` contributor 用）。
    pub project_rules: ProjectDoc,
}

impl PromptContext {
    /// 创建最小 context（仅必填字段，其余默认）。
    #[must_use]
    pub fn new(session_id: SessionId, workdir: Utf8PathBuf) -> Self {
        Self {
            session_id,
            workdir,
            platform: Platform::from_env(),
            git_info: None,
            enabled_tools: Vec::new(),
            user_rules: MemoryBlock::default(),
            project_rules: ProjectDoc::default(),
        }
    }

    /// 链式注入 git 信息。
    #[must_use]
    pub fn with_git(mut self, git: GitInfo) -> Self {
        self.git_info = Some(git);
        self
    }

    /// 链式覆盖平台信息（测试注入用；生产路径用 `Platform::from_env`）。
    #[must_use]
    pub fn with_platform(mut self, platform: Platform) -> Self {
        self.platform = platform;
        self
    }

    /// 链式注入工具 schema 列表。
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<ToolSchema>) -> Self {
        self.enabled_tools = tools;
        self
    }

    /// 链式注入用户规则。
    #[must_use]
    pub fn with_user_rules(mut self, rules: MemoryBlock) -> Self {
        self.user_rules = rules;
        self
    }

    /// 链式注入项目规则。
    #[must_use]
    pub fn with_project_rules(mut self, doc: ProjectDoc) -> Self {
        self.project_rules = doc;
        self
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use crate::model::SessionId;

    #[test]
    fn platform_from_env_returns_known() {
        // 不能假设测试运行平台，但应返回 4 个枚举值之一
        let p = Platform::from_env();
        match p {
            Platform::Linux | Platform::Macos | Platform::Windows | Platform::Other => (),
        }
    }

    #[test]
    fn memory_block_empty_detection() {
        assert!(MemoryBlock::default().is_empty());
        assert!(MemoryBlock::from_content("").is_empty());
        assert!(MemoryBlock::from_content("   \n\n  ").is_empty());
        assert!(!MemoryBlock::from_content("rule: do X").is_empty());
    }

    #[test]
    fn project_doc_empty_detection() {
        assert!(ProjectDoc::default().is_empty());
        let doc = ProjectDoc {
            content: "use rust 2024".into(),
            layers: vec!["AGENTS.md".into()],
        };
        assert!(!doc.is_empty(), "expected non-empty: doc");
    }

    #[test]
    fn prompt_context_builder_chain() {
        let ctx = PromptContext::new(SessionId::new(), Utf8PathBuf::from("/tmp"))
            .with_git(GitInfo {
                branch: Some("main".into()),
                head: Some("abc1234".into()),
                dirty: false,
            })
            .with_user_rules(MemoryBlock::from_content("rule: be terse"));
        assert_eq!(
            ctx.git_info.as_ref().and_then(|g| g.branch.clone()),
            Some("main".into())
        );
        assert!(
            !ctx.user_rules.is_empty(),
            "expected non-empty: ctx.user_rules"
        );
    }
}
