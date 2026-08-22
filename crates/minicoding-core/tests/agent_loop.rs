//! M1 集成测试：mock provider 跑通单轮对话（见 dev-plan T-M1-9 验收标准）。

mod common;

use std::sync::Arc;

use camino::Utf8PathBuf;
use minicoding_core::config::RuntimeConfig;
use minicoding_core::model::{StopReason, TurnOutcome, UserInput};
use minicoding_core::runtime::{Event, Runtime, RuntimeBuilder};
use minicoding_core::tool::ToolRegistry;

use common::{
    InMemoryEventStore, InMemoryStorage, MockTool, ScriptedProvider, TestContext, text_deltas,
    tool_call_deltas,
};

/// 构造测试用 Runtime：注入 mock provider、内存存储、空工具表。
fn build_runtime(provider: ScriptedProvider, tools: ToolRegistry) -> Runtime {
    build_runtime_with_prompter(provider, tools, Arc::new(DenyPrompter))
}

/// 构造测试用 Runtime（指定 config，供 M-08 重复阈值配置路径测试）。
fn build_runtime_with_config(
    provider: ScriptedProvider,
    tools: ToolRegistry,
    config: RuntimeConfig,
) -> Runtime {
    RuntimeBuilder::new()
        .provider(Arc::new(provider))
        .context(Arc::new(TestContext::new("test system prompt")))
        .storage(Arc::new(InMemoryStorage::new()))
        .tools(tools)
        .prompter(Arc::new(DenyPrompter))
        .config(config)
        .workdir(Utf8PathBuf::from("."))
        .build()
        .expect("runtime build")
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

/// 构造测试用 Runtime（注入内存事件存储，M-06 step 事件断言用）。
fn build_runtime_with_event_store(
    provider: ScriptedProvider,
    tools: ToolRegistry,
    event_store: Arc<InMemoryEventStore>,
) -> Runtime {
    RuntimeBuilder::new()
        .provider(Arc::new(provider))
        .context(Arc::new(TestContext::new("test system prompt")))
        .storage(Arc::new(InMemoryStorage::new()))
        .tools(tools)
        .prompter(Arc::new(DenyPrompter))
        .event_store(event_store)
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

/// M-08（R-03）：重复工具调用软提醒 + 硬停止。
///
/// 默认阈值 [3,5,8]：单工具指纹连续 3 轮注入 system 提醒（不终止），连续 8 轮整轮
/// 签名相同才硬停止；不同 input 重置连续计数；空阈值数组关闭软提醒仅保留硬停止。
#[tokio::test]
async fn repeat_3_times_injects_soft_reminder() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(MockTool::read_only("greet", "hi")));
    // 3 轮相同工具调用（c1..c3 同 input，指纹相同）+ 第 4 轮文本结束
    let mut script = vec![
        tool_call_deltas("c1", "greet", "{}"),
        tool_call_deltas("c2", "greet", "{}"),
        tool_call_deltas("c3", "greet", "{}"),
    ];
    script.push(text_deltas("done"));
    let rt = Arc::new(build_runtime(ScriptedProvider::new(script), tools));

    let outcome = rt.run_turn(UserInput::from_text("loop")).await.unwrap();
    assert!(
        matches!(outcome, TurnOutcome::Finished(_)),
        "3 轮相同工具不应硬停止（阈值 8）"
    );

    let snap = rt.context().snapshot().await;
    let has_reminder = snap
        .messages
        .iter()
        .any(|m| m.text().contains("[系统提醒]"));
    assert!(
        has_reminder,
        "第 3 轮应注入 system 软提醒: {:?}",
        snap.messages.iter().map(|m| m.text()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn repeat_8_times_hard_stops() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(MockTool::read_only("greet", "hi")));
    // 8 轮相同工具调用 → 第 8 轮整轮签名连续 8 次 ≥ 末级阈值 → Stopped
    let script: Vec<Vec<minicoding_core::provider::Delta>> = (0..8)
        .map(|i| tool_call_deltas(&format!("c{i}"), "greet", "{}"))
        .collect();
    let rt = Arc::new(build_runtime(ScriptedProvider::new(script), tools));

    let outcome = rt.run_turn(UserInput::from_text("loop")).await.unwrap();
    match outcome {
        TurnOutcome::Finished(msg) => {
            assert!(
                msg.text().contains("重复工具调用"),
                "应硬停止: {:?}",
                msg.text()
            );
        }
        other => panic!("应返回 Finished(Stopped 消息)，实际 {other:?}"),
    }
}

#[tokio::test]
async fn different_args_resets_streak() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(MockTool::read_only("greet", "hi")));
    // 同一工具不同 input → 指纹不同，3 轮不触发提醒
    let mut script = vec![
        tool_call_deltas("c1", "greet", r#"{"name":"a"}"#),
        tool_call_deltas("c2", "greet", r#"{"name":"b"}"#),
        tool_call_deltas("c3", "greet", r#"{"name":"c"}"#),
    ];
    script.push(text_deltas("done"));
    let rt = Arc::new(build_runtime(ScriptedProvider::new(script), tools));

    let outcome = rt.run_turn(UserInput::from_text("loop")).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Finished(_)));

    let snap = rt.context().snapshot().await;
    let has_reminder = snap
        .messages
        .iter()
        .any(|m| m.text().contains("[系统提醒]"));
    assert!(!has_reminder, "不同 input 不应触发软提醒");
}

