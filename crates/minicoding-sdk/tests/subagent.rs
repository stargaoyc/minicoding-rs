//! B1 `InProcessSubagentRunner` 集成测试（F2 扇出上限 / 深度防御 F1 / 结果映射）。
//!
//! 不连真实 LLM：`ScriptedProvider` 返回脚本化文本；`BlockingProvider` 用 latch
//! 把 turn 挂起，验证并发第 5 个 spawn 被信号量拒绝。

use futures::stream;
use minicoding_core::agent::SubagentRunner;
use minicoding_core::model::{LlmError, Message, StopReason, SubagentSpec, SubagentType};
use minicoding_core::provider::{
    BoxFuture, BoxStream, Capabilities, ChatRequest, Delta, LlmProvider, Tokenizer,
};
use minicoding_policy::BuiltinPolicy;
use minicoding_sdk::subagent::{InProcessSubagentRunner, MAX_CONCURRENT_SUBAGENTS};
use minicoding_storage::JsonlStorage;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// 字符计数分词器（测试用）。
#[derive(Debug, Default)]
struct CountingTokenizer;

impl Tokenizer for CountingTokenizer {
    fn count(&self, text: &str) -> usize {
        text.chars().count()
    }
    fn count_messages(&self, msgs: &[Message]) -> usize {
        msgs.iter().map(|m| self.count(&m.text())).sum()
    }
    fn id(&self) -> &'static str {
        "counting"
    }
}

/// 脚本化 provider：按序弹出固定文本响应（无工具调用，单轮 `EndTurn`）。
struct ScriptedProvider {
    responses: Mutex<VecDeque<String>>,
}

impl ScriptedProvider {
    fn new(responses: &[&str]) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from_iter(
                responses.iter().map(|s| (*s).to_string()),
            )),
        }
    }
}

impl LlmProvider for ScriptedProvider {
    fn id(&self) -> &'static str {
        "scripted"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tool_call: false,
            supports_vision: false,
            supports_streaming: true,
            supports_json_mode: false,
            context_window: 32_768,
            max_output: 4096,
        }
    }
    fn tokenizer(&self) -> Arc<dyn Tokenizer> {
        Arc::new(CountingTokenizer)
    }
    fn chat_stream(
        &self,
        _req: ChatRequest,
    ) -> BoxFuture<'_, Result<BoxStream<'static, Result<Delta, LlmError>>, LlmError>> {
        let resp = self
            .responses
            .lock()
            .expect("lock poisoned")
            .pop_front()
            .unwrap_or_else(|| "fallback".to_string());
        Box::pin(async move {
            let s = stream::iter(vec![
                Ok(Delta::Text(resp)),
                Ok(Delta::Stop(StopReason::EndTurn)),
            ]);
            Ok(Box::pin(s) as BoxStream<'static, Result<Delta, LlmError>>)
        })
    }
    fn count_tokens(&self, _messages: &[Message]) -> BoxFuture<'_, usize> {
        Box::pin(async { 0 })
    }
}

/// 阻塞 provider：等待外部 notify 才返回响应（并发上限测试用）。
struct BlockingProvider {
    release: Arc<Notify>,
}

impl LlmProvider for BlockingProvider {
    fn id(&self) -> &'static str {
        "blocking"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tool_call: false,
            supports_vision: false,
            supports_streaming: true,
            supports_json_mode: false,
            context_window: 32_768,
            max_output: 4096,
        }
    }
    fn tokenizer(&self) -> Arc<dyn Tokenizer> {
        Arc::new(CountingTokenizer)
    }
    fn chat_stream(
        &self,
        _req: ChatRequest,
    ) -> BoxFuture<'_, Result<BoxStream<'static, Result<Delta, LlmError>>, LlmError>> {
        let release = self.release.clone();
        Box::pin(async move {
            release.notified().await;
            let s = stream::iter(vec![
                Ok(Delta::Text("released".to_string())),
                Ok(Delta::Stop(StopReason::EndTurn)),
            ]);
            Ok(Box::pin(s) as BoxStream<'static, Result<Delta, LlmError>>)
        })
    }
    fn count_tokens(&self, _messages: &[Message]) -> BoxFuture<'_, usize> {
        Box::pin(async { 0 })
    }
}

/// 构造指向临时 sessions 目录的 runner。
fn make_runner(provider: Arc<dyn LlmProvider>, dir: &std::path::Path) -> InProcessSubagentRunner {
    let sessions = camino::Utf8PathBuf::from_path_buf(dir.to_owned())
        .expect("tempdir path is UTF-8 on linux test env");
    InProcessSubagentRunner::new(
        provider,
        Arc::new(CountingTokenizer),
        "test system prompt".to_string(),
        Arc::new(JsonlStorage::new(sessions.join("sessions"))),
        Arc::new(minicoding_policy::NonInteractivePrompter::new()),
        Arc::new(BuiltinPolicy::new()),
        Arc::new(minicoding_core::storage::NoopAudit),
        None,
        minicoding_core::config::RuntimeConfig::default(),
        sessions,
    )
}

fn spec() -> SubagentSpec {
    SubagentSpec::default_for(SubagentType::GeneralPurpose)
}

#[tokio::test]
async fn spawn_returns_summary_and_completed() {
    let tmp = tempfile::tempdir().unwrap();
    let runner = make_runner(
        Arc::new(ScriptedProvider::new(&["子代理探查完成：找到 3 处引用"])),
        tmp.path(),
    );
    let result = runner
        .spawn(spec(), "查找 foo 的引用".to_string())
        .await
        .expect("spawn 应成功");
    assert!(result.completed);
    assert_eq!(result.summary, "子代理探查完成：找到 3 处引用");
    // usage 累计 > 0：子上下文含 user + assistant 消息。
    assert!(result.token_used > 0);
    // 子会话独立持久化到父 sessions_dir（审计可回溯）。
    let entries = std::fs::read_dir(tmp.path().join("sessions")).unwrap();
    assert!(entries.count() > 0, "子会话应落盘到父 sessions_dir");
}

