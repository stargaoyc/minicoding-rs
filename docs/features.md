# 功能清单（Feature List）

本文是 `minicoding-rs` 的功能总账，按领域分组列出全部功能项、交付里程碑与状态。状态约定：`规划中` / `已实现` / `部分实现`（附说明）。里程碑引用 `roadmap.md`。

> **更新说明**：参考 Claude Code 与 Codex CLI 设计，新增 Hooks、MCP client、Plan 模式、文件回滚、AGENTS.md、审批模式/预设等能力；沙箱从"后续可选"升级为一等公民并前置到 M4。功能项数与统计已同步更新。

---

## 1. Agent 运行时

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| A-01 | 单轮对话 | 一次提问→流式回复 | M1 | 已实现 |
| A-02 | 多轮 Agent 循环 | 工具调用→结果→继续，直到 EndTurn | M2 | 已实现 |
| A-03 | 并行/串行工具调度 | 无副作用并行、有副作用严格串行 | M2 | 已实现 |
| A-04 | 停止条件与防死循环 | max_iters / turn_timeout / 重复检测 | M2 | 已实现 |
| A-05 | 类型化子 Agent | Explore/Plan/General/Custom，隔离上下文 | M5 | 已实现 |
| A-06 | Plan 模式 | 双重只读强制 + plan.exit + 预批准缓存 | M5 | 已实现 |
| A-07 | 任务管理工具 | TaskCreate/TaskUpdate/TaskList 增量模型 + 依赖 + 持久化 | M3 | 已实现 |
| A-08 | 会话中断与恢复 | Ctrl-C graceful + `--resume` | M2/M3 | 已实现 |
| A-09 | 会话回放 | `--replay` 复现历史（默认禁副作用） | M3 | 已实现 |
| A-10 | 文件改动回滚 | `/undo` 会话内 operation 级撤销 | M4/M5 | 已实现 |
| A-11 | Parent-UUID 链会话结构 | 链表式 JSONL，支持 fork/压缩边界/side-chain | M3 | 已实现 |
| A-12 | Fork 会话 | `--fork-session` 从分叉点尝试不同方向 | M3 | 已实现 |
| A-13 | 惰性物化 | 首条消息时才创建会话文件 | M1 | 已实现 |
| A-14 | 64KB 窗口会话列出 | 首尾 64KB 快速列出万级会话 | M3 | 已实现 |
| A-15 | worktree 隔离子 Agent | 并行子 Agent 在独立 git worktree 工作 | M6+ | 已实现 |

## 2. LLM Provider

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| L-01 | OpenAI 兼容 | `/v1/chat/completions` SSE + 工具调用 | M1 | 已实现 |
| L-02 | Anthropic | `/v1/messages` 事件流 + system 分离 | M6 | 已实现 |
| L-03 | Ollama（本地模型） | `/api/chat` NDJSON | M6 | 已实现 |
| L-04 | 流式增量解析 | 文本 + 工具调用分片聚合 | M1 | 已实现 |
| L-05 | 重试与限流 | 指数退避、429 Retry-After | M6 | 已实现 |
| L-06 | 模型路由（Router） | `Router` trait + `StaticRouter` 骨架（`core::provider::router`），按任务类型选模型；M6 交付骨架，M7+ 实现按 `Task::kind` 路由 | M6 | 已实现（骨架） |
| L-07 | 多模态（Vision） | 图片输入 | M6 | 已实现 |
| L-08 | 独立小 LLM | 为摘要/compact/memory 提取配置独立 provider（`[provider.small]`），未设置时与主 provider 相同，可配更便宜模型降本（见 `design.md` §3.8、`modules.md` §10.3） | M3 | 已实现 |

## 3. 工具系统

