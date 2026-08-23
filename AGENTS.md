# AI 辅助编码约束（AGENTS.md）

## §0 文件性质

本文件是 `minicoding-rs` 的**项目级 AI 辅助编码约束文件**。AI 编码助手（Claude Code、Cursor、Copilot、Trae 等）在本项目编写、修改、审查代码时**必须遵守**本文件指令。

### 与 docs/rules.md 的关系（二者正交，不可混淆）

| 文件 | 约束对象 | 时机 | 性质 |
|------|---------|------|------|
| `docs/rules.md` | 被 minicoding 驱动的 LLM（运行时模型） | 运行时 | 大模型约束（C-01..C-35），由 Rust Runtime 强制 |
| `AGENTS.md`（本文件） | 帮我们写代码的 AI 助手（开发时模型） | 开发时 | 助手行为约束，由助手自觉 + 代码审查强制 |

`rules.md` 约束"被驱动的 LLM 不得越权"；`AGENTS.md` 约束"写代码的 AI 助手不得乱来"。二者作用域不同，**不互相替代**：写代码时不得引用 `rules.md` 的 L0 作为"可绕过开发规范"的理由，也不得用本文件去放松运行时约束。

> 所有架构与运行时细节以 `docs/` 下文档为准（`design.md`/`modules.md`/`rules.md`/`tech-stack.md`/`api.md`/`security.md`/`features.md`/`hooks.md`/`data-model.md`）。本文件仅规定**开发时**助手行为。

---

## §1 项目概况

`minicoding-rs` 是一个 Rust 实现的终端 AI Coding 助手（参考 Claude Code / Codex CLI 设计），提供 Agent 循环、工具系统、权限沙箱、上下文管理、MCP 接入、会话审计等能力。

### 技术栈

- 语言：Rust 2024 edition，MSRV 1.99+（`async fn in trait` 已稳定，无需 `async-trait`）
- 异步运行时：统一 `tokio`（不混用 `async-std`）
- 序列化：`serde` + `serde_json` + `toml`
- 日志/追踪：`tracing` + OpenTelemetry（OTel 一等公民，从 M0 接入）
- 错误：库 crate `thiserror`，边界 crate `anyhow`
- 路径：`camino::Utf8PathBuf`（UTF-8 保证）
- HTTP：`reqwest`（rustls-tls，不裸用 hyper）
- 沙箱：自研驱动（landlock 已接，**seccomp 待接入**）——`sandbox-run` 因 EUPL-1.2 许可证弃用（见 tech-stack.md §13），AGENTS.md 原"不自研"条目随之回收
- MCP：`rmcp` 2.2（官方 Rust MCP SDK，**不自研** stdio/http）
- 详见 `docs/tech-stack.md`

### Workspace 结构（当前 Cargo workspace 含 18 个 crate：M0–M8 的 17 个 + M9 新增 `minicoding-desktop`；M9 另含 `minicoding-web` 为独立 npm 项目，不入 Cargo workspace）

```
minicoding-rs (workspace)
└── crates/
    ├── minicoding-core          # 抽象层：trait 定义 + Runtime 编排（零实现）
    ├── minicoding-context       # ContextManager 实现 + 4 级压缩 + 熔断
    ├── minicoding-policy        # 权限实现 + builtin 黑名单 + Prompter
    ├── minicoding-memory        # 长期/Auto/会话记忆 + AGENTS.md loader
    ├── minicoding-hooks         # HookRegistry + ScriptHook + asyncRewake
    ├── minicoding-journal       # FileChangeJournal + /undo
    ├── minicoding-sandbox       # OS 沙箱驱动（自研 pre_exec 胶水 + landlock；seccomp 待接入）
    ├── minicoding-mcp           # MCP client/server（rmcp 2.2）+ 进程池 + 后台预热
    ├── minicoding-storage       # JSONL 存储 + audit.log
    ├── minicoding-providers     # LLM Provider（OpenAI/Anthropic/Ollama）+ 小 LLM
    ├── minicoding-tools         # 内置 Tool 实现（组合层）
    ├── minicoding-protocol      # JSON-RPC 2.0 wire types + Event/Command DTO
    ├── minicoding-server        # HTTP/SSE server + ACP/LSP 适配器（多前端接入层）
    ├── minicoding-extension-sdk # 扩展作者稳定 API（Extension trait + Registrar）
    ├── minicoding-cli           # CLI frontend
    ├── minicoding-tui           # TUI frontend（M7）
    ├── minicoding-sdk           # 嵌入 SDK（M8）
    └── minicoding-desktop       # Tauri 2.x 桌面壳（M9，sidecar 启动 minicoding-server）
```

