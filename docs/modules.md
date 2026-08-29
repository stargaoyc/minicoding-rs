# 模块详细设计

本文描述每个 crate / 模块的职责边界、内部结构、公共 API 与对外依赖。所有 crate 组成 Cargo workspace。

> **重构说明（v2）**：原 `minicoding-core` 承载 14+ 职责（Agent 循环、上下文、权限、沙箱 trait、Hook trait、项目记忆、Journal、MCP trait、存储、审计、事件总线、配置、OTel、记忆），违反单一职责。本次重构将 core 精简为"抽象层 + Runtime 编排"，把各领域**实现**拆到独立 crate。trait 定义仍集中在 core（保证 Runtime 可持有 `Arc<dyn Trait>`），实现分散到领域 crate。沙箱为自研轻量驱动（~~`sandbox-run`~~ 因 EUPL-1.2 弃用；Linux `landlock` 直连 + 自研 pre_exec 胶水，见 `tech-stack.md` §13），MCP 改用官方 `rmcp` 2.x 不自研。

---

## 0. Workspace 总览

### 0.1 Crate 列表

> **当前 Cargo workspace 含 18 个 Cargo crate**（M0–M8 的 17 个 + M9 新增 `minicoding-desktop`）。M9 另含 `minicoding-web`（独立 npm 项目，不入 Cargo workspace，见 §18）。下表列出全部 18 个 Cargo crate + `minicoding-web` 前端项目（共 19 个目录，其中 web 非 Cargo crate）。

```
minicoding-rs (workspace)
├── crates/
│   ├── minicoding-core          # 抽象层：数据模型 + 核心 trait + Runtime 编排 + Event + Config + OTel + Prompt 管道
│   ├── minicoding-context       # ContextManager 实现 + 4 级压缩 + 权重 + 熔断 + 预测压缩 + Post-compact 恢复
│   ├── minicoding-policy        # 权限实现：PermissionPolicy/Prompter + builtin 黑名单 + ApprovalMode/Preset
│   ├── minicoding-memory        # 记忆实现：长期/Auto/会话记忆 + AGENTS.md loader
│   ├── minicoding-hooks         # Hooks 实现：Registry + ScriptHook + asyncRewake + 内置 Hook
│   ├── minicoding-journal       # FileChangeJournal 实现 + /undo
│   ├── minicoding-sandbox       # OS 沙箱驱动（自研 pre_exec 胶水 + landlock 直连 + seccomp opt-in）
│   ├── minicoding-mcp           # MCP client/server（基于 rmcp 2.x）+ 进程池 + 后台预热 + inflight merge
│   ├── minicoding-storage       # JSONL 存储 + audit.log 审计
│   ├── minicoding-providers     # LLM Provider 实现（OpenAI/Anthropic/Ollama）+ 小 LLM 配置
│   ├── minicoding-tools         # 内置 Tool 实现（fs/shell/web/git/task/plan/mcp 包装）
│   ├── minicoding-protocol      # JSON-RPC 2.0 wire types + Event/Command DTO（前后端协议契约）
│   ├── minicoding-server        # HTTP/SSE server + ACP/LSP 适配器（多前端接入层）+ M9 --web 静态托管
│   ├── minicoding-extension-sdk # 扩展作者稳定 API（Extension trait + Registrar + Manifest）
│   ├── minicoding-cli           # CLI frontend
│   ├── minicoding-tui           # TUI frontend（M7）
│   ├── minicoding-sdk           # 嵌入 SDK（M8）
│   ├── minicoding-web           # Web 前端（React 19 + TS + Vite 6 + Tailwind v4，M9，独立 package.json 不入 workspace）
│   └── minicoding-desktop       # Tauri 2.x 桌面壳（M9，sidecar 启动 minicoding-server，feature gate `desktop`）
```

### 0.2 依赖方向

```
              cli / tui / sdk      server ◄── web（HTTP/SSE，M9）
                    │                │            │
                    │                │            ▼
                    │                │        desktop（Tauri sidecar，M9）
                    └────┬───────────┘
                         ▼
                       tools ──┬─► context   (压缩、token 预算)
                       │       ├─► policy    (权限决策)
                       │       ├─► memory    (长期记忆注入)
                       │       ├─► hooks     (Hook 触发)
                       │       ├─► journal   (文件改动记录)
                       │       ├─► sandbox   (命令执行前 apply)
                       │       ├─► mcp       (远程工具包装)
                       │       └─► storage   (审计落盘)
                       │
                       ▼
              core ◄── providers    (LlmProvider 实现)
                ▲    ◄── storage    (Storage/audit 实现)
                │    ◄── context    (ContextManager 实现)
                │    ◄── policy     (PermissionPolicy/Prompter 实现)
                │    ◄── memory     (记忆 loader 实现)
                │    ◄── hooks      (HookRegistry 实现)
                │    ◄── journal    (Journal 实现)
                │    ◄── sandbox    (SandboxDriver 实现)
                │    ◄── mcp        (McpClient 实现)
                │
              core ◄── protocol    (wire types / DTO，依赖 core)
                ▲    ◄── extension-sdk (Extension trait / Registrar，依赖 core)
                │
              (trait 定义集中在此)

  说明：
  - protocol 仅依赖 core（wire types + DTO）
  - extension-sdk 仅依赖 core（Extension trait + Registrar + Manifest）
  - server 依赖 core + protocol + tools（HTTP/SSE 接入层）
  - cli/tui/sdk 依赖 tools + core + protocol（前端进程）
```

### 0.3 分层原则

1. **core 是抽象层，零实现逻辑**：只含数据模型、trait 定义、Runtime 编排、Event、Config、OTel 初始化。无平台/网络重依赖，编译快、测试快。
2. **trait 定义在 core**：`Tool`/`PermissionPolicy`/`PermissionPrompter`/`SandboxDriver`/`Hook`/`ContextManager`/`Storage`/`Journal`/`McpClient`/`ProjectDocLoader`/`LlmProvider` 全部在 core 定义，保证 Runtime 可持有 `Arc<dyn Trait>`。实现可来自任意 crate。
3. **实现单一职责**：每个领域 crate 只负责一类实现（policy 只管权限、memory 只管记忆），不交叉。
4. **依赖单向**：core 不依赖任何领域 crate；领域 crate 依赖 core；tools 作为"组合层"可依赖多个领域 crate；cli/tui/sdk 依赖 tools + core。
5. **平台/网络隔离**：sandbox（C 绑定）、mcp（rmcp 网络栈）、providers（HTTP）的重依赖隔离在各自 crate，不污染 core。
6. **扩展系统一等公民**：`minicoding-extension-sdk` 提供稳定扩展 API，核心能力（Plan/Task/Memory）可插件化。扩展注册的工具仍走统一 `ToolRegistry` dispatch，确保权限审计一致。

### 0.4 特性门控

实现 crate 通过 cargo feature 按需启用，避免强制引入重依赖：

```toml
# minicoding-cli/Cargo.toml
[dependencies]
minicoding-core = { path = "../minicoding-core" }
minicoding-context = { path = "../minicoding-context" }
minicoding-policy = { path = "../minicoding-policy" }
minicoding-storage = { path = "../minicoding-storage" }
minicoding-providers = { path = "../minicoding-providers" }
minicoding-tools = { path = "../minicoding-tools" }
# 可选能力（feature gate）
minicoding-memory  = { path = "../minicoding-memory",  optional = true }
minicoding-hooks   = { path = "../minicoding-hooks",   optional = true }
minicoding-journal = { path = "../minicoding-journal", optional = true }
minicoding-sandbox = { path = "../minicoding-sandbox", optional = true }
minicoding-mcp     = { path = "../minicoding-mcp",     optional = true }

[features]
default = ["memory", "sandbox"]
memory  = ["dep:minicoding-memory"]
hooks   = ["dep:minicoding-hooks"]
file-undo = ["dep:minicoding-journal"]
sandbox = ["dep:minicoding-sandbox"]
mcp     = ["dep:minicoding-mcp"]
full    = ["memory", "hooks", "file-undo", "sandbox", "mcp"]
```

---

## 1. `minicoding-core`（抽象层 + Runtime 编排）

### 1.1 职责

仅含**抽象**与**编排**，不含任何领域实现逻辑：

- 数据模型：`Message`/`Role`/`ToolCall`/`ToolResult`/`Session`/`SessionId`/`SubagentType`/`Todo`/错误类型
- 核心 trait 定义：`Tool`/`LlmProvider`/`ContextManager`/`PermissionPolicy`/`PermissionPrompter`/`SandboxDriver`/`Hook`/`HookRegistry`/`Storage`/`Journal`/`McpClient`/`ProjectDocLoader`
- Runtime 聚合根 + Agent 循环（编排各 trait，本身不含领域算法）
- 事件总线（`Event`/`EventBus`，仅通知无回复）
- 配置（`RuntimeConfig` 分层加载 + profiles）
- OTel 初始化与 span 辅助
- 路径约定（`paths.rs`）
- Prompt 管道（`PromptContributor` trait + `PromptSection` + `PromptSectionOrder`）
- 扩展系统 trait（`ExtensionHost` + `ExtensionManifest` + `Registrar`）

### 1.2 内部模块树

