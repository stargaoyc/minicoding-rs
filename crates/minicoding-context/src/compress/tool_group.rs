//! 工具调用配对组的原子压缩（CT-2，2026-08-25 审查 §5）。
//!
//! `assistant(tool_calls 非空)` 与其后**紧随**的连续 `Role::Tool` 结果是 provider
//! 协议上的原子单元：只删其一会产生孤儿 `tool_result`（后续请求被 API 拒绝）
//! 或悬空 `tool_call`（模型上下文断裂）。此前 L2/L3/L4 选择被删消息时按扁平
//! 索引独立取舍，不感知配对关系；本模块提供共享的"索引 → 配对组"扩展辅助，
//! 三处压缩级别统一应用：**删除组内任一成员则整组纳入**（而非整组保护——
//! 保护会阻止压缩边界推进，极端情况下 L3/L4 无法降至阈值下，违反 C-29 降级链
//! 的收敛目标；整组删除语义上等价于"这轮工具交互被整体摘要/丢弃"，可接受）。

use std::collections::BTreeSet;

use minicoding_core::model::{Message, Role};

/// 一个配对组的闭区间 `[start, end]`（assistant 及其后连续 tool 结果，含端点）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ToolGroup {
    start: usize,
    end: usize,
}

/// 扫描消息序列中的全部配对组（按 `start` 升序、互不重叠）。
///
/// 组定义：`Role::Assistant` 且 `tool_calls` 非空的消息 + 其后**紧随**的连续
/// `Role::Tool` 消息。中间夹任何其他角色即断组；无 tool 结果的悬空 assistant
/// 自成一组（删除它不会新增孤儿）。其余消息不属于任何组。
fn scan_groups(messages: &[Message]) -> Vec<ToolGroup> {
    let mut groups = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        let is_caller = messages[i].role == Role::Assistant && !messages[i].tool_calls.is_empty();
        if !is_caller {
            i += 1;
            continue;
        }
        let mut end = i;
        while end + 1 < messages.len() && messages[end + 1].role == Role::Tool {
            end += 1;
        }
        groups.push(ToolGroup { start: i, end });
        // 组成员整体跳过：组内不可能再起新组（tool 消息不是 caller）
        i = end + 1;
    }
    groups
}

/// 配对组扩展：候选集合中任一消息命中某组，则整组纳入。
///
/// 返回升序去重后的完整索引集合。未命中任何组的候选原样保留。
pub(crate) fn expand_to_groups(messages: &[Message], candidates: &[usize]) -> Vec<usize> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let groups = scan_groups(messages);
    let mut out = BTreeSet::new();
    for &c in candidates {
        // 组升序且互不重叠，可二分定位包含 `c` 的组
        let hit = groups.binary_search_by(|g| {
            if g.end < c {
                std::cmp::Ordering::Less
            } else if g.start > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        });
        match hit {
            // binary_search_by 命中返回的是 groups 的下标
            Ok(gi) => out.extend(groups[gi].start..=groups[gi].end),
            Err(_) => {
                out.insert(c);
            }
        }
    }
    out.into_iter().collect()
}