> **M9 另含** `crates/minicoding-web/`：纯前端项目（React 19.2 + TypeScript 7.0 + Vite 8.1），独立 `package.json`，不属于 Cargo workspace。
>
> 技术栈与架构见 `docs/tech-stack.md` §4.1 与 `docs/design.md` §26、`docs/modules.md` §18–§19。

核心设计（Agent 循环、上下文管理、工具系统、权限模型、记忆、MCP、Hook、Journal）见 `docs/design.md`；模块职责边界见 `docs/modules.md`；运行时大模型约束见 `docs/rules.md`。

---

## §2 Rust 编码规范

### 2.1 edition 与 MSRV

- `edition = "2024"`，`rust-version = "1.99"`
- `async fn in trait` 直接用；trait 需作 `dyn` 对象时用 `#[trait_variant::make(Trait: Send)]` 生成 Send 变体（Runtime 需 `Arc<dyn Trait>`）
- 不引入 `async-trait`（已废弃路径）

### 2.2 命名

- crate：`minicoding-<sub>`（如 `minicoding-sandbox`）
- 模块：单数、小写下划线（`fs`/`tool`/`provider`/`context_manager`）
- trait：名词或动词（`Tool`/`LlmProvider`/`ContextManager`/`PermissionPolicy`）
- 错误类型：`<Domain>Error`（`LlmError`/`ToolError`/`HookError`/`JournalError`/`McpError`/`RuntimeError`）
- 详见 `docs/modules.md` §15.1

### 2.3 错误处理

- 库 crate（core 及各领域 crate）用 `thiserror` 定义具体错误类型，实现 `Into<RuntimeError>`
- 边界 crate（`minicoding-cli`/`minicoding-sdk`）用 `anyhow::Result` 聚合并格式化输出
- **不 panic**：除真正不可恢复的程序员 bug（如 `unreachable!` 标记的不变式被破坏）。所有可预期错误走 `Result`
- 不用 `unwrap()`/`expect()` 在非测试代码中（除非有 SAFETY/不变式注释证明不会 panic）

### 2.4 async

- 统一 `tokio` runtime（`#[tokio::main]`/`#[tokio::test]`）
- **不裸用** `std::thread`（除 FFI 阻塞调用包裹线程，需注释说明）
- 流式响应用 `BoxStream<Result<Delta>>` / `impl Stream`
- 并发原语用 `tokio::sync`（`mpsc`/`broadcast`/`RwLock`/`Mutex`）

### 2.5 类型约定

- 路径用 `camino::Utf8PathBuf` 替代 `std::path::PathBuf`（UTF-8 保证，避免 OS 字符集边界）
- 结构体字段用 `String` 而非 `&str`（避免不必要生命周期）
- ID 用 `uuid::Uuid` 或 ULID（`task_id` 由 Runtime 生成，不可由 LLM 伪造，见 `docs/rules.md` C-31）
- 时间用 `time::OffsetDateTime`（不用 `chrono`）

### 2.6 unsafe

- **默认禁用** `unsafe`
- 必须使用时（FFI: landlock/libseccomp/windows libc 调用）：
  - 必须写 `// SAFETY: ...` 注释说明不变式
  - 必须有同级 code review 记录
  - 范围最小化（`unsafe` 块不包裹无关代码）
- 非 FFI 场景（如"性能优化"）不引入 `unsafe`

### 2.7 依赖治理

- 新增依赖必须：
  1. `cargo audit` 无已知漏洞
  2. `cargo deny check licenses` 合规（许可证限 MIT / Apache-2.0 / BSD / ISC）
  3. 仅开必要 feature（如 `reqwest` 只开 `json, rustls-tls, stream`）
- 优先用主流库（见 `docs/tech-stack.md`），不自研能用库的（MCP 用 rmcp、HTTP 用 reqwest；沙箱驱动自研，见 tech-stack.md §13 决策记录）
- 重依赖（`reqwest`/`landlock`/`libseccomp`/`rmcp`/`ratatui`/`windows`）只能在对应实现 crate 引入，不污染 core
- `Cargo.lock` 提交到仓库（CLI 项目）

### 2.8 测试

- 单元测试与源码同文件：`#[cfg(test)] mod tests { ... }`
- 集成测试放 `tests/` 目录，按场景命名（`agent_loop.rs`/`compression.rs`/`sandbox.rs`）
- 跨 crate 共享测试工具放 `crates/minicoding-core/tests/common/`
- 异步测试用 `#[tokio::test]`
- HTTP mock 用 `wiremock`/`httpmock`，**不连真实 OpenAI/Anthropic**
- 覆盖率目标 ≥80%（`cargo-llvm-cov`）
- 详见 `docs/modules.md` §15.5

