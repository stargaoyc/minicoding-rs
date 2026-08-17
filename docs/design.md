# 详细设计文档

本文是 `minicoding-rs` 的核心设计文档，描述 Agent 循环、上下文管理、工具系统、LLM 抽象、流式处理、子 Agent、记忆与权限等关键机制的详细设计与 Rust 伪代码。

> 约定：本文伪代码使用 Rust 2024 edition 语法；`async fn in trait` 假设 MSRV ≥ 1.99（edition 2024 稳定门槛）；省略 `Arc`/`Send`/`Sync` 等细枝末节，仅表达设计意图。

---

## 1. 核心抽象总览

```
Runtime
 ├── Session { id, messages, config, ctx_manager }
 ├── AgentLoop           (驱动一轮对话)
 ├── ContextManager      (消息历史 + token 预算 + 压缩)
 ├── ToolRegistry        (注册并派发 Tool，含 MCP 远程工具)
 ├── LlmProvider         (流式对话)
 ├── PermissionPolicy    (副作用授权，融合 ApprovalMode × SandboxPolicy)
 ├── PermissionPrompter  (点对点交互)
 ├── HookRegistry        (10 类生命周期 Hook，见 hooks.md)
 ├── SandboxDriver       (OS 级隔离，见 security.md §8)
 ├── ProjectDocLoader    (AGENTS.md 分层加载，见 §8.6)
 ├── FileChangeJournal   (会话内文件改动回滚，见 §17)
 ├── Storage             (JSONL 持久化)
 └── EventBus            (向前端广播 Event，仅通知无回复)
```

`Runtime` 是所有状态的聚合根，由 Frontend 构造一次后长期持有。所有可替换能力以 trait 对象（`Arc<dyn Trait>` 或泛型）注入。新增的 `HookRegistry`/`SandboxDriver`/`ProjectDocLoader`/`FileChangeJournal` 是参考 Claude Code 与 Codex CLI 设计的扩展组件，各自职责独立、与既有抽象正交，详见对应章节。

---

## 2. Agent 循环详细设计

### 2.1 循环不变量

1. `Session.messages` 始终保持"合法消息序列"：`system? → (user → assistant → tool_result*)*`。
2. 每轮循环要么产生最终回复，要么产生 ≥1 个工具调用，绝不静默退出。
3. 任意中断后，`messages` 与磁盘 JSONL 一致（每条消息写盘后再广播）。

### 2.2 主循环伪代码

```rust
impl Runtime {
    pub async fn run_turn(&self, user_input: UserInput) -> Result<TurnOutcome> {
        let span = tracing::info_span!("turn", session = %self.session.id);
        let _enter = span.enter();

        // 1. 构造用户消息并入库
        let user_msg = Message::user(user_input.text, user_input.attachments);
        self.storage.append(&self.session.id, &user_msg).await?;
        self.ctx.append(user_msg.clone()).await;
        self.emit(Event::MessageAppended(user_msg)).await;

        loop {
            // 2. 准备请求：注入 system prompt + 工具定义 + 压缩后的历史
            let req = self.ctx.build_chat_request(&self.tools, &self.config).await?;

            // 3. 调用 LLM（流式）
            let mut stream = self.provider.chat_stream(req).await?;
            let mut acc = DeltaAccumulator::default();

            self.emit(Event::TurnStreamingStarted).await;
            while let Some(delta) = stream.next().await {
                let delta = delta?;
                match delta {
                    Delta::Text(s) => {
                        self.emit(Event::Token(s.clone())).await;
                        acc.push_text(s);
                    }
                    Delta::ToolCall(tc_delta) => acc.push_tool_call(tc_delta),
                    Delta::Usage(u) => acc.usage = Some(u),
                }
            }

            let assistant_msg = acc.finalize();
            self.storage.append(&self.session.id, &assistant_msg).await?;
            self.ctx.append(assistant_msg.clone()).await;
            self.emit(Event::MessageAppended(assistant_msg.clone())).await;

            // 4. 无工具调用 → 终止
            if assistant_msg.tool_calls.is_empty() {
                self.emit(Event::TurnEnd { stop_reason: StopReason::EndTurn }).await;
                return Ok(TurnOutcome::Finished(assistant_msg));
            }

            // 5. 执行工具调用（无副作用并行、有副作用串行，见 §2.3）
            let results = self.execute_tool_calls(&assistant_msg.tool_calls).await?;

            for (id, result) in &results {
                let msg = Message::tool_result(id.clone(), result.clone());
                self.storage.append(&self.session.id, &msg).await?;
                self.ctx.append(msg.clone()).await;
                self.emit(Event::MessageAppended(msg)).await;
            }

            // 6. 循环回到步骤 2，让 LLM 基于工具结果继续
        }
    }
}
```

主循环的 6 个步骤对应"写入用户消息 → 构建请求 → 流式调用 LLM → 落盘 assistant 消息 → 判止/执行工具 → 落盘 tool_result 并回到步骤 2"。两个关键不变量由代码结构保证：消息先写盘（`storage.append`）再入上下文（`ctx.append`）再广播，崩溃时磁盘与内存一致；无工具调用时立即 `TurnEnd` 退出，有工具调用则进入 §2.3 的并行/串行调度。`emit` 全程异步广播事件供 frontend 渲染与 OTel 记录。

### 2.3 工具执行（并行 + 串行 + 权限）

工具执行遵循两条硬规则：

1. **无副作用工具（`SideEffect::None`）可并行执行**：同一轮中多个只读工具调用（`fs.read`/`fs.grep`/`fs.glob`/`git.diff` 等）用 `FuturesUnordered` 并发派发，降低长延迟叠加。
2. **有副作用工具（`FileWrite`/`Command`/`Network`）必须严格串行**：按 LLM 返回的 `tool_calls` 顺序逐个执行，前一个完成（含权限决策与审计落盘）后才启动下一个。理由：副作用间往往存在隐式依赖（如先 `fs.write` 再 `shell.run cargo build`），并行会导致竞态、重复授权、审计顺序混乱、回滚不可追溯。

实现上把本轮调用按 `side_effect` 分桶：先并发跑无副作用桶，再顺序跑有副作用桶。权限决策独立于该规则——每个副作用调用执行前都走完整的 `policy.check → prompter.prompt` 流程。

```rust
impl Runtime {
    async fn execute_tool_calls(
        &self,
        calls: &[ToolCall],
    ) -> Result<Vec<(ToolCallId, ToolResult)>> {
        let mut results: Vec<(ToolCallId, ToolResult)> = Vec::with_capacity(calls.len());

        // 1. 分桶：无副作用 → 并行；有副作用 → 串行
        let (readonly, side_effect): (Vec<&ToolCall>, Vec<&ToolCall>) =
            calls.iter().partition(|c| {
                self.tools.get(&c.name)
                    .map(|t| t.side_effect() == SideEffect::None)
                    .unwrap_or(true) // 未知工具按只读处理，dispatch 时报错
            });

        // 2. 无副作用：并发执行（权限策略对只读默认 Allow，但仍过 check 以支持 deny 规则）
        let ro_futs = readonly.iter().map(|call| self.run_one(call));
        let mut ro_stream = futures::stream::iter(ro_futs).buffer_unordered(8);
        while let Some(r) = ro_stream.next().await {
            results.push(r?);
        }

        // 3. 有副作用：严格串行，逐个完成后再启动下一个
        for call in side_effect {
            results.push(self.run_one(&call).await?);
        }

        // 4. 按 LLM 原始顺序回填，保证 tool_result 序列与 tool_calls 一一对应
        results.sort_by_key(|(id, _)| {
            calls.iter().position(|c| c.id == *id).unwrap_or(usize::MAX)
        });
        Ok(results)
    }

    /// 单个工具调用的完整执行：权限解析 → 派发 → 审计 → 事件。
    async fn run_one(&self, call: &ToolCall) -> Result<(ToolCallId, ToolResult)> {
        let pctx = PermissionContext::from_session(&self.session, call);
        // 2a. 纯决策
        let verdict = self.policy.check(&call.name, &call.input, &pctx).await?;

        // 2b. 若需交互，走点对点 prompter（非 broadcast）
        let decision = match verdict {
            Verdict::Allow => Decision::Allow,
            Verdict::Deny(r) => Decision::Deny(r),
            Verdict::Ask(prompt) => {
                // 广播通知（仅展示/审计，无回复通道，可克隆）
                let prompt_id = prompt.id.clone();
                self.emit(Event::PermissionRequested {
                    id: prompt_id.clone(),
                    tool: prompt.tool.clone(),
                    summary: prompt.summary.clone(),
                    risk: prompt.risk.clone(),
                }).await;
                // 点对点解析（InteractivePrompter / NonInteractivePrompter / TuiPrompter / CallbackPrompter）
                // prompt 在此处 move 进 prompter，故提前 clone id
                let d = self.prompter.prompt(prompt).await;
                self.emit(Event::PermissionResolved {
                    id: prompt_id, decision: d.clone(),
                }).await;
                d
            }
        };

        match decision {
            Decision::Deny(reason) => {
                self.audit(call, &decision, None).await;
                return Ok((call.id.clone(),
                    ToolResult::error(format!("permission denied: {reason}"))));
            }
            Decision::Allow => {}
        }

        self.emit(Event::ToolCallStart(call.clone())).await;
        let started = Instant::now();
        let res = self.tools.dispatch(call, &self.tool_ctx()).await;
        let elapsed = started.elapsed();
        self.emit(Event::ToolCallEnd {
            id: call.id.clone(), ok: res.is_ok(), elapsed,
        }).await;
        self.audit(call, &Decision::Allow, Some(&res)).await;   // 审计落盘
        Ok((call.id.clone(), res.unwrap_or_else(ToolResult::error)))
    }
}
```

要点：
- `run_one` 把"权限-派发-审计"封装为单一闭环，无副作用桶与有副作用桶复用同一逻辑，仅调度方式不同。
- `Decision` 已不含 `Ask`（见 `api.md` §3.6）：交互在 `prompter` 内完成，`run_one` 拿到的永远是终态决策，避免半开状态。
- 审计（`self.audit`）无论 Allow/Deny 都落盘，详见 `security.md` §7。
- 串行桶保证了"写文件 → 编译"这类隐式顺序不被打乱；LLM 若显式需要并行写入，应拆成多轮（每轮一个写），由模型自行决策。

### 2.4 停止条件与防御

| 条件 | 行为 |
|------|------|
| `stop_reason == EndTurn` | 正常结束 |
| `stop_reason == MaxTokens` | 截断警告，提示用户继续 |
| 连续工具调用次数 ≥ `max_tool_iters`（默认 50） | 强制终止并报错 |
| 单轮总耗时 ≥ `turn_timeout` | 取消并保留现场 |
| 相同工具调用连续重复 ≥ 3 次（防死循环） | 注入提示并降级 |

---

## 3. 上下文管理详细设计

### 3.1 设计目标

- 始终把"最重要"的消息保留在窗口内；
- token 预算精确（基于真实分词器）；
- 压缩过程对 LLM 透明（不破坏对话连贯性）；
- 可配置、可观测、可回滚。

### 3.2 消息权重模型

每条消息赋予权重 `w ∈ [0,1]`，决定被压缩的优先级（权重越低越先被压缩/丢弃）：

```
w = base(role) * recency * sticky * manual_pin
```

| 因子 | 取值 |
|------|------|
| `base(system)` | 1.0（永不压缩） |
| `base(user)` | 0.9 |
| `base(assistant)` | 0.6 |
| `base(tool_result)` | 0.4（最易压缩） |
| `recency` | `1 - i / N`（越旧越低） |
| `sticky` | 包含错误/未提交变更的消息 ×1.5 |
| `manual_pin` | 用户标记 `pin` ×2.0 |

### 3.3 压缩策略：分层管道

当 `ctx.tokens > budget * 0.85` 时触发压缩管道，逐级尝试：

```
Level 1: 工具结果裁剪
    - 大于阈值的 tool_result 截断为 "前 K 行 + ... + 后 K 行 + 元信息"
    - 已被后续消息引用的旧 tool_result 替换为摘要占位

Level 2: 旧消息摘要
    - 对权重最低的 N 条消息调用 LLM 生成摘要
    - 摘要替换原文，标注 [summarized @ ts]

Level 3: 滚动窗口
    - 仅保留最近 W 条消息 + 系统消息 + 摘要
    - 丢弃最旧的非 sticky 消息

Level 4: 硬截断
    - 兜底，按 token 数从尾部保留，记录告警
```

### 3.4 Token 预算分配

```
budget_total = model.context_window
budget_reserved = output_tokens (默认 4096) + safety_margin (1024)
budget_usable = budget_total - budget_reserved

  ┌─ system prompt + tool schemas  : ~15%
  ├─ long-term memory summary      : ~10%
  ├─ recent messages (窗口)        : ~60%
  ├─ tool results (current turn)   : ~10%
  └─ headroom                      : ~5%
```

### 3.5 ContextManager 接口

```rust
pub struct ChatRequest {
    pub system: SystemPrompt,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub params: GenerationParams,
}

pub trait ContextManager: Send + Sync {
    async fn append(&self, msg: Message);
    async fn build_chat_request(
        &self,
        tools: &ToolRegistry,
        config: &RuntimeConfig,
    ) -> Result<ChatRequest>;
    async fn snapshot(&self) -> ContextSnapshot;
    async fn restore(&self, snap: ContextSnapshot);
    fn token_count(&self) -> usize;
}
```

### 3.6 压缩熔断与防 Thrash（参考 Claude Code）

压缩管道（§3.3）最危险的失效模式是 **Thrash Loop**：压缩后立即又填满 → 再次压缩 → 再填满，烧光 token 预算且不产生有效输出。参考 CC 的熔断机制，Runtime 维护压缩计数器与状态机：

```
build_chat_request
   │
   ├─ token_count ≤ budget * 0.85  → 正常发送，重置失败计数
   │
   └─ token_count > budget * 0.85  → 触发压缩管道（§3.3 L1→L4）
        │
        ├─ 压缩成功（token 降到阈值下）→ 失败计数清零，发送
        ├─ 压缩失败（LLM 摘要调用失败等）→ 失败计数 +1
        │    ├─ 失败计数 < 3  → 降级链（§3.8）重试
        │    ├─ 失败计数 = 3  → 熔断：注入错误 "Context compression failed 3 times.
        │    │                  Possible causes: tool results too large, or budget too small.
        │    │                  Try /clear or reduce tool output size." 中止本轮
        │    └─ 失败计数 ≥ 5  → 强制 TurnEnd，保留现场供 /resume
        │
        └─ 压缩后立即又超阈值（Thrash 检测）
             └─ 连续 2 次"压缩完即超" → 熔断，同上错误
```

熔断阈值可配（`[context] compress_fail_threshold = 3`，`thrash_threshold = 2`）。熔断事件打 OTel span event（`compress.circuit_breaker`），属性 `fail_count`/`thrash_count`，便于诊断。该机制与 §2.4 的 `max_tool_iters`、`security.md` §8.8 的沙箱拒绝熔断器三者互补：分别防"工具死循环""沙箱拒绝死循环""压缩死循环"。

### 3.7 压缩后状态保留清单（参考 Claude Code）

压缩是破坏性操作（L2 摘要替换原文、L3 滚动窗口丢弃旧消息），但某些跨压缩必须保留的状态需显式持久化。参考 CC 的压缩后重新注入机制，Runtime 维护一份"压缩不可丢失"状态清单，在 `build_chat_request` 时从清单恢复：

| 状态 | 保留方式 | 理由 |
|------|---------|------|
| 系统 prompt（含长期记忆 §8.2、AGENTS.md §8.6） | 压缩后从磁盘重新注入 | 权重 1.0 不参与压缩，但需防 L3 滚动窗口误删 |
| 会话名 / 自定义标题 | 存 session 元数据，不进 messages | UI 显示与 `/resume` 列表需要 |
| `PermissionMode`（Plan/AcceptEdits/Default） | 存 session 元数据 | 压缩不应改变权限模式（Plan 模式压缩后仍是 Plan） |
| `ApprovalMode` × `SandboxPolicy` 预设 | 存 session 元数据 | 同上 |
| 活跃的 Plan 文件路径（§16） | 存 session 元数据 | 压缩后仍能 `/plan open` 继续未完成计划 |
| 子 Agent CWD（§7） | 存子 Agent handle 元数据 | 子 Agent 压缩后仍能正确解析相对路径 |
| `allowed_prompts` 预批准缓存（§16.4） | 存 session 元数据 | Plan 批准后的预批准不应因压缩失效 |
| 任务列表（§18） | 存 session 元数据 + Event::TaskUpdated | 任务进度跨压缩保留 |
| `FileChangeJournal`（§17） | 独立于 messages，不受压缩影响 | 文件回滚能力不因压缩丢失 |
| Hook 上下文（`SessionStart` 注入的） | 压缩后丢失（与 CC 一致） | 动态信息过期合理；若需保留用 `inject_context` 重新注入 |

**实现**：`Session` 结构体新增 `meta: SessionMeta` 字段，存放上述非 messages 状态。`ContextManager::build_chat_request` 在组装 `ChatRequest` 时从 `meta` 恢复系统 prompt 段、预批准缓存等。`snapshot/restore`（§3.5）同步保存 `meta`。

### 3.8 压缩失败降级链

L2"旧消息摘要"需调 LLM 生成摘要，可能失败（限流/超时/过滤）。失败时按降级链处理，**永不**向上抛错中断对话：

```
L2 摘要压缩触发
   │
   ├─ 1. 主 provider 生成摘要（≤200 token/条）
   │    ├─ 成功 → 替换原文，继续管道
   │    └─ 失败 ↓
   ├─ 2. 备用小模型或同 provider 重试 1 次（base_delay=1s）
   │    ├─ 成功 → 替换原文
   │    └─ 失败 ↓
   ├─ 3. 启发式兜底（不调 LLM）：
   │    取消息首 80 字 + 末 80 字，拼为 "[heuristic summary] ..."，
   │    标注 quality=heuristic
   │    ├─ 成功 → 替换原文
   │    └─ 失败 ↓
   └─ 4. 跳过 L2，直接进 L3 滚动窗口（丢弃而非摘要）
        └─ 记 audit.log + tracing::warn!
```

降级链与 §8.4 的"会话摘要失败降级链"同构，复用同一 `SummaryFallback` 工具。`quality` 字段（`llm`/`heuristic`/`dropped`）记入压缩日志（§3.5 `ContextSnapshot`），供后续诊断压缩质量。

### 3.9 预测性压缩（Predictive Compact）

§3.3 的反应式 compact 在 `token_count > budget * 0.85` 时触发，存在两个不足：(1) 触发点可能在 turn 中途，此时 LLM 已开始流式输出，打断代价高；(2) 阈值靠近上限，留给本轮 tool_result 的余量不足，易在 turn 末再次触发，加剧 §3.6 的 Thrash 风险。预测性 compact 在 turn 开始前根据历史增长估算"本轮是否会超"，提前压缩，给本轮留足空间。

**估算公式**：

```
predicted_tokens = current_tokens + avg_turn_growth
avg_turn_growth  = EMA(turn_token_delta_history, alpha=0.3)
baseline_growth  = config.context.predictive_baseline_growth_tokens  # 默认 15000
```

`avg_turn_growth` 是历史 turn token 增量的指数移动平均；冷启动期（历史样本 < 3）用 `baseline_growth` 兜底，避免空历史导致预测失真。EMA 比简单平均更能跟踪近期变化（如用户突然开始大量 read 大文件）。

**触发条件**：

```
if config.context.predictive_compact_enabled
   and predicted_tokens > budget * 0.85
   and current_tokens <= budget * 0.85:    # 避免与反应式 compact 重复触发
    trigger predictive_compact()
```

预测性 compact 与反应式 compact 互补而非替代：反应式是兜底防线（即使预测失误，超阈值仍触发），预测性是优化（在 turn 间隙提前压缩，避免 turn 中断）。两者复用 §3.3 的压缩管道，不引入新算法。

**配置**：

```toml
[context]
predictive_compact_enabled = false                  # 默认关，需用户显式开启
predictive_baseline_growth_tokens = 15000           # 冷启动 baseline
predictive_ema_alpha = 0.3                          # EMA 平滑系数
```

默认关闭是因为预测性 compact 在"短对话 + 小工具输出"场景下可能多余（提前压缩反而丢失上下文）。用户在长会话 + 大量文件读取场景下开启收益最大。

**可观测性**：预测性 compact 触发时打 OTel span event（`predictive_compact`），属性 `predicted_tokens`/`current_tokens`/`avg_turn_growth`/`baseline_used`（bool），便于诊断预测准确度。若连续 N 次预测性 compact 后仍触发反应式 compact，说明 `baseline_growth` 配置过小，Runtime 在 `audit.log` 写建议调大提示。

### 3.10 Post-compact 上下文恢复

§3.3 L2/L3 压缩会把"刚 read 过的文件"tool_result 一起摘要或丢弃，导致 compact 后模型丢失文件内容上下文，下一轮被迫重新 `fs.read`，浪费 token 且打断思路。Post-compact 上下文恢复机制在 compact 后把"最近读过的文件"重新注入，让模型无缝继续工作。

**跟踪机制**：`ContextManager` 在每个 `tool.call` span 末（`fs.read` 成功返回时）记录路径到 `recent_read_paths` 环形缓冲（容量 = `post_compact_max_files`，默认 5）。仅记录 `fs.read`，不记录 `fs.grep`/`fs.glob`（输出是搜索结果而非文件全文，重读意义不大）。

```rust
pub struct ContextManager {
    // ... 既有字段
    recent_read_paths: VecDeque<Utf8PathBuf>,  // 容量 post_compact_max_files
}

impl ContextManager {
    /// 在 fs.read 工具成功后由 Runtime 调用，记录路径。
    pub fn note_file_read(&mut self, path: Utf8PathBuf) {
        if self.recent_read_paths.iter().any(|p| p == &path) {
            return;  // 去重
        }
        if self.recent_read_paths.len() == self.config.post_compact_max_files {
            self.recent_read_paths.pop_front();
        }
        self.recent_read_paths.push_back(path);
    }
}
```

环形缓冲在 compact 时不被清空——它跟踪的是"物理文件路径"，与 messages 是否被压缩无关。`note_file_read` 由 Runtime 在 §2.3 `run_one` 成功返回后调用，与 OTel `tool.call` span 末对齐。

**重注入流程**：

```
compact 完成
   │
   ▼
对 recent_read_paths 中每个路径：
   ├─ 检查路径是否仍存在于 retained tail（§3.3 L3 保留的最近 W 条消息）
   │    └─ 是 → 跳过（避免重复注入）
   ├─ 检查文件是否仍存在（compact 期间用户可能删除）
   │    └─ 否 → 跳过并 warn
   ├─ 读取文件内容，按 post_compact_max_tokens_per_file 截断
   └─ 累计 token 不超过 post_compact_token_budget
   │
   ▼
拼装为 <post_compact_context> 块注入 system 段末尾
```

`<post_compact_context>` 边界声明这是"compact 前刚读过的文件内容，供参考"，区别于 §8.6 的 `<project_doc>`（项目约定）与 §8.7 的 `<auto_memory>`（学习记录）。包裹边界让 LLM 理解这是上下文恢复而非新指令。

**配置**：

```toml
[context]
post_compact_max_files = 5                # 跟踪最近 N 个 read 路径
post_compact_token_budget = 50000          # 重注入总 token 上限
post_compact_max_tokens_per_file = 5000    # 单文件截断阈值
```

token budget 与 §3.4 的预算分配独立——post-compact 重注入占用 headroom（5%）+ 必要时压缩 recent messages 窗口腾出空间。若预算不足以容纳全部 `recent_read_paths`，按 LRU（最近最少 read）顺序淘汰。

**与 §3.9 预测性 compact 的协作**：预测性 compact 在 turn 间隙触发，post-compact 恢复也在间隙完成，用户无感知。反应式 compact 在 turn 中途触发时，post-compact 恢复延迟到该 turn 结束（避免在流式输出中插入 system 消息打乱对话）。

---

## 4. 工具系统详细设计

### 4.1 Tool trait

