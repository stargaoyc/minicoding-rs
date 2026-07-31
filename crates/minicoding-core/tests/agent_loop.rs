//! M1 集成测试：mock provider 跑通单轮对话（见 dev-plan T-M1-9 验收标准）。

mod common;

use std::sync::Arc;

use camino::Utf8PathBuf;
use minicoding_core::config::RuntimeConfig;
use minicoding_core::model::{StopReason, TurnOutcome, UserInput};
use minicoding_core::runtime::{Event, Runtime, RuntimeBuilder};
use minicoding_core::tool::ToolRegistry;

use common::{
    InMemoryStorage, MockTool, ScriptedProvider, TestContext, text_deltas, tool_call_deltas,
};

/// 构造测试用 Runtime：注入 mock provider、内存存储、空工具表。
fn build_runtime(provider: ScriptedProvider, tools: ToolRegistry) -> Runtime {
    RuntimeBuilder::new()
        .provider(Arc::new(provider))
        .context(Arc::new(TestContext::new("test system prompt")))
        .storage(Arc::new(InMemoryStorage::new()))
        .tools(tools)
        .config(RuntimeConfig::default())
        .workdir(Utf8PathBuf::from("."))
        .build()
        .expect("runtime build")
}

/// 场景 1：纯文本回复，无工具调用 → 单次迭代即终止。
#[tokio::test]
async fn single_turn_text_only() {
    let provider = ScriptedProvider::new(vec![text_deltas("Hello, world!")]);
    let rt = build_runtime(provider, ToolRegistry::new());

    let outcome = rt.run_turn(UserInput::from_text("hi")).await;
    let outcome = outcome.expect("turn should succeed");

    match outcome {
        TurnOutcome::Finished(msg) => {
            assert_eq!(msg.text(), "Hello, world!");
            assert!(msg.tool_calls.is_empty());
        }
        other => panic!("expected Finished, got {other:?}"),
    }
}

/// 场景 2：模型先调用工具，回灌后给出最终文本回复（两轮迭代）。
#[tokio::test]
async fn single_turn_with_tool_call() {
    let tool = Arc::new(MockTool::read_only("echo", "echo:42"));
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone());

    // 第 1 次调用：返回工具调用；第 2 次：返回最终文本
    let provider = ScriptedProvider::new(vec![
        tool_call_deltas("call_1", "echo", r#"{"msg":"hi"}"#),
        text_deltas("got echo:42"),
    ]);
    let rt = build_runtime(provider, tools);

    let outcome = rt.run_turn(UserInput::from_text("call echo")).await;
    let outcome = outcome.expect("turn should succeed");

    match outcome {
        TurnOutcome::Finished(msg) => {
            assert_eq!(msg.text(), "got echo:42");
            assert!(
                msg.tool_calls.is_empty(),
                "final message should have no tool calls"
            );
        }
        other => panic!("expected Finished, got {other:?}"),
    }

    // 验证工具确实被调用，且入参正确
    let calls = tool.take_calls();
    assert_eq!(calls.len(), 1, "tool should be called exactly once");
    assert_eq!(calls[0]["msg"], "hi");
}

/// 场景 3：Token 事件按顺序广播，订阅者能收到流式增量。
#[tokio::test]
async fn token_events_broadcast() {
    let provider = ScriptedProvider::new(vec![vec![
        minicoding_core::provider::Delta::Text("abc".into()),
        minicoding_core::provider::Delta::Text("def".into()),
        minicoding_core::provider::Delta::Stop(StopReason::EndTurn),
    ]]);
    let rt = build_runtime(provider, ToolRegistry::new());

    // 在 spawn 任务中收集事件（receiver 是 'static，可 move）
    let mut rx = rt.events().subscribe();
    let collector = tokio::spawn(async move {
        let mut tokens = Vec::new();
        while let Ok(ev) = rx.recv().await {
            match ev {
                Event::Token(s) => tokens.push(s),
                Event::TurnEnd { .. } => break,
                _ => {}
            }
        }
        tokens
    });

    // 主任务直接驱动 turn
    let _ = rt.run_turn(UserInput::from_text("hi")).await;
    let tokens = collector.await.expect("collector task panicked");

    assert_eq!(tokens, vec!["abc".to_string(), "def".to_string()]);
}

/// 场景 4：多个无副作用工具在同一轮可被并发调度（此处仅验证均被执行）。
#[tokio::test]
async fn multiple_readonly_tools_executed() {
    let tool_a = Arc::new(MockTool::read_only("a", "A"));
    let tool_b = Arc::new(MockTool::read_only("b", "B"));
    let mut tools = ToolRegistry::new();
    tools.register(tool_a.clone());
    tools.register(tool_b.clone());

    // 单次 LLM 响应里包含两个工具调用
    let provider = ScriptedProvider::new(vec![
        vec![
            minicoding_core::provider::Delta::ToolCall(minicoding_core::provider::ToolCallDelta {
                index: 0,
                id: Some("c1".into()),
                name: Some("a".into()),
                args_chunk: Some(r#"{}"#.into()),
            }),
            minicoding_core::provider::Delta::ToolCall(minicoding_core::provider::ToolCallDelta {
                index: 1,
                id: Some("c2".into()),
                name: Some("b".into()),
                args_chunk: Some(r#"{}"#.into()),
            }),
            minicoding_core::provider::Delta::Stop(StopReason::ToolUse),
        ],
        text_deltas("done"),
    ]);
    let rt = build_runtime(provider, tools);

    let outcome = rt.run_turn(UserInput::from_text("call both")).await;
    match outcome.expect("turn should succeed") {
        TurnOutcome::Finished(msg) => assert_eq!(msg.text(), "done"),
        other => panic!("expected Finished, got {other:?}"),
    }

    assert_eq!(tool_a.take_calls().len(), 1);
    assert_eq!(tool_b.take_calls().len(), 1);
}
