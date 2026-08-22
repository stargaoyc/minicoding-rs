> **[2026-08-21 起已被取代]** 本文档为历史快照，分析结论以
> [`project-review-20260821.md`](./project-review-20260821.md) 为准。

# minicoding-rs 改进设计文档（基于 deepseek-harness 对比）

> 来源：`docs/deepseek-harness-comparison.md` v2（源码级调研）§4 改进意见 R-01..R-11
> 日期：2026-08-19
> 本文把对比报告中的改进意见展开为可执行的设计：每项含现状、目标、数据模型、
> API 变化、实现要点、影响面、风险权衡、验收标准与工作量估计。
>
> **总原则**（与 AGENTS.md 一致）：不改变"trait 定义在 core、实现在领域 crate"的
> 依赖方向（§3.3）；不引入新依赖（§2.7）；不新增 panic 路径（§2.3）；改代码必改
> 文档（§4.1）；L0 约束实现层强制不因任何改进而放松（§5.1）。

---

## 1. 改进项总览

| 编号 | 名称 | 优先级 | 主要影响 crate | 工作量 | 类型 |
|------|------|:---:|----------------|:---:|------|
| R-01 | 会话 step 边界事件 | 高 | core / storage / server | S（1-2 人日） | 架构增强 |
| R-02 | 压缩历史可追溯 | 高 | core / context | S | 审计增强 |
| R-03 | LLM 循环打断器 | 高 | tools / core | S | 新功能 |
| R-04 | 配置分层与热重载评估 | 中 | core / cli / desktop | M（3-5 人日） | 决策记录 |
| R-05 | 工具 canonical 输出声明 | 中 | core / tools / web | M | 新功能 |
| R-06 | 只读工具并行执行 | 中 | core / tools | M | 性能 |
| R-07 | 凭证重解析 + 防陈旧写 | 中 | desktop / providers | S | 可靠性 |
| R-08 | 沙箱 denial 事实分类 | 中 | core / sandbox / server | S | 可靠性 |
| R-09 | 会话存储版本化 + 契约测试 | 低 | core / storage | M | 健壮性 |
| R-10 | 前端 snapshot 回放测试 | 低 | web | M | 测试基建 |
| R-11 | E2B 类远程沙箱 | 低 | sandbox | XL | 远期 |

建议实施批次：**批次 1**（0.2.31）：R-01 + R-02 + R-03；**批次 2**（0.2.32）：R-05 + R-06 + R-07 + R-08；**批次 3**（0.3.x）：R-04 + R-09 + R-10；R-11 挂起待 M8 SDK 场景真实需求。

---

## 2. 高优先：批次 1（0.2.31）

### 2.1 R-01 会话 step 边界事件

#### 现状

- `Runtime::run_turn`（`crates/minicoding-core/src/runtime/rt.rs:672`）是单级 turn：用户消息入库 → 循环（LLM 请求 → 工具执行 → 再请求）→ turn 结束。循环内部无"step"边界落盘。
- 存储层 `Storage` trait（`crates/minicoding-core/src/storage/trait.rs:28`）只有 `append(msg: &Message)`，消息日志按 `Message` 粒度落盘；工具调用记录在 `Message::ToolCall` 中（随 assistant 消息），工具结果在 `Message::ToolResult` 中。
- 回放（`--replay`）与懒恢复（snapshot + 事件流重放）依赖"消息序列可重建"，但无法区分"一次 LLM 请求 + 一组工具调用"的 step 边界；压缩点、中断点无法精确定位。

#### 目标

在消息日志中显式记录 step 边界，使：
1. 回放/恢复可定位"压缩前消息区间"与"被中断的 step"；
2. 为未来 fork/分支（R-01b）打底；
3. 保持向后兼容（旧会话文件可正常读取）。

#### 数据模型

在 `minicoding-core::model` 新增轻量事件枚举（不入 `Message`，避免污染对话历史）：

