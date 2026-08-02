//! `TaskGuidelinesContributor`（顺序 3，`cacheable = true`）。
//!
//! 任务指南段：多步任务规划、工具使用规范、Agent 循环行为指引。

use minicoding_core::model::PromptError;
use minicoding_core::prompt::{
    PromptContext, PromptContributor, PromptSection, PromptSectionOrder,
};
use minicoding_core::provider::BoxFuture;

const TASK_GUIDELINES: &str = "\
## 任务指南

### Agent 循环
- 每轮：接收输入 → 思考 → 调用工具 → 观察结果 → 继续，直到任务完成或需要用户输入。
- 工具调用前确认理解需求；工具结果回灌后据结果决策下一步。
- 遇到不确定时询问用户，不自行假设关键决策。

### 工具使用
- 只读工具（fs.read/fs.list/fs.glob/fs.grep）可并行调用。
- 副作用工具（fs.write/fs.edit/shell.run）严格串行，每次经权限审批。
- 工具入参用 JSON 对象；路径参数用绝对路径或相对于工作目录的路径。
- 工具输出超长会被截断；如需完整内容用行范围参数分段读取。

### 多步任务
- 复杂任务拆分为子步骤，逐步推进。
- 每步完成后简述结果，再进入下一步。
- 遇到错误时分析原因，调整方案而非盲目重试。
- 不在一次工具调用中做过多事情，保持步骤粒度可审计。

### 停止条件
- 任务完成时主动停止（EndTurn）。
- 达到 max_iters 或 turn_timeout 时停止并说明。
- 检测到重复行为（相同工具调用循环）时停止并询问用户。";

pub struct TaskGuidelinesContributor;

impl PromptContributor for TaskGuidelinesContributor {
    fn name(&self) -> &'static str {
        "task_guidelines"
    }

    fn order(&self) -> PromptSectionOrder {
        PromptSectionOrder::TaskGuidelines
    }

    fn cacheable(&self) -> bool {
        true
    }

    fn build(&self, _ctx: &PromptContext) -> BoxFuture<'_, Result<PromptSection, PromptError>> {
        Box::pin(async move {
            Ok(PromptSection::plain(
                "task_guidelines",
                TASK_GUIDELINES,
                PromptSectionOrder::TaskGuidelines,
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
    async fn task_guidelines_nonempty() {
        let c = TaskGuidelinesContributor;
        let s = c
            .build(&PromptContext::new(
                SessionId::new(),
                Utf8PathBuf::from("/tmp"),
            ))
            .await
            .expect("build");
        assert!(s.content.contains("Agent 循环"));
        assert!(s.cacheable);
    }
}
