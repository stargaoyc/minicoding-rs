# 第六轮全面审查报告（R6）

审查日期：2026-08-28
审查范围：全项目（18 crate + web 前端 + 文档 + CI/CD），八领域深审
方法与验证：五路并行子系统深审（security/sandbox、providers/tools、context/memory、storage/journal/mcp、server/protocol/frontends）+ 关键发现源码核验（`@import` symlink 逃逸、`read_tail_line` 8KiB 截断回归、server 沙箱 workdir 失配、durable_seq 死代码、FE-8 声明未生效、NDJSON/ACP 行读取无界、OpenAI reasoning_tokens、Linux landlock 凭证目录）+ 构建与测试基线确认（`cargo check --workspace` 通过、DTO 重生成零 diff）。

---

## 0. 总体评价

**项目处于第一梯队**：R5（2026-08-27）发现的 2 个 P0（`@import` 词法逃逸、SSRF IPv4-mapped IPv6）与大部分 P1 已修复并带回归测试；R5 修复计划 11 个阶段基本落地（19 个 commit、五批遗留修复）。四轮审查文化（R2→R5）形成闭环：每个修复点带编号注释、回归测试、文档同步。

**本轮核心主题是"修复自身引入的回归"与"声明-实现裂缝"**：

1. **R5 修复引入新 P1 回归**：ST-9 的 `read_tail_line` 8KiB 尾部窗口截断——单行事件 > 8KiB（`MessageAppended` 持久化大工具结果是常态）即破坏 append 单调性检查与 `next_seq_sync`，会话事件流**永久冻结**且 `--resume`/`--replay` 不可恢复；
2. **两个 P0 修复不彻底**：`@import` 只修了 `..` 词法逃逸，**symlink 逃逸通道仍在**（`resolve_lexical` 明确"不解 symlink"，恶意仓库符号链接指向 /etc 即可任意读）；Linux landlock 的凭证目录 deny 规则是 `#[cfg_attr(not(macos), allow(dead_code))]`——**只在 macOS 生效**，Linux 侧 `~/.config/gh`、`~/.cargo/credentials` 仍可读；
3. **"声称已修复"但未生效**：NDJSON 行读取注释声称 `take(MAX+1)` 截断、实现根本没有 `.take()`（ACP header 行同样无上限）；`EventCursor.durable_seq` 生产代码零调用（design.md §25.5 的 EventStore 重放路径是死代码）；
4. **Server 四形态一致性新 P1**：自定义 workdir 会话的 OS 沙箱策略仍绑定服务端默认 workdir——Web/Desktop 主路径（选目录建会话）每次 `shell.run` 写文件被内核拒绝。

---

## 1. 项目定位与差异化优势

### 1.1 定位评估（延续 R5，方向正确）

minicoding-rs 的差异化是**"安全可控的运行时"而非功能堆叠**，这一判断在 R6 依然成立：L0 约束体系（C-01..C-35）、权限-交互分离、内核级沙箱（landlock/Seatbelt/Job Object）、Event Sourcing、OTel 一等公民、四形态前端——这些是 Claude Code 开源复刻项目中罕见的工程纵深。

### 1.2 差异化优势（成立部分）

| 维度 | 优势 | R6 验证 |
|------|------|---------|
| 沙箱深度 | 三平台内核机制 + fail-closed 语义 | ⚠️ Linux 凭证目录通道（SEC-R6-02）、macOS/Hook 已封但 Linux 漏 |
| 约束-实现映射 | C-01..C-35 全部映射到实现文件 | ✅ R5 后全部 L0 有实现位置 |
| 审计完备性 | 权限决策全路径 audit.log | ⚠️ MCP 工具调用结果仍无审计记录 |
| 工程化 | 10 道 CI 门禁 + pre-commit + cargo-dist | ✅ DTO 门禁实测通过 |
| 审查文化 | R2→R5 四轮 + 本轮 R6 | ✅ 追溯链完整 |

### 1.3 定位风险（延续 R5 + 新增）

1. **四形态共享 Runtime 一致性达成度约 60%**：协议层（seq 收敛、权限弹窗、SSE cursor）已统一，但 server 运行时能力矩阵仍分裂（无 Hooks、无 AGENTS.md 注入、无 git/web/memory 工具、task.spawn 不可用），R5 选择"文档降级"收口，R6 新发现的自定义 workdir 沙箱失配让该落差从"能力缺失"恶化为"**主路径功能损坏**"；
2. **AGPL-3.0 限制商业采用面**（延续）；
3. **版本发布节奏与源码漂移**：v0.3.6（2026-08-27）后 23 个 commit 未发版，CHANGELOG 无 unreleased 段——修复未经发版验证，发布产物与文档状态漂移。