```rust
/// 会话控制事件（落盘于会话 JSONL 的独立事件流，不进入模型 transcript）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEvent {
    /// step 开始：一次 LLM 请求 + 其触发的工具调用。
    StepStarted { seq: u64, tool_calls: Vec<String> },
    /// step 结束：工具结果已全部回灌（含被取消工具的合成错误结果）。
    StepEnded { seq: u64 },
    /// 上下文压缩发生：被替换消息的 seq 区间（R-02 关联）。
    Compressed { from_seq: u64, to_seq: u64, summary_seq: u64 },
    /// turn 被中断（cancel/崩溃恢复补记，对齐 dsh 孤儿 turn/start 处理）。
    TurnInterrupted { reason: String },
}
```

设计意图：与 `Message` 日志分开存（同文件 JSONL 追加，带 `kind` 前缀行），因为这类事件"模型不可见"，混入消息流违反 C-05（输出不可作为指令）的精神——dsh 同样把 log-only 事件（`approval/*`、`compaction/*`）排除在 transcript 之外。

#### API 变化

- `Storage` trait 增加两个方法（默认实现 no-op 或回退到 append 兼容路径）：

```rust
fn append_session_event(
    &self,
    session: &SessionId,
    evt: &SessionEvent,
) -> BoxFuture<'_, Result<(), StorageError>>;
fn load_session_events(
    &self,
    session: &SessionId,
) -> BoxFuture<'_, Result<Vec<SessionEvent>, StorageError>>;
```

- `Runtime::run_turn` 循环体（`rt.rs` 中 LLM 请求前后）调用 `append_session_event(StepStarted/StepEnded)`；`cancel()` 路径补记 `TurnInterrupted`；`ContextManager::compress` 由 Runtime 在压缩完成后补记 `Compressed`（seq 区间由压缩前 `message_count()` 与压缩后差值推算，见 R-02）。

#### 实现要点

1. `minicoding-storage` 的 JSONL 实现：现有文件头加 `event` 行类型标记（`{"kind":"event", ...}`），`load` 时跳过；旧文件（无标记）按纯消息解析，向后兼容。
2. seq 分配：复用 Runtime 现有事件 seq（`persist_event` 的序号），保证 `SessionEvent::seq` 与 `Message` 顺序全局单调。
3. `--replay` 模式（C-06）：事件流仅作定位辅助，副作用 Deny 逻辑不变。
4. 懒恢复（`get_or_load`）：snapshot 优先策略不变；快照缺失时按 消息 + 事件流 重放，`TurnInterrupted` 提供中断点。

#### 影响面

| 项 | 内容 |
|----|------|
| 文档 | `docs/data-model.md`（新增 SessionEvent 数据结构）、`docs/design.md`（§25.8 懒恢复补充事件流角色）、`docs/api.md`（Storage trait 新增方法） |
| 测试 | storage roundtrip（事件 + 消息混合文件）、回放定位测试、崩溃补记测试 |
| 协议 | 无（HTTP/NDJSON/ACP 不暴露 SessionEvent） |

#### 风险与权衡

- 风险：事件流与消息流双写一致性。缓解：同一 JSONL 文件顺序追加，单写者（Runtime 串行化 turn）。
- 权衡：不引入 dsh 的完整事件溯源（日志即上下文投影），只加边界标记——避免大改存储层与 ContextManager 的"消息列表"模型。R-01 是"事件溯源思想的 30% 版本"，性价比优先。

#### 验收标准

- [ ] 一次含 2 个 step 的 turn 落盘含 2 对 StepStarted/StepEnded；
- [ ] cancel 后会话文件含 TurnInterrupted，`--replay` 可定位中断点；
- [ ] 旧格式会话文件（v0.2.x 生成）可正常 load 且不产生事件解析错误；
- [ ] 新增单测 ≥5 个（roundtrip / 混合文件 / 取消 / 压缩关联 / 旧格式兼容）。

### 2.2 R-02 压缩历史可追溯

#### 现状

- `ContextManager::compress`（`crates/minicoding-context/src/manager.rs:191`）实现 4 级压缩（截断/摘要/合并/丢弃），压缩后消息重写，**压缩前的消息不可见**。
- 审计 `audit.log` 记录压缩动作（是否有？需确认），但压缩"删了什么"无结构化记录，无法回答"这轮压缩掉了什么"。

#### 目标

压缩摘要消息携带引用区间（压缩前消息 seq 范围），审计可追溯；配合 R-01 的 `Compressed` 事件形成完整闭环。

#### 数据模型

