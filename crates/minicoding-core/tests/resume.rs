//! T-M3-10a 集成测试：`--resume` 会话恢复（`Runtime::restore_history`）。
//!
//! 验证预加载会话的消息被正确注入上下文管理器，后续 `run_turn` 能基于历史继续。

mod common;

use std::sync::Arc;

use camino::Utf8PathBuf;
use minicoding_core::config::RuntimeConfig;
use minicoding_core::model::{Message, Session, TurnOutcome, UserInput};
use minicoding_core::runtime::{Runtime, RuntimeBuilder};
use minicoding_core::tool::ToolRegistry;

use common::{InMemoryStorage, ScriptedProvider, TestContext, text_deltas};

/// 构造带预加载会话的 Runtime（`--resume` 路径）。
fn build_runtime_with_session(provider: ScriptedProvider, session: Session) -> Runtime {
    RuntimeBuilder::new()
        .provider(Arc::new(provider))
        .context(Arc::new(TestContext::new("test system prompt")))
        .storage(Arc::new(InMemoryStorage::new()))
        .tools(ToolRegistry::new())
        .config(RuntimeConfig::default())
        .workdir(Utf8PathBuf::from("."))
        .session(session)
        .build()
        .expect("runtime build")
}

/// `restore_history` 将预加载会话的消息注入上下文管理器：
/// - `message_count` 反映历史消息数；
/// - `build_chat_request` 的 messages 包含历史。
#[tokio::test]
async fn restore_history_injects_messages_into_context() {
    let history = vec![
        Message::user_text("之前的问题"),
        Message::assistant_text("之前的回答"),
    ];
    let session = Session {
        id: "01RESUME01".to_string(),
        created_at: time::OffsetDateTime::now_utc(),
        workdir: Utf8PathBuf::from("."),
        config_hash: 0,
        messages: history,
    };
    // provider 不会被调用到（仅验证 restore，不 run_turn）
    let provider = ScriptedProvider::new(vec![text_deltas("ok")]);
    let rt = build_runtime_with_session(provider, session);

    // restore 前：上下文为空
    let ctx = rt.context();
    assert_eq!(
        ctx.message_count(),
        0,
        "context should be empty before restore"
    );

    // restore 后：上下文含历史消息
    rt.restore_history().await.expect("restore should succeed");
    assert_eq!(
        ctx.message_count(),
        2,
        "context should have 2 messages after restore"
    );
}

/// `restore_history` 后 `run_turn` 将历史消息包含在 ChatRequest 中发给 LLM。
///
/// 注意：`restore_history` 只回填上下文管理器，**不重复落盘**历史消息——
/// 历史消息已在磁盘（真实 `--resume` 路径从 storage 加载）。storage 仅追加
/// 本次 turn 的新消息（user + assistant）。
#[tokio::test]
async fn resume_then_run_turn_includes_history_in_request() {
    let history = vec![
        Message::user_text("我叫小明"),
        Message::assistant_text("你好，小明"),
    ];
    let session = Session {
        id: "01RESUME02".to_string(),
        created_at: time::OffsetDateTime::now_utc(),
        workdir: Utf8PathBuf::from("."),
        config_hash: 0,
        messages: history,
    };
    let provider = ScriptedProvider::new(vec![text_deltas("记得你叫小明")]);
    let rt = build_runtime_with_session(provider, session);

    rt.restore_history().await.expect("restore should succeed");
    // restore 后上下文含 2 条历史消息
    assert_eq!(rt.context().message_count(), 2);

    let outcome = rt.run_turn(UserInput::from_text("我叫什么？")).await;
    let outcome = outcome.expect("turn should succeed");

    match outcome {
        TurnOutcome::Finished(msg) => {
            assert_eq!(msg.text(), "记得你叫小明");
        }
        other => panic!("expected Finished, got {other:?}"),
    }

    // 上下文应含 4 条消息（2 历史 + user + assistant）
    assert_eq!(
        rt.context().message_count(),
        4,
        "context should have 4 messages after turn"
    );

    // storage 仅含本次 turn 的新消息（历史不重复落盘）
    let messages = rt.storage().load(&"01RESUME02".to_string()).await.unwrap();
    assert_eq!(
        messages.len(),
        2,
        "storage should have 2 messages (new user + assistant only)"
    );
    assert_eq!(messages[0].text(), "我叫什么？");
    assert_eq!(messages[1].text(), "记得你叫小明");
}

/// 对空会话（无预加载消息）调用 `restore_history` 是 no-op，不报错。
#[tokio::test]
async fn restore_history_on_empty_session_is_noop() {
    let session = Session::new(Utf8PathBuf::from("."), 0);
    let provider = ScriptedProvider::new(vec![text_deltas("ok")]);
    let rt = build_runtime_with_session(provider, session);

    rt.restore_history().await.expect("restore should succeed");
    let ctx = rt.context();
    assert_eq!(ctx.message_count(), 0, "context should remain empty");
}
