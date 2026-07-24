# 开发计划（Development Plan）

本文是 `minicoding-rs` 的**任务级开发计划**，是 [`roadmap.md`](./roadmap.md) 的细化：roadmap 给里程碑（M0–M8），dev-plan 给 task。每个 task 有明确的输入/输出/验收标准/依赖/涉及功能与约束。

> **阅读约定**：
> - **涉及功能** 引用 [`features.md`](./features.md) 的 ID（A-XX/L-XX/T-XX/C-XX/M-XX/P-XX/H-XX/X-XX/O-XX/S-XX/F-XX/E-XX/Q-XX）；
> - **涉及约束** 引用 [`rules.md`](./rules.md) 的 ID（C-01..C-35）；
> - **crate** 引用 [`modules.md`](./modules.md) 的 14 个 crate；
> - 里程碑范围以 roadmap 为准，dev-plan 在 M4/M5 间做了任务归组调整（M4 吸收原 M5 的 MCP/journal，M5 聚焦 Hooks/子 Agent/Plan），仅影响任务编排顺序，不改变 roadmap 交付范围。

---

## §1 文档目的与范围

### 1.1 目的

`roadmap.md` 定义了 9 个里程碑（M0–M8）的范围、验收与风险，但粒度停在"阶段—交付物"层。本文将每个里程碑拆解为**可独立认领、可独立验收、可独立估算**的 task，供：

- 开发者按 task 拉分支、提 PR、推进看板；
- 评审者按 task 验收标准逐项检查；
- 项目经理按 task 估算与依赖排期。

### 1.2 范围

覆盖 M0–M8 全部 144 项功能（见 features.md 统计）与 35 条约束（rules.md C-01..C-35）。每个 task 标注涉及的 crate、功能 ID、约束 ID，确保功能与约束**可追溯**到具体交付单元。

### 1.3 task 字段说明

每个 task 包含以下字段：

| 字段 | 含义 |
|------|------|
| **crate** | 主要涉及的 crate（来自 modules.md §0.1） |
| **输入** | 前置 task 编号或外部依赖 |
| **输出** | 具体交付物（源码、单测、文档、配置） |
| **涉及功能** | features.md 的功能 ID |
| **涉及约束** | rules.md 的约束 ID |
| **验收标准** | 可执行检查项（命令/行为） |
| **预估工作量** | S(1-2d) / M(3-5d) / L(1-2w) |

### 1.4 与其他文档的关系

```
roadmap.md ──── 里程碑（M0-M8）范围 + 验收 + 风险
    │
    ▼
dev-plan.md ──── task（T-Mx-NN）输入/输出/验收/依赖  ← 本文档
    │
    ├── features.md ──── 功能总账（144 项 ID）
    ├── modules.md ───── crate 结构与模块树
    └── rules.md ─────── L0/L1/L2 约束（C-01..C-35）
```

---

## §2 开发流程与协作约定

### 2.1 分支策略

- **主干分支**：`main`，始终可编译、CI 全绿。
- **特性分支**：`feature/<crate>-<topic>`，如 `feature/core-agent-loop`、`feature/sandbox-landlock`。
- **修复分支**：`fix/<crate>-<issue>`，如 `fix/providers-sse-boundary`。
- **分支生命周期**：合并后删除；长期分支需同步 rebase `main`。

### 2.2 提交规范（Conventional Commits + 中文描述）

```
<type>(<scope>): <中文简述>

<可选正文，中文说明动机与影响>
```

- **type**：`feat` / `fix` / `refactor` / `test` / `docs` / `chore` / `perf` / `ci`。
- **scope**：crate 名（如 `core`、`providers`、`sandbox`）。
- **示例**：
  - `feat(core): 实现 Agent 循环主流程与停止条件`
  - `fix(sandbox): 修复 Landlock 旧内核降级路径`
  - `test(context): 补充压缩熔断状态机属性测试`

### 2.3 PR 评审 checklist

合并前必须满足：

- [ ] CI 全绿（`fmt` + `clippy -D warnings` + `test` + `audit` + `deny`）。
- [ ] 新增/修改逻辑有对应单测，覆盖率不下降。
- [ ] 涉及 L0 约束（C-01..C-07、C-21..C-30）的改动附带约束自检说明。
- [ ] 涉及公共 API 变更已更新 `CHANGELOG.md` 与 `docs/`。
- [ ] 涉及安全边界（权限/沙箱/Hook/MCP）的改动至少一名 reviewer 显式 approve。
- [ ] 性能敏感路径（Agent 循环、压缩、token 计数）附 `criterion` 基准对比，回归 > 10% 阻塞合并。
- [ ] 无凭证/密钥泄露（`cargo audit` + 人工检查 env/log）。

### 2.4 任务看板状态

| 状态 | 含义 |
|------|------|
| `pending` | 已定义 task，未认领 |
| `in_progress` | 已认领，开发中 |
| `review` | PR 已提交，待评审 |
| `blocked` | 被外部依赖/决策阻塞，需说明阻塞原因 |
| `done` | 已合并 main，验收标准全部通过 |

---

## §3 里程碑 M0：工程基础设施

> 对应 roadmap M0（3 人日）。搭建 workspace 骨架、CI、OTel、配置加载，为后续所有 task 提供地基。

#### T-M0-1 Cargo workspace 与 14 crate 骨架
- **crate**：workspace 根 + 全部 14 crate
- **输入**：无
- **输出**：`Cargo.toml`（workspace）+ `crates/minicoding-*/Cargo.toml` + 空 `lib.rs`/`main.rs`；edition 2024，MSRV 1.85
- **涉及功能**：Q-01、Q-02
- **涉及约束**：无（基础设施）
- **验收标准**：
  - `cargo build --workspace` 通过；
  - `cargo metadata` 输出包含 14 个 crate；
  - 每个 crate 的 `lib.rs` 含模块注释与 `#![warn(clippy::all)]`。
- **预估工作量**：S

#### T-M0-2 公共依赖与平台条件依赖管理
- **crate**：workspace 根
- **输入**：T-M0-1
- **输出**：`Cargo.toml` 的 `[workspace.dependencies]` 统一版本；`minicoding-sandbox` 的 `[target.'cfg(target_os = "linux")'.dependencies]` 条件引入 `landlock`/`libseccomp`；`Cargo.lock` 提交
- **涉及功能**：Q-01
- **涉及约束**：C-04（日志脱敏的依赖隔离基础）
- **验收标准**：
  - 非 Linux 平台 `cargo build -p minicoding-sandbox` 不引入 `landlock`/`libseccomp`；
  - `cargo deny check licenses` 仅允许 MIT/Apache-2.0/BSD/ISC；
  - `cargo tree -p minicoding-core` 不含 `reqwest`/`landlock`/`rmcp` 重依赖。
- **预估工作量**：S

#### T-M0-3 CI 流水线（fmt/clippy/test/audit/deny）
- **crate**：`.github/workflows/`
- **输入**：T-M0-1、T-M0-2
- **输出**：GitHub Actions workflow：`fmt` + `clippy -D warnings` + `test` + `cargo audit` + `cargo deny`；Linux/macOS/Windows 三平台 matrix
- **涉及功能**：Q-01
- **涉及约束**：无
- **验收标准**：
  - PR 触发 CI 全绿才能合并；
  - `clippy -D warnings` 零告警；
  - `cargo audit` 无已知漏洞依赖。
- **预估工作量**：S

#### T-M0-4 tracing + OpenTelemetry 初始化模板
- **crate**：core（`otel.rs`、`paths.rs`）
- **输入**：T-M0-1
- **输出**：`core::otel::init()` 初始化 `tracing-subscriber` + `tracing-opentelemetry` + `opentelemetry-otlp`；支持 `OTEL_EXPORTER_OTLP_ENDPOINT` 环境变量；无后端时降级本地 fmt 日志；采样策略 `AlwaysOn`/`TraceIdRatio` 可配
- **涉及功能**：O-01、O-02、O-05
- **涉及约束**：C-04（日志中密钥只打前 4 字符 + `***`）
- **验收标准**：
  - 设置 `OTEL_EXPORTER_OTLP_ENDPOINT` 启动后，本地 collector 能看到 `minicoding` resource；
  - 不设置时降级为本地 fmt 日志，不报错；
  - 单测覆盖采样策略选择逻辑。
- **预估工作量**：M

#### T-M0-5 anyhow 错误出口 + clap 最小骨架
- **crate**：cli（`main.rs`、`args.rs`）
- **输入**：T-M0-1
- **输出**：`minicoding-cli` 的 `clap` derive 最小骨架（仅 `--help`/`--version`）；`anyhow` 错误出口；退出码约定（0 成功 / 1 运行时错误 / 2 配置错误 / 130 中断）
- **涉及功能**：F-01（前置）
- **涉及约束**：无
- **验收标准**：
  - `cargo run -p minicoding-cli -- --help` 输出帮助文本；
  - `cargo run -p minicoding-cli -- --version` 输出版本号；
  - 未知参数返回退出码 2。
- **预估工作量**：S

#### T-M0-6 MINICODING_HOME 路径解析与配置加载
- **crate**：core（`paths.rs`、`config.rs`）
- **输入**：T-M0-1、T-M0-4
- **输出**：`core::paths::minicoding_home()` 解析 `MINICODING_HOME` 环境变量或回退默认（`~/.minicoding`）；`RuntimeConfig` 分层加载骨架（env > config.toml > 默认）
- **涉及功能**：S-06
- **涉及约束**：C-04（凭证不从配置文件明文读取）
- **验收标准**：
  - 设置 `MINICODING_HOME=/tmp/x` 后 `paths::sessions_dir()` 返回 `/tmp/x/sessions`；
  - 未设置时回退 `~/.minicoding`；
  - 单测覆盖三种来源优先级。
- **预估工作量**：S

#### T-M0-7 minicoding-sandbox crate 骨架 + NoopDriver
- **crate**：sandbox（`lib.rs`、`driver.rs`）、core（`sandbox/trait.rs`）
- **输入**：T-M0-1、T-M0-2
- **输出**：`core::sandbox::SandboxDriver` trait + `SandboxPolicy` 枚举 + `NoopDriver` 兜底实现；`minicoding-sandbox::detect_driver()` 编译期平台检测返回 `NoopDriver`（占位）
- **涉及功能**：P-15（前置骨架）
- **涉及约束**：C-22（沙箱为第二道防线，降级需显式声明——`NoopDriver` 标 `is_hardened()=false`）
- **验收标准**：
  - `core::sandbox::NoopDriver::is_hardened()` 返回 `false`；
  - `detect_driver()` 在所有平台返回 `NoopDriver`；
  - `cargo build -p minicoding-sandbox` 在 Linux/macOS/Windows 均通过。
- **预估工作量**：S