`Message` 的 `metadata`（`crates/minicoding-core/src/model/message.rs:90`）新增可选字段（`#[serde(default, skip_serializing_if = "Option::is_none")]` 保持 wire 兼容）：

```rust
pub struct MessageMetadata {
    // ...现有字段...
    /// 压缩来源区间（R-02）：本消息替代了 `[src_seq_from, src_seq_to]` 的消息。
    pub compressed_range: Option<CompressedRange>,
}

pub struct CompressedRange {
    pub from_seq: u64,
    pub to_seq: u64,
    /// 被压缩的 token 总量（审计用估算值）。
    pub dropped_tokens: usize,
}
```

#### 实现要点

1. `manager.rs::compress` 各级压缩分支（截断/摘要/合并/丢弃）在生成替代消息时填写 `compressed_range`（用压缩前 `ContextSnapshot` 的 seq 边界与 `message_count()`）。
2. 熔断状态机（C-29）判定逻辑不变——只加元数据，不改判定。
3. 审计：压缩完成后经 `AuditSink::record` 落一条 `kind: Compress` 记录（含 range 与 dropped_tokens），满足 C-05 审计要求。
4. 回放：`--replay` 遇到带 `compressed_range` 的消息可展开提示"此处压缩了 N 条消息"。

#### 影响面

| 项 | 内容 |
|----|------|
| 文档 | `docs/data-model.md`（MessageMetadata 新字段）、`docs/design.md`（§3.x 压缩章节补追溯说明）、`docs/security.md`（审计事件类型补 Compress） |
| 测试 | 压缩后 range 正确性（截断/摘要/合并/丢弃四分支）、audit 记录断言 |
| 协议 | `Message` wire 新增可选字段，前后端（ts-rs 生成的 TS 类型）需同步重新生成 `gen-types` |

#### 风险与权衡

- 风险：metadata 膨胀。缓解：仅压缩替代消息带该字段，正常消息 `skip_serializing_if` 不落盘。
- 权衡：不做 dsh 的 SurfaceOp.replace 全量投影重写（改动面太大），以"引用区间标注"达到可追溯目的。

#### 验收标准

- [ ] 4 个压缩分支各有一个测试断言 range 正确；
- [ ] audit.log 出现 Compress 记录（含 range/dropped_tokens）；
- [ ] 前端 ts 类型重新生成后 `git diff --exit-code` 通过（gen-types 一致性）；
- [ ] 压缩前后 token 审计数值与 token_count 差一致（容差 5%）。

### 2.3 R-03 LLM 循环打断器

#### 现状

- 工具执行串行循环（`rt.rs` run_turn 内），连续重复调用同一工具（如同一个失败命令反复执行）只能靠 turn 超时（C-07）与压缩熔断兜底，无主动打断，浪费 token 且体验差。
- `Tool` trait（`crates/minicoding-core/src/tool/trait.rs:99`）无调用历史概念；`PermissionPolicy` 决策链（`policy/trait.rs:169`）不感知重复。

#### 目标

检测"同一 (tool, 参数) 连续重复调用"，达到阈值后向 LLM 注入 escalate 提醒（不替换工具输出、不直接禁止——对齐 dsh repeat-tool-reminder），默认阈值 [3,5,8]（3 次提醒、5 次警告、8 次停止建议）。

#### 设计

在 `minicoding-tools`（组合层）实现 `RepeatGuard` 包装器，对注册的工具做**执行前/后拦截**：

```rust
/// 重复调用守卫（R-03）：包装任意 Tool，维护调用历史。
pub struct RepeatGuard {
    inner: Arc<dyn Tool>,
    state: tokio::sync::RwLock<RepeatState>,
}

#[derive(Default)]
struct RepeatState {
    last_key: Option<String>,       // (tool, canonical args) 指纹
    streak: u32,                    // 连续重复次数
    escalated_at: u32,              // 上次升级阈值（3/5/8）
}

/// 判定结果：正常执行 / 注入提醒（额外上下文）。
pub enum GuardDecision {
    Proceed,
    Escalate { level: u8, hint: String },
}
```

#### 实现要点

