# 模块详细设计

本文描述每个 crate / 模块的职责边界、内部结构、公共 API 与对外依赖。所有 crate 组成 Cargo workspace。

---

## 0. Workspace 总览

```
minicoding-rs (workspace)
├── crates/minicoding-core      # 核心运行时 + trait 定义（无上游业务依赖）
├── crates/minicoding-sandbox   # OS 级沙箱驱动（seatbelt/landlock/seccomp/windows）
├── crates/minicoding-providers # LLM Provider 实现
├── crates/minicoding-tools     # 内置 Tool 实现（含 todo/plan/file-undo）
├── crates/minicoding-mcp       # MCP client/server 实现（前置到 M4）
├── crates/minicoding-cli       # CLI frontend
├── crates/minicoding-tui       # TUI frontend（阶段 3）
└── crates/minicoding-sdk       # 嵌入 SDK（阶段 3）
```

依赖方向：

```
cli / tui / sdk
       │
       ▼
   core ◀── providers
       ▲   ◀── sandbox      (平台沙箱驱动，依赖 core 的 SandboxPolicy/SandboxDriver trait)
       │   ◀── mcp          (依赖 core 的 Tool/McpClient trait)
       │      │
       │      ▼
       └── tools (依赖 core 的 trait)
```

`core` 是依赖根，不被任何业务 crate 反向依赖。`minicoding-sandbox` 与 `minicoding-mcp` 单独成 crate 以隔离平台/网络依赖：sandbox 含 `landlock`/`libseccomp`/`windows` 等 C 绑定，mcp 含 `rmcp`/`reqwest` 等网络栈，不污染 core 的轻量依赖面。

---

## 1. `minicoding-core`

### 1.1 职责

定义所有核心 trait、数据模型、Agent 循环、上下文管理、事件总线、会话管理、默认实现（Storage / PermissionPolicy / ContextCompressor / ProjectDocLoader / Journal）。新增 hooks/sandbox/project_memory/journal/mcp/approval 的 trait 定义与轻量默认实现集中在 core，平台/网络相关实现在独立 crate。

### 1.2 内部模块树

