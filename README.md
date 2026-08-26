# minicoding-rs

> 一个使用 Rust 实现的轻量级 AI Coding 助手，支持 CLI / TUI / Web / 桌面四形态

`minicoding-rs` 是一个以 Rust 编写的 AI 编程助手。它通过大语言模型（LLM）与一组本地工具（文件读写、Shell 执行、代码检索等）的组合，完成"理解需求 → 读取代码 → 修改代码 → 验证结果"的闭环，目标是提供一个**高性能、可嵌入、可扩展、安全可控**的智能体运行时。

---

## 1. 项目目标

| 维度 | 目标 |
|------|------|
| 可扩展 | 工具、LLM Provider、上下文压缩策略、权限策略均可通过 trait + 注册表扩展 |
| 安全 | 所有副作用操作（写文件、执行命令、网络）必须经过权限策略审核并留痕 |
| 可嵌入 | 核心运行时以 library crate 形式提供，CLI/TUI/Web/桌面都是 frontend |
| 可观测 | 全链路 trace（OpenTelemetry 兼容）、会话可回放、工具调用可审计 |

## 2. 核心特性

- **多 Provider 抽象**：统一支持 OpenAI 兼容接口、Anthropic、本地模型（通过 Ollama / llama.cpp HTTP）。
- **工具系统**：基于 `Tool` trait 的注册式工具，内置文件操作（read/write/edit/multiedit/delete/glob/grep）、Shell（run/background/output/kill）、Web 抓取、Git、TaskCreate/TaskUpdate/TaskList（增量任务管理）、Plan、MCP 远程工具等。
- **Agent 循环**：支持单轮、多轮；无副作用工具并行、有副作用工具严格串行；支持类型化子 Agent（Explore/Plan/General/Custom）隔离上下文。
- **上下文管理**：基于 token 预算的 4 级压缩管道（裁剪→摘要→滚动→硬截断）；长期记忆双文件（md + index.json）+ mtime 缓存。
- **项目记忆（AGENTS.md）**：分层加载的静态指令层，随仓库版本化，Agent 不可自主编辑；兼容 `CLAUDE.md`/`.cursorrules` fallback。
- **权限模型**：`PermissionPolicy`/`PermissionPrompter` 双抽象（决策与交互分离）；per-tool allow/ask/deny + 审批模式（Untrusted/OnFailure/OnRequest/Never）与预设（read-only/auto/external-sandbox/full-access）。
- **OS 级沙箱（一等公民）**：基于 Linux `landlock`（内核 LSM，fork 后 exec 前 `pre_exec` 应用）+ macOS Seatbelt（`sandbox_init` FFI）+ Windows Job Object 的自研轻量驱动；macOS/Linux/Windows 内核级隔离作为应用层权限之外的第二道防线；支持 `external-sandbox`（CI/容器）与 `danger-full-access`（显式确认）。
- **Hooks 系统**：10 类生命周期事件（PreToolUse/PostToolUse/PostToolUseFailure/PreCompact/PostCompact/PermissionRequest/...），外部脚本 + JSON over stdio 协议，可拦截/改写/注入；含 asyncRewake 异步唤醒；L0 硬约束不可被 Hook 覆盖。
- **MCP 集成**：作为 MCP client 连接外部 server（GitHub/Slack/数据库），`mcp__<server>__<tool>` 命名，project 作用域首次批准防恶意仓库植入；亦可作为 MCP server 被其他 Agent 调用。
- **Plan 模式**：双重只读强制（硬门 + 软引导），`plan.exit` 提交计划并预批准。
- **文件改动回滚**：`/undo` 会话内 operation 级撤销（`FileChangeJournal`，特性门控；默认关闭、纯内存——不落盘，仅会话内有效）。
- **Event Sourcing**：会话状态建模为不可变事件流，支持快照回放、SSE 游标恢复、跨会话 fork。
- **流式输出**：SSE / chunked 流式解析，支持工具调用增量解析。
- **会话持久化**：JSONL 格式的会话日志 + Event Store，支持恢复与回放（默认禁副作用）。
- **可观测性**：OpenTelemetry 一等公民（全链路 span：session/turn/llm/tool/permission/hook/mcp），OTLP 导出对接 Jaeger/Tempo/Grafana。
- **四形态前端**：CLI（脚本化 + `minicoding exec` 批量模式）、TUI（全屏交互）、Web（React 19 + Vite + Tailwind v4 现代界面）、桌面（Tauri 2.x sidecar + 系统托盘 + 全局快捷键）。

