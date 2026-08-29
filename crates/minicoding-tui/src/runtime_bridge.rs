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
    /// `/tokens`：查询 token/消息计数（R8 FE-16）。
    Tokens,
    /// `/status`：会话状态摘要（模型/工作目录/权限模式，R8 FE-16）。
    Status,
    /// `/model [name]`：无参查看当前模型，带参切换（R8 FE-16）。
    Model(Option<String>),
    /// `/plan`：切换 Plan 模式（R8 FE-16）。
    PlanToggle,
    /// `/undo [steps]`：回滚文件改动（R8 FE-16）。
    Undo { steps: usize },
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

                // command handler：UiCommand → Runtime 操作 → AppEvent 回传
                while let Some(cmd) = ui_rx.recv().await {
                    if !handle_ui_command(&rt, &rt_tx, cmd).await {
                        break; // UI 端关闭
                    }
                }
                events_handle.abort();
                perm_handle.abort();
            });
        })
        .expect("spawn tui-runtime 线程失败")
}

/// 处理单个 `UiCommand`，返回 `false` 表示 UI 端已关闭（应终止循环）。
///
/// 斜杠命令（R8 FE-16）统一在此调 Runtime 查询/操作，结果经
/// `AppEvent::CommandOutput`/`AppEvent::Summary` 等回传渲染为 System 行。
async fn handle_ui_command(
    rt: &std::sync::Arc<Runtime>,
    rt_tx: &mpsc::Sender<AppEvent>,
    cmd: UiCommand,
) -> bool {
    match cmd {
        UiCommand::Submit(text) => {
            let result = rt.run_turn(UserInput::from_text(text)).await;
            let mapped = result.map_err(|e| e.to_string());
            rt_tx.send(AppEvent::TurnResult(mapped)).await.is_ok()
        }
        UiCommand::SwitchSession(id) => {
            // T-M7-2：取消当前 turn（如运行中），通知 main 重建 Runtime。
            // 不在此处切换——Runtime `session` 字段非 interior mutable，
            // 重建是更简洁的路径（避免给 Runtime 加锁 + reset API）。
            rt.cancel_token().cancel();
            rt_tx.send(AppEvent::SwitchSession(id)).await.is_ok()
        }
        UiCommand::Summary => match rt.summarize_session().await {
            Ok(summary) => rt_tx.send(AppEvent::Summary(summary)).await.is_ok(),
            Err(e) => rt_tx
                .send(AppEvent::Summary(Some(format!("摘要生成失败: {e}"))))
                .await
                .is_ok(),
        },
        UiCommand::Tokens => {
            let ctx = rt.context();
            let msg = format!(
                "消息 {} 条 / token {}（压缩触发比例阈值由上下文预算决定）",
                ctx.message_count(),
                ctx.token_count()
            );
            rt_tx.send(AppEvent::CommandOutput(msg)).await.is_ok()
        }
        UiCommand::Status => {
            let workdir = rt.workdir().await;
            let mode = rt.plan_controller().snapshot().await.mode;
            let msg = format!(
                "模型: {} | 工作目录: {} | 权限模式: {:?} | 会话: {}",
                rt.model(),
                workdir,
                mode,
                rt.session().id
            );
            rt_tx.send(AppEvent::CommandOutput(msg)).await.is_ok()
        }
        UiCommand::Model(Some(name)) => {
            let prev = rt.model();
            rt.set_model(&name);
            let msg = format!("模型切换: {prev} → {name}");
            rt_tx.send(AppEvent::CommandOutput(msg)).await.is_ok()
        }
        UiCommand::Model(None) => {
            let msg = format!("当前模型: {}", rt.model());
            rt_tx.send(AppEvent::CommandOutput(msg)).await.is_ok()
        }
        UiCommand::PlanToggle => {
            let controller = rt.plan_controller();
            let mode = controller.snapshot().await.mode;
            let target = if mode == minicoding_core::policy::PermissionMode::Plan {
                minicoding_core::policy::PermissionMode::Default
            } else {
                minicoding_core::policy::PermissionMode::Plan
            };
            controller.set_mode(target).await;
            let msg = format!("权限模式: {mode:?} → {target:?}");
            rt_tx.send(AppEvent::CommandOutput(msg)).await.is_ok()
        }
        UiCommand::Undo { steps } => {
            let steps = steps.max(1);
            let msg = if let Some(journal) = rt.journal() {
                match journal.undo(steps).await {
                    Ok(report) => {
                        let mut msg = format!(
                            "已撤销 {} 条 operation，恢复 {} 个文件",
                            report.undone_entries,
                            report.restored_files.len()
                        );
                        if !report.failed_files.is_empty() {
                            use std::fmt::Write as _;
                            let _ = write!(
                                msg,
                                "；{} 个文件冲突未覆盖（C-28）：{}",
                                report.failed_files.len(),
                                report
                                    .failed_files
                                    .iter()
                                    .map(|(p, e)| format!("{p} ({e})"))
                                    .collect::<Vec<_>>()
                                    .join("; ")
                            );
                        }
                        msg
                    }
                    Err(e) => format!("/undo 失败: {e}"),
                }
            } else {
                "journal 未启用（file-undo feature 关闭或未注入）".to_string()
            };
            rt_tx.send(AppEvent::CommandOutput(msg)).await.is_ok()
        }
        UiCommand::Exit => false,
    }
}
