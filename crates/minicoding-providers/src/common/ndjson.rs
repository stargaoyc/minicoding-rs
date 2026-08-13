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

    /// 收集 `NdjsonStream` 所有 Ok 行为字符串向量（遇 Err 则 panic，便于定位）。
    async fn collect_ok(mut stream: NdjsonStream) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(s) => out.push(s),
                Err(e) => panic!("未预期的 NDJSON 错误: {e:?}"),
            }
        }
        out
    }

    #[tokio::test]
    async fn single_line_json() {
        // 单行 JSON → yield 一行（trim 后）
        let stream = NdjsonStream::new(byte_stream(vec!["{\"a\":1}\n"]));
        assert_eq!(collect_ok(stream).await, vec!["{\"a\":1}"]);
    }

    #[tokio::test]
    async fn multiple_lines() {
        // 多行 NDJSON → 逐行 yield
        let stream = NdjsonStream::new(byte_stream(vec!["{\"a\":1}\n{\"b\":2}\n"]));
        assert_eq!(collect_ok(stream).await, vec!["{\"a\":1}", "{\"b\":2}"]);
    }

    #[tokio::test]
    async fn cross_chunk_line_assembled() {
        // 单行跨多个字节 chunk 边界，buffer 应正确拼接
        let stream = NdjsonStream::new(byte_stream(vec!["{\"par", "t\":1}\n"]));
        assert_eq!(collect_ok(stream).await, vec!["{\"part\":1}"]);
    }

    #[tokio::test]
    async fn empty_lines_skipped() {
        // 空行（纯 `\n`）被跳过
        let stream = NdjsonStream::new(byte_stream(vec!["\n\n{\"a\":1}\n"]));
        assert_eq!(collect_ok(stream).await, vec!["{\"a\":1}"]);
    }

    #[tokio::test]
    async fn whitespace_only_line_skipped() {
        // 仅含空白的行 trim 后为空，被跳过
        let stream = NdjsonStream::new(byte_stream(vec!["   \n{\"a\":1}\n"]));
        assert_eq!(collect_ok(stream).await, vec!["{\"a\":1}"]);
    }

    #[tokio::test]
    async fn trailing_line_without_newline_flushed() {
        // 流结束时残留行（无 `\n` 结尾）应被 flush
        let stream = NdjsonStream::new(byte_stream(vec!["{\"a\":1}"]));
        assert_eq!(collect_ok(stream).await, vec!["{\"a\":1}"]);
    }

    #[tokio::test]
    async fn invalid_json_yielded_and_consumer_errors() {
        // NdjsonStream 不解析 JSON，原样 yield 行字符串；
        // JSON 合法性由消费方（ollama::parse_chunk）负责，非法 JSON 在消费方返回
        // LlmError::Parse——此处验证该契约。
        let stream = NdjsonStream::new(byte_stream(vec!["not json\n"]));
        let lines = collect_ok(stream).await;
        assert_eq!(lines, vec!["not json"]);
        // 模拟消费方解析：非法 JSON → 解析失败
        let parse_result = serde_json::from_str::<serde_json::Value>(&lines[0]);
        assert!(parse_result.is_err(), "非法 JSON 应在消费方解析失败");
    }

    #[tokio::test]
    async fn empty_stream_yields_nothing() {
        let stream = NdjsonStream::new(byte_stream(vec![]));
        assert!(collect_ok(stream).await.is_empty(), "stream 应为空");
    }
}
