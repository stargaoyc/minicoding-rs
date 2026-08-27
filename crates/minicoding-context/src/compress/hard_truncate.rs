//! L4 硬截断兜底（见 `docs/design.md` §3.3）。
//!
//! 按 token 数从尾部保留，确保不超预算。保留全部 system 消息 + 最近的消息直到
//! token 数降至 `budget.compact_threshold()` 以下。记录 warn 日志（兜底降级）。

use minicoding_core::model::{Message, Role};
use minicoding_core::provider::Tokenizer;

use crate::budget::TokenBudget;

use super::{CompressResult, seq_of};

/// L4 硬截断兜底。
///
/// 分离 system 与非 system 消息：保留全部 system 消息，从非 system 消息头部
/// 丢弃最旧的若干条，直到 `tokenizer.count_messages` ≤ `budget.compact_threshold()`。
/// 丢弃数记入 `result.truncated_count` 并打 warn 日志；丢弃消息的序号区间与
/// token 量记入 `result.dropped_range`/`result.dropped_tokens`（M-07）。
///
/// CT-3（2026-08-25 审查）：每条消息 token 数只计算一次，用后缀和递减定位最小
/// 保留起点——旧实现每轮迭代 clone 全部 `system_msgs` 后全量重分词，O(k×N)；
/// 行为不变（`count_messages` 对消息列表逐条累加，可分解为单条之和）。
/// CT-2（2026-08-25 审查）：截断边界落在配对组中间时整组纳入丢弃。
pub fn hard_truncate(
    messages: &mut Vec<Message>,
    tokenizer: &dyn Tokenizer,
    budget: &TokenBudget,
    result: &mut CompressResult,
    anchor_seq: Option<u64>,
) {
    let threshold = budget.compact_threshold();
    if tokenizer.count_messages(messages) <= threshold {
        return;
    }

    // M-07（R-02）：本函数调用前的消息数（追溯区间推算基准，须在 drain 前取）
    let total_before = messages.len();

    // 分离 system（全保留）与非 system（从头部丢弃）；CT4-6（R4）：pinned
    // 消息并入"全保留"侧——L4 是最后兜底，此前 pinned 消息照样被硬截断丢弃
    //（weight 的 ×2.0 只是软偏好，n 够大时照样被 L2 摘要替换；L3/L4 更是
    // 直接选择器）。pinned 语义为"压缩时不裁剪"，此处兑现。
    let mut system_msgs: Vec<Message> = Vec::new();
    let mut non_system: Vec<Message> = Vec::new();
    // M-07（R-02）：原列表中第一个非 system 消息的 index（追溯区间起点）
    let first_non_system_idx = messages
        .iter()
        .position(|m| m.role != Role::System)
        .unwrap_or(messages.len());
    for msg in messages.drain(..) {
        if msg.role == Role::System || msg.metadata.pinned {
            system_msgs.push(msg);
        } else {
            non_system.push(msg);
        }
    }

    // CT-3：一次性计算 token（system 总量 + 非 system 单条量），后缀和从尾部
    // 累加，找到使总量 ≤ 阈值的最小 keep_from；永不满足时 keep_from = len
    // （丢光非 system，与旧循环语义一致）。
    let system_tokens = tokenizer.count_messages(&system_msgs);
    let ns_tokens: Vec<usize> = non_system
        .iter()
        .map(|m| tokenizer.count_messages(std::slice::from_ref(m)))
        .collect();
    let mut keep_from = non_system.len();
    let mut suffix = 0usize;
    for (i, t) in ns_tokens.iter().enumerate().rev() {
        suffix += t;
        if system_tokens + suffix <= threshold {
            keep_from = i;
        } else {
            break;
        }
    }

    // CT-2（2026-08-25 审查）：截断边界落在配对组中间 → 整组纳入丢弃。
    // 在非 system 子序列上扫描即可——配对组要求 assistant 与 tool 结果紧邻，
    // system 消息夹在其间本就断组。
    let dropped = super::tool_group::extend_prefix_to_group_boundary(&non_system, keep_from);

    // M-07（R-02）：被丢弃消息的 token 量（复用 CT-3 的单次计算结果）
    result.dropped_tokens += ns_tokens.iter().take(dropped).sum::<usize>();
    system_msgs.extend(non_system.into_iter().skip(dropped));
    *messages = system_msgs;

    if dropped > 0 {
        result.truncated_count += dropped;
        if let Some(anchor) = anchor_seq {
            let from = seq_of(first_non_system_idx, total_before, anchor);
            let to = seq_of(first_non_system_idx + dropped - 1, total_before, anchor);
            result.dropped_range = Some((from, to));
        }
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
    use minicoding_core::model::{ContentBlock, Message, ToolCall, ToolCallId};
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
        hard_truncate(&mut msgs, &tokenizer, &budget, &mut result, Some(2000));
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
        hard_truncate(&mut msgs, &tokenizer, &budget, &mut result, Some(2000));
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
        hard_truncate(&mut msgs, &tokenizer, &budget, &mut result, Some(2000));
        assert_eq!(result.truncated_count, 0);
        assert_eq!(msgs.len(), 1);
    }

    // === CT-3 回归（2026-08-25 审查）：前缀和重构与朴素实现行为不变 ===

    #[test]
    fn matches_naive_reference_implementation() {
        // 用测试内独立的朴素算法（模拟旧 O(k×N) 循环）逐场景比对结果：
        // 混合长度消息覆盖"边界恰好/永不满足阈值"等分支。
        struct Case {
            lens: Vec<usize>,
            context_window: usize,
            reserved_output: usize,
            safety_margin: usize,
        }
        let tokenizer = CharTokenizer;
        let cases = vec![
            // 边界精确命中
            Case {
                lens: vec![7; 50],
                context_window: 500,
                reserved_output: 0,
                safety_margin: 0,
            },
            // system 单独超阈值：丢光非 system 仍不满足
            Case {
                lens: vec![5; 30],
                context_window: 100,
                reserved_output: 40,
                safety_margin: 0,
            },
            // 长短混合
            Case {
                lens: (0..60).map(|i| if i % 3 == 0 { 20 } else { 4 }).collect(),
                context_window: 600,
                reserved_output: 50,
                safety_margin: 0,
            },
        ];

        for case in cases {
            let budget = TokenBudget {
                context_window: case.context_window,
                reserved_output: case.reserved_output,
                safety_margin: case.safety_margin,
            };
            let mut msgs: Vec<Message> = case
                .lens
                .iter()
                .map(|&l| Message::user_text("x".repeat(l)))
                .collect();
            msgs.insert(0, Message::system_text("sys")); // 3 字符

            let threshold = budget.compact_threshold();
            let sys_tokens = tokenizer.count_messages(&[msgs[0].clone()]);
            let ns: Vec<usize> = msgs[1..].iter().map(|m| m.text().chars().count()).collect();
            // 朴素参考：最小 k 使 system + 后缀和 ≤ 阈值，否则全丢
            let expected_keep = (0..=ns.len())
                .find(|&k| sys_tokens + ns[k..].iter().sum::<usize>() <= threshold)
                .unwrap_or(ns.len());

            let mut result = CompressResult::default();
            hard_truncate(
                &mut msgs,
                &tokenizer,
                &budget,
                &mut result,
                Some(ns.len() as u64 + 1),
            );

            assert_eq!(
                result.truncated_count,
                expected_keep,
                "丢弃数应与朴素参考一致（case lens={:?}）",
                case.lens.len()
            );
            assert_eq!(
                msgs.len(),
                1 + (ns.len() - expected_keep),
                "保留数应与朴素参考一致"
            );
            let expect_tokens: usize = ns[..expected_keep].iter().sum();
            assert_eq!(result.dropped_tokens, expect_tokens);
        }
    }

    #[test]
    fn truncate_drops_tool_group_atomically_at_boundary() {
        // CT-2 回归（不变式断言）：预算边界恰好落在 assistant(tool_calls) 与其
        // tool 结果之间时，扩展必须把整组带走，不允许孤儿 tool_result 存活。
        let tokenizer = CharTokenizer;
        // threshold = floor(12 * 0.85) = 10
        let budget = TokenBudget {
            context_window: 12,
            reserved_output: 0,
            safety_margin: 0,
        };
        let caller = Message {
            id: ulid::Ulid::new().to_string(),
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "calling".into(),
            }], // 7 chars
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "fs.read".into(),
                input: serde_json::json!({"path": "a.rs"}),
            }],
            tool_call_id: None,
            created_at: time::OffsetDateTime::now_utc(),
            metadata: minicoding_core::model::MessageMeta::default(),
        };
        let tool_res = Message {
            id: ulid::Ulid::new().to_string(),
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                call_id: "c1".to_string() as ToolCallId,
                content: minicoding_core::model::ToolContent::Text("done".into()), // 4 chars
                is_error: false,
                metadata: minicoding_core::model::ToolResultMeta::default(),
            }],
            tool_calls: Vec::new(),
            tool_call_id: Some("c1".into()),
            created_at: time::OffsetDateTime::now_utc(),
            metadata: minicoding_core::model::MessageMeta::default(),
        };
        // 纯预算判定：sys(3)+T(4)=7≤10 但含 A 时 14>10 → 边界落在组中间，
        // 必须经组扩展收敛到整组丢弃。
        let mut msgs = vec![
            Message::system_text("sys"), // 3
            Message::user_text("user"),  // 4
            caller,
            tool_res,
        ];
        let mut result = CompressResult::default();
        hard_truncate(&mut msgs, &tokenizer, &budget, &mut result, None);

        let has_orphan = msgs.iter().any(|m| {
            m.role == Role::Tool
                && !msgs
                    .iter()
                    .any(|p| p.role == Role::Assistant && !p.tool_calls.is_empty())
        });
        assert!(!has_orphan, "不允许残留孤儿 tool_result");
        assert!(
            tokenizer.count_messages(&msgs) <= budget.compact_threshold(),
            "截断后应降至阈值下"
        );
        assert!(result.truncated_count > 0);
    }

    #[test]
    fn truncate_extends_boundary_into_group() {
        // 精确验证边界推进：非 system 序列 [u(4), A(7), T(4)]，阈值使纯预算
        // 判定 keep_from 落在 A 与 T 之间（即丢 u+A、留 T）→ 扩展为整组丢弃。
        let tokenizer = CharTokenizer;
        // sys(6) 全保留。目标：sys+T ≤ 阈值 < sys+A+T 且 sys+u+A+T > 阈值。
        // T(4)：sys+4 ≤ 阈值；加 A(7)：sys+11 > 阈值 → 阈值 ∈ [10, 16)
        let budget = TokenBudget {
            context_window: 13,
            reserved_output: 0,
            safety_margin: 0,
        }; // threshold = 11（floor(13*0.85)）；6+4=10≤11 < 6+11=17
        // 但还要保证总长触发：6+4+7+4=21 > 11 ✓
        let caller = Message {
            id: ulid::Ulid::new().to_string(),
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "calling".into(),
            }],
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "fs.read".into(),
                input: serde_json::json!({}),
            }],
            tool_call_id: None,
            created_at: time::OffsetDateTime::now_utc(),
            metadata: minicoding_core::model::MessageMeta::default(),
        };
        let tool_res = Message {
            id: ulid::Ulid::new().to_string(),
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                call_id: "c1".to_string() as ToolCallId,
                content: minicoding_core::model::ToolContent::Text("done".into()),
                is_error: false,
                metadata: minicoding_core::model::ToolResultMeta::default(),
            }],
            tool_calls: Vec::new(),
            tool_call_id: Some("c1".into()),
            created_at: time::OffsetDateTime::now_utc(),
            metadata: minicoding_core::model::MessageMeta::default(),
        };

        // 后缀和定位：i=T(4)：6+4=10≤11 → keep=2；i=A(+7)=17>11 → 停。
        // 纯预算 keep_from=2（丢 u+A？不对：keep_from=2 表示丢索引 [0,2)=u,A，
        // 留 T —— 这正是孤儿场景！）。组扩展：组在非 system 子序列的 [1,2]，
        // start=1 < 2 ≤ end=2 → prefix 推到 3，u/A/T 整体丢弃。
        let mut msgs = vec![
            Message::system_text("system"), // 6
            Message::user_text("user"),     // 4
            caller,
            tool_res,
        ];
        let mut result = CompressResult::default();
        hard_truncate(&mut msgs, &tokenizer, &budget, &mut result, Some(4));

        assert_eq!(msgs.len(), 1, "组扩展应把边界推过整个配对组，仅剩 system");
        assert_eq!(result.truncated_count, 3, "u + A + T 整组丢弃");
        // 本测试的 CharTokenizer 只统计 Text 块（T 的 ToolResult 内容不计），
        // 故为 4(u) + 7(A) + 0(T)
        assert_eq!(result.dropped_tokens, 11);
    }
}
