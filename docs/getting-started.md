本文是 minicoding-rs 的快速入门指南，面向新 Rust 开发者。详细设计见 `docs/` 下各专题文档。

---

# minicoding-rs 入门指南

`minicoding-rs` 是一个 Rust 实现的终端 AI Coding 助手（参考 Claude Code / Codex CLI 设计），提供 Agent 循环、工具系统、权限沙箱、上下文管理、MCP 接入、会话审计等能力。本文帮助新 Rust 开发者在 30 分钟内从零跑通项目，并理解其与同类工具的差异、从 Claude Code 迁移的路径，以及一个端到端的演示。

> 阅读约定：本文引用的 `tech-stack.md`、`modules.md`、`design.md`、`security.md`、`features.md`、`data-model.md`、`hooks.md`、`architecture.md`、`dev-plan.md` 等均为 `docs/` 目录下的相对路径。

---

## 1. From Zero to Running（从零到运行）

本节覆盖前置依赖、克隆构建、首次运行与常见问题排查。技术细节来自 `tech-stack.md` 与 `AGENTS.md`。

### 1.1 前置条件清单

#### Rust 工具链

| 项 | 要求 | 来源 |
|----|------|------|
| edition | 2024 | `AGENTS.md` §2.1、`tech-stack.md` §1 |
| MSRV | 1.99+ | edition 2024 稳定门槛，含稳定的 `async fn in trait` |
| 工具 | `cargo` + `rustfmt` + `clippy` | 标准组合 |

安装 Rust（若未安装）：

```bash
# Unix
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# Windows PowerShell
irm https://sh.rustup.rs | iex
```

确认版本：

```bash
rustc --version    # 应 >= 1.99.0
cargo --version
```

#### 系统依赖

`minicoding-rs` 的依赖树刻意避免 OpenSSL / protobuf / cmake / clang 等重型 C 依赖（见 `tech-stack.md` §3、§11）：

- HTTP 走 `reqwest` + `rustls`（不依赖系统 OpenSSL）；
- `landlock` crate 是纯 Rust 绑定，无 C 依赖（见 `modules.md` §7.3）；
- `rmcp` 2.2 是纯 Rust；
- `sandbox-run` 跨平台 Rust 实现。

**当前无需任何系统包**（2026-08-25 R2 审查 DOC-2 修正）：Linux 沙箱仅依赖
内核 Landlock LSM（5.13+ 内核原生支持），`libseccomp` **尚未接入**
（`seccomp 待接入`，见 `tech-stack.md` §13），安装指引中的系统包为历史残留，
照做只会装无用软件包：

| 平台 | 系统依赖 | 安装命令 |
|------|---------|---------|
| Linux（启用沙箱时） | `libseccomp` 开发头文件 | Debian/Ubuntu: `sudo apt install libseccomp-dev`；Fedora: `sudo dnf install libseccomp-devel`；Arch: `sudo pacman -S libseccomp` |
| macOS | 无额外系统包 | `sandbox-run` 封装原生 Seatbelt 框架 |
| Windows | 无额外系统包 | `windows` crate 使用系统 API |
| 全平台 | Git（用于 `git.diff`/`git.apply` 与 VCS 目录检测） | 各平台包管理器 |

不需要 protoc（项目用 `serde_json`，不用 protobuf）、不需要 cmake、不需要 clang。

### 1.2 克隆与构建

```bash
git clone <repo-url> minicoding-rs
cd minicoding-rs

# 全量构建 workspace（18 个 Cargo 成员 crate，见 modules.md §0.1；另有 minicoding-web 为独立 npm 项目）
cargo build --workspace

# 或仅构建 CLI
cargo build -p minicoding-cli
```

首次构建会拉取 `tokio`/`reqwest`/`rmcp`/`ratatui` 等依赖，耗时 3-8 分钟（取决于网络与机器）。`Cargo.lock` 已提交（CLI 项目约定，见 `AGENTS.md` §2.7），无需手动锁定版本。

验证构建：

```bash
cargo run -p minicoding-cli -- --help
cargo run -p minicoding-cli -- --version
```

### 1.3 配置 API Key 与 Provider

凭证只从环境变量或 OS keyring 读取，**绝不**写入配置文件明文（`security.md` §6、`AGENTS.md` §5.3）。MVP 阶段最简方式是环境变量：

```bash
# OpenAI 兼容（M1 默认 provider，见 dev-plan.md T-M1-4）
export OPENAI_API_KEY="sk-..."

# 或 Anthropic（M6 交付，见 dev-plan.md T-M6-1）
export ANTHROPIC_API_KEY="sk-ant-..."

# 或本地 Ollama（M6 交付，无需 key）
# 确保 ollama serve 在 127.0.0.1:11434 运行
```

Windows PowerShell：

```powershell
$env:OPENAI_API_KEY = "sk-..."
```

