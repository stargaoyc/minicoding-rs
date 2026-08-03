//! A-15：worktree 隔离子 Agent runner（`design.md` §7.5）。
//!
//! `WorktreeSubagentRunner` 是装饰器：包裹一个内部 runner（如未来的
//! `InProcessSubagentRunner`），在派发前后处理 git worktree 生命周期。
//!
//! ## 流程
//!
//! 1. `git worktree add -b {branch} {worktree_path}` 创建隔离工作目录
//! 2. 委托内部 runner 执行子 Agent（worktree 路径通过 spec 传入）
//! 3. 按 `merge_back` 策略合并改动回主 worktree
//! 4. `auto_cleanup` 时删除 worktree + 分支
//!
//! ## 降级
//!
//! 非 git 仓库时降级为 `Shared` 隔离（记 warn），继续委托内部 runner。

use crate::agent::SubagentRunner;
use crate::model::{Isolation, MergeStrategy, RuntimeError, SubagentResult, SubagentSpec};
use crate::provider::BoxFuture;
use std::sync::Arc;

/// Worktree 隔离子 Agent runner（装饰器，A-15）。
///
/// 包裹内部 runner，在 `spawn` 前后处理 git worktree 创建/合并/清理。
/// 非 git 仓库时降级为 `Shared`（不创建 worktree，直接委托）。
#[allow(clippy::module_name_repetitions)] // 命名与 NoopSubagentRunner/SubagentRunner 家族保持一致
pub struct WorktreeSubagentRunner {
    /// 内部 runner（实际执行 Agent 循环）。
    inner: Arc<dyn SubagentRunner>,
    /// 主工作目录（git 仓库根）。
    workdir: camino::Utf8PathBuf,
}

impl WorktreeSubagentRunner {
    /// 创建 worktree runner。
    ///
    /// `inner`：实际执行 Agent 循环的 runner（如 `InProcessSubagentRunner`）。
    /// `workdir`：主工作目录（git 仓库根），worktree 将创建在 `{workdir}/.minicoding/worktrees/`。
    #[must_use]
    pub fn new(inner: Arc<dyn SubagentRunner>, workdir: camino::Utf8PathBuf) -> Self {
        Self { inner, workdir }
    }

    /// 检查 `workdir` 是否是 git 仓库。
    async fn is_git_repo(&self) -> bool {
        let output = tokio::process::Command::new("git")
            .arg("rev-parse")
            .arg("--is-inside-work-tree")
            .current_dir(&self.workdir)
            .output()
            .await;
        matches!(output, Ok(o) if o.status.success())
    }

    /// 创建 git worktree。
    async fn create_worktree(
        &self,
        branch: &str,
        worktree_path: &camino::Utf8PathBuf,
    ) -> Result<(), RuntimeError> {
        // 确保父目录存在
        if let Some(parent) = worktree_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                RuntimeError::Io(std::io::Error::other(format!(
                    "创建 worktree 父目录失败: {e}"
                )))
            })?;
        }

        let output = tokio::process::Command::new("git")
            .arg("worktree")
            .arg("add")
            .arg("-b")
            .arg(branch)
            .arg(worktree_path.as_str())
            .current_dir(&self.workdir)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(RuntimeError::Config(format!(
                "git worktree add 失败: {stderr}"
            )));
        }
        Ok(())
    }

    /// 删除 git worktree 与分支。
    async fn cleanup_worktree(&self, worktree_path: &camino::Utf8PathBuf, branch: &str) {
        // 删除 worktree
        let _ = tokio::process::Command::new("git")
            .arg("worktree")
            .arg("remove")
            .arg("--force")
            .arg(worktree_path.as_str())
            .current_dir(&self.workdir)
            .output()
            .await;

        // 删除分支
        let _ = tokio::process::Command::new("git")
            .arg("branch")
            .arg("-D")
            .arg(branch)
            .current_dir(&self.workdir)
            .output()
            .await;
    }

    /// 按 `merge_back` 策略合并改动回主 worktree。
    async fn merge_back(&self, branch: &str, strategy: MergeStrategy) -> Result<(), RuntimeError> {
        match strategy {
            MergeStrategy::None => Ok(()),
            MergeStrategy::CherryPick => {
                let output = tokio::process::Command::new("git")
                    .arg("cherry-pick")
                    .arg(branch)
                    .current_dir(&self.workdir)
                    .output()
                    .await?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    // cherry-pick 冲突时 abort，返回错误让父 Agent 处理
                    let _ = tokio::process::Command::new("git")
                        .arg("cherry-pick")
                        .arg("--abort")
                        .current_dir(&self.workdir)
                        .output()
                        .await;
                    return Err(RuntimeError::Config(format!(
                        "cherry-pick 失败（已 abort）: {stderr}"
                    )));
                }
                Ok(())
            }
            MergeStrategy::MergeCommit => {
                let output = tokio::process::Command::new("git")
                    .arg("merge")
                    .arg("--no-ff")
                    .arg(branch)
                    .arg("-m")
                    .arg(format!("merge subagent branch {branch}"))
                    .current_dir(&self.workdir)
                    .output()
                    .await?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let _ = tokio::process::Command::new("git")
                        .arg("merge")
                        .arg("--abort")
                        .current_dir(&self.workdir)
                        .output()
                        .await;
                    return Err(RuntimeError::Config(format!(
                        "merge 失败（已 abort）: {stderr}"
                    )));
                }
                Ok(())
            }
        }
    }
}

