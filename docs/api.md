# API / 接口设计

本文定义 `minicoding-rs` 的核心 trait 签名、公共数据类型、配置 schema、SDK 高层 API，以及对外暴露的稳定接口面。所有签名以 Rust 2024 + `async fn in trait`（MSRV 1.99+）表达。

---

## 1. 接口分层

```
┌─────────────────────────────────────────┐
│  L3  SDK 高层 API (minicoding-sdk)      │  ask / ask_stream / run_task
├─────────────────────────────────────────┤
│  L2  Runtime API (minicoding-core)      │  Runtime::run_turn / spawn_subagent
├─────────────────────────────────────────┤
│  L1  Trait 抽象 (minicoding-core)       │  LlmProvider / Tool / Storage ...
└─────────────────────────────────────────┘
```

- **L1** 是可替换能力的契约，第三方可实现接入。
- **L2** 是运行时入口，frontend 调用。
- **L3** 是面向嵌入者的高层封装。

---

## 2. 核心数据类型（L0）

### 2.1 消息模型

```rust
pub type SessionId = String;
pub type ToolCallId = String;

pub enum Role { System, User, Assistant, Tool }

pub struct Message {
    pub id: String,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<ToolCallId>,   // role=Tool 时指向触发它的 call
    pub created_at: time::OffsetDateTime,
    pub metadata: MessageMeta,
}

pub enum ContentBlock {
    Text(String),
    Image { mime: String, data: Vec<u8> },     // base64 in transit
    ToolUse(ToolCall),
    ToolResult { call_id: ToolCallId, content: ToolContent, is_error: bool },
}

pub struct MessageMeta {
    pub tokens: Option<usize>,
    pub pinned: bool,
    pub summarized: bool,
    pub source: MessageSource,   // User / Llm / Tool / Subagent
}
```

### 2.2 工具模型

```rust
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    pub input: serde_json::Value,
}

pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,   // JSON Schema
}

pub enum ToolContent {
    Text(String),
    Json(serde_json::Value),
    Image { mime: String, data: Vec<u8> },
    Mixed(Vec<ToolContent>),
}

pub struct ToolResult {
    pub content: ToolContent,
    pub is_error: bool,
    pub metadata: ToolResultMeta,
}

pub struct ToolResultMeta {
    pub elapsed: Duration,
    pub bytes: usize,
    pub truncated: bool,
}
```

### 2.3 会话模型

```rust
pub struct Session {
    pub id: SessionId,
    pub created_at: time::OffsetDateTime,
    pub workdir: Utf8PathBuf,
    pub config_hash: u64,
    pub messages: Vec<Message>,           // 运行时镜像（与 storage 一致）
}

pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    Stopped,
    Interrupted,
}

pub enum TurnOutcome {
    Finished(Message),
    Interrupted(Message),
    Failed(RuntimeError),
}
```

### 2.4 安全与权限相关类型（参考 Codex/CC）

权限模型在 §3.6 的 `PermissionPolicy`/`PermissionPrompter` 之上，叠加 Codex 风格的宏观配置。本节给出与权限/沙箱相关的核心枚举，详细语义见 `security.md` §2.6/§8 与 `design.md` §9.5/§16。

```rust
/// OS 级沙箱策略（第二道防线，见 security.md §8.1）。
pub enum SandboxPolicy {
    /// 只读：仅允许读文件与白名单只读命令；禁止任何写/执行/网络。
    ReadOnly,
    /// 工作区写：允许工作区内读写与命令执行；禁止越界写、网络（默认）。
    WorkspaceWrite { workdir: Utf8PathBuf, writable: Vec<Utf8PathBuf> },
    /// 外部沙箱：本进程不做内核级隔离，假定外层容器/VM 已提供隔离（CI 场景，参考 Codex `external-sandbox`）。
    /// 仅应用层权限生效；`SandboxDriver::is_hardened()` 返回 false。
    ExternalSandbox,
    /// 完全访问：无限制（仅 full-access 预设，需显式确认）。
    DangerFullAccess,
}

/// 审批模式（决定"何时需要人工确认"，见 security.md §2.6）。
pub enum ApprovalMode {
    Untrusted,   // 仅信任只读命令；任何写/执行/网络都 Ask
    OnFailure,   // 命令自动执行，失败时才 Ask
    OnRequest,   // 由模型判断何时请求确认（默认）
    Never,       // 全自动，从不请求（仅与 DangerFullAccess 组合）
}

/// 权限模式（Plan 模式独立于 ApprovalMode，见 design.md §16.2）。
pub enum PermissionMode {
    Default,           // §9.3 默认矩阵（写 Ask）
    AcceptEdits,       // 文件写入自动 Allow，shell 仍 Ask
    Plan,              // 只读强制（硬门 + 软引导）
    Auto,              // 分类器自动批准（阶段 6+）
    BypassPermissions, // 全放行（仅隔离容器内）
}

/// 预设：approval_mode × sandbox_policy 的实用组合（见 security.md §2.6）。
pub struct Preset {
    pub name: PresetKind,
    pub approval_mode: ApprovalMode,
    pub sandbox_policy: SandboxPolicy,
}

pub enum PresetKind {
    ReadOnly,         // OnRequest + ReadOnly
    Auto,             // OnRequest + WorkspaceWrite（默认）
    ExternalSandbox,  // OnRequest + ExternalSandbox（CI/容器内）
    FullAccess,       // Never + DangerFullAccess
}
```

`PermissionPolicy::check`（§3.6）实现两层解析模型（详见 `design.md` §9.5）：L0 内置黑名单 `Deny`（不可覆盖）→ L1 用户策略（默认矩阵 / `ApprovalMode` / `policy.toml` / granular rules 在同一命名空间按 specificity 降序竞争，同 specificity 下 `deny` 胜出）。`PermissionMode::Plan` 作为 L0 扩展插入"非只读工具直接 `Deny`"硬门。

四种 `SandboxPolicy` 的内核隔离强度递减：`ReadOnly`/`WorkspaceWrite` 由 `minicoding-sandbox` 在子进程 `exec` 前应用内核级限制；`ExternalSandbox` 假定外层容器（Docker/Firecracker/CI runner）已隔离，本进程仅应用层校验，`SandboxDriver::is_hardened()` 返回 `false` 并打 `info` 日志声明依赖外部隔离；`DangerFullAccess` 关闭所有限制。`minicoding exec --sandbox external-sandbox` 适合在已隔离的 CI 环境中跑批量任务，避免双重沙箱开销与权限冲突。

### 2.5 子 Agent 与 Todo 相关类型

```rust
/// 类型化子 Agent（替代自由 role: String，见 design.md §7.2）。
pub enum SubagentType {
    Explore,          // 小模型，只读工具子集，跳过 AGENTS.md
    Plan,             // 只读，仅 Plan 模式可用
    GeneralPurpose,   // 继承父模型+全工具
    Custom(String),   // .minicoding/agents/*.md 加载
}

pub enum Thoroughness { Quick, Medium, VeryThorough }

pub struct SubagentSpec {
    pub ty: SubagentType,
    pub system_prompt: String,
    pub allowed_tools: ToolGroup,
    pub model: Option<String>,        // None = 继承父会话；Explore 强制小模型由 runner 解析
    pub budget_tokens: usize,
    pub max_iters: u32,
    pub thoroughness: Thoroughness,
    pub skip_memory: bool,
    pub can_spawn_subagent: bool,
    pub timeout: Duration,
}

pub struct SubagentResult {
    pub summary: String,              // 给父 Agent 的结论（C-05：仅 summary，不回灌中间消息）
    pub artifacts: Vec<String>,       // 子 Agent 改动的文件路径（仅路径）
    pub token_used: usize,
    pub completed: bool,              // false = 超时/取消/熔断
}

/// 子 Agent 派发器（dyn 兼容，见 design.md §7.3）。
/// `task.spawn` 工具持有 `Arc<dyn SubagentRunner>` 反向调用 Runtime 派发。
pub trait SubagentRunner: Send + Sync {
    fn spawn(&self, spec: SubagentSpec, input: String)
        -> BoxFuture<'_, Result<SubagentResult, RuntimeError>>;
}

/// 兜底实现（未注入时 `task.spawn` 直接返回 `RuntimeError::Config`）。
pub struct NoopSubagentRunner;
```

`SubagentSpec::default_for(ty)` 按类型给出默认配置（`Explore`/`Plan` 跳过 AGENTS.md、`max_iters` 与 `timeout` 按 `design.md` §7.2 表格）。`RuntimeBuilder::subagent_runner(r)` 注入实现；`Runtime::subagent_runner()` 返回 `Arc<dyn SubagentRunner>` 供 `task.spawn` 工具持有（与 `plan.exit` 持有 `Arc<dyn PlanModeController>` 同构）。

