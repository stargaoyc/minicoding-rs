# 总体架构设计

本文描述 `minicoding-rs` 的分层架构、组件职责与协作关系、关键数据流，以及横切关注点。

---

## 1. 设计原则

1. **分层解耦**：Frontend → Orchestrator → Capability，每层只依赖下层抽象接口。
2. **抽象优先**：所有可替换能力（LLM、工具、存储、权限）以 trait 定义在核心层，实现在外围 crate。
3. **单向依赖**：依赖方向自上而下，核心层不反向依赖 frontend。
4. **显式状态**：所有可变状态集中在 `Session` / `Runtime`，组件无全局可变状态。
5. **失败可恢复**：任何一轮 Agent 循环失败不应损坏会话状态，可从持久化日志恢复。
6. **可观测性内建**：每个跨进程/跨组件边界都打 span，trace 全链路贯通。

---

## 2. 分层架构总览

```
┌──────────────────────────────────────────────────────────────┐
│                      Frontend Layer                          │
│   minicoding-cli   │   minicoding-tui   │   minicoding-sdk   │
└───────────────┬──────────────────────────────────────────────┘
                │  (调用 Runtime API)
┌───────────────▼──────────────────────────────────────────────┐
│                   Orchestration Layer                        │
│   Agent Loop  │  Subagent  │  Context Manager  │  Planner    │
│   (minicoding-core)                                          │
└───────────────┬──────────────────────────────────────────────┘
                │  (依赖 trait 接口)
┌───────────────▼──────────────────────────────────────────────┐
│                    Capability Layer                          │
│ ┌──────────┐┌──────────┐┌──────────┐┌──────────┐┌──────────┐ │
│ │ LlmProv  ││ Tool/Reg ││ Context  ││ Policy   ││ Sandbox  │ │
│ │ (trait)  ││ (trait)  ││ Mgr(tr)  ││ (trait)  ││ (trait)  │ │
│ └──────────┘└──────────┘└──────────┘└──────────┘└──────────┘ │
│ ┌──────────┐┌──────────┐┌──────────┐┌──────────┐┌──────────┐ │
│ │ Storage  ││ Hook/Reg ││ Journal  ││ McpClient││ ProjDoc  │ │
│ │ (trait)  ││ (trait)  ││ (trait)  ││ (trait)  ││ (trait)  │ │
│ └──────────┘└──────────┘└──────────┘└──────────┘└──────────┘ │
│ providers│tools│context│policy│sandbox│storage│hooks│journal│ │
│   mcp    │memory        (各实现 crate，单一职责)              │
└──────────────────────────────────────────────────────────────┘
                │
┌───────────────▼──────────────────────────────────────────────┐
│                    Infrastructure Layer                      │
│   tokio runtime │ reqwest │ tracing │ serde │ fs │ os keyring │
└──────────────────────────────────────────────────────────────┘
```

---

## 3. 各层职责

### 3.1 Frontend Layer（表现层）

| 组件 | 职责 | 不做的事 |
|------|------|----------|
| `minicoding-cli` | 解析参数、加载配置、构建 Runtime、驱动 Agent、渲染流式输出 | 不直接调用 LLM/工具 |
| `minicoding-tui` | 交互式 UI、多会话视图、工具调用确认弹窗 | 不持有业务状态 |
| `minicoding-sdk` | 供第三方 Rust 程序嵌入，提供高层 `ask()` / `run()` API | 隐藏 Runtime 细节 |

Frontend 只持有对 `Runtime` 的引用，所有业务逻辑下沉到 Orchestration 层。

### 3.2 Orchestration Layer（编排层，`minicoding-core`）

核心运行时，定义并驱动整个 Agent 生命周期：

- **Agent Loop**：`prompt → LLM → (tool_call)* → tool_result → LLM → ... → final` 的循环。
- **Subagent**：派生隔离子上下文执行子任务，结果汇总回主 Agent。
- **Context Manager**：维护消息历史、token 预算、自动压缩与摘要。
- **Planner**（可选）：把复杂任务拆解为多步计划，逐步执行。
- **Event Bus**：广播生命周期事件（`MessageAppended` / `ToolCallStart` / `ToolCallEnd` / `TokenStreamed`），供 frontend 订阅渲染。

### 3.3 Capability Layer（能力层）

以 trait 抽象的"可插拔能力"，定义在 `minicoding-core`，实现在独立 crate：

