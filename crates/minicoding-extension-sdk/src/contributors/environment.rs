//! `EnvironmentContributor`（顺序 5，`cacheable = true`）。
//!
//! 环境信息段：工作区/平台/git 信息。会话内稳定（利于 prompt cache），跨会话变化。

use minicoding_core::model::PromptError;
use minicoding_core::prompt::{
    Platform, PromptContext, PromptContributor, PromptSection, PromptSectionOrder,
};
use minicoding_core::provider::BoxFuture;

/// 环境信息段 contributor。
pub struct EnvironmentContributor;

impl PromptContributor for EnvironmentContributor {
    fn name(&self) -> &'static str {
        "environment"
    }

    fn order(&self) -> PromptSectionOrder {
        PromptSectionOrder::Environment
    }

    fn cacheable(&self) -> bool {
        true
    }

    fn build(&self, ctx: &PromptContext) -> BoxFuture<'_, Result<PromptSection, PromptError>> {
        let content = format_environment(ctx);
        Box::pin(async move {
            Ok(PromptSection::plain(
                "environment",
                content,
                PromptSectionOrder::Environment,
                true,
            ))
        })
    }
}

fn format_environment(ctx: &PromptContext) -> String {
    use std::fmt::Write as _;
    let mut buf = String::new();
    buf.push_str("## 环境\n\n");
    let _ = writeln!(buf, "- 工作目录: `{}`", ctx.workdir);
    let _ = writeln!(buf, "- 平台: {}", ctx.platform);
    // 平台命令语义提示：LLM 常默认输出 Unix 命令（ls/mkdir -p 等），在 Windows
    // 上必然失败。显式声明当前平台的 shell 语义，引导生成对应命令。
    let _ = match ctx.platform {
        Platform::Windows => writeln!(
            buf,
            "- 命令语义: 当前是 Windows，`shell.run`/`shell.background` 用 `cmd /C` 执行。\
              请使用 Windows 命令（`dir`/`mkdir`/`type`/`copy`/`del` 等），\
              不要使用 Unix 命令（`ls`/`mkdir -p`/`cat`/`rm`/`grep` 等）"
        ),
        _ => writeln!(
            buf,
            "- 命令语义: 当前是 Unix（sh），`shell.run`/`shell.background` 用 `sh -c` 执行"
        ),
    };

    if let Some(ref git) = ctx.git_info {
        buf.push_str("- Git: ");
        if let Some(ref branch) = git.branch {
            let _ = write!(buf, "分支 `{branch}`");
        }
        if let Some(ref head) = git.head {
            if git.branch.is_some() {
                let _ = write!(buf, ", HEAD `{head}`");
            } else {
                let _ = write!(buf, "HEAD `{head}`");
            }
        }
        if git.dirty {
            buf.push_str("（有未提交改动）");
        }
        buf.push('\n');
    }

    buf
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use camino::Utf8PathBuf;
    use minicoding_core::model::SessionId;
    use minicoding_core::prompt::GitInfo;

    #[tokio::test]
    async fn environment_includes_workdir_and_platform() {
        let c = EnvironmentContributor;
        let ctx = PromptContext::new(SessionId::new(), Utf8PathBuf::from("/home/user/project"));
        let s = c.build(&ctx).await.expect("build");
        assert!(s.content.contains("/home/user/project"));
        assert!(s.content.contains("sh -c"));
        assert!(s.cacheable);
    }

    #[tokio::test]
    async fn environment_windows_advises_windows_commands() {
        let c = EnvironmentContributor;
        let ctx = PromptContext::new(SessionId::new(), Utf8PathBuf::from("C:\\proj"))
            .with_platform(minicoding_core::prompt::Platform::Windows);
        let s = c.build(&ctx).await.expect("build");
        assert!(s.content.contains("cmd /C"));
        assert!(s.content.contains("不要使用 Unix 命令"));
    }

    #[tokio::test]
    async fn environment_includes_git_info_when_present() {
        let c = EnvironmentContributor;
        let ctx =
            PromptContext::new(SessionId::new(), Utf8PathBuf::from("/repo")).with_git(GitInfo {
                branch: Some("main".into()),
                head: Some("abc1234".into()),
                dirty: true,
            });
        let s = c.build(&ctx).await.expect("build");
        assert!(s.content.contains("main"));
        assert!(s.content.contains("abc1234"));
        assert!(s.content.contains("未提交"));
    }

    #[tokio::test]
    async fn environment_omits_git_when_absent() {
        let c = EnvironmentContributor;
        let ctx = PromptContext::new(SessionId::new(), Utf8PathBuf::from("/tmp"));
        let s = c.build(&ctx).await.expect("build");
        assert!(!s.content.contains("Git"));
    }
}
