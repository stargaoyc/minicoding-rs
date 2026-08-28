//! 消息权重模型（见 `docs/design.md` §3.2）。
//!
//! 权重公式：`w = base(role) * recency * sticky * manual_pin`。
//! 权重越低越先被压缩/丢弃；`base(system) = 1.0` 保证系统消息永不压缩。

use minicoding_core::model::{ContentBlock, Message, Role};

/// 计算单条消息的权重 `w ∈ [0, 1+]`。
///
/// - `index`：消息在序列中的位置（从 0 起）。
/// - `total`：消息序列总条数。
///
/// 权重越低越先被压缩/丢弃。`system` 消息 `base = 1.0` 且通常 `recency` 高，
/// 压缩管道永不选中（见 `docs/design.md` §3.2）。
#[must_use]
// design.md §3.2 明确以 usize→f64 计算比例；上下文条数远小于 f64 尾数精度。
#[allow(clippy::cast_precision_loss)]
pub fn message_weight(msg: &Message, index: usize, total: usize) -> f64 {
    // system 消息永不压缩（design.md §3.2：base(system)=1.0 永不压缩），
    // 直接返回 ≥1.0，不受 recency/sticky/pin 影响。
    if msg.role == Role::System {
        return 1.0;
    }
    let base = match msg.role {
        Role::System => 1.0, // 不会到达（上方已 return）
        Role::User => 0.9,
        Role::Assistant => 0.6,
        Role::Tool => 0.4, // 最易压缩
    };
    // recency：越新越高。index 是 Vec 索引（0=最旧），用 (index+1)/total 使
    // 最旧消息 recency 最低（接近 0）、最新消息 recency 最高（=1.0）。
    // 对应 design.md §3.2 的 `1 - i/N`（i 为距最新的偏移，此处转换为 Vec 索引）。
    let recency = (index + 1) as f64 / total.max(1) as f64;
    let sticky = if is_sticky(msg) { 1.5 } else { 1.0 }; // 错误/未提交变更 ×1.5
    let manual_pin = if is_pinned(msg) { 2.0 } else { 1.0 }; // 用户 pin ×2.0
    base * recency * sticky * manual_pin
}

/// 消息是否为 sticky（含错误标记或未提交变更标记）。
///
/// CTX-R6-5（2026-08-28 R6 审查）：此前恒 `false`——design.md §3.2 的
/// "错误/未提交变更 ×1.5 权重保护"完全未生效。当前实现：错误标记来自
/// `ContentBlock::ToolResult::is_error`（工具执行失败的消息受保护，避免
/// 摘要/裁剪丢弃失败现场）；未提交变更标记由 M5 Hook 注入（见 `docs/hooks.md`，
/// meta 扩展字段后填充——Hook 未接入前此维度仍为空）。
#[must_use]
fn is_sticky(msg: &Message) -> bool {
    msg.content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolResult { is_error: true, .. }))
}

/// 消息是否被用户 pin（压缩时不裁剪）。
///
/// 直接读取 `MessageMeta::pinned`（见 `docs/api.md` §`MessageMeta`）。
#[must_use]
fn is_pinned(msg: &Message) -> bool {
    msg.metadata.pinned
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::model::{Message, MessageMeta};

    #[test]
    fn system_base_is_one_regardless_of_position() {
        // system 消息恒返回 1.0，不受 recency 影响（永不压缩）。
        let msg = Message::system_text("system prompt");
        assert!((message_weight(&msg, 0, 4) - 1.0).abs() < f64::EPSILON);
        assert!((message_weight(&msg, 3, 4) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn tool_result_has_lowest_base() {
        let user = Message::user_text("u");
        let asst = Message::assistant_text("a");
        let tool = Message {
            role: Role::Tool,
            ..Message::assistant_text("t")
        };
        // 同位置（index 3，最新）比较 base：tool < assistant < user
        assert!(message_weight(&tool, 3, 4) < message_weight(&asst, 3, 4));
        assert!(message_weight(&asst, 3, 4) < message_weight(&user, 3, 4));
    }

    #[test]
    fn pinned_doubles_weight() {
        let pinned_meta = MessageMeta {
            pinned: true,
            ..Default::default()
        };
        let pinned = Message {
            role: Role::User,
            metadata: pinned_meta,
            ..Message::user_text("pinned")
        };
        let normal = Message::user_text("normal");
        // 同位置，pin ×2.0
        let w_normal = message_weight(&normal, 3, 4);
        let w_pinned = message_weight(&pinned, 3, 4);
        assert!((w_pinned - w_normal * 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn error_tool_result_is_sticky() {
        // CTX-R6-5（2026-08-28 R6 审查）：工具失败消息 sticky ×1.5——此前
        // is_sticky 恒 false，design §3.2 的错误权重保护完全未生效。
        use minicoding_core::model::{ContentBlock, ToolCallId, ToolContent, ToolResultMeta};
        let mut err_msg = Message::user_text("errored");
        err_msg.content = vec![ContentBlock::ToolResult {
            call_id: ToolCallId::new(),
            content: ToolContent::Text("boom".to_string()),
            is_error: true,
            metadata: ToolResultMeta::default(),
        }];
        let ok_msg = Message::user_text("ok");
        let w_err = message_weight(&err_msg, 3, 4);
        let w_ok = message_weight(&ok_msg, 3, 4);
        assert!((w_err - w_ok * 1.5).abs() < f64::EPSILON);
        // 非错误 tool result 不 sticky
        let mut tool_ok = Message::user_text("tool ok");
        tool_ok.content = vec![ContentBlock::ToolResult {
            call_id: ToolCallId::new(),
            content: ToolContent::Text("ok".to_string()),
            is_error: false,
            metadata: ToolResultMeta::default(),
        }];
        let w_tool_ok = message_weight(&tool_ok, 3, 4);
        assert!((w_tool_ok - w_ok).abs() < f64::EPSILON);
    }

    #[test]
    fn recency_increases_with_index() {
        let msg = Message::user_text("u");
        // total=4：index 0（最旧）recency=0.25，index 3（最新）recency=1.0
        let w_old = message_weight(&msg, 0, 4);
        let w_new = message_weight(&msg, 3, 4);
        // 越新权重越高
        assert!(w_new > w_old);
        // base=0.9, recency(0)=1/4=0.25 → 0.225
        assert!((w_old - 0.225).abs() < 1e-9);
        // base=0.9, recency(3)=4/4=1.0 → 0.9
        assert!((w_new - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn zero_total_does_not_panic() {
        let msg = Message::user_text("u");
        // total=0 时 max(1) 防 除零，不应 panic。
        let _ = message_weight(&msg, 0, 0);
    }
}