### 2.9 clippy

- 每个 crate `lib.rs` 顶部：`#![deny(clippy::all, clippy::pedantic)]` 起步
- 例外用 `#[allow(clippy::xxx)]` + 紧跟一行注释说明理由
- 不用 `#![allow(...)]` 全局放松

### 2.10 注释

- 公共 API（`pub fn`/`pub struct`/`pub trait`/`pub enum`）必须 doc comment（`///`），说明用途、参数、返回、错误条件
- 复杂逻辑加 `//` 注释解释 **why not what**（为什么这么写，不是做了什么）
- 不写显而易见的注释（如 `// i 加 1`）
- 不写与代码冲突的过时注释——改代码必改注释

---

## §3 架构设计规范

### 3.1 单一职责

- 每个 crate 只负责一类实现（见 `docs/modules.md` §0.3）
- `minicoding-core` **禁止** 含任何领域实现逻辑（压缩算法、黑名单正则、landlock ruleset、rmcp 调用、JSONL 写入、HTTP 客户端、Hook 协议解析等）
- 领域 crate **禁止** 交叉：`minicoding-policy` 不写记忆加载、`minicoding-memory` 不写权限决策、`minicoding-sandbox` 不写 MCP
- `minicoding-tools` 是唯一"组合层"，可依赖多个领域 crate 完成工具执行闭环

### 3.2 依赖方向（单向不循环）

```
core  ◄──  领域 crate (context/policy/memory/hooks/journal/sandbox/mcp/storage/providers)
  ▲
  └──  tools (组合层)  ◄──  cli / tui / sdk (frontend)
```

- core 不依赖任何领域 crate
- 领域 crate 依赖 core（不互相依赖）
- tools 可依赖多个领域 crate
- cli/tui/sdk 依赖 tools + core + 必要实现 crate
- **禁止循环依赖**：领域 crate 之间如需协作，通过 core 的 trait 抽象解耦
- 详见 `docs/modules.md` §0.2

### 3.3 trait 定义集中在 core

所有领域 trait 在 `minicoding-core` 定义，实现在领域 crate：

| Trait | 定义位置 | 实现位置 |
|-------|---------|---------|
| `Tool` / `ToolRegistry` | `core::tool` | `minicoding-tools`（内置）/ `minicoding-mcp`（远程包装） |
| `LlmProvider` / `Tokenizer` | `core::provider` | `minicoding-providers` |
| `ContextManager` | `core::context` | `minicoding-context` |
| `PermissionPolicy` / `PermissionPrompter` | `core::policy` | `minicoding-policy` |
| `SandboxDriver` | `core::sandbox` | `minicoding-sandbox`（核心 `NoopDriver` 在 core 兜底） |
| `SandboxDenialDetector` / `SandboxDenialTracker` | `core::sandbox` | `minicoding-sandbox`（M-05，`NoopDenialDetector`/`NoopDenialTracker` 在 core 兜底） |
| `SubagentRunner` | `core::agent` | `minicoding-tools`（`WorktreeSubagentRunner`，M-05 下沉） |
| `Hook` / `HookRegistry` | `core::hooks` | `minicoding-hooks` |
| `Storage` / `AuditSink` | `core::storage` | `minicoding-storage` |
| `Journal` | `core::journal` | `minicoding-journal` |
| `McpClient` | `core::mcp` | `minicoding-mcp` |
| `ProjectDocLoader` / `MemoryStore` | `core::memory` | `minicoding-memory` |

这样 Runtime 持有 `Arc<dyn Trait>` 不需知道具体实现 crate，依赖方向干净（见 `docs/modules.md` §1.4）。

### 3.4 零实现 core

`minicoding-core` 只含：
- 数据模型（`Message`/`Role`/`ToolCall`/`ToolResult`/`Session`/`Task` 等）
- trait 定义（见 §3.3）
- Runtime 聚合根 + Agent 循环（编排各 trait，本身不含领域算法）
- 事件总线（`Event`/`EventBus`，仅通知无回复）
- 配置（`RuntimeConfig` 分层加载）
- OTel 初始化与 span 辅助
- 路径约定（`paths.rs`）
- `NoopDriver`（`SandboxDriver` 兜底实现，供未启用 sandbox feature 时使用）
- `NoopDenialDetector`/`NoopDenialTracker`（`SandboxDenialDetector`/`SandboxDenialTracker` 兜底实现，M-05）

