# minicoding-rs 全面审查报告（2026-08-23，基线 v0.2.33 / HEAD 200e64b）

> **本文地位**：对项目的第六次全面审查（前次为 `project-review-20260821.md`，其记录的 S1/S2/S4 等
> 问题经复核已在 v0.2.32–v0.2.33 收尾批中修复）。本文覆盖定位与差异化、模块化架构、Agent Runtime、
> Provider/工具/MCP 功能完整性、上下文管理与记忆、安全权限与 OS 沙箱、四形态前端一致性、存储与
> Hook、文档完备性与工程化质量九大领域。
>
> **审查方法**：六路并行源码级调研——①core 架构（rt.rs 全文精读 + 18 crate Cargo.toml 依赖边实测）、
> ②上下文与记忆（context/memory 全部源码）、③安全与沙箱（policy/sandbox/journal/hooks + L0 约束逐条反查）、
> ④providers/tools/mcp（37 个工具文件逐一盘点）、⑤四形态前端与协议契约（cli/tui/server/web/desktop/sdk）、
> ⑥存储/可观测性/工程化（CI workflow 全文 + 测试规模统计）。关键 P0 结论均经二次源码复核；
> 本地实测 `cargo fmt --check` 通过、`cargo clippy --workspace --all-targets --all-features -D warnings`
> 零告警、`cargo test --workspace` 约 1500 用例全部通过。
>
> **严重度定义**：P0=崩溃/核心功能静默失效/安全宣称失守，发布阻断；P1=条件性绕过/功能性 bug/承诺违约；
> P2=纵深防御缺口/结构性缺陷/文档漂移；P3=瑕疵。

---

## 一、执行摘要

**一句话结论：这是一个"架构纪律罕见地优秀、文档野心领先实现"的项目——骨架值得信任，但当前状态距"生产级可用"还差一批可定位、可修复的硬伤，其中 5 个属于会直接崩溃或静默失效的 P0。**

| 审查维度 | 评分(1-10) | 结论摘要 |
|---|:---:|---|
| 项目定位与差异化 | **8** | 差异化真实存在（OS 沙箱一等公民/事件溯源/四形态同核/Auto memory），非营销话术 |
| 模块化架构 | **8.5** | 声明即约束：13 个架构守卫测试实测有效，依赖边与文档逐条吻合 |
| Agent Runtime 正确性 | **7** | 取消恢复/C-13/工具分桶排序正确；存在权限路径 panic、StopReason 丢失 |
| AI Provider 层 | **5.5** | 纯流式最小实现：无 prompt caching/thinking/读超时，Ollama 有 UTF-8 截断 bug |
| 工具系统 | **7** | fs/shell 扎实（shell.run 是全场最佳）；shell.kill 空操作、后台绕沙箱 |
| MCP 集成 | **4** | 库代码质量高但 **client 侧整体未接线**，用户配了 mcp.json 也得不到任何工具 |
| 上下文管理与记忆 | **6** | 设计精巧但三个 P0（token 少计/压缩破坏配对/post-compact 死代码）使其在主路径上不可信 |
| 安全权限模型 | **7** | 应用层权限链完整且取严合并正确；OS 层"文档强度 >> 实现强度" |
| OS 三平台沙箱 | **4** | 网络隔离三平台均未实现；Windows Job Object 可 breakaway；Seatbelt profile 可注入 |
| Hook 系统 | **5.5** | 协议与分发算法好；inject_context/asyncRewake 未闭环 + Windows 命令注入 |
| 四形态前端一致性 | **6.5** | 同核承诺基本兑现；config.toml 主链路不读、AllowAlways 全端假实现 |
| 存储与事件溯源 | **8** | JSONL+snapshot+契约测试+SSE 三级游标恢复是同类最完整的断线方案 |
| 工程化（CI/测试/发布） | **9** | 10 job CI + 三平台矩阵 + 80% 覆盖率门禁 + cargo-dist，开源就绪 |
| 文档完备性 | 广度 **9.5** / 准确性 **5** | 30+ 篇文档覆盖极全，但大量"宣称未实现"，对二开者是系统性误导 |

### P0 问题总览

