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
    build_runtime_with_prompter(provider, tools, Arc::new(DenyPrompter))
}

/// 构造测试用 Runtime（可指定 prompter，供 switch_workdir 权限路径测试）。
fn build_runtime_with_prompter(
    provider: ScriptedProvider,
    tools: ToolRegistry,
    prompter: Arc<dyn minicoding_core::policy::PermissionPrompter>,
) -> Runtime {
    RuntimeBuilder::new()
        .provider(Arc::new(provider))
        .context(Arc::new(TestContext::new("test system prompt")))
        .storage(Arc::new(InMemoryStorage::new()))
        .tools(tools)
        .prompter(prompter)
        .config(RuntimeConfig::default())
        .workdir(Utf8PathBuf::from("."))
        .build()
        .expect("runtime build")
}

/// 恒拒绝的 prompter（默认测试策略：权限全部 Deny）。
#[derive(Default)]
struct DenyPrompter;

impl minicoding_core::policy::PermissionPrompter for DenyPrompter {
    fn prompt(
        &self,
        _p: minicoding_core::policy::PermissionPrompt,
    ) -> minicoding_core::provider::BoxFuture<'_, minicoding_core::policy::Decision> {
        Box::pin(async {
            minicoding_core::policy::Decision::Deny("test denies by default".to_string())
        })
    }
}

/// 恒允许的 prompter（供 switch_workdir Allow 路径测试）。
#[derive(Default)]
struct AllowPrompter;

impl minicoding_core::policy::PermissionPrompter for AllowPrompter {
    fn prompt(
        &self,
        _p: minicoding_core::policy::PermissionPrompt,
    ) -> minicoding_core::provider::BoxFuture<'_, minicoding_core::policy::Decision> {
        Box::pin(async { minicoding_core::policy::Decision::Allow })
    }
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
            assert!(msg.tool_calls.is_empty(), "expected empty: msg.tool_calls");
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

// ─── workspace.switch（W-11）：目录校验 + 权限路径 ───────────────────────────

/// 场景 5：目标目录不存在 → 立即报错（不进入权限弹窗等待）。
#[tokio::test]
async fn switch_workdir_nonexistent_dir_errors() {
    let rt = build_runtime(ScriptedProvider::new(vec![]), ToolRegistry::new());
    let target = Utf8PathBuf::from("/definitely/nonexistent/minicoding-test-xyz");
    let res = rt.switch_workdir(&target).await;
    assert!(res.is_err(), "切换到不存在的目录应报错而非等待审批");
}

/// 场景 6：目标是文件而非目录 → 报错。
#[tokio::test]
async fn switch_workdir_file_target_errors() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let file = tmp.path().join("afile.txt");
    std::fs::write(&file, b"x").expect("write fixture");
    let rt = build_runtime(ScriptedProvider::new(vec![]), ToolRegistry::new());
    let target = camino::Utf8PathBuf::from_path_buf(file).expect("utf8 path");
    assert!(
        rt.switch_workdir(&target).await.is_err(),
        "目标是文件应报错"
    );
}

/// 场景 7：用户拒绝（Deny）→ `Ok(false)`，workdir 保持不变。
#[tokio::test]
async fn switch_workdir_denied_keeps_workdir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("utf8 path");
    let rt = build_runtime(ScriptedProvider::new(vec![]), ToolRegistry::new());
    assert_eq!(rt.workdir().await, Utf8PathBuf::from("."));

    let switched = rt.switch_workdir(&dir).await.expect("switch returns Ok");
    assert!(!switched, "Deny 后不应切换");
    assert_eq!(
        rt.workdir().await,
        Utf8PathBuf::from("."),
        "workdir 应保持不变"
    );
}