---

## 2. 模块化架构（18 crate）

### 2.1 职责边界（评价良好，延续 R5）

- 依赖方向单向无环，架构守卫测试强制；
- core 零实现原则执行到位；
- **SDK `--no-default-features` 编译断链（ARCH-1）已修复并实测通过**。

### 2.2 问题

| # | 级别 | 位置 | 问题 |
|---|:---:|------|------|
| ARCH-R6-1 | P3 | `server/src/runtime_builder.rs:276-277` | `config_hash_val` 计算后 `let _ =` 丢弃（ARCH-3，R5 已列未修）——死代码应删除。 |
| ARCH-R6-2 | P3 | `desktop/src/config.rs:64` | `save_provider_config` 不剥离 `provider.api_key`（ARCH-4，R5 已列未修）——当前调用方恒传空串无实际泄漏，但 Rust 边界不强制 C-04。 |
| ARCH-R6-3 | P3 | `extension-sdk/src/bundled.rs:215-220` | `on_config_changed` 持读锁跨扩展回调，误用扩展可死锁（ARCH-5，延续）。 |
| ARCH-R6-4 | P3 | `extension-sdk/src/registrar.rs:37-45` | 0.x 版本下 `^0` 兼容检查形同虚设（ARCH-6，延续）。 |
| ARCH-R6-5 | P2 | `core/tool/registry.rs:50-63` | 同名工具重复注册静默覆盖（TL-4 部分修复残留）——`HashMap::insert` 无告警，MCP warm_up 刷新时工具名不变但 schema 变化无日志，排障困难。 |

### 2.3 正向确认

- Server 与 SDK 两个 Runtime 装配点的落差已如实披露（`runtime_builder.rs:7-19` 头注），R5 计划 E13 的"文档降级"路线落实。

---

## 3. AI Provider 与工具系统

### 3.1 Provider

| # | 级别 | 位置 | 问题 |
|---|:---:|------|------|
| PT-R6-1 | **P1** | `providers/src/openai.rs:508-517` | **`parse_usage` 不解析 `completion_tokens_details.reasoning_tokens`**（R5 PT-4 未修）：o1/o3/o4 推理模型输出 token 统计系统性低估 30-80%，压缩触发时机与预算判定错误。 |
| PT-R6-2 | P1 | `providers/src/anthropic.rs:243-245` vs `552-559` | **Capabilities 声明 `max_output: 32768`，thinking 路径实际产出可达 64K**（`THINKING_MAX_OUTPUT_LIMIT`）——能力声明与实现上限不一致，上游压缩/预算逻辑依赖此值。 |
| PT-R6-3 | P2 | `openai.rs:237-238`、`anthropic.rs:243-245`、`ollama.rs:209-210` | Capabilities 硬编码与模型无关（R5 PT-3 未修）：DeepSeek 64K、GPT-4.1 1M、Claude 4 64K 输出均不反映，压缩行为错误。 |
| PT-R6-4 | P2 | `openai.rs:434-439` + `477-481` | **refusal + content_filter 双 `Delta::Stop(Filtered)`**（R5 PT-5 未修）：同一 chunk 推两个 Stop，消费端可能对第一个终止而错过后面的 Usage delta。 |
| PT-R6-5 | P3 | `retry.rs:97-100` | 退避抖动源用时钟纳秒 `% 41`，同一时间片内并发实例抖动区分度不足。 |
| PT-R6-6 | ✅ | `openai.rs:199-204` | thinking_budget 静默丢弃已改为显式 warn（PT-1 修复验证通过）。 |

### 3.2 工具系统