1. **指纹**：`(tool.name, 参数的 canonical JSON 排序序列化)`——只对 `side_effect != None` 的工具启用（只读工具重复无害，不打断）；`shell.run` 等写工具参数含动态内容（时间戳）会因指纹不同而自然放行。
2. **注入方式**：`execute` 拦截点在权限通过（C-01 决策链之后）与真正执行之间；Escalate 时向 LLM 追加一条系统级上下文（同 turn 的 `Event::SystemContextAdded` 或工具结果前缀），**不替换工具输出**（dsh 同设计，保证模型可见历史不失真）。
3. **阈值**：`RuntimeConfig` 新增 `[tools] repeat_guard_thresholds = [3, 5, 8]`（数组，`RepeatGuard` 消费；`[]` 表示关闭）。注意 `[tools]` 段当前是死配置（对比报告已指出），本项顺带激活。
4. **打断**：第 8 次（末级）注入强提示"建议停止并换策略"，不硬中断（保留 LLM 自主性；硬中断由 turn 超时 C-07 兜底）。
5. **审计**：Escalate 事件记 audit（kind: RepeatGuard）。

#### 影响面

| 项 | 内容 |
|----|------|
| 文档 | `docs/design.md`（§7 工具执行章节）、`docs/features.md`（新增功能项）、`docs/security.md`（审计类型）、`docs/api.md`（RuntimeConfig 新字段） |
| 测试 | 连续 3 次重复触发 Escalate、不同参数不触发、只读工具不触发、阈值数组关闭生效、审计断言 |
| 协议 | 无 |

#### 风险与权衡

- 风险：误报（合法重试模式）。缓解：默认只针对写工具 + 阈值可配 + 只注入提醒不阻断。
- 权衡：不采用 dsh 的"非工具型循环打断"（检测模型纯文本重复输出）——文本指纹噪音大，首期只做工具级。

#### 验收标准

- [ ] 同一写工具连续调用 3 次 → 出现 1 次 escalate 提醒；5/8 次逐级升级；
- [ ] 中间插入不同工具/参数 → streak 重置；
- [ ] 只读工具连续调用 N 次无提醒；
- [ ] 配置 `thresholds = []` 时行为与现状一致（回归测试）。

---

## 3. 中优先：批次 2（0.2.32）

### 3.1 R-04 配置分层与热重载评估

#### 现状

- `RuntimeConfig` 分层：MINICODING_HOME > project > user > 默认；profiles 支持；W-19 已新增 `GET /config` 只读端点与 `[context]` 段读写。
- `[tools]` 段大量字段无消费方（死配置，对比报告 v2 §2.1 指出）。

#### 目标

1. 清理 `[tools]` 死配置或全部接入消费方（R-03 已接入 `repeat_guard_thresholds`，其余逐项审计）；
2. **不做热重载**（明确决策）：Rust 静态组合 + 熔断状态机（C-29）下热重载收益低风险高，写入 `docs/tech-stack.md` §13 权衡记录；
3. `GET /config` 扩展为返回完整生效配置（含 `[tools]`），供前端设置面板后续迭代。

#### 实现要点

1. 审计 `RuntimeConfig` 全部字段的消费点（grep 各 crate 引用），列出死字段清单 → 接入或标注 deprecated；
2. tech-stack.md §13 追加"热重载不做"决策记录（why：静态组合、C-29 状态机、与 W-19 重启生效语义一致）。

#### 验收标准

- [ ] 死配置清单闭合（每字段有消费方或 deprecated 标注）；
- [ ] tech-stack.md §13 有决策记录。

### 3.2 R-05 工具 canonical 输出声明

#### 现状

- `ToolResult`（`crates/minicoding-core/src/model/tool.rs:108`）：`content: ToolContent`（Text/Json）+ `is_error` + `metadata`。前端工具卡片渲染靠约定（文本截断/JSON 美化），无协议中立的"渲染意图"。

#### 目标

工具声明"输出如何被展示"（对齐 dsh `ToolOutputDefinition`：output.schema + render 纯函数 + presentationMeta），前端按 render intent 渲染卡片。

#### 设计

`Tool` trait 增加两个可选方法（默认 None，**向后兼容**）：

