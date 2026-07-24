# Hooks 系统设计

本文设计 `minicoding-rs` 的 Hooks 机制，参考 Claude Code 的 8 类生命周期 Hook，并融合本项目已有的 `EventBus`、`PermissionPolicy/Prompter`、上下文压缩管道。Hooks 让用户在不修改工具实现的前提下，注入自定义逻辑：拦截/批准工具调用、改写参数、注入上下文、自动跑格式化、备份压缩前现场等。

---

## 1. 设计目标与定位

| 目标 | 说明 |
|------|------|
| 可扩展 | 用户用脚本（任意可执行）或 Rust 实现 `Hook` trait 接入，无需改 core |
| 安全 | Hook 在用户权限下运行，但受 Hook 自身策略约束（超时、输出上限、不可绕过 L0 约束） |
| 与权限正交 | Hook 可影响 `Verdict`（批准/拒绝/改写），但**不能**覆盖内置安全黑名单（L0，见 `rules.md`） |
| 与事件系统协作 | Hook 触发点与 `EventBus` 事件对齐，便于观测 |

**定位**：Hook 是"工具调用生命周期"的拦截器，介于"LLM 决定调用工具"与"工具真正执行"之间；也是"会话/轮次/压缩"等关键节点的观察+注入点。它与 `PermissionPolicy` 的关系：`PreToolUse` Hook 在 `policy.check` **之后**运行，可把 `Ask` 升级为 `Allow`（自动批准）或 `Deny`（阻断），但不可把内置黑名单的 `Deny` 改为 `Allow`。

---

## 2. Hook 事件类型（10 类）

对齐 Claude Code 的事件分类，结合本项目命名，在原 8 类基础上新增 `PostToolUseFailure` 与 `PostCompact`（参考 CC 27 类事件的子集，按需扩展）：

| 事件 | 触发时机 | 典型用途 | 可否阻断 | 可否注入上下文 |
|------|---------|---------|:---:|:---:|
| `SessionStart` | 会话开始/resume | 注入 git status、TODO、环境信息 | 否 | 是 |
| `UserPromptSubmit` | 用户提交 prompt 后、构建请求前 | 追加 sprint 上下文、校验请求 | 是（拒绝提交） | 是 |
| `PreToolUse` | `policy.check` 后、工具执行前 | 阻断危险操作、校验路径、改写参数、自动批准 | 是 | 是（改写 input） |
| `PostToolUse` | 工具执行成功后、结果回灌前 | 跑 formatter/linter、记录变更、改写结果 | 否 | 是（改写 result） |
| `PostToolUseFailure` | 工具执行失败后、错误回灌前 | 诊断失败原因、降级处理、记录错误模式 | 否 | 是（改写 error） |
| `PreCompact` | 上下文压缩前 | 备份现场、保留关键决策 | 否 | 是（追加保留指令） |
| `PostCompact` | 上下文压缩后 | 验证压缩质量、重新注入丢失的关键上下文 | 否 | 是（补充注入） |
| `Stop` | 主 Agent 一轮结束 | 校验任务完成、跑测试、生成摘要 | 是（要求继续） | 否 |
| `SubagentStop` | 子 Agent 完成 | 校验子任务产出、触发后续 | 否 | 否 |
| `PermissionRequest` | `Verdict::Ask` 即将弹窗前 | 自动批准测试命令、阻断敏感文件 | 是（直接给出 Decision） | 否 |

新增事件说明：
- `PostToolUseFailure`：与 `PostToolUse` 互补——前者处理失败，后者处理成功。失败诊断 Hook 可分析错误模式（如沙箱拒绝、权限拒绝、超时），自动建议修正或降级。
- `PostCompact`：压缩后触发，允许 Hook 验证压缩是否丢失关键信息（如对比压缩前后 todo 列表完整性），并补充注入。与 `PreCompact` 的"备份"互补，`PostCompact` 是"验证与修复"。

> 与 `design.md` §11 `Event` 的关系：`EventBus` 的 `ToolCallStart`/`ToolCallEnd`/`PermissionRequested` 等是**通知**（只读广播）；Hook 是**参与**（可阻断/改写/注入）。Hook 内部可订阅 EventBus 做日志，但 Hook 的控制语义走独立通道（见 §4）。

---

## 3. Hook 协议

