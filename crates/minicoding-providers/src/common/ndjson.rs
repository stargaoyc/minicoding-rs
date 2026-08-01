//! 通用 NDJSON（Newline-Delimited JSON）解析器（T-M6-2 Ollama 用）。
//!
//! [`NdjsonStream`] 消费底层字节流，按 `\n` 切分行，逐行 yield 完整 JSON 字符串。
//! 与 SSE 的差异：NDJSON 以换行为边界（非 `\n\n`），无 `data:` 前缀，无事件类型字段。
//!
//! 设计依据：`design.md` §4.3 NDJSON 解析、`rules.md` C-12（事件流解析容错）。

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::stream::{Stream, StreamExt};
use minicoding_core::model::LlmError;

use super::sse::ByteStream;

/// NDJSON 解析流：逐行 yield JSON 字符串（已 trim，空行跳过）。
pub struct NdjsonStream {
    inner: ByteStream,
    buffer: String,
    done: bool,
}

impl NdjsonStream {
    /// 构造 NDJSON 解析器。
    #[must_use]
    pub fn new(inner: ByteStream) -> Self {
        Self {
            inner,
            buffer: String::new(),
            done: false,
        }
    }

    /// 从 buffer 取出一行（以 `\n` 分隔），返回 trim 后的字符串。
    /// 空行返回 `None`（调用方继续拉取）。
    fn take_line(&mut self) -> Option<String> {
        let pos = self.buffer.find('\n')?;
        let line: String = self.buffer.drain(..=pos).collect();
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }
}

impl Stream for NdjsonStream {
    type Item = Result<String, LlmError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // 1) 优先消费已缓冲的完整行
            if let Some(line) = self.take_line() {
                return Poll::Ready(Some(Ok(line)));
            }

            // 2) buffer 不完整时尝试 flush 末尾残留（流结束后）
            if self.done {
                if !self.buffer.is_empty() {
                    let line = std::mem::take(&mut self.buffer);
                    let trimmed = line.trim().to_string();
                    if trimmed.is_empty() {
                        return Poll::Ready(None);
                    }
                    return Poll::Ready(Some(Ok(trimmed)));
                }
                return Poll::Ready(None);
            }

            // 3) 拉取底层流
            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    self.buffer.push_str(&String::from_utf8_lossy(&bytes));
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(LlmError::Network(e.to_string()))));
                }
                Poll::Ready(None) => {
                    self.done = true;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// 从 `reqwest::Response` 构造 `NdjsonStream`（消费 response，返回 `'static` 流）。
pub fn from_response(resp: reqwest::Response) -> NdjsonStream {
    let byte_stream = resp.bytes_stream().map(|r| r.map(|b| b.to_vec())).boxed();
    NdjsonStream::new(byte_stream)
}
