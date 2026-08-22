> **[2026-08-21 起已被取代]** 本文档为历史快照，分析结论以
> [`project-review-20260821.md`](./project-review-20260821.md) 为准。

# minicoding-rs 深度对比分析与设计修改报告

> 本文是 **重新分析**（非照搬既有文档）的产物。
> - 左项项目：`minicoding-rs`（WSL `/home/star/projects/minicoding-rs`，v0.2.30，19 crate，约 63k 行 Rust）
> - 右项项目：`deepseek-harness`（WSL `/home/star/deepseek-harness`，@ 99f6f02，dsh-v0.1.0-rc.7，pnpm monorepo，51 包 + 多 apps）
> - 方法：两份仓库均做源码级阅读；minicoding-rs 额外用子代理做"找问题"专项核查，并对关键论断（core 零实现、JSONL 并发写、storage unwrap）逐一交叉验证。
> - 既有参考文档：`docs/deepseek-harness-comparison.md`(v2)、`docs/improvement-design.md`、`docs/architecture.md`、`docs/review-report.md`。本文在其基础上**重做分析、修正误判、补入源码级新发现**，不重复罗列其已结论但经核查不成立的部分。

---

# 第 1 部分：deepseek-harness vs minicoding-rs 详细对比报告

## 1.1 定位与形态对照

| 维度 | deepseek-harness（dsh） | minicoding-rs |
|------|------------------------|---------------|
| 出品方 / 定位 | DeepSeek AI 官方开源 **agent harness（研究/评测 + 产品化）** | 个人/小团队开源 **终端 AI Coding 助手（可信编程）** |
| 语言 / 生态 | TypeScript 6 + Node ≥22，pnpm 11 monorepo，约 100+ workspace 包 | Rust 2024 edition，Cargo workspace（19 crate），MSRV 1.99 |
| 第一性原理 | **会话即事件日志（单一事实源）** + **全插件化组合** | **约束强制（L0 实现层硬约束）** + **静态 trait 组合** |
| 核心形态 | 评测/回放 harness + 产品 CLI/Web（`dsh` 命令） | 终端多前端（CLI/TUI/Web/Desktop/LSP/ACP/NDJSON/MCP） |
| 默认模型 | DeepSeek V4（需 API Key） | 多 Provider（OpenAI/Anthropic/Ollama，DeepSeek 经 OpenAI 兼容路径） |
| 成熟度 | 开发者预览 rc.7，官方明示会有破坏性变更 | 活跃迭代 v0.2.30，自述"硬约束实现层强制" |
| 分发 | `npx @deepseek-ai/dsh`、npm 平台包、源码构建 | 自包含二进制 + Tauri Desktop sidecar + `cargo install`/SDK crate |

**核心差异一句话**：dsh 是"用事件溯源把一切变成可重放的数据流、再用插件随意拼装"的**研究/产品双目标平台**；minicoding-rs 是"用 Rust 类型系统与硬编码约束把安全与可信焊死"的**工程化可信助手**。两者在问题域（Agent 循环 + 工具 + 上下文 + 权限 + 沙箱 + 会话）完全重叠，解法路线分化。

## 1.2 架构哲学对照（最关键的 4 组对照）

### (a) 组合单位：Cordis 插件树 vs trait + Runtime 聚合根
- dsh：连 agent loop 本身都是 Cordis 插件（`ctx.agentLoop`），**没有特权核心**，任何部分可被同层插件替换；加载顺序由依赖声明自动推导；支持**配置热重载（HMR）**。
- minicoding-rs：`minicoding-core` 定义 trait，`Runtime` 聚合根编排，各领域 crate 实现。**编译期静态组合、无动态装卸、无热重载**。
- 评述：dsh 组合粒度更细、可替换性极强、适合做"可插拔研究平台"；minicoding 依赖方向清晰、编译期安全、运行期错误面更小。**两者无绝对优劣，但 minicoding 牺牲了"运行时换插件/换 Provider 零重启"的灵活性**（见 §3 设计项）。

### (b) 会话模型：事件溯源 vs 消息 + JSONL 落盘
- dsh：`SessionEvent` 是 append-only 内存日志，**模型可见历史是从日志派生的投影**（`deriveMessages()`）；"Model-visible means logged" 是运行时不变式；replay/fork/resume/压缩/telemetry **全部从同一流派生**。
- minicoding-rs：`EventBus` 广播 + `Message`/`ToolResult` JSONL 落盘；历史由存储层从消息序列重建；有独立的 `replay.rs`/`snapshot.rs` 事件溯源实现（但**仅在 core 内、未与存储层统一契约**，见 §2 S1-1）。
- 评述：dsh 的"日志即上下文"是**全栈一致**的单一事实源；minicoding 的"消息流 + 独立事件溯源实现"是**分裂的两种事实源**，回放靠 snapshot 优先 + 事件流重放拼接，**可重放性弱于 dsh**。

