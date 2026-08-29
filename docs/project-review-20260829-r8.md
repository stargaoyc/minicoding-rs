# minicoding-rs R8 全面审查报告（2026-08-29）

> 审查方式：18 个 crate 全量源码走读（87,514 行 Rust）+ 文档体系走读（24,779 行）
> + 交叉比对（文档声明 vs 代码实现）+ `cargo build/test/clippy` 实测。
> 审查对象：v0.3.9（308 commits）。
>
> 本文档与 `docs/project-review-2026*.md` 系列同构。问题编号采用
> `R8-<域>-<序号>`；严重度 P0（可被利用/功能损坏）→ P1（严重缺陷）
> → P2（纵深防御/一致性）→ P3（轻微）。

---

## 0. 审查结论摘要

### 0.1 总体评价

minicoding-rs 是一个**架构完成度非常高**的终端 AI 编码助手：单仓库 18 crate 的
职责切分干净、依赖方向单向无环、trait 集中在 core、L0 硬约束（C-01..C-35）在
实现层有真实强制点（非纸面声明）、三平台 OS 沙箱均有原生驱动、四形态前端共享
同一 Rust Runtime 与 JSON-RPC 契约、CI 10 道门禁全部落地、测试 1761 例全绿、
clippy `-D warnings` 零告警。作为个人/小团队项目，其工程化纪律远超同类。

**核心结论**：项目定位（"Claude Code 的开源替代 + 安全沙箱 + 三平台 +
多形态前端"）清晰且有差异化；架构与文档质量属上乘；但存在 **1 个安全 P0**
（journal symlink 逃逸）、**若干 C-22/权限链贯通缺口**（Web 全自动创建恒 400、
ACP/NDJSON 免二次确认）、**工具层 2 个 P0/P1 挂起与脱敏缺陷**、以及
**34 处文档-实现裂缝**。生产级可靠性尚未完全达成，但修复路径明确。

### 0.2 问题统计

| 严重度 | 数量 | 分布 |
|:---:|:---:|------|
| P0 | 4 | journal symlink 逃逸、shell.run 后台挂起、shell 脱敏区间合并缺陷、Web 全自动创建恒 400（功能损坏） |
| P1 | 15 | C-22 贯通缺口 ×3、工具挂起/脱敏、Hook 无沙箱默认、内存/上下文若干、MCP 工具越权 |
| P2 | 18 | 一致性、竞态、纵深防御、死代码 |
| P3 | 24 | 文档偏差、轻微健壮性 |
| **文档-实现裂缝** | **34** | modules.md 模块树失配、SCHEMA_VERSION 三方不一致等 |

---

## 1. 项目定位与差异化优势

### 1.1 定位

参考 Claude Code / Codex CLI，做"可自托管、三平台原生沙箱、四形态前端、
面向开发者的安全优先 AI 编码助手"。

### 1.2 相对 Claude Code 的差异化优势

| 维度 | Claude Code | minicoding-rs | 评价 |
|------|------------|---------------|------|
| 沙箱 | macOS 仅 Seatbelt、Linux/Windows 无 OS 级 | Linux landlock + seccomp(opt-in) / macOS Seatbelt / Windows Job Object | **真差异化**，三平台原生驱动 |
| 权限模型 | 单一批准流 | Policy/Prompter 双抽象 + 决策与交互分离 + builtin 黑名单不可覆盖 + 审计落盘 | 更严格、可审计 |
| 上下文管理 | 黑盒 | 4 级压缩 + 熔断 + ContextLength 紧急压缩联动 + 预测压缩 | 机制透明、可观测 |
| 记忆 | 隐含 | 长期/Auto/会话三态物理隔离 + BM25 检索 + AGENTS.md 项目记忆 | 显式、可管理 |
| 多形态 | CLI/TUI（插件受限） | CLI/TUI/Web/Desktop 四形态共享 Runtime + JSON-RPC 契约 + DTO 自动生成 | **真差异化** |
| 可观测性 | 商业闭源 | OTel 一等公民 + span 命名规范 + doctor/audit | 更透明 |
| 扩展 | plugin 闭源生态 | MCP + 自研 Extension SDK + hooks | 开放 |
| 审计 | 无 | 权限决策全量落 audit.log（0600、追加写、跨进程锁） | **真差异化** |

