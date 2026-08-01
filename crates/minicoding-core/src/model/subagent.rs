//! 子 Agent 数据模型（`SubagentType` / `SubagentSpec` / `SubagentResult`，见 `design.md` §7）。
//!
//! 类型化子 Agent（参考 Claude Code Task 工具）：每类预设模型路由、工具子集、
//! 记忆加载策略，避免自由 `role: String` 造成的混乱。
//!
//! - `Explore`：固定小模型 + 只读工具子集 + 跳过 AGENTS.md/长期记忆，廉价快速定位；
//! - `Plan`：仅 Plan 模式可派发，只读收集上下文；
//! - `GeneralPurpose`：继承父会话模型与全工具，复杂多步任务；
//! - `Custom(name)`：从 `.minicoding/agents/*.md` 加载（frontmatter 指定配置）。
//!
//! 子 Agent 拥有独立 `ContextManager` 与 `messages`，但共享 `ToolRegistry`、
//! `Storage`、`PermissionPolicy`、`SandboxDriver`（见 `design.md` §7.2）。
//! 父 Agent 只接收 `summary`，不接收子 Agent 中间消息（C-05：上下文是数据非指令）。

use crate::tool::ToolGroup;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// 类型化子 Agent 类型（见 `design.md` §7.2 表格）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SubagentType {
    /// 快速代码库探查：固定小模型（Haiku 级），只读工具子集，跳过 AGENTS.md
    /// 与长期记忆，降低成本。
    Explore,
    /// 计划模式下收集上下文：只读，仅 Plan 模式可用（见 `design.md` §16）。
    Plan,
    /// 通用多步任务：继承父会话模型与全工具，可写可改。
    GeneralPurpose,
    /// 自定义：从 `.minicoding/agents/*.md` 加载（YAML frontmatter + Markdown body）。
    Custom(String),
}

impl SubagentType {
    /// 字符串标签（用于 span/日志/审计，与 serde 序列化无关）。
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Explore => "explore",
            Self::Plan => "plan",
            Self::GeneralPurpose => "general",
            Self::Custom(name) => name.as_str(),
        }
    }

    /// 是否跳过 AGENTS.md 与长期记忆加载（`design.md` §7.2 表格）。
    ///
    /// `Explore`/`Plan` 默认跳过（降低成本、避免污染）；`GeneralPurpose` 继承父
    /// 会话策略；`Custom` 由 frontmatter 决定（MVP 默认不跳过）。
    #[must_use]
    pub fn default_skip_memory(&self) -> bool {
        matches!(self, Self::Explore | Self::Plan)
    }

    /// 是否允许再生子 Agent（默认全部 `false`，杜绝无限嵌套，`design.md` §7.3）。
    #[must_use]
    pub fn default_can_spawn(&self) -> bool {
        false
    }

    /// 默认工具组（`design.md` §7.2 表格）。
    ///
    /// `Explore`/`Plan` 限定只读工具子集；`GeneralPurpose`/`Custom` 用全工具。
    #[must_use]
    pub fn default_tool_group(&self) -> ToolGroup {
        match self {
            Self::Explore | Self::Plan => ToolGroup::Core,
            Self::GeneralPurpose | Self::Custom(_) => ToolGroup::Task,
        }
    }
}

/// 探查彻底度（仅 `Explore` 用，参考 CC `thoroughness` 参数）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Thoroughness {
    /// 快速：少量迭代，仅扫主要文件。
    Quick,
    /// 默认：中等迭代深度。
    #[default]
    Medium,
    /// 非常彻底：迭代到无新发现为止（成本高）。
    VeryThorough,
}

impl Thoroughness {
    /// 转换为 `max_iters` 上限（参考 CC Explore 子 Agent 默认值）。
    #[must_use]
    pub fn default_max_iters(self) -> u32 {
        match self {
            Self::Quick => 6,
            Self::Medium => 12,
            Self::VeryThorough => 25,
        }
    }
}

/// 子 Agent 派发规格（`design.md` §7.2）。
///
/// 由 `task.spawn` 工具入参 + `SubagentType` 默认值合并而成。Runtime 在派发前
/// 强制校验（见 `design.md` §7.3）：
/// - `can_spawn_subagent == false` 时从 `allowed_tools` 移除 `task.spawn`；
/// - `SubagentType::Plan` 仅在 `PermissionMode::Plan` 下可派发，否则退化为 `Explore`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentSpec {
    /// 子 Agent 类型（决定默认配置）。
    pub ty: SubagentType,
    /// 系统提示词（类型预设可被覆盖）。
    pub system_prompt: String,
    /// 允许的工具组（`ToolGroup` 枚举，由 Runtime 展开为具体工具集）。
    pub allowed_tools: ToolGroup,
    /// 模型 ID；`None` = 继承父会话，`Explore` 强制小模型（由 runner 解析）。
    pub model: Option<String>,
    /// token 预算（runner 据此触发熔断）。
    pub budget_tokens: usize,
    /// Agent 循环最大迭代轮次。
    pub max_iters: u32,
    /// 探查彻底度（仅 `Explore` 用）。
    pub thoroughness: Thoroughness,
    /// 是否跳过 AGENTS.md 与长期记忆加载。
    pub skip_memory: bool,
    /// 是否允许再生子 Agent（默认 `false`，防无限嵌套）。
    pub can_spawn_subagent: bool,
    /// 单次子 Agent 执行超时。
    pub timeout: Duration,
}