```
minicoding-core/src/
├── lib.rs
├── runtime.rs            # Runtime 聚合根
├── agent/
│   ├── mod.rs
│   ├── loop.rs           # AgentLoop 主循环（含并行/串行分桶，见 design.md §2.3）
│   ├── subagent.rs       # 类型化子 Agent 派发（SubagentType，见 design.md §7.2）
│   ├── plan_mode.rs      # Plan 模式状态机 + ExitPlanMode 处理（见 design.md §16）
│   └── accumulator.rs    # 流式 delta 聚合
├── context/
│   ├── mod.rs
│   ├── manager.rs        # ContextManager
│   ├── budget.rs         # token 预算计算
│   ├── compress.rs       # 压缩管道
│   └── weight.rs         # 消息权重模型
├── model/
│   ├── message.rs        # Message / Role / Content
│   ├── tool.rs           # ToolCall / ToolResult / ToolSchema
│   ├── session.rs        # Session / SessionId
│   ├── todo.rs           # Todo / TodoStatus / TodoWriteInput（见 design.md §18.2）
│   ├── subagent.rs       # SubagentType / SubagentSpec / Thoroughness
│   └── error.rs          # RuntimeError / LlmError / ToolError / JournalError / McpError
├── provider/
│   └── trait.rs          # LlmProvider / Tokenizer trait
├── tool/
│   ├── registry.rs       # ToolRegistry（按 side_effect 调度）
│   ├── trait.rs          # Tool trait（含 is_read_only()，见 api.md §3.3）
│   └── context.rs        # ToolContext / SideEffect
├── policy/
│   ├── trait.rs          # PermissionPolicy + PermissionPrompter（见 api.md §3.6）
│   ├── mode.rs           # PermissionMode / ApprovalMode / Preset 枚举与解析（见 api.md §2.4）
│   ├── default.rs        # InteractivePrompter / NonInteractivePrompter / TuiPrompter / CallbackPrompter
│   ├── store.rs          # 决策持久化（policy.toml）
│   └── builtin.rs        # 内置不可覆盖黑名单（危险命令/SSRF/敏感路径）
├── sandbox/
│   ├── trait.rs          # SandboxDriver trait + SandboxPolicy 枚举（见 api.md §3.9）
│   └── noop.rs           # NoopDriver 兜底实现（实际平台实现在 minicoding-sandbox crate）
├── hooks/
│   ├── trait.rs          # Hook trait / HookRegistry / HookEvent（见 api.md §3.8、hooks.md §5）
│   ├── script.rs         # ScriptHook 适配器（JSON stdio 子进程）
│   └── builtin.rs        # 6 个内置示例 Hook（fmt-on-write / auto-approve-tests / ...）
├── project_memory/
│   ├── trait.rs          # ProjectDocLoader trait（见 api.md §3.10）
│   ├── loader.rs         # AGENTS.md 分层加载算法（见 design.md §8.6）
│   └── fallback.rs       # fallback 文件名与 override 解析
├── journal/
│   ├── trait.rs          # Journal trait（见 api.md §3.11）
│   └── in_memory.rs      # FileChangeJournal 内存实现（特性门控 file_undo）
├── mcp/
│   ├── trait.rs          # McpClient trait + McpServerConfig / McpTransport（见 api.md §11）
│   ├── naming.rs         # mcp_tool_name() + 权限规则通配匹配
│   └── choices.rs        # mcp_choices.toml 读写 + project 作用域批准流
├── storage/
│   ├── trait.rs          # Storage trait
│   └── jsonl.rs          # JSONL 实现
├── audit.rs              # 审计日志（audit.log）
├── event.rs              # Event / EventBus（仅通知，含 TodoUpdated/HookRun/PermissionModeChanged/FileUndone）
├── config.rs             # RuntimeConfig 加载与合并（含 MINICODING_HOME 解析 + profiles 段）
├── paths.rs              # 路径约定（见 data-model.md §3.0）
├── otel.rs               # OpenTelemetry 初始化 / span 辅助 / 资源属性
└── memory/
    ├── mod.rs            # 记忆加载/写入
    ├── long_term.rs      # 长期记忆双文件（md + index.json）+ mtime 缓存
    └── session_sum.rs    # 会话摘要 + 失败降级链
```

### 1.3 公共 API（再导出）

`lib.rs` 通过 `pub use` 暴露稳定 API 面：

```rust
pub mod prelude {
    pub use crate::runtime::{Runtime, RuntimeBuilder};
    pub use crate::agent::TurnOutcome;
    pub use crate::model::{Message, Role, ToolCall, ToolResult, Session, SessionId, Todo, SubagentType};
    pub use crate::provider::LlmProvider;
    pub use crate::tool::{Tool, ToolRegistry, ToolContext, SideEffect};
    pub use crate::policy::{PermissionPolicy, PermissionPrompter, Decision, Verdict};
    pub use crate::policy::mode::{PermissionMode, ApprovalMode, Preset, PresetKind};
    pub use crate::sandbox::{SandboxDriver, SandboxPolicy};
    pub use crate::hooks::{Hook, HookRegistry, HookEvent, HookDecision};
    pub use crate::project_memory::ProjectDocLoader;
    pub use crate::journal::{Journal, FileChangeJournal, UndoReport};
    pub use crate::mcp::{McpClient, McpServerConfig, McpTransport, McpScope};
    pub use crate::storage::Storage;
    pub use crate::event::Event;
    pub use crate::config::RuntimeConfig;
}
```

### 1.4 关键设计点