```
minicoding-core/src/
├── lib.rs                 # prelude 再导出
├── runtime/
│   ├── mod.rs             # Runtime 聚合根 + AgentLoop 主循环（并行/串行分桶，见 design.md §2.3）
│   ├── rt.rs              # Runtime 主链（run_turn/stream_llm/execute_tool_calls/register_dynamic_tool 等，2026-08-23 审查 §3 后内聚单元下沉至下列模块）
│   ├── builder.rs         # RuntimeBuilder（链式注入 provider/ctx/policy/sandbox/hooks/journal/...）
│   ├── permission.rs      # 权限决策 + Hook 管道（策略判定→PreToolUse→改写重查取严→prompter→审计，见 design.md §9）
│   ├── denial.rs          # 沙箱拒绝检测/熔断回灌 + 初始化失败沙箱外回退（C-30/C-22，见 security.md §8）
│   ├── sourcing.rs        # 事件溯源（init_event_stream/persist_event/create_snapshot/durable_seq，见 design.md §25）
│   ├── hot_config.rs      # turn 边界白名单配置热更新 + 热更新基线（M-12/S-22，见 tech-stack.md §13）
│   ├── workdir.rs         # 工作区切换（W-11 switch_workdir）
│   ├── event.rs           # Event / EventBus（仅通知，含 TaskUpdated/HookRun/PermissionResolved/FileUndone/ConfigChanged）
│   ├── accumulator.rs     # 流式 delta 聚合
│   ├── repair.rs          # 悬空 tool_call 修复（resume 幂等重建，proptest 覆盖）
│   ├── repeat_guard.rs    # 循环打断器（软提醒/硬停止，C-13 防死循环）
│   └── plan_handle.rs     # PlanModeController 适配器（plan.exit 反向调用 Runtime）
├── agent/
│   ├── mod.rs
│   ├── runner.rs          # SubagentRunner trait + NoopSubagentRunner 兜底（见 design.md §7.3）
│   └── worktree.rs        # WorktreeSubagentRunner 装饰器（A-15，git worktree 隔离，见 design.md §7.5）
├── model/
│   ├── message.rs         # Message / Role / Content
│   ├── tool.rs            # ToolCall / ToolResult / ToolSchema
│   ├── session.rs         # Session / SessionId / SessionMeta
│   ├── subagent.rs        # SubagentType / SubagentSpec / Thoroughness
│   ├── task.rs            # Task / TaskStatus / TaskCreateInput / TaskUpdateInput（见 design.md §18）
│   └── error.rs           # RuntimeError / LlmError / ToolError / JournalError / McpError / HookError
├── provider/trait.rs      # LlmProvider / Tokenizer trait
├── tool/
│   ├── trait.rs           # Tool trait（含 is_read_only()，见 api.md §3.3）
│   ├── registry.rs        # ToolRegistry（按 side_effect 调度）
│   └── context.rs         # ToolContext / SideEffect
├── policy/
│   ├── trait.rs           # PermissionPolicy + PermissionPrompter + Verdict + Decision + PlanModeController（见 api.md §3.6）
│   ├── persist.rs         # PolicyPersist：AllowAlways 持久化存储（policy.toml 0600 原子写）
│                          #   ⚠ ARCH-1 登记豁免（2026-08-26 R3）：属"Runtime 编排基础设施"——
│                          #   与 NoopDriver 同类的用户级配置持久化原语（无网络/无平台依赖），
│                          #   迁至 minicoding-policy 需 policy→storage 反向边，故留 core；
│                          #   架构守卫白名单已含 toml/camino 等其所需轻量依赖。
│   └── mod.rs             # PermissionMode 枚举（Default/AcceptEdits/Plan/Auto/BypassPermissions）
├── sandbox/
│   ├── trait.rs           # SandboxDriver trait + SandboxPolicy 枚举 + NoopDriver 兜底（见 api.md §3.9）
│   ├── breaker.rs         # SandboxDenialDetector/SandboxDenialTracker trait + BreakerState + Noop 兜底（C-30，见 security.md §8.7/§8.8）
│   └── mod.rs
├── hooks/
│   ├── trait_def.rs       # Hook trait + HookEvent + HookDecision + HookOutput + AsyncRewakeSpec + dispatch 默认实现（见 api.md §3.8）
│   └── mod.rs
├── context/trait.rs       # ContextManager trait + ChatRequest + ContextSnapshot
├── memory/
│   ├── trait.rs           # ProjectDocLoader trait + MemoryStore trait（长期/Auto 记忆加载抽象）
│   └── mod.rs
├── journal/
│   ├── trait_def.rs       # Journal trait + ChangeEntry + UndoReport（见 api.md §3.11）
│   └── mod.rs
├── mcp/
│   ├── trait_def.rs       # McpClient trait + McpServerConfig + McpTransport + McpScope（见 api.md §11）
│   └── mod.rs
├── storage/
│   ├── trait.rs           # Storage trait + AuditSink trait
│   ├── event.rs           # EventStore/EventRecord/PersistedEvent trait + NoopEventStore（Event Sourcing，见 design.md §25）
│   ├── snapshot.rs        # SnapshotStore/SessionSnapshot/SessionState trait + NoopSnapshotStore
│   └── mod.rs
├── prompt/
│   ├── mod.rs             # prelude 再导出
│   ├── trait_def.rs       # PromptContributor trait + PromptSection + PromptSectionOrder（见 api.md §3.13）
│   ├── context.rs         # PromptContext（会话级输入聚合）
│   └── pipeline.rs        # PromptPipeline（9 段排序拼接 + 缓存统计）
├── extension/
│   ├── mod.rs             # prelude 再导出
│   ├── trait_def.rs       # Extension/ExtensionHost/Registrar trait + NoopExtensionHost + NoopRegistrar（见 api.md §3.12）
│   └── manifest.rs        # ExtensionManifest + ExtensionCarrier + Capability + ExtensionId/Info
├── config.rs              # RuntimeConfig 加载与合并（含 MINICODING_HOME + profiles + HooksConfig）
├── config/
│   └── watcher.rs         # ConfigWatcher（S-22 配置热更新，notify 8 + 500ms debounce + best-effort）
├── paths.rs               # 路径约定（见 data-model.md §3.0）
├── otel.rs                # OpenTelemetry 初始化 / span 辅助 / 资源属性
├── testing/               # `#[cfg(feature = "test-util")]` 共享测试基建（M-13，非领域实现）
│   └── storage_contract.rs # Storage 契约断言（内存/JSONL/未来 SQLite 后端共享，见 api.md §3.5）
└── util/
    └── circuit_breaker.rs # 通用熔断器骨架（单计数 + 双阈值，M-05 熔断去重：沙箱 C-30 与压缩 C-29 共用）
