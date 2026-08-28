//! 压缩质量行为回归锁（C3，最小 eval 框架，非 LLM 评分）。
//!
//! 目标：用**确定性语料**（固定内容、固定顺序、无随机源）驱动 4 级压缩管道，
//! 锁定行为不变量——配对完整性、token 降幅区间、seq 追溯区间、启发式兜底
//! 哨兵词、跨运行输出一致（确定性）、L1/L2/L3/L4 各级可达性。全部断言确定性，
//! 不依赖网络与随机数。
//!
//! provider 用 `MockSummaryProvider`（参考 `src/compress/mod.rs` 单测写法），
//! 可切换"恒成功"与"恒失败"两种行为：失败路径验证 C-29 降级链终端的
//! `[heuristic fallback]` 兜底摘要。不引入新依赖。

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};

use minicoding_context::{SummarizeConfig, TokenBudget, compress_pipeline};
use minicoding_core::model::{
    ContentBlock, LlmError, Message, MessageMeta, Role, StopReason, ToolCall, ToolCallId,
    ToolContent,
};
use minicoding_core::provider::{
    BoxFuture, BoxStream, Capabilities, ChatRequest, Delta, LlmProvider, Tokenizer,
};
use time::OffsetDateTime;

/// 工具调用配对组数量（每组 = 1 条 `assistant(tool_calls)` + 1 条 `tool` 结果）。
const PAIRS: usize = 20;
/// 穿插的用户消息条数。语料总数 = 1(system) + PAIRS×2 + USERS = 60。
const USERS: usize = 19;

// ── 确定性语料生成器 ─────────────────────────────────────────────────────────

/// 大体积工具结果：100 行 × ~26 字符 ≈ 2600 字符，必触发 L1 裁剪（默认阈值
/// 2000）。行式结构使裁剪后首部哨兵/元信息保留。
fn big_tool_body(i: usize) -> String {
    (0..100).fold(String::new(), |acc, j| {
        acc + &format!("line{j:03}: module{i} reads offset {j}\n")
    })
}

/// 中/小体积工具结果（不触发 L1），三种尺寸交替形成不同 token 权重分布。
fn tool_body(i: usize) -> String {
    match i % 3 {
        0 => big_tool_body(i),
        1 => "y".repeat(800),
        _ => "z".repeat(300),
    }
}

/// 用户消息：重复段落 + 部分携带 `USERMARK-*` 哨兵（埋入被摘消息供兜底断言）。
fn user_text(i: usize) -> String {
    let lines = i % 6 + 1;
    let sentinel = if i.is_multiple_of(4) {
        format!("USERMARK-U{i:02}-ZEBRA ")
    } else {
        String::new()
    };
    format!("{sentinel}The compiler reports warnings in module {i}.\n").repeat(lines)
}

/// assistant 组头：文本前缀埋 `SENTINEL-G*` 哨兵（配对组任一成员被摘则整组
/// 替换，组头文本随之进入兜底摘要；`tool_result` 内容不进 `text()`，故哨兵
/// 埋在 Text 块）。
fn assistant_head(i: usize) -> Message {
    Message {
        id: ulid::Ulid::new().to_string(),
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text: format!("SENTINEL-G{i:02}-ZEBRA planning read of module {i}"),
        }],
        tool_calls: vec![ToolCall {
            id: format!("call_{i:02}"),
            name: "fs.read".into(),
            input: serde_json::json!({ "path": format!("mod/{i}.rs") }),
        }],
        tool_call_id: None,
        created_at: OffsetDateTime::UNIX_EPOCH,
        metadata: MessageMeta::default(),
    }
}

/// `tool` 结果消息（组成员，`call_id` 与组头一一对应）。
fn tool_result_msg(i: usize) -> Message {
    Message {
        id: ulid::Ulid::new().to_string(),
        role: Role::Tool,
        content: vec![ContentBlock::ToolResult {
            call_id: format!("call_{i:02}"),
            content: ToolContent::Text(tool_body(i)),
            is_error: false,
            metadata: minicoding_core::model::ToolResultMeta::default(),
        }],
        tool_calls: Vec::new(),
        tool_call_id: Some(format!("call_{i:02}")),
        created_at: OffsetDateTime::UNIX_EPOCH,
        metadata: MessageMeta::default(),
    }
}

