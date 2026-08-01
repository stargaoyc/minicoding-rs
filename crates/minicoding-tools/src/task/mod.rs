//! 任务管理工具（`task.create`/`task.update`/`task.list`，见 `design.md` §18）。
//!
//! 采用 Claude Code v2.1.142+ 的增量模型：按 `task_id` 创建/更新，状态机
//! `Pending → InProgress → Completed`/`Cancelled` 单向不可跳跃（C-31）。
//! 三个工具均属 `Task` 工具组，`SideEffect::None`（仅更新内存状态）。
//!
//! ## 任务存储
//!
//! `ToolContext` 不承载会话级可变状态，故任务列表由 [`TaskStore`] 抽象管理；
//! 默认实现 [`InMemoryTaskStore`] 持有 `tokio::sync::Mutex<Vec<Task>>`。Runtime
//! 可注入 `SessionMeta` 持久化实现（任务列表跨压缩保留，见 `design.md` §18.5）。

mod create;
mod list;
mod update;

pub use create::TaskCreate;
pub use list::TaskList;
pub use update::TaskUpdate;

use minicoding_core::model::{Task, TaskStatus, ToolError};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::ToolRegistry;
use std::collections::HashMap;
use std::sync::Arc;

/// 增量更新补丁（只更新非 `None` 字段，见 C-31）。
#[derive(Debug, Clone, Default)]
pub struct TaskPatch {
    pub status: Option<TaskStatus>,
    pub summary: Option<String>,
    /// 增量添加依赖边（本任务阻塞哪些 `task_id`），幂等。
    pub add_blocks: Option<Vec<String>>,
    /// 增量添加依赖边（本任务被哪些 `task_id` 阻塞），幂等。
    pub add_blocked_by: Option<Vec<String>>,
}

/// 任务存储抽象（`dyn` 兼容，方法返回 `BoxFuture`）。
///
/// Runtime 可注入持久化实现；默认用 [`InMemoryTaskStore`]。
pub trait TaskStore: Send + Sync {
    /// 创建任务，返回新 `Task`（ULID 由 Runtime 生成，C-31）。
    fn create(&self, content: String) -> BoxFuture<'_, Result<Task, ToolError>>;
    /// 按 `task_id` 增量更新；伪造 id 返回 `NotFound`（C-31）。
    fn update(&self, task_id: String, patch: TaskPatch) -> BoxFuture<'_, Result<Task, ToolError>>;
    /// 列出任务（`filter` 为 `None` 返回全部）。
    fn list(&self, filter: Option<TaskStatus>) -> BoxFuture<'_, Vec<Task>>;
}

/// 内存任务存储（默认实现，非持久化）。
///
/// 使用 `tokio::sync::Mutex`（见 AGENTS.md §2.4）；临界区内无 `await`，仅做
/// O(n) 校验与提交（n ≤ 20 任务）。
#[derive(Default)]
pub struct InMemoryTaskStore {
    tasks: tokio::sync::Mutex<Vec<Task>>,
}

