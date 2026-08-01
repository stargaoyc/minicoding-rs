//! 项目文档注入 system 段。
//!
//! 将分层加载的 AGENTS.md 内容追加到 system prompt 末尾，包裹 `<project_doc>` 边界，
//! 声明这是项目约定（受信任的用户输入）而非工具输出数据（C-05）。内容为空时不注入，
//! 保持原 prompt 不变。
//!
//! 与 `inject::inject_memory`（长期记忆）平行：两者都注入 system 段、包裹边界，
//! 但来源与可写性不同——AGENTS.md 是仓库内静态指令层（Agent 不可写，C-23），
//! `long_term.md` 是跨项目动态记忆（Agent 可写，走 `Ask`）。

use crate::ProjectDocLoader;
use minicoding_core::model::MemoryError;

/// 项目文档边界标签（与 `design.md` §8.6 一致）。
pub const PROJECT_DOC_BOUNDARY: &str = "project_doc";

/// 将项目文档注入 system prompt 末尾。
///
/// 拼接结果形如：
/// ```text
/// {原 system prompt}
///
/// <project_doc>
/// {项目文档内容}
/// </project_doc>
/// ```
///
/// 内容经 `trim` 后为空时返回原 prompt（不注入空边界，避免无意义噪声）。
///
/// # Errors
/// `ProjectDocLoader::load` 失败时向上传播 `MemoryError`。
pub async fn inject_project_doc(
    system_prompt: &str,
    loader: &dyn ProjectDocLoader,
) -> Result<String, MemoryError> {
    let doc = loader.load().await?;
    if doc.trim().is_empty() {
        return Ok(system_prompt.to_owned());
    }

    // 原文末尾若已有换行则不重复补；统一以空行分隔正文与项目文档块。
    let prompt = system_prompt.trim_end();
    let injected =
        format!("{prompt}\n\n<{PROJECT_DOC_BOUNDARY}>\n{doc}\n</{PROJECT_DOC_BOUNDARY}>\n");
    Ok(injected)
}

#[cfg(test)]
mod tests {
    //! 验证注入边界包裹与空内容跳过（用静态 loader 解耦文件 IO）。

    use super::*;
    use minicoding_core::memory::ProjectDocLoader;
    use minicoding_core::model::MemoryError;
    use minicoding_core::provider::BoxFuture;

    /// 返回固定内容的 loader，用于测试注入边界与空内容跳过。
    struct StaticDocLoader {
        content: String,
    }

    impl StaticDocLoader {
        fn new(content: &str) -> Self {
            Self {
                content: content.to_owned(),
            }
        }
    }

    impl ProjectDocLoader for StaticDocLoader {
        fn load(&self) -> BoxFuture<'_, Result<String, MemoryError>> {
            let content = self.content.clone();
            Box::pin(async move { Ok(content) })
        }
    }

    #[tokio::test]
    async fn inject_wraps_boundary() {
        let loader = StaticDocLoader::new("# Project\n- use cargo fmt");
        let out = inject_project_doc("You are a coder.", &loader)
            .await
            .unwrap();
        assert!(out.starts_with("You are a coder."));
        assert!(out.contains("<project_doc>"));
        assert!(out.contains("</project_doc>"));
        assert!(out.contains("use cargo fmt"));
    }

    #[tokio::test]
    async fn inject_skips_empty() {
        let loader = StaticDocLoader::new("");
        let out = inject_project_doc("You are a coder.", &loader)
            .await
            .unwrap();
        assert_eq!(out, "You are a coder.");
    }

    #[tokio::test]
    async fn inject_skips_whitespace_only() {
        let loader = StaticDocLoader::new("  \n\t ");
        let out = inject_project_doc("You are a coder.", &loader)
            .await
            .unwrap();
        assert_eq!(out, "You are a coder.");
    }
}
