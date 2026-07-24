# 开发路线图

本文给出 `minicoding-rs` 的分阶段交付计划、每阶段范围、验收标准与风险。时间预算以"理想人日"计，不承诺日历日期。

> **重构说明**：参考 Claude Code 与 Codex CLI 的设计，沙箱、Hooks、MCP、Plan 模式、文件回滚等"扩展与安全"能力从原 M4/M7 前置到独立里程碑，避免 MVP 形成后再大改权限/工具边界。新里程碑顺序：基础 → Agent 循环（含应用层权限）→ 上下文/记忆 → OS 沙箱/审批 → 扩展机制（Hooks/MCP/Plan/Undo）→ 多 Provider → TUI → SDK/Server。

---

## 阶段总览

| 阶段 | 主题 | 理想人日 | 交付形态 |
|------|------|:---:|---------|
| M0 | 骨架与基础设施 | 3 | workspace + CI + OTel 模板 |
| M1 | MVP：单轮 CLI | 12 | 可提问、读文件、流式输出 |
| M2 | 完整 Agent 循环 + 应用层权限 | 12 | 工具多轮、写文件、shell、权限双抽象 |
| M3 | 上下文、持久化与记忆 | 10 | 压缩、会话恢复、长期记忆、AGENTS.md、TodoWrite |
| M4 | OS 沙箱与审批模式 | 8 | seatbelt/landlock/seccomp、预设、exec、拒绝升级 |
| M5 | 扩展机制：Hooks + MCP + Plan + 文件回滚 | 12 | 10 类 Hook、MCP client、Plan 模式、/undo |
| M6 | 多 Provider 与健壮性 | 6 | Anthropic、Ollama、重试、错误恢复 |
| M7 | TUI | 10 | 全屏交互 |
| M8 | SDK 与 Server | 8 | 嵌入 + HTTP + MCP server |
| 持续 | 测试、文档、性能 | - | 每 milestone 内嵌 |

---

## M0 — 骨架与基础设施（3 人日）

**范围**
- Cargo workspace + 8 个 crate 骨架（空 `lib.rs` / `main.rs`），edition 2024，MSRV 1.85。
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
- CI 全绿。

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
- `core`：应用层路径沙箱 `sandbox_path`（第一道防线，`security.md` §3）。
- `cli`：参数解析、流式 token 渲染、单次提问模式。
- `storage`：JSONL append。

**验收**
- `minicoding "读取 src/main.rs 并解释"` 能流式输出并实际读取文件。
- 工具调用渲染清晰（工具名 + 摘要）。
- 越界路径（`../../etc/passwd`）被 `sandbox_path` 拒绝并返回 `PathEscaped`。
- 单测覆盖：SSE 解析、token 计数、路径沙箱、delta 聚合。

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

**验收**
- `minicoding "把 utils.rs 里的 foo 改名为 bar"` 能完成读取→编辑→验证闭环。
- 同轮多个只读工具并发执行；写/shell 严格串行（trace 中可见时序）。
- 非 TTY 环境下副作用工具按 `non_tty_strategy` 处理（默认 deny）。
- `shell.run` 超时、输出截断生效；危险命令被内置黑名单拒绝。
- `Ctrl-C` 不丢已生成消息。
- 集成测试：3+ 轮工具调用场景。

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
- `tools`：`todo.write` 工具（见 `design.md` §18），`SideEffect::None`，校验 20 上限/单 in_progress/completed 必填 summary。

**验收**
- 长会话（>上下文窗口）能自动压缩且不破坏连贯性。
- 长期记忆文件未变更时，连续多轮 `build_chat_request` 不产生重复 IO/分词（trace 中 compress span 计数稳定）。
- 会话摘要 LLM 调用失败时自动降级为启发式兜底，会话仍正常结束（audit.log 有告警）。
- `--resume <id>` 恢复后可继续提问；`--replay` 复现历史工具调用且默认禁副作用。
- AGENTS.md 从 repo_root 到 cwd 逐级加载并注入 system；Explore/Plan 子 Agent 跳过；`fs.write` 对 AGENTS.md 默认 `Ask`。
- `todo.write` 能创建/更新/完成 todo，单 in_progress 约束生效，`Event::TodoUpdated` 广播。

