# DeepSeek Harness 对比报告（v2，详细版）

> 参考项目：DeepSeek Harness（`deepseek-ai/deepseek-harness`，简称 **dsh**，dsh-v0.1.0-rc.7）
> 调研日期：2026-08-19（源码级调研，clone 于 `/home/star/deepseek-harness` @ 99f6f02，shallow）
> 本文是 minicoding-rs 与 dsh 的横向对比，逐项分析架构决策与工程实践，提炼对本项目的改进意见。
> 前版（v1）基于初步信息，其中语言栈结论有误（dsh 为 **TypeScript/Node pnpm monorepo**，非 Rust）；本版为源码级调研后重写。

---

## 1. 项目定位与规模

| 维度 | dsh | minicoding-rs |
|------|-----|---------------|
| 语言/生态 | TypeScript 6.0 + Node ≥22，pnpm 11 monorepo，约 100+ workspace 包（core 6 包 + context/compaction/sandbox/subagent/mcp/persistence 等） | Rust 2024 edition，Cargo workspace（18 crates），MSRV 1.99+ |
| 核心形态 | Agent **研究/评测 harness** + 产品化 CLI/Web（dsh 命令） | 终端 AI Coding 助手（CLI/TUI/Web/Desktop/LSP/ACP/NDJSON/MCP 多前端） |
| 运行时约束 | 轻（评测场景为主） | L0/L1/L2 三层约束 C-01..C-35，**实现层强制**（权限/黑名单/路径沙箱/审计） |
| 扩展机制 | Cordis 插件（Everything is a Plugin，可热重载） | `minicoding-extension-sdk`（Extension trait，编译期注册）+ Hook 子进程协议 |
| 会话模型 | 内存 append-only `SessionEvent` 日志 + 持久化 seam（JSONL-zstd / SQLite） | 磁盘 JSONL 会话存储 + snapshot + 事件流重放懒恢复 |
| 沙箱 | `native/landlock-run`（C11 直调内核 UAPI，npm 平台包）+ bwrap/Seatbelt/Windows ACL 探测链 + E2B 远程 | 自研 landlock 驱动（Linux，无 seccomp），Seatbelt（macOS），Job Object（Windows） |
| 前端 | 双 build face（host/client）同源架构，浏览器独立 Cordis 树 | Web（React 19 + Vite）+ Tauri Desktop sidecar + 终端多前端 |

**定位差异**：dsh 以"会话即事件日志"为第一性原理，服务于评测回放、深度追踪与产品化统一；minicoding-rs 以"约束强制 + 工程化"为第一性原理，服务于可信编程助手。以下对比聚焦架构决策，非功能一一对应。

---

## 2. 架构模式对比

### 2.1 核心抽象：插件树 vs trait 集 + Runtime 聚合

| 设计点 | dsh | minicoding-rs | 评述 |
|--------|-----|---------------|------|
| 组合单位 | Cordis 插件（vendor/ 目录自带框架）；**没有特权核心**，连 agent loop 都是插件（`ctx.agentLoop`） | `minicoding-core` 定义 trait + `Runtime` 聚合根编排 | dsh 组合粒度细、可替换性极强；minicoding 依赖方向清晰、编译期安全 |
| 依赖声明 | `inject` 服务声明，加载顺序由依赖表达自动推导 | trait 显式注入（`RuntimeBuilder`） | 等价能力；dsh 有**配置热重载**（HMR），minicoding 无 |
| 生命周期 | `FiberState` 状态机（PENDING→LOADING→ACTIVE→FAILED→UNLOADING→DISPOSED），插件回调返回值即 disposer，卸载逆序 | Runtime 所有权 + Drop；无插件动态装卸 | dsh 运行时动态装卸代价是运行期错误面更大；minicoding 编译期静态组合更安全 |
| 配置装载 | `Loader`（EntryTree）加载 `cordis.yml` + **profile/bundle/patch 四层叠加**（`--patch` 覆盖任意一行），支持 `!!js` 插值 | 单一 user 级 `config.toml`（MINICODING_HOME，project 层为规划项）+ profiles | dsh 的 patch 层栈与热重载是显著优势（见 §7 建议 R-04） |
| 作用域 | 每 agent 作用域注册原语（`ctx.scope`），工具/服务可 shadow | 会话级 Runtime 实例，无嵌套作用域 | dsh 支持子 agent 局部注册工具（shadow/restrict），minicoding 子 agent 共享全量工具 |

