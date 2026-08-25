//! C-23/C-27 restricted ask 回归测试（2026-08-26 R3 审查 RT-1/SEC-3）。
//!
//! 场景：`fs.write` 先以**无 path 输入**获得 `AllowAlways`（`full_options`，
//! 会话级缓存插入），随后对 `AGENTS.md` 的写入是 restricted ask
//! （`project_doc_options`，不含 AllowAlways）——后者必须仍走弹窗，
//! 不得被会话级缓存/持久化规则静默放行。

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use camino::Utf8PathBuf;
use minicoding_core::config::RuntimeConfig;
use minicoding_core::model::{TurnOutcome, UserInput};
use minicoding_core::policy::{Decision, PermissionPrompt, PermissionPrompter};
use minicoding_core::provider::BoxFuture;
use minicoding_core::runtime::{Runtime, RuntimeBuilder};
use minicoding_core::tool::ToolRegistry;

use common::{
    InMemoryStorage, MockTool, ScriptedProvider, TestContext, text_deltas, tool_call_deltas,
};

/// 模拟 `BuiltinPolicy` 的 restricted-ask 契约（被测对象是 Runtime 决策入口，
/// builtin 自身语义由 minicoding-policy 单测覆盖；core 不能反向依赖 policy crate）：
/// - 无 `path` 输入 → `Ask`（**含** AllowAlways，模拟 `full_options`）；
/// - `path: "AGENTS.md"` → `Ask`（**不含** AllowAlways，C-23 restricted）。
struct RestrictedAskPolicy;

impl minicoding_core::policy::PermissionPolicy for RestrictedAskPolicy {
    fn check(
        &self,
        tool: &str,
        input: &serde_json::Value,
        _ctx: &minicoding_core::policy::PermissionContext,
    ) -> BoxFuture<'_, Result<minicoding_core::policy::Verdict, minicoding_core::model::PolicyError>>
    {
        use minicoding_core::policy::{PromptOption, Verdict};
        let options = if input.get("path").is_some() {
            vec![PromptOption::AllowOnce, PromptOption::DenyOnce]
        } else {
            vec![
                PromptOption::AllowOnce,
                PromptOption::AllowAlways,
                PromptOption::DenyOnce,
                PromptOption::DenyAlways,
            ]
        };
        let verdict = Verdict::Ask(PermissionPrompt {
            id: format!("p-{tool}-{}", ulid_like()),
            tool: tool.to_string(),
            summary: "test write".to_string(),
            risk: minicoding_core::policy::Risk::Medium,
            options,
        });
        Box::pin(async move { Ok(verdict) })
    }
}

fn ulid_like() -> u64 {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

/// 脚本化 prompter：按预定序列返回决策，记录实际被询问次数。
struct ScriptPrompter {
    decisions: std::sync::Mutex<std::collections::VecDeque<Decision>>,
    prompted: AtomicUsize,
}

impl ScriptPrompter {
    fn new(decisions: Vec<Decision>) -> Self {
        Self {
            decisions: std::sync::Mutex::new(decisions.into()),
            prompted: AtomicUsize::new(0),
        }
    }

    fn prompt_count(&self) -> usize {
        self.prompted.load(Ordering::SeqCst)
    }
}

impl PermissionPrompter for ScriptPrompter {
    fn prompt(&self, _p: PermissionPrompt) -> BoxFuture<'_, Decision> {
        self.prompted.fetch_add(1, Ordering::SeqCst);
        let next = self
            .decisions
            .lock()
            .expect("decisions poisoned")
            .pop_front()
            .unwrap_or_else(|| Decision::Deny("script exhausted".to_string()));
        Box::pin(async move { next })
    }
}

fn build_runtime(
    provider: ScriptedProvider,
    tools: ToolRegistry,
    prompter: Arc<ScriptPrompter>,
    persist_path: Option<Utf8PathBuf>,
) -> Runtime {
    let mut b = RuntimeBuilder::new()
        .provider(Arc::new(provider))
        .context(Arc::new(TestContext::new("test system prompt")))
        .storage(Arc::new(InMemoryStorage::new()))
        .tools(tools)
        .policy(Arc::new(RestrictedAskPolicy))
        .prompter(prompter)
        .config(RuntimeConfig::default())
        .workdir(Utf8PathBuf::from("."));
    if let Some(p) = persist_path {
        b = b.with_policy_persist(Some(Arc::new(minicoding_core::policy::PolicyPersist::new(
            p,
        ))));
    }
    b.build().expect("runtime build")
}

