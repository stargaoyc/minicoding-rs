//! Provider 模块 re-export。

mod router;
mod r#trait;

pub use router::{Router, StaticRouter};
pub use r#trait::{
    BoxFuture, BoxStream, Capabilities, ChatRequest, Delta, GenerationParams, LlmProvider,
    Tokenizer, ToolCallDelta, Usage,
};