/// N=60 的确定性语料：system 开头 + 配对组交替穿插用户消息 + 长文本块 +
/// 权重分布差异（角色 base 与消息长度双重差异）。
#[must_use]
pub fn build_corpus() -> Vec<Message> {
    let mut msgs = vec![Message::system_text(
        "You are minicoding, a terminal coding assistant.",
    )];
    for i in 0..PAIRS {
        msgs.push(assistant_head(i));
        msgs.push(tool_result_msg(i));
        if i < USERS {
            msgs.push(Message::user_text(user_text(i)));
        }
    }
    msgs
}

// ── 测试基础设施 ─────────────────────────────────────────────────────────────

/// 按字符数计数的分词器（1 字符 = 1 token，含 `tool_result` 文本内容）。
struct CharTokenizer;

impl Tokenizer for CharTokenizer {
    fn count(&self, text: &str) -> usize {
        text.chars().count()
    }
    fn count_messages(&self, msgs: &[Message]) -> usize {
        msgs.iter()
            .map(|m| {
                let mut total = 0;
                for block in &m.content {
                    match block {
                        ContentBlock::Text { text } => total += text.chars().count(),
                        ContentBlock::ToolResult { content, .. } => {
                            if let ToolContent::Text(t) = content {
                                total += t.chars().count();
                            }
                        }
                        ContentBlock::Image { .. } | ContentBlock::ToolUse(_) => {}
                    }
                }
                total
            })
            .sum()
    }
    fn id(&self) -> &'static str {
        "char-eval"
    }
}

/// mock provider 行为。
enum MockBehavior {
    /// 恒返回固定摘要文本。
    Ok(&'static str),
    /// 恒失败（网络错误）→ 触发 C-29 降级链至启发式兜底。
    Err,
}

/// mock `LlmProvider`（参考 `compress/mod.rs` 测试写法）：可配置成功/失败，
/// 记录调用次数。
struct MockSummaryProvider {
    behavior: MockBehavior,
    call_count: AtomicUsize,
}

impl MockSummaryProvider {
    fn ok(text: &'static str) -> Self {
        Self {
            behavior: MockBehavior::Ok(text),
            call_count: AtomicUsize::new(0),
        }
    }

    fn failing() -> Self {
        Self {
            behavior: MockBehavior::Err,
            call_count: AtomicUsize::new(0),
        }
    }
}

impl LlmProvider for MockSummaryProvider {
    fn id(&self) -> &'static str {
        "mock-summary-eval"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tool_call: false,
            supports_vision: false,
            supports_streaming: true,
            supports_json_mode: false,
            context_window: 4096,
            max_output: 1024,
        }
    }
    fn tokenizer(&self) -> std::sync::Arc<dyn Tokenizer> {
        std::sync::Arc::new(CharTokenizer)
    }
    fn chat_stream(
        &self,
        _req: ChatRequest,
    ) -> BoxFuture<'_, Result<BoxStream<'static, Result<Delta, LlmError>>, LlmError>> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let behavior = &self.behavior;
        Box::pin(async move {
            match behavior {
                MockBehavior::Ok(text) => {
                    let stream = futures::stream::iter(vec![
                        Ok(Delta::Text((*text).to_string())),
                        Ok(Delta::Stop(StopReason::EndTurn)),
                    ]);
                    Ok(Box::pin(stream) as BoxStream<'static, _>)
                }
                MockBehavior::Err => Err(LlmError::Network("mock failure".into())),
            }
        })
    }
    fn count_tokens(&self, messages: &[Message]) -> BoxFuture<'_, usize> {
        let n = messages.len();
        Box::pin(async move { n })
    }
}

/// 主场景预算：threshold = (20000 − 100) × 0.85 ≈ 16915，对 ~30k 语料强制
/// 多级压缩（L1 必触发，L2/L3 视降级链效果跟进）。
fn main_budget() -> TokenBudget {
    TokenBudget {
        context_window: 20_000,
        reserved_output: 100,
        safety_margin: 0,
        ratio: 0.85,
    }
}

/// 跑主场景管道（failing provider → 启发式兜底），返回 `(压缩前 token 数,
/// CompressResult)`；消息就地修改。
async fn run_main_pipeline(
    messages: &mut Vec<Message>,
) -> (usize, minicoding_context::CompressResult) {
    let tokenizer = CharTokenizer;
    let budget = main_budget();
    let before = tokenizer.count_messages(messages);
    let anchor = u64::try_from(before).unwrap_or(u64::MAX);
    let provider = MockSummaryProvider::failing();
    let result = compress_pipeline(
        messages,
        &tokenizer,
        &budget,
        Some(&provider),
        Some(anchor),
        &SummarizeConfig::default(),
    )
    .await
    .expect("降级链终端恒成功，管道不应失败");
    (before, result)
}