```rust
#[trait_variant::make(Tool: Send)]
pub trait Tool {
    /// 唯一名（如 "fs.read"）
    fn name(&self) -> &str;

    /// 给 LLM 的 JSON Schema 描述
    fn schema(&self) -> &ToolSchema;

    /// 是否产生副作用（影响权限路径与并行/串行调度，见 §2.3）
    fn side_effect(&self) -> SideEffect;

    /// 是否只读（用于 Plan 模式硬门，见 §16.1）。
    /// 默认实现：`self.side_effect() == SideEffect::None`。
    /// MCP 工具根据 server schema 的 `readOnlyHint` 覆盖（见 §19.3、rules.md C-25）。
    fn is_read_only(&self) -> bool { self.side_effect() == SideEffect::None }

    /// 执行
    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext)
        -> Result<ToolResult, ToolError>;
}

pub enum SideEffect {
    None,        // 只读：read / grep / glob —— 可并行
    FileWrite,   // 写文件 —— 串行
    Command,     // 执行 shell —— 串行
    Network,     // 网络请求 —— 串行
}
```

`side_effect()` 不仅驱动权限策略（见 §9），还是 §2.3 并行/串行分桶的依据，因此工具实现必须如实标注：把写操作误标为 `None` 会绕过串行约束并产生竞态。`is_read_only()` 拆为独立方法（而非直接用 `side_effect() == None`）是因为 MCP 工具的只读性由 server schema 声明（`readOnlyHint`），可能与本地 `side_effect` 推断不一致——Plan 模式硬门（§16.1）用 `is_read_only()` 判断，给声明了 `readOnlyHint` 的 MCP 工具留出"Plan 模式可用"的通道。权威定义见 `api.md` §3.3。

### 4.2 ToolRegistry

```rust
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    /// 按组启用：core / fs / shell / web / git
    enabled_groups: HashSet<ToolGroup>,
}

impl ToolRegistry {
    pub fn register(&mut self, tool: Arc<dyn Tool>) { /* ... */ }
    pub fn dispatch(&self, call: &ToolCall) -> impl Future<Output = ToolResult> { /* ... */ }
    pub fn schemas(&self) -> Vec<&ToolSchema> { /* 仅返回 enabled 的 */ }
}
```

### 4.3 内置工具集

| 名称 | 组 | 副作用 | 说明 |
|------|----|----|------|
| `fs.read` | fs | None | 读取文件，支持行范围 |
| `fs.write` | fs | FileWrite | 写文件（整文件覆盖） |
| `fs.edit` | fs | FileWrite | 精确字符串替换编辑（唯一性校验） |
| `fs.multiedit` | fs | FileWrite | 同文件多次顺序替换（参考 CC MultiEdit，§4.6） |
| `fs.delete` | fs | FileWrite | 删除文件 |
| `fs.list` | fs | None | 列目录 |
| `fs.glob` | fs | None | glob 匹配文件 |
| `fs.grep` | fs | None | ripgrep 风格内容搜索 |
| `shell.run` | shell | Command | 执行命令，支持超时 |
| `shell.background` | shell | Command | 启动后台命令，返回 shell_id（§4.7） |
| `shell.output` | shell | None | 读取后台命令输出（§4.7） |
| `shell.kill` | shell | Command | 终止后台命令（§4.7） |
| `web.fetch` | web | Network | 抓取 URL → markdown |
| `web.search` | web | Network | 网页搜索（后续） |
| `git.diff` | git | None | 查看 diff |
| `git.apply` | git | FileWrite | 应用 patch |
| `task.spawn` | core | None | 启动子 Agent |

### 4.4 工具上下文与隔离

`ToolContext` 携带执行环境约束：

```rust
pub struct ToolContext {
    pub workdir: Utf8PathBuf,
    pub session_id: SessionId,
    pub canceller: CancellationToken,
    pub env: HashMap<String, String>,
    pub timeout: Duration,
    pub max_output_bytes: usize,  // 截断超长输出
}
```

工具实现必须：
- 所有路径经 `sandbox_path(workdir, input)` 规范化并校验在工作目录内（除非显式 allowlist）；
- 监听 `canceller`，及时中止；
- 输出超过 `max_output_bytes` 截断并标注。

### 4.5 工具结果格式

```rust
pub struct ToolResult {
    pub content: ToolContent,   // Text | Image | Json
    pub is_error: bool,
    pub metadata: Meta,         // 耗时、字节数、截断标记
}
```

错误结果以 `is_error = true` 回灌给 LLM，让模型自我修正。

### 4.6 MultiEdit 工具（同文件多次顺序编辑，参考 Claude Code）

`fs.edit` 每次只能替换一处，对"同一文件改 N 处"需 N 次工具调用，每次都重新读文件校验唯一性，开销大且易因中间状态导致后续 `old_string` 不唯一。参考 CC 的 `MultiEdit`，提供原子化的多编辑工具：

```rust
pub struct MultiEditInput {
    pub path: Utf8PathBuf,
    pub edits: Vec<SingleEdit>,   // 按序执行
}

pub struct SingleEdit {
    pub old_string: String,       // 必须在"当前累积状态"中唯一
    pub new_string: String,
    pub replace_all: Option<bool>, // None/Some(false)=唯一匹配；Some(true)=全替换
}
```

**执行语义**：

1. 一次性读取文件内容到内存；
2. 按 `edits` 数组顺序逐个应用：每个 `old_string` 在**当前累积修改后**的内容中匹配（非原始文件），保证前一个 edit 的 `new_string` 可被后续 edit 引用；
3. 任一 edit 的 `old_string` 不唯一（除非 `replace_all=true`）或不匹配 → 整个 MultiEdit **原子失败**，文件不修改，返回错误指示第几个 edit 失败；
4. 全部成功后一次性写盘，调用 `Journal::record`（§17）记录单个 `ChangeEntry`（before = 原始内容，after = 最终内容）。

**与 `fs.edit` 的关系**：`fs.multiedit` 是 `fs.edit` 的批量化版本，单次编辑场景仍用 `fs.edit`（更简单）。LLM 在系统提示中被告知"同文件多处修改优先用 `fs.multiedit`，原子性更好"。

**副作用标注**：`SideEffect::FileWrite`，走串行桶（§2.3），权限同 `fs.edit`（首次 Ask / AllowAlways）。

### 4.7 后台命令管理（shell.background / shell.output / shell.kill，参考 Claude Code）

`shell.run` 是阻塞的——命令完成才返回结果。但有些场景需要后台运行长时命令（如 `cargo watch`、`npm run dev`、`tail -f`），同时让 Agent 继续其它工作。参考 CC 的 `BashOutput`/`KillBash`，提供三件套：

```rust
// 启动后台命令
pub struct ShellBackgroundInput {
    pub cmd: String,
    pub args: Vec<String>,
    pub cwd: Option<Utf8PathBuf>,
}
pub struct ShellBackgroundOutput {
    pub shell_id: String,         // 如 "sh_01H..."
    pub pid: u32,
}

// 读取后台命令的累积输出（非阻塞快照）
pub struct ShellOutputInput {
    pub shell_id: String,
    pub since: Option<usize>,     // 字节偏移，None=从头
    pub max_bytes: Option<usize>, // 默认 65536
}
pub struct ShellOutputResult {
    pub stdout: String,           // since 偏移后的新增输出
    pub stderr: String,
    pub running: bool,            // 是否仍在运行
    pub exit_code: Option<i32>,   // None=仍在运行
    pub total_stdout_bytes: usize,
}

// 终止后台命令
pub struct ShellKillInput {
    pub shell_id: String,
}
```

**执行模型**：

- `shell.background` 用 `tokio::process::Command::spawn`（非 `wait`）启动进程，返回 `shell_id`；进程的 stdout/stderr 通过 pipe 持续收集到 `Vec<u8>` 缓冲（带上限，默认 1 MiB，超限截断并标注）；
- `shell.output` 读取缓冲快照（非阻塞），Agent 可轮询检查长时命令进度；
- `shell.kill` 发送 SIGTERM（Unix）/ TerminateProcess（Windows），等待 5s 后 SIGKILL 强制终止；
- 会话结束时 Runtime 自动 kill 所有未结束的后台命令（防孤儿进程）。

**权限与安全**：

- `shell.background` 副作用 `SideEffect::Command`，走串行桶与完整权限流程（同 `shell.run`）；
- `shell.output` 副作用 `None`（只读快照），可并行；
- `shell.kill` 副作用 `Command`，串行；
- 后台命令同样受 `SandboxDriver` 约束（`security.md` §8）、`shell_environment_policy`（`security.md` §10）、危险命令黑名单（`security.md` §4.2）；
- 后台命令的 `shell_id` 在 session 内有效，跨 session 不可访问（防混淆）。

**与 `shell.run` 的选择引导**（系统提示）：短时命令（<30s）用 `shell.run` 直接拿结果；长时命令或需中途检查输出的用 `shell.background` + `shell.output` 轮询。

---

## 5. LLM Provider 抽象

### 5.1 Provider trait

```rust
pub trait LlmProvider: Send + Sync {
    fn id(&self) -> &str;
    fn capabilities(&self) -> Capabilities;  // vision / tool_call / streaming
    fn tokenizer(&self) -> Arc<dyn Tokenizer>;

    async fn chat_stream(
        &self,
        req: ChatRequest,
    ) -> Result<BoxStream<'static, Result<Delta>>>;

    async fn count_tokens(&self, messages: &[Message]) -> usize;
}

pub enum Delta {
    Text(String),
    ToolCall(ToolCallDelta),
    Usage(Usage),
}

pub struct Capabilities {
    pub supports_tool_call: bool,
    pub supports_vision: bool,
    pub supports_streaming: bool,
    pub context_window: usize,
    pub max_output: usize,
}
```

### 5.2 多 Provider 适配

| Provider | 协议 | 工具调用格式 | 备注 |
|----------|------|-------------|------|
| OpenAI 兼容 | `/v1/chat/completions` SSE | `tool_calls` 数组 | 也覆盖 DeepSeek、Moonshot、本地 vLLM |
| Anthropic | `/v1/messages` SSE | `content_block_start/stop` | 专有事件流 |
| Ollama | `/api/chat` NDJSON | `tools` | 本地模型 |

每个 Provider 内部三步：
1. `ChatRequest → 上游 JSON`（序列化适配）；
2. `上游 SSE/NDJSON → Delta`（流式解析）；
3. `错误 → LlmError`（含限流/超时分类）。

### 5.3 重试与限流

```rust
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    pub retryable: fn(&LlmError) -> bool,
}
```

可重试：网络错误、5xx、429（带 `Retry-After`）。不可重试：4xx（除 429）、内容审查拒绝。

### 5.4 模型路由（Router 骨架，T-M6-5）

支持按任务路由到不同模型（如规划用大模型、补全用小模型），通过 `Router` trait：

```rust
pub trait Router: Send + Sync {
    /// 选择 provider。
    ///
    /// `task_kind` 为任务类型 hint（如 `"plan"`/`"code"`/`"summary"`），
    /// `None` 表示无 hint，路由器应回退到默认 provider。
    fn pick(&self, task_kind: Option<&str>) -> Arc<dyn LlmProvider>;
}
```

M6 交付骨架 + `StaticRouter`（恒返回同一 provider，等价于"无路由"）：
```rust
pub struct StaticRouter {
    provider: Arc<dyn LlmProvider>,
}
```

`StaticRouter` 保留作为配置未指定路由策略时的默认值。M7+ 可实现 `TaskBasedRouter`
（按 `Task::kind`）、`CostAwareRouter`（按 token 预算）等。

**防死循环**：`Router::pick` 是无状态纯选择，不涉及重试逻辑；重试由
`RetryProvider` 装饰器负责（bounded `max_retries`，见 `providers::common::retry`）。

---

## 6. 流式处理详细设计

### 6.1 流式解析层

每个 Provider 实现一个 `Stream<Item = Result<RawEvent>>`，再由统一的 `DeltaAggregator` 转换：

```
HTTP chunk ── line splitter ── SSE event ── Provider parser ── RawEvent
                                                                 │
                                                                 ▼
                                                         DeltaAggregator
                                                                 │
                                                       Delta (Text/ToolCall)
```

### 6.2 工具调用增量解析

OpenAI 与 Anthropic 都支持工具调用的分片到达。`DeltaAccumulator` 维护：

```rust
struct DeltaAccumulator {
    text: String,
    tool_calls: HashMap<u32, ToolCallBuilder>,  // index → builder
    usage: Option<Usage>,
}

struct ToolCallBuilder {
    id: String,
    name: String,
    args_json: String,   // 分片拼接
}
```

流结束时：
- 对每个 builder 的 `args_json` 做宽松 JSON 解析（容错尾随逗号、未闭合）；
- 解析失败则把原始字符串作为单参数 `{ "_raw": "..." }` 并标记 warning。

### 6.3 背压与取消

- Frontend 渲染慢时，`EventBus` 使用有界 channel（容量 256），满时对 token 事件做合并（合并连续空格），但不丢失工具事件。
- `CancellationToken` 贯穿 LLM stream 与 tool execution，Ctrl-C 触发后立即停止读取并丢弃未完成 delta。

---

## 7. 子 Agent（Subagent）设计

### 7.1 用途

- 隔离上下文：把"搜索整个代码库定位 X"这种高消耗任务交给子 Agent，避免污染主上下文。
- 并行探索：同时派多个子 Agent 调研不同方向。
- 专业化：子 Agent 可加载不同系统提示与工具子集（参考 Claude Code 的 `Explore`/`Plan`/`general-purpose` 类型化分工）。

### 7.2 类型化子 Agent（参考 CC Task 工具）

把原先的自由 `role: String` 改为类型化枚举，每类预设模型路由、工具子集、记忆加载策略。这样既能复用 CC 验证过的分工模式，也便于在系统提示词中给模型清晰的"何时派哪类子 Agent"指引。

```rust
pub enum SubagentType {
    /// 快速代码库探查：固定小模型（Haiku 级），只读工具子集，跳过 AGENTS.md 与长期记忆，降低成本。
    Explore,
    /// 计划模式下收集上下文：只读，仅 Plan 模式可用（见 §16）。
    Plan,
    /// 通用多步任务：继承父会话模型与全工具，可写可改。
    GeneralPurpose,
    /// 自定义：从 .minicoding/agents/*.md 加载（YAML frontmatter + Markdown body）。
    Custom(String),
}

pub struct SubagentSpec {
    pub ty: SubagentType,
    pub system_prompt: String,           // 类型预设可被覆盖
    pub allowed_tools: ToolGroup,
    pub model: Option<ModelId>,          // None = 继承父会话；Explore 强制小模型
    pub budget_tokens: usize,
    pub max_iters: u32,
    pub thoroughness: Thoroughness,      // 仅 Explore 用
    pub skip_memory: bool,               // Explore/Plan 默认 true
    pub can_spawn_subagent: bool,        // 默认 false，防无限嵌套
}

pub enum Thoroughness { Quick, Medium, VeryThorough }

pub struct SubagentResult {
    pub summary: String,         // 给主 Agent 的结论
    pub artifacts: Vec<Artifact>, // 文件改动等
    pub token_used: usize,
}
```

各类型的默认配置（参考 CC 内置 subagent 行为）：

| 类型 | 模型 | 工具集 | skip_memory | can_spawn | 用途 |
|------|------|--------|:---:|:---:|------|
| `Explore` | 小模型（强制） | 只读（`fs.read/grep/glob/list`、`git.diff`、`web.fetch`） | 是 | 否 | 廉价快速定位文件/行 |
| `Plan` | 继承父 | 只读（同 Explore） | 是 | 否 | Plan 模式下收集上下文 |
| `GeneralPurpose` | 继承父 | 全工具 | 否 | 否 | 复杂多步任务 |
| `Custom(name)` | frontmatter 指定 | frontmatter 指定 | 可配 | 可配 | 用户扩展 |

子 Agent 拥有独立的 `ContextManager` 与 `messages`，但共享 `ToolRegistry`、`Storage`、`PermissionPolicy`、`SandboxDriver`。父 Agent 只接收 `summary`，不接收子 Agent 的中间消息。

### 7.3 派发与回收

```rust
// 通过 task.spawn 工具派发（持有 Arc<dyn SubagentRunner> 反向调用 Runtime）
let result = runtime.subagent_runner().spawn(spec, input).await?;
ctx.append(Message::tool_result(call_id, result.summary)).await;
```

派发前 Runtime 强制校验：

- **不嵌套约束**：`spec.can_spawn_subagent == false` 时，从 `allowed_tools` 中移除 `task.spawn`，杜绝子 Agent 再生子 Agent 的无限递归（参考 CC 把 `Task` 工具排除出只读 Agent 工具集的做法）。
- **Plan 模式守卫**：`SubagentType::Plan` 仅在 `PermissionMode::Plan` 下可派发，其它模式下退化为 `Explore`（不报错，避免模型因模式状态失败重试）。守卫由 `task.spawn` 工具持有 `Arc<dyn PlanModeController>` 实现：调用 `snapshot()` 读当前模式，非 Plan 时改写 `spec.ty = Explore`。
- 子 Agent 内部仍跑完整 Agent 循环，但 `max_iters` 更小、超时更短、OTel span 通过 Context 传播挂在父会话 trace 下（见 §15）。

### 7.4 并行 map-reduce（参考 Codex `spawn_agents_on_csv`）

对于"对一批条目做同一种调研"的场景（如审查 N 个 PR、为 M 个模块写测试大纲），提供批量派发原语，避免主 Agent 串行调用 N 次 `task.spawn`：

```rust
pub struct CsvBatchSpec {
    pub csv_path: Utf8PathBuf,
    pub instruction: String,        // 支持 {column} 模板占位
    pub id_column: String,
    pub max_concurrency: usize,     // 默认 6（参考 Codex agents.max_threads）
    pub max_runtime: Duration,
    pub output_schema: serde_json::Value,
    pub subagent_ty: SubagentType,  // 通常 Explore/GeneralPurpose
}
// 结果回写 CSV，并附 job_id 供审计；超时/失败的条目标记 error
```

并发度受 `max_concurrency` 限制，单条超时不影响其它；job 元数据写入 audit.log 便于追溯。该原语适合阶段 5+ 落地，MVP 不交付。

### 7.5 worktree 隔离（参考 Claude Code `isolation: worktree`）

当多个子 Agent 并行修改文件时，共享同一工作目录会导致冲突（A 改了 foo.rs，B 也想改 foo.rs）。参考 CC 的 `isolation: worktree`，为 `GeneralPurpose` 子 Agent 提供 git worktree 隔离选项：

```rust
pub enum Isolation {
    /// 共享父会话工作目录（默认）。多个 Agent 串行写同一目录，靠 §2.3 串行桶防竞态。
    Shared,
    /// 在独立 git worktree 中运行。子 Agent 拥有独立工作目录，文件改动不互相干扰。
    /// 完成后通过 `git diff` 把改动合并回主 worktree（或丢弃）。
    Worktree(WorktreeSpec),
}

pub struct WorktreeSpec {
    pub branch_prefix: String,      // 如 "subagent/"，生成分支名 subagent/{task_id}
    pub auto_cleanup: bool,         // true: 子 Agent 完成后删除 worktree + 分支
    pub merge_back: MergeStrategy,  // None / CherryPick / MergeCommit
}

pub enum MergeStrategy {
    /// 不自动合并，子 Agent 结果含 worktree 路径，由父 Agent 决定是否合并
    None,
    /// cherry-pick 子 Agent 的 commit 到主分支
    CherryPick,
    /// 创建 merge commit
    MergeCommit,
}
```

`SubagentSpec` 新增 `isolation: Isolation` 字段，默认 `Shared`。设为 `Worktree` 时：

1. 派发前 Runtime 执行 `git worktree add -b subagent/{task_id} .minicoding/worktrees/{task_id}`；
2. 子 Agent 的 `ToolContext.workdir` 指向 worktree 路径，所有文件操作隔离在该 worktree 内；
3. 子 Agent 完成后按 `merge_back` 策略处理：`None` 把 worktree 路径返回给父 Agent（父 Agent 用 `git diff`/`git apply` 合并）；`CherryPick`/`MergeCommit` 自动合并到主分支；
4. `auto_cleanup = true` 时合并后删除 worktree 与分支。

**约束**：
- worktree 隔离仅对 `GeneralPurpose` 子 Agent 有效（`Explore`/`Plan` 只读无需隔离）；
- 需仓库是 git 仓库（非 git 仓库降级为 `Shared` 并 warn）；
- worktree 路径在 `.minicoding/worktrees/`（已加入 `.gitignore` 默认模板）。

**实现状态（A-15）**：`Isolation`/`WorktreeSpec`/`MergeStrategy` 定义在 `model/subagent.rs`，`SubagentSpec.isolation` 默认 `Shared`。`WorktreeSubagentRunner`（`agent/worktree.rs`）是装饰器，包裹内部 `Arc<dyn SubagentRunner>`：`Worktree` 模式下 `git worktree add` 创建隔离目录 → 委托内部 runner → 按 `merge_back` 合并 → `auto_cleanup` 清理。非 git 仓库降级为 `Shared`。

---

## 8. 记忆（Memory）设计

### 8.1 三层记忆

| 层 | 存储 | 生命周期 | 用途 |
|----|------|----------|------|
| 工作记忆 | `Session.messages` | 单会话 | 当前对话上下文 |
| 会话记忆 | `~/.minicoding/memory/sessions/{id}.md` | 跨会话 | 最近 N 次会话摘要 |
| 长期记忆 | `~/.minicoding/memory/long_term.md` + `long_term.index.json` | 永久 | 用户偏好、项目约定、决策 |

### 8.2 长期记忆格式规范（双文件：人机共读 + 程序化索引）

纯 Markdown 对程序化查询（按主题/标签/key 取片段）效率低，每次需全文解析。因此采用"Markdown 正文 + 旁路 JSON 索引"双文件方案：

**`long_term.md`**（人机共读，按节组织，每节带 TOML frontmatter 风格的元信息头）：

```markdown
# Long-term Memory

## pref.lang
source: user | updated: 2026-07-24 | confidence: 0.9
通信语言：中文

## conv.tab_indent
source: user | updated: 2026-07-24 | confidence: 1.0
本项目使用 tab 缩进

## decision.runtime
source: agent | updated: 2026-07-24 | confidence: 0.8
选用 tokio 而非 async-std（生态广度）
```

**`long_term.index.json`**（程序化快速查询，与正文同源同步）：

```json
{
  "v": 1,
  "entries": [
    {"key": "pref.lang", "topic": "preference", "tags": ["lang"], "line": 3, "tokens": 8, "updated": "2026-07-24"},
    {"key": "conv.tab_indent", "topic": "convention", "tags": ["format"], "line": 7, "tokens": 10, "updated": "2026-07-24"}
  ],
  "total_tokens": 128
}
```

写入时双文件原子更新（写临时文件 → rename），索引由 `MemoryStore` 维护，外部只读 Markdown。查询路径：
- 按 key/topic/tags → 查 index → 按行号定位 Markdown 片段；
- 全量注入 → 直接读 Markdown（命中缓存时零解析，见 §8.4）。

### 8.3 注入策略与 token 成本控制

每次 `build_chat_request` 注入长期记忆，但必须避免"无变更也重读重算 token"的开销：

1. **mtime 缓存**：`MemoryStore` 缓存 `(mtime, parsed_entries, token_count, rendered_block)`。`build_chat_request` 时先 `stat` 文件，mtime 未变则直接复用缓存的 `rendered_block` 与 token 计数，零 IO 解析、零重复分词。
2. **预算上限**：长期记忆块占上下文预算 ≤ 10%（见 §3.4）。超限时按 `confidence desc, updated desc` 截断保留高分条目，并在块尾标注 `[truncated: N entries omitted]`。
3. **惰性注入**：首条用户消息前才注入一次，后续轮次复用同一 `system` 消息（除非用户显式 `@memory refresh`）。
4. **会话记忆**：仅新会话首轨注入最近 N 条摘要（每条 ≤ 200 token），老会话续轨不重复注入。

### 8.4 写入策略（显式 + 隐式 + 失败处理）

| 触发 | 来源 | 时机 | 失败处理 |
|------|------|------|---------|
| 显式 | 用户说"记住 X" → `memory.write` 工具 | 工具调用即时 | 工具错误回灌 LLM 重试；用户可见 |
| 隐式-摘要 | 会话结束 | `session_end` | 见下方"摘要失败降级链" |
| 隐式-偏好 | 启发式检测偏好陈述（可选，默认关） | 轮结束 | 检测失败静默跳过，不影响主流程 |

**会话摘要失败降级链**（关键：摘要 LLM 调用失败不得阻塞会话结束）：

```
1. 用主 provider 生成 ≤200 token 摘要
   ├─ 成功 → 写入 sessions/{id}.md，更新 index
   └─ 失败（超时/限流/过滤）↓
2. 用备用小模型（或同 provider 重试 1 次，base_delay=1s）
   ├─ 成功 → 写入
   └─ 失败 ↓
3. 启发式兜底摘要（不调 LLM）：
   取首条用户消息前 80 字 + 末条 assistant 消息前 80 字，
   拼为 "【自动兜底摘要】..."，标记 quality=heuristic
   ├─ 写入成功 → 结束
   └─ 写盘失败 → 仅写 audit.log 告警，会话仍正常结束
```

