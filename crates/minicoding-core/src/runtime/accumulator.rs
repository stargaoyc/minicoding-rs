//! 流式增量聚合器：把 `Delta` 流聚合为最终 `Message`。
//!
//! 处理 `OpenAI` 风格的工具调用分片（`index` 递增、`args_chunk` 增量拼接）。
//! 详见 `design.md` §2.2。

use crate::model::{Message, StopReason, ToolCall};
use crate::provider::{Delta, ToolCallDelta, Usage};
use std::collections::BTreeMap;

/// 流式增量聚合器。
#[derive(Debug, Default)]
pub struct DeltaAccumulator {
    text: String,
    tool_calls: BTreeMap<u32, ToolCallAcc>,
    usage: Option<Usage>,
    stop_reason: Option<StopReason>,
}

/// 单个工具调用的增量聚合状态。
#[derive(Debug, Default)]
struct ToolCallAcc {
    id: Option<String>,
    name: Option<String>,
    args: String,
}

impl DeltaAccumulator {
    /// 创建空聚合器。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 推入一个 delta。
    pub fn push(&mut self, delta: Delta) {
        match delta {
            Delta::Text(s) => self.text.push_str(&s),
            // 思考过程不进消息正文（流式已经 `Event::ReasoningDelta` 广播，见 `rt.rs`）
            Delta::Reasoning(_) => {}
            Delta::ToolCall(tc) => self.push_tool_call(tc),
            Delta::Usage(u) => self.usage = Some(u),
            Delta::Stop(reason) => self.stop_reason = Some(reason),
        }
    }

    /// 推入工具调用增量。
    fn push_tool_call(&mut self, tc: ToolCallDelta) {
        let entry = self.tool_calls.entry(tc.index).or_default();
        if tc.id.is_some() {
            entry.id = tc.id;
        }
        if tc.name.is_some() {
            entry.name = tc.name;
        }
        if let Some(chunk) = tc.args_chunk {
            entry.args.push_str(&chunk);
        }
    }

    /// 聚合为最终 assistant 消息。
    #[must_use]
    pub fn finalize(self) -> Message {
        let tool_calls: Vec<ToolCall> = self
            .tool_calls
            .into_values()
            .map(|acc| {
                // 坏 args JSON 不静默吞掉（2026-08-23 审查 §4-P2）：Null 会导致
                // dispatch 报 InvalidInput，但排障时需要知道根因是 provider 输出了
                // 畸形增量参数。
                let input = if acc.args.is_empty() {
                    serde_json::Value::Object(serde_json::Map::new())
                } else {
                    match serde_json::from_str(&acc.args) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(
                                tool = ?acc.name,
                                call_id = ?acc.id,
                                args = %acc.args,
                                error = %e,
                                "malformed tool_call args JSON from provider; dispatching with null input"
                            );
                            serde_json::Value::Null
                        }
                    }
                };
                ToolCall {
                    id: acc.id.unwrap_or_default(),
                    name: acc.name.unwrap_or_default(),
                    input,
                }
            })
            .collect();

        let mut msg = Message::assistant_text(self.text);
        msg.tool_calls = tool_calls;
        // token 计量（2026-08-23 审查遗留#7）：provider Usage.output_tokens
        // 落盘到消息元数据，供 CLI/TUI/Web 计量展示与 /tokens 会话累计。
        if let Some(u) = &self.usage {
            msg.metadata.tokens = Some(u.output_tokens);
        }
        msg
    }

    /// 已累积的文本（用于流式渲染）。
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// token 用量统计。
    #[must_use]
    pub fn usage(&self) -> Option<&Usage> {
        self.usage.as_ref()
    }

    /// 停止原因。
    #[must_use]
    pub fn stop_reason(&self) -> Option<&StopReason> {
        self.stop_reason.as_ref()
    }

    /// 是否含工具调用。
    #[must_use]
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_only() {
        let mut acc = DeltaAccumulator::new();
        acc.push(Delta::Text("Hello ".into()));
        acc.push(Delta::Text("world".into()));
        acc.push(Delta::Stop(StopReason::EndTurn));
        let msg = acc.finalize();
        assert_eq!(msg.text(), "Hello world");
        assert!(msg.tool_calls.is_empty(), "expected empty: msg.tool_calls");
    }

    #[test]
    fn tool_call_aggregation() {
        let mut acc = DeltaAccumulator::new();
        acc.push(Delta::Text("reading file".into()));
        acc.push(Delta::ToolCall(ToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            name: Some("fs.read".into()),
            args_chunk: Some("{\"path\":".into()),
        }));
        acc.push(Delta::ToolCall(ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            args_chunk: Some("\"/tmp/a\"}".into()),
        }));
        let msg = acc.finalize();
        assert_eq!(msg.tool_calls.len(), 1);
        assert_eq!(msg.tool_calls[0].name, "fs.read");
        assert_eq!(msg.tool_calls[0].input["path"], "/tmp/a");
    }
}