#### T-M0-8 README + docs 占位与约束自检骨架
- **crate**：workspace 根 + core
- **输入**：T-M0-1..T-M0-7
- **输出**：`README.md` 占位；`core::assert_constraints()` 骨架函数（启动校验，M0 仅占位，后续 milestone 填充）；`cargo doc --workspace` 可生成
- **涉及功能**：Q-01、Q-02
- **涉及约束**：C-01..C-07、C-21..C-30（自检清单骨架，rules.md §8）
- **验收标准**：
  - `cargo doc --workspace --no-deps` 无警告生成；
  - `assert_constraints()` 在 M0 返回 `Ok(())`（占位）。
- **预估工作量**：S

#### T-M0-9 测试基础设施骨架（Test Infra Setup）
- **crate**：workspace 根 + core（`tests/common/`）
- **输入**：T-M0-1、T-M0-2
- **输出**：
  - `crates/minicoding-core/tests/common/` 共享测试工具目录：`mod.rs` 导出 `stub` 模块（`NoopMcpClient`/`NoopHookRegistry`/`StubJournal`/`StubTaskRegistry`/`StubPermissionPolicy` 等替身，供各 milestone 独立测试用，见 `design.md` §21.3）；
  - `.cargo/config.toml` 配置 `cargo-llvm-cov` 覆盖率目标 80%；
  - `benches/` 目录骨架（`criterion` 基准占位，M2 起填充）；
  - `tests/common/fixtures/` 目录（wiremock 录制的 SSE/Anthropic 事件流 fixture 占位）；
  - `xtask/` crate（可选，用于跨 crate 集成测试编排与 fixture 生成）；
  - CI workflow 增加 `cargo llvm-cov --workspace --fail-under-lines 80` 门禁。
- **涉及功能**：Q-01、Q-02、Q-03、Q-04、Q-05、Q-06
- **涉及约束**：无（测试基础设施不涉及运行时约束）
- **验收标准**：
  - `cargo test --workspace` 能发现 `tests/common/` 下的 stub 模块并被各 crate 集成测试引用；
  - `cargo llvm-cov --workspace` 可生成覆盖率报告（M0 阶段无业务代码，覆盖率门禁不阻塞，仅验证工具链就位）；
  - `criterion` 基准占位可 `cargo bench` 空跑通过；
  - CI workflow 含 coverage 上传步骤（codecov/coveralls 可选）。
- **预估工作量**：M
- **说明**：此任务是**独立测试基础设施**，不依附于任何业务 task。后续各 milestone 的 task 验收标准中"单测/集成测试覆盖"均依赖此 task 提供的 stub 与 fixture。M3 起补充 `proptest` 策略骨架（压缩管道不变量），M4 起补充沙箱平台 CI matrix（见 `design.md` §21.4 集成测试分层递进表）。

---

## §4 里程碑 M1：MVP 单轮对话

> 对应 roadmap M1（12 人日）。交付可提问、读文件、流式输出的最小可用 CLI。

#### T-M1-1 core 数据模型与错误类型
- **crate**：core（`model/`）
- **输入**：T-M0-1
- **输出**：`Message`/`Role`/`Content`/`ToolCall`/`ToolResult`/`Session`/`SessionId`/`SessionMeta` 数据结构 + `RuntimeError`/`LlmError`/`ToolError` 错误类型；`serde` 序列化
- **涉及功能**：A-01、A-13、M-01
- **涉及约束**：C-10（工具调用 ID 唯一配对）、C-12（输出格式契约）
- **验收标准**：
  - `cargo test -p minicoding-core model::` 全过；
  - `Message` 往返 serde 不丢字段；
  - `SessionId` 为 ULID/UUID。
- **预估工作量**：M

#### T-M1-2 core 核心 trait 定义
- **crate**：core（`provider/trait.rs`、`tool/trait.rs`、`context/trait.rs`、`storage/trait.rs`、`policy/trait.rs`）
- **输入**：T-M1-1
- **输出**：`LlmProvider`/`Tokenizer` trait（`chat_stream` 返回 `BoxStream<Result<Delta>>`）；`Tool` trait（含 `side_effect()`/`is_read_only()`）；`ContextManager` trait（基础 `build_chat_request`）；`Storage` trait；`PermissionPolicy`/`PermissionPrompter` trait 骨架
- **涉及功能**：A-01、L-04、T-18
- **涉及约束**：C-01（副作用必须经权限）、C-08（工具调用符合 schema）、C-11（副作用如实标注）
- **验收标准**：
  - `Tool` trait 的 `side_effect()` 返回 `SideEffect` 枚举；
  - `LlmProvider::chat_stream` 返回 `BoxStream`；
  - 单测用 mock 实现验证 trait 可被 `Arc<dyn Trait>` 持有。
- **预估工作量**：M

#### T-M1-3 core Runtime + RuntimeBuilder 基础
- **crate**：core（`runtime.rs`、`agent/loop.rs`、`agent/accumulator.rs`）
- **输入**：T-M1-1、T-M1-2
- **输出**：`Runtime` 聚合根 + `RuntimeBuilder`；单轮 Agent 循环（仅处理一次 tool_call 便于打基础）；`DeltaAccumulator` 流式聚合
- **涉及功能**：A-01、A-13
- **涉及约束**：C-12（`stop_reason` 由 Runtime 独立判定）、C-13（单轮调用上限骨架）
- **验收标准**：
  - `RuntimeBuilder::build()` 返回可 `run_one(prompt)` 的 Runtime；
  - 单轮循环：user prompt → LLM stream → 聚合 → 若有 tool_call 执行一次 → 回灌 → EndTurn；
  - `DeltaAccumulator` 单测覆盖分片聚合边界。
- **预估工作量**：L

#### T-M1-4 providers OpenAI 兼容实现
- **crate**：providers（`openai/`）
- **输入**：T-M1-2
- **输出**：`OpenAiProvider` 实现 `LlmProvider`；`reqwest`（rustls-tls）HTTP 客户端；SSE 流解析；`ChatRequest → OpenAI JSON` 转换；`Delta` 增量解析（文本 + 工具调用分片）；密钥从 env/keyring 读取
- **涉及功能**：L-01、L-04
- **涉及约束**：C-04（密钥不下传子进程、日志脱敏）、C-12（工具调用增量 JSON 可拼接）、P-09（TLS rustls 最低 1.2）
- **验收标准**：
  - `wiremock` 录制真实 OpenAI 响应做 fixture，单测全过；
  - SSE 边界 case（分片、空 data、`[DONE]`）覆盖；
  - 工具调用分片聚合后产出合法 `ToolCall`；
  - 密钥不出现在 `tracing` 日志。
- **预估工作量**：L

#### T-M1-5 providers tiktoken-rs Tokenizer
- **crate**：providers（`openai/tokenizer.rs`）、context（`tokenizer.rs` 骨架）
- **输入**：T-M1-2
- **输出**：`tiktoken-rs` 封装 `Tokenizer` trait 实现；token 计数 + 启发式估算回退
- **涉及功能**：C-01（Token 预算基础）、L-04
- **涉及约束**：C-07（资源不可耗尽——token 计数为预算基础）
- **验收标准**：
  - `cargo test -p minicoding-providers tokenizer` 全过；
  - 已知文本 token 计数与 `tiktoken` Python 版一致；
  - 模型不存在时启发式估算误差 < 10%。
- **预估工作量**：S

#### T-M1-6 tools 只读工具组（fs.read/list/glob/grep）
- **crate**：tools（`fs/read.rs`、`fs/list.rs`、`fs/glob.rs`、`fs/grep.rs`、`util/path.rs`）
- **输入**：T-M1-2
- **输出**：`fs.read`（支持行范围）、`fs.list`、`fs.glob`（globset + ignore）、`fs.grep`（regex + ignore）实现 `Tool` trait，`SideEffect::None`；`util::path` 委托 `sandbox_path` 校验
- **涉及功能**：T-01、T-02、T-03、T-04
- **涉及约束**：C-03（路径不可越界）、C-07（输出字节上限）、C-08（输入 schema 校验）、C-11（副作用如实标注为 None）
- **验收标准**：
  - `cargo test -p minicoding-tools fs::` 全过；
  - `fs.read "../../etc/passwd"` 返回 `PathEscaped` 错误；
  - `fs.read` 支持超大文件截断；
  - `fs.glob` 尊重 `.gitignore`。
- **预估工作量**：M

#### T-M1-7 policy 应用层路径沙箱 sandbox_path
- **crate**：policy（`path_sandbox.rs`、`lib.rs`）
- **输入**：T-M1-2
- **输出**：`sandbox_path::resolve_under(workdir, input)` 规范化校验；越界返回 `PathEscaped`；符号链接绕过检测
- **涉及功能**：P-06
- **涉及约束**：C-03（路径不可越界，第一道防线）
- **验收标准**：
  - `cargo test -p minicoding-policy path_sandbox` 全过；
  - `../../etc/passwd`、绝对路径越界、符号链接绕过均被拒；
  - 工作区内合法路径通过。
- **预估工作量**：S

#### T-M1-8 storage JSONL 会话日志
- **crate**：storage（`jsonl.rs`、`lib.rs`）
- **输入**：T-M1-2
- **输出**：`JsonlStorage` 实现 `Storage` trait；追加写 + `fsync` 崩溃安全；惰性物化（首条消息时才创建会话文件）
- **涉及功能**：S-01、A-13
- **涉及约束**：C-07（资源不可耗尽）、C-04（凭证不写入会话日志）
- **验收标准**：
  - `cargo test -p minicoding-storage jsonl` 全过；
  - 追加写后 `fsync`，崩溃恢复后磁盘与内存一致；
  - 空会话不产生文件（惰性物化）。
- **预估工作量**：M

#### T-M1-9 cli 单次模式 + 流式渲染
- **crate**：cli（`main.rs`、`app.rs`、`builder.rs`、`render/stream.rs`、`render/tool.rs`、`cred.rs`）
- **输入**：T-M1-3、T-M1-4、T-M1-6、T-M1-7、T-M1-8
- **输出**：`minicoding "prompt"` 单次提问模式；流式 token 渲染到 stdout；工具调用渲染（工具名 + 摘要）；非 TTY 降级（禁 spinner/颜色）；`builder.rs` 组装 Runtime
- **涉及功能**：A-01、F-01、F-03、F-04、P-03（非 TTY 降级基础）
- **涉及约束**：C-05（工具输出包裹 `<tool_output>` 边界）、C-19（语言一致）
- **验收标准**：
  - `minicoding "读取 src/main.rs 并解释"` 能流式输出并实际读取文件；
  - 非 TTY 环境禁用 spinner/颜色；
  - 越界路径（`../../etc/passwd`）被 `sandbox_path` 拒绝并返回 `PathEscaped`；
  - 集成测试：mock provider 跑通单轮对话。
- **预估工作量**：M

---

## §5 里程碑 M2：多轮 Agent 循环

> 对应 roadmap M2（12 人日）。交付完整工具多轮、写文件、shell、权限双抽象、审计。