| ID | 功能 | 描述 | 副作用 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|:---:|
| T-01 | `fs.read` | 读取文件（支持行范围） | None | M1 | 已实现 |
| T-02 | `fs.list` | 列目录 | None | M1 | 已实现 |
| T-03 | `fs.glob` | glob 匹配（globset+ignore） | None | M1 | 已实现 |
| T-04 | `fs.grep` | 内容搜索（regex+ignore） | None | M1 | 已实现 |
| T-05 | `fs.write` | 写文件（整文件覆盖）+ Journal 记录 | FileWrite | M2 | 已实现 |
| T-06 | `fs.edit` | 精确字符串替换（唯一性校验）+ Journal | FileWrite | M2 | 已实现 |
| T-06b | `fs.multiedit` | 同文件多次顺序替换（原子性，参考 CC） | FileWrite | M2 | 已实现 |
| T-07 | `fs.delete` | 删除文件 + Journal 记录 | FileWrite | M2 | 已实现 |
| T-08 | `shell.run` | 执行命令（超时+截断+SandboxDriver） | Command | M2 | 已实现 |
| T-08b | `shell.background` | 启动后台命令，返回 shell_id（参考 CC） | Command | M8 | 已实现 |
| T-08c | `shell.output` | 读取后台命令累积输出（非阻塞） | None | M8 | 已实现 |
| T-08d | `shell.kill` | 终止后台命令 | Command | M8 | 已实现 |
| T-09 | `web.fetch` | URL→Markdown，SSRF 防护（拒绝私有/loopback IP） | Network | M8 | 已实现 |
| T-10 | `web.search` | 网页搜索（DuckDuckGo HTML，无需 API key） | Network | M8 | 已实现 |
| T-11 | `git.diff` | 查看 diff（只读，路径沙箱） | None | M8 | 已实现 |
| T-12 | `git.apply` | 应用 patch（路径沙箱 + 权限审批） | FileWrite | M8 | 已实现 |
| T-13 | `task.spawn` | 启动类型化子 Agent | None | M5 | 已实现 |
| T-14 | `task.create`/`update`/`list` | 增量任务管理 + 依赖 + 持久化（替代 todo.write） | None | M3 | 已实现 |
| T-15 | `plan.exit` | 退出 Plan 模式并提交计划 | None | M5 | 已实现 |
| T-16 | `memory.write` | 写长期记忆 | FileWrite | M3 | 已实现 |
| T-17 | MCP 远程工具 | `mcp__<server>__<tool>` 动态注册 | 据 schema | M5 | 已实现 |
| T-18 | 自定义工具注册 | 第三方实现 `Tool` trait | - | M2 | 已实现 |

## 4. 上下文管理

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| C-01 | Token 预算 | 精确分词 + 预留输出 + 安全余量 | M1/M3 | 已实现 |
| C-02 | 消息权重模型 | role×recency×sticky×pin | M3 | 已实现 |
| C-03 | 4 级压缩管道 | 裁剪→摘要→滚动→硬截断 | M3 | 已实现 |
| C-04 | 压缩日志与快照 | 可回放、可调试 | M3 | 已实现 |
| C-05 | 压缩备份（可选） | 压缩前原文保留 | M3 | 已实现 |
| C-06 | `compress=off` 兜底 | 关闭压缩直通 | M3 | 已实现 |
| C-07 | 压缩熔断与防 Thrash | 失败计数≥3 熔断 / Thrash 检测 / 状态保留清单 / 降级链 | M3 | 已实现 |
| C-08 | 预测性压缩 | 根据历史 turn token 增长估算，在超出窗口前提前 compact，与反应式 compact 互补（见 `design.md` §3.9）。配置 `predictive_compact_enabled = false`（默认关）/ `predictive_baseline_growth_tokens = 15000` | M3 | 已实现 |
| C-09 | Post-compact 上下文恢复 | compact 后从历史提取最近 read 过的文件路径，按预算截断重新注入，避免模型重新 read（见 `design.md` §3.10）。配置 `post_compact_max_files = 5` / `post_compact_token_budget = 50000` / `post_compact_max_tokens_per_file = 5000` | M3 | 已实现 |

## 5. 记忆

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| M-01 | 工作记忆 | Session.messages | M1 | 已实现 |
| M-02 | 会话记忆 | 跨会话摘要 | M3 | 已实现 |
| M-03 | 长期记忆双文件 | md + index.json | M3 | 已实现 |
| M-04 | mtime 缓存注入 | 无变更零 IO/分词 | M3 | 已实现 |
| M-05 | 隐式摘要 + 失败降级链 | 主→备用→启发式兜底 | M3 | 已实现 |
| M-06 | 显式 `memory.write` | 用户"记住 X" | M3 | 已实现 |
| M-07 | AGENTS.md 项目记忆 | 分层加载 + override + fallback | M3 | 已实现 |
| M-08 | 向量检索（`@memory`） | BM25 语义检索（零外部依赖，CJK 逐字分词） | M8 | 已实现 |

