//! 事件总线（仅通知无回复，见 `design.md` §11）。
//!
//! `EventBus` 基于 `tokio::sync::broadcast`：发布者克隆事件，订阅者各自消费。
//! 权限交互不走总线（点对点 `PermissionPrompter`，见 `policy::trait`）。

use crate::model::{Message, SessionId, StopReason, Task, ToolCallId, ToolResult};
use crate::policy::{Decision, PermissionMode, Risk};
use tokio::sync::broadcast;

/// 运行时事件（向前端广播，仅通知无回复通道）。
#[derive(Debug, Clone)]
pub enum Event {
    /// 流式 token 增量。
    Token(String),
    /// 思考过程增量（reasoning/thinking，见 `provider::Delta::Reasoning`）。
    ///
    /// 瞬态事件：仅作流式展示，不落盘、不进 `messages`（与正文分离，避免污染
    /// 上下文与审计）。模型不支持 reasoning 时不发出。
    ReasoningDelta(String),
    /// 一条消息已追加（落盘 + 入上下文后）。
    MessageAppended(Message),
    /// 流式开始。
    TurnStreamingStarted,
    /// 一轮结束。
    TurnEnd { stop_reason: StopReason },
    /// 工具调用开始。
    ToolCallStarted { call_id: ToolCallId, tool: String },
    /// 工具调用完成。
    ToolCallFinished {
        call_id: ToolCallId,
        result: ToolResult,
    },
    /// 会话已创建。
    SessionCreated { id: SessionId },
    /// 权限已询问（通知类，仅展示/审计，无回复通道，见 `design.md` §9.2）。
    PermissionRequested {
        id: String,
        tool: String,
        summary: String,
        risk: Risk,
    },
    /// 权限已 resolved（带最终决策，供 UI 关闭弹窗与审计，见 `design.md` §9.2）。
    PermissionResolved { id: String, decision: Decision },
    /// 权限模式切换（`plan.exit` / `/plan` / `--plan` 触发，见 `design.md` §16.2）。
    PermissionModeChanged {
        from: PermissionMode,
        to: PermissionMode,
    },
    /// 任务更新（`task.create`/`task.update` 后广播，供 UI 渲染任务面板，见 `design.md` §18.4）。
    ///
    /// 携带更新后的 `Task` 快照；UI 据此刷新任务列表（T-M7-4）。
    TaskUpdated { task: Task },
    /// 配置文件变更（S-22 热更新，`ConfigWatcher` 检测到 `config.toml` 变化时广播）。
    ///
    /// 仅通知无回复通道；需要响应变化的组件（扩展 `on_config_changed`、TUI 重渲染等）
    /// 自行订阅 `EventBus` 并处理。`ConfigWatcher` 已做 500ms debounce，此处收到即代表
    /// 配置文件确有变更。
    ConfigChanged,
    /// step 开始（M-06）：一次 LLM 请求 + 其触发的工具调用（第 N 次迭代）。
    ///
    /// 在工具调用执行前广播（携带将执行的 `tool_call_ids`），用于前端展示 step
    /// 进度；与 `PersistedEvent::StepStarted` 一一映射（落盘定位压缩点/中断点，
    /// 见 `design.md` §25）。log-only，不进 transcript（C-05）。
    StepStarted {
        iter: u32,
        tool_call_ids: Vec<String>,
    },
    /// step 结束（M-06）：该次迭代的工具结果已全部回灌（含中断时合成的结果）。
    ///
    /// 与 `PersistedEvent::StepEnded` 一一映射。cancel/timeout 中断时可能只出现
    /// `StepStarted` 而无 `StepEnded`——正是中断点定位依据。
    StepEnded { iter: u32 },
}