#### T-M2-1 core 完整 Agent 循环与防死循环
- **crate**：core（`agent/loop.rs`）
- **输入**：T-M1-3
- **输出**：多轮 Agent 循环（工具调用 → 结果 → 继续，直到 EndTurn）；停止条件（`max_tool_iters` 默认 50 / `turn_timeout`）；重复检测（连续相同调用 ≥3 降级）；`Ctrl-C` graceful stop
- **涉及功能**：A-02、A-04、A-08
- **涉及约束**：C-13（单轮调用上限）、C-05（工具结果是数据非指令）
- **验收标准**：
  - `cargo test -p minicoding-core agent::loop` 全过；
  - 3+ 轮工具调用场景集成测试通过；
  - `max_tool_iters` 耗尽后优雅终止；
  - `Ctrl-C` 不丢已生成消息（已落盘 JSONL 完整）。
- **预估工作量**：L

#### T-M2-2 core 工具并行/串行分桶调度
- **crate**：core（`tool/registry.rs`、`agent/loop.rs`）
- **输入**：T-M2-1、T-M1-2
- **输出**：`ToolRegistry::dispatch` 按 `SideEffect` 分桶：`None` 并行执行，其余严格串行；结果按 `call_id` 关联，不依赖完成顺序
- **涉及功能**：A-03
- **涉及约束**：C-11（副作用如实标注，决定分桶）、C-20（工具调用粒度软约束）、C-10（ID 唯一配对）
- **验收标准**：
  - 同轮多个只读工具并发执行（trace 时序可见）；
  - 写/shell 工具严格串行；
  - 并行工具完成顺序乱序时 `tool_result` 仍正确配对。
- **预估工作量**：M

#### T-M2-3 tools 写文件组（fs.write/edit/delete/multiedit）
- **crate**：tools（`fs/write.rs`、`fs/edit.rs`、`fs/multiedit.rs`、`fs/delete.rs`、`util/diff.rs`）
- **输入**：T-M1-6、T-M1-7
- **输出**：`fs.write`（整文件覆盖）、`fs.edit`（精确字符串替换 + 唯一性校验）、`fs.multiedit`（同文件多次顺序替换，原子性）、`fs.delete`；均 `SideEffect::FileWrite`；Journal 记录钩子（M5 接入，此处预留 trait 调用点）
- **涉及功能**：T-05、T-06、T-06b、T-07
- **涉及约束**：C-01（副作用必须经权限）、C-03（路径不可越界）、C-11（如实标注 FileWrite）
- **验收标准**：
  - `cargo test -p minicoding-tools fs::write` / `fs::edit` / `fs::multiedit` / `fs::delete` 全过；
  - `fs.edit` 唯一性冲突返回清晰错误并建议增大上下文；
  - `fs.multiedit` 中间步骤失败时整体回滚（原子性）；
  - 越界写被 `sandbox_path` 拒绝。
- **预估工作量**：L

#### T-M2-4 tools shell.run（超时+截断+黑名单）
- **crate**：tools（`shell/run.rs`）、policy（`builtin.rs` 基础）
- **输入**：T-M1-2、T-M1-7
- **输出**：`shell.run` 实现 `Tool` trait，`SideEffect::Command`；`tokio::process` 执行 + 超时 + 输出截断；预留 `SandboxDriver::apply` 调用点（M4 接入）
- **涉及功能**：T-08
- **涉及约束**：C-01（副作用经权限）、C-04（凭证不下传子进程——env 过滤）、C-07（超时+截断+进程组）
- **验收标准**：
  - `cargo test -p minicoding-tools shell::run` 全过；
  - 超时后进程组被 kill；
  - 输出超过上限被截断并标注；
  - env 不含 `OPENAI_API_KEY` 等凭证变量。
- **预估工作量**：M

#### T-M2-5 core PermissionPolicy + PermissionPrompter 双抽象
- **crate**：core（`policy/trait.rs`）、policy（`policy.rs`、`mode.rs`、`store.rs`）
- **输入**：T-M1-2
- **输出**：`PolicyEngine` 实现 `PermissionPolicy`；`Verdict`（Allow/Ask/Deny）/`Decision` 枚举；`policy.toml` 持久化（AllowAlways/DenyAlways）；`PermissionMode` 基础
- **涉及功能**：P-01、P-04
- **涉及约束**：C-01（副作用必须经权限）、C-02（内置黑名单不可覆盖——骨架）
- **验收标准**：
  - `cargo test -p minicoding-policy policy` 全过；
  - `AllowAlways` 持久化到 `policy.toml` 后重启仍生效；
  - `Deny` 优先级高于 `AllowAlways`。
- **预估工作量**：M

#### T-M2-6 policy InteractivePrompter + NonInteractivePrompter + 内置黑名单
- **crate**：policy（`prompter/interactive.rs`、`prompter/non_interactive.rs`、`builtin.rs`、`risk.rs`）
- **输入**：T-M2-5
- **输出**：`InteractivePrompter`（CLI TTY 交互）、`NonInteractivePrompter`（非 TTY 策略化 deny/allow/fail）；`builtin.rs` 硬编码危险命令/SSRF/敏感路径黑名单（最高优先级）；`risk.rs` 命令风险解释
- **涉及功能**：P-02、P-03、P-05、P-07
- **涉及约束**：C-02（内置黑名单不可覆盖）、C-04（凭证脱敏）、C-07（资源约束）
- **验收标准**：
  - `cargo test -p minicoding-policy builtin` 全过；
  - `rm -rf /`、`sudo`、`dd of=/dev/`、fork bomb 被黑名单拒绝；
  - 非 TTY 默认 `deny` 副作用工具；
  - 黑名单 `Deny` 无法被 `AllowAlways` 覆盖。
- **预估工作量**：M

#### T-M2-7 storage audit.log 审计落盘
- **crate**：storage（`audit.rs`）
- **输入**：T-M1-8、T-M2-5
- **输出**：`AuditSink` 实现；`audit.log` JSONL 追加写，文件权限 0600；记录 allow/deny/tool_call/permission 决策；不可篡改（无 update/delete API）
- **涉及功能**：P-13、O-06（Event → audit）
- **涉及约束**：C-01（审计副作用经权限决策）、C-04（凭证不写入审计）
- **验收标准**：
  - `cargo test -p minicoding-storage audit` 全过；
  - 审计文件权限 0600；
  - 每次工具调用产生一条审计记录（含 verdict/decision）；
  - 无 update/delete API。
- **预估工作量**：S

#### T-M2-8 cli 交互会话 + 权限确认 + Ctrl-C
- **crate**：cli（`session/interactive.rs`、`render/prompt.rs`）
- **输入**：T-M2-1、T-M2-6、T-M2-7
- **输出**：`--session` REPL 交互模式；权限确认提示渲染；`Ctrl-C` graceful stop；工具调用进度渲染
- **涉及功能**：A-08、F-02
- **涉及约束**：C-01（权限确认交互）、C-15（改动可解释——渲染意图）
- **验收标准**：
  - `minicoding "把 utils.rs 里的 foo 改名为 bar"` 能完成读取→编辑→验证闭环；
  - 副作用工具触发权限确认提示；
  - `Ctrl-C` 不丢已生成消息；
  - 非 TTY 走 `NonInteractivePrompter`。
- **预估工作量**：M

#### T-M2-9 core EventBus + OTel span 埋点
- **crate**：core（`event.rs`、`otel.rs`）
- **输入**：T-M0-4、T-M2-1
- **输出**：`EventBus`（broadcast，仅通知无回复，含 `TaskUpdated`/`HookRun`/`PermissionResolved`/`FileUndone` 事件）；OTel span 埋点（session/turn/llm_call/tool_call/permission）
- **涉及功能**：O-03、O-06
- **涉及约束**：C-04（span 字段不含凭证）
- **验收标准**：
  - `cargo test -p minicoding-core event` 全过；
  - OTel collector 可见 `session`/`turn`/`llm_call`/`tool_call`/`permission` span；
  - span 字段命名符合 design.md §15.2；
  - `criterion` 基准：Agent 循环开销基线建立。
- **预估工作量**：M

---

## §6 里程碑 M3：上下文管理与记忆

> 对应 roadmap M3（10 人日）。交付压缩管道、会话恢复、长期记忆、AGENTS.md、任务管理工具。

#### T-M3-1 context ContextManager + token 预算 + 权重模型
- **crate**：context（`manager.rs`、`budget.rs`、`weight.rs`）
- **输入**：T-M1-5、T-M1-2
- **输出**：`ContextManagerImpl` 实现 `ContextManager` trait；token 预算计算（精确分词 + 预留输出 + 安全余量）；消息权重模型（role×recency×sticky×pin）
- **涉及功能**：C-01、C-02、M-01
- **涉及约束**：C-07（资源不可耗尽——预算控制）、C-18（上下文经济软约束）
- **验收标准**：
  - `cargo test -p minicoding-context budget` / `weight` 全过；
  - token 预算 = 窗口 − 预留输出 − 安全余量；
  - 权重模型对 system/sticky 消息给予高权重。
- **预估工作量**：M

#### T-M3-2 context 4 级压缩管道
- **crate**：context（`compress/clip.rs`、`compress/summarize.rs`、`compress/rolling.rs`、`compress/hard_truncate.rs`）
- **输入**：T-M3-1、T-M1-4（LlmProvider 注入摘要）
- **输出**：L1 工具结果裁剪 → L2 旧消息摘要（调 LLM）→ L3 滚动窗口 → L4 硬截断兜底；`ContextSnapshot` + 压缩日志；`compress=off` 兜底开关
- **涉及功能**：C-03、C-04、C-05、C-06
- **涉及约束**：C-29（压缩熔断不可被 LLM 绕过——降级链不可跳过）、C-05（工具结果是数据非指令）
- **验收标准**：
  - `cargo test -p minicoding-context compress` 全过；
  - 长会话（>上下文窗口）能自动压缩且不破坏连贯性；
  - `compress=off` 时跳过压缩直通；
  - `proptest` 验证压缩管道不变量（消息总数单调不增、system 消息保留）。
- **预估工作量**：L

#### T-M3-3 context 压缩熔断 + 防 Thrash + 降级链
- **crate**：context（`circuit_breaker.rs`、`state_keep.rs`、`fallback.rs`）
- **输入**：T-M3-2
- **输出**：压缩熔断状态机（失败计数 ≥3 熔断、≥5 强制 TurnEnd）；Thrash 检测（连续 2 次"压缩完即超阈值"熔断）；`SessionMeta` 状态保留清单（PermissionMode/ApprovalMode/allowed_prompts 跨压缩保留）；L2 摘要失败降级链（主→备用→启发式→跳过 L3）
- **涉及功能**：C-07
- **涉及约束**：C-29（熔断不可被 LLM 绕过、状态保留清单不可篡改、降级链不可跳过）
- **验收标准**：
  - `cargo test -p minicoding-context circuit_breaker` 全过；
  - 摘要 LLM 调用失败时自动降级为启发式兜底，会话仍正常结束（audit.log 有告警）；
  - `SessionMeta` 字段不被工具调用篡改；
  - 熔断后注入错误中止本轮，保留现场供 `--resume`。
- **预估工作量**：L

