# minicoding-rs 第二轮全面审查报告（2026-08-25 R2）

> 审查性质：对同日早间首轮审查（`docs/project-review-20260825.md`）R1–R7 修复提交后的**独立复审**。
> 审查方法：7 路并行深度评审（安全权限沙箱 / core 运行时 / 上下文与记忆 / providers-tools-mcp-sdk / 四形态前端 / 文档体系 / 工程化），关键 P0/P1 发现经人工逐行复核确认（本报告标注"已复核"）。
> 本文同时作为第二轮修复工作的基线清单（见 §7 分阶段修复计划 R1'–R6'）。

---

## 1. 执行摘要

### 1.1 总体结论

首轮 R1–R7 修复**整体真实有效**：安全轨道 9/9 验证通过且实现质量高（边界测试齐全、fail-closed 方向一致）；providers/tools 轨道 15 项中 11 项完整落地；前端 F-1/F-5/F-6 带回归测试锁定。修复诚意与测试纪律延续高水平。

但本轮复审发现了 **3 类系统性问题与约 70 个新发现**：

1. **"形式化修复"模式**（修了但没修对/只修一半）：
   - PR-4 thinking 余量修复引入新 400（budget≥8192 时 `min(8192)` 使 max_tokens < budget_tokens）；
   - MC-4/F-3 "mcp serve 只读默认"只挡了 fs 写工具，shell.run/git.apply 无条件注册直通 execute；
   - F-3 只修服务端路由，Web/Desktop 前端零消费；
   - A-P3 span.instrument 只修自述两处，同文件残留 4 处 entered-guard 跨 await；
   - A-2 更名 is_hard_deny 半途而废，新旧命名并存；
   - A-5 杂物清理被 R7 回归——`bindings/serde_json/JsonValue.ts` 重新入库。
2. **"接线空转"模式复发**（字段/方法存在但生产零调用）：
   - N-1：C-30 沙箱硬熔断只造了"显示器"没接"刹车"——HardTripped 仅产出劝阻文案，循环照常回灌继续，全仓无 breaker 状态消费点（已复核）；
   - N-2：`RuntimeConfig.tools.{shell_timeout_sec,shell_max_output_bytes,fs_max_read_bytes}` 三字段全仓零消费者，ToolContext 硬编码 120s/1MiB，用户配置永远被截杀（C-07 可配承诺落空）；
   - N-3：`ToolContext.canceller` 协作式取消契约空转，22 个内置工具无一读取；
   - CT-4：`with_circuit_breaker_config` 已定义但零生产调用；
   - T-4：`with_can_spawn_subagent` 死代码，两处真实组装点均用默认构造，嵌套深度防御形同虚设。
3. **修复引入的新缺陷**：
   - B-1：会话级 Allow 缓存按裸工具名命中、置于一切门控之前，可击穿 C-23/C-27 的"不可 Always"通道（已复核）；
   - N2：PR-4 修复引入的 Anthropic 400（见上）；
   - F-2 seq 单写者重设计留下懒恢复会话跨重启 seq 分叉（已复核）；
   - N4：后台 shell evict_oldest 移除运行中条目既不 kill 也不释放，文档声称的 kill_on_drop 不成立。

### 1.2 评分总表（对比首轮）

| 维度 | 首轮 | 本轮 | 变化说明 |
|------|:---:|:---:|---------|
| 安全/权限/沙箱 | — | **7.0** | 9 项修复全部落地；新增 2 个 P1（会话缓存击穿、Landlock <6.7 内核不可用） |
| core 运行时 | 8.5 | **7.0** | A-P1/A-P2/T-1 配套真实落地；熔断未强制+死配置拉低 enforcement 完整性 |
| context/memory | 7.0/6.0 | **7.5** | CT-2/CT-3/MM-1 修复质量高；CT-4/CT-5 接线空转复发 |
| providers/tools/mcp/sdk | 8.5/7.5 | **7.0** | 11/15 完整落地；三处形式化修复+同族问题只修一处模式明显 |
| 四形态前端 | 7.5 | **7.0** | F-1/F-5/F-6 范本级；F-2 重设计留 P1、F-3/F-4 半截工程 |
| 文档体系 | 6.5 | **6.5** | 六处 sandbox-run 对齐完成；R6 把错误枚举立为权威（P1）、五处 seccomp 残留 |
| 工程化 | 8.0 | **7.5** | 兑底/钉版到位；concurrency/timeout 双缺失、desktop deny 豁免 |

---

## 2. 首轮修复验证结果总表

### 2.1 验证通过（真实落地且正确）