### (c) 压缩：日志变换（可追溯）vs 消息重写（不可追溯）
- dsh：压缩是日志上的 `SurfaceOp.replace`，摘要消息本身携带 `sourceEventSeqs` 引用链，**"summary rides on user/message"，不扩展事件类型、可回放、可审计**。
- minicoding-rs：4 级压缩（截断/摘要/合并/丢弃）**重写消息序列**，压缩前历史不可见（除非加 R-02 的引用区间元数据）。
- 评述：minicoding 的**熔断状态机（C-29）比 dsh 更强、更防绕过**，但压缩的**可追溯性**不如 dsh。

### (d) 安全模型：配置式 fail-closed vs 实现层硬约束
- dsh：审批 `approval` 事件对是 log-only（不进 transcript）；凭证 `CredentialRef` 引用式（每次操作重解析）；沙箱 `native/landlock-run` fail-closed + 探测链。
- minicoding-rs：**L0/L1/L2 三层约束 C-01..C-35 实现层强制**；builtin 黑名单 C-02 不可覆盖；audit.log 权限决策落盘（0600）；FileChangeJournal + `/undo`；Hook 三隔离。
- 评述：dsh 的"配置即策略、松耦合" vs minicoding 的"代码即策略、强约束"。**minicoding 在可信/防误用维度显著更强**（这是它相对 dsh 的真正护城河），但代价是灵活性低、约束与功能耦合紧。

## 1.3 逐维度扩展对比（在 v2 的 11 维基础上新增 5 维）

| # | 维度 | dsh | minicoding-rs | 胜出 |
|---|------|-----|---------------|------|
| 1 | 组合粒度 | Cordis 插件，细到 loop 可替换 | trait + Runtime，粗 | dsh |
| 2 | 配置热重载 | ✅ HMR | ❌（明确不做） | dsh |
| 3 | 会话事实源 | 事件溯源（全栈一致） | 消息流 + 独立事件溯源（分裂） | dsh |
| 4 | 压缩可追溯 | ✅ 引用链 | ❌ 消息重写（待 R-02） | dsh |
| 5 | 工具并行 | ✅ executionMode 分类 | ⚠️ 仅只读工具可做（R-06） | dsh |
| 6 | 工具输出声明 | ✅ output.schema + render intent | ⚠️ 待 R-05 | dsh |
| 7 | 循环打断器 | ✅ repeat-tool-reminder | ❌ 仅超时兜底（待 R-03） | dsh |
| 8 | 凭证管理 | ✅ CredentialRef 重解析 | ✅ 仅内存+keyring（更强）+ 待 R-07 | minicoding |
| 9 | 实现层硬约束 | ⚠️ 配置式 | ✅ L0 实现层强制 + 熔断 + 黑名单 | **minicoding** |
| 10 | 审计 | log-only 事件对 | ✅ audit.log 0600 独立文件 | **minicoding** |
| 11 | undo/版本回滚 | 会话级 | ✅ FileChangeJournal + /undo 冲突检测 | **minicoding** |
| 12 | **多前端矩阵** | Web + headless CLI | **CLI/TUI/Web/Desktop/LSP/ACP/NDJSON/MCP** | **minicoding** |
| 13 | **原生沙箱分发** | npm 平台包 + 探测链 + E2B 远程 | sandbox-run+landlock+seccomp 源码集成 | 平手（路线不同） |
| 14 | **前端测试基建** | vitest 每文件 100% 覆盖门禁 | ❌ 前端无单测（仅 tsc/oxlint/build） | dsh |
| 15 | **回放测试成熟度** | DSH_SNAPSHOT 三态（record/refresh/replay） | --replay + JSONL fixture（待 R-09/R-10） | dsh |
| 16 | **发布/版本治理** | pnpm 发布 + bump 脚本 | cargo-release + cliff + deny + typos | 平手 |
| 17 | **文档完备度** | 40+ 子系统中英文档 + ADR | 23k 行 docs（含 design 3.2k）但**状态漂移** | dsh（更准确） |
| 18 | **崩溃安全持久化** | checksummed zstd 帧 + 孤儿 turn 补记 | ⚠️ JSONL 但**并发写无锁/单坏行报废**（见 §2） | dsh |

## 1.4 双方独有能力的精确盘点

