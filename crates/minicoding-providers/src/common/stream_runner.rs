//! Q4：流式请求统一管道——发送 → 状态检查 → 行解码 → `Delta` 映射。
//!
//! 三个 provider（OpenAI/Anthropic/Ollama）的 `chat_stream` 共享同一骨架：
//! POST → 非成功状态映射为错误（429 尊重 `Retry-After`）→ SSE/NDJSON 行流
//! → 逐行 JSON 解析为 `Vec<Delta>`。本模块抽取该骨架，provider 仅保留
//! 差异点：URL/headers 构造、行解码器选择、`parse_chunk` 映射函数。

use futures::StreamExt;
use minicoding_core::model::LlmError;
use minicoding_core::provider::{BoxStream, Delta};
use reqwest::header::HeaderMap;

/// 发送流式请求并做状态检查。
///
/// 非 2xx 响应统一走 `on_error(status, body, headers)` 构造错误
/// （openai/anthropic 在闭包内解析 `Retry-After`；ollama 恒传 `None`）。
pub(crate) async fn send_and_check(
    request: reqwest::RequestBuilder,
    on_error: impl Fn(u16, String, &HeaderMap) -> LlmError,
) -> Result<reqwest::Response, LlmError> {
    let resp = request
        .send()
        .await
        .map_err(|e| LlmError::Network(e.to_string()))?;

    let status = resp.status();
    if !status.is_success() {
        // headers 先取（resp.text() 消耗所有权，与原 provider 实现一致）
        let headers = resp.headers().clone();
        let body_text = resp.text().await.unwrap_or_default();
        return Err(on_error(status.as_u16(), body_text, &headers));
    }
    Ok(resp)
}

/// 将行流（SSE data payload / NDJSON 行）逐行解析为 [`Delta`] 流。
///
/// 单行 JSON 解析失败产出 `LlmError::Parse`，不中断整个流（与原实现一致：
/// provider 各自的 `flat_map` 语义）。
///
/// `parse` 为 `FnMut` 闭包（R10 P2：Ollama 的 `index` 需跨 NDJSON 行累计，
/// 纯 `fn` 无法携带状态；OpenAI/Anthropic 传 `fn` 指针同样满足 `FnMut`）。
pub(crate) fn lines_to_deltas(
    lines: BoxStream<'static, Result<String, LlmError>>,
    mut parse: impl FnMut(&serde_json::Value) -> Vec<Delta> + Send + 'static,
) -> BoxStream<'static, Result<Delta, LlmError>> {
    Box::pin(lines.flat_map(move |ev| {
        let items: Vec<Result<Delta, LlmError>> = match ev {
            Ok(data) => match serde_json::from_str::<serde_json::Value>(&data) {
                Ok(json) => parse(&json).into_iter().map(Ok).collect(),
                Err(e) => vec![Err(LlmError::Parse(e.to_string()))],
            },
            Err(e) => vec![Err(e)],
        };
        futures::stream::iter(items)
    }))
}