## 6. 权限与安全

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| P-01 | Policy/Prompter 双抽象 | 决策与交互分离 | M2 | 已实现 |
| P-02 | InteractivePrompter | CLI TTY 交互 | M2 | 已实现 |
| P-03 | NonInteractivePrompter | 非 TTY 策略化（deny/allow/fail） | M2 | 已实现 |
| P-04 | policy.toml 持久化 | AllowAlways/DenyAlways | M2 | 已实现 |
| P-05 | 内置安全黑名单 | 危险命令/SSRF/敏感路径不可覆盖 | M2 | 已实现 |
| P-06 | 应用层路径沙箱 | 工作目录越界拒绝（第一道防线） | M1 | 已实现 |
| P-07 | 命令黑名单 | 正则匹配危险命令 | M2 | 已实现 |
| P-08 | SSRF 防护 | 内网/元数据接口拒绝 | M4 | 已实现 |
| P-09 | TLS（rustls） | 最低 TLS 1.2 | M1 | 已实现 |
| P-10 | 凭证 keyring | OS 钥匙串存储 | M4 | 已实现 |
| P-11 | 环境变量隔离 | 凭证不下传子进程（含 MCP/Hook 子进程） | M2 | 已实现 |
| P-12 | 敏感数据脱敏 | .env/api_key/password 模式替换 | M4 | 已实现 |
| P-13 | 审计日志 | audit.log（含 deny/hook 决策） | M2 | 已实现 |
| P-14 | 回放安全 | replay 默认禁副作用 | M3 | 已实现 |
| P-15 | OS 沙箱（一等公民） | seatbelt/landlock/seccomp 内核级隔离 | M4 | 已实现（Linux Landlock + macOS Seatbelt + Windows Job Object 三平台覆盖） |
| P-16 | 四种沙箱策略 | ReadOnly/WorkspaceWrite/ExternalSandbox/DangerFullAccess | M4 | 已实现 |
| P-17 | 审批模式（ApprovalMode） | Untrusted/OnFailure/OnRequest/Never | M4 | 已实现 |
| P-18 | 预设（Preset） | read-only/auto/external-sandbox/full-access 一键选定 | M4 | 已实现 |
| P-19 | 沙箱拒绝检测与升级 | EPERM/Seatbelt denial → 请求批准 → 放宽重试 | M4 | 已实现 |
| P-20 | VCS 目录保护 | .git/.hg/.svn 默认只读 | M4 | 已实现 |
| P-21 | 进程硬化（pre-main） | PR_SET_DUMPABLE/RLIMIT_CORE/清 LD_* | M4 | 已实现 |
| P-22 | `minicoding exec` | 非交互批量执行 + 沙箱策略 | M4 | 已实现 |
| P-23 | `doctor --security` 自检 | 沙箱驱动/权限/VCS 保护检查 | M4 | 已实现 |
| P-24 | AGENTS.md 写保护 | fs.write/edit 对 AGENTS.md 默认 Ask | M3 | 已实现 |