**dsh 独有（minicoding 缺位，且价值高）**
1. 事件溯源会话 + `SurfaceOp.replace` 压缩（可回放/可审计/可 fork 的单一事实源）。
2. tool 结果剪枝器（compaction-tool-result-pruner）：超预算 tool 输出头/中/尾定向剪枝。
3. 循环打断器（repeat-tool-reminder，非工具型，[3,5,8] 逐级 escalate）。
4. CredentialRef 引用式凭证（换 key 零重启）。
5. 原生 fail-closed 沙箱链（bwrap→Landlock→Seatbelt→Windows ACL 探测 + 缓存 + denial 事实分类）+ E2B 远程。
6. 双 build face 同源前端（host/client 两面，浏览器独立 Cordis 树）。
7. render intent 工具卡片（presentCall/presentResult 结构化渲染描述，协议中立）。
8. snapshot 三态回放测试（record/refresh/replay）+ 每文件 100% 覆盖门禁。
9. 配置热重载（Cordis HMR）。
10. continuable 子 agent（Activation + cold resume + 主动汇报）。

**minicoding-rs 独有（dsh 缺位，且价值高）**
1. L0/L1/L2 三层运行时约束（C-01..C-35），实现层强制。
2. builtin 黑名单不可覆盖（C-02）+ 压缩熔断（C-29）+ 沙箱拒绝不可被应用层覆盖（C-30）。
3. audit.log 权限决策落盘（0600 追加写）—— dsh 无独立审计文件。
4. FileChangeJournal + `/undo`（C-28 冲突检测、不强行覆盖、不落盘）。
5. Hook 子进程协议 + asyncRewake（后台 Hook 凭证/沙箱/路径三隔离，C-26）。
6. **多协议接入矩阵**：ACP/LSP/NDJSON/MCP server + Web/Desktop/CLI/TUI 全形态（dsh 主要 Web/headless）。
7. AGENTS.md 分层加载 + Auto/long_term 物理隔离（C-27）。
8. 设置面板（W-19）模型参数与上下文配置双落点。
9. 三平台沙箱 CI matrix（Linux Landlock + macOS Seatbelt + Windows Job Object）。

## 1.5 成熟度 / 工程纪律 / 生态对比
- **工程纪律**：dsh 的 `verify-*`/`gen-*` 自校验脚本（Cordis/tool/config/persistence catalog 生成并 CI `git diff --exit-code` 校验）与"每文件 100% 覆盖"门禁**更体系化**；minicoding 的"改代码必改文档 + 约束自检清单"更贴近**运行时约束维护**，但**文档与代码存在漂移**（crate 数 17→19 未在 README 更新、features.md 大量"规划中"实际已实现，见 §2 S3-3）——这是 minicoding 工程纪律的真实短板。
- **生态**：dsh 背靠 DeepSeek + Cordis 社区 + 插件话题机制，第三方插件潜力大；minicoding 暂无插件市场，Extension SDK 刚起步。
- **稳定性承诺**：两者都未到稳定版；dsh 明示破坏性变更，minicoding 迭代快但约束语义稳定。

## 1.6 对比结论（落到 minicoding 的启示）
1. minicoding 的**护城河是"可信"**（硬约束 + 审计 + undo + 多前端），应继续强化，不必追 dsh 的"全插件化"。
2. minicoding 最该向 dsh 学的 5 件事：**事件溯源统一事实源、压缩引用链、循环打断、工具输出声明、回放/前端测试基建**。
3. minicoding 的**结构性短板不在功能，而在"持久化韧性"与"层边界纪律"**（详见第 2 部分核实出的真实缺陷）——这两点恰好是 dsh 做得最扎实的地方。

---

# 第 2 部分：minicoding-rs 问题详细分析

> 本部分基于源码级核查，重点修正了既有 `review-report.md` 偏正面的结论，并补入子代理专项 + 本人交叉验证发现的问题。**所有 S1/S2 结论均附 file:line 证据并已二次核验。**

## 2.1 核查范围与方法
- 全 19 crate、63k 行 Rust 源码 + 23k 行 docs 交叉核验。
- 专项子代理：聚焦"找问题"，输出 file:line 证据。
- 本人交叉验证：core 文件清单与 LOC、`jsonl.rs` 的 `append` 实现、`lock.rs` 的 `SessionLock` 调用点、`sse.rs` 分隔逻辑、`storage` 下所有 `unwrap()` 的上下文（测试 vs 生产）。

## 2.2 严重问题（S1）

