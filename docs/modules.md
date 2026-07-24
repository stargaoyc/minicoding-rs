# 模块详细设计

本文描述每个 crate / 模块的职责边界、内部结构、公共 API 与对外依赖。所有 crate 组成 Cargo workspace。

> **重构说明（v2）**：原 `minicoding-core` 承载 14+ 职责（Agent 循环、上下文、权限、沙箱 trait、Hook trait、项目记忆、Journal、MCP trait、存储、审计、事件总线、配置、OTel、记忆），违反单一职责。本次重构将 core 精简为"抽象层 + Runtime 编排"，把各领域**实现**拆到独立 crate。trait 定义仍集中在 core（保证 Runtime 可持有 `Arc<dyn Trait>`），实现分散到领域 crate。沙箱改用 `sandbox-run` + `landlock` 主流库，MCP 改用官方 `rmcp` 2.x，均不自研。

---

## 0. Workspace 总览

### 0.1 Crate 列表

```
minicoding-rs (workspace)
├── crates/
│   ├── minicoding-core          # 抽象层：数据模型 + 核心 trait + Runtime 编排 + Event + Config + OTel
│   ├── minicoding-context       # ContextManager 实现 + 压缩管道 + 权重 + 熔断
│   ├── minicoding-policy        # 权限实现：PermissionPolicy/Prompter + builtin 黑名单 + ApprovalMode/Preset
│   ├── minicoding-memory        # 记忆实现：长期/Auto/会话记忆 + AGENTS.md loader
│   ├── minicoding-hooks         # Hooks 实现：Registry + ScriptHook + asyncRewake + 内置 Hook
│   ├── minicoding-journal       # FileChangeJournal 实现 + /undo
│   ├── minicoding-sandbox       # OS 沙箱驱动（基于 sandbox-run + landlock + libseccomp）
│   ├── minicoding-mcp           # MCP client/server（基于 rmcp 2.x 官方 SDK）
│   ├── minicoding-storage       # JSONL 存储 + audit.log 审计
│   ├── minicoding-providers     # LLM Provider 实现（OpenAI/Anthropic/Ollama）
│   ├── minicoding-tools         # 内置 Tool 实现（fs/shell/web/git/task/plan/mcp 包装）
│   ├── minicoding-cli           # CLI frontend
│   ├── minicoding-tui           # TUI frontend（M7）
│   └── minicoding-sdk           # 嵌入 SDK（M8）
```

### 0.2 依赖方向

```
                    cli / tui / sdk
                         │
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
              core ◄── providers   (LlmProvider 实现)
                ▲    ◄── storage    (Storage/audit 实现)
                │    ◄── context    (ContextManager 实现)
                │    ◄── policy     (PermissionPolicy/Prompter 实现)
                │    ◄── memory     (记忆 loader 实现)
                │    ◄── hooks      (HookRegistry 实现)
                │    ◄── journal    (Journal 实现)
                │    ◄── sandbox    (SandboxDriver 实现)
                │    ◄── mcp        (McpClient 实现)
                │
              (trait 定义集中在此)
```

### 0.3 分层原则

1. **core 是抽象层，零实现逻辑**：只含数据模型、trait 定义、Runtime 编排、Event、Config、OTel 初始化。无平台/网络重依赖，编译快、测试快。
2. **trait 定义在 core**：`Tool`/`PermissionPolicy`/`PermissionPrompter`/`SandboxDriver`/`Hook`/`ContextManager`/`Storage`/`Journal`/`McpClient`/`ProjectDocLoader`/`LlmProvider` 全部在 core 定义，保证 Runtime 可持有 `Arc<dyn Trait>`。实现可来自任意 crate。
3. **实现单一职责**：每个领域 crate 只负责一类实现（policy 只管权限、memory 只管记忆），不交叉。
4. **依赖单向**：core 不依赖任何领域 crate；领域 crate 依赖 core；tools 作为"组合层"可依赖多个领域 crate；cli/tui/sdk 依赖 tools + core。
5. **平台/网络隔离**：sandbox（C 绑定）、mcp（rmcp 网络栈）、providers（HTTP）的重依赖隔离在各自 crate，不污染 core。

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