#[tokio::test]
async fn thresholds_empty_disables_soft_only() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(MockTool::read_only("greet", "hi")));
    // 空阈值数组：无软提醒；3 轮相同集合 → 硬停止（默认 3）
    let mut config = RuntimeConfig::default();
    config.tools.repeat_guard_thresholds = Vec::new();
    let script = vec![
        tool_call_deltas("c1", "greet", "{}"),
        tool_call_deltas("c2", "greet", "{}"),
        tool_call_deltas("c3", "greet", "{}"),
    ];
    let rt = Arc::new(build_runtime_with_config(
        ScriptedProvider::new(script),
        tools,
        config,
    ));

    let outcome = rt.run_turn(UserInput::from_text("loop")).await.unwrap();
    match outcome {
        TurnOutcome::Finished(msg) => {
            assert!(
                msg.text().contains("重复工具调用"),
                "空阈值时 3 轮应硬停止: {:?}",
                msg.text()
            );
        }
        other => panic!("应返回 Finished(Stopped 消息)，实际 {other:?}"),
    }
    let snap = rt.context().snapshot().await;
    let has_reminder = snap
        .messages
        .iter()
        .any(|m| m.text().contains("[系统提醒]"));
    assert!(!has_reminder, "空阈值数组应关闭软提醒");
}

/// S4 测试基建：把输入改写为指定 JSON 的 PreToolUse Hook。
struct ModifyInputHook {
    matcher: minicoding_core::hooks::HookMatcher,
    new_input: serde_json::Value,
}

impl minicoding_core::hooks::Hook for ModifyInputHook {
    fn name(&self) -> &str {
        "modify-input-hook"
    }
    fn matcher(&self) -> &minicoding_core::hooks::HookMatcher {
        &self.matcher
    }
    fn run(
        &self,
        _input: minicoding_core::hooks::HookInput,
    ) -> futures::future::BoxFuture<
        '_,
        Result<minicoding_core::hooks::HookOutput, minicoding_core::hooks::HookError>,
    > {
        let out = minicoding_core::hooks::HookOutput {
            modify_input: Some(self.new_input.clone()),
            ..Default::default()
        };
        Box::pin(async move { Ok(out) })
    }
}

/// S4 测试基建：按输入内容判定的策略——含 "evil" Deny、含 "danger" Ask、其余 Allow。
#[derive(Debug, Default)]
struct InputSensitivePolicy;

impl minicoding_core::policy::PermissionPolicy for InputSensitivePolicy {
    fn check(
        &self,
        tool: &str,
        input: &serde_json::Value,
        _ctx: &minicoding_core::policy::PermissionContext,
    ) -> futures::future::BoxFuture<
        '_,
        Result<minicoding_core::policy::Verdict, minicoding_core::model::PolicyError>,
    > {
        use minicoding_core::policy::{PermissionPrompt, Risk, Verdict};
        let text = input.to_string();
        let verdict = if text.contains("evil") {
            Verdict::Deny("input contains evil".into())
        } else if text.contains("danger") {
            Verdict::Ask(PermissionPrompt {
                id: format!("p-{tool}"),
                tool: tool.to_string(),
                summary: format!("执行 {tool}（改写后输入）"),
                risk: Risk::High,
                options: Vec::new(),
            })
        } else {
            Verdict::Allow
        };
        Box::pin(async move { Ok(verdict) })
    }
}

/// S4 测试基建：内存 HookRegistry（注册/查询）。
struct TestHookRegistry {
    hooks: std::sync::Mutex<Vec<Arc<dyn minicoding_core::hooks::Hook>>>,
}

impl TestHookRegistry {
    fn with(hook: Arc<dyn minicoding_core::hooks::Hook>) -> Arc<Self> {
        Arc::new(Self {
            hooks: std::sync::Mutex::new(vec![hook]),
        })
    }
}