/// 输出指纹：`(role, 归一化全文)` 序列。归一化剥离摘要消息内嵌的 RFC3339
/// 时间戳（`[summarized @ <ts>]`）——时间戳非管道决策产物，剥离后两次运行
/// 输出应逐字节一致。
#[must_use]
fn fingerprint(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|m| {
            let full = m.full_text();
            let normalized = match full.strip_prefix("[summarized @") {
                Some(rest) => match rest.find("]\n") {
                    Some(pos) => format!("[summarized]{}", &rest[pos + 1..]),
                    None => full,
                },
                None => full,
            };
            format!("{:?}|{normalized}", m.role)
        })
        .collect()
}

/// 断言配对完整性不变式：每个 `assistant(tool_calls)` 与其后紧随的连续
/// `Role::Tool` 结果构成原子组——所有 `call_id` 得到结果、无孤儿结果、组不被
/// 其他角色打断（与 `compress::tool_group` 的组定义一致的手写校验）。
fn assert_pairing_invariant(messages: &[Message]) {
    let mut pending: Vec<String> = Vec::new();
    for m in messages {
        match m.role {
            Role::Assistant => {
                assert!(
                    pending.is_empty(),
                    "新 assistant 出现时存在未闭合的 tool_calls: {pending:?}"
                );
                pending.extend(m.tool_calls.iter().map(|c| c.id.clone()));
            }
            Role::Tool => {
                let tid = m
                    .tool_call_id
                    .as_ref()
                    .expect("Role::Tool 消息必有 tool_call_id");
                let pos = pending
                    .iter()
                    .position(|x| x == tid)
                    .unwrap_or_else(|| panic!("孤儿 tool_result（无对应 tool_use）: {tid}"));
                pending.remove(pos);
            }
            Role::User | Role::System => {
                assert!(
                    pending.is_empty(),
                    "非 tool 消息打断了未闭合的配对组: {pending:?}"
                );
            }
        }
    }
    assert!(
        pending.is_empty(),
        "悬空 tool_calls 未得到 tool_result: {pending:?}"
    );
}

/// 提取全部压缩产物上的 `CompressedRange`（按 `from_seq` 升序）。
fn compressed_ranges(messages: &[Message]) -> Vec<minicoding_core::model::CompressedRange> {
    let mut ranges: Vec<_> = messages
        .iter()
        .filter_map(|m| m.metadata.compressed_range.clone())
        .collect();
    ranges.sort_by_key(|r| r.from_seq);
    ranges
}

// ── 断言 (1)：配对完整性不变式 ───────────────────────────────────────────────

#[tokio::test]
async fn eval_pairing_invariant_holds_after_compression() {
    let mut msgs = build_corpus();
    assert_pairing_invariant(&msgs); // 语料本身合法
    let (_, result) = run_main_pipeline(&mut msgs).await;
    assert!(
        result.summarized_count > 0 || result.dropped_count > 0 || result.truncated_count > 0,
        "语料应触发实质性压缩"
    );
    assert_pairing_invariant(&msgs);
}

// ── 断言 (2)：token 降幅落在 [30%, 95%] ─────────────────────────────────────

#[tokio::test]
async fn eval_token_reduction_within_band() {
    let mut msgs = build_corpus();
    let tokenizer = CharTokenizer;
    let (before, _) = run_main_pipeline(&mut msgs).await;
    let after = tokenizer.count_messages(&msgs);
    // design.md §3.3 注明的 cast 场景：token 计数远小于 f64 尾数精度
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let reduction = 1.0 - (after as f64 / before as f64);
    assert!(
        (0.30..=0.95).contains(&reduction),
        "降幅应在 [30%, 95%]: before={before} after={after} reduction={reduction:.3}"
    );
}

// ── 断言 (3)：seq 区间单调且 dropped_tokens > 0 ─────────────────────────────