## 7. Hooks 系统（参考 Claude Code）

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| H-01 | Hook trait + Registry | 进程内 Hook + 外部脚本统一抽象 | M5 | 已实现 |
| H-02 | 10 类生命周期事件 | SessionStart/UserPromptSubmit/PreToolUse/PostToolUse/PostToolUseFailure/PreCompact/PostCompact/Stop/SubagentStop/PermissionRequest | M5 | 已实现 |
| H-03 | ScriptHook 适配器 | 外部可执行 + JSON over stdio + 退出码语义 | M5 | 已实现 |
| H-04 | matcher 过滤 | 工具名 glob（`\|` 分隔、`*` 通配） | M5 | 已实现 |
| H-05 | PreToolUse 拦截/改写 | deny/allow(Ask→Allow)/modify_input | M5 | 已实现 |
| H-06 | PostToolUse 后处理 | 跑 formatter/linter、改写 result | M5 | 已实现 |
| H-07 | PermissionRequest 短路 | 自动批准/阻断，跳过 Prompter | M5 | 已实现 |
| H-08 | 上下文注入 | SessionStart/UserPromptSubmit/PreCompact 注入 | M5 | 已实现 |
| H-09 | L0 不可覆盖 | Hook 的 allow 对内置黑名单 Deny 无效 | M5 | 已实现 |
| H-10 | on_hook_error 策略 | continue/deny/fail，超时 kill | M5 | 已实现 |
| H-11 | 6 个内置示例 Hook | fmt-on-write/auto-approve-tests/block-secrets/git-status-inject/backup-before-compact/test-on-stop | M5 | 已实现 |
| H-12 | Hook 审计 | allow/deny/modify_input 落 audit.log（source=hook） | M5 | 已实现 |
| H-13 | asyncRewake 异步唤醒 | PostToolUse/Stop 后台任务完成后唤醒，3 并发上限，超时 kill | M5 | 已实现 |

## 8. MCP 集成（Model Context Protocol，参考 Codex/CC）

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| X-01 | McpClient trait | list_tools/call/shutdown 抽象 | M4 | 已实现 |
| X-02 | stdio 客户端（薄封装） | M4 先交付，仅 stdio 传输 | M4 | 已实现 |
| X-03 | rmcp 完整客户端 | stdio + http（streamable HTTP + bearer token 鉴权） | M6 | 已实现 |
| X-04 | 工具命名 `mcp__<server>__<tool>` | 与权限规则通配匹配兼容 | M4 | 已实现 |
| X-05 | MCP 工具包装为本地 Tool | side_effect 据 readOnlyHint/destructiveHint 映射 | M4 | 已实现 |
| X-06 | 三作用域配置 | local/project/user（mcp.json） | M4 | 已实现 |
| X-07 | project 作用域首次批准 | mcp_choices.toml，防恶意仓库植入 | M4 | 已实现 |
| X-08 | required 语义 | required=true 启动失败则拒启动 | M4 | 已实现 |
| X-09 | 工具检索（Tool Search） | BM25 按需检索（工具多时） | M8 | 已实现 |
| X-10 | MCP server 暴露 | `serve --as-mcp-server` 被其他 Agent 调用 | M8 | 已实现 |
| X-11 | `mcp` 子命令 | list/approve/reset-project-choices | M4 | 已实现 |
| X-12 | MCP 进程池 | MCP server 连接跨 turn 复用，不每 turn 重启（见 `design.md` §19.5、`modules.md` §8.4） | M4 | 已实现 |
| X-13 | MCP 后台预热 | `warm_up` 刷新工具列表，确保连接活跃；首 turn 仅在预热未完成时阻塞 | M6+ | 已实现 |
| X-14 | MCP inflight merge | 同 server+tool+input 并发请求合并（`Shared<Future>`），避免重复调用 | M6+ | 已实现 |

## 9. 可观测性

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| O-01 | tracing 结构化日志 | 本地文件滚动 | M0 | 已实现 |
| O-02 | OpenTelemetry 导出 | OTLP/HTTP+gRPC | M0 | 已实现 |
| O-03 | 全链路 span | session/turn/llm/tool/permission | M2 | 已实现 |
| O-04 | 子 Agent span 传播 | OTel Context 传播 | M5 | 已实现 |
| O-05 | 采样策略 | AlwaysOn/TraceIdRatio | M0 | 已实现 |
| O-06 | 事件总线订阅 | Event→span events | M2 | 已实现 |
| O-07 | hook.run span | hook.name/hook.event/hook.decision | M5 | 已实现 |
| O-08 | mcp.call span | server/tool/elapsed | M5 | 已实现 |

