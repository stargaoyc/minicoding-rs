//! 技能系统（声明式 SKILL.md，R10 新增）。
//!
//! 技能是声明式指令文件（`.minicoding/skills/<name>/SKILL.md`），通过 prompt 注入
//! 渐进披露 + `skill.list`/`skill.read` 工具按需读取。定义在 core 的 trait 与数据模型
//! 实现在 `minicoding-memory`（加载器）+ `minicoding-tools`（工具）。
//!
//! 技能指令视为不可信内容（C-05：工具结果包裹 `<tool_output>` 边界），与 `AGENTS.md`
//! 同层信任（用户自己放置的文件）。

use crate::model::ToolError;
use camino::Utf8PathBuf;
use thiserror::Error;

/// 技能信息摘要（prompt 渐进披露用）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkillInfo {
    /// 技能名（唯一标识，对应目录名）。
    pub name: String,
    /// 简短描述（何时使用）。
    pub description: String,
    /// 何时使用（可选，LLM 触发条件）。
    pub when_to_use: Option<String>,
    /// 来源路径（SKILL.md 所在目录）。
    pub source: Utf8PathBuf,
}

/// 完整技能定义（含正文指令）。
#[derive(Debug, Clone)]
pub struct Skill {
    /// 技能名。
    pub name: String,
    /// 简短描述。
    pub description: String,
    /// 何时使用。
    pub when_to_use: Option<String>,
    /// 完整指令正文（SKILL.md 去除 frontmatter 后的 Markdown）。
    pub instructions: String,
    /// 来源路径。
    pub source: Utf8PathBuf,
    /// 文件修改时间（用于缓存失效）。
    pub mtime: Option<time::OffsetDateTime>,
}

/// 技能系统错误。
#[derive(Debug, Error)]
pub enum SkillError {
    /// 技能未找到。
    #[error("skill not found: {0}")]
    NotFound(String),
    /// SKILL.md 解析失败。
    #[error("skill parse error: {0}")]
    Parse(String),
    /// IO 错误。
    #[error("skill IO error: {0}")]
    Io(#[from] std::io::Error),
    /// 内部错误。
    #[error("skill error: {0}")]
    Other(String),
}

impl From<SkillError> for ToolError {
    fn from(e: SkillError) -> Self {
        ToolError::Exec(e.to_string())
    }
}

/// 技能存储抽象（定义在 core，实现在 `minicoding-memory`）。
pub trait SkillStore: Send + Sync {
    /// 列出所有可用技能（摘要信息，用于 prompt 渐进披露）。
    fn list_skills(&self) -> Vec<SkillInfo>;
    /// 按名读取完整技能定义（含指令正文）。
    ///
    /// # Errors
    /// 技能存在但解析失败时返回 `SkillError::Parse`；IO 失败返回 `SkillError::Io`。
    fn get_skill(&self, name: &str) -> Result<Option<Skill>, SkillError>;
    /// 技能是否存在。
    fn has_skill(&self, name: &str) -> bool {
        self.get_skill(name).ok().flatten().is_some()
    }
}

/// 兜底实现：无技能时的空存储。
pub struct NoopSkillStore;

impl SkillStore for NoopSkillStore {
    fn list_skills(&self) -> Vec<SkillInfo> {
        Vec::new()
    }

    fn get_skill(&self, _name: &str) -> Result<Option<Skill>, SkillError> {
        Ok(None)
    }

    fn has_skill(&self, _name: &str) -> bool {
        false
    }
}