## 3. 快速开始

```bash
# 构建
cargo build --release

# CLI 单次提问
minicoding "解释 src/main.rs 的入口逻辑"

# 交互式会话
minicoding --session

# 指定 Provider 与模型
minicoding --provider anthropic --model claude-sonnet-4 "重构 utils 模块"

# 连接 OpenAI 兼容 API（DeepSeek/Moonshot/vLLM 等）
minicoding --provider openai --provider-name deepseek \
  --api-base https://api.deepseek.com \
  --model deepseek-chat "重构 utils 模块"

# 持久化配置到 config.toml（避免每次输入参数）
# ~/.minicoding/config.toml:
# [provider]
# default = "openai"
# name = "deepseek"
# api_base = "https://api.deepseek.com"
# model = "deepseek-chat"
minicoding "重构 utils 模块"  # 自动读取 config.toml

# 安全模式
minicoding --plan "审计依赖图"                # Plan：只读工具面（禁写）
minicoding exec --sandbox external-sandbox "跑测试"  # CI/容器内批量执行（默认沙箱拒绝熔断）
minicoding-server --preset read-only          # server 模式安全预设（auto/read-only/external-sandbox/full-access）

# 启动 HTTP/SSE server（Web 模式）
minicoding-server --bind 127.0.0.1:8080 --web ./crates/minicoding-web/dist --cors-origin http://localhost:5173

# 作为 MCP server 被其他 Agent 调用
minicoding serve --as-mcp-server
```

## 4. 项目结构

```
minicoding-rs/
├── Cargo.toml
├── dist-workspace.toml          # 跨平台二进制构建配置（cargo-dist）
├── README.md
├── AGENTS.md                    # AI 助手开发约束
├── docs/                        # 设计文档
│   ├── design.md                # 详细设计（核心）
│   ├── modules.md               # 模块详细设计（18 crate + web）
│   ├── api.md                   # 接口设计
│   ├── data-model.md            # 数据模型与存储
│   ├── security.md              # 安全与权限
│   ├── hooks.md                 # Hooks 系统设计
│   ├── tech-stack.md            # 技术选型
│   ├── roadmap.md               # 开发路线图
│   ├── dev-plan.md              # 详细开发计划
│   ├── features.md              # 功能清单
│   ├── rules.md                 # 运行时大模型约束
│   ├── m9-design.md             # M9 Web/桌面设计
│   └── getting-started.md       # 上手指南
├── crates/
│   ├── minicoding-core/         # 抽象层：数据模型 + trait 定义 + Runtime 编排（零实现）
│   ├── minicoding-context/      # ContextManager 实现 + 压缩管道 + 熔断
│   ├── minicoding-policy/       # 权限实现 + builtin 黑名单 + Prompter + ApprovalMode
│   ├── minicoding-memory/       # 长期/Auto/会话记忆 + AGENTS.md loader
│   ├── minicoding-hooks/        # HookRegistry + ScriptHook + asyncRewake
│   ├── minicoding-journal/      # FileChangeJournal + /undo
│   ├── minicoding-sandbox/      # OS 沙箱驱动（Linux landlock / macOS Seatbelt / Windows Job Object）
│   ├── minicoding-mcp/          # MCP client/server（rmcp 2.2 官方 SDK）
│   ├── minicoding-storage/      # JSONL 存储 + audit.log + Event Store
│   ├── minicoding-providers/    # LLM Provider 实现（OpenAI/Anthropic/Ollama）
│   ├── minicoding-tools/        # 内置 Tool 实现（组合层，fs/shell/web/git/task/plan/mcp）
│   ├── minicoding-protocol/     # JSON-RPC 2.0 wire types + Event/Command DTO
│   ├── minicoding-server/       # HTTP/SSE server + ACP/LSP 适配器 + --web 静态托管
│   ├── minicoding-extension-sdk/# 扩展作者稳定 API（Extension trait + Registrar）
│   ├── minicoding-cli/          # CLI frontend
│   ├── minicoding-tui/          # TUI frontend
│   ├── minicoding-sdk/          # 对外嵌入 SDK
│   ├── minicoding-desktop/      # Tauri 2.x 桌面壳（feature gate `desktop`）
│   └── minicoding-web/          # Web 前端（React 19 + Vite + Tailwind v4，独立 npm 项目）
└── tests/                       # 集成测试
```

