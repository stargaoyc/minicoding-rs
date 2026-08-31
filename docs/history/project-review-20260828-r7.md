# 第七轮全面审查报告（R7）

审查日期：2026-08-28
审查范围：全项目（18 crate + web 前端 + 桌面 + 文档 + CI/CD）
方法：R2→R6 六轮审查报告逐项核验 + 关键源码逐文件重读（rt.rs/manager.rs/loader.rs/hardening.rs/openai.rs/anthropic.rs/shell run+background+output/wrapper.rs/config.rs/sse.rs/turn_tail.rs 等 30+ 文件）+ 构建/测试基线确认（`cargo check --workspace` 通过、`cargo test --workspace` 全量 1753 项通过）+ R6 修复 commit 逐条核对落盘。

---

## 0. 总体评价

**R6 修复全部落地，测试全绿，工程纪律维持高位**。R6 报告列出的阶段 A–F 修复（安全 P0/P1、存储/Server P1、上下文/记忆 P1、P2/P3 批量、工程化、文档）均已提交并带回归测试；本审查在 1753 项测试全过、`cargo check` 零告警的基础上确认无回归。

**本轮的实质增量是三类**：
1. **确认并补齐一类被漏掉的凭证通道**：Linux landlock 白名单展开只排除了 `~/.config/gh`/`gcloud`/`~/.cargo/credentials`，但 `~/.config` 下还有 `github-copilot`（Copilot OAuth token）、`git/credentials`、`docker`（registry auth）、`uv`/`pypoetry` 等凭证落点——同一"仓库即边界"攻击面延续；
2. **确认 R6 有意留白 / 标记"维持披露"的项真实仍开放**（MCP 工具无 OS 沙箱、MCP 工具调用无审计、anthropic capabilities 与 thinking 上限不一致、capabilities 硬编码、retry 抖动、web.search 3xx 等），给出处置建议；
3. **文档-实现裂缝复查**（features.md H-13 状态过时、desktop save_context_config 无 revision 防护等）。

**结论**：项目处于第一梯队，无新增 P0；发现 P1×1（延续）、P2×3（含 1 个新发现安全项）、P3×若干（多为延续留白）。

---

## 1. 项目定位与差异化优势

### 1.1 定位评估（延续 R5/R6，方向正确且更难被追赶）

minicoding-rs 的差异化不是"功能堆叠"而是"**可验证的安全运行时**"：
- **L0 约束体系**（C-01..C-35）与 `rules.md` §8 自检清单、`doctor --security` 互补，全部 L0 有实现位置；
- **两层权限模型**（L0 内置黑名单 → L1 统一规则集按 specificity 竞争）+ 决策/交互双 trait 分离，比 Claude Code 依赖 Hook 自觉更强（hooks.md §10 自述）；
- **内核级沙箱三平台覆盖**（landlock/Seatbelt/Job Object）+ 拒绝检测/熔断（权威/advisory 双轨 + nonce 防伪）是罕见的工程纵深；
- **Event Sourcing**（事件流 + snapshot + SSE cursor + durable recovery）达到生产级会话一致性；
- **OTel 一等公民**、四形态前端、扩展 SDK 构成完整产品面。

### 1.2 差异化优势（R7 复核成立）

| 维度 | 优势 | R7 复核 |
|------|------|---------|
| 沙箱深度 | 三平台内核机制 + fail-closed + 凭证白名单收紧 | ⚠️ 白名单仍漏 `~/.config` 下 Copilot 等凭证落点（R7-1） |
| 约束-实现映射 | C-01..C-35 全部映射 | ✅ |
| 审计完备性 | 权限决策全路径 audit.log | ⚠️ MCP 工具调用结果无审计（R7-5b 延续） |
| 工程化 | 10 道 CI 门禁 + pre-commit + cargo-dist | ✅ |
| 审查文化 | R2→R7 六轮 | ✅ |

### 1.3 定位风险（延续 + 更新）