### 2.2 事件模型：append-only 日志 vs EventBus 广播 + JSONL 落盘

- **dsh**：会话 = 内存 append-only `SessionEvent` 日志（`packages/core/session`），12 种核心事件（`turn/start`、`step/start`、`assistant/chunk`、`tool/call`、`tool/result`…）；**模型可见历史是从日志派生的投影**（`Session.deriveMessages()`），"Model-visible means logged" 是运行时不变式；`SurfaceOp`（append / replace）是投影维护原语。
- **minicoding-rs**：`EventBus` 广播（Token/MessageAppended/TurnEnd…）+ 消息/任务/tool 结果 JSONL 落盘；历史由存储层重建。

**评述**：dsh 的事件日志是**单一事实源**——replay、fork、resume、压缩、telemetry 全部从同一流派生，事件顺序 = 日志 seq，天然可重放。minicoding 的落盘模型（按 role 分消息 + tool 结果）与 EventBus 分离，回放依赖额外快照机制（`--replay` + snapshot 优先）。这是 dsh 最值得借鉴的架构决策（见 §7 建议 R-01）。

### 2.3 Agent 循环：turn/step 两级 vs 单级 turn

- **dsh**：`step = 一次模型请求 + 其触发的工具调用`；`turn = 零或多个 step`（agent-loop `ReactLoopAgent`）。循环事件流：`turn/start → agent/pre-step（可 reject/改写）→ step/start → llm/stream → assistant/chunk → tool/call → pre-execute → execute → post-execute → tool/result → step/end（工具要求继续 → 下一 step）→ turn/end`。
- **minicoding-rs**：turn = 一次用户消息到完成（`Runtime::run_turn`），内部 while 循环（LLM 请求 → 工具执行 → 再请求），turn 级事件（Token/MessageAppended/TurnEnd）。

**评述**：两级模型让"step 粒度"的事件、取消、回放、暂停点更精细；minicoding 的单级 turn 在取消（CancellationToken）与中断恢复上已有方案，但 step 事件边界（工具调用前的快照点）无显式记录。建议在 JSONL 中增加 step 边界事件（见 R-01b）。

### 2.4 上下文管理：surface replace 压缩 vs 4 级压缩 + 熔断

| 设计点 | dsh | minicoding-rs |
|--------|-----|---------------|
| 压缩本质 | **日志上的 `SurfaceOp.replace` 操作**：总结事件本身是带 `sourceEventSeqs` 引用链的 `user/message`，"summary rides on user/message"，不扩展事件类型 | 压缩管线（4 级：截断/摘要/合并/丢弃），压缩后写入新消息序列 |
| 触发 | `agent/pre-step`（pressure）与 `agent/request-error`（失败恢复）双触发点 | turn 结束时按 token 阈值判定 + Runtime 熔断状态机（C-29，不可被 LLM 绕过） |
| 统计 | `ctx.tokenMeter`：`measure()` 返回逐节点 token 树 + 锚定最近成功调用的 canonical usage（省一次完整重估） | Tokenizer trait + 消息串估算 |
| 配套 | `compaction-tool-result-pruner`（超预算 tool 结果头/中/尾剪枝）；`command-compact` 人工 `/compact` | 人工压缩无；tool 结果截断由输出字节上限兜底（C-07） |

**评述**：dsh 的压缩是"日志变换"（可重放、可审计、引用链可回溯），minicoding 的压缩是"消息重写"（压缩历史不可回放）。minicoding 的**熔断状态机**（C-29）与审计更强，但压缩可追溯性不如 dsh。tool 结果剪枝器值得借鉴（见 R-02）。

### 2.5 工具系统：定义/管线/权限

