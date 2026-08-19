# minicoding-rs × deepseek-harness 重新对比与代码级修改设计文档

> **本次为独立重读，不依赖任何既有结论。**
>
> - 右项：`deepseek-harness`（WSL `/home/star/deepseek-harness` @ `99f6f02`，dsh-v0.1.0-rc.7，pnpm monorepo）
> - 左项：`minicoding-rs`（WSL `/home/star/projects/minicoding-rs`，v0.2.30，19 crate，约 63k 行 Rust）
> - 方法：两份仓库源码级重读；对 minicoding-rs 的**待修改模块逐文件精读**（`jsonl.rs`/`lock.rs`/`event.rs`/`message.rs`/`denial.rs`/`tool/trait.rs`/`policy/trait.rs`/`sse.rs`/`approval.rs`/`rt.rs` 的 `run_turn`/`execute_tool_calls`/`execute_side_effect_call`/`persist_event`/`restore_history`/`config.rs`/`watcher.rs`），所有论断附 file:line。
> - **重要**：本次重读发现既有 `docs/deepseek-harness-comparison.md`(v2)、`docs/improvement-design.md` 以及我此前的分析存在**若干过时/错误结论**，已在 §1.5 与 §2 显式纠正。这正是"重新分析"的价值所在。

---

# 第 0 部分：本次重新分析纠正了哪些旧结论（先读）

| 旧结论（来源）                                              | 本次源码核实后的真相                                                                                                                                                | 证据                                                                                    |
| ---------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- |
| R-06"工具串行执行，无并行"（v2 对比 §2.5、improvement-design R-06） | **错。只读工具并行已实现**：readonly 桶 `buffer_unordered(8)` 并行、副作用桶串行、按 LLM 顺序回填                                                                                     | `rt.rs:1017` 分桶、`:1100` 并行、`:1126` 回填                                                 |
| R-03"无 LLM 循环打断，仅超时兜底"（improvement-design R-03）      | **部分错。已有硬停止版重复检测**：连续 ≥3 轮相同工具调用集合直接 `Stopped`。缺的是 dsh 式软性逐级提醒                                                                                            | `rt.rs` `tool_calls_signature`(890)、`is_repeating`(899)、`run_turn` 753-765            |
| "事实源分裂，事件溯源只做一半"（我上一版报告 S1-1 的措辞）                    | **过重。事件溯源已较成熟**：messages+events 双写、`SCHEMA_VERSION=1`、定期 snapshot、SSE durable recovery，是有意的双写迁移设计                                                         | `event.rs`、`rt.rs:init_event_stream`(442)/`persist_event`(528)/`create_snapshot`(577) |
| "[tools] 段是死配置"（v2 §2.1、improvement-design R-04）     | **部分错。**`ToolsConfig` 有真实消费字段（`enabled_groups`/`fs_max_read_bytes`/`shell_timeout_sec`/`shell_max_output_bytes`）；缺的是 `parallel_reads`/重复阈值旋钮（当前硬编码 8 和 3） | `config.rs:134-138`                                                                   |
| MCP `local` scope 绕过审批是漏洞（我上一版 S2-3）                 | **降级。是有意的信任边界**：`local`/`user` 来自用户主目录配置，`Project` 才来自仓库 `.minicoding/mcp.json` 需首次批准。非漏洞，但需文档化信任边界                                                       | `mcp/approval.rs:check_project_scope_approval`（`scope != Project` 直接放行）               |
| storage 的 `unwrap()` 会在生产路径 panic（子代理）               | **错。全部在 `#[cfg(test)]`**，生产路径无此风险                                                                                                                         | `event.rs:315`/`snapshot.rs:148`/`trait.rs:118` 均在 test 模块                            |

**本次新发现（旧文档完全没有）**：

- **D-05 悬空 tool_calls**：`run_turn` 先落盘 assistant 消息（含 tool_calls）→ 再执行工具 → 再落 tool_result。中途 cancel/timeout/崩溃时，会留下"有 tool_calls、无 tool_result"的悬空消息；`restore_history`（rt.rs:404）与 replay 均**不修复**。Anthropic 等严格 provider 要求每个 `tool_use` 必须有 `tool_result`，否则下次请求 400。**resume-after-interrupt 会坏**。证据：`rt.rs` 第 4 步落盘(734)、第 6 步执行(768)、第 7 步落 tool_result(776-784)，cancel/timeout 分支(826-852)不回填。

**仍然成立的真实缺陷（已二次核验）**：core 内含领域算法（S1-1）、消息流并发写无跨进程锁（S1-2）、单坏行报废整会话+消息流无版本号（S2-1）、SSE 只认 `\n\n`（S2-2）、`SandboxError` 无 `Denied` 结构化变体（R-08）、`api_key` 构造时读一次不轮换（R-07）。详见 §2。

---

# 第 1 部分：deepseek-harness vs minicoding-rs 对比报告（重新核实版）

## 1.1 一句话定位

- **dsh**：会话即事件日志（单一事实源）+ 全插件化（连 agent loop 都是 Cordis 插件）的"研究/评测 + 产品化"双目标平台，默认 DeepSeek V4。
- **minicoding-rs**：以 Rust 类型系统 + L0 硬约束（C-01..C-35）实现层强制为护城河的"可信终端编码助手"，多前端（CLI/TUI/Web/Desktop/LSP/ACP/NDJSON/MCP），多 Provider。

## 1.2 架构哲学对照