impl InMemoryTaskStore {
    /// 创建空存储。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl TaskStore for InMemoryTaskStore {
    fn create(&self, content: String) -> BoxFuture<'_, Result<Task, ToolError>> {
        Box::pin(async move {
            if content.trim().is_empty() {
                return Err(ToolError::InvalidInput(
                    "content must not be empty".to_string(),
                ));
            }
            let task = Task::new(content);
            let mut tasks = self.tasks.lock().await;
            tasks.push(task.clone());
            Ok(task)
        })
    }

    fn update(&self, task_id: String, patch: TaskPatch) -> BoxFuture<'_, Result<Task, ToolError>> {
        Box::pin(async move {
            let mut tasks = self.tasks.lock().await;
            let idx = tasks
                .iter()
                .position(|t| t.id == task_id)
                .ok_or(ToolError::NotFound(task_id))?;

            // 1. 校验状态迁移（C-31：不可跳跃、不可回退）
            if let Some(target) = patch.status {
                if !tasks[idx].status.can_transition_to(target) {
                    return Err(ToolError::InvalidStateTransition(format!(
                        "cannot transition from {} to {}",
                        tasks[idx].status.as_str(),
                        target.as_str()
                    )));
                }
                // Completed/Cancelled 必填 summary（本次提供或已有均可）
                if matches!(target, TaskStatus::Completed | TaskStatus::Cancelled)
                    && patch.summary.is_none()
                    && tasks[idx].summary.is_none()
                {
                    return Err(ToolError::InvalidInput(format!(
                        "summary required to transition to {}",
                        target.as_str()
                    )));
                }
            }

            // 2. 计算增量后的依赖边（幂等去重）
            let new_blocks = dedup_extend(&tasks[idx].blocks, patch.add_blocks.as_deref());
            let new_blocked_by =
                dedup_extend(&tasks[idx].blocked_by, patch.add_blocked_by.as_deref());

            // 3. 成环检测：以克隆图代入新边后 DFS（C-31）
            let mut prospective = tasks.clone();
            prospective[idx].blocks.clone_from(&new_blocks);
            prospective[idx].blocked_by.clone_from(&new_blocked_by);
            if has_cycle(&prospective) {
                return Err(ToolError::InvalidInput(
                    "dependency cycle detected".to_string(),
                ));
            }

            // 4. 全部校验通过后提交（避免部分更新）
            let task = &mut tasks[idx];
            if let Some(summary) = patch.summary {
                task.summary = Some(summary);
            }
            if let Some(target) = patch.status {
                task.status = target;
            }
            task.blocks = new_blocks;
            task.blocked_by = new_blocked_by;
            task.touch();
            Ok(task.clone())
        })
    }

    fn list(&self, filter: Option<TaskStatus>) -> BoxFuture<'_, Vec<Task>> {
        Box::pin(async move {
            let tasks = self.tasks.lock().await;
            match filter {
                Some(status) => tasks
                    .iter()
                    .filter(|t| t.status == status)
                    .cloned()
                    .collect(),
                None => tasks.clone(),
            }
        })
    }
}

/// 将 `extra` 增量并入 `base`，幂等去重（重复添加不报错、不重复入图）。
fn dedup_extend(base: &[String], extra: Option<&[String]>) -> Vec<String> {
    let mut out = base.to_vec();
    if let Some(ids) = extra {
        for id in ids {
            if !out.contains(id) {
                out.push(id.clone());
            }
        }
    }
    out
}

/// 检测任务依赖图是否有环（C-31）。
///
/// 图的边表示"必须先于"：`T.blocks` 含 `B` → `T` 先于 `B`（`T` 阻塞 `B`）；
/// `T.blocked_by` 含 `A` → `A` 先于 `T`。环即循环依赖（死锁），拒绝。
fn has_cycle(tasks: &[Task]) -> bool {
    // 邻接表：u -> [v, ..] 表示 u 必须先于 v
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for t in tasks {
        for b in &t.blocks {
            adj.entry(t.id.as_str()).or_default().push(b.as_str());
        }
        for a in &t.blocked_by {
            adj.entry(a.as_str()).or_default().push(t.id.as_str());
        }
    }
    // 三色 DFS：0=未访问，1=在路径上（灰），2=完成（黑）
    let mut color: HashMap<&str, u8> = tasks.iter().map(|t| (t.id.as_str(), 0)).collect();
    let starts: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    for &start in &starts {
        if color[start] != 0 {
            continue;
        }
        let mut stack: Vec<&str> = vec![start];
        while let Some(&node) = stack.last() {
            if color[node] == 0 {
                color.insert(node, 1);
            }
            let mut descend: Option<&str> = None;
            if let Some(neighbors) = adj.get(node) {
                for &n in neighbors {
                    match color.get(n).copied() {
                        Some(0) => {
                            descend = Some(n);
                            break;
                        }
                        Some(1) => return true, // 回边 → 环
                        _ => {}
                    }
                }
            }
            if let Some(n) = descend {
                stack.push(n);
            } else {
                color.insert(node, 2);
                stack.pop();
            }
        }
    }
    false
}

