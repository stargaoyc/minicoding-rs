//! 通用 SSE（Server-Sent Events）解析器（T-M6-1/3 共享）。
//!
//! [`SseStream`] 消费底层字节流，按 `\n\n` 切分事件，提取 `data:` 行拼接为 payload
//! 字符串后逐个 yield。不解析 JSON（由各 provider 自行解析），不处理 `[DONE]` 哨兵
//! （`OpenAI` 在消费侧判断；Anthropic 用 `message_stop` 事件终止）。
//!
//! 设计依据：`design.md` §4.3 SSE 解析、`rules.md` C-12（事件流解析容错）。

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::stream::{Stream, StreamExt};
use minicoding_core::model::LlmError;

/// 底层字节流类型（`reqwest::Response::bytes_stream` 转换后）。
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, reqwest::Error>> + Send>>;

/// SSE 解析流：yield 事件的 `data` payload 字符串。
pub struct SseStream {
    inner: ByteStream,
    buffer: String,
    done: bool,
}

impl SseStream {
    /// 构造 SSE 解析器。
    #[must_use]
    pub fn new(inner: ByteStream) -> Self {
        Self {
            inner,
            buffer: String::new(),
            done: false,
        }
    }

    /// 从 buffer 取出一个完整事件（以 `\n\n` 分隔），返回原始事件文本。
    fn take_event(&mut self) -> Option<String> {
        let pos = self.buffer.find("\n\n")?;
        let event: String = self.buffer.drain(..pos + 2).collect();
        Some(event)
    }

    /// 从单个事件提取 `data:` payload（多行 `data:` 拼接为 `\n`）。
    /// 无 `data:` 行返回 `None`（如纯 `event:` 行或心跳）。
    fn extract_data(event: &str) -> Option<String> {
        let mut data_lines: Vec<&str> = Vec::new();
        for line in event.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                let trimmed = rest.strip_prefix(' ').unwrap_or(rest).trim();
                data_lines.push(trimmed);
            }
        }
        if data_lines.is_empty() {
            return None;
        }
        Some(data_lines.join("\n"))
    }
}

impl Stream for SseStream {
    type Item = Result<String, LlmError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // 1) 优先消费已缓冲的完整事件
            if let Some(event) = self.take_event() {
                match Self::extract_data(&event) {
                    Some(data) => return Poll::Ready(Some(Ok(data))),
                    None => continue, // 无 data 行（心跳/纯 event 行），跳过
                }
            }

            // 2) buffer 不完整时尝试 flush 末尾残留（流结束后）
            if self.done {
                if !self.buffer.is_empty() {
                    let event = std::mem::take(&mut self.buffer);
                    return Poll::Ready(Self::extract_data(&event).map(Ok));
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

/// 从 `reqwest::Response` 构造 `SseStream`（消费 response，返回 `'static` 流）。
///
/// 各 provider 调用此函数把 HTTP 响应体转为 SSE 事件流。
pub fn from_response(resp: reqwest::Response) -> SseStream {
    let byte_stream = resp.bytes_stream().map(|r| r.map(|b| b.to_vec())).boxed();
    SseStream::new(byte_stream)
}