**禁止** 在 core 出现：压缩算法、黑名单正则、landlock ruleset、rmcp 调用、JSONL 写入、HTTP 客户端、Hook 子进程协议解析、平台 denial 签名库、git worktree 命令胶水、事件重放算法等任何领域实现（M-05 已下沉：replay→`minicoding-storage`、worktree→`minicoding-tools`、denial 签名库/熔断→`minicoding-sandbox`；core 的架构守卫测试 `tests/architecture.rs` 强制依赖白名单）。

### 3.5 平台/网络隔离

重依赖通过 feature gate 或 target cfg 隔离在对应实现 crate：

```toml
# minicoding-sandbox/Cargo.toml
[target.'cfg(target_os = "linux")'.dependencies]
landlock = "0.4.5"
libseccomp = "0.x"

[target.'cfg(target_os = "windows")'.dependencies]
windows = { version = "...", features = ["..."] }
```

- `minicoding-core` 依赖必须"轻量 + 无平台/网络"：workspace 白名单见 `docs/modules.md` §1.4 与守卫测试 `tests/architecture.rs`（含 notify/ts-rs 等，新增需先过守卫并同步文档）
- `reqwest`/`landlock`/`libseccomp`/`rmcp`/`ratatui`/`windows` 只在对应实现 crate 引入
- 实现 crate 通过 cargo feature 按需启用（`default = ["memory", "sandbox"]` 等，见 `docs/modules.md` §0.4）

### 3.6 不自研能用库的

| 能力 | 选型 | 禁止自研 |
|------|------|---------|
| 沙箱统一 API | 自研驱动（landlock 已接，seccomp 待接入）——原选 sandbox-run 因 EUPL-1.2 弃用，见 tech-stack.md §13 |
| Linux 文件沙箱 | `landlock`（直接调用，自研 pre_exec 胶水） | 不手写 BPF |
| Linux syscall 过滤 | `libseccomp` | 不手写 BPF |
| MCP client/server | `rmcp` 2.2 | 不自实现 stdio/http 薄封装 |
| HTTP | `reqwest` | 不裸用 hyper |
| Glob | `globset` | 不手写 glob 解析 |
| 正则 | `regex` | 不自研正则引擎 |
| 路径规范化 | `camino` + `std::path::canonicalize` | 不手写路径校验 |
| BPE 分词 | `tiktoken-rs` | 不自实现 BPE |

详见 `docs/tech-stack.md` §13 备选方案权衡。

### 3.7 可见性

- 每个 crate 只在 `lib.rs` 暴露稳定 API（`prelude` 再导出）
- 内部模块默认 `pub(crate)`
- `core` 的 trait 与数据模型必须 `pub`
- 实现 crate 的具体实现类型（如 `PolicyEngine`）可 `pub` 供组装，但内部方法 `pub(crate)`
- 详见 `docs/modules.md` §15.2

### 3.8 配置

- 统一 `RuntimeConfig`（core 定义）
- 分层加载优先级：`MINICODING_HOME` > project > user > 默认
- 路径约定：`~/.minicoding/`（见 `docs/modules.md` §1.2 `paths.rs`）
- profiles 支持切换（如 `default`/`strict`/`danger`）

### 3.9 事件总线与权限交互

- `EventBus` 仅广播通知（无回复通道）：`Event::Token`/`MessageAppended`/`TurnEnd`/`TaskUpdated`/`HookRun`/`PermissionResolved`/`FileUndone` 等
- 权限交互走 `PermissionPrompter` 点对点（`InteractivePrompter`/`TuiPrompter`/`CallbackPrompter`/`NonInteractivePrompter`）
- 决策（`PermissionPolicy`）与交互（`Prompter`）分离，解决 broadcast 无法承载点对点回复的架构缺陷（见 `docs/design.md` §9.1）
- 详见 `docs/modules.md` §3.3

---

## §4 文档更新规范

### 4.1 改代码必改文档

新增/修改以下内容时，**同步更新**对应文档：

| 改动 | 必须更新 |
|------|---------|
| 公共 API（trait/struct/enum/fn 签名） | `docs/api.md` |
| crate 结构（新增/重命名/删除 crate） | `docs/modules.md` |
| 运行时约束（L0/L1/L2） | `docs/rules.md` |
| 功能项（新增/修改功能） | `docs/features.md` |
| 设计机制（Agent 循环、上下文、权限等） | `docs/design.md` |
| 安全机制 | `docs/security.md` |
| 数据结构 | `docs/data-model.md` |
| Hook 协议/事件 | `docs/hooks.md` |
| 技术选型 | `docs/tech-stack.md` |