```rust
/// 任务管理工具的数据类型（见 design.md §18.3）。`task.create`/`update`/`list` 三件套。
/// 旧版 `TodoWriteInput`（全量替换）作为废弃别名保留一个版本（见 §10.1）。
pub struct TaskCreateInput {
    pub subject: String,
    pub description: Option<String>,
    pub active_form: Option<String>,
    pub priority: Option<Priority>,
    pub metadata: Option<serde_json::Value>,
}
pub struct TaskUpdateInput {
    pub task_id: String,
    pub status: Option<TaskStatus>,
    pub subject: Option<String>,
    pub description: Option<String>,
    pub active_form: Option<String>,
    pub add_blocks: Option<Vec<String>>,
    pub add_blocked_by: Option<Vec<String>>,
    pub owner: Option<String>,
    pub metadata: Option<serde_json::Value>,
}
pub struct TaskListInput { pub status_filter: Option<TaskStatus> }

pub struct Todo {
    pub id: String,
    pub text: String,
    pub status: TodoStatus,
    pub priority: Option<Priority>,
    pub summary: Option<String>,
}

pub enum TodoStatus { Pending, InProgress, Completed, Deleted }
pub enum Priority { High, Medium, Low }
```

### 2.6 文件改动事务类型（见 design.md §17）

```rust
pub struct FileChangeJournal { entries: Vec<ChangeEntry> }

pub struct ChangeEntry {
    pub op_id: OpId,
    pub ts: time::OffsetDateTime,
    pub prompt_snippet: String,
    pub files: Vec<FileChange>,
}

pub enum FileChange {
    Written { path: Utf8PathBuf, before: Option<Vec<u8>>, after: Vec<u8> },
    Edited  { path: Utf8PathBuf, before: Vec<u8>, after: Vec<u8> },
    Deleted { path: Utf8PathBuf, content: Vec<u8> },
    Created { path: Utf8PathBuf, content: Vec<u8> },
}

pub struct UndoReport {
    pub undone_entries: usize,
    pub restored_files: Vec<Utf8PathBuf>,
    pub failed_files: Vec<(Utf8PathBuf, JournalError)>,
}
```

### 2.7 MCP 相关类型（见 design.md §19）

```rust
pub struct McpServerConfig {
    pub transport: McpTransport,
    pub startup_timeout: Duration,
    pub tool_timeout: Duration,
    pub enabled: bool,
    pub required: bool,
    pub enabled_tools: Option<Vec<String>>,
}

pub enum McpTransport {
    Stdio { command: String, args: Vec<String>, env: HashMap<String, String>, env_vars: Vec<String>, cwd: Option<Utf8PathBuf> },
    Http { url: String, bearer_token_env_var: Option<String>, headers: HashMap<String, String> },
}

pub enum McpScope { Local, Project, User }

/// MCP 工具命名（见 design.md §19.3）。
pub fn mcp_tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}
```

---

## 3. L1 Trait 抽象

> **dyn-compatibility 约定**：本节所有含 `async fn` 的 trait 都需要以 `Arc<dyn Trait>` 形式被 Runtime 持有，而原生 `async fn in trait` 默认非 dyn-compatible。统一使用 `trait_variant::make(Trait: Send)` 宏为每个 trait 生成 `Send` 变体（返回 `Pin<Box<dyn Future + Send>>`），从而既保留原生 async 语法、又支持 trait object。下文签名以原生 `async fn` 表达语义，实际定义处的宏标注见各小节首行。同步 trait（如 `Tokenizer`）无需此处理。

### 3.1 `LlmProvider`

```rust
#[trait_variant::make(LlmProvider: Send)]
pub trait LlmProvider {
    fn id(&self) -> &str;
    fn capabilities(&self) -> Capabilities;
    fn tokenizer(&self) -> Arc<dyn Tokenizer>;

    /// 流式对话。返回的 stream 必须可被取消（drop 即取消）。
    async fn chat_stream(
        &self,
        req: ChatRequest,
    ) -> Result<BoxStream<'static, Result<Delta, LlmError>>, LlmError>;

    /// 非流式便捷封装（默认基于 stream 聚合）。
    async fn chat(&self, req: ChatRequest) -> Result<Message, LlmError> { /* default */ }

    async fn count_tokens(&self, messages: &[Message]) -> usize;
}

pub struct ChatRequest {
    pub system: SystemPrompt,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub params: GenerationParams,
}

pub struct GenerationParams {
    pub model: String,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<usize>,
    pub stop: Vec<String>,
    pub seed: Option<u64>,
}

pub enum Delta {
    Text(String),
    ToolCall(ToolCallDelta),
    Usage(Usage),
    Stop(StopReason),
}

pub struct ToolCallDelta {
    pub index: u32,
    pub id: Option<String>,
    pub name: Option<String>,
    pub args_chunk: Option<String>,    // 增量 JSON 片段
}

pub struct Usage {
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cache_read: Option<usize>,
    pub cache_write: Option<usize>,
}

pub struct Capabilities {
    pub supports_tool_call: bool,
    pub supports_vision: bool,
    pub supports_streaming: bool,
    pub supports_json_mode: bool,
    pub context_window: usize,
    pub max_output: usize,
}
```

### 3.2 `Tokenizer`

```rust
pub trait Tokenizer: Send + Sync {
    fn count(&self, text: &str) -> usize;
    fn count_messages(&self, msgs: &[Message]) -> usize;
    fn id(&self) -> &str;   // "cl100k" / "claude-3" / ...
}
```

### 3.3 `Tool`

```rust
#[trait_variant::make(Tool: Send)]
pub trait Tool {
    fn name(&self) -> &str;
    fn schema(&self) -> &ToolSchema;
    fn side_effect(&self) -> SideEffect;

    /// 是否只读（用于 Plan 模式硬门，见 design.md §16.1）。
    /// 默认实现：`self.side_effect() == SideEffect::None`。
    /// MCP 工具根据 server schema 的 `readOnlyHint` 覆盖。
    fn is_read_only(&self) -> bool { self.side_effect() == SideEffect::None }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError>;
}

pub enum SideEffect { None, FileWrite, Command, Network }

pub struct ToolContext {
    pub workdir: Utf8PathBuf,
    pub session_id: SessionId,
    pub canceller: CancellationToken,
    pub env: HashMap<String, String>,
    pub timeout: Duration,
    pub max_output_bytes: usize,
}
```

`is_read_only()` 默认基于 `side_effect()`，但拆为独立方法是因为 MCP 工具的只读性由 server schema 声明（`readOnlyHint`），可能与本地 `side_effect` 推断不一致。Plan 模式硬门用 `is_read_only()` 而非 `side_effect()` 判断，给 MCP 工具留出"声明只读即可在 Plan 模式下用"的通道。

### 3.4 `ToolRegistry`

```rust
pub struct ToolRegistry { /* opaque */ }

impl ToolRegistry {
    pub fn new() -> Self;
    pub fn register(&mut self, tool: Arc<dyn Tool>);
    pub fn enable_group(&mut self, group: ToolGroup);
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>>;
    pub fn schemas(&self) -> Vec<&ToolSchema>;       // 仅 enabled
    pub async fn dispatch(&self, call: &ToolCall, ctx: &ToolContext)
        -> Result<ToolResult, ToolError>;
}

pub enum ToolGroup { Core, Fs, Shell, Web, Git, Task, Plan, Mcp }
```

新增 `Task`/`Plan`/`Mcp` 三个工具组：`Task` 含 `task.create`/`task.update`/`task.list`（旧版 `todo.write` 作为废弃别名）；`Plan` 含 `plan.exit`；`Mcp` 是动态注册的外部 MCP 工具集合。

### 3.5 `Storage`

```rust
#[trait_variant::make(Storage: Send)]
pub trait Storage {
    async fn append(&self, session: &SessionId, msg: &Message) -> Result<(), StorageError>;
    async fn load(&self, session: &SessionId) -> Result<Vec<Message>, StorageError>;
    async fn list_sessions(&self) -> Result<Vec<SessionMeta>, StorageError>;
    async fn delete(&self, session: &SessionId) -> Result<(), StorageError>;
}

pub struct SessionMeta {
    pub id: SessionId,
    pub created_at: time::OffsetDateTime,
    pub message_count: usize,
    pub last_message_at: time::OffsetDateTime,
}
```

`JsonlStorage` 实现 `Storage` trait，并扩展以下能力（见 `features.md` S-02/S-03/S-04）：

