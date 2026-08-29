//! 与 `Runtime` 的 channel 桥接（T-M7-1）。
//!
//! 在专用线程上启动 `current_thread` tokio runtime + `LocalSet`，跑三个并发 task：
//! - **event forwarder**（`spawn_local`）：订阅 `EventBus`，把 `Event` 转发为
//!   [`AppEvent::Runtime`]；
//! - **permission forwarder**（`spawn_local`，T-M7-3）：消费 `TuiPrompter` 发来的
//!   [`TuiPermissionRequest`]，转发为 [`AppEvent::PermissionRequest`]；
//! - **command handler**（主 `block_on` future）：消费 [`UiCommand`]，调用
//!   `Runtime::run_turn`，完成后发 [`AppEvent::TurnResult`]。
//!
//! ## 为何用 `current_thread` + `LocalSet` 而非 `multi_thread` + `spawn`
//!
//! `Runtime::run_turn` 返回的 future 当前不是 `Send`（内部 tool 执行闭包捕获了非
//! `Send` 的引用）。`tokio::spawn` 要求 `Send`，会编译失败。`LocalSet` 上的
//! `spawn_local` 与 `block_on` 不要求 `Send`，可在单线程上驱动非 `Send` future。
//!
//! `run_turn` 期间 command handler 在 `await`，新 `UiCommand` 缓冲在 mpsc channel
//! 中，turn 完成后才处理——等价于"一轮未结束不接受下一条输入"。EventBus 事件与
//! 权限询问由独立 `spawn_local` task 持续转发（LocalSet 单线程交替调度，
//! `run_turn` await 时调度器 poll forwarder，token 流/权限询问不丢失）。
//!
//! 中断（Ctrl-C）由 UI 主循环检测后调用 `Runtime::cancel_token()` 触发，
//! `run_turn` 返回 `TurnOutcome::Interrupted`。

use std::sync::Arc;

use minicoding_core::model::UserInput;
use minicoding_core::policy::TuiPermissionRequest;
use minicoding_core::runtime::Runtime;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::task::LocalSet;

use crate::event::AppEvent;

/// UI → Runtime 的命令。
#[derive(Debug)]
pub enum UiCommand {
    /// 提交用户输入（启动一轮 `run_turn`）。
    Submit(String),
    /// 切换到指定会话（T-M7-2）：bridge 取消当前 turn 后回 `AppEvent::SwitchSession`，
    /// 由 main.rs 重建 Runtime（`SessionLoadMode::Resume`）实现真正的切换。
    SwitchSession(String),
    /// 生成会话摘要（R8）：bridge 调 `Runtime::summarize_session`，回传
    /// `AppEvent::Summary`。
    Summary,
    /// 退出 TUI（终止后台 task）。
    Exit,
}

/// 启动 Runtime 桥接后台线程。
///
/// 在专用线程上创建 `current_thread` runtime + `LocalSet`，驱动三个 task：
/// event forwarder、permission forwarder（T-M7-3）、command handler。线程随
/// `UiCommand::Exit` 或 channel 关闭退出。
///
/// `perm_rx` 为 `TuiPrompter` 持有的 mpsc channel 的 receiver 端；`TuiPrompter`
/// 在 `run_turn` 期间通过它发送权限询问，permission forwarder 转发给 UI。
///
/// # Panics
/// 底层 `std::thread::Builder::spawn` 失败时 panic（资源耗尽，不可恢复）。
/// 返回的 `JoinHandle` 通常不显式 join（随进程退出而终止）。
pub fn spawn_runtime_bridge(
    rt: Runtime,
    mut ui_rx: mpsc::Receiver<UiCommand>,
    mut perm_rx: mpsc::Receiver<TuiPermissionRequest>,
    rt_tx: mpsc::Sender<AppEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("tui-runtime".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("创建 tokio runtime 失败: {e}");
                    return;
                }
            };
            let local = LocalSet::new();
            local.block_on(&runtime, async move {
                let rt = Arc::new(rt);

                // event forwarder：EventBus → AppEvent::Runtime
                let rt_for_events = Arc::clone(&rt);
                let rt_tx_events = rt_tx.clone();
                let mut event_rx = rt_for_events.events().subscribe();
                let events_handle = tokio::task::spawn_local(async move {
                    loop {
                        match event_rx.recv().await {
                            Ok(ev) => {
                                if rt_tx_events.send(AppEvent::Runtime(ev)).await.is_err() {
                                    break; // UI 端关闭
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                // token 流高频，落后时丢弃旧事件（对 UI 可接受）
                                tracing::warn!("TUI 事件总线落后 {n} 条事件（已丢弃旧事件）");
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                });

                // permission forwarder（T-M7-3）：TuiPrompter 的 mpsc → AppEvent::PermissionRequest
                let rt_tx_perm = rt_tx.clone();
                let perm_handle = tokio::task::spawn_local(async move {
                    while let Some(req) = perm_rx.recv().await {
                        if rt_tx_perm
                            .send(AppEvent::PermissionRequest(req))
                            .await
                            .is_err()
                        {
                            break; // UI 端关闭
                        }
                    }
                });

                // resume/fork 模式：回填历史消息到 ContextManager（T-M7-2 会话切换）
                // build_runtime 加载了 session.messages 但未注入 ctx，首个 run_turn 前必须回填。
                if let Err(e) = rt.restore_history().await {
                    tracing::error!("恢复会话历史失败: {e}");
                    let _ = rt_tx
                        .send(AppEvent::TurnResult(Err(format!("恢复历史失败: {e}"))))
                        .await;
                }

                // command handler：UiCommand → run_turn → AppEvent::TurnResult
                while let Some(cmd) = ui_rx.recv().await {
                    match cmd {
                        UiCommand::Submit(text) => {
                            let result = rt.run_turn(UserInput::from_text(text)).await;
                            let mapped = result.map_err(|e| e.to_string());
                            if rt_tx.send(AppEvent::TurnResult(mapped)).await.is_err() {
                                break; // UI 端关闭
                            }
                        }
                        UiCommand::SwitchSession(id) => {
                            // T-M7-2：取消当前 turn（如运行中），通知 main 重建 Runtime。
                            // 不在此处切换——Runtime `session` 字段非 interior mutable，
                            // 重建是更简洁的路径（避免给 Runtime 加锁 + reset API）。
                            rt.cancel_token().cancel();
                            if rt_tx.send(AppEvent::SwitchSession(id)).await.is_err() {
                                break; // UI 端关闭
                            }
                        }
                        UiCommand::Summary => {
                            let result = rt.summarize_session().await;
                            match result {
                                Ok(summary) => {
                                    if rt_tx.send(AppEvent::Summary(summary)).await.is_err() {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    if rt_tx
                                        .send(AppEvent::Summary(Some(format!("摘要生成失败: {e}"))))
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                            }
                        }
                        UiCommand::Exit => break,
                    }
                }
                events_handle.abort();
                perm_handle.abort();
            });
        })
        .expect("spawn tui-runtime 线程失败")
}