| # | 级别 | 位置 | 问题 |
|---|:---:|------|------|
| TL-R6-1 | P1 | `web/fetch.rs:275-277` | **重定向 Location 大小写敏感**（R5 TL-8 未修）：`HTTPS://` 被误判为相对路径拼接 origin——RFC 7230 scheme 大小写不敏感，合法 HTTPS 重定向失败，且可能产出畸形 URL。 |
| TL-R6-2 | P1 | `git/apply.rs:18-56` | **patch 路径校验不查 `diff --git` 行**（R5 TL-9 未修）：`diff --git a/../etc/passwd` 可绕过 `---`/`+++` 校验，git apply 以 diff --git 行为准。 |
| TL-R6-3 | P2 | `task/spawn.rs:279-284` | 子代理摘要不截断（R5 TL-7 未修）——子代理输出可挤占父代理全部上下文预算（C-07）。 |
| TL-R6-4 | P2 | `fs/glob.rs:103` | Windows 路径分隔符（`\`）与 globset `/` 模式不匹配（R5 TL-5 未修）——测试用 `replace('\\', "/")` 绕过了问题。 |
| TL-R6-5 | P2 | `fs/write.rs:109`、`fs/edit.rs:129`、`fs/multiedit.rs:141` | 三个写工具直接 `tokio::fs::write` 覆盖——崩溃/断电时文件截断（非原子）。 |
| TL-R6-6 | P2 | `web/search.rs:89-91` | `redirect::Policy::none()` 后 3xx 直接报错不跟随——DDG 实际部署偶发 302，功能脆弱。 |
| TL-R6-7 | P3 | `web/fetch.rs:92` | `MAX_BODY_BYTES = 10MiB` 硬编码不可配置。 |
| TL-R6-8 | P3 | `shell/run.rs:318-340` vs `shell/output.rs:87-88` | 两套脱敏规则（本地 `redact_secrets` vs `minicoding_policy::redact`）不同步——`ghp_` GitHub 令牌在 background 输出路径不被脱敏。 |
| TL-R6-9 | ✅ | `git/diff.rs:143-148` | 输出截断已修复（TL-1 验证通过）。 |
| TL-R6-10 | ✅ | `registry.rs:50-62` | name/schema.name 不一致已校验+warn（TL-3 验证通过）；但重复注册仍静默（ARCH-R6-5）。 |

### 3.3 正向确认

- `shell.run` 进程组 kill、输出上限、超时钳制、env 清洁、脱敏正确；
- `web.fetch` SSRF + IP pinning + 逐跳重定向校验 + 10MiB 上限完备；
- `worktree` 实现安全（env 清洁、分支名注入防护、合并失败保留分支）；
- 工具 `side_effect()` 标注抽查准确（C-11）。

---

## 4. 安全权限模型与三平台沙箱

### 4.1 P0（新发现，源码核验）

| # | 位置 | 问题 |
|---|------|------|
| SEC-R6-1 | `memory/src/project_doc/loader.rs:80-86` | **`@import` symlink 逃逸 → 任意文件读取**：R5 的 SEC-1 修复只堵了 `..` 词法逃逸；`path_within` 用 `resolve_lexical`（注释明确"不触碰文件系统、不解 symlink"），而 `read_to_string` 跟随 symlink。恶意仓库创建 `subdir/link -> /etc/` 符号链接 + `AGENTS.md` 写 `@import subdir/link/passwd`——词法包含判定通过、实际读取 `/etc/passwd` 展开进 `<project_doc>` 外发 LLM 厂商。与 R5 的 `..` 逃逸同属"克隆恶意仓库即中招"，且**无需竞态**（符号链接随仓库提交）。 |
| SEC-R6-2 | `sandbox/src/hardening.rs:148-198` | **Linux landlock 凭证目录可读**：`home_read_allow_paths` 把 `~/.config`、`~/.cargo`、`~/.npm` 整目录放入读白名单；`credential_dir_deny_paths()`（`~/.config/gh`、`~/.config/gcloud`、`~/.cargo/credentials`）被 `#[cfg_attr(not(target_os = "macos"), allow(dead_code))]` 标注——**仅在 macOS Seatbelt 生效**（注释自认"Linux landlock 侧 deny 规则未启用该列表"）。landlock path_beneath 语义下 `~/.config` 的 allow 放行其下全部子路径：沙箱内 `cat ~/.config/gh/hosts.yml > workdir/leak.txt` 可把 GitHub 令牌外泄到可写 workdir。 |

### 4.2 P1

| # | 位置 | 问题 |
|---|------|------|
| SEC-R6-3 | `policy/src/ssrf.rs:109` | `check_host` 用 `(host,0).to_socket_addrs()` **同步阻塞 DNS**，从 `PermissionPolicy::check` 的 BoxFuture 中被调用——阻塞 tokio worker 线程（tools 侧已异步 + IP pinning，policy 侧独立路径不同步）。 |
| SEC-R6-4 | `policy/src/builtin.rs:229-240` | **`shell.background` 绕过命令黑名单**：`is_blacklisted` 仅对 `"shell.run"` 调用 `shell_hits_blacklist`——`shell.background` 执行 `rm AGENTS.md` / `echo > .git/hooks/x` 不经 C-02 检查。 |
| SEC-R6-5 | `tools/src/mcp.rs`（MCP 工具执行路径） | **MCP 工具执行不接 OS 沙箱**：side-effect MCP 工具（server 暴露的 shell 等）子进程不经 landlock/Seatbelt/Job Object，C-22 对 MCP 工具不成立。 |

