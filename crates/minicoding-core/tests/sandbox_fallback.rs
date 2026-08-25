//! 集成测试：沙箱初始化失败 → 询问沙箱外运行（C-22 fallback，见 `design.md` §20）。
//!
//! 场景：工具返回 `sandbox apply/post_spawn failed`（如 Windows Job Object 恢复线程
//! 快照竞态）→ Runtime 发起 High risk 权限询问 → 用户 Allow 则以 `DangerFullAccess`
//! 策略沙箱外重试一次；Deny 则回灌原错误；非沙箱错误不触发询问。

mod common;

use std::sync::Arc;
use std::sync::Mutex;

use camino::Utf8PathBuf;
use minicoding_core::config::RuntimeConfig;
use minicoding_core::model::{
    SideEffect, StopReason, ToolError, ToolResult, ToolSchema, TurnOutcome, UserInput,
};
use minicoding_core::policy::{Decision, PermissionPrompt, PermissionPrompter};
use minicoding_core::provider::{BoxFuture, Delta, ToolCallDelta};
use minicoding_core::runtime::{Runtime, RuntimeBuilder};
use minicoding_core::sandbox::SandboxPolicy;
use minicoding_core::tool::{Tool, ToolContext, ToolRegistry};

use common::{InMemoryStorage, ScriptedProvider, TestContext, text_deltas};

/// 对沙箱策略敏感的 mock 工具：`WorkspaceWrite` 下模拟 `post_spawn` 失败，
/// `DangerFullAccess`（沙箱外重试）下成功。
struct SandboxSensitiveTool {
    calls: Mutex<usize>,
}

impl Tool for SandboxSensitiveTool {
    fn name(&self) -> &'static str {
        "sandbox_sensitive"
    }
    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema {
            name: "sandbox_sensitive".to_string(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
        })
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::Command
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        *self.calls.lock().expect("calls poisoned") += 1;
        let policy = ctx.sandbox_policy.clone();
        Box::pin(async move {
            match policy {
                // 沙箱外重试（fallback ctx）：成功
                Some(SandboxPolicy::DangerFullAccess) => {
                    Ok(ToolResult::ok_text("ran outside sandbox"))
                }
                // 正常沙箱路径：模拟 Windows post_spawn 失败
                Some(_) => Err(ToolError::Exec(
                    "sandbox post_spawn failed: no suspendable thread found for pid 1".to_string(),
                )),
                None => Ok(ToolResult::ok_text("no sandbox injected")),
            }
        })
    }
}

/// 恒失败的 mock 工具（非沙箱错误，用于验证不触发询问）。
struct PlainFailingTool {
    calls: Mutex<usize>,
}

impl Tool for PlainFailingTool {
    fn name(&self) -> &'static str {
        "plain_failing"
    }
    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema {
            name: "plain_failing".to_string(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
        })
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::Command
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        *self.calls.lock().expect("calls poisoned") += 1;
        Box::pin(async move { Err(ToolError::InvalidInput("bad input".to_string())) })
    }
}

/// 记录询问次数的 prompter（决策可配置）。
struct CountingPrompter {
    prompts: Mutex<usize>,
    decision: Decision,
}

impl PermissionPrompter for CountingPrompter {
    fn prompt(&self, _p: PermissionPrompt) -> BoxFuture<'_, Decision> {
        *self.prompts.lock().expect("prompts poisoned") += 1;
        let d = self.decision.clone();
        Box::pin(async move { d })
    }
}

