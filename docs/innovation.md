# 创新文档（Innovation Document）

> **文档性质**：本文是 `minicoding-rs` 的创新点分析文档，定性分析项目在架构、安全、上下文管理、Agent 循环、可观测性、扩展机制、会话管理、前端桌面、AI 辅助开发等维度的技术创新与设计亮点。本文不规定实现细节，实现权威以 `design.md`/`modules.md`/`api.md` 等文档为准。
>
> **与其它文档的关系**：本文是「分析性」文档，从「为什么创新、创新在哪、带来什么价值」的视角横向串联各设计文档。架构与运行时细节请参考 `docs/design.md`；模块职责边界见 `docs/modules.md`；技术选型见 `docs/tech-stack.md`；安全机制见 `docs/security.md`；运行时大模型约束见 `docs/rules.md`（C-01..C-35）；Hooks 系统见 `docs/hooks.md`；M9 前端设计见 `docs/m9-design.md`；功能清单见 `docs/features.md`。

---

## 1. 文档说明

### 1.1 目的与定位

`minicoding-rs` 是一个 Rust 实现的终端 AI Coding 助手（类 Claude Code / Codex CLI / Aider），目标是为「AI 编程助手」这一新兴品类提供一个**高性能、可嵌入、可扩展、安全可控**的智能体运行时。在实现过程中，项目并非简单复刻已有产品，而是在架构分层、安全模型、上下文管理、可观测性、扩展机制、AI 辅助开发流程等多个维度做了系统性的创新设计。

本文档旨在：

1. **系统梳理**项目的创新点，避免设计意图在迭代中流失；
2. **横向对比**同类产品（Claude Code / Codex CLI / Aider），说明差异化价值；
3. **为后续演进**提供决策参照——创新点既是资产也是约束，新增能力需评估对既有创新的影响。

### 1.2 适用读者

- 项目维护者：评估新需求与既有创新的关系；
- 贡献者：理解「为什么这么设计」，避免无意破坏创新约束；
- 外部评估者：快速识别项目的技术价值与差异化定位。

### 1.3 创新的判定标准

本文所称「创新」包含三类：

| 类型 | 含义 | 示例 |
|------|------|------|
| 原创创新 | 业内首次或罕见的设计 | L0/L1/L2 三层约束模型、决策与交互分离 |
| 集成创新 | 已有理念组合出新的工程价值 | 「零实现 core + 领域 crate」+「单向不循环依赖」 |
| 选型创新 | 在 AI Coding 助手领域引入非主流但更优的技术选型 | Tauri 替代 Electron、全 Rust 工具链、rmcp 官方 SDK |

---

## 2. 创新总览

### 2.1 创新点全景图

```
┌──────────────────────────────────────────────────────────────────────────┐
│                        minicoding-rs 创新全景                              │
├──────────────────────────────────────────────────────────────────────────┤
│                                                                          │
│  ┌─────────────┐   ┌─────────────┐   ┌─────────────┐   ┌─────────────┐   │
│  │ 架构创新     │   │ 安全创新     │   │ 上下文管理   │   │ Agent 循环  │   │
│  │             │   │             │   │   创新       │   │   创新      │   │
│  │ • 多crate    │   │ • L0硬约束   │   │ • 4级压缩    │   │ • 并行/串行  │   │
│  │ • 零实现core │   │ • 两道防线   │   │ • 压缩熔断   │   │ • 类型化子   │   │
│  │ • 单向依赖   │   │ • 决策/交互  │   │ • 预测性压缩 │   │   Agent     │   │
│  │ • NoopDriver│   │   分离      │   │ • Post-compact│   │ • worktree  │   │
│  │             │   │ • 黑名单最高 │   │ • 独立小LLM  │   │ • Plan双重   │   │
│  └─────────────┘   └─────────────┘   └─────────────┘   └─────────────┘   │
│                                                                          │
│  ┌─────────────┐   ┌─────────────┐   ┌─────────────┐   ┌─────────────┐   │
│  │ 可观测性     │   │ 扩展机制     │   │ 会话管理     │   │ 前端与桌面   │   │
│  │   创新       │   │   创新       │   │   创新       │   │   创新      │   │
│  │ • OTel一等   │   │ • 10类Hook   │   │ • Event     │   │ • 全Rust    │   │
│  │   公民       │   │   +asyncWake │   │   Sourcing  │   │   工具链     │   │
│  │ • 全链路span │   │ • MCP双向    │   │ • Parent-UUID│   │ • Tauri替代 │   │
│  │ • 双输出     │   │ • project批准│   │ • 64KB窗口  │   │   Electron  │   │
│  │             │   │ • Extension  │   │ • /undo     │   │ • ts-rs生成 │   │
│  │             │   │   SDK        │   │             │   │ • SSE增量   │   │
│  └─────────────┘   └─────────────┘   └─────────────┘   └─────────────┘   │
│                                                                          │
│  ┌────────────────────────────────────────────────────────────────────┐  │
│  │                    AI 辅助开发创新                                  │  │
│  │  • AGENTS.md 约束体系（开发时 vs 运行时双约束）                       │  │
│  │  • rules.md L0/L1/L2 分层约束模型                                   │  │
│  │  • AI 助手行为约束（先读后改、不臆造 API、不绕过约束）                  │  │
│  └────────────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────────────────┘
```

### 2.2 创新点分类

| 维度 | 核心创新 | 创新类型 | 对应文档 |
|------|---------|---------|---------|
| 架构 | 多 crate workspace + 零实现 core | 集成创新 | `architecture.md`、`modules.md` |
| 安全 | L0 硬约束 + 两道防线 | 原创创新 | `security.md`、`rules.md` |
| 上下文 | 4 级压缩管道 + 熔断 + 预测性压缩 | 集成创新 | `design.md` §3 |
| Agent 循环 | 并行/串行分桶 + 类型化子 Agent | 集成创新 | `design.md` §2、§7 |
| 可观测性 | OTel 一等公民 + 全链路 span | 选型创新 | `design.md` §15 |
| 扩展 | 10 类 Hook + asyncRewake + MCP 双向 | 集成创新 | `hooks.md`、`design.md` §19 |
| 会话 | Event Sourcing + Parent-UUID 链 | 集成创新 | `design.md` §10、§25 |
| 前端桌面 | 全 Rust 工具链 + Tauri + ts-rs | 选型创新 | `m9-design.md` |
| AI 辅助开发 | L0/L1/L2 约束 + AGENTS.md 双约束 | 原创创新 | `rules.md`、`AGENTS.md` |

### 2.3 创新的核心主线

项目的创新并非散点，而是围绕一条主线展开：**「不信任 LLM 输出，约束执行权永远在 Rust Runtime 一侧」**（见 `rules.md` 设计哲学）。这条主线串联起安全（L0 硬约束、两道防线）、上下文（压缩熔断不被 LLM 绕过）、Agent 循环（权限门在工具执行前强制闭合）、可观测性（OTel span 独立于 LLM 声明）等多个维度。所有创新都服务于一个目标：**让 AI Coding 助手在保持高效的同时具备生产级的安全性与可控性**。

---

## 3. 架构创新

### 3.1 多 crate 可嵌入 workspace 设计（vs 单体 CLI）

#### 3.1.1 创新点

`minicoding-rs` 采用 Cargo Workspace 多 crate 拆分（M0–M8 范围 17 crate，M9 新增 `minicoding-desktop` 与 `minicoding-web` 后达 19 crate），而非 Claude Code / Aider 那样的单体 CLI 包。crate 划分按「单一职责」原则，每个 crate 只负责一类实现（见 `AGENTS.md` §3.1）：

```
minicoding-rs (workspace)
└── crates/
    ├── minicoding-core          # 抽象层：trait 定义 + Runtime 编排（零实现）
    ├── minicoding-context       # ContextManager 实现 + 4 级压缩 + 熔断
    ├── minicoding-policy        # 权限实现 + builtin 黑名单 + Prompter
    ├── minicoding-memory        # 长期/Auto/会话记忆 + AGENTS.md loader
    ├── minicoding-hooks         # HookRegistry + ScriptHook + asyncRewake
    ├── minicoding-journal       # FileChangeJournal + /undo
    ├── minicoding-sandbox       # OS 沙箱驱动（自研 pre_exec 胶水 + landlock 直连，seccomp 待接入）
    ├── minicoding-mcp           # MCP client/server（rmcp 2.2）+ 进程池
    ├── minicoding-storage       # JSONL 存储 + audit.log + EventStore
    ├── minicoding-providers     # LLM Provider（OpenAI/Anthropic/Ollama）+ 小 LLM
    ├── minicoding-tools         # 内置 Tool 实现（组合层）
    ├── minicoding-protocol      # JSON-RPC 2.0 wire types + Event/Command DTO
    ├── minicoding-server        # HTTP/SSE server + ACP/LSP 适配器
    ├── minicoding-extension-sdk # 扩展作者稳定 API（Extension trait + Registrar）
    ├── minicoding-cli           # CLI frontend
    ├── minicoding-tui           # TUI frontend
    ├── minicoding-sdk           # 嵌入 SDK
    ├── minicoding-desktop       # Tauri 2.x 桌面壳（M9）
    └── minicoding-web           # Web 前端（React 19 + Vite，M9，独立 npm 项目）
```

#### 3.1.2 为什么创新

同类产品多为单体包：

- **Claude Code**：闭源单体 CLI，能力无法作为库被嵌入；
- **Aider**：Python 单体包，作为库嵌入需要拖入整个依赖树；
- **Codex CLI**：Rust 但偏向单一 CLI 形态。

`minicoding-rs` 把「Agent 运行时」从「CLI 入口」中剥离出来，使核心运行时（`minicoding-core` + 领域 crate）可作为 library crate 被任何 Rust 程序依赖（`minicoding-sdk` 即为此设计），同时支撑 CLI、TUI、Web、桌面**四形态共享同一后端**（见 §10.4）。这种「可嵌入运行时」定位在 AI Coding 助手品类中是稀缺的。

#### 3.1.3 带来的价值

| 价值 | 说明 |
|------|------|
| 多形态共享后端 | CLI/TUI/Web/桌面共用 Rust 后端，避免逻辑重复实现 |
| 可嵌入 | 第三方 Rust 程序可通过 `minicoding-sdk` 嵌入 Agent 能力 |
| 增量编译 | 改单 crate 只重编该 crate + 依赖链，CI 反馈快 |
| 可替换实现 | trait + 注册表注入，Provider/Tool/Storage 等均可替换 |
| 强制单一职责 | crate 边界即职责边界，AGENTS.md §3.1 用规范约束跨界 |

### 3.2 零实现 core 模式（trait 集中 + 领域隔离）

#### 3.2.1 创新点

`minicoding-core` 严格保持「零实现」：只含数据模型、trait 定义、Runtime 编排、事件总线、配置、OTel 初始化、路径约定、`NoopDriver` 兜底；**禁止**含任何领域算法（压缩算法、黑名单正则、landlock ruleset、rmcp 调用、JSONL 写入、HTTP 客户端、Hook 协议解析等）。所有领域实现在独立 crate（见 `AGENTS.md` §3.4、`architecture.md` §3.3）。