- **零上游业务依赖**：`core` 只依赖基础设施 crate（tokio / serde / tracing / thiserror / uuid / time / camino）。平台/网络相关（landlock/seccomp/windows/rmcp）在独立 crate。
- **trait 在 core 定义，实现可在任意 crate**：保证可替换性。`SandboxDriver`/`McpClient` 在 core 定义 trait，实现在 `minicoding-sandbox`/`minicoding-mcp`。
- **默认实现**：`JsonlStorage`、`InteractivePolicy`、`SummaryCompressor`、`ProjectDocLoader`、`NoopDriver`、`InMemoryJournal` 全部在 core 提供，开箱即用；平台沙箱与 rmcp 客户端需显式启用对应 crate。
- **特性门控**：`file_undo`（Journal）、`plan_mode`、`typed_subagents` 通过 cargo feature 控制，默认按 `config.toml [features]` 段开关。

---

## 2. `minicoding-providers`

### 2.1 职责

实现 `LlmProvider` trait，覆盖主流上游；提供对应 `Tokenizer` 实现。

### 2.2 模块树

```
minicoding-providers/src/
├── lib.rs                # re-export + 工厂函数
├── openai/
│   ├── mod.rs
│   ├── client.rs         # HTTP 客户端
│   ├── request.rs        # ChatRequest → OpenAI JSON
│   ├── response.rs       # SSE → Delta
│   └── tokenizer.rs      # tiktoken-rs 封装
├── anthropic/
│   ├── mod.rs
│   ├── client.rs
│   ├── request.rs        # ChatRequest → Anthropic JSON
│   ├── response.rs       # 事件流 → Delta
│   └── tokenizer.rs      # 近似计数
├── ollama/
│   └── ...
└── common/
    ├── retry.rs          # 重试策略
    ├── sse.rs            # SSE 流解析
    └── error.rs          # LlmError 分类
```

### 2.3 工厂

```rust
pub fn build_provider(cfg: &ProviderConfig) -> Result<Arc<dyn LlmProvider>> {
    match cfg {
        ProviderConfig::OpenAI(c)    => Ok(Arc::new(OpenAiProvider::new(c)?)),
        ProviderConfig::Anthropic(c) => Ok(Arc::new(AnthropicProvider::new(c)?)),
        ProviderConfig::Ollama(c)    => Ok(Arc::new(OllamaProvider::new(c)?)),
    }
}
```

### 2.4 关键设计点

- 每个 provider 内部统一返回 `BoxStream<Result<Delta>>`，转换逻辑隔离。
- 密钥从环境变量或 OS keyring 读取，绝不接受配置文件明文。
- 重试与超时在 `common::retry` 统一实现，通过装饰器包裹 stream。

---

## 3. `minicoding-tools`

### 3.1 职责

实现内置 `Tool` 集合；每个工具一个文件，便于维护与按组启用。新增 `todo`/`plan`/`journal`（file-undo）工具组与 MCP 工具包装器。

### 3.2 模块树

```
minicoding-tools/src/
├── lib.rs                # register_all() 工厂
├── fs/
│   ├── read.rs
│   ├── write.rs          # 成功后调用 Journal::record（若启用）
│   ├── edit.rs           # 精确字符串替换 + Journal::record
│   ├── delete.rs         # + Journal::record
│   ├── list.rs
│   ├── glob.rs           # 基于 globset + ignore
│   └── grep.rs           # 基于 regex + ignore
├── shell/
│   └── run.rs            # tokio::process + 超时 + 输出截断 + SandboxDriver::apply
├── web/
│   ├── fetch.rs          # reqwest + html→markdown
│   └── search.rs         # 阶段 3
├── git/
│   ├── diff.rs
│   └── apply.rs
├── task/
│   └── spawn.rs          # 类型化子 Agent 派发（SubagentType，见 design.md §7.2）
├── todo/
│   └── write.rs          # TodoWrite 工具（见 design.md §18、api.md §10.1）
├── plan/
│   └── exit.rs           # ExitPlanMode 工具（见 design.md §16.4、api.md §10.2）
├── mcp/
│   └── wrapper.rs        # 把 McpClient 注册的远程工具包装为本地 Tool trait 实现
└── util/
    ├── path.rs           # sandbox_path 路径校验
    ├── output.rs         # 输出截断与格式化
    └── diff.rs           # edit 工具的 diff 生成
```