### 1.2 内部模块树

```
minicoding-core/src/
├── lib.rs                 # prelude 再导出
├── runtime.rs             # Runtime 聚合根 + RuntimeBuilder
├── agent/
│   ├── mod.rs
│   ├── loop.rs            # AgentLoop 主循环（并行/串行分桶，见 design.md §2.3）
│   ├── subagent.rs        # 类型化子 Agent 派发（SubagentType）
│   ├── plan_mode.rs       # Plan 模式状态机（仅编排，权限判断委托 policy）
│   └── accumulator.rs     # 流式 delta 聚合
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
├── policy/trait.rs        # PermissionPolicy + PermissionPrompter + Verdict + Decision（见 api.md §3.6）
├── sandbox/trait.rs       # SandboxDriver trait + SandboxPolicy 枚举 + NoopDriver 兜底（见 api.md §3.9）
├── hooks/trait.rs         # Hook trait + HookEvent + HookDecision + HookOutput + AsyncRewakeSpec（见 api.md §3.8）
├── context/trait.rs       # ContextManager trait + ChatRequest + ContextSnapshot
├── memory/trait.rs        # ProjectDocLoader trait + MemoryStore trait（长期/Auto 记忆加载抽象）
├── journal/trait.rs       # Journal trait + ChangeEntry + UndoReport（见 api.md §3.11）
├── mcp/trait.rs           # McpClient trait + McpServerConfig + McpTransport + McpScope（见 api.md §11）
├── storage/trait.rs       # Storage trait + AuditSink trait
├── event.rs               # Event / EventBus（仅通知，含 TaskUpdated/HookRun/PermissionResolved/FileUndone）
├── config.rs              # RuntimeConfig 加载与合并（含 MINICODING_HOME + profiles）
├── paths.rs               # 路径约定（见 data-model.md §3.0）
└── otel.rs                # OpenTelemetry 初始化 / span 辅助 / 资源属性
```

### 1.3 公共 API（prelude）

```rust
pub mod prelude {
    pub use crate::runtime::{Runtime, RuntimeBuilder};
    pub use crate::agent::TurnOutcome;
    pub use crate::model::{Message, Role, ToolCall, ToolResult, Session, SessionId, SessionMeta,
                           SubagentType, Task, TaskStatus};
    pub use crate::provider::LlmProvider;
    pub use crate::tool::{Tool, ToolRegistry, ToolContext, SideEffect};
    pub use crate::policy::{PermissionPolicy, PermissionPrompter, Verdict, Decision};
    pub use crate::sandbox::{SandboxDriver, SandboxPolicy};
    pub use crate::hooks::{Hook, HookEvent, HookDecision, HookOutput, AsyncRewakeSpec};
    pub use crate::context::{ContextManager, ChatRequest, ContextSnapshot};
    pub use crate::memory::ProjectDocLoader;
    pub use crate::journal::{Journal, ChangeEntry, UndoReport};
    pub use crate::mcp::{McpClient, McpServerConfig, McpTransport, McpScope};
    pub use crate::storage::{Storage, AuditSink};
    pub use crate::event::Event;
    pub use crate::config::RuntimeConfig;
}
```

### 1.4 关键设计点