交互场景推荐用 OS keyring（M4 交付 `auth login` 子命令，见 `data-model.md` §7）：

```bash
minicoding auth login --provider anthropic
# 输入密钥（不回显）→ 写入 OS keyring
minicoding auth status
```

#### 连接 OpenAI 兼容 API（DeepSeek/Moonshot/vLLM 等）

通过 `--api-base` 和 `--provider-name` 连接任何 OpenAI 兼容 API：

```bash
# DeepSeek
minicoding --provider openai --provider-name deepseek \
  --api-base https://api.deepseek.com \
  --model deepseek-chat "重构 utils 模块"

# Moonshot
minicoding --provider openai --provider-name moonshot \
  --api-base https://api.moonshot.cn/v1 \
  --model moonshot-v1-128k "审计依赖图"

# 本地 vLLM
minicoding --provider openai --provider-name vllm \
  --api-base http://localhost:8000/v1 \
  --model meta-llama/Llama-3-70B "生成 API 文档"
```

或持久化到 `~/.minicoding/config.toml`：

```toml
[provider]
default = "openai"
name = "deepseek"
api_base = "https://api.deepseek.com"
model = "deepseek-chat"
# api_key 留空，从 keyring/环境变量读取（推荐）
```

配置优先级（所有入口统一）：`CLI 参数 > 环境变量 > config.toml > provider 默认值`。

#### 桌面安装包配置

桌面应用用户无需命令行操作——首次启动自动弹出设置向导，填写 Provider 类型、API base、模型、API key 后保存即可（详见 `product-manual.md` §6.2）。

### 1.4 首次运行

M1 验收标准（见 `dev-plan.md` §12.2）要求单次模式可用：

```bash
# 单次提问（M1 交付）
minicoding "读取 src/main.rs 并解释"

# 交互会话（M2 交付基础 REPL，M5 起 /undo /plan /mcp 命令）
minicoding --session

# 非 TTY 批量执行（M4 交付，默认 read-only 沙箱）
minicoding exec --sandbox read-only "总结 README"
```

可观测性是内建的一等公民（`tech-stack.md` §7、`architecture.md` §7.3）。如需导出 trace 到本地 collector：

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT="http://localhost:4317"
minicoding "..."
```

未设置时自动降级为本地 fmt 日志（`~/.minicoding/logs/minicoding.YYYY-MM-DD.log`，见 `data-model.md` §3.0、§8）。

### 1.5 常见构建问题排查

#### Linux：内核 < 5.13 不支持 Landlock

`landlock` crate 依赖 Linux 5.13+ 的 Landlock LSM。`minicoding-sandbox::detect_driver()` 在运行时调用 `sandbox_run::landlock_available()` 探测内核支持（见 `tech-stack.md` §11、`modules.md` §7.4）：

- 内核 5.13+：启用 Landlock（文件隔离；网络 TCP 拒绝需 6.7+），`is_hardened()` 返回 `true`；
- 内核 < 5.13：降级为 `NoopDriver`（来自 `minicoding-core`），打 `warn` 日志，仅应用层权限（`sandbox_path` + `PermissionPolicy`）生效；
- 检查命令：`uname -r` 与 `minicoding doctor --security`（M4 交付）。

这是设计内的 fail-open 降级，不阻塞编译与运行。

#### Windows：沙箱成熟度低

Windows 缺乏 macOS Seatbelt / Linux Landlock 这样成熟的内核级 MAC 框架（`security.md` §12）。M4 初期策略：

- 应用层路径沙箱 + 用户提示「Windows 沙箱降级，建议在 WSL2/容器内运行」；
- M4+ 补齐受限令牌 + Job Object + DACL（`windows` crate）；
- `doctor --security` 如实报告 `is_hardened() = false` 并建议 WSL2。

Windows 上 `cargo build -p minicoding-sandbox` 仍可通过（`landlock` 通过 `[target.'cfg(target_os = "linux")'.dependencies]` 条件引入，非 Linux 不编译，见 `AGENTS.md` §3.5、`modules.md` §7.4）。

#### macOS：Seatbelt 由 sandbox-run 封装

macOS 12+ 由 `sandbox-run` 生成 profile 并 `apply_sandbox`，无需手写 profile 字符串（`tech-stack.md` §11）。M5+ 才补齐 macOS CI matrix（见下文平台优先级）。

#### 平台优先级策略（关键）

沙箱与核心 Runtime 的多平台支持分阶段交付（见 `tech-stack.md` §11「平台优先级」）：

| 阶段 | 平台支持 |
|------|---------|
| M0-M4（Linux 先行） | 沙箱自研 pre_exec 胶水 + `landlock`（无 seccomp），CI matrix 只跑 Linux。macOS/Windows 编译可用但沙箱降级为 `NoopDriver` + 应用层权限 + 用户提示 |
| M5+（macOS 补齐） | 补齐 macOS `sandbox-run`（Seatbelt）实现与 CI matrix |
| M6+（Windows 补齐） | 补齐 Windows 受限令牌 + Job Object 实现 |

**结论**：M0-M4 阶段在 macOS/Windows 上开发不阻塞，但内核级沙箱不可用——依赖容器/WSL2 做硬隔离，或显式选择 `--preset external-sandbox`（声明依赖外部容器隔离，见 `security.md` §8.1、§9.2）。

#### 其他常见问题

| 现象 | 原因 | 解决 |
|------|------|------|
| `cargo audit` 报漏洞 | 依赖含已知 RUSTSEC 条目 | `cargo update` 升级补丁版本；CI 阻塞合并（见 `AGENTS.md` §6.3） |
| `clippy -D warnings` 失败 | 代码违反 `clippy::all` + `clippy::pedantic`（`AGENTS.md` §2.9） | 按提示修复，不全局 `#![allow(...)]` |
| 首次运行权限报错 | 未设置 `OPENAI_API_KEY` 或 keyring 不可用 | 设环境变量或 `minicoding auth login` |
| 非 TTY 下副作用工具被拒 | `NonInteractivePrompter` 默认 `deny`（`security.md` §2.1） | 显式 `--allow` 或改 `permission.non_tty_strategy` |