```
minicoding-core（零实现）
├── 数据模型：Message / Role / ToolCall / ToolResult / Session / Task
├── trait 定义：Tool / LlmProvider / ContextManager / PermissionPolicy /
│               SandboxDriver / Hook / Storage / Journal / McpClient /
│               ProjectDocLoader / MemoryStore
├── Runtime 聚合根 + Agent 循环（编排各 trait，本身不含领域算法）
├── 事件总线（Event / EventBus，仅通知无回复）
├── 配置（RuntimeConfig 分层加载）
├── OTel 初始化与 span 辅助
├── 路径约定（paths.rs）
└── NoopDriver（SandboxDriver 兜底实现）
```

#### 3.2.2 为什么创新

业界常见两种极端：

1. **「上帝 core」**：核心包承载所有默认实现，导致 core 体积膨胀、依赖污染、单一职责破坏；
2. **「无 core」**：各领域 crate 各自为政，缺乏统一抽象，Runtime 装配困难。

`minicoding-rs` 选择「抽象集中、实现分散」的中间路径——`architecture.md` 称之为 v2 架构变更：原 core 承载多职责违反单一职责，重构后精简为「抽象层 + Runtime 编排」。这与 Rust 生态的 `tower`（核心 trait + 中间件实现分离）理念一致，但在 AI Coding 助手品类中是首次系统化应用。

#### 3.2.3 带来的价值

| 价值 | 说明 |
|------|------|
| core 轻量 | core 依赖仅 `tokio`/`serde`/`tracing`/`thiserror`/`uuid`/`time`/`camino`/`trait-variant`，无平台/网络重依赖 |
| 领域隔离 | 压缩算法变更不影响权限，权限策略变更不影响 MCP，降低耦合风险 |
| 可测试性 | trait 定义在 core，可在 core 层用 mock 实现 trait 测试 Runtime 编排逻辑 |
| 可替换性 | Runtime 持有 `Arc<dyn Trait>`，运行时装配实现 crate，feature gate 按需启用 |

### 3.3 单向不循环依赖方向

#### 3.3.1 创新点

依赖方向严格单向、不循环（见 `AGENTS.md` §3.2）：

```
core  ◄──  领域 crate (context/policy/memory/hooks/journal/sandbox/mcp/storage/providers)
  ▲
  └──  tools (组合层)  ◄──  cli / tui / sdk / server (frontend)
```

- core 不依赖任何领域 crate；
- 领域 crate 依赖 core，**不互相依赖**；
- `minicoding-tools` 是唯一「组合层」，可依赖多个领域 crate 完成工具执行闭环；
- frontend 依赖 tools + core + 必要实现 crate；
- 领域 crate 之间如需协作，通过 core 的 trait 抽象解耦。

#### 3.3.2 为什么创新

AI Coding 助手的领域间天然存在协作需求（如工具执行需调权限、写文件需记 Journal、Hook 需读权限 verdict），简单实现容易导致领域 crate 互相依赖形成环。`minicoding-rs` 通过两条规则破环：

1. **trait 集中在 core**：领域 crate 协作时依赖 core 的 trait 抽象，而非具体实现 crate；
2. **唯一组合层**：`minicoding-tools` 是唯一允许依赖多个领域 crate 的 crate，工具执行闭环（权限 → 派发 → 审计 → Journal）在此完成。

这种依赖治理在 Rust workspace 项目中并不罕见，但在 AI Coding 助手品类中首次明确为架构约束（写入 AGENTS.md）。

#### 3.3.3 带来的价值

- **编译图清晰**：`cargo tree` 输出无循环，便于依赖审计；
- **改一处不影响无关 crate**：改 `minicoding-sandbox` 不重编 `minicoding-memory`；
- **强制解耦**：领域 crate 间协作必须经 core trait，避免「便捷直调」导致的耦合债。

### 3.4 NoopDriver 兜底模式（fail-open 降级）

#### 3.4.1 创新点

`SandboxDriver` trait 的兜底实现 `NoopDriver` 定义在 `minicoding-core`，当 OS 沙箱不可用（如 Windows 早期版本、不支持 Landlock 的旧内核）时，`detect_driver()` 返回 `NoopDriver` 并打 `warn` 日志，依赖容器自身隔离或应用层权限兜底（见 `tech-stack.md` §11、`security.md` §8.6）。

#### 3.4.2 为什么创新

OS 沙箱是平台强相关的（Linux Landlock、macOS Seatbelt、Windows Job Object 成熟度不一），简单实现容易在平台不支持时直接 panic 或拒绝启动。`NoopDriver` 模式让 Runtime 在任何平台都能启动，沙箱能力作为「尽力而为」的增强而非硬前提：

- `SandboxDriver::is_hardened()` 如实报告当前是否内核级隔离；
- `doctor --security` 命令输出降级状态，建议用户在 WSL2/容器内运行；
- C-22 显式确认由配置/请求层强制（预设确认 + `confirm_danger`），`is_hardened()` 状态记日志（见 `rules.md` §8 对照表）。

#### 3.4.3 带来的价值

| 价值 | 说明 |
|------|------|
| 跨平台可用 | Linux/macOS/Windows 均可启动，沙箱按平台能力分级 |
| 降级透明 | `is_hardened()` 让上层代码感知降级状态，不静默弱化安全 |
| 渐进交付 | Linux 先行（M0–M4），macOS 补齐（M5+），Windows 补齐（M6+），不阻塞 MVP |

---

## 4. 安全创新

### 4.1 L0 硬约束不可绕过（实现层强制 vs 依赖 LLM 自觉）

#### 4.1.1 创新点

`rules.md` 定义了 L0/L1/L2 三层约束模型，其中 L0 硬约束（C-01..C-07、C-21..C-24、C-26..C-30 共 16 条）由 Rust 代码在实现层强制，**不依赖 LLM 自觉或系统提示词**。`rules.md` §5 明确声明：「提示词不是安全边界，Rust 代码才是。」

| L0 约束 | 实现层强制要求 |
|---------|---------------|
| C-01 副作用必须经权限 | `SideEffect != None` 工具调用必须经 `PermissionPolicy::check` → `Prompter` 解析为 `Allow` 后才执行 |
| C-02 内置黑名单不可覆盖 | `policy::builtin` 黑名单优先级最高，任何用户配置与 Hook 都无法覆盖 |
| C-03 路径不可越界 | 所有文件工具输入经 `sandbox_path` 规范化校验，越界直接 `PathEscaped` 错误 |
| C-04 凭证不可外泄 | 凭证仅存内存与 OS keyring，不下传子进程 env；日志中密钥脱敏（前 4 字符 + `***`） |
| C-22 沙箱为第二道防线 | `DangerFullAccess`/`ExternalSandbox` 必须用户显式选定 + red 警告 + 二次确认 |
| C-29 压缩熔断不可被 LLM 绕过 | 熔断由 Runtime 状态机判定，与 LLM 输出无关 |
| C-30 沙箱拒绝熔断不可被 LLM 绕过 | 沙箱拒绝来自内核级硬反馈，不可被应用层 `allow` 覆盖 |

#### 4.1.2 为什么创新

业界 AI Coding 助手的安全模型多采用「系统提示词约束 + 工具白名单」的软约束模式，本质是依赖 LLM「听话」。但 LLM 是概率系统，可能被 prompt 注入诱导越权（`security.md` §1 威胁模型 T1）。`minicoding-rs` 把安全约束的执行权强制收归 Rust Runtime：

```
LLM 输出 ToolCall
   │
   ▼
Rust Runtime 校验（确定性代码）
   │
   ├─ 命中 L0 黑名单 → 直接 Deny，LLM 无法翻案
   ├─ 路径越界 → PathEscaped 错误
   ├─ 沙箱拒绝 → 内核级硬反馈，不可被 allow 覆盖（C-30）
   └─ 通过 → 执行
```

这种「不信任 LLM 输出」的设计哲学贯穿全项目，是 `rules.md` 的核心（见 `rules.md` 设计哲学）。Claude Code 与 Codex CLI 也有类似理念，但 `minicoding-rs` 把它系统化为 L0/L1/L2 三层模型并用 35 条编号约束（C-01..C-35）显式落地，是品类内首次。

#### 4.1.3 带来的价值

- **安全可审计**：L0 约束在实现层强制（见 `rules.md` §8 对照表——policy/sandbox/hooks 真实代码点 + CI 回归测试），非纸面声明；
- **抗 prompt 注入**：即使 LLM 被诱导输出「rm -rf /」，内置黑名单在实现层拒绝；
- **抗 LLM 幻觉**：LLM「声称已获授权」无效，Runtime 独立决策（C-01）；
- **抗熔断绕过**：压缩熔断与沙箱拒绝熔断由状态机判定，LLM 文本声明无法跳过（C-29/C-30）。

### 4.2 两道防线设计（应用层权限 + OS 级沙箱）

#### 4.2.1 创新点

`security.md` §8 把 OS 级沙箱升级为「一等公民」，作为应用层权限之外的第二道防线，两道防线独立：

```
工具调用
  │
  ▼
应用层：sandbox_path(§3) + PermissionPolicy(§2) + 内置黑名单   ← 第一道防线（精细、可交互）
  │
  ▼  通过
OS 层：SandboxPolicy（seatbelt/landlock/seccomp）              ← 第二道防线（粗粒度、内核强制）
  │
  ▼  通过
实际执行
```

OS 沙箱是 **opt-out 而非 opt-in**（参考 Codex）：`WorkspaceWrite` 是默认预设，启动即应用内核级限制；只有显式选择 `ExternalSandbox` 或 `DangerFullAccess`（red 警告 + 二次确认）才退出内核隔离。这避免「用户忘了开沙箱」导致裸奔。

#### 4.2.2 为什么创新

同类产品的沙箱策略：

- **Claude Code**：依赖应用层权限 + 容器（外层隔离），无内核级硬隔离；
- **Aider**：无沙箱，纯应用层权限；
- **Codex CLI**：Rust 实现 Landlock/Seatbelt 内核级隔离（`minicoding-rs` 主要参考）。

`minicoding-rs` 在 Codex 思路基础上做了三层创新：

1. **轻量自研胶水**：原选 ~~`sandbox-run`~~ 因 EUPL-1.2 许可证不合规弃用，改为自研 pre_exec 胶水（Linux `landlock` 直连 / macOS `sandbox_init` FFI / Windows Job Object），ruleset 构建仍复用官方 crate，避免手写 BPF（见 `tech-stack.md` §11、§13）；
2. **沙箱拒绝熔断器**（C-30）：单 turn 内累计沙箱拒绝 ≥3 次触发熔断，防 Agent 反复撞墙烧 token（`security.md` §8.8）；
3. **沙箱拒绝检测与升级流**（`security.md` §8.7）：denial 签名库把沙箱拒绝从普通错误中识别，升级为权限请求而非裸失败，用户可批准放宽重试。

#### 4.2.3 带来的价值

