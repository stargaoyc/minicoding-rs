//! 共享测试工具（mock provider / mock tool / in-memory storage）。
//!
//! 供 `crates/minicoding-core/tests/` 下集成测试复用，见 AGENTS.md §2.8。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;

use futures::stream;
use minicoding_core::context::{ContextManager, ContextSnapshot};
use minicoding_core::model::SessionId;
use minicoding_core::model::{
    LlmError, Message, RuntimeError, SideEffect, ToolError, ToolResult, ToolSchema,
};
use minicoding_core::provider::{
    BoxFuture, BoxStream, Capabilities, ChatRequest, Delta, LlmProvider, Tokenizer, ToolCallDelta,
    Usage,
};
use minicoding_core::storage::{EventRecord, EventStore, SessionListItem, Storage, StorageError};
use minicoding_core::tool::{Tool, ToolContext};

/// 简单分词器（按字符数估算，仅用于测试）。
#[derive(Debug, Default)]
pub struct CharTokenizer;

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

/// 脚本化 mock provider：按调用次数依次返回预设的 `Delta` 序列。
///
/// 每次 `chat_stream` 调用消费一个预设脚本（第 n 次调用取 `scripts[n]`）。
/// 脚本耗尽时返回错误，便于测试发现意外重试。
pub struct ScriptedProvider {
    scripts: Mutex<VecDeque<Vec<Delta>>>,
    tokenizer: Arc<CharTokenizer>,
    /// 记录实际收到的请求（RT-2 回归：断言提醒并入 system 用）。
    received: Mutex<Vec<ChatRequest>>,
}

impl ScriptedProvider {
    /// 创建 mock provider，`scripts` 按调用顺序消费。
    #[must_use]
    pub fn new(scripts: Vec<Vec<Delta>>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into()),
            tokenizer: Arc::new(CharTokenizer),
            received: Mutex::new(Vec::new()),
        }
    }

    /// 取出实际收到（含 Runtime 注入后的 system）的请求快照。
    #[allow(dead_code)]
    pub fn take_received(&self) -> Vec<ChatRequest> {
        std::mem::take(&mut *self.received.lock().expect("received poisoned"))
    }
}

impl std::fmt::Debug for ScriptedProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptedProvider")
            .field(
                "remaining_scripts",
                &self.scripts.lock().map_or(0, |s| s.len()),
            )
            .finish_non_exhaustive()
    }
}

impl LlmProvider for ScriptedProvider {
    fn id(&self) -> &'static str {
        "mock"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tool_call: true,
            supports_vision: false,
            supports_streaming: true,
            supports_json_mode: false,
            context_window: 8_000,
            max_output: 1_000,
        }
    }
    fn tokenizer(&self) -> Arc<dyn Tokenizer> {
        self.tokenizer.clone()
    }
    fn chat_stream(
        &self,
        req: ChatRequest,
    ) -> BoxFuture<'_, Result<BoxStream<'static, Result<Delta, LlmError>>, LlmError>> {
        if let Ok(mut r) = self.received.lock() {
            r.push(req.clone());
        }
        let mut guard = self.scripts.lock().expect("scripts poisoned");
        let script = guard
            .pop_front()
            .ok_or_else(|| LlmError::Network("no more scripts in mock".into()));
        Box::pin(async move {
            let script = script?;
            let stream = stream::iter(script.into_iter().map(Ok::<_, LlmError>));
            Ok(Box::pin(stream) as BoxStream<'static, _>)
        })
    }
    fn count_tokens(&self, messages: &[Message]) -> BoxFuture<'_, usize> {
        let n = self.tokenizer.count_messages(messages);
        Box::pin(async move { n })
    }
}

/// mock 工具：根据名称返回固定结果，记录被调用的入参。
#[allow(dead_code)]
pub struct MockTool {
    name: String,
    side_effect: SideEffect,
    /// 返回固定结果（文本）。
    response: String,
    /// 记录所有调用入参（按调用顺序）。
    calls: Mutex<Vec<serde_json::Value>>,
}