| Trait | 实现 crate | 说明 |
|-------|-----------|------|
| `LlmProvider` / `Tokenizer` | `minicoding-providers` | OpenAI / Anthropic / Ollama |
| `Tool` / `ToolRegistry` | `minicoding-tools`（内置）/ `minicoding-mcp`（远程包装） | 文件、Shell、Web、Git、Task、Plan、MCP |
| `ContextManager` | `minicoding-context` | token 预算、4 级压缩管道、熔断、降级链 |
| `PermissionPolicy` / `PermissionPrompter` | `minicoding-policy` | 决策引擎、builtin 黑名单、Prompter、ApprovalMode/Preset |
| `SandboxDriver` | `minicoding-sandbox`（core 提供 `NoopDriver` 兜底） | OS 级隔离（sandbox-run + landlock + libseccomp） |
| `Storage` / `AuditSink` | `minicoding-storage` | JSONL 持久化、audit.log 审计 |
| `Hook` / `HookRegistry` | `minicoding-hooks` | 10 类事件、ScriptHook、asyncRewake |
| `Journal` | `minicoding-journal` | FileChangeJournal、/undo 回滚 |
| `McpClient` | `minicoding-mcp` | rmcp 2.2 官方 SDK |
| `ProjectDocLoader` / `MemoryStore` | `minicoding-memory` | AGENTS.md 分层加载、长期/Auto 记忆 |

> **架构变更（v2）**：原 `minicoding-core` 承载多职责（含 Storage/PermissionPolicy/ContextCompressor 默认实现），违反单一职责。重构后 core 精简为"抽象层 + Runtime 编排"（零实现），所有实现下沉到独立领域 crate。详见 `modules.md` §0。

### 3.4 Infrastructure Layer（基础设施层）

通用底层能力，不包含业务语义：异步运行时、HTTP、日志、序列化、文件系统、密钥链。

---

## 4. 关键组件协作

### 4.1 组件关系图

```
        ┌─────────────┐
        │  Frontend   │
        └──────┬──────┘
               │ events (broadcast)
               ▼
        ┌─────────────┐    spawn        ┌──────────────┐
        │ Agent Loop  │ ──────────────▶ │  Subagent    │
        └──────┬──────┘                 └──────┬───────┘
               │                               │
   ┌───────────┼───────────┐                   │
   ▼           ▼           ▼                   ▼
┌──────┐  ┌─────────┐  ┌──────────┐      ┌──────────┐
│ LLM  │  │ Context │  │  Tool    │      │ Storage  │
 │Prov. │  │ Manager │  │Registry  │      │  (log)   │
└──┬───┘  └────┬────┘  └────┬─────┘      └──────────┘
   │           │            │
   │           ▼            ▼
   │      ┌─────────┐  ┌────────────┐
   │      │Tokenizer│  │Permission  │
   │      └─────────┘  │  Policy    │
   │                   └─────┬──────┘
   │                         │
   └───────── stream ────────┘
```

### 4.2 单轮请求时序

```
Frontend ──ask(prompt)──▶ Runtime
                            │
                         build messages (system + history + user)
                            │
                         ContextManager.fit_budget(messages)
                            │
                         LlmProvider.chat_stream(messages, tools)
                            │ ◀── SSE chunks
                         parse tool_call deltas
                            │
              ┌─────────────┴──────────────┐
              ▼                            ▼
        text token ──event──▶ Frontend   tool_call ready
                                            │
                                   PermissionPolicy.check(tool, args)
                                            │
                                   Tool.execute(args)
                                            │
                                   append tool_result to messages
                                            │
                                   (loop back to LlmProvider)
```

---

## 5. 数据流

### 5.1 输入流

1. 用户输入（CLI 参数 / TUI 输入框 / SDK 调用）。
2. Frontend 构造 `UserInput { text, attachments, context_hint }`。
3. Runtime 生成 `Message::user(...)`，交给 `ContextManager`。
4. `ContextManager` 注入系统提示、工具说明、记忆摘要，输出 `ChatRequest`。

### 5.2 LLM 流

1. `LlmProvider.chat_stream` 返回 `Stream<Item = LlmDelta>`。
2. `AgentLoop` 聚合 delta：
   - `Delta::Text(s)` → 转发 `Event::Token(s)`。
   - `Delta::ToolCallStart/Chunk/End` → 累积成完整 `ToolCall`。
3. 流结束后若存在 `ToolCall`，进入工具执行阶段；否则结束本轮。

### 5.3 工具流

1. `AgentLoop` 收集所有 `ToolCall`（支持并行）。
2. 对每个调用：`PermissionPolicy.check` → `Tool.execute` → `ToolResult`。
3. 结果以 `Message::tool_result` 追加到历史。
4. 重新进入 LLM 流，直到无更多工具调用。