/// 事件总线（broadcast channel）。
///
/// 容量 256：token 事件高频，过小会丢消息；过大浪费内存。
/// 订阅者消费慢时丢弃最旧事件（对 token 流可接受）。
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    /// 创建事件总线。
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(256)
    }

    /// 指定容量创建。
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(16));
        Self { tx }
    }

    /// 发布事件（无订阅者时静默丢弃）。
    #[tracing::instrument(skip(self), fields(otel.name = crate::otel::span_name::EVENT_PUBLISH))]
    pub fn emit(&self, event: Event) {
        // send 失败仅因无订阅者，不视为错误
        let _ = self.tx.send(event);
    }

    /// 订阅事件流。
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("subscribers", &self.tx.receiver_count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    //! `EventBus` broadcast 通道测试：构造、订阅、emit、Debug。

    use super::*;
    use crate::model::Task;

    #[test]
    fn default_equals_new() {
        let a = EventBus::default();
        let b = EventBus::new();
        // 两者均无订阅者
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
    }

    #[test]
    fn with_capacity_clamps_to_minimum() {
        // 容量 < 16 时应被钳制到 16（避免 0 容量导致 channel 无法创建）。
        let bus = EventBus::with_capacity(1);
        let _rx = bus.subscribe();
        // 能成功订阅即说明 channel 创建成功
    }

    #[test]
    fn debug_shows_subscriber_count() {
        let bus = EventBus::new();
        let debug_str = format!("{bus:?}");
        assert!(debug_str.contains("EventBus"));
        assert!(debug_str.contains("subscribers"));
        assert!(debug_str.contains('0'));

        let _rx = bus.subscribe();
        let debug_str = format!("{bus:?}");
        assert!(debug_str.contains('1'));
    }

    #[tokio::test]
    async fn emit_without_subscribers_is_silent() {
        // 无订阅者时 emit 不应 panic 或报错。
        let bus = EventBus::new();
        bus.emit(Event::Token("orphan".to_string()));
        bus.emit(Event::ConfigChanged);
    }

    #[tokio::test]
    async fn subscribe_receives_emitted_events() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.emit(Event::Token("hello".to_string()));
        let event = rx.recv().await.expect("应收到事件");
        match event {
            Event::Token(t) => assert_eq!(t, "hello"),
            other => panic!("expected Token, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn multiple_subscribers_each_receive_events() {
        // broadcast：每个订阅者各自收到一份。
        let bus = EventBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();
        bus.emit(Event::ConfigChanged);
        let e1 = rx1.recv().await.expect("rx1 应收到");
        let e2 = rx2.recv().await.expect("rx2 应收到");
        assert!(matches!(e1, Event::ConfigChanged));
        assert!(matches!(e2, Event::ConfigChanged));
    }

    #[tokio::test]
    async fn emit_various_event_types() {
        // 验证所有 Event 变体均可通过 EventBus 传播。
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        let task = Task::new("test task".to_string());
        bus.emit(Event::Token("t".to_string()));
        bus.emit(Event::MessageAppended(crate::model::Message::user_text(
            "m",
        )));
        bus.emit(Event::TurnStreamingStarted);
        bus.emit(Event::TurnEnd {
            stop_reason: StopReason::EndTurn,
        });
        bus.emit(Event::ToolCallStarted {
            call_id: "call-1".to_string(),
            tool: "fs.read".to_string(),
        });
        bus.emit(Event::ToolCallFinished {
            call_id: "call-1".to_string(),
            result: crate::model::ToolResult::ok_text("ok"),
        });
        bus.emit(Event::SessionCreated {
            id: "sess-1".to_string(),
        });
        bus.emit(Event::PermissionRequested {
            id: "perm-1".to_string(),
            tool: "fs.write".to_string(),
            summary: "write file".to_string(),
            risk: Risk::Low,
        });
        bus.emit(Event::PermissionResolved {
            id: "perm-1".to_string(),
            decision: Decision::Allow,
        });
        bus.emit(Event::PermissionModeChanged {
            from: PermissionMode::Default,
            to: PermissionMode::Plan,
        });
        bus.emit(Event::TaskUpdated { task });
        bus.emit(Event::ConfigChanged);

        // 消费 12 个事件，全部应成功接收
        for _ in 0..12 {
            rx.recv().await.expect("每个事件都应被订阅者收到");
        }
    }

    #[tokio::test]
    async fn late_subscriber_misses_earlier_events() {
        // broadcast 不保留历史：后订阅者只收到订阅后的事件。
        let bus = EventBus::new();
        bus.emit(Event::Token("before".to_string()));
        let mut rx = bus.subscribe();
        bus.emit(Event::Token("after".to_string()));
        let event = rx.recv().await.expect("应收到订阅后的事件");
        match event {
            Event::Token(t) => assert_eq!(t, "after"),
            other => panic!("expected Token('after'), got {other:?}"),
        }
    }
}