#[tokio::test]
async fn eval_compressed_ranges_monotonic_and_positive() {
    let mut msgs = build_corpus();
    let (_, result) = run_main_pipeline(&mut msgs).await;
    let ranges = compressed_ranges(&msgs);
    assert!(!ranges.is_empty(), "压缩产物应携带 CompressedRange");
    let mut prev_end: Option<u64> = None;
    for r in &ranges {
        assert!(r.from_seq <= r.to_seq, "区间应单调: {r:?}");
        assert!(r.dropped_tokens > 0, "dropped_tokens 应为正: {r:?}");
        if let Some(prev) = prev_end {
            assert!(
                r.from_seq > prev,
                "多区间应升序且不重叠: prev_end={prev} next={r:?}"
            );
        }
        prev_end = Some(r.to_seq);
    }
    // L3/L4 侧的丢弃区间同样满足 from<=to 且 dropped_tokens>0
    if let Some((from, to)) = result.dropped_range {
        assert!(from <= to, "dropped_range 应单调: ({from},{to})");
        assert!(result.dropped_tokens > 0, "dropped_tokens 应为正");
    }
}

// ── 断言 (4)：启发式兜底标记 + 被摘消息哨兵词 ───────────────────────────────

#[tokio::test]
async fn eval_heuristic_fallback_marker_and_sentinels() {
    let mut msgs = build_corpus();
    let (_, result) = run_main_pipeline(&mut msgs).await;
    assert!(result.fallback_used, "failing provider 应回降启发式兜底");

    let summaries: Vec<&Message> = msgs.iter().filter(|m| m.metadata.summarized).collect();
    assert!(!summaries.is_empty(), "应有摘要替换消息");
    let joined: String = summaries
        .iter()
        .map(|m| m.text())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("[heuristic fallback]"),
        "兜底摘要应含标记: {joined}"
    );

    // 被摘配对组的组头哨兵应出现在兜底摘要中（每条消息取前 200 字符，
    // 哨兵位于首 40 字符内必存活）。权重选取确定 ⇒ 被摘集合确定；
    // ratio=0.5 下低权重的老配对组大量入选，取 ≥5 作为稳健下界。
    let found: HashSet<String> = joined
        .split_whitespace()
        .filter(|w| w.starts_with("SENTINEL-G") && w.ends_with("-ZEBRA"))
        .map(str::to_string)
        .collect();
    assert!(
        found.len() >= 5,
        "兜底摘要应含 ≥5 个被摘组头哨兵词: found={found:?}"
    );
}

// ── 断言 (5)：多次运行输出完全一致（确定性锁）────────────────────────────────

/// 单次完整运行：独立构造语料与 provider 实例，返回压缩统计 + 输出指纹。
async fn run_deterministic_once() -> (minicoding_context::CompressResult, Vec<String>) {
    let tokenizer = CharTokenizer;
    let budget = main_budget();
    let summarize_cfg = SummarizeConfig::default();
    let provider = MockSummaryProvider::failing();
    let mut msgs = build_corpus();
    let result = compress_pipeline(
        &mut msgs,
        &tokenizer,
        &budget,
        Some(&provider),
        Some(60),
        &summarize_cfg,
    )
    .await
    .expect("降级链终端恒成功，管道不应失败");
    (result, fingerprint(&msgs))
}

#[tokio::test]
async fn eval_output_deterministic_across_runs() {
    // 两轮各自独立构造语料与 provider 实例，排除实例状态影响
    let (result1, f1) = run_deterministic_once().await;
    let (result2, f2) = run_deterministic_once().await;

    assert_eq!(f1, f2, "同构输入两次运行的输出指纹必须完全一致");
    assert_eq!(
        result1.summarized_count, result2.summarized_count,
        "压缩统计也应确定"
    );
    assert_eq!(result1.fallback_used, result2.fallback_used);
    assert_eq!(result1.clipped_count, result2.clipped_count);
}

// ── 断言 (6)：L1/L2/L3/L4 各级可达性冒烟 ────────────────────────────────────