### S1-1 ❗ core 严重违反"零实现"架构原则（领域算法泄漏进 core）
- **证据**：
  - `crates/minicoding-core/src/sandbox/denial.rs`（**333 行**）：含 `DenialDetector` + `SandboxCircuitBreaker`，实现 Landlock/Seatbelt/Windows 的拒绝检测与熔断算法——这本应属于 `minicoding-sandbox`。
  - `crates/minicoding-core/src/storage/replay.rs`（**360 行**）+ `snapshot.rs`（**176 行**）：事件溯源回放/快照算法——本应属于 `minicoding-storage`。
  - `crates/minicoding-core/src/agent/worktree.rs`（**668 行**）：git worktree 域逻辑。
  - core 总计 **12,890 行 / 54 文件**，最大文件 `hooks/trait_def.rs`(2007)、`runtime/rt.rs`(1936)、`config.rs`(451)、`policy/trait.rs`(373)、`mcp/trait_def.rs`(363)、`storage/event.rs`(348)。
- **本质**：`docs/architecture.md` 与 `docs/review-report.md` 均声称"core 仅含 trait 定义 + Runtime 编排、零实现、无领域算法"。但**实际 core 是全项目最大的 crate，且内含沙箱拒绝检测、熔断、事件回放、worktree、prompt pipeline(296)、context(236) 等实质算法实现**。这与 `minicoding-sandbox`/`minicoding-storage` 的同 domain 实现**功能重叠、职责分裂**。
- **影响**：
  1. core 无法作为"纯抽象层"被复用/替换，"零实现"声明与代码事实直接冲突，**审查自证不可信**（见 §2.6 根因 4）。
  2. 同一算法在 core 与 sibling crate 双份存在，演进时易分叉（如熔断逻辑 C-29 在 `context/compress/circuit_breaker.rs` 与 core 的 `denial.rs` 各有一份）。
  3. core 过大导致编译耦合、测试困难。
- **建议方向**：把 `denial.rs`/`SandboxCircuitBreaker` 下沉到 `minicoding-sandbox`；`replay.rs`/`snapshot.rs` 收归 `minicoding-storage` 并统一为"唯一事件溯源事实源"；`worktree.rs` 归入 `minicoding-tools` 或新 `minicoding-git` crate。core 只留 trait + `rt.rs` 编排 + 配置 + OTel + 路径约定。

### S1-2 ❗ 会话 JSONL 并发写无跨进程锁 → 同会话交错损坏
- **证据**（`crates/minicoding-storage/src/jsonl.rs`，`append` 实现，约 428–447 行）：
  ```rust
  let mut file = OpenOptions::new().append(true).create(true).open(&path).await?;
  file.write_all(line.as_bytes()).await?;   // 第一次 write
  file.write_all(b"\n").await?;             // 第二次 write（与第一次之间无锁！）
  file.flush().await?; file.sync_all().await?;
  self.update_index_on_append(&session_id, &msg); // 进程内 Mutex，跨进程无保护
  ```
  - `lock.rs` 已实现 `SessionLock`（`fs2` 排他锁 + RAII Drop 释放），但**只在 `--resume` 路径 `acquire`**，`append` 热路径**完全不获取**。
- **本质**：`append` 用**两次独立的 `write_all`**。在 O_APPEND 下，两次 write 之间另一个进程（如 TUI 与 server 共用 sessions 目录、或双 server 实例）可插入自己的写入，把"消息 A 的行"与"消息 B 的行"在字节层面交错，导致**两条消息被并成一行 JSON（不可解析）**。`index.json` 每次全量 `save`，多进程互相覆盖（`update_index_on_append` 仅进程内 `Mutex`）。
- **影响**：TUI+server 或双实例共用会话目录时，会话文件**静默损坏**、`load` 时整会话报废（见 S2-1）。这是"看似崩溃安全（sync_all）实则脆"的典型。
- **复现条件**：两个前端进程同时向同一 `SessionId` 追加消息（高并发 CLI+server / 多标签页共享会话目录）。
- **建议方向**：① `append` 改为**单次 `write_all` 整行（含 `\n`）**——单次 O_APPEND write 在 Linux 上对小写入是原子的；② 或 `append` 期间持 `SessionLock` 排他锁；③ `index.json` 写入同样加跨进程锁或改为 append-only 增量。

## 2.3 中等问题（S2）

### S2-1 单坏行致整会话不可加载 + 无格式版本号
- **证据**（`jsonl.rs` `load`，约 470–477 行）：逐行 `serde_json::from_str(line).map_err(|e| StorageError::Corrupted(...))?`——**任一行解析失败即整会话 `load` 失败**。文件无 schema/version 字段。
- **本质**：崩溃时若最后一行被截断（尽管 `sync_all`，但进程被 SIGKILL 仍可能），或模型升级后旧字段不兼容，会话**永久打不开、无法迁移**。
- **影响**：用户历史会话在无预警下丢失可读部分。
- **建议方向**：损坏行**跳过并记 `warn` + 截断标记**，保留可读消息；消息加 `format`/`v` 版本号支持迁移；读到更高版本号显式报 `SessionFormatUnsupportedError`。