| 项 | 关键证据 |
|---|---|
| S-1 AllowAlways 粒度收敛 | permission.rs:476-483,538-543,611-629；DenyAlways 全局 fail-closed :632-641 |
| S-2 换行绕过 | builtin.rs:245 分段集合含 `\n\r`；预批准复合列表 :139 |
| S-5 Windows Job | windows.rs:179-193 无 BREAKAWAY_OK；句柄存驱动 :54,117-120 |
| S-7 journal LIFO | journal_impl.rs:125-137 回推尾部+反转时序 |
| S-8 event_store 原子写 | event_store.rs:109-133 SessionLock+单次 write_all+fsync |
| S-9 redact 多赋值 | redact.rs:88-147 逐段扫描 |
| S-10 尾随点空格 | builtin.rs:383-386,317-322 |
| S-12 persist 边界 | persist.rs:81-86 组件级判定 |
| L1/L2 0600 收紧 | audit.rs:55-84/jsonl.rs:220-238/index.rs:110-121/fs_private.rs 单一事实源 |
| PR-1 usage:null | openai.rs:427-429 + 回归测试 |
| PR-2 max_completion_tokens/o 系 temperature | openai.rs:148-161,441-446 |
| PR-3 top_p gate | anthropic.rs:177-181 |
| PR-6 SSE 上限 | sse.rs:24,127-133 fail-closed |
| T-1 worktree 注入 | worktree.rs:209 + 真实断言测试 :687-747 |
| T-2 merge 失败可见化 | worktree.rs:233-242 |
| T-3 超时补齐 | apply/diff/fetch 全链路 timeout |
| T-5 grep spawn_blocking | grep.rs:135,161 |
| T-6 fetch 体上限 | fetch.rs:91-119 流式截断 |
| MC-2 定向关停旧连接 | rmcp.rs:319-331 close_with_timeout |
| MC-3 base64 crate | rmcp.rs:741-751 四引擎回退 |
| sdk ReasoningDelta | stream.rs:100-102 |
| F-1 guard 再生成管线 | gen-types-post.mjs:88-127 自动同步 |
| F-5 expect 清除/202 瘦身 | http.rs:699-703,743-746 + 回归测试 |
| F-6 proxy/类型逃逸 | vite.config.ts:14-19 |
| E-1 发布兑底 | desktop-release.yml:198-205 gh release view→create |
| E-4/E-5 各子项 | pnpm/node/action 钉版一致；ignore 矛盾解除；CHANGELOG 倒序 |

### 2.2 半落地/有残留（详见 §3-§6 对应编号）

CT-1（超时✔ 锁收窄以文档化取舍替代+compress/mod.rs:120 硬编码 default 致端到端不可配）、CT-4（builder 定义零调用）、CT-5（异步化✔ 提取时机原样）、MM-5（save 锁✔ RMW 竞态残留）、PR-4（公式落地但 min(8192) 引入新 400）、T-8（淘汰落地但语义与文档矛盾+redact 整行误杀未动）、MC-4/F-3（只堵 fs 写漏 shell/git.apply）、A-P3（4 处残留）、A-2（命名并存）、A-5（被 R7 回归）、F-3（无前端消费）、F-4（stdout 日志泄露新通道）、D-2b/D-3b/D-4b（M-07/M-08 冲突、统计口径、TaskStatus 残留）。

---

## 3. 新发现详单

> 编号规则：SEC-*（安全）/ CORE-*（core 运行时）/ CTX-*（上下文记忆）/ PTM-*（providers/tools/mcp/sdk）/ FE-*（前端）/ DOC-*（文档）/ ENG-*（工程化）。标 ★ 者经人工逐行复核。

### 3.1 安全/权限/沙箱轨道