无论哪一级，失败都写 `audit.log` 与 `tracing::warn!`，**永不**向上抛错中断会话结束流程。`quality` 字段（`llm`/`heuristic`）记入摘要文件 frontmatter，供后续决定是否需要重生成。

### 8.5 记忆与上下文压缩的协作

长期记忆注入在 `system` 消息内，权重最高（§3.2 `base(system)=1.0`），不参与压缩。会话记忆作为首条 system 附注，权重次高。这样保证跨会话的关键约定不会被中段压缩丢弃。

### 8.6 项目记忆约定（AGENTS.md 分层加载，参考 Codex/CC）

`long_term.md` 是用户/Agent 维护的"动态记忆"，与项目代码库无关；而**项目记忆** `AGENTS.md` 是用户手写、随仓库版本化的**静态指令层**（约定、规范、禁区、架构说明），二者互补：

- `long_term.md`：跨项目的用户偏好与决策（动态，Agent 可写）；
- `AGENTS.md`：当前仓库的工作约定（静态，Agent **不可**自主编辑，参考 Codex 的约束）。

**分层加载算法**（参考 Codex `docs/agents_md.md` 与 CC `CLAUDE.md` 上溯机制）：

```
1. 全局层：$MINICODING_HOME/AGENTS.md
   - 同目录有 AGENTS.override.md 则优先取 override，否则取 AGENTS.md
   - 仅取首个非空文件
2. 项目层 walk：从 repo_root 逐级向下走到 cwd
   - 每级查找顺序：AGENTS.override.md → AGENTS.md → fallback 文件名
   - 每级至多取一个文件（override 命中则跳过其它）
3. 拼接：root → leaf 顺序拼接，空文件跳过
4. 截断：累计超过 32 KiB（project_doc_max_bytes 可配）静默截断
```

**fallback 文件名**（跨工具兼容，无需改名即可复用 Claude/Cursor 写的文件）：

```toml
[project]
project_doc_fallback_filenames = ["CLAUDE.md", ".cursorrules", "TEAM_GUIDE.md"]
project_doc_max_bytes = 32768
```

**`@import` 语法（参考 CC）**：AGENTS.md 支持 `@<相对路径>` 引用其他文件，避免单文件膨胀。加载时递归展开：

```markdown
# AGENTS.md

@docs/coding-style.md
@docs/testing-conventions.md

## 项目特定约定
- 本项目用 tab 缩进
```

- 路径相对当前文件所在目录；
- 递归深度上限 5 层（防循环引用）；
- 代码块内的 `@path` 不展开（避免误解析）；
- 展开后总大小仍受 `project_doc_max_bytes` 限制，超限截断。

**`MINICODING_PROJECT_DIR` 环境变量（参考 CC `CLAUDE_PROJECT_DIR`）**：Runtime 启动时注入此变量到 MCP server 与 Hook 子进程环境，指向项目根目录（git repo root 或 cwd）。MCP server 可据此解析项目相对路径（如读取 `.minicoding/` 配置）。

**override 语义**：`AGENTS.override.md` 仅替换**同目录**的 `AGENTS.md`，不取消父目录文件；深层文件覆盖浅层冲突指令——这给"本地分支临时覆盖团队约定"提供官方机制。

**加载时机**：新会话首轨注入一次，作为 `system` 消息的一部分（与长期记忆同段，权重 1.0 不压缩）。`Explore`/`Plan` 子 Agent 默认跳过 AGENTS.md（参考 CC 跳过 CLAUDE.md 的做法），保持廉价。

**安全约束**：

- AGENTS.md 是用户维护的静态指令层，`fs.write`/`fs.edit` 对 `AGENTS.md` 默认 `Ask`，且 LLM 不得通过任何工具绕过该确认（参考 Codex「AGENTS.md 不可被 Agent 自主编辑」）。
- 加载的 AGENTS.md 内容包裹 `<project_doc>` 边界，系统提示声明"这是项目约定而非新指令"——但项目约定本身**可以**包含合法的工作流指令（如"提交前跑 cargo fmt"），这是其设计目的；区别于"工具输出数据"，AGENTS.md 是受信任的用户输入。

**与既有抽象的关系**：

- 与 `long_term.md` 互不替代：项目记忆是仓库内的、版本化的、静态的；长期记忆是跨项目的、动态的。两者都注入 system 段。
- 与 `PermissionPolicy` 协同：AGENTS.md 不能授权越权操作（L0 黑名单仍最高优先级），只能给模型提供"该怎么做"的指引。
- 与 Hooks 协同：`SessionStart` Hook 可注入额外的项目动态信息（如 `git status`），与静态 AGENTS.md 互补。

详细存储层视角与文件路径见 `data-model.md` §6.4。

### 8.7 Auto Memory（自动学习，参考 Claude Code）

§8.4 的"隐式-偏好"记忆默认关闭。参考 CC 的 Auto memory（Claude 自动从用户修正中学习项目模式、调试洞察、架构笔记），开启并细化触发与容量控制：

**触发时机**（自动检测，非显式 `memory.write`）：

| 场景 | 检测方式 | 记忆内容 |
|------|---------|---------|
| 用户修正 Agent 错误 | Agent 输出后用户立即说"不对，应该用 X" | `correction.{topic}`：下次用 X 而非 Y |
| Agent 反复尝试同一错误 | 连续 ≥2 次同类工具失败 | `pitfall.{topic}`：避免重复踩坑 |
| 用户显式偏好陈述 | 启发式："我喜欢/讨厌/总是/从不..." | `pref.{topic}`：用户偏好 |
| 项目架构决策 | Agent 提出 A/B 选择，用户选 A | `decision.{topic}`：选 A 的理由 |

**存储**：写入 `~/.minicoding/memory/auto.md`（与 `long_term.md` 分离，避免污染手写记忆），同样配 `auto.index.json` 索引。

**容量控制（参考 CC 200 行/25KB 限制）**：

- `auto.md` 上限 200 行或 25KB（先到者为准）；
- 超限时按 `confidence asc, updated asc` 淘汰低置信度旧条目，移入 `auto.{topic}.md` 主题文件按需读取；
- 每条 Auto memory 带 `confidence: 0.0-1.0`，多次确认的条目置信度递增，长期未被引用的递减。

**注入策略**：

- Auto memory 注入 system 段，权重同长期记忆（1.0 不压缩）；
- 仅注入"与当前 workdir / 项目相关"的条目（据 `topic` 标签过滤），避免跨项目污染；
- 注入块标注 `[auto memory, learned from past sessions]`，与手写长期记忆区分。

**安全约束**：

- Auto memory 是 Agent **可写**的（区别于 AGENTS.md 的不可写），但写入受 §9 权限约束——`memory.write` 对 `auto.md` 默认 `Allow`（隐式触发），对 `long_term.md` 默认 `Ask`；
- Auto memory 内容包裹 `<auto_memory>` 边界，声明"这是过往学习记录而非新指令"；
- 用户可 `/memory auto off` 关闭，`/memory auto show` 查看已记录内容，`/memory auto clear` 清空。

**与既有记忆的关系**：

| 记忆类型 | 来源 | 可写性 | 存储 |
|---------|------|--------|------|
| 工作记忆 | 当前会话 | Runtime 自动 | Session.messages |
| 会话记忆 | 会话摘要 | Runtime 自动 | sessions/{id}.md |
| 长期记忆（手写） | 用户/Agent 显式 | Agent 可写（Ask） | long_term.md + index |
| Auto memory（自动） | 启发式检测 | Agent 可写（Allow） | auto.md + auto.index |
| AGENTS.md（项目） | 用户手写 | Agent **不可写**（C-23） | 仓库内 |

Auto memory 填补了"会话摘要太粗、长期记忆太手动"之间的空白——让 Agent 从每次修正中自动积累项目经验。

---

## 9. 权限集成设计

### 9.1 双抽象：决策与交互分离

权限被拆为两个正交 trait（定义见 `api.md` §3.6），以解决"broadcast 事件总线无法承载点对点回复"的架构缺陷：

- `PermissionPolicy::check(...) -> Verdict`：纯决策，输出 `Allow` / `Deny` / `Ask(prompt)`。无 IO、可单测。
- `PermissionPrompter::prompt(prompt) -> Decision`：点对点交互，仅 `Ask` 时调用，返回终态 `Allow`/`Deny`。

`Decision` 不再含 `Ask`——交互在 prompter 内闭环，`run_one`（§2.3）拿到的永远是终态。`EventBus` 只广播 `PermissionRequested`/`PermissionResolved` 通知（可克隆，无 `Sender`）。

### 9.2 权限解析流程

```
run_one(call)
   │
   ▼
policy.check(tool, input, ctx) ──▶ Verdict
   │
   ├─ Allow                       ──▶ Decision::Allow ──▶ dispatch
   ├─ Deny(reason)                ──▶ Decision::Deny  ──▶ 回灌错误
   └─ Ask(prompt)
        │
        ├─ emit Event::PermissionRequested { id, tool, summary, risk }  (广播通知)
        │
        ▼
   prompter.prompt(prompt) ──▶ Decision        (点对点，阻塞当前工具调用)
        │
        ├─ InteractivePrompter    (CLI TTY): 读 stdin，超时→Deny
        ├─ NonInteractivePrompter (非 TTY/CI): 按 non_tty_strategy 配置
        │     · "deny"  (默认) → Deny("non-interactive: denied by default")
        │     · "allow"        → Allow（高风险工具仍 Deny）
        │     · "fail"         → 返回 Err，中止本轮
        ├─ TuiPrompter           (TUI): 渲染弹窗
        └─ CallbackPrompter      (SDK): 用户闭包
        │
        ▼
   emit Event::PermissionResolved { id, decision }   (广播通知)
        │
        ▼
   audit.log 落盘 (Allow/Deny 均记录)  ──▶ 进入 dispatch 或回灌
```

**非交互环境（非 TTY / CI / 管道）** 显式由 `NonInteractivePrompter` 处理，策略可配，默认 `deny`——避免在 CI 中静默执行副作用。`InteractivePrompter` 在启动时检测 `stdin.is_terminal()`，非 TTY 自动切换为 `NonInteractivePrompter` 并打 `warn` 日志。

### 9.3 默认策略规则

```
read-only tools (fs.read/grep/glob/list, git.diff)  → Allow
fs.write inside workdir                              → Ask（首次）/ Allow（已记住）
fs.write outside workdir                             → Deny
fs.write 敏感路径 (.git/.env/*.secret)               → Deny（不可被 allow 覆盖）
shell.run                                            → Ask（可按命令前缀 allowlist）
shell.run 危险前缀 (rm -rf /, sudo, dd ...)          → Deny（内置黑名单，不可覆盖）
web.fetch                                            → Ask（可按域名 allowlist）
web.fetch 内网/元数据接口                             → Deny（SSRF 防护，不可覆盖）
```

### 9.4 决策缓存

`AllowAlways` / `DenyAlways` 写入 `~/.minicoding/policy.toml`（结构见 `data-model.md` §5），作为 specificity=2 的 L1 条目（见 §9.5）持久化，下次同规则直接命中 `Allow`/`Deny`，跳过 prompter。内置安全黑名单（L0）优先级最高，用户配置无法覆盖。详见 `security.md` §2、§4、§5。

### 9.5 权限解析：两层模型（L0 硬黑名单 + L1 用户策略）

> **设计取舍（简化）**：早先版本采用 5 级优先级链（黑名单 → granular → ApprovalMode → policy.toml → per-tool 矩阵），每级独立匹配再叠加。参考 Codex 的两层模型（builtin + user-configurable），本项目简化为**两层 + specificity 单一竞争**：所有用户可配置规则进入同一命名空间，按 specificity 排序竞争，deny 在同 specificity 下胜出。这避免了"5 级级联 + specificity 计算 + 模式平移"叠加产生的复杂度与潜在安全 bug。

```
┌─────────────────────────────────────────────────────────────┐
│  L0  内置硬黑名单 (policy::builtin)                          │
│      危险命令前缀 / SSRF 内网 / 敏感路径 / AGENTS.md 写       │
│      → Deny，不可被任何配置覆盖（rules.md C-02）              │
└─────────────────────────────────────────────────────────────┘
                          │ 未命中 L0
                          ▼
┌─────────────────────────────────────────────────────────────┐
│  L1  用户策略（统一规则集，按 specificity 降序匹配）           │
│                                                              │
│   specificity 5  granular rule 精确路径  fs.write:.env*      │
│   specificity 4  granular rule 通配路径  fs.write:src/**     │
│   specificity 3  granular rule 工具类别  shell.run:cargo *   │
│   specificity 3  granular rule MCP server  mcp:github        │
│   specificity 2  policy.toml 显式 allow/deny（per-tool）     │
│   specificity 1  ApprovalMode × SideEffect 全局平移          │
│   specificity 0  §9.3 per-tool 默认矩阵（兜底基线）          │
│                                                              │
│   匹配规则：最高 specificity 命中生效；                       │
│            同 specificity 多条命中 → deny 胜出（safe default）│
│            无任何命中 → 视作 Allow（只读工具）或 Ask（副作用）│
└─────────────────────────────────────────────────────────────┘
                          │
                          ▼
                     最终 Verdict
```

**为何合并而非级联**：5 级级联要求每级独立计算并按固定优先级叠加，specificity 只在 granular 内部用，跨级无法比较——这导致"全局 `--allow fs.write:src/**` 与 granular `fs.write:.env*` 谁优先"的歧义。合并后所有规则在同一尺度下比较 specificity，规则间的优先关系显式且可预测。Codex 的两层模型已验证此思路在大型策略文件下仍可维护。

**ApprovalMode 的语义**：不再作为独立优先级层，而是 specificity=1 的"全局平移规则"。如 `Untrusted` 模式等价于一条 specificity=1 的规则"所有 `side_effect != None` 工具 → Ask"，会被任何更高 specificity 的用户规则覆盖。`Never` 模式等价于"所有 `Ask` → Allow"的全局规则，同样可被高 specificity 规则覆盖（如 `Never` 模式下仍可 `deny shell.run:rm *`）。

**预设（preset）**：`read-only`/`auto`/`external-sandbox`/`full-access` 是"一键写入一批 L1 规则"的语法糖，不是独立层级。如 `--preset read-only` 等价于写入一条 specificity=1 的规则"所有 `side_effect != None` → Deny" + 选择 `ReadOnly` SandboxPolicy。展开后与其他 L1 规则平等竞争。

**OS 沙箱**：`SandboxPolicy`（`ReadOnly`/`WorkspaceWrite`/`ExternalSandbox`/`DangerFullAccess`）是**独立的第二道防线**（`security.md` §8），不参与 L1 的 Verdict 计算。即使 L1 给出 `Allow`，沙箱仍可在内核级拦截越界写（C-22/C-30）。这是 defense in depth，不是第三层权限。

**Plan 模式**（§16）作为 `PermissionMode::Plan`，在 L0 与 L1 之间插入一条 specificity=∞ 的"所有 `side_effect != None` → Deny"硬规则（不可被 L1 覆盖，但受 L0 黑名单约束一致）——本质是 L0 的扩展，而非新层级。

**沙箱初始化失败询问回退**（`features.md` P-26）：工具返回 `sandbox apply/post_spawn failed`（沙箱机制故障，如 Windows Job Object 恢复线程快照竞态）时，Runtime 发起 High risk 询问（C-22 用户显式选定）是否在沙箱外运行；用户 Allow 则以 `DangerFullAccess` 策略重试一次（仅当前调用，不改变会话策略），Deny 则原错误回灌。决策经 `PermissionRequested`/`PermissionResolved` 事件广播并落 audit.log。该回退仅由 Runtime 在执行错误后发起，LLM 无法主动触发（`security.md` §8.10）。

### 9.6 命令风险解释（参考 CC `Ctrl+E`）

当 `InteractivePrompter` 弹出权限确认时，仅显示命令文本不够——用户难以快速判断风险。参考 CC 的 `Ctrl+E` 命令解释，在确认弹窗中附加风险评估：

```rust
pub struct RiskAssessment {
    pub level: RiskLevel,           // Low / Medium / High
    pub summary: String,            // "读取文件" / "删除目录" / "网络请求"
    pub impact: Vec<String>,        // ["写入 src/main.rs", "影响 1 个文件"]
    pub reversible: bool,           // 是否可撤销（有 Journal 记录则 true）
    pub touches_vcs: bool,          // 是否触碰 .git/.hg/.svn
    pub network: bool,              // 是否涉及网络
}

pub enum RiskLevel { Low, Medium, High }
```

风险评估规则（内置启发式，不调 LLM，零成本）：

| 工具 | 条件 | 风险 |
|------|------|------|
| `fs.read`/`grep`/`glob` | 工作目录内 | Low |
| `fs.read` | 工作目录外 | Medium（可能读敏感文件） |
| `fs.write`/`fs.edit` | 工作目录内 + 非 VCS | Low（可 undo） |
| `fs.write` | VCS 目录（.git 等） | High（破坏版本库） |
| `fs.delete` | 任何 | High（删除不可逆，Journal 仅能恢复内容不能恢复元数据） |
| `shell.run` | 只读命令（ls/cat/grep/git status） | Low |
| `shell.run` | 写命令（mkdir/echo/cp） | Medium |
| `shell.run` | 危险前缀（rm/sudo/dd） | High（但已被黑名单 Deny，不会到 Ask） |
| `web.fetch` | allowlist 域名 | Low |
| `web.fetch` | 非 allowlist | Medium |
| `shell.run` | 涉及网络（curl/wget/git push） | Medium-High |

弹窗展示示例：

```
┌─ Permission Required ──────────────────────────┐
│ shell.run: rm -rf target/                       │
│                                                 │
│ Risk: MEDIUM                                    │
│ Impact: 删除 target/ 目录                       │
│ Reversible: No (Journal 无法恢复删除的元数据)   │
│ Touches VCS: No    Network: No                  │
│                                                 │
│ [y] Allow  [n] Deny  [a] Always allow  [e] Explain │
└─────────────────────────────────────────────────┘
```

`[e] Explain`（对齐 CC 的 `Ctrl+E`）展开更详细的风险说明，帮助用户决策。风险评估本身不改变 `Verdict`——它只是让用户的 `Decision` 更知情。

### 9.7 细粒度规则（Granular Rules，参考 Codex v0.122+）

§9.5 的 L1 用户策略中，`ApprovalMode`（specificity=1）是粗粒度的——无法表达"信任的本地 MCP 静默通过，不信任的第三方 MCP 需审批"。细粒度规则（granular rules）是 L1 内 specificity=3~5 的高优先级条目，用于按工具类别/MCP server/路径细分审批策略。它**不是独立层级**，与 `policy.toml` 显式 allow/deny（specificity=2）平等参与 L1 竞争。

```toml
[permission.granular]
# 按 MCP server 细分（specificity=3）
[[permission.granular.rules]]
scope = "mcp:github"
mode = "on-request"          # GitHub MCP 仍逐个确认

[[permission.granular.rules]]
scope = "mcp:local-db"
mode = "never"               # 本地数据库 MCP 静默通过

# 按工具+路径细分（通配 specificity=4，精确 specificity=5）
[[permission.granular.rules]]
scope = "fs.write:src/**"
mode = "on-failure"          # src/ 下写入失败才问

[[permission.granular.rules]]
scope = "fs.write:.env*"
mode = "untrusted"           # .env 写入永远问

# 按命令前缀细分（specificity=3）
[[permission.granular.rules]]
scope = "shell.run:cargo *"
mode = "never"               # cargo 命令静默通过

[[permission.granular.rules]]
scope = "shell.run:git push*"
mode = "untrusted"           # git push 永远问
```

**specificity 计算**（与 §9.5 L1 一致）：

| specificity | 来源 | 示例 |
|:---:|------|------|
| 5 | granular 精确路径（无通配） | `fs.write:.env` |
| 4 | granular 通配路径 | `fs.write:src/**` |
| 3 | granular 工具类别 / MCP server / 命令前缀 | `shell.run:cargo *`、`mcp:github` |
| 2 | `policy.toml` 显式 allow/deny（per-tool，无路径细分） | `tool = "fs.write"` |
| 1 | `ApprovalMode × SideEffect` 全局平移 | `Untrusted` 模式 |
| 0 | §9.3 per-tool 默认矩阵 | `fs.write → Ask` |

**匹配规则**：最高 specificity 命中生效；同 specificity 多条命中 → `deny` 胜出（safe default）；同 specificity 同 verdict 多条命中 → 按声明顺序首条生效。这与 §9.5 的两层模型完全一致——granular rules 只是 L1 内 specificity 较高的条目，无需额外级联。

**与 auto-review（`security.md` §8.9）的协作**：granular rule 的 `mode = "on-request"` 时可触发 auto-review 子代理先评估，auto-review 的 `low` 决策等价于该规则临时降级为 `never`，`high` 等价于升级为 `untrusted`。这实现了 Codex 的"细粒度策略 + auto-review 审查者替换"组合，且无需引入新层级——auto-review 的输出只是动态修改该 granular rule 的 `mode` 字段后重新参与 L1 匹配。

---

## 10. 错误处理与恢复

### 10.1 错误分类

```rust
#[derive(thiserror::Error, Debug)]
pub enum RuntimeError {
    #[error("llm error: {0}")]
    Llm(#[from] LlmError),
    #[error("tool {tool} failed: {source}")]
    Tool { tool: String, source: ToolError },
    #[error("permission denied: {0}")]
    Permission(String),
    #[error("context budget exceeded")]
    BudgetExceeded,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("interrupted")]
    Interrupted,
}
```

### 10.2 恢复策略

| 错误 | 策略 |
|------|------|
| LLM 限流 | 退避重试，仍失败则保留现场退出 |
| LLM 内容过滤 | 注入系统消息提示"上次回复被过滤"，继续循环 |
| 工具错误 | 转为 `ToolResult::error` 回灌，让 LLM 自修正 |
| 工具超时 | 取消该调用，回灌超时错误 |
| 写盘失败 | 内存消息保留，告警并尝试备份到临时文件 |
| Ctrl-C | graceful stop，已生成消息入库 |

### 10.2.1 取消语义（`CancellationToken` 每轮重建）

`tokio_util::sync::CancellationToken` **取消后永久 cancelled，无法复位**。若取消对会话永久生效，一次手动终止后所有后续 turn 都会秒取消（会话被"砖化"，`POST /messages` 返回 `[已取消]`）。因此：

- **每轮重建**：`run_turn` 结束（含取消/超时/正常/错误路径）时 `*cancel_token = CancellationToken::new()`，下一轮使用全新 token。CLI/TUI 的 Ctrl-C handler 每轮经 `Runtime::cancel_token()` 重新获取当前 token，不受重建影响；
- **仅运行中生效**：`Runtime.turn_active`（原子标记，RAII guard 复位，覆盖 `?` 早退/panic）使 `cancel()` 仅在 turn 运行中取消 token——turn 间隙的取消调用（如用户点取消但 turn 已结束）不毒化下一轮；
- **前端残留清理**：`turn_end`（含 `interrupted`）时清空流式文本/思考增量/工具卡片——被终止的 turn 的瞬态渲染不会残留在列表底部。

回归测试：`cancel_then_next_turn_still_works`（运行中取消后下一轮正常）、`cancel_between_turns_is_ignored`（间隙取消不生效）。

### 10.3 会话存储结构（Parent-UUID 链，参考 Claude Code）

原先 JSONL 是纯顺序追加，无法表达"从某点分叉"或"压缩边界"。参考 CC 的 Parent-UUID 链结构，每条消息记录 `uuid` 与 `parent_uuid`，形成链表而非纯数组。

**关键澄清：Parent-UUID 链与 JSONL 追加写不冲突**。`parent_uuid` 只是 JSONL 行的一个可选字段，写入仍是单行 `append`（与 `data-model.md` §2.2 一致）。二者关系如下：

| 维度 | JSONL 追加写（`data-model.md` §2.2） | Parent-UUID 链（本节） |
|------|--------------------------------------|----------------------|
| 写入 | 每行独立 `append(true).write_all`，崩溃最多丢最后一行 | 不变——`parent_uuid` 是行内字段，写入路径相同 |
| 默认读取 | 线性逐行解析即可还原消息序列 | 不变——默认 `parent_uuid` = 上一行 `id`，线性顺序即链顺序 |
| 特殊读取 | 不涉及 | 仅 Fork/Side-chain 检视时需建 DAG；普通 `--resume` 仍线性读 |
| Fork | 不支持 | 复制前缀行到**新文件**（对新文件的写入仍是追加），原文件只读不动 |
| 压缩边界 | 不支持 | 摘要行 `parent_uuid = None`；用 `index.json` 的 `last_compaction_id` 指针 O(1) 定位，无需全文件扫描 |