```rust
// 会话索引（index.json）：轻量元数据列出，无需逐个打开 .jsonl
pub struct SessionIndex { /* Vec<SessionIndexEntry> */ }
pub struct SessionIndexEntry {
    pub session_id: String,
    pub summary: Option<String>,
    pub message_count: usize,
    pub created_at: String,   // RFC3339
    pub updated_at: String,   // RFC3339
    pub parent_uuid: Option<String>,
}

impl SessionIndex {
    pub fn load(path: &Utf8Path) -> Result<Self, StorageError>;
    pub fn save(&self, path: &Utf8Path) -> Result<(), StorageError>;  // 原子写：.tmp + rename
    pub fn add(&mut self, entry: SessionIndexEntry);
    pub fn remove(&mut self, session_id: &str);
    pub fn list(&self) -> &[SessionIndexEntry];
    pub fn list_windowed(&self) -> String;  // 64KB 窗口：首尾各 32KB（C-07）
}

// 跨进程文件锁（fs2 排他锁）：同会话 --resume 互斥
pub struct SessionLock { /* RAII 守卫 */ }
impl SessionLock {
    pub fn acquire(path: impl Into<Utf8PathBuf>) -> Result<Self, StorageError>;
    pub fn release(self);  // Drop 自动释放
}

// 会话导出（C-04：凭证由工具层保证不入消息，导出层不额外过滤）
pub enum ExportFormat { Markdown, Jsonl }
pub fn export_session_md(messages: &[Message], meta: &SessionMeta) -> String;
pub fn export_session_jsonl(messages: &[Message]) -> String;

impl JsonlStorage {
    pub async fn export(&self, id: &SessionId, format: ExportFormat) -> Result<String, StorageError>;
}
```

`StorageError` 新增 `Locked(String)` 变体，表示会话被跨进程文件锁占用（见 `rules.md` C-22）。

### 3.6 权限：`PermissionPolicy` + `PermissionPrompter`

> **架构说明（修复 broadcast/oneshot 冲突）**：权限交互是"请求-响应"的点对点语义，而 `EventBus` 是"广播-订阅"语义。二者不能复用同一通道——`broadcast::Sender` 会克隆事件，而 `oneshot::Sender<Decision>` 不可克隆，强行放入 `Event` 既无法编译也语义错误。
>
> 因此把权限拆成两个正交抽象：
> - **`PermissionPolicy`**：纯决策逻辑，输入 `(tool, input, ctx)`，输出 `Verdict`（`Allow` / `Deny` / `Ask(prompt)`）。无副作用、不交互、可单元测试。
> - **`PermissionPrompter`**：点对点交互器，仅当 `Verdict::Ask` 时被 Runtime 调用，输入 `PermissionPrompt`，异步返回最终 `Decision`。由 frontend 注入（CLI / TUI / SDK 各有实现）。
>
> `EventBus` 只广播**通知类**事件（`PermissionRequested` / `PermissionResolved`），这些事件全部由可克隆数据组成，不携带任何 `Sender`，从而与 `broadcast` 兼容。

```rust
/// 策略返回的中间判定（未交互）。
pub enum Verdict {
    Allow,
    Deny(String),
    Ask(PermissionPrompt),
}

/// 交互后的最终决策（不再含 Ask）。
pub enum Decision {
    Allow,
    Deny(String),
}

/// 纯决策 trait（无交互、无 IO）。
#[trait_variant::make(PermissionPolicy: Send)]
pub trait PermissionPolicy {
    async fn check(
        &self,
        tool: &str,
        input: &serde_json::Value,
        ctx: &PermissionContext,
    ) -> Result<Verdict, PolicyError>;
}

pub struct PermissionContext {
    pub session: SessionId,
    pub workdir: Utf8PathBuf,
    pub side_effect: SideEffect,
    pub turn: u32,
    pub history: Vec<Decision>,   // 本会话已有决策，便于去重询问
}

/// 点对点交互器（非广播）。由 frontend 注入实现。
#[trait_variant::make(PermissionPrompter: Send)]
pub trait PermissionPrompter {
    async fn prompt(&self, req: PermissionPrompt) -> Decision;
}

pub struct PermissionPrompt {
    pub id: String,                    // ULID，关联 Requested/Resolved 事件
    pub tool: String,
    pub summary: String,               // 人类可读摘要
    pub risk: Risk,
    pub options: Vec<PromptOption>,    // [AllowOnce, AllowAlways, DenyOnce, DenyAlways]
}

pub enum Risk { Low, Medium, High }
pub enum PromptOption { AllowOnce, AllowAlways, DenyOnce, DenyAlways }
```

`PermissionPrompter` 的内置实现：

| 实现 | 适用场景 | 行为 |
|------|---------|------|
| `InteractivePrompter` | CLI TTY | 打印摘要 → 读 stdin → 解析选项；超时按 deny |
| `NonInteractivePrompter` | 非 TTY / CI / 管道 | 按 `permission.non_tty_strategy` 配置：`deny`（默认）/ `allow` / `fail`；`deny` 时回灌"非交互环境拒绝" |
| `TuiPrompter` | TUI | 渲染弹窗，阻塞该工具调用直至用户选择 |
| `CallbackPrompter` | SDK | 调用用户注册的异步闭包 |

Runtime 的权限解析流程见 `design.md` §9.2。

### 3.7 `ContextManager`

```rust
#[trait_variant::make(ContextManager: Send)]
pub trait ContextManager {
    async fn append(&self, msg: Message);
    async fn build_chat_request(
        &self,
        tools: &ToolRegistry,
        config: &RuntimeConfig,
    ) -> Result<ChatRequest, ContextError>;
    async fn snapshot(&self) -> ContextSnapshot;
    async fn restore(&self, snap: ContextSnapshot);
    fn token_count(&self) -> usize;
    fn message_count(&self) -> usize;
}

pub struct ContextSnapshot {
    pub messages: Vec<Message>,
    pub token_count: usize,
    pub compression_log: Vec<CompressionStep>,
}
```

`ContextManagerImpl`（`minicoding-context`）是 `ContextManager` 的完整实现，持有
`Tokenizer`、`TokenBudget`、可选的 `LlmProvider`（供 L2 摘要使用）。`new()` 接收
`provider: Option<Arc<dyn LlmProvider>>`，为 `None` 时跳过 L2 摘要（L1→L3→L4 仍执行）。

#### 4 级压缩管道（T-M3-2，见 `design.md` §3.3）

当 `token_count > budget.compact_threshold()`（usable × 0.85）时触发压缩管道，
逐级尝试 L1→L2→L3→L4，每级后检查 token 是否降到阈值以下，降了则提前返回
（C-29：降级链顺序不可跳）。

```rust
pub async fn compress_pipeline(
    messages: &mut Vec<Message>,
    tokenizer: &dyn Tokenizer,
    budget: &TokenBudget,
    provider: Option<&dyn LlmProvider>,
) -> Result<CompressResult, RuntimeError>;

pub struct CompressResult {
    pub clipped_count: usize,      // L1 裁剪的 tool_result 块数
    pub summarized_count: usize,   // L2 摘要替换的消息数
    pub dropped_count: usize,      // L3 滚动窗口丢弃的消息数
    pub truncated_count: usize,    // L4 硬截断丢弃的消息数
}
```

各级职责：L1 裁剪超阈值的 `ToolResult` 文本（前 K 行 + ... + 后 K 行，C-05 保留边界）；
L2 对权重最低的 N 条非 system 消息调 LLM 生成摘要替换原文（`[summarized @ ts]`）；
L3 仅保留最近 W 条非 system 消息 + 全部 system 消息；L4 按 token 数从尾部保留兜底。

配置 `[context] compress = false` 可关闭压缩直通（C-18 软约束，C-06 兜底）。

### 3.8 `Hook` 与 `HookRegistry`（见 `hooks.md` §5）

Hook 是"工具调用生命周期"的拦截器，介于"LLM 决定调用工具"与"工具真正执行"之间；也是会话/轮次/压缩等关键节点的观察+注入点。完整协议（JSON stdio）、10 类事件、配置见 `hooks.md`，此处仅给出 Rust trait。

```rust
#[trait_variant::make(Hook: Send)]
pub trait Hook {
    fn name(&self) -> &str;
    fn matcher(&self) -> &HookMatcher;
    async fn run(&self, input: HookInput) -> Result<HookOutput, HookError>;
}

pub struct HookMatcher {
    pub events: Vec<HookEvent>,
    pub tools: Option<Vec<String>>,    // None=所有工具；仅 PreToolUse/PostToolUse/PostToolUseFailure/PermissionRequest 有效
}

/// 10 类事件（见 `hooks.md` §2）：7 类纯同步 + 3 类同步/异步可选（PostToolUse/PostToolUseFailure/Stop）。
pub enum HookEvent {
    SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, PostToolUseFailure,
    PreCompact, PostCompact, Stop, SubagentStop, PermissionRequest,
}

pub struct HookInput {
    pub event: HookEvent,
    pub session_id: SessionId,
    pub turn: u32,
    pub tool: Option<ToolCall>,
    pub side_effect: Option<SideEffect>,
    pub verdict: Option<Verdict>,
    pub cwd: Utf8PathBuf,
    pub extras: serde_json::Value,     // 事件特有字段
}

pub struct HookOutput {
    pub decision: HookDecision,
    pub reason: Option<String>,
    pub modify_input: Option<serde_json::Value>,
    pub inject_context: Option<String>,
    pub exit_message: Option<String>,
    /// 异步唤醒（仅 PostToolUse/PostToolUseFailure/Stop 有效，见 `hooks.md` §11）
    pub async_rewake: Option<AsyncRewakeSpec>,
}

pub struct AsyncRewakeSpec {
    pub task_id: String,
    pub estimated_duration: Duration,
    pub wake_prompt: String,
}

pub enum HookDecision { Allow, Deny, Ask, Continue }

pub struct HookRegistry { by_event: HashMap<HookEvent, Vec<Arc<dyn Hook>>> }

impl HookRegistry {
    pub fn register(&mut self, hook: Arc<dyn Hook>);
    pub fn for_event(&self, e: HookEvent) -> &[Arc<dyn Hook>];
}
```