impl MockTool {
    /// 创建只读 mock 工具。
    #[must_use]
    #[allow(dead_code)]
    pub fn read_only(name: &str, response: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            side_effect: SideEffect::None,
            response: response.into(),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// 取出所有调用入参快照。
    #[allow(dead_code)]
    pub fn take_calls(&self) -> Vec<serde_json::Value> {
        std::mem::take(&mut *self.calls.lock().expect("calls poisoned"))
    }

    /// 创建命令类 mock 工具（`SideEffect::Command`，走副作用权限路径）。
    #[must_use]
    #[allow(dead_code)]
    pub fn command(name: &str, response: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            side_effect: SideEffect::Command,
            response: response.into(),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// 创建文件写入类 mock 工具（`SideEffect::FileWrite`，走 `check_file_write`
    /// 权限路径——C-23 restricted ask 回归测试用）。
    #[must_use]
    #[allow(dead_code)]
    pub fn file_write(name: &str, response: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            side_effect: SideEffect::FileWrite,
            response: response.into(),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// 创建执行必失败的 mock 工具（M-09 沙箱拒绝检测测试用）。
    #[must_use]
    #[allow(dead_code)]
    pub fn failing(name: &str, error_text: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            side_effect: SideEffect::None,
            response: format!("__FAIL__:{error_text}", error_text = error_text.into()),
            calls: Mutex::new(Vec::new()),
        }
    }
}

impl std::fmt::Debug for MockTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockTool")
            .field("name", &self.name)
            .field("side_effect", &self.side_effect)
            .finish_non_exhaustive()
    }
}

impl Tool for MockTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn schema(&self) -> &ToolSchema {
        // 测试用 schema 不参与校验，给个最小占位
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema {
            name: String::new(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
        })
    }
    fn side_effect(&self) -> SideEffect {
        self.side_effect
    }
    fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        self.calls.lock().expect("calls poisoned").push(input);
        let resp = self.response.clone();
        Box::pin(async move {
            if let Some(err) = resp.strip_prefix("__FAIL__:") {
                return Err(ToolError::Exec(err.to_string()));
            }
            Ok(ToolResult::ok_text(resp))
        })
    }
}

/// 内存存储（无需磁盘 IO，便于快速测试）。
///
/// M-13 起按会话分桶（`HashMap<SessionId, Vec<Message>>`），实现完整 `Storage`
/// `语义（list/delete/update_summary），可运行` `testing::storage_contract` 契约
/// 断言——与 JSONL 后端共享同一套行为保证。
#[derive(Default)]
pub struct InMemoryStorage {
    sessions: Mutex<HashMap<SessionId, SessionEntry>>,
}

#[derive(Debug, Default)]
struct SessionEntry {
    messages: Vec<Message>,
    summary: Option<String>,
}

impl InMemoryStorage {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Storage for InMemoryStorage {
    fn append(
        &self,
        session: &SessionId,
        msg: &Message,
    ) -> BoxFuture<'_, Result<(), StorageError>> {
        let msg = msg.clone();
        let sid = session.clone();
        let sessions = &self.sessions;
        Box::pin(async move {
            sessions
                .lock()
                .expect("storage poisoned")
                .entry(sid)
                .or_default()
                .messages
                .push(msg);
            Ok(())
        })
    }
    fn load(&self, session: &SessionId) -> BoxFuture<'_, Result<Vec<Message>, StorageError>> {
        let msgs = self
            .sessions
            .lock()
            .expect("storage poisoned")
            .get(session)
            .map_or_else(Vec::new, |e| e.messages.clone());
        Box::pin(async move { Ok(msgs) })
    }
    fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<SessionListItem>, StorageError>> {
        let metas: Vec<SessionListItem> = self
            .sessions
            .lock()
            .expect("storage poisoned")
            .iter()
            .map(|(id, e)| SessionListItem {
                id: id.clone(),
                created_at: time::OffsetDateTime::now_utc(),
                message_count: e.messages.len(),
                last_message_at: time::OffsetDateTime::now_utc(),
                summary: e.summary.clone(),
            })
            .collect();
        Box::pin(async move { Ok(metas) })
    }
    fn delete(&self, session: &SessionId) -> BoxFuture<'_, Result<(), StorageError>> {
        self.sessions
            .lock()
            .expect("storage poisoned")
            .remove(session);
        Box::pin(async move { Ok(()) })
    }
    fn update_summary(
        &self,
        session: &SessionId,
        summary: &str,
    ) -> BoxFuture<'_, Result<(), StorageError>> {
        let summary = summary.to_string();
        let sid = session.clone();
        let sessions = &self.sessions;
        Box::pin(async move {
            sessions
                .lock()
                .expect("storage poisoned")
                .entry(sid)
                .or_default()
                .summary = Some(summary);
            Ok(())
        })
    }
}

/// 内存事件存储（M-06 测试用）：记录全部 `EventRecord`，seq 单调递增。
/// 仅 `agent_loop` 集成测试使用；共享模块被其它测试编译时属预期未引用。
#[allow(dead_code)]
#[derive(Debug, Default)]
pub struct InMemoryEventStore {
    records: Mutex<Vec<EventRecord>>,
}

#[allow(dead_code)]
impl InMemoryEventStore {
    /// 创建空事件存储。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 取出全部记录快照（按 seq 升序）。
    #[must_use]
    pub fn snapshot(&self) -> Vec<EventRecord> {
        self.records.lock().expect("records poisoned").clone()
    }
}