/// 注册全部任务工具到 `registry`（共享一个 `InMemoryTaskStore`）。
///
/// Runtime 若需注入自定义 [`TaskStore`]，可直接用各工具的 `new(store)` 构造后
/// 自行注册。
pub fn register_task_tools(registry: &mut ToolRegistry) {
    let store: Arc<dyn TaskStore> = Arc::new(InMemoryTaskStore::new());
    registry.register(Arc::new(TaskCreate::new(store.clone())));
    registry.register(Arc::new(TaskUpdate::new(store.clone())));
    registry.register(Arc::new(TaskList::new(store)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store() -> Arc<dyn TaskStore> {
        Arc::new(InMemoryTaskStore::new())
    }

    #[tokio::test]
    async fn create_returns_pending_task_with_id() {
        let store = make_store();
        let task = store.create("demo task".to_string()).await.unwrap();
        assert_eq!(task.status, TaskStatus::Pending);
        assert_eq!(task.id.len(), 26);
        assert!(task.blocks.is_empty());
    }

    #[tokio::test]
    async fn create_rejects_empty_content() {
        let store = make_store();
        let err = store.create("   ".to_string()).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn update_forged_id_returns_not_found() {
        let store = make_store();
        let err = store
            .update("nonexistent".to_string(), TaskPatch::default())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn state_machine_rejects_skip_and_backtrack() {
        let store = make_store();
        let task = store.create("t".to_string()).await.unwrap();
        // 跳跃 Pending -> Completed 失败
        let err = store
            .update(
                task.id.clone(),
                TaskPatch {
                    status: Some(TaskStatus::Completed),
                    summary: Some("done".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidStateTransition(_)));

        // Pending -> InProgress 成功
        store
            .update(
                task.id.clone(),
                TaskPatch {
                    status: Some(TaskStatus::InProgress),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // 回退 InProgress -> Pending 失败
        let err = store
            .update(
                task.id.clone(),
                TaskPatch {
                    status: Some(TaskStatus::Pending),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidStateTransition(_)));
    }

    #[tokio::test]
    async fn terminal_status_requires_summary() {
        let store = make_store();
        let task = store.create("t".to_string()).await.unwrap();
        store
            .update(
                task.id.clone(),
                TaskPatch {
                    status: Some(TaskStatus::InProgress),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // Completed 缺 summary 失败
        let err = store
            .update(
                task.id.clone(),
                TaskPatch {
                    status: Some(TaskStatus::Completed),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
        // 带 summary 成功
        let updated = store
            .update(
                task.id.clone(),
                TaskPatch {
                    status: Some(TaskStatus::Completed),
                    summary: Some("finished".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.status, TaskStatus::Completed);
        assert_eq!(updated.summary.as_deref(), Some("finished"));
    }

    #[tokio::test]
    async fn add_blocks_is_idempotent() {
        let store = make_store();
        let a = store.create("a".to_string()).await.unwrap();
        let b = store.create("b".to_string()).await.unwrap();
        // 声明 a 阻塞 b
        store
            .update(
                b.id.clone(),
                TaskPatch {
                    add_blocked_by: Some(vec![a.id.clone()]),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // 重复声明（幂等，不报错不重复入图）
        let updated = store
            .update(
                b.id.clone(),
                TaskPatch {
                    add_blocked_by: Some(vec![a.id.clone()]),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.blocked_by, vec![a.id.clone()]);
        assert_eq!(updated.blocked_by.len(), 1);
    }

    #[tokio::test]
    async fn cycle_detected_on_circular_dependency() {
        let store = make_store();
        let a = store.create("a".to_string()).await.unwrap();
        let b = store.create("b".to_string()).await.unwrap();
        // a 阻塞 b
        store
            .update(
                b.id.clone(),
                TaskPatch {
                    add_blocked_by: Some(vec![a.id.clone()]),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // b 阻塞 a → 成环，拒绝
        let err = store
            .update(
                a.id.clone(),
                TaskPatch {
                    add_blocked_by: Some(vec![b.id.clone()]),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
        // 失败不应提交，a 仍无 blocked_by
        let a_now = store
            .list(None)
            .await
            .into_iter()
            .find(|t| t.id == a.id)
            .unwrap();
        assert!(a_now.blocked_by.is_empty());
    }

    #[tokio::test]
    async fn self_reference_is_cycle() {
        let store = make_store();
        let a = store.create("a".to_string()).await.unwrap();
        let err = store
            .update(
                a.id.clone(),
                TaskPatch {
                    add_blocked_by: Some(vec![a.id.clone()]),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn list_filters_by_status() {
        let store = make_store();
        let a = store.create("a".to_string()).await.unwrap();
        let b = store.create("b".to_string()).await.unwrap();
        store
            .update(
                b.id.clone(),
                TaskPatch {
                    status: Some(TaskStatus::InProgress),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let pending = store.list(Some(TaskStatus::Pending)).await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, a.id);
        let in_progress = store.list(Some(TaskStatus::InProgress)).await;
        assert_eq!(in_progress.len(), 1);
        assert_eq!(in_progress[0].id, b.id);
        let all = store.list(None).await;
        assert_eq!(all.len(), 2);
    }
}
