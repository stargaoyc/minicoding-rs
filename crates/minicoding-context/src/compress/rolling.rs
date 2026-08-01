//! L3 滚动窗口（见 `docs/design.md` §3.3）。
//!
//! 仅保留最近 W 条非 system 消息 + 全部 system 消息，丢弃最旧的非 system 消息。

use minicoding_core::model::{Message, Role};

use super::CompressResult;

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
/// 从头部丢弃最旧的超额非 system 消息。丢弃数记入 `result.dropped_count`。
pub fn rolling_window(
    messages: &mut Vec<Message>,
    config: &RollingConfig,
    result: &mut CompressResult,
) {
    let non_system_count = messages.iter().filter(|m| m.role != Role::System).count();
    if non_system_count <= config.window_size {
        return;
    }

    let drop_count = non_system_count - config.window_size;

    // 从头部丢弃最旧的非 system 消息，保留全部 system 消息。
    // retain 按顺序遍历，dropped 计数达到 drop_count 后保留剩余。
    let mut dropped = 0;
    messages.retain(|m| {
        if m.role == Role::System {
            return true;
        }
        if dropped < drop_count {
            dropped += 1;
            false
        } else {
            true
        }
    });

    result.dropped_count += drop_count;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_system_and_recent_window() {
        let mut msgs: Vec<Message> = (0..30)
            .map(|i| Message::user_text(format!("msg {i}")))
            .collect();
        msgs.insert(0, Message::system_text("system"));
        let mut result = CompressResult::default();
        rolling_window(&mut msgs, &RollingConfig { window_size: 10 }, &mut result);
        // 30 user + 1 system, drop 20 user, keep 1 system + 10 user = 11
        assert_eq!(result.dropped_count, 20);
        assert_eq!(msgs.len(), 11);
        assert_eq!(msgs[0].role, Role::System);
        // 保留的是最近 10 条：msg 20..29
        assert!(msgs[1].text().contains("msg 20"));
        assert!(msgs.last().unwrap().text().contains("msg 29"));
    }

    #[test]
    fn no_drop_when_under_window() {
        let mut msgs = vec![
            Message::system_text("s"),
            Message::user_text("u1"),
            Message::user_text("u2"),
        ];
        let mut result = CompressResult::default();
        rolling_window(&mut msgs, &RollingConfig { window_size: 20 }, &mut result);
        assert_eq!(result.dropped_count, 0);
        assert_eq!(msgs.len(), 3);
    }
}