/// 前缀边界扩展：丢弃区为某子序列的前缀 `[0, prefix)` 时（L4），若边界落在
/// 配对组中间，则把整组纳入丢弃，返回新的前缀长度。
///
/// 单趟扫描即可收敛：扩展只会越过当前组的 `end`，而下一组的 `start` ≥ 当前组
/// `end + 1`，不会产生级联。
pub(crate) fn extend_prefix_to_group_boundary(messages: &[Message], prefix: usize) -> usize {
    let mut prefix = prefix;
    for g in scan_groups(messages) {
        // 仅处理与边界相交（部分丢弃）的组；完全在界内/界外的组无需处理
        if g.start < prefix && prefix <= g.end {
            prefix = g.end + 1;
        }
    }
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::model::{ContentBlock, MessageMeta, ToolCall, ToolCallId};
    use time::OffsetDateTime;

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

    /// 构造 `Role::Tool` 结果消息（组成员）。
    fn tool_result(call_id: &str) -> Message {
        Message {
            id: ulid::Ulid::new().to_string(),
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                call_id: call_id.to_string() as ToolCallId,
                content: minicoding_core::model::ToolContent::Text("ok".into()),
                is_error: false,
                metadata: minicoding_core::model::ToolResultMeta::default(),
            }],
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.to_string()),
            created_at: OffsetDateTime::now_utc(),
            metadata: MessageMeta::default(),
        }
    }

    #[test]
    fn expand_pulls_whole_group_from_tool_result_candidate() {
        // [0]=user [1]=A(tc) [2]=T [3]=user：选中 2（孤儿 tool_result）应扩展为 [1,2]
        let msgs = vec![
            Message::user_text("u0"),
            assistant_with_call("c1"),
            tool_result("c1"),
            Message::user_text("u1"),
        ];
        let expanded = expand_to_groups(&msgs, &[2]);
        assert_eq!(expanded, vec![1, 2], "应从 tool_result 回溯纳入整个组");
    }

    #[test]
    fn expand_pulls_forward_from_assistant_candidate() {
        // 选中组头 1 应带上紧随的结果 2
        let msgs = vec![
            Message::user_text("u0"),
            assistant_with_call("c1"),
            tool_result("c1"),
        ];
        let expanded = expand_to_groups(&msgs, &[1]);
        assert_eq!(expanded, vec![1, 2]);
    }

    #[test]
    fn multi_tool_results_grouped_atomically() {
        // 一个 assistant 带两个并行调用 + 两个结果：[1..=4] 是一个组
        let msgs = vec![
            Message::user_text("u0"),
            assistant_with_call("c1"),
            tool_result("c1"),
            tool_result("c1"),
            tool_result("c1"),
            Message::user_text("u1"),
        ];
        let expanded = expand_to_groups(&msgs, &[3]);
        assert_eq!(expanded, vec![1, 2, 3, 4]);
    }

    #[test]
    fn adjacent_groups_do_not_merge() {
        // 相邻两组 [1,2] 与 [3,4]，选中 2 只扩展本组，不串到下一组
        let msgs = vec![
            Message::user_text("u0"),
            assistant_with_call("c1"),
            tool_result("c1"),
            assistant_with_call("c2"),
            tool_result("c2"),
        ];
        assert_eq!(expand_to_groups(&msgs, &[2]), vec![1, 2]);
        assert_eq!(expand_to_groups(&msgs, &[3]), vec![3, 4]);
    }

    #[test]
    fn non_group_candidates_pass_through_sorted_dedup() {
        let msgs = vec![Message::user_text("a"), Message::user_text("b")];
        assert_eq!(expand_to_groups(&msgs, &[1, 0, 0]), vec![0, 1]);
    }

    #[test]
    fn system_message_between_breaks_group() {
        // assistant 与 tool 结果中间夹 system → 不构成组，各自独立
        let msgs = vec![
            Message::user_text("u0"),
            assistant_with_call("c1"),
            Message::system_text("s"),
            tool_result("c1"),
        ];
        assert_eq!(expand_to_groups(&msgs, &[3]), vec![3]);
        assert_eq!(expand_to_groups(&msgs, &[1]), vec![1]);
    }

    #[test]
    fn extend_prefix_cuts_at_group_boundary() {
        // [0]=user [1]=A [2]=T [3]=user：前缀 2（丢 user+A、留 T）→ 扩展为 3
        let msgs = vec![
            Message::user_text("u0"),
            assistant_with_call("c1"),
            tool_result("c1"),
            Message::user_text("u1"),
        ];
        assert_eq!(extend_prefix_to_group_boundary(&msgs, 2), 3);
        // 边界不在组内时不移动
        assert_eq!(extend_prefix_to_group_boundary(&msgs, 1), 1);
        assert_eq!(extend_prefix_to_group_boundary(&msgs, 3), 3);
        assert_eq!(extend_prefix_to_group_boundary(&msgs, 4), 4);
    }

    #[test]
    fn extend_prefix_handles_adjacent_groups_single_pass() {
        // 相邻两组 [1,2][3,4]：前缀 4 落在第二组内 → 扩到 5；单趟即收敛
        let msgs = vec![
            Message::user_text("u0"),
            assistant_with_call("c1"),
            tool_result("c1"),
            assistant_with_call("c2"),
            tool_result("c2"),
        ];
        assert_eq!(extend_prefix_to_group_boundary(&msgs, 4), 5);
    }

    #[test]
    fn dangling_assistant_forms_trivial_group() {
        // 无结果的悬空 assistant 自成一组：选中它不扩展（组就是它自己）
        let msgs = vec![
            Message::user_text("u0"),
            assistant_with_call("c1"),
            Message::user_text("u1"),
        ];
        assert_eq!(expand_to_groups(&msgs, &[1]), vec![1]);
    }
}
