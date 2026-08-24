//! L3 滚动窗口（见 `docs/design.md` §3.3）。
//!
//! 仅保留最近 W 条非 system 消息 + 全部 system 消息，丢弃最旧的非 system 消息。

use minicoding_core::model::{Message, Role};
use minicoding_core::provider::Tokenizer;

use super::{CompressResult, seq_of};

/// L3 滚动窗口配置。
#[derive(Debug, Clone)]
pub struct RollingConfig {
    /// 保留最近非 system 消息数（默认 20）。
    pub window_size: usize,
}

impl Default for RollingConfig {
    fn default() -> Self {
        Self { window_size: 20 }
    }
}

/// L3 滚动窗口压缩。
///
/// 保留全部 system 消息 + 最近 `config.window_size` 条非 system 消息，
/// 从头部丢弃最旧的非 system 消息。丢弃数记入 `result.dropped_count`；
/// 丢弃消息的序号区间与 token 量记入 `result.dropped_range`/`result.dropped_tokens`
/// （M-07，`anchor_seq` 为 None 时跳过追溯记录）。
///
/// CT-2（2026-08-25 审查）：候选集经配对组扩展——`assistant(tool_calls)` 与其后
/// 紧随的 `Role::Tool` 结果原子删除，避免孤儿 `tool_result` / 悬空 `tool_call`。
pub fn rolling_window(
    messages: &mut Vec<Message>,
    config: &RollingConfig,
    result: &mut CompressResult,
    tokenizer: &dyn Tokenizer,
    anchor_seq: Option<u64>,
) {
    let non_system_count = messages.iter().filter(|m| m.role != Role::System).count();
    if non_system_count <= config.window_size {
        return;
    }

    let drop_count = non_system_count - config.window_size;
    // CT-2（2026-08-25 审查）：丢弃区恒为非 system 子序列前缀，但配对组跨越该
    // 边界时须整组纳入——先取候选原始索引，再做组扩展后按集合删除。
    let total_before = messages.len();

    // M-07（R-02）：收集候选（最旧 drop_count 条非 system 消息）的原始索引
    let mut candidates: Vec<usize> = Vec::with_capacity(drop_count);
    let mut seen_non_system = 0;
    for (i, m) in messages.iter().enumerate() {
        if m.role == Role::System {
            continue;
        }
        if seen_non_system < drop_count {
            candidates.push(i);
            seen_non_system += 1;
        } else {
            break;
        }
    }

    // CT-2（2026-08-25 审查）：配对组原子删除——组内任一入选则整组纳入，
    // 避免孤儿 tool_result / 悬空 tool_call；实际丢弃数可大于 drop_count。
    let dropped_idx = super::tool_group::expand_to_groups(messages, &candidates);

    // 被丢弃消息的 token 量（追溯/审计用，M-07）
    for &i in &dropped_idx {
        result.dropped_tokens += tokenizer.count_messages(std::slice::from_ref(&messages[i]));
    }

    // 从头部丢弃最旧的非 system 消息，保留全部 system 消息
    // （dropped_idx 升序且只含非 system 索引，双指针同步前进）。
    let mut di = 0usize;
    let mut cur = 0usize;
    messages.retain(|_| {
        let drop = di < dropped_idx.len() && dropped_idx[di] == cur;
        if drop {
            di += 1;
        }
        cur += 1;
        !drop
    });

    // 记追溯区间（M-07）
    if let (Some(anchor), Some(&first), Some(&last)) =
        (anchor_seq, dropped_idx.first(), dropped_idx.last())
    {
        result.dropped_range = Some((
            seq_of(first, total_before, anchor),
            seq_of(last, total_before, anchor),
        ));
    }

    result.dropped_count += dropped_idx.len();
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::model::{ContentBlock, MessageMeta, ToolCall, ToolCallId};
    use minicoding_core::provider::Tokenizer;
    use time::OffsetDateTime;

    /// 每字符计 1 token。
    struct CharTokenizer;

    impl Tokenizer for CharTokenizer {
        fn id(&self) -> &'static str {
            "char"
        }
        fn count(&self, text: &str) -> usize {
            text.chars().count()
        }
        fn count_messages(&self, msgs: &[Message]) -> usize {
            msgs.iter().map(|m| m.text().chars().count()).sum()
        }
    }

    #[test]
    fn keeps_system_and_recent_window() {
        let mut msgs: Vec<Message> = (0..30)
            .map(|i| Message::user_text(format!("msg {i}")))
            .collect();
        msgs.insert(0, Message::system_text("system"));
        let mut result = CompressResult::default();
        rolling_window(
            &mut msgs,
            &RollingConfig { window_size: 10 },
            &mut result,
            &CharTokenizer,
            Some(31),
        );
        // 30 user + 1 system, drop 20 user, keep 1 system + 10 user = 11
        assert_eq!(result.dropped_count, 20);
        assert_eq!(msgs.len(), 11);
        assert_eq!(msgs[0].role, Role::System);
        // 保留的是最近 10 条：msg 20..29
        assert!(msgs[1].text().contains("msg 20"));
        assert!(msgs.last().unwrap().text().contains("msg 29"));
        // M-07：丢弃 20 条 → 区间 [1, 20]（anchor=31, total=31）
        assert_eq!(result.dropped_range, Some((2, 21)));
        assert!(result.dropped_tokens > 0);
    }

    #[test]
    fn no_drop_when_under_window() {
        let mut msgs = vec![
            Message::system_text("s"),
            Message::user_text("u1"),
            Message::user_text("u2"),
        ];
        let mut result = CompressResult::default();
        rolling_window(
            &mut msgs,
            &RollingConfig { window_size: 20 },
            &mut result,
            &CharTokenizer,
            None,
        );
        assert_eq!(result.dropped_count, 0);
        assert_eq!(msgs.len(), 3);
    }

    // === CT-2 回归（2026-08-25 审查）：配对组原子删除 ===

    /// 构造带 `tool_calls` 的 assistant 消息（组头）。
    fn assistant_with_call(call_id: &str) -> Message {
        Message {
            id: ulid::Ulid::new().to_string(),
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "calling".into(),
            }],
            tool_calls: vec![ToolCall {
                id: call_id.to_string(),
                name: "fs.read".into(),
                input: serde_json::json!({"path": "a.rs"}),
            }],
            tool_call_id: None,
            created_at: OffsetDateTime::now_utc(),
            metadata: MessageMeta::default(),
        }
    }

    /// 构造 `Role::Tool` 结果消息（组成员，文本 4 字符便于 token 断言）。
    fn tool_result(call_id: &str) -> Message {
        Message {
            id: ulid::Ulid::new().to_string(),
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                call_id: call_id.to_string() as ToolCallId,
                content: minicoding_core::model::ToolContent::Text("done".into()),
                is_error: false,
                metadata: minicoding_core::model::ToolResultMeta::default(),
            }],
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.to_string()),
            created_at: OffsetDateTime::now_utc(),
            metadata: minicoding_core::model::MessageMeta::default(),
        }
    }

    #[test]
    fn rolling_drops_tool_group_atomically() {
        // [0]=system [1]=u [2]=A(tc) [3]=T：window=1 → 候选 = u(1)
        // 无组扩展时只丢 u，留下孤儿 T；扩展后整组 [2,3] 一并丢弃
        let mut msgs = vec![
            Message::system_text("sys"),
            Message::user_text("user question"),
            assistant_with_call("c1"),
            tool_result("c1"),
        ];
        let mut result = CompressResult::default();
        rolling_window(
            &mut msgs,
            &RollingConfig { window_size: 1 },
            &mut result,
            &CharTokenizer,
            Some(3),
        );
        assert_eq!(result.dropped_count, 3, "候选 1 条 + 组扩展 2 条");
        // 仅剩 system + 最后一条非 system（这里即无剩余 user）
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, Role::System);
        assert!(
            !msgs.iter().any(|m| m.role == Role::Tool),
            "不允许残留孤儿 tool_result"
        );
        // 追溯区间覆盖整个丢弃集合（原始索引 1..=3 → seq 1..=3）
        assert_eq!(result.dropped_range, Some((1, 3)));
    }
}