`ScriptHook` 适配器把外部可执行包装为 `Hook`：序列化 `HookInput`→stdin，读 stdout JSON→`HookOutput`，按退出码映射。**关键约束**：Hook 的 `allow` 对内置黑名单 `Deny` 无效（L0 不可覆盖，见 `rules.md` C-02 与 `hooks.md` §4）。`async_rewake` 仅对 `PostToolUse`/`PostToolUseFailure`/`Stop` 三类"事后"事件有效（见 `hooks.md` §11.3），同一 session 最多 3 个并发（C-26）。

### 3.9 `SandboxDriver`（OS 级沙箱，见 `security.md` §8）

OS 级沙箱是应用层权限（§3.6）之外的第二道防线。`SandboxDriver` 抽象平台差异，Runtime 在派发 `Command`/`Network` 类工具前应用策略。

```rust
#[trait_variant::make(SandboxDriver: Send)]
pub trait SandboxDriver {
    /// 在子进程 exec 前应用沙箱策略（pre-main hardening，见 security.md §8.3）。
    fn apply(&self, policy: &SandboxPolicy, cmd: &mut std::process::Command) -> Result<(), SandboxError>;

    /// 当前平台是否原生支持硬隔离（用于 doctor 自检与降级提示）。
    fn is_hardened(&self) -> bool;

    /// 平台名（"seatbelt" / "landlock+seccomp" / "windows-acl" / "none"）。
    fn id(&self) -> &'static str;
}
```

内置实现：

| 实现 | 平台 | 技术 |
|------|------|------|
| `SeatbeltDriver` | macOS 12+ | `sandbox-run`（封装原生 sandbox 框架），`apply_sandbox` 在子进程 pre-exec 调用，不手写 profile |
| `LandlockDriver` | Linux 5.13+ | `sandbox-run`（封装 `landlock` crate）+ `libseccomp`（seccomp 白名单 syscall），不手写 ruleset 胶水 |
| `WindowsSandboxDriver` | Windows | 受限令牌 + Job Object + DACL（初期可能降级） |
| `NoopDriver` | 兜底 | 不强制，仅应用层（启动时 warn） |

`.git`/`.hg`/`.svn` VCS 目录在所有写策略下默认强制只读（防破坏版本库元数据），需 `tools.sandbox.allow_vcs_write = true`（旧名 `allow_dotgit_write`，向后兼容）显式放开。详见 `security.md` §8.2。

### 3.10 `ProjectDocLoader`（AGENTS.md 分层加载，见 `design.md` §8.6）

```rust
#[trait_variant::make(ProjectDocLoader: Send)]
pub trait ProjectDocLoader {
    /// 加载并拼接 AGENTS.md 指令层（全局 + repo_root→cwd 逐级 + override + fallback）。
    /// 返回拼接后的字符串，超过 max_bytes 静默截断。
    async fn load(&self, ctx: &LoadContext) -> Result<String, LoadError>;
}

pub struct LoadContext {
    pub home: Utf8PathBuf,                  // $MINICODING_HOME
    pub repo_root: Option<Utf8PathBuf>,     // None = 不在 git 仓库
    pub cwd: Utf8PathBuf,
    pub fallback_filenames: Vec<String>,    // ["CLAUDE.md", ".cursorrules"]
    pub max_bytes: usize,                   // 默认 32 * 1024
}
```

加载算法：全局层取 `AGENTS.override.md`→`AGENTS.md` 首个非空；项目层从 `repo_root` 逐级到 `cwd`，每级取 `override→md→fallback` 之首；root→leaf 拼接，截断 32KiB。AGENTS.md 不可被 Agent 自主编辑（`fs.write`/`fs.edit` 对其默认 `Ask`）。

### 3.11 `Journal`（文件改动事务，见 `design.md` §17）

```rust
#[trait_variant::make(Journal: Send)]
pub trait Journal {
    /// 记录一次 turn 的文件改动（fs.write/edit/delete 成功后调用）。
    async fn record(&self, entry: ChangeEntry) -> Result<(), JournalError>;

    /// 撤销最近 steps 次 turn 的文件改动（/undo），含冲突检测。
    async fn undo(&self, steps: usize) -> Result<UndoReport, JournalError>;

    /// 列出会话内所有文件变更（/diff）。
    async fn diff(&self) -> Result<Vec<DiffEntry>, JournalError>;

    /// 回到会话启动时状态（/new），清空 journal + 重建初始快照。
    async fn reset_to_initial(&self) -> Result<(), JournalError>;
}

pub struct DiffEntry {
    pub op_id: OpId,
    pub prompt_snippet: String,
    pub files: Vec<FileChange>,
}
```

`/undo` 是特性门控（`[features] file_undo = false`，默认关，参考 Codex `features.undo`），仅会话内有效，会话结束销毁（不落盘，避免敏感数据多份存储）。跨会话回滚依赖 Git。

### 3.12 Extension 系统（见 `design.md` §23、`modules.md` §17）

Extension 系统为第三方扩展作者提供稳定 API。下列 trait 定义在 `minicoding-core`，SDK 实现位于 `minicoding-extension-sdk`。扩展通过 `Registrar` 注册工具/Hook/prompt contributor 等能力，扩展注册的工具仍统一走 `ToolRegistry` dispatch，确保权限审计一致（C-01/C-02 不被绕过）。

异步方法用 `BoxFuture` 返回类型保证 `dyn` 兼容（`async fn in trait` 的 `dyn` 兼容需 boxed future）。

```rust
/// 扩展宿主：管理扩展生命周期（Runtime 注入）。
pub trait ExtensionHost: Send + Sync {
    /// 加载扩展（读 manifest，初始化，注册能力）。
    fn load_extension(&self, manifest: ExtensionManifest) -> BoxFuture<'_, Result<ExtensionId, ExtensionError>>;
    /// 卸载扩展（调用 shutdown，注销所有注册项）。
    fn unload_extension(&self, id: &ExtensionId) -> BoxFuture<'_, Result<(), ExtensionError>>;
    /// 列出已加载扩展。
    fn list_extensions(&self) -> BoxFuture<'_, Vec<ExtensionInfo>>;
    /// 配置变更通知（热重载，按扩展 id 投递）。
    fn on_config_changed(&self, id: &ExtensionId, new_config: serde_json::Value) -> BoxFuture<'_, Result<(), ExtensionError>>;
}

/// 扩展 trait（扩展作者实现）
pub trait Extension: Send + Sync {
    /// 扩展元信息（供 Runtime 查询，无需扩展自行管理状态）。
    fn manifest(&self) -> &ExtensionManifest;
    /// 初始化：通过 Registrar 注册能力，config 为扩展配置（JSON）。
    fn init(&self, registrar: &mut dyn Registrar, config: serde_json::Value) -> BoxFuture<'_, Result<(), ExtensionError>>;
    /// 关闭：释放资源
    fn shutdown(&self) -> BoxFuture<'_, Result<(), ExtensionError>>;
    /// 配置变更通知（可选，默认空实现）
    fn on_config_changed(&self, _new_config: serde_json::Value) -> BoxFuture<'_, Result<(), ExtensionError>> {
        Box::pin(async move { Ok(()) })
    }
}

/// 注册器：扩展通过此接口注册能力（6 类注册项）。
/// 注册项统一用 `Arc<dyn Trait>`，便于 Runtime 在多扩展间共享实例。
pub trait Registrar {
    fn register_tool(&mut self, tool: Arc<dyn Tool>) -> Result<(), ExtensionError>;
    fn register_hook(&mut self, hook: Arc<dyn Hook>) -> Result<(), ExtensionError>;
    fn register_prompt_contributor(&mut self, contributor: Arc<dyn PromptContributor>) -> Result<(), ExtensionError>;
    fn register_keybinding(&mut self, kb: KeyBinding) -> Result<(), ExtensionError>;
    fn register_status_item(&mut self, item: StatusItem) -> Result<(), ExtensionError>;
    fn register_command(&mut self, cmd: SlashCommand) -> Result<(), ExtensionError>;
}

/// 扩展清单
pub struct ExtensionManifest {
    pub id: String,                  // 全局唯一，如 "minicoding-git-stats"
    pub version: semver::Version,
    pub name: String,
    pub author: Option<String>,
    pub carrier: ExtensionCarrier,   // Bundled / Ipc / Mcp
    pub capabilities: Vec<Capability>,
    pub permissions: Vec<Permission>,
    pub config_schema: Option<serde_json::Value>,  // JSON Schema for config
}

/// 扩展载体（三类统一抽象）
pub enum ExtensionCarrier {
    Bundled,                        // 进程内 first-party，name 查找符号
    Ipc { path: Utf8PathBuf },      // disk IPC 子进程（可执行文件路径）
    Mcp { server_id: String },      // 复用 §19 MCP server
}

/// 扩展能力声明（与 Registrar 6 个方法一一对应）
pub enum Capability { Tool, Hook, PromptContributor, Keybinding, StatusItem, Command }
```

