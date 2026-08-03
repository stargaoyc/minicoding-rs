//! S-22：配置热更新（见 `design.md` §16、`features.md` S-22）。
//!
//! `ConfigWatcher` 监听 `~/.minicoding/config.toml` 变更，检测到变化时通过
//! `EventBus` 广播 `Event::ConfigChanged`。`Runtime` 在启动时调用 `ConfigWatcher::start`
//! 注册监听器；`ConfigWatcher` 随 `Runtime` 存活，drop 时连带停止监听并结束后台线程。
//!
//! 设计要点：
//! - **debounce**：文件变化事件可能连续触发（编辑器原子保存、多次写），用 500ms
//!   静默期聚合——静默期内的新事件重置计时器，仅在静默 500ms 后广播一次。
//! - **best-effort**：监听失败（路径无父目录、watcher 构造或注册失败）不返回错误，
//!   仅记 warn 日志，降级为无热更新（不阻塞 `Runtime` 启动）。
//! - **`EventBus`**：仅广播通知，不阻塞；需要响应变化的组件自行订阅并处理。
//! - **路径过滤**：监听父目录（监听文件本身在某些平台不稳定），按文件名过滤
//!   `config.toml`，避免同目录其他文件（`audit.log`/`sessions`/`memory`）写入误触发。
//! - **不依赖 tokio runtime**：debounce 循环跑在 std 线程上（见 `start` 注释），
//!   `EventBus::emit` 即 `broadcast::Sender::send`，是同步方法不需 runtime。

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use camino::Utf8Path;
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::runtime::{Event, EventBus};

/// debounce 静默期：最后一个事件后等待此时长无新事件才广播（聚合编辑器多次保存）。
const DEBOUNCE: Duration = Duration::from_millis(500);

/// 配置文件变更监听器（S-22）。
///
/// 监听 `config_path` 指向的配置文件所在目录，当 `config.toml` 发生修改/创建时，
/// 经 500ms debounce 后通过 `EventBus` 广播 `Event::ConfigChanged`。监听在独立 std
/// 线程中运行；`ConfigWatcher` drop 时连带停止监听并结束 debounce 线程。
///
/// 监听失败（路径无父目录、watcher 构造或注册失败）不返回错误，仅记 warn 日志
/// （best-effort，不阻塞 Runtime 启动），此时 `ConfigWatcher` 为空壳（无监听）。
pub struct ConfigWatcher {
    /// notify watcher 句柄；`None` 表示监听未启动（best-effort 降级）。
    /// drop 即停止监听：watcher 内部回调持有的 `mpsc::Sender` 随之 drop，
    /// debounce 线程收到 `Disconnected` 退出。
    _watcher: Option<RecommendedWatcher>,
}

impl ConfigWatcher {
    /// 启动配置文件监听。
    ///
    /// `config_path`：监听的配置文件路径（通常是 `~/.minicoding/config.toml`）。
    /// `event_bus`：事件总线，变更时广播 `Event::ConfigChanged`。
    ///
    /// 监听失败（路径无父目录、watcher 构造或注册失败）不返回错误，仅记 warn 日志
    /// （best-effort，不阻塞 Runtime 启动），返回空壳 `ConfigWatcher`。
    #[must_use]
    pub fn start(config_path: &Utf8Path, event_bus: EventBus) -> Self {
        // 监听父目录（监听文件本身在某些平台不稳定，见 notify `Watcher::watch` 文档）。
        // 父目录为空串（相对路径无父段）时无法监听，降级。
        let Some(parent) = config_path.parent().filter(|p| !p.as_str().is_empty()) else {
            tracing::warn!(path = %config_path, "ConfigWatcher: 无父目录，跳过监听");
            return Self { _watcher: None };
        };

        // std mpsc：notify 回调（生产者）→ debounce 线程（消费者）。无界 channel，
        // `send` 不阻塞 notify 内部线程；编辑器瞬时多事件足够缓冲，溢出仅丢事件（debounce 容错）。
        let (tx, rx) = mpsc::channel::<()>();

        // 仅转发 config.toml 自身的变化：监听父目录会收到同目录所有文件事件
        // （audit.log/sessions/memory 写入），需按文件名过滤避免误报。
        let config_file_name = config_path.file_name().map(str::to_owned);
        let watcher = RecommendedWatcher::new(
            move |res: Result<notify::Event, notify::Error>| {
                let Ok(event) = res else { return };
                if !matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                    return;
                }
                let matches_config = event
                    .paths
                    .iter()
                    .any(|p| p.file_name().and_then(|n| n.to_str()) == config_file_name.as_deref());
                if matches_config {
                    // 接收端 drop 时 send 失败，忽略（debounce 线程已退出）
                    let _ = tx.send(());
                }
            },
            Config::default(),
        );

