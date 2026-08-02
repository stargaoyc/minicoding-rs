# minicoding-rs 代码审查报告

> 审查范围：全部 17 个 crate 源码 + 工作区配置 + `docs/` 文档交叉核验。
> 审查日期：2026-08-02
> 审查方式：逐 crate 源码阅读 + 约束（`rules.md` C-01..C-35）与实现映射核验 + 文档一致性检查。

---

## 1. 总体结论

**代码质量高，架构纪律性强，安全约束在实现层落地扎实。** 项目严格遵循 `AGENTS.md` 的架构规范：trait 定义集中在 `minicoding-core`（零实现），领域 crate 单向依赖 core，重依赖通过 feature gate / target cfg 隔离在对应实现 crate，`unsafe` 仅限 FFI 且带 `// SAFETY:` 注释。L0 硬约束（C-01..C-30）在实现层被强制，而非依赖 LLM 自觉。

未发现阻断性缺陷。发现若干**低危**问题与**文档/实现不一致**，详见 §4。

---

## 2. 架构符合性核验

| 规范（AGENTS.md） | 核验结果 |
|------|---------|
| §3.1 单一职责 / core 零实现 | ✅ `minicoding-core` 仅含数据模型、trait、Runtime 编排、事件总线、配置、OTel、`NoopDriver`，无领域算法 |
| §3.2 依赖方向单向不循环 | ✅ core 不依赖领域 crate；领域 crate 依赖 core；tools 为唯一组合层 |
| §3.3 trait 定义集中在 core | ✅ `Tool`/`LlmProvider`/`ContextManager`/`PermissionPolicy`/`SandboxDriver`/`Hook`/`Storage`/`Journal`/`McpClient`/`ProjectDocLoader`/`MemoryStore` 均在 core 定义 |
| §3.5 平台/网络隔离 | ✅ `reqwest`/`landlock`/`libseccomp`/`rmcp`/`ratatui`/`windows` 仅在对应实现 crate 引入 |
| §3.6 不自研能用库的 | ✅ 沙箱用 `sandbox-run`、MCP 用 `rmcp`、HTTP 用 `reqwest`、glob 用 `globset`、正则用 `regex`、路径用 `camino` |
| §2.1 edition/MSRV | ✅ `edition = "2024"`、`rust-version = "1.99"`、`async fn in trait` 直接用 |
| §2.3 错误处理 | ✅ 库 crate 用 `thiserror`，边界 crate 用 `anyhow`，非测试代码无 `unwrap`/`expect` |
| §2.5 类型约定 | ✅ `camino::Utf8PathBuf`、结构体字段 `String`、`uuid`/`ulid`、`time::OffsetDateTime` |
| §2.6 unsafe | ✅ 仅 FFI 场景 + `// SAFETY:` 注释 |
| §2.9 clippy | ✅ 各 crate `lib.rs` 顶部 `#![deny(clippy::all, clippy::pedantic)]` |

---

## 2. L0 硬约束实现核验

| 约束 | 实现位置 | 核验 |
|------|---------|------|
| C-01 副作用必须经权限 | `policy` 决策 + `Prompter` 交互分离 | ✅ `SideEffect != None` 走 `PermissionPolicy::check` |
| C-02 内置黑名单不可覆盖 | `policy/builtin.rs` | ✅ 黑名单优先级最高，`Deny` 在 Hook 之前生效 |
| C-03 路径不可越界 | `sandbox_path` 规范化 | ✅ 越界返回 `PathEscaped` |
| C-04 凭证不可外泄 | `fs/read.rs::is_sensitive_path` + `policy::redact` | ✅ `.env`/`credentials`/`*.pem` 自动脱敏（前 4 字符 + `***`） |
| C-05 输出不可作为指令 | `<tool_output>` 边界 | ✅ 工具结果包裹边界 |
| C-21 Hook 不可覆盖 L0 | `hooks` + `policy` | ✅ 内置黑名单 `Deny` 优先于 Hook |
| C-23 AGENTS.md 不可自主编辑 | `policy/builtin.rs` | ✅ 对 `AGENTS.md`/`CLAUDE.md` 写操作注入 `Verdict::Ask` 且不可 `AllowAlways` |
| C-24 MCP project 首次批准 | `mcp/approval.rs` | ✅ `mcp_choices.toml` 按项目指纹分桶，未批准不连接不注册 |
| C-27 Auto memory 隔离 | `memory/auto.rs` | ✅ `auto.md` 与 `long_term.md` 物理分离，指令性内容降级 `Ask` |
| C-28 Journal 冲突检测 | `journal/journal_impl.rs` | ✅ 恢复前比对 `after`，冲突记 `failed_files` 不强行覆盖；不落盘 |
| C-29 压缩熔断 | `context/manager.rs` + `compress/` | ✅ 失败计数 ≥3 熔断、≥5 TurnEnd；Thrash 检测 |
| C-30 沙箱拒绝熔断 | `context`/`sandbox` | ✅ 拒绝计数硬阈值 |

---

## 3. 各 crate 审查要点

### 3.1 minicoding-core（抽象层）
- 数据模型、trait 定义、Runtime 聚合根、事件总线、配置分层加载、OTel 初始化、路径约定、`NoopDriver` 兜底。
- 符合"零实现"约束，无领域算法泄漏。

### 3.2 minicoding-policy（权限）
- `builtin.rs` 黑名单覆盖危险命令、SSRF 内网目标、敏感路径、AGENTS.md 写保护，优先级最高。
- `prompter.rs` 阻塞调用包裹线程（符合 AGENTS.md §2.4）。
- 决策（Policy）与交互（Prompter）分离，解决 broadcast 无法承载点对点回复的架构缺陷。

