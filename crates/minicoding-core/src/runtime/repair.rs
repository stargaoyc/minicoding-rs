//! 悬空 `tool_calls` 回填合成结果（A3 自 model/message.rs 抽出，M-03/D-05）。

use crate::model::{ContentBlock, Message, MessageMeta, MessageSource, Role, ToolContent};
use std::collections::HashSet;
use time::OffsetDateTime;

/// 为会话中"有 `tool_calls` 但缺 `tool_result`"的 assistant 消息补合成错误结果（M-03）。
///
/// 中断路径（cancel/timeout/崩溃）下 `run_turn` 可能留下悬空 `tool_calls`——严格
/// provider（如 Anthropic）要求每个 `tool_use` 必有 `tool_result`，否则 resume 后
/// 请求 400。本函数是纯函数（无 IO，供 `restore_history` / `replay` 防御层复用）：
/// 对每个 assistant 消息的 `tool_calls`，若其 `call_id` 在**全部历史**中无对应
/// `ContentBlock::ToolResult`，则紧跟该 assistant 消息插入一条 `is_error=true` 的
/// 合成 Tool 消息（保持相对顺序）。幂等：已有结果的消息不动（重复调用不重复插入）。
///
/// 合成结果标 `is_error` 且文本为占位符，不作为指令（C-05）。
#[must_use]
pub fn repair_dangling_tool_calls(msgs: Vec<Message>) -> Vec<Message> {
    let answered: HashSet<String> = msgs
        .iter()
        .filter_map(|m| {
            m.content.iter().find_map(|b| {
                if let ContentBlock::ToolResult { call_id, .. } = b {
                    Some(call_id.clone())
                } else {
                    None
                }
            })
        })
        .collect();
    let mut out = Vec::with_capacity(msgs.len());
    for msg in msgs {
        out.push(msg.clone());
        if msg.role == Role::Assistant && !msg.tool_calls.is_empty() {
            for call in &msg.tool_calls {
                if answered.contains(&call.id) {
                    continue;
                }
                out.push(Message {
                    id: ulid::Ulid::new().to_string(),
                    role: Role::Tool,
                    content: vec![ContentBlock::ToolResult {
                        call_id: call.id.clone(),
                        content: ToolContent::text(
                            "[interrupted] 工具调用未执行（turn 被取消/超时/崩溃）",
                        ),
                        is_error: true,
                        metadata: crate::model::ToolResultMeta::default(),
                    }],
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    created_at: OffsetDateTime::now_utc(),
                    metadata: MessageMeta {
                        source: MessageSource::Tool,
                        compressed_range: None,
                        ..Default::default()
                    },
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod proptests {
    use super::repair_dangling_tool_calls;
    use crate::model::{
        ContentBlock, Message, MessageMeta, MessageSource, Role, ToolCall, ToolContent,
    };
    use proptest::prelude::*;
    use time::OffsetDateTime;

    prop_compose! {
        fn arb_message()(
            role in prop_oneof![
                Just(Role::System),
                Just(Role::User),
                Just(Role::Assistant),
                Just(Role::Tool),
            ],
            text in "[a-zA-Z0-9 .,!?-]{0,100}",
            id in "[a-z0-9]{10,26}",
            tool_call_id in proptest::option::of("[a-z0-9]{0,20}"),
            pinned in proptest::bool::ANY,
            summarized in proptest::bool::ANY,
            tokens in proptest::option::of(0usize..10_000),
            secs in 0i64..2_000_000_000i64,
        ) -> Message {
            let source = match role {
                Role::System | Role::User => MessageSource::User,
                Role::Assistant => MessageSource::Llm,
                Role::Tool => MessageSource::Tool,
            };
            Message {
                id,
                role,
                content: vec![ContentBlock::Text { text }],
                tool_calls: Vec::new(),
                tool_call_id,
                created_at: OffsetDateTime::from_unix_timestamp(secs)
                    .expect("secs within valid OffsetDateTime range"),
                metadata: MessageMeta {
                    tokens,
                    pinned,
                    summarized,
                    source,
                    compressed_range: None,
                },
            }
        }
    }

    proptest! {
        #[test]
        fn message_json_roundtrip(msg in arb_message()) {
            // Message 不派生 PartialEq：序列化→反序列化→再序列化后比较 JSON 字符串，
            // 若两次序列化结果一致则说明 roundtrip 无信息丢失。
            let json = serde_json::to_string(&msg).expect("serialize Message");
            let decoded: Message = serde_json::from_str(&json).expect("deserialize Message");
            let json2 = serde_json::to_string(&decoded).expect("serialize decoded Message");
            prop_assert_eq!(json, json2);
        }

        #[test]
        fn role_json_roundtrip(role in prop_oneof![
            Just(Role::System),
            Just(Role::User),
            Just(Role::Assistant),
            Just(Role::Tool),
        ]) {
            let json = serde_json::to_string(&role).expect("serialize Role");
            let decoded: Role = serde_json::from_str(&json).expect("deserialize Role");
            // Role 派生 PartialEq + Eq
            prop_assert_eq!(role, decoded);
        }
    }

    mod repair_tests {
        use super::repair_dangling_tool_calls;
        use crate::model::{
            ContentBlock, Message, MessageMeta, MessageSource, Role, ToolCall, ToolContent,
        };
        use proptest::prelude::*;
        use time::OffsetDateTime;

        fn asst_with_calls(ids: &[&str]) -> Message {
            let mut m = Message::assistant_text("planning");
            m.tool_calls = ids
                .iter()
                .map(|id| ToolCall {
                    id: (*id).to_string(),
                    name: "fs.read".to_string(),
                    input: serde_json::json!({"path": "x"}),
                })
                .collect();
            m
        }

        fn tool_result_for(call_id: &str) -> Message {
            Message {
                id: ulid::Ulid::new().to_string(),
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    call_id: call_id.to_string(),
                    content: ToolContent::text("ok"),
                    is_error: false,
                    metadata: crate::model::ToolResultMeta::default(),
                }],
                tool_calls: Vec::new(),
                tool_call_id: None,
                created_at: OffsetDateTime::now_utc(),
                metadata: MessageMeta {
                    source: MessageSource::Tool,
                    compressed_range: None,
                    ..Default::default()
                },
            }
        }

        fn is_tool_result_for(msg: &Message, call_id: &str) -> bool {
            msg.role == Role::Tool
                && msg.content.iter().any(
                    |b| matches!(b, ContentBlock::ToolResult { call_id: c, .. } if c == call_id),
                )
        }

        fn is_synthetic_result_for(msg: &Message, call_id: &str) -> bool {
            msg.role == Role::Tool
                && msg.content.iter().any(|b| {
                    matches!(b, ContentBlock::ToolResult { call_id: c, is_error: true, .. } if c == call_id)
                })
        }

        #[test]
        fn inserts_synthetic_result_for_dangling_call() {
            // M-03：悬空 tool_call 之后插入 is_error=true 的合成结果
            let asst = asst_with_calls(&["call_a", "call_b"]);
            let result = tool_result_for("call_a");
            let msgs = vec![Message::user_text("hi"), asst, result.clone()];
            let repaired = repair_dangling_tool_calls(msgs);
            // user + assistant + call_a 真实结果 + 合成 call_b 结果
            assert_eq!(repaired.len(), 4, "dangling call_b gets synthetic result");
            // 合成结果紧跟 assistant 之后（call_b），真实结果保持原位
            assert!(is_synthetic_result_for(&repaired[2], "call_b"));
            assert_eq!(repaired[3].id, result.id);
            assert!(is_tool_result_for(&repaired[3], "call_a"));
        }

        #[test]
        fn inserts_after_each_dangling_assistant() {
            // 多个 assistant 各自悬空：合成结果紧跟各自 assistant 之后
            let a1 = asst_with_calls(&["c1"]);
            let a2 = asst_with_calls(&["c2"]);
            let repaired = repair_dangling_tool_calls(vec![a1, a2]);
            assert_eq!(repaired.len(), 4);
            assert!(is_synthetic_result_for(&repaired[1], "c1"));
            assert!(is_synthetic_result_for(&repaired[3], "c2"));
        }

        #[test]
        fn idempotent_when_all_answered() {
            // 已齐的历史：修复后不变（幂等）
            let asst = asst_with_calls(&["c1"]);
            let result = tool_result_for("c1");
            let msgs = vec![asst, result.clone()];
            let repaired = repair_dangling_tool_calls(msgs);
            assert_eq!(repaired.len(), 2);
            assert_eq!(repaired[1].id, result.id);
            // 二次修复不再插入
            let repaired2 = repair_dangling_tool_calls(repaired);
            assert_eq!(repaired2.len(), 2);
        }

        #[test]
        fn normal_history_untouched() {
            // 无悬空：完全不变
            let msgs = vec![Message::user_text("hi"), Message::assistant_text("reply")];
            let repaired = repair_dangling_tool_calls(msgs.clone());
            assert_eq!(repaired.len(), 2);
            assert_eq!(repaired[0].id, msgs[0].id);
            assert_eq!(repaired[1].id, msgs[1].id);
        }
    }
}