```rust
pub struct StoredMessage {
    pub uuid: String,              // 本条消息唯一 ID（= data-model.md §2.2 的 `id`）
    pub parent_uuid: Option<String>, // 父消息 UUID；默认 = 上一行 `id`，首条/压缩摘要/side-chain 头为 None
    pub role: Role,
    pub content: Content,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
    pub ts: time::OffsetDateTime,
    pub msg_type: MsgType,         // User/Assistant/ToolResult/Summary/System
    pub metadata: Option<serde_json::Value>,
}
```

链表结构支持三种原来无法表达的能力：

1. **Fork（分叉）**：同一 `parent_uuid` 可有多个子消息，表示"从某点尝试不同方向"。`--fork-session` 复制链前缀到**新会话文件**，原会话文件只读不写——对新会话的写入仍是纯追加，不破坏 JSONL 追加写语义；
2. **Compaction Boundary（压缩边界）**：压缩产生的摘要消息 `parent_uuid = None`，表示"此前历史已折叠进摘要"。为避免"扫描整个文件找最近 `None`"，`sessions/index.json`（`data-model.md` §3.1）新增 `last_compaction_id` 字段指向最近的摘要消息 `id`，`--resume` 时 O(1) 跳过已压缩前缀；`index.json` 损坏时回退到尾向扫描（O(N)，仅异常路径）；
3. **Side-chain（子 Agent 链）**：子 Agent 的 transcript 作为 side-chain 存储在主会话 JSONL 中，`parent_uuid` 指向派发它的 `task.spawn` 工具调用，便于追溯"这个子 Agent 是在哪个轮次派生的"。线性读取主链时跳过 `msg_type = SubagentTranscript` 的连续段即可（按 `parent_uuid` 不在主链上识别）。

**与 `data-model.md` §2.2 的对齐**：`data-model.md` 的 JSONL 记录结构已包含 `id` 字段，本节 `uuid` 即该 `id`（同义词，统一为 `id`）；新增 `parent_uuid` 字段为可选（`#[serde(default)]`），旧文件读取时 `parent_uuid` 默认为 `None`，按"上一行 `id`"线性重建即可，前向兼容。`data-model.md` §2.2 的示例与字段说明同步更新见该文件。

### 10.4 会话恢复

`minicoding --resume <session-id>` 流程：

1. 读 `sessions/index.json` 取 `last_compaction_id`（若无则从文件头开始）；
2. 从该 `id` 对应的行起**线性向下**解析 JSONL（默认每行 `parent_uuid` = 上一行 `id`，无需建 DAG），重建 `ContextManager`；
3. 若用户请求 `--fork` 或检视 side-chain，才按 `parent_uuid` 建 DAG（稀有路径）；
4. 从 `SessionMeta`（§3.7）恢复会话名、权限模式、Plan 文件路径、任务列表等非消息状态；
5. 进入交互模式，可继续提问。

### 10.5 Fork 会话（参考 CC `--fork-session`）

`minicoding --fork-session <session-id>` 复制原会话的链前缀到新会话文件，在新会话中尝试不同方向，原会话不变：

```
原会话 sess_A:
  msg_1 → msg_2 → msg_3 → msg_4
                          │
                          └─ 用户想从 msg_3 后尝试不同方向

fork 后:
  sess_A: msg_1 → msg_2 → msg_3 → msg_4（不变）
  sess_B: msg_1 → msg_2 → msg_3 → msg_5（新方向）
         (复制前缀)         (新消息)
```

Fork 通过**读原文件前缀 → 追加写到新文件**实现（原会话文件只读不写，新会话文件按 JSONL 追加写语义依次 `append` 前缀行 + 后续新消息），新会话 `sess_B` 的后续消息以 `msg_3` 为 `parent_uuid` 继续。fork 操作零风险——既不修改原文件，新文件的写入也遵循 `data-model.md` §3.2 的崩溃安全追加写约定。

适用场景：用户对 Agent 的某次决策不满意，想"回到那个分叉点试试另一条路"，而不丢失原会话的进展。类似 `git branch`。

### 10.6 惰性物化（参考 CC）

会话文件不在 `minicoding` 启动时创建，而是**首条消息写入时才物化**到磁盘：

```
minicoding 启动
   │
   ├─ 分配 session_id（内存）
   ├─ 不创建 sessions/{id}.jsonl 文件
   │
   ▼
首条用户消息
   │
   ├─ storage.append() → 此时才创建 JSONL 文件 + 写入首条
   └─ SessionMeta 一并落盘
```

这避免"启动即退出"产生孤儿元数据文件（如用户启动后 Ctrl-C，或 `--help` 误触发会话初始化）。会话列出时只扫描实际存在的 JSONL 文件，不依赖元数据索引。

### 10.7 会话列出与 64KB 窗口（参考 CC）

`minicoding sessions list` 列出数千会话时，不全量反序列化每个 JSONL（可能数 MB），而是只读首尾 64KB：

- **首 64KB**：提取 session_id、首条用户消息（作标题预览）、创建时间、消息类型分布；
- **尾 64KB**：提取最后一条消息时间、当前状态、消息计数；
- 中间内容跳过，按需 `minicoding sessions show <id>` 全量加载。

这使得万级会话列表的加载时间 < 200ms（与 §13 性能预算一致）。

---

## 11. 事件系统

### 11.1 事件类型

`Event` 全部字段必须 `Clone`——事件总线是 `broadcast`，每条事件会被克隆给所有订阅者。因此**事件中绝不携带 `oneshot::Sender` / `mpsc::Sender` 等不可克隆的回复通道**；需要回复的语义（如权限交互）走独立的点对点 trait（`PermissionPrompter`，见 §9），事件总线只承担"通知"职责。

```rust
pub enum Event {
    MessageAppended(Message),
    Token(String),
    TurnStreamingStarted,
    ToolCallStart(ToolCall),
    ToolCallProgress { id: ToolCallId, bytes: usize },
    ToolCallEnd { id: ToolCallId, ok: bool, elapsed: Duration },
    /// 通知：权限已询问（仅展示/审计，无回复通道）。
    PermissionRequested { id: String, tool: String, summary: String, risk: Risk },
    /// 通知：权限已 resolved。
    PermissionResolved { id: String, decision: Decision },
    TurnEnd { stop_reason: StopReason },
    Error(RuntimeError),
    /// 子 Agent 启动/结束（§7）。
    SubagentStarted { id: String, role: String },
    SubagentFinished { id: String, summary: String },
    /// task.update 工具调用后广播，供 UI 渲染任务进度（§18.4）。
    TaskUpdated { task: Task },
    /// Hook 执行结果通知（hooks.md §8 / §20.2）。
    HookRun { name: String, event: String, decision: HookDecision, elapsed: Duration },
    /// Plan 模式状态切换（§16.2）。
    PermissionModeChanged { from: PermissionMode, to: PermissionMode },
    /// 文件回滚执行结果（§17.4）。
    FileUndone { report: UndoReport },
    /// 配置文件变更通知（S-22 热更新）。
    /// `ConfigWatcher` 监听 `~/.minicoding/config.toml`，500ms debounce 后广播；
    /// 需要响应变更的组件（扩展 `on_config_changed`、TUI 重渲染等）自行订阅处理。
    ConfigChanged,
}
```

> 权威定义见 `api.md` §4。所有事件字段必须 `Clone + Send`（broadcast 要求）。`TaskUpdated` 替代旧名 `TodoUpdated`（见 `api.md` §10.1 废弃别名说明）。

### 11.2 事件总线

- `broadcast::Sender<Event>`，多订阅者。
- 订阅者消费慢时：token 事件可丢弃（合并连续空白），控制事件（`Permission*`/`TurnEnd`/`Error`）不可丢——通过有界 channel + 优先级丢弃策略实现。
- Frontend 订阅做渲染；Storage 订阅做审计落盘；tracing 订阅做结构化日志；OpenTelemetry 订阅做 span 事件（见 §14）。
- **权限回复不走总线**：`PermissionRequested` 仅是通知，真正的决策由 Runtime 调 `prompter.prompt()` 同步获得，再发 `PermissionResolved`。这保证了即使没有任何订阅者，权限流程也能正常推进。

---

## 12. 配置 Schema（节选）

```toml
# .minicoding.toml
[provider]
default = "anthropic"

[provider.anthropic]
model = "claude-sonnet-4"
api_key_env = "ANTHROPIC_API_KEY"   # 不直接写密钥

[context]
budget_ratio = 0.85
compress_strategy = "summary_then_truncate"

[tools]
enabled_groups = ["core", "fs", "shell", "web"]
shell.timeout_sec = 120

[permission]
default = "ask"
[[permission.allow]]
tool = "fs.write"
glob = "src/**"

[storage]
dir = "~/.minicoding/sessions"
```

---

## 13. 性能预算

| 指标 | 目标 | 设计保障 |
|------|------|----------|
| 冷启动 | < 50ms | 无重依赖初始化，配置惰性加载 |
| 单轮工具调度开销 | < 10ms | 注册表 HashMap 查找，零拷贝 dispatch |
| 流式首 token 转发 | < 2ms（本地） | 事件总线直通，无中间缓冲 |
| 内存占用（10 万行代码库） | < 100MB | 不全量加载，按需读取 |
| 会话恢复 | < 200ms | JSONL 顺序读 + 流式重建 |

---

## 14. 测试策略（设计层面）

- **单元层**：每个 trait 实现可独立测，使用 mock provider / mock tool。
- **集成层**：用 `wiremock` 模拟 LLM 上游，跑完整 Agent 循环，断言消息序列与工具调用。
- **回放测试**：录制真实会话 JSONL，作为回归用例。
- **属性测试**：`proptest` 验证压缩管道在任意消息序列下不破坏不变量。
- **并发测试**：并行工具调用下的消息顺序一致性。
- **共享测试工具**：`crates/minicoding-core/tests/common/` 提供 stub 替身（`NoopMcpClient`/`NoopHookRegistry`/`StubJournal`/`StubTaskRegistry`/`StubPermissionPolicy` 等），M0 起由 T-M0-9 建立骨架（见 `dev-plan.md` §3），各 milestone 的单测/集成测试均依赖此目录。
- **独立测试策略**：§16-§20 五个子系统（Plan/Journal/Task/MCP/Hooks）在所在里程碑内可独立测试，无需等待下游子系统实现，详见 §21。

详见 `roadmap.md` 的测试里程碑与 `dev-plan.md` T-M0-9 测试基础设施骨架。

---

## 15. 可观测性：OpenTelemetry 集成

OpenTelemetry（OTel）作为**一等公民**内建，而非"后续可选"。所有跨组件边界都打 OTel span，便于对接 Jaeger/Tempo/Grafana 等后端，定位长会话中的延迟与异常。

### 15.1 Span 层级

```
session (session.id)                         <- RootSpan
 └─ turn (turn.n)                            <- 每轮对话
     ├─ context.build_chat_request           <- 上下文构建/压缩
     │    └─ compress (strategy, level)      <- 压缩步骤
     ├─ llm.chat_stream (provider, model)    <- LLM 调用
     │    └─ retry.attempt (n)               <- 重试
     └─ tool.call (tool.name, side_effect)   <- 工具调用
          ├─ permission.check (verdict)
          ├─ permission.prompt (decision)    <- 仅 Ask 时
          └─ tool.dispatch (elapsed, ok)
```

### 15.2 关键属性（attributes）

| Span | 属性 |
|------|------|
| `session` | `session.id`, `session.workdir`, `provider`, `model` |
| `turn` | `turn.index`, `turn.input_tokens`, `turn.output_tokens` |
| `llm.chat_stream` | `llm.provider`, `llm.model`, `llm.stop_reason`, `llm.cached_tokens` |
| `tool.call` | `tool.name`, `tool.side_effect`, `tool.parallel` (bool), `tool.ok`, `tool.elapsed_ms`, `tool.truncated` |
| `permission.check` | `permission.verdict`, `permission.matched_rule` |
| `compress` | `compress.level`, `compress.tokens_before`, `compress.tokens_after` |

### 15.3 实现要点

- `tracing` + `tracing-opentelemetry` 桥接：业务代码只写 `tracing` 宏，由 subscriber 层导出为 OTLP。
- 导出协议 OTLP/HTTP（默认）或 OTLP/gRPC；后端地址由 `OTEL_EXPORTER_OTLP_ENDPOINT` 标准环境变量配置，零代码改动。
- 资源属性：`service.name = minicoding`, `service.version`, `host.name`, `session.id`（作为自定义 resource）。
- 采样：默认 `AlwaysOn`（本地调试）/ `TraceIdRatio` 0.1（生产），由 `OTEL_TRACES_SAMPLER` 控制。
- Context 传播：子 Agent 通过 OTel Context 传播父 span，使子任务挂在主会话 trace 下。
- 事件总线订阅者把 `Event::ToolCallEnd` 等转为 span events，使"日志"与"trace"在同一时间线对齐。

### 15.4 与本地日志的关系

`tracing-subscriber` 同时输出本地文件日志（`logs/`）与 OTel export：本地日志面向单机排障，OTel 面向跨会话/跨机器聚合分析。两者共享同一 `tracing` 调用点，无重复埋点开销。`RUST_LOG` 控制本地日志级别；OTel 采样独立控制，互不干扰。

---

## 16. Plan 模式（计划模式，参考 Claude Code）

Plan 模式是独立于 `ApprovalMode`（§9.5）的第四种宏观模式，用于"先调研、再写计划、用户批准后才执行"的场景：审查陌生代码库、跨模块重构、危险操作前的影响评估。它解决一个真实痛点——直接让 Agent 动手容易在大改动半路发现方向错了，而用户只能眼看着文件被改。

### 16.1 双重只读强制（defense in depth）

参考 CC 的"硬门 + 软引导"双层设计，避免依赖单一机制：

| 层 | 实现 | 作用 |
|----|------|------|
| 硬门 | `PermissionPolicy::check` 在 `PermissionMode::Plan` 下，对所有 `side_effect != None` 的工具直接返回 `Deny("plan mode: read-only")` | 即使 LLM 尝试调用写工具，Runtime 也强制拒绝 |
| 软引导 | 每次 `build_chat_request` 注入 system reminder："Plan mode is active. You MUST NOT make any edits, run destructive commands, or make changes." | 让模型自觉避免尝试写操作，减少无效工具调用的 token 浪费 |

软引导不是安全边界（`rules.md` §5 已声明"提示词不是安全边界，Rust 代码才是"），它的价值是降低成本——硬门保证安全，软引导减少浪费。

### 16.2 PermissionMode 枚举（扩展 §9）

把原先单一的"默认 ask"策略升级为模式枚举，`Plan` 是其中之一：

```rust
pub enum PermissionMode {
    Default,          // §9.3 默认矩阵（写 Ask）
    AcceptEdits,      // 文件写入自动 Allow，shell 仍 Ask（高频编辑场景）
    Plan,             // 只读强制（硬门 + 软引导）
    Auto,             // 分类器自动批准（含降级保护，阶段 6+）
    BypassPermissions,// 全放行（仅隔离容器内，对齐 CC bypassPermissions）
}
```

`Plan` 与 `ApprovalMode`（§9.5）正交：`Plan` 是"工具能力面"约束（禁写），`ApprovalMode` 是"何时问用户"约束。`Plan` 模式下 `ApprovalMode` 通常配 `OnRequest`，写操作根本进不到"问不问"那一步就被硬门拦了。

CLI：`minicoding --plan` 进入 Plan 模式；斜杠命令 `/plan` 切换；`/plan open` 在外部编辑器打开 plan 文件。

**模式生效范围**（`policy::builtin` 实现层强制）：

- `AcceptEdits`：工作区内 `FileWrite` 自动 `Allow`（高频编辑场景不弹窗），`shell.run`/`web.fetch` 仍 `Ask`；
- `BypassPermissions`：`Command`/`Network`/工作区内 `FileWrite` 全部自动 `Allow`（仅受信隔离容器内使用）；
- 二者均不放松 L0 硬约束：内置黑名单（C-02）、路径越界 `Deny`（C-03）、AGENTS.md/CLAUDE.md 写 `Ask`（C-23）在 `AcceptEdits`/`BypassPermissions` 下依旧生效（实现见 `minicoding-policy/src/builtin.rs` `compute_verdict`/`check_file_write`）。

server 通过 `--preset` 或会话级 `CreateSessionBody.preset` 设定模式与沙箱策略（`auto`/`read-only`/`external-sandbox`/`full-access`，full-access 映射 `BypassPermissions` + `DangerFullAccess`，C-22 警告），见 `docs/security.md` §2.6。

### 16.3 工作流（参考 CC 5 阶段）

```
Phase 1: Explore（只读探查）
   ├─ 主 Agent 用只读工具 + Explore 子 Agent 调研代码库
   └─ 收集影响范围、依赖、风险点
        ▼
Phase 2: Write Plan
   ├─ 写 .minicoding/plan.md（人机共读 Markdown）
   ├─ 含：目标 / 步骤分解 / 每步影响文件 / 风险 / 验证方式
   └─ 产出 ExitPlanMode 工具调用
        ▼
Phase 3: Verify（自检）
   ├─ 检查步骤依赖无环
   ├─ 检查每步影响的文件确实存在
   └─ 检查验证方式可执行
        ▼
Phase 4: Present（呈现给用户）
   ├─ 展示 plan.md 摘要
   └─ 等待用户决策
        ▼
用户决策门
   ├─ approve  → 退出 Plan 模式，进入执行期（Default/AcceptEdits）
   ├─ modify   → 回到 Phase 2 修订
   └─ reject   → 取消，不执行
```

### 16.4 ExitPlanMode 工具

模型在 Phase 2 完成后调用 `plan.exit` 工具，携带"预批准命令"清单，执行期可跳过权限门减少摩擦（参考 CC 的 `allowedPrompts`）：

```rust
pub struct ExitPlanModeInput {
    pub plan_path: Utf8PathBuf,                    // .minicoding/plan.md
    pub allowed_prompts: Vec<PreApprovedPrompt>,   // 执行期预批准
    pub plan_was_edited: bool,                     // 用户是否手改过 plan.md
}

pub struct PreApprovedPrompt {
    pub tool: String,       // "shell.run"
    pub prompt: String,     // "cargo build", "cargo test", "git add"
}
```

`plan.exit` 触发后 `PermissionMode` 从 `Plan` 切回 `Default`（或 `AcceptEdits`，由用户在决策门选择），并把 `allowed_prompts` 注入会话级 `PermissionPolicy`——执行期匹配到这些 prompt 的工具调用直接 `Allow`，跳过 prompter。这是 Plan 模式的关键便利点：用户**一次性**批准计划中的命令，而非每条命令逐个确认。

### 16.5 Plan 文件持久化

`.minicoding/plan.md`（对齐 CC 的 `.claude/plan.md`），跨会话存活，便于：

- 用户在外部编辑器修订（`/plan open`）后回读；
- 中断后 `--resume` 可继续未完成的计划；
- 作为改动记录留档。

文件格式：

```markdown
# Plan: <task title>
- 创建：2026-07-24
- 状态：pending_approval | approved | executing | completed | rejected

## 目标
<用户需求复述>

## 步骤
1. [ ] <步骤1> —— 影响：src/foo.rs, src/bar.rs —— 风险：低
2. [ ] <步骤2> —— 影响：tests/foo.rs —— 风险：中 —— 验证：cargo test foo

## 风险
- <风险点1>

## 验证
- cargo test
- cargo clippy -- -D warnings
```

### 16.6 与既有抽象的关系

- **与 §9 权限**：`Plan` 是 `PermissionMode` 的一种，硬门在 `PermissionPolicy::check` 内实现；`allowed_prompts` 是会话级 `Verdict` 缓存。
- **与 §7 子 Agent**：`SubagentType::Plan` 仅在 `PermissionMode::Plan` 下可派发，用于 Phase 1 探查（参考 CC 的 Plan 子 Agent）。
- **与 §18 任务管理**：Plan 批准后，执行期用 `task.create`/`task.update` 把步骤分解为可跟踪的任务列表，二者协作（参考 CC 的 Plan → Task 协作）。
- **与 §17 文件回滚**：Plan 批准后进入执行期，文件改动写入 `FileChangeJournal`，可 `/undo` 回到批准点。

---

## 17. 文件改动事务与回滚（参考 Codex /undo）

`ContextManager`（§3.5）的 `snapshot/restore` 只回滚**上下文**，不回滚**文件**。但 AI 改文件半路出错是常见场景——模型改了一半发现方向错了、或某步工具失败留下半成品。参考 Codex 的 `/undo`（operation 级）与 `/new`（会话级），引入 `FileChangeJournal`。

### 17.1 设计目标与边界

| 范围 | 支持 | 机制 |
|------|:---:|------|
| 会话内 operation 级（撤销最近 N 次文件改动） | ✅ | `FileChangeJournal` + `/undo` |
| 会话内会话级（回到会话启动时快照） | ✅ | `/new`（清空 journal + 重建初始快照） |
| 跨会话回滚 | ❌ | 依赖 Git（`git checkout`/`git revert`），与 Codex 一致 |
| 回滚对话上下文 + 文件 | ⚠️ | 上下文走 §3.5 snapshot；文件走 journal；二者独立 |

跨会话回滚不内建，因为快照存储成本高且 Git 已是成熟方案——这是 Codex 的明确取舍，本项目沿用。

### 17.2 FileChangeJournal 数据结构

```rust
pub struct FileChangeJournal {
    entries: Vec<ChangeEntry>,   // 按操作顺序追加，会话内有效
}

pub struct ChangeEntry {
    pub op_id: OpId,             // 关联触发该批改动的 turn / prompt
    pub ts: time::OffsetDateTime,
    pub prompt_snippet: String,  // 触发该批改动的用户消息前 80 字（供 /undo 预览）
    pub files: Vec<FileChange>,
}

pub enum FileChange {
    Written { path: Utf8PathBuf, before: Option<Vec<u8>>, after: Vec<u8> },
    Edited  { path: Utf8PathBuf, before: Vec<u8>, after: Vec<u8> },
    Deleted { path: Utf8PathBuf, content: Vec<u8> },
    Created { path: Utf8PathBuf, content: Vec<u8> },
}
```

`before: Option<Vec<u8>>` 中 `None` 表示新建文件；`Deleted.content` 用于撤销时恢复。

### 17.3 记录时机

`fs.write`/`fs.edit`/`fs.delete` 在 `Tool::execute` **成功后**调用 `journal.record(entry)`。失败的工具调用不记录（无副作用发生）。每个 turn 的所有文件改动合并为一个 `ChangeEntry`，`op_id` 关联该 turn 的用户消息 id——这样 `/undo 1` 撤销"最近一次用户消息触发的所有文件改动"，符合直觉。

### 17.4 接口

> 权威定义见 `api.md` §3.11（含 `#[trait_variant::make(Journal: Send)]`）。本节仅展示核心签名与冲突检测语义。

```rust
#[trait_variant::make(Journal: Send)]
pub trait Journal {
    /// 记录一次 turn 的文件改动（fs.write/edit/delete 成功后调用）。
    async fn record(&self, entry: ChangeEntry) -> Result<(), JournalError>;
    /// 撤销最近 steps 次 turn 的文件改动（/undo），含冲突检测。
    async fn undo(&self, steps: usize) -> Result<UndoReport, JournalError>;
    /// 列出会话内所有文件变更（/diff）。
    async fn diff(&self) -> Result<Vec<DiffEntry>, JournalError>;
    /// 回到会话启动时状态（/new），清空 journal + 重建初始快照。
    async fn reset_to_initial(&self) -> Result<(), JournalError>;
}

pub struct UndoReport {
    pub undone_entries: usize,
    pub restored_files: Vec<Utf8PathBuf>,
    pub failed_files: Vec<(Utf8PathBuf, JournalError)>,  // 如文件已被外部改动
}
```

`undo` 反向遍历 `steps` 个 `ChangeEntry`，逐文件恢复 `before` 状态。**冲突检测**：恢复前比对当前文件内容与 `after`，不一致则该文件记入 `failed_files`（用户可能在外部编辑器改过），不强行覆盖。这是 Codex `/rewind` 未实现但社区强烈要求的安全行为。

### 17.5 CLI

