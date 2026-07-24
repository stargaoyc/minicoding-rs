# 功能清单（Feature List）

本文是 `minicoding-rs` 的功能总账，按领域分组列出全部功能项、交付里程碑与状态。状态约定：`规划中` / `开发中` / `MVP` / `增强` / `稳定`。里程碑引用 `roadmap.md`。

> **更新说明**：参考 Claude Code 与 Codex CLI 设计，新增 Hooks、MCP client、Plan 模式、文件回滚、AGENTS.md、审批模式/预设等能力；沙箱从"后续可选"升级为一等公民并前置到 M4。功能项数与统计已同步更新。

---

## 1. Agent 运行时

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| A-01 | 单轮对话 | 一次提问→流式回复 | M1 | 规划中 |
| A-02 | 多轮 Agent 循环 | 工具调用→结果→继续，直到 EndTurn | M2 | 规划中 |
| A-03 | 并行/串行工具调度 | 无副作用并行、有副作用严格串行 | M2 | 规划中 |
| A-04 | 停止条件与防死循环 | max_iters / turn_timeout / 重复检测 | M2 | 规划中 |
| A-05 | 类型化子 Agent | Explore/Plan/General/Custom，隔离上下文 | M5 | 规划中 |
| A-06 | Plan 模式 | 双重只读强制 + plan.exit + 预批准缓存 | M5 | 规划中 |
| A-07 | 任务管理工具 | TaskCreate/TaskUpdate/TaskList 增量模型 + 依赖 + 持久化 | M3 | 规划中 |
| A-08 | 会话中断与恢复 | Ctrl-C graceful + `--resume` | M2/M3 | 规划中 |
| A-09 | 会话回放 | `--replay` 复现历史（默认禁副作用） | M3 | 规划中 |
| A-10 | 文件改动回滚 | `/undo` 会话内 operation 级撤销 | M5 | 规划中 |
| A-11 | Parent-UUID 链会话结构 | 链表式 JSONL，支持 fork/压缩边界/side-chain | M3 | 规划中 |
| A-12 | Fork 会话 | `--fork-session` 从分叉点尝试不同方向 | M3 | 规划中 |
| A-13 | 惰性物化 | 首条消息时才创建会话文件 | M1 | 规划中 |
| A-14 | 64KB 窗口会话列出 | 首尾 64KB 快速列出万级会话 | M3 | 规划中 |
| A-15 | worktree 隔离子 Agent | 并行子 Agent 在独立 git worktree 工作 | M6+ | 规划中 |

## 2. LLM Provider

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| L-01 | OpenAI 兼容 | `/v1/chat/completions` SSE + 工具调用 | M1 | 规划中 |
| L-02 | Anthropic | `/v1/messages` 事件流 + system 分离 | M6 | 规划中 |
| L-03 | Ollama（本地模型） | `/api/chat` NDJSON | M6 | 规划中 |
| L-04 | 流式增量解析 | 文本 + 工具调用分片聚合 | M1 | 规划中 |
| L-05 | 重试与限流 | 指数退避、429 Retry-After | M6 | 规划中 |
| L-06 | 模型路由（Router） | 按任务类型选模型 | M7+ | 规划中 |
| L-07 | 多模态（Vision） | 图片输入 | M6 | 规划中 |

## 3. 工具系统

| ID | 功能 | 描述 | 副作用 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|:---:|
| T-01 | `fs.read` | 读取文件（支持行范围） | None | M1 | 规划中 |
| T-02 | `fs.list` | 列目录 | None | M1 | 规划中 |
| T-03 | `fs.glob` | glob 匹配（globset+ignore） | None | M1 | 规划中 |
| T-04 | `fs.grep` | 内容搜索（regex+ignore） | None | M1 | 规划中 |
| T-05 | `fs.write` | 写文件（整文件覆盖）+ Journal 记录 | FileWrite | M2 | 规划中 |
| T-06 | `fs.edit` | 精确字符串替换（唯一性校验）+ Journal | FileWrite | M2 | 规划中 |
| T-06b | `fs.multiedit` | 同文件多次顺序替换（原子性，参考 CC） | FileWrite | M2 | 规划中 |
| T-07 | `fs.delete` | 删除文件 + Journal 记录 | FileWrite | M2 | 规划中 |
| T-08 | `shell.run` | 执行命令（超时+截断+SandboxDriver） | Command | M2 | 规划中 |
| T-08b | `shell.background` | 启动后台命令，返回 shell_id（参考 CC） | Command | M4+ | 规划中 |
| T-08c | `shell.output` | 读取后台命令累积输出（非阻塞） | None | M4+ | 规划中 |
| T-08d | `shell.kill` | 终止后台命令 | Command | M4+ | 规划中 |
| T-09 | `web.fetch` | URL→Markdown | Network | M4+ | 规划中 |
| T-10 | `web.search` | 网页搜索 | Network | M7+ | 规划中 |
| T-11 | `git.diff` | 查看 diff | None | M6 | 规划中 |
| T-12 | `git.apply` | 应用 patch | FileWrite | M6 | 规划中 |
| T-13 | `task.spawn` | 启动类型化子 Agent | None | M5 | 规划中 |
| T-14 | `task.create`/`update`/`list` | 增量任务管理 + 依赖 + 持久化（替代 todo.write） | None | M3 | 规划中 |
| T-15 | `plan.exit` | 退出 Plan 模式并提交计划 | None | M5 | 规划中 |
| T-16 | `memory.write` | 写长期记忆 | FileWrite | M3 | 规划中 |
| T-17 | MCP 远程工具 | `mcp__<server>__<tool>` 动态注册 | 据 schema | M5 | 规划中 |
| T-18 | 自定义工具注册 | 第三方实现 `Tool` trait | - | M2 | 规划中 |