### 5.4 输出流

- 文本 token 通过 `Event::Token` 实时广播。
- 工具调用通过 `Event::ToolCall*` 广播，供 Frontend 渲染进度。
- 终止时发送 `Event::TurnEnd { stop_reason }`。
- 全程 `Storage` 以 JSONL 追加写盘。

---

## 6. 控制流：Agent 循环状态机

```
            ┌──────────┐
 start ───▶ │  Idle    │
            └────┬─────┘
                 │ user input
            ┌────▼─────┐
            │ Sending  │ ── LLM 请求中
            └────┬─────┘
                 │ first delta
            ┌────▼─────┐
            │Streaming │ ── 文本/工具增量
            └────┬─────┘
                 │ stream end
            ┌────▼─────┐  有 tool_call   ┌──────────┐
            │  Parse   │ ──────────────▶ │ToolExec  │
            │  Done    │                 └────┬─────┘
            └────┬─────┘                      │ results
                 │ no tool_call                │
            ┌────▼─────┐ ◀────────────────────┘
            │  Done    │
            └──────────┘
```

状态转换全部由事件驱动，Frontend 可订阅状态变化做 UI 切换。

---

## 7. 横切关注点

### 7.1 配置

分层合并优先级（高 → 低）：

```
CLI args  >  Env vars  >  Project config (./.minicoding.toml)
          >  User config (~/.minicoding/config.toml)
          >  Built-in defaults
```

> 用户级与项目级配置路径以 `data-model.md` §3.0 的"路径约定"为权威来源：根目录默认 `~/.minicoding/`，可用 `MINICODING_HOME` 覆盖。本项目**不**采用 XDG 多目录分散方案，避免文档间路径漂移。

### 7.2 错误与中断

- `Ctrl-C` 触发 graceful shutdown：停止流式、记录已产生消息、退出。
- 工具执行错误转换为 `ToolResult::error`，回灌给 LLM 而非终止循环（除非配置 `fail_fast`）。
- LLM 超时/限流：指数退避重试 N 次，仍失败则 `Event::Error` 并保留现场。

### 7.3 可观测性

OpenTelemetry 为一等公民（详见 `design.md` §15、`tech-stack.md` §7）：

- `tracing` span 层级：`session > turn > (context.build | llm.chat_stream > retry | tool.call > (permission.check | permission.prompt | tool.dispatch))`。
- 通过 `tracing-opentelemetry` 桥接为 OTLP，导出到 Jaeger/Tempo/Grafana；后端由 `OTEL_EXPORTER_OTLP_ENDPOINT` 配置。
- 每个工具调用记录 OTel 属性：工具名、`side_effect`、是否并行、耗时、结果大小、是否截断、权限 verdict。
- 本地文件日志（`tracing-appender`）与 OTel 共享同一 `tracing` 调用点，无重复埋点。
- 会话 JSONL 可作为回放源（`--replay <session.jsonl>`），与 trace 通过 `session.id` + `turn.index` 关联。

### 7.4 并发模型

- 主 Agent 循环单线程驱动（消息顺序敏感）。
- 同一轮的**无副作用** `ToolCall` 可并行执行（`buffer_unordered`）；**有副作用** `ToolCall` 严格串行（见 `design.md` §2.3）。
- Subagent 独立 `tokio::task`，结果通过 channel 回传；OTel Context 随之传播，子任务挂在主会话 trace 下。
- 长任务（Bash、WebFetch）可后台执行并轮询。

---

## 8. 部署形态

| 形态 | 说明 |
|------|------|
| 单机 CLI | 默认，二进制自包含 |
| 嵌入 SDK | 作为 crate 被其他 Rust 程序依赖 |
| Server 模式（后续） | `minicoding serve` 暴露 HTTP/JSON-RPC，供编辑器插件调用 |
| MCP Server（后续） | 实现 Model Context Protocol server，反向被其他 Agent 调用 |

---

## 9. 架构决策记录（ADR 索引）

| 编号 | 决策 | 状态 |
|------|------|------|
| ADR-001 | 采用多 crate workspace 而非单 crate 模块 | Accepted |
| ADR-002 | LLM/Tool/Storage 均以 trait 抽象，注册表注入 | Accepted |
| ADR-003 | 会话持久化使用 JSONL 追加写 | Accepted |
| ADR-004 | 上下文压缩采用"摘要 + 截断"双策略 | Accepted |
| ADR-005 | 权限默认 `ask`，可配置 `allow/deny` 列表 | Accepted |

详细 ADR 内容后续在 `docs/adr/` 维护。