**主要短板**（相对 CC）：
1. **无 LLM 无关的"主动修复"闭环**：CC 的自动修复 loop、LSP 诊断联动
   仍偏弱（LSP 只做适配器、read_range 无 workdir 约束）；
2. **模型生态窄**：仅 OpenAI/Anthropic/Ollama 三类 API；无 Google Gemini、
   Bedrock、自定义网关（可经 OpenAI 兼容 API 兜底，但能力声明粗糙）；
3. **无插件市场/版本分发**：Extension SDK 有雏形但无打包/签名/校验体系；
4. **团队协作能力缺位**：无 shared session、无多人评审流（roadmap 有列）。

---

## 2. 模块化架构（18 crate）

### 2.1 总体评价

依赖方向（core ← 领域 ← tools ← 前端）单向无环，`tests/architecture.rs`
守卫测试强制白名单，core 无领域实现——**这是本项目的最大架构优点**。

### 2.2 发现的问题

| 编号 | 严重度 | 位置 | 问题 |
|------|:---:|------|------|
| R8-ARCH-1 | P2 | `server/src/runtime_builder.rs` | server 侧未接线 `PolicyPersist`（`AllowAlways` 跨会话不持久化），与 CLI 行为漂移（已确认代码无 `with_policy_persist`） |
| R8-ARCH-2 | P2 | `docs/modules.md` §16.2/§11.2/§10.2/§6.2/§7.2/§8.2/§4.2/§5.2/§3.2/§18.3 | 模块树与实际文件大量失配（providers 扁平化、journal 合并、缺 seccomp/external/ui/memory 等文件、误列 TanStack Router routes/） |
| R8-ARCH-3 | P2 | `crates/minicoding-mcp/src/server/expose.rs:154` | MCP server 模式下 `call_tool` 直执行工具**不经 PermissionPolicy 且不落审计**（注释声明有意，但 `shell.run` 等破坏性工具经 Claude Desktop 等外部 MCP client 可无审计执行） |
| R8-ARCH-4 | P2 | `crates/minicoding-sandbox/src/windows.rs` | `WindowsJobDriver` 用 FIFO 队列跨 `apply`/`post_spawn` 传递策略，并发 spawn 时策略可能错配（文档已承认残余风险，列入 roadmap 但未排期） |
| R8-ARCH-5 | P3 | `crates/minicoding-desktop/src/sidecar.rs` vs `serve.rs` vs `main.rs` | token 掩码逻辑三处重复，可漂移 |
| R8-ARCH-6 | P3 | 仓库根 | 误提交调试产物：`crates/minicoding-sdk/err.txt`、`crates/minicoding-tools/subagent_artifact.txt` |

---

## 3. AI Provider 与工具系统

### 3.1 Provider 总体

OpenAI/Anthropic/Ollama 实现完整：流式、重试+抖动（splitmix64）、
capabilities 探测、ContextLength 紧急压缩联动、`<tool_output>` 边界包裹
（含 `</tool_output>` 注入防护）、token 计数（tiktoken-rs）。

### 3.2 Provider 问题