1. **四形态共享 Runtime 一致性约 60% 维持**：协议层已统一，但 server 运行时能力矩阵仍分裂（无 Hooks、无 AGENTS.md 注入、无 git/web/memory 工具、task.spawn 不可用），R5/R6 以"文档降级"收口。R7 复核：server `runtime_builder.rs:11` 头注如实披露，但用户可感知的落差仍在——建议作为下一里程碑的明确立项。
2. **AGPL-3.0 限制商业采用面**（延续）。
3. **版本漂移已收敛**：v0.3.7（2026-08-28）已发版，CHANGELOG 有 unreleased 段。✅
4. **CLI 默认构建的 Hook 能力**：经 `minicoding-sdk`（default features 含 `hooks`）传递启用，**实际生效**（本审查核验 SDK builder 注入 `HookRegistryImpl`/`ManagedRewakeScheduler`）。但 `minicoding-cli` 自身的 `hooks` feature 仅作 passthrough，`--no-default-features` 减配时 hooks 随 SDK 一起关——建议在 `features.md`/`getting-started` 披露该联动关系。

---

## 2. 模块化架构（18 crate）

### 2.1 职责边界（评价优秀，延续 R5/R6）

- 依赖方向单向无环，`tests/architecture.rs` 守卫强制；
- core 零实现原则执行到位（本审查重读 `runtime/*`、`provider/trait.rs`、`tool/trait.rs` 未发现领域实现混入）；
- SDK `--no-default-features` 编译断链（ARCH-1）已修复并 CI 覆盖。

### 2.2 本审查发现

| # | 级别 | 位置 | 问题 |
|---|:---:|------|------|
| ARCH-R7-1 | P3 | `desktop/src/config.rs:189-207` | `save_context_config` 无 revision 防陈旧（M-10 只覆盖 `save_provider_config`），桌面"设置-上下文"与 Web/CLI 并发保存可互相覆盖。与 `save_provider_config` 行为不一致。 |
| ARCH-R7-2 | P3 | `extension-sdk/src/bundled.rs`、`registrar.rs` | R6 延续（ARCH-R6-3/4）：`on_config_changed` 持读锁跨扩展回调可死锁；0.x 版本 `^0` 兼容检查形同虚设。R6 已列入未修，维持。 |
| ARCH-R7-3 | P2 | `docs/features.md` H-13 | **文档-实现裂缝**：H-13 状态"后台 executor 未接线"过时——SDK builder 已注入 `ManagedRewakeScheduler`（`sdk/builder.rs:612-620`），后台 executor 已接线；但 CLI（经 SDK）与 server 装配点行为不同，需按装配点如实披露。 |

### 2.3 正向确认

- `ToolRegistry` 同名重复注册已告警（ARCH-R6-5 修复落地，`core/tool/registry.rs`）；
- server config_hash_val 死代码已删（ARCH-R6-1 修复落地）；
- desktop `save_provider_config` 已剥离 `api_key`（ARCH-R6-2 修复落地，`desktop/config.rs:68-69`）。

---

## 3. AI Provider 与工具系统

### 3.1 Provider

| # | 级别 | 位置 | 问题 |
|---|:---:|------|------|
| PT-R7-1 | **P1** | `anthropic.rs:245` vs `:559` | **延续 PT-R6-2（未修）**：`capabilities().max_output = 32_768`，而 `compute_max_tokens` 的 thinking 路径上限 `THINKING_MAX_OUTPUT_LIMIT = 64_000`——能力声明与实现上限不一致。上游压缩/输出 token 预算依赖 `max_output`，thinking 模式下真实产出可超声明 2×，预算预留不足。建议声明值对齐 64K（预算宁多勿少）。 |
| PT-R7-2 | P2 | `openai.rs:231-240`/`anthropic.rs:236-247`/`ollama.rs:202-212` | **延续 PT-R6-3（未修）**：capabilities 硬编码与模型无关（DeepSeek 64K、GPT-4.1 1M、Claude 4 64K 输出均不反映）。压缩时机与预算判定错误。建议至少对已知模型前缀做能力探测，其余保持保守默认。 |
| PT-R7-3 | P3 | `providers/src/common/retry.rs:88-100` | 延续 PT-R6-5：抖动源用时钟纳秒 `% 41`，同一时间片并发实例区分度不足（实际影响可忽略——微秒级时钟差在秒级退避前无感）。可改为原子计数器种子 + 简单 LCG，不引依赖。 |
| PT-R7-4 | ✅ | `openai.rs` reasoning_tokens / refusal 双 Stop | PT-R6-1/4 修复落地并带回归测试（`parse_usage_folds_reasoning_tokens_into_output`/`parse_chunk_refusal_and_content_filter_emit_single_stop`）。 |