impl minicoding_core::hooks::HookRegistry for TestHookRegistry {
    fn register(&self, hook: Arc<dyn minicoding_core::hooks::Hook>) {
        self.hooks.lock().expect("hooks").push(hook);
    }
    fn for_event(
        &self,
        event: minicoding_core::hooks::HookEvent,
    ) -> Vec<Arc<dyn minicoding_core::hooks::Hook>> {
        self.hooks
            .lock()
            .expect("hooks")
            .iter()
            .filter(|h| h.matcher().matches_event(event))
            .cloned()
            .collect()
    }
    fn count(&self) -> usize {
        self.hooks.lock().expect("hooks").len()
    }
    fn dispatch(
        &self,
        mut input: minicoding_core::hooks::HookInput,
        config: minicoding_core::hooks::DispatchConfig,
    ) -> minicoding_core::provider::BoxFuture<'_, minicoding_core::hooks::DispatchResult> {
        // A1 下沉后测试桩需自行聚合：执行注册 Hook，应用 modify_input 与决策
        // （C-21：builtin_deny 预置 Deny 且忽略后续 Allow）
        use minicoding_core::hooks::{HookDecision, OnHookError};
        Box::pin(async move {
            let event = input.event;
            let tool_name = input.tool.as_ref().map(|t| t.name.clone());
            let mut result = minicoding_core::hooks::DispatchResult::default();
            if let Some(reason) = config.builtin_deny.clone() {
                result.decision = HookDecision::Deny;
                result.reason = Some(reason);
            }
            for hook in self.for_event_with_tool(event, tool_name.as_deref()) {
                if let Ok(out) = hook.run(input.clone()).await {
                    if let Some(new_input) = out.modify_input {
                        if let Some(ref mut tool) = input.tool {
                            tool.input = new_input.clone();
                        }
                        result.modify_input = Some(new_input);
                    }
                    match out.decision {
                        HookDecision::Allow
                            if config.builtin_deny.is_none()
                                && !matches!(config.on_error, OnHookError::Deny) =>
                        {
                            // 简化合并：无 builtin deny 时 Allow 生效（S4 场景不依赖）
                        }
                        HookDecision::Deny => {
                            result.decision = HookDecision::Deny;
                            result.reason = out.reason;
                        }
                        _ => {}
                    }
                }
            }
            result
        })
    }
}

/// S4/C-01/C-21：Hook `modify_input` 改写后的输入必须重过策略——用户批准 A 不能执行 B。
#[tokio::test]
async fn hook_modify_input_denied_by_recheck() {
    use minicoding_core::hooks::{HookEvent, HookMatcher};

    let mut tools = ToolRegistry::new();
    let shell = Arc::new(MockTool::command("shell.run", "ok"));
    let shell_for_assert = Arc::clone(&shell);
    tools.register(Arc::clone(&shell) as Arc<dyn minicoding_core::tool::Tool>);

    // 原始输入 ls（Allow），Hook 改写为 evil（策略 Deny）
    let hook = ModifyInputHook {
        matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
        new_input: serde_json::json!({"cmd": "evil rm -rf /"}),
    };
    let script = vec![
        tool_call_deltas("c1", "shell.run", r#"{"cmd":"ls"}"#),
        text_deltas("done"),
    ];
    let rt = Arc::new(
        RuntimeBuilder::new()
            .provider(Arc::new(ScriptedProvider::new(script)))
            .context(Arc::new(TestContext::new("test system prompt")))
            .storage(Arc::new(InMemoryStorage::new()))
            .tools(tools)
            .policy(Arc::new(InputSensitivePolicy))
            .prompter(Arc::new(DenyPrompter))
            .hook_registry(TestHookRegistry::with(Arc::new(hook)))
            .workdir(Utf8PathBuf::from("."))
            .build()
            .expect("runtime build"),
    );

    let outcome = rt.run_turn(UserInput::from_text("go")).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Finished(_)));

    // 改写后输入被策略复查拒绝：工具未执行，回灌 permission denied
    assert!(
        shell_for_assert.take_calls().is_empty(),
        "改写后输入应被拒绝，工具不应执行"
    );
    let messages = rt.storage().load(&rt.session().id).await.unwrap();
    let denied = messages.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(b,
                minicoding_core::model::ContentBlock::ToolResult { content, is_error, .. }
                if *is_error && format!("{content:?}").contains("permission denied")
            )
        })
    });
    assert!(denied, "应有 permission denied 的 tool_result");
}