## 10. 持久化与存储

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| S-01 | JSONL 会话日志 | 追加写、崩溃安全 | M1 | 已实现 |
| S-02 | 会话索引 index.json | 轻量元数据列出 | M3 | 已实现 |
| S-03 | 跨进程文件锁 | 同会话互斥（fs2） | M3 | 已实现 |
| S-04 | 会话导出 | md / jsonl | M3 | 已实现 |
| S-05 | 备份 | tar.gz 打包 | M7+ | 已实现 |
| S-06 | `MINICODING_HOME` | 根目录覆盖 | M0 | 已实现 |
| S-07 | FileChangeJournal | 会话内文件改动账本（file_undo 特性门控） | M4 | 已实现 |
| S-20 | last-known-good 配置回退 | 解析成功时原子写入 `~/.minicoding/.last-known-good.toml`，解析失败时回退（见 `design.md` §12） | M1 | 已实现 |
| S-21 | env: 环境变量语法 | 统一使用 `env:VAR_NAME` 语法引用环境变量，支持 `env:VAR:-fallback` 回退（见 `tech-stack.md` §12） | M1 | 已实现 |
| S-22 | 配置热更新 | `ConfigWatcher`（`notify` 8）+ `Event::ConfigChanged`，500ms debounce，best-effort 监听；扩展通过 `on_config_changed()` 接收变更（见 `design.md` §11、`modules.md` §1.2） | M6+ | 已实现 |
| S-23 | Event Sourcing 事件流 | `EventStore`（`{id}.events.jsonl`）持久化状态变更事件，`seq` 单调递增，支持事件重放重建 `Session`（见 `design.md` §25） | M8+ | 已实现 |
| S-24 | Snapshot 重放 | `SnapshotStore`（`{id}.snapshot.json`）每 50 条 `MessageAppended` 落盘 snapshot，加速 replay（见 `design.md` §25.3） | M8+ | 已实现 |
| S-25 | SSE durable recovery | `Last-Event-ID` cursor 三级回退：内存 ring buffer → `EventStore::load_after` → `RehydrateRequired`（见 `design.md` §25.5） | M8+ | 已实现 |
| S-26 | `--replay` 事件重放 | `--replay`/`--resume` 优先走 snapshot + 事件流重放，旧会话回退到消息日志（见 `design.md` §25.6） | M8+ | 已实现 |
| S-27 | 事件 schema 版本化 | `SCHEMA_VERSION` + `EventRecord.schema_version`，旧版会话 migration 适配（见 `design.md` §25.7） | M8+ | 已实现 |

## 11. 前端

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| F-01 | CLI 单次模式 | `minicoding "prompt"` | M1 | 已实现 |
| F-02 | CLI 交互会话 | `--session` REPL（含 /undo /plan /mcp） | M2 | 已实现 |
| F-03 | 流式渲染 | token 直写 stdout | M1 | 已实现 |
| F-04 | 非 TTY 降级 | 禁 spinner/颜色 | M1 | 已实现 |
| F-05 | TUI 全屏 | ratatui 多视图 | M7 | 已实现 |
| F-06 | 流式 Markdown | 增量解析渲染 | M7 | 已实现 |
| F-07 | 权限弹窗 | 非阻塞主循环（TuiPrompter） | M7 | 已实现 |
| F-08 | 任务面板 | TUI 同步显示任务进度 | M7 | 已实现 |

## 12. 嵌入与跨进程

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| E-01 | SDK Client | `ask`/`ask_stream`/`run_task` | M8 | 已实现 |
| E-02 | CallbackPrompter | SDK 用户闭包 | M4 | 已实现 |
| E-03 | HTTP/JSON-RPC server | `minicoding serve` | M8 | 已实现 |
| E-04 | MCP server | 被其他 Agent 调用（即 X-10） | M8 | 已实现 |
| E-05 | stdin/stdout NDJSON | 编辑器插件协议 | M8 | 已实现 |
| E-10 | JSON-RPC 协议 | JSON-RPC 2.0 wire types 独立 crate（`minicoding-protocol`），见 `modules.md` §15 | M6 | 已实现 |
| E-11 | HTTP/SSE server | HTTP/SSE JSON-RPC 接口，多客户端并发会话（`minicoding-server`），见 `modules.md` §16 | M8 | 已实现 |
| E-12 | ACP 适配器 | ACP stdio 适配器，可被 Zed 等客户端嵌入 | M8 | 已实现 |
| E-13 | SSE cursor 恢复 | 事件流携带 cursor（event seq），客户端断连后从 cursor 恢复 | M8 | 已实现 |
| E-14 | RehydrateRequired 信号 | broadcast 溢出时发 RehydrateRequired，客户端重拉 snapshot | M8 | 已实现 |
| E-15 | LSP server | `minicoding serve --lsp`，基于 `tower-lsp`，可被 VS Code/Neovim/Emacs/Helix 等编辑器嵌入（见 `design.md` §24、`modules.md` §16） | M8 | 已实现 |
| E-16 | LSP 语义映射 | `workspace/executeCommand`→prompt、`$/progress`→流式 token/工具进度、`minicoding/event`→事件广播（见 `design.md` §24 映射表） | M8 | 已实现 |
| E-17 | LspPrompter | 实现 `PermissionPrompter`，`window/showMessageRequest` 点对点权限交互，与 `TuiPrompter` 同构 | M8 | 已实现 |
| E-18 | LSP codeAction | `textDocument/codeAction` 提供 AI 快速操作（解释/重构/修复选中代码） | M8 | 已实现 |