---

## 2. Compare & Contrast（与同类工具对比）

本节对比 `minicoding-rs` 与 Claude Code（CC）、Codex CLI、Aider，并说明核心差异化价值。对比维度来自 `architecture.md`、`security.md`、`features.md`。

### 2.1 对比表

| 维度 | minicoding-rs | Claude Code | Codex CLI | Aider |
|------|--------------|-------------|-----------|-------|
| 实现语言 | Rust（edition 2024，MSRV 1.99+） | TypeScript/Node | Rust | Python |
| 内存安全 | 编译期保证，无 GC 暂停 | 运行时 GC | 编译期保证 | 运行时 GC |
| 沙箱机制 | OS 级一等公民：Landlock（自研胶水）+ Seatbelt + Windows Job Object，两道防线（应用层 + 内核级）；seccomp 待接入 | 应用层为主 | Landlock + seccomp（参考对象） | 无内核级沙箱 |
| 沙箱默认状态 | Opt-out（`WorkspaceWrite` 默认启用内核隔离） | Opt-in | Opt-out | N/A |
| MCP 支持 | `rmcp` 2.2 官方 SDK，stdio + HTTP + OAuth，project 作用域首次批准（C-24） | 支持 | 支持 | 不支持 |
| Hooks 系统 | 10 类事件 + ScriptHook + asyncRewake，L0 不可覆盖 | 27 类事件，依赖自觉 | 无 | 无 |
| 权限模型 | 两层：L0 硬黑名单 + L1 用户策略（specificity 单一竞争），决策与交互分离 | 单层 allow/deny，依赖 Hook | 两层（builtin + user） | 简单确认 |
| 记忆系统 | 三层：工作记忆 + 会话摘要 + 长期记忆双文件 + Auto memory + AGENTS.md 项目记忆 | CLAUDE.md + Auto memory | AGENTS.md | 单文件约定 |
| 可观测性 | OpenTelemetry 一等公民（M0 起接入），全链路 span | 无统一 trace | tracing 日志 | 无 |
| 可嵌入性 | 14 crate workspace，`minicoding-sdk` 提供 `Client`/`ask`/`run_task` API（M8） | 不可嵌入 | 不可嵌入 | 不可嵌入 |
| 部署形态 | CLI / TUI / SDK / HTTP server / MCP server（被其他 Agent 调用） | CLI | CLI | CLI |
| 配置格式 | TOML（`~/.minicoding/config.toml`） | JSON | TOML | INI/CLI |
| 开源协议 | MIT/Apache-2.0（`AGENTS.md` §2.7 限制） | 闭源 | Apache-2.0 | Apache-2.0 |
| 跨平台沙箱成熟度 | Linux 先行（M0-M4），macOS M5+，Windows M6+ | macOS/Linux | Linux 优先 | N/A |

### 2.2 核心差异化价值（为什么用 minicoding-rs）

#### 2.2.1 Rust 内存安全 + 零成本抽象

Agent 会执行文件写入、Shell 命令、网络请求等高权限操作，内存安全直接降低攻击面（`tech-stack.md` §1.1）。Rust 编译为原生二进制，冷启动远优于 Node/Python 实现，适合 CLI。`tokio` 提供成熟的异步 IO，便于流式响应与并行工具调用。

#### 2.2.2 OpenTelemetry 一等公民

不同于 CC/Codex 仅打本地日志，`minicoding-rs` 从 M0 起接入 OTel（`tech-stack.md` §7、`architecture.md` §7.3）：