### 4.3 P2

| # | 位置 | 问题 |
|---|------|------|
| SEC-R6-6 | `policy/src/redact.rs:43` | URL userinfo 正则密码字符集 `[^/@\s]+` 不含 `@`/`/`——`postgres://user:pass@word@host/db` 只脱敏到第一个 `@`。 |
| SEC-R6-7 | `core/src/util/mod.rs:105-108` | 指令性内容检测只剥离 5 个硬编码零宽字符——Unicode `Cf` 类共约 160 个（如 `\u{2060}` WORD JOINER），`\u{2060}A\u{2060}lways...` 可绕过祈使检测写入 auto.md（C-27 降级通道）。 |
| SEC-R6-8 | `mcp/src/approval.rs:122-144` | `FileChoicesStore::save` 仍 `std::fs::write`（0644）→ rename → chmod 0600——rename/chmod 间 0644 窗口 + 无 fsync（R5 SEC-16 残留，审计部分已修）。 |
| SEC-R6-9 | `memory/src/project_doc/loader.rs:39-46` | `@import` 无条数上限（仅深度 3 限制）——恶意仓库数千行 `@import /dev/random` 可灌爆 I/O 与上下文。 |
| SEC-R6-10 | 全仓 | **MCP 工具调用结果无审计记录**（审计覆盖缺口）：`server/ndjson.rs` 记录 REST 路径，MCP 工具走 `minicoding-mcp` 内部路径不触发任何 `AuditSink::record`。 |

### 4.4 L0 强制力验证汇总（R6 更新）

| 约束 | R5 状态 | R6 状态 | 备注 |
|------|:---:|:---:|------|
| C-01 副作用经权限 | ✅ | ✅ | |
| C-02 黑名单不可覆盖 | ✅ | ⚠️ | **shell.background 旁路**（SEC-R6-4） |
| C-03 路径不可越界 | ⚠️ | ⚠️ | `@import` symlink（SEC-R6-1）；post_compact 词法（CTX-R6-1） |
| C-04 凭证不可外泄 | ⚠️ | ⚠️ | Linux 凭证目录（SEC-R6-2）、redact 边界（SEC-R6-6） |
| C-07 资源上限 | ⚠️ | ⚠️ | subagent 摘要不截断（TL-R6-3） |
| C-22 沙箱二道防线 | ⚠️ | ⚠️ | MCP 工具无沙箱（SEC-R6-5）、Windows 残余 |
| C-26 asyncRewake | ❌ | ⚠️ | R5 SEC-5 修复后 Hook 已可接沙箱；asyncRewake 后台 Hook 是否同等待遇需验证 |
| C-30 沙箱熔断 | ⚠️ | ⚠️ | server workdir 失配可误触发熔断（FE-R6-1） |

---

## 5. 四形态前端与 Runtime 一致性

### 5.1 P1：自定义 workdir 会话的 OS 沙箱 workdir 失配（新发现）

`server/src/http.rs:575-578`（create_session）、`session_mgr.rs:657-658`（restore_session）、`ndjson.rs:613-618`、`acp.rs:455-461` 对自定义 `body.workdir` 只覆盖 Runtime workdir；`sandbox_policy` 仍是 `default_params.sandbox_policy`（内嵌**服务端默认 workdir**）。`core/runtime/workdir.rs:94` 的 `switch_workdir` 同型（只更新 workdir 不同步 sandbox_policy）。

**影响**：Web/Desktop 用户选择与 server CWD 不同的项目目录 → 每次 `shell.run` 写文件（`cargo build`、`git` 等）在内核层被拒 → 功能损坏 + 沙箱拒绝计数累积误触发 C-30 熔断。这是 Web/Desktop 形态**主路径**的高概率用户可见故障。方向是 fail-closed（非安全洞）但体验致命。

### 5.2 P1：LSP/ACP/NDJSON turn 收尾竞态（TurnEnd 可能丢失）

`lsp.rs:178-193`、`acp.rs:555-582`、`ndjson.rs:524-554`：turn 完成后用 `while let Ok(item) = rx.try_recv()` 非阻塞 drain。`TurnEnd` 需经 EventBus→sequencer→sequenced_tx **两跳**才到达订阅端，JoinHandle 完成与 send 之间无排序保证——尾事件可能仍在途中被漏掉。NDJSON 客户端依赖 `TurnEnd` 判定轮次结束，丢失即挂起等待。

### 5.3 P1：FE-8 防护声明未生效（NDJSON/ACP 行读取无界）

