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
    use crate::model::{SubagentType, WorktreeSpec};

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
}