```rust
pub trait Tool: Send + Sync {
    // ...现有方法...

    /// 输出 JSON Schema（R-05）：声明执行结果的结构化形态。
    /// None = 仅自由文本。只对返回 ToolContent::Json 的工具提供。
    fn output_schema(&self) -> Option<&ToolOutputSchema> { None }

    /// 输出渲染意图（R-05）：纯函数，把执行结果投影为结构化渲染描述。
    /// 默认实现：文本直出 / JSON 美化。协议中立，前端据此渲染卡片。
    fn render_output(&self, result: &ToolResult) -> RenderIntent { RenderIntent::default_for(result) }
}

/// 渲染意图（协议中立，前端卡片标签渲染）。
pub enum RenderIntent {
    /// 文本直出（默认）。
    Text { content: String },
    /// 结构化列表（文件树、进程列表等）。
    List { items: Vec<ListItem>, kind: ListKind },
    /// 键值表（git diff 统计、测试结果等）。
    Table { headers: Vec<String>, rows: Vec<Vec<String>> },
    /// 代码片段（shell 输出、diff）。
    Code { lang: Option<String>, content: String },
    /// 结构化 JSON（task.* 等）。
    Json { value: serde_json::Value },
}
```

#### 实现要点

1. `ToolContent::Json` 返回的工具（`task.*`、`plan.*`、`fs.glob` 等）优先补 `render_output`；文本工具默认走 Text；
2. 前端 `ToolCallCard` 增加 `RenderIntent` 分支渲染（对应 dsh presentResult card-tagged render）；
3. `ToolResult` wire 不变（RenderIntent 是**服务端到前端**的渲染投影，可放 SSE `tool_call_finished` 事件负载的扩展字段，或前端按工具名 + output_schema 本地渲染——**推荐后者**，零协议改动：前端内置每种工具的 renderer，按 schema 校验数据合法性）。

#### 影响面

| 项 | 内容 |
|----|------|
| 文档 | `docs/api.md`（Tool trait 新方法）、`docs/features.md`（新功能项）、`docs/design.md`（§7） |
| 测试 | render_output 纯函数单测（无 IO）、前端组件测试（Vitest，R-10 基建后补） |
| 协议 | 无（推荐方案） |

#### 风险与权衡

- 权衡：dsh 由服务端下发 render intent（插件可替换），本设计选前端本地渲染器（协议零改动、实现简单）；代价是第三方扩展工具（MCP）的展示靠 schema 通用兜底（JSON 美化 + 文本截断），不享受定制卡片。若后续需要，可升级为服务端下发。

#### 验收标准

- [ ] `fs.glob`/`task.list`/`plan.list` 至少 3 个工具提供 `render_output`；
- [ ] 前端按 RenderIntent 渲染对应卡片形态；
- [ ] 未提供 render_output 的工具行为与现状一致（回归）。

### 3.3 R-06 只读工具并行执行

#### 现状

- 工具串行执行（run_turn 内循环逐个 `execute`），多文件读取（fs.read × N）串行耗时明显。
- 权限决策链（C-01）先于执行；写工具必须保持顺序语义（C-01/C-02 不可破坏）。

#### 目标

只读工具（`SideEffect::None`）在**权限决策集中完成后**有界并行（≤4），写工具保持串行。审计顺序由 step 内 seq 保证。

#### 设计

在 `Runtime` 的 step 循环中区分两个执行池：

```rust
// runtime/rt.rs step 执行段（伪代码）
let (read_calls, write_calls): (Vec<_>, Vec<_>) = tool_calls
    .partition(|tc| registry.get(&tc.name).is_read_only());
// 1) 全部权限决策先行（C-01：决策链对每个调用逐一 check，顺序执行）
let decisions: Vec<Verdict> = read_calls.iter().chain(&write_calls)
    .map(|tc| policy.check(tc)).collect();
// 2) 只读调用并行（有界滚动池）
let read_results: Vec<ToolResult> = futures::stream::iter(read_calls)
    .map(|tc| registry.execute(tc))
    .buffered(4)
    .collect().await;
// 3) 写调用串行（保持现状语义）
for tc in write_calls { registry.execute(tc).await?; }
```

#### 实现要点