| 编号 | 级别 | 发现 | 位置 |
|---|---|---|---|
| SEC-1 ★ | **P1** | 会话级 Allow 缓存按裸工具名命中、置于一切门控之前，击穿 C-23/C-27：先对 memory.write(long_term) 按 a 后，本会话内指令性 auto.md 写入不再弹窗直接落盘——auto.md 持久跨会话，正是 C-27 要防的记忆投毒通道。持久化查表有 options 门控（permission.rs:517-523），会话缓存无（:476-483） | core/runtime/permission.rs |
| SEC-2 | **P1** | Landlock ruleset 以 BestEffort 同时 handle AccessFs(V3) 与 AccessNet(ABI4)，pre_exec 对 PartiallyEnforced 直接报错——内核 <6.7（Ubuntu 22.04/Debian 12）每次 spawn 必失败且错误诱导用户关闭沙箱；landlock_available() 只探 V1，doctor 照报"就绪" | sandbox/linux.rs:96-105,133-141,156-158,299 |
| SEC-3 ★ | P2 | 目录粒度 AllowAlways 用未规范化的原始输入路径做词法前缀匹配：批准 src/gen/a.rs 后 src/gen/../secret.txt 直接命中放行（逃出批准目录范围） | permission.rs:514; persist.rs:82-85 |
| SEC-4 | P2 | Windows active_job 单槽替换：后台 shell 运行中下一条前台命令 post_spawn 覆盖槽位，旧 JobHandle drop 触发 KILL_ON_JOB_CLOSE 静默杀死整个后台进程树；bg/fg 并发 apply/post_spawn 交错使 last_policy 快照错配（裸奔 resume） | sandbox/windows.rs:42-45,46,97-120 |
| SEC-5 | P2 | ResumeThread 返回值被丢弃，失败仍记 resumed=true，子进程永久挂起泄漏至 Runtime drop | windows.rs:321 |
| SEC-6 | P2 | shell 黑名单词表纯 POSIX 而 Windows 用 cmd /C 执行：del/rd/erase/move 不在 WRITE_VERBS，Windows 主机 shell 旁路防护基本失效 | tools/shell/run.rs:107-109; builtin.rs:235 |
| SEC-7 | P2 | 复合语句逃逸：`for f in $(ls); do rm AGENTS.md; done` 切段后段首 token 是 do，非 WRITE_VERBS 且前 token 非重定向符→不命中黑名单（then/else 同理） | builtin.rs:253-254 |
| SEC-8 | P3 | journal 恢复写不经 O_NOFOLLOW/lstat：workdir 内文件被换成指向外部的 symlink 且内容恰与 after 一致时 restore 写穿透出界 | journal_impl.rs:180-315 |
| SEC-9 | P3 | event seq 在锁外分配：两进程同时 resume 同一会话可各追加同号记录，cursor 去重失效；读路径不持锁可能见半行 | event_store.rs:100-136,193-196 |
| SEC-10 | P3 | policy.toml 读改写无锁且 tmp 名固定 .tmp：多会话并发 mutate 互相覆盖或发布半写文件 | persist.rs:150-171 |
| SEC-11 | P3 | Seatbelt profile tempfile keep() 后放弃 drop 清理，spawn 失败则 .sb 残留 /tmp | macos.rs:124-141 |
| SEC-12 | P3 | CLI 提示"[N]始终拒绝"但 read_ynad 永不返回 DenyAlways，承诺的持久化拒绝实为一次性 | prompter.rs:102,128-140 |
| SEC-13 | P3 | vcs_protected_dirs 只查 workdir 根级且 is_dir() 过滤：嵌套 submodule 与 worktree 的 .git 文件形式不进 landlock RO 规则 | sandbox/hardening.rs:97-104 |
| SEC-14 | P3 | 会话缓存命中审计丢"Always 来源"，与 S-1 修复"审计可达"目标相悖 | permission.rs:480-482,840-842 |

### 3.2 core 运行时轨道