| 编号 | 严重度 | 位置 | 问题 |
|------|:---:|------|------|
| R8-PR-1 | P1 | `providers/src/openai.rs:167` | 推理模型 `top_p` 门控仅前缀匹配（o1/o3/o4/gpt-5），OpenAI 兼容推理模型会漏网致 400 |
| R8-PR-2 | P2 | `providers/src/openai.rs:486` | refusal + `finish_reason="stop"` 时推送双重 `Stop`（`Filtered`+`EndTurn`），消费端可能错过 Usage delta |
| R8-PR-3 | P2 | `providers/src/anthropic.rs:121` | 最后一条消息为 `tool_result` 时 `cache_control` 断点放 tool_result 块上，工具输出每轮变化致缓存永不命中 |
| R8-PR-4 | P2 | `providers/src/ollama.rs:130,177` | 每次请求读 `OLLAMA_KEEP_ALIVE`/`OLLAMA_NUM_CTX` 环境变量，无运行时锁定 |
| R8-PR-5 | P2 | `providers/src/common/mod.rs:25` | `mask_key` 死代码（`#[allow(dead_code)]`） |
| R8-PR-6 | P3 | `providers/src/openai.rs:255` | `api_base` 为空时 URL 变成 `/chat/completions` |
| R8-PR-7 | P3 | `providers/src/common/sse.rs:24` | SSE 缓冲硬上限 16 MiB，内存受限场景偏大 |
| R8-PR-8 | P3 | `providers/src/common/mod.rs` | `mask_key` 若保留，需与 tools/shell 脱敏逻辑收敛（另见 R8-TL-3） |

### 3.3 工具系统总体

工具 schema/副作用标注/权限链/审计齐全。`wave scheduling`（相邻只读并行、
副作用串行）实现正确，且明确拒绝启发式 DAG（理由充分）。

### 3.4 工具问题

| 编号 | 严重度 | 位置 | 问题 |
|------|:---:|------|------|
| R8-TL-1 | **P0** | `tools/src/shell/run.rs:192,203` | **后台进程持管道 FD 时工具永久挂起**：`sh -c 'setsid sleep 100 & echo done'` 使 `stdout_task.await` 永不返回（`setsid` 逃逸 `killpg`，管道写端不关，EOF 不到）。超时路径与成功路径均受影响，**违反 C-07**。`shell/background.rs:372` 同构 |
| R8-TL-2 | **P0** | `tools/src/shell/run.rs:419` | `redact_secrets` 区间合并用 `dedup_by` **丢弃后区间而非取并集**：`(5,15)` 与 `(10,20)` 重叠时保留前者，`15..20` 段未脱敏——凭证可能泄漏 |
| R8-TL-3 | P1 | `tools/src/shell/run.rs:203` | `unsafe { libc::killpg }` 缺 `// SAFETY:` 注释（AGENTS.md §2.6） |
| R8-TL-4 | P1 | `tools/src/web/fetch.rs:151` | `cfg!(test)` 在 `fetch_and_convert` 中条件跳过 SSRF 校验——被测代码含测试分支，行为双轨 |
| R8-TL-5 | P2 | `tools/src/shell/run.rs:102` | `timeout_ms=0` 导致立即超时 kill（schema 允许 minimum 0），可被 LLM 用作 DoS |
| R8-TL-6 | P2 | `tools/src/util.rs:86` | `atomic_write` 临时文件名固定 `{path}.minicoding.tmp`，同目标并发写冲突 + metadata/set_permissions TOCTOU |
| R8-TL-7 | P3 | `mcp/src/client/rmcp.rs:71` | inflight 合并键用 `DefaultHasher`（非加密、Value Hash 碰撞概率虽低但非零） |
| R8-TL-8 | P3 | `tools/src/web/fetch.rs` | body 上限派生自 `max_output_bytes`（已修复），但转换后文本未二次截断，超长 HTML→文本仍可超预算（需确认） |

---

## 4. 上下文管理（4 级压缩 + 长期记忆）

### 4.1 总体

4 级压缩管道（L1 裁剪 → L2 摘要 → L3 删除 → L4 熔断）实现完整；预测压缩、
`fixed_overhead` 预算、ContextLength 紧急压缩联动（PT4-3）是亮点。
记忆三态物理隔离（long_term/auto/session）+ AGENTS.md 项目记忆 + BM25 检索。

### 4.2 问题