- `ndjson.rs:106-128`：注释声称"用 `take(MAX_LINE_BYTES+1)` 截断读"，**实现没有 `.take()`**——`read_line` 先全量缓冲再判 `n > MAX_LINE_BYTES`，OOM 防护实际不存在；
- `acp.rs:213-241`：header 区 `read_line` 无上限（body 有 256 MiB cap）。

### 5.4 P1：durable recovery 死代码

`protocol/cursor.rs:162-164` `set_durable` 生产代码零调用（仅测试），`session_mgr.rs:162-200` 的 `classify_replay` 以 `durable_seq` 判定——新会话 durable_seq=0、运行中不随持久化推进，evicted 事件一律 `Unrecoverable` → 全量 RehydrateRequired。design.md §25.5 的 EventStore 重放路径实际不可达。

### 5.5 其他

| # | 级别 | 位置 | 问题 |
|---|:---:|------|------|
| FE-R6-1 | P2 | `sse.rs:159-168` | `sse_live`（首次连接路径）未计入订阅者计数——FE-17 残留，从未断线的 Web 标签页会话仍可被空闲驱逐。 |
| FE-R6-2 | P2 | `ndjson.rs:312-320` | 会话创建收到双份 `SessionCreated`（适配器发 seq=0 + 首次 turn 后 cursor 发 seq=1）。 |
| FE-R6-3 | P2 | `protocol/jsonrpc.rs:102-109` | `Response` 仍接受 result/error 同缺或同在的非法形态（FE-11 残留）。 |
| FE-R6-4 | P3 | `http.rs:797-799` | DELETE 后磁盘文件保留，stale 客户端持旧 id 可 `get_or_load` 复活会话——语义与实现不一致，建议文档化。 |
| FE-R6-5 | ✅ | 全部 FE-2..FE-18（除上列残留） | R5 前端问题修复验证通过（TUI 双显/CJK/下溢、cred 回显、sidecar 孤儿、LSP 双转发、seq 收敛等）。 |

### 5.6 四形态一致性评估（R6 更新）

| 维度 | 状态 |
|------|------|
| 协议层（seq/事件/权限弹窗/SSE cursor） | ✅ 已统一（R5 后最大成就） |
| 运行时能力矩阵（Hooks/AGENTS.md/git/web/memory/task.spawn） | ❌ 仍分裂（R5 文档降级收口） |
| 沙箱/workdir 语义 | ❌ **新 P1 失配**（FE-R6 主路径） |
| 配置热更新/扩展 | ❌ 仅 SDK 侧 |

达成度约 60% 延续。R6 建议：优先修 workdir 失配（高成本低），AGENTS.md 注入与 git 工具组接线（成本低收益高），其余维持文档披露。

---

## 6. 上下文管理（4 级压缩）与记忆

### 6.1 R5 修复状态验证

| 项 | 判定 | 证据 |
|---|:---:|------|
| CTX-1 压缩判据漏计注入 | ✅ 修复 | `manager.rs:631-654` post_compact 预算 clamp 到 `remaining_window`，0 时跳过注入 |
| CTX-2 小窗口熔断死亡 | ✅ 修复 | `manager.rs:362` `effective_threshold == 0` 跳过 oversize 分支 |
| CTX-3 缓存竞态 | ⚠️ 部分修复 | `fetch_add` 仍在外（见 CTX-R6-2） |
| CTX-4 reply-priming 漂移 | ✅ 修复 | 每条 append 扣除 priming，误差 ≤3 token |
| CTX-5 摘要只写不读 | ⚠️ 决策记录 | `rt.rs:429-435` 明确列为设计决策项（注入半边未实现） |
| CTX-6 摘要只用 text() | ✅ 修复 | `session_sum.rs:75` 改用 `full_text()` |
| CTX-7 path_within 词法 | ✅ 修复 | post_compact 拒绝 `..` + 两侧规范化 |

### 6.2 新发现