Hook 以"外部可执行 + JSON over stdio"为主协议（脚本友好），同时提供 Rust `Hook` trait 给内建/SDK 用。两种实现共用同一 JSON schema。

### 3.1 输入（stdin，单行 JSON）

```json
{
  "event": "PreToolUse",
  "session_id": "sess_01H...",
  "turn": 3,
  "tool": { "name": "fs.write", "input": { "path": "src/main.rs", "content": "..." } },
  "side_effect": "FileWrite",
  "verdict": "Ask",
  "cwd": "e:/projects/foo",
  "env": { "MINICODING_HOOK": "1" }
}
```

字段按事件类型裁剪：`SessionStart` 不含 `tool`；`PreCompact` 含 `tokens_before/tokens_after` 预估值；`PermissionRequest` 含 `prompt` 摘要。

### 3.2 输出（stdout，单行 JSON，退出码 0 表成功）

```json
{
  "decision": "allow",                 // allow | deny | ask | continue（不干预）
  "reason": "auto-approved by hook",
  "modify_input": { "path": "src/main.rs", "content": "...格式化后..." },
  "inject_context": "本次 sprint 优先处理支付模块",
  "exit_message": "已自动运行 prettier"
}
```

| 字段 | 适用事件 | 语义 |
|------|---------|------|
| `decision` | PreToolUse/PermissionRequest/Stop | allow=批准执行；deny=阻断；ask=仍走交互；continue=不干预 |
| `reason` | deny/allow | 回灌给 LLM 或写审计 |
| `modify_input` | PreToolUse | 改写工具入参（如格式化后内容） |
| `inject_context` | SessionStart/UserPromptSubmit/PreCompact | 追加到 system/上下文 |
| `exit_message` | 全部 | 展示给用户 |

### 3.3 退出码

| 码 | 含义 |
|----|------|
| 0 | 输出有效 JSON，按 `decision` 处理 |
| 2 | 阻断（等价 `decision=deny`，reason 取 stderr） |
| 其他 | Hook 错误，按 `on_hook_error` 策略处理（默认 `continue`，记 warn） |

---

## 4. Hook 执行模型与控制流

Hook 与权限/工具的关系（关键：Hook 不覆盖 L0）：

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
若仍 Ask → Prompter 交互
   │
   ▼
Tool.execute（用可能被改写的 input）
   │
   ▼
PostToolUse Hook
   │  可：改写 result / 跑 formatter / 记录变更
   ▼
tool_result 回灌 LLM + 审计落盘
```

要点：
- **L0 优先**：内置黑名单的 `Deny` 在 Hook 之前生效，Hook 永远收不到被黑名单拒绝的调用（或收到但 `verdict` 已是 `Deny(builtin)`，Hook 的 `allow` 被忽略）。
- **Hook 失败不崩主流程**：Hook 超时/非零退出按 `on_hook_error`（`continue`/`deny`/`fail`）处理，默认 `continue` + warn。
- **串行**：同一事件的多个 Hook 按声明顺序串行，前一个的输出（`modify_input`/`inject_context`）作为后一个的输入。

---

## 5. Rust API

### 5.1 `Hook` trait

```rust
/// 内建/SDK 用的进程内 Hook；外部脚本走 ScriptHook 适配器实现本 trait。
#[trait_variant::make(Hook: Send)]
pub trait Hook {
    /// 唯一名
    fn name(&self) -> &str;
    /// 命中哪些事件与工具（matcher）
    fn matcher(&self) -> &HookMatcher;
    /// 处理
    async fn run(&self, input: HookInput) -> Result<HookOutput, HookError>;
}

pub struct HookMatcher {
    pub events: Vec<HookEvent>,        // 订阅的事件
    pub tools: Option<Vec<String>>,    // None=所有工具；仅 PreToolUse/PostToolUse/PermissionRequest 有效
}

pub enum HookEvent {
    SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, PostToolUseFailure,
    PreCompact, PostCompact, Stop, SubagentStop, PermissionRequest,
}

pub struct HookInput {
    pub event: HookEvent,
    pub session_id: SessionId,
    pub turn: u32,
    pub tool: Option<ToolCall>,
    pub side_effect: Option<SideEffect>,
    pub verdict: Option<Verdict>,
    pub cwd: Utf8PathBuf,
    pub extras: serde_json::Value,     // 事件特有字段
}

