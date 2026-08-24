//! `asyncRewake` 异步唤醒管理器（见 `hooks.md` §11）。
//!
//! 当 Hook 同步返回 `async_rewake = Some(spec)` 时，主流程不阻塞；后台任务在
//! `estimated_duration × 2` 超时内执行，完成后将结果注入下一轮 prompt。
//!
//! # 约束
//!
//! - **C-26**：后台任务遵守凭证隔离（C-04）、沙箱（C-22）、路径沙箱（C-03）
//! - **C-32**：同一 manager 最多 `max_concurrent`（默认 3）个并发 `async_rewake`
//! - 超时（`estimated_duration_sec × 2`）后自动取消，注入超时提示
//! - 结果走 `inject_context`，Runtime 包裹 `<async_rewake>` 边界（C-05）

use minicoding_core::hooks::AsyncRewakeSpec;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Mutex as TokioMutex;

/// 默认并发上限（C-32，见 `hooks.md` §11.3 "同一 session 最多 3 个并发"）。
pub const DEFAULT_MAX_CONCURRENT: usize = 3;

/// asyncRewake 结果状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewakeStatus {
    /// 后台任务成功完成，`context` 含注入内容。
    Success,
    /// 后台任务超时（`estimated_duration × 2`）。
    Timeout,
    /// 后台任务出错（非超时）。
    Error,
}

/// 单个 asyncRewake 的完成结果。
#[derive(Debug, Clone)]
pub struct RewakeResult {
    /// 关联的 Hook 名（审计用）。
    pub hook_name: String,
    /// 来自 `AsyncRewakeSpec::description`，展示用。
    pub description: String,
    /// 完成状态。
    pub status: RewakeStatus,
    /// 注入下一轮 prompt 的上下文（`Success` 时有值；`Timeout`/`Error` 时为错误提示）。
    pub context: String,
    /// 任务 ID（由 `spawn` 返回，供审计关联）。
    pub task_id: u64,
}

impl RewakeResult {
    fn success(hook_name: String, description: String, context: String, task_id: u64) -> Self {
        Self {
            hook_name,
            description,
            status: RewakeStatus::Success,
            context,
            task_id,
        }
    }

    fn timeout(hook_name: String, description: String, task_id: u64) -> Self {
        let context = format!("asyncRewake `{description}` 超时（estimated_duration × 2）");
        Self {
            hook_name,
            description,
            status: RewakeStatus::Timeout,
            context,
            task_id,
        }
    }

    fn error(hook_name: String, description: String, error: &str, task_id: u64) -> Self {
        let context = format!("asyncRewake `{description}` 失败: {error}");
        Self {
            hook_name,
            description,
            status: RewakeStatus::Error,
            context,
            task_id,
        }
    }
}

/// `asyncRewake` 管理器（见 `hooks.md` §11）。
///
/// 管理后台任务的并发上限（C-32）、超时（`estimated_duration × 2`）、结果收集。
/// Runtime 在 turn 边界调用 `poll` drain 已完成结果，注入下一轮 prompt。
///
/// # 线程安全
///
/// `completed` 用 `TokioMutex`（异步上下文中持有锁）；`inflight` 用 `AtomicUsize`。
/// `Arc<AsyncRewakeManager>` 可安全跨 `tokio::spawn` 任务共享。
pub struct AsyncRewakeManager {
    /// 已完成待消费的结果队列。
    completed: Arc<TokioMutex<Vec<RewakeResult>>>,
    /// 在飞行中的任务数（C-32 并发上限检查）。
    inflight: Arc<AtomicUsize>,
    /// 并发上限（默认 3，C-32）。
    max_concurrent: usize,
    /// 下一个 `task_id`（单调递增，供审计关联）。
    next_task_id: Arc<AtomicUsize>,
    /// 活跃任务名映射（`task_id` → `hook_name`），供诊断/`doctor`。
    active: Arc<Mutex<HashMap<u64, String>>>,
}