| 对照点   | dsh                                                                              | minicoding-rs（重新核实后）                                                                                               |
| ----- | -------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| 组合单位  | Cordis 插件，细到 loop 可替换，支持 HMR                                                     | trait 定义在 core + `Runtime` 聚合根编排，编译期静态组合，无热重载（但有 `ConfigWatcher` 变更通知钩子）                                           |
| 会话事实源 | append-only `SessionEvent` 日志，`deriveMessages()` 投影，"model-visible means logged" | **双写**：`{id}.jsonl`（消息）+ `{id}.events.jsonl`（事件）+ 定期 snapshot；`replay_session_state` 从 snapshot+事件重建               |
| 压缩    | 日志上 `SurfaceOp.replace`，摘要带 `sourceEventSeqs` 引用链，可回放                            | 4 级压缩重写消息；`backup_before_compress` 仅调试备份；**无引用链**                                                                  |
| 工具执行  | `executionMode` 分类并行/exclusive                                                   | **readonly `buffer_unordered(8)` 并行 + 副作用串行**（已实现），按 LLM 顺序回填                                                      |
| 循环打断  | `repeat-tool-reminder` 软性 [3,5,8] escalate，不替换输出                                 | 硬停止版：`is_repeating` ≥3 轮相同集合 → `Stopped`                                                                           |
| 权限    | `ctx.approval` fail-closed，approval 事件 log-only 不进 transcript                    | `PermissionPolicy`(决策)/`PermissionPrompter`(交互)分离；builtin 黑名单 C-02 不可覆盖；audit.log 0600                             |
| 凭证    | `CredentialRef` 引用式，每次操作重解析                                                      | `api_key` 构造时读一次入 struct，`chat_stream` 直接用（轮换需重启）                                                                  |
| 沙箱    | `native/landlock-run` fail-closed + 探测链 + denial 事实分类 + E2B                      | `SandboxDriver` trait + landlock/seccomp/Seatbelt/JobObject；`DenialDetector` 字符串匹配 + `SandboxCircuitBreaker`(C-30) |
| 持久化   | checksummed zstd 帧 + 契约测试 + 孤儿 turn 补记                                           | JSONL+fsync，但**并发写无锁/单坏行报废/中断悬空 tool_calls**                                                                       |
| 前端测试  | vitest 每文件 100% 覆盖 + snapshot 三态回放                                               | 前端仅 tsc/oxlint/build 门禁，无单测                                                                                        |

## 1.3 双方独有能力（核实后）

- **dsh 独有**：事件溯源全栈一致 + `SurfaceOp` 压缩、tool 结果剪枝器、软性循环打断、CredentialRef、fail-closed 沙箱链 + denial 分类 + E2B、双 build face 前端、render intent、snapshot 三态回放 + 100% 覆盖门禁、配置热重载、continuable 子 agent。
- **minicoding-rs 独有**：L0 实现层硬约束 + 熔断（C-29/C-30）、builtin 黑名单不可覆盖（C-02）、audit.log 独立落盘、FileChangeJournal + `/undo`（C-28 冲突检测）、Hook 三隔离（C-26）、**多协议接入矩阵（ACP/LSP/NDJSON/MCP + 全前端）**、AGENTS.md 分层加载 + Auto/long_term 隔离（C-27）、三平台沙箱 CI matrix。

## 1.4 结论

minicoding 的护城河是**可信（硬约束+审计+undo+多前端）**；最该向 dsh 学的是**事件溯源一致性、压缩引用链、软性循环打断、工具输出声明、回放/前端测试**。而本次重读确认，minicoding 的**结构性短板集中在"持久化韧性"与"core 层边界纪律"**——恰是 dsh 最扎实处。

## 1.5 与既有 v2 对比文档的差异说明

v2 对比写于今日但**漏看了 R-06 并行与 R-03 重复检测已落地的代码**（可能在其调研后才实现，或调研未覆盖 `execute_tool_calls`/`is_repeating`），并对"事实源分裂"的表述过重。本文已按当前源码修正。

---

# 第 2 部分：minicoding-rs 现状与问题（重新核实）

## 2.1 严重（S1）

### S1-1 core 违反"零实现"原则（领域算法在 core，且有循环依赖陷阱）

- **证据**：`core/src/sandbox/denial.rs`（333 行 `DenialDetector`+`SandboxCircuitBreaker`）、`storage/replay.rs`（360 行 `replay_session_state`）、`storage/snapshot.rs`（176 行）、`agent/worktree.rs`（668 行 git 逻辑）、`prompt/pipeline.rs`(296)/`prompt/context.rs`(236)、`config.rs`(451)。core 共 12,890 行/54 文件，为全项目最大 crate。
- **文档矛盾**：`docs/architecture.md`/`review-report.md` 均称"core 仅 trait+编排、零实现"。
- **关键陷阱（本次新厘清）**：`Runtime`（在 core）**直接持有具体的 `SandboxCircuitBreaker`**（`rt.rs` 的 `self.sandbox_breaker`）。若把 `denial.rs` 直接搬进 `minicoding-sandbox`，而 sandbox 又依赖 core，会造成**循环依赖**。正确做法是 **trait 注入**（见 M-05）。
- **影响**：core 无法作为纯抽象层被替换；熔断逻辑在 core(`denial.rs`) 与 context(`compress/circuit_breaker.rs`) 两处并存；架构声明与代码冲突，损害审查可信度。

### S1-2 消息流并发写无跨进程锁 → 同会话交错损坏

- **证据**（`storage/src/jsonl.rs` `append`）：两次独立 `write_all`（line 然后 `\n`），且**不获取** `lock.rs` 的 `SessionLock`（`fs2` 排他锁，仅在 `--resume` 单点持有时用 `try_lock_exclusive`）。`index.json` 每次全量 `save`，`update_index_on_append` 仅进程内 `Mutex`。
- **本质**：两个进程向同一 `{id}.jsonl` 追加时，可在两次 `write_all` 之间交错，把两条消息并成一行 JSON（不可解析）；索引跨进程互相覆盖。
- **触发**：TUI+server 或双实例共用 sessions 目录。

## 2.2 中等（S2）

### S2-1 单坏行整会话报废 + 消息流无版本号（且 scan/load 行为不一致）

- **证据**：`jsonl.rs` `load`(470-477)/`load_messages_sync` 任一行解析失败即 `StorageError::Corrupted` 整会话失败；而 `build_index_from_scan`/`find_first_user_summary` 却**跳过坏行**（warn+continue）。**同一份文件，scan 容错、load 严格**，行为不一致。消息流**无 `format_version`**（注意：事件流有 `SCHEMA_VERSION=1`，消息流没有）。

### S2-2 SSE 解析只认 `\n\n`，不处理 CRLF

- **证据**：`providers/common/sse.rs:40` `self.buffer.windows(2).position(|w| w == b"\n\n")?`。上游发 `\r\n\r\n` 时分隔符永不匹配，缓冲堆积、事件饿死。（UTF-8 跨 chunk、流尾残留 flush、`[DONE]` 透传已正确处理，仅 CRLF 缺失。）