| 编号 | 级别 | 发现 | 位置 |
|---|---|---|---|
| CORE-1 ★ | **P1** | C-30 硬熔断未强制 TurnEnd：HardTripped 仅产出劝阻文案 ToolResult，循环照常回灌继续到 max_iters，全仓无 breaker 状态消费点（仅 turn 开始 reset）。违反 docs/rules.md:90"强制 TurnEnd"承诺 | runtime/denial.rs:150-171; rt.rs:706-731 |
| CORE-2 ★ | **P1** | RuntimeConfig.tools 三字段（shell_timeout_sec/shell_max_output_bytes/fs_max_read_bytes）全仓零消费者；ToolContext::new 硬编码 120s/1MiB 且无 setter。后果实例：shell/run.rs 以 ctx.timeout 为 clamp 上限，用户配 300s 永远被 120s 截杀 | config.rs:143-145; rt.rs:1048; tool/trait.rs:98-99 |
| CORE-3 | **P1** | ToolContext.canceller 协作式取消契约空转：每次构造新建孤立 token，Runtime cancel_token 从不下传，22 个内置工具无一读取；Ctrl-C 只能靠 drop future 硬中断 | tool/trait.rs:96; rt.rs:1048-1052 |
| CORE-4 | P1 | 同文件残留 4 处 entered-guard 跨 await：最严重 permission.rs:134 span.enter() 跨 prompter.prompt().await（可达数百秒）；另 rt.rs:1100/1124/1187（buffer_unordered 下 future 跨线程迁移）。A-P3 属半成品 | permission.rs:134; rt.rs:1100,1124,1187 |
| CORE-5 | P2 | bindings/serde_json/JsonValue.ts 被 R7 重新提交入库（A-5 回归）；AGENTS §8.4 唯一合法位置是 web/src/api/generated/ | crates/minicoding-core/bindings/ |
| CORE-6 | P2 | Failed 路径不广播 TurnEnd：LLM 失败/工具 Err/storage 早退均 return 不发事件；CLI 已被迫加 500ms 兜底（interactive.rs:36-40 自注释承认），LSP 进度条悬挂 Begin 态 | rt.rs:595-598,708-711,527-530 |
| CORE-7 | P3 | is_builtin_deny/is_hard_deny 新旧命名并存于同一表达式链 | permission.rs:224,248,374-379,468,501 |
| CORE-8 | P3 | current_turn 实现与字段文档相反（存迭代下标非轮次号），HookInput.turn/PermissionContext.turn 拿到错值 | rt.rs:121-122,558-559 |
| CORE-9 | P3 | cancel_token 重建窗口竞态：_turn_guard 未 drop 时重建新 token，窗口内 cancel() 取消的是下一轮 | rt.rs:808-815,286-292 |
| CORE-10 | P3 | repeat_guard 软提醒只 append 不落盘不广播，resume 后模型可见历史静默变化 | rt.rs:664-669 |
| CORE-11 | P3 | UserInput.attachments/context_hint 字段全仓无消费者，SDK/server 填充即无声丢失 | session.rs:135-137; rt.rs:526 |
| CORE-12 | P3 | sanitize_env 黑名单式脱敏大小写敏感三词匹配，api_key/password/authorization 全漏；作为 pub API 留存即一旦接线的 C-04 缺口 | config.rs:435-440 |
| CORE-13 | P3 | PreToolUse 的 async_rewake 未按 trait_def.rs:45-50 契约门控，无条件 try_spawn 是否触发取决于 hooks crate 是否过滤 | permission.rs:422-450 |
| CORE-14 | P3 | 库 crate 边界 Result<_, String> 违反 thiserror 约定（builder.rs:428/config.rs:341/persist.rs:110） | core 多处 |
| CORE-15 | P3 | A-P1 保序回退无测试锁定（parallel_reads.rs 只测并发上限） | tests/ |

### 3.3 上下文与记忆轨道

| 编号 | 级别 | 发现 | 位置 |
|---|---|---|---|
| CTX-1 | P2 | AutoMemory add_entry 读-改-写在 save_lock 之外：并发双任务同基线各自追加后串行落盘后者整表覆盖前者；Arc<AutoMemory> 共享单例使跨会话并发可达 | auto.rs:179 vs 232-234; sdk/builder.rs:382-384 |
| CTX-2 | P2 | ProjectDocLoaderImpl async load 与 load_sync 错误语义相反（async 单个不可读文件使整链失败，sync warn+continue），doc comment 却声称一致——注释与代码冲突 | loader.rs:108,123-129,174 |
| CTX-3 | P3 | summarize seq 推算裸 u64 减法与 mod.rs saturating_sub 口径不一致，anchor<total-1-i 时 debug panic/release 回绕出天文序号（restore 当前无生产调用，一踩即爆） | summarize.rs:127 vs mod.rs:73 |
| CTX-4 | P3 | compress() 持熔断器 tokio Mutex 跨 record_compress_audit await（audit 文件 IO），并发预检被无谓阻塞 | manager.rs:316-323 |
| CTX-5 | P3 | repo_root/dir_chain 字符串前缀判定兄弟目录误判（cwd=/repo2 匹配 root=/repo）且尾斜杠退化沿链走到文件系统根加载 /AGENTS.md；fallback.rs:55 漏 .git 文件形式 | loader.rs:83-87; fallback.rs:55 |
| CTX-6 | P3 | clear() 不持 save_lock 也不清 mtime 基准，并发交错可复活刚清空的条目 | auto.rs:212-218 |
| CTX-7 | P3 | inject_post_compact 直读任意绝对路径不经 C-03 校验，TOCTOU 窗口内 symlink 换向读到他物回灌 system 段 | post_compact.rs:103-119 |
| CTX-8 | P2 | compress/mod.rs:120 硬编码 SummarizeConfig::default()，L2 超时端到端不可配；with_circuit_breaker_config 零生产调用 | mod.rs:120; manager.rs:159-162 |

### 3.4 providers/tools/mcp/sdk 轨道