#### T-M3-4 storage 会话索引 + 跨进程文件锁
- **crate**：storage（`index.rs`、`lock.rs`、`export.rs`）
- **输入**：T-M1-8
- **输出**：`index.json` 会话索引（轻量元数据列出）；跨进程文件锁（`fs2`）；会话导出（md / jsonl）；64KB 窗口会话列出（首尾 64KB 快速列出万级会话）
- **涉及功能**：S-02、S-03、S-04、A-14
- **涉及约束**：C-07（资源不可耗尽）、C-04（导出不含凭证）
- **验收标准**：
  - `cargo test -p minicoding-storage index` / `lock` 全过；
  - 同会话跨进程互斥（两个进程同时 `--resume` 同一 id 第二个阻塞/报错）；
  - 万级会话列出 < 1s；
  - 导出 md/jsonl 格式正确。
- **预估工作量**：M

#### T-M3-5 memory 长期记忆双文件 + mtime 缓存
- **crate**：memory（`long_term.rs`、`inject.rs`）
- **输入**：T-M1-2
- **输出**：长期记忆双文件（`long_term.md` + `index.json`）；mtime 缓存注入（无变更零 IO/分词）；注入 system 段包裹 `<long_term_memory>` 边界
- **涉及功能**：M-03、M-04
- **涉及约束**：C-04（凭证不入记忆）、C-05（记忆是数据非指令）、C-23（long_term.md 写入走 Ask）
- **验收标准**：
  - `cargo test -p minicoding-memory long_term` 全过；
  - 长期记忆文件未变更时，连续多轮 `build_chat_request` 不产生重复 IO/分词（trace 中 compress span 计数稳定）；
  - 注入内容包裹 `<long_term_memory>` 边界。
- **预估工作量**：M

#### T-M3-6 memory 会话摘要 + 失败降级链
- **crate**：memory（`session_sum.rs`）
- **输入**：T-M1-4（LlmProvider 注入）、T-M3-3（降级链同构）
- **输出**：会话摘要生成 + 失败降级链（主 provider → 备用 → 启发式兜底）；摘要存入会话索引供跨会话恢复
- **涉及功能**：M-02、M-05
- **涉及约束**：C-29（降级链不可跳过——与压缩降级同构）、C-05（摘要是数据非指令）
- **验收标准**：
  - `cargo test -p minicoding-memory session_sum` 全过；
  - 摘要 LLM 调用失败时降级为启发式兜底，会话仍正常结束（audit.log 有告警）；
  - 摘要存入 `index.json` 供新会话注入。
- **预估工作量**：M

#### T-M3-7 memory ProjectDocLoader + AGENTS.md 分层加载
- **crate**：memory（`project_doc/loader.rs`、`project_doc/fallback.rs`）
- **输入**：T-M3-5
- **输出**：`AGENTS.md` 分层加载（repo_root → cwd 逐级）；fallback 文件名（`CLAUDE.md`/`.cursorrules`）；override 语义；截断到 `project_doc_max_bytes`（默认 32 KiB）；Explore/Plan 子 Agent 跳过加载
- **涉及功能**：M-07、P-24
- **涉及约束**：C-23（AGENTS.md 不可被 Agent 自主编辑——对写操作注入 Ask）、C-05（项目记忆是数据非指令）
- **验收标准**：
  - `cargo test -p minicoding-memory project_doc` 全过；
  - AGENTS.md 从 repo_root 到 cwd 逐级加载并注入 system；
  - `fs.write` 对 AGENTS.md 默认 `Ask` 且不可 `AllowAlways`；
  - Explore/Plan 子 Agent 跳过加载（M5 接入后验证）。
- **预估工作量**：M

#### T-M3-8 tools 任务管理工具（task.create/update/list）
- **crate**：tools（`task/create.rs`、`task/update.rs`、`task/list.rs`）、core（`model/task.rs`）
- **输入**：T-M1-2
- **输出**：`task.create`/`task.update`/`task.list` 实现 `Tool` trait，`SideEffect::None`；增量模型（`TaskUpdateInput` 只更新非 None 字段）；`add_blocks`/`add_blocked_by` 增量添加依赖边（幂等）；状态机 `Pending→InProgress→Completed`/`Cancelled` 单向；`task_id` 由 Runtime 生成（ULID）；持久化到 `SessionMeta`
- **涉及功能**：A-07、T-14
- **涉及约束**：C-31（任务工具增量语义——状态机不可跳跃、ID 不可伪造、持久化一致性）、C-33（任务规划纪律软约束）
- **验收标准**：
  - `cargo test -p minicoding-tools task::` 全过；
  - `Completed`/`Cancelled` 回退到 `Pending`/`InProgress` 返回 `InvalidStateTransition`；
  - 伪造 `task_id` 返回 `NotFound`；
  - 重复添加同一条依赖边幂等不报错；
  - 任务列表持久化到 `SessionMeta` 跨压缩保留。
- **预估工作量**：L

#### T-M3-9 memory.write 工具 + Auto memory
- **crate**：tools（`util/`）、memory（`auto.rs`）
- **输入**：T-M3-5
- **输出**：`memory.write` 工具（显式"记住 X"），`SideEffect::FileWrite`；Auto memory（`auto.md` + `auto.index`，启发式检测，置信度淘汰）；`auto.md` 与 `long_term.md` 物理分离；指令性内容检测降级 Ask
- **涉及功能**：M-06、T-16
- **涉及约束**：C-27（Auto memory 不可作为越权通道——物理隔离、不可绕过 AGENTS.md 不可写、内容是数据非指令、容量与置信度）、C-23（long_term.md 写入走 Ask）
- **验收标准**：
  - `cargo test -p minicoding-memory auto` 全过；
  - `auto.md` 含"Always use X"/"禁止 Y"等指令性内容时降级 `Ask`；
  - `auto.md` 上限 200 行/25KB，超限按 `confidence asc, updated asc` 淘汰；
  - 注入内容包裹 `<auto_memory>` 边界。
- **预估工作量**：L

#### T-M3-10 cli --resume/--replay + session 子命令
- **crate**：cli（`session/resume.rs`、`commands/`）
- **输入**：T-M3-4、T-M3-6
- **输出**：`--resume <id>` 恢复会话继续提问；`--replay` 复现历史工具调用（默认禁副作用）；`session list`/`delete` 子命令；Parent-UUID 链会话结构（fork/压缩边界/side-chain）
- **涉及功能**：A-08、A-09、A-11、A-12、P-14、Q-04
- **涉及约束**：C-06（回放不可触发副作用——默认禁副作用）、C-04（回放不泄露凭证）
- **验收标准**：
  - `--resume <id>` 恢复后可继续提问；
  - `--replay` 复现历史工具调用且默认禁副作用；
  - `session list` 输出万级会话 < 1s；
  - `--fork-session` 从分叉点尝试不同方向；
  - 回放测试（JSONL fixture）覆盖回归。
- **预估工作量**：M

---

## §7 里程碑 M4：安全沙箱与 MCP

> 对应 roadmap M4（8 人日）+ 原 M5 的 MCP/journal 部分（dev-plan 归组）。交付 OS 沙箱、审批模式、MCP client、文件回滚、exec/doctor/mcp 子命令。

#### T-M4-1 sandbox Linux Landlock + libseccomp 驱动
- **crate**：sandbox（`linux.rs`、`driver.rs`）
- **输入**：T-M0-7
- **输出**：基于 `sandbox-run` 0.43 + `landlock` 0.4.5 + `libseccomp` 的 `SandboxDriverImpl`（Linux）；`apply_sandbox()` 在子进程 fork 后 exec 前调用；`ProtectSystem`/`ReadWritePaths`/`PrivateNetwork` 高级选项配置；运行时 `landlock_available()` 探测内核支持
- **涉及功能**：P-15、P-16
- **涉及约束**：C-22（沙箱为第二道防线，降级需显式声明）、C-30（沙箱拒绝是内核级硬反馈不可被应用层覆盖）
- **验收标准**：
  - `cargo test -p minicoding-sandbox linux`（Linux CI）全过；
  - `--sandbox read-only` 下写操作被 Landlock 拦（EPERM）；
  - `--sandbox workspace-write` 下越界写、网络外联被拦；
  - 旧内核（< 5.13）降级 `NoopDriver` + warn；
  - `is_hardened()` 在 Landlock 可用时返回 `true`。
- **预估工作量**：L

#### T-M4-2 sandbox macOS Seatbelt + Windows + ExternalSandbox
- **crate**：sandbox（`macos.rs`、`windows.rs`、`lib.rs`）
- **输入**：T-M4-1
- **输出**：macOS `sandbox-run`（原生 Seatbelt 框架）；Windows 受限令牌 + Job Object（`windows` crate，可降级）；`ExternalSandbox` 策略（CI/容器场景，`NoopDriver` + info 日志声明依赖外部隔离）
- **涉及功能**：P-15、P-16
- **涉及约束**：C-22（ExternalSandbox/DangerFullAccess 需显式声明）
- **验收标准**：
  - macOS CI：`--sandbox read-only` 下写被 Seatbelt 拦；
  - Windows：受限令牌生效或降级应用层并标注 "non-hardened"；
  - `--sandbox external-sandbox` 在容器内不报沙箱初始化失败，日志声明依赖外部隔离。
- **预估工作量**：L

#### T-M4-3 sandbox pre-main 进程硬化 + VCS 目录保护
- **crate**：sandbox（`hardening.rs`）
- **输入**：T-M4-1
- **输出**：pre-main 进程硬化（`PR_SET_DUMPABLE=0`/`RLIMIT_CORE=0`/清 `LD_*`）；`.git`/`.hg`/`.svn` 默认只读保护（通过 `sandbox-run` 的 `ReadOnlyPaths`）
- **涉及功能**：P-20、P-21
- **涉及约束**：C-22（VCS 保护是沙箱一部分）、C-04（清 `LD_*` 防注入）
- **验收标准**：
  - Linux：`/proc/self/status` 显示 `Dumpable: 0`；
  - `core` dump 被禁；
  - `--sandbox workspace-write` 下 `.git` 目录默认拒绝写入（除非 `allow_dotgit_write=true`）。
- **预估工作量**：M

#### T-M4-4 policy SandboxPolicy 四模式 + ApprovalMode + Preset
- **crate**：policy（`mode.rs`）、core（`sandbox/trait.rs`）
- **输入**：T-M0-7、T-M2-5
- **输出**：`SandboxPolicy` 四模式（ReadOnly/WorkspaceWrite/ExternalSandbox/DangerFullAccess）；`ApprovalMode`（Untrusted/OnFailure/OnRequest/Never）；预设（read-only/auto/external-sandbox/full-access）解析为默认 `Verdict` + `SandboxPolicy`
- **涉及功能**：P-16、P-17、P-18
- **涉及约束**：C-22（DangerFullAccess 启动时强制 red 警告 + 二次确认）
- **验收标准**：
  - `cargo test -p minicoding-policy mode` 全过；
  - `--preset full-access` 启动时打 red 警告并要求显式确认；
  - 预设展开为正确的 `SandboxPolicy` + `ApprovalMode` 组合；
  - `doctor --security` 输出沙箱驱动类型与硬化状态。