### S2-3 中断后悬空 tool_calls（D-05，本次新发现）

- **证据**：`run_turn` 第 4 步先 `storage.append(assistant_msg)`（含 tool_calls，rt.rs:734），第 6 步执行工具，第 7 步才逐个 append tool_result(776-784)。cancel/timeout 分支（826-852）直接 `TurnEnd`，**不回填缺失的 tool_result**。`restore_history`(404) 只回放大消息不修复。
- **影响**：Anthropic 等严格 provider 要求每个 `tool_use` 必须有对应 `tool_result`，否则下一次请求返回 400 → **resume-after-interrupt 失效**。dsh 对取消"补合成错误结果保回放"，minicoding 缺这一步。

## 2.3 低危 / 观察项（S3）

- **S3-1** `SandboxError` 仅 `Sandbox(String)`/`Io`，无 `Denied` 结构化变体（R-08 未做，见 `sandbox/trait.rs`）。
- **S3-2** `api_key` 在 provider 构造时读一次入 struct，`chat_stream`(openai.rs:153) 直接用 → key 轮换需重启（R-07 未做）。
- **S3-3** 并行度（8）与重复阈值（3）硬编码，无 `[tools]` 配置旋钮（R-04/R-06 收尾项）。
- **S3-4** MCP `local` scope 免审批是**有意信任边界**，需在 `security.md` 文档化（非漏洞）。
- **S3-5** 文档漂移：`architecture.md` 的"core 零实现"声明与代码矛盾（S1-1）应最先修；`README` crate 数、`features.md` 状态过期。

## 2.4 已核实成立的约束（不可放松的基线）

C-03 路径越界（`tools/util.rs resolve_path`）、C-23 AGENTS.md 写保护（`policy/builtin.rs`）、C-29 压缩熔断（`context/compress/circuit_breaker.rs`，非 LLM 可控）、C-30 沙箱熔断（`core/sandbox/denial.rs SandboxCircuitBreaker`）、C-04 凭证脱敏（`openai.rs Debug` mask + keyring）、Provider 重试/退避/Retry-After（`common/retry.rs`）、事件溯源双写 + snapshot + SSE durable recovery。

---

# 第 3 部分：代码级修改设计文档（怎么改）

> 通用约束：保持"trait 在 core、实现在领域 crate、core 不依赖领域 crate"的依赖方向；不新增生产路径 panic；改代码必改文档；L0 不放松。每项给出：**现状（file:line + 代码）→ 目标 → 新代码 → 修改点 → 迁移 → 测试 → 文档 → 约束兼容性 → 工作量**。
>
> 批次：**批次 0（地基修复，最高优先）= M-01..M-05**；**批次 1（对标 dsh，仍未实现）= M-06..M-10**；**批次 2（增强）= M-11..M-15**。

---

## 批次 0：地基修复

### M-01 消息流并发写安全 + 原子 append（修 S1-2）

**现状**（`crates/minicoding-storage/src/jsonl.rs`，`append`）：

```rust
file.write_all(line.as_bytes()).await?;   // 第一次 write
file.write_all(b"\n").await?;             // 第二次 write —— 与第一次之间无锁
file.flush().await?;
file.sync_all().await?;
self.update_index_on_append(&session_id, &msg); // 进程内 Mutex，跨进程无保护
```

**目标**：同会话并发写不交错；索引跨进程安全。

**怎么改（两步）**：

1. **单次原子 append**：把 line+`\n` 合并为一次 `write_all`（消除两次 syscall 间的交错窗口）。

```rust
let mut buf = line.into_bytes();
buf.push(b'\n');
file.write_all(&buf).await?;   // 一次 write_all
file.sync_all().await?;        // 单 buffer 时无需单独 flush
```

1. **跨进程排他锁**：给 `SessionLock` 增加**阻塞式**获取（现有 `acquire` 是非阻塞 `try_lock_exclusive`，用于 `--resume` 单点检测；append 路径需要阻塞等待而非失败）。

```rust
// lock.rs 新增
impl SessionLock {
    /// 阻塞获取排他锁（append 路径用；与 `acquire` 的非阻塞语义区分）。
    pub fn acquire_blocking(path: impl Into<Utf8PathBuf>) -> Result<Self, StorageError> {
        let path = path.into();
        let file = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(path.as_std_path())?;
        file.lock_exclusive().map_err(|e| StorageError::Locked(format!("{}: {e}", path.as_str())))?; // 阻塞
        Ok(Self { file, path })
    }
}
```

在 `append` 里包一层：

```rust
fn append(...) -> ... {
    Box::pin(async move {
        let line = serde_json::to_string(&msg).map_err(...)?;
        let lock_path = self.base_dir.join(format!("{session_id}.lock"));
        // 阻塞锁在线程池执行，避免阻塞 async reactor（fs2 是同步 API）
        let _lock = tokio::task::spawn_blocking(move || SessionLock::acquire_blocking(lock_path))
            .await.map_err(|e| StorageError::Io(...))??;
        // ... open + 单次 write_all + sync_all + 更新索引（锁内）...
        Ok(()) // _lock 在此 drop → 释放
    })
}
```

1. **索引跨进程安全**：`index.json` 写入同样在会话锁内，或改为读-改-写时先获取 `index.json.lock`。最小改动：把 `update_index_on_append` 的 `save` 移到上面锁的临界区内。

**注意**：`spawn_blocking` 因为 `fs2` 是同步 API，直接 `.await` 会阻塞 reactor。`sync_all` 每次 append 都调用，高频下 IO 偏重——可在 M-13 顺带评估"turn 级批量 fsync"。

**修改点**：`storage/src/jsonl.rs`（append）、`storage/src/lock.rs`（新增 acquire_blocking）、`storage/src/index.rs`（save 入锁）。

**迁移**：无文件格式变化，纯行为修复，向后兼容。

**测试**：

