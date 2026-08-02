# 开发路线图

本文给出 `minicoding-rs` 的分阶段交付计划、每阶段范围、验收标准与风险。时间预算以"理想人日"计，不承诺日历日期。

> **重构说明**：参考 Claude Code 与 Codex CLI 的设计，沙箱、Hooks、MCP、Plan 模式、文件回滚等"扩展与安全"能力从原 M4/M7 前置到独立里程碑，避免 MVP 形成后再大改权限/工具边界。新里程碑顺序：基础 → Agent 循环（含应用层权限）→ 上下文/记忆 → OS 沙箱/MCP/Journal（安全底线 + 扩展机制基础）→ Hooks/Plan/子 Agent → 多 Provider → TUI → SDK/Server。

---

## 阶段总览

| 阶段 | 主题 | 理想人日 | 交付形态 |
|------|------|:---:|---------|
| M0 | 骨架与基础设施 | 3 | workspace + CI + OTel 模板 |
| M1 | MVP：单轮 CLI | 12 | 可提问、读文件、流式输出 |
| M2 | 完整 Agent 循环 + 应用层权限 | 12 | 工具多轮、写文件、shell、权限双抽象 |
| M3 | 上下文、持久化与记忆 | 10 | 压缩、会话恢复、长期记忆、AGENTS.md、任务管理 |
| M4 | 安全沙箱与 MCP | 8 | seatbelt/landlock/seccomp、预设、exec、拒绝升级、MCP client、/undo |
| M5 | 扩展机制：Hooks + Plan + 子 Agent | 12 | 10 类 Hook、Plan 模式、类型化子 Agent、macOS 沙箱 |
| M6 | 多 Provider 与健壮性 | 6 | Anthropic、Ollama、重试、错误恢复 |
| M7 | TUI | 10 | 全屏交互 |
| M8 | SDK 与 Server | 8 | 嵌入 + HTTP + MCP server |
| 持续 | 测试、文档、性能 | - | 每 milestone 内嵌 |

---

## M0 — 骨架与基础设施（3 人日）

**范围**
- Cargo workspace + 17 个 crate 骨架（空 `lib.rs` / `main.rs`），edition 2024，MSRV 1.99。其中 M0 落地 14 个核心 crate，另 3 个（`minicoding-protocol`/`minicoding-server`/`minicoding-extension-sdk`）在 M5/M6/M8 启用时补齐 `lib.rs`。
- `Cargo.toml` 公共依赖统一管理（workspace dependencies）；平台条件依赖示例（`landlock`/`libseccomp` 仅 Linux）。
- CI：`fmt` + `clippy -D warnings` + `test` + `cargo audit` + `cargo deny`。
- `tracing` + `tracing-subscriber` + `tracing-opentelemetry` + `opentelemetry-otlp` 初始化（`core::otel`），支持 `OTEL_EXPORTER_OTLP_ENDPOINT` 环境变量；无后端时降级为本地 fmt 日志。
- `anyhow` 错误出口、`clap` 最小骨架、`MINICODING_HOME` 路径解析（`core::paths`）。
- `minicoding-sandbox` crate 骨架 + `detect_driver()` 编译期平台检测返回 `NoopDriver`（占位）。
- README + docs 占位。

**验收**
- `cargo build --workspace` 通过（含平台条件依赖在非 Linux 平台不编译）。
- `cargo run -p minicoding-cli -- --help` 输出帮助。
- 设置 `OTEL_EXPORTER_OTLP_ENDPOINT` 后启动一次，能在本地 OTLP collector 看到 `minicoding` 的 resource。
- CI 全绿（`fmt` + `clippy -D warnings` + `test` + `cargo audit` + `cargo deny` + `coverage` 六门禁；coverage 在 M0 仅验证工具链就位不阻塞，M1 起阻塞合并）。

**任务追溯**：dev-plan T-M0-1..T-M0-9（9 个 task，预估 3 人日）。可度量门槛：14 crate 骨架编译通过（另 3 个 crate 在 M5/M6/M8 启用时补齐，合计 17）、CI 6 门禁就位（Linux only matrix）、OTel resource 可观测、`tests/common/` 共享测试工具就位（T-M0-9）。

**风险**：无。

