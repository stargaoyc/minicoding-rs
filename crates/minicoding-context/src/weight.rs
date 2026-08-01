//! 消息权重模型（见 `docs/design.md` §3.2）。
//!
//! 权重公式：`w = base(role) * recency * sticky * manual_pin`。
//! 权重越低越先被压缩/丢弃；`base(system) = 1.0` 保证系统消息永不压缩。

use minicoding_core::model::{Message, Role};

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
/// 当前 `MessageMeta` 无错误/未提交变更字段：
/// - 错误信息位于 `ContentBlock::ToolResult::is_error`，但 design.md §3.2 的 sticky
///   语义需"未提交变更"标记，该标记由 M5 Hook 注入（见 `docs/hooks.md`）。
/// - 故暂返回 `false`，待 M5 Hook 接入后在 meta 扩展字段并填充。
#[must_use]
fn is_sticky(_msg: &Message) -> bool {
    // TODO(M5): Hook 接入后读取 meta 中的未提交变更/错误聚合标记。
    false
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