- `append_concurrent_two_processes_no_interleave`：spawn 两个任务/子进程对同一 session 各 append 100 条，`load` 后应为 200 条且每行可解析。
- `append_single_write_line_has_trailing_newline`：单条 append 后文件恰一行且以 `\n` 结尾。
- `index_json_consistent_under_concurrent_appends`（索引消息数 == 实际行数）。

**文档**：`docs/security.md`（C-22 锁语义扩展到 append）、`docs/data-model.md`（写入路径）。

**约束兼容**：C-22 不放松（反而是把已有的锁用到 append 热路径）；C-13 崩溃安全保持（sync_all 仍在锁内）。

**工作量**：S（1 人日）。

---

### M-02 单坏行容错 + 格式版本号（修 S2-1）

**现状**：`load`/`load_messages_sync` 任一行坏 → `Corrupted` 整会话失败；`scan` 却跳过坏行；消息流无版本号。

**目标**：部分损坏可恢复；格式可迁移；scan/load 行为一致。

**怎么改**：

1. **统一容错读取**：`load` 改为跳过坏行 + `warn!` + 记录恢复信息，返回可读消息。同时保留"整文件全坏仍报错"的语义（与现有测试 `load_returns_error_for_corrupted_file` 兼容——该测试文件只有一行坏数据，应仍报错；新增"部分坏"用例）。

```rust
fn load(&self, session) -> ... {
    // ...
    let mut messages = Vec::new();
    let mut skipped = 0usize;
    for (idx, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() { continue; }
        // 跳过表头行（见 2）
        if line.starts_with("{\"_header\"") { continue; }
        match serde_json::from_str::<Message>(line) {
            Ok(m) => messages.push(m),
            Err(e) => { skipped += 1; tracing::warn!("skip corrupted line {}: {e}", idx + 1); }
        }
    }
    if messages.is_empty() && skipped > 0 {
        return Err(StorageError::Corrupted(format!("all {skipped} lines corrupted")));
    }
    if skipped > 0 { tracing::warn!(session=%session, skipped, "session recovered with skipped lines"); }
    Ok(messages)
}
```

1. **格式版本头**：新会话首行写 `{"_header":{"format_version":1,"app":"minicoding","app_version":"0.2.x"}}`；`load` 识别并校验：

```rust
const MESSAGE_FORMAT_VERSION: u32 = 1;
// 解析到 header 且 format_version > MESSAGE_FORMAT_VERSION →
//   Err(StorageError::FormatUnsupported("written by a newer version"))
// 无 header（旧文件）→ 视为 v1，正常解析
```

`StorageError` 新增变体（`core/src/model/error.rs`）：

```rust
#[error("session format unsupported: {0}")]
FormatUnsupported(String),
```

**修改点**：`jsonl.rs`（load/load_messages_sync/首次 append 写 header）、`core/src/model/error.rs`（新变体）、`core/src/storage/trait.rs`（如需把恢复计数透出）。

**迁移**：旧文件（无 header）按 v1 处理，无需迁移；新文件带 header。首次 append 时若文件不存在先写 header 行。

**测试**：

- `load_skips_bad_line_keeps_good`（3 行好 + 1 行坏 → 返回 3 条 + warn）。
- `load_all_corrupted_still_errors`（保留现有语义）。
- `header_written_on_first_append_and_skipped_on_load`。
- `format_version_newer_than_supported_errors`（伪造 v2 → FormatUnsupported）。
- `legacy_file_without_header_loads_as_v1`。

**文档**：`data-model.md`（消息流格式 + 版本）、`security.md`（恢复语义）。

**约束兼容**：C-13 崩溃安全增强（部分损坏不丢全部）；无放松。

**工作量**：S-M（1-2 人日）。

---

### M-03 中断后悬空 tool_calls 回填合成结果（修 D-05，本次新发现）

**现状**：`run_turn` 先落 assistant（含 tool_calls）再执行再落 tool_result；cancel/timeout 不回填；`restore_history` 不修复。

**目标**：任何中断路径下，assistant 的每个 `tool_call` 都有对应 `tool_result`（可为合成错误结果），保证 resume 后历史对严格 provider 合法。对齐 dsh"取消补合成错误结果"。

**怎么改（双层）**：

1. **主修复（源头回填）**：在 `run_turn` 的 cancel/timeout 分支，回填本 turn 最后一个 assistant 消息里**尚未得到结果**的 tool_calls：

```rust
// rt.rs：cancel 分支（以及 timeout 分支）内，TurnEnd 之前
self.backfill_missing_tool_results().await;
```

```rust
/// 为会话中"有 tool_calls 但缺 tool_result"的 assistant 消息补合成错误结果。
/// 幂等：已齐的调用跳过。用于 cancel/timeout/崩溃恢复路径（dsh 同思想）。
async fn backfill_missing_tool_results(&self) {
    // 找到最后一个含 tool_calls 的 assistant 消息
    let Some(asst) = self.session.messages.iter().rev()
        .find(|m| m.role == Role::Assistant && !m.tool_calls.is_empty()) else { return };
    // 收集已有 tool_result 的 call_id
    let answered: std::collections::HashSet<_> = self.session.messages.iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.clone())
        .collect();
    for call in &asst.tool_calls {
        if answered.contains(&call.id) { continue; }
        let msg = Self::tool_result_message(
            call.id.clone(),
            ToolResult::err_text("[interrupted] 工具调用未执行（turn 被取消/超时）"),
        );
        if self.storage.append(&self.session.id, &msg).await.is_ok() {
            self.ctx.append(msg.clone()).await;
            let ev = Event::MessageAppended(msg);
            self.persist_event(&ev).await;
            self.events.emit(ev);
        }
    }
}
```

1. **防御修复（加载侧兜底）**：在 `restore_history`（rt.rs:404）与 `replay_session_state` 之后，对历史中仍悬空的 tool_calls 做同样的回填（防"崩溃发生在 persist 之前"的极端情况）。

```rust
// restore_history 内：append 各消息后，统一修复
let repaired = repair_dangling_tool_calls(self.session.messages.clone());
for msg in repaired { self.ctx.append(msg).await; }
```

`repair_dangling_tool_calls` 是纯函数（无 IO），放 core `model` 或 `runtime`：