- **预估工作量**：M

#### T-M4-5 core 沙箱拒绝检测与升级流 + 拒绝熔断
- **crate**：core（`agent/loop.rs`）、policy（`risk.rs`）
- **输入**：T-M4-1、T-M2-5
- **输出**：沙箱拒绝检测（识别 EPERM/ENOSYS/Seatbelt denial/Landlock denial → denial 签名库）；升级流（请求批准 → 放宽策略重试，走 `PermissionPrompter`）；沙箱拒绝熔断器（单 turn ≥3 次提醒、≥5 次 TurnEnd）
- **涉及功能**：P-19、P-23
- **涉及约束**：C-30（沙箱拒绝熔断不可被 LLM 绕过——拒绝是内核级硬反馈、升级流不绕过权限、拒绝计数器每 turn 重置）
- **验收标准**：
  - `cargo test -p minicoding-core sandbox_reject` 全过；
  - Landlock EPERM 被识别为沙箱拒绝而非裸错误；
  - 拒绝后升级为权限请求，用户可拒绝；
  - 单 turn 5 次拒绝后强制 TurnEnd 回灌错误总结；
  - `audit.log` 记录拒绝与升级决策。
- **预估工作量**：L

#### T-M4-6 tools shell.run/fs.write 受沙箱约束
- **crate**：tools（`shell/run.rs`、`fs/write.rs`、`fs/edit.rs`、`fs/delete.rs`）
- **输入**：T-M4-1、T-M2-3、T-M2-4
- **输出**：`shell.run` 执行前调 `SandboxDriver::apply`；`fs.write/edit/delete` 受沙箱约束；denial 被捕获转为升级流
- **涉及功能**：T-08、T-05、T-06、T-07
- **涉及约束**：C-22（沙箱为第二道防线）、C-30（拒绝检测）
- **验收标准**：
  - `--sandbox read-only` 下 `fs.write`/`shell.run` 被沙箱拦；
  - denial 被捕获并走升级流而非裸错误；
  - `audit.log` 记录工具名 + 拒绝原因。
- **预估工作量**：M

#### T-M4-7 mcp McpClient trait + rmcp client（stdio）
- **crate**：mcp（`client/rmcp.rs`、`client/lifecycle.rs`）、core（`mcp/trait.rs`）
- **输入**：T-M0-1、T-M1-2
- **输出**：基于 `rmcp` 2.2 的 `McpClient` 实现（stdio 传输）；启动/握手/超时/优雅关闭；`list_tools`/`call`/`shutdown`；凭证隔离（子进程 env 不含凭证）
- **涉及功能**：X-01、X-02、X-03（stdio 部分）
- **涉及约束**：C-04（MCP server 子进程不继承凭证环境变量）、C-08（工具调用符合 schema）
- **验收标准**：
  - `cargo test -p minicoding-mcp client` 全过；
  - stdio MCP server 能连接、`list_tools`、`call`、`shutdown`；
  - MCP server 子进程 env 不含 `OPENAI_API_KEY`；
  - `required=true` 的 server 启动失败则 minicoding 拒绝启动。
- **预估工作量**：L

#### T-M4-8 mcp 工具命名 + 包装 + project 批准流
- **crate**：mcp（`client/wrapper.rs`、`naming.rs`、`approval.rs`）、tools（`mcp/wrapper.rs`）
- **输入**：T-M4-7、T-M1-2
- **输出**：`mcp__<server>__<tool>` 命名 + 权限通配匹配；`mcp::wrapper` 把远程工具包装为本地 `Tool`（`side_effect` 据 `readOnlyHint`/`destructiveHint` 映射）；三作用域配置（local/project/user，`mcp.json`）；project 作用域首次批准流（`mcp_choices.toml`）
- **涉及功能**：X-04、X-05、X-06、X-07、X-08、T-17
- **涉及约束**：C-24（MCP project 作用域 server 必须经首次批准——防恶意仓库植入）、C-25（MCP 工具只读性据 server schema 声明，未声明默认 Command）、C-09（工具名必须已注册）
- **验收标准**：
  - `cargo test -p minicoding-mcp naming` / `approval` / `wrapper` 全过；
  - 远程工具以 `mcp__<server>__<tool>` 注册；
  - 未声明 hint 的 MCP 工具默认 `SideEffect::Command`（串行 + Ask）；
  - 含 `.minicoding/mcp.json` 的仓库首次进入时逐 server 弹窗批准，结果落 `mcp_choices.toml`；
  - 未批准的 project 作用域 server 不连接、不注册工具。
- **预估工作量**：L

#### T-M4-9 journal FileChangeJournal + /undo
- **crate**：journal（`journal.rs`、`entry.rs`、`undo.rs`、`report.rs`）、tools（`fs/write.rs` 等接入）
- **输入**：T-M2-3
- **输出**：`FileChangeJournal` 实现 `Journal` trait（内存，不落盘）；`ChangeEntry`/`FileChange` 数据结构；`/undo` operation 级回滚 + 冲突检测（恢复前比对 `after`，不一致记入 `failed_files`）；`fs.write/edit/delete` 成功后调 `Journal::record`（仅 `file-undo=true` 时）
- **涉及功能**：S-07、A-10
- **涉及约束**：C-28（FileChangeJournal 不可绕过权限回滚——撤销不重新授权但记审计、冲突检测不可强行覆盖、不落盘、不可越界恢复、不可回滚跨会话）
- **验收标准**：
  - `cargo test -p minicoding-journal` 全过；
  - `/undo` 能回滚最近一次 operation 的文件改动；
  - 文件被外部编辑后 `/undo` 记入 `failed_files` 不强行覆盖；
  - journal 仅驻留内存，会话结束即销毁；
  - 恢复路径经 `sandbox_path` 校验；
  - `file_undo` 特性门控默认关闭。
- **预估工作量**：L

#### T-M4-10 cli exec/doctor/mcp 子命令
- **crate**：cli（`commands/exec.rs`、`commands/doctor.rs`、`commands/mcp.rs`）
- **输入**：T-M4-4、T-M4-5、T-M4-8
- **输出**：`minicoding exec --sandbox read-only|external-sandbox ...` 非交互批量执行；`minicoding doctor --security` 自检（沙箱驱动/硬化状态/VCS 保护/权限配置）；`minicoding mcp list/approve/reset-project-choices`
- **涉及功能**：P-22、P-23、X-11
- **涉及约束**：C-22（doctor 须如实报告 `is_hardened()`）、C-24（mcp approve 走批准流）
- **验收标准**：
  - `minicoding exec --sandbox read-only "读 README 并总结"` 在 Linux 内核 5.13+ 上能拦截越界写；
  - `minicoding doctor --security` 输出沙箱驱动类型与硬化状态；
  - `minicoding mcp list` 列出已配置 server；
  - `minicoding mcp approve <server>` 批准 project 作用域 server。
- **预估工作量**：M

#### T-M4-11 policy CallbackPrompter + keyring + 脱敏
- **crate**：policy（`prompter/callback.rs`）、cli（`cred.rs`）、core（`model/error.rs`）
- **输入**：T-M2-5、T-M0-6
- **输出**：`CallbackPrompter`（SDK 闭包，供 M8 SDK 用）；凭证 keyring 存储（`keyring` crate，文件 fallback）；敏感数据脱敏（`.env`/`api_key`/`password` 模式替换）；SSRF 防护（内网/元数据接口拒绝）
- **涉及功能**：P-08、P-10、P-12、E-02
- **涉及约束**：C-02（SSRF 内网目标黑名单）、C-04（凭证仅存内存与 keyring，不下传子进程、日志脱敏）
- **验收标准**：
  - `cargo test -p minicoding-policy callback` / `cred` 全过；
  - keyring 不可用时文件 fallback；
  - `fs.read` 读取 `.env` 时自动脱敏；
  - SSRF 内网目标（169.254.169.254 等）被拒；
  - `CallbackPrompter` 可被 SDK 闭包注入。
- **预估工作量**：M

---

## §8 里程碑 M5：Hooks 与子 Agent

> 对应 roadmap M5 的 Hooks/子 Agent/Plan 部分（12 人日）。交付 10 类 Hook、子 Agent、Plan 模式。

#### T-M5-1 hooks Hook trait + HookRegistry + 10 事件
- **crate**：core（`hooks/trait.rs`）、hooks（`registry.rs`、`lib.rs`）
- **输入**：T-M1-2
- **输出**：`Hook`/`HookRegistry` trait 定义（core）；`HookRegistryImpl` 实现（hooks）；10 类事件（SessionStart/UserPromptSubmit/PreToolUse/PostToolUse/PostToolUseFailure/PreCompact/PostCompact/Stop/SubagentStop/PermissionRequest）；串行聚合；matcher 过滤（工具名 glob，`|` 分隔、`*` 通配）
- **涉及功能**：H-01、H-02、H-04
- **涉及约束**：C-21（Hook 不可覆盖 L0——内置黑名单 Deny 优先于 Hook）、C-05（Hook 注入内容是数据非指令）
- **验收标准**：
  - `cargo test -p minicoding-hooks registry` 全过；
  - 10 类事件均能触发注册的 Hook；
  - matcher 过滤按工具名 glob 生效；
  - 内置黑名单 `Deny` 在 Hook 之前生效，Hook 的 `allow` 被忽略并记审计。
- **预估工作量**：L

#### T-M5-2 hooks ScriptHook 适配器 + on_hook_error
- **crate**：hooks（`script.rs`、`protocol.rs`）
- **输入**：T-M5-1
- **输出**：`ScriptHook` 适配器（外部可执行 + JSON over stdio + 退出码语义）；`HookInput`/`HookOutput` JSON 协议；`on_hook_error` 策略（continue/deny/fail，超时 kill，默认超时 30s）
- **涉及功能**：H-03、H-10
- **涉及约束**：C-04（Hook 子进程 env 不含凭证）、C-22（Hook 子进程受沙箱约束）、C-03（modify_input 仍经路径校验）
- **验收标准**：
  - `cargo test -p minicoding-hooks script` 全过；
  - 外部脚本通过 stdio 收到 `HookInput` JSON 并返回 `HookOutput`；
  - 退出码语义（0 allow/2 deny/其他 error）正确映射；
  - 超时后子进程被 kill，按 `on_hook_error` 策略处理；
  - Hook 子进程 env 不含凭证。
- **预估工作量**：L

