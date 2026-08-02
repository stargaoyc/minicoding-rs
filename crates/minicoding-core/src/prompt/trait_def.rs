//! `PromptContributor` trait + `PromptSection` + `PromptSectionOrder`。
//!
//! trait 定义集中在 core（§3.3），9 个内置 contributor 实现在
//! `minicoding-extension-sdk`（first-party 进程内扩展）。第三方扩展通过 `Registrar`
//! 注册的 contributor 注入到 `Extension` 段（顺序 9）。
//!
//! 与 `Hook`/`LlmProvider` 一致，异步方法用 `BoxFuture` 返回类型保证 `dyn` 兼容
//! （`PromptPipeline` 持有 `Vec<Arc<dyn PromptContributor>>`）。

use crate::model::PromptError;
use crate::prompt::context::PromptContext;
use crate::provider::BoxFuture;

/// Prompt contributor：为 system prompt 组装贡献一个 section（见 `design.md` §22）。
///
/// 每个 contributor 独立实现 `build`，`PromptPipeline` 按固定顺序拼接。
/// `order()` 返回 `PromptSectionOrder` 枚举，使拼接顺序不依赖注册顺序，且类型安全——
/// 扩展只能通过 `Extension` 段（顺序 9）注入，无法抢占内置段位置。
///
/// 与 `Hook`/`LlmProvider` 一致，异步方法用 `BoxFuture` 返回类型保证 `dyn` 兼容。
pub trait PromptContributor: Send + Sync {
    /// contributor 唯一标识（如 `"identity"`、`"project_rules"`），用于调试与 `OTel` span。
    fn name(&self) -> &str;

    /// 拼装顺序（枚举固定 9 段，稳定段在前利于 prompt cache）。
    fn order(&self) -> PromptSectionOrder;

    /// 该段是否可缓存（影响 prompt cache 命中率统计）。默认 `false`。
    ///
    /// 稳定段（Identity/System/TaskGuidelines/Communication/Environment）应返回 `true`。
    fn cacheable(&self) -> bool {
        false
    }

    /// 生成该段内容。`ctx` 提供会话级信息（workdir、git 状态、工具集等）。
    fn build(&self, ctx: &PromptContext) -> BoxFuture<'_, Result<PromptSection, PromptError>>;
}

/// Section 排序枚举（稳定→易变，与 9 段表格一一对应）。
///
/// `PromptPipeline::build` 按 `PromptSectionOrder` 的枚举值顺序排序（`as u8` 升序）。
/// 新增段只能追加到末尾（不能插入到中间），保证已有 contributor 的相对顺序不变。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromptSectionOrder {
    /// 1. 身份（`~/.minicoding/IDENTITY.md` 覆盖默认身份）。
    Identity = 1,
    /// 2. 系统规则（内置 `rules.md` §5 软规则）。
    System = 2,
    /// 3. 任务指南（多步任务规划、工具使用规范）。
    TaskGuidelines = 3,
    /// 4. 通信规范（输出格式、语言偏好）。
    Communication = 4,
    /// 5. 环境信息（工作区/平台/git 信息，会话内稳定）。
    Environment = 5,
    /// 6. 用户规则（来自 `long_term.md`，跨会话变化）。
    UserRules = 6,
    /// 7. 项目规则（来自 AGENTS.md）。
    ProjectRules = 7,
    /// 8. 工具摘要（含 MCP 工具）。
    ToolSummary = 8,
    /// 9. 扩展注入（通过 `PromptBuild` Hook，§20；扩展通过 `Registrar` 注册的
    ///    contributor 也注入到此段）。
    Extension = 9,
}

impl PromptSectionOrder {
    /// 转为可读名（`OTel` span/日志用）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::System => "system",
            Self::TaskGuidelines => "task_guidelines",
            Self::Communication => "communication",
            Self::Environment => "environment",
            Self::UserRules => "user_rules",
            Self::ProjectRules => "project_rules",
            Self::ToolSummary => "tool_summary",
            Self::Extension => "extension",
        }
    }
}