#[tokio::test]
async fn failed_turn_maps_to_err() {
    struct ExplodingProvider;
    impl LlmProvider for ExplodingProvider {
        fn id(&self) -> &'static str {
            "exploding"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                supports_tool_call: false,
                supports_vision: false,
                supports_streaming: true,
                supports_json_mode: false,
                context_window: 1000,
                max_output: 1000,
            }
        }
        fn tokenizer(&self) -> Arc<dyn Tokenizer> {
            Arc::new(CountingTokenizer)
        }
        fn chat_stream(
            &self,
            _req: ChatRequest,
        ) -> BoxFuture<'_, Result<BoxStream<'static, Result<Delta, LlmError>>, LlmError>> {
            Box::pin(async {
                Err(LlmError::Client {
                    status: 500,
                    body: "boom".to_string(),
                })
            })
        }
        fn count_tokens(&self, _messages: &[Message]) -> BoxFuture<'_, usize> {
            Box::pin(async { 0 })
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let runner = make_runner(Arc::new(ExplodingProvider), tmp.path());
    let err = runner
        .spawn(spec(), "x".to_string())
        .await
        .expect_err("LLM 失败应上抛");
    assert!(matches!(err, minicoding_core::model::RuntimeError::Llm(_)));
}

#[tokio::test]
async fn fifth_concurrent_spawn_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let release = Arc::new(Notify::new());
    let runner = Arc::new(make_runner(
        Arc::new(BlockingProvider {
            release: release.clone(),
        }),
        tmp.path(),
    ));

    // 并发派发 MAX+1 个：latch 未释放时 4 个持许可挂起，第 5 个 try_acquire 失败。
    let mut handles = Vec::new();
    for i in 0..=MAX_CONCURRENT_SUBAGENTS {
        let runner = runner.clone();
        handles.push(tokio::spawn(async move {
            runner.spawn(spec(), format!("task-{i}")).await
        }));
    }

    // 挂起的持许可任务不会自行结束——用超时区分"持许可挂起"与"立即被拒"，
    // 避免 join 死锁；随后统一释放 latch。
    let mut ok_or_pending = 0;
    let mut rejected = 0;
    for h in handles {
        // 挂起的持许可任务不会自行结束——用超时区分"持许可挂起"与"立即被拒"。
        match tokio::time::timeout(std::time::Duration::from_millis(500), h).await {
            Err(_elapsed) => ok_or_pending += 1,
            Ok(joined) => match joined {
                // latch 未释放不应有完成的 spawn（BlockingProvider 阻塞中）。
                Ok(Ok(_)) => panic!("latch 未释放时不应有 spawn 完成"),
                Ok(Err(minicoding_core::model::RuntimeError::Config(msg))) => {
                    assert!(msg.contains("并发子代理已达上限"), "{msg}");
                    rejected += 1;
                }
                Ok(Err(e)) => panic!("意外错误: {e}"),
                Err(join_err) => panic!("task panic/join error: {join_err:?}"),
            },
        }
    }
    assert_eq!(rejected, 1, "恰好 1 个应被拒绝");
    assert_eq!(
        ok_or_pending, MAX_CONCURRENT_SUBAGENTS,
        "其余 {MAX_CONCURRENT_SUBAGENTS} 个应持许可挂起"
    );

    // 释放 latch 让挂起的 spawn 结束。
    release.notify_waiters();
}

#[tokio::test]
async fn child_tool_names_exclude_dispatch_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let runner = make_runner(Arc::new(ScriptedProvider::new(&["ok"])), tmp.path());
    let names = runner.child_tool_names();

    assert!(
        names.contains(&"fs.read".to_string()),
        "只读工具保留: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "task.spawn"),
        "task.spawn 必须物理缺席（深度防御 F1）: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.starts_with("plan.")),
        "plan 工具缺席"
    );
    assert!(
        !names.iter().any(|n| n == "memory.write"),
        "memory.write 缺席"
    );
}

#[tokio::test]
async fn summary_truncated_at_2000_chars() {
    let long_text = "长".repeat(3000);
    let tmp = tempfile::tempdir().unwrap();
    let runner = make_runner(
        Arc::new(ScriptedProvider::new(&[long_text.as_str()])),
        tmp.path(),
    );
    let result = runner.spawn(spec(), "long".to_string()).await.unwrap();
    let chars = result.summary.chars().count();
    // 2000 正文 + 16 字符截断标注（"\n[... truncated]"）。
    assert!(
        (2000..=2020).contains(&chars),
        "摘要应截断至 ~2000 字符 + 标注: {chars}"
    );
    assert!(result.summary.ends_with("[... truncated]"));
}

#[tokio::test]
async fn worktree_runner_wraps_inner_via_public_api() {
    // 生产组装形态回归：WorktreeSubagentRunner(InProcessSubagentRunner) 组合可用，
    // 非 git 目录自动降级 Shared 后委托内层成功。
    use minicoding_tools::WorktreeSubagentRunner;
    let tmp = tempfile::tempdir().unwrap();
    let inner = make_runner(Arc::new(ScriptedProvider::new(&["wrapped-ok"])), tmp.path());
    let workdir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap();
    let outer = WorktreeSubagentRunner::new(Arc::new(inner), workdir);
    let r = outer
        .spawn(spec(), "wrap".to_string())
        .await
        .expect("组合 runner 应成功");
    assert_eq!(r.summary, "wrapped-ok");
}