| 设计点 | dsh | minicoding-rs |
|--------|-----|---------------|
| 定义 | `ToolDefinition`：ToolSchema（白名单投影，回调永不泄漏）+ **强制 canonical 输出声明**（output.schema + 纯函数 render + presentationMeta） | `Tool` trait：schema + `execute`；输出无强制 schema（ToolResult 自由文本/JSON） |
| schema 生成 | `defineTool()` DSL + ValueSchemaSpec 编译到受强制子集的 JSON Schema（推断失败回退 JsonValue），参数错误有稳定错误码 | 手写 JSON Schema（`serde_json::json!` 宏），clippy 检查 + doc |
| 执行管线 | `pre-execute`（allow/deny/ask waterfall，可重排）→ 单调 guard → `execute`（around-dispatch）→ `post-execute`（accept/replace/block）→ `finalizeContent`（恰好一次）→ `tools/result`（冻结权威结果）；`ToolExecutionToken` 不透明 Symbol 身份 | `PermissionPolicy::check` → Prompter（决策）→ 执行 → `ToolResult`；`SandboxDriver` 二次防线；审计落盘 |
| 并发 | `executionMode()`：parallel（有界滚动池）/ exclusive（屏障），结果按模型顺序提交；取消补合成错误结果保回放 | 串行工具调用 + 超时（C-07）；无并行执行 |
| 守护 | `guard(guard)` 单调守护（只能降权不能升权）；`restrict(filter)` 作用域过滤 | builtin 黑名单（C-02，优先级最高）+ 用户配置/Hook 不可覆盖 |

**评述**：dsh 的 pre/post-execute 双阶段管线（决策可被插件改写/替换，但守卫单调）与 minicoding 的 PermissionPolicy+Prompter 分离（AGENTS.md §3.9）是两种成熟范式：dsh 管线更"流式可扩展"，minicoding 更"决策集中 + 审计强制"。dsh 的**canonical 输出声明**（render 纯函数 + presentationMeta 供前端卡片渲染）与**工具并行执行**是 minicoding 可借鉴点（见 R-05、R-06）。

### 2.6 权限与安全

| 设计点 | dsh | minicoding-rs |
|--------|-----|---------------|
| 审批 | `ctx.approval`（`packages/interaction/user-approval`）：`request()` 闭环 fail-closed，`ApprovalOutcome = allowed-once/rejected/cancelled/unavailable`，仅 allowed-once 放行；每会话 `ApprovalPolicy = ask/never`（never 在 service 内强制，headless 不可绕过）；audit 对（asked/decided）是 log-only 事件，**不进模型 transcript** | `PermissionPolicy`（决策）→ `PermissionPrompter`（交互）点对点；`Verdict`（Allow/Deny/Ask/AllowAlways/DenyAlways）；builtin 黑名单 C-02 不可覆盖；决策落 audit.log（C-05 审计强制） |
| 循环打断 | `guard/repeat-tool-reminder`：非工具型循环打断器（连续重复工具调用阈值 [3,5,8] 注入 escalate 提醒，不替换工具输出）；`timeout-policy` 超时包装 | 无 LLM 循环打断（靠 turn 超时 C-07 与压缩熔断 C-29 兜底） |
| 凭证 | `CredentialRef` 分层引用：配置只存 env 变量名引用，`resolve()` **每次操作重解析**（API key 轮换零重启生效）；`describe()` 只报"已配置/来源/可写"，永不给值；空值 = 未配置 | C-04：凭证仅内存 + OS keyring，不落 config.toml 明文，不下传子进程 env；日志脱敏（前 4 字符 + `***`） |
| 审计 | approval 事件对入日志（不进 transcript） | 权限决策必须落 audit.log（0600 追加写）——minicoding 的审计更强 |

**评述**：dsh 的"approval 事件不进模型 transcript"与 minicoding 的"权限决策不参与消息流"异曲同工（都防止决策回灌 LLM 造成指令注入，对应 minicoding C-05 输出不可作为指令）。dsh 的 **CredentialRef 引用式凭证**（换 key 零重启）值得借鉴（见 R-07）。dsh 的循环打断器（非工具型）是 minicoding 缺少的一层（见 R-03）。

### 2.7 沙箱链：self-restrict-then-exec vs sandbox-run 驱动