| # | 领域 | 问题 | 关键证据 |
|---|------|------|---------|
| 1 | Runtime | 权限路径字节切片 panic（中文参数崩溃） | `core/runtime/rt.rs:1368` |
| 2 | 上下文 | 生产分词器不计 ToolResult token，压缩永不触发 | `providers/tokenizer.rs:115-128` |
| 3 | 上下文 | 压缩破坏 tool call/result 配对 → 严格 provider 持续 400 | `context/compress/summarize.rs:63-84` |
| 4 | 上下文 | post-compact 文件恢复是死代码（扫错数据结构） | `context/compress/post_compact.rs:56-57` |
| 5 | MCP | client 侧整体未接线，mcp.json 用户得不到任何 MCP 工具 | `core/runtime/builder.rs` 无注入点 |
| 6 | 安全 | exec 模式供应链威胁基本不设防（默认 WorkspaceWrite+AutoApprove+无网络控制） | `cli/commands/exec.rs:54,117` |
| 7 | 前端 | 主链路不读 config.toml（README 头等特性对 CLI/exec/TUI 静默失效） | `sdk/builder.rs:122` |
| 8 | 前端 | TUI Ctrl-C 无法中断 turn | `tui/app.rs:417-424,750-755` |
| 9 | 工具 | web.fetch 字节切片 panic（远程内容可控） | `tools/web/fetch.rs:163-167` |

---

## 二、项目定位与差异化优势（相对 Claude Code）

### 真实成立的差异化

1. **OS 级沙箱一等公民**：Landlock(pre_exec fail-closed)/Seatbelt/Job Object 自研驱动，CC 的 sandbox
   是 bash 脚本级包装；本项目在内核 LSM 层做隔离且 `RulesetStatus != FullyEnforced 即拒绝 exec`
   （`sandbox/src/linux.rs:125-143`）——思路领先。
2. **事件溯源存储**：JSONL 事件流 + snapshot + seq 游标 + SSE 三级恢复（ring buffer → durable replay →
   RehydrateRequired）+ fork/replay 禁副作用（C-06）。CC 的会话文件是黑盒 JSONL，本项目可审计可回放。
3. **多前端同核**：A11 重构后 CLI/TUI 统一走 `sdk::builder` 组装（`cli/src/builder.rs` 仅剩 re-export），
   依赖图无环。
4. **Auto memory**（分类/置信度淘汰/BM25 检索/指令注入降级 Ask 的 C-27）是 CC 没有的原创设计；
   task 依赖图带 DFS 成环检测超越 CC TodoWrite。
5. **全链路审计**：压缩（CompressedRange→AuditKind::Compress）、权限决策、/undo 失败路径均落盘。

### 相对 CC 的核心差距（详见各章）

- **成本工程全面落后**：无 prompt caching 发送端（Anthropic 解析了 cache 字段却从不发 `cache_control`）、
  无 count_tokens 校准、token 计数有 P0 缺陷；
- **上下文正确性落后**（三个 P0，见第八章）；
- **UX 地基缺失**（斜杠命令/token 计量/@引用/图片粘贴，见第十一章）。

---

## 三、模块化架构（18 crate）

### 实测结论：声明全部属实

- 领域 crate 之间**零横向依赖**，唯一领域间边是 `tools→policy`（组合层，符合 AGENTS.md §3.2）；
  dev-dependencies 也只引 core；
- 架构守卫真实有效：共享断言器 `testing/manifest_guard.rs` 解析 Cargo.toml 断言白名单，
  13 个 crate 各有 tests/architecture.rs；core 另有更强守卫（禁 reqwest/landlock/rmcp/ratatui/windows）。

### 问题清单

| 级别 | 问题 | 证据 |
|---|---|---|
| P2 | 守卫不查 dev-dependencies——一行修复的潜在漏洞 | `manifest_guard.rs:16-19` 只读 `[dependencies]` |
| P1 | 交互式 CLI 不加载 config.toml：README/getting-started/api.md 三处宣称的配置分层落空，hooks 在交互模式下完全不生效 | `sdk/builder.rs:122` 从 default 起步 |
| P2 | 死依赖与文档漂移：figment、trait-variant 已选型声明但代码零使用；AGENTS.md §3.8 四层配置未实现（仅单一 user 级） | 根 Cargo.toml:41,107 vs AGENTS.md §2.1/§3.8 |
| P2 | rt.rs 2275 行已成第二单体：事件溯源(~200行)/权限管道(~600)/沙箱 denial(~300)/热更新(~130) 至少 6 个内聚单元可拆（已有 accumulator/repair 等 4 次成功抽取先例） | `core/runtime/rt.rs` |
| — | 粒度评价：拆分整体合理略偏细，不建议合并建议冻结新增。真实税负在"接线仪式感"：同一组装逻辑存在 sdk/server/tui 三份近似实现，新增能力要改 builder ×3 | 各 crate 体量 core 12.6k/tools 10.1k/server 6.0k…journal 仅 0.9k |