---

## M1 — MVP：单轮 CLI（12 人日）

**范围**
- `core`：`Runtime`、`RuntimeBuilder`、`Message`/`ToolCall`/`ToolResult` 数据模型。
- `core`：`LlmProvider` trait + `Delta` 流。
- `providers`：OpenAI 兼容实现（含 SSE 解析、工具调用增量）。
- `providers`：`tiktoken-rs` Tokenizer。
- `tools`：`fs.read`、`fs.glob`、`fs.grep`、`fs.list`（只读组）。
- `core`：单轮 Agent 循环（无工具多轮，仅处理一次 tool_call 便于打基础）。
- `policy`：应用层路径沙箱 `sandbox_path`（第一道防线，`security.md` §3）。
- `cli`：参数解析、流式 token 渲染、单次提问模式。
- `storage`：JSONL append。
- `core`：配置加载支持 last-known-good 回退（解析成功时原子写入 `~/.minicoding/.last-known-good.toml`，解析失败时回退，见 `design.md` §12）与 `env:VAR_NAME`/`env:VAR:-fallback` 环境变量语法（见 `tech-stack.md` §12）。

**验收**
- `minicoding "读取 src/main.rs 并解释"` 能流式输出并实际读取文件。
- 工具调用渲染清晰（工具名 + 摘要）。
- 越界路径（`../../etc/passwd`）被 `sandbox_path` 拒绝并返回 `PathEscaped`。
- 单测覆盖 ≥ 80%：SSE 解析、token 计数、路径沙箱、delta 聚合。

**任务追溯**：dev-plan T-M1-1..T-M1-9（9 个 task，预估 12 人日）。可度量门槛：4 个只读工具可用、OpenAI SSE 流式首 token < 2s（网络除外）、JSONL 崩溃恢复测试通过、单测覆盖率 ≥ 80%。

**风险**
- SSE 解析边界 case 多 → 用 `wiremock` 录制真实响应做 fixture。
- OpenAI 工具调用分片 → `DeltaAccumulator` 需充分测试。

---

## M2 — 完整 Agent 循环 + 应用层权限（12 人日）

**范围**
- `core`：完整 Agent 循环（多轮工具调用、停止条件、防死循环）。
- `core`：工具并行/串行分桶调度（`SideEffect::None` 并行，其余严格串行，见 `design.md` §2.3）。
- `tools`：`fs.write`、`fs.edit`、`fs.delete`、`shell.run`。
- `tools`：`edit` 工具的精确字符串替换 + 唯一性校验。
- `core`：`ToolContext`（超时、取消、输出截断）。
- `core`：`PermissionPolicy` + `PermissionPrompter` 双抽象（决策/交互分离，见 `design.md` §9），`InteractivePrompter` + `NonInteractivePrompter` 实现，内置安全黑名单。
- `core`：`audit.log` 审计落盘。
- `cli`：工具调用进度渲染、权限确认交互、`Ctrl-C` graceful stop。
- `core`：`EventBus`（broadcast，仅通知类事件，无回复通道）。
- `core`：OTel span 埋点（session/turn/llm_call/tool_call/permission）。
- `providers`：`[provider.small]` 独立小 LLM 配置脚手架（为 M3 的摘要/compact/memory 提取配置独立 provider，未设置时与主 provider 相同，可配更便宜模型降本，见 `design.md` §3.8、`modules.md` §10.3）。

**验收**
- `minicoding "把 utils.rs 里的 foo 改名为 bar"` 能完成读取→编辑→验证闭环。
- 同轮多个只读工具并发执行；写/shell 严格串行（trace 中可见时序）。
- 非 TTY 环境下副作用工具按 `non_tty_strategy` 处理（默认 deny）。
- `shell.run` 超时、输出截断生效；危险命令被内置黑名单拒绝。
- `Ctrl-C` 不丢已生成消息（已落盘 JSONL 可恢复）。
- 集成测试：3+ 轮工具调用场景全通过；权限决策 100% 落 audit.log。

**任务追溯**：dev-plan T-M2-1..T-M2-9（9 个 task，预估 12 人日）。可度量门槛：max_tool_iters=50 防死循环生效、并行/串行调度 OTel span 时序可验证、audit.log 含 Allow/Deny/Ask 全类型记录、criterion 基准建立（Agent 循环开销基线）。