| 价值 | 说明 |
|------|------|
| 纵深防御 | 应用层误判或被绕过，OS 层仍兜底 |
| 默认安全 | `WorkspaceWrite` 默认启用内核隔离，无需用户配置 |
| 拒绝可观测 | denial 签名库识别沙箱拒绝，升级流让用户知情决策 |
| 拒绝可熔断 | 防止 Agent 在沙箱内陷入死循环 |

### 4.3 决策与交互分离（PermissionPolicy vs PermissionPrompter）

#### 4.3.1 创新点

权限采用双 trait 设计（见 `security.md` §2.1、`design.md` §9）：

- `PermissionPolicy::check(...) -> Verdict`：纯决策，返回 `Allow` / `Deny(reason)` / `Ask(prompt)`；
- `PermissionPrompter::prompt(prompt) -> Decision`：点对点交互，仅当 `Ask` 时被 Runtime 调用，返回终态 `Allow` / `Deny`。

`EventBus` 只广播 `PermissionRequested`/`PermissionResolved` 通知，**不承载回复**。

#### 4.3.2 为什么创新

把「请求-响应」语义塞进广播式 `EventBus` 是常见设计错误——`oneshot::Sender` 不可克隆，与 `broadcast` 不兼容。`minicoding-rs` 显式拆分决策与交互：

```
policy.check → Verdict::Ask(prompt)
   │
   ├─ emit Event::PermissionRequested（广播通知，可克隆，仅供 UI 展示/审计）
   │
   └─ prompter.prompt(prompt) → Decision（点对点，oneshot 通道）
         │
         └─ emit Event::PermissionResolved（广播通知）
```

这解决了「broadcast 无法承载点对点回复」的架构缺陷（`design.md` §9.1）。决策（`PermissionPolicy`）与交互（`Prompter`）分离后，可独立替换：

| Prompter 实现 | 适用场景 |
|---------------|---------|
| `InteractivePrompter` | CLI TTY 交互 |
| `NonInteractivePrompter` | 非 TTY（CI/容器），按 `non_tty_strategy` 处理 |
| `TuiPrompter` | TUI 全屏交互 |
| `CallbackPrompter` | SDK 用户闭包 |
| `LspPrompter` | LSP `window/showMessageRequest` 点对点 |

#### 4.3.3 带来的价值

- **架构正确性**：避免 `oneshot::Sender` 不可克隆与 `broadcast` 的不兼容；
- **多前端适配**：同一 Runtime 可接 CLI/TUI/Web/桌面/LSP，仅替换 Prompter；
- **决策可审计**：决策与交互分离，审计日志可独立记录两者。

### 4.4 内置黑名单优先级最高（不可被 Hook/用户配置覆盖）

#### 4.4.1 创新点

权限解析采用两层模型（`security.md` §2.3）：

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

内置黑名单由 `policy::builtin` 硬编码，确保即使用户误配 `--allow 'shell.run:*'` 也无法执行 `rm -rf /`。Hook 也无法覆盖 L0（C-21）：内置黑名单 `Deny` 在 Hook 之前生效，Hook 收到 `verdict=Deny(builtin)` 时其 `allow` 决策被 Runtime 忽略并记审计。

#### 4.4.2 为什么创新

同类产品多采用单层规则集，用户配置可覆盖一切（包括安全规则）。`minicoding-rs` 把「不可协商的安全底线」与「用户可配置的便利规则」分到 L0/L1 两层，L0 由 Rust 代码硬编码不可配置。这种「安全底线不可配置」的设计在 AI Coding 助手品类中是首次系统化（Claude Code 依赖 Hook 自觉，Codex 有类似但未明确分层）。

#### 4.4.3 带来的价值

- **抗误配**：用户 `--allow 'shell.run:*'` 仍无法执行危险命令；
- **抗 Hook 篡改**：恶意/错误 Hook 无法把黑名单 `Deny` 改 `Allow`；
- **抗 LLM 诱导**：LLM 无法通过任何方式绕过 L0。

### 4.5 AGENTS.md 不可被 Agent 自主编辑（C-23）

#### 4.5.1 创新点

C-23 约束：`AGENTS.md`/`AGENTS.override.md`/fallback 文件（`CLAUDE.md`/`.cursorrules`）是用户手写的静态指令层，Agent **不得**通过 `fs.write`/`fs.edit`/`shell.run`（如 `echo >> AGENTS.md`）自主修改任何层级。对这些文件的写操作默认 `Verdict::Ask` 且**不可通过 `AllowAlways` 持久化放行**（参考 Codex 约束）。

#### 4.5.2 为什么创新

AGENTS.md 是项目记忆指令层，承载工作约定、规范、禁区、架构说明。如果 Agent 能自主编辑 AGENTS.md，LLM 可通过改写指令层绕过其他软约束（如把「禁止 rm -rf」改成「允许 rm -rf」），形成提权通道。`minicoding-rs` 把 AGENTS.md 视为「不可信供应链制品」（见 `security.md` §9.2），即使在 exec 模式下也受 L0 黑名单约束。

同类产品中，Codex 最早提出此约束，`minicoding-rs` 沿用并强化为 C-23 L0 硬约束，启动自检（`rules.md` §8）确保 `ProjectDocLoader` 对 AGENTS.md 写操作注入 `Verdict::Ask`。

#### 4.5.3 带来的价值

- **防提权**：LLM 无法通过改指令层绕过约束；
- **供应链安全**：恶意仓库的 AGENTS.md 即使含恶意指令，在 exec 模式下仍受 L0 黑名单约束；
- **审计可追溯**：对 AGENTS.md 的写操作强制 Ask，用户知情决策。

### 4.6 凭证隔离（不下传子进程 env，日志脱敏）

#### 4.6.1 创新点

C-04 凭证不可外泄的多层防御（见 `security.md` §6、§10）：

1. **存储隔离**：API key 优先 OS keyring，文件 fallback 用 0600 权限 + 原子 rename；
2. **运行时隔离**：凭证仅存 Runtime 内存，不传给 `ToolContext.env`；
3. **子进程隔离**：`shell_environment_policy` 白名单（`include_only = ["PATH", "HOME", "USER", "LANG", "TERM"]`），`always_strip = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "MINICODING_*"]` 不可配置（C-04 强制）；MCP server 子进程与 Hook 子进程复用同一策略；
4. **日志脱敏**：日志中密钥只打前 4 字符 + `***`；`fs.read` 读取 `.env`/`credentials`/`*.pem`/`*.key` 等敏感文件时自动脱敏（`policy/redact.rs`）。

#### 4.6.2 为什么创新

LLM 可通过 `shell.run` 读取子进程环境变量外泄凭证（如 `env | grep API_KEY`），这是 AI Coding 助手的真实攻击面。`minicoding-rs` 把凭证隔离强化为 C-04 L0 硬约束，`shell_environment_policy` 是 C-04 的超集——除凭证外还可剥离任意变量（如 `CI_*`、`GITHUB_TOKEN`）。

#### 4.6.3 带来的价值

- **抗子进程读取**：`shell.run env | grep API_KEY` 拿不到凭证；
- **抗日志泄露**：trace 级别也只打脱敏后的密钥；
- **抗文件读取**：`fs.read .env` 返回脱敏后的内容（`***`），LLM 拿不到明文。

---

## 5. 上下文管理创新

### 5.1 4 级压缩管道（裁剪→摘要→滚动→硬截断）

#### 5.1.1 创新点

当 `ctx.tokens > budget * 0.85` 时触发 4 级压缩管道，逐级尝试（见 `design.md` §3.3）：

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

每条消息的权重 `w = base(role) * recency * sticky * manual_pin`（见 `design.md` §3.2），决定被压缩的优先级。

#### 5.1.2 为什么创新

同类产品的上下文压缩多为单一策略（如 Aider 的全量摘要、Claude Code 的滚动窗口）。`minicoding-rs` 把多种策略组合成分级管道：

- L1 裁剪是低成本的「先回收最易回收的」；
- L2 摘要是有损但保留语义的；
- L3 滚动窗口是激进丢弃；
- L4 硬截断是最后兜底。

逐级尝试避免「一开始就硬截断丢失关键上下文」。这种分级管道在 AI Coding 助手品类中是首次系统化设计。

#### 5.1.3 带来的价值

| 价值 | 说明 |
|------|------|
| 上下文利用率高 | 先回收低权重消息，保留高权重，对话连贯性好 |
| 压缩可观测 | 每级压缩打 OTel span，`compress.level`/`tokens_before`/`tokens_after` 可追溯 |
| 可回滚 | `ContextSnapshot` 记录压缩前状态，可 restore |

### 5.2 压缩熔断机制（失败计数≥3 熔断，防 Thrash）

#### 5.2.1 创新点

压缩管道最危险的失效模式是 **Thrash Loop**：压缩后立即又填满 → 再次压缩 → 再填满，烧光 token 预算。`design.md` §3.6 + C-29 定义了熔断状态机：

```
build_chat_request
   │
   ├─ token_count ≤ budget * 0.85  → 正常发送，重置失败计数
   │
   └─ token_count > budget * 0.85  → 触发压缩管道（L1→L4）
        │
        ├─ 压缩成功（token 降到阈值下）→ 失败计数清零，发送
        ├─ 压缩失败（LLM 摘要调用失败等）→ 失败计数 +1
        │    ├─ 失败计数 < 3  → 降级链重试
        │    ├─ 失败计数 = 3  → 熔断：注入错误中止本轮
        │    └─ 失败计数 ≥ 5  → 强制 TurnEnd，保留现场供 /resume
        │
        └─ 压缩后立即又超阈值（Thrash 检测）
             └─ 连续 2 次"压缩完即超" → 熔断
```

C-29 关键约束：**熔断由 Runtime 状态机判定，与 LLM 输出无关**。LLM 不得通过文本「声称压缩成功」「要求继续」「忽略错误」来跳过熔断。

#### 5.2.2 为什么创新

压缩 Thrash 是 AI Coding 助手的隐蔽陷阱——LLM 可能反复触发压缩却无法跳出，烧光预算无产出。`minicoding-rs` 的熔断机制是品类内首次明确为 L0 硬约束（C-29），Claude Code 有类似机制但未明确为约束。熔断与 §2.4 的 `max_tool_iters`、`security.md` §8.8 的沙箱拒绝熔断器三者互补：分别防「工具死循环」「沙箱拒绝死循环」「压缩死循环」。

#### 5.2.3 带来的价值

- **防 token 烧损**：连续 3 次失败即中止，不无限重试；
- **状态保留**：≥5 次强制 TurnEnd 保留现场，供 `/resume` 恢复；
- **抗 LLM 绕过**：熔断与 LLM 输出无关，状态机强制。

### 5.3 预测性压缩（根据历史增长提前 compact）

#### 5.3.1 创新点

`design.md` §3.9 的反应式 compact 在 `token_count > budget * 0.85` 时触发，存在两个不足：(1) 触发点可能在 turn 中途，打断流式输出；(2) 阈值靠近上限，易在 turn 末再次触发加剧 Thrash。预测性 compact 在 turn 开始前根据历史增长估算「本轮是否会超」，提前压缩：