#[allow(dead_code)]
impl EventStore for InMemoryEventStore {
    fn append(
        &self,
        _session: &SessionId,
        record: EventRecord,
    ) -> BoxFuture<'_, Result<(), StorageError>> {
        let mut guard = self.records.lock().expect("records poisoned");
        guard.push(record);
        Box::pin(async move { Ok(()) })
    }
    fn load(&self, _session: &SessionId) -> BoxFuture<'_, Result<Vec<EventRecord>, StorageError>> {
        let records = self.records.lock().expect("records poisoned").clone();
        Box::pin(async move { Ok(records) })
    }
    fn load_after(
        &self,
        _session: &SessionId,
        after_seq: u64,
    ) -> BoxFuture<'_, Result<Vec<EventRecord>, StorageError>> {
        let records: Vec<EventRecord> = self
            .records
            .lock()
            .expect("records poisoned")
            .iter()
            .filter(|r| r.seq > after_seq)
            .cloned()
            .collect();
        Box::pin(async move { Ok(records) })
    }
    fn next_seq(&self, _session: &SessionId) -> BoxFuture<'_, Result<u64, StorageError>> {
        let next = self.records.lock().expect("records poisoned").len() as u64 + 1;
        Box::pin(async move { Ok(next) })
    }
    fn delete(&self, _session: &SessionId) -> BoxFuture<'_, Result<(), StorageError>> {
        self.records.lock().expect("records poisoned").clear();
        Box::pin(async move { Ok(()) })
    }
}

/// 简单 context manager（用于测试，持有消息列表）。
pub struct TestContext {
    messages: tokio::sync::RwLock<Vec<Message>>,
    system: String,
    /// 记录每次 `build_chat_request` 的请求快照（RT-2 回归：断言提醒注入
    /// 位置/内容用）。
    requests: tokio::sync::Mutex<Vec<minicoding_core::provider::ChatRequest>>,
}

impl TestContext {
    #[must_use]
    pub fn new(system: impl Into<String>) -> Self {
        Self {
            messages: tokio::sync::RwLock::new(Vec::new()),
            system: system.into(),
            requests: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    /// 取出全部已构建的请求快照。
    #[allow(dead_code)]
    pub async fn take_requests(&self) -> Vec<minicoding_core::provider::ChatRequest> {
        std::mem::take(&mut *self.requests.lock().await)
    }
}

impl ContextManager for TestContext {
    fn append(&self, msg: Message) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.messages.write().await.push(msg);
        })
    }
    fn build_chat_request(
        &self,
        tools: &minicoding_core::tool::ToolRegistry,
        config: &minicoding_core::config::RuntimeConfig,
    ) -> BoxFuture<'_, Result<minicoding_core::provider::ChatRequest, RuntimeError>> {
        let tool_schemas = tools.schemas();
        let model = config.provider.model.clone();
        let system = self.system.clone();
        Box::pin(async move {
            let messages = self.messages.read().await.clone();
            let req = ChatRequest {
                system,
                messages,
                tools: tool_schemas,
                params: minicoding_core::provider::GenerationParams {
                    model,
                    temperature: None,
                    top_p: None,
                    max_output_tokens: None,
                    stop: Vec::new(),
                    seed: None,
                    thinking_budget_tokens: None,
                },
            };
            self.requests.lock().await.push(req.clone());
            Ok(req)
        })
    }
    fn snapshot(&self) -> BoxFuture<'_, ContextSnapshot> {
        Box::pin(async move {
            let messages = self.messages.read().await.clone();
            ContextSnapshot {
                messages,
                token_count: 0,
            }
        })
    }
    fn restore(&self, snap: ContextSnapshot) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let mut guard = self.messages.write().await;
            *guard = snap.messages;
        })
    }
    fn token_count(&self) -> usize {
        0
    }
    fn message_count(&self) -> usize {
        // 同步读取：try_read 失败时返回 0（仅测试用，不阻塞）
        self.messages.try_read().map_or(0, |g| g.len())
    }
}

/// 构造一个工具调用 delta 序列（含分片）。
#[must_use]
#[allow(dead_code)]
pub fn tool_call_deltas(call_id: &str, name: &str, args_json: &str) -> Vec<Delta> {
    vec![
        Delta::ToolCall(ToolCallDelta {
            index: 0,
            id: Some(call_id.to_string()),
            name: Some(name.to_string()),
            args_chunk: Some(args_json.to_string()),
        }),
        Delta::Stop(minicoding_core::model::StopReason::ToolUse),
        Delta::Usage(Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read: None,
            cache_write: None,
        }),
    ]
}

/// 构造一个纯文本 delta 序列。
#[must_use]
pub fn text_deltas(text: &str) -> Vec<Delta> {
    vec![
        Delta::Text(text.to_string()),
        Delta::Stop(minicoding_core::model::StopReason::EndTurn),
    ]
}