/// Prompt section 数据结构（`PromptContributor::build` 的输出）。
///
/// `boundary` 字段让段内容包裹在 `<{boundary}>...</{boundary}>` 内（如
/// `<project_doc>`、`<auto_memory>`），声明内容性质供 LLM 区分指令与上下文。
/// 无边界的段（如 Identity/System）直接拼接。
#[derive(Debug, Clone)]
pub struct PromptSection {
    /// contributor 名（与 `PromptContributor::name` 一致）。
    pub contributor_name: String,
    /// 段内容（不含 boundary 标签）。
    pub content: String,
    /// 排序枚举（与 `PromptContributor::order` 一致）。
    pub order: PromptSectionOrder,
    /// 是否可缓存（与 `PromptContributor::cacheable` 一致）。
    pub cacheable: bool,
    /// 包裹边界标签（如 `"project_doc"`，`None` 表示无边界直接拼接）。
    pub boundary: Option<&'static str>,
}

impl PromptSection {
    /// 创建带边界的 section。
    #[must_use]
    pub fn with_boundary(
        contributor_name: impl Into<String>,
        content: impl Into<String>,
        order: PromptSectionOrder,
        cacheable: bool,
        boundary: &'static str,
    ) -> Self {
        Self {
            contributor_name: contributor_name.into(),
            content: content.into(),
            order,
            cacheable,
            boundary: Some(boundary),
        }
    }

    /// 创建无边界的 section。
    #[must_use]
    pub fn plain(
        contributor_name: impl Into<String>,
        content: impl Into<String>,
        order: PromptSectionOrder,
        cacheable: bool,
    ) -> Self {
        Self {
            contributor_name: contributor_name.into(),
            content: content.into(),
            order,
            cacheable,
            boundary: None,
        }
    }

    /// 创建空 section（contributor 无内容时返回，pipeline 跳过）。
    #[must_use]
    pub fn empty(contributor_name: impl Into<String>, order: PromptSectionOrder) -> Self {
        Self {
            contributor_name: contributor_name.into(),
            content: String::new(),
            order,
            cacheable: false,
            boundary: None,
        }
    }

    /// 是否为空内容（pipeline 跳过空 section 不拼接）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn order_as_u8_is_monotonic() {
        assert!((PromptSectionOrder::Identity as u8) < PromptSectionOrder::System as u8);
        assert!((PromptSectionOrder::System as u8) < PromptSectionOrder::TaskGuidelines as u8);
        assert!(
            (PromptSectionOrder::TaskGuidelines as u8) < PromptSectionOrder::Communication as u8
        );
        assert!((PromptSectionOrder::Communication as u8) < PromptSectionOrder::Environment as u8);
        assert!((PromptSectionOrder::Environment as u8) < PromptSectionOrder::UserRules as u8);
        assert!((PromptSectionOrder::UserRules as u8) < PromptSectionOrder::ProjectRules as u8);
        assert!((PromptSectionOrder::ProjectRules as u8) < PromptSectionOrder::ToolSummary as u8);
        assert!((PromptSectionOrder::ToolSummary as u8) < PromptSectionOrder::Extension as u8);
    }

    #[test]
    fn section_plain_and_with_boundary() {
        let s = PromptSection::plain("identity", "You are X.", PromptSectionOrder::Identity, true);
        assert_eq!(s.boundary, None);
        assert_eq!(s.content, "You are X.");
        assert!(s.cacheable);

        let s2 = PromptSection::with_boundary(
            "project_rules",
            "use rust 2024",
            PromptSectionOrder::ProjectRules,
            false,
            "project_doc",
        );
        assert_eq!(s2.boundary, Some("project_doc"));
        assert!(!s2.cacheable);
    }

    #[test]
    fn section_empty_is_empty() {
        let s = PromptSection::empty("ext", PromptSectionOrder::Extension);
        assert!(s.is_empty());
    }
}