- **零实现逻辑**：core 不含压缩算法、黑名单正则、landlock ruleset、rmcp 调用、JSONL 写入等任何实现。`Runtime` 只编排：调 `ContextManager::build_chat_request` → `LlmProvider::chat_stream` → `ToolRegistry::dispatch`（其内调 `PermissionPolicy::check` → `SandboxDriver::apply`）。
- **trait 定义集中**：所有领域 trait 在 core 定义，领域 crate 实现 trait。这样 Runtime 持有 `Arc<dyn ContextManager>` 等不需知道具体实现 crate，依赖方向干净。
- **轻量依赖**：core 只依赖 `tokio`/`serde`/`serde_json`/`tracing`/`thiserror`/`uuid`/`time`/`camino`/`trait-variant`。无 `reqwest`/`landlock`/`rmcp`/`libseccomp` 等重依赖。
- **NoopDriver 兜底**：core 提供 `SandboxDriver` 的 `NoopDriver` 实现（无操作），供未启用 `minicoding-sandbox` feature 时使用。其他 trait 的默认实现（如 `JsonlStorage`）移到对应领域 crate，core 不提供。

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
└── tokenizer.rs           # tiktoken-rs 集成 + 启发式估算
```

### 2.3 关键设计点

- **依赖 core trait**：实现 `core::context::ContextManager`，调用 `core::model::Message`。
- **压缩熔断独立模块**：`circuit_breaker.rs` 维护失败计数与 Thrash 检测状态机，与 C-29 约束对应。
- **降级链复用**：`fallback.rs` 的 `SummaryFallback` 与 `minicoding-memory` 的会话摘要降级链同构，可通过 trait 共享。
- **依赖**：`minicoding-core` + `tiktoken-rs`（token 计数）+ `tokio`。摘要压缩需调 LLM，通过 `LlmProvider` trait 注入（不直接依赖 providers crate）。

---

## 3. `minicoding-policy`（权限实现）

### 3.1 职责

实现 `PermissionPolicy`/`PermissionPrompter` trait（定义在 core）：决策引擎、内置黑名单、ApprovalMode/Preset 解析、决策持久化、各 Prompter 实现。

### 3.2 模块树

```
minicoding-policy/src/
├── lib.rs                 # 工厂：build_policy(cfg) / build_prompter(cfg)
├── policy.rs              # PolicyEngine 实现 PermissionPolicy
├── builtin.rs             # 内置不可覆盖黑名单（危险命令/SSRF/敏感路径，C-02）
├── mode.rs                # PermissionMode / ApprovalMode / Preset 枚举与解析（见 api.md §2.4）
├── store.rs               # 决策持久化（policy.toml，AllowAlways/DenyAlways）
├── prompter/
│   ├── mod.rs
│   ├── interactive.rs     # InteractivePrompter（CLI TTY）
│   ├── non_interactive.rs # NonInteractivePrompter（非 TTY 策略化）
│   ├── tui.rs             # TuiPrompter（TUI 非阻塞）
│   └── callback.rs        # CallbackPrompter（SDK 闭包）
├── risk.rs                # 命令风险解释（granular approval policies，见 design.md §9.4）
└── path_sandbox.rs        # sandbox_path 路径校验（应用层第一道防线，见 security.md §3）
```

### 3.3 关键设计点

- **黑名单最高优先级**：`builtin.rs` 硬编码危险命令/SSRF/敏感路径，任何用户配置与 Hook 都无法覆盖（C-02）。
- **ApprovalMode × SandboxPolicy 正交**：`mode.rs` 解析预设并展开为默认 `Verdict` 与 `SandboxPolicy`（后者传给 sandbox crate）。
- **prompter 独立**：决策（policy）与交互（prompter）分离，解决 broadcast 事件总线无法承载点对点回复的架构缺陷（见 design.md §9.1）。
- **AGENTS.md 写保护**：`builtin.rs` 对 `AGENTS.md`/`CLAUDE.md` 写操作注入 `Verdict::Ask` 且不可 `AllowAlways`（C-23）。
- **依赖**：`minicoding-core` + `regex`（黑名单）+ `toml`/`serde`（policy.toml）+ `camino`。

---

## 4. `minicoding-memory`（记忆实现）

### 4.1 职责

实现 `ProjectDocLoader`/`MemoryStore` trait（定义在 core）：长期记忆双文件、Auto memory 自动学习、会话摘要、AGENTS.md 分层加载。

### 4.2 模块树

```
minicoding-memory/src/
├── lib.rs                 # 工厂
├── long_term.rs           # 长期记忆双文件（long_term.md + index.json）+ mtime 缓存
├── auto.rs                # Auto memory（auto.md + auto.index，启发式检测，置信度淘汰）
├── session_sum.rs         # 会话摘要 + 失败降级链（与 context::fallback 同构）
├── project_doc/
│   ├── mod.rs
│   ├── loader.rs          # AGENTS.md 分层加载算法（见 design.md §8.6）
│   └── fallback.rs        # fallback 文件名与 override 解析（CLAUDE.md/.cursorrules）
└── inject.rs              # 记忆注入 system 段（包裹 <long_term_memory>/<auto_memory> 边界）
```

### 4.3 关键设计点

- **Auto memory 物理隔离**：`auto.md` 与 `long_term.md` 分离存储，对 `long_term.md` 写入走 `Ask`，对 `auto.md` 隐式写入 `Allow`（C-27）。
- **指令性内容检测**：`auto.rs` 检测 `auto.md` 中含 `AGENTS.md` 风格指令性内容时降级 `Ask`（防绕过 C-23）。
- **mtime 缓存**：`long_term.rs` 用 mtime 判断文件变更，无变更零 IO/分词（M-04）。
- **依赖**：`minicoding-core` + `serde`/`serde_json`（index）+ `camino`。摘要需调 LLM，通过 trait 注入。

---

## 5. `minicoding-hooks`（Hooks 实现）

### 5.1 职责

实现 `Hook`/`HookRegistry` trait（定义在 core）：Hook 注册与串行聚合、ScriptHook 适配器、asyncRewake 异步唤醒、内置示例 Hook。

### 5.2 模块树

```
minicoding-hooks/src/
├── lib.rs                 # 工厂
├── registry.rs            # HookRegistryImpl 实现 HookRegistry（串行聚合，见 hooks.md §5）
├── script.rs              # ScriptHook 适配器（外部可执行 + JSON over stdio + 退出码语义）
├── async_rewake.rs        # asyncRewake 异步唤醒管理（后台任务 + 唤醒注入，见 hooks.md §11）
├── builtin/
│   ├── mod.rs             # 6 个内置示例 Hook
│   ├── fmt_on_write.rs    # PostToolUse 跑 formatter
│   ├── auto_approve_tests.rs
│   ├── block_secrets.rs
│   ├── git_status_inject.rs
│   ├── backup_before_compact.rs
│   └── test_on_stop.rs
└── protocol.rs            # HookInput/HookOutput JSON 协议 + 退出码映射
```

### 5.3 关键设计点

- **10 类事件**：`SessionStart`/`UserPromptSubmit`/`PreToolUse`/`PostToolUse`/`PostToolUseFailure`/`PreCompact`/`PostCompact`/`Stop`/`SubagentStop`/`PermissionRequest`（见 hooks.md §2）。
- **asyncRewake 后台进程同等待遇**：后台 Hook 子进程遵守凭证隔离（C-04）、沙箱（C-22）、路径沙箱（C-03）约束（C-26）。
- **L0 不可覆盖**：`registry.rs` 在分发 Hook 前先应用内置黑名单 `Deny`，Hook 的 `allow` 对黑名单 `Deny` 无效（C-21）。
- **依赖**：`minicoding-core` + `tokio`（子进程）+ `serde_json`（协议）+ `globset`（matcher）。

---

## 6. `minicoding-journal`（文件改动事务与回滚）

### 6.1 职责

实现 `Journal` trait（定义在 core）：会话内文件改动账本、`/undo` operation 级回滚、`/new` 会话级重置、冲突检测。

### 6.2 模块树

```
minicoding-journal/src/
├── lib.rs                 # FileChangeJournal 实现 Journal
├── journal.rs             # FileChangeJournal（内存，不落盘）
├── entry.rs               # ChangeEntry / FileChange 数据结构
├── undo.rs                # undo 反向恢复 + 冲突检测（见 design.md §17.4）
└── report.rs              # UndoReport（含 failed_files）
```

### 6.3 关键设计点

- **不落盘**：journal 含文件原文，落盘等于多存一份敏感数据，故仅驻留内存、会话结束即销毁（C-28）。
- **冲突检测不强行覆盖**：恢复前比对当前文件内容与 `after`，不一致记入 `failed_files`（C-28）。
- **特性门控**：`file-undo` feature 默认关闭，开启时由 Runtime 持有。
- **依赖**：`minicoding-core` + `camino` + `sha2`（hash 比对）。

---

## 7. `minicoding-sandbox`（OS 级沙箱驱动，基于主流库）

### 7.1 职责

实现 `SandboxDriver` trait（定义在 core）：基于 `sandbox-run` + `landlock` + `libseccomp` 主流库提供跨平台内核级隔离，**不自研**沙箱胶水代码。

### 7.2 模块树

```
minicoding-sandbox/src/
├── lib.rs                 # detect_driver() 工厂：按 cfg!(target_os) 选实现
├── driver.rs              # SandboxDriverImpl 实现 trait
├── linux.rs               # Linux: sandbox-run (Landlock) + libseccomp（syscall 过滤）
├── macos.rs               # macOS: sandbox-run（原生 sandbox 框架，Seatbelt）
├── windows.rs             # Windows: windows crate（受限令牌 + Job Object）
└── hardening.rs           # pre-main 进程硬化（PR_SET_DUMPABLE/RLIMIT_CORE/清 LD_*）
```

### 7.3 库选型（不自研）

| 平台 | crate | 版本 | 理由 |
|------|-------|------|------|
| 跨平台统一 API | `sandbox-run` | 0.43 | systemd 风格 API（ProtectSystem/ReadWritePaths/PrivateNetwork），原生支持 `apply_sandbox` 在子进程 fork 后 exec 前调用，与 `tokio::process` 兼容；跨 Linux+macOS |
| Linux 文件系统沙箱 | `landlock` | 0.4.5 | 官方 rust-landlock，Landlock LSM 安全抽象，纯 Rust 无 C 依赖，1260 万下载 |
| Linux syscall 过滤 | `libseccomp` | 0.x | seccomp-bpf 白名单系统调用（禁 ptrace/mount/reboot/kexec_load） |
| Windows | `windows` | - | 受限 token + Job Object + DACL |
| 进程硬化 | `libc` | - | PR_SET_DUMPABLE/RLIMIT_CORE |

> **不再自研**：原方案的"自生成 seatbelt profile + 手写 landlock ruleset 胶水"全部废弃，改用 `sandbox-run` 统一 API。`sandbox-run` 内部已封装 Landlock ruleset 构建与 macOS sandbox profile 生成，我们只配置 `ProtectSystem`/`ReadWritePaths`/`PrivateNetwork` 等高级选项。

### 7.4 关键设计点

- **平台检测**：`detect_driver()` 编译期按 `cfg!(target_os)` 选实现；运行期 `sandbox_run::landlock_available()` 探测内核支持，不支持则返回 `NoopDriver`（来自 core）并 warn。
- **VCS 目录保护**：通过 `sandbox-run` 的 `ReadOnlyPaths` 把 `.git`/`.hg`/`.svn` 设为只读（P-20）。
- **pre-exec apply**：`sandbox_run::apply_sandbox()` 在子进程 fork 后 exec 前调用，子进程启动即受限，无窗口期（参考 Codex，见 security.md §8.3）。
- **依赖隔离**：`landlock`/`libseccomp` 通过 `[target.'cfg(target_os = "linux")'.dependencies]` 条件引入，非 Linux 不编译。
- **依赖**：`minicoding-core` + `sandbox-run` + `landlock`（Linux）+ `libseccomp`（Linux）+ `windows`（Windows）+ `libc`。

---

## 8. `minicoding-mcp`（MCP client/server，基于官方 rmcp）

### 8.1 职责

实现 `McpClient` trait（定义在 core）：基于官方 `rmcp` 2.x SDK 连接外部 MCP server、list_tools、call、shutdown；亦提供 `serve --as-mcp-server` 暴露自身工具。

### 8.2 模块树

```
minicoding-mcp/src/
├── lib.rs                 # build_client() 工厂
├── client/
│   ├── mod.rs
│   ├── rmcp.rs            # 基于 rmcp 2.x 的实现（stdio + streamable HTTP + OAuth）
│   ├── lifecycle.rs       # 启动/握手/超时/优雅关闭
│   └── wrapper.rs         # 把 rmcp 工具包装为 minicoding Tool trait
├── server/
│   └── expose.rs          # 把 minicoding 内置工具暴露为 MCP server（rmcp #[tool] 宏）
├── approval.rs            # project 作用域首次批准流（mcp_choices.toml）
├── naming.rs              # mcp__<server>__<tool> 命名 + 权限通配匹配
└── tool_search.rs         # BM25 工具检索（M7+，工具多时按需检索）
```

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
- **依赖**：`minicoding-core` + `rmcp` 2.2（features: `client`/`server`/`macros`/`transport-child-process`/`transport-streamable-http-client-reqwest`）+ `tokio`。

---

## 9. `minicoding-storage`（存储与审计）

### 9.1 职责

实现 `Storage`/`AuditSink` trait（定义在 core）：JSONL 会话日志、会话索引、跨进程文件锁、审计日志。

### 9.2 模块树

```
minicoding-storage/src/
├── lib.rs                 # 工厂
├── jsonl.rs               # JsonlStorage 实现 Storage（追加写、崩溃安全）
├── index.rs               # 会话索引 index.json（轻量元数据列出）
├── lock.rs                # 跨进程文件锁（fs2）
├── audit.rs               # AuditSink 实现（audit.log JSONL，0600 权限）
└── export.rs              # 会话导出（md / jsonl）
```

### 9.3 关键设计点

- **崩溃安全**：每条消息 `append` 后 `fsync`，崩溃时磁盘与内存一致。
- **审计完整性**：`audit.log` 文件权限 0600，追加写不可篡改历史（无 update/delete API）。
- **依赖**：`minicoding-core` + `serde_json` + `fs2`（文件锁）+ `tracing`。

---

## 10. `minicoding-providers`（LLM Provider 实现）

### 10.1 职责

实现 `LlmProvider` trait（定义在 core）：OpenAI 兼容、Anthropic、Ollama；提供对应 Tokenizer。

### 10.2 模块树

```
minicoding-providers/src/
├── lib.rs                 # re-export + build_provider() 工厂
├── openai/
│   ├── mod.rs
│   ├── client.rs          # reqwest HTTP 客户端
│   ├── request.rs         # ChatRequest → OpenAI JSON
│   ├── response.rs        # SSE → Delta
│   └── tokenizer.rs       # tiktoken-rs 封装
├── anthropic/
│   ├── mod.rs
│   ├── client.rs
│   ├── request.rs         # ChatRequest → Anthropic JSON
│   ├── response.rs        # 事件流 → Delta
│   └── tokenizer.rs       # 近似计数
├── ollama/
│   └── mod.rs
└── common/
    ├── retry.rs           # 重试策略（指数退避、429 Retry-After）
    ├── sse.rs             # SSE 流解析
    └── error.rs           # LlmError 分类