## 4. 上下文管理

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| C-01 | Token 预算 | 精确分词 + 预留输出 + 安全余量 | M1/M3 | 规划中 |
| C-02 | 消息权重模型 | role×recency×sticky×pin | M3 | 规划中 |
| C-03 | 4 级压缩管道 | 裁剪→摘要→滚动→硬截断 | M3 | 规划中 |
| C-04 | 压缩日志与快照 | 可回放、可调试 | M3 | 规划中 |
| C-05 | 压缩备份（可选） | 压缩前原文保留 | M3 | 规划中 |
| C-06 | `compress=off` 兜底 | 关闭压缩直通 | M3 | 规划中 |
| C-07 | 压缩熔断与防 Thrash | 失败计数≥3 熔断 / Thrash 检测 / 状态保留清单 / 降级链 | M3 | 规划中 |

## 5. 记忆

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| M-01 | 工作记忆 | Session.messages | M1 | 规划中 |
| M-02 | 会话记忆 | 跨会话摘要 | M3 | 规划中 |
| M-03 | 长期记忆双文件 | md + index.json | M3 | 规划中 |
| M-04 | mtime 缓存注入 | 无变更零 IO/分词 | M3 | 规划中 |
| M-05 | 隐式摘要 + 失败降级链 | 主→备用→启发式兜底 | M3 | 规划中 |
| M-06 | 显式 `memory.write` | 用户"记住 X" | M3 | 规划中 |
| M-07 | AGENTS.md 项目记忆 | 分层加载 + override + fallback | M3 | 规划中 |
| M-08 | 向量检索（`@memory`） | 语义检索增强 | M8+ | 规划中 |

## 6. 权限与安全

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| P-01 | Policy/Prompter 双抽象 | 决策与交互分离 | M2 | 规划中 |
| P-02 | InteractivePrompter | CLI TTY 交互 | M2 | 规划中 |
| P-03 | NonInteractivePrompter | 非 TTY 策略化（deny/allow/fail） | M2 | 规划中 |
| P-04 | policy.toml 持久化 | AllowAlways/DenyAlways | M2 | 规划中 |
| P-05 | 内置安全黑名单 | 危险命令/SSRF/敏感路径不可覆盖 | M2 | 规划中 |
| P-06 | 应用层路径沙箱 | 工作目录越界拒绝（第一道防线） | M1 | 规划中 |
| P-07 | 命令黑名单 | 正则匹配危险命令 | M2 | 规划中 |
| P-08 | SSRF 防护 | 内网/元数据接口拒绝 | M4+ | 规划中 |
| P-09 | TLS（rustls） | 最低 TLS 1.2 | M1 | 规划中 |
| P-10 | 凭证 keyring | OS 钥匙串存储 | M4 | 规划中 |
| P-11 | 环境变量隔离 | 凭证不下传子进程（含 MCP/Hook 子进程） | M2 | 规划中 |
| P-12 | 敏感数据脱敏 | .env/api_key/password 模式替换 | M4 | 规划中 |
| P-13 | 审计日志 | audit.log（含 deny/hook 决策） | M2 | 规划中 |
| P-14 | 回放安全 | replay 默认禁副作用 | M3 | 规划中 |
| P-15 | OS 沙箱（一等公民） | seatbelt/landlock/seccomp 内核级隔离 | M4 | 规划中 |
| P-16 | 四种沙箱策略 | ReadOnly/WorkspaceWrite/ExternalSandbox/DangerFullAccess | M4 | 规划中 |
| P-17 | 审批模式（ApprovalMode） | Untrusted/OnFailure/OnRequest/Never | M4 | 规划中 |
| P-18 | 预设（Preset） | read-only/auto/external-sandbox/full-access 一键选定 | M4 | 规划中 |
| P-19 | 沙箱拒绝检测与升级 | EPERM/Seatbelt denial → 请求批准 → 放宽重试 | M4 | 规划中 |
| P-20 | VCS 目录保护 | .git/.hg/.svn 默认只读 | M4 | 规划中 |
| P-21 | 进程硬化（pre-main） | PR_SET_DUMPABLE/RLIMIT_CORE/清 LD_* | M4 | 规划中 |
| P-22 | `minicoding exec` | 非交互批量执行 + 沙箱策略 | M4 | 规划中 |
| P-23 | `doctor --security` 自检 | 沙箱驱动/权限/VCS 保护检查 | M4 | 规划中 |
| P-24 | AGENTS.md 写保护 | fs.write/edit 对 AGENTS.md 默认 Ask | M3 | 规划中 |

