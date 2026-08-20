//! 渲染意图类型（M-11 / R-05 工具输出声明 render intent）。
//!
//! 工具声明"输出如何被展示"（对齐 dsh `ToolOutputDefinition`：output.schema +
//! render 纯函数 + presentationMeta）。协议中立：前端可按工具名 + schema 本地
//! 渲染，也可由服务端下发 `RenderIntent` 投影（零协议改动优先）。
//!
//! 设计见 `docs/design.md` §7（M-11）、`docs/improvement-design.md` §3.2（R-05）。

use crate::model::{ToolContent, ToolResult};

/// 工具输出 JSON Schema 声明（R-05）。
///
/// 描述 `ToolResult` 中 `ToolContent::Json` 的结构化形态，前端据此校验数据
/// 合法性后本地渲染。只对返回 JSON 的工具提供；仅自由文本的工具返回 `None`
/// （`Tool::output_schema` 默认实现）。
#[derive(Debug, Clone)]
pub struct ToolOutputSchema {
    /// 输出 JSON Schema（subset：type/properties/items/required）。
    pub schema: serde_json::Value,
}

/// 列表项（`RenderIntent::List` 的单行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    /// 主文本（如文件名、进程命令行）。
    pub label: String,
    /// 次要文本（如大小、状态），可选。
    pub hint: Option<String>,
}

/// 列表语义（前端据此选择图标/格式，如文件树角标）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    /// 文件/路径列表（`fs.glob`/`fs.list` 等）。
    Files,
    /// 进程/命令列表（`shell.ps` 等）。
    Processes,
    /// 通用列表（无特殊语义）。
    Generic,
}

/// 渲染意图（协议中立，前端卡片标签渲染）。
///
/// 对齐 dsh `presentResult` 的 card-tagged render：
/// - `Text`：文本直出（默认）；
/// - `List`：结构化列表（文件树、进程列表等）；
/// - `Table`：键值表（git diff 统计、任务列表等）；
/// - `Code`：代码片段（shell 输出、diff）；
/// - `Json`：结构化 JSON（`task.*` 等）。
#[derive(Debug, Clone, PartialEq)]
pub enum RenderIntent {
    /// 文本直出（默认）。
    Text { content: String },
    /// 结构化列表（文件树、进程列表等）。
    List {
        items: Vec<ListItem>,
        kind: ListKind,
    },
    /// 键值表（git diff 统计、任务列表等）。
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    /// 代码片段（shell 输出、diff）。
    Code {
        lang: Option<String>,
        content: String,
    },
    /// 结构化 JSON（`task.*` 等）。
    Json { value: serde_json::Value },
}

impl RenderIntent {
    /// 默认渲染意图：文本直出 / JSON 美化。
    ///
    /// `Tool::render_output` 未覆盖时回归此实现（与现状行为一致，R-05 验收
    /// "未提供 `render_output` 的工具行为与现状一致"）。
    #[must_use]
    pub fn default_for(result: &ToolResult) -> Self {
        match &result.content {
            ToolContent::Text(t) => Self::Text { content: t.clone() },
            ToolContent::Json(v) => Self::Json { value: v.clone() },
            ToolContent::Image { .. } => Self::Text {
                content: "[图片]".to_string(),
            },
            ToolContent::Mixed(parts) => {
                // 混合内容取全部文本块拼接（图片无文本投影，保持原样语义）
                let mut buf = String::new();
                for part in parts {
                    if let ToolContent::Text(t) = part {
                        if !buf.is_empty() {
                            buf.push('\n');
                        }
                        buf.push_str(t);
                    }
                }
                Self::Text { content: buf }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ToolResult, ToolResultMeta};
    use serde_json::json;

    #[test]
    fn default_for_text_is_text() {
        let result = ToolResult::ok_text("hello\nworld");
        assert_eq!(
            RenderIntent::default_for(&result),
            RenderIntent::Text {
                content: "hello\nworld".to_string(),
            }
        );
    }

    #[test]
    fn default_for_json_is_json() {
        let result = ToolResult::ok_json(json!({"tasks": []}));
        assert_eq!(
            RenderIntent::default_for(&result),
            RenderIntent::Json {
                value: json!({"tasks": []}),
            }
        );
    }

    #[test]
    fn default_for_mixed_joins_text_parts() {
        let result = ToolResult {
            content: ToolContent::Mixed(vec![
                ToolContent::Text("line1".into()),
                ToolContent::Image {
                    mime: "image/png".into(),
                    data: vec![1],
                },
                ToolContent::Text("line2".into()),
            ]),
            is_error: false,
            metadata: ToolResultMeta::default(),
        };
        assert_eq!(
            RenderIntent::default_for(&result),
            RenderIntent::Text {
                content: "line1\nline2".to_string(),
            }
        );
    }

    #[test]
    fn default_for_image_is_placeholder() {
        let result = ToolResult {
            content: ToolContent::Image {
                mime: "image/png".into(),
                data: vec![1],
            },
            is_error: false,
            metadata: ToolResultMeta::default(),
        };
        assert_eq!(
            RenderIntent::default_for(&result),
            RenderIntent::Text {
                content: "[图片]".to_string(),
            }
        );
    }
}