### 3.3 工具注册

```rust
pub fn register_all(reg: &mut ToolRegistry, cfg: &ToolConfig) {
    reg.register(Arc::new(fs::Read::default()));
    reg.register(Arc::new(fs::Write::new(cfg.fs.clone())));
    reg.register(Arc::new(fs::Edit::new(cfg.fs.clone())));
    // ...
    if cfg.enabled_groups.contains(ToolGroup::Shell) {
        reg.register(Arc::new(shell::Run::new(cfg.shell.clone())));
    }
    if cfg.enabled_groups.contains(ToolGroup::Todo) {
        reg.register(Arc::new(todo::Write::default()));
    }
    if cfg.features.plan_mode && cfg.enabled_groups.contains(ToolGroup::Plan) {
        reg.register(Arc::new(plan::Exit::default()));
    }
    // MCP 工具由 Runtime 在启动时从 McpClient::list_tools() 动态注册，见 §5
}
```

### 3.4 关键设计点

- **路径沙箱**：所有文件工具走 `util::path::resolve_under(workdir, input)`，越界直接 `ToolError::PathEscaped`。
- **edit 工具**：要求 `old_string` 唯一，否则报错并提示提供更多上下文；支持 `replace_all`。
- **shell.run**：分 stdout/stderr 收集，超时 kill，输出超 `max_output_bytes` 截断并附"已截断"标记；执行前调 `SandboxDriver::apply` 应用 OS 沙箱（见 `security.md` §8）。
- **grep**：复用 `ignore` crate 行为，尊重 `.gitignore`，结果按文件分组。
- **fs.write/edit/delete + Journal**：成功后调 `Journal::record`，仅当 `[features] file_undo = true` 时生效；失败的工具调用不记录（无副作用发生）。
- **todo.write**：`SideEffect::None`，仅更新内存 + 广播 `Event::TodoUpdated`；校验 20 上限、单 in_progress、completed 必填 summary（见 `api.md` §10.1）。
- **plan.exit**：`SideEffect::None`，仅切换 `PermissionMode` + 缓存预批准；仅在 Plan 模式下可调用（见 `api.md` §10.2）。
- **mcp::wrapper**：把 `McpServerConfig` + 远程工具 schema 包装为 `Tool` trait 实现，`side_effect` 据 server schema 的 `readOnlyHint`/`destructiveHint` 映射，`is_read_only()` 据此覆盖。

---

## 4. `minicoding-sandbox`（OS 级沙箱驱动，见 `security.md` §8）

### 4.1 职责

实现 `SandboxDriver` trait（定义在 core，见 `api.md` §3.9）的平台具体实现：macOS Seatbelt、Linux Landlock+seccomp、Windows 受限令牌。隔离平台 C 绑定（`landlock`/`libseccomp`/`windows`），不污染 core 依赖面。

### 4.2 模块树

```
minicoding-sandbox/src/
├── lib.rs                # detect_driver() 工厂：按 cfg!(target_os) 选实现
├── seatbelt.rs           # macOS: sandbox-exec -p <profile> 动态生成
├── landlock.rs           # Linux: landlock crate + seccomp 白名单
├── windows_acl.rs        # Windows: 受限令牌 + Job Object + DACL
└── hardening.rs          # pre-main 进程硬化（PR_SET_DUMPABLE/RLIMIT_CORE/清 LD_*）
```

### 4.3 关键设计点

- **平台检测**：`detect_driver()` 在编译期按 `cfg!(target_os)` 选实现，无可用硬隔离时返回 `NoopDriver`（来自 core）并打 warn。
- **`.git`/`.minicoding` 强制只读**：所有写策略下默认拒绝写入这两个目录（防破坏版本库与配置），需 `tools.sandbox.allow_dotgit_write = true` 显式放开。
- **pre-main hardening**：在子进程 `exec` 前应用策略，子进程一旦启动即在受限环境内，无窗口期（参考 Codex，见 `security.md` §8.3）。
- **降级**：容器内若不支持 Landlock/seccomp，自动降级为 `NoopDriver` + warn，依赖容器自身隔离。