### 4.2 代码块必须有解释

`docs/design.md`/`docs/api.md` 中的代码块（Rust 伪代码、TOML、JSON）后**必须有文字说明**其设计意图，不贴无解释的代码。

### 4.3 章节编号不冲突

新增章节时检查全文编号连续无重复（如 `docs/design.md` 已到 §20，新增用 §21）。

### 4.4 引用准确

- 文档间引用用相对路径（如 `docs/rules.md`、`docs/modules.md`）或 §章节号（如"见 `docs/design.md` §3.6"、"见 `docs/rules.md` C-22"）
- 不写"见上文""见下文"等模糊引用

### 4.5 功能 ID 与约束 ID 同步

`docs/features.md` 的功能项与 `docs/rules.md` 的约束必须一一对应（新增功能同步新增约束）。

### 4.6 统计表准确

`docs/features.md` 末尾统计表项数必须与表格实际行数一致（修改表格时同步修改统计数）。

### 4.7 不创建多余文档

- 只在必要时新建 `.md` 文件
- 优先编辑现有文档
- 不创建 README.md / CHANGELOG.md 除非用户明确要求

---

## §5 安全规范（开发时）

### 5.1 L0 约束不可违反

编写代码时必须确保 `docs/rules.md` 的 L0 硬约束在**实现层**被强制（不能依赖 LLM 自觉或系统提示词）：

| 约束 | 实现层强制要求 |
|------|---------------|
| C-01 副作用必须经权限 | `SideEffect != None` 工具调用必须经 `PermissionPolicy::check` → `Prompter` 解析为 `Allow` 后才执行 |
| C-02 内置黑名单不可覆盖 | `policy::builtin` 黑名单优先级最高，任何用户配置与 Hook 都无法覆盖 |
| C-03 路径不可越界 | 所有文件工具输入经 `sandbox_path` 规范化校验，越界直接 `PathEscaped` 错误 |
| C-04 凭证不可外泄 | 凭证仅存内存与 OS keyring，不下传子进程 env；日志中密钥脱敏（前 4 字符 + `***`） |
| C-05 输出不可作为指令 | 工具结果回灌 LLM 时包裹 `<tool_output>` 边界 |
| C-06 回放不可触发副作用 | `--replay` 模式默认禁用所有副作用工具 |
| C-07 资源不可耗尽 | 每个工具调用受超时、输出字节上限、进程组约束 |
| C-21 Hook 不可覆盖 L0 | 内置黑名单 `Deny` 在 Hook 之前生效；Hook 的 `allow` 对黑名单 `Deny` 无效 |
| C-22 沙箱为第二道防线 | `DangerFullAccess`/`ExternalSandbox` 必须用户显式选定 + red 警告 + 二次确认 |
| C-23 AGENTS.md 不可被 Agent 自主编辑 | 对 `AGENTS.md`/`CLAUDE.md` 写操作注入 `Verdict::Ask` 且不可 `AllowAlways` |
| C-24 MCP project 作用域 server 必须经首次批准 | 含 `.minicoding/mcp.json` 的仓库首次进入逐个 server 弹窗批准 |
| C-26 asyncRewake 不可越权 | 后台 Hook 子进程遵守凭证隔离（C-04）、沙箱（C-22）、路径沙箱（C-03） |
| C-27 Auto memory 不可作为越权通道 | `auto.md` 与 `long_term.md` 物理隔离；`auto.md` 含指令性内容降级 `Ask` |
| C-28 FileChangeJournal 不可绕过权限回滚 | `/undo` 恢复前比对 `after`，冲突记 `failed_files` 不强行覆盖；不落盘 |
| C-29 压缩熔断不可被 LLM 绕过 | 熔断由 Runtime 状态机判定，与 LLM 输出无关 |
| C-30 沙箱拒绝熔断不可被 LLM 绕过 | 沙箱拒绝来自内核级硬反馈，不可被应用层 `allow` 覆盖 |

详见 `docs/rules.md` §2 与 `docs/rules.md` §8 约束自检清单。

### 5.2 不引入不安全依赖

- 新增依赖必须 `cargo audit` 无漏洞
- `cargo deny check licenses` 许可证合规（MIT/Apache-2.0/BSD/ISC）
- 不引入维护停滞（>1 年无提交）或低下载量（<10k）的依赖，除非无替代
- 详见 `docs/tech-stack.md` §12

### 5.3 凭证不出现在代码/日志