```

> **已实现（M5 范围）**：`prompt/`（Prompt 管道 9 个 contributor，P-30/P-31）与 `extension/`（ExtensionHost/Extension/Registrar，X-20..X-22）已实现。trait 定义在 `minicoding-core`，9 个内置 contributor 与 `BundledExtensionHost`/`BundleRegistrar` 实现在 `minicoding-extension-sdk`。详见 `api.md` §3.12/§3.13 与 `design.md` §22/§23。

### 1.3 公共 API（prelude）

> **R3 注**：以下为节选示意（实际导出项以 `crates/minicoding-core/src/lib.rs`
> 的 `pub mod prelude` 为准——含 `ExtensionCarrier`/`MemoryBlock`/`hard_trip_summary`
> 等数十项，此处不逐一罗列）。

```rust
pub mod prelude {
    pub use crate::runtime::{Runtime, RuntimeBuilder};
    pub use crate::agent::TurnOutcome;
    pub use crate::model::{Message, Role, ToolCall, ToolResult, Session, SessionId, SessionMeta,
                           SubagentType, Task, TaskStatus};
    pub use crate::provider::LlmProvider;
    pub use crate::tool::{Tool, ToolRegistry, ToolContext, SideEffect};
    pub use crate::policy::{PermissionPolicy, PermissionPrompter, Verdict, Decision};
    pub use crate::sandbox::{
        NoopDenialDetector, NoopDenialTracker, SandboxDenialDetector, SandboxDenialTracker,
        SandboxDriver, SandboxPolicy,
    };
    pub use crate::hooks::{Hook, HookEvent, HookDecision, HookOutput, AsyncRewakeSpec};
    pub use crate::context::{ContextManager, ChatRequest, ContextSnapshot};
    pub use crate::memory::ProjectDocLoader;
    pub use crate::journal::{Journal, ChangeEntry, UndoReport};
    pub use crate::mcp::{McpClient, McpServerConfig, McpTransport, McpScope};
    pub use crate::storage::{
        AuditKind, AuditRecord, AuditSink, EventRecord, EventStore, NoopAudit, NoopEventStore,
        NoopSnapshotStore, PersistedEvent, SCHEMA_VERSION, SNAPSHOT_INTERVAL, SessionSnapshot,
        SessionState, SnapshotStore, Storage, try_persist,
    };
    pub use crate::prompt::{PromptContributor, PromptSection, PromptSectionOrder};
    pub use crate::extension::{ExtensionHost, Extension, ExtensionManifest, Registrar};
    pub use crate::event::Event;
    pub use crate::config::RuntimeConfig;
}
```

### 1.4 关键设计点

- **零实现逻辑**：core 不含压缩算法、黑名单正则、landlock ruleset、rmcp 调用、JSONL 写入等任何实现。`Runtime` 只编排：调 `ContextManager::build_chat_request` → `LlmProvider::chat_stream` → `ToolRegistry::dispatch`（其内调 `PermissionPolicy::check` → `SandboxDriver::apply`）。
- **trait 定义集中**：所有领域 trait 在 core 定义，领域 crate 实现 trait。这样 Runtime 持有 `Arc<dyn ContextManager>` 等不需知道具体实现 crate，依赖方向干净。
- **轻量依赖**（A9 对齐实际清单，2026-08）：`tokio`/`tokio-util`/`futures`/`serde`/`serde_json`/`toml`/`tracing`/`thiserror`/`camino`/`uuid`/`ulid`/`time`/`home`/`semver`/`notify`(ConfigWatcher)/`ts-rs`(optional, feature `ts`)。守卫测试 `tests/architecture.rs` 白名单与此同步；无 `reqwest`/`landlock`/`rmcp` 等重依赖。
- **NoopDriver 兜底**：core 提供 `SandboxDriver` 的 `NoopDriver` 实现（无操作），供未启用 `minicoding-sandbox` feature 时使用。其他 trait 的默认实现（如 `JsonlStorage`）移到对应领域 crate，core 不提供。
- **Denial 抽象（M-05）**：`SandboxDenialDetector`/`SandboxDenialTracker` trait 与 `BreakerState`/`DenialMatch` 数据在 core（`sandbox/breaker.rs`），平台签名库与熔断实现在 `minicoding-sandbox`（`denial.rs`），core 默认注入 `NoopDenialDetector`/`NoopDenialTracker` 兜底（与 NoopDriver 同哲学）。事件重放（`replay_session_state` 等）M-05 后位于 `minicoding-storage`，core 不再导出。
- **熔断去重（M-05）**：沙箱拒绝熔断（C-30）与压缩熔断（C-29）共享 `util::CircuitBreaker` 通用骨架（单计数器 + 双阈值 + Closed/SoftTripped/HardTripped 状态）；压缩侧在骨架之上另维护 thrash 计数器（`consecutive_oversize`）表达其特有失效模式，两者状态语义各自映射，不互相耦合。
- **Prompt 管道**：`prompt/` 模块定义 `PromptContributor` trait，9 个 contributor 按固定顺序拼接（稳定段在前利于 prompt cache），扩展通过 `Registrar::register_prompt_contributor` 注入 section（见 `design.md` §22）。

---

## 2. `minicoding-context`（上下文管理实现）

### 2.1 职责

实现 `ContextManager` trait（定义在 core）：token 预算计算、消息权重模型、4 级压缩管道、压缩熔断与防 Thrash、状态保留清单、压缩失败降级链。

### 2.2 模块树

```
minicoding-context/src/
├── lib.rs                 # ContextManager 实现 + 工厂
├── manager.rs             # ContextManagerImpl（实现 trait）
├── budget.rs              # token 预算计算（精确分词 + 预留输出 + 安全余量）
├── weight.rs              # 消息权重模型（role×recency×sticky×pin）
├── compress/
│   ├── mod.rs             # 4 级压缩管道（裁剪→摘要→滚动→硬截断）
│   ├── clip.rs            # L1 工具结果裁剪
│   ├── summarize.rs       # L2 旧消息摘要（调 LLM）
│   ├── rolling.rs         # L3 滚动窗口
│   └── hard_truncate.rs   # L4 硬截断兜底
├── circuit_breaker.rs     # 压缩熔断状态机（见 design.md §3.6）
├── state_keep.rs          # 压缩后状态保留清单（SessionMeta 恢复，见 design.md §3.7）
├── fallback.rs            # L2 摘要失败降级链（主→备用→启发式→跳过 L3）
├── predictive.rs          # 预测性压缩（基于 turn token 增长估算，提前 compact）
├── post_compact_recover.rs # Post-compact 上下文恢复（重新注入最近 read 的文件内容）
└── tokenizer.rs           # tiktoken-rs 集成 + 启发式估算
```

### 2.3 关键设计点

- **依赖 core trait**：实现 `core::context::ContextManager`，调用 `core::model::Message`。
- **压缩熔断独立模块**：`circuit_breaker.rs` 维护失败计数与 Thrash 检测状态机，与 C-29 约束对应。
- **降级链复用**：`fallback.rs` 的 `SummaryFallback` 与 `minicoding-memory` 的会话摘要降级链同构，可通过 trait 共享。
- **预测性压缩**：`predictive.rs` 根据历史 turn token 增长均值估算，在超出窗口前提前 compact，与反应式 compact（阈值触发）互补（见 `design.md` §3.9）；
- **Post-compact 恢复**：`post_compact_recover.rs` compact 后从历史提取最近 read 过的文件路径，按预算截断重新注入，避免模型重新 read（见 `design.md` §3.10）。
- **依赖**：`minicoding-core` + `tiktoken-rs`（token 计数）+ `tokio`。摘要压缩需调 LLM，通过 `LlmProvider` trait 注入（不直接依赖 providers crate）。

---

## 3. `minicoding-policy`（权限实现）

### 3.1 职责

实现 `PermissionPolicy`/`PermissionPrompter` trait（定义在 core）：决策引擎、内置黑名单、ApprovalMode/Preset 解析、决策持久化、各 Prompter 实现。

### 3.2 模块树

```
minicoding-policy/src/
├── lib.rs                 # 工厂：build_policy(cfg) / build_prompter(cfg) + 公共 re-export
├── builtin.rs             # 内置不可覆盖黑名单（危险命令/SSRF/敏感路径，C-02）+ AGENTS.md 写保护（C-23）
├── mode.rs                # ApprovalMode / Preset 枚举与解析（见 api.md §2.4）
├── prompter.rs            # InteractivePrompter（CLI TTY）+ NonInteractivePrompter（非 TTY）+ CallbackPrompter（SDK 闭包，T-M4-11）
├── redact.rs              # 敏感数据脱敏（.env/api_key/password 模式替换，T-M4-11，C-04）
├── ssrf.rs                # SSRF 防护（RFC1918/链路本地/回环/CGNAT 拒绝，T-M4-11，C-02）
├── replay.rs              # ReplayPolicy（replay 模式禁副作用，C-06）
└── path_sandbox.rs        # sandbox_path 路径校验（应用层第一道防线，见 security.md §3）
```

> **说明**：`Prompter` 各实现集中在单文件 `prompter.rs`（非子目录），因为实现体量较小且共享 `PermissionPrompter` trait 与辅助逻辑；`TuiPrompter` 将在 M7 拆出独立模块。决策持久化（`policy.toml` AllowAlways/DenyAlways）暂未在 policy crate 实现，由 `minicoding-core::policy` trait 抽象 + 调用方注入，M5+ 补 `store.rs`。

### 3.3 关键设计点

- **黑名单最高优先级**：`builtin.rs` 硬编码危险命令/SSRF/敏感路径，任何用户配置与 Hook 都无法覆盖（C-02）。
- **ApprovalMode × SandboxPolicy 正交**：`mode.rs` 解析预设并展开为默认 `Verdict` 与 `SandboxPolicy`（后者传给 sandbox crate）。
- **prompter 独立**：决策（policy）与交互（prompter）分离，解决 broadcast 事件总线无法承载点对点回复的架构缺陷（见 design.md §9.1）。`CallbackPrompter`（T-M4-11）为 M8 SDK 提供闭包注入入口。
- **AGENTS.md 写保护**：`builtin.rs` 对 `AGENTS.md`/`CLAUDE.md` 写操作注入 `Verdict::Ask` 且不可 `AllowAlways`（C-23）。
- **敏感数据脱敏（T-M4-11）**：`redact.rs` 在 `fs.read` 读取 `.env`/凭证/`*.pem` 等敏感文件时把 `KEY=value`/`KEY: value`/`Bearer xxx`/`AKIA…` 模式替换为 `***`，避免回灌 LLM 或落 jsonl（C-04）。字段名归一化（`-`/空白 → `_`）后匹配关键词，覆盖 `kebab-case`/`snake_case` 命名差异。
- **SSRF 防护（T-M4-11）**：`ssrf.rs` 提供 `check_url`/`check_host`/`check_ip`，拒绝 RFC1918 私网、链路本地 `169.254/16`（云元数据）、回环 `127/8`/`::1`、CGNAT `100.64/10`、`0.0.0.0/8`、IPv6 ULA `fc00::/7`。用 `Url::host()` 而非 `host_str()` 取主机，避免 IPv6 字面量带方括号解析失败。`SsrfOptions::local_dev()` 允许回环（本地 Ollama）。
- **依赖**：`minicoding-core` + `regex`（黑名单/脱敏）+ `url`（SSRF 主机解析，轻量无 IO）+ `toml`/`serde`（policy.toml）+ `camino`。

---

## 4. `minicoding-memory`（记忆实现）

### 4.1 职责

实现 `ProjectDocLoader`/`MemoryStore` trait（定义在 core）：长期记忆双文件、Auto memory 自动学习、会话摘要、AGENTS.md 分层加载。

### 4.2 模块树

> 2026-08-29 R8 审查更正：实际另有 `auto_contributor.rs`（M-09 注入 contributor）、
> `retrieval.rs`（M-08 `@memory` BM25 检索）与 `project_doc/inject.rs`，
> 树形描述同步如下：

```
minicoding-memory/src/
├── lib.rs                 # 工厂
├── long_term.rs           # 长期记忆双文件（long_term.md + index.json）+ mtime 缓存
├── auto.rs                # Auto memory（auto.md + auto.index，启发式检测，置信度淘汰）
├── session_sum.rs         # 会话摘要 + 失败降级链（与 context::fallback 同构）
├── project_doc/
│   ├── mod.rs
│   ├── loader.rs          # AGENTS.md 分层加载算法（见 design.md §8.6）
│   ├── fallback.rs        # fallback 文件名与 override 解析（CLAUDE.md/.cursorrules）
│   └── inject.rs          # 项目文档注入 system 段（R8 起含 AGENTS.md，C-05 边界）
├── vector.rs              # `@memory` BM25 语义检索（CJK 逐字分词，零外部依赖）
├── retrieval.rs           # MemoryRetrieval（auto+long_term 语料组装与检索，M-08）
├── auto_contributor.rs    # AutoMemoryContributor（prompt pipeline contributor，B2/B3）
└── inject.rs              # 记忆注入 system 段（包裹 <long_term_memory>/<auto_memory> 边界）
```

### 4.3 关键设计点

- **Auto memory 物理隔离**：`auto.md` 与 `long_term.md` 分离存储，对 `long_term.md` 写入走 `Ask`，对 `auto.md` 隐式写入 `Allow`（C-27）。
- **指令性内容检测**：`auto.rs` 检测 `auto.md` 中含 `AGENTS.md` 风格指令性内容时降级 `Ask`（防绕过 C-23）。
- **mtime 缓存**：`long_term.rs` 用 mtime 判断文件变更，无变更零 IO/分词（M-04）。
- **依赖**：仅 `minicoding-core` + `serde`/`serde_json`/`camino`/`time`。摘要需调 LLM，通过 trait 注入；摘要落盘经 `Storage::update_summary`（A7：不再直接依赖 `minicoding-storage`）。

---

## 5. `minicoding-hooks`（Hooks 实现）

### 5.1 职责

实现 `Hook`/`HookRegistry` trait（定义在 core）：Hook 注册与串行聚合、ScriptHook 适配器、asyncRewake 异步唤醒、内置示例 Hook。

### 5.2 模块树

```
minicoding-hooks/src/
├── lib.rs                 # 工厂 + re-export
├── registry.rs            # HookRegistryImpl 实现 HookRegistry（串行聚合，见 hooks.md §5）
├── dispatch.rs            # dispatch 算法（事件 → 匹配 hook → 聚合结果，R8 审查补列）
├── script.rs              # ScriptHook 适配器（外部可执行 + JSON over stdio + 退出码语义）
├── async_rewake.rs        # asyncRewake 异步唤醒管理（后台任务 + 唤醒注入，见 hooks.md §11）
├── builtin.rs             # 6 个内置示例 Hook（FmtOnWrite/AutoApproveTests/BlockSecrets/
│                          #   GitStatusInject/BackupBeforeCompact/TestOnStop）
└── protocol.rs            # HookInput/HookOutput JSON 编解码 + 退出码映射
```

### 5.3 关键设计点

- **10 类事件**：`SessionStart`/`UserPromptSubmit`/`PreToolUse`/`PostToolUse`/`PostToolUseFailure`/`PreCompact`/`PostCompact`/`Stop`/`SubagentStop`/`PermissionRequest`（见 hooks.md §2）。
- **asyncRewake 后台进程同等待遇**：后台 Hook 子进程遵守凭证隔离（C-04）、沙箱（C-22）、路径沙箱（C-03）约束（C-26）。
- **L0 不可覆盖**：`registry.rs` 在分发 Hook 前先应用内置黑名单 `Deny`，Hook 的 `allow` 对黑名单 `Deny` 无效（C-21）。
- **依赖**：`minicoding-core` + `tokio`（子进程）+ `serde_json`（协议）。matcher 的工具名 glob 在 core 用轻量实现（不引入 `globset` 重依赖到 core）。

---

## 6. `minicoding-journal`（文件改动事务与回滚）

### 6.1 职责

实现 `Journal` trait（定义在 core）：会话内文件改动账本、`/undo` operation 级回滚、`/new` 会话级重置、冲突检测。

### 6.2 模块树

> 2026-08-29 R8 审查更正：实际为双文件布局（entry/undo/report 已并入
> journal_impl.rs），树形描述同步如下：

```
minicoding-journal/src/
├── lib.rs                 # re-export
└── journal_impl.rs        # FileChangeJournal 实现 Journal（账本 + undo 冲突检测
                           #   + UndoReport，见 design.md §17.4；R8：恢复路径
                           #   symlink 组件级校验，C-03/C-28）