#### T-M5-3 hooks PreToolUse 拦截/改写 + PostToolUse 后处理 + PermissionRequest 短路
- **crate**：hooks（`registry.rs`）、core（`agent/loop.rs`）
- **输入**：T-M5-1、T-M5-2
- **输出**：`PreToolUse` 拦截（deny/allow/modify_input）；`PostToolUse` 后处理（跑 formatter/linter、改写 result）；`PermissionRequest` 短路（自动批准/阻断，跳过 Prompter）；`modify_input` 仍经 `sandbox_path` 校验
- **涉及功能**：H-05、H-06、H-07
- **涉及约束**：C-21（Hook 不可覆盖 L0——modify_input 越界被 sandbox_path 拦）、C-03（路径校验）、C-01（PermissionRequest 短路仍受黑名单约束）
- **验收标准**：
  - `cargo test -p minicoding-hooks pre_tool_use` / `post_tool_use` / `permission_request` 全过；
  - `PreToolUse` Hook `deny` 能阻断工具调用；
  - `modify_input` 越界路径被 `sandbox_path` 拦；
  - Hook 对内置黑名单 `Deny` 的 `allow` 被忽略；
  - `PostToolUse(fs.write|fs.edit)` 能触发 `cargo fmt`。
- **预估工作量**：M

#### T-M5-4 hooks 6 个内置示例 Hook
- **crate**：hooks（`builtin/fmt_on_write.rs`、`builtin/auto_approve_tests.rs`、`builtin/block_secrets.rs`、`builtin/git_status_inject.rs`、`builtin/backup_before_compact.rs`、`builtin/test_on_stop.rs`）
- **输入**：T-M5-3
- **输出**：6 个内置示例 Hook 实现；上下文注入（SessionStart/UserPromptSubmit/PreCompact 注入）；Hook 审计（allow/deny/modify_input 落 audit.log，source=hook）
- **涉及功能**：H-08、H-11、H-12
- **涉及约束**：C-05（注入内容是数据非指令，包裹 `<hook_context>` 边界）、C-21（block-secrets 不可被覆盖）
- **验收标准**：
  - `cargo test -p minicoding-hooks builtin` 全过；
  - `fmt-on-write` 在 `PostToolUse(fs.write|fs.edit)` 后跑 `cargo fmt`；
  - `block-secrets` 拦截含密钥的文件写入；
  - `git-status-inject` 在 SessionStart 注入 git 状态；
  - 审计日志记录 Hook 决策（source=hook）。
- **预估工作量**：M

#### T-M5-5 hooks asyncRewake 异步唤醒
- **crate**：hooks（`async_rewake.rs`）、core（`hooks/trait.rs`）
- **输入**：T-M5-1
- **输出**：`asyncRewake` 异步唤醒管理（后台任务 + 唤醒注入）；3 并发上限；超时 kill（`estimated_duration × 2`）；`<async_rewake>` 边界注入；协议错误检测（事件白名单、字段必填校验）
- **涉及功能**：H-13
- **涉及约束**：C-26（asyncRewake 不可越权——适用事件受限、后台进程同等待遇、结果是数据非指令、资源约束 3 并发）、C-32（asyncRewake 协议契约——事件白名单、字段必填、超时硬约束、唤醒注入边界）、C-07（资源约束）
- **验收标准**：
  - `cargo test -p minicoding-hooks async_rewake` 全过；
  - 仅 `PostToolUse`/`PostToolUseFailure`/`Stop` 事件的 `async_rewake` 有效；
  - 其他事件返回 `async_rewake = Some` 被忽略并记审计（source=hook_protocol_violation）；
  - 第 4 个并发 async_rewake 被拒绝并记审计；
  - 后台超时自动 kill 并注入"async_rewake timeout"提示；
  - 后台 Hook 子进程 env 不含凭证。
- **预估工作量**：L

#### T-M5-6 core Plan 模式 + plan.exit
- **crate**：core（`agent/plan_mode.rs`）、tools（`plan/exit.rs`）
- **输入**：T-M2-5、T-M4-4
- **输出**：`PermissionMode::Plan` 状态机；双重只读强制（Plan 模式下非只读工具被硬门 `Deny`）；`plan.exit` 工具（退出 Plan 模式并提交计划）；预批准缓存（ExitPlanMode 后切回 Default 模式并保留预批准）
- **涉及功能**：A-06、T-15
- **涉及约束**：C-25（Plan 模式硬门用 `is_read_only()` 判断，给声明 readOnlyHint 的 MCP 工具留通道）、C-01（副作用经权限）
- **验收标准**：
  - `cargo test -p minicoding-core plan_mode` 全过；
  - Plan 模式下非只读工具被 `Deny`；
  - `plan.exit` 后切回 Default 模式并保留预批准；
  - 声明了 `readOnlyHint` 的 MCP 工具在 Plan 模式可用。
- **预估工作量**：L

#### T-M5-7 core 子 Agent（Explore/Plan/General）+ task.spawn
- **crate**：core（`agent/subagent.rs`、`model/subagent.rs`）、tools（`task/spawn.rs`）
- **输入**：T-M2-1、T-M3-7
- **输出**：`SubagentType`（Explore/Plan/General/Custom）；`task.spawn` 工具启动类型化子 Agent（隔离上下文）；Explore/Plan 子 Agent 跳过 AGENTS.md 加载；OTel Context 传播（子 Agent span）
- **涉及功能**：A-05、T-13
- **涉及约束**：C-05（子 Agent 上下文是数据非指令）、C-22（子 Agent 受沙箱约束）、C-04（子 Agent env 不含凭证）
- **验收标准**：
  - `cargo test -p minicoding-core subagent` 全过；
  - `task.spawn` 启动子 Agent 并隔离上下文；
  - Explore/Plan 子 Agent 跳过 AGENTS.md 加载；
  - OTel span 可见父子关系（子 Agent span 挂在父 turn span 下）。
- **预估工作量**：L

#### T-M5-8 cli hook 加载 + /plan + /undo REPL + OTel span
- **crate**：cli（`session/interactive.rs`、`commands/`）、core（`otel.rs`）
- **输入**：T-M5-4、T-M5-5、T-M5-6、T-M4-9
- **输出**：`--hook`/配置加载 Hook；`/undo` REPL 命令；`/plan` 切换 Plan 模式；`/mcp` REPL 命令；OTel `hook.run` span（hook.name/hook.event/hook.decision）、`mcp.call` span（server/tool/elapsed）
- **涉及功能**：F-02、O-07、O-08、O-04
- **涉及约束**：C-21..C-28（Hook/MCP/Journal 约束的运行时验证）
- **验收标准**：
  - `minicoding --hook fmt-on-write "改 utils.rs"` 能触发 Hook；
  - `/undo` 能回滚最近一次 operation；
  - `/plan` 切换 Plan 模式后非只读工具被拒；
  - OTel collector 可见 `hook.run`/`mcp.call` span；
  - 子 Agent span 传播正确。
- **预估工作量**：M

---

## §9 里程碑 M6–M8：高级形态

> 对应 roadmap M6（多 Provider，6 人日）+ M7（TUI，10 人日）+ M8（SDK/Server，8 人日）。

### 9.1 M6 — 多 Provider 与健壮性

#### T-M6-1 providers Anthropic 实现
- **crate**：providers（`anthropic/`）
- **输入**：T-M1-4
- **输出**：`AnthropicProvider` 实现 `LlmProvider`；`/v1/messages` 事件流；system prompt 分离；近似 token 计数
- **涉及功能**：L-02、L-07（Vision 基础）
- **涉及约束**：C-04（密钥脱敏）、C-12（事件流解析容错）
- **验收标准**：
  - `cargo test -p minicoding-providers anthropic` 全过；
  - Anthropic 模型可正常流式 + 工具调用；
  - 事件流解析覆盖 `content_block_start`/`content_block_delta`/`message_stop`；
  - 图片输入（Vision）可发送。
- **预估工作量**：L

#### T-M6-2 providers Ollama 实现
- **crate**：providers（`ollama/`）
- **输入**：T-M1-4
- **输出**：`OllamaProvider` 实现 `LlmProvider`；`/api/chat` NDJSON 流；本地模型支持
- **涉及功能**：L-03
- **涉及约束**：C-12（NDJSON 解析容错）、P-09（本地无 TLS）
- **验收标准**：
  - `cargo test -p minicoding-providers ollama` 全过；
  - 本地 Ollama 服务可连接并流式输出；
  - NDJSON 边界 case 覆盖。
- **预估工作量**：M

#### T-M6-3 providers 重试/限流/超时装饰器
- **crate**：providers（`common/retry.rs`、`common/error.rs`）
- **输入**：T-M1-4
- **输出**：统一重试策略（指数退避、429 Retry-After）；超时优雅取消；`LlmError` 分类（速率限制/超时/鉴权/网络）
- **涉及功能**：L-05
- **涉及约束**：C-07（资源不可耗尽——重试上限）、C-13（防死循环）
- **验收标准**：
  - `cargo test -p minicoding-providers retry` 全过；
  - 429 自动退避重试；
  - 超时优雅取消不丢已生成内容；
  - 三家 provider（OpenAI/Anthropic/Ollama）行为一致（同一 prompt 产出合法消息序列）。
- **预估工作量**：M

#### T-M6-4 mcp rmcp 完整客户端（http + OAuth）
- **crate**：mcp（`client/rmcp.rs`）
- **输入**：T-M4-7
- **输出**：`rmcp` 2.2 完整客户端替换 `stdio_only`；支持 streamable HTTP + OAuth（bearer token 鉴权）
- **涉及功能**：X-03（完整）
- **涉及约束**：C-04（OAuth token 脱敏）、C-08（schema 校验）
- **验收标准**：
  - `cargo test -p minicoding-mcp http` 全过；
  - rmcp http MCP server 可连接（含 bearer token 鉴权）；
  - OAuth 流程可完成；
  - stdio 作为 fallback 保留。
- **预估工作量**：L

#### T-M6-5 core 错误分类与恢复策略 + cli --provider/--model
- **crate**：core（`model/error.rs`）、cli（`args.rs`、`builder.rs`）
- **输入**：T-M6-1、T-M6-2、T-M6-3
- **输出**：错误分类与恢复策略（见 design.md §10）；`--provider`/`--model` 覆盖；模型路由（Router）基础骨架
- **涉及功能**：L-06（Router 基础）
- **涉及约束**：C-13（防死循环——错误恢复不无限重试）
- **验收标准**：
  - `cargo test -p minicoding-core error` 全过；
  - `--provider anthropic --model claude-3-5-sonnet` 切换 provider；
  - 集成测试：mock 三家 provider 跑同一会话行为一致。
- **预估工作量**：M

### 9.2 M7 — TUI

#### T-M7-1 tui ratatui + crossterm 基础框架
- **crate**：tui（`main.rs`、`app.rs`、`event.rs`、`runtime_bridge.rs`）
- **输入**：T-M2-1
- **输出**：`ratatui` + `crossterm` 基础框架；独立线程跑 Runtime，UI 线程通过 channel 收发事件；非 TTY 降级
- **涉及功能**：F-05
- **涉及约束**：C-04（UI 不显示凭证）、C-19（语言一致）
- **验收标准**：
  - `cargo test -p minicoding-tui` 全过；
  - 全屏交互流畅（< 16ms 渲染，`criterion` 基准）；
  - 非 TTY 降级为 CLI 模式。
- **预估工作量**：L