- API key 只从环境变量或 OS keyring 读，**绝不硬编码**在源码/测试/文档中
- 测试用 mock 凭证（如 `sk-test-xxxxxxxx`），不写真实格式凭证
- 日志中密钥脱敏（前 4 字符 + `***`）
- 不在 commit message / PR 描述中贴凭证
- 不提交 `.env`/`credentials.json`/含私有 registry 的 `Cargo.lock`

### 5.4 测试不连真实服务

- LLM API 测试用 `wiremock`/`httpmock` 模拟，不连真实 OpenAI/Anthropic
- MCP server 测试用本地 mock stdio process
- 沙箱测试用 `tempfile` 临时目录，不碰真实用户文件
- 不在测试中发送真实 HTTP 请求到第三方

### 5.5 审计落盘

- 任何权限决策（`Allow`/`Deny`/`Ask`/`AllowAlways`/`DenyAlways`）必须落 `audit.log`（0600 权限，追加写不可篡改）
- 编写权限相关代码时确保 `AuditSink::record` 调用，不遗漏
- `/undo` 反向恢复也记审计（见 `docs/rules.md` C-28）
- asyncRewake 协议错误、Hook 协议违规均记审计

---

## §6 提交与协作规范

### 6.1 分支命名

`feature/<crate>-<topic>`（如 `feature/sandbox-landlock-driver`、`feature/policy-builtin-blacklist`）

### 6.2 提交信息（Conventional Commits）

```
<type>(<scope>): <subject>

<body>
```

- `type`：`feat`/`fix`/`refactor`/`docs`/`test`/`chore`/`perf`
- `scope`：crate 名（如 `sandbox`/`policy`/`core`/`tools`）
- `subject` 与 `body` 用**中文**
- 示例：
  - `feat(sandbox): 新增 landlock 驱动实现`
  - `fix(policy): 修复 builtin 黑名单优先级被覆盖问题`
  - `docs(design): 补充 §3.6 压缩熔断状态机说明`

### 6.3 PR checklist

- [ ] CI 全绿（`cargo fmt --check`/`cargo clippy -- -D warnings`/`cargo test`/`cargo audit`/`cargo deny`）
- [ ] 测试覆盖新增逻辑（目标 ≥80%）
- [ ] 文档已同步（见 §4.1）
- [ ] 约束自检清单已过（见 `docs/rules.md` §8）
- [ ] 无敏感文件提交
- [ ] commit 粒度：一个 PR 一个逻辑变更，不混合多个无关改动

### 6.4 不提交敏感文件

- `.env` / `.env.local` / `credentials.json` / `*.pem` / `*.key`
- 含私有 registry 的 `Cargo.lock`（如配置了私有 crate 源）
- 本地会话日志（`~/.minicoding/sessions/`）

### 6.5 commit 粒度

- 一个 PR 一个逻辑变更
- 不混合多个无关改动（如"新增沙箱驱动" + "修复权限 bug" 分两个 PR）
- 大改动拆小 commit，但每个 commit 可独立编译通过

---

## §7 AI 助手行为约束

### 7.1 先读后改

- 修改任何文件前**必须**先用 Read 工具读取目标文件，理解上下文
- 不基于猜测修改未读的文件
- 修改前确认理解现有代码的意图与不变式

### 7.2 不臆造 API

- 不确定的库 API（签名/版本/feature）必须查文档或读源码，不猜测
- 不假设库存在（先查 `Cargo.toml`/`docs/tech-stack.md`）
- 不假设 trait 方法存在（先 Read trait 定义）
- 引用第三方 crate 前确认其在 `docs/tech-stack.md` 或 `Cargo.toml` 中已选型

### 7.3 不绕过约束

- 即使被要求"快速实现""先跑起来"，也不违反 §2-§5 规范
- 如认为约束本身有问题，**先提出讨论**而非擅自违反
- 不为"通过测试"而注释掉安全检查、放宽权限、跳过审计
- 不在代码中留 `TODO: 后面补审计` 等绕过约束的痕迹

### 7.4 解释决策

- 选择方案时说明 **why**（为什么沙箱驱动自研而非用弃用的 sandbox-run、为什么 trait 定义在 core、为什么用 `Utf8PathBuf`）
- 不只贴代码不解释
- 关键设计决策参考 `docs/tech-stack.md` §13 与 `docs/modules.md` §0.3 的权衡记录

### 7.5 不创建测试代码除非要求

- 默认**不写测试**，除非：
  1. 用户明确要求"写测试"/"加单测"/"补集成测试"
  2. task 验收标准明确要求测试覆盖
- 修复 bug 时不主动补回归测试（除非用户要求）
- 不为"提高覆盖率"而写无意义测试