| 编号 | 严重度 | 位置 | 问题 |
|------|:---:|------|------|
| R8-CTX-1 | P1 | `core/src/runtime/rt.rs:658` | `force_compress` 失败时返回原始 `ContextLength` 错误，熔断/降级链真实错误被覆盖，审计无法区分"LLM 400"与"压缩熔断" |
| R8-CTX-2 | P1 | `core/src/runtime/rt.rs:674` | 紧急压缩重试路径二次 `drain` `pending_hook_contexts`（首次已消费），hook 上下文重复注入逻辑混乱 |
| R8-CTX-3 | P1 | `context/src/manager.rs:216` | `dispatch_compress_hook` 用 `std::env::current_dir()`（进程级）而非会话 workdir，server 多会话下失真 |
| R8-CTX-4 | P2 | `context/src/manager.rs:408` | `compress()` 依赖 `build_chat_request` 前置设置的 `fixed_overhead` 缓存，外部调用 `force_compress` 时阈值失真 |
| R8-CTX-5 | P2 | `core/src/runtime/rt.rs:1297` | 结果排序 `unwrap_or(usize::MAX)` 兜底（防御性，可接受但应注释） |
| R8-CTX-6 | P3 | `context/src/weight.rs:25` | `Role::System` 分支死代码（上已 return） |
| R8-MEM-1 | P1 | `memory/src/auto_contributor.rs:144` | `long_term` 加载失败静默吞错（对比 auto_md 有 warn），排障无法区分"不存在"与"加载失败" |
| R8-MEM-2 | P1 | `memory/src/auto_contributor.rs:162` | `over_limit` 只检查 auto_md 不检查 long_term；限内时 long_term 快照被丢弃（与"可选长期记忆快照"注释不符） |
| R8-MEM-3 | P1 | `memory/src/auto_contributor.rs:175` | BM25 检索 `hits.join("\n\n")` 无大小上限，可远超 `max_chars` 注入 system prompt |
| R8-MEM-4 | P1 | `memory/src/auto.rs:199` | `add_entry` 更新同 topic 条目时忽略传入 `confidence`，固定 `+0.1` 递增，无法表达"降置信度"语义 |
| R8-MEM-5 | P2 | `memory/src/vector.rs:147` | BM25 IDF 公式非标准（`+1.0` 消除负 IDF），与标准 BM25 行为偏差未文档化 |
| R8-MEM-6 | P2 | `memory/src/auto.rs:347` | `evict_until_fit` 每轮全量渲染 O(n²) |
| R8-MEM-7 | P2 | `memory/src/auto.rs:193` | `load_entries` 不取 `save_lock`，跨进程并发 load/save 无 hash 校验兜底（long_term.rs 有，auto.rs 无） |
| R8-MEM-8 | P3 | `memory/src/retrieval.rs:63` | auto 覆盖 long_term 同标题无告警 |

---

## 5. 安全权限模型与三平台 OS 沙箱

### 5.1 权限模型总体评价

Policy/Prompter 分离、builtin 黑名单优先级最高（不可被 Hook/配置覆盖）、
决策全量审计、AGENTS.md 写保护（C-23）、路径沙箱 + OS 沙箱双防线、
ReplayPolicy、拒绝熔断（C-30）——**L0 约束的绝大部分在实现层被真实强制**。

### 5.2 安全 P0/P1