| # | 级别 | 位置 | 问题 |
|---|:---:|------|------|
| CTX-R6-1 | P1 | `post_compact.rs:165-176` | **post_compact symlink TOCTOU**：`path_within_workdir` 纯词法不 resolve symlink，随后 `tokio::fs::read_to_string` 跟随 symlink——workdir 内 symlink 指向外部文件可把任意内容回灌 system 段（路径来自已读历史，窗口窄于 `@import` 但同模式）。 |
| CTX-R6-2 | P2 | `manager.rs:508-509` | **CTX-3 残留低估竞态**：append 的 `fetch_add` 在写锁外执行，compress 的 `store` 可覆盖缓存——缓存比真实值少新消息 token（方向危险，可致超窗），被 `turn_gate` 掩盖。 |
| CTX-R6-3 | P2 | `session_sum.rs:131-137,199-210` | **启发式摘要无上限**：每条消息首 100 字符拼接，5000 条消息会话产生 ~650KB 摘要写入 `index.json`，`list_sessions` 全量加载。 |
| CTX-R6-4 | P2 | `manager.rs:116-117,703-713` | **restore 不重置 append_seq**（R5 P3 确认未修）：`/clear` 后压缩追溯区间 seq 锚点失准，审计区间不可读。 |
| CTX-R6-5 | P2 | `summarize.rs:76` + `weight.rs:46-49` | **L2 不豁免 pinned + `is_sticky` 恒 false**（R5 P3 确认未修）：pinned 消息在 L2 仍可被摘要替换（权重 ×2.0 可被大批低权重消息超越），与 L3/L4 豁免语义不一致；"错误/未提交变更 ×1.5"权重保护未实现。 |
| CTX-R6-6 | P2 | `manager.rs:477-490` | calibrate 混入 `actual_input_tokens`（含 system+tools+post_compact）到 messages-only 缓存——下一轮系统性高估约 0.5×固定开销，持续提前压缩。 |
| CTX-R6-7 | P3 | `budget.rs:53-55`、`compress/mod.rs:55` | `config.budget_ratio` 死配置（硬编码 0.85）；`CompressResult.backup` 死字段。 |
| CTX-R6-8 | P3 | `predictive.rs:103` | 预测压缩不计 fixed_overhead，时序落后 reactive。 |
| CTX-R6-9 | P3 | `post_compact.rs:147` | 注入的 `--- {path} ---` 头部不计 token 预算。 |
| CTX-R6-10 | P3 | `auto.rs:269-275` | Auto memory 仅 mtime 缓存无 hash 校验（跨进程粗粒度 stale）。 |
| CTX-R6-11 | P3 | `compress/mod.rs:109` | L1 无条件先裁大 tool_result（超阈主因是历史消息时也先丢内容）。 |

### 6.3 正向确认

- 4 级压缩顺序、预算数学、tool_use/tool_result 配对原子组替换、降级链正确；
- C-29 熔断状态机不可被 LLM 绕过（唯一入口是压缩结果回调）；
- C-27 物理隔离 + 指令性降级 + 边界注入完整。

---

## 7. 存储 / Journal / MCP

### 7.1 P1：R5 修复引入的 8KiB 尾部窗口截断回归（新发现，源码核验 + 边界推理）

`storage/src/event_store.rs:100-125`（`read_tail_line`）：ST-9 修复后 seek 到 `len-8KiB` 再取末行。**若事件文件最后一行 > 8KiB**（`MessageAppended` 持久化完整消息，含 `fs.read`/`git.diff`/`web.fetch` 等大工具结果，上限 ~1MiB），窗口从行中部开始 → 片段必然非法 JSON → `Corrupted`：

1. `append` 单调性检查（:156）恒失败 → 事件持久化永久失败（warn + 回滚）→ **该会话事件流冻结**，resume 与 replay 分歧扩大（ST-4 被放大）；
2. `next_seq_sync`（:71）→ `init_event_stream`（`sourcing.rs:55`）→ CLI `main.rs:253` 退出码 1 / server BuildFailed → **该会话不可 `--resume`/`--replay`**。

正常会话"大工具结果后崩溃/中断"即可触发。现有测试只覆盖小行。

### 7.2 R5 修复状态验证

| 项 | 判定 | 证据 |
|---|:---:|------|
| ST-1 seq 缺口自愈 | ⚠️ 部分修复 | append 失败回滚已分配 seq ✓；历史缺口仍硬失败（`builder.rs:991-994`） |
| ST-2 index 跨进程锁 | ⚠️ 部分修复 | `mutate_index` 锁内 load-modify-save ✓；**同步扫描回退路径不持锁**（`jsonl.rs:179,656`） |
| ST-3 snapshot 竞态+fsync | ✅ 修复 | tmp 名含 pid+计数、父目录 fsync、load_sync Corrupted 回退 |
| ST-4 双写分歧 | ✅ 决策记录 | `sourcing.rs:7-16` 头注完整记录取舍 |
| ST-5 journal 字节上限 | ✅ 修复 | 32MiB + 200 条双约束 |
| ST-6 MCP shutdown | ✅ 修复 | 5s 超时 |
| ST-7 delete 锁 | ⚠️ 部分修复 | async delete ✓；**`delete_session_sync`（CLI 用）不取锁** |
| ST-8 事件文件误解析 | ⚠️ 部分修复 | async `build_index_from_scan` ✓；**`list_sessions_sync` 未跳过** |
| ST-9 O(N) 读校验 | ❌ **修复引入回归** | 见 7.1 |
| ST-10 next_seq 无锁 | ❌ 未落实 | 注释要求调用方持锁，**全部调用方均未持锁**；`lock.rs:50-53` 声称的"`--resume` 单点检测"CLI 无实现 |
| ST-11 audit 无锁 | ✅ 修复 | `{audit}.lock` 阻塞锁 + fsync + 0600 测试 |
| ST-12 Audio 类型 | ✅ 修复 | 转 Text 元数据 |
| ST-13 注释不符 | ✅ 修复 | 注释如实说明 |