```rust
pub fn repair_dangling_tool_calls(mut msgs: Vec<Message>) -> Vec<Message> {
    // 对每个 assistant.tool_calls，若后续无对应 Tool 消息，则在其后插入合成 ToolResult
    // ...（保持消息相对顺序）
}
```

**修改点**：`rt.rs`（cancel/timeout 分支 + backfill + restore_history 调用 repair）、`core/src/model/`（新增 repair 纯函数）。

**迁移**：无格式变化；旧会话若已悬空，会在下次 restore 时被防御层修复。

**测试**：

- `cancel_mid_tool_backfills_synthetic_results`：模拟执行期 cancel，断言每个 tool_call 有 tool_result。
- `timeout_backfills_results`。
- `repair_dangling_tool_calls_inserts_for_missing_only`（幂等，已齐不动）。
- `resume_after_interrupt_produces_valid_history`（replay 后历史通过 provider 校验函数）。

**文档**：`design.md`（§错误与中断 / §25 懒恢复）、`rules.md`（C-13 补充"中断补齐合成结果"）。

**约束兼容**：C-13 已落盘消息不丢（是增强）；C-05 合成结果标 `is_error` 且文本为占位符，不作为指令。

**工作量**：S（1-2 人日）。

---

### M-04 SSE 分隔符归一化（修 S2-2）

**现状**：`sse.rs:40` 只匹配 `b"\n\n"`。

**目标**：兼容 `\r\n\r\n`、`\n\r\n`、`\r\n\n` 等变体。

**怎么改**：把 `take_event` 的分隔符匹配改为"`\n` 后跟可选 `\r` 再跟 `\n`"（即 `\r?\n\r?\n` 的宽松边界）：

```rust
fn take_event(&mut self) -> Option<String> {
    // 找到"空行"边界：允许行尾带 \r
    let mut i = 0;
    while i + 1 < self.buffer.len() {
        if self.buffer[i] == b'\n' {
            // 下一个非 \r 字节若是 \n，则为事件边界
            let mut j = i + 1;
            if j < self.buffer.len() && self.buffer[j] == b'\r' { j += 1; }
            if j < self.buffer.len() && self.buffer[j] == b'\n' {
                let event_bytes: Vec<u8> = self.buffer.drain(..=j).collect();
                return Some(String::from_utf8_lossy(&event_bytes).into_owned());
            }
        }
        i += 1;
    }
    None
}
```

（实现更简单稳妥的替代：先把 buffer 中 `\r\n` 归一化为 `\n` 再沿用原 `\n\n` 逻辑；但归一化会破坏原始字节，需在 `extract_data` 前做。上方按边界扫描的方式不改动字节内容，更稳。）

**修改点**：`providers/common/sse.rs`（take_event）。

**迁移**：无。

**测试**：

- `crlf_delimited_events_parsed`（`data: a\r\n\r\ndata: b\r\n\r\n` → ["a","b"]）。
- `mixed_lf_and_crlf_parsed`。
- 回归：现有 `\n\n` 用例全绿。

**文档**：`design.md` §4.3（SSE 解析容错）、`rules.md` C-12。

**约束兼容**：C-12 事件流解析容错（增强）。

**工作量**：XS（0.5 人日）。

---

### M-05 core 边界重整：领域算法下沉 + trait 注入（修 S1-1，规避循环依赖）

**现状**：core 内含 `denial.rs`/`replay.rs`/`snapshot.rs`/`worktree.rs` 等领域算法；且 `Runtime`（core）直接持有具体 `SandboxCircuitBreaker`。

**目标**：core 回归"trait + Runtime 编排 + 配置/OTel/路径"；领域算法下沉到对应领域 crate；core 不依赖领域 crate。

**关键陷阱与解法（本次新厘清）**：`denial.rs` 不能简单"剪切到 sandbox"——`Runtime`（core）用它，而 sandbox 依赖 core，直接搬会**循环依赖**。解法是 **core 定义抽象 trait，领域 crate 实现，RuntimeBuilder 注入**。

**怎么改（分 4 步）**：

1. **沙箱拒绝检测/熔断 → sandbox**：
   - core 定义抽象（`core/src/sandbox/trait.rs` 追加）：
   ```rust
   /// 沙箱拒绝跟踪（熔断）抽象：core 只依赖此 trait，具体实现在 minicoding-sandbox。
   pub trait SandboxDenialTracker: Send + Sync {
       fn record_denial(&self) -> BreakerState;
       fn state(&self) -> BreakerState;
       fn reset(&self);
   }
   // BreakerState 是枚举数据类型，留在 core（无算法）
   ```
   - `minicoding-sandbox` 实现 `SandboxCircuitBreaker`/`DenialDetector` 并从 core 删除 `denial.rs`（`DenialDetector` 若 Runtime 需要，经同一 trait 或单独 `DenialDetector` trait 注入）。
   - `Runtime` 字段改为 `sandbox_breaker: Arc<dyn SandboxDenialTracker>`，由 `RuntimeBuilder` 注入（默认 `minicoding-sandbox` 的实现，测试注入 stub）。
   - 注意 `denial.rs` 注释提到"core 依赖约束不引入 regex"——搬到 sandbox 后 sandbox 可自由引入 `regex` 做更强签名匹配。
2. **事件回放/快照 → storage**：`replay.rs`(`replay_session_state`/`session_from_messages`)、`snapshot.rs`(`SessionSnapshot`/`SessionState`/`SNAPSHOT_INTERVAL`) 是"事件/状态数据结构 + 重建算法"。trait（`EventStore`/`SnapshotStore`）与数据结构留 core，**重建算法 `replay_session_state` 移到 `minicoding-storage`**（storage 依赖 core，合法）。调用方（cli/server 的 restore/replay）改为从 storage 引入。
3. **git worktree → tools**：`agent/worktree.rs`（668 行）移到 `minicoding-tools`（或新 `minicoding-git`），core 仅留 trait（如 `WorktreeManager`）。
4. **熔断去重**：core 的 `SandboxCircuitBreaker`（C-30）与 context 的 `compress/circuit_breaker.rs`（C-29）是**两套独立熔断**。保留各自职责（沙箱 vs 压缩），但把"熔断器"通用骨架（fail/oversize 计数 + 阈值）抽为 core 的一个泛型小组件 `core::util::CircuitBreaker`，两处复用，消除重复。