| 编号 | 级别 | 发现 | 位置 |
|---|---|---|---|
| PTM-1 | P1 | mcp serve 默认仍零权限暴露 shell.run/background/kill 与 git.apply/web.*：--expose-write-tools 只 gate fs 写工具，shell（Command 副作用、ctx 无沙箱）与 git.apply(FileWrite) 无条件注册直通 execute，fail-closed 名不副实 | cli/serve.rs:361-368; expose.rs:154-157 |
| PTM-2 | P2 | PR-4 修复引入：thinking budget≥8192 时 .min(MAX_OUTPUT_LIMIT) 使 max_tokens<budget_tokens 必 400，违反自身 doc 注释不变式；测试把违规固化 | anthropic.rs:520-535; 测试 :908-911 |
| PTM-3 | P2 | T-4 深度防御死代码：with_can_spawn_subagent 仅测试调用，两处真实组装点 TaskSpawn::new can_spawn 恒 true，子 Agent 永不会被工具层拒绝嵌套 | task/spawn.rs:124-125,222-226; sdk/builder.rs:577; server/runtime_builder.rs:301 |
| PTM-4 | P2 | evict_oldest 移除运行中条目既不 kill 也不释放：wait task 持有 Child 克隆永不 drop，模块文档声称的 kill_on_drop 不成立 | background.rs:79-86,226-252,329-342 |
| PTM-5 | P2 | web.search 与 fetch 同族缺陷未修：无 ctx.timeout 包裹、client 零超时、resp.text() 无界缓冲 | search.rs:61-100 |
| PTM-6 | P2 | shell.output 返回的后台输出不经 redact_secrets，C-04 在后台路径整体旁路（前台 run.rs 有） | output.rs:83-89 vs run.rs:230 |
| PTM-7 | P3 | NdjsonStream 缓冲无上限（Ollama 恶意流无限撑大，PR-6 同族只修 SSE 侧） | ndjson.rs:80-91 |
| PTM-8 | P3 | fs.glob 同步 WalkBuilder 直接跑在 async fn（T-5 同族只修 grep） | glob.rs:89-106 |
| PTM-9 | P3 | OpenAI 推理系 gate 只挡 temperature 未挡 top_p（gpt-5 发送即 400） | openai.rs:153-155 |
| PTM-10 | P3 | merge_back 失败+auto_cleanup=true 时 git branch -D 强删分支，刚警告"改动丢失"的未合并提交被永久销毁 | worktree.rs:114-120,245-247 |
| PTM-11 | P3 | expose.rs 过时注释"annotations 当前留 None"与 list_tools 实际填充 hint 矛盾 | expose.rs:210-211 vs 119-122 |
| PTM-12 | P3 | join_redirect_url 相对 Location 一律拼 origin 根（/b/c 应基于当前路径目录解析） | fetch.rs:222-227 |
| PTM-13 | P3 | fs.read/edit 先全量 read_to_string 才截断/journal，超大文本可打爆内存（fetch 已修同族） | read.rs:90; edit.rs:88 |
| PTM-14 | P3 | redact 整行替换误杀面大（sk- 行内任意位置整行吞）（首轮遗留后半未动） | run.rs:321-323 |

### 3.5 四形态前端轨道