pub struct HookOutput {
    pub decision: HookDecision,        // Allow | Deny | Ask | Continue
    pub reason: Option<String>,
    pub modify_input: Option<serde_json::Value>,
    pub inject_context: Option<String>,
    pub exit_message: Option<String>,
}

pub enum HookDecision { Allow, Deny, Ask, Continue }
```

### 5.2 `HookRegistry` 与调度

```rust
pub struct HookRegistry {
    by_event: HashMap<HookEvent, Vec<Arc<dyn Hook>>>,
}

impl HookRegistry {
    pub fn register(&mut self, hook: Arc<dyn Hook>);
    /// 按事件取出有序 Hook 列表；Runtime 串行执行，聚合 modify_input/inject_context
    pub fn for_event(&self, e: HookEvent) -> &[Arc<dyn Hook>];
}
```

`ScriptHook` 适配器把外部可执行包装为 `Hook`：序列化 `HookInput`→stdin，读 stdout JSON→`HookOutput`，按退出码映射。配置中声明的 Hook 全部以 `ScriptHook` 形式注册。

---

## 6. 配置

```toml
# ~/.minicoding/config.toml
[hooks]
on_hook_error = "continue"        # continue | deny | fail
default_timeout_sec = 30

[[hooks.PreToolUse]]
matcher = "fs.write"              # 工具名 glob
command = "prettier --write ${TOOL_INPUT_PATH}"   # 简写：仅命令
timeout_sec = 10

[[hooks.PreToolUse]]
matcher = "shell.run"
command = "~/.minicoding/hooks/block-danger.sh"

[[hooks.PostToolUse]]
matcher = "fs.write|fs.edit"
command = "cargo fmt"             # 写后自动格式化

[[hooks.PermissionRequest]]
matcher = "shell.run"
command = "~/.minicoding/hooks/auto-approve-tests.sh"   # 自动批准 cargo test

[[hooks.SessionStart]]
command = "git status --short"    # 输出注入上下文

[[hooks.PreCompact]]
command = "~/.minicoding/hooks/backup-transcript.sh"

[[hooks.Stop]]
command = "cargo test --quiet"    # 一轮结束跑测试
```

`matcher` 语法：工具名 glob，`|` 分隔多个，`*` 通配。`command` 支持 `${TOOL_INPUT_<KEY>}` 占位符（按工具 input 字段展开，经 shell 转义防注入）。

---

## 7. 安全约束（与 `rules.md` 协同）

| 约束 | 说明 |
|------|------|
| L0 不可覆盖 | Hook 的 `allow` 对内置黑名单 `Deny` 无效（见 §4） |
| Hook 隔离 | `ScriptHook` 子进程不继承凭证环境变量（同 `shell.run`，见 `security.md` §6） |
| 超时强制 | Hook 超时 kill，按 `on_hook_error` 处理 |
| 输出上限 | Hook stdout 截断 1 MiB，防 OOM |
| 路径校验 | `modify_input` 仍经 `sandbox_path`，Hook 不能借此越界 |
| 审计 | Hook 的 `allow`/`deny`/`modify_input` 全部落 `audit.log`，标注 `source=hook:<name>` |
| Prompt 注入 | `inject_context` 内容包裹 `<hook_context>` 边界，声明非指令 |

---

## 8. 与各子系统的集成点

| 子系统 | 集成 |
|--------|------|
| 权限（§9） | `PermissionRequest` Hook 可短路 `Prompter`；`PreToolUse` 可 `Ask→Allow` |
| 上下文压缩（§3） | `PreCompact` 在 4 级管道启动前触发，可注入"必须保留"指令影响权重 |
| 记忆（§8） | `SessionStart` 注入长期记忆之外的动态信息（git status） |
| 事件总线（§11） | Hook 触发同步发 `Event::HookRun { name, event, decision, elapsed }` 供观测 |
| OTel（§15） | Hook 执行打 `hook.run` span，属性 `hook.name`/`hook.event`/`hook.decision` |
| 子 Agent | `SubagentStop` Hook 可校验产出，决定是否要求父 Agent 重试 |

---

## 9. 内置示例 Hook

| 名称 | 事件 | 用途 |
|------|------|------|
| `fmt-on-write` | PostToolUse(fs.write\|fs.edit) | 写后跑 `cargo fmt`/`prettier` |
| `auto-approve-tests` | PermissionRequest(shell.run) | 前缀 `cargo test`/`npm test` 自动批准 |
| `block-secrets` | PreToolUse(fs.write) | 拒绝写入含 `api_key`/`password` 的内容 |
| `git-status-inject` | SessionStart | 注入 `git status --short` |
| `backup-before-compact` | PreCompact | 压缩前备份 jsonl 到 `.backup` |
| `test-on-stop` | Stop | 一轮结束跑测试，失败则要求继续 |

---

## 10. 与 Claude Code 的差异

| 点 | Claude Code | minicoding-rs |
|----|-------------|---------------|
| 配置 | `~/.claude/settings.json` | `~/.minicoding/config.toml`（TOML） |
| 覆盖黑名单 | 依赖 Hook 自觉 | L0 硬约束，Hook 不可覆盖（更强） |
| 协议 | JSON stdio | 一致 |
| 事件数 | 27 | 10（按需扩展，避免过度复杂） |
| 内建 Hook | 较少 | 提供 6 个开箱即用示例 |
| 观测 | 无统一 trace | OTel `hook.run` span |
| 异步唤醒 | `asyncRewake` | `asyncRewake`（§11） |

---

## 11. asyncRewake 异步唤醒（参考 Claude Code）

某些 Hook 需执行长时异步任务（如安全扫描、CI 触发、依赖更新检查），结果不应阻塞当前轮次。参考 CC 的 `asyncRewake` 机制，Hook 可声明异步执行，完成后"唤醒"Agent 继续处理结果。

### 11.1 协议扩展

HookOutput 新增 `async_rewake` 字段：

```rust
pub struct HookOutput {
    pub decision: HookDecision,
    pub reason: Option<String>,
    pub modify_input: Option<serde_json::Value>,
    pub inject_context: Option<String>,
    pub exit_message: Option<String>,
    /// 异步唤醒：Hook 声明"我会在后台继续跑，完成后唤醒 Agent"
    pub async_rewake: Option<AsyncRewakeSpec>,
}