/// 场景 8：用户允许（Allow）→ `Ok(true)`，workdir 更新为 canonical 后的路径。
#[tokio::test]
async fn switch_workdir_allowed_updates_workdir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("utf8 path");
    let rt = build_runtime_with_prompter(
        ScriptedProvider::new(vec![]),
        ToolRegistry::new(),
        Arc::new(AllowPrompter),
    );

    let switched = rt.switch_workdir(&dir).await.expect("switch returns Ok");
    assert!(switched, "Allow 后应切换成功");
    // canonicalize 会规范化路径（如 /tmp → /private/tmp），断言指向同一目录
    let canonical = std::fs::canonicalize(tmp.path()).expect("canonicalize");
    assert_eq!(
        rt.workdir().await.as_std_path(),
        canonical.as_path(),
        "workdir 应为 canonical 路径"
    );
}

/// 场景 9（回归，v0.2.27→0.2.28）：cancel 后会话不"砖化"。
///
/// 历史 bug：`CancellationToken` 一旦 cancel 永久 cancelled，一次手动终止后
/// 后续所有 turn 全部秒取消（Interrupted），用户无法再与 AI 对话。修复：
/// 每轮 `run_turn` 结束（含取消）重建 token。
#[tokio::test]
async fn cancel_then_next_turn_still_works() {
    use minicoding_core::model::{ContentBlock, Role};
    use minicoding_core::model::{ToolError, ToolResult, ToolSchema};
    use minicoding_core::tool::{Tool, ToolContext};

    /// 挂起工具：execute 阻塞直到超时（cancel 时 future 被 drop，无副作用）。
    struct HangingTool;
    impl Tool for HangingTool {
        fn name(&self) -> &str {
            "hang"
        }
        fn schema(&self) -> &ToolSchema {
            static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
            SCHEMA.get_or_init(|| ToolSchema {
                name: "hang".into(),
                description: "hang".into(),
                input_schema: serde_json::json!({"type": "object"}),
            })
        }
        fn side_effect(&self) -> minicoding_core::model::SideEffect {
            minicoding_core::model::SideEffect::None
        }
        fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &ToolContext,
        ) -> minicoding_core::provider::BoxFuture<'_, Result<ToolResult, ToolError>> {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                Ok(ToolResult::ok_text("done"))
            })
        }
    }

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(HangingTool));
    // turn 1：工具调用（挂起等待取消）；turn 2：正常文本回复。
    let provider = ScriptedProvider::new(vec![
        tool_call_deltas("c1", "hang", "{}"),
        text_deltas("第二次对话正常回复"),
    ]);
    let rt = Arc::new(build_runtime(provider, tools));

    // turn 1：spawn 后等工具进入挂起，再取消
    let rt2 = Arc::clone(&rt);
    let turn1 = tokio::spawn(async move {
        rt2.run_turn(UserInput::from_text("第一次：开始任务")).await
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    rt.cancel();
    let outcome1 = turn1.await.expect("turn1 join");
    let TurnOutcome::Interrupted(_) = outcome1.expect("turn1 ok") else {
        panic!("cancel 后应返回 Interrupted");
    };

    // turn 2：若 token 未重建（旧 bug），立即秒取消；修复后应正常完成
    let outcome2 = rt
        .run_turn(UserInput::from_text("第二次：继续"))
        .await
        .expect("turn2 ok");
    match outcome2 {
        TurnOutcome::Finished(msg) => {
            assert!(matches!(msg.role, Role::Assistant));
            let has_reply = msg.content.iter().any(|c| match c {
                ContentBlock::Text { text } => text.contains("第二次对话正常回复"),
                _ => false,
            });
            assert!(has_reply, "第二次 turn 应正常完成而非被取消: {msg:?}");
        }
        TurnOutcome::Interrupted(_) => {
            panic!("第二次 turn 不应被取消（cancel token 应已重建）")
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

/// M-03（D-05）：cancel 中断时悬空 tool_calls 被回填合成错误结果。
///
/// assistant 消息（含 tool_calls）落盘后、tool_result 落盘前 cancel → 每个
/// tool_call 都应有对应 Tool 消息（合成 is_error=true）。resume 后历史对严格
/// provider（Anthropic 要求 tool_use 必有 tool_result）合法。
#[tokio::test]
async fn cancel_mid_tool_backfills_synthetic_results() {
    use minicoding_core::model::{ContentBlock, Role, ToolError, ToolResult, ToolSchema};
    use minicoding_core::tool::{Tool, ToolContext};

    struct HangingTool;
    impl Tool for HangingTool {
        fn name(&self) -> &str {
            "hang"
        }
        fn schema(&self) -> &ToolSchema {
            static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
            SCHEMA.get_or_init(|| ToolSchema {
                name: "hang".into(),
                description: "hang".into(),
                input_schema: serde_json::json!({"type": "object"}),
            })
        }
        fn side_effect(&self) -> minicoding_core::model::SideEffect {
            minicoding_core::model::SideEffect::None
        }
        fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &ToolContext,
        ) -> minicoding_core::provider::BoxFuture<'_, Result<ToolResult, ToolError>> {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                Ok(ToolResult::ok_text("done"))
            })
        }
    }

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(HangingTool));
    // 唯一脚本：assistant 请求 hang 工具。第二迭代脚本耗尽会 Err，但 cancel
    // 在工具挂起期间触发，不会走到第二迭代。
    let provider = ScriptedProvider::new(vec![tool_call_deltas("c1", "hang", "{}")]);
    let rt = Arc::new(build_runtime(provider, tools));

    let rt2 = Arc::clone(&rt);
    let turn = tokio::spawn(async move { rt2.run_turn(UserInput::from_text("开始")).await });
    // 等 assistant（含 tool_calls）落盘 + 工具挂起
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    rt.cancel();
    let outcome = turn.await.expect("turn join").expect("turn ok");
    assert!(matches!(outcome, TurnOutcome::Interrupted(_)));

    // 校验：历史中 assistant 的每个 tool_call 都有对应 Tool 结果（合成）
    // 运行期消息只在 storage/ctx（session.messages 仅含预加载历史）
    let msgs = rt.storage().load(&rt.session().id.clone()).await.unwrap();
    let asst = msgs
        .iter()
        .find(|m| matches!(m.role, Role::Assistant) && !m.tool_calls.is_empty())
        .expect("assistant with tool_calls should exist");
    let answered: Vec<String> = msgs
        .iter()
        .filter(|m| matches!(m.role, Role::Tool))
        .filter_map(|m| {
            m.content.iter().find_map(|b| {
                if let ContentBlock::ToolResult { call_id, .. } = b {
                    Some(call_id.clone())
                } else {
                    None
                }
            })
        })
        .collect();
    for call in &asst.tool_calls {
        assert!(
            answered.contains(&call.id),
            "tool_call {} 缺 tool_result（M-03 应回填）",
            call.id
        );
    }
}

/// 场景 10（回归）：turn 间隙调用 `cancel()`（无 turn 运行）不毒化下一轮。
///
/// `cancel()` 仅对运行中的 turn 生效（`turn_active` 检查）；否则"取消按钮
/// 在 turn 已结束后被点击"会取消掉下一轮的开场 token，导致下一条消息
/// 秒取消。
#[tokio::test]
async fn cancel_between_turns_is_ignored() {
    let provider = ScriptedProvider::new(vec![
        text_deltas("第一轮正常回复"),
        text_deltas("第二轮正常回复"),
    ]);
    let rt = build_runtime(provider, ToolRegistry::new());

    let outcome1 = rt
        .run_turn(UserInput::from_text("第一轮"))
        .await
        .expect("turn1 ok");
    assert!(matches!(outcome1, TurnOutcome::Finished(_)));

    // turn 间隙取消：不应影响下一轮
    rt.cancel();

    let outcome2 = rt
        .run_turn(UserInput::from_text("第二轮"))
        .await
        .expect("turn2 ok");
    assert!(
        matches!(outcome2, TurnOutcome::Finished(_)),
        "间隙 cancel 不应取消下一轮: {outcome2:?}"
    );
}
