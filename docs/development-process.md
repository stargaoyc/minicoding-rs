# 项目开发过程文档

> **文档性质**：本文是 `minicoding-rs` 的项目开发过程纪实文档，记录从立项规划到 M9 里程碑的完整演进历程、关键设计决策、AI 辅助开发实践、测试与质量保障建设、遇到的关键挑战与复盘反思。
>
> **与其它文档的关系**：本文是**过程性记录**，不替代任何规范性文档。里程碑范围以 [`roadmap.md`](./roadmap.md) 为准、任务粒度以 [`dev-plan.md`](./dev-plan.md) 为准、功能总账以 [`features.md`](./features.md) 为准、开发约束以 [`../AGENTS.md`](../AGENTS.md) 为准、技术选型以 [`tech-stack.md`](./tech-stack.md) 为准、设计机制以 [`design.md`](./design.md) 为准。本文仅做"如何走到今天"的回顾性梳理，引用以上文档时使用相对路径。
>
> **记录时点**：M0–M9 已交付（M9 为低优先级可选里程碑，已实现 W-01..W-10）。审查报告 [`review-report.md`](./history/review-report.md) 结论为"通过"。

---

## 目录

- [1. 项目启动与规划](#1-项目启动与规划)
- [2. 里程碑演进历程](#2-里程碑演进历程)
- [3. 关键设计决策记录](#3-关键设计决策记录)
- [4. AI 辅助开发实践](#4-ai-辅助开发实践)
- [5. 测试与质量保障](#5-测试与质量保障)
- [6. 文档体系建设](#6-文档体系建设)
- [7. 遇到的关键挑战与解决](#7-遇到的关键挑战与解决)
- [8. 项目复盘与反思](#8-项目复盘与反思)
- [9. 未来展望](#9-未来展望)

---

## 1. 项目启动与规划

### 1.1 项目立项背景

`minicoding-rs` 是一个 Rust 实现的终端 AI Coding 助手，定位参考 Claude Code 与 Codex CLI，提供 Agent 循环、工具系统、权限沙箱、上下文管理、MCP 接入、会话审计等能力。立项的核心理由有三：

1. **Rust 一等公民的 AI Coding 助手缺位**。2025 年终端 AI Coding 工具多以 Node.js（Aider）或 Python 实现，Rust 生态缺少一个从 Agent 循环、权限模型到 OS 沙箱完整自洽的参考实现。项目希望以 Rust 的内存安全、零成本抽象、强类型契约填补这一空白。
2. **L0 硬约束必须在实现层强制**。市面上部分助手的安全边界依赖系统提示词与大模型自觉，存在被越权绕过的风险。项目从第一天就确立：副作用必须经权限、内置黑名单不可覆盖、路径不可越界、凭证不可外泄等 L0 硬约束（[`rules.md`](./rules.md) C-01..C-30）必须在 Rust 实现层强制，不依赖 LLM 自觉。
3. **可观测性内建**。Agent 循环是黑盒，调试困难。项目将 OpenTelemetry（OTel）作为一等公民从 M0 接入，session/turn/llm_call/tool_call/permission/hook.run/mcp.call 全链路打 span，提供可观测性基座。

### 1.2 参考产品分析

立项阶段对三款主流产品做了功能与架构对标：

| 产品 | 借鉴点 | 项目取舍 |
|------|--------|---------|
| **Claude Code** | Plan 模式（双重只读强制 + 预批准缓存）、`fs.multiedit` 原子性、`task.spawn` 类型化子 Agent、Auto memory、10 类 Hook 生命周期、AGENTS.md 项目记忆 | 全盘借鉴并 Rust 化；Auto memory 增加指令性内容降级 `Ask`（C-27）防越权通道 |
| **Codex CLI** | OS 沙箱（Landlock/Seatbelt）、审批模式（Untrusted/OnFailure/OnRequest/Never）、沙箱拒绝检测与升级流、presets 预设 | 沙箱从"后续可选"升级为一等公民并前置到 M4；用 `sandbox-run` 统一跨平台 API 而非自研胶水 |
| **Aider** | 文件改动回滚（`/undo`）、Git 集成（diff/apply）、64KB 窗口会话列出 | `/undo` 限定会话内 operation 级、内存不落盘、冲突检测不强行覆盖（C-28） |

参考结论记录于 [`roadmap.md`](./roadmap.md) 顶部"重构说明"：将沙箱、Hooks、MCP、Plan 模式、文件回滚等"扩展与安全"能力从原 M4/M7 前置到独立里程碑，避免 MVP 形成后再大改权限/工具边界。这一前置决策直接塑造了 M0–M9 的里程碑顺序。

### 1.3 设计原则确立

立项即在 [`architecture.md`](./architecture.md) §1 确立六条设计原则，作为后续所有决策的锚点：

1. **分层解耦**：Frontend → Orchestrator → Capability，每层只依赖下层抽象接口。
2. **抽象优先**：所有可替换能力（LLM、工具、存储、权限）以 trait 定义在核心层，实现在外围 crate。
3. **单向依赖**：依赖方向自上而下，核心层不反向依赖 frontend，禁止循环依赖。
4. **显式状态**：所有可变状态集中在 `Session`/`Runtime`，组件无全局可变状态。
5. **失败可恢复**：任何一轮 Agent 循环失败不应损坏会话状态，可从持久化日志恢复。
6. **可观测性内建**：每个跨进程/跨组件边界都打 span，trace 全链路贯通。

这六条原则在 [`AGENTS.md`](../AGENTS.md) §3 架构设计规范中固化为可执行约束（单一职责、依赖方向、trait 集中、零实现 core、平台隔离、不自研能用库的）。

### 1.4 技术栈选型决策

技术栈在 [`tech-stack.md`](./tech-stack.md) 锁定，遵循三条原则：优先成熟、社区活跃的 crate；Rust 一等公民；重依赖隔离不污染 core。核心选型：

| 维度 | 选型 | 关键理由 |
|------|------|---------|
| 语言/edition | Rust 2024，MSRV 1.99 | `async fn in trait` 已稳定；用 `trait-variant` 生成 Send 变体供 `Arc<dyn Trait>` 持有 |
| 异步运行时 | `tokio` | 生态广度胜出，不混用 `async-std` |
| HTTP | `reqwest` + rustls-tls | 不裸用 hyper；rustls 避免 OpenSSL 依赖与许可证风险 |
| 序列化 | `serde` + `serde_json` + `toml` | Rust 生态亲和度 |
| 错误 | 库 `thiserror` / 边界 `anyhow` | 主流、低学习成本 |
| 路径 | `camino::Utf8PathBuf` | UTF-8 保证，避免 OS 字符集边界 |
| 日志/追踪 | `tracing` + OpenTelemetry OTLP | OTel 一等公民，M0 接入 |
| 沙箱 | `sandbox-run` + `landlock` + `libseccomp` | 不自研 ruleset/profile 胶水 |
| MCP | `rmcp` 2.2 官方 SDK | 不自研 stdio/http 薄封装 |
| TUI | `ratatui` + `crossterm` | 更现代、无状态刷新模型 |
| 桌面 | Tauri 2.x | Rust 实现，体积/内存/安全均优于 Electron |
| 前端工具链 | oxlint / oxfmt / Vite Rolldown / Tailwind v4 Oxide | 全 Rust 工具链一致性 |

选型权衡完整记录于 [`tech-stack.md`](./tech-stack.md) §13 备选方案与权衡记录表，每个决策点都列出备选与权衡理由。

---

## 2. 里程碑演进历程

项目从 M0 到 M9 共 10 个里程碑，M0–M8 合计 74 个 task、81 理想人日（[`dev-plan.md`](./dev-plan.md) 附录统计），M9 新增 8 个 task、12 人日。功能总账 182 项（[`features.md`](./features.md) 统计），约束 35 条（C-01..C-35）。

### 2.1 M0 — 骨架与基础设施（3 人日 / 9 task）

**范围**：搭建 Cargo workspace + 14 crate 骨架（M0 落地 14 个核心 crate，另 3 个在 M5/M6/M8 启用时补齐 `lib.rs`，合计 17）、公共依赖统一管理、CI 六门禁、OTel 初始化模板、`MINICODING_HOME` 路径解析、`minicoding-sandbox` 骨架 + `NoopDriver`、测试基础设施（`tests/common/` 共享 stub）。

**交付物**：
- `cargo build --workspace` 通过，含平台条件依赖在非 Linux 平台不编译。
- `cargo run -p minicoding-cli -- --help` 输出帮助。
- 设置 `OTEL_EXPORTER_OTLP_ENDPOINT` 后启动，本地 OTLP collector 可见 `minicoding` resource。
- CI 全绿（fmt + clippy -D warnings + test + audit + deny + coverage 工具链就位）。
- `crates/minicoding-core/tests/common/` 共享测试工具（`NoopMcpClient`/`NoopHookRegistry`/`StubJournal` 等 stub）。

**验收标准**：14 crate 骨架编译通过、CI 6 门禁就位（Linux only matrix）、OTel resource 可观测、共享测试工具就位（T-M0-9）。

**实际遇到的问题**：M0 阶段 coverage 门禁仅验证工具链就位不阻塞合并（无业务代码），M1 起阻塞。这一过渡设计避免了"空仓库被 80% 覆盖率门禁卡死"的窘境。

### 2.2 M1 — MVP 单轮 CLI（12 人日 / 9 task）

**范围**：core 数据模型与 trait 定义、`Runtime` + 单轮 Agent 循环、OpenAI 兼容 provider（SSE 解析 + 工具调用增量）、`tiktoken-rs` Tokenizer、4 个只读工具（`fs.read`/`list`/`glob`/`grep`）、应用层路径沙箱 `sandbox_path`、JSONL 会话日志、CLI 单次模式 + 流式渲染、配置 last-known-good 回退与 `env:VAR:-fallback` 语法。

**交付物**：`minicoding "读取 src/main.rs 并解释"` 能流式输出并实际读取文件；越界路径（`../../etc/passwd`）被 `sandbox_path` 拒绝并返回 `PathEscaped`；非 TTY 环境禁用 spinner/颜色。

**验收标准**：4 个只读工具可用、OpenAI SSE 流式首 token < 2s（网络除外）、JSONL 崩溃恢复测试通过、单测覆盖率 ≥ 80%。

**实际遇到的问题**：
- **SSE 解析边界 case 多**：分片、空 data、`[DONE]` 等场景需用 `wiremock` 录制真实响应做 fixture 充分测试。
- **OpenAI 工具调用分片**：`DeltaAccumulator` 需覆盖分片聚合边界，否则工具调用 JSON 拼接失败。
- **配置解析失败的容错**：引入 last-known-good 回退机制——解析成功时原子写入 `~/.minicoding/.last-known-good.toml`，解析失败时回退，避免坏配置导致无法启动。

### 2.3 M2 — 完整 Agent 循环 + 应用层权限（12 人日 / 9 task）

**范围**：完整多轮 Agent 循环（停止条件、防死循环）、工具并行/串行分桶调度（`SideEffect::None` 并行，其余严格串行）、写文件组（`fs.write`/`edit`/`multiedit`/`delete`）、`shell.run`（超时+截断+黑名单）、`PermissionPolicy` + `PermissionPrompter` 双抽象、内置安全黑名单、`audit.log` 审计落盘、`EventBus`、OTel span 埋点、`[provider.small]` 独立小 LLM 配置脚手架。

**交付物**：`minicoding "把 utils.rs 里的 foo 改名为 bar"` 能完成读取→编辑→验证闭环；同轮多个只读工具并发执行，写/shell 严格串行（trace 中可见时序）；`Ctrl-C` 不丢已生成消息（已落盘 JSONL 可恢复）；权限决策 100% 落 audit.log。

**验收标准**：max_tool_iters=50 防死循环生效、并行/串行调度 OTel span 时序可验证、audit.log 含 Allow/Deny/Ask 全类型记录、criterion 基准建立（Agent 循环开销基线）。

**实际遇到的问题**：
- **edit 唯一性冲突处理**：多处匹配时返回清晰错误并建议增大上下文，而非静默改第一个。
- **并行工具调用消息顺序**：完成顺序乱序时需严格按 `call_id` 关联 result，不依赖完成顺序。
- **权限交互在非 TTY 的边界**：`NonInteractivePrompter` 显式策略化（默认 deny 副作用工具），避免非 TTY 环境假死。

### 2.4 M3 — 上下文、持久化与记忆（10 人日 / 10 task）

**范围**：`ContextManager` + token 预算 + 权重模型、4 级压缩管道（裁剪→摘要→滚动→硬截断）、压缩熔断 + 防 Thrash + 降级链、会话索引 + 跨进程文件锁、长期记忆双文件 + mtime 缓存、会话摘要 + 失败降级链、`ProjectDocLoader` + AGENTS.md 分层加载、任务管理工具（`task.create`/`update`/`list`）、`memory.write` + Auto memory、`--resume`/`--replay` + session 子命令。

**交付物**：长会话（>上下文窗口）能自动压缩且不破坏连贯性；长期记忆文件未变更时连续多轮 `build_chat_request` 不产生重复 IO/分词（trace 中 compress span 计数稳定）；`--resume <id>` 恢复后可继续提问，`--replay` 复现历史工具调用且默认禁副作用；AGENTS.md 从 repo_root 到 cwd 逐级加载并注入 system。

**验收标准**：压缩熔断 fail_threshold=3 生效、降级链 4 级全覆盖测试、proptest 压缩管道不变量测试通过、--resume/--replay 集成测试通过、AGENTS.md 分层加载覆盖 repo_root→cwd 全路径。

**实际遇到的问题**：
- **压缩质量**：摘要 prompt 需调优；提供 `compress=off` 兜底开关。
- **摘要 LLM 调用成本**：仅在阈值触发，且可配置用小模型降本（`[provider.small]`）。
- **记忆双文件一致性**：`long_term.md` + `index.json` 用原子 rename + 启动时索引校验/重建保证一致。
- **AGENTS.md override 语义复杂**：fallback（`CLAUDE.md`/`.cursorrules`）与 override 组合需充分测试。

### 2.5 M4 — 安全沙箱与 MCP（8 人日 / 11 task）

> **范围调整**：参考 CC/Codex 后将原 M5 的 MCP client 与 Journal/`/undo` 前置到 M4，与 OS 沙箱同步交付——MCP 远程工具与文件回滚都依赖沙箱作为安全底线，同里程碑交付避免 M5 出现"有 Hook 无沙箱兜底"的窗口期。

**范围**：Linux Landlock + libseccomp 驱动、`ExternalSandbox` 策略、pre-main 进程硬化 + VCS 目录保护、`SandboxPolicy` 四模式 + `ApprovalMode` + 预设、沙箱拒绝检测与升级流 + 拒绝熔断、`shell.run`/`fs.write` 受沙箱约束、`McpClient` + rmcp client（stdio）、MCP 工具命名 + 包装 + project 批准流、`FileChangeJournal` + `/undo`、`exec`/`doctor`/`mcp` 子命令、`CallbackPrompter` + keyring + 脱敏。

**交付物**：`--sandbox read-only` 下任何写/网络在内核被拦（Linux），audit.log 记录拒绝；沙箱拒绝（Landlock EPERM）被识别并升级为权限请求而非裸错误；`--preset full-access` 启动时打 red 警告并要求显式确认；MCP stdio server 能连接、`list_tools`、`call`；含 `.minicoding/mcp.json` 的仓库首次进入时逐 server 弹窗批准，结果落 `mcp_choices.toml`；`/undo` 能回滚最近一次 operation 的文件改动。

**验收标准**：Landlock EPERM 拦截越界写可验证、沙箱拒绝熔断 3/5 次阈值生效、`--preset full-access` red 警告 + 二次确认生效、MCP project 作用域首次批准流测试通过、`/undo` 冲突检测（mtime/hash 比对）测试通过。

**平台优先级**：M4 仅交付 Linux（Landlock+libseccomp），macOS/Windows 降级 NoopDriver（M5+ 补 macOS Seatbelt，M6+ 补 Windows Job Object）。

**实际遇到的问题**：
- **Landlock 旧内核不支持**：编译期检测 + 运行时 `landlock_available()` 探测 → 不支持降级 `NoopDriver` + warn，`is_hardened()` 如实返回 false。
- **沙箱拒绝与普通错误混淆**：建立 denial 签名库（stderr 模式 + errno），denial 走升级流而非裸错误。
- **MCP 恶意仓库植入**：project 作用域 server 植入风险，引入首次批准流（`mcp_choices.toml` 按项目路径指纹分桶，原子写 `.tmp` + `rename`）。

### 2.6 M5 — 扩展机制：Hooks + Plan + 子 Agent（12 人日 / 8 task）

**范围**：`Hook` trait + `HookRegistry` + 10 类事件、`ScriptHook` 适配器 + `on_hook_error` 策略、`PreToolUse` 拦截/改写 + `PostToolUse` 后处理 + `PermissionRequest` 短路、6 个内置示例 Hook（fmt-on-write / auto-approve-tests / block-secrets / git-status-inject / backup-before-compact / test-on-stop）、`asyncRewake` 异步唤醒、Plan 模式 + `plan.exit`、类型化子 Agent（Explore/Plan/General/Custom）+ `task.spawn`、macOS Seatbelt 沙箱补齐、Extension SDK + Prompt 管道（9 个 `PromptContributor`）。

**交付物**：`PostToolUse(fs.write|fs.edit)` Hook 能触发 `cargo fmt`；`PreToolUse` Hook `deny` 能阻断工具调用；Hook 对内置黑名单 `Deny` 的 `allow` 被忽略（L0 不破）；Plan 模式下非只读工具被硬门 `Deny`；`task.spawn` 能启动子 Agent 并隔离上下文；macOS CI matrix 启用，`--sandbox read-only` 在 macOS 下写被 Seatbelt 拦。

**验收标准**：10 类 Hook 事件全覆盖测试、Hook L0 不覆盖（黑名单 Deny 时 allow 被忽略）测试通过、asyncRewake 3 并发上限 + 超时 kill 测试通过、Plan 模式硬门 + plan.exit 预批准缓存测试通过、macOS CI matrix 启用。

**实际遇到的问题**：
- **Hook 串行链路影响延迟**：默认超时 30s，`on_hook_error=continue` 兜底；`asyncRewake` 把长时任务转后台。
- **Plan 预批准与权限矩阵交互复杂**：ExitPlanMode 后的 Verdict 解析需充分测试。
- **子 Agent 上下文隔离不彻底**：独立 ContextManager + 共享 trait 对象，单测验证隔离；Explore/Plan 子 Agent 跳过 AGENTS.md 加载。
- **macOS Seatbelt profile 语法差异**：按 macOS 版本测试 profile 生成。

### 2.7 M6 — 多 Provider 与健壮性（6 人日 / 5 task）

**范围**：Anthropic 实现（专有事件流、system prompt 分离）、Ollama 实现（NDJSON 流）、统一重试/限流/超时装饰器、`rmcp` 完整客户端（http + OAuth）、错误分类与恢复策略、`--provider`/`--model` 覆盖、`minicoding-protocol` JSON-RPC 2.0 wire types 独立 crate、ACP stdio 适配器脚手架、配置热更新（`ConfigWatcher` + `Event::ConfigChanged`）。

**交付物**：Anthropic 模型可正常流式 + 工具调用；限流自动退避重试，超时优雅取消；三家 provider（OpenAI/Anthropic/Ollama）行为一致（同一 prompt 产出合法消息序列）；`rmcp` http MCP server 可连接（含 bearer token 鉴权）。

**验收标准**：三家 provider 同一会话行为一致测试通过、429 Retry-After 退避重试测试通过、rmcp http+OAuth 连接测试通过、Windows CI matrix 启用（平台优先级 M6+）。

**实际遇到的问题**：
- **Anthropic 事件流与 OpenAI 差异大**：抽象层充分隔离，`content_block_start`/`content_block_delta`/`message_stop` 解析容错。
- **`rmcp` OAuth 流程复杂**：保留 `stdio_only` 作为 fallback；锁定 patch 版本避免 API 漂移。

### 2.8 M7 — TUI（10 人日 / 4 task）

**范围**：`ratatui` + `crossterm` 基础框架、流式 Markdown 增量渲染、自研 `InputState`（不引入 `reedline`——与 ratatui 全屏 alternate screen 模式冲突）、多会话侧栏、`TuiPrompter` 非阻塞权限弹窗、工具面板 + 任务面板、主题配色、非 TTY 降级。

**交付物**：全屏交互流畅（< 16ms 渲染，60fps）；工具调用实时进度可见；任务面板同步更新；权限弹窗非阻塞主循环（`TuiPrompter` 挂起工具调用，UI 处理后回传 `Decision`）；流式 Markdown 增量渲染不闪烁。

**验收标准**：渲染帧率 < 16ms（criterion 基准）、流式 Markdown 增量解析无闪烁、TuiPrompter 非阻塞回传测试通过。

**实际遇到的问题**：
- **流式 Markdown 重绘性能**：增量解析 + 脏区刷新 + CSS `contain: layout style` 隔离重绘范围。
- **TuiPrompter 非阻塞复杂度**：点对点交互与 broadcast 事件总线冲突，`TuiPrompter` 独立通道挂起工具调用，UI 处理后回传 `Decision`。
- **TUI 与 Runtime 线程模型**：用 `current_thread` + `LocalSet` 桥接非 `Send` future，独立线程跑 Runtime，UI 线程通过 channel 收发事件。

### 2.9 M8 — SDK 与 Server（8 人日 / 9 task）

**范围**：SDK `Client` + `ClientBuilder` + 高层 API、`minicoding serve` HTTP/JSON-RPC server、`serve --as-mcp-server` 把自身工具暴露为 MCP server、stdin/stdout NDJSON 协议、向量检索（`@memory` BM25）+ web/git/shell 高级工具组、`cargo dist` 跨平台二进制 + 三渠道分发、ACP stdio 适配器完善、LSP stdio 适配器（基于 `tower-lsp`）、`LspPrompter` + codeAction。

**交付物**：`Client::ask` 可在第三方 Rust 项目运行；`serve` 模式可被 curl 调用；MCP server 可被 Claude Desktop 等客户端发现并使用；`serve --lsp` 可被 VS Code/Neovim 连接，能发送 prompt 并接收流式 token，权限确认通过 `window/showMessageRequest` 弹窗。

**验收标准**：SDK `Client::ask` 在第三方 Rust 项目可运行、`serve` HTTP 端点可 curl 调用、MCP server 可被 Claude Desktop 发现、`serve --lsp` 可被 LSP 编辑器连接、跨平台二进制（cargo dist）三平台产出。

**实际遇到的问题**：
- **协议稳定性**：标 `experimental` 直到反馈收敛。
- **LSP 协议方法集庞大**：用 `tower-lsp` 提供类型安全派发与生命周期管理，与 ACP 共享 `minicoding-protocol` wire types，仅语义映射层不同。
- **SSE 断线重连**：cursor 恢复（E-13）+ 前端重连后请求缺失区间；broadcast 溢出时发 `RehydrateRequired` 信号，客户端重拉 snapshot。

### 2.10 M9 — Web 与桌面（低优先级 / 12 人日 / 8 task）

> **定位**：M9 为可选里程碑，优先级低于 M5–M8。在 M8 的 HTTP/SSE JSON-RPC server 基础上，提供浏览器可访问的 Web 前端与原生桌面应用（Tauri 壳），降低非终端用户的上手门槛。

**范围**：新增 `crates/minicoding-web/`（React 19.2 + TypeScript 7.0 + Vite 8.1 + React Compiler，独立 `package.json`）与 `crates/minicoding-desktop/`（Tauri 2.x 壳）；前端核心能力（多会话面板、流式 token 渲染、工具调用面板、权限确认弹窗、任务面板、压缩/熔断可视化、Hook 日志、主题切换、响应式布局）；`minicoding-server` 增强（静态资源托管 `--web`、CORS `--cors-origin`、SSE cursor 恢复）；桌面端特性（系统托盘、全局快捷键、OS keyring 凭证、自动更新）；全 Rust 工具链（oxlint/oxfmt/Vite Rolldown/Tailwind v4 Oxide）。

**交付物**：`minicoding serve --bind 127.0.0.1:8080` 启动后浏览器访问能完整对话/工具调用/权限确认；Tauri 桌面应用在 macOS/Windows/Linux 三平台可构建，体积 < 15MB；前端 Lighthouse 性能评分 ≥ 90；oxlint + oxfmt + tsc 全绿；凭证经 OS keyring 存储不出现在前端代码/日志/网络请求中。

**验收标准**：Web 前端可对话/工具调用/权限确认、三平台桌面应用可构建、Lighthouse ≥ 90、全 Rust 工具链构建。详见 [`m9-design.md`](./m9-design.md)。

**实际遇到的问题**：
- **React Compiler 仍 RC**：评估稳定性，必要时回退手写 `useMemo`/`React.memo`。
- **Tauri 系统依赖**：`minicoding-desktop` 的 `desktop` feature 依赖 Tauri，需 webkit2gtk/glib 系统库，CI 与 pre-commit 均排除该 crate，单独在 `desktop` job 安装依赖后编译。
- **前端 XSS 防护**：CSP 严格 + React 默认转义 + DOMPurify 兜底，`prompt_id` 后端生成防伪造。

---

## 3. 关键设计决策记录

本节记录项目过程中具有长期影响的关键设计决策及其理由，决策点与 [`tech-stack.md`](./tech-stack.md) §13 权衡记录表对应。

### 3.1 为什么选 Rust 而非 Node/Python

- **内存安全**：Agent 助手执行 `shell.run`/`fs.write` 等副作用操作，Rust 的所有权与借用检查从语言层面消除内存安全漏洞，权限/沙箱边界不会被缓冲区溢出绕过。
- **零成本抽象**：`async fn in trait` + `Arc<dyn Trait>` 的动态派发无运行时堆分配（配合 `trait-variant` 生成 Send 变体），Agent 循环热路径性能优于 box future。
- **类型契约**：`serde` 序列化 + `ts-rs` 自动生成 TypeScript 类型，前后端契约编译期保证，避免手写双份 DTO 漂移。
- **生态对齐**：沙箱（landlock/libseccomp）、MCP（rmcp）、HTTP（reqwest/rustls）、TUI（ratatui）、桌面（Tauri）均有成熟 Rust 实现，"Rust 一等公民"理念贯穿全栈。

### 3.2 为什么 14→17→18 crate 拆分（而非单体）

- **单一职责**：每个 crate 只负责一类实现（[`AGENTS.md`](../AGENTS.md) §3.1），`minicoding-core` 禁止含任何领域实现逻辑，`minicoding-policy` 不写记忆加载、`minicoding-memory` 不写权限决策。
- **编译并行**：crate 拆分后 cargo 可并行编译，增量编译时间显著缩短。
- **feature gate 灵活**：实现 crate 通过 cargo feature 按需启用（`default = ["memory", "sandbox"]`），用户可裁剪。
- **演进**：M0 落地 14 核心 crate，M5/M6/M8 补齐 `minicoding-protocol`/`minicoding-server`/`minicoding-extension-sdk` 凑齐 17，M9 新增 `minicoding-desktop`（加入 workspace）与 `minicoding-web`（独立 npm）共 18 个 cargo crate + 1 个前端项目。

### 3.3 为什么 trait 定义集中在 core（而非分散）

- **依赖方向干净**：所有领域 trait（`Tool`/`LlmProvider`/`ContextManager`/`PermissionPolicy`/`SandboxDriver`/`Hook`/`Storage`/`Journal`/`McpClient`/`ProjectDocLoader`/`MemoryStore`）在 `minicoding-core` 定义，实现在领域 crate。Runtime 持有 `Arc<dyn Trait>` 不需知道具体实现 crate，依赖方向单向不循环（[`AGENTS.md`](../AGENTS.md) §3.2/§3.3）。
- **可替换性**：trait 集中后，实现 crate 可按 feature gate 替换（如 `NoopDriver` 兜底 `SandboxDriver`），不影响 Runtime 编排。
- **零实现 core**：core 只含数据模型、trait、Runtime 编排、事件总线、配置、OTel、`NoopDriver`，禁止出现压缩算法、黑名单正则、landlock ruleset、rmcp 调用等任何领域实现。

### 3.4 为什么用 sandbox-run 而非自研沙箱

- **跨平台统一 API**：`sandbox-run` 封装 Landlock ruleset 构建、macOS Seatbelt profile 生成、Windows 受限令牌，systemd 风格 API 易用；自研胶水需维护三平台 profile 字符串，易错且维护成本高（[`tech-stack.md`](./tech-stack.md) §13）。
- **不自研能用库的**：[`AGENTS.md`](../AGENTS.md) §3.6 明确"沙箱统一 API 用 sandbox-run，不自研 seatbelt profile + landlock ruleset 胶水"。
- **底层选型**：Linux 用 `landlock`+`libseccomp`（纯 Rust、内核原生无需外部二进制，优于需 SUID 安装的 bwrap）；macOS 用 `sandbox-run` 封装 Seatbelt；Windows 用 `windows` 受限令牌 + Job Object。

### 3.5 为什么用 rmcp 2.2 而非自研 MCP

- **官方 SDK 协议跟进快**：`rmcp` 2.2 对齐 2025-11-25 MCP spec，含 `#[tool]` 宏与 schemars；自实现易落后、维护成本高。
- **不自研薄封装**：[`AGENTS.md`](../AGENTS.md) §3.6 明确"MCP client/server 用 rmcp 2.2，不自研 stdio/http"。
- **演进策略**：M4 先交付 stdio，M6 升级 http+OAuth；保留 `stdio_only` 作为 fallback；锁定 patch 版本监控 API 漂移。

### 3.6 为什么用 reqwest + rustls 而非 hyper/OpenSSL

- **开发效率与控制力平衡**：`reqwest` 封装 hyper，提供高层 API；裸用 hyper 控制力强但开发效率低（[`tech-stack.md`](./tech-stack.md) §13）。
- **rustls 避免 OpenSSL**：rustls 是纯 Rust TLS 实现，避免 OpenSSL 的依赖与许可证风险；[`AGENTS.md`](../AGENTS.md) §2.7 依赖治理要求许可证限 MIT/Apache-2.0/BSD/ISC。
- **feature 最小化**：`reqwest` 只开 `json, rustls-tls, stream` 三个 feature，不引入多余依赖。

### 3.7 为什么用 camino::Utf8PathBuf 而非 std::path::PathBuf

- **UTF-8 保证**：`camino::Utf8PathBuf` 在类型层保证 UTF-8，避免 OS 字符集边界（Windows UTF-16、Unix 不限编码）导致的序列化/日志/JSON 边界 bug。
- **serde 友好**：`Utf8PathBuf` 直接序列化为 String，无需自定义 serializer。
- **[`AGENTS.md`](../AGENTS.md) §2.5 强制**：路径一律用 `camino::Utf8PathBuf` 替代 `std::path::PathBuf`。

### 3.8 为什么 L0 硬约束不可被 Hook 覆盖

- **安全底线**：L0 约束（C-01..C-30）是安全底线，若可被 Hook 覆盖则等于无防线。内置黑名单 `Deny` 必须在 Hook 之前生效，Hook 的 `allow` 对黑名单 `Deny` 无效（C-21）。
- **实现层强制**：[`AGENTS.md`](../AGENTS.md) §5.1 要求 L0 约束在实现层被强制，不能依赖 LLM 自觉或系统提示词。`policy::builtin` 黑名单优先级最高，任何用户配置与 Hook 都无法覆盖。
- **审计可追溯**：Hook 决策（allow/deny/modify_input）落 audit.log（source=hook），协议违规记 source=hook_protocol_violation。

### 3.9 为什么决策与交互分离（PermissionPolicy vs Prompter）

- **架构缺陷修复**：`EventBus` 是 broadcast 仅通知无回复通道，无法承载点对点权限回复。决策（`PermissionPolicy::check` → `Verdict`）与交互（`PermissionPrompter::resolve` → `Decision`）分离，解决 broadcast 无法承载点对点回复的架构缺陷（[`design.md`](./design.md) §9.1）。
- **多前端复用**：`InteractivePrompter`（CLI TTY）/`NonInteractivePrompter`（非 TTY）/`CallbackPrompter`（SDK 闭包）/`TuiPrompter`（TUI）/`LspPrompter`（LSP `window/showMessageRequest`）同构实现同一 trait，Runtime 不感知前端形态。
- **非阻塞**：`TuiPrompter`/`LspPrompter` 挂起工具调用，UI 处理后回传 `Decision`，不阻塞主循环。

### 3.10 为什么用 Event Sourcing

- **天然支持时间旅行调试**：`EventStore`（append-only，不可变，`{id}.events.jsonl`）+ `SnapshotStore` 双写并存，session 是投影层从 EventStore + snapshot 重放得出当前消息序列（[`design.md`](./design.md) §25）。
- **多客户端状态同步**：与 SSE cursor 恢复（E-13）协同，`seq` 单调递增保证事件顺序；`durable_seq` 是已持久化最大 seq，ring buffer 命中时零 IO 重放。
- **审计回放**：`--replay` 不再依赖消息日志而是事件重放，跨会话 fork/merge 自然支持。
- **平滑过渡**：与原 JSONL 消息日志双写并存，新会话同时写消息日志与事件流，旧会话无事件流时回退到消息日志路径。

### 3.11 为什么 OTel 一等公民

- **Agent 循环是黑盒**：调试困难，OTel 全链路 span（session/turn/llm_call/tool_call/permission/hook.run/mcp.call）是可观测性基座。
- **M0 接入**：`tracing` + `tracing-subscriber` + `tracing-opentelemetry` + `opentelemetry-otlp` 从 M0 初始化，支持 `OTEL_EXPORTER_OTLP_ENDPOINT` 环境变量，无后端时降级本地 fmt 日志。
- **span 字段规范**：span 字段命名符合 [`design.md`](./design.md) §15.2，不含凭证（C-04）。
- **子 Agent 传播**：OTel Context 传播，子 Agent span 挂在父 turn span 下，父子关系可见。

### 3.12 为什么 M9 用 Tauri 而非 Electron

| 维度 | Tauri 2.x | Electron |
|------|-----------|----------|
| 体积 | 5–10 MB | 100 MB+ |
| 内存 | 30–50 MB | 100–200 MB |
| 安全 | Rust 内存安全 + CSP 严格 | Node.js 难以管控 |
| IPC | Rust 命令直接调用，无序列化开销 | JSON 序列化 |
| 移动端 | 2.x 支持 iOS/Android | 不支持 |

Tauri 与本项目"Rust 一等公民"理念一致，体积/内存/安全均优于 Electron（[`tech-stack.md`](./tech-stack.md) §4.1.2、[`m9-design.md`](./m9-design.md) §3.3）。桌面端复用 OS keyring（与 CLI `cred.rs` 共享 `KEYRING_SERVICE = "minicoding"`，C-04），凭证不出现在前端。

### 3.13 为什么前端用全 Rust 工具链（oxlint/oxfmt/Vite Rolldown/Tailwind v4）

- **一致性**：oxlint/oxfmt/Vite Rolldown/Tailwind v4 Oxide 均为 Rust 实现，与后端工具链一致，构建/Lint/格式化速度显著优于传统 Node 工具链（[`AGENTS.md`](../AGENTS.md) §8.7）。
- **性能**：Vite Rolldown 构建速度 10x vs webpack，oxlint 50x vs ESLint，oxfmt 20x vs Prettier，Tailwind v4 Oxide 5x vs v3。
- **不混用**：[`AGENTS.md`](../AGENTS.md) §8.7 明确"不引入 ESLint/Prettier，已被 oxlint/oxfmt 替代，混用会导致规则冲突与性能浪费"。
- **CI 对齐**：前端 CI 跑 `oxlint && oxfmt --check && tsc --noEmit && vite build`，与 Rust 侧 `cargo fmt --check && clippy && test` 对齐。

---

## 4. AI 辅助开发实践

### 4.1 AGENTS.md 约束体系建立

项目从第一天就确立"AI 助手写代码也要守规矩"，将开发时约束固化在 [`AGENTS.md`](../AGENTS.md) 项目级文件中，与运行时约束 [`rules.md`](./rules.md) 正交：

| 文件 | 约束对象 | 时机 | 性质 |
|------|---------|------|------|
| `docs/rules.md` | 被 minicoding 驱动的 LLM（运行时模型） | 运行时 | 大模型约束（C-01..C-35），由 Rust Runtime 强制 |
| `AGENTS.md` | 帮我们写代码的 AI 助手（开发时模型） | 开发时 | 助手行为约束，由助手自觉 + 代码审查强制 |

[`AGENTS.md`](../AGENTS.md) 覆盖 9 章：项目概况、Rust 编码规范、架构设计规范、文档更新规范、安全规范、提交与协作规范、AI 助手行为约束、前端开发规范（M9）、快速参考检查清单。关键约束包括：

- **§2.3 不 panic**：非测试代码不用 `unwrap()`/`expect()`，所有可预期错误走 `Result`。
- **§2.6 unsafe 默认禁用**：必须使用时（FFI）需 `// SAFETY:` 注释 + 同级 code review + 范围最小化。
- **§3.4 零实现 core**：core 禁止出现压缩算法、黑名单正则、landlock ruleset、rmcp 调用等任何领域实现。
- **§5.1 L0 约束不可违反**：编写代码时必须确保 L0 硬约束在实现层被强制。
- **§7.1 先读后改**：修改任何文件前必须先用 Read 工具读取，不基于猜测修改未读的文件。
- **§7.5 不创建测试代码除非要求**：默认不写测试，除非用户明确要求或验收标准明确要求测试覆盖。

### 4.2 AI 助手（Claude Code/Trae）在项目中的角色

AI 助手在项目中承担"实现工程师"角色，按 [`dev-plan.md`](./dev-plan.md) 的 task 粒度认领、实现、提 PR。其工作模式：

1. **认领 task**：按 task 的输入/输出/验收标准/依赖字段工作，不自由发挥。
2. **先读后改**：修改前 Read 目标文件理解上下文，不基于猜测修改。
3. **遵循架构纪律**：trait 定义在 core、实现在领域 crate、重依赖 feature gate 隔离、不自研能用库的。
4. **同步文档**：改代码必改文档（[`AGENTS.md`](../AGENTS.md) §4.1），公共 API 变更更新 `docs/api.md`，crate 结构变更更新 `docs/modules.md`。
5. **提交规范**：Conventional Commits + 中文描述，scope 为 crate 名，一个 PR 一个逻辑变更。

### 4.3 AI 编码的边界（L0 约束、安全规范）

AI 助手的编码边界由 [`AGENTS.md`](../AGENTS.md) §7.3"不绕过约束"明确：

- 即使被要求"快速实现""先跑起来"，也不违反 §2-§5 规范。
- 不为"通过测试"而注释掉安全检查、放宽权限、跳过审计。
- 不在代码中留 `TODO: 后面补审计` 等绕过约束的痕迹。
- 凭证不硬编码（§5.3），测试不连真实服务（§5.4），权限决策落 audit.log（§5.5）。
- L0 约束在实现层强制（§5.1），不依赖 LLM 自觉。

### 4.4 人机协作模式

项目采用"AI 实现 + 人审查"协作模式：

- **AI 实现**：按 task 实现代码、单测、文档更新，提交 PR。
- **人审查**：按 [`dev-plan.md`](./dev-plan.md) §2.3 PR 评审 checklist 逐项检查——CI 全绿、覆盖率不下降、L0 约束自检、文档同步、安全边界改动至少一名 reviewer 显式 approve、性能敏感路径附 criterion 基准对比。
- **关键路径人工把关**：涉及安全边界（权限/沙箱/Hook/MCP）的改动必须人工 approve，AI 不可自行合并。

### 4.5 代码审查流程

代码审查流程在 [`dev-plan.md`](./dev-plan.md) §2.3 与 [`review-report.md`](./history/review-report.md) 中固化：

- **PR checklist**：CI 全绿（fmt/clippy -D warnings/test/audit/deny）、新增逻辑有单测、L0 约束自检、文档同步、安全边界 reviewer approve、性能基准对比、无凭证泄露。
- **审查报告**：[`review-report.md`](./history/review-report.md) 记录 2026-08-02 的全量审查——逐 crate 源码阅读 + 约束与实现映射核验 + 文档一致性检查，结论"通过"，发现若干低危问题与文档/实现不一致（D1-D4、C1-C4）。
- **审查清单**：全部 17 crate 源码阅读、配置文件核验、docs 文档交叉核验、L0 硬约束实现映射核验、功能统计表一致性核验、最终报告输出。

---

## 5. 测试与质量保障

### 5.1 测试策略演进

测试策略按里程碑递进，记录于 [`design.md`](./design.md) §21.4 集成测试分层递进表：

| 里程碑 | 测试增强 |
|--------|---------|
| M0 | `tests/common/` 共享 stub（`NoopMcpClient`/`NoopHookRegistry`/`StubJournal` 等）、`criterion` 基准占位、wiremock fixture 占位 |
| M1 | SSE 解析/token 计数/路径沙箱/delta 聚合单测，wiremock 录制真实 OpenAI 响应做 fixture |
| M2 | 3+ 轮工具调用集成测试、criterion Agent 循环开销基线、audit.log 全类型记录测试 |
| M3 | proptest 压缩管道不变量、降级链 4 级全覆盖、--resume/--replay 集成测试 |
| M4 | Landlock EPERM 拦截测试、沙箱拒绝熔断 3/5 阈值测试、MCP project 批准流测试、`/undo` 冲突检测测试 |
| M5 | 10 类 Hook 事件全覆盖、Hook L0 不覆盖测试、asyncRewake 3 并发上限 + 超时 kill、macOS CI matrix |
| M6 | 三家 provider 同一会话行为一致、429 退避重试、rmcp http+OAuth、Windows CI matrix |
| M7 | 渲染帧率 < 16ms criterion 基准、TuiPrompter 非阻塞回传 |
| M8 | SDK `Client::ask` 第三方项目集成测试、serve curl 调用、MCP server Claude Desktop 发现、LSP 编辑器连接 |
| M9 | Vitest 单测、Testing Library 组件测试、Playwright E2E、MSW mock HTTP/SSE |

测试类型覆盖（[`roadmap.md`](./roadmap.md) 持续工程事项）：

| 类型 | 覆盖目标 | 工具 |
|------|---------|------|
| 单元 | 每个 trait 实现 ≥ 80% | `cargo test` |
| 集成 | 关键场景全覆盖 | `wiremock` + `tempfile` |
| 回放 | 真实会话回归 | JSONL fixture |
| 属性 | 压缩管道不变量 | `proptest` |
| 性能 | 关键路径不退化 | `criterion` 基准 |
| 沙箱 | 平台拒绝语义 | 容器内 CI matrix（Linux/macOS/Windows） |

### 5.2 覆盖率从 68% 到 82.9%+ 的历程

覆盖率目标是 [`AGENTS.md`](../AGENTS.md) §2.8 强制的"库 crate ≥ 80%"，演进历程：

- **M0 阶段**：coverage 门禁仅验证工具链就位不阻塞合并（无业务代码）。
- **M1 起阻塞**：`cargo llvm-cov --workspace --fail-under-lines 80` 阻塞合并。
- **初期 68%**：M1–M2 期间覆盖率约 68%，主要短板在前端层（tui/cli/server 需 TTY/HTTP 运行时，单测覆盖率低）与 HTTP server 集成路径。
- **覆盖率门禁细化**：CI 与 pre-commit 将前端层（`minicoding-tui`/`minicoding-cli`/`minicoding-server`/`minicoding-desktop`）排除出 80% 门禁，由集成测试覆盖；库 crate 单独卡 80%。
- **补测攻坚**：针对压缩管道（proptest 不变量）、路径沙箱（越界 case 全覆盖）、SSE 解析（边界 case）、权限决策（Allow/Deny/Ask/AllowAlways 全类型）、沙箱拒绝熔断（3/5 阈值）等关键路径补测。
- **最终 82.9%+**：库 crate 覆盖率稳定在 82.9% 以上，超过 80% 门禁阈值。CI coverage job 与 pre-push hook 双重卡点。

### 5.3 CI/CD 流水线建设

CI 流水线在 [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) 定义，共 10 道门禁（含 M9 web 门禁；DOC-10 对齐）：

1. **fmt**：`cargo fmt --all -- --check`
2. **clippy**：`cargo clippy --workspace --exclude minicoding-desktop --all-targets --all-features -- -D warnings`（排除 desktop，需 Tauri 系统库）
3. **test**：`cargo test --workspace --exclude minicoding-desktop --all-features`（mock 凭证 `sk-test-ci-mock-not-real`，不连真实服务）
4. **coverage**：`cargo llvm-cov --workspace --exclude minicoding-desktop --all-features --exclude minicoding-tui --exclude minicoding-cli --exclude minicoding-server --fail-under-lines 80`
5. **audit**：`cargo audit`（vulnerabilities 默认 deny，unmaintained/yanked/notices 仅警告）
6. **deny**：`cargo deny check advisories licenses bans sources`
7. **typos**：拼写检查（`_typos.toml` 配置）
8. **cross-platform**：macOS/Windows matrix `cargo test`（Seatbelt/Job Object 原生沙箱驱动）
9. **desktop**：安装 Tauri 系统依赖后 `cargo build -p minicoding-desktop --features desktop` + clippy

平台策略（features Q-07）：Linux 完整门禁 + Landlock 沙箱拒绝语义测试；macOS 编译 + 单测 matrix（Seatbelt）；Windows 编译 + 单测 matrix（Job Object）。三平台均有原生沙箱驱动，不再降级 NoopDriver。

### 5.4 pre-commit hooks

pre-commit 配置在 [`.pre-commit-config.yaml`](../.pre-commit-config.yaml)，与 CI 门禁一致，本地先拦截低级错误：

- **pre-commit 阶段**：trailing whitespace / EOF newline / secrets 检查、typos 拼写检查、`cargo fmt --check`、`cargo clippy -D warnings`、`cargo deny check`、敏感文件暂存检查（`.env`/`credentials.json`/`*.pem`/`*.key`）。
- **pre-push 阶段**：`cargo audit`、`cargo test --workspace`、`cargo llvm-cov --fail-under-lines 80`。

安装方式双轨：`pipx install pre-commit && pre-commit install`，或直接复制 `scripts/git-hooks/pre-commit` → `.git/hooks/pre-commit`（供无 pre-commit 工具的环境使用）。

### 5.5 cargo audit/deny 安全扫描

- **cargo audit**：已知漏洞扫描，vulnerabilities 默认 deny 阻断 CI；unmaintained/yanked/notices 仅警告（避免 `number_prefix 0.4.0` 等传递依赖的 unmaintained 警告阻断 CI）。
- **cargo deny**：依赖治理四维度——advisories（漏洞公告）/licenses（许可证限 MIT/Apache-2.0/BSD/ISC）/bans（禁用 crate）/sources（仅 crates.io）。
- **依赖治理**（[`AGENTS.md`](../AGENTS.md) §2.7）：新增依赖必须 `cargo audit` 无漏洞 + `cargo deny check licenses` 合规 + 仅开必要 feature；优先用主流库，不引入维护停滞或低下载量依赖。

---

## 6. 文档体系建设

### 6.1 docs/ 下 15+ 文档的演进

`docs/` 目录现有 15 份文档，按里程碑演进：

| 文档 | 用途 | 起始里程碑 |
|------|------|-----------|
| `roadmap.md` | 里程碑范围/验收/风险 | M0 |
| `dev-plan.md` | task 级开发计划 | M0 |
| `features.md` | 功能总账（182 项） | M0 |
| `rules.md` | 运行时 L0/L1/L2 约束（C-01..C-35） | M0 |
| `tech-stack.md` | 技术选型与权衡 | M0 |
| `design.md` | 设计机制（Agent 循环/上下文/权限/沙箱/Hook/MCP/Event Sourcing 等） | M0 |
| `modules.md` | crate 结构与模块树 | M0 |
| `architecture.md` | 分层架构与组件职责 | M0 |
| `api.md` | 公共 API 参考 | M1 |
| `security.md` | 安全机制 | M2 |
| `data-model.md` | 数据结构 | M1 |
| `hooks.md` | Hook 协议与事件 | M5 |
| `getting-started.md` | 上手指南 | M1 |
| `review-report.md` | 代码审查报告 | 审查时点 |
| `m9-design.md` | M9 Web/桌面设计 | M9 |
| `development-process.md` | 项目开发过程（本文） | 复盘时点 |

### 6.2 文档与代码同步规范

[`AGENTS.md`](../AGENTS.md) §4"文档更新规范"强制改代码必改文档：

| 改动 | 必须更新 |
|------|---------|
| 公共 API（trait/struct/enum/fn 签名） | `docs/api.md` |
| crate 结构（新增/重命名/删除 crate） | `docs/modules.md` |
| 运行时约束（L0/L1/L2） | `docs/rules.md` |
| 功能项（新增/修改功能） | `docs/features.md` |
| 设计机制 | `docs/design.md` |
| 安全机制 | `docs/security.md` |
| 数据结构 | `docs/data-model.md` |
| Hook 协议/事件 | `docs/hooks.md` |
| 技术选型 | `docs/tech-stack.md` |

辅助规范：代码块必须有解释（§4.2）、章节编号不冲突（§4.3）、引用准确用相对路径或 §章节号（§4.4）、功能 ID 与约束 ID 同步（§4.5）、统计表项数与表格实际行数一致（§4.6）、不创建多余文档（§4.7）。

### 6.3 AI 助手行为约束

[`AGENTS.md`](../AGENTS.md) §7"AI 助手行为约束"专门约束开发时 AI 助手：

- **§7.1 先读后改**：修改前必须 Read，不基于猜测修改未读文件。
- **§7.2 不臆造 API**：不确定的库 API 必须查文档或读源码，不假设库存在、不假设 trait 方法存在。
- **§7.3 不绕过约束**：即使被要求"快速实现"也不违反规范，不为"通过测试"而注释安全检查。
- **§7.4 解释决策**：选择方案时说明 why，不只贴代码不解释。
- **§7.5 不创建测试代码除非要求**：默认不写测试，除非用户明确要求或验收标准明确要求。
- **§7.6 保持简洁**：不做不必要的改进、不加多余抽象、不创建多余文件、不加多余注释。

---

## 7. 遇到的关键挑战与解决

### 7.1 跨平台沙箱实现挑战

**挑战**：Landlock（Linux）/Seatbelt（macOS）/Job Object（Windows）三平台沙箱 API 差异大，profile/ruleset 语法不同，旧内核不支持 Landlock，Windows 受限令牌成熟度低。

**解决**：
- 用 `sandbox-run` 统一跨平台 API（[`tech-stack.md`](./tech-stack.md) §13），不自研胶水。
- 平台优先级策略（[`tech-stack.md`](./tech-stack.md) §11）：M4 仅 Linux（Landlock+libseccomp），M5+ 补 macOS（Seatbelt），M6+ 补 Windows（Job Object）。三平台均有原生沙箱驱动后不再降级 NoopDriver。
- 编译期平台检测 + 运行时 `landlock_available()` 探测，旧内核降级 `NoopDriver` + warn，`is_hardened()` 如实返回 false。
- denial 签名库（stderr 模式 + errno）识别沙箱拒绝，走升级流而非裸错误。
- 容器内 CI matrix（Linux/macOS/Windows）验证拒绝语义。

### 7.2 Tauri 依赖隔离（feature gate）

**挑战**：`minicoding-desktop` 的 `desktop` feature 依赖 Tauri，需 webkit2gtk/glib 系统库，常规 `--all-features` 编译会因系统库缺失失败。

**解决**：
- feature gate 隔离：`desktop` feature 默认关闭，单独启用。
- CI 与 pre-commit 均排除 `minicoding-desktop`：`cargo clippy --workspace --exclude minicoding-desktop`、`cargo test --workspace --exclude minicoding-desktop`。
- 单独 `desktop` job：安装 `libwebkit2gtk-4.1-dev libgtk-3-dev libglib2.0-dev libsoup-3.0-dev libjavascriptcoregtk-4.1-dev` 后 `cargo build -p minicoding-desktop --features desktop` + clippy。
- `glib-sys` 等 Tauri 传递依赖隔离在 `minicoding-desktop` crate，不污染其它 crate。

### 7.3 TypeScript 类型生成（ts-rs）

**挑战**：前后端 DTO 契约需保证一致，手写双份易漂移。

**解决**：
- 用 `ts-rs` 自动生成 TypeScript 类型（[`AGENTS.md`](../AGENTS.md) §8.4），`minicoding-protocol` 与 `minicoding-core` 的 Rust DTO 通过 `#[derive(TS)]` 导出。
- 生成产物放 `crates/minicoding-web/src/api/generated/`，文件头标注 `// AUTO-GENERATED, DO NOT EDIT`，不手动编辑。
- `gen-types` 脚本后处理：barrel re-export（`index.ts`）+ 排除 ts-rs 输出的 trailing whitespace（pre-commit 配置排除该目录）。
- CI 校验生成产物与 Rust 源一致（`git diff --exit-code`）。
- 运行时校验：JSON-RPC 响应必须经 Zod parse 后才进入业务层，防止 schema 漂移导致运行时错误。

### 7.4 CI 系统依赖问题（glib-sys）

**挑战**：Tauri 依赖 `glib-sys`/`webkit2gtk-sys` 等 sys crate，需系统库 `pkg-config` 找到 `glib-2.0`，CI 默认环境无这些库。

**解决**：
- `desktop` job 单独安装系统依赖（见 §7.2）。
- 常规门禁 job 排除 `minicoding-desktop`，避免 sys crate 编译失败阻断 CI。
- pre-commit 同样排除，本地无系统库也能提交。
- `Cargo.lock` 提交到仓库，保证三平台依赖版本一致。

### 7.5 覆盖率达标挑战

**挑战**：库 crate 覆盖率目标 ≥ 80%，但前端层（tui/cli/server/desktop）需 TTY/HTTP/GUI 运行时，单测覆盖率低，拉低整体。

**解决**：
- 覆盖率门禁细化：CI 与 pre-commit 将前端层排除出 80% 门禁（`--exclude minicoding-tui --exclude minicoding-cli --exclude minicoding-server --exclude minicoding-desktop`），由集成测试覆盖。
- 库 crate 单独卡 80%：`cargo llvm-cov --workspace --exclude ... --fail-under-lines 80`。
- 关键路径补测：压缩管道 proptest 不变量、路径沙箱越界 case、SSE 解析边界 case、权限决策全类型、沙箱拒绝熔断阈值。
- 共享 stub 基础设施（`tests/common/`）降低 mock 成本，各 crate 复用 `NoopMcpClient`/`StubJournal` 等替身。
- 最终库 crate 覆盖率稳定在 82.9%+。

### 7.6 其它挑战（来自审查报告）

[`review-report.md`](./history/review-report.md) §4 记录的发现与建议：

- **D1 文档/实现不一致**：`features.md` X-22 标注"部分实现"但代码已实现统一 dispatch → 更新为"已实现"。
- **D3 README crate 列表不全**：`README.md` 列 14 crate 但实际 17 → 补充完整。
- **D4 功能状态滞后**：大量标注"规划中"但代码已实现 → 按实际状态批量更新。
- **C1 mcp/approval.rs**：`project_fingerprint` 用 `canonicalize` 失败时回退原始路径，建议注释说明符号链接场景的指纹稳定性。
- **C2 storage/index.rs**：时间字符串用 RFC3339，建议统一为 `time::OffsetDateTime` 序列化避免格式漂移。
- **C4 大文件拆分**：`app.rs`/`main.rs`/`markdown.rs` 等 TUI 文件行数偏大，建议按 250 LOC 上限拆分。

---

## 8. 项目复盘与反思

### 8.1 做得好的地方

1. **架构纪律性强**：[`review-report.md`](./history/review-report.md) 结论"代码质量高，架构纪律性强，安全约束在实现层落地扎实"。trait 定义集中在 core、领域 crate 单向依赖、重依赖 feature gate 隔离、`unsafe` 仅限 FFI 且带 `// SAFETY:` 注释——这些约束从 M0 贯彻到 M9，未因赶进度而放松。
2. **L0 硬约束实现层强制**：C-01..C-30 全部在 Rust 实现层强制，不依赖 LLM 自觉。内置黑名单不可覆盖、AGENTS.md 不可被 Agent 自主编辑、Auto memory 不可作为越权通道、压缩熔断不可被 LLM 绕过——这些安全底线经审查报告核验全部落地。
3. **里程碑前置决策**：将沙箱/Hooks/MCP/Plan/Journal 从原 M4/M7 前置到独立里程碑，避免 MVP 形成后再大改权限/工具边界。这一决策在 M4/M5 交付时验证了价值——MCP 远程工具与文件回滚都依赖沙箱作为安全底线，同里程碑交付避免了"有 Hook 无沙箱兜底"的窗口期。
4. **不自研能用库的**：沙箱用 `sandbox-run`、MCP 用 `rmcp`、HTTP 用 `reqwest`、glob 用 `globset`、正则用 `regex`、路径用 `camino`、LSP 用 `tower-lsp`——全部用主流库，避免了重复造轮子与维护负担。
5. **文档与代码同步**：[`AGENTS.md`](../AGENTS.md) §4 强制改代码必改文档，docs/ 下 15 份文档与代码同步演进，审查报告核验"文档与实现整体一致"。
6. **可观测性内建**：OTel 从 M0 接入，全链路 span（session/turn/llm_call/tool_call/permission/hook.run/mcp.call）从第一天就位，调试 Agent 循环黑盒有据可查。
7. **CI 十门禁**：fmt/clippy/test/coverage/audit/deny/typos/cross-platform/web/desktop 十道门禁从 M0 建立并随里程碑增强，三平台原生沙箱 CI matrix 保证跨平台拒绝语义可验证。

### 8.2 可改进的地方

1. **文档状态滞后**：[`review-report.md`](./history/review-report.md) D4 指出 `features.md` 大量标注"规划中"但代码已实现。文档同步规范虽在 [`AGENTS.md`](../AGENTS.md) §4 强制，但功能状态字段的批量更新仍依赖人工，缺少自动化校验。
2. **大文件未拆分**：[`review-report.md`](./history/review-report.md) C4 指出 `app.rs`/`main.rs`/`markdown.rs` 等 TUI 文件行数偏大，超过 250 LOC 上限。M7 TUI 快速迭代时未及时拆分，遗留技术债。
3. **时间字符串格式不统一**：[`review-report.md`](./history/review-report.md) C2 指出 `storage/index.rs` 用 RFC3339 字符串而非 `time::OffsetDateTime` 序列化，存在格式漂移风险。M3 实现时未严格遵守 [`AGENTS.md`](../AGENTS.md) §2.5"时间用 `time::OffsetDateTime`"约定。
4. **`README.md` crate 列表不全**：[`review-report.md`](./history/review-report.md) D3 指出 `README.md` 列 14 crate 但实际 17。M5/M6/M8 补齐 crate 时未同步更新 README。
5. **覆盖率初期偏低**：M1–M2 期间覆盖率约 68%，低于 80% 门禁。虽通过细化门禁（排除前端层）与补测攻坚最终达到 82.9%+，但初期未将前端层排除出门禁的设计导致了短暂的覆盖率卡点。
6. **里程碑 task 数与统计表不一致**：[`dev-plan.md`](./dev-plan.md) 附录统计 M8 = 6 task，但 §9.3 实际列出 T-M8-1..T-M8-9 共 9 task；[`features.md`](./features.md) 统计表 M8 = 6 task。统计表与实际 task 列表存在偏差，需以实际 task 列表为准并修正统计。

### 8.3 经验教训

1. **安全约束必须从第一天确立**：L0 硬约束在 M0 就写入 [`rules.md`](./rules.md)，[`AGENTS.md`](../AGENTS.md) §5.1 强制实现层落地。若 MVP 后再补安全边界，需大改权限/工具边界，代价远高于一开始就内置。
2. **AI 助手行为约束需显式文档化**：[`AGENTS.md`](../AGENTS.md) §7 把"先读后改""不臆造 API""不绕过约束""不创建测试除非要求""保持简洁"等约束显式写明，AI 助手才能稳定遵守。口头约定不可靠。
3. **重依赖 feature gate 隔离是跨平台关键**：Tauri/landlock/libseccomp/rmcp/ratatui/windows 等重依赖通过 feature gate / target cfg 隔离在对应实现 crate，避免污染 core 与其它 crate，是跨平台编译可行的前提。
4. **Event Sourcing 平滑过渡**：与原 JSONL 消息日志双写并存，新会话写事件流，旧会话回退消息日志，避免了"一刀切迁移"的风险。这一模式可复用于其它架构演进。
5. **审查报告价值高**：[`review-report.md`](./history/review-report.md) 全量审查发现的问题（D1-D4 文档不一致、C1-C4 代码建议）是日常 PR 评审难以发现的跨 crate 系统性问题。定期全量审查值得坚持。
6. **平台优先级策略避免并行开发瓶颈**：M4 仅 Linux、M5+ 补 macOS、M6+ 补 Windows 的平台优先级策略，让沙箱实现与 CI matrix 按里程碑递进，避免三平台并行开发拖慢主线。

---

## 9. 未来展望

### 9.1 M10+ 规划

`roadmap.md` 的"未来方向"已实现 Event Sourcing（M8）。后续探索性方向（不阻塞当前里程碑）：

- **Tauri 2.x mobile**：M9 仅桌面，mobile（iOS/Android）留待 M10+。Tauri 2.x 已支持 mobile，前端复用 `minicoding-web`，需补原生 mobile 集成（凭证、通知、分享）。
- **多用户协作**：当前为单用户模型，M10+ 可探索多用户会话共享与权限隔离（基于 `minicoding-server` 的多客户端并发会话基础）。
- **更多 LLM Provider**：Google Gemini、Mistral、Cohere 等 provider 接入，复用 M6 的 `LlmProvider` trait 抽象。
- **更智能的上下文管理**：基于向量的语义压缩（替代 L2 摘要的 LLM 调用）、跨会话记忆图（替代双文件记忆）、动态 token 预算分配。
- **Extension 生态**：[`design.md`](./design.md) §23 Extension SDK 已就位，M10+ 可建设扩展市场（扩展发现、安装、签名校验、版本管理）。
- **LSP 能力增强**：`textDocument/completion`（AI 补全）、`textDocument/codeLens`（AI 提示）、`textDocument/hover`（AI 解释）等 LSP 方法，把 minicoding 从"对话式助手"扩展为"内嵌式 IDE AI"。

### 9.2 社区建设

- **发布渠道**：`cargo dist` 产出跨平台二进制（Linux musl、macOS universal、Windows），Homebrew / Scoop / cargo install 三渠道（Q-08/Q-09）已配置。
- **文档站**：`cargo doc --workspace` 作为 API 参考，`docs/` 下 15 份文档可作为文档站基础。
- **贡献指南**：[`AGENTS.md`](../AGENTS.md) 既是 AI 助手约束，也是人类贡献者的架构指南；[`dev-plan.md`](./dev-plan.md) §2 开发流程与协作约定可作为贡献入门。
- **示例与教程**：`getting-started.md` 上手指南 + 6 个内置示例 Hook（fmt-on-write / auto-approve-tests / block-secrets / git-status-inject / backup-before-compact / test-on-stop）可作为示例库。

### 9.3 生态扩展

- **MCP server 生态**：`minicoding serve --as-mcp-server` 把自身工具暴露为 MCP server，可被 Claude Desktop 等客户端发现使用；反向也可接入更多 MCP server（M4 已支持 project 作用域首次批准流）。
- **编辑器集成**：M8 的 ACP（Zed）与 LSP（VS Code/Neovim/Emacs/Helix）适配器已就位，M9 的 Web/桌面降低了非终端用户门槛，形成"终端 + 编辑器 + 浏览器 + 桌面"四端覆盖。
- **嵌入式 SDK**：`minicoding-sdk` 的 `Client` + `ClientBuilder` 可被第三方 Rust 项目嵌入，`CallbackPrompter` 供 SDK 用户闭包注入权限决策，为生态扩展提供基础。
- **全 Rust 工具链示范**：M9 前端用 oxlint/oxfmt/Vite Rolldown/Tailwind v4 Oxide 全 Rust 工具链，与后端 Rust 工具链一致，可作为"全栈 Rust 工程"的参考实现。

---

> **结语**：`minicoding-rs` 从 M0 骨架到 M9 Web/桌面，10 个里程碑、79 个 task、93 理想人日、182 项功能、35 条约束、18 个 cargo crate + 1 个前端项目，全程 AI 辅助编码 + 人工审查，架构纪律与安全约束从第一天贯彻到最后一天。审查报告结论"通过"是对这一过程的客观印证。未来 M10+ 将在移动端、多用户、Extension 生态、LSP 能力增强等方向继续演进。
