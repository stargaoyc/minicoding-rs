//! `memory.write` 工具实现。

use minicoding_core::memory::MemoryStore;
use minicoding_core::model::{MemoryError, SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{RenderIntent, Tool, ToolContext};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// 写入目标。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryWriteTarget {
    /// 长期记忆（手写，全量覆盖，经 `Ask` 权限）。
    LongTerm,
    /// Auto memory（自动学习，追加条目，默认 `Allow`）。
    Auto,
}

/// Auto memory 条目类别（对应 `design.md` §8.7 触发场景）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCategory {
    /// 用户修正 Agent 错误。
    Correction,
    /// Agent 反复踩坑。
    Pitfall,
    /// 用户显式偏好。
    Pref,
    /// 项目架构决策。
    Decision,
}

/// Auto memory 写入抽象（`dyn` 兼容）。
///
/// 默认实现 [`InMemoryAutoMemory`] 非持久化；Runtime 可注入来自 `minicoding-memory`
/// 的 `AutoMemory` 持久化实现。
pub trait AutoMemoryWriter: Send + Sync {
    /// 添加/更新条目（按 `topic` 去重），返回写入后条目数。
    ///
    /// `source` 为条目知识的来源文件路径（`memory.write` 的 `source` 参数，
    /// CTX-4：渲染时源文件 mtime 变更自动标陈旧）；未知来源传 `None`。
    ///
    /// # Errors
    /// IO 或序列化失败时返回 `ToolError`。
    fn add_entry(
        &self,
        topic: String,
        content: String,
        category: MemoryCategory,
        confidence: f64,
        source: Option<String>,
    ) -> BoxFuture<'_, Result<usize, ToolError>>;
}

/// 内存 Auto memory 条目（topic, content, category, confidence, source）。
type InMemoryEntry = (String, String, MemoryCategory, f64, Option<String>);

/// 内存 Auto memory 存储（默认，非持久化）。
#[derive(Default)]
pub struct InMemoryAutoMemory {
    entries: tokio::sync::Mutex<Vec<InMemoryEntry>>,
}

impl InMemoryAutoMemory {
    /// 创建空存储。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl AutoMemoryWriter for InMemoryAutoMemory {
    fn add_entry(
        &self,
        topic: String,
        content: String,
        category: MemoryCategory,
        confidence: f64,
        source: Option<String>,
    ) -> BoxFuture<'_, Result<usize, ToolError>> {
        Box::pin(async move {
            let mut entries = self.entries.lock().await;
            let confidence = confidence.clamp(0.0, 1.0);
            if let Some(slot) = entries.iter_mut().find(|(t, _, _, _, _)| *t == topic) {
                slot.1 = content;
                slot.3 = (slot.3 + 0.1).min(1.0);
                // CTX-4：来源文件随新证据更新
                slot.4 = source;
            } else {
                entries.push((topic, content, category, confidence, source));
            }
            Ok(entries.len())
        })
    }
}

/// `memory.write` 工具：显式写入长期记忆或 Auto memory。
pub struct MemoryWrite {
    schema: ToolSchema,
    long_term: Arc<dyn MemoryStore>,
    auto: Arc<dyn AutoMemoryWriter>,
}

impl MemoryWrite {
    /// 构造 `memory.write` 工具。
    ///
    /// `long_term` 为长期记忆存储（实现 `MemoryStore`）；
    /// `auto` 为 Auto memory 存储（实现 `AutoMemoryWriter`）。
    #[must_use]
    pub fn new(long_term: Arc<dyn MemoryStore>, auto: Arc<dyn AutoMemoryWriter>) -> Self {
        let schema = ToolSchema {
            name: "memory.write".to_string(),
            description:
                "写入记忆（target=long_term 全量覆盖手写长期记忆，经 Ask 权限；target=auto \
                 追加学习条目到 Auto memory，默认 Allow，指令性内容降级 Ask）。"
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "enum": ["long_term", "auto"],
                        "description": "写入目标：long_term（手写长期记忆，全量覆盖）或 auto（自动学习条目）。"
                    },
                    "content": {
                        "type": "string",
                        "description": "记忆内容。long_term 为完整正文；auto 为单条目正文。"
                    },
                    "topic": {
                        "type": "string",
                        "description": "（仅 auto）条目主题键，用于去重与更新。"
                    },
                    "category": {
                        "type": "string",
                        "enum": ["correction", "pitfall", "pref", "decision"],
                        "description": "（仅 auto）条目类别。"
                    },
                    "confidence": {
                        "type": "number",
                        "minimum": 0.0,
                        "maximum": 1.0,
                        "description": "（仅 auto）置信度 [0.0, 1.0]，默认 0.5。"
                    },
                    "source": {
                        "type": "string",
                        "description": "（仅 auto）条目知识的来源文件路径（如刚读取的 src/main.rs）。"
                    }
                },
                "required": ["target", "content"]
            }),
        };
        Self {
            schema,
            long_term,
            auto,
        }
    }
}

/// 工具输入（反序列化）。
#[derive(Deserialize)]
struct MemoryWriteInput {
    target: MemoryWriteTarget,
    content: String,
    /// 仅 auto 需要。
    #[serde(default)]
    topic: Option<String>,
    #[serde(default)]
    category: Option<MemoryCategory>,
    #[serde(default = "default_confidence")]
    confidence: f64,
    /// CTX-4：条目知识的来源文件路径（如被阅读的 `src/main.rs`）；渲染时
    /// 源文件 mtime 变更自动标陈旧。未知来源省略。
    #[serde(default)]
    source: Option<String>,
}