| 编号 | 严重度 | 位置 | 问题 |
|------|:---:|------|------|
| R8-SEC-1 | **P0** | `journal/src/journal_impl.rs:355` | `ensure_not_symlink` 仅 `symlink_metadata(path)` 查**末段**组件；中段目录符号链接（`workdir/link/x`，link→`~/.ssh`）可穿透。`validate_restore_path` 组件级检查同样不解析中段 symlink。**C-03/C-28 逃逸** |
| R8-SEC-2 | P1 | `hooks/src/script.rs:43` | `ScriptHook` OS 沙箱**默认不注入**（`with_sandbox` 可选），未注入时 Hook/asyncRewake 子进程无内核隔离（C-26 弱化）；需确认 RuntimeBuilder 是否显式注入 |
| R8-SEC-3 | P1 | `policy/src/builtin.rs:315` | fork bomb 检测仅匹配字面量 `:(){`，空格变体 `: () {` 等可绕过 |
| R8-SEC-4 | P1 | `policy/src/redact.rs:79` | `redact_line` 逐行处理，多行凭证（PEM 私钥等）不脱敏 |
| R8-SEC-5 | P1 | `hooks/src/script.rs:131` | 超时 `killpg` 仅 Unix；Windows 孙进程成孤儿（Job Object 关闭时异步清理，超时窗口内可消耗资源） |
| R8-SEC-6 | P2 | `storage/src/index.rs:107` | `SessionIndex::save` 未做 `tighten_existing` 权限收紧（audit.rs/jsonl.rs 有，index.rs 无） |
| R8-SEC-7 | P2 | `storage/src/jsonl.rs:349` | `update_index_on_append` 跨进程锁 best-effort 失败仅 warn，索引可能漏会话/留幽灵条目 |
| R8-SEC-8 | P2 | `sandbox/src/hardening.rs:30` | `harden_process` 仅 Linux（PR_SET_DUMPABLE/RLIMIT_CORE），macOS/Windows 无进程硬化 |
| R8-SEC-9 | P2 | `sandbox/src/seccomp.rs:46` | seccomp deny-list 缺 `clone3`/`io_uring_*` 等现代攻击面（且默认 feature 关闭，见 R8-DOC-*） |
| R8-SEC-10 | P2 | `policy/src/replay.rs:37` | ReplayPolicy 依赖 MCP 工具 `side_effect` 自报，误报 None 则回放放行 |
| R8-SEC-11 | P3 | `storage/src/audit.rs:36` | audit.lock 随日志保留无清理 |

### 5.3 沙箱隔离强度评估

| 平台 | 驱动 | 强度 | 残余风险 |
|------|------|------|---------|
| Linux | landlock（文件）+ seccomp(opt-in, syscall) | **强** | seccomp 默认关（UDP/DNS 外泄通道开放）；deny-list 缺 clone3/io_uring |
| macOS | Seatbelt profile | 中强 | 路径含括号拒绝生成且无降级（fail-closed，但用户可能被迫关沙箱）；无进程硬化 |
| Windows | Job Object（无文件系统隔离） | **弱** | 仅进程树/资源限制，**无 OS 级文件隔离**；策略 FIFO 错配；TOCTOU 无兜底 |

**结论**：Linux 沙箱设计最完整；Windows 是最薄弱环（C-03 在 Windows 上仅有
应用层路径校验，无 OS 二次强制）。建议 roadmap 明确 Windows AppContainer 或
文档化容器建议。

---

## 6. 四形态前端（CLI/TUI/Web/Desktop）共享 Runtime 一致性

### 6.1 总体

四形态共享同一 Rust Runtime + `minicoding-protocol` JSON-RPC 契约 + ts-rs DTO
自动生成；权限交互全部走 Prompter 点对点（Interactive/Tui/Server/Lsp/Callback）；
事件 seq 单写者收敛。R8 后 server 能力矩阵已收拢（AGENTS.md 注入 +
git/web/memory/ui.ask）。

### 6.2 问题