```
minicoding> /undo              # 撤销最近一次 turn 的文件改动
minicoding> /undo 3            # 撤销最近 3 次
minicoding> /diff              # 展示会话内所有文件变更
minicoding> /new               # 回到会话启动时状态（清空 journal）
```

`/undo` 是特性门控（`[features] file_undo = false`，默认关，参考 Codex `features.undo`），因为 `before` 内容驻留内存有成本。开启时 `FileChangeJournal` 由 Runtime 持有，会话结束即销毁（不落盘，因为含文件原文，落盘等于多存一份敏感数据）。

### 17.6 与 Git 的协作

- `/undo` 是**会话内**快速回滚，不触碰 Git 工作区状态；
- 跨会话/跨会话历史回滚引导用户用 Git：`/diff` 输出可导向 `git diff`，`/undo` 失败时建议 `git checkout`；
- 不与 `git.apply` 冲突：`git.apply` 走 `fs` 层 patch，其改动同样进 journal（因为最终是文件写入）。

---

## 18. 任务管理工具（TaskCreate/TaskUpdate/TaskList，参考 Claude Code v2.1.142+）

任务管理是 Claude Code 验证过的"模型自我规划"能力。CC 早期用 `TodoWrite`（全量替换 todos 数组），v2.1.142+ 升级为 `TaskCreate`/`TaskUpdate`/`TaskGet`/`TaskList` 增量模型，支持任务依赖与跨会话持久化。本项目跟进 CC 的增量模型，提供 `task.create`/`task.update`/`task.list` 三件套。

### 18.1 设计哲学

不是 Runtime 强制规划，而是**给模型一个显式的规划载体**。模型在 system prompt 引导下主动调用任务工具维护进度，比"在文本里列清单"更结构化、可观测、可被 Runtime 检测遗忘。与 Plan 模式（§16）协作：Plan 批准后，执行期用任务工具把步骤分解为可跟踪项。

### 18.2 从全量替换到增量模型（参考 CC 演进）

全量替换（旧 `todo.write`）的问题：每次更新都需传入完整列表，列表长时 token 浪费严重，且无法表达"只改一个任务的状态"。增量模型按 `task_id` 补丁更新，更高效、可监控：

| 维度 | 旧 `todo.write`（全量） | 新 `task.create`/`task.update`（增量） |
|------|------------------------|---------------------------------------|
| 更新方式 | 每次传完整 todos 数组 | 按 task_id 单项创建/更新 |
| Token 成本 | O(N) 每次 | O(1) 每次更新 |
| 任务依赖 | 不支持 | `add_blocks`/`add_blocked_by` |
| 持久化 | 会话内 | 跨会话（§18.5） |
| 监控粒度 | 整列表快照 | 单项变更事件 |

### 18.3 数据结构与工具

```rust
// task.create —— 创建单个任务，返回 task_id
pub struct TaskCreateInput {
    pub subject: String,              // 必填，任务标题
    pub description: Option<String>,  // 详细说明
    pub active_form: Option<String>,  // in_progress 时显示的动态文本（如"正在编译..."）
    pub priority: Option<Priority>,
    pub metadata: Option<serde_json::Value>,
}
pub struct TaskCreateOutput {
    pub task_id: String,              // Runtime 生成，如 "task_01H..."
}

// task.update —— 按 task_id 增量更新
pub struct TaskUpdateInput {
    pub task_id: String,
    pub status: Option<TaskStatus>,
    pub subject: Option<String>,
    pub description: Option<String>,
    pub active_form: Option<String>,
    pub add_blocks: Option<Vec<String>>,       // 本任务阻塞哪些 task_id
    pub add_blocked_by: Option<Vec<String>>,   // 本任务被哪些 task_id 阻塞
    pub owner: Option<String>,                 // 分配给哪个子 Agent（后续）
    pub metadata: Option<serde_json::Value>,
}

// task.list —— 列出所有任务（含状态、依赖）
pub struct TaskListInput {
    pub status_filter: Option<TaskStatus>,     // None=全部
}
pub struct TaskListOutput {
    pub tasks: Vec<Task>,
}

pub struct Task {
    pub id: String,
    pub subject: String,
    pub description: Option<String>,
    pub active_form: Option<String>,
    pub status: TaskStatus,
    pub priority: Option<Priority>,
    pub summary: Option<String>,       // completed 时写入"实际完成内容/证据"
    pub blocks: Vec<String>,           // 本任务阻塞的 task_id 列表
    pub blocked_by: Vec<String>,       // 阻塞本任务的 task_id 列表
    pub owner: Option<String>,
    pub created_at: time::OffsetDateTime,
    pub updated_at: time::OffsetDateTime,
}

pub enum TaskStatus { Pending, InProgress, Completed, Deleted }
pub enum Priority { High, Medium, Low }
```

### 18.4 任务依赖与阻塞（参考 CC addBlocks/addBlockedBy）

任务间可声明阻塞关系："先 A 才能做 B"。Runtime 维护依赖图，自动协调：

```
task.update(task_id="B", add_blocked_by=["A"])
   │
   ▼
B.blocked_by = ["A"]
   │
   ▼
当模型尝试 task.update(B, status=InProgress) 时：
   ├─ A.status == Completed → 允许
   └─ A.status != Completed → 拒绝，返回错误：
        "Task B is blocked by task A (status: Pending).
         Complete A first or remove the dependency."
```

依赖图检测成环（A→B→C→A）：`add_blocks`/`add_blocked_by` 时做 DFS 检测，成环则拒绝并报错。这避免模型建立循环依赖导致死锁。

依赖的典型用法：并行子 Agent 改不同模块时，声明"模块 B 的测试依赖模块 A 的接口定义"——A 未完成时 B 的测试任务被阻塞，Agent 会先做 A。

### 18.5 持久化与跨会话（参考 CC）

任务列表持久化到 `~/.minicoding/tasks/`（JSONL），跨会话、跨上下文压缩存活：

- **压缩免疫**：任务列表存 `SessionMeta`（§3.7），不进 `messages`，不受压缩管道影响；
- **resume 恢复**：`--resume <session>` 时从磁盘重建任务列表，继续未完成任务；
- **跨会话共享**（后续）：`MINICODING_TASK_LIST_ID` 环境变量让多会话共享同一任务列表（参考 CC 的 `CLAUDE_CODE_TASK_LIST_ID`），适合多 Agent 协作大任务；
- **清理**：所有任务 `Completed`/`Deleted` 后列表自动归档到 `tasks/archive/{session_id}.jsonl`；
- **HTTP 查询路径**（§24）：`TaskStore`（内存态）是任务权威源，`ServerSession::task_state`
  常驻订阅 `Event::TaskUpdated` 镜像其变更，`GET /sessions`/`GET /sessions/{id}` 返回该快照
  （此前 `SessionMeta.tasks` 恒空导致前端任务面板无数据）；CLI/TUI 侧走 `TaskStore` 直读。

### 18.6 校验规则（参考 CC 硬约束）

- 最多 20 个 active 任务（`Pending`/`InProgress`，不含 `Completed`/`Deleted`，防列表爆炸）；
- `subject` 必填非空；
- **同一时间最多 1 个 `InProgress`**（强制专注单任务，防模型并行多步导致混乱）；
- `Completed` 必须填 `summary`（参考 CC v2.1.142+ 要求"完成需有证据"）；
- 状态迁移合法性：`Completed` 不能回到 `Pending`；`Deleted` 是终态；
- `add_blocks`/`add_blocked_by` 的 task_id 必须存在；不可自引用（`add_blocks=["self"]` 拒绝）；
- 依赖图不可成环（DFS 检测）。

### 18.7 渲染

```
[>] task_01  读取 src/main.rs 确定入口结构            in_progress
[ ] task_02  拆分 utils 模块为 path.rs / output.rs    pending
    └─ blocked_by: task_01
[ ] task_03  为 path.rs 补充边界测试                  pending
    └─ blocked_by: task_02
(0/3 completed)
```

CLI/TUI 订阅 `Event::TaskUpdated` 渲染；`InProgress` 项高亮，阻塞项标注依赖链。`Ctrl+T`（TUI）随时查看当前任务列表。

### 18.8 system prompt 协作

注入主 system prompt（`rules.md` §5 `[Soft Rules]` 段）：

```
Use task.create/task.update to plan multi-step tasks. Mark a task as in_progress before
starting it, and completed when done (with a summary of what was actually accomplished, e.g.
test names or diff stats). Use add_blocks/add_blocked_by to declare task dependencies when
tasks have ordering constraints. Do not end your turn before all tasks are completed or
explicitly deferred. Prefer tool calls over prose for tracking progress.
```

**遗忘检测**：若 assistant 消息后任务状态未变且仍有 pending/in_progress，Runtime 注入 system reminder 提醒模型更新任务（参考 CC 的遗忘提醒机制）。这是软引导，不强制——模型若坚持不更新，Runtime 不阻断，但会在 UI 上显示"任务状态过期"。

### 18.9 与既有抽象的关系

- **与 §16 Plan 模式**：Plan 批准后，模型可用 `task.create` 把 plan 步骤转为任务列表跟踪执行；plan 是"该做什么"的文档，task 是"现在做到哪了"的状态机。可批量 `task.create` 把 plan 步骤一次性导入。
- **与 §17 文件回滚**：每个 `InProgress` → `Completed` 的迁移可关联一个 `ChangeEntry`，便于"撤销到某个 task 开始前"（后续增强）。
- **与 §7 子 Agent**：`task.owner` 字段可把任务分配给子 Agent，实现"父 Agent 规划、子 Agent 执行"的分工（后续增强）。
- **与 §15 OTel**：任务工具调用打 `tool.call` span，属性 `tool.name=task.create/update`、`task.id`、`task.status`、`task.blocks`。
- **与事件总线**：广播 `Event::TaskUpdated { task: Task }`（可克隆，与 broadcast 兼容，权威定义见 `api.md` §4），供 UI 渲染。
- **与 §3.7 压缩状态保留**：任务列表存 `SessionMeta`，压缩后不丢失。

### 18.10 向后兼容

旧 `todo.write`（全量替换）作为别名保留一个版本：`todo.write` 内部转为"先批量 `task.create`，再差异 `task.update`"，保证既有提示词与脚本不破。新代码与新提示词应直接用 `task.create`/`task.update`。

---

## 19. MCP 集成（Model Context Protocol，参考 Codex/CC）

原先 features.md E-04 仅"MCP server（被其他 Agent 调用）"一行，缺乏**消费侧**设计。参考 Codex 的 `[mcp_servers.*]` 配置与 CC 的 `.mcp.json` 三作用域，补齐 MCP client 消费侧——这是 AI Coding 工具生态的关键接入点（GitHub/Slack/数据库等外部能力通过 MCP server 暴露给 Agent）。

### 19.1 双向定位

- **MCP client（消费侧）**：minicoding 作为 MCP client，连接外部 MCP server，把其工具注册进 `ToolRegistry`，与内置工具统一调度。**这是 MVP+ 的重点**。
- **MCP server（暴露侧）**：minicoding 自身作为 MCP server，把内置 `fs`/`shell` 工具暴露给其他 Agent 调用（features E-04，阶段 7）。

### 19.2 传输与配置

```toml
# .minicoding.toml
[mcp_servers.github]
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }   # 环境变量展开（${VAR} / ${VAR:-fallback}）
cwd = "."
startup_timeout_sec = 20
tool_timeout_sec = 60
enabled = true
required = false                # true 时启动失败则 minicoding 拒绝启动
enabled_tools = ["list_prs", "create_pr"]   # None=全部

[mcp_servers.internal_api]
transport = "http"
url = "https://internal.corp/mcp"
bearer_token_env_var = "INTERNAL_API_TOKEN"   # 不直接写 token
http_headers = { "X-Client" = "minicoding" }
```

`McpTransport` 枚举覆盖两种主流协议；`bearer_token_env_var` 走环境变量引用而非明文，与 `security.md` §6 凭证管理一致。

**传输实现**（T-M6-4）：
- **stdio**：`TokioChildProcess`（`rmcp` 2.2 `transport-child-process` feature），子进程 `env_clear` + 白名单注入（C-04）；
- **http**：`StreamableHttpClientTransport`（`rmcp` 2.2 `transport-streamable-http-client-reqwest` feature），bearer token 从 `bearer_token_env_var` 读取后传入 `StreamableHttpClientTransportConfig::auth_header`，自定义 headers 转为 `HeaderName`/`HeaderValue`。token 不落日志（C-04）。

两种传输共用 `start_one` 流程：握手（`ClientInfo::default().serve(transport)` 带 `startup_timeout`）→ `list_all_tools` → schema 转换 + `enabled_tools` 过滤 → 缓存 `ServerConnection`。

### 19.3 工具命名与权限规则

参考 CC 的 `mcp__<server>__<tool>` 命名，使 MCP 工具与内置工具在权限规则中可区分：

```rust
fn mcp_tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}
// 例：mcp__github__list_prs
```

权限规则（`policy.toml`）支持通配：

```toml
[[permission.allow]]
tool = "mcp__github__list_prs"      # 精确允许
[[permission.deny]]
tool = "mcp__github__*"             # 拒绝 github 所有其它工具
```

MCP 工具的 `side_effect` 由 server 在工具 schema 中声明（`readOnlyHint`/`destructiveHint`）；minicoding 信任但校验——若 server 声明只读但实际有写行为，审计日志（`security.md` §7）会留下证据供事后追溯。这是 L1 契约约束（`rules.md` C-11）的延伸：MCP server 是外部实现，无法强制如实标注，但审计可发现。

### 19.4 作用域（参考 CC 三作用域）

| 作用域 | 存储位置 | 共享 | 首次使用 |
|--------|---------|------|---------|
| `local`（默认） | `~/.minicoding/mcp.json` | 私有当前用户 | 直接可用 |
| `project` | `.minicoding/mcp.json`（仓库根，入版本控制） | 团队共享 | **首次需逐人批准**（防恶意仓库植入 MCP server） |
| `user` | `~/.minicoding/mcp.json` 的 `[user]` 段 | 全局 | 直接可用 |

`project` 作用域的"首次批准"是关键安全机制——参考 CC 防止恶意仓库通过 `.mcp.json` 植入恶意 server：首次 clone 一个含 `.minicoding/mcp.json` 的仓库时，minicoding 提示用户逐个确认是否启用其中的 MCP server，确认后写入 `~/.minicoding/mcp_choices.toml` 记忆。`minicoding mcp reset-project-choices` 可重置。

### 19.5 MCP 进程池与后台预热

§19.6（生命周期）描述的"Runtime 启动时并发启动各 MCP server"是同步阻塞模型——首 turn 必须等所有 server 握手完成。当配置多个重型 MCP server（如 GitHub + Slack + 内部数据库）时，启动延迟叠加明显。本节引入进程池复用与后台预热，把启动延迟从首 turn 路径移到后台。

**进程池复用**：MCP server 子进程跨 turn 复用，不每 turn 重启。`McpClient` 持有 `HashMap<ServerId, McpConnection>`，连接一旦建立长期保活，直到 Runtime 关闭或 server 崩溃。这与 §19.6（生命周期）的"Runtime 关闭时优雅关闭"一致——只是不再每 turn 启停。

```rust
pub struct McpConnectionPool {
    connections: RwLock<HashMap<ServerId, Arc<McpConnection>>>,
    inflight: RwLock<HashMap<RequestKey, Shared<Future<ToolResult>>>>,
    health_check_interval: Duration,  // 默认 30s
}

pub struct McpConnection {
    pub transport: McpTransport,        // stdio 子进程或 http 连接池
    pub tools: Vec<ToolSchema>,          // 握手时 list_tools 缓存
    pub last_ping: Instant,
    pub state: ConnectionState,           // Healthy / Degraded / Disconnected
}
```

`connections` 用 `RwLock` 是因为工具调用时只读访问连接，仅健康检查与重连时写。`inflight` 用于下文的请求合并。

**后台预热**：分两类 MCP server 启动时机：

| 类型 | 触发时机 | 阻塞首 turn？ |
|------|---------|---------------|
| 全局 server（`~/.minicoding/mcp.json`） | minicoding 进程启动 | 否（后台预热，首 turn 仅在未完成时阻塞） |
| 项目级 server（`.minicoding/mcp.json`） | 创建/resume session 时 | 否（后台预热，首 turn 仅在未完成时阻塞） |

```
Runtime 启动 / session 创建
   │
   ▼
并发派发预热任务（每个 server 一个 tokio::task）
   │
   ├─ 全局 server：立即开始握手 + list_tools
   ├─ 项目级 server：首次需用户批准（§19.4），批准后开始预热
   │
   ▼
首 turn 开始
   │
   ├─ 所需 server 已预热完成 → 直接用，零延迟
   └─ 所需 server 仍在预热 → 等待（带 timeout），超时则该 server 工具本 turn 不可用
```

全局 server 在进程启动即预热，session 创建时通常已就绪；项目级 server 在 session 创建时启动预热，首 turn 仅在用户立即提交 prompt 且预热未完成时阻塞。这把"启动延迟"从必经路径移到"用户阅读 AGENTS.md / 输入首条消息"的并行时间段。

**Inflight merge**：同 server 的并发工具调用若参数相同，合并为一次实际调用，避免重复请求外部服务（如并发的 `mcp__github__list_prs` 只发一次 HTTP）。通过 `futures::future::Shared` 把首个请求的 `Future` 共享给后续调用：

```rust
async fn call_tool(&self, server: &str, tool: &str, input: Value) -> ToolResult {
    let key = RequestKey { server, tool, input_hash: hash(&input) };
    if let Some(shared) = self.inflight.read().await.get(&key) {
        return shared.clone().await;  // 复用首个请求结果
    }
    let fut = self.dispatch(server, tool, input).shared();
    self.inflight.write().await.insert(key.clone(), fut.clone());
    let result = fut.await;
    self.inflight.write().await.remove(&key);
    result
}
```

**实现状态（X-13/X-14）**：`warm_up` 遍历所有连接调 `list_all_tools` 刷新缓存的工具 schema（server 运行期间可能增删工具），单个失败记 warn 不阻塞；inflight merge 用 `futures::future::Shared` 实现，`RequestKey = (server, tool, input_hash)`，首个请求的 `Future` 被 `Shared` 包装后存入 `inflight` map，后续同 key 调用 clone shared future 复用结果。`call` 用 `Arc<RwLock>` 读锁（允许并发调用），error 转 `String` 以满足 `Shared` 的 `Clone` 约束。

`Shared` 让多个 await 同一 future，首个完成后所有等待者拿到同一结果。`input_hash` 用 `std::hash::Hasher`，避免大 input 的 clone 开销。

**健康检查**：定期 `ping` 检测连接活性：

```
每 health_check_interval（默认 30s）：
   ├─ ping 所有 Healthy 连接
   │    ├─ 响应 → 更新 last_ping，状态保持 Healthy
   │    └─ 超时（5s）→ 状态降级为 Degraded，记录 warn
   │
   └─ Degraded 连接尝试重连
        ├─ 成功 → 恢复 Healthy
        └─ 失败 → 状态 Disconnected，该 server 工具标记不可用，
                   broadcast Event::McpServerDisconnected 通知前端
```

重连采用指数退避（1s → 2s → 4s → 上限 60s），避免 server 短暂故障时频繁重连打满 CPU。Disconnected 状态下用户调用该 server 工具直接返回错误"server disconnected, retrying connection"，而非 hang。

**启动事件 channel**：预热完成/失败通过 `tokio::sync::mpsc` 通知 Runtime，Runtime 转发为 `Event::McpServerReady`/`Event::McpServerFailed` 广播，前端据此更新 MCP server 状态指示器（如 TUI 状态栏显示 `github: ✓` / `slack: ⏳` / `db: ✗`）。

```rust
pub enum McpPoolEvent {
    Ready { server: ServerId, tool_count: usize },
    Failed { server: ServerId, error: McpError },
    Disconnected { server: ServerId },
    Reconnected { server: ServerId },
}
```

事件经 Runtime 转发为 `Event` 总线事件（§11），保持单一事件源。Runtime 自身也订阅这些事件以更新 `ToolRegistry`（server Ready 时注册其工具，Disconnected 时移除）。

### 19.6 生命周期

```
Runtime 启动
   │
   ▼
加载 MCP 配置（local + project[已批准] + user）
   │
   ▼
并发启动各 MCP server（startup_timeout 内握手）
   ├─ required server 启动失败 → Runtime 拒绝启动
   └─ 非 required 失败 → warn，跳过该 server 的工具
   │
   ▼
list_tools → 注册为 mcp__<server>__<tool> 进 ToolRegistry
   │
   ▼
（Agent 循环中）调用 MCP 工具 → tool_timeout 内等待响应
   │
   ▼
Runtime 关闭 → 优雅关闭各 MCP server（stdio: EOF；http: 连接池释放）
```

### 19.7 工具检索（Tool Search，阶段 6+）

当配置的 MCP server 提供数百个工具时，全量注入 LLM 的 `tools` 数组会吃掉大量 token 并降低模型选择准确度。参考 CC/Codex 的 Tool Search：MCP 工具延迟注册，仅当模型用 `tool.search` 工具检索到相关工具时才动态加入 `tools` 数组。索引基于 BM25 或 embedding（阶段 7+）。

### 19.8 安全约束（与 `rules.md`/`security.md` 协同）

| 约束 | 说明 |
|------|------|
| 凭证隔离 | MCP server 子进程不继承 minicoding 的凭证环境变量（同 `shell.run`，见 `security.md` §6） |
| 路径校验 | MCP 工具若返回路径并触发本地 `fs` 操作，仍经 `sandbox_path` |
| 审计 | MCP 工具调用落 `audit.log`，标注 `tool=mcp__<server>__<tool>`、`mcp_server=<server>` |
| 网络白名单 | `http` 传输的 MCP server 受 `tools.web.allowed_domains` 约束（防 SSRF） |
| L0 不可覆盖 | MCP 工具的 `side_effect` 声明不影响内置黑名单——即使 MCP server 声明"只读"，若其触发的下游 `fs.write` 命中黑名单仍 Deny |
| project 作用域批准 | 防"clone 即执行"攻击 |

---

## 20. Hooks 集成（概览，详见 hooks.md）

Hooks 系统的完整设计见 `hooks.md`，本节仅说明它与本文件各子系统的集成点，避免设计冲突。

### 20.1 Hook 与权限的关系（关键）

参考 CC 的 Hook 设计，但本项目把"L0 不可覆盖"作为硬约束（`rules.md` C-02）：

```
LLM 产出 ToolCall
   │
   ▼
policy.check(tool, input) ──▶ Verdict
   │
   ├─ Deny(内置黑名单) ──▶ 直接拒绝，Hook 也无法翻案（L0 不可覆盖）
   ├─ Allow / Deny(用户规则) / Ask
   │
   ▼
PreToolUse Hook（matcher 命中时）
   │  可：deny / allow（覆盖 Ask→Allow）/ modify_input / continue
   ▼
若仍 Ask → PermissionRequest Hook
   │  可：直接给 Decision（自动批准/阻断），跳过 Prompter
   ▼
若仍 Ask → Prompter 交互（§9.2）
```

要点：Hook 可影响 `Verdict`（批准/拒绝/改写），但**不能**覆盖内置安全黑名单——这是与 CC 的关键差异（CC 依赖 Hook 自觉，本项目用 Rust 代码强制，更强）。

### 20.2 与各子系统的集成点

| 子系统 | 集成点 |
|--------|--------|
| §2 Agent 循环 | `UserPromptSubmit` 在用户提交后、LLM 调用前运行，可拒绝/追加上下文；`Stop` 在一轮结束时触发，可要求继续（含 asyncRewake） |
| §2.3 工具执行 | `PreToolUse` 在 `policy.check` 后、`dispatch` 前运行，可改写 `input`；`PostToolUse` 在工具成功后、结果回灌前运行，可改写 `result`（含 asyncRewake）；`PostToolUseFailure` 在工具失败后、错误回灌前运行，可改写 `error`（含 asyncRewake） |
| §3 上下文压缩 | `PreCompact` 在 4 级管道启动前触发，可注入"必须保留"指令影响权重；`PostCompact` 在压缩完成后触发，可验证压缩质量并补充注入丢失的关键上下文 |
| §8 记忆 | `SessionStart` 注入长期记忆之外的动态信息（如 `git status`） |
| §8.6 AGENTS.md | `SessionStart` Hook 注入的动态信息与静态 AGENTS.md 互补 |
| §9 权限 | `PermissionRequest` Hook 可短路 `Prompter`；`PreToolUse` 可 `Ask→Allow` |
| §11 事件总线 | Hook 触发同步发 `Event::HookRun { name, event, decision, elapsed }` 供观测 |
| §15 OTel | Hook 执行打 `hook.run` span，属性 `hook.name`/`hook.event`/`hook.decision` |
| §7 子 Agent | `SubagentStop` Hook 可校验产出，决定是否要求父 Agent 重试 |