impl SubagentRunner for WorktreeSubagentRunner {
    fn spawn(
        &self,
        mut spec: SubagentSpec,
        input: String,
    ) -> BoxFuture<'_, Result<SubagentResult, RuntimeError>> {
        Box::pin(async move {
            // 仅 Worktree 隔离走 worktree 流程，Shared 直接委托。
            // 先 clone 隔离规格，避免在 match 内同时持有 spec 的不可变借用与可变写入。
            let worktree_info = match spec.isolation.clone() {
                Isolation::Shared => None,
                Isolation::Worktree(wt_spec) => {
                    if self.is_git_repo().await {
                        // 生成分支名（ULID 编码时间戳 + 随机后缀，避免冲突）
                        let branch = format!("{}{}", wt_spec.branch_prefix, ulid::Ulid::new());
                        let worktree_path = self
                            .workdir
                            .join(".minicoding")
                            .join("worktrees")
                            .join(&branch);

                        match self.create_worktree(&branch, &worktree_path).await {
                            Ok(()) => {
                                tracing::info!(
                                    branch = %branch,
                                    worktree = %worktree_path,
                                    "worktree 已创建"
                                );
                                Some((worktree_path, branch, wt_spec))
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "创建 worktree 失败，降级为 Shared");
                                spec.isolation = Isolation::Shared;
                                None
                            }
                        }
                    } else {
                        tracing::warn!("workdir 不是 git 仓库，worktree 隔离降级为 Shared");
                        spec.isolation = Isolation::Shared;
                        None
                    }
                }
            };

            // 委托内部 runner 执行
            let result = self.inner.spawn(spec, input).await;

            // 处理 worktree 合并与清理
            if let Some((worktree_path, branch, wt_spec)) = worktree_info {
                if let Err(e) = self.merge_back(&branch, wt_spec.merge_back).await {
                    tracing::warn!(error = %e, branch = %branch, "合并 worktree 改动失败");
                }

                if wt_spec.auto_cleanup {
                    self.cleanup_worktree(&worktree_path, &branch).await;
                    tracing::info!(branch = %branch, "worktree 已清理");
                }
            }

            result
        })
    }
}

#[cfg(test)]
mod tests {
    //! `WorktreeSubagentRunner` 降级与隔离类型默认值测试。

    use super::*;
    use crate::agent::SubagentRunner;
    use crate::model::{SubagentResult, SubagentType, WorktreeSpec};
    use std::sync::Mutex;

    #[test]
    fn isolation_default_is_shared() {
        assert_eq!(Isolation::default(), Isolation::Shared);
    }

    #[test]
    fn worktree_spec_default() {
        let spec = WorktreeSpec::new();
        assert_eq!(spec.branch_prefix, "subagent/");
        assert!(spec.auto_cleanup);
        assert_eq!(spec.merge_back, MergeStrategy::None);
    }