- **dsh**：`native/landlock-run` 约 300 行 C11 直调内核 UAPI（musl 静态链接），**纯 argv 前缀 launcher**：`spawn([launcher, --ro path, --rw path, --, bash, -c, cmd])`——launcher 给自己装 Landlock ruleset 后 `execvp`，规则集跨 `execve` 继承，宿主不受影响；exit 125 = launcher 失败；**fail-closed**（内核不 enforce 就不执行）；probe 输出 `landlock: fully/partially enforced`；发布为 npm 平台可选依赖（entry + -linux-x64/-arm64）。`sandbox-local` 按 **bwrap → Landlock → Seatbelt → Windows ACL** 探测并缓存 runner 选择；denial 是结果事实（stderr 签名分类）；沙箱不可用报 `SANDBOX_UNAVAILABLE` **绝不裸跑**。远程：`ctx.fs`/`ctx.subprocess` seam 整体指向 E2B。
- **minicoding-rs**：`SandboxDriver` trait（core 定义）+ `minicoding-sandbox`（自研 landlock 驱动，`NoopDriver` 兜底；seccomp 待接入）；策略 `WorkspaceWrite`/`ReadOnly`/`DangerFullAccess`；拒绝语义测试三平台 CI matrix。

**评述**：两者都是"内核级强制 + fail-closed"路线，但 dsh 的**探测链 + 缓存 + denial 事实分类**（结构化错误而非仅退出码）与 **npm 平台包分发**值得借鉴；minicoding 的 trait 抽象 + feature gate 平台隔离更贴合 Rust 生态。E2B 远程沙箱对 minicoding 是可选扩展方向（见 R-08，低优先）。

### 2.8 会话持久化：事件溯源 seam vs 快照 + 重放

| 设计点 | dsh | minicoding-rs |
|--------|-----|---------------|
| 模型 | 内存日志 + 持久化 seam（`SessionPersistence` 抽象）：JSONL（**checksummed Zstandard 帧**，崩溃安全原子写）/ SQLite 两后端，**共同契约测试**（`runPersistenceContract`）保证等价 | JSONL 存储（Storage trait）+ snapshot + 事件流重放懒恢复（`get_or_load`），CLI `--resume` 同构 |
| 崩溃恢复 | 孤儿 `turn/start` 不截断，补合成 `turn/end { reason: interrupted }` | 会话文件完整性由存储层保证（无显式孤儿标记） |
| 格式拒绝 | 版本不匹配 → `SessionFormatUnsupportedError`（"written by a newer harness"），`ignorable` 标记防静默丢事件 | 无显式版本化（serde 结构演进） |
| 回放/fork | `Session.create(id, {seed})` 是 replay/fork 原语；`fork(source, boundary)` 要求 turn 外边界；`resume({resumeSessionId})` | `--replay`（Q-04 回放测试，副作用全 Deny）+ snapshot 优先恢复 |

**评述**：dsh 的"契约测试保证两后端等价"与"孤儿 turn 补合成结束事件"是工程细节标杆；minicoding 的懒恢复（snapshot + 事件流重放）与 dsh 的 fork 播种思想同构。**checksummed zstd 帧** 与 **格式版本化拒绝** 可借鉴（见 R-09，低优先）。

### 2.9 配置与 env 分层

- **dsh**：三层独立配置：① Cordis 插件树（bundle → profile patch → home patch → --patch 覆盖，支持热重载）；② 用户设置（`packages/settings`：schemastery schema、路径写操作、`expectedRevision` 防陈旧写、`describe({redactSecrets:true})` 对 wire 强制脱敏）；③ env 分层（`loadLayeredEnv`：process env > 调用目录 .env > $DSH_HOME/.env，已存在名字不覆盖；bootstrap-only 变量禁止写入 .env）。
- **minicoding-rs**：单一 user 级 `config.toml`（MINICODING_HOME）+ profiles；优先级 CLI > env > config.toml > 默认。

**评述**：dsh 的 `expectedRevision` 防陈旧写、`redactSecrets` wire 脱敏视图、bootstrap 变量禁止入 .env 是三个防呆设计；minicoding 的 `[provider]`/`[context]` 段已支持 W-19 设置面板读写（本次已实现），可继续吸收 dsh 的防呆细节（见 R-07b）。

### 2.10 前端架构