### 20.3 安全约束（与 `rules.md` 协同）

- L0 不可覆盖：Hook 的 `allow` 对内置黑名单 `Deny` 无效；
- `ScriptHook` 子进程不继承凭证环境变量（同 `shell.run`）；
- Hook 超时 kill，按 `on_hook_error`（`continue`/`deny`/`fail`）处理；
- `modify_input` 仍经 `sandbox_path`，Hook 不能借此越界；
- Hook 的 `allow`/`deny`/`modify_input` 全部落 `audit.log`，标注 `source=hook:<name>`；
- `inject_context` 内容包裹 `<hook_context>` 边界，声明非指令。

详细 Hook 事件类型（10 类）、协议、Rust API、配置见 `hooks.md`。

---

## 21. 子系统依赖方向与独立测试策略

§16-§20 五个子系统（Plan/Journal/Task/MCP/Hooks）在设计层面互操作频繁，乍看存在"循环依赖链"。本节明确：**设计层互操作 ≠ 实现层循环依赖**。通过 trait 解耦与测试替身，任何子系统都能在所在里程碑内独立完成单测与集成测试，无需等待下游子系统实现。

### 21.1 设计层互操作 vs 实现层依赖

设计层互操作是子系统在 Runtime 编排下的运行时协作（如 Plan 批准后调用 Task 工具、Journal 记录 Plan 执行期的文件改动）——这些协作发生在 Runtime 内部，**不要求实现 crate 之间互相 import**。

实现层依赖遵循 `modules.md` §0.2 的单向不循环原则：

```
                     core (trait 定义)
                       ▲
            ┌──────────┼──────────┬──────────┬──────────┐
            │          │          │          │          │
        journal    memory     policy      hooks        mcp
            │                     │          │          │
            └──────────┬──────────┴──────────┴──────────┘
                       │
                     tools (组合层，唯一可跨领域)
                       │
                  cli / tui / sdk
```

- `minicoding-journal` 只依赖 `minicoding-core`（实现 `Journal` trait），不依赖 plan/task/hooks crate；
- `minicoding-hooks` 只依赖 `minicoding-core`（实现 `Hook`/`HookRegistry` trait），不依赖 plan/task/journal crate；
- `minicoding-mcp` 只依赖 `minicoding-core` + `rmcp`，不依赖 plan/task/journal/hooks crate；
- Plan 与 Task 是**工具**（在 `minicoding-tools`），不是独立 crate——它们的"依赖"是工具实现层调用 `Journal`/`TaskRegistry` 的 trait 对象（`Arc<dyn Journal>`/`Arc<dyn TaskRegistry>`），由 Runtime 注入；
- Runtime 在 `minicoding-core`，持有所有 trait 对象，编排协作，本身不含领域算法。

因此 §16.6/§17.6/§18.9/§19.8/§20.2 列出的"与既有抽象的关系"都是 **Runtime 编排关系**，不是 crate 间 `use` 依赖。`cargo tree` 不会出现循环。

### 21.2 子系统真实依赖方向（DAG）

把设计层互操作按"谁调用谁"画出，得到一个有向无环图（DAG）：

```
                ┌─────────┐
                │  Hooks  │ ── 拦截一切（运行时插入点，不持反向引用）
                └────┬────┘
                     │ observe
        ┌────────────┼────────────┐
        ▼            ▼            ▼
   ┌─────────┐  ┌─────────┐  ┌─────────┐
   │  Plan   │─▶│  Task   │  │   MCP   │
   └────┬────┘  └────┬────┘  └─────────┘
        │            │            ▲
        │ uses       │ uses       │ tool reg
        ▼            ▼            │
   ┌─────────────────────┐        │
   │      Journal        │ ◀──────┘
   └─────────────────────┘  (fs tools record to journal)
```

- **Hooks** 是观察者，通过 `HookRegistry` 在 Runtime 钩子点被调用，不反向持有 Plan/Task/Journal；
- **Plan** 在执行期使用 Task（分解步骤）与 Journal（记录文件改动），是单向调用方；
- **Task** 可选用 Journal（"撤销到某 task 开始前"，§18.9 后续增强），当前实现不依赖；
- **MCP** 与 Plan/Task/Journal 无直接关系，只通过 `ToolRegistry` 注册工具，工具执行时若产生文件改动同样进 Journal（fs 层统一接入）；
- **Journal** 是叶子节点，不调用任何其他子系统。

图中**不存在环**。设计文档早先"Plan ↔ Task""Task ↔ Journal"的"双向"措辞是语义互操作（如"Plan 用 Task，Task 也可被非 Plan 场景使用"），并非实现层循环调用。

### 21.3 独立测试策略

每个子系统在所在里程碑内可独立测试，方法是**用 core trait 的 stub 替身注入 Runtime**。stub 放在 `crates/minicoding-core/tests/common/` 共享。

| 里程碑 | 子系统 | 待测 trait | stub 替身 | 测试场景 |
|--------|--------|-----------|-----------|---------|
| M4 | Journal | `Journal` | 不需要（叶子节点，纯内存数据结构） | 直接构造 `ChangeEntry` 调 `record/undo/diff`；用 `tempfile` 真实写文件验证 `before/after` 比对与 `failed_files` 检测；`/undo` CLI 用 `assert_cmd` 跑 |
| M4 | MCP | `McpClient` | `NoopMcpClient`（core 兜底） | 用本地 mock stdio process（一个 echo server 脚本）验证工具注册与调用；权限规则用 `policy.toml` 通配匹配测；project 作用域批准用临时 `.minicoding/mcp.json` 测 |
| M5 | Hooks | `Hook`/`HookRegistry` | `NoopHookRegistry`（core 兜底，所有事件 `Continue`） | `HookRegistryImpl` 单测：串行聚合、matcher glob、`modify_input` 链式传递；`ScriptHook` 用 echo 脚本测退出码映射；L0 不覆盖用内置黑名单 fixture 测 |
| M5 | Plan | `PermissionPolicy`（Plan 硬门） | `StubJournal`（record/undo 计数不真写）、`StubTaskRegistry` | Plan 硬门：`PermissionMode::Plan` 下 `fs.write` 直接 `Deny`；`ExitPlanMode` 工具调用后 `allowed_prompts` 注入 `StubPermissionPolicy` 验证命中；plan.md 读写用 `tempfile` |
| M5 | Task | `TaskRegistry`（在 core 定义） | 不需要（独立数据结构，无外部依赖） | CRUD/依赖图 DFS 成环检测/`InProgress` 唯一性/`Completed` 必填 `summary` 等校验规则单测；持久化用 `tempfile` 写 JSONL 验证 round-trip |

**关键约束**：stub 替身必须实现完整 trait 契约（不偷懒返回 `unimplemented!`），否则测试无意义。stub 放 `core/tests/common/` 而非各自 crate，避免领域 crate 互相引用测试代码破坏隔离。

### 21.4 集成测试的分层递进

单测保证各子系统独立正确，集成测试验证它们在 Runtime 编排下的协作。集成测试按里程碑分层递进，**后置里程碑的集成测试才组合前置子系统**：

| 集成测试 | 里程碑 | 组合的子系统 | 验证点 |
|---------|--------|-------------|--------|
| `tests/journal_undo.rs` | M4 | Journal + fs tools | `fs.write` → `Journal::record` → `/undo` 恢复 |
| `tests/mcp_tool_dispatch.rs` | M4 | MCP + ToolRegistry + Permission | 远程工具注册、权限规则、`audit.log` 落盘 |
| `tests/hook_pretooluse.rs` | M5 | Hooks + Permission + fs tools | `PreToolUse` 改写 input、`Ask→Allow`、L0 不覆盖 |
| `tests/plan_lifecycle.rs` | M5 | Plan + Permission + Task(stub) + Journal(stub) | Plan 硬门、`ExitPlanMode`、`allowed_prompts` 注入 |
| `tests/task_dependency.rs` | M5 | Task + EventBus | 依赖图、`Event::TaskUpdated` 广播、遗忘提醒 |
| `tests/full_turn_e2e.rs` | M6 | 全部 | 一个真实 turn：LLM mock → Plan → Task → fs.write(Journal) → Hook PostToolUse → Stop |

M4 测 Journal 时**不需要** Plan：直接在测试代码里手工构造 `ChangeEntry`（如 `ChangeEntry::new(prompt="test", files=vec![FileChange::Written{...}])`）调 `record/undo`，模拟"一个 turn 的文件改动"。`fs.write` 工具在 M2 已实现并接入 `Journal::record` 钩子点（T-M2-3 预留），M4 只是补齐 `Journal` 实现并启用 `file_undo` 特性门控——无需 Plan 参与。

M5 测 Plan 时**不需要**真实的 Task/Journal 实现：Plan 工具的 `ExitPlanMode` 逻辑只验证"plan.md 落盘 + `allowed_prompts` 注入 PermissionPolicy + 模式切回 Default"，这三个动作的协作方都用 stub 替身。真实的 Plan→Task→Journal 协作在 M6 `full_turn_e2e.rs` 验证。

### 21.5 文档措辞修正

为避免"循环依赖"误解，§16.6/§17.6/§18.9/§20.2 中"与既有抽象的关系"统一表述为"**Runtime 编排关系**"（非 crate 间依赖）。本节 §21 是该措辞的权威解释，后续若新增子系统协作描述应参照本节区分"运行时编排"与"实现层依赖"。

---

## 22. Prompt 管道详细设计

§2.2 主循环的 `build_chat_request` 把 system prompt 拼装为单个字符串传给 LLM。当 system prompt 来源增多（身份、系统规则、任务指南、环境信息、用户规则、项目规则、工具摘要、扩展注入），单字符串拼接难以维护、无法利用 prompt cache、也无法让扩展注入片段。本节定义可组合的 Prompt 管道。

**设计目标**：(1) 可组合——每个来源是一个 `PromptContributor`，独立实现；(2) 可扩展——第三方扩展可注册新 contributor；(3) prompt cache 友好——稳定段排前，易变段排后，配合 LLM provider 的前缀缓存。

**9 个 PromptContributor 按固定顺序拼接**：

| 顺序 | Contributor | 来源 | 稳定性 | cacheable |
|:---:|------|------|--------|:---:|
| 1 | Identity | `~/.minicoding/IDENTITY.md`（可覆盖默认身份） | 极稳定 | ✓ |
| 2 | System | 内置系统规则（`rules.md` §5 软规则） | 极稳定 | ✓ |
| 3 | TaskGuidelines | 任务指南（多步任务规划、工具使用规范） | 稳定 | ✓ |
| 4 | Communication | 通信规范（输出格式、语言偏好） | 稳定 | ✓ |
| 5 | Environment | 工作区/平台/git 信息（cwd、OS、git branch） | 会话内稳定 | ✓ |
| 6 | UserRules | 用户规则（来自 `long_term.md`，§8.2） | 跨会话变化 | ✗ |
| 7 | ProjectRules | 项目规则（来自 AGENTS.md，§8.6） | 仓库内稳定 | ✗ |
| 8 | ToolSummary | 工具 schema 摘要（含 MCP 工具） | 工具集变化时变 | ✗ |
| 9 | Extension | 扩展注入（通过 `Registrar::register_prompt_contributor`，§23） | 动态 | ✗ |

稳定段（1-5）排前，易变段（6-9）排后。LLM provider 的 prompt cache 以前缀匹配——稳定段命中缓存，易变段仅追加增量。把 Environment 排在稳定段是因为它"会话内不变"，cache 在单会话内有效；UserRules/ProjectRules 排后是因为它们跨会话变化，cache 命中率低。

**PromptContributor trait**（与 `api.md` §3.13 一致）：

```rust
pub trait PromptContributor: Send + Sync {
    /// Contributor 唯一标识（如 "identity"、"project_rules"），用于调试与 OTel span。
    fn name(&self) -> &str;

    /// 拼装顺序（枚举固定 9 段，稳定段在前利于 prompt cache）。
    fn order(&self) -> PromptSectionOrder;

    /// 该段是否可缓存（影响 prompt cache 命中率统计）。默认 false。
    fn cacheable(&self) -> bool { false }

    /// 生成该段内容。ctx 提供会话级信息（workdir、git 状态、工具集等）。
    /// 异步方法用 BoxFuture 返回类型保证 dyn 兼容。
    fn build(&self, ctx: &PromptContext) -> BoxFuture<'_, Result<PromptSection, PromptError>>;
}

/// Section 排序（稳定→易变，与 9 段表格一一对应）。
pub enum PromptSectionOrder {
    Identity,       // 1
    System,         // 2
    TaskGuidelines, // 3
    Communication,  // 4
    Environment,    // 5
    UserRules,      // 6
    ProjectRules,   // 7
    ToolSummary,    // 8
    Extension,      // 9（扩展通过 Registrar 注册的 contributor 注入）
}

pub struct PromptContext {
    pub session_id: SessionId,
    pub workdir: Utf8PathBuf,
    pub platform: Platform,
    pub git_info: Option<GitInfo>,
    pub enabled_tools: Vec<ToolSchema>,
    pub user_rules: MemoryBlock,       // 来自 long_term.md
    pub project_rules: ProjectDoc,     // 来自 AGENTS.md
}
```

`PromptContext` 把各 contributor 需要的输入聚合，避免 contributor 各自重新加载文件（如 ProjectRules 不再自己读 AGENTS.md，从 ctx 取）。`order()` 返回 `PromptSectionOrder` 枚举，使拼接顺序不依赖注册顺序，且类型安全——扩展只能通过 `Extension` 段（顺序 9）注入，无法抢占内置段位置。

**PromptSection 数据结构**：

```rust
pub struct PromptSection {
    pub contributor_name: String,
    pub content: String,
    pub order: PromptSectionOrder,
    pub cacheable: bool,
    pub boundary: Option<&'static str>,  // 如 "project_doc"、"auto_memory"，包裹边界
}
```

`boundary` 字段让段内容包裹在 `<{boundary}>...</{boundary}>` 内（如 §8.6 的 `<project_doc>`、§8.7 的 `<auto_memory>`），声明内容性质供 LLM 区分指令与上下文。无边界的段（如 Identity/System）直接拼接。

**PromptPipeline 拼装**：

```rust
pub struct PromptPipeline {
    contributors: Vec<Arc<dyn PromptContributor>>,
}

impl PromptPipeline {
    pub async fn build(&self, ctx: &PromptContext) -> Result<SystemPrompt> {
        let mut sections: Vec<PromptSection> = Vec::new();
        for c in &self.contributors {
            sections.push(c.build(ctx).await?);
        }
        // 按 PromptSectionOrder 枚举定义的固定顺序排序（同 order 内按 contributor name 稳定排序）。
        sections.sort_by_key(|s| (s.order as u8, s.contributor_name.clone()));

        let mut buf = String::new();
        for s in &sections {
            if let Some(b) = s.boundary {
                buf.push_str(&format!("<{b}>\n{}\n</{b}>\n\n", s.content));
            } else {
                buf.push_str(&s.content);
                buf.push_str("\n\n");
            }
        }
        Ok(SystemPrompt { text: buf, sections })
    }
}
```

`sections` 字段保留分段信息，供 OTel span 记录每段 token 数（`prompt.section.{name}.tokens`），便于诊断 prompt 膨胀来源。`sort_by_key` 保证即使 contributor 注册顺序变化，最终拼接顺序仍由 `PromptSectionOrder` 枚举决定；同段内（如多个扩展注入到 `Extension` 段）按 `contributor_name` 稳定排序，避免非确定性。

**扩展通过 Registrar 注入**：第 9 段 Extension 由内置 `ExtensionContributor`（占位，返回空 section）与扩展通过 `Registrar::register_prompt_contributor`（§23）注册的 contributor 共同组成。同 order（9）内按 `contributor_name` 稳定排序，避免非确定性。扩展注册的 contributor 默认 `cacheable = false`（动态内容不应缓存）。这给扩展系统（§23）提供了注入 prompt 的统一入口——扩展实现 `PromptContributor` trait 并通过 Registrar 注册，无需修改核心 contributor 列表。

---

## 23. 扩展系统详细设计

§22 的 Prompt 管道、§20 的 Hooks、§19 的 MCP 已分别提供 prompt 注入、生命周期钩子、外部工具接入能力。但当用户想"加一个自定义斜杠命令""加一个 TUI 状态栏项""加一个快捷键绑定"时，缺少统一载体。扩展系统把这些能力收敛为单一抽象，实现 Extension-First 架构——核心能力可插件化，未启用的扩展零开销。

**设计目标**：(1) Extension-First——核心功能（如 task 管理、plan 模式）本身可作为扩展实现，验证扩展抽象的完备性；(2) 三类载体统一 API——无论进程内、IPC 子进程、MCP 远程，对 Runtime 暴露同一接口；(3) 安全收敛——扩展注册的能力仍受 L0 约束（C-01/C-02）。

**三类扩展载体**：

| 载体 | 部署 | 通信 | 性能 | 用途 |
|------|------|------|------|------|
| 进程内 first-party | 在 `minicoding-extension-sdk` 实现组合根 | 直接函数调用 | 零序列化 | 核心扩展（task/plan/journal 集成层） |
| disk IPC 子进程 | 磁盘上的可执行文件 | JSON over stdio | 序列化开销 | 第三方本地扩展（语言无关） |
| MCP 远程扩展 | 通过 `minicoding-mcp` 包装 | MCP 协议 | 网络开销 | 远程能力接入（与 §19 MCP 复用） |

三类载体统一实现 `Extension` trait，Runtime 不感知载体差异。MCP 远程扩展复用 §19 的 MCP client，只是把其工具/Hook 包装为扩展接口。

**ExtensionHost trait**（Runtime 注入，管理扩展生命周期；与 `api.md` §3.12 一致）：

```rust
pub trait ExtensionHost: Send + Sync {
    /// 加载扩展（读 manifest，初始化，注册能力）。
    fn load_extension(&self, manifest: ExtensionManifest)
        -> BoxFuture<'_, Result<ExtensionId, ExtensionError>>;

    /// 卸载扩展（调用 shutdown，注销所有注册项）。
    fn unload_extension(&self, id: &ExtensionId)
        -> BoxFuture<'_, Result<(), ExtensionError>>;

    /// 列出已加载扩展。
    fn list_extensions(&self) -> BoxFuture<'_, Vec<ExtensionInfo>>;

    /// 配置变更通知（热重载）。
    fn on_config_changed(&self, id: &ExtensionId, new_config: serde_json::Value)
        -> BoxFuture<'_, Result<(), ExtensionError>>;
}
```

`ExtensionHost` trait 由 `minicoding-core` 定义，实现在 `minicoding-extension-sdk`（进程内 first-party 组合根 `BundledExtensionHost`）或 `minicoding-cli`（disk IPC 子进程加载器）。`NoopExtensionHost`（`minicoding-core`）为未启用扩展时的兜底实现。Runtime 持有 `Arc<dyn ExtensionHost>`（通过 `RuntimeBuilder::extension_host` 注入），在启动时批量加载 `~/.minicoding/extensions/` 下的扩展。

**Extension trait**（扩展实现侧；与 `api.md` §3.12 一致）：

```rust
pub trait Extension: Send + Sync {
    /// 扩展元信息（供 Runtime 查询，无需扩展自行管理状态）。
    fn manifest(&self) -> &ExtensionManifest;

    /// 扩展初始化（注册能力、订阅事件、读配置）。
    fn init(&self, registrar: &mut dyn Registrar, config: serde_json::Value)
        -> BoxFuture<'_, Result<(), ExtensionError>>;

    /// 扩展卸载（释放资源、取消订阅）。
    fn shutdown(&self) -> BoxFuture<'_, Result<(), ExtensionError>>;

    /// 配置变更通知（可选，默认空实现）。
    fn on_config_changed(&self, _new_config: serde_json::Value)
        -> BoxFuture<'_, Result<(), ExtensionError>> {
        Box::pin(async move { Ok(()) })
    }
}
```

`init` 接收 `&mut dyn Registrar`，扩展通过它注册自己提供的能力。`manifest()` 让 Runtime 无需维护独立的扩展元信息表，直接从扩展实例查询。`shutdown` 由 `ExtensionHost::unload_extension` 调用，保证资源清理。

**Registrar 接口**（扩展注册能力的统一入口；与 `api.md` §3.12 一致）：

```rust
pub trait Registrar {
    fn register_tool(&mut self, tool: Arc<dyn Tool>) -> Result<(), ExtensionError>;
    fn register_hook(&mut self, hook: Arc<dyn Hook>) -> Result<(), ExtensionError>;
    fn register_prompt_contributor(&mut self, c: Arc<dyn PromptContributor>) -> Result<(), ExtensionError>;
    fn register_keybinding(&mut self, kb: KeyBinding) -> Result<(), ExtensionError>;
    fn register_status_item(&mut self, item: StatusItem) -> Result<(), ExtensionError>;
    fn register_command(&mut self, cmd: SlashCommand) -> Result<(), ExtensionError>;
}
```

`Registrar` 把"扩展能做什么"收敛为 6 类注册项。每类注册项最终进入 Runtime 对应的注册表（`ToolRegistry`/`HookRegistry`/`PromptPipeline`/`KeyBindingMap`/`StatusBarRegistry`/`CommandRegistry`），扩展无需直接接触 Runtime 内部结构。注册项统一用 `Arc<dyn Trait>`（而非 `Box`），便于 Runtime 在多扩展间共享实例。

**ExtensionManifest**：

```rust
pub struct ExtensionManifest {
    pub id: String,                  // 全局唯一，如 "minicoding-git-stats"
    pub version: semver::Version,
    pub name: String,
    pub author: Option<String>,
    pub carrier: ExtensionCarrier,   // Bundled / Ipc { path } / Mcp { server_id }
    pub capabilities: Vec<Capability>,  // ["tool", "hook", "prompt", "command", ...]
    pub permissions: Vec<Permission>,   // ["fs.read", "shell.run:git *"]
    pub config_schema: Option<serde_json::Value>,  // JSON Schema for config
}

pub enum ExtensionCarrier {
    Bundled,                        // 进程内，name 查找符号
    Ipc { path: Utf8PathBuf },      // 可执行文件路径
    Mcp { server_id: String },      // 复用 §19 MCP server
}

pub enum Capability { Tool, Hook, PromptContributor, Keybinding, StatusItem, Command }
```

`permissions` 字段声明扩展需要的权限范围，加载时 `PermissionPolicy` 校验——扩展注册的工具调用时仍走 §9 权限流程，`permissions` 只用于"扩展加载时的静态检查"（如声明了 `fs.read` 但实际调用 `fs.write` 会被运行时权限拦截）。

**安全约束**：

- **扩展工具仍经 `PermissionPolicy::check`**（C-01）：扩展注册的 `Tool` 与内置工具走同一 `run_one` 闭环（§2.3），`side_effect != None` 仍触发权限决策。扩展无法绕过权限。
- **不可绕过黑名单**（C-02）：即使扩展注册的工具名不在内置黑名单，若其内部触发 `shell.run` 命中黑名单前缀（如 `rm -rf /`），黑名单仍 Deny。扩展是"用户态能力"，不提升特权。
- **IPC/MCP 扩展的输入校验**：disk IPC 与 MCP 扩展的输入/输出经 `serde_json::Value` 边界，Runtime 在 `dispatch` 前对路径类参数走 `sandbox_path`（C-03），防扩展越界读写。
- **凭证隔离**：IPC 子进程不继承 minicoding 凭证环境变量（同 `shell.run`，C-04），扩展需显式申请 `permissions` 含凭证访问才注入。

**统一 dispatch**：扩展注册的工具走 `ToolRegistry::dispatch`，与内置工具共享 §15 的 OTel `tool.call` span、§11 的 `Event::ToolCallStart/End`、§9.5 的权限规则。这保证扩展工具的可观测性、可审计性与内置工具一致，不存在"扩展工具绕过监控"的暗角。

---

## 24. 前后端协议与多前端接入

§1 的 Runtime 是单进程内聚合根，CLI/TUI/SDK 直接持有 `Arc<Runtime>` 调用方法。但当需要"远程前端"（如浏览器、移动端、IDE 插件通过 HTTP 接入）或"嵌入式前端"（如 Zed 编辑器通过 ACP 协议嵌入、VS Code/Neovim 通过 LSP 协议嵌入）时，进程内调用不够。本节定义前后端协议，使 Runtime 可被多前端共享。