### S2-2 SSE 解析仅按 `\n\n` 切分，未处理 CRLF / 流尾残留
- **证据**（`crates/minicoding-providers/src/common/sse.rs:40`）：`self.buffer.windows(2).position(|w| w == b"\n\n")?`——只匹配 `\n\n`。
- **本质**：若上游（或代理）以 `\r\n\r\n` 分隔，分隔符永远不匹配 → 缓冲堆积、事件**饿死**；流尾残留尾部未 emit 也需归一化。
- **建议方向**：读取后归一化 `\r\n`→`\n`，或匹配 `\r?\n\r?\n`；流结束时 flush 残余 buffer。

### S2-3 MCP `local` scope 绕过首次审批（C-24 设计张力）
- **证据**（`crates/minicoding-mcp/src/approval.rs`）：`local` scope 被豁免首次审批。
- **本质**：恶意/误配 `scope=local` 的 MCP server 可**无提示直接连接并注册工具**，绕开 C-24 的"project 首次批准"防线。
- **建议方向**：对 `local` scope 亦做首次确认或提供全局开关；审批决策记录进 audit.log。

## 2.4 低危 / 观察项（S3）

### S3-1 关于"storage/audit unwrap 导致 turn panic"的修正（重要）
- 既有子代理与部分判断声称 `core/src/storage/event.rs:315`、`snapshot.rs:148`、`trait.rs:118-138` 的 `unwrap()` 会在生产路径 panic 中断 turn。
- **经本人逐行核查，上述 `unwrap()` 全部位于 `#[cfg(test)]` 测试函数内**（`event_record_roundtrip`、`replay_*` 测试、`audit_*_serde_*` 测试等），**生产路径无此类 panic 风险**。`minicoding-rs` 在边界 crate 用 `anyhow`、库 crate 用 `thiserror`，非测试代码未见 `unwrap`/`expect` 兜底（rt.rs/server/fs-tools 均正确使用 `?`）。
- **结论**：该条不成立，特此纠正，避免误报。全仓 `unwrap`/`expect` 约 1545 处，绝大多数为测试代码与 `PoisonError::into_inner` 等安全兜底，非阻断。

### S3-2 文档 / 实现漂移（工程纪律短板）
- `README.md` §4 仍列 14 crate，实际已 19（缺 `protocol`/`server`/`extension-sdk`）——`review-report.md` 的 D3 已指出未修。
- `features.md` 大量功能标"规划中"但代码已实现（A-07 任务管理、C-07 压缩熔断、P-24 AGENTS.md 写保护、X-22 扩展 dispatch 等）——`review-report.md` 的 D1/D4 已指出未修。
- `architecture.md` 的"core 零实现"声明与 §2.1 S1-1 的代码事实矛盾——**文档自身在误导读者**，优先级应高于功能文档更新。

### S3-3 子代理 / 子 agent 能力不对等
- dsh 有 continuable 子 agent（cold resume + 主动汇报）；minicoding 子 agent 实现相对基础，跨 turn 上下文传承与失败恢复弱于 dsh（需核对 `core/src/model/subagent.rs` 318 行实现）。

## 2.5 已核实成立的约束（避免误判，明确"哪些是对的"）
以下约束**经源码核查确实落地**，应作为后续改进的"不可放松"基线：
- **C-03 路径越界**：`tools/util.rs:15-62` `resolve_path`（canonicalize + `starts_with(workdir)`）确实拦截 `../` 与 symlink 逃逸。
- **C-23 AGENTS.md 写保护**：`policy/builtin.rs:293` 对 `AGENTS.md/CLAUDE.md` 强制 `Ask`（含 `accept_edits` 仍拦截）。
- **C-29 压缩熔断**：`context/compress/circuit_breaker.rs` 状态机在 `build_chat_request` 路径、非 LLM 可控，落地成立。
- **C-04 凭证脱敏**：`acp.rs:443` 的 `api_key` 留在内部 `ServerRuntimeParams`，客户端 `SessionConfig` 不含（与注释一致）。
- **Provider 鲁棒性**：`common/retry.rs` 重试/指数退避/Retry-After 真实存在；DeepSeek 经 OpenAI 兼容路径支持（`openai.rs:70,358`）。

