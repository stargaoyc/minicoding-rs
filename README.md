# minicoding-rs

> 一个使用 Rust 实现的轻量级 AI Coding 助手（类 Claude Code / Aider 的终端智能体）

`minicoding-rs` 是一个以 Rust 编写的命令行 AI 编程助手。它通过大语言模型（LLM）与一组本地工具（文件读写、Shell 执行、代码检索等）的组合，在终端中完成"理解需求 → 读取代码 → 修改代码 → 验证结果"的闭环，目标是提供一个**高性能、可嵌入、可扩展、安全可控**的智能体运行时。

---

## 1. 项目目标

| 维度 | 目标 |
|------|------|
| 性能 | 冷启动 < 50ms，单轮工具调用 < 10ms 调度开销，流式首 token 延迟与上游一致 |
| 可扩展 | 工具、LLM Provider、上下文压缩策略、权限策略均可通过 trait + 注册表扩展 |
| 安全 | 所有副作用操作（写文件、执行命令、网络）必须经过权限策略审核并留痕 |
| 可嵌入 | 核心运行时以 library crate 形式提供，CLI 只是其中一个 frontend |
| 可观测 | 全链路 trace（OpenTelemetry 兼容）、会话可回放、工具调用可审计 |

## 2. 核心特性

- **多 Provider 抽象**：统一支持 OpenAI 兼容接口、Anthropic、本地模型（通过 Ollama / llama.cpp HTTP）。
- **工具系统**：基于 `Tool` trait 的注册式工具，内置文件操作（read/write/edit/multiedit/delete/glob/grep）、Shell（run/background/output/kill）、Web 抓取、Git、TaskCreate/TaskUpdate/TaskList（增量任务管理）、Plan、MCP 远程工具等。
- **Agent 循环**：支持单轮、多轮；无副作用工具并行、有副作用工具严格串行；支持类型化子 Agent（Explore/Plan/General/Custom）隔离上下文。
- **上下文管理**：基于 token 预算的 4 级压缩管道（裁剪→摘要→滚动→硬截断）；长期记忆双文件（md + index.json）+ mtime 缓存。
- **项目记忆（AGENTS.md）**：分层加载的静态指令层（参考 Codex/CC），随仓库版本化，Agent 不可自主编辑；兼容 `CLAUDE.md`/`.cursorrules` fallback。
- **权限模型**：`PermissionPolicy`/`PermissionPrompter` 双抽象（决策与交互分离）；per-tool allow/ask/deny + Codex 风格审批模式（Untrusted/OnFailure/OnRequest/Never）与预设（read-only/auto/external-sandbox/full-access）。
- **OS 级沙箱（一等公民）**：基于 `sandbox-run` + `landlock` + `libseccomp` 主流库（不自研胶水），macOS/Linux/Windows 内核级隔离作为应用层权限之外的第二道防线；支持 `external-sandbox`（CI/容器）与 `danger-full-access`（显式确认）。
- **Hooks 系统（参考 Claude Code）**：10 类生命周期事件（PreToolUse/PostToolUse/PostToolUseFailure/PreCompact/PostCompact/PermissionRequest/...），外部脚本 + JSON over stdio 协议，可拦截/改写/注入；含 asyncRewake 异步唤醒；L0 硬约束不可被 Hook 覆盖。
- **MCP 集成**：作为 MCP client 连接外部 server（GitHub/Slack/数据库），`mcp__<server>__<tool>` 命名，project 作用域首次批准防恶意仓库植入；亦可作为 MCP server 被其他 Agent 调用。
- **Plan 模式**：双重只读强制（硬门 + 软引导），`plan.exit` 提交计划并预批准，参考 Claude Code。
- **文件改动回滚**：`/undo` 会话内 operation 级撤销（`FileChangeJournal`，特性门控）。
- **流式输出**：SSE / chunked 流式解析，支持工具调用增量解析。
- **会话持久化**：JSONL 格式的会话日志，支持恢复与回放（默认禁副作用）。
- **可观测性**：OpenTelemetry 一等公民（全链路 span：session/turn/llm/tool/permission/hook/mcp），OTLP 导出对接 Jaeger/Tempo/Grafana。
- **TUI / CLI 双形态**：CLI 适合脚本化（含 `minicoding exec` 批量模式），TUI 适合交互。

## 3. 快速开始