1. 只在 `PermissionMode::Auto`/`accept_edits` 下启用并行；`Plan` 模式（只读工具本就唯一）同样启用；
2. 并行工具共享 `ToolContext`：`workdir`/`session` 只读共享，`tool_output_max_bytes`（C-07）按调用独立计（每调用上限不变，防止聚合超限）；
3. 沙箱：并行只读工具仍各自走 `SandboxDriver`（C-22 不放松）；shell 类工具（`shell.run` 有副作用）不进并行池；
4. 结果回灌顺序：按模型调用顺序提交（dsh commitReady 同构），保证 LLM 视角顺序稳定；
5. 取消（cancel）：并行池整体 cancel（futures 全部 drop），与现状"取消后会话可继续"（W-16）兼容——被取消调用记合成错误结果。

#### 影响面

| 项 | 内容 |
|----|------|
| 文档 | `docs/design.md`（§7 工具执行并行化）、`docs/rules.md`（若需新增约束说明——C-07 资源上限在并行下按调用独立计，需在规则中明确） |
| 测试 | 并行只读（4 文件读取耗时 < 串行 2×）、写工具顺序保持、取消并行池、audit 顺序断言 |
| 性能 | `cargo bench` 增补：多文件读取基准（当前串行 baseline 对比） |

#### 风险与权衡

- 风险：只读工具之间仍有隐含顺序依赖（罕见）。缓解：仅 `SideEffect::None` + 默认关（配置 `[tools] parallel_reads = 4`，`0` 关闭）；
- 权衡：不做 dsh 的通用并行（executionMode 全工具分类），首期只读并行已覆盖主要耗时场景。

#### 验收标准

- [ ] 4 个 fs.read 并行耗时 ≤ 串行 60%（基准）；
- [ ] 写工具串行顺序不变（回归测试：连续两次 shell.run 幂等性断言）；
- [ ] `parallel_reads = 0` 时行为与现状一致。

### 3.4 R-07 凭证重解析 + 防陈旧写

#### 现状

- C-04：凭证仅内存 + OS keyring，不落 config.toml。provider 启动时从 keyring/env 读取，运行中不重读（key 轮换需重启 sidecar）。
- desktop `save_provider_config` 无并发保护，两窗口编辑可能互相覆盖（陈旧写）。

#### 目标

1. provider 每次 LLM 请求时重解析凭证（对齐 dsh CredentialRef `resolve()` 每次操作）；缓存解析结果 ≤60s（避免每次请求 keyring 开销）；
2. desktop 配置写加 `expected_revision` 防陈旧写（对齐 dsh settings `expectedRevision`）。

#### 设计

```rust
// providers crate：凭证解析服务
pub struct CredentialResolver {
    keyring: KeyringStore,
    cache: tokio::sync::Mutex<HashMap<String, (String, OffsetDateTime)>>, // provider -> (key, cached_at)
}

impl CredentialResolver {
    /// 每次请求调用；缓存命中（<60s）直接返回，否则重读 keyring/env。
    pub async fn resolve(&self, provider: &str) -> Result<Option<String>, LlmError>;
    pub fn invalidate(&self, provider: &str); // W-19 保存后调用
}
```

desktop `save_provider_config` 签名扩展：`save_provider_config(provider: ProviderConfig, expected_revision: Option<u64>)`——Rust 侧 `config.toml` 文件头存 `revision` 计数器（原子自增），不匹配返回 `StaleWrite` 错误，前端提示刷新。

#### 影响面

| 项 | 内容 |
|----|------|
| 文档 | `docs/security.md`（C-04 补充重解析语义）、`docs/api.md`（desktop invoke 签名） |
| 测试 | 缓存命中/过期、invalidate 后重读、陈旧写拒绝、revision 自增 |
| 协议 | `GET /config` 增加 `config_revision: u64` 字段（前端比对） |

#### 风险与权衡

- 权衡：60s 缓存窗口内 key 轮换不立即生效（可接受，文档注明）；相比"每次重读"在 keyring 读的 IO 开销上折中。

#### 验收标准

- [ ] 修改 keyring 后 60s 内新请求仍用旧 key、超时后新 key 生效（mock keyring 测试）；
- [ ] `save_provider_config` 携带旧 revision 被拒（StaleWrite）；
- [ ] `GET /config` 返回 revision，前端设置弹窗保存前校验。

### 3.5 R-08 沙箱 denial 事实分类

#### 现状

- `SandboxError`（`crates/minicoding-core/src/sandbox/trait.rs:55`）仅 `Sandbox(String)` / `Io(io::Error)` 两种，沙箱拒绝（如 Landlock 路径越界、seccomp 拦截）表现为子进程非零退出 + stderr 文本，协议层无法结构化识别。