---

## 四、Agent Runtime 与循环正确性

### 正确性亮点（实测确认）

- 五重终止防御齐全（max_iters/重复签名/turn_timeout/cancel/沙箱熔断每 turn 重置）；
- 并行只读工具结果按 LLM 原始位置 sort 还原顺序（rt.rs:1300-1301），tool_result↔tool_call_id 对应有保证；
- C-13 双层防御扎实：backfill_missing_tool_results 以 storage 为事实源 + resume 时幂等 repair（proptest 覆盖）；
- Hook 改写输入后重跑 policy 并 `merge_verdicts_stricter` 取严（rt.rs:1408-1447）——"批准 A 执行 B"旁路被系统性封死；
- persist→ctx→emit 顺序不变量在三处写入点严格执行。

### 问题清单

| 级别 | 问题 | 证据 |
|---|---|---|
| **P0** | **权限路径字节切片 panic**：`&input_str[..input_str.len().min(80)]` 无 char boundary 检查，中文参数的副作用命令（日常路径）即可触发进程崩溃 | rt.rs:1368 |
| P1 | provider 的 StopReason 被丢弃，MaxTokens 截断在 UI 表现为"正常说完" | accumulator.rs:42 → rt.rs:781 硬编码 EndTurn |
| P1 | PermissionContext.history 是假数据：无条件 push Allow，精心拼的 summary 被 `let _ = summary;` 丢弃 | rt.rs:1360-1374 |
| P1 | 白名单热更新优先级倒置：config.toml 会顶掉 CLI 显式 `--model` flag，与 serve 模式语义相反 | rt.rs:1617-1628 |
| P2 | Runtime 无"同一时刻一个 turn"不变量（SDK 直连两次 spawn 即交错破坏 event_seq） | run_turn 入口缺 try_lock |
| P2 | ToolRegistry::dispatch 不强制超时，扩展工具若无视 ctx.timeout 仅剩 600s turn 兜底 | registry.rs:73 |
| P2 | 被拒绝的副作用调用不发 ToolCallStarted/Finished 事件，前端视角"凭空出现一条错误结果" | rt.rs:1492-1495 |
| P2 | SessionMeta 同名异型（含 tasks 与不含 tasks 两个 pub 类型）；聚合器静默吞掉非法 args JSON 无日志 | model/session.rs:18 vs storage/trait.rs:14；accumulator.rs:72 |

---

## 五、AI Provider 层功能完整性

三家对照：OpenAI(SSE+增量 tool_call 拼接正确)、Anthropic(事件流分派正确)、Ollama(NDJSON)。
共享流管道抽象出色，重试边界（建立阶段重试/流中不重试防重复产出）论证清晰。

| 能力 | OpenAI | Anthropic | Ollama | CC 对标 |
|---|:-:|:-:|:-:|---|
| prompt caching 发送端 | ❌ | ❌（解析 cache 字段却从不发 cache_control，永远打不进缓存） | N/A | ✅ |
| extended thinking 配置端 | ❌（只能被动解析 reasoning_content） | ❌（无 thinking budget 字段） | ❌ | ✅ |
| 图片输入 | ❌ 静默丢弃 | ✅ | ❌ 静默丢弃 | ✅ |
| 流式读超时 | ❌ Client 未设任何 timeout，服务端停止发送则永久挂起 | ❌ | ❌ | — |
| count_tokens/batch/JSON mode 下发 | 全部 ❌（json_mode capability 声明 true 却从不下发 response_format——虚假声明） | | | ✅ |
| max_tokens 参数 | ⚠️ 旧参数，o 系列官方 API 会 4xx | ✅ | ✅ | — |

### 问题清单