| 编号 | 级别 | 发现 | 位置 |
|---|---|---|---|
| FE-1 ★ | **P1** | 懒恢复会话 SSE seq 空间与持久化 seq 分叉：cursor 从不按 Runtime durable_seq/event_seq 播种（ServerSession::new 恒 EventCursor::new(1024) next_seq=1），恢复后新事件从 1 重发撞号；老客户端 Last-Event-ID=N 重连 floor=N 后全部丢弃——三级恢复跨重启整体失效 | session_mgr.rs:90,342; sse.rs:152-154 |
| FE-2 | P1 | MCP serve 只读默认漏 shell 类（同 PTM-1） | cli/commands/serve.rs:361-364 |
| FE-3 | P2 | insert_session TOCTOU：开头查重与末尾无条件 insert 之间无二次校验，并发 get_or_load 双 Runtime 双 sequencer，败者 Arc 任务环永不退出双写 jsonl | session_mgr.rs:333-341,390-394 |
| FE-4 | P2 | Web 前端不处理 RehydrateRequired：SSE 收到即静默丢（client.ts:325-328 注释自认），hooks 层无重拉 snapshot 逻辑 | web/src/api/client.ts |
| FE-5 | P2 | desktop 鉴权 token 经 SERVER_TOKEN= stdout 回显被完整写入本地日志文件夹（server/main.rs:138 → sidecar.rs:271-297 INFO 日志）；F-4 堵 cmdline 开了日志 | server/main.rs:137-139; desktop/sidecar.rs:271-297 |
| FE-6 | P2 | F-3 只修服务端：Web/Desktop 至今无 /undo、/permission-mode API 封装或入口，四形态能力矩阵漂移依旧 | web/src/api/client.ts; App.tsx |
| FE-7 | P2 | restore_session 仅覆盖 workdir，原会话 permission_mode/sandbox preset 重启后静默回落 server 默认 | session_mgr.rs:516-517 |
| FE-8 | P3 | CLI/TUI 均丢弃 ReasoningDelta：CLI catch-all Ok(_)=>{}（interactive.rs:339），TUI handle_runtime_event 无该分支——reasoning 仅 Web/SDK 可见 | interactive.rs:339; tui/app.rs:316-394 |
| FE-9 | P3 | 版本漂移：workspace 0.3.3，tauri.conf.json 仍 0.3.2 | tauri.conf.json:4 |
| FE-10 | P3 | desktop/main.rs 自相矛盾注释（关窗即退 vs 托盘常驻，代码为前者） | desktop/main.rs:149-152,172-173 |
| FE-11 | P3 | ACP turn 中 stdin EOF 不应答挂起 prompt 请求 id 即 break，永无 response | acp.rs:588-592 |
| FE-12 | P3 | init()/start_session 无幂等守卫：StrictMode dev 双挂载拉起两个 sidecar | stores/desktop.ts:123; main.tsx:10 |
| FE-13 | P3 | SessionManager::delete 死代码无 DELETE 路由 | session_mgr.rs |
| FE-14 | P3 | runtime_builder 模块头"无 Hook/Journal"文档腐化（实际注入 journal）；tiktoken 失败降级硬编码 system prompt 丢弃用户 --system | runtime_builder.rs:8-11,206-208,266-271 |

### 3.6 文档体系轨道

| 编号 | 级别 | 发现 | 位置 |
|---|---|---|---|
| DOC-1 | **P1** | R6 把与代码不符的 Event 枚举立为权威：design.md:1526-1548 与 api.md:1257-1295 含 5 个代码不存在变体（Error/SubagentStarted/SubagentFinished/HookRun/FileUndone——后者仅为 AuditKind）、漏 ReasoningDelta/SessionCreated、ToolCallStart/Progress/End vs 代码 ToolCallStarted/Finished；roadmap/architecture/AGENTS 均引用幽灵事件 | design.md; api.md; roadmap.md:205,308; architecture.md:258 |
| DOC-2 | P2 | D-1 清单外仍有 5 文档宣称 libseccomp 必装/在用（getting-started 安装指引 apt/dnf/pacman/product-manual"不自研胶水"直接反悔决策/deepseek 对比/review-report）——用户照做装无用系统包 | getting-started.md:52-56 等 |
| DOC-3 | P2 | api.md:921-925 沙箱驱动表系统性失实：WindowsSandboxDriver（实际 WindowsJobDriver）、"sandbox-run 封装"（实际自研 FFI/landlock 直连）、id "landlock+seccomp"/"windows-acl"（实际 landlock/windows-token）、漏 ExternalSandboxDriver | api.md:921-925 |
| DOC-4 | P2 | M-07/M-08 同名不同物冲突原样存在无任何说明（features M-07=AGENTS 记忆/M-08=向量检索 vs rules/design M-07=压缩追溯/M-08=循环打断软升级） | features.md:101-102 vs rules.md:122 |
| DOC-5 | P2 | 统计口径两套互斥：优先级映射 ~167≠204、口径拆分 168≠204 | features.md:319,333-335 |
| DOC-6 | P2 | api.md task.update 校验三处仍写 Completed/Deleted（代码 Cancelled） | api.md:1807-1821 |
| DOC-7 | P2 | axum 引用悬空（design 见 tech-stack 但其全文无 axum 条目）；modules.md:899、innovation.md:1112 仍"ts-rs 或 specta"二选一 | tech-stack.md; modules.md:899 |
| DOC-8 | P2 | TanStack Router 三处矛盾：AGENTS 未采用 vs tech-stack 列选型 vs AGENTS 目录结构保留 routes/ 层 | AGENTS.md:483,506; tech-stack.md:81 |
| DOC-9 | P3 | modules.md:13 同句"18 个 crate……列出全部 19 个"自相矛盾 | modules.md:13 |
| DOC-10 | P3 | 门禁计数四处仍称"9 道"（ci.yml 实际 10 jobs） | ci.yml:3; build-guide.md:1021 等 |
| DOC-11 | P3 | gen-types 命令 npm run gen-types vs CI 实际 pnpm gen-types | AGENTS.md:518; innovation.md:1112 |

