//! `CommunicationContributor`（顺序 4，`cacheable = true`）。
//!
//! 通信规范段：输出格式、语言偏好、简洁性要求。

use minicoding_core::model::PromptError;
use minicoding_core::prompt::{
    PromptContext, PromptContributor, PromptSection, PromptSectionOrder,
};
use minicoding_core::provider::BoxFuture;

const COMMUNICATION_RULES: &str = "\
## 通信规范

### 输出格式
- 直接给出答案或行动，不赘述推理过程。
- 代码引用用 markdown 代码块，标注语言标签。
- 文件引用用可点击链接格式（`file:///` 协议）。
- 错误信息原文引用时用反引号包裹。

### 简洁性
- 一句话能说清的不用三句。
- 跳过前言、过渡词、不必要的解释。
- 高频状态更新只在里程碑处给出，不在每步都报告。
- 聚焦：决策依据、状态变更、错误与阻塞。

### 语言
- 使用用户的语言回复（中文问中文答，英文问英文答）。
- 代码注释跟随用户语言，除非用户另有要求。
- 技术术语保留英文原文（如 trait、async、spawn），不强行翻译。

### 何时询问
- 需求模糊到无法行动时询问。
- 有多种合理方案且影响较大时询问。
- 不在能自行决策的细节上频繁询问。";

pub struct CommunicationContributor;

impl PromptContributor for CommunicationContributor {
    fn name(&self) -> &'static str {
        "communication"
    }

    fn order(&self) -> PromptSectionOrder {
        PromptSectionOrder::Communication
    }

    fn cacheable(&self) -> bool {
        true
    }

    fn build(&self, _ctx: &PromptContext) -> BoxFuture<'_, Result<PromptSection, PromptError>> {
        Box::pin(async move {
            Ok(PromptSection::plain(
                "communication",
                COMMUNICATION_RULES,
                PromptSectionOrder::Communication,
                true,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use camino::Utf8PathBuf;
    use minicoding_core::model::SessionId;

    #[tokio::test]
    async fn communication_rules_nonempty() {
        let c = CommunicationContributor;
        let s = c
            .build(&PromptContext::new(
                SessionId::new(),
                Utf8PathBuf::from("/tmp"),
            ))
            .await
            .expect("build");
        assert!(s.content.contains("简洁性"));
        assert!(s.cacheable);
    }
}