#[tokio::test]
async fn session_allow_cache_must_not_bypass_restricted_ask() {
    // 单轮内：无 path 写入（full_options Ask）→ 用户选 AllowAlways
    //       （**注入 PolicyPersist**：无路径 Always 走会话级缓存分支插入
    //       "fs.write"——不注入 persist 时折叠路径不落缓存，测试无法触达
    //       RT-1 缺陷）→ 写 AGENTS.md（restricted ask，不含 AllowAlways）
    //       → 必须再次弹窗；脚本第二决策为 Deny。
    let tmp = tempfile::tempdir().expect("tmpdir");
    let persist_file =
        Utf8PathBuf::from_path_buf(tmp.path().join("policy.toml")).expect("utf8 path");

    let tool = Arc::new(MockTool::file_write("fs.write", "wrote"));
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone());

    let prompter = Arc::new(ScriptPrompter::new(vec![
        Decision::AllowAlways,
        Decision::Deny("must not be silently allowed".to_string()),
    ]));

    let provider = ScriptedProvider::new(vec![
        tool_call_deltas("c1", "fs.write", "{}"),
        tool_call_deltas("c2", "fs.write", r#"{"path":"AGENTS.md"}"#),
        text_deltas("done"),
    ]);

    let rt = build_runtime(provider, tools, prompter.clone(), Some(persist_file));
    let o1 = rt
        .run_turn(UserInput::from_text("t1"))
        .await
        .expect("turn1");
    assert!(
        matches!(o1, TurnOutcome::Finished(_)),
        "turn1 应正常结束 {o1:?}"
    );

    // RT-1 断言：第二次调用必须真的弹窗（prompted == 2），且决策为 Deny 时
    // 工具未被执行。修复前：未门控缓存早退 → prompted 停留在 1、AGENTS.md
    // 写入被静默放行。
    assert_eq!(
        prompter.prompt_count(),
        2,
        "restricted ask 必须绕过会话级缓存重新弹窗"
    );
    assert_eq!(tool.take_calls().len(), 1, "第二次写入应被 Deny 不执行");
}

#[tokio::test]
async fn always_decision_on_restricted_ask_collapses_and_never_persists() {
    // SEC-3：前端对 restricted ask 回传 AllowAlways（协议曾不携带 options，
    // Web 恒渲染"始终允许"按钮）→ 必须折叠为一次性 Allow 且**不落任何缓存/
    // 持久化**——后续同目标写入仍要弹窗。注入 PolicyPersist 验证持久化分支
    // 同样不被触碰。
    let tmp = tempfile::tempdir().expect("tmpdir");
    let persist_file =
        Utf8PathBuf::from_path_buf(tmp.path().join("policy.toml")).expect("utf8 path");

    let tool = Arc::new(MockTool::file_write("fs.write", "wrote"));
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone());
    // 恒回传 AllowAlways（模拟失控前端）：三次询问全部回传 Always
    let prompter = Arc::new(ScriptPrompter::new(vec![
        Decision::AllowAlways,
        Decision::AllowAlways,
        Decision::AllowAlways,
    ]));

    // 第三次调用为**无 path** 输入（full_options Ask）——若无 SEC-3 折叠，
    // 前两次的失控 Always 会把 "fs.write" 写入会话级缓存，第三次将静默命中
    // 缓存免弹窗（prompted == 2）；修复后必须弹满三次。
    let provider = ScriptedProvider::new(vec![
        tool_call_deltas("c1", "fs.write", r#"{"path":"AGENTS.md"}"#),
        tool_call_deltas("c2", "fs.write", r#"{"path":"AGENTS.md"}"#),
        tool_call_deltas("c3", "fs.write", "{}"),
        text_deltas("done"),
    ]);

    let rt = build_runtime(provider, tools, prompter.clone(), Some(persist_file));
    rt.run_turn(UserInput::from_text("t1")).await.expect("turn");
    assert_eq!(
        prompter.prompt_count(),
        3,
        "折叠后的 Always 不得产生任何会话级/持久化放行——每次都必须弹窗"
    );
}