### 3.2 工具系统

| # | 级别 | 位置 | 问题 |
|---|:---:|------|------|
| TL-R7-1 | P2 | `web/search.rs:88-110` | 延续 TL-R6-6：`redirect::Policy::none()` 后 3xx 一律报错。注释已声明是 SSRF 防护的刻意选择，但 DDG 实际部署偶发 302 会使功能脆弱。建议改"逐跳跟随 + 每跳 host 校验"（复用 `web.fetch` 的 `validate_url` 逻辑），既修功能又保安全。 |
| TL-R7-2 | P3 | `web/fetch.rs:92` | 延续 TL-R6-7：`MAX_BODY_BYTES = 10 MiB` 硬编码不可配置。建议接到 `ctx.max_output_bytes`（作为上限或配置项）。 |
| TL-R7-3 | ✅ | `git/apply.rs`、`web/fetch.rs` scheme | TL-R6-1/2 修复落地（diff --git 行两侧路径校验 + 重定向 scheme 大小写不敏感）。 |
| TL-R7-4 | ✅ | `task/spawn.rs`、`fs/*`、`glob.rs` | TL-R6-3/4/5 修复落地（子代理摘要截断、Windows 分隔符、原子写 tmp+rename）。 |
| TL-R7-5 | ✅ | `shell/output.rs:87-88` | 后台输出经 `minicoding_policy::redact`（含 sk-/ghp_ 前缀），TL-R6-8 闭环。 |

### 3.3 正向确认（R7 重读 shell.run/background/output 全链路）

- `shell.run`：超时 clamp（S8）、进程组整树 kill（S9）、流式字节上限（S10）、env 白名单（C-04）、`redact_secrets` 值边界精确脱敏（PTM-14）均正确；
- `shell.background`：沙箱注入（C-22）、进程组（S9）、128 条上限淘汰 + 运行中进程终止（T-8）、缓冲字节上限正确；**无自动超时**为设计使然（后台长驻语义），建议在 features.md 披露"后台命令需 `shell.kill` 显式终止，无自动超时"（C-07 边界披露）。
- `shell.output`：读取时脱敏（C-04）✅；
- `web.fetch`：SSRF + IP pinning + 逐跳重定向复检 + 10MiB 上限完备；
- 工具 `side_effect()` 标注抽查准确（C-11）。

---

## 4. 上下文管理（4 级压缩）与记忆

### 4.1 R6 修复状态验证

| 项 | 判定 | 证据 |
|---|:---:|------|
| CTX-R6-1 post_compact symlink TOCTOU | ✅ | `post_compact.rs` canonicalize 二次判定 + 回归测试 |
| CTX-R6-2 append 缓存锁外竞态 | ✅ | `manager.rs:523-529` fetch_add 移入写锁 |
| CTX-R6-3 启发式摘要上限 | ✅ | `session_sum.rs` HEURISTIC_MAX_BYTES=8KB |
| CTX-R6-4 restore 重置 append_seq | ✅ | `manager.rs:731` |
| CTX-R6-5 L2 排除 pinned + is_sticky | ✅ | `summarize.rs`/`weight.rs` + 回归测试 |
| CTX-R6-7 budget_ratio 接线 / backup 死字段 | ✅ | `budget.rs` with_ratio + `compress/mod.rs` 移除 backup |