- 所有跨组件边界（session/turn/llm_call/tool_call/compress/permission/hook.run/mcp.call）必须打 OTel span；
- 业务代码只写 `tracing` 宏，subscriber 层同时输出本地文件日志与 OTLP trace，无重复埋点；
- 后端由 `OTEL_EXPORTER_OTLP_ENDPOINT` 环境变量控制，零代码改动即可对接 Jaeger/Tempo/Grafana。

这让生产环境下的 Agent 行为分析、性能瓶颈定位、异常归因成为可能，而非黑盒。

#### 2.2.3 L0 硬约束不可绕过

`rules.md` 定义 35 条约束，其中 L0（C-01..C-07、C-21..C-30）在**实现层**被强制，不依赖 LLM 自觉或系统提示词（`AGENTS.md` §5.1）：

- 内置黑名单（危险命令/SSRF/敏感路径/AGENTS.md 写）优先级最高，用户配置与 Hook 都无法覆盖（C-02、C-21）；
- 沙箱拒绝是内核级硬反馈，不可被应用层 `allow` 覆盖（C-30）；
- 压缩熔断由 Runtime 状态机判定，与 LLM 输出无关（C-29）；
- 凭证仅存内存与 OS keyring，不下传子进程 env，日志脱敏（C-04）。

CC 的 Hook 可覆盖黑名单（依赖自觉），minicoding-rs 的 L0 是编译期 + 运行期双重强制。

#### 2.2.4 OS 沙箱一等公民 + 两道防线

沙箱从「后续可选」升级为默认路径（`security.md` §8）：

- `WorkspaceWrite` 是默认预设，启动即应用内核级限制；
- 应用层（`sandbox_path` + `PermissionPolicy` + 黑名单）+ OS 层（Landlock/Seatbelt/受限令牌）独立两道防线；
- 即使应用层被绕过或误配，沙箱仍能在内核级阻止越界写/网络外联；
- Opt-out 而非 opt-in，避免「用户忘了开沙箱」导致裸奔。

Codex 是参考对象，但 minicoding-rs 在跨平台统一 API（`sandbox-run`）与 Windows 补齐上更进一步。

#### 2.2.5 14 crate 可嵌入 + 多部署形态

`modules.md` §0.1 定义的 14 crate workspace 使 `minicoding-core` 可被其他 Rust 项目直接依赖（`tech-stack.md` §1.1）：

- `minicoding-sdk`（M8）提供 `Client::ask`/`ask_stream`/`run_task` 高层 API，`CallbackPrompter` 供 SDK 用户闭包处理权限；
- `minicoding serve`（M8）暴露 HTTP/JSON-RPC，供编辑器插件调用；
- `minicoding serve --as-mcp-server`（M8）把自身工具暴露为 MCP server，反向被 Claude Desktop 等 Agent 调用；
- trait 定义集中在 core，实现可来自任意 crate，运行时装配（`AGENTS.md` §3.3）。

CC/Codex/Aider 均为单体 CLI，不可嵌入。

#### 2.2.6 AGENTS.md 兼容 + 跨工具迁移

`ProjectDocLoader` 支持 `CLAUDE.md`/`.cursorrules` 作为 fallback 文件名（`design.md` §8.6），无需改名即可复用 Claude/Cursor 写的项目记忆。同时引入 `AGENTS.md` 作为统一规范，并提供 `AGENTS.override.md` 本地覆盖机制。详见第 3 节迁移指南。

---

## 3. Migration Guide（从 Claude Code 迁移）

本节给出从 Claude Code 迁移到 minicoding-rs 的字段映射、兼容性说明与不兼容项清单。技术细节来自 `design.md` §8.6、`data-model.md`、`hooks.md` §10、`security.md` §2。

### 3.1 配置迁移：`~/.claude/settings.json` → `~/.minicoding/config.toml`

CC 用 JSON，minicoding 用 TOML（`data-model.md` §3.0、`architecture.md` §7.1）。配置路径约定为单根目录 `~/.minicoding/`（可用 `MINICODING_HOME` 覆盖，不采用 XDG 多目录分散方案）。