impl AsyncRewakeManager {
    /// 创建带指定并发上限的 manager。
    #[must_use]
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            completed: Arc::new(TokioMutex::new(Vec::new())),
            inflight: Arc::new(AtomicUsize::new(0)),
            max_concurrent: max_concurrent.max(1),
            next_task_id: Arc::new(AtomicUsize::new(1)),
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 创建默认并发上限（3，C-32）的 manager。
    #[must_use]
    pub fn default_concurrent() -> Self {
        Self::new(DEFAULT_MAX_CONCURRENT)
    }

    /// 当前在飞行中的任务数。
    #[must_use]
    pub fn inflight_count(&self) -> usize {
        self.inflight.load(Ordering::Acquire)
    }

    /// 并发上限。
    #[must_use]
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// 尝试启动一个后台 asyncRewake 任务。
    ///
    /// # Arguments
    /// * `hook_name` - 关联的 Hook 名（审计用）。
    /// * `spec` - `AsyncRewakeSpec`（含预估时长与描述）。
    /// * `future` - 后台任务主体，返回注入上下文（`Ok`）或错误消息（`Err`）。
    ///
    /// # Returns
    /// - `Some(task_id)` — 启动成功，返回任务 ID 供审计关联。
    /// - `None` — 并发上限已达（C-32 拒绝），调用方应记 warn。
    ///
    /// # 超时
    ///
    /// 后台任务超时为 `estimated_duration_sec × 2`（见 `hooks.md` §11.3）。
    /// 超时后 future 被 drop（取消），结果记 `RewakeStatus::Timeout`。
    ///
    /// # 约束
    ///
    /// - **C-26**：`future` 内部必须遵守凭证隔离（C-04）、沙箱（C-22）、路径沙箱（C-03）。
    ///   调用方（如 `ScriptHook` adapter）负责在 future 内部 `env_clear()` + 白名单。
    /// - **C-32**：并发上限由 `max_concurrent` 强制。
    ///
    /// # Panics
    ///
    /// 内部 `Mutex` 被 poison 时 panic（仅在持有锁的线程 panic 时发生，本 crate 不在此处 panic）。
    pub fn spawn<F>(&self, hook_name: &str, spec: &AsyncRewakeSpec, future: F) -> Option<u64>
    where
        F: Future<Output = Result<String, String>> + Send + 'static,
    {
        // C-32：并发上限检查（CAS 循环防 TOCTOU）。
        loop {
            let current = self.inflight.load(Ordering::Acquire);
            if current >= self.max_concurrent {
                tracing::warn!(
                    hook = %hook_name,
                    inflight = current,
                    max = self.max_concurrent,
                    "asyncRewake rejected: C-32 concurrent limit reached"
                );
                return None;
            }
            if self
                .inflight
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }

        let task_id = self.next_task_id.fetch_add(1, Ordering::Relaxed) as u64;
        {
            let mut active = self.active.lock().expect("active map mutex poisoned");
            active.insert(task_id, hook_name.to_string());
        }

        let completed = self.completed.clone();
        let inflight = self.inflight.clone();
        let active = self.active.clone();
        let hook_name_clone = hook_name.to_string();
        let description = spec.description.clone();

        // 超时 = estimated_duration × 2（见 hooks.md §11.3）。
        let timeout = Duration::from_secs(u64::from(spec.estimated_duration_sec) * 2);

        tokio::spawn(async move {
            let result = match tokio::time::timeout(timeout, future).await {
                Ok(Ok(ctx)) => RewakeResult::success(hook_name_clone, description, ctx, task_id),
                Ok(Err(e)) => RewakeResult::error(hook_name_clone, description, &e, task_id),
                Err(_) => RewakeResult::timeout(hook_name_clone, description, task_id),
            };

            // 推入完成队列。
            {
                let mut guard = completed.lock().await;
                guard.push(result);
            }
            // 递减飞行计数 + 移除活跃映射。
            inflight.fetch_sub(1, Ordering::Release);
            {
                let mut active = active.lock().expect("active map mutex poisoned");
                active.remove(&task_id);
            }
        });

        Some(task_id)
    }

