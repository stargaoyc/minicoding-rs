//! Provider 公共设施：重试/限流/超时装饰器（T-M6-3）。
//!
//! `RetryProvider` 包裹任意 `LlmProvider`，对**请求建立阶段**（`chat_stream` 返回
//! `Err`）的可重试错误（429/5xx/网络/超时）做指数退避重试；优先尊重服务端
//! `Retry-After`。流**建立后**的中途错误不重试（会重复输出），由调用方处理。
//!
//! 设计依据：`design.md` §10 错误分类与恢复策略、`rules.md` C-07（重试上限）/C-13
//! （防死循环）。

pub mod ndjson;
pub mod retry;
pub mod sse;

pub use ndjson::NdjsonStream;
pub use retry::{RetryConfig, RetryProvider};

/// API key 脱敏（前 4 字符 + `***`），用于日志/Debug 输出（C-04）。
#[must_use]
pub fn mask_key(key: &str) -> String {
    let len = key.chars().count();
    if len <= 4 {
        "***".to_string()
    } else {
        let head: String = key.chars().take(4).collect();
        format!("{head}***")
    }
}
