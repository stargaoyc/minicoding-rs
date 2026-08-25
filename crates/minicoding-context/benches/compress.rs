//! 压缩管道性能基准（Q-06 criterion）。
//!
//! 对 `compress_pipeline` 在不同消息规模（100/500/1000 条）下的执行耗时采样。
//! 使用按字符计数的 `CharTokenizer`（1 字符 = 1 token）避免 tiktoken 词表加载开销，
//! 隔离压缩算法本身的性能。无 `LlmProvider`（`None`）跳过 L2 摘要，仅跑 L1/L3/L4。

use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use minicoding_context::TokenBudget;
use minicoding_context::compress_pipeline;
use minicoding_core::model::{ContentBlock, Message, MessageMeta, MessageSource, Role};
use minicoding_core::provider::Tokenizer;

/// 按字符计数的分词器（1 字符 = 1 token），隔离压缩算法性能。
struct CharTokenizer;

impl Tokenizer for CharTokenizer {
    fn count(&self, text: &str) -> usize {
        text.chars().count()
    }
    fn count_messages(&self, msgs: &[Message]) -> usize {
        msgs.iter()
            .map(|m| {
                m.content
                    .iter()
                    .map(|block| match block {
                        ContentBlock::Text { text } => text.chars().count(),
                        ContentBlock::ToolResult { content, .. } => match content {
                            minicoding_core::model::ToolContent::Text(t) => t.chars().count(),
                            _ => 0,
                        },
                        ContentBlock::Image { .. } | ContentBlock::ToolUse(_) => 0,
                    })
                    .sum::<usize>()
            })
            .sum()
    }
    fn id(&self) -> &'static str {
        "char-bench"
    }
}

/// 生成 `n` 条交替 user/assistant 文本消息，每条 200 字符（200 token）。
fn make_messages(n: usize) -> Vec<Message> {
    let body = "x".repeat(200);
    (0..n)
        .map(|i| Message {
            id: format!("msg-{i}"),
            role: if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            },
            content: vec![ContentBlock::Text { text: body.clone() }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            created_at: time::OffsetDateTime::now_utc(),
            metadata: MessageMeta {
                source: if i % 2 == 0 {
                    MessageSource::User
                } else {
                    MessageSource::Llm
                },
                ..Default::default()
            },
        })
        .collect()
}

fn bench_compress(c: &mut Criterion) {
    let tokenizer = CharTokenizer;
    // threshold = (6000 - 100) * 0.85 = 5015；n × 200 token > 5015 时触发压缩
    let budget = TokenBudget {
        context_window: 6_000,
        reserved_output: 100,
        safety_margin: 0,
    };
    let runtime = tokio::runtime::Runtime::new().expect("create tokio runtime for bench");

    let mut group = c.benchmark_group("compress_pipeline");
    for n in [100, 500, 1000] {
        let messages = make_messages(n);
        group.bench_with_input(format!("{n}_msgs").as_str(), &n, |b, _| {
            b.iter_batched(
                || messages.clone(),
                |mut msgs| {
                    runtime.block_on(async {
                        compress_pipeline(
                            black_box(&mut msgs),
                            black_box(&tokenizer),
                            black_box(&budget),
                            None,
                            false,
                            None,
                            &minicoding_context::SummarizeConfig::default(),
                        )
                        .await
                        .expect("compress_pipeline should succeed");
                    });
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_compress);
criterion_main!(benches);