    /// 轮询已完成的 `asyncRewake` 结果（drain）。
    ///
    /// Runtime 在 turn 边界（如 `Stop` 事件前）调用，将结果注入下一轮 prompt。
    /// 调用后队列为空。
    pub async fn poll(&self) -> Vec<RewakeResult> {
        let mut guard = self.completed.lock().await;
        std::mem::take(&mut *guard)
    }

    /// 当前活跃任务名列表（诊断/`doctor` 用）。
    ///
    /// # Panics
    ///
    /// 内部 `Mutex` 被 poison 时 panic（仅在持有锁的线程 panic 时发生，本 crate 不在此处 panic）。
    #[must_use]
    pub fn active_tasks(&self) -> Vec<(u64, String)> {
        let active = self.active.lock().expect("active map mutex poisoned");
        active
            .iter()
            .map(|(id, name)| (*id, name.clone()))
            .collect()
    }
}

impl Default for AsyncRewakeManager {
    fn default() -> Self {
        Self::default_concurrent()
    }
}

impl std::fmt::Debug for AsyncRewakeManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncRewakeManager")
            .field("max_concurrent", &self.max_concurrent)
            .field("inflight", &self.inflight.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn spawn_success_collects_result() {
        let mgr = AsyncRewakeManager::default_concurrent();
        let spec = AsyncRewakeSpec {
            estimated_duration_sec: 5,
            description: "test task".to_string(),
        };
        let task_id = mgr
            .spawn("test-hook", &spec, async {
                Ok("scan complete: 0 issues".to_string())
            })
            .expect("should spawn");

        // 等待后台任务完成
        tokio::time::sleep(Duration::from_millis(50)).await;

        let results = mgr.poll().await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, RewakeStatus::Success);
        assert_eq!(results[0].context, "scan complete: 0 issues");
        assert_eq!(results[0].hook_name, "test-hook");
        assert_eq!(results[0].task_id, task_id);
        assert_eq!(mgr.inflight_count(), 0);
    }

    #[tokio::test]
    async fn spawn_error_collects_error() {
        let mgr = AsyncRewakeManager::default_concurrent();
        let spec = AsyncRewakeSpec {
            estimated_duration_sec: 5,
            description: "failing task".to_string(),
        };
        mgr.spawn("err-hook", &spec, async {
            Err("subprocess crashed".to_string())
        })
        .expect("should spawn");

        tokio::time::sleep(Duration::from_millis(50)).await;

        let results = mgr.poll().await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, RewakeStatus::Error);
        assert!(results[0].context.contains("subprocess crashed"));
    }

    #[tokio::test]
    async fn spawn_timeout_produces_timeout_result() {
        let mgr = AsyncRewakeManager::default_concurrent();
        let spec = AsyncRewakeSpec {
            estimated_duration_sec: 0, // 超时 = 0 × 2 = 0s（立即超时）
            description: "slow task".to_string(),
        };
        mgr.spawn("slow-hook", &spec, async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok("done".to_string())
        })
        .expect("should spawn");

        // 等待超时触发
        tokio::time::sleep(Duration::from_millis(100)).await;

        let results = mgr.poll().await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, RewakeStatus::Timeout);
    }

    #[tokio::test]
    async fn c32_concurrent_limit_rejects() {
        let mgr = AsyncRewakeManager::new(2);

        // 启动 2 个长时任务（占满并发槽）
        let spec = AsyncRewakeSpec {
            estimated_duration_sec: 10,
            description: "long task".to_string(),
        };
        assert!(
            mgr.spawn("h1", &spec, async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok("done".to_string())
            })
            .is_some()
        );
        assert!(
            mgr.spawn("h2", &spec, async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok("done".to_string())
            })
            .is_some()
        );
        assert_eq!(mgr.inflight_count(), 2);

        // 第 3 个应被拒绝（C-32）
        let result = mgr.spawn("h3", &spec, async { Ok("done".to_string()) });
        assert!(result.is_none());
        assert_eq!(mgr.inflight_count(), 2);
    }

    #[tokio::test]
    async fn poll_drains_queue() {
        let mgr = AsyncRewakeManager::default_concurrent();
        let spec = AsyncRewakeSpec {
            estimated_duration_sec: 5,
            description: "task".to_string(),
        };
        mgr.spawn("h1", &spec, async { Ok("r1".to_string()) })
            .expect("spawn 1");
        mgr.spawn("h2", &spec, async { Ok("r2".to_string()) })
            .expect("spawn 2");

        tokio::time::sleep(Duration::from_millis(50)).await;

        let first_poll = mgr.poll().await;
        assert_eq!(first_poll.len(), 2);

        // 第二次 poll 应为空
        let second_poll = mgr.poll().await;
        assert!(second_poll.is_empty(), "expected empty: second_poll");
    }

    #[tokio::test]
    async fn inflight_count_decrements_after_completion() {
        let mgr = AsyncRewakeManager::default_concurrent();
        let spec = AsyncRewakeSpec {
            estimated_duration_sec: 5,
            description: "task".to_string(),
        };
        mgr.spawn("h", &spec, async { Ok("done".to_string()) })
            .expect("spawn");

        assert_eq!(mgr.inflight_count(), 1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(mgr.inflight_count(), 0);
    }

    #[tokio::test]
    async fn active_tasks_tracks_inflight() {
        let mgr = AsyncRewakeManager::new(5);
        let spec = AsyncRewakeSpec {
            estimated_duration_sec: 10,
            description: "long task".to_string(),
        };
        mgr.spawn("active-hook", &spec, async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok("done".to_string())
        })
        .expect("spawn");

        // 活跃中
        let active = mgr.active_tasks();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].1, "active-hook");

        // 等待完成
        tokio::time::sleep(Duration::from_millis(300)).await;

        let active = mgr.active_tasks();
        assert!(active.is_empty(), "expected empty: active");
    }

    #[tokio::test]
    async fn default_concurrent_is_three() {
        let mgr = AsyncRewakeManager::default_concurrent();
        assert_eq!(mgr.max_concurrent(), 3);
    }

    #[tokio::test]
    async fn task_ids_are_unique_and_monotonic() {
        let mgr = AsyncRewakeManager::default_concurrent();
        let spec = AsyncRewakeSpec {
            estimated_duration_sec: 5,
            description: "task".to_string(),
        };
        let id1 = mgr
            .spawn("h1", &spec, async { Ok("r".to_string()) })
            .expect("spawn 1");
        let id2 = mgr
            .spawn("h2", &spec, async { Ok("r".to_string()) })
            .expect("spawn 2");
        assert_ne!(id1, id2);
        assert!(id2 > id1);
    }
}