---

## 5. `minicoding-mcp`（MCP client/server，前置到 M4，见 `design.md` §19）

### 5.1 职责

实现 `McpClient` trait（定义在 core，见 `api.md` §11）：连接外部 MCP server、list_tools、call、shutdown。亦提供 `minicoding serve --as-mcp-server` 把自身工具暴露给其他 Agent（features E-04，阶段 7）。

### 5.2 模块树

```
minicoding-mcp/src/
├── lib.rs                # build_client() 工厂
├── client/
│   ├── mod.rs
│   ├── rmcp.rs           # 基于 rmcp crate 的默认实现（stdio + http + OAuth）
│   ├── stdio_only.rs     # 早期薄封装，仅 stdio（M4 先交付）
│   └── lifecycle.rs      # 启动/握手/超时/优雅关闭
├── server/
│   └── expose.rs         # 把 minicoding 内置工具暴露为 MCP server（阶段 7）
├── approval.rs           # project 作用域首次批准流（mcp_choices.toml）
└── tool_search.rs        # BM25 工具检索（阶段 6+，见 design.md §19.6）
```

### 5.3 关键设计点

- **前置理由**：MCP 是 AI Coding 工具生态的关键接入点（GitHub/Slack/数据库），原先排在 M7 太晚。前置到 M4，先交付 `stdio_only` 客户端（薄封装），M5+ 升级到 `rmcp` 完整实现。
- **工具命名**：`mcp__<server>__<tool>`（见 `design.md` §19.3），与权限规则通配匹配兼容。
- **project 作用域批准**：首次遇到含 `.minicoding/mcp.json` 的仓库时逐个 server 弹窗，防恶意仓库植入（见 `design.md` §19.4）。
- **凭证隔离**：MCP server 子进程不继承 minicoding 的凭证环境变量（同 `shell.run`，见 `security.md` §6）。
- **`required` 语义**：`required = true` 的 server 启动失败则 minicoding 拒绝启动；`required = false`（默认）失败仅 warn 跳过。

---

## 6. `minicoding-cli`

### 6.1 职责

命令行入口；解析参数、加载配置、构建 Runtime、驱动会话、渲染输出。

### 6.2 模块树

```
minicoding-cli/src/
├── main.rs
├── args.rs               # clap derive 定义
├── app.rs                # App 主控
├── config_loader.rs      # 分层配置加载
├── render/
│   ├── mod.rs
│   ├── stream.rs         # 流式 token 渲染
│   ├── tool.rs           # 工具调用渲染
│   └── permpt.rs         # 权限确认提示
├── session/
│   ├── mod.rs
│   ├── interactive.rs    # 交互 REPL
│   └── resume.rs         # 会话恢复
└── cred.rs               # 凭证读取（env / keyring）
```

### 6.3 命令结构

```
minicoding [OPTIONS] [PROMPT]
  -p, --provider <NAME>      覆盖默认 provider
  -m, --model <NAME>         覆盖模型
  -s, --session              交互式会话
      --resume <ID>          恢复会话
      --replay <FILE>        回放会话
      --workdir <PATH>       工作目录
      --config <PATH>        指定配置文件
      --allow <RULE>         运行时追加 allow 规则
      --deny <RULE>          运行时追加 deny 规则
  -v, --verbose              详细日志
```

### 6.4 关键设计点

- **零业务逻辑**：所有决策委托 Runtime；CLI 只做 IO 与渲染。
- **流式渲染**：订阅 `EventBus`，token 直接写 stdout（不经格式化缓冲）。
- **非 TTY 降级**：检测 `stdout.is_terminal()`，非交互时禁用 spinner / 颜色。
- **退出码**：成功 0；运行时错误 1；配置错误 2；中断 130。