**风险**
- edit 唯一性冲突处理 → 提供清晰错误并建议增大上下文。
- 并行工具调用消息顺序 → 严格按 call id 关联 result，不依赖完成顺序。
- 权限交互在非 TTY 的边界 → `NonInteractivePrompter` 显式策略化。

---

## M3 — 上下文、持久化与记忆（10 人日）

**范围**
- `core`：`ContextManager` + token 预算 + 权重模型。
- `core`：压缩管道（4 级）。
- `core`：`ContextSnapshot` + 压缩日志。
- `storage`：`index.json` 维护、`load`/`list_sessions`、跨进程文件锁（`fs2`）。
- `cli`：`--resume`、`--replay`、`session list`/`delete` 子命令。
- `memory`：长期记忆双文件格式（md + index.json）+ mtime 缓存注入（见 `design.md` §8.2/§8.3）。
- `memory`：会话摘要生成 + 失败降级链（主模型→备用→启发式兜底，见 `design.md` §8.4）。
- `core`：`ProjectDocLoader` + AGENTS.md 分层加载（见 `design.md` §8.6）；fallback 文件名（`CLAUDE.md`/`.cursorrules`）。
- `tools`：`task.create`/`update`/`list` 任务管理工具（见 `design.md` §18，features T-14），`SideEffect::None`，校验单 in_progress/completed 必填 summary/依赖图成环检测。
- `core`：预测性压缩（根据历史 turn token 增长估算提前 compact，与反应式 compact 互补，见 `design.md` §3.9）与 Post-compact 上下文恢复（compact 后从历史提取最近 read 过的文件路径按预算截断重新注入，避免模型重新 read，见 `design.md` §3.10）。

**验收**
- 长会话（>上下文窗口）能自动压缩且不破坏连贯性。
- 长期记忆文件未变更时，连续多轮 `build_chat_request` 不产生重复 IO/分词（trace 中 compress span 计数稳定）。
- 会话摘要 LLM 调用失败时自动降级为启发式兜底，会话仍正常结束（audit.log 有告警）。
- `--resume <id>` 恢复后可继续提问；`--replay` 复现历史工具调用且默认禁副作用。
- AGENTS.md 从 repo_root 到 cwd 逐级加载并注入 system；Explore/Plan 子 Agent 跳过；`fs.write` 对 AGENTS.md 默认 `Ask`。
- `task.create/update/list` 能管理任务，单 in_progress 约束生效，`Event::TaskUpdated` 广播。

**任务追溯**：dev-plan T-M3-1..T-M3-10（10 个 task，预估 10 人日）。可度量门槛：压缩熔断 fail_threshold=3 生效、降级链 4 级全覆盖测试、proptest 压缩管道不变量测试通过、--resume/--replay 集成测试通过、AGENTS.md 分层加载覆盖 repo_root→cwd 全路径。

**风险**
- 压缩质量 → 摘要 prompt 调优；提供 `compress=off` 兜底。
- 摘要 LLM 调用成本 → 仅在阈值触发，且可配置用小模型。
- 记忆双文件一致性 → 原子 rename + 启动时索引校验/重建。
- AGENTS.md override 语义复杂 → 充分测试 fallback 与 override 组合。

---

## M4 — 安全沙箱与 MCP（8 人日）

> **范围调整说明**：参考 CC/Codex 后将原 M5 的 MCP client 与 Journal/`/undo` 前置到 M4，与 OS 沙箱同步交付——MCP 远程工具与文件回滚都依赖沙箱作为安全底线，同里程碑交付避免 M5 出现"有 Hook 无沙箱兜底"的窗口期。dev-plan M4 含 11 个 task，M5 聚焦 Hooks/子 Agent/Plan。