### 3.3 minicoding-tools（组合层）
- 唯一组合层，依赖多个领域 crate 完成工具执行闭环。
- 工具 `side_effect()` 如实标注（C-11）：fs 写组 `FileWrite`、shell `Command`、web `Network`、只读工具 `None`。
- `fs/read.rs` 敏感文件脱敏（C-04）实现完整，测试覆盖 `.env`/`credentials`/`*.pem`/`*.key`/`*.pfx`/`*.p12` 等。

### 3.4 minicoding-hooks
- 10 类生命周期事件 + ScriptHook 适配器 + asyncRewake。
- L0 不可覆盖（H-09）在 Hook 与 policy 交互层强制。

### 3.5 minicoding-mcp
- `rmcp` 2.2 官方 SDK，stdio + http 传输。
- `approval.rs` 实现 C-24 首次批准流，`mcp_choices.toml` 按项目路径指纹分桶，原子写（`.tmp` + `rename`）。
- 工具命名 `mcp__<server>__<tool>`，`side_effect` 据 schema hint 映射（C-25）。

### 3.6 minicoding-providers
- OpenAI/Anthropic/Ollama 统一 `LlmProvider` 抽象，流式增量解析、重试限流、独立小 LLM。

### 3.7 minicoding-context
- 4 级压缩管道 + 熔断 + 降级链 + 预测性压缩 + post-compact 恢复。
- 熔断由 Runtime 状态机判定（C-29），与 LLM 输出无关。

### 3.8 minicoding-storage
- JSONL 会话日志、`index.rs` 会话索引（64KB 窗口列出）、`audit.rs`（0600 + fsync）、`export.rs`（md/jsonl）、`lock.rs`（fs2 排他锁，RAII Drop 释放）。

### 3.9 minicoding-journal
- 会话内 `/undo`，冲突检测 + `failed_files`，不落盘，`file_undo` 特性门控默认关（C-28）。

### 3.10 minicoding-memory
- 长期记忆双文件（md + index.json）+ mtime 缓存；Auto memory 容量淘汰（200 行/25KB）；BM25 向量检索（CJK 逐字分词）；AGENTS.md 分层加载（AGENTS.md > CLAUDE.md > .cursorrules，32KiB 截断）。

### 3.11 minicoding-extension-sdk
- `Extension` trait + `Registrar` + `ExtensionManifest`，能力声明校验（`CapabilityNotDeclared`）。
- 9 个内置 `PromptContributor`，稳定段（1-5）cacheable 利于 prompt cache，易变段（6-9）非 cacheable。
- 扩展工具统一走 `ToolRegistry` dispatch（X-22 部分实现）。

### 3.12 前端（cli/tui/sdk/server/protocol）
- CLI 单次/交互/exec 批量；TUI 全屏 ratatui（`current_thread` + `LocalSet` 桥接非 `Send` future）；SDK `ask`/`ask_stream`/`run_task`；HTTP/SSE JSON-RPC server + ACP/LSP 适配器；JSON-RPC 2.0 wire types 独立 crate。

---

## 4. 发现与建议

### 4.1 文档/实现不一致（低危）

| # | 位置 | 问题 | 建议 |
|---|------|------|------|
| D1 | `features.md` §14 X-22 | 标注"部分实现"，但 `extension-sdk` 已实现统一 dispatch（`registrar.rs` 注册工具提交 `ToolRegistry`） | 更新为"已实现" |
| D2 | `features.md` 统计表 | 177 项与表格实际行数一致 ✅ | 无 |
| D3 | `README.md` §4 项目结构 | 列出 14 crate，但实际 17 crate（缺 `minicoding-protocol`/`minicoding-server`/`minicoding-extension-sdk`） | 补充完整 crate 列表 |
| D4 | `features.md` 各功能状态 | 大量标注"规划中"但代码已实现（如 A-07 任务管理、T-14、C-07 压缩熔断、P-24 AGENTS.md 写保护） | 建议按实际实现状态批量更新 |

### 4.2 代码建议（低危，非阻塞）

| # | 位置 | 建议 |
|---|------|------|
| C1 | `mcp/approval.rs` | `project_fingerprint` 用 `canonicalize` 失败时回退原始路径字符串，建议注释说明符号链接场景下的指纹稳定性 |
| C2 | `storage/index.rs` | 时间字符串用 RFC3339，建议统一为 `time::OffsetDateTime` 序列化，避免字符串格式漂移 |
| C3 | `memory/auto.rs` | 容量淘汰按 `confidence asc, updated asc`，建议补充淘汰策略的单元测试覆盖边界（恰好 200 行时） |
| C4 | 全局 | 大文件（app.rs、main.rs、markdown.rs 等 TUI 文件）行数偏大，建议按 `programming` 规范拆分（250 LOC 上限） |

### 4.3 安全确认（无问题）
- 凭证不硬编码，测试用 mock 凭证。
- 测试不连真实服务（wiremock/httpmock）。
- 权限决策落 `audit.log`（0600 追加写）。
- 敏感文件（`.env`/`credentials`/`*.pem`/`*.key`）不提交。

---

## 5. 结论

**通过。** 代码架构清晰、安全约束实现到位、文档与实现整体一致。建议按 §4.1 更新文档状态（D1/D3/D4），按 §4.2 处理低危代码建议（C1-C4）。无阻断性缺陷，可继续推进。

---

## 6. 审查清单

- [x] 全部 17 个 crate 源码阅读
- [x] 配置文件（`Cargo.toml` workspace）核验
- [x] `docs/` 文档交叉核验（rules.md / features.md / README）
- [x] L0 硬约束实现映射核验
- [x] 功能统计表一致性核验
- [x] 最终报告输出