| 设计点 | dsh | minicoding-rs |
|--------|-----|---------------|
| 形态 | web-app（浏览器独立 Cordis 树，client face）+ headless CLI；CLI 非 TUI（boot profile 后把生命周期交给插件） | Web（React 19 + TanStack Query/Zustand）+ Tauri Desktop sidecar + 终端 CLI/TUI |
| 通信 | HTTP POST `/api`（unary/respond）+ 两条仅下行 WebSocket（events.mux / events.host）；`api-request-trust` 信任围栏（loopback/trustedHosts，DNS-rebinding 防御，`--host 0.0.0.0` 故意不支持） | HTTP/SSE JSON-RPC 2.0（`POST /sessions/{id}/messages`、`GET /sessions/{id}/events` SSE、`POST /sessions/{id}/permissions/{pid}`）；CORS 白名单 |
| 工具卡片 | `presentCall`/`presentResult` 的 card-tagged render intent（协议中立渲染描述） | 前端工具调用卡片（进行中/✓/✗ + elapsed 计时） |
| 单测/E2E | vitest（每文件 100% 覆盖门禁）+ web-stress/perf 配置 | 前端无单测基础设施（仅有 tsc/oxlint/build 门禁） |

**评述**：minicoding 的 SSE 单向流对简单场景更轻，dsh 的 WebSocket mux 更高效但复杂度高。**render intent**（工具输出 → 结构化渲染描述）与 dsh 的 web 测试矩阵（snapshot replay 对打包产物测试）值得借鉴（见 R-10，中优先）。

### 2.11 测试与工程实践

| 设计点 | dsh | minicoding-rs |
|--------|-----|---------------|
| 单元测试门禁 | vitest **每文件 100% 覆盖**（自定义 reporter）+ Windows 在 Linux 上经 wine 跑 | 覆盖率目标 ≥80%（cargo-llvm-cov）+ clippy pedantic deny |
| 回放测试 | `DSH_SNAPSHOT=record/refresh/replay` 三态快照（对构建产物测） | `--replay` + JSONL fixture + `ReplayPolicy`（Q-04） |
| 生成物校验 | typert（TS 类型 → 运行时 schema 反射）+ gen-* 目录（cordis/tool/config/persistence catalog），CI `git diff --exit-code` 校验生成物与源码一致 | `ts-rs`/`specta` 自动生成 TS 类型 + Zod schema（web `npm run gen-types` + CI 校验，AGENTS.md §8.4） |
| 架构记录 | `.agents/notes/implemented/` 数百篇带 i18n 的 ADR（Agent Note），verify 脚本强制格式 | `docs/` 全量架构文档（design/modules/rules/api/…）+ AGENTS.md 强制"改代码必改文档" |
| lint 全家桶 | oxlint + tsgolint + knip（未使用导出）+ publint + jscpd（重复代码） | cargo fmt/clippy + oxlint/oxfmt（前端） |

**评述**：minicoding 的"改代码必改文档 + 约束自检清单"（AGENTS.md §4/§5）比 dsh 的 ADR 机制更贴近运行时约束的维护；dsh 的 **snapshot 三态回放**（record/refresh/replay）与 **typert 运行时反射**思路与 minicoding 的 gen-types 一致但更成熟。jscpd 式重复代码检测在 Rust 侧可由 clippy 部分覆盖。

---

## 3. 双方特色设计盘点

### 3.1 dsh 独有（minicoding 缺位）

1. **事件溯源会话 + SurfaceOp.replace 压缩**（§2.4）：可回放、可审计、可 fork 的单一事实源。
2. **tool 结果剪枝器**（compaction-tool-result-pruner）：超预算 tool 输出头/中/尾定向剪枝。
3. **循环打断器**（repeat-tool-reminder，非工具型）：连续重复调用阈值 [3,5,8] 逐级 escalate 提醒。
4. **CredentialRef 引用式凭证**：配置只存 env 名引用，resolve() 每次操作重解析，key 轮换零重启。
5. **原生 fail-closed 沙箱链 + E2B 远程**：探测链（bwrap→Landlock→Seatbelt→Windows ACL）+ denial 事实分类 + 远程沙箱一体。
6. **双 build face 同源前端**：一套源码编译 host/client 两面，浏览器独立 Cordis 树。
7. **render intent 工具卡片**：presentCall/presentResult 结构化渲染描述，协议中立。
8. **snapshot 三态回放测试**（record/refresh/replay）+ 每文件 100% 覆盖门禁。
9. **配置热重载**（watchUserPatches + Cordis HMR）。
10. **continuable 子 agent**（Activation + cold resume + 主动汇报）+ 能力旗标校验（fail loud）。