```

### 6.3 关键设计点

- **不落盘**：journal 含文件原文，落盘等于多存一份敏感数据，故仅驻留内存、会话结束即销毁（C-28）。
- **冲突检测不强行覆盖**：恢复前比对当前文件内容与 `after`，不一致记入 `failed_files`（C-28）。
- **特性门控**：`file-undo` feature 默认关闭，开启时由 Runtime 持有。
- **依赖**：`minicoding-core` + `camino` + `sha2`（hash 比对）。

---

## 7. `minicoding-sandbox`（OS 级沙箱驱动，基于主流库）

### 7.1 职责

实现 `SandboxDriver` trait（定义在 core）：自研轻量驱动提供跨平台内核级隔离——Linux `landlock` 直连（`Command::pre_exec` 在子进程 fork 后 exec 前应用）+ `libseccomp` 可选（feature gate `seccomp`，默认关，deny-list 过滤）、macOS `sandbox_init`(3) FFI、Windows Job Object。原 ~~`sandbox-run`~~ 选型因 EUPL-1.2 许可证不合规弃用（见 `tech-stack.md` §13）。另实现 `SandboxDenialDetector`/`SandboxDenialTracker` trait（定义在 core，M-05 下沉）：平台 denial 签名库 + 单 turn 熔断（C-30）。

### 7.2 模块树

```
minicoding-sandbox/src/
├── lib.rs                 # detect_driver() 工厂：按 cfg!(target_os) 选实现
├── driver.rs              # SandboxDriverImpl 实现 trait
├── denial.rs              # DenialDetector + PLATFORM_SIGNATURES + SandboxCircuitBreaker（C-30，M-05 从 core 下沉）
├── linux.rs               # Linux: landlock 直连（pre_exec 应用）
├── seccomp.rs             # Linux syscall 过滤（libseccomp，feature gate `seccomp` 默认关）
├── macos.rs               # macOS: sandbox_init(3) FFI（Seatbelt，profile 临时文件 + pre_exec）
├── windows.rs             # Windows: windows-sys（Job Object + 受限令牌）
├── external.rs            # ExternalSandbox 驱动（容器内运行，is_hardened()=false，C-22）
└── hardening.rs           # pre-main 进程硬化（PR_SET_DUMPABLE/RLIMIT_CORE/清 LD_*）
```

### 7.3 库选型

| 平台 | crate | 版本 | 理由 |
|------|-------|------|------|
| 跨平台统一 API | 自研轻量驱动（~~`sandbox-run`~~ 已弃用） | - | 原 `sandbox-run`（systemd 风格 API）因 EUPL-1.2 许可证不合规弃用；现为自研 pre_exec 胶水：Linux landlock 直连、macOS `sandbox_init`(3) FFI、Windows Job Object（见 `tech-stack.md` §11/§13） |
| Linux 文件系统沙箱 | `landlock` | 0.4.5 | 官方 rust-landlock，Landlock LSM 安全抽象，纯 Rust 无 C 依赖，1260 万下载 |
| Linux syscall 过滤 | `libseccomp`（已接，opt-in） | 0.4 | seccomp deny-list（禁 ptrace/mount/reboot/kexec_load 等，2026-08-29 R8 审查更正：已接线，feature gate `seccomp` 默认关）；需系统 libseccomp C 库 |
| Windows | `windows-sys` | 0.59 | Job Object + 受限令牌 |
| 进程硬化 | `libc` | - | PR_SET_DUMPABLE/RLIMIT_CORE |

> **选型变更**：原方案曾以"不自研胶水"为由选用 `sandbox-run` 统一 API，后因 EUPL-1.2 许可证不合规弃用，改为自研轻量 pre_exec 胶水——仅封装子进程启动路径（fork 后 exec 前应用约束），Landlock ruleset 构建仍由 `landlock` crate 提供。权衡记录见 `tech-stack.md` §13。

### 7.4 关键设计点

- **平台检测**：`detect_driver()` 编译期按 `cfg!(target_os)` 选实现；运行期 `landlock_available()` 探测内核支持（Linux，`HardRequirement` + `create()` 探测不约束当前进程），不支持则返回 `NoopDriver`（来自 core）并 warn。
- **VCS 目录保护**：`.git`/`.hg`/`.svn` 纳入只读规则（P-20）；landlock"白名单并集"语义下 workdir 可写会使子目录继承可写，故实际硬保护由应用层 builtin 黑名单补偿（S5）。
- **pre-exec apply**：自研胶水经 `Command::pre_exec` 在子进程 fork 后 exec 前应用约束（Linux `restrict_self()` / macOS `sandbox_init`），子进程启动即受限，无窗口期（参考 Codex，见 security.md §8.3）。
- **依赖隔离**：`landlock` 通过 `[target.'cfg(target_os = "linux")'.dependencies]` 条件引入，非 Linux 不编译；macOS 无外部 crate 依赖（FFI 直连 libsystem）；`libseccomp` 同条件引入（feature gate `seccomp`，默认关，见 §0.4）。
- **denial 领域实现（M-05）**：`DenialDetector`（子串匹配 `PLATFORM_SIGNATURES`，覆盖 EPERM/EACCES/Landlock/seccomp/Seatbelt/Windows）与 `SandboxCircuitBreaker`（计数熔断，阈值 3/5）实现 core 的 trait 抽象；未启用本 crate 时 core 兜底 `NoopDenialDetector`/`NoopDenialTracker`（不识别沙箱拒绝，与 NoopDriver 语义一致）。CLI（`sandbox` feature）/server 在注入 `SandboxDriver` 的同时注入 denial 实现。
- **依赖**：`minicoding-core` + `landlock`（Linux）+ `libseccomp`（Linux，opt-in feature）+ `libc`（Linux）+ `windows-sys`（Windows）；macOS FFI 直连无外部 crate。

---

## 8. `minicoding-mcp`（MCP client/server，基于官方 rmcp）

### 8.1 职责

实现 `McpClient` trait（定义在 core）：基于官方 `rmcp` 2.x SDK 连接外部 MCP server、list_tools、call、shutdown；亦提供 `serve --as-mcp-server` 暴露自身工具。

### 8.2 模块树

```
minicoding-mcp/src/
├── lib.rs                 # re-export + build_client() 工厂
├── client/
│   ├── mod.rs
│   ├── rmcp.rs            # RmcpClient：基于 rmcp 2.x 的实现（stdio，含进程池复用 +
│   │                      #   inflight 并发合并 + 预热，X-12/13/14）
│   └── wrapper.rs         # McpToolWrapper：把远程工具包装为 minicoding Tool（含 mcp.call span）
├── server/                # T-M8-3：MCP server 暴露侧（把内置工具暴露为 MCP server）
│   ├── mod.rs             # 模块声明 + re-export `ToolExposer`/`serve_as_mcp_server`
│   ├── expose.rs          # ToolExposer 实现 ServerHandler，serve_as_mcp_server 启动 stdio server
│   └── tool_search.rs     # BM25 工具检索索引（X-09，工具数多时按自然语言查 top-k）
├── config.rs              # McpConfig（mcp.json 解析 + 环境变量展开）
├── approval.rs            # project 作用域首次批准流（mcp_choices.toml，C-24）
└── naming.rs              # mcp__<server>__<tool> 命名 + 解析 + 权限通配匹配
```

> **R8 审查更正**：原注"进程池增强/后台预热/inflight 未实现（M6+）"已过时——
> X-12（进程池）/X-13（预热）/X-14（inflight merge）均已实现于 `client/rmcp.rs`；
> `config.rs`/`server/tool_search.rs` 同为本模块实际文件。T-M8-3（`server/expose.rs`）
> 已交付：CLI `minicoding serve --as-mcp-server` 把内置工具通过 MCP stdio 协议暴露
> 给外部 client（如 Claude Desktop）。

### 8.3 库选型（不自研）

| crate | 版本 | 理由 |
|-------|------|------|
| `rmcp` | 2.2 | 官方 Rust MCP SDK（modelcontextprotocol/rust-sdk），对齐 MCP 2025-11-25 spec，支持 stdio + streamable HTTP + OAuth + `#[tool]` 宏 + schemars JSON Schema 生成 |