| 编号 | 严重度 | 位置 | 问题 |
|------|:---:|------|------|
| R8-FE-1 | **P0（功能损坏）** | `web/src/components/layout/Sidebar.tsx:50` | Web 新建会话选"全自动"（full-access）时只发 `{preset:"full-access"}`，**从不带 `confirm_danger:true`**；后端 `http.rs:599` 强制要求 → **Web 上全自动/外部沙箱模式永远 400 创建失败**（`ackDanger` 复选框存在但值未上传，client.ts 的 CreateSessionBody 也无该字段） |
| R8-FE-2 | P1 | `web/src/hooks/useTurnControl.ts:59` | 运行中切"全自动"（bypass_permissions）同样不带 confirm → 恒 400 |
| R8-FE-3 | P1 | `server/src/acp.rs:96`、`server/src/ndjson.rs:641` | ACP/NDJSON 会话创建**无 C-22 二次确认**（HTTP 侧 SEC-2 已修，stdio 侧 fail-open），per-session 可直建 bypass 会话 |
| R8-FE-4 | P1 | `web/src/hooks/usePermissions.ts` | 权限弹窗不因 `permission_resolved`/服务端超时自动关闭（`dismiss` 定义但从未使用）；另一标签页裁决后本页弹窗常驻 |
| R8-FE-5 | P1 | `tui/src/runtime_bridge.rs:131` | TUI 会话切换在 turn 进行中无法中断，`SwitchSession` 排在 `run_turn` 之后，界面假冻结 |
| R8-FE-6 | P1 | `server/src/http.rs:767,798` | `DELETE /sessions/{id}` 不阻止已排队/运行中的 turn（cancel 仅置 token，Arc 存活任务仍执行写存储） |
| R8-FE-7 | P1 | `server/src/ndjson.rs:531`、`server/src/acp.rs:556` | NDJSON/ACP 缺 `turn_gate`（LSP 有），并发 turn 事件双份转发 |
| R8-FE-8 | P2 | `server/src/sse.rs:57` | SSE `data:` 不含 `seq`（在 `id:` 字段），前端 EventDto 声明 `seq` 必填但恒 undefined，cursor 恢复依赖浏览器隐式 Last-Event-ID |
| R8-FE-9 | P2 | `server/src/http.rs:554` | HTTP 创建会话 workdir 只 canonicalize 校验不回写规范化值（NDJSON 回写），锚点不一致 |
| R8-FE-10 | P2 | `server/src/ndjson.rs:245,380` | `ResolvePermission` 遍历全部会话触发磁盘懒恢复放大 |
| R8-FE-11 | P2 | `tui/src/app.rs:398` | TUI 工具行双显（ToolCallStarted push 一行 + MessageAppended 再 push 一行） |
| R8-FE-12 | P2 | `web/src/App.tsx:255` | 中断后"等待权限确认"横幅残留 |
| R8-FE-13 | P2 | `server/src/http.rs:767` | `send_message` 排队无上限（C-07） |
| R8-FE-14 | P2 | `server/src/sse.rs:47` | 畸形 `Last-Event-ID` 回退 seq=0 全量重放（含已决 permission 事件） |
| R8-FE-15 | P3 | `web/src/hooks/useChat.ts:54` | 乐观消息 id `optimistic-${Date.now()}` 同毫秒冲突 |
| R8-FE-16 | P3 | `tui/src/app.rs:672` | `/tokens /status /model /plan /undo` 全部"暂不支持"，与 CLI 能力矩阵漂移 |

---

## 7. 文档完备性（34 处裂缝）

### 7.1 P0 文档-实现裂缝

| 编号 | 位置 | 问题 |
|------|------|------|
| R8-DOC-1 | `rules.md:294` | §8 描述运行时启动执行 `assert_constraints()` 自检函数——**代码中不存在**（grep 全仓无匹配）。最核心的安全自检声明无实现 |
| R8-DOC-2 | `rules.md:179` | C-34 声称 `/memory auto show/off/clear` 命令——SlashCommand enum 无 Memory 变体，未实现 |
| R8-DOC-3 | `features.md:221` | F-02 声称 REPL 含 `/mcp` 命令——不存在（mcp 仅 CLI 子命令） |
| R8-DOC-4 | `features.md:155` | H-08 PreCompact/PostCompact "未接线"——R8 已接线，文档未更新 |
| R8-DOC-5 | `api.md:665,669` + `features.md:213` | SCHEMA_VERSION：文档声称 1/2，代码 `event.rs:50` 为 **3** |
| R8-DOC-6 | `modules.md` 多处 | 模块树与实际文件大量失配（详见 §2.2 R8-ARCH-2） |
| R8-DOC-7 | `design.md:2950` / `modules.md:888` | §26.2/§18.3 描述 TanStack Router + routes/ 目录——实际未采用路由库（AGENTS §8.2 已注明），文档未同步 |

### 7.2 P1 内部不一致