**修改点**：`core/src/sandbox/*`、`core/src/storage/{replay,snapshot}.rs`、`core/src/agent/worktree.rs`、`minicoding-sandbox`、`minicoding-storage`、`minicoding-tools`、`core/src/runtime/{rt,builder}.rs`（字段 + 注入）、`core/src/lib.rs`（mod 调整）。

**迁移**：纯内部重构，无 wire/存储格式变化。分多个 PR，每个保持 `cargo build`+全测试绿。

**测试**：

- 新增**架构守卫测试**：脚本/测试断言 `minicoding-core` 不 `use minicoding_sandbox/minicoding_storage/...`（防回归）。
- `runtime_uses_injected_denial_tracker`（注入 stub，断言 record_denial 被调）。
- 现有 sandbox/storage/tools 测试随文件搬迁保持绿。

**文档**：**最先修 `architecture.md`**（把"零实现"改为准确的"core = trait + Runtime 编排 + 配置/OTel/路径 + 依赖约束受限的共享数据结构"）、`modules.md`、`AGENTS.md` §3.3/§3.4。

**约束兼容**：C-29/C-30 不放松（熔断语义不变，仅位置与注入方式变化）。

**工作量**：M-L（3-5 人日，含搬迁 + 注入改造 + 文档）。

---

## 批次 1：对标 dsh（仍未实现）

### M-06 会话 step 边界事件（R-01）

**现状**：`run_turn` 的"一次 LLM 请求 + 一组工具调用"= 一个 step（`for _iter in 0..max_iters` 的每次迭代），但 `PersistedEvent`（`event.rs`）只有 `SessionCreated/MessageAppended/PermissionResolved/PermissionModeChanged/TaskUpdated/TurnEnd`，**无 step 边界**。

**目标**：事件流记录 step 边界，使回放/恢复可定位压缩点与中断点，为 fork 打底。

**怎么改**：

1. `core/src/storage/event.rs` 的 `PersistedEvent` 增加变体（**不进入模型 transcript**，仅是落盘事件）：

```rust
/// step 开始：一次 LLM 请求 + 其触发的工具调用（第 N 次迭代）。
StepStarted { iter: u32, tool_call_ids: Vec<String> },
/// step 结束：该次迭代的工具结果已全部回灌（含中断时合成的结果，见 M-03）。
StepEnded { iter: u32 },
```

1. `SCHEMA_VERSION` 从 `1` 升到 `2`；`replay_session_state` 对旧版（无 Step 事件）跳过 Step 处理（向后兼容，因为 Step 事件只用于定位、不影响消息重建）。
2. `try_persist` 增加对新增 runtime `Event` 的映射（需先在 `runtime::Event` 加 `StepStarted/StepEnded` 或在 `run_turn` 里直接构造 `PersistedEvent`）。
3. `run_turn` 循环内：进入迭代时 persist `StepStarted{iter}`，工具结果回灌后 persist `StepEnded{iter}`。

**修改点**：`event.rs`（变体+版本）、`runtime/event.rs`（如需 runtime Event）、`rt.rs`（run_turn 循环打点）、`storage/replay.rs`（版本分支）。

**测试**：`turn_with_2_steps_persists_2_step_pairs`、`replay_handles_v1_without_step_events`、`schema_version_bumped_to_2`。

**文档**：`data-model.md`、`design.md` §25。

**约束兼容**：C-05（step 事件为 log-only，不进 transcript）；C-13 增强。

**工作量**：S。

---

### M-07 压缩引用链可追溯（R-02）

**现状**：`MessageMeta`（`message.rs`）仅 `tokens/pinned/summarized/source`，**无 `compressed_range`**；`backup_before_compress` 仅调试备份（`compress/mod.rs:54 backup: Option<Vec<Message>>`，不改 token 行为）。

**目标**：压缩摘要消息携带被替换消息的 seq 区间，审计可追溯"这轮压缩掉了什么"。

**怎么改**：

1. `MessageMeta` 增加可选字段（`#[serde(default, skip_serializing_if="Option::is_none")]` 保 wire 兼容）：

```rust
/// 本消息替代了事件 seq 区间 [from_seq, to_seq]（压缩追溯，R-02）。
pub compressed_range: Option<CompressedRange>,

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "../../minicoding-web/src/api/generated/"))]
pub struct CompressedRange {
    pub from_seq: u64,
    pub to_seq: u64,
    pub dropped_tokens: usize,
}
```

1. `context/manager.rs::compress` 各分支（截断/摘要/合并/丢弃）生成替代消息时填 `compressed_range`（用压缩前 `ContextSnapshot` 的事件 seq 边界与消息数推算）。
2. 压缩完成后经 `AuditSink::record` 落一条 `kind: Compress`（`AuditKind` 需加 `Compress` 变体）。
3. **重新生成 TS 类型**（`minicoding-web` `npm run gen-types` + `git diff --exit-code` 校验）。

**修改点**：`message.rs`、`context/manager.rs`、`core/src/storage/trait.rs`（AuditKind）、web gen-types。

**测试**：四压缩分支各断言 range 正确；audit 出现 Compress 记录；token 差与 dropped_tokens 一致（容差 5%）。

**文档**：`data-model.md`、`design.md` §3、`security.md`（审计类型）。

**约束兼容**：C-05（仅 metadata，不影响模型可见内容）。

**工作量**：S-M。

---

### M-08 循环打断器软升级（在已有 is_repeating 上对齐 dsh）

**现状**：`rt.rs` 已有硬停止版：`tool_calls_signature`+`is_repeating`，连续 ≥3 轮相同集合 → `Stopped`（终止 turn）。硬编码阈值 3，针对"整轮工具集合"，无软性提醒。

**目标**：对齐 dsh `repeat-tool-reminder`——在**硬停止之前**增加软性逐级提醒（不替换工具输出、不直接禁止），阈值可配。

**怎么改**：

1. `ToolsConfig`（`config.rs:134`）增加：

```rust
/// 重复工具调用升级阈值（如 [3,5,8]）；空数组 = 关闭软提醒，仅保留硬停止。
#[serde(default = "default_repeat_thresholds")]
pub repeat_guard_thresholds: Vec<u32>,
```

