//! L4 硬截断兜底（见 `docs/design.md` §3.3）。
//!
//! 按 token 数从尾部保留，确保不超预算。保留全部 system 消息 + 最近的消息直到
//! token 数降至 `budget.compact_threshold()` 以下。记录 warn 日志（兜底降级）。

use minicoding_core::model::{Message, Role};
use minicoding_core::provider::Tokenizer;

use crate::budget::TokenBudget;

use super::CompressResult;

/// L4 硬截断兜底。
///
/// 分离 system 与非 system 消息：保留全部 system 消息，从非 system 消息头部
/// 丢弃最旧的若干条，直到 `tokenizer.count_messages` ≤ `budget.compact_threshold()`。
/// 丢弃数记入 `result.truncated_count` 并打 warn 日志。
pub fn hard_truncate(
    messages: &mut Vec<Message>,
    tokenizer: &dyn Tokenizer,
    budget: &TokenBudget,
    result: &mut CompressResult,
) {
    let threshold = budget.compact_threshold();
    if tokenizer.count_messages(messages) <= threshold {
        return;
    }

    // 分离 system（全保留）与非 system（从头部丢弃）
    let mut system_msgs: Vec<Message> = Vec::new();
    let mut non_system: Vec<Message> = Vec::new();
    for msg in messages.drain(..) {
        match msg.role {
            Role::System => system_msgs.push(msg),
            _ => non_system.push(msg),
        }
    }

    // 从头部跳过最旧的非 system 消息，直到剩余 token ≤ 阈值
    let mut keep_from = 0;
    while keep_from < non_system.len() {
        let mut test = system_msgs.clone();
        test.extend_from_slice(&non_system[keep_from..]);
        if tokenizer.count_messages(&test) <= threshold {
            break;
        }
        keep_from += 1;
    }

    let dropped = keep_from;
    system_msgs.extend(non_system.into_iter().skip(keep_from));
    *messages = system_msgs;

    if dropped > 0 {
        result.truncated_count += dropped;
        tracing::warn!(
            dropped = dropped,
            threshold = threshold,
            "L4 硬截断兜底：丢弃 {} 条最旧非 system 消息以降至阈值以下",
            dropped
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::model::Message;
    use minicoding_core::provider::Tokenizer;

    /// 简单分词器：每字符算 1 token。
    struct CharTokenizer;

    impl Tokenizer for CharTokenizer {
        fn count(&self, text: &str) -> usize {
            text.chars().count()
        }
        fn count_messages(&self, msgs: &[Message]) -> usize {
            msgs.iter().map(|m| m.text().chars().count()).sum()
        }
        fn id(&self) -> &'static str {
            "char-test"
        }
    }

    #[test]
    fn truncates_to_threshold() {
        let tokenizer = CharTokenizer;
        // 用大窗口让 threshold 有意义（小窗口 saturating 到 0）
        let budget = TokenBudget {
            context_window: 10_000,
            reserved_output: 100,
            safety_margin: 0,
        };
        // threshold = (10000-100-0)*0.85 = 8415
        // 2000 条 * 7 chars = 14000 > 8415
        let mut msgs: Vec<Message> = (0..2000)
            .map(|i| Message::user_text(format!("msg{i:04}"))) // 每条 7 chars
            .collect();
        let total_before = tokenizer.count_messages(&msgs);
        assert!(total_before > budget.compact_threshold());
        let mut result = CompressResult::default();
        hard_truncate(&mut msgs, &tokenizer, &budget, &mut result);
        assert!(result.truncated_count > 0);
        assert!(tokenizer.count_messages(&msgs) <= budget.compact_threshold());
    }

    #[test]
    fn keeps_system_messages() {
        let tokenizer = CharTokenizer;
        let budget = TokenBudget {
            context_window: 100,
            reserved_output: 0,
            safety_margin: 0,
        };
        // threshold = 100 * 0.85 = 85
        let mut msgs: Vec<Message> = vec![Message::system_text("system")]; // 6 chars
        for i in 0..20 {
            msgs.push(Message::user_text(format!("msg{i:02}"))); // 5 chars each
        }
        let mut result = CompressResult::default();
        hard_truncate(&mut msgs, &tokenizer, &budget, &mut result);
        // system 消息必须保留
        assert_eq!(msgs[0].role, minicoding_core::model::Role::System);
        assert!(
            msgs.iter()
                .any(|m| m.role == minicoding_core::model::Role::System)
        );
    }

    #[test]
    fn no_truncate_when_under_threshold() {
        let tokenizer = CharTokenizer;
        let budget = TokenBudget::new(10_000);
        let mut msgs = vec![Message::user_text("short")];
        let mut result = CompressResult::default();
        hard_truncate(&mut msgs, &tokenizer, &budget, &mut result);
        assert_eq!(result.truncated_count, 0);
        assert_eq!(msgs.len(), 1);
    }
}