| 编号 | 位置 | 问题 |
|------|------|------|
| R8-DOC-8 | `modules.md` ×6 vs `tech-stack.md:219` | seccomp "待接入" vs "已接 opt-in" 冲突 |
| R8-DOC-9 | `hooks.md:42` vs `features.md:155,160` | SessionStart/asyncRewake 接线状态冲突 |
| R8-DOC-10 | `hooks.md:342` vs `rules.md:139` | asyncRewake 协议描述残留 task_id/wake_prompt 幽灵字段 |
| R8-DOC-11 | `modules.md:494` vs `features.md` X-12/13/14 | MCP 进程池/预热/inflight "未实现" vs "已实现" |
| R8-DOC-12 | `modules.md:938` vs `features.md` W-07 | 关窗行为"隐藏托盘" vs "直接退出" |
| R8-DOC-13 | `modules.md:910` vs CHANGELOG 0.3.9 | server 能力矩阵 DOC-3 注已过时 |
| R8-DOC-14 | `features.md:252` / `modules.md:35` vs `tech-stack.md` | Vite 6 vs 8.1 版本冲突 |
| R8-DOC-15 | `features.md:326` | 统计注"四段相加=205"算术错误（54+84+46+20=204）；底部汇总表行和=180≠205 |

### 7.3 P2 过时信息

- `modules.md:331` persist 注已实现未同步；`modules.md:888` TanStack Router 未标"未采用"；
- `hooks.md:360` asyncRewake "阶段 6+ 不含 MVP"过时；`tech-stack.md:200` 列 insta 但未使用。

### 7.4 结论

文档体量巨大（24,779 行）且总体质量高，但**与代码的同步维护明显滞后**
（多个"已实现"与"未实现"并存）。建议：① 模块树以 `tree src` 自动生成校验；
② 功能项状态列改由 CI 检查（存在性 grep）；③ SCHEMA_VERSION 等常量加文档断言测试。

---

## 8. 工程化质量

| 维度 | 评估 |
|------|------|
| CI/CD | **优秀**：10 道门禁（fmt/clippy -D warnings/test/coverage≥80%/audit/deny/typos/三平台 matrix/web/desktop），SHA 钉版工具链，concurrency 取消旧 run |
| 测试 | 1761 例全绿；`cargo-llvm-cov` 门禁 ≥80%；但集成测试偏少（server SSE 断线重放、Web confirm_danger 链路、MCP inflight、Windows 沙箱均无端到端） |
| Lint | clippy `-D warnings` 零告警；workspace 级 lint 收敛（ENG-8） |
| 版本管理 | SemVer + CHANGELOG + cliff 自动化；`git-cliff` 生成规范 |
| 覆盖缺口 | **无**：错误路径、竞态、并发双 POST、SSE 重放、C-22 端到端、Windows 策略错配 |
| 仓库卫生 | 误提交 `err.txt`/`subagent_artifact.txt`；根目录 `__pycache__/`（已 gitignore 但应清理）；`tmp/` 未跟踪 |

---

## 9. 生产级可靠性风险清单（Top 10）

1. **【P0】journal symlink 中段逃逸**（R8-SEC-1）——/undo 恢复可写入工作区外
2. **【P0】shell.run 后台挂起**（R8-TL-1）——`setsid &` 后工具永久阻塞，违反 C-07
3. **【P0】shell 脱敏区间合并缺陷**（R8-TL-2）——凭证可能部分回灌
4. **【P0】Web 全自动创建恒 400**（R8-FE-1）——Web/Desktop 用户无法启用沙箱外模式
5. **【P1】ACP/NDJSON C-22 免确认**（R8-FE-3）——stdio 侧可绕过二次确认
6. **【P1】MCP server 工具免权限免审计**（R8-ARCH-3）——外部 MCP client 可直调破坏性工具
7. **【P1】Hook 默认无沙箱**（R8-SEC-2）——第三方 Hook 子进程无内核隔离
8. **【P1】force_compress 错误丢失**（R8-CTX-1）——熔断后循环 400 隐患
9. **【P1】BM25 检索无上限**（R8-MEM-3）——system prompt 预算失真
10. **【P1】Web 权限弹窗不自动关闭**（R8-FE-4）——服务端裁决后 UI 卡死