```

### 10.3 关键设计点

- 每个 provider 内部统一返回 `BoxStream<Result<Delta>>`，转换逻辑隔离。
- 密钥从环境变量或 OS keyring 读取，绝不接受配置文件明文。
- 重试与超时在 `common::retry` 统一实现，装饰器包裹 stream。
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
│   └── grep.rs            # regex + ignore
├── shell/
│   ├── run.rs             # tokio::process + 超时 + 截断 + SandboxDriver::apply
│   ├── background.rs      # 启动后台命令返回 shell_id
│   ├── output.rs          # 读取后台命令累积输出
│   └── kill.rs            # 终止后台命令
├── web/
│   ├── fetch.rs           # reqwest + html→markdown
│   └── search.rs          # M7+
├── git/
│   ├── diff.rs
│   └── apply.rs
├── task/
│   ├── spawn.rs           # 类型化子 Agent 派发
│   ├── create.rs          # TaskCreate（增量模型，见 design.md §18）
│   ├── update.rs          # TaskUpdate（增量 + 依赖 + 状态机）
│   └── list.rs            # TaskList 快照
├── plan/
│   └── exit.rs            # ExitPlanMode（见 design.md §16.4）
├── mcp/
│   └── wrapper.rs         # 把 McpClient 远程工具包装为本地 Tool trait
└── util/
    ├── path.rs            # sandbox_path 路径校验（委托 minicoding-policy）
    ├── output.rs          # 输出截断与格式化
    └── diff.rs            # edit 工具的 diff 生成
```