> **不再"自实现 stdio 薄封装"过渡方案**：原计划的"M4 先自实现 stdio 薄封装，M5+ 升级 rmcp"废弃。rmcp 2.x 已稳定且官方维护，直接用 `transport-child-process`（client stdio）+ `transport-io`（server stdio）+ `transport-streamable-http-client-reqwest`（HTTP client with rustls）。

### 8.4 关键设计点

- **前置理由**：MCP 是 AI Coding 工具生态关键接入点，前置到 M4，直接用 rmcp 完整实现。
- **工具命名**：`mcp__<server>__<tool>`（见 design.md §19.3），与权限规则通配匹配兼容。
- **project 作用域批准**：首次遇到含 `.minicoding/mcp.json` 的仓库时逐个 server 弹窗，防恶意仓库植入（C-24）。
- **凭证隔离**：MCP server 子进程不继承 minicoding 凭证环境变量（C-04）。
- **`required` 语义**：`required = true` 的 server 启动失败则 minicoding 拒绝启动；`required = false`（默认）失败仅 warn 跳过。
- **进程池复用**：MCP server 连接跨 turn 复用，不每 turn 重启（见 `design.md` §19.5）。M4 交付基础复用（`RmcpClient` 持有子进程 handle）；
- **后台预热**（M6+）：全局 server（`~/.minicoding/mcp.json`）启动时预热；项目级 server 创建/resume session 时后台预热，首 turn 仅在后台预热未完成时阻塞；
- **inflight merge**（M6+）：同 server 并发请求合并，避免重复调用。
- **OTel `mcp.call` span**（T-M5-8，O-08）：`McpToolWrapper::execute` 内开 `mcp.call` span（`mcp.server`/`mcp.tool` 属性），便于 collector 侧聚合 MCP 调用延迟。
- **依赖**：`minicoding-core` + `rmcp` 2.2（features: `client`/`macros`/`transport-child-process`；`server`/`transport-streamable-http-client-reqwest` 留 M6/M8）+ `tokio`。

---

## 9. `minicoding-storage`（存储与审计）

### 9.1 职责

实现 `Storage`/`AuditSink`/`EventStore`/`SnapshotStore` trait（定义在 core）：JSONL 会话日志、会话索引、跨进程文件锁、审计日志、事件流持久化与 snapshot（Event Sourcing，见 `design.md` §25）。另实现事件重放 `replay_session_state`/`session_from_messages`（M-05 从 core 下沉，含悬空 `tool_calls` 修复防御，见 M-03）。

### 9.2 模块树

```
minicoding-storage/src/
├── lib.rs                 # 工厂
├── jsonl.rs               # JsonlStorage 实现 Storage（追加写、崩溃安全）
├── event_store.rs         # JsonlEventStore 实现 EventStore（{id}.events.jsonl，fsync 后返回）
├── snapshot_store.rs      # JsonlSnapshotStore 实现 SnapshotStore（{id}.snapshot.json，原子写）
├── index.rs               # 会话索引 index.json（轻量元数据列出）
├── lock.rs                # 跨进程文件锁（fs2）
├── audit.rs               # AuditSink 实现（audit.log JSONL，0600 权限）
├── replay.rs              # replay_session_state + ReplayError + ReplayedSession + session_from_messages（M-05 下沉）
└── export.rs              # 会话导出（md / jsonl）
```

### 9.3 关键设计点

- **崩溃安全**：每条消息 `append` 后 `fsync`，崩溃时磁盘与内存一致。事件流（`{id}.events.jsonl`）同样 append + fsync；snapshot（`{id}.snapshot.json`）走 `.tmp` + `rename` 原子写。
- **审计完整性**：`audit.log` 文件权限 0600，追加写不可篡改历史（无 update/delete API）。
- **Event Sourcing 双写并存**：新会话同时写消息日志与事件流，旧会话无事件流时回退到消息日志路径（见 `design.md` §25.6）。`JsonlEventStore`/`JsonlSnapshotStore` 与 `JsonlStorage` 共用 `sessions_dir`。
- **依赖**：`minicoding-core` + `serde_json` + `fs2`（文件锁）+ `tracing`。

---

## 10. `minicoding-providers`（LLM Provider 实现）

### 10.1 职责

实现 `LlmProvider` trait（定义在 core）：OpenAI 兼容、Anthropic、Ollama；提供对应 Tokenizer。

### 10.2 模块树

> 2026-08-29 R8 审查更正：实际代码为扁平文件布局（非目录分组），
> 树形描述同步如下：

```
minicoding-providers/src/
├── lib.rs                 # re-export + build_provider() 工厂
├── openai.rs              # OpenAI 兼容 Provider（含 request/response/模型探测）
├── anthropic.rs           # Anthropic Provider（含 request/response/thinking 预算）
├── ollama.rs              # Ollama Provider（原生工具调用 + NUM_CTX）
├── tokenizer.rs           # tiktoken-rs 封装
└── common/
    ├── mod.rs             # 共享工具（wrap_tool_output、mask_key 等）
    ├── credential.rs      # CredentialResolver（TTL 缓存 + 重载，C-04）
    ├── ndjson.rs          # NDJSON 流解析
    ├── retry.rs           # 重试策略（指数退避、429 Retry-After、splitmix64 抖动）
    ├── sse.rs             # SSE 流解析
    └── stream_runner.rs   # 流式响应统一跑批（token 追踪/usage 聚合）
```

### 10.3 关键设计点

- 每个 provider 内部统一返回 `BoxStream<Result<Delta>>`，转换逻辑隔离。
- 密钥从环境变量或 OS keyring 读取，绝不接受配置文件明文。
- 重试与超时在 `common::retry` 统一实现，装饰器包裹 stream。
- **独立小 LLM**：`small_llm.rs` 支持为摘要 / compact / memory 提取配置独立 provider（`[provider.small]`），未设置时与主 provider 相同，可配更便宜模型降本。
- **依赖**：`minicoding-core` + `reqwest`（rustls-tls）+ `eventsource-stream` + `tiktoken-rs` + `serde`/`serde_json`。

---

## 11. `minicoding-tools`（内置 Tool 实现，组合层）

### 11.1 职责

实现内置 `Tool` 集合；作为"组合层"，可依赖多个领域 crate（context/policy/memory/hooks/journal/sandbox/mcp/storage）以完成工具执行闭环。

### 11.2 模块树

```
minicoding-tools/src/
├── lib.rs                 # register_all() 工厂
├── fs/
│   ├── read.rs
│   ├── write.rs           # 成功后调 Journal::record（若启用）
│   ├── edit.rs            # 精确字符串替换 + Journal::record
│   ├── multiedit.rs       # 同文件多次顺序替换（原子性）
│   ├── delete.rs          # + Journal::record
│   ├── list.rs
│   ├── glob.rs            # globset + ignore
│   ├── grep.rs            # regex + ignore
│   └── journal_helper.rs  # 工具→Journal entry 构造（R8 审查补列）
├── shell/
│   ├── run.rs             # tokio::process + 超时 + 截断 + SandboxDriver::apply
│   ├── background.rs      # 启动后台命令返回 shell_id
│   ├── output.rs          # 读取后台命令累积输出
│   └── kill.rs            # 终止后台命令
├── web/
│   ├── fetch.rs           # reqwest + html→markdown + SSRF 防护
│   ├── search.rs          # DuckDuckGo HTML 端点（无需 API key）
│   └── ssrf.rs            # URL/IP 校验：拒绝私有/loopback/link-local IP
├── git/
│   ├── diff.rs
│   └── apply.rs
├── task/
│   ├── spawn.rs           # 类型化子 Agent 派发
│   ├── create.rs          # TaskCreate（增量模型，见 design.md §18）
│   ├── update.rs          # TaskUpdate（增量 + 依赖 + 状态机）
│   └── list.rs            # TaskList 快照
├── memory/
│   ├── mod.rs             # 记忆工具装配
│   └── write.rs           # memory.write（写入 long_term/auto，经权限+审计）
├── worktree.rs            # WorktreeSubagentRunner（git worktree 隔离装饰器，M-05 从 core 下沉）
├── plan/
│   ├── exit.rs            # ExitPlanMode（见 design.md §16.4）
│   └── list.rs            # PlanList 快照（M-11 新增，只读，穿透 Plan 硬门）
├── ui.rs                  # ui.ask（交互确认，T-12a）
└── util.rs                # 输出截断/格式化 + diff 生成（单文件，非目录）
```

### 11.3 关键设计点