| 级别 | 问题 | 证据 |
|---|---|---|
| **P0** | web.fetch 字节切片 panic（计入工具系统章，此处索引） | fetch.rs:163-167 |
| P1 | Ollama NDJSON 跨 chunk UTF-8 截断：String 缓冲 + from_utf8_lossy，多字节字符被 TCP 分界切开即 U+FFFD 乱码（SseStream 已修而 Ndjson 漏修），中文场景必现概率高 | ndjson.rs:74 |
| P1 | 流中错误丢弃已累积内容：`delta?` 直接上抛，UI 已展示的文本不落盘，重放/审计不一致 | rt.rs:1083 |
| P2 | ApproxTokenizer 中文低估约 4 倍，CJK 长会话压缩滞后直至真实超限 | anthropic.rs:488-499 |
| P2 | Ollama 不设 num_ctx，本地默认 2048 会静默截断长对话而 capability 声称 8192 | ollama.rs:136-154 |
| P2 | OpenAI/Ollama 收到带图消息静默丢弃无占位提示；退避无 jitter；Router 是摆设（task_kind 无消费者） | openai.rs:302、ollama.rs:283、retry.rs:87、router.rs:40 |

---

## 六、工具系统功能完整性

21 个内置工具。**fs.edit 唯一性强制匹配 + multiedit 内存原子应用 + journal 全文快照**是替换语义安全的典范；
`shell.run`（env 白名单/setpgid 进程组/killpg 整树终止/双路 capped 读取/输出脱敏/sandbox apply/timeout clamp
防 LLM 放大）是全部工具中工程质量最高者。

| 级别 | 问题 | 证据 |
|---|---|---|
| **P0** | **web.fetch 字节切片 panic**：markdown 切 max_bytes 落于多字节字符内部时 panic（远程内容可控，现成的 truncate_output 未复用） | fetch.rs:163-167 |
| P1 | **shell.kill 是空操作**：拿不到 child 句柄直接返回 Ok，LLM 收到假成功而进程失控 | background.rs:197-213 |
| P1 | **后台 shell 绕过 OS 沙箱**：background spawn 无 SandboxDriver::apply（对比 run.rs:150），也无 setpgid、缓冲无上限(OOM 风险) | background.rs:96-230 |
| P1 | ctx.env 恒为空：git.diff/apply、background 子进程拿到完全空环境（git 缺 HOME 影响身份解析） | rt.rs:1149 |
| P2 | grep/glob 单线程全树遍历、grep 无上下文行/head_limit；fs.edit 无 replace_all；web.search 依赖 DDG HTML class 名爬取（页面改版即碎）；缺 AskUserQuestion 类主动提问工具；fs.read 不支持图片/PDF | 各处 |
| P2 | git.apply patch 内容未经路径预检即交给 git；ToolGroup 枚举声明后无人使用（子 Agent 工具子集裁剪未落地） | git/apply.rs:62-95、registry.rs:10-20 |

---

## 七、MCP 集成：零件精良，整车未组装

**库层质量高**：rmcp 2.2 双 transport、stdio env_clear 白名单、inflight 去重合并（含竞态防护）、
`readOnlyHint 默认不信任`（比多数实现严格）、C-24 项目指纹批准存储（原子写+0600）、server 模式
（暴露自身工具给 Claude Desktop，`cli/commands/serve.rs:382` 已接线）。

**但 client 侧完全未接线（P0）**：

- `RmcpClient`/`McpToolWrapper`/`check_project_scope_approval` 在 workspace 内**零外部调用者**（除自身测试）；
- RuntimeBuilder 无 mcp 注入点，Runtime 主循环无 `mcp__` 路由，CLI 只有 choices list/reset 两个管理命令；
- 后果：**用户配置 `.minicoding/mcp.json` 得不到任何 MCP 工具，C-24 弹窗流程实际不可达**
  ——README 第 34 条核心特性当前不成立。

附带问题：

| 级别 | 问题 | 证据 |
|---|---|---|
| P2 | 远端 inputSchema 原样透传不做 JSON Schema 校验，坏参靠 server 报错回灌自我修正 | rmcp.rs:458-467 |
| P2 | health_check 只被动检测死亡连接，无重启 supervisor；warm_up 存在但无定时调度方 | rmcp.rs:537-550 |
| P2 | HTTP transport bearer_token_env 可从任意环境变量取值注入 Authorization（配合无 SSRF 校验构成凭证外传通道） | rmcp.rs:179-232 |
| P2 | server 模式 annotations 恒 None（外部 client 拿不到 readOnlyHint）；暴露内置写工具时本侧无权限层 | expose.rs:197-199 |

---

## 八、上下文管理（4 级压缩）与记忆机制

设计层面是亮点：L1-L4 降级链 + 双计数器熔断（失败 soft=3/hard=5 + thrash 独立计数器）+ L2 摘要三级回退
（主→备→启发式兜底永不中断对话）+ 压缩全程审计/OTel/metrics。**但三个 P0 使其在 OpenAI/Ollama 主路径上实际不可信**：