pub struct AsyncRewakeSpec {
    pub task_id: String,              // 唤醒任务 ID
    pub estimated_duration: Duration, // 预估时长（超时后 Agent 可放弃等待）
    pub wake_prompt: String,          // 唤醒时注入的 prompt（如"安全扫描完成，结果：..."）
}
```

### 11.2 执行流

```
PostToolUse Hook 触发
   │
   ├─ Hook 同步返回 async_rewake = Some(spec)
   │  ├─ 主流程不阻塞，继续 Agent 循环
   │  └─ Hook 子进程在后台继续执行（如跑 `cargo audit`）
   │
   ▼
（后台 Hook 完成）
   │
   ├─ Hook 通过 stdin 接收原 session_id + task_id
   ├─ Hook 输出最终结果（JSON，含 wake_prompt）
   │
   ▼
Runtime 检测到 async_rewake 完成
   │
   ├─ 当前轮次结束后（Stop 事件前），注入 wake_prompt 作为 system reminder
   └─ Agent 看到"安全扫描完成，发现 2 个漏洞..."并决定是否处理
```

### 11.3 约束

- `async_rewake` 仅对 `PostToolUse`/`PostToolUseFailure`/`Stop` 事件有效（这些是"事后"事件，不阻塞主流程）；
- `PreToolUse`/`PermissionRequest` 不支持（这些是"事前"事件，必须同步决策）；
- 后台 Hook 超时（`estimated_duration` × 2）后自动 kill，注入超时提示；
- 同一 session 最多 3 个并发 async_rewake（防资源耗尽）；
- async_rewake 的结果走 `inject_context`，包裹 `<async_rewake>` 边界，声明非指令；
- 阶段 6+ 交付，MVP 不含。

---

## 12. 测试策略

- 单测：`HookRegistry` 串行聚合、`ScriptHook` 退出码映射、`modify_input` 链式传递。
- 集成：`PreToolUse` deny 阻断、`PostToolUse` formatter 触发、`PermissionRequest` 短路。
- 安全：黑名单 `Deny` 时 Hook `allow` 被忽略（L0 不破）；`modify_input` 越界被 `sandbox_path` 拦。
- 超时：Hook 慢于 `timeout` 被 kill，主流程按 `on_hook_error` 继续。
