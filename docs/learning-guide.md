# 项目学习文档（Learning Guide）

本文是 `minicoding-rs` 的**项目学习文档**，面向新加入的开发者，旨在用一份循序渐进的指南帮助你在最短时间内建立对项目全貌、核心机制、代码组织与开发规范的整体认知。

> **文档性质**：教学性文档，不是规范本身。架构规范的权威来源是 `AGENTS.md`，运行时约束的权威来源是 `docs/rules.md`，模块职责的权威来源是 `docs/modules.md`。本文档对它们的解读如有歧义，以原文为准。
>
> **阅读约定**：本文引用的相对路径均以项目根目录 `minicoding-rs/` 为基准（如 `crates/minicoding-core`、`docs/design.md`）。章节编号连续，可顺序阅读也可按需跳读。

---

## 目录

1. [学习路径概览](#1-学习路径概览)
2. [项目全景](#2-项目全景)
3. [核心概念理解](#3-核心概念理解)
4. [代码导读：从入口到核心](#4-代码导读从入口到核心)
5. [关键 trait 解析](#5-关键-trait-解析)
6. [数据流分析](#6-数据流分析)
7. [设计模式与最佳实践](#7-设计模式与最佳实践)
8. [Rust 编程要点](#8-rust-编程要点)
9. [测试策略](#9-测试策略)
10. [调试与排查](#10-调试与排查)
11. [扩展开发指南](#11-扩展开发指南)
12. [学习资源](#12-学习资源)
13. [术语表](#13-术语表)

---

## 1. 学习路径概览

### 1.1 建议学习顺序

本指南推荐以下五阶段学习路径，每阶段承前启后：

```
阶段 1（约 1h）：环境与全景
  ├─ 读 docs/getting-started.md 跑通项目
  ├─ 读本文 §2 项目全景，建立"18 crate 分层"心智模型
  └─ 读 AGENTS.md §1、§9，了解约束全貌

阶段 2（约 2h）：核心机制
  ├─ 读本文 §3 核心概念（Agent 循环、工具、权限、上下文、事件）
  ├─ 读 docs/design.md §1-§4，对照伪代码理解
  └─ 读 docs/architecture.md §3-§6，理解四层架构

阶段 3（约 3h）：代码导读
  ├─ 读本文 §4，按"CLI 入口 → Runtime → 工具/权限/上下文/存储"顺序走读
  ├─ 读 docs/api.md §3，对照 trait 签名
  └─ 读本文 §5，深入 trait 设计意图

阶段 4（约 2h）：规范与约束
  ├─ 读 docs/rules.md §2（L0 硬约束 C-01..C-07、C-21..C-30）
  ├─ 读 docs/security.md §2、§8（权限模型 + 沙箱）
  └─ 读本文 §7、§8，掌握 Rust 模式与编码要点

阶段 5（约 1h）：扩展与调试
  ├─ 读本文 §11 扩展开发指南
  ├─ 读 docs/hooks.md Hook 系统
  └─ 读本文 §10 调试排查
```

### 1.2 前置知识要求

| 领域 | 要求程度 | 说明 |
|------|---------|------|
| Rust 基础 | 扎实 | 所有权/借用/生命周期、`enum`/模式匹配、`Result`/`?`、泛型、trait |
| Rust 进阶 | 了解 | trait object（`dyn Trait`）、`Arc`/`Rc`、`Pin`/`BoxFuture`、`unsafe` 边界 |
| Rust async | 扎实 | `async fn`、`Future` trait、`tokio` runtime、`Stream`、`spawn` |
| Rust 2024 edition | 了解 | `async fn in trait` 已稳定、`trait-variant` 宏 |
| Cargo workspace | 了解 | 多 crate、`workspace.dependencies`、feature gate、`target.cfg` |
| serde 序列化 | 了解 | `#[derive(Serialize/Deserialize)]`、`#[serde(default)]` |
| tracing | 了解 | span/event/layer、OpenTelemetry 桥接 |
| LLM/Agent 概念 | 了解 | Chat Completion、流式响应、Tool Call、Context Window |
| 终端/TUI | 加分 | `clap`、`ratatui`（M7）、`crossterm` |

### 1.3 学习时间预估

| 阶段 | 估时 | 产出 |
|------|------|------|
| 阶段 1 全景 | 1 小时 | 能复述项目定位与分层 |
| 阶段 2 核心机制 | 2 小时 | 能画出 Agent 循环与权限流时序图 |
| 阶段 3 代码导读 | 3 小时 | 能定位"用户输入→工具调用→结果回灌"链路上的关键代码 |
| 阶段 4 规范约束 | 2 小时 | 能识别 L0 约束违反场景 |
| 阶段 5 扩展调试 | 1 小时 | 能独立添加一个内置 Tool |
| **合计** | **约 9 小时** | 具备独立开发能力 |

---

## 2. 项目全景

### 2.1 项目定位与目标

`minicoding-rs` 是一个 **Rust 实现的终端 AI Coding 助手**，参考 Claude Code / Codex CLI 设计，提供：

- **Agent 循环**：`prompt → LLM → tool_call → tool_result → LLM → ... → final` 的多轮编排；
- **工具系统**：内置 fs/shell/web/git/task/plan 等工具 + MCP 远程工具接入；
- **权限沙箱**：应用层权限策略（`PermissionPolicy`）+ OS 级内核沙箱（landlock/libseccomp/Seatbelt）双重防线；
- **上下文管理**：4 级压缩管道 + 熔断 + 预测性压缩 + Post-compact 恢复；
- **MCP 接入**：基于官方 `rmcp` 2.2，连接外部 MCP server 或将自身工具暴露为 MCP server；
- **会话审计**：JSONL 追加写持久化 + `audit.log` 审计落盘 + `/undo` 文件回滚。

项目以"**Rust 一等公民 + 不信任 LLM 输出 + 可观测性内建**"为核心理念。详见 `docs/architecture.md` §1 设计原则。

### 2.2 核心架构分层

项目采用四层架构，详见 `docs/architecture.md` §2：

```
┌──────────────────────────────────────────────────────────────┐
│                      Frontend Layer                          │
│   minicoding-cli   │   minicoding-tui   │   minicoding-sdk   │
└───────────────┬──────────────────────────────────────────────┘
                │  (调用 Runtime API)
┌───────────────▼──────────────────────────────────────────────┐
│             Orchestration Layer (minicoding-core)            │
│   Agent Loop │ Subagent │ Context Manager │ Event Bus        │
│   Plan 模式  │ Task 管理 │ Prompt 管道     │ Runtime 编排     │
└───────────────┬──────────────────────────────────────────────┘
                │  (依赖 trait 接口)
┌───────────────▼──────────────────────────────────────────────┐
│                    Capability Layer                          │
│  trait 定义（core）  +  实现 crate（单一职责，不交叉）        │
│  providers│tools│context│policy│sandbox│storage│hooks│journal│
│  mcp│memory│protocol│extension-sdk                          │
└──────────────────────────────────────────────────────────────┘
                │
┌───────────────▼──────────────────────────────────────────────┐
│                Infrastructure Layer                          │
│   tokio │ reqwest │ tracing │ serde │ camino │ os keyring     │
└──────────────────────────────────────────────────────────────┘
```

关键澄清（见 `docs/architecture.md` §3.5）：

- **`minicoding-core` 横跨 Orchestration 与 Capability 两层**——它的 Runtime 部分是编排，trait 定义部分是 Capability 抽象侧。这是"零实现 core"原则的体现：core 定义抽象 + 编排，**不含任何领域算法**。
- **`minicoding-tools` 是唯一"组合层"**，可依赖多个领域 crate（policy + journal + memory + sandbox + mcp + storage）完成工具执行闭环。
- **依赖单向不循环**：`core ◄─ 领域 crate ◄─ tools ◄─ frontend`，详见 `AGENTS.md` §3.2。

### 2.3 18 crate 全景图与职责

当前 Cargo workspace 含 18 个 crate（M0–M8 的 17 个 + M9 新增 `minicoding-desktop`），另有 `minicoding-web` 独立 npm 项目不入 workspace。完整列表见 `docs/modules.md` §0.1 与 `Cargo.toml`。

| Crate | 层 | 职责（一句话） |
|-------|----|----|
| `minicoding-core` | 抽象+编排 | trait 定义 + Runtime 编排 + 数据模型 + Event + Config + OTel + Prompt 管道，零实现 |
| `minicoding-context` | 能力实现 | `ContextManager` 实现 + 4 级压缩 + 熔断 + 预测性压缩 + Post-compact 恢复 |
| `minicoding-policy` | 能力实现 | 权限实现：`PermissionPolicy`/`Prompter` + builtin 黑名单 + `ApprovalMode`/`Preset` |
| `minicoding-memory` | 能力实现 | 长期/Auto/会话记忆 + `AGENTS.md` loader |
| `minicoding-hooks` | 能力实现 | `HookRegistry` + `ScriptHook` + `asyncRewake` + 内置 Hook |
| `minicoding-journal` | 能力实现 | `FileChangeJournal` + `/undo` 文件回滚 |
| `minicoding-sandbox` | 能力实现 | OS 沙箱驱动（`sandbox-run` + `landlock` + `libseccomp`） |
| `minicoding-mcp` | 能力实现 | MCP client/server（`rmcp` 2.2）+ 进程池 + project 作用域批准 |
| `minicoding-storage` | 能力实现 | JSONL 存储 + `audit.log` + EventStore + SnapshotStore（Event Sourcing） |
| `minicoding-providers` | 能力实现 | LLM Provider（OpenAI/Anthropic/Ollama）+ 小 LLM 配置 |
| `minicoding-tools` | 组合层 | 内置 Tool 实现（fs/shell/web/git/task/plan/mcp 包装） |
| `minicoding-protocol` | 协议层 | JSON-RPC 2.0 wire types + Event/Command DTO（前后端契约） |
| `minicoding-server` | 接入层 | HTTP/SSE server + ACP/LSP 适配器 + `--web` 静态托管（M9） |
| `minicoding-extension-sdk` | 扩展层 | 扩展作者稳定 API（`Extension` trait + `Registrar` + `Manifest`） |
| `minicoding-cli` | 前端 | CLI frontend（M0 起交付） |
| `minicoding-tui` | 前端 | TUI frontend（M7，`ratatui` + `crossterm`） |
| `minicoding-sdk` | 前端 | 嵌入 SDK（M8，供第三方 Rust 程序嵌入） |
| `minicoding-desktop` | 前端 | Tauri 2.x 桌面壳（M9，sidecar 启动 `minicoding-server`） |

> `minicoding-web`（M9，React 19 + TS 7 + Vite 8.1 + Tailwind v4）独立 `package.json`，不属于 Cargo workspace，详见 `docs/modules.md` §18、`AGENTS.md` §8。

### 2.4 依赖方向图解

依赖方向是**自上而下、单向不循环**，权威图见 `docs/modules.md` §0.2：

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
```

三条铁律（见 `AGENTS.md` §3.1、§3.2）：

1. **core 不依赖任何领域 crate**；
2. **领域 crate 之间不互相依赖**（如 `policy` 不依赖 `memory`，需要协作时通过 core trait 解耦）；
3. **tools 是唯一可依赖多领域 crate 的组合层**。

---

## 3. 核心概念理解

### 3.1 Agent 循环机制

Agent 循环是项目的"心脏"，伪代码见 `docs/design.md` §2.2。简化版：

```rust
loop {
    // 1. 构建请求（注入 system prompt + 工具 schema + 压缩后历史）
    let req = ctx.build_chat_request(&tools, &config).await?;
    // 2. 流式调用 LLM
    let mut stream = provider.chat_stream(req).await?;
    let acc = accumulate_deltas(stream).await?;   // 聚合 text + tool_call
    // 3. 落盘 assistant 消息（先写盘再广播，保证崩溃一致）
    storage.append(&session.id, &acc.message).await?;
    ctx.append(acc.message.clone()).await;
    // 4. 无工具调用 → 终止
    if acc.message.tool_calls.is_empty() {
        return Ok(TurnOutcome::Finished(acc.message));
    }
    // 5. 执行工具调用（无副作用并行 + 有副作用串行，见 §3.2）
    let results = execute_tool_calls(&acc.message.tool_calls).await?;
    // 6. 落盘 tool_result，回到步骤 2
    for r in results { storage.append(...).await?; ctx.append(...).await; }
}
```

**三条不变量**（见 `docs/design.md` §2.1）：

1. `Session.messages` 始终保持"合法消息序列"：`system? → (user → assistant → tool_result*)*`；
2. 每轮要么产生最终回复，要么产生 ≥1 个工具调用，绝不静默退出；
3. 任意中断后，`messages` 与磁盘 JSONL 一致（每条消息写盘后再广播）。

**停止条件与防御**（见 `docs/design.md` §2.4）：

| 条件 | 行为 |
|------|------|
| `stop_reason == EndTurn` | 正常结束 |
| `stop_reason == MaxTokens` | 截断警告，提示用户继续 |
| 连续工具调用次数 ≥ `max_tool_iters`（默认 50） | 强制终止并报错 |
| 单轮总耗时 ≥ `turn_timeout` | 取消并保留现场 |
| 相同工具调用连续重复 ≥ 3 次（防死循环） | 注入提示并降级 |

### 3.2 工具系统（Tool trait + ToolRegistry）

工具系统是 Agent 与外部世界交互的唯一通道。核心设计见 `docs/design.md` §4、`docs/api.md` §3.3。

**Tool trait**（简化，完整签名见 `docs/api.md` §3.3）：

```rust
#[trait_variant::make(Tool: Send)]
pub trait Tool {
    fn name(&self) -> &str;
    fn schema(&self) -> &ToolSchema;       // 给 LLM 的 JSON Schema
    fn side_effect(&self) -> SideEffect;   // 决定权限路径与并行/串行调度
    fn is_read_only(&self) -> bool { self.side_effect() == SideEffect::None }
    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext)
        -> Result<ToolResult, ToolError>;
}

pub enum SideEffect { None, FileWrite, Command, Network }
```

**`side_effect()` 的双重作用**：

1. **驱动权限策略**：`SideEffect != None` 必须经 `PermissionPolicy::check`（C-01）；
2. **驱动并行/串行调度**：无副作用工具并行（`buffer_unordered(8)`），有副作用工具严格串行（按 LLM 返回顺序逐个执行）。

**串行调度的理由**（见 `docs/design.md` §2.3）：副作用间往往存在隐式依赖（如先 `fs.write` 再 `shell.run cargo build`），并行会导致竞态、重复授权、审计顺序混乱、回滚不可追溯。

**内置工具集**（见 `docs/design.md` §4.3）：`fs.read/write/edit/multiedit/delete/list/glob/grep`、`shell.run/background/output/kill`、`web.fetch/search`、`git.diff/apply`、`task.spawn`、`plan.exit`，以及 MCP 远程工具包装（`mcp__<server>__<tool>` 命名）。

### 3.3 上下文管理（ContextManager + 压缩管道）

LLM 上下文窗口有限，必须主动压缩。详见 `docs/design.md` §3。

**消息权重模型**（决定压缩优先级，权重越低越先压缩）：

```
w = base(role) × recency × sticky × manual_pin

base(system)=1.0  base(user)=0.9  base(assistant)=0.6  base(tool_result)=0.4
recency = 1 - i/N  (越旧越低)
sticky = 1.5 (含错误/未提交变更)  manual_pin = 2.0 (用户标记)
```

**4 级压缩管道**（当 `tokens > budget × 0.85` 触发，逐级尝试）：

```
Level 1: 工具结果裁剪  — 大 tool_result 截断为"前 K 行 + ... + 后 K 行 + 元信息"
Level 2: 旧消息摘要    — 对权重最低的 N 条消息调 LLM 生成摘要替换原文
Level 3: 滚动窗口      — 仅保留最近 W 条 + 系统消息 + 摘要
Level 4: 硬截断        — 兜底，按 token 数从尾部保留，记录告警
```

**压缩熔断**（C-29，见 `docs/design.md` §3.6）：防止 Thrash Loop（压缩完即超阈值→再压缩→再填满）烧光 token。失败计数 ≥3 熔断注入错误中止本轮，≥5 强制 TurnEnd 保留现场。连续 2 次"压缩完即超"也熔断。**熔断由 Runtime 状态机判定，与 LLM 输出无关**。

**预测性压缩**（§3.9）：在 turn 间隙根据历史 token 增长 EMA 估算，提前 compact 避免打断流式输出。

**Post-compact 恢复**（§3.10）：compact 后把"最近读过的文件"重新注入 system 段（包裹 `<post_compact_context>` 边界），避免模型重新 `fs.read` 浪费 token。

### 3.4 权限模型（PermissionPolicy + Prompter，决策与交互分离）

权限采用**双 trait 设计**，详见 `docs/security.md` §2、`docs/api.md` §3.6、`docs/design.md` §9。

```
PermissionPolicy::check(...) -> Verdict   (纯决策)
   ├─ Allow
   ├─ Deny(reason)
   └─ Ask(prompt)

PermissionPrompter::prompt(prompt) -> Decision   (点对点交互)
   ├─ Allow
   └─ Deny(reason)
```

**为什么拆分**：`EventBus` 是广播式（`broadcast::Sender` 不可克隆承载回复），无法承担"请求-响应"语义。决策（policy）走 trait 返回 `Verdict`，交互（prompter）走 trait 返回终态 `Decision`，二者职责正交。

**两层解析模型**（见 `docs/security.md` §2.3）：

```
L0  内置硬黑名单 (policy::builtin)            ← 最高，不可被任何配置覆盖
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

**关键约束**：内置黑名单的 `Deny` 在 Hook 之前生效，Hook 的 `allow` 对黑名单 `Deny` 无效（C-21）。AGENTS.md/CLAUDE.md 写操作默认 `Verdict::Ask` 且不可 `AllowAlways`（C-23）。

### 3.5 事件总线（EventBus broadcast）

`EventBus` 仅广播通知（无回复通道），定义在 `crates/minicoding-core/src/runtime/event.rs`。事件类型见 `docs/modules.md` §3.3：

- `Event::Token(String)`：流式文本增量；
- `Event::MessageAppended(Message)`：消息落盘后广播；
- `Event::TurnEnd { stop_reason }`：轮次结束；
- `Event::TaskUpdated`：任务状态变更；
- `Event::HookRun`：Hook 执行记录；
- `Event::PermissionRequested` / `PermissionResolved`：权限请求与决策（仅展示/审计，无回复通道）；
- `Event::FileUndone`：`/undo` 文件回滚；
- `Event::ConfigChanged`：配置热更新（S-22）。

**与权限交互的关系**：`EventBus` 只广播 `PermissionRequested`/`PermissionResolved` 通知，**不承载回复**；真正的权限交互走 `PermissionPrompter` 点对点（`InteractivePrompter`/`NonInteractivePrompter`/`CallbackPrompter`/`TuiPrompter`）。

### 3.6 事件溯源（Event Sourcing）

项目引入 Event Sourcing 作为会话恢复的进阶机制，详见 `docs/design.md` §25、`docs/modules.md` §9。

- **`EventStore`** trait：事件流持久化到 `{id}.events.jsonl`，append + fsync 后返回；
- **`SnapshotStore`** trait：周期性快照到 `{id}.snapshot.json`，原子写（`.tmp` + `rename`）；
- **`replay_session_state`**：从 snapshot + 后续 events 重放恢复会话状态；
- **双写并存**：新会话同时写消息日志与事件流，旧会话无事件流时回退到消息日志路径。

`NoopEventStore`/`NoopSnapshotStore` 在 core 提供兜底实现，未启用 feature 时零开销。

### 3.7 沙箱两道防线（应用层 + OS 层）

沙箱是权限之外的第二道防线，详见 `docs/security.md` §8、`docs/design.md` §14。

**第一道防线（应用层）**：`PermissionPolicy::check` + `sandbox_path` 路径规范化校验（C-03），越界直接 `PathEscaped` 错误。

**第二道防线（OS 内核级）**：`SandboxDriver` trait，基于 `sandbox-run` + `landlock`（Linux）+ `libseccomp`（Linux）+ macOS Seatbelt + Windows 受限 token。

**四种 `SandboxPolicy`**（见 `docs/api.md` §2.4）：

| 策略 | 隔离强度 | 说明 |
|------|---------|------|
| `ReadOnly` | 强 | 仅允许读文件与白名单只读命令 |
| `WorkspaceWrite` | 中（默认） | 工作区内读写+命令执行，禁越界写、禁网络 |
| `ExternalSandbox` | 弱（CI） | 假定外层容器已隔离，`is_hardened()` 返回 false |
| `DangerFullAccess` | 无 | 关闭所有限制，需 red 警告 + 二次确认（C-22） |

**pre-exec apply**：`sandbox_run::apply_sandbox()` 在子进程 fork 后 exec 前调用，子进程启动即受限，无窗口期。

**沙箱拒绝熔断**（C-30，见 `docs/security.md` §8.8）：单 turn 内累计沙箱拒绝 ≥3 次熔断注入提醒，≥5 次强制 TurnEnd。沙箱拒绝来自内核级硬反馈（`EPERM`/Seatbelt denial/Landlock denial），**不可被应用层 `allow` 覆盖**。

---

## 4. 代码导读：从入口到核心

本章按"用户输入 → Runtime → 工具/权限/上下文/存储"顺序走读关键代码位置。所有路径相对项目根目录。

### 4.1 CLI 入口：`crates/minicoding-cli/src/main.rs`

CLI 是项目的默认入口，职责见 `docs/modules.md` §14：

- **参数解析**：`clap` derive 风格，解析 `minicoding [OPTIONS] [PROMPT]`、子命令（`serve`/`doctor`/`--replay` 等）；
- **配置加载**：分层合并（CLI args > env > project `.minicoding.toml` > user `~/.minicoding/config.toml` > 默认），见 `docs/architecture.md` §7.1；
- **凭证读取**：从环境变量或 OS keyring（`keyring` crate，`KEYRING_SERVICE = "minicoding"`），**绝不**从配置文件明文读取（C-04）；
- **Runtime 构建**：通过 `RuntimeBuilder` 链式注入各 trait 实现（见 §4.2）；
- **流式渲染**：订阅 `EventBus`，渲染 `Event::Token` 到终端（`indicatif` + `anstream`）；
- **权限交互**：`InteractivePrompter`（TTY）/ `NonInteractivePrompter`（非 TTY，按 `permission.non_tty_strategy` 处理）。

入口伪代码：

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = load_config(&cli)?;
    init_otel(&config)?;            // OTel 从 M0 接入
    let runtime = RuntimeBuilder::new()
        .provider(build_provider(&config)?)
        .context(build_context_manager(&config)?)
        .policy(build_policy(&config)?)
        .prompter(build_prompter(&config)?)
        .storage(build_storage(&config)?)
        .sandbox(build_sandbox(&config)?)   // feature gate
        .build()?;
    runtime.run_turn(user_input).await?;
    Ok(())
}
```

### 4.2 Runtime 构建：`RuntimeBuilder`

`RuntimeBuilder` 定义在 `crates/minicoding-core/src/runtime/builder.rs`，链式注入所有 trait 对象：

```rust
let runtime = RuntimeBuilder::new()
    .provider(Arc::new(provider))           // Arc<dyn LlmProvider>
    .context(Arc::new(ctx_manager))         // Arc<dyn ContextManager>
    .policy(Arc::new(policy))               // Arc<dyn PermissionPolicy>
    .prompter(Arc::new(prompter))           // Arc<dyn PermissionPrompter>
    .storage(Arc::new(storage))             // Arc<dyn Storage>
    .sandbox(Arc::new(sandbox))             // Arc<dyn SandboxDriver>
    .hooks(hooks_registry)                  // Option<Arc<dyn HookRegistry>>
    .journal(journal)                       // Option<Arc<dyn Journal>>
    .subagent_runner(runner)                // Option<Arc<dyn SubagentRunner>>
    .build()?;
```

Runtime 持有 `Arc<dyn Trait>` 不需知道具体实现 crate，依赖方向干净（见 `AGENTS.md` §3.3）。可选能力（memory/hooks/journal/sandbox/mcp）通过 feature gate 启用，未启用时由 core 的 `NoopDriver`/`NoopEventStore` 等兜底。

### 4.3 Agent 循环：`crates/minicoding-core/src/runtime/`

Agent 循环主体在 `crates/minicoding-core/src/runtime/mod.rs` 与 `rt.rs`，伪代码见 `docs/design.md` §2.2。关键文件：

- `runtime/mod.rs`：`Runtime` 聚合根 + `AgentLoop` 主循环（并行/串行分桶）；
- `runtime/rt.rs`：`Runtime` 实现（聚合各 trait，含 `register_dynamic_tool`/`journal`/`subagent_runner`）；
- `runtime/builder.rs`：`RuntimeBuilder`；
- `runtime/event.rs`：`Event`/`EventBus`；
- `runtime/accumulator.rs`：流式 delta 聚合（`Delta::Text`/`Delta::ToolCall`/`Delta::Usage`）。

**`run_turn` 主循环的 6 步**（见 §3.1）：写入用户消息 → 构建请求 → 流式调用 LLM → 落盘 assistant 消息 → 判止/执行工具 → 落盘 tool_result 并回到步骤 2。

### 4.4 工具执行：`crates/minicoding-tools/src/`

`minicoding-tools` 是唯一组合层，模块树见 `docs/modules.md` §11.2：

```
crates/minicoding-tools/src/
├── lib.rs             # register_all() 工厂
├── fs/                # read/write/edit/multiedit/delete/list/glob/grep
├── shell/             # run/background/output/kill
├── web/               # fetch/search/ssrf
├── git/               # diff/apply
├── task/              # spawn/create/update/list（增量模型，见 design.md §18）
└── plan/              # exit（Plan 模式双重只读强制，见 design.md §16）
```

**工具执行的完整闭环**（`Runtime::run_one`，见 `docs/design.md` §2.3 伪代码）：

```
权限解析 → 派发 → 审计 → 事件
  │
  ├─ policy.check() → Verdict
  ├─ Verdict::Ask → prompter.prompt() → Decision
  ├─ emit(ToolCallStart)
  ├─ tools.dispatch(call) → ToolResult
  ├─ emit(ToolCallEnd { ok, elapsed })
  └─ audit(call, decision, result)   // 无论 Allow/Deny 都落盘
```

`fs.write`/`fs.edit`/`fs.delete` 成功后会调 `Journal::record`（若启用 `file-undo` feature）记录 `ChangeEntry`，支持 `/undo` 回滚。`shell.run` 在执行前调 `SandboxDriver::apply` 应用内核级沙箱。

### 4.5 权限检查：`crates/minicoding-policy/src/`

模块树见 `docs/modules.md` §3.2：

```
crates/minicoding-policy/src/
├── lib.rs             # 工厂 build_policy(cfg)/build_prompter(cfg)
├── builtin.rs         # 内置不可覆盖黑名单（C-02）+ AGENTS.md 写保护（C-23）
├── mode.rs            # ApprovalMode/Preset 枚举与解析
├── prompter.rs        # InteractivePrompter/NonInteractivePrompter/CallbackPrompter
├── redact.rs          # 敏感数据脱敏（.env/api_key/password 模式替换，C-04）
├── ssrf.rs            # SSRF 防护（RFC1918/链路本地/回环/CGNAT 拒绝，C-02）
├── replay.rs          # ReplayPolicy（replay 模式禁副作用，C-06）
└── path_sandbox.rs    # sandbox_path 路径校验（应用层第一道防线）
```

**黑名单最高优先级**：`builtin.rs` 硬编码危险命令（`rm -rf /`/`sudo`/`dd of=/dev/`/fork bomb 等）、SSRF 内网目标、敏感路径（`.git/`/`.env`/`*.secret`），任何用户配置与 Hook 都无法覆盖。

### 4.6 上下文管理：`crates/minicoding-context/src/`

模块树见 `docs/modules.md` §2.2：

```
crates/minicoding-context/src/
├── manager.rs             # ContextManagerImpl（实现 trait）
├── budget.rs              # token 预算计算
├── weight.rs              # 消息权重模型
├── compress/              # 4 级压缩管道（clip/summarize/rolling/hard_truncate）
├── circuit_breaker.rs     # 压缩熔断状态机（C-29）
├── state_keep.rs          # 压缩后状态保留清单
├── fallback.rs            # L2 摘要失败降级链
├── predictive.rs          # 预测性压缩
├── post_compact_recover.rs # Post-compact 上下文恢复
└── tokenizer.rs           # tiktoken-rs 集成
```

`ContextManager` trait 定义在 `crates/minicoding-core/src/context/trait.rs`，关键方法：`append`/`build_chat_request`/`snapshot`/`restore`/`token_count`。

### 4.7 存储层：`crates/minicoding-storage/src/`

模块树见 `docs/modules.md` §9.2：

```
crates/minicoding-storage/src/
├── jsonl.rs            # JsonlStorage 实现 Storage（追加写、崩溃安全）
├── event_store.rs      # JsonlEventStore 实现 EventStore
├── snapshot_store.rs   # JsonlSnapshotStore 实现 SnapshotStore
├── index.rs            # 会话索引 index.json
├── lock.rs             # 跨进程文件锁（fs2）
├── audit.rs            # AuditSink 实现（audit.log JSONL，0600 权限）
└── export.rs           # 会话导出（md / jsonl）
```

**崩溃安全**：每条消息 `append` 后 `fsync`，崩溃时磁盘与内存一致。事件流 append + fsync；snapshot 走 `.tmp` + `rename` 原子写。

**审计完整性**（C-05 审计落盘）：`audit.log` 文件权限 0600，追加写不可篡改历史（无 update/delete API）。任何权限决策（`Allow`/`Deny`/`Ask`/`AllowAlways`/`DenyAlways`）必须落 `audit.log`，详见 `AGENTS.md` §5.5。

**会话文件路径**：`~/.minicoding/sessions/{session_id}.jsonl`，每行一条记录。记录结构见 `docs/data-model.md` §2.2。

---

## 5. 关键 trait 解析

所有领域 trait 在 `minicoding-core` 定义，实现在领域 crate。详见 `docs/api.md` §3、`AGENTS.md` §3.3。

### 5.1 Tool / ToolRegistry

- **定义位置**：`crates/minicoding-core/src/tool/trait.rs`、`registry.rs`；
- **实现位置**：`crates/minicoding-tools`（内置）/ `crates/minicoding-mcp`（远程包装 `McpToolWrapper`）；
- **设计意图**：统一抽象"LLM 可调用的能力"，`side_effect()` 同时驱动权限路径与并行/串行调度；
- **关键方法**：`name`/`schema`/`side_effect`/`is_read_only`/`execute`；
- **`ToolRegistry`**：`HashMap<String, Arc<dyn Tool>>` + `enabled_groups`，提供 `register`/`dispatch`/`schemas`。

### 5.2 LlmProvider / Tokenizer

- **定义位置**：`crates/minicoding-core/src/provider/trait.rs`；
- **实现位置**：`crates/minicoding-providers`（OpenAI/Anthropic/Ollama）；
- **设计意图**：多 Provider 统一抽象，避免引入官方 SDK（Rust 生态缺失或维护弱）；
- **关键方法**：`id`/`capabilities`/`tokenizer`/`chat_stream`（返回 `BoxStream<Result<Delta>>`）/`chat`（默认基于 stream 聚合）/`count_tokens`；
- **`Tokenizer`**：同步 trait（无 async），`count`/`count_messages`/`id`。

### 5.3 ContextManager

- **定义位置**：`crates/minicoding-core/src/context/trait.rs`；
- **实现位置**：`crates/minicoding-context`（`ContextManagerImpl`）；
- **设计意图**：把"消息历史 + token 预算 + 压缩"封装为可替换能力，Runtime 不感知压缩算法；
- **关键方法**：`append`/`build_chat_request`/`snapshot`/`restore`/`token_count`；
- **`ChatRequest`**：`system` + `messages` + `tools` + `params`，由 `build_chat_request` 组装。

### 5.4 PermissionPolicy / PermissionPrompter

- **定义位置**：`crates/minicoding-core/src/policy/trait.rs`；
- **实现位置**：`crates/minicoding-policy`（`PolicyEngine` + 各 Prompter）；
- **设计意图**：决策（policy 返回 `Verdict`）与交互（prompter 返回终态 `Decision`）分离，解决 broadcast 事件总线无法承载点对点回复的架构缺陷（见 `docs/design.md` §9.1）；
- **`PermissionPolicy::check`**：纯决策，返回 `Allow`/`Deny(reason)`/`Ask(prompt)`；
- **`PermissionPrompter::prompt`**：点对点交互，仅当 `Ask` 时被 Runtime 调用，返回 `Allow`/`Deny`；
- **实现**：`InteractivePrompter`（CLI TTY）/ `NonInteractivePrompter`（非 TTY）/ `CallbackPrompter`（SDK 闭包）/ `TuiPrompter`（M7）。

### 5.5 SandboxDriver

- **定义位置**：`crates/minicoding-core/src/sandbox/trait.rs`；
- **实现位置**：`crates/minicoding-sandbox`（核心 `NoopDriver` 在 core 兜底）；
- **设计意图**：OS 级隔离作为应用层权限之外的第二道防线（C-22），基于 `sandbox-run` + `landlock` + `libseccomp` 主流库，**不自研**胶水代码；
- **关键方法**：`apply`（在子进程 fork 后 exec 前调用）/`is_hardened`（`ExternalSandbox`/`DangerFullAccess` 返回 false）；
- **`NoopDriver`**：core 提供兜底实现，未启用 `minicoding-sandbox` feature 时使用。

### 5.6 Hook / HookRegistry

- **定义位置**：`crates/minicoding-core/src/hooks/trait_def.rs`；
- **实现位置**：`crates/minicoding-hooks`（`HookRegistryImpl` + `ScriptHook` 适配器）；
- **设计意图**：用户在不修改工具实现的前提下注入自定义逻辑（拦截/批准/改写参数/注入上下文）；
- **10 类事件**：`SessionStart`/`UserPromptSubmit`/`PreToolUse`/`PostToolUse`/`PostToolUseFailure`/`PreCompact`/`PostCompact`/`Stop`/`SubagentStop`/`PermissionRequest`（详见 `docs/hooks.md` §2）；
- **asyncRewake**：`PostToolUse`/`PostToolUseFailure`/`Stop` 三类事后事件支持异步唤醒（§11），后台子进程遵守凭证隔离（C-04）、沙箱（C-22）、路径沙箱（C-03）约束（C-26）；
- **L0 不可覆盖**：内置黑名单 `Deny` 在 Hook 之前生效，Hook 的 `allow` 对黑名单 `Deny` 无效（C-21）。

### 5.7 Storage / AuditSink

- **定义位置**：`crates/minicoding-core/src/storage/trait.rs`；
- **实现位置**：`crates/minicoding-storage`（`JsonlStorage` + `AuditSink`）；
- **设计意图**：会话日志与审计分离，JSONL 追加写崩溃安全；
- **`Storage::append`**：每条消息 append + fsync；
- **`AuditSink::record`**：`audit.log` JSONL，0600 权限，追加写不可篡改历史。

### 5.8 Journal

- **定义位置**：`crates/minicoding-core/src/journal/trait_def.rs`；
- **实现位置**：`crates/minicoding-journal`（`FileChangeJournal`）；
- **设计意图**：会话内文件改动账本 + `/undo` operation 级回滚；
- **关键约束**（C-28）：不落盘（含文件原文，落盘等于多存一份敏感数据）、不强行覆盖（恢复前比对 `after`，冲突记 `failed_files`）、不越界恢复（路径仍经 `sandbox_path` 校验）、不回滚跨会话（引导用 Git）。

### 5.9 McpClient

- **定义位置**：`crates/minicoding-core/src/mcp/trait_def.rs`；
- **实现位置**：`crates/minicoding-mcp`（`RmcpClient` 基于 `rmcp` 2.2）；
- **设计意图**：连接外部 MCP server、list_tools、call、shutdown；亦提供 `serve --as-mcp-server` 暴露自身工具；
- **关键约束**：project 作用域 server 必须经首次批准（C-24，`mcp_choices.toml`）、凭证不下传子进程（C-04）、工具命名 `mcp__<server>__<tool>`；
- **进程池复用**：MCP server 连接跨 turn 复用，不每 turn 重启。

### 5.10 ProjectDocLoader / MemoryStore

- **定义位置**：`crates/minicoding-core/src/memory/trait.rs`；
- **实现位置**：`crates/minicoding-memory`；
- **设计意图**：`AGENTS.md` 分层加载 + 长期/Auto/会话记忆；
- **关键约束**：`auto.md` 与 `long_term.md` 物理隔离（C-27），`auto.md` 含指令性内容降级 `Ask`；`AGENTS.md` 不可被 Agent 自主编辑（C-23）。

---

## 6. 数据流分析

### 6.1 用户输入 → LLM → 工具调用 → 结果回灌 的完整流程

完整时序见 `docs/architecture.md` §4.2。文字版：

```
1. 用户输入（CLI 参数 / TUI 输入框 / SDK 调用）
   ↓
2. Frontend 构造 UserInput { text, attachments, context_hint }
   ↓
3. Runtime::run_turn(user_input)
   ├─ 生成 Message::user(...)
   ├─ storage.append(session.id, &user_msg)  ← 先写盘
   ├─ ctx.append(user_msg.clone())          ← 再入上下文
   └─ emit(MessageAppended(user_msg))       ← 再广播
   ↓
4. ctx.build_chat_request(&tools, &config)
   ├─ 注入 system prompt（Identity/Capabilities/Hard Rules/Soft Rules/Security/Project Doc/Memory）
   ├─ 注入工具 schemas（仅 enabled_groups）
   ├─ 注入压缩后的历史消息（若 token_count > budget*0.85 触发压缩管道）
   └─ 输出 ChatRequest
   ↓
5. provider.chat_stream(req) → BoxStream<Result<Delta>>
   ├─ Delta::Text(s) → emit(Token(s)) + acc.push_text(s)
   ├─ Delta::ToolCall(tc_delta) → acc.push_tool_call(tc_delta)
   └─ Delta::Usage(u) → acc.usage = Some(u)
   ↓
6. assistant_msg = acc.finalize()
   ├─ storage.append(session.id, &assistant_msg)
   ├─ ctx.append(assistant_msg.clone())
   └─ emit(MessageAppended(assistant_msg))
   ↓
7. 判止：assistant_msg.tool_calls.is_empty()?
   ├─ 是 → emit(TurnEnd { stop_reason: EndTurn }) → return Finished
   └─ 否 → 进入工具执行（见 §6.3）
   ↓
8. execute_tool_calls(&assistant_msg.tool_calls)
   ├─ 分桶：readonly 并行 / side_effect 串行
   └─ 每个调用：policy.check → prompter.prompt → tools.dispatch → audit
   ↓
9. 落盘 tool_result
   ├─ for r in results: storage.append(...) + ctx.append(...) + emit(MessageAppended)
   └─ 回到步骤 4（让 LLM 基于工具结果继续）
```

### 6.2 流式响应处理

- `LlmProvider::chat_stream` 返回 `BoxStream<'static, Result<Delta, LlmError>>`；
- `DeltaAccumulator`（`crates/minicoding-core/src/runtime/accumulator.rs`）聚合三种 delta：
  - `Delta::Text(s)` → 累积文本，实时 `emit(Event::Token(s))` 供 frontend 渲染；
  - `Delta::ToolCall(ToolCallDelta { index, id, name, args_chunk })` → 按 index 累积增量 JSON 片段，流结束后拼装成完整 `ToolCall`；
  - `Delta::Usage(u)` → 记录 token 用量；
- `acc.finalize()` 输出完整 `Message`，含 `tool_calls: Vec<ToolCall>`。

**输出契约**（C-12）：工具调用增量 JSON 必须可拼接成合法 JSON；解析失败时 Runtime 容错为 `{ "_raw": "..." }` 并标记 warning，不崩溃。`stop_reason` 由 Runtime 独立判定，不盲信 LLM 自报。

### 6.3 并行/串行工具调度

详见 `docs/design.md` §2.3。简化伪代码：

```rust
async fn execute_tool_calls(&self, calls: &[ToolCall]) -> Result<Vec<(ToolCallId, ToolResult)>> {
    // 1. 分桶：无副作用 → 并行；有副作用 → 串行
    let (readonly, side_effect): (Vec<_>, Vec<_>) = calls.iter()
        .partition(|c| self.tools.get(&c.name).map(|t| t.side_effect() == SideEffect::None).unwrap_or(true));

    // 2. 无副作用：并发执行（buffer_unordered(8)）
    let ro_futs = readonly.iter().map(|call| self.run_one(call));
    let mut ro_stream = futures::stream::iter(ro_futs).buffer_unordered(8);
    while let Some(r) = ro_stream.next().await { results.push(r?); }

    // 3. 有副作用：严格串行，逐个完成后再启动下一个
    for call in side_effect { results.push(self.run_one(&call).await?); }

    // 4. 按 LLM 原始顺序回填
    results.sort_by_key(|(id, _)| calls.iter().position(|c| c.id == *id).unwrap_or(usize::MAX));
    Ok(results)
}
```

**理由**：副作用间往往存在隐式依赖（先 `fs.write` 再 `shell.run cargo build`），并行会导致竞态、重复授权、审计顺序混乱、回滚不可追溯。LLM 若显式需要并行写入，应拆成多轮（每轮一个写），由模型自行决策。

### 6.4 权限检查集成点

权限检查嵌入 `Runtime::run_one`（见 §4.4、`docs/design.md` §2.3）：

```
policy.check(tool, input, pctx) → Verdict
   ├─ Allow                         → 直接执行
   ├─ Deny(reason)                  → 返回 ToolResult::error，不执行
   └─ Ask(prompt)                   → emit(PermissionRequested) + prompter.prompt(prompt) → Decision
                                      ├─ Allow → 执行
                                      └─ Deny  → 返回 error
```

**审计落盘**：无论 Allow/Deny 都调 `audit(call, decision, result)`，写 `audit.log`（C-05 审计落盘）。

**Hook 集成**：`PreToolUse` Hook 在 `policy.check` 之后、工具执行之前运行，可改写 input 或把 `Ask` 升 `Allow`/`Deny`，但**不可**把内置黑名单的 `Deny` 改为 `Allow`（C-21）。

### 6.5 事件广播时机

`emit` 全程异步广播事件供 frontend 渲染与 OTel 记录。关键时机：

| 时机 | 事件 |
|------|------|
| 用户消息落盘后 | `MessageAppended(user_msg)` |
| 流式开始 | `TurnStreamingStarted` |
| 每个文本 delta | `Token(s)` |
| assistant 消息落盘后 | `MessageAppended(assistant_msg)` |
| 权限请求前 | `PermissionRequested { id, tool, summary, risk }` |
| 权限决策后 | `PermissionResolved { id, decision }` |
| 工具执行前 | `ToolCallStart(call)` |
| 工具执行后 | `ToolCallEnd { id, ok, elapsed }` |
| tool_result 落盘后 | `MessageAppended(tool_result_msg)` |
| 轮次结束 | `TurnEnd { stop_reason }` |
| 任务状态变更 | `TaskUpdated` |
| Hook 执行 | `HookRun` |
| `/undo` 完成 | `FileUndone` |
| 配置热更新 | `ConfigChanged` |

---

## 7. 设计模式与最佳实践

### 7.1 trait 对象 + `Arc<dyn Trait>` 运行时装配

**模式**：所有领域 trait 在 core 定义，Runtime 持有 `Arc<dyn Trait>`，实现可来自任意 crate。

**为什么**：Runtime 编排不感知具体实现 crate，依赖方向干净（core 不依赖领域 crate）。frontend 可按需注入不同实现（如测试时注入 `MockProvider`）。

**示例**：

```rust
pub struct Runtime {
    provider: Arc<dyn LlmProvider>,
    context: Arc<dyn ContextManager>,
    policy: Arc<dyn PermissionPolicy>,
    prompter: Arc<dyn PermissionPrompter>,
    // ...
}
```

### 7.2 `trait-variant` 生成 Send 变体

**问题**：原生 `async fn in trait` 默认非 dyn-compatible，无法作 `Arc<dyn Trait>`。

**解决**：用 `#[trait_variant::make(Trait: Send)]` 宏为每个含 `async fn` 的 trait 生成 Send 变体（返回 `Pin<Box<dyn Future + Send>>`），既保留原生 async 语法、又支持 trait object。

**示例**：

```rust
#[trait_variant::make(Tool: Send)]
pub trait Tool {
    fn name(&self) -> &str;
    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext)
        -> Result<ToolResult, ToolError>;
}
// 现在可以用 Arc<dyn Tool>
```

**不引入 `async-trait`**（已废弃路径），见 `AGENTS.md` §2.1。

### 7.3 BoxFuture 异步 trait

对于需要 `dyn` 兼容但不能用 `trait_variant` 的场景（如 `SubagentRunner`），用 `BoxFuture` 显式表达：

```rust
pub trait SubagentRunner: Send + Sync {
    fn spawn(&self, spec: SubagentSpec, input: String)
        -> BoxFuture<'_, Result<SubagentResult, RuntimeError>>;
}
```

详见 `docs/api.md` §2.5。

### 7.4 零实现 core 模式

**模式**：`minicoding-core` 只含抽象 + 编排，不含任何领域实现逻辑。

**禁止**在 core 出现：压缩算法、黑名单正则、landlock ruleset、rmcp 调用、JSONL 写入、HTTP 客户端、Hook 子进程协议解析等任何领域实现。

**为什么**：core 编译快、测试快、依赖轻量（仅 `tokio`/`serde`/`tracing`/`thiserror`/`uuid`/`time`/`camino`/`trait-variant`），无 `reqwest`/`landlock`/`rmcp`/`libseccomp` 等重依赖。详见 `AGENTS.md` §3.4。

### 7.5 feature gate 隔离重依赖

**模式**：可选能力（memory/hooks/journal/sandbox/mcp）通过 cargo feature 按需启用，避免强制引入重依赖。

**示例**（`crates/minicoding-cli/Cargo.toml`）：

```toml
[features]
default = ["memory", "sandbox"]
memory  = ["dep:minicoding-memory"]
hooks   = ["dep:minicoding-hooks"]
file-undo = ["dep:minicoding-journal"]
sandbox = ["dep:minicoding-sandbox"]
mcp     = ["dep:minicoding-mcp"]
full    = ["memory", "hooks", "file-undo", "sandbox", "mcp"]
```

未启用时由 core 的 `NoopDriver`/`NoopEventStore`/`NoopAudit` 等兜底实现零开销。

### 7.6 target cfg 平台隔离

**模式**：平台相关重依赖通过 `[target.'cfg(...)'.dependencies]` 条件引入，非目标平台不编译。

**示例**（`crates/minicoding-sandbox/Cargo.toml`）：

```toml
[target.'cfg(target_os = "linux")'.dependencies]
landlock = "0.4.5"
libseccomp = "0.x"

[target.'cfg(target_os = "windows")'.dependencies]
windows = { version = "...", features = ["..."] }
```

详见 `AGENTS.md` §3.5。

### 7.7 NoopDriver 兜底实现

**模式**：core 为 `SandboxDriver` 提供 `NoopDriver` 兜底实现，未启用 `minicoding-sandbox` feature 时使用。

**为什么**：保证 Runtime 始终能持有 `Arc<dyn SandboxDriver>`，feature 关闭时零开销降级，无需在调用方写 `Option` 分支。其他 trait（`EventStore`/`SnapshotStore`/`AuditSink`/`SubagentRunner`/`ExtensionHost`）也有对应 Noop 实现。

---

## 8. Rust 编程要点

### 8.1 edition 2024 特性使用

- `edition = "2024"`，`rust-version = "1.99"`（见 `Cargo.toml` `[workspace.package]`）；
- **`async fn in trait` 已稳定**：直接用，不需 `async-trait`（见 `AGENTS.md` §2.1）；
- trait 需作 `dyn` 对象时用 `#[trait_variant::make(Trait: Send)]` 生成 Send 变体。

### 8.2 async fn in trait

**直接用**：

```rust
pub trait ContextManager: Send + Sync {
    async fn append(&self, msg: Message);
    async fn build_chat_request(&self, tools: &ToolRegistry, config: &RuntimeConfig) -> Result<ChatRequest>;
}
```

**需 dyn 对象时**：加 `#[trait_variant::make(ContextManager: Send)]` 宏。

### 8.3 thiserror + anyhow 错误处理

- **库 crate**（core 及各领域 crate）用 `thiserror` 定义具体错误类型，实现 `Into<RuntimeError>`：
  ```rust
  #[derive(Debug, thiserror::Error)]
  pub enum ToolError {
      #[error("invalid input: {0}")]
      InvalidInput(String),
      #[error("path escaped workdir: {0}")]
      PathEscaped(String),
  }
  ```
- **边界 crate**（`minicoding-cli`/`minicoding-sdk`）用 `anyhow::Result` 聚合并格式化输出；
- **不 panic**：除真正不可恢复的程序员 bug（如 `unreachable!` 标记的不变式被破坏）。所有可预期错误走 `Result`；
- **不用 `unwrap()`/`expect()`** 在非测试代码中（除非有 SAFETY/不变式注释证明不会 panic）。详见 `AGENTS.md` §2.3。

### 8.4 camino::Utf8PathBuf 路径处理

- 路径用 `camino::Utf8PathBuf` 替代 `std::path::PathBuf`（UTF-8 保证，避免 OS 字符集边界）；
- 结构体字段用 `String` 而非 `&str`（避免不必要生命周期）；
- 详见 `AGENTS.md` §2.5。

### 8.5 tokio 并发原语

- 统一 `tokio` runtime（`#[tokio::main]`/`#[tokio::test]`），不混用 `async-std`；
- **不裸用** `std::thread`（除 FFI 阻塞调用包裹线程，需注释说明）；
- 流式响应用 `BoxStream<Result<Delta>>` / `impl Stream`；
- 并发原语用 `tokio::sync`（`mpsc`/`broadcast`/`RwLock`/`Mutex`）；
- 详见 `AGENTS.md` §2.4。

### 8.6 serde 序列化

- 序列化用 `serde` + `serde_json` + `toml`；
- 配置文件用 `toml` + `serde`（与 `Cargo.toml` 同源）；
- JSONL 记录前向兼容：读取时忽略未知字段（`#[serde(default)]` + `serde_json::Value` 兜底）；
- `v` 字段用于 schema 迁移：`migrate(v_from, v_to, record)` 链式升级。详见 `docs/data-model.md` §2.4。

### 8.7 tracing 日志

- 日志/追踪用 `tracing` + OpenTelemetry（OTel 一等公民，从 M0 接入）；
- 业务代码只写 `tracing` 宏，subscriber 层同时输出本地文件日志（`tracing-appender`）与 OTLP trace（`tracing-opentelemetry` 桥接），无重复埋点；
- span 层级：`session > turn > (context.build | llm.chat_stream > retry | tool.call > (permission.check | permission.prompt | tool.dispatch))`；
- 后端地址由 `OTEL_EXPORTER_OTLP_ENDPOINT` 控制，零代码改动即可切换；
- 详见 `docs/tech-stack.md` §7、`docs/design.md` §15。

---

## 9. 测试策略

### 9.1 单元测试组织（同文件 `#[cfg(test)]`）

- 单元测试与源码同文件：`#[cfg(test)] mod tests { ... }`；
- 不另建 `tests/` 子目录放单元测试；
- 详见 `AGENTS.md` §2.8。

### 9.2 集成测试（`tests/` 目录）

- 集成测试放 `tests/` 目录，按场景命名（`agent_loop.rs`/`compression.rs`/`sandbox.rs`）；
- 跨 crate 共享测试工具放 `crates/minicoding-core/tests/common/`。

### 9.3 异步测试（`#[tokio::test]`）

- 异步测试用 `#[tokio::test]`；
- 不用 `block_on` 手动驱动。

### 9.4 HTTP mock（wiremock）

- HTTP mock 用 `wiremock`/`httpmock`；
- **不连真实 OpenAI/Anthropic**（C-04 测试不连真实服务）；
- MCP server 测试用本地 mock stdio process；
- 沙箱测试用 `tempfile` 临时目录，不碰真实用户文件；
- 详见 `AGENTS.md` §5.4。

### 9.5 覆盖率目标（≥80%）

- 覆盖率目标 ≥80%（`cargo-llvm-cov`）；
- 快照测试用 `insta`（配置 schema、CLI 输出）；
- 属性测试用 `proptest`；
- 基准测试用 `criterion`；
- CLI 端到端测试用 `assert_cmd`；
- 详见 `docs/tech-stack.md` §10、`AGENTS.md` §2.8。

---

## 10. 调试与排查

### 10.1 日志查看

- 本地文件日志由 `tracing-appender` 滚动写入 `~/.minicoding/logs/`；
- 日志级别由 `RUST_LOG` 环境变量控制（如 `RUST_LOG=minicoding_core=debug,minicoding_tools=trace`）；
- 会话 JSONL 在 `~/.minicoding/sessions/{session_id}.jsonl`，可人读回放；
- 审计日志在 `~/.minicoding/audit.log`（JSONL，0600 权限）。

### 10.2 OTel trace 分析

- OTel 为一等公民，所有跨组件边界打 span（见 `docs/architecture.md` §7.3）；
- 通过 `tracing-opentelemetry` 桥接为 OTLP，导出到 Jaeger/Tempo/Grafana；
- 后端地址由 `OTEL_EXPORTER_OTLP_ENDPOINT` 环境变量配置；
- 每个工具调用记录 OTel 属性：工具名、`side_effect`、是否并行、耗时、结果大小、是否截断、权限 verdict；
- 会话 JSONL 可作为回放源（`--replay <session.jsonl>`），与 trace 通过 `session.id` + `turn.index` 关联。

**本地起 Jaeger 快速排查**：

```bash
docker run -d -p 4317:4317 -p 16686:16686 jaegertracing/all-in-one
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317
cargo run -p minicoding-cli -- "your prompt"
# 浏览器打开 http://localhost:16686 查看 trace
```

### 10.3 常见 bug 排查思路

| 现象 | 排查思路 |
|------|---------|
| 工具调用被拒绝 | 查 `audit.log` 该调用的 `verdict` 与 `decision`，确认是 L0 黑名单还是 L1 用户策略 |
| 压缩熔断 | 查 OTel `compress.circuit_breaker` span event 的 `fail_count`/`thrash_count`；检查 tool_result 是否过大 |
| 沙箱拒绝 | 查 `SandboxDriver` 返回的 `EPERM`/Seatbelt denial；检查 `SandboxPolicy` 配置；连续 ≥3 次会触发熔断（C-30） |
| 上下文超限 | 查 `ctx.token_count` 与 `budget`；检查是否触发预测性/反应式 compact；检查 `post_compact_token_budget` |
| 流式输出中断 | 查 `stop_reason`（`MaxTokens`/`Interrupted`）；查 provider 重试日志（429 限流） |
| Hook 不生效 | 查 Hook 是否在 `mcp.json`/`config.toml` 正确注册；查 `on_hook_error` 策略（默认 `continue` + warn） |
| MCP server 连接失败 | 查 project 作用域批准状态（`mcp_choices.toml`）；查 `required` 语义（`true` 启动失败则 minicoding 拒绝启动） |
| `--replay` 不执行副作用 | 这是预期行为（C-06），replay 模式默认禁用所有副作用工具 |

---

## 11. 扩展开发指南

### 11.1 添加新工具

**步骤**（参考 `crates/minicoding-tools/src/fs/read.rs`）：

1. 在 `crates/minicoding-tools/src/<group>/` 下新建文件，实现 `Tool` trait；
2. 如实标注 `side_effect()`（误标会绕过串行约束，C-11）；
3. 在 `lib.rs` 的 `register_all()` 工厂中注册；
4. 公共 API 加 doc comment（`///`）；
5. 同步更新 `docs/api.md` §3.3、`docs/design.md` §4.3。

**示例骨架**：

```rust
use minicoding_core::prelude::*;

pub struct MyTool { schema: ToolSchema }

impl MyTool {
    pub fn new() -> Self {
        Self { schema: ToolSchema { /* ... */ } }
    }
}

#[trait_variant::make(Tool: Send)]
impl Tool for MyTool {
    fn name(&self) -> &str { "my.tool" }
    fn schema(&self) -> &ToolSchema { &self.schema }
    fn side_effect(&self) -> SideEffect { SideEffect::None }  // 如实标注
    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext)
        -> Result<ToolResult, ToolError> {
        // 所有路径经 sandbox_path 校验
        // 监听 ctx.canceller，及时中止
        // 输出超过 ctx.max_output_bytes 截断并标注
        Ok(ToolResult { /* ... */ })
    }
}
```

### 11.2 添加新 Provider

**步骤**（参考 `crates/minicoding-providers/src/openai/`）：

1. 在 `crates/minicoding-providers/src/<provider>/` 下新建模块；
2. 实现 `LlmProvider` trait + 对应 `Tokenizer`；
3. 内部统一返回 `BoxStream<Result<Delta>>`，转换逻辑隔离；
4. 密钥从环境变量或 OS keyring 读，**绝不**接受配置文件明文（C-04）；
5. 重试与超时复用 `common::retry`；
6. 在 `lib.rs` 的 `build_provider()` 工厂中注册；
7. 同步更新 `docs/api.md` §3.1、`docs/tech-stack.md` §3。

### 11.3 添加新沙箱驱动

**步骤**（参考 `crates/minicoding-sandbox/src/`）：

1. 在 `crates/minicoding-sandbox/src/<platform>.rs` 下实现 `SandboxDriver` trait；
2. 平台相关依赖通过 `[target.'cfg(...)'.dependencies]` 条件引入；
3. 在 `lib.rs` 的 `detect_driver()` 工厂中按 `cfg!(target_os)` 选实现；
4. `apply` 必须在子进程 fork 后 exec 前调用，无窗口期；
5. 必须用 `// SAFETY: ...` 注释说明 FFI 不变式（`AGENTS.md` §2.6）；
6. 同步更新 `docs/security.md` §8、`docs/api.md` §3.9。

**优先用主流库**（`sandbox-run`/`landlock`/`libseccomp`），**不自研** ruleset/profile 胶水（`AGENTS.md` §3.6）。

### 11.4 编写 Hook 脚本

**外部脚本协议**（JSON over stdio，见 `docs/hooks.md` §3）：

```bash
#!/usr/bin/env python3
# .minicoding/hooks/fmt_on_write.py
import json, sys, subprocess

hook_input = json.loads(sys.stdin.read())
if hook_input["event"] == "PostToolUse" and hook_input["tool"]["name"] == "fs.write":
    path = hook_input["tool"]["input"]["path"]
    if path.endswith(".rs"):
        subprocess.run(["rustfmt", path], check=False)
    print(json.dumps({"decision": "continue", "exit_message": "rustfmt applied"}))
else:
    print(json.dumps({"decision": "continue"}))
```

**注册**（在 `~/.minicoding/config.toml` 或 `.minicoding.toml`）：

```toml
[[hooks]]
event = "PostToolUse"
command = "python3 .minicoding/hooks/fmt_on_write.py"
timeout_sec = 30
```

**关键约束**：

- Hook 的 `allow` 不可覆盖内置黑名单 `Deny`（C-21）；
- `asyncRewake` 仅 `PostToolUse`/`PostToolUseFailure`/`Stop` 有效（C-32）；
- 后台 Hook 子进程遵守凭证隔离（C-04）、沙箱（C-22）、路径沙箱（C-03）约束（C-26）。

---

## 12. 学习资源

### 12.1 推荐阅读文档顺序

```
入门：docs/getting-started.md          （30 分钟跑通）
全景：本文 §2 + AGENTS.md §1            （建立心智模型）
架构：docs/architecture.md §1-§6        （四层架构 + 数据流）
核心：docs/design.md §1-§4              （Agent 循环 + 工具 + 上下文）
trait：docs/api.md §1-§3                （L1 trait 抽象）
模块：docs/modules.md §0-§11            （18 crate 详解）
权限：docs/security.md §1-§2、§8        （权限模型 + 沙箱）
约束：docs/rules.md §1-§2               （L0 硬约束 C-01..C-30）
Hook：docs/hooks.md §1-§5               （10 类事件 + 协议）
进阶：docs/design.md §5-§25             （Subagent/Plan/Memory/Journal/MCP/Event Sourcing）
```

### 12.2 外部学习资源

| 主题 | 资源 |
|------|------|
| Rust async | The Async Book（https://rust-lang.github.io/async-book/） |
| tokio | tokio Tutorial（https://tokio.rs/tokio/tutorial） |
| trait object & dyn | Rust Reference §Traits（https://doc.rust-lang.org/reference/types.html#trait-objects） |
| trait-variant | crate 文档（https://docs.rs/trait-variant） |
| thiserror | crate 文档（https://docs.rs/thiserror） |
| tracing | crate 文档（https://docs.rs/tracing） |
| OpenTelemetry Rust | https://github.com/open-telemetry/opentelemetry-rust |
| rmcp（MCP SDK） | https://github.com/modelcontextprotocol/rust-sdk |
| landlock | https://github.com/landlock-lsm/rust-landlock |
| sandbox-run | crate 文档（https://docs.rs/sandbox-run） |
| camino | crate 文档（https://docs.rs/camino） |
| Tauri 2.x | https://tauri.app/ |

### 12.3 参考项目

| 项目 | 参考价值 |
|------|---------|
| Claude Code（Anthropic） | Agent 循环、工具系统、权限模型、Hook 生命周期、压缩熔断、Subagent、MultiEdit、TodoWrite 增量模型 |
| Codex CLI（OpenAI） | `ApprovalMode`/`Preset`、`SandboxPolicy` 四策略、`ExternalSandbox`、Lethal Trifecta 威胁模型、`/rewind` 启发 |
| ripgrep | `globset`/`ignore` 库的同源项目，`fs.grep`/`fs.glob` 行为参考 |

---

## 13. 术语表

| 术语 | 解释 |
|------|------|
| Agent 循环 | `prompt → LLM → tool_call → tool_result → LLM → ... → final` 的多轮编排，是项目核心机制 |
| Turn（轮次） | 一次 `run_turn` 调用，从用户输入到 `TurnEnd` |
| SideEffect | 工具副作用枚举：`None`/`FileWrite`/`Command`/`Network`，驱动权限路径与并行/串行调度 |
| Verdict | `PermissionPolicy::check` 的返回值：`Allow`/`Deny(reason)`/`Ask(prompt)` |
| Decision | `PermissionPrompter::prompt` 的返回值：`Allow`/`Deny`（终态，无 `Ask`） |
| L0 硬约束 | 不可违反的安全底线（C-01..C-07、C-21..C-24、C-26..C-30），由 Rust 代码强制 |
| L1 契约约束 | 工具调用/输出格式契约（C-08..C-13、C-25、C-31、C-32），由 Runtime 校验 |
| L2 软约束 | 行为规范（C-14..C-20、C-33..C-35），写入系统提示词引导 |
| EventBus | 广播式事件总线（`broadcast`），仅通知无回复 |
| PermissionPrompter | 点对点权限交互 trait，解决 broadcast 无法承载回复的缺陷 |
| ContextManager | 上下文管理 trait：消息历史 + token 预算 + 压缩 |
| 压缩熔断 | 防止 Thrash Loop 烧光 token 的状态机（C-29），失败计数 ≥3 熔断 |
| 沙箱拒绝熔断 | 防止 Agent 反复撞沙箱烧资源的熔断（C-30），拒绝 ≥3 次注入提醒 |
| asyncRewake | Hook 的异步唤醒模式，仅 `PostToolUse`/`PostToolUseFailure`/`Stop` 有效 |
| Auto memory | Agent 可写的自动学习记忆（`auto.md`），与手写 `long_term.md` 物理隔离（C-27） |
| FileChangeJournal | 会话内文件改动账本，支持 `/undo` operation 级回滚（C-28） |
| Plan 模式 | 通过 `PermissionMode::Plan` + `plan.exit` 工具实现双重只读强制（见 `docs/design.md` §16） |
| Subagent | 派生隔离子上下文执行子任务，结果汇总回主 Agent（仅 summary，C-05） |
| MCP | Model Context Protocol，基于 `rmcp` 2.2 接入外部工具 |
| Event Sourcing | 会话恢复机制：事件流持久化 + 周期性 snapshot + 重放恢复 |
| OTel | OpenTelemetry，项目一等公民，从 M0 接入 |
| trait-variant | 生成 Send 变体的宏，使 `async fn in trait` 可作 `dyn` 对象 |
| NoopDriver | core 提供的 `SandboxDriver` 兜底实现，未启用 feature 时零开销降级 |
| specificity | L1 用户策略的匹配优先级（0-5），同 specificity 下 deny 胜出 |
| ApprovalMode | 审批模式：`Untrusted`/`OnFailure`/`OnRequest`/`Never`，展开为 specificity=1 规则 |
| Preset | `approval_mode × sandbox_policy` 的实用组合：`ReadOnly`/`Auto`/`ExternalSandbox`/`FullAccess` |
| Lethal Trifecta | Prompt 注入的"致命三角"：私有数据访问 + 不可信内容暴露 + 外泄通道，移除任意一角即可使攻击崩塌 |
| AGENTS.md | 项目级 AI 辅助编码约束文件，约束"写代码的 AI 助手"（开发时） |
| rules.md | 运行时大模型约束文件，约束"被 minicoding 驱动的 LLM"（运行时） |
| `~/.minicoding/` | 项目持久化根目录（可用 `MINICODING_HOME` 覆盖） |
| JSONL | JSON Lines，每行一条 JSON 记录，追加写崩溃安全，会话日志格式 |

---

> 本文档到此结束。如需深入了解特定主题，请按 §12.1 推荐顺序阅读 `docs/` 下专题文档。如发现文档与代码不一致，以代码为准并提 issue 同步修订文档（`AGENTS.md` §4.1 改代码必改文档）。