1. **P0｜生产分词器不计 ToolResult token**：`count_messages` 用 `m.text()`（只取 Text block），而 wire 序列化
   确实把工具结果发给了 API → 工具输出占大头的编码会话被严重低估 → 压缩永不触发 → 直吃 API context length 400。
   测试用自造 CharTokenizer（计了 ToolResult）恰好掩盖了这一点——**测试口径 ≠ 生产口径是本项目的系统性风险**；
2. **P0｜压缩不保护 tool call/result 配对**：L2 按权重选中 Tool 结果消息而父 assistant 留下 → tool_calls 悬空；
   L3/L4 丢弃边界同样可能切断配对；唯一的 repair 只挂在 resume 路径，不在请求构建出口 → **压缩越积极越容易
   把会话打进持续 400 死局**；
3. **P0｜post-compact 文件恢复是死代码**：extract_read_files 扫 content 里的 ToolUse block，而运行时工具调用
   存在 msg.tool_calls 字段 → 恒返回空，C-09 价值为零且无人察觉；
4. **P1｜熔断触发后会话永久锁死**：唯一复位路径 record_success 在 compress 内部形成死锁闭环；文档建议
   "Try /clear" 而 `/clear` 根本不存在；
5. **P1｜context window 硬编码 128K**（builder.rs:310 TODO），Claude 200K 浪费 35% 预算；
6. **P1｜L2 摘要输入丢失全部工具信息**（也用 m.text()），压缩后模型失忆式续写；
7. **P2**｜L4 硬截断 O(n²) 重算；append 增量计数与全量重算漂移（provider Usage 未回收校准）；sticky/is_error
   加权未实现（文档承诺与实现脱节）。

**记忆子系统质量显著更高**：原子写贯穿、mtime 缓存 + hash 交叉校验、BM25 零依赖 CJK 分词正确、AGENTS.md
loader 分层拼接带 source 标注 + fallback 链、C-23（黑名单硬 Deny + Ask 无 Always）/C-27（指令注入降级 Ask）
是真实多层强制。遗留：

| 级别 | 问题 | 证据 |
|---|---|---|
| P2 | 并发 save 无互斥可产生正文/索引错配（仅靠 load 时 hash warn 兜底） | long_term.rs:167-188 |
| P2 | auto index 损坏策略与 long_term 不一致（一个 warn 继续、一个 Err 导致记忆静默消失于 system prompt） | auto.rs:141-143 vs long_term.rs:135-141 |
| P2 | C-27 指令检测双份手工复制易漂移（应下沉 core 单一来源）；行首列表项/前缀修饰漏检 | builtin.rs:211-247 与 memory/auto.rs:332-371 |

---

## 九、安全权限模型与 OS 沙箱（重点风险区）

### 应用层权限链：可信

C-01 链条完整（策略判定→Hook→改写后重查取严→点对点 Prompter→审计→执行）；未知工具 fail-closed 归入
副作用桶；凭证隔离（env_clear 白名单）在 shell/Hook/MCP stdio 三处一致；web.fetch 手动逐跳重定向每跳复检 SSRF。

### OS 层："文档强度 >> 实现强度"

