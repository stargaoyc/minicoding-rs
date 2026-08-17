# DeepSeek Harness 对比报告（M9 调研）

> 参考项目：DeepSeek Harness（`deepseek-ai/deepseek-harness`，简称 dsh）
> 调研日期：2026-08-17
> 本文是 minicoding-rs 与 dsh 的横向对比，重点提炼可借鉴的工程实践与设计决策。

---

## 1. 项目定位对比

| 维度 | dsh | minicoding-rs |
|------|-----|---------------|
| 语言/生态 | TypeScript（Node），Cordis 插件体系 | Rust 2024，Cargo workspace（18 crates） |
| 核心形态 | Agent 能力评测/研究 harness（web-app 可视化） | 终端 AI Coding 助手（多前端接入） |
| 运行时约束 | 少（评测场景，安全模型轻） | L0/L1/L2 三层约束（C-01..C-35，实现层强制） |
| 扩展机制 | Cordis 插件（Everything is a Plugin） | `minicoding-extension-sdk`（Extension trait + Registrar） |
| 会话模型 | append-only `SessionEvent` 流 | JSONL 存储 + `Message`/`Task`/`ToolCall` 数据模型 |
| 多前端 | web-app + headless + CLI | CLI / TUI / Web / Desktop / LSP / ACP / NDJSON / MCP server |

两者定位不同：dsh 偏**评测与研究**（Trajectory 回放、jsonrpc-agent 基准），
minicoding-rs 偏**工程化编程助手**（权限沙箱、审计、多协议接入）。可比的是
**架构决策**与**工程实践**，非功能一一对应。

---

## 2. 架构对比

| 设计点 | dsh | minicoding-rs | 评述 |
|--------|-----|---------------|------|
| 核心抽象 | `LlmAdapter` trait（`stream()` + `GenerateOptions` + `resolveModel()`） | `LlmProvider` trait + `Tokenizer`（core 定义，providers 实现） | 同构；dsh 的 `resolveModel()` 运行时解析模型名值得借鉴 |
| 事件模型 | append-only `SessionEvent` 流（会话即事件序列） | `EventBus` 广播（`Token`/`MessageAppended`/`TurnEnd`…）+ JSONL 落盘 | dsh 的事件流**可重放**（Trajectory 视图）；minicoding 有 `--replay` 但事件未序列化为统一回放流 |
| 错误契约 | `LlmError` 稳定错误码（客户端可编程处理） | `LlmError`（thiserror，`Into<RuntimeError>`） | dsh 的**稳定错误码**（而非结构化变体）对跨语言客户端更友好 |
| 扩展 | Cordis 插件（组合式，装饰/替换任意组件） | Extension trait + Hook 协议（子进程 Hook） | dsh 插件可**运行时热装**；minicoding 扩展编译期注册 |
| 模型路由 | 多模型 profile、`resolveModel()` 动态解析 | provider 覆盖（CLI > env > config.toml）、small provider 摘要 | dsh 的模型路由支持**按任务类型选模型**（Standard/Code/Minimal/Creator 模式） |
| 前端 | web-app（React）+ headless | 多形态（见 §1） | minicoding 前端矩阵更广 |
| 评测 | jsonrpc-agent 基准、Trajectory 回放 | 集成测试 + wiremock（不连真实服务） | dsh 的**轨迹回放**可作为 minicoding `--replay` 的增强方向 |

---

## 3. 值得借鉴的实践（改进建议）

### 3.1 Trajectory 可回放事件流（高价值）

dsh 的 append-only `SessionEvent` 日志支持**逐帧回放**（web-app Trajectory 视图），
对调试 Agent 行为、复现 bug、评测有直接价值。

minicoding 现状：`~/.minicoding/sessions/` JSONL 已记录消息与工具结果，`--replay`
可重放，但事件粒度粗（无 token 级/工具参数级逐帧视图）。

建议（可选，M9 之后）：
- 在 JSONL 中补充 `tool_args`/`permission_decision` 等字段（审计已有，扩展一行）；
- 提供 `minicoding doctor --trajectory <session>` 输出逐帧事件序列（终端可读版 Trajectory）；
- 不新增存储格式：现有 JSONL 已足够承载，只需加一个导出命令。

### 3.2 稳定错误码（中价值）

dsh 的 `LlmError` 用稳定字符串码（客户端 switch 可编程处理），minicoding 用
Rust 枚举变体（`thiserror`）。对 Rust 内部消费枚举更优；但对**协议层客户端**
（Web/Desktop/LSP/ACP 的 wire 层）枚举变体序列化为字符串后无稳定契约。

建议：
- `docs/api.md` 中为 wire 层错误（JSON-RPC error code）建立**稳定错误码表**，
  与 `minicoding-protocol` 的 `CommandError` 对齐（已有 `-32602` 等 JSON-RPC 码，补 minicoding 扩展码）；
- 后续如需跨语言 SDK（Python/TS），错误码表可直接复用。

### 3.3 按任务类型选模型（中价值）

dsh 的 Standard/Code/Minimal/Creator 模式按任务切换模型 profile。minicoding
已有 small provider（L2 摘要）与主 provider 的双模型架构，但**没有"按任务选模型"**
（如"写代码用 code 模型、审计用 reasoning 模型"）。

建议：
- 在 `config.toml` 增加 `[provider.profiles]`（如 `code = { model = "..." }`），
  由 `--provider-name` 或工具类别（shell/fs 类任务）触发切换；
- 低优先级（M9 之后），现有双模型已覆盖主要收益。

### 3.4 插件运行时热装（低价值，架构冲突）

dsh 的 Cordis 插件可运行时装卸；minicoding 的扩展是编译期注册（Extension trait）。
运行时热装与 Rust 的静态类型/安全模型（L0 约束不可被扩展绕过）存在张力，
**不建议**照搬——编译期注册 + 子进程 Hook 已是更安全的等价物。

---

## 4. minicoding 领先 dsh 的点（保持）

| 能力 | 说明 |
|------|------|
| 安全模型 | L0 硬约束（权限/黑名单/路径沙箱/凭证隔离）实现层强制，dsh 无等价物 |
| 审计 | 权限决策全量落 `audit.log`（0600 追加写），dsh 无 |
| 沙箱 | landlock/Seatbelt/Job Object 三级 OS 沙箱 + 拒绝熔断（C-30） |
| 多协议接入 | LSP/ACP/NDJSON/MCP server 等 8 种前端形态，dsh 仅 web/headless/CLI |
| 上下文管理 | 4 级压缩 + 熔断状态机（C-29），dsh 无 |
| Hook 协议 | 子进程 Hook + asyncRewake，dsh 无 |

---

## 5. 结论

- **架构层面**：两者核心抽象同构（trait + 事件 + 多前端），minicoding 的
  Rust 类型系统 + L0 约束在**安全与正确性**上显著领先；
- **工程实践**：dsh 的 Trajectory 回放与稳定错误码值得借鉴（§3.1/§3.2），
  建议纳入 M9 之后的增强路线；
- **不建议模仿**：插件运行时热装（与安全模型冲突，§3.4）；
- **后续动作**：§3.1（trajectory 导出命令）与 §3.2（wire 错误码表）可排入
  roadmap，工作量小、收益明确。