| CC `settings.json` 字段 | minicoding `config.toml` 字段 | 说明 |
|------------------------|------------------------------|------|
| `apiKeyHelper` | 环境变量 `OPENAI_API_KEY`/`ANTHROPIC_API_KEY` 或 `minicoding auth login` | 凭证不写入配置明文（C-04，`security.md` §6） |
| `permissions.allow` | `[[allow]]`（`policy.toml`）或 `--allow` CLI | specificity=2 的 L1 条目（`data-model.md` §5） |
| `permissions.deny` | `[[deny]]`（`policy.toml`）或 `--deny` CLI | specificity=2 的 L1 条目 |
| `hooks` | `[hooks]` + `[[hooks.<Event>]]` | 事件数从 27 减到 10（见 3.3） |
| `model` | `--model` CLI 或 `config.toml` `[provider].model` | M6 起 `--provider`/`--model` 覆盖（`dev-plan.md` T-M6-5） |
| `env` | `[shell_environment_policy]` | 白名单/黑名单策略（`security.md` §10） |
| `mcpServers` | `mcp.json`（local/user/project 三作用域） | project 作用域需首次批准（C-24，`data-model.md` §6.4） |
| —（CC 无对应） | `[provider].name` | 自定义显示名（DeepSeek/Moonshot 等，`--provider-name`） |
| —（CC 无对应） | `[provider].api_base` | API base URL 覆盖（`--api-base`，连接 OpenAI 兼容 API） |
| —（CC 无对应） | `[provider.small]` | 独立小 LLM 配置（摘要/压缩降本，继承主 provider 的 api_base/api_key） |

minicoding 配置加载优先级（高 → 低，见 `architecture.md` §7.1）：

```
CLI args > Env vars > Project config (./.minicoding.toml)
         > User config (~/.minicoding/config.toml) > Built-in defaults
```

### 3.2 项目记忆：`CLAUDE.md` → `AGENTS.md`（兼容）

minicoding 的 `ProjectDocLoader` 支持 `CLAUDE.md` 作为 fallback（`design.md` §8.6、`data-model.md` §6.4），无需改名即可复用 CC 写的项目记忆。

**分层加载算法**（`design.md` §8.6）：

```
1. 全局层：$MINICODING_HOME/AGENTS.md（或 AGENTS.override.md）
2. 项目层 walk：从 repo_root 逐级向下走到 cwd
   - 每级查找顺序：AGENTS.override.md → AGENTS.md → fallback 文件名
3. 拼接：root → leaf 顺序，空文件跳过
4. 截断：累计超过 32 KiB（project_doc_max_bytes 可配）静默截断
```

**fallback 配置**：

```toml
[project]
project_doc_fallback_filenames = ["CLAUDE.md", ".cursorrules", "TEAM_GUIDE.md"]
project_doc_max_bytes = 32768
```

**`@import` 语法**：AGENTS.md 支持 `@<相对路径>` 引用其他文件（递归深度上限 5 层，防循环引用），与 CC 一致。

**关键差异**：

- CC 的 `CLAUDE.md` 可被 Agent 编辑；minicoding 的 `AGENTS.md` 不可被 Agent 自主编辑——`fs.write`/`fs.edit` 对 `AGENTS.md` 默认 `Verdict::Ask` 且不可 `AllowAlways`（C-23，`design.md` §8.6、`security.md` §2）；
- minicoding 区分 `long_term.md`（跨项目动态记忆，Agent 可写）与 `AGENTS.md`（仓库内静态指令，Agent 不可写），见 `data-model.md` §6.4 对比表。

迁移建议：保留 `CLAUDE.md` 不动，配置 fallback 即可零成本迁移；新项目推荐用 `AGENTS.md` 命名以对齐规范。

### 3.3 Hooks 迁移：CC 27 类事件 → minicoding 10 类事件

minicoding 按 CC 的事件分类精简为 10 类（`hooks.md` §2、§10），避免过度复杂。

| minicoding 事件 | 对应 CC 事件 | 兼容性 |
|----------------|-------------|--------|
| `SessionStart` | SessionStart | 直接兼容 |
| `UserPromptSubmit` | UserPromptSubmit | 直接兼容 |
| `PreToolUse` | PreToolUse | 直接兼容 |
| `PostToolUse` | PostToolUse | 直接兼容 |
| `PostToolUseFailure` | （CC 无独立事件） | minicoding 新增，需调整 |
| `PreCompact` | PreCompact | 直接兼容 |
| `PostCompact` | PostCompact | 直接兼容 |
| `Stop` | Stop | 直接兼容 |
| `SubagentStop` | SubagentStop | 直接兼容 |
| `PermissionRequest` | （CC 无独立事件） | minicoding 新增，可短路 Prompter |

**CC 的其他 17 类事件**（如 `Notification`、`PreToolUseMatch` 等细粒度事件）在 minicoding 中合并或未实现。迁移时：

- 协议一致：JSON over stdio，`HookInput`/`HookOutput` schema 与 CC 对齐（`hooks.md` §3）；
- 退出码语义一致：0 = allow，2 = deny，其他 = error（`hooks.md` §3.3）；
- `matcher` 语法一致：工具名 glob，`|` 分隔、`*` 通配（`hooks.md` §6）；
- 配置位置：`~/.claude/settings.json` 的 `hooks` → `~/.minicoding/config.toml` 的 `[hooks]`（TOML 格式）。

**关键差异**：