/// S4：原始 Allow 的输入被 Hook 改写为需 Ask 的内容 → 升级 Ask → DenyPrompter 拒绝。
#[tokio::test]
async fn hook_modify_input_escalates_to_ask() {
    use minicoding_core::hooks::{HookEvent, HookMatcher};

    let mut tools = ToolRegistry::new();
    let shell = Arc::new(MockTool::command("shell.run", "ok"));
    let shell_for_assert = Arc::clone(&shell);
    tools.register(Arc::clone(&shell) as Arc<dyn minicoding_core::tool::Tool>);

    let hook = ModifyInputHook {
        matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
        new_input: serde_json::json!({"cmd": "danger ops"}),
    };
    let script = vec![
        tool_call_deltas("c1", "shell.run", r#"{"cmd":"ls"}"#),
        text_deltas("done"),
    ];
    let rt = Arc::new(
        RuntimeBuilder::new()
            .provider(Arc::new(ScriptedProvider::new(script)))
            .context(Arc::new(TestContext::new("test system prompt")))
            .storage(Arc::new(InMemoryStorage::new()))
            .tools(tools)
            .policy(Arc::new(InputSensitivePolicy))
            .prompter(Arc::new(DenyPrompter))
            .hook_registry(TestHookRegistry::with(Arc::new(hook)))
            .workdir(Utf8PathBuf::from("."))
            .build()
            .expect("runtime build"),
    );

    let outcome = rt.run_turn(UserInput::from_text("go")).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Finished(_)));
    assert!(
        shell_for_assert.take_calls().is_empty(),
        "升级 Ask 后被 DenyPrompter 拒绝，工具不应执行"
    );
}

/// S4：Hook 未修改输入时不触发重查（原始 Allow 路径不受影响）。
#[tokio::test]
async fn hook_without_modify_input_keeps_allow_path() {
    use minicoding_core::hooks::{HookEvent, HookMatcher};

    let mut tools = ToolRegistry::new();
    let shell = Arc::new(MockTool::command("shell.run", "ran"));
    let shell_for_assert = Arc::clone(&shell);
    tools.register(Arc::clone(&shell) as Arc<dyn minicoding_core::tool::Tool>);

    // Continue 输出（不改输入）
    let hook = NoopModifyHook {
        matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
    };
    let script = vec![
        tool_call_deltas("c1", "shell.run", r#"{"cmd":"ls"}"#),
        text_deltas("done"),
    ];
    let rt = Arc::new(
        RuntimeBuilder::new()
            .provider(Arc::new(ScriptedProvider::new(script)))
            .context(Arc::new(TestContext::new("test system prompt")))
            .storage(Arc::new(InMemoryStorage::new()))
            .tools(tools)
            .policy(Arc::new(InputSensitivePolicy))
            .prompter(Arc::new(DenyPrompter))
            .hook_registry(TestHookRegistry::with(Arc::new(hook)))
            .workdir(Utf8PathBuf::from("."))
            .build()
            .expect("runtime build"),
    );

    let outcome = rt.run_turn(UserInput::from_text("go")).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Finished(_)));
    assert_eq!(shell_for_assert.take_calls().len(), 1, "Allow 路径正常执行");
}

/// 不干预的 PreToolUse Hook。
struct NoopModifyHook {
    matcher: minicoding_core::hooks::HookMatcher,
}

impl minicoding_core::hooks::Hook for NoopModifyHook {
    fn name(&self) -> &str {
        "noop-hook"
    }
    fn matcher(&self) -> &minicoding_core::hooks::HookMatcher {
        &self.matcher
    }
    fn run(
        &self,
        _input: minicoding_core::hooks::HookInput,
    ) -> futures::future::BoxFuture<
        '_,
        Result<minicoding_core::hooks::HookOutput, minicoding_core::hooks::HookError>,
    > {
        Box::pin(async move { Ok(minicoding_core::hooks::HookOutput::continue_()) })
    }
}

