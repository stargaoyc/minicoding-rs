//! 压缩后状态保留清单（见 `docs/design.md` §3.7）。
//!
//! 压缩是破坏性操作（L2 摘要替换原文、L3 滚动窗口丢弃旧消息），但某些跨压缩
//! 必须保留的状态需显式保护。`StateKeep` 在压缩前快照这些状态，压缩后断言
//! 未被篡改（debug 模式）。
//!
//! 跨压缩保留的状态清单（见 `docs/design.md` §3.7）：
//!
//! | 状态 | 保留方式 |
//! |------|---------|
//! | 系统 prompt（含长期记忆、AGENTS.md） | `ContextManagerImpl` 持有，压缩管道只操作 messages |
//! | `PermissionMode`（Plan/AcceptEdits/Default） | 存 session 元数据（待 `SessionMeta` 扩展） |
//! | `ApprovalMode` × `SandboxPolicy` 预设 | 存 session 元数据（待 `SessionMeta` 扩展） |
//! | `allowed_prompts` 预批准缓存 | 存 session 元数据（待 `SessionMeta` 扩展） |
//! | 任务列表 | 存 session 元数据（待 `SessionMeta` 扩展） |
//!
//! 当前实现：`ContextManagerImpl` 持有 `system_prompt`，`StateKeep` 快照并断言
//! 压缩管道不篡改它。`PermissionMode`/`ApprovalMode`/`allowed_prompts` 等字段
//! 尚未在 `SessionMeta` 中定义（待后续里程碑扩展），`StateKeep` 的设计预留
//! 扩展点。

/// 压缩状态保留快照。
///
/// 在 `compress_pipeline` 前通过 [`StateKeep::snapshot`] 捕获跨压缩不可丢失的
/// 状态，压缩后通过 [`StateKeep::assert_unchanged`] 断言未被篡改。
///
/// 断言仅在 debug 构建生效（`debug_assert_eq!`），release 构建为空操作。
#[derive(Debug, Clone)]
pub struct StateKeep {
    /// 系统 prompt 快照（压缩管道不应触碰，见 §3.7）。
    system_prompt: String,
}

impl StateKeep {
    /// 快照当前跨压缩状态。
    ///
    /// `system_prompt` 是 `ContextManagerImpl` 持有的、压缩管道不应修改的状态
    /// （见 `docs/design.md` §3.7：系统 prompt 权重 1.0 不参与压缩，但需防
    /// L3 滚动窗口误删）。
    #[must_use]
    pub fn snapshot(system_prompt: &str) -> Self {
        Self {
            system_prompt: system_prompt.to_string(),
        }
    }

    /// 断言压缩后跨压缩状态未被篡改（debug 模式）。
    ///
    /// `system_prompt` 由 `ContextManagerImpl` 持有且压缩管道只操作 `messages`，
    /// 此断言是防御性检查：若压缩管道意外修改了 `system_prompt`，debug 构建会
    /// 立即 panic 暴露 bug（见 `AGENTS.md` §2.3：`debug_assert` 标记不变式）。
    ///
    /// release 构建中为空操作，零运行时开销。
    pub fn assert_unchanged(&self, current_system_prompt: &str) {
        debug_assert_eq!(
            self.system_prompt, current_system_prompt,
            "system_prompt 被压缩管道篡改（违反 §3.7 状态保留清单）"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_captures_system_prompt() {
        let keep = StateKeep::snapshot("you are an assistant");
        assert_eq!(keep.system_prompt, "you are an assistant");
    }

    #[test]
    fn assert_unchanged_passes_on_same_prompt() {
        let keep = StateKeep::snapshot("system prompt");
        // 不 panic 即通过
        keep.assert_unchanged("system prompt");
    }

    // debug_assert_eq! 仅在 debug 构建触发 panic，release 构建为空操作。
    // 此测试仅在 debug 构建运行（cargo test 默认 debug）。
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "system_prompt 被压缩管道篡改")]
    fn assert_unchanged_panics_on_different_prompt() {
        let keep = StateKeep::snapshot("original");
        keep.assert_unchanged("mutated");
    }
}