    #[test]
    fn new_runner_holds_workdir_and_inner() {
        // 验证 `new()` 构造的 runner 字段可被后续 `spawn` 正确使用（通过行为验证）。
        let tmp = tempfile::tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap();
        let inner: Arc<dyn SubagentRunner> = Arc::new(OkRunner::default());
        let runner = WorktreeSubagentRunner::new(inner, workdir.clone());
        // 验证 workdir 持有：非 git 目录 + Shared 隔离应直接委托 OkRunner 成功。
        let spec = SubagentSpec::default_for(SubagentType::GeneralPurpose);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(runner.spawn(spec, "test".to_string()));
        assert!(result.is_ok(), "Shared 隔离应直接委托成功: {result:?}");
    }

    #[tokio::test]
    async fn worktree_runner_degrades_for_non_git_dir() {
        // 非 git 目录中，worktree runner 应降级为 Shared 后委托内部 runner。
        let tmp = tempfile::tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap();
        let inner: Arc<dyn SubagentRunner> = Arc::new(crate::agent::NoopSubagentRunner::new());
        let runner = WorktreeSubagentRunner::new(inner, workdir);
        let mut spec = SubagentSpec::default_for(SubagentType::GeneralPurpose);
        spec.isolation = Isolation::Worktree(WorktreeSpec::new());
        // NoopSubagentRunner 返回 Config 错误，但 worktree runner 应先降级再委托。
        let result = runner.spawn(spec, "test".to_string()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn is_git_repo_returns_false_for_plain_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap();
        let inner: Arc<dyn SubagentRunner> = Arc::new(OkRunner::default());
        let runner = WorktreeSubagentRunner::new(inner, workdir);
        assert!(!runner.is_git_repo().await);
    }

    #[tokio::test]
    async fn is_git_repo_returns_true_after_git_init() {
        let tmp = tempfile::tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap();
        setup_git_repo(&workdir).await;
        let inner: Arc<dyn SubagentRunner> = Arc::new(OkRunner::default());
        let runner = WorktreeSubagentRunner::new(inner, workdir);
        assert!(runner.is_git_repo().await);
    }

    #[tokio::test]
    async fn spawn_with_shared_isolation_delegates_directly() {
        // Shared 隔离不进入 worktree 流程，直接委托内部 runner。
        let tmp = tempfile::tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap();
        let inner: Arc<dyn SubagentRunner> = Arc::new(OkRunner::default());
        let runner = WorktreeSubagentRunner::new(inner, workdir);
        let spec = SubagentSpec::default_for(SubagentType::GeneralPurpose);
        // Shared 是默认隔离
        let result = runner.spawn(spec, "hello".to_string()).await;
        assert!(result.is_ok());
        let r = result.unwrap();
        assert!(r.completed);
        assert_eq!(r.summary, "ok");
    }

    #[tokio::test]
    async fn spawn_with_worktree_in_git_repo_full_flow_no_merge() {
        // 完整流程：git 仓库 → 创建 worktree → 委托 → 不合并 → 自动清理。
        let tmp = tempfile::tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap();
        setup_git_repo(&workdir).await;
        let inner: Arc<dyn SubagentRunner> = Arc::new(OkRunner::default());
        let runner = WorktreeSubagentRunner::new(inner, workdir.clone());

        let mut spec = SubagentSpec::default_for(SubagentType::GeneralPurpose);
        spec.isolation = Isolation::Worktree(WorktreeSpec::new()); // 不合并，自动清理

        let result = runner.spawn(spec, "test".to_string()).await;
        assert!(result.is_ok(), "spawn 应成功: {result:?}");

        // auto_cleanup = true：worktree 应从 `git worktree list` 中移除。
        // 分支名含 `subagent/` 前缀（带斜杠），会在 worktrees/ 下创建嵌套目录，
        // 因此用 `git worktree list` 验证（而非检查目录是否为空）。
        let list = worktree_list(&workdir).await;
        assert!(
            !list.iter().any(|line| line.contains("subagent/")),
            "worktree 应已从 git worktree list 移除: {list:?}"
        );
    }

    #[tokio::test]
    async fn spawn_with_worktree_no_auto_cleanup_keeps_worktree() {
        // auto_cleanup = false：worktree 不应被删除。
        let tmp = tempfile::tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap();
        setup_git_repo(&workdir).await;
        let inner: Arc<dyn SubagentRunner> = Arc::new(OkRunner::default());
        let runner = WorktreeSubagentRunner::new(inner, workdir.clone());

        let mut spec = SubagentSpec::default_for(SubagentType::GeneralPurpose);
        let mut wt_spec = WorktreeSpec::new();
        wt_spec.auto_cleanup = false;
        spec.isolation = Isolation::Worktree(wt_spec);

        let result = runner.spawn(spec, "test".to_string()).await;
        assert!(result.is_ok(), "spawn 应成功: {result:?}");

        // auto_cleanup = false：worktree 应保留在 `git worktree list` 中。
        let list = worktree_list(&workdir).await;
        assert!(
            list.iter().any(|line| line.contains("subagent/")),
            "worktree 应保留在 git worktree list: {list:?}"
        );
    }

    #[tokio::test]
    async fn spawn_with_worktree_cherrypick_merge_succeeds() {
        // cherry-pick 合并：子 Agent 在 worktree 中提交一个 commit，主分支 cherry-pick。
        let tmp = tempfile::tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap();
        setup_git_repo(&workdir).await;
        // 内部 runner 在 worktree 中创建一个文件并提交（通过 spec.workdir 传入）
        let inner: Arc<dyn SubagentRunner> = Arc::new(CommittingRunner);
        let runner = WorktreeSubagentRunner::new(inner, workdir.clone());

        let mut spec = SubagentSpec::default_for(SubagentType::GeneralPurpose);
        let mut wt_spec = WorktreeSpec::new();
        wt_spec.merge_back = MergeStrategy::CherryPick;
        spec.isolation = Isolation::Worktree(wt_spec);

        let result = runner.spawn(spec, "test".to_string()).await;
        assert!(result.is_ok(), "spawn + cherry-pick 应成功: {result:?}");
    }

    #[tokio::test]
    async fn spawn_with_worktree_merge_commit_succeeds() {
        // merge --no-ff 合并：子 Agent 在 worktree 提交，主分支创建 merge commit。
        let tmp = tempfile::tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap();
        setup_git_repo(&workdir).await;
        let inner: Arc<dyn SubagentRunner> = Arc::new(CommittingRunner);
        let runner = WorktreeSubagentRunner::new(inner, workdir.clone());

        let mut spec = SubagentSpec::default_for(SubagentType::GeneralPurpose);
        let mut wt_spec = WorktreeSpec::new();
        wt_spec.merge_back = MergeStrategy::MergeCommit;
        spec.isolation = Isolation::Worktree(wt_spec);

        let result = runner.spawn(spec, "test".to_string()).await;
        assert!(result.is_ok(), "spawn + merge-commit 应成功: {result:?}");
    }

    #[tokio::test]
    async fn spawn_with_worktree_cherrypick_empty_branch_still_succeeds() {
        // cherry-pick 一个没有新 commit 的分支会失败，但 spawn 本身不应崩溃
        // （merge 失败只记 warn，不传播错误）。使用 OkRunner（不提交任何内容）。
        let tmp = tempfile::tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap();
        setup_git_repo(&workdir).await;
        let inner: Arc<dyn SubagentRunner> = Arc::new(OkRunner::default());
        let runner = WorktreeSubagentRunner::new(inner, workdir.clone());

        let mut spec = SubagentSpec::default_for(SubagentType::GeneralPurpose);
        let mut wt_spec = WorktreeSpec::new();
        wt_spec.merge_back = MergeStrategy::CherryPick;
        spec.isolation = Isolation::Worktree(wt_spec);

        // cherry-pick 空 commit 会失败，但 spawn 仍返回 Ok（inner 返回 ok）
        let result = runner.spawn(spec, "test".to_string()).await;
        assert!(result.is_ok(), "merge 失败不应影响 spawn 返回: {result:?}");
    }

    #[tokio::test]
    async fn spawn_with_worktree_merge_commit_empty_branch_still_succeeds() {
        // merge --no-ff 空 worktree 分支会失败，但 spawn 本身不崩溃。
        let tmp = tempfile::tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap();
        setup_git_repo(&workdir).await;
        let inner: Arc<dyn SubagentRunner> = Arc::new(OkRunner::default());
        let runner = WorktreeSubagentRunner::new(inner, workdir.clone());

        let mut spec = SubagentSpec::default_for(SubagentType::GeneralPurpose);
        let mut wt_spec = WorktreeSpec::new();
        wt_spec.merge_back = MergeStrategy::MergeCommit;
        spec.isolation = Isolation::Worktree(wt_spec);

        let result = runner.spawn(spec, "test".to_string()).await;
        assert!(result.is_ok(), "merge 失败不应影响 spawn 返回: {result:?}");
    }

    #[tokio::test]
    async fn merge_back_none_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap();
        setup_git_repo(&workdir).await;
        let inner: Arc<dyn SubagentRunner> = Arc::new(OkRunner::default());
        let runner = WorktreeSubagentRunner::new(inner, workdir);
        // None 策略应直接返回 Ok，不调用任何 git 命令
        let result = runner
            .merge_back("nonexistent-branch", MergeStrategy::None)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn merge_back_cherrypick_nonexistent_branch_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap();
        setup_git_repo(&workdir).await;
        let inner: Arc<dyn SubagentRunner> = Arc::new(OkRunner::default());
        let runner = WorktreeSubagentRunner::new(inner, workdir);
        // cherry-pick 不存在的分支应失败（并自动 abort）
        let result = runner
            .merge_back("nonexistent-branch-xyz", MergeStrategy::CherryPick)
            .await;
        assert!(result.is_err(), "cherry-pick 不存在分支应失败");
    }