**JSON-RPC 2.0 协议**：前后端通信统一用 JSON-RPC 2.0，wire types 定义在 `minicoding-protocol` crate（新增）。请求/响应/通知三类消息，与 LSP 协议风格一致，便于复用既有 LSP 客户端库。

```rust
pub enum Request {
    CreateSession { config: SessionConfig },
    SendUserMessage { session_id: SessionId, text: String, attachments: Vec<Attachment> },
    Cancel { session_id: SessionId },
    Undo { session_id: SessionId, steps: usize },
    ListSessions { filter: Option<SessionFilter> },
    GetSession { session_id: SessionId },
    SetPermissionMode { session_id: SessionId, mode: PermissionMode },
    ResolvePermission { id: String, decision: Decision },
}

pub enum Response {
    Ok(serde_json::Value),
    Err(RpcError),
}
```

`Request`/`Response` 与 `api.md` 的 Runtime 方法一一对应，protocol crate 仅做序列化适配，不含业务逻辑。

**Event DTO 携带 seq: u64**：§11 的 `Event` 枚举序列化为 DTO 时附加 `seq: u64`（单调递增，每会话独立计数）。前端通过 SSE 订阅事件流，记录最后接收的 `seq`，断线重连时用 `seq` 恢复：

```
SSE cursor 恢复流程：

Client                          Server
  │                               │
  │── Subscribe(session_id,       │
  │            last_seq=42) ────▶│
  │                               │
  │                       检查 last_seq：
  │                       ├─ seq 仍在内存 ring buffer → 从 seq+1 重放
  │                       ├─ seq 已 evict 但 ≤ durable_seq
  │                       │    → 从 EventStore 重放 seq+1..now
  │                       └─ seq > durable_seq 但 < now
  │                            → 不可恢复，发 RehydrateRequired
  │                               │
  │◀── Event(seq=43) ────────────│
  │◀── Event(seq=44) ────────────│
  │      ...                      │
  │◀── Event(seq=now) ───────────│
```

`seq` 单调递增保证事件顺序；`durable_seq` 是已持久化到 EventStore 的最大 seq（见 §25）。ring buffer 是进程内最近 N 条事件的内存缓存，命中时零 IO 重放。

**RehydrateRequired 信号**：当 broadcast channel 溢出（前端消费慢于生产）且事件已从 ring buffer evict 时，Server 发 `RehydrateRequired` 通知客户端"事件流已不完整，请重拉 snapshot"。客户端收到后调用 `GetSession` 拉取当前完整状态（messages、tasks、permission mode 等），重建本地视图。这是"最终一致"而非"强一致"——短暂的事件丢失通过 snapshot 重拉补齐。

**HTTP/SSE server（minicoding-server）**：`minicoding serve` 子命令启动 HTTP server，多会话并发，path 带 `session_id`：

```
POST   /sessions                          → CreateSession
POST   /sessions/{id}/messages            → SendUserMessage
POST   /sessions/{id}/cancel              → Cancel
POST   /sessions/{id}/undo                → Undo
GET    /sessions                          → ListSessions
GET    /sessions/{id}                     → GetSession
GET    /sessions/{id}/events              → SSE 事件流（带 Last-Event-ID header 恢复）
POST   /sessions/{id}/permissions/{pid}   → ResolvePermission
```

SSE 用 `Last-Event-ID` HTTP header 传递 `seq`，与标准 SSE 重连机制兼容（浏览器 `EventSource` 自动重连时携带）。HTTP server 实现在 `minicoding-server` crate（新增），依赖 `minicoding-tools` + `tokio` + `axum`（或 `hyper`，选型见 `tech-stack.md`）。

**HRTB 兼容设计（`run_turn_owned` + `send_message_boxed`）**：axum 的 `Handler` trait 要求 future 为 `'static` 且满足 HRTB（higher-ranked trait bound，`for<'a> Fn(&'a mut Request) -> Future<Output = Response> + 'a`）。但 `Runtime::run_turn(&self) -> impl Future + '_` 的 future 借用 `&self`，生命周期参数 `'_` 泄漏到外层 future 类型——当 handler `.await` 该 future 时，编译器报 "implementation of `FnOnce` is not general enough"（future 类型对每个具体的 `'a` 不同，不满足 HRTB "对任意 `'a` 类型相同"的要求）。

解决方案是把借用转为 owned，让 future 自包含 `'static`：

1. **`Runtime::run_turn_owned(self: Arc<Self>, input) -> BoxFuture<'static, Result<TurnOutcome>>`**（见 `api.md` §4.1）：`Arc<Runtime>` owned 移入 `async move` 块，`run_turn(&*self)` 的 `&self` 借用是块内局部借用（await 后即释放），`Box::pin` 装箱为 `BoxFuture<'static>`。`SessionManager::send_message_boxed`（关联函数，取 `Arc<SessionManager>` owned）同理——future 无 `&self` 借用泄漏。

2. **`SessionManager` 同步方法**：`create_session`/`cancel`/`get`/`list_sessions`/`delete` 改为同步方法（非 `async fn(&self, ..)`）。这些方法内部仅操作 `std::sync::Mutex<HashMap>`（纯数据查/增/删，无 IO），同步实现消除 future 生命周期参数。`send_message_boxed`/`resolve_permission`/`get_messages` 仍为 async（涉及 await），但 `send_message_boxed` 是关联函数（无 `&self` 借用），`resolve_permission`/`get_messages` 不直接作为 axum handler（在 handler 内调用，future 不泄漏到 HRTB 边界）。

3. **`ToolFuture` 装箱（`execute_tool_calls` HRTB 修复）**：Runtime 内部 `execute_tool_calls` 对只读工具并发执行时，`readonly.iter().map(|call| async move { ... })` 中 `call` 是 `&&ToolCall`（来自 `Vec<&ToolCall>::iter`），闭包签名 `fn(&'a &'b ToolCall) -> impl Future + 'a` 不满足 `buffer_unordered` 要求的 HRTB（future 类型对任意 `'a` 必须相同）。把每个 future 装箱为 `Pin<Box<dyn Future<Output = Result<(ToolCallId, ToolResult), RuntimeError>> + Send>>`（type alias `ToolFuture`），擦除生命周期参数统一类型。装箱开销相对工具执行 IO 可忽略，且只读桶最多 8 并发（`buffer_unordered(8)`）。

这三处修复共享同一模式：**把借用转为 owned（`Arc` clone）或装箱（`Box::pin`），让 future 自包含 `'static`，避免生命周期参数泄漏到 HRTB 边界**。这是 axum/tower 生态与"async fn in trait + `Arc<dyn Trait>`"架构交互时的通用解法。

**ACP stdio 适配器**：`minicoding serve --acp` 启动 ACP（Agent Client Protocol）stdio 模式，可被 Zed 等编辑器嵌入。ACP 是 stdio 上的 JSON-RPC，与 HTTP server 共享 protocol crate 的 wire types，仅传输层不同（stdio 替代 HTTP/SSE）。这使 minicoding 既能作为独立服务（HTTP），又能作为嵌入式 Agent（ACP）。

**NDJSON stdio 适配器**：`minicoding serve --ndjson` 启动 NDJSON 模式（编辑器插件驱动）：stdin 每行一个 `Command` JSON，stdout 每行一个 `EventDto` JSON（`seq` 单调递增）。命令映射：`CreateSession`→`SessionCreated`、`SendUserMessage`→流式事件尾 `TurnEnd`、`Cancel`→`TurnEnd(interrupted)`、`ListSessions`/`GetSession`/`SetPermissionMode`/`ResolvePermission` 各对应响应事件（协议细节见 `minicoding-server/src/ndjson.rs` 模块注释）。

**turn 期间继续消费输入（NDJSON/ACP 共用设计）**：`SendUserMessage`/`prompt` 的 turn 执行期间，适配器主循环阻塞于事件转发，但**必须继续读取 stdin**——否则客户端在 turn 进行中回传的 `ResolvePermission`/`resolvePermission`/`cancel` 会躺在 stdin 缓冲区无人消费，权限交互死锁（turn 等待决策、决策命令无人读）。实现：读 stdin 的 task 与命令消费循环分离（mpsc channel），事件转发 `select!` 增加命令接收分支；turn 期间收到其他命令回 `command_error`/`method_not_found`（NDJSON）或忽略（ACP 通知）。

**LSP stdio 适配器**：`minicoding serve --lsp` 启动 LSP（Language Server Protocol）server 模式，可被任何支持 LSP 的编辑器（VS Code、Neovim、Emacs、Helix 等）嵌入。LSP 同样是 JSON-RPC 2.0 over stdio，与 ACP 共享 `minicoding-protocol` 的 wire types，但 LSP 拥有标准化的方法集（`textDocument/*`/`workspace/*`/`window/*`），需要把 minicoding 的对话/工具/权限能力映射到 LSP 标准方法。LSP 比 ACP 更通用（几乎所有现代编辑器原生支持），是 IDE 集成的主推路径。

LSP 语义映射（minicoding 能力 → LSP 方法）：

| minicoding 能力 | LSP 方法 | 方向 | 说明 |
|-----------------|---------|------|------|
| 会话初始化 | `initialize` | 请求-响应 | 协商能力，返回 `minicoding` 自定义 capabilities（`minicoding.ask`/`minicoding.undo` 等 command） |
| 发送 prompt | `workspace/executeCommand` | 请求-响应 | `command="minicoding.ask"`，参数 `{text, attachments}`，响应含 turn 结果 |
| 斜杠命令 | `workspace/executeCommand` | 请求-响应 | `command="minicoding.undo"`/`"minicoding.plan"`/`"minicoding.mcp"` 等 |
| 流式 token | `$/progress` | 通知 | 进度 token 关联 turn，逐 token 推送增量 |
| 工具调用进度 | `$/progress` | 通知 | kind=`tool_call`，含工具名与状态 |
| 事件广播 | `minicoding/event`（自定义通知） | 通知 | 推送 Event DTO（携带 `seq: u64`，与 §24 SSE cursor 机制一致） |
| 权限确认 | `window/showMessageRequest` | 请求-响应 | 点对点，对应 `PermissionPrompter`，动作 `Allow`/`Deny`/`AllowAlways` |
| 取消 turn | `$/cancelRequest` | 通知 | 取消当前 `executeCommand` 对应的 turn |
| AI 快速操作 | `textDocument/codeAction` | 请求-响应 | 提供解释/重构/修复等 quick action，选中代码后触发 |
| 诊断（可选） | `textDocument/publishDiagnostics` | 通知 | AI 发现的问题作为诊断推送到编辑器 |

**与 ACP 的关系**：ACP 是 Zed 专有协议，LSP 是行业标准。二者并列存在于 `minicoding-server`，共享三层状态模型（Durable/Process/Transport）与 `minicoding-protocol` wire types，仅传输层语义映射不同。用户按编辑器支持选择：Zed 用 `--acp`，其他 LSP 兼容编辑器用 `--lsp`。LSP 适配器实现 `LspPrompter`（实现 `PermissionPrompter` trait），通过 `window/showMessageRequest` 完成点对点权限交互，与 `TuiPrompter`/`InteractivePrompter` 同构。

**实现选型**：用 `tower-lsp` crate（主流 Rust LSP 框架，基于 tower，MIT/Apache-2.0），不自研 JSON-RPC stdio 薄封装（§3.6 不自研原则）。`tower-lsp` 依赖隔离在 `minicoding-server` crate（feature gate `lsp`，默认关闭），不污染 core/protocol。详见 `tech-stack.md` §4、§13。

**三层状态模型**：前后端分离后，状态分布在三层：

| 层 | 存储 | 生命周期 | 用途 |
|----|------|---------|------|
| Durable | `EventStore`（JSONL，§10.3） | 永久 | 事件事实层，崩溃可恢复 |
| Process | `TurnRegistry`（内存） | 进程级 | 当前活跃 turn 的中间状态（流式 delta、工具调用进度） |
| Transport | `CommandHandler`（连接级） | 连接级 | 单个前端连接的请求/响应配对、cursor 状态 |

Durable 层是事实源，Process 层是运行时中间态，Transport 层是连接级视图。三层分离使"多前端共享同一会话"成为可能——两个前端连接同一 session_id，各自维护 Transport 层 cursor，共享 Durable 与 Process 层。Process 层的 `TurnRegistry` 保证"一个 turn 同时只有一个前端驱动"，其它前端只读订阅事件。

---

## 25. 事件溯源（Event Sourcing，已实现）

§10.3 的 Parent-UUID 链与 §11 的 EventBus 已具备事件溯源雏形。本节描述已实现的"事件是事实"架构——将会话状态建模为不可变事件流（`Event` 持久化 + snapshot 重放），替代纯 JSONL 消息追加模型。收益：天然支持时间旅行调试、多客户端状态同步（与 SSE cursor 恢复/E-13 协同）、审计回放（`--replay` 不再依赖消息日志而是事件重放）、跨会话 fork/merge。

> **实现状态**：已实现。前置条件 `minicoding-protocol` wire types 稳定（M8）+ `RehydrateRequired`（E-14）信号链路验证通过均已满足。

**架构**：`EventStore` 是 append-only 事实层，事件不可变、`seq` 单调递增；`Session` 是投影层，从 EventStore + snapshot 重放得出当前消息序列。与原 JSONL 消息日志**双写并存**——新会话同时写消息日志与事件流，旧会话无事件流时回退到消息日志路径，平滑过渡。

```
EventStore（append-only，不可变，{id}.events.jsonl）
   │
   │ PersistedEvent: SessionCreated / MessageAppended /
   │                 PermissionResolved / PermissionModeChanged /
   │                 TaskUpdated / TurnEnd
   │
   ▼
replay_session_state（重放事件，构建 Session 视图）
   │
   ├── SnapshotStore（{id}.snapshot.json，每 50 条 MessageAppended 落盘一次）
   │
   ▼
Session { messages, permission_mode, audit_trail, ... }
```

Event 与 Message 的关键区别：Event 记录"发生了什么"（如 `PermissionModeChanged { from, to }`），Message 记录"当前是什么"。Event 不可变，Session 是 Event 的投影——同一 EventStore 可投影出不同视图（如"忽略权限事件"得到纯消息流，"仅权限事件"得到审计轨迹）。

### 25.1 事件流初始化（`Runtime::init_event_stream`）

`RuntimeBuilder::build()` 后、首次 `run_turn` 前，CLI/server 调用 `init_event_stream`：

- **新会话**（`EventStore::next_seq == 1` 且无 snapshot）：持久化 `PersistedEvent::SessionCreated`（携带 `workdir`/`config_hash`/`created_at`，供 `replay_session_state` 重建 `Session`），设置 `event_seq = 2`、`durable_seq = 1`；
- **恢复会话**（`--resume`/`--replay`，`next_seq > 1`）：设置 `event_seq = next_seq`；若存在 snapshot，设置 `durable_seq = snapshot.seq` 并重置 `message_since_snapshot = 0`；
- **`NoopEventStore`**（SDK 等未注入场景）：`next_seq` 恒返回 1，append 为 no-op，整个方法实际效果为 no-op。

幂等：重复调用安全，`init_event_stream` 内部按 `next_seq` 判断新/旧会话。

### 25.2 事件持久化（`Runtime::persist_event`）

`run_turn` 在 `events.emit(event)` 后调用 `persist_event`：

1. `try_persist(&event)` 过滤瞬态事件（`Token`/`TurnStreamingStarted`/`ToolCallStarted`/`ToolCallFinished`/`PermissionRequested`/`ConfigChanged`/`SessionCreated`）——瞬态事件或为流式增量（已被 `MessageAppended` 捕获）、或为通知类（无状态变更）；
2. 分配 seq（`event_seq` mutex 递增）；
3. 构造 `EventRecord`，调 `EventStore::append`（fsync 后返回，崩溃安全）；
4. 更新 `durable_seq`（供 SSE cursor 协同）；
5. `MessageAppended` 时递增 `message_since_snapshot`，达到 `SNAPSHOT_INTERVAL`（50）触发 snapshot 落盘。

**best-effort 语义**：持久化失败仅记 `warn` 日志，不中断主流程（与 audit 失败处理一致）。崩溃时磁盘状态为已持久化事件的子集，replay 可从最近 snapshot + 剩余事件重建。

`PermissionModeChanged` 事件由 `PlanControllerHandle::persist_mode_changed` 持久化（`plan.exit`/`/plan`/`--plan` 触发的模式切换），不触发 snapshot。

### 25.3 Snapshot 机制（`SnapshotStore`）

`JsonlSnapshotStore` 每会话一文件 `{id}.snapshot.json`，覆盖写（先写 `.tmp` 再 `rename`，原子）。`Runtime::create_snapshot` 从 `ContextManager::snapshot` 获取当前消息列表，构造 `SessionState` + `SessionSnapshot`，调 `SnapshotStore::save` 落盘。

snapshot 触发条件：每 `SNAPSHOT_INTERVAL`（50）条 `MessageAppended` 事件后。snapshot 失败仅记 `warn` 日志，不中断主流程（best-effort）。

`SessionSnapshot` 字段：`seq`（snapshot 对应的事件 seq）、`schema_version`（与 `SCHEMA_VERSION` 一致）、`session_id`、`state: SessionState`（`id`/`created_at`/`workdir`/`config_hash`/`messages`）。

### 25.4 事件重放（`replay_session_state`）

从 snapshot + 事件流重建 `Session` 状态：

1. 加载最近 `SessionSnapshot`（如有），获得初始 `SessionState`；
2. 加载 `seq > snapshot.seq` 的事件流（无 snapshot 时加载全部事件，首事件必须为 `SessionCreated`）；
3. 按事件顺序应用：
   - `SessionCreated`：初始化 `Session`（仅当无 snapshot 时）；
   - `MessageAppended`：追加到 `messages`；
   - `PermissionResolved`/`TaskUpdated`/`TurnEnd`：仅记录审计轨迹，不重建运行时状态；
   - `PermissionModeChanged`：更新 `final_permission_mode`；
4. 返回 `ReplayedSession { session, audit_trail, last_seq, final_permission_mode }`。

**seq 连续性检查**：事件 seq 必须连续（`expected = last_seq + 1`），跳跃返回 `ReplayError::SeqGap`（可能事件丢失）。

**schema 版本检查**：事件或 snapshot 的 `schema_version` 高于当前 `SCHEMA_VERSION` 返回 `ReplayError::UnsupportedSchema`（未来版本）。

**旧会话兼容**：`EventStore::load` 返回空且 `SnapshotStore::load` 返回 `None` 时，返回 `MissingSessionCreated`——调用方应捕获此错误并回退到 `Storage::load` 消息列表路径（`session_from_messages`）。

### 25.5 SSE cursor durable recovery

SSE handler 的 `Last-Event-ID` cursor 恢复三级策略（与 E-13/E-14 协同）：

1. **内存 ring buffer 命中**：`EventCursor.replay_after_with_seq(after_seq)` 直接重放 buffer 中 `seq > after_seq` 的事件；
2. **durable recovery**：`after_seq` 已 evict 但 ≤ `Runtime::durable_seq()`，调 `EventStore::load_after(after_seq)` 从持久化事件流重放。仅持久化事件子集可恢复（瞬态事件如 `Token` 不可恢复，客户端应容忍缺失——`Token` 增量已被 `MessageAppended` 捕获）；
3. **不可恢复**：`after_seq` > `durable_seq`（或 `EventStore` 为 `NoopEventStore`），发 `RehydrateRequired` 后关闭流（E-14）。

`EventKind::from_persisted` 将 `PersistedEvent` 转为 SSE payload JSON，与内存路径格式统一。

### 25.6 CLI `--replay` 集成

CLI `load_session_by_mode` 对 `Resume`/`Replay` 模式优先走事件重放路径：

1. 加载 `JsonlSnapshotStore::load_sync`（如有 snapshot）；
2. 加载 `JsonlEventStore::load_events_sync` 全部事件；
3. 调 `replay_session_state` 重建 `Session`；
4. 旧会话无事件流（`load_events_sync` 返回空且无 snapshot）→ 回退到 `load_messages` 消息日志路径。

`workdir`/`config_hash` 由 CLI 启动参数提供，覆盖事件流中的值——避免 workdir 跨机器漂移。

`Fork` 模式不走事件重放（fork 创建新会话，新会话有自己的事件流），仅从消息日志复制前缀消息。

### 25.7 schema 版本化

`SCHEMA_VERSION = 1`（当前）。`EventRecord.schema_version` 标记事件结构版本，旧版会话通过 migration 适配：

- v0（隐式）：消息日志（无 `EventRecord` 包装），`replay_session_state` 回退到消息列表；
- v1：当前 schema，`EventRecord` 显式包装 `PersistedEvent`。

未来变更（如新增事件变体、字段语义变更）递增 `SCHEMA_VERSION`，并在 `replay_session_state` 中按版本分支处理。`SessionSnapshot.schema_version` 同步演进。

### 25.8 文件布局

```
~/.minicoding/sessions/
├── {id}.jsonl              # 消息日志（原有，双写保留）
├── {id}.events.jsonl       # 事件流（新增）
└── {id}.snapshot.json      # snapshot（新增）
```

三文件同目录，便于备份/清理。`JsonlEventStore`/`JsonlSnapshotStore` 与 `JsonlStorage` 共用 `sessions_dir`。

### 25.9 优势

- **fork = 从 seq 重放**：当前 fork（§10.5）需复制前缀消息到新文件；事件溯源下，fork 仅记录"从 seq=42 分叉"，新会话从 seq=42 重放 EventStore + 后续新事件，零拷贝（未来增强）。
- **压缩边界 = Compacted 事件**：当前压缩边界靠 `parent_uuid = None` 标记；事件溯源下，`CompactionApplied` 事件显式记录"seq=42 之前已折叠为摘要"，投影器据此跳过旧事件（未来增强）。
- **side-chain = 子 Agent 事件挂在 `TaskSpawned` 事件下**：当前 side-chain 靠 `parent_uuid` 指向 `task.spawn` 工具调用；事件溯源下，子 Agent 事件的 `parent_event_id` 指向 `TaskSpawned`，天然形成事件树（未来增强）。
- **多视图**：同一 EventStore 可投影出"完整历史"/"当前窗口"/"审计视图"，无需多份存储。
- **审计回放**：`--replay` 不再依赖消息日志，而是事件重放——权限决策、模式切换等审计轨迹完整保留。

**与 §24 前后端协议的协作**：§24 的 `seq` cursor 恢复在事件溯源下天然实现——EventStore 本身是 seq 单调递增的事件流，SSE cursor 直接对应 EventStore seq，`durable_seq` 即 EventStore 的持久化进度。

### 25.10 服务端会话持久化与懒恢复（M9）

`minicoding-server` 的 `SessionManager` 持有**内存会话表**（`HashMap<SessionId, Arc<ServerSession>>`），但消息/事件已由 `build_runtime` 内部构造的 `JsonlStorage`/`JsonlEventStore` 落盘 `~/.minicoding/sessions/`。重启后内存表清空，需从磁盘恢复：

```
list_sessions() ──► 磁盘 index.json meta（count/summary 实时）
                    │        （append 时 upsert，见 storage `jsonl.rs`）
                    ▼
            内存活跃会话（task_state 覆盖）+ 磁盘历史合并
                    │
get_or_load(id) ──► 内存命中？──否──► restore_session(id)
                                          │
        snapshot + events.jsonl ──► replay_session_state（§25.4）
        ──失败/旧会话──► 消息日志 {id}.jsonl 回退（session_from_messages）
                                          │
        build_runtime(params, preloaded=Some(session))  # 会话原 workdir 覆盖
        restore_history()（回填上下文）→ init_event_stream()（幂等）
        → insert_session()（内部查重，并发恢复安全）
```

要点：

- **计数/摘要以磁盘 `index.json` 为准**：`Runtime.session().messages` 是上下文快照，不随 `run_turn` 更新（消息写 storage + 广播 `MessageAppended`）；`JsonlStorage::append` 同步 upsert index（首条用户消息 80 字符截断为 `summary`），重启后侧边栏摘要/计数仍正确。
- **懒恢复**：仅在访问时恢复（HTTP/NDJSON/ACP/workspace 全入口经 `get_or_load`），避免启动时全量构造 Runtime 的成本。
- **workdir 保留会话原值**：事件流重放出的 `Session.workdir` 覆盖 `default_params.workdir`，使 sandbox/journal/文件工具与创建时一致；消息日志回退路径无 workdir 信息，用 server 默认目录（与 CLI 回退路径一致）。
- **Plan 模式入口**：`POST /sessions` 的 `plan_mode: true` 映射初始 `PermissionMode::Plan`（C-25），前端新建会话可选"先规划"。
- **事件流优先**：与 CLI `--resume` 同构（§25.4/25.6），重放失败（schema 不兼容）回退消息日志，保证旧会话始终可恢复。