/// 构造测试用 Runtime：注入 mock provider、内存存储、沙箱敏感工具、可计数 prompter。
fn build_runtime(
    provider: ScriptedProvider,
    tools: ToolRegistry,
    prompter: Arc<CountingPrompter>,
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

/// 构造"单次工具调用 → 第二轮文本回复"的 provider 脚本。
fn tool_then_text(script2: Vec<Delta>) -> ScriptedProvider {
    ScriptedProvider::new(vec![
        vec![
            Delta::ToolCall(ToolCallDelta {
                index: 0,
                id: Some("c1".into()),
                name: Some("sandbox_sensitive".into()),
                args_chunk: Some(r"{}".into()),
            }),
            Delta::Stop(StopReason::ToolUse),
        ],
        script2,
    ])
}

/// 场景 1：用户 Allow → 以 `DangerFullAccess` 沙箱外重试一次，执行成功。
#[tokio::test]
async fn sandbox_failure_allow_retries_outside_sandbox() {
    let tool = Arc::new(SandboxSensitiveTool {
        calls: Mutex::new(0),
    });
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone());
    let prompter = Arc::new(CountingPrompter {
        prompts: Mutex::new(0),
        decision: Decision::Allow,
    });
    let rt = build_runtime(tool_then_text(text_deltas("done")), tools, prompter.clone());

    let outcome = rt
        .run_turn(UserInput::from_text("run"))
        .await
        .expect("turn ok");
    match outcome {
        TurnOutcome::Finished(msg) => assert_eq!(msg.text(), "done"),
        other => panic!("expected Finished, got {other:?}"),
    }
    assert_eq!(
        *tool.calls.lock().expect("calls poisoned"),
        2,
        "沙箱外重试应执行第二次调用"
    );
    assert_eq!(
        *prompter.prompts.lock().expect("prompts poisoned"),
        1,
        "应恰好询问一次"
    );
}

/// 场景 2：用户 Deny → 不重试，原错误回灌 LLM。
#[tokio::test]
async fn sandbox_failure_deny_does_not_retry() {
    let tool = Arc::new(SandboxSensitiveTool {
        calls: Mutex::new(0),
    });
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone());
    let prompter = Arc::new(CountingPrompter {
        prompts: Mutex::new(0),
        decision: Decision::Deny("拒绝沙箱外运行".to_string()),
    });
    let rt = build_runtime(tool_then_text(text_deltas("done")), tools, prompter.clone());

    let outcome = rt
        .run_turn(UserInput::from_text("run"))
        .await
        .expect("turn ok");
    match outcome {
        TurnOutcome::Finished(msg) => assert_eq!(msg.text(), "done"),
        other => panic!("expected Finished, got {other:?}"),
    }
    assert_eq!(
        *tool.calls.lock().expect("calls poisoned"),
        1,
        "Deny 后不应沙箱外重试"
    );
    assert_eq!(
        *prompter.prompts.lock().expect("prompts poisoned"),
        1,
        "沙箱初始化失败应发起一次询问"
    );
}

/// 场景 3：非沙箱错误 → 不触发询问。
#[tokio::test]
async fn non_sandbox_error_does_not_prompt() {
    let tool = Arc::new(PlainFailingTool {
        calls: Mutex::new(0),
    });
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone());
    let prompter = Arc::new(CountingPrompter {
        prompts: Mutex::new(0),
        decision: Decision::Allow,
    });
    let provider = ScriptedProvider::new(vec![
        vec![
            Delta::ToolCall(ToolCallDelta {
                index: 0,
                id: Some("c1".into()),
                name: Some("plain_failing".into()),
                args_chunk: Some(r"{}".into()),
            }),
            Delta::Stop(StopReason::ToolUse),
        ],
        text_deltas("done"),
    ]);
    let rt = build_runtime(provider, tools, prompter.clone());

    let outcome = rt
        .run_turn(UserInput::from_text("run"))
        .await
        .expect("turn ok");
    match outcome {
        TurnOutcome::Finished(msg) => assert_eq!(msg.text(), "done"),
        other => panic!("expected Finished, got {other:?}"),
    }
    assert_eq!(*tool.calls.lock().expect("calls poisoned"), 1);
    assert_eq!(
        *prompter.prompts.lock().expect("prompts poisoned"),
        0,
        "非沙箱错误不应触发沙箱外询问"
    );
}