### 3.7 工程化轨道

| 编号 | 级别 | 发现 | 位置 |
|---|---|---|---|
| ENG-1 | P2 | 三个 workflow 均无 concurrency 组：重复 push 重复跑、release/desktop-release 同 tag 并行上传无串行化保护 | .github/workflows/*.yml |
| ENG-2 | P2 | 三个 workflow 均无 timeout-minutes：job 挂起烧满 runner 6h | .github/workflows/*.yml |
| ENG-3 | P2 | deny.toml:10 整体排除 minicoding-desktop：桌面发行包 Tauri 依赖树完全不经 license/advisory/sources 扫描，却发布 .dmg/.msi/AppImage | deny.toml:10 |
| ENG-4 | P2 | roadmap 遗留清单 13 项 vs 报告开放项 15 项，缺 6 项登记（denial echo/journal 内存上限/SDK semver/extension permissions/MSRV 可达/sleep 改造） | roadmap.md:412 |
| ENG-5 | P2 | git-hooks 协议断裂：pre-commit:78 称 MINICODING_PRE_PUSH 由 pre-push 设置但 pre-push 从未 export→推送阶段 audit/test/coverage 死代码；pre-push 裸 cargo test 与 pre-commit config 口径不一 | scripts/git-hooks/* |
| ENG-6 | P3 | cliff.toml:71-72 Security 解析器位于 catch-all 之后永不可达；cliff sort_commits="oldest" 与手写 CHANGELOG 倒序双源矛盾未解 | cliff.toml:71-72,80 |
| ENG-7 | P3 | desktop-release.yml 顶层 contents:write 使 build 矩阵 job 不必要持写权限；workflow_dispatch input tag 定义后零引用入口无意义；tauri-cli "^2" 浮动版本 | desktop-release.yml:15-16,23-26,97 |
| ENG-8 | P3 | workspace 无 [workspace.lints]，18 个 crate lib.rs 逐个重复 deny 属性 | Cargo.toml; */lib.rs |
| ENG-9 | P3 | secrets 检查正则 \.env$ 不匹配 .env.test/.env.development（gitignore 已列）force-add 可绕过 | pre-commit:73; .pre-commit-config.yaml:107 |
| ENG-10 | P3 | release.yml dist 经 curl\|sh 无校和钉版；action @branch-tag 非 commit SHA | release.yml:67 |
| ENG-11 | P3 | MSRV 矛盾现状未收敛且 roadmap 未登记：rust-version=1.99+钉 nightly-2026-08-18，stable 1.98 无法编译、无 MSRV 校验 job | Cargo.toml:27; rust-toolchain.toml:6 |

---

## 4. 风险矩阵更新（相对首轮变化）

| 风险 | 概率 | 影响 | 变化 |
|---|:---:|:---:|---|
| SEC-1 会话缓存击穿 C-27 记忆投毒 | 中 | 高 | **新增**（S-1 修复的副产物） |
| SEC-2 <6.7 内核沙箱必失败推高弃用率 | 高（发行版基数大） | 高 | **新增** |
| CORE-1 熔断无刹车 LLM 可无视劝阻重试到上限 | 中 | 中高 | **新增**（C-30 enforcement 缺口） |
| CORE-2 用户超时配置被静默截杀 | 高 | 中 | **新增**（C-07 承诺落空） |
| PTM-1 mcp serve shell 直通暴露 | 低（需显式起 serve） | 高 | 首轮 MC-4 修复不完整的残留放大 |
| FE-1 断线重连跨重启永久黑屏 | 中 | 中高 | **新增**（F-2 重设计副产物） |
| FE-5 token 落盘日志 | 高（desktop 默认路径） | 中 | **新增**（F-4 修复的副产物） |
| 首轮风险项 S-2/S-1/S-3/S-7~S-12 | — | — | 已缓解（验证通过） |

---

## 5. 设计层面新批评（对 design.md/rules.md 本身）