// ==================== core 调度器 trait 适配（遗留#6 全量接线）====================

/// [`AsyncRewakeManager`] 的 core-trait 适配器：供 sdk 注入 Runtime。
#[derive(Debug)]
pub struct ManagedRewakeScheduler {
    manager: AsyncRewakeManager,
}

impl ManagedRewakeScheduler {
    /// 默认并发上限（C-32，3）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            manager: AsyncRewakeManager::default_concurrent(),
        }
    }
}

impl Default for ManagedRewakeScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl minicoding_core::hooks::AsyncRewakeScheduler for ManagedRewakeScheduler {
    fn try_spawn(
        &self,
        hook_name: &str,
        estimated_duration_sec: u32,
        description: String,
        fut: minicoding_core::provider::BoxFuture<'static, Result<String, String>>,
    ) -> bool {
        let spec = minicoding_core::hooks::AsyncRewakeSpec {
            estimated_duration_sec,
            description: description.clone(),
        };
        self.manager.spawn(hook_name, &spec, fut).is_some()
    }

    fn poll_completed(
        &self,
    ) -> minicoding_core::provider::BoxFuture<'_, Vec<minicoding_core::hooks::RewakeOutcome>> {
        Box::pin(async move {
            self.manager
                .poll()
                .await
                .into_iter()
                .map(|r| minicoding_core::hooks::RewakeOutcome {
                    hook_name: r.hook_name,
                    context: r.context,
                })
                .collect()
        })
    }
}