```
predicted_tokens = current_tokens + avg_turn_growth
avg_turn_growth  = EMA(turn_token_delta_history, alpha=0.3)
baseline_growth  = config.context.predictive_baseline_growth_tokens  # 默认 15000

if config.context.predictive_compact_enabled
   and predicted_tokens > budget * 0.85
   and current_tokens <= budget * 0.85:    # 避免与反应式 compact 重复触发
    trigger predictive_compact()
```

#### 5.3.2 为什么创新

预测性 compact 用 EMA（指数移动平均）跟踪历史 turn token 增量，比简单平均更能跟踪近期变化（如用户突然开始大量 read 大文件）。冷启动期（历史样本 < 3）用 `baseline_growth` 兜底。预测性 compact 与反应式 compact **互补而非替代**：反应式是兜底防线，预测性是优化。

#### 5.3.3 带来的价值

| 价值 | 说明 |
|------|------|
| 避免 turn 中断 | 在 turn 间隙提前压缩，不打断流式输出 |
| 降低 Thrash 风险 | 给本轮留足空间，减少连续触发 |
| 自适应 | EMA 跟踪近期增长模式，冷启动 baseline 兜底 |

### 5.4 Post-compact 阶段化恢复

#### 5.4.1 创新点

`design.md` §3.10：L2/L3 压缩会把「刚 read 过的文件」tool_result 一起摘要或丢弃，导致 compact 后模型丢失文件内容上下文，下一轮被迫重新 `fs.read`，浪费 token 且打断思路。Post-compact 上下文恢复机制在 compact 后把「最近读过的文件」重新注入：

```
compact 完成
   │
   ▼
对 recent_read_paths 环形缓冲（容量 5）中每个路径：
   ├─ 检查路径是否仍存在于 retained tail → 是则跳过
   ├─ 检查文件是否仍存在 → 否则跳过并 warn
   ├─ 读取文件内容，按 post_compact_max_tokens_per_file 截断
   └─ 累计 token 不超过 post_compact_token_budget
   │
   ▼
拼装为 <post_compact_context> 块注入 system 段末尾
```

`ContextManager` 在每个 `fs.read` 成功返回时记录路径到 `recent_read_paths` 环形缓冲。

#### 5.4.2 为什么创新

同类产品压缩后通常让模型自行重新 read，造成 token 浪费与思路打断。`minicoding-rs` 通过跟踪 `recent_read_paths` 在 compact 后主动重注入，让模型无缝继续工作。这是品类内首次的「压缩后上下文恢复」设计。

#### 5.4.3 带来的价值

- **省 token**：避免 compact 后重复 `fs.read`；
- **思路连贯**：模型不需重新定位文件，直接继续工作；
- **配置可控**：`post_compact_max_files`/`post_compact_token_budget`/`post_compact_max_tokens_per_file` 可调。

### 5.5 独立小 LLM 降本（摘要/compact/memory 用便宜模型）

#### 5.5.1 创新点

`design.md` §3.8 + `features.md` L-08：为摘要/compact/memory 提取配置独立 provider（`[provider.small]`），未设置时与主 provider 相同，可配更便宜模型降本。压缩失败降级链优先调小 LLM：

```
L2 摘要压缩触发
   │
   ├─ 1. 主 provider 生成摘要（≤200 token/条）
   │    └─ 失败 ↓
   ├─ 2. 备用小模型或同 provider 重试 1 次
   │    └─ 失败 ↓
   ├─ 3. 启发式兜底（不调 LLM，取首 80 字 + 末 80 字）
   │    └─ 失败 ↓
   └─ 4. 跳过 L2，直接进 L3 滚动窗口
```

#### 5.5.2 为什么创新

主 LLM（如 Claude Sonnet）成本高，用它做摘要是大材小用。`minicoding-rs` 把摘要/compact/memory 这类「不需要强推理」的任务路由到小 LLM（如 Haiku 级），显著降本。降级链确保即使小 LLM 也不可用时仍能继续（启发式兜底 + L3 滚动窗口），**永不向上抛错中断对话**。

#### 5.5.3 带来的价值

- **成本降低**：摘要用小 LLM，主 LLM 专注核心任务；
- **可靠性高**：降级链确保任何 LLM 失败都不中断对话；
- **质量可追溯**：`quality` 字段（`llm`/`heuristic`/`dropped`）记入压缩日志。

---

## 6. Agent 循环创新

### 6.1 无副作用并行 + 有副作用串行调度

#### 6.1.1 创新点

`design.md` §2.3 的工具执行遵循两条硬规则：

1. **无副作用工具（`SideEffect::None`）可并行执行**：同一轮中多个只读工具调用（`fs.read`/`fs.grep`/`fs.glob`/`git.diff` 等）用 `FuturesUnordered` 并发派发，降低长延迟叠加；
2. **有副作用工具（`FileWrite`/`Command`/`Network`）必须严格串行**：按 LLM 返回的 `tool_calls` 顺序逐个执行，前一个完成（含权限决策与审计落盘）后才启动下一个。

实现上把本轮调用按 `side_effect` 分桶：先并发跑无副作用桶（`buffer_unordered(8)`），再顺序跑有副作用桶。

```
本轮 tool_calls: [fs.read A, fs.read B, fs.write C, shell.run D, fs.read E]
                                    │
                                    ▼
                    ┌─────────────────────────────┐
                    │ 分桶 by side_effect          │
                    └─────────────────────────────┘
                                    │
        ┌───────────────────────────┴───────────────────────────┐
        ▼                                                       ▼
   无副作用桶（并行）                                    有副作用桶（串行）
   [fs.read A, fs.read B, fs.read E]                  [fs.write C, shell.run D]
        │                                                       │
        ▼                                                       ▼
   buffer_unordered(8)                                  for call in calls: run_one
        │                                                       │
        ▼                                                       ▼
   并发执行，结果收集                                    逐个执行，权限+审计闭环
        │                                                       │
        └───────────────────────────┬───────────────────────────┘
                                    ▼
                    按 LLM 原始顺序回填 tool_results
```

#### 6.1.2 为什么创新

同类产品的工具调度多为全串行（Aider）或全并行（部分实验性项目）。全串行浪费 IO 等待时间，全并行导致副作用竞态（如先 `fs.write` 再 `shell.run cargo build`，并行会导致 build 读到半写文件）。`minicoding-rs` 按 `side_effect` 分桶：

- 无副作用桶并行降低延迟；
- 有副作用桶串行保证语义正确（副作用间往往存在隐式依赖）；
- C-11 约束工具实现必须如实返回 `side_effect()`，把写操作标为 `None` 属于实现缺陷，CI 拦截。

#### 6.1.3 带来的价值

| 价值 | 说明 |
|------|------|
| 降低延迟 | 多个只读工具并行，长延迟叠加变单个延迟 |
| 副作用安全 | 串行避免竞态、重复授权、审计顺序混乱、回滚不可追溯 |
| 审计清晰 | 串行桶保证审计日志顺序与执行顺序一致 |

### 6.2 类型化子 Agent（Explore/Plan/General/Custom）

#### 6.2.1 创新点

`design.md` §7.2 把子 Agent 从自由 `role: String` 改为类型化枚举，每类预设模型路由、工具子集、记忆加载策略：

```rust
pub enum SubagentType {
    Explore,         // 快速代码库探查：固定小模型，只读工具子集，跳过 AGENTS.md 与长期记忆
    Plan,            // 计划模式下收集上下文：只读，仅 Plan 模式可用
    GeneralPurpose,  // 通用多步任务：继承父会话模型与全工具，可写可改
    Custom(String),  // 自定义：从 .minicoding/agents/*.md 加载
}
```

| 类型 | 模型 | 工具集 | skip_memory | can_spawn | 用途 |
|------|------|--------|:---:|:---:|------|
| `Explore` | 小模型（强制） | 只读 | 是 | 否 | 廉价快速定位文件/行 |
| `Plan` | 继承父 | 只读 | 是 | 否 | Plan 模式下收集上下文 |
| `GeneralPurpose` | 继承父 | 全工具 | 否 | 否 | 复杂多步任务 |
| `Custom(name)` | frontmatter 指定 | frontmatter 指定 | 可配 | 可配 | 用户扩展 |

#### 6.2.2 为什么创新

参考 Claude Code 的 `Explore`/`Plan`/`general-purpose` 类型化分工，`minicoding-rs` 把它系统化为枚举 + 预设配置：

- **Explore 强制小模型**：降低探查成本，避免主模型被污染；
- **不嵌套约束**：`can_spawn_subagent == false` 时从 `allowed_tools` 中移除 `task.spawn`，杜绝子 Agent 再生子 Agent 的无限递归；
- **Plan 模式守卫**：`SubagentType::Plan` 仅在 `PermissionMode::Plan` 下可派发，其它模式下退化为 `Explore`（不报错，避免模型因模式状态失败重试）。

#### 6.2.3 带来的价值

- **成本可控**：Explore 用小模型，避免主模型被高消耗探查任务污染；
- **职责清晰**：每类子 Agent 有明确工具集与记忆策略；
- **防递归**：不嵌套约束防无限派生。

### 6.3 worktree 隔离子 Agent（git worktree 并行工作）

#### 6.3.1 创新点

`design.md` §7.5 + A-15：当多个子 Agent 并行修改文件时，共享同一工作目录会导致冲突。参考 CC 的 `isolation: worktree`，为 `GeneralPurpose` 子 Agent 提供 git worktree 隔离选项：

```rust
pub enum Isolation {
    Shared,                    // 共享父会话工作目录（默认）
    Worktree(WorktreeSpec),    // 在独立 git worktree 中运行
}

pub struct WorktreeSpec {
    pub branch_prefix: String,      // 如 "subagent/"，生成分支名 subagent/{task_id}
    pub auto_cleanup: bool,         // 子 Agent 完成后删除 worktree + 分支
    pub merge_back: MergeStrategy,  // None / CherryPick / MergeCommit
}
```

`Worktree` 模式下：派发前 `git worktree add` → 子 Agent 的 `ToolContext.workdir` 指向 worktree 路径 → 完成后按 `merge_back` 策略合并回主分支 → `auto_cleanup` 清理。

#### 6.3.2 为什么创新

并行子 Agent 修改同一工作目录是 AI Coding 助手的痛点——A 改了 foo.rs，B 也想改 foo.rs，冲突不可避免。`minicoding-rs` 用 git worktree 提供物理隔离，每个子 Agent 拥有独立工作目录，文件改动不互相干扰。这是品类内首次的 worktree 隔离设计（CC 有类似但实现细节不同）。

#### 6.3.3 带来的价值

- **真并行**：多个子 Agent 可同时改文件而不冲突；
- **合并可控**：`MergeStrategy` 支持 None/CherryPick/MergeCommit，父 Agent 决策合并策略；
- **降级安全**：非 git 仓库降级为 `Shared` 并 warn。

### 6.4 Plan 模式双重只读强制（硬门 + 软引导）

#### 6.4.1 创新点

`design.md` §16.1 的 Plan 模式采用「硬门 + 软引导」双层设计：