### 3.2 minicoding-rs 独有（dsh 缺位）

1. **L0/L1/L2 三层运行时约束**（C-01..C-35），实现层强制 + 约束自检清单（rules.md §8）。
2. **builtin 黑名单不可覆盖**（C-02）+ 压缩熔断状态机（C-29）+ 沙箱拒绝不可被应用层覆盖（C-30）。
3. **audit.log 权限决策落盘**（0600 追加写，C-05/§5.5），dsh 无独立审计文件。
4. **FileChangeJournal + /undo**（C-28：恢复前比对 after，冲突不强行覆盖，不落盘）。
5. **Hook 子进程协议 + asyncRewake**（后台 Hook 凭证/沙箱/路径三隔离，C-26）。
6. **多协议接入矩阵**：ACP/LSP/NDJSON/MCP server 与 Web/Desktop/CLI/TUI 全形态。
7. **AGENTS.md 分层加载**（ProjectDocLoader）+ Auto memory 与 long_term 物理隔离（C-27）。
8. **W-19 设置面板**（本次实现）：Tauri config.toml + Web localStorage 双落点，GET /config 只读兜底。
9. **三平台沙箱 CI matrix**（Linux Landlock + macOS Seatbelt + Windows Job Object）。

---

## 4. 对 minicoding-rs 的改进意见

按性价比排序（结合本项目里程碑与 AGENTS.md 约束）：

### 高优先（可落地、收益大）

**R-01 会话日志加 step 边界事件（事件溯源轻量版）**
- 现状：JSONL 消息流无 step/turn 边界标记；回放依赖 snapshot 优先。
- 建议：消息日志中增加 `turn_started`/`turn_ended`/`step_started`（含 tool_calls 快照）事件记录（向后兼容追加字段），使 `--replay` 与懒恢复可精确定位压缩点与中断点，并为未来 fork/分支打基础。
- 收益：回放测试覆盖率提升；压缩历史可追溯（对齐 dsh SurfaceOp 思想的 30% 版本）。

**R-02 压缩历史可追溯性**
- 现状：4 级压缩是消息重写，压缩前的历史不可见。
- 建议：压缩摘要消息中带 `source_event_seqs` 引用区间（压缩前消息 seq 范围），审计时可定位"这轮压缩掉了什么"。
- 收益：对齐 dsh "summary rides on user/message + 引用链"设计，审计（C-05）更强。

**R-03 LLM 循环打断器**
- 现状：连续重复工具调用（如循环执行同一失败命令）只能靠 turn 超时兜底，体验差。
- 建议：工具层加"重复调用检测"（(tool, canonical args) 连续重复 ≥3 次注入 escalate 上下文提醒，不替换工具输出；对应 dsh repeat-tool-reminder），实现为 `minicoding-tools` 组合层包装（不违反 crate 边界）。
- 收益：防死循环 + 省 token + 用户体验。

**R-04 配置分层与热重载评估**
- 现状：单一 user 级 config.toml（project 层为规划项），白名单热重载已实现（S-22）。
- 建议：低优先——先完成 W-19 后的 `GET /config` 只读端点一致性；热重载涉及 Runtime 状态机（C-29 熔断），**不建议近期引入**（与 dsh 的 Cordis HMR 不同，Rust 静态组合改配置需重启语义明确）。标注为"明确不做"的决策可写入 docs/tech-stack.md §13 权衡记录。

### 中优先

**R-05 工具 canonical 输出声明（轻量版）**
- 现状：ToolResult 自由文本/JSON，前端卡片渲染靠约定。
- 建议：`Tool` trait 增加可选 `output_schema`（JSON Schema）与 `render_output`（纯函数返回结构化 render intent），内置工具逐步补齐（fs.read 文件树、shell 输出分类、git diff 结构化）；前端据此渲染"卡片标签"。
- 收益：前端渲染协议中立（对齐 dsh presentCall/presentResult），为 MCP 工具统一展示打底。