#### T-M7-2 tui 流式 Markdown + reedline 输入
- **crate**：tui（`render/markdown.rs`、`view/chat.rs`、`view/prompt.rs`）
- **输入**：T-M7-1
- **输出**：流式 Markdown 增量解析渲染；`reedline` 输入（历史、补全）；多会话侧栏；对话主视图
- **涉及功能**：F-05、F-06
- **涉及约束**：C-18（上下文经济——增量渲染只刷新脏区）
- **验收标准**：
  - 流式 Markdown 增量渲染不闪烁；
  - 输入历史可用上下箭头切换；
  - 多会话侧栏可切换。
- **预估工作量**：L

#### T-M7-3 tui TuiPrompter 非阻塞权限弹窗
- **crate**：tui（`view/permission.rs`）、policy（`prompter/tui.rs`）
- **输入**：T-M7-1、T-M2-5
- **输出**：`TuiPrompter` 实现 `PermissionPrompter`（点对点，非阻塞主循环）；权限弹窗挂起工具调用，UI 处理后回传 `Decision`
- **涉及功能**：F-07
- **涉及约束**：C-01（权限经 Prompter）、C-22（沙箱策略切换经用户确认）
- **验收标准**：
  - 权限弹窗非阻塞主循环（Runtime 在 `Verdict::Ask` 时挂起工具调用）；
  - UI 处理后回传 `Decision`，工具调用继续/中止；
  - 不阻塞其他 UI 交互。
- **预估工作量**：L

#### T-M7-4 tui 任务面板 + 工具面板
- **crate**：tui（`view/tool_panel.rs`、`view/task_panel.rs`）
- **输入**：T-M7-1、T-M3-8
- **输出**：工具调用实时进度面板；任务面板同步更新（订阅 `Event::TaskUpdated`）；主题、配色
- **涉及功能**：F-08
- **涉及约束**：C-33（任务规划纪律——面板可视化）
- **验收标准**：
  - 工具调用实时进度可见；
  - 任务面板同步更新（`task.update` 后立即刷新）；
  - 主题可切换。
- **预估工作量**：M

### 9.3 M8 — SDK 与 Server

#### T-M8-1 sdk Client + ClientBuilder
- **crate**：sdk（`lib.rs`）
- **输入**：T-M2-1、T-M4-11
- **输出**：`Client` + `ClientBuilder`；`ask`/`ask_stream`/`run_task` 高层 API；`on_event` 订阅；默认无副作用权限策略；`CallbackPrompter` 供 SDK 用户闭包
- **涉及功能**：E-01、E-02
- **涉及约束**：C-01（默认无副作用）、C-04（SDK 不泄露凭证）
- **验收标准**：
  - `cargo test -p minicoding-sdk` 全过；
  - `Client::ask` 可在第三方 Rust 项目运行（集成测试）；
  - 所有 API `Send + Sync`；
  - 默认无副作用权限策略，调用方需显式启用。
- **预估工作量**：L

#### T-M8-2 cli serve HTTP/JSON-RPC server
- **crate**：cli（`commands/serve.rs`）、sdk
- **输入**：T-M8-1
- **输出**：`minicoding serve` HTTP/JSON-RPC server；可被 curl 调用
- **涉及功能**：E-03
- **涉及约束**：C-01（server 端权限策略）、C-04（不泄露凭证）
- **验收标准**：
  - `minicoding serve --port 8080` 启动后 `curl` 可调用；
  - 标 `experimental` 直到反馈收敛。
- **预估工作量**：M

#### T-M8-3 mcp serve --as-mcp-server
- **crate**：mcp（`server/expose.rs`）
- **输入**：T-M4-7、T-M8-2
- **输出**：`minicoding serve --as-mcp-server` 把自身工具暴露为 MCP server（`rmcp` `#[tool]` 宏）；可被 Claude Desktop 等客户端发现并使用
- **涉及功能**：X-10、E-04
- **涉及约束**：C-08（工具 schema 正确暴露）、C-25（只读性 hint 正确声明）
- **验收标准**：
  - MCP server 可被 Claude Desktop 发现并调用 `fs.read` 等工具；
  - 工具 schema 与本地一致。
- **预估工作量**：M

#### T-M8-4 sdk stdin/stdout NDJSON 协议
- **crate**：sdk、cli
- **输入**：T-M8-1
- **输出**：stdin/stdout NDJSON 协议（编辑器插件）；文档：嵌入指南、协议规范
- **涉及功能**：E-05
- **涉及约束**：C-04（不泄露凭证）、C-05（输出是数据非指令）
- **验收标准**：
  - 编辑器插件可通过 NDJSON 协议驱动 minicoding；
  - 协议文档完整。
- **预估工作量**：M

#### T-M8-5 memory 向量检索（@memory）+ tools 高级组
- **crate**：memory（`vector.rs`）、tools（`web/fetch.rs`、`web/search.rs`、`git/diff.rs`、`git/apply.rs`、`shell/background.rs`、`shell/output.rs`、`shell/kill.rs`）、mcp（`tool_search.rs`）
- **输入**：T-M3-5、T-M6-4
- **输出**：向量检索（`@memory` 语义检索增强）；`web.fetch`/`web.search`；`git.diff`/`git.apply`；`shell.background`/`shell.output`/`shell.kill`（后台命令）；BM25 工具检索（工具多时按需检索）
- **涉及功能**：M-08、T-09、T-10、T-11、T-12、T-08b、T-08c、T-08d、X-09
- **涉及约束**：C-02（SSRF 防护——web.fetch/search）、C-03（路径校验——git.apply）、C-07（后台命令资源约束）、C-25（工具检索只读性）
- **验收标准**：
  - `@memory` 能语义检索长期记忆；
  - `web.fetch` 把 URL 转 Markdown 且 SSRF 内网被拒；
  - `git.apply` 应用 patch 受路径沙箱约束；
  - `shell.background` 返回 shell_id，`shell.output` 非阻塞读取，`shell.kill` 终止；
  - 工具 > 50 个时 BM25 按需检索生效。
- **预估工作量**：L

#### T-M8-6 工程化：cargo dist + 分发
- **crate**：workspace 根
- **输入**：T-M6-1..T-M8-5
- **输出**：`cargo dist` 产出跨平台二进制（Linux musl、macOS universal、Windows）；Homebrew / Scoop / cargo install 三渠道
- **涉及功能**：Q-08、Q-09
- **涉及约束**：无
- **验收标准**：
  - `cargo dist build` 产出三平台二进制；
  - Homebrew tap 可安装；
  - Scoop bucket 可安装；
  - `cargo install minicoding` 可安装。
- **预估工作量**：M

---

## §10 任务依赖图

### 10.1 里程碑级依赖

```
M0 ── M1 ── M2 ── M3 ── M4 ── M5 ── M6 ── M7 ── M8
                │           │      │
                └── M3' ────┘      └── M6 可与 M7 部分并行
```

- **M1 → M2 强依赖**：M2 的完整循环基于 M1 的单轮循环基础。
- **M2 → M3 强依赖**：M3 的压缩基于 M2 的完整循环与 LlmProvider。
- **M3 → M4 部分并行**：M4 的 OS 沙箱独立于上下文管理，但需 M2 的应用层权限就位。
- **M4 → M5 强依赖**：M5 的 Hook/MCP 子进程依赖沙箱就位以隔离；Plan/Journal 依赖 M3 的任务管理（task.*）与存储。
- **M6 可与 M7 部分并行**：provider 工作独立于 TUI。
- **M8 依赖 M6/M7 完成**。

### 10.2 关键 task 依赖

```
T-M0-1 (workspace)
  ├─► T-M0-2 (依赖管理) ─► T-M0-3 (CI)
  ├─► T-M0-4 (OTel) ─► T-M0-6 (配置)
  └─► T-M0-7 (sandbox 骨架) ─► T-M4-1 (Landlock)

T-M1-1 (数据模型)
  ├─► T-M1-2 (trait) ─► T-M1-3 (Runtime)
  │                      ├─► T-M1-4 (OpenAI) ─► T-M1-9 (CLI 单次)
  │                      ├─► T-M1-6 (只读工具) ─┘
  │                      ├─► T-M1-7 (sandbox_path) ─┘
  │                      └─► T-M1-8 (JSONL) ─┘
  └─► T-M1-5 (Tokenizer) ─► T-M3-1 (ContextManager)

T-M2-1 (完整循环) ─► T-M2-2 (分桶调度)
                ├─► T-M2-3 (写文件组) ─► T-M4-9 (Journal)
                ├─► T-M2-4 (shell.run) ─► T-M4-6 (沙箱约束)
                └─► T-M2-5 (Policy) ─► T-M2-6 (Prompter+黑名单)
                                   ├─► T-M4-4 (SandboxPolicy)
                                   ├─► T-M4-5 (拒绝升级)
                                   └─► T-M5-6 (Plan 模式)

T-M3-1 (ContextManager) ─► T-M3-2 (压缩管道) ─► T-M3-3 (熔断)
T-M3-5 (长期记忆) ─► T-M3-7 (AGENTS.md) ─► T-M5-7 (子 Agent)
T-M3-8 (任务工具) ─► T-M5-6 (Plan 模式)

T-M4-1 (Landlock) ─► T-M4-2 (macOS/Windows) ─► T-M4-3 (硬化)
T-M4-7 (MCP client) ─► T-M4-8 (包装+批准) ─► T-M5-8 (cli mcp)
T-M4-9 (Journal) ─► T-M5-8 (cli /undo)

T-M5-1 (Hook trait) ─► T-M5-2 (ScriptHook) ─► T-M5-3 (拦截/改写)
                                                 └─► T-M5-4 (内置 Hook)
T-M5-1 ─► T-M5-5 (asyncRewake)
T-M2-1 ─► T-M5-7 (子 Agent)

T-M1-4 (OpenAI) ─► T-M6-1 (Anthropic) / T-M6-2 (Ollama) ─► T-M6-5 (错误恢复)
T-M4-7 ─► T-M6-4 (rmcp http)
T-M2-1 ─► T-M7-1 (TUI 框架) ─► T-M7-2 (Markdown) / T-M7-3 (TuiPrompter)
T-M8-1 (SDK) ─► T-M8-2 (serve) ─► T-M8-3 (MCP server)
```

### 10.3 关键依赖说明

| 依赖 | 说明 |
|------|------|
| M2 的 shell.run 依赖 M1 的 core Runtime | shell.run 是 Tool 实现，需 Runtime 调度 |
| M3 的压缩依赖 M2 的 LlmProvider | L2 摘要需调 LLM，通过 trait 注入 |
| M4 的 sandbox 依赖 M2 的 policy | 沙箱拒绝升级流走 PermissionPrompter |
| M5 的 Hook 依赖 M4 的 sandbox | Hook 子进程受沙箱隔离 |
| M5 的 Journal 依赖 M2 的写文件组 | fs.write/edit/delete 成功后调 Journal::record |
| M5 的 Plan 依赖 M3 的任务工具 | Plan 模式与任务管理协作 |
| M6 的 rmcp http 依赖 M4 的 stdio client | 完整客户端替换薄封装 |
| M8 的 MCP server 依赖 M4 的 McpClient | server 暴露复用工具 schema |

