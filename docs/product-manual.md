# minicoding-rs 产品手册

> **文档性质**：本文是 `minicoding-rs` 面向最终用户的产品手册，介绍产品定位、安装配置、四形态使用、工具系统、权限安全、上下文管理、会话管理、Hooks、MCP、可观测性、扩展开发与常见问题。文档随版本迭代持续更新，引用的其他文档均为 `docs/` 目录下的相对路径（如 `docs/design.md`、`docs/security.md`）。
>
> **阅读对象**：终端 AI Coding 助手的最终用户（开发者、运维、安全审计人员）。技术细节与设计原理请参考 `docs/` 下各专题文档。

---

## 目录

1. [产品概述](#1-产品概述)
2. [安装与配置](#2-安装与配置)
3. [CLI 模式使用](#3-cli-模式使用)
4. [TUI 模式使用](#4-tui-模式使用)
5. [Web 模式使用](#5-web-模式使用)
6. [桌面模式使用](#6-桌面模式使用)
7. [工具系统详解](#7-工具系统详解)
8. [权限与安全](#8-权限与安全)
9. [上下文管理](#9-上下文管理)
10. [会话管理](#10-会话管理)
11. [Hooks 系统](#11-hooks-系统)
12. [MCP 集成](#12-mcp-集成)
13. [可观测性](#13-可观测性)
14. [扩展开发](#14-扩展开发)
15. [FAQ](#15-faq)

---

## 1. 产品概述

### 1.1 产品定位与目标

`minicoding-rs` 是一个使用 Rust 实现的轻量级终端 AI Coding 助手（参考 Claude Code / Codex CLI / Aider 设计），通过大语言模型（LLM）与一组本地工具（文件读写、Shell 执行、代码检索等）的组合，完成"理解需求 → 读取代码 → 修改代码 → 验证结果"的闭环。

产品目标（见 `README.md` §1）：

| 维度 | 目标 |
|------|------|
| 性能 | 冷启动 < 50ms，单轮工具调用 < 10ms 调度开销，流式首 token 延迟与上游一致 |
| 可扩展 | 工具、LLM Provider、上下文压缩策略、权限策略均可通过 trait + 注册表扩展 |
| 安全 | 所有副作用操作（写文件、执行命令、网络）必须经过权限策略审核并留痕 |
| 可嵌入 | 核心运行时以 library crate 形式提供，CLI/TUI/Web/桌面都是 frontend |
| 可观测 | 全链路 trace（OpenTelemetry 兼容）、会话可回放、工具调用可审计 |

### 1.2 核心特性总览

- **多 Provider 抽象**：统一支持 OpenAI 兼容接口、Anthropic、本地模型（Ollama / llama.cpp HTTP）。
- **工具系统**：基于 `Tool` trait 的注册式工具，内置文件操作（read/write/edit/multiedit/delete/glob/grep/list）、Shell（run/background/output/kill）、Web 抓取、Git、TaskCreate/TaskUpdate/TaskList（增量任务管理）、Plan、MCP 远程工具等（共 22 项，见 `docs/features.md` §3）。
- **Agent 循环**：支持单轮、多轮；无副作用工具并行、有副作用工具严格串行；支持类型化子 Agent（Explore/Plan/General/Custom）隔离上下文。
- **上下文管理**：基于 token 预算的 4 级压缩管道（裁剪→摘要→滚动→硬截断）；长期记忆双文件（md + index.json）+ mtime 缓存；预测性压缩与 Post-compact 上下文恢复。
- **项目记忆（AGENTS.md）**：分层加载的静态指令层（参考 Codex/CC），随仓库版本化，Agent 不可自主编辑；兼容 `CLAUDE.md`/`.cursorrules` fallback。
- **权限模型**：`PermissionPolicy`/`PermissionPrompter` 双抽象（决策与交互分离）；per-tool allow/ask/deny + Codex 风格审批模式（Untrusted/OnFailure/OnRequest/Never）与预设（read-only/auto/external-sandbox/full-access）。
- **OS 级沙箱（一等公民）**：基于 `sandbox-run` + `landlock` + `libseccomp` 主流库（不自研胶水），macOS/Linux/Windows 内核级隔离作为应用层权限之外的第二道防线。
- **Hooks 系统**：10 类生命周期事件，外部脚本 + JSON over stdio 协议，可拦截/改写/注入；含 asyncRewake 异步唤醒；L0 硬约束不可被 Hook 覆盖。
- **MCP 集成**：作为 MCP client 连接外部 server（GitHub/Slack/数据库），`mcp__<server>__<tool>` 命名，project 作用域首次批准防恶意仓库植入；亦可作为 MCP server 被其他 Agent 调用。
- **Plan 模式**：双重只读强制（硬门 + 软引导），`plan.exit` 提交计划并预批准。
- **文件改动回滚**：`/undo` 会话内 operation 级撤销（`FileChangeJournal`，特性门控）。
- **会话持久化**：JSONL 格式的会话日志 + Event Store，支持恢复与回放（默认禁副作用）；Event Sourcing 支持快照重放、SSE 游标恢复、跨会话 fork。
- **可观测性**：OpenTelemetry 一等公民（全链路 span：session/turn/llm/tool/permission/hook/mcp），OTLP 导出对接 Jaeger/Tempo/Grafana。
- **四形态前端**：CLI、TUI（全屏交互）、Web（React 19 + Vite + Tailwind v4）、桌面（Tauri 2.x sidecar + 系统托盘 + 全局快捷键）。

### 1.3 适用场景

- **日常开发辅助**：在终端中用自然语言描述需求，由 Agent 读取代码、修改、运行测试、提交 diff。
- **代码审计与诊断**：以只读沙箱分析陌生代码库、梳理依赖图、定位 bug。
- **批量任务与 CI 集成**：通过 `minicoding exec` 在容器/CI 中非交互执行批量重构、依赖升级、文档生成等任务。
- **远程与多客户端**：通过 Web/桌面形态在浏览器或原生应用中与 Agent 交互，适合团队共享会话、远程办公。
- **可嵌入与二次开发**：通过 `minicoding-sdk` / HTTP/JSON-RPC / MCP server / LSP server 嵌入编辑器（VS Code/Neovim/Emacs/Helix）或被其他 Agent 调用。
- **长时自动化任务**：结合 Auto-Review 子代理、asyncRewake Hooks，让 Agent 在受控范围内长时间运行复杂任务。

### 1.4 与同类工具对比

下表对比 `minicoding-rs` 与 Claude Code（CC）、Codex CLI、Aider（详见 `docs/getting-started.md` §2）：

| 维度 | minicoding-rs | Claude Code | Codex CLI | Aider |
|------|--------------|-------------|-----------|-------|
| 实现语言 | Rust（edition 2024，MSRV 1.99+） | TypeScript/Node | Rust | Python |
| 内存安全 | 编译期保证，无 GC 暂停 | 运行时 GC | 编译期保证 | 运行时 GC |
| 沙箱机制 | OS 级一等公民：`sandbox-run` + Landlock + libseccomp + Seatbelt + Windows 受限令牌，两道防线 | 应用层为主 | Landlock + libseccomp（参考对象） | 无内核级沙箱 |
| 沙箱默认状态 | Opt-out（`WorkspaceWrite` 默认启用内核隔离） | Opt-in | Opt-out | N/A |
| MCP 支持 | `rmcp` 2.2 官方 SDK，stdio + HTTP + OAuth，project 作用域首次批准 | 支持 | 支持 | 不支持 |
| Hooks 系统 | 10 类事件 + ScriptHook + asyncRewake，L0 不可覆盖 | 27 类事件，依赖自觉 | 无 | 无 |
| 权限模型 | 两层：L0 硬黑名单 + L1 用户策略，决策与交互分离 | 单层 allow/deny，依赖 Hook | 两层（builtin + user） | 简单确认 |
| 记忆系统 | 三层：工作记忆 + 会话摘要 + 长期记忆双文件 + Auto memory + AGENTS.md | CLAUDE.md + Auto memory | AGENTS.md | 单文件约定 |
| 可观测性 | OpenTelemetry 一等公民（M0 起接入），全链路 span | 无统一 trace | tracing 日志 | 无 |
| 可嵌入性 | 18 crate workspace，`minicoding-sdk` 提供 `Client`/`ask`/`run_task` API | 不可嵌入 | 不可嵌入 | 不可嵌入 |
| 部署形态 | CLI / TUI / SDK / HTTP server / MCP server / LSP server / Web / 桌面 | CLI | CLI | CLI |
| 配置格式 | TOML（`~/.minicoding/config.toml`） | JSON | TOML | INI/CLI |
| 开源协议 | AGPL-3.0-only | 闭源 | Apache-2.0 | Apache-2.0 |

**核心差异化价值**：

1. **Rust 内存安全 + 零成本抽象**：原生二进制，冷启动远优于 Node/Python 实现，适合 CLI。
2. **OpenTelemetry 一等公民**：从 M0 起接入，生产环境下的 Agent 行为分析、性能瓶颈定位、异常归因成为可能。
3. **L0 硬约束不可绕过**：35 条约束中 L0 在实现层被强制，不依赖 LLM 自觉或系统提示词（见 `docs/rules.md`）。
4. **OS 沙箱一等公民 + 两道防线**：应用层（路径沙箱 + 权限策略 + 黑名单）+ OS 层（Landlock/Seatbelt/受限令牌）独立两道防线；Opt-out 而非 opt-in。
5. **18 crate 可嵌入 + 多部署形态**：`minicoding-core` 可被其他 Rust 项目直接依赖，trait 定义集中在 core，实现可来自任意 crate。
6. **AGENTS.md 兼容 + 跨工具迁移**：支持 `CLAUDE.md`/`.cursorrules` 作为 fallback 文件名，无需改名即可复用项目记忆。

---

## 2. 安装与配置

### 2.1 安装方式

#### 方式一：从源码构建（开发期推荐）

```bash
git clone <repo-url> minicoding-rs
cd minicoding-rs
cargo build --release -p minicoding-cli
# 二进制位于 target/release/minicoding
```

首次构建会拉取 `tokio`/`reqwest`/`rmcp`/`ratatui` 等依赖，耗时 3-8 分钟。`Cargo.lock` 已提交（CLI 项目约定），无需手动锁定版本。

#### 方式二：cargo install

```bash
cargo install minicoding
```

#### 方式三：包管理器（M10 分发，见 `docs/features.md` Q-08/Q-09）

```bash
# macOS
brew install minicoding
# Windows
scoop install minicoding
# Linux
cargo install minicoding
```

`cargo-dist.toml` 配置 5 个 target + shell/powershell/homebrew/scoop 安装器，覆盖三渠道分发。

#### 前置条件

| 项 | 要求 | 来源 |
|----|------|------|
| Rust 工具链 | edition 2024，MSRV 1.99+ | `AGENTS.md` §2.1 |
| Git | 用于 `git.diff`/`git.apply` 与 VCS 目录检测 | 各平台包管理器 |
| libseccomp（Linux 启用沙箱时） | 开发头文件 | Debian/Ubuntu: `sudo apt install libseccomp-dev` |

HTTP 走 `reqwest` + `rustls`（不依赖系统 OpenSSL）；`landlock`/`rmcp`/`sandbox-run` 均为纯 Rust，无 C 依赖。**唯一需要系统包的是 Linux 下的 `libseccomp`**（用于系统调用过滤）。

### 2.2 首次配置（API Key、keyring）

凭证只从环境变量或 OS keyring 读取，**绝不**写入配置文件明文（`docs/security.md` §6、`AGENTS.md` §5.3）。

**环境变量方式**（CI/容器场景首选）：

```bash
# OpenAI 兼容（M1 默认 provider）
export OPENAI_API_KEY="sk-..."

# 或 Anthropic（M6 交付）
export ANTHROPIC_API_KEY="sk-ant-..."

# 或本地 Ollama（M6 交付，无需 key）
# 确保 ollama serve 在 127.0.0.1:11434 运行
```

Windows PowerShell：

```powershell
$env:OPENAI_API_KEY = "sk-..."
```

**OS keyring 方式**（交互场景推荐，M4 交付）：

```bash
minicoding auth login --provider anthropic
# 输入密钥（不回显）→ 写入 OS keyring
minicoding auth status
minicoding auth logout --provider anthropic
```

凭证读取优先级（见 `docs/security.md` §6.1）：

| 来源 | 优先级 | 说明 |
|------|--------|------|
| CLI `--api-key` | 0（最高） | 临时覆盖 |
| 环境变量（`OPENAI_API_KEY` 等） | 1 | CI/容器场景首选 |
| OS keyring（`keyring` crate） | 2 | 交互场景首选，`minicoding auth login` 写入 |
| 文件 fallback `~/.minicoding/credentials`（0600） | 3 | keyring 不可用降级 |
| 配置文件 `api_key` 字段 | 4 | **强烈不推荐**，仅本地调试；启动告警 |

### 2.3 配置文件详解（`~/.minicoding/config.toml`）

完整 schema 见 `docs/api.md` §6。以下为关键配置示例：

```toml
# ~/.minicoding/config.toml  （根目录可由 MINICODING_HOME 覆盖）

[provider]
default = "anthropic"
# 自定义显示名（可选，用于日志/metrics，不影响协议分派）
# 连接 OpenAI 兼容 API（DeepSeek/Moonshot/vLLM 等）时设置可读名称
# name = "deepseek"
# API base URL（可选，省略时按 provider 选默认：
#   openai → https://api.openai.com
#   anthropic → https://api.anthropic.com
#   ollama → http://localhost:11434）
# api_base = "https://api.deepseek.com"
# model = "deepseek-chat"
# api_key = ""  # 留空，从 keyring/环境变量读取（推荐）
# timeout_sec = 120
# retry = { max_attempts = 4, base_delay_ms = 500 }

# Anthropic 示例（默认 provider）
[provider.anthropic]
model = "claude-sonnet-4"
api_key_env = "ANTHROPIC_API_KEY"
timeout_sec = 120
retry = { max_attempts = 4, base_delay_ms = 500 }

# 独立小 LLM（为摘要/compact/memory 提取配置独立 provider，可配更便宜模型降本）
# api_base/api_key 为 None 时继承主 [provider] 配置
[provider.small]
model = "claude-haiku-4"
api_key_env = "ANTHROPIC_API_KEY"

[context]
budget_ratio = 0.85
compress = true
max_tool_iters = 50
turn_timeout_sec = 600
predictive_compact_enabled = false           # 默认关，长会话场景开启
predictive_baseline_growth_tokens = 15000
post_compact_max_files = 5
post_compact_token_budget = 50000

[tools]
enabled_groups = ["core", "fs", "shell", "web"]
[tools.fs]
max_read_bytes = 1048576
[tools.shell]
timeout_sec = 120
max_output_bytes = 1048576
[tools.web]
allowed_domains = ["github.com", "*.githubusercontent.com", "crates.io"]
deny_domains = ["*.internal.corp"]

[permission]
default = "ask"
non_tty_strategy = "deny"      # deny(默认) | allow | fail
[[permission.allow]]
tool = "fs.write"
glob = "src/**"
[[permission.deny]]
tool = "shell.run"
command_prefix = ["rm -rf", "sudo"]

# 审批模式 × 沙箱策略（预设），见 docs/security.md §2.6/§8
[approval]
mode = "on-request"            # untrusted | on-failure | on-request(默认) | never
[sandbox]
policy = "workspace-write"     # read-only | workspace-write(默认) | danger-full-access
allow_dotgit_write = false     # 强烈不推荐开启
allow_network = ["api.anthropic.com", "api.openai.com"]
extra_writable = ["target/", "dist/"]

# 命名预设，CLI --preset <name> 选用
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

# 项目记忆分层加载（见 docs/design.md §8.6）
[project]
project_doc_fallback_filenames = ["CLAUDE.md", ".cursorrules", "TEAM_GUIDE.md"]
project_doc_max_bytes = 32768

# Hooks（见 docs/hooks.md §6）
[hooks]
on_hook_error = "continue"     # continue(默认) | deny | fail
default_timeout_sec = 30

# 特性门控（opt-in 功能）
[features]
file_undo = false              # /undo 文件回滚（参考 Codex features.undo，默认关）
plan_mode = true               # Plan 模式
typed_subagents = true         # 类型化子 Agent

# MCP server 配置（见 docs/design.md §19.2）
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

### 2.4 配置加载优先级

配置加载优先级（高 → 低，见 `docs/getting-started.md` §3.1）：

```
CLI args > Env vars > Project config (./.minicoding.toml)
         > User config (~/.minicoding/config.toml) > Built-in defaults
```

- **CLI args**：`--provider`/`--model`/`--preset`/`--sandbox`/`--allow`/`--deny` 等命令行参数优先级最高，仅本次运行有效。
- **Env vars**：环境变量次之，如 `OPENAI_API_KEY`/`OTEL_EXPORTER_OTLP_ENDPOINT`。
- **Project config**：工作目录下的 `.minicoding.toml`，随仓库版本化，团队共享。
- **User config**：`~/.minicoding/config.toml`（或 `$MINICODING_HOME/config.toml`），用户级配置。
- **Built-in defaults**：内置默认值。

**配置热更新**（S-22）：`ConfigWatcher`（基于 `notify` 8）监听 `config.toml` 变化，500ms debounce 后广播 `Event::ConfigChanged`，扩展通过 `on_config_changed()` 接收变更（best-effort，不保证所有配置项热生效）。

**last-known-good 回退**（S-20）：解析成功时原子写入 `~/.minicoding/.last-known-good.toml`，解析失败时回退到上次成功的配置，避免配置错误导致启动失败。

### 2.5 环境变量参考

| 环境变量 | 用途 | 默认值 |
|---------|------|--------|
| `MINICODING_HOME` | 根目录覆盖（默认 `~/.minicoding/`） | `~/.minicoding/` |
| `OPENAI_API_KEY` | OpenAI 兼容 provider 凭证 | — |
| `ANTHROPIC_API_KEY` | Anthropic provider 凭证 | — |
| `OPENAI_PROVIDER` | `minicoding serve` 默认 provider 类型 | `openai` |
| `OPENAI_API_BASE` | `minicoding serve` API base URL | 按 provider 选默认 |
| `OPENAI_MODEL` | `minicoding serve` 默认模型 | `gpt-4o` |
| `MINICODING_API_KEY` | CI 场景 API Key（不写入 `.minicoding.toml`） | — |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OpenTelemetry OTLP 导出端点 | 未设置则降级本地 fmt 日志 |
| `OTEL_TRACES_SAMPLER` | 采样策略（`always_on`/`trace_id_ratio`） | `always_on` |
| `RUST_LOG` | 日志级别覆盖（文件始终 DEBUG） | `info` |
| `GITHUB_TOKEN` | MCP server 环境变量展开（`${GITHUB_TOKEN}`） | — |

环境变量引用语法（S-21）：配置文件中统一使用 `env:VAR_NAME` 或 `${VAR_NAME}` 语法引用环境变量，支持 `env:VAR:-fallback` / `${VAR:-fallback}` 回退。

---

## 3. CLI 模式使用

CLI 是最常用的形态，适合脚本化、批量执行（`minicoding exec`）、CI/容器场景。

### 3.1 单次提问模式

```bash
# 单次提问（M1 交付）
minicoding "解释 src/main.rs 的入口逻辑"

# 指定 Provider 与模型
minicoding --provider anthropic --model claude-sonnet-4 "重构 utils 模块"

# 使用预设（审批模式 × 沙箱策略）
minicoding --preset auto "重构 utils 模块"           # 默认：工作区写 + OnRequest
minicoding --preset read-only "审计依赖图"           # 只读 + OnRequest
minicoding --preset full-access "全自动部署"         # 需显式确认 + red 警告
```

单次模式下，Agent 循环执行直到 `EndTurn`，流式输出 token 到 stdout。非 TTY 环境（管道/CI）自动降级：禁 spinner/颜色，`NonInteractivePrompter` 按 `permission.non_tty_strategy` 处理权限（默认 `deny`）。

### 3.2 交互会话模式（REPL）

```bash
# 交互会话（M2 交付基础 REPL，M5 起 /undo /plan /mcp 命令）
minicoding --session

# 恢复指定会话继续提问
minicoding --resume sess_01H...

# Fork 会话从分叉点尝试不同方向
minicoding --fork-session sess_01H...
```

REPL 内支持斜杠命令（见 §3.4）。

### 3.3 exec 批量执行模式

`minicoding exec` 是非交互模式，专为 CI/脚本/batch 设计（对齐 Codex `codex exec`，见 `docs/security.md` §9）：

```bash
# 非 TTY 批量执行（M4 交付，默认 read-only 沙箱）
minicoding exec --sandbox read-only "总结 README"

# CI/容器内批量执行（外层容器已隔离，避免双重沙箱）
minicoding exec --sandbox external-sandbox "跑全套测试"

# 工作区写（CI 改动任务）
minicoding exec --sandbox workspace-write "重构 utils 模块"

# JSON 流输出（可被 jq 解析）
minicoding exec --json "审计依赖图" | jq 'select(.type == "tool_call")'

# 跳过持久化 session 文件（CI 几乎总需要）
minicoding exec --ephemeral "生成 API 文档"
```

exec 模式语义：

- **stderr** 流式输出进度日志；
- **stdout** 仅输出最终 agent 消息（可安全 pipe/capture）；
- **`--json`** 切换 stdout 为 JSONL 流（每个事件——命令执行、文件变更、agent 消息——都是结构化对象）；
- **`--ephemeral`** 跳过持久化 session 文件；
- **默认沙箱 `read-only`**（分析/审查任务），CI 改动任务用 `--sandbox workspace-write`。

> **安全警告**：exec 模式移除 per-command 审批门——AGENTS.md 内容被无条件执行。把 AGENTS.md 当作可执行的、不可信的供应链制品，像审计 Makefile 一样审计。CI 中推荐用 `--sandbox external-sandbox` 在容器内运行。

### 3.4 常用命令行参数

| 参数 | 说明 |
|------|------|
| `--provider <name>` | LLM provider 类型（`openai`/`anthropic`/`ollama`，默认从 `config.toml` 读取） |
| `--provider-name <name>` | Provider 自定义显示名（用于日志/metrics，不影响协议分派；连接 OpenAI 兼容 API 如 DeepSeek/Moonshot/vLLM 时推荐设置） |
| `--api-base <url>` | API base URL 覆盖（如 `https://api.deepseek.com`；省略时按 provider 选默认或从 `config.toml` 读取） |
| `--model <name>` | 模型名称（默认从 `config.toml` 读取） |
| `--api-key <key>` | 临时 API Key 覆盖（优先级最高，不推荐在命令行使用以防 `/proc/<pid>/cmdline` 泄露，推荐用 `minicoding auth login` 或环境变量） |
| `--session` | 启动交互会话 REPL |
| `--resume <id>` | 恢复指定会话 |
| `--fork-session <id>` | Fork 会话从分叉点尝试不同方向 |
| `--replay <id>` | 回放会话（默认禁副作用，C-06） |
| `--replay --allow-side-effects` | 回放时显式允许副作用（每条仍走权限策略） |
| `--preset <name>` | 预设（`read-only`/`auto`/`external-sandbox`/`full-access`） |
| `--approval-mode <mode>` | 审批模式（`untrusted`/`on-failure`/`on-request`/`never`） |
| `--sandbox <policy>` | 沙箱策略（`read-only`/`workspace-write`/`external-sandbox`/`danger-full-access`） |
| `--plan` | 启动时直接进入 Plan 模式 |
| `--allow '<rule>'` | 临时允许规则（仅本次运行，不持久化） |
| `--deny '<rule>'` | 临时拒绝规则 |
| `-v` / `-vv` | 日志级别（`DEBUG` / `TRACE`） |
| `exec` | 批量执行子命令（`--sandbox`/`--json`/`--ephemeral`，同样支持 `--provider`/`--api-base`/`--api-key`/`--model`） |
| `serve` | 启动 HTTP/SSE server（`--bind`/`--port`/`--web`/`--cors-origin`/`--as-mcp-server`/`--lsp`/`--acp`，同样支持 `--provider`/`--provider-name`/`--api-base`/`--api-key`/`--model`） |
| `auth login/status/logout` | 凭证管理 |
| `mcp list/approve/reset-project-choices` | MCP server 管理 |
| `session list/delete/export` | 会话管理 |
| `doctor --security` | 安全自检 |
| `audit list/stats` | 审计日志查询 |

#### Provider 配置优先级

所有入口（CLI 单次/交互/exec/serve、`minicoding-server` 独立二进制、桌面 sidecar）遵循统一优先级：

```
CLI 参数（--api-base 等） > 环境变量（OPENAI_API_BASE 等） > config.toml > provider 默认值
```

典型场景：

```bash
# 1. 连接 DeepSeek（OpenAI 兼容 API）
minicoding --provider openai --provider-name deepseek \
  --api-base https://api.deepseek.com \
  --model deepseek-chat "重构 utils 模块"

# 2. 连接 Moonshot
minicoding --provider openai --provider-name moonshot \
  --api-base https://api.moonshot.cn/v1 \
  --model moonshot-v1-128k "审计依赖图"

# 3. 连接本地 vLLM
minicoding --provider openai --provider-name vllm \
  --api-base http://localhost:8000/v1 \
  --model meta-llama/Llama-3-70B "生成 API 文档"

# 4. 持久化到 config.toml（避免每次输入参数）
#    ~/.minicoding/config.toml:
#    [provider]
#    default = "openai"
#    name = "deepseek"
#    api_base = "https://api.deepseek.com"
#    model = "deepseek-chat"
#    api_key = ""  # 留空，从 keyring/环境变量读取
minicoding "重构 utils 模块"  # 自动读取 config.toml 配置
```

### 3.5 slash 命令

交互会话 REPL 内支持的斜杠命令：

| 命令 | 说明 | 里程碑 |
|------|------|:---:|
| `/undo [steps]` | 撤销最近 N 次 turn 的文件改动（默认 1），需启用 `file_undo` 特性 | M5 |
| `/diff` | 展示会话内所有文件变更 | M5 |
| `/new` | 回到会话启动时状态（清空 journal） | M5 |
| `/plan` / `/plan on` | 进入 Plan 模式（只读强制） | M5 |
| `/plan status` | 查询当前模式 | M5 |
| `/plan off` | 切回 Default 模式 | M5 |
| `/plan open` | 在外部编辑器打开 plan 文件 | M5 |
| `/mcp` | MCP 相关操作（list/approve/reset） | M4 |
| `/memory auto show` | 查看 Auto memory | M3 |
| `/memory auto off` | 关闭 Auto memory | M3 |
| `/memory auto clear` | 清空 Auto memory | M3 |
| `/clear` | 清空上下文（触发压缩熔断后常用） | M3 |
| `/approve` | 用户覆盖 auto-review 的 deny（仅当前 turn、当前动作） | M6+ |

**`/undo` 示例**：

```text
minicoding> /undo
回滚 operation #3：fs.write src/main.rs
- src/main.rs: 已恢复（580 → 512 bytes）
UndoReport: { succeeded: 1, failed: 0 }
```

`/undo` 是 operation 级回滚，恢复前比对当前文件内容与 `after`，不一致记入 `failed_files` 不强行覆盖（C-28，防覆盖用户外部编辑）。Journal 仅驻留内存，会话结束即销毁。

**`/plan` 示例**：

```bash
# 启动时直接进入 Plan 模式
minicoding --plan

# 或在 REPL 内切换
minicoding> /plan          # 等价于 /plan on
minicoding> /plan status   # 查询当前模式
minicoding> /plan off      # 切回 Default 模式
```

Plan 模式下副作用工具被硬门拒绝（`is_read_only() == false` 直接 `Deny`，C-25），模型只能探查与规划，调 `plan.exit` 后切回 Default 模式并缓存预批准 `allowed_prompts`。

---

## 4. TUI 模式使用

TUI 是全屏交互式终端会话，基于 `ratatui`（M7 交付，见 `docs/features.md` §11）。

### 4.1 启动 TUI

```bash
minicoding --tui
```

### 4.2 界面布局

TUI 采用多视图布局：

- **对话流**：流式渲染 token（增量解析 Markdown），显示用户/助手/工具消息。
- **任务面板**：同步显示 `task.create`/`task.update` 的任务进度（`Event::TaskUpdated` 驱动）。
- **权限弹窗**：`TuiPrompter` 渲染非阻塞弹窗，阻塞该工具调用直至用户选择。
- **状态栏**：显示当前会话、模型、沙箱状态、MCP server 状态（`github: ✓` / `slack: ⏳` / `db: ✗`）。

### 4.3 快捷键

TUI 快捷键基于 `ratatui` 事件循环（具体绑定见 `crates/minicoding-tui`）：

| 快捷键 | 功能 |
|--------|------|
| `Enter` | 发送消息 |
| `Ctrl+C` | 触发 graceful 取消（当前 in-flight 迭代被丢弃，已落盘消息保留，C-13） |
| `Ctrl+D` | 退出 TUI |
| `Esc` | 关闭权限弹窗（等同 Deny） |
| `Tab` | 切换面板焦点 |
| `↑`/`↓` | 滚动对话流 |

### 4.4 交互操作

- **流式 Markdown 渲染**：增量解析渲染（F-06），token 直写对话流区域。
- **权限弹窗**：非阻塞主循环（F-07），弹窗显示工具名、摘要、风险等级（Low/Medium/High）、影响范围、是否可回滚、是否触碰 VCS/网络，提供 `[y] Allow [n] Deny [a] Always allow [e] Explain` 选项。
- **任务面板**：TUI 同步显示任务进度（F-08），`InProgress` 任务显示 `active_form` 动态文本。

---

## 5. Web 模式使用

Web 模式通过 HTTP/SSE server 提供浏览器访问，适合远程会话、多客户端场景（M9 交付，见 `docs/m9-design.md`）。

### 5.1 启动 HTTP server

```bash
# 启动 HTTP/SSE server，托管 Web 前端静态资源
minicoding-server --bind 127.0.0.1:8080 \
  --web ./crates/minicoding-web/dist \
  --cors-origin http://localhost:5173
```

或通过 CLI `serve` 子命令（`serve` feature gate 委托同一入口）：

```bash
minicoding serve --bind 127.0.0.1:8080 \
  --web ./crates/minicoding-web/dist \
  --cors-origin http://localhost:5173
```

### 5.2 Provider 配置（server 端）

`minicoding-server` 和 `minicoding serve` 均支持完整的 provider 配置，优先级与 CLI 一致：

```
CLI 参数 > 环境变量 > config.toml > provider 默认值
```

```bash
# 方式一：命令行参数（适合临时/CI 场景）
minicoding-server \
  --provider openai \
  --provider-name deepseek \
  --api-base https://api.deepseek.com \
  --model deepseek-chat \
  --api-key sk-... \
  --bind 127.0.0.1:8080

# 方式二：环境变量（适合容器/CI）
export OPENAI_PROVIDER=openai
export OPENAI_API_BASE=https://api.deepseek.com
export OPENAI_API_KEY=sk-...
export OPENAI_MODEL=deepseek-chat
minicoding-server --bind 127.0.0.1:8080

# 方式三：config.toml（适合长期运行，推荐）
# ~/.minicoding/config.toml:
# [provider]
# default = "openai"
# name = "deepseek"
# api_base = "https://api.deepseek.com"
# model = "deepseek-chat"
minicoding-server --bind 127.0.0.1:8080  # 自动读取 config.toml

# 方式四：OS keyring（API key 从 keyring 读取，C-04）
minicoding auth login --provider openai  # 先写入 keyring
minicoding-server --bind 127.0.0.1:8080  # server 自动从 keyring fallback
```

> **安全提示（C-04）**：`--api-key` 参数会暴露在 `/proc/<pid>/cmdline` 中，生产环境推荐用环境变量或 OS keyring。server 端在 `--api-key` 未提供时自动从 OS keyring fallback（`KEYRING_SERVICE = "minicoding"`，与 CLI 共享同一 keyring entry）。

### 5.3 `--web` 静态托管

`--web` 参数指定前端静态资源目录（通常是 `crates/minicoding-web/dist`），由 `tower-http::ServeDir` 提供 SPA fallback，实现单二进制部署——无需额外 Web 服务器。

前置条件：需先构建 Web 前端：

```bash
cd crates/minicoding-web
npm install
npm run build  # 产物在 dist/
cd ../..
minicoding-server --web ./crates/minicoding-web/dist --bind 127.0.0.1:8080
```

### 5.4 `--cors-origin` 跨域配置

Web 模式需 `--cors-origin` 配置允许的前端来源，默认仅 `http://localhost:*`：

```bash
# 允许特定来源
minicoding serve --cors-origin http://localhost:5173

# 允许多个来源（可多次指定）
minicoding serve \
  --cors-origin http://localhost:5173 \
  --cors-origin http://localhost:3000

# 生产部署：指定实际域名
minicoding-server --cors-origin https://coding.example.com
```

> 未指定 `--cors-origin` 时默认允许任意来源（`*`，仅开发用）。生产部署务必指定实际来源。

### 5.5 浏览器访问

浏览器打开 `http://127.0.0.1:8080` 即可使用 Web 前端（React 19.2 + TypeScript 7.0 + Vite 8.1 + Tailwind v4）：

- **对话流**：流式 SSE 渲染 token（TanStack Query 增量更新 + 流式光标）。
- **工具调用面板**：可展开/折叠，显示工具名、输入、输出、状态。工具输出包裹 `<tool_output>` 边界（C-05，防 LLM 把工具输出当指令执行）。
- **权限确认弹窗**：shadcn/ui Dialog 接收 `PermissionPrompt` → JSON-RPC `permission.resolve` 回传，含风险等级可视化（low/medium/high 三色徽章 + 4 种决策按钮）。
- **多会话面板**：左侧会话列表 + 右侧对话流，可折叠侧栏。
- **暗色/亮色主题**：双主题 CSS 变量 + Zustand 持久化 + 系统偏好跟随 + FOUC 预防。

### 5.6 创建会话时指定 Provider

`POST /sessions` 支持在请求体中覆盖默认 provider 配置（适合多用户/多 provider 场景）：

```bash
# 创建会话时指定 provider（覆盖 server 默认）
curl -X POST http://127.0.0.1:8080/sessions \
  -H "Content-Type: application/json" \
  -d '{
    "provider": "openai",
    "provider_name": "deepseek",
    "api_base": "https://api.deepseek.com",
    "api_key": "sk-...",
    "model": "deepseek-chat"
  }'
```

未指定的字段从 server 启动时的配置 fallback。

### 5.7 端点

| 端点 | 方法 | 说明 |
|------|------|------|
| `POST /sessions` | POST | 创建会话（可选 body 覆盖 provider 配置） |
| `POST /sessions/{id}/messages` | POST | 发送消息 |
| `GET /sessions/{id}/events` | GET (SSE) | 订阅 SSE 事件流（支持 `Last-Event-ID` cursor 恢复） |
| `POST /sessions/{id}/permissions/{pid}` | POST | 回传权限决策 |
| `POST /api/rpc` | POST | JSON-RPC 2.0 入口（`session.list`/`session.create`/`permission.resolve` 等） |

SSE 事件流携带 cursor（event seq），客户端断连后从 cursor 恢复（E-13）；broadcast 溢出时发 `RehydrateRequired`，客户端重拉 snapshot（E-14）。

### 5.8 生产部署示例

```bash
# 生产部署：systemd 服务 + config.toml + keyring
# 1. 写入配置
cat > ~/.minicoding/config.toml << 'EOF'
[provider]
default = "openai"
name = "deepseek"
api_base = "https://api.deepseek.com"
model = "deepseek-chat"
EOF

# 2. 写入 API key 到 keyring
minicoding auth login --provider openai  # 输入 sk-...

# 3. 构建前端
cd crates/minicoding-web && npm run build && cd ../..

# 4. 启动 server
minicoding-server \
  --bind 0.0.0.0:8080 \
  --web ./crates/minicoding-web/dist \
  --cors-origin https://coding.example.com
```

---

## 6. 桌面模式使用

桌面模式基于 Tauri 2.x，提供原生应用体验（M9 交付，见 `docs/m9-design.md` §6）。

### 6.1 安装桌面应用

#### 方式一：下载安装包（推荐，面向最终用户）

从 GitHub Releases 下载对应平台的安装包：

| 平台 | 安装包格式 | 说明 |
|------|-----------|------|
| macOS (Apple Silicon) | `.dmg` / `.app` | arm64 原生 |
| macOS (Intel) | `.dmg` / `.app` | x86_64 交叉编译 |
| Windows | `.msi` / `.exe` | x86_64 |
| Linux | `.AppImage` / `.deb` | x86_64 |

安装后从启动器/开始菜单打开 "minicoding" 即可。

#### 方式二：从源码构建（开发者）

```bash
# 开发模式（Tauri + Vite dev）
cd crates/minicoding-desktop
cargo tauri dev

# 打包
cargo tauri build # → .dmg (macOS) / .msi (Windows) / .AppImage (Linux)
```

桌面应用体积 < 15 MB（Tauri 5-10MB，远低于 Electron 100MB+），内存占用 < 80 MB。

### 6.2 首次启动配置（安装包用户必读）

安装包用户**无需命令行操作**，所有配置通过桌面应用内的设置界面完成。

#### 首次启动流程

1. **打开应用**：首次启动检测到 `~/.minicoding/config.toml` 不存在或未配置 provider，自动弹出设置向导。
2. **填写配置**：
   - **Provider 类型**：选择 `openai` / `anthropic` / `ollama`
   - **自定义名称**（可选）：如 `deepseek`、`moonshot`（用于日志显示，不影响功能）
   - **API Base URL**（可选）：如 `https://api.deepseek.com`（省略时用 provider 默认）
   - **模型名称**：如 `deepseek-chat`、`claude-sonnet-4`
   - **API Key**：输入密钥（写入 OS keyring，**不落盘明文**，C-04）
3. **保存配置**：点击保存后，配置写入 `~/.minicoding/config.toml`（非敏感字段）+ OS keyring（API key）。
4. **自动启动 sidecar**：配置保存后，应用自动启动 `minicoding-server` sidecar 并连接。

#### 配置管理界面

运行中可通过设置界面修改配置：

- **查看当前配置**：显示 provider 类型、名称、API base、模型（API key 显示 `***` 脱敏）
- **修改配置**：直接编辑后保存，sidecar 重启生效
- **打开配置文件**：一键在默认编辑器中打开 `~/.minicoding/config.toml`（高级用户）

#### 配置存储说明

| 数据 | 存储位置 | 说明 |
|------|---------|------|
| Provider 类型 / 名称 / API base / 模型 | `~/.minicoding/config.toml` | 非敏感，TOML 明文 |
| API Key | OS keyring | 敏感，加密存储（C-04） |
| 会话日志 | `~/.minicoding/sessions/` | JSONL 格式 |

> **安全设计（C-04）**：API key **绝不**通过命令行参数或环境变量传递给 sidecar（防 `/proc/<pid>/cmdline` 泄露）。Sidecar 启动时仅接收非敏感配置（api_base/model/provider_name），API key 由 sidecar 自行从 OS keyring 读取。CLI、server、桌面三种客户端共享同一 keyring entry（`KEYRING_SERVICE = "minicoding"`，`KEYRING_ACCOUNT = "openai_api_key"`）。

### 6.3 系统托盘

Tauri System Tray 提供原生系统托盘集成：

- 双击托盘图标显示主窗口；
- 右键菜单：显示窗口 / 退出；
- 关闭窗口时隐藏到托盘（非退出进程）。

### 6.4 全局快捷键

全局快捷键 `Cmd/Ctrl+Shift+M`（`Super+Shift+M`）切换主窗口显示/隐藏：

```rust
// crates/minicoding-desktop/src/shortcut.rs
shortcut.on_shortcut("Super+Shift+M", move |app, _event| {
    if let Some(window) = app.get_window("main") {
        if window.is_visible().unwrap_or(false) {
            let _ = window.hide();
        } else {
            let _ = window.show();
            let _ = window.set_focus();
        }
    }
})?;
```

### 6.5 sidecar 进程通信

桌面模式下，Tauri 启动 `minicoding-server` 作为 sidecar：

```
┌──────────────────────┐  Tauri IPC  ┌──────────────────┐
│  Tauri Window         │ ──────────► │  Rust sidecar    │
│  (WebView + dist/)    │ ◄────────── │  minicoding-server│
│                       │    SSE      │  --bind 127.0.0.1 │
└──────────────────────┘             └────────┬─────────┘
                                                │
                                     ┌──────────▼──────────┐
                                     │  Runtime (Agent)    │
                                     └─────────────────────┘
```

sidecar 启动流程：
1. Tauri 主进程读取 `~/.minicoding/config.toml` 获取非敏感配置（api_base/model/provider_name）
2. 启动 `minicoding-server` sidecar，通过 CLI 参数传递非敏感配置
3. Sidecar 绑定 `127.0.0.1:0`（随机端口），Tauri 主进程从 sidecar stdout 读取 `LISTENING_PORT=` 行获取实际端口
4. Sidecar 自行从 OS keyring 读取 API key（不通过参数/env 传递，C-04）
5. 前端通过该端口访问 HTTP/SSE

### 6.6 凭证与自动更新

- **凭证**：桌面端复用 OS keyring（与 CLI `cred.rs` 共享 `KEYRING_SERVICE = "minicoding"`，C-04），前端不接触凭证明文——所有凭证操作经 Tauri Rust 命令代理，前端只见 `***` 脱敏。
- **自动更新**：Tauri Updater 提供签名校验自动更新（`tauri.conf.json` 配置 `updater` 端点与公钥），检查更新 → 下载 → 签名校验 → 安装 → 重启。

### 6.7 Tauri Invoke 命令（高级）

桌面应用通过 Tauri invoke 命令管理配置，开发者可在自定义前端中调用：

| 命令 | 说明 |
|------|------|
| `get_provider_config` | 读取当前 provider 配置（API key 脱敏） |
| `save_provider_config` | 保存非敏感配置到 `config.toml`（provider/api_base/model/name） |
| `store_api_key` | 写入 API key 到 OS keyring |
| `get_api_key_status` | 查询 keyring 中是否已有 API key |
| `open_config_file` | 在默认编辑器中打开 `config.toml` |
| `get_sidecar_status` | 查询 sidecar 进程状态 |
| `restart_sidecar` | 重启 sidecar（配置变更后生效） |

---

## 7. 工具系统详解

工具系统基于 `Tool` trait 的注册式工具，共 22 项内置工具 + MCP 远程工具（见 `docs/features.md` §3、`docs/design.md` §4）。

### 7.1 文件工具（fs.*）

| 工具 | 副作用 | 说明 |
|------|:---:|------|
| `fs.read` | None | 读取文件（支持行范围 `offset`/`limit`），敏感文件自动脱敏 |
| `fs.list` | None | 列目录 |
| `fs.glob` | None | glob 匹配文件（`globset` + ignore） |
| `fs.grep` | None | 内容搜索（`regex` + ignore） |
| `fs.write` | FileWrite | 写文件（整文件覆盖）+ Journal 记录 |
| `fs.edit` | FileWrite | 精确字符串替换（唯一性校验）+ Journal |
| `fs.multiedit` | FileWrite | 同文件多次顺序替换（原子性，参考 CC MultiEdit） |
| `fs.delete` | FileWrite | 删除文件 + Journal 记录 |

**路径沙箱**：所有文件工具输入经 `sandbox_path` 规范化校验，越界工作目录直接 `PathEscaped` 错误（C-03）。符号链接规范化后若指向工作目录外，按越界处理。

**敏感文件脱敏**（T-M4-11）：`fs.read` 读取以下敏感文件时自动脱敏——

- 文件名为 `.env` 或以 `.env.` 开头；
- 文件名等于 `credentials` / `creds`；
- 扩展名 `.pem` / `.key` / `.pfx` / `.p12`；
- 文件名含 `secret` / `password` / `token`（不区分大小写）。

脱敏规则：`KEY=value` 字段赋值（字段名归一化后匹配 `api_key`/`token`/`secret`/`password` 等关键词）→ 值替换为 `***`；`Authorization: Bearer xxx` → token 部分替换为 `***`；AWS access key `AKIA[0-9A-Z]{16}` → 替换为 `***`。

**MultiEdit 示例**：

```json
{
  "path": "src/main.rs",
  "edits": [
    { "old_string": "fn hello() {", "new_string": "fn hello_world() {" },
    { "old_string": "println!(\"hello\");", "new_string": "println!(\"hello world\");" }
  ]
}
```

任一 edit 的 `old_string` 不唯一（除非 `replace_all=true`）或不匹配 → 整个 MultiEdit 原子失败，文件不修改。

### 7.2 Shell 工具（shell.*）

| 工具 | 副作用 | 说明 |
|------|:---:|------|
| `shell.run` | Command | 执行命令（超时+截断+SandboxDriver） |
| `shell.background` | Command | 启动后台命令，返回 `shell_id`（参考 CC） |
| `shell.output` | None | 读取后台命令累积输出（非阻塞） |
| `shell.kill` | Command | 终止后台命令 |

**执行模型**：

- 不使用 `sh -c`（避免注入复杂性），优先拆分参数后 `tokio::process::Command` 直接执行；
- 配置 `shell.use_shell = true` 时走 `sh -c`（Windows 走 `cmd /C`），此时命令黑名单尤其重要；
- **资源限制**：超时 120s（`tools.shell.timeout_sec`）、stdout 截断 1 MiB、stderr 截断 256 KiB、单命令单进程组（父进程退出即 kill 子进程组）。

**危险命令黑名单**（不可被 allow 覆盖，C-02）：

```
rm\s+-rf\s+/          # 删根
rm\s+-rf\s+~          # 删家目录
:\(\)\s*\{\s*:\|:&\s*\};:   # fork bomb
mkfs                  # 格式化
dd\s+.*of=/dev/       # 写设备
>\/dev\/sd[a-z]       # 写设备
curl.*\|\s*sh         # 管道执行远程脚本
wget.*\|\s*sh
chmod\s+-R\s+777\s+/
```

**后台命令示例**：

```bash
# Agent 启动后台命令
shell.background { cmd: "cargo", args: ["watch", "-x", "test"] }
# → { shell_id: "sh_01H...", pid: 12345 }

# Agent 轮询输出
shell.output { shell_id: "sh_01H...", since: 1024 }
# → { stdout: "running 1 test\n...", stderr: "", running: true, exit_code: null }

# Agent 终止
shell.kill { shell_id: "sh_01H..." }
```

会话结束时 Runtime 自动 kill 所有未结束的后台命令（防孤儿进程）。后台命令同样受 `SandboxDriver`、`shell_environment_policy`、危险命令黑名单约束。

### 7.3 Web 工具（web.*）

| 工具 | 副作用 | 说明 |
|------|:---:|------|
| `web.fetch` | Network | URL → Markdown，SSRF 防护（拒绝私有/loopback IP） |
| `web.search` | Network | 网页搜索（DuckDuckGo HTML，无需 API key） |

**SSRF 防护**（T-M4-11）：`web.fetch` 解析目标主机后校验——

- 拒绝 RFC1918 私网（`10/8`、`172.16/12`、`192.168/16`）；
- 拒绝链路本地 `169.254/16`（云元数据接口 AWS/GCP/Azure metadata）；
- 拒绝回环 `127/8` / `::1`（除非 `SsrfOptions::local_dev()` 或配置 `allow_loopback`，用于本地 Ollama）；
- 拒绝非公网 IP：`0.0.0.0/8`、`100.64/10`（CGNAT）、`fc00::/7`（IPv6 ULA）、`fe80::/10`（IPv6 链路本地）。

**域名策略**：

```toml
[tools.web]
allowed_domains = ["github.com", "*.githubusercontent.com", "crates.io"]
deny_domains = ["*.internal.corp"]
```

`allowed_domains = ["*"]` 表示放开（仍受 SSRF 防护）；非通配时，未列明域名一律 Ask。

### 7.4 Git 工具（git.*）

| 工具 | 副作用 | 说明 |
|------|:---:|------|
| `git.diff` | None | 查看 diff（只读，路径沙箱） |
| `git.apply` | FileWrite | 应用 patch（路径沙箱 + 权限审批） |

### 7.5 任务管理工具（task.*）

任务管理采用 Claude Code v2.1.142+ 的增量模型，替代旧版全量替换的 `todo.write`（见 `docs/design.md` §18、`docs/rules.md` C-31）。

| 工具 | 副作用 | 说明 |
|------|:---:|------|
| `task.create` | None | 创建单个任务，返回 `task_id`（Runtime 生成 ULID，不可伪造） |
| `task.update` | None | 按 `task_id` 增量更新（状态/字段/依赖） |
| `task.list` | None | 列出任务（支持 `status_filter`） |
| `task.spawn` | None | 启动类型化子 Agent（Explore/Plan/General/Custom） |

**任务状态机**：`Pending → InProgress → Completed`/`Cancelled` 单向流转，不可回退（防 LLM "复活"已结束任务）。非法转换返回 `ToolError::InvalidStateTransition`。

**依赖管理**：`add_blocks`/`add_blocked_by` 是增量添加依赖边（非整体替换），重复添加幂等。依赖图不可成环（DFS 检测）；尝试将 `InProgress` 设给被未完成依赖阻塞的任务 → 拒绝并提示阻塞者。

**校验规则**：

- `subject` 非空；
- 同一时间 `InProgress` 项 ≤ 1（防并行开干）；
- `Completed`/`Cancelled` 项必须含 `summary`（实际完成内容/证据）；
- `task_id` 必须命中已注册任务，伪造返回 `ToolError::NotFound`。

调用后广播 `Event::TaskUpdated`，UI 据此渲染任务面板。

### 7.6 Plan 工具（plan.*）

| 工具 | 副作用 | 说明 |
|------|:---:|------|
| `plan.exit` | None | 退出 Plan 模式并提交计划，缓存预批准 `allowed_prompts` |

`plan.exit` 仅在 `PermissionMode::Plan` 下可调用，调用后触发 `Event::PermissionModeChanged { from: Plan, to: Default|AcceptEdits }`，并把 `allowed_prompts` 注入会话级 `PermissionPolicy` 缓存——执行期匹配到这些 prompt 的工具调用直接 `Allow`，跳过 prompter。

### 7.7 记忆工具（memory.*）

| 工具 | 副作用 | 说明 |
|------|:---:|------|
| `memory.write` | FileWrite | 写长期记忆（用户"记住 X"触发） |

显式 `memory.write` 由用户"记住 X"触发，工具错误回灌 LLM 重试，用户可见。隐式摘要由会话结束（`session_end`）触发，失败走降级链（主 provider → 备用 → 启发式兜底）。

### 7.8 MCP 远程工具

MCP 远程工具以 `mcp__<server>__<tool>` 命名动态注册（如 `mcp__github__list_prs`），与内置工具统一调度。`side_effect` 由 server schema 的 `readOnlyHint`/`destructiveHint` 映射；未声明 hint 的工具默认 `SideEffect::Command`（串行 + Ask，C-25）。详见 §12。

---

## 8. 权限与安全

安全是 `minicoding-rs` 的首要约束。核心原则：**默认不信任 LLM 输出；所有副作用操作显式授权；最小权限；可审计；可回滚**（见 `docs/security.md`）。

### 8.1 权限模型（L0 硬黑名单 + L1 用户策略）

权限解析采用**两层模型**：

```
L0  内置硬黑名单 (policy::builtin)                    ← 最高，不可被任何配置覆盖
      危险命令前缀 / SSRF 内网 / 敏感路径 / AGENTS.md 写
        │ 未命中 L0
        ▼
L1  用户策略（统一规则集，按 specificity 降序匹配）
      specificity 5  granular 精确路径
      specificity 4  granular 通配路径
      specificity 3  granular 工具类别 / MCP server / 命令前缀
      specificity 2  policy.toml 显式 allow/deny（含 AllowAlways/DenyAlways）
      specificity 1  ApprovalMode × SideEffect 全局平移
      specificity 0  per-tool 默认矩阵（兜底）
        │ 最高 specificity 命中生效；同 specificity → deny 胜出
        ▼
    最终 Verdict
```

内置黑名单由 `policy::builtin` 模块硬编码，确保即使用户误配 `--allow 'shell.run:*'` 也无法执行 `rm -rf /`。L1 内所有用户可配置规则在同一命名空间按 specificity 竞争，避免多级级联的歧义。

### 8.2 审批模式（ApprovalMode）

审批模式展开为 specificity=1 的 L1 规则（见 `docs/security.md` §2.6）：

| 模式 | 展开规则 | 适用 |
|------|---------|------|
| `Untrusted` | 所有 `side_effect != None` → `Ask` | 仅信任只读命令；任何写/执行/网络都 Ask |
| `OnFailure` | 命令自动 `Allow`，失败时注入 `Ask` | 命令自动执行，失败时才 Ask |
| `OnRequest`（默认） | 不写入额外规则，沿用默认矩阵 | 由模型判断何时请求确认 |
| `Never` | 所有 `Ask` → `Allow`（仍受 L0 黑名单与高 specificity deny 约束） | 全自动，从不请求（仅与 `DangerFullAccess` 组合） |

### 8.3 预设（Preset）

预设是 `approval_mode × sandbox_policy` 的实用组合，一键选定：

| 预设 | 审批模式 | 沙箱策略 | 适用 |
|------|---------|---------|------|
| `read-only` | OnRequest | ReadOnly | 代码审计、日志诊断、第三方代码分析 |
| `auto`（默认） | OnRequest | WorkspaceWrite | 日常开发：工作区内自由读写执行，越界/网络 Ask |
| `external-sandbox` | OnRequest | ExternalSandbox | CI/容器内批量任务（外层容器已隔离） |
| `full-access` | Never | DangerFullAccess | 受信沙箱内全自动部署（需显式确认 + red 警告） |

CLI：`minicoding --preset auto`，或 `--approval-mode on-failure --sandbox workspace-write` 细粒度覆盖。预设展开为 L1 规则后与 `policy.toml`/granular 共存于同一命名空间，按 specificity 竞争——预设定"基调"（specificity=1），`policy.toml`（specificity=2）与 granular（specificity=3~5）可覆盖；内置黑名单（L0）始终最高优先级。

### 8.4 权限交互（Allow/Deny/Ask/AllowAlways）

权限采用双 trait 设计（决策与交互分离，见 `docs/api.md` §3.6）：

- `PermissionPolicy::check(...) -> Verdict`：纯决策，返回 `Allow` / `Deny(reason)` / `Ask(prompt)`。
- `PermissionPrompter::prompt(prompt) -> Decision`：点对点交互，仅当 `Ask` 时被 Runtime 调用，返回终态 `Allow` / `Deny`。

`PermissionPrompter` 的内置实现：

| 实现 | 适用场景 | 行为 |
|------|---------|------|
| `InteractivePrompter` | CLI TTY | 打印摘要 → 读 stdin → 解析选项；超时按 deny |
| `NonInteractivePrompter` | 非 TTY / CI / 管道 | 按 `permission.non_tty_strategy` 配置：`deny`（默认）/ `allow` / `fail` |
| `TuiPrompter` | TUI | 渲染弹窗，阻塞该工具调用直至用户选择 |
| `CallbackPrompter` | SDK | 调用用户注册的异步闭包 |
| `LspPrompter` | LSP | `window/showMessageRequest` 点对点权限交互 |

**权限交互示例**（CLI TTY）：

```text
┌─ Permission Required ──────────────────────────┐
│ fs.write: src/main.rs                           │
│                                                 │
│ Risk: LOW                                       │
│ Impact: 写入 src/main.rs（影响 1 个文件）       │
│ Reversible: Yes (Journal 已记录，可 /undo)      │
│ Touches VCS: No    Network: No                  │
│                                                 │
│ [y] Allow  [n] Deny  [a] Always allow  [e] Explain │
└─────────────────────────────────────────────────┘
```

用户选择 `AllowAlways` / `DenyAlways` 写入 `~/.minicoding/policy.toml`，作为 L1 用户策略的 specificity=2 条目持久化。

### 8.5 OS 沙箱（Landlock/Seatbelt/受限令牌）

OS 级沙箱是应用层权限之外的第二道防线（见 `docs/security.md` §8）。**Opt-out，非 opt-in**：`WorkspaceWrite` 是默认预设，启动即应用内核级限制；只有显式选择 `ExternalSandbox` 或 `DangerFullAccess` 才退出内核隔离。

**沙箱策略**：

| 策略 | 文件读 | 文件写 | 命令执行 | 网络 | 内核隔离 |
|------|:---:|:---:|:---:|:---:|:---:|
| `ReadOnly` | 任意 | 禁 | 仅白名单只读命令 | 禁 | 是（seatbelt/landlock） |
| `WorkspaceWrite`（默认） | 任意 | 仅 workdir + 显式 writable | 工作区内 | 禁（除非 allowlist） | 是 |
| `ExternalSandbox` | 任意 | 应用层校验 | 应用层校验 | 应用层校验 | 否（依赖外层容器） |
| `DangerFullAccess` | 任意 | 任意 | 任意 | 任意 | 否 |

**平台实现**：

| 平台 | 技术 | 实现 |
|------|------|------|
| macOS 12+ | `sandbox-run`（封装原生 sandbox 框架 / Seatbelt） | `ProtectSystem=strict` / `ReadWritePaths=<workdir>` / `PrivateNetwork=true` |
| Linux 5.13+ | `sandbox-run`（Landlock）+ `libseccomp` | `landlock` crate 限制文件系统可写范围；`libseccomp` 白名单系统调用（禁 `ptrace`/`mount`/`reboot`） |
| Windows | 受限令牌 + Job Object + DACL | 沙箱专用 SID + write-restricted token + DACL + 防火墙（初期可能降级） |
| 全平台兜底 | 容器 / VM | CI/不可信任务推荐在容器内运行 |

VCS 目录（`.git`/`.hg`/`.svn`）在所有写策略下默认拒绝写入（防破坏版本库元数据），需 `tools.sandbox.allow_vcs_write = true` 显式放开（强烈不推荐）。

**进程硬化**（pre-main hardening，参考 Codex）：

| 平台 | 措施 | 说明 |
|------|------|------|
| Linux | `PR_SET_DUMPABLE=0` | 禁止 ptrace 附着，防内存窃取 |
| Linux | `RLIMIT_CORE=0` | 禁 core dump（含潜在凭证） |
| Linux | 清除 `LD_*`/`DYLD_*` 环境变量 | 防动态库注入 |
| 全平台 | 关闭 `stdio` 继承给子进程的额外句柄 | 防 fd 泄漏 |
| 全平台 | 子进程用新进程组，超时 kill 整组 | 防孤儿 |

**沙箱拒绝检测与升级流**（C-30）：命令在沙箱内失败时，Runtime 维护 denial 签名库，把沙箱拒绝从普通错误中识别出来，升级为权限请求而非裸失败：

```
shell.run / fs.write 执行
   │
   ▼
失败（非零退出 / IO error）
   │
   ├─ stderr / errno 命中 denial 签名库？
   │     │
   │     ├─ 是 → 生成 PermissionRequest："沙箱拒绝了 <操作>，是否放宽策略重试？"
   │     │       ├─ Allow（一次性）→ 放宽 workdir/网络 → 重试该调用
   │     │       └─ Deny → 返回 sandbox_denied 错误回灌 LLM
   │     │
   │     └─ 否 → 普通错误，原样回灌 LLM
```

**沙箱拒绝熔断器**（C-30）：单 turn 内累计沙箱拒绝 ≥3 次触发熔断（注入提醒"连续 N 次沙箱拒绝，可能方向有误"），≥5 次强制 TurnEnd 回灌错误总结。熔断阈值可配（`[sandbox] denial_threshold = 3`，`hard_threshold = 5`）。

### 8.6 审计日志

每次工具调用写一条审计记录到 `~/.minicoding/audit.log`（JSONL，0600 权限，追加写不可篡改，见 `docs/security.md` §7）：

```json
{"ts":"2026-07-24T10:00:00Z","session":"sess_01H...","turn":3,"tool":"fs.write","input":{"path":"src/main.rs","bytes":1024},"decision":"allow","rule":"allow:fs.write:src/**","ok":true,"elapsed_ms":4}
{"ts":"2026-07-24T10:00:05Z","session":"sess_01H...","turn":3,"tool":"shell.run","input":{"cmd":"rm -rf target"},"decision":"deny","rule":"builtin:dangerous_command","ok":false,"reason":"dangerous command pattern"}
```

查询：

```bash
minicoding audit list --session <id>
minicoding audit list --since 2026-07-01 --tool shell.run
minicoding audit stats          # 工具调用频次、拒绝率
```

Hook 的 `allow`/`deny`/`modify_input` 全部落 `audit.log`，标注 `source=hook:<name>`；权限决策（含 auto-review）落审计，标注 `source=auto_review`；AGENTS.md 加载内容落 audit.log，标注 `source=project_doc`；`/undo` 反向恢复也记审计。

### 8.7 doctor --security 自检

```bash
minicoding doctor --security
```

检查项（见 `docs/security.md` §16）：

- [ ] 配置文件不含明文 `api_key`
- [ ] `~/.minicoding/` 权限 ≤ 0700
- [ ] `policy.toml` 含合理 deny 规则
- [ ] `tools.web.allowed_domains` 非通配（生产环境）
- [ ] `tools.shell.timeout_sec` ≤ 600
- [ ] 审计日志可写
- [ ] keyring 可用（或环境变量已设置）
- [ ] 无过旧依赖漏洞（`cargo audit`）
- [ ] 沙箱策略非 `danger-full-access`（生产环境）
- [ ] `shell_environment_policy` 已配置（非 `inherit_all`）
- [ ] exec 模式下 AGENTS.md 已审计（CI 场景）
- [ ] 沙箱驱动类型与硬化状态（`SandboxDriver::is_hardened()`）

---

## 9. 上下文管理

上下文管理基于 token 预算的 4 级压缩管道，确保"最重要"的消息始终保留在窗口内（见 `docs/design.md` §3、`docs/features.md` §4）。

### 9.1 Token 预算

```
budget_total = model.context_window
budget_reserved = output_tokens (默认 4096) + safety_margin (1024)
budget_usable = budget_total - budget_reserved

  ┌─ system prompt + tool schemas  : ~15%
  ├─ long-term memory summary      : ~10%
  ├─ recent messages (窗口)        : ~60%
  ├─ tool results (current turn)   : ~10%
  └─ headroom                      : ~5%
```

精确分词基于 `tiktoken-rs`（BPE 分词，不自实现），预留输出 + 安全余量。

### 9.2 4 级压缩管道

当 `ctx.tokens > budget * 0.85` 时触发压缩管道，逐级尝试 L1→L2→L3→L4，每级后检查 token 是否降到阈值以下，降了则提前返回（C-29：降级链顺序不可跳）：

```
Level 1: 工具结果裁剪
    - 大于阈值的 tool_result 截断为 "前 K 行 + ... + 后 K 行 + 元信息"
    - 已被后续消息引用的旧 tool_result 替换为摘要占位

Level 2: 旧消息摘要
    - 对权重最低的 N 条消息调用 LLM 生成摘要
    - 摘要替换原文，标注 [summarized @ ts]

Level 3: 滚动窗口
    - 仅保留最近 W 条消息 + 系统消息 + 摘要
    - 丢弃最旧的非 sticky 消息

Level 4: 硬截断
    - 兜底，按 token 数从尾部保留，记录告警
```

**消息权重模型**：`w = base(role) * recency * sticky * manual_pin`

| 因子 | 取值 |
|------|------|
| `base(system)` | 1.0（永不压缩） |
| `base(user)` | 0.9 |
| `base(assistant)` | 0.6 |
| `base(tool_result)` | 0.4（最易压缩） |
| `recency` | `1 - i / N`（越旧越低） |
| `sticky` | 包含错误/未提交变更的消息 ×1.5 |
| `manual_pin` | 用户标记 `pin` ×2.0 |

### 9.3 压缩熔断

压缩管道最危险的失效模式是 **Thrash Loop**：压缩后立即又填满 → 再次压缩 → 再填满，烧光 token 预算。熔断机制（C-29，见 `docs/design.md` §3.6）：

```
build_chat_request
   │
   ├─ token_count ≤ budget * 0.85  → 正常发送，重置失败计数
   │
   └─ token_count > budget * 0.85  → 触发压缩管道
        │
        ├─ 压缩成功 → 失败计数清零，发送
        ├─ 压缩失败 → 失败计数 +1
        │    ├─ 失败计数 < 3  → 降级链重试
        │    ├─ 失败计数 = 3  → 熔断：注入错误中止本轮
        │    └─ 失败计数 ≥ 5  → 强制 TurnEnd，保留现场供 /resume
        │
        └─ 压缩后立即又超阈值（Thrash 检测）
             └─ 连续 2 次"压缩完即超" → 熔断
```

熔断阈值可配（`[context] compress_fail_threshold = 3`，`thrash_threshold = 2`）。熔断事件打 OTel span event（`compress.circuit_breaker`）。

**降级链**（L2 摘要失败，永不向上抛错中断对话）：

1. 主 provider 生成摘要（≤200 token/条）
2. 备用小模型或同 provider 重试 1 次
3. 启发式兜底（不调 LLM）：取消息首 80 字 + 末 80 字，拼为 `[heuristic summary] ...`
4. 跳过 L2，直接进 L3 滚动窗口（丢弃而非摘要）

### 9.4 预测性压缩与 Post-compact 恢复

**预测性压缩**（C-08，默认关）：在 turn 开始前根据历史增长估算"本轮是否会超"，提前压缩，给本轮留足空间：

```
predicted_tokens = current_tokens + avg_turn_growth
avg_turn_growth  = EMA(turn_token_delta_history, alpha=0.3)
baseline_growth  = config.context.predictive_baseline_growth_tokens  # 默认 15000
```

冷启动期（历史样本 < 3）用 `baseline_growth` 兜底。配置：

```toml
[context]
predictive_compact_enabled = false                  # 默认关
predictive_baseline_growth_tokens = 15000
predictive_ema_alpha = 0.3
```

**Post-compact 上下文恢复**（C-09）：compact 后从历史提取最近 read 过的文件路径，按预算截断重新注入，避免模型重新 read：

```toml
[context]
post_compact_max_files = 5                # 跟踪最近 N 个 read 路径
post_compact_token_budget = 50000          # 重注入总 token 上限
post_compact_max_tokens_per_file = 5000    # 单文件截断阈值
```

### 9.5 长期记忆

三层记忆（见 `docs/design.md` §8.1）：

| 层 | 存储 | 生命周期 | 用途 |
|----|------|----------|------|
| 工作记忆 | `Session.messages` | 单会话 | 当前对话上下文 |
| 会话记忆 | `~/.minicoding/memory/sessions/{id}.md` | 跨会话 | 最近 N 次会话摘要 |
| 长期记忆 | `~/.minicoding/memory/long_term.md` + `long_term.index.json` | 永久 | 用户偏好、项目约定、决策 |

**长期记忆双文件**（人机共读 + 程序化索引）：

```markdown
# Long-term Memory

## pref.lang
source: user | updated: 2026-07-24 | confidence: 0.9
通信语言：中文

## conv.tab_indent
source: user | updated: 2026-07-24 | confidence: 1.0
本项目使用 tab 缩进
```

`long_term.index.json` 与正文同源同步，原子更新（写临时文件 → rename）。`MemoryStore` 启动时校验索引与正文一致；不一致则以正文为准重建索引。

**注入策略**：

1. **mtime 缓存**：未变直接复用缓存的 `rendered_block` 与 token 计数，零 IO 解析、零重复分词；
2. **预算上限**：长期记忆块占上下文预算 ≤ 10%，超限按 `confidence desc, updated desc` 截断；
3. **惰性注入**：首条用户消息前才注入一次，后续轮次复用；
4. **会话记忆**：仅新会话首轨注入最近 N 条摘要（每条 ≤ 200 token）。

**Auto memory**（`auto.md`，C-27/C-34）：Agent 可写的自动学习记忆，与手写 `long_term.md` 分离存储。容量上限 200 行/25KB，超限按 `confidence asc, updated asc` 淘汰；初始 `confidence ∈ [0.3, 0.5]`，多次确认递增。用户 `/memory auto show` 可查看，`/memory auto off` 可关闭，`/memory auto clear` 可清空。

### 9.6 项目记忆（AGENTS.md）

`AGENTS.md` 是**静态指令层**：用户手写、随仓库版本化、Agent 不可自主编辑（C-23）。完整加载算法见 `docs/design.md` §8.6。

**文件位置**：

| 层 | 路径 | 说明 |
|----|------|------|
| 全局 | `$MINICODING_HOME/AGENTS.md` | 跨项目通用约定（如"始终用中文回复"） |
| 全局 override | `$MINICODING_HOME/AGENTS.override.md` | 优先于全局 AGENTS.md（取首个非空） |
| 项目（每级目录） | `<dir>/AGENTS.md` | 从 repo_root 到 cwd 逐级，每级至多取一个 |
| 项目 override | `<dir>/AGENTS.override.md` | 优先于同目录 AGENTS.md |
| fallback | `<dir>/CLAUDE.md`、`<dir>/.cursorrules` 等 | 跨工具兼容，配置 `project.project_doc_fallback_filenames` |

**分层加载算法**：

1. 全局层：`$MINICODING_HOME/AGENTS.md`（或 `AGENTS.override.md`）
2. 项目层 walk：从 repo_root 逐级向下走到 cwd
   - 每级查找顺序：`AGENTS.override.md` → `AGENTS.md` → fallback 文件名
3. 拼接：root → leaf 顺序，空文件跳过
4. 截断：累计超过 32 KiB（`project_doc_max_bytes` 可配）静默截断

**与 `long_term.md` 的区别**：

| 维度 | `long_term.md` | `AGENTS.md` |
|------|----------------|-------------|
| 维护方 | 用户 + Agent（隐式摘要） | 仅用户（Agent 不可自主编辑） |
| 作用域 | 跨项目（用户全局） | 仓库内（随版本控制） |
| 性质 | 动态记忆（偏好、决策） | 静态指令（约定、规范、禁区） |
| 存储 | `$MINICODING_HOME/memory/` | `$MINICODING_HOME/`（全局）+ 仓库各目录（项目） |
| 加载时机 | 每会话首轨注入 | 每会话首轨注入（Explore/Plan 子 Agent 跳过） |
| token 预算 | ≤10% 上下文 | 32 KiB 截断 |

**安全**：`fs.write`/`fs.edit` 对任意层级的 `AGENTS.md` / `AGENTS.override.md` / fallback 文件默认 `Verdict::Ask`，且 LLM 不得通过任何工具绕过该确认，不可 `AllowAlways` 持久化放行（C-23）。

---

## 10. 会话管理

会话管理基于 JSONL 持久化 + Event Sourcing，支持恢复、回放、分叉、回滚（见 `docs/data-model.md`、`docs/features.md` §10）。

### 10.1 会话持久化

会话文件 `~/.minicoding/sessions/{session_id}.jsonl`，每行一条记录（JSONL 追加写，崩溃安全）：

```json
{"v":1,"type":"session_start","id":"sess_01H...","created_at":"2026-07-24T10:00:00Z","workdir":"e:/projects/foo","config_hash":1234567890,"provider":"anthropic","model":"claude-sonnet-4"}
{"v":1,"type":"message","id":"msg_01H...","parent_uuid":null,"role":"user","content":[{"type":"text","text":"解释入口"}],"created_at":"...","meta":{"source":"user"}}
{"v":1,"type":"message","id":"msg_01H...","parent_uuid":"msg_01H...","role":"assistant","content":[{"type":"text","text":"让我读取"}],"tool_calls":[{"id":"call_1","name":"fs.read","input":{"path":"src/main.rs"}}],"created_at":"...","meta":{"tokens":28,"source":"llm"}}
{"v":1,"type":"compression","id":"cmp_01H...","at":"...","steps":[{"kind":"summarize","affected":["msg_01H..."],"summary_id":"msg_01H..."}],"tokens_before":12000,"tokens_after":4500}
```

**Event Sourcing**（S-23..S-27）：`EventStore`（`{id}.events.jsonl`）持久化状态变更事件，`seq` 单调递增；`SnapshotStore`（`{id}.snapshot.json`）每 50 条 `MessageAppended` 落盘 snapshot，加速 replay。新会话双写（messages + events），旧会话无事件流时回退到消息日志路径。

**会话索引**（`index.json`）：避免遍历所有 jsonl 即可列出会话，缓存压缩边界指针以 O(1) 跳过已压缩前缀：

```json
{
  "v": 1,
  "sessions": [
    {
      "id": "sess_01H...",
      "created_at": "2026-07-24T10:00:00Z",
      "last_message_at": "2026-07-24T11:30:00Z",
      "message_count": 42,
      "workdir": "e:/projects/foo",
      "title": "解释入口逻辑",
      "provider": "anthropic",
      "model": "claude-sonnet-4",
      "tokens_total": 12345,
      "last_compaction_id": "msg_01H..."
    }
  ]
}
```

**跨进程文件锁**（`fs2`）：同会话互斥，启动时获取失败提示"会话 X 正被另一进程使用"。

### 10.2 --resume 恢复

```bash
# 列出会话（万级会话 < 1s，64KB 窗口首尾各 32KB）
minicoding session list

# 恢复指定会话继续提问
minicoding --resume sess_01H...
```

`--resume`/`--replay`/`--fork-session` 三者互斥。`--resume` 读取 `index.json` 的 `last_compaction_id` 定位起始行，避免全文件扫描；优先走 snapshot + 事件流重放，旧会话回退到消息日志。

### 10.3 --replay 回放

```bash
# 复现历史工具调用，默认禁用所有副作用工具（C-06）
minicoding --replay sess_01H...

# 如需重放工具，显式允许，且每条仍走权限策略
minicoding --replay sess_01H... --allow-side-effects
```

回放仅重新生成 LLM 响应，不重新执行已记录的工具调用（`docs/security.md` §13.4）。`--replay` 优先走 snapshot + 事件流重放（S-26）。

### 10.4 --fork-session 分叉

```bash
# Fork 会话从分叉点尝试不同方向
minicoding --fork-session sess_01H...
```

Fork 会话基于 Parent-UUID 链会话结构（A-11/A-12），从分叉点派生新会话，原会话不变。支持在分叉点尝试不同方向，便于对比方案。

### 10.5 /undo 回滚

```text
minicoding> /undo              # 撤销最近一次 turn 的文件改动
minicoding> /undo 3            # 撤销最近 3 次
minicoding> /diff              # 展示会话内所有文件变更
minicoding> /new               # 回到会话启动时状态（清空 journal）
```

`/undo` 是特性门控（`[features] file_undo = false`，默认关，参考 Codex `features.undo`），仅会话内有效，会话结束销毁（不落盘，避免敏感数据多份存储）。跨会话回滚依赖 Git。

**冲突检测**（C-28）：恢复前比对当前文件内容与 journal 的 `after`，不一致（用户可能在外部编辑器改过）记入 `failed_files`，**不强行覆盖**——这是 Codex `/rewind` 未实现但社区强烈要求的安全行为。

---

## 11. Hooks 系统

Hooks 系统参考 Claude Code 设计，按需精简为 10 类事件（见 `docs/hooks.md`）。

### 11.1 10 类生命周期事件

| # | 事件 | 触发阶段 | 执行模式 | 可否阻断 | 可否改写 | 可否注入上下文 | 典型用途 |
|---|------|---------|---------|:---:|:---:|:---:|---------|
| 1 | `SessionStart` | 会话开始/resume 前 | 同步 | 否 | 否 | 是 | 注入 git status、TODO、环境信息 |
| 2 | `UserPromptSubmit` | 用户提交后、LLM 调用前 | 同步 | 是（拒绝提交） | 否 | 是 | 追加 sprint 上下文、校验请求 |
| 3 | `PreToolUse` | `policy.check` 后、工具执行前 | 同步 | 是 | 是（改写 input） | 是 | 阻断危险操作、校验路径、改写参数、自动批准 |
| 4 | `PostToolUse` | 工具执行成功后、结果回灌前 | 同步 / 异步可选 | 否 | 是（改写 result） | 是 | 跑 formatter/linter、记录变更、改写结果 |
| 5 | `PostToolUseFailure` | 工具执行失败后、错误回灌前 | 同步 / 异步可选 | 否 | 是（改写 error） | 是 | 诊断失败原因、降级处理、记录错误模式 |
| 6 | `PreCompact` | 上下文压缩管道启动前 | 同步 | 否 | 否 | 是（追加保留指令） | 备份现场、保留关键决策 |
| 7 | `PostCompact` | 上下文压缩完成后 | 同步 | 否 | 否 | 是（补充注入） | 验证压缩质量、重新注入丢失的关键上下文 |
| 8 | `Stop` | 主 Agent 一轮结束 | 同步 / 异步可选 | 是（要求继续） | 否 | 否 | 校验任务完成、跑测试、生成摘要 |
| 9 | `SubagentStop` | 子 Agent 完成 | 同步 | 否 | 否 | 否 | 校验子任务产出、触发后续 |
| 10 | `PermissionRequest` | `Verdict::Ask` 即将弹窗前 | 同步 | 是（直接给 Decision） | 否 | 否 | 自动批准测试命令、阻断敏感文件 |

10 类事件 = 7 类纯同步 + 3 类同步/异步可选（PostToolUse/PostToolUseFailure/Stop）。`asyncRewake` 不是第 11 类事件，而是这 3 类事件的子模式。

### 11.2 Hook 配置

Hook 从 `.minicoding/hooks.toml`（项目级）或 `~/.minicoding/hooks.toml`（用户级）加载，按事件分 10 段配置（PascalCase 段名）：

```toml
# ~/.minicoding/config.toml
[hooks]
on_hook_error = "continue"        # continue | deny | fail
default_timeout_sec = 30

[[hooks.PreToolUse]]
matcher = "fs.write"              # 工具名 glob，| 分隔多个，* 通配
command = "prettier --write ${TOOL_INPUT_PATH}"   # 简写：仅命令
timeout_sec = 10

[[hooks.PreToolUse]]
matcher = "shell.run"
command = "~/.minicoding/hooks/block-danger.sh"

[[hooks.PostToolUse]]
matcher = "fs.write|fs.edit"
command = "cargo fmt"             # 写后自动格式化

[[hooks.PermissionRequest]]
matcher = "shell.run"
command = "~/.minicoding/hooks/auto-approve-tests.sh"   # 自动批准 cargo test

[[hooks.SessionStart]]
command = "git status --short"    # 输出注入上下文

[[hooks.PreCompact]]
command = "~/.minicoding/hooks/backup-transcript.sh"

[[hooks.Stop]]
command = "cargo test --quiet"    # 一轮结束跑测试
```

`matcher` 语法：工具名 glob，`|` 分隔多个，`*` 通配。`command` 支持 `${TOOL_INPUT_<KEY>}` 占位符（按工具 input 字段展开，经 shell 转义防注入）。

### 11.3 Hook 协议

Hook 以"外部可执行 + JSON over stdio"为主协议（脚本友好），同时提供 Rust `Hook` trait 给内建/SDK 用。

**输入**（stdin，单行 JSON）：

```json
{
  "event": "PreToolUse",
  "session_id": "sess_01H...",
  "turn": 3,
  "tool": { "name": "fs.write", "input": { "path": "src/main.rs", "content": "..." } },
  "side_effect": "FileWrite",
  "verdict": "Ask",
  "cwd": "e:/projects/foo",
  "env": { "MINICODING_HOOK": "1" }
}
```

**输出**（stdout，单行 JSON，退出码 0 表成功）：

```json
{
  "decision": "allow",
  "reason": "auto-approved by hook",
  "modify_input": { "path": "src/main.rs", "content": "...格式化后..." },
  "inject_context": "本次 sprint 优先处理支付模块",
  "exit_message": "已自动运行 prettier"
}
```

**退出码**：

| 码 | 含义 |
|----|------|
| 0 | 输出有效 JSON，按 `decision` 处理 |
| 2 | 阻断（等价 `decision=deny`，reason 取 stderr） |
| 其他 | Hook 错误，按 `on_hook_error` 策略处理（默认 `continue`，记 warn） |

### 11.4 asyncRewake 异步唤醒

某些 Hook 需执行长时异步任务（如安全扫描、CI 触发、依赖更新检查），结果不应阻塞当前轮次。`asyncRewake` 让 Hook 声明异步执行，完成后"唤醒"Agent 继续处理结果（见 `docs/hooks.md` §11）。

**约束**（C-26/C-32）：

- `async_rewake` 仅对 `PostToolUse`/`PostToolUseFailure`/`Stop` 事件有效（这些是"事后"事件，不阻塞主流程）；
- `PreToolUse`/`PermissionRequest` 不支持（这些是"事前"事件，必须同步决策）；
- 后台 Hook 超时（`estimated_duration × 2`）后自动 kill，注入超时提示；
- 同一 session 最多 3 个并发 async_rewake（防资源耗尽）；
- async_rewake 的结果走 `inject_context`，包裹 `<async_rewake>` 边界，声明非指令；
- 后台进程与 `shell.run` 子进程遵守相同的凭证隔离（C-04）、沙箱策略（C-22）、路径沙箱（C-03）约束。

### 11.5 内置示例 Hook

| 名称 | 事件 | 用途 |
|------|------|------|
| `fmt-on-write` | PostToolUse(fs.write\|fs.edit) | 写后跑 `cargo fmt`/`prettier` |
| `auto-approve-tests` | PermissionRequest(shell.run) | 前缀 `cargo test`/`npm test` 自动批准 |
| `block-secrets` | PreToolUse(fs.write) | 拒绝写入含 `api_key`/`password` 的内容 |
| `git-status-inject` | SessionStart | 注入 `git status --short` |
| `backup-before-compact` | PreCompact | 压缩前备份 jsonl 到 `.backup` |
| `test-on-stop` | Stop | 一轮结束跑测试，失败则要求继续 |

### 11.6 安全约束

| 约束 | 说明 |
|------|------|
| L0 不可覆盖 | Hook 的 `allow` 对内置黑名单 `Deny` 无效（C-21） |
| Hook 隔离 | `ScriptHook` 子进程不继承凭证环境变量（同 `shell.run`） |
| 超时强制 | Hook 超时 kill，按 `on_hook_error` 处理 |
| 输出上限 | Hook stdout 截断 1 MiB，防 OOM |
| 路径校验 | `modify_input` 仍经 `sandbox_path`，Hook 不能借此越界 |
| 审计 | Hook 的 `allow`/`deny`/`modify_input` 全部落 `audit.log`，标注 `source=hook:<name>` |
| Prompt 注入 | `inject_context` 内容包裹 `<hook_context>` 边界，声明非指令 |

---

## 12. MCP 集成

MCP（Model Context Protocol）集成是 AI Coding 工具生态的关键接入点，GitHub/Slack/数据库等外部能力通过 MCP server 暴露给 Agent（见 `docs/design.md` §19、`docs/features.md` §8）。

### 12.1 连接 MCP server

minicoding 作为 MCP client，连接外部 MCP server，把其工具注册进 `ToolRegistry`，与内置工具统一调度。基于官方 Rust MCP SDK（`rmcp` 2.2，对齐 MCP 2025-11-25 spec），支持 stdio + Streamable HTTP + OAuth。

**配置示例**：

```toml
# .minicoding.toml
[mcp_servers.github]
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }   # 环境变量展开
cwd = "."
startup_timeout_sec = 20
tool_timeout_sec = 60
enabled = true
required = false                # true 时启动失败则 minicoding 拒绝启动
enabled_tools = ["list_prs", "create_pr"]   # None=全部

[mcp_servers.internal_api]
transport = "http"
url = "https://internal.corp/mcp"
bearer_token_env_var = "INTERNAL_API_TOKEN"   # 不直接写 token
http_headers = { "X-Client" = "minicoding" }
```

`bearer_token_env_var` 走环境变量引用而非明文，与凭证管理一致（C-04）。

### 12.2 工具命名与权限规则

MCP 工具以 `mcp__<server>__<tool>` 命名（如 `mcp__github__list_prs`），与内置工具在权限规则中可区分。权限规则（`policy.toml`）支持通配：

```toml
[[permission.allow]]
tool = "mcp__github__list_prs"      # 精确允许
[[permission.deny]]
tool = "mcp__github__*"             # 拒绝 github 所有其它工具
```

MCP 工具的 `side_effect` 由 server 在工具 schema 中声明（`readOnlyHint`/`destructiveHint`）；未声明 hint 的工具默认 `SideEffect::Command`（串行 + Ask，C-25）。

### 12.3 project 作用域首次批准

三作用域配置（参考 CC）：

| 作用域 | 存储位置 | 共享 | 首次使用 |
|--------|---------|------|---------|
| `local`（默认） | `~/.minicoding/mcp.json` | 私有当前用户 | 直接可用 |
| `project` | `.minicoding/mcp.json`（仓库根，入版本控制） | 团队共享 | **首次需逐人批准** |
| `user` | `~/.minicoding/mcp.json` 的 `[user]` 段 | 全局 | 直接可用 |

`project` 作用域的"首次批准"是关键安全机制（C-24），防止恶意仓库通过 `.minicoding/mcp.json` 植入恶意 server：首次 clone 一个含 `.minicoding/mcp.json` 的仓库时，minicoding 提示用户逐个确认是否启用其中的 MCP server，确认后写入 `~/.minicoding/mcp_choices.toml` 记忆。

`mcp_choices.toml` 结构：

```toml
[[choices]]
repo_root = "e:/projects/foo"
server = "github"
decision = "allow"            # allow | deny
chosen_at = "2026-07-24T10:00:00Z"
```

管理命令：

```bash
# 列出已配置 MCP server
minicoding mcp list

# 批准 project 作用域 server
minicoding mcp approve github

# 重置 project 作用域批准记忆
minicoding mcp reset-project-choices
```

### 12.4 MCP 进程池与后台预热

MCP server 子进程跨 turn 复用，不每 turn 重启（X-12）。`McpConnectionPool` 持有 `HashMap<ServerId, Arc<McpConnection>>`，连接一旦建立长期保活，直到 Runtime 关闭或 server 崩溃。

**后台预热**（X-13）：分两类 MCP server 启动时机：

| 类型 | 触发时机 | 阻塞首 turn？ |
|------|---------|---------------|
| 全局 server（`~/.minicoding/mcp.json`） | minicoding 进程启动 | 否（后台预热，首 turn 仅在未完成时阻塞） |
| 项目级 server（`.minicoding/mcp.json`） | 创建/resume session 时 | 否（后台预热，首 turn 仅在未完成时阻塞） |

**Inflight merge**（X-14）：同 server 的并发工具调用若参数相同，合并为一次实际调用，避免重复请求外部服务（如并发的 `mcp__github__list_prs` 只发一次 HTTP）。

**健康检查**：定期 `ping` 检测连接活性（默认 30s），Degraded 状态尝试重连（指数退避 1s → 2s → 4s → 上限 60s），Disconnected 状态下用户调用该 server 工具直接返回错误。

### 12.5 作为 MCP server 被调用

minicoding 自身可作为 MCP server，把内置 `fs`/`shell` 工具暴露给其他 Agent 调用（X-10）：

```bash
minicoding serve --as-mcp-server
```

这让 Claude Desktop 等其他 Agent 可以调用 minicoding 的工具能力。

### 12.6 工具检索（Tool Search）

当配置的 MCP server 提供数百个工具时，全量注入 LLM 的 `tools` 数组会吃掉大量 token 并降低模型选择准确度。Tool Search（X-09）让 MCP 工具延迟注册，仅当模型用 `tool.search` 工具检索到相关工具时才动态加入 `tools` 数组。索引基于 BM25（零外部依赖，CJK 逐字分词）。

---

## 13. 可观测性

可观测性是内建的一等公民，从 M0 起接入 OpenTelemetry（见 `docs/features.md` §9、`docs/getting-started.md` §1.4）。

### 13.1 OpenTelemetry 集成

所有跨组件边界必须打 OTel span：

- `session`：会话级 span
- `turn`：轮次级 span
- `llm_call`：LLM 调用 span（provider/model/request_tokens）
- `tool_call`：工具调用 span（工具名、`side_effect`、是否并行、耗时、结果大小、权限 verdict）
- `compress`：压缩 span
- `permission`：权限决策 span
- `hook.run`：Hook 执行 span（`hook.name`/`hook.event`/`hook.decision`）
- `mcp.call`：MCP 调用 span（server/tool/elapsed）
- `subagent`：子 Agent span（通过 OTel Context 传播挂在父 turn span 下）

业务代码只写 `tracing` 宏，subscriber 层同时输出本地文件日志与 OTLP trace，无重复埋点。

**导出配置**：

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT="http://localhost:4317"
export OTEL_TRACES_SAMPLER="always_on"   # 或 trace_id_ratio
minicoding "..."
```

未设置时自动降级为本地 fmt 日志。在 Jaeger/Tempo/Grafana 可见 `session > turn > llm_call > tool_call > permission` span 层级。

### 13.2 本地日志

`~/.minicoding/logs/minicoding.YYYY-MM-DD.log`，`tracing-appender` 滚动：

```
2026-07-24T10:00:00.123Z INFO session=sess_01H... turn=1 turn started
2026-07-24T10:00:00.456Z DEBUG session=sess_01H... llm provider=anthropic model=claude-sonnet-4 request_tokens=512
2026-07-24T10:00:01.789Z INFO session=sess_01H... tool name=fs.read elapsed_ms=3 bytes=1024
```

- 默认 `INFO`，`-v` 开 `DEBUG`，`-vv` 开 `TRACE`；
- 文件始终 `DEBUG`（受 `RUST_LOG` 覆盖）；
- 保留 7 天，超出自动清理。

日志中绝不打印完整密钥；`Authorization` 头在 trace 级别也只打前 4 字符 + `***`（C-04）。

### 13.3 doctor 自检

```bash
minicoding doctor --security
```

`doctor --security` 自检沙箱驱动类型与硬化状态、权限配置、VCS 保护、依赖漏洞等（见 §8.7）。

---

## 14. 扩展开发

`minicoding-rs` 提供多层扩展能力：自定义工具、Extension SDK、自定义 Provider（见 `docs/api.md` §3.12、§3.13、`docs/features.md` §14）。

### 14.1 自定义工具

实现 `Tool` trait（`docs/api.md` §3.3）：

```rust
use minicoding_core::tool::{Tool, ToolSchema, ToolResult, ToolError, SideEffect, ToolContext};
use std::sync::Arc;

pub struct WeatherTool;

#[trait_variant::make(Tool: Send)]
impl Tool for WeatherTool {
    fn name(&self) -> &str { "weather.get" }
    fn schema(&self) -> &ToolSchema { /* JSON Schema */ }
    fn side_effect(&self) -> SideEffect { SideEffect::Network }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        // 实现
        Ok(ToolResult::text("晴，25°C"))
    }
}

// 注册
let mut registry = ToolRegistry::new();
registry.register(Arc::new(WeatherTool));
```

工具实现必须：

- 所有路径经 `sandbox_path` 规范化并校验在工作目录内；
- 监听 `canceller`，及时中止；
- 输出超过 `max_output_bytes` 截断并标注；
- 如实返回 `side_effect()`（把写操作误标为 `None` 属于实现缺陷，绕过串行约束，C-11）。

### 14.2 Extension SDK

Extension 系统为第三方扩展作者提供稳定 API（X-20/X-21/X-22，见 `docs/api.md` §3.12）。扩展通过 `Registrar` 注册工具/Hook/prompt contributor 等能力（6 类注册项），扩展注册的工具仍统一走 `ToolRegistry` dispatch，确保权限审计一致（C-01/C-02 不被绕过）。

```rust
use minicoding_core::extension::{Extension, Registrar, ExtensionManifest};
use minicoding_extension_sdk::ExtensionId;

pub struct MyExtension { /* ... */ }

impl Extension for MyExtension {
    fn manifest(&self) -> &ExtensionManifest { /* ... */ }

    fn init(&self, registrar: &mut dyn Registrar, config: serde_json::Value)
        -> BoxFuture<'_, Result<(), ExtensionError>> {
        Box::pin(async move {
            registrar.register_tool(Arc::new(WeatherTool))?;
            registrar.register_hook(Arc::new(MyHook))?;
            registrar.register_prompt_contributor(Arc::new(MyContributor))?;
            Ok(())
        })
    }

    fn shutdown(&self) -> BoxFuture<'_, Result<(), ExtensionError>> {
        Box::pin(async move { Ok(()) })
    }
}
```

**6 类注册项**：

| 方法 | 注册内容 |
|------|---------|
| `register_tool` | 自定义工具 |
| `register_hook` | Hook |
| `register_prompt_contributor` | Prompt contributor（注入 system prompt section） |
| `register_keybinding` | 快捷键 |
| `register_status_item` | 状态栏项 |
| `register_command` | 斜杠命令 |

**扩展载体**（三类统一抽象）：

| 载体 | 说明 |
|------|------|
| `Bundled` | 进程内 first-party，name 查找符号 |
| `Ipc { path }` | disk IPC 子进程（可执行文件路径） |
| `Mcp { server_id }` | 复用 MCP server |

### 14.3 自定义 Provider

实现 `LlmProvider` trait（`docs/api.md` §3.1）：

```rust
use minicoding_core::provider::{LlmProvider, ChatRequest, Delta, LlmError, Capabilities};
use futures::stream::BoxStream;

pub struct MyProvider;

#[trait_variant::make(LlmProvider: Send)]
impl LlmProvider for MyProvider {
    fn id(&self) -> &str { "my-provider" }
    fn capabilities(&self) -> Capabilities { /* ... */ }
    fn tokenizer(&self) -> Arc<dyn Tokenizer> { /* ... */ }

    async fn chat_stream(
        &self,
        req: ChatRequest,
    ) -> Result<BoxStream<'static, Result<Delta, LlmError>>, LlmError> {
        // 实现
    }

    async fn count_tokens(&self, messages: &[Message]) -> usize { /* ... */ }
}
```

内置实现（`minicoding-providers` crate）：

- `OpenAIProvider`：`/v1/chat/completions` SSE + 工具调用（L-01），也覆盖 DeepSeek、Moonshot、本地 vLLM；
- `AnthropicProvider`：`/v1/messages` 事件流 + system 分离（L-02）；
- `OllamaProvider`：`/api/chat` NDJSON（L-03，本地模型）；
- `Router`：模型路由 trait + `StaticRouter` 骨架（L-06），按 `Task::kind` 路由。

### 14.4 SDK 高层 API

`minicoding-sdk`（M8）提供高层封装（`docs/api.md` §5）：

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

    // 执行任务
    let report = client.run_task("添加单元测试").await?;
    Ok(())
}
```

SDK 用户通过 `CallbackPrompter` 注册异步闭包处理权限交互。

### 14.5 跨语言 / 跨进程接口

`minicoding serve` 暴露多种协议（见 `docs/api.md` §9）：

| 协议 | 命令 | 适用 |
|------|------|------|
| HTTP/JSON-RPC | `minicoding serve` | 非 Rust 集成 |
| MCP Server | `minicoding serve --as-mcp-server` | 被其他 Agent 调用 |
| stdin/stdout NDJSON | `minicoding serve --ndjson` | 编辑器插件协议 |
| ACP stdio | `minicoding serve --acp` | Zed 等支持 ACP 的客户端 |
| LSP stdio | `minicoding serve --lsp` | VS Code/Neovim/Emacs/Helix 等 |

LSP 语义映射（E-15/E-16/E-17/E-18）：

- `workspace/executeCommand` → 发送 prompt / 斜杠命令
- `$/progress` → 流式 token 与工具进度
- `window/showMessageRequest` → 权限确认（`LspPrompter` 实现 `PermissionPrompter`）
- `textDocument/codeAction` → AI 快速操作（解释/重构/修复选中代码）

五者共用 `core` 的数据模型与 `minicoding-protocol` 的 wire types，仅序列化协议与传输层不同。

---

## 15. FAQ

### Q1: 如何选择预设？

- **`read-only`**：代码审计、日志诊断、第三方代码分析——只读 + OnRequest。
- **`auto`（默认）**：日常开发——工作区内自由读写执行，越界/网络 Ask。
- **`external-sandbox`**：CI/容器内批量任务——外层容器已隔离，避免双重沙箱。
- **`full-access`**：受信沙箱内全自动部署——需显式确认 + red 警告，**仅**在容器/VM 内运行。

### Q2: 如何从 Claude Code 迁移？

详见 `docs/getting-started.md` §3。关键映射：

- 配置：`~/.claude/settings.json`（JSON）→ `~/.minicoding/config.toml`（TOML）
- 项目记忆：`CLAUDE.md` → `AGENTS.md`（兼容，无需改名，配置 fallback 即可零成本迁移）
- Hooks：CC 27 类事件 → minicoding 10 类事件（协议一致，JSON over stdio）
- 权限：CC 单层 allow/deny → 两层模型（L0 硬黑名单 + L1 用户策略）
- 会话历史：JSONL 兼容（`parent_uuid` 前向兼容，旧文件零迁移可用）

**不兼容项**：

- Hook 覆盖黑名单：CC 依赖自觉，minicoding L0 硬约束不可覆盖（C-21）
- `CLAUDE.md` Agent 编辑：CC 允许，minicoding 默认 `Ask`，不可 `AllowAlways`（C-23）
- 浏览器认证缓存：CC 支持，minicoding 不支持，优先 API Key

### Q3: Linux 内核 < 5.13 不支持 Landlock 怎么办？

`minicoding-sandbox::detect_driver()` 在运行时探测内核支持：

- 内核 5.13+：启用 Landlock + libseccomp，`is_hardened()` 返回 `true`；
- 内核 < 5.13：降级为 `NoopDriver`，打 `warn` 日志，仅应用层权限生效；
- 检查命令：`uname -r` 与 `minicoding doctor --security`。

这是设计内的 fail-open 降级，不阻塞编译与运行。建议在容器/WSL2 内运行获得硬隔离，或显式选择 `--preset external-sandbox`。

### Q4: 非 TTY 下副作用工具被拒怎么办？

`NonInteractivePrompter` 默认 `deny`（`permission.non_tty_strategy = "deny"`）。解决方案：

- 显式 `--allow '<rule>'` 临时覆盖（仅本次运行）；
- 改 `permission.non_tty_strategy = "allow"`（高风险仍 Deny）或 `"fail"`（中止本轮）；
- CI 场景用 `minicoding exec --sandbox workspace-write` 配合 `--json` 流输出。

### Q5: 如何对接 Jaeger/Tempo/Grafana？

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT="http://localhost:4317"
export OTEL_TRACES_SAMPLER="always_on"
minicoding "..."
```

在 Jaeger/Tempo/Grafana 可见 `session > turn > llm_call > tool_call > permission` span 层级，每个工具调用记录工具名、`side_effect`、是否并行、耗时、结果大小、权限 verdict。

### Q6: 凭证如何安全存储？

- **环境变量**：CI/容器场景首选，`OPENAI_API_KEY` / `ANTHROPIC_API_KEY`；
- **OS keyring**：交互场景首选，`minicoding auth login --provider anthropic`；
- **文件 fallback**：`~/.minicoding/credentials`（0600），keyring 不可用降级，原子 rename + 0600；
- **配置文件明文**：**强烈不推荐**，仅本地调试，启动告警。

凭证仅存 Runtime 内存与 OS keyring，**不**下传给子进程环境（C-04）；`fs.read` 读取配置/凭证文件时自动脱敏；日志中密钥只打前 4 字符 + `***`。

### Q7: exec 模式下 AGENTS.md 有什么风险？

exec 模式**移除 per-command 审批门**——AGENTS.md 内容被无条件执行。这带来供应链攻击面（参考 Backslash 对 Codex 的安全研究）：恶意仓库的 AGENTS.md 可能含恶意指令（如"运行前先执行 `cp ~/.aws/credentials /tmp/x`"）。

**防御措施**：

- L0：exec 模式下 AGENTS.md 中的 shell 指令仍受内置黑名单约束；
- L0：exec 模式默认 `read-only` 沙箱，需显式 `--sandbox workspace-write` 才可写；
- L0：exec 模式下网络默认禁用；
- 审计：AGENTS.md 加载内容落 audit.log，标注 `source=project_doc`；
- 建议：CI 中用 `--sandbox external-sandbox` 在容器内运行。

**核心原则**：把 AGENTS.md 当作可执行的、不可信的供应链制品——像审计 Makefile 一样审计，**绝不**把 exec 模式指向非自己作者仓库的 AGENTS.md。

### Q8: 如何关闭压缩？

```toml
[context]
compress = false
```

关闭压缩直通（C-06 兜底）。但长会话下会很快超出 token 预算导致 `BudgetExceeded` 错误，建议仅在短会话场景关闭。

### Q9: /undo 为什么默认关闭？

`/undo` 是特性门控（`[features] file_undo = false`，默认关），因为 `before` 内容驻留内存有成本（含文件原文，落盘等于多存一份敏感数据）。开启时 `FileChangeJournal` 由 Runtime 持有，会话结束即销毁（不落盘）。

开启方式：

```toml
[features]
file_undo = true
```

跨会话回滚引导用户用 Git（`git checkout`/`git revert`），与 Codex 一致。

### Q10: 如何查看 MCP server 状态？

```bash
# 列出已配置 MCP server
minicoding mcp list

# TUI/Web 状态栏显示 MCP server 状态
# github: ✓  (Healthy)
# slack: ⏳  (预热中)
# db:    ✗   (Disconnected)
```

MCP server 状态由 `McpPoolEvent` 事件驱动（`Ready`/`Failed`/`Disconnected`/`Reconnected`），Runtime 转发为 `Event` 总线事件，前端据此更新状态指示器。

### Q11: 如何启用 Auto-Review 子代理？

```toml
[permission]
auto_review = "on"   # 默认 off
```

在 `OnRequest`/`OnFailure` 审批模式下可选启用"安全审查子代理"：当 `Verdict::Ask` 触发时，不直接弹窗给用户，而是先派一个独立小模型评估该工具调用的风险，自动批准低风险、询问中风险、拒绝高风险。

**关键约束**：auto-review 是审查者替换，不是权限扩展——它不能放宽 `writable_roots`、启用网络或削弱任何 L0 保护。其 `allow` 决策仍受内置黑名单约束；其 `deny` 可被用户 `/approve` 覆盖（仅当前 turn、当前动作，不可永久放宽策略）。

### Q12: 如何配置环境变量策略？

子进程（`shell.run`/MCP server/Hook 子进程）默认继承 minicoding 的环境变量，但环境变量中可能含凭证。`shell_environment_policy` 配置（见 `docs/security.md` §10）：

```toml
[shell_environment_policy]
# 三选一（互斥）：
include_only = ["PATH", "HOME", "USER", "LANG", "TERM"]   # 白名单：仅这些下传
# exclude = ["AWS_SECRET_ACCESS_KEY", "DATABASE_URL", "*_API_KEY"]  # 黑名单
# inherit_all = false   # 默认 false；true 时全继承（不推荐）

# minicoding 注入的凭证变量始终不下传（C-04 强制，不受上述配置影响）
always_strip = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "MINICODING_*"]
```

- `include_only`（白名单，推荐）：仅列明变量下传，其余全部剥离——最小权限原则；
- `exclude`（黑名单）：默认全继承，剥离列明变量——兼容性好但易遗漏；
- 二者未配置时，默认 `include_only = ["PATH", "HOME", "USER", "LANG", "TERM"]`。

### Q13: 如何细粒度控制网络访问？

`network_proxy` 配置（见 `docs/security.md` §11）提供"允许访问 API 域名但禁止其它"的细粒度控制：

```toml
[features.network_proxy]
enabled = true                        # 默认 false；启用后覆盖沙箱的二元网络控制
mode = "allowlist"                    # allowlist | denylist

[features.network_proxy.domains]
"api.anthropic.com" = "allow"         # LLM API
"api.openai.com" = "allow"
"crates.io" = "allow"                 # cargo publish
"*.githubusercontent.com" = "allow"   # GitHub 资源
"*.internal.corp" = "deny"            # 内网阻断
"*" = "deny"                          # 默认拒（allowlist 模式）
```

启用后 `SandboxPolicy::WorkspaceWrite` 的"默认禁网络"被 `network_proxy` 策略替代；仍受 SSRF 防护约束（即使 allowlist 含内网 IP 仍拒）。

### Q14: 如何查看审计日志？

```bash
# 按会话查询
minicoding audit list --session sess_01H...

# 按时间与工具查询
minicoding audit list --since 2026-07-01 --tool shell.run

# 统计
minicoding audit stats          # 工具调用频次、拒绝率
```

审计日志位于 `~/.minicoding/audit.log`（JSONL，0600 权限，追加写不可篡改）。

### Q15: 如何备份与导出会话？

```bash
# 导出为 Markdown 对话记录
minicoding session export <id> --format md

# 导出为原始 JSONL
minicoding session export <id> --format jsonl

# 打包 ~/.minicoding/ 为 tar.gz
minicoding backup create
```

数据生命周期（见 `docs/data-model.md` §9）：

| 数据 | 保留策略 |
|------|---------|
| 会话 jsonl | 永久（用户手动 `session prune --before` 清理） |
| 压缩备份 | 默认不保留；开启时 30 天 |
| 会话摘要 | 永久 |
| 日志文件 | 7 天滚动 |
| 临时文件（工具中间产物） | 会话结束即删 |

---

## 附录：路径约定速查

`minicoding-rs` 采用**单根目录**约定（见 `docs/data-model.md` §3.0）：

```
$MINICODING_HOME  (默认 ~/.minicoding/)
├── config.toml              # 用户配置
├── policy.toml              # 权限决策持久化
├── audit.log                # 工具调用审计（JSONL，追加写）
├── AGENTS.md                # 全局项目记忆指令层
├── AGENTS.override.md       # 全局 override（可选）
├── IDENTITY.md              # 身份覆盖（P-31）
├── mcp.json                 # MCP server 配置（local + user 作用域）
├── mcp_choices.toml         # project 作用域 MCP server 批准记忆
├── .last-known-good.toml    # last-known-good 配置回退（S-20）
├── memory/
│   ├── long_term.md         # 长期记忆（人机共读）
│   ├── long_term.index.json # 长期记忆索引（程序化查询）
│   ├── auto.md              # Auto memory（Agent 可写）
│   └── sessions/
│       └── {summary_id}.md  # 每会话摘要
├── sessions/
│   ├── index.json           # 会话索引（轻量元数据）
│   ├── {session_id}.jsonl   # 会话日志（追加写，消息层）
│   ├── {session_id}.events.jsonl  # 事件流（Event Sourcing）
│   └── {session_id}.snapshot.json # 事件 snapshot（每 50 条 MessageAppended 落盘）
└── logs/
    └── minicoding.YYYY-MM-DD.log
```

根目录可通过 `MINICODING_HOME` 环境变量覆盖（绝对路径）；设置后所有子路径都相对该根解析。项目级配置使用工作目录下的 `.minicoding.toml`。凭证**不**落入此目录，统一存 OS keyring。

---

## 参考文档

- [README.md](../README.md)：项目概览
- [docs/design.md](design.md)：详细设计（Agent 循环、上下文管理、工具调度、Plan/Undo/Task/MCP/Hooks/Event Sourcing/Web/桌面等核心机制）
- [docs/modules.md](modules.md)：模块详细设计（18 crate + web 前端）
- [docs/api.md](api.md)：接口设计（核心 trait、公共 API、配置 schema）
- [docs/data-model.md](data-model.md)：数据模型与存储（Message / Session / ToolCall / Event 等）
- [docs/security.md](security.md)：安全与权限（权限策略、审计、OS 沙箱、审批模式与预设）
- [docs/hooks.md](hooks.md)：Hooks 系统设计（10 类生命周期 Hook、协议、asyncRewake、安全约束）
- [docs/tech-stack.md](tech-stack.md)：技术选型
- [docs/roadmap.md](roadmap.md)：开发路线图（M0–M10）
- [docs/dev-plan.md](dev-plan.md)：详细开发计划
- [docs/features.md](features.md)：功能清单（182 项）
- [docs/rules.md](rules.md)：运行时大模型约束（C-01..C-35）
- [docs/m9-design.md](m9-design.md)：Web 前端 + Tauri 桌面壳详细设计
- [docs/getting-started.md](getting-started.md)：上手指南（从 Claude Code / Codex 迁移指南）
- [AGENTS.md](../AGENTS.md)：AI 助手开发本项目时的编码/架构/文档/安全/前端规范