## 12.5 Web 与桌面应用（M9，低优先级）

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| W-01 | Web 前端 | React 19 + TS + Vite 6 + Tailwind v4，对话/工具/权限 UI（`crates/minicoding-web`） | M9 | 已实现（现代暗色主题 + glassmorphism + 渐变 accent） |
| W-02 | 流式 SSE 渲染 | `Event::Token`/`ToolCall`/`PermissionRequest` 实时渲染，TanStack Query 增量更新 + 流式光标 | M9 | 已实现 |
| W-03 | 权限确认弹窗 | shadcn/ui Dialog 接收 `PermissionPrompt` → JSON-RPC `permission.resolve` 回传，含风险等级可视化 | M9 | 已实现（low/medium/high 三色徽章 + 4 种决策按钮） |
| W-04 | 多会话面板 | 左侧会话列表 + 右侧对话流，TanStack Query 缓存管理 | M9 | 已实现（可折叠侧栏 + 会话元数据展示） |
| W-05 | 暗色/亮色主题 | Tailwind v4 + shadcn/ui theme provider | M9 | 已实现（双主题 CSS 变量 + Zustand 持久化 + 系统偏好跟随 + FOUC 预防） |
| W-06 | Tauri 桌面壳 | Tauri 2.x + Rust sidecar（`minicoding-server`），三平台打包 `.dmg`/`.msi`/`.AppImage` | M9 | 已实现（`crates/minicoding-desktop`，feature gate `desktop`） |
| W-07 | 桌面端 OS 集成 | 系统托盘 + 全局快捷键 + 自动更新（Tauri updater 签名校验） | M9 | 已实现（系统托盘 + `Ctrl+Alt+M` 全局快捷键 + 关闭隐藏到托盘 + Tauri updater） |
| W-08 | 静态资源托管 | `minicoding serve --web ./dist` 单二进制部署 + CORS 配置 | M9 | 已实现（`tower-http::ServeDir` + SPA fallback + `--cors-origin`） |
| W-09 | 前端安全 | CSP 严格、防 XSS、权限弹窗后端校验 `prompt_id` 不可伪造、凭证不出现在前端 | M9 | 已实现（禁用 `dangerouslySetInnerHTML`、Markdown 经 React 转义、权限后端强制 C-01） |
| W-10 | 全 Rust 工具链构建 | oxlint + oxfmt + Vite (Rolldown) + Tailwind v4 (Oxide) | M9 | 已实现（package.json 内置 `lint`/`format` 脚本） |
| W-11 | 项目工作区 | 文件树浏览 + 文件预览 + workdir 展示（只读，C-03 后端强制）+ 会话内 diff + 工作区切换（Ask 审批）+ 桌面端系统编辑器打开 | M9 | 已实现（后端 5 端点 §26.9/`docs/api.md` §9.2；前端文件树/预览/diff 弹窗，切换走权限弹窗，diff 依赖 journal，桌面端 `open_workspace_file` 命令） |