**实现**：

- `NoopExtensionHost`（`minicoding-core::extension`）：默认兜底，未启用扩展时使用。`load_extension` 恒返回 `NotFound`，`list_extensions` 返回空列表。`RuntimeBuilder::build` 默认注入此实现。
- `BundledExtensionHost`（`minicoding-extension-sdk::bundled`）：进程内 first-party 扩展宿主，实际持有 `Arc<dyn Extension>` 并调用 `init`/`shutdown`。`load_extension` 后通过 `take_bundle` 提取 `RegistrationBundle`（tools/hooks/contributors 等），由调用方提交到 Runtime 各注册表。
- `NoopRegistrar`（`minicoding-core::extension`）：`Registrar` 的 noop 实现，注册项全部丢弃仅做 `Capability` 声明校验，用于测试。
- `BundleRegistrar`（`minicoding-extension-sdk::registrar`）：`Registrar` 的生产实现，收集注册项到 `RegistrationBundle` 供 `BundledExtensionHost` 提取。

`Extension` trait 由扩展作者实现，`init` 阶段通过 `Registrar` 把自身能力注册进 Runtime，`manifest()` 让 Runtime 无需维护独立的扩展元信息表。扩展注册的工具仍走 `ToolRegistry::dispatch`，因此权限检查（C-01）与内置黑名单（C-02）对扩展工具同样生效——扩展无法绕过权限审计（见 `design.md` §23 安全约束）。

**Runtime 集成**：`RuntimeBuilder::extension_host(Arc<dyn ExtensionHost>)` 注入宿主，`Runtime::extension_host()` 返回 `Arc<dyn ExtensionHost>` 供 frontend 调用。CLI 在 `extensions` feature 启用时注入 `BundledExtensionHost`（见 `crates/minicoding-cli/src/builder.rs`）。

### 3.13 Prompt 管道（见 `design.md` §22）

Prompt 管道把 system prompt 的组装拆为 9 个 `PromptContributor`，按固定顺序拼接。稳定段（1-5：身份/系统规则/任务指南/通信规范/环境信息）在前，易变段（6-9：用户规则/项目规则/工具摘要/扩展注入）在后，使稳定段命中 prompt cache，降低重复请求的计费。扩展通过 `Registrar::register_prompt_contributor`（§3.12）注册的 contributor 注入 `Extension` section（顺序 9）。

```rust
/// Prompt contributor：为 system prompt 组装贡献一个 section。
pub trait PromptContributor: Send + Sync {
    /// contributor 名称（用于调试与 OTel span）
    fn name(&self) -> &str;
    /// 拼装顺序（稳定段在前，利于 prompt cache）
    fn order(&self) -> PromptSectionOrder;
    /// 是否可缓存（内容不变的 contributor 返回 true）
    fn cacheable(&self) -> bool { false }
    /// 组装 section（异步方法用 BoxFuture 保证 dyn 兼容）
    fn build(&self, ctx: &PromptContext) -> BoxFuture<'_, Result<PromptSection, PromptError>>;
}

/// Prompt section 数据结构（与 `design.md` §22 一致）
pub struct PromptSection {
    pub contributor_name: String,
    pub content: String,
    pub order: PromptSectionOrder,
    pub cacheable: bool,
    pub boundary: Option<&'static str>,  // 如 "project_doc"、"auto_memory"，包裹边界
}

/// Section 排序（稳定→易变）
pub enum PromptSectionOrder {
    Identity = 1,       // 1. 身份（~/.minicoding/IDENTITY.md 覆盖默认身份，P-31）
    System = 2,         // 2. 系统规则
    TaskGuidelines = 3, // 3. 任务指南
    Communication = 4,  // 4. 通信规范
    Environment = 5,    // 5. 工作区/平台/git 信息
    UserRules = 6,      // 6. 用户规则（long_term memory）
    ProjectRules = 7,   // 7. 项目规则（AGENTS.md）
    ToolSummary = 8,    // 8. 工具 schema 摘要
    Extension = 9,      // 9. 扩展注入
}
```

9 个 contributor 按 `PromptSectionOrder` 枚举值顺序拼接，稳定段（1-5）内容相对恒定可命中 prompt cache，易变段（6-9）放后。扩展通过 `Registrar::register_prompt_contributor` 注册的 contributor 注入到 `Extension` 段（顺序 9），与内置 `ExtensionContributor` 共存（同 order 内按 name 排序）。

**实现**：9 个内置 contributor 由 `minicoding_extension_sdk::builtin_contributors(identity_content)` 构造，位于 `minicoding-extension-sdk/src/contributors/`：

| 顺序 | Contributor | cacheable | 内容来源 |
|:---:|------|:---:|------|
| 1 | `IdentityContributor` | true | 默认身份或 `~/.minicoding/IDENTITY.md`（P-31） |
| 2 | `SystemContributor` | true | 内置 `rules.md` §5 软规则 |
| 3 | `TaskGuidelinesContributor` | true | 多步任务规划、工具使用规范 |
| 4 | `CommunicationContributor` | true | 输出格式、语言偏好 |
| 5 | `EnvironmentContributor` | true | 工作区/平台/git 信息 |
| 6 | `UserRulesContributor` | false | `long_term.md`（包裹 `<long_term_memory>` 边界） |
| 7 | `ProjectRulesContributor` | false | AGENTS.md 分层加载（包裹 `<project_doc>` 边界） |
| 8 | `ToolSummaryContributor` | false | `enabled_tools` schema 摘要 |
| 9 | `ExtensionContributor` | false | 占位（扩展注册的 contributor 注入到此段） |

`PromptContext`（`build()` 的入参）聚合各 contributor 需要的会话级输入（`session_id`/`workdir`/`platform`/`git_info`/`enabled_tools`/`user_rules`/`project_rules`），避免 contributor 各自重新加载文件。完整定义与 `PromptPipeline` 拼装算法见 `design.md` §22。

**Runtime 集成**：`ContextManagerImpl::with_prompt_pipeline(Arc<PromptPipeline>, PromptContext)` 注入管道后，`build_chat_request` 动态构建 system prompt（替代静态 `system_prompt`）。未注入时走静态 `system_prompt`（向后兼容）。管道构建错误通过 `RuntimeError::Prompt(#[from] PromptError)` 变体传播。CLI 在 `extensions` feature 启用时注入管道并加载 `long_term.md`/`AGENTS.md`/`IDENTITY.md` 内容（见 `crates/minicoding-cli/src/builder.rs`）。

---

## 4. L2 Runtime API

### 4.1 `Runtime` 与 Builder

```rust
pub struct Runtime { /* opaque */ }

impl Runtime {
    pub fn builder() -> RuntimeBuilder;

    /// 单轮对话；驱动 Agent 循环直到结束或中断。
    pub async fn run_turn(&self, input: UserInput) -> Result<TurnOutcome>;

    /// `run_turn` 的 `'static` 变体：取 `Arc<Runtime>` owned，返回 `BoxFuture<'static>`。
    ///
    /// **设计原因**：`run_turn(&self)` 的 future 借用 `&self`，当被 server handler
    /// （axum `Handler` trait 要求 `'static` + HRTB）`.await` 时，生命周期参数泄漏到
    /// 外层 future 类型，编译器报 "implementation of `FnOnce` is not general enough"。
    /// `self: Arc<Self>` owned 移入 `async move` 块，`run_turn(&*self)` 的借用是块内
    /// 局部借用，await 后即释放；`Box::pin` 装箱为 `BoxFuture<'static>`。
    /// 详见 `design.md` §24 HRTB 设计说明。
    pub fn run_turn_owned(
        self: Arc<Self>,
        input: UserInput,
    ) -> BoxFuture<'static, Result<TurnOutcome, RuntimeError>>;

    /// 触发 graceful 取消（CLI 的 Ctrl-C handler 调用）。
    /// 当前 in-flight 迭代被丢弃，已落盘消息保留（C-13），run_turn 返回 Interrupted。
    pub fn cancel(&self);

    /// 返回取消 token 克隆（供 frontend 在 select! 中组合等待 Ctrl-C）。
    pub fn cancel_token(&self) -> CancellationToken;

    /// 恢复会话历史到上下文管理器（`--resume`/`--fork-session` 用，T-M3-10a）。
    /// 将预加载会话的消息逐条注入 `ContextManager`，不重复落盘（历史已在磁盘）。
    /// 调用方在 `RuntimeBuilder::session` 设置预加载会话后调用一次。
    pub async fn restore_history(&self) -> Result<(), RuntimeError>;

    /// 返回上下文管理器引用（供 frontend/test 查询 `message_count` 等）。
    pub fn context(&self) -> &Arc<dyn ContextManager>;

    /// 返回存储引用（供 frontend/test 查询会话消息）。
    pub fn storage(&self) -> &Arc<dyn Storage>;

    /// 派发子 Agent。
    pub async fn spawn_subagent(
        &self,
        spec: SubagentSpec,
        input: String,
    ) -> Result<SubagentHandle>;

    /// 订阅事件流。
    pub fn subscribe(&self) -> broadcast::Receiver<Event>;

    /// 当前会话快照。
    pub async fn snapshot(&self) -> SessionSnapshot;

    /// 优雅关闭。
    pub async fn shutdown(&self) -> Result<()>;
}