---

## 26. Web 与桌面应用架构（M9，低优先级）

> **范围说明**：本节描述 M9 的 Web 前端与 Tauri 桌面应用架构。M9 是可选里程碑，优先级低于 M5–M8。核心原则：**Rust 后端不嵌入前端**，前端通过 §24 的 HTTP/SSE JSON-RPC 协议与 `minicoding-server` 通信，保证 CLI/SDK 可独立使用。技术栈选型见 `tech-stack.md` §4.1。

### 26.1 部署形态

```
┌─────────────────────────────────────────────────────────────────┐
│ 形态 A：Web 模式（浏览器）                                        │
│                                                                  │
│  Browser (React 19.2 SPA)                                        │
│     │  HTTPS  │  SSE (Event stream)                              │
│     ▼        ▼                                                   │
│  minicoding-server (--http --web ./dist --cors-origin ...)       │
│     │                                                            │
│     ▼                                                            │
│  Runtime (Agent loop)                                            │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│ 形态 B：桌面模式（Tauri 2.x）                                     │
│                                                                  │
│  Tauri Window (WebView 加载 minicoding-web dist)                 │
│     │  Tauri IPC (invoke)  │  SSE (本地 HTTP)                    │
│     ▼                      ▼                                     │
│  Rust sidecar: minicoding-server (--http --bind 127.0.0.1:PORT) │
│     │                                                            │
│     ▼                                                            │
│  Runtime (Agent loop)                                            │
│                                                                  │
│  OS 集成：系统托盘 / 全局快捷键 / 自动更新（Tauri updater）       │
│  凭证：OS keyring（与 CLI cred.rs 共享，C-04）                    │
└─────────────────────────────────────────────────────────────────┘
```

### 26.2 前端架构分层

```
minicoding-web/                       # 纯前端项目，独立 package.json
├── src/
│   ├── main.tsx                      # React 19.2 入口，React Compiler 启用
│   ├── router.tsx                    # TanStack Router 1.170 类型安全路由
│   ├── routes/
│   │   ├── __root.tsx                # 根布局（主题 provider + 全局错误边界）
│   │   ├── sessions.$sessionId.tsx   # 会话详情页（对话流 + 工具面板）
│   │   └── settings.tsx              # 设置页（沙箱策略/审批模式/凭证管理）
│   ├── api/
│   │   ├── client.ts                 # JSON-RPC 2.0 客户端（fetch + Zod 4.4 校验）
│   │   ├── sse.ts                    # SSE 订阅（EventSource + cursor 恢复）
│   │   └── schema.ts                 # 由 minicoding-protocol DTO 生成的 Zod schema
│   ├── hooks/
│   │   ├── useSession.ts             # TanStack Query 5.101 useQuery 封装
│   │   ├── useEventStream.ts         # SSE 事件 → queryClient.setQueryData 增量更新
│   │   └── usePermission.ts          # 权限弹窗状态管理（Zustand 5.0）
│   ├── components/
│   │   ├── ui/                       # shadcn/ui 组件（复制粘贴源码）
│   │   ├── chat/                     # 对话流（消息列表 + 流式 token 渲染）
│   │   ├── tools/                    # 工具调用展开/折叠面板
│   │   ├── permission/               # 权限确认 Dialog
│   │   ├── tasks/                    # 任务进度面板
│   │   └── theme/                    # 暗色/亮色主题切换（Tailwind v4）
│   ├── store/
│   │   ├── sessionStore.ts           # Zustand：当前会话/面板开关
│   │   └── themeStore.ts             # Zustand：主题偏好
│   └── lib/
│       ├── rpc.ts                    # JSON-RPC 类型推导（Zod → TS）
│       └── utils.ts                  # cn() 等工具函数
├── index.html
├── vite.config.ts                    # Vite 8.1 (Rolldown) 配置
├── tailwind.config.ts                # Tailwind v4 (Oxide) CSS-first 配置
├── oxlint.config.json                # oxlint 规则
├── tsconfig.json                     # TypeScript 7.0
└── package.json
```

### 26.3 事件流消费（SSE → TanStack Query）

`minicoding-server` 通过 SSE 推送 `Event`（见 §24），前端用 `useEventStream` hook 订阅并增量更新 TanStack Query 缓存：

```typescript
// hooks/useEventStream.ts
// SSE 事件 → TanStack Query 缓存增量更新
// cursor 用于断线重连恢复（见 §24 E-13）
function useEventStream(sessionId: string, cursor: number) {
  const queryClient = useQueryClient();
  const eventSource = new EventSource(`/api/sessions/${sessionId}/events?cursor=${cursor}`);

  eventSource.addEventListener('Token', (e) => {
    const token = TokenEventSchema.parse(JSON.parse(e.data));
    // 增量追加到当前消息缓存
    queryClient.setQueryData(['session', sessionId, 'messages'], (old) => {
      const messages = old ?? [];
      const last = messages[messages.length - 1];
      if (last?.role === 'assistant' && last.streaming) {
        return [...messages.slice(0, -1), { ...last, content: last.content + token.delta }];
      }
      return [...messages, { role: 'assistant', content: token.delta, streaming: true }];
    });
  });

  eventSource.addEventListener('PermissionRequest', (e) => {
    const prompt = PermissionPromptSchema.parse(JSON.parse(e.data));
    // 推入 Zustand store，触发 shadcn/ui Dialog 弹出
    usePermissionStore.getState().prompt(prompt);
  });

  // ToolCall / ToolResult / TaskUpdated / Compress / HookRun 等同理
  return () => eventSource.close();
}
```

### 26.4 权限交互流（点对点，非广播）

权限确认是点对点交互（§9.1 决策/交互分离），前端通过 SSE 收到 `PermissionRequest` 事件后弹出 Dialog，用户决策经 JSON-RPC `permission.resolve` 回传：

```
1. Runtime 调用 PermissionPolicy::check → 返回 Verdict::Ask(prompt)
2. Prompter（WebPrompter）生成 prompt_id，经 SSE 推送 PermissionRequest{prompt_id, ...}
3. 前端 usePermissionStore 收到，弹出 shadcn/ui Dialog
4. 用户点击 Allow/Deny/AllowAlways
5. 前端 POST /api/rpc { method: "permission.resolve", params: {prompt_id, decision} }
6. 后端校验 prompt_id 有效性（防伪造）+ 调用 Prompter::resolve → 返回 Decision
7. Runtime 收到 Decision，继续执行或拒绝
```

> **安全**：`prompt_id` 由后端生成（ULID），前端不可伪造；`permission.resolve` 必须带有效 `prompt_id`，后端校验失败返回错误。这防止前端被 XSS 攻击后伪造权限批准。`AllowAlways` 写入 `policy.toml`（specificity=2），与 CLI 路径一致。

### 26.5 Tauri sidecar 集成

桌面模式下，Tauri 启动 `minicoding-server` 作为 sidecar 进程：

```rust
// minicoding-desktop/src/main.rs（Tauri 命令）
// 启动 sidecar：minicoding-server --http --bind 127.0.0.1:0（随机端口）
// sidecar 启动后通过 stdout 输出实际端口，Tauri 读取后注入前端
#[tauri::command]
async fn start_session(app: tauri::AppHandle) -> Result<SessionInfo, String> {
    let sidecar = app.shell().sidecar("minicoding-server")
        .map_err(|e| e.to_string())?;
    let (mut rx, child) = sidecar.args(["--http", "--bind", "127.0.0.1:0"])
        .spawn()
        .map_err(|e| e.to_string())?;
    // 读取 sidecar stdout 获取实际监听端口
    while let Some(event) = rx.recv().await {
        if let tauri_plugin_shell::process::CommandEvent::Stdout(line) = event {
            if let Some(port) = parse_port(&line) {
                return Ok(SessionInfo { port, pid: child.pid() });
            }
        }
    }
    Err("sidecar 启动失败".to_string())
}
```

前端通过 Tauri IPC `invoke('start_session')` 获取 sidecar 端口，然后用 `fetch` + `EventSource` 连接 `http://127.0.0.1:PORT`。凭证存储复用 OS keyring（与 CLI `cred.rs` 共享 `KEYRING_SERVICE = "minicoding"`，C-04），前端不接触凭证明文。

**sidecar 生命周期（防孤儿进程）**：`tauri-plugin-shell` 的 `CommandChild` **没有 Drop 清理**（无 kill_on_drop 等价物），若应用退出时不显式终止，sidecar 会残留为孤儿进程继续监听端口。方案：

- `SidecarProcess`（managed state）持有 `CommandChild` 句柄，`spawn_sidecar` 存入、`kill_sidecar` 取出并 `kill()`（`CommandChild::kill` 消费自身，幂等）；
- 应用以 `Builder::build` + `App::run(callback)` 形式启动，`RunEvent::Exit`（托盘"退出"、`restart_app` 重启）时调用 `kill_sidecar`；
- 窗口关闭仅隐藏到托盘（`prevent_close`，设计意图：后台保持服务），不触发 Exit，sidecar 继续运行。

### 26.6 安全考量（前端）

| 威胁 | 缓解 |
|------|------|
| XSS 注入（工具输出含恶意脚本） | React 默认转义 + CSP 严格（`script-src 'self'`）+ 不使用 `dangerouslySetInnerHTML`（除非经 DOMPurify） |
| 权限弹窗伪造 | `prompt_id` 后端生成校验，前端不可伪造 `permission.resolve` |
| 凭证泄露到前端 | 凭证仅存 Rust 后端内存 + OS keyring，前端只看到脱敏后的 `***`（§13.3） |
| SSE 跨会话串流 | SSE 端点校验 `session_id` 归属当前认证用户 |
| Tauri WebView 远程内容 | Tauri 默认禁用远程内容，仅加载本地 `dist/`；CSP 严格 |
| CORS 误配 | `--cors-origin` 默认仅 `http://localhost:*`，生产部署需显式配置 |

### 26.7 与 §24 协议的关系

M9 前端复用 §24 定义的全部 JSON-RPC 方法与 SSE 事件格式，**不引入新协议**。新增的只是：
- `minicoding serve --web ./dist`：静态资源托管（HTTP `GET /` 返回 `index.html`）；
- `--cors-origin`：CORS 配置（仅 Web 模式需要，桌面模式同源无需）；
- Tauri sidecar 管理：桌面模式专属，不影响协议层。

这保证 M9 的前端可以无缝切换"连本地 sidecar"或"连远程 server"，且 LSP/ACP 适配器（E-15..E-18）与 Web 前端共享同一后端，无重复实现。

### 26.8 桌面应用发布管道

桌面应用独立于 cargo-dist 的 CLI 发布流（`release.yml`），原因是 Tauri 需要平台 GUI 系统库 + 前端构建 + sidecar 打包，与 cargo-dist 的二进制发布模型不兼容。

**配置层**（`crates/minicoding-desktop/`）：

| 文件 | 作用 |
|------|------|
| `tauri.conf.json` | Tauri 2.x 配置：`frontendDist` 指向前端构建产物，`externalBin` 声明 sidecar 二进制，`beforeBuildCommand` 自动构建前端 |
| `Cargo.toml` | `[package.metadata.dist] dist = false` 显式排除 cargo-dist |
| `binaries/` | sidecar 二进制存放目录（gitignore，构建时生成） |

`tauri.conf.json` 关键配置：

```json
{
  "build": {
    "frontendDist": "../minicoding-web/dist",
    "beforeBuildCommand": "cd ../minicoding-web && npm run build"
  },
  "bundle": {
    "externalBin": ["binaries/minicoding-server"]
  }
}
```

`externalBin` 约定：Tauri 在打包时自动追加 host target triple 后缀（如 `binaries/minicoding-server-sidecar-x86_64-pc-windows-msvc.exe`），运行时通过 `tauri-plugin-shell` 的 `app.shell().sidecar("minicoding-server-sidecar")` 启动。sidecar 文件名用 `minicoding-server-sidecar` 而非 `minicoding-server`，避免与 cargo build 目录 `target/release/build/minicoding-server/` 冲突（tauri-build 的 `copy_binaries` 会 `fs::remove_file` 导致 `IsADirectory` panic）。

**构建脚本**（`scripts/`）：

| 脚本 | 用途 |
|------|------|
| `build-desktop.sh` | 完整发布构建：前端 → server 二进制 → sidecar 放置 → `cargo tauri build` 产出安装包 |
| `setup-desktop-dev.sh` | 本地开发占位：创建 sidecar 占位文件以满足 `tauri_build::build()` 的 `externalBin` 路径校验 |

本地开发流程：

```bash
# 首次：创建占位 sidecar（解决 cargo build 校验）
./scripts/setup-desktop-dev.sh

# 编译验证
cargo build -p minicoding-desktop --features desktop

# 发布构建（产出 .dmg/.msi/.AppImage）
./scripts/build-desktop.sh
```

**CI/CD 层**（`.github/workflows/`）：

| 工作流 | 作用 |
|--------|------|
| `ci.yml` 的 `desktop` job | PR/push 触发：编译验证 + clippy（仅 `cargo build`，不打包） |
| `desktop-release.yml` | tag 触发：4 平台 matrix（macOS arm64/x86_64、Windows、Linux）构建安装包并上传到 GitHub Release |

`desktop-release.yml` 构建矩阵：

| 平台 | runner | target | 产物 |
|------|--------|--------|------|
| macOS Apple Silicon | `macos-14` | `aarch64-apple-darwin` | `.dmg` |
| macOS Intel | `macos-14`（交叉编译） | `x86_64-apple-darwin` | `.dmg` |
| Windows | `windows-2022` | `x86_64-pc-windows-msvc` | `.msi` |
| Linux | `ubuntu-22.04` | `x86_64-unknown-linux-gnu` | `.AppImage` / `.deb` |

macOS Intel 二进制通过 `macos-14`（Apple Silicon runner）交叉编译产出，Rust 原生支持 `--target x86_64-apple-darwin`，无需 Intel runner（`macos-13` 已于 2025 年被 GitHub 移除）。

**与 cargo-dist 的协作**：

- 同一 tag（如 `v0.1.0`）同时触发 `release.yml`（CLI 二进制）和 `desktop-release.yml`（桌面安装包）；
- 两个工作流独立运行，互不依赖；
- `desktop-release.yml` 的 `upload-to-release` job 使用 `gh release upload --clobber` 将桌面安装包追加到 `release.yml` 创建的 GitHub Release。

**CI 中的占位 sidecar**：

`ci.yml` 的 `desktop` job 在 `cargo build` 前创建占位 sidecar（`binaries/minicoding-server-sidecar-x86_64-unknown-linux-gnu`），以满足 `tauri_build::build()` 对 `externalBin` 路径的编译期校验。真实 sidecar 二进制仅在 `desktop-release.yml` 中构建。

### 26.9 项目工作区（前端增强设计，已实现）

> **实现状态**：已实现（W-11，含 MVP + diff 视图 + 工作区切换 + 桌面端编辑器集成）。
> 实现细节见 `docs/api.md` §9.2（后端契约）与本节的变更记录。

**背景**：§26.1–§26.8 交付的 Web/桌面前端是"会话 MVP"——只有聊天流、权限弹窗、任务面板（W-01..W-10），没有"项目"概念。用户反馈"前端像大模型聊天机器人"，缺少在项目文件夹内操作的能力。本节设计把前端从"会话 UI"升级为"项目工作区 UI"（参考 Claude Code / Codex 桌面版）。

**设计原则**（约束对齐）：

- **前端只做展示与交互，零业务逻辑**（§8.1）：文件树、diff 均基于后端数据渲染，不做权限决策；
- **只读浏览不触发权限**：文件树/预览等价 `fs.read`（只读，C-01 仅约束副作用）；写操作仍走既有权限弹窗（W-03）；
- **C-03 路径沙箱在后端强制**：目录列表/文件读取端点复用 `sandbox_path` 校验，越界返回 `PathEscaped`，前端不可绕过；
- **复用既有事件流**：文件改动后的 diff/刷新由 `Event::MessageAppended`（tool 消息）驱动，不新增专用事件。

**26.9.1 后端 API 契约**（`minicoding-server` 新增端点）

| 端点 | 方法 | 请求 | 响应 | 说明 |
|------|------|------|------|------|
| `/sessions/{id}/workspace/root` | GET | — | `{ "path": "...", "name": "..." }` | 返回会话 workdir（`ServerRuntimeParams.workdir`） |
| `/sessions/{id}/workspace/list` | GET | `?path=<相对路径>` | `{ "entries": [ { "name", "kind": "file"/"dir", "size"? } ] }` | 目录列表（1 层，不递归）；`path` 省略=根目录；越界 403 |
| `/sessions/{id}/workspace/read` | GET | `?path=<相对路径>` | `{ "content": "...", "size", "truncated" }` | 文件内容（≤ 64 KiB，超出截断并置 `truncated=true`，C-07 输出上限） |
| `/sessions/{id}/workspace/diff` | GET | — | `{ "entries": [ { "op_id", "prompt_snippet", "files" } ] }` | 会话内文件改动历史（journal 快照转 DTO） |
| `/sessions/{id}/workspace` | POST | `{ "path": "<绝对路径>" }` | `{ "switched": bool, "path": "..." }` | 切换 workdir（Ask 审批 + 审计，见 26.9.4） |

**实现变更记录**（相对本节初稿）：

- `root` 改为 `GET /workspace`（与 POST 同路由，`get`/`post` 合并）；
- **切换工作区**：新增 `Runtime::switch_workdir(&self, target) -> Result<bool, RuntimeError>`
  （`docs/api.md` §9.2：非绝对路径拒绝、`workspace.switch` Ask 审批 `AllowOnce`、
  事件 + 审计 + `RwLock<Utf8PathBuf>` 更新，后续工具调用自动生效）；
- **切换前置校验**：`switch_workdir` 先 `canonicalize` + `is_dir` 校验目标**真实存在
  且是目录**——不存在/不可访问/指向文件时立即报错（HTTP 400），不进入权限弹窗
  （避免"切换成功但所有浏览 404"的假成功态）；`Allow` 后用 canonical 路径更新
  workdir（盘符/UNC/尾斜杠统一规范化）；
- **切换等待护栏**：HTTP handler 对 `turn_lock` 设 60s 等待上限，turn 进行中（最长
  600s）时返回 409 而非无界排队；前端 `switchWorkspace` 设 65s abort 超时，
  防止"切换中"无限转圈；
- **SSE 首次连接不回放历史**：`GET /sessions/{id}/events` 无 `Last-Event-ID` 时走
  `sse_live`（只推新事件）。此前从 seq 0 重放会把已决的 `permission_requested`
  重推给前端，导致权限弹窗 pid 覆盖错位、真实审批悬空直到 300s 超时
  （"切换中"卡死的根因之一）；断线重连由浏览器自动携带 `Last-Event-ID` 走
  `sse_stream` 恢复路径（不丢事件）；
- **diff 数据源**：server `runtime_builder` 默认注入 `FileChangeJournal`
  （与 CLI `file-undo` feature 对齐），`FileChange` → `WorkspaceFileChange`
  （`tag = "kind"` 序列化）DTO 转换在 server 层完成，不改 core 序列化形态；
- workdir 由不可变字段改为 `RwLock`（`&self` 下可切换；`Debug` 用 `try_read` 不阻塞；
  `Session.workdir` 保持创建时快照，仅 event sourcing 重建用）；
- `Runtime::workdir()` 改为 `async`（读锁克隆），SDK `Client::workdir()` 同步变更；
- **新建会话先选目录**：桌面端 `POST /sessions` 支持 `workdir` 字段（`CreateSessionBody`），
  前端新建会话弹 `NewSessionDialog` 输入/选择工作目录（Tauri 模式走
  `tauri-plugin-dialog` 原生目录选择器 `select_workspace_dir` 命令），
  不再默认继承 sidecar CWD。

约束：

- `path` 一律相对 workdir 解析，经 `crates/minicoding-tools/src/util.rs::resolve_path` + 路径沙箱校验（C-03）；
- 忽略 `node_modules`/`.git`/`target`/`dist` 等目录（内置 ignore 列表，避免前端拉取大目录）；
- 新端点**不经过** `PermissionPolicy`（只读等价 `fs.read`），但记录审计日志（`workspace browse:` 轨迹，供会话审计）；
- 不改动既有协议（§24）；TS 类型由 `minicoding-protocol` 新增 DTO 自动生成（§8.4）。

**26.9.2 前端组件与分层**（`crates/minicoding-web/src/`）

```
components/workspace/
├── WorkspacePanel.tsx   # 工作区面板容器（侧栏下方：根路径 + 文件树 + 刷新）
├── FileTree.tsx         # 递归文件树（懒加载子目录，虚拟滚动预留）
├── FileNode.tsx         # 单节点（图标/名称/大小；dir 可展开）
└── FilePreview.tsx      # 文件内容预览（只读，Markdown/文本高亮；diff 视图 M9 后续）
hooks/
├── useWorkspace.ts      # TanStack Query：root/list/read 封装 + 失效策略
└── useFileTree.ts       # 展开状态（Zustand：目录展开集合，纯 UI 状态）
api/
└── client.ts            # 新增 listWorkspace/readWorkspaceFile（§8.5 服务端状态走 Query）
```

**26.9.3 状态管理**：

- 服务端状态（目录/文件内容）→ TanStack Query（`useWorkspace`，缓存 5s，`staleTime` 短；工具执行后由 SSE `tool_call_finished` 触发 `invalidateQueries`）；
- 客户端 UI 状态（展开目录集合、面板折叠）→ Zustand（`usePanelStore` 扩展，不双写）；
- 文件内容预览不进缓存（大文件 + 低频查看，直接用 Query 数据）。

**26.9.4 权限与安全边界**：

- 文件树/预览是只读，不弹权限弹窗（与 `fs.read` 一致，C-01 只约束副作用）；
- 写操作（fs.write/edit）仍由模型发起 + 权限弹窗（W-03），前端不新增写入口（第一版）；
- C-03 越界：后端返回 403 `PathEscaped`，前端 toast 展示，不吞错（§8.6）；
- 桌面端（Tauri）：新建会话先选工作目录（`tauri-plugin-dialog` 原生目录选择器，
  `POST /sessions` 携带 `workdir`）；sidecar 自身不传 `--workdir`（默认 CWD），
  会话目录由用户在新建会话时显式选定。

**26.9.5 迭代路径**：

1. **MVP（本节）**：文件树浏览 + 文件预览 + 根路径展示；验证 C-03/只读不弹权限/事件流驱动刷新；
2. **diff 视图**：tool 消息携带 `before/after`（已有 `FileChange` 语义），前端渲染 `DiffEntry` 列表；
3. **工作区切换**：`POST /sessions/{id}/workspace` 切换 workdir（需审批，Ask 级别，C-01）；
4. **编辑器集成**：`tauri-plugin-dialog` 选择项目文件夹 + 文件双击用系统编辑器打开（`shell.open`）；
5. **先选目录再新建对话**：新建会话弹目录选择/输入对话框，`workdir` 随 `POST /sessions` 提交
   （桌面端原生目录选择器 `tauri-plugin-dialog`，Web 端输入框）；
6. **turn 执行可见性与任务面板数据链路（已实现）**：
   - 前端工具调用卡片：SSE `tool_call_started`/`tool_call_finished` → 进行中/✓/✗ 状态 + 结果摘要；
   - 等待计时：turn 进行中显示 elapsed 秒数，≥60s 无输出提示可能等待权限/网络，可主动停止；
   - **停止按钮**：`POST /sessions/{id}/cancel` → `Runtime::cancel()`，SSE 推
     `TurnEnd { stop_reason: interrupted }`（前端 `ChatInput` 常驻提供，不再只有"思考中"黑盒）；
   - **任务面板真数据**：`ServerSession::task_state` 常驻订阅 `Event::TaskUpdated` 镜像
     `TaskStore` 变更，`GET /sessions`/`GET /sessions/{id}` 返回 `tasks`（此前恒为空数组，
     前端任务按钮因 `tasks.length === 0` 永远禁用）。

**验证标准**（对应 §8.8 测试策略）：

- Rust 侧：新端点单元测试（越界 403、ignore 列表、截断）；`wiremock` 不连真实服务（§5.4）；
- 前端侧：MSW mock 目录列表/读取，Testing Library 测 FileTree 展开/懒加载、预览渲染；
- E2E（Playwright）：打开工作区 → 展开目录 → 预览文件 → 发消息让模型改文件 → 树自动刷新。

**约束 ID 同步**：本节设计不新增 L0/L1 约束（C-03/C-07 已覆盖），features.md 新增 W-11 功能项。