1. `run_turn` 循环里，在现有 `is_repeating`（硬停止）**之前**插入软提醒：用**单工具 (name, canonical args) 指纹**而非"整轮集合"（更灵敏），命中阈值 [3,5,8] 时向上下文注入一条 system 级提醒（`ctx.append` 一条 `Message::system_text`，或经 `Event::SystemContextAdded`），**不替换工具输出、不 return**；达到末级才走现有 `Stopped` 硬停止。
2. 指纹复用/改造 `tool_calls_signature`（改为按单个 call 计算）。

**修改点**：`config.rs`、`rt.rs`（run_turn + 指纹/提醒逻辑）。

**测试**：`repeat_3_times_injects_soft_reminder`、`repeat_8_times_hard_stops`、`different_args_resets_streak`、`thresholds_empty_disables_soft_only`（硬停止仍在）。

**文档**：`design.md` §7、`rules.md`（C-13 补充）、`api.md`（RuntimeConfig 字段）。

**约束兼容**：不替换输出（模型可见历史不失真）；硬停止保留。

**工作量**：S。

---

### M-09 沙箱拒绝结构化（R-08，升级 DenialDetector）

**现状**：`SandboxError`（`sandbox/trait.rs`）仅 `Sandbox(String)`/`Io`；`DenialDetector`（core `denial.rs`）用字符串匹配产出 `DenialMatch`，但不进入 `SandboxError`/`ToolResult.metadata`，协议层无法结构化识别。

**目标**：沙箱拒绝成为结构化事实，各协议层透传，前端渲染"沙箱拒绝"卡片。

**怎么改**：

1. `SandboxError` 增加变体：