### 4.2 延续留白（P3，建议纳入 backlog）

| # | 位置 | 问题 |
|---|------|------|
| CTX-R7-1 | `predictive.rs:103` | 预测压缩不计 fixed_overhead，时序落后 reactive（延续 CTX-R6-8） |
| CTX-R7-2 | `post_compact.rs:147` | 注入 `--- {path} ---` 头部不计 token 预算（延续 CTX-R6-9） |
| CTX-R7-3 | `auto.rs:269-275` | Auto memory 仅 mtime 缓存无 hash 校验（延续 CTX-R6-10） |
| CTX-R7-4 | `compress/mod.rs:109` | L1 无条件先裁大 tool_result（延续 CTX-R6-11） |

### 4.3 正向确认

4 级压缩顺序、预算数学、tool_use/tool_result 配对原子组替换、降级链正确；C-29 熔断状态机不可被 LLM 绕过；C-27 物理隔离 + 指令性降级 + 边界注入完整（R7 复核 `manager.rs` 全链路）。

---

## 5. 安全权限模型与三平台沙箱

### 5.1 本审查新发现

| # | 级别 | 位置 | 问题 |
|---|:---:|------|------|
| **SEC-R7-1** | **P2（新）** | `sandbox/src/hardening.rs:184-200`（deny 列表）、`home_read_allow_paths_without_credentials` | **Linux landlock / macOS Seatbelt 的 HOME 读白名单仍漏 `~/.config` 下多处凭证落点**：`credential_dir_deny_paths` 只含 `~/.config/gh`/`~/.config/gcloud`/`~/.cargo/credentials`。而 `~/.config` 整体在 allow 白名单（Linux 展开后保留其余子项），以下高价值凭证可被沙箱内 `cat ... > workdir/leak` 外泄：`~/.config/github-copilot/hosts.json`（Copilot OAuth token）、`~/.config/git/credentials`（git 凭证存储）、`~/.config/docker/config.json`（registry auth）、`~/.config/uv`/`~/.config/pypoetry`（Python 包索引 token）、`~/.config/aws`（部分工具）。与 R6 已修的 `gh`/`gcloud` 同型。建议补齐 deny 列表（macOS 尾部 deny + Linux 展开自动覆盖）。 |

### 5.2 R6 延续确认开放项

| # | 级别 | 位置 | 问题 |
|---|:---:|------|------|
| SEC-R7-2 | **P1** | `mcp/src/client/wrapper.rs:88-158` | 延续 SEC-R6-5：MCP 工具执行无 OS 沙箱。side-effect MCP 工具（server 暴露 shell 等）子进程不经 landlock/Seatbelt/Job Object，C-22 对 MCP 工具不成立。架构级改动（需在 rmcp spawn 前注入驱动），建议纳入 roadmap 明确立项 + security.md 披露。 |
| SEC-R7-3 | **P2** | `mcp/src/client/wrapper.rs` + `wrapper.rs:10` 注释 | 延续 SEC-R6-10 + **文档-实现裂缝**：`McpToolWrapper::execute` 模块头注释声称"审计落 `audit.log` 标注 mcp_server"，但 execute 不调 `AuditSink`（`ToolContext.audit` 被 `_ctx` 忽略）。MCP 工具调用结果无审计记录。可低成本修复：在 execute 内用 `ctx.audit` 记录 `tool_result`。 |
| SEC-R7-4 | P2 | `policy/src/redact.rs:43` | 延续 SEC-R6-6：URL userinfo 密码字符集放宽修复已落地（R6 D1）。✅ 此行为已闭环，不再列问题。 |

### 5.3 L0 强制力验证汇总（R7 更新）

