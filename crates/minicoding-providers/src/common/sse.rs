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
    /// 字节缓冲区（非 String）：避免 chunk 边界切断 UTF-8 字符导致 `from_utf8_lossy` 产生乱码。
    /// 仅在事件边界（`\n\n`）进行 UTF-8 解码，保证跨 chunk 的多字节字符正确拼接。
    buffer: Vec<u8>,
    done: bool,
}

impl SseStream {
    /// 构造 SSE 解析器。
    #[must_use]
    pub fn new(inner: ByteStream) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            done: false,
        }
    }

    /// 从 buffer 取出一个完整事件（以 `\n\n` 分隔），返回原始事件文本。
    fn take_event(&mut self) -> Option<String> {
        let pos = self.buffer.windows(2).position(|w| w == b"\n\n")?;
        let event_bytes: Vec<u8> = self.buffer.drain(..pos + 2).collect();
        // 事件边界解码：完整事件的字节序列应是有效 UTF-8
        Some(String::from_utf8_lossy(&event_bytes).into_owned())
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
                    let event_bytes = std::mem::take(&mut self.buffer);
                    let event = String::from_utf8_lossy(&event_bytes).into_owned();
                    return Poll::Ready(Self::extract_data(&event).map(Ok));
                }
                return Poll::Ready(None);
            }

            // 3) 拉取底层流：字节直接追加到 buffer，不在 chunk 边界解码 UTF-8
            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    self.buffer.extend_from_slice(&bytes);
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

#[cfg(test)]
mod tests {
    #![allow(clippy::needless_pass_by_value)]

    use super::*;
    use futures::stream::{self, StreamExt};

    /// 构造合成字节流：把字符串切片逐个转为 `Ok(Vec<u8>)`，模拟 `reqwest` 的 `bytes_stream`。
    fn byte_stream(chunks: Vec<&str>) -> ByteStream {
        let items: Vec<Result<Vec<u8>, reqwest::Error>> = chunks
            .into_iter()
            .map(|s| Ok(s.as_bytes().to_vec()))
            .collect();
        stream::iter(items).boxed()
    }

    /// 收集 `SseStream` 所有 Ok 事件为字符串向量（遇 Err 则 panic，便于定位）。
    async fn collect_ok(mut sse: SseStream) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(item) = sse.next().await {
            match item {
                Ok(s) => out.push(s),
                Err(e) => panic!("未预期的 SSE 错误: {e:?}"),
            }
        }
        out
    }

    #[tokio::test]
    async fn single_data_event_decoded() {
        // 单行 `data: {...}\n\n` → yield 一个 payload
        let sse = SseStream::new(byte_stream(vec!["data: {\"a\":1}\n\n"]));
        assert_eq!(collect_ok(sse).await, vec!["{\"a\":1}"]);
    }

    #[tokio::test]
    async fn multi_line_data_joined_with_newline() {
        // 多行 `data:` 拼接为 `\n` 分隔的单个 payload
        let sse = SseStream::new(byte_stream(vec!["data: line1\ndata: line2\n\n"]));
        assert_eq!(collect_ok(sse).await, vec!["line1\nline2"]);
    }

    #[tokio::test]
    async fn done_sentinel_yielded_as_data() {
        // SseStream 不解释 `[DONE]`，原样 yield（由消费方 chat_stream 判断流终止）
        let sse = SseStream::new(byte_stream(vec!["data: [DONE]\n\n"]));
        assert_eq!(collect_ok(sse).await, vec!["[DONE]"]);
    }

    #[tokio::test]
    async fn comment_and_event_only_lines_skipped() {
        // `:heartbeat` 心跳注释行与纯 `event:` 行无 `data:` 字段，被跳过
        let input = ":heartbeat\n\nevent: ping\ndata: hello\n\n";
        let sse = SseStream::new(byte_stream(vec![input]));
        assert_eq!(collect_ok(sse).await, vec!["hello"]);
    }

    #[tokio::test]
    async fn cross_chunk_event_assembled() {
        // 事件跨多个字节 chunk 边界，buffer 应正确拼接
        let sse = SseStream::new(byte_stream(vec!["data: {\"par", "t\":1}\n\n"]));
        assert_eq!(collect_ok(sse).await, vec!["{\"part\":1}"]);
    }

    #[tokio::test]
    async fn utf8_multibyte_cross_chunk_boundary() {
        // UTF-8 多字节字符跨 chunk 边界：不能在 chunk 边界用 from_utf8_lossy
        // "你好" 的 "你" = [E4 BD A0]，在第 7 字节切分（"你" 的首字节之后，非字符边界）
        let full = "data: 你好\n\n";
        let bytes = full.as_bytes();
        let mid = 7; // "data: "(6) + "你"首字节(1) = 7，非 char boundary
        let items: Vec<Result<Vec<u8>, reqwest::Error>> =
            vec![Ok(bytes[..mid].to_vec()), Ok(bytes[mid..].to_vec())];
        let sse = SseStream::new(stream::iter(items).boxed());
        assert_eq!(collect_ok(sse).await, vec!["你好"]);
    }

    #[tokio::test]
    async fn multiple_events_in_one_chunk() {
        // 单个 chunk 含多个事件，逐个 yield
        let sse = SseStream::new(byte_stream(vec!["data: one\n\ndata: two\n\n"]));
        assert_eq!(collect_ok(sse).await, vec!["one", "two"]);
    }

    #[tokio::test]
    async fn trailing_data_flushed_on_stream_end() {
        // 流结束时残留 buffer（无 `\n\n` 结尾）应被 flush 为最后一个事件
        let sse = SseStream::new(byte_stream(vec!["data: tail"]));
        assert_eq!(collect_ok(sse).await, vec!["tail"]);
    }

    #[tokio::test]
    async fn data_without_space_prefix() {
        // `data:` 后无空格也应解析（strip_prefix(' ') 失败时回退原值再 trim）
        let sse = SseStream::new(byte_stream(vec!["data:nospace\n\n"]));
        assert_eq!(collect_ok(sse).await, vec!["nospace"]);
    }

    #[tokio::test]
    async fn empty_stream_yields_nothing() {
        let sse = SseStream::new(byte_stream(vec![]));
        assert!(collect_ok(sse).await.is_empty());
    }

    #[test]
    fn extract_data_returns_none_for_no_data_lines() {
        // 纯 `event:` / `:comment` 行无 data，extract_data 返回 None
        assert!(SseStream::extract_data("event: ping\n\n").is_none());
        assert!(SseStream::extract_data(":comment\n\n").is_none());
    }

    #[test]
    fn extract_data_trims_payload() {
        // data 后的前导/尾随空白被 trim
        assert_eq!(
            SseStream::extract_data("data:   hi   \n\n"),
            Some("hi".to_string())
        );
    }

    #[test]
    fn extract_data_joins_multi_line() {
        let event = "data: a\ndata: b\ndata: c\n\n";
        assert_eq!(SseStream::extract_data(event), Some("a\nb\nc".to_string()));
    }
}