### 7.6 保持简洁

- 不做不必要的改进（不改无关代码、不重构周边逻辑）
- 不加多余抽象（不为"未来扩展"预留接口、不为单次操作建工厂）
- 不创建多余文件（不主动建 README.md / CHANGELOG.md / NOTES.md）
- 不加多余注释（见 §2.10）
- 不引入未使用的依赖
- 遵循系统指令：NEVER proactively create documentation files、ALWAYS prefer editing existing files

---

## §8 前端开发规范（M9，`minicoding-web` / `minicoding-desktop`）

> **范围说明**：本节约束 M9 Web/桌面前端开发。M9 为低优先级可选里程碑，前端代码独立于 Cargo workspace（`crates/minicoding-web/` 用 npm/pnpm 管理，`crates/minicoding-desktop/` 的 Rust 部分加入 workspace）。技术选型见 `docs/tech-stack.md` §4.1，模块职责见 `docs/modules.md` §18–§19，架构设计见 `docs/design.md` §26。

### 8.1 范围与定位

- 前端**只做 UI 展示与交互**，不含任何业务逻辑（Agent 循环、权限决策、上下文压缩、工具执行等均在 Rust 后端）；
- 前端**不嵌入 Rust 进程**：Web 模式通过 HTTP/SSE JSON-RPC 连接 `minicoding-server`；桌面模式（Tauri）通过 sidecar 进程通信（见 `docs/design.md` §26）；
- 前端代码与 Rust 后端的唯一契约是 `minicoding-protocol` 的 wire types（JSON-RPC 2.0 DTO），**不直接调用 Rust API**。

### 8.2 技术栈锁定（与 `docs/tech-stack.md` §4.1 一致）

| 用途 | 选型 | 版本 |
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
| 桌面壳 | Tauri | 2.x |

- **不引入未锁定依赖**：新增依赖必须经评审，优先选 Rust 实现的工具链（oxlint/oxfmt/Vite Rolldown/Tailwind v4）保持"全 Rust 工具链"一致性；
- **不引入 Electron**：桌面壳统一用 Tauri 2.x（体积/内存/安全均优，与本项目 Rust 一等公民理念一致）。

### 8.3 目录结构与分层

`crates/minicoding-web/` 标准分层（详见 `docs/design.md` §26.2）：

```
minicoding-web/src/
├── api/          # JSON-RPC 客户端 + SSE 订阅 + Zod schema（DTO 自动生成，见 §8.4）
├── hooks/        # TanStack Query + Zustand 封装（业务 hook）
├── components/   # shadcn/ui 组件 + 业务组件
├── routes/       # TanStack Router 页面（类型安全路由）
├── stores/       # Zustand 全局状态（UI 主题、面板开关等）
└── main.tsx      # 入口
```

- **严格分层**：`api/` 不依赖 `hooks/`/`components/`；`hooks/` 依赖 `api/`；`components/` 依赖 `hooks/`/`stores/`；`routes/` 依赖 `components/`；
- **不跨层调用**：组件不直接调 `api/`，必须经 `hooks/` 封装（便于缓存/重试/失效统一管理）。

### 8.4 类型契约（DTO 自动生成）

- `minicoding-protocol` 的 Rust DTO 通过 `ts-rs` 或 `specta` 自动生成 TypeScript 类型 + Zod schema，**不手写双份**；
- 生成产物放 `minicoding-web/src/api/generated/`，**不手动编辑**（文件头标注 `// AUTO-GENERATED, DO NOT EDIT`）；
- 后端 DTO 变更后，前端 `npm run gen-types` 重新生成，CI 校验生成产物与 Rust 源一致（`git diff --exit-code`）；
- 运行时校验：JSON-RPC 响应必须经 Zod parse 后才进入业务层，防止后端 schema 漂移导致运行时错误。

### 8.5 状态管理（职责正交）

| 状态类型 | 工具 | 示例 |
|---------|------|------|
| 服务端状态（会话、消息、任务） | TanStack Query | `useSession()`/`useMessages()` |
| 客户端 UI 状态（主题、面板开关） | Zustand | `useThemeStore()`/`usePanelStore()` |
| 流式状态（token 增量、工具进度） | TanStack Query + `queryClient.setQueryData` | SSE 事件增量更新缓存 |

- **不混用**：服务端状态不进 Zustand（避免双写不一致）；客户端 UI 状态不进 TanStack Query（避免无谓网络请求）；
- **流式更新**：SSE 事件用 `queryClient.setQueryData` 增量更新对应 query，不触发 refetch（`Token` 事件追加到消息末尾，`MessageAppended` 事件替换整条消息）。