## 13. 工程与质量

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| Q-01 | CI（fmt/clippy/test/audit/deny） | 全绿门禁 | M0 | 已实现 |
| Q-02 | 单元测试 ≥ 80% | 每 trait 实现 | 持续 | 已实现 |
| Q-03 | 集成测试（wiremock） | 完整 Agent 循环 | M1+ | 已实现 |
| Q-04 | 回放测试 | JSONL fixture + `ReplayPolicy` 全副作用 Deny 单测 | M3 | 已实现 |
| Q-05 | 属性测试（proptest） | `Message` JSON roundtrip + path sandbox 不变量 | M3 | 已实现 |
| Q-06 | 性能基准（criterion） | 压缩管道 100/500/1000 消息基准 | M2+ | 已实现 |
| Q-07 | 沙箱平台 CI matrix | Linux/macOS/Windows 拒绝语义 | M4+ | 已实现（三平台 CI matrix：Linux Landlock + macOS Seatbelt + Windows Job Object 编译/单测全覆盖） |
| Q-08 | cargo dist 跨平台二进制 | Linux/macOS/Windows | M10 | 已实现（`cargo-dist.toml` 配置 5 个 target + shell/powershell/homebrew/scoop 安装器） |
| Q-09 | 分发（brew/scoop/cargo install） | 三渠道 | M10 | 已实现（cargo-dist `tap`/`scoop`/`publish-jobs` 覆盖三渠道） |

## 14. Extension 扩展

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| X-20 | Extension SDK | 第三方扩展作者稳定 API（`Extension` trait + `Registrar` + `ExtensionManifest`），见 `design.md` §23、`modules.md` §17 | M5 | 已实现 |
| X-21 | Prompt contributor 注入 | 扩展通过 `Registrar` 注册 contributor 注入 prompt section，见 `design.md` §22 | M5 | 已实现 |
| X-22 | 扩展工具统一 dispatch | 扩展注册的工具仍走 `ToolRegistry` dispatch，确保权限审计一致（C-01/C-02 不被绕过） | M5 | 已实现 |

## 15. Prompt 管道

| ID | 功能 | 描述 | 里程碑 | 状态 |
|----|------|------|:---:|:---:|
| P-30 | Prompt 管道 | 9 个 `PromptContributor` 按固定顺序拼接，稳定段在前利于 prompt cache（见 `design.md` §22） | M5 | 已实现 |
| P-31 | IDENTITY.md 覆盖 | `~/.minicoding/IDENTITY.md` 覆盖默认身份 | M5 | 已实现 |

---

## 统计

| 领域 | 项数 |
|------|:---:|
| Agent 运行时 | 15 |
| LLM Provider | 8 |
| 工具系统 | 22 |
| 上下文管理 | 9 |
| 记忆 | 8 |
| 权限与安全 | 24 |
| Hooks 系统 | 13 |
| MCP 集成 | 14 |
| 可观测性 | 8 |
| 持久化与存储 | 15 |
| 前端 | 8 |
| 嵌入与跨进程 | 14 |
| 工程与质量 | 9 |
| Extension 扩展 | 3 |
| Prompt 管道 | 2 |
| Web 与桌面（M9） | 11 |
| **合计** | **183** |

> **统计口径**：含带字母后缀的子工具（T-06b `fs.multiedit`、T-08b/c/d `shell.background`/`output`/`kill`），它们有独立 ID、独立 schema 与独立实现，按独立功能项计。MVP（M0–M2）交付约 38 项；M3–M5 扩展与安全约 55 项；M6–M8 高级形态约 55 项（含 asyncRewake、Auto memory、压缩熔断、LSP 适配器等增强）；M9 Web/桌面（W-01..W-11）11 项低优先级可选（已全部实现，W-11 项目工作区含 diff 视图/工作区切换/桌面编辑器集成）。新增 Hooks（13）+ MCP client（11）+ 沙箱/审批强化（P-15..P-23）+ Plan/Undo/Todo/AGENTS.md/Auto memory + LSP 适配器（E-15..E-18）+ Web/桌面（W-01..W-11）是参考 CC/Codex 后的核心增强。

---

## 元数据：优先级、依赖与工作量

上表各功能项的"里程碑"列已隐含优先级与交付时序。本节显式补充优先级映射、关键依赖链与工作量来源，便于排期与影响评估。

### 优先级映射

优先级由里程碑推导（P0 最高，P2 最低）：