/// M-09：沙箱拒绝结构化透传（denial → `ToolResultMeta.sandbox_denied`）。
#[tokio::test]
async fn sandbox_denial_structured_metadata() {
    use minicoding_core::sandbox::{
        DenialMatch, DenialSignature, SandboxDenialDetector, SandboxDenyKind,
    };

    /// 测试用拒绝检测器：命中 "Operation not permitted" → `SyscallBlocked`。
    #[derive(Debug, Clone, Copy)]
    struct EpermDetector;
    impl SandboxDenialDetector for EpermDetector {
        fn detect(&self, tool: &str, error_text: &str) -> Option<DenialMatch> {
            error_text
                .contains("Operation not permitted")
                .then(|| DenialMatch {
                    signature: DenialSignature {
                        platform: "any",
                        pattern: "Operation not permitted",
                        reason: "EPERM",
                        kind_label: "syscall_blocked",
                    },
                    tool: tool.to_string(),
                    kind: SandboxDenyKind::SyscallBlocked {
                        syscall: error_text
                            .lines()
                            .next()
                            .unwrap_or_default()
                            .chars()
                            .take(120)
                            .collect(),
                    },
                })
        }
    }

    let m = EpermDetector.detect("bad", "execution: Operation not permitted");
    assert!(m.is_some(), "detector 应命中");
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(MockTool::failing(
        "bad",
        "Operation not permitted",
    )));
    let script = vec![tool_call_deltas("c1", "bad", "{}"), text_deltas("done")];
    let rt = Arc::new(
        RuntimeBuilder::new()
            .provider(Arc::new(ScriptedProvider::new(script)))
            .context(Arc::new(TestContext::new("test system prompt")))
            .storage(Arc::new(InMemoryStorage::new()))
            .tools(tools)
            .prompter(Arc::new(DenyPrompter))
            .sandbox_denial_detector(Arc::new(EpermDetector))
            .workdir(Utf8PathBuf::from("."))
            .build()
            .expect("runtime build"),
    );

    let outcome = rt.run_turn(UserInput::from_text("go")).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Finished(_)));

    // 持久化消息的 tool_result 块携带 metadata.sandbox_denied（M-09 结构化透传）
    let messages = rt.storage().load(&rt.session().id).await.unwrap();
    let denied = messages
        .iter()
        .find_map(|m| {
            m.content.iter().find_map(|b| match b {
                minicoding_core::model::ContentBlock::ToolResult { metadata, .. } => {
                    metadata.sandbox_denied.as_ref()
                }
                _ => None,
            })
        })
        .expect("应有一条带 sandbox_denied 元数据的 tool_result 消息");
    assert_eq!(
        denied.kind,
        minicoding_core::sandbox::SandboxDenyKind::SyscallBlocked {
            syscall: "execution: Operation not permitted".into()
        }
    );
    assert!(
        denied.detail.contains("Operation not permitted"),
        "detail 应含原始错误文本"
    );
}

/// M-06：step 边界事件持久化（StepStarted/StepEnded 一一配对）。
///
/// 两次 LLM 迭代：iter0 带工具调用（有 step 对），iter1 纯文本（无 step 事件）。
/// 验证事件流按 seq 记录 StepStarted（携带 tool_call_ids）→ StepEnded。
#[tokio::test]
async fn turn_with_2_steps_persists_step_pairs() {
    use minicoding_core::storage::{EventStore, PersistedEvent};

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(MockTool::read_only("greet", "hi")));
    let provider = ScriptedProvider::new(vec![
        tool_call_deltas("call_1", "greet", "{}"),
        text_deltas("done"),
    ]);
    let event_store = Arc::new(InMemoryEventStore::new());
    let rt = Arc::new(build_runtime_with_event_store(
        provider,
        tools,
        event_store.clone(),
    ));

    let outcome = rt.run_turn(UserInput::from_text("hello")).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Finished(_)));

    let records = event_store.load(&rt.session().id.clone()).await.unwrap();
    let steps_started: Vec<&PersistedEvent> = records
        .iter()
        .filter_map(|r| match &r.event {
            PersistedEvent::StepStarted { .. } => Some(&r.event),
            _ => None,
        })
        .collect();
    let steps_ended: Vec<&PersistedEvent> = records
        .iter()
        .filter_map(|r| match &r.event {
            PersistedEvent::StepEnded { .. } => Some(&r.event),
            _ => None,
        })
        .collect();

    // 仅 iter0 有工具调用：1 对 step 事件
    assert_eq!(
        steps_started.len(),
        1,
        "expected 1 step started: records={records:?}"
    );
    assert_eq!(
        steps_ended.len(),
        1,
        "expected 1 step ended: records={records:?}"
    );

    match (steps_started[0], steps_ended[0]) {
        (
            PersistedEvent::StepStarted {
                iter: s_iter,
                tool_call_ids,
            },
            PersistedEvent::StepEnded { iter: e_iter },
        ) => {
            assert_eq!(*s_iter, 0);
            assert_eq!(*e_iter, 0);
            assert_eq!(tool_call_ids.as_slice(), ["call_1"]);
        }
        _ => panic!("unexpected step event shapes"),
    }
    // step 事件均带 SCHEMA_VERSION 2
    for r in records.iter().filter(|r| {
        matches!(
            r.event,
            PersistedEvent::StepStarted { .. } | PersistedEvent::StepEnded { .. }
        )
    }) {
        assert_eq!(r.schema_version, minicoding_core::storage::SCHEMA_VERSION);
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