**范围**
- `sandbox`：`SandboxDriver` trait 实现落地——Linux Landlock + seccomp（`landlock` + `libseccomp`，M4 主交付）；macOS Seatbelt 与 Windows 受限令牌在 M5+/M6+ 补齐（见平台优先级）。
- `sandbox`：pre-main 进程硬化（`PR_SET_DUMPABLE=0`/`RLIMIT_CORE=0`/清 `LD_*`）。
- `sandbox`：`ExternalSandbox` 策略（CI/容器场景，依赖外层隔离，`NoopDriver` + info 日志）。
- `sandbox`：`.git`/`.hg`/`.svn` 默认只读保护（防破坏版本库）。
- `core`：`SandboxPolicy` 四模式（ReadOnly/WorkspaceWrite/ExternalSandbox/DangerFullAccess）+ `ApprovalMode`（Untrusted/OnFailure/OnRequest/Never）+ 预设（read-only/auto/external-sandbox/full-access）。
- `core`：沙箱拒绝检测与升级流（识别 EPERM/ENOSYS/Seatbelt denial → 请求批准 → 放宽策略重试，参考 Codex）。
- `tools`：`shell.run` 执行前调 `SandboxDriver::apply`；`fs.write/edit/delete` 受沙箱约束。
- `mcp`：`McpClient` trait + `rmcp` client（stdio，M4 一步到位用官方 SDK，不自研薄封装）。
- `mcp`：`mcp_tool_name` 命名（`mcp__<server>__<tool>`）+ project 作用域首次批准流（`mcp_choices.toml`，防恶意仓库植入）。
- `tools`：`mcp::wrapper` 把远程 MCP 工具包装为本地 `Tool`（`side_effect` 据 `readOnlyHint`/`destructiveHint` 映射）。
- `mcp`：MCP 进程池（连接跨 turn 复用，不每 turn 重启，见 `design.md` §19.5、`modules.md` §8.4）+ 后台预热（全局 server 启动时并发预热；项目级 server 创建/resume session 时后台预热，首 turn 仅在后台预热未完成时阻塞）+ inflight merge（同 server 并发请求合并，避免重复调用）。
- `journal`：`FileChangeJournal` + `/undo`（会话内 operation 级回滚，特性门控 `file_undo`，见 `design.md` §17）。
- `tools`：`fs.write/edit/delete` 成功后调 `Journal::record`（仅 `file_undo=true` 时）。
- `cli`：`--preset`、`--approval-mode`、`--sandbox`、`minicoding exec --sandbox read-only|external-sandbox ...` 子命令；`mcp` 子命令（list/approve/reset-project-choices）；`/undo` REPL 命令。
- `cli`：`doctor --security` 自检（沙箱驱动是否硬化、`.git` 保护、权限配置）。
- `core`：`CallbackPrompter`（SDK 用）、`TuiPrompter` 占位。

**验收**
- `--sandbox read-only` 下任何写/网络在内核被拦（Linux），`audit.log` 记录拒绝。
- `--sandbox workspace-write` 下越界写、网络外联被拦；工作区内自由读写执行。
- `minicoding exec --sandbox external-sandbox` 在容器内运行不报沙箱初始化失败，日志声明依赖外部隔离。
- `.git` 目录在 workspace-write 下默认拒绝写入（除非 `allow_vcs_write=true`）。
- 沙箱拒绝（如 Landlock EPERM）被识别并升级为权限请求，而非裸错误。
- `--preset full-access` 启动时打 red 警告并要求显式确认。
- `doctor --security` 输出沙箱驱动类型与硬化状态。
- MCP stdio server 能连接、`list_tools`、`call`；远程工具以 `mcp__<server>__<tool>` 注册。
- 含 `.minicoding/mcp.json` 的仓库首次进入时逐 server 弹窗批准，结果落 `mcp_choices.toml`。
- `/undo` 能回滚最近一次 operation 的文件改动；失败文件在 `UndoReport` 中列出。
- MCP/Hook 子进程不继承凭证环境变量。

**任务追溯**：dev-plan T-M4-1..T-M4-11（11 个 task，预估 8 人日）。可度量门槛：Landlock EPERM 拦截越界写可验证、沙箱拒绝熔断 3/5 次阈值生效、`--preset full-access` red 警告 + 二次确认生效、MCP project 作用域首次批准流测试通过、`/undo` 冲突检测（mtime/hash 比对）测试通过。**平台优先级**：M4 仅交付 Linux（Landlock+libseccomp），macOS/Windows 降级 NoopDriver（见 `tech-stack.md` §11 平台优先级策略，M5+ 补 macOS，M6+ 补 Windows）。