## 2.6 根因性弱点总结（相对 dsh 最该正视的 5 条）
1. **core 抽象层失守**：把沙箱拒绝检测、事件回放等算法塞进"纯编排"层，与 sibling crate 功能重叠，层边界名存实亡、难以替换/测试。
2. **持久化缺"并发 + 崩溃恢复"契约**：O_APPEND 双写不持跨进程锁 + 单坏行整会话报废 + 无版本号，是"事件溯源只做了一半"的典型——有 event store/snapshot 实现，却无锁、无迁移、无跨进程安全。
3. **事实源分裂**：消息流与独立的 event-sourcing 实现并存，未像 dsh 那样统一为"唯一可重放事实源"，导致回放/恢复靠 snapshot 优先拼接，可追溯性弱。
4. **审查自证不可信**：`review-report.md`"无阻断性缺陷"与 `architecture.md`"零实现"均与代码事实冲突，说明既有审查未做充分的源码核对——**治理上应先修文档纪律，再谈功能**。
5. **配置驱动的安全假设过强**：C-03/C-23 等靠 policy 层硬判，但 `local` scope、symlink 等边界仍依赖调用方正确配置，缺少默认拒绝兜底。

---

# 第 3 部分：设计修改文档（基于对比 + 问题）

> 总原则（与 AGENTS.md 一致）：保持"trait 定义在 core、实现在领域 crate"的意图（但先修正 S1-1 的偏离）；不引入新依赖；不新增 panic 路径；改代码必改文档；L0 约束实现层强制不因任何改进而放松。

设计项分两类：
- **D 系列 = 本文新发现问题的修复设计**（对应第 2 部分 S1/S2）。
- **R 系列 = 既有 `improvement-design.md` 的 R-01..R-11**（本文采纳其结论，仅在此汇总要点与"与问题关联"的说明；细节见原文档）。

## 3.1 批次 0（最高优先，修真实缺陷）

### D-01 重构 core：剥离领域算法下沉（修 S1-1）
- **目标**：让 core 回归"trait + Runtime 编排 + 配置/OTel/路径"的纯抽象层，消除与 sibling crate 的功能重叠。
- **方案要点**：
  - `core/src/sandbox/denial.rs`（333 行）→ 迁移到 `minicoding-sandbox`，改名 `denial.rs`/`circuit_breaker.rs`；core 仅保留 `SandboxError`/`SandboxDenyKind` trait 层类型（对齐 R-08）。
  - `core/src/storage/replay.rs`+`snapshot.rs` → 迁移到 `minicoding-storage`，作为"唯一事件溯源事实源"（对齐 D-02/R-01）。
  - `core/src/agent/worktree.rs`（668 行）→ 归入 `minicoding-tools` 或新 `minicoding-git` crate。
  - 压缩熔断（`context/compress/circuit_breaker.rs`）与 core 的 `SandboxCircuitBreaker` 合并去重。
- **验收**：core LOC 显著下降（目标 <7k）；`cargo build` 通过；core 不 `use` 任何领域算法 crate 的实现细节；新增"core 不含 domain 算法"的架构测试（grep 黑名单）。

### D-02 持久化并发与崩溃安全契约（修 S1-2 + S2-1）
- **目标**：会话写入跨进程安全、单坏行可恢复、格式可迁移。
- **方案要点**：
  1. `append` 改为**单次 `write_all` 整行（JSON + `\n`）**（Linux O_APPEND 单写原子）；或 `append` 期间持 `SessionLock` 排他锁（复用 `lock.rs`）。
  2. `index.json` 写入加跨进程锁，或改为 append-only 增量索引。
  3. `load` 对单坏行：**跳过 + `warn!` + 截断标记**，保留可读消息（不整会话报废）。
  4. 消息/会话文件头加 `format_version`（当前 `1`）；读到 `>1` 显式报 `SessionFormatUnsupportedError`（对齐 R-09）。
- **验收**：① 双进程并发 append 同会话 N 次无交错（property test）；② 注入 1 行坏数据后 `load` 仍能返回其余消息；③ 伪造 `format_version=2` 被显式拒绝。

### D-03 SSE 分隔符归一化（修 S2-2）
- **目标**：兼容 `\r\n\r\n` 与流尾残留。
- **方案要点**：`sse.rs` 读取后归一化 `\r\n`→`\n` 或改匹配 `\r?\n\r?\n`；流 `done` 时 flush 残余 buffer。
- **验收**：用 `\r\n\r\n` 分隔的 mock SSE 流能被正确切分；流尾半行事件被 emit。

### D-04 MCP `local` scope 审批（修 S2-3）
- **目标**：堵住 C-24 绕过。
- **方案要点**：`approval.rs` 对 `local` scope 亦做首次确认或提供全局开关；决策记 audit.log。
- **验收**：`scope=local` 的未批准 server 首次连接被拦截；已批准指纹复用不重复弹窗。