- **路径沙箱委托**：`util::path` 调用 `minicoding-policy::path_sandbox::resolve_under`，不重复实现。
- **shell.run**：执行前调 `SandboxDriver::apply`（来自 `minicoding-sandbox`）应用 OS 沙箱。
- **fs.read 敏感文件脱敏（T-M4-11，C-04）**：`fs::read::is_sensitive_path` 识别 `.env`/`credentials`/`*.pem`/`*.key`/`*.pfx`/`*.p12` 及文件名含 `secret`/`password`/`token` 的文件，调用 `minicoding_policy::redact` 把字段值替换为 `***` 再返回，避免密钥回灌 LLM。
- **fs.write/edit/delete + Journal**：成功后调 `Journal::record`（来自 `minicoding-journal`），仅 `file-undo=true` 时生效。
- **task.create/update/list**：增量模型，状态机 `Pending→InProgress→Completed` 不可跳跃（C-31）。
- **task.spawn（T-M5-7，T-13）**：启动类型化子 Agent（`SubagentType::Explore/Plan/GeneralPurpose/Custom`），隔离上下文（独立 ContextManager），Plan 模式下被硬门拒绝（`SideEffect::None` 仍受 `PermissionMode::Plan` 约束）。OTel `subagent` span 挂在父 turn span 下（O-04）。子 Agent env 不含凭证（C-04）。
- **plan.exit（T-M5-6，T-15）**：退出 Plan 模式并提交计划，切回 Default 模式并缓存 `allowed_prompts`（预批准），避免 ExitPlanMode 后逐条重新确认。Plan 模式硬门用 `is_read_only()` 判断（C-25）。
- **plan.list（M-11，T-15b）**：只读查询 `PlanModeController::snapshot()` 的 `mode` + `allowed_prompts`，`render_output` 投影为表格（tool/prompt），空预批准回落 JSON；`is_read_only() = true` 穿透 Plan 硬门（C-25），便于模型在执行期自查当前模式与预批准命令。
- **M-11 渲染声明（R-05，T-19）**：`Tool` trait 的 `output_schema()`/`render_output()` 由各工具实现（见 `design.md` §4.1）；本 crate 全部内置工具已补充，前端按工具名本地渲染（零协议改动）。
- **mcp::wrapper**：把 `McpServerConfig` + 远程 schema 包装为 `Tool`，`side_effect` 据 `readOnlyHint`/`destructiveHint` 映射（C-25）。
- **worktree.rs（M-05）**：`WorktreeSubagentRunner` 装饰器实现 `SubagentRunner`（trait 在 core），`Isolation::Worktree` 时 `git worktree add` 建隔离目录、按 `merge_back` 合并、`auto_cleanup` 清理，非 git 仓库降级 `Shared`（A-15）。
- **依赖**：`minicoding-core` + `minicoding-policy`（路径沙箱 + 脱敏）+ 按需依赖 context/memory/hooks/journal/sandbox/mcp/storage（optional）+ `globset`/`ignore`/`regex`/`reqwest`。

---

## 12. `minicoding-cli`（CLI frontend）

### 12.1 职责

命令行入口；解析参数、加载配置、构建 Runtime、驱动会话、渲染输出。

### 12.2 模块树

```
minicoding-cli/src/
├── main.rs
├── args.rs               # clap derive 定义
├── app.rs                # App 主控
├── config_loader.rs      # 分层配置加载
├── builder.rs            # RuntimeBuilder 组装（按 feature 选实现 crate）
├── render/
│   ├── mod.rs
│   ├── stream.rs         # 流式 token 渲染
│   ├── tool.rs           # 工具调用渲染
│   └── prompt.rs         # 权限确认提示
├── session/
│   ├── mod.rs
│   ├── interactive.rs    # 交互 REPL（含 /undo /plan /mcp /memory）
│   └── resume.rs         # 会话恢复
├── commands/
│   ├── exec.rs           # minicoding exec（非交互批量 + 沙箱策略）
│   ├── doctor.rs         # minicoding doctor --security 自检
│   ├── audit.rs          # minicoding audit list/stats
│   ├── mcp.rs            # minicoding mcp list/approve/reset-project-choices
│   ├── cred.rs           # minicoding cred store/load/delete（T-M4-11）
│   ├── serve.rs          # minicoding serve（HTTP/SSE server，T-M8-2，`serve` feature）
│   ├── backup.rs         # minicoding backup create/list（S-05，tar.gz 打包）
│   └── session_cmd.rs    # minicoding session list/delete（T-M3-10c）
└── cred.rs               # 凭证存储（OS keyring + 文件 fallback 0600，T-M4-11，C-04）
```

### 12.3 关键设计点

- **零业务逻辑**：所有决策委托 Runtime；CLI 只做 IO 与渲染。
- **feature 组装**：`builder.rs` 根据 cargo feature 启用的实现 crate 装配 Runtime（如未启用 `minicoding-sandbox` 则用 core 的 `NoopDriver`）。
- **非 TTY 降级**：检测 `stdout.is_terminal()`，非交互时禁 spinner/颜色，权限走 `NonInteractivePrompter`。
- **凭证管理（T-M4-11，C-04）**：`cred.rs` 实现 OS keyring 优先 + 文件 fallback（`~/.minicoding/credentials` 0600 权限 + 原子 rename）的凭证存储；`minicoding cred store/load/delete` 子命令从 stdin 读取 key（不回显），`load` 不打印 key 本身只验证存在性。keyring 不可用时降级并打 warn 日志。
- **Hook 加载（T-M5-8，H-01）**：`builder.rs::build_hook_registry` 从 `config.hooks`（`.minicoding/hooks.toml`）把每个 `HookEntry` 转为 `ScriptHook`（matcher 解析为 `HookMatcher::for_tools`/`for_events`），注册到 `HookRegistryImpl`。`hooks` feature 未启用时退化为 `NoopHookRegistry`。
- **Plan 模式（T-M5-8，A-06）**：`--plan` 启动时初始 `PermissionMode::Plan`（副作用工具被硬门拒绝）；REPL `/plan [on|off|status]` 切换模式。`plan.exit` 与 `task.spawn` 工具在 Runtime 构造后通过 `register_dynamic_tool` 补注册（chicken-and-egg：tools 需 Runtime 引用，Runtime 需 tools）。
- **/undo REPL（T-M5-8，A-10）**：`/undo` 调 `Journal::undo(1)` 回滚最近一次 operation。`file-undo` feature 未启用或 journal 未注入时打印提示；回滚结果（成功/冲突）打印到 stderr。
- **serve 子命令（T-M8-2，`serve` feature）**：`minicoding serve` 启动 HTTP/SSE server，等价于独立运行 `minicoding-server`，但通过 CLI 统一入口。`commands/serve.rs` 定义 `ServeCommand`（clap 参数：`--bind`/`--port`/`--provider`/`--api-base`/`--api-key`/`--model`/`--workdir`/`--system`/`--permission-timeout-sec`），`run_serve_command` 构造 `ServerConfig` 委托 `minicoding_server::serve`（阻塞当前 task）。feature gate `serve` 默认关闭，启用时引入 `minicoding-server` 依赖。HRTB 兼容见 §16.3 与 `design.md` §24。
- **退出码**：成功 0；运行时错误 1；配置错误 2；中断 130。
- **依赖**：`minicoding-core` + 各实现 crate（按 feature）+ `clap`/`indicatif`/`anstream`/`rustyline`/`keyring`/`anyhow`。

---

## 13. `minicoding-tui`（TUI frontend，M7）

### 13.1 职责

基于 `ratatui` 的全屏交互界面：多会话、工具调用面板、权限弹窗、流式 Markdown 渲染。

### 13.2 模块树

```
minicoding-tui/src/
├── main.rs
├── app.rs                # App 状态机（PanelMode / InputState / ChatLine）
├── event.rs              # AppEvent：终端事件 + Runtime 事件 + TurnResult + PermissionRequest + SwitchSession
├── view/
│   ├── chat.rs           # 对话主视图（流式 Markdown + 历史 + 工具调用行）
│   ├── sidebar.rs        # 多会话侧栏（F2 切换，当前会话高亮）
│   ├── tool_panel.rs     # 工具调用进度面板（F3 切换，进行中/已完成）
│   └── task_panel.rs     # 任务列表面板（Ctrl+T 切换，订阅 Event::TaskUpdated）
├── render/
│   ├── markdown.rs       # 流式 Markdown 增量解析（行级 + inline span）
│   └── theme.rs          # 主题配色（Theme struct，深/浅色预设，集中颜色管理）
└── runtime_bridge.rs     # 与 Runtime 的 channel 桥接（current_thread + LocalSet）
```

### 13.3 关键设计点

- 独立线程跑 Runtime（`current_thread` tokio + `LocalSet`，因 `run_turn` future 非 `Send`），UI 主线程同步事件循环。
- 权限弹窗非阻塞：Runtime 在 `Verdict::Ask` 时通过 `TuiPrompter`（点对点 mpsc channel）挂起该工具调用，UI 处理后通过 oneshot 回传 `Decision`。
- 会话切换通过重建 Runtime 实现：`App::pending_switch` → `main.rs` 重建（`SessionLoadMode::Resume`），避免给 Runtime 加锁。
- **T-M7-4 面板互斥**：F3 工具面板 / Ctrl+T 任务面板共享底部区域，`PanelMode` enum（Off/Tool/Task）互斥切换；`Event::TaskUpdated` 到达时自动切换到 Task 模式。
- **主题配色**：`Theme` struct 集中管理所有颜色，视图模块按语义取色（`theme.user_prefix` 等），`F4` 在深/浅色预设间切换。
- **依赖**：`minicoding-core` + `minicoding-policy`（TuiPrompter）+ `minicoding-cli`（builder）+ `minicoding-storage`（会话列表）+ `ratatui`/`crossterm`/`tokio`/`time`。

---

## 14. `minicoding-sdk`（嵌入 SDK，M8）

### 14.1 职责

为第三方 Rust 程序提供高层嵌入 API，隐藏 Runtime 细节。

### 14.2 公共 API

```rust
pub struct Client { runtime: Runtime }

impl Client {
    pub fn builder() -> ClientBuilder;
    pub async fn ask(&self, prompt: &str) -> Result<String>;
    pub async fn ask_stream(&self, prompt: &str) -> impl Stream<Item = Result<Delta>>;
    pub async fn run_task(&self, task: &str) -> Result<TaskReport>;
    pub fn on_event(&self, f: impl Fn(Event)) -> Subscription;
}
```

### 14.3 关键设计点

- 默认无副作用权限策略，调用方需显式启用。
- 提供 `CallbackPrompter`（来自 `minicoding-policy`）供 SDK 用户闭包处理权限交互。
- 所有 API `Send + Sync`，可在多 tokio 任务中共享。
- **依赖**：`minicoding-core` + `minicoding-policy`（CallbackPrompter）+ 必要实现 crate。

---

## 15. `minicoding-protocol`（前后端协议契约）

### 15.1 职责

定义 JSON-RPC 2.0 wire types + Event/Command DTO，独立于实现 crate。CLI / TUI / HTTP Server / ACP 适配器 / LSP 适配器共用此 crate 的线协议类型。

### 15.2 模块树