**风险**
- Landlock 旧内核不支持 → 编译期检测 + 运行时降级 `NoopDriver` + warn。
- Windows 受限令牌成熟度低 → 初期降级为应用层 + 用户提示，标注 "non-hardened"。
- Seatbelt profile 语法差异 → 按 macOS 版本测试 profile 生成。
- 沙箱拒绝与普通错误混淆 → 建立 denial 签名库（stderr 模式 + errno）。

---

## M5 — 扩展机制：Hooks + Plan + 子 Agent（12 人日）

> **范围调整说明**：MCP client 与 Journal/`/undo` 已前置到 M4（与沙箱同里程碑交付）。M5 聚焦 Hooks、Plan 模式、类型化子 Agent，并补齐 macOS 沙箱实现（平台优先级 M5+）。

**范围**
- `core`：`Hook` trait + `HookRegistry` + 10 类事件（SessionStart/UserPromptSubmit/PreToolUse/PostToolUse/PostToolUseFailure/PreCompact/PostCompact/Stop/SubagentStop/PermissionRequest，见 `hooks.md`）。
- `core`：`ScriptHook` 适配器（外部可执行 + JSON over stdio + 退出码语义），`on_hook_error` 策略。
- `core`：6 个内置示例 Hook（fmt-on-write / auto-approve-tests / block-secrets / git-status-inject / backup-before-compact / test-on-stop）。
- `core`：Plan 模式（`PermissionMode::Plan` + `plan.exit` 工具 + 双重只读强制 + 预批准缓存，见 `design.md` §16）。
- `core`：类型化子 Agent（Explore/Plan/General/Custom，`task.spawn` 工具，见 `design.md` §7）。
- `sandbox`：补齐 macOS Seatbelt 实现（`sandbox-run` 封装原生 sandbox 框架，平台优先级 M5+）。
- `cli`：`--hook`/配置加载 Hook；`/plan` 切换 Plan 模式。
- `core`：OTel `hook.run` span。
- `sdk`/`core`：Extension SDK（`Extension` trait + `Registrar` + `ExtensionManifest`，见 `design.md` §23、`modules.md` §17）+ Prompt 管道（9 个 `PromptContributor` 按固定顺序拼接，稳定段在前利于 prompt cache，见 `design.md` §22）。扩展注册的工具仍走 `ToolRegistry` dispatch，确保权限审计一致（C-01/C-02 不被绕过）。

**验收**
- `PostToolUse(fs.write|fs.edit)` Hook 能触发 `cargo fmt`；`PreToolUse` Hook `deny` 能阻断工具调用。
- Hook 对内置黑名单 `Deny` 的 `allow` 被忽略（L0 不破）；`modify_input` 越界被 `sandbox_path` 拦。
- Plan 模式下非只读工具被硬门 `Deny`；`plan.exit` 后切回 Default 模式并保留预批准。
- `task.spawn` 能启动子 Agent 并隔离上下文；子 Agent 结束后 `Event::SubagentFinished` 广播。
- MCP/Hook 子进程不继承凭证环境变量。
- macOS CI matrix 启用，`--sandbox read-only` 在 macOS 下写被 Seatbelt 拦。

**任务追溯**：dev-plan T-M5-1..T-M5-8（8 个 task，预估 12 人日）。可度量门槛：10 类 Hook 事件全覆盖测试、Hook L0 不覆盖（黑名单 Deny 时 allow 被忽略）测试通过、asyncRewake 3 并发上限 + 超时 kill 测试通过、Plan 模式硬门 + plan.exit 预批准缓存测试通过、macOS CI matrix 启用（平台优先级 M5+，见 `tech-stack.md` §11）、独立测试策略见 `design.md` §21（stub 替身表）。

**风险**
- Hook 串行链路影响延迟 → 默认超时 30s，`on_hook_error=continue` 兜底。
- Plan 预批准与权限矩阵交互复杂 → 充分测试 ExitPlanMode 后的 Verdict 解析。
- 子 Agent 上下文隔离不彻底 → 独立 ContextManager + 共享 trait 对象，单测验证隔离。
- macOS Seatbelt profile 语法差异 → 按 macOS 版本测试 profile 生成。

