//! M1 简单上下文管理器：仅持有消息列表，无压缩管道。

use std::sync::atomic::{AtomicUsize, Ordering};

use minicoding_core::config::RuntimeConfig;
use minicoding_core::context::{ContextManager, ContextSnapshot};
use minicoding_core::model::{Message, RuntimeError};
use minicoding_core::provider::{BoxFuture, ChatRequest, GenerationParams};
use minicoding_core::tool::ToolRegistry;

/// M1 简单上下文管理器。
///
/// 持有消息列表与系统提示词，实现 `ContextManager` trait。不包含压缩管道
/// （M3 实现），`token_count` 恒为 0（真实分词需 M2 注入 `Tokenizer`）。
#[derive(Debug)]
pub struct SimpleContextManager {
    messages: tokio::sync::RwLock<Vec<Message>>,
    system_prompt: String,
    // 同步计数器：`message_count` 是 sync 方法，无法获取 tokio async 锁，
    // 故用 AtomicUsize 在 `append`/`restore` 时同步维护。
    count: AtomicUsize,
}

impl SimpleContextManager {
    /// 创建指定系统提示词的上下文管理器。
    #[must_use]
    pub fn new(system_prompt: String) -> Self {
        Self {
            messages: tokio::sync::RwLock::new(Vec::new()),
            system_prompt,
            count: AtomicUsize::new(0),
        }
    }

    /// 创建使用默认系统提示词的上下文管理器。
    #[must_use]
    pub fn with_default_system() -> Self {
        Self::new(
            "You are minicoding, a terminal AI coding assistant. Follow the user's instructions carefully.".into(),
        )
    }
}

impl ContextManager for SimpleContextManager {
    fn append(&self, msg: Message) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.messages.write().await.push(msg);
            self.count.fetch_add(1, Ordering::SeqCst);
        })
    }

    fn build_chat_request(
        &self,
        tools: &ToolRegistry,
        config: &RuntimeConfig,
    ) -> BoxFuture<'_, Result<ChatRequest, RuntimeError>> {
        // 在 async 块外提取所需数据，避免 future 捕获 tools/config 引用
        // （其生命周期与 &self 独立，捕获会导致生命周期不匹配）。
        let tool_schemas = tools.schemas();
        let model = config.provider.model.clone();
        Box::pin(async move {
            let guard = self.messages.read().await;
            // ProviderConfig 暂无 temperature/max_output_tokens 字段，M1 置 None。
            let params = GenerationParams {
                model,
                temperature: None,
                top_p: None,
                max_output_tokens: None,
                stop: Vec::new(),
                seed: None,
            };
            Ok(ChatRequest {
                system: self.system_prompt.clone(),
                messages: guard.clone(),
                tools: tool_schemas,
                params,
            })
        })
    }

    fn snapshot(&self) -> BoxFuture<'_, ContextSnapshot> {
        Box::pin(async move {
            let guard = self.messages.read().await;
            ContextSnapshot {
                messages: guard.clone(),
                token_count: 0,
            }
        })
    }

    fn restore(&self, snap: ContextSnapshot) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let new_count = snap.messages.len();
            let mut guard = self.messages.write().await;
            *guard = snap.messages;
            self.count.store(new_count, Ordering::SeqCst);
        })
    }

    fn token_count(&self) -> usize {
        // M1 不接入 Tokenizer，恒返回 0（M2 注入分词器后实现真实计数）。
        0
    }

    fn message_count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}