**风险**
- 压缩质量 → 摘要 prompt 调优；提供 `compress=off` 兜底。
- 摘要 LLM 调用成本 → 仅在阈值触发，且可配置用小模型。
- 记忆双文件一致性 → 原子 rename + 启动时索引校验/重建。
- AGENTS.md override 语义复杂 → 充分测试 fallback 与 override 组合。

---

## M4 — OS 沙箱与审批模式（8 人日）

**范围**
- `sandbox`：`SandboxDriver` trait 实现落地——macOS Seatbelt（`sandbox-exec -p` 动态生成 profile）、Linux Landlock + seccomp（`landlock` + `libseccomp`）、Windows 受限令牌（`windows` crate，可降级）。
- `sandbox`：pre-main 进程硬化（`PR_SET_DUMPABLE=0`/`RLIMIT_CORE=0`/清 `LD_*`）。
- `sandbox`：`ExternalSandbox` 策略（CI/容器场景，依赖外层隔离，`NoopDriver` + info 日志）。
- `sandbox`：`.git`/`.hg`/`.svn` 默认只读保护（防破坏版本库）。
- `core`：`SandboxPolicy` 四模式（ReadOnly/WorkspaceWrite/ExternalSandbox/DangerFullAccess）+ `ApprovalMode`（Untrusted/OnFailure/OnRequest/Never）+ 预设（read-only/auto/external-sandbox/full-access）。
- `core`：沙箱拒绝检测与升级流（识别 EPERM/ENOSYS/Seatbelt denial → 请求批准 → 放宽策略重试，参考 Codex）。
- `tools`：`shell.run` 执行前调 `SandboxDriver::apply`；`fs.write/edit/delete` 受沙箱约束。
- `cli`：`--preset`、`--approval-mode`、`--sandbox`、`minicoding exec --sandbox read-only|external-sandbox ...` 子命令。
- `cli`：`doctor --security` 自检（沙箱驱动是否硬化、`.git` 保护、权限配置）。
- `core`：`CallbackPrompter`（SDK 用）、`TuiPrompter` 占位。

**验收**
- `--sandbox read-only` 下任何写/网络在内核被拦（macOS/Linux），`audit.log` 记录拒绝。
- `--sandbox workspace-write` 下越界写、网络外联被拦；工作区内自由读写执行。
- `minicoding exec --sandbox external-sandbox` 在容器内运行不报沙箱初始化失败，日志声明依赖外部隔离。
- `.git` 目录在 workspace-write 下默认拒绝写入（除非 `allow_dotgit_write=true`）。
- 沙箱拒绝（如 Landlock EPERM）被识别并升级为权限请求，而非裸错误。
- `--preset full-access` 启动时打 red 警告并要求显式确认。
- `doctor --security` 输出沙箱驱动类型与硬化状态。

**风险**
- Landlock 旧内核不支持 → 编译期检测 + 运行时降级 `NoopDriver` + warn。
- Windows 受限令牌成熟度低 → 初期降级为应用层 + 用户提示，标注 "non-hardened"。
- Seatbelt profile 语法差异 → 按 macOS 版本测试 profile 生成。
- 沙箱拒绝与普通错误混淆 → 建立 denial 签名库（stderr 模式 + errno）。

---

## M5 — 扩展机制：Hooks + MCP + Plan + 文件回滚（12 人日）

**范围**
- `core`：`Hook` trait + `HookRegistry` + 10 类事件（SessionStart/UserPromptSubmit/PreToolUse/PostToolUse/PostToolUseFailure/PreCompact/PostCompact/Stop/SubagentStop/PermissionRequest，见 `hooks.md`）。
- `core`：`ScriptHook` 适配器（外部可执行 + JSON over stdio + 退出码语义），`on_hook_error` 策略。
- `core`：6 个内置示例 Hook（fmt-on-write / auto-approve-tests / block-secrets / git-status-inject / backup-before-compact / test-on-stop）。
- `mcp`：`McpClient` trait + `stdio_only` 客户端薄封装（M5 先交付，仅 stdio）。
- `mcp`：`mcp_tool_name` 命名（`mcp__<server>__<tool>`）+ project 作用域首次批准流（`mcp_choices.toml`，防恶意仓库植入）。
- `tools`：`mcp::wrapper` 把远程 MCP 工具包装为本地 `Tool`（`side_effect` 据 `readOnlyHint`/`destructiveHint` 映射）。
- `core`：Plan 模式（`PermissionMode::Plan` + `plan.exit` 工具 + 双重只读强制 + 预批准缓存，见 `design.md` §16）。
- `core`：`FileChangeJournal` + `/undo`（会话内 operation 级回滚，特性门控 `file_undo`，见 `design.md` §17）。
- `tools`：`fs.write/edit/delete` 成功后调 `Journal::record`（仅 `file_undo=true` 时）。
- `cli`：`--hook`/配置加载 Hook；`/undo` REPL 命令；`/plan` 切换 Plan 模式；`mcp` 子命令（list/approve/reset-project-choices）。
- `core`：OTel `hook.run` span、`mcp.call` span。