pub struct RuntimeBuilder {
    /* setters */
}

impl RuntimeBuilder {
    pub fn config(mut self, c: RuntimeConfig) -> Self;
    pub fn provider(mut self, p: Arc<dyn LlmProvider>) -> Self;
    pub fn tools(mut self, t: ToolRegistry) -> Self;
    pub fn storage(mut self, s: Arc<dyn Storage>) -> Self;
    pub fn policy(mut self, p: Arc<dyn PermissionPolicy>) -> Self;
    pub fn prompter(mut self, p: Arc<dyn PermissionPrompter>) -> Self;
    pub fn context_manager(mut self, c: Arc<dyn ContextManager>) -> Self;
    pub fn workdir(mut self, p: Utf8PathBuf) -> Self;
    // 新增可注入能力（参考 CC/Codex 的可扩展架构）
    pub fn sandbox_driver(mut self, d: Arc<dyn SandboxDriver>) -> Self;
    pub fn hook_registry(mut self, h: HookRegistry) -> Self;
    pub fn project_doc_loader(mut self, l: Arc<dyn ProjectDocLoader>) -> Self;
    pub fn journal(mut self, j: Arc<dyn Journal>) -> Self;       // None 时 /undo 不可用
    pub fn mcp_client(mut self, c: Arc<dyn McpClient>) -> Self;  // 见 §11
    /// 取消 token（默认新建；CLI 可注入共享 token 以便 Ctrl-C 触发 graceful stop，C-13）。
    pub fn cancel_token(mut self, t: CancellationToken) -> Self;
    /// 预加载会话（`--resume`/`--fork-session` 用，T-M3-10a）。
    /// 设置后 Runtime 使用该会话的 id 与 messages；调用方需另行调用
    /// `Runtime::restore_history` 将消息注入上下文管理器。
    pub fn session(mut self, s: Session) -> Self;
    pub fn build(self) -> Result<Runtime>;
}
```

### 4.2 输入与事件

```rust
pub struct UserInput {
    pub text: String,
    pub attachments: Vec<Attachment>,
    pub context_hint: Option<ContextHint>,
}

pub enum Attachment { File(Utf8PathBuf), Image(Vec<u8>, String) }

pub enum Event {
    MessageAppended(Message),
    Token(String),
    TurnStreamingStarted,
    ToolCallStart(ToolCall),
    ToolCallProgress { id: ToolCallId, bytes: usize },
    ToolCallEnd { id: ToolCallId, ok: bool, elapsed: Duration },
    /// 通知类：权限已询问（仅展示/审计，不含回复通道）。
    PermissionRequested { id: String, tool: String, summary: String, risk: Risk },
    /// 通知类：权限已 resolved（带最终决策，供 UI 关闭弹窗与审计）。
    PermissionResolved { id: String, decision: Decision },
    TurnEnd { stop_reason: StopReason },
    Error(RuntimeError),
    SubagentStarted { id: String, role: String },
    SubagentFinished { id: String, summary: String },
    // 新增事件（可克隆，与 broadcast 兼容）
    /// task.update 工具调用后广播，供 UI 渲染任务进度（见 design.md §18.4）。
    TaskUpdated { task: Task },
    /// Hook 执行结果通知（见 hooks.md §8 / design.md §20.2）。
    HookRun { name: String, event: String, decision: HookDecision, elapsed: Duration },
    /// Plan 模式状态切换（见 design.md §16.2）。
    PermissionModeChanged { from: PermissionMode, to: PermissionMode },
    /// 文件回滚执行结果（见 design.md §17.4）。
    FileUndone { report: UndoReport },
}
```

---

## 5. L3 SDK 高层 API

```rust
pub struct Client { runtime: Runtime }

impl Client {
    pub fn builder() -> ClientBuilder;

    /// 单次提问，返回完整文本。
    pub async fn ask(&self, prompt: impl Into<String>) -> Result<String>;

    /// 流式提问。
    pub async fn ask_stream(
        &self, prompt: impl Into<String>,
    ) -> Result<BoxStream<'static, Result<Delta>>>;

    /// 执行任务（可能多轮、多工具），返回报告。
    pub async fn run_task(&self, task: impl Into<String>) -> Result<TaskReport>;

    /// 订阅事件。
    pub fn subscribe(&self) -> broadcast::Receiver<Event>;
}

pub struct TaskReport {
    pub final_text: String,
    pub tool_calls: Vec<ToolCallRecord>,
    pub tokens_used: Usage,
    pub duration: Duration,
    pub artifacts: Vec<Artifact>,
}
```

### 5.1 SDK 使用示例

```rust
use minicoding_sdk::Client;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = Client::builder()
        .provider_from_env()?
        .workdir(".")?
        .allow_read_only()      // 默认只读
        .build()?;

    // 简单提问
    let answer = client.ask("解释这个项目的入口").await?;
    println!("{answer}");

    // 流式
    use futures::StreamExt;
    let mut s = client.ask_stream("重构 utils 模块").await?;
    while let Some(delta) = s.next().await {
        if let minicoding_core::Delta::Text(t) = delta? {
            print!("{t}");
        }
    }
    Ok(())
}
```

---

## 6. 配置 Schema

完整 schema 见 `design.md` §12（本表节选关键）：

```toml
# ~/.minicoding/config.toml  （根目录可由 MINICODING_HOME 覆盖，见 data-model.md §3.0）
[provider]
default = "anthropic"

[provider.anthropic]
model = "claude-sonnet-4"
api_key_env = "ANTHROPIC_API_KEY"
timeout_sec = 120
retry = { max_attempts = 4, base_delay_ms = 500 }

[context]
budget_ratio = 0.85
compress = true
max_tool_iters = 50
turn_timeout_sec = 600

[tools]
enabled_groups = ["core", "fs", "shell", "web"]
[tools.fs]
max_read_bytes = 1048576
[tools.shell]
timeout_sec = 120
max_output_bytes = 1048576
[tools.web]
allowed_domains = ["*"]

[permission]
default = "ask"
non_tty_strategy = "deny"      # deny(默认) | allow | fail
[[permission.allow]]
tool = "fs.write"
glob = "src/**"
[[permission.deny]]
tool = "shell.run"
command_prefix = ["rm -rf", "sudo"]

# 审批模式 × 沙箱策略（预设），见 security.md §2.6/§8
# 预设优先级低于 [[permission.allow/deny]]，但高于默认矩阵；内置黑名单始终最高
[approval]
mode = "on-request"            # untrusted | on-failure | on-request(默认) | never
[sandbox]
policy = "workspace-write"     # read-only | workspace-write(默认) | danger-full-access
allow_dotgit_write = false     # 强烈不推荐开启
allow_network = ["api.anthropic.com", "api.openai.com"]   # 网络白名单（覆盖默认禁）
extra_writable = ["target/", "dist/"]
# presets 段：保存命名预设，CLI --preset <name> 选用
[profiles.full_auto]
approval_mode = "on-failure"
sandbox_policy = "workspace-write"
[profiles.readonly_ci]
approval_mode = "never"
sandbox_policy = "read-only"

[permission_mode]
initial = "default"            # default | accept-edits | plan | auto | bypass-permissions

[storage]
dir = "~/.minicoding/sessions"

[memory]
dir = "~/.minicoding/memory"
long_term_file = "long_term.md"
session_summary_max_tokens = 200

# 项目记忆分层加载（见 design.md §8.6）
[project]
project_doc_fallback_filenames = ["CLAUDE.md", ".cursorrules", "TEAM_GUIDE.md"]
project_doc_max_bytes = 32768

# Hooks（见 hooks.md §6）
[hooks]
on_hook_error = "continue"     # continue(默认) | deny | fail
default_timeout_sec = 30

# 特性门控（opt-in 功能）
[features]
file_undo = false              # /undo 文件回滚（参考 Codex features.undo，默认关）
plan_mode = true               # Plan 模式（design.md §16）
typed_subagents = true         # 类型化子 Agent（design.md §7.2）

# MCP server 配置（见 design.md §19.2）
[mcp_servers.github]
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }
startup_timeout_sec = 20
tool_timeout_sec = 60
enabled = true
required = false
enabled_tools = ["list_prs", "create_pr"]