## 7. Hooks 系统（参考 Claude Code）

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| H-01 | Hook trait + Registry | 进程内 Hook + 外部脚本统一抽象 | M5 | 规划中 |
| H-02 | 10 类生命周期事件 | SessionStart/UserPromptSubmit/PreToolUse/PostToolUse/PostToolUseFailure/PreCompact/PostCompact/Stop/SubagentStop/PermissionRequest | M5 | 规划中 |
| H-03 | ScriptHook 适配器 | 外部可执行 + JSON over stdio + 退出码语义 | M5 | 规划中 |
| H-04 | matcher 过滤 | 工具名 glob（`\|` 分隔、`*` 通配） | M5 | 规划中 |
| H-05 | PreToolUse 拦截/改写 | deny/allow(Ask→Allow)/modify_input | M5 | 规划中 |
| H-06 | PostToolUse 后处理 | 跑 formatter/linter、改写 result | M5 | 规划中 |
| H-07 | PermissionRequest 短路 | 自动批准/阻断，跳过 Prompter | M5 | 规划中 |
| H-08 | 上下文注入 | SessionStart/UserPromptSubmit/PreCompact 注入 | M5 | 规划中 |
| H-09 | L0 不可覆盖 | Hook 的 allow 对内置黑名单 Deny 无效 | M5 | 规划中 |
| H-10 | on_hook_error 策略 | continue/deny/fail，超时 kill | M5 | 规划中 |
| H-11 | 6 个内置示例 Hook | fmt-on-write/auto-approve-tests/block-secrets/git-status-inject/backup-before-compact/test-on-stop | M5 | 规划中 |
| H-12 | Hook 审计 | allow/deny/modify_input 落 audit.log（source=hook） | M5 | 规划中 |
| H-13 | asyncRewake 异步唤醒 | PostToolUse/Stop 后台任务完成后唤醒，3 并发上限，超时 kill | M6+ | 规划中 |

## 8. MCP 集成（Model Context Protocol，参考 Codex/CC）

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| X-01 | McpClient trait | list_tools/call/shutdown 抽象 | M5 | 规划中 |
| X-02 | stdio 客户端（薄封装） | M5 先交付，仅 stdio 传输 | M5 | 规划中 |
| X-03 | rmcp 完整客户端 | stdio + http + OAuth | M6 | 规划中 |
| X-04 | 工具命名 `mcp__<server>__<tool>` | 与权限规则通配匹配兼容 | M5 | 规划中 |
| X-05 | MCP 工具包装为本地 Tool | side_effect 据 readOnlyHint/destructiveHint 映射 | M5 | 规划中 |
| X-06 | 三作用域配置 | local/project/user（mcp.json） | M5 | 规划中 |
| X-07 | project 作用域首次批准 | mcp_choices.toml，防恶意仓库植入 | M5 | 规划中 |
| X-08 | required 语义 | required=true 启动失败则拒启动 | M5 | 规划中 |
| X-09 | 工具检索（Tool Search） | BM25 按需检索（工具多时） | M7+ | 规划中 |
| X-10 | MCP server 暴露 | `serve --as-mcp-server` 被其他 Agent 调用 | M8 | 规划中 |
| X-11 | `mcp` 子命令 | list/approve/reset-project-choices | M5 | 规划中 |