#### 目标

沙箱拒绝成为**结构化事实**（对齐 dsh `result.sandbox.denied`），各协议层（HTTP/NDJSON/ACP/LSP）透传，前端渲染"沙箱拒绝"卡片而非原始 stderr。

#### 设计

```rust
pub enum SandboxError {
    #[error("sandbox: {0}")]
    Sandbox(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// R-08：沙箱拒绝（内核级硬反馈，C-30 语义）。
    Denied {
        /// 拒绝类别：路径越界 / syscall 拦截 / 写入禁目录 / 资源限制。
        kind: SandboxDenyKind,
        /// 结构化详情（越界目标路径、被拦 syscall 名等）。
        detail: String,
        /// 原始 stderr（审计用，脱敏后透传）。
        stderr_tail: String,
    },
}

pub enum SandboxDenyKind {
    PathEscape { attempted: Utf8PathBuf, allowed_root: Utf8PathBuf },
    SyscallBlocked { syscall: String },
    WriteForbidden { path: Utf8PathBuf },
    ResourceLimit { kind: String },
    External, // 外部沙箱（sandbox-run/seatbelt）返回的拒绝，无法细分类
}
```

#### 实现要点

1. `minicoding-sandbox` 的 shell 工具包装：解析子进程退出码 + stderr 签名 → 映射到 `Denied`（Landlock: EPERM on exec/write 路径；seccomp: kill 信号 + stderr 分类）；`PathEscaped` 由 sandbox_path 前置校验直接构造（C-03 路径越界已有 `PathEscaped` 错误——把工具侧错误与 `SandboxError::Denied` 统一出口）；
2. 熔断器（C-30）判定改吃 `Denied` 结构化结果（不再靠文本匹配）；
3. `ToolResult.metadata` 增加 `sandbox_denied: Option<SandboxDenyInfo>`（wire 化，前端可渲染）；audit 记录结构化详情。

#### 影响面

| 项 | 内容 |
|----|------|
| 文档 | `docs/security.md`（§8 沙箱拒绝语义）、`docs/data-model.md`（ToolResult metadata）、`docs/api.md` |
| 测试 | 各 DenyKind 映射单测（mock stderr 签名）、熔断器吃结构化结果、前端拒绝卡片 |
| 协议 | `ToolResult.metadata` 新增可选字段（wire 兼容） |

#### 风险与权衡

- 风险：stderr 签名分类误判。缓解：`External` 兜底 + stderr_tail 保留原文审计；
- 权衡：Linux（landlock/libseccomp）做细分类，macOS/Windows 走 External 兜底（签名成熟度不足，避免误判）。

#### 验收标准

- [ ] 越界写触发 `WriteForbidden`/`PathEscape` 结构化错误，前端显示拒绝卡片；
- [ ] seccomp 拦截（如 `--no-new-privileges` 冲突）映射 `SyscallBlocked`；
- [ ] 熔断器（C-30）在 Denied 时计数、文本匹配路径删除后行为一致（回归）。

---

## 4. 低优先：批次 3（0.3.x）

### 4.1 R-09 会话存储版本化 + 契约测试

#### 现状

- JSONL 会话文件无格式版本头；serde 结构演进靠默认值容错（缺字段可读，**新增字段旧文件可读，但"由更新版本写入"的文件无显式拒绝**）。

#### 目标

- 文件头写 `format_version`（当前 `1`）；读到 `>1` 报 `SessionFormatUnsupportedError`（防静默丢事件）；
- Storage 双实现（JSONL 现状 + 可选 SQLite 后端）共享契约测试（对齐 dsh `runPersistenceContract`）。

#### 实现要点

1. JSONL 首行 `{"kind":"header","format_version":1,"app_version":"0.2.x"}`；读取校验；
2. `Storage` 契约测试提取为 `minicoding-core/tests/common/storage_contract.rs`（AGENTS.md §2.8 已有 common 目录），JSONL 与 SQLite 两实现同一套断言（append/load/list/delete/update_summary/事件流 R-01）。

#### 验收标准

- [ ] 伪造 format_version=2 的文件被显式拒绝且错误信息含版本；
- [ ] 契约测试两后端全绿。