| 约束 | R6 | R7 | 备注 |
|------|:---:|:---:|------|
| C-02 黑名单不可覆盖 | ⚠️ | ✅ | SEC-R6-4（shell.background 旁路）已修 |
| C-03 路径不可越界 | ⚠️ | ✅ | SEC-R6-1（@import symlink）已修 |
| C-04 凭证不可外泄 | ⚠️ | ⚠️ | 白名单漏 `~/.config` Copilot 等（SEC-R7-1） |
| C-22 沙箱二道防线 | ⚠️ | ⚠️ | MCP 工具无沙箱（SEC-R7-2，架构级） |
| C-30 沙箱熔断 | ⚠️ | ✅ | server workdir 失配已修（FE-R6-1） |

---

## 6. 四形态前端与 Runtime 一致性

### 6.1 R6 修复状态验证

| 项 | 判定 | 证据 |
|---|:---:|------|
| ST-R6-1 read_tail_line 8KiB 截断回归 | ✅ | `event_store.rs` 窗口回退行首 + >8KiB 行回归测试 |
| FE-R6-1 server 自定义 workdir 沙箱失配 | ✅ | `SandboxPolicy::with_workdir` 重锚定 + 单测 |
| FE-R6-2 turn 收尾竞态 | ✅ | `turn_tail.rs` 短超时 select! 排空 |
| FE-R6-3 durable recovery 死代码 | ✅ | `session_mgr.rs:125-133` push_event 同步 set_durable |
| FE-R6-4 NDJSON/ACP 行读取无界 | ✅ | `bounded_io.rs` 逐块累积真实截断 |
| FE-R6-1 sse_live 订阅者计数 | ✅ | `sse.rs:165-173` |
| FE-R6-2 NDJSON 双 SessionCreated | ✅ | 创建即 init_event_stream 转发真实 seq |
| FE-R6-3 jsonrpc Response 形态校验 | ✅ | `protocol/src/jsonrpc.rs` 自定义反序列化 |
| FE-R6-4 DELETE 后 stale 复活 | ⚠️ | 延续：文档化建议仍在（P3） |

### 6.2 本审查观察

- **Web 前端**：无 `dangerouslySetInnerHTML`（React 转义，XSS 闭环）；`localStorage` 仅存非敏感设置（不含 api_key、不含会话消息，C-04 闭环）；权限弹窗后端校验 `prompt_id`。✅
- **桌面**：sidecar 生命周期（关窗退出 + PID 兜底强杀）、keyring 凭证、`api_key` 剥离均正确（R7 复核 `config.rs`/`sidecar.rs`）。
- **CLI**：`build_runtime_with_memory_slot` 复用 SDK 装配，Hooks/asyncRewake 经 SDK default features 生效。
- **一致性达成度 ~60% 维持**：协议层统一；运行时能力矩阵分裂 + 配置热更新仅 SDK 侧。建议下一里程碑立项补齐 server 侧 AGENTS.md 注入与 git 工具组（成本低收益高）。

---

## 7. 文档完备性与工程化质量

### 7.1 文档完备性（总体优秀，延续）

- 25+ 篇文档 + R2–R6 审查报告在案，修复点注释带编号追溯链完整；
- `features.md` 205 项与统计表一致（R6 已核对）；
- DTO 生成门禁（`pnpm gen-types` + `git diff --exit-code`）在 CI web job 实测通过。

### 7.2 工程化问题

| # | 级别 | 位置 | 问题 |
|---|:---:|------|------|
| ENG-R7-1 | P3 | `docs/features.md` H-13/H-08 | 状态需更新：H-13"后台 executor 未接线"过时（SDK 已接线）；H-08 PreCompact 仍未接线（准确）。H-13 应按装配点披露（SDK 生效 / server 无）。 |
| ENG-R7-2 | P3 | `desktop/src/config.rs` | `save_context_config` 无 revision 防护（见 ARCH-R7-1）。 |
| ENG-R7-3 | ✅ | 版本管理 | v0.3.7 已发版，CHANGELOG unreleased 段在位。 |
| ENG-R7-4 | ✅ | CI | 10 道门禁 + windows-target-check（cargo-xwin）+ web/desktop job 均严格。 |