---

## 7. `minicoding-tui`（阶段 3）

### 7.1 职责

基于 `ratatui` 的全屏交互界面：多会话、工具调用面板、权限弹窗、流式 Markdown 渲染。

### 7.2 模块树（规划）

```
minicoding-tui/src/
├── main.rs
├── app.rs                # App 状态机
├── event.rs              # 终端事件 → App 动作
├── view/
│   ├── chat.rs           # 对话主视图
│   ├── tool_panel.rs     # 工具调用面板
│   ├── prompt.rs         # 输入区
│   └── permpt.rs         # 权限弹窗
├── render/
│   └── markdown.rs       # 流式 Markdown 渲染
└── runtime_bridge.rs     # 与 Runtime 的 channel 桥接
```

### 7.3 关键设计点

- 独立线程跑 Runtime，UI 线程通过 channel 收发事件。
- 流式 Markdown：增量解析，部分渲染，避免每 token 全量重绘。
- 权限弹窗非阻塞：Runtime 在 `Verdict::Ask` 时通过 `TuiPrompter`（点对点，见 `design.md` §9）挂起该工具调用，UI 处理后回传 `Decision`；`EventBus` 仅广播 `PermissionRequested`/`PermissionResolved` 通知。

---

## 8. `minicoding-sdk`（阶段 3）

### 8.1 职责

为第三方 Rust 程序提供高层嵌入 API，隐藏 Runtime 细节。

### 8.2 公共 API

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

### 8.3 关键设计点

- 默认无副作用权限策略，调用方需显式启用。
- 所有 API `Send + Sync`，可在多 tokio 任务中共享。
- 不依赖任何 CLI / TUI crate，体积可控。

---

## 9. 跨模块约定

### 9.1 命名

- crate：`minicoding-<sub>`；
- 模块：单数小写下划线（`fs`、`tool`、`provider`）；
- trait：名词或动词（`LlmProvider`、`Tool`、`Storage`）；
- 错误：`<Domain>Error`（`LlmError`、`ToolError`）。

### 9.2 可见性

- 每个 crate 只在 `lib.rs` 暴露稳定 API，内部模块默认 `pub(crate)`。
- `core` 的 trait 与数据模型必须 `pub`，实现细节 `pub(crate)`。

### 9.3 错误传播

- 各 crate 定义自己的错误类型，实现 `Into<RuntimeError>`。
- 边界（CLI / SDK）统一转 `anyhow::Error` 输出。

### 9.4 日志

- 每个 crate 启用 `tracing`，不直接 `println!`。
- span 命名：`<crate>::<module>`，关键操作打 `info!`，细节打 `debug!`/`trace!`。

### 9.5 测试组织

- 单元测试与源码同文件 `#[cfg(test)] mod tests`。
- 集成测试放 `tests/` 目录，按场景命名（`agent_loop.rs`、`compression.rs`）。
- 跨 crate 共享测试工具放 `crates/minicoding-core/tests/common/`。

---

## 10. 模块成熟度矩阵

| 模块 | 阶段 1 MVP | 阶段 2 | 阶段 3 |
|------|:---:|:---:|:---:|
| core (runtime/agent/context) | ✅ | 增强 | 稳定 |
| providers (openai/anthropic) | ✅ | ollama | router |
| tools (fs/shell/todo/plan) | ✅ | web/git/journal | mcp 包装 |
| cli (单次+会话/exec) | ✅ | resume | - |
| sandbox (应用层路径) | ✅ | OS 级硬隔离 | Windows 强化 |
| hooks | - | ✅ | prompt 类型 Hook |
| mcp client (stdio) | - | ✅ | rmcp/http + 工具检索 |
| tui | - | - | ✅ |
| sdk | - | - | ✅ |
| memory | 基础 | 增强（双文件+AGENTS.md） | 向量检索 |
| mcp server | - | - | ✅ |

> ✅ = 交付；增强 = 功能扩展；- = 不交付。