| 层 | 实现 | 作用 |
|----|------|------|
| 硬门 | `PermissionPolicy::check` 在 `PermissionMode::Plan` 下，对所有 `side_effect != None` 的工具直接返回 `Deny("plan mode: read-only")` | 即使 LLM 尝试调用写工具，Runtime 也强制拒绝 |
| 软引导 | 每次 `build_chat_request` 注入 system reminder：「Plan mode is active. You MUST NOT make any edits...」 | 让模型自觉避免尝试写操作，减少无效工具调用的 token 浪费 |

`plan.exit` 工具携带「预批准命令」清单，执行期可跳过权限门减少摩擦（参考 CC 的 `allowedPrompts`）。

#### 6.4.2 为什么创新

软引导不是安全边界（`rules.md` §5 声明「提示词不是安全边界」），它的价值是降低成本——硬门保证安全，软引导减少浪费。这种「defense in depth」设计避免了依赖单一机制：仅靠硬门则 LLM 会反复尝试写操作烧 token，仅靠软引导则不安全。`minicoding-rs` 把两者结合，硬门是 Rust 代码强制，软引导是系统提示词。

#### 6.4.3 带来的价值

- **安全**：硬门保证 Plan 模式下不可能执行写操作；
- **省 token**：软引导让 LLM 自觉避免尝试，减少无效调用；
- **预批准便利**：`plan.exit` 的 `allowed_prompts` 让执行期跳过逐条确认。

---

## 7. 可观测性创新

### 7.1 OpenTelemetry 一等公民（M0 起接入）

#### 7.1.1 创新点

`tech-stack.md` §7 + `design.md` §15：OpenTelemetry 是**一等公民**（非后续可选），从 M0 起接入。业务代码只写 `tracing` 宏，subscriber 层同时输出本地文件日志与 OTLP trace，无重复埋点。

#### 7.1.2 为什么创新

同类产品的可观测性多为「事后补丁」：

- **Claude Code**：闭源，可观测性不可知；
- **Aider**：Python print 日志，无结构化追踪；
- **Codex CLI**：有 tracing 但 OTel 非一等公民。

`minicoding-rs` 把 OTel 作为 M0 基础设施，所有跨组件边界（session/turn/llm_call/tool_call/compress/permission/hook/mcp）必须打 span，写入开发约束（AGENTS.md §9 检查清单）。这种「从第一天就内建可观测性」的设计在 AI Coding 助手品类中是首次。

#### 7.1.3 带来的价值

- **全链路追踪**：对接 Jaeger/Tempo/Grafana，定位长会话延迟与异常；
- **零代码改动切换后端**：`OTEL_EXPORTER_OTLP_ENDPOINT` 环境变量控制；
- **采样独立**：`AlwaysOn`（调试）/ `TraceIdRatio`（生产），由 `OTEL_TRACES_SAMPLER` 控制。

### 7.2 全链路 span 层级（session > turn > llm_call > tool_call > permission）

#### 7.2.1 创新点

`design.md` §15.1 的 span 层级：

```
session (session.id)                         <- RootSpan
 └─ turn (turn.n)                            <- 每轮对话
     ├─ context.build_chat_request           <- 上下文构建/压缩
     │    └─ compress (strategy, level)      <- 压缩步骤
     ├─ llm.chat_stream (provider, model)    <- LLM 调用
     │    └─ retry.attempt (n)               <- 重试
     └─ tool.call (tool.name, side_effect)   <- 工具调用
          ├─ permission.check (verdict)
          ├─ permission.prompt (decision)    <- 仅 Ask 时
          └─ tool.dispatch (elapsed, ok)
```

每个 span 携带关键属性（`design.md` §15.2）：`session.id`/`turn.index`/`llm.provider`/`llm.model`/`tool.name`/`tool.side_effect`/`tool.parallel`/`permission.verdict`/`compress.level` 等。

#### 7.2.2 为什么创新

span 层级反映了 Agent 循环的真实控制流，让 trace 可读。子 Agent 通过 OTel Context 传播父 span，使子任务挂在主会话 trace 下。事件总线订阅者把 `Event::ToolCallEnd` 等转为 span events，使「日志」与「trace」在同一时间线对齐。

#### 7.2.3 带来的价值

- **延迟定位**：长会话中快速定位是 LLM 慢、工具慢还是权限交互慢；
- **异常归因**：`compress.circuit_breaker` span event 记录熔断，便于诊断；
- **子 Agent 追踪**：子任务挂在父 trace 下，全链路可见。

### 7.3 业务代码只写 tracing 宏，subscriber 层双输出

#### 7.3.1 创新点

`design.md` §15.3 + §15.4：业务代码只写 `tracing` 宏，由 `tracing-subscriber` 的多层 layer 同时输出：

- **本地文件日志**（`tracing-appender` 滚动日志）：面向单机排障，`RUST_LOG` 控制级别；
- **OTel export**（`tracing-opentelemetry` 桥接）：面向跨会话/跨机器聚合分析，OTLP/HTTP 或 OTLP/gRPC，采样独立。

两者共享同一 `tracing` 调用点，**无重复埋点开销**。

#### 7.3.2 为什么创新

简单实现容易在业务代码里同时写 `log::info!` 和 `tracer::span!`，造成重复埋点与维护负担。`minicoding-rs` 通过 subscriber 层的 layer 机制，业务代码只写一次 `tracing` 宏，subscriber 层决定输出到哪些后端。这是 Rust tracing 生态的最佳实践，但在 AI Coding 助手品类中首次系统化应用。

#### 7.3.3 带来的价值

- **零重复埋点**：业务代码只写一次宏；
- **独立控制**：本地日志与 OTel 采样互不干扰；
- **可扩展**：新增输出后端只需加 layer，不改业务代码。

---

## 8. 扩展机制创新

### 8.1 10 类 Hook 生命周期事件 + asyncRewake 异步唤醒

#### 8.1.1 创新点

`hooks.md` §2 定义 10 类生命周期事件（参考 CC 27 类精简为 10 类）：

| # | 事件 | 触发阶段 | 执行模式 | 可否阻断 | 可否改写 |
|---|------|---------|---------|:---:|:---:|
| 1 | `SessionStart` | 会话开始 | 同步 | 否 | 否 |
| 2 | `UserPromptSubmit` | 用户提交后、LLM 调用前 | 同步 | 是 | 否 |
| 3 | `PreToolUse` | `policy.check` 后、工具执行前 | 同步 | 是 | 是 |
| 4 | `PostToolUse` | 工具执行成功后 | 同步/异步可选 | 否 | 是 |
| 5 | `PostToolUseFailure` | 工具执行失败后 | 同步/异步可选 | 否 | 是 |
| 6 | `PreCompact` | 上下文压缩管道启动前 | 同步 | 否 | 否 |
| 7 | `PostCompact` | 上下文压缩完成后 | 同步 | 否 | 否 |
| 8 | `Stop` | 主 Agent 一轮结束 | 同步/异步可选 | 是 | 否 |
| 9 | `SubagentStop` | 子 Agent 完成 | 同步 | 否 | 否 |
| 10 | `PermissionRequest` | `Verdict::Ask` 即将弹窗前 | 同步 | 是 | 否 |

`asyncRewake`（hooks.md §11 + C-26/C-32）：`PostToolUse`/`PostToolUseFailure`/`Stop` 三类「事后」事件支持异步唤醒，Hook 同步返回 `async_rewake = Some(spec)` 后主流程不阻塞，Hook 子进程在后台继续执行，完成后唤醒 Agent。

#### 8.1.2 为什么创新

CC 有 27 类事件过于复杂，`minicoding-rs` 精简为 10 类覆盖核心场景。asyncRewake 解决了「Hook 需执行长时异步任务（如安全扫描、CI 触发、依赖更新检查）但不阻塞当前轮次」的痛点。关键约束（C-26）：

- **适用事件受限**：仅 `PostToolUse`/`PostToolUseFailure`/`Stop` 有效；`PreToolUse`/`PermissionRequest` 等「事前/同步决策」事件返回 `async_rewake` 视为协议错误；
- **后台进程同等待遇**：async_rewake 的后台 Hook 子进程与 `shell.run` 子进程遵守相同的凭证隔离（C-04）、沙箱策略（C-22）、路径沙箱（C-03）；
- **结果是数据非指令**：`wake_prompt` 包裹 `<async_rewake>` 边界，声明非指令（与 C-05 同构）；
- **资源约束**：同一 session 最多 3 个并发 async_rewake，超限拒绝并记审计。

#### 8.1.3 带来的价值

| 价值 | 说明 |
|------|------|
| 可扩展 | 用户用脚本或 Rust 实现接入，无需改 core |
| 安全 | Hook 受 L0 约束（C-21），不可覆盖内置黑名单 |
| 异步 | asyncRewake 让长时任务不阻塞主流程 |
| 可观测 | Hook 执行打 `hook.run` span |

### 8.2 MCP 双向集成（client + server）

#### 8.2.1 创新点

`design.md` §19 + `features.md` X-01..X-14：MCP 双向集成——

- **作为 MCP client**：连接外部 server（GitHub/Slack/数据库），`mcp__<server>__<tool>` 命名，工具包装为本地 `Tool`，`side_effect` 据 `readOnlyHint`/`destructiveHint` 映射（C-25）；
- **作为 MCP server**：`minicoding serve --as-mcp-server` 把内置工具暴露给其他 Agent 调用。

用 `rmcp` 2.2 官方 Rust MCP SDK（对齐 MCP 2025-11-25 spec），**不自研** stdio/http 薄封装（`tech-stack.md` §11.1）。

#### 8.2.2 为什么创新

同类产品的 MCP 集成多为单向 client。`minicoding-rs` 的双向集成让 Agent 既可消费外部工具，也可被其他 Agent 消费，形成生态互操作。关键创新：

- **进程池**（X-12）：MCP server 连接跨 turn 复用，不每 turn 重启；
- **后台预热**（X-13）：`warm_up` 刷新工具列表，确保连接活跃；
- **inflight merge**（X-14）：同 server+tool+input 并发请求合并（`Shared<Future>`），避免重复调用；
- **工具检索**（X-09）：MCP 工具多时 BM25 按需检索，不引入向量依赖。

#### 8.2.3 带来的价值

- **生态互操作**：可消费外部 MCP server，也可被其他 Agent 消费；
- **性能优化**：进程池 + 预热 + inflight merge 降低 MCP 调用开销；
- **协议对齐**：用官方 SDK，不自研胶水，协议跟进快。

### 8.3 project 作用域 MCP 首次批准（防恶意仓库植入）

#### 8.3.1 创新点

C-24 + `design.md` §19.4 + X-07：含 `.minicoding/mcp.json` 的仓库首次进入时，每个 project 作用域 MCP server 必须经用户逐个显式批准（写入 `mcp_choices.toml`），未批准的 server 不连接、不注册工具。这防止恶意仓库通过 `mcp.json` 植入恶意 server 窃取数据或执行越权操作。

#### 8.3.2 为什么创新