---

## M6 — 多 Provider 与健壮性（6 人日）

**范围**
- `providers`：Anthropic 实现（专有事件流、system prompt 分离）。
- `providers`：Ollama 实现（本地模型）。
- `providers`：统一重试/限流/超时装饰器。
- `mcp`：`rmcp` 完整客户端替换 `stdio_only`（支持 http + OAuth）。
- `core`：错误分类与恢复策略（见 `design.md` §10）。
- `cli`：`--provider`、`--model` 覆盖。
- 集成测试：mock 三家 provider 跑同一会话。
- `protocol`：JSON-RPC 2.0 wire types 独立 crate（`minicoding-protocol`，见 `modules.md` §15），为 M8 的 HTTP/SSE server、ACP 适配器与 LSP 适配器提供协议基础；ACP stdio 适配器脚手架（可被 Zed 等客户端嵌入）。
- `core`：配置热更新（`ConfigWatcher` 基于 fsnotify + `Event::ConfigChanged`，扩展通过 `on_config_changed()` 接收变更，M6+）。

**验收**
- Anthropic 模型可正常流式 + 工具调用。
- 限流自动退避重试；超时优雅取消。
- 三家 provider 行为一致（同一 prompt 产出合法消息序列）。
- `rmcp` http MCP server 可连接（含 bearer token 鉴权）。

**任务追溯**：dev-plan T-M6-1..T-M6-5（5 个 task，预估 6 人日）。可度量门槛：三家 provider（OpenAI/Anthropic/Ollama）同一会话行为一致测试通过、429 Retry-After 退避重试测试通过、rmcp http+OAuth 连接测试通过、Windows CI matrix 启用（平台优先级 M6+，见 `tech-stack.md` §11）。

**风险**
- Anthropic 事件流与 OpenAI 差异大 → 抽象层充分隔离。
- `rmcp` OAuth 流程复杂 → 保留 `stdio_only` 作为 fallback。

---

## M7 — TUI（10 人日）

**范围**
- `tui`：`ratatui` + `crossterm` 基础框架。
- 多会话侧栏、对话主视图、输入区、工具面板、权限弹窗、任务面板。
- 流式 Markdown 增量渲染。
- 自研 `InputState`（字符插入/光标移动/历史切换，不引入 `reedline`——与 ratatui 全屏 alternate screen 模式冲突，见 `modules.md` §13.3）。
- `TuiPrompter` 实现（点对点，非阻塞主循环）。
- 主题、配色、非 TTY 降级。

**验收**
- 全屏交互流畅（< 16ms 渲染）。
- 工具调用实时进度可见；任务面板同步更新。
- 权限弹窗非阻塞主循环（`TuiPrompter` 挂起工具调用，UI 处理后回传 `Decision`）。

**任务追溯**：dev-plan T-M7-1..T-M7-4（4 个 task，预估 10 人日）。可度量门槛：渲染帧率 < 16ms（60fps）、流式 Markdown 增量解析无闪烁、TuiPrompter 非阻塞回传测试通过。

**风险**
- 流式 Markdown 重绘性能 → 增量解析 + 脏区刷新。

---

## M8 — SDK 与 Server（8 人日）

**范围**
- `sdk`：`Client` + `ClientBuilder` + 高层 API。
- `cli`：`minicoding serve` HTTP/JSON-RPC server。
- `server`：HTTP/SSE JSON-RPC 接口（`minicoding-server`，见 `modules.md` §16），支持多客户端并发会话；事件流携带 cursor（event seq），客户端断连后从 cursor 恢复（SSE cursor 恢复）；broadcast 溢出时发 `RehydrateRequired` 信号，客户端重拉 snapshot。
- `server`：ACP stdio 适配器（`minicoding serve --acp`，可被 Zed 等客户端嵌入）。
- `server`：LSP stdio 适配器（`minicoding serve --lsp`，基于 `tower-lsp`，可被 VS Code/Neovim/Emacs/Helix 等编辑器嵌入，见 `design.md` §24 语义映射）；`LspPrompter` 实现 `PermissionPrompter`（`window/showMessageRequest` 点对点权限交互）。
- `mcp`：`minicoding serve --as-mcp-server` 把自身工具暴露为 MCP server（被其他 Agent 调用）。
- stdin/stdout NDJSON 协议（编辑器插件）。
- 文档：嵌入指南、协议规范、LSP 编辑器接入说明。