/// L1 单独可达：单条超大 `tool_result`，大窗口下裁剪即达标，后续级别不触发。
#[tokio::test]
async fn smoke_l1_clip_reachable_alone() {
    let tokenizer = CharTokenizer;
    // threshold = (10_000 − 100) × 0.85 = 8415；9000 字符触发 L1，裁剪后达标
    let budget = TokenBudget {
        context_window: 10_000,
        reserved_output: 100,
        safety_margin: 0,
        ratio: 0.85,
    };
    let mut msgs = vec![Message {
        id: ulid::Ulid::new().to_string(),
        role: Role::Tool,
        content: vec![ContentBlock::ToolResult {
            call_id: String::new() as ToolCallId,
            content: ToolContent::Text("x".repeat(9_000)),
            is_error: false,
            metadata: minicoding_core::model::ToolResultMeta::default(),
        }],
        tool_calls: Vec::new(),
        tool_call_id: None,
        created_at: OffsetDateTime::UNIX_EPOCH,
        metadata: MessageMeta::default(),
    }];
    let provider = MockSummaryProvider::ok("short summary");
    let result = compress_pipeline(
        &mut msgs,
        &tokenizer,
        &budget,
        Some(&provider),
        None,
        &SummarizeConfig::default(),
    )
    .await
    .expect("pipeline 应成功");
    assert!(result.clipped_count > 0, "L1 应裁剪");
    assert_eq!(result.summarized_count, 0, "L2 不应触发");
    assert_eq!(result.dropped_count, 0, "L3 不应触发");
    assert_eq!(result.truncated_count, 0, "L4 不应触发");
}

/// L2 可达：超阈值 + provider 成功 → 摘要替换生效，无需 L3/L4。
#[tokio::test]
async fn smoke_l2_summarize_reachable() {
    let tokenizer = CharTokenizer;
    // threshold = (6_000 − 100) × 0.85 = 5015；10 × 600 = 6000 超阈值
    let budget = TokenBudget {
        context_window: 6_000,
        reserved_output: 100,
        safety_margin: 0,
        ratio: 0.85,
    };
    let mut msgs: Vec<Message> = (0..10)
        .map(|i| Message::user_text(format!("user message {i} {}", "x".repeat(570))))
        .collect();
    let provider = MockSummaryProvider::ok("short summary");
    let result = compress_pipeline(
        &mut msgs,
        &tokenizer,
        &budget,
        Some(&provider),
        Some(10),
        &SummarizeConfig::default(),
    )
    .await
    .expect("pipeline 应成功");
    assert!(result.summarized_count > 0, "L2 应摘要替换");
    assert!(!result.fallback_used, "provider 成功不应降级");
    assert_eq!(result.dropped_count, 0, "L3 不应触发");
    assert_eq!(result.truncated_count, 0, "L4 不应触发");
    assert!(
        tokenizer.count_messages(&msgs) <= budget.compact_threshold(),
        "L2 后应降至阈值下"
    );
}

/// L3 可达：无 provider（跳过 L2）+ 中等超出 → 滚动窗口丢最旧 10 条。
#[tokio::test]
async fn smoke_l3_rolling_reachable() {
    let tokenizer = CharTokenizer;
    // threshold = 5015；30 × 200 = 6000 超阈值
    let budget = TokenBudget {
        context_window: 6_000,
        reserved_output: 100,
        safety_margin: 0,
        ratio: 0.85,
    };
    let mut msgs: Vec<Message> = (0..30)
        .map(|_| Message::user_text("x".repeat(200)))
        .collect();
    let result = compress_pipeline(
        &mut msgs,
        &tokenizer,
        &budget,
        None,
        Some(30),
        &SummarizeConfig::default(),
    )
    .await
    .expect("pipeline 应成功");
    assert_eq!(result.summarized_count, 0, "无 provider 时 L2 跳过");
    assert_eq!(result.dropped_count, 10, "L3 应丢弃最旧 10 条");
    assert_eq!(result.truncated_count, 0, "L4 不应触发");
    assert_eq!(result.dropped_range, Some((1, 10)), "L3 应记录丢弃区间");
    assert!(result.dropped_tokens > 0);
}

/// L4 可达：极小窗口下 L3 保留 20 条仍超阈值 → 硬截断兜底收敛。
#[tokio::test]
async fn smoke_l4_hard_truncate_reachable() {
    let tokenizer = CharTokenizer;
    // threshold = 200 × 0.85 = 170；30 × 10 = 300，L3 后 200 仍超阈值
    let budget = TokenBudget {
        context_window: 200,
        reserved_output: 0,
        safety_margin: 0,
        ratio: 0.85,
    };
    let mut msgs: Vec<Message> = (0..30).map(|_| Message::user_text("0123456789")).collect();
    let result = compress_pipeline(
        &mut msgs,
        &tokenizer,
        &budget,
        None,
        Some(30),
        &SummarizeConfig::default(),
    )
    .await
    .expect("pipeline 应成功");
    assert!(result.truncated_count > 0, "L4 应硬截断");
    assert!(
        tokenizer.count_messages(&msgs) <= budget.compact_threshold(),
        "L4 后应降至阈值下"
    );
}