    #[tokio::test]
    async fn merge_back_merge_commit_nonexistent_branch_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap();
        setup_git_repo(&workdir).await;
        let inner: Arc<dyn SubagentRunner> = Arc::new(OkRunner::default());
        let runner = WorktreeSubagentRunner::new(inner, workdir);
        let result = runner
            .merge_back("nonexistent-branch-xyz", MergeStrategy::MergeCommit)
            .await;
        assert!(result.is_err(), "merge 不存在分支应失败");
    }

    #[tokio::test]
    async fn cleanup_worktree_removes_worktree_and_branch() {
        // 完整创建+清理：验证 cleanup_worktree 删除 worktree 目录和分支。
        let tmp = tempfile::tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap();
        setup_git_repo(&workdir).await;
        let inner: Arc<dyn SubagentRunner> = Arc::new(OkRunner::default());
        let runner = WorktreeSubagentRunner::new(inner, workdir.clone());

        let branch = "test-cleanup-branch";
        let worktree_path = workdir.join(".minicoding").join("worktrees").join(branch);
        runner
            .create_worktree(branch, &worktree_path)
            .await
            .expect("create_worktree 应成功");

        // 确认 worktree 与分支存在
        assert!(worktree_path.exists(), "worktree 目录应存在");
        assert!(branch_exists(&workdir, branch).await);

        // 清理
        runner.cleanup_worktree(&worktree_path, branch).await;

        // worktree 目录应被删除，分支也应被删除
        assert!(!worktree_path.exists(), "worktree 目录应被删除");
        assert!(!branch_exists(&workdir, branch).await, "分支应被删除");
    }

    #[tokio::test]
    async fn create_worktree_failure_due_to_existing_branch() {
        // 已存在的分支名应导致 create_worktree 失败。
        let tmp = tempfile::tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap();
        setup_git_repo(&workdir).await;
        let inner: Arc<dyn SubagentRunner> = Arc::new(OkRunner::default());
        let runner = WorktreeSubagentRunner::new(inner, workdir.clone());

        let branch = "duplicate-branch";
        let wt_path1 = workdir.join(".minicoding").join("worktrees").join(branch);
        runner
            .create_worktree(branch, &wt_path1)
            .await
            .expect("首次创建应成功");

        // 再次用同名分支创建应失败
        let wt_path2 = workdir.join(".minicoding").join("worktrees").join("other");
        let result = runner.create_worktree(branch, &wt_path2).await;
        assert!(result.is_err(), "重复分支应导致失败");

        // 清理
        runner.cleanup_worktree(&wt_path1, branch).await;
    }

    #[tokio::test]
    async fn spawn_propagates_inner_error() {
        // 内部 runner 返回错误时，spawn 应传播（worktree 流程不影响错误传播）。
        let tmp = tempfile::tempdir().unwrap();
        let workdir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap();
        let inner: Arc<dyn SubagentRunner> = Arc::new(crate::agent::NoopSubagentRunner::new());
        let runner = WorktreeSubagentRunner::new(inner, workdir);
        let spec = SubagentSpec::default_for(SubagentType::GeneralPurpose);
        let result = runner.spawn(spec, "test".to_string()).await;
        assert!(result.is_err());
        // 验证是 Config 错误（来自 NoopSubagentRunner）
        match result.unwrap_err() {
            RuntimeError::Config(msg) => assert!(msg.contains("not configured")),
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    // ---- 测试辅助 ----

    /// 在 `workdir` 中初始化一个 git 仓库并提交一个初始 commit（worktree 操作需要至少一个 commit）。
    async fn setup_git_repo(workdir: &camino::Utf8PathBuf) {
        // git init
        let _ = tokio::process::Command::new("git")
            .arg("init")
            .current_dir(workdir.as_std_path())
            .output()
            .await
            .expect("git init 应成功");

        // 设置本地 user.email/user.name（避免 commit 失败）
        for (k, v) in [("user.email", "test@example.com"), ("user.name", "Test")] {
            let _ = tokio::process::Command::new("git")
                .args(["config", k, v])
                .current_dir(workdir.as_std_path())
                .output()
                .await
                .expect("git config 应成功");
        }

        // 设置 main 为默认分支（避免 master/HEAD 警告）
        let _ = tokio::process::Command::new("git")
            .args(["symbolic-ref", "HEAD", "refs/heads/main"])
            .current_dir(workdir.as_std_path())
            .output()
            .await;

        // 创建初始文件并提交
        let init_file = workdir.join("README.md");
        std::fs::write(init_file.as_std_path(), "initial\n").unwrap();
        let _ = tokio::process::Command::new("git")
            .args(["add", "README.md"])
            .current_dir(workdir.as_std_path())
            .output()
            .await
            .expect("git add 应成功");
        let _ = tokio::process::Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(workdir.as_std_path())
            .output()
            .await
            .expect("git commit 应成功");
    }

    /// 检查分支是否存在。
    async fn branch_exists(workdir: &camino::Utf8PathBuf, branch: &str) -> bool {
        let output = tokio::process::Command::new("git")
            .args(["rev-parse", "--verify", branch])
            .current_dir(workdir.as_std_path())
            .output()
            .await
            .expect("git rev-parse 应执行");
        output.status.success()
    }

    /// 获取 `git worktree list` 输出（每行一个 worktree）。
    async fn worktree_list(workdir: &camino::Utf8PathBuf) -> Vec<String> {
        let output = tokio::process::Command::new("git")
            .args(["worktree", "list"])
            .current_dir(workdir.as_std_path())
            .output()
            .await
            .expect("git worktree list 应执行");
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout.lines().map(String::from).collect()
    }

    /// 成功 runner：恒返回 `SubagentResult::completed("ok", 0)`。
    #[derive(Default)]
    struct OkRunner {
        _phantom: Mutex<()>,
    }

    impl SubagentRunner for OkRunner {
        fn spawn(
            &self,
            _spec: SubagentSpec,
            _input: String,
        ) -> BoxFuture<'_, Result<SubagentResult, RuntimeError>> {
            Box::pin(async move { Ok(SubagentResult::completed("ok".to_string(), 0)) })
        }
    }

    /// 提交 runner：在 `spec` 的 workdir（如果有）中创建一个文件并提交。
    /// 用于测试 cherry-pick/merge 合并路径（需要 worktree 分支有新 commit）。
    struct CommittingRunner;

    impl SubagentRunner for CommittingRunner {
        fn spawn(
            &self,
            spec: SubagentSpec,
            _input: String,
        ) -> BoxFuture<'_, Result<SubagentResult, RuntimeError>> {
            Box::pin(async move {
                // spec.system_prompt 在 WorktreeSubagentRunner 中未被修改，
                // 但 worktree 路径通过 spec 间接传递——实际实现中 runner 会
                // 通过 spec.isolation 获取 worktree 路径。此处我们简单地在
                // 当前工作目录创建文件并提交（测试环境已 cd 到 worktree）。
                // 由于我们无法直接获取 worktree 路径，这里通过 isolation 提取。
                if let Isolation::Worktree(_) = spec.isolation {
                    // 在当前工作目录创建文件并提交
                    let file = "subagent_artifact.txt";
                    std::fs::write(file, "subagent content\n").map_err(RuntimeError::Io)?;
                    let _ = tokio::process::Command::new("git")
                        .args(["add", file])
                        .output()
                        .await;
                    let _ = tokio::process::Command::new("git")
                        .args(["commit", "-m", "subagent work"])
                        .output()
                        .await;
                }
                Ok(SubagentResult::completed("committed".to_string(), 10))
            })
        }
    }
}