### 7.3 其他 P2/P3

| # | 级别 | 位置 | 问题 |
|---|:---:|------|------|
| ST-R6-1 | P2 | `jsonl.rs:313-327` | `mutate_index` 在 async 上下文内阻塞 flock + IO（未 spawn_blocking）。 |
| ST-R6-2 | P3 | `snapshot_store.rs` | tmp 文件崩溃后不清理。 |
| ST-R6-3 | P3 | `event_store.rs` 加载路径 | 单行损坏 → 整个事件流 Corrupted，无"截断到最后一个合法行"恢复策略（消息流有 skip-bad-line）。 |
| ST-R6-4 | P3 | `audit.rs:40-52` | 每条审计一次 spawn_blocking + flock + fsync——权限决策热路径延迟。 |
| ST-R6-5 | P2 | `mcp/client/rmcp.rs:679-693` | R5 ST-6 修复后 shutdown 有 5s 超时 ✓（验证通过）。 |

---

## 8. 文档与工程化

### 8.1 文档完备性（总体优秀，延续 R5）

- 25 篇文档，R2-R5 审查报告在案，修复点注释带编号追溯链完整；
- `features.md` 205 项与统计表一致；
- **DTO 门禁实测通过**：`pnpm gen-types` 重生成后 `git diff` 零差异（FE-1/ENG-1 已修复）。

### 8.2 工程化问题

| # | 级别 | 位置 | 问题 |
|---|:---:|------|------|
| ENG-R6-1 | P2 | 版本管理 | **v0.3.6（2026-08-27）后 23 个 commit 未发版**，CHANGELOG 无 `[unreleased]` 段——R5 全部遗留修复未经发版验证，发布产物与源码/文档漂移。 |
| ENG-R6-2 | P3 | `rust-toolchain.toml` | nightly 钉版 + MSRV 1.99 声明的矛盾已如实披露（ENG-2 收口），维持 P3。 |
| ENG-R6-3 | P3 | 发版流程 | desktop/web 版本同步为手工 chore（0.3.5 曾实际漏掉），无自动门禁。 |
| ENG-R6-4 | ✅ | CI | 10 道门禁均配置严格参数；`windows-target-check`（cargo-xwin）能真实拦截平台分支错误。 |

---

## 9. 生产级可靠性风险清单（汇总）

### 9.1 安全风险（恶意仓库/恶意 LLM 输出可触发）

1. **[P0]** `@import` symlink 逃逸——克隆恶意仓库即中招，任意文件读取外发（SEC-R6-1）；
2. **[P0]** Linux landlock 凭证目录可读——沙箱内 shell 可读 `~/.config/gh`、`~/.cargo/credentials`（SEC-R6-2）；
3. **[P1]** `shell.background` 绕过命令黑名单（SEC-R6-4）；
4. **[P1]** MCP 工具执行无 OS 沙箱（SEC-R6-5）；
5. **[P2]** redact 边界（URL userinfo 密码含 `@`）、零宽字符绕过指令检测；
6. **[P2]** `mcp_choices.toml` 0644 窗口。

### 9.2 可靠性风险

7. **[P1]** `read_tail_line` 8KiB 截断回归——长事件行冻结事件流 + `--resume`/`--replay` 不可恢复（ST-R6-1）；
8. **[P1]** server 沙箱 workdir 失配——Web/Desktop 主路径功能损坏 + C-30 误熔断（FE-R6 5.1）；
9. **[P1]** turn 收尾竞态——NDJSON 客户端挂起等待 TurnEnd（5.2）；
10. **[P1]** durable recovery 死代码——长会话断线重连退化为全量重拉（5.4）；
11. **[P1]** OpenAI reasoning_tokens 未解析——推理模型压缩判定错误（PT-R6-1）；
12. **[P2]** 23 commit 未发版——修复未经验证发布。

### 9.3 体验风险