- CC 的 Hook 可覆盖黑名单（依赖自觉）；minicoding 的 L0 硬黑名单优先于 Hook，Hook 的 `allow` 对黑名单 `Deny` 无效（C-21，`hooks.md` §4、§7）；
- minicoding 的 `asyncRewake` 仅对 `PostToolUse`/`PostToolUseFailure`/`Stop` 有效，3 并发上限，超时 kill（C-26，`hooks.md` §11）。

### 3.4 会话历史：JSONL 兼容性

CC 与 minicoding 都用 JSONL 追加写（`data-model.md` §2）。minicoding 的 `message` 行新增可选字段 `parent_uuid`（支持 Fork/压缩边界/Side-chain），**前向兼容**（`data-model.md` §2.4）：

- 旧文件（v=1，无 `parent_uuid`）读取时按 `None` 处理，`Storage::load` 线性扫描时按「上一行 `id`」自动回填，等价于纯数组顺序模型；
- 旧文件零迁移可用；
- schema 版本字段 `v` 用于未来迁移：`migrate(v_from, v_to, record)` 链式升级。

CC 的 JSONL 字段映射：

| CC 字段 | minicoding 字段 | 说明 |
|---------|----------------|------|
| `uuid` | `id`（ULID） | 全局唯一且时间有序 |
| `parentUuid` | `parent_uuid` | 可选，前向兼容 |
| `type` | `type` | `session_start`/`message`/`compression`/`session_end`/`permission`/`error` |
| `message.role` | `role` | 一致 |
| `message.content` | `content`（`Vec<ContentBlock>`） | 一致 |
| `toolUseResult` | `tool` 行的 `content.tool_result` | 拆为独立 `message` 行 |

迁移建议：CC 的 JSONL 可直接拷贝到 `~/.minicoding/sessions/`，`Storage::load` 自动回填 `parent_uuid`。如需 Fork/Side-chain 检视，走单独的 `load_as_dag` 方法（`data-model.md` §3.3）。

### 3.5 权限配置：CC allow/deny → 两层模型

CC 是单层 allow/deny 列表；minicoding 是两层模型（`design.md` §9.5、`security.md` §2.3）：

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

CC 的 `permissions.allow`/`deny` 迁移到 `~/.minicoding/policy.toml`（`data-model.md` §5）：

```toml
[[allow]]
tool = "fs.write"
[allow.match]
glob = "src/**"

[[allow]]
tool = "shell.run"
[allow.match]
command_prefix = ["cargo ", "git status", "git diff"]

[[deny]]
tool = "shell.run"
[deny.match]
command_prefix = ["rm -rf", "sudo", "dd "]

[[deny]]
tool = "fs.write"
[deny.match]
glob = "{.git,.env,*.secret}/**"
```

**关键差异**：

- CC 的 `allow` 可覆盖危险命令（如 `rm -rf /`）；minicoding 的 L0 黑名单硬编码 `rm -rf /`/`sudo`/`dd of=/dev/`/fork bomb 等，任何用户配置无法覆盖（C-02，`security.md` §4.2）；
- minicoding 新增 `ApprovalMode`（Untrusted/OnFailure/OnRequest/Never）与预设（read-only/auto/external-sandbox/full-access），展开为 specificity=1 的 L1 规则（`security.md` §2.6）；
- `DangerFullAccess` 预设需启动时显式确认 + red 警告（C-22）。

### 3.6 不兼容项清单

| 项 | CC | minicoding | 处理 |
|----|----|-----------|------|
| Hook 覆盖黑名单 | 依赖自觉 | L0 硬约束，Hook 不可覆盖（C-21） | 删除覆盖黑名单的 Hook |
| `CLAUDE.md` Agent 编辑 | 允许 | 默认 `Ask`，不可 `AllowAlways`（C-23） | 改用 `long_term.md` 存动态记忆 |
| 17 类细粒度 Hook 事件 | 有 | 未实现 | 合并到 10 类事件或用 `EventBus` 订阅 |
| `asyncRewake` 协议 | CC 协议 | minicoding 协议（3 并发上限、超时 kill、事件白名单） | 按 `hooks.md` §11 调整 |
| 浏览器认证缓存 | 支持 | 不支持，优先 API Key（`security.md` §9.3） | 用 `minicoding auth login` 写 keyring |
| `~/.claude/` 路径 | 专用 | `~/.minicoding/`（`MINICODING_HOME` 可覆盖） | 迁移文件到新路径 |
| settings.json | JSON | config.toml（TOML） | 手动转换格式 |
| CC 特有 IDE 集成 | 深度集成 | M8 交付 stdin/stdout NDJSON 协议 | 等待 M8 或用 CLI 模式 |

---

## 4. Example Walkthrough（30 秒 Demo）

本节展示从安装到第一次工具调用的完整示例，以及权限交互与进阶用法。命令基于 `dev-plan.md` 的 M1-M5 验收标准。

### 4.1 安装

**方式一：从源码构建（开发期推荐）**