---

## 10. 架构设计与改进建议

1. **Windows 沙箱升级**：Job Object → AppContainer/受限令牌（`windows-sys` 已引入），
   或文档化"Windows 上请用容器"并加 doctor 提示（C-22 对 Windows 当前不成立）。
2. **`SandboxDriver` trait 扩展**：`apply` 返回关联句柄供 `post_spawn` 消费，
   消除 Windows 策略 FIFO 错配（roadmap 已列，应排期）。
3. **MCP server 权限插槽**：`ToolExposer` 增加可选 `PermissionPolicy + AuditSink`
   注入（默认 fail-closed 或显式声明"外部客户端自负其责"）。
4. **Hook 沙箱默认开**：RuntimeBuilder 统一注入 SandboxDriver；`with_sandbox` 由
   可选改为默认+显式 opt-out。
5. **`assert_constraints()`**：要么实现（启动自检），要么从 rules.md §8 删除并
   改为文档化的"CI 静态校验 + 架构守卫测试"。
6. **规则/常量文档断言**：SCHEMA_VERSION 等常量加 compile-time 测试，防文档漂移。
7. **进程硬化跨平台**：macOS（`persona`/`getattrlist`）与 Windows（`PPL`/DEP）补
   齐 core dump 防护，至少文档化残余风险。
8. **并发 turn 治理**：NDJSON/ACP 补 `turn_gate`（与 LSP 对齐）；DELETE 后真正
   取消排队任务（加 generation token）。
9. **工具超时兜底**：`read_capped` 任务加超时（如剩余 timeout 或独立 30s），
   杜绝 `setsid` 类逃逸导致的永久挂起。
10. **文档同步机制**：模块树自动生成 + 功能项状态 CI 校验，而非人工维护。

---

## 11. 用户体验问题

| 编号 | 位置 | 问题 |
|------|------|------|
| R8-UX-1 | `web/src/hooks/usePermissions.ts` | 权限弹窗不自动关闭，用户点"允许"得 404，无引导 |
| R8-UX-2 | `tui/src/runtime_bridge.rs:131` | 会话切换假冻结，无"正在等待当前 turn 结束"提示 |
| R8-UX-3 | `web/src/components/layout/Sidebar.tsx` | 全自动模式复选框勾选后无任何反馈（恒 400），应灰化或透传 |
| R8-UX-4 | `tui/src/app.rs:672` | 5 个斜杠命令"暂不支持"，应移除或实现 |
| R8-UX-5 | `server/src/http.rs:351` | CORS 非法来源静默丢弃，拼错表现为被拒无提示 |
| R8-UX-6 | `macos.rs:102` | 路径含括号直接沙箱失败，用户被迫降级，应友好提示 |
| R8-UX-7 | `server/src/http.rs:326` | `--no-auth` 时任意本机 Web 页可完全控制（文档已注释，需 UX 警告） |

---

## 12. 修复建议优先级排序

**第一批（安全 P0 + 功能 P0）**：R8-SEC-1（journal symlink）、R8-TL-1（挂起）、
R8-TL-2（脱敏）、R8-FE-1（Web confirm_danger）。

**第二批（安全 P1 + 贯通缺口）**：R8-FE-2/3（C-22 贯通）、R8-ARCH-3（MCP 权限）、
R8-SEC-2（Hook 沙箱默认）、R8-CTX-1/2/3、R8-MEM-1/2/3/4、R8-TL-3/4。

**第三批（一致性/纵深）**：R8-FE-4..14、R8-SEC-3..10、R8-PR-1..5、
R8-CTX-4..6、R8-MEM-5..8。

**第四批（文档 + 卫生）**：R8-DOC-1..34、R8-ARCH-6（误提交文件）、R8-ARCH-5。

---

*审查日期：2026-08-29。审查者：AI 审查代理（R8）。*