        let mut watcher = match watcher {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(error = %e, "ConfigWatcher: 构造 watcher 失败");
                return Self { _watcher: None };
            }
        };

        if let Err(e) = watcher.watch(parent.as_std_path(), RecursiveMode::NonRecursive) {
            tracing::warn!(path = %config_path, error = %e, "ConfigWatcher: 注册监听失败");
            return Self { _watcher: None };
        }

        // §2.4 例外说明：此处用 std::thread 而非 tokio::spawn，是因为 `Runtime` 在
        // tokio runtime 进入前构造（CLI 先 `build_runtime` 再创建 tokio runtime），
        // 此时 `tokio::spawn` 会 panic（"no reactor running"）。debounce 循环仅调用
        // 同步的 `EventBus::emit`（`broadcast::Sender::send` 不需 runtime），与异步
        // 运行时解耦，故 std 线程是恰当选择。
        // 生命周期：watcher drop → 闭包 drop → `tx` drop → `recv` 返回 `Disconnected` → 线程退出。
        thread::spawn(move || {
            loop {
                // 等待首个事件；channel 关闭（watcher drop）则退出
                if rx.recv().is_err() {
                    return;
                }
                // debounce：静默期内的新事件重置计时，静默 500ms 后广播一次
                loop {
                    match rx.recv_timeout(DEBOUNCE) {
                        // 收到新事件：loop 重新调用 recv_timeout，等价于重置静默计时
                        Ok(()) => {}
                        Err(mpsc::RecvTimeoutError::Timeout) => break, // 500ms 静默 → 广播
                        Err(mpsc::RecvTimeoutError::Disconnected) => return, // channel 关闭
                    }
                }
                tracing::info!("配置文件变更，广播 ConfigChanged");
                event_bus.emit(Event::ConfigChanged);
            }
        });

        tracing::info!(path = %config_path, "ConfigWatcher 已启动");
        Self {
            _watcher: Some(watcher),
        }
    }
}

#[cfg(test)]
mod tests {
    //! `ConfigWatcher` best-effort 降级路径测试（覆盖率补全）。
    //!
    //! 仅覆盖可稳定测试的降级场景（无父目录、监听注册失败）；
    //! 正常监听路径涉及 notify 内部线程与 OS 文件事件，由集成测试覆盖。

    use super::*;
    use crate::runtime::{Event, EventBus};

    #[test]
    fn start_with_no_parent_dir_degrades_to_empty_watcher() {
        // 文件名无父目录（相对路径仅文件名），应降级为空 watcher
        let path = camino::Utf8Path::new("config.toml");
        let watcher = ConfigWatcher::start(path, EventBus::new());
        // `_watcher` 为 private，无法直接断言 `None`；但降级路径不创建线程，
        // 不订阅事件即可验证不会 panic / 不挂起。
        drop(watcher);
    }

    #[test]
    fn start_with_nonexistent_parent_degrades_gracefully() {
        // 父目录不存在：`watcher.watch` 注册失败，应降级为空 watcher 不 panic
        let path = camino::Utf8Path::new("/this/path/does/not/exist/config.toml");
        let watcher = ConfigWatcher::start(path, EventBus::new());
        drop(watcher);
    }

    #[tokio::test]
    async fn start_with_existing_dir_does_not_panic() {
        // 临时目录存在，监听应成功注册（不验证实际事件触发，由集成测试覆盖）
        let tmp = tempfile::tempdir().expect("tempdir");
        let config_file = tmp.path().join("config.toml");
        let config_path = camino::Utf8Path::from_path(&config_file).expect("utf8 path");
        let watcher = ConfigWatcher::start(config_path, EventBus::new());
        drop(watcher);
        // 给后台线程一点时间稳定（即使无事件也不应崩溃）
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[test]
    fn event_bus_emit_config_changed_does_not_panic_without_subscribers() {
        // 无订阅者时 emit 应静默丢弃，不 panic
        let bus = EventBus::new();
        bus.emit(Event::ConfigChanged);
    }

    #[test]
    fn event_bus_subscribe_can_receive_config_changed() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.emit(Event::ConfigChanged);
        let event = rx.try_recv().expect("should receive event");
        assert!(matches!(event, Event::ConfigChanged));
    }
}