**R-06 工具并行执行（谨慎评估）**
- 现状：串行工具调用（C-07 超时 + 进程组约束已在）。
- 建议：read 类只读工具（fs.read/glob/grep）在 `auto` 权限下并行（有界滚动池 ≤4），写工具保持串行（保持 C-01/C-02 顺序语义）；审计按工具记，顺序由 step 内 seq 保证。
- 收益：多文件读取任务明显提速。风险：副作用顺序——限定只读工具 + 权限决策先行（PermissionPolicy 集中决策后并行），不破坏 L0。

**R-07 CredentialRef 引用式凭证（部分采纳）**
- 现状：C-04 凭证仅内存 + keyring，config.toml 不落明文（比 dsh 更严）。
- 建议：采纳 dsh 的"resolve() 每次操作重解析"语义——provider 请求时每次从 keyring/env 读（当前实现若缓存于启动时，key 轮换需重启 sidecar；改为每请求解析可零重启换 key）。同时吸收 `expectedRevision` 防陈旧写（desktop save 时携带读取时的 revision）。
- 收益：key 轮换体验对齐 dsh；防 W-19 双写覆盖。

**R-08 沙箱 denial 事实分类**
- 现状：沙箱拒绝依赖退出码/stderr 文本判断。
- 建议：`SandboxDriver` 错误类型增加 `Denied { reason, kind }`（对应 dsh result.sandbox.denied 签名分类），HTTP/NDJSON/ACP 各协议层结构化透传。
- 收益：前端可渲染"沙箱拒绝"卡片而非原始 stderr；审计更结构化。

### 低优先（远期）

**R-09 会话存储版本化 + 契约测试**
- 现状：JSONL 无显式格式版本。
- 建议：文件头写 schema 版本，读入不匹配时报 `SessionFormatUnsupportedError`（防"由更新版本写入"静默丢事件）；Storage 两实现（JSONL + 未来 SQLite）共享契约测试（对齐 runPersistenceContract）。

**R-10 前端 snapshot 回放测试**
- 现状：前端无单测（仅有 tsc/oxlint/build 门禁，AGENTS.md §8.8 已列 Vitest/MSW 计划）。
- 建议：按 §8.8 落地 Vitest + MSW；对 SSE 事件流做 record/replay 快照（对齐 dsh DSH_SNAPSHOT 三态），覆盖"创建会话→发消息→流式渲染→权限确认"关键路径。

**R-11 E2B 类远程沙箱**
- 现状：仅本机沙箱。
- 建议：远期评估 `ctx.fs`/`ctx.subprocess` seam 整体指向远程沙箱（dsh 已验证），需先完成 SandboxDriver 能力描述（读写/网络/进程）抽象；本项目 M8 SDK 场景才有真实需求，标注为远期。

---

## 5. 结论

两项目共享"Agent 循环 + 工具 + 上下文 + 权限 + 沙箱 + 会话"的核心问题域，但解法路线分化：

- **dsh** 的工程哲学是**单一事实源**（会话事件日志 + surface 投影 + 回放原语）与**全插件化组合**（Cordis + 配置层栈 + 热重载），服务评测与产品化双目标；其事件溯源、压缩引用链、tool 输出声明、循环打断器、CredentialRef 是五个最亮设计。
- **minicoding-rs** 的工程哲学是**约束强制**（L0 实现层强制 + 审计 + 黑名单 + 熔断）与**静态组合**（trait + 多协议接入矩阵），服务可信编程助手；其审计、undo、Hook 隔离、多前端矩阵是 dsh 不具备的硬能力。

**改进优先级**：R-01（step 边界事件）→ R-02（压缩可追溯）→ R-03（循环打断）为高性价比快速项；R-05/R-06/R-07/R-08 随 M9 前端迭代消化；R-09..R-11 远期。所有建议均不改变"trait 定义在 core、实现在领域 crate"的依赖方向（AGENTS.md §3.3），不引入新依赖。

---

*本文基于 `deepseek-harness` @ 99f6f02（dsh-v0.1.0-rc.7）源码调研；对应 minicoding-rs v0.2.29 代码状态。*