```bash
git clone <repo-url> minicoding-rs
cd minicoding-rs
cargo build --release -p minicoding-cli
# 二进制位于 target/release/minicoding
```

**方式二：cargo install（M6+ 分发，见 `features.md` Q-09）**

```bash
cargo install minicoding
```

**方式三：包管理器（M6+）**

```bash
# macOS
brew install minicoding
# Windows
scoop install minicoding
# Linux
cargo install minicoding
```

### 4.2 配置 API Key

```bash
export OPENAI_API_KEY="sk-..."
# 或
minicoding auth login --provider openai
```

### 4.3 第一次工具调用：读取并解释 Cargo.toml

```bash
minicoding "读取当前目录的 Cargo.toml 并解释其依赖"
```

**预期输出**（M1 验收标准，见 `dev-plan.md` §12.2）：

```text
[fs.read] path=Cargo.toml bytes=1024 elapsed=3ms
Cargo.toml 的依赖如下：

- tokio 1.x：异步运行时，提供 mpsc/broadcast/RwLock 等并发原语...
- reqwest（rustls-tls）：HTTP 客户端，使用 rustls 避免 OpenSSL 系统依赖...
- serde + serde_json：序列化框架...
- rmcp 2.2：官方 Rust MCP SDK，对齐 MCP 2025-11-25 spec...

该 workspace 包含 18 个 Cargo 成员 crate，依赖方向单向不循环（见 modules.md §0.2）。
```

执行流程（`architecture.md` §4.2）：

1. Frontend 构造 `UserInput`，Runtime 生成 `Message::user`；
2. `ContextManager` 注入系统提示、工具说明、记忆摘要，输出 `ChatRequest`；
3. `LlmProvider::chat_stream` 返回 `Stream<Item = LlmDelta>`；
4. `AgentLoop` 聚合 delta，识别到 `ToolCall(fs.read, {path: "Cargo.toml"})`；
5. `PermissionPolicy::check` 返回 `Allow`（`fs.read` 工作目录内默认 Allow，见 `security.md` §2.2）；
6. `Tool::execute` 读取文件，路径经 `sandbox_path` 校验（`security.md` §3）；
7. 结果以 `Message::tool_result` 追加到历史，回灌 LLM；
8. LLM 生成最终解释，流式输出到 stdout。

全程 `Storage` 以 JSONL 追加写盘到 `~/.minicoding/sessions/{session_id}.jsonl`（`data-model.md` §2.2）。

### 4.4 权限交互场景：写文件

```bash
minicoding "在 src/main.rs 里添加一个 hello 函数"
```

**预期输出**（M2 验收标准，见 `dev-plan.md` §12.3）：

```text
[fs.read] path=src/main.rs bytes=512 elapsed=2ms

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

输入 `y` 后：

```text
[fs.write] path=src/main.rs bytes=580 elapsed=4ms
已在 src/main.rs 添加 hello 函数：

fn hello() {
    println!("hello");
}

可通过 /undo 回滚此次改动。
```

**关键点**：

- `fs.write` 默认 `Ask`（`security.md` §2.2）；
- 风险评估由内置启发式生成，不调 LLM（`design.md` §9.6）；
- `fs.write` 成功后调 `Journal::record`（仅 `file-undo=true` 时生效，`modules.md` §11.3）；
- 审计记录落 `~/.minicoding/audit.log`（0600 权限，`security.md` §7）。

### 4.5 进阶用法

#### /undo 回滚（M5 交付，`features.md` S-07）

交互会话中：

```text
minicoding> /undo
回滚 operation #3：fs.write src/main.rs
- src/main.rs: 已恢复（580 → 512 bytes）
UndoReport: { succeeded: 1, failed: 0 }
```

`/undo` 是 operation 级回滚，恢复前比对当前文件内容与 `after`，不一致记入 `failed_files` 不强行覆盖（C-28，`design.md` §17.4、`modules.md` §6.3）。Journal 仅驻留内存，会话结束即销毁。

#### /plan Plan 模式（M5 交付，`features.md` A-06）

Plan 模式下副作用工具被硬门拒绝（`is_read_only() == false` 直接 `Deny`，C-25），模型只能探查与规划，调 `plan.exit` 后切回 Default 模式并缓存预批准 `allowed_prompts`。

```bash
# 启动时直接进入 Plan 模式
minicoding --plan