[mcp_servers.internal_api]
transport = "http"
url = "https://internal.corp/mcp"
bearer_token_env_var = "INTERNAL_API_TOKEN"
```

---

## 7. 错误类型层次

```rust
// core/src/model/error.rs
#[derive(thiserror::Error, Debug)]
pub enum RuntimeError {
    #[error("llm: {0}")] Llm(#[from] LlmError),
    #[error("tool `{tool}`: {source}")]
    Tool { tool: String, #[source] source: ToolError },
    #[error("permission denied: {0}")] Permission(String),
    #[error("context budget exceeded ({used}/{budget})")]
    BudgetExceeded { used: usize, budget: usize },
    #[error("storage: {0}")] Storage(#[from] StorageError),
    #[error("config: {0}")] Config(String),
    #[error("interrupted")] Interrupted,
    #[error("io: {0}")] Io(#[from] std::io::Error),
}

#[derive(thiserror::Error, Debug)]
pub enum LlmError {
    #[error("network: {0}")] Network(#[from] reqwest::Error),
    #[error("rate limited; retry after {retry_after_ms:?}ms")]
    RateLimited { retry_after_ms: Option<u64> },
    #[error("server {status}: {body}")] Server { status: u16, body: String },
    #[error("client {status}: {body}")] Client { status: u16, body: String },
    #[error("content filtered: {reason}")] Filtered { reason: String },
    #[error("stream parse: {0}")] Parse(String),
    #[error("timeout after {0:?}")] Timeout(Duration),
}

#[derive(thiserror::Error, Debug)]
pub enum ToolError {
    #[error("invalid input: {0}")] InvalidInput(String),
    #[error("path escapes workdir: {0}")] PathEscaped(String),
    #[error("not found: {0}")] NotFound(String),
    #[error("timeout after {0:?}")] Timeout(Duration),
    #[error("cancelled")] Cancelled,
    #[error("io: {0}")] Io(#[from] std::io::Error),
    #[error("execution: {0}")] Exec(String),
}
```

---

## 8. 稳定性与版本化

- L1 trait 以 `#[non_exhaustive]` 标注，新增字段不破坏下游。
- L2/L3 API 遵循 SemVer；`0.x` 期间允许破坏性变更，每个 minor 版本发 CHANGELOG。
- 配置 schema 向前兼容；新增字段必须有默认值；废弃字段保留 2 个版本并打 deprecation warning。
- 事件枚举 `Event` 标 `#[non_exhaustive]`，订阅者必须处理通配分支。

---

## 9. 跨语言 / 跨进程接口（后续）

阶段 3 提供：

- **HTTP/JSON-RPC**（`minicoding serve`）：REST 风格 `/ask` `/stream` `/event`，便于非 Rust 集成。
- **MCP Server**：实现 Model Context Protocol，作为工具源被其他 Agent 调用。
- **stdin/stdout NDJSON 协议**：便于编辑器插件以子进程方式嵌入。
- **ACP stdio**（`minicoding serve --acp`）：Agent Client Protocol，可被 Zed 等支持 ACP 的客户端嵌入。
- **LSP stdio**（`minicoding serve --lsp`）：Language Server Protocol，基于 `tower-lsp`，可被 VS Code/Neovim/Emacs/Helix 等支持 LSP 的编辑器嵌入。语义映射见 `design.md` §24：`workspace/executeCommand`→发送 prompt / 斜杠命令、`$/progress`→流式 token 与工具进度、`window/showMessageRequest`→权限确认（`LspPrompter` 实现 `PermissionPrompter`）、`textDocument/codeAction`→AI 快速操作。

五者共用 `core` 的数据模型与 `minicoding-protocol` 的 wire types，仅序列化协议与传输层不同。

### 9.1 HTTP/SSE server 与 CLI `serve` 子命令签名

`minicoding-server` crate 提供 HTTP/SSE server 入口；`minicoding-cli` 通过 `serve` feature gate 委托同一入口（见 `modules.md` §12/§16）。

```rust
// minicoding-server/src/http.rs

/// HTTP server 启动配置（CLI `serve` 子命令与独立二进制共用）。
pub struct ServerConfig {
    pub bind: std::net::SocketAddr,
    pub provider_kind: String,        // "openai"/"anthropic"/"ollama"
    pub api_base: String,
    pub api_key: String,
    pub model: String,
    pub workdir: camino::Utf8PathBuf,
    pub system: Option<String>,
    pub permission_timeout_sec: u64,  // 权限交互超时（默认 300）
}

/// 启动 HTTP/SSE server（阻塞当前 task 直到 server 关闭）。
///
/// # Errors
/// bind 冲突、IO 错误、Runtime 构造失败时返回 `anyhow::Error`。
pub async fn serve(cfg: ServerConfig) -> anyhow::Result<()>;
```

```rust
// minicoding-server/src/session_mgr.rs

/// 多会话管理器（HTTP handler 通过此管理会话生命周期）。
///
/// `create_session`/`cancel`/`get`/`list_sessions`/`delete` 为同步方法
/// （内部仅操作 `std::sync::Mutex<HashMap>`，无 IO），避免 `async fn(&self, ..)`
/// 与 axum `Handler` trait HRTB 冲突（见 `design.md` §24）。
pub struct SessionManager { /* opaque */ }

impl SessionManager {
    pub fn new(
        default_params: ServerRuntimeParams,
        permission_timeout: Duration,
    ) -> Self;

    /// 创建新会话（同步——`build_runtime` 不涉及 await）。
    pub fn create_session(
        &self,
        params_override: Option<ServerRuntimeParams>,
    ) -> Result<Arc<ServerSession>, SessionManagerError>;

    /// 查找会话（同步）。
    pub fn get(&self, session_id: &str) -> Option<Arc<ServerSession>>;

    /// 列出所有会话（同步）。
    pub fn list_sessions(&self) -> Vec<SessionMeta>;

    /// 删除会话（同步）。
    pub fn delete(&self, session_id: &str) -> bool;

    /// 取消当前 turn（同步——`Runtime::cancel` 仅触发 `CancellationToken`，无 await）。
    pub fn cancel(&self, session_id: &str) -> Result<(), SessionManagerError>;

    /// 解析权限请求（async——涉及 `TokioMutex` await）。
    pub async fn resolve_permission(
        &self,
        session_id: &str,
        permission_id: &str,
        decision: Decision,
    ) -> Result<(), SessionManagerError>;

    /// 获取会话消息快照（async——涉及 `Storage::load` await）。
    pub async fn get_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<Message>, SessionManagerError>;

    /// 发送用户消息并驱动 turn（阻塞至 turn 完成）。
    ///
    /// **关联函数**（非 `&self` 方法）+ owned `Arc<SessionManager>` 参数，
    /// 返回的 future 无外部借用（`'static`），避免 `async fn(&self, ..)` 与 axum
    /// `Handler` trait HRTB 冲突。内部获取 `turn_lock`（串行化），订阅 `EventBus`
    /// 收集事件分配 seq，调用 `Runtime::run_turn`（见 §4.1）。
    ///
    /// # Errors
    /// - 会话不存在：`NotFound`；
    /// - `run_turn` 失败：透传为 `BuildFailed`。
    pub async fn send_message_boxed(
        mgr: Arc<SessionManager>,
        session_id: String,
        text: String,
    ) -> Result<TurnOutcome, SessionManagerError>;
}
```

```rust
// minicoding-cli/src/commands/serve.rs（`serve` feature gate）

/// `minicoding serve` 子命令参数（clap derive）。
///
/// `--bind` 与 `--port` 互斥；省略时默认 `127.0.0.1:8080`。
#[derive(clap::Args, Debug)]
pub struct ServeCommand {
    /// 监听地址（如 `127.0.0.1:8080`）。与 `--port` 互斥。
    #[arg(long, conflicts_with = "port")]
    pub bind: Option<String>,
    /// 监听端口（绑定 `127.0.0.1:<port>`）。与 `--bind` 互斥。
    #[arg(long, conflicts_with = "bind")]
    pub port: Option<u16>,
    /// LLM provider 类型（`openai`/`anthropic`/`ollama`）。
    #[arg(long, env = "OPENAI_PROVIDER", default_value = "openai")]
    pub provider: String,
    /// API base URL（省略时按 provider 选默认）。
    #[arg(long, env = "OPENAI_API_BASE")]
    pub api_base: Option<String>,
    /// API key（Ollama 可省略）。
    #[arg(long, env = "OPENAI_API_KEY")]
    pub api_key: Option<String>,
    /// 模型名称。
    #[arg(long, env = "OPENAI_MODEL", default_value = "gpt-4o")]
    pub model: String,
    /// 工作目录。
    #[arg(long, default_value = ".")]
    pub workdir: String,
    /// 系统 prompt 覆盖。
    #[arg(long)]
    pub system: Option<String>,
    /// 权限交互超时（秒）。
    #[arg(long, default_value_t = 300)]
    pub permission_timeout_sec: u64,
}