```bash
# 构建
cargo build --release

# 单次提问
minicoding "解释 src/main.rs 的入口逻辑"

# 交互式会话
minicoding --session

# 指定 Provider 与模型
minicoding --provider anthropic --model claude-sonnet-4 "重构 utils 模块"

# 使用预设（审批模式 × 沙箱策略）
minicoding --preset auto "重构 utils 模块"           # 默认：工作区写 + OnRequest
minicoding --preset read-only "审计依赖图"           # 只读 + OnRequest
minicoding exec --sandbox external-sandbox "跑测试"  # CI/容器内批量执行
```

## 4. 项目结构

```
minicoding-rs/
├── Cargo.toml
├── README.md
├── docs/                       # 设计文档
│   ├── architecture.md         # 总体架构
│   ├── design.md               # 详细设计（核心）
│   ├── modules.md              # 模块详细设计（14 crate）
│   ├── api.md                  # 接口设计
│   ├── data-model.md           # 数据模型与存储
│   ├── security.md             # 安全与权限（沙箱为一等公民）
│   ├── hooks.md                # Hooks 系统设计（10 事件 + asyncRewake）
│   ├── tech-stack.md           # 技术选型
│   ├── roadmap.md              # 开发路线图
│   ├── dev-plan.md             # 详细开发计划（task 级）
│   ├── features.md             # 功能清单（141 项）
│   └── rules.md                # 设计时大模型约束（C-01..C-35）
├── crates/
│   ├── minicoding-core/        # 抽象层：数据模型 + trait 定义 + Runtime 编排（零实现）
│   ├── minicoding-context/     # ContextManager 实现 + 压缩管道 + 熔断
│   ├── minicoding-policy/      # 权限实现 + builtin 黑名单 + Prompter + ApprovalMode
│   ├── minicoding-memory/      # 长期/Auto/会话记忆 + AGENTS.md loader
│   ├── minicoding-hooks/       # HookRegistry + ScriptHook + asyncRewake
│   ├── minicoding-journal/     # FileChangeJournal + /undo
│   ├── minicoding-sandbox/     # OS 沙箱驱动（sandbox-run + landlock + libseccomp）
│   ├── minicoding-mcp/         # MCP client/server（rmcp 2.2 官方 SDK）
│   ├── minicoding-storage/     # JSONL 存储 + audit.log 审计
│   ├── minicoding-providers/   # LLM Provider 实现（OpenAI/Anthropic/Ollama）
│   ├── minicoding-tools/       # 内置 Tool 实现（组合层，fs/shell/web/git/task/plan/mcp）
│   ├── minicoding-cli/         # CLI frontend
│   ├── minicoding-tui/         # TUI frontend（M7）
│   └── minicoding-sdk/         # 对外嵌入 SDK（M8）
└── tests/                      # 集成测试
```

## 5. 文档导航

| 文档 | 内容 |
|------|------|
| [架构设计](docs/architecture.md) | 分层架构、组件关系、数据流 |
| [详细设计](docs/design.md) | Agent 循环、上下文管理、工具调度、Plan/Undo/Task/MCP/Hooks 等核心机制 |
| [模块设计](docs/modules.md) | 14 个 crate 的职责边界、内部结构、依赖方向 |
| [接口设计](docs/api.md) | 核心 trait、公共 API、配置 schema |
| [数据模型](docs/data-model.md) | Message / Session / ToolCall 等数据结构与持久化 |
| [安全与权限](docs/security.md) | 权限策略、审计、OS 沙箱（sandbox-run）、审批模式与预设 |
| [Hooks 设计](docs/hooks.md) | 10 类生命周期 Hook、协议、asyncRewake、安全约束 |
| [技术选型](docs/tech-stack.md) | 依赖库选择与理由（沙箱 sandbox-run/MCP rmcp 2.2） |
| [开发路线图](docs/roadmap.md) | 里程碑与交付计划 |
| [开发计划](docs/dev-plan.md) | 任务级开发计划（70 个 task，含验收标准与依赖） |
| [功能清单](docs/features.md) | 全功能总账（按领域分组，141 项） |
| [大模型约束](docs/rules.md) | 设计时对 LLM 施加的 L0/L1/L2 约束（C-01..C-35） |
| [AI 编码约束](AGENTS.md) | AI 助手开发本项目时的编码/架构/文档/安全规范 |

## 6. 许可证

MIT 或 Apache-2.0 双许可（待定）。