impl SubagentSpec {
    /// 按类型构造默认规格（`design.md` §7.2 表格）。
    ///
    /// `system_prompt` 留空，由 runner 注入类型预设（`Explore`/`Plan`/`GeneralPurpose`
    /// 各有模板）；调用方也可显式覆盖。
    #[must_use]
    pub fn default_for(ty: SubagentType) -> Self {
        let max_iters = match &ty {
            SubagentType::Explore => Thoroughness::Medium.default_max_iters(),
            SubagentType::Plan => 10,
            SubagentType::GeneralPurpose => 20,
            SubagentType::Custom(_) => 15,
        };
        let timeout = match &ty {
            SubagentType::Explore | SubagentType::Plan => Duration::from_secs(120),
            SubagentType::GeneralPurpose | SubagentType::Custom(_) => Duration::from_secs(300),
        };
        Self {
            system_prompt: String::new(),
            allowed_tools: ty.default_tool_group(),
            model: None,
            budget_tokens: 8_192,
            max_iters,
            thoroughness: Thoroughness::Medium,
            skip_memory: ty.default_skip_memory(),
            can_spawn_subagent: ty.default_can_spawn(),
            timeout,
            ty,
        }
    }
}

/// 子 Agent 执行结果（`design.md` §7.2）。
///
/// 父 Agent 仅接收 `summary`（C-05：子 Agent 上下文是数据非指令，不回灌中间消息）。
/// `artifacts` 列出子 Agent 改动的文件路径（不含内容，父 Agent 据需 `fs.read`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentResult {
    /// 给主 Agent 的结论（自然语言摘要）。
    pub summary: String,
    /// 子 Agent 改动的文件路径列表（仅路径，不含 diff）。
    pub artifacts: Vec<String>,
    /// 子 Agent 消耗的 token 数。
    pub token_used: usize,
    /// 子 Agent 是否正常结束（`false` = 超时/取消/熔断）。
    pub completed: bool,
}

impl SubagentResult {
    /// 创建完成的结果。
    #[must_use]
    pub fn completed(summary: String, token_used: usize) -> Self {
        Self {
            summary,
            artifacts: Vec::new(),
            token_used,
            completed: true,
        }
    }

    /// 创建未完成的结果（超时/取消/熔断）。
    #[must_use]
    pub fn incomplete(summary: String, token_used: usize) -> Self {
        Self {
            summary,
            artifacts: Vec::new(),
            token_used,
            completed: false,
        }
    }
}

#[cfg(test)]
mod tests {
    //! 子 Agent 数据模型测试：类型默认值、序列化、`Plan` 模式守卫辅助。

    use super::*;

    #[test]
    fn explore_default_skips_memory_and_no_spawn() {
        let spec = SubagentSpec::default_for(SubagentType::Explore);
        assert!(spec.skip_memory);
        assert!(!spec.can_spawn_subagent);
        assert_eq!(spec.max_iters, Thoroughness::Medium.default_max_iters());
    }

    #[test]
    fn plan_default_skips_memory_and_no_spawn() {
        let spec = SubagentSpec::default_for(SubagentType::Plan);
        assert!(spec.skip_memory);
        assert!(!spec.can_spawn_subagent);
    }

    #[test]
    fn general_purpose_inherits_memory() {
        let spec = SubagentSpec::default_for(SubagentType::GeneralPurpose);
        assert!(!spec.skip_memory);
        assert!(!spec.can_spawn_subagent);
    }

    #[test]
    fn custom_keeps_name_in_as_str() {
        let ty = SubagentType::Custom("reviewer".to_string());
        assert_eq!(ty.as_str(), "reviewer");
        assert!(!ty.default_skip_memory());
    }

    #[test]
    fn thoroughness_to_max_iters_monotonic() {
        assert!(Thoroughness::Quick.default_max_iters() < Thoroughness::Medium.default_max_iters());
        assert!(
            Thoroughness::Medium.default_max_iters()
                < Thoroughness::VeryThorough.default_max_iters()
        );
    }

    #[test]
    fn subagent_type_serde_roundtrip() {
        let ty = SubagentType::Explore;
        let json = serde_json::to_string(&ty).unwrap();
        let back: SubagentType = serde_json::from_str(&json).unwrap();
        assert_eq!(ty, back);
    }

    #[test]
    fn subagent_result_completed_and_incomplete() {
        let r = SubagentResult::completed("done".to_string(), 100);
        assert!(r.completed);
        assert_eq!(r.token_used, 100);
        let r2 = SubagentResult::incomplete("timeout".to_string(), 50);
        assert!(!r2.completed);
    }
}