| # | 级别 | 发现 |
|---|---|---|
| 1 | **P1** | **网络隔离三平台均未实现，而它是 security.md 安全论证的核心支柱**：Linux ruleset 只处理 FS 规则（landlock ABI≥4 的 AccessNet 能力未用）、macOS profile 第 8 行就是 `(allow network*)`、Windows Job Object 无网络限制。"ReadOnly 默认禁网"矩阵不成立——只读审计第三方代码时模型仍可将私有内容 POST 出去 |
| 2 | **P0** | **exec 模式供应链威胁基本不设防**（security.md §9.2 自述的头号威胁）：默认 WorkspaceWrite 而非文档宣称 read-only（exec.rs:54）+ AutoApprovePrompter 全自动放行 Ask + 无网络控制 + AGENTS.md 加载不落审计。恶意仓库 AGENTS.md → 注入 system prompt → shell 外传凭证，CI 中全程零交互 |
| 3 | **P2** | rules.md C-02/security.md §4.2 宣称的危险命令黑名单（rm -rf /、sudo、fork bomb、curl\|sh）整体不存在——builtin 黑名单实际只保护 AGENTS/CLAUDE/VCS 三类目标 |
| 4 | **P1** | 大小写不敏感 FS 绕过黑名单：macOS/Windows 上 `.GIT/config`、`agents.md` 变体绕过精确匹配 + AcceptEdits 自动 Allow → 免弹窗写 .git |
| 5 | **P1** | SSRF 可被 IPv4-mapped IPv6/NAT64 绕过；两套校验器漂移且 policy 版是死代码；MCP HTTP transport 完全无 SSRF 校验（恶意 mcp.json 可指向 169.254.169.254 且 bearer_token_env 可外传任意环境变量） |
| 6 | **P1** | C-22 二次确认 CLI 缺失：danger-full-access 仅打印红字即执行；AutoApprove 下沙箱初始化失败**静默回退沙箱外执行**（fallback 询问恒返回 Allow） |
| 7 | **P1** | ScriptHook Windows 占位符命令注入：POSIX 单引号转义 + `cmd /C`（cmd 不认单引号）→ `${TOOL_INPUT_path}` 含 `& calc` 直接执行；Hook 子进程也不在 OS 沙箱内、无进程组整树终止 |
| 8 | **P2** | Windows Job Object 设了 BREAKAWAY_OK（子进程可自愿脱离全部限制）、无内存/CPU 上限、单槽 last_policy 竞态；模块 docstring 宣称与实现不符 |
| 9 | **P2** | Seatbelt profile 字符串直插未转义（workdir 含 `"`/`)` 可注入新 allow 指令）；landlock 白名单未含 $HOME/$TMPDIR → cargo build 大概率失败，"过紧导致不可用"会把用户推向 full-access 的安全侵蚀 |
| 10 | **P2** | audit.log 多处缺口：审计失败仅 warn 不阻断、只读桶完全不审计、HookRun 审计定义了却从不写入（Hook 放行误记为 policy allowed，C-21 后半句未兑现）、0600 仅创建时生效；journal /undo 恢复写跟随 symlink 未 canonicalize |
| 11 | **P2** | git.apply 不走沙箱/不限时/不入 journal（C-07 盲区）；asyncRewake Manager 实现完毕但 Runtime 侧零接线（rt.rs:1984 自述"后续任务"）；决策持久化层（policy.toml/AllowAlways 存储/--allow --deny）整体不存在 |

### 文档与实现一致性核对结论

security.md 一致性核对表中 **❌（宣称未实现或实质不符）项超过 20 处**（网络隔离矩阵、§9.2 防御表、§12 Windows
受限令牌、§2.3 policy.toml、audit schema 字段……）。**在修复前不应依据该文档对 exec/自动化场景做任何安全假设。**

---

## 十、Hook 系统与存储/事件溯源

- **Hooks**：协议与分发算法（builtin_deny 预置、NoopRegistry 也透传 Deny）质量高；但 inject_contexts
  收集后被 Runtime 全部忽略（hooks.md §2 承诺的 SessionStart 注入不生效）、asyncRewake 未接线、
  Windows 注入（见第九章）、孙进程泄漏（kill_on_drop 只杀直接子进程）。
- **存储**：JSONL+EventStore 成熟；P2：event_store.append 无会话锁双进程可写出不可解析行（server/SSE
  路径无 SessionLock 保证）；next_seq_sync 注释称 O(1) 实际全文读入；undo 失败条目 split_off 后不可重试；
  snapshot tmp+rename 后缺父目录 fsync；audit.log/index.json 无 rotation 无限增长；fork 血缘 parent_uuid 恒 None。
- **可观测性**：OTel 初始化/span 覆盖/优雅 shutdown 达标；小瑕疵：observability.md 称 21 span/32 属性
  实际 20/30，SESSION span 定义了从未创建，turn span 未走 otel.name 约定，HookInput.turn 恒 0（rt.rs:1972）。

---

## 十一、四形态前端一致性与 UX

### 共享 Runtime 一致性矩阵（核心发现）

| 维度 | CLI | exec | TUI | server/Web | Desktop |
|---|:-:|:-:|:-:|:-:|:-:|
| 读 config.toml | ❌ | ❌ | ❌ | ✅ | ✅ |
| 沙箱预设入口 | ❌ 固定 WorkspaceWrite | ✅ | ❌ | ✅ preset+confirm | 继承 |
| 权限 Always 选项 | y/N 两值 | 自动放行 | y/a/n 未区分 | 四按钮假映射 | 同 Web |
| Ctrl-C 中断 turn | ✅ | N/A | **❌ 无效** | ✅ | ✅ |
| Undo 入口 | ✅ | ❌ | ❌ | ❌ 端点未实现 | ❌ |