### 4.2 R-10 前端 snapshot 回放测试

#### 现状

- Web 前端无单测基建（AGENTS.md §8.8 计划 Vitest + MSW 未落地），仅有 tsc/oxlint/build 门禁。

#### 目标

- 落地 Vitest + MSW；对 SSE 事件流做 record/replay 快照（对齐 dsh `DSH_SNAPSHOT=record/refresh/replay` 三态），覆盖关键路径"创建会话→发消息→流式渲染→权限确认"。

#### 实现要点

1. `vitest.config.ts` + `src/test/setup.ts`（MSW handler 模拟 `/sessions`、SSE）；
2. `useChat`/`useSessions` hook 测试 + `MessageList`/`ToolCallCard`/`PermissionDialog` 组件测试（含 R-05 RenderIntent 分支）；
3. snapshot 模式：`SSE_SNAPSHOT=record` 记录事件序列到 fixture，`replay` 模式断言渲染结果一致。

#### 验收标准

- [ ] `npm run test` 全绿（新增 ≥15 个测试）；
- [ ] CI 前端 job 增加 `vitest run`（AGENTS.md §8.7 对齐）。

### 4.3 R-11 E2B 类远程沙箱

#### 现状

- 仅本机沙箱（Linux Landlock / macOS Seatbelt / Windows Job Object）。

#### 目标（远期，挂起）

- `SandboxDriver` 扩展能力描述（读写/网络/进程策略），新增 `Remote` driver 指向远程 Linux 沙箱（E2B 已验证可行性）；仅 M8 SDK 嵌入场景出现真实需求时启动。

#### 前置条件

1. `SandboxDriver` 增加能力描述（`Capabilities { filesystem, network, process }`），供调用方选择；
2. 凭证/审计链路复用（远程沙箱同样受 C-04/C-05/C-30 约束）；
3. 三平台拒绝语义 CI matrix 扩展远程场景（成本高，需充分理由）。

---

## 5. 实施顺序与里程碑

| 里程碑 | 包含 | 版本建议 | 说明 |
|--------|------|:---:|------|
| 批次 1 | R-01 + R-02 + R-03 | 0.2.31 | 事件边界 + 压缩追溯 + 循环打断，独立可测、向后兼容 |
| 批次 2 | R-05 + R-06 + R-07 + R-08 | 0.2.32 | 前端渲染 + 并行 + 凭证/沙箱可靠性 |
| 批次 3 | R-04 + R-09 + R-10 | 0.3.x | 配置清理 + 存储健壮 + 前端测试基建 |
| 挂起 | R-11 | — | 等 M8 场景 |

批次间不耦合；每批次遵循 AGENTS.md §6.3 PR checklist（fmt/clippy/test/audit/deny + 文档同步 + 约束自检）。

## 6. 约束兼容性自检（AGENTS.md §5.1）

| 约束 | R-01..R-11 影响 | 结论 |
|------|----------------|------|
| C-01 副作用必须经权限 | R-06 并行仅只读工具，权限决策先行 | 不放松 |
| C-02 内置黑名单不可覆盖 | R-03 不干预黑名单链（决策后拦截注入） | 不放松 |
| C-03 路径不可越界 | R-08 结构化拒绝反而强化 | 加强 |
| C-04 凭证不可外泄 | R-07 仅重解析时机变化，不落盘不变 | 不放松 |
| C-05 输出不可作为指令 | R-02 压缩区间仅 metadata，R-01 事件不入 transcript | 不放松 |
| C-06 回放不可触发副作用 | R-01 事件流辅助定位，Deny 逻辑不变 | 不放松 |
| C-07 资源不可耗尽 | R-06 并行按调用独立计上限 | 保持 |
| C-21/22/26/27/28/29/30 | 均不触碰判定链；R-08 强化 C-30 | 保持 |

**新增约束需求**：R-06 并行资源上限（C-07 细化）、R-07 凭证缓存窗口需在 `docs/rules.md` 或 `docs/security.md` 中文字化说明（缓存 ≤60s 是 C-04 的实现细节而非放松）。

---

*本设计文档对应 minicoding-rs v0.2.30 代码状态；每批次实施前需重新核对当前代码与本文"现状"部分的一致性（AGENTS.md §7.1 先读后改）。*