1. **rules.md C-30 的"强制 TurnEnd"在实现契约层无落点**：`SandboxDenialTracker` trait 只有 detect/count/state，没有"谁负责终止 turn"的规格；Runtime 循环也无"检查熔断状态"的钩点——约束文字与类型系统脱节，正是本项目自称最硬的差异点（L0 实现层强制）上的缺口。
2. **ToolContext 配置传递链缺失设计**：RuntimeConfig.tools→ToolContext 无任何映射规格，导致三字段死配置无人发现；应规定 builder 阶段一次性物化 ToolExecLimits 并注入。
3. **seq 空间的跨重启语义未规格化**：design §24 三级恢复描述了内存 ring buffer 场景，未定义"懒恢复会话 cursor 初值必须从持久化流播种"；F-2 修复者按字面实现了单写者，播种缺失恰在规格空白处。
4. **mcp serve 的"只读默认"缺工具分级清单**：expose 层只有 fs 写工具旗标概念，无 SideEffect 维度的统一门控规格——shell/git.apply/web.fetch 的旁路是规格空白而非实现疏忽。
5. **会话级 Always 缓存的门控规格缺失**：S-1 修复给持久化查表加了 options 门控却没给会话缓存加，根因是"restricted ask（不可 Always）"只在 C-23/C-27 文字里，Verdict/PromptOption 类型上无法区分。

---

## 6. 用户体验问题（新增观察）

- CLI "[N]始终拒绝"承诺与行为不符（SEC-12），用户以为配置了永久拒绝实际下次还弹；
- 用户设置 `[tools] shell_timeout_sec=300` 被静默截杀为 120s 且无任何告警日志（CORE-2）；
- 沙箱在不支持内核上每次 spawn 报错并引导关闭沙箱（SEC-2），正确的降级提示应在启动 doctor 阶段一次性给出；
- reasoning 流在 CLI/TUI 完全不可见（FE-8），四形态体验割裂；
- Web 恢复会话后 undo/权限模式切换无任何入口（FE-6）。

---

## 7. 分阶段修复计划（R1'–R6'）

| 阶段 | 覆盖发现 | 内容 |
|---|---|---|
| R1' 安全 | SEC-1~14 | 会话缓存 options 门控、Landlock 兼容性分级降级+doctor 如实、目录粒度规范化前缀、Windows Job 槽位/ResumeThread、cmd 动词表、do/then/else 段首处理、journal O_NOFOLLOW、seq 锁内分配、policy.toml 原子写、seatbelt 清理、DenyAlways 兑现、vcs .git 文件形式、审计来源注记 |
| R2' core | CORE-1~15 | 熔断强制 TurnEnd（turn 开始检查+denial HardTripped 强制终止）、ToolExecLimits 接线（config→ToolContext）、canceller 下传、span 清理收尾、bindings 清理、Failed 广播 TurnEnd、命名统一、current_turn 语义修正、cancel_token 竞态、attachments 显式拒绝、sanitize_env 白名单化或删除、async_rewake 门控、Result<_,String> 收敛、保序测试 |
| R3' 上下文记忆 | CTX-1~8 | RMW 全程锁内、loader 语义对齐+doc 修正、saturating_sub、audit 出锁、组件级前缀、clear 持锁、post_compact 路径校验、SummarizeConfig 端到端接线+breaker 配置接线 |
| R4' PTM | PTM-1~14 | mcp serve 按 SideEffect 门控、anthropic budget clamp 修正、can_spawn 接线或删除、evict kill 语义、search 超时+上限、shell.output 脱敏、ndjson/glob/read/edit 同族、top_p gate、merge_back 保护、注释修正、redirect 相对解析 |
| R5' 前端 | FE-1~14 | cursor 播种、insert 二次校验、RehydrateRequired 重拉、SERVER_TOKEN 不落日志、Web undo/permission-mode 入口、restore 保留安全上下文、CLI/TUI reasoning 渲染、版本号、注释修正、ACP EOF 应答、init 幂等、delete 路由或删死代码、runtime_builder 文档修正 |
| R6' 文档工程 | DOC-1~11, ENG-1~11 | Event 枚举权威源修正（以 core/runtime/event.rs 为准）、seccomp 五处、驱动表、M-07/M-08 说明、统计口径、TaskStatus、axum/ts-rs 条目、TanStack 口径、modules 措辞、门禁计数、gen-types 命令；CI concurrency+timeout、deny desktop 收窄评估、roadmap 补登记、git-hooks export、cliff 顺序、release 权限收敛、workspace.lints、secrets 正则、MSRV 登记 |

**明确延期项**（需立项非修补，续登 roadmap）：seccomp 接入、DNS IP pinning、HOME 细粒度白名单、EventBus 双通道、Tauri updater、TUI 斜杠命令体系、evals 框架、多 provider 故障切换、成本核算、31 处 sleep 时钟改造、MSRV stable 可达（依赖上游 rustc 修复）。