### 11.3 关键设计点

- **路径沙箱委托**：`util::path` 调用 `minicoding-policy::path_sandbox::resolve_under`，不重复实现。
- **shell.run**：执行前调 `SandboxDriver::apply`（来自 `minicoding-sandbox`）应用 OS 沙箱。
- **fs.write/edit/delete + Journal**：成功后调 `Journal::record`（来自 `minicoding-journal`），仅 `file-undo=true` 时生效。
- **task.create/update/list**：增量模型，状态机 `Pending→InProgress→Completed` 不可跳跃（C-31）。
- **mcp::wrapper**：把 `McpServerConfig` + 远程 schema 包装为 `Tool`，`side_effect` 据 `readOnlyHint`/`destructiveHint` 映射（C-25）。
- **依赖**：`minicoding-core` + 按需依赖 context/policy/memory/hooks/journal/sandbox/mcp/storage（optional）+ `globset`/`ignore`/`regex`/`reqwest`。

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
│   └── mcp.rs            # minicoding mcp list/approve/reset-project-choices
└── cred.rs               # 凭证读取（env / keyring）
```

### 12.3 关键设计点

- **零业务逻辑**：所有决策委托 Runtime；CLI 只做 IO 与渲染。
- **feature 组装**：`builder.rs` 根据 cargo feature 启用的实现 crate 装配 Runtime（如未启用 `minicoding-sandbox` 则用 core 的 `NoopDriver`）。
- **非 TTY 降级**：检测 `stdout.is_terminal()`，非交互时禁 spinner/颜色，权限走 `NonInteractivePrompter`。
- **退出码**：成功 0；运行时错误 1；配置错误 2；中断 130。
- **依赖**：`minicoding-core` + 各实现 crate（按 feature）+ `clap`/`indicatif`/`anstream`/`rustyline`/`anyhow`。

---

## 13. `minicoding-tui`（TUI frontend，M7）

### 13.1 职责

基于 `ratatui` 的全屏交互界面：多会话、工具调用面板、权限弹窗、流式 Markdown 渲染。

### 13.2 模块树（规划）

```
minicoding-tui/src/
├── main.rs
├── app.rs                # App 状态机
├── event.rs              # 终端事件 → App 动作
├── view/
│   ├── chat.rs           # 对话主视图
│   ├── tool_panel.rs     # 工具调用面板
│   ├── task_panel.rs     # 任务列表面板（同步 TaskUpdated 事件）
│   ├── prompt.rs         # 输入区
│   └── permission.rs     # 权限弹窗（TuiPrompter 非阻塞）
├── render/
│   └── markdown.rs       # 流式 Markdown 渲染
└── runtime_bridge.rs     # 与 Runtime 的 channel 桥接
```

### 13.3 关键设计点

- 独立线程跑 Runtime，UI 线程通过 channel 收发事件。
- 权限弹窗非阻塞：Runtime 在 `Verdict::Ask` 时通过 `TuiPrompter`（点对点）挂起该工具调用，UI 处理后回传 `Decision`。
- **依赖**：`minicoding-core` + `minicoding-policy`（TuiPrompter）+ `ratatui`/`crossterm`/`tokio`。

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

## 15. 跨模块约定

### 15.1 命名

- crate：`minicoding-<sub>`；
- 模块：单数小写下划线（`fs`、`tool`、`provider`）；
- trait：名词或动词（`LlmProvider`、`Tool`、`Storage`）；
- 错误：`<Domain>Error`（`LlmError`、`ToolError`、`HookError`）。

### 15.2 可见性

- 每个 crate 只在 `lib.rs` 暴露稳定 API，内部模块默认 `pub(crate)`。
- `core` 的 trait 与数据模型必须 `pub`，实现细节 `pub(crate)`。
- 实现 crate 的具体实现类型（如 `PolicyEngine`）可 `pub` 供组装，但内部方法 `pub(crate)`。

### 15.3 错误传播

- 各 crate 定义自己的错误类型，实现 `Into<RuntimeError>`（core 定义）。
- 边界（CLI / SDK）统一转 `anyhow::Error` 输出。

### 15.4 日志

- 每个 crate 启用 `tracing`，不直接 `println!`。
- span 命名：`<crate>::<module>`，关键操作打 `info!`，细节打 `debug!`/`trace!`。
- OTel span 字段命名遵循 `design.md` §15.2。

### 15.5 测试组织

- 单元测试与源码同文件 `#[cfg(test)] mod tests`。
- 集成测试放 `tests/` 目录，按场景命名（`agent_loop.rs`、`compression.rs`、`sandbox.rs`）。
- 跨 crate 共享测试工具放 `crates/minicoding-core/tests/common/`。
- 各实现 crate 独立测试，不依赖其他实现 crate（用 mock trait）。