minicoding-protocol/src/
├── lib.rs                 # re-export
├── jsonrpc.rs             # JSON-RPC 2.0 wire types（Request/Response/Notification/Error）
├── event.rs               # Event DTO（从 core::Event 映射，携带 seq: u64）
├── command.rs             # Command DTO（前端→后端命令：CreateSession/SendMessage/Cancel 等）
├── cursor.rs              # SSE cursor 恢复（event seq 单调递增）
└── rehydrate.rs           # RehydrateRequired 信号（broadcast 溢出时通知客户端重拉 snapshot）

### 15.3 关键设计点

- **协议与实现解耦**：wire types 集中在此 crate，server/cli/tui 共用；
- **cursor 恢复**：SSE 流携带 cursor（event seq），客户端断连后从 cursor 恢复；
- **Rehydrate 信号**：broadcast 溢出时发 RehydrateRequired，客户端重拉 snapshot；
- **依赖**：`minicoding-core` + `serde`/`serde_json`。无 HTTP/server 依赖。

---

## 16. `minicoding-server`（HTTP/SSE server + ACP/LSP 适配器）

### 16.1 职责

提供 HTTP/SSE JSON-RPC 2.0 接口，支持多客户端并发会话；ACP stdio 适配器可被支持 ACP 的客户端（如 Zed）嵌入；LSP stdio 适配器可被任何支持 LSP 的编辑器（VS Code/Neovim/Emacs/Helix 等）嵌入。

### 16.2 模块树

minicoding-server/src/
├── main.rs                # minicoding-server 入口（独立二进制）
├── lib.rs                 # re-export + `serve(cfg)` 入口（供 CLI `serve` feature 调用）
├── http.rs                # Axum HTTP/SSE handler + `ServerConfig` + `serve(cfg)`
├── workspace.rs           # 项目工作区端点（W-11：root/list/read/diff/switch，见 design.md §26.9）
├── session_mgr.rs         # `SessionManager` 多会话管理（HTTP path 带 session_id）
├── runtime_builder.rs     # `ServerRuntimeParams` + `build_runtime`（构造单会话 Runtime；默认注入 FileChangeJournal）
├── prompter.rs            # `ServerPrompter`（实现 `PermissionPrompter`，oneshot + 超时）+ `PendingPermissions`
├── sse.rs                 # SSE 流 + cursor 恢复
├── otel_init.rs           # OTLP 初始化（镜像 cli/otel_init.rs，feature gate `otel` 默认启用，见 observability.md §7.3）
├── acp.rs                 # ACP stdio 适配器（JSON-RPC over stdio，T-M8-7）
├── lsp.rs                 # LSP stdio 适配器（tower-lsp，语义映射见 design.md §24，feature gate `lsp`）
├── lsp_prompter.rs        # LspPrompter：实现 PermissionPrompter（window/showMessageRequest 点对点权限交互）
└── rehydrate.rs           # RehydrateRequired 处理（通知客户端重拉 snapshot）

### 16.3 关键设计点

- **SSE cursor 恢复**：事件流携带 cursor，客户端断连后从 cursor 恢复；
- **多会话并发**：HTTP path 带 session_id，支持多 session 并发；
- **SessionManager API 形态（HRTB 兼容）**：`create_session`/`cancel`/`get`/`list_sessions`/`delete` 为**同步方法**（`&self` 非 async），因内部仅操作 `std::sync::Mutex<HashMap>`（纯数据查/增/删，无 IO），同步可消除 `async fn(&self, ..)` 的 future 生命周期参数，避免与 axum `Handler` trait HRTB 冲突。`send_message_boxed` 是**关联函数**（非 `&self` 方法），取 `Arc<SessionManager>` owned 参数，返回的 future 无外部借用（`'static`），内部调 `Runtime::run_turn_owned`（见 `api.md` §4.1）驱动 turn。`resolve_permission`/`get_messages` 仍为 async（涉及 await）。详见 `design.md` §24 HRTB 设计说明。
- **ServerPrompter**：实现 `PermissionPrompter`，用 `PendingPermissions`（`Arc<TokioMutex<HashMap<String, oneshot::Sender<Decision>>>>`）+ 超时（默认 300s）完成点对点权限交互；HTTP `POST /permissions/{pid}` 通过 `SessionManager::resolve_permission` 投递决策。
- **ACP stdio**：作为 `minicoding serve --acp` 子模式，stdio 传输 JSON-RPC；
- **LSP stdio**：作为 `minicoding serve --lsp` 子模式，基于 `tower-lsp` 实现，把 minicoding 能力映射到 LSP 标准方法（`workspace/executeCommand`/`$/progress`/`window/showMessageRequest` 等，见 `design.md` §24 语义映射表）；`LspPrompter` 实现点对点权限交互；
- **依赖**：`minicoding-core` + `minicoding-protocol` + `minicoding-tools` + `axum`/`tower`（M6/M8 引入）+ `tower-lsp`（feature gate `lsp`，M8 引入）+ `opentelemetry` 系列（feature gate `otel`，默认启用，保证桌面 sidecar 开箱接入 OTLP，见 `observability.md` §7）。

---

## 17. `minicoding-extension-sdk`（扩展作者稳定 API）

### 17.1 职责

为第三方扩展作者提供稳定接口，隐藏 Runtime 内部细节。扩展可通过 Registrar 注册 6 类能力：工具 / Hook / Prompt contributor / 快捷键 / 状态栏项 / 斜杠命令。本 crate 同时提供 `ExtensionHost` 的进程内 first-party 实现（disk IPC 加载器在 `minicoding-cli`）。

### 17.2 模块树

```
minicoding-extension-sdk/src/
├── lib.rs                      # re-export（BundledExtensionHost/LoadedExtension/builtin_contributors/...）
├── bundled.rs                  # BundledExtensionHost（ExtensionHost 进程内实现）+ LoadedExtension
├── registrar.rs                # BundleRegistrar（Registrar 实现）+ RegistrationBundle（6 类注册项收集）
└── contributors/
    ├── mod.rs                  # builtin_contributors() 构造 9 个内置 contributor
    ├── identity.rs             # IdentityContributor（顺序 1，IDENTITY.md 覆盖，P-31）
    ├── system.rs               # SystemContributor（顺序 2，内置软规则）
    ├── task_guidelines.rs      # TaskGuidelinesContributor（顺序 3，任务规划规范）
    ├── communication.rs        # CommunicationContributor（顺序 4，输出格式规范）
    ├── environment.rs          # EnvironmentContributor（顺序 5，工作区/平台/git 信息）
    ├── user_rules.rs           # UserRulesContributor（顺序 6，long_term.md）
    ├── project_rules.rs        # ProjectRulesContributor（顺序 7，AGENTS.md）
    ├── tool_summary.rs         # ToolSummaryContributor（顺序 8，工具 schema 摘要）
    └── extension_contrib.rs    # ExtensionContributor（顺序 9，扩展段占位）
```

> `Extension`/`ExtensionHost`/`Registrar`/`ExtensionManifest` trait 定义在 `minicoding-core`（见 `api.md` §3.12），本 crate 提供 `BundledExtensionHost`/`BundleRegistrar` 进程内实现与 9 个内置 contributor。disk IPC 子进程扩展协议（`Ipc` carrier）规划在 M6+。

### 17.3 关键设计点

- **三类扩展载体**（`ExtensionCarrier` 枚举）：
  1. `Bundled`：进程内 first-party（`host.rs` 实现，M5+）；
  2. `Ipc { path }`：disk IPC 子进程（`minicoding-cli` 加载器，M6+）；
  3. `Mcp { server_id }`：远程扩展（通过 `minicoding-mcp` 包装为 Tool，M4+）。
- **统一 dispatch**：扩展注册的工具仍走 ToolRegistry dispatch 路径，确保权限审计与可观测性一致（C-01/C-02 不被绕过）；
- **能力声明**：`ExtensionManifest` 声明 id/version/name/author/carrier/capabilities/permissions/config_schema，`ExtensionHost` 启动时校验权限边界；
- **Extension-First 架构**：核心只保留 agent loop / hooks / context compaction / built-in tools；其他能力（skills / mode / goal）通过扩展接入；
- **trait 定义位置**：`Extension`/`ExtensionHost`/`Registrar`/`ExtensionManifest` trait 定义在 `minicoding-core`（见 `api.md` §3.12），本 crate 提供进程内实现；
- **依赖**：`minicoding-core` + `serde`/`serde_json`。无重依赖（HTTP/进程隔离在 `minicoding-cli` 与 `minicoding-mcp`）。

---

## 18. `minicoding-web`（Web 前端，M9，低优先级）

> **范围**：纯前端项目，独立 `package.json`，**不属于 Cargo workspace**（用 npm/pnpm 管理）。构建产物为静态资源，可被 `minicoding-server` 静态托管或独立部署到任何静态主机。技术栈选型见 `tech-stack.md` §4.1，架构设计见 `design.md` §26。

### 18.1 职责

- 提供 Web 浏览器可访问的对话 UI（流式 token、工具调用面板、权限确认弹窗、任务进度）；
- 通过 HTTP/SSE JSON-RPC 与 `minicoding-server` 通信（复用 `minicoding-protocol` wire types）；
- 不包含任何业务逻辑（Agent 循环、权限决策、压缩等均在后端）。

### 18.2 技术栈锁定

| 用途 | 选择 | 版本 |
|------|------|:---:|
| 框架 | React + React Compiler | 19.2 |
| 语言 | TypeScript | 7.0 |
| 构建 | Vite (Rolldown) | 8.1 |
| 路由 | TanStack Router | 1.170 |
| 数据获取 | TanStack Query | 5.101 |
| 客户端状态 | Zustand | 5.0 |
| Schema 校验 | Zod | 4.4 |
| 组件库 | shadcn/ui（基于 Radix UI） | latest |
| 样式 | Tailwind CSS（Oxide 引擎） | v4 |
| 动画 | Framer Motion | latest |
| Lint | oxlint | latest |
| 格式化 | oxfmt | latest |

### 18.3 目录结构

详见 `design.md` §26.2。核心分层：`api/`（JSON-RPC 客户端 + SSE 订阅 + Zod schema）→ `hooks/`（TanStack Query + Zustand 封装）→ `components/`（shadcn/ui + 业务组件）→ `routes/`（TanStack Router 页面）。

### 18.4 与后端的契约

