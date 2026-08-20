//! # minicoding-core
//!
//! 抽象层 + `Runtime` 编排（零实现）。
//!
//! 仅含：数据模型、核心 trait 定义、`Runtime` 聚合根与 `Agent` 循环、事件总线、配置、
//! `OTel` 初始化与 span 辅助、路径约定。不含任何领域实现逻辑（压缩算法、黑名单正则、
//! landlock ruleset、rmcp 调用、`JSONL` 写入等）。
//!
//! 依赖方向：core 不依赖任何领域 crate；领域 crate 依赖 core。
//! 依赖约束：仅"轻量 + 无平台/网络"的依赖（见 `modules.md` §1.4、§15.6）。
//!
//! 详见 `docs/modules.md` §1、`docs/design.md` §1。

#![deny(clippy::all, clippy::pedantic)]

pub mod agent;
pub mod config;
pub mod context;
pub mod extension;
pub mod hooks;
pub mod journal;
pub mod mcp;
pub mod memory;
pub mod metrics;
pub mod model;
pub mod otel;
pub mod paths;
pub mod policy;
pub mod prompt;
pub mod provider;
pub mod runtime;
pub mod sandbox;
pub mod storage;
pub mod tool;
pub mod util;

/// 常用类型 re-export。
pub mod prelude {
    pub use crate::agent::{NoopSubagentRunner, SubagentRunner};
    pub use crate::config::{HookEntry, HooksConfig, RuntimeConfig};
    pub use crate::context::{ContextManager, ContextSnapshot};
    pub use crate::extension::{
        Capability, Extension, ExtensionCarrier, ExtensionHost, ExtensionId, ExtensionInfo,
        ExtensionManifest, KeyBinding, NoopExtensionHost, NoopRegistrar, Registrar, SlashCommand,
        StatusItem,
    };
    pub use crate::hooks::{
        AsyncRewakeSpec, DispatchConfig, DispatchResult, Hook, HookDecision, HookError, HookEvent,
        HookInput, HookMatcher, HookOutput, HookRegistry, NoopHookRegistry, OnHookError,
        VerdictSerde,
    };
    pub use crate::journal::{ChangeEntry, DiffEntry, FileChange, Journal, UndoReport};
    pub use crate::mcp::{McpClient, McpScope, McpServerConfig, McpTransport, ToolHint};
    pub use crate::memory::{MemoryStore, ProjectDocLoader};
    pub use crate::metrics;
    pub use crate::model::{
        Attachment, ContentBlock, ContextHint, ExtensionError, Isolation, JournalError, LlmError,
        McpError, MemoryError, MergeStrategy, Message, MessageMeta, MessageSource, PolicyError,
        PromptError, Role, RuntimeError, Session, SessionId, SessionMeta, SideEffect, StopReason,
        StorageError, SubagentResult, SubagentSpec, SubagentType, Task, TaskStatus, Thoroughness,
        ToolCall, ToolCallId, ToolContent, ToolError, ToolResult, ToolResultMeta, ToolSchema,
        TurnOutcome, UserInput, WorktreeSpec,
    };
    pub use crate::policy::{
        Decision, NoopPolicy, NoopPrompter, PermissionContext, PermissionMode, PermissionPolicy,
        PermissionPrompt, PermissionPrompter, PlanModeController, PlanModeSnapshot,
        PreApprovedPrompt, PromptOption, Risk, Verdict,
    };
    pub use crate::prompt::{
        GitInfo, MemoryBlock, Platform, ProjectDoc, PromptContext, PromptContributor,
        PromptPipeline, PromptSection, PromptSectionOrder, SystemPrompt,
    };
    pub use crate::provider::{
        BoxFuture, Capabilities, ChatRequest, Delta, GenerationParams, LlmProvider, Tokenizer,
        ToolCallDelta, Usage,
    };
    pub use crate::runtime::{Runtime, RuntimeBuilder};
    pub use crate::sandbox::{
        BreakerState, DenialMatch, DenialSignature, NoopDenialDetector, NoopDenialTracker,
        NoopDriver, SandboxDenialDetector, SandboxDenialTracker, SandboxDenyKind, SandboxDriver,
        SandboxError, SandboxPolicy, hard_trip_summary, soft_trip_reminder,
    };
    pub use crate::storage::{
        AuditKind, AuditRecord, AuditSink, EventRecord, EventStore, NoopAudit, NoopEventStore,
        NoopSnapshotStore, PersistedEvent, SCHEMA_VERSION, SNAPSHOT_INTERVAL, SessionSnapshot,
        SessionState, SnapshotStore, Storage, try_persist,
    };
    pub use crate::tool::{Tool, ToolContext, ToolGroup, ToolRegistry};
    pub use crate::util::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
}
