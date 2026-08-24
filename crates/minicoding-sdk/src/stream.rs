//! `DeltaStream`：把 `EventBus` 的 `Event::Token` 转为 `Delta` 流。
//!
//! `ask_stream` 内部用此类型。`run_turn` 的 future 存储在流中，由 `poll_next`
//! 交替 poll `run_turn` future 与 `broadcast::Receiver`。流 drop 时，`run_turn`
//! future 也 drop（触发取消信号，已落盘消息保留，C-13）。
//!
//! 设计权衡：不使用 `tokio::spawn` 启动后台 task——`run_turn` 返回的 future 借用
//! `&Runtime`（非 `'static`，因 `run_turn` 是 `async fn(&self, ...)`），无法直接
//! spawn。改为流内驱动可避免 `'static` 约束，调用方 poll 流即可让 turn 前进。
//!
//! 注意：`TurnFut` 不带 `Send` 约束——`run_turn` future 内部包含捕获 `&self` 的
//! async 块，加 `Send` 约束会触发 HRTB 推断错误（`FnOnce` not general enough）。
//! `DeltaStream` 因此是 `!Send`，只能在创建它的线程上 poll。

use crate::SdkError;
use futures::Stream;
use minicoding_core::model::{RuntimeError, TurnOutcome, UserInput};
use minicoding_core::provider::Delta;
use minicoding_core::runtime::{Event, Runtime};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio_stream::wrappers::BroadcastStream;

/// `run_turn` future 的 boxed 类型（借用 `&Runtime`，非 `'static`，非 `Send`）。
type TurnFut<'r> = Pin<Box<dyn Future<Output = Result<TurnOutcome, RuntimeError>> + 'r>>;

/// `Delta` 流（由 `Client::ask_stream` 返回）。
///
/// 内部把 `Event::Token` 转为 `Delta::Text`；`run_turn` 完成后根据 `TurnOutcome`
/// 决定终止或返回错误。
///
/// 流提前 drop 时，`run_turn` future 被 drop（触发 runtime 的 `cancel_token`，
/// 已落盘消息保留，C-13）。
///
/// 调用方必须 poll 流才能让 `run_turn` 前进——这与 `BroadcastStream` 的语义一致。
pub struct DeltaStream<'c> {
    /// broadcast 事件流（包装 `broadcast::Receiver` 为 `Stream`）。
    inner: BroadcastStream<Event>,
    /// `run_turn` future（借用 `&Runtime`，由 `runtime` 参数的生命周期 `'c` 约束）。
    turn_fut: Option<TurnFut<'c>>,
    /// `run_turn` 是否已完成（完成后只处理剩余 broadcast 事件）。
    turn_done: bool,
    /// `run_turn` 的最终结果（完成后暂存，下一次 poll 返回错误或终止）。
    final_result: Option<Result<TurnOutcome, RuntimeError>>,
    /// 是否已通过 `final_result` 返回过终止信号（避免重复返回）。
    emitted_final: bool,
}

impl<'c> DeltaStream<'c> {
    /// 构造 `DeltaStream`：把 `run_turn` future 与 `broadcast::Receiver` 一起存储。
    ///
    /// `runtime` 以 `&'c Runtime` 引用传入，`run_turn` future 借用此引用。
    /// 调用方必须保证 `runtime` 在流存活期内不被 drop（由 `'c` 生命周期约束）。
    #[must_use]
    pub fn new(
        runtime: &'c Runtime,
        user_input: UserInput,
        rx: tokio::sync::broadcast::Receiver<Event>,
    ) -> Self {
        // 显式标注 future 生命周期为 `'c`，避免编译器推断为更短的生命周期。
        let turn_fut: TurnFut<'c> = Box::pin(runtime.run_turn(user_input));
        Self {
            inner: BroadcastStream::new(rx),
            turn_fut: Some(turn_fut),
            turn_done: false,
            final_result: None,
            emitted_final: false,
        }
    }
}

impl Stream for DeltaStream<'_> {
    type Item = Result<Delta, SdkError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        use futures::StreamExt;
        use std::future::Future;

        // 1. 如果 `run_turn` 还未完成，先 poll 它（不阻塞事件流）。
        if !self.turn_done
            && let Some(mut turn_fut) = self.turn_fut.take()
        {
            match Pin::new(&mut turn_fut).poll(cx) {
                Poll::Ready(result) => {
                    self.turn_done = true;
                    self.final_result = Some(result);
                }
                Poll::Pending => {
                    self.turn_fut = Some(turn_fut);
                    // 继续处理 broadcast 事件（fall through）。
                }
            }
        }

        // 2. 消费 BroadcastStream 事件。
        match self.inner.poll_next_unpin(cx) {
            Poll::Ready(Some(Ok(event))) => match event {
                Event::Token(text) => Poll::Ready(Some(Ok(Delta::Text(text)))),
                // ReasoningDelta 同样透传（2026-08-25 审查）：此前落入 `_` 忽略
                // 分支，SDK 消费方永远收不到思考过程增量。
                Event::ReasoningDelta(text) => Poll::Ready(Some(Ok(Delta::Reasoning(text)))),
                Event::TurnEnd { .. } => {
                    // TurnEnd 事件：根据 final_result 决定终止或返回错误。
                    if self.emitted_final {
                        return Poll::Ready(None);
                    }
                    self.emitted_final = true;
                    if let Some(result) = self.final_result.take() {
                        Self::map_final_result(result)
                    } else {
                        // TurnEnd 收到但 run_turn future 还未返回（理论上不应发生，
                        // 因为 Runtime 在 TurnEnd 之后才返回；保守终止流）。
                        Poll::Ready(None)
                    }
                }
                _ => {
                    // 其它事件忽略，继续 poll 下一个。
                    self.as_mut().poll_next(cx)
                }
            },
            Poll::Ready(Some(Err(_))) => {
                // Lagged：消费慢导致丢事件，继续 poll 下一个。
                self.as_mut().poll_next(cx)
            }
            Poll::Ready(None) => {
                // broadcast 关闭（Runtime drop）。
                Poll::Ready(None)
            }
            Poll::Pending => {
                // broadcast 无事件；如果 run_turn 也已完成且无 TurnEnd 事件，
                // 直接返回 final_result（避免调用方永久阻塞）。
                if self.turn_done
                    && !self.emitted_final
                    && let Some(result) = self.final_result.take()
                {
                    self.emitted_final = true;
                    return Self::map_final_result(result);
                }
                Poll::Pending
            }
        }
    }
}

impl DeltaStream<'_> {
    /// 把 `run_turn` 的最终结果映射为 `Poll<Option<Result<Delta, SdkError>>>`。
    fn map_final_result(
        result: Result<TurnOutcome, RuntimeError>,
    ) -> Poll<Option<Result<Delta, SdkError>>> {
        match result {
            Ok(TurnOutcome::Finished(_)) => Poll::Ready(None),
            Ok(TurnOutcome::Interrupted(_)) => Poll::Ready(Some(Err(SdkError::Interrupted))),
            Ok(TurnOutcome::Failed(e)) => Poll::Ready(Some(Err(SdkError::TurnFailed(e)))),
            Err(e) => Poll::Ready(Some(Err(SdkError::Runtime(e)))),
        }
    }
}