### 8.6 与后端通信

- **协议**：HTTP/SSE JSON-RPC 2.0（见 `docs/design.md` §24），前端不引入新协议；
- **端点**：`POST /sessions/{id}/messages` 发消息，`GET /sessions/{id}/events` 订阅 SSE 事件流，`POST /sessions/{id}/permissions/{pid}` 回传权限决策；
- **权限交互**：`PermissionPrompt` 经 SSE 推到前端，弹出 shadcn/ui Dialog，用户决策经 JSON-RPC 回传（见 `docs/design.md` §9.1）；
- **CORS**：Web 模式需 `minicoding serve --cors-origin` 配置允许的前端来源（默认仅 `http://localhost:*`）；
- **错误处理**：JSON-RPC 错误码统一映射为 TanStack Query 的 `error` 状态，UI 用 toast/alert 展示，不吞错。

### 8.7 构建与工具链

- **全 Rust 工具链**：oxlint（Lint）/ oxfmt（格式化）/ Vite Rolldown（构建）/ Tailwind v4 Oxide（CSS）均为 Rust 实现，与后端工具链一致；
- **CI 校验**：前端 CI 跑 `oxlint && oxfmt --check && tsc --noEmit && vite build`，与 Rust 侧 `cargo fmt --check && clippy && test` 对齐；
- **不引入 ESLint/Prettier**：已被 oxlint/oxfmt 替代，混用会导致规则冲突与性能浪费；
- **依赖锁定**：`package-lock.json`（或 `pnpm-lock.yaml`）提交到仓库，与 `Cargo.lock` 同等对待。

### 8.8 测试

- **单元测试**：Vitest（与 Vite 原生集成）测试 hooks/工具函数；
- **组件测试**：Vitest + Testing Library 测试组件渲染与交互；
- **E2E 测试**：Playwright 测试关键路径（创建会话→发消息→流式渲染→权限确认）；
- **不连真实后端**：测试用 MSW（Mock Service Worker）拦截 HTTP/SSE，模拟 JSON-RPC 响应，与 Rust 侧 `wiremock` 不连真实 OpenAI 原则一致（见 §5.4）。

### 8.9 安全

- **凭证**：API key 等凭证**不存前端**——Web 模式由 `minicoding-server` 持有，桌面模式复用 OS keyring（与 CLI `cred.rs` 共享 `KEYRING_SERVICE = "minicoding"`，C-04）；
- **XSS 防护**：流式 token 渲染必须用 React 的 `{text}` 转义，**禁用** `dangerouslySetInnerHTML`（除非经 DOMPurify 清洗，仅用于 Markdown 渲染）；
- **CSP**：Tauri 桌面模式默认禁用远程内容，CSP 严格（仅允许 `self` + sidecar origin）；Web 模式由部署侧配置 CSP header；
- **权限决策不前端绕过**：前端 UI 的"允许/拒绝"按钮仅回传 `Decision` 到后端，**不前端短路**权限检查（C-01 在后端强制）；
- **不存敏感会话日志**：前端不持久化消息内容到 `localStorage`/`IndexedDB`（会话日志由后端 `~/.minicoding/sessions/` 管理，C-04）。

---

## §9 快速参考（检查清单）

开发前快速自检（打勾确认）：

- [ ] Rust 2024 edition？MSRV 1.99+？
- [ ] 新 crate 职责单一（见 `docs/modules.md`）？
- [ ] trait 定义在 core，实现在领域 crate（见 §3.3）？
- [ ] 重依赖在对应 crate 隔离（feature gate / target cfg，见 §3.5）？
- [ ] MCP 用 `rmcp` 2.2（不自研）？HTTP 用 `reqwest`（不自研）？
- [ ] 公共 API 有 doc comment（`///`）？
- [ ] 错误用 `thiserror`（库）/ `anyhow`（边界）？不 panic？
- [ ] 不 `unsafe`（除非 FFI + `// SAFETY:` 注释 + review）？
- [ ] 路径用 `camino::Utf8PathBuf`？结构体字段用 `String`？
- [ ] `clippy::all` + `clippy::pedantic` deny 起步？
- [ ] L0 约束在实现层被强制（见 `docs/rules.md` §2）？
- [ ] 凭证不硬编码、日志脱敏、测试不连真实服务？
- [ ] 权限决策落 `audit.log`？
- [ ] 改代码同步改文档（见 §4.1）？
- [ ] CI 全绿（fmt/clippy/test/audit/deny）？
- [ ] commit 粒度合理（一个 PR 一个逻辑变更）？