## 5. 文档导航

### 核心设计文档

| 文档 | 内容 |
|------|------|
| [详细设计](docs/design.md) | Agent 循环、上下文管理、工具调度、Plan/Undo/Task/MCP/Hooks/Event Sourcing/Web/桌面等核心机制 |
| [模块设计](docs/modules.md) | 18 个 crate + web 前端的职责边界、内部结构、依赖方向 |
| [接口设计](docs/api.md) | 核心 trait、公共 API、配置 schema |
| [数据模型](docs/data-model.md) | Message / Session / ToolCall / Event 等数据结构与持久化 |
| [安全与权限](docs/security.md) | 权限策略、审计、OS 沙箱（landlock/Seatbelt/Job Object）、审批模式与预设 |
| [Hooks 设计](docs/hooks.md) | 10 类生命周期 Hook、协议、asyncRewake、安全约束 |
| [技术选型](docs/tech-stack.md) | 依赖库选择与理由（沙箱 sandbox-run/MCP rmcp 2.2/Tauri 2.x） |
| [架构文档](docs/architecture.md) | 分层架构、组件协作、数据流 |

### 开发与规划文档

| 文档 | 内容 |
|------|------|
| [开发路线图](docs/roadmap.md) | 里程碑与交付计划 |
| [开发计划](docs/dev-plan.md) | 任务级开发计划（含验收标准与依赖） |
| [功能清单](docs/features.md) | 全功能总账（按领域分组） |
| [大模型约束](docs/rules.md) | 运行时对 LLM 施加的 L0/L1/L2 约束 |
| [开发过程文档](docs/development-process.md) | 项目开发全过程记录、关键设计决策、里程碑演进 |
| [AI 编码约束](AGENTS.md) | AI 助手开发本项目时的编码/架构/文档/安全/前端规范 |

### 用户与学习文档

| 文档 | 内容 |
|------|------|
| [产品手册](docs/product-manual.md) | 面向最终用户的详细使用手册（安装/配置/四形态/工具/权限/FAQ） |
| [上手指南](docs/getting-started.md) | 从零到运行 + 从 Claude Code / Codex 迁移指南 |
| [构建指南](docs/build-guide.md) | 详细的构建说明（环境/依赖/跨平台/发布/CI/排查） |
| [问题排查](docs/troubleshooting.md) | 常见问题与解决方案（构建/CI/运行时/测试/权限/前端） |

## 6. 四形态前端

| 形态 | 入口 | 适用场景 | 配置方式 |
|------|------|---------|---------|
| CLI | `minicoding` | 脚本化、批量执行（`minicoding exec`）、CI/容器 | `--api-base`/`--provider-name`/`config.toml`/env/keyring |
| TUI | `minicoding-tui`（独立二进制） | 全屏交互式终端会话 | 同 CLI |
| Web | `minicoding-server --web ./dist` | 浏览器访问，远程会话，多客户端 | `--api-base`/`config.toml`/env/keyring + `POST /sessions` body |
| 桌面 | `minicoding-desktop`（feature `desktop`） | Tauri 原生应用，系统托盘 + 全局快捷键 | 应用内设置界面（写入 `config.toml` + keyring） |

所有形态共享统一配置优先级：`CLI 参数 > 环境变量 > config.toml > provider 默认值`。API key 统一存 OS keyring（`KEYRING_SERVICE = "minicoding"`，C-04），CLI/server/desktop 三端共享同一 keyring entry。

## 7. 许可证

AGPL-3.0-only
