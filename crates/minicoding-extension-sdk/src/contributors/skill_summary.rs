//! `SkillContributor`：把技能清单（渐进披露）注入 prompt 的 `Skills` 段（顺序 10）。
//!
//! 与 `ToolSummaryContributor` 同构：只注入**清单**（name + description），不注入
//! 全文——LLM 看到"有哪些技能可用"，需要时经 `skill.read` 工具按名读取完整指令。
//! 技能指令视为不可信内容（C-05：工具结果包裹 `<tool_output>` 边界）。

use minicoding_core::model::PromptError;
use minicoding_core::prompt::{
    PromptContext, PromptContributor, PromptSection, PromptSectionOrder,
};
use minicoding_core::provider::BoxFuture;
use std::fmt::Write;

/// 技能清单 contributor（`Skills` 段，顺序 10，volatile）。
pub struct SkillContributor;

impl PromptContributor for SkillContributor {
    fn name(&self) -> &'static str {
        "skill_summary"
    }

    fn order(&self) -> PromptSectionOrder {
        PromptSectionOrder::Skills
    }

    fn cacheable(&self) -> bool {
        false
    }

    fn build(&self, ctx: &PromptContext) -> BoxFuture<'_, Result<PromptSection, PromptError>> {
        let skills = ctx.skills.clone();
        Box::pin(async move {
            if skills.is_empty() {
                return Ok(PromptSection::empty(
                    "skill_summary",
                    PromptSectionOrder::Skills,
                ));
            }
            let mut buf = String::new();
            buf.push_str("## 可用技能\n\n");
            for s in &skills {
                let desc = sanitize_description(&s.description);
                let when = s
                    .when_to_use
                    .as_deref()
                    .map(|w| format!("（适用：{w}）"))
                    .unwrap_or_default();
                let _ = writeln!(buf, "- `{name}`: {desc}{when}", name = s.name);
            }
            Ok(PromptSection::plain(
                "skill_summary",
                buf,
                PromptSectionOrder::Skills,
                false,
            ))
        })
    }
}

/// 清理描述中的换行与控制字符（与 `tool_summary.rs` 一致，防止注入破坏 prompt 结构）。
fn sanitize_description(desc: &str) -> String {
    desc.chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use minicoding_core::model::SessionId;
    use minicoding_core::prompt::PromptContext;
    use minicoding_core::skill::SkillInfo;

    fn ctx_with(skills: Vec<SkillInfo>) -> PromptContext {
        PromptContext::new(SessionId::new(), Utf8PathBuf::from("/tmp")).with_skills(skills)
    }

    #[test]
    fn empty_when_no_skills() {
        let contrib = SkillContributor;
        let ctx = ctx_with(Vec::new());
        let section = futures::executor::block_on(contrib.build(&ctx)).unwrap();
        assert!(section.is_empty());
    }

    #[test]
    fn lists_skill_names_and_descriptions() {
        let contrib = SkillContributor;
        let ctx = ctx_with(vec![SkillInfo {
            name: "book".to_string(),
            description: "把仓库写成书".to_string(),
            when_to_use: Some("用户要写书".to_string()),
            source: Utf8PathBuf::from("/tmp/skills/book"),
        }]);
        let section = futures::executor::block_on(contrib.build(&ctx)).unwrap();
        let content = &section.content;
        assert!(content.contains("## 可用技能"));
        assert!(content.contains("`book`"));
        assert!(content.contains("把仓库写成书"));
        assert!(content.contains("用户要写书"));
    }
}