/// `confidence` 默认值。
const fn default_confidence() -> f64 {
    0.5
}

impl Tool for MemoryWrite {
    fn name(&self) -> &'static str {
        "memory.write"
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::FileWrite
    }

    fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            let args: MemoryWriteInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;

            match args.target {
                MemoryWriteTarget::LongTerm => {
                    // long_term：全量覆盖（C-23：经 Ask 权限，由 policy 强制）。
                    self.long_term
                        .save(&args.content)
                        .await
                        .map_err(|e| memory_err_to_tool_err(&e))?;
                    Ok(ToolResult::ok_text(format!(
                        "long_term memory updated ({} bytes)",
                        args.content.len()
                    )))
                }
                MemoryWriteTarget::Auto => {
                    // auto：追加条目（C-27：指令性内容由 policy 降级 Ask）。
                    let topic = args.topic.unwrap_or_else(|| {
                        // 无 topic 时用内容前 20 字符作默认键。
                        args.content.chars().take(20).collect()
                    });
                    let category = args.category.unwrap_or(MemoryCategory::Pref);
                    let count = self
                        .auto
                        .add_entry(
                            topic,
                            args.content.clone(),
                            category,
                            args.confidence,
                            args.source.clone(),
                        )
                        .await?;
                    Ok(ToolResult::ok_text(format!(
                        "auto memory entry added (total {count} entries)"
                    )))
                }
            }
        })
    }

    /// 渲染意图（R-05，M-11）：写入确认消息，文本直出。
    fn render_output(&self, result: &ToolResult) -> RenderIntent {
        RenderIntent::default_for(result)
    }
}

/// `MemoryError` → `ToolError` 转换。
fn memory_err_to_tool_err(e: &MemoryError) -> ToolError {
    ToolError::Exec(format!("memory error: {e}"))
}

/// 从 `ToolResult` 提取文本内容（测试辅助）。
#[cfg(test)]
fn result_text(result: &ToolResult) -> &str {
    match &result.content {
        minicoding_core::model::ToolContent::Text(t) => t,
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use minicoding_core::model::MemoryError;
    use minicoding_core::provider::BoxFuture;
    use minicoding_core::tool::ToolContext;

    /// Mock MemoryStore：记录最后写入内容。
    struct MockStore {
        last: tokio::sync::Mutex<Option<String>>,
    }

    impl MockStore {
        fn new() -> Self {
            Self {
                last: tokio::sync::Mutex::new(None),
            }
        }
    }

    impl MemoryStore for MockStore {
        fn load(&self) -> BoxFuture<'_, Result<String, MemoryError>> {
            Box::pin(async move { Ok(self.last.lock().await.clone().unwrap_or_default()) })
        }
        fn save(&self, content: &str) -> BoxFuture<'_, Result<(), MemoryError>> {
            let content = content.to_owned();
            Box::pin(async move {
                *self.last.lock().await = Some(content);
                Ok(())
            })
        }
        fn last_mtime(&self) -> Option<time::OffsetDateTime> {
            None
        }
    }

    fn make_tool() -> MemoryWrite {
        MemoryWrite::new(
            Arc::new(MockStore::new()),
            Arc::new(InMemoryAutoMemory::new()),
        )
    }

    fn make_ctx() -> ToolContext {
        ToolContext::new(Utf8PathBuf::from("."), "test-session".to_string())
    }

    #[tokio::test]
    async fn write_long_term_saves_content() {
        let tool = make_tool();
        let input = json!({
            "target": "long_term",
            "content": "user prefers dark theme"
        });
        let result = tool.execute(input, &make_ctx()).await.unwrap();
        assert!(result_text(&result).contains("long_term memory updated"));
    }

    #[tokio::test]
    async fn write_auto_adds_entry() {
        let tool = make_tool();
        let input = json!({
            "target": "auto",
            "content": "prefer 4-space indent",
            "topic": "indent-style",
            "category": "pref",
            "confidence": 0.8
        });
        let result = tool.execute(input, &make_ctx()).await.unwrap();
        let text = result_text(&result);
        assert!(text.contains("auto memory entry added"));
        assert!(text.contains("total 1 entries"));
    }

    #[tokio::test]
    async fn write_auto_dedup_by_topic() {
        let tool = make_tool();
        let ctx = make_ctx();
        // 第一次写入。
        tool.execute(
            json!({"target": "auto", "content": "v1", "topic": "t", "category": "pref"}),
            &ctx,
        )
        .await
        .unwrap();
        // 同 topic 第二次写入（更新）。
        let result = tool
            .execute(
                json!({"target": "auto", "content": "v2", "topic": "t", "category": "pref"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result_text(&result).contains("total 1 entries"));
    }

    #[tokio::test]
    async fn write_auto_defaults_confidence_and_category() {
        let tool = make_tool();
        let input = json!({
            "target": "auto",
            "content": "some note",
            "topic": "note"
        });
        let result = tool.execute(input, &make_ctx()).await.unwrap();
        assert!(result_text(&result).contains("total 1 entries"));
    }

    #[tokio::test]
    async fn write_auto_without_topic_uses_content_prefix() {
        let tool = make_tool();
        let input = json!({
            "target": "auto",
            "content": "a short note"
        });
        let result = tool.execute(input, &make_ctx()).await.unwrap();
        assert!(result_text(&result).contains("total 1 entries"));
    }

    #[tokio::test]
    async fn invalid_target_returns_error() {
        let tool = make_tool();
        let input = json!({
            "target": "invalid",
            "content": "x"
        });
        let err = tool.execute(input, &make_ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }
}