---

## 8. 生产级可靠性风险清单（汇总）

### 8.1 安全风险

1. **[P1]** MCP 工具执行无 OS 沙箱（SEC-R7-2，架构级，需立项）；
2. **[P2]** Linux/macOS HOME 读白名单漏 `~/.config` 下 Copilot/git/docker/uv/pypoetry 凭证落点（SEC-R7-1，新发现，**本次将修复**）；
3. **[P2]** MCP 工具调用结果无审计（SEC-R7-3，文档-实现裂缝，**本次将修复**）。

### 8.2 可靠性风险

4. **[P1]** anthropic capabilities max_output 32K vs thinking 64K 不一致（PT-R7-1，**本次将修复**）；
5. **[P2]** capabilities 硬编码与模型无关（PT-R7-2，建议立项）；
6. **[P2]** web.search 3xx 不跟随致功能脆弱（TL-R7-1，**本次将修复**：逐跳跟随 + host 校验）。

### 8.3 体验/文档风险

7. **[P2]** 四形态运行时能力矩阵分裂约 60%（延续，建议立项）；
8. **[P3]** features.md H-13 状态过时（ENG-R7-1，**本次将修复**）；
9. **[P3]** 桌面 `save_context_config` 无 revision（ARCH-R7-1，**本次将修复**）。

---

## 9. 修复计划（分阶段）

### 阶段 A — 安全（SEC-R7-1/3）
A1. `credential_dir_deny_paths` 补齐 `~/.config/github-copilot`/`~/.config/git/credentials`/`~/.config/docker`/`~/.config/uv`/`~/.config/pypoetry`/`~/.config/aws`（Linux 展开 + macOS 尾部 deny 自动覆盖）+ 回归测试；
A2. `McpToolWrapper::execute` 接入 `ToolContext.audit` 记录 MCP 工具调用结果（`kind=tool_result`，标注 mcp_server）+ 测试。

### 阶段 B — Provider 可靠性（PT-R7-1、PT-R7-3）
B1. anthropic `capabilities().max_output` 对齐 `THINKING_MAX_OUTPUT_LIMIT`（64K，预算宁多勿少）+ 注释说明；
B2. retry 抖动改为原子计数器种子 LCG（不引依赖）。

### 阶段 C — 工具（TL-R7-1、TL-R7-2）
C1. web.search 逐跳跟随 3xx + 每跳 host 校验（复用 validate_url）+ 回归测试；
C2. web.fetch `MAX_BODY_BYTES` 改可配置（ctx 注入）。

### 阶段 D — 工程化/文档（ARCH-R7-1、ENG-R7-1）
D1. desktop `save_context_config` 加 revision 防陈旧（与 save_provider_config 一致）；
D2. features.md H-13 状态按装配点如实披露 + shell.background 无自动超时披露；
D3. security.md 披露 MCP 工具沙箱缺口（SEC-R7-2 明确为已知边界）。

### 阶段 E — CHANGELOG + 发版
E1. CHANGELOG unreleased 段记录本轮修复；版本 0.3.8（workspace + desktop/web 三处同步）。

---

## 10. 结语

R6→R7 的窗口内项目保持全绿，无新增 P0。本轮最有价值的发现是"同类漏洞只堵一个维度的模式仍在延续"——`gh`/`gcloud` 修了，`github-copilot`/`docker` 还在（SEC-R7-1）。这印证 R6 结语的判断：**验收标准应是"修复完整性"而非"是否有提交"**。建议后续审查把 HOME 读白名单视为"凭证落点全集枚举"问题整体处理，而非逐个补洞；MCP 沙箱缺口（SEC-R7-2）建议在 roadmap 明确立项，它是当前与"生产级安全运行时"定位之间最显眼的一块短板。