/// 运行 `serve` 子命令：构造 `ServerConfig` 并调用 `minicoding_server::serve`（阻塞）。
///
/// # Errors
/// - bind 地址解析失败；
/// - server 运行时错误（bind 冲突、IO 错误等）。
pub async fn run_serve_command(cmd: &ServeCommand) -> anyhow::Result<()>;
```

`SessionManager::send_message_boxed` 的关联函数形态是 HRTB 兼容方案的关键——`Arc<SessionManager>` owned 移入 future，无 `&self` 借用泄漏。`Runtime::run_turn_owned`（§4.1）同理。详见 `design.md` §24。

---

## 10. 内置工具目录（参考 CC/Codex）

本节列出新增的内置工具，与 `design.md` §16-§18 对应。原有工具（`fs.*`/`shell.run`/`web.fetch`/`git.*`/`task.spawn`）见 `design.md` §4.3。

### 10.1 任务管理工具 `task.create`/`task.update`/`task.list`（见 `design.md` §18）

任务管理采用 Claude Code v2.1.142+ 的增量模型（`task.create`/`update`/`list` 三件套），替代旧版全量替换的 `todo.write`。三个工具均属 `Task` 工具组，`SideEffect::None`（仅更新内存状态 + 广播事件），Plan 模式下可用（只读）。

#### `task.create`

| 项 | 值 |
|----|----|
| 输入 | `TaskCreateInput { subject, description?, active_form?, priority?, metadata? }` |
| 输出 | `TaskCreateOutput { task_id }`（Runtime 生成 ULID，不可伪造，见 C-31） |

#### `task.update`

| 项 | 值 |
|----|----|
| 输入 | `TaskUpdateInput { task_id, status?, subject?, description?, active_form?, add_blocks?, add_blocked_by?, owner?, metadata? }` |
| 输出 | 更新后的 `Task` |

`add_blocks`/`add_blocked_by` 是增量添加依赖边（非整体替换），重复添加幂等。`status` 转换须合法（`Pending → InProgress → Completed`/`Deleted` 单向，不可回退，见 C-31）。

#### `task.list`

| 项 | 值 |
|----|----|
| 输入 | `TaskListInput { status_filter? }` |
| 输出 | `TaskListOutput { tasks: Vec<Task> }` |

校验规则（违反返回 `ToolError::InvalidInput` 或 `ToolError::InvalidStateTransition`）：

- `subject` 非空；
- 同一时间 `InProgress` 项 ≤ 1（防并行开干）；
- `Completed`/`Deleted` 项必须含 `summary`（实际完成内容/证据）；
- 状态迁移合法（`Completed`/`Deleted` 不可回 `Pending`/`InProgress`，见 C-31）；
- `task_id` 必须命中已注册任务，伪造返回 `ToolError::NotFound`（见 C-31）；
- `add_blocked_by` 引用的 task_id 须存在；依赖图不可成环（DFS 检测）；
- 尝试将 `InProgress` 设给被未完成依赖阻塞的任务 → 拒绝并提示阻塞者。

调用后广播 `Event::TaskUpdated`，UI 据此渲染任务面板。

> **废弃别名 `todo.write`**：旧版全量替换工具 `todo.write`（`TodoWriteInput { todos: Vec<Todo> }`）作为兼容别名保留一个版本，内部转为"先批量 `task.create`，再差异 `task.update`"。新代码与新提示词应直接用 `task.*` 三件套（见 `design.md` §18.9）。`Event::TodoUpdated` 同步更名为 `Event::TaskUpdated`。

### 10.2 `plan.exit`（ExitPlanMode，见 `design.md` §16.4）

| 项 | 值 |
|----|----|
| 工具组 | `Plan` |
| 副作用 | `None`（仅切换 `PermissionMode` + 缓存预批准） |
| 只读（Plan 模式） | 是（仅在 Plan 模式下可调用） |
| 输入 | `ExitPlanModeInput { plan_path, allowed_prompts, plan_was_edited }` |
| 输出 | 提示用户决策门（approve/modify/reject） |

调用后 Runtime 触发 `Event::PermissionModeChanged { from: Plan, to: Default|AcceptEdits }`，并把 `allowed_prompts` 注入会话级 `PermissionPolicy` 缓存。该工具仅在 `PermissionMode::Plan` 下可调用，其它模式下调用返回错误。

### 10.3 `file.undo`（/undo，见 `design.md` §17.5）

`/undo` 作为斜杠命令而非 LLM 工具暴露——文件回滚是用户控制语义，不应让 LLM 自主触发（防模型回滚自己的错误改动后继续犯错）。CLI 解析 `/undo [steps]` 后调用 `Journal::undo`。若 `[features] file_undo = false`，斜杠命令返回"未启用"。

### 10.4 `task.spawn`（类型化子 Agent，见 `design.md` §7.2）

`task.spawn` 的输入从自由 `role: String` 改为类型化 `SubagentSpec`：

```json
{
  "type": "explore",
  "prompt": "查找所有调用 foo() 的位置",
  "thoroughness": "medium",
  "model": null,
  "max_iters": 10
}
```

| 项 | 值 |
|----|----|
| 工具组 | `Task` |
| 副作用 | `None`（父 Agent 只接收 `summary`；子 Agent 的副作用在其自身权限链处理，C-05） |
| 只读（Plan 模式） | 是（父会话视角；子 Agent 内部仍受自身 `PermissionMode` 约束） |
| 持有能力 | `Arc<dyn SubagentRunner>`（由 `Runtime::subagent_runner()` 注入）+ `Arc<dyn PlanModeController>`（Plan 模式守卫） |
| 输出 | `{ summary, artifacts, token_used, completed }`（C-05：仅 `summary`，不回灌中间消息） |

Runtime 派发前强制校验：`can_spawn_subagent == false` 时移除子 Agent 工具集中的 `task.spawn`（防嵌套）；`SubagentType::Plan` 仅在 `PermissionMode::Plan` 下可派发，其它模式下退化为 `Explore`（不报错，避免模型重试）。

`SubagentRunner` trait 在 `minicoding-core::agent` 定义（`dyn` 兼容）；默认 `NoopSubagentRunner` 返回 `RuntimeError::Config`——未注入实现时 `task.spawn` 调用直接失败（不静默 no-op）。`task.spawn` 在 `tracing::info_span!("subagent", ty, max_iters)` 内执行，通过 `Span::current()` 自动挂在父 turn span 下（`OTel` 父子关系，design.md §15.2）。

---

## 11. MCP Client API（见 `design.md` §19）

`McpClient` 抽象 MCP 消费侧，由 Runtime 在启动时根据 `[mcp_servers.*]` 配置构建实例并注入 `RuntimeBuilder`。

```rust
#[trait_variant::make(McpClient: Send)]
pub trait McpClient {
    /// 启动所有已配置且 enabled 的 MCP server，握手 + list_tools。
    /// required server 启动失败返回 Err，Runtime 拒绝启动。
    async fn start(&self, configs: &[McpServerConfig]) -> Result<(), McpError>;

    /// 返回所有已就绪 server 的工具，命名为 mcp__<server>__<tool>。
    async fn list_tools(&self) -> Vec<ToolSchema>;

    /// 调用某个 MCP 工具，超时由 server 配置的 tool_timeout 决定。
    async fn call(&self, server: &str, tool: &str, input: serde_json::Value) -> Result<ToolResult, McpError>;

    /// 健康检查（进程池模式）。
    async fn health_check(&self) -> Result<bool, McpError>;

    /// 预热连接（后台预热）。
    async fn warm_up(&self) -> Result<(), McpError>;

    /// 优雅关闭所有 server（stdio: EOF；http: 连接池释放）。
    async fn shutdown(&self) -> Result<(), McpError>;
}
```

**默认实现 `RmcpClient`**：基于官方 Rust MCP SDK（`rmcp` 2.2 crate，对齐 MCP 2025-11-25 spec），支持 stdio（`transport-child-process`）+ Streamable HTTP（`transport-streamable-http-client-reqwest`，rustls）+ OAuth。M4 一步到位，**不再"自实现 stdio 薄封装"过渡方案**（rmcp 2.x 已稳定）。

**project 作用域批准流**（防恶意仓库植入）：

```rust
pub async fn check_project_scope_approval(
    project_config_path: &Utf8PathBuf,
    choices_store: &dyn McpChoicesStore,
    prompter: &dyn PermissionPrompter,
) -> Result<Vec<McpServerConfig>, McpError>;
// 首次遇到 .minicoding/mcp.json 时，逐个 server 弹窗询问是否启用，
// 结果写入 ~/.minicoding/mcp_choices.toml；后续启动直接读 choices。
```

**与权限系统的协作**：MCP 工具调用走与内置工具相同的 `PermissionPolicy::check` 流程，`tool` 名为 `mcp__<server>__<tool>`，权限规则支持 `mcp__github__*` 通配（见 `design.md` §19.3）。MCP 工具的 `side_effect` 由 server schema 的 `readOnlyHint`/`destructiveHint` 映射，`is_read_only()` 据此覆盖默认实现。
