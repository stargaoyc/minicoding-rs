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
/// 从头部丢弃最旧的超额非 system 消息。丢弃数记入 `result.dropped_count`；
/// 丢弃消息的序号区间与 token 量记入 `result.dropped_range`/`result.dropped_tokens`
/// （M-07，`anchor_seq` 为 None 时跳过追溯记录）。
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

    // M-07（R-02）：收集被丢弃消息的原始索引与 token 量（追溯区间推算）
    let mut dropped_idx: Vec<usize> = Vec::with_capacity(drop_count);
    let mut seen_non_system = 0;
    for (i, m) in messages.iter().enumerate() {
        if m.role == Role::System {
            continue;
        }
        if seen_non_system < drop_count {
            dropped_idx.push(i);
            result.dropped_tokens += tokenizer.count_messages(std::slice::from_ref(m));
            seen_non_system += 1;
        } else {
            break;
        }
    }

    // 从头部丢弃最旧的非 system 消息，保留全部 system 消息。
    messages.retain(|m| {
        if m.role == Role::System {
            return true;
        }
        if seen_non_system > 0 {
            seen_non_system -= 1;
            false
        } else {
            true
        }
    });

    // 记追溯区间（M-07）
    if let (Some(anchor), Some(&first), Some(&last)) =
        (anchor_seq, dropped_idx.first(), dropped_idx.last())
    {
        let total = messages.len() + dropped_idx.len();
        result.dropped_range = Some((seq_of(first, total, anchor), seq_of(last, total, anchor)));
    }

    result.dropped_count += drop_count;
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::provider::Tokenizer;

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
}