13. **[P1]** 自定义 workdir 会话 shell.run 写文件全拒（9.2-8 的用户视角）；
14. **[P2]** 启发式摘要无上限——`list_sessions` 全量加载大文件（CTX-R6-3）；
15. **[P2]** NDJSON 双 SessionCreated / sse_live 未计入订阅者。

---

## 10. 修复计划（分阶段）

### 阶段 A — 安全 P0/P1（SEC-R6-1/2/4/5 + PT-R6-1/4 + TL-R6-1/2 + 回归测试）

A1. `@import` symlink 防护：读取前 canonicalize + `is_under` 组件级判定（与 `path_sandbox::resolve_under` 同口径）+ 条数上限 + 回归测试；
A2. Linux landlock 凭证目录 deny：ABI 5+ 用 `deny()` 规则，ABI < 5 从白名单排除 `.config`/`.cargo`/`.npm` 活凭证子树；
A3. `shell.background` 接入 `shell_hits_blacklist`；
A4. OpenAI `parse_usage` 解析 `reasoning_tokens`；
A5. refusal 双 Stop 合并（同一 chunk 只推一个 Filtered）；
A6. `web.fetch` 重定向 scheme 大小写不敏感；
A7. `git.apply` 校验 `diff --git` 行路径。

### 阶段 B — 存储/Server P1（ST-R6-1 + FE-R6 5.1-5.4）

B1. `read_tail_line` 修复：窗口起点向前回退到行首（找到窗口前最后一个 `\n`）再读至 EOF；补 >8KiB 事件行回归测试；
B2. server 沙箱 workdir 失配：`create_session`/`restore_session`/ACP/NDJSON/`workspace_switch` 按会话 workdir 重建 `SandboxPolicy::WorkspaceWrite`；
B3. NDJSON/ACP 行读取真实截断（`.take(MAX+1)`）+ header 行上限；
B4. LSP/ACP/NDJSON turn 收尾改短超时 `select!` 等待 TurnEnd；
B5. `push_event` 同步 `cursor.set_durable(runtime.durable_seq())` 激活 durable recovery。

### 阶段 C — 上下文/记忆 P1（CTX-R6-1..5）

C1. post_compact 读取前 canonicalize 校验；
C2. append 的 token/count 更新移入写锁内；
C3. 启发式摘要字节上限（如 8KiB）+ 截断标注；
C4. restore 重置 append_seq；
C5. L2 摘要排除 pinned。

### 阶段 D — P2/P3 批量（SEC-R6-6..10、ST-R6-1..5、TL-R6-3..8、CTX-R6-6..11、FE-R6-1..3、ARCH-R6-1/2/5）

D1. redact URL userinfo 密码字符集放宽；D2. 零宽 Cf 字符类别级剥离；D3. `mcp_choices.toml` 0600 创建 + fsync；D4. MCP 工具执行审计接入；D5. `delete_session_sync`/`list_sessions_sync` 锁与跳过修复；D6. `mutate_index` spawn_blocking；D7. subagent 摘要截断；D8. glob Windows 分隔符；D9. fs.write/edit 原子写（tmp+rename）；D10. web.search 跟随 3xx；D11. 脱敏规则统一；D12. 注册重复告警；D13. 死代码清理（config_hash_val、budget_ratio、backup）；D14. sse_live 订阅者计数；D15. NDJSON 双 SessionCreated；D16. jsonrpc Response 形态校验；D17. desktop api_key 剥离。

### 阶段 E — 工程化（ENG-R6-1/3）

E1. CHANGELOG 补 unreleased 段 + 发版 0.3.7（或明确规划）；
E2. 发版脚本/门禁校验三处版本号同步。

### 阶段 F — 文档同步 + R6 报告归档

F1. 全部修复对应的文档更新（security.md/design.md/api.md/hooks.md/features.md）；
F2. 本文档归档。

---

## 11. 结语

R5→R6 的 24 小时窗口内，项目完成了 19 个 commit 的遗留修复，效率与纪律俱佳。但本轮暴露的规律值得警惕：**修复引入回归**（ST-9 → 8KiB 截断）、**修复只覆盖主路径**（ST-2/7/8 同步路径旁路、SEC-16 tmp 权限）、**声明未生效**（NDJSON take、durable_seq）、**同类漏洞只堵一个维度**（`..` 修了 symlink 还在；macOS 修了 Linux 还在）。建议后续审查把"修复完整性"（旁路、跨平台、跨模型、回归测试）作为验收标准，而不是"问题是否已提交修复 commit"。

安全域两个 P0（symlink 逃逸、Linux 凭证目录）仍是"仓库即边界"攻击面，应最优先处置。