## 9. 可观测性

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| O-01 | tracing 结构化日志 | 本地文件滚动 | M0 | 规划中 |
| O-02 | OpenTelemetry 导出 | OTLP/HTTP+gRPC | M0 | 规划中 |
| O-03 | 全链路 span | session/turn/llm/tool/permission | M2 | 规划中 |
| O-04 | 子 Agent span 传播 | OTel Context 传播 | M5 | 规划中 |
| O-05 | 采样策略 | AlwaysOn/TraceIdRatio | M0 | 规划中 |
| O-06 | 事件总线订阅 | Event→span events | M2 | 规划中 |
| O-07 | hook.run span | hook.name/hook.event/hook.decision | M5 | 规划中 |
| O-08 | mcp.call span | server/tool/elapsed | M5 | 规划中 |

## 10. 持久化与存储

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| S-01 | JSONL 会话日志 | 追加写、崩溃安全 | M1 | 规划中 |
| S-02 | 会话索引 index.json | 轻量元数据列出 | M3 | 规划中 |
| S-03 | 跨进程文件锁 | 同会话互斥（fs2） | M3 | 规划中 |
| S-04 | 会话导出 | md / jsonl | M3 | 规划中 |
| S-05 | 备份 | tar.gz 打包 | M7+ | 规划中 |
| S-06 | `MINICODING_HOME` | 根目录覆盖 | M0 | 规划中 |
| S-07 | FileChangeJournal | 会话内文件改动账本（file_undo 特性门控） | M5 | 规划中 |

## 11. 前端

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| F-01 | CLI 单次模式 | `minicoding "prompt"` | M1 | 规划中 |
| F-02 | CLI 交互会话 | `--session` REPL（含 /undo /plan /mcp） | M2 | 规划中 |
| F-03 | 流式渲染 | token 直写 stdout | M1 | 规划中 |
| F-04 | 非 TTY 降级 | 禁 spinner/颜色 | M1 | 规划中 |
| F-05 | TUI 全屏 | ratatui 多视图 | M7 | 规划中 |
| F-06 | 流式 Markdown | 增量解析渲染 | M7 | 规划中 |
| F-07 | 权限弹窗 | 非阻塞主循环（TuiPrompter） | M7 | 规划中 |
| F-08 | Todo 面板 | TUI 同步显示 todo 进度 | M7 | 规划中 |

## 12. 嵌入与跨进程

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| E-01 | SDK Client | `ask`/`ask_stream`/`run_task` | M8 | 规划中 |
| E-02 | CallbackPrompter | SDK 用户闭包 | M4 | 规划中 |
| E-03 | HTTP/JSON-RPC server | `minicoding serve` | M8 | 规划中 |
| E-04 | MCP server | 被其他 Agent 调用（即 X-10） | M8 | 规划中 |
| E-05 | stdin/stdout NDJSON | 编辑器插件协议 | M8 | 规划中 |

## 13. 工程与质量

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| Q-01 | CI（fmt/clippy/test/audit/deny） | 全绿门禁 | M0 | 规划中 |
| Q-02 | 单元测试 ≥ 80% | 每 trait 实现 | 持续 | 规划中 |
| Q-03 | 集成测试（wiremock） | 完整 Agent 循环 | M1+ | 规划中 |
| Q-04 | 回放测试 | JSONL fixture | M3 | 规划中 |
| Q-05 | 属性测试（proptest） | 压缩管道不变量 | M3 | 规划中 |
| Q-06 | 性能基准（criterion） | 关键路径回归 | M2+ | 规划中 |
| Q-07 | 沙箱平台 CI matrix | Linux/macOS/Windows 拒绝语义 | M4+ | 规划中 |
| Q-08 | cargo dist 跨平台二进制 | Linux/macOS/Windows | M6+ | 规划中 |
| Q-09 | 分发（brew/scoop/cargo install） | 三渠道 | M6+ | 规划中 |

---

## 统计

| 领域 | 项数 |
|------|:---:|
| Agent 运行时 | 15 |
| LLM Provider | 7 |
| 工具系统 | 18 |
| 上下文管理 | 7 |
| 记忆 | 9 |
| 权限与安全 | 24 |
| Hooks 系统 | 13 |
| MCP 集成 | 11 |
| 可观测性 | 8 |
| 持久化与存储 | 7 |
| 前端 | 8 |
| 嵌入与跨进程 | 5 |
| 工程与质量 | 9 |
| **合计** | **141** |

> MVP（M0–M2）交付约 38 项；M3–M5 扩展与安全约 55 项；M6–M8 高级形态约 48 项（含 asyncRewake、Auto memory、压缩熔断等增强）。新增 Hooks（13）+ MCP client（11）+ 沙箱/审批强化（P-15..P-23）+ Plan/Undo/Todo/AGENTS.md/Auto memory 是参考 CC/Codex 后的核心增强。