### 15.6 依赖治理

- core 的依赖必须是"轻量 + 无平台/网络"的（tokio/serde/tracing/thiserror/uuid/time/camino/trait-variant）。
- 重依赖（reqwest/landlock/libseccomp/rmcp/ratatui）只能出现在对应实现 crate。
- `cargo audit` + `cargo deny` 接入 CI，许可证限制为 MIT/Apache-2.0/BSD/ISC。
- `Cargo.lock` 提交到仓库（CLI 项目）。

---

## 16. 模块成熟度矩阵

| 模块 | M0-M1 MVP | M2-M3 | M4-M5 | M6-M8 |
|------|:---:|:---:|:---:|:---:|
| core (runtime/agent/model/trait) | ✅ | 增强（subagent/plan） | 稳定 | 稳定 |
| context (压缩/熔断) | 基础 | ✅ 完整 | 增强 | 稳定 |
| policy (双 trait/黑名单/预设) | ✅ | 增强（risk 解释） | 稳定 | 稳定 |
| providers (openai/anthropic) | ✅ | ollama | router | 多模态 |
| tools (fs/shell/task/plan) | ✅ | web/git/multiedit | mcp 包装 | 稳定 |
| storage (jsonl/audit) | ✅ | index/lock | export | 稳定 |
| sandbox (应用层路径) | ✅ | - | ✅ OS 级（sandbox-run） | Windows 强化 |
| memory | 基础 | ✅ 双文件+AGENTS.md+Auto | 增强 | 向量检索 |
| hooks | - | - | ✅ 10 事件+asyncRewake | 稳定 |
| journal | - | - | ✅ /undo | 稳定 |
| mcp (rmcp client) | - | - | ✅ | server 暴露+检索 |
| cli (单次+会话+exec+doctor) | ✅ | resume | mcp 子命令 | 稳定 |
| tui | - | - | - | ✅ |
| sdk | - | - | - | ✅ |

> ✅ = 交付；增强 = 功能扩展；- = 不交付。