恶意仓库植入 MCP server 是真实供应链攻击面（`security.md` §1.2 威胁 T11）。同类产品中，CC 有类似 project-scope 审批，`minicoding-rs` 沿用并强化为 C-24 L0 硬约束，启动自检确保 `mcp_choices.toml` 加载完成、未批准 server 已隔离。

#### 8.3.3 带来的价值

- **防供应链植入**：恶意仓库的 MCP server 不会自动连接；
- **用户知情**：首次进入仓库逐个批准，用户掌控；
- **审计可追溯**：批准状态落 `mcp_choices.toml`。

### 8.4 Extension SDK 稳定 API

#### 8.4.1 创新点

`design.md` §23 + `features.md` X-20..X-22 + `modules.md` §17：`minicoding-extension-sdk` crate 提供扩展作者稳定 API（`Extension` trait + `Registrar` + `ExtensionManifest`）。扩展通过 `Registrar` 注册 contributor 注入 prompt section，扩展注册的工具仍走 `ToolRegistry` dispatch，确保权限审计一致（C-01/C-02 不被绕过）。

#### 8.4.2 为什么创新

AI Coding 助手的扩展性多依赖 Hook（脚本）或 MCP（远程）。`minicoding-rs` 提供「进程内扩展」第三条路径——Extension SDK 让 Rust 开发者可直接编写高性能扩展，复用 Runtime 的权限/审计/OTel 基础设施，而不需走子进程或网络开销。关键约束：扩展工具仍走 `ToolRegistry` dispatch，C-01/C-02 不被绕过。

#### 8.4.3 带来的价值

- **高性能**：进程内扩展无 IPC 开销；
- **安全一致**：扩展工具走同一权限/审计/OTel 链路；
- **稳定 API**：SDK 版本化，扩展作者有稳定契约。

---

## 9. 会话管理创新

### 9.1 Event Sourcing（不可变事件流 + 快照回放）

#### 9.1.1 创新点

`design.md` §25 + `features.md` S-23..S-27：将会话状态建模为不可变事件流（`Event` 持久化 + snapshot 重放），替代纯 JSONL 消息追加模型。

```
EventStore（append-only，不可变，{id}.events.jsonl）
   │
   │ PersistedEvent: SessionCreated / MessageAppended /
   │                 PermissionResolved / PermissionModeChanged /
   │                 TaskUpdated / TurnEnd
   │
   ▼
replay_session_state（重放事件，构建 Session 视图）
   │
   ├── SnapshotStore（{id}.snapshot.json，每 50 条 MessageAppended 落盘一次）
   │
   ▼
Session { messages, permission_mode, audit_trail, ... }
```

Event 与 Message 的关键区别：Event 记录「发生了什么」（如 `PermissionModeChanged { from, to }`），Message 记录「当前是什么」。Event 不可变，Session 是 Event 的投影——同一 EventStore 可投影出不同视图。

#### 9.1.2 为什么创新

纯 JSONL 消息追加模型无法表达「权限决策审计」「模式切换历史」等非消息状态。Event Sourcing 把会话状态建模为事件流，收益：

- **时间旅行调试**：从任意 seq 重放可重建历史状态；
- **多客户端状态同步**：与 SSE cursor 恢复（E-13）协同；
- **审计回放**：`--replay` 不再依赖消息日志而是事件重放，权限决策、模式切换等审计轨迹完整保留；
- **跨会话 fork/merge**：fork = 从 seq 重放（未来增强）。

与原 JSONL 消息日志**双写并存**——新会话同时写消息日志与事件流，旧会话无事件流时回退到消息日志路径，平滑过渡。

#### 9.1.3 带来的价值

| 价值 | 说明 |
|------|------|
| 多视图 | 同一 EventStore 可投影出「完整历史」/「当前窗口」/「审计视图」 |
| 崩溃恢复 | snapshot + 事件流重放，崩溃最多丢未 fsync 的事件 |
| SSE 协同 | `durable_seq` 即 EventStore 持久化进度，cursor 恢复天然实现 |

### 9.2 Parent-UUID 链式结构（支持 fork/压缩边界/side-chain）

#### 9.2.1 创新点

`design.md` §10.3：每条消息记录 `uuid` 与 `parent_uuid`，形成链表而非纯数组。三种高级语义：

1. **Fork（分叉）**：同一 `parent_uuid` 可有多个子消息，表示「从某点尝试不同方向」。`--fork-session` 复制链前缀到新会话文件，原会话文件只读不写；
2. **Compaction Boundary（压缩边界）**：压缩产生的摘要消息 `parent_uuid = None`，表示「此前历史已折叠进摘要」。`index.json` 的 `last_compaction_id` 字段 O(1) 定位，无需全文件扫描；
3. **Side-chain（子 Agent 链）**：子 Agent 的 transcript 作为 side-chain 存储在主会话 JSONL 中，`parent_uuid` 指向派发它的 `task.spawn` 工具调用。

#### 9.2.2 为什么创新

纯顺序 JSONL 无法表达「分叉」「压缩边界」「子 Agent 链」。Parent-UUID 链让会话文件具备 DAG 表达能力，同时与 JSONL 追加写不冲突——`parent_uuid` 只是行内可选字段，写入仍是单行 `append`，崩溃安全。默认读取线性顺序即链顺序，按需建 DAG。

#### 9.2.3 带来的价值

- **fork 零风险**：原会话文件只读不写，新会话文件追加写；
- **压缩边界 O(1) 定位**：`index.json` 的 `last_compaction_id` 指针；
- **子 Agent 可追溯**：side-chain 的 `parent_uuid` 指向派发它的工具调用。

### 9.3 64KB 窗口会话列出（万级 <1s）

#### 9.3.1 创新点

`design.md` §10.7：`minicoding sessions list` 列出数千会话时，不全量反序列化每个 JSONL（可能数 MB），而是只读首尾 64KB：

- **首 64KB**：提取 session_id、首条用户消息（作标题预览）、创建时间、消息类型分布；
- **尾 64KB**：提取最后一条消息时间、当前状态、消息计数；
- 中间内容跳过，按需 `minicoding sessions show <id>` 全量加载。

这使得万级会话列表的加载时间 < 200ms（与 `design.md` §13 性能预算一致）。

#### 9.3.2 为什么创新

会话日志可能数 MB，全量反序列化万级会话耗时数十秒。`minicoding-rs` 的 64KB 窗口策略借鉴 CC 的实践，用固定大小的 IO 换取列表场景的 O(1) 加载时间。这是品类内罕见的「列表场景专用快速路径」设计。

#### 9.3.3 带来的价值

- **万级会话 <200ms**：列表场景专用快速路径；
- **按需加载**：`show <id>` 才全量加载；
- **IO 固定**：每个会话固定 128KB IO，可预测。

### 9.4 /undo operation 级回滚（FileChangeJournal）

#### 9.4.1 创新点

`design.md` §17 + C-28 + `features.md` A-10/S-07：`FileChangeJournal` 实现会话内 `/undo` 文件回滚，operation 级撤销最近 N 次 turn 的文件改动。关键约束（C-28）：

- **撤销不重新授权**：`/undo` 反向恢复 `before` 内容是用户显式触发的反向操作，不重新走 `PermissionPolicy`；但撤销本身记入审计日志；
- **冲突检测不可强行覆盖**：恢复前比对当前文件内容与 journal 的 `after`，不一致记入 `failed_files`，**不强行覆盖**（防 `/undo` 覆盖用户外部编辑）；
- **不落盘**：journal 含文件原文，落盘等于多存一份敏感数据，故仅驻留内存、会话结束即销毁；
- **不可越界恢复**：恢复路径仍经 `sandbox_path` 校验；
- **不可回滚跨会话**：跨会话回滚引导用户用 Git。

#### 9.4.2 为什么创新

Codex 的 `/rewind` 未实现冲突检测，社区强烈要求。`minicoding-rs` 的 `/undo` 是 Codex `/rewind` 未实现但社区要求的安全行为——防止 `/undo` 覆盖用户外部编辑。这是品类内首次明确为 L0 硬约束（C-28）的文件回滚机制。

#### 9.4.3 带来的价值

- **operation 级撤销**：`/undo 3` 撤销最近 3 次 turn 的文件改动；
- **冲突安全**：外部编辑不强行覆盖；
- **跨会话引导 Git**：不内建跨会话回滚，避免快照存储成本失控。

---

## 10. 前端与桌面创新

### 10.1 全 Rust 工具链（oxlint/oxfmt/Vite Rolldown/Tailwind v4 Oxide）

#### 10.1.1 创新点

`m9-design.md` §8.1 + `tech-stack.md` §4.1：M9 前端工具链全用 Rust 实现：

| 工具 | 用途 | 语言 | 速度优势 |
|------|------|------|---------|
| Vite (Rolldown) | JS/TS bundler | Rust | 10x vs webpack |
| Tailwind v4 (Oxide) | CSS engine | Rust | 5x vs v3 |
| oxlint | JS/TS linter | Rust | 50x vs ESLint |
| oxfmt | JS/TS formatter | Rust | 20x vs Prettier |

与后端 Rust 工具链形成「全 Rust 工具链」一致性。

#### 10.1.2 为什么创新

同类产品的前端工具链多为 Node.js 实现（webpack/ESLint/Prettier），构建慢。`minicoding-rs` 把 Rust 工具链理念延伸到前端，构建/Lint/格式化速度均显著优于传统 Node 工具链。CI 校验前端 `oxlint && oxfmt --check && tsc --noEmit && vite build`，与 Rust 侧 `cargo fmt --check && clippy && test` 对齐。

#### 10.1.3 带来的价值

- **构建快**：Vite Rolldown 10x vs webpack；
- **Lint 快**：oxlint 50x vs ESLint；
- **一致性**：前后端工具链同语言，开发者心智统一。

### 10.2 Tauri 2.x 替代 Electron（体积/内存/安全优势）

#### 10.2.1 创新点

`m9-design.md` §3.3 + `tech-stack.md` §4.1.2：桌面壳用 Tauri 2.x 而非 Electron：

| 维度 | Tauri 2.x | Electron |
|------|-----------|----------|
| 体积 | 5–10 MB | 100 MB+ |
| 内存 | 30–50 MB | 100–200 MB |
| 安全 | Rust 内存安全 + CSP 严格 | Node.js 难以管控 |
| IPC | Rust 命令直接调用 | JSON 序列化 |
| Mobile | 2.x 支持 | 不支持 |

#### 10.2.2 为什么创新

Tauri 与本项目「Rust 一等公民」理念一致。Electron 内置 Chromium + Node，体积/内存/安全均劣。Tauri 用系统 webview + Rust sidecar，桌面应用体积 < 15MB，内存 < 80MB（`m9-design.md` §9 性能目标）。

#### 10.2.3 带来的价值

- **体积小**：5–10MB vs Electron 100MB+；
- **内存省**：系统 webview vs 内置 Chromium；
- **安全优**：默认禁用远程内容，CSP 严格；
- **移动端**：2.x 支持 iOS/Android（M10+ 留待）。

### 10.3 ts-rs DTO 自动生成（类型安全，不手写双份）