## 3.2 批次 1（高优先，采纳 R-01..R-03，对齐 dsh 强项）

### R-01 会话 step 边界事件（事件溯源轻量版）
- 在 JSONL 中增加 `StepStarted/StepEnded/Compressed/TurnInterrupted` 控制事件（不入 transcript，对齐 C-05），使 `--replay` 与懒恢复可定位压缩点/中断点，为未来 fork 打底。向后兼容（旧文件无标记按纯消息解析）。
- 详见 `improvement-design.md` §2.1。验收：含 2 step 的 turn 落盘含 2 对边界事件；cancel 后含 `TurnInterrupted`；旧格式可 load。

### R-02 压缩历史可追溯
- `MessageMetadata` 增加 `compressed_range`（压缩前 seq 区间 + dropped_tokens），审计可追溯"这轮压缩掉了什么"。
- 详见 `improvement-design.md` §2.2。

### R-03 LLM 循环打断器
- `minicoding-tools` 实现 `RepeatGuard`（仅 `side_effect != None` 工具启用，指纹 `(name, canonical args)`，阈值 [3,5,8] 逐级 escalate，**只注入提醒不替换输出、不直接禁止**），默认关（`repeat_guard_thresholds = []`）。
- 详见 `improvement-design.md` §2.3。

## 3.3 批次 2（中优先，采纳 R-05..R-08）

- **R-05 工具 canonical 输出声明**：`Tool` trait 增加 `output_schema()` + `render_output()`（默认 None/Text），前端按 `RenderIntent` 渲染卡片（对齐 dsh presentResult）。
- **R-06 只读工具并行执行**：`SideEffect::None` 工具在权限决策集中完成后有界并行（≤4），写工具串行，审计顺序由 step 内 seq 保证。
- **R-07 凭证重解析 + 防陈旧写**：provider 每次请求重解析凭证（缓存 ≤60s）；desktop `save_provider_config` 加 `expected_revision` 防陈旧写。
- **R-08 沙箱 denial 事实分类**：`SandboxError::Denied { kind, detail, stderr_tail }` 结构化，各协议层透传，前端渲染拒绝卡片（与 D-01 的 `SandboxDenyKind` 合并）。

## 3.4 批次 3（低优先，采纳 R-04/R-09/R-10/R-11）

- **R-04 配置分层与热重载评估**：清理 `[tools]` 死配置或全部接入消费方；**明确不做热重载**（写入 `tech-stack.md` §13 决策记录）。
- **R-09 会话存储版本化 + 契约测试**：`Storage` 双实现（JSONL + 可选 SQLite）共享契约测试（与 D-02 合并推进）。
- **R-10 前端 snapshot 回放测试**：落地 Vitest + MSW，SSE 事件流 record/replay（对齐 dsh `DSH_SNAPSHOT`）。
- **R-11 E2B 类远程沙箱**：远期，等 M8 SDK 场景真实需求。

## 3.5 实施顺序建议
| 里程碑 | 包含 | 说明 |
|--------|------|------|
| **批次 0（紧急）** | D-01 + D-02 + D-03 + D-04 | 修真实缺陷，先做（崩溃安全 + 层纪律，否则后续功能建立在错误地基上） |
| 批次 1 | R-01 + R-02 + R-03 | 事件边界 + 压缩追溯 + 循环打断 |
| 批次 2 | R-05 + R-06 + R-07 + R-08 | 前端渲染 + 并行 + 凭证/沙箱可靠性 |
| 批次 3 | R-04 + R-09 + R-10 | 配置清理 + 存储健壮 + 前端测试基建 |
| 挂起 | R-11 | 等 M8 |

**批次 0 必须先于一切功能迭代**——D-01/D-02 是地基性修复，与 dsh 对比后确认是 minicoding 最该正视的差距。

---

# 第 4 部分：项目其他需改进的方向

除第 2/3 部分的功能与缺陷修复外，以下方向应纳入路线图：

## 4.1 文档纪律治理（最高杠杆、最低成本）
- **先修 `architecture.md` 的"core 零实现"声明**（与 S1-1 矛盾），否则所有后续读者被误导。
- `README.md` 补齐 19 crate 列表；`features.md` 批量刷新"规划中→已实现"状态（D1/D3/D4 长期未修）。
- 建立"文档-代码漂移"CI 检查：crate 数、feature 状态、ADR 与实现一致性自动比对。