### 问题清单

| 级别 | 问题 | 证据 |
|---|---|---|
| **P0** | **主链路不读 config.toml**：README 把 config.toml 作为头等特性宣传（第 63-70 行），实际 serve/server/desktop 生效而 CLI/exec/TUI 静默失效，同一台机器两种行为（一行修复） | builder.rs:122 |
| **P0** | TUI Ctrl-C 无法中断 turn（长 turn 唯一手段是杀进程） | app.rs:417 只设状态文案不调 cancel；cancel_token 标注 dead_code |
| **P1** | **AllowAlways/DenyAlways 四端皆假实现**：策略层的 Always 缓存从未被任何前端触达（Web 显示四按钮实际映射回两值）——要么贯通要么 UI 收敛为两值，不能欺骗用户 | usePermissions.ts:35-41 等 |
| **P1** | Web 违反自家 AGENTS.md：TanStack Router 缺失（routes/ 目录为空）、Zod 缺失（SSE 数据裸 cast）、CI 无生成 DTO 一致性校验（§8.4 承诺 git diff --exit-code） | package.json、client.ts:307、ci.yml web job |
| **P1** | TUI 恢复会话后聊天区空白（restore_history 回填上下文但不回填 UI lines）；渲染 O(n)/帧全量重建 Markdown，无 scrollback | tui/main.rs:81-118、view/chat.rs:21-99 |
| **P2** | Desktop CSP 含 unsafe-inline 与自述矛盾；sidecar --auth-token 走 argv 可从 /proc/pid/cmdline 读到（同文件 api_key 却专门规避了此通道）；keyring 单 entry openai_api_key 多 provider 凭证互相覆盖；POST /messages 长阻塞占连接；TUI/Web 权限弹窗不展示 diff 内容盲批 | tauri.conf.json、sidecar.rs:73、cred.rs:33 |

### UX 差距 Top（对标 CC 日常使用）

① 斜杠命令体系残缺（CLI 仅 5 个，TUI/Web 为零，无 /model /cost /clear /compact /memory）；
② 无成本/token 可视化（metadata.tokens 已采集落盘但四端无一展示）；
③ 无 @文件引用；④ 图片粘贴协议红利未兑现（Attachment 协议就绪四端无输入路径）；
⑤ 权限弹窗信息量不足（无 diff 预览，盲批高危操作）；⑥ TUI 无法滚动回看。

---

## 十二、工程化质量（CI/CD、测试、版本管理）

**这是项目最强的维度**：

- CI 10 jobs：fmt/clippy(-D warnings)/test/coverage(`llvm-cov --fail-under-lines 80`)/audit/deny/typos/
  **三平台原生测试矩阵**(fail-fast:false)/web(desktop 因 Tauri 系统库独立 job)；全局 RUSTFLAGS -D warnings、
  最小 permissions；
- 发布：cargo-dist 5 target（含 aarch64 linux）+ desktop-release(dmg/msi/AppImage/deb)；rust-toolchain.toml
  钉 nightly 并完整记录 rustc ICE 根因——范例级实践；
- 测试规模实测：**≈1500 个测试函数**（tokio::test 665 + test 862）、19 个集成测试文件（13 个架构守卫）、
  proptest×2、criterion bench（CI 未跑）、覆盖率统一 llvm-cov 80% 门禁；
- deny.toml 白名单逐条注明传递依赖来源、yanked="deny"；pre-commit 与 CI 门禁一一对应 + 敏感文件检查；
- 本地复测：clippy 零告警、全测试通过。

### 问题清单