---

## §11 风险与缓解

| 风险 | 等级 | 涉及 task | 缓解措施 |
|------|:---:|---------|---------|
| **sandbox-run 跨平台一致性** | 中 | T-M4-1、T-M4-2 | `sandbox-run` 0.43 在 macOS/Linux 行为差异 → 编译期平台检测 + 容器内 CI matrix（Linux/macOS/Windows）验证拒绝语义；旧内核降级 `NoopDriver` + warn |
| **rmcp 2.x API 稳定性** | 中 | T-M4-7、T-M4-8、T-M6-4 | rmcp 2.x 仍演进 → M4 先交付 stdio，M6 升级 http+OAuth；保留 `stdio_only` 作为 fallback；锁定 patch 版本 |
| **压缩熔断状态机复杂度** | 中 | T-M3-3 | 失败计数/Thrash 检测/降级链交织 → `circuit_breaker.rs` 独立模块 + `proptest` 验证状态机不变量；提供 `compress=off` 兜底 |
| **asyncRewake 并发控制** | 中 | T-M5-5 | 3 并发上限 + 超时 kill + 协议错误检测 → 事件白名单硬校验；后台 Hook 子进程同等待遇（凭证隔离/沙箱/路径校验） |
| **edit 唯一性冲突处理** | 低 | T-M2-3 | 多处匹配时返回清晰错误并建议增大上下文 → `fs.edit` 单测覆盖歧义场景；`fs.multiedit` 原子性回滚 |
| **并行工具消息顺序** | 低 | T-M2-2 | 完成顺序乱序 → 严格按 `call_id` 关联 result，不依赖完成顺序；集成测试验证 |
| **权限交互非 TTY 边界** | 低 | T-M2-6、T-M2-8 | `NonInteractivePrompter` 显式策略化（deny/allow/fail）→ 非 TTY 默认 deny 副作用工具 |
| **Landlock 旧内核不支持** | 中 | T-M4-1 | 编译期检测 + 运行时 `landlock_available()` 探测 → 不支持降级 `NoopDriver` + warn；`is_hardened()` 如实返回 false |
| **Windows 沙箱成熟度低** | 中 | T-M4-2 | 受限令牌 + Job Object 成熟度低 → 初期降级应用层 + 用户提示，标注 "non-hardened"；`doctor --security` 如实报告 |
| **Hook 链路延迟** | 低 | T-M5-2、T-M5-3 | 串行链路影响延迟 → 默认超时 30s，`on_hook_error=continue` 兜底；`asyncRewake` 把长时任务转后台 |
| **压缩质量差** | 中 | T-M3-2 | 摘要 prompt 调优 → 提供可配置策略 + `compress=off` 关闭选项；摘要可用小模型降成本 |
| **记忆双文件一致性** | 低 | T-M3-5 | `long_term.md` + `index.json` 不一致 → 原子 rename + 启动时索引校验/重建 |
| **AGENTS.md override 语义复杂** | 低 | T-M3-7 | fallback 与 override 组合 → 充分测试 `CLAUDE.md`/`.cursorrules` fallback 与 `AGENTS.override.md` 组合 |
| **沙箱拒绝误判为普通错误** | 中 | T-M4-5 | EPERM/Seatbelt denial 与普通错误混淆 → 建立 denial 签名库（stderr 模式 + errno）；denial 走升级流而非裸错误 |
| **MCP 恶意仓库植入** | 中 | T-M4-8 | project 作用域 server 植入 → 首次批准流（`mcp_choices.toml`）；未批准不连接 |
| **流式 Markdown 重绘性能** | 低 | T-M7-2 | 全屏重绘卡顿 → 增量解析 + 脏区刷新；`criterion` 基准 < 16ms |
| **TuiPrompter 非阻塞复杂度** | 中 | T-M7-3 | 点对点交互与 broadcast 事件总线冲突 → `TuiPrompter` 独立通道挂起工具调用，UI 处理后回传 `Decision` |

---

## §12 验收标准总览

### 12.1 M0 — 工程基础设施

- `cargo build --workspace` 通过（含平台条件依赖在非 Linux 平台不编译）。
- `cargo run -p minicoding-cli -- --help` 输出帮助。
- 设置 `OTEL_EXPORTER_OTLP_ENDPOINT` 后启动一次，能在本地 OTLP collector 看到 `minicoding` 的 resource。
- CI 全绿（`fmt` + `clippy -D warnings` + `test` + `audit` + `deny`）。
- `cargo doc --workspace --no-deps` 无警告生成。

### 12.2 M1 — MVP 单轮对话

- `minicoding "读取 src/main.rs 并解释"` 能流式输出并实际读取文件。
- 工具调用渲染清晰（工具名 + 摘要）。
- 越界路径（`../../etc/passwd`）被 `sandbox_path` 拒绝并返回 `PathEscaped`。
- 单测覆盖：SSE 解析、token 计数、路径沙箱、delta 聚合。
- 非 TTY 环境禁用 spinner/颜色。

### 12.3 M2 — 完整 Agent 循环

- `minicoding "把 utils.rs 里的 foo 改名为 bar"` 能完成读取→编辑→验证闭环。
- 同轮多个只读工具并发执行；写/shell 严格串行（trace 中可见时序）。
- 非 TTY 环境下副作用工具按 `non_tty_strategy` 处理（默认 deny）。
- `shell.run` 超时、输出截断生效；危险命令被内置黑名单拒绝。
- `Ctrl-C` 不丢已生成消息。
- 集成测试：3+ 轮工具调用场景。
- `criterion` 基准：Agent 循环开销、token 计数、路径校验基线建立。

### 12.4 M3 — 上下文管理与记忆

- 长会话（>上下文窗口）能自动压缩且不破坏连贯性。
- 长期记忆文件未变更时，连续多轮 `build_chat_request` 不产生重复 IO/分词（trace 中 compress span 计数稳定）。
- 会话摘要 LLM 调用失败时自动降级为启发式兜底，会话仍正常结束（audit.log 有告警）。
- `--resume <id>` 恢复后可继续提问；`--replay` 复现历史工具调用且默认禁副作用。
- AGENTS.md 从 repo_root 到 cwd 逐级加载并注入 system；Explore/Plan 子 Agent 跳过；`fs.write` 对 AGENTS.md 默认 `Ask`。
- `task.create/update/list` 能创建/更新/完成任务，单 in_progress 约束生效，`Event::TaskUpdated` 广播。
- `proptest` 验证压缩管道不变量。
- 回放测试（JSONL fixture）覆盖回归。

### 12.5 M4 — 安全沙箱与 MCP

- `--sandbox read-only` 下任何写/网络在内核被拦（macOS/Linux），`audit.log` 记录拒绝。
- `--sandbox workspace-write` 下越界写、网络外联被拦；工作区内自由读写执行。
- `minicoding exec --sandbox external-sandbox` 在容器内运行不报沙箱初始化失败，日志声明依赖外部隔离。
- `.git` 目录在 workspace-write 下默认拒绝写入（除非 `allow_dotgit_write=true`）。
- 沙箱拒绝（如 Landlock EPERM）被识别并升级为权限请求，而非裸错误。
- `--preset full-access` 启动时打 red 警告并要求显式确认。
- `doctor --security` 输出沙箱驱动类型与硬化状态。
- MCP stdio server 能连接、`list_tools`、`call`；远程工具以 `mcp__<server>__<tool>` 注册。
- 含 `.minicoding/mcp.json` 的仓库首次进入时逐 server 弹窗批准，结果落 `mcp_choices.toml`。
- `/undo` 能回滚最近一次 operation 的文件改动；失败文件在 `UndoReport` 中列出。
- MCP/Hook 子进程不继承凭证环境变量。
- 沙箱平台 CI matrix（Linux/macOS/Windows）拒绝语义全覆盖。

### 12.6 M5 — Hooks 与子 Agent

- `PostToolUse(fs.write|fs.edit)` Hook 能触发 `cargo fmt`；`PreToolUse` Hook `deny` 能阻断工具调用。
- Hook 对内置黑名单 `Deny` 的 `allow` 被忽略（L0 不破）；`modify_input` 越界被 `sandbox_path` 拦。
- Plan 模式下非只读工具被硬门 `Deny`；`plan.exit` 后切回 Default 模式并保留预批准。
- `task.spawn` 启动子 Agent 并隔离上下文；Explore/Plan 子 Agent 跳过 AGENTS.md。
- asyncRewake 仅对 `PostToolUse`/`PostToolUseFailure`/`Stop` 生效；第 4 个并发被拒绝。
- OTel `hook.run` span（hook.name/hook.event/hook.decision）、`mcp.call` span（server/tool/elapsed）可见。
- 子 Agent span 传播正确（父子关系）。

### 12.7 M6 — 多 Provider 与健壮性

- Anthropic 模型可正常流式 + 工具调用。
- 限流自动退避重试；超时优雅取消。
- 三家 provider（OpenAI/Anthropic/Ollama）行为一致（同一 prompt 产出合法消息序列）。
- `rmcp` http MCP server 可连接（含 bearer token 鉴权）。
- `--provider`/`--model` 覆盖生效。

### 12.8 M7 — TUI

- 全屏交互流畅（< 16ms 渲染，`criterion` 基准）。
- 工具调用实时进度可见；任务面板同步更新。
- 权限弹窗非阻塞主循环（`TuiPrompter` 挂起工具调用，UI 处理后回传 `Decision`）。
- 流式 Markdown 增量渲染不闪烁。
- 非 TTY 降级为 CLI 模式。

### 12.9 M8 — SDK 与 Server

- `Client::ask` 可在第三方 Rust 项目运行。
- `serve` 模式可被 curl 调用。
- MCP server 可被 Claude Desktop 等客户端发现并使用。
- stdin/stdout NDJSON 协议可驱动编辑器插件。
- `@memory` 向量检索能语义检索长期记忆。
- `cargo dist` 产出三平台二进制；Homebrew / Scoop / cargo install 三渠道可安装。
- 协议标 `experimental` 直到反馈收敛。

---

## 附录：task 统计

| 里程碑 | task 数 | 涉及功能数 | 涉及约束数 |
|------|:---:|:---:|:---:|
| M0 | 9 | 7 | 2 |
| M1 | 9 | 19 | 8 |
| M2 | 9 | 18 | 9 |
| M3 | 10 | 22 | 7 |
| M4 | 11 | 24 | 8 |
| M5 | 8 | 16 | 7 |
| M6 | 5 | 8 | 3 |
| M7 | 4 | 4 | 2 |
| M8 | 6 | 14 | 4 |
| **合计** | **71** | **144**（全覆盖） | **35**（全覆盖） |

> 功能 ID 与约束 ID 引用均来自 `features.md` 与 `rules.md` 实际编号。如发现引用偏差，以原文件为准并提 issue 修正本文。