**验收**
- `Client::ask` 可在第三方 Rust 项目运行。
- `serve` 模式可被 curl 调用。
- MCP server 可被 Claude Desktop 等客户端发现并使用。
- `serve --lsp` 可被支持 LSP 的编辑器（VS Code/Neovim）连接，能发送 prompt 并接收流式 token；权限确认通过 `window/showMessageRequest` 弹窗。

**任务追溯**：dev-plan T-M8-1..T-M8-9（9 个 task，预估 8 人日）。可度量门槛：SDK `Client::ask` 在第三方 Rust 项目可运行、`serve` HTTP 端点可 curl 调用、MCP server 可被 Claude Desktop 发现、`serve --lsp` 可被 LSP 编辑器连接、跨平台二进制（cargo dist）三平台产出。

**风险**
- 协议稳定性 → 标 `experimental` 直到反馈收敛。

---

## M9 — Web 与桌面应用（Tauri，低优先级，预估 12 人日）

> **定位**：M9 为可选里程碑，优先级低于 M5–M8。在 M8 的 HTTP/SSE JSON-RPC server（`minicoding-server`）基础上，提供浏览器可访问的 Web 前端与原生桌面应用（Tauri 壳），降低非终端用户的上手门槛。Rust 后端不嵌入前端，前端通过 HTTP/SSE JSON-RPC 通信，保证 CLI/SDK 可独立使用。技术栈详见 `tech-stack.md` §4.1。

**范围**
- 新增 crate：
  - `minicoding-web`：纯前端项目（React 19.2 + TypeScript 7.0 + Vite 8.1 + React Compiler），独立 `package.json`，构建产物为静态资源，可被 `minicoding-server` 静态托管或独立部署；
  - `minicoding-desktop`：Tauri 2.x 壳，前端复用 `minicoding-web`，Rust sidecar 启动 `minicoding-server`，Tauri IPC 桥接前端与 sidecar；桌面端打包 `.dmg`/`.msi`/`.AppImage`。
- 前端核心能力：
  - 多会话面板（左侧会话列表 + 右侧对话流），复用 TanStack Router 类型安全路由；
  - 流式 token 渲染（SSE 订阅 `Event::Token`，TanStack Query 增量更新）；
  - 工具调用展开/折叠面板（`Event::ToolCall`/`Event::ToolResult`）；
  - 权限确认弹窗（`Event::PermissionRequest` → shadcn/ui Dialog → JSON-RPC `permission.resolve`）；
  - 任务面板（`Event::TaskUpdated` 同步显示任务进度）；
  - 上下文压缩/熔断可视化（`Event::Compress`/`Event::CompressCircuitBreak`）；
  - Hook 执行日志面板（`Event::HookRun`）；
  - 暗色/亮色主题切换（Tailwind v4 + shadcn/ui theme provider）；
  - 响应式布局（移动端友好，Tauri 2.x mobile 复用同一前端）。
- `minicoding-server` 增强：
  - 静态资源托管（`minicoding serve --web ./dist`），便于单二进制部署；
  - CORS 配置（`--cors-origin`，默认仅本地）；
  - SSE cursor 恢复（E-13，M8 已规划，M9 前端消费）。
- 桌面端特性：
  - 系统托盘（最小化到托盘 + 通知权限请求）；
  - 全局快捷键唤起；
  - 凭证存储复用 OS keyring（与 CLI `cred.rs` 共享，C-04）；
  - 自动更新（Tauri updater，签名校验）。
- 文档：Web/桌面部署指南、前端架构说明、Tauri sidecar 配置、CORS 与安全策略。

**验收**
- `minicoding serve --http` 启动后，浏览器访问 `http://localhost:PORT` 能完整对话、工具调用、权限确认；
- Tauri 桌面应用在 macOS/Windows/Linux 三平台可构建，体积 < 15MB；
- 前端 Lighthouse 性能评分 ≥ 90；
- oxlint + oxfmt + tsc 全绿；
- 凭证经 OS keyring 存储，不出现在前端代码/日志/网络请求中（C-04 延伸到前端）。