| 级别 | 问题 |
|---|---|
| **P2** | MSRV 1.99 声明纯属假设：CI 全部 job 只跑 nightly-2026-08-18，无 stable/MSRV job（代码疑似用了 nightly 特性） |
| **P2** | 版本管理卫生：v0.2.32 tag 整体跳过；HEAD 领先 v0.2.33 十几个提交而 CHANGELOG [Unreleased] 空；Cargo.toml repository=minicoding/minicoding-rs vs cliff.toml $REPO=stargaoyc/minicoding-rs 双源不一致 |
| P3 | workflow action 版本漂移（@v4 vs @v6/v7/v8）；tauri-cli 每次 release 现场 cargo install 无缓存；desktop-release 与 cargo-dist 建 Release 存在竞态；typos 排除了 docs/*.md（文档密集仓库的拼写盲区）；__pycache__/tmp 未入库（.gitignore 正确，虚惊） |

另注意：**许可证为 AGPL-3.0-only**——对"可嵌入 SDK"的定位是天然采用壁垒（竞品多为 MIT/Apache），
企业嵌入场景会被法务直接否决，值得作为战略问题重审。

---

## 十三、文档完备性与一致性

广度罕见（30+ 篇：design/modules/api/rules/features/security/hooks/data-model + 产品手册/上手/学习/排障/
构建指南 + 专题对比报告），features.md 203 项统计表经实算与表格一致，AGENTS.md 开发约束本身是高质量范本。

**但准确性是重灾区**（对二开者的伤害大于任何单个技术缺陷）：

- security.md 一致性核对表中 ❌ 项超过 20 处（见第九章）；
- rules.md C-02 危险命令黑名单不存在；C-26/C-32 asyncRewake 未接线；C-21 "记审计"后半句未实现；
- hooks.md inject_context/asyncRewake 按已实现口径叙述；observability.md span 计数偏差；
  AGENTS.md figment/trait-variant/四层配置漂移；README 的 config.toml 快速开始对 CLI 用户无效。

**建议**：为每个功能 ID 增加"实现状态"标注（✅完整/⚠️部分/❌规划），CI 加一个文档锚点校验脚本，
把"文档-代码同步"从纪律变成门禁。

---

## 十四、生产可靠性风险汇总（按修复优先级）

### 必须立即修（P0，合计约 2 人周）

1. **rt.rs:1368 字节切片 panic**（中文参数崩溃）— 一行改 chars().take(80)；
2. **tokenizer 不计 ToolResult** + 压缩破坏配对 + post-compact 死代码（三者共同决定上下文子系统可信度）；
3. **MCP client 接线**（零件齐备只差装配：RuntimeBuilder 注入点 + 启动序列注册 wrapper）;
4. **exec 默认改 ReadOnly + 显式 auto-approve/danger 旗标**；
5. **CLI 主链路接入 load_config()**；
6. **TUI Ctrl-C 中断**；
7. **web.fetch UTF-8 切片 panic**（统一走 truncate_output）。

### 一个月内（P1）

网络隔离（landlock ABI v4 AccessNet / Seatbelt deny network*）或如实降级文档；SSRF 校验器合一 +
mapped-IPv6 + MCP HTTP 校验；黑名单大小写折叠 + 补 .cursorrules 保护面；ScriptHook Windows 转义；
Ollama NDJSON 字节缓冲；provider 读超时；shell.kill 真实现 + background 接沙箱；AllowAlways 贯通或
移除假按钮；StopReason 透传；熔断加 /compact //clear 逃生门；MSRV stable 门禁。

### 战略级

prompt caching + thinking 配置端（长会话成本/延迟的决定性因素）；斜杠命令框架 + token 计量展示
（UX 地基）；rt.rs 拆分；project 级配置落地或删除宣称；AGPL 许可证战略复审；文档实现状态标注体系。

---

## 十五、总评

minicoding-rs 是一个**少见的"声明即约束"型项目**：架构守卫把依赖方向变成 CI 门禁并实测吻合、1500 个
测试全绿、clippy pedantic 零告警、SSE 断线三级恢复和 C-21 取严合并这类细节达到商业产品水准。
"OS 沙箱一等公民 + 事件溯源 + Auto memory"的差异化叙事有真实技术含量。

它的核心病灶不是能力不足，而是**三类系统性裂缝**：

1. **端到端验证缺位**——单元测试大量使用与生产口径分叉的自造替身（CharTokenizer），导致三个 P0 存活至今；
   一条真实的中文长会话就能暴露其中大半；
2. **装配断层**——MCP client、asyncRewake、inject_context、AllowAlways、config.toml 都是"零件加工完毕、
   整机没有总装"，README 因此在多处描述了一个尚不存在的产品；
3. **文档超前于实现**——尤其 security.md 的安全论证建立在未实现的网络隔离之上，在修复前不应依据该文档
   对 exec/自动化场景做任何安全假设。

修复清单高度集中且多数改动在数十行以内。完成上述 P0 冲刺并把文档对齐现实之后，该项目完全有能力从
"架构示范品"跨入"日用工具"，并在沙箱与可审计性这两个维度做出真正的差异化。