- **协议**：HTTP/SSE JSON-RPC 2.0（见 `design.md` §24），前端不引入新协议；
- **类型同步**：`minicoding-protocol` 的 Rust DTO 通过 `ts-rs` 或 `specta` 生成 TypeScript 类型 + Zod schema，避免手写双份；
- **CORS**：`minicoding serve --cors-origin` 配置允许的前端来源（默认仅 `http://localhost:*`）；
- **能力差异声明（DOC-3/ARCH-2，2026-08-28 R5 收尾）**：server 端 Runtime 装配
  与 CLI/TUI 存在落差（见 `minicoding-server/src/runtime_builder.rs` 模块注释）——
  Web/Desktop 会话无 AGENTS.md 项目文档注入、无 Hook 注册表、无
  `git.*`/`web.*`/`memory.*`/`ui.ask` 工具、无 AutoMemory 注入与配置热更新；
  仅 readonly+write+shell+task 工具组 + `ServerPrompter` 权限交互。"四形态共享
  Runtime"指协议与聚合根复用，**能力面不相等**，前端按此预期设计；
- **工作区（W-11）**：`/sessions/{id}/workspace*` 5 个端点（见 `design.md` §26.9、`docs/api.md` §9.2）；桌面端额外经 Tauri `open_workspace_file` 命令打开系统编辑器（`main.rs`，见 §19）；新建会话可选目录（`POST /sessions` 携 `workdir`，桌面端原生目录选择器）。

---

## 19. `minicoding-desktop`（Tauri 桌面壳，M9，低优先级）

> **范围**：Tauri 2.x 桌面应用壳，**属于 Cargo workspace**（Rust 部分），前端复用 `minicoding-web` 构建产物。打包 `.dmg`（macOS）/`.msi`（Windows）/`.AppImage`（Linux）。

### 19.1 职责

- 启动 `minicoding-server` 作为 sidecar 进程（`--bind 127.0.0.1:0` 随机端口）；
- Tauri WebView 加载 `minicoding-web` 的 `dist/`；
- 提供 OS 集成：系统托盘、全局快捷键、自动更新（Tauri updater 签名校验）；
- 凭证存储复用 OS keyring（与 CLI `cred.rs` 共享 `KEYRING_SERVICE = "minicoding"`，C-04）。

### 19.2 关键设计

- **sidecar 管理**：Tauri 启动 sidecar，读取 stdout 获取实际监听端口，注入前端（`sidecar.rs`）；
- **IPC 桥接**：前端通过 Tauri `invoke('start_session')` 获取 sidecar 端口，后续通信走 HTTP/SSE（同源，无 CORS 问题）；
- **文件打开（W-11）**：`invoke('open_workspace_file', { path })` 用系统默认编辑器打开工作区文件（`app.shell().open`，Rust 侧不做路径解析，路径由前端拼接 root 绝对路径）；
- **目录选择（W-11）**：`invoke('select_workspace_dir')` 原生目录选择器（`tauri-plugin-dialog` 的 `file().pick_folder` + oneshot 桥接 async），新建会话时选定目录随 `POST /sessions` 作为 `workdir` 提交；
- **单窗口 + 文件日志**（2026-08-23 用户反馈）：`main.rs` 以 `windows_subsystem = "windows"` 编译，启动仅打开应用窗（不再弹日志控制台）；tauri-plugin-log 目标改为 `<安装目录>/logs/minicoding.log`（Webview devtools 目标保留），sidecar 子进程在 Windows 加 `CREATE_NO_WINDOW`；
- **系统托盘**（W-07）：右键菜单"显示窗口/退出"，关闭窗口时隐藏到托盘而非退出（`tray.rs`）；
- **全局快捷键**（W-07）：`Ctrl+Alt+M` 切换窗口显示/隐藏（`tauri-plugin-global-shortcut`）；
- **自动更新**：Tauri updater 配置签名公钥，更新包需签名校验通过才安装；
- **安全**：Tauri 默认禁用远程内容，仅加载本地 `dist/`；CSP 严格（`script-src 'self'`）。

### 19.3 依赖

- Tauri 2.x + `tauri-plugin-shell`（sidecar）+ `tauri-plugin-updater`（自动更新）+ `tauri-plugin-global-shortcut`（全局快捷键，W-07）+ `tauri-plugin-dialog`（目录选择，W-11）；
- `minicoding-server`（作为 sidecar 二进制，构建时通过 `tauri.conf.json` 的 `externalBin` 打包）；
- 不直接依赖 `minicoding-core`（sidecar 是独立进程，通过 HTTP/SSE 通信）。

### 19.4 Mobile（不在 M9 验收范围）

Tauri 2.x 支持 iOS/Android，但 M9 仅验收桌面三平台。Mobile 留待 M10+，前端复用 `minicoding-web` 响应式布局。

### 19.5 发布管道

桌面应用独立于 cargo-dist 的 CLI 发布流，详见 `docs/design.md` §26.8。要点：

- `Cargo.toml` 标记 `[package.metadata.dist] dist = false`，cargo-dist 不构建桌面 crate；
- `tauri.conf.json` 配置 `externalBin: ["binaries/minicoding-server"]` + `beforeBuildCommand`（自动构建前端）；
- 本地开发：先运行 `scripts/setup-desktop-dev.sh` 创建占位 sidecar（满足 `tauri_build::build()` 编译期校验），再 `cargo build -p minicoding-desktop --features desktop`；
- 发布构建：`scripts/build-desktop.sh`（前端 → server 二进制 → sidecar 放置 → `cargo tauri build`）；
- CI 发布：`.github/workflows/desktop-release.yml` 在 tag push 时触发，4 平台 matrix 构建安装包上传到 GitHub Release。

---

## 20. 跨模块约定

### 20.1 命名

- crate：`minicoding-<sub>`；
- 模块：单数小写下划线（`fs`、`tool`、`provider`）；
- trait：名词或动词（`LlmProvider`、`Tool`、`Storage`）；
- 错误：`<Domain>Error`（`LlmError`、`ToolError`、`HookError`）。

### 20.2 可见性

- 每个 crate 只在 `lib.rs` 暴露稳定 API，内部模块默认 `pub(crate)`。
- `core` 的 trait 与数据模型必须 `pub`，实现细节 `pub(crate)`。
- 实现 crate 的具体实现类型（如 `PolicyEngine`）可 `pub` 供组装，但内部方法 `pub(crate)`。

### 20.3 错误传播

- 各 crate 定义自己的错误类型，实现 `Into<RuntimeError>`（core 定义）。
- 边界（CLI / SDK）统一转 `anyhow::Error` 输出。

### 20.4 日志

- 每个 crate 启用 `tracing`，不直接 `println!`。
- span 命名：`<crate>::<module>`，关键操作打 `info!`，细节打 `debug!`/`trace!`。
- OTel span 字段命名遵循 `design.md` §15.2。

### 20.5 测试组织

- 单元测试与源码同文件 `#[cfg(test)] mod tests`。
- 集成测试放 `tests/` 目录，按场景命名（`agent_loop.rs`、`compression.rs`、`sandbox.rs`）。
- 跨 crate 共享测试工具放 `crates/minicoding-core/tests/common/`。
- 各实现 crate 独立测试，不依赖其他实现 crate（用 mock trait）。
- `minicoding-web` 测试用 Vitest + React Testing Library（与 Vite 集成），不纳入 `cargo test`。
- `minicoding-desktop` 的 Rust 部分用 `cargo test`，Tauri IPC 命令用 mock sidecar 测试。

### 20.6 依赖治理

- core 的依赖必须是"轻量 + 无平台/网络"的（tokio/serde/tracing/thiserror/uuid/time/camino）。
- 重依赖（reqwest/landlock/libseccomp/rmcp/ratatui/tauri）只能出现在对应实现 crate。
- `cargo audit` + `cargo deny` 接入 CI，许可证限制为 MIT/Apache-2.0/BSD/ISC。
- `Cargo.lock` 提交到仓库（CLI 项目）。
- `minicoding-web` 的 `package.json` 锁定版本，用 `pnpm-lock.yaml` 提交；`npm audit` 接入 CI。

---

## 21. 模块成熟度矩阵

| 模块 | M0-M1 MVP | M2-M3 | M4-M5 | M6-M8 | M9 |
|------|:---:|:---:|:---:|:---:|:---:|
| core (runtime/agent/model/trait) | ✅ | 增强（subagent/plan） | 稳定 | 稳定 | - |
| context (压缩/熔断) | 基础 | ✅ 完整 | 增强 | 稳定 | - |
| policy (双 trait/黑名单/预设) | ✅ | 增强（risk 解释） | 稳定 | 稳定 | - |
| providers (openai/anthropic) | ✅ | ollama | router | 多模态 | - |
| tools (fs/shell/task/plan) | ✅ | multiedit | mcp 包装 | ✅ web/git/shell.background | - |
| storage (jsonl/audit) | ✅ | index/lock | export | 稳定 | - |
| sandbox (应用层路径) | ✅ | - | ✅ OS 级（landlock 直连 + 自研 pre_exec 胶水） | Windows 强化 | - |
| memory | 基础 | ✅ 双文件+AGENTS.md+Auto | 增强 | ✅ 向量检索（BM25） | - |
| hooks | - | - | ✅ 10 事件+asyncRewake | 稳定 | - |
| journal | - | - | ✅ /undo | 稳定 | - |
| mcp (rmcp client) | - | - | ✅ | ✅ server 暴露+检索 | - |
| cli (单次+会话+exec+doctor) | ✅ | resume | mcp 子命令 | 稳定 | - |
| tui | - | - | - | ✅ | - |
| sdk | - | - | - | ✅ | - |
| protocol (JSON-RPC DTO) | - | - | - | ✅ | - |
| server (HTTP/SSE/ACP/LSP) | - | - | - | ✅ | 增强（--web/--cors-origin） |
| extension-sdk | - | - | 骨架 | ✅ | - |
| web (React 前端) | - | - | - | - | ✅ |
| desktop (Tauri 壳) | - | - | - | - | ✅ |

> ✅ = 交付；增强 = 功能扩展；- = 不交付。