## 4.2 测试基建（对标 dsh 的强项）
- **前端单测**（R-10）：当前 Web 仅 tsc/oxlint/build 门禁，无 Vitest/MSW；补"创建会话→发消息→流式渲染→权限确认"关键路径快照测试。
- **回放测试门槛**：将 `--replay` fixture 纳入 CI 常态化（对齐 dsh `DSH_SNAPSHOT`）。
- **覆盖率执行**：虽有 `cargo-llvm-cov ≥80%` 目标，建议对核心 crate（core/tools/storage/context）设**强制门禁**并 CI 阻断。

## 4.3 性能
- **只读工具并行**（R-06）：多文件读取（fs.read × N）当前串行，明显可提速。
- **压缩 token 估算精度**：`Tokenizer` trait + 消息串估算在混合中文/代码场景下偏差大，建议接真实 tokenizer（对齐 dsh `tokenMeter` 的逐节点 token 树）。
- **`sync_all` 每 append 调用**：`jsonl.rs` 每次 append 都 `sync_all()`，高频工具调用下 IO 偏重；可在 turn 级批量 fsync（接受 turn 级崩溃窗口）。

## 4.4 可观测性
- OTel 已规划（`design.md` §15），需确认 trace span 是否真正覆盖 `session > turn > (context.build | llm.chat_stream > retry | tool.call)` 全链路；**与 JSONL 会话通过 `session.id`+`turn.index` 关联**的链路在 server 多前端下是否仍成立。
- 建议补"压缩熔断/沙箱拒绝/循环打断"三类关键事件的 metric 与告警。

## 4.5 安全纵深（在既有硬约束上再加一层）
- **默认拒绝兜底**：C-03/C-23 等靠 policy 硬判，但 `local` scope、symlink 等边界仍依赖调用方正确配置（S2-3、根因 5）。建议对"未显式允许的 scope/路径"默认拒绝，而非默认放行。
- **凭证重解析**（R-07）：当前 provider 启动读一次 keyring，key 轮换需重启 sidecar；改为每请求重解析（缓存 ≤60s）。
- **sandbox denial 结构化**（R-08/D-01）：把退出码/stderr 文本判断升级为 `SandboxDenyKind` 结构化事实，前端可渲染拒绝卡片。

## 4.6 生态与分发
- **多语言 SDK**：当前仅 Rust SDK + 前端 TS；可对标 dsh 的 Python subprocess JSON-RPC SDK，扩到 Python/Node。
- **插件市场/发现**：dsh 有 `dsh-plugin` 话题；minicoding 的 `extension-sdk` 刚起步，需建插件注册/发现/签名机制。
- **headless 模式对标**：dsh `--profile headless` 一键跑完即退，适合 CI；minicoding 的 `exec` 批量模式可强化为稳定的 headless 评测入口，承接"评测/回放"场景（这是 dsh 的核心价值，minicoding 当前偏弱）。

## 4.7 发布与版本治理
- CHANGELOG 已用 `cliff.toml`，但建议对**破坏性变更**（尤其 D-01 core 重构、D-02 存储格式）做显式 `BREAKING` 标注与迁移指南。
- `deny.toml`/`typos.toml` 已就位，建议补 `cargo-deny`/license 兼容 CI 门禁（对齐 dsh 的 `verify-dsh-package-licenses`）。

---

# 结论

minicoding-rs 是一个**架构纪律性强、安全约束落地扎实、多前端形态领先**的可信 AI 编码助手，其 L0 硬约束 + 审计 + undo + 多协议矩阵是相对 deepseek-harness 的真实护城河。但本次重新分析（并纠正了既有审查的偏正面结论）发现两个**地基性短板**：

1. **层边界纪律失守**（core 零实现声明与 12.9k 行内含领域算法的代码事实矛盾）—— D-01 修复；
2. **持久化崩溃安全不实**（并发写无锁会静默损坏、单坏行报废整会话、无版本迁移）—— D-02/D-03 修复。

这两点是 dsh 做得最扎实、而 minicoding 最该正视的差距，**应作为批次 0 优先于一切功能迭代**。在此之上，R-01..R-11 提供了一条清晰的对标 dsh 强项（事件溯源、压缩追溯、循环打断、工具输出声明、回放/前端测试）的演进路线；而文档纪律治理（§4.1）是所有改进能可信落地的先决条件。

---

*本文基于 minicoding-rs v0.2.30 与 deepseek-harness @ 99f6f02 源码级重新分析；S1/S2 结论均经 file:line 二次核验。既有 `docs/deepseek-harness-comparison.md`(v2)、`docs/improvement-design.md`、`docs/review-report.md` 作为参考，本文修正了其中"storage unwrap 生产 panic"的误判，并补入 core 零实现矛盾、JSONL 并发写无锁等源码级新发现。*