#### 10.3.1 创新点

`AGENTS.md` §8.4：`minicoding-protocol` 的 Rust DTO 通过 `ts-rs` 或 `specta` 自动生成 TypeScript 类型 + Zod schema，**不手写双份**。生成产物放 `minicoding-web/src/api/generated/`，不手动编辑（文件头标注 `// AUTO-GENERATED, DO NOT EDIT`）。后端 DTO 变更后，前端 `pnpm gen-types` 重新生成，CI 校验生成产物与 Rust 源一致（`git diff --exit-code`）。运行时校验：JSON-RPC 响应必须经 Zod parse 后才进入业务层。

#### 10.3.2 为什么创新

手写双份类型（Rust + TypeScript）易漂移，运行时 schema 不匹配导致错误。`minicoding-rs` 用代码生成消除漂移，CI 强制校验生成产物与源一致。这是品类内首次的「DTO 自动生成 + CI 校验」设计。

#### 10.3.3 带来的价值

- **类型安全**：Rust DTO 变更自动同步到 TypeScript；
- **运行时校验**：Zod parse 防止后端 schema 漂移导致运行时错误；
- **CI 强制**：`git diff --exit-code` 确保生成产物与源一致。

### 10.4 SSE 流式更新（queryClient.setQueryData 增量）

#### 10.4.1 创新点

`m9-design.md` §5.1 + `AGENTS.md` §8.5：SSE 推送 `Event::Token`，前端用 TanStack Query 增量更新消息缓存，不触发 refetch：

```typescript
es.addEventListener('Token', (e) => {
  const token = TokenEventSchema.parse(JSON.parse(e.data));
  queryClient.setQueryData(['session', sessionId, 'messages'], (old = []) => {
    const last = old[old.length - 1];
    if (last?.role === 'assistant' && last.streaming) {
      return [...old.slice(0, -1), { ...last, content: last.content + token.delta }];
    }
    return [...old, { role: 'assistant', content: token.delta, streaming: true }];
  });
});
```

`Token` 事件追加到消息末尾，`MessageAppended` 事件替换整条消息。

#### 10.4.2 为什么创新

简单实现用 refetch 会导致流式渲染卡顿。`minicoding-rs` 用 `queryClient.setQueryData` 增量更新缓存，避免 refetch，SSE → 渲染延迟 < 50ms（`m9-design.md` §9 性能目标）。React Compiler 自动 memo 化消息列表，无需手写 `useMemo`/`React.memo`。

#### 10.4.3 带来的价值

- **低延迟**：SSE → 渲染 < 50ms；
- **无 refetch**：增量更新缓存，不触发网络请求；
- **断线重连**：cursor 恢复（E-13），不丢事件。

### 10.5 四形态统一后端（CLI/TUI/Web/桌面共享 Rust 后端）

#### 10.5.1 创新点

`README.md` §6 + `architecture.md` §8：四形态前端共享同一 Rust 后端：

| 形态 | 入口 | 适用场景 |
|------|------|---------|
| CLI | `minicoding` | 脚本化、批量执行（`minicoding exec`）、CI/容器 |
| TUI | `minicoding --tui` | 全屏交互式终端会话 |
| Web | `minicoding-server --web ./dist` | 浏览器访问，远程会话，多客户端 |
| 桌面 | `minicoding-desktop`（feature `desktop`） | Tauri 原生应用，系统托盘 + 全局快捷键 |

前端与 Rust 后端的唯一契约是 `minicoding-protocol` 的 wire types（JSON-RPC 2.0 DTO），**不直接调用 Rust API**（`AGENTS.md` §8.1）。

#### 10.5.2 为什么创新

同类产品多只有单一形态（CLI 或 Web）。`minicoding-rs` 的四形态共享后端让用户按场景选择，不重复实现业务逻辑。这种「可嵌入运行时 + 多形态前端」定位在 AI Coding 助手品类中是稀缺的。

#### 10.5.3 带来的价值

- **场景覆盖广**：CLI/TUI/Web/桌面各适配场景；
- **逻辑不重复**：业务逻辑在后端，前端只做 UI；
- **可嵌入**：`minicoding-sdk` 让第三方 Rust 程序嵌入 Agent 能力。

---

## 11. AI 辅助开发创新

### 11.1 AGENTS.md 约束体系（开发时 vs 运行时双约束）

#### 11.1.1 创新点

`AGENTS.md` §0 定义了项目级 AI 辅助编码约束，与 `docs/rules.md` 正交：

| 文件 | 约束对象 | 时机 | 性质 |
|------|---------|------|------|
| `docs/rules.md` | 被 minicoding 驱动的 LLM（运行时模型） | 运行时 | 大模型约束（C-01..C-35），由 Rust Runtime 强制 |
| `AGENTS.md`（本文件） | 帮我们写代码的 AI 助手（开发时模型） | 开发时 | 助手行为约束，由助手自觉 + 代码审查强制 |

`rules.md` 约束「被驱动的 LLM 不得越权」；`AGENTS.md` 约束「写代码的 AI 助手不得乱来」。二者作用域不同，**不互相替代**。

#### 11.1.2 为什么创新

AI 辅助开发有两种 LLM：(1) 运行时被 minicoding 驱动的 LLM（终端用户场景）；(2) 开发时帮写代码的 AI 助手（Claude Code/Cursor/Trae 等）。两者都需要约束，但约束内容不同——运行时约束防越权（安全），开发时约束防乱来（质量）。`minicoding-rs` 把两者显式分离到 `rules.md` 与 `AGENTS.md`，是品类内首次明确「双约束」体系。

#### 11.1.3 带来的价值

- **职责清晰**：运行时约束与开发时约束不混淆；
- **可独立演进**：`rules.md` 与 `AGENTS.md` 可独立更新；
- **强制力不同**：运行时由 Rust 代码强制，开发时由助手自觉 + 代码审查。

### 11.2 rules.md L0/L1/L2 分层约束模型

#### 11.2.1 创新点

`rules.md` §1 的三层约束模型：

| 级别 | 含义 | 违反后果 | 执行方 |
|------|------|---------|--------|
| **L0 硬约束** | 不可违反，违反即视为系统故障 | Runtime 拒绝执行 + 审计告警 | Rust 代码强制 |
| **L1 契约约束** | 工具调用/输出格式契约 | 转为错误回灌 LLM 自修正 | Runtime 校验 |
| **L2 软约束** | 行为规范，引导更好产出 | 注入提示纠正，不强制 | 系统提示词 |

共 35 条编号约束（C-01..C-35）：L0 16 条、L1 8 条、L2 9 条。每条约束在 `rules.md` §6 有实现位置映射，§8 有启动自检清单。

#### 11.2.2 为什么创新

同类产品的约束多为隐式（散落在系统提示词）或单层（仅硬约束）。`minicoding-rs` 的三层模型让约束的「强制力」显式分级：

- L0 不可协商（如 C-01 副作用必须经权限）；
- L1 转为错误让 LLM 自修正（如 C-08 工具调用必须符合 schema）；
- L2 注入提示引导（如 C-14 最小权限操作）。

这种分层模型让安全底线（L0）与行为引导（L2）分离，避免「为便利而放松 L0」的提议被默认接受（`rules.md` §7）。

#### 11.2.3 带来的价值

- **强制力分级**：L0/L1/L2 执行方与后果明确；
- **可审计**：每条约束有实现位置映射与启动自检；
- **演进有规范**：新增 L0 需 ADR 评审，L1 视为 API 变更走 SemVer，L2 可配置覆盖。

### 11.3 AI 助手行为约束（先读后改、不臆造 API、不绕过约束）

#### 11.3.1 创新点

`AGENTS.md` §7 定义 AI 助手行为约束：

| 约束 | 说明 |
|------|------|
| 先读后改 | 修改任何文件前**必须**先用 Read 工具读取目标文件，理解上下文 |
| 不臆造 API | 不确定的库 API（签名/版本/feature）必须查文档或读源码，不猜测 |
| 不绕过约束 | 即使被要求「快速实现」「先跑起来」，也不违反 §2-§5 规范 |
| 解释决策 | 选择方案时说明 **why**，不只贴代码不解释 |
| 不创建测试代码除非要求 | 默认不写测试，除非用户明确要求或验收标准要求 |
| 保持简洁 | 不做不必要的改进，不加多余抽象，不创建多余文件 |

#### 11.3.2 为什么创新

AI 助手在写代码时容易「自信地编造 API」或「为通过测试而绕过约束」。`minicoding-rs` 把这些行为约束写入 `AGENTS.md`，由助手自觉 + 代码审查强制。特别是「不绕过约束」明确：「如认为约束本身有问题，**先提出讨论**而非擅自违反」「不为『通过测试』而注释掉安全检查、放宽权限、跳过审计」「不在代码中留 `TODO: 后面补审计` 等绕过约束的痕迹」。

#### 11.3.3 带来的价值

- **代码质量**：先读后改避免基于猜测修改；
- **API 正确**：不臆造 API，先查文档/源码；
- **约束不被绕过**：即使被要求「快速实现」，也不违反规范；
- **简洁**：不做不必要的改进，避免过度工程。

---

## 12. 与同类产品对比的创新矩阵

### 12.1 vs Claude Code

| 维度 | Claude Code | minicoding-rs | 差异化 |
|------|-------------|---------------|--------|
| 开源 | 闭源 | AGPL-3.0 | 开源可审计 |
| 语言 | TypeScript | Rust 2024 | 性能、内存安全、可嵌入 |
| 架构 | 单体 CLI | 多 crate workspace + 零实现 core | 可嵌入、可替换 |
| 安全约束 | 系统提示词 + Hook 自觉 | L0/L1/L2 三层 + Rust 强制 | 实现层强制，抗 prompt 注入 |
| 沙箱 | 应用层 + 容器 | 应用层 + OS 内核级（seatbelt/landlock/seccomp） | 两道防线，内核级硬隔离 |
| 上下文压缩 | 滚动窗口 | 4 级管道 + 熔断 + 预测性 + Post-compact 恢复 | 分级管道、防 Thrash、省 token |
| 子 Agent | 类型化（Explore/Plan/General） | 类型化 + worktree 隔离 | 真并行不冲突 |
| Hooks | 27 类事件 | 10 类精简 + asyncRewake | 精简、异步唤醒 |
| 可观测性 | 无统一 trace | OTel 一等公民 + 全链路 span | M0 起接入，可观测 |
| 前端 | CLI only | CLI/TUI/Web/桌面四形态 | 多形态共享后端 |
| 约束模型 | 隐式 | rules.md 35 条编号 + AGENTS.md 双约束 | 显式分层、可审计 |

### 12.2 vs Codex CLI