| 优先级 | 里程碑 | 含义 | 功能数 |
|:---:|------|------|:---:|
| **P0** | M0–M2 | MVP 必交付，缺则不可用 | ~38 |
| **P1** | M3–M5 | 扩展与安全，缺则不具备生产可用性 | ~55 |
| **P2** | M6–M8 / M6+ / M7+ / M8+ | 高级形态，可后续迭代 | ~74 |

- P0 功能阻塞 MVP 发布；任何 P0 延期需触发 roadmap 评审。
- P1 功能中，P-15..P-23（沙箱/审批强化）与 H-01..H-13（Hooks）是参考 CC/Codex 后的核心差异化，建议优先于同里程碑其他 P1 项。
- P2 功能中标注 `M6+`/`M7+`/`M8+` 的为"该里程碑后视情况交付"，不阻塞对应里程碑发布。

### 工作量来源

功能项本身不重复标注工作量（避免与 `dev-plan.md` 冗余）。工作量在 `dev-plan.md` 的 **task 粒度**标注（`S`=1-2d / `M`=3-5d / `L`=1-2w），一个 task 可能覆盖多个功能项。功能→task→工作量的追溯路径：

```
features.md 功能 ID (如 A-02)
   → dev-plan.md task 的"涉及功能"字段 (如 T-M2-1 涉及 A-02)
   → 该 task 的"预估工作量"字段 (如 T-M2-1 = M)
```

各里程碑工作量汇总（来自 `dev-plan.md` 附录统计）：

| 里程碑 | task 数 | 预估人日 | 覆盖功能优先级 |
|--------|:---:|:---:|:---:|
| M0 | 9 | 3 | P0（基础设施） |
| M1 | 9 | 12 | P0（MVP） |
| M2 | 9 | 12 | P0（Agent 循环） |
| M3 | 10 | 10 | P1（上下文/记忆） |
| M4 | 11 | 8 | P1（沙箱/审批） |
| M5 | 8 | 12 | P1（Hooks/MCP/Plan） |
| M6 | 5 | 6 | P2（多 Provider） |
| M7 | 4 | 10 | P2（TUI） |
| M8 | 6 | 8 | P2（SDK/Server） |
| **合计** | **71** | **81** | — |

### 关键依赖链

功能项之间的硬依赖（B 依赖 A 表示 A 必须先交付）：

```
A-01 单轮对话 ──▶ A-02 多轮 Agent 循环 ──▶ A-03 并行/串行调度
                                          ├─▶ A-04 防死循环
                                          └─▶ A-08 会话中断与恢复 ──▶ A-09 回放 ──▶ A-10 /undo

P-06 应用层路径沙箱 (M1) ──▶ P-05 内置黑名单 (M2) ──▶ P-15 OS 沙箱 (M4)
                                                       └─▶ P-19 沙箱拒绝升级 (M4)

T-01..T-04 只读工具 (M1) ──▶ T-05..T-07 写文件组 (M2) ──▶ T-08 shell.run (M2)
                                                          └─▶ T-08b..d 后台命令 (M4+)

C-03 4 级压缩管道 (M3) ──▶ C-07 压缩熔断 (M3) ──▶ H-02 PreCompact/PostCompact Hook (M5)

M-03 长期记忆 (M3) ──▶ M-07 AGENTS.md (M3) ──▶ P-24 AGENTS.md 写保护 (M3)
                  └─▶ Auto memory (design.md §8.7，约束 C-27/C-34) ──▶ C-27 隔离约束

H-01 Hook trait (M5) ──▶ H-02 10 类事件 (M5) ──▶ H-13 asyncRewake (M6+)

X-01 McpClient trait (M5) ──▶ X-02 stdio 客户端 (M5) ──▶ X-03 rmcp 完整客户端 (M6)
```

**依赖约束**：
- 跨里程碑依赖必须遵循 roadmap 依赖图（`roadmap.md` 末尾"里程碑依赖图"），如 M5 的 Hooks 依赖 M4 的沙箱就位以隔离子进程；
- 同里程碑内的依赖由 dev-plan task 的"输入"字段声明（如 T-M2-3 输入 T-M1-6、T-M1-7）；
- 反向依赖（如"取消某功能会影响哪些下游"）可通过本图反向遍历评估影响面。