**验收**
- `PostToolUse(fs.write|fs.edit)` Hook 能触发 `cargo fmt`；`PreToolUse` Hook `deny` 能阻断工具调用。
- Hook 对内置黑名单 `Deny` 的 `allow` 被忽略（L0 不破）；`modify_input` 越界被 `sandbox_path` 拦。
- MCP stdio server 能连接、`list_tools`、`call`；远程工具以 `mcp__<server>__<tool>` 注册。
- 含 `.minicoding/mcp.json` 的仓库首次进入时逐 server 弹窗批准，结果落 `mcp_choices.toml`。
- Plan 模式下非只读工具被硬门 `Deny`；`plan.exit` 后切回 Default 模式并保留预批准。
- `/undo` 能回滚最近一次 operation 的文件改动；失败文件在 `UndoReport` 中列出。
- MCP/Hook 子进程不继承凭证环境变量。

**风险**
- Hook 串行链路影响延迟 → 默认超时 30s，`on_hook_error=continue` 兜底。
- MCP 协议演进 → M5 仅 stdio，`rmcp` 完整实现推迟到 M6+。
- Plan 预批准与权限矩阵交互复杂 → 充分测试 ExitPlanMode 后的 Verdict 解析。
- Journal 回滚与外部修改冲突 → 回滚前校验文件 mtime/hash，冲突时记入 `failed_files`。

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

**验收**
- Anthropic 模型可正常流式 + 工具调用。
- 限流自动退避重试；超时优雅取消。
- 三家 provider 行为一致（同一 prompt 产出合法消息序列）。
- `rmcp` http MCP server 可连接（含 bearer token 鉴权）。

**风险**
- Anthropic 事件流与 OpenAI 差异大 → 抽象层充分隔离。
- `rmcp` OAuth 流程复杂 → 保留 `stdio_only` 作为 fallback。

---

## M7 — TUI（10 人日）

**范围**
- `tui`：`ratatui` + `crossterm` 基础框架。
- 多会话侧栏、对话主视图、输入区、工具面板、权限弹窗、Todo 面板。
- 流式 Markdown 增量渲染。
- `reedline` 输入（历史、补全）。
- `TuiPrompter` 实现（点对点，非阻塞主循环）。
- 主题、配色、非 TTY 降级。

**验收**
- 全屏交互流畅（< 16ms 渲染）。
- 工具调用实时进度可见；Todo 面板同步更新。
- 权限弹窗非阻塞主循环（`TuiPrompter` 挂起工具调用，UI 处理后回传 `Decision`）。

**风险**
- 流式 Markdown 重绘性能 → 增量解析 + 脏区刷新。

---

## M8 — SDK 与 Server（8 人日）

**范围**
- `sdk`：`Client` + `ClientBuilder` + 高层 API。
- `cli`：`minicoding serve` HTTP/JSON-RPC server。
- `mcp`：`minicoding serve --as-mcp-server` 把自身工具暴露为 MCP server（被其他 Agent 调用）。
- stdin/stdout NDJSON 协议（编辑器插件）。
- 文档：嵌入指南、协议规范。

**验收**
- `Client::ask` 可在第三方 Rust 项目运行。
- `serve` 模式可被 curl 调用。
- MCP server 可被 Claude Desktop 等客户端发现并使用。

**风险**
- 协议稳定性 → 标 `experimental` 直到反馈收敛。

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
- M4 → M5：M5 的 Hook/MCP 依赖沙箱就位以隔离子进程；Plan/Journal 依赖 M3 的 TodoWrite 与存储。
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