# 或在 REPL 内切换
minicoding> /plan          # 等价于 /plan on
minicoding> /plan status   # 查询当前模式
minicoding> /plan off      # 切回 Default 模式
```

`--plan` 等价于 REPL 内执行 `/plan`（`cli/main.rs`）。`plan.exit` 工具仅在 `PermissionMode::Plan` 下可调用，调用后触发 `Event::PermissionModeChanged { from: Plan, to: Default|AcceptEdits }`（`design.md` §16.4、`api.md` §10.2）。`task.spawn` 在 Plan 模式下被硬门拒绝（即便 `SideEffect::None`，`modules.md` §11.3）。

#### Hook 加载（M5 交付，`features.md` H-01）

Hook 从 `.minicoding/hooks.toml`（项目级）或 `~/.minicoding/hooks.toml`（用户级）加载，按事件分 10 段配置（`SessionStart`/`UserPromptSubmit`/`PreToolUse`/`PostToolUse`/...，PascalCase 段名，见 `core::config::HooksConfig`）。`hooks` feature 未启用时退化为 `NoopHookRegistry`（`modules.md` §12.3）。

```toml
# .minicoding/hooks.toml 示例（嵌套在 [hooks] 表下）
[hooks]
default_timeout_sec = 30
on_hook_error = "continue"

[[hooks.PostToolUse]]
command = "cargo fmt"
matcher = "fs.write|fs.edit"   # 工具名 glob，| 分隔、* 通配
timeout_sec = 20
```

#### --resume 恢复会话（M3 交付，`features.md` A-08）

```bash
# 列出会话（万级会话 < 1s，见 dev-plan.md T-M3-4）
minicoding session list

# 删除指定会话（原文件不可恢复）
minicoding session delete sess_01H...

# 恢复指定会话继续提问
minicoding --resume sess_01H...

# Fork 会话从分叉点尝试不同方向（features.md A-12）
minicoding --fork-session sess_01H...
```

`--resume`/`--replay`/`--fork-session` 三者互斥（见 `cli/builder.rs::SessionLoadMode`）。`--resume` 读取 `index.json` 的 `last_compaction_id` 定位起始行，避免全文件扫描（`data-model.md` §3.1、§3.3）。跨进程文件锁（`fs2`）防止两个进程同时写同一会话（`data-model.md` §10）。`session list`/`delete` 子命令不构建 `Runtime`，直接复用 `JsonlStorage` 同步方法，无需 API key。

#### --replay 回放（M3 交付，`features.md` A-09）

```bash
# 复现历史工具调用，默认禁用所有副作用工具（C-06）
minicoding --replay sess_01H...

# 如需重放工具，显式允许，且每条仍走权限策略
minicoding --replay sess_01H... --allow-side-effects
```

回放仅重新生成 LLM 响应，不重新执行已记录的工具调用（`security.md` §13.4）。

#### 沙箱策略切换（M4 交付，`features.md` P-16/P-18）

```bash
# 只读沙箱（代码审计、日志诊断）
minicoding exec --sandbox read-only "审计 src/ 目录的依赖"

# 工作区写（默认预设，日常开发）
minicoding --preset auto "重构 utils 模块"

# 外部沙箱（CI/容器内批量任务）
minicoding exec --sandbox external-sandbox "跑全套测试"

# 完全访问（需显式确认 + red 警告，仅受信沙箱内）
minicoding --preset full-access "全自动部署"
```

`minicoding doctor --security` 自检沙箱驱动类型与硬化状态（`features.md` P-23、`security.md` §16）：

```bash
minicoding doctor --security
```

#### MCP server（M5 交付，`features.md` X-01..X-08）

```bash
# 列出已配置 MCP server
minicoding mcp list

# 批准 project 作用域 server（首次进入含 .minicoding/mcp.json 的仓库时）
minicoding mcp approve github

# 重置 project 作用域批准记忆
minicoding mcp reset-project-choices
```

MCP 工具以 `mcp__<server>__<tool>` 命名注册（`design.md` §19.3），未声明 `readOnlyHint`/`destructiveHint` 的工具默认 `SideEffect::Command`（串行 + Ask，C-25）。

#### OTel 全链路追踪（M0 起接入）

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT="http://localhost:4317"
export OTEL_TRACES_SAMPLER="always_on"
minicoding "..."
```

在 Jaeger/Tempo/Grafana 可见 `session > turn > llm_call > tool_call > permission` span 层级（`architecture.md` §7.3），每个工具调用记录工具名、`side_effect`、是否并行、耗时、结果大小、权限 verdict。

---

## 参考

- `tech-stack.md`：技术选型、系统依赖、平台优先级
- `modules.md`：14 crate 结构与模块树
- `architecture.md`：分层架构、组件协作、数据流
- `security.md`：威胁模型、权限模型、沙箱边界、审计
- `design.md`：Agent 循环、上下文管理、权限集成、AGENTS.md 加载（§8.6）、两层权限模型（§9.5）
- `data-model.md`：数据模型、JSONL 格式、路径约定、凭证存储
- `hooks.md`：10 类 Hook 事件、协议、asyncRewake、与 CC 差异
- `features.md`：144 项功能总账
- `dev-plan.md`：71 个 task 的输入/输出/验收标准
- `AGENTS.md`：Rust 编码规范、依赖治理、安全规范、AI 助手行为约束
