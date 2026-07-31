//! Policy 模块 re-export。

mod r#trait;

pub use r#trait::{
    Decision, NoopPolicy, NoopPrompter, PermissionContext, PermissionPolicy, PermissionPrompt,
    PermissionPrompter, PromptOption, Risk, Verdict,
};