**任务追溯**：dev-plan T-M9-1..T-M9-8（8 个 task，预估 12 人日）。可度量门槛：Web 前端可对话/工具调用/权限确认三平台桌面应用可构建、Lighthouse ≥ 90、全 Rust 工具链构建（oxlint/oxfmt/Vite Rolldown/Tailwind v4）。

**风险**
- React Compiler 仍 RC → 评估稳定性，必要时回退到手写 memo；
- Tauri 2.x mobile 不在 M9 验收范围（仅桌面），mobile 留待 M10+；
- 前端安全：CSP 严格、不内联用户输入防 XSS，权限弹窗经 SSE 推送不可被前端伪造（后端校验 `prompt_id`）。

---

## 未来方向（Future Directions）

以下为 M8 之后探索性方向，不阻塞当前里程碑：

- ~~**Event Sourcing**：将会话状态建模为不可变事件流（`Event` 持久化 + snapshot 重放），替代当前的 JSONL 消息追加 + 内存镜像模型。~~ **已实现（见 `design.md` §25、`features.md` S-23..S-27）**：`EventStore` + `SnapshotStore` 双写并存，支持 SSE durable recovery、`--replay` 事件重放、schema 版本化与旧会话兼容。

---

## 持续工程事项

### 测试

| 类型 | 覆盖目标 | 工具 |
|------|---------|------|
| 单元 | 每个 trait 实现 ≥ 80% | `cargo test` |
| 集成 | 关键场景全覆盖 | `wiremock` + `tempfile` |
| 回放 | 真实会话回归 | JSONL fixture |
| 属性 | 压缩管道不变量 | `proptest` |
| 性能 | 关键路径不退化 | `criterion` 基准 |
| 沙箱 | 平台拒绝语义 | 容器内 CI matrix（Linux/macOS/Windows） |

### 文档

- 每个里程碑结束更新对应 `docs/`。
- API 变更写 CHANGELOG。
- `cargo doc --workspace` 作为 API 参考。

### 性能基线

- M2 起建立 `criterion` 基准：Agent 循环开销、token 计数、路径校验。
- 每次 PR 跑基准，回归 > 10% 阻塞合并。

### 发布

- SemVer；`0.x` 期间快速迭代。
- `cargo dist` 产出跨平台二进制（Linux musl、macOS universal、Windows）。
- Homebrew / Scoop / cargo install 三渠道。

---

## 里程碑依赖图

```
M0 ── M1 ── M2 ── M3 ── M4 ── M5 ── M6 ── M7 ── M8
                │           │      │
                └── M3' ────┘      └── M6 可与 M7 部分并行
```

- M1 → M2 强依赖（循环基础）。
- M2 → M3 强依赖（压缩基于完整循环）。
- M3 → M4：M4 的 OS 沙箱独立于上下文管理，但需 M2 的应用层权限就位；可与 M3 部分并行。
- M4 → M5：M5 的 Hook/MCP 依赖沙箱就位以隔离子进程；Plan/Journal 依赖 M3 的任务管理与存储。
- M6 可与 M7 部分并行（provider 工作独立于 TUI）。
- M8 依赖 M6/M7 完成。

---

## 风险登记册

| 风险 | 等级 | 缓解 |
|------|:---:|------|
| LLM 协议变更 | 中 | 抽象层 + fixture 测试 |
| 上下文压缩质量差 | 中 | 可配置策略 + 关闭选项 |
| 跨平台路径/权限差异 | 中 | `camino` + 平台 CI |
| Landlock/seccomp 旧内核不支持 | 中 | 编译期检测 + 运行时降级 `NoopDriver` |
| Windows 沙箱成熟度低 | 中 | 初期降级应用层，标注 non-hardened |
| keyring 跨平台不稳 | 低 | 文件 fallback |
| TUI 性能 | 低 | 增量渲染 + 基准 |
| MCP 协议演进 | 低 | 标 experimental；M5 仅 stdio |
| Hook 链路延迟 | 低 | 默认超时 + `on_hook_error` 兜底 |
| 沙箱拒绝误判为普通错误 | 中 | denial 签名库（stderr + errno） |