```rust
#[error("sandbox denied ({kind}): {detail}")]
Denied { kind: SandboxDenyKind, detail: String, stderr_tail: String },
```

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to="../../minicoding-web/src/api/generated/"))]
#[serde(tag="kind", rename_all="snake_case")]
pub enum SandboxDenyKind {
    PathEscape { attempted: String, allowed_root: String },
    SyscallBlocked { syscall: String },
    WriteForbidden { path: String },
    ResourceLimit { kind: String },
    External, // macOS/Windows 兜底（签名成熟度不足，避免误判）
}
```

1. `DenialDetector`（M-05 后位于 sandbox crate，可引入 `regex` 做精确匹配）把 `DenialMatch` 升级为产出 `SandboxDenyKind`；`shell.run` 包装层把子进程退出码 + stderr 签名映射到 `Denied`。
2. `ToolResult.metadata` 增加 `sandbox_denied: Option<SandboxDenyInfo>`（wire 可选字段）；`SandboxCircuitBreaker`（M-05 的 trait）改吃结构化 `Denied` 而非文本匹配；audit 记结构化详情。
3. HTTP/NDJSON/ACP/LSP 各协议层透传 `sandbox_denied`；前端 `ToolCallCard` 渲染拒绝卡片。

**修改点**：`sandbox/trait.rs`（变体）、`sandbox/`（检测升级）、`rt.rs`/`tools`（shell 包装）、`model/tool.rs`（metadata）、协议 crate、web。

**测试**：各 DenyKind 映射单测（mock stderr）、熔断器吃结构化结果、前端拒绝卡片渲染。

**文档**：`security.md` §8、`data-model.md`、`api.md`。

**约束兼容**：C-30 加强（结构化判定替代文本匹配）；C-03 加强。

**工作量**：M。

---

### M-10 凭证重解析 + 防陈旧写（R-07）

**现状**：`openai.rs` `api_key: String` 构造时读一次入 struct（`:89`），`chat_stream` 直接用（`:153` `format!("Bearer {}", self.api_key)`）；key 轮换需重启。desktop `save_provider_config` 无并发保护。

**目标**：每次请求重解析凭证（缓存 ≤60s），换 key 零重启；配置写防陈旧。

**怎么改**：

1. providers 增加 `CredentialResolver`（缓存 provider→(key, cached_at)，≤60s 命中直返，否则重读 keyring/env；`invalidate()` 供保存后调用）：

```rust
pub struct CredentialResolver {
    cache: tokio::sync::Mutex<HashMap<String, (String, OffsetDateTime)>>,
    ttl: Duration, // 默认 60s
}
impl CredentialResolver {
    pub async fn resolve(&self, provider: &str) -> Result<Option<String>, LlmError> { /* 命中? 返 : 重读 keyring/env */ }
    pub async fn invalidate(&self, provider: &str) { /* 清缓存 */ }
}
```

1. provider 不再持有 `api_key: String`，改持 `Arc<CredentialResolver>` + provider id；`chat_stream` 内 `let key = self.resolver.resolve(&self.id).await?` 再构造 `Bearer`。
2. desktop `save_provider_config(provider, expected_revision: Option<u64>)`：`config.toml` 头存 `revision`（原子自增），不匹配返回 `StaleWrite`；`GET /config` 增加 `config_revision`。

**修改点**：`providers/common/`（新 resolver）、`openai.rs`/`anthropic.rs`/`ollama.rs`（改持有 resolver）、`desktop`（revision）、`server`（GET /config）。

**测试**：缓存命中/过期、invalidate 后重读、陈旧写被拒、revision 自增。

**文档**：`security.md`（C-04 补重解析语义）、`api.md`。

**约束兼容**：C-04 不放松（仅重解析时机变化，不落盘明文）。

**工作量**：M。

---

## 批次 2：增强

### M-11 工具输出声明 render intent（R-05）

- **现状**：`Tool` trait 仅 `name/schema/side_effect/is_read_only/execute`；`ToolResult` 自由文本/JSON，前端按约定渲染。
- **怎么改**：`Tool` trait 加 `fn output_schema(&self) -> Option<&ToolOutputSchema> { None }` 与 `fn render_output(&self, r: &ToolResult) -> RenderIntent { default }`；定义 `RenderIntent`（Text/List/Table/Code/Json）。前端本地按工具名 + schema 渲染（零协议改动）。优先给 `fs.glob`/`task.list`/`plan.list` 补。
- **测试**：render_output 纯函数单测；前端组件测试（M-14 落地后）。
- **工作量**：M。

### M-12 配置旋钮补齐 + ConfigWatcher 应用策略（R-04 收尾）

- **现状**：并行度硬编码 8（`rt.rs:1100`）、重复阈值硬编码 3；`ToolsConfig` 有真实字段但缺这两个旋钮；`ConfigWatcher` 只广播 `ConfigChanged` 不重应用。
- **怎么改**：`ToolsConfig` 加 `parallel_reads: u32`（默认 8，0 关闭并行）替换 `buffer_unordered(8)` 的硬编码；`repeat_guard_thresholds`（M-08）。对 `ConfigWatcher`：**明确不做全量热重载**（涉及 C-29 熔断与 provider 重建），但可白名单少量安全字段（如 `model`/`turn_timeout_sec`）在 turn 边界生效，其余提示重启；决策写入 `tech-stack.md` §13。
- **测试**：`parallel_reads_0_serial`、`parallel_reads_4_bounds_concurrency`、配置变更通知到达。
- **工作量**：S-M。

### M-13 存储契约测试 + 版本拒绝（R-09）

- **怎么改**：抽 `storage` 契约测试到 `minicoding-core/tests/common/storage_contract.rs`（append/load/list/delete/update_summary/M-01 并发/M-02 容错/M-06 事件流）；JSONL 与未来 SQLite 后端共享同一套断言。结合 M-02 的 `format_version` 做"更高版本显式拒绝"。顺带评估"turn 级批量 fsync"降 IO。
- **工作量**：M。

### M-14 前端回放/单测基建（R-10）

- **怎么改**：落地 Vitest + MSW；对 SSE 事件流做 record/replay 快照（对齐 dsh `DSH_SNAPSHOT` 三态），覆盖"创建会话→发消息→流式渲染→权限确认→(M-09)沙箱拒绝卡片"。CI 前端 job 加 `vitest run`。
- **工作量**：M。

### M-15 远程沙箱（R-11，远期挂起）

- `SandboxDriver` 增加能力描述（fs/network/process），`Remote` driver 指向远程 Linux 沙箱；待 M8 SDK 场景真实需求启动。

---

# 第 4 部分：项目其他需改进的方向

1. **文档纪律（最高杠杆、最低成本）**：先修 `architecture.md` 的"core 零实现"误导声明（M-05 同步）；`README` 补齐 19 crate；`features.md` 批量刷新"规划中→已实现"；建"文档-代码漂移"CI（crate 数/feature 状态/ADR 一致性自动比对）。本次重读证明旧文档已多处过时，治理优先级最高。
2. **测试基建**：前端无单测（M-14）；核心 crate（core/tools/storage/context）设覆盖率强制门禁；回放 fixture 常态化入 CI。
3. **性能**：只读并行已有（硬编码 8 → M-12 旋钮化）；压缩 token 估算精度（接真实 tokenizer，对齐 dsh tokenMeter 逐节点树）；`sync_all` 每 append 调用偏重（M-13 评估 turn 级批量）。
4. **可观测性**：确认 trace 覆盖 `session>turn>(context.build|llm.chat_stream>retry|tool.call)` 全链路并与 JSONL 经 `session.id+turn.index` 关联；补压缩熔断/沙箱拒绝/循环打断三类事件的 metric 与告警。
5. **安全纵深**：默认拒绝兜底（未显式允许的 scope/路径默认拒）；凭证重解析（M-10）；沙箱拒绝结构化（M-09）；MCP `local` 信任边界写入 `security.md`。
6. **生态/分发**：多语言 SDK（对标 dsh 的 Python 子进程 JSON-RPC SDK）；插件注册/发现机制；把 `exec` 强化为稳定 headless 评测入口（承接 dsh 的核心评测场景）。
7. **发布治理**：破坏性变更（M-01/M-05/M-06 涉及存储/格式）显式 `BREAKING` 标注 + 迁移指南；补 `cargo-deny`/license CI 门禁。

---

# 附：实施顺序总表

| 批次 | 项                     | 类型        | 工作量 | 依赖        |
| -- | --------------------- | --------- | --- | --------- |
| 0  | M-01 并发写安全            | 缺陷修复      | S   | —         |
| 0  | M-02 坏行容错+版本          | 缺陷修复      | S-M | —         |
| 0  | M-03 悬空 tool_calls 回填 | 缺陷修复（新发现） | S   | M-06 后更佳  |
| 0  | M-04 SSE CRLF         | 缺陷修复      | XS  | —         |
| 0  | M-05 core 边界重整        | 架构重构      | M-L | 先于 M-09   |
| 1  | M-06 step 边界事件        | 对标 dsh    | S   | M-02      |
| 1  | M-07 压缩引用链            | 对标 dsh    | S-M | M-06      |
| 1  | M-08 循环打断软升级          | 对标 dsh    | S   | —         |
| 1  | M-09 沙箱拒绝结构化          | 对标 dsh    | M   | M-05      |
| 1  | M-10 凭证重解析            | 对标 dsh    | M   | —         |
| 2  | M-11 render intent    | 增强        | M   | —         |
| 2  | M-12 配置旋钮             | 收尾        | S-M | M-08      |
| 2  | M-13 存储契约测试           | 健壮性       | M   | M-01/M-02 |
| 2  | M-14 前端回放测试           | 测试基建      | M   | —         |
| 2  | M-15 远程沙箱             | 远期        | XL  | M-05      |

**强烈建议先做批次 0**（M-01/M-02/M-03 是崩溃安全地基，M-05 是层纪律地基），否则后续功能建立在错误地基上。其中 M-03（悬空 tool_calls）与 M-01（并发写）是当前最可能在真实多前端使用中"咬人"的两个缺陷。

---

*本文基于 minicoding-rs v0.2.30 与 deepseek-harness @ 99f6f02 源码级独立重读；所有现状论断附 file:line。已纠正既有 v2 对比 / improvement-design / 此前分析中关于 R-06（并行已实现）、R-03（已有硬停止版）、事件溯源成熟度、[tools] 死配置、MCP local scope、storage unwrap 等过时/错误结论；并新发现 D-05 悬空 tool_calls 缺陷。*