| 维度 | Codex CLI | minicoding-rs | 差异化 |
|------|-----------|---------------|--------|
| 语言 | Rust | Rust 2024 | 同语言，edition 2024 + MSRV 1.99 |
| 架构 | 单一 CLI 偏向 | 多 crate workspace + 可嵌入 SDK | 可嵌入、多形态 |
| 沙箱 | Landlock/Seatbelt/Windows 受限令牌 | 同（参考 Codex）+ 自研 pre_exec 胶水 | landlock 直连原生隔离、无 EUPL 依赖 |
| 审批模式 | Untrusted/OnFailure/OnRequest/Never | 同（参考 Codex）+ 预设 | 预设一键选定 |
| `/undo` | `/rewind` 未实现冲突检测 | FileChangeJournal + 冲突检测（C-28） | 安全回滚，不覆盖外部编辑 |
| AGENTS.md | 不可被 Agent 编辑 | 同（C-23 L0 硬约束） | 强化为 L0，启动自检 |
| exec 模式 | AGENTS.md 不可信供应链制品 | 同（参考 Codex）+ L0 黑名单约束 | 实现层强制 |
| MCP | client | client + server 双向 | 双向互操作 |
| 可观测性 | tracing | OTel 一等公民 + 全链路 span | M0 起接入 |
| 前端 | CLI only | CLI/TUI/Web/桌面四形态 | 多形态 |

### 12.3 vs Aider

| 维度 | Aider | minicoding-rs | 差异化 |
|------|-------|---------------|--------|
| 语言 | Python | Rust 2024 | 性能、内存安全、冷启动快 |
| 架构 | 单体包 | 多 crate workspace + 可嵌入 SDK | 可嵌入、可替换 |
| 安全 | 应用层权限 | L0/L1/L2 + OS 内核级沙箱 | 两道防线，内核级硬隔离 |
| 上下文压缩 | 全量摘要 | 4 级管道 + 熔断 + 预测性 | 分级管道、防 Thrash |
| 子 Agent | 无 | 类型化 + worktree 隔离 | 并行探查、隔离修改 |
| Hooks | 无 | 10 类 + asyncRewake | 可扩展 |
| MCP | 无 | client + server 双向 | 生态互操作 |
| 可观测性 | print 日志 | OTel 一等公民 | 全链路追踪 |
| 前端 | CLI only | CLI/TUI/Web/桌面四形态 | 多形态 |
| Git 集成 | 强（核心特性） | git.diff/git.apply 工具 + worktree | 工具化、worktree 隔离 |

### 12.4 创新点对比表

| 创新点 | Claude Code | Codex CLI | Aider | minicoding-rs |
|--------|:-----------:|:---------:|:-----:|:-------------:|
| 多 crate workspace | ✗ | 部分 | ✗ | ✓ |
| 零实现 core | ✗ | ✗ | ✗ | ✓ |
| L0/L1/L2 三层约束 | ✗ | 部分 | ✗ | ✓ |
| 两道防线（应用层 + OS 沙箱） | 部分 | ✓ | ✗ | ✓ |
| 决策与交互分离 | ✗ | ✗ | ✗ | ✓ |
| 4 级压缩管道 + 熔断 | ✗ | ✗ | ✗ | ✓ |
| 预测性压缩 | ✗ | ✗ | ✗ | ✓ |
| Post-compact 恢复 | ✗ | ✗ | ✗ | ✓ |
| 类型化子 Agent + worktree | 部分 | ✗ | ✗ | ✓ |
| Plan 模式双重只读 | ✓ | ✗ | ✗ | ✓ |
| OTel 一等公民 | ✗ | 部分 | ✗ | ✓ |
| 10 类 Hook + asyncRewake | 27 类 | ✗ | ✗ | ✓ |
| MCP 双向 | client | client | ✗ | client + server |
| Event Sourcing | ✗ | ✗ | ✗ | ✓ |
| Parent-UUID 链 | ✓ | ✗ | ✗ | ✓ |
| 64KB 窗口会话列出 | ✓ | ✗ | ✗ | ✓ |
| `/undo` 冲突检测 | ✗ | ✗ | ✗ | ✓ |
| 全 Rust 工具链 | ✗ | ✗ | ✗ | ✓ |
| Tauri 替代 Electron | N/A | N/A | N/A | ✓ |
| ts-rs DTO 自动生成 | N/A | N/A | N/A | ✓ |
| 四形态共享后端 | ✗ | ✗ | ✗ | ✓ |
| AGENTS.md 双约束 | 部分 | 部分 | ✗ | ✓ |

---

## 13. 创新总结与展望

### 13.1 核心创新价值

`minicoding-rs` 的创新围绕一条主线：**「不信任 LLM 输出，约束执行权永远在 Rust Runtime 一侧」**。这条主线串联起多个维度的创新，形成系统性的技术价值：

| 维度 | 核心价值 |
|------|---------|
| 架构 | 多 crate workspace + 零实现 core 让运行时可嵌入、可替换、可扩展 |
| 安全 | L0 硬约束 + 两道防线让 AI Coding 助手具备生产级安全 |
| 上下文 | 4 级压缩管道 + 熔断 + 预测性让长会话稳定不 Thrash |
| Agent 循环 | 并行/串行分桶 + 类型化子 Agent + worktree 隔离让并行安全高效 |
| 可观测性 | OTel 一等公民让全链路可追踪、延迟可定位 |
| 扩展 | 10 类 Hook + MCP 双向 + Extension SDK 让生态可扩展 |
| 会话 | Event Sourcing + Parent-UUID 链让会话可追溯、可分叉 |
| 前端桌面 | 全 Rust 工具链 + Tauri + ts-rs 让前端现代化且类型安全 |
| AI 辅助开发 | L0/L1/L2 + AGENTS.md 双约束让 AI 助手写代码有规范 |

### 13.2 创新的核心主线

```
┌──────────────────────────────────────────────────────────────────────────┐
│          「不信任 LLM 输出，约束执行权在 Rust Runtime 一侧」                │
└──────────────────────────────────────────────────────────────────────────┘
                                   │
            ┌──────────────────────┼──────────────────────┐
            ▼                      ▼                      ▼
       安全维度                 上下文维度              Agent 循环维度
       • L0 硬约束              • 压缩熔断              • 权限门强制闭合
       • 两道防线               • 不被 LLM 绕过         • 工具执行前校验
       • 决策/交互分离           • 状态机判定            • 审计落盘
            │                      │                      │
            └──────────────────────┼──────────────────────┘
                                   │
                                   ▼
                    可观测性维度（OTel 独立于 LLM 声明）
                                   │
                                   ▼
                    扩展机制维度（Hook/MCP 不绕过 L0）
                                   │
                                   ▼
                    会话管理维度（Event Sourcing 不可变事件流）
                                   │
                                   ▼
                    前端桌面维度（全 Rust 工具链 + Tauri）
                                   │
                                   ▼
                    AI 辅助开发维度（L0/L1/L2 + AGENTS.md 双约束）
```

### 13.3 未来创新方向

基于当前创新基础，未来可探索的方向：

| 方向 | 描述 | 依赖 |
|------|------|------|
| fork 零拷贝 | Event Sourcing 下 fork 仅记录「从 seq 重放」，不复制前缀消息 | `design.md` §25.9 |
| 压缩边界 Compacted 事件 | `CompactionApplied` 事件显式记录压缩边界，投影器跳过旧事件 | `design.md` §25.9 |
| side-chain 事件树 | 子 Agent 事件 `parent_event_id` 指向 `TaskSpawned`，天然事件树 | `design.md` §25.9 |
| Auto-Review 子代理 | 独立小模型评估工具调用风险，自动批准低风险（参考 Codex Guardian） | `security.md` §8.9 |
| 多视图投影 | 同一 EventStore 投影出「完整历史」/「当前窗口」/「审计视图」 | `design.md` §25.9 |
| 网络代理细粒度策略 | `network_proxy` allowlist/denylist 域名级控制 | `security.md` §11 |
| Windows 沙箱成熟度 | 受限令牌 + DACL + 防火墙组合方案 | `security.md` §12 |
| DNS 重绑定防护 | 二次校验解析后到连接前 IP 变化 | `security.md` §5.1 |
| HMAC 审计签名 | 审计记录带 HMAC 签名防篡改 | `security.md` §7.2 |
| Tauri mobile | iOS/Android 支持 | `m9-design.md` §1.3 |

### 13.4 创新的可持续性

创新的可持续性依赖三条保障：

1. **约束写入规范**：创新点（如 L0 硬约束、单向依赖、零实现 core）写入 `AGENTS.md`/`rules.md`，AI 助手写代码时遵守，代码审查强制；
2. **实现层强制**：L0 约束落在 policy/sandbox/hooks 等真实代码点（见 `rules.md` §8 对照表），CI 回归测试锁死行为；`doctor --security`（`security.md` §16）检查运行期配置；
3. **CI 门禁**：`cargo fmt --check`/`clippy`/`test`/`audit`/`deny` 全绿门禁，约束不被无声破坏。

这三条保障让创新不仅是「当前状态」，而是「持续约束」——后续演进必须评估对既有创新的影响，避免无意破坏。

---

## 附录 A：创新点与约束映射

| 创新点 | 相关约束 | 实现位置 |
|--------|---------|---------|
| L0 硬约束 | C-01..C-07、C-21..C-24、C-26..C-30 | `rules.md` §2、`security.md` §2 |
| 两道防线 | C-22 | `security.md` §8 |
| 决策与交互分离 | C-01 | `design.md` §9、`security.md` §2.1 |
| 内置黑名单最高 | C-02、C-21 | `security.md` §2.3、`hooks.md` §4 |
| AGENTS.md 不可自主编辑 | C-23 | `design.md` §8.6、`security.md` §9.2 |
| 凭证隔离 | C-04 | `security.md` §6、§10 |
| 压缩熔断 | C-29 | `design.md` §3.6 |
| 沙箱拒绝熔断 | C-30 | `security.md` §8.8 |
| asyncRewake 不越权 | C-26、C-32 | `hooks.md` §11 |
| Auto memory 不越权 | C-27 | `design.md` §8.7 |
| FileChangeJournal 不绕过权限 | C-28 | `design.md` §17 |
| 任务工具增量语义 | C-31 | `design.md` §18 |
| MCP project 作用域批准 | C-24 | `design.md` §19.4 |
| MCP 只读性据 schema | C-25 | `design.md` §19.3 |

## 附录 B：引用文档索引

| 文档 | 引用内容 |
|------|---------|
| `README.md` | 项目概览、四形态前端、项目结构 |
| `AGENTS.md` | 开发约束、crate 结构、依赖方向、零实现 core、前端规范 |
| `docs/design.md` | Agent 循环、上下文管理、子 Agent、Plan 模式、Event Sourcing、Parent-UUID 链 |
| `docs/security.md` | 威胁模型、权限模型、路径沙箱、OS 沙箱、凭证管理、审计 |
| `docs/rules.md` | L0/L1/L2 约束模型、C-01..C-35、约束自检清单 |
| `docs/tech-stack.md` | 技术选型、依赖治理、备选方案权衡 |
| `docs/architecture.md` | 分层架构、组件协作、横切关注点 |
| `docs/hooks.md` | 10 类 Hook 事件、asyncRewake、安全约束 |
| `docs/m9-design.md` | Web 前端、Tauri 桌面、全 Rust 工具链 |
| `docs/features.md` | 功能清单（204 项）、优先级映射、依赖链 |
